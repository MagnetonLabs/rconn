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

/// Is the host set up to start at login here? (The settings file outlives turning incoming
/// connections off, so it doesn't say.)
pub fn installed() -> bool {
    service_path().exists()
}

/// Make sure the host's folder (0700) and settings file (0600, empty if new) exist, so
/// single settings can be written into it (e.g. the access password before the ID came).
pub fn ensure_settings_file(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let file = dir.join("host.toml");
    if !file.exists() {
        write_private(&file, "")?;
    }
    Ok(())
}

/// Point the settings file at `server`, keeping everything else in it (the access password
/// above all). An unreadable file is replaced.
pub fn write_server_settings(
    dir: &Path,
    server: &str,
    fingerprint: Option<&str>,
) -> Result<(), String> {
    use rconn_core::settings::{SettingsLayers, Writer, write_setting};
    let file = dir.join("host.toml");
    ensure_settings_file(dir).map_err(|e| e.to_string())?;
    let layers = SettingsLayers {
        system_file: Some(file.clone()),
        ..Default::default()
    };
    let kept = [
        ("server.url", server),
        ("server.fingerprint", fingerprint.unwrap_or("")),
    ]
    .iter()
    .try_for_each(|(k, v)| write_setting(layers.clone(), k, v, Writer::Admin, false));
    if let Err(e) = kept {
        tracing::warn!(error = %e, "host.toml unreadable; writing a new one");
        let toml = match fingerprint {
            Some(fp) => format!("[server]\nurl = {server:?}\nfingerprint = {fp:?}\n"),
            None => format!("[server]\nurl = {server:?}\n"),
        };
        return write_private(&file, &toml).map_err(|e| e.to_string());
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())
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
    write_server_settings(dir, server, fingerprint)?;
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

/// Stop the agent and remove its start-at-login definition. The identity and the settings
/// (with the access password) are kept for turning incoming connections on again.
pub fn uninstall(dir: &Path) {
    if cfg!(target_os = "macos") {
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("gui/{}/{LABEL}", uid())])
            .status();
    } else {
        let _ = run("systemctl", &["--user", "disable", "--now", UNIT]);
    }
    let _ = std::fs::remove_file(service_path());
    forget_status(dir);
}

/// Drop what only means something while the agent runs (its status file).
pub fn forget_status(dir: &Path) {
    let _ = std::fs::remove_file(dir.join("status.json"));
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

    fn settings(dir: &Path) -> rconn_core::settings::HostSettings {
        rconn_core::settings::LoadedSettings::load(rconn_core::settings::SettingsLayers {
            system_file: Some(dir.join("host.toml")),
            ..Default::default()
        })
        .unwrap()
        .settings()
        .clone()
    }

    fn set_password(dir: &Path, verifier: &str) {
        ensure_settings_file(dir).unwrap();
        rconn_core::settings::write_setting(
            rconn_core::settings::SettingsLayers {
                system_file: Some(dir.join("host.toml")),
                ..Default::default()
            },
            "access_password",
            verifier,
            rconn_core::settings::Writer::Admin,
            false,
        )
        .unwrap();
    }

    /// Turning incoming connections on (again) rewrites the server, not the password.
    #[test]
    fn installing_keeps_a_saved_access_password() {
        let d = tempfile::tempdir().unwrap();
        set_password(d.path(), "v1$abc$def");
        write_server_settings(d.path(), "wss://a/v1/ws", Some("sha256:ab")).unwrap();
        let s = settings(d.path());
        assert_eq!(s.access_password, "v1$abc$def");
        let server = s.server.unwrap();
        assert_eq!(server.url, "wss://a/v1/ws");
        assert_eq!(server.fingerprint.as_deref(), Some("sha256:ab"));
        // Another server, no pin: the old pin goes, the password stays.
        write_server_settings(d.path(), "wss://b/v1/ws", None).unwrap();
        let s = settings(d.path());
        assert_eq!(s.access_password, "v1$abc$def");
        assert_eq!(s.server.unwrap().fingerprint, None);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(d.path().join("host.toml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the verifier stays private");
        // A file that doesn't parse is replaced rather than blocking the host.
        std::fs::write(d.path().join("host.toml"), "not = [toml").unwrap();
        write_server_settings(d.path(), "wss://c/v1/ws", None).unwrap();
        assert_eq!(settings(d.path()).server.unwrap().url, "wss://c/v1/ws");
    }

    /// Turning incoming connections off keeps the settings (and so the password).
    #[test]
    fn turning_off_keeps_the_settings_file() {
        let d = tempfile::tempdir().unwrap();
        set_password(d.path(), "v1$abc$def");
        std::fs::write(d.path().join("status.json"), "{}").unwrap();
        forget_status(d.path());
        assert!(!d.path().join("status.json").exists());
        assert_eq!(settings(d.path()).access_password, "v1$abc$def");
        // "Installed" is the start-at-login file, not host.toml.
        assert_eq!(installed(), service_path().exists());
    }

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
