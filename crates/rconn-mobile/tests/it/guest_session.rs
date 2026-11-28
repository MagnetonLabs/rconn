//! A whole mobile guest session in-process: signaling server, a host streaming a synthetic
//! test pattern (openh264) behind an access password, and `rconn_mobile::connect_guest`.
//! The AVCC samples the app would get are turned back into Annex-B and decoded, and the
//! input the app sends is checked on the host.

use crate::support;

use rconn_core::hid::usage as u;
use rconn_core::protocol::InputEvent;
use rconn_media::video::VideoDecoder;
use rconn_mobile::{PointerButton, SessionListener, VideoFrame, connect_guest};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::{HostSetup, openh264};

const PASSWORD: &str = "tulip-anchor-91";
const W: usize = 640;
const H: usize = 360;

async fn start() -> (String, String, Arc<support::HostLog>) {
    support::start(HostSetup {
        password: PASSWORD.into(),
        width: W,
        height: H,
        listen: "127.0.0.1:0".into(),
        files_root: None,
    })
    .await
}

/// What the app would see.
#[derive(Default)]
struct App {
    statuses: Mutex<Vec<String>>,
    frames: Mutex<Vec<VideoFrame>>,
    closed: Mutex<Option<String>>,
}

impl SessionListener for App {
    fn on_status(&self, text: String) {
        self.statuses.lock().unwrap().push(text);
    }
    fn on_video(&self, frame: VideoFrame) {
        self.frames.lock().unwrap().push(frame);
    }
    fn on_clipboard(&self, _text: String) {}
    fn on_host_name(&self, _name: String) {}
    fn on_closed(&self, reason: String) {
        let mut c = self.closed.lock().unwrap();
        assert!(c.is_none(), "on_closed called twice");
        *c = Some(reason);
    }
}

fn key(hid_usage: u16, pressed: bool) -> InputEvent {
    InputEvent::Key { hid_usage, pressed }
}

async fn wait_for(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Annex-B again, as the host sent it: what the app's sample buffers must decode to.
fn annex_b(f: &VideoFrame) -> Vec<u8> {
    let mut out = Vec::new();
    for ps in [&f.sps, &f.pps].into_iter().flatten() {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(ps);
    }
    let mut rest = &f.avcc[..];
    while rest.len() >= 4 {
        let n = u32::from_be_bytes(rest[..4].try_into().unwrap()) as usize;
        assert!(rest.len() >= 4 + n, "AVCC length runs past the sample");
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&rest[4..4 + n]);
        rest = &rest[4 + n..];
    }
    assert!(rest.is_empty(), "trailing bytes after the last NAL unit");
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_guest_sees_the_computer_and_controls_it() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let (url, id, host) = start().await;
        let app = Arc::new(App::default());
        let t0 = Instant::now();
        let session = connect_guest(url, None, id, PASSWORD.into(), app.clone());

        wait_for("60 frames", Duration::from_secs(20), || {
            app.frames.lock().unwrap().len() >= 60
        })
        .await;
        let first_60 = t0.elapsed();

        session.send_resize(1170, 2532);
        session.send_pointer(0.5, 0.25);
        session.send_button(PointerButton::Left, true);
        session.send_button(PointerButton::Left, false);
        session.send_key(rconn_core::hid::usage::A, true);
        session.send_key(rconn_core::hid::usage::A, false);
        session.send_text("hi".into());
        session.send_scroll(0.0, -3.0);
        session.send_combo(0); // Ctrl+Alt+Del from the Keys menu
        wait_for("input on the host", Duration::from_secs(5), || {
            host.input.lock().unwrap().len() >= 13
        })
        .await;
        session.disconnect();
        wait_for("on_closed", Duration::from_secs(5), || {
            app.closed.lock().unwrap().is_some()
        })
        .await;

        assert_eq!(app.closed.lock().unwrap().as_deref(), Some("disconnected"));
        assert_eq!(
            *host.input.lock().unwrap(),
            [
                InputEvent::PointerMove { x: 0.5, y: 0.25 },
                InputEvent::PointerButton {
                    button: rconn_core::protocol::MouseButton::Left,
                    pressed: true
                },
                InputEvent::PointerButton {
                    button: rconn_core::protocol::MouseButton::Left,
                    pressed: false
                },
                InputEvent::Key {
                    hid_usage: rconn_core::hid::usage::A,
                    pressed: true
                },
                InputEvent::Key {
                    hid_usage: rconn_core::hid::usage::A,
                    pressed: false
                },
                InputEvent::Text("hi".into()),
                InputEvent::Wheel { dx: 0.0, dy: -3.0 },
                key(u::CONTROL_LEFT, true),
                key(u::ALT_LEFT, true),
                key(u::DELETE, true),
                key(u::DELETE, false),
                key(u::ALT_LEFT, false),
                key(u::CONTROL_LEFT, false),
            ]
        );
        assert_eq!(*host.resizes.lock().unwrap(), [(1170, 2532)]);
        assert!(host.keyframe_requests.load(Ordering::Relaxed) >= 1);
        let statuses = app.statuses.lock().unwrap().clone();
        assert!(
            statuses.iter().any(|s| s.contains("access password")),
            "{statuses:?}"
        );
        assert_eq!(
            statuses.last().map(String::as_str),
            Some(""),
            "{statuses:?}"
        );

        let frames = std::mem::take(&mut *app.frames.lock().unwrap());
        let first = &frames[0];
        assert!(first.keyframe && first.sps.is_some() && first.pps.is_some());
        assert!(frames.windows(2).all(|w| w[0].pts_us <= w[1].pts_us));
        let mut dec = VideoDecoder::new(&openh264()).unwrap();
        let mut decoded = 0;
        for f in &frames {
            if let Some(pic) = dec.decode(&annex_b(f)).unwrap() {
                assert_eq!((pic.width, pic.height), (W, H));
                decoded += 1;
            }
        }
        eprintln!(
            "mobile: {} samples, {decoded} decoded ({}), 60 frames after {first_60:?}",
            frames.len(),
            dec.engine_name()
        );
        assert!(decoded + 2 >= frames.len(), "{decoded} of {}", frames.len());
    })
    .await
    .expect("test timed out");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_password_shows_nothing_and_says_why() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (url, id, host) = start().await;
        let app = Arc::new(App::default());
        let session = connect_guest(url, None, id, "not-the-password".into(), app.clone());
        wait_for("on_closed", Duration::from_secs(15), || {
            app.closed.lock().unwrap().is_some()
        })
        .await;
        session.send_key(rconn_core::hid::usage::A, true);
        assert_eq!(
            app.closed.lock().unwrap().as_deref(),
            Some("wrong access password")
        );
        assert!(app.frames.lock().unwrap().is_empty());
        assert!(host.input.lock().unwrap().is_empty());
    })
    .await
    .expect("test timed out");
}

/// The app asks from its own threads, outside any Tokio runtime (this used to panic).
#[test]
fn online_computers_answers_from_an_app_thread() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (url, id, _host) = rt.block_on(start());
    let asked = id.clone();
    let answer = std::thread::spawn(move || {
        rconn_mobile::online_computers(url, None, vec![asked, "999 999 999".into()])
    })
    .join()
    .expect("online_computers panicked");
    assert_eq!(answer, Some(vec![id]));
}
