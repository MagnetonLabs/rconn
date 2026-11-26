//! WebSocket client for the signaling server.

use crate::trust::ServerTrust;
use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use rconn_core::PROTOCOL_VERSION;
use rconn_core::fleet::{HostKey, host_proof_transcript};
use rconn_core::ids::{HostId, OrgId};
use rconn_core::protocol::{CHALLENGE_LEN, ClientMessage, IceServer, ServerMessage};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async, connect_async_tls_with_config,
};

pub struct SignalingClient {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    pub org_id: OrgId,
    pub ice_servers: Vec<IceServer>,
}

/// What a host needs to authenticate.
pub struct HostCredentials<'a> {
    /// The server identity this host is enrolled with (its profile URL). Bound into the proof.
    pub server_id: &'a str,
    pub host_id: HostId,
    pub key: &'a HostKey,
    pub cert_pem: &'a str,
    /// Name shown in viewers' computer lists (usually the hostname).
    pub label: Option<String>,
}

impl SignalingClient {
    pub async fn connect_host(
        url: &str,
        trust: &ServerTrust,
        creds: HostCredentials<'_>,
    ) -> Result<Self> {
        let mut ws = open(url, trust).await?;
        let client_nonce: [u8; CHALLENGE_LEN] = random_nonce();
        send_on(
            &mut ws,
            &ClientMessage::HostHello {
                protocol_version: PROTOCOL_VERSION,
                host_id: creds.host_id,
                cert_pem: creds.cert_pem.to_owned(),
                client_nonce: client_nonce.to_vec(),
                label: creds.label.clone(),
            },
        )
        .await?;
        let ServerMessage::Challenge { nonce } = recv_on(&mut ws).await? else {
            bail!("expected challenge from server");
        };
        let transcript = host_proof_transcript(
            creds.server_id,
            creds.host_id,
            creds.cert_pem,
            &client_nonce,
            &nonce,
        );
        send_on(
            &mut ws,
            &ClientMessage::HostProof {
                signature: creds.key.sign(&transcript)?,
            },
        )
        .await?;
        Self::finish(ws).await
    }

    pub async fn connect_operator(url: &str, trust: &ServerTrust, token: &str) -> Result<Self> {
        let mut ws = open(url, trust).await?;
        send_on(
            &mut ws,
            &ClientMessage::OperatorHello {
                protocol_version: PROTOCOL_VERSION,
                token: token.to_owned(),
            },
        )
        .await?;
        Self::finish(ws).await
    }

    /// Connect without an account, to the computer with the ID `typed` (its access password
    /// is checked later by the host itself). Returns the computer's full id.
    pub async fn connect_guest(
        url: &str,
        trust: &ServerTrust,
        typed: &str,
    ) -> Result<(Self, HostId)> {
        let mut ws = open(url, trust).await?;
        send_on(
            &mut ws,
            &ClientMessage::GuestHello {
                protocol_version: PROTOCOL_VERSION,
                host: typed.to_owned(),
            },
        )
        .await?;
        let mut client = Self::finish(ws).await?;
        match client.recv().await? {
            ServerMessage::GuestHost { host_id } => Ok((client, host_id)),
            ServerMessage::Error { code, message } => bail!("refused: {code:?}: {message}"),
            other => bail!("unexpected answer to a guest connection: {other:?}"),
        }
    }

    /// Exchange a username and password for a session token `(token, expires_at)`.
    /// Refuses plaintext `ws://` except to loopback, so a password never crosses a network
    /// unencrypted.
    pub async fn password_login(
        url: &str,
        trust: &ServerTrust,
        username: &str,
        password: &str,
    ) -> Result<(String, u64)> {
        if url.starts_with("ws://") && !is_loopback_url(url) {
            bail!("refusing to send a password over plaintext ws://; use wss://");
        }
        let mut ws = open(url, trust).await?;
        send_on(
            &mut ws,
            &ClientMessage::PasswordLogin {
                protocol_version: PROTOCOL_VERSION,
                username: username.to_owned(),
                password: password.to_owned(),
            },
        )
        .await?;
        match recv_on(&mut ws).await? {
            ServerMessage::LoginToken { token, expires_at } => Ok((token, expires_at)),
            ServerMessage::Error { message, .. } => bail!("login failed: {message}"),
            other => bail!("unexpected login reply: {other:?}"),
        }
    }

