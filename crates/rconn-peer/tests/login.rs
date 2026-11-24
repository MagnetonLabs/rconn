//! Account login over signaling: password -> session token -> operator connection.

use rconn_core::ids::{OperatorId, OrgId};
use rconn_peer::signaling::SignalingClient;
use rconn_peer::trust::ServerTrust;
use rconn_signaling::accounts::{Accounts, SESSION_TOKEN_TTL, SessionTokens};
use rconn_signaling::auth::OperatorIdentity;
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::{AppState, router};
use std::sync::Arc;

const PASSWORD: &str = "correct horse battery staple";

async fn start(key: [u8; 32]) -> (String, OrgId) {
    let org = OrgId::new();
    let mut accounts = Accounts::default();
    accounts.add("alice", PASSWORD).unwrap();
    let auth = FleetAuthenticator::new("wss://login.test").with_accounts(org, accounts, SessionTokens::new(key));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/v1/ws", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router(AppState::new(Arc::new(auth)))).await.unwrap() });
    (url, org)
}

#[tokio::test]
async fn password_login_yields_a_working_operator_token() {
    let (url, org) = start([1; 32]).await;
    let (token, expires_at) = SignalingClient::password_login(&url, &ServerTrust::WebPki, "alice", PASSWORD).await.unwrap();
    assert!(token.starts_with("v1."));
    assert!(expires_at > 0);
    let sig = SignalingClient::connect_operator(&url, &ServerTrust::WebPki, &token).await.unwrap();
    assert_eq!(sig.org_id, org);
}

#[tokio::test]
async fn wrong_password_and_unknown_user_get_the_same_answer() {
    let (url, _) = start([1; 32]).await;
    let a = SignalingClient::password_login(&url, &ServerTrust::WebPki, "alice", "wrong password!!").await.err().unwrap();
    let b = SignalingClient::password_login(&url, &ServerTrust::WebPki, "mallory", PASSWORD).await.err().unwrap();
    assert_eq!(a.to_string(), b.to_string(), "must not reveal which usernames exist");
    assert!(a.to_string().contains("invalid username or password"));
}

#[tokio::test]
async fn token_from_another_server_key_is_refused() {
    let (url, org) = start([1; 32]).await;
    let forged_by = SessionTokens::new([2; 32]);
    let who = OperatorIdentity { operator_id: OperatorId::new(), org_id: org };
    let (token, _) = forged_by.issue(who, SESSION_TOKEN_TTL).unwrap();
    assert!(SignalingClient::connect_operator(&url, &ServerTrust::WebPki, &token).await.is_err());
}

#[tokio::test]
async fn password_is_never_sent_over_plaintext_to_a_remote_host() {
    let err = SignalingClient::password_login("ws://10.0.0.1:1/v1/ws", &ServerTrust::WebPki, "alice", PASSWORD)
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("plaintext"), "{err}");
}
