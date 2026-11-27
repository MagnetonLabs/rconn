//! Signing in to the portal: password, magic link, sign-out, and which methods to offer.
//!
//! Unknown emails get the same answers as known ones: password sign-in answers 401 for
//! both (after the same argon2 work), and asking for a magic link always answers 202.

use super::me::{self, Me};
use super::ratelimit::ip_key;
use super::security::{clear_cookie, cookie, set_cookie};
use super::session::{
    NONCE_COOKIE, SESSION_COOKIE, looks_like_token, random_token, revoke, sha256_hex, start_session,
};
use super::{ApiError, ApiJson, ApiResult, ClientInfo, PortalState};
use crate::audit::AuditEvent;
use crate::db::Db;
use crate::mail::Mail;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rconn_core::ids::{OperatorId, OrgId};
use serde::{Deserialize, Serialize};
use sqlx::Row;

/// Wrong codes allowed per magic link before it stops working.
pub const MAX_CODE_ATTEMPTS: i64 = 5;
/// Longest password checked (argon2 work is bounded by the input).
const MAX_PASSWORD_BYTES: usize = 1024;
const MAX_EMAIL_CHARS: usize = 254;

fn tidy_email(s: &str) -> String {
    s.trim().to_owned()
}

pub(super) fn plausible_email(s: &str) -> bool {
    let parts = s.split_once('@');
    s.chars().count() <= MAX_EMAIL_CHARS
        && !s.chars().any(|c| c.is_whitespace() || c.is_control())
        && parts.is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'))
}

/// Rate-limit key for an email (case-insensitive, like the users table).
fn email_key(email: &str) -> String {
    email.to_lowercase()
}

/// The orgs a person is a member of (for audit fan-out).
async fn orgs_of(db: &Db, user: OperatorId) -> anyhow::Result<Vec<OrgId>> {
    let ids: Vec<String> = sqlx::query_scalar("SELECT org_id FROM memberships WHERE user_id = ?")
        .bind(user.to_string())
        .fetch_all(db.pool())
        .await?;
    ids.iter()
        .map(|s| Ok(OrgId(uuid::Uuid::parse_str(s)?)))
        .collect()
}

/// An enabled person with this email.
async fn user_by_email(db: &Db, email: &str) -> anyhow::Result<Option<OperatorId>> {
    let id: Option<String> =
        sqlx::query_scalar("SELECT id FROM users WHERE email = ? AND disabled_at IS NULL")
            .bind(email)
            .fetch_optional(db.pool())
            .await?;
    id.map(|s| Ok(OperatorId(uuid::Uuid::parse_str(&s)?)))
        .transpose()
}

/// Finish a successful sign-in: new session (old one revoked), audit, `Me` + cookie.
async fn signed_in(
    st: &PortalState,
    user: OperatorId,
    method: &str,
    client: &ClientInfo,
    headers: &HeaderMap,
    clear_nonce: bool,
) -> ApiResult<Response> {
    let cookie = start_session(st, user, client, headers).await?;
    st.audit.record(AuditEvent::PortalSignIn {
        user_id: user,
        method: method.to_owned(),
        ip: client.ip,
        orgs: orgs_of(&st.db, user).await?,
    });
    let me: Me = me::load(&st.db, user).await?;
    let mut resp = Json(me).into_response();
    resp.headers_mut().append(header::SET_COOKIE, cookie);
    if clear_nonce {
        resp.headers_mut()
            .append(header::SET_COOKIE, clear_cookie(NONCE_COOKIE));
    }
    Ok(resp)
}

async fn failed(
    st: &PortalState,
    email: &str,
    method: &str,
    reason: &str,
    user: Option<OperatorId>,
    client: &ClientInfo,
) -> anyhow::Result<()> {
    let orgs = match user {
        Some(u) => orgs_of(&st.db, u).await?,
        None => vec![],
    };
    st.audit.record(AuditEvent::PortalSignInFailed {
        email: email.chars().take(MAX_EMAIL_CHARS).collect(),
        method: method.to_owned(),
        reason: reason.to_owned(),
        ip: client.ip,
        orgs,
    });
    Ok(())
}

