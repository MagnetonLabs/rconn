//! macOS `CGEvent` injection.
//!
//! No unsafe here: objc2-core-graphics exposes the CGEvent calls we use as safe functions.
//! Events are posted at the HID tap, so they behave like real hardware input. Posting
//! needs the Accessibility permission; without it macOS drops events silently, so
//! creation logs a warning when `CGPreflightPostEventAccess` says access is missing.

use super::Injector;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect};
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID,
    CGEventTapLocation, CGEventType, CGMainDisplayID, CGMouseButton, CGPreflightPostEventAccess,
    CGScrollEventUnit,
};
use rconn_core::hid::{to_mac_keycode, usage};
use rconn_core::protocol::MouseButton;
use std::time::{Duration, Instant};

/// macOS's default double-click interval and a few points of slop.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);
const DOUBLE_CLICK_SLOP: f64 = 4.0;

pub struct CgInjector {
    source: Option<CFRetained<CGEventSource>>,
    /// Last pointer position, global display coordinates in points.
    pos: CGPoint,
    /// Buttons held, in press order (drags report the first).
    held: Vec<MouseButton>,
    /// Modifier keys held, as HID usages.
    modifiers: Vec<u16>,
    clicks: Clicks,
    wheel: WheelAccumulator,
}

impl CgInjector {
    pub fn new() -> Self {
        if !CGPreflightPostEventAccess() {
            tracing::warn!(
                "no Accessibility permission: macOS will drop injected input; grant it in \
                 System Settings > Privacy & Security > Accessibility"
            );
        }
        let pos = CGEvent::new(None)
            .map(|e| CGEvent::location(Some(&e)))
            .unwrap_or(CGPoint { x: 0.0, y: 0.0 });
        Self {
            source: CGEventSource::new(CGEventSourceStateID::HIDSystemState),
            pos,
            held: Vec::new(),
            modifiers: Vec::new(),
            clicks: Clicks::default(),
            wheel: WheelAccumulator::default(),
        }
    }

    fn flags(&self) -> CGEventFlags {
        self.modifiers
            .iter()
            .fold(CGEventFlags::empty(), |f, &k| f | modifier_flag(k))
    }

    fn post(&self, event: Option<CFRetained<CGEvent>>) {
        let Some(event) = event else {
            tracing::debug!("CGEvent creation failed");
            return;
        };
        CGEvent::set_flags(Some(&event), self.flags());
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
    }

    fn mouse(&self, kind: CGEventType, button: MouseButton, click_state: Option<i64>) {
        let ev = CGEvent::new_mouse_event(
            self.source.as_deref(),
            kind,
            self.pos,
            button_info(button).cg,
        );
        if let Some(e) = ev.as_deref() {
            let number = button_info(button).number;
            CGEvent::set_integer_value_field(Some(e), CGEventField::MouseEventButtonNumber, number);
            if let Some(n) = click_state {
                CGEvent::set_integer_value_field(Some(e), CGEventField::MouseEventClickState, n);
            }
        }
        self.post(ev);
    }
}

impl Injector for CgInjector {
    fn pointer_move(&mut self, x: f32, y: f32) {
        self.pos = to_point(CGDisplayBounds(CGMainDisplayID()), x, y);
        match self.held.first() {
            Some(&b) => self.mouse(button_info(b).dragged, b, None),
            None => self.mouse(CGEventType::MouseMoved, MouseButton::Left, None),
        }
    }

    fn button(&mut self, button: MouseButton, pressed: bool) {
        let info = button_info(button);
        self.held.retain(|b| *b != button);
        let count = if pressed {
            self.held.push(button);
            self.clicks.press(button, self.pos, Instant::now())
        } else {
            self.clicks.count
        };
        let kind = if pressed { info.down } else { info.up };
        self.mouse(kind, button, Some(count));
    }

    fn wheel(&mut self, dx: f32, dy: f32) {
        let (lines_x, lines_y) = self.wheel.add(dx, dy);
        if lines_x == 0 && lines_y == 0 {
            return;
        }
        // Positive = up / left, the same convention as the viewer's (and Windows') wheel.
        let ev = CGEvent::new_scroll_wheel_event2(
            self.source.as_deref(),
            CGScrollEventUnit::Line,
            2,
            lines_y,
            lines_x,
            0,
        );
        self.post(ev);
    }

    fn key(&mut self, hid_usage: u16, pressed: bool) {
        let Some(code) = to_mac_keycode(hid_usage) else {
            tracing::debug!(hid_usage, "unmapped key");
            return;
        };
        if !modifier_flag(hid_usage).is_empty() {
            self.modifiers.retain(|k| *k != hid_usage);
            if pressed {
                self.modifiers.push(hid_usage);
            }
        }
        self.post(CGEvent::new_keyboard_event(
            self.source.as_deref(),
            code,
            pressed,
        ));
    }

    fn repeat(&mut self, hid_usage: u16) {
        let Some(code) = to_mac_keycode(hid_usage) else {
            return;
        };
        let ev = CGEvent::new_keyboard_event(self.source.as_deref(), code, true);
        if let Some(e) = ev.as_deref() {
            // Marked as a repeat, as the keyboard's own repeats are.
            CGEvent::set_integer_value_field(Some(e), CGEventField::KeyboardEventAutorepeat, 1);
        }
        self.post(ev);
    }
}

