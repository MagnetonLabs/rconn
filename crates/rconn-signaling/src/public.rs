//! Hosts without an account (AnyDesk-style): a computer sends its public key and gets an
//! ID and a certificate from the server's *public* CA, a separate org that no operator
//! belongs to. Such hosts are owned by nobody: they never show up in a computer list, and
//! every session to them must know their access password.
//!
//! Enrollment is rate limited per source address and globally, because each one costs the
//! server a registry entry and its 9-digit ID space. The registry (append-only JSON lines)
//! keeps IDs unique and records each host's owner (`null` until an account claims it).

use rconn_core::fleet::FleetCa;
use rconn_core::ids::{HostId, OrgId};
use rconn_core::owner::HostOwner;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Abuse limits for anonymous enrollment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnrollLimits {
    /// Enrollments per source address (IPv6: per /64) per hour.
    pub per_source_per_hour: usize,
    /// Enrollments per hour for the whole server.
    pub global_per_hour: usize,
}

impl Default for EnrollLimits {
    fn default() -> Self {
        Self {
            per_source_per_hour: 5,
            global_per_hour: 1000,
        }
    }
}

const HOUR: Duration = Duration::from_secs(3600);

/// One registry line.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    host_id: HostId,
    /// Unix seconds.
    #[serde(default)]
    enrolled: u64,
    /// `null`: nobody. A later line for the same host replaces earlier ones.
    #[serde(default)]
    owner: Option<HostOwner>,
}

struct State {
    owners: HashMap<HostId, Option<HostOwner>>,
    /// Short codes in use (as numbers), so a new ID never repeats an existing one.
    codes: HashSet<u32>,
    per_source: HashMap<IpAddr, VecDeque<Instant>>,
    global: VecDeque<Instant>,
}

/// The server's public CA and the hosts it enrolled.
pub struct PublicHosts {
    ca: FleetCa,
    /// `None`: hosts enrolled earlier still connect, but no new ones are accepted.
    limits: Option<EnrollLimits>,
    file: Option<PathBuf>,
    state: Mutex<State>,
}

/// Why an anonymous enrollment was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Disabled,
    SourceLimit,
    GlobalLimit,
    BadKey,
    Internal,
}

impl Refusal {
    pub fn message(self) -> &'static str {
        match self {
            Self::Disabled => "this server doesn't give IDs to computers without an account",
            Self::SourceLimit => "too many new IDs from your network; try again in an hour",
            Self::GlobalLimit => "the server is handing out too many new IDs; try again later",
            Self::BadKey => "not a valid Ed25519 public key",
            Self::Internal => "the server couldn't record the new ID; try again later",
        }
    }
}

pub fn short_code_number(id: HostId) -> u32 {
    (id.0.as_u128() % 1_000_000_000) as u32
}

/// Rate-limit bucket for a source address: IPv4 as is, IPv6 by /64 (one subscriber).
pub fn source_bucket(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let s = v6.segments();
                IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        },
    }
}

/// Every host in a registry file and its owner (empty if the file doesn't exist).
fn read_registry(path: &std::path::Path) -> anyhow::Result<HashMap<HostId, Option<HostOwner>>> {
    let mut owners = HashMap::new();
    if !path.exists() {
        return Ok(owners);
    }
    let text = std::fs::read_to_string(path)?;
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Entry>(line) {
            Ok(e) => {
                owners.insert(e.host_id, e.owner);
            }
            // A torn last line (crash mid-write) must not stop the server.
            Err(e) => {
                tracing::warn!(line = n + 1, error = %e, "skipping bad registry line")
            }
        }
    }
    Ok(owners)
}

/// The IDs in a registry file (`public-hosts.jsonl`), without loading its CA.
pub fn registry_ids(path: &std::path::Path) -> anyhow::Result<HashSet<HostId>> {
    Ok(read_registry(path)?.into_keys().collect())
}

impl PublicHosts {
    /// `file`: the registry to load and append to (`None` keeps it in memory, for tests).
    pub fn new(
        ca: FleetCa,
        limits: Option<EnrollLimits>,
        file: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let owners = match &file {
            Some(path) => read_registry(path)?,
            None => HashMap::new(),
        };
        let codes = owners.keys().map(|h| short_code_number(*h)).collect();
        Ok(Self {
            ca,
            limits,
            file,
            state: Mutex::new(State {
                owners,
                codes,
                per_source: HashMap::new(),
                global: VecDeque::new(),
            }),
        })
    }

    pub fn org_id(&self) -> OrgId {
        self.ca.org_id()
    }

    pub fn ca_pem(&self) -> &str {
        self.ca.cert_pem()
    }

    pub fn enrollment_open(&self) -> bool {
        self.limits.is_some()
    }