fn limit_sign_in(st: &PortalState, client: &ClientInfo, email: Option<&str>) -> ApiResult<()> {
    let l = &st.config.limits;
    if !st.limiter.hit(
        &format!("sign-in-ip:{}", ip_key(client.ip)),
        l.sign_in_per_ip,
    ) {
        return Err(ApiError::RateLimited);
    }
    if let Some(e) = email
        && !st.limiter.hit(
            &format!("sign-in-email:{}", email_key(e)),
            l.sign_in_per_email,
        )
    {
        return Err(ApiError::RateLimited);
    }
    Ok(())
}

// -- password ------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct PasswordBody {
    email: String,
    password: String,
}

/// `POST /auth/password` `{email, password}` → `Me` | 401 (unknown email = wrong password).
/// The email matches a person's email, or a login account's username.
pub async fn password(
    State(st): State<PortalState>,
    client: ClientInfo,
    headers: HeaderMap,
    ApiJson(body): ApiJson<PasswordBody>,
) -> ApiResult<Response> {
    let email = tidy_email(&body.email);
    limit_sign_in(&st, &client, Some(&email))?;
    let found = sqlx::query(
        "SELECT a.user_id, a.password_hash FROM accounts a JOIN users u ON u.id = a.user_id
         WHERE u.disabled_at IS NULL AND (u.email = ?1 OR a.username = ?1)
         ORDER BY (u.email = ?1) DESC LIMIT 1",
    )
    .bind(&email)
    .fetch_optional(st.db.pool())
    .await?;
    let (user, hash) = match &found {
        Some(r) => (
            Some(OperatorId(uuid::Uuid::parse_str(
                &r.get::<String, _>("user_id"),
            )?)),
            Some(r.get::<String, _>("password_hash")),
        ),
        None => (None, None),
    };
    let pw = body.password;
    let ok = pw.len() <= MAX_PASSWORD_BYTES
        && tokio::task::spawn_blocking(move || {
            crate::accounts::verify_password(hash.as_deref(), &pw)
        })
        .await?;
    match user {
        Some(u) if ok => signed_in(&st, u, "password", &client, &headers, false).await,
        _ => {
            let known = match user {
                Some(u) => Some(u),
                None => user_by_email(&st.db, &email).await?,
            };
            failed(
                &st,
                &email,
                "password",
                "wrong email or password",
                known,
                &client,
            )
            .await?;
            Err(ApiError::Unauthorized)
        }
    }
}

// -- magic link ----------------------------------------------------------------------

#[derive(Deserialize)]
pub struct MagicLinkBody {
    email: String,
}