/// Normalized (0..1) position on `bounds` -> global point; 1.0 is the last point.
fn to_point(bounds: CGRect, x: f32, y: f32) -> CGPoint {
    let span = |len: f64| (len - 1.0).max(0.0);
    CGPoint {
        x: bounds.origin.x + f64::from(x.clamp(0.0, 1.0)) * span(bounds.size.width),
        y: bounds.origin.y + f64::from(y.clamp(0.0, 1.0)) * span(bounds.size.height),
    }
}

fn modifier_flag(hid: u16) -> CGEventFlags {
    use usage::*;
    match hid {
        CONTROL_LEFT | CONTROL_RIGHT => CGEventFlags::MaskControl,
        SHIFT_LEFT | SHIFT_RIGHT => CGEventFlags::MaskShift,
        ALT_LEFT | ALT_RIGHT => CGEventFlags::MaskAlternate,
        META_LEFT | META_RIGHT => CGEventFlags::MaskCommand,
        _ => CGEventFlags::empty(),
    }
}

struct ButtonInfo {
    down: CGEventType,
    up: CGEventType,
    dragged: CGEventType,
    cg: CGMouseButton,
    /// `kCGMouseEventButtonNumber`: 0 left, 1 right, 2 middle, 3 back, 4 forward.
    number: i64,
}

fn button_info(b: MouseButton) -> ButtonInfo {
    let other = |number| ButtonInfo {
        down: CGEventType::OtherMouseDown,
        up: CGEventType::OtherMouseUp,
        dragged: CGEventType::OtherMouseDragged,
        cg: CGMouseButton::Center,
        number,
    };
    match b {
        MouseButton::Left => ButtonInfo {
            down: CGEventType::LeftMouseDown,
            up: CGEventType::LeftMouseUp,
            dragged: CGEventType::LeftMouseDragged,
            cg: CGMouseButton::Left,
            number: 0,
        },
        MouseButton::Right => ButtonInfo {
            down: CGEventType::RightMouseDown,
            up: CGEventType::RightMouseUp,
            dragged: CGEventType::RightMouseDragged,
            cg: CGMouseButton::Right,
            number: 1,
        },
        MouseButton::Middle => other(2),
        MouseButton::Back => other(3),
        MouseButton::Forward => other(4),
    }
}

/// Click counting: macOS apps see double/triple clicks only via the click-state field.
#[derive(Default)]
struct Clicks {
    last: Option<(MouseButton, CGPoint, Instant)>,
    count: i64,
}

impl Clicks {
    fn press(&mut self, button: MouseButton, at: CGPoint, now: Instant) -> i64 {
        let repeat = self.last.is_some_and(|(b, p, t)| {
            b == button
                && now.duration_since(t) <= DOUBLE_CLICK
                && (p.x - at.x).abs() <= DOUBLE_CLICK_SLOP
                && (p.y - at.y).abs() <= DOUBLE_CLICK_SLOP
        });
        self.count = if repeat { self.count + 1 } else { 1 };
        self.last = Some((button, at, now));
        self.count
    }
}

/// Line-unit scrolling takes whole lines; carry fractions so slow wheels still scroll.
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
    use objc2_core_foundation::CGSize;

    #[test]
    fn normalized_points_cover_the_display() {
        let b = CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: CGSize {
                width: 1512.0,
                height: 982.0,
            },
        };
        let p = to_point(b, 0.0, 0.0);
        assert_eq!((p.x, p.y), (0.0, 0.0));
        let p = to_point(b, 1.0, 1.0);
        assert_eq!((p.x, p.y), (1511.0, 981.0));
        let p = to_point(b, 0.5, 2.0); // clamped
        assert_eq!((p.x, p.y), (755.5, 981.0));
    }

    #[test]
    fn repeated_clicks_count_up_and_reset() {
        let mut c = Clicks::default();
        let t = Instant::now();
        let at = CGPoint { x: 10.0, y: 10.0 };
        assert_eq!(c.press(MouseButton::Left, at, t), 1);
        assert_eq!(
            c.press(MouseButton::Left, at, t + Duration::from_millis(200)),
            2
        );
        assert_eq!(
            c.press(MouseButton::Left, at, t + Duration::from_millis(400)),
            3
        );
        // Too slow, other button, or moved away: back to a single click.
        assert_eq!(
            c.press(MouseButton::Left, at, t + Duration::from_secs(2)),
            1
        );
        let t2 = t + Duration::from_secs(2);
        assert_eq!(c.press(MouseButton::Right, at, t2), 1);
        let far = CGPoint { x: 50.0, y: 10.0 };
        assert_eq!(c.press(MouseButton::Right, far, t2), 1);
    }

    #[test]
    fn wheel_fractions_accumulate() {
        let mut w = WheelAccumulator::default();
        assert_eq!(w.add(0.0, 0.5), (0, 0));
        assert_eq!(w.add(0.0, 0.5), (0, 1));
        assert_eq!(w.add(-1.5, -3.0), (-1, -3));
        assert_eq!(w.add(-0.5, 0.0), (-1, 0));
    }

    #[test]
    fn modifiers_and_buttons_map() {
        assert_eq!(modifier_flag(usage::META_RIGHT), CGEventFlags::MaskCommand);
        assert!(modifier_flag(usage::A).is_empty());
        assert_eq!(button_info(MouseButton::Forward).number, 4);
        assert_eq!(
            button_info(MouseButton::Right).dragged,
            CGEventType::RightMouseDragged
        );
    }
}
