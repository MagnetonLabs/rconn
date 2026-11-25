//! One session's screen stream: capture thread -> latest-frame slot -> encoder task ->
//! video track, steered by congestion control and keyframe requests from the viewer.

use anyhow::Result;
use rconn_media::capture::Capturer;
use rconn_media::video::{EncoderSettings, Frame, VideoEncoder, should_retarget};
use rconn_peer::video::{TargetBitrate, VideoSender};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, watch};

pub struct StreamConfig {
    pub openh264: PathBuf,
    pub max_fps: u32,
    pub max_bitrate_bps: u32,
    /// Larger desktops are halved until they fit (HiDPI VMs report e.g. 2992×1878).
    pub max_size: (usize, usize),
}

/// Asks the encoder for an IDR frame (viewer joined, decoder lost sync).
#[derive(Clone, Default)]
pub struct KeyframeRequests(Arc<Notify>);

impl KeyframeRequests {
    pub fn request(&self) {
        self.0.notify_one();
    }
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
    let (tx, mut rx) = watch::channel::<Option<Arc<Frame>>>(None);
    let frame_budget = Duration::from_secs(1) / cfg.max_fps.max(1);

    // Capture supervisor. Each capturer "generation" lives on a fresh OS thread: after a
    // desktop switch (lock screen, UAC, login) the old D3D device and the thread's desktop
    // binding are stale, and a thread that has created D3D objects can't reliably move to
    // another desktop. So on any capture error the generation ends and a new one starts.
    let cap_stop = stop.clone();
    let max_size = cfg.max_size;
    let tx = Arc::new(tx);
    let capture = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut backoff = Duration::from_millis(250);
        let mut generation = 0u32;
        while !cap_stop.load(Ordering::Relaxed) {
            generation += 1;
            let (factory, stop, gen_tx) = (make_capturer.clone(), cap_stop.clone(), tx.clone());
            let worker = std::thread::Builder::new()
                .name(format!("rconn-capture-{generation}"))
                .spawn(move || {
                    capture_generation(&*factory, &stop, &gen_tx, frame_budget, max_size)
                })?;
            match worker.join() {
                Ok(Ok(true)) => backoff = Duration::from_millis(250), // produced frames before failing
                Ok(Ok(false)) => {}
                Ok(Err(e)) => {
                    tracing::warn!(error = %format!("{e:#}"), generation, "capture restarting")
                }
                Err(_) => tracing::error!(generation, "capture thread panicked; restarting"),
            }
            if cap_stop.load(Ordering::Relaxed) || tx.receiver_count() == 0 {
                break;
            }
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_secs(2));
        }
        Ok(())
    });

    let mut enc = VideoEncoder::new(
        &cfg.openh264,
        EncoderSettings {
            bitrate_bps: (target.get() as u32).min(cfg.max_bitrate_bps),
            max_fps: cfg.max_fps as f32,
            threads: 0,
            slice_bytes: None,
        },
    )?;
    let mut last: Option<Arc<Frame>> = None;
    // The most recent frame sent. An idle desktop produces no new frames, so a keyframe
    // request re-encodes this one; otherwise a viewer joining a static screen sees nothing.
    let mut sent: Option<Arc<Frame>> = None;
    let mut stats = Stats::default();
    let mut retarget_at = Instant::now() + Duration::from_secs(1);
    let mut next_send = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        // Wake for: a new frame, a keyframe request, or the send slot of a frame we hold.
        let slot = tokio::time::Instant::from_std(next_send);
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() { break; } // capture thread ended
                last = rx.borrow_and_update().clone();
            }
            () = keyframes.0.notified() => {
                enc.request_keyframe();
                if last.is_none() {
                    last = sent.clone();
                }
                tracing::debug!("keyframe requested");
            }
            () = tokio::time::sleep_until(slot), if last.is_some() => {}
            () = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
        // Pace to max_fps: hold the newest frame until its slot; older ones are dropped.
        let now = Instant::now();
        if now < next_send {
            continue;
        }
        let Some(frame) = last.take() else { continue };
        next_send = (next_send + frame_budget).max(now);

        if now >= retarget_at {
            retarget_at = now + Duration::from_secs(1);
            let want = (target.get() as u32).clamp(300_000, cfg.max_bitrate_bps);
            if should_retarget(enc.settings().bitrate_bps, want) {
                tracing::info!(
                    from_kbps = enc.settings().bitrate_bps / 1000,
                    to_kbps = want / 1000,
                    "retarget encoder"
                );
                enc.set_bitrate(want);
            }
        }

        sent = Some(frame.clone());
        let t0 = Instant::now();
        let encoded = enc.encode(&frame)?;
        stats.encode += t0.elapsed();
        if let Some(au) = encoded {
            stats.bytes += au.len();
            stats.frames += 1;
            stats.latency += frame.captured_at.elapsed();
            if let Err(e) = video.write(au, frame_budget).await {
                tracing::info!(error = %e, "video track closed");
                break;
            }
        }
        stats.maybe_log(&frame, target.get());
    }
    stop.store(true, Ordering::Relaxed);
    let _ = capture.await;
    Ok(())
}

/// Builds a capturer; called again for every capture generation.
pub type CapturerFactory = Arc<dyn Fn() -> Result<Box<dyn Capturer>> + Send + Sync>;

/// One capturer's lifetime. Returns `Ok(true)` if it produced frames before ending
/// (so the supervisor resets its backoff), or the error that ended it.
fn capture_generation(
    make: &(dyn Fn() -> Result<Box<dyn Capturer>> + Send + Sync),
    stop: &AtomicBool,
    tx: &watch::Sender<Option<Arc<Frame>>>,
    frame_budget: Duration,
    max_size: (usize, usize),
) -> Result<bool> {
    let mut capturer = make()?;
    let mut produced = false;
    let mut last_frame_at = Instant::now();
    let mut warned = false;
    while !stop.load(Ordering::Relaxed) {
        match capturer.next_frame(frame_budget) {
            Ok(Some(f)) => {
                produced = true;
                last_frame_at = Instant::now();
                warned = false;
                if tx
                    .send(Some(Arc::new(f.fit_within(max_size.0, max_size.1))))
                    .is_err()
                {
                    break;
                }
            }
            Ok(None) => {
                // An idle desktop is normal; never getting a first frame is not.
                if !warned && last_frame_at.elapsed() > Duration::from_secs(5) {
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
    }
    Ok(produced)
}

#[derive(Default)]
struct Stats {
    frames: u32,
    bytes: usize,
    encode: Duration,
    latency: Duration,
    since: Option<Instant>,
}

impl Stats {
    fn maybe_log(&mut self, frame: &Frame, target_bps: f64) {
        let since = *self.since.get_or_insert_with(Instant::now);
        let secs = since.elapsed().as_secs_f64();
        if secs < 5.0 {
            return;
        }
        let n = self.frames.max(1);
        tracing::info!(
            size = format!("{}x{}", frame.width, frame.height),
            fps = format!("{:.1}", f64::from(self.frames) / secs),
            kbps = format!("{:.0}", self.bytes as f64 * 8.0 / secs / 1000.0),
            encode_ms = format!("{:.1}", self.encode.as_secs_f64() * 1000.0 / f64::from(n)),
            capture_to_send_ms =
                format!("{:.1}", self.latency.as_secs_f64() * 1000.0 / f64::from(n)),
            gcc_kbps = format!("{:.0}", target_bps / 1000.0),
            "stream"
        );
        *self = Self::default();
    }
}
