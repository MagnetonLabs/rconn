//! File transfer on the `rconn-files/1` data channel (see [`rconn_core::files`]): the host
//! side ([`serve`], [`refuse`]) and the viewer side ([`FilesClient`]).
//!
//! Everything streams: a file is read and written a chunk at a time, hashed as it goes,
//! and at most [`TRANSFER_WINDOW`] bytes are unacknowledged, so memory stays small however
//! large the file. The sender also waits while the connection's SCTP queue holds more than
//! [`HIGH_WATER`] bytes: the queue is shared with the main channel, and a short one keeps
//! input and control messages from waiting behind file data.

use anyhow::{Context, Result, anyhow, bail};
use bytes::BytesMut;
use rconn_core::files::{
    ACK_EVERY, FILE_CHUNK_BYTES, FILES_CHANNEL_LABEL, FILES_CHANNEL_PROTOCOL, FileEntry,
    FileMessage, TRANSFER_WINDOW, numbered_name, relative_components, sanitize_file_name,
};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::sync::{Notify, mpsc};
use tokio::task::{JoinHandle, JoinSet};
use webrtc::data_channel::{DataChannel, DataChannelEvent, RTCDataChannelInit};
use webrtc::peer_connection::PeerConnection;

/// Bytes queued in SCTP (this channel's share) before a sender waits. Measured on
/// loopback (tests/file_transfer.rs): 1 MiB let bursts overflow and cost a main-channel
/// ping up to ~1 s (a retransmission timeout); 256 KiB kept the worst ping under 20 ms
/// and moved data faster. The price is throughput on long paths: at most this much per
/// round trip (about 5 MB/s at 50 ms).
pub const HIGH_WATER: usize = 256 * 1024;
/// Inbound messages buffered per request. More than a window's worth of chunks, so a peer
/// that honours the window never fills it; one that doesn't loses the request.
const QUEUE: usize = 128;
/// Requests a viewer may have running on the host at once.
const MAX_ACTIVE: usize = 8;
/// Entries listed per folder, at most.
const MAX_ENTRIES: usize = 10_000;
/// Rough encoded size of one page of entries (well under the 64 KiB message limit).
const PAGE_BYTES: usize = 48 * 1024;
/// Space an upload must leave free on the host's disk.
const FREE_SPACE_RESERVE: u64 = 256 * 1024 * 1024;

/// How a transfer stopped other than by failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Stopped {
    #[error("cancelled")]
    Cancelled,
    #[error("cancelled on the other computer")]
    PeerCancelled,
    #[error("the file connection closed")]
    Closed,
}

/// Lowercase hex, for logs.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One file channel's sending side.
struct Wire {
    dc: Arc<dyn DataChannel>,
    /// Woken on `OnBufferedAmountLow`.
    low: Notify,
}

impl Wire {
    fn new(dc: Arc<dyn DataChannel>) -> Self {
        Self {
            dc,
            low: Notify::new(),
        }
    }

    async fn send(&self, m: &FileMessage) -> Result<()> {
        let b = m.encode()?;
        self.dc
            .send(BytesMut::from(&b[..]))
            .await
            .map_err(|_| anyhow!(Stopped::Closed))
    }

    /// Wait until SCTP has room for more file data.
    async fn writable(&self) -> Result<()> {
        loop {
            let queued = self
                .dc
                .outstanding_bytes()
                .await
                .map_err(|_| anyhow!(Stopped::Closed))?;
            if queued <= HIGH_WATER {
                return Ok(());
            }
            // The event may not come (it tracks a slightly different counter): poll too.
            let _ = tokio::time::timeout(Duration::from_millis(10), self.low.notified()).await;
        }
    }
}

/// A file being written that is removed unless [`Partial::keep`] is called.
struct Partial(Option<PathBuf>);

