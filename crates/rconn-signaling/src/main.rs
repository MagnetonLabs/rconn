//! `rconn-signaling`: self-hosted signaling server and its admin commands.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rconn_core::fleet::HostKey;
use rconn_core::ids::HostId;
use rconn_core::protocol::IceServer;
use rconn_signaling::db::{DB_FILE, Db, Revoked};
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::hosts::DbInventory;
use rconn_signaling::import;
use rconn_signaling::public::{EnrollLimits, PublicHosts};
use rconn_signaling::secrets::default_master_key_path;
use rconn_signaling::store::{DataDir, write_private};
use rconn_signaling::turn_relay::{RelayUsage, TurnConfig, TurnMinter, TurnRelay};
use rconn_signaling::{AppState, router};
use std::fs;
use std::path::{Path, PathBuf};
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
    /// The server master key that seals org CA keys in the database. Keep it outside the
    /// data dir and its backups. Default: `master.key` next to the data dir.
    #[arg(long, env = "RCONN_MASTER_KEY_FILE", global = true)]
    master_key_file: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

// The CLI enum is built once per process, so the large `Serve` variant costs nothing.
#[allow(clippy::large_enum_variant)]
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
    /// Refuse a host from now on (a running server evicts it within seconds).
    RevokeHost { host_id: uuid::Uuid },
    /// Print the TLS pin (`sha256:…`) for host and viewer server profiles, or say why none
    /// is needed.
    Pin {
        /// The server runs plain behind a TLS proxy (e.g. Caddy).
        #[arg(long, env = "RCONN_PLAIN")]
        plain: bool,
    },
    /// Manage the org.
    Org {
        #[command(subcommand)]
        cmd: OrgCmd,
    },
    /// Run the server.
    Serve(ServeArgs),
}

#[derive(Subcommand)]
enum OrgCmd {
    /// Make the person with this email the owner of the server's original org (the one
    /// from `init`). Other owners become admins.
    SetOwner {
        #[arg(long)]
        email: String,
        /// Give this existing login account the email, so the owner signs in with it.
        #[arg(long)]
        account: Option<String>,
    },
}

#[derive(clap::Args)]
struct ServeArgs {
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
    /// OIDC issuer URL for operator login (e.g. https://login.example.com).
    #[arg(long, env = "RCONN_OIDC_ISSUER", requires = "oidc_client_id")]
    oidc_issuer: Option<String>,
    /// OIDC client id registered for the viewer (public client, PKCE).
    #[arg(long, env = "RCONN_OIDC_CLIENT_ID")]
    oidc_client_id: Option<String>,
    /// Also require a verified email in one of these domains (comma-separated).
    #[arg(long, env = "RCONN_OIDC_ALLOWED_DOMAINS", value_delimiter = ',')]
    oidc_allowed_domain: Vec<String>,
    /// Serve plain HTTP/WebSocket (only behind a TLS-terminating proxy).
    #[arg(long, env = "RCONN_PLAIN")]
    plain: bool,
    /// Public server (AnyDesk-style, no accounts needed): turns on account-less
    /// enrollment and the relay caps below unless they are set explicitly.
    #[arg(long, env = "RCONN_PUBLIC")]
    public: bool,
    /// Give IDs to computers without an account (owned by nobody: every session needs
    /// their access password). Default: on with --public, else off.
    #[arg(long, env = "RCONN_ANONYMOUS_ENROLL")]
    anonymous_enroll: Option<bool>,
    /// Account-less enrollments allowed per source address (IPv6: per /64) per hour.
    #[arg(long, env = "RCONN_ENROLL_PER_IP_PER_HOUR", default_value_t = 5)]
    enroll_per_ip_per_hour: usize,
    /// Account-less enrollments allowed per hour, server-wide.
    #[arg(long, env = "RCONN_ENROLL_PER_HOUR", default_value_t = 1000)]
    enroll_per_hour: usize,
    /// Header holding the client's address, set by the proxy in front (its last entry is
    /// used). Default: `x-forwarded-for` with --plain, none otherwise; `none` turns it off.
    #[arg(long, env = "RCONN_CLIENT_IP_HEADER")]
    client_ip_header: Option<String>,
    /// Built-in relay: megabytes one allocation may relay before it is closed; 0 means no
    /// limit. Default: 4096 with --public, else 0.
    #[arg(long, env = "RCONN_TURN_SESSION_CAP_MB")]
    turn_session_cap_mb: Option<u64>,
    /// Built-in relay: gigabytes relayed per calendar month (UTC) before relaying stops
    /// until the next month; 0 means no limit. Default: 1000 with --public, else 0.
    #[arg(long, env = "RCONN_TURN_MONTHLY_CAP_GB")]
    turn_monthly_cap_gb: Option<u64>,
    /// The admin portal's public origin, e.g. https://app.rconn.app. Sign-in links point
    /// here and portal requests must come from it. Without it, sign-in links are off.
    #[arg(long, env = "RCONN_PORTAL_URL")]
    portal_url: Option<String>,
    /// Let anyone create a company in the portal (hosted sign-up).
    #[arg(long, env = "RCONN_ALLOW_SIGNUP")]
    allow_signup: bool,
    /// Sender of portal email, e.g. "Rconn <login@rconn.app>". Mail goes out through
    /// Resend when RESEND_API_KEY is set; otherwise it is written to the log.
    #[arg(long, env = "RCONN_MAIL_FROM")]
    mail_from: Option<String>,
    /// The current host release (e.g. 0.1.14): the portal marks computers running an
    /// older version "needs update". Default: this server's version.
    #[arg(long, env = "RCONN_HOST_RELEASE")]
    host_release: Option<String>,
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
    let key_path = cli
        .master_key_file
        .clone()
        .unwrap_or_else(|| default_master_key_path(&cli.data_dir));

