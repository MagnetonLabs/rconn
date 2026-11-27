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
    /// The codec negotiated with the viewer ([`VideoSender::codec`]).
    pub codec: rconn_media::caps::VideoCodec,
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

/// How often the encoder may follow the bandwidth estimate (changes are live and cheap,
/// but each upsets the rate controller for a few frames: a 200 ms period made 99 changes
/// in 115 s of a live session).
const RETARGET_EVERY: Duration = Duration::from_secs(1);
/// ...or this often, where a bitrate change costs a keyframe (an engine seen to answer
/// changes with one, [`VideoEncoder::bitrate_change_costs_keyframe`]).
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
/// Microsoft's software encoder scrolls a plain page well (11-28 KB a picture at 8
/// Mbps), but codes dense or redrawn content as new pictures (0.5-1.2 MB a scrolled
/// picture of dense text, 2.8 MB a redrawn canvas of line art: seconds on an 8 Mbps
/// link). A picture of a large change from it this many times
/// its share of the bitrate (a quarter of a second on the link; a page switching came
/// out at 4-5 times) is not sent: the same change goes out at once at half size from
/// openh264 (`VideoEncoder::set_motion_input`; a keyframe, which the viewer takes after
/// the skipped frame id), and large changes stay there until still.
const MF_OVERSIZE: usize = 8;
/// Back to Microsoft's encoder at full size after this long without a large change, and
/// only once the link is free (each switch is a keyframe: a live session that switched
/// back after 1 s made 35 size keyframes in 2 minutes of drawing, the full-size ones
/// holding the link for up to 3.4 s)...
const MF_MOTION_SETTLE: Duration = Duration::from_secs(3);
/// ...twice as long each time motion comes back this soon after switching back (a
/// session of scrolls and drawing makes one round trip, not one per pause), up to
/// [`MF_MOTION_SETTLE_MAX`]; after a longer calm it starts over.
const MF_MOTION_RETURN: Duration = Duration::from_secs(15);
const MF_MOTION_SETTLE_MAX: Duration = Duration::from_secs(16);
/// The switch back also waits for the screen to be still this long: its keyframe can be
/// large (199 KB a page of text at 2992x1878, 2.3 MB with a canvas of line art), and
/// typing behind it would wait for it on the link. Someone typing on and on gets it
/// [`MF_BACK_ANYWAY`] after the settle (never sharp text while typing otherwise).
const MF_BACK_STILL: Duration = Duration::from_secs(1);
const MF_BACK_ANYWAY: Duration = Duration::from_secs(3);

/// Microsoft's encoder's half-size spell starts: how long it must be over before the
/// switch back (`settle`: the last one's; `back`: when it last switched back).
fn mf_settle_for(settle: Duration, back: Option<Instant>, now: Instant) -> Duration {
    match back {
        Some(t) if now.saturating_duration_since(t) < MF_MOTION_RETURN => {
            (settle * 2).min(MF_MOTION_SETTLE_MAX)
        }
        _ => MF_MOTION_SETTLE,
    }
}

/// Microsoft's encoder at half size: switch back now? `since_big`: since the last large
/// change; `since_change`: since any; `link_free`: nothing sent is still on the link.
fn mf_switch_back(
    settle: Duration,
    since_big: Duration,
    since_change: Duration,
    link_free: bool,
) -> bool {
    since_big >= settle
        && (since_change >= MF_BACK_STILL || since_big >= settle + MF_BACK_ANYWAY)
        && link_free
}
/// Changes covering at least this share of the screen, coming one after another (each
/// within [`ANIMATION_GAP`] of the last, [`ANIMATION_FRAMES`] of them), are something
/// playing that someone watches (a video in a window, a terminal printing): the full
/// rate, input or not. A spinner or a blinking caret stays batched.
const ANIMATION_CHANGE: f32 = 0.005;
const ANIMATION_FRAMES: u32 = 4;
const ANIMATION_GAP: Duration = Duration::from_millis(250);
/// A new capture's first keyframe is sent again after this long if nothing followed it.
const STILL_REPEAT: Duration = Duration::from_secs(1);
/// The first keyframe's repeats only go out on a stream this quiet: while frames flow, a
/// viewer missing the keyframe sees the gap and asks.
const RESEND_QUIET: Duration = Duration::from_millis(250);
/// A loss report from before the last keyframe (or repair) is about frames that keyframe
/// already replaced: ignored for this long after it went out, plus its time on the link.
/// (Later, the keyframe may have been lost too: the report counts.)
const RESYNC_GRACE: Duration = Duration::from_millis(500);
/// A new picture waits while what was already sent needs more than this to leave at the
/// encoder's bitrate: an encoder that overshoots (Microsoft's software encoder scrolling:
/// 575 KB pictures at 8 Mbps) then costs frame rate, never a growing delay (a queue
/// that freezes the picture for seconds).
const LINK_AHEAD: Duration = Duration::from_millis(100);
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
/// `encode_bench::sharpness_after_scrolling`). Engines that take refinement passes at
/// any size (Media Foundation) get those instead.
pub(crate) const TOPOFF_FRAMES: u32 = 6;
/// Microsoft's software encoder's rate control sharpens a still picture in bursts (a
/// large picture every 8-12 re-encodes, nearly empty ones between): it is re-encoded at
/// the full rate until it has made no large picture (half a frame's share of the
/// bitrate) for this long... (2688x1686 text after scrolling: 19.7 dB, 33.8 dB after 3 s
/// at 8 Mbps; a still caret: no large pictures, done after this long)
pub(crate) const CONVERGE_QUIET: Duration = Duration::from_millis(600);
/// ...or for this long in all.
pub(crate) const CONVERGE_MAX: Duration = Duration::from_secs(6);

/// With [`ViewScale::Refine`], a still screen gets one full-size picture after this long.
const REFINE_AFTER: Duration = Duration::from_secs(2);
/// Scaling to less than this share of the screen's width is worth a size switch (each
/// costs a keyframe); closer to full size, the full picture is sent.
const VIEW_SCALE_BELOW: f32 = 0.9;

/// The size to encode a `w` x `h` screen at for a viewer showing it `view` large, or
/// `None` for the full size. Same shape as the screen; width a multiple of 16 (a size
/// that changes by a few pixels would cost a keyframe for nothing), height even; at most
/// a quarter of the screen's width.
///
/// The size only follows the viewer: never how long frames take to encode (a CPU cap
/// that did took a 2992x1878 screen down to 752x472, unreadable).
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

