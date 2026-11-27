//! A signaling server and a host streaming a synthetic test pattern behind an access
//! password, all in-process: shared by the session test and the `test_host` example (which
//! the iOS app connects to in the simulator). Once the password checks out it also sends a
//! test tone and, given a folder, shares it on the file channel as a real host does.

#![allow(dead_code)]

use bytes::BytesMut;
use rconn_core::files::FILES_CHANNEL_LABEL;
use rconn_core::fleet::{FleetCa, HostKey};
use rconn_core::guest::{HostCheck, make_verifier};
use rconn_core::ids::{HostId, OrgId};
use rconn_core::protocol::{Control, DataMessage, GuestMessage, GuestRefusal, InputEvent};
use rconn_media::capture::{Capturer, TestPattern};
use rconn_media::cisco::local_binary_path;
use rconn_media::video::{EncoderSettings, VideoEncoder};
use rconn_peer::files::ServeConfig;
use rconn_peer::rtc::PeerOptions;
use rconn_peer::session::{HostSession, SessionHandler, run_host};
use rconn_peer::signaling::{HostCredentials, SignalingClient};
use rconn_peer::trust::ServerTrust;
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::{AppState, router};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use webrtc::data_channel::DataChannelEvent;

const SERVER_ID: &str = "wss://mobile.test";

/// What the host streams and where it listens.
#[derive(Clone)]
pub struct HostSetup {
    pub password: String,
    pub width: usize,
    pub height: usize,
    /// Signaling listen address, e.g. `127.0.0.1:0`.
    pub listen: String,
    /// The folder the file channel shares (none: the channel is refused).
    pub files_root: Option<PathBuf>,
}

/// What the host saw from the viewer.
#[derive(Default)]
pub struct HostLog {
    pub input: Mutex<Vec<InputEvent>>,
    pub resizes: Mutex<Vec<(u32, u32)>>,
    pub keyframe_requests: AtomicUsize,
    pub frames_sent: AtomicUsize,
    pub sound_packets_sent: AtomicUsize,
}

pub fn openh264() -> PathBuf {
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
pub async fn start(setup: HostSetup) -> (String, String, Arc<HostLog>) {
    let ca = FleetCa::generate(OrgId::new()).unwrap();
    let auth = FleetAuthenticator::new(SERVER_ID).with_org_ca(ca.org_id(), ca.cert_pem());
    let listener = tokio::net::TcpListener::bind(&setup.listen).await.unwrap();
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
            os: None,
            version: None,
            capabilities: vec![],
        },
    )
    .await
    .unwrap();
    let log = Arc::new(HostLog::default());
    let verifier = make_verifier(&setup.password).unwrap();
    let lib = openh264();
    let host_log = log.clone();
    let handler: SessionHandler = Arc::new(move |s: HostSession| {
        tokio::spawn(host_session(
            s,
            verifier.clone(),
            lib.clone(),
            host_log.clone(),
            (setup.width, setup.height),
            setup.files_root.clone(),
        ));
    });
    let opts = PeerOptions {
        udp_addrs: vec!["127.0.0.1:0".into()],
        relay_only: false,
        ..Default::default()
    };
    tokio::spawn(run_host(sig, opts, handler));
    (url, host_id.short_code(), log)
}

/// The host agent's guest flow in miniature: challenge first, nothing else until the
/// password checks out, then the test pattern at 30 fps.
async fn host_session(
    s: HostSession,
    verifier: String,
    lib: PathBuf,
    log: Arc<HostLog>,
    size: (usize, usize),
    files_root: Option<PathBuf>,
) {
    let HostSession {
        binding,
        policy,
        video,
        audio,
        mut channels,
        ..
    } = s;
    assert!(policy.guest, "a guest session must be flagged to the host");
    let mut video = Some(video);
    let mut audio = audio;
    let Some(dc) = channels.recv().await else {
        return;
    };
    // Later channels: the file channel, served once the password checked out.
    let (allowed_tx, allowed) = watch::channel(false);
    let more = tokio::spawn(async move {
        while let Some(dc) = channels.recv().await {
            if dc.label().await.unwrap_or_default() != FILES_CHANNEL_LABEL {
                continue;
            }
            let (root, allowed) = (files_root.clone(), *allowed.borrow());
            tokio::spawn(async move {
                match root {
                    Some(root) if allowed => {
                        let cfg = ServeConfig {
                            inbox: root.join("Downloads").join("Rconn"),
                            root,
                            free_space: None,
                            session: "test".into(),
                        };
                        rconn_peer::files::serve(dc, cfg).await;
                    }
                    Some(_) => rconn_peer::files::refuse(dc, "not allowed in yet").await,
                    None => rconn_peer::files::refuse(dc, "file transfer is off").await,
                }
            });
        }
    });
    let send = |m: DataMessage| {
        let dc = dc.clone();
        async move {
            let b = m.encode().unwrap();
            let _ = dc.send(BytesMut::from(&b[..])).await;
        }
    };
    let (check, challenge) = HostCheck::start(&verifier, binding).unwrap();
    let mut check = Some(check);
    send(DataMessage::Guest(GuestMessage::Challenge {
        salt: challenge.salt,
        msg: challenge.msg,
    }))
    .await;
    let keyframe = Arc::new(AtomicBool::new(false));
    let mut streaming = None;
    let mut tones = Vec::new();
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
                        allowed_tx.send_replace(true);
                        if let Some(a) = audio.take() {
                            tones.push(tokio::spawn(tone(a, log.clone())));
                        }
                        if let Some(v) = video.take() {
                            streaming = Some(tokio::spawn(stream(
                                v,
                                lib.clone(),
                                keyframe.clone(),
                                size,
                                log.clone(),
                            )));
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
    for t in tones {
        t.abort();
    }
    more.abort();
}

/// A quiet 440 Hz beep, 150 ms every second, as Opus in 20 ms packets (silence between,
/// as a real host fills gaps).
async fn tone(audio: rconn_peer::video::AudioSender, log: Arc<HostLog>) {
    const FRAME: usize = 960;
    let mut enc = rconn_media::codec::opus_encoder().unwrap();
    let mut out = vec![0u8; 4000];
    let mut tick = tokio::time::interval(Duration::from_millis(20));
    let mut n = 0usize;
    loop {
        tick.tick().await;
        let mut pcm = Vec::with_capacity(FRAME * 2);
        for _ in 0..FRAME {
            let on = n % 48_000 < 7_200;
            let v = if on {
                (2.0 * std::f32::consts::PI * 440.0 * n as f32 / 48_000.0).sin() * 0.1
            } else {
                0.0
            };
            pcm.extend([v, v]);
            n += 1;
        }
        let len = enc.encode_float(&pcm, &mut out).unwrap();
        if audio
            .write(out[..len].to_vec(), Duration::from_millis(20))
            .await
            .is_err()
        {
            return;
        }
        log.sound_packets_sent.fetch_add(1, Ordering::Relaxed);
    }
}

async fn stream(
    video: rconn_peer::video::VideoSender,
    lib: PathBuf,
    keyframe: Arc<AtomicBool>,
    (w, h): (usize, usize),
    log: Arc<HostLog>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    std::thread::spawn(move || {
        let mut pattern = TestPattern::new(w, h, 30);
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
        log.frames_sent.fetch_add(1, Ordering::Relaxed);
    }
}