impl Partial {
    fn keep(&mut self) {
        self.0 = None;
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Create `name` in `dir`, or `name (2)`, `name (3)`, ...: never replaces a file.
async fn create_unique(dir: &Path, name: &str) -> Result<(PathBuf, fs::File)> {
    for n in 1..=9999 {
        let path = dir.join(numbered_name(name, n));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(anyhow!("could not create the file: {}", io_text(&e))),
        }
    }
    bail!("too many files called {name}")
}

/// An I/O error as the other side may see it (no paths).
fn io_text(e: &std::io::Error) -> String {
    use std::io::ErrorKind as K;
    match e.kind() {
        K::NotFound => "no such file or folder".into(),
        K::PermissionDenied => "permission denied".into(),
        K::NotADirectory => "not a folder".into(),
        K::IsADirectory => "it is a folder".into(),
        K::StorageFull => "the disk is full".into(),
        K::QuotaExceeded => "over the disk quota".into(),
        other => format!("{other}"),
    }
}

/// Read until `buf` is full or the file ends.
async fn read_full(file: &mut fs::File, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let got = file
            .read(&mut buf[n..])
            .await
            .map_err(|e| anyhow!("could not read the file: {}", io_text(&e)))?;
        if got == 0 {
            break;
        }
        n += got;
    }
    Ok(n)
}

/// Receiving a file's content: written, hashed and acknowledged chunk by chunk.
struct Incoming {
    file: BufWriter<fs::File>,
    hasher: Sha256,
    received: u64,
    acked: u64,
    /// Refuse more than this (the announced size of an upload).
    limit: Option<u64>,
}

impl Incoming {
    fn new(file: fs::File, limit: Option<u64>) -> Self {
        Self {
            file: BufWriter::with_capacity(256 * 1024, file),
            hasher: Sha256::new(),
            received: 0,
            acked: 0,
            limit,
        }
    }

    async fn chunk(&mut self, wire: &Wire, req: u32, bytes: &[u8]) -> Result<()> {
        self.received += bytes.len() as u64;
        if self.limit.is_some_and(|l| self.received > l) {
            bail!("more data than announced");
        }
        self.hasher.update(bytes);
        self.file
            .write_all(bytes)
            .await
            .map_err(|e| anyhow!("could not write the file: {}", io_text(&e)))?;
        if self.received - self.acked >= ACK_EVERY {
            wire.send(&FileMessage::Ack {
                req,
                received: self.received,
            })
            .await?;
            self.acked = self.received;
        }
        Ok(())
    }

    /// Flush to disk and check the sender's hash. Returns (bytes, hash).
    async fn finish(mut self, sha256: [u8; 32]) -> Result<(u64, [u8; 32])> {
        let write_err = |e: std::io::Error| anyhow!("could not write the file: {}", io_text(&e));
        self.file.flush().await.map_err(write_err)?;
        self.file.get_mut().sync_all().await.map_err(write_err)?;
        let got: [u8; 32] = self.hasher.finalize().into();
        if got != sha256 {
            bail!("checksum mismatch: the file was damaged on the way");
        }
        Ok((self.received, got))
    }
}

/// Send up to `limit` bytes of `file` as Data chunks within the credit window, then Done.
/// `control` brings the peer's Acks; a Cancel, an Error or the channel closing stops it,
/// as does `progress` being cancelled. Returns (bytes sent, hash).
async fn send_content(
    wire: &Wire,
    req: u32,
    file: &mut fs::File,
    limit: u64,
    control: &mut mpsc::Receiver<FileMessage>,
    progress: Option<&Progress>,
) -> Result<(u64, [u8; 32])> {
    let mut buf = vec![0u8; FILE_CHUNK_BYTES];
    let (mut sent, mut acked) = (0u64, 0u64);
    let mut hasher = Sha256::new();
    loop {
        // Take what the peer sent; wait for credit while the window is full.
        loop {
            let full = sent - acked + FILE_CHUNK_BYTES as u64 > TRANSFER_WINDOW;
            let msg = if full {
                match progress {
                    Some(p) => tokio::select! {
                        m = control.recv() => m,
                        _ = p.cancelled() => return Err(Stopped::Cancelled.into()),
                    },
                    None => control.recv().await,
                }
            } else {
                match control.try_recv() {
                    Ok(m) => Some(m),
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => None,
                }
            };
            match msg {
                Some(FileMessage::Ack { received, .. }) => acked = acked.max(received.min(sent)),
                Some(FileMessage::Cancel { .. }) => return Err(Stopped::PeerCancelled.into()),
                Some(FileMessage::Error { message, .. }) => bail!("{message}"),
                Some(_) => {}
                None => return Err(Stopped::Closed.into()),
            }
        }
        if progress.is_some_and(|p| p.is_cancelled()) {
            return Err(Stopped::Cancelled.into());
        }
        let want = (limit - sent).min(FILE_CHUNK_BYTES as u64) as usize;
        if want == 0 {
            break;
        }
        let n = read_full(file, &mut buf[..want]).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        wire.writable().await?;
        wire.send(&FileMessage::Data {
            req,
            bytes: buf[..n].to_vec(),
        })
        .await?;
        sent += n as u64;
        if let Some(p) = progress {
            p.done.store(sent, Ordering::Relaxed);
        }
    }
    let sha256: [u8; 32] = hasher.finalize().into();
    wire.send(&FileMessage::Done { req, sha256 }).await?;
    Ok((sent, sha256))
}

