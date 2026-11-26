//! The iOS viewer's core: one guest session (a computer's ID and its access password, no
//! account), exposed to Swift through uniffi.
//!
//! [`connect_guest`] returns a [`Session`] at once and connects in the background; progress,
//! video and the end of the session arrive on the app's [`SessionListener`]. Video is the
//! host's H.264 as AVCC samples plus SPS/PPS, for `AVSampleBufferDisplayLayer` (hardware
//! decode and display, no copies through Rust). Input goes the other way as protocol
//! messages on the data channel.

// uniffi's generated FFI scaffolding is `unsafe extern "C"`; this crate's own code has none.
#![allow(unsafe_code)]

pub mod h264;

use anyhow::{Result, anyhow};
use bytes::BytesMut;
use rconn_core::protocol::{Control, DataMessage, InputEvent, MouseButton};
use rconn_peer::guest::{Exchange, Step};
use rconn_peer::rtc::{PeerEvent, PeerOptions};
use rconn_peer::session::open_viewer_session;
use rconn_peer::signaling::SignalingClient;
use rconn_peer::trust::ServerTrust;
use rconn_peer::video::VideoReceiver;
use rtc::rtp_transceiver::rtp_sender::RtpCodecKind;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::RTCPeerConnectionState;

uniffi::setup_scaffolding!();

/// How long to wait for the first picture before giving up.
pub const FIRST_PICTURE_TIMEOUT: Duration = Duration::from_secs(20);
/// Keyframe requests are throttled: each one costs the host a full-size encode.
pub const KEYFRAME_THROTTLE: Duration = Duration::from_millis(300);

/// One H.264 picture for `AVSampleBufferDisplayLayer`.
#[derive(uniffi::Record, Debug, Clone)]
pub struct VideoFrame {
    /// NAL units with 4-byte big-endian length prefixes (a `CMBlockBuffer`'s contents).
    pub avcc: Vec<u8>,
    /// Stream headers, present on keyframes: (re)build the format description from them.
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
    pub keyframe: bool,
    /// Arrival time in microseconds since the session started.
    pub pts_us: u64,
}

/// What the app hears from a session. Called on background threads.
#[uniffi::export(with_foreign)]
pub trait SessionListener: Send + Sync {
    /// Progress for the user ("checking the access password…"); empty once the picture shows.
    fn on_status(&self, text: String);
    fn on_video(&self, frame: VideoFrame);
    /// Text the computer copied, for the app's pasteboard.
    fn on_clipboard(&self, text: String);
    /// The session is over (refused, failed, ended or disconnected). Called exactly once.
    fn on_closed(&self, reason: String);
}

#[derive(uniffi::Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
}

enum Cmd {
    Send(DataMessage),
    Keyframe,
    Close,
}

/// A running session. Dropping it disconnects.
#[derive(uniffi::Object)]
pub struct Session {
    cmds: mpsc::UnboundedSender<Cmd>,
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("rconn")
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

/// Log to stderr (Xcode's console). `RUST_LOG` filters; default `info`.
fn init_logging() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .try_init();
    });
}

/// Connect to the computer `computer_id` ("067 425 387") through the signaling server at
/// `server_url` (`wss://…`), proving `password`. `fingerprint_pin` (`sha256:<hex>`) pins a
/// self-hosted server's key; without it the server needs a publicly trusted certificate.
#[uniffi::export]
pub fn connect_guest(
    server_url: String,
    fingerprint_pin: Option<String>,
    computer_id: String,
    password: String,
    listener: Arc<dyn SessionListener>,
) -> Arc<Session> {
    init_logging();
    let (cmds, rx) = mpsc::unbounded_channel();
    let pin = fingerprint_pin.filter(|p| !p.trim().is_empty());
    let trust = ServerTrust::from_fingerprint(pin.as_deref().map(str::trim));
    runtime().spawn(async move {
        let reason = match run(
            server_url.trim(),
            &trust,
            &computer_id,
            password,
            &listener,
            rx,
        )
        .await
        {
            Ok(reason) => reason,
            Err(e) => format!("{e:#}"),
        };
        tracing::info!(%reason, "session over");
        listener.on_closed(reason);
    });
    Arc::new(Session { cmds })
}

