//! Background worker: talks to the server and the local host so the window never blocks.

use crate::home::{Home, Request, resolve};
use crate::profile::{self, SavedToken};
use rconn_core::ids::HostId;
use rconn_core::status::HostStatus;
use rconn_peer::signaling::SignalingClient;
use rconn_peer::trust::ServerTrust;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

pub type Shared = Arc<Mutex<Home>>;

/// Process ids of open session windows (so quitting rconn closes them too).
static SESSIONS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Close every session window this app opened.
pub fn end_all_sessions() {
    for pid in SESSIONS
        .lock()
        .map(|mut v| std::mem::take(&mut *v))
        .unwrap_or_default()
    {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let _ = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F"])
                .creation_flags(0x0800_0000)
                .status();
        }
        #[cfg(not(windows))]
        {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
    }
}

#[derive(Clone)]
pub struct Paths {
    /// Per-user app config (profile, token, viewer log).
    pub config: PathBuf,
    /// The local host's status (written by the host agent).
    pub status_file: PathBuf,
    /// The local host's admin-only settings folder.
    pub host_data: PathBuf,
    pub viewer_exe: PathBuf,
    pub settings_exe: PathBuf,
}

impl Paths {
    pub fn installed() -> Self {
        let beside = |name: &str| {
            std::env::current_exe()
                .unwrap_or_default()
                .with_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX))
        };
        let config = profile::config_dir();
        // macOS: the app sets the host up per user (see machost); Windows: the installer.
        let (status_file, host_data) = if cfg!(target_os = "macos") {
            (config.join("host").join("status.json"), config.join("host"))
        } else {
            (
                rconn_host_settings::platform::status_file(),
                rconn_host_settings::platform::default_data_dir(),
            )
        };
        Self {
            config,
            status_file,
            host_data,
            viewer_exe: beside("rconn-viewer"),
            settings_exe: beside("rconn-settings"),
        }
    }
}

struct Worker {
    shared: Shared,
    ctx: egui::Context,
    paths: Paths,
}

/// Start the worker thread; returns where the window sends its requests.
pub fn spawn(shared: Shared, ctx: egui::Context, paths: Paths) -> mpsc::UnboundedSender<Request> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let w = Arc::new(Worker { shared, ctx, paths });
    std::thread::Builder::new()
        .name("rconn-app-worker".into())
        .spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(5));
                let mut n: u64 = 0;
                loop {
                    tokio::select! {
                        req = rx.recv() => {
                            let Some(req) = req else { break };
                            let w = w.clone();
                            tokio::spawn(async move { w.handle(req).await });
                        }
                        _ = tick.tick() => {
                            w.refresh_local(n.is_multiple_of(3));
                            if n.is_multiple_of(3) && w.home(|h| h.signed_in) {
                                let w = w.clone();
                                tokio::spawn(async move { w.fetch_computers(false).await; });
                            }
                            n += 1;
                        }
                    }
                }
            });
        })
        .expect("spawn worker thread");
    tx
}

impl Worker {
    fn home<R>(&self, f: impl FnOnce(&mut Home) -> R) -> R {
        let r = f(&mut self.shared.lock().expect("home state"));
        self.ctx.request_repaint();
        r
    }

    fn notice(&self, text: impl Into<String>, error: bool) {
        let text = text.into();
        self.home(|h| {
            h.busy = None;
            h.notice = Some((text, error));
        });
    }

    fn server(&self) -> (String, ServerTrust, Option<String>) {
        self.home(|h| {
            let fp = h.profile.fingerprint().map(str::to_owned);
            (
                h.profile.server.clone(),
                ServerTrust::from_fingerprint(fp.as_deref()),
                fp,
            )
        })
    }

    fn token(&self) -> Option<String> {
        let server = self.home(|h| h.profile.server.clone());
        profile::token_for(&self.paths.config, &server, profile::now_unix())
    }

    /// Re-read this computer's status (and, every few ticks, its settings).
    fn refresh_local(&self, settings_too: bool) {
        let status = HostStatus::read(&self.paths.status_file);
        // macOS: installed once the app wrote host.toml; Windows: the admin-only folder
        // exists (its contents aren't visible to ordinary users).
        let installed = if cfg!(target_os = "macos") {
            self.paths.host_data.join("host.toml").exists()
        } else {
            self.paths.host_data.exists()
        };
        let settings = (settings_too && installed)
            .then(|| rconn_host_settings::model::Model::load(&self.paths.host_data));
        let signed_in = self.token().is_some();
        self.home(|h| {
            h.now = profile::now_unix();
            h.this = status;
            h.adopt_host_server();
            if h.signed_in && !signed_in {
                h.computers = None;
            }
            h.signed_in = signed_in;
            if !installed {
                h.host_settings = None;
            } else if let Some(fresh) = settings {
                // Don't throw away edits in progress.
                if h.host_settings
                    .as_ref()
                    .is_none_or(|m| m.changes().is_empty())
                {
                    h.host_settings = Some(fresh);
                }
            }
        });
    }

