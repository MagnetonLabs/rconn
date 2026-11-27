//! Company SSO (OIDC against an in-process identity provider), domain verification
//! through a fake DNS, and passkeys with a software authenticator.

mod portal_support;

use axum::extract::{Form, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use openidconnect::core::{
    CoreIdToken, CoreIdTokenClaims, CoreJsonWebKeySet, CoreJwsSigningAlgorithm,
    CoreProviderMetadata, CoreResponseType, CoreRsaPrivateSigningKey, CoreSubjectIdentifierType,
};
use openidconnect::{
    Audience, AuthUrl, EmptyAdditionalClaims, EmptyAdditionalProviderMetadata, EndUserEmail,
    IssuerUrl, JsonWebKeyId, JsonWebKeySetUrl, Nonce, PrivateSigningKey, ResponseTypes,
    StandardClaims, SubjectIdentifier, TokenUrl,
};
use portal_support::{Browser, ORIGIN, Portal, Resp};
use rconn_core::ids::OrgId;
use rconn_signaling::db::Role;
use rconn_signaling::portal::sso::{Dns, StaticTxt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use webauthn_authenticator_rs::WebauthnAuthenticator;
use webauthn_authenticator_rs::softpasskey::SoftPasskey;
use webauthn_rs::prelude::{CreationChallengeResponse, RequestChallengeResponse, Url};

/// Test-only RSA key the mock IdP signs ID tokens with.
const IDP_KEY: &str = "-----BEGIN RSA PRIVATE KEY-----
MIIEpAIBAAKCAQEAwhaKpIB46RH+PVWBqqKS82URpGgr/K0kV98cZJfp5BH5X22V
zo1kXGLi+mKtaWiWttHY1i73IjDGsLE/P3AGfQB3Z1InkuWCoCrIXCTw95BpjoSt
1r05vk4FrgEMtqdyyJmCMWA2gN5V/wL6lg6+FqlMdxKSCssNCWewhwvMC0e0JVYM
bn8oL7nOQ9pzXtvH4LNNdmKHapgnhoI5FS1eNZjg0CiCIOSpsRgNm4fucgO6cH1c
mM+DNQtaGEBkNjjfiH0TjQgS3/3aRZpGokjNGJVRNyfuVa5p0NnlKciPnbW0uFIh
BgsPdk5CkjvGFFGVHDpXQwXj0T3pe4BN1nqYYwIDAQABAoIBAClgCtBEvwHYnx1O
JCVPIQqYCinFN1NXSb8dxfy/0dGX71RjlSMCS1nSjtaXwKYO8MoqdBI80txMD59N
OdF5PTjap5m0DtGm8bDZTfsuuNvdwlLqf/CeSDgEg/tzJ9WsXrDa4MFL5HKWczi4
1ti0bduA3dcoc0+vt6jAnQY9ApOIp1cAR71mw6Fe8dGvf4fa/38gCeHB++1KC0Oh
n0fqf9IEek086wD/m3HHi7imSI/Lpcki77r+Y/tc6wn5b0gcemWwTc4F8VnJ9r1F
0EgqDbWrukJSmaj+b0CAllKYracxjYf/PckMbA9Vip/hKsTupjDHqAlHUcb3voPo
iWmDbTECgYEA8BEQg5lUjjGS3DDCTV1u0eWrTyYNa4+C4/KXUND+EMn8iYdg2A8N
YLC8E2hLGpQvv9L7PG+A1V+kpXElz/Uixx3X/uK2bWXXG8HZ2t2r2/Kgxut8I6hc
y1seqcO+U3UD2R1qM1wHggSOAaCJ9eLJ+w7oqt0jrc3vOmTzor02oWsCgYEAzvhC
/gS1BJjwoXm82yDqEYTQv1bNnG2QIeLzHi7cLDoKKsy00rtAG1TsVyJchhARVZPy
zOGtiodFWWcgkPbcjLtwe5VXy9oR6zhW8JQRSAHsQlse2KpZQWUmwi8kajod5jRV
kBkf88SnkozO6u0HNJ9l7KkIiweMhHk9ZO+6iukCgYEAlthiJA7IjsCfozQQI0S+
Sa2iww0KCbgXatwYMHPh8PK6eEBUne53XXYv95Hk/0J17nbH3uXBclGyxzIdrFpA
DBW4jzQtpPgE/jAEj/4R6xa958A4NL+p7y3GSq3wfchdLqPJA5SkMxJhmKwjhBmJ
CPQJRrPFo4Ru+aZNYMi79xkCgYAUUacyDQBAfUtSbwMteCPCqIld34W8D72Qthks
I2mWJfSNkx9FeQCoLkQtob/3oWgyQJ4boyRIb1NNnaRC1xl774Lxfw3pg3Uy5IsU
f1Ta/uVU9mEm4EJ2+xk9AyfODfqVeRnVVcIfSnUxHAXMZozIi8WT4FxoWctfzg9K
84Ya0QKBgQChH7MlzoYzNoKC+GJpNbWXsxW+lVHfns7GuqlLSGCUaVHZfEnQ8n07
2B5KR8P1OLtZG3gRFyRvUhPw+f/cUHg5qg+DWYqQHxhuTcUFgWjDcQvpQ+Uy6l56
DI4yrKUQrFB8UuV9whqKwgDf7yZVNrY4EEN5b7o9z6cXA6wGWJHUpQ==
-----END RSA PRIVATE KEY-----
";

const CLIENT_ID: &str = "rconn-portal";
const CLIENT_SECRET: &str = "idp-client-secret-123";

// -- mock identity provider ------------------------------------------------------------

/// The account the IdP signs someone in as.
#[derive(Clone)]
struct Who {
    sub: String,
    email: Option<String>,
    email_verified: Option<bool>,
}

fn who(sub: &str, email: &str) -> Who {
    Who {
        sub: sub.into(),
        email: Some(email.into()),
        email_verified: Some(true),
    }
}

struct Grant {
    nonce: String,
    challenge: String,
    redirect_uri: String,
    who: Who,
}

struct Idp {
    issuer: String,
    key: CoreRsaPrivateSigningKey,
    grants: Mutex<HashMap<String, Grant>>,
    /// Sign tokens with this `iss` instead of the real one.
    iss_override: Mutex<Option<String>>,
    /// Put this nonce in tokens instead of the requested one.
    nonce_override: Mutex<Option<String>>,
}

impl Idp {
    async fn start() -> Arc<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let idp = Arc::new(Self {
            issuer,
            key: CoreRsaPrivateSigningKey::from_pem(IDP_KEY, Some(JsonWebKeyId::new("k1".into())))
                .unwrap(),
            grants: Mutex::default(),
            iss_override: Mutex::default(),
            nonce_override: Mutex::default(),
        });
        let app = axum::Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/token", post(token))
            .with_state(idp.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        idp
    }

    /// What the IdP does when the browser arrives at its authorize URL and the person
    /// signs in as `who`: a code for the redirect URI.
    fn authorize(&self, location: &str, who: Who) -> (String, String) {
        let url = Url::parse(location).unwrap();
        assert!(
            location.starts_with(&format!("{}/authorize?", self.issuer)),
            "{location}"
        );
        let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["client_id"], CLIENT_ID);
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(
            q["redirect_uri"],
            format!("{ORIGIN}/api/v1/auth/sso/callback")
        );
        let scopes: Vec<&str> = q["scope"].split(' ').collect();
        assert!(
            scopes.contains(&"openid") && scopes.contains(&"email"),
            "{scopes:?}"
        );
        let code = format!("code-{}", rand_hex());
        self.grants.lock().unwrap().insert(
            code.clone(),
            Grant {
                nonce: q["nonce"].clone(),
                challenge: q["code_challenge"].clone(),
                redirect_uri: q["redirect_uri"].clone(),
                who,
            },
        );
        (code, q["state"].clone())
    }
}

fn rand_hex() -> String {
    format!("{:016x}", rand::random::<u64>())
}

async fn discovery(State(idp): State<Arc<Idp>>) -> Response {
    let i = &idp.issuer;
    let meta = CoreProviderMetadata::new(
        IssuerUrl::new(i.clone()).unwrap(),
        AuthUrl::new(format!("{i}/authorize")).unwrap(),
        JsonWebKeySetUrl::new(format!("{i}/jwks")).unwrap(),
        vec![ResponseTypes::new(vec![CoreResponseType::Code])],
        vec![CoreSubjectIdentifierType::Public],
        vec![CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256],
        EmptyAdditionalProviderMetadata {},
    )
    .set_token_endpoint(Some(TokenUrl::new(format!("{i}/token")).unwrap()));
    axum::Json(meta).into_response()
}

async fn jwks(State(idp): State<Arc<Idp>>) -> Response {
    axum::Json(CoreJsonWebKeySet::new(vec![idp.key.as_verification_key()])).into_response()
}

async fn token(
    State(idp): State<Arc<Idp>>,
    headers: HeaderMap,
    Form(f): Form<HashMap<String, String>>,
) -> Response {
    let bad = |why: &str| {
        (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": "invalid_grant", "error_description": why })),
        )
            .into_response()
    };
    // Client authentication: HTTP Basic with our confidential client's secret.
    let basic = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        .map(|b| String::from_utf8(b).unwrap());
    if basic.as_deref() != Some(&format!("{CLIENT_ID}:{CLIENT_SECRET}")) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({ "error": "invalid_client" })),
        )
            .into_response();
    }
    if f.get("grant_type").map(String::as_str) != Some("authorization_code") {
        return bad("grant_type");
    }
    let Some(grant) = f
        .get("code")
        .and_then(|c| idp.grants.lock().unwrap().remove(c))
    else {
        return bad("unknown or used code");
    };
    let verifier = f.get("code_verifier").cloned().unwrap_or_default();
    if B64.encode(Sha256::digest(verifier.as_bytes())) != grant.challenge {
        return bad("PKCE verifier doesn't match");
    }
    if f.get("redirect_uri") != Some(&grant.redirect_uri) {
        return bad("redirect_uri");
    }
    let iss = idp
        .iss_override
        .lock()
        .unwrap()
        .clone()
        .unwrap_or(idp.issuer.clone());
    let nonce = idp
        .nonce_override
        .lock()
        .unwrap()
        .clone()
        .unwrap_or(grant.nonce);
    let now = chrono::Utc::now();
    let mut std_claims = StandardClaims::new(SubjectIdentifier::new(grant.who.sub));
    if let Some(e) = grant.who.email {
        std_claims = std_claims.set_email(Some(EndUserEmail::new(e)));
    }
    std_claims = std_claims.set_email_verified(grant.who.email_verified);
    let claims = CoreIdTokenClaims::new(
        IssuerUrl::new(iss).unwrap(),
        vec![Audience::new(CLIENT_ID.into())],
        now + chrono::Duration::minutes(5),
        now,
        std_claims,
        EmptyAdditionalClaims {},
    )
    .set_nonce(Some(Nonce::new(nonce)));
    let id_token = CoreIdToken::new(
        claims,
        &idp.key,
        CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
        None,
        None,
    )
    .unwrap();
    axum::Json(json!({
        "access_token": "at",
        "token_type": "bearer",
        "expires_in": 300,
        "id_token": id_token,
    }))
    .into_response()
}

