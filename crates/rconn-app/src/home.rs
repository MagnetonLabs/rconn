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
        /// Keep the password in the system's credential store once it's let in.
        remember: bool,
    },
    /// A recent computer whose password is remembered: connect with it.
    ConnectSaved(String),
    /// Forget the remembered password of this ID ("123 456 789").
    ForgetPassword(String),
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
    /// Take this ID ("123 456 789") off the recent list.
    ForgetRecent(String),
    /// Name a recent computer here (empty: back to the computer's own name).
    RenameRecent {
        code: String,
        label: String,
    },
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
    /// Recent IDs (digits only) the server last said were online or not; no account needed.
    /// Empty when the server hasn't answered (or is too old to know the question).
    pub presence: std::collections::HashMap<String, bool>,
    /// A recent computer being renamed: (its ID, the name being typed).
    pub renaming: Option<(String, String)>,
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
    /// "Remember the password" is ticked under the password field.
    pub remember_password: bool,
}

impl Home {
    pub fn new(profile: Profile) -> Self {
        Self {
            remember_password: profile.remember_passwords,
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
            presence: Default::default(),
            renaming: None,
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
    // The ID line (30 pt digits + copy button, 236 px) is the widest thing on the left.
    egui::Panel::left("this_computer")
        .exact_size(270.0)
        .resizable(false)
        .frame(
            egui::Frame::new()
                .fill(ui.visuals().faint_bg_color)
                .inner_margin(MARGIN),
        )
        .show(ui, |ui| this_computer(ui, h, &mut out));
    egui::Panel::bottom("status")
        .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(16, 8)))
        .show(ui, |ui| status_bar(ui, h));
    egui::CentralPanel::default()
        .frame(egui::Frame::new().inner_margin(MARGIN))
        .show(ui, |ui| {
            connect_card(ui, h, &mut out);
            ui.add_space(GAP);
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
        if let Some(step) = h.host_setup.clone() {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(step.as_str());
            });
            // No need to wait for the ID to choose the password.
            ui.add_space(12.0);
            access_password_block(ui, h, None, out);
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
            // Getting the ID failed (offline?): the password can still be set meanwhile,
            // and saving it tries again.
            ui.add_space(12.0);
            access_password_block(ui, h, None, out);
        }
        return;
    };
    ui.label("Your ID");
    ui.label(
        RichText::new(if access_password_on(h, Some(&s)) {
            "Give it, with the access password, to whoever should connect."
        } else {
            "Give it to whoever should connect; you accept each connection here."
        })
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
    access_password_block(ui, h, Some(&s), out);
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
            RichText::new("The host restarts by itself once you allow it.")
                .small()
                .color(Color32::GRAY),
        );
        if ui
            .small_button("🔄 Restart host")
            .on_hover_text("Only if it hasn't picked up the permission after a few seconds")
            .clicked()
        {
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
        .inner_margin(MARGIN)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new("Connect to a computer")
                    .font(FontId::proportional(20.0))
                    .strong(),
            );
            ui.add_space(8.0);
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
            // Wide enough: ID, password and Connect on one line; otherwise the ID above.
            let wide = ui.available_width() >= 520.0;
            let mut enter = false;
            let mut height = 0.0;
            ui.horizontal(|ui| {
                let width = if wide {
                    (ui.available_width() - CONNECT_W - 16.0) * 0.42
                } else {
                    f32::INFINITY
                };
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut h.connect_to)
                        .hint_text("Computer ID (123 456 789)")
                        .font(FontId::proportional(18.0))
                        .desired_width(width),
                );
                enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                height = edit.rect.height();
                if wide {
                    password_and_connect(ui, h, out, enter, height);
                }
            });
            if !wide {
                ui.horizontal(|ui| password_and_connect(ui, h, out, enter, height));
            }
            if !h.access_password.is_empty() {
                ui.checkbox(&mut h.remember_password, "Remember this password")
                    .on_hover_text(
                        "Kept in this computer's password store (Keychain, Credential \
                         Manager, Secret Service), so next time a click on its card connects",
                    );
            }
            if !h.signed_in {
                ui.label(
                    RichText::new(if h.access_password.is_empty() {
                        "No account needed. Without a password, the person there accepts."
                    } else {
                        "No account needed: the ID and its access password."
                    })
                    .color(Color32::GRAY),
                );
            }
        });
}

