//! Viewer window: shows the host's screen and sends mouse and keyboard input.
//!
//! The session runs on a background Tokio runtime. Decoded frames go to the UI through a
//! latest-wins slot; input goes back through a channel. The UI never blocks on the network.

use crate::keymap::to_hid;
use anyhow::{Result, anyhow};
use bytes::BytesMut;
use egui::{Color32, ColorImage, Pos2, Rect, Sense, TextureHandle, TextureOptions, Vec2};
use rconn_core::ids::HostId;
use rconn_core::protocol::{Control, DataMessage, InputEvent, MouseButton};
use rconn_media::video::{DecodedFrame, VideoDecoder};
use rconn_peer::rtc::{PeerEvent, PeerOptions};
use rconn_peer::session::open_viewer_session;
use rconn_peer::signaling::SignalingClient;
use rconn_peer::trust::ServerTrust;
use rconn_peer::video::VideoReceiver;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use webrtc::data_channel::DataChannelEvent;
use webrtc::media_stream::track_remote::TrackRemoteEvent;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ConnectArgs {
    pub server: String,
    /// Pinned server key (`sha256:...`), or None for a public certificate.
    pub fingerprint: Option<String>,
    pub token: String,
    pub host: HostId,
    pub openh264: PathBuf,
    pub udp_addrs: Vec<String>,
    pub relay_only: bool,
    /// Borderless, maximized window; the toolbar appears on the hotkey.
    pub chromeless: bool,
    /// Don't play the host's sound (can be switched in the toolbar).
    pub mute: bool,
    /// Shown in the tab, toolbar and title (the computer's name or ID).
    pub name: String,
}

/// The local hotkey that toggles the toolbar: Ctrl+Alt+Shift+M. Never sent to the host.
pub const HOTKEY_HINT: &str = if cfg!(target_os = "macos") {
    "Control+Option+Shift+M"
} else {
    "Ctrl+Alt+Shift+M"
};

const MAC: bool = cfg!(target_os = "macos");
const LETTER_A: u16 = rconn_core::hid::usage::A;

/// Decides which key events reach the host, and tracks what the host thinks is held so it
/// can be released when the toolbar takes over the keyboard or the window loses focus.
#[derive(Default)]
pub struct KeyGate {
    pub toolbar: bool,
    held: std::collections::BTreeSet<u16>,
}

pub enum KeyAction {
    /// Forward these to the host, in order.
    Send(Vec<InputEvent>),
    /// Toolbar toggled; release these keys on the host first.
    Toggled(Vec<InputEvent>),
    /// Swallow (toolbar is open, or unmapped key).
    Drop,
}

fn key_event(hid_usage: u16, pressed: bool) -> InputEvent {
    InputEvent::Key { hid_usage, pressed }
}

/// Is the modifier `hid` still down according to the viewer's own modifier state?
/// `None` for keys that aren't modifiers.
fn modifier_down(hid: u16, mods: egui::Modifiers) -> Option<bool> {
    use rconn_core::hid::usage as u;
    match hid {
        // On a Mac, Command is sent as Control (so Cmd+C copies on Windows).
        u::CONTROL_LEFT | u::CONTROL_RIGHT => Some(mods.ctrl || (MAC && mods.mac_cmd)),
        u::SHIFT_LEFT | u::SHIFT_RIGHT => Some(mods.shift),
        u::ALT_LEFT | u::ALT_RIGHT => Some(mods.alt),
        _ => None,
    }
}

impl KeyGate {
    pub fn key(
        &mut self,
        key: egui::Key,
        pressed: bool,
        repeat: bool,
        mods: egui::Modifiers,
    ) -> KeyAction {
        use rconn_core::hid::usage as u;
        if key == egui::Key::M && pressed && mods.ctrl && mods.alt && mods.shift {
            self.toolbar = !self.toolbar;
            return KeyAction::Toggled(self.release_all());
        }
        if self.toolbar || repeat {
            return KeyAction::Drop;
        }
        let Some(mut hid_usage) = to_hid(key) else {
            return KeyAction::Drop;
        };
        if MAC {
            hid_usage = match hid_usage {
                u::META_LEFT => u::CONTROL_LEFT,
                u::META_RIGHT => u::CONTROL_RIGHT,
                other => other,
            };
        }
        // A modifier whose release we never saw (macOS drops some key-ups) is released now,
        // so the host never keeps typing in capitals or with Ctrl stuck.
        let mut out: Vec<InputEvent> = Vec::new();
        let stale: Vec<u16> = self
            .held
            .iter()
            .copied()
            .filter(|&h| h != hid_usage && modifier_down(h, mods) == Some(false))
            .collect();
        for h in stale {
            self.held.remove(&h);
            out.push(key_event(h, false));
        }
        if MAC && hid_usage == u::CAPS_LOCK {
            // macOS reports Caps Lock "down" when it turns on and "up" when it turns off;
            // Windows toggles on each press, so send a full press for every change.
            out.extend([key_event(hid_usage, true), key_event(hid_usage, false)]);
            return KeyAction::Send(out);
        }
        if pressed {
            self.held.insert(hid_usage);
        } else {
            self.held.remove(&hid_usage);
        }
        out.push(key_event(hid_usage, pressed));
        KeyAction::Send(out)
    }

    /// Release everything the host thinks is held (focus lost, toolbar opened).
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        std::mem::take(&mut self.held)
            .into_iter()
            .map(|h| key_event(h, false))
            .collect()
    }

    /// A full key press for a shortcut the OS turned into an edit command (Cmd+C/X/V).
    pub fn tap(&self, hid_usage: u16) -> Vec<InputEvent> {
        if self.toolbar {
            return Vec::new();
        }
        vec![key_event(hid_usage, true), key_event(hid_usage, false)]
    }
}

#[derive(Default)]
struct Shared {
    frame: Option<DecodedFrame>,
    status: String,
    fps: f32,
    kbps: f32,
}

/// Local port of the open session window: later connections become tabs in it.
pub const VIEWER_PORT: u16 = 47_393;

/// If a session window is already open, hand `args` to it as a new tab.
/// Returns false when there is none (the caller then opens the window).
pub fn hand_off(args: &ConnectArgs) -> bool {
    use std::io::Write;
    let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", VIEWER_PORT)) else {
        return false;
    };
    let Ok(mut line) = serde_json::to_string(args) else {
        return false;
    };
    line.push('\n');
    s.write_all(line.as_bytes()).is_ok()
}