#[uniffi::export]
impl Session {
    /// Move the remote pointer to (`x`, `y`), fractions (0..=1) of the remote screen.
    pub fn send_pointer(&self, x: f32, y: f32) {
        self.input(InputEvent::PointerMove {
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
        });
    }

    pub fn send_button(&self, button: PointerButton, pressed: bool) {
        let button = match button {
            PointerButton::Left => MouseButton::Left,
            PointerButton::Right => MouseButton::Right,
            PointerButton::Middle => MouseButton::Middle,
        };
        self.input(InputEvent::PointerButton { button, pressed });
    }

    /// Scroll by (`dx`, `dy`) lines; positive `dy` scrolls the content up.
    pub fn send_scroll(&self, dx: f32, dy: f32) {
        self.input(InputEvent::Wheel { dx, dy });
    }

    /// A physical key as a USB HID usage (page 7), which is what `UIKey.keyCode` holds.
    pub fn send_key(&self, hid_usage: u16, pressed: bool) {
        self.input(InputEvent::Key { hid_usage, pressed });
    }

    /// Type text on the computer, independent of its keyboard layout (soft keyboard).
    pub fn send_text(&self, text: String) {
        if !text.is_empty() {
            self.input(InputEvent::Text(text));
        }
    }

    /// Put text on the computer's clipboard.
    pub fn send_clipboard(&self, text: String) {
        self.send(DataMessage::Clipboard(text));
    }

    /// The picture area in pixels: the computer fits its screen resolution to it.
    pub fn send_resize(&self, width: u32, height: u32) {
        if width > 0 && height > 0 {
            self.send(DataMessage::Control(Control::Resize { width, height }));
        }
    }

    /// The app's decoder lost its state (e.g. after returning from the background).
    pub fn request_keyframe(&self) {
        let _ = self.cmds.send(Cmd::Keyframe);
    }

    pub fn disconnect(&self) {
        let _ = self.cmds.send(Cmd::Close);
    }
}

impl Session {
    fn input(&self, ev: InputEvent) {
        self.send(DataMessage::Input(ev));
    }

    fn send(&self, m: DataMessage) {
        let _ = self.cmds.send(Cmd::Send(m));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmds.send(Cmd::Close);
    }
}

async fn send_on(dc: &Arc<dyn DataChannel>, m: DataMessage) {
    match m.encode() {
        Ok(b) => {
            let _ = dc.send(BytesMut::from(&b[..])).await;
        }
        Err(e) => tracing::warn!(error = %e, "could not encode data message"),
    }
}

