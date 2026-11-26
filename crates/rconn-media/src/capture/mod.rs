//! Screen capture backends. Each produces tightly packed BGRA [`Frame`]s.
//!
//! * Windows: DXGI Desktop Duplication ([`dxgi`]): GPU-side copy of the desktop, works on
//!   basic display adapters in VMs; only returns frames when something changed.
//! * macOS: ScreenCaptureKit ([`macos`]): an `SCStream` of the main display at native
//!   pixel size; needs the Screen Recording permission. Also only sends changed frames.
//! * Linux (X11): MIT-SHM reads of the root window, converting only what XDamage reports
//!   changed ([`x11`]).
//! * Linux (Wayland, e.g. standard Ubuntu/GNOME): the desktop portal's PipeWire screen
//!   stream, converting only what the compositor reports as damaged ([`portal`]). The
//!   person at the computer allows it once; the choice is remembered.
//! * [`TestPattern`]: synthetic moving content, for pipelines without a desktop.

use crate::video::Frame;
use crate::yuv::I420;
use std::time::{Duration, Instant};
use thiserror::Error;

#[cfg(windows)]
pub mod dxgi;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "linux")]
pub mod portal;
#[cfg(target_os = "linux")]
pub mod x11;

/// A captured picture still on the GPU, handed straight to a hardware encoder (macOS:
/// ScreenCaptureKit's IOSurface-backed pixel buffer, for VideoToolbox).
#[cfg(target_os = "macos")]
pub use macos::Surface;

/// A captured picture still on the GPU. Only macOS has them so far.
#[cfg(not(target_os = "macos"))]
pub enum Surface {}

#[cfg(not(target_os = "macos"))]
impl Surface {
    pub fn dimensions(&self) -> (usize, usize) {
        match *self {}
    }
}

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

    /// Wait up to `timeout` for a change and apply it to `picture`, which holds the
    /// previous picture this method produced (or `None`: it is allocated at the right
    /// size, as it is after a size change). Frames larger than `max` are halved until
    /// they fit. Backends that know which areas changed convert only those.
    fn next_into(
        &mut self,
        timeout: Duration,
        picture: &mut Option<I420>,
        max: (usize, usize),
    ) -> Result<Option<Captured>, CaptureError> {
        let Some(frame) = self.next_frame(timeout)? else {
            return Ok(None);
        };
        Ok(convert_whole(frame, picture, max))
    }

    /// Deliver GPU surfaces ([`Capturer::next_surface`]) from now on instead of converted
    /// pictures, at most `max` in size (halved to fit, as [`Capturer::next_into`] does).
    /// `false` if this backend has none; it then carries on as before.
    fn use_surfaces(&mut self, _max: (usize, usize)) -> bool {
        false
    }

    /// Surface mode: wait up to `timeout` for a change. `Ok(None)` means nothing changed.
    fn next_surface(
        &mut self,
        _timeout: Duration,
    ) -> Result<Option<(Surface, Captured)>, CaptureError> {
        Err(CaptureError::Backend(
            "this capturer has no GPU surfaces".into(),
        ))
    }

    /// Areas the OS says were moved (scrolled, dragged) in the pictures given out since
    /// the last call, in picture coordinates (Windows: DXGI move rectangles). Hints only:
    /// [`crate::copyrect::Composer`] checks them against the pixels.
    fn take_moves(&mut self) -> Vec<crate::copyrect::Move> {
        Vec::new()
    }
}

/// What [`Capturer::next_into`] changed.
#[derive(Clone, Copy, Debug)]
pub struct Captured {
    /// The even row range of `picture` that changed.
    pub rows: (usize, usize),
    /// Share of the picture's area that changed (0..=1; overlapping areas may add up).
    pub changed: f32,
    pub captured_at: Instant,
}

