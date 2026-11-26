//! Video codecs the peers negotiate, and the side data Rconn carries inside its video
//! streams (frame tags, copy instructions) in each codec's own syntax.
//!
//! Negotiation is plain SDP: the viewer offers the codecs it can decode, the host answers
//! with those it can encode and sends the best of them ([`choose`]). Peers from before the
//! codec ladder offer and answer H.264 alone, so they keep getting H.264.
//!
//! Side data is a "user data unregistered" SEI message in H.264 and H.265 (decoders skip
//! it) and a metadata OBU of a private type in AV1. H.264 bytes are exactly what hosts
//! sent before, so older viewers still read them.

/// A video codec of the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    H264,
    H265,
    Av1,
}

/// The environment variable that forces a codec (`h264`, `hevc` or `av1`): on the viewer
/// it limits the offer to H.264 and that codec, on the host it picks that codec whenever
/// the viewer offered it and the host has an encoder for it.
pub const CODEC_ENV: &str = "RCONN_CODEC";

impl VideoCodec {
    /// Best first: the order the host picks in.
    pub const LADDER: [VideoCodec; 3] = [VideoCodec::Av1, VideoCodec::H265, VideoCodec::H264];

    /// The RTP/SDP media type.
    pub fn mime(self) -> &'static str {
        match self {
            VideoCodec::H264 => "video/H264",
            VideoCodec::H265 => "video/H265",
            VideoCodec::Av1 => "video/AV1",
        }
    }

    /// Its name in SDP `rtpmap` lines.
    pub fn rtpmap_name(self) -> &'static str {
        &self.mime()["video/".len()..]
    }

    /// For logs and the stats line.
    pub fn name(self) -> &'static str {
        match self {
            VideoCodec::H264 => "H.264",
            VideoCodec::H265 => "HEVC",
            VideoCodec::Av1 => "AV1",
        }
    }

    pub fn from_mime(mime: &str) -> Option<Self> {
        Self::LADDER
            .into_iter()
            .find(|c| c.mime().eq_ignore_ascii_case(mime))
    }

    /// `h264`/`avc`, `hevc`/`h265`, `av1` (any case).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "h264" | "h.264" | "avc" => Some(VideoCodec::H264),
            "hevc" | "h265" | "h.265" => Some(VideoCodec::H265),
            "av1" => Some(VideoCodec::Av1),
            _ => None,
        }
    }

    /// The codec [`CODEC_ENV`] asks for, if any.
    pub fn from_env() -> Option<Self> {
        Self::parse(&std::env::var(CODEC_ENV).ok()?)
    }

    /// Is this an H.264/H.265 stream (Annex-B NAL units) rather than AV1 (OBUs)?
    pub fn is_nal(self) -> bool {
        !matches!(self, VideoCodec::Av1)
    }
}

/// The codecs a viewer offers: H.264 always, plus those it can decode, limited by a
/// forced codec (`forced`: H.264 and that one only).
pub fn offer(decodable: &[VideoCodec], forced: Option<VideoCodec>) -> Vec<VideoCodec> {
    VideoCodec::LADDER
        .into_iter()
        .filter(|c| {
            *c == VideoCodec::H264 || (decodable.contains(c) && forced.is_none_or(|f| f == *c))
        })
        .collect()
}

/// The codec the host sends: `forced` when the viewer offered it and the host can encode
/// it, else the best codec on both lists. H.264 when nothing else is shared (every viewer
/// decodes it, and it costs the least CPU in software).
pub fn choose(
    offered: &[VideoCodec],
    encodable: &[VideoCodec],
    forced: Option<VideoCodec>,
) -> VideoCodec {
    let both = |c: &VideoCodec| offered.contains(c) && encodable.contains(c);
    if let Some(f) = forced.filter(both) {
        return f;
    }
    VideoCodec::LADDER
        .into_iter()
        .find(|c| *c != VideoCodec::H264 && forced.is_none() && both(c))
        .unwrap_or(VideoCodec::H264)
}

