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
    /// Windows: the host service program (sets admin-only settings when elevated).
    pub service_exe: PathBuf,
    /// The app runs this computer's host per user (see `userhost`): macOS, Linux, and
    /// Windows installed "just for me" (no service beside the app).
    pub user_host: bool,
}

impl Paths {
    pub fn installed() -> Self {
        let beside = |name: &str| {
            std::env::current_exe()
                .unwrap_or_default()
                .with_file_name(format!("{name}{}", std::env::consts::EXE_SUFFIX))
        };
        let config = profile::config_dir();
        // macOS and Linux, and Windows without the service: the app sets the host up per
        // user (see userhost). Windows with the service: the installer did.
        let user_host = cfg!(unix) || !beside("rconn-host-service").exists();
        let (status_file, host_data) = if cfg!(unix) {
            (config.join("host").join("status.json"), config.join("host"))
        } else if user_host {
            let dir = crate::userhost::windows_host_dir();
            (dir.join("status.json"), dir)
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
            service_exe: beside("rconn-host-service"),
            user_host,
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
                {
                    let w = w.clone();
                    tokio::spawn(async move { w.first_launch().await });
                }
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
                            if n.is_multiple_of(3) {
                                let w = w.clone();
                                tokio::spawn(async move { w.fetch_presence().await });
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
                h.profile.server_url().to_owned(),
                ServerTrust::from_fingerprint(fp.as_deref()),
                fp,
            )
        })
    }

    fn token(&self) -> Option<String> {
        let server = self.home(|h| h.profile.server_url().to_owned());
        profile::token_for(&self.paths.config, &server, profile::now_unix())
    }

    /// First launch, like AnyDesk: give this computer an ID right away (no account, no
    /// "allow" step), unless incoming connections were turned off here before.
    async fn first_launch(&self) {
        let decided = self.home(|h| h.profile.incoming);
        if self.paths.user_host
            && decided != Some(false)
            && !rconn_peer::identity::has_identity(&self.paths.host_data)
        {
            self.enable_host().await;
        }
    }

    /// Re-read this computer's status (and, every few ticks, its settings).
    fn refresh_local(&self, settings_too: bool) {
        let status = HostStatus::read(&self.paths.status_file);
        // macOS/Linux: installed while the start-at-login definition exists (host.toml
        // stays when incoming connections are turned off); Windows: the admin-only folder
        // exists (its contents aren't visible to ordinary users).
        let installed = if self.paths.user_host {
            crate::userhost::installed()
        } else {
            self.paths.host_data.exists()
        };
        // The settings are worth showing even while off (the access password is kept).
        let readable =
            installed || (self.paths.user_host && self.paths.host_data.join("host.toml").exists());
        let settings = (settings_too && readable)
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
                // A status file left behind by a stopped agent isn't "on".
                h.this = None;
            }
            if !readable {
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

    /// Ask the server which recent computers are online (no account needed).
    async fn fetch_presence(&self) {
        let ids: Vec<String> = self.home(|h| {
            h.profile
                .recent
                .iter()
                .map(|r| r.code().replace(' ', ""))
                .filter(|c| c.len() == 9 && c.bytes().all(|b| b.is_ascii_digit()))
                .take(rconn_core::protocol::MAX_PRESENCE_IDS)
                .collect()
        });
        if ids.is_empty() {
            return;
        }
        let (server, trust, _) = self.server();
        let presence = match SignalingClient::presence(&server, &trust, ids.clone()).await {
            Ok(online) => ids
                .into_iter()
                .map(|id| {
                    let up = online.contains(&id);
                    (id, up)
                })
                .collect(),
            Err(e) => {
                tracing::debug!("presence: {e:#}");
                Default::default()
            }
        };
        self.home(|h| h.presence = presence);
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
                    None => self.notice(
                        format!(
                            "No online computer in your account has ID {typed}. If it has an access password, enter it under the ID."
                        ),
                        true,
                    ),
                }
            }
            Request::Connect(id, label) => self.connect(id, label).await,
            Request::ConnectGuest { id, password } => {
                self.home(|h| {
                    h.profile.remember_code(&id);
                    let _ = h.profile.save(&self.paths.config);
                });
                self.connect_guest(id, password.0).await
            }
            Request::RenameRecent { code, label } => self.home(|h| {
                h.profile.rename(&code, &label);
                let _ = h.profile.save(&self.paths.config);
            }),
            Request::ForgetRecent(code) => self.home(|h| {
                h.profile.recent.retain(|r| r.code() != code);
                let _ = h.profile.save(&self.paths.config);
            }),
            Request::SetAccessPassword(p) => self.set_access_password(Some(p.0)).await,
            Request::ClearAccessPassword => self.set_access_password(None).await,
            Request::SaveServer {
                server,
                fingerprint,
            } => {
                self.home(|h| {
                    h.profile.server = server;
                    h.profile.fingerprint = fingerprint;
                    h.computers = None;
                    h.presence.clear();
                    let _ = h.profile.save(&self.paths.config);
                });
                let signed_in = self.token().is_some();
                self.home(|h| h.signed_in = signed_in);
                self.notice("Server saved", false);
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
                if self.paths.user_host {
                    crate::userhost::uninstall(&self.paths.host_data);
                }
                self.home(|h| {
                    h.this = None;
                    h.host_settings = None;
                    h.profile.incoming = Some(false);
                    let _ = h.profile.save(&self.paths.config);
                });
                self.notice("Incoming connections to this computer are off", false);
            }
            Request::OpenPermission(name) => {
                #[cfg(target_os = "macos")]
                crate::userhost::open_permission_settings(&name);
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

    /// Why turning on incoming connections failed: shown in "This computer".
    fn host_failed(&self, text: String) {
        self.home(|h| {
            h.host_setup = None;
            h.host_error = Some(text);
        });
    }

    /// macOS and Linux: get an ID from the server for this computer (reusing a saved one),
    /// then start the host agent at login. No account needed: the computer belongs to
    /// nobody and takes the access password. Signed in to a fleet, it joins the fleet.
    async fn enable_host(&self) {
        if !self.paths.user_host {
            return self.notice("The Rconn installer set up this computer's host", true);
        }
        {
            let this = if cfg!(target_os = "macos") {
                "this Mac"
            } else {
                "this computer"
            };
            self.home(|h| {
                h.host_error = None;
                h.host_setup = Some(format!("Getting an ID for {this}…"));
            });
            let (server, trust, fp) = self.server();
            let dir = self.paths.host_data.clone();
            if !rconn_peer::identity::has_identity(&dir) {
                let enrolled = match self.token() {
                    Some(token) => {
                        let key = match rconn_core::fleet::HostKey::generate() {
                            Ok(k) => k,
                            Err(e) => {
                                return self.host_failed(format!("Couldn't create a key: {e}"));
                            }
                        };
                        SignalingClient::enroll_host(&server, &trust, &token, &key)
                            .await
                            .and_then(|(id, cert)| {
                                rconn_peer::identity::save_identity(&dir, id, &cert, &key, None)
                            })
                    }
                    None => rconn_peer::identity::enroll_anonymous_into(&dir, &server, &trust)
                        .await
                        .map(|_| ()),
                };
                if let Err(e) = enrolled {
                    return self.host_failed(format!("Couldn't get an ID from {server}: {e:#}"));
                }
            }
            self.home(|h| h.host_setup = Some(format!("Starting the host on {this}…")));
            let agent = std::env::current_exe()
                .unwrap_or_default()
                .with_file_name(format!("rconn-host-agent{}", std::env::consts::EXE_SUFFIX));
            match crate::userhost::install(&agent, &dir, &server, fp.as_deref()) {
                Ok(()) => {
                    self.home(|h| {
                        h.profile.incoming = Some(true);
                        let _ = h.profile.save(&self.paths.config);
                    });
                    // The ID shows once the agent publishes its status (a few seconds).
                    for _ in 0..20 {
                        if HostStatus::read(&self.paths.status_file).is_some() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                    self.home(|h| h.host_setup = None);
                    self.refresh_local(true);
                    let password = self.home(|h| {
                        let this = h.this.clone();
                        crate::home::access_password_on(h, this.as_ref())
                    });
                    self.notice(
                        match (cfg!(target_os = "macos"), password) {
                            (true, true) => "This Mac has an ID. Allow the macOS permissions when asked.",
                            (true, false) => "This Mac has an ID. Allow the macOS permissions when asked; until you set an access password, you accept each connection here.",
                            (false, true) => "This computer has an ID; connections need its access password.",
                            (false, false) => "This computer has an ID. Until you set an access password, you accept each connection here.",
                        },
                        false,
                    );
                }
                Err(e) => self.host_failed(format!("Couldn't start the host: {e}")),
            }
        }
    }

    /// Set (or, with `None`, turn off) this computer's access password. Only the derived
    /// verifier is stored; on Windows it is written by the host service with admin rights.
    async fn set_access_password(&self, password: Option<String>) {
        self.home(|h| h.busy = Some("Saving the access password…".into()));
        let verifier = match password {
            None => String::new(),
            Some(p) => {
                match tokio::task::spawn_blocking(move || rconn_core::guest::make_verifier(&p))
                    .await
                {
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => return self.notice(e.to_string(), true),
                    Err(e) => return self.notice(e.to_string(), true),
                }
            }
        };
        let on = !verifier.is_empty();
        let (host_data, service) = (self.paths.host_data.clone(), self.paths.service_exe.clone());
        let user_host = self.paths.user_host;
        let result = tokio::task::spawn_blocking(move || {
            if !user_host {
                let args: &[&str] = if verifier.is_empty() {
                    &["unset", "access_password"]
                } else {
                    &["set", "access_password", &verifier]
                };
                rconn_host_settings::platform::run_elevated_wait(&service, args)
            } else {
                // Possibly before the ID arrived: the folder may not exist yet.
                crate::userhost::ensure_settings_file(&host_data).map_err(|e| e.to_string())?;
                rconn_core::settings::write_setting(
                    rconn_core::settings::SettingsLayers {
                        system_file: Some(host_data.join("host.toml")),
                        ..Default::default()
                    },
                    "access_password",
                    &verifier,
                    rconn_core::settings::Writer::Admin,
                    host_data.join("session.active").exists(),
                )
                .map_err(|e| e.to_string())
            }
        })
        .await;
        match result {
            Ok(Ok(())) => {
                self.home(|h| h.host_password_saved = Some((on, profile::now_unix())));
                self.notice(
                    if on {
                        "Access password set: connect with this computer's ID and the password"
                    } else {
                        "Access password removed: each connection now needs your OK here"
                    },
                    false,
                );
                self.refresh_local(true);
                // Saving a password is what makes this computer take connections: turn them
                // on if they were off (or the ID never arrived). Not while that is underway.
                if self.paths.user_host
                    && on
                    && !crate::userhost::installed()
                    && self.home(|h| h.host_setup.is_none())
                {
                    self.enable_host().await;
                }
            }
            Ok(Err(e)) => self.notice(format!("Couldn't save the access password: {e}"), true),
            Err(e) => self.notice(format!("Couldn't save the access password: {e}"), true),
        }
    }

    /// Open a session window for the computer with ID `typed`, using its access password
    /// (no account). The password goes to the viewer in its environment, never argv.
    async fn connect_guest(&self, typed: String, password: String) {
        self.home(|h| h.busy = Some(format!("Connecting to {typed}…")));
        let (server, _, fp) = self.server();
        let log = self.paths.config.join("viewer.log");
        // The viewer writes the computer's name here once it's let in.
        let code = profile::tidy_code(&typed);
        let name_file = self
            .paths
            .config
            .join(format!("name-{}.txt", code.replace(' ', "")));
        let _ = std::fs::remove_file(&name_file);
        let child =
            viewer_cmd(&self.paths.viewer_exe, &server, fp.as_deref(), &log).and_then(|mut c| {
                c.env("RCONN_ACCESS_PASSWORD", &password)
                    .env("RCONN_NAME_FILE", &name_file)
                    .args(["connect", &typed]);
                c.spawn()
                    .map_err(|e| format!("{}: {e}", self.paths.viewer_exe.display()))
            });
        self.wait_session(child, typed, Some((code, name_file)))
            .await;
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
        self.wait_session(child, name, None).await;
    }

    /// Track a session window until it closes; report how it ended.
    async fn wait_session(
        &self,
        child: Result<std::process::Child, String>,
        name: String,
        mut name_file: Option<(String, std::path::PathBuf)>,
    ) {
        let mut child = match child {
            Ok(c) => c,
            Err(e) => return self.notice(format!("Couldn't open the session window: {e}"), true),
        };
        self.notice(format!("Session window opened for {name}"), false);
        let pid = child.id();
        if let Ok(mut v) = SESSIONS.lock() {
            v.push(pid);
        }
        let status = loop {
            let done = child.try_wait();
            if let Some((code, path)) = &name_file
                && let Ok(learned) = std::fs::read_to_string(path)
            {
                let _ = std::fs::remove_file(path);
                self.home(|h| {
                    h.profile.learn_name(code, &learned);
                    let _ = h.profile.save(&self.paths.config);
                });
                name_file = None;
            }
            match done {
                Ok(None) => tokio::time::sleep(Duration::from_secs(1)).await,
                Ok(Some(s)) => break Ok(s),
                Err(e) => break Err(e),
            }
        };
        if let Some((_, path)) = &name_file {
            let _ = std::fs::remove_file(path);
        }
        if let Ok(mut v) = SESSIONS.lock() {
            v.retain(|p| *p != pid);
        }
        if let Ok(s) = status
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
