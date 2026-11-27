//! Viewer client. The egui UI arrives with video in phase 3; today `ping` opens a real
//! session to a host and measures data-channel round trips.

mod access;
mod files;
mod gui;
mod keymap;
mod oidc_login;
mod prefs;
mod video_render;
mod watch;

use anyhow::{Result, bail};
use bytes::BytesMut;
use clap::{Parser, Subcommand};
use rconn_core::ids::HostId;
use rconn_core::protocol::{Control, DataMessage};
use rconn_peer::rtc::PeerOptions;
use rconn_peer::session::open_viewer_session;
use rconn_peer::signaling::SignalingClient;
use rconn_peer::trust::ServerTrust;
use std::time::{Duration, Instant};
use webrtc::data_channel::DataChannelEvent;

#[derive(Parser)]
#[command(version, about = "rconn viewer")]
struct Cli {
    /// Signaling server. Default: the public Rconn server.
    #[arg(long, env = "RCONN_SERVER", default_value = rconn_core::DEFAULT_SERVER)]
    server: String,
    /// Operator token. Defaults to the one saved by `login` for this server.
    #[arg(long, env = "RCONN_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Pinned server key (`sha256:...`). Omit for servers with a public certificate.
    #[arg(long, env = "RCONN_FINGERPRINT")]
    fingerprint: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open a window on the host's screen with mouse and keyboard control.
    Connect {
        /// The computer: its id (signed in), or its 9-digit ID with the access password in
        /// the RCONN_ACCESS_PASSWORD environment variable (no account needed). Without a
        /// password the person at the computer is asked to accept.
        host: String,
        #[arg(long, env = "RCONN_OPENH264")]
        openh264: Option<std::path::PathBuf>,
        #[arg(long, default_value = "0.0.0.0:0")]
        udp: Vec<String>,
        #[arg(long)]
        relay_only: bool,
        /// Borderless, maximized window; Ctrl+Alt+Shift+M shows the toolbar.
        #[arg(long)]
        chromeless: bool,
        /// Don't play the host's sound.
        #[arg(long)]
        mute: bool,
        /// Name shown in the toolbar (default: the computer's ID).
        #[arg(long)]
        name: Option<String>,
    },
    /// Open a session and receive the host's screen (headless): print stream stats, and
    /// optionally save a PNG of the latest frame.
    Watch {
        /// As for `connect`: an id, or a 9-digit ID plus RCONN_ACCESS_PASSWORD.
        host: String,
        #[arg(long, default_value_t = 15)]
        seconds: u64,
        #[arg(long)]
        snapshot: Option<std::path::PathBuf>,
        /// Test aid: tap one key `SECONDS:HID` (HID usage, hex ok) after connecting,
        /// e.g. `5:0x29` for Escape.
        #[arg(long, value_parser = parse_tap)]
        tap_key: Option<(u64, u16)>,
        /// Test aid: after 2 s, ask the host to fit its screen to `WIDTHxHEIGHT`, as a
        /// viewer window of that size would.
        #[arg(long, value_parser = parse_size)]
        fit: Option<(u32, u32)>,
        /// Cisco openh264 library. Default: next to this executable, else target/openh264.
        #[arg(long, env = "RCONN_OPENH264")]
        openh264: Option<std::path::PathBuf>,
        #[arg(long, default_value = "0.0.0.0:0")]
        udp: Vec<String>,
        #[arg(long)]
        relay_only: bool,
    },
    /// Log in with an account; saves a session token for this server.
    /// Password from RCONN_PASSWORD or the first line of stdin.
    Login {
        /// Account name (password login). Omit with --oidc.
        #[arg(long, required_unless_present = "oidc")]
        username: Option<String>,
        /// Sign in with the server's identity provider in your browser.
        #[arg(long)]
        oidc: bool,
        /// Loopback port registered as the redirect URI with your IdP.
        #[arg(long, default_value_t = 8765)]
        redirect_port: u16,
    },
    /// List the computers that are online (ID, short code, name).
    Hosts,
    /// Open a session to a host and measure data-channel round-trip time.
    Ping {
        host: uuid::Uuid,
        #[arg(long, default_value_t = 20)]
        count: u32,
        #[arg(long, default_value = "0.0.0.0:0")]
        udp: Vec<String>,
        /// Force the TURN relay path.
        #[arg(long)]
        relay_only: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Watch {
            host,
            seconds,
            snapshot,
            tap_key,
            fit,
            openh264,
            udp,
            relay_only,
        } => {
            let lib = match openh264 {
                Some(p) => p.clone(),
                None => default_openh264()?,
            };
            let (host_id, guest) = target(host)?;
            let token = if guest.is_some() {
                String::new()
            } else {
                token_for(&cli)?
            };
            watch::run(
                &cli.server,
                &trust(&cli),
                &token,
                host_id,
                *seconds,
                snapshot.clone(),
                &lib,
                PeerOptions {
                    udp_addrs: udp.clone(),
                    relay_only: *relay_only,
                    video_codecs: rconn_media::caps::viewer_codecs(),
                },
                *tap_key,
                *fit,
                guest,
            )
            .await
        }
        Cmd::Connect {
            host,
            openh264,
            udp,
            relay_only,
            chromeless,
            mute,
            name,
        } => {
            let openh264 = match openh264 {
                Some(p) => p.clone(),
                None => default_openh264()?,
            };
            let (host_id, guest) = target(host)?;
            let args = gui::ConnectArgs {
                server: cli.server.clone(),
                fingerprint: cli.fingerprint.clone(),
                token: if guest.is_some() {
                    String::new()
                } else {
                    token_for(&cli)?
                },
                host: host_id,
                openh264,
                udp_addrs: udp.clone(),
                relay_only: *relay_only,
                chromeless: *chromeless,
                mute: *mute,
                name: name.clone().unwrap_or_else(|| match &guest {
                    Some(g) => g.id.clone(),
                    None => host_id.short_code(),
                }),
                guest,
                app_files: gui::AppFiles::from_env(),
            };
            // A session window is already open: add a tab to it instead of a new window.
            if gui::hand_off(&args) {
                return Ok(());
            }
            // eframe must own the main thread (macOS requirement); it blocks until closed.
            return gui::run(args);
        }
        Cmd::Login {
            username,
            oidc,
            redirect_port,
        } => {
            login(
                &cli.server,
                &trust(&cli),
                username.as_deref(),
                *oidc,
                *redirect_port,
            )
            .await
        }
        Cmd::Hosts => {
            let hosts =
                SignalingClient::list_hosts(&cli.server, &trust(&cli), &token_for(&cli)?).await?;
            for h in &hosts {
                println!(
                    "{}  {}  {}",
                    h.host_id.short_code(),
                    h.host_id,
                    h.label.as_deref().unwrap_or("-")
                );
            }
            println!("{} computer(s) online", hosts.len());
            Ok(())
        }
        Cmd::Ping {
            host,
            count,
            udp,
            relay_only,
        } => {
            ping(
                &cli.server,
                &trust(&cli),
                &token_for(&cli)?,
                HostId(*host),
                *count,
                udp.clone(),
                *relay_only,
            )
            .await
        }
    }
}

