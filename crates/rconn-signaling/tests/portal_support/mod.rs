//! A portal on a temporary database, driven in-process with cookie-carrying "browsers".
#![allow(dead_code)]

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use rconn_core::fleet::FleetCa;
use rconn_core::ids::{OperatorId, OrgId};
use rconn_signaling::AppState;
use rconn_signaling::accounts::SessionTokens;
use rconn_signaling::audit_db::DbAudit;
use rconn_signaling::db::{Db, Role};
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::hosts::DbInventory;
use rconn_signaling::mail::CaptureMailer;
use rconn_signaling::portal::computers::DbTokenEnroller;
use rconn_signaling::portal::{self, PortalConfig, PortalState};
use rconn_signaling::secrets::MasterKey;
use serde_json::Value;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tower::ServiceExt;

pub const ORIGIN: &str = "https://portal.test";
pub const PASSWORD: &str = "correct horse battery staple";
/// The server identity hosts sign for (`Portal::serve_ws`).
pub const SERVER_ID: &str = "wss://portal.test";
/// Key of the desktop app's v1 session tokens.
pub const TOKEN_KEY: [u8; 32] = [7; 32];

pub struct Portal {
    pub app: Router,
    pub db: Db,
    /// The broker the portal drives (evictions), sharing the database's roles.
    pub broker: AppState,
    pub auth: Arc<FleetAuthenticator>,
    pub mail: Arc<CaptureMailer>,
    pub audit: Arc<DbAudit>,
    _dir: tempfile::TempDir,
}

/// A browser: its cookie jar and address.
#[derive(Debug, Clone)]
pub struct Browser {
    pub cookies: HashMap<String, String>,
    pub ip: IpAddr,
}

impl Browser {
    pub fn new() -> Self {
        Self::at("192.0.2.10")
    }
    pub fn at(ip: &str) -> Self {
        Self {
            cookies: HashMap::new(),
            ip: ip.parse().unwrap(),
        }
    }
    fn cookie_header(&self) -> Option<String> {
        (!self.cookies.is_empty()).then(|| {
            self.cookies
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ")
        })
    }
    fn absorb(&mut self, headers: &HeaderMap) {
        for v in headers.get_all(header::SET_COOKIE) {
            let v = v.to_str().unwrap();
            let (kv, attrs) = v.split_once(';').unwrap_or((v, ""));
            let (k, val) = kv.split_once('=').unwrap();
            if attrs.contains("Max-Age=0") {
                self.cookies.remove(k);
            } else {
                self.cookies.insert(k.to_owned(), val.to_owned());
            }
        }
    }
    pub fn session(&self) -> Option<&String> {
        self.cookies.get("__Host-rconn_session")
    }
}

#[derive(Debug)]
pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub text: String,
    pub json: Value,
}

impl Resp {
    pub fn code(&self) -> &str {
        self.json["error"]["code"].as_str().unwrap_or("")
    }
}

impl Portal {
    pub async fn new() -> Self {
        Self::with(|_| {}).await
    }

