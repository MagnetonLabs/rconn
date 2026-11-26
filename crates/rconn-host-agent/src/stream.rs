//! One session's screen stream. A single thread captures, converts and encodes; the
//! session task only writes the encoded frames to the video track.
//!
//! CPU is spent only on what changed and only when it is worth showing:
//! * capture is pulled (a frame is taken only when the encoder will use it), and only the
//!   areas the OS reports as changed are copied and converted;
//! * frames are encoded at the full rate while the viewer is typing or moving the mouse,
//!   or when a large area changes (scrolling, video); small background changes (spinners,
//!   blinking carets) are batched to a few frames per second;
//! * once the screen is still, refinement passes sharpen what the viewer has (text sent
//!   coarsely while it changed, scrolled or moved), within the bandwidth estimate
//!   ([`rconn_media::refine`]; `RCONN_REFINE=0` turns them off).

use anyhow::Result;
use rconn_media::capture::{Captured, Capturer, Surface};
use rconn_media::copyrect::{Composer, Move};
use rconn_media::refine::{self, Refiner};
use rconn_media::video::{EncoderSettings, VideoEncoder, should_retarget, should_retarget_costly};
use rconn_media::yuv::{I420, Scaler};
use rconn_peer::frames::{self, Feedback, FrameTag};
use rconn_peer::video::{TargetBitrate, VideoSender};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
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
    /// openh264 even where a hardware encoder exists (tests; `RCONN_ENCODER=openh264`).
    pub software: bool,
}

/// The viewer's requests to the encoder: an IDR frame (viewer joined, decoder lost
/// sync), per-frame feedback (frames decoded intact; a loss to repair), and its other
/// wishes for the stream.
#[derive(Clone, Default)]
pub struct KeyframeRequests {
    notify: Arc<Notify>,
    feedback: Arc<Mutex<Vec<Feedback>>>,
    copy_rects: Arc<AtomicBool>,
    view: Arc<AtomicU64>,
}

impl KeyframeRequests {
    /// `Control::ViewSize`: the viewer shows the picture this many pixels large.
    pub fn set_view_size(&self, width: u32, height: u32) {
        let packed = u64::from(width) << 32 | u64::from(height);
        if self.view.swap(packed, Ordering::Relaxed) != packed {
            tracing::debug!(width, height, "viewer shows the picture at this size");
        }
    }

    pub fn request(&self) {
        self.notify.notify_one();
    }

    /// `Control::FrameAck` from the viewer.
    pub fn frame_feedback(&self, f: Feedback) {
        let mut q = self.feedback.lock().expect("feedback");
        if q.len() < 64 {
            q.push(f);
        }
    }

    /// The viewer composes its picture from copy instructions (see
    /// [`rconn_media::copyrect`]): scrolls and window moves stop being re-encoded.
    pub fn enable_copy_rects(&self) {
        if !self.copy_rects.swap(true, Ordering::Relaxed) {
            tracing::info!("viewer takes copy instructions: scrolls and moves sent as copies");
        }
    }
}

