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
        let (mut sink, mut stream) = self.ws.split();
        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                let Ok(text) = msg.to_json() else { continue };
                if sink.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            while let Some(Ok(frame)) = stream.next().await {
                let Message::Text(t) = frame else { continue };
                match ServerMessage::from_json(&t) {
                    Ok(ServerMessage::Unknown) => {}
                    Ok(m) => {
                        if in_tx.send(m).is_err() {
                            break;
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "bad server message"),
                }
            }
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
