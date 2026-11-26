//! Loss recovery without keyframes: which long-term reference (LTR) pictures the viewer
//! is known to hold, so a loss is repaired by predicting from one of them instead of
//! sending an IDR (~1 MB for a detailed full-size screen).
//!
//! Engine-agnostic bookkeeping: frames are named by the stream-wide ids the host puts in
//! every access unit; each engine says which of its frames are long-term references
//! (openh264 decides itself and we read it from the bitstream, Media Foundation marks
//! the frames we ask it to). The viewer acknowledges the newest frame it decoded with
//! nothing missing before it; a loss report names the last good one.

use crate::h264::SliceInfo;
use std::collections::VecDeque;

/// What the host tells the viewer about one encoded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameInfo {
    /// Stream-wide frame id (consecutive across encoders and sizes).
    pub id: u32,
    /// An IDR: decodable on its own.
    pub key: bool,
    /// The frame repairs a loss: it predicts only from frame `base` (a long-term
    /// reference the viewer acknowledged) and what it holds from before that.
    pub recovers_from: Option<u32>,
}

/// One frame as sent.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Sent {
    pub id: u32,
    /// The encoder instance it came from; references never cross instances.
    pub instance: u32,
    pub key: bool,
    pub slice: Option<SliceInfo>,
    /// Long-term reference slot (Media Foundation's index; openh264: always 0).
    pub ltr_slot: Option<u8>,
}

/// How many frames are remembered: several seconds at 30 fps, more than any feedback
/// round trip worth recovering from.
const HISTORY: usize = 512;

#[derive(Debug, Default)]
pub(crate) struct Ledger {
    sent: VecDeque<Sent>,
    instance: u32,
    /// Long-term references the viewer acknowledged, newest last (this instance only).
    confirmed: Vec<Sent>,
    /// The newest frame the viewer acknowledged.
    acked: Option<u32>,
    /// Frames the viewer reported lost or broken, `(after, up_to]`: never confirmed by
    /// a later acknowledgement, which only covers the frames that repaired them.
    tainted: VecDeque<(u32, u32)>,
}

impl Ledger {
    /// A new encoder instance (new size, new capture, restart): nothing before it can be
    /// referenced.
    pub fn new_instance(&mut self) {
        self.instance = self.instance.wrapping_add(1);
        self.confirmed.clear();
    }

    pub fn instance(&self) -> u32 {
        self.instance
    }

    pub fn record(&mut self, s: Sent) {
        if s.key {
            // An IDR empties the reference buffers on both sides.
            self.confirmed.clear();
        }
        if self.sent.len() == HISTORY {
            self.sent.pop_front();
        }
        self.sent.push_back(s);
    }

    pub fn get(&self, id: u32) -> Option<&Sent> {
        // Ids are consecutive: index from the back.
        let newest = self.sent.back()?.id;
        let back = newest.checked_sub(id)? as usize;
        let i = self.sent.len().checked_sub(1 + back)?;
        self.sent.get(i).filter(|s| s.id == id)
    }

    pub fn newest(&self) -> Option<&Sent> {
        self.sent.back()
    }

    /// The viewer decoded every frame up to `id` intact. Returns the long-term references
    /// this newly confirms (oldest first).
    pub fn ack(&mut self, id: u32) -> Vec<Sent> {
        let from = self.acked.map_or(0, |a| a.wrapping_add(1));
        if self.acked.is_some_and(|a| a >= id) {
            return Vec::new();
        }
        self.acked = Some(id);
        let instance = self.instance;
        let tainted = &self.tainted;
        let fresh: Vec<Sent> = self
            .sent
            .iter()
            .filter(|s| s.id >= from && s.id <= id && s.instance == instance)
            .filter(|s| s.ltr_slot.is_some())
            .filter(|s| !tainted.iter().any(|&(a, b)| s.id > a && s.id <= b))
            .copied()
            .collect();
        for s in &fresh {
            // A slot holds one picture: a newer one in the same slot replaces it.
            self.confirmed.retain(|c| c.ltr_slot != s.ltr_slot);
            self.confirmed.push(*s);
        }
        fresh
    }

    /// The newest confirmed long-term reference at or before `last_good`, from the
    /// current instance: what a repair may predict from.
    pub fn base_for(&self, last_good: u32) -> Option<Sent> {
        self.confirmed
            .iter()
            .rev()
            .find(|c| c.id <= last_good && c.instance == self.instance)
            .copied()
    }

    /// A slot that holds no confirmed reference, or the one holding the oldest: where the
    /// next mark goes, never over the newest confirmed picture.
    #[cfg_attr(not(windows), allow(dead_code))] // Media Foundation marks slots itself
    pub fn free_slot(&self, slots: u8) -> u8 {
        (0..slots)
            .find(|&s| !self.confirmed.iter().any(|c| c.ltr_slot == Some(s)))
            .unwrap_or_else(|| self.confirmed.first().and_then(|c| c.ltr_slot).unwrap_or(0))
    }

    /// The viewer lost frames after `last_good`: everything sent since is suspect.
    pub fn loss_after(&mut self, last_good: u32) {
        if self.acked.is_some_and(|a| a > last_good) {
            self.acked = Some(last_good);
        }
        self.confirmed.retain(|c| c.id <= last_good);
        if let Some(newest) = self.sent.back().map(|s| s.id)
            && newest > last_good
        {
            if self.tainted.len() == 8 {
                self.tainted.pop_front();
            }
            self.tainted.push_back((last_good, newest));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(id: u32, key: bool, slot: Option<u8>) -> Sent {
        Sent {
            id,
            instance: 1,
            key,
            slice: None,
            ltr_slot: slot,
        }
    }

    #[test]
    fn acknowledged_references_become_bases() {
        let mut l = Ledger::default();
        l.new_instance();
        l.record(sent(10, true, Some(0)));
        for id in 11..20 {
            l.record(sent(id, false, (id == 15).then_some(1)));
        }
        assert!(l.base_for(19).is_none(), "nothing acknowledged yet");
        assert_eq!(l.ack(12).len(), 1, "the keyframe");
        assert_eq!(l.base_for(19).map(|s| s.id), Some(10));
        assert_eq!(l.ack(16).len(), 1, "frame 15");
        assert_eq!(l.base_for(19).map(|s| s.id), Some(15));
        assert_eq!(l.base_for(14).map(|s| s.id), Some(10), "older loss");
        assert_eq!(l.free_slot(2), 0, "never over the newest confirmed");
        l.new_instance();
        assert!(l.base_for(19).is_none(), "a new encoder holds none of them");
    }

    #[test]
    fn references_in_a_lost_stretch_are_never_confirmed() {
        let mut l = Ledger::default();
        l.new_instance();
        l.record(sent(0, true, Some(0)));
        l.ack(0);
        for id in 1..10 {
            l.record(sent(id, false, (id == 5).then_some(1)));
        }
        l.loss_after(3); // 4.. lost or broken, 5 among them
        l.record(sent(10, false, None)); // the repair
        l.ack(10);
        assert_eq!(l.base_for(10).map(|s| s.id), Some(0), "not frame 5");
    }

    #[test]
    fn frames_are_found_by_id() {
        let mut l = Ledger::default();
        for id in 100..700 {
            l.record(sent(id, false, None));
        }
        assert_eq!(l.get(650).map(|s| s.id), Some(650));
        assert!(l.get(100).is_none(), "fell out of the history");
        assert!(l.get(800).is_none());
    }
}
