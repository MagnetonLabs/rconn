//! H.264 encode/decode for screen content, on Cisco's openh264.

use crate::codec::CodecError;
use crate::h264::SyntaxTracker;
pub use crate::recovery::FrameInfo;
use crate::recovery::{Ledger, Sent};
use crate::yuv::I420;
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode,
    SpsPpsStrategy, UsageType,
};
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
    OpenH264(Box<OpenH264>),
    /// Windows' built-in encoder: 3.5-7x faster than openh264 on a measured 8-core VM,
    /// and the GPU's encoder where a vendor registers one.
    #[cfg(windows)]
    MediaFoundation(crate::mf::MfH264Encoder),
    /// Apple's encoder (the hardware one where there is one), in low-latency mode; it
    /// can also take the capturer's GPU surfaces without a copy.
    #[cfg(target_os = "macos")]
    VideoToolbox(Box<crate::vt_encode::VtEncoder>),
}

/// An openh264 encoder and the rate it was last told.
struct OpenH264 {
    encoder: Encoder,
    /// Bits/s last applied (at build, or live with [`crate::oh264::set_bitrate`]).
    bps: u32,
    /// Has encoded a frame: openh264 initializes on its first frame, and options set
    /// before that are lost.
    started: bool,
    /// Built in long-term-reference mode (see [`VideoEncoder::set_long_term_refs`]).
    ltr: bool,
}

/// Frames between long-term reference marks on Media Foundation (~1 s at 30 fps; openh264
/// marks every 30 frames on its own).
#[cfg(windows)]
const MF_LTR_PERIOD: u32 = 30;

