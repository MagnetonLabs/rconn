//! Passkeys: `/auth/passkey/register/{start,finish}`, `/auth/passkey/login/{start,finish}`,
//! `GET /me/passkeys`, `DELETE /me/passkeys/{id}`.
//!
//! WebAuthn through webauthn-rs. The relying party is the portal URL (`--portal-url`): its
//! host is the RP id and it is the only allowed origin. Without an `https` portal URL
//! passkeys are off (`/auth/options` says `passkey: false`, these endpoints answer 403).
//!
//! Bodies (binary fields base64url, as webauthn-rs serializes them): register start
//! `{name}` → `{publicKey}`; finish `{name, credential}` → `{id, name, created_at,
//! last_used}`; login start `{email}` → `{publicKey}`; finish `{credential}` → `Me`.
//!
//! Ceremony state stays on the server (`webauthn_challenges`), single use, five minutes.
//! Sign-in start answers the same way for any email: an address with no passkey gets a
//! challenge for a made-up credential (stable per email), which can't succeed.

use super::auth::{record_sign_in, sign_in_failed};
use super::me::{self, Me};
use super::ratelimit::{Limit, ip_key};
use super::security::{clear_cookie, cookie, set_cookie};
use super::session::{looks_like_token, random_token, sha256_hex, start_session};
use super::{
    ApiError, ApiJson, ApiResult, ClientInfo, CurrentUser, PortalConfig, PortalState, clean_name,
    rfc3339,
};
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{KeyInit, Mac};
use rconn_core::ids::OperatorId;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::time::Duration;
use webauthn_rs::prelude::{
    Passkey, PasskeyAuthentication, PasskeyRegistration, PublicKeyCredential,
    RegisterPublicKeyCredential, Url, Uuid, Webauthn, WebauthnBuilder,
};

/// Binds a passkey sign-in to the browser that started it.
pub const PASSKEY_COOKIE: &str = "__Host-rconn_passkey";
/// A ceremony must finish within this (seconds); webauthn-rs asks the browser for the same.
const CHALLENGE_TTL: i64 = 300;
const MAX_PASSKEYS: i64 = 20;
/// Passkey sign-ins started per client address and per email.
const LOGIN_PER_IP: Limit = Limit::new(30, Duration::from_secs(600));
const LOGIN_PER_EMAIL: Limit = Limit::new(20, Duration::from_secs(600));

/// The relying party for the portal URL, or `None` (passkeys off) without an https one.
pub fn relying_party(config: &PortalConfig) -> Option<Webauthn> {
    let origin = Url::parse(config.origin()?).ok()?;
    if origin.scheme() != "https" {
        return None;
    }
    let rp_id = origin.domain()?.to_owned();
    match WebauthnBuilder::new(&rp_id, &origin).and_then(|b| b.rp_name("Rconn").build()) {
        Ok(w) => Some(w),
        Err(e) => {
            tracing::warn!(%origin, error = %e, "passkeys off: the portal URL is not a WebAuthn origin");
            None
        }
    }
}

fn rp(st: &PortalState) -> ApiResult<&Webauthn> {
    st.passkeys.as_ref().ok_or(ApiError::Forbidden(
        "passkeys need the server's portal URL (--portal-url) to be https",
    ))
}

