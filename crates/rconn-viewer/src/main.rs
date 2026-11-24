//! Viewer client (egui). Phase 3 adds decode + render.

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    tracing::info!(
        protocol = rconn_core::PROTOCOL_VERSION,
        "rconn-viewer (scaffold)"
    );
    Ok(())
}
