//! Portal sign-in over HTTP with cookies: password, magic link, sign-out, sessions,
//! request checks, rate limits, `/me`, company sign-up, and the audit rows they write.

mod portal_support;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use portal_support::{Browser, ORIGIN, PASSWORD, Portal, token_and_code};
use rconn_signaling::db::Role;
use rconn_signaling::portal::ratelimit::Limit;
use serde_json::json;
use std::time::Duration;

const ME: &str = "/api/v1/me";

async fn ask_link(p: &Portal, b: &mut Browser, email: &str) -> portal_support::Resp {
    p.call(
        b,
        Method::POST,
        "/api/v1/auth/magic-link",
        Some(json!({ "email": email })),
    )
    .await
}

async fn use_link(
    p: &Portal,
    b: &mut Browser,
    token: &str,
    code: Option<&str>,
) -> portal_support::Resp {
    p.call(
        b,
        Method::POST,
        "/api/v1/auth/magic",
        Some(json!({ "token": token, "code": code })),
    )
    .await
}

#[tokio::test]
async fn password_sign_in_sets_a_host_cookie_and_answers_me() {
    let p = Portal::new().await;
    let org = p.org("Acme").await;
    let alice = p.user("alice@acme.test", true, &[(org, Role::Admin)]).await;
    let mut b = Browser::new();
    let r = p
        .call(
            &mut b,
            Method::POST,
            "/api/v1/auth/password",
            Some(json!({ "email": "Alice@Acme.test ", "password": PASSWORD })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let cookie = r.headers[header::SET_COOKIE].to_str().unwrap();
    assert!(cookie.starts_with("__Host-rconn_session="), "{cookie}");
    for attr in ["Path=/", "Secure", "HttpOnly", "SameSite=Lax", "Max-Age="] {
        assert!(cookie.contains(attr), "{attr} missing in {cookie}");
    }
    assert_eq!(r.json["user"]["id"], alice.to_string());
    assert_eq!(r.json["memberships"][0]["org_id"], org.to_string());
    assert_eq!(r.json["memberships"][0]["org_name"], "Acme");
    assert_eq!(r.json["memberships"][0]["role"], "admin");

    let me = p.call(&mut b, Method::GET, ME, None).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.json, r.json);
    assert_eq!(me.headers[header::CACHE_CONTROL], "no-store");

    let rows = p.audit_rows().await;
    assert!(
        rows.contains(&(
            Some(org.to_string()),
            Some(alice.to_string()),
            "sign_in".into()
        )),
        "{rows:?}"
    );
    // The stored session id is a hash, never the cookie value.
    let raw = b.session().unwrap().clone();
    let stored: Vec<String> = sqlx::query_scalar("SELECT id_sha256 FROM sessions")
        .fetch_all(p.db.pool())
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_ne!(stored[0], raw);
}

#[tokio::test]
async fn wrong_password_answers_exactly_like_an_unknown_email() {
    let p = Portal::new().await;
    let org = p.org("Acme").await;
    p.user("alice@acme.test", true, &[(org, Role::Owner)]).await;
    let mut b = Browser::new();
    let wrong = p
        .call(
            &mut b,
            Method::POST,
            "/api/v1/auth/password",
            Some(json!({ "email": "alice@acme.test", "password": "not the password" })),
        )
        .await;
    let unknown = p
        .call(
            &mut b,
            Method::POST,
            "/api/v1/auth/password",
            Some(json!({ "email": "nobody@acme.test", "password": "not the password" })),
        )
        .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert_eq!(unknown.status, wrong.status);
    assert_eq!(unknown.text, wrong.text);
    assert_eq!(wrong.code(), "unauthorized");
    assert!(b.session().is_none());
    // A person without a password login can't sign in with one either.
    p.user("bob@acme.test", false, &[(org, Role::Admin)]).await;
    let nopw = p
        .call(
            &mut b,
            Method::POST,
            "/api/v1/auth/password",
            Some(json!({ "email": "bob@acme.test", "password": "" })),
        )
        .await;
    assert_eq!(nopw.text, wrong.text);

    let rows = p.audit_rows().await;
    let failed: Vec<_> = rows.iter().filter(|r| r.2 == "sign_in.failed").collect();
    assert_eq!(failed.len(), 3, "{rows:?}");
    // Alice's failure shows in her org; the unknown email's in no org.
    assert_eq!(failed[0].0.as_deref(), Some(org.to_string().as_str()));
    assert_eq!(failed[1].0, None);
}

#[tokio::test]
async fn magic_link_same_browser_signs_in_once() {
    let p = Portal::new().await;
    let org = p.org("Acme").await;
    let alice = p
        .user("alice@acme.test", false, &[(org, Role::Viewer)])
        .await;
    let mut b = Browser::new();
    let r = ask_link(&p, &mut b, "alice@acme.test").await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    assert!(b.cookies.contains_key("__Host-rconn_magic_nonce"));
    let mail = p.mail.wait_for(1).await;
    assert_eq!(mail[0].to, "alice@acme.test");
    assert!(
        mail[0]
            .text
            .contains(&format!("{ORIGIN}/admin/magic?token=")),
        "{}",
        mail[0].text
    );
    let (token, _code) = token_and_code(&mail[0].text);

    // Opening the link (or a mail scanner fetching it) uses nothing.
    let page = p
        .call(
            &mut b,
            Method::GET,
            &format!("/admin/magic?token={token}"),
            None,
        )
        .await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(page.headers[header::REFERRER_POLICY], "no-referrer");
    assert!(b.session().is_none());

    let r = use_link(&p, &mut b, &token, None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json["user"]["id"], alice.to_string());
    assert!(b.session().is_some());
    assert!(
        !b.cookies.contains_key("__Host-rconn_magic_nonce"),
        "nonce cleared"
    );
    assert_eq!(
        p.call(&mut b, Method::GET, ME, None).await.status,
        StatusCode::OK
    );

    // Single use, even with the code.
    let (_, code) = token_and_code(&mail[0].text);
    let reuse = use_link(&p, &mut b, &token, Some(&code)).await;
    assert_eq!(reuse.status, StatusCode::BAD_REQUEST);
    assert_eq!(reuse.code(), "invalid");

    let rows = p.audit_rows().await;
    assert!(rows.contains(&(
        Some(org.to_string()),
        Some(alice.to_string()),
        "sign_in".into()
    )));
}

#[tokio::test]
async fn magic_link_in_another_browser_needs_the_code() {
    let p = Portal::new().await;
    let org = p.org("Acme").await;
    p.user("alice@acme.test", false, &[(org, Role::Admin)])
        .await;
    let mut asked = Browser::new();
    ask_link(&p, &mut asked, "alice@acme.test").await;
    let (token, code) = token_and_code(&p.mail.wait_for(1).await[0].text);

    let mut phone = Browser::at("198.51.100.7");
    let r = use_link(&p, &mut phone, &token, None).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.code(), "needs_code");
    let wrong = if code == "000000" { "111111" } else { "000000" };
    let r = use_link(&p, &mut phone, &token, Some(wrong)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.code(), "invalid");
    assert!(phone.session().is_none());
    let spaced = format!("{} {}", &code[..3], &code[3..]);
    let r = use_link(&p, &mut phone, &token, Some(&spaced)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert!(phone.session().is_some());
    // The asking browser can't use it any more.
    assert_eq!(
        use_link(&p, &mut asked, &token, None).await.code(),
        "invalid"
    );
}

#[tokio::test]
async fn magic_link_code_guesses_are_capped() {
    let p = Portal::new().await;
    let org = p.org("Acme").await;
    p.user("alice@acme.test", false, &[(org, Role::Admin)])
        .await;
    ask_link(&p, &mut Browser::new(), "alice@acme.test").await;
    let (token, code) = token_and_code(&p.mail.wait_for(1).await[0].text);
    let wrong = if code == "000000" { "111111" } else { "000000" };
    let mut other = Browser::at("198.51.100.8");
    for _ in 0..5 {
        assert_eq!(
            use_link(&p, &mut other, &token, Some(wrong)).await.code(),
            "invalid"
        );
    }
    let r = use_link(&p, &mut other, &token, Some(&code)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.text);
    assert!(other.session().is_none());
}

#[tokio::test]
async fn magic_link_expires() {
    let p = Portal::new().await;
    let org = p.org("Acme").await;
    p.user("alice@acme.test", false, &[(org, Role::Admin)])
        .await;
    let mut b = Browser::new();
    ask_link(&p, &mut b, "alice@acme.test").await;
    let (token, _) = token_and_code(&p.mail.wait_for(1).await[0].text);
    let ttl: i64 = sqlx::query_scalar("SELECT expires_at - created_at FROM magic_links")
        .fetch_one(p.db.pool())
        .await
        .unwrap();
    assert_eq!(ttl, 15 * 60);
    sqlx::query("UPDATE magic_links SET expires_at = created_at - 1")
        .execute(p.db.pool())
        .await
        .unwrap();
    let r = use_link(&p, &mut b, &token, None).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.code(), "invalid");
    assert!(b.session().is_none());
}

