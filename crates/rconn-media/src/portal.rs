//! The xdg-desktop-portal session behind Wayland screen capture and input.
//!
//! On Wayland (standard Ubuntu/GNOME) a program can neither read the screen nor inject
//! input by itself: the desktop's portal asks the person at the computer, then hands out
//! a PipeWire video stream and accepts input events for that session. One RemoteDesktop
//! session with a ScreenCast source covers both, so one consent dialog allows both, and
//! with `PersistMode::ExplicitlyRevoked` plus the restore token saved on disk, GNOME asks
//! only the first time (until revoked in Settings > Privacy > Screen Sharing / Remote
//! Desktop).
//!
//! The session is process-wide: the capturer and the input injector each hold a [`Lease`].
//! It opens on the first lease and closes a few seconds after the last one is dropped
//! (so a capture restart reuses it). All D-Bus traffic runs on one thread with its own
//! tokio runtime; callers use plain blocking calls.

use ashpd::desktop::remote_desktop::{
    Axis, DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions,
};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc as std_mpsc};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;

/// Kept open this long after the last lease goes (capture restarts reuse the session).
const IDLE_GRACE: Duration = Duration::from_secs(3);
/// After the person declines or stops sharing, don't ask again for this long.
const DECLINE_COOLDOWN: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Error)]
pub enum PortalError {
    #[error(
        "no usable xdg-desktop-portal on the session bus ({0}). Wayland capture needs \
         xdg-desktop-portal with the GNOME (or KDE) backend, and the host agent must run \
         inside the logged-in desktop session (systemd --user unit of that user)"
    )]
    Unavailable(String),
    #[error(
        "waiting for someone at this computer to allow screen sharing and remote control \
         in the desktop's dialog (asked once; the choice is remembered)"
    )]
    Waiting,
    #[error("screen sharing was declined or stopped at this computer; asking again in {0} s")]
    Declined(u64),
    #[error("portal: {0}")]
    Other(String),
}

fn other(what: &str, e: impl std::fmt::Display) -> PortalError {
    PortalError::Other(format!("{what}: {e}"))
}

/// The shared screen stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamInfo {
    /// PipeWire node to connect the video stream to; also the target of absolute pointer
    /// motion.
    pub node_id: u32,
    /// The stream's size in the compositor's logical coordinates (pointer positions use
    /// these), when the portal reports it.
    pub size: Option<(i32, i32)>,
}

/// One input event for the portal (Linux evdev codes).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputCmd {
    /// Absolute position in stream coordinates.
    Motion { x: f64, y: f64 },
    /// evdev button (`BTN_LEFT` = 0x110, ...).
    Button { code: i32, pressed: bool },
    /// Wheel clicks; positive = down / right (Wayland's convention).
    Scroll { horizontal: bool, steps: i32 },
    /// evdev key code (`KEY_A` = 30, ...).
    Key { code: i32, pressed: bool },
    /// X keysym (for typing text).
    Keysym { sym: i32, pressed: bool },
}

enum Cmd {
    Acquire(std_mpsc::Sender<Result<Grant, PortalError>>),
    Release(u64),
    Input(u64, InputCmd),
    OpenPipeWire(u64, std_mpsc::Sender<Result<OwnedFd, PortalError>>),
    Closed(u64),
}

struct Grant {
    generation: u64,
    stream: StreamInfo,
    closed: Arc<AtomicBool>,
}

/// A hold on the shared portal session; dropping it lets the session close.
pub struct Lease {
    generation: u64,
    stream: StreamInfo,
    closed: Arc<AtomicBool>,
    tx: mpsc::UnboundedSender<Cmd>,
}

impl Lease {
    pub fn stream(&self) -> StreamInfo {
        self.stream
    }

    /// The session ended (stopped at the computer, or the portal went away).
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// A fresh connection to the PipeWire daemon that can see this session's stream.
    pub fn open_pipewire(&self) -> Result<OwnedFd, PortalError> {
        let (tx, rx) = std_mpsc::channel();
        self.tx
            .send(Cmd::OpenPipeWire(self.generation, tx))
            .map_err(|_| other("portal thread", "gone"))?;
        rx.recv_timeout(Duration::from_secs(10))
            .map_err(|_| other("OpenPipeWireRemote", "no answer"))?
    }

    /// Queue an input event (sent in order; errors are logged by the portal thread).
    pub fn input(&self, cmd: InputCmd) {
        let _ = self.tx.send(Cmd::Input(self.generation, cmd));
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Release(self.generation));
    }
}

