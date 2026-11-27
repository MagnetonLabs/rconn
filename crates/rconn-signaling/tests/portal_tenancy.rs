//! Tenant isolation and the role table: a member of org A gets 404 for every org B route
//! (like an org that doesn't exist), and each role gets exactly the rights the contract
//! lists. Routes not built yet answer 501 to members whose role passes.

mod portal_support;

use axum::http::{Method, StatusCode};
use portal_support::{Browser, Portal};
use rconn_core::ids::OrgId;
use rconn_signaling::db::Role;
use rconn_signaling::portal::Action;
use serde_json::{Value, json};

const X: &str = "00000000-0000-4000-8000-000000000001";

/// Every `/orgs/{org_id}/…` route of the contract, the action it needs, and a valid body.
fn org_routes() -> Vec<(Method, String, Action, Option<Value>)> {
    let name = || Some(json!({ "name": "Renamed" }));
    vec![
        (Method::GET, String::new(), Action::View, None),
        (Method::PATCH, String::new(), Action::ManageCompany, name()),
        (Method::GET, "/computers".into(), Action::View, None),
        (
            Method::PATCH,
            format!("/computers/{X}"),
            Action::ManageFleet,
            name(),
        ),
        (
            Method::POST,
            format!("/computers/{X}/revoke"),
            Action::ManageFleet,
            None,
        ),
        (
            Method::GET,
            "/enrollment-tokens".into(),
            Action::ManageFleet,
            None,
        ),
        (
            Method::POST,
            "/enrollment-tokens".into(),
            Action::ManageFleet,
            Some(json!({ "label": "x", "expires_in_hours": 1 })),
        ),
        (
            Method::DELETE,
            format!("/enrollment-tokens/{X}"),
            Action::ManageFleet,
            None,
        ),
        (Method::GET, "/people".into(), Action::View, None),
        (
            Method::PATCH,
            format!("/people/{X}"),
            Action::ManagePeople,
            Some(json!({ "role": "viewer" })),
        ),
        (
            Method::DELETE,
            format!("/people/{X}"),
            Action::ManagePeople,
            None,
        ),
        (Method::GET, "/invites".into(), Action::ManagePeople, None),
        (
            Method::POST,
            "/invites".into(),
            Action::ManagePeople,
            Some(json!({ "email": "x@y.test", "role": "viewer" })),
        ),
        (
            Method::DELETE,
            format!("/invites/{X}"),
            Action::ManagePeople,
            None,
        ),
        (Method::GET, "/groups".into(), Action::View, None),
        (Method::POST, "/groups".into(), Action::ManageFleet, name()),
        (
            Method::PATCH,
            format!("/groups/{X}"),
            Action::ManageFleet,
            name(),
        ),
        (
            Method::DELETE,
            format!("/groups/{X}"),
            Action::ManageFleet,
            None,
        ),
        (
            Method::GET,
            format!("/groups/{X}/policy"),
            Action::View,
            None,
        ),
        (
            Method::PUT,
            format!("/groups/{X}/policy"),
            Action::ManageFleet,
            Some(json!({})),
        ),
        (Method::GET, "/policy/default".into(), Action::View, None),
        (
            Method::PUT,
            "/policy/default".into(),
            Action::ManageFleet,
            Some(json!({})),
        ),
        (Method::GET, "/audit".into(), Action::ViewAudit, None),
        (Method::GET, "/audit.csv".into(), Action::ViewAudit, None),
        (Method::GET, "/sso".into(), Action::ManageCompany, None),
        (
            Method::PUT,
            "/sso".into(),
            Action::ManageCompany,
            Some(
                json!({ "issuer": "https://idp.test", "client_id": "x", "auto_join_role": "viewer" }),
            ),
        ),
        (
            Method::POST,
            "/sso/domains".into(),
            Action::ManageCompany,
            Some(json!({ "domain": "acme.test" })),
        ),
        (
            Method::POST,
            "/sso/domains/acme.test/verify".into(),
            Action::ManageCompany,
            None,
        ),
    ]
}

struct World {
    p: Portal,
    a: OrgId,
    b: OrgId,
}

async fn world() -> World {
    let p = Portal::new().await;
    let a = p.org("Acme").await;
    let b = p.org("Beta").await;
    for (email, role) in [
        ("owner@a.test", Role::Owner),
        ("admin@a.test", Role::Admin),
        ("tech@a.test", Role::Technician),
        ("viewer@a.test", Role::Viewer),
    ] {
        p.user(email, true, &[(a, role)]).await;
    }
    p.user("owner@b.test", true, &[(b, Role::Owner)]).await;
    World { p, a, b }
}

#[tokio::test]
async fn a_member_of_one_org_gets_404_for_every_route_of_another() {
    let w = world().await;
    let mut owner_a = w.p.signed_in("owner@a.test").await;
    let mut anon = Browser::new();
    for (method, rest, _, body) in org_routes() {
        for org in [
            w.b.to_string(),
            OrgId::new().to_string(),
            "not-a-uuid".into(),
        ] {
            let path = format!("/api/v1/orgs/{org}{rest}");
            let r =
                w.p.call(&mut owner_a, method.clone(), &path, body.clone())
                    .await;
            assert_eq!(
                r.status,
                StatusCode::NOT_FOUND,
                "{method} {path}: {}",
                r.text
            );
            assert_eq!(r.code(), "not_found", "{method} {path}");
        }
        let path = format!("/api/v1/orgs/{}{rest}", w.b);
        let r =
            w.p.call(&mut anon, method.clone(), &path, body.clone())
                .await;
        assert_eq!(
            r.status,
            StatusCode::UNAUTHORIZED,
            "{method} {path} signed out"
        );
    }
    // B's data is untouched and A's owner never shows up in it.
    let name: String = sqlx::query_scalar("SELECT name FROM orgs WHERE id = ?")
        .bind(w.b.to_string())
        .fetch_one(w.p.db.pool())
        .await
        .unwrap();
    assert_eq!(name, "Beta");
    let rows = w.p.audit_rows().await;
    assert!(
        !rows
            .iter()
            .any(|r| r.0.as_deref() == Some(w.b.to_string().as_str()) && r.2 != "sign_in"),
        "{rows:?}"
    );
}

