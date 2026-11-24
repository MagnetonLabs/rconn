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
}

#[derive(Default)]
struct Shared {
    frame: Option<DecodedFrame>,
    status: String,
    fps: f32,
    kbps: f32,
}

pub fn run(args: ConnectArgs) -> Result<()> {
    let shared = Arc::new(Mutex::new(Shared {
        status: "connecting…".into(),
        ..Default::default()
    }));
    let (input_tx, input_rx) = mpsc::unbounded_channel::<InputEvent>();
    let ctx_slot: Arc<Mutex<Option<egui::Context>>> = Arc::default();

    // Background session.
    let (sh, ctx2) = (shared.clone(), ctx_slot.clone());
    std::thread::Builder::new()
        .name("rconn-session".into())
        .spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            let result = rt.block_on(session(args, sh.clone(), input_rx, ctx2.clone()));
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
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_title("rconn"),
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
                texture: None,
                last_pointer: None,
            }))
        }),
    )
    .map_err(|e| anyhow!("window: {e}"))
}

async fn session(
    args: ConnectArgs,
    shared: Arc<Mutex<Shared>>,
    mut input: mpsc::UnboundedReceiver<InputEvent>,
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
        loop {
            tokio::select! {
                ev = dc.poll() => match ev {
                    Some(DataChannelEvent::OnOpen) => {
                        open = true;
                        send(dc.clone(), DataMessage::Control(Control::RequestKeyframe)).await;
                    }
                    Some(DataChannelEvent::OnClose) | None => break,
                    _ => {}
                },
                ev = input.recv() => match ev {
                    Some(ev) if open => send(dc.clone(), DataMessage::Input(ev)).await,
                    Some(_) => {}
                    None => break,
                },
                req = kf_rx.recv() => if req.is_some() && open {
                    send(dc.clone(), DataMessage::Control(Control::RequestKeyframe)).await;
                },
            }
        }
    });

    let track = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match session.peer.events.recv().await {
                Some(PeerEvent::Track(t)) => return Ok(t),
                Some(_) => continue,
                None => return Err(anyhow!("session closed before video arrived")),
            }
        }
    })
    .await
    .map_err(|_| anyhow!("no video from the host within 20 s"))??;

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
    session.close().await;
    Ok(())
}

struct ViewerApp {
    shared: Arc<Mutex<Shared>>,
    input: mpsc::UnboundedSender<InputEvent>,
    texture: Option<TextureHandle>,
    last_pointer: Option<(f32, f32)>,
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
                egui::Event::Key {
                    key,
                    physical_key,
                    pressed,
                    repeat,
                    ..
                } => {
                    if let Some(hid) = to_hid(physical_key.unwrap_or(key)) {
                        // Key repeat is the host's job; forward only transitions.
                        if !repeat {
                            self.send(InputEvent::Key {
                                hid_usage: hid,
                                pressed,
                            });
                        }
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

        let area = ui.available_rect_before_wrap();
        ui.painter().rect_filled(area, 0.0, Color32::BLACK);
        let (_, _) = ui.allocate_exact_size(area.size(), Sense::click_and_drag());
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
}
