//! macOS and Linux: make this computer a host without an installer or root.
//!
//! The host agent runs per user and starts at login, restarting if it stops: a
//! LaunchAgent on macOS, a systemd user unit on Linux. Its identity lives in the app's
//! config folder under `host` (0700). This covers a logged-in desktop; connecting at the
//! login screen needs a system service (not yet).

use std::path::{Path, PathBuf};
use std::process::Command;

pub const LABEL: &str = "app.rconn.host";
/// Linux: the systemd user unit's name.
pub const UNIT: &str = "rconn-host.service";

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// Where the start-at-login definition goes.
pub fn service_path() -> PathBuf {
    if cfg!(target_os = "macos") {
        home()
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map_or_else(|| home().join(".config"), PathBuf::from)
            .join("systemd/user")
            .join(UNIT)
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
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

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The agent's command line for this server.
fn agent_args(agent: &Path, dir: &Path, server: &str, fingerprint: Option<&str>) -> Vec<String> {
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
    args
}

/// The LaunchAgent definition running `agent` for this server.
pub fn plist(agent: &Path, dir: &Path, server: &str, fingerprint: Option<&str>) -> String {
    let d = |name: &str| dir.join(name).to_string_lossy().into_owned();
    let args: String = agent_args(agent, dir, server, fingerprint)
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

/// systemd quoting: every argument in double quotes, with `\`, `"`, `%` and `$` escaped.
fn systemd_quote(a: &str) -> String {
    let escaped = a
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{escaped}\"")
}

/// The systemd user unit running `agent` for this server.
pub fn unit(agent: &Path, dir: &Path, server: &str, fingerprint: Option<&str>) -> String {
    let exec = agent_args(agent, dir, server, fingerprint)
        .iter()
        .map(|a| systemd_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[Unit]
Description=Rconn host (accepts remote connections to this desktop)
After=graphical-session.target
PartOf=graphical-session.target

[Service]
ExecStart={exec}
Restart=always
RestartSec=2
StandardError=append:{log}

[Install]
WantedBy=graphical-session.target
",
        log = dir.join("agent.log").to_string_lossy(),
    )
}

fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Write the settings file and the start-at-login definition, then (re)start the agent.
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
    let service_file = service_path();
    if let Some(parent) = service_file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    if !cfg!(target_os = "macos") {
        std::fs::write(&service_file, unit(agent, dir, server, fingerprint))
            .map_err(|e| e.to_string())?;
        run("systemctl", &["--user", "daemon-reload"])?;
        run("systemctl", &["--user", "enable", UNIT])?;
        return run("systemctl", &["--user", "restart", UNIT]);
    }
    let plist_file = service_file;
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

/// Stop the agent and remove its start-at-login definition (the identity is kept for
/// re-enabling).
pub fn uninstall(dir: &Path) {
    if cfg!(target_os = "macos") {
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("gui/{}/{LABEL}", uid())])
            .status();
    } else {
        let _ = run("systemctl", &["--user", "disable", "--now", UNIT]);
    }
    let _ = std::fs::remove_file(service_path());
    let _ = std::fs::remove_file(dir.join("status.json"));
    let _ = std::fs::remove_file(dir.join("host.toml"));
}

/// macOS: open the System Settings pane for a permission named in `HostStatus`.
#[cfg(target_os = "macos")]
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

    #[test]
    fn unit_quotes_every_argument_for_systemd() {
        let u = unit(
            Path::new("/opt/rconn/rconn-host-agent"),
            Path::new("/h/my host"),
            "wss://s/v1/ws?a=%1&b=$x",
            None,
        );
        assert!(u.contains(
            r#"ExecStart="/opt/rconn/rconn-host-agent" "--server" "wss://s/v1/ws?a=%%1&b=$$x""#
        ));
        assert!(u.contains(r#""--identity" "/h/my host""#));
        assert!(u.contains("Restart=always"));
        assert!(!u.contains("--fingerprint"));
    }
}
