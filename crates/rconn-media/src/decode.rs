//! H.264 decoding for the viewer: the platform's decoder where there is one, Cisco's
//! openh264 otherwise. Pictures come out as YUV planes (NV12 or I420), which the viewer
//! uploads as they are and turns into RGB on the GPU; [`DecodedFrame::to_rgba`] is for
//! the few places that need pixels on the CPU (screenshots, tests).
//!
//! | Platform | Engine | Output |
//! |---|---|---|
//! | macOS, iOS | VideoToolbox (hardware) | NV12 |
//! | Windows | Media Foundation on Direct3D 11 (DXVA) | NV12 |
//! | Linux, fallback | openh264 | I420 |
//!
//! `RCONN_DECODER=openh264` forces software everywhere (comparisons); on Windows
//! `RCONN_DECODER=mf-software` keeps Media Foundation but off the GPU.

use crate::codec::CodecError;
use crate::sps::{Colour, SpsInfo};
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::formats::YUVSource;
use rconn_core::codec::VideoCodec;
use std::time::{Duration, Instant};

/// How the planes of a [`DecodedFrame`] are laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelLayout {
    /// Y plane, then one plane of interleaved Cb/Cr (2 bytes per chroma sample).
    Nv12,
    /// Y plane, then a Cb plane, then a Cr plane.
    I420,
}

/// A decoded picture: 4:2:0 YUV, tightly packed planes (no row padding).
#[derive(Clone)]
pub struct DecodedFrame {
    pub width: usize,
    pub height: usize,
    pub layout: PixelLayout,
    /// Y (`width * height` bytes), then the chroma planes (see [`PixelLayout`]), each
    /// [`chroma_size`](Self::chroma_size) samples.
    pub data: Vec<u8>,
    pub colour: Colour,
    /// Time the decoder took for this picture, copying it out included.
    pub decode_time: Duration,
}

impl DecodedFrame {
    /// Chroma plane size in samples.
    pub fn chroma_size(&self) -> (usize, usize) {
        (self.width.div_ceil(2), self.height.div_ceil(2))
    }

    pub fn y(&self) -> &[u8] {
        &self.data[..self.width * self.height]
    }

    /// NV12: the interleaved Cb/Cr plane. I420: the Cb plane.
    pub fn chroma1(&self) -> &[u8] {
        let (cw, ch) = self.chroma_size();
        let n = self.width * self.height;
        match self.layout {
            PixelLayout::Nv12 => &self.data[n..n + 2 * cw * ch],
            PixelLayout::I420 => &self.data[n..n + cw * ch],
        }
    }

    /// I420: the Cr plane. NV12: empty.
    pub fn chroma2(&self) -> &[u8] {
        let (cw, ch) = self.chroma_size();
        let n = self.width * self.height;
        match self.layout {
            PixelLayout::Nv12 => &[],
            PixelLayout::I420 => &self.data[n + cw * ch..n + 2 * cw * ch],
        }
    }

    /// A picture from RGBA pixels (BT.601 limited range, as hosts encode); for tests and
    /// placeholders. `width` and `height` must be even.
    pub fn from_rgba(width: usize, height: usize, rgba: &[u8]) -> Self {
        let mut bgra = rgba.to_vec();
        for px in bgra.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
        let mut yuv = crate::yuv::I420::new(width, height);
        yuv.fill_from_bgra(&bgra, width);
        let mut data = Vec::with_capacity(width * height * 3 / 2);
        data.extend_from_slice(yuv.y());
        data.extend_from_slice(yuv.u());
        data.extend_from_slice(yuv.v());
        Self {
            width,
            height,
            layout: PixelLayout::I420,
            data,
            colour: Colour::default(),
            decode_time: Duration::ZERO,
        }
    }