#[tokio::test]
async fn magic_link_answers_the_same_for_unknown_emails() {
    let p = Portal::new().await;
    let org = p.org("Acme").await;
    p.user("alice@acme.test", false, &[(org, Role::Admin)])
        .await;
    let mut b1 = Browser::new();
    let mut b2 = Browser::at("192.0.2.11");
    let known = ask_link(&p, &mut b1, "alice@acme.test").await;
    let unknown = ask_link(&p, &mut b2, "nobody@acme.test").await;
    assert_eq!(known.status, StatusCode::ACCEPTED);
    assert_eq!(unknown.status, known.status);
    assert_eq!(unknown.text, known.text);
    assert!(b2.cookies.contains_key("__Host-rconn_magic_nonce"));
    p.mail.wait_for(1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(p.mail.sent().len(), 1, "no mail to an unknown address");
    let bad = ask_link(&p, &mut b1, "not an email").await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn logout_revokes_the_session_server_side() {
    let p = Portal::new().await;
    let org = p.org("Acme").await;
    p.user("alice@acme.test", true, &[(org, Role::Admin)]).await;
    let mut b = p.signed_in("alice@acme.test").await;
    let copy = b.clone();
    let r = p
        .call(&mut b, Method::POST, "/api/v1/auth/logout", None)
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert!(b.session().is_none(), "cookie cleared");
    let mut stolen = copy;
    let r = p.call(&mut stolen, Method::GET, ME, None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED, "old cookie is dead");
    assert_eq!(r.code(), "unauthorized");
}

#[tokio::test]
async fn signing_in_again_rotates_the_session() {
    let p = Portal::new().await;
    let org = p.org("Acme").await;
    p.user("alice@acme.test", true, &[(org, Role::Admin)]).await;
    let mut b = p.signed_in("alice@acme.test").await;
    let first = b.clone();
    let r = p
        .call(
            &mut b,
            Method::POST,
            "/api/v1/auth/password",
            Some(json!({ "email": "alice@acme.test", "password": PASSWORD })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_ne!(b.session(), first.session());
    assert_eq!(
        p.call(&mut first.clone(), Method::GET, ME, None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        p.call(&mut b, Method::GET, ME, None).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn expired_idle_and_disabled_sessions_end() {
    let p = Portal::new().await;
    let org = p.org("Acme").await;
    let alice = p.user("alice@acme.test", true, &[(org, Role::Admin)]).await;
    let mut b = p.signed_in("alice@acme.test").await;
    sqlx::query("UPDATE sessions SET last_seen = last_seen - 25 * 3600")
        .execute(p.db.pool())
        .await
        .unwrap();
    assert_eq!(
        p.call(&mut b, Method::GET, ME, None).await.status,
        StatusCode::UNAUTHORIZED,
        "idle for more than a day"
    );
    let mut b = p.signed_in("alice@acme.test").await;
    sqlx::query("UPDATE sessions SET expires_at = 0")
        .execute(p.db.pool())
        .await
        .unwrap();
    assert_eq!(
        p.call(&mut b, Method::GET, ME, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    let mut b = p.signed_in("alice@acme.test").await;
    sqlx::query("UPDATE users SET disabled_at = 1 WHERE id = ?")
        .bind(alice.to_string())
        .execute(p.db.pool())
        .await
        .unwrap();
    assert_eq!(
        p.call(&mut b, Method::GET, ME, None).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn non_get_requests_need_our_origin_and_json() {
    let p = Portal::new().await;
    let org = p.org("Acme").await;
    p.user("alice@acme.test", true, &[(org, Role::Admin)]).await;
    let mut b = p.signed_in("alice@acme.test").await;
    let body = || Body::from(r#"{"name":"Mallory"}"#);
    let patch = |origin: Option<&str>, ct: &str| {
        let mut r = Request::builder()
            .method(Method::PATCH)
            .uri(ME)
            .header(header::CONTENT_TYPE, ct);
        if let Some(o) = origin {
            r = r.header(header::ORIGIN, o);
        }
        r.body(body()).unwrap()
    };
    let r = p.send(&mut b, patch(None, "application/json")).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "no Origin");
    let r = p
        .send(&mut b, patch(Some("https://evil.test"), "application/json"))
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "foreign Origin");
    assert_eq!(r.code(), "forbidden");
    let r = p
        .send(
            &mut b,
            patch(Some(ORIGIN), "application/x-www-form-urlencoded"),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "form body");
    let r = p.send(&mut b, patch(Some(ORIGIN), "text/plain")).await;
    assert_eq!(r.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    // Missing content type on a JSON endpoint.
    let r = p
        .send(
            &mut b,
            Request::builder()
                .method(Method::PATCH)
                .uri(ME)
                .header(header::ORIGIN, ORIGIN)
                .body(body())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    // POST always carries JSON, even when the endpoint takes no body.
    let r = p
        .send(
            &mut b,
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/auth/logout")
                .header(header::ORIGIN, ORIGIN)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(b.session().is_some(), "still signed in");
    let me = p.call(&mut b, Method::GET, ME, None).await;
    assert_eq!(me.json["user"]["name"], "alice", "nothing changed");
    let r = p
        .send(
            &mut b,
            patch(Some(ORIGIN), "application/json; charset=utf-8"),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json["user"]["name"], "Mallory");
}

#[tokio::test]
async fn without_a_portal_url_the_origin_must_match_the_host() {
    let p = Portal::with(|c| c.portal_url = None).await;
    let mut b = Browser::new();
    let req = |path: &str, origin: &str, body: &str| {
        Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::HOST, "rconn.example.com")
            .header(header::ORIGIN, origin)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap()
    };
    let logout = "/api/v1/auth/logout";
    let r = p
        .send(&mut b, req(logout, "https://rconn.example.com", ""))
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = p.send(&mut b, req(logout, "https://evil.test", "")).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = ask_link(&p, &mut b, "alice@acme.test").await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "no Host header: refused");
    // Magic links are off: the same 202, nothing sent.
    let org = p.org("Acme").await;
    p.user("alice@acme.test", false, &[(org, Role::Admin)])
        .await;
    let r = p
        .send(
            &mut b,
            req(
                "/api/v1/auth/magic-link",
                "https://rconn.example.com",
                r#"{"email":"alice@acme.test"}"#,
            ),
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(p.mail.sent().is_empty());
    let opts = p
        .call(
            &mut b,
            Method::GET,
            "/api/v1/auth/options?email=a@b.c",
            None,
        )
        .await;
    assert_eq!(opts.json["magic_link"], false);
}

#[tokio::test]
async fn sign_in_is_rate_limited_per_email_and_per_address() {
    let p = Portal::with(|c| {
        c.limits.sign_in_per_email = Limit::new(3, Duration::from_secs(600));
        c.limits.sign_in_per_ip = Limit::new(5, Duration::from_secs(600));
        c.limits.magic_link_per_email = Limit::new(2, Duration::from_secs(600));
        c.limits.magic_link_per_ip = Limit::new(4, Duration::from_secs(600));
    })
    .await;
    let bad = |email: &str| json!({ "email": email, "password": "wrong wrong wrong" });
    // Per email, across addresses.
    for i in 0..3 {
        let mut b = Browser::at(&format!("203.0.113.{i}"));
        let r = p
            .call(
                &mut b,
                Method::POST,
                "/api/v1/auth/password",
                Some(bad("victim@acme.test")),
            )
            .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    let mut b = Browser::at("203.0.113.9");
    let r = p
        .call(
            &mut b,
            Method::POST,
            "/api/v1/auth/password",
            Some(bad("victim@acme.test")),
        )
        .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.code(), "rate_limited");
    // Per address, across emails (IPv6 counts by /64).
    for i in 0..5 {
        let mut b = Browser::at(&format!("2001:db8::{i}"));
        let r = p
            .call(
                &mut b,
                Method::POST,
                "/api/v1/auth/password",
                Some(bad(&format!("u{i}@acme.test"))),
            )
            .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    let mut b = Browser::at("2001:db8::ff");
    let r = p
        .call(
            &mut b,
            Method::POST,
            "/api/v1/auth/password",
            Some(bad("fresh@acme.test")),
        )
        .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    // Magic links: per email, then per address.
    let mut b = Browser::at("198.51.100.1");
    for _ in 0..2 {
        assert_eq!(
            ask_link(&p, &mut b, "x@acme.test").await.status,
            StatusCode::ACCEPTED
        );
    }
    assert_eq!(
        ask_link(&p, &mut b, "x@acme.test").await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        ask_link(&p, &mut b, "y@acme.test").await.status,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        ask_link(&p, &mut b, "z@acme.test").await.status,
        StatusCode::TOO_MANY_REQUESTS,
        "4 per address"
    );
}

#[tokio::test]
async fn options_are_the_same_for_every_email() {
    let p = Portal::new().await;
    let mut b = Browser::new();
    let none = p
        .call(
            &mut b,
            Method::GET,
            "/api/v1/auth/options?email=a@acme.test",
            None,
        )
        .await;
    assert_eq!(
        none.json,
        json!({ "password": false, "magic_link": true, "sso": null, "passkey": true })
    );
    let org = p.org("Acme").await;
    p.user("alice@acme.test", true, &[(org, Role::Admin)]).await;
    let known = p
        .call(
            &mut b,
            Method::GET,
            "/api/v1/auth/options?email=alice@acme.test",
            None,
        )
        .await;
    let unknown = p
        .call(
            &mut b,
            Method::GET,
            "/api/v1/auth/options?email=nobody@acme.test",
            None,
        )
        .await;
    assert_eq!(known.json["password"], true);
    assert_eq!(known.json, unknown.json);
}

#[tokio::test]
async fn me_needs_a_session_and_patch_checks_the_name() {
    let p = Portal::new().await;
    let mut anon = Browser::new();
    assert_eq!(
        p.call(&mut anon, Method::GET, ME, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    anon.cookies
        .insert("__Host-rconn_session".into(), "forged".into());
    assert_eq!(
        p.call(&mut anon, Method::GET, ME, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    let org = p.org("Acme").await;
    p.user("alice@acme.test", true, &[(org, Role::Viewer)])
        .await;
    let mut b = p.signed_in("alice@acme.test").await;
    for bad in ["", "   ", &"x".repeat(101), "a\u{7}b"] {
        let r = p
            .call(&mut b, Method::PATCH, ME, Some(json!({ "name": bad })))
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad:?}");
        assert_eq!(r.code(), "invalid");
    }
    let r = p
        .call(
            &mut b,
            Method::PATCH,
            ME,
            Some(json!({ "name": " Alice A. " })),
        )
        .await;
    assert_eq!(r.json["user"]["name"], "Alice A.");
}

#[tokio::test]
async fn creating_a_company_needs_signup_enabled() {
    let closed = Portal::new().await;
    let org = closed.org("Acme").await;
    closed
        .user("alice@acme.test", true, &[(org, Role::Owner)])
        .await;
    let mut b = closed.signed_in("alice@acme.test").await;
    let r = closed
        .call(
            &mut b,
            Method::POST,
            "/api/v1/orgs",
            Some(json!({ "name": "Beta" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    let open = Portal::with(|c| c.allow_signup = true).await;
    // Sign-up by magic link creates the person on first use.
    let mut b = Browser::new();
    ask_link(&open, &mut b, "new@beta.test").await;
    let (token, _) = token_and_code(&open.mail.wait_for(1).await[0].text);
    let me = use_link(&open, &mut b, &token, None).await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.text);
    assert_eq!(me.json["user"]["email"], "new@beta.test");
    assert_eq!(me.json["memberships"], json!([]));
    let r = open
        .call(
            &mut b,
            Method::POST,
            "/api/v1/orgs",
            Some(json!({ "name": " Beta " })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.json["name"], "Beta");
    assert_eq!(r.json["plan"], "free");
    let id = r.json["id"].as_str().unwrap().to_owned();
    let me = open.call(&mut b, Method::GET, ME, None).await;
    assert_eq!(me.json["memberships"][0]["org_id"], id);
    assert_eq!(me.json["memberships"][0]["role"], "owner");
    let sealed: Option<Vec<u8>> = sqlx::query_scalar("SELECT ca_key_sealed FROM orgs WHERE id = ?")
        .bind(&id)
        .fetch_one(open.db.pool())
        .await
        .unwrap();
    assert!(sealed.is_some(), "the new org gets its own sealed CA key");
    let rows = open.audit_rows().await;
    assert!(
        rows.iter()
            .any(|r| r.0.as_deref() == Some(id.as_str()) && r.2 == "org.created")
    );
}

#[tokio::test]
async fn admin_ui_and_unknown_api_paths() {
    let p = Portal::new().await;
    let mut b = Browser::new();
    let r = p.call(&mut b, Method::GET, "/admin", None).await;
    assert_eq!(r.status, StatusCode::PERMANENT_REDIRECT);
    let r = p.call(&mut b, Method::GET, "/admin/people", None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers[header::X_FRAME_OPTIONS], "DENY");
    let r = p.call(&mut b, Method::GET, "/api/v1/nope", None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.code(), "not_found");
    let r = p
        .call(
            &mut b,
            Method::POST,
            "/api/v1/auth/passkey/login/start",
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "no email: {}", r.text);
}
