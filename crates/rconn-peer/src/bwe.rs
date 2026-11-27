//! Send-side bandwidth estimation for screen sharing, after libwebrtc's GoogCC and str0m's
//! BWE (both documented designs; no code copied).
//!
//! # Why not `rtc`'s `Gcc`
//!
//! `rtc` 0.21.1 ships a Pion-style GCC (Kalman delay filter, AIMD on a 200 ms cadence). It
//! has no notion of an *application-limited* sender, and a screen is application-limited
//! nearly all the time: idle, then one large keyframe. On overuse it backs off to
//! 0.85 x the *received* rate, and the received rate of an idle screen is a few hundred
//! kb/s, so a single self-made burst took the estimate from 8.5 to 1 Mb/s, and climbing
//! back at 8 %/s took half a minute (see `tests` here: the same trace through both). It
//! cannot be configured out of that, so the estimator is replaced at the
//! [`BandwidthEstimator`] seam `rtc` provides.
//!
//! # What this one does
//!
//! * **Delay:** trendline filter over 5 ms send groups (20-group window, adaptive
//!   threshold 6-600), libwebrtc's constants.
//! * **Backoff base:** 0.85 x a rate that measured the link, never more than half the
//!   estimate per backoff. Measured means: the acknowledged rate while the network
//!   delivered clearly less than was sent (a queue grew), or the rate a packet *train*
//!   (packets the pacer sent back-to-back: a keyframe, an update) arrived at, taken from
//!   its median packet gap so a receiver that stalled mid-train (a busy viewer or VM
//!   stamping a bunch of packets late) does not read as a slow link. A rate measured
//!   while application-limited (all that was sent arrived) says nothing about capacity.
//! * **ALR (application-limited region):** detected with an interval budget at 65 % of the
//!   estimate (enter above 80 % unused, leave below 50 %). While application-limited the
//!   estimate never grows past 1.5 x the delivered rate, and an overuse signal that no
//!   measurement backs is held, not cut: jitter on a screen sending 0.3 Mb/s once took a
//!   25 Mb/s estimate to 1.5. A link that really shrank shows at the next burst.
//! * **Loss:** libwebrtc's classic rule (< 2 % may grow, 2-10 % hold, > 10 % cut by half
//!   the loss fraction).
//! * **Large-drop recovery:** after a drop to <= 66 % of the recent peak, once delay has
//!   been normal for 500 ms, the estimate ramps (x2 per second) back to 85 % of that
//!   peak. str0m sends padding probes for this; `rtc`'s pacer cannot emit padding, so the
//!   video itself is the probe, and ramping rather than jumping keeps the overshoot small
//!   when the link really did shrink. A ramp that meets overuse backs off as usual and
//!   is retried 3 s later, for up to 30 s after the drop.
//! * **Cadence:** the controller runs on every feedback report and on a 25 ms timer while
//!   reports keep coming (`poll_timeout`), idle otherwise.
//!
//! The pacer rate and the encoder target are derived from the estimate by the caller
//! ([`crate::video`]): pacer at [`PACING_FACTOR`] x estimate, encoder below it.

use rtc::interceptor::{BandwidthEstimator, EstimatorStats, PacketReport};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// The pacer releases at this multiple of the estimate: room for keyframes and
/// retransmissions above the encoder's average without building our own queue.
pub const PACING_FACTOR: f64 = 1.25;
/// The controller's timer while feedback flows.
pub const UPDATE_INTERVAL: Duration = Duration::from_millis(25);

const GROUP_SPAN: Duration = Duration::from_millis(5);
const TREND_WINDOW: usize = 20;
const SMOOTHING: f64 = 0.9;
const THRESHOLD_GAIN: f64 = 4.0;
const MAX_DELTAS: f64 = 60.0;
const OVERUSE_TIME_MS: f64 = 10.0;
const K_UP: f64 = 0.0087;
const K_DOWN: f64 = 0.039;
const THRESHOLD_INIT: f64 = 12.5;

const BETA: f64 = 0.85;
const INCREASE_PER_SECOND: f64 = 1.08;
const RATE_WINDOW: Duration = Duration::from_millis(500);
const TRAIN_MIN_PACKETS: usize = 8;
/// Below this share of the send rate, what arrived measures the link (a queue grew)...
const LINK_LIMITED: f64 = 0.8;
/// ...if it also grew by at least this much: more than jitter does.
const LINK_LIMITED_GROWTH: Duration = Duration::from_millis(50);
/// A backoff not backed by a queue that grew never takes more than half the estimate.
const MAX_CUT: f64 = 0.5;
const TRAIN_MAX_AGE: Duration = Duration::from_secs(1);

const ALR_USAGE: f64 = 0.65;
const ALR_ENTER: f64 = 0.8;
const ALR_EXIT: f64 = 0.5;

const LOSS_WINDOW: Duration = Duration::from_secs(1);
/// Backoffs look at what was delivered this recently: the link as it is now.
const DECREASE_WINDOW: Duration = Duration::from_millis(300);
const LOSS_LOW: f64 = 0.02;
const LOSS_HIGH: f64 = 0.10;

