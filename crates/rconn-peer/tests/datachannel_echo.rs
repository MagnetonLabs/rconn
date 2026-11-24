//! Phase 2 acceptance: a viewer and a host, both authenticated against a live signaling
//! server, negotiate WebRTC through it and exchange `DataMessage`s over a real data channel.

use bytes::BytesMut;
use rconn_core::fleet::{FleetCa, HostKey};
use rconn_core::ids::{HostId, OperatorId, OrgId};
use rconn_core::protocol::{
    ClientMessage, Control, DATA_CHANNEL_PROTOCOL, DataMessage, SdpType, ServerMessage,
    SignalPayload,
};
use rconn_peer::rtc::{Peer, PeerEvent};
use rconn_peer::signaling::{HostCredentials, SignalingClient};
use rconn_signaling::auth::OperatorIdentity;
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::{AppState, router};
use std::sync::Arc;
use std::time::Duration;
use webrtc::data_channel::{DataChannelEvent, RTCDataChannelInit};

const SERVER_ID: &str = "wss://fleet.test";
const LOOPBACK: &str = "127.0.0.1:0";

#[tokio::test]
async fn viewer_and_host_echo_over_a_real_data_channel() {
    tokio::time::timeout(Duration::from_secs(30), run())
        .await
        .expect("test timed out");
}

async fn run() {
    // Signaling server with one org, one host, one operator.
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

    let mut host_sig = SignalingClient::connect_host(
        &url,
        HostCredentials {
            server_id: SERVER_ID,
            host_id,
            key: &key,
            cert_pem: &cert,
        },
    )
    .await
    .unwrap();
    let mut viewer_sig = SignalingClient::connect_operator(&url, "tok")
        .await
        .unwrap();

    // Host side: answer an offer, then echo Ping -> Pong on whatever channel opens.
    let host_task = tokio::spawn(async move {
        let ServerMessage::IncomingSession { session_id, .. } = host_sig.recv().await.unwrap()
        else {
            panic!("host: expected incoming session")
        };
        let ServerMessage::Signal {
            payload:
                SignalPayload::Sdp {
                    sdp_type: SdpType::Offer,
                    sdp,
                },
            ..
        } = host_sig.recv().await.unwrap()
        else {
            panic!("host: expected offer")
        };
        let mut peer = Peer::new(&host_sig.ice_servers, vec![LOOPBACK.into()])
            .await
            .unwrap();
        let answer = peer.answer(sdp).await.unwrap();
        host_sig
            .send(&ClientMessage::Signal {
                session_id,
                payload: answer,
            })
            .await
            .unwrap();

        let dc = loop {
            if let Some(PeerEvent::DataChannel(dc)) = peer.events.recv().await {
                break dc;
            }
        };
        assert_eq!(
            dc.protocol().await.unwrap(),
            DATA_CHANNEL_PROTOCOL,
            "host must check the data protocol"
        );
        while let Some(ev) = dc.poll().await {
            if let DataChannelEvent::OnMessage(msg) = ev {
                let DataMessage::Control(Control::Ping(n)) =
                    DataMessage::decode(&msg.data).unwrap()
                else {
                    panic!("host: expected ping")
                };
                let pong = DataMessage::Control(Control::Pong(n)).encode().unwrap();
                dc.send(BytesMut::from(&pong[..])).await.unwrap();
                break;
            }
        }
        peer // keep the connection alive until the viewer has its reply
    });

    // Viewer side: request a session, offer with a data channel, ping, expect pong.
    viewer_sig
        .send(&ClientMessage::RequestSession { host_id })
        .await
        .unwrap();
    let ServerMessage::SessionReady { session_id } = viewer_sig.recv().await.unwrap() else {
        panic!("viewer: expected session ready")
    };
    let mut peer = Peer::new(&viewer_sig.ice_servers, vec![LOOPBACK.into()])
        .await
        .unwrap();
    let init = RTCDataChannelInit {
        ordered: true,
        protocol: DATA_CHANNEL_PROTOCOL.into(),
        ..Default::default()
    };
    let dc = peer
        .pc
        .create_data_channel("rconn", Some(init))
        .await
        .unwrap();
    let offer = peer.offer().await.unwrap();
    viewer_sig
        .send(&ClientMessage::Signal {
            session_id,
            payload: offer,
        })
        .await
        .unwrap();
    let ServerMessage::Signal {
        payload:
            SignalPayload::Sdp {
                sdp_type: SdpType::Answer,
                sdp,
            },
        ..
    } = viewer_sig.recv().await.unwrap()
    else {
        panic!("viewer: expected answer")
    };
    peer.accept_answer(sdp).await.unwrap();

    let mut got_pong = false;
    while let Some(ev) = dc.poll().await {
        match ev {
            DataChannelEvent::OnOpen => {
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
    let host_peer = host_task.await.unwrap();
    let _ = peer.pc.close().await;
    let _ = host_peer.pc.close().await;
}