/// The whole session; returns why it ended.
async fn run(
    server: &str,
    trust: &ServerTrust,
    computer_id: &str,
    password: String,
    listener: &Arc<dyn SessionListener>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
) -> Result<String> {
    let started = Instant::now();
    listener.on_status("connecting to the server…".into());
    let (sig, host) = SignalingClient::connect_guest(server, trust, computer_id).await?;
    listener.on_status("connecting to the computer…".into());
    let mut session = open_viewer_session(sig, host, PeerOptions::default()).await?;
    let mut exchange = Exchange::new(password, session.session_id);
    let dc = session.data.clone();
    let mut events = std::mem::replace(&mut session.peer.events, mpsc::unbounded_channel().1);
    let (kf_tx, mut kf_rx) = mpsc::unbounded_channel::<()>();
    let got_picture = Arc::new(AtomicBool::new(false));
    let mut tasks = Vec::new();
    let (mut open, mut accepted) = (false, false);
    let mut last_kf: Option<Instant> = None;
    let first_picture = tokio::time::sleep(FIRST_PICTURE_TIMEOUT);
    tokio::pin!(first_picture);

    let outcome: Result<String> = loop {
        tokio::select! {
            ev = dc.poll() => match ev {
                Some(DataChannelEvent::OnOpen) => {
                    open = true;
                    listener.on_status("checking the access password…".into());
                }
                Some(DataChannelEvent::OnMessage(m)) => match DataMessage::decode(&m.data) {
                    Ok(DataMessage::Guest(g)) => match exchange.handle(g).await {
                        Step::Send(reply) => send_on(&dc, reply).await,
                        Step::Accepted => {
                            accepted = true;
                            if !got_picture.load(Ordering::Relaxed) {
                                listener.on_status("waiting for the picture…".into());
                            }
                            // The first keyframe may have gone out before we were listening.
                            send_on(&dc, DataMessage::Control(Control::RequestKeyframe)).await;
                            last_kf = Some(Instant::now());
                        }
                        Step::Refused(why) => break Err(anyhow!(why)),
                    },
                    Ok(DataMessage::Clipboard(text)) if accepted => listener.on_clipboard(text),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "bad data message"),
                },
                Some(DataChannelEvent::OnClose) | None => {
                    break Ok("the computer ended the session".into())
                }
                _ => {}
            },
            ev = events.recv() => match ev {
                Some(PeerEvent::Track(t)) => {
                    if t.kind().await == RtpCodecKind::Video {
                        tasks.push(tokio::spawn(video(
                            t,
                            listener.clone(),
                            kf_tx.clone(),
                            got_picture.clone(),
                            started,
                        )));
                    } else {
                        // No sound on iOS yet; keep the track drained.
                        tasks.push(tokio::spawn(async move { while t.poll().await.is_some() {} }));
                    }
                }
                Some(PeerEvent::State(s)) => {
                    tracing::info!(state = %s, "peer connection");
                    if matches!(s, RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed) {
                        break Ok("the connection to the computer was lost".into());
                    }
                }
                Some(PeerEvent::DataChannel(_)) => {}
                None => break Ok("the session ended".into()),
            },
            cmd = cmds.recv() => match cmd {
                Some(Cmd::Send(m)) => {
                    if open && accepted {
                        send_on(&dc, m).await;
                    }
                }
                Some(Cmd::Keyframe) => {
                    if open && accepted && last_kf.is_none_or(|t| t.elapsed() >= KEYFRAME_THROTTLE) {
                        send_on(&dc, DataMessage::Control(Control::RequestKeyframe)).await;
                        last_kf = Some(Instant::now());
                    }
                }
                Some(Cmd::Close) | None => break Ok("disconnected".into()),
            },
            Some(()) = kf_rx.recv() => {
                if open && accepted && last_kf.is_none_or(|t| t.elapsed() >= KEYFRAME_THROTTLE) {
                    tracing::warn!("video packets lost; requesting keyframe");
                    send_on(&dc, DataMessage::Control(Control::RequestKeyframe)).await;
                    last_kf = Some(Instant::now());
                }
            }
            () = &mut first_picture, if !got_picture.load(Ordering::Relaxed) => {
                break Err(anyhow!(if accepted {
                    "no picture from the computer within 20 s"
                } else {
                    "the computer did not answer within 20 s"
                }));
            }
        }
    };
    for t in tasks {
        t.abort();
    }
    session.close().await;
    outcome
}

/// Reassemble the video track into pictures for the app. Packet loss asks for a keyframe.
async fn video(
    track: Arc<dyn TrackRemote>,
    listener: Arc<dyn SessionListener>,
    keyframe: mpsc::UnboundedSender<()>,
    got_picture: Arc<AtomicBool>,
    started: Instant,
) {
    let mut rx = VideoReceiver::default();
    // Until the first keyframe, the app's decoder has nothing to build on.
    let mut have_key = false;
    while let Some(ev) = track.poll().await {
        let TrackRemoteEvent::OnRtpPacket(pkt) = ev else {
            continue;
        };
        let aus = rx.push(pkt);
        if rx.take_loss() {
            let _ = keyframe.send(());
        }
        for au in aus {
            let Some(a) = h264::to_avcc(&au) else {
                continue;
            };
            have_key |= a.keyframe && a.sps.is_some() && a.pps.is_some();
            if !have_key {
                let _ = keyframe.send(());
                continue;
            }
            if !got_picture.swap(true, Ordering::Relaxed) {
                listener.on_status(String::new());
            }
            listener.on_video(VideoFrame {
                avcc: a.data,
                sps: a.sps,
                pps: a.pps,
                keyframe: a.keyframe,
                pts_us: started.elapsed().as_micros() as u64,
            });
        }
    }
}
