//! A whole mobile guest session in-process: signaling server, a host streaming a synthetic
//! test pattern (openh264) behind an access password, and `rconn_mobile::connect_guest`.
//! The AVCC samples the app would get are turned back into Annex-B and decoded, and the
//! input the app sends is checked on the host.

use bytes::BytesMut;
use rconn_core::fleet::{FleetCa, HostKey};
use rconn_core::guest::{HostCheck, make_verifier};
use rconn_core::ids::{HostId, OrgId};
use rconn_core::protocol::{Control, DataMessage, GuestMessage, GuestRefusal, InputEvent};
use rconn_media::capture::{Capturer, TestPattern};
use rconn_media::cisco::local_binary_path;
use rconn_media::video::{EncoderSettings, VideoDecoder, VideoEncoder};
use rconn_mobile::{PointerButton, SessionListener, VideoFrame, connect_guest};
use rconn_peer::rtc::PeerOptions;
use rconn_peer::session::{HostSession, SessionHandler, run_host};
use rconn_peer::signaling::{HostCredentials, SignalingClient};
use rconn_peer::trust::ServerTrust;
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::{AppState, router};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use webrtc::data_channel::DataChannelEvent;

const SERVER_ID: &str = "wss://mobile.test";
const PASSWORD: &str = "tulip-anchor-91";
const W: usize = 640;
const H: usize = 360;

/// What the host saw from the viewer.
#[derive(Default)]
struct HostLog {
    input: Mutex<Vec<InputEvent>>,
    resizes: Mutex<Vec<(u32, u32)>>,
    keyframe_requests: AtomicUsize,
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
    fn on_closed(&self, reason: String) {
        let mut c = self.closed.lock().unwrap();
        assert!(c.is_none(), "on_closed called twice");
        *c = Some(reason);
    }
}

async fn wait_for(what: &str, limit: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn openh264() -> PathBuf {
    let lib = local_binary_path().expect("no Cisco binary for this platform");
    assert!(
        lib.exists(),
        "missing {}; run scripts/fetch-openh264.sh",
        lib.display()
    );
    lib
}

/// Signaling server plus a host whose access password is [`PASSWORD`]. Returns the server
/// URL, the computer's ID as a user would type it, and the host's log.
async fn start() -> (String, String, Arc<HostLog>) {
    let ca = FleetCa::generate(OrgId::new()).unwrap();
    let auth = FleetAuthenticator::new(SERVER_ID).with_org_ca(ca.org_id(), ca.cert_pem());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/v1/ws", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, router(AppState::new(Arc::new(auth))))
            .await
            .unwrap()
    });

    let host_id = HostId::new();
    let key = HostKey::generate().unwrap();
    let cert = ca.issue_host_cert(host_id, &key).unwrap();
    let sig = SignalingClient::connect_host(
        &url,
        &ServerTrust::WebPki,
        HostCredentials {
            server_id: SERVER_ID,
            host_id,
            key: &key,
            cert_pem: &cert,
            label: None,
        },
    )
    .await
    .unwrap();
    let log = Arc::new(HostLog::default());
    let verifier = make_verifier(PASSWORD).unwrap();
    let lib = openh264();
    let host_log = log.clone();
    let handler: SessionHandler = Arc::new(move |s: HostSession| {
        tokio::spawn(host_session(
            s,
            verifier.clone(),
            lib.clone(),
            host_log.clone(),
        ));
    });
    let opts = PeerOptions {
        udp_addrs: vec!["127.0.0.1:0".into()],
        relay_only: false,
    };
    tokio::spawn(run_host(sig, opts, handler));
    (url, host_id.short_code(), log)
}

/// The host agent's guest flow in miniature: challenge first, nothing else until the
/// password checks out, then the test pattern at 30 fps.
async fn host_session(s: HostSession, verifier: String, lib: PathBuf, log: Arc<HostLog>) {
    let HostSession {
        session_id,
        policy,
        video,
        mut channels,
        ..
    } = s;
    assert!(policy.guest, "a guest session must be flagged to the host");
    let mut video = Some(video);
    let Some(dc) = channels.recv().await else {
        return;
    };
    let send = |m: DataMessage| {
        let dc = dc.clone();
        async move {
            let b = m.encode().unwrap();
            let _ = dc.send(BytesMut::from(&b[..])).await;
        }
    };
    let (check, challenge) = HostCheck::start(&verifier, session_id).unwrap();
    let mut check = Some(check);
    send(DataMessage::Guest(GuestMessage::Challenge {
        salt: challenge.salt,
        msg: challenge.msg,
    }))
    .await;
    let keyframe = Arc::new(AtomicBool::new(false));
    let mut streaming = None;
    while let Some(ev) = dc.poll().await {
        let DataChannelEvent::OnMessage(m) = ev else {
            continue;
        };
        match DataMessage::decode(&m.data).unwrap() {
            DataMessage::Guest(GuestMessage::Proof { msg, confirm }) => {
                let Some(c) = check.take() else { continue };
                match c.finish(&msg, &confirm) {
                    Ok(confirm) => {
                        send(DataMessage::Guest(GuestMessage::Accepted { confirm })).await;
                        if let Some(v) = video.take() {
                            streaming =
                                Some(tokio::spawn(stream(v, lib.clone(), keyframe.clone())));
                        }
                    }
                    Err(_) => {
                        send(DataMessage::Guest(GuestMessage::Refused(
                            GuestRefusal::WrongPassword,
                        )))
                        .await;
                    }
                }
            }
            _ if streaming.is_none() => {} // gate closed
            DataMessage::Control(Control::RequestKeyframe) => {
                log.keyframe_requests.fetch_add(1, Ordering::Relaxed);
                keyframe.store(true, Ordering::Relaxed);
            }
            DataMessage::Control(Control::Resize { width, height }) => {
                log.resizes.lock().unwrap().push((width, height));
            }
            DataMessage::Input(ev) => log.input.lock().unwrap().push(ev),
            _ => {}
        }
    }
    if let Some(t) = streaming {
        t.abort();
    }
}

async fn stream(video: rconn_peer::video::VideoSender, lib: PathBuf, keyframe: Arc<AtomicBool>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    std::thread::spawn(move || {
        let mut pattern = TestPattern::new(W, H, 30);
        let mut enc = VideoEncoder::new(&lib, EncoderSettings::default()).unwrap();
        loop {
            let Ok(Some(frame)) = pattern.next_frame(Duration::from_millis(100)) else {
                continue;
            };
            if keyframe.swap(false, Ordering::Relaxed) {
                enc.request_keyframe();
            }
            if let Some(au) = enc.encode(&frame).unwrap()
                && tx.blocking_send(au).is_err()
            {
                return;
            }
        }
    });
    while let Some(au) = rx.recv().await {
        if video.write(au, Duration::from_millis(33)).await.is_err() {
            break;
        }
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
        wait_for("input on the host", Duration::from_secs(5), || {
            host.input.lock().unwrap().len() >= 7
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
