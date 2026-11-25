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

pub struct ConnectArgs {
    pub server: String,
    pub trust: ServerTrust,
    pub token: String,
    pub host: HostId,
    pub openh264: PathBuf,
    pub opts: PeerOptions,
    /// Borderless, maximized window; the toolbar appears on the hotkey.
    pub chromeless: bool,
    /// Don't play the host's sound (can be switched in the toolbar).
    pub mute: bool,
    /// Shown in the toolbar and title (the computer's name or ID).
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

pub fn run(args: ConnectArgs) -> Result<()> {
    let chromeless = args.chromeless;
    let name = args.name.clone();
    let muted = Arc::new(std::sync::atomic::AtomicBool::new(args.mute));
    let muted_audio = muted.clone();
    let shared = Arc::new(Mutex::new(Shared {
        status: "connecting…".into(),
        ..Default::default()
    }));
    let (input_tx, input_rx) = mpsc::unbounded_channel::<InputEvent>();
    let (kf_ui, kf_from_ui) = mpsc::unbounded_channel::<()>();
    let ctx_slot: Arc<Mutex<Option<egui::Context>>> = Arc::default();

    // Background session.
    let (sh, ctx2) = (shared.clone(), ctx_slot.clone());
    std::thread::Builder::new()
        .name("rconn-session".into())
        .spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            let result = rt.block_on(session(
                args,
                muted_audio,
                sh.clone(),
                input_rx,
                kf_from_ui,
                ctx2.clone(),
            ));
            let mut s = sh.lock().expect("ui state");
            s.status = match result {
                Ok(()) => "session ended".into(),
                Err(e) => format!("error: {e:#}"),
            };
            if let Some(ctx) = ctx2.lock().expect("ctx").as_ref() {
                ctx.request_repaint();
            }
        })?;

    let options = eframe::NativeOptions {
        viewport: {
            let v = egui::ViewportBuilder::default()
                .with_inner_size([1280.0, 800.0])
                .with_title(format!("rconn — {name}"));
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
            Ok(Box::new(ViewerApp {
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
            }))
        }),
    )
    .map_err(|e| anyhow!("window: {e}"))
}

async fn session(
    args: ConnectArgs,
    muted: Arc<std::sync::atomic::AtomicBool>,
    shared: Arc<Mutex<Shared>>,
    mut input: mpsc::UnboundedReceiver<InputEvent>,
    mut kf_from_ui: mpsc::UnboundedReceiver<()>,
    ctx: Arc<Mutex<Option<egui::Context>>>,
) -> Result<()> {
    let repaint = || {
        if let Some(c) = ctx.lock().expect("ctx").as_ref() {
            c.request_repaint();
        }
    };
    let sig = SignalingClient::connect_operator(&args.server, &args.trust, &args.token).await?;
    let mut session = open_viewer_session(sig, args.host, args.opts).await?;
    shared.lock().expect("ui state").status = "negotiating…".into();
    repaint();

    // Data channel: keyframe request on open, then forward input as it comes.
    let dc = session.data.clone();
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
                    Some(DataChannelEvent::OnMessage(msg)) => {
                        if let Ok(DataMessage::Clipboard(text)) = DataMessage::decode(&msg.data) {
                            clip.apply_remote(text);
                        }
                    }
                    Some(DataChannelEvent::OnClose) | None => break,
                    _ => {}
                },
                ev = input.recv() => match ev {
                    Some(ev) if open => send(dc.clone(), DataMessage::Input(ev)).await,
                    Some(_) => {}
                    None => break,
                },
                Some(()) = kf_from_ui.recv() => if open {
                    send(dc.clone(), DataMessage::Control(Control::RequestKeyframe)).await;
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
    keyframe: mpsc::UnboundedSender<()>,
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
    let max = monitor.unwrap_or(Vec2::new(1600.0, 1000.0)) * 0.9 - Vec2::new(0.0, BAR_H);
    let scale = (max.x / remote.x).min(max.y / remote.y).min(1.0);
    Vec2::new(
        (remote.x * scale).round(),
        (remote.y * scale).round() + BAR_H,
    )
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
        if ui
            .button("🔄 Refresh")
            .on_hover_text("Ask the host for a fresh picture")
            .clicked()
        {
            let _ = self.keyframe.send(());
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
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
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
                    self.texture =
                        Some(ui.ctx().load_texture("screen", img, TextureOptions::LINEAR));
                    self.fit_window = true; // first picture, or the remote resolution changed
                }
            }
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
            }
            if let Some(image) = image {
                self.forward_input(ui, image);
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

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_matches_the_remote_shape_and_fits_the_display() {
        let remote = Vec2::new(1496.0, 938.0);
        let big = window_size_for(remote, Some(Vec2::new(3000.0, 2000.0)));
        assert_eq!(big, Vec2::new(1496.0, 938.0 + BAR_H), "1:1 when it fits");
        let small = window_size_for(remote, Some(Vec2::new(1440.0, 900.0)));
        let picture = Vec2::new(small.x, small.y - BAR_H);
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
