//! RTP depacketizing for each video codec, behind one type the viewer's reassembly uses.
//!
//! * H.264: `rtc`'s [`H264Packet`].
//! * H.265 (RFC 7798): [`H265Depacketizer`] here. `rtc` has a packetizer (`HevcPayloader`,
//!   used by the host's track) and parses the three packet shapes, but its depacketizer
//!   hands the payload back unchanged instead of NAL units. This one turns single NAL
//!   unit packets, aggregation packets and fragmentation units back into Annex-B. No
//!   DONL (the stream never reorders: `sprop-max-don-diff` is 0); PACI packets are
//!   dropped (nobody sends them).
//! * AV1: `rtc`'s `Av1Depacketizer` (the AV1 RTP specification's OBU aggregation), which
//!   gives back OBUs with size fields (the low-overhead bitstream format), temporal
//!   delimiters left out.

use bytes::{BufMut, Bytes, BytesMut};
use rconn_core::codec::VideoCodec;
use rtc::rtp::codec::av1::Av1Depacketizer;
use rtc::rtp::codec::h264::H264Packet;
use rtc::rtp::packetizer::Depacketizer;
use rtc::shared::error::{Error, Result};

/// RFC 7798 payload header types.
const AP: u8 = 48;
const FU: u8 = 49;
const PACI: u8 = 50;

/// RTP payloads (RFC 7798) back to Annex-B NAL units, 4-byte start codes.
#[derive(Default, Debug, Clone)]
pub struct H265Depacketizer {
    /// The NAL unit being reassembled from fragmentation units (start code included).
    fragment: Option<BytesMut>,
}

fn nal_type(header: &[u8]) -> u8 {
    (header[0] >> 1) & 0x3f
}

impl Depacketizer for H265Depacketizer {
    fn depacketize(&mut self, payload: &Bytes) -> Result<Bytes> {
        if payload.len() < 3 {
            return Err(Error::ErrShortPacket);
        }
        if payload[0] & 0x80 != 0 {
            return Err(Error::ErrH265CorruptedPacket); // forbidden_zero_bit
        }
        let mut out = BytesMut::new();
        match nal_type(payload) {
            AP => {
                self.fragment = None;
                let mut rest = &payload[2..];
                while rest.len() >= 2 {
                    let n = usize::from(u16::from_be_bytes([rest[0], rest[1]]));
                    let unit = rest.get(2..2 + n).ok_or(Error::ErrShortPacket)?;
                    if n >= 2 {
                        out.put_slice(&[0, 0, 0, 1]);
                        out.put_slice(unit);
                    }
                    rest = &rest[2 + n..];
                }
            }
            FU => {
                let fu = payload[2];
                let (start, end) = (fu & 0x80 != 0, fu & 0x40 != 0);
                if start {
                    // The NAL header again: the FU's type, the payload header's F, layer
                    // and temporal id.
                    let mut nal = BytesMut::with_capacity(payload.len() * 8);
                    nal.put_slice(&[0, 0, 0, 1]);
                    nal.put_u8((payload[0] & 0x81) | ((fu & 0x3f) << 1));
                    nal.put_u8(payload[1]);
                    self.fragment = Some(nal);
                }
                // A fragment whose start was lost is dropped with the rest of its unit.
                let Some(nal) = self.fragment.as_mut() else {
                    return Ok(Bytes::new());
                };
                nal.put_slice(&payload[3..]);
                if end {
                    out = self.fragment.take().expect("checked above");
                }
            }
            PACI => self.fragment = None,
            _ => {
                self.fragment = None;
                out.put_slice(&[0, 0, 0, 1]);
                out.put_slice(payload);
            }
        }
        Ok(out.freeze())
    }

    /// A packet that starts a NAL unit: anything but a fragment after the first.
    fn is_partition_head(&self, payload: &Bytes) -> bool {
        payload.len() >= 3 && (nal_type(payload) != FU || payload[2] & 0x80 != 0)
    }

    fn is_partition_tail(&self, marker: bool, _payload: &Bytes) -> bool {
        marker
    }
}

/// The depacketizer of the stream's codec.
#[derive(Debug, Clone)]
pub enum CodecDepacketizer {
    H264(H264Packet),
    H265(H265Depacketizer),
    Av1(Av1Depacketizer),
}

