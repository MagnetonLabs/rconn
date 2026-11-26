//! Built-in TURN relay for self-hosted servers, plus minting of the short-lived
//! credentials clients receive in `Welcome`.
//!
//! Credentials follow the TURN REST API convention: username = expiry (Unix seconds),
//! password = base64(HMAC-SHA1(shared_secret, username)). The relay checks both
//! statelessly, so the signaling server and the relay only share the secret.
//! Relayed media stays DTLS-SRTP encrypted end to end; the relay sees ciphertext.

use anyhow::{Context, Result};
use async_trait::async_trait;
use rconn_core::protocol::IceServer;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::watch;
use turn::auth::{LongTermAuthHandler, generate_long_term_credentials};
use turn::relay::RelayAddressGenerator;
use turn::relay::relay_range::RelayAddressGeneratorRanges;
use turn::server::Server;
use turn::server::config::{ConnConfig, ServerConfig};
use util::Conn;
use util::vnet::net::Net;

pub const REALM: &str = "rconn";

/// How long a minted TURN credential stays valid. Sessions longer than this keep their
/// existing allocation; new allocations need a fresh `Welcome`.
pub const CREDENTIAL_TTL: Duration = Duration::from_secs(12 * 3600);

#[derive(Debug, Clone)]
pub struct TurnConfig {
    /// UDP socket the relay listens on, e.g. 0.0.0.0:3478.
    pub listen: SocketAddr,
    /// Address clients can reach the relay at (the VM's public IP).
    pub public_ip: IpAddr,
    /// Port range for relayed allocations. Open it in the cloud firewall.
    pub relay_ports: (u16, u16),
    pub shared_secret: String,
    /// Bytes one allocation may relay (both directions) before it is closed.
    pub per_allocation_bytes: Option<u64>,
    /// Usage across all allocations, with its monthly cap.
    pub usage: Arc<RelayUsage>,
}

/// Relayed bytes this calendar month (UTC), against an optional cap. When the cap is
/// reached, every allocation closes, new ones are refused, and `Welcome` stops offering
/// TURN credentials (sessions then need a direct path) until the month turns.
pub struct RelayUsage {
    cap: Option<u64>,
    /// (yyyymm, bytes).
    month: Mutex<(u32, u64)>,
    file: Option<PathBuf>,
    exhausted: watch::Sender<bool>,
}

impl std::fmt::Debug for RelayUsage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayUsage")
            .field("cap", &self.cap)
            .field("month", &self.month)
            .finish_non_exhaustive()
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct UsageFile {
    month: u32,
    bytes: u64,
}

/// `yyyymm` of a Unix time, UTC (civil-from-days, Howard Hinnant).
pub fn month_of(unix_secs: u64) -> u32 {
    let z = (unix_secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y * 100 + m) as u32
}

impl RelayUsage {
    /// `file` keeps the count across restarts (`None`: memory only).
    pub fn new(cap: Option<u64>, file: Option<PathBuf>) -> Self {
        let now = month_of(crate::unix_now());
        let saved = file
            .as_ref()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .and_then(|t| serde_json::from_str::<UsageFile>(&t).ok())
            .filter(|u| u.month == now)
            .map_or(0, |u| u.bytes);
        let full = cap.is_some_and(|c| saved >= c);
        Self {
            cap,
            month: Mutex::new((now, saved)),
            file,
            exhausted: watch::channel(full).0,
        }
    }

    pub fn unlimited() -> Arc<Self> {
        Arc::new(Self::new(None, None))
    }

    /// Bytes relayed this month.
    pub fn used(&self) -> u64 {
        self.roll(0);
        self.month.lock().expect("relay usage").1
    }

    pub fn exhausted(&self) -> bool {
        self.roll(0);
        *self.exhausted.borrow()
    }

