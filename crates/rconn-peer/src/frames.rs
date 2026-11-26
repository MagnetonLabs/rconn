//! Frame identity for loss repair and timing: the host puts a small tag in every H.264
//! access unit (an SEI "user data unregistered" message, which decoders ignore), the
//! viewer reads it, follows whether its picture is intact, and answers with
//! [`Feedback`] (sent as `Control::FrameAck`).
//!
//! With ids the viewer knows exactly which frames it missed (a gap) and which repair
//! frame makes its picture whole again, so the host can repair a loss from a long-term
//! reference instead of a keyframe; the tag also carries the host's capture-to-encoded
//! time for the stats line. Hosts that predate it send no tag; viewers that predate it
//! ignore it.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Identifies our SEI payload.
const UUID: [u8; 16] = *b"rconn/frame-tag1";
const VERSION: u8 = 1;
const PAYLOAD_LEN: usize = 16 + 1 + 4 + 1 + 4 + 4;

/// What the host says about one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameTag {
    /// Stream-wide, consecutive.
    pub id: u32,
    /// An IDR: decodable on its own.
    pub key: bool,
    /// A loss repair predicting only from this earlier frame (and what came before it).
    pub recovers_from: Option<u32>,
    /// Host time from capture to encoded, microseconds.
    pub host_us: u32,
}

/// Escape `00 00 0x` (x <= 3) as H.264 requires inside a NAL unit.
fn escape(payload: &[u8], out: &mut Vec<u8>) {
    let mut zeros = 0;
    for &b in payload {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
}

fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Start offsets of the NAL units in an Annex-B unit (just after each start code).
fn nal_starts(data: &[u8]) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
}

/// Put `tag` into `au`, just before its first slice.
pub fn tag(au: Vec<u8>, tag: &FrameTag) -> Vec<u8> {
    let mut payload = Vec::with_capacity(PAYLOAD_LEN);
    payload.extend_from_slice(&UUID);
    payload.push(VERSION);
    payload.extend_from_slice(&tag.id.to_be_bytes());
    payload.push(u8::from(tag.key) | (u8::from(tag.recovers_from.is_some()) << 1));
    payload.extend_from_slice(&tag.recovers_from.unwrap_or(0).to_be_bytes());
    payload.extend_from_slice(&tag.host_us.to_be_bytes());
    let mut sei = vec![0, 0, 0, 1];
    let mut body = vec![5, PAYLOAD_LEN as u8]; // user_data_unregistered, size
    body.extend_from_slice(&payload);
    body.push(0x80); // rbsp trailing bits
    sei.push(0x06);
    escape(&body, &mut sei);
    // Before the first slice (type 1 or 5), after any parameter sets.
    let at = nal_starts(&au)
        .into_iter()
        .find(|&s| matches!(au.get(s).map(|b| b & 0x1f), Some(1 | 5)))
        .map(|s| {
            // Back over the start code, and a fourth zero byte if there is one.
            let mut i = s - 3;
            if i > 0 && au[i - 1] == 0 {
                i -= 1;
            }
            i
        })
        .unwrap_or(0);
    let mut out = Vec::with_capacity(au.len() + sei.len());
    out.extend_from_slice(&au[..at]);
    out.extend_from_slice(&sei);
    out.extend_from_slice(&au[at..]);
    out
}

/// The tag in `au`, if the host put one there.
pub fn read(au: &[u8]) -> Option<FrameTag> {
    for s in nal_starts(au) {
        match au.get(s).map(|b| b & 0x1f) {
            Some(6) => {}
            Some(1 | 5) => return None, // slices: no tag before them
            _ => continue,
        }
        let end = (s + 3 + 2 * PAYLOAD_LEN).min(au.len());
        let body = unescape(&au[s + 1..end]);
        if body.len() < 2 + PAYLOAD_LEN
            || body[0] != 5
            || usize::from(body[1]) != PAYLOAD_LEN
            || body[2..18] != UUID
            || body[18] != VERSION
        {
            continue;
        }
        let p = &body[19..];
        let u32_at = |i: usize| u32::from_be_bytes([p[i], p[i + 1], p[i + 2], p[i + 3]]);
        let flags = p[4];
        return Some(FrameTag {
            id: u32_at(0),
            key: flags & 1 != 0,
            recovers_from: (flags & 2 != 0).then(|| u32_at(5)),
            host_us: u32_at(9),
        });
    }
    None
}

/// What the viewer tells the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Feedback {
    /// The newest frame decoded with every frame before it intact.
    pub decoded: u32,
    /// Frames after `decoded` are missing or broken: repair, please.
    pub lost: bool,
}

/// Acknowledgements at most this often (each is a few bytes on the data channel).
const ACK_EVERY: Duration = Duration::from_millis(100);
/// A loss report is repeated after this, doubling, while the picture stays broken.
const REPORT_AGAIN: Duration = Duration::from_millis(300);

