//! Built-in TURN relay for self-hosted servers, plus minting of the short-lived
//! credentials clients receive in `Welcome`.
//!
//! Credentials follow the TURN REST API convention: username = expiry (Unix seconds),
//! password = base64(HMAC-SHA1(shared_secret, username)). The relay checks both
//! statelessly, so the signaling server and the relay only share the secret.
//! Relayed media stays DTLS-SRTP encrypted end to end; the relay sees ciphertext.

use anyhow::{Context, Result};
use rconn_core::protocol::IceServer;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use turn::auth::{LongTermAuthHandler, generate_long_term_credentials};
use turn::relay::relay_range::RelayAddressGeneratorRanges;
use turn::server::Server;
use turn::server::config::{ConnConfig, ServerConfig};
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
                relay_addr_generator: Box::new(RelayAddressGeneratorRanges {
                    relay_address: cfg.public_ip,
                    min_port: cfg.relay_ports.0,
                    max_port: cfg.relay_ports.1,
                    max_retries: 10,
                    address: cfg.listen.ip().to_string(),
                    net: Arc::new(Net::new(None)),
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
}

impl TurnMinter {
    pub fn for_relay(public: SocketAddr, shared_secret: String) -> Self {
        Self {
            shared_secret,
            turn_urls: vec![format!("turn:{public}?transport=udp")],
            stun_urls: vec![format!("stun:{public}")],
        }
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
