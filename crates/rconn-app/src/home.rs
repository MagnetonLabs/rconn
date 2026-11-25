//! The main window: this computer's ID on the left, "connect to a computer" on the right,
//! with tabs for the computer list, recent connections and settings.

use crate::profile::Profile;
use egui::{Color32, FontId, RichText};
use rconn_core::ids::HostId;
use rconn_core::protocol::HostInfo;
use rconn_core::status::HostStatus;
use rconn_host_settings::model::Model as HostSettings;

const ACCENT: Color32 = Color32::from_rgb(28, 110, 240);
const GOOD: Color32 = Color32::from_rgb(52, 180, 110);
const BAD: Color32 = Color32::from_rgb(220, 80, 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Computers,
    Recent,
    Settings,
}

/// Things the window asks the background worker to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    SignIn {
        username: String,
        password: String,
    },
    SignInSso,
    SignOut,
    RefreshComputers,
    /// A typed ID (9-digit code or full id).
    ConnectTyped(String),
    /// A typed ID with that computer's access password: no account needed.
    ConnectGuest {
        id: String,
        password: Secret,
    },
    Connect(HostId, Option<String>),
    SaveServer {
        server: String,
        fingerprint: String,
    },
    /// Open the admin settings window (UAC prompt on Windows).
    OpenHostSettings,
    RestartHostAgent,
    /// macOS and Linux: make this computer a host ("Allow connections to this computer").
    EnableHost,
    /// macOS and Linux: stop being a host (the identity is kept).
    DisableHost,
    /// macOS: open System Settings at this permission ("Screen Recording", "Accessibility").
    OpenPermission(String),
    /// Set this computer's access password (Windows asks for administrator approval).
    SetAccessPassword(Secret),
    /// Turn access by password off for this computer.
    ClearAccessPassword,
}

/// A password that must not show up in logs or debug output.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<hidden>")
    }
}

pub struct Home {
    pub profile: Profile,
    /// This computer as a host (None: rconn host not installed here).
    pub this: Option<HostStatus>,
    pub now: u64,
    pub signed_in: bool,
    pub tab: Tab,
    pub connect_to: String,
    /// The typed computer's access password (connect without an account). Cleared once used.
    pub access_password: String,
    /// A new access password for this computer, being typed in Settings.
    pub new_host_password: String,
    /// What was just saved (on/off, when): shown until the host's status file catches up.
    pub host_password_saved: Option<(bool, u64)>,
    /// None until loaded.
    pub computers: Option<Vec<HostInfo>>,
    pub username: String,
    pub password: String,
    pub server_edit: String,
    pub fingerprint_edit: String,
    /// `(text, is_error)`.
    pub notice: Option<(String, bool)>,
    pub busy: Option<String>,
    /// The host's own settings when readable (admin); None when not installed.
    pub host_settings: Option<HostSettings>,
    /// The app itself can make this computer a host (macOS; Windows uses the installer).
    pub can_host_here: bool,
}

impl Home {
    pub fn new(profile: Profile) -> Self {
        Self {
            server_edit: profile.server.clone(),
            fingerprint_edit: profile.fingerprint.clone(),
            username: profile.username.clone(),
            profile,
            this: None,
            now: 0,
            signed_in: false,
            tab: Tab::Computers,
            connect_to: String::new(),
            access_password: String::new(),
            new_host_password: String::new(),
            host_password_saved: None,
            computers: None,
            password: String::new(),
            notice: None,
            busy: None,
            host_settings: None,
            can_host_here: cfg!(unix),
        }
    }

    /// Fill an empty server profile from the local host's own server (zero-config on hosts).
    pub fn adopt_host_server(&mut self) {
        if let Some(s) = &self.this
            && self.profile.server.is_empty()
            && !s.server.is_empty()
        {
            self.profile.server = s.server.clone();
            self.profile.fingerprint = s.fingerprint.clone().unwrap_or_default();
            self.server_edit = self.profile.server.clone();
            self.fingerprint_edit = self.profile.fingerprint.clone();
        }
    }
}

/// Find the computer a typed ID refers to.
pub fn resolve<'a>(typed: &str, computers: &'a [HostInfo]) -> Option<&'a HostInfo> {
    computers.iter().find(|h| h.host_id.matches(typed))
}