// -- world -----------------------------------------------------------------------------

struct World {
    p: Portal,
    dns: Arc<StaticTxt>,
    idp: Arc<Idp>,
    acme: OrgId,
    owner: Browser,
}

async fn world() -> World {
    let dns = Arc::new(StaticTxt::default());
    let d = dns.clone();
    let p = Portal::with(|c| {
        c.dns = Dns::new(d);
        // The mock IdP listens on http://127.0.0.1.
        c.sso_private_issuers = true;
    })
    .await;
    let acme = p.org("Acme").await;
    p.user("owner@acme.test", true, &[(acme, Role::Owner)])
        .await;
    let owner = p.signed_in("owner@acme.test").await;
    World {
        p,
        dns,
        idp: Idp::start().await,
        acme,
        owner,
    }
}

fn sso_path(org: OrgId, rest: &str) -> String {
    format!("/api/v1/orgs/{org}/sso{rest}")
}

impl World {
    async fn put_sso(&mut self, body: Value) -> Resp {
        let path = sso_path(self.acme, "");
        self.p
            .call(&mut self.owner, Method::PUT, &path, Some(body))
            .await
    }

    /// SSO on for Acme with the mock IdP, and `acme.test` verified.
    async fn sso_on(&mut self, role: &str) {
        let r = self
            .put_sso(json!({
                "issuer": self.idp.issuer,
                "client_id": CLIENT_ID,
                "client_secret": CLIENT_SECRET,
                "auto_join_role": role,
                "enabled": true,
            }))
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        self.verify(self.acme, "acme.test").await;
    }

