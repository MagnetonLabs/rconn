//! Signaling server. Phase 2 adds the WebSocket endpoint, host challenge auth,
//! operator login and TURN credential minting.

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    tracing::info!(
        protocol = rconn_core::PROTOCOL_VERSION,
        "rconn-signaling (scaffold)"
    );
    Ok(())
}