/// `POST /auth/magic-link` `{email}` → 202, always (unless the request itself is bad or
/// rate-limited). Sets the nonce cookie that binds the link to this browser.
pub async fn magic_link(
    State(st): State<PortalState>,
    client: ClientInfo,
    headers: HeaderMap,
    ApiJson(body): ApiJson<MagicLinkBody>,
) -> ApiResult<Response> {
    let email = tidy_email(&body.email);
    if !plausible_email(&email) {
        return Err(ApiError::Invalid("that is not an email address".into()));
    }
    let l = &st.config.limits;
    if !st.limiter.hit(
        &format!("magic-ip:{}", ip_key(client.ip)),
        l.magic_link_per_ip,
    ) || !st.limiter.hit(
        &format!("magic-email:{}", email_key(&email)),
        l.magic_link_per_email,
    ) {
        return Err(ApiError::RateLimited);
    }
    // Keep this browser's nonce if it has one, so a second request doesn't unbind the first.
    let nonce = cookie(&headers, NONCE_COOKIE)
        .filter(|n| looks_like_token(n))
        .unwrap_or_else(random_token);
    let ttl = st.config.magic_link_ttl.as_secs();
    match st.config.origin() {
        None => tracing::warn!(
            "magic link asked for, but the server has no portal URL (--portal-url); not sent"
        ),
        Some(origin) => {
            let user = user_by_email(&st.db, &email).await?;
            if user.is_some() || st.config.allow_signup {
                let token = random_token();
                let code = format!("{:06}", rand::random_range(0..1_000_000u32));
                let now = crate::db::now();
                sqlx::query(
                    "INSERT INTO magic_links (token_sha256, email, user_id, code_sha256,
                       nonce_sha256, created_at, expires_at, ip)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(sha256_hex(&token))
                .bind(&email)
                .bind(user.map(|u| u.to_string()))
                .bind(code_hash(&token, &code))
                .bind(sha256_hex(&nonce))
                .bind(now)
                .bind(now + ttl as i64)
                .bind(client.ip.map(|i| i.to_string()))
                .execute(st.db.pool())
                .await?;
                st.send_mail(magic_mail(&email, origin, &token, &code, ttl));
            }
        }
    }
    let mut resp = (StatusCode::ACCEPTED, Json(serde_json::json!({}))).into_response();
    resp.headers_mut()
        .append(header::SET_COOKIE, set_cookie(NONCE_COOKIE, &nonce, ttl));
    Ok(resp)
}

fn code_hash(token: &str, code: &str) -> String {
    sha256_hex(&format!("{token}:{code}"))
}

fn magic_mail(to: &str, origin: &str, token: &str, code: &str, ttl: u64) -> Mail {
    let minutes = ttl.div_ceil(60);
    Mail {
        to: to.to_owned(),
        subject: "Sign in to Rconn".to_owned(),
        text: format!(
            "Open this link to sign in to the Rconn admin portal:\n\n\
             {origin}/admin/magic?token={token}\n\n\
             If you open it on another device or browser, enter this code: {} {}\n\n\
             The link works once, for {minutes} minutes. If you didn't ask to sign in, \
             ignore this email.\n",
            &code[..3],
            &code[3..]
        ),
    }
}

#[derive(Deserialize)]
pub struct MagicBody {
    token: String,
    code: Option<String>,
}

