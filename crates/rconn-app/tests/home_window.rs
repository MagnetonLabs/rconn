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
    harness.run_steps(4); // not `run`: a spinner never settles
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
    render("home-first-run", first_run());
    render("home-no-account", no_account(false));
    render("home-no-account-password-set", no_account(true));
}

/// First launch: no server configured, no account, the ID being fetched.
fn first_run() -> Home {
    let mut h = Home::new(Profile::default());
    h.can_host_here = true;
    h.host_setup = Some("Getting an ID for this Mac…".into());
    h
}

/// The first-launch window against the stored image (`UPDATE_SNAPSHOTS=1` rewrites it):
/// the password can be chosen while the ID is still on its way.
#[test]
fn first_launch_looks_as_stored_and_offers_the_password_right_away() {
    use egui_kittest::kittest::Queryable;
    let mut hs = harness(first_run());
    hs.run_steps(4); // a spinner never settles
    assert!(hs.query_by_label("Set an access password").is_some());
    hs.snapshot("first-launch");
    // Typing one and saving it works before the ID is there.
    hs.state_mut().0.new_host_password = "correct horse".into();
    hs.run_steps(2);
    hs.get_by_label("Set password").click();
    hs.run_steps(2);
    assert_eq!(
        hs.state().1,
        vec![Request::SetAccessPassword(home::Secret(
            "correct horse".into()
        ))]
    );
}

#[test]
fn enrollment_failed_still_offers_the_password_and_settings_has_it_too() {
    use egui_kittest::kittest::Queryable;
    let mut failed = Home::new(Profile::default());
    failed.can_host_here = true;
    failed.host_error = Some("Couldn't get an ID from wss://x: offline".into());
    let mut hs = harness(failed);
    hs.run();
    assert!(hs.query_by_label("Set an access password").is_some());
    assert!(hs.query_by_label("Turn on incoming connections").is_some());

    let mut settings = no_account(true);
    settings.tab = Tab::Settings;
    let mut hs = harness(settings);
    hs.run();
    assert!(
        hs.query_by_label("🔑  This computer's access password")
            .is_some()
    );
    assert_eq!(hs.query_all_by_label("Access password set").count(), 2);
}

#[test]
fn without_a_password_the_home_screen_says_connections_need_approval_here() {
    use egui_kittest::kittest::Queryable;
    let mut hs = harness(no_account(false));
    hs.run();
    assert!(
        hs.query_by_label_contains("you accept each connection here")
            .is_some()
    );
    assert!(
        hs.query_by_label_contains("every connection needs your OK")
            .is_some()
    );
    // And connecting to another computer works with just its ID (its person accepts).
    hs.state_mut().0.connect_to = "987 654 321".into();
    hs.run();
    hs.get_all_by_label("  Connect  ").next().unwrap().click();
    hs.run();
    assert_eq!(
        hs.state().1,
        vec![Request::ConnectGuest {
            id: "987 654 321".into(),
            password: home::Secret(String::new()),
        }]
    );
}

