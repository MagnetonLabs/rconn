//! H.264 encode/decode for screen content, on Cisco's openh264.

use crate::codec::CodecError;
use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode,
    SpsPpsStrategy, UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncoderSettings {
    pub bitrate_bps: u32,
    pub max_fps: f32,
    /// Worker threads; 0 lets openh264 decide.
    pub threads: u16,
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self {
            bitrate_bps: 4_000_000,
            max_fps: 30.0,
            threads: 0,
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
    yuv: Option<YUVBuffer>,
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
            .skip_frames(true)
            // Keyframes come from RequestKeyframe/PLI, not a timer.
            .intra_frame_period(IntraFramePeriod::from_num_frames(0))
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .num_threads(self.settings.threads);
        let _ = (width, height); // dimensions are taken from the first frame by openh264
        Encoder::with_api_config(api, cfg).map_err(|e| CodecError::OpenH264(e.to_string()))
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
        let (w, h) = frame.even();
        if w == 0 || h == 0 {
            return Ok(None);
        }
        if self.dims != Some((w, h)) || self.encoder.is_none() {
            self.encoder = Some(self.build(w, h)?);
            self.yuv = Some(YUVBuffer::new(w, h));
            self.dims = Some((w, h));
        }
        let yuv = self.yuv.as_mut().expect("allocated above");
        if w == frame.width && h == frame.height {
            yuv.read_rgb(BgraSliceU8::new(&frame.bgra, (w, h)));
        } else {
            // Crop odd edges: copy the even-sized region row by row.
            let mut cropped = Vec::with_capacity(w * h * 4);
            for row in frame.bgra.chunks_exact(frame.width * 4).take(h) {
                cropped.extend_from_slice(&row[..w * 4]);
            }
            yuv.read_rgb(BgraSliceU8::new(&cropped, (w, h)));
        }
        let encoder = self.encoder.as_mut().expect("built above");
        if std::mem::take(&mut self.force_key) {
            encoder.force_intra_frame();
        }
        let bits = encoder
            .encode(&*yuv)
            .map_err(|e| CodecError::OpenH264(e.to_string()))?;
        let out = bits.to_vec();
        Ok(if out.is_empty() { None } else { Some(out) })
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
}
