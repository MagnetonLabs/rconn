//! Privileged host service: Windows SYSTEM service / macOS LaunchDaemon. Phase 5 adds the
//! service loop (agent supervision across logon/logoff/lock); today it exposes the
//! session launcher the loop is built on.

#[cfg(windows)]
mod console;
mod paths;
#[cfg(windows)]
mod service;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about = "rconn host service")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Change one host setting (admin). Refused during a session, for keys locked by fleet
    /// policy, and for invalid values. Example: `set hidden_mode indicator`.
    Set { key: String, value: String },
    /// Clear one host setting back to its default (admin), e.g. `unset access_password`.
    Unset { key: String },
    /// Set the access password (admin): anyone with this computer's ID and the password can
    /// connect without an account. Read from the first line of stdin, never the command
    /// line; an empty line turns access by password off.
    SetAccessPassword,
    /// (Windows, admin) write host.toml + identity into ProgramData\rconn and lock it down.
    Configure {
        /// Signaling server URL (also the identity bound into host proofs).
        #[arg(long)]
        server: String,
        #[arg(long)]
        fingerprint: Option<String>,
        /// Directory from `rconn-signaling enroll-host`.
        #[arg(long)]
        identity_from: std::path::PathBuf,
    },
    /// (Windows, admin) register and start the `rconn-host` service.
    Install,
    /// (Windows, admin) stop and remove the service.
    Uninstall,
    /// Entry point used by the Service Control Manager.
    #[command(hide = true)]
    RunService,
    /// (Windows, as SYSTEM) start a program in the active console session with the SYSTEM
    /// token, so it can reach the lock screen and UAC desktop. Output goes to --log.
    LaunchInConsole {
        #[arg(long)]
        log: std::path::PathBuf,
        exe: std::path::PathBuf,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let filter =
        || tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    // Under the SCM there is no console: log to ProgramData\rconn\service.log.
    #[cfg(windows)]
    if matches!(cli.cmd, Cmd::RunService) {
        let dir = service::data_dir();
        let _ = std::fs::create_dir_all(&dir);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("service.log"))?;
        tracing_subscriber::fmt()
            .with_env_filter(filter())
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter()).init();
    }
    #[cfg(not(windows))]
    tracing_subscriber::fmt().with_env_filter(filter()).init();
    tracing::info!(
        protocol = rconn_core::PROTOCOL_VERSION,
        os = std::env::consts::OS,
        "rconn-host-service"
    );
    match cli.cmd {
        Cmd::LaunchInConsole { log, exe, args } => launch(&log, &exe, &args),
        Cmd::SetAccessPassword => {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let password = line.trim_end_matches(['\r', '\n']);
            let verifier = if password.is_empty() {
                String::new()
            } else {
                rconn_core::guest::make_verifier(password)?
            };
            let dir = paths::data_dir();
            rconn_core::settings::write_setting(
                rconn_core::settings::SettingsLayers {
                    system_file: Some(dir.join("host.toml")),
                    ..Default::default()
                },
                "access_password",
                &verifier,
                rconn_core::settings::Writer::Admin,
                dir.join("session.active").exists(),
            )?;
            println!(
                "access password {}",
                if verifier.is_empty() { "off" } else { "set" }
            );
            Ok(())
        }
        Cmd::Unset { key } => {
            let dir = paths::data_dir();
            rconn_core::settings::write_setting(
                rconn_core::settings::SettingsLayers {
                    system_file: Some(dir.join("host.toml")),
                    ..Default::default()
                },
                &key,
                "",
                rconn_core::settings::Writer::Admin,
                dir.join("session.active").exists(),
            )?;
            println!("{key} cleared");
            Ok(())
        }
        Cmd::Set { key, value } => {
            let dir = paths::data_dir();
            let session_active = dir.join("session.active").exists();
            rconn_core::settings::write_setting(
                rconn_core::settings::SettingsLayers {
                    system_file: Some(dir.join("host.toml")),
                    ..Default::default()
                },
                &key,
                &value,
                rconn_core::settings::Writer::Admin,
                session_active,
            )?;
            println!("{key} = {value}");
            Ok(())
        }
        Cmd::Configure {
            server,
            fingerprint,
            identity_from,
        } => windows_only(|| {
            #[cfg(windows)]
            service::configure(&server, fingerprint.as_deref(), &identity_from)?;
            #[cfg(not(windows))]
            let _ = (&server, &fingerprint, &identity_from);
            Ok(())
        }),
        Cmd::Install => windows_only(|| {
            #[cfg(windows)]
            service::install()?;
            Ok(())
        }),
        Cmd::Uninstall => windows_only(|| {
            #[cfg(windows)]
            service::uninstall()?;
            Ok(())
        }),
        Cmd::RunService => windows_only(|| {
            #[cfg(windows)]
            service::run()?;
            Ok(())
        }),
    }
}

#[cfg(windows)]
fn launch(log: &std::path::Path, exe: &std::path::Path, args: &[String]) -> Result<()> {
    let session = console::console_session()?;
    let child = console::launch_in_session(session, exe, args, log)?;
    println!("started pid {} in console session {session}", child.pid);
    std::mem::forget(child); // leave it running after we exit
    Ok(())
}

#[cfg(not(windows))]
fn launch(_: &std::path::Path, _: &std::path::Path, _: &[String]) -> Result<()> {
    anyhow::bail!("launch-in-console is Windows-only")
}

fn windows_only(f: impl FnOnce() -> Result<()>) -> Result<()> {
    if cfg!(windows) {
        f()
    } else {
        anyhow::bail!("this command is Windows-only (macOS uses a LaunchDaemon)")
    }
}
