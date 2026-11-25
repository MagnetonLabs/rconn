//! One session's screen stream. A single thread captures, converts and encodes; the
//! session task only writes the encoded frames to the video track.
//!
//! CPU is spent only on what changed and only when it is worth showing:
//! * capture is pulled (a frame is taken only when the encoder will use it), and only the
//!   areas the OS reports as changed are copied and converted;
//! * frames are encoded at the full rate while the viewer is typing or moving the mouse,
//!   or when a large area changes (scrolling, video); small background changes (spinners,
//!   blinking carets) are batched to a few frames per second.

use anyhow::Result;
use rconn_media::capture::{Captured, Capturer};
use rconn_media::video::{EncoderSettings, VideoEncoder, should_retarget};
use rconn_media::yuv::I420;
use rconn_peer::video::{TargetBitrate, VideoSender};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, mpsc};

pub struct StreamConfig {
    pub openh264: PathBuf,
    pub max_fps: u32,
    pub max_bitrate_bps: u32,
    /// Larger desktops are halved until they fit.
    pub max_size: (usize, usize),
    /// Send large screens at half size while a big area keeps changing (scrolling, video):
    /// a quarter of the encoding work; a sharp full-size picture follows when it settles.
    pub motion_half_res: bool,
}

/// Asks the encoder for an IDR frame (viewer joined, decoder lost sync).
#[derive(Clone, Default)]
pub struct KeyframeRequests(Arc<Notify>);

impl KeyframeRequests {
    pub fn request(&self) {
        self.0.notify_one();
    }
}

/// Encode rate for small background changes (spinner, caret) when nobody is interacting.
const BACKGROUND_INTERVAL: Duration = Duration::from_millis(100);
/// Encode rate for medium background changes (an animated banner, a small video).
const MEDIUM_INTERVAL: Duration = Duration::from_millis(66);
/// Input this recent means someone is watching closely: full frame rate.
const INTERACTIVE_FOR: Duration = Duration::from_secs(2);
/// Background changes covering this share of the screen are "medium" (15 fps).
const BIG_CHANGE: f32 = 0.02;
/// Frames in a row changing this share of the screen count as motion (scrolling, a big
/// video; not an animated banner)...
const MOTION_CHANGE: f32 = 0.25;
const MOTION_FRAMES: u32 = 4;
/// ...which ends after this long without such a frame.
const MOTION_SETTLE: Duration = Duration::from_millis(700);
/// Only screens this large (in pixels) are halved during motion.
const MOTION_MIN_PIXELS: usize = 2_500_000;

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Milliseconds after `epoch()` of the last viewer input (0 = none yet).
static LAST_INPUT_MS: AtomicU64 = AtomicU64::new(0);

/// Record that the viewer sent input (key, pointer, wheel): stream at the full rate.
pub fn note_input() {
    LAST_INPUT_MS.store(epoch().elapsed().as_millis() as u64 + 1, Ordering::Relaxed);
}

fn interactive() -> bool {
    let last = LAST_INPUT_MS.load(Ordering::Relaxed);
    last != 0 && epoch().elapsed().as_millis() as u64 <= last + INTERACTIVE_FOR.as_millis() as u64
}

/// Stream until the video track stops accepting writes or `stop` is set.
pub async fn run(
    make_capturer: CapturerFactory,
    video: VideoSender,
    target: TargetBitrate,
    keyframes: KeyframeRequests,
    cfg: StreamConfig,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let frame_budget = Duration::from_secs(1) / cfg.max_fps.max(1);
    let want_key = Arc::new(AtomicBool::new(false));
    let forward = {
        let want_key = want_key.clone();
        tokio::spawn(async move {
            loop {
                keyframes.0.notified().await;
                want_key.store(true, Ordering::Relaxed);
            }
        })
    };
    // Two frames in flight at most: if the network can't keep up, encoding waits.
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(2);
    let ctx = Ctx {
        cfg,
        target,
        want_key,
        stop: stop.clone(),
        first_sent: AtomicBool::new(false),
    };
    let worker = tokio::task::spawn_blocking(move || supervise(make_capturer, &ctx, &tx));
    while let Some(au) = rx.recv().await {
        if let Err(e) = video.write(au, frame_budget).await {
            tracing::info!(error = %e, "video track closed");
            break;
        }
    }
    stop.store(true, Ordering::Relaxed);
    drop(rx); // unblocks a worker waiting to hand over a frame
    forward.abort();
    worker.await??;
    Ok(())
}