/// Start a session in the background; the returned tab shows it.
fn open_tab(args: ConnectArgs, ctx_slot: Arc<Mutex<Option<egui::Context>>>) -> ViewerApp {
    let chromeless = args.chromeless;
    let host = args.host;
    let name = args.name.clone();
    let muted = Arc::new(std::sync::atomic::AtomicBool::new(args.mute));
    let muted_audio = muted.clone();
    let shared_cursor: Arc<Mutex<Option<rconn_core::protocol::CursorShape>>> = Arc::default();
    let cursor_for_session = shared_cursor.clone();
    let shared = Arc::new(Mutex::new(Shared {
        status: "connecting…".into(),
        ..Default::default()
    }));
    let (input_tx, input_rx) = mpsc::unbounded_channel::<InputEvent>();
    let (kf_ui, kf_from_ui) = mpsc::unbounded_channel::<Control>();
    // Dropping the tab drops `close`, which ends the session and its runtime.
    let (close, mut closed) = tokio::sync::watch::channel(());
    let (sh, ctx2) = (shared.clone(), ctx_slot);
    let spawned = std::thread::Builder::new().name("rconn-session".into()).spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let result = rt.block_on(async {
            tokio::select! {
                r = session(args, muted_audio, cursor_for_session, sh.clone(), input_rx, kf_from_ui, ctx2.clone()) => Some(r),
                _ = closed.changed() => None, // tab closed
            }
        });
        let Some(result) = result else { return };
        sh.lock().expect("ui state").status = match result {
            Ok(()) => "session ended".into(),
            Err(e) => format!("error: {e:#}"),
        };
        if let Some(ctx) = ctx2.lock().expect("ctx").as_ref() {
            ctx.request_repaint();
        }
    });
    if let Err(e) = spawned {
        shared.lock().expect("ui state").status = format!("error: {e}");
    }
    ViewerApp {
        shared,
        input: input_tx,
        keyframe: kf_ui,
        texture: None,
        last_pointer: None,
        keys: KeyGate::default(),
        fullscreen: false,
        chromeless,
        name,
        muted,
        chrome: None,
        reveal_until: None,
        fit_window: false,
        cursor: shared_cursor,
        last_frame: None,
        confirm_restart: false,
        toast: None,
        closing: false,
        host,
        _close: Some(close),
        seen_window: None,
        resized_at: None,
        snapped_to: None,
    }
}

/// Open the session window with a first tab; it blocks until the window closes.
/// Later `rconn-viewer connect` runs add tabs to it (see [`hand_off`]).
pub fn run(args: ConnectArgs) -> Result<()> {
    let chromeless = args.chromeless;
    let title = format!("rconn — {}", args.name);
    let ctx_slot: Arc<Mutex<Option<egui::Context>>> = Arc::default();
    let (tab_tx, tab_rx) = std::sync::mpsc::channel::<ConnectArgs>();
    if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", VIEWER_PORT)) {
        let ctx = ctx_slot.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for conn in listener.incoming().flatten() {
                let mut line = String::new();
                if std::io::BufReader::new(conn).read_line(&mut line).is_ok()
                    && let Ok(args) = serde_json::from_str::<ConnectArgs>(&line)
                {
                    let _ = tab_tx.send(args);
                    if let Some(c) = ctx.lock().expect("ctx").as_ref() {
                        c.send_viewport_cmd(egui::ViewportCommand::Focus);
                        c.request_repaint();
                    }
                }
            }
        });
    }
    let options = eframe::NativeOptions {
        viewport: {
            let v = egui::ViewportBuilder::default()
                .with_inner_size([1280.0, 800.0])
                .with_title(title);
            if chromeless {
                v.with_decorations(false).with_maximized(true)
            } else {
                v
            }
        },
        ..Default::default()
    };
    eframe::run_native(
        "rconn",
        options,
        Box::new(move |cc| {
            *ctx_slot.lock().expect("ctx") = Some(cc.egui_ctx.clone());
            let first = open_tab(args, ctx_slot.clone());
            Ok(Box::new(Window {
                tabs: vec![first],
                active: 0,
                incoming: tab_rx,
                ctx_slot,
            }))
        }),
    )
    .map_err(|e| anyhow!("window: {e}"))
}

/// The session window: one tab per remote computer.
struct Window {
    tabs: Vec<ViewerApp>,
    active: usize,
    incoming: std::sync::mpsc::Receiver<ConnectArgs>,
    ctx_slot: Arc<Mutex<Option<egui::Context>>>,
}

const TABS_H: f32 = 32.0;

impl Window {
    fn switch_to(&mut self, i: usize) {
        if i == self.active || i >= self.tabs.len() {
            return;
        }
        // Keys held on the tab we leave would stay down on that computer.
        let old = &mut self.tabs[self.active];
        for ev in old.keys.release_all() {
            old.send(ev);
        }
        self.active = i;
        self.tabs[i].fit_window = true;
    }

    fn tab_strip(&mut self, ui: &mut egui::Ui) {
        let mut switch = None;
        egui::Panel::top("tabs")
            .exact_size(TABS_H)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(ui.visuals().extreme_bg_color)
                    .inner_margin(egui::Margin::symmetric(8, 3)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    for (i, tab) in self.tabs.iter_mut().enumerate() {
                        let active = i == self.active;
                        let frame = egui::Frame::new()
                            .fill(if active {
                                ui.visuals().panel_fill
                            } else {
                                Color32::TRANSPARENT
                            })
                            .corner_radius(6.0)
                            .inner_margin(egui::Margin::symmetric(10, 3));
                        frame.show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let color = if active {
                                    Color32::from_rgb(80, 150, 255)
                                } else {
                                    Color32::GRAY
                                };
                                ui.label(egui::RichText::new("🖥").color(color));
                                let text = egui::RichText::new(&tab.name);
                                let text = if active { text.strong() } else { text };
                                if ui
                                    .add(egui::Label::new(text).sense(Sense::click()))
                                    .clicked()
                                {
                                    switch = Some(i);
                                }
                                if ui
                                    .small_button("❌")
                                    .on_hover_text("Disconnect and close this tab")
                                    .clicked()
                                {
                                    tab.closing = true;
                                }
                            });
                        });
                    }
                });
            });
        if let Some(i) = switch {
            self.switch_to(i);
        }
    }
}

