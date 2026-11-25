//! The `rconn-host` Windows service (LocalSystem, auto-start).
//!
//! It keeps exactly one agent running in the active console session: launched with the
//! SYSTEM token (see [`crate::console`]) so it can capture and drive the lock screen and
//! UAC prompts, relaunched if it exits or the console session changes (logon, logoff, fast
//! user switching), and killed when the service stops.
//!
//! Layout:
//! ```text
//! <install dir>\rconn-host-service.exe, rconn-host-agent.exe, openh264-*.dll
//! C:\ProgramData\rconn\host.toml     settings (system layer; admin-only ACL)
//! C:\ProgramData\rconn\identity\     host.id, host.cert.pem, host.key.pem
//! C:\ProgramData\rconn\agent.log     agent output (truncated past 10 MB)
//! ```

use crate::console::{Child, console_session, launch_in_session, shutting_down};
use anyhow::{Context, Result};
use rconn_core::settings::{LoadedSettings, SettingsLayers};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceControl, ServiceControlAccept,
    ServiceErrorControl, ServiceExitCode, ServiceFailureActions, ServiceFailureResetPeriod,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

pub const NAME: &str = "rconn-host";
const DISPLAY: &str = "rconn host";
const LOG_LIMIT: u64 = 10 * 1024 * 1024;

pub use crate::paths::{data_dir, status_dir};

/// Everything needed to launch the agent, read fresh each launch so config edits apply
/// at the next (re)start without restarting the service.
struct AgentLaunch {
    exe: PathBuf,
    args: Vec<String>,
    log: PathBuf,
}

fn agent_launch() -> Result<AgentLaunch> {
    let dir = data_dir();
    let settings = LoadedSettings::load(SettingsLayers {
        system_file: Some(dir.join("host.toml")),
        ..Default::default()
    })?;
    let server = settings
        .settings()
        .server
        .clone()
        .context("host.toml has no [server]; run `rconn-host-service configure`")?;
    let install = std::env::current_exe()?
        .parent()
        .context("service exe has no directory")?
        .to_path_buf();
    let mut args = vec![
        "--server".into(),
        server.url.clone(),
        "--server-id".into(),
        server.url.clone(),
        "--identity".into(),
        dir.join("identity").to_string_lossy().into_owned(),
        "--max-kbps".into(),
        settings.settings().max_bitrate_kbps.to_string(),
    ];
    let status_dir = status_dir();
    let _ = std::fs::create_dir_all(&status_dir);
    args.extend([
        "--status-file".into(),
        status_dir
            .join("status.json")
            .to_string_lossy()
            .into_owned(),
        "--settings".into(),
        dir.join("host.toml").to_string_lossy().into_owned(),
        "--state-dir".into(),
        dir.to_string_lossy().into_owned(),
    ]);
    if let Some(fp) = &server.fingerprint {
        args.extend(["--fingerprint".into(), fp.clone()]);
    }
    Ok(AgentLaunch {
        exe: install.join("rconn-host-agent.exe"),
        args,
        log: dir.join("agent.log"),
    })
}

fn rotate(log: &std::path::Path) {
    if std::fs::metadata(log).is_ok_and(|m| m.len() > LOG_LIMIT) {
        let _ = std::fs::rename(log, log.with_extension("log.old"));
    }
}

/// Keep one agent alive in the console session until `stop` fires.
fn supervise(stop: &mpsc::Receiver<()>) {
    let mut current: Option<(u32, Child, Instant)> = None;
    let mut backoff = Duration::from_secs(1);
    loop {
        match stop.recv_timeout(Duration::from_secs(2)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if shutting_down() {
            continue; // the OS is stopping processes; wait for our own Stop/Shutdown
        }
        let Ok(session) = console_session() else {
            continue;
        };
        let healthy = current
            .as_ref()
            .is_some_and(|(s, c, _)| *s == session && c.alive());
        if healthy {
            continue;
        }
        if let Some((old_session, child, started)) = current.take() {
            child.kill();
            if old_session == session && started.elapsed() < Duration::from_secs(30) {
                // Died young in the same session: back off so a crash loop can't spin.
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(60));
            } else {
                backoff = Duration::from_secs(1);
            }
        }
        match agent_launch().and_then(|l| {
            rotate(&l.log);
            launch_in_session(session, &l.exe, &l.args, &l.log)
        }) {
            Ok(child) => {
                tracing::info!(pid = child.pid, session, "agent started");
                current = Some((session, child, Instant::now()));
            }
            Err(e) => tracing::error!(error = %format!("{e:#}"), session, "could not start agent"),
        }
    }
    if let Some((_, child, _)) = current {
        child.kill();
    }
}

