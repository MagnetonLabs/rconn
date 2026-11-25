//! Per-session host agent: connects to signaling with the host identity, answers
//! sessions, streams the screen, and serves the data channel. Input and audio attach
//! here in phases 4 and 7.

mod audio;
mod awake;
mod cursor;
mod hostkind;
mod indicator;
mod input;
mod stream;

use anyhow::{Context, Result};
use bytes::BytesMut;
use clap::Parser;
use rconn_core::fleet::HostKey;
use rconn_core::ids::HostId;
use rconn_core::protocol::{Control, DATA_CHANNEL_PROTOCOL, DataMessage};
use rconn_media::capture::{Capturer, TestPattern};
use rconn_peer::clipboard::ClipboardSync;
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
    /// Send a 440 Hz tone instead of the host's sound (tests, soundless setups).
    #[arg(long)]
    test_tone: bool,
    /// Don't send host sound.
    #[arg(long)]
    no_audio: bool,
    /// Frame-rate cap. Idle screens send far fewer frames; the encoder drops frames when
    /// the CPU can't keep up.
    #[arg(long, default_value_t = 60)]
    max_fps: u32,
    #[arg(long, default_value_t = 8000)]
    max_kbps: u32,
    /// Host settings file (the service passes ProgramData\\rconn\\host.toml).
    #[arg(long, env = "RCONN_SETTINGS")]
    settings: Option<PathBuf>,
    /// User-readable status file for the local app (the service passes one).
    #[arg(long, env = "RCONN_STATUS_FILE")]
    status_file: Option<PathBuf>,
    /// Directory for runtime state (the `session.active` marker that locks settings).
    #[arg(long, env = "RCONN_STATE_DIR")]
    state_dir: Option<PathBuf>,
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
        let dir = exe.parent().context("executable has no directory")?;
        // macOS app bundle: Contents/Resources holds Cisco's (unmodified) library.
        let bundled = dir.parent().map(|c| c.join("Resources").join(name));
        Ok(bundled
            .filter(|p| p.exists())
            .unwrap_or_else(|| dir.join(name)))
    }

    fn capturer_factory(&self) -> Result<stream::CapturerFactory> {
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
        Ok(Arc::new(move || -> Result<Box<dyn Capturer>> {
            Ok(match pattern {
                Some((w, h)) => Box::new(TestPattern::new(w, h, fps)),
                None => rconn_media::capture::primary_display()?,
            })
        }))
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

    let host_kind = hostkind::detect();
    request_permissions();
    raise_priority();
    let status = StatusPublisher::new(
        cli.status_file.clone(),
        host_id,
        &server,
        cli.fingerprint.clone(),
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
            label: Some(machine_name()),
        };
        match SignalingClient::connect_host(&server, &trust, creds).await {
            Ok(sig) => {
                backoff = Duration::from_secs(1);
                tracing::info!("connected to signaling");
                status.set_online(true);
                let opts = PeerOptions {
                    udp_addrs: cli.udp.clone(),
                    relay_only: false,
                };
                let cli2 = cli.clone();
                let openh264 = openh264.clone();
                let status2 = status.clone();
                let handler: SessionHandler = Arc::new(move |session: HostSession| {
                    let (cli, openh264, status) = (cli2.clone(), openh264.clone(), status2.clone());
                    tokio::spawn(async move {
                        status.set_session(true);
                        let result = serve_session(session, &cli, openh264, host_kind).await;
                        status.set_session(false);
                        if let Err(e) = result {
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
        status.set_online(false);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Marks a session as active for its lifetime (settings writes are refused meanwhile).
struct SessionMarker(Option<PathBuf>);

impl SessionMarker {
    fn new(state_dir: Option<&PathBuf>, session: &rconn_core::ids::SessionId) -> Self {
        let path = state_dir.map(|d| d.join("session.active"));
        if let Some(p) = &path
            && let Err(e) = fs::write(p, session.to_string())
        {
            tracing::warn!(error = %e, path = %p.display(), "could not write session marker");
        }
        Self(path)
    }
}

impl Drop for SessionMarker {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            let _ = fs::remove_file(p);
        }
    }
}

async fn serve_session(
    mut session: HostSession,
    cli: &Cli,
    openh264: PathBuf,
    host_kind: rconn_core::settings::HostKind,
) -> Result<()> {
    let settings =
        rconn_core::settings::LoadedSettings::load(rconn_core::settings::SettingsLayers {
            system_file: cli.settings.clone(),
            ..Default::default()
        })?;
    let _marker = SessionMarker::new(cli.state_dir.as_ref(), &session.session_id);
    // Wake the display and keep it on: a sleeping display gives screen capture nothing.
    let _awake = awake::start();
    let show_indicator =
        settings.settings().shows_indicator(host_kind) || session.policy.show_indicator;
    tracing::info!(?host_kind, show_indicator, "session policy resolved");
    let _indicator = show_indicator.then(indicator::Indicator::show);
    let keyframes = KeyframeRequests::default();
    let stop = Arc::new(AtomicBool::new(false));

    // Data channels: Ping/Pong, keyframe requests, input, clipboard (if policy allows).
    let clipboard = session.policy.clipboard;
    let kf = keyframes.clone();
    let mut channels = std::mem::replace(
        &mut session.channels,
        tokio::sync::mpsc::unbounded_channel().1,
    );
    tokio::spawn(async move {
        while let Some(dc) = channels.recv().await {
            tokio::spawn(serve_channel(dc, kf.clone(), clipboard));
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
    // Host sound runs alongside the picture; losing it never ends the session.
    if let (Some(audio_track), false) = (session.audio.take(), cli.no_audio) {
        let source = if cli.test_tone {
            audio::Source::TestTone
        } else {
            audio::Source::Loopback
        };
        let audio_stop = stop.clone();
        tokio::spawn(async move {
            if let Err(e) = audio::run(source, audio_track, audio_stop).await {
                tracing::warn!(error = %format!("{e:#}"), "host audio unavailable; continuing without sound");
            }
        });
    }
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

async fn serve_channel(dc: Arc<dyn DataChannel>, keyframes: KeyframeRequests, clipboard: bool) {
    match dc.protocol().await {
        Ok(p) if p == DATA_CHANNEL_PROTOCOL => {}
        other => {
            tracing::warn!(?other, "refusing data channel with unknown protocol");
            let _ = dc.close().await;
            return;
        }
    }
    let input = input::InputSink::start();
    let mut cursor_shapes = cursor::watch();
    let (clip, mut local_clip) = if clipboard {
        let (c, rx) = ClipboardSync::start();
        (Some(c), rx)
    } else {
        (None, tokio::sync::mpsc::unbounded_channel().1)
    };
    let send = |m: DataMessage| {
        let dc = dc.clone();
        async move {
            if let Ok(b) = m.encode() {
                let _ = dc.send(BytesMut::from(&b[..])).await;
            }
        }
    };
    loop {
        tokio::select! {
            ev = dc.poll() => match ev {
                Some(DataChannelEvent::OnMessage(msg)) => match DataMessage::decode(&msg.data) {
                    Ok(DataMessage::Control(Control::Ping(n))) => send(DataMessage::Control(Control::Pong(n))).await,
                    Ok(DataMessage::Control(Control::RequestKeyframe)) => keyframes.request(),
                    Ok(DataMessage::Control(Control::Action(action))) => host_action(action),
                    Ok(DataMessage::Input(ev)) => input.send(ev),
                    Ok(DataMessage::Clipboard(text)) => match &clip {
                        Some(c) => c.apply_remote(text),
                        None => tracing::debug!("clipboard disabled by policy; ignoring"),
                    },
                    Ok(other) => tracing::debug!(?other, "data message (not handled)"),
                    Err(e) => tracing::warn!(error = %e, "bad data message"),
                },
                Some(DataChannelEvent::OnClose) | None => break,
                _ => {}
            },
            Some(text) = local_clip.recv() => send(DataMessage::Clipboard(text)).await,
            Some(shape) = cursor_shapes.recv() => send(DataMessage::Control(Control::Cursor(shape))).await,
        }
    }
}

/// This computer's name, shown in viewers' computer lists.
fn machine_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().trim_end_matches(".local").to_owned())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unnamed computer".into())
}

/// Publishes `HostStatus` for the local app; a no-op without `--status-file`.
#[derive(Clone)]
struct StatusPublisher {
    path: Option<PathBuf>,
    base: rconn_core::status::HostStatus,
    state: Arc<std::sync::Mutex<(bool, bool)>>, // (online, session_active)
}

impl StatusPublisher {
    fn new(
        path: Option<PathBuf>,
        host_id: HostId,
        server: &str,
        fingerprint: Option<String>,
    ) -> Self {
        let base = rconn_core::status::HostStatus {
            host_id: host_id.to_string(),
            short_code: host_id.short_code(),
            label: machine_name(),
            server: server.to_owned(),
            fingerprint,
            ..Default::default()
        };
        let p = Self {
            path,
            base,
            state: Arc::default(),
        };
        p.publish();
        // Heartbeat so the app can tell a dead agent from an idle one.
        let hb = p.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                hb.publish();
            }
        });
        p
    }
    fn set_online(&self, v: bool) {
        self.state.lock().expect("status").0 = v;
        self.publish();
    }
    fn set_session(&self, v: bool) {
        self.state.lock().expect("status").1 = v;
        self.publish();
    }
    fn publish(&self) {
        let Some(path) = &self.path else { return };
        let (online, session_active) = *self.state.lock().expect("status");
        let updated = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let s = rconn_core::status::HostStatus {
            missing_permissions: missing_permissions(),
            online,
            session_active,
            updated,
            ..self.base.clone()
        };
        if let Err(e) = s.write(path) {
            tracing::debug!(error = %e, "could not write status file");
        }
    }
}

/// OS permissions this host still needs to capture the screen and inject input.
fn missing_permissions() -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        use objc2_core_graphics::{CGPreflightPostEventAccess, CGPreflightScreenCaptureAccess};
        let mut missing = Vec::new();
        if !CGPreflightScreenCaptureAccess() {
            missing.push("Screen Recording".to_owned());
        }
        if !CGPreflightPostEventAccess() {
            missing.push("Accessibility".to_owned());
        }
        missing
    }
    #[cfg(not(target_os = "macos"))]
    Vec::new()
}

/// macOS: ask once for the permissions a host needs (shows the system prompts).
fn request_permissions() {
    #[cfg(target_os = "macos")]
    {
        use objc2_core_graphics::{
            CGPreflightPostEventAccess, CGPreflightScreenCaptureAccess, CGRequestPostEventAccess,
            CGRequestScreenCaptureAccess,
        };
        if !CGPreflightScreenCaptureAccess() {
            let _ = CGRequestScreenCaptureAccess();
        }
        if !CGPreflightPostEventAccess() {
            let _ = CGRequestPostEventAccess();
        }
    }
}

/// A toolbar action from the viewer.
fn host_action(action: rconn_core::protocol::HostAction) {
    use rconn_core::protocol::HostAction;
    tracing::info!(?action, "viewer requested an action");
    let result = match action {
        HostAction::LockScreen => lock_screen(),
        HostAction::Restart => restart_computer(),
    };
    if let Err(e) = result {
        tracing::warn!(?action, error = %e, "action failed");
    }
}

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 FFI: LockWorkStation, desktop attachment
fn lock_screen() -> Result<()> {
    use windows::Win32::System::Shutdown::LockWorkStation;
    use windows::Win32::System::StationsAndDesktops::{
        DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, OpenInputDesktop, SetThreadDesktop,
    };
    // LockWorkStation must come from a thread on the interactive desktop.
    std::thread::spawn(|| unsafe {
        let _ = OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(0x1000_0000),
        )
        .and_then(|d| SetThreadDesktop(d));
        LockWorkStation()
    })
    .join()
    .map_err(|_| anyhow::anyhow!("lock thread panicked"))??;
    Ok(())
}

#[cfg(target_os = "macos")]
fn lock_screen() -> Result<()> {
    // Same as Ctrl+Cmd+Q: the login window's "suspend" puts the Mac on the lock screen.
    let status = std::process::Command::new(
        "/System/Library/CoreServices/Menu Extras/User.menu/Contents/Resources/CGSession",
    )
    .arg("-suspend")
    .status()?;
    anyhow::ensure!(status.success(), "CGSession -suspend failed");
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn lock_screen() -> Result<()> {
    anyhow::bail!("locking isn't supported on this OS")
}

fn restart_computer() -> Result<()> {
    #[cfg(windows)]
    let status = std::process::Command::new("shutdown")
        .args(["/r", "/t", "5", "/c", "Restart requested from rconn"])
        .status()?;
    #[cfg(not(windows))]
    let status = std::process::Command::new("osascript")
        .args(["-e", "tell application \"System Events\" to restart"])
        .status()?;
    anyhow::ensure!(status.success(), "restart command failed");
    Ok(())
}

/// Run above ordinary apps so a busy machine (video playing, builds) can't starve
/// capture and encoding, which is what makes a remote session stutter.
fn raise_priority() {
    #[cfg(windows)]
    #[allow(unsafe_code)] // Win32 FFI: SetPriorityClass on our own process
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentProcess, HIGH_PRIORITY_CLASS, SetPriorityClass,
        };
        if let Err(e) = SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS) {
            tracing::warn!(error = %e, "could not raise process priority");
        }
    }
}
