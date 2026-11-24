//! Viewer client. The egui UI arrives with video in phase 3; today `ping` opens a real
//! session to a host and measures data-channel round trips.

use anyhow::{Result, bail};
use bytes::BytesMut;
use clap::{Parser, Subcommand};
use rconn_core::ids::HostId;
use rconn_core::protocol::{Control, DataMessage};
use rconn_peer::rtc::PeerOptions;
use rconn_peer::session::open_viewer_session;
use rconn_peer::signaling::SignalingClient;
use rconn_peer::trust::ServerTrust;
use std::time::{Duration, Instant};
use webrtc::data_channel::DataChannelEvent;

#[derive(Parser)]
#[command(version, about = "rconn viewer")]
struct Cli {
    #[arg(long, env = "RCONN_SERVER")]
    server: String,
    #[arg(long, env = "RCONN_TOKEN", hide_env_values = true)]
    token: String,
    /// Pinned server key (`sha256:...`). Omit for servers with a public certificate.
    #[arg(long, env = "RCONN_FINGERPRINT")]
    fingerprint: Option<String>,
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
        /// Force the TURN relay path.
        #[arg(long)]
        relay_only: bool,
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
        Cmd::Ping {
            host,
            count,
            udp,
            relay_only,
        } => {
            ping(
                &cli.server,
                &ServerTrust::from_fingerprint(cli.fingerprint.as_deref()),
                &cli.token,
                HostId(host),
                count,
                udp,
                relay_only,
            )
            .await
        }
    }
}

async fn ping(
    server: &str,
    trust: &ServerTrust,
    token: &str,
    host: HostId,
    count: u32,
    udp: Vec<String>,
    relay_only: bool,
) -> Result<()> {
    let t0 = Instant::now();
    let sig = SignalingClient::connect_operator(server, trust, token).await?;
    let session = open_viewer_session(
        sig,
        host,
        PeerOptions {
            udp_addrs: udp,
            relay_only,
        },
    )
    .await?;
    let dc = session.data.clone();
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
    session.close().await;

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
