//! `rconn-tray`: the menu-bar / tray icon on its own (macOS ships it as a helper app inside
//! rconn.app, so opening rconn while the icon runs still opens the window).
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> anyhow::Result<()> {
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    return rconn_app::tray::run(rconn_app::worker::Paths::installed().status_file);
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    Ok(())
}