    /// Count `n` more bytes (starting over when the month turned). `false` once the
    /// monthly cap is reached.
    fn roll(&self, n: usize) -> bool {
        let now = month_of(crate::unix_now());
        let mut m = self.month.lock().expect("relay usage");
        if m.0 != now {
            *m = (now, 0);
            self.exhausted.send_replace(false);
        }
        m.1 = m.1.saturating_add(n as u64);
        let full = self.cap.is_some_and(|c| m.1 >= c);
        if full && !*self.exhausted.borrow() {
            tracing::warn!(bytes = m.1, cap = ?self.cap, "monthly relay cap reached; relays closed until next month");
            self.exhausted.send_replace(true);
        }
        !full
    }

    /// Write the count to its file (called periodically and at shutdown).
    pub fn save(&self) {
        let Some(path) = &self.file else { return };
        let (month, bytes) = *self.month.lock().expect("relay usage");
        let tmp = path.with_extension("json.tmp");
        let ok = serde_json::to_vec(&UsageFile { month, bytes })
            .map_err(std::io::Error::other)
            .and_then(|b| std::fs::write(&tmp, b))
            .and_then(|()| std::fs::rename(&tmp, path));
        if let Err(e) = ok {
            tracing::warn!(error = %e, "could not save relay usage");
        }
    }
}

/// A relay socket that counts what passes through it and fails once a cap is reached.
/// The TURN server treats a failing relay socket as the end of that allocation and
/// removes it; the client's ICE then sees the relay path go away.
pub struct CappedConn {
    inner: Arc<dyn Conn + Send + Sync>,
    used: AtomicU64,
    cap: Option<u64>,
    usage: Arc<RelayUsage>,
    killed: watch::Sender<bool>,
}

fn capped() -> util::Error {
    util::Error::Other("relay cap reached".into())
}