define_windows_service!(ffi_service_main, service_main);

/// Entry point when started by the Service Control Manager.
pub fn run() -> Result<()> {
    service_dispatcher::start(NAME, ffi_service_main)
        .context("not started by the service control manager")
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        tracing::error!(error = %format!("{e:#}"), "service failed");
    }
}

fn run_service() -> Result<()> {
    let (stop_tx, stop_rx) = mpsc::channel();
    let handler = move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        // Session changes are picked up by the 2 s poll; accepting them keeps the SCM happy.
        ServiceControl::SessionChange(_) | ServiceControl::Interrogate => {
            ServiceControlHandlerResult::NoError
        }
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status = service_control_handler::register(NAME, handler)?;
    let set = |state, accept| {
        status.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        })
    };
    set(
        ServiceState::Running,
        ServiceControlAccept::STOP
            | ServiceControlAccept::SHUTDOWN
            | ServiceControlAccept::SESSION_CHANGE,
    )?;
    tracing::info!("service running");
    supervise(&stop_rx);
    set(ServiceState::Stopped, ServiceControlAccept::empty())?;
    Ok(())
}

/// Register the service: auto-start, LocalSystem, restart on failure.
pub fn install() -> Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;
    let info = ServiceInfo {
        name: NAME.into(),
        display_name: DISPLAY.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: vec!["run-service".into()],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let service =
        manager.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)?;
    service.set_description(
        "Keeps the rconn agent running in the console session for remote access.",
    )?;
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 3600)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(5)
            };
            3
        ]),
    })?;
    service.start::<&str>(&[])?;
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        NAME,
        ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS,
    )?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        let _ = service.stop();
        for _ in 0..20 {
            if service.query_status()?.current_state == ServiceState::Stopped {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    service.delete()?;
    Ok(())
}

/// Write host.toml and the identity, then lock the data dir to SYSTEM + Administrators.
pub fn configure(
    server: &str,
    fingerprint: Option<&str>,
    identity_from: &std::path::Path,
) -> Result<()> {
    let dir = data_dir();
    std::fs::create_dir_all(dir.join("identity"))?;
    // Re-running configure must work even if an earlier run left odd ACLs behind:
    // reset the contents to inherit from the (locked) directory before writing.
    let _ = std::process::Command::new("icacls")
        .arg(dir.join("*"))
        .args(["/reset", "/T", "/Q"])
        .status();
    for f in ["host.id", "host.cert.pem", "host.key.pem"] {
        std::fs::copy(identity_from.join(f), dir.join("identity").join(f))
            .with_context(|| format!("copying {f}"))?;
    }
    let mut toml = format!("[server]\nurl = {server:?}\n");
    if let Some(fp) = fingerprint {
        toml.push_str(&format!("fingerprint = {fp:?}\n"));
    }
    std::fs::write(dir.join("host.toml"), toml)?;
    // Admin-only: users must not change the server, read the host key, or edit policy.
    // Lock the directory (inheritable grants for SYSTEM and Administrators only), then
    // reset everything inside to inherit from it. Applying /inheritance:r to the files
    // themselves would leave them with an empty ACL: (OI)(CI) grants don't apply to files.
    let icacls = |args: &[&std::ffi::OsStr]| -> Result<()> {
        let status = std::process::Command::new("icacls")
            .args(args)
            .status()
            .context("running icacls")?;
        anyhow::ensure!(status.success(), "icacls {args:?} failed");
        Ok(())
    };
    let os = |s: &'static str| std::ffi::OsStr::new(s);
    icacls(&[
        dir.as_os_str(),
        os("/inheritance:r"),
        os("/grant:r"),
        os("*S-1-5-18:(OI)(CI)F"),
        os("/grant:r"),
        os("*S-1-5-32-544:(OI)(CI)F"),
        os("/Q"),
    ])?;
    // Status for the local app: readable by Users (no secrets), writable by SYSTEM only.
    let sdir = status_dir();
    std::fs::create_dir_all(&sdir)?;
    icacls(&[
        sdir.as_os_str(),
        os("/inheritance:r"),
        os("/grant:r"),
        os("*S-1-5-18:(OI)(CI)F"),
        os("/grant:r"),
        os("*S-1-5-32-544:(OI)(CI)F"),
        os("/grant:r"),
        os("*S-1-5-32-545:(OI)(CI)RX"),
        os("/Q"),
    ])?;
    let children = dir.join("*");
    icacls(&[children.as_os_str(), os("/reset"), os("/T"), os("/Q")])?;
    Ok(())
}
