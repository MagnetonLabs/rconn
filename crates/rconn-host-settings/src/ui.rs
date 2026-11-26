//! The settings window layout.

use crate::model::{Load, Model};
use egui::{Color32, RichText};
use rconn_core::settings::{HiddenMode, HostKind};

/// What the user asked for this frame (side effects run by the caller).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Actions {
    pub restart_agent: bool,
    pub relaunch_elevated: bool,
}

const LEVELS: [&str; 4] = ["error", "warn", "info", "debug"];

pub fn draw(ui: &mut egui::Ui, m: &mut Model) -> Actions {
    let mut actions = Actions::default();
    ui.heading("Rconn host settings");
    ui.add_space(4.0);

    egui::Grid::new("status")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            ui.label("Service");
            let color = if m.service == "Running" {
                Color32::from_rgb(40, 160, 70)
            } else {
                Color32::GRAY
            };
            ui.label(RichText::new(&m.service).color(color));
            ui.end_row();
            ui.label("Host ID");
            ui.label(m.host_id.as_deref().unwrap_or("not enrolled"));
            ui.end_row();
            ui.label("Remote session");
            ui.label(if m.session_active {
                "active now"
            } else {
                "none"
            });
            ui.end_row();
        });
    ui.separator();

    if let Load::Failed(why) = &m.load {
        ui.colored_label(Color32::from_rgb(200, 60, 40), why.as_str());
        if ui.button("Restart as administrator").clicked() {
            actions.relaunch_elevated = true;
        }
        return actions;
    }
    if m.session_active {
        banner(ui, "Settings are locked while a remote session is active.");
    }

    let locks: Vec<Option<&'static str>> = [
        "server.url",
        "server.fingerprint",
        "hidden_mode",
        "host_kind",
        "clipboard",
        "max_bitrate_kbps",
        "log_level",
        "access_password",
        "ask_with_password",
        "file_transfer",
        "allow_resize",
    ]
    .iter()
    .map(|k| m.lock_reason(k))
    .collect();
    let mut access_msg: Option<(String, bool)> = None;
    let Some(form) = m.form.as_mut() else {
        return actions;
    };

    egui::Grid::new("settings")
        .num_columns(3)
        .spacing([16.0, 8.0])
        .striped(true)
        .show(ui, |ui| {
            row(ui, "Session indicator", locks[2], |ui| {
                egui::ComboBox::from_id_salt("hidden_mode")
                    .selected_text(match form.hidden_mode {
                        HiddenMode::Auto => "Automatic (show on physical PCs)",
                        HiddenMode::Indicator => "Always show",
                        HiddenMode::Hidden => "Never show",
                    })
                    .width(320.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut form.hidden_mode,
                            HiddenMode::Auto,
                            "Automatic (show on physical PCs)",
                        );
                        ui.selectable_value(
                            &mut form.hidden_mode,
                            HiddenMode::Indicator,
                            "Always show",
                        );
                        ui.selectable_value(
                            &mut form.hidden_mode,
                            HiddenMode::Hidden,
                            "Never show",
                        );
                    })
                    .response
            });
            row(ui, "Machine type", locks[3], |ui| {
                egui::ComboBox::from_id_salt("host_kind")
                    .selected_text(match form.host_kind {
                        None => "Detect automatically",
                        Some(HostKind::Vm) => "Virtual machine",
                        Some(HostKind::Physical) => "Physical computer",
                    })
                    .width(320.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut form.host_kind, None, "Detect automatically");
                        ui.selectable_value(
                            &mut form.host_kind,
                            Some(HostKind::Vm),
                            "Virtual machine",
                        );
                        ui.selectable_value(
                            &mut form.host_kind,
                            Some(HostKind::Physical),
                            "Physical computer",
                        );
                    })
                    .response
            });
            row(ui, "Clipboard sync", locks[4], |ui| {
                ui.checkbox(&mut form.clipboard, "Share clipboard text with the viewer")
            });
            row(ui, "File transfer", locks[9], |ui| {
                ui.checkbox(
                    &mut form.file_transfer,
                    "Let the viewer copy files both ways",
                )
                .on_hover_text("Your home folder is shared; received files go to Downloads/Rconn.")
            });
            row(ui, "Screen resolution", locks[10], |ui| {
                ui.checkbox(
                    &mut form.allow_resize,
                    "Let viewers change this screen's resolution",
                )
                .on_hover_text(
                    "Only when the viewer turns on \"Match resolution\"; the original \
                     resolution comes back when the session ends.",
                )
            });
            row(ui, "Max video bitrate", locks[5], |ui| {
                ui.add(
                    // Up to the server-side cap; a narrower range would silently lower the
                    // saved value just by showing it.
                    egui::Slider::new(&mut form.max_bitrate_kbps, 500..=50_000)
                        .suffix(" kbps")
                        .logarithmic(true),
                )
            });
            row(ui, "Access password", locks[7], |ui| {
                ui.vertical(|ui| {
                    ui.label(if form.access_password.is_empty() {
                        "None: whoever connects with the ID must be accepted at this computer."
                    } else {
                        "On: anyone with this computer's ID and the password can connect."
                    });
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut form.new_access_password)
                                .password(true)
                                .desired_width(200.0)
                                .hint_text("new password (8+ characters)"),
                        );
                        if ui.button("Set").clicked() {
                            match form.set_access_password() {
                                Ok(()) => {
                                    access_msg = Some(("Password set: click Save.".into(), false))
                                }
                                Err(e) => access_msg = Some((e, true)),
                            }
                        }
                        if !form.access_password.is_empty() && ui.button("Remove").clicked() {
                            form.access_password.clear();
                        }
                    });
                })
                .response
            });
            row(ui, "Without the password", locks[8], |ui| {
                ui.checkbox(
                    &mut form.ask_with_password,
                    "Also let people ask: someone here accepts or declines",
                )
                .on_hover_text(
                    "Off: with a password set, it is the only way in. Without a password, \
                     connections always need someone here to accept.",
                )
            });
            row(ui, "Log level", locks[6], |ui| {
                egui::ComboBox::from_id_salt("log_level")
                    .selected_text(form.log_level.clone())
                    .show_ui(ui, |ui| {
                        for l in LEVELS {
                            ui.selectable_value(&mut form.log_level, l.to_owned(), l);
                        }
                    })
                    .response
            });
        });
    ui.add_space(4.0);
    egui::CollapsingHeader::new("Advanced: self-hosted server")
        .id_salt("host_self_hosted_server")
        .default_open(self_hosted(&form.server_url, &form.server_pin))
        .show(ui, |ui| {
            server_fields(
                ui,
                "host_server",
                &mut form.server_url,
                &mut form.server_pin,
                [locks[0], locks[1]],
            );
        });

    if let Some(msg) = access_msg {
        m.messages = vec![msg];
    }
    ui.add_space(8.0);
    let changes = m.changes().len();
    ui.horizontal(|ui| {
        let can_save = changes > 0 && !m.session_active;
        if ui
            .add_enabled(can_save, egui::Button::new(format!("Save ({changes})")))
            .clicked()
        {
            m.save();
        }
        if ui
            .add_enabled(changes > 0, egui::Button::new("Revert"))
            .clicked()
        {
            m.revert();
        }
        if ui
            .add_enabled(
                !m.session_active,
                egui::Button::new("Apply now (restart agent)"),
            )
            .clicked()
        {
            actions.restart_agent = true;
        }
    });
    for (text, is_error) in &m.messages {
        let color = if *is_error {
            Color32::from_rgb(200, 60, 40)
        } else {
            Color32::from_rgb(40, 140, 70)
        };
        ui.colored_label(color, text.as_str());
    }
    actions
}

