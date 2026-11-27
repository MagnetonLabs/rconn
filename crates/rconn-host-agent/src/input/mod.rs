//! Input injection: viewer events in, native OS input out.
//!
//! Injection runs on one dedicated OS thread per session. On Windows that thread attaches
//! to the input desktop, so input reaches the lock screen and UAC prompts too (the agent
//! runs as SYSTEM in the console session). On macOS it posts CGEvents at the HID tap,
//! which needs the Accessibility permission. On Linux it sends XTest events on X11, and
//! on Wayland goes through the desktop portal's remote-desktop session (shared with screen
//! capture, one consent dialog). When the session ends, every key and button still held
//! is released, so a dropped connection can't leave Ctrl stuck down.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod portal;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "linux")]
mod x11;

/// Inject a pointer move and check the OS cursor followed (Windows). On a Wayland
/// desktop: open the portal session (consent dialog), capture one picture and move the
/// pointer to the middle of the screen.
pub fn selftest() -> anyhow::Result<String> {
    #[cfg(windows)]
    {
        let (want, got) = windows::selftest_pointer()?;
        let ok = (want.0 - got.0).abs() <= 2 && (want.1 - got.1).abs() <= 2;
        anyhow::ensure!(ok, "cursor at {got:?}, expected {want:?}");
        Ok(format!(
            "pointer injection ok: expected {want:?}, cursor at {got:?}"
        ))
    }
    #[cfg(target_os = "linux")]
    {
        anyhow::ensure!(
            rconn_media::capture::uses_portal(),
            "input self-test needs Windows or a Wayland desktop"
        );
        portal::selftest()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        anyhow::bail!("input self-test is Windows-only")
    }
}

use rconn_core::protocol::{InputEvent, MouseButton};
use std::collections::HashSet;
use std::sync::mpsc;

pub trait Injector {
    fn pointer_move(&mut self, x: f32, y: f32);
    fn button(&mut self, button: MouseButton, pressed: bool);
    fn wheel(&mut self, dx: f32, dy: f32);
    fn key(&mut self, hid_usage: u16, pressed: bool);
    /// The viewer's keyboard repeated a key that is held down here. Windows and macOS don't
    /// repeat keys pressed by software, so by default this presses it again; injectors on
    /// systems that repeat held keys themselves (X11, Wayland) ignore it.
    fn repeat(&mut self, hid_usage: u16) {
        self.key(hid_usage, true);
    }
    /// Type text as keystrokes (default: unsupported, ignored).
    fn text(&mut self, _text: &str) {
        tracing::info!("typing text isn't supported on this OS yet");
    }
    /// Is Caps Lock on right now? (Default: unknown, treated as off.)
    fn caps_lock_on(&mut self) -> bool {
        false
    }
}

/// Handle to a running injection thread. Dropping it releases held input and stops it.
pub struct InputSink {
    tx: Option<mpsc::Sender<InputEvent>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InputSink {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel::<InputEvent>();
        let thread = std::thread::Builder::new()
            .name("rconn-input".into())
            .spawn(move || run(rx))
            .expect("spawning input thread");
        Self {
            tx: Some(tx),
            thread: Some(thread),
        }
    }

    pub fn send(&self, ev: InputEvent) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(ev);
        }
    }
}