/// How often the encoder follows the bandwidth estimate (changes are live and cheap).
const RETARGET_EVERY: Duration = Duration::from_millis(200);
/// ...or this often, where a bitrate change costs a keyframe (Microsoft's software Media
/// Foundation encoder).
const COSTLY_RETARGET_EVERY: Duration = Duration::from_secs(2);

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
/// (Each switch between half and full size costs a keyframe, so both ways have slack.)
const MOTION_FRAMES: u32 = 6;
/// ...which ends after this long without such a frame.
const MOTION_SETTLE: Duration = Duration::from_millis(1000);
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
    let view = keyframes.view.clone();
    let forward = {
        let want_key = want_key.clone();
        let keyframe = keyframes.notify.clone();
        tokio::spawn(async move {
            loop {
                keyframe.notified().await;
                want_key.store(true, Ordering::Relaxed);
            }
        })
    };
    // Two frames in flight at most: if the network can't keep up, encoding waits.
    let (tx, mut rx) = mpsc::channel::<(Vec<u8>, Instant)>(2);
    let sending = Arc::new(SendClock::default());
    let ctx = Ctx {
        cfg,
        target,
        want_key,
        feedback: keyframes.feedback.clone(),
        stop: stop.clone(),
        first_sent: AtomicBool::new(false),
        next_id: AtomicU32::new(0),
        sending: sending.clone(),
        copy_rects,
        view,
    };
    let worker = tokio::task::spawn_blocking(move || supervise(make_capturer, &ctx, &tx));
    while let Some((au, encoded_at)) = rx.recv().await {
        let t0 = Instant::now();
        let result = video.write(au, frame_budget).await;
        sending.note(t0 - encoded_at, t0.elapsed());
        if let Err(e) = result {
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
    feedback: Arc<Mutex<Vec<Feedback>>>,
    stop: Arc<AtomicBool>,
    first_sent: AtomicBool,
    /// The viewer takes copy instructions ([`KeyframeRequests::enable_copy_rects`]).
    copy_rects: Arc<AtomicBool>,
    /// Id of the next frame of the stream (ids run on across capture generations).
    next_id: AtomicU32,
    sending: Arc<SendClock>,
    /// The viewer's picture size ([`KeyframeRequests::set_view_size`]); 0 = not told.
    view: Arc<AtomicU64>,
}

impl Ctx {
    fn view_size(&self) -> Option<(usize, usize)> {
        let v = self.view.load(Ordering::Relaxed);
        let (w, h) = ((v >> 32) as usize, (v & 0xffff_ffff) as usize);
        (w > 0 && h > 0).then_some((w, h))
    }
}

/// `RCONN_VIEW_SCALE`: a CPU encoder sends the picture at the viewer's size ("on", the
/// default), and also one full-size picture once the screen has been still for
/// [`REFINE_AFTER`] ("refine"), or always the full screen ("0" / "off").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ViewScale {
    Off,
    On,
    Refine,
}

impl ViewScale {
    fn from_env() -> Self {
        match std::env::var("RCONN_VIEW_SCALE").as_deref() {
            Ok("0" | "off") => Self::Off,
            Ok("refine") => Self::Refine,
            _ => Self::On,
        }
    }
}

/// Scaled pictures may be coarse while they move (openh264 allows a higher quantizer for
/// them): once the screen is still, the same picture is encoded this many times more, each
/// sharper (720p text after a scroll: 25.5 dB, then 31 dB after 6;
/// `encode_bench::sharpness_after_scrolling`).
const TOPOFF_FRAMES: u32 = 6;

/// With [`ViewScale::Refine`], a still screen gets one full-size picture after this long.
const REFINE_AFTER: Duration = Duration::from_secs(2);
/// Scaling to less than this share of the screen's width is worth a size switch (each
/// costs a keyframe); closer to full size, the full picture is sent.
const VIEW_SCALE_BELOW: f32 = 0.9;
/// Encoding taking this share of the frame interval (averaged) counts as CPU-bound: the
/// encoder then gets fewer pixels ([`CpuCap`]).
const CPU_BOUND: f64 = 0.7;
/// [`CpuCap`] aims at this share of the frame interval when it lowers the size...
const CPU_AIM: f64 = 0.5;
/// ...and raises it again when encoding takes less than this share for a while.
const CPU_IDLE: f64 = 0.25;
/// [`CpuCap`] never goes below this many pixels (640x360).
const MIN_CAP_PIXELS: usize = 640 * 360;

/// Keeps an encoder that can't keep up within the frame interval by capping the pixels
/// it gets, whatever size the viewer shows the picture at: openh264 on a 4-vCPU VM took
/// 85-143 ms a picture at 2992x1878 (4-5 fps), 11-16 ms at 1152x722 (23-30 fps).
#[derive(Debug, Default)]
struct CpuCap {
    max_pixels: Option<usize>,
    /// Encoder time per picture (ms, moving average) at `size`, over `samples` pictures.
    avg_ms: f64,
    size: (usize, usize),
    samples: u32,
}