static TOKEN_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);
static THREAD: OnceLock<mpsc::UnboundedSender<Cmd>> = OnceLock::new();

/// Where the restore token lives (the host's data directory). Without one, the person
/// at the computer is asked every time the agent starts.
pub fn set_restore_token_path(path: PathBuf) {
    *TOKEN_PATH.lock().expect("token path") = Some(path);
}

fn token_path() -> Option<PathBuf> {
    TOKEN_PATH.lock().expect("token path").clone()
}

/// Join the shared session, opening it (and possibly showing the consent dialog) if
/// needed. Waits up to `timeout`; a dialog still open then yields
/// [`PortalError::Waiting`], and the next call keeps waiting for the same dialog.
pub fn acquire(timeout: Duration) -> Result<Lease, PortalError> {
    let tx = THREAD.get_or_init(spawn).clone();
    let (reply, rx) = std_mpsc::channel();
    tx.send(Cmd::Acquire(reply))
        .map_err(|_| other("portal thread", "gone"))?;
    let grant = match rx.recv_timeout(timeout) {
        Ok(r) => r?,
        Err(std_mpsc::RecvTimeoutError::Timeout) => return Err(PortalError::Waiting),
        Err(std_mpsc::RecvTimeoutError::Disconnected) => {
            return Err(other("portal thread", "gone"));
        }
    };
    Ok(Lease {
        generation: grant.generation,
        stream: grant.stream,
        closed: grant.closed,
        tx,
    })
}

fn spawn() -> mpsc::UnboundedSender<Cmd> {
    let (tx, rx) = mpsc::unbounded_channel();
    let loopback = tx.clone();
    std::thread::Builder::new()
        .name("rconn-portal".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("portal runtime");
            rt.block_on(run(rx, loopback));
        })
        .expect("spawning portal thread");
    tx
}

/// An open portal session.
struct Live {
    generation: u64,
    rd: RemoteDesktop,
    sc: Screencast,
    session: Arc<Session<RemoteDesktop>>,
    stream: StreamInfo,
    closed: Arc<AtomicBool>,
    watcher: tokio::task::JoinHandle<()>,
    users: usize,
    can_pointer: bool,
    can_keyboard: bool,
}

async fn run(mut rx: mpsc::UnboundedReceiver<Cmd>, loopback: mpsc::UnboundedSender<Cmd>) {
    let mut live: Option<Live> = None;
    let mut generation = 0u64;
    let mut idle_deadline: Option<tokio::time::Instant> = None;
    let mut declined_until: Option<tokio::time::Instant> = None;
    loop {
        let cmd = tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(c) => c,
                None => return,
            },
            _ = sleep_until_opt(idle_deadline) => {
                idle_deadline = None;
                if let Some(l) = live.take_if(|l| l.users == 0) {
                    tracing::info!(generation = l.generation, "closing the idle portal session");
                    close(l).await;
                }
                continue;
            }
        };
        match cmd {
            Cmd::Acquire(reply) => {
                if live.is_none() {
                    if let Some(until) = declined_until {
                        let now = tokio::time::Instant::now();
                        if now < until {
                            let left = (until - now).as_secs().max(1);
                            let _ = reply.send(Err(PortalError::Declined(left)));
                            continue;
                        }
                        declined_until = None;
                    }
                    generation += 1;
                    match open(generation, loopback.clone()).await {
                        Ok(l) => live = Some(l),
                        Err(e) => {
                            if matches!(e, PortalError::Declined(_)) {
                                declined_until =
                                    Some(tokio::time::Instant::now() + DECLINE_COOLDOWN);
                            }
                            tracing::warn!(error = %e, "portal session not available");
                            let _ = reply.send(Err(e));
                            continue;
                        }
                    }
                }
                let l = live.as_mut().expect("live session");
                let grant = Grant {
                    generation: l.generation,
                    stream: l.stream,
                    closed: l.closed.clone(),
                };
                // A caller that gave up waiting doesn't count as a user.
                if reply.send(Ok(grant)).is_ok() {
                    l.users += 1;
                    idle_deadline = None;
                } else if l.users == 0 {
                    idle_deadline = Some(tokio::time::Instant::now() + IDLE_GRACE);
                }
            }
            Cmd::Release(g) => {
                if let Some(l) = live.as_mut().filter(|l| l.generation == g) {
                    l.users = l.users.saturating_sub(1);
                    if l.users == 0 {
                        idle_deadline = Some(tokio::time::Instant::now() + IDLE_GRACE);
                    }
                }
            }
            Cmd::Input(g, ev) => {
                if let Some(l) = live.as_ref().filter(|l| l.generation == g) {
                    inject(l, ev).await;
                }
            }
            Cmd::OpenPipeWire(g, reply) => {
                let r = match live.as_ref().filter(|l| l.generation == g) {
                    Some(l) => {
                        l.sc.open_pipe_wire_remote(&l.session, Default::default())
                            .await
                            .map_err(|e| other("OpenPipeWireRemote", e))
                    }
                    None => Err(other("OpenPipeWireRemote", "session already closed")),
                };
                let _ = reply.send(r);
            }
            Cmd::Closed(g) => {
                if let Some(l) = live.take_if(|l| l.generation == g) {
                    tracing::warn!(
                        generation = g,
                        "the portal session was closed by the desktop (sharing stopped at \
                         this computer?)"
                    );
                    l.closed.store(true, Ordering::Relaxed);
                    l.watcher.abort();
                    declined_until = Some(tokio::time::Instant::now() + DECLINE_COOLDOWN);
                }
            }
        }
    }
}