    /// Add `domain` to `org` (as its owner), publish its TXT record, verify it.
    async fn verify(&mut self, org: OrgId, domain: &str) {
        let mut owner = if org == self.acme {
            self.owner.clone()
        } else {
            self.p.signed_in(&format!("owner@{org}.test")).await
        };
        let r = self
            .p
            .call(
                &mut owner,
                Method::POST,
                &sso_path(org, "/domains"),
                Some(json!({ "domain": domain })),
            )
            .await;
        let txt = if r.status == StatusCode::CONFLICT {
            // Added before: its record is in the settings.
            let got = self
                .p
                .call(&mut owner, Method::GET, &sso_path(org, ""), None)
                .await;
            got.json["domains"]
                .as_array()
                .unwrap()
                .iter()
                .find(|d| d["domain"] == domain)
                .unwrap()["txt_record"]
                .clone()
        } else {
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            r.json["txt_record"].clone()
        };
        self.dns.set(domain, vec![txt.as_str().unwrap().into()]);
        let r = self
            .p
            .call(
                &mut owner,
                Method::POST,
                &sso_path(org, &format!("/domains/{domain}/verify")),
                None,
            )
            .await;
        assert_eq!(r.json["verified"], true, "{}", r.text);
    }

    /// The whole round trip in `b` for `org`, signing in at the IdP as `who`: the
    /// callback's response.
    async fn sso_round_trip(&self, b: &mut Browser, org: OrgId, who: Who) -> Resp {
        let r = self
            .p
            .call(
                b,
                Method::GET,
                &format!("/api/v1/auth/sso/start?org={org}"),
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::FOUND, "{}", r.text);
        let loc = r.headers[header::LOCATION].to_str().unwrap().to_owned();
        let (code, state) = self.idp.authorize(&loc, who);
        self.callback(b, &code, &state).await
    }

    async fn callback(&self, b: &mut Browser, code: &str, state: &str) -> Resp {
        self.p
            .call(
                b,
                Method::GET,
                &format!("/api/v1/auth/sso/callback?code={code}&state={state}"),
                None,
            )
            .await
    }
}

fn location(r: &Resp) -> &str {
    r.headers[header::LOCATION].to_str().unwrap()
}

async fn audit_details(p: &Portal, event: &str) -> Vec<Value> {
    p.audit.flush().await;
    let rows: Vec<String> =
        sqlx::query_scalar("SELECT detail FROM audit WHERE event = ? ORDER BY id")
            .bind(event)
            .fetch_all(p.db.pool())
            .await
            .unwrap();
    rows.iter()
        .map(|d| serde_json::from_str(d).unwrap())
        .collect()
}

// -- SSO settings --------------------------------------------------------------------

