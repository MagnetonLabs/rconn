//! Windows `SendInput` injection.
//!
//! Unsafe is confined to this module (Win32 FFI): `SendInput` with stack-built `INPUT`
//! structs, and desktop attachment for the calling thread.
#![allow(unsafe_code)]

use super::Injector;
use rconn_core::hid::to_windows_scancode;
use rconn_core::protocol::MouseButton;
use windows::Win32::System::StationsAndDesktops::{
    DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, OpenInputDesktop, SetThreadDesktop,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    SendInput, VIRTUAL_KEY, VK_CAPITAL,
};

const WHEEL_DELTA: f32 = 120.0;
const XBUTTON1: u32 = 1;
const XBUTTON2: u32 = 2;

#[derive(Default)]
pub struct SendInputInjector {
    attached: bool,
}

impl SendInputInjector {
    /// Attach this thread to the desktop receiving input (user desktop or Winlogon).
    fn attach(&mut self) {
        const GENERIC_ALL: u32 = 0x1000_0000;
        let ok = unsafe {
            OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(GENERIC_ALL),
            )
            .and_then(|d| SetThreadDesktop(d))
            .is_ok()
        };
        if !ok {
            tracing::debug!("could not attach input thread to the input desktop");
        }
        self.attached = ok;
    }

    /// Send one INPUT; if the desktop switched under us (0 injected), re-attach and retry.
    fn send(&mut self, input: INPUT) {
        if !self.attached {
            self.attach();
        }
        let size = std::mem::size_of::<INPUT>() as i32;
        if unsafe { SendInput(&[input], size) } == 0 {
            self.attach();
            if unsafe { SendInput(&[input], size) } == 0 {
                tracing::debug!("SendInput injected nothing (desktop locked to us?)");
            }
        }
    }

    fn mouse(&mut self, dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) {
        self.send(INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
    }
}

impl Injector for SendInputInjector {
    fn caps_lock_on(&mut self) -> bool {
        if !self.attached {
            self.attach();
        }
        // Low bit of GetKeyState = toggle state.
        unsafe { GetKeyState(i32::from(VK_CAPITAL.0)) & 1 == 1 }
    }

    fn pointer_move(&mut self, x: f32, y: f32) {
        // Absolute coordinates are 0..=65535 across the primary display, which is what we
        // capture (output 0).
        let (ax, ay) = ((x * 65535.0).round() as i32, (y * 65535.0).round() as i32);
        self.mouse(ax, ay, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE);
    }

    fn button(&mut self, button: MouseButton, pressed: bool) {
        let (flags, data) = match (button, pressed) {
            (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
            (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
            (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
            (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
        };
        self.mouse(0, 0, data, flags);
    }

    fn wheel(&mut self, dx: f32, dy: f32) {
        if dy != 0.0 {
            self.mouse(
                0,
                0,
                (dy * WHEEL_DELTA).round() as i32 as u32,
                MOUSEEVENTF_WHEEL,
            );
        }
        if dx != 0.0 {
            self.mouse(
                0,
                0,
                (dx * WHEEL_DELTA).round() as i32 as u32,
                MOUSEEVENTF_HWHEEL,
            );
        }
    }

    fn key(&mut self, hid_usage: u16, pressed: bool) {
        let Some(code) = to_windows_scancode(hid_usage) else {
            tracing::debug!(hid_usage, "unmapped key");
            return;
        };
        let mut flags: KEYBD_EVENT_FLAGS = KEYEVENTF_SCANCODE;
        if code.extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !pressed {
            flags |= KEYEVENTF_KEYUP;
        }
        self.send(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: code.code,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
    }
}

/// Self-test: inject an absolute move to 25%/25% of the primary display, read the cursor
/// back, then restore it. Returns (expected, actual) in pixels.
pub fn selftest_pointer() -> windows::core::Result<((i32, i32), (i32, i32))> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN, SetCursorPos,
    };
    let mut inj = SendInputInjector::default();
    inj.attach();
    unsafe {
        let mut saved = POINT::default();
        GetCursorPos(&mut saved)?;
        let (w, h) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        inj.pointer_move(0.25, 0.25);
        std::thread::sleep(std::time::Duration::from_millis(50));
        let mut now = POINT::default();
        GetCursorPos(&mut now)?;
        let _ = SetCursorPos(saved.x, saved.y);
        Ok(((w / 4, h / 4), (now.x, now.y)))
    }
}
