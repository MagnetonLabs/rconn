//! H.264 on the GPU through VA-API, tuned like the other engines for latency: Constrained
//! Baseline, P frames predicting from the previous frame only (no B-frames, no
//! reordering), keyframes only on request, VBR/CBR whose bitrate changes live.
//!
//! The stream headers (SPS/PPS) are ours where the driver takes them packed (Intel's
//! do, [`super::headers`]), else the driver's; the slice headers are always the
//! driver's, from the parameters given here. Each new encoder proves itself before use:
//! two frames are encoded and decoded with openh264, and the picture must come back.
//!
//! Loss repair: no long-term references (the driver writes the slice headers, so their
//! marking can't be set from here); a reported loss is answered with a keyframe, as on
//! VideoToolbox.

use super::ffi::*;
use super::vpp::{ColourCheck, Vpp};
use super::{Display, display};
use crate::codec::CodecError;
use crate::dmabuf::DmaBuf;
use std::ffi::c_void;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// frame_num wraps at 2^8 (every frame is a reference, so it counts frames since the IDR).
const LOG2_MAX_FRAME_NUM: u32 = 8;
const LOG2_MAX_POC_LSB: u32 = 8;
/// The quantizer for the first frame (the rate controller takes over from there).
const INITIAL_QP: u32 = 26;
/// Never finer than this (as openh264's floor): spends no bits on invisible detail.
const MIN_QP: u32 = 10;

fn err(e: impl std::fmt::Display) -> CodecError {
    CodecError::VaApi(e.to_string())
}

/// Where the last frame's time went (the self-test reports these).
#[derive(Clone, Copy, Debug, Default)]
pub struct Stages {
    /// Copying the picture into GPU memory (va-upload).
    pub upload: Duration,
    /// Importing the DMA-BUF and converting it on the video processor (va-zerocopy).
    pub convert: Duration,
    /// Encoding, until the bitstream is back in memory.
    pub encode: Duration,
}

/// The reconstructed picture the next P frame predicts from.
#[derive(Clone, Copy, Debug)]
struct Reference {
    slot: usize,
    frame_num: u32,
    poc: i32,
}

pub struct VaH264Encoder {
    d: Arc<Display>,
    config: VAConfigID,
    context: VAContextID,
    /// The picture to encode (NV12, the encoder's size): uploaded into, or written by the
    /// video processor.
    input: VASurfaceID,
    /// Reconstructed pictures: the reference and the one being made.
    recon: [VASurfaceID; 2],
    coded: VABufferID,
    width: u32,
    height: u32,
    mb_w: u32,
    mb_h: u32,
    fps: u32,
    bps: u32,
    /// Bitrate last given to the driver's rate controller.
    rc_sent: Option<u32>,
    reference: Option<Reference>,
    force_key: bool,
    frame_num: u32,
    poc: i32,
    idr_pic_id: u16,
    /// NV12 staging image for uploads (and readbacks in checks).
    image: Option<VAImage>,
    /// `image` holds the last picture uploaded (rows not reported changed are valid).
    image_valid: bool,
    vpp: Option<Vpp>,
    name: String,
    stages: Stages,
}

// SAFETY: VA objects are handles; one thread uses an encoder at a time.
unsafe impl Send for VaH264Encoder {}

/// The `seq_fields` bit fields of [`VAEncSequenceParameterBufferH264`]: 4:2:0, frames
/// only, direct 8x8 inference, picture order count type 2 (output order is decode order).
pub(super) fn seq_fields() -> u32 {
    let chroma_format_idc = 1;
    let frame_mbs_only = 1 << 2;
    let direct_8x8_inference = 1 << 5;
    let log2_max_frame_num_minus4 = (LOG2_MAX_FRAME_NUM - 4) << 6;
    let pic_order_cnt_type = 2 << 10;
    let log2_max_poc_lsb_minus4 = (LOG2_MAX_POC_LSB - 4) << 12;
    chroma_format_idc
        | frame_mbs_only
        | direct_8x8_inference
        | log2_max_frame_num_minus4
        | pic_order_cnt_type
        | log2_max_poc_lsb_minus4
}

/// The `pic_fields` bit fields of [`VAEncPictureParameterBufferH264`]: CAVLC, a reference
/// picture, deblocking control in the slice header.
pub(super) fn pic_fields(idr: bool) -> u32 {
    let idr_pic_flag = u32::from(idr);
    let reference_pic_flag = 1 << 1;
    let deblocking_filter_control_present = 1 << 9;
    idr_pic_flag | reference_pic_flag | deblocking_filter_control_present
}

