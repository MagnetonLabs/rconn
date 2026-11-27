//! Groups, session policies and the audit log through the portal API, and the effective
//! policy applied when a session to an org computer is requested over `/v1/ws`.

mod portal_support;

use axum::http::{Method, StatusCode};
use futures_util::{SinkExt, StreamExt};
use portal_support::Portal;
use rconn_core::PROTOCOL_VERSION;
use rconn_core::fleet::{FleetCa, HostKey, host_proof_transcript};
use rconn_core::ids::{HostId, OperatorId, OrgId};
use rconn_core::protocol::{
    CHALLENGE_LEN, ClientMessage, ErrorCode, HOST_CAPABILITIES, ServerMessage,
};
use rconn_signaling::audit_db::DbAudit;
use rconn_signaling::auth::OperatorIdentity;
use rconn_signaling::db::{Db, Role};
use rconn_signaling::fleet_auth::FleetAuthenticator;
use rconn_signaling::portal::policies::{AllowedHours, Day, DbPolicies, Policy};
use rconn_signaling::{AppState, router};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

// -- groups and policies over the API --------------------------------------------------

struct World {
    p: Portal,
    a: OrgId,
    b: OrgId,
}

async fn world() -> World {
    let p = Portal::new().await;
    let a = p.org("Acme").await;
    let b = p.org("Beta").await;
    p.user("admin@a.test", true, &[(a, Role::Admin)]).await;
    p.user("viewer@a.test", true, &[(a, Role::Viewer)]).await;
    p.user("owner@b.test", true, &[(b, Role::Owner)]).await;
    World { p, a, b }
}

async fn add_host(db: &Db, org: OrgId, group: Option<&str>, name: &str) -> HostId {
    let id = HostId::new();
    sqlx::query("INSERT INTO hosts (id, org_id, hostname, group_id) VALUES (?, ?, ?, ?)")
        .bind(id.to_string())
        .bind(org.to_string())
        .bind(name)
        .bind(group)
        .execute(db.pool())
        .await
        .unwrap();
    id
}

fn office_hours() -> Value {
    json!({
        "clipboard": false, "file_transfer": true, "audio": false, "require_approval": true,
        "show_indicator": true, "allow_access_password": false,
        "allowed_hours": {
            "timezone": "America/Chicago", "days": ["fri", "mon"], "start": "08:00", "end": "18:00"
        }
    })
}

