//! Fit this computer's screen to the viewer's window, as AnyDesk and RustDesk do: the
//! viewer reports its picture area in pixels and the host switches to the closest
//! display mode. The original mode comes back when the session ends.
//!
//! * Windows: `ChangeDisplaySettingsExW` (not saved; the registry mode comes back).
//! * macOS: CoreGraphics display modes of the main display ([`macos`]).
//! * Linux, GNOME on Wayland: Mutter's `DisplayConfig` D-Bus API ([`mutter`]).
//! * Linux, X11: RandR on the primary output ([`randr`]).
//!
//! Only modes the display lists are used. Where fitting fails (another Wayland desktop,
//! mirrored displays...) the session goes on and the viewer scales the picture.

#[cfg(target_os = "macos")]
mod macos;
mod mutter;
#[cfg(target_os = "linux")]
mod randr;

/// The screen-size change made for this session, undone on drop.
#[derive(Default)]
pub struct Fit {
    changed: bool,
}

impl Fit {
    /// Switch to the display mode closest to `width` x `height`. Returns the size set,
    /// if the screen changed.
    pub fn apply(&mut self, width: u32, height: u32) -> Option<(u32, u32)> {
        match imp::set(width, height) {
            Ok(Some((w, h))) => {
                self.changed = true;
                tracing::info!(
                    want = format!("{width}x{height}"),
                    set = format!("{w}x{h}"),
                    "screen fitted to the viewer's window"
                );
                Some((w, h))
            }
            Ok(None) => {
                tracing::debug!(width, height, "screen already fits");
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, width, height, "could not fit the screen");
                None
            }
        }
    }

    /// Put the original display mode back (if this session changed it).
    pub fn restore(&mut self) {
        if std::mem::take(&mut self.changed) {
            imp::restore();
            tracing::info!("screen size restored");
        }
    }
}

impl Drop for Fit {
    fn drop(&mut self) {
        self.restore();
    }
}

