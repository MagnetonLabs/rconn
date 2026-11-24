//! Screen capture backends. Each produces tightly packed BGRA [`Frame`]s.
//!
//! * Windows: DXGI Desktop Duplication ([`dxgi`]): GPU-side copy of the desktop, works on
//!   basic display adapters in VMs; only returns frames when something changed.
//! * macOS: ScreenCaptureKit ([`macos`]): an `SCStream` of the main display at native
//!   pixel size; needs the Screen Recording permission. Also only sends changed frames.
//! * [`TestPattern`]: synthetic moving content, for pipelines without a desktop.

use crate::video::Frame;
use std::time::{Duration, Instant};
use thiserror::Error;

#[cfg(windows)]
pub mod dxgi;
#[cfg(target_os = "macos")]
pub mod macos;

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("no display output to capture")]
    NoOutput,
    #[error("desktop format {0} is not supported (HDR?)")]
    UnsupportedFormat(u32),
    #[error("capture backend error: {0}")]
    Backend(String),
}

pub trait Capturer: Send {
    /// Wait up to `timeout` for a changed frame. `Ok(None)` means nothing changed.
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError>;
}

/// The platform's default capturer for the primary display.
pub fn primary_display() -> Result<Box<dyn Capturer>, CaptureError> {
    #[cfg(windows)]
    {
        Ok(Box::new(dxgi::DxgiCapturer::new(0)?))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::MacCapturer::new()?))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Err(CaptureError::Backend(
            "no capture backend for this OS yet".into(),
        ))
    }
}

/// Synthetic content: gradient plus a moving box, `fps` frames per second.
pub struct TestPattern {
    width: usize,
    height: usize,
    interval: Duration,
    next: Instant,
    t: usize,
}

impl TestPattern {
    pub fn new(width: usize, height: usize, fps: u32) -> Self {
        Self {
            width,
            height,
            interval: Duration::from_secs(1) / fps.max(1),
            next: Instant::now(),
            t: 0,
        }
    }
}

impl Capturer for TestPattern {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame>, CaptureError> {
        let now = Instant::now();
        if self.next > now {
            let wait = self.next - now;
            if wait > timeout {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            std::thread::sleep(wait);
        }
        self.next += self.interval;
        let (w, h) = (self.width, self.height);
        let mut bgra = vec![0u8; w * h * 4];
        let bx = (self.t * 8) % w.max(1);
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                let boxed = (bx..bx + w / 4).contains(&x) && (h / 6..h / 2).contains(&y);
                let px = if boxed {
                    [255, 255, 255, 255]
                } else {
                    [(x * 255 / w) as u8, (y * 255 / h) as u8, 80, 255]
                };
                bgra[i..i + 4].copy_from_slice(&px);
            }
        }
        self.t += 1;
        Ok(Some(Frame {
            width: w,
            height: h,
            bgra,
            captured_at: Instant::now(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pattern_paces_and_moves() {
        let mut p = TestPattern::new(64, 48, 100);
        let a = p.next_frame(Duration::from_millis(50)).unwrap().unwrap();
        let b = p.next_frame(Duration::from_millis(50)).unwrap().unwrap();
        assert_eq!((a.width, a.height, a.bgra.len()), (64, 48, 64 * 48 * 4));
        assert_ne!(a.bgra, b.bgra, "content should move between frames");
    }
}
