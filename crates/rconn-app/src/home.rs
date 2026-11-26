//! The main window, like AnyDesk: this computer's ID and access password on the left,
//! "connect to: ID + password" on the right, with recent computers below. No account is
//! needed for either side; signing in (Settings) adds a self-hosted fleet's computer list.

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
    /// macOS and Linux: make this computer a host (an ID from the server, no account
    /// needed; the first launch does this by itself).
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
    /// Setting this computer up as a host right now (e.g. "Getting an ID…").
    pub host_setup: Option<String>,
    /// Why turning on incoming connections failed last time.
    pub host_error: Option<String>,
    /// "Change" was clicked next to a set access password.
    pub changing_password: bool,
    /// Move the keyboard focus to the access-password field (after picking a recent ID).
    pub focus_password: bool,
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
            tab: Tab::Recent,
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
            host_setup: None,
            host_error: None,
            changing_password: false,
            focus_password: false,
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
        .exact_size(290.0)
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

fn this_computer(ui: &mut egui::Ui, h: &mut Home, out: &mut Vec<Request>) {
    ui.label(
        RichText::new("This computer")
            .font(FontId::proportional(24.0))
            .strong(),
    );
    ui.add_space(6.0);
    let Some(s) = h.this.clone() else {
        if let Some(step) = &h.host_setup {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(step.as_str());
            });
            return;
        }
        if !h.can_host_here {
            ui.label("Incoming connections are off: the Rconn host isn't installed here.");
            return;
        }
        ui.label("Incoming connections are off.");
        ui.add_space(12.0);
        let button =
            egui::Button::new(RichText::new("Turn on incoming connections").color(Color32::WHITE))
                .fill(ACCENT);
        if ui.add(button).clicked() {
            out.push(Request::EnableHost);
        }
        ui.add_space(6.0);
        ui.label(
            RichText::new(if cfg!(target_os = "macos") {
                "Gives this Mac an ID (no account needed). macOS then asks you to allow Screen Recording and Accessibility."
            } else {
                "Gives this computer an ID (no account needed) and starts the host when you log in."
            })
            .small()
            .color(Color32::GRAY),
        );
        if let Some(e) = &h.host_error {
            ui.add_space(8.0);
            ui.label(RichText::new(e.as_str()).color(BAD).small());
        }
        return;
    };
    ui.label("Your ID");
    ui.label(
        RichText::new("Give it, with the access password, to whoever should connect.")
            .small()
            .color(Color32::GRAY),
    );
    ui.add_space(6.0);
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
    ui.label(RichText::new(&s.label).color(Color32::GRAY));
    ui.add_space(10.0);
    access_password_block(ui, h, &s, out);
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
        if ui.small_button("Turn off incoming connections").clicked() {
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
                    .hint_text("Computer ID (123 456 789)")
                    .font(FontId::proportional(22.0))
                    .desired_width(f32::INFINITY),
            );
            let mut enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.add_space(6.0);
            let pw = ui.add(
                egui::TextEdit::singleline(&mut h.access_password)
                    .password(true)
                    .hint_text("Its access password")
                    .font(FontId::proportional(18.0))
                    .desired_width(f32::INFINITY),
            );
            if std::mem::take(&mut h.focus_password) {
                pw.request_focus();
            }
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
                        RichText::new("No account needed: the ID and its access password.")
                            .color(Color32::GRAY),
                    );
                }
            });
        });
}

