//! OS bits: service state, restarting the agent, relaunching elevated.

use std::process::Command;

pub fn default_data_dir() -> std::path::PathBuf {
    if cfg!(windows) {
        let base = std::env::var_os("ProgramData")
            .map_or_else(|| r"C:\ProgramData".into(), std::path::PathBuf::from);
        base.join("rconn")
    } else {
        "/Library/Application Support/rconn".into()
    }
}

/// "Running", "Stopped", "Not installed", ...
pub fn service_state() -> String {
    if !cfg!(windows) {
        return "n/a on this OS".into();
    }
    let Ok(out) = Command::new("sc").args(["query", "rconn-host"]).output() else {
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

/// Restart the agent so new settings apply; the service relaunches it within ~2 s.
pub fn restart_agent() -> Result<(), String> {
    if !cfg!(windows) {
        return Err("Restarting the agent is Windows-only for now".into());
    }
    let out = Command::new("taskkill")
        .args(["/IM", "rconn-host-agent.exe", "/F"])
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() || String::from_utf8_lossy(&out.stderr).contains("not found") {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

/// Start this program again with administrator rights (shows the UAC prompt).
pub fn relaunch_elevated() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    if !cfg!(windows) {
        return Err("Run with sudo on this OS".into());
    }
    Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("Start-Process -FilePath '{}' -Verb RunAs", exe.display()),
        ])
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}