// ---------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------

/// Free bytes on the disk holding a path (None: unknown).
pub type FreeSpace = Arc<dyn Fn(&Path) -> Option<u64> + Send + Sync>;

/// What a host shares on the file channel.
#[derive(Clone)]
pub struct ServeConfig {
    /// Every listed or sent path is inside this folder (symlinks included).
    pub root: PathBuf,
    /// Where uploads are saved (created when first needed).
    pub inbox: PathBuf,
    pub free_space: Option<FreeSpace>,
    /// Names the session in audit lines.
    pub session: String,
}

/// Refuse a file channel: tell the viewer why, then close it.
pub async fn refuse(dc: Arc<dyn DataChannel>, message: &str) {
    tracing::info!(target: "rconn_audit", reason = message, "file channel refused");
    let wire = Wire::new(dc.clone());
    let _ = wire
        .send(&FileMessage::Error {
            req: 0,
            message: message.into(),
        })
        .await;
    // Let the reason leave before the channel goes.
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while dc.outstanding_bytes().await.unwrap_or(0) > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = dc.close().await;
}

struct Host {
    wire: Wire,
    /// `ServeConfig::root`, canonical.
    root: PathBuf,
    cfg: ServeConfig,
}

/// Serve a viewer's file channel until it closes: listings, downloads and uploads.
pub async fn serve(dc: Arc<dyn DataChannel>, cfg: ServeConfig) {
    let root = match fs::canonicalize(&cfg.root).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, root = %cfg.root.display(), "shared folder unavailable");
            return refuse(dc, "this computer's shared folder is unavailable").await;
        }
    };
    let _ = dc
        .set_buffered_amount_low_threshold(HIGH_WATER as u32 / 2)
        .await;
    tracing::info!(target: "rconn_audit", session = %cfg.session, root = %root.display(), "file channel open");
    let host = Arc::new(Host {
        wire: Wire::new(dc.clone()),
        root,
        cfg,
    });
    let mut routes: HashMap<u32, mpsc::Sender<FileMessage>> = HashMap::new();
    let mut tasks: JoinSet<u32> = JoinSet::new();
    loop {
        tokio::select! {
            ev = dc.poll() => match ev {
                Some(DataChannelEvent::OnMessage(m)) => match FileMessage::decode(&m.data) {
                    Ok(msg) => host.handle(msg, &mut routes, &mut tasks).await,
                    Err(e) => tracing::warn!(error = %e, "bad file message"),
                },
                Some(DataChannelEvent::OnBufferedAmountLow) => host.wire.low.notify_waiters(),
                Some(DataChannelEvent::OnClose) | None => break,
                _ => {}
            },
            Some(done) = tasks.join_next() => {
                if let Ok(req) = done {
                    routes.remove(&req);
                }
            }
        }
    }
    // Dropping the routes stops every transfer (partial uploads are removed).
    routes.clear();
    while tasks.join_next().await.is_some() {}
    tracing::info!(target: "rconn_audit", session = %host.cfg.session, "file channel closed");
}