impl CodecDepacketizer {
    pub fn new(codec: VideoCodec) -> Self {
        match codec {
            VideoCodec::H264 => Self::H264(H264Packet::default()),
            VideoCodec::H265 => Self::H265(H265Depacketizer::default()),
            VideoCodec::Av1 => Self::Av1(Av1Depacketizer::new()),
        }
    }

    pub fn codec(&self) -> VideoCodec {
        match self {
            Self::H264(_) => VideoCodec::H264,
            Self::H265(_) => VideoCodec::H265,
            Self::Av1(_) => VideoCodec::Av1,
        }
    }
}

impl Depacketizer for CodecDepacketizer {
    fn depacketize(&mut self, b: &Bytes) -> Result<Bytes> {
        match self {
            Self::H264(d) => d.depacketize(b),
            Self::H265(d) => d.depacketize(b),
            Self::Av1(d) => d.depacketize(b),
        }
    }

    fn is_partition_head(&self, payload: &Bytes) -> bool {
        match self {
            Self::H264(d) => d.is_partition_head(payload),
            Self::H265(d) => d.is_partition_head(payload),
            Self::Av1(d) => d.is_partition_head(payload),
        }
    }

    fn is_partition_tail(&self, marker: bool, payload: &Bytes) -> bool {
        match self {
            Self::H264(d) => d.is_partition_tail(marker, payload),
            Self::H265(d) => d.is_partition_tail(marker, payload),
            Self::Av1(d) => d.is_partition_tail(marker, payload),
        }
    }
}

