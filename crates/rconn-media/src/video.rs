//! H.264 encode/decode for screen content, on Cisco's openh264.

use crate::codec::CodecError;
use crate::yuv::I420;
use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode,
    SpsPpsStrategy, UsageType,
};
use openh264::formats::YUVSource;
use std::time::Instant;

/// A captured frame: tightly packed BGRA, `width * 4` bytes per row.
#[derive(Clone)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub bgra: Vec<u8>,
    pub captured_at: Instant,
}

impl Frame {
    /// H.264 (4:2:0) needs even dimensions; drop a trailing odd row/column.
    fn even(&self) -> (usize, usize) {
        (self.width & !1, self.height & !1)
    }
}

impl Frame {
    /// Halve the frame (2×2 box average) until it fits `max_w` × `max_h`. Halving keeps
    /// text crisp and costs one pass per step; HiDPI desktops usually need one step.
    pub fn fit_within(self, max_w: usize, max_h: usize) -> Frame {
        let mut f = self;
        while (f.width > max_w || f.height > max_h) && f.width >= 4 && f.height >= 4 {
            f = f.half();
        }
        f
    }

    fn half(&self) -> Frame {
        let (w, h) = (self.width / 2, self.height / 2);
        let stride = self.width * 4;
        let mut out = vec![0u8; w * h * 4];
        // Bands of output rows, one per core.
        let cores = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(8);
        let band = h.div_ceil(cores).max(1);
        std::thread::scope(|s| {
            for (i, dst_band) in out.chunks_mut(band * w * 4).enumerate() {
                let src = &self.bgra;
                s.spawn(move || {
                    for (j, dst) in dst_band.chunks_exact_mut(w * 4).enumerate() {
                        let y = i * band + j;
                        let r0 = &src[(2 * y) * stride..(2 * y) * stride + stride];
                        let r1 = &src[(2 * y + 1) * stride..(2 * y + 1) * stride + stride];
                        for x in 0..w {
                            for c in 0..4 {
                                let k = 8 * x + c;
                                let sum = u16::from(r0[k])
                                    + u16::from(r0[k + 4])
                                    + u16::from(r1[k])
                                    + u16::from(r1[k + 4]);
                                dst[4 * x + c] = ((sum + 2) / 4) as u8;
                            }
                        }
                    }
                });
            }
        });
        Frame {
            width: w,
            height: h,
            bgra: out,
            captured_at: self.captured_at,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncoderSettings {
    pub bitrate_bps: u32,
    pub max_fps: f32,
    /// Worker threads; 0 lets openh264 decide. Threads only help with several slices.
    pub threads: u16,
    /// Cut each frame into slices of at most this many bytes (lets openh264 encode slices
    /// on several cores, and keeps each slice within one network packet). None: one slice.
    pub slice_bytes: Option<u32>,
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self {
            bitrate_bps: 4_000_000,
            max_fps: 30.0,
            threads: 0,
            slice_bytes: None,
        }
    }
}

/// Screen-content H.264 encoder tuned for latency: no B-frames (Baseline profile via
/// openh264), low complexity, keyframes only on request, frame skipping under pressure.
/// The H.264 encoder doing the work.
enum Engine {
    OpenH264(Box<Encoder>),
    /// Windows' built-in encoder: 3.5-7x faster than openh264 on a measured 8-core VM,
    /// and the GPU's encoder where a vendor registers one.
    #[cfg(windows)]
    MediaFoundation(crate::mf::MfH264Encoder),
    /// Apple's encoder (the hardware one where there is one), in low-latency mode; it
    /// can also take the capturer's GPU surfaces without a copy.
    #[cfg(target_os = "macos")]
    VideoToolbox(Box<crate::vt_encode::VtEncoder>),
}

pub struct VideoEncoder {
    api_path: std::path::PathBuf,
    settings: EncoderSettings,
    dims: Option<(usize, usize)>,
    encoder: Option<Engine>,
    /// Name of the engine last built (for logs).
    engine_name: String,
    yuv: Option<I420>,
    /// The NV12 copy of the last picture (Media Foundation input), updated row-wise.
    nv12: Vec<u8>,
    /// The stream headers (SPS/PPS) of the current encoder.
    headers: Vec<u8>,
    /// The encoder for the previously used size (and its headers), kept for switching back.
    spare: Option<((usize, usize), Engine, Vec<u8>)>,
    /// Fraction of the captured size to encode (1.0 = full); lowered when the CPU is busy.
    scale: f32,
    last_was_key: bool,
    force_key: bool,
    /// macOS: openh264 rather than VideoToolbox, as asked for (`RCONN_ENCODER=openh264`,
    /// [`VideoEncoder::software`]) or because VideoToolbox failed.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    software_only: bool,
    /// VideoToolbox failures in a row.
    #[cfg(target_os = "macos")]
    vt_failures: u32,
}

impl VideoEncoder {
    /// Cisco's openh264 rather than VideoToolbox (comparisons, tests).
    #[cfg(target_os = "macos")]
    pub fn software(
        openh264_path: &std::path::Path,
        settings: EncoderSettings,
    ) -> Result<Self, CodecError> {
        let mut enc = Self::new(openh264_path, settings)?;
        enc.software_only = true;
        Ok(enc)
    }

    /// `openh264_path` is Cisco's shared library (see [`crate::cisco`]).
    pub fn new(
        openh264_path: &std::path::Path,
        settings: EncoderSettings,
    ) -> Result<Self, CodecError> {
        // Fail early if the binary is missing or not a known Cisco build.
        crate::codec::load_openh264(openh264_path)?;
        Ok(Self {
            api_path: openh264_path.to_owned(),
            settings,
            dims: None,
            encoder: None,
            engine_name: String::new(),
            yuv: None,
            nv12: Vec::new(),
            headers: Vec::new(),
            spare: None,
            scale: 1.0,
            last_was_key: false,
            force_key: false,
            software_only: std::env::var("RCONN_ENCODER").is_ok_and(|v| v == "openh264"),
            #[cfg(target_os = "macos")]
            vt_failures: 0,
        })
    }

    fn build(&mut self, width: usize, height: usize) -> Result<Engine, CodecError> {
        #[cfg(target_os = "macos")]
        if !self.software_only {
            match crate::vt_encode::VtEncoder::new(width, height, &self.settings) {
                Ok(vt) => {
                    self.engine_name = vt.name().to_owned();
                    return Ok(Engine::VideoToolbox(Box::new(vt)));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "VideoToolbox encoder unavailable; using openh264");
                    self.software_only = true;
                }
            }
        }
        #[cfg(windows)]
        match crate::mf::MfH264Encoder::new(
            width as u32,
            height as u32,
            self.settings.max_fps.round().max(1.0) as u32,
            self.settings.bitrate_bps,
        ) {
            Ok(mf) => {
                self.engine_name = format!("Media Foundation ({})", mf.name());
                return Ok(Engine::MediaFoundation(mf));
            }
            Err(e) => {
                tracing::warn!(error = %e, "Media Foundation encoder unavailable; using openh264")
            }
        }
        self.engine_name = "openh264".into();
        Ok(Engine::OpenH264(Box::new(
            self.build_openh264(width, height)?,
        )))
    }

    fn build_openh264(&self, width: usize, height: usize) -> Result<Encoder, CodecError> {
        let api: OpenH264API = crate::codec::load_openh264(&self.api_path)?;
        let cfg = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(self.settings.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(self.settings.max_fps))
            .complexity(Complexity::Low)
            // Never let quality fall apart (blocky text/images): cap the quantizer. Costs
            // bandwidth on busy screens, which screen sharing can afford.
            .qp(openh264::encoder::QpRange::new(10, 32))
            // Not supported for screen content; openh264 warns and disables them anyway.
            .adaptive_quantization(false)
            .background_detection(false)
            .skip_frames(true)
            // Keyframes come from RequestKeyframe/PLI, not a timer.
            .intra_frame_period(IntraFramePeriod::from_num_frames(0))
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .num_threads(self.settings.threads);
        let cfg = match self.settings.slice_bytes {
            Some(n) => cfg.max_slice_len(n),
            None => cfg,
        };
        let _ = (width, height); // dimensions are taken from the first frame by openh264
        Encoder::with_api_config(api, cfg).map_err(|e| CodecError::OpenH264(e.to_string()))
    }

    /// Encode at this fraction of the captured size (0.25..=1.0). Changing it restarts the
    /// encoder (one keyframe), so callers change it rarely.
    pub fn set_scale(&mut self, scale: f32) {
        self.scale = scale.clamp(0.25, 1.0);
    }

    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// The next encoded frame will be an IDR (after a decoder loss or a new viewer).
    pub fn request_keyframe(&mut self) {
        self.force_key = true;
    }

    pub fn settings(&self) -> EncoderSettings {
        self.settings
    }

    /// Change the target bitrate. Rebuilding costs one keyframe, so callers should only do
    /// it on meaningful changes (see `should_retarget`).
    pub fn set_bitrate(&mut self, bps: u32) {
        if bps == self.settings.bitrate_bps {
            return;
        }
        self.settings.bitrate_bps = bps;
        match &mut self.encoder {
            #[cfg(windows)]
            Some(Engine::MediaFoundation(mf)) => mf.set_bitrate(bps), // live, no keyframe
            #[cfg(target_os = "macos")]
            Some(Engine::VideoToolbox(vt)) => vt.set_bitrate(bps), // live, no keyframe
            _ => self.encoder = None, // openh264: rebuilt lazily; the first frame out is an IDR
        }
    }

    /// Which encoder is in use ("Media Foundation (...)" or "openh264").
    pub fn engine_name(&self) -> &str {
        &self.engine_name
    }

    /// Encode one frame into an Annex-B access unit. `None` means the rate controller
    /// skipped the frame.
    pub fn encode(&mut self, frame: &Frame) -> Result<Option<Vec<u8>>, CodecError> {
        let (w, h) = if self.scale < 1.0 {
            let s = |n: usize| ((n as f32 * self.scale) as usize) & !1;
            (s(frame.width), s(frame.height))
        } else {
            frame.even()
        };
        if w == 0 || h == 0 {
            return Ok(None);
        }
        let mut yuv = match self.yuv.take() {
            Some(y) if y.dimensions() == (w, h) => y,
            _ => I420::new(w, h),
        };
        // Odd edges are cropped by converting only the even-sized top-left region.
        yuv.fill_from_bgra_scaled(&frame.bgra, frame.width, frame.height);
        let out = self.encode_yuv(&yuv, None);
        self.yuv = Some(yuv);
        out
    }

    /// Encode an already converted picture. `changed_rows` (an even range) are the only
    /// rows that differ from the previous picture given here; `None` means "any".
    pub fn encode_yuv(
        &mut self,
        yuv: &I420,
        changed_rows: Option<(usize, usize)>,
    ) -> Result<Option<Vec<u8>>, CodecError> {
        let (w, h) = yuv.dimensions();
        self.ensure_engine(w, h)?;
        let force_key = std::mem::take(&mut self.force_key);
        let out = match self.encoder.as_mut().expect("built above") {
            Engine::OpenH264(encoder) => {
                if force_key {
                    encoder.force_intra_frame();
                }
                let bits = encoder
                    .encode(yuv)
                    .map_err(|e| CodecError::OpenH264(e.to_string()))?;
                self.last_was_key = matches!(
                    bits.frame_type(),
                    openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I
                );
                bits.to_vec()
            }
            #[cfg(windows)]
            Engine::MediaFoundation(mf) => {
                if force_key {
                    mf.force_keyframe();
                }
                yuv.write_nv12(&mut self.nv12, changed_rows);
                let out = mf.encode(&self.nv12)?;
                self.last_was_key = has_idr(&out);
                out
            }
            #[cfg(target_os = "macos")]
            Engine::VideoToolbox(vt) => match vt.encode_i420(yuv, changed_rows, force_key) {
                Ok(au) => self.vt_output(au),
                Err(e) => {
                    // A new session (or openh264) gets the whole picture again.
                    self.vt_failed(&e, force_key);
                    return self.encode_yuv(yuv, None);
                }
            },
        };
        let _ = changed_rows; // only the NV12 paths can use it
        Ok(self.finish(out, force_key))
    }

    /// Can this encoder take the capturer's GPU surfaces as they are (VideoToolbox)?
    pub fn wants_surfaces(&self) -> bool {
        cfg!(target_os = "macos") && !self.software_only
    }

    /// Encode a captured GPU surface. VideoToolbox reads it where it is (no copy); a
    /// software encoder gets a CPU conversion of it.
    pub fn encode_surface(
        &mut self,
        surface: &crate::capture::Surface,
    ) -> Result<Option<Vec<u8>>, CodecError> {
        let (w, h) = surface.dimensions();
        let (w, h) = (w & !1, h & !1);
        if w == 0 || h == 0 {
            return Ok(None);
        }
        #[cfg(target_os = "macos")]
        {
            if !self.software_only {
                self.ensure_engine(w, h)?;
            }
            if let Some(Engine::VideoToolbox(vt)) = self.encoder.as_mut() {
                let force_key = std::mem::take(&mut self.force_key);
                return match vt.encode_surface(surface, force_key) {
                    Ok(au) => {
                        let out = self.vt_output(au);
                        Ok(self.finish(out, force_key))
                    }
                    Err(e) => {
                        self.vt_failed(&e, force_key);
                        self.encode_surface(surface)
                    }
                };
            }
            let mut yuv = self.yuv.take();
            if !surface.to_i420(&mut yuv) {
                return Err(CodecError::VideoToolbox(
                    "could not read the captured surface".into(),
                ));
            }
            let yuv = yuv.expect("allocated by to_i420");
            let out = self.encode_yuv(&yuv, None);
            self.yuv = Some(yuv);
            out
        }
        #[cfg(not(target_os = "macos"))]
        match *surface {}
    }

    /// Make sure the engine encodes `w` x `h` pictures.
    fn ensure_engine(&mut self, w: usize, h: usize) -> Result<(), CodecError> {
        if self.dims != Some((w, h)) || self.encoder.is_none() {
            // Switching between two sizes (full / half while things move) reuses the
            // encoder kept for the other size; its next frame must be a keyframe.
            let previous = self.encoder.take().zip(self.dims);
            let old_headers = std::mem::take(&mut self.headers);
            let engine = match self.spare.take() {
                Some((dims, engine, headers)) if dims == (w, h) => {
                    self.force_key = true;
                    self.headers = headers;
                    engine
                }
                _ => self.build(w, h)?,
            };
            self.spare = previous.map(|(e, d)| (d, e, old_headers));
            self.encoder = Some(engine);
            self.dims = Some((w, h));
            self.nv12.clear(); // forces a full NV12 rewrite
            // A kept encoder may still have an older bitrate.
            #[cfg(windows)]
            if let Some(Engine::MediaFoundation(mf)) = &mut self.encoder {
                mf.set_bitrate(self.settings.bitrate_bps);
            }
            #[cfg(target_os = "macos")]
            if let Some(Engine::VideoToolbox(vt)) = &mut self.encoder {
                vt.set_bitrate(self.settings.bitrate_bps);
                vt.forget_picture(); // a kept session's copy predates the rows reported
            }
        }
        Ok(())
    }

    /// A VideoToolbox frame out: `None` (dropped) becomes an empty unit.
    #[cfg(target_os = "macos")]
    fn vt_output(&mut self, au: Option<(Vec<u8>, bool)>) -> Vec<u8> {
        self.vt_failures = 0;
        let (out, key) = au.unwrap_or_default();
        self.last_was_key = key;
        out
    }

    /// VideoToolbox failed to encode (its session dies on sleep/wake or a GPU change):
    /// drop the session so the next frame makes a new one. After a few failures in a row,
    /// openh264 takes over for good.
    #[cfg(target_os = "macos")]
    fn vt_failed(&mut self, error: &str, force_key: bool) {
        self.vt_failures += 1;
        self.encoder = None;
        self.spare = None;
        self.force_key |= force_key;
        if self.vt_failures >= 3 {
            tracing::warn!(error, "VideoToolbox keeps failing; using openh264");
            self.software_only = true;
        } else {
            tracing::warn!(error, "VideoToolbox encode failed; starting a new session");
        }
    }

    /// Bookkeeping after a frame: a keyframe still owed, and stream headers in front of
    /// keyframes. `None` for an empty unit (a skipped frame).
    fn finish(&mut self, out: Vec<u8>, force_key: bool) -> Option<Vec<u8>> {
        // A keyframe asked for (a viewer lost its picture) is owed until one comes out: the
        // rate controller may skip the frame it was asked of, and a viewer without a
        // picture has nothing to show until it gets one.
        if force_key && !self.last_was_key {
            self.force_key = true;
            // Media Foundation's "force keyframe" is a request an encoder may ignore (the
            // property is optional, and its result is not checked). A picture came out and
            // it is not a keyframe: start a new encoder, whose first frame always is.
            #[cfg(windows)]
            if !out.is_empty() && matches!(self.encoder, Some(Engine::MediaFoundation(_))) {
                tracing::warn!(
                    engine = %self.engine_name,
                    "encoder ignored a keyframe request; restarting it"
                );
                self.encoder = None;
            }
        }
        let out = with_headers(out, &mut self.headers);
        if out.is_empty() { None } else { Some(out) }
    }

    /// The VideoToolbox session in use, if any.
    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn vt_for_test(&mut self) -> Option<&mut crate::vt_encode::VtEncoder> {
        match self.encoder.as_mut() {
            Some(Engine::VideoToolbox(vt)) => Some(vt),
            _ => None,
        }
    }

    /// Was the last encoded frame a keyframe? (Keyframes are always slow and large.)
    pub fn last_was_key(&self) -> bool {
        self.last_was_key
    }
}

/// Does this Annex-B access unit contain an IDR slice (NAL type 5)?
pub fn has_idr(annex_b: &[u8]) -> bool {
    annex_b
        .windows(4)
        .any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 5)
}