/// Whether a frame should go to the decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Decode,
    /// It builds on something missing: decoding it shows garbage, and a decoder fed
    /// broken frames may refuse the repair after them too.
    Skip,
}

/// The viewer's view of its own picture: intact or broken since a gap, and what to tell
/// the host about it.
#[derive(Debug, Default)]
pub struct Integrity {
    last_id: Option<u32>,
    broken: bool,
    /// Runs of frames decoded intact, `(first, last)`, newest last.
    intact: VecDeque<(u32, u32)>,
    acked: Option<u32>,
    last_ack: Option<Instant>,
    last_report: Option<Instant>,
    reports: u32,
}

impl Integrity {
    /// Has the host tagged its frames (so it understands [`Feedback`])?
    pub fn active(&self) -> bool {
        self.last_id.is_some()
    }

    /// Is the picture known to be broken (waiting for a repair)?
    pub fn broken(&self) -> bool {
        self.broken
    }

    fn newest_intact(&self) -> Option<u32> {
        self.intact.back().map(|&(_, b)| b)
    }

    fn is_intact(&self, id: u32) -> bool {
        self.intact.iter().any(|&(a, b)| id >= a && id <= b)
    }

    fn start_run(&mut self, id: u32) {
        if self.intact.len() == 16 {
            self.intact.pop_front();
        }
        self.intact.push_back((id, id));
        self.broken = false;
        self.reports = 0;
        self.last_report = None;
    }

    fn breaks(&mut self) {
        if !self.broken {
            self.broken = true;
            self.reports = 0;
            self.last_report = None;
        }
    }

    /// A tagged frame arrived.
    pub fn frame(&mut self, tag: &FrameTag) -> Verdict {
        // Older than one already seen (a late retransmission after a repair was asked
        // for): too late to show, and no news.
        if self
            .last_id
            .is_some_and(|l| tag.id.wrapping_sub(l).wrapping_sub(1) > u32::MAX / 2)
        {
            return Verdict::Skip;
        }
        let follows = self.last_id.is_some_and(|l| tag.id == l.wrapping_add(1));
        self.last_id = Some(tag.id);
        if tag.key {
            self.start_run(tag.id);
            return Verdict::Decode;
        }
        if self.broken {
            if tag.recovers_from.is_some_and(|b| self.is_intact(b)) {
                self.start_run(tag.id);
                return Verdict::Decode;
            }
            return Verdict::Skip;
        }
        if follows && let Some(run) = self.intact.back_mut() {
            run.1 = tag.id;
            return Verdict::Decode;
        }
        // A gap: frames between went missing (or the stream started without a keyframe).
        self.breaks();
        Verdict::Skip
    }

    /// A frame without a tag from a host that tags them: its first packets (the tag's)
    /// were lost.
    pub fn untagged(&mut self) {
        if self.active() {
            self.breaks();
        }
    }

    /// The decoder could not make the frame `id` (it was handed as intact).
    pub fn decode_failed(&mut self, id: u32) {
        if let Some(run) = self.intact.back_mut()
            && run.1 == id
        {
            if run.0 == id {
                self.intact.pop_back();
            } else {
                run.1 = id.wrapping_sub(1);
            }
        }
        self.breaks();
    }

    /// What to send now, if anything. Call after every frame and on a timer.
    pub fn poll(&mut self, now: Instant) -> Option<Feedback> {
        let decoded = self.newest_intact()?;
        if self.broken {
            let gap = REPORT_AGAIN * 2u32.saturating_pow(self.reports.min(4));
            if self
                .last_report
                .is_some_and(|t| now.saturating_duration_since(t) < gap)
            {
                return None;
            }
            if self.last_report.is_some() {
                self.reports += 1;
            }
            self.last_report = Some(now);
            return Some(Feedback {
                decoded,
                lost: true,
            });
        }
        let fresh = self.acked.is_none_or(|a| a != decoded);
        let due = self
            .last_ack
            .is_none_or(|t| now.saturating_duration_since(t) >= ACK_EVERY);
        if fresh && due {
            self.acked = Some(decoded);
            self.last_ack = Some(now);
            return Some(Feedback {
                decoded,
                lost: false,
            });
        }
        None
    }
}

/// The viewer's per-stage timing: host (capture to encoded, from the tags), receive
/// (first to last packet of a frame), decode. Averaged per second for the stats line,
/// and logged every 5 s.
#[derive(Debug, Default)]
pub struct StageTimes {
    second: Sums,
    log: Sums,
    log_since: Option<Instant>,
}

#[derive(Debug, Default, Clone, Copy)]
struct Sums {
    host_us: u64,
    hosts: u32,
    receive: Duration,
    receives: u32,
    decode: Duration,
    decodes: u32,
}