pub fn draw(ui: &mut egui::Ui, h: &mut Home) -> Vec<Request> {
    let mut out = Vec::new();
    egui::Panel::left("this_computer")
        .exact_size(270.0)
        .resizable(false)
        .frame(
            egui::Frame::new()
                .fill(ui.visuals().faint_bg_color)
                .inner_margin(20.0),
        )
        .show(ui, |ui| this_computer(ui, h, &mut out));
    egui::Panel::bottom("status")
        .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(16, 8)))
        .show(ui, |ui| status_bar(ui, h));
    egui::CentralPanel::default()
        .frame(egui::Frame::new().inner_margin(20.0))
        .show(ui, |ui| {
            connect_card(ui, h, &mut out);
            ui.add_space(14.0);
            tabs(ui, h, &mut out);
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| match h.tab {
                Tab::Computers => computers(ui, h, &mut out),
                Tab::Recent => recent(ui, h, &mut out),
                Tab::Settings => settings(ui, h, &mut out),
            });
        });
    out
}

fn this_computer(ui: &mut egui::Ui, h: &Home, out: &mut Vec<Request>) {
    ui.label(
        RichText::new("This computer")
            .font(FontId::proportional(24.0))
            .strong(),
    );
    ui.add_space(6.0);
    let Some(s) = &h.this else {
        if !h.can_host_here {
            ui.label("Connections to this computer are off: the Rconn host isn't installed here.");
            return;
        }
        ui.label("Connections to this computer are off.");
        ui.add_space(12.0);
        let ready = h.signed_in && h.busy.is_none();
        let button = egui::Button::new(
            RichText::new("Allow connections to this computer").color(Color32::WHITE),
        )
        .fill(ACCENT);
        if ui.add_enabled(ready, button).clicked() {
            out.push(Request::EnableHost);
        }
        ui.add_space(6.0);
        ui.label(
            RichText::new(if h.signed_in && cfg!(target_os = "macos") {
                "Gives this Mac an ID on your server. macOS will then ask you to allow Screen Recording and Accessibility."
            } else if h.signed_in {
                "Gives this computer an ID on your server and starts the host when you log in."
            } else {
                "Sign in first (⚙ Settings)."
            })
            .small()
            .color(Color32::GRAY),
        );
        return;
    };
    ui.label("Others can connect to this computer with this ID.");
    ui.add_space(14.0);
    ui.label(RichText::new("ID").color(Color32::GRAY));
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(&s.short_code)
                .font(FontId::monospace(30.0))
                .strong(),
        );
        if ui.small_button("📋").on_hover_text("Copy ID").clicked() {
            ui.ctx().copy_text(s.short_code.clone());
        }
    });
    ui.add_space(10.0);
    ui.label(RichText::new("Name").color(Color32::GRAY));
    ui.label(RichText::new(&s.label).size(16.0));
    ui.add_space(10.0);
    let fresh = s.is_fresh(h.now);
    let (dot, text) = if !fresh {
        (Color32::GRAY, "Offline: host service not running")
    } else if !s.missing_permissions.is_empty() {
        (BAD, "Waiting for macOS permissions")
    } else if s.online {
        (GOOD, "Online, ready for connections")
    } else {
        (BAD, "Can't reach the server")
    };
    ui.horizontal(|ui| {
        status_dot(ui, dot);
        ui.label(text);
    });
    if s.session_active && fresh {
        ui.add_space(8.0);
        ui.label(
            RichText::new("🔴 Someone is connected now")
                .color(BAD)
                .strong(),
        );
    }
    if fresh && !s.missing_permissions.is_empty() {
        ui.add_space(10.0);
        ui.label(RichText::new("macOS needs your OK:").strong());
        for p in &s.missing_permissions {
            if ui
                .button(format!("Allow {p}…"))
                .on_hover_text("Opens System Settings; switch Rconn on")
                .clicked()
            {
                out.push(Request::OpenPermission(p.clone()));
            }
        }
        ui.label(
            RichText::new("Then restart the host so macOS applies it:")
                .small()
                .color(Color32::GRAY),
        );
        if ui.button("🔄 Restart host").clicked() {
            out.push(Request::RestartHostAgent);
        }
    }
    if h.can_host_here {
        ui.add_space(16.0);
        if ui.small_button("Turn off connections").clicked() {
            out.push(Request::DisableHost);
        }
    }
}