/// `target` (a size for a `w` x `h` screen, `None`: full), no wider than `limit`, the
/// widest picture the encoder handles well ([`VideoEncoder::width_limit`]). Same shape.
pub(crate) fn fit_width(
    (w, h): (usize, usize),
    target: Option<(usize, usize)>,
    limit: Option<usize>,
) -> Option<(usize, usize)> {
    let Some(limit) = limit else {
        return target;
    };
    if target.unwrap_or((w, h)).0 <= limit {
        return target;
    }
    let tw = limit & !15;
    Some((tw, ((tw * h + w / 2) / w) & !1))
}

/// When the next frame may start: frames start at most once per `budget`, measured from
/// the start of the frame before (not its end: a 15 ms encode at 30 fps then still makes
/// 30 fps, where stamping after encoding made it 1000 / (33 + 15) = 21). A frame later
/// than a whole slot starts a new grid: missed slots are skipped, never caught up.
struct FrameClock {
    budget: Duration,
    next: Instant,
}

impl FrameClock {
    fn new(budget: Duration, now: Instant) -> Self {
        Self { budget, next: now }
    }

    /// The earliest start of the next frame.
    fn next(&self) -> Instant {
        self.next
    }

    /// A frame started (its picture taken, its encode about to begin) at `now`: the slot
    /// it fills (`now`, or earlier on the grid). Intervals count from slots, not from
    /// starts: a start a few ms late (the picture took longer to take, a coarse sleep)
    /// must not push the next frame's due time back, or each frame waits for the
    /// screen's next present after it (a 60 Hz screen: 24 fps of 30).
    fn started(&mut self, now: Instant) -> Instant {
        let slot = if now < self.next + self.budget {
            self.next
        } else {
            now
        };
        self.next = slot + self.budget;
        slot
    }
}

/// How long after the last frame's slot a change is due: at once (the frame budget)
/// while someone interacts, a big area moves or something plays; background animation
/// gets fewer frames (each full-screen encode costs the same, however small the change).
fn change_interval(urgent: bool, changed: f32, budget: Duration) -> Duration {
    if urgent {
        budget
    } else if changed >= BIG_CHANGE {
        MEDIUM_INTERVAL.max(budget)
    } else {
        BACKGROUND_INTERVAL.max(budget)
    }
}

/// Is something playing (see [`ANIMATION_CHANGE`])?
#[derive(Default)]
struct Animation {
    streak: u32,
    last: Option<Instant>,
}

impl Animation {
    /// A capture changed this share of the screen.
    fn note(&mut self, now: Instant, changed: f32) {
        if changed < ANIMATION_CHANGE {
            return;
        }
        let close = self.last.is_some_and(|l| now - l <= ANIMATION_GAP);
        self.streak = if close { self.streak + 1 } else { 1 };
        self.last = Some(now);
    }

    fn playing(&self, now: Instant) -> bool {
        self.streak >= ANIMATION_FRAMES && self.last.is_some_and(|l| now - l <= ANIMATION_GAP)
    }
}

/// Is frame `a` before frame `b` (ids wrap)?
fn precedes(a: u32, b: u32) -> bool {
    (b.wrapping_sub(a) as i32) > 0
}

/// The last frame that resynchronized the viewer (a keyframe, or a loss repair), so that
/// reports of losses it already answered don't make another one.
#[derive(Default)]
struct Resync {
    id: u32,
    until: Option<Instant>,
    /// The same for the last keyframe: a request for one meanwhile was made before it
    /// arrived.
    key_until: Option<Instant>,
}

impl Resync {
    /// Frame `id` (`bytes` long, a keyframe or a repair) went out at `now` and replaces
    /// everything before it.
    fn sent(&mut self, id: u32, now: Instant, bytes: usize, bps: f64, key: bool) {
        let link = Duration::from_secs_f64((bytes as f64 * 8.0 / bps.max(1.0) * 1.5).min(3.0));
        self.id = id;
        self.until = Some(now + RESYNC_GRACE + link);
        if key {
            self.key_until = self.until;
        }
    }

    /// A loss after frame `decoded`: already answered?
    fn answered(&self, decoded: u32, now: Instant) -> bool {
        self.until.is_some_and(|u| now < u) && precedes(decoded, self.id)
    }

    /// A keyframe asked for: is one still on its way?
    fn key_on_its_way(&self, now: Instant) -> bool {
        self.key_until.is_some_and(|u| now < u)
    }
}

/// Why a keyframe went out (counted in the stats log).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyCause {
    /// A new encoder or capture: its first picture.
    Start,
    /// The viewer asked (`RequestKeyframe`: it joined, or has no picture).
    Request,
    /// A loss the viewer reported, not repairable otherwise.
    Loss,
    /// The first picture again, in case the viewer missed it.
    Resend,
    /// The encoded size changed.
    Size,
    /// A bitrate change (on an engine that answers them with one).
    Retarget,
    /// The encoder decided on its own.
    Encoder,
}

impl KeyCause {
    const ALL: [KeyCause; 7] = [
        KeyCause::Start,
        KeyCause::Request,
        KeyCause::Loss,
        KeyCause::Resend,
        KeyCause::Size,
        KeyCause::Retarget,
        KeyCause::Encoder,
    ];

    fn name(self) -> &'static str {
        match self {
            KeyCause::Start => "start",
            KeyCause::Request => "request",
            KeyCause::Loss => "loss",
            KeyCause::Resend => "resend",
            KeyCause::Size => "size",
            KeyCause::Retarget => "retarget",
            KeyCause::Encoder => "encoder",
        }
    }
}

/// Keyframes by cause, as "loss:2 size:1" ("-" for none).
#[derive(Default)]
struct KeyCauses([u32; KeyCause::ALL.len()]);

impl KeyCauses {
    fn add(&mut self, cause: KeyCause) {
        self.0[cause as usize] += 1;
    }
}