async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

async fn close(l: Live) {
    l.closed.store(true, Ordering::Relaxed);
    l.watcher.abort();
    if let Err(e) = l.session.close().await {
        tracing::debug!(error = %e, "closing the portal session");
    }
}

fn is_cancelled(e: &ashpd::Error) -> bool {
    matches!(
        e,
        ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)
    )
}

/// Create the RemoteDesktop session with one monitor and start it (consent dialog, or
/// silent with a valid restore token).
async fn open(generation: u64, loopback: mpsc::UnboundedSender<Cmd>) -> Result<Live, PortalError> {
    let unavailable = |e: ashpd::Error| PortalError::Unavailable(e.to_string());
    let rd = RemoteDesktop::new().await.map_err(unavailable)?;
    let sc = Screencast::new().await.map_err(unavailable)?;
    let devices = rd
        .available_device_types()
        .await
        .unwrap_or(DeviceType::KEYBOARD | DeviceType::POINTER);
    let cursors = sc
        .available_cursor_modes()
        .await
        .unwrap_or(CursorMode::HIDDEN);
    // The viewer has no cursor shapes from Linux hosts yet: draw the pointer into the video.
    let cursor = if cursors.contains(CursorMode::EMBEDDED) {
        CursorMode::EMBEDDED
    } else {
        CursorMode::HIDDEN
    };
    let path = token_path();
    let token = path.as_deref().and_then(load_token);
    tracing::info!(
        generation,
        remote_desktop_version = rd.version(),
        screencast_version = sc.version(),
        ?devices,
        ?cursor,
        restore_token = token.is_some(),
        "opening the desktop portal session"
    );
    let session = rd
        .create_session(Default::default())
        .await
        .map_err(|e| other("CreateSession", e))?;
    let wanted = (DeviceType::KEYBOARD | DeviceType::POINTER) & devices;
    let mut select = SelectDevicesOptions::default().set_devices(wanted);
    // Persisting a remote-desktop session needs RemoteDesktop v2 (the persist options
    // are on SelectDevices; ScreenCast's are refused for remote-desktop sessions).
    if rd.version() >= 2 {
        select = select
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(token.as_deref());
    } else {
        tracing::warn!(
            version = rd.version(),
            "this portal can't remember the choice (RemoteDesktop v2 needed): the person at \
             the computer is asked every time"
        );
    }
    rd.select_devices(&session, select)
        .await
        .and_then(|r| r.response())
        .map_err(|e| other("SelectDevices", e))?;
    sc.select_sources(
        &session,
        SelectSourcesOptions::default()
            .set_cursor_mode(cursor)
            .set_sources(SourceType::MONITOR)
            .set_multiple(false),
    )
    .await
    .and_then(|r| r.response())
    .map_err(|e| other("SelectSources", e))?;
    if token.is_none() {
        tracing::info!(
            "asking the person at this computer to allow screen sharing and remote control"
        );
    }
    let started = rd
        .start(&session, None, Default::default())
        .await
        .and_then(|r| r.response())
        .map_err(|e| {
            if is_cancelled(&e) {
                PortalError::Declined(DECLINE_COOLDOWN.as_secs())
            } else {
                other("Start", e)
            }
        })?;
    if let (Some(p), Some(t)) = (&path, started.restore_token()) {
        match save_token(p, t) {
            Ok(()) => tracing::info!(path = %p.display(), "saved the portal restore token"),
            Err(e) => {
                tracing::warn!(error = %e, path = %p.display(), "could not save the portal restore token")
            }
        }
    }
    let granted = started.devices();
    let Some(first) = started.streams().first() else {
        let _ = session.close().await;
        return Err(other("Start", "the portal returned no screen stream"));
    };
    let stream = StreamInfo {
        node_id: first.pipe_wire_node_id(),
        size: first.size(),
    };
    tracing::info!(
        generation,
        node = stream.node_id,
        size = ?stream.size,
        ?granted,
        "desktop portal session started"
    );
    if !granted.contains(DeviceType::POINTER | DeviceType::KEYBOARD) {
        tracing::warn!(
            ?granted,
            "remote control was not fully allowed: only the screen is shared"
        );
    }
    let session = Arc::new(session);
    let watched = session.clone();
    let watcher = tokio::spawn(async move {
        use futures_util::StreamExt as _;
        if let Ok(mut s) = watched.receive_closed().await {
            s.next().await;
            let _ = loopback.send(Cmd::Closed(generation));
        }
    });
    Ok(Live {
        generation,
        rd,
        sc,
        session,
        stream,
        closed: Arc::new(AtomicBool::new(false)),
        watcher,
        users: 0,
        can_pointer: granted.contains(DeviceType::POINTER),
        can_keyboard: granted.contains(DeviceType::KEYBOARD),
    })
}

