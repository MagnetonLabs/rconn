//! A host for the admin portal's end-to-end test: joins a company with an enrollment
//! token, stays connected to signaling, and reports what the server does with it. No
//! screen, input or media.
//!
//!   cargo run -p rconn-peer --example portal_host -- <ws url> <server id> <token file> <identity dir> [label]
//!
//! Prints `enrolled <host id>`, `online`, then `evicted: <reason>` when the server drops
//! it, and `refused: <reason>` (exit 0) once the server no longer accepts it.

use anyhow::{Context, Result};
use rconn_core::fleet::HostKey;
use rconn_core::ids::HostId;
use rconn_peer::signaling::{HostCredentials, SignalingClient};
use rconn_peer::trust::ServerTrust;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [url, server_id, token_file, dir, rest @ ..] = args.as_slice() else {
        anyhow::bail!(
            "usage: portal_host <ws url> <server id> <token file> <identity dir> [label]"
        );
    };
    let label = rest.first().cloned().unwrap_or_else(|| "e2e-host".into());
    let dir = PathBuf::from(dir);
    let trust = ServerTrust::from_fingerprint(None);
    if !rconn_peer::identity::has_identity(&dir) {
        let token = std::fs::read_to_string(token_file).context("reading the token file")?;
        let id = rconn_peer::identity::enroll_with_token_into(&dir, url, &trust, token.trim())
            .await
            .context("enrolling")?;
        say(&format!("enrolled {id}"));
    }
    let read =
        |f: &str| std::fs::read_to_string(dir.join(f)).with_context(|| format!("reading {f}"));
    let host_id = HostId(read("host.id")?.trim().parse()?);
    let cert = read("host.cert.pem")?;
    let key = HostKey::from_pem(&read("host.key.pem")?)?;
    let mut was_online = false;
    loop {
        let creds = HostCredentials {
            server_id,
            host_id,
            key: &key,
            cert_pem: &cert,
            label: Some(label.clone()),
            os: Some("e2e test".into()),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            capabilities: vec![],
        };
        match SignalingClient::connect_host(url, &trust, creds).await {
            Ok(sig) => {
                was_online = true;
                say("online");
                let (_tx, mut rx) = sig.split();
                while rx.recv().await.is_some() {}
                say("evicted: connection closed by the server");
            }
            Err(e) if was_online => {
                say(&format!("refused: {e:#}"));
                return Ok(());
            }
            Err(e) => anyhow::bail!("could not connect: {e:#}"),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