/// The password field (the rest of the line) and the Connect button, `height` tall like the
/// ID field. `enter`: Enter was pressed in the ID field.
fn password_and_connect(
    ui: &mut egui::Ui,
    h: &mut Home,
    out: &mut Vec<Request>,
    mut enter: bool,
    height: f32,
) {
    let pw = ui.add(
        egui::TextEdit::singleline(&mut h.access_password)
            .password(true)
            .hint_text("Its access password (if it has one)")
            .font(FontId::proportional(18.0))
            .desired_width(ui.available_width() - CONNECT_W - ui.spacing().item_spacing.x),
    );
    if std::mem::take(&mut h.focus_password) {
        pw.request_focus();
    }
    enter |= pw.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    // Signed in, a bare ID is looked up in the fleet; otherwise it connects as a
    // guest: with the password, or by asking the person at the computer.
    let guest = !h.access_password.is_empty() || !h.signed_in;
    let ready = !h.connect_to.trim().is_empty() && h.busy.is_none();
    let button = egui::Button::new(
        RichText::new("  Connect  ")
            .size(18.0)
            .color(Color32::WHITE),
    )
    .fill(ACCENT)
    .min_size(egui::vec2(CONNECT_W, height));
    if (ui.add_enabled(ready, button).clicked() || (enter && ready))
        && !h.connect_to.trim().is_empty()
    {
        let id = h.connect_to.trim().to_owned();
        out.push(if guest {
            Request::ConnectGuest {
                id,
                password: Secret(std::mem::take(&mut h.access_password)),
                remember: h.remember_password,
            }
        } else {
            Request::ConnectTyped(id)
        });
    }
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

/// What was clicked on a computer card.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CardClick {
    Connect,
    ForgetPassword,
    Remove,
    Rename,
    SaveName,
    CancelName,
}

/// One computer, `width` wide in all: its name (one line, the full one on hover), its ID
/// and online state, then Connect on the left and Rename / Remove on the right. Every card
/// has the same height, so the rows line up.
#[allow(clippy::too_many_arguments)]
fn computer_card(
    ui: &mut egui::Ui,
    width: f32,
    code: &str,
    label: Option<&str>,
    online: Option<bool>,
    saved: bool,
    enabled: bool,
    editable: bool,
    renaming: Option<&mut String>,
) -> Option<CardClick> {
    let is_renaming = renaming.is_some();
    let mut clicked = None;
    let frame = egui::Frame::group(ui.style())
        .corner_radius(10.0)
        .inner_margin(12.0);
    let inner = width - frame.total_margin().sum().x;
    frame.show(ui, |ui| {
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 4.0);
        ui.vertical(|ui| {
            ui.set_width(inner);
            ui.horizontal(|ui| {
                ui.label(RichText::new("🖥").size(26.0));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    // A text field is a little taller than a label: keep the height either way.
                    ui.horizontal(|ui| {
                        ui.set_min_height(NAME_ROW_H);
                        match renaming {
                            Some(text) => {
                                let r = ui.add(
                                    egui::TextEdit::singleline(text)
                                        .hint_text("Name")
                                        .char_limit(rconn_core::protocol::MAX_HOST_NAME_CHARS)
                                        .desired_width(f32::INFINITY),
                                );
                                if !r.has_focus() && !r.lost_focus() {
                                    r.request_focus();
                                }
                                if r.lost_focus() {
                                    clicked =
                                        Some(if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                                            CardClick::CancelName
                                        } else {
                                            CardClick::SaveName
                                        });
                                }
                            }
                            None => {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(label.unwrap_or("Unnamed computer"))
                                            .size(15.0)
                                            .strong(),
                                    )
                                    .truncate(),
                                );
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(code).monospace().color(Color32::GRAY));
                        if let Some(online) = online {
                            let (c, t) = if online {
                                (GOOD, "online")
                            } else {
                                (Color32::GRAY, "offline")
                            };
                            ui.add_space(4.0);
                            status_dot(ui, c);
                            ui.label(RichText::new(t).color(c).size(12.0));
                        }
                        if saved
                            && ui
                                .add(egui::Button::new(RichText::new("🔑").size(12.0)).frame(false))
                                .on_hover_text("Password remembered: click to forget it")
                                .clicked()
                        {
                            clicked = Some(CardClick::ForgetPassword);
                        }
                    });
                });
            });
            ui.add_space(8.0);
            let link = |ui: &mut egui::Ui, text: &str, hover: &str| {
                ui.add(egui::Button::new(RichText::new(text).size(13.0)).frame(false))
                    .on_hover_text(hover)
                    .clicked()
            };
            ui.horizontal(|ui| {
                if is_renaming {
                    if ui.button("Save").clicked() {
                        clicked = Some(CardClick::SaveName);
                    }
                    if link(ui, "Cancel", "Keep the current name") {
                        clicked = Some(CardClick::CancelName);
                    }
                    return;
                }
                if ui
                    .add_enabled(enabled, egui::Button::new("Connect"))
                    .clicked()
                {
                    clicked = Some(CardClick::Connect);
                }
                if editable {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if link(ui, "Remove", "Take it off this list") {
                            clicked = Some(CardClick::Remove);
                        }
                        if link(ui, "Rename", "Give it a name on this computer") {
                            clicked = Some(CardClick::Rename);
                        }
                    });
                }
            });
        });
    });
    clicked
}

