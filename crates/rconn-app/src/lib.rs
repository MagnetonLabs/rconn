//! rconn desktop app: window, worker, tray.

pub mod home;
pub mod icon;
pub mod profile;
#[cfg(any(windows, target_os = "macos"))]
pub mod tray;
pub mod worker;

/// Held by the open window; a second launch (or the tray) talks to it here.
pub const WINDOW_PORT: u16 = 47_391;