/// The NAL units of an Annex-B stream, without start codes.
pub(crate) fn nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
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
            // Trim the next start code, and the zero before it if it was 4 bytes long.
            let mut e = s - 3;
            while e > 0 && data[e - 1] == 0 {
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

/// The sequence parameter set (SPS) in `annex_b`, if any (without its start code).
pub(crate) fn sequence_header(annex_b: &[u8]) -> Option<&[u8]> {
    nal_units(annex_b).find(|nal| nal[0] & 0x1f == 7)
}

/// The SPS and PPS of `annex_b` (with start codes), if it carries both.
fn parameter_sets(annex_b: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut sps, mut pps) = (false, false);
    for nal in nal_units(annex_b) {
        match nal[0] & 0x1f {
            7 => sps = true,
            8 => pps = true,
            _ => continue,
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    (sps && pps).then_some(out)
}

/// Make every keyframe self-contained: remember the stream headers (SPS/PPS) an encoder
/// sends, and put them in front of any keyframe that comes without them. A decoder that
/// lost its place, or a stream that switched encoders (and picture size), can only start
/// again from a keyframe it can read on its own.
fn with_headers(out: Vec<u8>, headers: &mut Vec<u8>) -> Vec<u8> {
    if let Some(h) = parameter_sets(&out) {
        *headers = h;
        return out;
    }
    if !headers.is_empty() && has_idr(&out) {
        let mut v = headers.clone();
        v.extend_from_slice(&out);
        return v;
    }
    out
}

/// Should a bandwidth estimate move the encoder? Only on a >25% change, so GCC jitter
/// doesn't trigger a keyframe storm.
pub fn should_retarget(current_bps: u32, estimate_bps: u32) -> bool {
    let (c, e) = (f64::from(current_bps), f64::from(estimate_bps));
    c > 0.0 && ((e - c).abs() / c) > 0.25
}

/// A decoded frame, RGBA.
pub struct DecodedFrame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// H.264 decoder: Apple's hardware decoder (VideoToolbox) where available, Cisco's
/// openh264 otherwise. `RCONN_DECODER=openh264` forces software, for comparisons.
pub struct VideoDecoder {
    engine: DecEngine,
    /// For a fresh software decoder per stream, or falling back to software if the
    /// hardware decoder refuses a stream.
    openh264_path: std::path::PathBuf,
    /// The sequence header (SPS) of the stream being decoded.
    sps: Vec<u8>,
}

enum DecEngine {
    OpenH264(Box<Decoder>),
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    VideoToolbox(crate::vt::VtDecoder),
}

impl VideoDecoder {
    pub fn new(openh264_path: &std::path::Path) -> Result<Self, CodecError> {
        let software = std::env::var("RCONN_DECODER").is_ok_and(|v| v == "openh264");
        let engine = if cfg!(any(target_os = "macos", target_os = "ios")) && !software {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            {
                DecEngine::VideoToolbox(crate::vt::VtDecoder::new())
            }
            #[cfg(not(any(target_os = "macos", target_os = "ios")))]
            unreachable!()
        } else {
            Self::openh264(openh264_path)?
        };
        Ok(Self {
            engine,
            openh264_path: openh264_path.to_owned(),
            sps: Vec::new(),
        })
    }

    /// Cisco's openh264 on every platform (comparisons, tests).
    pub fn software(openh264_path: &std::path::Path) -> Result<Self, CodecError> {
        Ok(Self {
            engine: Self::openh264(openh264_path)?,
            openh264_path: openh264_path.to_owned(),
            sps: Vec::new(),
        })
    }

    fn openh264(path: &std::path::Path) -> Result<DecEngine, CodecError> {
        let api = crate::codec::load_openh264(path)?;
        let decoder = Decoder::with_api_config(api, DecoderConfig::new())
            .map_err(|e| CodecError::OpenH264(e.to_string()))?;
        Ok(DecEngine::OpenH264(Box::new(decoder)))
    }

    pub fn engine_name(&self) -> &'static str {
        match self.engine {
            DecEngine::OpenH264(_) => "openh264 (software)",
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            DecEngine::VideoToolbox(_) => "VideoToolbox (hardware)",
        }
    }

    /// Decode one Annex-B access unit. `None` until the decoder has a full picture.
    ///
    /// A new sequence header (the host restarted its encoder: a new screen size, a new
    /// capture after a desktop switch) gets a fresh software decoder, so nothing of the
    /// old stream (its size, its reference pictures, a header replaced half-way) can keep
    /// the new one from showing. VideoToolbox opens a new session for it ([`crate::vt`]).
    pub fn decode(&mut self, annex_b: &[u8]) -> Result<Option<DecodedFrame>, CodecError> {
        if let Some(sps) = sequence_header(annex_b)
            && sps != self.sps.as_slice()
        {
            let restart = !self.sps.is_empty();
            self.sps = sps.to_vec();
            if restart && matches!(self.engine, DecEngine::OpenH264(_)) {
                self.engine = Self::openh264(&self.openh264_path)?;
            }
        }
        match &mut self.engine {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            DecEngine::VideoToolbox(vt) => {
                let mut rgba = Vec::new();
                match vt.decode(annex_b, &mut rgba) {
                    Ok(Some((width, height))) => Ok(Some(DecodedFrame {
                        width,
                        height,
                        rgba,
                    })),
                    Ok(None) => Ok(None),
                    Err(e) if e.starts_with("VTDecompressionSessionCreate") => {
                        // No hardware decoder for this stream: software from here on.
                        tracing::warn!(error = %e, "hardware decoder unavailable; using openh264");
                        self.engine = Self::openh264(&self.openh264_path)?;
                        self.decode(annex_b)
                    }
                    Err(e) => Err(CodecError::VideoToolbox(e)),
                }
            }
            DecEngine::OpenH264(decoder) => {
                let Some(yuv) = decoder
                    .decode(annex_b)
                    .map_err(|e| CodecError::OpenH264(e.to_string()))?
                else {
                    return Ok(None);
                };
                let (width, height) = yuv.dimensions();
                let mut rgba = vec![0u8; width * height * 4];
                // Our conversion uses every core (openh264's uses one: ~20 ms at 2992x1878).
                crate::yuv::i420_to_rgba(
                    (yuv.y(), yuv.u(), yuv.v()),
                    yuv.strides(),
                    (width, height),
                    &mut rgba,
                );
                Ok(Some(DecodedFrame {
                    width,
                    height,
                    rgba,
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annex_b_with_3_and_4_byte_start_codes() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5,
        ];
        let nals: Vec<&[u8]> = nal_units(&data).collect();
        assert_eq!(nals, [&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4, 5]]);
    }

    #[test]
    fn keyframes_always_carry_the_stream_headers() {
        let mut headers = Vec::new();
        let first = vec![
            0, 0, 0, 1, 0x67, 9, 0, 0, 0, 1, 0x68, 8, 0, 0, 0, 1, 0x65, 1,
        ];
        assert_eq!(
            with_headers(first.clone(), &mut headers),
            first,
            "already complete"
        );
        let p_frame = vec![0, 0, 0, 1, 0x41, 2];
        assert_eq!(
            with_headers(p_frame.clone(), &mut headers),
            p_frame,
            "untouched"
        );
        let bare_idr = vec![0, 0, 0, 1, 0x65, 3];
        let fixed = with_headers(bare_idr, &mut headers);
        assert_eq!(
            fixed,
            [
                0, 0, 0, 1, 0x67, 9, 0, 0, 0, 1, 0x68, 8, 0, 0, 0, 1, 0x65, 3
            ],
            "headers put back in front"
        );
    }

    #[test]
    fn retarget_threshold() {
        assert!(!should_retarget(4_000_000, 4_500_000));
        assert!(should_retarget(4_000_000, 2_000_000));
        assert!(should_retarget(4_000_000, 6_000_000));
        assert!(!should_retarget(0, 1));
    }

    #[test]
    fn fit_within_halves_until_it_fits() {
        let f = Frame {
            width: 2992,
            height: 1878,
            bgra: vec![200; 2992 * 1878 * 4],
            captured_at: Instant::now(),
        };
        let g = f.fit_within(1920, 1080);
        assert_eq!((g.width, g.height), (1496, 939));
        assert!(g.bgra.iter().all(|&b| b == 200), "flat input stays flat");
        let small = Frame {
            width: 800,
            height: 600,
            bgra: vec![1; 800 * 600 * 4],
            captured_at: Instant::now(),
        };
        assert_eq!(
            small.fit_within(1920, 1080).width,
            800,
            "already fits: untouched"
        );
    }
}

#[cfg(test)]
mod usage_bench {
    //! `cargo test --release -p rconn-media usage_bench -- --ignored --nocapture`
    use super::*;
    use openh264::encoder::UsageType;

    /// Photo/video-like frames: smooth gradients plus noise that changes every frame.
    fn natural_frames(w: usize, h: usize, n: usize) -> Vec<Frame> {
        let mut seed = 0x1234_5678_u32;
        (0..n)
            .map(|k| {
                let mut bgra = vec![0u8; w * h * 4];
                for (i, px) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let (x, y) = (i % w, i / w);
                    let noise = (seed >> 24) as usize % 40;
                    *px = [
                        ((x + k * 7) % 256 + noise).min(255) as u8,
                        ((y + k * 3) % 256) as u8,
                        ((x ^ y) % 200 + noise) as u8,
                        255,
                    ];
                }
                Frame {
                    width: w,
                    height: h,
                    bgra,
                    captured_at: Instant::now(),
                }
            })
            .collect()
    }

    #[test]
    #[ignore = "timing, run by hand"]
    fn screen_vs_camera_mode_on_natural_content() {
        let Some(lib) = crate::cisco::local_binary_path() else {
            return;
        };
        let frames = natural_frames(1496, 938, 30);
        for usage in [
            UsageType::ScreenContentRealTime,
            UsageType::CameraVideoRealTime,
        ] {
            let api = crate::codec::load_openh264(&lib).unwrap();
            let cfg = EncoderConfig::new()
                .usage_type(usage)
                .rate_control_mode(RateControlMode::Bitrate)
                .bitrate(BitRate::from_bps(4_000_000))
                .max_frame_rate(FrameRate::from_hz(30.0))
                .complexity(Complexity::Low)
                .skip_frames(true)
                .intra_frame_period(IntraFramePeriod::from_num_frames(0));
            let mut enc = Encoder::with_api_config(api, cfg).unwrap();
            let mut yuv = crate::yuv::I420::new(1496, 938);
            let t = Instant::now();
            for f in &frames {
                yuv.fill_from_bgra(&f.bgra, f.width);
                let _ = enc.encode(&yuv).unwrap().to_vec();
            }
            println!(
                "{usage:?}: {:.1} ms/frame",
                t.elapsed().as_secs_f64() * 1e3 / frames.len() as f64
            );
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod hardware_decode {
    //! `cargo test --release -p rconn-media hardware_decode -- --nocapture` prints speed.
    use super::*;

    /// A 2992x1878 desktop-like stream: text-ish stripes that scroll every frame.
    fn stream(lib: &std::path::Path, n: usize) -> Vec<Vec<u8>> {
        let (w, h) = (2992, 1878);
        let mut enc = VideoEncoder::new(
            lib,
            EncoderSettings {
                bitrate_bps: 8_000_000,
                max_fps: 30.0,
                threads: 0,
                slice_bytes: None,
            },
        )
        .unwrap();
        (0..n)
            .filter_map(|k| {
                let mut bgra = vec![0u8; w * h * 4];
                for (i, px) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    let (x, y) = (i % w, i / w + k * 9);
                    let ink = (y / 14) % 3 != 0 && (x / 7 + y / 14) % 5 != 0;
                    let c = if ink { 40 } else { 235 };
                    *px = [
                        c,
                        (c as usize + x / 12) as u8,
                        (c as usize + y % 200) as u8,
                        255,
                    ];
                }
                enc.encode(&Frame {
                    width: w,
                    height: h,
                    bgra,
                    captured_at: Instant::now(),
                })
                .unwrap()
            })
            .collect()
    }

    /// Pictures, and steady-state ms per frame (the first two frames start the decoder).
    fn decode_all(dec: &mut VideoDecoder, aus: &[Vec<u8>]) -> (Vec<DecodedFrame>, f64) {
        let mut frames = Vec::new();
        let mut t = Instant::now();
        for (i, au) in aus.iter().enumerate() {
            if i == 2 {
                t = Instant::now();
            }
            frames.extend(dec.decode(au).unwrap());
        }
        let ms = t.elapsed().as_secs_f64() * 1e3 / (aus.len() - 2) as f64;
        (frames, ms)
    }

    #[test]
    fn matches_software_decoder_and_reports_speed() {
        let Some(lib) = crate::cisco::local_binary_path() else {
            return;
        };
        let aus = stream(&lib, 40);
        let mut hw = VideoDecoder::new(&lib).unwrap();
        assert_eq!(hw.engine_name(), "VideoToolbox (hardware)");
        let mut sw = VideoDecoder::software(&lib).unwrap();
        let (a, hw_ms) = decode_all(&mut hw, &aus);
        let (b, sw_ms) = decode_all(&mut sw, &aus);
        assert_eq!(hw.engine_name(), "VideoToolbox (hardware)", "no fallback");
        assert_eq!(a.len(), b.len());
        assert_eq!(a.len(), aus.len(), "every unit gives a picture");
        for (x, y) in a.iter().zip(&b) {
            assert_eq!((x.width, x.height), (y.width, y.height));
            let diff: u64 = x
                .rgba
                .iter()
                .zip(&y.rgba)
                .map(|(p, q)| u64::from(p.abs_diff(*q)))
                .sum();
            let mean = diff as f64 / x.rgba.len() as f64;
            if mean >= 3.0 {
                for i in [0usize, 1001, 50_000, 2_000_000] {
                    println!(
                        "px {i}: hw {:?} sw {:?}",
                        &x.rgba[i * 4..i * 4 + 4],
                        &y.rgba[i * 4..i * 4 + 4]
                    );
                }
            }
            assert!(mean < 3.0, "hardware picture differs: mean {mean:.2}");
        }
        println!(
            "{}x{}: VideoToolbox {hw_ms:.1} ms/frame, openh264 {sw_ms:.1} ms/frame",
            a[0].width, a[0].height
        );
    }
}