const DROP_RATIO: f64 = 0.66;
const RECOVER_TO: f64 = 0.85;
const PEAK_WINDOW: Duration = Duration::from_secs(3);
const RECOVER_AFTER_NORMAL: Duration = Duration::from_millis(500);
const RECOVER_RAMP_PER_SECOND: f64 = 2.0;
const RECOVER_FAILED_WITHIN: Duration = Duration::from_secs(2);
const RECOVER_BACKOFF: Duration = Duration::from_secs(3);
/// A drop this old is history: the estimate grows normally from there.
const DROP_MEMORY: Duration = Duration::from_secs(30);
/// Reports stop: the timer stops too.
const IDLE_AFTER: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Usage {
    Normal,
    Over,
    Under,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    Hold,
    Increase,
    Decrease,
}

/// Packets sent within [`GROUP_SPAN`] of each other: one burst, one delay sample.
#[derive(Debug, Clone, Copy)]
struct Group {
    first_departure: Instant,
    last_departure: Instant,
    last_arrival: Duration,
}

/// Delay trend over send groups, and the overuse detector on top of it.
#[derive(Debug)]
struct Trendline {
    prev: Option<Group>,
    cur: Option<Group>,
    first_arrival: Option<Duration>,
    accumulated: f64,
    smoothed: f64,
    window: VecDeque<(f64, f64)>,
    deltas: f64,
    trend: f64,
    prev_trend: f64,
    threshold: f64,
    last_threshold_update: Option<f64>,
    time_over: f64,
    over_count: u32,
    usage: Usage,
}