/// The `rc_flags` of [`VAEncMiscParameterRateControl`]: never skip frames (a skipped
/// frame would leave the viewer behind) and no CBR padding (a still screen costs nothing).
pub(super) fn rc_flags() -> u32 {
    let disable_frame_skip = 1 << 1;
    let disable_bit_stuffing = 1 << 2;
    disable_frame_skip | disable_bit_stuffing
}

/// The lowest H.264 level (`level_idc`) for `w` x `h` at `fps`.
pub(super) fn level_idc(w: u32, h: u32, fps: u32) -> u8 {
    let mbs = w.div_ceil(16) * h.div_ceil(16);
    let mbps = mbs * fps.max(1);
    // (level_idc, max macroblocks/s, max frame size in macroblocks), Table A-1.
    const LEVELS: [(u8, u32, u32); 11] = [
        (30, 40_500, 1_620),
        (31, 108_000, 3_600),
        (32, 216_000, 5_120),
        (40, 245_760, 8_192),
        (42, 522_240, 8_704),
        (50, 589_824, 22_080),
        (51, 983_040, 36_864),
        (52, 2_073_600, 36_864),
        (60, 4_177_920, 139_264),
        (61, 8_355_840, 139_264),
        (62, 16_711_680, 139_264),
    ];
    LEVELS
        .iter()
        .find(|&&(_, max_mbps, max_fs)| mbps <= max_mbps && mbs <= max_fs)
        .map_or(62, |l| l.0)
}

/// Frame cropping for a `w` x `h` picture coded in whole macroblocks: (right, bottom)
/// in 4:2:0 crop units (2 pixels).
pub(super) fn crop(w: u32, h: u32) -> (u32, u32) {
    ((w.div_ceil(16) * 16 - w) / 2, (h.div_ceil(16) * 16 - h) / 2)
}

/// A misc parameter buffer's bytes: its type, then the parameters.
fn misc<T: Plain>(kind: u32, payload: &T) -> Vec<u8> {
    let mut v = kind.to_ne_bytes().to_vec();
    v.extend_from_slice(bytes_of(payload));
    v
}

