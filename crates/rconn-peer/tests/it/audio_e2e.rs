//! Host sound path: Opus frames on the audio track arrive, decode, and keep their pitch.

use rconn_core::fleet::{FleetCa, HostKey};
use rconn_core::ids::{HostId, OperatorId, OrgId};
use rconn_media::codec::{opus_decoder, opus_encoder};
use rconn_peer::rtc::{PeerEvent, PeerOptions};
use rconn_peer::session::{HostSession, SessionHandler, open_viewer_session, run_host};
use rconn_peer::signaling::{HostCredentials, SignalingClient};
use rconn_peer::trust::ServerTrust;
use rconn_signaling::auth::OperatorIdentity;
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::{AppState, router};
use rtc::rtp_transceiver::rtp_sender::RtpCodecKind;
use std::sync::Arc;
use std::time::Duration;
use webrtc::media_stream::track_remote::TrackRemoteEvent;

const SID: &str = "wss://audio.test";
const RATE: usize = 48_000;
const FRAME: usize = 960; // 20 ms

#[tokio::test]
async fn tone_survives_the_audio_track() {
    tokio::time::timeout(Duration::from_secs(30), run())
        .await
        .expect("timed out");
}

async fn run() {
    let ca = FleetCa::generate(OrgId::new()).unwrap();
    let op = OperatorIdentity {
        operator_id: OperatorId::new(),
        org_id: ca.org_id(),
    };
    let auth = FleetAuthenticator::new(SID)
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
        ..Default::default()
    };
    let host_sig = SignalingClient::connect_host(
        &url,
        &ServerTrust::WebPki,
        HostCredentials {
            server_id: SID,
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

    // Host: 3 s of a 440 Hz stereo tone, 20 ms Opus frames, once connected.
    let tone: SessionHandler = Arc::new(|mut s: HostSession| {
        tokio::spawn(async move {
            while let Some(st) = s.states.recv().await {
                if st.to_string() == "connected" {
                    break;
                }
            }
            let audio = s.audio.expect("viewer offered audio");
            let mut enc = opus_encoder().unwrap();
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            let mut n = 0usize;
            for _ in 0..150 {
                tick.tick().await;
                let mut pcm = Vec::with_capacity(FRAME * 2);
                for _ in 0..FRAME {
                    let v =
                        (2.0 * std::f32::consts::PI * 440.0 * n as f32 / RATE as f32).sin() * 0.4;
                    pcm.extend([v, v]);
                    n += 1;
                }
                let mut out = vec![0u8; 4000];
                let len = enc.encode_float(&pcm, &mut out).unwrap();
                out.truncate(len);
                if audio.write(out, Duration::from_millis(20)).await.is_err() {
                    break;
                }
            }
        });
    });
    let host_task = tokio::spawn(run_host(host_sig, opts.clone(), tone));

    let viewer_sig = SignalingClient::connect_operator(&url, &ServerTrust::WebPki, "tok")
        .await
        .unwrap();
    let mut session = open_viewer_session(viewer_sig, host_id, opts)
        .await
        .unwrap();
    let track = loop {
        match session.peer.events.recv().await.expect("events ended") {
            PeerEvent::Track(t) if t.kind().await == RtpCodecKind::Audio => break t,
            _ => continue,
        }
    };
    let mut dec = opus_decoder().unwrap();
    let mut samples: Vec<f32> = Vec::new();
    let mut packets = 0;
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_secs(2), track.poll()).await {
        let TrackRemoteEvent::OnRtpPacket(pkt) = ev else {
            continue;
        };
        packets += 1;
        let mut pcm = vec![0f32; FRAME * 2];
        if let Ok(n) = dec.decode_float(&pkt.payload, &mut pcm, false) {
            samples.extend(pcm[..n * 2].iter().step_by(2)); // left channel
        }
    }
    assert!(packets >= 120, "expected ~150 packets, got {packets}");
    // Skip codec warm-up, then check level and pitch.
    let s = &samples[RATE / 2..];
    let rms = (s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32).sqrt();
    let crossings = s.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
    let hz = crossings as f32 / (s.len() as f32 / RATE as f32);
    eprintln!("audio: {packets} packets, rms {rms:.3}, ~{hz:.0} Hz");
    assert!(rms > 0.2, "tone too quiet: {rms}");
    assert!((hz - 440.0).abs() < 10.0, "pitch {hz} Hz, want 440");
    session.close().await;
    host_task.abort();
}