    /// Number of hosts in the registry.
    pub fn len(&self) -> usize {
        self.state.lock().expect("public hosts").owners.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The owner of a host certified by the public CA. Unknown hosts (registry lost)
    /// belong to nobody, which is the safe answer.
    pub fn owner(&self, host: HostId) -> Option<HostOwner> {
        self.state
            .lock()
            .expect("public hosts")
            .owners
            .get(&host)
            .copied()
            .flatten()
    }

    /// Is this short code taken by a host in the registry?
    pub fn code_taken(&self, code: u32) -> bool {
        self.state
            .lock()
            .expect("public hosts")
            .codes
            .contains(&code)
    }

    /// Issue an ID and certificate for `public_key_der`, after the rate limits. `in_use`
    /// says whether a short code is taken elsewhere (e.g. by an online org host).
    pub fn enroll(
        &self,
        source: Option<IpAddr>,
        public_key_der: &[u8],
        in_use: impl Fn(u32) -> bool,
    ) -> Result<(HostId, String), Refusal> {
        let limits = self.limits.ok_or(Refusal::Disabled)?;
        let now = Instant::now();
        let mut st = self.state.lock().expect("public hosts");
        st.global.retain(|t| now.duration_since(*t) < HOUR);
        if st.global.len() >= limits.global_per_hour {
            return Err(Refusal::GlobalLimit);
        }
        // Unknown source (no address available) shares one bucket: conservative.
        let bucket = source
            .map(source_bucket)
            .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        let recent = st.per_source.entry(bucket).or_default();
        recent.retain(|t| now.duration_since(*t) < HOUR);
        if recent.len() >= limits.per_source_per_hour {
            return Err(Refusal::SourceLimit);
        }
        let host_id = loop {
            let id = HostId::new();
            let code = short_code_number(id);
            if !st.codes.contains(&code) && !in_use(code) {
                break id;
            }
        };
        let cert = self
            .ca
            .issue_host_cert_for_public_key(host_id, public_key_der)
            .map_err(|_| Refusal::BadKey)?;
        if let Some(path) = &self.file {
            let line = serde_json::to_string(&Entry {
                host_id,
                enrolled: crate::unix_now(),
                owner: None,
            })
            .map_err(|_| Refusal::Internal)?;
            let written = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut f| writeln!(f, "{line}").and_then(|_| f.flush()));
            if let Err(e) = written {
                tracing::error!(error = %e, path = %path.display(), "could not record a new host");
                return Err(Refusal::Internal);
            }
        }
        // Count issued IDs (a refused key cost only one cheap parse on its own connection).
        st.global.push_back(now);
        st.per_source.entry(bucket).or_default().push_back(now);
        // Keep the source table from growing without bound under a spray of addresses.
        if st.per_source.len() > 100_000 {
            st.per_source
                .retain(|_, v| v.back().is_some_and(|t| now.duration_since(*t) < HOUR));
        }
        st.owners.insert(host_id, None);
        st.codes.insert(short_code_number(host_id));
        Ok((host_id, cert))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rconn_core::fleet::HostKey;

    fn hosts(limits: EnrollLimits, file: Option<PathBuf>) -> PublicHosts {
        PublicHosts::new(FleetCa::generate(OrgId::new()).unwrap(), Some(limits), file).unwrap()
    }

    #[test]
    fn per_source_and_global_limits_and_ipv6_buckets() {
        let h = hosts(
            EnrollLimits {
                per_source_per_hour: 2,
                global_per_hour: 3,
            },
            None,
        );
        let key = HostKey::generate().unwrap().public_der();
        let a: IpAddr = "203.0.113.7".parse().unwrap();
        let v6a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let v6b: IpAddr = "2001:db8:1:2:bbbb::9".parse().unwrap();
        assert!(h.enroll(Some(a), &key, |_| false).is_ok());
        assert!(h.enroll(Some(a), &key, |_| false).is_ok());
        assert_eq!(
            h.enroll(Some(a), &key, |_| false),
            Err(Refusal::SourceLimit)
        );
        assert!(h.enroll(Some(v6a), &key, |_| false).is_ok());
        // Same /64, and now the global cap (3) is reached anyway.
        assert_eq!(
            h.enroll(Some(v6b), &key, |_| false),
            Err(Refusal::GlobalLimit)
        );
        assert_eq!(source_bucket(v6a), source_bucket(v6b));
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn registry_survives_restart_and_codes_stay_unique() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("public-hosts.jsonl");
        let ca = FleetCa::generate(OrgId::new()).unwrap();
        let (ca_cert, ca_key) = (ca.cert_pem().to_owned(), ca.key_pem().to_owned());
        let h = PublicHosts::new(ca, Some(EnrollLimits::default()), Some(file.clone())).unwrap();
        let key = HostKey::generate().unwrap().public_der();
        let (id, cert) = h.enroll(None, &key, |_| false).unwrap();
        let claims = rconn_core::fleet::verify_host_cert(h.ca_pem(), &cert).unwrap();
        assert_eq!((claims.host_id, claims.org_id), (id, h.org_id()));
        assert_eq!(h.owner(id), None);
        // A torn line is skipped, not fatal.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap()
            .write_all(b"{\"host_id\":")
            .unwrap();
        let again = PublicHosts::new(
            FleetCa::from_pem(&ca_cert, &ca_key).unwrap(),
            None,
            Some(file),
        )
        .unwrap();
        assert_eq!(again.len(), 1);
        assert!(again.code_taken(short_code_number(id)));
        assert_eq!(
            again.enroll(None, &key, |_| false),
            Err(Refusal::Disabled),
            "closed enrollment still lets existing hosts in, but issues no new IDs"
        );
        // A taken code is never handed out again: block every code but a fresh one's.
        let h2 = hosts(EnrollLimits::default(), None);
        let (id2, _) = h2
            .enroll(None, &key, |c| c == short_code_number(id))
            .unwrap();
        assert_ne!(short_code_number(id2), short_code_number(id));
    }
}