    async fn fetch_computers(&self, report: bool) -> Option<Vec<rconn_core::protocol::HostInfo>> {
        let token = self.token()?;
        let (server, trust, _) = self.server();
        match SignalingClient::list_hosts(&server, &trust, &token).await {
            Ok(list) => {
                self.home(|h| h.computers = Some(list.clone()));
                Some(list)
            }
            Err(e) => {
                if report {
                    self.notice(format!("Couldn't load computers: {e:#}"), true);
                }
                None
            }
        }
    }

    async fn handle(&self, req: Request) {
        match req {
            Request::SignIn { username, password } => {
                self.home(|h| h.busy = Some("Signing in…".into()));
                let (server, trust, _) = self.server();
                match SignalingClient::password_login(&server, &trust, &username, &password).await {
                    Ok((token, expires_at)) => {
                        if let Err(e) = profile::save_token(
                            &self.paths.config,
                            &SavedToken {
                                server,
                                token,
                                expires_at,
                            },
                        ) {
                            return self.notice(
                                format!("Signed in, but couldn't save the login: {e}"),
                                true,
                            );
                        }
                        self.home(|h| {
                            h.profile.username = username.clone();
                            h.signed_in = true;
                            h.tab = crate::home::Tab::Computers;
                            let _ = h.profile.save(&self.paths.config);
                        });
                        self.notice(format!("Signed in as {username}"), false);
                        self.fetch_computers(true).await;
                    }
                    Err(e) => self.notice(format!("Sign-in failed: {e:#}"), true),
                }
            }
            Request::SignInSso => {
                self.home(|h| h.busy = Some("Finish signing in in your browser…".into()));
                let (server, _, fp) = self.server();
                let viewer = self.paths.viewer_exe.clone();
                let log = self.paths.config.join("viewer.log");
                let ok = tokio::task::spawn_blocking(move || {
                    let mut c = viewer_cmd(&viewer, &server, fp.as_deref(), &log)?;
                    c.args(["login", "--oidc"]);
                    Ok::<_, String>(c.status().map_err(|e| e.to_string())?.success())
                })
                .await
                .unwrap_or(Ok(false));
                match ok {
                    Ok(true) => {
                        self.home(|h| {
                            h.signed_in = true;
                            h.profile.username.clear();
                            let _ = h.profile.save(&self.paths.config);
                        });
                        self.notice("Signed in with SSO", false);
                        self.fetch_computers(true).await;
                    }
                    Ok(false) => self.notice(
                        format!("SSO sign-in failed: {}", log_tail(&self.paths.config)),
                        true,
                    ),
                    Err(e) => self.notice(format!("SSO sign-in failed: {e}"), true),
                }
            }
            Request::SignOut => {
                profile::forget_token(&self.paths.config);
                self.home(|h| {
                    h.signed_in = false;
                    h.computers = None;
                });
                self.notice("Signed out", false);
            }
            Request::RefreshComputers => {
                self.fetch_computers(true).await;
            }
            Request::ConnectTyped(typed) => {
                self.home(|h| h.busy = Some(format!("Looking up {typed}…")));
                let list = self.fetch_computers(true).await.unwrap_or_default();
                match resolve(&typed, &list) {
                    Some(c) => self.connect(c.host_id, c.label.clone()).await,
                    None => self.notice(format!("No online computer has ID {typed}"), true),
                }
            }
            Request::Connect(id, label) => self.connect(id, label).await,
            Request::SaveServer {
                server,
                fingerprint,
            } => {
                self.home(|h| {
                    h.profile.server = server;
                    h.profile.fingerprint = fingerprint;
                    h.computers = None;
                    let _ = h.profile.save(&self.paths.config);
                });
                let signed_in = self.token().is_some();
                self.home(|h| h.signed_in = signed_in);
                self.notice(
                    if signed_in {
                        "Server saved"
                    } else {
                        "Server saved. Now sign in below."
                    },
                    false,
                );
            }
            Request::OpenHostSettings => {
                if let Err(e) =
                    rconn_host_settings::platform::run_elevated(&self.paths.settings_exe)
                {
                    self.notice(format!("Couldn't open host settings: {e}"), true);
                }
            }
            Request::EnableHost => self.enable_host().await,
            Request::DisableHost => {
                #[cfg(target_os = "macos")]
                crate::machost::uninstall(&self.paths.host_data);
                self.home(|h| {
                    h.this = None;
                    h.host_settings = None;
                });
                self.notice("Connections to this computer are off", false);
            }
            Request::OpenPermission(name) => {
                #[cfg(target_os = "macos")]
                crate::machost::open_permission_settings(&name);
                let _ = name;
            }
            Request::RestartHostAgent => match rconn_host_settings::platform::restart_agent() {
                Ok(()) => self.notice(
                    "Host restarting; new settings apply in a few seconds",
                    false,
                ),
                Err(e) => self.notice(format!("Couldn't restart the host: {e}"), true),
            },
        }
    }