    /// The picture as tightly packed RGBA, converted on the CPU (all cores).
    pub fn to_rgba(&self) -> Vec<u8> {
        let (w, h) = (self.width, self.height);
        let mut rgba = vec![0u8; w * h * 4];
        let cw = self.chroma_size().0;
        if self.colour == Colour::default() {
            match self.layout {
                PixelLayout::Nv12 => crate::yuv::nv12_to_rgba(
                    (self.y(), self.chroma1()),
                    (w, 2 * cw),
                    (w, h),
                    &mut rgba,
                ),
                PixelLayout::I420 => crate::yuv::i420_to_rgba(
                    (self.y(), self.chroma1(), self.chroma2()),
                    (w, cw, cw),
                    (w, h),
                    &mut rgba,
                ),
            }
            return rgba;
        }
        // Other colour spaces (rare): the same maths the GPU does.
        let m = self.colour.yuv_to_rgb_rows();
        let (y, c1, c2) = (self.y(), self.chroma1(), self.chroma2());
        for (i, px) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, row) = (i % w, i / w);
            let c = (row / 2) * cw + x / 2;
            let (u, v) = match self.layout {
                PixelLayout::Nv12 => (c1[2 * c], c1[2 * c + 1]),
                PixelLayout::I420 => (c1[c], c2[c]),
            };
            let s = [y[i], u, v].map(|b| f32::from(b) / 255.0);
            let [r, g, b] = m.map(|k| {
                ((k[0] * s[0] + k[1] * s[1] + k[2] * s[2] + k[3]) * 255.0)
                    .round()
                    .clamp(0.0, 255.0) as u8
            });
            *px = [r, g, b, 255];
        }
        rgba
    }
}

/// H.264 decoder (see the module docs for which engine runs where).
pub struct VideoDecoder {
    engine: DecEngine,
    /// For a fresh software decoder per stream, or falling back to software if the
    /// hardware decoder refuses a stream.
    openh264_path: std::path::PathBuf,
    /// The sequence header (SPS) of the stream being decoded, and what it says.
    sps: Vec<u8>,
    info: Option<SpsInfo>,
    /// Pictures from the current engine (a failure before the first one moves down to
    /// the next engine).
    pictures: u64,
    /// The planes of the picture being decoded.
    buf: Vec<u8>,
    /// The stream's codec ([`Self::set_codec`]); H.264 unless told otherwise.
    codec: VideoCodec,
    /// `RCONN_DECODER`, for engines made later.
    choice: String,
}

enum DecEngine {
    OpenH264(Box<Decoder>),
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    VideoToolbox(crate::vt::VtDecoder),
    #[cfg(windows)]
    MediaFoundation(crate::mf_decode::MfH264Decoder),
}

impl VideoDecoder {
    pub fn new(openh264_path: &std::path::Path) -> Result<Self, CodecError> {
        let choice = std::env::var("RCONN_DECODER").unwrap_or_default();
        let engine = if choice == "openh264" {
            Self::openh264(openh264_path)?
        } else {
            Self::platform(VideoCodec::H264, &choice)
                .map_or_else(|| Self::openh264(openh264_path), Ok)?
        };
        Ok(Self::with_engine(engine, openh264_path))
    }

    /// Cisco's openh264 on every platform (comparisons, tests).
    pub fn software(openh264_path: &std::path::Path) -> Result<Self, CodecError> {
        Ok(Self::with_engine(
            Self::openh264(openh264_path)?,
            openh264_path,
        ))
    }

    fn with_engine(engine: DecEngine, openh264_path: &std::path::Path) -> Self {
        Self {
            engine,
            openh264_path: openh264_path.to_owned(),
            sps: Vec::new(),
            info: None,
            pictures: 0,
            buf: Vec::new(),
            codec: VideoCodec::H264,
            choice: std::env::var("RCONN_DECODER").unwrap_or_default(),
        }
    }

