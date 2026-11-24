//! Viewer client. The egui UI arrives with video in phase 3; today `ping` opens a real
//! session to a host and measures data-channel round trips.

use anyhow::{Result, bail};
use bytes::BytesMut;
use clap::{Parser, Subcommand};
use rconn_core::ids::HostId;
use rconn_core::protocol::{
    ClientMessage, Control, DATA_CHANNEL_PROTOCOL, DataMessage, SdpType, ServerMessage,
    SignalPayload,
};
use rconn_peer::rtc::{Peer, PeerEvent};
use rconn_peer::signaling::SignalingClient;
use std::time::{Duration, Instant};
use webrtc::data_channel::{DataChannelEvent, RTCDataChannelInit};

#[derive(Parser)]
#[command(version, about = "rconn viewer")]
struct Cli {
    #[arg(long, env = "RCONN_SERVER")]
    server: String,
    #[arg(long, env = "RCONN_TOKEN", hide_env_values = true)]
    token: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open a session to a host and measure data-channel round-trip time.
    Ping {
        host: uuid::Uuid,
        #[arg(long, default_value_t = 20)]
        count: u32,
        #[arg(long, default_value = "0.0.0.0:0")]
        udp: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Ping { host, count, udp } => {
            ping(&cli.server, &cli.token, HostId(host), count, udp).await
        }
    }
}

async fn ping(server: &str, token: &str, host: HostId, count: u32, udp: Vec<String>) -> Result<()> {
    let t0 = Instant::now();
    let mut sig = SignalingClient::connect_operator(server, token).await?;
    sig.send(&ClientMessage::RequestSession { host_id: host })
        .await?;
    let session_id = match sig.recv().await? {
        ServerMessage::SessionReady { session_id } => session_id,
        ServerMessage::Error { code, message } => bail!("session refused: {code:?}: {message}"),
        other => bail!("unexpected: {other:?}"),
    };

    let mut peer = Peer::new(&sig.ice_servers, udp).await?;
    let init = RTCDataChannelInit {
        ordered: true,
        protocol: DATA_CHANNEL_PROTOCOL.into(),
        ..Default::default()
    };
    let dc = peer.pc.create_data_channel("rconn", Some(init)).await?;
    let offer = peer.offer().await?;
    sig.send(&ClientMessage::Signal {
        session_id,
        payload: offer,
    })
    .await?;
    let ServerMessage::Signal {
        payload:
            SignalPayload::Sdp {
                sdp_type: SdpType::Answer,
                sdp,
            },
        ..
    } = sig.recv().await?
    else {
        bail!("expected answer");
    };
    peer.accept_answer(sdp).await?;
    tokio::spawn(async move {
        while let Some(ev) = peer.events.recv().await {
            if let PeerEvent::State(s) = ev {
                tracing::info!(state = %s, "peer connection");
            }
        }
    });

    let mut rtts = Vec::new();
    let mut sent_at = Instant::now();
    let mut seq = 0u64;
    let send_ping = |n: u64| async move { DataMessage::Control(Control::Ping(n)).encode() };
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(ev) = dc.poll().await {
            match ev {
                DataChannelEvent::OnOpen => {
                    println!(
                        "connected in {:.0} ms (signaling + ICE + DTLS + SCTP)",
                        t0.elapsed().as_secs_f64() * 1e3
                    );
                    sent_at = Instant::now();
                    dc.send(BytesMut::from(&send_ping(seq).await?[..])).await?;
                }
                DataChannelEvent::OnMessage(msg) => {
                    if DataMessage::decode(&msg.data)? == DataMessage::Control(Control::Pong(seq)) {
                        rtts.push(sent_at.elapsed());
                        seq += 1;
                        if seq == u64::from(count) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        sent_at = Instant::now();
                        dc.send(BytesMut::from(&send_ping(seq).await?[..])).await?;
                    }
                }
                DataChannelEvent::OnClose => bail!("data channel closed early"),
                _ => {}
            }
        }
        Ok(())
    })
    .await;
    match result {
        Ok(r) => r?,
        Err(_) => bail!("timed out after {} of {count} pongs", rtts.len()),
    }
    let _ = sig.send(&ClientMessage::EndSession { session_id }).await;

    rtts.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    println!(
        "{} pings: min {:.2} ms, median {:.2} ms, max {:.2} ms",
        rtts.len(),
        ms(rtts[0]),
        ms(rtts[rtts.len() / 2]),
        ms(rtts[rtts.len() - 1])
    );
    Ok(())
}
