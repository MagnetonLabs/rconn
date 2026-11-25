//! OS bits: service state, restarting the agent, relaunching elevated.

use std::process::Command;

/// A command that never flashes a console window (this is a GUI program).
fn quiet(program: &str) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

pub fn default_data_dir() -> std::path::PathBuf {
    if cfg!(windows) {
        let base = std::env::var_os("ProgramData")
            .map_or_else(|| r"C:\ProgramData".into(), std::path::PathBuf::from);
        base.join("rconn")
    } else if cfg!(target_os = "macos") {
        "/Library/Application Support/rconn".into()
    } else {
        linux_host_dir()
    }
}

/// Linux: the host runs per user (like the Mac's LaunchAgent), with its identity and
/// settings in `$XDG_CONFIG_HOME/rconn/host` (the app's config folder, `host` inside).
fn linux_host_dir() -> std::path::PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_else(std::env::temp_dir);
    config.join("rconn").join("host")
}

/// "Running", "Stopped", "Not installed", ...
pub fn service_state() -> String {
    if !cfg!(windows) {
        return "n/a on this OS".into();
    }
    let Ok(out) = quiet("sc").args(["query", "rconn-host"]).output() else {
        return "unknown".into();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    if text.contains("RUNNING") {
        "Running".into()
    } else if text.contains("STOPPED") {
        "Stopped".into()
    } else if text.contains("1060") || !out.status.success() {
        "Not installed".into()
    } else {
        "Starting/stopping".into()
    }
}

/// Restart the agent so new settings apply; it is back within ~2 s.
pub fn restart_agent() -> Result<(), String> {
    if cfg!(target_os = "macos") {
        // Per-user LaunchAgent set up by the app (see rconn-app's machost).
        let uid = quiet("id").arg("-u").output().map_err(|e| e.to_string())?;
        let uid = String::from_utf8_lossy(&uid.stdout).trim().to_owned();
        let out = quiet("launchctl")
            .args(["kickstart", "-k", &format!("gui/{uid}/app.rconn.host")])
            .output()
            .map_err(|e| e.to_string())?;
        return if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
        };
    }
    if !cfg!(windows) {
        // Per-user systemd unit set up by the app (see rconn-app's userhost).
        let out = quiet("systemctl")
            .args(["--user", "restart", "rconn-host.service"])
            .output()
            .map_err(|e| e.to_string())?;
        return if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
        };
    }
    let out = quiet("taskkill")
        .args(["/IM", "rconn-host-agent.exe", "/F"])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() || String::from_utf8_lossy(&out.stderr).contains("not found") {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

/// Where the host agent publishes its status (readable without admin rights).
pub fn status_file() -> std::path::PathBuf {
    if !cfg!(any(windows, target_os = "macos")) {
        return linux_host_dir().join("status.json");
    }
    default_data_dir()
        .with_file_name("rconn-status")
        .join("status.json")
}

/// Start this program again with administrator rights (shows the UAC prompt).
pub fn relaunch_elevated() -> Result<(), String> {
    run_elevated(&std::env::current_exe().map_err(|e| e.to_string())?)
}

/// Run `exe args` with administrator rights (UAC prompt) and wait for it. Arguments must
/// be non-empty, without spaces or quotes. Fails if the prompt is declined or the program fails.
pub fn run_elevated_wait(exe: &std::path::Path, args: &[&str]) -> Result<(), String> {
    if !cfg!(windows) {
        return Err("Run with sudo on this OS".into());
    }
    // Start-Process mangles empty and quoted arguments (an empty one arrived as " "):
    // refuse them rather than pass something else.
    if args
        .iter()
        .any(|a| a.trim().is_empty() || a.contains(['\'', '"', ' ']))
    {
        return Err("unsupported argument (empty, spaces or quotes)".into());
    }
    let arglist = args
        .iter()
        .map(|a| format!("'{a}'"))
        .collect::<Vec<_>>()
        .join(",");
    let script = format!(
        "try {{ $p = Start-Process -FilePath '{}' -ArgumentList {arglist} -Verb RunAs -Wait -PassThru -WindowStyle Hidden; exit $p.ExitCode }} catch {{ exit 1223 }}",
        exe.display()
    );
    let status = quiet("powershell")
        .args(["-NoProfile", "-Command", &script])
        .status()
        .map_err(|e| e.to_string())?;
    match status.code() {
        Some(0) => Ok(()),
        Some(1223) => Err("the administrator prompt was declined".into()),
        Some(c) => Err(format!("failed (exit code {c})")),
        None => Err("failed".into()),
    }
}

/// Start `exe` with administrator rights (shows the UAC prompt).
pub fn run_elevated(exe: &std::path::Path) -> Result<(), String> {
    if !cfg!(windows) {
        return Err("Run with sudo on this OS".into());
    }
    quiet("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("Start-Process -FilePath '{}' -Verb RunAs", exe.display()),
        ])
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}
