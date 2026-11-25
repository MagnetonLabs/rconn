//! Linux (X11) input: XTest fake events on the root window. Wayland sessions inject
//! through the remote-desktop portal instead.

use super::Injector;
use rconn_core::hid::to_linux_evdev;
use rconn_core::protocol::MouseButton;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt as _, KEY_PRESS_EVENT,
    KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

/// X keycodes are evdev codes plus 8 (the evdev and xkb drivers both use this offset).
const X_KEYCODE_OFFSET: u16 = 8;

pub struct XTestInjector {
    conn: RustConnection,
    root: Window,
    wheel: WheelAccumulator,
}

impl XTestInjector {
    pub fn new() -> anyhow::Result<Self> {
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        conn.xtest_get_version(2, 2)?.reply()?;
        Ok(Self {
            conn,
            root,
            wheel: WheelAccumulator::default(),
        })
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) {
        let sent = self
            .conn
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)
            .map(|_| ())
            .and_then(|()| self.conn.flush());
        if let Err(e) = sent {
            tracing::warn!(error = %e, "XTest input failed");
        }
    }

    /// The root window's current size (it changes with the resolution).
    fn screen_size(&self) -> (f32, f32) {
        self.conn
            .get_geometry(self.root)
            .ok()
            .and_then(|c| c.reply().ok())
            .map_or((1.0, 1.0), |g| (g.width.into(), g.height.into()))
    }

    fn click(&self, button: u8) {
        self.fake(BUTTON_PRESS_EVENT, button, 0, 0);
        self.fake(BUTTON_RELEASE_EVENT, button, 0, 0);
    }
}

/// X button numbers: 1-3 left/middle/right, 8/9 back/forward (4-7 are the wheel).
pub fn x_button(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
        MouseButton::Back => 8,
        MouseButton::Forward => 9,
    }
}

impl Injector for XTestInjector {
    fn pointer_move(&mut self, x: f32, y: f32) {
        let (w, h) = self.screen_size();
        let px = (x * (w - 1.0)).round() as i16;
        let py = (y * (h - 1.0)).round() as i16;
        self.fake(MOTION_NOTIFY_EVENT, 0, px, py);
    }

    fn button(&mut self, button: MouseButton, pressed: bool) {
        let kind = if pressed {
            BUTTON_PRESS_EVENT
        } else {
            BUTTON_RELEASE_EVENT
        };
        self.fake(kind, x_button(button), 0, 0);
    }

    fn wheel(&mut self, dx: f32, dy: f32) {
        // Positive = up / left (the viewer's convention). X: 4 up, 5 down, 6 left, 7 right.
        let (lx, ly) = self.wheel.add(dx, dy);
        for _ in 0..ly.unsigned_abs() {
            self.click(if ly > 0 { 4 } else { 5 });
        }
        for _ in 0..lx.unsigned_abs() {
            self.click(if lx > 0 { 6 } else { 7 });
        }
    }

    fn key(&mut self, hid_usage: u16, pressed: bool) {
        let Some(code) = to_linux_evdev(hid_usage) else {
            tracing::debug!(hid_usage, "unmapped key");
            return;
        };
        let Ok(keycode) = u8::try_from(code + X_KEYCODE_OFFSET) else {
            return;
        };
        let kind = if pressed {
            KEY_PRESS_EVENT
        } else {
            KEY_RELEASE_EVENT
        };
        self.fake(kind, keycode, 0, 0);
    }

    fn caps_lock_on(&mut self) -> bool {
        // LED 1 is Caps Lock on X servers.
        self.conn
            .get_keyboard_control()
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|k| k.led_mask & 1 != 0)
    }
}

/// X scrolls in whole wheel clicks; carry fractions so slow trackpads still scroll.
#[derive(Default)]
struct WheelAccumulator {
    x: f32,
    y: f32,
}

impl WheelAccumulator {
    fn add(&mut self, dx: f32, dy: f32) -> (i32, i32) {
        self.x += dx;
        self.y += dy;
        let (lx, ly) = (self.x.trunc(), self.y.trunc());
        self.x -= lx;
        self.y -= ly;
        (lx as i32, ly as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_and_wheel_map_to_x_conventions() {
        assert_eq!(x_button(MouseButton::Left), 1);
        assert_eq!(x_button(MouseButton::Right), 3);
        let mut w = WheelAccumulator::default();
        assert_eq!(w.add(0.0, 0.6), (0, 0));
        assert_eq!(w.add(0.0, 0.6), (0, 1));
        assert_eq!(w.add(-2.0, 0.0), (-2, 0));
    }

    /// Needs an X server with XTest (`xvfb-run cargo test`); skipped otherwise.
    #[test]
    fn pointer_moves_where_asked() {
        if std::env::var_os("DISPLAY").is_none() {
            eprintln!("skipped: no DISPLAY");
            return;
        }
        let mut inj = XTestInjector::new().expect("XTest");
        let (w, h) = inj.screen_size();
        inj.pointer_move(0.5, 0.25);
        let p = inj.conn.query_pointer(inj.root).unwrap().reply().unwrap();
        let want = (
            (0.5 * (w - 1.0)).round() as i16,
            (0.25 * (h - 1.0)).round() as i16,
        );
        assert_eq!((p.root_x, p.root_y), want);

        // A key press and release (A) goes through without errors.
        inj.key(rconn_core::hid::usage::A, true);
        inj.key(rconn_core::hid::usage::A, false);
        assert!(!inj.caps_lock_on());
    }
}