fn tabs(ui: &mut egui::Ui, h: &mut Home, out: &mut Vec<Request>) {
    ui.horizontal(|ui| {
        // "Computers" is a signed-in fleet's list; without an account there is none.
        if !h.signed_in && h.tab == Tab::Computers {
            h.tab = Tab::Recent;
        }
        let tabs = [
            (Tab::Recent, "🕘", "Recent"),
            (Tab::Computers, "🖥", "Computers"),
            (Tab::Settings, "⚙", "Settings"),
        ];
        for (tab, icon, name) in tabs
            .into_iter()
            .filter(|(t, _, _)| *t != Tab::Computers || h.signed_in)
        {
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
    code: &str,
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
                    ui.label(RichText::new(code).monospace().color(Color32::GRAY));
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
        .map(|c| (c.host_id.short_code(), c.label.clone(), Some(true)))
        .collect();
    if let Some(i) = card_grid(ui, &cards, enabled) {
        out.push(Request::Connect(list[i].host_id, list[i].label.clone()));
    }
}

fn recent(ui: &mut egui::Ui, h: &mut Home, out: &mut Vec<Request>) {
    if h.profile.recent.is_empty() {
        return empty(
            ui,
            "🕘",
            "Computers you connect to appear here. Enter an ID and its password above.",
        );
    }
    let online: Option<Vec<HostId>> = h
        .computers
        .as_ref()
        .map(|l| l.iter().map(|c| c.host_id).collect());
    let enabled = h.busy.is_none();
    let cards: Vec<_> = h
        .profile
        .recent
        .iter()
        .map(|r| {
            let listed = match (&online, r.host_id) {
                (Some(o), Some(id)) if h.signed_in => Some(o.contains(&id)),
                _ => None,
            };
            (r.code(), r.label.clone(), listed)
        })
        .collect();
    if let Some(i) = card_grid(ui, &cards, enabled) {
        let r = h.profile.recent[i].clone();
        let in_list = cards[i].2 == Some(true);
        match r.host_id {
            // One of the signed-in fleet's computers: no password needed.
            Some(id) if in_list => out.push(Request::Connect(id, r.label)),
            // Otherwise it takes the access password: fill in the ID, ask for it.
            _ => {
                h.connect_to = r.code();
                h.access_password.clear();
                h.focus_password = true;
            }
        }
    }
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(12.0);
    ui.label(RichText::new(title).size(18.0).strong());
    ui.add_space(4.0);
}

fn settings(ui: &mut egui::Ui, h: &mut Home, out: &mut Vec<Request>) {
    section(ui, "🌐  Server");
    ui.label(
        RichText::new(format!(
            "Empty: the public Rconn server ({}). Self-hosted: your server's URL and pin.",
            rconn_core::DEFAULT_SERVER
        ))
        .small()
        .color(Color32::GRAY),
    );
    egui::Grid::new("server")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            let label = ui.label("Server URL").id;
            ui.add(
                egui::TextEdit::singleline(&mut h.server_edit)
                    .hint_text(rconn_core::DEFAULT_SERVER)
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

    section(ui, "👤  Account (optional)");
    ui.label(
        RichText::new(
            "Not needed to connect or to be connected: the ID and access password are enough. \
             Sign in only to see a self-hosted fleet's computers.",
        )
        .small()
        .color(Color32::GRAY),
    );
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
    } else {
        egui::CollapsingHeader::new("Sign in to a self-hosted fleet")
            .id_salt("sign_in")
            .show(ui, |ui| sign_in_form(ui, h, out));
    }

    section(ui, "🖥  This computer (host)");
    match &mut h.host_settings {
        None => {
            ui.label("The Rconn host isn't running here, so nobody can connect to this computer.");
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

fn sign_in_form(ui: &mut egui::Ui, h: &mut Home, out: &mut Vec<Request>) {
    {
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
}

/// Is an access password set? (Just saved: trusted for a minute, until the host's status
/// file reports it.)
pub fn access_password_on(h: &Home, this: &HostStatus) -> bool {
    match h.host_password_saved {
        Some((on, at)) if h.now.saturating_sub(at) < 60 => on,
        _ => this.access_password,
    }
}

/// "Access password": with this computer's ID it is all anyone needs to connect, and
/// without one nobody can (the ID alone never lets anyone in).
fn access_password_block(
    ui: &mut egui::Ui,
    h: &mut Home,
    this: &HostStatus,
    out: &mut Vec<Request>,
) {
    let on = access_password_on(h, this);
    let editing = !on || h.changing_password;
    let frame = if on {
        egui::Frame::new()
    } else {
        // Not set yet: the one thing left to do, so make it stand out.
        egui::Frame::group(ui.style())
            .stroke(egui::Stroke::new(1.5, ACCENT))
            .corner_radius(8.0)
            .inner_margin(10.0)
    };
    frame.show(ui, |ui| {
        if on {
            ui.horizontal(|ui| {
                status_dot(ui, GOOD);
                ui.label(RichText::new("Access password set").strong());
                if !h.changing_password && ui.small_button("Change").clicked() {
                    h.changing_password = true;
                }
            });
        } else {
            ui.label(
                RichText::new("Set an access password")
                    .strong()
                    .color(ACCENT),
            );
            ui.label(RichText::new("Nobody can connect to this computer until you do.").small());
        }
        if !editing {
            return;
        }
        ui.add_space(4.0);
        let field = ui.add(
            egui::TextEdit::singleline(&mut h.new_host_password)
                .password(true)
                .hint_text("8 or more characters")
                .desired_width(f32::INFINITY),
        );
        let ok = h.new_host_password.chars().count() >= rconn_core::guest::MIN_PASSWORD_LEN
            && h.busy.is_none();
        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        ui.horizontal(|ui| {
            let set =
                egui::Button::new(RichText::new("Set password").color(Color32::WHITE)).fill(ACCENT);
            if (ui.add_enabled(ok, set).clicked() || (enter && ok))
                && !h.new_host_password.is_empty()
            {
                h.changing_password = false;
                out.push(Request::SetAccessPassword(Secret(std::mem::take(
                    &mut h.new_host_password,
                ))));
            }
            if on {
                if ui.button("Cancel").clicked() {
                    h.changing_password = false;
                    h.new_host_password.clear();
                }
                if ui
                    .add_enabled(h.busy.is_none(), egui::Button::new("Remove"))
                    .on_hover_text("Nobody can connect with the ID alone then")
                    .clicked()
                {
                    h.changing_password = false;
                    out.push(Request::ClearAccessPassword);
                }
            }
        });
        if cfg!(windows) {
            ui.label(
                RichText::new("Windows asks for administrator approval.")
                    .small()
                    .color(Color32::GRAY),
            );
        }
    });
}

fn status_bar(ui: &mut egui::Ui, h: &Home) {
    ui.horizontal(|ui| {
        if let Some(b) = &h.busy {
            ui.spinner();
            ui.label(b.as_str());
        } else if let Some((text, err)) = &h.notice {
            status_dot(ui, if *err { BAD } else { GOOD });
            ui.label(text.as_str());
        } else {
            status_dot(ui, GOOD);
            ui.label(format!("Ready. Server: {}", h.profile.server_url()));
        }
    });
}

/// A small filled circle (status light), drawn rather than typed so no font is needed.
fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
}

const CARD_W: f32 = 210.0;

/// Cards (ID, name, online?) in even rows; returns the index of the one clicked.
fn card_grid(
    ui: &mut egui::Ui,
    cards: &[(String, Option<String>, Option<bool>)],
    enabled: bool,
) -> Option<usize> {
    let per_row = ((ui.available_width() + 12.0) / (CARD_W + 12.0 + 26.0))
        .floor()
        .max(1.0) as usize;
    let mut clicked = None;
    ui.add_space(8.0);
    for (r, row) in cards.chunks(per_row).enumerate() {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 12.0;
            for (i, (code, label, online)) in row.iter().enumerate() {
                if computer_card(ui, code, label.as_deref(), *online, enabled) {
                    clicked = Some(r * per_row + i);
                }
            }
        });
        ui.add_space(12.0);
    }
    clicked
}
