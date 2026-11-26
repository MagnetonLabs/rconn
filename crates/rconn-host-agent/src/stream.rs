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
use rconn_media::capture::{Captured, Capturer, Surface};
use rconn_media::copyrect::{Composer, Move};
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

/// Asks the encoder for an IDR frame (viewer joined, decoder lost sync), and carries the
/// viewer's other wishes for the stream.
#[derive(Clone, Default)]
pub struct KeyframeRequests {
    notify: Arc<Notify>,
    copy_rects: Arc<AtomicBool>,
}

impl KeyframeRequests {
    pub fn request(&self) {
        self.notify.notify_one();
    }

    /// The viewer composes its picture from copy instructions (see
    /// [`rconn_media::copyrect`]): scrolls and window moves stop being re-encoded.
    pub fn enable_copy_rects(&self) {
        if !self.copy_rects.swap(true, Ordering::Relaxed) {
            tracing::info!("viewer takes copy instructions: scrolls and moves sent as copies");
        }
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
/// A new capture's first keyframe is sent again after this long if nothing followed it.
const STILL_REPEAT: Duration = Duration::from_secs(1);
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
    let copy_rects = keyframes.copy_rects.clone();
    let forward = {
        let want_key = want_key.clone();
        tokio::spawn(async move {
            loop {
                keyframes.notify.notified().await;
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
        copy_rects,
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
    /// The viewer takes copy instructions ([`KeyframeRequests::enable_copy_rects`]).
    copy_rects: Arc<AtomicBool>,
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
    // A hardware encoder that reads the capturer's GPU surfaces (macOS VideoToolbox;
    // Windows: a D3D11-aware Media Foundation encoder, fed by the GPU's video processor)
    // gets them as they are: no CPU conversion, and no half-size motion frames (a CPU
    // saving it doesn't need). Otherwise pictures are converted in memory, as before.
    let surfaces = enc.wants_surfaces() && cap.use_surfaces(cfg.max_size);
    let mut surface: Option<Surface> = None;
    let mut picture: Option<I420> = None;
    let mut small: Option<I420> = None;
    // Copy instructions (scrolls and moves as copies), once the viewer asks for them.
    // Pictures only: GPU surfaces go to the encoder untouched.
    let mut composer: Option<Composer> = None;
    let mut hints: Vec<Move> = Vec::new();
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
    // This capture's first frame has gone out; a repeat of it is due if the screen stays
    // still (see below).
    let mut sent_any = false;
    let mut repeat_if_still = false;
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
        let got = if surfaces {
            cap.next_surface(timeout).map(|r| {
                r.map(|(s, c)| {
                    surface = Some(s);
                    c
                })
            })
        } else {
            cap.next_into(timeout, &mut picture, cfg.max_size)
        };
        hints.extend(cap.take_moves());
        hints.truncate(64);
        match got {
            Ok(Some(c)) => {
                stats.capture += t0.elapsed();
                stats.captures += 1;
                if !produced {
                    let (w, h) = picture_size(&picture, &surface).unwrap_or((0, 0));
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
        let Some((w, h)) = picture_size(&picture, &surface) else {
            continue;
        };

        let now = Instant::now();
        let mut force = ctx.want_key.swap(false, Ordering::Relaxed);
        if resends.first().is_some_and(|&t| now >= t) {
            resends.remove(0);
            force = true;
        }
        if composer.is_none() && !surfaces && ctx.copy_rects.load(Ordering::Relaxed) {
            composer = Some(Composer::new()); // its first picture is whole
        }
        if force {
            enc.request_keyframe();
            if let Some(c) = composer.as_mut() {
                c.request_full();
            }
        }
        // Not with copy instructions: they keep scrolls cheap at full size, and a size
        // switch costs a keyframe.
        let may_halve =
            !surfaces && composer.is_none() && cfg.motion_half_res && w * h >= MOTION_MIN_PIXELS;
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
        let encoded = match (&surface, &picture) {
            (Some(s), _) => enc.encode_surface(s)?,
            (None, Some(pic)) if motion && may_halve => {
                pic.half_into(&mut small);
                stats.half += 1;
                enc.encode_yuv(small.as_ref().expect("allocated by half_into"), None)?
            }
            (None, Some(pic)) => match composer.as_mut() {
                Some(c) => {
                    let (canvas, found) = c.prepare(pic, &std::mem::take(&mut hints));
                    match enc.encode_yuv(canvas, found.rows)? {
                        Some(au) => {
                            stats.moved += f64::from(c.last().moved);
                            Some(c.sent(pic, au))
                        }
                        None => {
                            // openh264's rate control drops frames for good; another
                            // encoder may hand the picture out later.
                            let was = c.is_passthrough();
                            c.no_output(enc.engine_name() == "openh264");
                            if !was && c.is_passthrough() {
                                tracing::info!(
                                    engine = %enc.engine_name(),
                                    "encoder gave no picture; copy instructions off for this capture"
                                );
                            }
                            None
                        }
                    }
                }
                None => {
                    hints.clear();
                    enc.encode_yuv(pic, Some(rows))?
                }
            },
            (None, None) => unreachable!("checked above"),
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
            } else if !sent_any {
                // A new capture (the screen changed size, a desktop switch): its first
                // frame is a keyframe at a new size. Lost on a still screen, nothing would
                // follow it for the viewer to notice the gap by: send it once more unless
                // other frames go out meanwhile.
                resends = vec![Instant::now() + STILL_REPEAT];
                repeat_if_still = true;
            } else if repeat_if_still && !force {
                // Later frames went out: a loss shows as a gap, and the viewer asks.
                resends.clear();
                repeat_if_still = false;
            }
            sent_any = true;
            stats.bytes += au.len();
            stats.frames += 1;
            if let Some(p) = &p {
                stats.latency += p.captured_at.elapsed();
                stats.changed += f64::from(p.changed);
            }
            if tx.blocking_send(au).is_err() {
                break; // the session ended
            }
        } else if let Some(p) = p {
            // The rate controller dropped the frame (VideoToolbox's low-latency mode does
            // under load): its change still has to reach the viewer, with the next frame.
            pending = Some(p);
        }
        stats.maybe_log((w, h), ctx.target.get());
    }
    Ok(produced)
}

/// The size of the current picture: a converted one, or a GPU surface (even part).
fn picture_size(picture: &Option<I420>, surface: &Option<Surface>) -> Option<(usize, usize)> {
    match (picture, surface) {
        (Some(p), _) => Some(p.dimensions()),
        (None, Some(s)) => {
            let (w, h) = s.dimensions();
            Some((w & !1, h & !1))
        }
        (None, None) => None,
    }
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
    /// Share of the screen sent as copy instructions instead of pixels (summed per frame).
    moved: f64,
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
            moved_pct = format!("{:.1}", self.moved * 100.0 / n),
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
