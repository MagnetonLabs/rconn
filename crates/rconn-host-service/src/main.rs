//! Privileged host service: Windows SYSTEM service / macOS LaunchDaemon. Phase 5 adds the
//! service loop (agent supervision across logon/logoff/lock); today it exposes the
//! session launcher the loop is built on.

#[cfg(windows)]
mod console;

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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    tracing::info!(
        protocol = rconn_core::PROTOCOL_VERSION,
        os = std::env::consts::OS,
        "rconn-host-service"
    );
    match Cli::parse().cmd {
        Cmd::LaunchInConsole { log, exe, args } => launch(&log, &exe, &args),
    }
}

#[cfg(windows)]
fn launch(log: &std::path::Path, exe: &std::path::Path, args: &[String]) -> Result<()> {
    let session = console::console_session()?;
    let pid = console::launch_in_session(session, exe, args, log)?;
    println!("started pid {pid} in console session {session}");
    Ok(())
}

#[cfg(not(windows))]
fn launch(_: &std::path::Path, _: &std::path::Path, _: &[String]) -> Result<()> {
    anyhow::bail!("launch-in-console is Windows-only")
}