impl eframe::App for Window {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        while let Ok(args) = self.incoming.try_recv() {
            // Already open: show that tab rather than a second session to the same computer.
            match self
                .tabs
                .iter()
                .position(|t| t.host == args.host && !t.closing)
            {
                Some(i) => self.switch_to(i),
                None => {
                    self.tabs.push(open_tab(args, self.ctx_slot.clone()));
                    let last = self.tabs.len() - 1;
                    self.switch_to(last);
                }
            }
        }
        // Remove closed tabs (dropping a tab ends its session); keep the same tab active.
        let active = self.active;
        let removed_before = self.tabs.iter().take(active).filter(|t| t.closing).count();
        self.tabs.retain(|t| !t.closing);
        if self.tabs.is_empty() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        self.active = active
            .saturating_sub(removed_before)
            .min(self.tabs.len() - 1);
        let fullscreen = self.tabs[self.active].is_fullscreen(ui);
        if !fullscreen {
            self.tab_strip(ui);
        }
        let tab = &mut self.tabs[self.active];
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Title(format!(
                "rconn — {}",
                tab.name
            )));
        tab.draw(ui);
    }
}

async fn session(
    args: ConnectArgs,
    muted: Arc<std::sync::atomic::AtomicBool>,
    shared_cursor: Arc<Mutex<Option<rconn_core::protocol::CursorShape>>>,
    shared: Arc<Mutex<Shared>>,
    mut input: mpsc::UnboundedReceiver<InputEvent>,
    mut kf_from_ui: mpsc::UnboundedReceiver<Control>,
    ctx: Arc<Mutex<Option<egui::Context>>>,
) -> Result<()> {
    let repaint = || {
        if let Some(c) = ctx.lock().expect("ctx").as_ref() {
            c.request_repaint();
        }
    };
    let trust = ServerTrust::from_fingerprint(args.fingerprint.as_deref());
    let sig = SignalingClient::connect_operator(&args.server, &trust, &args.token).await?;
    let opts = PeerOptions {
        udp_addrs: args.udp_addrs.clone(),
        relay_only: args.relay_only,
    };
    let mut session = open_viewer_session(sig, args.host, opts).await?;
    shared.lock().expect("ui state").status = "connected, waiting for the picture…".into();
    repaint();

    // Data channel: keyframe request on open, then forward input as it comes.
    let dc = session.data.clone();
    let cursor = shared_cursor.clone();
    let repaint_cursor = {
        let ctx = ctx.clone();
        move || {
            if let Some(c) = ctx.lock().expect("ctx").as_ref() {
                c.request_repaint();
            }
        }
    };
    let (kf_tx, mut kf_rx) = mpsc::unbounded_channel::<()>();
    let dc_task = tokio::spawn(async move {
        let send = |dc: Arc<dyn webrtc::data_channel::DataChannel>, m: DataMessage| async move {
            if let Ok(b) = m.encode() {
                let _ = dc.send(BytesMut::from(&b[..])).await;
            }
        };
        let mut open = false;
        let (clip, mut local_clip) = rconn_peer::clipboard::ClipboardSync::start();
        loop {
            tokio::select! {
                Some(text) = local_clip.recv() => if open {
                    send(dc.clone(), DataMessage::Clipboard(text)).await;
                },
                ev = dc.poll() => match ev {
                    Some(DataChannelEvent::OnOpen) => {
                        open = true;
                        send(dc.clone(), DataMessage::Control(Control::RequestKeyframe)).await;
                    }
                    Some(DataChannelEvent::OnMessage(msg)) => match DataMessage::decode(&msg.data) {
                        Ok(DataMessage::Clipboard(text)) => clip.apply_remote(text),
                        Ok(DataMessage::Control(Control::Cursor(shape))) => {
                            cursor.lock().expect("cursor").replace(shape);
                            repaint_cursor();
                        }
                        _ => {}
                    },
                    Some(DataChannelEvent::OnClose) | None => break,
                    _ => {}
                },
                ev = input.recv() => match ev {
                    Some(ev) if open => send(dc.clone(), DataMessage::Input(ev)).await,
                    Some(_) => {}
                    None => break,
                },
                Some(c) = kf_from_ui.recv() => if open {
                    send(dc.clone(), DataMessage::Control(c)).await;
                },
                req = kf_rx.recv() => if req.is_some() && open {
                    send(dc.clone(), DataMessage::Control(Control::RequestKeyframe)).await;
                },
            }
        }
    });

    // Route tracks: video to the decoder below, audio to the speakers (unless muted).
    let (video_tx, mut video_rx) = mpsc::unbounded_channel();
    let mut events = std::mem::replace(&mut session.peer.events, mpsc::unbounded_channel().1);
    let router = tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            let PeerEvent::Track(t) = ev else { continue };
            if t.kind().await == rtc::rtp_transceiver::rtp_sender::RtpCodecKind::Audio {
                let muted = muted.clone();
                tokio::spawn(async move {
                    let player = match crate::playback::Player::start() {
                        Ok(p) => p,
                        Err(e) => return tracing::warn!(error = %e, "no audio playback"),
                    };
                    let Ok(mut dec) = rconn_media::codec::opus_decoder() else {
                        return;
                    };
                    let mut pcm = vec![0f32; 960 * 2 * 6];
                    while let Some(ev) = t.poll().await {
                        let TrackRemoteEvent::OnRtpPacket(pkt) = ev else {
                            continue;
                        };
                        if muted.load(std::sync::atomic::Ordering::Relaxed) {
                            continue;
                        }
                        if let Ok(n) = dec.decode_float(&pkt.payload, &mut pcm, false) {
                            player.push(&pcm[..n * 2]);
                        }
                    }
                });
            } else {
                let _ = video_tx.send(t);
            }
        }
    });
    let track = tokio::time::timeout(Duration::from_secs(20), video_rx.recv())
        .await
        .map_err(|_| anyhow!("no video from the host within 20 s"))?
        .ok_or_else(|| anyhow!("session closed before video arrived"))?;

    let mut rx = VideoReceiver::default();
    let mut dec = VideoDecoder::new(&args.openh264)?;
    let (mut frames, mut bytes, mut since) = (0u32, 0usize, Instant::now());
    loop {
        let Some(ev) = track.poll().await else { break };
        let TrackRemoteEvent::OnRtpPacket(pkt) = ev else {
            continue;
        };
        for au in rx.push(pkt) {
            bytes += au.len();
            match dec.decode(&au) {
                Ok(Some(f)) => {
                    frames += 1;
                    let mut s = shared.lock().expect("ui state");
                    s.frame = Some(f);
                    s.status.clear();
                    drop(s);
                    repaint();
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "decode error; requesting keyframe");
                    let _ = kf_tx.send(());
                }
            }
        }
        if since.elapsed() >= Duration::from_secs(1) {
            let secs = since.elapsed().as_secs_f32();
            let mut s = shared.lock().expect("ui state");
            s.fps = frames as f32 / secs;
            s.kbps = bytes as f32 * 8.0 / secs / 1000.0;
            (frames, bytes, since) = (0, 0, Instant::now());
        }
    }
    dc_task.abort();
    router.abort();
    session.close().await;
    Ok(())
}

