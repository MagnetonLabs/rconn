//! Privileged host service: Windows SYSTEM service / macOS LaunchDaemon.
//! Phase 5 adds session tracking and agent launch; settings lock lives here.

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    tracing::info!(
        protocol = rconn_core::PROTOCOL_VERSION,
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        "rconn-host-service (scaffold)"
    );
    Ok(())
}
