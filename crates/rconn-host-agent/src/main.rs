//! Per-session host agent: connects to signaling with the host identity, answers
//! sessions, streams the screen, and serves the data channel. Input and audio attach
//! here in phases 4 and 7.

mod input;
mod stream;

use anyhow::{Context, Result};
use bytes::BytesMut;
use clap::Parser;
use rconn_core::fleet::HostKey;
use rconn_core::ids::HostId;
use rconn_core::protocol::{Control, DATA_CHANNEL_PROTOCOL, DataMessage};
use rconn_media::capture::{Capturer, TestPattern};
use rconn_peer::rtc::PeerOptions;
use rconn_peer::session::{HostSession, SessionHandler, run_host};
use rconn_peer::signaling::{HostCredentials, SignalingClient};
use rconn_peer::trust::ServerTrust;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use stream::{KeyframeRequests, StreamConfig};
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::peer_connection::RTCPeerConnectionState;

#[derive(Parser, Clone)]
#[command(version, about = "rconn host agent")]
struct Cli {
    /// Check that input injection reaches the active desktop, then exit.
    #[arg(long, exclusive = true)]
    selftest_input: bool,
    /// Signaling WebSocket URL, e.g. wss://rconn.example.com/v1/ws
    #[arg(long, env = "RCONN_SERVER", required_unless_present = "selftest_input")]
    server: Option<String>,
    /// Server identity this host was enrolled for (bound into the auth proof).
    #[arg(
        long,
        env = "RCONN_SERVER_ID",
        required_unless_present = "selftest_input"
    )]
    server_id: Option<String>,
    /// Pinned server key (`sha256:...` from `rconn-signaling init`). Omit for servers
    /// with a public certificate.
    #[arg(long, env = "RCONN_FINGERPRINT")]
    fingerprint: Option<String>,
    /// Directory with host.id, host.cert.pem and host.key.pem from `enroll-host`.
    #[arg(
        long,
        env = "RCONN_IDENTITY_DIR",
        required_unless_present = "selftest_input"
    )]
    identity: Option<PathBuf>,
    /// Cisco openh264 library. Default: next to this executable.
    #[arg(long, env = "RCONN_OPENH264")]
    openh264: Option<PathBuf>,
    /// Stream a synthetic pattern of this size (e.g. 1280x720) instead of the screen.
    #[arg(long)]
    test_pattern: Option<String>,
    #[arg(long, default_value_t = 30)]
    max_fps: u32,
    #[arg(long, default_value_t = 8000)]
    max_kbps: u32,
    /// Largest streamed size; bigger desktops are halved to fit.
    #[arg(long, default_value = "1920x1080")]
    max_size: String,
    /// Local UDP sockets for ICE.
    #[arg(long, default_value = "0.0.0.0:0")]
    udp: Vec<String>,
}

impl Cli {
    fn openh264_path(&self) -> Result<PathBuf> {
        if let Some(p) = &self.openh264 {
            return Ok(p.clone());
        }
        let name = rconn_media::cisco::current_binary_name()
            .context("no Cisco openh264 build for this platform")?;
        let exe = std::env::current_exe()?;
        Ok(exe
            .parent()
            .context("executable has no directory")?
            .join(name))
    }

    fn capturer_factory(
        &self,
    ) -> Result<impl FnOnce() -> Result<Box<dyn Capturer>> + Send + 'static> {
        let pattern = match &self.test_pattern {
            Some(s) => {
                let (w, h) = s
                    .split_once('x')
                    .context("--test-pattern must look like 1280x720")?;
                Some((w.parse::<usize>()?, h.parse::<usize>()?))
            }
            None => None,
        };
        let fps = self.max_fps;
        Ok(move || -> Result<Box<dyn Capturer>> {
            Ok(match pattern {
                Some((w, h)) => Box::new(TestPattern::new(w, h, fps)),
                None => rconn_media::capture::primary_display()?,
            })
        })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    if cli.selftest_input {
        println!("{}", input::selftest()?);
        return Ok(());
    }
    let (server, server_id, identity) = (
        cli.server.clone().context("--server is required")?,
        cli.server_id.clone().context("--server-id is required")?,
        cli.identity.clone().context("--identity is required")?,
    );
    let read =
        |f: &str| fs::read_to_string(identity.join(f)).with_context(|| format!("reading {f}"));
    let host_id = HostId(
        read("host.id")?
            .trim()
            .parse()
            .context("host.id is not a UUID")?,
    );
    let cert = read("host.cert.pem")?;
    let key = HostKey::from_pem(&read("host.key.pem")?)?;
    let openh264 = cli.openh264_path()?;
    rconn_media::codec::load_openh264(&openh264)
        .with_context(|| format!("loading {}", openh264.display()))?;
    tracing::info!(
        %host_id, protocol = rconn_core::PROTOCOL_VERSION, os = std::env::consts::OS,
        arch = std::env::consts::ARCH, openh264 = %openh264.display(), "host agent starting"
    );