impl CappedConn {
    pub fn new(
        inner: Arc<dyn Conn + Send + Sync>,
        cap: Option<u64>,
        usage: Arc<RelayUsage>,
    ) -> Self {
        Self {
            inner,
            used: AtomicU64::new(0),
            cap,
            usage,
            killed: watch::channel(false).0,
        }
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    fn is_dead(&self) -> bool {
        *self.killed.borrow() || self.usage.exhausted()
    }

    /// Count `n` bytes; `false` (and the socket dies) once either cap is passed.
    fn charge(&self, n: usize) -> bool {
        let used = self.used.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
        let mut ok = self.usage.roll(n);
        if self.cap.is_some_and(|c| used > c) {
            if !*self.killed.borrow() {
                tracing::info!(bytes = used, cap = ?self.cap, "relay allocation reached its cap; closing it");
            }
            ok = false;
        }
        if !ok {
            self.killed.send_replace(true);
        }
        ok
    }
}

#[async_trait]
impl Conn for CappedConn {
    async fn connect(&self, addr: SocketAddr) -> util::Result<()> {
        self.inner.connect(addr).await
    }
    async fn recv(&self, buf: &mut [u8]) -> util::Result<usize> {
        Ok(self.recv_from(buf).await?.0)
    }
    async fn recv_from(&self, buf: &mut [u8]) -> util::Result<(usize, SocketAddr)> {
        let mut killed = self.killed.subscribe();
        let mut exhausted = self.usage.exhausted.subscribe();
        if self.is_dead() {
            return Err(capped());
        }
        tokio::select! {
            r = self.inner.recv_from(buf) => {
                let (n, from) = r?;
                if self.charge(n) { Ok((n, from)) } else { Err(capped()) }
            }
            _ = killed.wait_for(|k| *k) => Err(capped()),
            _ = exhausted.wait_for(|e| *e) => Err(capped()),
        }
    }
    async fn send(&self, buf: &[u8]) -> util::Result<usize> {
        if self.is_dead() || !self.charge(buf.len()) {
            return Err(capped());
        }
        self.inner.send(buf).await
    }
    async fn send_to(&self, buf: &[u8], target: SocketAddr) -> util::Result<usize> {
        if self.is_dead() || !self.charge(buf.len()) {
            return Err(capped());
        }
        self.inner.send_to(buf, target).await
    }
    fn local_addr(&self) -> util::Result<SocketAddr> {
        self.inner.local_addr()
    }
    fn remote_addr(&self) -> Option<SocketAddr> {
        self.inner.remote_addr()
    }
    async fn close(&self) -> util::Result<()> {
        self.killed.send_replace(true);
        self.inner.close().await
    }
    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

/// Port-range relay sockets wrapped in [`CappedConn`]; refuses new allocations while the
/// monthly cap is reached.
struct CappedGenerator {
    inner: RelayAddressGeneratorRanges,
    per_allocation: Option<u64>,
    usage: Arc<RelayUsage>,
}

#[async_trait]
impl RelayAddressGenerator for CappedGenerator {
    fn validate(&self) -> std::result::Result<(), turn::Error> {
        self.inner.validate()
    }
    fn supports_family(&self, use_ipv4: bool) -> bool {
        self.inner.supports_family(use_ipv4)
    }
    async fn allocate_conn(
        &self,
        use_ipv4: bool,
        requested_port: u16,
    ) -> std::result::Result<(Arc<dyn Conn + Send + Sync>, SocketAddr), turn::Error> {
        if self.usage.exhausted() {
            return Err(turn::Error::Other("monthly relay cap reached".into()));
        }
        let (conn, addr) = self.inner.allocate_conn(use_ipv4, requested_port).await?;
        let capped: Arc<dyn Conn + Send + Sync> = Arc::new(CappedConn::new(
            conn,
            self.per_allocation,
            self.usage.clone(),
        ));
        Ok((capped, addr))
    }
}

/// A running relay. Dropping it does not stop the server; call [`TurnRelay::close`].
pub struct TurnRelay {
    server: Server,
    pub local_addr: SocketAddr,
}

impl TurnRelay {
    pub async fn start(cfg: &TurnConfig) -> Result<Self> {
        let conn = Arc::new(
            tokio::net::UdpSocket::bind(cfg.listen)
                .await
                .with_context(|| format!("binding TURN {}", cfg.listen))?,
        );
        let local_addr = conn.local_addr()?;
        let server = Server::new(ServerConfig {
            conn_configs: vec![ConnConfig {
                conn,
                relay_addr_generator: Box::new(CappedGenerator {
                    inner: RelayAddressGeneratorRanges {
                        relay_address: cfg.public_ip,
                        min_port: cfg.relay_ports.0,
                        max_port: cfg.relay_ports.1,
                        max_retries: 10,
                        address: cfg.listen.ip().to_string(),
                        net: Arc::new(Net::new(None)),
                    },
                    per_allocation: cfg.per_allocation_bytes,
                    usage: cfg.usage.clone(),
                }),
            }],
            realm: REALM.into(),
            auth_handler: Arc::new(LongTermAuthHandler::new(cfg.shared_secret.clone())),
            channel_bind_timeout: Duration::from_secs(600),
            alloc_close_notify: None,
        })
        .await
        .context("starting TURN server")?;
        Ok(Self { server, local_addr })
    }

    pub async fn close(&self) -> Result<()> {
        Ok(self.server.close().await?)
    }
}

/// Mints ICE server entries with fresh TURN credentials.
#[derive(Debug, Clone)]
pub struct TurnMinter {
    pub shared_secret: String,
    /// e.g. `turn:203.0.113.7:3478?transport=udp`
    pub turn_urls: Vec<String>,
    /// e.g. `stun:203.0.113.7:3478` (the relay answers STUN binding requests too)
    pub stun_urls: Vec<String>,
    /// The built-in relay's usage: no TURN credentials while its monthly cap is reached.
    pub usage: Option<Arc<RelayUsage>>,
}

impl TurnMinter {
    pub fn for_relay(public: SocketAddr, shared_secret: String) -> Self {
        Self {
            shared_secret,
            turn_urls: vec![format!("turn:{public}?transport=udp")],
            stun_urls: vec![format!("stun:{public}")],
            usage: None,
        }
    }