impl VaH264Encoder {
    /// A low-latency H.264 encoder for `width` x `height` (even) on the process's VA
    /// display. With `check` (openh264's library), it must first encode two frames that
    /// openh264 decodes back to the picture.
    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
        check: Option<&Path>,
    ) -> Result<Self, CodecError> {
        if width < 16 || height < 16 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(err(format!("unsupported size {width}x{height}")));
        }
        let d = display().map_err(err)?;
        let (va, dpy) = (d.va, d.dpy);
        let mut attribs = vec![
            VAConfigAttrib {
                type_: VAConfigAttribRTFormat,
                value: VA_RT_FORMAT_YUV420,
            },
            VAConfigAttrib {
                type_: VAConfigAttribRateControl,
                value: d.caps.rc_mode,
            },
        ];
        if d.caps.packed_sequence {
            // Our SPS and PPS; slice headers stay the driver's.
            attribs.push(VAConfigAttrib {
                type_: VAConfigAttribEncPackedHeaders,
                value: VA_ENC_PACKED_HEADER_SEQUENCE,
            });
        }
        let mut config = VA_INVALID_ID;
        let s = unsafe {
            (va.vaCreateConfig)(
                dpy,
                d.caps.profile,
                d.caps.entrypoint,
                attribs.as_mut_ptr(),
                attribs.len() as i32,
                &mut config,
            )
        };
        d.check(s, "vaCreateConfig").map_err(err)?;
        let (mb_w, mb_h) = (width.div_ceil(16), height.div_ceil(16));
        let name = format!(
            "{} {} {}",
            d.vendor.split(" - ").next().unwrap_or(&d.vendor).trim(),
            d.profile_name(),
            if d.caps.rc_mode == VA_RC_VBR {
                "VBR"
            } else {
                "CBR"
            }
        );
        // From here on, Drop releases whatever was made.
        let mut e = Self {
            d: d.clone(),
            config,
            context: VA_INVALID_ID,
            input: VA_INVALID_SURFACE,
            recon: [VA_INVALID_SURFACE; 2],
            coded: VA_INVALID_ID,
            width,
            height,
            mb_w,
            mb_h,
            fps: fps.max(1),
            bps: bitrate_bps.max(100_000),
            rc_sent: None,
            reference: None,
            force_key: true,
            frame_num: 0,
            poc: 0,
            idr_pic_id: 0,
            image: None,
            image_valid: false,
            vpp: None,
            name,
            stages: Stages::default(),
        };
        e.input = e.nv12_surface(width, height)?;
        for i in 0..2 {
            e.recon[i] = e.nv12_surface(mb_w * 16, mb_h * 16)?;
        }
        let s = unsafe {
            (va.vaCreateContext)(
                dpy,
                config,
                (mb_w * 16) as i32,
                (mb_h * 16) as i32,
                VA_PROGRESSIVE,
                e.recon.as_mut_ptr(),
                2,
                &mut e.context,
            )
        };
        d.check(s, "vaCreateContext").map_err(err)?;
        // Room for an intra frame at a low quantizer.
        let coded_size = (mb_w * mb_h * 400).max(1 << 20);
        let s = unsafe {
            (va.vaCreateBuffer)(
                dpy,
                e.context,
                VAEncCodedBufferType,
                coded_size,
                1,
                std::ptr::null_mut(),
                &mut e.coded,
            )
        };
        d.check(s, "coded buffer").map_err(err)?;
        if let Some(openh264) = check {
            e.self_test(openh264)?;
        }
        Ok(e)
    }

    fn nv12_surface(&self, w: u32, h: u32) -> Result<VASurfaceID, CodecError> {
        let mut attribs = [VASurfaceAttrib::int(
            VASurfaceAttribPixelFormat,
            VA_FOURCC_NV12 as i32,
        )];
        let mut id = VA_INVALID_SURFACE;
        let s = unsafe {
            (self.d.va.vaCreateSurfaces)(
                self.d.dpy,
                VA_RT_FORMAT_YUV420,
                w,
                h,
                &mut id,
                1,
                attribs.as_mut_ptr(),
                1,
            )
        };
        self.d.check(s, "NV12 surface").map_err(err)?;
        Ok(id)
    }

    /// Driver and settings, e.g. "Intel iHD driver for Intel(R) Gen Graphics Constrained
    /// Baseline VBR".
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Where the last frame's time went.
    pub fn stages(&self) -> Stages {
        self.stages
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The next frame is an IDR.
    pub fn force_keyframe(&mut self) {
        self.force_key = true;
    }

    /// Live: the next frame carries the new rate to the driver's rate controller (no
    /// keyframe).
    pub fn set_bitrate(&mut self, bps: u32) {
        self.bps = bps.max(100_000);
    }

    /// Encode an NV12 picture of the encoder's size (`width` bytes per row, UV after Y).
    /// `rows` (even) are the only ones changed since the last picture given here; `None`
    /// means all.
    pub fn encode_nv12(
        &mut self,
        nv12: &[u8],
        rows: Option<(usize, usize)>,
        force_key: bool,
    ) -> Result<Vec<u8>, CodecError> {
        let t = Instant::now();
        self.upload(nv12, rows)?;
        self.stages = Stages {
            upload: t.elapsed(),
            ..Stages::default()
        };
        self.encode_input(force_key)
    }

    /// Convert a captured DMA-BUF screen to NV12 at the encoder's size on the video
    /// processor, and encode it.
    pub fn encode_dmabuf(
        &mut self,
        frame: &DmaBuf,
        force_key: bool,
    ) -> Result<Vec<u8>, CodecError> {
        let t = Instant::now();
        let vpp = self.vpp()?;
        let src = vpp.import(frame).map_err(err)?;
        let input = self.input;
        self.vpp
            .as_mut()
            .expect("made above")
            .convert(src, input)
            .map_err(err)?;
        self.image_valid = false; // the input no longer holds the uploaded picture
        self.stages = Stages {
            convert: t.elapsed(),
            ..Stages::default()
        };
        self.encode_input(force_key)
    }

    fn vpp(&mut self) -> Result<&mut Vpp, CodecError> {
        if self.vpp.is_none() {
            self.vpp =
                Some(Vpp::new(self.d.clone(), self.input, self.width, self.height).map_err(err)?);
        }
        Ok(self.vpp.as_mut().expect("set above"))
    }

    /// Run colour bars through the video processor and compare with the CPU conversion.
    /// `Err` if the colours are not BT.601 limited range (some drivers ignore the
    /// requested colour standard).
    pub fn check_colours(&mut self) -> Result<ColourCheck, CodecError> {
        let check = self.colour_check()?;
        if let Some(e) = &check.refused {
            return Err(err(e));
        }
        Ok(check)
    }

    /// As [`Self::check_colours`], but a refusal is only reported in the result.
    pub fn colour_check(&mut self) -> Result<ColourCheck, CodecError> {
        let image = self.staging()?;
        let (w, h) = (self.width, self.height);
        let input = self.input;
        self.image_valid = false;
        self.vpp()?.check_colours(input, w, h, &image).map_err(err)
    }

    /// The NV12 staging image (made on first use).
    fn staging(&mut self) -> Result<VAImage, CodecError> {
        if let Some(i) = self.image {
            return Ok(i);
        }
        let mut fmt: VAImageFormat = zeroed();
        fmt.fourcc = VA_FOURCC_NV12;
        fmt.byte_order = VA_LSB_FIRST;
        fmt.bits_per_pixel = 12;
        let mut image: VAImage = zeroed();
        let s = unsafe {
            (self.d.va.vaCreateImage)(
                self.d.dpy,
                &mut fmt,
                self.width as i32,
                self.height as i32,
                &mut image,
            )
        };
        self.d.check(s, "NV12 image").map_err(err)?;
        self.image = Some(image);
        self.image_valid = false;
        Ok(image)
    }

    fn upload(&mut self, nv12: &[u8], rows: Option<(usize, usize)>) -> Result<(), CodecError> {
        let (w, h) = (self.width as usize, self.height as usize);
        if nv12.len() < w * h * 3 / 2 {
            return Err(err(format!(
                "NV12 picture of {} bytes for {w}x{h}",
                nv12.len()
            )));
        }
        let image = self.staging()?;
        let (r0, r1) = match rows {
            Some((a, b)) if self.image_valid => (a & !1, b.next_multiple_of(2).min(h)),
            _ => (0, h),
        };
        if r1 > r0 {
            let (va, dpy) = (self.d.va, self.d.dpy);
            let mut p: *mut c_void = std::ptr::null_mut();
            self.d
                .check(
                    unsafe { (va.vaMapBuffer)(dpy, image.buf, &mut p) },
                    "map image",
                )
                .map_err(err)?;
            let written = write_nv12(&image, p.cast(), nv12, (w, h), (r0, r1));
            unsafe { (va.vaUnmapBuffer)(dpy, image.buf) };
            written.map_err(err)?;
        }
        let (va, dpy) = (self.d.va, self.d.dpy);
        let s = unsafe {
            (va.vaPutImage)(
                dpy,
                self.input,
                image.image_id,
                0,
                0,
                w as u32,
                h as u32,
                0,
                0,
                w as u32,
                h as u32,
            )
        };
        self.d.check(s, "vaPutImage").map_err(err)?;
        self.image_valid = true;
        Ok(())
    }

    /// What our packed SPS/PPS say (the same as the parameter buffers).
    fn header_settings(&self) -> super::headers::Settings {
        let (crop_right, crop_bottom) = crop(self.width, self.height);
        super::headers::Settings {
            profile_idc: if self.d.caps.profile == VAProfileH264ConstrainedBaseline {
                66
            } else {
                77
            },
            level_idc: level_idc(self.width, self.height, self.fps),
            mb_w: self.mb_w,
            mb_h: self.mb_h,
            crop_right,
            crop_bottom,
            log2_max_frame_num: LOG2_MAX_FRAME_NUM,
            max_num_ref_frames: 1,
            pic_init_qp: INITIAL_QP as i32,
        }
    }

    fn seq_params(&self) -> VAEncSequenceParameterBufferH264 {
        let mut s: VAEncSequenceParameterBufferH264 = zeroed();
        s.seq_parameter_set_id = 0;
        s.level_idc = level_idc(self.width, self.height, self.fps);
        // Keyframes come on request only; the period is a hint for rate control.
        s.intra_period = self.fps * 60;
        s.intra_idr_period = self.fps * 60;
        s.ip_period = 1;
        s.bits_per_second = self.bps;
        s.max_num_ref_frames = 1;
        s.picture_width_in_mbs = self.mb_w as u16;
        s.picture_height_in_mbs = self.mb_h as u16;
        s.seq_fields = seq_fields();
        let (right, bottom) = crop(self.width, self.height);
        if right > 0 || bottom > 0 {
            s.frame_cropping_flag = 1;
            s.frame_crop_right_offset = right;
            s.frame_crop_bottom_offset = bottom;
        }
        s
    }

    fn pic_params(&self, idr: bool, slot: usize) -> VAEncPictureParameterBufferH264 {
        let mut p: VAEncPictureParameterBufferH264 = zeroed();
        p.CurrPic = VAPictureH264 {
            picture_id: self.recon[slot],
            frame_idx: self.frame_num,
            flags: 0,
            TopFieldOrderCnt: self.poc,
            BottomFieldOrderCnt: self.poc,
            va_reserved: [0; 4],
        };
        p.ReferenceFrames = [VAPictureH264::INVALID; 16];
        if let (false, Some(r)) = (idr, self.reference) {
            p.ReferenceFrames[0] = self.ref_pic(r);
        }
        p.coded_buf = self.coded;
        p.frame_num = self.frame_num as u16;
        p.pic_init_qp = INITIAL_QP as u8;
        p.pic_fields = pic_fields(idr);
        p
    }

    fn ref_pic(&self, r: Reference) -> VAPictureH264 {
        VAPictureH264 {
            picture_id: self.recon[r.slot],
            frame_idx: r.frame_num,
            flags: VA_PICTURE_H264_SHORT_TERM_REFERENCE,
            TopFieldOrderCnt: r.poc,
            BottomFieldOrderCnt: r.poc,
            va_reserved: [0; 4],
        }
    }

    fn slice_params(&self, idr: bool) -> VAEncSliceParameterBufferH264 {
        let mut s: VAEncSliceParameterBufferH264 = zeroed();
        s.macroblock_address = 0;
        s.num_macroblocks = self.mb_w * self.mb_h;
        s.macroblock_info = VA_INVALID_ID;
        s.slice_type = if idr { 2 } else { 0 }; // I, P
        s.pic_parameter_set_id = 0;
        s.idr_pic_id = self.idr_pic_id;
        s.pic_order_cnt_lsb = (self.poc as u32 & ((1 << LOG2_MAX_POC_LSB) - 1)) as u16;
        s.RefPicList0 = [VAPictureH264::INVALID; 32];
        s.RefPicList1 = [VAPictureH264::INVALID; 32];
        if let (false, Some(r)) = (idr, self.reference) {
            s.RefPicList0[0] = self.ref_pic(r);
        }
        s
    }

    fn rate_control(&self) -> [Vec<u8>; 2] {
        let mut rc: VAEncMiscParameterRateControl = zeroed();
        rc.bits_per_second = self.bps;
        // VBR: aim a little under the peak, so bursts (scrolling) fit the link.
        rc.target_percentage = if self.d.caps.rc_mode == VA_RC_VBR {
            90
        } else {
            100
        };
        rc.window_size = 1000;
        rc.initial_qp = INITIAL_QP;
        rc.min_qp = MIN_QP;
        rc.rc_flags = rc_flags();
        let mut hrd: VAEncMiscParameterHRD = zeroed();
        hrd.buffer_size = self.bps; // one second
        hrd.initial_buffer_fullness = self.bps / 4 * 3;
        [
            misc(VAEncMiscParameterTypeRateControl, &rc),
            misc(VAEncMiscParameterTypeHRD, &hrd),
        ]
    }

    fn frame_rate(&self) -> Vec<u8> {
        let mut fr: VAEncMiscParameterFrameRate = zeroed();
        fr.framerate = self.fps | (1 << 16);
        misc(VAEncMiscParameterTypeFrameRate, &fr)
    }

    fn buffer(&self, kind: VABufferType, data: &[u8]) -> Result<VABufferID, CodecError> {
        let mut id = VA_INVALID_ID;
        let s = unsafe {
            (self.d.va.vaCreateBuffer)(
                self.d.dpy,
                self.context,
                kind,
                data.len() as u32,
                1,
                data.as_ptr() as *mut c_void,
                &mut id,
            )
        };
        self.d.check(s, "vaCreateBuffer").map_err(err)?;
        Ok(id)
    }

    /// Encode the input surface; returns the Annex-B access unit.
    fn encode_input(&mut self, force_key: bool) -> Result<Vec<u8>, CodecError> {
        let t = Instant::now();
        let idr = force_key || self.force_key || self.reference.is_none();
        if idr {
            self.frame_num = 0;
            self.poc = 0;
        }
        let slot = match self.reference {
            Some(r) if !idr => 1 - r.slot,
            _ => 0,
        };
        let mut buffers = Vec::with_capacity(7);
        let submitted = self.submit(idr, slot, &mut buffers);
        for b in buffers {
            unsafe { (self.d.va.vaDestroyBuffer)(self.d.dpy, b) };
        }
        submitted?;
        let out = self.read_coded()?;
        if idr {
            self.idr_pic_id = self.idr_pic_id.wrapping_add(1);
            self.force_key = false;
        }
        self.reference = Some(Reference {
            slot,
            frame_num: self.frame_num,
            poc: self.poc,
        });
        self.frame_num = (self.frame_num + 1) % (1 << LOG2_MAX_FRAME_NUM);
        self.poc = self.poc.wrapping_add(2);
        self.rc_sent = Some(self.bps);
        self.stages.encode = t.elapsed();
        Ok(out)
    }

    fn submit(
        &self,
        idr: bool,
        slot: usize,
        buffers: &mut Vec<VABufferID>,
    ) -> Result<(), CodecError> {
        if idr {
            buffers.push(self.buffer(
                VAEncSequenceParameterBufferType,
                bytes_of(&self.seq_params()),
            )?);
            buffers.push(self.buffer(VAEncMiscParameterBufferType, &self.frame_rate())?);
        }
        if idr || self.rc_sent != Some(self.bps) {
            for m in self.rate_control() {
                buffers.push(self.buffer(VAEncMiscParameterBufferType, &m)?);
            }
        }
        buffers.push(self.buffer(
            VAEncPictureParameterBufferType,
            bytes_of(&self.pic_params(idr, slot)),
        )?);
        if idr && self.d.caps.packed_sequence {
            let header = super::headers::sequence_header(&self.header_settings());
            let mut p: VAEncPackedHeaderParameterBuffer = zeroed();
            p.type_ = VAEncPackedHeaderSequence;
            p.bit_length = (header.len() * 8) as u32;
            p.has_emulation_bytes = 1;
            buffers.push(self.buffer(VAEncPackedHeaderParameterBufferType, bytes_of(&p))?);
            buffers.push(self.buffer(VAEncPackedHeaderDataBufferType, &header)?);
        }
        buffers.push(self.buffer(
            VAEncSliceParameterBufferType,
            bytes_of(&self.slice_params(idr)),
        )?);
        let (va, dpy, ctx) = (self.d.va, self.d.dpy, self.context);
        self.d
            .check(
                unsafe { (va.vaBeginPicture)(dpy, ctx, self.input) },
                "vaBeginPicture",
            )
            .map_err(err)?;
        let rendered = self.d.check(
            unsafe { (va.vaRenderPicture)(dpy, ctx, buffers.as_mut_ptr(), buffers.len() as i32) },
            "vaRenderPicture",
        );
        let ended = self
            .d
            .check(unsafe { (va.vaEndPicture)(dpy, ctx) }, "vaEndPicture");
        rendered.and(ended).map_err(err)
    }

    /// Wait for the frame and copy its bitstream out of the coded buffer.
    fn read_coded(&self) -> Result<Vec<u8>, CodecError> {
        let (va, dpy) = (self.d.va, self.d.dpy);
        self.d
            .check(
                unsafe { (va.vaSyncSurface)(dpy, self.input) },
                "vaSyncSurface",
            )
            .map_err(err)?;
        let mut p: *mut c_void = std::ptr::null_mut();
        self.d
            .check(
                unsafe { (va.vaMapBuffer)(dpy, self.coded, &mut p) },
                "map coded buffer",
            )
            .map_err(err)?;
        let mut out = Vec::new();
        let mut overflow = false;
        let mut seg = p as *const VACodedBufferSegment;
        let mut n = 0;
        while !seg.is_null() && n < 64 {
            // SAFETY: libva's list of segments, valid while the buffer is mapped; each
            // segment's `buf` holds `size` bytes.
            let s = unsafe { &*seg };
            overflow |= s.status & VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK != 0;
            if !s.buf.is_null() && s.size > 0 {
                out.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(s.buf as *const u8, s.size as usize)
                });
            }
            seg = s.next as *const VACodedBufferSegment;
            n += 1;
        }
        unsafe { (va.vaUnmapBuffer)(dpy, self.coded) };
        if overflow {
            return Err(err("the frame did not fit the coded buffer"));
        }
        Ok(out)
    }

    /// Encode two frames of a test picture and decode them with openh264: the stream
    /// must carry its headers and give the picture back.
    fn self_test(&mut self, openh264: &Path) -> Result<(), CodecError> {
        let (w, h) = (self.width as usize, self.height as usize);
        let nv12 = test_picture(w, h);
        let mut dec = crate::decode::VideoDecoder::software(openh264)?;
        let mut last = None;
        // At a generous rate: the stream's may be low for the size, and this checks the
        // plumbing, not the rate controller. The real rate goes out with the next frame.
        let bps = std::mem::replace(&mut self.bps, 20_000_000);
        let mut frames = || -> Result<(), CodecError> {
            for i in 0..2 {
                let au = self.encode_nv12(&nv12, None, i == 0)?;
                if i == 0 && crate::video::sequence_header(&au).is_none() {
                    return Err(err("the driver wrote no stream headers"));
                }
                if let Some(p) = dec.decode(&au)? {
                    last = Some(p);
                }
            }
            Ok(())
        };
        let encoded = frames();
        self.bps = bps;
        encoded?;
        let p = last.ok_or_else(|| err("openh264 decoded no picture from the self-test"))?;
        if (p.width, p.height) != (w, h) {
            return Err(err(format!(
                "self-test decoded {}x{}, not {w}x{h}",
                p.width, p.height
            )));
        }
        let db = psnr(p.y(), &nv12[..w * h]);
        if db < 25.0 {
            return Err(err(format!("self-test picture came back at {db:.1} dB")));
        }
        tracing::debug!(encoder = %self.name, w, h, psnr = db, "VA-API encoder passed its self-test");
        // The viewer never saw these frames.
        self.force_key = true;
        self.image_valid = false;
        Ok(())
    }
}

