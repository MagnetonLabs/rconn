//! The portal's Computers area against a live broker: listing with live online state,
//! rename/move inside the org only, revoke (evicts now, refuses reconnects), enrollment
//! tokens, and computers joining an org with one.

use crate::portal_support;

use axum::http::{Method, StatusCode};
use futures_util::{SinkExt, StreamExt};
use portal_support::{BROKER_SERVER_ID, Broker, Browser, Portal};
use rconn_core::PROTOCOL_VERSION;
use rconn_core::fleet::{self, FleetCa, HostKey, host_proof_transcript};
use rconn_core::ids::{HostId, OrgId};
use rconn_core::protocol::{
    CHALLENGE_LEN, ClientMessage, EndReason, ErrorCode, HOST_CAPABILITIES, ServerMessage,
};
use rconn_signaling::db::Role;
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

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

/// Connect as host `id` with `cert`; the server's verdict (Welcome or Error).
async fn connect_host(
    url: &str,
    id: HostId,
    key: &HostKey,
    cert: &str,
    version: Option<&str>,
) -> (Ws, Option<ServerMessage>) {
    let (mut ws, _) = connect_async(url).await.unwrap();
    let cn = vec![5u8; CHALLENGE_LEN];
    send(
        &mut ws,
        &ClientMessage::HostHello {
            protocol_version: PROTOCOL_VERSION,
            host_id: id,
            cert_pem: cert.into(),
            client_nonce: cn.clone(),
            label: Some(format!("HOST-{}", &id.to_string()[..4])),
            os: version.map(|_| "linux ubuntu 24.04".into()),
            version: version.map(Into::into),
            capabilities: HOST_CAPABILITIES.iter().map(|c| (*c).into()).collect(),
        },
    )
    .await;
    let Some(ServerMessage::Challenge { nonce }) = recv(&mut ws).await else {
        panic!("no challenge")
    };
    let sig = key
        .sign(&host_proof_transcript(
            BROKER_SERVER_ID,
            id,
            cert,
            &cn,
            &nonce,
        ))
        .unwrap();
    send(&mut ws, &ClientMessage::HostProof { signature: sig }).await;
    let verdict = recv(&mut ws).await;
    (ws, verdict)
}

/// A host of `ca`'s org online now (its inventory row written).
async fn live_host(w: &World, ca: &FleetCa, version: &str) -> (HostId, HostKey, String, Ws) {
    let id = HostId::new();
    let key = HostKey::generate().unwrap();
    let cert = ca.issue_host_cert(id, &key).unwrap();
    let (ws, verdict) = connect_host(&w.b.url, id, &key, &cert, Some(version)).await;
    assert!(
        matches!(verdict, Some(ServerMessage::Welcome { .. })),
        "{verdict:?}"
    );
    wait_until(|| async {
        w.p.db
            .host(ca.org_id(), id)
            .await
            .unwrap()
            .is_some_and(|r| r.version.as_deref() == Some(version))
    })
    .await;
    (id, key, cert, ws)
}

async fn wait_until<F: std::future::Future<Output = bool>>(f: impl Fn() -> F) {
    for _ in 0..250 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition never held");
}

/// Ask for an id and certificate with an enrollment token.
async fn enroll(url: &str, token: &str, key: &HostKey) -> Option<ServerMessage> {
    let (mut ws, _) = connect_async(url).await.unwrap();
    send(
        &mut ws,
        &ClientMessage::EnrollWithToken {
            protocol_version: PROTOCOL_VERSION,
            token: token.into(),
            public_key_der: key.public_der(),
        },
    )
    .await;
    let answer = recv(&mut ws).await;
    assert!(recv(&mut ws).await.is_none(), "the socket closes after");
    answer
}

struct World {
    p: Portal,
    b: Broker,
    a: OrgId,
    a_ca: FleetCa,
    other: OrgId,
    other_ca: FleetCa,
}

async fn world() -> World {
    let (p, b) = Portal::with_broker().await;
    let (a, a_ca) = p.org_with_key("Acme", &b.master).await;
    let (other, other_ca) = p.org_with_key("Beta", &b.master).await;
    for (email, role) in [
        ("admin@a.test", Role::Admin),
        ("tech@a.test", Role::Technician),
        ("viewer@a.test", Role::Viewer),
    ] {
        p.user(email, true, &[(a, role)]).await;
    }
    p.user("admin@b.test", true, &[(other, Role::Admin)]).await;
    b.reload(&p.db).await;
    World {
        p,
        b,
        a,
        a_ca,
        other,
        other_ca,
    }
}