    /// Stop offering TURN while `usage` is over its monthly cap.
    pub fn with_usage(mut self, usage: Arc<RelayUsage>) -> Self {
        self.usage = Some(usage);
        self
    }

    pub fn mint(&self) -> Vec<IceServer> {
        let mut out = Vec::new();
        if !self.stun_urls.is_empty() {
            out.push(IceServer {
                urls: self.stun_urls.clone(),
                username: None,
                credential: None,
            });
        }
        if self.usage.as_ref().is_some_and(|u| u.exhausted()) {
            return out;
        }
        match generate_long_term_credentials(&self.shared_secret, CREDENTIAL_TTL) {
            Ok((username, password)) => out.push(IceServer {
                urls: self.turn_urls.clone(),
                username: Some(username),
                credential: Some(password),
            }),
            Err(e) => tracing::error!(error = %e, "could not mint TURN credentials"),
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn udp() -> Arc<dyn Conn + Send + Sync> {
        Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap())
    }

    #[test]
    fn months_are_utc_calendar_months() {
        assert_eq!(month_of(0), 197_001);
        assert_eq!(month_of(1_764_000_000), 202_511); // 2025-11-24
        assert_eq!(month_of(1_767_225_599), 202_512); // 2025-12-31 23:59:59
        assert_eq!(month_of(1_767_225_600), 202_601); // 2026-01-01
        assert_eq!(month_of(1_709_164_800), 202_402); // 2024-02-29 (leap day)
    }

    #[tokio::test]
    async fn an_allocation_stops_relaying_at_its_cap() {
        let usage = RelayUsage::unlimited();
        let (a, b) = (udp().await, udp().await);
        let to_b = b.local_addr().unwrap();
        let capped = Arc::new(CappedConn::new(a, Some(1000), usage.clone()));
        assert_eq!(capped.send_to(&[0; 600], to_b).await.unwrap(), 600);
        assert!(
            capped.send_to(&[0; 600], to_b).await.is_err(),
            "over the cap"
        );
        assert!(capped.send_to(&[0; 1], to_b).await.is_err(), "stays closed");
        // A receive (the TURN server's packet loop always has one waiting) fails too, so
        // the server drops the allocation.
        let mut buf = [0u8; 64];
        assert!(capped.recv_from(&mut buf).await.is_err());
        assert_eq!(capped.used(), 1200);
    }

    #[tokio::test]
    async fn the_monthly_cap_closes_every_allocation_and_stops_offering_turn() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("relay-usage.json");
        let usage = Arc::new(RelayUsage::new(Some(1500), Some(file.clone())));
        let (a, b, peer) = (udp().await, udp().await, udp().await);
        let to_peer = peer.local_addr().unwrap();
        let first = Arc::new(CappedConn::new(a, None, usage.clone()));
        let second = Arc::new(CappedConn::new(b, None, usage.clone()));
        // `second` is waiting for a packet when `first` uses up the month.
        let waiting = {
            let second = second.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                second.recv_from(&mut buf).await.is_err()
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        first.send_to(&[0; 1000], to_peer).await.unwrap();
        assert!(first.send_to(&[0; 1000], to_peer).await.is_err());
        let ended = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .expect("a waiting relay must end when the month's cap is reached")
            .unwrap();
        assert!(ended);
        assert!(usage.exhausted());

        // Meanwhile Welcome carries only STUN, no TURN credentials.
        let minter = TurnMinter::for_relay("127.0.0.1:3478".parse().unwrap(), "s".into())
            .with_usage(usage.clone());
        let servers = minter.mint();
        assert!(!servers.is_empty());
        assert!(
            servers.iter().all(|s| s.credential.is_none()),
            "{servers:?}"
        );

        // The count survives a restart within the month.
        usage.save();
        let again = RelayUsage::new(Some(1500), Some(file));
        assert_eq!(again.used(), 2000);
        assert!(again.exhausted());
    }
}