    /// Turn this computer into a host: returns its new id and certificate for `key`.
    pub async fn enroll_host(
        url: &str,
        trust: &ServerTrust,
        token: &str,
        key: &rconn_core::fleet::HostKey,
    ) -> Result<(rconn_core::ids::HostId, String)> {
        let mut c = Self::connect_operator(url, trust, token).await?;
        c.send(&ClientMessage::EnrollHost {
            public_key_der: key.public_der(),
        })
        .await?;
        loop {
            match c.recv().await? {
                ServerMessage::HostEnrolled { host_id, cert_pem } => {
                    return Ok((host_id, cert_pem));
                }
                ServerMessage::Error { message, .. } => bail!("{message}"),
                _ => continue,
            }
        }
    }

    /// Get an ID for this computer without an account (servers that allow it, like the
    /// public one): returns its new id and certificate for `key`. The host belongs to
    /// nobody, so every session to it must know its access password.
    pub async fn enroll_anonymous(
        url: &str,
        trust: &ServerTrust,
        key: &rconn_core::fleet::HostKey,
    ) -> Result<(HostId, String)> {
        let mut ws = open(url, trust).await?;
        send_on(
            &mut ws,
            &ClientMessage::EnrollAnonymous {
                protocol_version: PROTOCOL_VERSION,
                public_key_der: key.public_der(),
            },
        )
        .await?;
        match recv_on(&mut ws).await? {
            ServerMessage::HostEnrolled { host_id, cert_pem } => Ok((host_id, cert_pem)),
            ServerMessage::Error { message, .. } => bail!("{message}"),
            other => bail!("unexpected answer to enrollment: {other:?}"),
        }
    }

    /// The org's online computers (needs an operator token).
    pub async fn list_hosts(
        url: &str,
        trust: &ServerTrust,
        token: &str,
    ) -> Result<Vec<rconn_core::protocol::HostInfo>> {
        let mut c = Self::connect_operator(url, trust, token).await?;
        c.send(&ClientMessage::ListHosts).await?;
        loop {
            match c.recv().await? {
                ServerMessage::Hosts { hosts } => return Ok(hosts),
                ServerMessage::Error { message, .. } => bail!("{message}"),
                _ => continue,
            }
        }
    }

