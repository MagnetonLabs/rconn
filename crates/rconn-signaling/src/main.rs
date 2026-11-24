//! `rconn-signaling`: self-hosted signaling server and its admin commands.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rconn_core::fleet::HostKey;
use rconn_core::ids::HostId;
use rconn_core::protocol::IceServer;
use rconn_signaling::auth::OperatorIdentity;
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::store::{DataDir, unhex32, write_private};
use rconn_signaling::turn_relay::{TurnConfig, TurnMinter, TurnRelay};
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
    /// Create the org's fleet CA and the server TLS identity in the data dir.
    Init {
        /// Hostnames for the TLS certificate (repeatable).
        #[arg(long = "hostname")]
        hostnames: Vec<String>,
    },
    /// Issue a host identity (key + cert) into `out`. Copy `out` to the host's
    /// admin-only config directory, then delete it here.
    EnrollHost {
        #[arg(long)]
        out: PathBuf,
    },
    /// Create an operator and print its token once (for automation; people use accounts).
    AddOperator,
    /// Create a login account. Password from RCONN_PASSWORD or the first line of stdin.
    AddAccount {
        #[arg(long)]
        username: String,
    },
    /// Refuse a host from its next connect onward (restart `serve` to apply).
    RevokeHost { host_id: uuid::Uuid },
    /// Run the server.
    Serve {
        #[arg(long, env = "RCONN_BIND", default_value = "0.0.0.0:8080")]
        bind: String,
        /// Identity hosts bind into their proofs: the exact URL in their server profile.
        #[arg(long, env = "RCONN_SERVER_ID")]
        server_id: String,
        /// Extra STUN URLs handed to clients (comma-separated).
        #[arg(long = "ice", env = "RCONN_ICE", value_delimiter = ',')]
        ice: Vec<String>,
        /// Run the built-in TURN relay, advertised at this public IP.
        #[arg(long, env = "RCONN_TURN_PUBLIC_IP")]
        turn_public_ip: Option<std::net::IpAddr>,
        #[arg(long, env = "RCONN_TURN_LISTEN", default_value = "0.0.0.0:3478")]
        turn_listen: std::net::SocketAddr,
        /// Relay port range, `min-max`. Open it (UDP) in the firewall.
        #[arg(long, env = "RCONN_TURN_PORTS", default_value = "49160-49260")]
        turn_ports: String,
        /// Use an external TURN server (e.g. coturn with `use-auth-secret`) instead of the
        /// built-in one: its `turn:` URL. Credentials are minted with `--turn-secret`.
        #[arg(long, env = "RCONN_TURN_URL", conflicts_with = "turn_public_ip")]
        turn_url: Option<String>,
        /// Shared secret for the external TURN server (coturn `static-auth-secret`).
        #[arg(
            long,
            env = "RCONN_TURN_SECRET",
            hide_env_values = true,
            requires = "turn_url"
        )]
        turn_secret: Option<String>,
        /// Serve plain HTTP/WebSocket (only behind a TLS-terminating proxy).
        #[arg(long, env = "RCONN_PLAIN")]
        plain: bool,
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
        Cmd::Init { hostnames } => {
            let ca = store.init(&hostnames)?;
            println!(
                "org {} initialised in {}",
                ca.org_id(),
                cli.data_dir.display()
            );
            if let Some(fp) = store.tls_fingerprint()? {
                println!("server fingerprint (pin this in host and viewer profiles): {fp}");
            }
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
        Cmd::AddAccount { username } => {
            let password = rconn_signaling::accounts::read_password()?;
            let id = store.add_account(&username, &password)?;
            println!("account {username} created (operator {id})");
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
            turn_public_ip,
            turn_listen,
            turn_ports,
            turn_url,
            turn_secret,
            plain,
        } => {
            let turn = match turn_public_ip {
                Some(ip) => Some(TurnConfig {
                    listen: turn_listen,
                    public_ip: ip,
                    relay_ports: parse_ports(&turn_ports)?,
                    shared_secret: store.turn_secret()?,
                }),
                None => None,
            };
            let external = match (turn_url, turn_secret) {
                (Some(url), Some(secret)) => Some(TurnMinter {
                    shared_secret: secret,
                    turn_urls: vec![url],
                    stun_urls: vec![],
                }),
                (Some(_), None) => anyhow::bail!("--turn-url needs --turn-secret"),
                _ => None,
            };
            serve(&store, &bind, server_id, ice, turn, external, plain).await?
        }
    }
    Ok(())
}

