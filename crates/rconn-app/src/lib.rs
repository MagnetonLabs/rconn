//! rconn desktop app: window, worker, tray.

pub mod home;
pub mod icon;
pub mod profile;
#[cfg(any(windows, target_os = "macos"))]
pub mod tray;
pub mod worker;
