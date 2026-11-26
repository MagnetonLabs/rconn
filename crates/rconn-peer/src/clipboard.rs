//! Two-way text clipboard sync over the data channel.
//!
//! The OS clipboard lives on one dedicated thread (arboard handles are not shareable).
//! Local changes are polled and sent; remote text is applied. Text we just applied is
//! remembered, so it isn't echoed straight back. Bounded by `MAX_CLIPBOARD_BYTES`.

use rconn_core::protocol::MAX_CLIPBOARD_BYTES;
use std::sync::mpsc;
use std::time::Duration;
use tokio::sync::mpsc as tmpsc;

/// How often the local clipboard is checked for changes.
pub const POLL: Duration = Duration::from_millis(300);

#[cfg(feature = "clipboard")]
pub struct ClipboardSync {
    to_os: mpsc::Sender<String>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(feature = "clipboard")]
impl ClipboardSync {
    /// Start syncing. Local changes arrive on the returned receiver, to send to the peer.
    pub fn start() -> (Self, tmpsc::UnboundedReceiver<String>) {
        let (to_os, from_peer) = mpsc::channel::<String>();
        let (to_peer, local_changes) = tmpsc::unbounded_channel::<String>();
        let thread = std::thread::Builder::new()
            .name("rconn-clipboard".into())
            .spawn(move || run(from_peer, to_peer))
            .expect("spawning clipboard thread");
        (
            Self {
                to_os,
                thread: Some(thread),
            },
            local_changes,
        )
    }

    /// Apply text received from the peer.
    pub fn apply_remote(&self, text: String) {
        let _ = self.to_os.send(text);
    }
}

#[cfg(feature = "clipboard")]
impl Drop for ClipboardSync {
    fn drop(&mut self) {
        let (dead, _) = mpsc::channel();
        drop(std::mem::replace(&mut self.to_os, dead)); // closes the channel; thread exits
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// What the sync loop needs from an OS clipboard. A seam for tests.
pub trait ClipBackend {
    fn get_text(&mut self) -> Option<String>;
    fn set_text(&mut self, text: String) -> bool;
}

#[cfg(feature = "clipboard")]
impl ClipBackend for arboard::Clipboard {
    fn get_text(&mut self) -> Option<String> {
        arboard::Clipboard::get_text(self).ok()
    }
    fn set_text(&mut self, text: String) -> bool {
        arboard::Clipboard::set_text(self, text).is_ok()
    }
}

#[cfg(feature = "clipboard")]
fn run(from_peer: mpsc::Receiver<String>, to_peer: tmpsc::UnboundedSender<String>) {
    match arboard::Clipboard::new() {
        Ok(mut c) => sync_loop(&mut c, &from_peer, &to_peer),
        Err(e) => tracing::warn!(error = %e, "clipboard unavailable; sync disabled"),
    }
}

/// Runs until either side hangs up.
pub fn sync_loop(
    clip: &mut dyn ClipBackend,
    from_peer: &mpsc::Receiver<String>,
    to_peer: &tmpsc::UnboundedSender<String>,
) {
    // Don't push whatever was on the clipboard before the session started.
    let mut last = clip.get_text();
    loop {
        match from_peer.recv_timeout(POLL) {
            Ok(text) => {
                if clip.set_text(text.clone()) {
                    last = Some(text);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(now) = clip.get_text()
                    && last.as_deref() != Some(now.as_str())
                    && now.len() <= MAX_CLIPBOARD_BYTES
                {
                    last = Some(now.clone());
                    if to_peer.send(now).is_err() {
                        return;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<Option<String>>>);
    impl ClipBackend for Fake {
        fn get_text(&mut self) -> Option<String> {
            self.0.lock().unwrap().clone()
        }
        fn set_text(&mut self, text: String) -> bool {
            *self.0.lock().unwrap() = Some(text);
            true
        }
    }

    #[test]
    fn sends_local_changes_applies_remote_and_never_echoes() {
        let fake = Fake(Arc::new(Mutex::new(Some("before session".into()))));
        let (to_os, from_peer) = mpsc::channel();
        let (to_peer, mut out) = tmpsc::unbounded_channel();
        let mut backend = fake.clone();
        let t = std::thread::spawn(move || sync_loop(&mut backend, &from_peer, &to_peer));

        std::thread::sleep(POLL * 2);
        assert!(
            out.try_recv().is_err(),
            "pre-existing clipboard must not be sent"
        );

        *fake.0.lock().unwrap() = Some("copied locally".into());
        std::thread::sleep(POLL * 2);
        assert_eq!(out.try_recv().unwrap(), "copied locally");

        to_os.send("from the other side".into()).unwrap();
        std::thread::sleep(POLL * 2);
        assert_eq!(
            fake.0.lock().unwrap().as_deref(),
            Some("from the other side")
        );
        assert!(
            out.try_recv().is_err(),
            "remote text must not be echoed back"
        );

        *fake.0.lock().unwrap() = Some("x".repeat(MAX_CLIPBOARD_BYTES + 1));
        std::thread::sleep(POLL * 2);
        assert!(
            out.try_recv().is_err(),
            "oversized clipboard must not be sent"
        );

        drop(to_os);
        t.join().unwrap();
    }
}
