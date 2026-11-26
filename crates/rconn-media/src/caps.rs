//! Which video codecs this machine can decode (a viewer's offer) and encode (a host's
//! answer), probed at run time, once.
//!
//! | Codec | Decode | Encode |
//! |---|---|---|
//! | H.264 | everywhere (openh264 fallback) | everywhere (openh264 fallback) |
//! | HEVC | VideoToolbox; Media Foundation when an HEVC decoder MFT is installed ("HEVC Video Extensions") | hardware only: VideoToolbox, a GPU's Media Foundation MFT |
//! | AV1 | VideoToolbox (M3/A17 Pro and later); Media Foundation AV1 decoder MFT | a GPU's Media Foundation MFT (NVIDIA Ada+, AMD VCN 4+, Snapdragon X; Windows 11 24H2) |
//!
//! No software HEVC encoder or decoder is bundled (patent pools); software encoding is
//! always H.264, the cheapest in CPU. There is no software AV1 decoder either: `dav1d`
//! needs a C/assembly build (meson, nasm) on every target, which this workspace does not
//! have. `RCONN_CODEC` ([`rconn_core::codec::CODEC_ENV`]) forces a codec; on a host it may
//! use the operating system's own software encoder for it.

pub use rconn_core::codec::VideoCodec;
use std::sync::OnceLock;

/// Codecs whose whole path (encode, decode, packetization, side data) this build has.
const BUILT: &[VideoCodec] = &[];

/// Codecs besides H.264 this machine decodes.
pub fn decodable() -> Vec<VideoCodec> {
    static CACHE: OnceLock<Vec<VideoCodec>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let found: Vec<VideoCodec> = BUILT
                .iter()
                .copied()
                .filter(|&c| platform::can_decode(c))
                .collect();
            tracing::info!(?found, "video decoders besides H.264");
            found
        })
        .clone()
}

/// Codecs besides H.264 this machine encodes in hardware, and a codec forced with
/// `RCONN_CODEC` that any operating-system encoder can do.
pub fn encodable() -> Vec<VideoCodec> {
    static CACHE: OnceLock<Vec<VideoCodec>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let forced = VideoCodec::from_env();
            let found: Vec<VideoCodec> = BUILT
                .iter()
                .copied()
                .filter(|&c| {
                    platform::can_encode(c, true)
                        || (forced == Some(c) && platform::can_encode(c, false))
                })
                .collect();
            tracing::info!(?found, "video encoders besides H.264");
            found
        })
        .clone()
}

/// What a viewer offers besides H.264: what it decodes, limited by `RCONN_CODEC`.
pub fn viewer_codecs() -> Vec<VideoCodec> {
    rconn_core::codec::offer(&decodable(), VideoCodec::from_env())
        .into_iter()
        .filter(|&c| c != VideoCodec::H264)
        .collect()
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod platform {
    use super::VideoCodec;

    pub fn can_decode(codec: VideoCodec) -> bool {
        crate::vt::hardware_decodes(codec)
    }

    #[cfg(target_os = "macos")]
    pub fn can_encode(codec: VideoCodec, hardware: bool) -> bool {
        crate::vt_encode::encoder_exists(codec, hardware)
    }

    #[cfg(target_os = "ios")]
    pub fn can_encode(_codec: VideoCodec, _hardware: bool) -> bool {
        false // iOS is never a host
    }
}

#[cfg(windows)]
mod platform {
    use super::VideoCodec;

    pub fn can_decode(codec: VideoCodec) -> bool {
        crate::mf_decode::decoder_exists(codec)
    }

    pub fn can_encode(codec: VideoCodec, hardware: bool) -> bool {
        crate::mf::encoder_exists(codec, hardware)
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "ios")))]
mod platform {
    use super::VideoCodec;

    /// Linux: openh264 only (no VA-API path yet).
    pub fn can_decode(_codec: VideoCodec) -> bool {
        false
    }

    pub fn can_encode(_codec: VideoCodec, _hardware: bool) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_is_never_listed_and_probes_are_stable() {
        let (d, e) = (decodable(), encodable());
        assert!(!d.contains(&VideoCodec::H264) && !e.contains(&VideoCodec::H264));
        assert_eq!(decodable(), d, "probed once");
        println!("decodes {d:?}, encodes {e:?}");
    }
}