#[tokio::test]
async fn sso_settings_keep_the_secret_sealed_and_refuse_bad_input() {
    let mut w = world().await;
    let path = sso_path(w.acme, "");
    let r = w.p.call(&mut w.owner, Method::GET, &path, None).await;
    assert_eq!(
        r.json,
        json!({ "enabled": false, "issuer": "", "client_id": "", "domains": [], "auto_join_role": "viewer" })
    );

    let base =
        json!({ "issuer": w.idp.issuer, "client_id": CLIENT_ID, "auto_join_role": "technician" });
    // On without a secret, owner as join role, a non-URL issuer: refused.
    for bad in [
        json!({ "enabled": true }),
        json!({ "auto_join_role": "owner", "client_secret": "x" }),
        json!({ "issuer": "login.example.com", "client_secret": "x" }),
        json!({ "issuer": "ftp://idp.test", "client_secret": "x" }),
        json!({ "issuer": "https://idp.test/?q=1", "client_secret": "x" }),
    ] {
        let mut body = base.clone();
        for (k, v) in bad.as_object().unwrap() {
            body[k] = v.clone();
        }
        let r = w.put_sso(body.clone()).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{body}: {}", r.text);
        assert_eq!(r.code(), "invalid");
    }
    // An unknown role is refused as a bad body.
    let mut body = base.clone();
    body["auto_join_role"] = json!("boss");
    assert_eq!(w.put_sso(body).await.status, StatusCode::BAD_REQUEST);

    // An IdP that doesn't answer discovery can't be turned on.
    let mut body = base.clone();
    body["issuer"] = json!("http://127.0.0.1:9/nothing");
    body["client_secret"] = json!(CLIENT_SECRET);
    body["enabled"] = json!(true);
    let r = w.put_sso(body).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.text);
    assert!(
        r.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("openid-configuration")
    );

    // Saved: the secret is never returned and is not stored in the clear.
    let mut body = base.clone();
    body["client_secret"] = json!(CLIENT_SECRET);
    body["enabled"] = json!(true);
    let r = w.put_sso(body).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json["enabled"], true);
    assert_eq!(r.json["auto_join_role"], "technician");
    assert!(!r.text.contains(CLIENT_SECRET));
    let got = w.p.call(&mut w.owner, Method::GET, &path, None).await;
    assert!(
        !got.text.contains(CLIENT_SECRET) && !got.text.contains("secret"),
        "{}",
        got.text
    );
    let sealed: Vec<u8> =
        sqlx::query_scalar("SELECT client_secret_sealed FROM org_sso WHERE org_id = ?")
            .bind(w.acme.to_string())
            .fetch_one(w.p.db.pool())
            .await
            .unwrap();
    assert!(
        !sealed
            .windows(CLIENT_SECRET.len())
            .any(|x| x == CLIENT_SECRET.as_bytes())
    );

    // Without a secret in the body the stored one stays (SSO still on and working).
    let r = w
        .put_sso(json!({ "issuer": w.idp.issuer, "client_id": CLIENT_ID, "auto_join_role": "viewer", "enabled": true }))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let after: Vec<u8> =
        sqlx::query_scalar("SELECT client_secret_sealed FROM org_sso WHERE org_id = ?")
            .bind(w.acme.to_string())
            .fetch_one(w.p.db.pool())
            .await
            .unwrap();
    assert_eq!(sealed, after);

    let changes = audit_details(&w.p, "sso.changed").await;
    assert_eq!(changes.len(), 2, "{changes:?}");
    assert_eq!(changes[0]["detail"]["client_secret_changed"], true);
    assert_eq!(changes[1]["detail"]["client_secret_changed"], false);
    assert!(
        !changes
            .iter()
            .any(|c| c.to_string().contains(CLIENT_SECRET))
    );
}