/// Builds a capturer; called again for every capture generation.
pub type CapturerFactory = Arc<dyn Fn() -> Result<Box<dyn Capturer>> + Send + Sync>;

struct Ctx {
    cfg: StreamConfig,
    target: TargetBitrate,
    want_key: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    first_sent: AtomicBool,
}

/// Each capture "generation" lives on a fresh OS thread: after a desktop switch (lock
/// screen, UAC, login) the old D3D device and the thread's desktop binding are stale, and
/// a thread that has created D3D objects can't reliably move to another desktop. So on
/// any capture error the generation ends and a new one starts (with a new encoder, whose
/// first frame is a keyframe).
fn supervise(make: CapturerFactory, ctx: &Ctx, tx: &mpsc::Sender<Vec<u8>>) -> Result<()> {
    let mut backoff = Duration::from_millis(250);
    let mut generation = 0u32;
    while !ctx.stop.load(Ordering::Relaxed) {
        generation += 1;
        let outcome = std::thread::scope(|s| {
            std::thread::Builder::new()
                .name(format!("rconn-stream-{generation}"))
                .spawn_scoped(s, || run_generation(&*make, ctx, tx))
                .map(|t| t.join())
        })?;
        match outcome {
            Ok(Ok(true)) => backoff = Duration::from_millis(250), // produced frames first
            Ok(Ok(false)) => {}
            Ok(Err(e)) => {
                tracing::warn!(error = %format!("{e:#}"), generation, "capture restarting")
            }
            Err(_) => tracing::error!(generation, "stream thread panicked; restarting"),
        }
        if ctx.stop.load(Ordering::Relaxed) || tx.is_closed() {
            break;
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(2));
    }
    Ok(())
}

/// Changes captured but not yet encoded.
struct Pending {
    rows: (usize, usize),
    changed: f32,
    captured_at: Instant,
}

impl Pending {
    fn merge(p: Option<Pending>, c: Captured) -> Pending {
        match p {
            None => Pending {
                rows: c.rows,
                changed: c.changed,
                captured_at: c.captured_at,
            },
            Some(p) => Pending {
                rows: (p.rows.0.min(c.rows.0), p.rows.1.max(c.rows.1)),
                changed: (p.changed + c.changed).min(1.0),
                captured_at: p.captured_at,
            },
        }
    }
}