/// Routes under `/api/v1`.
pub fn routes() -> Router<PortalState> {
    Router::new()
        .route("/auth/passkey/register/start", post(register_start))
        .route("/auth/passkey/register/finish", post(register_finish))
        .route("/auth/passkey/login/start", post(login_start))
        .route("/auth/passkey/login/finish", post(login_finish))
        .route("/me/passkeys", get(list))
        .route("/me/passkeys/{id}", delete(remove))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PasskeyView {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub last_used: Option<String>,
}

struct Stored {
    id: String,
    passkey: Passkey,
}

async fn passkeys_of(st: &PortalState, user: OperatorId) -> anyhow::Result<Vec<Stored>> {
    let rows =
        sqlx::query("SELECT id, passkey FROM passkeys WHERE user_id = ? ORDER BY created_at")
            .bind(user.to_string())
            .fetch_all(st.db.pool())
            .await?;
    rows.iter()
        .map(|r| {
            Ok(Stored {
                id: r.get("id"),
                passkey: serde_json::from_str(&r.get::<String, _>("passkey"))?,
            })
        })
        .collect()
}

/// Store a ceremony's state under `key` (replacing an older one under the same key).
async fn put_challenge(
    st: &PortalState,
    key: &str,
    kind: &str,
    user: Option<OperatorId>,
    email: Option<&str>,
    state: &impl Serialize,
) -> anyhow::Result<()> {
    let now = crate::db::now();
    sqlx::query("DELETE FROM webauthn_challenges WHERE expires_at < ?")
        .bind(now)
        .execute(st.db.pool())
        .await?;
    sqlx::query(
        "INSERT OR REPLACE INTO webauthn_challenges (id_sha256, kind, user_id, email, state,
           expires_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(sha256_hex(key))
    .bind(kind)
    .bind(user.map(|u| u.to_string()))
    .bind(email)
    .bind(serde_json::to_string(state)?)
    .bind(now + CHALLENGE_TTL)
    .execute(st.db.pool())
    .await?;
    Ok(())
}

struct Challenge {
    user: Option<OperatorId>,
    email: Option<String>,
    state: String,
}

/// Take (single use) a live ceremony of `kind` stored under `key`.
async fn take_challenge(
    st: &PortalState,
    key: &str,
    kind: &str,
) -> anyhow::Result<Option<Challenge>> {
    let r = sqlx::query(
        "DELETE FROM webauthn_challenges WHERE id_sha256 = ? AND kind = ? AND expires_at > ?
         RETURNING user_id, email, state",
    )
    .bind(sha256_hex(key))
    .bind(kind)
    .bind(crate::db::now())
    .fetch_optional(st.db.pool())
    .await?;
    r.map(|r| {
        Ok(Challenge {
            user: r
                .get::<Option<String>, _>("user_id")
                .map(|u| uuid::Uuid::parse_str(&u).map(OperatorId))
                .transpose()?,
            email: r.get("email"),
            state: r.get("state"),
        })
    })
    .transpose()
}

/// Registrations are keyed by the sign-in that started them.
fn register_key(user: &CurrentUser) -> String {
    format!("register:{}", user.session_sha256)
}

// -- registration ----------------------------------------------------------------------

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

/// `POST /auth/passkey/register/start` `{name}` → WebAuthn creation options.
async fn register_start(
    State(st): State<PortalState>,
    user: CurrentUser,
    ApiJson(body): ApiJson<NameBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let rp = rp(&st)?;
    clean_name(&body.name, 64)?;
    let existing = passkeys_of(&st, user.id).await?;
    if existing.len() as i64 >= MAX_PASSKEYS {
        return Err(ApiError::Invalid(format!(
            "you can have at most {MAX_PASSKEYS} passkeys"
        )));
    }
    let exclude = existing
        .iter()
        .map(|s| s.passkey.cred_id().clone())
        .collect();
    let account = user.email.clone().unwrap_or_else(|| user.id.to_string());
    let display = user
        .name
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| account.clone());
    let (options, state) = rp
        .start_passkey_registration(user.id.0, &account, &display, Some(exclude))
        .map_err(|e| anyhow::anyhow!("starting passkey registration: {e}"))?;
    put_challenge(
        &st,
        &register_key(&user),
        "register",
        Some(user.id),
        None,
        &state,
    )
    .await?;
    Ok(Json(serde_json::to_value(options)?))
}

#[derive(Deserialize)]
struct RegisterFinish {
    name: String,
    credential: serde_json::Value,
}

/// `POST /auth/passkey/register/finish` `{name, credential}` → the new passkey.
async fn register_finish(
    State(st): State<PortalState>,
    user: CurrentUser,
    ApiJson(body): ApiJson<RegisterFinish>,
) -> ApiResult<Json<PasskeyView>> {
    let rp = rp(&st)?;
    let name = clean_name(&body.name, 64)?;
    let expired = || ApiError::Invalid("start adding the passkey again".into());
    let Some(ch) = take_challenge(&st, &register_key(&user), "register").await? else {
        return Err(expired());
    };
    if ch.user != Some(user.id) {
        return Err(expired());
    }
    let state: PasskeyRegistration = serde_json::from_str(&ch.state)?;
    let cred: RegisterPublicKeyCredential = serde_json::from_value(body.credential)
        .map_err(|_| ApiError::Invalid("that is not a passkey credential".into()))?;
    let passkey = rp.finish_passkey_registration(&cred, &state).map_err(|e| {
        tracing::info!(user = %user.id, error = %e, "passkey registration refused");
        ApiError::Invalid("the passkey couldn't be verified".into())
    })?;
    let id = Uuid::new_v4().to_string();
    let now = crate::db::now();
    let r = sqlx::query(
        "INSERT INTO passkeys (id, user_id, cred_id, name, passkey, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(user.id.to_string())
    .bind(B64.encode(passkey.cred_id()))
    .bind(&name)
    .bind(serde_json::to_string(&passkey)?)
    .bind(now)
    .execute(st.db.pool())
    .await;
    match r {
        Err(sqlx::Error::Database(d)) if d.is_unique_violation() => {
            return Err(ApiError::Conflict(
                "that passkey is already registered".into(),
            ));
        }
        r => r?,
    };
    Ok(Json(PasskeyView {
        id,
        name,
        created_at: rfc3339(now),
        last_used: None,
    }))
}

// -- sign-in ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct EmailBody {
    email: String,
}

/// A credential id that doesn't exist, the same for an email every time.
fn decoy_credential(st: &PortalState, email: &str) -> String {
    let key = st.master.derive("passkey-decoy");
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&key).expect("any key size");
    mac.update(email.to_lowercase().as_bytes());
    B64.encode(mac.finalize().into_bytes())
}