impl Drop for VaH264Encoder {
    fn drop(&mut self) {
        let (va, dpy) = (self.d.va, self.d.dpy);
        self.vpp = None;
        unsafe {
            if let Some(i) = self.image.take() {
                (va.vaDestroyImage)(dpy, i.image_id);
            }
            if self.coded != VA_INVALID_ID {
                (va.vaDestroyBuffer)(dpy, self.coded);
            }
            if self.context != VA_INVALID_ID {
                (va.vaDestroyContext)(dpy, self.context);
            }
            let mut surfaces: Vec<VASurfaceID> = [self.input, self.recon[0], self.recon[1]]
                .into_iter()
                .filter(|&s| s != VA_INVALID_SURFACE)
                .collect();
            if !surfaces.is_empty() {
                (va.vaDestroySurfaces)(dpy, surfaces.as_mut_ptr(), surfaces.len() as i32);
            }
            if self.config != VA_INVALID_ID {
                (va.vaDestroyConfig)(dpy, self.config);
            }
        }
    }
}

/// Copy rows `r0..r1` (even) of packed NV12 `src` (`w` x `h`) into a mapped VA image at
/// `base`, with the image's plane offsets and pitches.
fn write_nv12(
    image: &VAImage,
    base: *mut u8,
    src: &[u8],
    (w, h): (usize, usize),
    (r0, r1): (usize, usize),
) -> Result<(), String> {
    let size = image.data_size as usize;
    let (yo, yp) = (image.offsets[0] as usize, image.pitches[0] as usize);
    let (uo, up) = (image.offsets[1] as usize, image.pitches[1] as usize);
    let fits = image.num_planes >= 2
        && yp >= w
        && up >= w
        && yo + yp * (h - 1) + w <= size
        && uo + up * (h / 2 - 1) + w <= size;
    if !fits {
        return Err(format!(
            "NV12 image layout {:?}/{:?} ({} bytes) does not fit {w}x{h}",
            image.offsets, image.pitches, image.data_size
        ));
    }
    // SAFETY: mapped by the caller; every write below is within `data_size` (checked).
    let dst = unsafe { std::slice::from_raw_parts_mut(base, size) };
    for y in r0..r1 {
        dst[yo + y * yp..yo + y * yp + w].copy_from_slice(&src[y * w..(y + 1) * w]);
    }
    let uv = &src[w * h..];
    for cy in r0 / 2..r1 / 2 {
        dst[uo + cy * up..uo + cy * up + w].copy_from_slice(&uv[cy * w..(cy + 1) * w]);
    }
    Ok(())
}