/// `au` with every start code four bytes long. `rtc`'s HEVC packetizer splits on 4-byte
/// start codes if there is any (else on 3-byte ones), so a mix would glue NAL units
/// together; some encoders write both.
pub fn four_byte_start_codes(au: Vec<u8>) -> Vec<u8> {
    let short = |i: usize| au[i..i + 3] == [0, 0, 1] && (i == 0 || au[i - 1] != 0);
    if !(0..au.len().saturating_sub(2)).any(short) {
        return au;
    }
    let mut out = Vec::with_capacity(au.len() + 64);
    let mut i = 0;
    while i < au.len() {
        if i + 3 <= au.len() && short(i) {
            out.extend_from_slice(&[0, 0, 0, 1]);
            i += 3;
        } else {
            out.push(au[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtc::rtp::codec::av1::Av1Payloader;
    use rtc::rtp::codec::h265::HevcPayloader;
    use rtc::rtp::packetizer::Payloader;

    /// An HEVC keyframe: VPS, SPS, PPS (aggregated), a prefix SEI, and an IDR slice
    /// big enough to be fragmented.
    fn hevc_keyframe(slice_len: usize) -> Vec<u8> {
        let mut au = Vec::new();
        for nal in [
            &[0x40, 0x01, 0x0c, 0x01, 0xff][..],
            &[0x42, 0x01, 0x01, 0x01, 0x60],
            &[0x44, 0x01, 0xc1, 0x72],
            &[0x4e, 0x01, 0x05, 0x02, 0x11, 0x22, 0x80],
        ] {
            au.extend_from_slice(&[0, 0, 0, 1]);
            au.extend_from_slice(nal);
        }
        au.extend_from_slice(&[0, 0, 0, 1, 0x26, 0x01, 0xaf]);
        au.extend((0..slice_len).map(|i| (i % 251) as u8 | 1));
        au
    }

    fn payloads(codec: VideoCodec, au: &[u8]) -> Vec<Bytes> {
        let au = Bytes::copy_from_slice(au);
        match codec {
            VideoCodec::H265 => HevcPayloader.payload(1200, &au),
            VideoCodec::Av1 => Av1Payloader {}.payload(1200, &au),
            VideoCodec::H264 => unreachable!(),
        }
        .expect("payload")
    }

    fn depacketize(codec: VideoCodec, payloads: &[Bytes]) -> Vec<u8> {
        let mut d = CodecDepacketizer::new(codec);
        let mut out = Vec::new();
        for p in payloads {
            out.extend_from_slice(&d.depacketize(p).expect("depacketize"));
        }
        out
    }

    #[test]
    fn hevc_round_trips_single_aggregated_and_fragmented_units() {
        for slice in [10, 1500, 20_000] {
            let au = hevc_keyframe(slice);
            let p = payloads(VideoCodec::H265, &au);
            assert_eq!(nal_type(&p[0]), AP, "parameter sets and SEI aggregated");
            if slice > 1200 {
                assert!(p[1..].iter().all(|p| nal_type(p) == FU), "slice fragmented");
            }
            let d = CodecDepacketizer::new(VideoCodec::H265);
            assert!(p.iter().filter(|x| d.is_partition_head(x)).count() == 2 || slice <= 1200);
            assert_eq!(depacketize(VideoCodec::H265, &p), au, "{slice}-byte slice");
        }
        // A NAL unit alone in its packet.
        let single = [0, 0, 0, 1, 0x02, 0x01, 0xd0, 7, 7, 7];
        let p = payloads(VideoCodec::H265, &single);
        assert_eq!(p.len(), 1);
        assert_eq!(depacketize(VideoCodec::H265, &p), single);
    }

    #[test]
    fn hevc_drops_a_unit_whose_first_fragment_was_lost() {
        // (A middle fragment lost shows as a sequence gap, which the receiver's frame
        // assembly catches: `video::receiver_tests`.)
        let au = hevc_keyframe(5000);
        let mut p = payloads(VideoCodec::H265, &au);
        p.remove(1);
        let mut next = payloads(VideoCodec::H265, &[0, 0, 0, 1, 0x02, 0x01, 0xd0, 9]);
        p.append(&mut next);
        let out = depacketize(VideoCodec::H265, &p);
        assert!(out.ends_with(&[0, 0, 0, 1, 0x02, 0x01, 0xd0, 9]));
        assert!(out.len() < 200);
    }

    #[test]
    fn mixed_start_codes_become_four_bytes() {
        let au = vec![
            0, 0, 1, 0x40, 1, 0, 0, 0, 1, 0x42, 1, 0, 0, 1, 0x26, 1, 0xaf,
        ];
        let fixed = four_byte_start_codes(au);
        assert_eq!(
            fixed,
            [
                0, 0, 0, 1, 0x40, 1, 0, 0, 0, 1, 0x42, 1, 0, 0, 0, 1, 0x26, 1, 0xaf
            ]
        );
        let p = payloads(VideoCodec::H265, &fixed);
        assert_eq!(depacketize(VideoCodec::H265, &p), fixed);
    }

    /// A temporal unit: temporal delimiter, sequence header, metadata, a big frame.
    fn av1_unit(frame_len: usize) -> Vec<u8> {
        let mut au = vec![0x12, 0x00, 0x0a, 3, 0, 0, 0x2a];
        au.extend_from_slice(&[0x2a, 3, 25, 7, 0x80]); // metadata OBU
        au.push(0x32);
        rconn_core::codec::put_leb128(frame_len as u64, &mut au);
        au.extend((0..frame_len).map(|i| (i % 253) as u8));
        au
    }

    #[test]
    fn av1_round_trips_and_a_loss_costs_only_its_obu() {
        for len in [5, 3000, 40_000] {
            let au = av1_unit(len);
            let p = payloads(VideoCodec::Av1, &au);
            // The temporal delimiter is not sent; the rest comes back as it was.
            assert_eq!(
                depacketize(VideoCodec::Av1, &p),
                au[2..],
                "{len}-byte frame"
            );
            let d = CodecDepacketizer::new(VideoCodec::Av1);
            assert!(d.is_partition_head(&p[0]));
            assert!(p.len() == 1 || !d.is_partition_head(&p[1]));
        }
        let au = av1_unit(10_000);
        let mut p = payloads(VideoCodec::Av1, &au);
        p.remove(3);
        let mut d = CodecDepacketizer::new(VideoCodec::Av1);
        let out: Vec<u8> = p
            .iter()
            .flat_map(|x| d.depacketize(x).unwrap_or_default().to_vec())
            .collect();
        assert!(
            out.len() < au.len() - 1000,
            "the broken frame is not handed out whole"
        );
    }
}