impl CpuCap {
    /// A picture of `size` took `ms` to encode; frames are due every `budget_ms`.
    fn note(&mut self, size: (usize, usize), ms: f64, budget_ms: f64, screen_pixels: usize) {
        if size != self.size {
            *self = Self {
                max_pixels: self.max_pixels,
                size,
                ..Self::default()
            };
        }
        self.avg_ms = if self.samples == 0 {
            ms
        } else {
            self.avg_ms * 0.9 + ms * 0.1
        };
        self.samples += 1;
        let pixels = size.0 * size.1;
        if self.samples >= 10 && self.avg_ms > budget_ms * CPU_BOUND {
            let fit = (pixels as f64 * budget_ms * CPU_AIM / self.avg_ms) as usize;
            let max = fit
                .min(self.max_pixels.unwrap_or(usize::MAX))
                .max(MIN_CAP_PIXELS);
            if self.max_pixels != Some(max) {
                tracing::info!(
                    encode_ms = format!("{:.1}", self.avg_ms),
                    at = format!("{}x{}", size.0, size.1),
                    max_pixels = max,
                    "encoder can't keep up; sending fewer pixels"
                );
            }
            self.max_pixels = Some(max);
            self.samples = 0; // measure the new size
        } else if self.samples >= 60
            && self.avg_ms < budget_ms * CPU_IDLE
            && let Some(max) = self.max_pixels
        {
            let up = max * 3 / 2;
            self.max_pixels = (up < screen_pixels).then_some(up);
            self.samples = 0;
        }
    }

    fn capped(&self) -> bool {
        self.max_pixels.is_some()
    }

    /// `target` (the size to encode a `w` x `h` screen at; `None`: full), made smaller
    /// if it has more pixels than the cap.
    fn apply(
        &self,
        (w, h): (usize, usize),
        target: Option<(usize, usize)>,
    ) -> Option<(usize, usize)> {
        let Some(max) = self.max_pixels else {
            return target;
        };
        let (tw, th) = target.unwrap_or((w, h));
        if tw * th <= max {
            return target;
        }
        let s = (max as f64 / (w * h) as f64).sqrt();
        view_target(
            (w, h),
            Some(((w as f64 * s) as usize, (h as f64 * s) as usize)),
        )
        .or(target)
    }
}

/// The size to encode a `w` x `h` screen at for a viewer showing it `view` large, or
/// `None` for the full size. Same shape as the screen; width a multiple of 16 (a size
/// that changes by a few pixels would cost a keyframe for nothing), height even; at most
/// a quarter of the screen's width.
pub(crate) fn view_target(
    (w, h): (usize, usize),
    view: Option<(usize, usize)>,
) -> Option<(usize, usize)> {
    let (vw, vh) = view?;
    if w < 64 || h < 64 {
        return None;
    }
    let s = (vw as f32 / w as f32).min(vh as f32 / h as f32);
    if s >= VIEW_SCALE_BELOW {
        return None;
    }
    let tw = ((w as f32 * s.max(0.25)).round() as usize)
        .next_multiple_of(16)
        .min(w & !1);
    let th = ((tw * h + w / 2) / w) & !1;
    (th >= 16).then_some((tw, th))
}

/// Time encoded frames spend waiting for the session task, and being packetized and
/// handed to the pacer (`write`), summed in microseconds, for the stats log.
#[derive(Default)]
struct SendClock {
    queue_us: AtomicU64,
    write_us: AtomicU64,
    frames: AtomicU64,
}

impl SendClock {
    fn note(&self, queued: Duration, write: Duration) {
        self.queue_us
            .fetch_add(queued.as_micros() as u64, Ordering::Relaxed);
        self.write_us
            .fetch_add(write.as_micros() as u64, Ordering::Relaxed);
        self.frames.fetch_add(1, Ordering::Relaxed);
    }

    /// (queue ms, write ms) per frame since the last call.
    fn take(&self) -> (f64, f64) {
        let n = self.frames.swap(0, Ordering::Relaxed).max(1) as f64;
        let q = self.queue_us.swap(0, Ordering::Relaxed) as f64;
        let w = self.write_us.swap(0, Ordering::Relaxed) as f64;
        (q / n / 1000.0, w / n / 1000.0)
    }
}