    /// Which login methods the server offers: (password accounts?, OIDC provider).
    pub async fn login_options(
        url: &str,
        trust: &ServerTrust,
    ) -> Result<(bool, Option<rconn_core::protocol::OidcLoginInfo>)> {
        let mut ws = open(url, trust).await?;
        send_on(
            &mut ws,
            &ClientMessage::GetLoginOptions {
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .await?;
        match recv_on(&mut ws).await? {
            ServerMessage::LoginOptions { password, oidc } => Ok((password, oidc)),
            ServerMessage::Error { message, .. } => bail!("{message}"),
            other => bail!("unexpected reply: {other:?}"),
        }
    }

    /// Exchange an ID token from the server's IdP for a session token `(token, expires_at)`.
    pub async fn oidc_login(
        url: &str,
        trust: &ServerTrust,
        id_token: &str,
    ) -> Result<(String, u64)> {
        let mut ws = open(url, trust).await?;
        send_on(
            &mut ws,
            &ClientMessage::OidcLogin {
                protocol_version: PROTOCOL_VERSION,
                id_token: id_token.to_owned(),
            },
        )
        .await?;
        match recv_on(&mut ws).await? {
            ServerMessage::LoginToken { token, expires_at } => Ok((token, expires_at)),
            ServerMessage::Error { message, .. } => bail!("login failed: {message}"),
            other => bail!("unexpected login reply: {other:?}"),
        }
    }

    async fn finish(mut ws: WebSocketStream<MaybeTlsStream<TcpStream>>) -> Result<Self> {
        match recv_on(&mut ws).await? {
            ServerMessage::Welcome {
                org_id,
                ice_servers,
                ..
            } => Ok(Self {
                ws,
                org_id,
                ice_servers,
            }),
            ServerMessage::Error { code, message } => {
                bail!("signaling refused: {code:?}: {message}")
            }
            other => bail!("unexpected first message: {other:?}"),
        }
    }

    pub async fn send(&mut self, msg: &ClientMessage) -> Result<()> {
        send_on(&mut self.ws, msg).await
    }

    /// Next message from the server, skipping ones this build doesn't understand.
    pub async fn recv(&mut self) -> Result<ServerMessage> {
        loop {
            match recv_on(&mut self.ws).await? {
                ServerMessage::Unknown => continue,
                m => return Ok(m),
            }
        }
    }
}

/// How often an idle signaling connection pings the server.
pub const KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(20);

/// Cloneable sending half of a split [`SignalingClient`].
#[derive(Clone)]
pub struct SignalingTx(tokio::sync::mpsc::UnboundedSender<ClientMessage>);

impl SignalingTx {
    pub fn send(&self, msg: ClientMessage) -> Result<()> {
        self.0
            .send(msg)
            .map_err(|_| anyhow::anyhow!("signaling connection closed"))
    }
}

impl SignalingClient {
    /// Hand the socket to a background task. Returns a cloneable sender and the stream of
    /// server messages (unknown ones already dropped). The stream ends when the socket does.
    pub fn split(
        self,
    ) -> (
        SignalingTx,
        tokio::sync::mpsc::UnboundedReceiver<ServerMessage>,
    ) {
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<ClientMessage>();
        let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();
        let mut ws = self.ws;
        // One task owns both directions: if either fails, the whole connection ends (both
        // channels close together), so callers notice immediately and reconnect, instead of
        // a dead writer surfacing only at the next send. Pings keep NAT mappings alive and
        // detect silently dead connections.
        tokio::spawn(async move {
            let mut ping = tokio::time::interval(KEEPALIVE);
            ping.tick().await; // first tick is immediate
            let reason: String = loop {
                tokio::select! {
                    out = out_rx.recv() => {
                        let Some(msg) = out else { break "client closed".into() };
                        let Ok(text) = msg.to_json() else { continue };
                        if let Err(e) = ws.send(Message::Text(text.into())).await {
                            break format!("send failed: {e}");
                        }
                    }
                    frame = ws.next() => match frame {
                        Some(Ok(Message::Text(t))) => match ServerMessage::from_json(&t) {
                            Ok(ServerMessage::Unknown) => {}
                            Ok(m) => {
                                if in_tx.send(m).is_err() {
                                    break "receiver dropped".into();
                                }
                            }
                            Err(e) => tracing::warn!(error = %e, "bad server message"),
                        },
                        Some(Ok(Message::Close(c))) => break format!("server closed: {c:?}"),
                        Some(Ok(_)) => {} // ping/pong/binary
                        Some(Err(e)) => break format!("receive failed: {e}"),
                        None => break "connection ended".into(),
                    },
                    _ = ping.tick() => {
                        if let Err(e) = ws.send(Message::Ping(Vec::new().into())).await {
                            break format!("keepalive failed: {e}");
                        }
                    }
                }
            };
            tracing::info!(%reason, "signaling connection closed");
            let _ = ws.close(None).await;
        });
        (SignalingTx(out_tx), in_rx)
    }
}

/// Open the WebSocket, applying `trust` to `wss://`. Plain `ws://` is for local testing
/// only and is refused for pinned profiles, which exist precisely to authenticate the server.
async fn open(
    url: &str,
    trust: &ServerTrust,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    if url.starts_with("wss://") {
        let connector = Connector::Rustls(Arc::new(trust.client_config()?));
        let (ws, _) = connect_async_tls_with_config(url, None, false, Some(connector))
            .await
            .context("connecting to signaling server")?;
        Ok(ws)
    } else if url.starts_with("ws://") {
        if matches!(trust, ServerTrust::Pinned(_)) {
            bail!("server is pinned but the URL is plaintext ws://; use wss://");
        }
        tracing::warn!(%url, "plaintext signaling connection (testing only)");
        let (ws, _) = connect_async(url)
            .await
            .context("connecting to signaling server")?;
        Ok(ws)
    } else {
        bail!("signaling URL must start with wss:// (or ws:// for local testing)");
    }
}

fn is_loopback_url(url: &str) -> bool {
    let rest = url.trim_start_matches("ws://");
    let host = rest.split(['/', ':']).next().unwrap_or("");
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn random_nonce() -> [u8; CHALLENGE_LEN] {
    // Two v4 UUIDs = 32 bytes from the OS RNG (uuid uses getrandom).
    let mut n = [0u8; CHALLENGE_LEN];
    n[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    n[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    n
}

async fn send_on(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    msg: &ClientMessage,
) -> Result<()> {
    ws.send(Message::Text(msg.to_json()?.into()))
        .await
        .context("signaling send")
}

async fn recv_on(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) -> Result<ServerMessage> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return Ok(ServerMessage::from_json(&t)?),
            Some(Ok(Message::Close(_))) | None => bail!("signaling connection closed"),
            Some(Err(e)) => return Err(e).context("signaling receive"),
            Some(Ok(_)) => {}
        }
    }
}