    /// macOS: get an id from the server for this Mac (reusing a saved one), then start the
    /// host agent as a login item.
    async fn enable_host(&self) {
        #[cfg(not(target_os = "macos"))]
        self.notice("Use the Rconn host installer on this OS", true);
        #[cfg(target_os = "macos")]
        {
            let Some(token) = self.token() else {
                return self.notice("Sign in first (⚙ Settings)", true);
            };
            self.home(|h| h.busy = Some("Setting up connections to this Mac…".into()));
            let (server, trust, fp) = self.server();
            let dir = self.paths.host_data.clone();
            if !crate::machost::has_identity(&dir) {
                let key = match rconn_core::fleet::HostKey::generate() {
                    Ok(k) => k,
                    Err(e) => return self.notice(format!("Couldn't create a key: {e}"), true),
                };
                match SignalingClient::enroll_host(&server, &trust, &token, &key).await {
                    Ok((id, cert)) => {
                        if let Err(e) = crate::machost::save_identity(
                            &dir,
                            &id.to_string(),
                            &cert,
                            &key.to_pem(),
                        ) {
                            return self
                                .notice(format!("Couldn't save this Mac's identity: {e}"), true);
                        }
                    }
                    Err(e) => return self.notice(format!("The server refused: {e:#}"), true),
                }
            }
            let agent = std::env::current_exe()
                .unwrap_or_default()
                .with_file_name("rconn-host-agent");
            match crate::machost::install(&agent, &dir, &server, fp.as_deref()) {
                Ok(()) => {
                    self.notice(
                        "This Mac now accepts connections. Allow the macOS permissions when asked.",
                        false,
                    );
                    self.refresh_local(true);
                }
                Err(e) => self.notice(format!("Couldn't start the host: {e}"), true),
            }
        }
    }

    /// Open a session window (`rconn-viewer connect`) for `id`.
    async fn connect(&self, id: HostId, label: Option<String>) {
        let Some(token) = self.token() else {
            return self.notice("Sign in first (⚙ Settings)", true);
        };
        let name = label.clone().unwrap_or_else(|| id.short_code());
        self.home(|h| {
            h.busy = Some(format!("Connecting to {name}…"));
            h.profile.remember(id, label.clone());
            let _ = h.profile.save(&self.paths.config);
        });
        let (server, _, fp) = self.server();
        let log = self.paths.config.join("viewer.log");
        let child =
            viewer_cmd(&self.paths.viewer_exe, &server, fp.as_deref(), &log).and_then(|mut c| {
                c.env("RCONN_TOKEN", token)
                    .args(["connect", &id.0.to_string()]);
                c.spawn()
                    .map_err(|e| format!("{}: {e}", self.paths.viewer_exe.display()))
            });
        let mut child = match child {
            Ok(c) => c,
            Err(e) => return self.notice(format!("Couldn't open the session window: {e}"), true),
        };
        self.notice(format!("Session window opened for {name}"), false);
        let pid = child.id();
        if let Ok(mut v) = SESSIONS.lock() {
            v.push(pid);
        }
        let status = tokio::task::spawn_blocking(move || child.wait()).await;
        if let Ok(mut v) = SESSIONS.lock() {
            v.retain(|p| *p != pid);
        }
        if let Ok(Ok(s)) = status
            && !s.success()
        {
            self.notice(
                format!(
                    "Session to {name} ended with an error: {}",
                    log_tail(&self.paths.config)
                ),
                true,
            );
        }
    }
}

/// `rconn-viewer --server .. [--fingerprint ..]`, output to `log`, no console window.
fn viewer_cmd(
    exe: &std::path::Path,
    server: &str,
    fp: Option<&str>,
    log: &std::path::Path,
) -> Result<Command, String> {
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let out = std::fs::File::create(log).map_err(|e| e.to_string())?;
    let mut c = Command::new(exe);
    c.args(["--server", server]);
    if let Some(fp) = fp {
        c.args(["--fingerprint", fp]);
    }
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(out);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: the viewer opens its own window
    }
    Ok(c)
}

/// The viewer's last error line (it prints `Error: ...` on failure).
fn log_tail(config: &std::path::Path) -> String {
    std::fs::read_to_string(config.join("viewer.log"))
        .ok()
        .and_then(|s| {
            s.lines().rev().find(|l| !l.trim().is_empty()).map(|l| {
                l.trim()
                    .trim_start_matches("Error: ")
                    .chars()
                    .take(160)
                    .collect()
            })
        })
        .unwrap_or_else(|| "see viewer.log".into())
}
