//! What the viewer needs from an H.264 sequence header (SPS): the picture size after
//! cropping, and the colour signalling (matrix, range) of the VUI.
//!
//! Hosts encode BT.601 limited range and today write no colour description, so that is
//! the default; a stream that says otherwise (BT.709, BT.2020, full range) is shown with
//! its own colours.

/// The YUV -> RGB matrix of a stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Matrix {
    #[default]
    Bt601,
    Bt709,
    Bt2020,
}

/// Colour signalling of a stream (H.264 VUI `matrix_coefficients`, `video_full_range_flag`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Colour {
    pub matrix: Matrix,
    /// Full range (0-255) instead of limited (Y 16-235, chroma 16-240).
    pub full_range: bool,
}

impl Colour {
    /// Luma weights (Kr, Kb) of the matrix.
    pub fn kr_kb(self) -> (f32, f32) {
        match self.matrix {
            Matrix::Bt601 => (0.299, 0.114),
            Matrix::Bt709 => (0.2126, 0.0722),
            Matrix::Bt2020 => (0.2627, 0.0593),
        }
    }

    /// The affine YUV -> RGB transform on normalised samples (each 0..1, as a GPU reads an
    /// 8-bit texture): rows R, G, B of `[y, u, v, 1]`. Results are gamma-encoded RGB 0..1.
    pub fn yuv_to_rgb_rows(self) -> [[f32; 4]; 3] {
        let (kr, kb) = self.kr_kb();
        let kg = 1.0 - kr - kb;
        // Sample s (0..1) -> Y' 0..1 and Cb/Cr -0.5..0.5.
        let (ys, yo, cs, co) = if self.full_range {
            (1.0, 0.0, 1.0, -128.0 / 255.0)
        } else {
            (255.0 / 219.0, -16.0 / 219.0, 255.0 / 224.0, -128.0 / 224.0)
        };
        let rv = 2.0 * (1.0 - kr);
        let bu = 2.0 * (1.0 - kb);
        let gu = -2.0 * kb * (1.0 - kb) / kg;
        let gv = -2.0 * kr * (1.0 - kr) / kg;
        // R = Y' + rv*Cr; G = Y' + gu*Cb + gv*Cr; B = Y' + bu*Cb, with
        // Y' = ys*y + yo, Cb = cs*u + co, Cr = cs*v + co.
        [
            [ys, 0.0, rv * cs, yo + rv * co],
            [ys, gu * cs, gv * cs, yo + (gu + gv) * co],
            [ys, bu * cs, 0.0, yo + bu * co],
        ]
    }
}

/// What a sequence header says about the pictures that follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpsInfo {
    /// Displayed size (after the SPS cropping window).
    pub width: usize,
    pub height: usize,
    pub colour: Colour,
}

