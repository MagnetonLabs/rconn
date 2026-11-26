//! The stream headers (SPS and PPS) for the VA-API encoder's settings, for drivers that
//! take them packed (Intel's, like ffmpeg and Chromium hand them): the driver then
//! writes only the slice headers, from the same settings ([`super::encode`]).
//!
//! The SPS adds what the drivers' own leave out: VUI bitstream restrictions saying no
//! frame is ever reordered, so hardware decoders show each frame as soon as it arrives.

/// What the headers describe; the encoder's parameter buffers say the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Settings {
    /// 66 (Constrained Baseline) or 77 (Main).
    pub profile_idc: u8,
    pub level_idc: u8,
    pub mb_w: u32,
    pub mb_h: u32,
    /// Frame cropping in 4:2:0 units (2 pixels).
    pub crop_right: u32,
    pub crop_bottom: u32,
    pub log2_max_frame_num: u32,
    pub max_num_ref_frames: u32,
    pub pic_init_qp: i32,
}

/// Exp-Golomb bit writer.
#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn u(&mut self, bits: u32, v: u64) {
        for i in (0..bits).rev() {
            self.acc = (self.acc << 1) | ((v >> i) & 1);
            self.n += 1;
            if self.n == 8 {
                self.bytes.push(self.acc as u8);
                (self.acc, self.n) = (0, 0);
            }
        }
    }

    fn flag(&mut self, b: bool) {
        self.u(1, u64::from(b));
    }

    fn ue(&mut self, v: u32) {
        let v = u64::from(v) + 1;
        let len = 64 - v.leading_zeros();
        self.u(len - 1, 0);
        self.u(len, v);
    }

    fn se(&mut self, v: i32) {
        let k = if v > 0 {
            2 * v as u32 - 1
        } else {
            2 * v.unsigned_abs()
        };
        self.ue(k);
    }

    #[cfg(test)]
    fn aligned(&self) -> bool {
        self.n == 0
    }

    /// rbsp_trailing_bits: a stop bit, then zeros to the byte.
    fn finish(mut self) -> Vec<u8> {
        self.u(1, 1);
        while self.n != 0 {
            self.u(1, 0);
        }
        self.bytes
    }
}