/// The question shown to the person at the computer (`rconn --ask`), against the stored
/// image, and its answers.
#[test]
fn the_accept_prompt_looks_as_stored_and_answers() {
    use egui_kittest::kittest::Queryable;
    use rconn_app::ask::{self, Answer, Ask};
    use std::time::{Duration, Instant};
    let start = Instant::now();
    let request = ask::parse(r#"{"viewer":"Ana on LAPTOP","timeout_secs":30}"#).unwrap();
    let prompt = |now: Instant| {
        Harness::builder()
            .with_size(egui::vec2(ask::WINDOW[0], ask::WINDOW[1]))
            .build_ui_state(
                move |ui, a: &mut Ask| ask::draw(ui, a, now),
                Ask {
                    request: request.clone(),
                    deadline: start + Duration::from_secs(23),
                    answer: None,
                },
            )
    };
    let mut hs = prompt(start);
    hs.run();
    assert!(
        hs.query_by_label_contains("“Ana on LAPTOP” wants to see and control this computer")
            .is_some()
    );
    assert!(hs.query_by_label("Declines in 23 s").is_some());
    hs.snapshot("accept-prompt");
    hs.get_by_label("  Accept  ").click();
    hs.run();
    assert_eq!(hs.state().answer, Some(Answer::Accept));

    let mut hs = prompt(start);
    hs.run();
    hs.get_by_label("  Decline  ").click();
    hs.run();
    assert_eq!(hs.state().answer, Some(Answer::Decline));

    // Nobody answers: past the deadline it times out (= decline on the host).
    let mut hs = prompt(start + Duration::from_secs(24));
    hs.run();
    assert_eq!(hs.state().answer, Some(Answer::Timeout));
    assert!(ask::parse("{}").is_err());
    assert_eq!(Answer::Accept.word(), "accept");
}

/// No account anywhere: an ID from the public server, a recent computer reached by ID.
fn no_account(password_set: bool) -> Home {
    let mut p = Profile::default();
    p.remember_code("987654321");
    let mut h = Home::new(p);
    let me = id("1e5ede94-60e3-408a-9cc4-b1c92e0ca86b");
    h.now = 1_000;
    h.this = Some(HostStatus {
        host_id: me.to_string(),
        short_code: me.short_code(),
        label: "Matts-MacBook".into(),
        server: rconn_core::DEFAULT_SERVER.into(),
        online: true,
        access_password: password_set,
        updated: 990,
        ..Default::default()
    });
    h
}

#[test]
fn no_server_means_the_public_one_and_guests_are_remembered_by_id() {
    let mut p = Profile::default();
    assert_eq!(p.server_url(), rconn_core::DEFAULT_SERVER);
    assert!(p.on_public_server());
    assert_eq!(
        p.incoming, None,
        "first launch turns incoming connections on"
    );
    p.remember_code("123-456789");
    p.remember_code("555 666 777");
    p.remember_code("123456789");
    let codes: Vec<_> = p.recent.iter().map(|r| r.code()).collect();
    assert_eq!(codes, vec!["123 456 789", "555 666 777"]);
    assert_eq!(rconn_app::profile::tidy_code(" abc "), "abc");
    // Older app.json files (host_id only) still load.
    let old: Profile = serde_json::from_str(
        r#"{"server":"","fingerprint":"","username":"","recent":[{"host_id":"1e5ede94-60e3-408a-9cc4-b1c92e0ca86b","label":"X"}]}"#,
    )
    .unwrap();
    assert_eq!(
        old.recent[0].code(),
        id("1e5ede94-60e3-408a-9cc4-b1c92e0ca86b").short_code()
    );
}

/// Drive the window like a person would: clicks in, requests out.
fn harness(h: Home) -> Harness<'static, (Home, Vec<Request>)> {
    Harness::builder()
        .with_size(egui::vec2(980.0, 640.0))
        .build_ui_state(
            |ui, (h, out): &mut (Home, Vec<Request>)| out.extend(home::draw(ui, h)),
            (h, Vec::new()),
        )
}

#[test]
fn without_an_account_the_window_asks_for_the_password_and_connects_by_id() {
    use egui_kittest::kittest::Queryable;
    let mut hs = harness(no_account(false));
    hs.run();
    // No sign-in anywhere on the way: the access password is the prompt.
    assert!(hs.query_by_label("Set an access password").is_some());
    assert!(hs.query_by_label("Sign in").is_none());
    // A recent computer: clicking it fills in its ID and waits for the password.
    hs.get_by_label("Connect").click();
    hs.run();
    assert_eq!(hs.state().0.connect_to, "987 654 321");
    hs.state_mut().0.access_password = "correct horse".into();
    hs.run();
    hs.get_all_by_label("  Connect  ").next().unwrap().click();
    hs.run();
    assert_eq!(
        hs.state().1,
        vec![Request::ConnectGuest {
            id: "987 654 321".into(),
            password: home::Secret("correct horse".into()),
        }]
    );
}

#[test]
fn incoming_connections_can_be_turned_off_and_on_without_an_account() {
    use egui_kittest::kittest::Queryable;
    let mut hs = harness(no_account(true));
    hs.run();
    assert!(hs.query_by_label("Access password set").is_some());
    hs.get_by_label("Turn off incoming connections").click();
    hs.run();
    assert_eq!(hs.state().1, vec![Request::DisableHost]);
    let mut off = Home::new(Profile::default());
    off.can_host_here = true;
    let mut hs = harness(off);
    hs.run();
    hs.get_by_label("Turn on incoming connections").click();
    hs.run();
    assert_eq!(hs.state().1, vec![Request::EnableHost]);
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