impl Host {
    async fn handle(
        self: &Arc<Self>,
        msg: FileMessage,
        routes: &mut HashMap<u32, mpsc::Sender<FileMessage>>,
        tasks: &mut JoinSet<u32>,
    ) {
        let req = msg.req();
        let starts = matches!(
            msg,
            FileMessage::List { .. } | FileMessage::Get { .. } | FileMessage::Put { .. }
        );
        if starts {
            let busy = if routes.contains_key(&req) || req == 0 {
                Some("that request id is in use")
            } else if tasks.len() >= MAX_ACTIVE {
                Some("too many transfers at once; wait for one to finish")
            } else {
                None
            };
            if let Some(why) = busy {
                let _ = self
                    .wire
                    .send(&FileMessage::Error {
                        req,
                        message: why.into(),
                    })
                    .await;
                return;
            }
            let (tx, rx) = mpsc::channel(QUEUE);
            routes.insert(req, tx);
            let me = self.clone();
            match msg {
                FileMessage::List { path, .. } => tasks.spawn(async move {
                    me.list(req, &path).await;
                    req
                }),
                FileMessage::Get { path, .. } => tasks.spawn(async move {
                    me.get(req, &path, rx).await;
                    req
                }),
                FileMessage::Put { name, size, .. } => tasks.spawn(async move {
                    me.put(req, &name, size, rx).await;
                    req
                }),
                _ => unreachable!("matched above"),
            };
            return;
        }
        match msg {
            FileMessage::Data { .. }
            | FileMessage::Done { .. }
            | FileMessage::Ack { .. }
            | FileMessage::Cancel { .. } => {
                let Some(tx) = routes.get(&req) else { return };
                if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send(msg) {
                    // Over the window: the viewer ignored flow control. Drop the request.
                    tracing::warn!(req, "viewer overran the transfer window; stopping it");
                    routes.remove(&req);
                    let _ = self
                        .wire
                        .send(&FileMessage::Error {
                            req,
                            message: "too much data at once".into(),
                        })
                        .await;
                }
            }
            other => tracing::debug!(?other, "unexpected file message from the viewer"),
        }
    }

    /// The real path of `rel`, which must exist and be inside the root.
    async fn resolve(&self, rel: &str) -> Result<PathBuf> {
        let mut path = self.root.clone();
        for c in relative_components(rel)? {
            path.push(c);
        }
        let real = fs::canonicalize(&path)
            .await
            .map_err(|e| anyhow!("{}", io_text(&e)))?;
        if !real.starts_with(&self.root) {
            bail!("only files inside the shared folder are allowed");
        }
        Ok(real)
    }

    /// `p` relative to the root, with `/` separators.
    fn display(&self, p: &Path) -> String {
        match p.strip_prefix(&self.root) {
            Ok(rel) => rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/"),
            Err(_) => p.display().to_string(),
        }
    }

    async fn fail(&self, req: u32, e: &anyhow::Error) {
        if e.downcast_ref::<Stopped>().is_some() {
            return; // nobody to tell, or they know
        }
        let message: String = format!("{e:#}").chars().take(500).collect();
        let _ = self.wire.send(&FileMessage::Error { req, message }).await;
    }

