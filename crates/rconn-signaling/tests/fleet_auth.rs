//! Host authentication with real fleet-CA certificates over real WebSockets.

use futures_util::{SinkExt, StreamExt};
use rconn_core::PROTOCOL_VERSION;
use rconn_core::fleet::{FleetCa, HostKey, host_proof_transcript};
use rconn_core::ids::{HostId, OperatorId, OrgId};
use rconn_core::protocol::{CHALLENGE_LEN, ClientMessage, ErrorCode, ServerMessage};
use rconn_signaling::auth::OperatorIdentity;
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::{AppState, router};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
const SERVER_ID: &str = "wss://fleet.test";

struct Host {
    id: HostId,
    key: HostKey,
    cert: String,
}

struct Fixture {
    url: String,
    ca: FleetCa,
    auth: Arc<FleetAuthenticator>,
}

async fn start() -> Fixture {
    let ca = FleetCa::generate(OrgId::new()).unwrap();
    let op = OperatorIdentity {
        operator_id: OperatorId::new(),
        org_id: ca.org_id(),
    };
    let auth = Arc::new(
        FleetAuthenticator::new(SERVER_ID)
            .with_org_ca(ca.org_id(), ca.cert_pem())
            .with_operator_token("secret-op", op),
    );
    let state = AppState::new(auth.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/v1/ws", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    Fixture { url, ca, auth }
}

fn enroll(ca: &FleetCa) -> Host {
    let id = HostId::new();
    let key = HostKey::generate().unwrap();
    let cert = ca.issue_host_cert(id, &key).unwrap();
    Host { id, key, cert }
}

async fn send(ws: &mut Ws, m: &ClientMessage) {
    ws.send(Message::Text(m.to_json().unwrap().into()))
        .await
        .unwrap();
}

async fn recv(ws: &mut Ws) -> Option<ServerMessage> {
    loop {
        match tokio::time::timeout(Duration::from_secs(15), ws.next())
            .await
            .expect("timeout")
        {
            Some(Ok(Message::Text(t))) => return Some(ServerMessage::from_json(&t).unwrap()),
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
            Some(Ok(_)) => {}
        }
    }
}

/// Run the handshake as `claimed_id`, signing for `server_id`. Returns the server's verdict.
async fn handshake(
    url: &str,
    host: &Host,
    claimed_id: HostId,
    server_id: &str,
) -> Option<ServerMessage> {
    let (mut ws, _) = connect_async(url).await.unwrap();
    let client_nonce = vec![0x5a; CHALLENGE_LEN];
    send(
        &mut ws,
        &ClientMessage::HostHello {
            protocol_version: PROTOCOL_VERSION,
            host_id: claimed_id,
            cert_pem: host.cert.clone(),
            client_nonce: client_nonce.clone(),
            label: None,
        },
    )
    .await;
    let Some(ServerMessage::Challenge { nonce }) = recv(&mut ws).await else {
        panic!("no challenge")
    };
    let t = host_proof_transcript(server_id, claimed_id, &host.cert, &client_nonce, &nonce);
    send(
        &mut ws,
        &ClientMessage::HostProof {
            signature: host.key.sign(&t).unwrap(),
        },
    )
    .await;
    recv(&mut ws).await
}

fn is_unauthenticated(m: &Option<ServerMessage>) -> bool {
    matches!(
        m,
        Some(ServerMessage::Error {
            code: ErrorCode::Unauthenticated,
            ..
        })
    )
}

#[tokio::test]
async fn enrolled_host_authenticates_and_gets_capabilities() {
    let f = start().await;
    let host = enroll(&f.ca);
    let Some(ServerMessage::Welcome {
        org_id,
        capabilities,
        ..
    }) = handshake(&f.url, &host, host.id, SERVER_ID).await
    else {
        panic!("expected welcome")
    };
    assert_eq!(org_id, f.ca.org_id());
    assert!(capabilities.iter().any(|c| c == "sessions"));
}

#[tokio::test]
async fn revoked_host_is_refused() {
    let f = start().await;
    let host = enroll(&f.ca);
    f.auth.revoke(host.id);
    let verdict = handshake(&f.url, &host, host.id, SERVER_ID).await;
    assert!(
        matches!(&verdict, Some(ServerMessage::Error { message, .. }) if message.contains("revoked")),
        "{verdict:?}"
    );
}

#[tokio::test]
async fn cert_from_unknown_org_is_refused() {
    let f = start().await;
    let stranger = enroll(&FleetCa::generate(OrgId::new()).unwrap());
    assert!(is_unauthenticated(
        &handshake(&f.url, &stranger, stranger.id, SERVER_ID).await
    ));
}

#[tokio::test]
async fn host_cannot_claim_another_hosts_id() {
    let f = start().await;
    let host = enroll(&f.ca);
    assert!(is_unauthenticated(
        &handshake(&f.url, &host, HostId::new(), SERVER_ID).await
    ));
}

#[tokio::test]
async fn proof_made_for_a_different_server_is_refused() {
    let f = start().await;
    let host = enroll(&f.ca);
    assert!(is_unauthenticated(
        &handshake(&f.url, &host, host.id, "wss://some-other-server").await
    ));
}

#[tokio::test]
async fn short_client_nonce_is_refused() {
    let f = start().await;
    let host = enroll(&f.ca);
    let (mut ws, _) = connect_async(&f.url).await.unwrap();
    send(
        &mut ws,
        &ClientMessage::HostHello {
            protocol_version: PROTOCOL_VERSION,
            host_id: host.id,
            cert_pem: host.cert,
            client_nonce: vec![1],
            label: None,
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
}

#[tokio::test]
async fn silent_unauthenticated_socket_is_dropped_after_timeout() {
    let f = start().await;
    let (mut ws, _) = connect_async(&f.url).await.unwrap();
    // Say nothing. The server should give up after AUTH_TIMEOUT (10s).
    assert!(is_unauthenticated(&recv(&mut ws).await));
    assert_eq!(recv(&mut ws).await, None);
}

#[tokio::test]
async fn operator_token_auth() {
    let f = start().await;
    for (token, ok) in [("secret-op", true), ("wrong", false)] {
        let (mut ws, _) = connect_async(&f.url).await.unwrap();
        send(
            &mut ws,
            &ClientMessage::OperatorHello {
                protocol_version: PROTOCOL_VERSION,
                token: token.into(),
            },
        )
        .await;
        let got = recv(&mut ws).await;
        assert_eq!(
            matches!(got, Some(ServerMessage::Welcome { .. })),
            ok,
            "{token}: {got:?}"
        );
    }
}
