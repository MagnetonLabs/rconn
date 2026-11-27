//! `rconn --ask <json>`: the question the host agent shows the person at this computer
//! when someone connects by ID without a password ("Ana on LAPTOP wants to see and control
//! this computer"). Prints `accept`, `decline` or `timeout` and exits; closing the window
//! declines. See `rconn-host-agent/src/prompt.rs` for the other end.

use egui::{Color32, FontId, RichText};
use rconn_core::guest::{ANSWER_ACCEPT, ANSWER_DECLINE, ANSWER_TIMEOUT, PromptRequest};
use std::time::{Duration, Instant};

const ACCENT: Color32 = Color32::from_rgb(28, 110, 240);
pub const WINDOW: [f32; 2] = [460.0, 230.0];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Accept,
    Decline,
    Timeout,
}

impl Answer {
    pub fn word(self) -> &'static str {
        match self {
            Self::Accept => ANSWER_ACCEPT,
            Self::Decline => ANSWER_DECLINE,
            Self::Timeout => ANSWER_TIMEOUT,
        }
    }
}

pub struct Ask {
    pub request: PromptRequest,
    pub deadline: Instant,
    pub answer: Option<Answer>,
}

impl Ask {
    pub fn new(request: PromptRequest) -> Self {
        let deadline = Instant::now() + Duration::from_secs(request.timeout_secs);
        Self {
            request,
            deadline,
            answer: None,
        }
    }
}

/// The request from `--ask`'s argument.
pub fn parse(json: &str) -> Result<PromptRequest, String> {
    serde_json::from_str(json).map_err(|e| format!("bad --ask request: {e}"))
}

/// Draw the question; sets `a.answer` once given (or timed out).
pub fn draw(ui: &mut egui::Ui, a: &mut Ask, now: Instant) {
    let left = a.deadline.saturating_duration_since(now);
    if left.is_zero() && a.answer.is_none() {
        a.answer = Some(Answer::Timeout);
    }
    egui::CentralPanel::default()
        .frame(
            egui::Frame::new()
                .inner_margin(20.0)
                .fill(ui.visuals().panel_fill),
        )
        .show(ui, |ui| {
            ui.label(
                RichText::new("Allow a connection?")
                    .font(FontId::proportional(22.0))
                    .strong(),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new(format!(
                    "“{}” wants to see and control this computer.",
                    a.request.viewer
                ))
                .size(16.0),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "That name comes from the other computer and isn't checked. Accept only if \
                     you expect this connection.",
                )
                .small()
                .color(Color32::GRAY),
            );
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                let accept =
                    egui::Button::new(RichText::new("  Accept  ").size(16.0).color(Color32::WHITE))
                        .fill(ACCENT);
                if ui.add(accept).clicked() {
                    a.answer = Some(Answer::Accept);
                }
                if ui.button(RichText::new("  Decline  ").size(16.0)).clicked() {
                    a.answer = Some(Answer::Decline);
                }
                ui.label(
                    RichText::new(format!(
                        "Declines in {} s",
                        left.as_secs_f32().ceil() as u64
                    ))
                    .color(Color32::GRAY),
                );
            });
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                a.answer = Some(Answer::Decline);
            }
        });
}

struct Window(Ask);

impl eframe::App for Window {
    /// Clear to the theme's panel colour (light or dark, following the system): eframe's
    /// default is near-black, which shows through the unfilled panels in light mode.
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if ui.input(|i| i.viewport().close_requested()) && self.0.answer.is_none() {
            self.0.answer = Some(Answer::Decline);
        }
        draw(ui, &mut self.0, Instant::now());
        if let Some(answer) = self.0.answer {
            println!("{}", answer.word());
            std::process::exit(match answer {
                Answer::Accept => 0,
                Answer::Decline => 1,
                Answer::Timeout => 2,
            });
        }
        // Keep the countdown moving.
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }
}

/// Show the question until it is answered (this never returns normally).
pub fn run(json: &str) -> anyhow::Result<()> {
    let request = parse(json).map_err(anyhow::Error::msg)?;
    let icon = crate::icon::app();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(WINDOW)
            .with_resizable(false)
            .with_always_on_top()
            .with_active(true)
            .with_title("Rconn: allow a connection?")
            .with_icon(egui::IconData {
                rgba: icon.rgba,
                width: icon.width,
                height: icon.height,
            }),
        ..Default::default()
    };
    let result = eframe::run_native(
        "Rconn question",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            Ok(Box::new(Window(Ask::new(request))))
        }),
    );
    match result {
        // The window went away without an answer (e.g. quit): that's a no.
        Ok(()) => {
            println!("{ANSWER_DECLINE}");
            Ok(())
        }
        // It never opened (no desktop): print nothing, so the agent reports that.
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}
