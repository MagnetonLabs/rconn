//! OIDC login against a real IdP (Dex). Set RCONN_TEST_DEX=http://127.0.0.1:5556/dex with
//! the Dex config from `scripts/dev-dex.yaml` running; otherwise the test is skipped.

use futures_util::{SinkExt, StreamExt};
use rconn_core::PROTOCOL_VERSION;
use rconn_core::ids::OrgId;
use rconn_core::protocol::{ClientMessage, ErrorCode, ServerMessage};
use rconn_signaling::accounts::SessionTokens;
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::oidc::OidcVerifier;
use rconn_signaling::{AppState, router};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn ask(url: &str, m: ClientMessage) -> Option<ServerMessage> {
    let (mut ws, _): (Ws, _) = connect_async(url).await.unwrap();
    ws.send(Message::Text(m.to_json().unwrap().into()))
        .await
        .unwrap();
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

/// ID token from Dex's password grant (test-only shortcut for the PKCE browser flow).
async fn id_token(issuer: &str, client: &str, secret: Option<&str>) -> String {
    let http = openidconnect::reqwest::Client::new();
    let mut form = vec![
        ("grant_type", "password"),
        ("username", "ops@example.com"),
        ("password", "password"),
        ("scope", "openid email"),
        ("client_id", client),
    ];
    if let Some(s) = secret {
        form.push(("client_secret", s));
    }
    let body = http
        .post(format!("{issuer}/token"))
        .form(&form)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    v["id_token"]
        .as_str()
        .unwrap_or_else(|| panic!("no id_token: {v}"))
        .to_owned()
}

async fn server(issuer: &str, domains: Vec<String>) -> String {
    let org = OrgId::new();
    let verifier = OidcVerifier::discover(issuer, "rconn-viewer", org, domains)
        .await
        .unwrap();
    let auth = FleetAuthenticator::new("wss://oidc.test")
        .with_accounts(org, Default::default(), SessionTokens::new([9; 32]))
        .with_oidc(verifier);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/v1/ws", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, router(AppState::new(Arc::new(auth))))
            .await
            .unwrap()
    });
    url
}

#[tokio::test]
async fn oidc_login_end_to_end() {
    let Ok(issuer) = std::env::var("RCONN_TEST_DEX") else {
        eprintln!("skipped: set RCONN_TEST_DEX to run against Dex");
        return;
    };
    let url = server(&issuer, vec![]).await;

    // The server advertises its IdP.
    let Some(ServerMessage::LoginOptions { password, oidc }) = ask(
        &url,
        ClientMessage::GetLoginOptions {
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await
    else {
        panic!("no login options")
    };
    assert!(!password, "no password accounts configured");
    assert_eq!(oidc.unwrap().client_id, "rconn-viewer");

    // A valid ID token becomes a working session token, with a stable operator identity.
    let mut operators = Vec::new();
    for _ in 0..2 {
        let tok = id_token(&issuer, "rconn-viewer", None).await;
        let Some(ServerMessage::LoginToken { token, .. }) = ask(
            &url,
            ClientMessage::OidcLogin {
                protocol_version: PROTOCOL_VERSION,
                id_token: tok,
            },
        )
        .await
        else {
            panic!("OIDC login refused")
        };
        let Some(ServerMessage::Welcome { .. }) = ask(
            &url,
            ClientMessage::OperatorHello {
                protocol_version: PROTOCOL_VERSION,
                token: token.clone(),
            },
        )
        .await
        else {
            panic!("session token not accepted")
        };
        operators.push(token);
    }

    // A token minted for another app (wrong audience) is refused.
    let foreign = id_token(&issuer, "other-app", Some("other-secret")).await;
    assert!(matches!(
        ask(
            &url,
            ClientMessage::OidcLogin {
                protocol_version: PROTOCOL_VERSION,
                id_token: foreign
            }
        )
        .await,
        Some(ServerMessage::Error {
            code: ErrorCode::Unauthenticated,
            ..
        })
    ));

    // A tampered token is refused.
    let mut forged = id_token(&issuer, "rconn-viewer", None).await;
    let last = forged.pop().unwrap();
    forged.push(if last == 'A' { 'B' } else { 'A' });
    assert!(matches!(
        ask(
            &url,
            ClientMessage::OidcLogin {
                protocol_version: PROTOCOL_VERSION,
                id_token: forged
            }
        )
        .await,
        Some(ServerMessage::Error { .. })
    ));

    // A domain allowlist that excludes the user's verified email domain refuses them.
    let strict = server(&issuer, vec!["corp.example".into()]).await;
    let tok = id_token(&issuer, "rconn-viewer", None).await;
    assert!(matches!(
        ask(
            &strict,
            ClientMessage::OidcLogin {
                protocol_version: PROTOCOL_VERSION,
                id_token: tok
            }
        )
        .await,
        Some(ServerMessage::Error { .. })
    ));
}
