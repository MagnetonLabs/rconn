//! Live reload from the database: revocations evict connected hosts; new accounts and
//! operator tokens work without a restart.

use futures_util::{SinkExt, StreamExt};
use rconn_core::PROTOCOL_VERSION;
use rconn_core::fleet::{HostKey, host_proof_transcript};
use rconn_core::ids::HostId;
use rconn_core::protocol::{CHALLENGE_LEN, ClientMessage, ErrorCode, ServerMessage};
use rconn_signaling::accounts::SessionTokens;
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::import;
use rconn_signaling::reload::{Seen, reload_once};
use rconn_signaling::store::DataDir;
use rconn_signaling::{AppState, router};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
const SID: &str = "wss://reload.test";

async fn send(ws: &mut Ws, m: &ClientMessage) {
    ws.send(Message::Text(m.to_json().unwrap().into()))
        .await
        .unwrap();
}
async fn recv(ws: &mut Ws) -> Option<ServerMessage> {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timeout")
        {
            Some(Ok(Message::Text(t))) => return Some(ServerMessage::from_json(&t).unwrap()),
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
            Some(Ok(_)) => {}
        }
    }
}

async fn host_hello(
    url: &str,
    id: HostId,
    key: &HostKey,
    cert: &str,
) -> (Ws, Option<ServerMessage>) {
    let (mut ws, _) = connect_async(url).await.unwrap();
    let cn = vec![1u8; CHALLENGE_LEN];
    send(
        &mut ws,
        &ClientMessage::HostHello {
            protocol_version: PROTOCOL_VERSION,
            host_id: id,
            cert_pem: cert.into(),
            client_nonce: cn.clone(),
            label: None,
            os: None,
            version: None,
            capabilities: vec![],
        },
    )
    .await;
    let Some(ServerMessage::Challenge { nonce }) = recv(&mut ws).await else {
        panic!("no challenge")
    };
    let sig = key
        .sign(&host_proof_transcript(SID, id, cert, &cn, &nonce))
        .unwrap();
    send(&mut ws, &ClientMessage::HostProof { signature: sig }).await;
    let verdict = recv(&mut ws).await;
    (ws, verdict)
}

#[tokio::test]
async fn revocation_evicts_and_new_accounts_apply_live() {
    let dir = tempfile::tempdir().unwrap();
    let store = DataDir::new(dir.path());
    let ca = store.init(&[]).unwrap();
    let org = ca.org_id();
    let (db, _, _) = import::open(&store, &dir.path().join("master.key"))
        .await
        .unwrap();
    // No CA given to the builder: it comes from the database.
    let auth = Arc::new(FleetAuthenticator::new(SID).with_accounts(
        org,
        Default::default(),
        SessionTokens::new([3; 32]),
    ));
    let state = AppState::new(auth.clone());
    let mut seen = Seen::default();
    assert!(reload_once(&db, &auth, &state, &mut seen).await.unwrap());
    assert!(
        !reload_once(&db, &auth, &state, &mut seen).await.unwrap(),
        "nothing changed, nothing reloaded"
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/v1/ws", listener.local_addr().unwrap());
    let st = state.clone();
    tokio::spawn(async move { axum::serve(listener, router(st)).await.unwrap() });

    // A host connects, then gets revoked in the database.
    let id = HostId::new();
    let key = HostKey::generate().unwrap();
    let cert = ca.issue_host_cert(id, &key).unwrap();
    let (mut ws, verdict) = host_hello(&url, id, &key, &cert).await;
    assert!(matches!(verdict, Some(ServerMessage::Welcome { .. })));
    assert_eq!(state.online_hosts().await, 1);

    db.revoke_host(id, false).await.unwrap();
    assert!(reload_once(&db, &auth, &state, &mut seen).await.unwrap());
    assert_eq!(
        state.online_hosts().await,
        0,
        "revoked host must be evicted immediately"
    );
    assert!(matches!(
        recv(&mut ws).await,
        Some(ServerMessage::Error {
            code: ErrorCode::Forbidden,
            ..
        })
    ));
    let (_, again) = host_hello(&url, id, &key, &cert).await;
    assert!(
        matches!(
            again,
            Some(ServerMessage::Error {
                code: ErrorCode::Unauthenticated,
                ..
            })
        ),
        "{again:?}"
    );

    // An account added after startup works after the reload.
    db.add_account(org, "late", "a perfectly long password")
        .await
        .unwrap();
    assert!(reload_once(&db, &auth, &state, &mut seen).await.unwrap());
    let (mut ws, _) = connect_async(&url).await.unwrap();
    send(
        &mut ws,
        &ClientMessage::PasswordLogin {
            protocol_version: PROTOCOL_VERSION,
            username: "late".into(),
            password: "a perfectly long password".into(),
        },
    )
    .await;
    assert!(matches!(
        recv(&mut ws).await,
        Some(ServerMessage::LoginToken { .. })
    ));
    // So does an operator token.
    let (_, token) = db.add_operator(org).await.unwrap();
    reload_once(&db, &auth, &state, &mut seen).await.unwrap();
    let (mut ws, _) = connect_async(&url).await.unwrap();
    send(
        &mut ws,
        &ClientMessage::OperatorHello {
            protocol_version: PROTOCOL_VERSION,
            token,
        },
    )
    .await;
    assert!(matches!(
        recv(&mut ws).await,
        Some(ServerMessage::Welcome { .. })
    ));
}
