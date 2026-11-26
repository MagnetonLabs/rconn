//! Computers without an account (the public server, AnyDesk-style), over real WebSockets
//! with real certificates: they enroll themselves, nobody owns them, and no session to
//! them skips the access password.

use futures_util::{SinkExt, StreamExt};
use rconn_core::PROTOCOL_VERSION;
use rconn_core::fleet::{FleetCa, HostKey, host_proof_transcript};
use rconn_core::ids::{HostId, OperatorId, OrgId};
use rconn_core::protocol::{ClientMessage, ErrorCode, ServerMessage};
use rconn_signaling::audit::{AuditEvent, MemoryAudit};
use rconn_signaling::auth::OperatorIdentity;
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::public::{EnrollLimits, PublicHosts};
use rconn_signaling::{AppState, router};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const SERVER_ID: &str = "wss://connect.test/v1/ws";
type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Server {
    url: String,
    audit: Arc<MemoryAudit>,
    org_ca: FleetCa,
}

/// A server with one org (operator token "op") plus account-less enrollment.
async fn start(limits: Option<EnrollLimits>, trust_forwarded: bool) -> Server {
    let org_ca = FleetCa::generate(OrgId::new()).unwrap();
    let org = org_ca.org_id();
    let public = PublicHosts::new(FleetCa::generate(OrgId::new()).unwrap(), limits, None).unwrap();
    let auth = FleetAuthenticator::new(SERVER_ID)
        .with_org_ca(org, org_ca.cert_pem())
        .with_org_ca(public.org_id(), public.ca_pem())
        .with_operator_token(
            "op",
            OperatorIdentity {
                operator_id: OperatorId::new(),
                org_id: org,
            },
        );
    let audit = Arc::new(MemoryAudit::default());
    let state = AppState::with_audit(Arc::new(auth), audit.clone());
    state.allow_enrollment(
        org,
        FleetCa::from_pem(org_ca.cert_pem(), org_ca.key_pem()).unwrap(),
    );
    state.allow_public_hosts(public);
    if trust_forwarded {
        state.trust_client_ip_header("x-forwarded-for".try_into().unwrap());
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server {
        url: format!("ws://{addr}/v1/ws"),
        audit,
        org_ca,
    }
}

async fn send(ws: &mut Ws, m: &ClientMessage) {
    ws.send(Message::Text(m.to_json().unwrap().into()))
        .await
        .unwrap();
}

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

async fn ws(url: &str, forwarded_for: Option<&str>) -> Ws {
    let mut req = url.into_client_request().unwrap();
    if let Some(ip) = forwarded_for {
        req.headers_mut()
            .insert("x-forwarded-for", ip.parse().unwrap());
    }
    connect_async(req).await.unwrap().0
}

/// Ask for an ID without an account.
async fn enroll(url: &str, key: &HostKey, from: Option<&str>) -> ServerMessage {
    let mut ws = ws(url, from).await;
    send(
        &mut ws,
        &ClientMessage::EnrollAnonymous {
            protocol_version: PROTOCOL_VERSION,
            public_key_der: key.public_der(),
        },
    )
    .await;
    let reply = recv(&mut ws).await.expect("an answer");
    assert!(
        recv(&mut ws).await.is_none(),
        "the socket closes after enrolling"
    );
    reply
}

/// Authenticate as a host with a real certificate and key.
async fn connect_host(url: &str, host_id: HostId, cert: &str, key: &HostKey) -> Ws {
    let mut ws = ws(url, None).await;
    let client_nonce = [7u8; 32];
    send(
        &mut ws,
        &ClientMessage::HostHello {
            protocol_version: PROTOCOL_VERSION,
            host_id,
            cert_pem: cert.into(),
            client_nonce: client_nonce.to_vec(),
            label: Some("LAPTOP".into()),
        },
    )
    .await;
    let Some(ServerMessage::Challenge { nonce }) = recv(&mut ws).await else {
        panic!("expected challenge")
    };
    let t = host_proof_transcript(SERVER_ID, host_id, cert, &client_nonce, &nonce);
    send(
        &mut ws,
        &ClientMessage::HostProof {
            signature: key.sign(&t).unwrap(),
        },
    )
    .await;
    assert!(
        matches!(recv(&mut ws).await, Some(ServerMessage::Welcome { .. })),
        "host must be accepted"
    );
    ws
}

async fn anonymous_host(url: &str) -> (Ws, HostId) {
    let key = HostKey::generate().unwrap();
    let ServerMessage::HostEnrolled { host_id, cert_pem } = enroll(url, &key, None).await else {
        panic!("enrollment refused")
    };
    (connect_host(url, host_id, &cert_pem, &key).await, host_id)
}

async fn operator(url: &str) -> Ws {
    let mut ws = ws(url, None).await;
    send(
        &mut ws,
        &ClientMessage::OperatorHello {
            protocol_version: PROTOCOL_VERSION,
            token: "op".into(),
        },
    )
    .await;
    assert!(matches!(
        recv(&mut ws).await,
        Some(ServerMessage::Welcome { .. })
    ));
    ws
}

#[tokio::test]
async fn an_anonymous_computer_enrolls_and_guests_reach_it_only_with_the_password_check() {
    let s = start(Some(EnrollLimits::default()), false).await;
    let (mut host, host_id) = anonymous_host(&s.url).await;

    let mut guest = ws(&s.url, None).await;
    send(
        &mut guest,
        &ClientMessage::GuestHello {
            protocol_version: PROTOCOL_VERSION,
            host: host_id.short_code(),
        },
    )
    .await;
    assert!(matches!(
        recv(&mut guest).await,
        Some(ServerMessage::Welcome { .. })
    ));
    assert_eq!(
        recv(&mut guest).await,
        Some(ServerMessage::GuestHost { host_id })
    );
    send(&mut guest, &ClientMessage::RequestSession { host_id }).await;
    let Some(ServerMessage::IncomingSession { policy, .. }) = recv(&mut host).await else {
        panic!("host not told")
    };
    assert!(policy.guest, "the host must check the access password");
    assert!(matches!(
        recv(&mut guest).await,
        Some(ServerMessage::SessionReady { .. })
    ));
    let events = s.audit.0.lock().unwrap().clone();
    assert!(
        events.iter().any(
            |e| matches!(e, AuditEvent::AnonymousEnrolled { host_id: h, .. } if *h == host_id)
        ),
        "enrollment is audited: {events:?}"
    );
}

#[tokio::test]
async fn signed_in_operators_neither_see_nor_reach_computers_owned_by_nobody() {
    let s = start(Some(EnrollLimits::default()), false).await;
    let (mut anon, anon_id) = anonymous_host(&s.url).await;
    // An org computer, for contrast.
    let key = HostKey::generate().unwrap();
    let org_id = HostId::new();
    let cert = s.org_ca.issue_host_cert(org_id, &key).unwrap();
    let mut owned = connect_host(&s.url, org_id, &cert, &key).await;

    let mut op = operator(&s.url).await;
    send(&mut op, &ClientMessage::ListHosts).await;
    let Some(ServerMessage::Hosts { hosts }) = recv(&mut op).await else {
        panic!("no list")
    };
    let ids: Vec<_> = hosts.iter().map(|h| h.host_id).collect();
    assert_eq!(ids, vec![org_id], "only the org's own computer is listed");

    // A session without the password to the anonymous computer: refused, host not told.
    send(&mut op, &ClientMessage::RequestSession { host_id: anon_id }).await;
    assert!(matches!(
        recv(&mut op).await,
        Some(ServerMessage::Error {
            code: ErrorCode::NotFound,
            ..
        })
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(300), anon.next())
            .await
            .is_err(),
        "the anonymous computer must hear nothing"
    );
    let events = s.audit.0.lock().unwrap().clone();
    assert!(
        events.iter().any(
            |e| matches!(e, AuditEvent::SessionRefused { host_id, .. } if *host_id == anon_id)
        ),
        "{events:?}"
    );

    // Its own org computer still works without a password.
    send(&mut op, &ClientMessage::RequestSession { host_id: org_id }).await;
    let Some(ServerMessage::IncomingSession { policy, .. }) = recv(&mut owned).await else {
        panic!("org host not told")
    };
    assert!(!policy.guest);
}

#[tokio::test]
async fn enrollment_is_rate_limited_per_source_and_globally() {
    let s = start(
        Some(EnrollLimits {
            per_source_per_hour: 2,
            global_per_hour: 4,
        }),
        true,
    )
    .await;
    let key = HostKey::generate().unwrap();
    let a = Some("198.51.100.1");
    for _ in 0..2 {
        assert!(matches!(
            enroll(&s.url, &key, a).await,
            ServerMessage::HostEnrolled { .. }
        ));
    }
    let refused = enroll(&s.url, &key, a).await;
    assert!(
        matches!(
            refused,
            ServerMessage::Error {
                code: ErrorCode::Forbidden,
                ..
            }
        ),
        "{refused:?}"
    );
    // Another address (the proxy's header, last entry) has its own allowance...
    let b = Some("203.0.113.50, 198.51.100.2");
    for _ in 0..2 {
        assert!(matches!(
            enroll(&s.url, &key, b).await,
            ServerMessage::HostEnrolled { .. }
        ));
    }
    // ...until the server-wide cap (4) is reached.
    let c = Some("192.0.2.9");
    assert!(matches!(
        enroll(&s.url, &key, c).await,
        ServerMessage::Error { .. }
    ));
    let events = s.audit.0.lock().unwrap().clone();
    let refusals: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AuditEvent::AnonymousEnrollRefused { source, reason } => {
                Some((source.map(|s| s.to_string()), reason.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        refusals,
        vec![
            (Some("198.51.100.1".into()), "SourceLimit".into()),
            (Some("192.0.2.9".into()), "GlobalLimit".into()),
        ]
    );
}

#[tokio::test]
async fn servers_without_anonymous_enrollment_refuse_it_and_keep_token_enrollment() {
    let s = start(None, false).await;
    let key = HostKey::generate().unwrap();
    assert!(matches!(
        enroll(&s.url, &key, None).await,
        ServerMessage::Error {
            code: ErrorCode::Forbidden,
            ..
        }
    ));
    let mut op = operator(&s.url).await;
    send(
        &mut op,
        &ClientMessage::EnrollHost {
            public_key_der: key.public_der(),
        },
    )
    .await;
    assert!(matches!(
        recv(&mut op).await,
        Some(ServerMessage::HostEnrolled { .. })
    ));
}
