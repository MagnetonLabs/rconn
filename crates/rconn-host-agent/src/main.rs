//! Per-session host agent: capture, input injection, clipboard, loopback audio.

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    tracing::info!(
        protocol = rconn_core::PROTOCOL_VERSION,
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        openh264 = rconn_media::cisco::current_binary_name(),
        "rconn-host-agent (scaffold)"
    );
    Ok(())
}