/// The self-test picture (NV12): a diagonal luma ramp under coloured blocks.
pub(super) fn test_picture(w: usize, h: usize) -> Vec<u8> {
    let mut v = vec![0u8; w * h * 3 / 2];
    for y in 0..h {
        for x in 0..w {
            let block = ((x / 32) + (y / 32)) % 3;
            v[y * w + x] = (40 + ((x + y) * 160) / (w + h) + block * 20) as u8;
        }
    }
    for cy in 0..h / 2 {
        for cx in 0..w / 2 {
            let i = w * h + cy * w + cx * 2;
            v[i] = (64 + (cx * 128) / (w / 2).max(1)) as u8;
            v[i + 1] = (192 - (cy * 128) / (h / 2).max(1)) as u8;
        }
    }
    v
}

/// PSNR (dB) of `a` against `b`.
pub(super) fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let n = a.len().min(b.len()).max(1) as f64;
    let sum: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2))
        .sum();
    if sum == 0.0 {
        99.0
    } else {
        10.0 * (255.0f64 * 255.0 / (sum / n)).log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_fields_match_the_c_layout() {
        // chroma_format_idc 1 (bits 0-1), frame_mbs_only (2), direct_8x8 (5),
        // log2_max_frame_num_minus4 4 (6-9), poc type 2 (10-11), log2_max_poc_lsb_minus4
        // 4 (12-15).
        assert_eq!(
            seq_fields(),
            1 | 1 << 2 | 1 << 5 | 4 << 6 | 2 << 10 | 4 << 12
        );
        // idr (0), reference_pic_flag (1-2), deblocking control (9); CAVLC (bit 3 clear).
        assert_eq!(pic_fields(true), 1 | 1 << 1 | 1 << 9);
        assert_eq!(pic_fields(false), 1 << 1 | 1 << 9);
        assert_eq!(pic_fields(false) & (1 << 3), 0, "Baseline: no CABAC");
        // disable_frame_skip (1), disable_bit_stuffing (2); no reset (0).
        assert_eq!(rc_flags(), 0b110);
    }

    #[test]
    fn levels_follow_table_a1() {
        assert_eq!(level_idc(1280, 720, 30), 31);
        assert_eq!(level_idc(1920, 1080, 30), 40);
        assert_eq!(level_idc(1920, 1080, 60), 42);
        assert_eq!(level_idc(2560, 1440, 60), 51);
        assert_eq!(level_idc(3840, 2160, 30), 51);
        assert_eq!(level_idc(3840, 2160, 60), 52);
        assert_eq!(level_idc(7680, 4320, 30), 60);
        assert_eq!(level_idc(640, 480, 30), 30);
    }

    #[test]
    fn cropping_in_chroma_units() {
        assert_eq!(crop(1920, 1080), (0, 4)); // 1088 coded rows
        assert_eq!(crop(1280, 720), (0, 0));
        assert_eq!(crop(1366, 768), (5, 0)); // 1376 coded columns
    }

    #[test]
    fn misc_buffers_start_with_their_type() {
        let mut hrd: VAEncMiscParameterHRD = zeroed();
        hrd.buffer_size = 0x0102_0304;
        let b = misc(VAEncMiscParameterTypeHRD, &hrd);
        assert_eq!(b.len(), 4 + 24);
        assert_eq!(&b[..4], &5u32.to_ne_bytes());
        assert_eq!(&b[8..12], &0x0102_0304u32.to_ne_bytes());
    }

    #[test]
    fn nv12_rows_go_to_the_image_planes() {
        let (w, h) = (8usize, 4usize);
        let src: Vec<u8> = (0..(w * h * 3 / 2) as u8).collect();
        let mut image: VAImage = zeroed();
        image.num_planes = 2;
        image.pitches = [16, 16, 0];
        image.offsets = [4, 4 + 16 * 4, 0];
        image.data_size = 4 + 16 * 4 + 16 * 2;
        let mut mem = vec![0xEEu8; image.data_size as usize];
        write_nv12(&image, mem.as_mut_ptr(), &src, (w, h), (2, 4)).unwrap();
        assert_eq!(&mem[4..4 + 8], &[0xEE; 8], "row 0 not asked for");
        assert_eq!(&mem[4 + 32..4 + 40], &src[16..24], "row 2");
        assert_eq!(&mem[4 + 48..4 + 56], &src[24..32], "row 3");
        assert_eq!(&mem[4 + 40..4 + 48], &[0xEE; 8], "pitch padding untouched");
        let uv = 4 + 64;
        assert_eq!(
            &mem[uv + 16..uv + 24],
            &src[32 + 8..32 + 16],
            "chroma row 1"
        );
        assert_eq!(&mem[uv..uv + 8], &[0xEE; 8], "chroma row 0 not asked for");
        // A layout that doesn't fit is refused, not written past.
        image.data_size = 4 + 64 + 16 + 8 - 1; // one byte short of the last chroma row
        assert!(write_nv12(&image, mem.as_mut_ptr(), &src, (w, h), (0, 4)).is_err());
    }

    #[test]
    fn test_picture_is_nv12_sized_and_not_flat() {
        let p = test_picture(64, 32);
        assert_eq!(p.len(), 64 * 32 * 3 / 2);
        assert!(p[..64 * 32].iter().any(|&v| v != p[0]));
        assert!(psnr(&p, &p) > 90.0);
    }

    /// Needs an Intel/AMD GPU with a VA-API H.264 encoder, and openh264
    /// (`RCONN_OPENH264=<path to Cisco's libopenh264>`).
    #[test]
    #[ignore = "needs a VA-API capable GPU"]
    fn gpu_vaapi_encoder_round_trips_through_openh264() {
        let lib = std::env::var_os("RCONN_OPENH264").expect("RCONN_OPENH264");
        let mut e = VaH264Encoder::new(1366, 768, 30, 4_000_000, Some(Path::new(&lib)))
            .expect("VA-API encoder");
        let pic = test_picture(1366, 768);
        let key = e.encode_nv12(&pic, None, true).unwrap();
        assert!(crate::video::has_idr(&key));
        e.set_bitrate(1_000_000);
        let p = e.encode_nv12(&pic, Some((0, 2)), false).unwrap();
        assert!(
            !crate::video::has_idr(&p),
            "a live bitrate change makes no keyframe"
        );
        e.force_keyframe();
        assert!(crate::video::has_idr(
            &e.encode_nv12(&pic, None, false).unwrap()
        ));
        eprintln!("{}: {:?}", e.name(), e.stages());
        eprintln!("colours: {:?}", e.check_colours());
    }
}