/// Per-frame loss-repair state between the start of an encode and [`VideoEncoder::finish`].
#[derive(Default)]
struct Repair {
    /// The loss report being answered (last good frame id).
    asked: Option<u32>,
    /// The long-term reference the frame predicts from.
    base: Option<Sent>,
    /// Media Foundation: the slot the frame was marked into, and the slots the encoder
    /// says it predicted from.
    marked: Option<u8>,
    used: Option<u16>,
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
    /// openh264 in long-term-reference mode for engines built from now on.
    openh264_ltr: bool,
    /// Reads our own output: frame numbers and reference marks for loss repair.
    syntax: SyntaxTracker,
    ledger: Ledger,
    /// Id of the next frame out.
    next_id: u32,
    last: FrameInfo,
    /// A loss was reported after this frame: repair at the next encode.
    repair: Option<u32>,
    /// Frames since the last long-term mark (Media Foundation).
    #[cfg_attr(not(windows), allow(dead_code))]
    since_mark: u32,
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
            openh264_ltr: std::env::var("RCONN_OPENH264_LTR").is_ok_and(|v| v == "1"),
            syntax: SyntaxTracker::default(),
            ledger: Ledger::default(),
            next_id: 0,
            last: FrameInfo::default(),
            repair: None,
            since_mark: 0,
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
        self.engine_name = if self.openh264_ltr {
            "openh264 (long-term references)".into()
        } else {
            "openh264".into()
        };
        Ok(Engine::OpenH264(Box::new(OpenH264 {
            encoder: self.build_openh264(width, height)?,
            bps: self.settings.bitrate_bps,
            started: false,
            ltr: self.openh264_ltr,
        })))
    }

    fn build_openh264(&self, width: usize, height: usize) -> Result<Encoder, CodecError> {
        let api: OpenH264API = crate::codec::load_openh264(&self.api_path)?;
        // Long-term references (loss repair without a keyframe) only work in openh264's
        // camera mode: in screen mode it switches them off ("not supported yet"). Camera
        // mode measured 3.7x faster per frame, ~25 % more bytes on scrolling and static
        // text (`ltr_tests::compare_screen_and_ltr_modes`), so it is opt-in:
        // `RCONN_OPENH264_LTR=1`.
        let (usage, ltr) = if self.openh264_ltr {
            (UsageType::CameraVideoRealTime, true)
        } else {
            (UsageType::ScreenContentRealTime, false)
        };
        let cfg = EncoderConfig::new()
            .usage_type(usage)
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
            .long_term_reference(ltr)
            // Camera mode would add IDRs of its own on big changes (scrolling).
            .scene_change_detect(!ltr)
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

    /// Change the target bitrate, live on every engine: no restart, no keyframe. (Before
    /// 0.1.6 openh264 was rebuilt for it, and every retarget cost a full-size IDR.)
    pub fn set_bitrate(&mut self, bps: u32) {
        if bps == self.settings.bitrate_bps {
            return;
        }
        self.settings.bitrate_bps = bps;
        self.apply_bitrate();
    }

    /// Bring the current engine to the configured bitrate (also after switching to the
    /// spare encoder, which may have an older one).
    fn apply_bitrate(&mut self) {
        let bps = self.settings.bitrate_bps;
        match &mut self.encoder {
            Some(Engine::OpenH264(o)) if o.started && o.bps != bps => {
                if !crate::oh264::set_bitrate(&mut o.encoder, o.bps, bps) {
                    tracing::warn!(bps, "openh264 refused a live bitrate change");
                }
                o.bps = bps; // not retried every frame; the next change tries again
            }
            #[cfg(windows)]
            Some(Engine::MediaFoundation(mf)) => mf.set_bitrate(bps), // live, no keyframe
            #[cfg(target_os = "macos")]
            Some(Engine::VideoToolbox(vt)) => vt.set_bitrate(bps), // live, no keyframe
            _ => {}
        }
    }

    /// Which encoder is in use ("Media Foundation (...)" or "openh264").
    pub fn engine_name(&self) -> &str {
        &self.engine_name
    }

    /// openh264 engines built from now on use long-term references (camera mode), so
    /// losses can be repaired without a keyframe. Default: `RCONN_OPENH264_LTR=1`.
    pub fn set_long_term_refs(&mut self, on: bool) {
        self.openh264_ltr = on;
    }

    /// Can a reported loss be repaired without a keyframe ([`Self::request_ltr_recovery`])?
    /// VideoToolbox: not yet (its LTR properties are not wired; repairs are keyframes).
    pub fn supports_ltr(&self) -> bool {
        match &self.encoder {
            Some(Engine::OpenH264(o)) => o.ltr,
            #[cfg(windows)]
            Some(Engine::MediaFoundation(mf)) => mf.ltr_slots() > 0,
            #[cfg(target_os = "macos")]
            Some(Engine::VideoToolbox(_)) => false,
            None => self.openh264_ltr,
        }
    }

    /// Frame ids continue from `id` (a new encoder within the same stream).
    pub fn start_frame_ids_at(&mut self, id: u32) {
        self.next_id = id;
    }

    /// The last frame put out.
    pub fn last_frame(&self) -> FrameInfo {
        self.last
    }

    /// The viewer decoded every frame up to `id` with nothing missing: the long-term
    /// references among them may be predicted from.
    pub fn ltr_acked(&mut self, id: u32) {
        for s in self.ledger.ack(id) {
            // openh264 trusts an IDR on its own; later marks need the viewer's word, by
            // the frame number of the picture marked (with delayed marking, an earlier one).
            if let (Some(Engine::OpenH264(o)), Some(slice)) = (&mut self.encoder, s.slice)
                && o.ltr
                && !slice.idr
                && let Some(marked) = slice.marks_long_term
            {
                crate::oh264::ltr_received(&mut o.encoder, slice.idr_pic_id, marked);
            }
        }
    }

    /// The viewer lost frames after `last_good`: the next frame repairs it by predicting
    /// from a long-term reference the viewer has, or is an IDR if there is none.
    pub fn request_ltr_recovery(&mut self, last_good: u32) {
        self.repair = Some(last_good);
    }

    /// Plan the repair for the frame about to be encoded: `Some(base)` if it will predict
    /// from long-term reference `base`, `None` if it must be a keyframe.
    fn plan_repair(&mut self, last_good: u32) -> Option<Sent> {
        self.ledger.loss_after(last_good);
        let good = *self.ledger.get(last_good)?;
        if good.instance != self.ledger.instance() {
            return None; // from an encoder since replaced
        }
        let base = self.ledger.base_for(last_good)?;
        match self.encoder.as_mut()? {
            Engine::OpenH264(o) if o.ltr => {
                let (good, newest) = (good.slice?, self.ledger.newest()?.slice?);
                crate::oh264::ltr_recover(
                    &mut o.encoder,
                    good.idr_pic_id,
                    good.frame_num,
                    newest.frame_num,
                );
                Some(base)
            }
            #[cfg(windows)]
            Engine::MediaFoundation(mf) => mf.use_ltr(base.ltr_slot?).then_some(base),
            _ => None,
        }
    }

    /// Start a frame: the live bitrate, a pending loss repair, a keyframe asked for.
    fn begin_frame(&mut self) -> (bool, Repair) {
        self.apply_bitrate();
        let mut repair = Repair {
            asked: self.repair.take(),
            ..Repair::default()
        };
        if let Some(last_good) = repair.asked {
            repair.base = self.plan_repair(last_good);
            if repair.base.is_none() {
                self.force_key = true; // nothing to repair from
            }
        }
        (std::mem::take(&mut self.force_key), repair)
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
        #[cfg_attr(not(windows), allow(unused_mut))]
        let (force_key, mut repair) = self.begin_frame();
        let out = match self.encoder.as_mut().expect("built above") {
            Engine::OpenH264(o) => {
                if force_key {
                    o.encoder.force_intra_frame();
                }
                let bits = o
                    .encoder
                    .encode(yuv)
                    .map_err(|e| CodecError::OpenH264(e.to_string()))?;
                self.last_was_key = matches!(
                    bits.frame_type(),
                    openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I
                );
                let out = bits.to_vec();
                if !o.started {
                    o.started = true;
                    // Changed before the first frame, the rate would have been lost.
                    let bps = self.settings.bitrate_bps;
                    if o.bps != bps && crate::oh264::set_bitrate(&mut o.encoder, o.bps, bps) {
                        o.bps = bps;
                    }
                }
                out
            }
            #[cfg(windows)]
            Engine::MediaFoundation(mf) => {
                if force_key {
                    mf.force_keyframe();
                }
                // Long-term references: the keyframe into slot 0 (a keyframe empties
                // them), then about once a second into a slot not holding the newest
                // picture the viewer confirmed.
                let slots = mf.ltr_slots();
                if slots > 0 && repair.base.is_none() {
                    let slot = if force_key || self.ledger.newest().is_none() {
                        Some(0)
                    } else if self.since_mark >= MF_LTR_PERIOD {
                        Some(self.ledger.free_slot(slots))
                    } else {
                        None
                    };
                    if let Some(slot) = slot
                        && mf.mark_ltr(slot)
                    {
                        repair.marked = Some(slot);
                    }
                }
                yuv.write_nv12(&mut self.nv12, changed_rows);
                let out = mf.encode(&self.nv12)?;
                self.last_was_key = has_idr(&out);
                // Where the encoder reports what it did, its word wins.
                if let Some((m, u)) = mf.last_ltr_info() {
                    repair.marked = m;
                    repair.used = Some(u);
                }
                out
            }
            #[cfg(target_os = "macos")]
            Engine::VideoToolbox(vt) => match vt.encode_i420(yuv, changed_rows, force_key) {
                Ok(au) => self.vt_output(au),
                Err(e) => {
                    // A new session (or openh264) gets the whole picture again.
                    self.vt_failed(&e, force_key);
                    self.repair = self.repair.or(repair.asked);
                    return self.encode_yuv(yuv, None);
                }
            },
        };
        let _ = changed_rows; // only the NV12 paths can use it
        Ok(self.finish(out, force_key, repair))
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
            if matches!(self.encoder, Some(Engine::VideoToolbox(_))) {
                let (force_key, repair) = self.begin_frame();
                let Some(Engine::VideoToolbox(vt)) = self.encoder.as_mut() else {
                    unreachable!("checked above");
                };
                return match vt.encode_surface(surface, force_key) {
                    Ok(au) => {
                        let out = self.vt_output(au);
                        Ok(self.finish(out, force_key, repair))
                    }
                    Err(e) => {
                        self.vt_failed(&e, force_key);
                        self.repair = self.repair.or(repair.asked);
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
            // References never cross encoders.
            self.ledger.new_instance();
            self.since_mark = 0;
            // A kept encoder may still have an older bitrate (applied in `begin_frame`).
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

    /// Bookkeeping after a frame: a keyframe still owed, stream headers in front of
    /// keyframes, the frame's id and references. `None` for an empty unit (a skipped
    /// frame).
    fn finish(&mut self, out: Vec<u8>, force_key: bool, repair: Repair) -> Option<Vec<u8>> {
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
        if out.is_empty() {
            if repair.base.is_some() {
                self.repair = repair.asked; // the repair frame was skipped: again next frame
            }
            return None;
        }
        self.note_output(&out, &repair);
        Some(out)
    }

    /// Record a frame that went out: its id, what it references, for repairs to come.
    fn note_output(&mut self, out: &[u8], repair: &Repair) {
        let slice = self.syntax.access_unit(out);
        let key = self.last_was_key;
        let ltr_slot = match &self.encoder {
            Some(Engine::OpenH264(o)) if o.ltr => slice.and_then(|s| s.marks_long_term).map(|_| 0),
            _ => repair.marked,
        };
        let recovers_from = match (repair.base, &self.encoder) {
            _ if key => None,
            (None, _) => None,
            // openh264 says in the bitstream whether it predicted from a long-term picture.
            (Some(b), Some(Engine::OpenH264(_))) => {
                slice.filter(|s| s.refs_long_term).map(|_| b.id)
            }
            // Media Foundation, where it reports the slots it used.
            (Some(b), _) => {
                let slot = b.ltr_slot.unwrap_or(0);
                repair
                    .used
                    .is_none_or(|u| u & (1 << slot) != 0)
                    .then_some(b.id)
            }
        };
        if repair.base.is_some() && !key && recovers_from.is_none() {
            // The encoder did not honour the repair: the viewer still has a broken picture.
            tracing::warn!(engine = %self.engine_name, "loss repair not taken; sending a keyframe");
            self.force_key = true;
        }
        if repair.marked.is_some() || key {
            self.since_mark = 0;
        } else {
            self.since_mark += 1;
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.ledger.record(Sent {
            id,
            instance: self.ledger.instance(),
            key,
            slice,
            ltr_slot,
        });
        self.last = FrameInfo {
            id,
            key,
            recovers_from,
        };
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

/// Should a bandwidth estimate move the encoder? Changes are live (no keyframe) on every
/// engine, so anything beyond estimator jitter (5 %) is followed.
pub fn should_retarget(current_bps: u32, estimate_bps: u32) -> bool {
    let (c, e) = (f64::from(current_bps), f64::from(estimate_bps));
    c > 0.0 && ((e - c).abs() / c) > 0.05
}

pub use crate::decode::{DecodedFrame, PixelLayout, VideoDecoder};

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
        assert!(!should_retarget(4_000_000, 4_150_000));
        assert!(should_retarget(4_000_000, 4_500_000));
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

#[cfg(test)]
mod ltr_tests {
    //! Rate changes and loss repair on openh264. `--nocapture` prints the numbers.
    use super::*;

    /// A text desktop: lines of glyph-like strokes; `typed` characters on line 20,
    /// scrolled up by `scroll` x 7 px.
    pub(crate) fn desktop(w: usize, h: usize, typed: usize, scroll: usize) -> Frame {
        let mut bgra = vec![0u8; w * h * 4];
        for (i, px) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (i % w, i / w + scroll * 7);
            let (line, col) = (y / 18, x / 9);
            let in_text = (y % 18) > 3 && (y % 18) < 15 && (col * 7 + line * 13) % 11 != 0;
            let glyph = ((x % 9) * 3 + (y % 18) * 5 + line + col) % 7 < 3;
            let shown = line != 20 || col < typed;
            let c = if in_text && glyph && shown { 30 } else { 245 };
            *px = [c, c, c.saturating_sub((line % 3) as u8 * 20), 255];
        }
        Frame {
            width: w,
            height: h,
            bgra,
            captured_at: Instant::now(),
        }
    }

    fn encoder(ltr: bool, bps: u32) -> Option<VideoEncoder> {
        let lib = crate::cisco::local_binary_path()?;
        let settings = EncoderSettings {
            bitrate_bps: bps,
            ..EncoderSettings::default()
        };
        // openh264 everywhere (macOS would pick VideoToolbox).
        #[cfg(target_os = "macos")]
        let mut e = VideoEncoder::software(&lib, settings).ok()?;
        #[cfg(not(target_os = "macos"))]
        let mut e = VideoEncoder::new(&lib, settings).ok()?;
        e.set_long_term_refs(ltr);
        Some(e)
    }

    /// Typing for 30 frames, scrolling for 30, then still.
    fn content(k: usize) -> (usize, usize) {
        match k % 90 {
            k @ 0..30 => (k, 0),
            k @ 30..60 => (30, k - 30),
            _ => (30, 30),
        }
    }

    /// Bitrate swings every half second for 10 s: one keyframe (the first frame), and
    /// the output follows the target. Before 0.1.6 each change rebuilt openh264: one
    /// full-size IDR per change.
    #[test]
    fn live_retarget_makes_no_keyframes() {
        for ltr in [false, true] {
            let Some(mut e) = encoder(ltr, 4_000_000) else {
                return;
            };
            let rates = [8_000_000, 1_000_000, 6_000_000, 500_000, 4_000_000];
            let (mut keys, mut changes) = (0, 0);
            let mut bytes = [0usize; 5];
            for k in 0..300 {
                if k > 0 && k % 15 == 0 {
                    e.set_bitrate(rates[(k / 15) % rates.len()]);
                    changes += 1;
                }
                let (typed, scroll) = content(k);
                if let Some(au) = e.encode(&desktop(1280, 800, typed, scroll)).unwrap() {
                    keys += has_idr(&au) as u32;
                    if k >= 15 {
                        bytes[(k / 15) % rates.len()] += au.len();
                    }
                }
            }
            println!(
                "{}: {changes} bitrate changes in 300 frames, {keys} keyframe(s); bytes per rate {:?}",
                e.engine_name(),
                rates.iter().zip(bytes).collect::<Vec<_>>()
            );
            assert_eq!(keys, 1, "only the first frame");
        }
    }

    /// The viewer loses a frame; the encoder repairs from a long-term reference the
    /// viewer acknowledged: no keyframe, and the viewer's picture is bit-exact with a
    /// viewer that lost nothing, from the repair on.
    #[test]
    fn a_lost_frame_is_repaired_without_a_keyframe() {
        let Some(mut e) = encoder(true, 2_000_000) else {
            return;
        };
        let lib = crate::cisco::local_binary_path().unwrap();
        let mut full = VideoDecoder::software(&lib).unwrap();
        let mut lossy = VideoDecoder::software(&lib).unwrap();
        e.start_frame_ids_at(1000);
        // Lost frames, counted in frames put out (typing: every frame changes a little).
        let losses = [50u32, 140, 230];
        let (mut keys, mut repairs, mut compared, mut n) = (0, 0, 0, 0u32);
        let mut broken = false;
        let mut last_good = None;
        let mut repair_bytes = Vec::new();
        let (mut other_bytes, mut first_key) = (0, 0);
        for k in 0..400 {
            let Some(au) = e.encode(&desktop(640, 400, k % 70, k / 70)).unwrap() else {
                continue;
            };
            n += 1;
            let info = e.last_frame();
            keys += info.key as u32;
            if info.key {
                first_key = au.len();
            }
            let a = full.decode(&au).unwrap();
            if info.recovers_from.is_none() && !info.key {
                other_bytes += au.len();
            }
            if losses.contains(&n) {
                // Lost on the way: the viewer notices at the next frame and reports.
                broken = true;
                continue;
            }
            if broken {
                if info.recovers_from.is_some() || info.key {
                    broken = false;
                    repairs += info.recovers_from.is_some() as u32;
                    repair_bytes.push(au.len());
                } else {
                    // A frame built on the lost one: report, and don't decode it (a
                    // decoder fed broken frames may refuse the repair too).
                    e.request_ltr_recovery(last_good.unwrap());
                    continue;
                }
            }
            let b = lossy.decode(&au).unwrap();
            if let (Some(a), Some(b)) = (a, b) {
                assert!(a.data == b.data, "frame {k} differs after the repair");
                compared += 1;
            }
            last_good = Some(info.id);
            e.ltr_acked(info.id);
        }
        println!(
            "{} losses: {repairs} repaired from long-term references ({repair_bytes:?} bytes; other frames {} bytes on average; the first keyframe {} bytes), {keys} keyframe(s), {compared} pictures identical",
            losses.len(),
            other_bytes / n as usize,
            first_key
        );
        assert_eq!(keys, 1, "only the first frame was a keyframe");
        assert_eq!(repairs, 3);
        assert!(compared > 250);
    }

    /// Without long-term references (screen mode) a repair request is a keyframe.
    #[test]
    fn without_long_term_references_a_repair_is_a_keyframe() {
        let Some(mut e) = encoder(false, 2_000_000) else {
            return;
        };
        assert!(!e.supports_ltr());
        for k in 0..10 {
            e.encode(&desktop(320, 200, k, 0)).unwrap();
            e.ltr_acked(e.last_frame().id);
        }
        e.request_ltr_recovery(e.last_frame().id - 2);
        let mut key = false;
        for k in 10..14 {
            if e.encode(&desktop(320, 200, k, 0)).unwrap().is_some() {
                key = e.last_frame().key;
                break;
            }
        }
        assert!(key);
    }

    /// Screen vs camera (long-term reference) mode on the same text desktop.
    /// `cargo test --release -p rconn-media ltr_tests::compare -- --ignored --nocapture`
    #[test]
    #[ignore = "timing, run by hand"]
    fn compare_screen_and_ltr_modes() {
        for ltr in [false, true] {
            let Some(mut e) = encoder(ltr, 4_000_000) else {
                return;
            };
            let frames: Vec<Frame> = (0..90)
                .map(|k| {
                    let (t, s) = content(k);
                    desktop(1280, 800, t, s)
                })
                .collect();
            let mut bytes = [0usize; 3];
            let t = Instant::now();
            for (k, f) in frames.iter().enumerate() {
                if let Some(au) = e.encode(f).unwrap() {
                    if k > 0 {
                        bytes[k / 30] += au.len();
                    }
                }
            }
            println!(
                "{}: typing {} KB, scrolling {} KB, still {} KB (after the first keyframe), {:.1} ms/frame",
                e.engine_name(),
                bytes[0] / 1024,
                bytes[1] / 1024,
                bytes[2] / 1024,
                t.elapsed().as_secs_f64() * 1e3 / 90.0
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
            let (xr, yr) = (x.to_rgba(), y.to_rgba());
            let diff: u64 = xr
                .iter()
                .zip(&yr)
                .map(|(p, q)| u64::from(p.abs_diff(*q)))
                .sum();
            let mean = diff as f64 / xr.len() as f64;
            if mean >= 3.0 {
                for i in [0usize, 1001, 50_000, 2_000_000] {
                    println!(
                        "px {i}: hw {:?} sw {:?}",
                        &xr[i * 4..i * 4 + 4],
                        &yr[i * 4..i * 4 + 4]
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