impl World {
    async fn group(&self, org: OrgId, name: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO host_groups (id, org_id, name, created_at) VALUES (?, ?, ?, 0)")
            .bind(&id)
            .bind(org.to_string())
            .bind(name)
            .execute(self.p.db.pool())
            .await
            .unwrap();
        id
    }

    /// An inventory row written directly (a computer that isn't connected).
    async fn row(&self, org: OrgId, sql_set: &str) -> HostId {
        let id = HostId::new();
        sqlx::query("INSERT INTO hosts (id, org_id, enrolled_at) VALUES (?, ?, 1795773296)")
            .bind(id.to_string())
            .bind(org.to_string())
            .execute(self.p.db.pool())
            .await
            .unwrap();
        if !sql_set.is_empty() {
            // Test-only SQL from literals in this file.
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE hosts SET {sql_set} WHERE id = ?"
            )))
            .bind(id.to_string())
            .execute(self.p.db.pool())
            .await
            .unwrap();
        }
        id
    }

    fn path(&self, org: OrgId, rest: &str) -> String {
        format!("/api/v1/orgs/{org}{rest}")
    }

    async fn list(&self, b: &mut Browser, query: &str) -> Vec<Value> {
        let r = self
            .p
            .call(
                b,
                Method::GET,
                &self.path(self.a, &format!("/computers{query}")),
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{query}: {}", r.text);
        r.json.as_array().unwrap().clone()
    }
}