fn connect_card(ui: &mut egui::Ui, h: &mut Home, out: &mut Vec<Request>) {
    egui::Frame::group(ui.style())
        .corner_radius(10.0)
        .inner_margin(18.0)
        .show(ui, |ui| {
            ui.set_max_width(560.0);
            ui.label(
                RichText::new("Connect to a computer")
                    .font(FontId::proportional(22.0))
                    .strong(),
            );
            ui.add_space(8.0);
            let edit = ui.add(
                egui::TextEdit::singleline(&mut h.connect_to)
                    .hint_text("Enter computer ID")
                    .font(FontId::proportional(22.0))
                    .desired_width(f32::INFINITY),
            );
            let mut enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.add_space(6.0);
            let pw = ui.add(
                egui::TextEdit::singleline(&mut h.access_password)
                    .password(true)
                    .hint_text("Access password (no account needed)")
                    .desired_width(f32::INFINITY),
            );
            enter |= pw.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let guest = !h.access_password.is_empty();
                let ready =
                    (h.signed_in || guest) && !h.connect_to.trim().is_empty() && h.busy.is_none();
                let button = egui::Button::new(
                    RichText::new("  Connect  ")
                        .size(18.0)
                        .color(Color32::WHITE),
                )
                .fill(ACCENT);
                if (ui.add_enabled(ready, button).clicked() || (enter && ready))
                    && !h.connect_to.trim().is_empty()
                {
                    let id = h.connect_to.trim().to_owned();
                    out.push(if guest {
                        Request::ConnectGuest {
                            id,
                            password: Secret(std::mem::take(&mut h.access_password)),
                        }
                    } else {
                        Request::ConnectTyped(id)
                    });
                }
                if !h.signed_in && !guest {
                    ui.label(
                        RichText::new(
                            "Enter the computer's access password, or sign in (⚙ Settings)",
                        )
                        .color(Color32::GRAY),
                    );
                }
            });
        });
}

fn tabs(ui: &mut egui::Ui, h: &mut Home, out: &mut Vec<Request>) {
    ui.horizontal(|ui| {
        for (tab, icon, name) in [
            (Tab::Computers, "🖥", "Computers"),
            (Tab::Recent, "🕘", "Recent"),
            (Tab::Settings, "⚙", "Settings"),
        ] {
            let text = RichText::new(format!("{icon}  {name}")).size(17.0);
            if ui.selectable_label(h.tab == tab, text).clicked() {
                h.tab = tab;
                if tab == Tab::Computers && h.signed_in {
                    out.push(Request::RefreshComputers);
                }
            }
            ui.add_space(8.0);
        }
        if h.tab == Tab::Computers && h.signed_in {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(RichText::new("🔄").size(17.0))
                    .on_hover_text("Refresh")
                    .clicked()
                {
                    out.push(Request::RefreshComputers);
                }
            });
        }
    });
}

fn computer_card(
    ui: &mut egui::Ui,
    host_id: HostId,
    label: Option<&str>,
    online: Option<bool>,
    enabled: bool,
) -> bool {
    let mut clicked = false;
    egui::Frame::group(ui.style())
        .corner_radius(10.0)
        .inner_margin(12.0)
        .show(ui, |ui| {
            ui.set_width(CARD_W);
            ui.horizontal(|ui| {
                ui.label(RichText::new("🖥").size(28.0));
                ui.vertical(|ui| {
                    ui.label(RichText::new(label.unwrap_or("Unnamed computer")).strong());
                    ui.label(
                        RichText::new(host_id.short_code())
                            .monospace()
                            .color(Color32::GRAY),
                    );
                    if let Some(online) = online {
                        let (c, t) = if online {
                            (GOOD, "online")
                        } else {
                            (Color32::GRAY, "offline")
                        };
                        ui.horizontal(|ui| {
                            status_dot(ui, c);
                            ui.label(RichText::new(t).color(c).small());
                        });
                    }
                });
            });
            ui.add_space(6.0);
            clicked = ui
                .add_enabled(enabled, egui::Button::new("Connect"))
                .clicked();
        });
    clicked
}

fn empty(ui: &mut egui::Ui, icon: &str, text: &str) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(icon).size(44.0).color(Color32::GRAY));
        ui.add_space(6.0);
        ui.label(RichText::new(text).size(16.0).color(Color32::GRAY));
    });
}

