//! People, roles and invites: the role-change rules, the last owner, removing a person
//! (portal sign-ins and live desktop sockets end), invites, and roles deciding who may
//! list and connect from the desktop app, without breaking 0.1.x logins.

mod portal_support;

use axum::http::{Method, StatusCode};
use futures_util::{SinkExt, StreamExt};
use portal_support::{Browser, PASSWORD, Portal, SERVER_ID, TOKEN_KEY};
use rconn_core::PROTOCOL_VERSION;
use rconn_core::fleet::{FleetCa, HostKey, host_proof_transcript};
use rconn_core::ids::{HostId, OperatorId, OrgId, SessionId};
use rconn_core::protocol::{CHALLENGE_LEN, ClientMessage, EndReason, ErrorCode, ServerMessage};
use rconn_signaling::accounts::SessionTokens;
use rconn_signaling::auth::OperatorIdentity;
use rconn_signaling::db::Role;
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const ROLES: [Role; 4] = [Role::Owner, Role::Admin, Role::Technician, Role::Viewer];

// -- helpers -------------------------------------------------------------------------

/// An org whose CA the test keeps (to enroll computers).
async fn org_with_ca(p: &Portal, name: &str, legacy: bool) -> (OrgId, FleetCa) {
    let id = OrgId::new();
    let ca = FleetCa::generate(id).unwrap();
    sqlx::query(
        "INSERT INTO orgs (id, name, legacy, created_at, ca_cert_pem) VALUES (?, ?, ?, 1795773296, ?)",
    )
    .bind(id.to_string())
    .bind(name)
    .bind(i64::from(legacy))
    .bind(ca.cert_pem())
    .execute(p.db.pool())
    .await
    .unwrap();
    (id, ca)
}

/// A desktop operator token for `user` in `org` (stored hashed, like `add-operator`).
async fn static_token(p: &Portal, org: OrgId, user: OperatorId) -> String {
    let token = format!("tok-{}", uuid::Uuid::new_v4());
    let digest = rconn_signaling::portal::session::sha256_hex(&token);
    sqlx::query(
        "INSERT INTO api_tokens (id, org_id, user_id, token_sha256, kind, created_at)
         VALUES (?, ?, ?, ?, 'static', 0)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(org.to_string())
    .bind(user.to_string())
    .bind(digest)
    .execute(p.db.pool())
    .await
    .unwrap();
    token
}

async fn send(ws: &mut Ws, m: &ClientMessage) {
    ws.send(Message::Text(m.to_json().unwrap().into()))
        .await
        .unwrap();
}

/// Next server message, or `None` once the socket closed.
async fn recv(ws: &mut Ws) -> Option<ServerMessage> {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for the server")
        {
            Some(Ok(Message::Text(t))) => return Some(ServerMessage::from_json(&t).unwrap()),
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
            Some(Ok(_)) => {}
        }
    }
}

/// Signs in to the broker with `token`; returns the socket and the server's answer.
async fn operator(url: &str, token: &str) -> (Ws, Option<ServerMessage>) {
    let (mut ws, _) = connect_async(url).await.unwrap();
    send(
        &mut ws,
        &ClientMessage::OperatorHello {
            protocol_version: PROTOCOL_VERSION,
            token: token.into(),
        },
    )
    .await;
    let answer = recv(&mut ws).await;
    (ws, answer)
}

async fn signed_in_operator(url: &str, token: &str) -> Ws {
    let (ws, answer) = operator(url, token).await;
    assert!(
        matches!(answer, Some(ServerMessage::Welcome { .. })),
        "{answer:?}"
    );
    ws
}

/// A computer of `ca`'s org, online.
async fn host_online(url: &str, ca: &FleetCa) -> (Ws, HostId) {
    let id = HostId::new();
    let key = HostKey::generate().unwrap();
    let cert = ca.issue_host_cert(id, &key).unwrap();
    let (mut ws, _) = connect_async(url).await.unwrap();
    let cn = vec![1u8; CHALLENGE_LEN];
    send(
        &mut ws,
        &ClientMessage::HostHello {
            protocol_version: PROTOCOL_VERSION,
            host_id: id,
            cert_pem: cert.clone(),
            client_nonce: cn.clone(),
            label: Some("DESK-1".into()),
            os: None,
            version: None,
            capabilities: vec![],
        },
    )
    .await;
    let Some(ServerMessage::Challenge { nonce }) = recv(&mut ws).await else {
        panic!("no challenge")
    };
    let signature = key
        .sign(&host_proof_transcript(SERVER_ID, id, &cert, &cn, &nonce))
        .unwrap();
    send(&mut ws, &ClientMessage::HostProof { signature }).await;
    assert!(matches!(
        recv(&mut ws).await,
        Some(ServerMessage::Welcome { .. })
    ));
    (ws, id)
}