/// A server other than the public one (or a pin) is set: show its fields open.
pub fn self_hosted(url: &str, pin: &str) -> bool {
    let url = url.trim();
    !pin.trim().is_empty() || (!url.is_empty() && url != rconn_core::DEFAULT_SERVER)
}

/// Server URL and pin: only for a self-hosted server. The pin is only for one without a
/// public certificate (its setup prints it); everyone else leaves both empty.
pub fn server_fields(
    ui: &mut egui::Ui,
    id_salt: &str,
    url: &mut String,
    pin: &mut String,
    locks: [Option<&'static str>; 2],
) {
    // Two columns: a text field in a middle column would shrink to last frame's width.
    let fields = [
        ("Server URL", url, "wss://your-server:443/v1/ws", locks[0]),
        ("Server pin", pin, "sha256:… (usually empty)", locks[1]),
    ];
    egui::Grid::new(id_salt)
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            for (label, text, hint, lock) in fields {
                let label_id = ui.label(label).id;
                ui.horizontal(|ui| {
                    ui.add_enabled(
                        lock.is_none(),
                        egui::TextEdit::singleline(text)
                            .desired_width(340.0)
                            .hint_text(hint),
                    )
                    .labelled_by(label_id);
                    if let Some(why) = lock {
                        ui.label(
                            RichText::new(format!("🔒 {why}"))
                                .small()
                                .color(Color32::GRAY),
                        );
                    }
                });
                ui.end_row();
            }
        });
    ui.label(
        RichText::new(
            "URL empty: the public Rconn server. Pin: leave empty unless your server's setup \
             printed a pin (sha256:…).",
        )
        .small()
        .color(Color32::GRAY),
    );
    ui.add_space(4.0);
}

/// One labelled setting. The control is linked to its label for screen readers.
fn row(
    ui: &mut egui::Ui,
    label: &str,
    lock: Option<&'static str>,
    add: impl FnOnce(&mut egui::Ui) -> egui::Response,
) {
    let label_id = ui.label(label).id;
    ui.add_enabled_ui(lock.is_none(), |ui| {
        add(ui).labelled_by(label_id);
    });
    match lock {
        Some(why) => ui.label(
            RichText::new(format!("🔒 {why}"))
                .small()
                .color(Color32::GRAY),
        ),
        None => ui.label(""),
    };
    ui.end_row();
}

fn banner(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(Color32::from_rgb(255, 236, 200))
        .inner_margin(6.0)
        .corner_radius(4.0)
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(Color32::from_rgb(120, 70, 0)))
        });
    ui.add_space(6.0);
}