/// One capturer's lifetime. `Ok(true)` if it produced frames before ending.
fn run_generation(
    make: &(dyn Fn() -> Result<Box<dyn Capturer>> + Send + Sync),
    ctx: &Ctx,
    tx: &mpsc::Sender<Vec<u8>>,
) -> Result<bool> {
    let cfg = &ctx.cfg;
    let budget = Duration::from_secs(1) / cfg.max_fps.max(1);
    let mut cap = make()?;
    let mut enc = VideoEncoder::new(
        &cfg.openh264,
        EncoderSettings {
            bitrate_bps: (ctx.target.get() as u32).min(cfg.max_bitrate_bps),
            max_fps: cfg.max_fps as f32,
            threads: 0,
            slice_bytes: None,
        },
    )?;
    let mut picture: Option<I420> = None;
    let mut small: Option<I420> = None;
    let mut motion = false;
    let mut big_streak = 0u32;
    let mut last_big = Instant::now();
    let mut pending: Option<Pending> = None;
    let mut produced = false;
    let started = Instant::now();
    let mut last_encode = Instant::now() - Duration::from_secs(1);
    let mut retarget_at = Instant::now() + Duration::from_secs(1);
    let mut last_change = Instant::now();
    let mut warned = false;
    // The first keyframe can go out before the viewer is ready to receive video, and an
    // idle screen sends nothing else: repeat the current picture as a keyframe shortly
    // after, so a picture always arrives within about a second.
    let mut resends: Vec<Instant> = Vec::new();
    let mut stats = Stats::default();

    while !ctx.stop.load(Ordering::Relaxed) {
        // Never take frames faster than max_fps: the OS keeps collecting changes meanwhile.
        let next_slot = last_encode + budget;
        let now = Instant::now();
        if now < next_slot {
            std::thread::sleep(next_slot - now);
        }
        // Wait for a change, but wake for a due batch, resend, or keyframe request.
        let due = pending
            .as_ref()
            .map(|p| {
                last_encode
                    + if p.changed >= BIG_CHANGE {
                        MEDIUM_INTERVAL
                    } else {
                        BACKGROUND_INTERVAL
                    }
            })
            .into_iter()
            .chain(resends.first().copied())
            .min();
        let timeout = due
            .map(|d| d.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_millis(100))
            .clamp(Duration::from_millis(1), Duration::from_millis(100));
        let t0 = Instant::now();
        match cap.next_into(timeout, &mut picture, cfg.max_size) {
            Ok(Some(c)) => {
                stats.capture += t0.elapsed();
                stats.captures += 1;
                if !produced {
                    let (w, h) = picture.as_ref().map_or((0, 0), I420::dimensions);
                    tracing::info!(
                        w,
                        h,
                        after_ms = started.elapsed().as_millis() as u64,
                        "first frame captured"
                    );
                }
                produced = true;
                warned = false;
                last_change = Instant::now();
                pending = Some(Pending::merge(pending.take(), c));
            }
            Ok(None) => {
                // An idle desktop is normal; never getting a first frame is not.
                if !warned && last_change.elapsed() > Duration::from_secs(5) {
                    warned = true;
                    tracing::warn!(
                        "no screen update for 5 s (idle desktop, or no access to the interactive desktop)"
                    );
                }
            }
            Err(e) if produced => {
                // A capturer that worked and then failed: typically a desktop switch.
                tracing::info!(error = %e, "capture lost (desktop switch?); restarting");
                return Ok(true);
            }
            Err(e) => return Err(e.into()),
        }
        let Some(pic) = picture.as_ref() else {
            continue;
        };

        let now = Instant::now();
        let mut force = ctx.want_key.swap(false, Ordering::Relaxed);
        if resends.first().is_some_and(|&t| now >= t) {
            resends.remove(0);
            force = true;
        }
        if force {
            enc.request_keyframe();
        }
        let (w, h) = pic.dimensions();
        let may_halve = cfg.motion_half_res && w * h >= MOTION_MIN_PIXELS;
        // Motion over: one sharp full-size picture.
        let sharpen = motion && last_big.elapsed() >= MOTION_SETTLE;
        if sharpen {
            motion = false;
            big_streak = 0;
        }
        // Full rate while someone interacts or a big area moves; background animation
        // gets fewer frames (each full-screen encode costs the same, however small the
        // change).
        let changed = pending.as_ref().map_or(0.0, |p| p.changed);
        let urgent = interactive() || changed >= MOTION_CHANGE;
        let interval = if urgent {
            budget
        } else if changed >= BIG_CHANGE {
            MEDIUM_INTERVAL.max(budget)
        } else {
            BACKGROUND_INTERVAL.max(budget)
        };
        let encode_now = force || sharpen || (pending.is_some() && now >= last_encode + interval);
        if !encode_now {
            continue;
        }

        if now >= retarget_at {
            retarget_at = now + Duration::from_secs(1);
            let want = (ctx.target.get() as u32).clamp(300_000, cfg.max_bitrate_bps);
            if should_retarget(enc.settings().bitrate_bps, want) {
                tracing::info!(
                    from_kbps = enc.settings().bitrate_bps / 1000,
                    to_kbps = want / 1000,
                    "retarget encoder"
                );
                enc.set_bitrate(want);
            }
        }
        let p = pending.take();
        let rows = p.as_ref().map_or((0, 0), |p| p.rows);
        if p.as_ref().is_some_and(|p| p.changed >= MOTION_CHANGE) {
            last_big = now;
            big_streak += 1;
        } else if p.is_some() {
            big_streak = 0;
        }
        if may_halve && !motion && big_streak >= MOTION_FRAMES {
            motion = true;
        }
        let t0 = Instant::now();
        let encoded = if motion && may_halve {
            pic.half_into(&mut small);
            stats.half += 1;
            enc.encode_yuv(small.as_ref().expect("allocated by half_into"), None)?
        } else {
            enc.encode_yuv(pic, Some(rows))?
        };
        stats.encode += t0.elapsed();
        last_encode = Instant::now();
        if urgent {
            stats.urgent += 1;
        }
        if let Some(au) = encoded {
            if !ctx.first_sent.swap(true, Ordering::Relaxed) {
                let now = Instant::now();
                resends = [300, 1000, 2500]
                    .map(|ms| now + Duration::from_millis(ms))
                    .to_vec();
                tracing::info!(
                    bytes = au.len(),
                    key = enc.last_was_key(),
                    engine = enc.engine_name(),
                    "first frame encoded, sending"
                );
            }
            stats.bytes += au.len();
            stats.frames += 1;
            if let Some(p) = &p {
                stats.latency += p.captured_at.elapsed();
                stats.changed += f64::from(p.changed);
            }
            if tx.blocking_send(au).is_err() {
                break; // the session ended
            }
        }
        stats.maybe_log(pic.dimensions(), ctx.target.get());
    }
    Ok(produced)
}