/// Convert a whole frame (halved to fit `max`) into `picture`.
pub fn convert_whole(
    frame: Frame,
    picture: &mut Option<I420>,
    max: (usize, usize),
) -> Option<Captured> {
    let frame = frame.fit_within(max.0, max.1);
    let (w, h) = (frame.width & !1, frame.height & !1);
    if w == 0 || h == 0 {
        return None;
    }
    let pic = match picture {
        Some(p) if p.dimensions() == (w, h) => p,
        _ => picture.insert(I420::new(w, h)),
    };
    pic.fill_from_bgra(&frame.bgra, frame.width);
    Some(Captured {
        rows: (0, h),
        changed: 1.0,
        captured_at: frame.captured_at,
    })
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
    #[cfg(target_os = "linux")]
    {
        if uses_portal() {
            Ok(Box::new(portal::PortalCapturer::new()?))
        } else {
            Ok(Box::new(x11::X11Capturer::new()?))
        }
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        Err(CaptureError::Backend(
            "no capture backend for this OS yet".into(),
        ))
    }
}

/// Linux: is this a Wayland desktop, where capture and input go through the desktop
/// portal (rather than X11)? Input injection asks the same question.
#[cfg(target_os = "linux")]
pub fn uses_portal() -> bool {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let socket = env("XDG_RUNTIME_DIR").is_some_and(|d| {
        let name = env("WAYLAND_DISPLAY").unwrap_or_else(|| "wayland-0".into());
        std::path::Path::new(&d).join(name).exists()
    });
    let backend = linux_backend(
        env("RCONN_CAPTURE").as_deref(),
        env("XDG_SESSION_TYPE").as_deref(),
        env("WAYLAND_DISPLAY").is_some(),
        socket,
    );
    tracing::debug!(?backend, "Linux desktop backend");
    backend == LinuxBackend::Portal
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxBackend {
    X11,
    Portal,
}

/// Wayland or X11? On a Wayland desktop, X11 capture would only see Xwayland windows,
/// so any sign of Wayland picks the portal. `RCONN_CAPTURE=x11|portal` overrides. A
/// systemd user unit may lack the session variables, so a Wayland socket in
/// `XDG_RUNTIME_DIR` counts too.
#[cfg(target_os = "linux")]
fn linux_backend(
    forced: Option<&str>,
    session_type: Option<&str>,
    wayland_display: bool,
    wayland_socket: bool,
) -> LinuxBackend {
    match forced {
        Some("x11") => return LinuxBackend::X11,
        Some("portal" | "wayland") => return LinuxBackend::Portal,
        _ => {}
    }
    match session_type {
        Some("wayland") => LinuxBackend::Portal,
        Some("x11") => LinuxBackend::X11,
        _ if wayland_display || wayland_socket => LinuxBackend::Portal,
        _ => LinuxBackend::X11,
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

    #[cfg(target_os = "linux")]
    #[test]
    fn wayland_desktops_use_the_portal() {
        use LinuxBackend::*;
        // Ubuntu 24.04 default (GNOME on Wayland; DISPLAY is Xwayland's).
        assert_eq!(linux_backend(None, Some("wayland"), true, true), Portal);
        // "Ubuntu on Xorg".
        assert_eq!(linux_backend(None, Some("x11"), false, false), X11);
        // systemd user unit without the session variables.
        assert_eq!(linux_backend(None, None, false, true), Portal);
        assert_eq!(linux_backend(None, None, true, false), Portal);
        assert_eq!(linux_backend(None, None, false, false), X11);
        assert_eq!(linux_backend(Some("x11"), Some("wayland"), true, true), X11);
        assert_eq!(
            linux_backend(Some("portal"), Some("x11"), false, false),
            Portal
        );
    }

    #[test]
    fn test_pattern_paces_and_moves() {
        let mut p = TestPattern::new(64, 48, 100);
        let a = p.next_frame(Duration::from_millis(50)).unwrap().unwrap();
        let b = p.next_frame(Duration::from_millis(50)).unwrap().unwrap();
        assert_eq!((a.width, a.height, a.bgra.len()), (64, 48, 64 * 48 * 4));
        assert_ne!(a.bgra, b.bgra, "content should move between frames");
    }
}