impl Default for Trendline {
    fn default() -> Self {
        Self {
            prev: None,
            cur: None,
            first_arrival: None,
            accumulated: 0.0,
            smoothed: 0.0,
            window: VecDeque::new(),
            deltas: 0.0,
            trend: 0.0,
            prev_trend: 0.0,
            threshold: THRESHOLD_INIT,
            last_threshold_update: None,
            time_over: -1.0,
            over_count: 0,
            usage: Usage::Normal,
        }
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

impl Trendline {
    fn packet(&mut self, departure: Instant, arrival: Duration) {
        let Some(cur) = self.cur.as_mut() else {
            self.cur = Some(Group {
                first_departure: departure,
                last_departure: departure,
                last_arrival: arrival,
            });
            return;
        };
        if departure < cur.first_departure {
            return; // reordered: not useful for a gradient
        }
        if departure.duration_since(cur.first_departure) <= GROUP_SPAN {
            cur.last_departure = cur.last_departure.max(departure);
            cur.last_arrival = cur.last_arrival.max(arrival);
            return;
        }
        let done = *cur;
        self.cur = Some(Group {
            first_departure: departure,
            last_departure: departure,
            last_arrival: arrival,
        });
        if let Some(prev) = self.prev.replace(done) {
            if done.last_arrival < prev.last_arrival {
                return;
            }
            let send = ms(done.last_departure.duration_since(prev.last_departure));
            let recv = ms(done.last_arrival - prev.last_arrival);
            self.delta(recv - send, send, done.last_arrival);
        }
    }

    fn delta(&mut self, delay_ms: f64, send_delta_ms: f64, arrival: Duration) {
        let first = *self.first_arrival.get_or_insert(arrival);
        self.deltas = (self.deltas + 1.0).min(MAX_DELTAS);
        self.accumulated += delay_ms;
        self.smoothed = SMOOTHING * self.smoothed + (1.0 - SMOOTHING) * self.accumulated;
        let x = ms(arrival.saturating_sub(first));
        self.window.push_back((x, self.smoothed));
        if self.window.len() > TREND_WINDOW {
            self.window.pop_front();
        }
        if self.window.len() == TREND_WINDOW
            && let Some(slope) = slope(&self.window)
        {
            self.trend = slope;
        }
        self.detect(send_delta_ms, x);
    }

    fn detect(&mut self, send_delta_ms: f64, now_ms: f64) {
        if self.deltas < 2.0 {
            self.usage = Usage::Normal;
            return;
        }
        let modified = self.deltas * self.trend * THRESHOLD_GAIN;
        if modified > self.threshold {
            if self.time_over < 0.0 {
                self.time_over = send_delta_ms / 2.0;
            } else {
                self.time_over += send_delta_ms;
            }
            self.over_count += 1;
            if self.time_over > OVERUSE_TIME_MS
                && self.over_count > 1
                && self.trend >= self.prev_trend
            {
                self.time_over = 0.0;
                self.over_count = 0;
                self.usage = Usage::Over;
            }
        } else if modified < -self.threshold {
            self.time_over = -1.0;
            self.over_count = 0;
            self.usage = Usage::Under;
        } else {
            self.time_over = -1.0;
            self.over_count = 0;
            self.usage = Usage::Normal;
        }
        self.prev_trend = self.trend;
        // Adaptive threshold: follows the trend slowly up, faster down; ignores spikes.
        let last = *self.last_threshold_update.get_or_insert(now_ms);
        if modified.abs() > self.threshold + 15.0 {
            self.last_threshold_update = Some(now_ms);
            return;
        }
        let k = if modified.abs() < self.threshold {
            K_DOWN
        } else {
            K_UP
        };
        let dt = (now_ms - last).clamp(0.0, 100.0);
        self.threshold =
            (self.threshold + k * (modified.abs() - self.threshold) * dt).clamp(6.0, 600.0);
        self.last_threshold_update = Some(now_ms);
    }
}

/// Least-squares slope of y over x.
fn slope(points: &VecDeque<(f64, f64)>) -> Option<f64> {
    let n = points.len() as f64;
    let (sx, sy) = points
        .iter()
        .fold((0.0, 0.0), |(a, b), (x, y)| (a + x, b + y));
    let (mx, my) = (sx / n, sy / n);
    let (mut num, mut den) = (0.0, 0.0);
    for (x, y) in points {
        num += (x - mx) * (y - my);
        den += (x - mx) * (x - mx);
    }
    (den > 0.0).then(|| num / den)
}

/// Budget refilled at [`ALR_USAGE`] x the estimate, drained by what is sent: a mostly full
/// budget means the sender is not using what it may.
#[derive(Debug, Default)]
struct Alr {
    budget_bits: f64,
    last: Option<Instant>,
    active: bool,
}

impl Alr {
    fn update(&mut self, now: Instant, estimate: f64, sent_bits: f64) {
        let window = 0.5;
        let max = ALR_USAGE * estimate * window;
        if let Some(last) = self.last {
            let dt = now
                .saturating_duration_since(last)
                .as_secs_f64()
                .min(window);
            self.budget_bits += ALR_USAGE * estimate * dt;
        }
        self.last = Some(now);
        self.budget_bits = (self.budget_bits - sent_bits).clamp(-max, max);
        let level = if max > 0.0 {
            self.budget_bits / max
        } else {
            0.0
        };
        if level > ALR_ENTER {
            self.active = true;
        } else if level < ALR_EXIT {
            self.active = false;
        }
    }
}

/// The estimator. Construct with [`Bwe::new`]; drive through [`BandwidthEstimator`].
#[derive(Debug)]
pub struct Bwe {
    min: f64,
    max: f64,
    estimate: f64,
    trend: Trendline,
    control: Control,
    last_change: Option<Instant>,
    last_decrease: Option<Instant>,
    /// Exponentially averaged delivered rate at backoffs: the link's known ceiling.
    capacity: Option<f64>,
    /// Arrived packets (remote arrival time, departure, bytes), newest last.
    arrivals: VecDeque<(Duration, Instant, usize)>,
    /// Packet trains: when measured, the rate first to last packet arrived at, and the
    /// rate from the median packet gap.
    trains: VecDeque<(Instant, f64, f64)>,
    alr: Alr,
    /// (when, packets, lost) per report batch.
    loss_window: VecDeque<(Instant, usize, usize)>,
    loss_estimate: f64,
    last_loss_cut: Option<Instant>,
    rtt: Option<Duration>,
    peaks: VecDeque<(Instant, f64)>,
    /// The peak before a large drop, and when it dropped.
    pre_drop: Option<(f64, Instant)>,
    /// Ramping back up towards this after a large drop.
    recovering: Option<f64>,
    normal_since: Option<Instant>,
    recovered_at: Option<Instant>,
    recovery_failed_at: Option<Instant>,
    last_report: Option<Instant>,
    last_tick: Option<Instant>,
    /// Large drops and successful recoveries so far (for logs and tests).
    pub drops: u32,
    pub recoveries: u32,
}

impl Bwe {
    pub fn new(start: f64, min: f64, max: f64) -> Self {
        let start = start.clamp(min, max);
        Self {
            min,
            max,
            estimate: start,
            trend: Trendline::default(),
            control: Control::Increase,
            last_change: None,
            last_decrease: None,
            capacity: None,
            arrivals: VecDeque::new(),
            trains: VecDeque::new(),
            alr: Alr::default(),
            loss_window: VecDeque::new(),
            loss_estimate: start,
            last_loss_cut: None,
            rtt: None,
            peaks: VecDeque::new(),
            pre_drop: None,
            recovering: None,
            normal_since: None,
            recovered_at: None,
            recovery_failed_at: None,
            last_report: None,
            last_tick: None,
            drops: 0,
            recoveries: 0,
        }
    }

    pub fn usage(&self) -> Usage {
        self.trend.usage
    }

    pub fn application_limited(&self) -> bool {
        self.alr.active
    }

    /// The larger of the acknowledged rate and recent train rates, bits/s, over `window`
    /// (arrival time for the first, measurement time for the second).
    fn delivered(&self, now: Instant, window: Duration) -> Option<f64> {
        let acked = self.acked_rate(window);
        let train = self.train_rate(now, window);
        match (acked, train) {
            (Some(a), Some(t)) => Some(a.max(t)),
            (a, t) => a.or(t),
        }
    }

    fn acked_rate(&self, window: Duration) -> Option<f64> {
        let (newest, _, _) = *self.arrivals.back()?;
        let (oldest, _, _) = *self.arrivals.front()?;
        let span = newest
            .saturating_sub(oldest)
            .clamp(Duration::from_millis(100), window);
        let bytes: usize = self
            .arrivals
            .iter()
            .filter(|&&(a, _, _)| newest.saturating_sub(a) <= window)
            .map(|(_, _, b)| b)
            .sum();
        Some(bytes as f64 * 8.0 / span.as_secs_f64())
    }

    /// Did the packets that arrived within `window` arrive clearly slower than they were
    /// sent? Then a queue grew, and their arrival rate is the link's. If they arrived
    /// about as fast as they left, the sender was the limit and the rate says nothing.
    fn link_limited(&self, window: Duration) -> bool {
        let Some(&(newest, _, _)) = self.arrivals.back() else {
            return false;
        };
        let recent: Vec<_> = self
            .arrivals
            .iter()
            .filter(|&&(a, _, _)| newest.saturating_sub(a) <= window)
            .collect();
        let (Some(&&(first_arrival, d0, _)), true) = (recent.first(), recent.len() >= 10) else {
            return false;
        };
        let (sent_from, sent_to) = recent
            .iter()
            .fold((d0, d0), |(a, b), &&(_, d, _)| (a.min(d), b.max(d)));
        // Short spans are all jitter.
        let floor = Duration::from_millis(100);
        let arrived = newest.saturating_sub(first_arrival).max(floor);
        let sent = sent_to.duration_since(sent_from).max(floor);
        sent.as_secs_f64() < LINK_LIMITED * arrived.as_secs_f64()
            && arrived.saturating_sub(sent) >= LINK_LIMITED_GROWTH
    }

    /// Median of the trains measured within `window` (first to last packet).
    fn train_rate(&self, now: Instant, window: Duration) -> Option<f64> {
        self.median_train(now, window, |span, _| span)
    }

    /// What recent trains say the link carries at least: per train the larger of its
    /// first-to-last rate and its median-gap rate, so neither a receiver that stalled
    /// mid-train (one long gap) nor jitter that bunched packets makes a fast link look
    /// slow. Through a real bottleneck both are the link's rate. Median over trains.
    fn train_capacity(&self, now: Instant, window: Duration) -> Option<f64> {
        self.median_train(now, window, f64::max)
    }

    fn median_train(
        &self,
        now: Instant,
        window: Duration,
        rate: impl Fn(f64, f64) -> f64,
    ) -> Option<f64> {
        let mut r: Vec<f64> = self
            .trains
            .iter()
            .filter(|&&(t, _, _)| now.saturating_duration_since(t) <= window)
            .map(|&(_, span, gap)| rate(span, gap))
            .collect();
        if r.is_empty() {
            return None;
        }
        r.sort_by(f64::total_cmp);
        Some(r[r.len() / 2])
    }

    fn observe(&mut self, now: Instant, reports: &[PacketReport]) {
        let mut sent_bits = 0.0;
        let pacing = self.estimate * PACING_FACTOR;
        let mut train: Vec<&PacketReport> = Vec::new();
        let finish = |train: &mut Vec<&PacketReport>,
                      trains: &mut VecDeque<(Instant, f64, f64)>| {
            if train.len() >= TRAIN_MIN_PACKETS
                && let (Some(a), Some(b)) = (train[0].arrival, train[train.len() - 1].arrival)
                && b > a + Duration::from_millis(2)
            {
                let bytes: usize = train[1..].iter().map(|r| r.size).sum();
                let span = bytes as f64 * 8.0 / (b - a).as_secs_f64();
                // Packets a stalled receiver stamped late arrive as one bunch after one
                // long gap; through a bottleneck the gaps are even.
                let mut gaps: Vec<Duration> = train
                    .windows(2)
                    .filter_map(|w| Some(w[1].arrival?.saturating_sub(w[0].arrival?)))
                    .collect();
                gaps.sort();
                let gap = gaps[gaps.len() / 2].max(Duration::from_micros(50));
                let per_packet = bytes as f64 * 8.0 / (train.len() - 1) as f64;
                trains.push_back((now, span, per_packet / gap.as_secs_f64()));
            }
            train.clear();
        };
        for r in reports {
            sent_bits += r.size as f64 * 8.0;
            let Some(arrival) = r.arrival.filter(|_| r.arrived) else {
                finish(&mut train, &mut self.trains);
                continue;
            };
            self.trend.packet(r.departure, arrival);
            self.arrivals.push_back((arrival, r.departure, r.size));
            // Back-to-back at the pacer: the gap one packet takes at the pacing rate,
            // with slack.
            let gap_limit =
                Duration::from_secs_f64((2.5 * r.size as f64 * 8.0 / pacing.max(1.0)).max(0.002));
            if let Some(last) = train.last()
                && (r.departure.saturating_duration_since(last.departure) > gap_limit
                    || last.arrival.is_some_and(|a| arrival < a))
            {
                finish(&mut train, &mut self.trains);
            }
            train.push(r);
        }
        finish(&mut train, &mut self.trains);
        if let Some(&(newest, _, _)) = self.arrivals.back() {
            while self
                .arrivals
                .front()
                .is_some_and(|&(a, _, _)| newest.saturating_sub(a) > RATE_WINDOW)
            {
                self.arrivals.pop_front();
            }
        }
        while self
            .trains
            .front()
            .is_some_and(|&(t, _, _)| now.saturating_duration_since(t) > TRAIN_MAX_AGE)
        {
            self.trains.pop_front();
        }
        self.alr.update(now, self.estimate, sent_bits);
        let lost = reports.iter().filter(|r| !r.arrived).count();
        self.loss_window.push_back((now, reports.len(), lost));
        while self
            .loss_window
            .front()
            .is_some_and(|&(t, _, _)| now.saturating_duration_since(t) > LOSS_WINDOW)
        {
            self.loss_window.pop_front();
        }
        // Upper bound on the round trip: feedback for the newest packet came back now.
        if let Some(last) = reports.iter().map(|r| r.departure).max() {
            let sample = now.saturating_duration_since(last);
            self.rtt = Some(self.rtt.map_or(sample, |r| (r * 7 + sample) / 8));
        }
    }

    fn loss_fraction(&self) -> Option<f64> {
        let (n, lost) = self
            .loss_window
            .iter()
            .fold((0, 0), |(n, l), &(_, a, b)| (n + a, l + b));
        (n >= 20).then(|| lost as f64 / n as f64)
    }

    fn update_loss(&mut self, now: Instant) {
        let Some(loss) = self.loss_fraction() else {
            return;
        };
        if loss > LOSS_HIGH {
            let gap = Duration::from_millis(300) + self.rtt.unwrap_or(Duration::from_millis(100));
            if self
                .last_loss_cut
                .is_none_or(|t| now.saturating_duration_since(t) >= gap)
            {
                self.loss_estimate = (self.estimate.min(self.loss_estimate)) * (1.0 - 0.5 * loss);
                self.last_loss_cut = Some(now);
            }
        } else if loss < LOSS_LOW {
            // Not the limit: let the delay half decide.
            self.loss_estimate = self.max;
        }
        self.loss_estimate = self.loss_estimate.clamp(self.min, self.max);
    }

    fn update_delay(&mut self, now: Instant) {
        let usage = self.trend.usage;
        self.control = match (usage, self.control) {
            (Usage::Over, _) => Control::Decrease,
            (Usage::Under, _) => Control::Hold,
            (Usage::Normal, Control::Hold) => Control::Increase,
            (Usage::Normal, Control::Decrease) => Control::Hold,
            (Usage::Normal, Control::Increase) => Control::Increase,
        };
        let elapsed = self.last_change.map_or(Duration::from_millis(100), |t| {
            now.saturating_duration_since(t)
        });
        let delivered = self.delivered(now, RATE_WINDOW);
        match self.control {
            Control::Hold => {}
            Control::Decrease => {
                // Each backoff is given a round trip to show before the next.
                let wait = self
                    .rtt
                    .unwrap_or(Duration::from_millis(100))
                    .max(Duration::from_millis(100));
                if self
                    .last_decrease
                    .is_some_and(|t| now.saturating_duration_since(t) < wait)
                {
                    // Already cut for this episode.
                } else {
                    let train = self.train_capacity(now, DECREASE_WINDOW);
                    let delivered = self.delivered(now, DECREASE_WINDOW);
                    // What measured the link, if anything did.
                    let queue_grew = self.link_limited(DECREASE_WINDOW);
                    let measured = if queue_grew {
                        // Less arrived than was sent: the link's rate.
                        delivered
                    } else if !self.alr.active {
                        // Sending about what the estimate allows, and it all arrived:
                        // the link is about full.
                        delivered
                    } else if let Some(t) = train.filter(|&t| t < self.estimate) {
                        // Application-limited, but a burst arrived slower than the
                        // estimate allows.
                        Some(t)
                    } else {
                        // Application-limited, nothing measured the link: what arrived
                        // is what was sent. Hold; a link that really shrank shows at the
                        // next burst.
                        None
                    };
                    if let Some(d) = measured {
                        // Only a queue that grew may cut deeper than half at once (the
                        // link really shrank: waiting a round trip per halving would
                        // leave the queue standing for seconds).
                        let floor = if queue_grew {
                            0.0
                        } else {
                            MAX_CUT * self.estimate
                        };
                        let target = (BETA * d).max(floor);
                        if target < self.estimate {
                            // Averaged, but a sample far below means the link changed.
                            self.capacity = Some(match self.capacity {
                                Some(c) if d >= 0.7 * c => 0.95 * c + 0.05 * d,
                                _ => d,
                            });
                            self.estimate = target;
                        }
                    }
                    self.last_change = Some(now);
                    self.last_decrease = Some(now);
                    // A recovery that ran into this: the link did change. Not again soon.
                    let recent = self
                        .recovered_at
                        .is_some_and(|t| now.saturating_duration_since(t) < RECOVER_FAILED_WITHIN);
                    if self.recovering.take().is_some() || recent {
                        self.recovery_failed_at = Some(now);
                        self.recovered_at = None;
                    }
                }
                // Hold until the detector says normal again.
                self.control = Control::Hold;
            }
            Control::Increase => {
                let dt = elapsed.min(Duration::from_secs(1)).as_secs_f64();
                if self.capacity.is_some_and(|c| self.estimate > 1.15 * c) {
                    self.capacity = None; // above the old ceiling: it moved
                }
                let next = if let Some(goal) = self.recovering {
                    // Back towards the level before a large drop, doubling per second, so
                    // a link that really did shrink is found with little overshoot.
                    let next = (self.estimate * RECOVER_RAMP_PER_SECOND.powf(dt)).min(goal);
                    if next >= goal {
                        self.recovering = None;
                        self.recovered_at = Some(now);
                        self.recoveries += 1;
                        self.peaks.clear();
                    }
                    self.estimate = self.estimate.max(next);
                    self.last_change = Some(now);
                    return;
                } else if self.capacity.is_some() {
                    // Near the known ceiling: about one packet per response time.
                    let response =
                        self.rtt.unwrap_or(Duration::from_millis(100)).as_secs_f64() + 0.1;
                    let packet_bits = (self.estimate / 30.0).min(1200.0 * 8.0);
                    self.estimate + (packet_bits / response).max(4000.0) * dt
                } else {
                    self.estimate * INCREASE_PER_SECOND.powf(dt)
                };
                // Never far above what is actually being delivered (application-limited
                // screens would otherwise inflate it), but never lowered for that either.
                let cap = delivered.map_or(f64::INFINITY, |d| 1.5 * d + 10_000.0);
                self.estimate = self.estimate.max(next.min(cap));
                self.last_change = Some(now);
            }
        }
    }

    /// Large-drop bookkeeping and the recovery retry.
    fn update_recovery(&mut self, now: Instant) {
        if self.trend.usage == Usage::Normal {
            self.normal_since.get_or_insert(now);
        } else {
            self.normal_since = None;
        }
        let peak = self
            .peaks
            .iter()
            .map(|&(_, e)| e)
            .fold(self.estimate, f64::max);
        if self.estimate <= DROP_RATIO * peak && self.pre_drop.is_none_or(|(p, _)| p < peak) {
            if self.pre_drop.is_none() {
                self.drops += 1;
            }
            self.pre_drop = Some((peak, now));
        }
        if self
            .pre_drop
            .is_some_and(|(_, at)| now.saturating_duration_since(at) > DROP_MEMORY)
        {
            self.pre_drop = None;
        }
        if let Some((pre, _)) = self.pre_drop {
            let calm = self
                .normal_since
                .is_some_and(|t| now.saturating_duration_since(t) >= RECOVER_AFTER_NORMAL);
            // Known to be low: too few packets to tell is not "low".
            let low_loss = self.loss_fraction().is_some_and(|l| l < LOSS_LOW)
                && self.loss_estimate >= RECOVER_TO * pre;
            let allowed = self
                .recovery_failed_at
                .is_none_or(|t| now.saturating_duration_since(t) >= RECOVER_BACKOFF);
            if self.estimate >= RECOVER_TO * pre {
                self.pre_drop = None;
            } else if calm && low_loss && allowed && self.recovering.is_none() {
                // Kept until reached or forgotten: a ramp that fails is tried again.
                self.recovering = Some(RECOVER_TO * pre);
                self.capacity = None;
            }
        }
        self.peaks.push_back((now, self.estimate));
        while self
            .peaks
            .front()
            .is_some_and(|&(t, _)| now.saturating_duration_since(t) > PEAK_WINDOW)
        {
            self.peaks.pop_front();
        }
    }

    fn clamp(&mut self) {
        self.estimate = self
            .estimate
            .min(self.loss_estimate)
            .clamp(self.min, self.max);
    }
}

impl BandwidthEstimator for Bwe {
    fn on_reports(&mut self, now: Instant, reports: &[PacketReport]) {
        if reports.is_empty() {
            return;
        }
        self.last_report = Some(now);
        self.observe(now, reports);
        self.update_loss(now);
        self.update_delay(now);
        self.update_recovery(now);
        self.clamp();
        self.last_tick = Some(now);
    }

    fn target_bitrate(&self) -> f64 {
        self.estimate
    }

    fn handle_timeout(&mut self, now: Instant) {
        if self.last_tick.is_some_and(|t| now < t + UPDATE_INTERVAL) {
            return;
        }
        self.last_tick = Some(now);
        self.update_loss(now);
        self.update_recovery(now);
        self.clamp();
    }

    fn poll_timeout(&self) -> Option<Instant> {
        let last = self.last_report?;
        let tick = self.last_tick.unwrap_or(last) + UPDATE_INTERVAL;
        (tick <= last + IDLE_AFTER).then_some(tick)
    }

    fn stats(&self) -> EstimatorStats {
        let mut s = EstimatorStats::default();
        s.delay_based_bitrate = Some(self.estimate);
        s.loss_based_bitrate = Some(self.loss_estimate);
        s.packet_loss = self.loss_fraction();
        s.round_trip_time = self.rtt;
        s
    }
}

#[cfg(test)]
pub(crate) mod sim;

#[cfg(test)]
mod tests {
    //! Synthetic traces through the 1 ms simulation in [`sim`]; `--nocapture` prints the
    //! numbers, the same trace through 0.1.5's estimator (`rtc` GCC) alongside.
    use super::sim::*;
    use super::*;

    const MB: f64 = 1_000_000.0;

    fn print(name: &str, t: &Trace) {
        let line: Vec<String> = t
            .points
            .iter()
            .step_by(10)
            .map(|p| format!("{:.1}", p.1 / MB))
            .collect();
        println!("{name}: {} (lost {}/{})", line.join(" "), t.lost, t.sent);
    }

    /// An idle screen on an 8.5 Mb/s link, a 1 MB keyframe every 5 s (resizes, a new
    /// picture): the estimate must stay near the link, not collapse to what idle sends.
    #[test]
    fn keyframe_bursts_on_an_idle_screen_keep_the_estimate() {
        let link = 8.5 * MB;
        let s = Scenario {
            seconds: 30,
            capacity: Box::new(move |_| link),
            one_way: Duration::from_millis(20),
            loss: 0.0,
            jitter: Duration::ZERO,
            stalls: vec![],
            content: Box::new(|t| {
                if t < 8.0 {
                    Content::Busy
                } else {
                    Content::Idle
                }
            }),
            keyframes: vec![(12.0, 1_000_000), (17.0, 1_000_000), (22.0, 1_000_000)],
            start_bps: 8.0 * MB,
        };
        let new = run(bwe(s.start_bps), &s, NEW);
        let old = run(gcc(s.start_bps), &s, OLD);
        print("new", &new);
        print("old", &old);
        let (n, o) = (new.min_between(12.0, 30.0), old.min_between(12.0, 30.0));
        println!(
            "lowest estimate after the bursts: new {:.2} Mb/s ({:.0}% of link), old {:.2} Mb/s ({:.0}%)",
            n / MB,
            n / link * 100.0,
            o / MB,
            o / link * 100.0
        );
        assert!(n >= 0.7 * link, "collapsed to {:.2} Mb/s", n / MB);
    }

    /// The link drops from 10 to 3 Mb/s for 10 s, then comes back.
    #[test]
    fn follows_a_step_down_and_recovers() {
        let s = Scenario {
            seconds: 45,
            capacity: Box::new(|t| {
                if (10.0..20.0).contains(&t) {
                    3.0 * MB
                } else {
                    10.0 * MB
                }
            }),
            one_way: Duration::from_millis(20),
            loss: 0.0,
            jitter: Duration::ZERO,
            stalls: vec![],
            content: Box::new(|_| Content::Busy),
            keyframes: vec![],
            start_bps: 8.0 * MB,
        };
        let new = run(bwe(s.start_bps), &s, NEW);
        let old = run(gcc(s.start_bps), &s, OLD);
        print("new", &new);
        print("old", &old);
        let before = new.at(9.9);
        let down = new.below(10.0, 3.3 * MB).map(|t| t - 10.0);
        let back = new.reaches(20.0, 0.9 * before).map(|t| t - 20.0);
        let old_back = old.reaches(20.0, 0.9 * old.at(9.9)).map(|t| t - 20.0);
        println!(
            "new: {:.1} Mb/s before, below 3.3 after {down:?} s, worst queue {:.0} ms, back to 90% after {back:?} s (old: {old_back:?} s)",
            before / MB,
            new.max_queue_between(12.0, 20.0)
        );
        assert!(before >= 7.0 * MB, "converged to {:.1}", before / MB);
        assert!(down.is_some_and(|d| d <= 2.0), "slow to back off: {down:?}");
        assert!(
            new.max_queue_between(13.0, 20.0) < 200.0,
            "queue left standing"
        );
        assert!(back.is_some_and(|b| b <= 15.0), "slow to recover: {back:?}");
    }

    /// Random loss below 10 % is not congestion: the estimate holds.
    #[test]
    fn moderate_random_loss_is_tolerated_heavy_loss_is_not() {
        let mk = |loss| Scenario {
            seconds: 20,
            capacity: Box::new(|_| 10.0 * MB),
            one_way: Duration::from_millis(20),
            loss,
            jitter: Duration::ZERO,
            stalls: vec![],
            content: Box::new(|_| Content::Busy),
            keyframes: vec![],
            start_bps: 8.0 * MB,
        };
        let light = run(bwe(8.0 * MB), &mk(0.03), NEW);
        let heavy = run(bwe(8.0 * MB), &mk(0.20), NEW);
        print("3% loss", &light);
        print("20% loss", &heavy);
        assert!(light.min_between(5.0, 20.0) >= 6.0 * MB);
        assert!(heavy.at(19.0) < 3.0 * MB);
    }

    /// From 8 Mb/s on a 20 Mb/s link, busy screen.
    #[test]
    fn ramps_up_on_a_fast_link() {
        let s = Scenario {
            seconds: 30,
            capacity: Box::new(|_| 20.0 * MB),
            one_way: Duration::from_millis(10),
            loss: 0.0,
            jitter: Duration::ZERO,
            stalls: vec![],
            content: Box::new(|_| Content::Busy),
            keyframes: vec![],
            start_bps: 8.0 * MB,
        };
        let new = run(bwe(s.start_bps), &s, NEW);
        print("new", &new);
        let t = new.reaches(0.0, 15.0 * MB);
        println!("reaches 15 Mb/s after {t:?} s");
        assert!(t.is_some_and(|t| t <= 15.0));
        assert!(new.max_queue_between(20.0, 30.0) < 100.0);
    }

    /// What the logs showed (21354 -> 437 kb/s, 25000 -> 1489 kb/s while sending
    /// 0.2-0.5 Mb/s): an application-limited screen (typing, a small update each second)
    /// on a fast link whose packets arrive with jitter. Nothing is congested, so the
    /// estimate must not collapse to what the screen happened to send.
    #[test]
    fn an_application_limited_jittered_stream_keeps_the_estimate() {
        let start = 20.0 * MB;
        let s = Scenario {
            seconds: 40,
            capacity: Box::new(|_| 50.0 * MB),
            one_way: Duration::from_millis(15),
            loss: 0.0,
            jitter: Duration::from_millis(10),
            // The viewer stops reading for 120 ms every 2.5 s, now and then just as an
            // update arrives: that update's train seems to arrive at ~2 Mb/s.
            stalls: (0..16)
                .map(|k| (1.0 + 2.5 * k as f64 + 0.017, 0.12))
                .collect(),
            content: Box::new(|_| Content::Trickle(1500)),
            // A 30 KB update every second: a train of 25 packets.
            keyframes: (1..40).map(|t| (t as f64, 30_000)).collect(),
            start_bps: start,
        };
        let new = run(bwe(start), &s, NEW);
        print("new", &new);
        let low = new.min_between(2.0, 40.0);
        println!(
            "app-limited, jitter and read stalls: lowest estimate {:.2} Mb/s ({:.0}% of the start)",
            low / MB,
            low / start * 100.0
        );
        assert!(
            low >= 0.5 * start,
            "collapsed without congestion: {:.2} Mb/s",
            low / MB
        );
    }

    /// The other side of the above: a link that really shrinks (20 -> 2 Mb/s) is followed
    /// within about a second: under a busy screen, with and without jitter, and under an
    /// application-limited one (typing, a 100 KB update each second) whose viewer stalls
    /// now and then, where the next update's train shows the new link.
    #[test]
    fn a_real_capacity_drop_is_followed_within_a_second() {
        let stalls: Vec<(f64, f64)> = (0..8)
            .map(|k| (1.0 + 2.5 * k as f64 + 0.017, 0.12))
            .collect();
        let cases: [(&str, Duration, Vec<(f64, f64)>, Content, Vec<(f64, usize)>); 3] = [
            ("busy", Duration::ZERO, vec![], Content::Busy, vec![]),
            (
                "busy, 20 ms jitter",
                Duration::from_millis(20),
                vec![],
                Content::Busy,
                vec![],
            ),
            (
                "application-limited, jitter, stalls",
                Duration::from_millis(10),
                stalls,
                Content::Trickle(1500),
                (1..20).map(|t| (t as f64, 100_000)).collect(),
            ),
        ];
        for (name, jitter, stalls, content, keyframes) in cases {
            let s = Scenario {
                seconds: 20,
                capacity: Box::new(|t| if t < 10.0 { 20.0 * MB } else { 2.0 * MB }),
                one_way: Duration::from_millis(15),
                loss: 0.0,
                jitter,
                stalls,
                content: Box::new(move |_| content),
                keyframes,
                start_bps: 15.0 * MB,
            };
            let new = run(bwe(s.start_bps), &s, NEW);
            print("new", &new);
            let before = new.at(9.9);
            let followed = new.below(10.0, 2.5 * MB).map(|t| t - 10.0);
            println!(
                "{name}: {:.1} Mb/s before, at or below 2.5 Mb/s {followed:?} s after the drop, worst queue after 12 s {:.0} ms",
                before / MB,
                new.max_queue_between(12.0, 20.0)
            );
            assert!(
                before >= 12.0 * MB,
                "{name}: converged to {:.1}",
                before / MB
            );
            assert!(
                followed.is_some_and(|d| d <= 1.2),
                "{name}: slow to follow a real drop: {followed:?}"
            );
            assert!(
                new.max_queue_between(12.0, 20.0) < 200.0,
                "{name}: queue left standing"
            );
        }
    }

    #[test]
    fn idle_estimator_asks_for_no_timer() {
        let mut b = bwe(8.0 * MB);
        assert_eq!(b.poll_timeout(), None, "no reports yet");
        let t0 = Instant::now();
        b.on_reports(
            t0,
            &[PacketReport {
                ssrc: 1,
                id: 0,
                rtp_sequence_number: 0,
                is_twcc: true,
                twcc_sequence_number: 0,
                size: 1200,
                arrived: true,
                departure: t0,
                arrival: Some(Duration::from_millis(5)),
                ecn: Default::default(),
            }],
        );
        let tick = b.poll_timeout().expect("active");
        assert!(tick > t0);
        b.handle_timeout(t0 + IDLE_AFTER + UPDATE_INTERVAL);
        assert_eq!(b.poll_timeout(), None, "reports stopped: timer stops");
    }
}
