//! `rconn-signaling`: self-hosted signaling server and its admin commands.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rconn_core::fleet::HostKey;
use rconn_core::ids::HostId;
use rconn_core::protocol::IceServer;
use rconn_signaling::auth::OperatorIdentity;
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::store::{DataDir, unhex32, write_private};
use rconn_signaling::{AppState, router};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(version, about = "rconn signaling server")]
struct Cli {
    /// Directory holding the fleet CA, operators and revocations.
    #[arg(
        long,
        env = "RCONN_DATA_DIR",
        default_value = "./rconn-data",
        global = true
    )]
    data_dir: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the org's fleet CA in the data dir.
    Init,
    /// Issue a host identity (key + cert) into `out`. Copy `out` to the host's
    /// admin-only config directory, then delete it here.
    EnrollHost {
        #[arg(long)]
        out: PathBuf,
    },
    /// Create an operator and print its token once.
    AddOperator,
    /// Refuse a host from its next connect onward (restart `serve` to apply).
    RevokeHost { host_id: uuid::Uuid },
    /// Run the server.
    Serve {
        #[arg(long, env = "RCONN_BIND", default_value = "0.0.0.0:8080")]
        bind: String,
        /// Identity hosts bind into their proofs: the exact URL in their server profile.
        #[arg(long, env = "RCONN_SERVER_ID")]
        server_id: String,
        /// STUN/TURN URLs handed to clients (repeatable).
        #[arg(long = "ice", env = "RCONN_ICE", value_delimiter = ',')]
        ice: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let store = DataDir::new(&cli.data_dir);

    match cli.cmd {
        Cmd::Init => {
            let ca = store.init()?;
            println!(
                "org {} initialised in {}",
                ca.org_id(),
                cli.data_dir.display()
            );
        }
        Cmd::EnrollHost { out } => {
            let ca = store.load_ca()?;
            let host_id = HostId::new();
            let key = HostKey::generate()?;
            let cert = ca.issue_host_cert(host_id, &key)?;
            fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
            fs::write(out.join("host.id"), host_id.to_string())?;
            fs::write(out.join("host.cert.pem"), &cert)?;
            write_private(&out.join("host.key.pem"), &key.to_pem())?;
            println!("host {host_id} enrolled into {}", out.display());
        }
        Cmd::AddOperator => {
            let (id, token) = store.add_operator()?;
            println!("operator {id}\ntoken (shown once): {token}");
        }
        Cmd::RevokeHost { host_id } => {
            store.revoke(HostId(host_id))?;
            println!("host {host_id} revoked; restart serve to apply");
        }
        Cmd::Serve {
            bind,
            server_id,
            ice,
        } => serve(&store, &bind, server_id, ice).await?,
    }
    Ok(())
}

async fn serve(store: &DataDir, bind: &str, server_id: String, ice: Vec<String>) -> Result<()> {
    let ca = store.load_ca()?;
    let org_id = ca.org_id();
    let mut auth = FleetAuthenticator::new(server_id.clone()).with_org_ca(org_id, ca.cert_pem());
    for (digest_hex, operator_id) in store.operators()? {
        let digest = unhex32(&digest_hex).context("corrupt operators.json")?;
        auth = auth.with_operator_token_digest(
            digest,
            OperatorIdentity {
                operator_id,
                org_id,
            },
        );
    }
    if !ice.is_empty() {
        auth = auth.with_ice_servers(vec![IceServer {
            urls: ice,
            username: None,
            credential: None,
        }]);
    }
    let revoked = store.revoked()?;
    for h in &revoked {
        auth.revoke(*h);
    }

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    tracing::info!(%org_id, %server_id, addr = %listener.local_addr()?, revoked = revoked.len(), "serving");
    axum::serve(listener, router(AppState::new(Arc::new(auth))))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