/// `POST /auth/magic` `{token, code?}` → `Me` | 400 `invalid` | 400 `needs_code`.
/// Only this POST uses the link; opening it (GET /admin/magic) changes nothing.
pub async fn magic(
    State(st): State<PortalState>,
    client: ClientInfo,
    headers: HeaderMap,
    ApiJson(body): ApiJson<MagicBody>,
) -> ApiResult<Response> {
    limit_sign_in(&st, &client, None)?;
    let dead = || ApiError::Invalid("this sign-in link expired or was already used".into());
    if !looks_like_token(&body.token) {
        return Err(dead());
    }
    let hash = sha256_hex(&body.token);
    let now = crate::db::now();
    let Some(row) = sqlx::query(
        "SELECT email, user_id, code_sha256, nonce_sha256 FROM magic_links
         WHERE token_sha256 = ? AND used_at IS NULL AND expires_at > ?",
    )
    .bind(&hash)
    .bind(now)
    .fetch_optional(st.db.pool())
    .await?
    else {
        return Err(dead());
    };
    let email: String = row.get("email");
    let same_browser = cookie(&headers, NONCE_COOKIE)
        .is_some_and(|n| sha256_hex(&n) == row.get::<String, _>("nonce_sha256"));
    if !same_browser {
        let Some(code) = body.code.as_deref() else {
            return Err(ApiError::NeedsCode);
        };
        let digits: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        // Counted atomically, so parallel guesses can't exceed the allowance.
        let attempts: i64 = sqlx::query_scalar(
            "UPDATE magic_links SET code_attempts = code_attempts + 1 WHERE token_sha256 = ?
             RETURNING code_attempts",
        )
        .bind(&hash)
        .fetch_one(st.db.pool())
        .await?;
        if attempts > MAX_CODE_ATTEMPTS {
            return Err(dead());
        }
        if code_hash(&body.token, &digits) != row.get::<String, _>("code_sha256") {
            if attempts >= MAX_CODE_ATTEMPTS {
                sqlx::query("UPDATE magic_links SET used_at = ? WHERE token_sha256 = ?")
                    .bind(now)
                    .bind(&hash)
                    .execute(st.db.pool())
                    .await?;
            }
            failed(&st, &email, "magic_link", "wrong code", None, &client).await?;
            return Err(ApiError::Invalid("wrong code".into()));
        }
    }
    // Single use: only one request can flip used_at.
    let claimed = sqlx::query(
        "UPDATE magic_links SET used_at = ?1
         WHERE token_sha256 = ?2 AND used_at IS NULL AND expires_at > ?1",
    )
    .bind(now)
    .bind(&hash)
    .execute(st.db.pool())
    .await?
    .rows_affected();
    if claimed != 1 {
        return Err(dead());
    }
    let user = match row.get::<Option<String>, _>("user_id") {
        Some(id) => Some(OperatorId(uuid::Uuid::parse_str(&id)?)),
        None => user_by_email(&st.db, &email).await?,
    };
    let user = match user {
        Some(u) => u,
        None if st.config.allow_signup => create_user(&st.db, &email).await?,
        None => return Err(dead()),
    };
    let enabled: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM users WHERE id = ? AND disabled_at IS NULL")
            .bind(user.to_string())
            .fetch_optional(st.db.pool())
            .await?;
    if enabled.is_none() {
        return Err(dead());
    }
    signed_in(&st, user, "magic_link", &client, &headers, true).await
}

/// A new person with a verified email (they just used a link sent to it).
async fn create_user(db: &Db, email: &str) -> anyhow::Result<OperatorId> {
    let mut tx = db.pool().begin().await?;
    let existing: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE email = ?")
        .bind(email)
        .fetch_optional(&mut *tx)
        .await?;
    let id = match existing {
        Some(id) => OperatorId(uuid::Uuid::parse_str(&id)?),
        None => {
            let id = OperatorId::new();
            crate::db::insert_user(&mut tx, id, Some(email), None).await?;
            id
        }
    };
    tx.commit().await?;
    Ok(id)
}

// -- sign-out and options ------------------------------------------------------------

/// `POST /auth/logout` → 204; the session is revoked server-side.
pub async fn logout(State(st): State<PortalState>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(c) = cookie(&headers, SESSION_COOKIE).filter(|c| looks_like_token(c)) {
        revoke(&st, &sha256_hex(&c)).await?;
    }
    let mut resp = StatusCode::NO_CONTENT.into_response();
    resp.headers_mut()
        .append(header::SET_COOKIE, clear_cookie(SESSION_COOKIE));
    Ok(resp)
}

#[derive(Deserialize)]
pub struct OptionsQuery {
    #[allow(dead_code)]
    email: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct SsoOption {
    pub org_id: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct LoginOptions {
    pub password: bool,
    pub magic_link: bool,
    pub sso: Option<SsoOption>,
    pub passkey: bool,
}

/// `GET /auth/options?email=` → the sign-in methods to show. The same for every email
/// on this server (company SSO by verified domain comes with SSO support).
pub async fn options(
    State(st): State<PortalState>,
    Query(_q): Query<OptionsQuery>,
) -> ApiResult<Json<LoginOptions>> {
    let accounts: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM accounts)")
        .fetch_one(st.db.pool())
        .await?;
    Ok(Json(LoginOptions {
        password: accounts,
        magic_link: st.config.origin().is_some(),
        sso: None,
        passkey: false,
    }))
}
