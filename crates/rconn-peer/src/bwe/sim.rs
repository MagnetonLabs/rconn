//! A 1 ms-step network simulation for the estimators: encoder model, `rtc`'s pacer, one
//! bottleneck link (FIFO, drop-tail, optional random loss), TWCC-style feedback every
//! 50 ms. Deterministic: same scenario, same numbers.

use super::*;
use rtc::interceptor::{Gcc, Pacer};
use rtc::rtcp::transport_feedbacks::cc_feedback_report::Ecn;

pub const PACKET: usize = 1200;
const STEP: Duration = Duration::from_millis(1);
const FEEDBACK_EVERY: Duration = Duration::from_millis(50);

/// What the screen is doing at a given time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Content {
    /// Every frame uses the encoder's budget (scrolling, video).
    Busy,
    /// A caret blinks: a small frame 5 times a second.
    Idle,
}

pub struct Scenario {
    pub seconds: u64,
    /// Link capacity in bits/s at time t (seconds).
    pub capacity: Box<dyn Fn(f64) -> f64>,
    pub one_way: Duration,
    /// Random loss probability per packet.
    pub loss: f64,
    pub content: Box<dyn Fn(f64) -> Content>,
    /// Keyframes (time in seconds, bytes), on top of the content.
    pub keyframes: Vec<(f64, usize)>,
    pub start_bps: f64,
}

pub struct Trace {
    /// (seconds, estimate bits/s, queueing delay at the bottleneck in ms), every 100 ms.
    pub points: Vec<(f64, f64, f64)>,
    pub lost: usize,
    pub sent: usize,
}

impl Trace {
    pub fn min_between(&self, a: f64, b: f64) -> f64 {
        self.points
            .iter()
            .filter(|p| p.0 >= a && p.0 <= b)
            .map(|p| p.1)
            .fold(f64::INFINITY, f64::min)
    }
    pub fn at(&self, t: f64) -> f64 {
        self.points
            .iter()
            .find(|p| p.0 >= t)
            .map_or(f64::NAN, |p| p.1)
    }
    /// First time at or after `from` the estimate reaches `bps`.
    pub fn reaches(&self, from: f64, bps: f64) -> Option<f64> {
        self.points
            .iter()
            .find(|p| p.0 >= from && p.1 >= bps)
            .map(|p| p.0)
    }
    pub fn below(&self, from: f64, bps: f64) -> Option<f64> {
        self.points
            .iter()
            .find(|p| p.0 >= from && p.1 <= bps)
            .map(|p| p.0)
    }
    pub fn max_queue_between(&self, a: f64, b: f64) -> f64 {
        self.points
            .iter()
            .filter(|p| p.0 >= a && p.0 <= b)
            .map(|p| p.2)
            .fold(0.0, f64::max)
    }
}

struct Sent {
    id: u64,
    size: usize,
    departure: Instant,
    arrival: Option<Instant>,
}

/// Deterministic xorshift.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// How the sender around the estimator is set up.
#[derive(Clone, Copy)]
pub struct Sender {
    /// Pacer rate / estimate.
    pub pacing: f64,
    /// Encoder target / estimate.
    pub encoder: f64,
    /// Pacer burst in bits; `None`: `rtc`'s default (100 ms of the rate).
    pub burst_bits: Option<f64>,
}

/// As 0.1.5 ran `rtc`'s GCC: pacer and encoder at the estimate, default burst.
pub const OLD: Sender = Sender {
    pacing: 1.0,
    encoder: 1.0,
    burst_bits: None,
};

/// As [`crate::video`] runs [`Bwe`] now.
pub const NEW: Sender = Sender {
    pacing: PACING_FACTOR,
    encoder: crate::video::ENCODER_SHARE,
    burst_bits: Some(crate::video::PACER_BURST_BITS),
};

