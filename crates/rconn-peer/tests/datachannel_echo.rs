//! Phase 2 acceptance: a viewer and a host, both authenticated against a live signaling
//! server, negotiate WebRTC through it and exchange `DataMessage`s over a real data
//! channel: once with any path allowed, once forced through the built-in TURN relay.

use bytes::BytesMut;
use rconn_core::fleet::{FleetCa, HostKey};
use rconn_core::ids::{HostId, OperatorId, OrgId};
use rconn_core::protocol::{Control, DATA_CHANNEL_PROTOCOL, DataMessage};
use rconn_peer::rtc::PeerOptions;
use rconn_peer::session::{HostSession, SessionHandler, open_viewer_session, run_host};
use rconn_peer::signaling::{HostCredentials, SignalingClient};
use rconn_peer::trust::ServerTrust;
use rconn_signaling::auth::OperatorIdentity;
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::turn_relay::{TurnConfig, TurnMinter, TurnRelay};
use rconn_signaling::{AppState, router};
use std::sync::Arc;
use std::time::{Duration, Instant};
use webrtc::data_channel::DataChannelEvent;

const SERVER_ID: &str = "wss://fleet.test";
const LOOPBACK: &str = "127.0.0.1:0";

#[tokio::test]
async fn viewer_and_host_echo_over_a_real_data_channel() {
    tokio::time::timeout(Duration::from_secs(30), run(false))
        .await
        .expect("test timed out");
}

/// Relay-only ICE: the only candidates are allocations on our built-in TURN relay, so
/// the channel can only open if the relay and the minted credentials work.
#[tokio::test]
async fn echo_works_relay_only_through_builtin_turn() {
    tokio::time::timeout(Duration::from_secs(30), run(true))
        .await
        .expect("test timed out");
}

async fn run(relay_only: bool) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let ca = FleetCa::generate(OrgId::new()).unwrap();
    let op = OperatorIdentity {
        operator_id: OperatorId::new(),
        org_id: ca.org_id(),
    };
    let mut auth = FleetAuthenticator::new(SERVER_ID)
        .with_org_ca(ca.org_id(), ca.cert_pem())
        .with_operator_token("tok", op);

    let secret = "turn-test-secret".to_string();
    let relay = TurnRelay::start(&TurnConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        public_ip: "127.0.0.1".parse().unwrap(),
        relay_ports: (50000, 50100),
        shared_secret: secret.clone(),
    })
    .await
    .unwrap();
    if relay_only {
        auth = auth.with_turn(TurnMinter::for_relay(relay.local_addr, secret));
    }

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
        udp_addrs: vec![LOOPBACK.into()],
        relay_only,
    };
    let host_sig = SignalingClient::connect_host(
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
    let viewer_sig = SignalingClient::connect_operator(&url, &ServerTrust::WebPki, "tok")
        .await
        .unwrap();
    if relay_only {
        assert!(
            viewer_sig
                .ice_servers
                .iter()
                .any(|s| s.credential.is_some()),
            "Welcome must carry TURN credentials"
        );
    }

    // Host: echo Ping -> Pong on every channel, after checking its protocol.
    let echo: SessionHandler = Arc::new(|mut session: HostSession| {
        assert!(session.policy.clipboard);
        tokio::spawn(async move {
            let Some(dc) = session.channels.recv().await else {
                return;
            };
            assert_eq!(
                dc.protocol().await.unwrap(),
                DATA_CHANNEL_PROTOCOL,
                "host must check the data protocol"
            );
            while let Some(ev) = dc.poll().await {
                if let DataChannelEvent::OnMessage(msg) = ev
                    && let Ok(DataMessage::Control(Control::Ping(n))) =
                        DataMessage::decode(&msg.data)
                {
                    let pong = DataMessage::Control(Control::Pong(n)).encode().unwrap();
                    dc.send(BytesMut::from(&pong[..])).await.unwrap();
                }
            }
        });
    });
    let host_task = tokio::spawn(run_host(host_sig, opts.clone(), echo));

    let started = Instant::now();
    let session = open_viewer_session(viewer_sig, host_id, opts)
        .await
        .unwrap();
    let dc = session.data.clone();
    let mut got_pong = false;
    while let Some(ev) = dc.poll().await {
        match ev {
            DataChannelEvent::OnOpen => {
                eprintln!(
                    "relay_only={relay_only}: channel open after {:?}",
                    started.elapsed()
                );
                let ping = DataMessage::Control(Control::Ping(42)).encode().unwrap();
                dc.send(BytesMut::from(&ping[..])).await.unwrap();
            }
            DataChannelEvent::OnMessage(msg) => {
                assert_eq!(
                    DataMessage::decode(&msg.data).unwrap(),
                    DataMessage::Control(Control::Pong(42))
                );
                got_pong = true;
                break;
            }
            _ => {}
        }
    }
    assert!(got_pong, "viewer never received the pong");
    // Budgets guard the vendored webrtc relay-driver fix (vendor/README.md): relayed
    // setup took ~6.7 s here before it, ~2.9 s after; direct ~0.2 s.
    let budget = if relay_only {
        Duration::from_secs(6)
    } else {
        Duration::from_secs(2)
    };
    assert!(
        started.elapsed() < budget,
        "slow connect: {:?} (budget {budget:?})",
        started.elapsed()
    );
    session.close().await;
    host_task.abort();
    let _ = relay.close().await;
}