    async fn list(&self, req: u32, rel: &str) {
        let result: Result<()> = async {
            let dir = self.resolve(rel).await?;
            let mut rd = fs::read_dir(&dir)
                .await
                .map_err(|e| anyhow!("{}", io_text(&e)))?;
            let mut entries = Vec::new();
            while let Some(e) = rd
                .next_entry()
                .await
                .map_err(|e| anyhow!("{}", io_text(&e)))?
            {
                if entries.len() >= MAX_ENTRIES {
                    break;
                }
                // Follow symlinks for what they point at; a broken one shows as itself.
                let meta = match fs::metadata(e.path()).await {
                    Ok(m) => m,
                    Err(_) => match e.metadata().await {
                        Ok(m) => m,
                        Err(_) => continue,
                    },
                };
                entries.push(FileEntry {
                    name: e.file_name().to_string_lossy().into_owned(),
                    is_dir: meta.is_dir(),
                    size: if meta.is_dir() { 0 } else { meta.len() },
                    modified: meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs()),
                });
            }
            entries.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            let mut page = Vec::new();
            let mut bytes = 0;
            for e in entries {
                let size = e.name.len() + 24;
                if bytes + size > PAGE_BYTES && !page.is_empty() {
                    self.wire
                        .send(&FileMessage::Entries {
                            req,
                            entries: std::mem::take(&mut page),
                            last: false,
                        })
                        .await?;
                    bytes = 0;
                }
                bytes += size;
                page.push(e);
            }
            self.wire
                .send(&FileMessage::Entries {
                    req,
                    entries: page,
                    last: true,
                })
                .await
        }
        .await;
        if let Err(e) = result {
            tracing::debug!(req, error = %format!("{e:#}"), "listing failed");
            self.fail(req, &e).await;
        }
    }

    async fn get(&self, req: u32, rel: &str, mut control: mpsc::Receiver<FileMessage>) {
        let result: Result<(u64, [u8; 32])> = async {
            let path = self.resolve(rel).await?;
            let meta = fs::metadata(&path)
                .await
                .map_err(|e| anyhow!("{}", io_text(&e)))?;
            if meta.is_dir() {
                bail!("folders can't be copied yet; open it and pick its files");
            }
            let mut file = fs::File::open(&path)
                .await
                .map_err(|e| anyhow!("could not open the file: {}", io_text(&e)))?;
            let size = meta.len();
            self.wire.send(&FileMessage::Opened { req, size }).await?;
            send_content(&self.wire, req, &mut file, size, &mut control, None).await
        }
        .await;
        self.audit("download", rel, &result);
        if let Err(e) = result {
            self.fail(req, &e).await;
        }
    }

    async fn put(&self, req: u32, name: &str, size: u64, mut control: mpsc::Receiver<FileMessage>) {
        let mut saved = String::new();
        let result: Result<(u64, [u8; 32])> = async {
            let inbox = &self.cfg.inbox;
            fs::create_dir_all(inbox)
                .await
                .map_err(|e| anyhow!("could not create the Rconn downloads folder: {}", io_text(&e)))?;
            if let Some(free) = self.cfg.free_space.as_ref().and_then(|f| f(inbox.as_path()))
                && size.saturating_add(FREE_SPACE_RESERVE) > free
            {
                bail!(
                    "not enough free space on this computer ({} MB free, {} MB needed)",
                    free >> 20,
                    (size + FREE_SPACE_RESERVE) >> 20
                );
            }
            let (path, file) = create_unique(inbox, &sanitize_file_name(name)).await?;
            let mut partial = Partial(Some(path.clone()));
            self.wire
                .send(&FileMessage::Ack { req, received: 0 })
                .await?;
            let mut incoming = Incoming::new(file, Some(size));
            loop {
                match control.recv().await {
                    Some(FileMessage::Data { bytes, .. }) => {
                        incoming.chunk(&self.wire, req, &bytes).await?
                    }
                    Some(FileMessage::Done { sha256, .. }) => {
                        if incoming.received != size {
                            bail!(
                                "the file changed while it was sent ({size} bytes announced, {} arrived)",
                                incoming.received
                            );
                        }
                        let done = incoming.finish(sha256).await?;
                        partial.keep();
                        saved = match fs::canonicalize(&path).await {
                            Ok(real) => self.display(&real),
                            Err(_) => self.display(&path),
                        };
                        self.wire
                            .send(&FileMessage::Saved {
                                req,
                                path: saved.clone(),
                            })
                            .await?;
                        return Ok(done);
                    }
                    Some(FileMessage::Cancel { .. }) => return Err(Stopped::PeerCancelled.into()),
                    Some(_) => {}
                    None => return Err(Stopped::Closed.into()),
                }
            }
        }
        .await;
        let what = if saved.is_empty() {
            sanitize_file_name(name)
        } else {
            saved
        };
        self.audit("upload", &what, &result);
        if let Err(e) = result {
            self.fail(req, &e).await;
        }
    }

    /// One line per transfer, whatever its outcome.
    fn audit(&self, op: &str, path: &str, result: &Result<(u64, [u8; 32])>) {
        let session = &self.cfg.session;
        match result {
            Ok((bytes, sha)) => tracing::info!(
                target: "rconn_audit", session = %session, op, path, bytes, sha256 = %hex(sha),
                "file transfer done"
            ),
            Err(e) => tracing::info!(
                target: "rconn_audit", session = %session, op, path, outcome = %format!("{e:#}"),
                "file transfer stopped"
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Viewer
// ---------------------------------------------------------------------------

/// A transfer's progress, shared with whoever shows it; also how it is cancelled.
#[derive(Debug, Default)]
pub struct Progress {
    done: AtomicU64,
    total: AtomicU64,
    cancel: AtomicBool,
    wake: Notify,
}

impl Progress {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }
    /// Bytes moved so far.
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }
    /// The file's size, once known.
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }
    /// Stop the transfer; its partial file is removed on both sides.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
    async fn cancelled(&self) {
        while !self.is_cancelled() {
            self.wake.notified().await;
        }
    }
}

struct Client {
    wire: Wire,
    routes: Mutex<HashMap<u32, mpsc::Sender<FileMessage>>>,
    next: AtomicU32,
    /// Why the host refused the channel, once it did.
    refused: Mutex<Option<String>>,
}

impl Client {
    fn gone(&self) -> anyhow::Error {
        match self.refused.lock().expect("refused").clone() {
            Some(why) => anyhow!("{why}"),
            None => Stopped::Closed.into(),
        }
    }
}