struct ViewerApp {
    shared: Arc<Mutex<Shared>>,
    input: mpsc::UnboundedSender<InputEvent>,
    /// Control messages for the host (keyframe requests, toolbar actions).
    keyframe: mpsc::UnboundedSender<Control>,
    texture: Option<TextureHandle>,
    last_pointer: Option<(f32, f32)>,
    keys: KeyGate,
    fullscreen: bool,
    /// Borderless mode: like full screen, the toolbar slides out from the top.
    chromeless: bool,
    name: String,
    muted: Arc<std::sync::atomic::AtomicBool>,
    /// Where the floating toolbar (or its tab) is; pointer events there stay local.
    chrome: Option<Rect>,
    /// The floating toolbar stays out until then.
    reveal_until: Option<Instant>,
    /// Resize the window to the remote screen's shape on the next frame.
    fit_window: bool,
    /// The host's pointer shape, shown with the local pointer over the picture.
    cursor: Arc<Mutex<Option<rconn_core::protocol::CursorShape>>>,
    /// The latest picture (for "Save screenshot").
    last_frame: Option<DecodedFrame>,
    /// "Restart remote computer" is waiting for confirmation.
    confirm_restart: bool,
    /// Short feedback shown in the toolbar, until the deadline.
    toast: Option<(String, Instant)>,
    /// Disconnect was pressed; the window removes this tab.
    closing: bool,
    /// The computer this tab shows.
    host: HostId,
    /// Dropped with the tab, which ends the session.
    _close: Option<tokio::sync::watch::Sender<()>>,
    /// Window size last seen, when the user last resized it, and our pending snap size.
    seen_window: Option<Vec2>,
    resized_at: Option<Instant>,
    snapped_to: Option<Vec2>,
}

/// The local pointer icon for a host pointer shape.
fn cursor_icon(shape: rconn_core::protocol::CursorShape) -> egui::CursorIcon {
    use egui::CursorIcon as I;
    use rconn_core::protocol::CursorShape as C;
    match shape {
        C::Arrow => I::Default,
        C::Text => I::Text,
        C::Hand => I::PointingHand,
        C::ResizeHorizontal => I::ResizeHorizontal,
        C::ResizeVertical => I::ResizeVertical,
        C::ResizeNwSe => I::ResizeNwSe,
        C::ResizeNeSw => I::ResizeNeSw,
        C::Move => I::Move,
        C::Wait => I::Wait,
        C::Progress => I::Progress,
        C::NotAllowed => I::NotAllowed,
        C::Crosshair => I::Crosshair,
        C::Hidden => I::None,
    }
}

impl ViewerApp {
    fn send(&self, ev: InputEvent) {
        let _ = self.input.send(ev);
    }

    /// Translate this frame's egui input into remote input, relative to `image` on screen.
    fn forward_input(&mut self, ui: &egui::Ui, image: Rect) {
        let events = ui.ctx().input(|i| i.events.clone());
        let chrome = self.chrome;
        let on_chrome = |p: Pos2| chrome.is_some_and(|r| r.contains(p));
        let hover_on_chrome = ui.ctx().pointer_hover_pos().is_some_and(on_chrome);
        for ev in events {
            match ev {
                // Over the toolbar, the mouse belongs to the viewer.
                egui::Event::PointerMoved(p) if on_chrome(p) => {}
                egui::Event::PointerButton { pos, .. } if on_chrome(pos) => {}
                egui::Event::MouseWheel { .. } if hover_on_chrome => {}
                // While the toolbar is open, the mouse belongs to the viewer.
                egui::Event::PointerMoved(_)
                | egui::Event::PointerButton { .. }
                | egui::Event::MouseWheel { .. }
                    if self.keys.toolbar => {}
                egui::Event::PointerMoved(pos) => self.pointer_to(pos, image),
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } if image.contains(pos) => {
                    self.pointer_to(pos, image);
                    if let Some(b) = map_button(button) {
                        self.send(InputEvent::PointerButton { button: b, pressed });
                    }
                }
                egui::Event::MouseWheel { unit, delta, .. } => {
                    let lines = match unit {
                        egui::MouseWheelUnit::Line => delta,
                        egui::MouseWheelUnit::Page => delta * 10.0,
                        egui::MouseWheelUnit::Point => delta / 40.0,
                    };
                    self.send(InputEvent::Wheel {
                        dx: lines.x,
                        dy: lines.y,
                    });
                }
                egui::Event::WindowFocused(false) => {
                    self.keys
                        .release_all()
                        .into_iter()
                        .for_each(|ev| self.send(ev));
                }
                // Cmd/Ctrl+C, X, V arrive as edit commands, not keys: send the key itself
                // (Ctrl is already held on the host).
                egui::Event::Copy => self
                    .keys
                    .tap(LETTER_A + 2)
                    .into_iter()
                    .for_each(|ev| self.send(ev)),
                egui::Event::Cut => self
                    .keys
                    .tap(LETTER_A + 23)
                    .into_iter()
                    .for_each(|ev| self.send(ev)),
                egui::Event::Paste(_) => self
                    .keys
                    .tap(LETTER_A + 21)
                    .into_iter()
                    .for_each(|ev| self.send(ev)),
                egui::Event::Key {
                    key,
                    physical_key,
                    pressed,
                    repeat,
                    modifiers,
                    ..
                } => {
                    match self
                        .keys
                        .key(physical_key.unwrap_or(key), pressed, repeat, modifiers)
                    {
                        KeyAction::Send(evs) => evs.into_iter().for_each(|ev| self.send(ev)),
                        KeyAction::Toggled(releases) => {
                            releases.into_iter().for_each(|ev| self.send(ev))
                        }
                        KeyAction::Drop => {}
                    }
                }
                _ => {}
            }
        }
    }

    fn pointer_to(&mut self, pos: Pos2, image: Rect) {
        if !image.contains(pos) {
            return;
        }
        let x = ((pos.x - image.min.x) / image.width()).clamp(0.0, 1.0);
        let y = ((pos.y - image.min.y) / image.height()).clamp(0.0, 1.0);
        if self.last_pointer != Some((x, y)) {
            self.last_pointer = Some((x, y));
            self.send(InputEvent::PointerMove { x, y });
        }
    }
}