async fn ping(
    server: &str,
    trust: &ServerTrust,
    token: &str,
    host: HostId,
    count: u32,
    udp: Vec<String>,
    relay_only: bool,
) -> Result<()> {
    let t0 = Instant::now();
    let sig = SignalingClient::connect_operator(server, trust, token).await?;
    let session = open_viewer_session(
        sig,
        host,
        PeerOptions {
            udp_addrs: udp,
            relay_only,
            ..Default::default()
        },
    )
    .await?;
    let dc = session.data.clone();
    let mut rtts = Vec::new();
    let mut sent_at = Instant::now();
    let mut seq = 0u64;
    let send_ping = |n: u64| async move { DataMessage::Control(Control::Ping(n)).encode() };
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(ev) = dc.poll().await {
            match ev {
                DataChannelEvent::OnOpen => {
                    println!(
                        "connected in {:.0} ms (signaling + ICE + DTLS + SCTP)",
                        t0.elapsed().as_secs_f64() * 1e3
                    );
                    sent_at = Instant::now();
                    dc.send(BytesMut::from(&send_ping(seq).await?[..])).await?;
                }
                DataChannelEvent::OnMessage(msg) => {
                    if DataMessage::decode(&msg.data)? == DataMessage::Control(Control::Pong(seq)) {
                        rtts.push(sent_at.elapsed());
                        seq += 1;
                        if seq == u64::from(count) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        sent_at = Instant::now();
                        dc.send(BytesMut::from(&send_ping(seq).await?[..])).await?;
                    }
                }
                DataChannelEvent::OnClose => bail!("data channel closed early"),
                _ => {}
            }
        }
        Ok(())
    })
    .await;
    match result {
        Ok(r) => r?,
        Err(_) => bail!("timed out after {} of {count} pongs", rtts.len()),
    }
    session.close().await;

    rtts.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    println!(
        "{} pings: min {:.2} ms, median {:.2} ms, max {:.2} ms",
        rtts.len(),
        ms(rtts[0]),
        ms(rtts[rtts.len() / 2]),
        ms(rtts[rtts.len() - 1])
    );
    Ok(())
}