impl std::fmt::Display for KeyCauses {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut any = false;
        for c in KeyCause::ALL {
            let n = self.0[c as usize];
            if n > 0 {
                write!(f, "{}{}:{n}", if any { " " } else { "" }, c.name())?;
                any = true;
            }
        }
        if !any {
            f.write_str("-")?;
        }
        Ok(())
    }
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
    // The negotiated codec: H.264, or HEVC/AV1 on a hardware encoder (losses there are
    // repaired with keyframes where the encoder has no long-term references).
    enc.set_codec(cfg.codec);
    // Frame ids run on from the previous capture; the viewer acknowledges by id.
    let first_id = ctx.next_id.load(Ordering::Relaxed);
    enc.start_frame_ids_at(first_id);
    // A hardware encoder that reads the capturer's GPU surfaces (macOS VideoToolbox;
    // Windows: a D3D11-aware Media Foundation encoder, fed by the GPU's video processor;
    // Linux: VA-API, fed DMA-BUFs from the Wayland screen stream) gets them as they are:
    // no CPU conversion, and no half-size motion frames (a CPU saving it doesn't need).
    // Otherwise pictures are converted in memory, as before.
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
    // Top-off pictures still due for the last scaled picture with a change in it
    // (openh264), or since when (and the last large one) a still picture is re-encoded
    // until the encoder stops sharpening it (Microsoft's software encoder).
    let mut topoff_left = 0u32;
    let mut converge: Option<(Instant, Instant)> = None;
    // Copy instructions (scrolls and moves as copies), once the viewer asks for them.
    // Pictures only: GPU surfaces go to the encoder untouched.
    let mut composer: Option<Composer> = None;
    let mut hints: Vec<Move> = Vec::new();
    let mut motion = false;
    let mut big_streak = 0u32;
    let mut last_big = Instant::now();
    // The CPU encoder is Microsoft's software one: large changes it codes too large go
    // to openh264 at half size (see `MF_OVERSIZE`).
    let mut mf_main = false;
    let mut told_motion = false;
    // How long motion on it must be over before switching back, and when it last was.
    let mut mf_settle = MF_MOTION_SETTLE;
    let mut mf_back: Option<Instant> = None;
    // The widest picture the main CPU encoder handles well (kept while another engine
    // takes the moving pictures).
    let mut width_limit: Option<usize> = None;
    let mut animation = Animation::default();
    let mut pending: Option<Pending> = None;
    let mut produced = false;
    let started = Instant::now();
    let mut clock = FrameClock::new(budget, Instant::now());
    // Slot of the last frame encoded (intervals count from it, see `FrameClock::started`).
    let mut last_slot = Instant::now() - Duration::from_secs(1);
    let mut last_sent: Option<Instant> = None;
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
    // A refinement pass could be made at the size last chosen (only then is its due
    // time a reason to wake: one never acted on would poll the capturer every 1 ms).
    let mut refine_possible = false;
    let mut resync = Resync::default();
    // Why the keyframe being asked for is wanted, until one comes out.
    let mut owed_key: Option<KeyCause> = None;
    let mut last_size: Option<(usize, usize)> = None;
    // When what was sent has left at the encoder's bitrate (see `LINK_AHEAD`), and since
    // when a change has been held back for it.
    let mut link_free = Instant::now();
    let mut held_since: Option<Instant> = None;

    while !ctx.stop.load(Ordering::Relaxed) {
        // Never take frames faster than max_fps: the OS keeps collecting changes meanwhile.
        let now = Instant::now();
        if now < clock.next() {
            std::thread::sleep(clock.next() - now);
        }
        // Wait for a change, but wake for a due batch, resend, or keyframe request.
        let link_due = link_free.checked_sub(LINK_AHEAD).unwrap_or(link_free);
        let due = pending
            .as_ref()
            .map(|p| {
                let urgent = interactive() || p.changed >= MOTION_CHANGE || animation.playing(now);
                (last_slot + change_interval(urgent, p.changed, budget)).max(link_due)
            })
            .into_iter()
            .chain(resends.first().copied())
            // (Top-offs wait for the link too: a due time before it would poll.)
            .chain((topoff_left > 0).then(|| (last_slot + BACKGROUND_INTERVAL).max(link_due)))
            .chain(converging(converge, Instant::now()).then(|| (last_slot + budget).max(link_due)))
            .chain(
                refiner
                    .as_ref()
                    .filter(|_| refine_possible && !motion && enc.can_refine())
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
                animation.note(last_change, c.changed);
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
        // repaired at once, from a long-term reference where the encoder has one. A loss
        // from before the last keyframe or repair is one that frame already answered
        // (the viewer repeats its report until a picture comes): not answered twice.
        let (mut repair, mut lost_key) = (false, false);
        for f in std::mem::take(&mut *ctx.feedback.lock().expect("feedback")) {
            if f.lost {
                stats.losses += 1;
                if resync.answered(f.decoded, now) {
                    stats.answered += 1;
                } else if ctx.copy_rects.load(Ordering::Relaxed) {
                    // The viewer's composed picture also missed the lost frames' copy
                    // instructions, which a repair of the decoded canvas does not bring
                    // back: a whole picture (keyframe, shown as is) resyncs both.
                    lost_key = true;
                } else if !repair {
                    enc.request_ltr_recovery(f.decoded);
                    repair = true;
                }
            } else {
                enc.ltr_acked(f.decoded);
                if sent_any && !precedes(f.decoded, first_id) && !resends.is_empty() {
                    resends.clear(); // the viewer has this capture's picture
                    repeat_if_still = false;
                }
            }
        }
        let mut requested = ctx.want_key.swap(false, Ordering::Relaxed);
        if requested && resync.key_on_its_way(now) {
            // Asked before the keyframe just sent could arrive (a large one takes a
            // while); if it was lost, the viewer asks again.
            requested = false;
            stats.answered += 1;
        }
        let mut force = requested || lost_key;
        let mut cause = if requested {
            Some(KeyCause::Request)
        } else if lost_key {
            Some(KeyCause::Loss)
        } else {
            None
        };
        if resends.first().is_some_and(|&t| now >= t) {
            if link_free + RESEND_QUIET > now {
                // The keyframe is still on its way (a large one takes a while at the
                // bitrate), and so is the viewer's word that it arrived: ask again later.
                let later = link_free + RESEND_QUIET;
                for t in resends.iter_mut().filter(|t| **t < later) {
                    *t = later;
                }
            } else {
                resends.remove(0);
                // Frames flowing: a viewer without the picture sees the gap and asks.
                if last_sent.is_none_or(|t| now - t >= RESEND_QUIET) {
                    force = true;
                    cause = cause.or(Some(KeyCause::Resend));
                }
            }
        }
        if composer.is_none() && !surfaces && ctx.copy_rects.load(Ordering::Relaxed) {
            composer = Some(Composer::for_codec(cfg.codec)); // its first picture is whole
        }
        if force {
            enc.request_keyframe();
            owed_key = owed_key.or(cause);
            if let Some(c) = composer.as_mut() {
                c.request_full();
            }
        }
        // A CPU encoder gets the picture at the size the viewer shows it (no wider than it
        // encodes well); GPU surfaces go to a hardware encoder as they are.
        if !motion {
            width_limit = enc.width_limit();
        }
        let mut target = (!surfaces && view_scale != ViewScale::Off && enc.is_software())
            .then(|| fit_width((w, h), view_target((w, h), ctx.view_size()), width_limit))
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
        mf_main |= !surfaces && width_limit.is_some();
        let may_halve = !surfaces
            && composer.is_none()
            && ((target.is_none() && cfg.motion_half_res) || mf_main)
            && w * h >= MOTION_MIN_PIXELS;
        // Motion over: one sharp full-size picture (Microsoft's encoder: later, and not
        // into a busy link, see `MF_MOTION_SETTLE`).
        let sharpen = if mf_main {
            motion
                && mf_switch_back(
                    mf_settle,
                    last_big.elapsed(),
                    last_change.elapsed(),
                    link_free <= now,
                )
        } else {
            motion && last_big.elapsed() >= MOTION_SETTLE
        };
        if sharpen {
            motion = false;
            big_streak = 0;
            mf_back = Some(now);
        }
        // Full rate while someone interacts, a big area moves or something plays.
        let changed = pending.as_ref().map_or(0.0, |p| p.changed);
        let urgent = interactive() || changed >= MOTION_CHANGE || animation.playing(now);
        let interval = change_interval(urgent, changed, budget);
        // A still picture is sharpened by refinement passes where the engine codes a
        // frame at a chosen quantizer (openh264 at full size only: its camera mode, at
        // the viewer's size, gets top-offs), else by top-offs where a software encoder's
        // rate control codes each re-encode finer (Microsoft's software encoder, which
        // ignores quantizer changes while encoding).
        let passes =
            enc.can_refine() && (target.is_none() || !enc.engine_name().starts_with("openh264"));
        let topoffs = !passes && !surfaces && enc.is_software();
        let counted = enc.engine_name().starts_with("openh264");
        refine_possible = passes;
        // A still picture: sharpen it (see `TOPOFF_FRAMES`, `CONVERGE_QUIET`).
        let topoff = pending.is_none()
            && topoffs
            && if counted {
                topoff_left > 0 && now >= last_slot + BACKGROUND_INTERVAL
            } else {
                converging(converge, now)
            };
        if pending.is_some() || !topoffs {
            topoff_left = 0;
            converge = None;
        }
        // A refinement pass: only over a still picture, and never instead of a keyframe,
        // a repair or a change.
        let refine_qp = match refiner.as_mut() {
            Some(r)
                if !force
                    && !repair
                    && !sharpen
                    && !motion
                    && !refine
                    && passes
                    && pending.is_none() =>
            {
                enc.can_refine().then(|| r.next(now)).flatten()
            }
            _ => None,
        };
        // Changes and top-offs wait for the link (keyframes, repairs and refinement
        // passes don't: the refiner paces itself).
        let link_busy = link_free > now + LINK_AHEAD;
        let change_due = pending.is_some() && now >= last_slot + interval;
        if link_busy && change_due {
            held_since.get_or_insert(now);
        }
        let encode_now = force
            || repair
            || sharpen
            || refine
            || (topoff && !link_busy)
            || refine_qp.is_some()
            || (change_due && !link_busy);
        if !encode_now {
            continue;
        }
        if let Some(t) = held_since.take() {
            stats.link_held += now - t;
        }
        if now < clock.next() + budget {
            // A frame of a steady stream: how long after its slot it started.
            stats.late += now.saturating_duration_since(clock.next());
            stats.late_frames += 1;
        }
        last_slot = clock.started(now);
        if let Some(qp) = refine_qp {
            enc.refine_next(qp);
            if let Some(c) = composer.as_mut() {
                // The canvas becomes the screen picture, shown whole: areas the viewer
                // only has as copies (scrolled, moved) are sharpened too.
                c.request_full();
            }
        }

        let mut retargeted = false;
        if now >= retarget_at {
            let costly = enc.bitrate_change_costs_keyframe();
            let want = (ctx.target.get() as u32).clamp(300_000, cfg.max_bitrate_bps);
            let current = enc.settings().bitrate_bps;
            let move_it = if costly {
                should_retarget_costly(current, want)
            } else {
                should_retarget(current, want)
            };
            if move_it {
                tracing::debug!(
                    from_kbps = current / 1000,
                    to_kbps = want / 1000,
                    "retarget encoder (live)"
                );
                enc.set_bitrate(want);
                stats.retargets += 1;
                retargeted = true;
                // At most one change per period (checked again each frame until then).
                retarget_at = now
                    + if costly {
                        COSTLY_RETARGET_EVERY
                    } else {
                        RETARGET_EVERY
                    };
            }
        }
        let p = pending.take();
        let rows = p.as_ref().map_or((0, 0), |p| p.rows);
        let big = p.as_ref().is_some_and(|p| p.changed >= MOTION_CHANGE);
        if big {
            last_big = now;
            big_streak += 1;
        } else if p.is_some() {
            big_streak = 0;
        }
        if may_halve && !motion && !mf_main && big_streak >= MOTION_FRAMES {
            motion = true;
        }
        enc.set_motion_input(motion && may_halve);
        let t0 = Instant::now();
        let mut size = (w, h);
        let mut encoded = match (&surface, &picture) {
            (Some(s), _) => enc.encode_surface(s)?,
            (None, Some(pic)) if motion && may_halve => {
                pic.half_into(&mut small);
                enc.set_scaled_input(true);
                stats.half += 1;
                let half = small.as_ref().expect("allocated by half_into");
                size = half.dimensions();
                enc.encode_yuv(half, None)?
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
                size = pic.dimensions();
                sent_scaled = target.is_some();
                enc.set_scaled_input(sent_scaled);
                if topoff {
                    topoff_left = topoff_left.saturating_sub(1);
                    stats.topoffs += 1;
                } else if topoffs && p.is_some() {
                    if counted {
                        topoff_left = TOPOFF_FRAMES;
                    } else {
                        converge = Some((now, now));
                    }
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
        // Microsoft's encoder coded a large change far too large (see `MF_OVERSIZE`): not
        // sent; the same picture at half size instead.
        let share = enc.settings().bitrate_bps as usize / 8 / cfg.max_fps.max(1) as usize;
        if mf_main
            && may_halve
            && !motion
            && big
            && !enc.last_frame().key
            && encoded
                .as_ref()
                .is_some_and(|au| au.len() > MF_OVERSIZE * share)
            && let Some(full) = &picture
        {
            stats.oversize += 1;
            tracing::debug!(
                bytes = encoded.as_ref().map_or(0, Vec::len),
                "picture of a large change too large; half size from here"
            );
            motion = true;
            mf_settle = mf_settle_for(mf_settle, mf_back, now);
            if !std::mem::replace(&mut told_motion, true) {
                tracing::info!(
                    "large changes too large for this encoder: half size on openh264 until still"
                );
            }
            enc.set_motion_input(true);
            full.half_into(&mut small);
            enc.set_scaled_input(true);
            stats.half += 1;
            let half = small.as_ref().expect("allocated by half_into");
            size = half.dimensions();
            encoded = enc.encode_yuv(half, None)?;
        }
        let took = t0.elapsed();
        stats.encode += took;
        if urgent {
            stats.urgent += 1;
        }
        if let Some(au) = encoded {
            let info = enc.last_frame();
            let large = enc.settings().bitrate_bps as usize / 8 / cfg.max_fps.max(1) as usize / 2;
            if let Some(c) = converge.as_mut().filter(|_| topoff && au.len() >= large) {
                c.1 = now; // still sharpening
            }
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
            if info.key {
                // Why: asked for (and owed until now), else what changed.
                let cause = owed_key.take().unwrap_or(if !sent_any {
                    KeyCause::Start
                } else if last_size != Some(size) {
                    KeyCause::Size
                } else if repair {
                    KeyCause::Loss // nothing to repair from
                } else if retargeted {
                    KeyCause::Retarget
                } else {
                    KeyCause::Encoder
                });
                stats.keys += 1;
                stats.key_causes.add(cause);
            }
            if info.key || au.len() > 8 * large {
                tracing::debug!(
                    bytes = au.len(),
                    key = info.key,
                    w = size.0,
                    h = size.1,
                    engine = enc.engine_name(),
                    changed = p.as_ref().map_or(0.0, |p| p.changed),
                    "large picture"
                );
            }
            if last_size.is_some_and(|s| s != size) {
                stats.size_switches += 1;
            }
            last_size = Some(size);
            stats.size = Some(size);
            if info.key || info.recovers_from.is_some() {
                resync.sent(
                    info.id,
                    Instant::now(),
                    au.len(),
                    ctx.target.get(),
                    info.key,
                );
            }
            stats.repairs += u32::from(info.recovers_from.is_some());
            let host_us = p
                .as_ref()
                .map_or(t0, |p| p.captured_at)
                .elapsed()
                .as_micros()
                .min(u128::from(u32::MAX)) as u32;
            let au = frames::tag(
                cfg.codec,
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
                    w = size.0,
                    h = size.1,
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
            let sent_at = Instant::now();
            if let (Some(l), Some(p)) = (last_sent, &p) {
                // How long the viewer waited for this change: since the frame before, or
                // since it happened if the screen was idle until then.
                stats.max_gap = stats.max_gap.max(sent_at - l.max(p.captured_at));
            }
            last_sent = Some(sent_at);
            let bps = f64::from(enc.settings().bitrate_bps.max(1));
            link_free =
                link_free.max(sent_at) + Duration::from_secs_f64(au.len() as f64 * 8.0 / bps);
            stats.bytes += au.len();
            stats.frames += 1;
            if let Some(p) = &p {
                let latency = p.captured_at.elapsed();
                stats.latency += latency;
                stats.max_latency = stats.max_latency.max(latency);
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
        stats.maybe_log((w, h), &ctx.target, &ctx.sending, enc.engine_name());
    }
    Ok(produced)
}

/// Is a still picture being re-encoded until the encoder stops sharpening it (since
/// `.0`, the last large picture at `.1`)?
fn converging(converge: Option<(Instant, Instant)>, now: Instant) -> bool {
    converge
        .is_some_and(|(since, large)| now - large < CONVERGE_QUIET && now - since < CONVERGE_MAX)
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
    /// Frames sent at the full rate (interaction, a big change, something playing).
    urgent: u32,
    /// Frames sent at half size (motion).
    half: u32,
    /// Pictures of large changes Microsoft's encoder made too large, not sent.
    oversize: u32,
    /// Frames sent at the viewer's (smaller) size.
    scaled: u32,
    /// Still pictures re-sent to sharpen them (`TOPOFF_FRAMES`).
    topoffs: u32,
    captures: u32,
    /// Keyframes (and why), loss repairs from long-term references, losses the viewer
    /// reported, its losses and keyframe requests an earlier keyframe or repair had
    /// answered already, live bitrate changes.
    keys: u32,
    key_causes: KeyCauses,
    repairs: u32,
    losses: u32,
    answered: u32,
    retargets: u32,
    /// Refinement passes over a still screen, and their bytes.
    refines: u32,
    refine_bytes: usize,
    bytes: usize,
    capture: Duration,
    encode: Duration,
    latency: Duration,
    /// The longest a change waited to go out (capture to send), and the longest the
    /// viewer waited for one (since the frame before, or since the change if idle).
    max_latency: Duration,
    max_gap: Duration,
    /// Time changes waited for the link to drain (an encoder above its bitrate).
    link_held: Duration,
    /// How long after their slot frames of a steady stream started (sleep overshoot,
    /// waiting for the capturer), and how many such frames.
    late: Duration,
    late_frames: u32,
    /// The size of the last picture encoded, and how often it changed.
    size: Option<(usize, usize)>,
    size_switches: u32,
    changed: f64,
    /// Share of the screen sent as copy instructions instead of pixels (summed per frame).
    moved: f64,
    since: Option<Instant>,
    /// Process and stream-thread CPU seconds at `since`.
    cpu_at_start: (f64, f64),
}

impl Stats {
    fn maybe_log(
        &mut self,
        (w, h): (usize, usize),
        target: &TargetBitrate,
        sending: &SendClock,
        engine: &str,
    ) {
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
        let ms = |d: Duration| format!("{:.0}", d.as_secs_f64() * 1000.0);
        let (queue_ms, write_ms) = sending.take();
        tracing::info!(
            // The screen, and what was encoded (its last size; how often it changed).
            size = format!("{w}x{h}"),
            encoded = self
                .size
                .map_or("-".into(), |(ew, eh)| format!("{ew}x{eh}")),
            size_switches = self.size_switches,
            engine,
            fps = format!("{:.1}", f64::from(self.frames) / secs),
            urgent_fps = format!("{:.1}", f64::from(self.urgent) / secs),
            half_size_fps = format!("{:.1}", f64::from(self.half) / secs),
            oversize = self.oversize,
            view_size_fps = format!("{:.1}", f64::from(self.scaled) / secs),
            topoff_fps = format!("{:.1}", f64::from(self.topoffs) / secs),
            moved_pct = format!("{:.1}", self.moved * 100.0 / n),
            kbps = format!("{:.0}", self.bytes as f64 * 8.0 / secs / 1000.0),
            changed_pct = format!("{:.1}", self.changed * 100.0 / n),
            capture_ms = per(self.capture, f64::from(self.captures.max(1))),
            encode_ms = per(self.encode, n),
            capture_to_send_ms = per(self.latency, n),
            max_latency_ms = ms(self.max_latency),
            max_gap_ms = ms(self.max_gap),
            link_held_ms = ms(self.link_held),
            late_ms = per(self.late, f64::from(self.late_frames.max(1))),
            // Stages after the encoder: waiting for the session task, then packetizing
            // into RTP and queueing at the pacer.
            queue_ms = format!("{queue_ms:.1}"),
            write_ms = format!("{write_ms:.1}"),
            bwe_kbps = format!("{:.0}", target.estimate() / 1000.0),
            encoder_kbps = format!("{:.0}", target.get() / 1000.0),
            keyframes = self.keys,
            key_causes = %self.key_causes,
            repairs = self.repairs,
            losses = self.losses,
            answered = self.answered,
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

#[cfg(all(test, windows))]
#[path = "stream_live.rs"]
mod live;

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
                codec: rconn_media::caps::VideoCodec::H264,
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
                tags.push(frames::read(rconn_media::caps::VideoCodec::H264, &au).expect("tagged"));
                // Before the third repeat of the first keyframe (2.5 s).
                if first.elapsed() > Duration::from_millis(2300) {
                    ctx.stop.store(true, Ordering::Relaxed);
                }
            }
            drop(rx);
            worker.join().unwrap().unwrap();
        });
        // 5 pictures for the changes, then the top-offs, then nothing until the next
        // repeat at 2.5 s. The first keyframe's repeats at 300 ms and 1 s may stand in for
        // top-offs or, on a busy machine, come on top of them; openh264 may skip one after
        // a keyframe.
        let after = tags.len().saturating_sub(5);
        assert!(
            (3..=TOPOFF_FRAMES as usize + 2).contains(&after),
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

    /// The CPU encoder's width limit: a wider target (or the full screen) is narrowed to
    /// it, same shape; narrower ones and no limit are left alone.
    #[test]
    fn fit_width_narrows_only_past_the_limit() {
        let screen = (2992, 1878);
        assert_eq!(fit_width(screen, None, None), None);
        assert_eq!(fit_width(screen, None, Some(2688)), Some((2688, 1686)));
        assert_eq!(
            fit_width(screen, Some((1920, 1206)), Some(2688)),
            Some((1920, 1206))
        );
        assert_eq!(fit_width((1920, 1080), None, Some(2688)), None);
    }

    /// Frames start once per budget from the start of the one before, whatever the
    /// encode took (under the budget): 30 fps, where stamping after the encode gave 21.
    /// A frame later than a whole slot starts a new grid (no burst to catch up).
    #[test]
    fn the_frame_clock_counts_from_frame_starts() {
        let budget = Duration::from_secs(1) / 30;
        let t0 = Instant::now();
        let mut clock = FrameClock::new(budget, t0);
        let mut now = t0;
        let mut starts = Vec::new();
        for _ in 0..30 {
            now = now.max(clock.next()) + Duration::from_millis(2); // capture
            starts.push(now);
            clock.started(now);
            now += Duration::from_millis(15); // encode
        }
        let fps = 29.0 / (starts[29] - starts[0]).as_secs_f64();
        assert!((29.0..=31.0).contains(&fps), "{fps:.1} fps");
        // An encode of 45 ms: frames back to back, never two in one slot.
        let mut clock = FrameClock::new(budget, t0);
        let mut now = t0;
        let mut starts = Vec::new();
        for _ in 0..10 {
            now = now.max(clock.next());
            starts.push(now);
            clock.started(now);
            now += Duration::from_millis(45);
        }
        assert!(
            starts
                .windows(2)
                .all(|w| w[1] - w[0] >= Duration::from_millis(45))
        );
        // Idle for a second, then a change: the next frame waits a whole budget.
        let idle = now + Duration::from_secs(1);
        clock.started(idle);
        assert_eq!(clock.next(), idle + budget);
    }

    /// A 60 Hz screen, pictures taking 2-8 ms to take, 11 ms encodes, someone typing:
    /// counting the next frame's due time from the last frame's slot keeps 30 fps. From
    /// its start (a few ms late), a frame taken at its slot was "not due yet" and waited
    /// for the screen's next present: 24 fps (a Windows VM: 24-26, never above 25.8).
    #[test]
    fn intervals_count_from_slots_not_starts() {
        let budget = Duration::from_secs(1) / 30;
        let present = Duration::from_secs(1) / 60;
        let fps = |from_slot: bool| {
            let t0 = Instant::now();
            let mut clock = FrameClock::new(budget, t0);
            let mut last = t0 - Duration::from_secs(1);
            let (mut now, mut seed, mut frames) = (t0, 7u32, 0u32);
            while now < t0 + Duration::from_secs(3) {
                now = now.max(clock.next()); // the frame clock's sleep
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                now += Duration::from_millis(2 + u64::from(seed >> 29)); // take the picture
                if now < last + change_interval(true, 0.01, budget) {
                    // Not due: the capturer waits for the screen's next present.
                    let n = (now - t0).as_nanos() / present.as_nanos() + 1;
                    now = t0 + present * n as u32;
                    continue;
                }
                let slot = clock.started(now);
                last = if from_slot { slot } else { now };
                frames += 1;
                now += Duration::from_millis(11); // encode
            }
            f64::from(frames) / 3.0
        };
        let (slots, starts) = (fps(true), fps(false));
        assert!(slots >= 29.5, "{slots:.1} fps");
        assert!(starts < 27.0, "the old way: {starts:.1} fps");
    }

    /// Something playing (a video in a window) is noticed after a few changes in a row;
    /// a blinking caret (twice a second) and a tiny spinner are not.
    /// Microsoft's encoder at half size: back after the settle once the screen is still
    /// (or long after, while typing goes on), never into a busy link; a quick return to
    /// motion doubles the settle (up to a ceiling), a long calm resets it.
    #[test]
    fn half_size_spells_end_late_and_rarely() {
        let s = MF_MOTION_SETTLE;
        let ms = Duration::from_millis;
        assert!(
            !mf_switch_back(s, s - ms(1), ms(5000), true),
            "settle not over"
        );
        assert!(
            mf_switch_back(s, s, MF_BACK_STILL, true),
            "settled and still"
        );
        assert!(!mf_switch_back(s, s, ms(100), true), "typing on");
        assert!(
            mf_switch_back(s, s + MF_BACK_ANYWAY, ms(10), true),
            "typing on and on"
        );
        assert!(!mf_switch_back(s, s * 10, ms(5000), false), "link busy");
        let t0 = Instant::now();
        let now = t0 + ms(5000);
        assert_eq!(mf_settle_for(s, None, now), s);
        assert_eq!(mf_settle_for(s, Some(t0), now), s * 2);
        assert_eq!(
            mf_settle_for(MF_MOTION_SETTLE_MAX, Some(t0), now),
            MF_MOTION_SETTLE_MAX
        );
        assert_eq!(mf_settle_for(s * 4, Some(t0), t0 + MF_MOTION_RETURN), s);
    }

    #[test]
    fn sustained_changes_count_as_playing() {
        let t0 = Instant::now();
        let mut video = Animation::default();
        for k in 0..6 {
            video.note(t0 + Duration::from_millis(40 * k), 0.05);
        }
        let last = t0 + Duration::from_millis(200);
        assert!(video.playing(last));
        assert!(!video.playing(last + Duration::from_millis(300)), "stopped");
        let mut caret = Animation::default();
        for k in 0..6 {
            caret.note(t0 + Duration::from_millis(500 * k), 0.01);
        }
        assert!(!caret.playing(t0 + Duration::from_millis(2500)));
        let mut spinner = Animation::default();
        for k in 0..30 {
            spinner.note(t0 + Duration::from_millis(33 * k), 0.0005);
        }
        assert!(!spinner.playing(t0 + Duration::from_millis(990)));
    }

    /// A loss reported from before the last keyframe is one that keyframe answered (the
    /// viewer repeats its report until a picture comes); later, or after the grace, a
    /// report counts.
    fn soon_after(t: Instant) -> Instant {
        t + Duration::from_millis(300)
    }

    #[test]
    fn losses_from_before_a_keyframe_are_answered() {
        let t0 = Instant::now();
        let mut resync = Resync::default();
        assert!(!resync.answered(5, t0), "nothing sent yet");
        // A 100 KB keyframe, id 10, at 8 Mbps: 100 ms on the link.
        resync.sent(10, t0, 100_000, 8e6, true);
        assert!(resync.key_on_its_way(soon_after(t0)));
        let soon = t0 + Duration::from_millis(300);
        assert!(resync.answered(9, soon));
        assert!(!resync.answered(10, soon), "a loss after the keyframe");
        assert!(!resync.answered(12, soon));
        assert!(
            !resync.answered(9, t0 + Duration::from_secs(2)),
            "the keyframe was lost too"
        );
        // Ids wrap.
        resync.sent(2, t0, 1000, 8e6, false);
        assert!(resync.answered(u32::MAX - 1, soon));
    }

    #[test]
    fn key_causes_read_as_counts() {
        let mut k = KeyCauses::default();
        assert_eq!(k.to_string(), "-");
        k.add(KeyCause::Loss);
        k.add(KeyCause::Size);
        k.add(KeyCause::Loss);
        assert_eq!(k.to_string(), "loss:2 size:1");
    }

    /// The first keyframe's repeats only go out while the stream is quiet: none while
    /// the screen keeps changing past their times (300 ms, 1 s, 2.5 s).
    #[test]
    fn first_frame_repeats_never_interrupt_motion() {
        let Some(lib) = rconn_media::cisco::local_binary_path().filter(|p| p.exists()) else {
            return;
        };
        let keyframes = KeyframeRequests::default();
        let mut ctx = software_ctx(&lib, &keyframes);
        ctx.cfg.max_fps = 10;
        let (tx, mut rx) = mpsc::channel(4);
        // Changes at 10 fps for longer than the test runs.
        let make = || -> Result<Box<dyn Capturer>> {
            Ok(Box::new(Changes {
                pattern: TestPattern::new(640, 360, 10),
                changes: usize::MAX,
            }))
        };
        let mut keys = 0;
        std::thread::scope(|s| {
            let worker = s.spawn(|| {
                let tx = tx;
                run_generation(&make, &ctx, &tx)
            });
            // Past the last repeat's time (2.5 s).
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(3000));
                ctx.stop.store(true, Ordering::Relaxed);
            });
            while let Some((au, _)) = rx.blocking_recv() {
                let tag = frames::read(rconn_media::caps::VideoCodec::H264, &au).expect("tagged");
                keys += u32::from(tag.key);
            }
            drop(rx);
            worker.join().unwrap().unwrap();
        });
        assert_eq!(keys, 1, "only the first frame");
    }

    /// Still at first for this long (the first keyframe drains), in [`host_loop`].
    const SETTLE: Duration = Duration::from_millis(2500);

    /// A synthetic desktop ([`crate::encode_bench`]'s) still for [`SETTLE`], changing at
    /// 30 fps for `moving`, then still: what `run_generation` gets from a capturer that
    /// reports changed areas.
    struct Scripted {
        desk: crate::encode_bench::Desktop,
        scenario: crate::encode_bench::Scenario,
        started: Option<Instant>,
        moving: Duration,
        /// Changes per second while it moves.
        hz: u32,
        k: usize,
        sent_first: bool,
    }

    impl Capturer for Scripted {
        fn next_frame(
            &mut self,
            _: Duration,
        ) -> Result<Option<rconn_media::video::Frame>, rconn_media::capture::CaptureError> {
            unreachable!("next_into")
        }

        fn next_into(
            &mut self,
            timeout: Duration,
            picture: &mut Option<I420>,
            _max: (usize, usize),
        ) -> Result<Option<Captured>, rconn_media::capture::CaptureError> {
            use crate::encode_bench::Scenario;
            let (w, h) = self.desk.size();
            let t0 = *self.started.get_or_insert_with(Instant::now);
            if !self.sent_first {
                self.sent_first = true;
                let pic = picture.insert(I420::new(w, h));
                pic.fill_from_bgra(self.desk.screen(), w);
                return Ok(Some(Captured {
                    rows: (0, h),
                    changed: 1.0,
                    captured_at: Instant::now(),
                }));
            }
            // After a still start (the first keyframe drains), the screen's next change is
            // due at `hz` while it moves.
            let t0 = t0 + SETTLE;
            let due = t0 + Duration::from_secs(1) / self.hz * (self.k as u32 + 1);
            if t0.elapsed() >= self.moving || Instant::now() + timeout < due {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
            self.k += 1;
            if matches!(self.scenario, Scenario::Typing | Scenario::Scrolling) {
                note_input(); // keys, the wheel
            }
            let rects = self.desk.step(self.k);
            let pic = picture.as_mut().expect("allocated above");
            pic.fill_rects_from_bgra(self.desk.screen(), w * 4, &rects);
            let (top, bottom) = rects
                .iter()
                .fold((h, 0), |(a, b), r| (a.min(r.y), b.max(r.y + r.h)));
            Ok(Some(Captured {
                rows: (top & !1, bottom.next_multiple_of(2).min(h)),
                changed: rects.iter().map(|r| r.w * r.h).sum::<usize>() as f32 / (w * h) as f32,
                captured_at: Instant::now(),
            }))
        }
    }

    /// The whole host loop on each benchmark scenario (`RCONN_BENCH_SIZE`, viewer
    /// `RCONN_BENCH_VIEW`): still for 2.5 s, 3 s of changes at 30 fps
    /// (`RCONN_BENCH_SECS`), then 4 s still, frames acknowledged as received, with the encoder
    /// the host picks. Frames per second and the longest gap while it moves, bytes,
    /// keyframes, encoded size, and what the still screen got afterwards.
    /// `cargo test --release -p rconn-host-agent host_loop -- --ignored --nocapture`
    #[test]
    #[ignore = "timing, run by hand"]
    fn host_loop() {
        use crate::encode_bench::Scenario;
        let Some(lib) = rconn_media::cisco::local_binary_path().filter(|p| p.exists()) else {
            return;
        };
        let knob = |name: &str, default: (usize, usize)| {
            std::env::var(name)
                .ok()
                .and_then(|v| {
                    let (w, h) = v.split_once('x')?;
                    Some((w.parse().ok()?, h.parse().ok()?))
                })
                .unwrap_or(default)
        };
        // The host's own stats lines too (`RUST_LOG=info`).
        if std::env::var("RUST_LOG").is_ok() {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
                .try_init();
        }
        let size: (usize, usize) = knob("RCONN_BENCH_SIZE", (1920, 1080));
        let view: (usize, usize) = knob("RCONN_BENCH_VIEW", (1280, 720));
        let moving = Duration::from_secs(
            std::env::var("RCONN_BENCH_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3),
        );
        // The screen's own rate (a 60 Hz display scrolling: `RCONN_BENCH_HZ=60`).
        let hz: u32 = std::env::var("RCONN_BENCH_HZ")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);
        println!(
            "screen {}x{}, viewer {}x{}: {} s of changes at {hz} Hz, then 4 s still; 8000 kbps",
            size.0,
            size.1,
            view.0,
            view.1,
            moving.as_secs()
        );
        println!(
            "{:<10} {:>9} {:>5} {:>6} {:>7} {:>4} {:>9} {:>8} {:>8}",
            "scenario",
            "encoded",
            "fps",
            "gap ms",
            "KB/pic",
            "keys",
            "still pics",
            "still KB",
            "last ms"
        );
        for scenario in Scenario::ALL {
            let keyframes = KeyframeRequests::default();
            keyframes.set_view_size(view.0 as u32, view.1 as u32);
            let mut ctx = software_ctx(&lib, &keyframes);
            ctx.cfg.max_fps = 30;
            ctx.cfg.software = false;
            let (tx, mut rx) = mpsc::channel(2);
            let make = || -> Result<Box<dyn Capturer>> {
                Ok(Box::new(Scripted {
                    desk: crate::encode_bench::Desktop::new(
                        scenario,
                        size,
                        (hz as usize + 10) * moving.as_secs() as usize,
                    ),
                    scenario,
                    started: None,
                    moving,
                    hz,
                    k: 0,
                    sent_first: false,
                }))
            };
            let mut out: Vec<(Instant, usize, bool)> = Vec::new();
            let mut encoded = None;
            std::thread::scope(|s| {
                let worker = s.spawn(|| {
                    let tx = tx;
                    run_generation(&make, &ctx, &tx)
                });
                s.spawn(|| {
                    std::thread::sleep(SETTLE + moving + Duration::from_secs(4));
                    ctx.stop.store(true, Ordering::Relaxed);
                });
                while let Some((au, at)) = rx.blocking_recv() {
                    let (au, tag) = frames::take(rconn_media::caps::VideoCodec::H264, &au);
                    let key = tag.is_some_and(|t| t.key);
                    if let Some(t) = tag {
                        // The viewer decoded it.
                        keyframes.frame_feedback(Feedback {
                            decoded: t.id,
                            lost: false,
                        });
                    }
                    if let Some(sps) = rconn_media::video::sequence_header_of(
                        rconn_media::caps::VideoCodec::H264,
                        &au,
                    )
                    .and_then(|h| {
                        rconn_media::video::sequence_info(rconn_media::caps::VideoCodec::H264, h)
                    }) {
                        encoded = Some((sps.width, sps.height));
                    }
                    out.push((at, au.len(), key));
                }
                worker.join().unwrap().unwrap();
            });
            let t0 = out[0].0 + SETTLE;
            let split = t0 + moving;
            let (moving_frames, still): (Vec<&(Instant, usize, bool)>, Vec<_>) =
                out.iter().filter(|f| f.0 >= t0).partition(|f| f.0 < split);
            let gap = moving_frames
                .windows(2)
                .map(|w| w[1].0 - w[0].0)
                .max()
                .unwrap_or_default();
            let span = moving_frames
                .last()
                .zip(moving_frames.first())
                .map_or(Duration::ZERO, |(l, f)| l.0 - f.0);
            let kb = moving_frames.iter().map(|f| f.1).sum::<usize>() as f64
                / 1024.0
                / moving_frames.len().max(1) as f64;
            let (ew, eh) = encoded.unwrap_or_default();
            println!(
                "{:<10} {:>9} {:>5.1} {:>6.0} {:>7.1} {:>4} {:>9} {:>8.0} {:>8.0}",
                format!("{scenario:?}"),
                format!("{ew}x{eh}"),
                (moving_frames.len().max(1) - 1) as f64 / span.as_secs_f64().max(1e-9),
                gap.as_secs_f64() * 1e3,
                kb,
                out.iter().filter(|f| f.2).count(),
                still.len(),
                still.iter().map(|f| f.1).sum::<usize>() as f64 / 1024.0,
                still
                    .last()
                    .map_or(0.0, |f| (f.0 - split).as_secs_f64() * 1e3)
            );
        }
    }

    /// How long `std::thread::sleep` really takes here (the frame clock sleeps to its next
    /// slot): `cargo test -p rconn-host-agent sleep_granularity -- --ignored --nocapture`
    #[test]
    #[ignore = "timing, run by hand"]
    fn sleep_granularity() {
        for ms in [1u64, 5, 10, 20] {
            let t0 = Instant::now();
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(ms));
            }
            println!(
                "sleep({ms} ms): {:.2} ms",
                t0.elapsed().as_secs_f64() * 1e3 / 20.0
            );
        }
    }
}