fn map_button(b: egui::PointerButton) -> Option<MouseButton> {
    Some(match b {
        egui::PointerButton::Primary => MouseButton::Left,
        egui::PointerButton::Secondary => MouseButton::Right,
        egui::PointerButton::Middle => MouseButton::Middle,
        egui::PointerButton::Extra1 => MouseButton::Back,
        egui::PointerButton::Extra2 => MouseButton::Forward,
    })
}

/// Window size showing a `remote`-sized screen with no black bars: 1:1 when it fits,
/// else scaled down to 90% of the display, plus the toolbar.
pub fn window_size_for(remote: Vec2, monitor: Option<Vec2>) -> Vec2 {
    let chrome = BAR_H + TABS_H;
    let max = monitor.unwrap_or(Vec2::new(1600.0, 1000.0)) * 0.9 - Vec2::new(0.0, chrome);
    let scale = (max.x / remote.x).min(max.y / remote.y).min(1.0);
    Vec2::new(
        (remote.x * scale).round(),
        (remote.y * scale).round() + chrome,
    )
}

/// After the user resizes the window, the window size that removes the black bars:
/// same width with the picture's height, or (if that won't fit on screen) the same
/// height with the picture's width. `None` when it already fits (within 2 points).
pub fn snap_to_picture(
    window: Vec2,
    picture_area: Vec2,
    remote: Vec2,
    monitor: Option<Vec2>,
) -> Option<Vec2> {
    let aspect = remote.x / remote.y;
    let chrome_h = window.y - picture_area.y;
    let want_h = (picture_area.x / aspect).round();
    let max_h = monitor.map_or(f32::INFINITY, |m| m.y * 0.95);
    let size = if want_h + chrome_h <= max_h {
        Vec2::new(window.x, want_h + chrome_h)
    } else {
        Vec2::new(
            (picture_area.y * aspect).round() + (window.x - picture_area.x),
            window.y,
        )
    };
    ((size - window).length() > 2.0).then_some(size)
}

/// Largest rect with `aspect` (w/h) centred in `area`.
pub fn fit(area: Rect, size: Vec2) -> Rect {
    let scale = (area.width() / size.x).min(area.height() / size.y);
    Rect::from_center_size(area.center(), size * scale)
}

const BAR_H: f32 = 40.0;
const DANGER: Color32 = Color32::from_rgb(210, 70, 60);

/// Key combinations offered in the toolbar's Keys menu (pressed in order, released in reverse).
const COMBOS: [(&str, &[u16]); 3] = {
    use rconn_core::hid::usage as u;
    [
        ("Windows key", &[u::META_LEFT]),
        ("Alt+Tab", &[u::ALT_LEFT, u::TAB]),
        (
            "Task Manager (Ctrl+Shift+Esc)",
            &[u::CONTROL_LEFT, u::SHIFT_LEFT, u::ESCAPE],
        ),
    ]
};

impl ViewerApp {
    fn is_fullscreen(&self, ui: &egui::Ui) -> bool {
        self.chromeless
            || self.fullscreen
            || ui.ctx().input(|i| i.viewport().fullscreen.unwrap_or(false))
    }

    fn notify(&mut self, text: String) {
        self.toast = Some((text, Instant::now() + Duration::from_secs(4)));
    }

