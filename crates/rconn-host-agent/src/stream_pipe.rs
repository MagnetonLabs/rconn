//! Capture a frame ahead of the encoder ([`super::run_generation`], opt-in: see
//! `capture_ahead`): a capture thread takes the screen's next picture while the stream
//! thread encodes the last one, so a frame costs the longer of the two instead of both.
//!
//! One picture waits at most, and never grows old: a capture made while the last one
//! still waits merges into it (its changed rows and areas added, the newest pixels), and
//! one waiting a whole frame slot is brought up to date by the next capture. Captures
//! start a capture's time before the encoder's next slot, so the picture it takes is
//! fresh, never a slot old.
//!
//! Pictures cross threads as whole buffers that trade places: the capture thread keeps
//! the capturer's own picture (which it updates in place) and copies the rows each
//! capture changed into the waiting one; the stream thread swaps that for the one it last
//! encoded, whose rows the captures since then changed are copied in before it waits
//! again. The capturer is made, used and dropped on the capture thread (DXGI: bound to
//! the input desktop, its frames released before it returns from a capture).

use super::Pending;
use rconn_media::capture::{CaptureError, Captured, Capturer};
use rconn_media::copyrect::Move;
use rconn_media::yuv::I420;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The longest one wait for the screen lasts on the capture thread (it stops promptly).
const CAPTURE_WAIT: Duration = Duration::from_millis(50);
/// Captures start this much earlier still than their time before the slot (each ms
/// earlier is a ms older picture; re-encodes of a still picture wait for a capture
/// under way, [`Pipe::capture_due`]).
const CAPTURE_LEAD: Duration = Duration::from_millis(1);
/// Extra time a capture under way may take beyond its usual (a quarter more, at least
/// this) before re-encodes of the still picture stop waiting for it.
const CAPTURE_SLACK: Duration = Duration::from_millis(3);

/// Why captures ended.
pub(super) enum Ended {
    /// The capturer could not be made.
    Start(anyhow::Error),
    /// It failed after being made (a desktop switch, typically).
    Capture(CaptureError),
}

/// The picture waiting between the capture thread and the stream thread.
pub(super) struct Pipe {
    state: Mutex<State>,
    wake: Condvar,
    budget: Duration,
}

struct State {
    /// The newest picture handed over, or one the stream thread handed back.
    picture: Option<I420>,
    /// Rows in which `picture` lags the capturer's picture (it was handed back).
    behind: Option<(usize, usize)>,
    /// Rows in which the stream thread's picture lags (captured since it took it).
    encoder_behind: Option<(usize, usize)>,
    /// Changes in `picture` the stream thread has not heard of.
    fresh: Option<Pending>,
    /// Changes it heard of ([`Pipe::poll`]) are in `picture`, not yet taken.
    told: bool,
    /// When `picture` was last brought up to date.
    published: Instant,
    moves: Vec<Move>,
    /// Time spent capturing, and captures that brought a change.
    capture_time: Duration,
    captures: u32,
    ended: Option<Ended>,
    /// The stream thread's next frame slot.
    next_slot: Instant,
    /// When the capture under way started, and how long one that brings a change takes.
    inflight: Option<Instant>,
    takes: Duration,
    stop: bool,
}

fn union(a: Option<(usize, usize)>, b: (usize, usize)) -> (usize, usize) {
    a.map_or(b, |a| (a.0.min(b.0), a.1.max(b.1)))
}

fn to_captured(p: Pending) -> Captured {
    Captured {
        rows: p.rows,
        changed: p.changed,
        captured_at: p.captured_at,
    }
}

impl State {
    /// Something captured waits (told of or not).
    fn waiting(&self) -> bool {
        self.told || self.fresh.is_some()
    }

    /// The waiting picture has another size than the stream thread's `picture`.
    fn resized(&self, picture: &Option<I420>) -> bool {
        self.waiting()
            && self
                .picture
                .as_ref()
                .is_some_and(|p| Some(p.dimensions()) != picture.as_ref().map(I420::dimensions))
    }

