//! Build to lossless: once the screen stops changing, the picture the viewer has is
//! sharpened by refinement passes (P-frames of the same picture at a finer quantizer;
//! [`crate::video::VideoEncoder::refine_next`]) until it reaches [`FLOOR_QP`].
//!
//! Frames sent while things change are coded as coarsely as the rate controller needs
//! (openh264 up to QP 32; VideoToolbox up to 51 at low rates), and so are areas uncovered
//! by a scroll or moved with copy instructions: without refinement a still screen keeps
//! that blur. Like Chrome Remote Desktop's "top-off" (at most 10 frames per cycle) and
//! Citrix's build to lossless, passes run only while nothing changes, stop at the first
//! change, and are paced so the stream stays within the bandwidth estimate:
//!
//! * a pass waits until the screen has been still for [`SETTLE`] and the bytes already
//!   sent have drained at the target rate (a leaky bucket modelling the pacer queue);
//! * each pass lowers the quantizer by a step sized so that a pass costs about
//!   [`PASS_SECS`] of the target rate (measured from the pass before);
//! * passes are P-frames that predict from the picture the viewer has: areas already fine
//!   cost almost nothing (skipped macroblocks), so a pass after a small change costs
//!   about that change.
//!
//! `RCONN_REFINE=0` turns it off.

use std::time::{Duration, Instant};

/// The quantizer refinement stops at (H.264 QP 18: usually called visually lossless).
pub const FLOOR_QP: u8 = 18;
/// Still for this long before the first pass (typing pauses are shorter than this).
pub const SETTLE: Duration = Duration::from_millis(400);
/// A pass should take about this long to send at the target rate.
pub const PASS_SECS: f64 = 0.25;
/// Passes per still period, at most (Chrome Remote Desktop tops off with up to 10).
pub const MAX_PASSES: u32 = 10;
/// Quantizer steps per pass: the first, and the bounds of the adapted step.
const FIRST_STEP: u8 = 4;
const MIN_STEP: u8 = 3;
const MAX_STEP: u8 = 8;
/// Assumed for frames whose quantizer could not be read.
const UNKNOWN_QP: u8 = 32;
/// After a dropped pass (the encoder's rate control refused it), try again this late.
const RETRY: Duration = Duration::from_secs(1);

/// Refinement is on unless `RCONN_REFINE=0`.
pub fn enabled() -> bool {
    std::env::var("RCONN_REFINE").map_or(true, |v| v != "0" && v != "off")
}

/// Decides when to send refinement passes, and at which quantizer.
#[derive(Debug)]
pub struct Refiner {
    /// The coarsest quantizer in the viewer's picture (as far as we know), `None` before
    /// the first frame.
    level: Option<u8>,
    /// Quantizer steps for the next pass.
    step: u8,
    /// Passes sent (or tried) since the screen last changed.
    passes: u32,
    last_change: Instant,
    /// When the bytes sent so far have left at the target rate.
    link_free_at: Instant,
    retry_at: Option<Instant>,
}

impl Refiner {
    pub fn new(now: Instant) -> Self {
        Self {
            level: None,
            step: FIRST_STEP,
            passes: 0,
            last_change: now,
            link_free_at: now,
            retry_at: None,
        }
    }

    /// The screen changed (a capture with changes came in): passes wait for stillness,
    /// and a new still period gets a new set of passes.
    pub fn changed(&mut self, now: Instant) {
        self.last_change = now;
        self.passes = 0;
        self.retry_at = None;
        self.step = FIRST_STEP;
    }

    /// A frame went out: `bytes` long, coded at `qp` (if known), a refinement pass or a
    /// keyframe or neither; the encoder's target is `target_bps`.
    pub fn sent(
        &mut self,
        now: Instant,
        bytes: usize,
        qp: Option<u8>,
        refine: bool,
        key: bool,
        target_bps: u32,
    ) {
        let secs = bytes as f64 * 8.0 / f64::from(target_bps.max(1));
        self.link_free_at = self.link_free_at.max(now) + Duration::from_secs_f64(secs);
        let qp = qp.unwrap_or(UNKNOWN_QP);
        if refine {
            // The whole picture is now coded at least this finely.
            self.level = Some(self.level.map_or(qp, |l| l.min(qp)));
            let budget = PASS_SECS * f64::from(target_bps) / 8.0;
            let scaled = f64::from(self.step) * budget / bytes.max(1) as f64;
            self.step = scaled.clamp(f64::from(MIN_STEP), f64::from(MAX_STEP)) as u8;
            self.retry_at = None;
        } else if key {
            self.level = Some(qp); // a whole new picture
        } else {
            // A change, coded at `qp`: the picture is no better than its coarsest part.
            self.level = Some(self.level.map_or(qp, |l| l.max(qp)));
        }
    }