    let trust = ServerTrust::from_fingerprint(cli.fingerprint.as_deref());
    // Reconnect forever with capped backoff: an unattended host must come back by itself.
    let mut backoff = Duration::from_secs(1);
    loop {
        let creds = HostCredentials {
            server_id: &server_id,
            host_id,
            key: &key,
            cert_pem: &cert,
        };
        match SignalingClient::connect_host(&server, &trust, creds).await {
            Ok(sig) => {
                backoff = Duration::from_secs(1);
                tracing::info!("connected to signaling");
                let opts = PeerOptions {
                    udp_addrs: cli.udp.clone(),
                    relay_only: false,
                };
                let cli2 = cli.clone();
                let openh264 = openh264.clone();
                let handler: SessionHandler = Arc::new(move |session: HostSession| {
                    let (cli, openh264) = (cli2.clone(), openh264.clone());
                    tokio::spawn(async move {
                        if let Err(e) = serve_session(session, &cli, openh264).await {
                            tracing::warn!(error = %e, "session failed");
                        }
                    });
                });
                if let Err(e) = run_host(sig, opts, handler).await {
                    tracing::warn!(error = %e, "signaling session ended");
                }
            }
            Err(e) => tracing::warn!(error = %e, "connect failed"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn serve_session(mut session: HostSession, cli: &Cli, openh264: PathBuf) -> Result<()> {
    let keyframes = KeyframeRequests::default();
    let stop = Arc::new(AtomicBool::new(false));

    // Data channels: Ping/Pong and keyframe requests.
    let kf = keyframes.clone();
    let mut channels = std::mem::replace(
        &mut session.channels,
        tokio::sync::mpsc::unbounded_channel().1,
    );
    tokio::spawn(async move {
        while let Some(dc) = channels.recv().await {
            tokio::spawn(serve_channel(dc, kf.clone()));
        }
    });

    // Don't send media before the connection is secured: RTP written before DTLS
    // completes is dropped, and a large first keyframe can starve the handshake.
    let mut states = std::mem::replace(
        &mut session.states,
        tokio::sync::mpsc::unbounded_channel().1,
    );
    let connected = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(s) = states.recv().await {
            match s {
                RTCPeerConnectionState::Connected => return true,
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => return false,
                _ => {}
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    if !connected {
        anyhow::bail!("peer connection did not connect");
    }
    // Stop streaming when the peer connection closes or fails.
    let watch_stop = stop.clone();
    tokio::spawn(async move {
        while states.recv().await.is_some() {}
        watch_stop.store(true, Ordering::Relaxed);
    });

    let (mw, mh) = cli
        .max_size
        .split_once('x')
        .context("--max-size must look like 1920x1080")?;
    let cfg = StreamConfig {
        openh264,
        max_fps: cli.max_fps,
        max_bitrate_bps: cli.max_kbps.saturating_mul(1000),
        max_size: (mw.parse()?, mh.parse()?),
    };
    stream::run(
        cli.capturer_factory()?,
        session.video,
        session.target_bitrate,
        keyframes,
        cfg,
        stop,
    )
    .await
}

async fn serve_channel(dc: Arc<dyn DataChannel>, keyframes: KeyframeRequests) {
    let input = input::InputSink::start();
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
                Ok(DataMessage::Control(Control::RequestKeyframe)) => keyframes.request(),
                Ok(DataMessage::Input(ev)) => input.send(ev),
                Ok(other) => tracing::debug!(?other, "data message (not handled yet)"),
                Err(e) => tracing::warn!(error = %e, "bad data message"),
            },
            DataChannelEvent::OnClose => break,
            _ => {}
        }
    }
}