/// A NAL unit with a 4-byte start code: header byte, then `rbsp` with emulation
/// prevention bytes.
fn nal(nal_ref_idc: u8, nal_type: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut out = vec![0, 0, 0, 1, (nal_ref_idc << 5) | nal_type];
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

pub(super) fn sps(s: &Settings) -> Vec<u8> {
    let mut b = Bits::default();
    b.u(8, u64::from(s.profile_idc));
    // constraint_set0 (Baseline) and set1 (Main) for Constrained Baseline; set1 for Main.
    let constraints: u64 = if s.profile_idc == 66 { 0xc0 } else { 0x40 };
    b.u(8, constraints);
    b.u(8, u64::from(s.level_idc));
    b.ue(0); // seq_parameter_set_id
    b.ue(s.log2_max_frame_num - 4);
    b.ue(2); // pic_order_cnt_type: output order is decode order
    b.ue(s.max_num_ref_frames);
    b.flag(false); // gaps_in_frame_num_value_allowed_flag
    b.ue(s.mb_w - 1);
    b.ue(s.mb_h - 1);
    b.flag(true); // frame_mbs_only_flag
    b.flag(true); // direct_8x8_inference_flag
    let crop = s.crop_right > 0 || s.crop_bottom > 0;
    b.flag(crop);
    if crop {
        b.ue(0);
        b.ue(s.crop_right);
        b.ue(0);
        b.ue(s.crop_bottom);
    }
    b.flag(true); // vui_parameters_present_flag
    b.flag(false); // aspect_ratio_info_present_flag
    b.flag(false); // overscan_info_present_flag
    b.flag(false); // video_signal_type_present_flag (untagged: BT.601 limited)
    b.flag(false); // chroma_loc_info_present_flag
    b.flag(false); // timing_info_present_flag
    b.flag(false); // nal_hrd_parameters_present_flag
    b.flag(false); // vcl_hrd_parameters_present_flag
    b.flag(false); // pic_struct_present_flag
    b.flag(true); // bitstream_restriction_flag
    b.flag(true); // motion_vectors_over_pic_boundaries_flag
    b.ue(2); // max_bytes_per_pic_denom (the default)
    b.ue(1); // max_bits_per_mb_denom (the default)
    b.ue(15); // log2_max_mv_length_horizontal
    b.ue(15); // log2_max_mv_length_vertical
    b.ue(0); // max_num_reorder_frames
    b.ue(s.max_num_ref_frames); // max_dec_frame_buffering
    nal(3, 7, &b.finish())
}

pub(super) fn pps(s: &Settings) -> Vec<u8> {
    let mut b = Bits::default();
    b.ue(0); // pic_parameter_set_id
    b.ue(0); // seq_parameter_set_id
    b.flag(false); // entropy_coding_mode_flag: CAVLC
    b.flag(false); // bottom_field_pic_order_in_frame_present_flag
    b.ue(0); // num_slice_groups_minus1
    b.ue(0); // num_ref_idx_l0_default_active_minus1
    b.ue(0); // num_ref_idx_l1_default_active_minus1
    b.flag(false); // weighted_pred_flag
    b.u(2, 0); // weighted_bipred_idc
    b.se(s.pic_init_qp - 26);
    b.se(0); // pic_init_qs_minus26
    b.se(0); // chroma_qp_index_offset
    b.flag(true); // deblocking_filter_control_present_flag
    b.flag(false); // constrained_intra_pred_flag
    b.flag(false); // redundant_pic_cnt_present_flag
    nal(3, 8, &b.finish())
}

/// SPS then PPS, as one packed sequence header.
pub(super) fn sequence_header(s: &Settings) -> Vec<u8> {
    let mut v = sps(s);
    v.extend(pps(s));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(w: u32, h: u32) -> Settings {
        let (mb_w, mb_h) = (w.div_ceil(16), h.div_ceil(16));
        Settings {
            profile_idc: 66,
            level_idc: 40,
            mb_w,
            mb_h,
            crop_right: (mb_w * 16 - w) / 2,
            crop_bottom: (mb_h * 16 - h) / 2,
            log2_max_frame_num: 8,
            max_num_ref_frames: 1,
            pic_init_qp: 26,
        }
    }

    #[test]
    fn exp_golomb() {
        let mut b = Bits::default();
        for v in [0, 1, 2, 3, 7] {
            b.ue(v);
        }
        b.se(1);
        b.se(-1);
        // 1 010 011 00100 0001000 010 011, then the stop bit.
        assert_eq!(
            b.finish(),
            vec![0b1010_0110, 0b0100_0001, 0b0000_1001, 0b1100_0000]
        );
    }

    #[test]
    fn emulation_prevention() {
        assert_eq!(
            nal(3, 7, &[0, 0, 1, 0, 0, 0, 5]),
            vec![0, 0, 0, 1, 0x67, 0, 0, 3, 1, 0, 0, 3, 0, 5]
        );
    }

    #[test]
    fn headers_parse_back() {
        let s = settings(1366, 768);
        let sps = sps(&s);
        let info = crate::sps::parse(&sps[4..]).expect("SPS");
        assert_eq!((info.width, info.height), (1366, 768));
        let parsed = crate::h264::parse_sps(&sps[4..]).expect("SPS for slices");
        assert_eq!(parsed.log2_max_frame_num, 8);
        assert!(crate::h264::parse_pps(&pps(&s)[4..]).is_some());
        let both = sequence_header(&s);
        assert!(crate::video::sequence_header(&both).is_some());
        assert_eq!(sps[4], 0x67);
        assert_eq!(&sps[5..8], &[66, 0xc0, 40]);
    }

    /// An IDR of I_PCM macroblocks (raw samples) behind our headers, for a decoder to
    /// check the headers with: the picture must come back exactly.
    fn pcm_idr(s: &Settings, y: &[u8], u: &[u8], v: &[u8], w: usize) -> Vec<u8> {
        let mut b = Bits::default();
        b.ue(0); // first_mb_in_slice
        b.ue(7); // slice_type: I (all slices)
        b.ue(0); // pic_parameter_set_id
        b.u(s.log2_max_frame_num, 0); // frame_num
        b.ue(0); // idr_pic_id
        b.flag(false); // no_output_of_prior_pics_flag
        b.flag(false); // long_term_reference_flag
        b.se(0); // slice_qp_delta
        b.ue(1); // disable_deblocking_filter_idc
        let cw = w / 2;
        for my in 0..s.mb_h as usize {
            for mx in 0..s.mb_w as usize {
                b.ue(25); // mb_type I_PCM
                while !b.aligned() {
                    b.u(1, 0); // pcm_alignment_zero_bit
                }
                let at = |p: &[u8], stride: usize, x: usize, y: usize| {
                    let rows = p.len() / stride;
                    p[y.min(rows - 1) * stride + x.min(stride - 1)]
                };
                for j in 0..16 {
                    for i in 0..16 {
                        b.u(8, u64::from(at(y, w, mx * 16 + i, my * 16 + j)));
                    }
                }
                for plane in [u, v] {
                    for j in 0..8 {
                        for i in 0..8 {
                            b.u(8, u64::from(at(plane, cw, mx * 8 + i, my * 8 + j)));
                        }
                    }
                }
            }
        }
        let mut au = sequence_header(s);
        au.extend(nal(3, 5, &b.finish()));
        au
    }

    /// openh264 decodes our headers (with a lossless IDR behind them) to the picture.
    #[test]
    fn openh264_decodes_our_headers() {
        let Some(lib) = crate::cisco::local_binary_path().filter(|p| p.exists()) else {
            eprintln!("no openh264 binary (set RCONN_OPENH264); skipped");
            return;
        };
        let (w, h) = (50usize, 34usize); // cropped on both edges
        let s = settings(w as u32, h as u32);
        let y: Vec<u8> = (0..w * h).map(|i| 16 + (i * 7 % 200) as u8).collect();
        let u: Vec<u8> = (0..w * h / 4).map(|i| 40 + (i * 3 % 150) as u8).collect();
        let v: Vec<u8> = (0..w * h / 4).map(|i| 200 - (i * 5 % 150) as u8).collect();
        let au = pcm_idr(&s, &y, &u, &v, w);
        let mut dec = crate::decode::VideoDecoder::software(&lib).unwrap();
        let p = dec.decode(&au).unwrap().expect("a picture");
        assert_eq!((p.width, p.height), (w, h));
        assert_eq!(p.y(), &y[..], "luma");
        assert_eq!(p.chroma1(), &u[..], "Cb");
        assert_eq!(p.chroma2(), &v[..], "Cr");
    }
}
