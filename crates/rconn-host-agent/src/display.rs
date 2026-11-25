//! Fit this computer's screen to the viewer's window, as AnyDesk and RustDesk do: the
//! viewer reports its picture area in pixels and the host switches to the closest
//! display mode. The original mode comes back when the session ends.
//!
//! Windows only for now; elsewhere requests are ignored (the viewer scales the picture).

/// The screen-size change made for this session, undone on drop.
#[derive(Default)]
pub struct Fit {
    changed: bool,
}

impl Fit {
    /// Switch to the display mode closest to `width` x `height`.
    pub fn apply(&mut self, width: u32, height: u32) {
        match imp::set(width, height) {
            Ok(Some((w, h))) => {
                self.changed = true;
                tracing::info!(
                    want = format!("{width}x{height}"),
                    set = format!("{w}x{h}"),
                    "screen fitted to the viewer's window"
                );
            }
            Ok(None) => tracing::debug!(width, height, "screen already fits"),
            Err(e) => tracing::warn!(error = %e, width, height, "could not fit the screen"),
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

/// Of `modes`, the one closest to `want`: among modes of a similar size (so the desktop
/// is neither cramped nor tiny), the closest shape (black bars), then the nearest size.
/// If no mode is of a similar size, the nearest size overall.
#[cfg_attr(not(windows), allow(dead_code))]
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

#[cfg(not(windows))]
mod imp {
    pub fn set(_width: u32, _height: u32) -> Result<Option<(u32, u32)>, String> {
        Err("fitting the screen to the viewer is Windows-only for now".into())
    }

    pub fn restore() {}
}

#[cfg(test)]
mod tests {
    use super::closest;

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