    /// The pass asked for gave no frame (the encoder's rate control dropped it).
    pub fn dropped(&mut self, now: Instant) {
        self.retry_at = Some(now + RETRY);
    }

    /// Is the viewer's picture coarser than the floor?
    pub fn wants_passes(&self) -> bool {
        self.level.is_some_and(|l| l > FLOOR_QP) && self.passes < MAX_PASSES
    }

    /// When the next pass may go, if one is wanted.
    pub fn due(&self) -> Option<Instant> {
        self.wants_passes().then(|| {
            (self.last_change + SETTLE)
                .max(self.link_free_at)
                .max(self.retry_at.unwrap_or(self.last_change))
        })
    }

    /// The quantizer of a pass to send now, if one is due (counted as sent).
    pub fn next(&mut self, now: Instant) -> Option<u8> {
        if self.due().is_none_or(|t| now < t) {
            return None;
        }
        self.passes += 1;
        let level = self.level.expect("wants_passes");
        Some(level.saturating_sub(self.step).max(FLOOR_QP))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MBPS: u32 = 1_000_000;

    #[test]
    fn passes_wait_for_stillness_and_stop_at_the_floor() {
        let t0 = Instant::now();
        let mut r = Refiner::new(t0);
        assert_eq!(r.due(), None, "nothing sent yet");
        r.changed(t0);
        r.sent(t0, 4_000, Some(32), false, false, 4 * MBPS);
        assert_eq!(
            r.next(t0 + Duration::from_millis(100)),
            None,
            "not still yet"
        );
        let mut t = t0 + SETTLE;
        let mut qps = Vec::new();
        while let Some(qp) = r.next(t) {
            qps.push(qp);
            // Cheap passes: the step grows.
            r.sent(t, 20_000, Some(qp), true, false, 4 * MBPS);
            t = r.due().unwrap_or(t);
        }
        assert_eq!(qps.first(), Some(&28), "first step 4 below the coarsest");
        assert_eq!(qps.last(), Some(&FLOOR_QP));
        assert!(qps.windows(2).all(|w| w[1] < w[0]), "{qps:?}");
        assert!(qps.len() <= 4, "{qps:?}");
        assert!(!r.wants_passes());
    }

    #[test]
    fn passes_are_paced_by_the_target_rate() {
        let t0 = Instant::now();
        let mut r = Refiner::new(t0);
        r.sent(t0, 1_000, Some(32), false, true, MBPS);
        let t = t0 + SETTLE;
        let qp = r.next(t).unwrap();
        // 250 kB at 1 Mbit/s: two seconds on the wire.
        r.sent(t, 250_000, Some(qp), true, false, MBPS);
        let due = r.due().unwrap();
        assert!(due >= t + Duration::from_millis(1990), "{:?}", due - t);
        assert_eq!(r.next(t + Duration::from_secs(1)), None);
        // An expensive pass: smaller steps from here.
        assert_eq!(r.step, MIN_STEP);
        assert_eq!(r.next(due), Some(qp - MIN_STEP));
    }

    #[test]
    fn a_change_stops_the_passes_and_a_coarse_frame_restarts_them() {
        let t0 = Instant::now();
        let mut r = Refiner::new(t0);
        r.sent(t0, 1_000, Some(30), false, true, 8 * MBPS);
        let t1 = t0 + SETTLE;
        let qp = r.next(t1).unwrap();
        r.sent(t1, 10_000, Some(qp), true, false, 8 * MBPS);
        // Typing again.
        let t2 = t1 + Duration::from_millis(50);
        r.changed(t2);
        assert_eq!(r.next(t2 + Duration::from_millis(100)), None);
        r.sent(t2, 2_000, Some(32), false, false, 8 * MBPS);
        assert_eq!(r.level, Some(32), "the coarsest part counts");
        assert!(r.due().unwrap() >= t2 + SETTLE);
        // Fine content needs nothing.
        let mut fine = Refiner::new(t0);
        fine.sent(t0, 1_000, Some(16), false, true, MBPS);
        assert_eq!(fine.due(), None);
    }

    #[test]
    fn dropped_passes_are_retried_a_bounded_number_of_times() {
        let t0 = Instant::now();
        let mut r = Refiner::new(t0);
        r.sent(t0, 1_000, Some(51), false, true, MBPS);
        let mut t = t0 + SETTLE;
        let mut tries = 0;
        while let Some(_qp) = r.next(t) {
            tries += 1;
            r.dropped(t);
            t = r.due().unwrap_or(t);
        }
        assert_eq!(tries, MAX_PASSES);
        assert!(r.wants_passes() || r.passes == MAX_PASSES);
    }
}

#[cfg(test)]
pub(crate) mod measure;