    /// The waiting picture to the stream thread, its last one back; the changes in it
    /// the stream thread had not heard of.
    fn swap(&mut self, picture: &mut Option<I420>) -> Option<Captured> {
        std::mem::swap(&mut self.picture, picture);
        self.behind = self.encoder_behind.take();
        self.told = false;
        self.fresh.take().map(to_captured)
    }
}

impl Pipe {
    /// For a stream of frames `budget` apart.
    pub(super) fn new(budget: Duration) -> Self {
        let now = Instant::now();
        Self {
            state: Mutex::new(State {
                picture: None,
                behind: None,
                encoder_behind: None,
                fresh: None,
                told: false,
                published: now,
                moves: Vec::new(),
                capture_time: Duration::ZERO,
                captures: 0,
                ended: None,
                next_slot: now,
                inflight: None,
                takes: Duration::ZERO,
                stop: false,
            }),
            wake: Condvar::new(),
            budget,
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait<'a>(&self, s: MutexGuard<'a, State>, d: Duration) -> MutexGuard<'a, State> {
        self.wake
            .wait_timeout(s, d)
            .unwrap_or_else(|e| e.into_inner())
            .0
    }

    /// The capture thread: makes the capturer, captures until [`Self::stop`] or a
    /// capture error (see [`Ended`]).
    pub(super) fn capture(
        &self,
        make: &(dyn Fn() -> anyhow::Result<Box<dyn Capturer>> + Send + Sync),
        max: (usize, usize),
    ) {
        /// However the thread ends, the stream thread learns of it.
        struct Ends<'a>(&'a Pipe);
        impl Drop for Ends<'_> {
            fn drop(&mut self) {
                let mut s = self.0.lock();
                if s.ended.is_none() && !s.stop {
                    s.ended = Some(Ended::Capture(CaptureError::Backend(
                        "capture thread ended".into(),
                    )));
                }
                self.0.wake.notify_all();
            }
        }
        let _ends = Ends(self);
        // Conversions here use a quarter of the cores (at least 2; `RCONN_CAPTURE_THREADS`):
        // the encoder works meanwhile (with all 8 of a VM's cores, Microsoft's encoder
        // took 21-47 ms a 2992x1878 picture alongside, 19-21 ms with 2).
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        rconn_media::yuv::limit_threads_here(Some(
            std::env::var("RCONN_CAPTURE_THREADS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or((cores / 4).max(2)),
        ));
        let mut cap = match make() {
            Ok(c) => c,
            Err(e) => {
                self.lock().ended = Some(Ended::Start(e));
                return;
            }
        };
        let mut live: Option<I420> = None;
        loop {
            {
                let mut s = self.lock();
                loop {
                    if s.stop {
                        return;
                    }
                    // (A capture ahead of the slot starts as long before it as one takes.)
                    let ahead = s.takes + CAPTURE_LEAD;
                    let early = s.next_slot.checked_sub(ahead).unwrap_or(s.next_slot);
                    let start = if s.waiting() {
                        early.max(s.published + self.budget)
                    } else {
                        early
                    };
                    let now = Instant::now();
                    if now >= start {
                        break;
                    }
                    s = self.wait(s, start - now);
                }
                s.inflight = Some(Instant::now());
            }
            let t0 = Instant::now();
            let got = cap.next_into(CAPTURE_WAIT, &mut live, max);
            let moves = cap.take_moves();
            self.lock().inflight = None;
            match got {
                Ok(Some(c)) => {
                    let took = t0.elapsed();
                    {
                        let mut s = self.lock();
                        let took = took.min(self.budget);
                        s.takes = if s.takes.is_zero() {
                            took
                        } else {
                            (s.takes + took) / 2
                        };
                    }
                    let live = live.as_ref().expect("a change was captured into it");
                    self.publish(live, c, moves, took);
                }
                Ok(None) => {
                    if !moves.is_empty() {
                        let mut s = self.lock();
                        s.moves.extend(moves);
                        s.moves.truncate(64);
                    }
                }
                Err(e) => {
                    self.lock().ended = Some(Ended::Capture(e));
                    return;
                }
            }
        }
    }

    /// A capture changed `c.rows` of the capturer's picture `live`.
    fn publish(&self, live: &I420, c: Captured, moves: Vec<Move>, took: Duration) {
        let mut guard = self.lock();
        let s = &mut *guard;
        let rows = union(s.behind, c.rows);
        match &mut s.picture {
            Some(p) if p.dimensions() == live.dimensions() => p.copy_rows_from(live, rows),
            slot => *slot = Some(live.clone()),
        }
        s.behind = None;
        s.encoder_behind = Some(union(s.encoder_behind, c.rows));
        s.fresh = Some(Pending::merge(s.fresh.take(), c));
        s.published = Instant::now();
        s.moves.extend(moves);
        s.moves.truncate(64);
        s.capture_time += took;
        s.captures += 1;
        drop(guard);
        self.wake.notify_all();
    }

    /// The stream thread: wait up to `timeout` for a change. Its rows and share, to add to
    /// the changes pending; its pixels stay waiting until [`Self::take`] (only a picture
    /// of another size, or the first, comes into `picture` at once).
    pub(super) fn poll(
        &self,
        timeout: Duration,
        picture: &mut Option<I420>,
    ) -> Result<Option<Captured>, Ended> {
        let deadline = Instant::now() + timeout;
        let mut s = self.lock();
        loop {
            if let Some(e) = s.ended.take() {
                return Err(e);
            }
            if s.fresh.is_some() || s.resized(picture) {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            s = self.wait(s, deadline - now);
        }
        let c = s.fresh.take().map(to_captured);
        s.told = true;
        if s.resized(picture) {
            let unheard = s.swap(picture);
            debug_assert!(unheard.is_none());
            drop(s);
            self.wake.notify_all();
        }
        Ok(c)
    }

    /// The stream thread, before encoding the changes it was told of: `picture` becomes
    /// the newest capture. The changes in it not told of yet, and whether its size
    /// changed (then the frame is decided again).
    pub(super) fn take(&self, picture: &mut Option<I420>) -> (Option<Captured>, bool) {
        let mut s = self.lock();
        if !s.waiting() {
            return (None, false);
        }
        let resized = s.resized(picture);
        let unheard = s.swap(picture);
        drop(s);
        self.wake.notify_all();
        (unheard, resized)
    }

    /// Areas the OS reported moved since the last call ([`Capturer::take_moves`]).
    pub(super) fn take_moves(&self) -> Vec<Move> {
        std::mem::take(&mut self.lock().moves)
    }

    /// Time spent capturing and captures made since the last call.
    pub(super) fn capture_stats(&self) -> (Duration, u32) {
        let mut s = self.lock();
        let out = (s.capture_time, s.captures);
        s.capture_time = Duration::ZERO;
        s.captures = 0;
        out
    }

    /// A capture under way that would be in by then if the screen changed (it started
    /// less than a capture's time ago): re-encodes of a still picture wait for it, or one
    /// would take the slot of the change it brings.
    pub(super) fn capture_due(&self, now: Instant) -> Option<Instant> {
        let s = self.lock();
        s.inflight
            .map(|t| t + s.takes + (s.takes / 4).max(CAPTURE_SLACK))
            .filter(|&due| due > now)
    }

    /// The stream thread's next frame may start at `slot`.
    pub(super) fn next_slot(&self, slot: Instant) {
        self.lock().next_slot = slot;
        self.wake.notify_all();
    }

    /// Captures end (the capture thread returns after its current wait).
    pub(super) fn stop(&self) {
        self.lock().stop = true;
        self.wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rconn_media::capture::CaptureError;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A capturer whose `k`-th capture (from 1) writes `k` into the first two bytes of
    /// row pair `k % (h / 2)` (and changes just those rows).
    struct Rows {
        size: (usize, usize),
        k: Arc<AtomicU32>,
        every: Duration,
    }

    impl Capturer for Rows {
        fn next_frame(
            &mut self,
            _: Duration,
        ) -> Result<Option<rconn_media::video::Frame>, CaptureError> {
            unreachable!("next_into")
        }

        fn next_into(
            &mut self,
            _: Duration,
            picture: &mut Option<I420>,
            _: (usize, usize),
        ) -> Result<Option<Captured>, CaptureError> {
            std::thread::sleep(self.every);
            let (w, h) = self.size;
            let k = self.k.fetch_add(1, Ordering::SeqCst) as usize + 1;
            let pic = picture.get_or_insert_with(|| I420::new(w, h));
            let row = (k % (h / 2)) * 2;
            let (y, _, _) = pic.planes_mut();
            for r in row..row + 2 {
                y[r * w..r * w + 2].copy_from_slice(&(k as u16).to_le_bytes());
            }
            Ok(Some(Captured {
                rows: (row, row + 2),
                changed: 2.0 / h as f32,
                captured_at: Instant::now(),
            }))
        }
    }

    /// Captures keep coming while the stream thread is busy; whatever it takes is the
    /// capturer's newest picture, whole (rows changed while it held an older one come
    /// with it), and the changes it is told of cover every row that changed.
    #[test]
    fn the_picture_taken_is_the_newest_and_whole() {
        let size = (16, 40);
        let k = Arc::new(AtomicU32::new(0));
        let pipe = Pipe::new(Duration::from_millis(1));
        let make = {
            let k = k.clone();
            move || -> anyhow::Result<Box<dyn Capturer>> {
                Ok(Box::new(Rows {
                    size,
                    k: k.clone(),
                    every: Duration::from_millis(1),
                }))
            }
        };
        std::thread::scope(|s| {
            s.spawn(|| pipe.capture(&make, (4096, 4096)));
            let mut picture = None;
            let mut covered = vec![false; size.1];
            for round in 0..30 {
                let mut told = Vec::new();
                while told.len() < 3 {
                    if let Some(c) = pipe
                        .poll(Duration::from_millis(100), &mut picture)
                        .ok()
                        .flatten()
                    {
                        told.push(c.rows);
                    }
                }
                // Busy "encoding" meanwhile: more captures land.
                std::thread::sleep(Duration::from_millis(3));
                let (unheard, resized) = pipe.take(&mut picture);
                assert!(!resized || round == 0);
                told.extend(unheard.map(|c| c.rows));
                for (a, b) in told {
                    covered[a..b].iter_mut().for_each(|c| *c = true);
                }
                // The capturer's picture as of its newest capture n: each row pair holds
                // the last capture up to n that wrote it.
                let pic = picture.as_ref().expect("taken");
                let y = pic.luma();
                let at =
                    |r: usize| usize::from(u16::from_le_bytes([y[r * size.0], y[r * size.0 + 1]]));
                let pairs = size.1 / 2;
                let n = (0..pairs).map(|p| at(p * 2)).max().unwrap();
                assert!(n > 0);
                for p in 0..pairs {
                    let back = (n + pairs - p) % pairs;
                    let want = if n >= back && n - back >= 1 {
                        n - back
                    } else {
                        0
                    };
                    assert_eq!(
                        (at(p * 2), at(p * 2 + 1)),
                        (want, want),
                        "round {round}, pair {p}, newest {n}"
                    );
                }
            }
            assert!(
                covered.iter().all(|&c| c),
                "a changed row was never reported"
            );
            pipe.stop();
        });
    }

    /// A capturer that cannot be made, or fails later, ends the stream thread's waits.
    #[test]
    fn capture_failures_reach_the_stream_thread() {
        let pipe = Pipe::new(Duration::from_millis(33));
        let make = || -> anyhow::Result<Box<dyn Capturer>> { anyhow::bail!("no screen") };
        std::thread::scope(|s| {
            s.spawn(|| pipe.capture(&make, (4096, 4096)));
            let mut picture = None;
            let t0 = Instant::now();
            loop {
                match pipe.poll(Duration::from_millis(50), &mut picture) {
                    Err(Ended::Start(e)) => {
                        assert!(e.to_string().contains("no screen"));
                        break;
                    }
                    Err(Ended::Capture(e)) => panic!("{e}"),
                    Ok(_) => assert!(t0.elapsed() < Duration::from_secs(5)),
                }
            }
        });
    }
}
