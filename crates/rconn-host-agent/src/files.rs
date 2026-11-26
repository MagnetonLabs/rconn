//! File transfer for a session: whether the viewer's `rconn-files/1` channel is allowed,
//! which folder it shares, and serving it (`rconn_peer::files` does the transfers).
//!
//! The shared folder is the home of the person signed in here (on Windows, where the agent
//! runs as SYSTEM in the console session, that session user's profile). Uploads land in its
//! Downloads/Rconn. Every transfer leaves an audit line (target `rconn_audit`).

use crate::access::Gate;
use rconn_core::ids::SessionId;
use rconn_peer::files::{ServeConfig, refuse, serve};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use webrtc::data_channel::DataChannel;

/// Why the file channel can't be used, if it can't: before the session is let in, or when
/// this computer's setting or the server's policy turns file transfer off.
pub fn refusal(setting: bool, policy: bool, gate_open: bool) -> Option<&'static str> {
    if !gate_open {
        Some("file transfer starts once the session is accepted")
    } else if !setting {
        Some("file transfer is turned off on this computer")
    } else if !policy {
        Some("file transfer is turned off by your organization's policy")
    } else {
        None
    }
}

/// Serve (or refuse) a viewer's file channel until it closes.
pub async fn serve_channel(
    dc: Arc<dyn DataChannel>,
    session: SessionId,
    setting: bool,
    policy: bool,
    gate: Arc<Gate>,
) {
    if let Some(why) = refusal(setting, policy, gate.is_open()) {
        tracing::info!(%session, why, "file channel refused");
        return refuse(dc, why).await;
    }
    let Some(home) = home_dir() else {
        tracing::warn!(%session, "no signed-in user's folder to share");
        return refuse(dc, "nobody is signed in to this computer").await;
    };
    let cfg = ServeConfig {
        inbox: home.join("Downloads").join("Rconn"),
        root: home,
        free_space: Some(Arc::new(free_space)),
        session: session.to_string(),
    };
    serve(dc, cfg).await;
}

/// The signed-in user's home folder.
#[cfg(unix)]
fn home_dir() -> Option<PathBuf> {
    // The agent runs as that user (a LaunchAgent on macOS, a systemd user unit on Linux).
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute() && h.is_dir())
}

/// The profile folder of the user signed in to this agent's session.
#[cfg(windows)]
fn home_dir() -> Option<PathBuf> {
    session_user_profile().or_else(|| {
        // Run by hand (not by the service): the agent is that user.
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .filter(|p| {
                p.is_dir()
                    && !p
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .contains("systemprofile")
            })
    })
}

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 FFI: the session user's token and profile folder
fn session_user_profile() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken};
    use windows::Win32::UI::Shell::GetUserProfileDirectoryW;
    use windows::core::PWSTR;
    unsafe {
        let mut session = 0u32;
        ProcessIdToSessionId(std::process::id(), &mut session).ok()?;
        // Needs SeTcbPrivilege (the service's SYSTEM token has it); fails with nobody
        // signed in to the session.
        let mut token = HANDLE::default();
        WTSQueryUserToken(session, &mut token).ok()?;
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let got = GetUserProfileDirectoryW(token, Some(PWSTR(buf.as_mut_ptr())), &mut len);
        let _ = CloseHandle(token);
        got.ok()?;
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let path = PathBuf::from(std::ffi::OsString::from_wide(&buf[..end]));
        path.is_dir().then_some(path)
    }
}

/// Free bytes for this user on the disk holding `path`.
#[cfg(unix)]
fn free_space(path: &Path) -> Option<u64> {
    let s = rustix::fs::statvfs(path).ok()?;
    Some(s.f_bavail.saturating_mul(s.f_frsize))
}

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 FFI: GetDiskFreeSpaceExW
fn free_space(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    use windows::core::PCWSTR;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free = 0u64;
    unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut free), None, None).ok()? };
    Some(free)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_channel_needs_an_open_session_the_setting_and_the_policy() {
        assert_eq!(refusal(true, true, true), None);
        assert!(refusal(true, true, false).unwrap().contains("accepted"));
        assert!(refusal(false, true, true).unwrap().contains("turned off"));
        assert!(refusal(true, false, true).unwrap().contains("policy"));
        // Before the session is let in, nothing else is even considered.
        assert!(refusal(false, false, false).unwrap().contains("accepted"));
    }

    #[test]
    fn there_is_a_home_and_its_free_space_is_known() {
        let home = home_dir().expect("tests run as a signed-in user");
        assert!(home.is_dir());
        assert!(free_space(&home).is_some_and(|b| b > 0));
    }
}