fn empty(ui: &mut egui::Ui, icon: &str, text: &str) {
    ui.add_space(24.0);
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
        .map(|c| (c.host_id.short_code(), c.label.clone(), Some(true), false))
        .collect();
    if let Some((i, CardClick::Connect)) = card_grid(ui, &cards, enabled, false, None) {
        out.push(Request::Connect(list[i].host_id, list[i].label.clone()));
    }
}

fn recent(ui: &mut egui::Ui, h: &mut Home, out: &mut Vec<Request>) {
    if h.profile.recent.is_empty() {
        return empty(
            ui,
            "🕘",
            "Computers you connect to appear here. Enter an ID above to connect.",
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
            let in_account = match (&online, r.host_id) {
                (Some(o), Some(id)) if h.signed_in => o.contains(&id),
                _ => false,
            };
            let code = r.code();
            let seen = h.presence.get(&code.replace(' ', "")).copied();
            let state = if in_account { Some(true) } else { seen };
            (code, r.label.clone(), state, in_account, r.password_saved)
        })
        .collect();
    let shown: Vec<_> = cards
        .iter()
        .map(|(c, l, o, _, saved)| (c.clone(), l.clone(), *o, *saved))
        .collect();
    let Some((i, click)) = card_grid(ui, &shown, enabled, true, h.renaming.as_mut()) else {
        return;
    };
    let code = cards[i].0.clone();
    match click {
        CardClick::Remove => return out.push(Request::ForgetRecent(code)),
        CardClick::ForgetPassword => return out.push(Request::ForgetPassword(code)),
        CardClick::Rename => {
            h.renaming = Some((code, cards[i].1.clone().unwrap_or_default()));
            return;
        }
        CardClick::SaveName => {
            if let Some((code, label)) = h.renaming.take() {
                out.push(Request::RenameRecent { code, label });
            }
            return;
        }
        CardClick::CancelName => {
            h.renaming = None;
            return;
        }
        CardClick::Connect => {}
    }
    {
        let r = h.profile.recent[i].clone();
        let in_list = cards[i].3;
        match r.host_id {
            // One of the signed-in fleet's computers: no password needed.
            Some(id) if in_list => out.push(Request::Connect(id, r.label)),
            // Its password is remembered: connect with it.
            _ if r.password_saved => out.push(Request::ConnectSaved(r.code())),
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
    ui.add_space(MARGIN);
    ui.label(RichText::new(title).size(18.0).strong());
    ui.add_space(4.0);
}

fn settings(ui: &mut egui::Ui, h: &mut Home, out: &mut Vec<Request>) {
    section(ui, "🌐  Server");
    let own = rconn_host_settings::ui::self_hosted(&h.profile.server, &h.profile.fingerprint);
    ui.label(if own {
        format!("Your own server: {}", h.profile.server)
    } else {
        format!("The public Rconn server ({}).", rconn_core::DEFAULT_SERVER)
    });
    egui::CollapsingHeader::new("Advanced: self-hosted server")
        .id_salt("self_hosted_server")
        .default_open(own)
        .show(ui, |ui| {
            rconn_host_settings::ui::server_fields(
                ui,
                "server",
                &mut h.server_edit,
                &mut h.fingerprint_edit,
                [None, None],
            );
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
        });

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

    if h.this.is_some() || h.can_host_here {
        section(ui, "🔑  This computer's access password");
        let this = h.this.clone();
        ui.scope(|ui| {
            ui.set_max_width(420.0);
            access_password_block(ui, h, this.as_ref(), out);
        });
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
/// file reports it. No status yet, e.g. while the ID is being fetched: the settings file.)
pub fn access_password_on(h: &Home, this: Option<&HostStatus>) -> bool {
    match (h.host_password_saved, this) {
        (Some((on, at)), _) if h.now.saturating_sub(at) < 60 => on,
        (_, Some(s)) => s.access_password,
        (Some((on, _)), None) => on,
        (None, None) => h
            .host_settings
            .as_ref()
            .and_then(|m| m.saved.as_ref())
            .is_some_and(|f| !f.access_password.is_empty()),
    }
}

/// "Access password": with this computer's ID it is all anyone needs to connect. Without
/// one, the ID alone never lets anyone in: the person at this computer accepts or declines
/// each connection.
fn access_password_block(
    ui: &mut egui::Ui,
    h: &mut Home,
    this: Option<&HostStatus>,
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
            let ask_too = h
                .host_settings
                .as_ref()
                .and_then(|m| m.saved.as_ref())
                .is_some_and(|f| f.ask_with_password);
            ui.label(
                RichText::new(if ask_too {
                    "Connections need it, or your OK here."
                } else {
                    "Connections need it."
                })
                .small()
                .color(Color32::GRAY),
            );
        } else {
            ui.label(
                RichText::new("Set an access password")
                    .strong()
                    .color(ACCENT),
            );
            ui.label(
                RichText::new(
                    "Until you do, every connection needs your OK: you accept or decline it \
                     here, at this computer.",
                )
                .small(),
            );
        }
        if this.is_none() && h.host_setup.is_none() && h.can_host_here {
            ui.label(
                RichText::new("Saving it also turns incoming connections on.")
                    .small()
                    .color(Color32::GRAY),
            );
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
                    .on_hover_text("Then every connection needs your OK here")
                    .clicked()
                {
                    h.changing_password = false;
                    out.push(Request::ClearAccessPassword);
                }
            }
        });
        if cfg!(windows) && !h.can_host_here {
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

/// Panel margins; the space between cards, and between the connect card and the tabs.
const MARGIN: f32 = 16.0;
const GAP: f32 = 12.0;
/// The Connect button next to the ID and password fields.
const CONNECT_W: f32 = 112.0;
/// The narrowest a computer card gets (name, ID + online, Connect / Rename / Remove).
const CARD_MIN_W: f32 = 210.0;
/// A card's name line: a text field's height, so renaming doesn't make the card taller.
const NAME_ROW_H: f32 = 22.0;

/// How many cards fit on a row `available` wide, and how wide each is so they fill it.
pub fn card_columns(available: f32) -> (usize, f32) {
    let per_row = ((available + GAP) / (CARD_MIN_W + GAP)).floor().max(1.0) as usize;
    let width = (available - GAP * (per_row - 1) as f32) / per_row as f32;
    (per_row, width.floor())
}

/// Cards (ID, name, online?) in even rows; returns the index of the one clicked and how.
fn card_grid(
    ui: &mut egui::Ui,
    cards: &[(String, Option<String>, Option<bool>, bool)],
    enabled: bool,
    editable: bool,
    mut renaming: Option<&mut (String, String)>,
) -> Option<(usize, CardClick)> {
    let (per_row, width) = card_columns(ui.available_width());
    let mut clicked = None;
    ui.add_space(GAP - ui.spacing().item_spacing.y);
    ui.spacing_mut().item_spacing = egui::vec2(GAP, GAP);
    for (r, row) in cards.chunks(per_row).enumerate() {
        ui.horizontal_top(|ui| {
            for (i, (code, label, online, saved)) in row.iter().enumerate() {
                let edit = match renaming.as_deref_mut() {
                    Some((c, text)) if c == code => Some(text),
                    _ => None,
                };
                if let Some(c) = computer_card(
                    ui,
                    width,
                    code,
                    label.as_deref(),
                    *online,
                    *saved,
                    enabled,
                    editable,
                    edit,
                ) {
                    clicked = Some((r * per_row + i, c));
                }
            }
        });
    }
    clicked
}