    pub async fn with(f: impl FnOnce(&mut PortalConfig)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("rconn.db")).await.unwrap();
        let mail = Arc::new(CaptureMailer::default());
        let audit = Arc::new(DbAudit::spawn(db.clone()));
        let auth = Arc::new(
            FleetAuthenticator::new(SERVER_ID)
                .with_db(db.clone())
                .with_accounts(
                    OrgId::new(),
                    Default::default(),
                    SessionTokens::new(TOKEN_KEY),
                ),
        );
        let app = AppState::new(auth.clone());
        let mut config = PortalConfig {
            portal_url: Some(ORIGIN.into()),
            // Tests never ask real DNS.
            dns: rconn_signaling::portal::sso::Dns::none(),
            ..Default::default()
        };
        f(&mut config);
        let state = PortalState::new(
            db.clone(),
            app.clone(),
            Arc::new(MasterKey::generate()),
            mail.clone(),
            audit.clone(),
            config,
        );
        Self {
            app: portal::router(state),
            db,
            broker: app,
            auth,
            mail,
            audit,
            _dir: dir,
        }
    }

    /// A portal plus the signaling broker sharing its `AppState`, serving `/v1/ws` on a
    /// local port, with the database inventory and the portal's enrollment tokens. Hosts
    /// prove themselves for [`BROKER_SERVER_ID`]. Call [`Broker::reload`] after adding orgs.
    pub async fn with_broker() -> (Self, Broker) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("rconn.db")).await.unwrap();
        let mail = Arc::new(CaptureMailer::default());
        let audit = Arc::new(DbAudit::spawn(db.clone()));
        let master = Arc::new(MasterKey::generate());
        let auth = Arc::new(FleetAuthenticator::new(BROKER_SERVER_ID));
        let app = AppState::new(auth.clone());
        app.allow_token_enrollment(Arc::new(DbTokenEnroller::new(
            db.clone(),
            master.clone(),
            audit.clone(),
        )));
        app.record_hosts_in(Arc::new(DbInventory::spawn(db.clone())));
        let state = PortalState::new(
            db.clone(),
            app.clone(),
            master.clone(),
            mail.clone(),
            audit.clone(),
            PortalConfig {
                portal_url: Some(ORIGIN.into()),
                ..Default::default()
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/v1/ws", listener.local_addr().unwrap());
        let ws = rconn_signaling::router(app.clone());
        tokio::spawn(async move {
            axum::serve(
                listener,
                ws.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap()
        });
        let broker = Broker {
            url,
            state: app.clone(),
            auth: auth.clone(),
            master,
        };
        (
            Self {
                app: portal::router(state),
                db,
                broker: app,
                auth,
                mail,
                audit,
                _dir: dir,
            },
            broker,
        )
    }

    /// A new org named `name` whose CA key is sealed in the database with `master` (so
    /// the server can enroll computers into it), and that CA.
    pub async fn org_with_key(&self, name: &str, master: &MasterKey) -> (OrgId, FleetCa) {
        let id = OrgId::new();
        let ca = FleetCa::generate(id).unwrap();
        sqlx::query(
            "INSERT INTO orgs (id, name, created_at, ca_cert_pem, ca_key_sealed)
             VALUES (?, ?, 1795773296, ?, ?)",
        )
        .bind(id.to_string())
        .bind(name)
        .bind(ca.cert_pem())
        .bind(master.seal(id, ca.key_pem().as_bytes()).unwrap())
        .execute(self.db.pool())
        .await
        .unwrap();
        (id, ca)
    }

    /// A new org (with a fresh CA) named `name`.
    pub async fn org(&self, name: &str) -> OrgId {
        let id = OrgId::new();
        let ca = FleetCa::generate(id).unwrap();
        sqlx::query(
            "INSERT INTO orgs (id, name, created_at, ca_cert_pem) VALUES (?, ?, 1795773296, ?)",
        )
        .bind(id.to_string())
        .bind(name)
        .bind(ca.cert_pem())
        .execute(self.db.pool())
        .await
        .unwrap();
        id
    }

    /// A person with this email (and a password login when `password`), member of `orgs`.
    pub async fn user(&self, email: &str, password: bool, orgs: &[(OrgId, Role)]) -> OperatorId {
        let id = OperatorId::new();
        sqlx::query("INSERT INTO users (id, email, display_name, created_at) VALUES (?, ?, ?, 0)")
            .bind(id.to_string())
            .bind(email)
            .bind(email.split('@').next().unwrap())
            .execute(self.db.pool())
            .await
            .unwrap();
        if password {
            sqlx::query(
                "INSERT INTO accounts (username, user_id, password_hash, created_at) VALUES (?, ?, ?, 0)",
            )
            .bind(format!("login-{id}"))
            .bind(id.to_string())
            .bind(password_hash())
            .execute(self.db.pool())
            .await
            .unwrap();
        }
        for (org, role) in orgs {
            sqlx::query(
                "INSERT INTO memberships (user_id, org_id, role, created_at) VALUES (?, ?, ?, 0)",
            )
            .bind(id.to_string())
            .bind(org.to_string())
            .bind(role.as_str())
            .execute(self.db.pool())
            .await
            .unwrap();
        }
        id
    }

    /// A browser signed in as `email` (password `PASSWORD`).
    pub async fn signed_in(&self, email: &str) -> Browser {
        let mut b = Browser::new();
        let r = self
            .call(
                &mut b,
                Method::POST,
                "/api/v1/auth/password",
                Some(serde_json::json!({ "email": email, "password": PASSWORD })),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "sign in {email}: {}", r.text);
        b
    }

    /// A same-origin JSON request from `b`, keeping its cookies, shaped like the web
    /// app's: non-GET sends `Content-Type: application/json`, and POST/PUT/PATCH always
    /// carry a body (`{}` when none is given).
    pub async fn call(
        &self,
        b: &mut Browser,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Resp {
        let mut req = Request::builder().method(method.clone()).uri(path);
        if method != Method::GET {
            req = req
                .header(header::ORIGIN, ORIGIN)
                .header(header::CONTENT_TYPE, "application/json");
        }
        let body = match method {
            Method::POST | Method::PUT | Method::PATCH => {
                Some(body.unwrap_or(Value::Object(Default::default())))
            }
            _ => body,
        };
        let req = req
            .body(match body {
                Some(v) => Body::from(serde_json::to_vec(&v).unwrap()),
                None => Body::empty(),
            })
            .unwrap();
        self.send(b, req).await
    }

    /// Send a request built by the test, with `b`'s cookies and address.
    pub async fn send(&self, b: &mut Browser, mut req: Request<Body>) -> Resp {
        if let Some(c) = b.cookie_header() {
            req.headers_mut().insert(header::COOKIE, c.parse().unwrap());
        }
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(b.ip, 40000)));
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        b.absorb(&headers);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let json = serde_json::from_str(&text).unwrap_or(Value::Null);
        Resp {
            status,
            headers,
            text,
            json,
        }
    }

    /// Load the database into the broker's authenticator (CAs, tokens, accounts), as the
    /// server's reload loop does.
    pub async fn reload(&self) {
        let mut seen = rconn_signaling::reload::Seen::default();
        rconn_signaling::reload::reload_once(&self.db, &self.auth, &self.broker, &mut seen)
            .await
            .unwrap();
    }

    /// Serve the broker's `/v1/ws` on localhost; returns its URL.
    pub async fn serve_ws(&self) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/v1/ws", listener.local_addr().unwrap());
        let app = rconn_signaling::router(self.broker.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    /// Audit rows `(org_id, actor_id, event)` after the writer caught up.
    pub async fn audit_rows(&self) -> Vec<(Option<String>, Option<String>, String)> {
        self.audit.flush().await;
        sqlx::query_as("SELECT org_id, actor_id, event FROM audit ORDER BY id")
            .fetch_all(self.db.pool())
            .await
            .unwrap()
    }
}

/// The server identity hosts of [`Portal::with_broker`] bind into their proofs.
pub const BROKER_SERVER_ID: &str = "wss://portal.test/v1/ws";

/// The broker of [`Portal::with_broker`].
pub struct Broker {
    /// `ws://…/v1/ws`.
    pub url: String,
    pub state: AppState,
    pub auth: Arc<FleetAuthenticator>,
    pub master: Arc<MasterKey>,
}

impl Broker {
    /// Load orgs' CAs, tokens and revocations from the database (as the server's reload
    /// loop does every few seconds).
    pub async fn reload(&self, db: &Db) {
        rconn_signaling::reload::reload_once(
            db,
            &self.auth,
            &self.state,
            &mut rconn_signaling::reload::Seen::default(),
        )
        .await
        .unwrap();
    }
}

/// One argon2 hash of `PASSWORD`, shared by every test (hashing is slow in debug builds).
fn password_hash() -> String {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| rconn_signaling::accounts::hash_password(PASSWORD).unwrap())
        .clone()
}

/// The magic link's token and code from a captured sign-in email.
pub fn token_and_code(text: &str) -> (String, String) {
    let token = text
        .split("token=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let code: String = text
        .split("enter this code: ")
        .nth(1)
        .unwrap()
        .chars()
        .take(7)
        .filter(|c| c.is_ascii_digit())
        .collect();
    assert_eq!(code.len(), 6);
    (token, code)
}