/// Each capture "generation" lives on a fresh OS thread: after a desktop switch (lock
/// screen, UAC, login) the old D3D device and the thread's desktop binding are stale, and
/// a thread that has created D3D objects can't reliably move to another desktop. So on
/// any capture error the generation ends and a new one starts (with a new encoder, whose
/// first frame is a keyframe).
fn supervise(
    make: CapturerFactory,
    ctx: &Ctx,
    tx: &mpsc::Sender<(Vec<u8>, Instant)>,
) -> Result<()> {
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
    tx: &mpsc::Sender<(Vec<u8>, Instant)>,
) -> Result<bool> {
    let cfg = &ctx.cfg;
    let budget = Duration::from_secs(1) / cfg.max_fps.max(1);
    let mut cap = make()?;
    let settings = EncoderSettings {
        bitrate_bps: (ctx.target.get() as u32).min(cfg.max_bitrate_bps),
        max_fps: cfg.max_fps as f32,
        threads: 0,
        slice_bytes: None,
    };
    let mut enc = if cfg.software {
        VideoEncoder::software(&cfg.openh264, settings)?
    } else {
        VideoEncoder::new(&cfg.openh264, settings)?
    };
    // Frame ids run on from the previous capture; the viewer acknowledges by id.
    let first_id = ctx.next_id.load(Ordering::Relaxed);
    enc.start_frame_ids_at(first_id);
    // A hardware encoder that reads the capturer's GPU surfaces (macOS VideoToolbox;
    // Windows: a D3D11-aware Media Foundation encoder, fed by the GPU's video processor)
    // gets them as they are: no CPU conversion, and no half-size motion frames (a CPU
    // saving it doesn't need). Otherwise pictures are converted in memory, as before.
    let surfaces = enc.wants_surfaces() && cap.use_surfaces(cfg.max_size);
    let mut surface: Option<Surface> = None;
    let mut picture: Option<I420> = None;
    let mut small: Option<I420> = None;
    // At the viewer's size: a CPU encoder's work shrinks with the pixels it gets.
    let view_scale = ViewScale::from_env();
    let mut scaler: Option<Scaler> = None;
    let mut scaled: Option<I420> = None;
    // `scaled` missed changes (the full picture went out meanwhile): redo it whole.
    let mut scaled_stale = true;
    // The last picture out was scaled; the full-size one after it (`Refine`) is sent.
    let mut sent_scaled = false;
    let mut refined = false;
    // Top-off pictures still due for the last scaled picture with a change in it.
    let mut topoff_left = 0u32;
    let mut cpu_cap = CpuCap::default();
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
    let mut retarget_at = Instant::now() + RETARGET_EVERY;
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
    // Refinement passes over a still screen (build to lossless).
    let mut refiner = refine::enabled().then(|| Refiner::new(Instant::now()));

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
            // (Only passes the encoder can make: a due time it never acts on would spin.)
            .chain(
                refiner
                    .as_ref()
                    .filter(|_| !motion && enc.can_refine())
                    .and_then(Refiner::due),
            )
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
                refined = false;
                if let Some(r) = refiner.as_mut() {
                    r.changed(last_change);
                }
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
        // The viewer's feedback: acknowledgements confirm long-term references (and that
        // this capture's first keyframe arrived: no repeats of it needed); a loss is
        // repaired at once, from a long-term reference where the encoder has one.
        let (mut repair, mut lost_key) = (false, false);
        for f in std::mem::take(&mut *ctx.feedback.lock().expect("feedback")) {
            if f.lost {
                stats.losses += 1;
                if ctx.copy_rects.load(Ordering::Relaxed) {
                    // The viewer's composed picture also missed the lost frames' copy
                    // instructions, which a repair of the decoded canvas does not bring
                    // back: a whole picture (keyframe, shown as is) resyncs both.
                    lost_key = true;
                } else {
                    enc.request_ltr_recovery(f.decoded);
                    repair = true;
                }
            } else {
                enc.ltr_acked(f.decoded);
                if sent_any && f.decoded >= first_id && !resends.is_empty() {
                    resends.clear(); // the viewer has this capture's picture
                    repeat_if_still = false;
                }
            }
        }
        let mut force = ctx.want_key.swap(false, Ordering::Relaxed) || lost_key;
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
        // A CPU encoder (or one that can't keep up) gets the picture at the size the
        // viewer shows it, and no more pixels than it can encode in time; GPU surfaces go
        // to a hardware encoder as they are.
        let mut target =
            (!surfaces && view_scale != ViewScale::Off && (enc.is_software() || cpu_cap.capped()))
                .then(|| cpu_cap.apply((w, h), view_target((w, h), ctx.view_size())))
                .flatten();
        // `Refine`: a still screen gets one sharp full-size picture, then stays so
        // until it changes.
        let refine = view_scale == ViewScale::Refine
            && target.is_some()
            && sent_scaled
            && !refined
            && pending.is_none()
            && last_change.elapsed() >= REFINE_AFTER;
        if refine {
            refined = true;
        }
        if refined {
            target = None;
        }
        // Not with copy instructions: they keep scrolls cheap at full size, and a size
        // switch costs a keyframe. Not at the viewer's size either (smaller already).
        let may_halve = !surfaces
            && composer.is_none()
            && target.is_none()
            && cfg.motion_half_res
            && w * h >= MOTION_MIN_PIXELS;
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
        // A still picture at the viewer's size: sharpen it (see `TOPOFF_FRAMES`).
        let topoff = topoff_left > 0
            && pending.is_none()
            && target.is_some()
            && now >= last_encode + BACKGROUND_INTERVAL;
        if pending.is_some() || target.is_none() {
            topoff_left = 0;
        }
        // A refinement pass: only over a still, full-size picture (a scaled one gets the
        // top-off above), and never instead of a keyframe, a repair or a change.
        let refine_qp = match refiner.as_mut() {
            Some(r)
                if !force
                    && !repair
                    && !sharpen
                    && !motion
                    && !refine
                    && target.is_none()
                    && pending.is_none() =>
            {
                enc.can_refine().then(|| r.next(now)).flatten()
            }
            _ => None,
        };
        let encode_now = force
            || repair
            || sharpen
            || refine
            || topoff
            || refine_qp.is_some()
            || (pending.is_some() && now >= last_encode + interval);
        if !encode_now {
            continue;
        }
        if let Some(qp) = refine_qp {
            enc.refine_next(qp);
            if let Some(c) = composer.as_mut() {
                // The canvas becomes the screen picture, shown whole: areas the viewer
                // only has as copies (scrolled, moved) are sharpened too.
                c.request_full();
            }
        }

        if now >= retarget_at {
            let costly = enc.bitrate_change_costs_keyframe();
            retarget_at = now
                + if costly {
                    COSTLY_RETARGET_EVERY
                } else {
                    RETARGET_EVERY
                };
            let want = (ctx.target.get() as u32).clamp(300_000, cfg.max_bitrate_bps);
            let current = enc.settings().bitrate_bps;
            let move_it = if costly {
                should_retarget_costly(current, want)
            } else {
                should_retarget(current, want)
            };
            if move_it {
                tracing::debug!(
                    from_kbps = enc.settings().bitrate_bps / 1000,
                    to_kbps = want / 1000,
                    "retarget encoder (live)"
                );
                enc.set_bitrate(want);
                stats.retargets += 1;
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
                enc.set_scaled_input(true);
                stats.half += 1;
                enc.encode_yuv(small.as_ref().expect("allocated by half_into"), None)?
            }
            (None, Some(full)) => {
                // The picture to send: the screen, or scaled down to the viewer's size.
                let (pic, rows) = match target {
                    Some(to) => {
                        if scaler.as_ref().is_none_or(|s| s.sizes() != ((w, h), to)) {
                            scaler = Some(Scaler::new((w, h), to));
                            scaled_stale = true;
                        }
                        let s = scaler.as_ref().expect("built above");
                        let changed = (!std::mem::take(&mut scaled_stale)).then_some(rows);
                        let rows = s.scale(full, &mut scaled, changed);
                        hints.clear(); // in screen coordinates
                        stats.scaled += 1;
                        (scaled.as_ref().expect("allocated by scale"), rows)
                    }
                    None => {
                        scaled_stale = true;
                        (full, rows)
                    }
                };
                sent_scaled = target.is_some();
                enc.set_scaled_input(sent_scaled);
                if topoff {
                    topoff_left -= 1;
                    stats.topoffs += 1;
                } else if sent_scaled && p.is_some() {
                    topoff_left = TOPOFF_FRAMES;
                }
                match composer.as_mut() {
                    Some(c) => {
                        if topoff {
                            // The viewer shows only the regions an instruction names: the
                            // sharper picture is shown whole.
                            c.request_full();
                        }
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
                                c.no_output(enc.engine_name().starts_with("openh264"));
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
                }
            }
            (None, None) => unreachable!("checked above"),
        };
        let took = t0.elapsed();
        stats.encode += took;
        if !surfaces && view_scale != ViewScale::Off {
            let size = target
                .or(motion
                    .then(|| small.as_ref().map(|s| s.dimensions()))
                    .flatten())
                .unwrap_or((w, h));
            cpu_cap.note(
                size,
                took.as_secs_f64() * 1e3,
                budget.as_secs_f64() * 1e3,
                w * h,
            );
        }
        last_encode = Instant::now();
        if urgent {
            stats.urgent += 1;
        }
        if let Some(au) = encoded {
            let info = enc.last_frame();
            if let Some(r) = refiner.as_mut() {
                let target = enc.settings().bitrate_bps;
                r.sent(
                    Instant::now(),
                    au.len(),
                    info.qp,
                    info.refine,
                    info.key,
                    target,
                );
            }
            if info.refine {
                stats.refines += 1;
                stats.refine_bytes += au.len();
            }
            ctx.next_id
                .store(info.id.wrapping_add(1), Ordering::Relaxed);
            stats.keys += u32::from(info.key);
            stats.repairs += u32::from(info.recovers_from.is_some());
            let host_us = p
                .as_ref()
                .map_or(t0, |p| p.captured_at)
                .elapsed()
                .as_micros()
                .min(u128::from(u32::MAX)) as u32;
            let au = frames::tag(
                au,
                &FrameTag {
                    id: info.id,
                    key: info.key,
                    recovers_from: info.recovers_from,
                    host_us,
                },
            );
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
            if tx.blocking_send((au, Instant::now())).is_err() {
                break; // the session ended
            }
        } else if refine_qp.is_some() {
            if let Some(r) = refiner.as_mut() {
                r.dropped(Instant::now()); // the encoder's rate control refused the pass
            }
        } else if let Some(p) = p {
            // The rate controller dropped the frame (VideoToolbox's low-latency mode does
            // under load): its change still has to reach the viewer, with the next frame.
            pending = Some(p);
        }
        stats.maybe_log((w, h), &ctx.target, &ctx.sending);
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

/// CPU seconds used so far by (this process, the calling thread).
#[cfg(windows)]
#[allow(unsafe_code)] // Win32 FFI: GetProcessTimes, GetThreadTimes
pub(crate) fn cpu_times() -> (f64, f64) {
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

#[cfg(unix)]
pub(crate) fn cpu_times() -> (f64, f64) {
    use rustix::time::{ClockId, clock_gettime};
    let secs = |id| {
        let t = clock_gettime(id);
        t.tv_sec as f64 + t.tv_nsec as f64 / 1e9
    };
    (secs(ClockId::ProcessCPUTime), secs(ClockId::ThreadCPUTime))
}

#[derive(Default)]
struct Stats {
    frames: u32,
    /// Frames sent at the full rate (interaction or a big change).
    urgent: u32,
    /// Frames sent at half size (motion).
    half: u32,
    /// Frames sent at the viewer's (smaller) size.
    scaled: u32,
    /// Still pictures re-sent to sharpen them (`TOPOFF_FRAMES`).
    topoffs: u32,
    captures: u32,
    /// Keyframes, loss repairs from long-term references, losses the viewer reported,
    /// live bitrate changes.
    keys: u32,
    repairs: u32,
    losses: u32,
    retargets: u32,
    /// Refinement passes over a still screen, and their bytes.
    refines: u32,
    refine_bytes: usize,
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
    fn maybe_log(&mut self, (w, h): (usize, usize), target: &TargetBitrate, sending: &SendClock) {
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
        let (queue_ms, write_ms) = sending.take();
        tracing::info!(
            size = format!("{w}x{h}"),
            fps = format!("{:.1}", f64::from(self.frames) / secs),
            urgent_fps = format!("{:.1}", f64::from(self.urgent) / secs),
            half_size_fps = format!("{:.1}", f64::from(self.half) / secs),
            view_size_fps = format!("{:.1}", f64::from(self.scaled) / secs),
            topoff_fps = format!("{:.1}", f64::from(self.topoffs) / secs),
            moved_pct = format!("{:.1}", self.moved * 100.0 / n),
            kbps = format!("{:.0}", self.bytes as f64 * 8.0 / secs / 1000.0),
            changed_pct = format!("{:.1}", self.changed * 100.0 / n),
            capture_ms = per(self.capture, f64::from(self.captures.max(1))),
            encode_ms = per(self.encode, n),
            capture_to_send_ms = per(self.latency, n),
            // Stages after the encoder: waiting for the session task, then packetizing
            // into RTP and queueing at the pacer.
            queue_ms = format!("{queue_ms:.1}"),
            write_ms = format!("{write_ms:.1}"),
            bwe_kbps = format!("{:.0}", target.estimate() / 1000.0),
            encoder_kbps = format!("{:.0}", target.get() / 1000.0),
            keyframes_per_min = format!("{:.1}", f64::from(self.keys) * 60.0 / secs),
            repairs = self.repairs,
            losses = self.losses,
            retargets = self.retargets,
            refines = self.refines,
            refine_kbps = format!("{:.0}", self.refine_bytes as f64 * 8.0 / secs / 1000.0),
            // 100 = one core. "thread" is capture + conversion + handing frames to the
            // encoder; the rest of "process" is the encoder's own threads and networking.
            cpu_process_pct = pct(process, self.cpu_at_start.0),
            cpu_thread_pct = pct(thread, self.cpu_at_start.1),
            "stream"
        );
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rconn_media::capture::TestPattern;
    use rconn_media::video::VideoDecoder;

    /// A software (openh264) host's stream state, for `run_generation`.
    fn software_ctx(lib: &std::path::Path, keyframes: &KeyframeRequests) -> Ctx {
        Ctx {
            cfg: StreamConfig {
                openh264: lib.to_owned(),
                // A slow frame rate: unoptimized builds must not look CPU-bound.
                max_fps: 5,
                max_bitrate_bps: 8_000_000,
                max_size: (3840, 2400),
                motion_half_res: false,
                software: true,
            },
            target: TargetBitrate::fixed(8_000_000.0),
            want_key: Arc::default(),
            feedback: keyframes.feedback.clone(),
            stop: Arc::default(),
            first_sent: AtomicBool::new(false),
            next_id: AtomicU32::new(0),
            sending: Arc::default(),
            copy_rects: Arc::default(),
            view: keyframes.view.clone(),
        }
    }

    /// A test pattern that changes `changes` times, then stays still.
    struct Changes {
        pattern: TestPattern,
        changes: usize,
    }

    impl Capturer for Changes {
        fn next_frame(
            &mut self,
            timeout: Duration,
        ) -> Result<Option<rconn_media::video::Frame>, rconn_media::capture::CaptureError> {
            if self.changes == 0 {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            self.changes -= 1;
            self.pattern.next_frame(timeout)
        }
    }

    /// At the viewer's size, a screen that stops changing gets `TOPOFF_FRAMES` more
    /// pictures of the same content (each sharper), then nothing.
    #[test]
    fn a_still_scaled_picture_is_topped_off() {
        let Some(lib) = rconn_media::cisco::local_binary_path().filter(|p| p.exists()) else {
            return;
        };
        let keyframes = KeyframeRequests::default();
        keyframes.set_view_size(1280, 720);
        let ctx = software_ctx(&lib, &keyframes);
        let (tx, mut rx) = mpsc::channel(4);
        let make = || -> Result<Box<dyn Capturer>> {
            Ok(Box::new(Changes {
                pattern: TestPattern::new(1920, 1080, 30),
                changes: 5,
            }))
        };
        let mut tags = Vec::new();
        std::thread::scope(|s| {
            let worker = s.spawn(|| {
                let tx = tx;
                run_generation(&make, &ctx, &tx)
            });
            let mut first = None;
            while let Some((au, _)) = rx.blocking_recv() {
                let first = *first.get_or_insert_with(Instant::now);
                tags.push(frames::read(&au).expect("tagged"));
                // Before the third repeat of the first keyframe (2.5 s).
                if first.elapsed() > Duration::from_millis(2300) {
                    ctx.stop.store(true, Ordering::Relaxed);
                }
            }
            drop(rx);
            worker.join().unwrap().unwrap();
        });
        // 5 pictures for the changes, then the top-offs (repeats of the first keyframe at
        // 300 ms and 1 s may stand in for one; openh264 may skip one after a keyframe),
        // then nothing until the next repeat at 2.5 s.
        let after = tags.len().saturating_sub(5);
        assert!(
            (3..=TOPOFF_FRAMES as usize).contains(&after),
            "{after} pictures after the changes: {tags:?}"
        );
    }

    /// A software host streaming a 1920x1080 screen to a viewer showing it 1280x720
    /// sends 1280x720 pictures; once the viewer shows it full size, full-size ones.
    #[test]
    fn a_software_encoder_sends_the_viewer_size() {
        let Some(lib) = rconn_media::cisco::local_binary_path().filter(|p| p.exists()) else {
            return;
        };
        let keyframes = KeyframeRequests::default();
        keyframes.set_view_size(1280, 720);
        let ctx = software_ctx(&lib, &keyframes);
        let (tx, mut rx) = mpsc::channel(4);
        let make =
            || -> Result<Box<dyn Capturer>> { Ok(Box::new(TestPattern::new(1920, 1080, 30))) };
        let mut decoder = VideoDecoder::software(&lib).unwrap();
        let mut sizes = Vec::new();
        std::thread::scope(|s| {
            let worker = s.spawn(|| {
                let tx = tx; // closes the channel when the generation ends
                run_generation(&make, &ctx, &tx)
            });
            while let Some((au, _)) = rx.blocking_recv() {
                if let Some(f) = decoder.decode(&au).unwrap() {
                    sizes.push((f.width, f.height));
                }
                if sizes.len() == 10 {
                    keyframes.set_view_size(2560, 1440); // HiDPI, full screen
                }
                if sizes.len() == 20 {
                    ctx.stop.store(true, Ordering::Relaxed);
                }
            }
            drop(rx);
            worker.join().unwrap().unwrap();
        });
        assert!(sizes[..10].iter().all(|&s| s == (1280, 720)), "{sizes:?}");
        // The switch back takes effect with the next picture encoded.
        assert!(sizes[11..].iter().all(|&s| s == (1920, 1080)), "{sizes:?}");
    }

    /// An encoder taking 90 ms a picture at 2992x1878 is capped until it takes about
    /// half the 33 ms interval; one with time to spare gets the full screen back.
    #[test]
    fn a_slow_encoder_gets_fewer_pixels_and_a_fast_one_more() {
        let screen = (2992, 1878);
        let px = screen.0 * screen.1;
        let mut cap = CpuCap::default();
        for _ in 0..10 {
            cap.note(screen, 90.0, 33.3, px);
        }
        let to = cap.apply(screen, None).expect("capped");
        assert!(to.0 * to.1 <= px * 20 / 100, "{to:?}");
        assert_eq!(to.0 * screen.1 / to.1 / 8, screen.0 / 8, "same shape");
        // A viewer showing it even smaller wins.
        assert_eq!(cap.apply(screen, Some((640, 400))), Some((640, 400)));
        // Fast at the capped size: the cap rises step by step and goes.
        for _ in 0..20 {
            for _ in 0..60 {
                let size = cap.apply(screen, None).unwrap_or(screen);
                cap.note(size, 5.0, 33.3, px);
            }
        }
        assert!(!cap.capped());
    }

    #[test]
    fn view_target_keeps_the_shape_and_skips_small_savings() {
        let screen = (1920, 1080);
        assert_eq!(view_target(screen, None), None);
        // Bigger than the screen, or within 10 % of it: the full picture.
        assert_eq!(view_target(screen, Some((2560, 1440))), None);
        assert_eq!(view_target(screen, Some((1800, 1100))), None);
        assert_eq!(view_target(screen, Some((1280, 720))), Some((1280, 720)));
        // A tall window: the width decides, the shape stays the screen's.
        assert_eq!(view_target(screen, Some((1000, 900))), Some((1008, 566)));
        // Never below a quarter.
        assert_eq!(view_target(screen, Some((100, 60))), Some((480, 270 & !1)));
        // A few pixels either way: the same size (no keyframe for nothing).
        assert_eq!(
            view_target(screen, Some((1275, 717))),
            view_target(screen, Some((1272, 716)))
        );
    }
}