fn computers(ui: &mut egui::Ui, h: &mut Home, out: &mut Vec<Request>) {
    if !h.signed_in {
        return empty(ui, "🔑", "Sign in to see your computers (⚙ Settings).");
    }
    let Some(list) = &h.computers else {
        return empty(ui, "⏳", "Loading your computers…");
    };
    if list.is_empty() {
        return empty(
            ui,
            "🖥",
            "No computers online. Install the Rconn host on a computer to add it.",
        );
    }
    let enabled = h.busy.is_none();
    let cards: Vec<_> = list
        .iter()
        .map(|c| (c.host_id, c.label.clone(), Some(true)))
        .collect();
    if let Some((id, label)) = card_grid(ui, &cards, enabled) {
        out.push(Request::Connect(id, label));
    }
}

fn recent(ui: &mut egui::Ui, h: &mut Home, out: &mut Vec<Request>) {
    if h.profile.recent.is_empty() {
        return empty(ui, "🕘", "No recent sessions yet.");
    }
    let online: Option<Vec<HostId>> = h
        .computers
        .as_ref()
        .map(|l| l.iter().map(|c| c.host_id).collect());
    let enabled = h.busy.is_none() && h.signed_in;
    let cards: Vec<_> = h
        .profile
        .recent
        .iter()
        .map(|r| {
            (
                r.host_id,
                r.label.clone(),
                online.as_ref().map(|o| o.contains(&r.host_id)),
            )
        })
        .collect();
    if let Some((id, label)) = card_grid(ui, &cards, enabled) {
        out.push(Request::Connect(id, label));
    }
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(12.0);
    ui.label(RichText::new(title).size(18.0).strong());
    ui.add_space(4.0);
}

fn settings(ui: &mut egui::Ui, h: &mut Home, out: &mut Vec<Request>) {
    section(ui, "🌐  Server");
    egui::Grid::new("server")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            let label = ui.label("Server URL").id;
            ui.add(
                egui::TextEdit::singleline(&mut h.server_edit)
                    .hint_text("wss://server:443/v1/ws")
                    .desired_width(360.0),
            )
            .labelled_by(label);
            ui.end_row();
            let label = ui.label("Server pin").id;
            ui.add(
                egui::TextEdit::singleline(&mut h.fingerprint_edit)
                    .hint_text("sha256:… (empty for a public certificate)")
                    .desired_width(360.0),
            )
            .labelled_by(label);
            ui.end_row();
        });
    let changed = h.server_edit.trim() != h.profile.server
        || h.fingerprint_edit.trim() != h.profile.fingerprint;
    if ui
        .add_enabled(changed, egui::Button::new("Save server"))
        .clicked()
    {
        out.push(Request::SaveServer {
            server: h.server_edit.trim().to_owned(),
            fingerprint: h.fingerprint_edit.trim().to_owned(),
        });
    }

    section(ui, "👤  Account");
    if h.signed_in {
        ui.horizontal(|ui| {
            ui.label(format!(
                "Signed in as {}",
                if h.profile.username.is_empty() {
                    "SSO user"
                } else {
                    &h.profile.username
                }
            ));
            if ui.button("Sign out").clicked() {
                out.push(Request::SignOut);
            }
        });
    } else if h.profile.server.is_empty() {
        ui.label(RichText::new("Enter and save the server first.").color(Color32::GRAY));
    } else {
        egui::Grid::new("login")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                let label = ui.label("Username").id;
                ui.add(egui::TextEdit::singleline(&mut h.username).desired_width(240.0))
                    .labelled_by(label);
                ui.end_row();
                let label = ui.label("Password").id;
                let pw = ui
                    .add(
                        egui::TextEdit::singleline(&mut h.password)
                            .password(true)
                            .desired_width(240.0),
                    )
                    .labelled_by(label);
                ui.end_row();
                if pw.lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    && !h.username.is_empty()
                {
                    out.push(Request::SignIn {
                        username: h.username.clone(),
                        password: std::mem::take(&mut h.password),
                    });
                }
            });
        ui.horizontal(|ui| {
            let ready = !h.username.trim().is_empty() && !h.password.is_empty() && h.busy.is_none();
            if ui
                .add_enabled(ready, egui::Button::new("Sign in"))
                .clicked()
            {
                out.push(Request::SignIn {
                    username: h.username.trim().to_owned(),
                    password: std::mem::take(&mut h.password),
                });
            }
            ui.label("or");
            if ui
                .add_enabled(h.busy.is_none(), egui::Button::new("🔐 Sign in with SSO"))
                .clicked()
            {
                out.push(Request::SignInSso);
            }
        });
    }

    section(ui, "🖥  This computer (host)");
    access_password_block(ui, h, out);
    match &mut h.host_settings {
        None => {
            ui.label(
                "The Rconn host isn't installed here, so nobody can connect to this computer.",
            );
        }
        Some(model) => {
            let actions = rconn_host_settings::ui::draw(ui, model);
            if actions.relaunch_elevated {
                out.push(Request::OpenHostSettings);
            }
            if actions.restart_agent {
                out.push(Request::RestartHostAgent);
            }
        }
    }
}