fn key_state(pressed: bool) -> KeyState {
    if pressed {
        KeyState::Pressed
    } else {
        KeyState::Released
    }
}

async fn inject(l: &Live, ev: InputCmd) {
    let (rd, s) = (&l.rd, &*l.session);
    let r = match ev {
        InputCmd::Motion { .. } | InputCmd::Button { .. } | InputCmd::Scroll { .. }
            if !l.can_pointer =>
        {
            return;
        }
        InputCmd::Key { .. } | InputCmd::Keysym { .. } if !l.can_keyboard => return,
        InputCmd::Motion { x, y } => {
            rd.notify_pointer_motion_absolute(s, l.stream.node_id, x, y, Default::default())
                .await
        }
        InputCmd::Button { code, pressed } => {
            rd.notify_pointer_button(s, code, key_state(pressed), Default::default())
                .await
        }
        InputCmd::Scroll { horizontal, steps } => {
            let axis = if horizontal {
                Axis::Horizontal
            } else {
                Axis::Vertical
            };
            rd.notify_pointer_axis_discrete(s, axis, steps, Default::default())
                .await
        }
        InputCmd::Key { code, pressed } => {
            rd.notify_keyboard_keycode(s, code, key_state(pressed), Default::default())
                .await
        }
        InputCmd::Keysym { sym, pressed } => {
            rd.notify_keyboard_keysym(s, sym, key_state(pressed), Default::default())
                .await
        }
    };
    if let Err(e) = r {
        tracing::debug!(error = %e, ?ev, "portal input event failed");
    }
}

/// The saved restore token, if any.
pub fn load_token(path: &Path) -> Option<String> {
    let t = std::fs::read_to_string(path).ok()?;
    let t = t.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

/// Save a restore token (owner-only, replaced atomically). Tokens are single-use: the
/// portal returns a new one every time a session starts.
pub fn save_token(path: &Path, token: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(token.trim().as_bytes())?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_token_round_trips_privately() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("rconn-token-{}", std::process::id()));
        let path = dir.join("nested").join("portal-restore-token");
        assert_eq!(load_token(&path), None, "missing file");
        save_token(&path, "  4b1d-token\n").unwrap();
        assert_eq!(load_token(&path).as_deref(), Some("4b1d-token"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // Single-use tokens: each save replaces the previous one.
        save_token(&path, "second").unwrap();
        assert_eq!(load_token(&path).as_deref(), Some("second"));
        assert!(!path.with_extension("tmp").exists());
        std::fs::write(&path, "  \n").unwrap();
        assert_eq!(load_token(&path), None, "blank file");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn errors_say_what_to_do() {
        let e = PortalError::Unavailable("org.freedesktop.portal.Desktop not found".into());
        assert!(e.to_string().contains("xdg-desktop-portal"));
        assert!(
            PortalError::Waiting
                .to_string()
                .contains("allow screen sharing")
        );
        assert!(PortalError::Declined(30).to_string().contains("30 s"));
    }
}
