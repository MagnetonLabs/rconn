//! Thin constructors for the codecs, so callers don't depend on crate specifics.

use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("openh264: {0}")]
    OpenH264(String),
    #[error("opus: {0}")]
    Opus(#[from] opus::Error),
}

/// Load Cisco's openh264 shared library from `path`. Fails if the file's SHA-256 is not
/// a known Cisco build.
pub fn load_openh264(path: &Path) -> Result<openh264::OpenH264API, CodecError> {
    openh264::OpenH264API::from_blob_path(path).map_err(|e| CodecError::OpenH264(e.to_string()))
}

/// Opus encoder tuned for remote-desktop audio: 48 kHz stereo, lowest-delay mode.
pub fn opus_encoder() -> Result<opus::Encoder, CodecError> {
    Ok(opus::Encoder::new(
        48_000,
        opus::Channels::Stereo,
        opus::Application::LowDelay,
    )?)
}

pub fn opus_decoder() -> Result<opus::Decoder, CodecError> {
    Ok(opus::Decoder::new(48_000, opus::Channels::Stereo)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_round_trips_a_20ms_frame() {
        let mut enc = opus_encoder().unwrap();
        let mut dec = opus_decoder().unwrap();
        let pcm: Vec<f32> = (0..960 * 2)
            .map(|i| ((i as f32) * 0.01).sin() * 0.3)
            .collect();
        let mut packet = [0u8; 4000];
        let n = enc.encode_float(&pcm, &mut packet).unwrap();
        assert!(n > 0);
        let mut out = vec![0f32; 960 * 2];
        assert_eq!(
            dec.decode_float(&packet[..n], &mut out, false).unwrap(),
            960
        );
    }

    #[test]
    fn missing_openh264_binary_is_a_clean_error() {
        assert!(load_openh264(Path::new("/definitely/not/here.dll")).is_err());
    }
}
