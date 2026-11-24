//! WebSocket client for the signaling server.

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use rconn_core::PROTOCOL_VERSION;
use rconn_core::fleet::{HostKey, host_proof_transcript};
use rconn_core::ids::{HostId, OrgId};
use rconn_core::protocol::{CHALLENGE_LEN, ClientMessage, IceServer, ServerMessage};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

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
    pub async fn connect_host(url: &str, creds: HostCredentials<'_>) -> Result<Self> {
        let (mut ws, _) = connect_async(url)
            .await
            .context("connecting to signaling server")?;
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

    pub async fn connect_operator(url: &str, token: &str) -> Result<Self> {
        let (mut ws, _) = connect_async(url)
            .await
            .context("connecting to signaling server")?;
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
