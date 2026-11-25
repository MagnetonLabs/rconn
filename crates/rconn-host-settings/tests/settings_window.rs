//! Settings window: model behaviour, plus an offscreen render of the real layout.
//! Set RCONN_SETTINGS_PNG_DIR to also save the rendered window as PNGs.

use egui_kittest::Harness;
use rconn_core::settings::HiddenMode;
use rconn_host_settings::model::Model;
use rconn_host_settings::ui;
use std::path::Path;

fn data_dir(session_active: bool) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(
        d.path().join("host.toml"),
        "[server]\nurl = \"wss://100.84.21.78:8443/v1/ws\"\nfingerprint = \"sha256:035c91a36ab00dc1ff0e752bdc6d5bbb2fbeb34a1bc48f7a067db16864b80a74\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(d.path().join("identity")).unwrap();
    std::fs::write(
        d.path().join("identity/host.id"),
        "1e5ede94-60e3-408a-9cc4-b1c92e0ca86b",
    )
    .unwrap();
    if session_active {
        std::fs::write(d.path().join("session.active"), "x").unwrap();
    }
    d
}

#[test]
fn edits_are_detected_saved_and_reloaded() {
    let d = data_dir(false);
    let mut m = Model::load(d.path());
    assert!(m.changes().is_empty());
    let f = m.form.as_mut().unwrap();
    f.hidden_mode = HiddenMode::Indicator;
    f.max_bitrate_kbps = 4000;
    f.server_pin.clear(); // clearing removes the pin
    assert_eq!(m.changes().len(), 3);
    m.save();
    assert!(m.messages.iter().all(|(_, err)| !err), "{:?}", m.messages);
    let again = Model::load(d.path());
    let f = again.form.unwrap();
    assert_eq!(
        (f.hidden_mode, f.max_bitrate_kbps, f.server_pin.as_str()),
        (HiddenMode::Indicator, 4000, "")
    );
}

#[test]
fn an_active_session_locks_everything_and_saving_is_refused() {
    let d = data_dir(true);
    let mut m = Model::load(d.path());
    assert_eq!(
        m.lock_reason("clipboard"),
        Some("Locked while a remote session is active")
    );
    m.form.as_mut().unwrap().clipboard = false;
    m.save();
    assert!(
        m.messages
            .iter()
            .any(|(t, err)| *err && t.contains("session")),
        "{:?}",
        m.messages
    );
    assert!(
        Model::load(d.path()).form.unwrap().clipboard,
        "disk unchanged"
    );
}

#[test]
fn revert_discards_edits() {
    let d = data_dir(false);
    let mut m = Model::load(d.path());
    m.form.as_mut().unwrap().log_level = "debug".into();
    m.revert();
    assert!(m.changes().is_empty());
}

fn render(name: &str, dir: &Path, session_active: bool) {
    let d = data_dir(session_active);
    let mut model = Model::load(d.path());
    if !session_active {
        model.form.as_mut().unwrap().hidden_mode = HiddenMode::Indicator; // one pending change
    }
    let mut harness = Harness::builder()
        .with_size(egui::vec2(720.0, 520.0))
        .build_ui(|ui| {
            ui::draw(ui, &mut model);
        });
    harness.run();
    let image = harness
        .render()
        .expect("offscreen render (needs a GPU adapter)");
    image.save(dir.join(format!("{name}.png"))).unwrap();
}

#[test]
fn renders_offscreen() {
    let dir = std::env::var_os("RCONN_SETTINGS_PNG_DIR").map(std::path::PathBuf::from);
    let tmp = tempfile::tempdir().unwrap();
    let out = dir.as_deref().unwrap_or(tmp.path());
    render("settings-normal", out, false);
    render("settings-session-locked", out, true);
}