/// One request's replies; unregisters itself when dropped.
struct Route {
    req: u32,
    rx: mpsc::Receiver<FileMessage>,
    client: Arc<Client>,
}

impl Route {
    async fn recv(&mut self, progress: Option<&Progress>) -> Result<FileMessage> {
        let msg = match progress {
            Some(p) => tokio::select! {
                m = self.rx.recv() => m,
                _ = p.cancelled() => return Err(Stopped::Cancelled.into()),
            },
            None => self.rx.recv().await,
        };
        match msg {
            Some(FileMessage::Error { message, .. }) => Err(anyhow!("{message}")),
            Some(FileMessage::Cancel { .. }) => Err(Stopped::PeerCancelled.into()),
            Some(m) => Ok(m),
            None => Err(self.client.gone()),
        }
    }
}

impl Drop for Route {
    fn drop(&mut self) {
        self.client.routes.lock().expect("routes").remove(&self.req);
    }
}

/// The viewer's end of the file channel.
pub struct FilesClient {
    client: Arc<Client>,
    reader: JoinHandle<()>,
}

impl Drop for FilesClient {
    fn drop(&mut self) {
        self.reader.abort();
        let dc = self.client.wire.dc.clone();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let _ = dc.close().await;
            });
        }
    }
}

impl FilesClient {
    /// Open the file channel on a connected session and wait for it. The host may still
    /// refuse it: the first request then fails with its reason.
    pub async fn open(pc: &dyn PeerConnection) -> Result<Self> {
        let init = RTCDataChannelInit {
            ordered: true,
            protocol: FILES_CHANNEL_PROTOCOL.into(),
            ..Default::default()
        };
        let dc = pc
            .create_data_channel(FILES_CHANNEL_LABEL, Some(init))
            .await
            .context("opening the file channel")?;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match dc.poll().await {
                    Some(DataChannelEvent::OnOpen) => return Ok(()),
                    Some(DataChannelEvent::OnClose) | None => {
                        bail!("the computer closed the file channel")
                    }
                    _ => {}
                }
            }
        })
        .await
        .map_err(|_| anyhow!("the file channel did not open"))??;
        let _ = dc
            .set_buffered_amount_low_threshold(HIGH_WATER as u32 / 2)
            .await;
        let client = Arc::new(Client {
            wire: Wire::new(dc),
            routes: Mutex::default(),
            next: AtomicU32::new(1),
            refused: Mutex::default(),
        });
        let reader = tokio::spawn(read_replies(client.clone()));
        Ok(Self { client, reader })
    }

    fn start(&self) -> Result<Route> {
        if let Some(why) = self.client.refused.lock().expect("refused").clone() {
            bail!("{why}");
        }
        let req = self.client.next.fetch_add(1, Ordering::Relaxed).max(1);
        let (tx, rx) = mpsc::channel(QUEUE);
        self.client.routes.lock().expect("routes").insert(req, tx);
        Ok(Route {
            req,
            rx,
            client: self.client.clone(),
        })
    }

    /// Why the channel is unusable, if it is (refused or closed).
    pub fn refused(&self) -> Option<String> {
        self.client.refused.lock().expect("refused").clone()
    }

    /// What is in a folder on the host ("" is the shared root).
    pub async fn list(&self, path: &str) -> Result<Vec<FileEntry>> {
        let mut route = self.start()?;
        self.client
            .wire
            .send(&FileMessage::List {
                req: route.req,
                path: path.into(),
            })
            .await
            .map_err(|_| self.client.gone())?;
        let mut all = Vec::new();
        loop {
            if let FileMessage::Entries { entries, last, .. } = route.recv(None).await? {
                all.extend(entries);
                if last {
                    return Ok(all);
                }
            }
        }
    }

    /// Copy a file from the host into `dest_dir` (never replacing a file there: a second
    /// copy is saved as `name (2)`). Returns where it was saved.
    pub async fn download(
        &self,
        remote: &str,
        dest_dir: &Path,
        progress: &Progress,
    ) -> Result<PathBuf> {
        let mut route = self.start()?;
        let req = route.req;
        let result = self
            .download_inner(&mut route, remote, dest_dir, progress)
            .await;
        if result.is_err() {
            let _ = self.client.wire.send(&FileMessage::Cancel { req }).await;
        }
        result
    }

    async fn download_inner(
        &self,
        route: &mut Route,
        remote: &str,
        dest_dir: &Path,
        progress: &Progress,
    ) -> Result<PathBuf> {
        let req = route.req;
        let wire = &self.client.wire;
        wire.send(&FileMessage::Get {
            req,
            path: remote.into(),
        })
        .await
        .map_err(|_| self.client.gone())?;
        let size = loop {
            if let FileMessage::Opened { size, .. } = route.recv(Some(progress)).await? {
                break size;
            }
        };
        progress.total.store(size, Ordering::Relaxed);
        fs::create_dir_all(dest_dir)
            .await
            .map_err(|e| anyhow!("could not create {}: {}", dest_dir.display(), io_text(&e)))?;
        let (path, file) = create_unique(dest_dir, &sanitize_file_name(remote)).await?;
        let mut partial = Partial(Some(path.clone()));
        let mut incoming = Incoming::new(file, None);
        loop {
            match route.recv(Some(progress)).await? {
                FileMessage::Data { bytes, .. } => {
                    incoming.chunk(wire, req, &bytes).await?;
                    progress.done.store(incoming.received, Ordering::Relaxed);
                }
                FileMessage::Done { sha256, .. } => {
                    incoming.finish(sha256).await?;
                    partial.keep();
                    return Ok(path);
                }
                _ => {}
            }
        }
    }

    /// Copy a local file to the host (its Downloads/Rconn folder). Returns where the host
    /// saved it.
    pub async fn upload(&self, local: &Path, progress: &Progress) -> Result<String> {
        let mut route = self.start()?;
        let req = route.req;
        let result = self.upload_inner(&mut route, local, progress).await;
        if result.is_err() {
            let _ = self.client.wire.send(&FileMessage::Cancel { req }).await;
        }
        result
    }

    async fn upload_inner(
        &self,
        route: &mut Route,
        local: &Path,
        progress: &Progress,
    ) -> Result<String> {
        let req = route.req;
        let wire = &self.client.wire;
        let meta = fs::metadata(local)
            .await
            .map_err(|e| anyhow!("{}: {}", local.display(), io_text(&e)))?;
        if meta.is_dir() {
            bail!("folders can't be sent yet; drop the files inside it");
        }
        let name = local
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let mut file = fs::File::open(local)
            .await
            .map_err(|e| anyhow!("{}: {}", local.display(), io_text(&e)))?;
        let size = meta.len();
        progress.total.store(size, Ordering::Relaxed);
        wire.send(&FileMessage::Put { req, name, size })
            .await
            .map_err(|_| self.client.gone())?;
        loop {
            if let FileMessage::Ack { .. } = route.recv(Some(progress)).await? {
                break;
            }
        }
        send_content(wire, req, &mut file, size, &mut route.rx, Some(progress))
            .await
            .map_err(|e| match e.downcast_ref::<Stopped>() {
                Some(Stopped::Closed) => self.client.gone(),
                _ => e,
            })?;
        loop {
            if let FileMessage::Saved { path, .. } = route.recv(Some(progress)).await? {
                return Ok(path);
            }
        }
    }
}