/// CPU seconds used so far by (this process, the calling thread). Windows only.
#[cfg(windows)]
#[allow(unsafe_code)] // Win32 FFI: GetProcessTimes, GetThreadTimes
fn cpu_times() -> (f64, f64) {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentThread, GetProcessTimes, GetThreadTimes,
    };
    let secs = |k: FILETIME, u: FILETIME| {
        let t = |f: FILETIME| u64::from(f.dwHighDateTime) << 32 | u64::from(f.dwLowDateTime);
        (t(k) + t(u)) as f64 / 1e7
    };
    let (mut c, mut e, mut k, mut u) = Default::default();
    let process = unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) }
        .map_or(0.0, |()| secs(k, u));
    let thread = unsafe { GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) }
        .map_or(0.0, |()| secs(k, u));
    (process, thread)
}

#[cfg(not(windows))]
fn cpu_times() -> (f64, f64) {
    (0.0, 0.0)
}

#[derive(Default)]
struct Stats {
    frames: u32,
    /// Frames sent at the full rate (interaction or a big change).
    urgent: u32,
    /// Frames sent at half size (motion).
    half: u32,
    captures: u32,
    bytes: usize,
    capture: Duration,
    encode: Duration,
    latency: Duration,
    changed: f64,
    since: Option<Instant>,
    /// Process and stream-thread CPU seconds at `since`.
    cpu_at_start: (f64, f64),
}

impl Stats {
    fn maybe_log(&mut self, (w, h): (usize, usize), target_bps: f64) {
        if self.since.is_none() {
            self.since = Some(Instant::now());
            self.cpu_at_start = cpu_times();
        }
        let secs = self.since.expect("set above").elapsed().as_secs_f64();
        if secs < 5.0 {
            return;
        }
        let (process, thread) = cpu_times();
        let pct = |now: f64, then: f64| format!("{:.0}", (now - then) / secs * 100.0);
        let n = f64::from(self.frames.max(1));
        let per = |d: Duration, n: f64| format!("{:.1}", d.as_secs_f64() * 1000.0 / n);
        tracing::info!(
            size = format!("{w}x{h}"),
            fps = format!("{:.1}", f64::from(self.frames) / secs),
            urgent_fps = format!("{:.1}", f64::from(self.urgent) / secs),
            half_size_fps = format!("{:.1}", f64::from(self.half) / secs),
            kbps = format!("{:.0}", self.bytes as f64 * 8.0 / secs / 1000.0),
            changed_pct = format!("{:.1}", self.changed * 100.0 / n),
            capture_ms = per(self.capture, f64::from(self.captures.max(1))),
            encode_ms = per(self.encode, n),
            capture_to_send_ms = per(self.latency, n),
            gcc_kbps = format!("{:.0}", target_bps / 1000.0),
            // 100 = one core. "thread" is capture + conversion + handing frames to the
            // encoder; the rest of "process" is the encoder's own threads and networking.
            cpu_process_pct = pct(process, self.cpu_at_start.0),
            cpu_thread_pct = pct(thread, self.cpu_at_start.1),
            "stream"
        );
        *self = Self::default();
    }
}