/// `POST /auth/passkey/login/start` `{email}` → WebAuthn request options (the same shape
/// for every email). Sets the cookie that binds the ceremony to this browser.
async fn login_start(
    State(st): State<PortalState>,
    client: ClientInfo,
    ApiJson(body): ApiJson<EmailBody>,
) -> ApiResult<Response> {
    let rp = rp(&st)?;
    let email = body.email.trim().to_owned();
    if email.is_empty() || email.chars().count() > 254 {
        return Err(ApiError::Invalid("that is not an email address".into()));
    }
    if !st
        .limiter
        .hit(&format!("passkey-ip:{}", ip_key(client.ip)), LOGIN_PER_IP)
        || !st.limiter.hit(
            &format!("passkey-email:{}", email.to_lowercase()),
            LOGIN_PER_EMAIL,
        )
    {
        return Err(ApiError::RateLimited);
    }
    let user: Option<String> =
        sqlx::query_scalar("SELECT id FROM users WHERE email = ? AND disabled_at IS NULL")
            .bind(&email)
            .fetch_optional(st.db.pool())
            .await?;
    let user = user
        .map(|u| uuid::Uuid::parse_str(&u).map(OperatorId))
        .transpose()?;
    let keys = match user {
        Some(u) => passkeys_of(&st, u).await?,
        None => vec![],
    };
    let keys: Vec<Passkey> = keys.into_iter().map(|s| s.passkey).collect();
    let (options, state, owner) = if keys.is_empty() {
        let (options, state) = rp
            .start_passkey_authentication(&[])
            .map_err(|e| anyhow::anyhow!("starting passkey sign-in: {e}"))?;
        let mut options = serde_json::to_value(options)?;
        if let Some(list) = options["publicKey"]["allowCredentials"].as_array_mut() {
            list.push(serde_json::json!({
                "type": "public-key",
                "id": decoy_credential(&st, &email),
                "transports": ["internal", "hybrid"],
            }));
        }
        (options, state, None)
    } else {
        let (options, state) = rp
            .start_passkey_authentication(&keys)
            .map_err(|e| anyhow::anyhow!("starting passkey sign-in: {e}"))?;
        (serde_json::to_value(options)?, state, user)
    };
    let token = random_token();
    put_challenge(&st, &token, "login", owner, Some(&email), &state).await?;
    let mut resp = Json(options).into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(PASSKEY_COOKIE, &token, CHALLENGE_TTL as u64),
    );
    Ok(resp)
}

#[derive(Deserialize)]
struct LoginFinish {
    credential: serde_json::Value,
}

