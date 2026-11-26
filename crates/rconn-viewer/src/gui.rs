//! Viewer window: shows the host's screen and sends mouse and keyboard input.
//!
//! The session runs on a background Tokio runtime. Decoded frames go to the UI through a
//! latest-wins slot; input goes back through a channel. The UI never blocks on the network.

use crate::keymap::to_hid;
use anyhow::{Result, anyhow};
use bytes::BytesMut;
use egui::{Color32, Pos2, Rect, Sense, Vec2};
use rconn_core::ids::HostId;
use rconn_core::protocol::{Control, DataMessage, InputEvent, MouseButton};
use rconn_media::copyrect::{self, Composed, Compositor};
use rconn_media::video::{DecodedFrame, VideoDecoder};
use rconn_peer::rtc::{PeerEvent, PeerOptions};
use rconn_peer::session::open_viewer_session;
use rconn_peer::signaling::SignalingClient;
use rconn_peer::trust::ServerTrust;
use rconn_peer::video::{KeyframeNeed, VideoReceiver};
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
    /// Connecting with the computer's ID and access password instead of an account.
    #[serde(default)]
    pub guest: Option<crate::access::GuestLogin>,
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
    /// Mean decode time per picture over the last second.
    decode_ms: f32,
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
    let prefs_path = crate::prefs::default_path();
    let match_resolution = prefs_path
        .as_deref()
        .is_some_and(|p| crate::prefs::load(p, host).match_resolution);
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
    let (files_tx, files_rx) = mpsc::unbounded_channel::<crate::files::Cmd>();
    let files_state = crate::files::Shared::default();
    let files_for_session = files_state.clone();
    // Dropping the tab drops `close`, which ends the session and its runtime.
    let (close, mut closed) = tokio::sync::watch::channel(());
    let (sh, ctx2) = (shared.clone(), ctx_slot);
    let spawned = std::thread::Builder::new().name("rconn-session".into()).spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let result = rt.block_on(async {
            let from_ui = FromUi { input: input_rx, control: kf_from_ui, files: files_rx };
            tokio::select! {
                r = session(args, muted_audio, cursor_for_session, sh.clone(), from_ui, files_for_session, ctx2.clone()) => Some(r),
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
        video: crate::video_render::VideoView::new(),
        frame_size: None,
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
        fit: FitRequest::default(),
        match_resolution,
        prefs_path,
        last_stats: String::new(),
        confirm_restart: false,
        toast: None,
        closing: false,
        host,
        files: crate::files::Panel::new(files_tx, files_state),
        files_rect: None,
        _close: Some(close),
    }
}

/// Open the session window with a first tab; it blocks until the window closes.
/// Later `rconn-viewer connect` runs add tabs to it (see [`hand_off`]).
pub fn run(args: ConnectArgs) -> Result<()> {
    let chromeless = args.chromeless;
    let title = format!("Rconn — {}", args.name);
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
            } else if cfg!(target_os = "macos") {
                // Tabs live in the title bar, as in RustDesk and Chrome: more height for
                // the picture, so a maximised window has the remote screen's shape.
                v.with_fullsize_content_view(true)
                    .with_titlebar_shown(false)
                    .with_title_shown(false)
            } else {
                v
            }
            .with_drag_and_drop(true) // files dropped on the window go to the remote computer
        },
        // Show each picture as soon as it is drawn: waiting for the display's refresh
        // (the default) adds up to two frames (16-33 ms at 60 Hz) of delay.
        wgpu_options: eframe::WgpuConfiguration {
            surface: eframe::SurfaceConfig {
                present_mode: eframe::wgpu::PresentMode::AutoNoVsync,
                desired_maximum_frame_latency: Some(1),
            },
            ..Default::default()
        },
        ..Default::default()
    };
    eframe::run_native(
        "Rconn",
        options,
        Box::new(move |cc| {
            *ctx_slot.lock().expect("ctx") = Some(cc.egui_ctx.clone());
            if let Some(rs) = &cc.wgpu_render_state {
                crate::video_render::install(rs);
            }
            let first = open_tab(args, ctx_slot.clone());
            Ok(Box::new(Window {
                title: String::new(),
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
    /// Title last set on the OS window.
    title: String,
    tabs: Vec<ViewerApp>,
    active: usize,
    incoming: std::sync::mpsc::Receiver<ConnectArgs>,
    ctx_slot: Arc<Mutex<Option<egui::Context>>>,
}

const TABS_H: f32 = 32.0;
/// Room for the macOS window buttons, which sit over the tab strip.
const WINDOW_BUTTONS_W: f32 = if cfg!(target_os = "macos") { 72.0 } else { 0.0 };

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
                    ui.add_space(WINDOW_BUTTONS_W);
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
                    // The active computer's toolbar, in the same row: one strip of
                    // window chrome leaves the most room (and the right shape) for the
                    // picture.
                    ui.separator();
                    let tab = &mut self.tabs[self.active];
                    let stats = tab.last_stats.clone();
                    tab.bar_contents(ui, &stats, false);
                    // The empty rest of the strip is the title bar: drag to move the
                    // window, double-click to zoom it.
                    let rest =
                        ui.allocate_rect(ui.available_rect_before_wrap(), Sense::click_and_drag());
                    if rest.drag_started() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                    if rest.double_clicked() {
                        let max = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
                        ui.ctx()
                            .send_viewport_cmd(egui::ViewportCommand::Maximized(!max));
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
                .position(|t| t.host == args.host && !t.closing && args.guest.is_none())
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
        // Only on change: every viewport command makes egui repaint, so sending the title
        // each frame would redraw the window nonstop (100%+ CPU).
        let title = format!("Rconn — {}", tab.name);
        if self.title != title {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
        tab.draw(ui);
    }
}

/// What the tab's UI sends the session.
struct FromUi {
    input: mpsc::UnboundedReceiver<InputEvent>,
    /// Control messages (keyframe requests, toolbar actions).
    control: mpsc::UnboundedReceiver<Control>,
    files: mpsc::UnboundedReceiver<crate::files::Cmd>,
}

async fn session(
    args: ConnectArgs,
    muted: Arc<std::sync::atomic::AtomicBool>,
    shared_cursor: Arc<Mutex<Option<rconn_core::protocol::CursorShape>>>,
    shared: Arc<Mutex<Shared>>,
    from_ui: FromUi,
    files_state: crate::files::Shared,
    ctx: Arc<Mutex<Option<egui::Context>>>,
) -> Result<()> {
    let FromUi {
        mut input,
        control: mut kf_from_ui,
        files: files_cmds,
    } = from_ui;
    let repaint = || {
        if let Some(c) = ctx.lock().expect("ctx").as_ref() {
            c.request_repaint();
        }
    };
    let trust = ServerTrust::from_fingerprint(args.fingerprint.as_deref());
    let (sig, host) = match &args.guest {
        Some(g) => SignalingClient::connect_guest(&args.server, &trust, &g.id).await?,
        None => (
            SignalingClient::connect_operator(&args.server, &trust, &args.token).await?,
            args.host,
        ),
    };
    let opts = PeerOptions {
        udp_addrs: args.udp_addrs.clone(),
        relay_only: args.relay_only,
    };
    let mut session = open_viewer_session(sig, host, opts).await?;
    let mut exchange = args
        .guest
        .as_ref()
        .map(|g| rconn_peer::guest::Exchange::new(g.password.clone(), &session));
    shared.lock().expect("ui state").status = match &args.guest {
        Some(g) if g.password.is_empty() => "asking the computer…".into(),
        Some(_) => "checking the access password…".into(),
        None => "connected, waiting for the picture…".into(),
    };
    repaint();
    // The computer refused the access password: the session ends with its reason.
    let (refused_tx, mut refused_rx) = tokio::sync::oneshot::channel::<String>();
    let mut refused_tx = Some(refused_tx);
    let status_shared = shared.clone();
    let status_ctx = ctx.clone();

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
    // File transfer: its own channel, opened on first use once the session is let in.
    let (allowed_tx, allowed_rx) = tokio::sync::watch::channel(false);
    let files_repaint = {
        let ctx = ctx.clone();
        move || {
            if let Some(c) = ctx.lock().expect("ctx").as_ref() {
                c.request_repaint();
            }
        }
    };
    let files_task = tokio::spawn(crate::files::run(
        session.peer.pc.clone(),
        files_cmds,
        files_state,
        allowed_rx,
        files_repaint,
    ));
    let guest = args.guest.is_some();
    // Scrolls and window moves as copy instructions (see rconn_media::copyrect).
    let copy_rects = rconn_media::copyrect::enabled();
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
                        if !guest {
                            let _ = allowed_tx.send(true);
                        }
                        send(dc.clone(), DataMessage::Control(Control::RequestKeyframe)).await;
                        if copy_rects {
                            send(dc.clone(), DataMessage::Control(Control::CopyRects { version: rconn_media::copyrect::VERSION })).await;
                        }
                    }
                    Some(DataChannelEvent::OnMessage(msg)) => match DataMessage::decode(&msg.data) {
                        Ok(DataMessage::Clipboard(text)) => clip.apply_remote(text),
                        Ok(DataMessage::HostName(name)) => {
                            // The Rconn app asks for the name to label its recent list.
                            if let (Some(path), Some(name)) = (
                                std::env::var_os("RCONN_NAME_FILE"),
                                rconn_core::protocol::tidy_host_name(&name),
                            ) {
                                let _ = std::fs::write(path, name);
                            }
                        }
                        Ok(DataMessage::Control(Control::Cursor(shape))) => {
                            cursor.lock().expect("cursor").replace(shape);
                            repaint_cursor();
                        }
                        Ok(DataMessage::Guest(m)) => {
                            if let Some(ex) = exchange.as_mut() {
                                match ex.handle(m).await {
                                    rconn_peer::guest::Step::Send(reply) => send(dc.clone(), reply).await,
                                    rconn_peer::guest::Step::Waiting(text) => {
                                        status_shared.lock().expect("ui state").status = text;
                                        if let Some(c) = status_ctx.lock().expect("ctx").as_ref() {
                                            c.request_repaint();
                                        }
                                    }
                                    rconn_peer::guest::Step::Accepted => {
                                        let _ = allowed_tx.send(true);
                                        status_shared.lock().expect("ui state").status =
                                            "accepted, waiting for the picture…".into();
                                        if let Some(c) = status_ctx.lock().expect("ctx").as_ref() {
                                            c.request_repaint();
                                        }
                                        send(dc.clone(), DataMessage::Control(Control::RequestKeyframe)).await;
                                        // Guests: the host takes nothing before this point.
                                        if copy_rects {
                                            send(dc.clone(), DataMessage::Control(Control::CopyRects { version: rconn_media::copyrect::VERSION })).await;
                                        }
                                    }
                                    rconn_peer::guest::Step::Refused(why) => {
                                        if let Some(tx) = refused_tx.take() {
                                            let _ = tx.send(why);
                                        }
                                    }
                                }
                            }
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
    // Guests may wait for the person at the computer (up to a minute on the host's side).
    let wait = if args.guest.is_some() { 75 } else { 20 };
    let track = tokio::select! {
        t = tokio::time::timeout(Duration::from_secs(wait), video_rx.recv()) => t
            .map_err(|_| anyhow!("no video from the host within {wait} s"))?
            .ok_or_else(|| anyhow!("session closed before video arrived"))?,
        Ok(why) = &mut refused_rx => {
            dc_task.abort();
            router.abort();
            files_task.abort();
            session.close().await;
            anyhow::bail!("{why}");
        }
    };

    let mut rx = VideoReceiver::default();
    let mut dec = VideoDecoder::new(&args.openh264)?;
    tracing::info!(decoder = dec.engine_name(), "video decoder");
    // Builds the picture from copy instructions, when the host sends them.
    let mut compositor = Compositor::new();
    let (mut frames, mut bytes, mut since) = (0u32, 0usize, Instant::now());
    let mut decode_time = Duration::ZERO;
    // Lost packets, decode errors, or video with no picture coming out of it: ask for a
    // fresh keyframe (throttled; each one costs the host a full-size encode).
    let mut need = KeyframeNeed::default();
    // Wakes the loop without packets: a lost keyframe on a still screen sends nothing more.
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let pkt = tokio::select! {
            ev = track.poll() => match ev {
                None => break,
                Some(TrackRemoteEvent::OnRtpPacket(pkt)) => Some(pkt),
                Some(_) => None,
            },
            _ = tick.tick() => None,
        };
        if let Some(pkt) = pkt {
            if !pkt.payload.is_empty() {
                need.data(Instant::now());
            }
            let aus = rx.push(pkt);
            // Video data was lost for good: later pictures build on a broken one.
            if rx.take_loss() {
                tracing::warn!("video packets lost");
                need.lost();
                compositor.lost();
            }
            for au in aus {
                bytes += au.len();
                let (bits, ops) = copyrect::split(&au);
                match dec.decode(&bits) {
                    Ok(Some(mut f)) => {
                        if compositor.compose(ops.as_ref(), &mut f) == Composed::Resync {
                            // A picture (or its instructions) was missed: hold the last
                            // picture until a whole one comes.
                            tracing::debug!("copy instructions out of step; asking for a keyframe");
                            need.lost();
                            continue;
                        }
                        frames += 1;
                        decode_time += f.decode_time;
                        need.picture();
                        let mut s = shared.lock().expect("ui state");
                        s.frame = Some(f);
                        s.status.clear();
                        drop(s);
                        repaint();
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::debug!(error = %e, "decode error");
                        need.lost();
                        compositor.lost();
                    }
                }
            }
        }
        if need.poll(Instant::now()) {
            tracing::warn!("no picture from the video; requesting keyframe");
            let _ = kf_tx.send(());
        }
        if since.elapsed() >= Duration::from_secs(1) {
            let secs = since.elapsed().as_secs_f32();
            let mut s = shared.lock().expect("ui state");
            s.fps = frames as f32 / secs;
            s.kbps = bytes as f32 * 8.0 / secs / 1000.0;
            if frames > 0 {
                s.decode_ms = decode_time.as_secs_f32() * 1e3 / frames as f32;
            }
            drop(s);
            (frames, bytes, since) = (0, 0, Instant::now());
            decode_time = Duration::ZERO;
            repaint(); // the toolbar's numbers, also when no picture comes
        }
    }
    dc_task.abort();
    router.abort();
    files_task.abort();
    session.close().await;
    Ok(())
}

/// The window must keep a size this long before the remote screen follows it.
const FIT_SETTLE: Duration = Duration::from_millis(400);
/// Smaller differences from the size last asked for are not worth a mode switch.
const FIT_MIN_CHANGE: u32 = 16;

/// What [`FitRequest::update`] says to do.
#[derive(Debug, PartialEq)]
enum FitStep {
    /// Ask the host for this size now.
    Send((u32, u32)),
    /// Look again after this long.
    Wait(Duration),
    Idle,
}

/// Debounces "fit the remote screen to this window": each mode switch blanks the remote
/// screen and restarts its capture, so only a size the window has kept for
/// [`FIT_SETTLE`], and not one a few pixels from the last, is sent.
#[derive(Debug, Default)]
struct FitRequest {
    /// The size last asked for.
    sent: Option<(u32, u32)>,
    /// A new size and since when the window has had it.
    pending: Option<((u32, u32), Instant)>,
}

impl FitRequest {
    fn update(&mut self, want: (u32, u32), now: Instant) -> FitStep {
        if let Some(sent) = self.sent
            && sent.0.abs_diff(want.0) < FIT_MIN_CHANGE
            && sent.1.abs_diff(want.1) < FIT_MIN_CHANGE
        {
            self.pending = None;
            return FitStep::Idle;
        }
        match self.pending {
            Some((size, since)) if size == want => {
                let held = now.saturating_duration_since(since);
                if held >= FIT_SETTLE {
                    self.sent = Some(want);
                    self.pending = None;
                    FitStep::Send(want)
                } else {
                    FitStep::Wait(FIT_SETTLE - held)
                }
            }
            _ => {
                self.pending = Some((want, now));
                FitStep::Wait(FIT_SETTLE)
            }
        }
    }

    /// Forget what was asked; whether anything had been (so the host should restore).
    fn reset(&mut self) -> bool {
        self.pending = None;
        self.sent.take().is_some()
    }
}

struct ViewerApp {
    shared: Arc<Mutex<Shared>>,
    input: mpsc::UnboundedSender<InputEvent>,
    /// Control messages for the host (keyframe requests, toolbar actions).
    keyframe: mpsc::UnboundedSender<Control>,
    /// The picture on screen (uploaded and drawn on the GPU).
    video: crate::video_render::VideoView,
    /// The remote picture's size, once one has arrived.
    frame_size: Option<[usize; 2]>,
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
    last_frame: Option<Arc<DecodedFrame>>,
    /// "Match resolution to window": the remote screen's resolution follows this window.
    /// Off (the default) keeps the remote resolution and scales the picture.
    match_resolution: bool,
    /// The sizes asked of the host while matching.
    fit: FitRequest,
    /// Where this tab's per-computer choices are saved (`None`: not saved).
    prefs_path: Option<std::path::PathBuf>,
    /// The toolbar's stats line, as of the last frame drawn.
    last_stats: String,
    /// "Restart remote computer" is waiting for confirmation.
    confirm_restart: bool,
    /// Short feedback shown in the toolbar, until the deadline.
    toast: Option<(String, Instant)>,
    /// Disconnect was pressed; the window removes this tab.
    closing: bool,
    /// The computer this tab shows.
    host: HostId,
    /// The Files panel (and its transfers).
    files: crate::files::Panel,
    /// Where the Files panel is on screen; pointer input there stays local.
    files_rect: Option<Rect>,
    /// Dropped with the tab, which ends the session.
    _close: Option<tokio::sync::watch::Sender<()>>,
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
        let (chrome, panel) = (self.chrome, self.files_rect);
        let on_chrome =
            |p: Pos2| chrome.is_some_and(|r| r.contains(p)) || panel.is_some_and(|r| r.contains(p));
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
    let chrome = TABS_H;
    let max = monitor.unwrap_or(Vec2::new(1600.0, 1000.0)) * 0.9 - Vec2::new(0.0, chrome);
    let scale = (max.x / remote.x).min(max.y / remote.y).min(1.0);
    Vec2::new(
        (remote.x * scale).round(),
        (remote.y * scale).round() + chrome,
    )
}

/// Largest rect with `size`'s shape centred in `area`, or all of `area` when the shapes
/// differ by 4% or less: a stretch that small can't be seen, a sliver of black can.
pub fn fit(area: Rect, size: Vec2) -> Rect {
    let mismatch = (area.aspect_ratio() / (size.x / size.y)).ln().abs();
    if mismatch <= 0.04 {
        return area;
    }
    let scale = (area.width() / size.x).min(area.height() / size.y);
    Rect::from_center_size(area.center(), size * scale)
}

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

    fn stats(&self, fps: f32, kbps: f32, decode_ms: f32) -> String {
        match self.frame_size {
            Some([w, h]) => format!(
                "{w}x{h}  ·  {fps:.0} fps  ·  {kbps:.0} kbps  ·  decode {decode_ms:.1} + upload {:.1} ms",
                self.video.upload_ms()
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
            // Off (the default): the remote screen keeps its resolution and the picture is
            // scaled to the window. Remembered for this computer.
            let mut matching = self.match_resolution;
            if ui
                .checkbox(&mut matching, "⛶ Match resolution to window")
                .on_hover_text(
                    "Change the remote screen's resolution to fit this window; it goes back \
                     when you turn this off or disconnect. Off: the picture is scaled.",
                )
                .changed()
            {
                self.set_match_resolution(matching);
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
        let active = self.files.active();
        let label = if active > 0 {
            format!("📁 Files ({active})")
        } else {
            "📁 Files".to_owned()
        };
        if ui
            .selectable_label(self.files.open, label)
            .on_hover_text(
                "Copy files to and from the remote computer (or drop files on the window)",
            )
            .clicked()
        {
            self.files.open = !self.files.open;
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
                "Rconn",
                egui::FontId::proportional(13.0),
                Color32::from_gray(220),
            );
            self.chrome = Some(tab);
        }
    }

    /// With "Match resolution to window" on, ask the host to fit its screen to `area`
    /// (in pixels) once the window has stopped changing size for a moment: no black bars,
    /// and the host's pixels map 1:1 to ours.
    fn fit_remote_to(&mut self, area: Rect, ctx: &egui::Context) {
        if self.frame_size.is_none() || !self.match_resolution {
            return; // not connected yet, or scaling the picture instead
        }
        let ppp = ctx.pixels_per_point();
        let px = |v: f32| ((v * ppp).round() as u32 & !1).max(480);
        let want = (px(area.width()), px(area.height()));
        match self.fit.update(want, Instant::now()) {
            FitStep::Send((width, height)) => {
                let _ = self.keyframe.send(Control::Resize { width, height });
            }
            FitStep::Wait(d) => ctx.request_repaint_after(d),
            FitStep::Idle => {}
        }
    }

    /// The toolbar's "Match resolution" toggle: remembered for this computer; turning it
    /// off gives the remote screen its own resolution back.
    fn set_match_resolution(&mut self, on: bool) {
        self.match_resolution = on;
        if !on && self.fit.reset() {
            let _ = self.keyframe.send(Control::RestoreResolution);
        }
        if let Some(path) = &self.prefs_path {
            let prefs = crate::prefs::HostPrefs {
                match_resolution: on,
            };
            if let Err(e) = crate::prefs::save(path, self.host, &prefs) {
                tracing::warn!(error = %e, "could not save viewer preferences");
            }
        }
    }

    fn draw(&mut self, ui: &mut egui::Ui) {
        let (new_frame, status, fps, kbps, decode_ms) = {
            let mut s = self.shared.lock().expect("ui state");
            (s.frame.take(), s.status.clone(), s.fps, s.kbps, s.decode_ms)
        };
        if let Some(f) = new_frame {
            // The YUV planes go to the GPU as they are (see `video_render`).
            let f = Arc::new(f);
            self.video.submit(f.clone());
            // Fit the window to the first picture only: later size changes are the host
            // trading resolution for speed, and keep the same shape.
            self.fit_window |= self.frame_size.is_none();
            self.frame_size = Some([f.width, f.height]);
            self.last_frame = Some(f);
        }
        let fullscreen = self.is_fullscreen(ui);
        if std::mem::take(&mut self.fit_window)
            && !fullscreen
            && let Some([w, h]) = self.frame_size
        {
            let monitor = ui.ctx().input(|i| i.viewport().monitor_size);
            let size = window_size_for(Vec2::new(w as f32, h as f32), monitor);
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        }
        let stats = self.stats(fps, kbps, decode_ms);
        self.last_stats.clone_from(&stats);
        let ctx = ui.ctx().clone();
        self.restart_dialog(&ctx);
        // Files dropped on the window go to the remote computer.
        let (dropped, hovering) = ctx.input(|i| {
            (
                i.raw
                    .dropped_files
                    .iter()
                    .map(|f| f.path().to_path_buf())
                    .collect::<Vec<_>>(),
                !i.raw.hovered_files.is_empty(),
            )
        });
        let n = self.files.upload(dropped);
        if n > 0 {
            self.notify(format!("Sending {n} file(s) to {}", self.name));
        }
        let name = self.name.clone();
        self.files_rect = self.files.show(&ctx, &name);
        if !fullscreen {
            // In a window the toolbar shares the tab strip's row (see `Window::tab_strip`).
            self.chrome = None;
        }
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let area = ui.max_rect();
            self.fit_remote_to(area, ui.ctx());
            ui.painter().rect_filled(area, 0.0, Color32::BLACK);
            let _ = ui.allocate_rect(area, Sense::click_and_drag());
            let image = self.frame_size.map(|[w, h]| {
                let image = fit(area, Vec2::new(w as f32, h as f32));
                self.video.paint(ui.painter(), image);
                image
            });
            if fullscreen {
                self.floating_bar(ui, area, &stats);
            }
            if let Some(image) = image {
                self.forward_input(ui, image);
                let over_picture = ui.ctx().pointer_hover_pos().is_some_and(|p| {
                    image.contains(p)
                        && !self.chrome.is_some_and(|r| r.contains(p))
                        && !self.files_rect.is_some_and(|r| r.contains(p))
                });
                let shape = *self.cursor.lock().expect("cursor");
                if over_picture && let Some(shape) = shape {
                    ui.ctx().set_cursor_icon(cursor_icon(shape));
                }
            }
            if hovering {
                ui.painter()
                    .rect_filled(area.shrink(12.0), 12.0, Color32::from_black_alpha(160));
                ui.painter().text(
                    area.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("Drop to send to {} (Downloads/Rconn)", self.name),
                    egui::FontId::proportional(22.0),
                    Color32::WHITE,
                );
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
        assert_eq!(big, Vec2::new(1496.0, 938.0 + TABS_H), "1:1 when it fits");
        let small = window_size_for(remote, Some(Vec2::new(1440.0, 900.0)));
        let picture = Vec2::new(small.x, small.y - TABS_H);
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
    fn the_remote_screen_follows_the_window_only_once_it_settles() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut f = FitRequest::default();
        // A drag: a new size every 16 ms for a second, nothing sent.
        for i in 0..60u32 {
            let step = f.update((1000 + i * 8, 700), at(u64::from(i) * 16));
            assert!(matches!(step, FitStep::Wait(_)), "{step:?}");
        }
        let last = (1000 + 59 * 8, 700);
        assert!(matches!(f.update(last, at(1200)), FitStep::Wait(_)));
        assert_eq!(
            f.update(last, at(1344)),
            FitStep::Send(last),
            "400 ms still"
        );
        assert_eq!(f.update(last, at(2000)), FitStep::Idle, "sent once");
        // A few pixels (a scrollbar, rounding) are not worth a mode switch.
        assert_eq!(f.update((last.0 + 6, 708), at(3000)), FitStep::Idle);
        assert_eq!(f.update((last.0 + 6, 708), at(4000)), FitStep::Idle);
        // A real change is, once it settles.
        assert!(matches!(f.update((1280, 800), at(5000)), FitStep::Wait(_)));
        assert_eq!(f.update((1280, 800), at(5400)), FitStep::Send((1280, 800)));
        // Turning matching off: the host is told to restore, once.
        assert!(f.reset());
        assert!(!f.reset());
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
            frame: Some(DecodedFrame::from_rgba(w, h, &rgba)),
            status: String::new(),
            fps: 29.0,
            kbps: 2400.0,
            decode_ms: 2.0,
        }));
        let keys = KeyGate {
            toolbar: hotkey,
            ..Default::default()
        };
        ViewerApp {
            shared,
            input,
            keyframe,
            video: crate::video_render::VideoView::new(),
            frame_size: None,
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
            fit: FitRequest::default(),
            match_resolution: false,
            prefs_path: None,
            last_stats: String::new(),
            confirm_restart: false,
            toast: None,
            closing: false,
            host: HostId(uuid::Uuid::nil()),
            files: crate::files::Panel::new(mpsc::unbounded_channel().0, Default::default()),
            files_rect: None,
            _close: None,
        }
    }

    #[test]
    fn scaling_is_the_default_and_no_resize_is_sent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut a = app(false, false);
        a.keyframe = tx;
        a.frame_size = Some([4, 4]);
        let ctx = egui::Context::default();
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0));
        a.fit_remote_to(area, &ctx);
        a.fit.pending = a.fit.pending.map(|(s, t)| (s, t - Duration::from_secs(1)));
        a.fit_remote_to(area, &ctx);
        assert!(
            rx.try_recv().is_err(),
            "off by default: the remote keeps its resolution"
        );
        // Matching on: the settled size goes out; off again: the host is told to restore.
        a.set_match_resolution(true);
        a.fit_remote_to(area, &ctx);
        a.fit.pending = a.fit.pending.map(|(s, t)| (s, t - Duration::from_secs(1)));
        a.fit_remote_to(area, &ctx);
        assert_eq!(
            rx.try_recv().ok(),
            Some(Control::Resize {
                width: 1200,
                height: 800
            })
        );
        a.set_match_resolution(false);
        assert_eq!(rx.try_recv().ok(), Some(Control::RestoreResolution));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn the_files_panel_shows_both_computers_and_transfer_progress() {
        use rconn_core::files::FileEntry;
        let mut a = app(false, false);
        {
            let mut s = a.files.state.lock().unwrap();
            s.entries = Some(vec![
                FileEntry {
                    name: "Documents".into(),
                    is_dir: true,
                    size: 0,
                    modified: None,
                },
                FileEntry {
                    name: "report.pdf".into(),
                    is_dir: false,
                    size: 2_400_000,
                    modified: None,
                },
            ]);
            let p = rconn_peer::files::Progress::new();
            s.transfers.push(crate::files::Transfer {
                upload: true,
                name: "movie.mp4".into(),
                progress: p,
                outcome: None,
            });
            s.transfers.push(crate::files::Transfer {
                upload: false,
                name: "report.pdf".into(),
                progress: rconn_peer::files::Progress::new(),
                outcome: Some(Err("checksum mismatch".into())),
            });
        }
        a.files.open = true;
        let a = render("viewer-files", a);
        let rect = a.files_rect.expect("panel shown");
        assert!(rect.width() > 400.0, "{rect:?}");
        assert_eq!(a.files.active(), 1);
    }

    /// The test renderer, with the video pipelines installed (as the window has).
    fn renderer() -> egui_kittest::wgpu::WgpuTestRenderer {
        let rs = egui_kittest::wgpu::create_render_state(
            egui_kittest::wgpu::default_wgpu_setup(),
            eframe::egui_wgpu::RendererOptions::PREDICTABLE,
        );
        crate::video_render::install(&rs);
        egui_kittest::wgpu::WgpuTestRenderer::from_render_state(rs)
    }

    fn render(name: &str, app: ViewerApp) -> ViewerApp {
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1100.0, 700.0))
            .renderer(renderer())
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
            title: String::new(),
            tabs: vec![app(false, false), second],
            active: 1,
            incoming,
            ctx_slot: Arc::default(),
        };
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1100.0, 700.0))
            .renderer(renderer())
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
