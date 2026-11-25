//! Annex-B access units (what the host sends) to the form Apple's media stack takes: NAL
//! units with 4-byte big-endian lengths (AVCC), and the stream headers (SPS/PPS) on their
//! own, for building a `CMVideoFormatDescription`.

const NAL_SLICE: u8 = 1;
const NAL_IDR: u8 = 5;
const NAL_SEI: u8 = 6;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;

/// One access unit, ready for a `CMSampleBuffer`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Avcc {
    /// The picture's NAL units, each prefixed with its length (4 bytes, big-endian).
    pub data: Vec<u8>,
    /// Stream headers carried by this access unit (keyframes carry them).
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
    /// Contains an IDR slice: decodable on its own.
    pub keyframe: bool,
}

/// NAL units of an Annex-B stream (3- or 4-byte start codes), without start codes.
pub fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
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
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|&s| {
            // The next start code begins 3 bytes before its payload, plus a leading zero
            // for 4-byte start codes.
            let mut e = s - 3;
            if e > 0 && data[e - 1] == 0 {
                e -= 1;
            }
            e
        })
        .chain(std::iter::once(data.len()))
        .collect();
    starts
        .into_iter()
        .zip(ends)
        .filter(|(s, e)| e > s)
        .map(move |(s, e)| &data[s..e])
}

/// Convert one Annex-B access unit. `None` if it holds no picture (headers or padding only).
pub fn to_avcc(annex_b: &[u8]) -> Option<Avcc> {
    let mut out = Avcc::default();
    let mut slices = false;
    for nal in nal_units(annex_b) {
        match nal[0] & 0x1f {
            NAL_SPS => out.sps = Some(nal.to_vec()),
            NAL_PPS => out.pps = Some(nal.to_vec()),
            t @ (NAL_SLICE..=NAL_IDR | NAL_SEI) => {
                slices |= t != NAL_SEI;
                out.keyframe |= t == NAL_IDR;
                let len = u32::try_from(nal.len()).ok()?;
                out.data.extend_from_slice(&len.to_be_bytes());
                out.data.extend_from_slice(nal);
            }
            _ => {} // access unit delimiters, filler, end of sequence
        }
    }
    slices.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_keyframe_splits_into_headers_and_length_prefixed_slices() {
        let au = [
            0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x1f, // SPS
            0, 0, 1, 0x68, 0xce, 0x3c, // PPS (3-byte start code)
            0, 0, 0, 1, 0x09, 0xf0, // access unit delimiter: dropped
            0, 0, 0, 1, 0x65, 1, 2, 3, // IDR slice
            0, 0, 0, 1, 0x65, 4, 5, // second slice of the same picture
        ];
        let a = to_avcc(&au).unwrap();
        assert!(a.keyframe);
        assert_eq!(a.sps.as_deref(), Some(&[0x67, 0x42, 0xe0, 0x1f][..]));
        assert_eq!(a.pps.as_deref(), Some(&[0x68, 0xce, 0x3c][..]));
        assert_eq!(a.data, [0, 0, 0, 4, 0x65, 1, 2, 3, 0, 0, 0, 3, 0x65, 4, 5]);
    }

    #[test]
    fn a_p_frame_has_no_headers_and_headers_alone_are_no_picture() {
        let a = to_avcc(&[0, 0, 0, 1, 0x41, 9, 9]).unwrap();
        assert!(!a.keyframe && a.sps.is_none() && a.pps.is_none());
        assert_eq!(a.data, [0, 0, 0, 3, 0x41, 9, 9]);
        assert_eq!(to_avcc(&[0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2]), None);
        assert_eq!(to_avcc(&[]), None);
    }
}