#[tokio::test]
async fn domains_are_verified_by_a_txt_record_and_by_one_company_only() {
    let mut w = world().await;
    let beta = w.p.org("Beta").await;
    w.p.user(&format!("owner@{beta}.test"), true, &[(beta, Role::Owner)])
        .await;
    let mut beta_owner = w.p.signed_in(&format!("owner@{beta}.test")).await;

    let add = |d: &str| json!({ "domain": d });
    for bad in ["", "acme", "-x.test", "a b.test", "bücher.test", "1.2.3.4"] {
        let r =
            w.p.call(
                &mut w.owner,
                Method::POST,
                &sso_path(w.acme, "/domains"),
                Some(add(bad)),
            )
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad:?}");
    }
    let r =
        w.p.call(
            &mut w.owner,
            Method::POST,
            &sso_path(w.acme, "/domains"),
            Some(add(" Acme.TEST. ")),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.json["domain"], "acme.test");
    assert_eq!(r.json["verified"], false);
    let txt = r.json["txt_record"].as_str().unwrap().to_owned();
    assert!(txt.starts_with("rconn-verify="), "{txt}");
    let r =
        w.p.call(
            &mut w.owner,
            Method::POST,
            &sso_path(w.acme, "/domains"),
            Some(add("acme.test")),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);

    // Beta may claim it too (no squatting), but can't verify without the record.
    let r =
        w.p.call(
            &mut beta_owner,
            Method::POST,
            &sso_path(beta, "/domains"),
            Some(add("acme.test")),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let beta_txt = r.json["txt_record"].as_str().unwrap().to_owned();
    assert_ne!(beta_txt, txt);

    let verify = sso_path(w.acme, "/domains/acme.test/verify");
    let r = w.p.call(&mut w.owner, Method::POST, &verify, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["verified"], false, "no TXT record yet");
    // The wrong record (Beta's), or one at a subdomain, doesn't count.
    w.dns
        .set("acme.test", vec![beta_txt.clone(), "v=spf1 -all".into()]);
    w.dns.set("_rconn.acme.test", vec![txt.clone()]);
    let r = w.p.call(&mut w.owner, Method::POST, &verify, None).await;
    assert_eq!(r.json["verified"], false);
    w.dns
        .set("acme.test", vec!["v=spf1 -all".into(), txt.clone()]);
    let r = w.p.call(&mut w.owner, Method::POST, &verify, None).await;
    assert_eq!(
        r.json,
        json!({ "domain": "acme.test", "verified": true, "txt_record": txt })
    );

    // Now Beta can neither verify it (its record is published too) nor add it again.
    w.dns.set("acme.test", vec![txt.clone(), beta_txt]);
    let r =
        w.p.call(
            &mut beta_owner,
            Method::POST,
            &sso_path(beta, "/domains/acme.test/verify"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.text);
    // Beta's owner can't reach Acme's domain through Beta's path, nor Acme's path at all.
    let r =
        w.p.call(
            &mut beta_owner,
            Method::POST,
            &sso_path(beta, "/domains/other.test/verify"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = w.p.call(&mut beta_owner, Method::POST, &verify, None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r =
        w.p.call(&mut beta_owner, Method::GET, &sso_path(w.acme, ""), None)
            .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    let got =
        w.p.call(&mut w.owner, Method::GET, &sso_path(w.acme, ""), None)
            .await;
    assert_eq!(got.json["domains"].as_array().unwrap().len(), 1);
    let got =
        w.p.call(&mut beta_owner, Method::GET, &sso_path(beta, ""), None)
            .await;
    assert_eq!(got.json["domains"][0]["verified"], false);

    let changes = audit_details(&w.p, "sso.changed").await;
    let actions: Vec<_> = changes
        .iter()
        .map(|c| c["detail"]["action"].clone())
        .collect();
    assert_eq!(
        actions,
        vec![
            json!("domain_added"),
            json!("domain_added"),
            json!("domain_verified")
        ]
    );
}

#[tokio::test]
async fn options_offer_sso_only_for_a_verified_domain_with_sso_on() {
    let mut w = world().await;
    let mut anon = Browser::new();
    let opts = |e: &str| format!("/api/v1/auth/options?email={e}");
    let r =
        w.p.call(&mut anon, Method::GET, &opts("new@acme.test"), None)
            .await;
    assert_eq!(r.json["sso"], Value::Null);

    // Added but unverified: still nothing.
    let r =
        w.p.call(
            &mut w.owner,
            Method::POST,
            &sso_path(w.acme, "/domains"),
            Some(json!({ "domain": "acme.test" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let r = w
        .put_sso(json!({ "issuer": w.idp.issuer, "client_id": CLIENT_ID, "client_secret": CLIENT_SECRET, "auto_join_role": "viewer", "enabled": true }))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let r =
        w.p.call(&mut anon, Method::GET, &opts("new@acme.test"), None)
            .await;
    assert_eq!(r.json["sso"], Value::Null);

    // Verified and on: offered, identically for known and unknown people there.
    w.verify(w.acme, "acme.test").await;
    let unknown =
        w.p.call(&mut anon, Method::GET, &opts("new@acme.test"), None)
            .await;
    assert_eq!(
        unknown.json["sso"],
        json!({ "org_id": w.acme.to_string(), "name": "Acme" })
    );
    let known =
        w.p.call(&mut anon, Method::GET, &opts("Owner@ACME.test"), None)
            .await;
    assert_eq!(known.json, unknown.json);
    // Other domains, look-alikes and subdomains get nothing.
    for e in [
        "x@other.test",
        "x@sub.acme.test",
        "x@acme.test.evil",
        "acme.test",
        "",
    ] {
        let r = w.p.call(&mut anon, Method::GET, &opts(e), None).await;
        assert_eq!(r.json["sso"], Value::Null, "{e}");
    }
    // SSO off: nothing either.
    let r = w
        .put_sso(json!({ "issuer": w.idp.issuer, "client_id": CLIENT_ID, "auto_join_role": "viewer", "enabled": false }))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let r =
        w.p.call(&mut anon, Method::GET, &opts("new@acme.test"), None)
            .await;
    assert_eq!(r.json["sso"], Value::Null);
}

// -- SSO sign-in ------------------------------------------------------------------------

#[tokio::test]
async fn sso_signs_in_and_auto_joins_people_at_the_verified_domain() {
    let mut w = world().await;
    w.sso_on("technician").await;
    let mut b = Browser::new();
    let r = w
        .sso_round_trip(&mut b, w.acme, who("idp-sub-1", "new@acme.test"))
        .await;
    assert_eq!(r.status, StatusCode::FOUND, "{}", r.text);
    assert_eq!(location(&r), "/admin/");
    assert!(b.session().is_some(), "signed in");
    assert!(
        !b.cookies.contains_key("__Host-rconn_sso"),
        "round-trip cookie cleared"
    );
    let me = w.p.call(&mut b, Method::GET, "/api/v1/me", None).await;
    assert_eq!(me.json["user"]["email"], "new@acme.test");
    assert_eq!(
        me.json["memberships"],
        json!([{ "org_id": w.acme.to_string(), "org_name": "Acme", "role": "technician" }])
    );
    let first = me.json["user"]["id"].clone();

    // Again from a new browser: the same person (linked by issuer + subject), even if
    // the IdP now reports another address.
    let mut b2 = Browser::new();
    let r = w
        .sso_round_trip(&mut b2, w.acme, who("idp-sub-1", "renamed@elsewhere.test"))
        .await;
    assert_eq!(location(&r), "/admin/");
    let me = w.p.call(&mut b2, Method::GET, "/api/v1/me", None).await;
    assert_eq!(me.json["user"]["id"], first);

    // Someone who already has an account at that domain (and another company) is linked
    // to it by email and joins Acme, keeping their other membership.
    let other = w.p.org("Other").await;
    w.p.user("alice@acme.test", true, &[(other, Role::Admin)])
        .await;
    let mut b3 = Browser::new();
    let r = w
        .sso_round_trip(&mut b3, w.acme, who("idp-sub-alice", "alice@acme.test"))
        .await;
    assert_eq!(location(&r), "/admin/");
    let me = w.p.call(&mut b3, Method::GET, "/api/v1/me", None).await;
    assert_eq!(me.json["user"]["email"], "alice@acme.test");
    assert_eq!(me.json["memberships"].as_array().unwrap().len(), 2);

    // A member keeps their role (the owner isn't demoted to the join role).
    let mut b4 = Browser::new();
    let r = w
        .sso_round_trip(&mut b4, w.acme, who("idp-sub-owner", "owner@acme.test"))
        .await;
    assert_eq!(location(&r), "/admin/");
    let me = w.p.call(&mut b4, Method::GET, "/api/v1/me", None).await;
    assert_eq!(me.json["memberships"][0]["role"], "owner");

    let signins = audit_details(&w.p, "sign_in").await;
    let sso = signins
        .iter()
        .filter(|d| d.to_string().contains("\"sso\""))
        .count();
    // Four SSO sign-ins; Alice's shows in both of her companies' logs.
    assert_eq!(sso, 5, "{signins:?}");
}

/// Start a round trip in a copy of `b`: the browser after it and the IdP URL.
async fn start(w: &World, b: &Browser) -> (Browser, String) {
    let mut b = b.clone();
    let path = format!("/api/v1/auth/sso/start?org={}", w.acme);
    let r = w.p.call(&mut b, Method::GET, &path, None).await;
    assert_eq!(r.status, StatusCode::FOUND);
    let loc = location(&r).to_owned();
    (b, loc)
}

#[tokio::test]
async fn sso_refuses_replays_wrong_nonce_wrong_issuer_and_other_browsers() {
    let mut w = world().await;
    w.sso_on("viewer").await;

    // Replay: the same code and state a second time, from the same browser.
    let (mut b, loc) = start(&w, &Browser::new()).await;
    let (code, state) = w.idp.authorize(&loc, who("s1", "a@acme.test"));
    let mut replay = b.clone();
    let r = w.callback(&mut b, &code, &state).await;
    assert_eq!(location(&r), "/admin/");
    let r = w.callback(&mut replay, &code, &state).await;
    assert_eq!(location(&r), "/admin/signin?sso_error=expired");
    assert!(replay.session().is_none());

    // Finished in another browser (no round-trip cookie): refused.
    let (_b, loc) = start(&w, &Browser::new()).await;
    let (code, state) = w.idp.authorize(&loc, who("s2", "b@acme.test"));
    let mut thief = Browser::at("198.51.100.7");
    let r = w.callback(&mut thief, &code, &state).await;
    assert_eq!(location(&r), "/admin/signin?sso_error=failed");
    assert!(thief.session().is_none());

    // The IdP's token carries another nonce: refused.
    *w.idp.nonce_override.lock().unwrap() = Some("not-the-nonce".into());
    let mut b = Browser::new();
    let r = w
        .sso_round_trip(&mut b, w.acme, who("s3", "c@acme.test"))
        .await;
    assert_eq!(location(&r), "/admin/signin?sso_error=failed");
    assert!(b.session().is_none());
    *w.idp.nonce_override.lock().unwrap() = None;

    // Signed by the right key but naming another issuer: refused.
    *w.idp.iss_override.lock().unwrap() = Some("https://evil.example".into());
    let r = w
        .sso_round_trip(&mut b, w.acme, who("s4", "d@acme.test"))
        .await;
    assert_eq!(location(&r), "/admin/signin?sso_error=failed");
    assert!(b.session().is_none());
    *w.idp.iss_override.lock().unwrap() = None;

    // A state that was never issued, and a callback without one.
    let r = w.callback(&mut b, "c", &"A".repeat(43)).await;
    assert_eq!(location(&r), "/admin/signin?sso_error=expired");
    let r =
        w.p.call(
            &mut b,
            Method::GET,
            "/api/v1/auth/sso/callback?code=x",
            None,
        )
        .await;
    assert_eq!(location(&r), "/admin/signin?sso_error=failed");
    // The IdP said no.
    let (mut b5, loc) = start(&w, &Browser::new()).await;
    let (_code, state) = w.idp.authorize(&loc, who("s5", "e@acme.test"));
    let r =
        w.p.call(
            &mut b5,
            Method::GET,
            &format!("/api/v1/auth/sso/callback?error=access_denied&state={state}"),
            None,
        )
        .await;
    assert_eq!(location(&r), "/admin/signin?sso_error=failed");

    // None of these created anyone.
    for e in ["b@acme.test", "c@acme.test", "d@acme.test", "e@acme.test"] {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE email = ?")
            .bind(e)
            .fetch_one(w.p.db.pool())
            .await
            .unwrap();
        assert_eq!(n, 0, "{e}");
    }
    let failed = audit_details(&w.p, "sign_in.failed").await;
    assert!(failed.len() >= 4, "{failed:?}");
    assert!(failed.iter().all(|d| d.to_string().contains("\"sso\"")));
}

#[tokio::test]
async fn sso_never_links_or_joins_an_unverified_domain() {
    let mut w = world().await;
    w.sso_on("admin").await;
    let mut b = Browser::new();
    // An address outside Acme's verified domains.
    let r = w
        .sso_round_trip(&mut b, w.acme, who("x1", "someone@gmail.test"))
        .await;
    assert_eq!(location(&r), "/admin/signin?sso_error=domain");
    // The IdP says the address isn't verified.
    let mut unverified = who("x2", "new@acme.test");
    unverified.email_verified = Some(false);
    let r = w.sso_round_trip(&mut b, w.acme, unverified).await;
    assert_eq!(location(&r), "/admin/signin?sso_error=domain");
    // No email at all.
    let r = w
        .sso_round_trip(
            &mut b,
            w.acme,
            Who {
                sub: "x3".into(),
                email: None,
                email_verified: None,
            },
        )
        .await;
    assert_eq!(location(&r), "/admin/signin?sso_error=domain");
    assert!(b.session().is_none());

    // Another company's IdP claiming an Acme address can't take over that person, even
    // though they exist, and can't add them to Beta.
    let beta = w.p.org("Beta").await;
    w.p.user(&format!("owner@{beta}.test"), true, &[(beta, Role::Owner)])
        .await;
    let mut beta_owner = w.p.signed_in(&format!("owner@{beta}.test")).await;
    let r = w
        .p
        .call(
            &mut beta_owner,
            Method::PUT,
            &sso_path(beta, ""),
            Some(json!({ "issuer": w.idp.issuer, "client_id": CLIENT_ID, "client_secret": CLIENT_SECRET, "auto_join_role": "admin", "enabled": true })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let path = format!("/api/v1/auth/sso/start?org={beta}");
    let r = w.p.call(&mut b, Method::GET, &path, None).await;
    let (code, state) = w
        .idp
        .authorize(location(&r), who("evil", "owner@acme.test"));
    let r = w.callback(&mut b, &code, &state).await;
    assert_eq!(location(&r), "/admin/signin?sso_error=domain");
    assert!(b.session().is_none());
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memberships WHERE org_id = ?")
        .bind(beta.to_string())
        .fetch_one(w.p.db.pool())
        .await
        .unwrap();
    assert_eq!(n, 1, "only Beta's owner");

    // Nobody was created for the refused addresses.
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM users WHERE email IN ('someone@gmail.test', 'new@acme.test')",
    )
    .fetch_one(w.p.db.pool())
    .await
    .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn sso_start_needs_a_company_with_sso_on() {
    let w = world().await;
    let mut b = Browser::new();
    for org in [w.acme.to_string(), OrgId::new().to_string(), "nope".into()] {
        let r =
            w.p.call(
                &mut b,
                Method::GET,
                &format!("/api/v1/auth/sso/start?org={org}"),
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::FOUND);
        assert_eq!(location(&r), "/admin/signin?sso_error=unavailable");
    }
}

// -- passkeys ---------------------------------------------------------------------------

fn origin() -> Url {
    Url::parse(ORIGIN).unwrap()
}

/// Register a passkey for the person signed in in `b`.
async fn register(p: &Portal, b: &mut Browser, key: &mut SoftPasskey, name: &str) -> Resp {
    let r = p
        .call(
            b,
            Method::POST,
            "/api/v1/auth/passkey/register/start",
            Some(json!({ "name": name })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json["publicKey"]["rp"]["id"], "portal.test");
    let options: CreationChallengeResponse = serde_json::from_value(r.json).unwrap();
    let cred = key.do_registration(origin(), options).unwrap();
    p.call(
        b,
        Method::POST,
        "/api/v1/auth/passkey/register/finish",
        Some(json!({ "name": name, "credential": cred })),
    )
    .await
}

/// Start a passkey sign-in for `email` in `b`: the request options.
async fn login_start(p: &Portal, b: &mut Browser, email: &str) -> Value {
    let r = p
        .call(
            b,
            Method::POST,
            "/api/v1/auth/passkey/login/start",
            Some(json!({ "email": email })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    r.json
}

async fn login_finish(p: &Portal, b: &mut Browser, cred: Value) -> Resp {
    p.call(
        b,
        Method::POST,
        "/api/v1/auth/passkey/login/finish",
        Some(json!({ "credential": cred })),
    )
    .await
}

#[tokio::test]
async fn passkeys_register_and_sign_in_with_a_software_authenticator() {
    let w = world().await;
    let p = &w.p;
    p.user("alice@acme.test", true, &[(w.acme, Role::Admin)])
        .await;
    let mut b = p.signed_in("alice@acme.test").await;
    let mut key = SoftPasskey::new(true);
    let r = register(p, &mut b, &mut key, "Laptop").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json["name"], "Laptop");
    assert_eq!(r.json["last_used"], Value::Null);
    let id = r.json["id"].as_str().unwrap().to_owned();
    // A finish without a start is refused.
    let r = p
        .call(
            &mut b,
            Method::POST,
            "/api/v1/auth/passkey/register/finish",
            Some(json!({ "name": "x", "credential": {} })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    // Sign in with it from a new browser.
    let mut fresh = Browser::new();
    let options = login_start(p, &mut fresh, "alice@acme.test").await;
    assert_eq!(
        options["publicKey"]["allowCredentials"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(fresh.cookies.contains_key("__Host-rconn_passkey"));
    let rcr: RequestChallengeResponse = serde_json::from_value(options).unwrap();
    let cred = serde_json::to_value(key.do_authentication(origin(), rcr).unwrap()).unwrap();
    let mut again = fresh.clone();
    let r = login_finish(p, &mut fresh, cred.clone()).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json["user"]["email"], "alice@acme.test");
    assert!(fresh.session().is_some());
    assert!(!fresh.cookies.contains_key("__Host-rconn_passkey"));

    // The same assertion again: the ceremony was single use.
    let r = login_finish(p, &mut again, cred).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(again.session().is_none());

    let list = p
        .call(&mut b, Method::GET, "/api/v1/me/passkeys", None)
        .await;
    assert_eq!(list.json.as_array().unwrap().len(), 1);
    assert_eq!(list.json[0]["id"], id);
    assert!(list.json[0]["last_used"].is_string(), "{}", list.text);

    let signins = audit_details(p, "sign_in").await;
    assert!(
        signins
            .iter()
            .any(|d| d.to_string().contains("\"passkey\"")),
        "{signins:?}"
    );
}

#[tokio::test]
async fn passkey_sign_in_start_answers_alike_for_unknown_emails() {
    let w = world().await;
    let p = &w.p;
    p.user("alice@acme.test", true, &[(w.acme, Role::Viewer)])
        .await;
    p.user("nokey@acme.test", true, &[(w.acme, Role::Viewer)])
        .await;
    let mut alice = p.signed_in("alice@acme.test").await;
    let mut key = SoftPasskey::new(true);
    assert_eq!(
        register(p, &mut alice, &mut key, "Key").await.status,
        StatusCode::OK
    );

    let mut b = Browser::new();
    let real = login_start(p, &mut b, "alice@acme.test").await;
    let shape = |v: &Value| {
        let mut keys: Vec<String> = v["publicKey"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        keys.sort();
        (
            keys,
            v["publicKey"]["allowCredentials"].as_array().unwrap().len(),
        )
    };
    let ghost = login_start(p, &mut b, "nobody@acme.test").await;
    let nokey = login_start(p, &mut b, "nokey@acme.test").await;
    assert_eq!(shape(&real), shape(&ghost));
    assert_eq!(shape(&real), shape(&nokey));
    // The made-up credential is stable per email, so asking twice tells nothing.
    let ghost2 = login_start(p, &mut b, "NOBODY@acme.test").await;
    assert_eq!(
        ghost["publicKey"]["allowCredentials"][0]["id"],
        ghost2["publicKey"]["allowCredentials"][0]["id"]
    );
    assert_ne!(
        ghost["publicKey"]["allowCredentials"][0]["id"],
        nokey["publicKey"]["allowCredentials"][0]["id"]
    );

    // Alice's real passkey answering a challenge started for someone else: refused.
    let mut other = Browser::new();
    let opts = login_start(p, &mut other, "nobody@acme.test").await;
    let mut rcr: RequestChallengeResponse = serde_json::from_value(opts).unwrap();
    let alices: RequestChallengeResponse = serde_json::from_value(real.clone()).unwrap();
    rcr.public_key.allow_credentials = alices.public_key.allow_credentials;
    let cred = key.do_authentication(origin(), rcr).unwrap();
    let r = login_finish(p, &mut other, serde_json::to_value(cred).unwrap()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(other.session().is_none());

    // A forged credential for Alice's real challenge.
    let mut forger = Browser::new();
    let _ = login_start(p, &mut forger, "alice@acme.test").await;
    let forged = json!({
        "id": "eA", "rawId": "eA", "type": "public-key",
        "response": { "clientDataJSON": "e30", "authenticatorData": "AA", "signature": "AA" },
    });
    let r = login_finish(p, &mut forger, forged).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(forger.session().is_none());
    let failed = audit_details(p, "sign_in.failed").await;
    assert!(
        failed.iter().any(|d| d.to_string().contains("\"passkey\"")),
        "{failed:?}"
    );
}

#[tokio::test]
async fn passkeys_belong_to_their_owner() {
    let w = world().await;
    let p = &w.p;
    let beta = p.org("Beta").await;
    p.user("alice@acme.test", true, &[(w.acme, Role::Viewer)])
        .await;
    p.user("bob@beta.test", true, &[(beta, Role::Owner)]).await;
    let mut alice = p.signed_in("alice@acme.test").await;
    let mut bob = p.signed_in("bob@beta.test").await;
    let mut key = SoftPasskey::new(true);
    let r = register(p, &mut alice, &mut key, "Alice's key").await;
    let id = r.json["id"].as_str().unwrap().to_owned();
    let mut b = Browser::new();
    let alices: RequestChallengeResponse =
        serde_json::from_value(login_start(p, &mut b, "alice@acme.test").await).unwrap();

    let list = p
        .call(&mut bob, Method::GET, "/api/v1/me/passkeys", None)
        .await;
    assert_eq!(list.json, json!([]));
    let path = format!("/api/v1/me/passkeys/{id}");
    let r = p.call(&mut bob, Method::DELETE, &path, None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let mut anon = Browser::new();
    assert_eq!(
        p.call(&mut anon, Method::GET, "/api/v1/me/passkeys", None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    // Bob can't finish a registration Alice started.
    let start = p
        .call(
            &mut alice,
            Method::POST,
            "/api/v1/auth/passkey/register/start",
            Some(json!({ "name": "x" })),
        )
        .await;
    let options: CreationChallengeResponse = serde_json::from_value(start.json).unwrap();
    let cred = SoftPasskey::new(true)
        .do_registration(origin(), options)
        .unwrap();
    let r = p
        .call(
            &mut bob,
            Method::POST,
            "/api/v1/auth/passkey/register/finish",
            Some(json!({ "name": "x", "credential": cred })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    let r = p.call(&mut alice, Method::DELETE, &path, None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let list = p
        .call(&mut alice, Method::GET, "/api/v1/me/passkeys", None)
        .await;
    assert_eq!(list.json, json!([]));
    // A deleted passkey no longer signs in.
    let opts = login_start(p, &mut b, "alice@acme.test").await;
    let mut rcr: RequestChallengeResponse = serde_json::from_value(opts).unwrap();
    rcr.public_key.allow_credentials = alices.public_key.allow_credentials;
    let cred = key.do_authentication(origin(), rcr).unwrap();
    let r = login_finish(p, &mut b, serde_json::to_value(cred).unwrap()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn passkeys_are_off_without_an_https_portal_url() {
    let p = Portal::with(|c| c.portal_url = None).await;
    let org = p.org("Acme").await;
    p.user("alice@acme.test", true, &[(org, Role::Viewer)])
        .await;
    let mut anon = Browser::new();
    let r = p
        .call(
            &mut anon,
            Method::GET,
            "/api/v1/auth/options?email=alice@acme.test",
            None,
        )
        .await;
    assert_eq!(r.json["passkey"], false);
    let r = p
        .call(
            &mut anon,
            Method::POST,
            "/api/v1/auth/passkey/login/start",
            Some(json!({ "email": "alice@acme.test" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.text);
}
