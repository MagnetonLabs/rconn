//! Wayland input: the desktop portal's RemoteDesktop session, the same one the screen
//! capture uses ([`rconn_media::portal`]), so the person at the computer allows screen
//! and control in one dialog. Keys go as evdev key codes, buttons as evdev `BTN_*` codes,
//! the pointer as absolute positions on the shared screen stream, the wheel as discrete
//! clicks, and typed text as keysyms.

use super::Injector;
use rconn_core::hid::to_linux_evdev;
use rconn_core::protocol::MouseButton;
use rconn_media::portal::{self, InputCmd, Lease};
use std::time::{Duration, Instant};

/// The first lease may wait for the consent dialog (input queues meanwhile).
const FIRST_ACQUIRE: Duration = Duration::from_secs(30);
/// Later attempts (after the session closed) wait briefly and are rate-limited.
const RETRY_ACQUIRE: Duration = Duration::from_secs(2);
const RETRY_EVERY: Duration = Duration::from_secs(5);

pub struct PortalInjector {
    lease: Option<Lease>,
    retry_at: Instant,
    wheel: WheelAccumulator,
    warned_size: bool,
}

impl PortalInjector {
    pub fn new() -> Self {
        let mut inj = Self {
            lease: None,
            retry_at: Instant::now(),
            wheel: WheelAccumulator::default(),
            warned_size: false,
        };
        inj.connect(FIRST_ACQUIRE);
        inj
    }

    fn connect(&mut self, timeout: Duration) {
        match portal::acquire(timeout) {
            Ok(l) => {
                tracing::info!(stream = ?l.stream(), "Wayland input via the desktop portal");
                self.lease = Some(l);
            }
            Err(e) => {
                tracing::warn!(error = %e, "Wayland input injection not available yet");
                self.retry_at = Instant::now() + RETRY_EVERY;
            }
        }
    }

    /// The live session, re-joined if it closed (capture reopens it).
    fn lease(&mut self) -> Option<&Lease> {
        if self.lease.as_ref().is_some_and(Lease::is_closed) {
            self.lease = None;
        }
        if self.lease.is_none() && Instant::now() >= self.retry_at {
            self.connect(RETRY_ACQUIRE);
        }
        self.lease.as_ref()
    }

    fn send(&mut self, cmd: InputCmd) {
        if let Some(l) = self.lease() {
            l.input(cmd);
        }
    }
}

impl Injector for PortalInjector {
    fn pointer_move(&mut self, x: f32, y: f32) {
        let Some(size) = self.lease().map(|l| l.stream().size) else {
            return;
        };
        let Some(size) = size else {
            if !self.warned_size {
                self.warned_size = true;
                tracing::warn!("the portal did not report the screen size; pointer moves ignored");
            }
            return;
        };
        let (x, y) = to_stream(size, x, y);
        self.send(InputCmd::Motion { x, y });
    }

    fn button(&mut self, button: MouseButton, pressed: bool) {
        self.send(InputCmd::Button {
            code: evdev_button(button),
            pressed,
        });
    }

    fn wheel(&mut self, dx: f32, dy: f32) {
        let (lx, ly) = self.wheel.add(dx, dy);
        // Viewer: positive = up / left. Wayland: positive = down / right.
        if ly != 0 {
            self.send(InputCmd::Scroll {
                horizontal: false,
                steps: -ly,
            });
        }
        if lx != 0 {
            self.send(InputCmd::Scroll {
                horizontal: true,
                steps: -lx,
            });
        }
    }

    fn key(&mut self, hid_usage: u16, pressed: bool) {
        let Some(code) = to_linux_evdev(hid_usage) else {
            tracing::debug!(hid_usage, "unmapped key");
            return;
        };
        self.send(InputCmd::Key {
            code: code.into(),
            pressed,
        });
    }

    fn text(&mut self, text: &str) {
        for sym in text.chars().filter_map(keysym) {
            self.send(InputCmd::Keysym { sym, pressed: true });
            self.send(InputCmd::Keysym {
                sym,
                pressed: false,
            });
        }
    }
}

