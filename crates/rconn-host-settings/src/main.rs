//! `rconn-settings`: the host settings window (run as administrator).
#![cfg_attr(windows, windows_subsystem = "windows")] // no console window behind the GUI

use rconn_host_settings::model::Model;
use rconn_host_settings::{platform, ui};

struct App(Model);

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            let actions = ui::draw(ui, &mut self.0);
            if actions.relaunch_elevated {
                match platform::relaunch_elevated() {
                    Ok(()) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
                    Err(e) => self.0.messages = vec![(e, true)],
                }
            }
            if actions.restart_agent {
                self.0.messages = vec![match platform::restart_agent() {
                    Ok(()) => (
                        "Agent restarting; new settings apply within a few seconds.".into(),
                        false,
                    ),
                    Err(e) => (format!("Could not restart the agent: {e}"), true),
                }];
            }
        });
    }
}

fn main() -> eframe::Result {
    let data_dir = std::env::args()
        .nth(1)
        .map_or_else(platform::default_data_dir, Into::into);
    let model = Model::load(&data_dir);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 520.0])
            .with_title("rconn settings"),
        ..Default::default()
    };
    eframe::run_native(
        "rconn settings",
        options,
        Box::new(|_| Ok(Box::new(App(model)))),
    )
}
