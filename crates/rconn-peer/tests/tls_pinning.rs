//! Self-hosted servers are authenticated by a pinned public-key fingerprint.

use rconn_core::ids::{OperatorId, OrgId};
use rconn_core::tls::generate_server_tls;
use rconn_peer::signaling::SignalingClient;
use rconn_peer::trust::ServerTrust;
use rconn_signaling::auth::OperatorIdentity;
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::{AppState, router};
use std::sync::Arc;

/// Start a TLS signaling server; returns its wss URL and pinned fingerprint.
async fn start_tls() -> (String, String) {
    let tls = generate_server_tls(&["localhost".into()]).unwrap();
    let org = OrgId::new();
    let op = OperatorIdentity { operator_id: OperatorId::new(), org_id: org };
    let auth = FleetAuthenticator::new("wss://pinned.test").with_operator_token("tok", op);
    let cfg = axum_server::tls_rustls::RustlsConfig::from_pem(tls.cert_pem.into_bytes(), tls.key_pem.into_bytes())
        .await
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let server = axum_server::from_tcp_rustls(listener, cfg).unwrap();
    let app = router(AppState::new(Arc::new(auth)));
    tokio::spawn(async move { server.serve(app.into_make_service()).await.unwrap() });
    (format!("wss://localhost:{}/v1/ws", addr.port()), tls.fingerprint)
}

#[tokio::test]
async fn correct_pin_connects() {
    let (url, fp) = start_tls().await;
    let sig = SignalingClient::connect_operator(&url, &ServerTrust::Pinned(fp), "tok").await.unwrap();
    assert!(sig.ice_servers.is_empty());
}

#[tokio::test]
async fn wrong_pin_is_refused() {
    let (url, _) = start_tls().await;
    let other = generate_server_tls(&[]).unwrap().fingerprint;
    let err = SignalingClient::connect_operator(&url, &ServerTrust::Pinned(other), "tok").await.err().expect("must fail");
    assert!(format!("{err:#}").to_lowercase().contains("certificate"), "{err:#}");
}

#[tokio::test]
async fn self_signed_server_is_refused_without_a_pin() {
    let (url, _) = start_tls().await;
    assert!(SignalingClient::connect_operator(&url, &ServerTrust::WebPki, "tok").await.is_err());
}

#[tokio::test]
async fn pinned_profile_refuses_plaintext() {
    let (_, fp) = start_tls().await;
    let err = SignalingClient::connect_operator("ws://127.0.0.1:1/v1/ws", &ServerTrust::Pinned(fp), "tok")
        .await
        .err()
        .expect("must refuse");
    assert!(format!("{err:#}").contains("plaintext"), "{err:#}");
}

#[tokio::test]
async fn malformed_pin_is_rejected() {
    let (url, _) = start_tls().await;
    assert!(SignalingClient::connect_operator(&url, &ServerTrust::Pinned("md5:abc".into()), "tok").await.is_err());
}