    /// Decode `codec` from now on (the codec of the stream: `VideoReceiver::codec`). HEVC
    /// and AV1 only have the platform's decoders (no software fallback); an error says
    /// there is none.
    pub fn set_codec(&mut self, codec: VideoCodec) -> Result<(), CodecError> {
        if codec == self.codec {
            return Ok(());
        }
        let engine = match codec {
            VideoCodec::H264 => Self::platform(VideoCodec::H264, &self.choice)
                .map_or_else(|| Self::openh264(&self.openh264_path), Ok)?,
            _ => Self::platform(codec, &self.choice).ok_or_else(|| {
                CodecError::OpenH264(format!("no {} decoder on this machine", codec.name()))
            })?,
        };
        self.engine = engine;
        self.codec = codec;
        self.sps.clear();
        self.info = None;
        self.pictures = 0;
        Ok(())
    }

    pub fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// The platform's own decoder, if it has one that starts.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    fn platform(codec: VideoCodec, _choice: &str) -> Option<DecEngine> {
        Some(DecEngine::VideoToolbox(crate::vt::VtDecoder::for_codec(
            codec,
        )))
    }

    /// The platform's own decoder, if it has one that starts: Media Foundation on the
    /// GPU (on the CPU only when asked for: it is no faster than openh264 there).
    #[cfg(windows)]
    fn platform(codec: VideoCodec, choice: &str) -> Option<DecEngine> {
        let hardware = choice != "mf-software";
        match crate::mf_decode::MfH264Decoder::new_codec(codec, hardware) {
            Ok(d) => Some(DecEngine::MediaFoundation(d)),
            Err(e) => {
                tracing::info!(error = %e, codec = codec.name(), "no GPU decoder");
                None
            }
        }
    }

    #[cfg(not(any(windows, target_os = "macos", target_os = "ios")))]
    fn platform(_codec: VideoCodec, _choice: &str) -> Option<DecEngine> {
        None
    }

    fn openh264(path: &std::path::Path) -> Result<DecEngine, CodecError> {
        let api = crate::codec::load_openh264(path)?;
        let decoder = Decoder::with_api_config(api, DecoderConfig::new())
            .map_err(|e| CodecError::OpenH264(e.to_string()))?;
        Ok(DecEngine::OpenH264(Box::new(decoder)))
    }

    pub fn engine_name(&self) -> &'static str {
        match &self.engine {
            DecEngine::OpenH264(_) => "openh264 (software)",
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            DecEngine::VideoToolbox(_) => "VideoToolbox (hardware)",
            #[cfg(windows)]
            DecEngine::MediaFoundation(d) if d.is_hardware() => "Media Foundation (hardware)",
            #[cfg(windows)]
            DecEngine::MediaFoundation(_) => "Media Foundation (software)",
        }
    }

    /// Decode one Annex-B access unit. `None` until the decoder has a full picture.
    ///
    /// A new sequence header (the host restarted its encoder: a new screen size, a new
    /// capture after a desktop switch) gets a fresh decoder for software and Media
    /// Foundation, so nothing of the old stream (its size, its reference pictures, a
    /// header replaced half-way) can keep the new one from showing. VideoToolbox opens a
    /// new session for it ([`crate::vt`]).
    pub fn decode(&mut self, annex_b: &[u8]) -> Result<Option<DecodedFrame>, CodecError> {
        let t0 = Instant::now();
        if let Some(sps) = crate::video::sequence_header_of(self.codec, annex_b)
            && sps != self.sps.as_slice()
        {
            let restart = !self.sps.is_empty();
            self.sps = sps.to_vec();
            self.info = crate::video::sequence_info(self.codec, sps);
            if restart {
                self.restart()?;
            }
        }
        let result = self.decode_with_engine(annex_b);
        match result {
            Ok(Some((width, height, layout))) => {
                self.pictures += 1;
                let colour = self.info.map(|i| i.colour).unwrap_or_default();
                Ok(Some(DecodedFrame {
                    width,
                    height,
                    layout,
                    data: std::mem::take(&mut self.buf),
                    colour,
                    decode_time: t0.elapsed(),
                }))
            }
            Ok(None) => Ok(None),
            Err(e) if self.fall_back(&e)? => match self.decode(annex_b)? {
                // Nothing to show from this unit: let the caller ask for a keyframe now.
                None => Err(e),
                some => Ok(some),
            },
            Err(e) => Err(e),
        }
    }

    /// Fresh decoder of the same kind for a new stream.
    fn restart(&mut self) -> Result<(), CodecError> {
        match &self.engine {
            DecEngine::OpenH264(_) => {
                self.engine = Self::openh264(&self.openh264_path)?;
                self.pictures = 0;
            }
            #[cfg(windows)]
            DecEngine::MediaFoundation(d) => {
                let hardware = d.is_hardware();
                self.engine = match crate::mf_decode::MfH264Decoder::new_codec(self.codec, hardware)
                {
                    Ok(d) => DecEngine::MediaFoundation(d),
                    Err(e) => {
                        tracing::warn!(error = %e, "Media Foundation decoder restart failed; using openh264");
                        Self::openh264(&self.openh264_path)?
                    }
                };
                self.pictures = 0;
            }
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            DecEngine::VideoToolbox(_) => {}
        }
        Ok(())
    }

    /// Move to openh264 if the engine (not the stream) failed: Media Foundation that
    /// fails before its first picture, VideoToolbox without a hardware decoder for the
    /// stream. `false`: keep the engine.
    #[cfg(not(any(windows, target_os = "macos", target_os = "ios")))]
    #[expect(clippy::unnecessary_wraps, clippy::unused_self)]
    fn fall_back(&self, _e: &CodecError) -> Result<bool, CodecError> {
        Ok(false) // openh264 is the only engine here
    }

    /// Move to openh264 if the engine (not the stream) failed: Media Foundation that
    /// fails before its first picture, VideoToolbox without a hardware decoder for the
    /// stream. `false`: keep the engine.
    #[cfg(any(windows, target_os = "macos", target_os = "ios"))]
    fn fall_back(&mut self, e: &CodecError) -> Result<bool, CodecError> {
        if self.codec != VideoCodec::H264 {
            return Ok(false); // openh264 is H.264 only
        }
        #[cfg(windows)]
        if self.pictures > 0 {
            return Ok(false);
        }
        let next = match &self.engine {
            DecEngine::OpenH264(_) => return Ok(false),
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            DecEngine::VideoToolbox(_) => {
                // Only when there is no hardware decoder for this stream; other errors
                // (a lost session, no keyframe yet) are the stream's, not the engine's.
                if !matches!(e, CodecError::VideoToolbox(m) if m.starts_with("VTDecompressionSessionCreate"))
                {
                    return Ok(false);
                }
                Self::openh264(&self.openh264_path)?
            }
            #[cfg(windows)]
            DecEngine::MediaFoundation(_) => Self::openh264(&self.openh264_path)?,
        };
        tracing::warn!(error = %e, from = self.engine_name(), "decoder failed; falling back");
        self.engine = next;
        self.pictures = 0;
        Ok(true)
    }

    /// Run the engine; the planes go to `self.buf`.
    fn decode_with_engine(
        &mut self,
        annex_b: &[u8],
    ) -> Result<Option<(usize, usize, PixelLayout)>, CodecError> {
        match &mut self.engine {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            DecEngine::VideoToolbox(vt) => Ok(vt
                .decode(annex_b, &mut self.buf)
                .map_err(CodecError::VideoToolbox)?
                .map(|(w, h)| (w, h, PixelLayout::Nv12))),
            #[cfg(windows)]
            DecEngine::MediaFoundation(mf) => {
                let hint = self.info.map(|i| (i.width, i.height));
                Ok(mf
                    .decode(annex_b, hint, &mut self.buf)?
                    .map(|(w, h)| (w, h, PixelLayout::Nv12)))
            }
            DecEngine::OpenH264(decoder) => {
                let Some(yuv) = decoder
                    .decode(annex_b)
                    .map_err(|e| CodecError::OpenH264(e.to_string()))?
                else {
                    return Ok(None);
                };
                let (w, h) = yuv.dimensions();
                let (ys, us, vs) = yuv.strides();
                let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
                let buf = &mut self.buf;
                buf.clear();
                buf.reserve(w * h + 2 * cw * ch);
                for (plane, stride, pw, ph) in [
                    (yuv.y(), ys, w, h),
                    (yuv.u(), us, cw, ch),
                    (yuv.v(), vs, cw, ch),
                ] {
                    for row in 0..ph {
                        buf.extend_from_slice(&plane[row * stride..row * stride + pw]);
                    }
                }
                Ok(Some((w, h, PixelLayout::I420)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_round_trip_through_planes() {
        let (w, h) = (64, 32);
        let rgba: Vec<u8> = (0..w * h)
            .flat_map(|i| [(i % w * 4) as u8, 120, (i / w * 8) as u8, 255])
            .collect();
        let f = DecodedFrame::from_rgba(w, h, &rgba);
        assert_eq!(f.data.len(), w * h * 3 / 2);
        assert_eq!(f.chroma1().len(), w * h / 4);
        let back = f.to_rgba();
        let worst = rgba
            .iter()
            .zip(&back)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        // 4:2:0 chroma on a gentle gradient: small error only.
        assert!(worst <= 12, "differs by {worst}");
        // The same picture as NV12 gives the same pixels.
        let mut nv12 = f.clone();
        nv12.layout = PixelLayout::Nv12;
        let n = w * h;
        let (u, v) = (f.chroma1(), f.chroma2());
        nv12.data.truncate(n);
        for (a, b) in u.iter().zip(v) {
            nv12.data.extend([*a, *b]);
        }
        assert_eq!(nv12.to_rgba(), back);
        // A BT.709 picture goes through the general path; grey stays grey.
        let mut grey = DecodedFrame::from_rgba(w, h, &vec![128; w * h * 4]);
        grey.colour.matrix = crate::sps::Matrix::Bt709;
        assert!(
            grey.to_rgba()
                .iter()
                .all(|&b| (127..=129).contains(&b) || b == 255)
        );
    }
}

#[cfg(all(test, windows))]
mod windows_decode {
    //! Media Foundation decoding against openh264, through size changes like a host's.
    //! `cargo test --release -p rconn-media windows_decode -- --nocapture` prints speed.
    use super::*;
    use crate::mf_decode::MfH264Decoder;
    use crate::video::{EncoderSettings, Frame, VideoEncoder};

    /// Desktop-like frames (text-ish stripes that scroll), `n` per size, the encoder
    /// restarting at each size change as a host's does.
    fn stream(lib: &std::path::Path, sizes: &[(usize, usize)], n: usize) -> Vec<Vec<u8>> {
        let mut enc = VideoEncoder::new(
            lib,
            EncoderSettings {
                bitrate_bps: 8_000_000,
                ..EncoderSettings::default()
            },
        )
        .unwrap();
        let mut out = Vec::new();
        for &(w, h) in sizes {
            for k in 0..n {
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
                let f = Frame {
                    width: w,
                    height: h,
                    bgra,
                    captured_at: Instant::now(),
                };
                out.extend(enc.encode(&f).unwrap());
            }
        }
        out
    }

    fn mean_diff(a: &DecodedFrame, b: &DecodedFrame) -> f64 {
        let (x, y) = (a.to_rgba(), b.to_rgba());
        let d: u64 = x
            .iter()
            .zip(&y)
            .map(|(p, q)| u64::from(p.abs_diff(*q)))
            .sum();
        d as f64 / x.len() as f64
    }

    /// Pictures, and mean ms per picture.
    fn decode_all(dec: &mut VideoDecoder, aus: &[Vec<u8>]) -> (Vec<DecodedFrame>, f64) {
        let mut frames = Vec::new();
        let mut ms = 0.0;
        for au in aus {
            match dec.decode(au) {
                Ok(Some(f)) => {
                    ms += f.decode_time.as_secs_f64() * 1e3;
                    frames.push(f);
                }
                Ok(None) => {}
                Err(e) => panic!("{} failed: {e}", dec.engine_name()),
            }
        }
        let n = frames.len().max(1) as f64;
        (frames, ms / n)
    }

    #[test]
    fn media_foundation_matches_openh264_through_size_changes() {
        let Some(lib) = crate::cisco::local_binary_path().filter(|p| p.exists()) else {
            eprintln!("no openh264 binary (set RCONN_OPENH264); skipped");
            return;
        };
        // 1080p, a size whose height is not a multiple of 16 (cropped), then back.
        let sizes = [(1920, 1080), (1496, 938), (1920, 1080)];
        let per = 12;
        let aus = stream(&lib, &sizes, per);
        let (sw, sw_ms) = decode_all(&mut VideoDecoder::software(&lib).unwrap(), &aus);
        let t = Instant::now();
        for f in &sw {
            std::hint::black_box(f.to_rgba());
        }
        let rgba_ms = t.elapsed().as_secs_f64() * 1e3 / sw.len() as f64;
        assert_eq!(sw.len(), aus.len(), "openh264: a picture per unit");
        println!(
            "openh264: {sw_ms:.2} ms/frame (+ {rgba_ms:.2} ms I420->RGBA on the CPU, the old viewer path)"
        );
        for hardware in [true, false] {
            let mf = match MfH264Decoder::new(hardware) {
                Ok(d) => d,
                Err(e) => {
                    assert!(hardware, "software Media Foundation must exist: {e}");
                    println!("no GPU decoder here: {e}");
                    continue;
                }
            };
            let mut dec = VideoDecoder::with_engine(DecEngine::MediaFoundation(mf), &lib);
            let (pics, ms) = decode_all(&mut dec, &aus);
            let name = dec.engine_name();
            let on_gpu = matches!(&dec.engine, DecEngine::MediaFoundation(d) if d.last_on_gpu());
            println!(
                "{name} (GPU surfaces: {on_gpu}): {ms:.2} ms/frame, {} pictures",
                pics.len()
            );
            assert!(name.starts_with("Media Foundation"), "fell back to {name}");
            assert_eq!(pics.len(), aus.len(), "{name}: a picture per unit");
            for (i, (a, b)) in pics.iter().zip(&sw).enumerate() {
                assert_eq!(
                    (a.width, a.height),
                    (b.width, b.height),
                    "{name}: size of picture {i}"
                );
                assert_eq!(a.layout, PixelLayout::Nv12);
                let d = mean_diff(a, b);
                assert!(
                    d < 2.0,
                    "{name}: picture {i} differs from openh264 by {d:.2}"
                );
            }
            assert_eq!(
                (pics[per].width, pics[per].height),
                sizes[1],
                "{name}: switched size"
            );
        }
        let dec = VideoDecoder::new(&lib).unwrap();
        println!("default decoder here: {}", dec.engine_name());
        let gpu = MfH264Decoder::new(true).is_ok();
        assert_eq!(
            dec.engine_name().starts_with("Media Foundation"),
            gpu,
            "Media Foundation exactly when the GPU decodes H.264"
        );
    }

    #[test]
    fn a_stream_joined_mid_way_shows_its_next_keyframe() {
        let Some(lib) = crate::cisco::local_binary_path().filter(|p| p.exists()) else {
            return;
        };
        let aus = stream(&lib, &[(640, 360)], 6);
        for hardware in [true, false] {
            let Ok(mf) = MfH264Decoder::new(hardware) else {
                continue;
            };
            let mut dec = VideoDecoder::with_engine(DecEngine::MediaFoundation(mf), &lib);
            // P-frames only: no picture (the viewer's 1 s timer then asks for a keyframe).
            for au in &aus[1..] {
                assert!(matches!(dec.decode(au), Ok(None) | Err(_)));
            }
            assert!(
                dec.decode(&aus[0]).unwrap().is_some(),
                "the keyframe shows at once"
            );
            assert!(dec.engine_name().starts_with("Media Foundation"));
        }
    }
}