/// Route the host's replies to their requests.
async fn read_replies(client: Arc<Client>) {
    let dc = client.wire.dc.clone();
    loop {
        match dc.poll().await {
            Some(DataChannelEvent::OnMessage(m)) => match FileMessage::decode(&m.data) {
                Ok(FileMessage::Error { req: 0, message }) => {
                    tracing::info!(%message, "the computer refused file transfer");
                    *client.refused.lock().expect("refused") = Some(message);
                    client.routes.lock().expect("routes").clear();
                }
                Ok(msg) => {
                    let req = msg.req();
                    let mut routes = client.routes.lock().expect("routes");
                    if let Some(tx) = routes.get(&req)
                        && let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send(msg)
                    {
                        tracing::warn!(req, "host overran the transfer window; stopping it");
                        routes.remove(&req);
                    }
                }
                Err(e) => tracing::warn!(error = %e, "bad file message"),
            },
            Some(DataChannelEvent::OnBufferedAmountLow) => client.wire.low.notify_waiters(),
            Some(DataChannelEvent::OnClose) | None => break,
            _ => {}
        }
    }
    let mut refused = client.refused.lock().expect("refused");
    if refused.is_none() {
        *refused = Some("the file connection closed".into());
    }
    drop(refused);
    client.routes.lock().expect("routes").clear();
}
