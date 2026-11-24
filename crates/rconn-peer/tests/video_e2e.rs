//! Phase 3 pipeline without OS capture: synthetic frames -> openh264 -> WebRTC video track
//! (with GCC) -> RTP reassembly -> openh264 decode -> pixels checked on the viewer.

use rconn_core::fleet::{FleetCa, HostKey};
use rconn_core::ids::{HostId, OperatorId, OrgId};
use rconn_media::cisco::local_binary_path;
use rconn_media::video::{EncoderSettings, Frame, VideoDecoder, VideoEncoder};
use rconn_peer::rtc::{PeerEvent, PeerOptions};
use rconn_peer::session::{HostSession, SessionHandler, open_viewer_session, run_host};
use rconn_peer::signaling::{HostCredentials, SignalingClient};
use rconn_peer::trust::ServerTrust;
use rconn_peer::video::VideoReceiver;
use rconn_signaling::auth::OperatorIdentity;
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::{AppState, router};
use std::sync::Arc;
use std::time::{Duration, Instant};
use webrtc::media_stream::track_remote::TrackRemoteEvent;

const SERVER_ID: &str = "wss://video.test";
const W: usize = 640;
const H: usize = 360;
const FPS: u64 = 30;

/// Gradient with a white box that moves 8 px per frame.
fn frame(t: usize) -> Frame {
    let mut bgra = vec![0u8; W * H * 4];
    for y in 0..H {
        for x in 0..W {
            let i = (y * W + x) * 4;
            let boxed = (40 + t * 8..240 + t * 8).contains(&x) && (60..180).contains(&y);
            let px = if boxed {
                [255, 255, 255, 255]
            } else {
                [(x * 255 / W) as u8, (y * 255 / H) as u8, 80, 255]
            };
            bgra[i..i + 4].copy_from_slice(&px);
        }
    }
    Frame {
        width: W,
        height: H,
        bgra,
        captured_at: Instant::now(),
    }
}

#[tokio::test]
async fn screen_video_reaches_the_viewer_intact() {
    tokio::time::timeout(Duration::from_secs(40), run())
        .await
        .expect("test timed out");
}

async fn run() {
    let lib = local_binary_path().expect("no Cisco binary for this platform");
    assert!(
        lib.exists(),
        "missing {}; run scripts/fetch-openh264.sh",
        lib.display()
    );

    let ca = FleetCa::generate(OrgId::new()).unwrap();
    let op = OperatorIdentity {
        operator_id: OperatorId::new(),
        org_id: ca.org_id(),
    };
    let auth = FleetAuthenticator::new(SERVER_ID)
        .with_org_ca(ca.org_id(), ca.cert_pem())
        .with_operator_token("tok", op);
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
    let opts = PeerOptions {
        udp_addrs: vec!["127.0.0.1:0".into()],
        relay_only: false,
    };
    let host_sig = SignalingClient::connect_host(
        &url,
        &ServerTrust::WebPki,
        HostCredentials {
            server_id: SERVER_ID,
            host_id,
            key: &key,
            cert_pem: &cert,
        },
    )
    .await
    .unwrap();

    // Host: stream 4 s of synthetic screen at 30 fps once the peer connects.
    let enc_lib = lib.clone();
    let stream: SessionHandler = Arc::new(move |mut s: HostSession| {
        let lib = enc_lib.clone();
        tokio::spawn(async move {
            while let Some(st) = s.states.recv().await {
                if st.to_string() == "connected" {
                    break;
                }
            }
            let mut enc = VideoEncoder::new(&lib, EncoderSettings::default()).unwrap();
            let mut tick = tokio::time::interval(Duration::from_millis(1000 / FPS));
            for t in 0..(4 * FPS as usize) {
                tick.tick().await;
                if let Some(au) = enc.encode(&frame(t % 40)).unwrap() {
                    if s.video
                        .write(au, Duration::from_millis(1000 / FPS))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
            eprintln!(
                "host: GCC target at end {:.0} kbps",
                s.target_bitrate.get() / 1000.0
            );
        });
    });
    let host_task = tokio::spawn(run_host(host_sig, opts.clone(), stream));

    // Viewer: decode whatever arrives on the video track.
    let viewer_sig = SignalingClient::connect_operator(&url, &ServerTrust::WebPki, "tok")
        .await
        .unwrap();
    let started = Instant::now();
    let mut session = open_viewer_session(viewer_sig, host_id, opts)
        .await
        .unwrap();
    let track = loop {
        match session.peer.events.recv().await.expect("peer events ended") {
            PeerEvent::Track(t) => break t,
            _ => continue,
        }
    };
    let mut rx = VideoReceiver::default();
    let mut dec = VideoDecoder::new(&lib).unwrap();
    let (mut packets, mut decoded, mut bytes) = (0usize, 0usize, 0usize);
    let mut first_frame_at = None;
    let mut last = None;
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(3), track.poll()).await {
        let TrackRemoteEvent::OnRtpPacket(pkt) = ev else {
            continue;
        };
        packets += 1;
        for au in rx.push(pkt) {
            bytes += au.len();
            if let Ok(Some(f)) = dec.decode(&au) {
                first_frame_at.get_or_insert_with(|| started.elapsed());
                decoded += 1;
                last = Some(f);
            }
        }
    }
    let last = last.expect("no frame decoded");
    eprintln!(
        "viewer: {packets} RTP packets, {decoded} frames decoded, {:.0} kbps, first frame after {:?}",
        bytes as f64 * 8.0 / 4.0 / 1000.0,
        first_frame_at.unwrap()
    );
    assert_eq!((last.width, last.height), (W, H));
    assert!(decoded >= 100, "expected most of 120 frames, got {decoded}");
    // Bottom-right corner is static gradient: (255*639/640, 255*359/360, 80) in RGB order.
    let i = ((H - 1) * W + (W - 1)) * 4;
    let px = &last.rgba[i..i + 3];
    let want = [80u8, 254, 254]; // R=80, G=y-gradient, B=x-gradient (source was BGRA)
    for (got, want) in px.iter().zip(want) {
        assert!(
            (i32::from(*got) - i32::from(want)).abs() <= 12,
            "corner pixel {px:?}, want ~{want:?}"
        );
    }
    session.close().await;
    host_task.abort();
}