    match cli.cmd {
        Cmd::Init { hostnames } => {
            if cli.data_dir.join(DB_FILE).exists()
                && let Some(org) = Db::open_in(&cli.data_dir).await?.legacy_org().await?
            {
                anyhow::bail!("{} already holds org {}", cli.data_dir.display(), org.id);
            }
            let ca = store.init(&hostnames)?;
            import::open(&store, &key_path).await?;
            println!(
                "org {} initialised in {} (master key: {})",
                ca.org_id(),
                cli.data_dir.display(),
                key_path.display()
            );
            if let Some(fp) = store.tls_fingerprint()? {
                println!("server fingerprint (pin this in host and viewer profiles): {fp}");
            }
        }
        Cmd::EnrollHost { out } => {
            let (db, master, _) = import::open(&store, &key_path).await?;
            let org = db.require_legacy_org().await?;
            let ca = db.org_ca(org.id, &master).await?;
            let host_id = HostId::new();
            let key = HostKey::generate()?;
            let cert = ca.issue_host_cert(host_id, &key)?;
            fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
            fs::write(out.join("host.id"), host_id.to_string())?;
            fs::write(out.join("host.cert.pem"), &cert)?;
            write_private(&out.join("host.key.pem"), &key.to_pem())?;
            db.host_enrolled(org.id, host_id, None, rconn_signaling::db::now())
                .await?;
            println!("host {host_id} enrolled into {}", out.display());
        }
        Cmd::AddAccount { username } => {
            let password = rconn_signaling::accounts::read_password()?;
            let (db, _, _) = import::open(&store, &key_path).await?;
            let org = db.require_legacy_org().await?;
            let id = db.add_account(org.id, &username, &password).await?;
            println!("account {username} created (operator {id})");
        }
        Cmd::AddOperator => {
            let (db, _, _) = import::open(&store, &key_path).await?;
            let org = db.require_legacy_org().await?;
            let (id, token) = db.add_operator(org.id).await?;
            println!("operator {id}\ntoken (shown once): {token}");
        }
        Cmd::RevokeHost { host_id } => {
            let (db, _, _) = import::open(&store, &key_path).await?;
            let public =
                rconn_signaling::public::registry_ids(&cli.data_dir.join("public-hosts.jsonl"))?;
            let host = HostId(host_id);
            match db.revoke_host(host, public.contains(&host)).await? {
                Revoked::Already => println!("host {host_id} was already revoked"),
                Revoked::PublicHost | Revoked::OrgHost(_) => println!(
                    "host {host_id} revoked; a running server disconnects it within seconds"
                ),
            }
        }
        Cmd::Pin { plain } => match store.tls_fingerprint()? {
            Some(fp) if !plain => {
                println!("{fp}");
                eprintln!("pin this in host and viewer server profiles");
            }
            Some(_) => println!(
                "no pin needed: the server runs behind a TLS proxy (--plain), so clients check \
                 the proxy's public certificate"
            ),
            None => println!(
                "no pin needed: there is no tls.cert.pem, so the server runs behind a TLS proxy \
                 (e.g. Caddy) and clients check its public certificate"
            ),
        },
        Cmd::Org {
            cmd: OrgCmd::SetOwner { email, account },
        } => {
            let (db, _, _) = import::open(&store, &key_path).await?;
            let org = db.require_legacy_org().await?;
            let (owner, demoted) = db.set_owner(org.id, &email, account.as_deref()).await?;
            println!("{email} (user {owner}) owns org {}", org.id);
            for d in demoted {
                println!("previous owner {d} is now an admin");
            }
        }
        Cmd::Serve(args) => serve(&store, &key_path, args).await?,
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

async fn serve(store: &DataDir, key_path: &Path, args: ServeArgs) -> Result<()> {
    let ServeArgs {
        bind,
        server_id,
        ice,
        turn_public_ip,
        turn_listen,
        turn_ports,
        turn_url,
        turn_secret,
        oidc_issuer,
        oidc_client_id,
        oidc_allowed_domain,
        plain,
        public,
        anonymous_enroll,
        enroll_per_ip_per_hour,
        enroll_per_hour,
        client_ip_header,
        turn_session_cap_mb,
        turn_monthly_cap_gb,
        portal_url,
        allow_signup,
        mail_from,
        host_release,
    } = args;
    if let Some(u) = &portal_url {
        anyhow::ensure!(
            (u.starts_with("https://") || u.starts_with("http://localhost"))
                && u.trim_end_matches('/').matches('/').count() == 2,
            "--portal-url must be an https origin without a path, like https://app.rconn.app"
        );
    }
    let mailer = rconn_signaling::mail::from_env(mail_from.as_deref())?;
    let bind = bind.as_str();
    let nonzero = |v: u64| (v > 0).then_some(v);
    let session_cap = nonzero(turn_session_cap_mb.unwrap_or(if public { 4096 } else { 0 }))
        .map(|mb| mb.saturating_mul(1024 * 1024));
    let monthly_cap = nonzero(turn_monthly_cap_gb.unwrap_or(if public { 1000 } else { 0 }))
        .map(|gb| gb.saturating_mul(1024 * 1024 * 1024));
    let usage = Arc::new(RelayUsage::new(
        monthly_cap,
        Some(store.path().join("relay-usage.json")),
    ));
    let turn = match turn_public_ip {
        Some(ip) => Some(TurnConfig {
            listen: turn_listen,
            public_ip: ip,
            relay_ports: parse_ports(&turn_ports)?,
            shared_secret: store.turn_secret()?,
            per_allocation_bytes: session_cap,
            usage: usage.clone(),
        }),
        None => None,
    };
    let external_turn = match (turn_url, turn_secret) {
        (Some(url), Some(secret)) => Some(TurnMinter {
            shared_secret: secret,
            turn_urls: vec![url],
            stun_urls: vec![],
            usage: None,
        }),
        (Some(_), None) => anyhow::bail!("--turn-url needs --turn-secret"),
        _ => None,
    };
    let oidc = oidc_issuer
        .zip(oidc_client_id)
        .map(|(i, c)| (i, c, oidc_allowed_domain));
    let (db, master, _) = import::open(store, key_path).await?;
    let org_id = db.require_legacy_org().await?.id;
    let ca = db.org_ca(org_id, &master).await?;
    // Org CAs, operator tokens, accounts and revocations come from the database (below).
    // Roles decide who lists and connects to org computers (checked on every request).
    let mut auth = FleetAuthenticator::new(server_id.clone()).with_db(db.clone());
    // Computers without an account: a separate CA (its own org, which no operator is in).
    // Its hosts keep connecting even after enrollment is turned off.
    let anonymous_enroll = anonymous_enroll.unwrap_or(public);
    let public_hosts = match store.public_ca(anonymous_enroll)? {
        Some(public_ca) => {
            auth = auth.with_org_ca(public_ca.org_id(), public_ca.cert_pem());
            let limits = anonymous_enroll.then_some(EnrollLimits {
                per_source_per_hour: enroll_per_ip_per_hour,
                global_per_hour: enroll_per_hour,
            });
            let hosts = PublicHosts::new(
                public_ca,
                limits,
                Some(store.path().join("public-hosts.jsonl")),
            )?;
            tracing::info!(
                enrollment = ?limits,
                hosts = hosts.len(),
                "computers without an account accepted"
            );
            Some(hosts)
        }
        None => None,
    };
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
            auth = auth.with_turn(
                TurnMinter::for_relay(public, cfg.shared_secret.clone()).with_usage(usage.clone()),
            );
            tracing::info!(
                listen = %relay.local_addr, %public, ports = ?cfg.relay_ports,
                session_cap_bytes = ?session_cap, monthly_cap_bytes = ?monthly_cap,
                used_this_month = usage.used(), "TURN relay up"
            );
            // Keep the month's count across restarts.
            let usage = usage.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    usage.save();
                }
            });
            Some(relay)
        }
        None => None,
    };
    auth = auth.with_accounts(
        org_id,
        Default::default(),
        rconn_signaling::accounts::SessionTokens::new(store.session_key()?),
    );
    if let Some((issuer, client_id, domains)) = oidc {
        let verifier =
            rconn_signaling::oidc::OidcVerifier::discover(&issuer, &client_id, org_id, domains)
                .await?;
        tracing::info!(%issuer, %client_id, "OIDC login enabled");
        auth = auth.with_oidc(verifier);
    }
    if let Some(minter) = external_turn {
        tracing::info!(urls = ?minter.turn_urls, "using external TURN");
        auth = auth.with_turn(minter);
    }
    let audit_path = store.path().join("audit.jsonl");
    let file_audit = rconn_signaling::audit::FileAudit::open(&audit_path)
        .with_context(|| format!("opening {}", audit_path.display()))?;
    let mut db_audit = rconn_signaling::audit_db::DbAudit::spawn(db.clone());
    if let Some(hosts) = &public_hosts {
        db_audit = db_audit.ignoring_org(hosts.org_id());
    }
    let audit: Arc<dyn rconn_signaling::audit::AuditSink> =
        Arc::new(rconn_signaling::audit::TeeAudit(vec![
            Arc::new(file_audit),
            Arc::new(db_audit),
        ]));
    let auth = Arc::new(auth);
    let state = AppState::with_audit(auth.clone(), audit.clone());
    // Signed-in operators may turn their own computers into hosts from the app.
    state.allow_enrollment(org_id, ca);
    if let Some(hosts) = public_hosts {
        state.allow_public_hosts(hosts);
    }
    let header = match client_ip_header.as_deref().map(str::trim) {
        Some("none") | Some("") => None,
        Some(h) => Some(h.to_owned()),
        None => plain.then(|| "x-forwarded-for".to_owned()),
    };
    if let Some(h) = header {
        let name = axum::http::HeaderName::try_from(h.as_str())
            .with_context(|| format!("--client-ip-header {h}"))?;
        tracing::info!(header = %name, "client addresses read from the proxy's header");
        state.trust_client_ip_header(name);
    }
    state.record_hosts_in(Arc::new(DbInventory::spawn(db.clone())));
    state.use_policies(Arc::new(
        rconn_signaling::portal::policies::DbPolicies::new(db.clone()),
    ));
    // Load the database before accepting anyone, then follow its changes (and import
    // legacy files that old scripts may still write) without a restart.
    let mut seen = rconn_signaling::reload::Seen::default();
    rconn_signaling::reload::reload_once(&db, &auth, &state, &mut seen).await?;
    let revoked = db.revoked_hosts().await?.len();
    let master = Arc::new(master);
    // Computers join a company with an enrollment token made in the portal.
    state.allow_token_enrollment(Arc::new(
        rconn_signaling::portal::computers::DbTokenEnroller::new(
            db.clone(),
            master.clone(),
            audit.clone(),
        ),
    ));
    let portal = rconn_signaling::portal::PortalState::new(
        db.clone(),
        state.clone(),
        master.clone(),
        mailer,
        audit,
        rconn_signaling::portal::PortalConfig {
            portal_url: portal_url.map(|u| u.trim_end_matches('/').to_owned()),
            allow_signup,
            host_release: host_release.unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned()),
            ..Default::default()
        },
    );
    rconn_signaling::portal::spawn_housekeeping(db.clone());
    {
        let (store, auth, state) = (DataDir::new(store.path()), auth.clone(), state.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                if import::pending(&store)
                    && let Err(e) = import::import(&db, &store, &master).await
                {
                    tracing::error!(error = %format!("{e:#}"), "importing data-dir files failed");
                }
                if let Err(e) =
                    rconn_signaling::reload::reload_once(&db, &auth, &state, &mut seen).await
                {
                    tracing::error!(error = %format!("{e:#}"), "reload failed; keeping previous config");
                }
            }
        });
    }
    let app = router(state).merge(rconn_signaling::portal::router(portal));
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
            tracing::info!(%org_id, %server_id, %addr, fingerprint = %fp, revoked, "serving wss");
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
                cert.into_bytes(),
                key.into_bytes(),
            )
            .await?;
            axum_server::bind_rustls(addr, tls)
                .handle(handle)
                .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
                .await?;
        }
        (false, None) => anyhow::bail!(
            "no tls.cert.pem in the data dir; run `init`, or pass --plain behind a TLS proxy"
        ),
        (true, _) => {
            tracing::warn!(%org_id, %server_id, %addr, revoked, "serving PLAIN ws (expecting a TLS proxy in front)");
            axum_server::bind(addr)
                .handle(handle)
                .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
                .await?;
        }
    }
    if let Some(relay) = relay {
        relay.close().await?;
    }
    usage.save();
    Ok(())
}
