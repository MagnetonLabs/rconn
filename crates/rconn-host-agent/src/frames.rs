//! Where the picture lives between capture and encode.
//!
//! * Memory (every platform): the capturer converts what changed into an I420 picture,
//!   which the encoder reads.
//! * GPU (Windows, with a D3D11-aware hardware encoder): Desktop Duplication copies what
//!   changed into a texture, the GPU's video processor converts (and scales) it to NV12,
//!   and the hardware encoder reads that texture. No picture crosses to the CPU.
//!
//! The GPU path is chosen per capture generation when it works, and given up (for a
//! while) the moment it fails: the next generation captures to memory as before.

use rconn_media::capture::{CaptureError, Captured, Capturer};
use rconn_media::codec::CodecError;
use rconn_media::video::VideoEncoder;
use rconn_media::yuv::I420;
use std::time::Duration;

pub struct Frames {
    cap: Box<dyn Capturer>,
    picture: Option<I420>,
    small: Option<I420>,
    gpu: bool,
}

impl Frames {
    /// Frames from `cap` for `enc`; on the GPU if both can, in memory otherwise.
    pub fn new(mut cap: Box<dyn Capturer>, enc: &mut VideoEncoder, max: (usize, usize)) -> Self {
        let gpu = gpu::try_enable(cap.as_mut(), enc, max);
        Self {
            cap,
            picture: None,
            small: None,
            gpu,
        }
    }

    /// Frames in memory only (comparisons in the GPU self-test).
    #[cfg(windows)]
    pub fn memory(cap: Box<dyn Capturer>) -> Self {
        Self {
            cap,
            picture: None,
            small: None,
            gpu: false,
        }
    }

    /// Are frames kept on the GPU?
    #[allow(dead_code)] // logs and the self-test; false off Windows
    pub fn on_gpu(&self) -> bool {
        self.gpu
    }

    /// Wait up to `timeout` for a change (see [`Capturer::next_into`]).
    pub fn next(
        &mut self,
        timeout: Duration,
        max: (usize, usize),
    ) -> Result<Option<Captured>, CaptureError> {
        #[cfg(windows)]
        if self.gpu {
            return self.dxgi().next_gpu(timeout, max);
        }
        self.cap.next_into(timeout, &mut self.picture, max)
    }

    /// The size of the current picture (as sent at full size), once there is one.
    pub fn dimensions(&mut self) -> Option<(usize, usize)> {
        #[cfg(windows)]
        if self.gpu {
            return self.dxgi().gpu_dimensions();
        }
        self.picture.as_ref().map(I420::dimensions)
    }

    /// Encode the current picture, at half size if `half`. `rows` are the rows changed
    /// since the last full-size encode (lets the memory path rewrite only those).
    pub fn encode(
        &mut self,
        enc: &mut VideoEncoder,
        half: bool,
        rows: (usize, usize),
    ) -> Result<Option<Vec<u8>>, CodecError> {
        #[cfg(windows)]
        if self.gpu {
            let d = self.dxgi();
            let (Some((w, h)), Some(frame)) = (d.gpu_dimensions(), d.gpu_frame().cloned()) else {
                return Ok(None);
            };
            let size = if half {
                ((w / 2) & !1, (h / 2) & !1)
            } else {
                (w, h)
            };
            let out = enc.encode_gpu(&frame, size);
            if let Err(e) = &out {
                gpu::failed(&format!("encode: {e}"));
            }
            return out;
        }
        let Some(pic) = self.picture.as_ref() else {
            return Ok(None);
        };
        if half {
            pic.half_into(&mut self.small);
            enc.encode_yuv(self.small.as_ref().expect("allocated by half_into"), None)
        } else {
            enc.encode_yuv(pic, Some(rows))
        }
    }

    #[cfg(windows)]
    pub fn dxgi(&mut self) -> &mut rconn_media::capture::dxgi::DxgiCapturer {
        self.cap
            .as_dxgi()
            .expect("GPU frames come from Desktop Duplication")
    }
}

#[cfg(windows)]
pub use gpu::retry as retry_gpu;

#[cfg(windows)]
mod gpu {
    use super::*;
    use std::sync::Mutex;
    use std::time::Instant;

    /// After a failure, the GPU path is tried again only after this long (a driver
    /// update, a freed encoder session); meanwhile every generation uses memory.
    const RETRY_AFTER: Duration = Duration::from_secs(600);

    static FAILED_AT: Mutex<Option<Instant>> = Mutex::new(None);

    /// Forget an earlier failure (the self-test tries again with other settings).
    pub fn retry() {
        *FAILED_AT.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub(super) fn failed(why: &str) {
        tracing::info!(error = why, "GPU frame path off; capturing to memory");
        *FAILED_AT.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }

    /// `RCONN_TEXTURE_ENCODER=software`: run the texture path with the software encoder
    /// (tests on machines without a hardware one; see `MfH264Encoder::new_d3d11`).
    fn software_test() -> bool {
        std::env::var("RCONN_TEXTURE_ENCODER").is_ok_and(|v| v == "software")
    }

    fn wanted() -> bool {
        let env = |k: &str| std::env::var(k).ok();
        if env("RCONN_GPU").is_some_and(|v| v == "0" || v == "off")
            || env("RCONN_ENCODER").is_some_and(|v| v == "software")
        {
            return false;
        }
        FAILED_AT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none_or(|t| t.elapsed() >= RETRY_AFTER)
    }

    pub(super) fn try_enable(
        cap: &mut dyn Capturer,
        enc: &mut VideoEncoder,
        max: (usize, usize),
    ) -> bool {
        if !wanted() {
            return false;
        }
        let Some(d) = cap.as_dxgi() else {
            return false;
        };
        let t = Instant::now();
        let gpu = match d.enable_gpu(max) {
            Ok(g) => g,
            Err(e) => {
                failed(&format!("no video processor: {e}"));
                return false;
            }
        };
        if let Err(e) = d.check_gpu_colours(max) {
            if !software_test() {
                d.disable_gpu();
                failed(&e.to_string());
                return false;
            }
            tracing::warn!(error = %e, "colour check failed; continuing (texture encoder test)");
        }
        let size = match d.output_size() {
            Ok((w, h)) => rconn_media::d3d11::fitted(w, h, max),
            Err(e) => {
                d.disable_gpu();
                failed(&e.to_string());
                return false;
            }
        };
        match enc.use_gpu(&gpu, size) {
            Ok(()) => {
                tracing::info!(
                    adapter = %gpu.adapter,
                    engine = enc.engine_name(),
                    setup_ms = t.elapsed().as_millis() as u64,
                    "frames stay on the GPU (capture, convert, encode)"
                );
                true
            }
            Err(e) => {
                d.disable_gpu();
                failed(&format!("no hardware encoder takes textures: {e}"));
                false
            }
        }
    }
}

#[cfg(not(windows))]
mod gpu {
    use super::*;

    pub(super) fn try_enable(
        _: &mut dyn Capturer,
        _: &mut VideoEncoder,
        _: (usize, usize),
    ) -> bool {
        false
    }
}