    /// Save the current picture as a PNG on the Desktop.
    fn save_screenshot(&mut self) {
        let Some(f) = self.last_frame.as_ref() else {
            return self.notify("No picture yet".into());
        };
        let dir = std::env::var_os("HOME")
            .map(|h| std::path::PathBuf::from(h).join("Desktop"))
            .filter(|d| d.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let safe: String = self
            .name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let path = dir.join(format!("rconn-{safe}-{stamp}.png"));
        let msg = match crate::watch::save_png(&path, f) {
            Ok(()) => format!(
                "Saved {}",
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ),
            Err(e) => format!("Screenshot failed: {e}"),
        };
        self.notify(msg);
    }

    /// "Restart remote computer?" dialog.
    fn restart_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_restart {
            return;
        }
        let modal = egui::Modal::new(egui::Id::unique("confirm-restart")).show(ctx, |ui| {
            ui.set_width(320.0);
            ui.heading(format!("Restart {}?", self.name));
            ui.label("Anyone using it loses unsaved work. This session ends; reconnect after it starts up again.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let restart = egui::Button::new(egui::RichText::new("Restart").color(Color32::WHITE)).fill(DANGER);
                if ui.add(restart).clicked() {
                    let _ = self.keyframe.send(Control::Action(rconn_core::protocol::HostAction::Restart));
                    self.confirm_restart = false;
                    self.toast = Some(("Restarting in 5 s".into(), Instant::now() + Duration::from_secs(6)));
                }
                if ui.button("Cancel").clicked() {
                    self.confirm_restart = false;
                }
            });
        });
        if modal.should_close() {
            self.confirm_restart = false;
        }
    }

    fn send_combo(&self, keys: &[u16]) {
        for &k in keys {
            self.send(key_event(k, true));
        }
        for &k in keys.iter().rev() {
            self.send(key_event(k, false));
        }
    }

    fn stats(&self, fps: f32, kbps: f32) -> String {
        match &self.texture {
            Some(t) => format!(
                "{}x{}  ·  {fps:.0} fps  ·  {kbps:.0} kbps",
                t.size()[0],
                t.size()[1]
            ),
            None => "connecting…".into(),
        }
    }

    /// The toolbar itself (docked in a window, floating in full screen).
    fn bar_contents(&mut self, ui: &mut egui::Ui, stats: &str, fullscreen: bool) {
        ui.spacing_mut().button_padding = Vec2::new(8.0, 4.0);
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(egui::RichText::new("🖥").size(16.0));
        ui.label(egui::RichText::new(&self.name).strong());
        ui.separator();
        ui.menu_button("⌨ Keys", |ui| {
            for (label, keys) in COMBOS {
                if ui.button(label).clicked() {
                    self.send_combo(keys);
                    ui.close();
                }
            }
        })
        .response
        .on_hover_text("Send keys your Mac would keep for itself");
        ui.menu_button("⚡ Actions", |ui| {
            if ui.button("📋 Type clipboard text").on_hover_text("Types your clipboard as keystrokes (works where paste doesn't, e.g. the login screen)").clicked() {
                match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                    Ok(text) if !text.is_empty() => {
                        let n = text.chars().count();
                        self.send(InputEvent::Text(text));
                        self.notify(format!("Typed {n} characters"));
                    }
                    _ => self.notify("Your clipboard has no text".into()),
                }
                ui.close();
            }
            if ui.button("📷 Save screenshot").clicked() {
                self.save_screenshot();
                ui.close();
            }
            ui.separator();
            if ui.button("🔒 Lock remote computer").clicked() {
                let _ = self.keyframe.send(Control::Action(rconn_core::protocol::HostAction::LockScreen));
                self.notify("Locking the remote computer".into());
                ui.close();
            }
            if ui.button("🔁 Restart remote computer…").clicked() {
                self.confirm_restart = true;
                ui.close();
            }
        });
        if ui
            .button("🔄 Refresh")
            .on_hover_text("Ask the host for a fresh picture")
            .clicked()
        {
            let _ = self.keyframe.send(Control::RequestKeyframe);
        }
        let muted = self.muted.load(std::sync::atomic::Ordering::Relaxed);
        if ui
            .button(if muted {
                "🔇 Sound off"
            } else {
                "🔊 Sound on"
            })
            .clicked()
        {
            self.muted
                .store(!muted, std::sync::atomic::Ordering::Relaxed);
        }
        if ui
            .button(if fullscreen {
                "🔳 Exit full screen"
            } else {
                "🔳 Full screen"
            })
            .clicked()
        {
            self.fullscreen = !fullscreen;
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
        }
        ui.separator();
        ui.label(egui::RichText::new(stats).color(Color32::GRAY).monospace());
        ui.separator();
        let bye = egui::Button::new(egui::RichText::new("❌ Disconnect").color(Color32::WHITE))
            .fill(DANGER);
        if ui.add(bye).clicked() {
            self.closing = true;
        }
        if let Some((text, until)) = &self.toast
            && Instant::now() < *until
        {
            ui.label(egui::RichText::new(text).color(Color32::from_rgb(120, 200, 140)));
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
        if self.keys.toolbar {
            ui.label(egui::RichText::new("Keyboard paused").color(Color32::GRAY))
                .on_hover_text(format!(
                    "{HOTKEY_HINT} gives the keyboard back to the remote computer"
                ));
        }
    }

    /// Full screen: a small tab at the top; the toolbar slides out when the mouse reaches
    /// the top edge (or on the hotkey) and goes away a moment after it leaves.
    fn floating_bar(&mut self, ui: &mut egui::Ui, area: Rect, stats: &str) {
        let now = Instant::now();
        let pointer = ui.ctx().pointer_hover_pos();
        let near_top = pointer.is_some_and(|p| p.y <= area.min.y + 6.0);
        let over = pointer
            .zip(self.chrome)
            .is_some_and(|(p, r)| r.expand(16.0).contains(p));
        if near_top || over || self.keys.toolbar {
            self.reveal_until = Some(now + Duration::from_millis(1200));
        }
        if self.reveal_until.is_some_and(|t| now < t) {
            let ctx = ui.ctx().clone();
            let shown = egui::Area::new(egui::Id::unique("floating-toolbar"))
                .anchor(egui::Align2::CENTER_TOP, [0.0, 0.0])
                .order(egui::Order::Foreground)
                .show(&ctx, |ui| {
                    egui::Frame::popup(ui.style())
                        .inner_margin(8.0)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| self.bar_contents(ui, stats, true));
                        });
                });
            self.chrome = Some(shown.response.rect);
            ctx.request_repaint_after(Duration::from_millis(250)); // to slide away on time
        } else {
            let tab = Rect::from_center_size(
                Pos2::new(area.center().x, area.min.y + 11.0),
                Vec2::new(96.0, 22.0),
            );
            let p = ui.painter();
            p.rect_filled(
                tab,
                egui::CornerRadius {
                    nw: 0,
                    ne: 0,
                    sw: 8,
                    se: 8,
                },
                Color32::from_black_alpha(170),
            );
            p.text(
                tab.center(),
                egui::Align2::CENTER_CENTER,
                "rconn",
                egui::FontId::proportional(13.0),
                Color32::from_gray(220),
            );
            self.chrome = Some(tab);
        }
    }

    fn draw(&mut self, ui: &mut egui::Ui) {
        let (new_frame, status, fps, kbps) = {
            let mut s = self.shared.lock().expect("ui state");
            (s.frame.take(), s.status.clone(), s.fps, s.kbps)
        };
        if let Some(f) = new_frame {
            let img = ColorImage::from_rgba_unmultiplied([f.width, f.height], &f.rgba);
            match &mut self.texture {
                Some(t) if t.size() == [f.width, f.height] => t.set(img, TextureOptions::LINEAR),
                _ => {
                    // Fit the window to the first picture only: later size changes are the
                    // host trading resolution for speed, and keep the same shape.
                    self.fit_window |= self.texture.is_none();
                    self.texture =
                        Some(ui.ctx().load_texture("screen", img, TextureOptions::LINEAR));
                }
            }
            self.last_frame = Some(f);
        }
        let fullscreen = self.is_fullscreen(ui);
        if std::mem::take(&mut self.fit_window)
            && !fullscreen
            && let Some(t) = &self.texture
        {
            let [w, h] = t.size();
            let monitor = ui.ctx().input(|i| i.viewport().monitor_size);
            let size = window_size_for(Vec2::new(w as f32, h as f32), monitor);
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        }
        let stats = self.stats(fps, kbps);
        let ctx = ui.ctx().clone();
        self.restart_dialog(&ctx);
        if !fullscreen {
            self.chrome = None;
            egui::Panel::top("toolbar")
                .exact_size(BAR_H)
                .resizable(false)
                .frame(
                    egui::Frame::new()
                        .fill(ui.visuals().panel_fill)
                        .inner_margin(egui::Margin::symmetric(10, 6)),
                )
                .show(ui, |ui| {
                    ui.horizontal_centered(|ui| self.bar_contents(ui, &stats, false));
                });
        }
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let area = ui.max_rect();
            ui.painter().rect_filled(area, 0.0, Color32::BLACK);
            let _ = ui.allocate_rect(area, Sense::click_and_drag());
            let image = self.texture.as_ref().map(|tex| {
                let [w, h] = tex.size();
                let image = fit(area, Vec2::new(w as f32, h as f32));
                ui.painter().image(
                    tex.id(),
                    image,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
                image
            });
            if fullscreen {
                self.floating_bar(ui, area, &stats);
            } else if let Some(t) = &self.texture {
                // After the user resizes the window, snap it once to the picture's shape.
                let (window, monitor, dragging, maximized) = ui.ctx().input(|i| {
                    let v = i.viewport();
                    (
                        v.inner_rect.map(|r| r.size()),
                        v.monitor_size,
                        i.pointer.any_down(),
                        v.maximized.unwrap_or(false),
                    )
                });
                if let Some(window) = window {
                    let changed = self.seen_window.is_none_or(|w| (w - window).length() > 2.0);
                    if changed {
                        self.seen_window = Some(window);
                        // Our own snap landing: nothing more to do.
                        let ours = self
                            .snapped_to
                            .take()
                            .is_some_and(|s| (s - window).length() < 8.0);
                        self.resized_at = (!ours).then(Instant::now);
                    } else if !dragging
                        && !maximized
                        && self
                            .resized_at
                            .is_some_and(|t| t.elapsed() > Duration::from_millis(300))
                    {
                        self.resized_at = None; // at most one snap per resize
                        let [w, h] = t.size();
                        if let Some(size) = snap_to_picture(
                            window,
                            area.size(),
                            Vec2::new(w as f32, h as f32),
                            monitor,
                        ) {
                            ui.ctx()
                                .send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
                            self.snapped_to = Some(size);
                        }
                    }
                    if self.resized_at.is_some() {
                        ui.ctx().request_repaint_after(Duration::from_millis(350));
                    }
                }
            }
            if let Some(image) = image {
                self.forward_input(ui, image);
                let over_picture = ui.ctx().pointer_hover_pos().is_some_and(|p| {
                    image.contains(p) && !self.chrome.is_some_and(|r| r.contains(p))
                });
                let shape = *self.cursor.lock().expect("cursor");
                if over_picture && let Some(shape) = shape {
                    ui.ctx().set_cursor_icon(cursor_icon(shape));
                }
            }
            if !status.is_empty() {
                ui.painter().text(
                    area.center(),
                    egui::Align2::CENTER_CENTER,
                    status,
                    egui::FontId::proportional(18.0),
                    Color32::LIGHT_GRAY,
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_matches_the_remote_shape_and_fits_the_display() {
        let remote = Vec2::new(1496.0, 938.0);
        let big = window_size_for(remote, Some(Vec2::new(3000.0, 2000.0)));
        assert_eq!(
            big,
            Vec2::new(1496.0, 938.0 + BAR_H + TABS_H),
            "1:1 when it fits"
        );
        let small = window_size_for(remote, Some(Vec2::new(1440.0, 900.0)));
        let picture = Vec2::new(small.x, small.y - BAR_H - TABS_H);
        assert!(
            small.x <= 1296.0 && small.y <= 810.0,
            "within 90% of the display: {small:?}"
        );
        assert!(
            (picture.x / picture.y - remote.x / remote.y).abs() < 0.01,
            "same shape: no bars"
        );
    }

    #[test]
    fn snapping_removes_black_bars_after_a_resize() {
        let remote = Vec2::new(1496.0, 938.0);
        // Window made wider than the picture: grow/shrink the height to match.
        let s = snap_to_picture(
            Vec2::new(1400.0, 800.0),
            Vec2::new(1400.0, 728.0),
            remote,
            Some(Vec2::new(1800.0, 1100.0)),
        )
        .unwrap();
        assert_eq!(
            s,
            Vec2::new(1400.0, (1400.0_f32 / (1496.0 / 938.0)).round() + 72.0)
        );
        // Already the right shape: nothing to do.
        assert!(snap_to_picture(s, Vec2::new(1400.0, s.y - 72.0), remote, None).is_none());
        // Too tall for the screen: narrow the window instead.
        let t = snap_to_picture(
            Vec2::new(1700.0, 900.0),
            Vec2::new(1700.0, 828.0),
            remote,
            Some(Vec2::new(1800.0, 950.0)),
        )
        .unwrap();
        assert_eq!(t.y, 900.0);
        assert!((t.x - (828.0_f32 * 1496.0 / 938.0).round()).abs() < 1.0);
    }

    #[test]
    fn fit_letterboxes_and_centres() {
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 1000.0));
        let r = fit(area, Vec2::new(1600.0, 900.0));
        assert!((r.width() - 1000.0).abs() < 0.01);
        assert!((r.height() - 562.5).abs() < 0.01);
        assert!((r.center().y - 500.0).abs() < 0.01);
    }

    #[test]
    fn hotkey_toggles_toolbar_releases_held_keys_and_is_never_forwarded() {
        let mut g = KeyGate::default();
        let none = egui::Modifiers::default();
        let all = egui::Modifiers {
            ctrl: true,
            alt: true,
            shift: true,
            ..Default::default()
        };
        assert!(matches!(
            g.key(egui::Key::A, true, false, none),
            KeyAction::Send(_)
        ));
        assert!(matches!(
            g.key(egui::Key::ControlLeft, true, false, none),
            KeyAction::Send(_)
        ));
        let KeyAction::Toggled(releases) = g.key(egui::Key::M, true, false, all) else {
            panic!("hotkey not recognised")
        };
        assert_eq!(releases.len(), 2, "A and Ctrl were held on the host");
        // (Ctrl was released by the toggle; the modifiers on the hotkey press itself are
        // the viewer's, never forwarded.)
        assert!(g.toolbar);
        assert!(
            matches!(g.key(egui::Key::B, true, false, none), KeyAction::Drop),
            "toolbar owns the keyboard"
        );
        assert!(matches!(
            g.key(egui::Key::M, true, false, all),
            KeyAction::Toggled(_)
        ));
        assert!(!g.toolbar);
        assert!(
            matches!(g.key(egui::Key::B, true, true, none), KeyAction::Drop),
            "repeats are the host's job"
        );
    }

    fn keys(a: KeyAction) -> Vec<(u16, bool)> {
        match a {
            KeyAction::Send(v) | KeyAction::Toggled(v) => v
                .into_iter()
                .map(|e| match e {
                    InputEvent::Key { hid_usage, pressed } => (hid_usage, pressed),
                    other => panic!("{other:?}"),
                })
                .collect(),
            KeyAction::Drop => Vec::new(),
        }
    }

    #[test]
    fn a_shift_whose_release_was_lost_is_released_before_the_next_key() {
        use rconn_core::hid::usage as u;
        let mut g = KeyGate::default();
        let shift = egui::Modifiers {
            shift: true,
            ..Default::default()
        };
        let none = egui::Modifiers::default();
        assert_eq!(
            keys(g.key(egui::Key::ShiftLeft, true, false, shift)),
            vec![(u::SHIFT_LEFT, true)]
        );
        // The Shift key-up never arrived; the next key says Shift isn't down.
        assert_eq!(
            keys(g.key(egui::Key::H, true, false, none)),
            vec![(u::SHIFT_LEFT, false), (u::A + 7, true)]
        );
        assert_eq!(
            keys(g.key(egui::Key::H, false, false, none)),
            vec![(u::A + 7, false)]
        );
    }

    #[test]
    fn focus_loss_releases_everything_held() {
        let mut g = KeyGate::default();
        let none = egui::Modifiers::default();
        g.key(egui::Key::A, true, false, none);
        g.key(egui::Key::B, true, false, none);
        assert_eq!(g.release_all().len(), 2);
        assert!(g.release_all().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_caps_lock_changes_are_full_presses_and_cmd_is_ctrl() {
        use rconn_core::hid::usage as u;
        let mut g = KeyGate::default();
        let none = egui::Modifiers::default();
        let on = keys(g.key(egui::Key::CapsLock, true, false, none));
        let off = keys(g.key(egui::Key::CapsLock, false, false, none));
        assert_eq!(on, vec![(u::CAPS_LOCK, true), (u::CAPS_LOCK, false)]);
        assert_eq!(off, on, "turning it off is a press too");
        let cmd = egui::Modifiers {
            mac_cmd: true,
            command: true,
            ..Default::default()
        };
        assert_eq!(
            keys(g.key(egui::Key::SuperLeft, true, false, cmd)),
            vec![(u::CONTROL_LEFT, true)]
        );
        assert_eq!(
            keys(g.key(egui::Key::C, true, false, cmd)),
            vec![(u::A + 2, true)],
            "Ctrl stays held while Cmd is"
        );
    }
}

#[cfg(test)]
mod render_tests {
    //! Offscreen renders of the session window (set RCONN_VIEWER_PNG_DIR to keep PNGs).
    use super::*;
    use egui_kittest::Harness;

    fn app(fullscreen: bool, hotkey: bool) -> ViewerApp {
        let (input, _) = mpsc::unbounded_channel();
        let (keyframe, _) = mpsc::unbounded_channel();
        let (w, h) = (1496, 938);
        let mut rgba = vec![0u8; w * h * 4];
        for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (i % w, i / w);
            *px = [(x * 255 / w) as u8, 90, (y * 255 / h) as u8, 255];
        }
        let shared = Arc::new(Mutex::new(Shared {
            frame: Some(DecodedFrame {
                width: w,
                height: h,
                rgba,
            }),
            status: String::new(),
            fps: 29.0,
            kbps: 2400.0,
        }));
        let keys = KeyGate {
            toolbar: hotkey,
            ..Default::default()
        };
        ViewerApp {
            shared,
            input,
            keyframe,
            texture: None,
            last_pointer: None,
            keys,
            fullscreen,
            chromeless: false,
            name: "MATTHEWLONG4FBB".into(),
            muted: Arc::default(),
            chrome: None,
            reveal_until: None,
            fit_window: false,
            cursor: Arc::default(),
            last_frame: None,
            confirm_restart: false,
            toast: None,
            closing: false,
            host: HostId(uuid::Uuid::nil()),
            _close: None,
            seen_window: None,
            resized_at: None,
            snapped_to: None,
        }
    }

    fn render(name: &str, app: ViewerApp) -> ViewerApp {
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1100.0, 700.0))
            .build_ui_state(|ui, app: &mut ViewerApp| app.draw(ui), app);
        harness.run_steps(3);
        let image = harness.render().expect("offscreen render");
        if let Some(dir) = std::env::var_os("RCONN_VIEWER_PNG_DIR") {
            image
                .save(std::path::Path::new(&dir).join(format!("{name}.png")))
                .unwrap();
        }
        harness.into_state()
    }

    #[test]
    fn two_tabs_render_and_closing_one_keeps_the_other_active() {
        let (_tx, incoming) = std::sync::mpsc::channel();
        let mut second = app(false, false);
        second.name = "BUILD-BOX".into();
        let window = Window {
            tabs: vec![app(false, false), second],
            active: 1,
            incoming,
            ctx_slot: Arc::default(),
        };
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1100.0, 700.0))
            .build_ui_state(
                |ui, w: &mut Window| eframe::App::ui(w, ui, &mut eframe::Frame::_new_kittest()),
                window,
            );
        harness.run_steps(3);
        let image = harness.render().expect("offscreen render");
        if let Some(dir) = std::env::var_os("RCONN_VIEWER_PNG_DIR") {
            image
                .save(std::path::Path::new(&dir).join("viewer-tabs.png"))
                .unwrap();
        }
        let mut w = harness.into_state();
        w.tabs[0].closing = true;
        let (_, rx) = std::sync::mpsc::channel();
        w.incoming = rx;
        assert_eq!(w.tabs[w.active].name, "BUILD-BOX");
    }

    #[test]
    fn windowed_has_a_docked_toolbar_above_the_picture() {
        let app = render("viewer-window", app(false, false));
        assert!(app.chrome.is_none(), "docked bar, nothing floating");
    }

    #[test]
    fn full_screen_shows_a_tab_then_the_toolbar_when_revealed() {
        let tab = render("viewer-fullscreen-tab", app(true, false));
        assert!(
            tab.chrome.is_some_and(|r| r.height() < 30.0),
            "just the tab"
        );
        let bar = render("viewer-fullscreen-bar", app(true, true));
        assert!(
            bar.chrome.is_some_and(|r| r.width() > 400.0),
            "the full toolbar"
        );
    }
}