fn ids(list: &[Value]) -> Vec<String> {
    let mut v: Vec<String> = list
        .iter()
        .map(|c| c["id"].as_str().unwrap().into())
        .collect();
    v.sort();
    v
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

#[tokio::test]
async fn computers_list_with_live_state_filters_and_unknown_facts() {
    let w = world().await;
    let g = w.group(w.a, "Front desk").await;
    let release = env!("CARGO_PKG_VERSION");
    let (live, _, _, _ws) = live_host(&w, &w.a_ca, release).await;
    let (old, _, _, _ws2) = live_host(&w, &w.a_ca, "0.1.3").await;
    // Seen once, from before hosts reported a version; and one never seen at all.
    let ancient = w
        .row(w.a, "hostname = 'OLD-PC', last_seen = 1795773000")
        .await;
    let fresh = w.row(w.a, &format!("group_id = '{g}'")).await;
    let revoked = w
        .row(
            w.a,
            "hostname = 'GONE', version = '0.1.1', revoked_at = 1795773100",
        )
        .await;
    let (_theirs, _, _, _ws3) = live_host(&w, &w.other_ca, release).await;

    let mut viewer = w.p.signed_in("viewer@a.test").await;
    let all = w.list(&mut viewer, "").await;
    assert_eq!(
        ids(&all),
        sorted(
            [live, old, ancient, fresh, revoked]
                .iter()
                .map(|h| h.to_string())
                .collect()
        ),
        "only the org's own computers"
    );
    let by = |id: HostId| {
        all.iter()
            .find(|c| c["id"] == id.to_string())
            .unwrap()
            .clone()
    };
    let c = by(live);
    assert_eq!(c["online"], true);
    assert_eq!(c["needs_update"], false);
    assert_eq!(c["os"], "linux ubuntu 24.04");
    assert_eq!(c["short_code"], live.short_code());
    assert_eq!(c["name"], c["hostname"]);
    assert!(c["last_seen"].is_string() && c["enrolled_at"].is_null());
    assert_eq!(by(old)["needs_update"], true);
    let c = by(ancient);
    assert_eq!(
        (&c["online"], &c["needs_update"]),
        (&json!(false), &json!(true))
    );
    assert!(c["os"].is_null() && c["version"].is_null(), "unknown: {c}");
    let c = by(fresh);
    assert_eq!(c["name"], format!("Computer {}", fresh.short_code()));
    assert_eq!(c["needs_update"], false, "never connected yet");
    assert_eq!(c["group_id"], g);
    assert_eq!(c["enrolled_at"], "2026-11-27T09:54:56Z");
    let c = by(revoked);
    assert_eq!(
        (&c["online"], &c["needs_update"]),
        (&json!(false), &json!(false))
    );
    assert!(c["revoked_at"].is_string());

    let q = |s: &str| format!("?{s}");
    assert_eq!(
        ids(&w.list(&mut viewer, &q("status=online")).await),
        sorted(vec![live.to_string(), old.to_string()])
    );
    assert_eq!(
        ids(&w.list(&mut viewer, &q("status=offline")).await),
        sorted(vec![ancient.to_string(), fresh.to_string()]),
        "offline leaves revoked computers out"
    );
    assert_eq!(
        ids(&w.list(&mut viewer, &q("status=revoked")).await),
        vec![revoked.to_string()]
    );
    assert_eq!(
        ids(&w.list(&mut viewer, &q(&format!("group_id={g}"))).await),
        vec![fresh.to_string()]
    );
    assert_eq!(
        ids(&w.list(&mut viewer, &q("q=old-p")).await),
        vec![ancient.to_string()]
    );
    let code = live.short_code();
    let spaced = code.replace(' ', "%20");
    assert!(
        ids(&w.list(&mut viewer, &q(&format!("q={spaced}"))).await).contains(&live.to_string())
    );
    assert!(
        w.list(&mut viewer, &q("q=nothing-like-this"))
            .await
            .is_empty()
    );
    let r =
        w.p.call(
            &mut viewer,
            Method::GET,
            &w.path(w.a, "/computers?status=asleep"),
            None,
        )
        .await;
    assert_eq!((r.status, r.code()), (StatusCode::BAD_REQUEST, "invalid"));
}

#[tokio::test]
async fn rename_and_move_stay_inside_the_org() {
    let w = world().await;
    let mine = w.group(w.a, "Ops").await;
    let theirs = w.group(w.other, "Theirs").await;
    let pc = w.row(w.a, "hostname = 'OP-01'").await;
    let their_pc = w.row(w.other, "hostname = 'B-01'").await;
    let mut admin = w.p.signed_in("admin@a.test").await;
    let patch = |id: HostId| w.path(w.a, &format!("/computers/{id}"));

    let r =
        w.p.call(
            &mut admin,
            Method::PATCH,
            &patch(pc),
            Some(json!({ "name": "  Operatory 1 " })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(
        (&r.json["name"], &r.json["hostname"]),
        (&json!("Operatory 1"), &json!("OP-01"))
    );
    let r =
        w.p.call(
            &mut admin,
            Method::PATCH,
            &patch(pc),
            Some(json!({ "group_id": mine })),
        )
        .await;
    assert_eq!(r.json["group_id"], mine);
    assert_eq!(r.json["name"], "Operatory 1", "a move keeps the name");

    // Another org's group or computer: 404, and nothing changes.
    let r =
        w.p.call(
            &mut admin,
            Method::PATCH,
            &patch(pc),
            Some(json!({ "group_id": theirs })),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r =
        w.p.call(
            &mut admin,
            Method::PATCH,
            &patch(their_pc),
            Some(json!({ "name": "Mine now" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r =
        w.p.call(
            &mut admin,
            Method::POST,
            &format!("{}/revoke", patch(their_pc)),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let row = w.p.db.host(w.other, their_pc).await.unwrap().unwrap();
    assert!(row.admin_name.is_none() && row.revoked_at.is_none());
    assert_eq!(
        w.p.db
            .host(w.a, pc)
            .await
            .unwrap()
            .unwrap()
            .group_id
            .as_deref(),
        Some(mine.as_str())
    );

    // Bad names; ungroup; clearing the name shows the hostname again.
    for bad in [json!({ "name": "" }), json!({ "name": "x".repeat(65) })] {
        let r =
            w.p.call(&mut admin, Method::PATCH, &patch(pc), Some(bad))
                .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
    }
    let r =
        w.p.call(
            &mut admin,
            Method::PATCH,
            &patch(pc),
            Some(json!({ "name": null, "group_id": null })),
        )
        .await;
    assert_eq!(
        (&r.json["name"], &r.json["group_id"]),
        (&json!("OP-01"), &Value::Null)
    );

    // Technicians can't; revoked computers can't be changed.
    let mut tech = w.p.signed_in("tech@a.test").await;
    let r =
        w.p.call(
            &mut tech,
            Method::PATCH,
            &patch(pc),
            Some(json!({ "name": "x" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let gone = w.row(w.a, "revoked_at = 1").await;
    let r =
        w.p.call(
            &mut admin,
            Method::PATCH,
            &patch(gone),
            Some(json!({ "name": "x" })),
        )
        .await;
    assert_eq!((r.status, r.code()), (StatusCode::CONFLICT, "conflict"));

    let rows = w.p.audit_rows().await;
    let kinds: Vec<&str> = rows.iter().map(|r| r.2.as_str()).collect();
    assert_eq!(
        kinds.iter().filter(|k| **k == "computer.renamed").count(),
        2,
        "{kinds:?}"
    );
    assert_eq!(
        kinds.iter().filter(|k| **k == "computer.moved").count(),
        2,
        "{kinds:?}"
    );
    // Moves name the groups (the audit page and CSV show them), not just their ids.
    let moved: Vec<String> =
        sqlx::query_scalar("SELECT detail FROM audit WHERE event = 'computer.moved' ORDER BY id")
            .fetch_all(w.p.db.pool())
            .await
            .unwrap();
    let moved: Vec<Value> = moved
        .iter()
        .map(|d| serde_json::from_str::<Value>(d).unwrap()["detail"].clone())
        .collect();
    assert_eq!(
        moved,
        vec![
            json!({ "from": null, "to": "Ops", "from_id": null, "to_id": mine }),
            json!({ "from": "Ops", "to": null, "from_id": mine, "to_id": null }),
        ]
    );
}

#[tokio::test]
async fn revoking_evicts_a_live_host_ends_its_sessions_and_blocks_reconnect() {
    let w = world().await;
    let (id, key, cert, mut host_ws) = live_host(&w, &w.a_ca, "0.1.14").await;
    assert_eq!(w.b.state.online_hosts().await, 1);

    // An operator of the org has a session open to it.
    let (_, token) = w.p.db.add_operator(w.a).await.unwrap();
    w.b.reload(&w.p.db).await;
    let (mut op, _) = connect_async(&w.b.url).await.unwrap();
    send(
        &mut op,
        &ClientMessage::OperatorHello {
            protocol_version: PROTOCOL_VERSION,
            token,
        },
    )
    .await;
    assert!(matches!(
        recv(&mut op).await,
        Some(ServerMessage::Welcome { .. })
    ));
    send(&mut op, &ClientMessage::RequestSession { host_id: id }).await;
    let Some(ServerMessage::SessionReady { session_id }) = recv(&mut op).await else {
        panic!("no session")
    };
    assert!(matches!(
        recv(&mut host_ws).await,
        Some(ServerMessage::IncomingSession { .. })
    ));

    let mut admin = w.p.signed_in("admin@a.test").await;
    let revoke = w.path(w.a, &format!("/computers/{id}/revoke"));
    let r = w.p.call(&mut admin, Method::POST, &revoke, None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json["online"], false);
    assert!(r.json["revoked_at"].is_string());

    // At once, without waiting for the reload loop.
    assert_eq!(w.b.state.online_hosts().await, 0);
    assert!(matches!(
        recv(&mut op).await,
        Some(ServerMessage::SessionEnded { session_id: s, reason: EndReason::Revoked }) if s == session_id
    ));
    let mut saw_error = false;
    while let Some(m) = recv(&mut host_ws).await {
        saw_error |= matches!(
            m,
            ServerMessage::Error {
                code: ErrorCode::Forbidden,
                ..
            }
        );
        if saw_error {
            break;
        }
    }
    assert!(saw_error, "the host is told it was revoked");
    // ...and its socket closes, so the host sees it is offline and stops looking online.
    while let Some(m) = recv(&mut host_ws).await {
        assert!(!matches!(m, ServerMessage::IncomingSession { .. }), "{m:?}");
    }
    let (_ws, verdict) = connect_host(&w.b.url, id, &key, &cert, Some("0.1.14")).await;
    assert!(
        matches!(
            verdict,
            Some(ServerMessage::Error {
                code: ErrorCode::Unauthenticated,
                ..
            })
        ),
        "{verdict:?}"
    );
    // Still refused after the reload loop replaces the revocation set from the database.
    w.b.reload(&w.p.db).await;
    let (_ws, verdict) = connect_host(&w.b.url, id, &key, &cert, Some("0.1.14")).await;
    assert!(
        matches!(verdict, Some(ServerMessage::Error { .. })),
        "{verdict:?}"
    );

    // Again: nothing changes, one audit entry.
    let r = w.p.call(&mut admin, Method::POST, &revoke, None).await;
    assert_eq!(r.status, StatusCode::OK);
    let mut tech = w.p.signed_in("tech@a.test").await;
    let r = w.p.call(&mut tech, Method::POST, &revoke, None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let rows = w.p.audit_rows().await;
    let revokes: Vec<_> = rows.iter().filter(|r| r.2 == "computer.revoked").collect();
    assert_eq!(revokes.len(), 1, "{rows:?}");
    assert_eq!(revokes[0].0.as_deref(), Some(w.a.to_string().as_str()));
}

#[tokio::test]
async fn enrollment_tokens_are_shown_once_and_stay_in_their_org() {
    let w = world().await;
    let g = w.group(w.a, "Lab").await;
    let theirs = w.group(w.other, "Theirs").await;
    let mut admin = w.p.signed_in("admin@a.test").await;
    let tokens = w.path(w.a, "/enrollment-tokens");

    let r =
        w.p.call(
            &mut admin,
            Method::POST,
            &tokens,
            Some(json!({ "label": "Lab rollout", "group_id": g, "expires_in_hours": 24, "max_uses": 3 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    let secret = r.json["token"].as_str().unwrap().to_owned();
    assert!(secret.starts_with("rcet_") && secret.len() > 60, "{secret}");
    assert_eq!(r.json["uses"], 0);
    assert_eq!(r.json["max_uses"], 3);
    assert_eq!(r.json["group_id"], g);
    assert_eq!(r.json["created_by"], "admin@a.test");
    let id = r.json["id"].as_str().unwrap().to_owned();
    let stored: String = sqlx::query_scalar("SELECT token_sha256 FROM enrollment_tokens")
        .fetch_one(w.p.db.pool())
        .await
        .unwrap();
    assert!(!stored.contains(&secret[5..]), "only a hash is stored");

    let r = w.p.call(&mut admin, Method::GET, &tokens, None).await;
    let list = r.json.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert!(list[0].get("token").is_none(), "the secret is never listed");
    assert_eq!(list[0]["label"], "Lab rollout");

    for bad in [
        json!({ "label": "x", "expires_in_hours": 2 }),
        json!({ "label": "x", "expires_in_hours": 721 }),
        json!({ "label": "", "expires_in_hours": 1 }),
        json!({ "label": "x", "expires_in_hours": 1, "max_uses": 0 }),
    ] {
        let r =
            w.p.call(&mut admin, Method::POST, &tokens, Some(bad.clone()))
                .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let r =
        w.p.call(
            &mut admin,
            Method::POST,
            &tokens,
            Some(json!({ "label": "x", "group_id": theirs, "expires_in_hours": 1 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "another org's group");

    // The other org neither sees nor deletes it.
    let mut b_admin = w.p.signed_in("admin@b.test").await;
    let r =
        w.p.call(
            &mut b_admin,
            Method::GET,
            &w.path(w.other, "/enrollment-tokens"),
            None,
        )
        .await;
    assert_eq!(r.json, json!([]));
    let r =
        w.p.call(
            &mut b_admin,
            Method::DELETE,
            &w.path(w.other, &format!("/enrollment-tokens/{id}")),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let mut viewer = w.p.signed_in("viewer@a.test").await;
    let r = w.p.call(&mut viewer, Method::GET, &tokens, None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    let one = format!("{tokens}/{id}");
    let r = w.p.call(&mut admin, Method::DELETE, &one, None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = w.p.call(&mut admin, Method::DELETE, &one, None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let key = HostKey::generate().unwrap();
    assert!(
        matches!(
            enroll(&w.b.url, &secret, &key).await,
            Some(ServerMessage::Error {
                code: ErrorCode::Unauthenticated,
                ..
            })
        ),
        "a deleted token enrolls nothing"
    );

    let rows = w.p.audit_rows().await;
    let kinds: Vec<&str> = rows.iter().map(|r| r.2.as_str()).collect();
    assert!(
        kinds.contains(&"token.created") && kinds.contains(&"token.deleted"),
        "{kinds:?}"
    );
}

#[tokio::test]
async fn a_token_enrolls_computers_into_its_org_and_group_until_used_up_or_expired() {
    let w = world().await;
    let g = w.group(w.a, "Lab").await;
    let mut admin = w.p.signed_in("admin@a.test").await;
    let tokens = w.path(w.a, "/enrollment-tokens");
    let r =
        w.p.call(
            &mut admin,
            Method::POST,
            &tokens,
            Some(json!({ "label": "Lab", "group_id": g, "expires_in_hours": 1, "max_uses": 2 })),
        )
        .await;
    let secret = r.json["token"].as_str().unwrap().to_owned();

    // A bad key is refused without using up the token.
    let (mut ws, _) = connect_async(&w.b.url).await.unwrap();
    send(
        &mut ws,
        &ClientMessage::EnrollWithToken {
            protocol_version: PROTOCOL_VERSION,
            token: secret.clone(),
            public_key_der: vec![1, 2, 3],
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

    let mut enrolled = vec![];
    for _ in 0..2 {
        let key = HostKey::generate().unwrap();
        // The file may end with a newline.
        let Some(ServerMessage::HostEnrolled { host_id, cert_pem }) =
            enroll(&w.b.url, &format!("{secret}\n"), &key).await
        else {
            panic!("not enrolled")
        };
        assert_eq!(fleet::claimed_org(&cert_pem).unwrap(), w.a);
        enrolled.push((host_id, key, cert_pem));
    }
    let key = HostKey::generate().unwrap();
    assert!(
        matches!(
            enroll(&w.b.url, &secret, &key).await,
            Some(ServerMessage::Error {
                code: ErrorCode::Unauthenticated,
                ..
            })
        ),
        "used up"
    );
    let r = w.p.call(&mut admin, Method::GET, &tokens, None).await;
    assert_eq!(r.json[0]["uses"], 2);

    // The new computer is in the org and group, and connects with its certificate.
    let (id, key, cert) = &enrolled[0];
    let (_ws, verdict) = connect_host(&w.b.url, *id, key, cert, Some("0.1.14")).await;
    assert!(
        matches!(verdict, Some(ServerMessage::Welcome { org_id, .. }) if org_id == w.a),
        "{verdict:?}"
    );
    let list = w.list(&mut admin, "").await;
    let c = list.iter().find(|c| c["id"] == id.to_string()).unwrap();
    assert_eq!(c["group_id"], g);
    assert_eq!(c["enrolled_by"], "admin@a.test");
    assert!(c["enrolled_at"].is_string());
    assert_eq!(list.len(), 2);

    // Expired tokens enroll nothing.
    let r =
        w.p.call(
            &mut admin,
            Method::POST,
            &tokens,
            Some(json!({ "label": "Old", "expires_in_hours": 1 })),
        )
        .await;
    let old = r.json["token"].as_str().unwrap().to_owned();
    sqlx::query("UPDATE enrollment_tokens SET expires_at = 1 WHERE label = 'Old'")
        .execute(w.p.db.pool())
        .await
        .unwrap();
    assert!(matches!(
        enroll(&w.b.url, &old, &HostKey::generate().unwrap()).await,
        Some(ServerMessage::Error {
            code: ErrorCode::Unauthenticated,
            ..
        })
    ));
    assert!(matches!(
        enroll(&w.b.url, "rcet_made-up", &HostKey::generate().unwrap()).await,
        Some(ServerMessage::Error {
            code: ErrorCode::Unauthenticated,
            ..
        })
    ));
    assert_eq!(
        w.p.db.host_count().await.unwrap(),
        2,
        "refusals leave no rows"
    );

    let rows = w.p.audit_rows().await;
    let enrolls: Vec<_> = rows.iter().filter(|r| r.2 == "computer.enrolled").collect();
    assert_eq!(enrolls.len(), 2, "{rows:?}");
    assert!(
        enrolls
            .iter()
            .all(|r| r.0.as_deref() == Some(w.a.to_string().as_str()))
    );
    let admin_id: String = sqlx::query_scalar("SELECT id FROM users WHERE email = 'admin@a.test'")
        .fetch_one(w.p.db.pool())
        .await
        .unwrap();
    assert!(
        enrolls
            .iter()
            .all(|r| r.1.as_deref() == Some(admin_id.as_str()))
    );
}