async fn listed(op: &mut Ws) -> Vec<HostId> {
    send(op, &ClientMessage::ListHosts).await;
    let Some(ServerMessage::Hosts { hosts }) = recv(op).await else {
        panic!("no host list")
    };
    hosts.iter().map(|h| h.host_id).collect()
}

/// Ask for a session; `Ok` with its id once the host was told, or the error code.
async fn request(op: &mut Ws, host: &mut Ws, host_id: HostId) -> Result<SessionId, ErrorCode> {
    send(op, &ClientMessage::RequestSession { host_id }).await;
    match recv(op).await {
        Some(ServerMessage::SessionReady { session_id }) => {
            let Some(ServerMessage::IncomingSession { session_id: s, .. }) = recv(host).await
            else {
                panic!("host not told")
            };
            assert_eq!(s, session_id);
            Ok(session_id)
        }
        Some(ServerMessage::Error { code, .. }) => Err(code),
        other => panic!("unexpected {other:?}"),
    }
}

fn people_path(org: OrgId, rest: &str) -> String {
    format!("/api/v1/orgs/{org}/people{rest}")
}

fn invites_path(org: OrgId, rest: &str) -> String {
    format!("/api/v1/orgs/{org}/invites{rest}")
}

async fn invite(
    p: &Portal,
    b: &mut Browser,
    org: OrgId,
    email: &str,
    role: &str,
) -> (Value, String) {
    let n = p.mail.sent().len() + 1;
    let r = p
        .call(
            b,
            Method::POST,
            &invites_path(org, ""),
            Some(json!({ "email": email, "role": role })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    let mail = p.mail.wait_for(n).await.pop().unwrap();
    assert_eq!(mail.to, email);
    let token = mail
        .text
        .split("/admin/invite?token=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    (r.json, token)
}

async fn accept(p: &Portal, b: &mut Browser, token: &str) -> portal_support::Resp {
    p.call(
        b,
        Method::POST,
        "/api/v1/invites/accept",
        Some(json!({ "token": token })),
    )
    .await
}

// -- people --------------------------------------------------------------------------

#[tokio::test]
async fn role_changes_and_removals_follow_the_rules() {
    let p = Portal::new().await;
    let org = p.org("Acme").await;
    let mut actors = Vec::new();
    for role in ROLES {
        let email = format!("{}@a.test", role.as_str());
        p.user(&email, true, &[(org, role)]).await;
        actors.push((role, p.signed_in(&email).await));
    }
    let mut n = 0;
    for (actor, b) in &mut actors {
        for target in ROLES {
            for new in ROLES {
                n += 1;
                let t = p
                    .user(&format!("t{n}@a.test"), false, &[(org, target)])
                    .await;
                let r = p
                    .call(
                        b,
                        Method::PATCH,
                        &people_path(org, &format!("/{t}")),
                        Some(json!({ "role": new.as_str() })),
                    )
                    .await;
                // Technicians and viewers manage nobody; admins don't touch or make owners.
                let refused = !actor.at_least(Role::Admin)
                    || (*actor == Role::Admin && (target == Role::Owner || new == Role::Owner));
                let expected = if refused {
                    StatusCode::FORBIDDEN
                } else {
                    StatusCode::OK
                };
                assert_eq!(r.status, expected, "{actor:?} sets {target:?} to {new:?}");
                let now = p.db.role(org, t).await.unwrap().unwrap();
                if expected == StatusCode::OK {
                    assert_eq!(r.json["role"], new.as_str());
                    assert_eq!(r.json["user_id"], t.to_string());
                    assert_eq!(now, new);
                } else {
                    assert_eq!(r.code(), "forbidden");
                    assert_eq!(now, target, "a refused change changes nothing");
                }
            }
            n += 1;
            let t = p
                .user(&format!("t{n}@a.test"), false, &[(org, target)])
                .await;
            let r = p
                .call(b, Method::DELETE, &people_path(org, &format!("/{t}")), None)
                .await;
            let allowed =
                actor.at_least(Role::Admin) && (*actor == Role::Owner || target != Role::Owner);
            let expected = if allowed {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::FORBIDDEN
            };
            assert_eq!(r.status, expected, "{actor:?} removes {target:?}");
            assert_eq!(p.db.role(org, t).await.unwrap().is_none(), allowed);
        }
    }
    // Unknown people, bad ids and unknown roles.
    let b = &mut actors[0].1;
    let unknown = people_path(org, &format!("/{}", OperatorId::new()));
    let r = p
        .call(
            b,
            Method::PATCH,
            &unknown,
            Some(json!({ "role": "viewer" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = p
        .call(b, Method::DELETE, &people_path(org, "/nope"), None)
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let t = p.user("odd@a.test", false, &[(org, Role::Viewer)]).await;
    let r = p
        .call(
            b,
            Method::PATCH,
            &people_path(org, &format!("/{t}")),
            Some(json!({ "role": "superuser" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    // Someone in another org is not found here.
    let other = p.org("Beta").await;
    let outsider = p.user("out@b.test", false, &[(other, Role::Viewer)]).await;
    let r = p
        .call(
            b,
            Method::PATCH,
            &people_path(org, &format!("/{outsider}")),
            Some(json!({ "role": "admin" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(
        p.db.role(other, outsider).await.unwrap(),
        Some(Role::Viewer)
    );

    let rows = p.audit_rows().await;
    let changed = rows.iter().filter(|r| r.2 == "person.role_changed").count();
    let removed = rows.iter().filter(|r| r.2 == "person.removed").count();
    // Owner: 16 changes minus the 4 no-ops; admin: 9 minus 3 no-ops.
    assert_eq!(changed, 12 + 6, "{rows:?}");
    assert_eq!(removed, 4 + 3);
}

#[tokio::test]
async fn people_list_shows_every_member_to_every_member() {
    let p = Portal::new().await;
    let org = p.org("Acme").await;
    p.user("owner@a.test", true, &[(org, Role::Owner)]).await;
    let v = p.user("viewer@a.test", true, &[(org, Role::Viewer)]).await;
    let other = p.org("Beta").await;
    p.user("b@b.test", false, &[(other, Role::Owner)]).await;
    let mut b = p.signed_in("viewer@a.test").await;
    let r = p
        .call(&mut b, Method::GET, &people_path(org, ""), None)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let list = r.json.as_array().unwrap();
    assert_eq!(list.len(), 2, "only this org's people: {list:?}");
    let me = list.iter().find(|x| x["user_id"] == v.to_string()).unwrap();
    assert_eq!(me["email"], "viewer@a.test");
    assert_eq!(me["name"], "viewer");
    assert_eq!(me["role"], "viewer");
    assert_eq!(me["joined_at"], "1970-01-01T00:00:00Z");
    assert!(me["last_active"].is_string(), "signed in just now");
    let owner = list.iter().find(|x| x["role"] == "owner").unwrap();
    assert!(owner["last_active"].is_null(), "never signed in");
}

#[tokio::test]
async fn the_last_owner_stays() {
    let p = Portal::new().await;
    let org = p.org("Acme").await;
    let first = p.user("first@a.test", true, &[(org, Role::Owner)]).await;
    p.user("admin@a.test", true, &[(org, Role::Admin)]).await;
    let mut b = p.signed_in("first@a.test").await;
    let me = people_path(org, &format!("/{first}"));
    let r = p
        .call(&mut b, Method::PATCH, &me, Some(json!({ "role": "admin" })))
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.code(), "conflict");
    let r = p.call(&mut b, Method::DELETE, &me, None).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(p.db.role(org, first).await.unwrap(), Some(Role::Owner));
    // An admin trying the same is refused for their role, not for the count.
    let mut admin = p.signed_in("admin@a.test").await;
    let r = p.call(&mut admin, Method::DELETE, &me, None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // With a second owner, the first can step down; then the second is the last.
    let second = p.user("second@a.test", true, &[(org, Role::Owner)]).await;
    let r = p
        .call(&mut b, Method::PATCH, &me, Some(json!({ "role": "admin" })))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let mut b2 = p.signed_in("second@a.test").await;
    let r = p
        .call(
            &mut b2,
            Method::DELETE,
            &people_path(org, &format!("/{second}")),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let owners: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM memberships WHERE org_id = ? AND role = 'owner'")
            .bind(org.to_string())
            .fetch_one(p.db.pool())
            .await
            .unwrap();
    assert_eq!(owners, 1);
}

#[tokio::test]
async fn removing_a_person_ends_their_sign_ins_tokens_and_live_sockets() {
    let p = Portal::new().await;
    let (org, ca) = org_with_ca(&p, "Acme", false).await;
    p.user("owner@a.test", true, &[(org, Role::Owner)]).await;
    let tech = p
        .user("tech@a.test", true, &[(org, Role::Technician)])
        .await;
    let token = static_token(&p, org, tech).await;
    p.reload().await;
    let url = p.serve_ws().await;

    let mut tech_browser = p.signed_in("tech@a.test").await;
    let (mut host, host_id) = host_online(&url, &ca).await;
    let mut op = signed_in_operator(&url, &token).await;
    let mut op2 = signed_in_operator(&url, &token).await;
    assert_eq!(listed(&mut op).await, vec![host_id]);
    let session = request(&mut op, &mut host, host_id).await.unwrap();

    let mut owner = p.signed_in("owner@a.test").await;
    let r = p
        .call(
            &mut owner,
            Method::DELETE,
            &people_path(org, &format!("/{tech}")),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.text);

    // Both desktop sockets are told and closed; the open session ends as revoked.
    for ws in [&mut op, &mut op2] {
        let mut seen = Vec::new();
        while let Some(m) = recv(ws).await {
            seen.push(m);
        }
        assert!(
            seen.iter().any(|m| matches!(
                m,
                ServerMessage::Error {
                    code: ErrorCode::Forbidden,
                    ..
                }
            )),
            "{seen:?}"
        );
    }
    assert_eq!(
        recv(&mut host).await,
        Some(ServerMessage::SessionEnded {
            session_id: session,
            reason: EndReason::Revoked
        })
    );
    // Their portal sign-in is over.
    let r = p
        .call(&mut tech_browser, Method::GET, "/api/v1/me", None)
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // The same token can't sign back in, before or after the server reloads.
    let (_, answer) = operator(&url, &token).await;
    assert!(
        matches!(
            answer,
            Some(ServerMessage::Error {
                code: ErrorCode::Unauthenticated,
                ..
            })
        ),
        "{answer:?}"
    );
    p.reload().await;
    let (_, answer) = operator(&url, &token).await;
    assert!(matches!(
        answer,
        Some(ServerMessage::Error {
            code: ErrorCode::Unauthenticated,
            ..
        })
    ));
    let revoked: Option<i64> =
        sqlx::query_scalar("SELECT revoked_at FROM api_tokens WHERE user_id = ?")
            .bind(tech.to_string())
            .fetch_one(p.db.pool())
            .await
            .unwrap();
    assert!(
        revoked.is_some(),
        "invited back later, the old token stays dead"
    );
    // The computer is still online and fine for others.
    assert_eq!(p.broker.online_hosts().await, 1);
    let rows = p.audit_rows().await;
    assert!(
        rows.iter()
            .any(|r| r.2 == "person.removed" && r.0.as_deref() == Some(org.to_string().as_str())),
        "{rows:?}"
    );
}

// -- desktop authorization -----------------------------------------------------------

#[tokio::test]
async fn viewers_list_but_cannot_connect_and_role_changes_apply_at_once() {
    let p = Portal::new().await;
    let (org, ca) = org_with_ca(&p, "Acme", false).await;
    p.user("owner@a.test", true, &[(org, Role::Owner)]).await;
    let viewer = p.user("viewer@a.test", false, &[(org, Role::Viewer)]).await;
    let tech = p
        .user("tech@a.test", false, &[(org, Role::Technician)])
        .await;
    let (vt, tt) = (
        static_token(&p, org, viewer).await,
        static_token(&p, org, tech).await,
    );
    p.reload().await;
    let url = p.serve_ws().await;
    let (mut host, host_id) = host_online(&url, &ca).await;

    let mut v = signed_in_operator(&url, &vt).await;
    assert_eq!(
        listed(&mut v).await,
        vec![host_id],
        "a viewer sees computers"
    );
    assert_eq!(
        request(&mut v, &mut host, host_id).await,
        Err(ErrorCode::Forbidden),
        "but can't connect"
    );
    let mut t = signed_in_operator(&url, &tt).await;
    assert_eq!(listed(&mut t).await, vec![host_id]);
    let session = request(&mut t, &mut host, host_id).await.unwrap();

    // Demoted to viewer: the open session ends and the next request is refused, with no
    // reload or reconnect.
    let mut owner = p.signed_in("owner@a.test").await;
    let r = p
        .call(
            &mut owner,
            Method::PATCH,
            &people_path(org, &format!("/{tech}")),
            Some(json!({ "role": "viewer" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let ended = Some(ServerMessage::SessionEnded {
        session_id: session,
        reason: EndReason::Revoked,
    });
    assert_eq!(recv(&mut t).await, ended);
    assert_eq!(recv(&mut host).await, ended);
    assert_eq!(
        request(&mut t, &mut host, host_id).await,
        Err(ErrorCode::Forbidden)
    );
    // Promoted: connects at once.
    let r = p
        .call(
            &mut owner,
            Method::PATCH,
            &people_path(org, &format!("/{viewer}")),
            Some(json!({ "role": "technician" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    request(&mut v, &mut host, host_id).await.unwrap();

    // Someone of another org sees nothing and can't connect.
    let (other, _) = org_with_ca(&p, "Beta", false).await;
    let boss = p.user("boss@b.test", false, &[(other, Role::Owner)]).await;
    let bt = static_token(&p, other, boss).await;
    p.reload().await;
    let mut b = signed_in_operator(&url, &bt).await;
    assert!(listed(&mut b).await.is_empty());
    assert_eq!(
        request(&mut b, &mut host, host_id).await,
        Err(ErrorCode::NotFound)
    );
}

#[tokio::test]
async fn legacy_logins_keep_working() {
    let p = Portal::new().await;
    let (org, ca) = org_with_ca(&p, "Legacy", true).await;
    // 0.1.x: a username/password account and an operator token, both imported as
    // technicians.
    p.db.add_account(org, "alice", PASSWORD).await.unwrap();
    let (_, token) = p.db.add_operator(org).await.unwrap();
    p.reload().await;
    let url = p.serve_ws().await;
    let (mut host, host_id) = host_online(&url, &ca).await;

    let (mut ws, _) = connect_async(&url).await.unwrap();
    send(
        &mut ws,
        &ClientMessage::PasswordLogin {
            protocol_version: PROTOCOL_VERSION,
            username: "alice".into(),
            password: PASSWORD.into(),
        },
    )
    .await;
    let Some(ServerMessage::LoginToken { token: v1, .. }) = recv(&mut ws).await else {
        panic!("password login failed")
    };
    // Kept open: a closing socket would end its session.
    let mut open = Vec::new();
    for t in [v1.as_str(), token.as_str()] {
        let mut op = signed_in_operator(&url, t).await;
        assert_eq!(listed(&mut op).await, vec![host_id]);
        request(&mut op, &mut host, host_id).await.unwrap();
        open.push(op);
    }

    // Someone the database has never seen (an identity provider's operator) keeps the
    // 0.1.x rule in the legacy org, and gets nothing in any other.
    let tokens = SessionTokens::new(TOKEN_KEY);
    let stranger = |org_id| {
        tokens
            .issue(
                OperatorIdentity {
                    operator_id: OperatorId::new(),
                    org_id,
                },
                Duration::from_secs(600),
            )
            .unwrap()
            .0
    };
    let mut op = signed_in_operator(&url, &stranger(org)).await;
    request(&mut op, &mut host, host_id).await.unwrap();
    let (other, _) = org_with_ca(&p, "Beta", false).await;
    let (_, answer) = operator(&url, &stranger(other)).await;
    assert!(matches!(
        answer,
        Some(ServerMessage::Error {
            code: ErrorCode::Unauthenticated,
            ..
        })
    ));
}

// -- invites -------------------------------------------------------------------------

#[tokio::test]
async fn an_invite_is_emailed_hashed_and_accepted_once() {
    let p = Portal::new().await;
    let org = p.org("Acme").await;
    p.user("owner@a.test", true, &[(org, Role::Owner)]).await;
    let newbie = p.user("New@X.test", true, &[]).await;
    let mut owner = p.signed_in("owner@a.test").await;
    let (inv, token) = invite(&p, &mut owner, org, "new@x.test", "technician").await;
    assert_eq!(inv["email"], "new@x.test");
    assert_eq!(inv["role"], "technician");
    assert_eq!(inv["invited_by"], "owner@a.test");
    assert!(inv["expires_at"].as_str().unwrap().ends_with('Z'));
    let mail = p.mail.sent().pop().unwrap();
    assert_eq!(mail.subject, "Join Acme on Rconn");
    assert!(
        mail.text
            .contains(&format!("https://portal.test/admin/invite?token={token}")),
        "{}",
        mail.text
    );
    // Only the hash is stored; it lasts 7 days.
    let (stored, created, expires): (String, i64, i64) =
        sqlx::query_as("SELECT token_sha256, created_at, expires_at FROM invites WHERE id = ?")
            .bind(inv["id"].as_str().unwrap())
            .fetch_one(p.db.pool())
            .await
            .unwrap();
    assert_eq!(stored, rconn_signaling::portal::session::sha256_hex(&token));
    assert_eq!(expires - created, 7 * 24 * 3600);
    let r = p
        .call(&mut owner, Method::GET, &invites_path(org, ""), None)
        .await;
    assert_eq!(r.json.as_array().unwrap().len(), 1);
    assert_eq!(r.json[0], inv);

    // Signed out: 401, so the app sends them to sign in.
    let mut anon = Browser::new();
    assert_eq!(
        accept(&p, &mut anon, &token).await.status,
        StatusCode::UNAUTHORIZED
    );
    // The invited email (any case) joins with the invited role.
    let mut b = p.signed_in("new@x.test").await;
    let r = accept(&p, &mut b, &token).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json["user"]["id"], newbie.to_string());
    assert_eq!(r.json["memberships"][0]["org_id"], org.to_string());
    assert_eq!(r.json["memberships"][0]["role"], "technician");
    assert_eq!(
        p.db.role(org, newbie).await.unwrap(),
        Some(Role::Technician)
    );
    // Single use, and no longer pending.
    let r = accept(&p, &mut b, &token).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.code(), "invalid");
    let r = p
        .call(&mut owner, Method::GET, &invites_path(org, ""), None)
        .await;
    assert_eq!(r.json, json!([]));
    // Garbage tokens are just invalid.
    assert_eq!(
        accept(&p, &mut b, "nope").await.status,
        StatusCode::BAD_REQUEST
    );

    let rows = p.audit_rows().await;
    let org_s = org.to_string();
    assert!(
        rows.iter()
            .any(|r| r.2 == "person.invited" && r.0.as_deref() == Some(&*org_s))
    );
    assert!(
        rows.iter()
            .any(|r| r.2 == "person.joined" && r.0.as_deref() == Some(&*org_s))
    );
}

#[tokio::test]
async fn someone_new_signs_in_by_link_to_accept_an_invite_without_signup() {
    // A self-hosted server (no sign-up): the invited email has no account yet.
    let p = Portal::new().await;
    let org = p.org("Acme").await;
    p.user("owner@a.test", true, &[(org, Role::Owner)]).await;
    let mut owner = p.signed_in("owner@a.test").await;
    let (_, invite_token) = invite(&p, &mut owner, org, "Fresh@X.test", "viewer").await;

    let mut b = Browser::new();
    let n = p.mail.sent().len();
    let r = p
        .call(
            &mut b,
            Method::POST,
            "/api/v1/auth/magic-link",
            Some(json!({ "email": "fresh@x.test" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.text);
    let mail = p.mail.wait_for(n + 1).await.pop().unwrap();
    assert_eq!(mail.to, "fresh@x.test");
    let (token, _) = portal_support::token_and_code(&mail.text);
    let me = p
        .call(
            &mut b,
            Method::POST,
            "/api/v1/auth/magic",
            Some(json!({ "token": token })),
        )
        .await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.text);
    assert_eq!(me.json["memberships"], json!([]));
    let r = accept(&p, &mut b, &invite_token).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json["memberships"][0]["role"], "viewer");

    // Without an invite (or once it is cancelled or used), a new email still gets nothing.
    let n = p.mail.sent().len();
    for email in ["fresh@x.test", "stranger@x.test"] {
        let mut b = Browser::new();
        let r = p
            .call(
                &mut b,
                Method::POST,
                "/api/v1/auth/magic-link",
                Some(json!({ "email": email })),
            )
            .await;
        assert_eq!(r.status, StatusCode::ACCEPTED);
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let sent: Vec<_> = p.mail.sent().into_iter().skip(n).map(|m| m.to).collect();
    assert_eq!(
        sent,
        vec!["fresh@x.test".to_owned()],
        "only the person who now exists"
    );
}

#[tokio::test]
async fn an_invite_for_another_email_is_refused() {
    let p = Portal::new().await;
    let org = p.org("Acme").await;
    p.user("owner@a.test", true, &[(org, Role::Owner)]).await;
    let eve = p.user("eve@x.test", true, &[]).await;
    p.user("bob@x.test", true, &[]).await;
    let mut owner = p.signed_in("owner@a.test").await;
    let (_, token) = invite(&p, &mut owner, org, "bob@x.test", "admin").await;
    let mut b = p.signed_in("eve@x.test").await;
    let r = accept(&p, &mut b, &token).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.code(), "forbidden");
    assert_eq!(p.db.role(org, eve).await.unwrap(), None);
    // The right person can still use it.
    let mut bob = p.signed_in("bob@x.test").await;
    assert_eq!(accept(&p, &mut bob, &token).await.status, StatusCode::OK);
}

#[tokio::test]
async fn expired_and_cancelled_invites_stop_working() {
    let p = Portal::new().await;
    let org = p.org("Acme").await;
    p.user("owner@a.test", true, &[(org, Role::Owner)]).await;
    let x = p.user("x@x.test", true, &[]).await;
    let mut owner = p.signed_in("owner@a.test").await;
    let mut b = p.signed_in("x@x.test").await;

    let (inv, token) = invite(&p, &mut owner, org, "x@x.test", "viewer").await;
    sqlx::query("UPDATE invites SET expires_at = 1 WHERE id = ?")
        .bind(inv["id"].as_str().unwrap())
        .execute(p.db.pool())
        .await
        .unwrap();
    let r = accept(&p, &mut b, &token).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.code(), "invalid");
    // An expired invite isn't pending, so a new one can go out.
    let (inv, token) = invite(&p, &mut owner, org, "x@x.test", "viewer").await;
    let path = invites_path(org, &format!("/{}", inv["id"].as_str().unwrap()));
    let r = p.call(&mut owner, Method::DELETE, &path, None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(
        accept(&p, &mut b, &token).await.status,
        StatusCode::BAD_REQUEST
    );
    let r = p.call(&mut owner, Method::DELETE, &path, None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(p.db.role(org, x).await.unwrap(), None);
}

#[tokio::test]
async fn invite_rules() {
    let p = Portal::new().await;
    let org = p.org("Acme").await;
    p.user("owner@a.test", true, &[(org, Role::Owner)]).await;
    p.user("admin@a.test", true, &[(org, Role::Admin)]).await;
    p.user("member@a.test", false, &[(org, Role::Viewer)]).await;
    let mut admin = p.signed_in("admin@a.test").await;
    let post = |email: &str, role: &str| {
        (
            invites_path(org, ""),
            Some(json!({ "email": email, "role": role })),
        )
    };
    for (email, role, status) in [
        ("boss@x.test", "owner", StatusCode::FORBIDDEN),
        ("not an email", "viewer", StatusCode::BAD_REQUEST),
        ("x@x.test", "wizard", StatusCode::BAD_REQUEST),
        ("MEMBER@a.test", "viewer", StatusCode::CONFLICT),
    ] {
        let (path, body) = post(email, role);
        let r = p.call(&mut admin, Method::POST, &path, body).await;
        assert_eq!(r.status, status, "{email} as {role}: {}", r.text);
    }
    invite(&p, &mut admin, org, "new@x.test", "admin").await;
    let (path, body) = post("NEW@x.test", "viewer");
    let r = p.call(&mut admin, Method::POST, &path, body).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "already invited");
    // Only an owner invites owners, and an admin can't cancel an owner's invite.
    let mut owner = p.signed_in("owner@a.test").await;
    let (inv, _) = invite(&p, &mut owner, org, "boss@x.test", "owner").await;
    let r = p
        .call(
            &mut admin,
            Method::DELETE,
            &invites_path(org, &format!("/{}", inv["id"].as_str().unwrap())),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // Another org's invite is not found.
    let other = p.org("Beta").await;
    p.user("b@b.test", true, &[(other, Role::Owner)]).await;
    let mut b = p.signed_in("b@b.test").await;
    let r = p
        .call(
            &mut b,
            Method::DELETE,
            &invites_path(other, &format!("/{}", inv["id"].as_str().unwrap())),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(p.mail.sent().len(), 2, "refused invites send nothing");
}