fn trust(cli: &Cli) -> ServerTrust {
    ServerTrust::from_fingerprint(cli.fingerprint.as_deref())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedToken {
    server: String,
    token: String,
    expires_at: u64,
}

fn token_path() -> Result<std::path::PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "rconn")
        .ok_or_else(|| anyhow::anyhow!("no home directory for the token cache"))?;
    Ok(dirs.config_dir().join("token.json"))
}

fn token_for(cli: &Cli) -> Result<String> {
    if let Some(t) = &cli.token {
        return Ok(t.clone());
    }
    let path = token_path()?;
    let saved: SavedToken =
        serde_json::from_str(&std::fs::read_to_string(&path).map_err(|_| {
            anyhow::anyhow!(
                "not logged in: run `rconn-viewer --server {} login --username <name>`",
                cli.server
            )
        })?)?;
    if saved.server != cli.server {
        bail!(
            "saved login is for {}, not {}; log in again",
            saved.server,
            cli.server
        );
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    if now >= saved.expires_at {
        bail!("saved login expired; log in again");
    }
    Ok(saved.token)
}

async fn login(
    server: &str,
    trust: &ServerTrust,
    username: Option<&str>,
    oidc: bool,
    port: u16,
) -> Result<()> {
    let (who, (token, expires_at)) = if oidc {
        let info = SignalingClient::login_options(server, trust)
            .await?
            .1
            .ok_or_else(|| anyhow::anyhow!("this server has no identity provider configured"))?;
        let id_token = oidc_login::id_token(&info.issuer, &info.client_id, port).await?;
        (
            "your identity provider account".to_owned(),
            SignalingClient::oidc_login(server, trust, &id_token).await?,
        )
    } else {
        let username = username.ok_or_else(|| anyhow::anyhow!("--username is required"))?;
        let password = read_password()?;
        (
            username.to_owned(),
            SignalingClient::password_login(server, trust, username, &password).await?,
        )
    };
    let username = who.as_str();
    let path = token_path()?;
    std::fs::create_dir_all(path.parent().expect("token path has a parent"))?;
    let saved = SavedToken {
        server: server.to_owned(),
        token,
        expires_at,
    };
    std::fs::write(&path, serde_json::to_string(&saved)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    println!("logged in as {username}; token saved to {}", path.display());
    Ok(())
}

fn read_password() -> Result<String> {
    if let Ok(p) = std::env::var("RCONN_PASSWORD") {
        return Ok(p);
    }
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

fn default_openh264() -> Result<std::path::PathBuf> {
    let name = rconn_media::cisco::current_binary_name()
        .ok_or_else(|| anyhow::anyhow!("no Cisco build for this platform"))?;
    let exe = std::env::current_exe()?;
    let beside = exe.with_file_name(name);
    if beside.exists() {
        return Ok(beside);
    }
    // macOS app bundle: Contents/Resources, kept byte-identical to Cisco's signed build
    // (the openh264 loader checks Cisco's hash, so the bundle must not re-sign it).
    if let Some(resources) = exe
        .parent()
        .and_then(|d| d.parent())
        .map(|c| c.join("Resources").join(name))
        && resources.exists()
    {
        return Ok(resources);
    }
    rconn_media::cisco::local_binary_path()
        .ok_or_else(|| anyhow::anyhow!("openh264 not found; pass --openh264"))
}

/// Which computer, and how: a full id means "signed in"; anything else is an ID to use with
/// the access password from [`access::PASSWORD_ENV`], or, with none, by asking the person
/// at the computer to accept.
fn target(host: &str) -> Result<(HostId, Option<access::GuestLogin>)> {
    let password = std::env::var(access::PASSWORD_ENV)
        .ok()
        .filter(|p| !p.is_empty());
    match (uuid::Uuid::parse_str(host.trim()), password) {
        (Ok(id), None) => Ok((HostId(id), None)),
        (id, password) => Ok((
            HostId(id.unwrap_or_default()),
            Some(access::GuestLogin {
                id: host.trim().to_owned(),
                password: password.unwrap_or_default(),
            }),
        )),
    }
}

fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s.split_once('x').ok_or("expected WIDTHxHEIGHT")?;
    Ok((
        w.parse().map_err(|e| format!("width: {e}"))?,
        h.parse().map_err(|e| format!("height: {e}"))?,
    ))
}

fn parse_tap(s: &str) -> Result<(u64, u16), String> {
    let (secs, hid) = s.split_once(':').ok_or("expected SECONDS:HID")?;
    let hid = hid.trim_start_matches("0x");
    Ok((
        secs.parse().map_err(|e| format!("{e}"))?,
        u16::from_str_radix(hid, 16).map_err(|e| format!("{e}"))?,
    ))
}