/// "Access password": lets anyone with this computer's ID and the password connect.
fn access_password_block(ui: &mut egui::Ui, h: &mut Home, out: &mut Vec<Request>) {
    ui.label(RichText::new("Access password").strong());
    let Some(this) = &h.this else {
        ui.label(RichText::new("Turn on connections to this computer first.").color(Color32::GRAY));
        ui.add_space(8.0);
        return;
    };
    // Just saved: trust that for a minute, until the host's status reports it.
    let on = match h.host_password_saved {
        Some((on, at)) if h.now.saturating_sub(at) < 60 => on,
        _ => this.access_password,
    };
    ui.label(if on {
        format!(
            "On: anyone with this computer's ID ({}) and the password can connect.",
            this.short_code
        )
    } else {
        "Off: only signed-in accounts can connect.".into()
    });
    ui.horizontal(|ui| {
        let label = ui.label(if on { "New password" } else { "Password" });
        ui.add(
            egui::TextEdit::singleline(&mut h.new_host_password)
                .password(true)
                .hint_text("8 or more characters")
                .desired_width(240.0),
        )
        .labelled_by(label.id);
        let ok = h.new_host_password.chars().count() >= rconn_core::guest::MIN_PASSWORD_LEN
            && h.busy.is_none();
        if ui
            .add_enabled(ok, egui::Button::new("Set password"))
            .clicked()
        {
            out.push(Request::SetAccessPassword(Secret(std::mem::take(
                &mut h.new_host_password,
            ))));
        }
        if on
            && ui
                .add_enabled(h.busy.is_none(), egui::Button::new("Turn off"))
                .clicked()
        {
            out.push(Request::ClearAccessPassword);
        }
    });
    if cfg!(windows) {
        ui.label(
            RichText::new("Windows asks for administrator approval to change it.")
                .small()
                .color(Color32::GRAY),
        );
    }
    ui.add_space(8.0);
}

fn status_bar(ui: &mut egui::Ui, h: &Home) {
    ui.horizontal(|ui| {
        if let Some(b) = &h.busy {
            ui.spinner();
            ui.label(b.as_str());
        } else if let Some((text, err)) = &h.notice {
            status_dot(ui, if *err { BAD } else { GOOD });
            ui.label(text.as_str());
        } else if h.signed_in {
            status_dot(ui, GOOD);
            ui.label(format!("Ready. Server: {}", h.profile.server));
        } else {
            status_dot(ui, Color32::GRAY);
            ui.label("Not signed in. Open ⚙ Settings to sign in.");
        }
    });
}

/// A small filled circle (status light), drawn rather than typed so no font is needed.
fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
}

const CARD_W: f32 = 210.0;

/// Cards in even rows; returns the one whose Connect was clicked.
fn card_grid(
    ui: &mut egui::Ui,
    cards: &[(HostId, Option<String>, Option<bool>)],
    enabled: bool,
) -> Option<(HostId, Option<String>)> {
    let per_row = ((ui.available_width() + 12.0) / (CARD_W + 12.0 + 26.0))
        .floor()
        .max(1.0) as usize;
    let mut clicked = None;
    ui.add_space(8.0);
    for row in cards.chunks(per_row) {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            for (id, label, online) in row {
                if computer_card(ui, *id, label.as_deref(), *online, enabled) {
                    clicked = Some((*id, label.clone()));
                }
            }
        });
        ui.add_space(12.0);
    }
    clicked
}
