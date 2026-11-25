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
    /// Don't play the host's sound.
    pub mute: bool,
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
                .with_title("rconn");
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
            }))
        }),
    )
    .map_err(|e| anyhow!("window: {e}"))
}

async fn session(
    args: ConnectArgs,
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
    let mute = args.mute;
    let mut events = std::mem::replace(&mut session.peer.events, mpsc::unbounded_channel().1);
    let router = tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            let PeerEvent::Track(t) = ev else { continue };
            if t.kind().await == rtc::rtp_transceiver::rtp_sender::RtpCodecKind::Audio {
                if mute {
                    continue;
                }
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
    /// Borderless mode: no toolbar strip, only the hotkey toolbar.
    chromeless: bool,
}

impl ViewerApp {
    fn send(&self, ev: InputEvent) {
        let _ = self.input.send(ev);
    }

    /// Translate this frame's egui input into remote input, relative to `image` on screen.
    fn forward_input(&mut self, ui: &egui::Ui, image: Rect) {
        let events = ui.ctx().input(|i| i.events.clone());
        for ev in events {
            match ev {
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

/// Largest rect with `aspect` (w/h) centred in `area`.
pub fn fit(area: Rect, size: Vec2) -> Rect {
    let scale = (area.width() / size.x).min(area.height() / size.y);
    Rect::from_center_size(area.center(), size * scale)
}

impl ViewerApp {
    fn toolbar_buttons(&mut self, ui: &mut egui::Ui) {
        if ui
            .button(if self.fullscreen {
                "Exit full screen"
            } else {
                "🔳 Full screen"
            })
            .clicked()
        {
            self.fullscreen = !self.fullscreen;
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }
        if ui.button("🔄 Refresh picture").clicked() {
            let _ = self.keyframe.send(());
        }
        if ui.button("❌ Disconnect").clicked() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Floating toolbar (hotkey); owns the keyboard while open.
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        egui::Area::new(egui::Id::unique("toolbar"))
            .anchor(egui::Align2::CENTER_TOP, [0.0, 8.0])
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        self.toolbar_buttons(ui);
                        ui.label(format!("{HOTKEY_HINT} to return"));
                    });
                });
            });
    }

    /// Fixed strip above the picture in a normal window.
    fn toolbar_strip(&mut self, ui: &mut egui::Ui, strip: Rect, stats: &str) {
        let mut child =
            ui.new_child(egui::UiBuilder::new().max_rect(strip.shrink2(Vec2::new(8.0, 4.0))));
        child.horizontal_centered(|ui| {
            self.toolbar_buttons(ui);
            ui.separator();
            ui.label(egui::RichText::new(stats).color(Color32::GRAY));
        });
    }
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
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
                        Some(ui.ctx().load_texture("screen", img, TextureOptions::LINEAR))
                }
            }
        }

        let full = ui.available_rect_before_wrap();
        let strip_h = if self.chromeless || self.fullscreen {
            0.0
        } else {
            34.0
        };
        let (strip, area) = full.split_top_bottom_at_y(full.min.y + strip_h);
        ui.painter().rect_filled(full, 0.0, Color32::BLACK);
        let (_, _) = ui.allocate_exact_size(full.size(), Sense::click_and_drag());
        if strip_h > 0.0 {
            ui.painter()
                .rect_filled(strip, 0.0, ui.visuals().panel_fill);
            let stats = match &self.texture {
                Some(t) => format!(
                    "{}x{} · {fps:.0} fps · {kbps:.0} kbps",
                    t.size()[0],
                    t.size()[1]
                ),
                None => String::new(),
            };
            self.toolbar_strip(ui, strip, &stats);
        }
        if let Some(tex) = &self.texture {
            let [w, h] = tex.size();
            let image = fit(area, Vec2::new(w as f32, h as f32));
            ui.painter().image(
                tex.id(),
                image,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
            self.forward_input(ui, image);
            if self.keys.toolbar {
                self.toolbar(ui);
            }
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Title(format!(
                    "rconn — {w}x{h} · {fps:.0} fps · {kbps:.0} kbps"
                )));
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
