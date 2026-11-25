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
pub struct VideoEncoder {
    api_path: std::path::PathBuf,
    settings: EncoderSettings,
    dims: Option<(usize, usize)>,
    encoder: Option<Encoder>,
    yuv: Option<I420>,
    /// Fraction of the captured size to encode (1.0 = full); lowered when the CPU is busy.
    scale: f32,
    last_was_key: bool,
    force_key: bool,
}

impl VideoEncoder {
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
            yuv: None,
            scale: 1.0,
            last_was_key: false,
            force_key: false,
        })
    }

    fn build(&self, width: usize, height: usize) -> Result<Encoder, CodecError> {
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
        if bps != self.settings.bitrate_bps {
            self.settings.bitrate_bps = bps;
            self.encoder = None; // rebuilt lazily; the first frame out is an IDR
        }
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
        if self.dims != Some((w, h)) || self.encoder.is_none() {
            self.encoder = Some(self.build(w, h)?);
            self.yuv = Some(I420::new(w, h));
            self.dims = Some((w, h));
        }
        let yuv = self.yuv.as_mut().expect("allocated above");
        // Odd edges are cropped by converting only the even-sized top-left region.
        yuv.fill_from_bgra_scaled(&frame.bgra, frame.width, frame.height);
        let encoder = self.encoder.as_mut().expect("built above");
        if std::mem::take(&mut self.force_key) {
            encoder.force_intra_frame();
        }
        let bits = encoder
            .encode(&*yuv)
            .map_err(|e| CodecError::OpenH264(e.to_string()))?;
        self.last_was_key = matches!(
            bits.frame_type(),
            openh264::encoder::FrameType::IDR | openh264::encoder::FrameType::I
        );
        let out = bits.to_vec();
        Ok(if out.is_empty() { None } else { Some(out) })
    }

    /// Was the last encoded frame a keyframe? (Keyframes are always slow and large.)
    pub fn last_was_key(&self) -> bool {
        self.last_was_key
    }
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

pub struct VideoDecoder {
    decoder: Decoder,
}

impl VideoDecoder {
    pub fn new(openh264_path: &std::path::Path) -> Result<Self, CodecError> {
        let api = crate::codec::load_openh264(openh264_path)?;
        let decoder = Decoder::with_api_config(api, DecoderConfig::new())
            .map_err(|e| CodecError::OpenH264(e.to_string()))?;
        Ok(Self { decoder })
    }

    /// Decode one Annex-B access unit. `None` until the decoder has a full picture.
    pub fn decode(&mut self, annex_b: &[u8]) -> Result<Option<DecodedFrame>, CodecError> {
        let Some(yuv) = self
            .decoder
            .decode(annex_b)
            .map_err(|e| CodecError::OpenH264(e.to_string()))?
        else {
            return Ok(None);
        };
        let (width, height) = yuv.dimensions();
        let mut rgba = vec![0u8; width * height * 4];
        yuv.write_rgba8(&mut rgba);
        Ok(Some(DecodedFrame {
            width,
            height,
            rgba,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