pub fn run<E: BandwidthEstimator>(mut est: E, s: &Scenario, sender: Sender) -> Trace {
    let Sender {
        pacing,
        encoder,
        burst_bits,
    } = sender;
    let t0 = Instant::now();
    let mut pacer = Pacer::new(est.target_bitrate() * pacing);
    if let Some(b) = burst_bits {
        pacer = pacer.with_burst_bits(b);
    }
    let mut queue: VecDeque<usize> = VecDeque::new();
    let mut sent: Vec<Sent> = Vec::new();
    let mut link_free = t0;
    let mut feedback: VecDeque<(Instant, Vec<PacketReport>)> = VecDeque::new();
    let mut reported = 0usize;
    let mut next_feedback = t0 + FEEDBACK_EVERY;
    let mut next_frame = t0;
    let mut keyframes = s.keyframes.clone();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut trace = Trace {
        points: Vec::new(),
        lost: 0,
        sent: 0,
    };
    let steps = s.seconds * 1000;
    for step in 0..steps {
        let now = t0 + STEP * step as u32;
        let secs = step as f64 / 1000.0;
        // The encoder.
        if now >= next_frame {
            let target = est.target_bitrate() * encoder;
            let (bytes, gap) = match (s.content)(secs) {
                Content::Busy => ((target / 30.0 / 8.0) as usize, Duration::from_millis(33)),
                Content::Idle => (1500, Duration::from_millis(200)),
            };
            let mut bytes = bytes;
            if keyframes.first().is_some_and(|&(t, _)| secs >= t) {
                bytes += keyframes.remove(0).1;
            }
            for _ in 0..bytes.div_ceil(PACKET) {
                if queue.len() < 4096 {
                    queue.push_back(PACKET);
                }
            }
            next_frame = now + gap;
        }
        // The pacer.
        pacer.refill(now);
        while let Some(&size) = queue.front() {
            let bits = (size * 8) as f64;
            if !pacer.can_release(bits) {
                break;
            }
            pacer.consume(bits);
            queue.pop_front();
            // The bottleneck.
            let cap = (s.capacity)(secs);
            let start = link_free.max(now);
            let exit = start + Duration::from_secs_f64(bits / cap);
            let dropped =
                exit.duration_since(now) > Duration::from_millis(400) || rng.next() < s.loss;
            if !dropped {
                link_free = exit;
            }
            trace.sent += 1;
            trace.lost += dropped as usize;
            sent.push(Sent {
                id: sent.len() as u64,
                size,
                departure: now,
                arrival: (!dropped).then_some(exit + s.one_way),
            });
        }
        // The receiver's feedback: everything up to the newest arrival.
        if now >= next_feedback {
            next_feedback = now + FEEDBACK_EVERY;
            let newest = sent
                .iter()
                .rposition(|p| p.arrival.is_some_and(|a| a <= now));
            if let Some(last) = newest
                && last >= reported
            {
                let reports = sent[reported..=last]
                    .iter()
                    .map(|p| {
                        let arrived = p.arrival.is_some_and(|a| a <= now);
                        PacketReport {
                            ssrc: 1,
                            id: p.id,
                            rtp_sequence_number: p.id as u16,
                            is_twcc: true,
                            twcc_sequence_number: p.id as u16,
                            size: p.size,
                            arrived,
                            departure: p.departure,
                            arrival: p.arrival.filter(|_| arrived).map(|a| a - t0),
                            ecn: Ecn::default(),
                        }
                    })
                    .collect();
                reported = last + 1;
                feedback.push_back((now + s.one_way, reports));
            }
        }
        while feedback.front().is_some_and(|(t, _)| *t <= now) {
            let (_, reports) = feedback.pop_front().expect("checked");
            est.on_reports(now, &reports);
            pacer.set_target_bitrate(est.target_bitrate() * pacing);
        }
        if est.poll_timeout().is_some_and(|t| t <= now) {
            est.handle_timeout(now);
        }
        if step % 100 == 0 {
            let qdelay = link_free.saturating_duration_since(now).as_secs_f64() * 1000.0;
            trace.points.push((secs, est.target_bitrate(), qdelay));
        }
    }
    trace
}

pub fn bwe(start: f64) -> Bwe {
    Bwe::new(start, 300_000.0, 25_000_000.0)
}

/// The estimator 0.1.5 shipped, configured as it was (pacer at the estimate).
pub fn gcc(start: f64) -> Gcc {
    Gcc::new(start, 300_000.0, 25_000_000.0)
}
