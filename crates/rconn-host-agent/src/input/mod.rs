//! Input injection: viewer events in, native OS input out.
//!
//! Injection runs on one dedicated OS thread per session. On Windows that thread attaches
//! to the input desktop, so input reaches the lock screen and UAC prompts too (the agent
//! runs as SYSTEM in the console session). When the session ends, every key and button
//! still held is released, so a dropped connection can't leave Ctrl stuck down.

#[cfg(windows)]
mod windows;

/// Inject a pointer move and check the OS cursor followed. Windows only.
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
    #[cfg(not(windows))]
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
    let Some(mut inj) = platform_injector() else {
        tracing::warn!("no input injection on this OS yet; input events are ignored");
        while rx.recv().is_ok() {}
        return;
    };
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
                held.key(hid_usage, pressed);
                inj.key(hid_usage, pressed);
            }
        }
    }
    held.release_all(inj.as_mut());
}

fn platform_injector() -> Option<Box<dyn Injector>> {
    #[cfg(windows)]
    {
        Some(Box::new(windows::SendInputInjector::default()))
    }
    #[cfg(not(windows))]
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