/// Capture one picture through the portal and move the pointer to the screen's middle.
pub fn selftest() -> anyhow::Result<String> {
    use rconn_media::capture::Capturer as _;
    // The dialog may sit unanswered for a while: keep waiting (it is asked only once).
    let asked = Instant::now();
    let mut cap = loop {
        match rconn_media::capture::portal::PortalCapturer::new() {
            Ok(c) => break c,
            Err(e)
                if asked.elapsed() < Duration::from_secs(120)
                    && e.to_string().contains("waiting for someone") =>
            {
                eprintln!(
                    "waiting for the dialog at this computer: allow remote interaction, then Share"
                );
            }
            Err(e) => return Err(e.into()),
        }
    };
    let mut picture = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    while picture.is_none() && Instant::now() < deadline {
        cap.next_into(Duration::from_millis(500), &mut picture, (16384, 16384))?;
    }
    let (w, h) = picture
        .as_ref()
        .map(rconn_media::yuv::I420::dimensions)
        .ok_or_else(|| anyhow::anyhow!("the portal stream sent no picture in 10 s"))?;
    let mut inj = PortalInjector::new();
    let stream = inj
        .lease()
        .map(Lease::stream)
        .ok_or_else(|| anyhow::anyhow!("no portal session for input"))?;
    anyhow::ensure!(
        stream.can_control,
        "captured {w}x{h}, but remote control was not allowed: turn on \"Allow Remote \
         Interaction\" in the dialog"
    );
    inj.pointer_move(0.5, 0.5);
    // Let the portal thread deliver the move before the session closes.
    std::thread::sleep(Duration::from_millis(500));
    Ok(format!(
        "Wayland portal ok: captured {w}x{h} from PipeWire node {}, logical size {:?}, remote \
         control allowed; the pointer was moved to the middle of the screen (check that it did)",
        stream.node_id, stream.size
    ))
}

/// Normalized (0..1) position -> stream coordinates; 1.0 is the last pixel.
fn to_stream((w, h): (i32, i32), x: f32, y: f32) -> (f64, f64) {
    let span = |len: i32| f64::from((len - 1).max(0));
    (
        f64::from(x.clamp(0.0, 1.0)) * span(w),
        f64::from(y.clamp(0.0, 1.0)) * span(h),
    )
}

/// evdev button codes (`linux/input-event-codes.h`).
fn evdev_button(b: MouseButton) -> i32 {
    match b {
        MouseButton::Left => 0x110,    // BTN_LEFT
        MouseButton::Right => 0x111,   // BTN_RIGHT
        MouseButton::Middle => 0x112,  // BTN_MIDDLE
        MouseButton::Back => 0x113,    // BTN_SIDE
        MouseButton::Forward => 0x114, // BTN_EXTRA
    }
}

/// The X keysym that types `c` (Latin-1 directly, the rest as Unicode keysyms).
fn keysym(c: char) -> Option<i32> {
    let cp = u32::from(c);
    let sym = match c {
        '\n' | '\r' => 0xff0d, // XK_Return
        '\t' => 0xff09,        // XK_Tab
        _ if c.is_control() => return None,
        _ if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) => cp,
        _ => 0x0100_0000 | cp,
    };
    i32::try_from(sym).ok()
}

/// Wheel clicks are whole; carry fractions so slow wheels still scroll.
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
    fn positions_cover_the_stream() {
        assert_eq!(to_stream((1920, 1080), 0.0, 0.0), (0.0, 0.0));
        assert_eq!(to_stream((1920, 1080), 1.0, 1.0), (1919.0, 1079.0));
        assert_eq!(to_stream((1920, 1080), 0.5, 2.0), (959.5, 1079.0));
        assert_eq!(to_stream((0, 0), 0.7, 0.7), (0.0, 0.0));
    }

    #[test]
    fn buttons_are_evdev_codes() {
        assert_eq!(evdev_button(MouseButton::Left), 272);
        assert_eq!(evdev_button(MouseButton::Right), 273);
        assert_eq!(evdev_button(MouseButton::Middle), 274);
        assert_eq!(evdev_button(MouseButton::Back), 275);
        assert_eq!(evdev_button(MouseButton::Forward), 276);
    }

    #[test]
    fn text_becomes_keysyms() {
        assert_eq!(keysym('a'), Some(0x61));
        assert_eq!(keysym('~'), Some(0x7e));
        assert_eq!(keysym('é'), Some(0xe9)); // XK_eacute
        assert_eq!(keysym('€'), Some(0x0100_20ac));
        assert_eq!(keysym('\n'), Some(0xff0d));
        assert_eq!(keysym('\t'), Some(0xff09));
        assert_eq!(keysym('\u{7}'), None);
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
    fn keys_use_the_shared_evdev_table() {
        use rconn_core::hid::usage;
        assert_eq!(to_linux_evdev(usage::A), Some(30)); // KEY_A
        assert_eq!(to_linux_evdev(usage::ENTER), Some(28)); // KEY_ENTER
        assert_eq!(to_linux_evdev(usage::META_LEFT), Some(125)); // KEY_LEFTMETA
    }
}
