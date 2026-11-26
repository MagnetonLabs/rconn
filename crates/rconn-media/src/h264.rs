//! Just enough H.264 syntax to follow our own encoder's reference structure: which frame
//! a slice is (`frame_num`, `idr_pic_id`), whether it is marked as a long-term reference,
//! and whether it predicts from one. Loss recovery with long-term references needs these
//! numbers ([`crate::video`]), and openh264 does not report them.
//!
//! Covers what Baseline/Main encoders emit (no scaling matrices, one slice group, no
//! weighted prediction); anything else parses to `None` and recovery falls back to IDR.

/// Exp-Golomb bit reader over a NAL unit's payload (emulation prevention removed).
struct Bits {
    data: Vec<u8>,
    pos: usize,
}

impl Bits {
    /// `nal` without its start code; the header byte is skipped.
    fn new(nal: &[u8]) -> Self {
        let mut data = Vec::with_capacity(nal.len().min(64));
        let mut zeros = 0;
        // 64 bytes covers any slice header we parse; the rest is slice data.
        for &b in nal.iter().skip(1).take(96) {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            data.push(b);
        }
        Self { data, pos: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(u32::from(b))
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + u64::from(self.bits(zeros)?)) as u32)
    }

    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k % 2 == 1 {
            k.div_ceil(2) as i32
        } else {
            -((k / 2) as i32)
        })
    }
}

/// What a slice header needs from its sequence parameter set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sps {
    pub log2_max_frame_num: u32,
    poc_type: u32,
    log2_max_poc_lsb: u32,
    delta_pic_order_always_zero: bool,
    frame_mbs_only: bool,
}

/// What a slice header needs from its picture parameter set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pps {
    bottom_field_pic_order_in_frame_present: bool,
    redundant_pic_cnt_present: bool,
    weighted_pred: bool,
}