#[tokio::test]
async fn groups_are_created_renamed_and_deleted_and_deleting_ungroups_computers() {
    let w = world().await;
    let mut admin = w.p.signed_in("admin@a.test").await;
    let base = format!("/api/v1/orgs/{}", w.a);

    let r =
        w.p.call(
            &mut admin,
            Method::POST,
            &format!("{base}/groups"),
            Some(json!({ "name": "  Front desk " })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.json["name"], "Front desk");
    assert_eq!(r.json["computers"], 0);
    let gid = r.json["id"].as_str().unwrap().to_owned();

    // Names are unique per company, ignoring case; other companies may reuse them.
    let r =
        w.p.call(
            &mut admin,
            Method::POST,
            &format!("{base}/groups"),
            Some(json!({ "name": "FRONT DESK" })),
        )
        .await;
    assert_eq!((r.status, r.code()), (StatusCode::CONFLICT, "conflict"));
    let r =
        w.p.call(
            &mut admin,
            Method::POST,
            &format!("{base}/groups"),
            Some(json!({ "name": " " })),
        )
        .await;
    assert_eq!((r.status, r.code()), (StatusCode::BAD_REQUEST, "invalid"));
    let mut owner_b = w.p.signed_in("owner@b.test").await;
    let r =
        w.p.call(
            &mut owner_b,
            Method::POST,
            &format!("/api/v1/orgs/{}/groups", w.b),
            Some(json!({ "name": "Front desk" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let b_gid = r.json["id"].as_str().unwrap().to_owned();

    let h1 = add_host(&w.p.db, w.a, Some(&gid), "PC-1").await;
    add_host(&w.p.db, w.a, Some(&gid), "PC-2").await;
    add_host(&w.p.db, w.a, None, "PC-3").await;
    sqlx::query("UPDATE hosts SET revoked_at = 1 WHERE hostname = 'PC-2'")
        .execute(w.p.db.pool())
        .await
        .unwrap();
    let r =
        w.p.call(&mut admin, Method::GET, &format!("{base}/groups"), None)
            .await;
    assert_eq!(
        r.json,
        json!([{ "id": gid, "name": "Front desk", "computers": 1 }]),
        "revoked ones don't count"
    );

    let r =
        w.p.call(
            &mut admin,
            Method::PATCH,
            &format!("{base}/groups/{gid}"),
            Some(json!({ "name": "Reception" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["name"], "Reception");

    // Another company's group, through this company's path: 404, untouched.
    for (m, path, body) in [
        (
            Method::PATCH,
            format!("{base}/groups/{b_gid}"),
            Some(json!({ "name": "Taken" })),
        ),
        (Method::DELETE, format!("{base}/groups/{b_gid}"), None),
        (Method::GET, format!("{base}/groups/{b_gid}/policy"), None),
        (
            Method::PUT,
            format!("{base}/groups/{b_gid}/policy"),
            Some(office_hours()),
        ),
        (
            Method::GET,
            format!("{base}/groups/not-a-uuid/policy"),
            None,
        ),
    ] {
        let r = w.p.call(&mut admin, m.clone(), &path, body).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{m} {path}");
    }

    let r =
        w.p.call(
            &mut admin,
            Method::DELETE,
            &format!("{base}/groups/{gid}"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text);
    let group: Option<String> = sqlx::query_scalar("SELECT group_id FROM hosts WHERE id = ?")
        .bind(h1.to_string())
        .fetch_one(w.p.db.pool())
        .await
        .unwrap();
    assert_eq!(group, None, "its computers are ungrouped");
    let r =
        w.p.call(&mut admin, Method::GET, &format!("{base}/groups"), None)
            .await;
    assert_eq!(r.json, json!([]));
    let r =
        w.p.call(
            &mut owner_b,
            Method::GET,
            &format!("/api/v1/orgs/{}/groups", w.b),
            None,
        )
        .await;
    assert_eq!(r.json.as_array().unwrap().len(), 1, "B keeps its group");

    let rows = w.p.audit_rows().await;
    for kind in ["group.created", "group.renamed", "group.deleted"] {
        assert!(
            rows.iter()
                .any(|r| r.2 == kind && r.0.as_deref() == Some(w.a.to_string().as_str())),
            "{kind}"
        );
    }
}

#[tokio::test]
async fn policies_are_validated_stored_inherited_and_audited() {
    let w = world().await;
    let mut admin = w.p.signed_in("admin@a.test").await;
    let mut viewer = w.p.signed_in("viewer@a.test").await;
    let base = format!("/api/v1/orgs/{}", w.a);

    // New companies start from the portal's default.
    let r =
        w.p.call(
            &mut viewer,
            Method::GET,
            &format!("{base}/policy/default"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json, serde_json::to_value(Policy::default()).unwrap());
    assert_eq!(r.json["show_indicator"], true);

    let r =
        w.p.call(
            &mut admin,
            Method::PUT,
            &format!("{base}/policy/default"),
            Some(office_hours()),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(
        r.json["allowed_hours"]["days"],
        json!(["mon", "fri"]),
        "ordered mon..sun"
    );
    let r =
        w.p.call(
            &mut viewer,
            Method::GET,
            &format!("{base}/policy/default"),
            None,
        )
        .await;
    assert_eq!(r.json["allow_access_password"], false);
    let r =
        w.p.call(
            &mut viewer,
            Method::PUT,
            &format!("{base}/policy/default"),
            Some(office_hours()),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // A new group starts with a copy of the company default.
    let r =
        w.p.call(
            &mut admin,
            Method::POST,
            &format!("{base}/groups"),
            Some(json!({ "name": "Night shift" })),
        )
        .await;
    let gid = r.json["id"].as_str().unwrap().to_owned();
    let gp = format!("{base}/groups/{gid}/policy");
    let r = w.p.call(&mut viewer, Method::GET, &gp, None).await;
    assert_eq!(r.json["allowed_hours"]["timezone"], "America/Chicago");

    // Midnight-crossing window; bad values are 400 invalid and change nothing.
    let mut night = office_hours();
    night["allowed_hours"] =
        json!({ "timezone": "Europe/Berlin", "days": ["sat"], "start": "22:00", "end": "06:00" });
    let r = w.p.call(&mut admin, Method::PUT, &gp, Some(night)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    for bad in [
        json!({ "timezone": "Mars/Base", "days": ["mon"], "start": "08:00", "end": "17:00" }),
        json!({ "timezone": "UTC", "days": [], "start": "08:00", "end": "17:00" }),
        json!({ "timezone": "UTC", "days": ["mon"], "start": "8am", "end": "17:00" }),
        json!({ "timezone": "UTC", "days": ["mon"], "start": "08:00", "end": "08:00" }),
    ] {
        let mut p = office_hours();
        p["allowed_hours"] = bad.clone();
        let r = w.p.call(&mut admin, Method::PUT, &gp, Some(p)).await;
        assert_eq!(
            (r.status, r.code()),
            (StatusCode::BAD_REQUEST, "invalid"),
            "{bad}"
        );
    }
    let r =
        w.p.call(
            &mut admin,
            Method::PUT,
            &gp,
            Some(json!({ "clipboard": true })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = w.p.call(&mut viewer, Method::GET, &gp, None).await;
    assert_eq!(r.json["allowed_hours"]["end"], "06:00");

    // policy.changed carries the whole policy before and after.
    w.p.audit.flush().await;
    let details: Vec<String> = sqlx::query_scalar(
        "SELECT detail FROM audit WHERE org_id = ? AND event = 'policy.changed' ORDER BY id",
    )
    .bind(w.a.to_string())
    .fetch_all(w.p.db.pool())
    .await
    .unwrap();
    assert_eq!(details.len(), 2);
    let last: Value = serde_json::from_str(&details[1]).unwrap();
    assert_eq!(
        last["target"],
        json!({ "type": "group", "id": gid, "label": "Night shift" })
    );
    assert_eq!(
        last["detail"]["before"]["allowed_hours"]["timezone"],
        "America/Chicago"
    );
    assert_eq!(
        last["detail"]["after"]["allowed_hours"]["timezone"],
        "Europe/Berlin"
    );
    assert_eq!(last["detail"]["changed"], json!(["allowed_hours"]));
    let first: Value = serde_json::from_str(&details[0]).unwrap();
    assert_eq!(first["target"]["id"], "default");

    // The audit API shows it the way the web app expects.
    let r =
        w.p.call(
            &mut admin,
            Method::GET,
            &format!("{base}/audit?kind=policy.changed"),
            None,
        )
        .await;
    let item = &r.json["items"][0];
    assert_eq!(item["actor"]["type"], "user");
    assert_eq!(item["actor"]["label"], "admin@a.test");
    assert_eq!(item["target"]["label"], "Night shift");
    assert_eq!(item["ip"], "192.0.2.10");
}

// -- the audit log ---------------------------------------------------------------------

async fn audit_row(
    db: &Db,
    org: OrgId,
    ts_ms: i64,
    actor: Option<&str>,
    event: &str,
    detail: Value,
) {
    sqlx::query(
        "INSERT INTO audit (org_id, ts_ms, actor_id, event, detail) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(org.to_string())
    .bind(ts_ms)
    .bind(actor)
    .bind(event)
    .bind(detail.to_string())
    .execute(db.pool())
    .await
    .unwrap();
}

#[tokio::test]
async fn the_audit_log_pages_filters_and_exports_only_its_company() {
    let w = world().await;
    let mut admin = w.p.signed_in("admin@a.test").await;
    w.p.audit.flush().await;
    sqlx::query("DELETE FROM audit")
        .execute(w.p.db.pool())
        .await
        .unwrap();
    let admin_id: String = sqlx::query_scalar("SELECT id FROM users WHERE email = 'admin@a.test'")
        .fetch_one(w.p.db.pool())
        .await
        .unwrap();
    let pc = add_host(&w.p.db, w.a, None, "RECEPTION-PC").await;
    // 2026-11-27T00:00:00Z, one event a minute.
    let t0 = 1_795_737_600_000i64;
    for i in 0..120 {
        let ts = t0 + i * 60_000;
        if i % 2 == 0 {
            audit_row(&w.p.db, w.a, ts, Some(&admin_id), "session.start",
                json!({ "session_id": format!("s{i}"), "host_id": pc.to_string(), "operator_id": admin_id })).await;
        } else {
            audit_row(&w.p.db, w.a, ts, Some(&admin_id), "session.end",
                json!({ "session_id": format!("s{}", i - 1), "reason": "normal", "host_id": pc.to_string(), "duration_s": 60 })).await;
        }
        audit_row(&w.p.db, w.b, ts, None, "sign_in.failed",
            json!({ "email": "x@b.test", "method": "password", "reason": "bad_password", "ip": "198.51.100.1" })).await;
    }
    audit_row(&w.p.db, w.a, t0 + 200 * 60_000, None, "sign_in.failed",
        json!({ "email": "Mallory@evil.test", "method": "password", "reason": "bad_password", "ip": "203.0.113.9", "orgs": [w.a] })).await;
    audit_row(&w.p.db, w.a, t0 + 201 * 60_000, Some(&admin_id), "computer.renamed",
        json!({ "target": { "type": "computer", "id": pc.to_string(), "label": "=cmd|' /C calc'!A0" }, "ip": "192.0.2.10", "detail": { "from": "a", "to": "b" } })).await;

    let base = format!("/api/v1/orgs/{}/audit", w.a);
    // Default page size 50, newest first, cursor until the end.
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let path = match &cursor {
            Some(c) => format!("{base}?cursor={c}"),
            None => base.clone(),
        };
        let r = w.p.call(&mut admin, Method::GET, &path, None).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        pages += 1;
        for i in r.json["items"].as_array().unwrap() {
            seen.push(i.clone());
        }
        match r.json["next"].as_str() {
            Some(n) => cursor = Some(n.to_owned()),
            None => break,
        }
    }
    assert_eq!((pages, seen.len()), (3, 122), "only A's 122 events");
    assert!(
        seen.windows(2)
            .all(|p| p[0]["at"].as_str() >= p[1]["at"].as_str())
    );
    let renamed = &seen[0];
    assert_eq!(renamed["kind"], "computer.renamed");
    assert_eq!(renamed["target"]["type"], "computer");
    assert_eq!(renamed["detail"], json!({ "from": "a", "to": "b" }));
    let failed = &seen[1];
    assert_eq!(
        failed["actor"],
        json!({ "type": "system", "id": "auth", "label": "Mallory@evil.test" })
    );
    assert_eq!(failed["ip"], "203.0.113.9");
    assert_eq!(
        failed["detail"],
        json!({ "method": "password", "reason": "bad_password" })
    );
    let end = &seen[2];
    assert_eq!(end["kind"], "session.end");
    assert_eq!(end["at"], "2026-11-27T01:59:00Z");
    assert_eq!(end["actor"]["label"], "admin@a.test");
    assert_eq!(
        end["target"],
        json!({ "type": "computer", "id": pc.to_string(), "label": "RECEPTION-PC" })
    );
    assert_eq!(end["detail"]["duration_s"], 60);

    // limit, kind, time range (inclusive), search.
    let get = |q: &str| format!("{base}?{q}");
    let r =
        w.p.call(
            &mut admin,
            Method::GET,
            &get("limit=7&kind=session.start"),
            None,
        )
        .await;
    assert_eq!(r.json["items"].as_array().unwrap().len(), 7);
    assert!(
        r.json["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["kind"] == "session.start")
    );
    let r =
        w.p.call(
            &mut admin,
            Method::GET,
            &get("from=2026-11-27T00:10:00Z&to=2026-11-27T00:19:00.000Z&limit=200"),
            None,
        )
        .await;
    assert_eq!(r.json["items"].as_array().unwrap().len(), 10);
    assert_eq!(r.json["next"], Value::Null);
    let r =
        w.p.call(&mut admin, Method::GET, &get("q=mallory"), None)
            .await;
    assert_eq!(
        r.json["items"].as_array().unwrap().len(),
        1,
        "case-insensitive"
    );
    let r =
        w.p.call(
            &mut admin,
            Method::GET,
            &get("q=reception-pc&limit=200"),
            None,
        )
        .await;
    assert_eq!(
        r.json["items"].as_array().unwrap().len(),
        120,
        "by computer name"
    );
    let r = w.p.call(&mut admin, Method::GET, &get("q=%25"), None).await;
    assert_eq!(r.json["items"].as_array().unwrap().len(), 0, "% is literal");
    for bad in ["from=yesterday", "cursor=bogus", "limit=many"] {
        let r = w.p.call(&mut admin, Method::GET, &get(bad), None).await;
        assert_eq!(
            (r.status, r.code()),
            (StatusCode::BAD_REQUEST, "invalid"),
            "{bad}"
        );
    }

    // CSV: header, every match, formulas defused.
    let r =
        w.p.call(
            &mut admin,
            Method::GET,
            &format!("{base}.csv?kind=computer.renamed"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/csv")
    );
    let lines: Vec<&str> = r.text.lines().collect();
    assert_eq!(
        lines[0],
        "at,kind,actor_type,actor,target_type,target,ip,detail"
    );
    assert_eq!(lines.len(), 2);
    assert!(
        lines[1]
            .starts_with("2026-11-27T03:21:00Z,computer.renamed,user,admin@a.test,computer,'=cmd|"),
        "{}",
        lines[1]
    );
    assert!(
        lines[1].ends_with(r#","{""from"":""a"",""to"":""b""}""#),
        "{}",
        lines[1]
    );
    let r =
        w.p.call(&mut admin, Method::GET, &format!("{base}.csv"), None)
            .await;
    assert_eq!(r.text.lines().count(), 123);
    assert!(!r.text.contains("x@b.test"));

    // Another company's log: 404; a viewer: 403.
    for path in [
        format!("/api/v1/orgs/{}/audit", w.b),
        format!("/api/v1/orgs/{}/audit.csv", w.b),
    ] {
        let r = w.p.call(&mut admin, Method::GET, &path, None).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
    }
    let mut viewer = w.p.signed_in("viewer@a.test").await;
    let r =
        w.p.call(&mut viewer, Method::GET, &format!("{base}.csv"), None)
            .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

// -- enforcement over /v1/ws -----------------------------------------------------------

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
const SID: &str = "wss://policies.test";

async fn send(ws: &mut Ws, m: &ClientMessage) {
    ws.send(Message::Text(m.to_json().unwrap().into()))
        .await
        .unwrap();
}

async fn recv(ws: &mut Ws) -> ServerMessage {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timeout")
        {
            Some(Ok(Message::Text(t))) => return ServerMessage::from_json(&t).unwrap(),
            Some(Ok(_)) => {}
            other => panic!("socket closed: {other:?}"),
        }
    }
}

struct Server {
    url: String,
    db: Db,
    org: OrgId,
    ca: FleetCa,
    audit: Arc<DbAudit>,
    _dir: tempfile::TempDir,
}

async fn server() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("rconn.db")).await.unwrap();
    let org = OrgId::new();
    let ca = FleetCa::generate(org).unwrap();
    sqlx::query("INSERT INTO orgs (id, name, created_at, ca_cert_pem) VALUES (?, 'Acme', 0, ?)")
        .bind(org.to_string())
        .bind(ca.cert_pem())
        .execute(db.pool())
        .await
        .unwrap();
    let op = OperatorIdentity {
        operator_id: OperatorId::new(),
        org_id: org,
    };
    let auth = FleetAuthenticator::new(SID)
        .with_org_ca(org, ca.cert_pem())
        .with_operator_token("op-token", op);
    let audit = Arc::new(DbAudit::spawn(db.clone()));
    let state = AppState::with_audit(Arc::new(auth), audit.clone());
    state.use_policies(Arc::new(DbPolicies::new(db.clone())));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/v1/ws", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    Server {
        url,
        db,
        org,
        ca,
        audit,
        _dir: dir,
    }
}

async fn host(s: &Server, caps: &[&str]) -> (HostId, Ws) {
    let id = HostId::new();
    let key = HostKey::generate().unwrap();
    let cert = s.ca.issue_host_cert(id, &key).unwrap();
    let cn = vec![7u8; CHALLENGE_LEN];
    let (mut ws, _) = connect_async(&s.url).await.unwrap();
    send(
        &mut ws,
        &ClientMessage::HostHello {
            protocol_version: PROTOCOL_VERSION,
            host_id: id,
            cert_pem: cert.clone(),
            client_nonce: cn.clone(),
            label: Some("BOX".into()),
            os: None,
            version: None,
            capabilities: caps.iter().map(|c| (*c).into()).collect(),
        },
    )
    .await;
    let ServerMessage::Challenge { nonce } = recv(&mut ws).await else {
        panic!("no challenge")
    };
    let signature = key
        .sign(&host_proof_transcript(SID, id, &cert, &cn, &nonce))
        .unwrap();
    send(&mut ws, &ClientMessage::HostProof { signature }).await;
    assert!(matches!(recv(&mut ws).await, ServerMessage::Welcome { .. }));
    (id, ws)
}

async fn operator(s: &Server) -> Ws {
    let (mut ws, _) = connect_async(&s.url).await.unwrap();
    send(
        &mut ws,
        &ClientMessage::OperatorHello {
            protocol_version: PROTOCOL_VERSION,
            token: "op-token".into(),
        },
    )
    .await;
    assert!(matches!(recv(&mut ws).await, ServerMessage::Welcome { .. }));
    ws
}

async fn guest(s: &Server, host: HostId) -> Ws {
    let (mut ws, _) = connect_async(&s.url).await.unwrap();
    send(
        &mut ws,
        &ClientMessage::GuestHello {
            protocol_version: PROTOCOL_VERSION,
            host: host.short_code(),
        },
    )
    .await;
    assert!(matches!(recv(&mut ws).await, ServerMessage::Welcome { .. }));
    assert!(matches!(
        recv(&mut ws).await,
        ServerMessage::GuestHost { .. }
    ));
    ws
}

async fn group_with(db: &Db, org: OrgId, p: &Policy) -> String {
    let gid = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO host_groups (id, org_id, name, created_at) VALUES (?, ?, ?, 0)")
        .bind(&gid)
        .bind(org.to_string())
        .bind(&gid)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("INSERT INTO host_group_policies (org_id, group_id, policy, updated_at) VALUES (?, ?, ?, 0)")
        .bind(org.to_string())
        .bind(&gid)
        .bind(serde_json::to_string(p).unwrap())
        .execute(db.pool())
        .await
        .unwrap();
    gid
}

async fn put_in(db: &Db, org: OrgId, host: HostId, group: &str) {
    sqlx::query(
        "INSERT INTO hosts (id, org_id, group_id) VALUES (?, ?, ?)
                 ON CONFLICT (id) DO UPDATE SET group_id = excluded.group_id",
    )
    .bind(host.to_string())
    .bind(org.to_string())
    .bind(group)
    .execute(db.pool())
    .await
    .unwrap();
}

/// Every day but today (UTC): sessions are never allowed now.
fn closed_today() -> AllowedHours {
    let today = jiff::Timestamp::now()
        .to_zoned_datetime(jiff::tz::TimeZone::UTC)
        .weekday();
    let days = Day::ALL
        .into_iter()
        .enumerate()
        .filter(|(i, _)| *i as i8 != today.to_monday_zero_offset())
        .map(|(_, d)| d)
        .collect();
    AllowedHours {
        timezone: "UTC".into(),
        days,
        start: "00:00".into(),
        end: "23:59".into(),
    }
}

#[tokio::test]
async fn the_effective_policy_reaches_the_host() {
    let s = server().await;
    let (id, mut h) = host(&s, HOST_CAPABILITIES).await;
    // Ungrouped: the company default (none stored: the portal's default).
    let mut op = operator(&s).await;
    send(&mut op, &ClientMessage::RequestSession { host_id: id }).await;
    let ServerMessage::IncomingSession { policy, .. } = recv(&mut h).await else {
        panic!("no session")
    };
    assert!(policy.show_indicator && policy.audio && policy.clipboard && !policy.require_approval);
    assert!(matches!(
        recv(&mut op).await,
        ServerMessage::SessionReady { .. }
    ));

    // In a group: its policy, around the clock (window crossing midnight every day).
    let strict = Policy {
        clipboard: false,
        file_transfer: false,
        audio: false,
        require_approval: true,
        show_indicator: true,
        allow_access_password: true,
        allowed_hours: Some(AllowedHours {
            timezone: "America/Chicago".into(),
            days: Day::ALL.to_vec(),
            start: "00:01".into(),
            end: "00:00".into(),
        }),
    };
    let gid = group_with(&s.db, s.org, &strict).await;
    put_in(&s.db, s.org, id, &gid).await;
    send(&mut op, &ClientMessage::RequestSession { host_id: id }).await;
    let ServerMessage::IncomingSession { policy, .. } = recv(&mut h).await else {
        panic!("no session")
    };
    assert!(!policy.clipboard && !policy.file_transfer && !policy.audio && !policy.guest);
    assert!(policy.require_approval && policy.show_indicator);
}

#[tokio::test]
async fn sessions_outside_the_hours_or_needing_what_old_hosts_lack_are_refused() {
    let s = server().await;
    let (new_id, mut new_host) = host(&s, HOST_CAPABILITIES).await;
    let (old_id, _old_host) = host(&s, &[]).await;
    let closed = Policy {
        allowed_hours: Some(closed_today()),
        ..Policy::default()
    };
    let gid = group_with(&s.db, s.org, &closed).await;
    put_in(&s.db, s.org, new_id, &gid).await;

    let mut op = operator(&s).await;
    send(&mut op, &ClientMessage::RequestSession { host_id: new_id }).await;
    let ServerMessage::Error { code, message } = recv(&mut op).await else {
        panic!("not refused")
    };
    assert_eq!(code, ErrorCode::Forbidden);
    assert!(
        message.contains("outside the hours") && message.contains("(UTC)"),
        "{message}"
    );

    // Old hosts can't keep sound off: refused (fails closed), with a reason to update.
    let quiet = Policy {
        audio: false,
        ..Policy::default()
    };
    let qgid = group_with(&s.db, s.org, &quiet).await;
    put_in(&s.db, s.org, old_id, &qgid).await;
    send(&mut op, &ClientMessage::RequestSession { host_id: old_id }).await;
    let ServerMessage::Error { code, message } = recv(&mut op).await else {
        panic!("not refused")
    };
    assert_eq!(code, ErrorCode::Forbidden);
    assert!(message.contains("too old"), "{message}");
    // A current host in that group gets it.
    put_in(&s.db, s.org, new_id, &qgid).await;
    send(&mut op, &ClientMessage::RequestSession { host_id: new_id }).await;
    let ServerMessage::IncomingSession { policy, .. } = recv(&mut new_host).await else {
        panic!("no session")
    };
    assert!(!policy.audio);

    // ID + access password turned off: guests are refused, members still connect.
    let no_pw = Policy {
        allow_access_password: false,
        ..Policy::default()
    };
    let pgid = group_with(&s.db, s.org, &no_pw).await;
    put_in(&s.db, s.org, new_id, &pgid).await;
    let mut g = guest(&s, new_id).await;
    send(&mut g, &ClientMessage::RequestSession { host_id: new_id }).await;
    let ServerMessage::Error { code, message } = recv(&mut g).await else {
        panic!("not refused")
    };
    assert_eq!(code, ErrorCode::Forbidden);
    assert!(message.contains("access password"), "{message}");
    send(&mut op, &ClientMessage::RequestSession { host_id: new_id }).await;
    assert!(matches!(
        recv(&mut new_host).await,
        ServerMessage::IncomingSession { .. }
    ));

    // Every refusal is in the company's log.
    s.audit.flush().await;
    let refused: Vec<String> = sqlx::query_scalar(
        "SELECT detail FROM audit WHERE org_id = ? AND event = 'session.refused'",
    )
    .bind(s.org.to_string())
    .fetch_all(s.db.pool())
    .await
    .unwrap();
    assert_eq!(refused.len(), 3, "{refused:?}");
}