impl Drop for InputSink {
    fn drop(&mut self) {
        self.tx.take(); // closes the channel; the thread releases held input and exits
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(rx: mpsc::Receiver<InputEvent>) {
    let started = std::time::Instant::now();
    let Some(mut inj) = platform_injector() else {
        tracing::warn!("no input injection on this OS yet; input events are ignored");
        while rx.recv().is_ok() {}
        return;
    };
    // Waiting for a consent dialog (Wayland): what the viewer did meanwhile is stale.
    if started.elapsed() > std::time::Duration::from_secs(1) {
        let stale = rx.try_iter().count();
        tracing::info!(stale, "input ready; dropped input sent while waiting");
    }
    // Start every session with Caps Lock off: the viewer can't know the host's lock state,
    // and a leftover Caps Lock makes everything type in capitals.
    if inj.caps_lock_on() {
        let caps = rconn_core::hid::usage::CAPS_LOCK;
        inj.key(caps, true);
        inj.key(caps, false);
        tracing::info!("turned Caps Lock off for the session");
    }
    let mut held = Held::default();
    while let Ok(ev) = rx.recv() {
        match ev {
            InputEvent::PointerMove { x, y } => {
                inj.pointer_move(x.clamp(0.0, 1.0), y.clamp(0.0, 1.0))
            }
            InputEvent::PointerButton { button, pressed } => {
                held.button(button, pressed);
                inj.button(button, pressed);
            }
            InputEvent::Wheel { dx, dy } => inj.wheel(dx.clamp(-50.0, 50.0), dy.clamp(-50.0, 50.0)),
            InputEvent::Key { hid_usage, pressed } => {
                key(&mut held, inj.as_mut(), hid_usage, pressed)
            }
            InputEvent::Text(text) => {
                let text: String = text.chars().take(4096).collect();
                inj.text(&text);
            }
        }
    }
    held.release_all(inj.as_mut());
}

/// A press of a key already held is the viewer's keyboard repeating it.
fn key(held: &mut Held, inj: &mut dyn Injector, hid_usage: u16, pressed: bool) {
    if pressed && held.keys.contains(&hid_usage) {
        inj.repeat(hid_usage);
    } else {
        held.key(hid_usage, pressed);
        inj.key(hid_usage, pressed);
    }
}

fn platform_injector() -> Option<Box<dyn Injector>> {
    #[cfg(windows)]
    {
        Some(Box::new(windows::SendInputInjector::default()))
    }
    #[cfg(target_os = "macos")]
    {
        Some(Box::new(macos::CgInjector::new()))
    }
    #[cfg(target_os = "linux")]
    {
        if rconn_media::capture::uses_portal() {
            return Some(Box::new(portal::PortalInjector::new()));
        }
        match x11::XTestInjector::new() {
            Ok(inj) => Some(Box::new(inj)),
            Err(e) => {
                tracing::warn!(error = %e, "X11 input injection unavailable");
                None
            }
        }
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Tracks what the viewer is holding down, to release it when the session ends.
#[derive(Default)]
struct Held {
    keys: HashSet<u16>,
    buttons: Vec<MouseButton>,
}

impl Held {
    fn key(&mut self, k: u16, pressed: bool) {
        if pressed {
            self.keys.insert(k);
        } else {
            self.keys.remove(&k);
        }
    }
    fn button(&mut self, b: MouseButton, pressed: bool) {
        self.buttons.retain(|x| *x != b);
        if pressed {
            self.buttons.push(b);
        }
    }
    fn release_all(&mut self, inj: &mut dyn Injector) {
        for k in self.keys.drain() {
            inj.key(k, false);
        }
        for b in self.buttons.drain(..) {
            inj.button(b, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder(Vec<String>);
    impl Injector for Recorder {
        fn pointer_move(&mut self, x: f32, y: f32) {
            self.0.push(format!("move {x} {y}"));
        }
        fn button(&mut self, b: MouseButton, p: bool) {
            self.0.push(format!("button {b:?} {p}"));
        }
        fn wheel(&mut self, dx: f32, dy: f32) {
            self.0.push(format!("wheel {dx} {dy}"));
        }
        fn key(&mut self, k: u16, p: bool) {
            self.0.push(format!("key {k:#x} {p}"));
        }
    }

    #[test]
    fn a_second_press_of_a_held_key_is_a_repeat() {
        struct Repeats(Recorder);
        impl Injector for Repeats {
            fn pointer_move(&mut self, x: f32, y: f32) {
                self.0.pointer_move(x, y)
            }
            fn button(&mut self, b: MouseButton, p: bool) {
                self.0.button(b, p)
            }
            fn wheel(&mut self, dx: f32, dy: f32) {
                self.0.wheel(dx, dy)
            }
            fn key(&mut self, k: u16, p: bool) {
                self.0.key(k, p)
            }
            fn repeat(&mut self, k: u16) {
                self.0.0.push(format!("repeat {k:#x}"));
            }
        }
        let mut held = Held::default();
        let mut rec = Repeats(Recorder::default());
        for pressed in [true, true, true, false] {
            key(&mut held, &mut rec, 0x2A, pressed); // Backspace held, repeated twice
        }
        key(&mut held, &mut rec, 0x2A, true);
        assert_eq!(
            rec.0.0,
            [
                "key 0x2a true",
                "repeat 0x2a",
                "repeat 0x2a",
                "key 0x2a false",
                "key 0x2a true"
            ]
        );
        // Without its own repeat, an injector presses the key again (Windows).
        let mut held = Held::default();
        let mut plain = Recorder::default();
        for pressed in [true, true, false] {
            key(&mut held, &mut plain, 0x2A, pressed);
        }
        assert_eq!(
            plain.0,
            ["key 0x2a true", "key 0x2a true", "key 0x2a false"]
        );
    }

    #[test]
    fn held_input_is_released_at_session_end() {
        let mut held = Held::default();
        let mut rec = Recorder::default();
        held.key(0xE0, true); // Ctrl down
        held.key(0x04, true);
        held.key(0x04, false); // A tapped
        held.button(MouseButton::Left, true);
        held.release_all(&mut rec);
        let mut out = rec.0.clone();
        out.sort();
        assert_eq!(out, vec!["button Left false", "key 0xe0 false"]);
    }
}
