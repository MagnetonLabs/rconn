//! macOS: the main display's mode through CoreGraphics display configuration. Modes are
//! compared in pixels (what is captured and sent) and only modes of the current kind are
//! used: on a Retina panel, "looks like" HiDPI modes stay HiDPI. The change is made for
//! this process only (`kCGConfigureForAppOnly`): should the host stop without putting it
//! back, macOS restores the mode itself.

#![allow(unsafe_code)] // CoreGraphics display configuration FFI

use super::{Mode, pick_fit};
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFRetained, CFString};
use objc2_core_graphics::{
    CGBeginDisplayConfiguration, CGCancelDisplayConfiguration, CGCompleteDisplayConfiguration,
    CGConfigureDisplayWithDisplayMode, CGConfigureOption, CGDirectDisplayID, CGDisplayConfigRef,
    CGDisplayCopyAllDisplayModes, CGDisplayCopyDisplayMode, CGDisplayMode, CGError,
    CGMainDisplayID, CGRestorePermanentDisplayConfiguration,
    kCGDisplayShowDuplicateLowResolutionModes,
};
use std::sync::Mutex;

/// The mode before this host's first change.
struct Before {
    display: CGDirectDisplayID,
    mode: Key,
}

static BEFORE: Mutex<Option<Before>> = Mutex::new(None);

/// What identifies a mode across `CGDisplayCopyAllDisplayModes` calls.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Key {
    io_id: i32,
    points: (usize, usize),
    pixels: (usize, usize),
    hz: f64,
}

fn key(m: &CGDisplayMode) -> Key {
    let m = Some(m);
    Key {
        io_id: CGDisplayMode::io_display_mode_id(m),
        points: (CGDisplayMode::width(m), CGDisplayMode::height(m)),
        pixels: (
            CGDisplayMode::pixel_width(m),
            CGDisplayMode::pixel_height(m),
        ),
        hz: CGDisplayMode::refresh_rate(m),
    }
}

impl Key {
    /// Pixels per point (2 on a Retina "looks like" mode).
    fn density(&self) -> f64 {
        self.pixels.0 as f64 / self.points.0.max(1) as f64
    }

    fn mode(&self) -> Mode {
        let px = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
        Mode {
            width: px(self.pixels.0),
            height: px(self.pixels.1),
            hz: self.hz,
        }
    }
}

/// The display's desktop modes, HiDPI ones included (macOS lists only 1x modes unless
/// asked for the "duplicate low resolution" ones).
fn modes(display: CGDirectDisplayID) -> Vec<CFRetained<CGDisplayMode>> {
    // SAFETY: a CoreGraphics constant key. The array holds CGDisplayModes (documented);
    // `to_vec` retains each one.
    unsafe {
        let options = CFDictionary::<CFString, CFBoolean>::from_slices(
            &[kCGDisplayShowDuplicateLowResolutionModes],
            &[CFBoolean::new(true)],
        );
        CGDisplayCopyAllDisplayModes(display, Some(options.as_opaque()))
            .map(|a: CFRetained<CFArray>| a.cast_unchecked::<CGDisplayMode>().to_vec())
            .unwrap_or_default()
    }
    .into_iter()
    .filter(|m| CGDisplayMode::is_usable_for_desktop_gui(Some(m)))
    .collect()
}

/// Switch `display` to `mode` until this process ends or switches back.
fn switch(display: CGDirectDisplayID, mode: &CGDisplayMode) -> Result<(), String> {
    let mut config: CGDisplayConfigRef = std::ptr::null_mut();
    // SAFETY: plain CoreGraphics calls with a configuration we begin and then complete
    // or cancel, a valid display ID and a mode of that display.
    unsafe {
        let e = CGBeginDisplayConfiguration(&mut config);
        if e != CGError::Success {
            return Err(format!("CGBeginDisplayConfiguration failed ({})", e.0));
        }
        let e = CGConfigureDisplayWithDisplayMode(config, display, Some(mode), None);
        if e != CGError::Success {
            CGCancelDisplayConfiguration(config);
            return Err(format!("the display refused the mode ({})", e.0));
        }
        let e = CGCompleteDisplayConfiguration(config, CGConfigureOption::ForAppOnly);
        if e != CGError::Success {
            return Err(format!("CGCompleteDisplayConfiguration failed ({})", e.0));
        }
    }
    Ok(())
}

pub fn set(width: u32, height: u32) -> Result<Option<(u32, u32)>, String> {
    let display = CGMainDisplayID();
    let current = CGDisplayCopyDisplayMode(display)
        .map(|m| key(&m))
        .ok_or("can't read the current display mode")?;
    let modes: Vec<_> = modes(display)
        .into_iter()
        .filter(|m| (key(m).density() - current.density()).abs() < 0.01)
        .collect();
    let keys: Vec<Mode> = modes.iter().map(|m| key(m).mode()).collect();
    let i = pick_fit(&keys, (width, height), current.hz).ok_or("the display lists no modes")?;
    let target = keys[i];
    if (target.width, target.height) == (current.mode().width, current.mode().height) {
        return Ok(None);
    }
    switch(display, &modes[i])?;
    BEFORE.lock().expect("display state").get_or_insert(Before {
        display,
        mode: current,
    });
    Ok(Some((target.width, target.height)))
}

pub fn restore() {
    let Some(before) = BEFORE.lock().expect("display state").take() else {
        return;
    };
    let same = |k: Key| {
        k.io_id == before.mode.io_id
            && k.points == before.mode.points
            && k.pixels == before.mode.pixels
            && (k.hz - before.mode.hz).abs() < 0.01
    };
    let r = match modes(before.display).into_iter().find(|m| same(key(m))) {
        Some(m) => switch(before.display, &m),
        None => Err("the original mode is no longer listed".into()),
    };
    if let Err(e) = r {
        // Undoes every change this process made to the displays.
        tracing::warn!(error = %e, "restoring the user's display settings");
        CGRestorePermanentDisplayConfiguration();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read-only: lists the main display's modes and checks what a fit would choose
    /// (nothing is switched). A machine without a display is skipped.
    #[test]
    fn lists_the_main_display_modes() {
        let display = CGMainDisplayID();
        let Some(current) = CGDisplayCopyDisplayMode(display).map(|m| key(&m)) else {
            eprintln!("skipped: no display");
            return;
        };
        let keys: Vec<Mode> = modes(display)
            .iter()
            .map(|m| key(m))
            .filter(|k| (k.density() - current.density()).abs() < 0.01)
            .map(|k| k.mode())
            .collect();
        eprintln!(
            "current {current:?}; {} modes of density {}",
            keys.len(),
            current.density()
        );
        assert!(!keys.is_empty(), "modes of the current kind are listed");
        let want = (current.mode().width, current.mode().height);
        let i = pick_fit(&keys, want, current.hz).expect("a mode");
        assert_eq!((keys[i].width, keys[i].height), want);
    }
}