impl Sums {
    fn ms(total_us: f64, n: u32) -> f32 {
        (total_us / 1000.0 / f64::from(n.max(1))) as f32
    }
    fn host_ms(&self) -> Option<f32> {
        (self.hosts > 0).then(|| Self::ms(self.host_us as f64, self.hosts))
    }
    fn receive_ms(&self) -> f32 {
        Self::ms(self.receive.as_micros() as f64, self.receives)
    }
    fn decode_ms(&self) -> f32 {
        Self::ms(self.decode.as_micros() as f64, self.decodes)
    }
}

impl StageTimes {
    pub fn host(&mut self, us: u32) {
        for s in [&mut self.second, &mut self.log] {
            s.host_us += u64::from(us);
            s.hosts += 1;
        }
    }

    pub fn received(&mut self, d: Duration) {
        for s in [&mut self.second, &mut self.log] {
            s.receive += d;
            s.receives += 1;
        }
    }

    pub fn decoded(&mut self, d: Duration) {
        for s in [&mut self.second, &mut self.log] {
            s.decode += d;
            s.decodes += 1;
        }
        let since = *self.log_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= Duration::from_secs(5) {
            let l = std::mem::take(&mut self.log);
            tracing::info!(
                host_ms = l.host_ms().map(|m| format!("{m:.1}")),
                receive_ms = format!("{:.1}", l.receive_ms()),
                decode_ms = format!("{:.1}", l.decode_ms()),
                frames = l.decodes,
                "video stages"
            );
            self.log_since = Some(Instant::now());
        }
    }

    /// (host, receive) ms per frame over the last second; starts the next second.
    pub fn take_second(&mut self) -> (Option<f32>, f32) {
        let s = std::mem::take(&mut self.second);
        (s.host_ms(), s.receive_ms())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_round_trip_and_land_before_the_slice() {
        let au = vec![
            0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 1, 0x65, 9, 9,
        ];
        let t = FrameTag {
            id: 0x0000_0102, // escaping needed: 00 00 01 inside
            key: true,
            recovers_from: Some(3),
            host_us: 4500,
        };
        let tagged = tag(au.clone(), &t);
        assert_eq!(read(&tagged), Some(t));
        let types: Vec<u8> = nal_starts(&tagged)
            .iter()
            .map(|&s| tagged[s] & 0x1f)
            .collect();
        assert_eq!(types, [7, 8, 6, 5], "SPS, PPS, tag, slice");
        assert_eq!(read(&au), None, "untagged");
        assert!(tagged.ends_with(&[0, 0, 1, 0x65, 9, 9]));
    }

    fn t(id: u32) -> FrameTag {
        FrameTag {
            id,
            ..FrameTag::default()
        }
    }

    #[test]
    fn a_gap_is_reported_and_a_repair_heals_it() {
        let now = Instant::now();
        let mut g = Integrity::default();
        assert_eq!(g.frame(&FrameTag { key: true, ..t(10) }), Verdict::Decode);
        for id in 11..=15 {
            assert_eq!(g.frame(&t(id)), Verdict::Decode);
        }
        assert_eq!(
            g.poll(now),
            Some(Feedback {
                decoded: 15,
                lost: false
            })
        );
        // 16 lost; 17 builds on it.
        assert_eq!(g.frame(&t(17)), Verdict::Skip);
        assert_eq!(
            g.poll(now),
            Some(Feedback {
                decoded: 15,
                lost: true
            })
        );
        assert_eq!(
            g.poll(now + Duration::from_millis(100)),
            None,
            "not again yet"
        );
        assert!(g.poll(now + Duration::from_millis(300)).is_some(), "again");
        assert_eq!(g.frame(&t(18)), Verdict::Skip);
        assert_eq!(
            g.frame(&t(16)),
            Verdict::Skip,
            "late retransmission: too late"
        );
        // A repair from a frame we never had does not heal.
        let bad = FrameTag {
            recovers_from: Some(16),
            ..t(19)
        };
        assert_eq!(g.frame(&bad), Verdict::Skip);
        let good = FrameTag {
            recovers_from: Some(12),
            ..t(20)
        };
        assert_eq!(g.frame(&good), Verdict::Decode);
        assert_eq!(g.frame(&t(21)), Verdict::Decode);
        assert_eq!(
            g.poll(now + Duration::from_secs(1)),
            Some(Feedback {
                decoded: 21,
                lost: false
            })
        );
    }

    #[test]
    fn a_decode_error_breaks_the_picture() {
        let mut g = Integrity::default();
        g.frame(&FrameTag { key: true, ..t(0) });
        g.frame(&t(1));
        g.decode_failed(1);
        assert!(g.broken());
        assert_eq!(
            g.poll(Instant::now()),
            Some(Feedback {
                decoded: 0,
                lost: true
            })
        );
    }
}