/// `--selftest-fit`: fit the screen to `want` as a viewer would and watch the captured
/// picture for a few seconds, then put the screen back and watch again. Capture errors
/// restart the capturer, as a session's stream does. On Wayland, the portal restore token
/// in the host's `state_dir` (if given) avoids the consent dialog.
pub fn selftest(want: (u32, u32), state_dir: Option<&std::path::Path>) -> anyhow::Result<String> {
    use anyhow::Context as _;
    use rconn_media::capture::{Capturer, primary_display};
    use rconn_media::yuv::I420;
    use std::time::{Duration, Instant};
    #[cfg(target_os = "linux")]
    if let Some(dir) = state_dir {
        rconn_media::portal::set_restore_token_path(dir.join("portal-restore-token"));
    }
    #[cfg(not(target_os = "linux"))]
    let _ = state_dir;
    const WATCH: Duration = Duration::from_secs(8);
    let mut cap: Option<Box<dyn Capturer>> = Some(primary_display()?);
    let mut picture: Option<I420> = None;
    let mut restarts = 0;
    // The sizes captured within `WATCH` (each new one with its time), the last first.
    let mut watch = |what: &str| -> Vec<((usize, usize), u128)> {
        let start = Instant::now();
        let mut seen: Vec<((usize, usize), u128)> = Vec::new();
        while start.elapsed() < WATCH {
            let c = match cap.as_mut() {
                Some(c) => c,
                None => match primary_display() {
                    Ok(c) => cap.insert(c),
                    Err(e) => {
                        tracing::info!(error = %e, "capture not back yet");
                        std::thread::sleep(Duration::from_millis(250));
                        continue;
                    }
                },
            };
            match c.next_into(Duration::from_millis(100), &mut picture, (16384, 16384)) {
                Ok(Some(_)) => {
                    let size = picture.as_ref().map_or((0, 0), I420::dimensions);
                    if seen.last().is_none_or(|&(s, _)| s != size) {
                        seen.push((size, start.elapsed().as_millis()));
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::info!(error = %e, what, "capture restarting");
                    restarts += 1;
                    cap = None;
                    picture = None;
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }
        seen
    };
    let show = |seen: &[((usize, usize), u128)]| {
        seen.iter()
            .map(|((w, h), ms)| format!("{w}x{h} at {ms} ms"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let first = watch("before");
    let &(before, _) = first.last().context("no picture before fitting")?;
    let mut fit = Fit::default();
    let set = fit
        .apply(want.0, want.1)
        .context("the screen size was not changed (see the log above)")?;
    let fitted = watch("fitted");
    fit.restore();
    let restored = watch("restored");
    let report = format!(
        "captured {}x{}; asked {}x{}, display set to {}x{}; then captured {}; restored, \
         then captured {}; {restarts} capture restarts",
        before.0,
        before.1,
        want.0,
        want.1,
        set.0,
        set.1,
        show(&fitted),
        show(&restored)
    );
    let back = restored.last().map(|&(s, _)| s);
    anyhow::ensure!(
        fitted.last().is_some_and(|&(s, _)| s != before) && back == Some(before),
        "screen fit failed: {report}"
    );
    Ok(format!("screen fit ok: {report}"))
}

/// Of `modes`, the one closest to `want`: among modes of a similar size (so the desktop
/// is neither cramped nor tiny), the closest shape (black bars), then the nearest size.
/// If no mode is of a similar size, the nearest size overall.
pub fn closest(modes: &[(u32, u32)], want: (u32, u32)) -> Option<(u32, u32)> {
    let aspect = |(w, h): (u32, u32)| f64::from(w) / f64::from(h.max(1));
    let area_ratio = |m: (u32, u32)| {
        f64::from(m.0) * f64::from(m.1) / (f64::from(want.0) * f64::from(want.1)).max(1.0)
    };
    let shape = |m: (u32, u32)| (aspect(m) / aspect(want)).ln().abs();
    let size = |m: (u32, u32)| area_ratio(m).ln().abs();
    let similar: Vec<(u32, u32)> = modes
        .iter()
        .copied()
        .filter(|&m| (0.7..=1.4).contains(&area_ratio(m)))
        .collect();
    if similar.is_empty() {
        return modes
            .iter()
            .copied()
            .min_by(|&a, &b| size(a).total_cmp(&size(b)));
    }
    similar
        .into_iter()
        .min_by(|&a, &b| (shape(a) * 50.0 + size(a)).total_cmp(&(shape(b) * 50.0 + size(b))))
}

/// A display mode: its size in pixels and refresh rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub hz: f64,
}

/// Of `modes`, the one to switch to for a viewer of `want` (see [`closest`]); of several
/// with that size, the one with the refresh rate nearest `hz` (the current one).
#[cfg_attr(windows, allow(dead_code))]
pub fn pick_fit(modes: &[Mode], want: (u32, u32), hz: f64) -> Option<usize> {
    let mut sizes: Vec<(u32, u32)> = modes.iter().map(|m| (m.width, m.height)).collect();
    sizes.sort_unstable();
    sizes.dedup();
    nearest_hz(modes, closest(&sizes, want)?, hz)
}

/// Of `modes`, the one that brings back `original` once the session ends: that size;
/// else the same shape (a VM display that follows its window lists new modes as the
/// window changes shape, and the window follows the shape back), nearest in size; else
/// the display's `preferred` mode.
#[cfg_attr(windows, allow(dead_code))]
pub fn pick_restore(modes: &[Mode], original: Mode, preferred: Option<usize>) -> Option<usize> {
    if let Some(i) = nearest_hz(modes, (original.width, original.height), original.hz) {
        return Some(i);
    }
    let aspect = |m: &Mode| f64::from(m.width) / f64::from(m.height.max(1));
    let area = |m: &Mode| f64::from(m.width) * f64::from(m.height);
    let shape = |m: &Mode| (aspect(m) / aspect(&original)).ln().abs();
    let size = |m: &Mode| (area(m) / area(&original).max(1.0)).ln().abs();
    modes
        .iter()
        .enumerate()
        .filter(|(_, m)| shape(m) < 0.02)
        .min_by(|a, b| size(a.1).total_cmp(&size(b.1)))
        .map(|(i, _)| i)
        .or(preferred.filter(|&i| i < modes.len()))
}

/// The mode of `size` whose refresh rate is nearest `hz`.
#[cfg_attr(windows, allow(dead_code))]
fn nearest_hz(modes: &[Mode], size: (u32, u32), hz: f64) -> Option<usize> {
    modes
        .iter()
        .enumerate()
        .filter(|(_, m)| (m.width, m.height) == size)
        .min_by(|a, b| (a.1.hz - hz).abs().total_cmp(&(b.1.hz - hz).abs()))
        .map(|(i, _)| i)
}

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 FFI: EnumDisplaySettingsW, ChangeDisplaySettingsExW, desktops
mod imp {
    use windows::Win32::Graphics::Gdi::{
        CDS_TEST, CDS_TYPE, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL,
        DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE,
        EnumDisplaySettingsW,
    };
    use windows::Win32::System::StationsAndDesktops::{
        DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, OpenInputDesktop, SetThreadDesktop,
    };
    use windows::core::PCWSTR;

    /// Display changes must come from a thread on the desktop the user sees.
    fn on_input_desktop<T: Send>(f: impl FnOnce() -> T + Send) -> T {
        std::thread::scope(|s| {
            s.spawn(|| {
                unsafe {
                    let _ = OpenInputDesktop(
                        DESKTOP_CONTROL_FLAGS(0),
                        false,
                        DESKTOP_ACCESS_FLAGS(0x1000_0000), // GENERIC_ALL
                    )
                    .and_then(|d| SetThreadDesktop(d));
                }
                f()
            })
            .join()
            .expect("display thread panicked")
        })
    }

    fn devmode() -> DEVMODEW {
        DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        }
    }

    pub fn set(width: u32, height: u32) -> Result<Option<(u32, u32)>, String> {
        on_input_desktop(|| unsafe {
            let mut current = devmode();
            if !EnumDisplaySettingsW(PCWSTR::null(), ENUM_CURRENT_SETTINGS, &mut current).as_bool()
            {
                return Err("can't read the current display mode".into());
            }
            let with = |w: u32, h: u32| {
                let mut m = current;
                m.dmPelsWidth = w;
                m.dmPelsHeight = h;
                m.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
                m
            };
            let accepts = |m: &DEVMODEW| {
                ChangeDisplaySettingsExW(PCWSTR::null(), Some(m), None, CDS_TEST, None)
                    == DISP_CHANGE_SUCCESSFUL
            };
            // The exact size if the display driver takes it (VM drivers usually do),
            // else the closest listed mode.
            let exact = with(width, height);
            let target = if accepts(&exact) {
                (width, height)
            } else {
                let mut modes = Vec::new();
                let mut m = devmode();
                let mut i = 0;
                while EnumDisplaySettingsW(PCWSTR::null(), ENUM_DISPLAY_SETTINGS_MODE(i), &mut m)
                    .as_bool()
                {
                    if m.dmBitsPerPel == current.dmBitsPerPel {
                        modes.push((m.dmPelsWidth, m.dmPelsHeight));
                    }
                    i += 1;
                }
                modes.sort_unstable();
                modes.dedup();
                tracing::info!(
                    want = format!("{width}x{height}"),
                    offered = ?modes,
                    "exact size not supported; choosing the closest display mode"
                );
                super::closest(&modes, (width, height))
                    .ok_or_else(|| "the display lists no modes".to_owned())?
            };
            if target == (current.dmPelsWidth, current.dmPelsHeight) {
                return Ok(None);
            }
            // Flags 0: change now, without saving it as the user's setting.
            let r = ChangeDisplaySettingsExW(
                PCWSTR::null(),
                Some(&with(target.0, target.1)),
                None,
                CDS_TYPE(0),
                None,
            );
            if r == DISP_CHANGE_SUCCESSFUL {
                Ok(Some(target))
            } else {
                Err(format!("display mode change refused ({})", r.0))
            }
        })
    }

    /// Back to the saved (registry) mode, which this module never changes.
    pub fn restore() {
        on_input_desktop(|| unsafe {
            let _ = ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None);
        });
    }
}

#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(target_os = "linux")]
mod imp {
    /// GNOME on Wayland through Mutter; X11 desktops through RandR (on Wayland, RandR
    /// would only change what X clients see).
    pub fn set(width: u32, height: u32) -> Result<Option<(u32, u32)>, String> {
        if rconn_media::capture::uses_portal() {
            super::mutter::set(width, height)
        } else {
            super::randr::set(width, height)
        }
    }

    /// Each puts back only what it changed.
    pub fn restore() {
        super::mutter::restore();
        super::randr::restore();
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod imp {
    pub fn set(_width: u32, _height: u32) -> Result<Option<(u32, u32)>, String> {
        Err("fitting the screen to the viewer is not supported on this system".into())
    }

    pub fn restore() {}
}

#[cfg(test)]
mod tests {
    use super::{Mode, closest, pick_fit, pick_restore};

    fn mode(width: u32, height: u32, hz: f64) -> Mode {
        Mode { width, height, hz }
    }

    #[test]
    fn fits_with_the_current_refresh_rate() {
        let modes = [
            mode(1920, 1080, 60.0),
            mode(1920, 1080, 59.94),
            mode(1920, 1200, 59.95),
            mode(1280, 720, 60.0),
        ];
        assert_eq!(pick_fit(&modes, (1900, 1070), 59.94), Some(1));
        assert_eq!(pick_fit(&modes, (1900, 1070), 60.0), Some(0));
        assert_eq!(pick_fit(&modes, (1900, 1190), 60.0), Some(2));
        assert_eq!(pick_fit(&[], (1900, 1190), 60.0), None);
    }

    #[test]
    fn restores_the_size_else_the_shape_else_the_preferred_mode() {
        let modes = [
            mode(4096, 2160, 60.0),
            mode(1920, 1080, 60.0),
            mode(1920, 1012, 59.98),
            mode(1616, 1012, 59.98),
        ];
        // The original size is still listed.
        assert_eq!(
            pick_restore(&modes, mode(1920, 1012, 59.98), Some(3)),
            Some(2)
        );
        // A Parallels VM replaced its 1920x1012 window mode with 1616x1012 (the shape the
        // viewer had): the original shape comes back through 4096x2160.
        let vm = [modes[0], modes[1], modes[3]];
        assert_eq!(pick_restore(&vm, mode(1920, 1012, 59.98), Some(2)), Some(0));
        // Nothing of that shape: the preferred mode.
        assert_eq!(pick_restore(&vm, mode(1000, 1000, 60.0), Some(2)), Some(2));
        assert_eq!(pick_restore(&vm, mode(1000, 1000, 60.0), None), None);
    }

    #[test]
    fn picks_the_same_shape_then_the_nearest_size() {
        let modes = [
            (1920, 1080),
            (1920, 1200),
            (2560, 1440),
            (2992, 1878),
            (1280, 800),
        ];
        // A wide 16:9 window: a 16:9 mode, the one nearest in size.
        assert_eq!(closest(&modes, (2400, 1350)), Some((2560, 1440)));
        // A Parallels VM on a Mac offers big 16:10 modes and only small 16:9 ones: a
        // similar-size mode wins over a cramped one of the exact shape.
        let vm = [
            (1024, 768),
            (2048, 1152),
            (2560, 1600),
            (2992, 1870),
            (2992, 1934),
        ];
        assert_eq!(closest(&vm, (2992, 1820)), Some((2992, 1870)));
        assert_eq!(closest(&vm, (3024, 1964)), Some((2992, 1934))); // full screen
        // Nothing of a similar size: the nearest size.
        assert_eq!(closest(&vm, (800, 500)), Some((1024, 768)));
        // 16:10 window.
        assert_eq!(closest(&modes, (1900, 1190)), Some((1920, 1200)));
        assert_eq!(closest(&[], (100, 100)), None);
    }
}