#[tokio::test]
async fn each_role_gets_what_the_table_says() {
    let w = world().await;
    let roles = [
        ("owner@a.test", Role::Owner),
        ("admin@a.test", Role::Admin),
        ("tech@a.test", Role::Technician),
        ("viewer@a.test", Role::Viewer),
    ];
    for (email, role) in roles {
        let mut b = w.p.signed_in(email).await;
        for (method, rest, action, body) in org_routes() {
            // Renaming the org would change what later checks see; done separately below.
            if method == Method::PATCH && rest.is_empty() {
                continue;
            }
            let path = format!("/api/v1/orgs/{}{rest}", w.a);
            let r = w.p.call(&mut b, method.clone(), &path, body).await;
            let allowed = role.at_least(action.min_role());
            if allowed {
                // Past the role check: any answer but 401/403/500 (a route may 404 the
                // made-up id, 409 a name taken by an earlier role, 400 a stub body).
                assert!(
                    !matches!(
                        r.status,
                        StatusCode::UNAUTHORIZED
                            | StatusCode::FORBIDDEN
                            | StatusCode::INTERNAL_SERVER_ERROR
                    ),
                    "{role:?} {method} {path}: {} {}",
                    r.status,
                    r.text
                );
            } else {
                assert_eq!(r.status, StatusCode::FORBIDDEN, "{role:?} {method} {path}");
                assert_eq!(r.code(), "forbidden");
            }
        }
    }
}

#[test]
fn the_role_table() {
    use Action::*;
    let table = [
        (View, [true, true, true, true]),
        (Connect, [true, true, true, false]),
        (ManageFleet, [true, true, false, false]),
        (ManagePeople, [true, true, false, false]),
        (ViewAudit, [true, true, false, false]),
        (ManageCompany, [true, false, false, false]),
    ];
    let roles = [Role::Owner, Role::Admin, Role::Technician, Role::Viewer];
    for (action, allowed) in table {
        for (role, ok) in roles.iter().zip(allowed) {
            assert_eq!(role.at_least(action.min_role()), ok, "{role:?} {action:?}");
        }
    }
}

#[tokio::test]
async fn only_the_owner_renames_the_company_and_it_is_audited() {
    let w = world().await;
    let path = format!("/api/v1/orgs/{}", w.a);
    let mut admin = w.p.signed_in("admin@a.test").await;
    let r =
        w.p.call(
            &mut admin,
            Method::PATCH,
            &path,
            Some(json!({ "name": "Nope" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let mut viewer = w.p.signed_in("viewer@a.test").await;
    let r = w.p.call(&mut viewer, Method::GET, &path, None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["name"], "Acme");
    assert_eq!(r.json["created_at"], "2026-11-27T09:54:56Z");
    assert_eq!(r.json["plan"], "free");

    let mut owner = w.p.signed_in("owner@a.test").await;
    let r =
        w.p.call(
            &mut owner,
            Method::PATCH,
            &path,
            Some(json!({ "name": "Acme Inc" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json["name"], "Acme Inc");
    let me = w.p.call(&mut viewer, Method::GET, "/api/v1/me", None).await;
    assert_eq!(me.json["memberships"][0]["org_name"], "Acme Inc");

    let rows = w.p.audit_rows().await;
    let renamed: Vec<_> = rows.iter().filter(|r| r.2 == "org.renamed").collect();
    assert_eq!(renamed.len(), 1, "{rows:?}");
    assert_eq!(renamed[0].0.as_deref(), Some(w.a.to_string().as_str()));
    let detail: String = sqlx::query_scalar("SELECT detail FROM audit WHERE event = 'org.renamed'")
        .fetch_one(w.p.db.pool())
        .await
        .unwrap();
    let detail: Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(
        detail["detail"],
        json!({ "from": "Acme", "to": "Acme Inc" })
    );
    assert_eq!(detail["target"]["type"], "org");
}

#[tokio::test]
async fn a_person_in_two_orgs_reaches_both_and_sees_both() {
    let w = world().await;
    w.p.user(
        "both@x.test",
        true,
        &[(w.a, Role::Viewer), (w.b, Role::Admin)],
    )
    .await;
    let mut b = w.p.signed_in("both@x.test").await;
    let me = w.p.call(&mut b, Method::GET, "/api/v1/me", None).await;
    assert_eq!(me.json["memberships"].as_array().unwrap().len(), 2);
    for org in [w.a, w.b] {
        let r =
            w.p.call(&mut b, Method::GET, &format!("/api/v1/orgs/{org}"), None)
                .await;
        assert_eq!(r.status, StatusCode::OK);
    }
    // The role is per org: viewer in A, admin in B.
    let r =
        w.p.call(
            &mut b,
            Method::GET,
            &format!("/api/v1/orgs/{}/audit", w.a),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r =
        w.p.call(
            &mut b,
            Method::GET,
            &format!("/api/v1/orgs/{}/audit", w.b),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    // The sign-in shows in both orgs' logs.
    let rows = w.p.audit_rows().await;
    for org in [w.a, w.b] {
        assert!(
            rows.iter()
                .any(|r| r.0.as_deref() == Some(org.to_string().as_str()) && r.2 == "sign_in")
        );
    }
}
