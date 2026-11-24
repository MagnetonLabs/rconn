//! End-to-end broker tests over real WebSockets on localhost.

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use rconn_core::PROTOCOL_VERSION;
use rconn_core::ids::{HostId, OperatorId, OrgId, SessionId};
use rconn_core::protocol::{
    ClientMessage, EndReason, ErrorCode, IceServer, SdpType, ServerMessage, SessionPolicy,
    SignalPayload,
};
use rconn_signaling::auth::{
    AuthError, Authenticator, HostIdentity, HostProofInput, OperatorIdentity,
};
use rconn_signaling::{AppState, router};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

/// Test-only scheme: a host "signs" by flipping every nonce bit. Hosts are in org A;
/// token "op-a" is an operator in org A, "op-b" in org B.
struct TestAuth {
    org_a: OrgId,
    org_b: OrgId,
    op_a: OperatorId,
    op_b: OperatorId,
}

#[async_trait]
impl Authenticator for TestAuth {
    async fn verify_host(&self, p: HostProofInput<'_>) -> Result<HostIdentity, AuthError> {
        let expected: Vec<u8> = p.server_nonce.iter().map(|b| !b).collect();
        if p.signature == expected {
            Ok(HostIdentity {
                host_id: p.host_id,
                org_id: self.org_a,
            })
        } else {
            Err(AuthError::Invalid)
        }
    }
    async fn verify_operator(&self, token: &str) -> Result<OperatorIdentity, AuthError> {
        match token {
            "op-a" => Ok(OperatorIdentity {
                operator_id: self.op_a,
                org_id: self.org_a,
            }),
            "op-b" => Ok(OperatorIdentity {
                operator_id: self.op_b,
                org_id: self.org_b,
            }),
            _ => Err(AuthError::Invalid),
        }
    }
    async fn ice_servers(&self, _org: OrgId) -> Vec<IceServer> {
        vec![IceServer {
            urls: vec!["stun:stun.example:3478".into()],
            username: None,
            credential: None,
        }]
    }
    async fn session_policy(&self, _host: HostIdentity) -> SessionPolicy {
        SessionPolicy {
            show_indicator: false,
            clipboard: true,
        }
    }
}

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn start() -> (String, AppState) {
    let auth = TestAuth {
        org_a: OrgId::new(),
        org_b: OrgId::new(),
        op_a: OperatorId::new(),
        op_b: OperatorId::new(),
    };
    let state = AppState::new(Arc::new(auth));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(state.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("ws://{addr}/v1/ws"), state)
}

async fn send(ws: &mut Ws, m: &ClientMessage) {
    ws.send(Message::Text(m.to_json().unwrap().into()))
        .await
        .unwrap();
}

/// Next server message, or `None` if the socket closed.
async fn recv(ws: &mut Ws) -> Option<ServerMessage> {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for server");
        match frame {
            Some(Ok(Message::Text(t))) => return Some(ServerMessage::from_json(&t).unwrap()),
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
            Some(Ok(_)) => continue,
        }
    }
}

async fn connect_host(url: &str) -> (Ws, HostId) {
    let (mut ws, _) = connect_async(url).await.unwrap();
    let host_id = HostId::new();
    send(
        &mut ws,
        &ClientMessage::HostHello {
            protocol_version: PROTOCOL_VERSION,
            host_id,
            cert_pem: "test".into(),
            client_nonce: vec![0; 32],
        },
    )
    .await;
    let Some(ServerMessage::Challenge { nonce }) = recv(&mut ws).await else {
        panic!("expected challenge")
    };
    send(
        &mut ws,
        &ClientMessage::HostProof {
            signature: nonce.iter().map(|b| !b).collect(),
        },
    )
    .await;
    assert!(matches!(
        recv(&mut ws).await,
        Some(ServerMessage::Welcome { .. })
    ));
    (ws, host_id)
}

async fn connect_operator(url: &str, token: &str) -> Ws {
    let (mut ws, _) = connect_async(url).await.unwrap();
    send(
        &mut ws,
        &ClientMessage::OperatorHello {
            protocol_version: PROTOCOL_VERSION,
            token: token.into(),
        },
    )
    .await;
    assert!(matches!(
        recv(&mut ws).await,
        Some(ServerMessage::Welcome { .. })
    ));
    ws
}

async fn open_session(op: &mut Ws, host: &mut Ws, host_id: HostId) -> SessionId {
    send(op, &ClientMessage::RequestSession { host_id }).await;
    let Some(ServerMessage::IncomingSession {
        session_id, policy, ..
    }) = recv(host).await
    else {
        panic!("host not told")
    };
    assert!(policy.clipboard);
    assert_eq!(
        recv(op).await,
        Some(ServerMessage::SessionReady { session_id })
    );
    session_id
}