pub fn parse_sps(nal: &[u8]) -> Option<Sps> {
    let mut r = Bits::new(nal);
    let profile = r.bits(8)?;
    r.bits(16)?; // constraint flags, level
    r.ue()?; // seq_parameter_set_id
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        if r.ue()? == 3 {
            r.bit()?; // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.bit()?; // qpprime_y_zero_transform_bypass_flag
        if r.bit()? == 1 {
            return None; // scaling matrices: not something we emit
        }
    }
    let log2_max_frame_num = r.ue()? + 4;
    let poc_type = r.ue()?;
    let mut log2_max_poc_lsb = 0;
    let mut delta_pic_order_always_zero = false;
    match poc_type {
        0 => log2_max_poc_lsb = r.ue()? + 4,
        1 => {
            delta_pic_order_always_zero = r.bit()? == 1;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.bit()?; // gaps_in_frame_num_value_allowed_flag
    r.ue()?; // pic_width_in_mbs_minus1
    r.ue()?; // pic_height_in_map_units_minus1
    let frame_mbs_only = r.bit()? == 1;
    Some(Sps {
        log2_max_frame_num,
        poc_type,
        log2_max_poc_lsb,
        delta_pic_order_always_zero,
        frame_mbs_only,
    })
}

pub fn parse_pps(nal: &[u8]) -> Option<Pps> {
    let mut r = Bits::new(nal);
    r.ue()?; // pic_parameter_set_id
    r.ue()?; // seq_parameter_set_id
    r.bit()?; // entropy_coding_mode_flag
    let bottom_field_pic_order_in_frame_present = r.bit()? == 1;
    if r.ue()? != 0 {
        return None; // slice groups
    }
    r.ue()?; // num_ref_idx_l0_default_active_minus1
    r.ue()?; // num_ref_idx_l1_default_active_minus1
    let weighted_pred = r.bit()? == 1;
    r.bits(2)?; // weighted_bipred_idc
    r.se()?; // pic_init_qp_minus26
    r.se()?; // pic_init_qs_minus26
    r.se()?; // chroma_qp_index_offset
    r.bit()?; // deblocking_filter_control_present_flag
    r.bit()?; // constrained_intra_pred_flag
    let redundant_pic_cnt_present = r.bit()? == 1;
    Some(Pps {
        bottom_field_pic_order_in_frame_present,
        redundant_pic_cnt_present,
        weighted_pred,
    })
}

/// The reference facts of one coded picture (its first slice).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceInfo {
    pub idr: bool,
    pub frame_num: u32,
    /// For IDR pictures; repeated by every later picture of the same IDR period.
    pub idr_pic_id: u32,
    /// This picture makes a picture a long-term reference: the `frame_num` of the one it
    /// marks. Itself for an IDR with `long_term_reference_flag` or memory management
    /// operation 6; an earlier picture for operation 3 (openh264's delayed marking).
    pub marks_long_term: Option<u32>,
    /// The picture predicts from a long-term reference (reference list modification by
    /// long-term picture number).
    pub refs_long_term: bool,
}

/// Parse the header of a slice NAL unit (type 1 or 5).
pub fn parse_slice(nal: &[u8], sps: &Sps, pps: &Pps) -> Option<SliceInfo> {
    let nal_type = nal.first()? & 0x1f;
    let nal_ref_idc = (nal[0] >> 5) & 3;
    let idr = nal_type == 5;
    let mut r = Bits::new(nal);
    r.ue()?; // first_mb_in_slice
    let slice_type = r.ue()? % 5; // 0 P, 1 B, 2 I, 3 SP, 4 SI
    r.ue()?; // pic_parameter_set_id
    let frame_num = r.bits(sps.log2_max_frame_num)?;
    let mut field = false;
    if !sps.frame_mbs_only {
        field = r.bit()? == 1;
        if field {
            r.bit()?; // bottom_field_flag
        }
    }
    let idr_pic_id = if idr { r.ue()? } else { 0 };
    if sps.poc_type == 0 {
        r.bits(sps.log2_max_poc_lsb)?;
        if pps.bottom_field_pic_order_in_frame_present && !field {
            r.se()?;
        }
    }
    if sps.poc_type == 1 && !sps.delta_pic_order_always_zero {
        r.se()?;
        if pps.bottom_field_pic_order_in_frame_present && !field {
            r.se()?;
        }
    }
    if pps.redundant_pic_cnt_present {
        r.ue()?;
    }
    if slice_type == 1 {
        return None; // B slices: never emitted (Baseline, no reordering)
    }
    let predicted = matches!(slice_type, 0 | 3);
    if predicted && r.bit()? == 1 {
        r.ue()?; // num_ref_idx_l0_active_minus1
    }
    let mut refs_long_term = false;
    if slice_type != 2 && slice_type != 4 && r.bit()? == 1 {
        loop {
            match r.ue()? {
                0 | 1 => {
                    r.ue()?;
                }
                2 => {
                    r.ue()?;
                    refs_long_term = true;
                }
                3 => break,
                _ => return None,
            }
        }
    }
    if pps.weighted_pred && predicted {
        return None;
    }
    let mut marks_long_term = None;
    let max_frame_num = 1u32 << sps.log2_max_frame_num;
    if nal_ref_idc != 0 {
        if idr {
            r.bit()?; // no_output_of_prior_pics_flag
            marks_long_term = (r.bit()? == 1).then_some(frame_num);
        } else if r.bit()? == 1 {
            loop {
                match r.ue()? {
                    0 => break,
                    1 => {
                        r.ue()?;
                    }
                    2 => {
                        r.ue()?;
                    }
                    3 => {
                        // difference_of_pic_nums_minus1, then long_term_frame_idx.
                        let diff = r.ue()? + 1;
                        r.ue()?;
                        marks_long_term = Some(
                            (frame_num + max_frame_num - diff % max_frame_num) % max_frame_num,
                        );
                    }
                    4 => {
                        r.ue()?;
                    }
                    5 => {}
                    6 => {
                        r.ue()?;
                        marks_long_term = Some(frame_num);
                    }
                    _ => return None,
                }
            }
        }
    }
    Some(SliceInfo {
        idr,
        frame_num,
        idr_pic_id,
        marks_long_term,
        refs_long_term,
    })
}

/// Follows an encoder's parameter sets across access units.
#[derive(Debug, Default)]
pub struct SyntaxTracker {
    sps: Option<Sps>,
    pps: Option<Pps>,
    /// Of the last IDR picture: later pictures belong to its period.
    idr_pic_id: u32,
}

impl SyntaxTracker {
    /// The first slice's facts of this access unit, updating parameter sets on the way.
    /// Non-IDR pictures get the `idr_pic_id` of the IDR period they belong to.
    pub fn access_unit(&mut self, annex_b: &[u8]) -> Option<SliceInfo> {
        for nal in crate::video::nal_units(annex_b) {
            match nal[0] & 0x1f {
                7 => self.sps = parse_sps(nal),
                8 => self.pps = parse_pps(nal),
                1 | 5 => {
                    let mut s = parse_slice(nal, self.sps.as_ref()?, self.pps.as_ref()?)?;
                    if s.idr {
                        self.idr_pic_id = s.idr_pic_id;
                    } else {
                        s.idr_pic_id = self.idr_pic_id;
                    }
                    return Some(s);
                }
                _ => {}
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb() {
        // 1 | 010 | 011 | 00100 | 00101 -> 0, 1, 2, 3, 4
        let mut r = Bits {
            data: vec![0b1010_0110, 0b0100_0010, 0b1000_0000],
            pos: 0,
        };
        let v: Vec<u32> = (0..5).map(|_| r.ue().unwrap()).collect();
        assert_eq!(v, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn emulation_prevention_is_removed() {
        let r = Bits::new(&[0x67, 0, 0, 3, 1, 0xff]);
        assert_eq!(r.data, [0, 0, 1, 0xff]);
    }
}
