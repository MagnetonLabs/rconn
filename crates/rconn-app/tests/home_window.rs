//! Main window: ID matching, profile, and an offscreen render of each tab.
//! Set RCONN_APP_PNG_DIR to keep the rendered PNGs.

use egui_kittest::Harness;
use rconn_app::home::{self, Home, Request, Tab};
use rconn_app::profile::Profile;
use rconn_core::ids::HostId;
use rconn_core::protocol::HostInfo;
use rconn_core::status::HostStatus;

fn id(s: &str) -> HostId {
    HostId(uuid::Uuid::parse_str(s).unwrap())
}

fn sample() -> Home {
    let mut p = Profile {
        server: "wss://rconn.example.com/v1/ws".into(),
        username: "admin".into(),
        ..Default::default()
    };
    p.remember(
        id("11111111-2222-4333-8444-555555555555"),
        Some("BUILD-BOX".into()),
    );
    let mut h = Home::new(p);
    let me = id("1e5ede94-60e3-408a-9cc4-b1c92e0ca86b");
    h.now = 1_000;
    h.this = Some(HostStatus {
        host_id: me.to_string(),
        short_code: me.short_code(),
        label: "WIN11-VM".into(),
        server: "wss://rconn.example.com/v1/ws".into(),
        online: true,
        updated: 990,
        ..Default::default()
    });
    h.signed_in = true;
    h.computers = Some(vec![
        HostInfo {
            host_id: me,
            label: Some("WIN11-VM".into()),
        },
        HostInfo {
            host_id: id("11111111-2222-4333-8444-555555555555"),
            label: Some("BUILD-BOX".into()),
        },
        HostInfo {
            host_id: id("99999999-2222-4333-8444-555555555555"),
            label: None,
        },
    ]);
    h
}

#[test]
fn typed_ids_resolve_by_code_or_uuid() {
    let h = sample();
    let list = h.computers.as_ref().unwrap();
    let me = list[0].host_id;
    assert_eq!(
        home::resolve(&me.short_code(), list).map(|c| c.host_id),
        Some(me)
    );
    assert_eq!(
        home::resolve(&me.short_code().replace(' ', ""), list).map(|c| c.host_id),
        Some(me)
    );
    assert_eq!(
        home::resolve(&me.0.to_string(), list).map(|c| c.host_id),
        Some(me)
    );
    assert!(home::resolve("000 000 000", list).is_none());
}

#[test]
fn profile_round_trips_and_recent_is_most_recent_first() {
    let d = tempfile::tempdir().unwrap();
    let mut p = Profile::default();
    for i in 0..20u128 {
        p.remember(HostId(uuid::Uuid::from_u128(i)), None);
    }
    p.remember(HostId(uuid::Uuid::from_u128(3)), Some("again".into()));
    assert_eq!(p.recent.len(), rconn_app::profile::MAX_RECENT);
    assert_eq!(p.recent[0].label.as_deref(), Some("again"));
    p.save(d.path()).unwrap();
    assert_eq!(Profile::load(d.path()), p);
}

#[test]
fn host_fills_in_an_empty_server_profile() {
    let mut h = Home::new(Profile::default());
    h.this = Some(HostStatus {
        server: "wss://s/v1/ws".into(),
        fingerprint: Some("sha256:ab".into()),
        ..Default::default()
    });
    h.adopt_host_server();
    assert_eq!(
        (h.profile.server.as_str(), h.profile.fingerprint.as_str()),
        ("wss://s/v1/ws", "sha256:ab")
    );
}

fn render(name: &str, mut h: Home) -> Vec<Request> {
    let mut requests = Vec::new();
    let mut harness = Harness::builder()
        .with_size(egui::vec2(980.0, 640.0))
        .build_ui(|ui| {
            requests.extend(home::draw(ui, &mut h));
        });
    harness.run();
    let image = harness.render().expect("offscreen render");
    let dir = std::env::var_os("RCONN_APP_PNG_DIR").map(std::path::PathBuf::from);
    if let Some(dir) = dir {
        image.save(dir.join(format!("{name}.png"))).unwrap();
    }
    drop(harness);
    requests
}

#[test]
fn renders_each_tab() {
    render("home-computers", sample());
    let mut recent = sample();
    recent.tab = Tab::Recent;
    render("home-recent", recent);
    let mut signed_out = sample();
    signed_out.signed_in = false;
    signed_out.computers = None;
    signed_out.tab = Tab::Settings;
    render("home-settings-signed-out", signed_out);
    let mut no_host = Home::new(Profile::default());
    no_host.tab = Tab::Settings;
    render("home-not-a-host", no_host);
}

/// The session window's toolbar labels (rconn-viewer) render with the bundled fonts.
#[test]
fn session_toolbar_glyphs_render() {
    let mut harness = Harness::builder()
        .with_size(egui::vec2(600.0, 60.0))
        .build_ui(|ui| {
            ui.horizontal(|ui| {
                for t in [
                    "🔳 Full screen",
                    "🔄 Refresh picture",
                    "❌ Disconnect",
                    "1496x938 · 45 fps",
                ] {
                    let _ = ui.button(t);
                }
            });
        });
    harness.run();
    harness.render().expect("render");
}