/// One unit of an access unit: a NAL unit (H.264/H.265) or an OBU (AV1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unit {
    /// Where the unit starts in the access unit, start code included.
    pub start: usize,
    /// Where its header starts (after the start code; `start` for AV1).
    pub header: usize,
    pub end: usize,
}

/// What a unit holds, as far as side data cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnitKind {
    /// Picture data: a slice, an AV1 frame or frame header. `first`: it starts a picture.
    Picture {
        first: bool,
    },
    /// SEI (H.264/H.265) or metadata OBU (AV1): where side data lives.
    UserData,
    Other,
}

/// The units of an access unit.
pub fn units(codec: VideoCodec, au: &[u8]) -> Vec<Unit> {
    if codec.is_nal() {
        nal_units(au)
    } else {
        obus(au)
    }
}

fn nal_units(au: &[u8]) -> Vec<Unit> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            let code = if i > 0 && au[i - 1] == 0 { i - 1 } else { i };
            starts.push((code, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends = starts.iter().skip(1).map(|s| s.0).chain([au.len()]);
    starts
        .iter()
        .zip(ends)
        .map(|(&(start, header), end)| Unit { start, header, end })
        .collect()
}

/// AV1 OBU types.
pub mod obu {
    pub const SEQUENCE_HEADER: u8 = 1;
    pub const TEMPORAL_DELIMITER: u8 = 2;
    pub const FRAME_HEADER: u8 = 3;
    pub const TILE_GROUP: u8 = 4;
    pub const METADATA: u8 = 5;
    pub const FRAME: u8 = 6;
    pub const PADDING: u8 = 15;

    /// The type of the OBU whose header byte is `b`.
    pub fn kind(b: u8) -> u8 {
        (b >> 3) & 0x0f
    }
}

/// Read a LEB128 number: (value, bytes it took).
pub fn leb128(data: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for (i, &b) in data.iter().enumerate().take(8) {
        v |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

pub fn put_leb128(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// An OBU's parts: (header length incl. extension, payload range within `data`).
pub fn obu_parts(data: &[u8]) -> Option<(usize, std::ops::Range<usize>)> {
    let h = *data.first()?;
    let mut at = 1 + usize::from(h & 0x04 != 0);
    let header = at;
    let size = if h & 0x02 != 0 {
        let (n, len) = leb128(data.get(at..)?)?;
        at += len;
        usize::try_from(n).ok()?
    } else {
        data.len().checked_sub(at)?
    };
    let end = at.checked_add(size).filter(|&e| e <= data.len())?;
    Some((header, at..end))
}

fn obus(au: &[u8]) -> Vec<Unit> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < au.len() {
        let Some((_, payload)) = obu_parts(&au[at..]) else {
            break;
        };
        let end = at + payload.end;
        out.push(Unit {
            start: at,
            header: at,
            end,
        });
        at = end;
    }
    out
}

/// What `unit` (from [`units`]) holds.
pub fn kind(codec: VideoCodec, au: &[u8], unit: &Unit) -> UnitKind {
    let nal = &au[unit.header..unit.end];
    let Some(&b) = nal.first() else {
        return UnitKind::Other;
    };
    match codec {
        VideoCodec::H264 => match b & 0x1f {
            // first_mb_in_slice == 0: Exp-Golomb "1".
            1 | 5 => UnitKind::Picture {
                first: nal.get(1).is_some_and(|b| b & 0x80 != 0),
            },
            6 => UnitKind::UserData,
            _ => UnitKind::Other,
        },
        VideoCodec::H265 => match (b >> 1) & 0x3f {
            // first_slice_segment_in_pic_flag, after the two-byte header.
            0..=31 => UnitKind::Picture {
                first: nal.get(2).is_some_and(|b| b & 0x80 != 0),
            },
            39 | 40 => UnitKind::UserData,
            _ => UnitKind::Other,
        },
        VideoCodec::Av1 => match obu::kind(b) {
            obu::FRAME | obu::FRAME_HEADER => UnitKind::Picture { first: true },
            obu::METADATA => UnitKind::UserData,
            _ => UnitKind::Other,
        },
    }
}

/// SEI payload type "user data unregistered" (H.264 and H.265).
const SEI_USER_DATA: u8 = 5;
/// AV1 metadata type for Rconn's side data: one of the "unregistered user private" types
/// (6-31), which decoders ignore.
const AV1_METADATA_PRIVATE: u64 = 25;

/// A whole unit (start code included for H.264/H.265) carrying `payload` as user data.
/// `payload` should start with a UUID that names it.
pub fn user_data_unit(codec: VideoCodec, payload: &[u8]) -> Vec<u8> {
    if codec == VideoCodec::Av1 {
        let mut body = Vec::with_capacity(payload.len() + 3);
        put_leb128(AV1_METADATA_PRIVATE, &mut body);
        body.extend_from_slice(payload);
        body.push(0x80); // trailing bits
        let mut out = vec![(obu::METADATA << 3) | 0x02];
        put_leb128(body.len() as u64, &mut out);
        out.extend_from_slice(&body);
        return out;
    }
    let mut rbsp = vec![SEI_USER_DATA];
    let mut n = payload.len();
    while n >= 255 {
        rbsp.push(255);
        n -= 255;
    }
    rbsp.push(n as u8);
    rbsp.extend_from_slice(payload);
    rbsp.push(0x80); // rbsp_trailing_bits
    let mut out = vec![0, 0, 0, 1];
    match codec {
        VideoCodec::H264 => out.push(0x06),
        // Prefix SEI (type 39), layer 0, temporal id 0 (+1).
        _ => out.extend_from_slice(&[39 << 1, 0x01]),
    }
    escape(&rbsp, &mut out);
    out
}

/// The user data payload of `unit`, if it is a user data unit (any payload: the caller
/// checks its UUID).
pub fn user_data(codec: VideoCodec, au: &[u8], unit: &Unit) -> Option<Vec<u8>> {
    if kind(codec, au, unit) != UnitKind::UserData {
        return None;
    }
    let data = &au[unit.header..unit.end];
    if codec == VideoCodec::Av1 {
        let (_, range) = obu_parts(data)?;
        let body = &data[range];
        let (t, n) = leb128(body)?;
        if t != AV1_METADATA_PRIVATE {
            return None;
        }
        let rest = &body[n..];
        // Trailing bits: the last non-zero byte is 0x80.
        let end = rest.iter().rposition(|&b| b != 0)?;
        return (rest[end] == 0x80).then(|| rest[..end].to_vec());
    }
    let header = if codec == VideoCodec::H264 { 1 } else { 2 };
    let rbsp = unescape(data.get(header..)?);
    let mut bytes = rbsp.iter().copied();
    let mut ff = || {
        let mut v = 0usize;
        loop {
            let b = bytes.next()?;
            v += usize::from(b);
            if b != 255 {
                return Some(v);
            }
        }
    };
    let (kind, size) = (ff()?, ff()?);
    let at = rbsp.len() - bytes.len();
    if kind != usize::from(SEI_USER_DATA) {
        return None;
    }
    rbsp.get(at..at + size).map(<[u8]>::to_vec)
}

/// Escape `00 00 0x` (x <= 3) as H.264/H.265 require inside a NAL unit.
fn escape(rbsp: &[u8], out: &mut Vec<u8>) {
    let mut zeros = 0;
    for &b in rbsp {
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

/// `au` with `unit` (from [`user_data_unit`]) in front of its first picture unit, after
/// any parameter sets; at the end if there is no picture.
pub fn insert_before_picture(codec: VideoCodec, mut au: Vec<u8>, unit: &[u8]) -> Vec<u8> {
    let at = units(codec, &au)
        .into_iter()
        .find(|u| matches!(kind(codec, &au, u), UnitKind::Picture { .. }))
        .map_or(au.len(), |u| u.start);
    au.splice(at..at, unit.iter().copied());
    au
}

/// The first user data unit before the first picture whose payload starts with `uuid`:
/// the unit and its payload.
pub fn find_user_data(codec: VideoCodec, au: &[u8], uuid: &[u8; 16]) -> Option<(Unit, Vec<u8>)> {
    for u in units(codec, au) {
        match kind(codec, au, &u) {
            UnitKind::Picture { .. } => return None, // side data comes first
            UnitKind::UserData => {}
            UnitKind::Other => continue,
        }
        if let Some(p) = user_data(codec, au, &u)
            && p.starts_with(uuid)
        {
            return Some((u, p));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use VideoCodec::*;

    #[test]
    fn the_host_picks_the_best_shared_codec() {
        assert_eq!(
            choose(&[H264], &[Av1, H265, H264], None),
            H264,
            "old viewer"
        );
        assert_eq!(choose(&[H264, H265, Av1], &[H264], None), H264, "old host");
        assert_eq!(choose(&[H264, H265, Av1], &[H264, H265], None), H265);
        assert_eq!(choose(&[H264, H265, Av1], &[H264, H265, Av1], None), Av1);
        assert_eq!(choose(&[H264, H265], &[H264, Av1], None), H264);
        // Forced: that codec if possible, else H.264 (not the next best).
        assert_eq!(choose(&[H264, H265, Av1], &[Av1, H265], Some(H265)), H265);
        assert_eq!(choose(&[H264, H265, Av1], &[Av1], Some(H265)), H264);
        assert_eq!(choose(&[H264, H265, Av1], &[Av1, H265], Some(H264)), H264);
    }

    #[test]
    fn the_viewer_offers_what_it_decodes() {
        assert_eq!(offer(&[], None), [H264]);
        assert_eq!(offer(&[H265, Av1, H264], None), [Av1, H265, H264]);
        assert_eq!(offer(&[H265, Av1], Some(H265)), [H265, H264]);
        assert_eq!(offer(&[H265, Av1], Some(H264)), [H264]);
        assert_eq!(VideoCodec::parse("HEVC"), Some(H265));
        assert_eq!(VideoCodec::from_mime("video/h265"), Some(H265));
        assert_eq!(H265.rtpmap_name(), "H265");
    }

    /// The H.264 unit is byte for byte what hosts sent before the codec ladder.
    #[test]
    fn h264_user_data_keeps_its_old_bytes() {
        let unit = user_data_unit(H264, &[0, 0, 1, 7]);
        assert_eq!(unit, [0, 0, 0, 1, 6, 5, 4, 0, 0, 3, 1, 7, 0x80]);
    }

    #[test]
    fn user_data_round_trips_in_every_codec() {
        let uuid = *b"rconn/test-uuid!";
        let mut payload = uuid.to_vec();
        payload.extend([0u8; 300]); // zeros: escaping needed
        let pictures: [(VideoCodec, Vec<u8>); 3] = [
            (H264, vec![0, 0, 0, 1, 0x67, 9, 0, 0, 0, 1, 0x65, 0x88, 1]),
            (
                H265,
                vec![0, 0, 0, 1, 0x40, 1, 9, 0, 0, 0, 1, 0x26, 1, 0xaf, 1],
            ),
            // Temporal delimiter, sequence header (2 bytes), frame (3 bytes).
            (Av1, vec![0x12, 0, 0x0a, 2, 1, 2, 0x32, 3, 0x10, 0, 0]),
        ];
        for (codec, au) in pictures {
            let unit = user_data_unit(codec, &payload);
            let with = insert_before_picture(codec, au.clone(), &unit);
            let (u, back) = find_user_data(codec, &with, &uuid).expect("found");
            assert_eq!(back, payload, "{codec:?}");
            let mut rest = with[..u.start].to_vec();
            rest.extend_from_slice(&with[u.end..]);
            assert_eq!(rest, au, "{codec:?}: removing it gives the unit back");
            let firsts = units(codec, &with)
                .iter()
                .filter(|u| kind(codec, &with, u) == UnitKind::Picture { first: true })
                .count();
            assert_eq!(firsts, 1, "{codec:?}");
            assert!(find_user_data(codec, &au, &uuid).is_none());
        }
    }

    #[test]
    fn leb128_round_trips() {
        for v in [0u64, 1, 127, 128, 300, 1 << 20] {
            let mut b = Vec::new();
            put_leb128(v, &mut b);
            assert_eq!(leb128(&b), Some((v, b.len())));
        }
    }
}
