//! Per-session host agent: connects to signaling with the host identity, answers
//! sessions, and serves the data channel. Capture, input and audio attach here in
//! phases 3, 4 and 7; today the data channel answers `Ping` with `Pong`.

use anyhow::{Context, Result, bail};
use bytes::BytesMut;
use clap::Parser;
use rconn_core::fleet::HostKey;
use rconn_core::ids::{HostId, SessionId};
use rconn_core::protocol::{
    ClientMessage, Control, DATA_CHANNEL_PROTOCOL, DataMessage, SdpType, ServerMessage,
    SignalPayload,
};
use rconn_peer::rtc::{Peer, PeerEvent};
use rconn_peer::signaling::{HostCredentials, SignalingClient};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use webrtc::data_channel::{DataChannel, DataChannelEvent};

#[derive(Parser)]
#[command(version, about = "rconn host agent")]
struct Cli {
    /// Signaling WebSocket URL, e.g. ws://10.0.0.5:8080/v1/ws
    #[arg(long, env = "RCONN_SERVER")]
    server: String,
    /// Server identity this host was enrolled for (bound into the auth proof).
    #[arg(long, env = "RCONN_SERVER_ID")]
    server_id: String,
    /// Directory with host.id, host.cert.pem and host.key.pem from `enroll-host`.
    #[arg(long, env = "RCONN_IDENTITY_DIR")]
    identity: PathBuf,
    /// Local UDP sockets for ICE.
    #[arg(long, default_value = "0.0.0.0:0")]
    udp: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let read =
        |f: &str| fs::read_to_string(cli.identity.join(f)).with_context(|| format!("reading {f}"));
    let host_id = HostId(
        read("host.id")?
            .trim()
            .parse()
            .context("host.id is not a UUID")?,
    );
    let cert = read("host.cert.pem")?;
    let key = HostKey::from_pem(&read("host.key.pem")?)?;
    tracing::info!(
        %host_id, protocol = rconn_core::PROTOCOL_VERSION, os = std::env::consts::OS, arch = std::env::consts::ARCH,
        openh264 = rconn_media::cisco::current_binary_name(), "host agent starting"
    );

    // Reconnect forever with capped backoff: an unattended host must come back by itself.
    let mut backoff = Duration::from_secs(1);
    loop {
        let creds = HostCredentials {
            server_id: &cli.server_id,
            host_id,
            key: &key,
            cert_pem: &cert,
        };
        match SignalingClient::connect_host(&cli.server, creds).await {
            Ok(sig) => {
                backoff = Duration::from_secs(1);
                tracing::info!("connected to signaling");
                if let Err(e) = run(sig, &cli.udp).await {
                    tracing::warn!(error = %e, "signaling session ended");
                }
            }
            Err(e) => tracing::warn!(error = %e, "connect failed"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn run(mut sig: SignalingClient, udp: &[String]) -> Result<()> {
    // One session at a time in v1; a new IncomingSession replaces the previous peer.
    let mut current: Option<(SessionId, Peer)> = None;
    loop {
        match sig.recv().await? {
            ServerMessage::IncomingSession {
                session_id,
                operator_id,
                policy,
            } => {
                tracing::info!(%session_id, %operator_id, ?policy, "incoming session");
                if let Some((_, old)) = current.take() {
                    let _ = old.pc.close().await;
                }
                let ServerMessage::Signal {
                    payload:
                        SignalPayload::Sdp {
                            sdp_type: SdpType::Offer,
                            sdp,
                        },
                    ..
                } = sig.recv().await?
                else {
                    bail!("expected offer after IncomingSession");
                };
                let mut peer = Peer::new(&sig.ice_servers, udp.to_vec()).await?;
                let answer = peer.answer(sdp).await?;
                sig.send(&ClientMessage::Signal {
                    session_id,
                    payload: answer,
                })
                .await?;
                current = Some((session_id, peer));
                if let Some((_, peer)) = current.as_mut() {
                    spawn_channel_watch(peer);
                }
            }
            ServerMessage::SessionEnded { session_id, reason } => {
                tracing::info!(%session_id, ?reason, "session ended");
                if current.as_ref().is_some_and(|(id, _)| *id == session_id)
                    && let Some((_, peer)) = current.take() {
                        let _ = peer.pc.close().await;
                    }
            }
            other => tracing::debug!(?other, "ignored signaling message"),
        }
    }
}

/// Hand each data channel the remote opens to its own task.
fn spawn_channel_watch(peer: &mut Peer) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    // Move event draining off the signaling loop: forward channels as they appear.
    let mut events = std::mem::replace(&mut peer.events, tokio::sync::mpsc::unbounded_channel().1);
    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            match ev {
                PeerEvent::DataChannel(dc) => {
                    let _ = tx.send(dc);
                }
                PeerEvent::State(s) => tracing::info!(state = %s, "peer connection"),
                PeerEvent::GatheringComplete => {}
            }
        }
    });
    tokio::spawn(async move {
        while let Some(dc) = rx.recv().await {
            tokio::spawn(serve_channel(dc));
        }
    });
}

async fn serve_channel(dc: Arc<dyn DataChannel>) {
    match dc.protocol().await {
        Ok(p) if p == DATA_CHANNEL_PROTOCOL => {}
        other => {
            tracing::warn!(?other, "refusing data channel with unknown protocol");
            let _ = dc.close().await;
            return;
        }
    }
    while let Some(ev) = dc.poll().await {
        match ev {
            DataChannelEvent::OnMessage(msg) => match DataMessage::decode(&msg.data) {
                Ok(DataMessage::Control(Control::Ping(n))) => {
                    if let Ok(pong) = DataMessage::Control(Control::Pong(n)).encode() {
                        let _ = dc.send(BytesMut::from(&pong[..])).await;
                    }
                }
                Ok(other) => tracing::debug!(?other, "data message (not handled yet)"),
                Err(e) => tracing::warn!(error = %e, "bad data message"),
            },
            DataChannelEvent::OnClose => break,
            _ => {}
        }
    }
}
