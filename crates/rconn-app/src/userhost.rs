//! Make this computer a host without an installer, root or administrator.
//!
//! The host agent runs per user and starts at login, restarting if it stops: a
//! LaunchAgent on macOS, a systemd user unit on Linux, and on Windows ("just for me"
//! installs, without the service) `rconn --start-host` from the user's Run key, which
//! keeps the agent running (see [`supervise`]). Its identity lives in the app's config
//! folder under `host` (0700; on Windows `%LOCALAPPDATA%\rconn\host`, which doesn't roam).
//! This covers a signed-in desktop; the sign-in screen and Windows' admin prompts need the
//! system service (the Windows installer for everyone).

use std::path::{Path, PathBuf};
use std::process::Command;

pub const LABEL: &str = "app.rconn.host";
/// Linux: the systemd user unit's name.
pub const UNIT: &str = "rconn-host.service";
/// Windows: the value under the user's Run key that starts the host at sign-in.
#[cfg(windows)]
const RUN_VALUE: &str = "rconn-host";
#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
/// Windows: held by the one running [`supervise`].
#[cfg(windows)]
const SUPERVISOR_PORT: u16 = 47_394;

/// Windows: the per-user host folder (machine-local, not roaming).
pub fn windows_host_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "rconn").map_or_else(
        || std::env::temp_dir().join("rconn").join("host"),
        |d| d.data_local_dir().join("host"),
    )
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// Where the start-at-login definition goes (Windows: the agent's command line, which
/// [`supervise`] runs).
pub fn service_path() -> PathBuf {
    if cfg!(windows) {
        windows_host_dir().join("autostart.json")
    } else if cfg!(target_os = "macos") {
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
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// A command that never flashes a console window (the app is a GUI program on Windows).
fn quiet(program: impl AsRef<std::ffi::OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
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
    let out = quiet(program)
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
    }
    Ok(())
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
    if cfg!(windows) {
        return start_windows(&service_file, &agent_args(agent, dir, server, fingerprint));
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

/// Windows: save the agent's command line, start the supervisor at sign-in, and now.
#[cfg_attr(not(windows), allow(unused_variables))]
fn start_windows(service_file: &Path, args: &[String]) -> Result<(), String> {
    #[cfg(not(windows))]
    return Err("Windows only".into());
    #[cfg(windows)]
    {
        let args = serde_json::to_string(args).map_err(|e| e.to_string())?;
        std::fs::write(service_file, args).map_err(|e| e.to_string())?;
        let app = std::env::current_exe().map_err(|e| e.to_string())?;
        let start = format!("\"{}\" --start-host", app.display());
        run(
            "reg",
            &[
                "add", RUN_KEY, "/v", RUN_VALUE, "/t", "REG_SZ", "/d", &start, "/f",
            ],
        )?;
        // A running supervisor restarts the agent with the new command line; otherwise
        // this one starts (a second one exits at once).
        stop_agent();
        quiet(&app)
            .arg("--start-host")
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("starting the host: {e}"))
    }
}

/// Stop the agent and remove its start-at-login definition. The identity and the settings
/// (with the access password) are kept for turning incoming connections on again.
pub fn uninstall(dir: &Path) {
    if cfg!(windows) {
        // The supervisor stops once its command line is gone.
        let _ = std::fs::remove_file(service_path());
        #[cfg(windows)]
        let _ = run("reg", &["delete", RUN_KEY, "/v", RUN_VALUE, "/f"]);
        stop_agent();
    } else if cfg!(target_os = "macos") {
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("gui/{}/{LABEL}", uid())])
            .status();
    } else {
        let _ = run("systemctl", &["--user", "disable", "--now", UNIT]);
    }
    let _ = std::fs::remove_file(service_path());
    forget_status(dir);
}

/// Windows: end this user's host agent (the supervisor starts it again if it should run).
fn stop_agent() {
    if cfg!(windows) {
        let _ = run("taskkill", &["/IM", "rconn-host-agent.exe", "/F"]);
    }
}

/// Windows (`rconn --start-host`, at sign-in): keep the host agent running, as launchd and
/// systemd do on macOS and Linux. Restarts it when it exits (after a pause if it keeps
/// failing) and stops, ending the agent, once [`uninstall`] removed its command line. Only
/// one runs per user.
#[cfg(windows)]
pub fn supervise() {
    use std::os::windows::process::CommandExt;
    use std::time::{Duration, Instant};
    let Ok(_only_one) = std::net::TcpListener::bind(("127.0.0.1", SUPERVISOR_PORT)) else {
        return;
    };
    let mut child: Option<std::process::Child> = None;
    let mut started = Instant::now();
    loop {
        let args: Option<Vec<String>> = std::fs::read_to_string(service_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .filter(|a: &Vec<String>| !a.is_empty());
        let Some(args) = args else {
            if let Some(c) = child.as_mut() {
                let _ = c.kill();
            }
            return;
        };
        let running = child
            .as_mut()
            .is_some_and(|c| matches!(c.try_wait(), Ok(None)));
        if !running {
            if child.is_some() && started.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_secs(5)); // failing at start: don't spin
            }
            let log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(windows_host_dir().join("agent.log"));
            let mut c = Command::new(&args[0]);
            c.args(&args[1..])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            if let Ok(log) = log {
                c.stderr(log);
            }
            started = Instant::now();
            child = c.spawn().ok();
        }
        std::thread::sleep(Duration::from_secs(1));
    }
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.path().join("host.toml"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the verifier stays private");
        }
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