#[tokio::test]
async fn full_session_relays_sdp_both_ways_and_ends_cleanly() {
    let (url, state) = start().await;
    let (mut host, host_id) = connect_host(&url).await;
    assert_eq!(state.online_hosts().await, 1);
    let mut op = connect_operator(&url, "op-a").await;
    let sid = open_session(&mut op, &mut host, host_id).await;

    let offer = SignalPayload::Sdp {
        sdp_type: SdpType::Offer,
        sdp: "v=0 offer".into(),
    };
    send(
        &mut op,
        &ClientMessage::Signal {
            session_id: sid,
            payload: offer.clone(),
        },
    )
    .await;
    assert_eq!(
        recv(&mut host).await,
        Some(ServerMessage::Signal {
            session_id: sid,
            payload: offer
        })
    );

    let answer = SignalPayload::Sdp {
        sdp_type: SdpType::Answer,
        sdp: "v=0 answer".into(),
    };
    send(
        &mut host,
        &ClientMessage::Signal {
            session_id: sid,
            payload: answer.clone(),
        },
    )
    .await;
    assert_eq!(
        recv(&mut op).await,
        Some(ServerMessage::Signal {
            session_id: sid,
            payload: answer
        })
    );

    send(&mut op, &ClientMessage::EndSession { session_id: sid }).await;
    let ended = Some(ServerMessage::SessionEnded {
        session_id: sid,
        reason: EndReason::Normal,
    });
    assert_eq!(recv(&mut op).await, ended);
    assert_eq!(recv(&mut host).await, ended);
}

#[tokio::test]
async fn bad_host_proof_is_refused_and_closed() {
    let (url, state) = start().await;
    let (mut ws, _) = connect_async(&url).await.unwrap();
    send(
        &mut ws,
        &ClientMessage::HostHello {
            protocol_version: PROTOCOL_VERSION,
            host_id: HostId::new(),
            cert_pem: "x".into(),
            client_nonce: vec![0; 32],
        },
    )
    .await;
    assert!(matches!(
        recv(&mut ws).await,
        Some(ServerMessage::Challenge { .. })
    ));
    send(
        &mut ws,
        &ClientMessage::HostProof {
            signature: vec![0; 32],
        },
    )
    .await;
    assert!(matches!(
        recv(&mut ws).await,
        Some(ServerMessage::Error {
            code: ErrorCode::Unauthenticated,
            ..
        })
    ));
    assert_eq!(recv(&mut ws).await, None, "socket should close");
    assert_eq!(state.online_hosts().await, 0);
}

#[tokio::test]
async fn bad_operator_token_and_wrong_version_are_refused() {
    let (url, _) = start().await;
    let (mut ws, _) = connect_async(&url).await.unwrap();
    send(
        &mut ws,
        &ClientMessage::OperatorHello {
            protocol_version: PROTOCOL_VERSION,
            token: "nope".into(),
        },
    )
    .await;
    assert!(matches!(
        recv(&mut ws).await,
        Some(ServerMessage::Error {
            code: ErrorCode::Unauthenticated,
            ..
        })
    ));

    let (mut ws, _) = connect_async(&url).await.unwrap();
    send(
        &mut ws,
        &ClientMessage::OperatorHello {
            protocol_version: PROTOCOL_VERSION + 1,
            token: "op-a".into(),
        },
    )
    .await;
    assert!(matches!(
        recv(&mut ws).await,
        Some(ServerMessage::Error {
            code: ErrorCode::VersionMismatch,
            ..
        })
    ));
}

#[tokio::test]
async fn operators_cannot_reach_hosts_in_another_org() {
    let (url, _) = start().await;
    let (_host, host_id) = connect_host(&url).await;
    let mut op_b = connect_operator(&url, "op-b").await;
    send(&mut op_b, &ClientMessage::RequestSession { host_id }).await;
    assert!(matches!(
        recv(&mut op_b).await,
        Some(ServerMessage::Error {
            code: ErrorCode::NotFound,
            ..
        })
    ));
}

#[tokio::test]
async fn outsiders_cannot_inject_signals_into_a_session() {
    let (url, _) = start().await;
    let (mut host, host_id) = connect_host(&url).await;
    let mut op = connect_operator(&url, "op-a").await;
    let sid = open_session(&mut op, &mut host, host_id).await;

    let mut intruder = connect_operator(&url, "op-b").await;
    let payload = SignalPayload::Sdp {
        sdp_type: SdpType::Offer,
        sdp: "evil".into(),
    };
    send(
        &mut intruder,
        &ClientMessage::Signal {
            session_id: sid,
            payload,
        },
    )
    .await;
    assert!(matches!(
        recv(&mut intruder).await,
        Some(ServerMessage::Error {
            code: ErrorCode::NotFound,
            ..
        })
    ));
    send(
        &mut intruder,
        &ClientMessage::EndSession { session_id: sid },
    )
    .await;
    assert!(matches!(
        recv(&mut intruder).await,
        Some(ServerMessage::Error {
            code: ErrorCode::NotFound,
            ..
        })
    ));
}

#[tokio::test]
async fn host_disconnect_ends_its_sessions() {
    let (url, state) = start().await;
    let (mut host, host_id) = connect_host(&url).await;
    let mut op = connect_operator(&url, "op-a").await;
    let sid = open_session(&mut op, &mut host, host_id).await;

    host.close(None).await.unwrap();
    drop(host);
    assert_eq!(
        recv(&mut op).await,
        Some(ServerMessage::SessionEnded {
            session_id: sid,
            reason: EndReason::HostOffline
        })
    );
    assert_eq!(state.online_hosts().await, 0);
}

#[tokio::test]
async fn messages_out_of_order_close_the_socket() {
    let (url, _) = start().await;
    let (mut ws, _) = connect_async(&url).await.unwrap();
    send(
        &mut ws,
        &ClientMessage::RequestSession {
            host_id: HostId::new(),
        },
    )
    .await;
    assert!(matches!(
        recv(&mut ws).await,
        Some(ServerMessage::Error {
            code: ErrorCode::BadRequest,
            ..
        })
    ));
    assert_eq!(recv(&mut ws).await, None);
}