fn parse_ports(s: &str) -> Result<(u16, u16)> {
    let (a, b) = s
        .split_once('-')
        .context("--turn-ports must look like 49160-49260")?;
    let (a, b): (u16, u16) = (a.trim().parse()?, b.trim().parse()?);
    anyhow::ensure!(a <= b, "--turn-ports: min must not exceed max");
    Ok((a, b))
}

async fn serve(
    store: &DataDir,
    bind: &str,
    server_id: String,
    ice: Vec<String>,
    turn: Option<TurnConfig>,
    external_turn: Option<TurnMinter>,
    plain: bool,
) -> Result<()> {
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
    let relay = match &turn {
        Some(cfg) => {
            let relay = TurnRelay::start(cfg).await?;
            let public = std::net::SocketAddr::new(cfg.public_ip, relay.local_addr.port());
            auth = auth.with_turn(TurnMinter::for_relay(public, cfg.shared_secret.clone()));
            tracing::info!(listen = %relay.local_addr, %public, ports = ?cfg.relay_ports, "TURN relay up");
            Some(relay)
        }
        None => None,
    };
    let accounts = store.accounts()?;
    if !accounts.0.is_empty() {
        tracing::info!(accounts = accounts.0.len(), "password login enabled");
    }
    auth = auth.with_accounts(
        org_id,
        accounts,
        rconn_signaling::accounts::SessionTokens::new(store.session_key()?),
    );
    if let Some(minter) = external_turn {
        tracing::info!(urls = ?minter.turn_urls, "using external TURN");
        auth = auth.with_turn(minter);
    }
    let revoked = store.revoked()?;
    for h in &revoked {
        auth.revoke(*h);
    }

    let audit_path = store.path().join("audit.jsonl");
    let audit = rconn_signaling::audit::FileAudit::open(&audit_path)
        .with_context(|| format!("opening {}", audit_path.display()))?;
    let app = router(AppState::with_audit(Arc::new(auth), Arc::new(audit)));
    let addr: std::net::SocketAddr = bind.parse().with_context(|| format!("--bind {bind}"))?;
    let handle = axum_server::Handle::new();
    let h = handle.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        h.graceful_shutdown(Some(std::time::Duration::from_secs(5)));
    });
    match (plain, store.tls_pem()?) {
        (false, Some((cert, key))) => {
            let fp = rconn_core::tls::spki_fingerprint_pem(&cert)?;
            tracing::info!(%org_id, %server_id, %addr, fingerprint = %fp, revoked = revoked.len(), "serving wss");
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
                cert.into_bytes(),
                key.into_bytes(),
            )
            .await?;
            axum_server::bind_rustls(addr, tls)
                .handle(handle)
                .serve(app.into_make_service())
                .await?;
        }
        (false, None) => anyhow::bail!(
            "no tls.cert.pem in the data dir; run `init`, or pass --plain behind a TLS proxy"
        ),
        (true, _) => {
            tracing::warn!(%org_id, %server_id, %addr, revoked = revoked.len(), "serving PLAIN ws (expecting a TLS proxy in front)");
            axum_server::bind(addr)
                .handle(handle)
                .serve(app.into_make_service())
                .await?;
        }
    }
    if let Some(relay) = relay {
        relay.close().await?;
    }
    Ok(())
}