/// `POST /auth/passkey/login/finish` `{credential}` → `Me` (sets the session cookie) | 401.
async fn login_finish(
    State(st): State<PortalState>,
    client: ClientInfo,
    headers: HeaderMap,
    ApiJson(body): ApiJson<LoginFinish>,
) -> ApiResult<Response> {
    let rp = rp(&st)?;
    let Some(token) = cookie(&headers, PASSKEY_COOKIE).filter(|c| looks_like_token(c)) else {
        return Err(ApiError::Unauthorized);
    };
    let Some(ch) = take_challenge(&st, &token, "login").await? else {
        return Err(ApiError::Unauthorized);
    };
    let email = ch.email.clone().unwrap_or_default();
    let refuse = |reason: &str, orgs| {
        sign_in_failed(&st, &email, "passkey", reason, orgs, &client);
        ApiError::Unauthorized
    };
    let Some(user) = ch.user else {
        return Err(refuse("no passkey for this email", vec![]));
    };
    let orgs = super::auth::orgs_of(&st.db, user).await?;
    let state: PasskeyAuthentication = serde_json::from_str(&ch.state)?;
    let Ok(cred) = serde_json::from_value::<PublicKeyCredential>(body.credential) else {
        return Err(refuse("not a passkey credential", orgs));
    };
    let result = match rp.finish_passkey_authentication(&cred, &state) {
        Ok(r) => r,
        Err(e) => {
            tracing::info!(%user, error = %e, "passkey sign-in refused");
            return Err(refuse("passkey didn't verify", orgs));
        }
    };
    // The credential is one of this person's (webauthn-rs only accepts the ones it was
    // started with); update its counter and backup state.
    let Some(mut stored) = passkeys_of(&st, user)
        .await?
        .into_iter()
        .find(|s| s.passkey.cred_id() == result.cred_id())
    else {
        return Err(refuse("passkey was removed", orgs));
    };
    stored.passkey.update_credential(&result);
    let enabled: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM users WHERE id = ? AND disabled_at IS NULL")
            .bind(user.to_string())
            .fetch_optional(st.db.pool())
            .await?;
    if enabled.is_none() {
        return Err(refuse("account disabled", orgs));
    }
    sqlx::query("UPDATE passkeys SET passkey = ?, last_used = ? WHERE id = ? AND user_id = ?")
        .bind(serde_json::to_string(&stored.passkey)?)
        .bind(crate::db::now())
        .bind(&stored.id)
        .bind(user.to_string())
        .execute(st.db.pool())
        .await?;
    let session = start_session(&st, user, &client, &headers).await?;
    record_sign_in(&st, user, "passkey", &client).await?;
    let me: Me = me::load(&st.db, user).await?;
    let mut resp = Json(me).into_response();
    resp.headers_mut().append(header::SET_COOKIE, session);
    resp.headers_mut()
        .append(header::SET_COOKIE, clear_cookie(PASSKEY_COOKIE));
    Ok(resp)
}

// -- managing ---------------------------------------------------------------------------

/// `GET /me/passkeys` → the signed-in person's passkeys.
async fn list(
    State(st): State<PortalState>,
    user: CurrentUser,
) -> ApiResult<Json<Vec<PasskeyView>>> {
    let rows = sqlx::query(
        "SELECT id, name, created_at, last_used FROM passkeys WHERE user_id = ?
         ORDER BY created_at, id",
    )
    .bind(user.id.to_string())
    .fetch_all(st.db.pool())
    .await?;
    Ok(Json(
        rows.iter()
            .map(|r| PasskeyView {
                id: r.get("id"),
                name: r.get("name"),
                created_at: rfc3339(r.get("created_at")),
                last_used: r.get::<Option<i64>, _>("last_used").map(rfc3339),
            })
            .collect(),
    ))
}

/// `DELETE /me/passkeys/{id}` → 204 | 404 (also for someone else's passkey).
async fn remove(
    State(st): State<PortalState>,
    user: CurrentUser,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let n = sqlx::query("DELETE FROM passkeys WHERE id = ? AND user_id = ?")
        .bind(&id)
        .bind(user.id.to_string())
        .execute(st.db.pool())
        .await?
        .rows_affected();
    if n == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}