/// Parse an SPS NAL unit (starting at its header byte, as split from Annex-B).
pub fn parse(nal: &[u8]) -> Option<SpsInfo> {
    if nal.first()? & 0x1f != 7 {
        return None;
    }
    let rbsp = unescape(&nal[1..]);
    let mut r = Bits::new(&rbsp);
    let profile_idc = r.bits(8)?;
    r.bits(16)?; // constraint flags, level_idc
    r.ue()?; // seq_parameter_set_id
    let mut chroma_format_idc = 1;
    let mut separate_planes = false;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            separate_planes = r.flag()?;
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.flag()?; // qpprime_y_zero_transform_bypass_flag
        if r.flag()? {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if r.flag()? {
                    r.skip_scaling_list(if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.flag()?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.flag()?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = r.ue()? as usize + 1;
    let height_units = r.ue()? as usize + 1;
    let frame_mbs_only = r.flag()?;
    if !frame_mbs_only {
        r.flag()?; // mb_adaptive_frame_field_flag
    }
    r.flag()?; // direct_8x8_inference_flag
    let field = if frame_mbs_only { 1 } else { 2 };
    let (mut width, mut height) = (width_mbs * 16, height_units * 16 * field);
    if r.flag()? {
        let chroma = if separate_planes {
            0
        } else {
            chroma_format_idc
        };
        let (cx, cy) = match chroma {
            0 => (1, field),
            1 => (2, 2 * field),
            2 => (2, field),
            _ => (1, field),
        };
        let (left, right, top, bottom) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
        width = width.checked_sub(cx * (left + right) as usize)?;
        height = height.checked_sub(cy * (top + bottom) as usize)?;
    }
    let mut colour = Colour::default();
    // A header cut short after the size still gives the size (default colours).
    if r.flag() == Some(true) {
        let _ = vui(&mut r, &mut colour);
    }
    Some(SpsInfo {
        width,
        height,
        colour,
    })
}

/// The VUI fields up to the colour description.
fn vui(r: &mut Bits<'_>, colour: &mut Colour) -> Option<()> {
    if r.flag()? {
        // aspect_ratio_info_present_flag
        if r.bits(8)? == 255 {
            r.bits(32)?; // sar_width, sar_height
        }
    }
    if r.flag()? {
        r.flag()?; // overscan_appropriate_flag
    }
    if r.flag()? {
        // video_signal_type_present_flag
        r.bits(3)?; // video_format
        colour.full_range = r.flag()?;
        if r.flag()? {
            r.bits(16)?; // colour_primaries, transfer_characteristics
            colour.matrix = match r.bits(8)? {
                1 => Matrix::Bt709,
                9 | 10 => Matrix::Bt2020,
                _ => Matrix::Bt601, // 5, 6 (BT.601), 2 (unspecified), others
            };
        }
    }
    Some(())
}

/// Remove emulation-prevention bytes (`00 00 03` -> `00 00`).
fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// MSB-first bit reader with Exp-Golomb codes.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn flag(&mut self) -> Option<bool> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(bit == 1)
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | u32::from(self.flag()?);
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + u64::from(self.bits(zeros)?)) as u32)
    }

    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        Some(if k & 1 == 1 {
            k.div_ceil(2) as i32
        } else {
            -((k / 2) as i32)
        })
    }

    fn skip_scaling_list(&mut self, size: usize) -> Option<()> {
        let (mut last, mut next) = (8i32, 8i32);
        for _ in 0..size {
            if next != 0 {
                next = (last + self.se()? + 256) % 256;
            }
            if next != 0 {
                last = next;
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MSB-first bit writer, to build headers field by field.
    #[derive(Default)]
    struct W {
        bytes: Vec<u8>,
        n: usize,
    }

    impl W {
        fn bit(&mut self, b: bool) -> &mut Self {
            if self.n.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if b {
                *self.bytes.last_mut().unwrap() |= 0x80 >> (self.n % 8);
            }
            self.n += 1;
            self
        }
        fn bits(&mut self, v: u32, n: u32) -> &mut Self {
            for i in (0..n).rev() {
                self.bit((v >> i) & 1 == 1);
            }
            self
        }
        fn ue(&mut self, v: u32) -> &mut Self {
            let x = v + 1;
            let len = 32 - x.leading_zeros();
            self.bits(0, len - 1).bits(x, len)
        }
        fn se(&mut self, v: i32) -> &mut Self {
            self.ue(if v > 0 {
                (2 * v - 1) as u32
            } else {
                (-2 * v) as u32
            })
        }
    }

    /// High profile, 1496x938 (94x59 macroblocks, cropped), with a scaling list and an
    /// optional colour description.
    fn high_sps(colour: Option<(bool, u32)>) -> Vec<u8> {
        let mut w = W::default();
        w.bits(100, 8).bits(0, 8).bits(40, 8).ue(0);
        w.ue(1).ue(0).ue(0).bit(false); // 4:2:0, 8-bit
        w.bit(true); // seq_scaling_matrix_present_flag
        w.bit(true).se(-8); // list 0: one delta to 0 ("use the default list")
        for _ in 1..8 {
            w.bit(false);
        }
        w.ue(0).ue(0).ue(4); // frame_num, POC type 0, lsb
        w.ue(1).bit(false); // refs, gaps
        w.ue(93).ue(58).bit(true).bit(true); // 94x59 MBs, frames only, direct 8x8
        w.bit(true).ue(0).ue(0).ue(0).ue(3); // crop 944 -> 938
        match colour {
            None => {
                w.bit(false);
            }
            Some((full, matrix)) => {
                w.bit(true); // VUI
                w.bit(true).bits(255, 8).bits(1, 16).bits(1, 16); // SAR 1:1 (extended)
                w.bit(false); // overscan
                w.bit(true).bits(5, 3).bit(full).bit(true);
                w.bits(1, 8).bits(1, 8).bits(matrix, 8);
                w.bit(false); // chroma loc, timing... (cut short: ignored)
            }
        }
        w.bit(true); // rbsp stop bit
        let mut nal = vec![0x67];
        // Escape like an encoder would.
        let mut zeros = 0;
        for &b in &w.bytes {
            if zeros >= 2 && b <= 3 {
                nal.push(3);
                zeros = 0;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            nal.push(b);
        }
        nal
    }

    #[test]
    fn high_profile_size_crop_and_colour() {
        let plain = parse(&high_sps(None)).unwrap();
        assert_eq!((plain.width, plain.height), (1504, 938));
        assert_eq!(plain.colour, Colour::default(), "no VUI: BT.601 limited");
        let hd = parse(&high_sps(Some((false, 1)))).unwrap();
        assert_eq!(hd.colour.matrix, Matrix::Bt709);
        assert!(!hd.colour.full_range);
        let full = parse(&high_sps(Some((true, 6)))).unwrap();
        assert_eq!(
            full.colour,
            Colour {
                matrix: Matrix::Bt601,
                full_range: true
            }
        );
        assert!(parse(&[0x68, 1, 2]).is_none(), "not an SPS");
        assert!(parse(&[0x67, 100]).is_none(), "truncated");
    }

    #[test]
    fn encoder_headers_parse() {
        let Some(lib) = crate::cisco::local_binary_path().filter(|p| p.exists()) else {
            return;
        };
        let mut enc =
            crate::video::VideoEncoder::new(&lib, crate::video::EncoderSettings::default())
                .unwrap();
        let (w, h) = (1494, 938);
        let au = enc
            .encode(&crate::video::Frame {
                width: w,
                height: h,
                bgra: vec![90; w * h * 4],
                captured_at: std::time::Instant::now(),
            })
            .unwrap()
            .expect("first frame");
        let sps = crate::video::nal_units(&au)
            .find(|n| n[0] & 0x1f == 7)
            .unwrap();
        let info = parse(sps).unwrap();
        assert_eq!((info.width, info.height), (w, h));
        assert_eq!(info.colour, Colour::default());
    }

    #[test]
    fn bt601_rows_match_the_integer_conversion() {
        // White, black and a saturated red in limited range.
        let m = Colour::default().yuv_to_rgb_rows();
        let rgb = |y: u8, u: u8, v: u8| {
            let s = [
                f32::from(y) / 255.0,
                f32::from(u) / 255.0,
                f32::from(v) / 255.0,
            ];
            m.map(|r| ((r[0] * s[0] + r[1] * s[1] + r[2] * s[2] + r[3]) * 255.0).round())
        };
        assert_eq!(rgb(235, 128, 128), [255.0, 255.0, 255.0]);
        assert_eq!(rgb(16, 128, 128), [0.0, 0.0, 0.0]);
        let red = rgb(81, 90, 240);
        assert!(red[0] >= 254.0 && red[1] <= 1.0 && red[2] <= 1.0, "{red:?}");
    }
}
