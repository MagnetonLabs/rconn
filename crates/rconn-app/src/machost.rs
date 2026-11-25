//! macOS: make this Mac a host without an installer or sudo.
//!
//! The host agent runs as a per-user LaunchAgent (starts at login, restarts if it stops),
//! with its identity in `~/Library/Application Support/rconn/host` (0700). This covers a
//! logged-in Mac; connecting at the login window needs a system LaunchDaemon (not yet).

use std::path::{Path, PathBuf};
use std::process::Command;

pub const LABEL: &str = "app.rconn.host";

pub fn plist_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

fn uid() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, contents)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Is a host identity already saved here? (Re-enabling reuses it.)
pub fn has_identity(dir: &Path) -> bool {
    dir.join("host.id").exists()
        && dir.join("host.key.pem").exists()
        && dir.join("host.cert.pem").exists()
}

pub fn save_identity(
    dir: &Path,
    host_id: &str,
    cert_pem: &str,
    key_pem: &str,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    write_private(&dir.join("host.id"), host_id)?;
    write_private(&dir.join("host.cert.pem"), cert_pem)?;
    write_private(&dir.join("host.key.pem"), key_pem)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The LaunchAgent definition running `agent` for this server.
pub fn plist(agent: &Path, dir: &Path, server: &str, fingerprint: Option<&str>) -> String {
    let d = |name: &str| dir.join(name).to_string_lossy().into_owned();
    let mut args = vec![
        agent.to_string_lossy().into_owned(),
        "--server".into(),
        server.into(),
        "--server-id".into(),
        server.into(),
        "--identity".into(),
        dir.to_string_lossy().into_owned(),
        "--settings".into(),
        d("host.toml"),
        "--state-dir".into(),
        dir.to_string_lossy().into_owned(),
        "--status-file".into(),
        d("status.json"),
    ];
    if let Some(fp) = fingerprint {
        args.extend(["--fingerprint".into(), fp.into()]);
    }
    let args: String = args
        .iter()
        .map(|a| format!("    <string>{}</string>\n", xml_escape(a)))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{args}  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        log = xml_escape(&d("agent.log")),
    )
}

/// Write the settings file and LaunchAgent, then (re)start the agent.
pub fn install(
    agent: &Path,
    dir: &Path,
    server: &str,
    fingerprint: Option<&str>,
) -> Result<(), String> {
    if !agent.exists() {
        return Err(format!("host agent not found at {}", agent.display()));
    }
    let toml = match fingerprint {
        Some(fp) => format!("[server]\nurl = {server:?}\nfingerprint = {fp:?}\n"),
        None => format!("[server]\nurl = {server:?}\n"),
    };
    write_private(&dir.join("host.toml"), &toml).map_err(|e| e.to_string())?;
    let plist_file = plist_path();
    if let Some(parent) = plist_file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&plist_file, plist(agent, dir, server, fingerprint))
        .map_err(|e| e.to_string())?;
    let domain = format!("gui/{}", uid());
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{LABEL}")])
        .status();
    let out = Command::new("launchctl")
        .args(["bootstrap", &domain])
        .arg(&plist_file)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "launchctl: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Stop the agent and remove the LaunchAgent (the identity is kept for re-enabling).
pub fn uninstall(dir: &Path) {
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{}/{LABEL}", uid())])
        .status();
    let _ = std::fs::remove_file(plist_path());
    let _ = std::fs::remove_file(dir.join("status.json"));
    let _ = std::fs::remove_file(dir.join("host.toml"));
}

/// Open the System Settings pane for a permission named in `HostStatus`.
pub fn open_permission_settings(name: &str) {
    let pane = if name == "Accessibility" {
        "Privacy_Accessibility"
    } else {
        "Privacy_ScreenCapture"
    };
    let _ = Command::new("open")
        .arg(format!(
            "x-apple.systempreferences:com.apple.preference.security?{pane}"
        ))
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_runs_the_agent_with_its_identity_and_escapes_values() {
        let p = plist(
            Path::new("/A/rconn-host-agent"),
            Path::new("/h/host"),
            "wss://s/v1/ws?a&b",
            Some("sha256:ab"),
        );
        assert!(p.contains("<string>/A/rconn-host-agent</string>"));
        assert!(p.contains("<string>wss://s/v1/ws?a&amp;b</string>"));
        assert!(p.contains("<string>--fingerprint</string>\n    <string>sha256:ab</string>"));
        assert!(p.contains("<string>/h/host/status.json</string>"));
        assert!(p.contains("<key>KeepAlive</key><true/>"));
    }
}
