//! rconn desktop app: window, worker, tray.

pub mod ask;
pub mod home;
pub mod icon;
pub mod profile;
pub mod saved;
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub mod tray;
pub mod userhost;
pub mod worker;

/// Held by the open window; a second launch (or the tray) talks to it here.
pub const WINDOW_PORT: u16 = 47_391;
