//! Portal sign-ins: the `__Host-rconn_session` cookie and the `sessions` table.
//!
//! The cookie holds 32 random bytes (base64url); the table holds only their SHA-256.
//! A sign-in ends when it is revoked (sign-out), reaches `session_ttl`, or goes unused for
//! `session_idle`. Every sign-in issues a new id and revokes the one the browser had.

use super::security::{cookie, set_cookie};
use super::{ApiError, PortalState};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, header};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use rconn_core::ids::OperatorId;
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::net::IpAddr;

pub const SESSION_COOKIE: &str = "__Host-rconn_session";
/// Binds a magic link to the browser that asked for it.
pub const NONCE_COOKIE: &str = "__Host-rconn_magic_nonce";

/// Refresh `last_seen` at most this often (seconds).
const TOUCH_EVERY: i64 = 60;
const MAX_USER_AGENT: usize = 256;

pub fn sha256_hex(s: &str) -> String {
    crate::store::hex(&Sha256::digest(s.as_bytes()))
}

/// 32 random bytes, base64url (43 characters).
pub fn random_token() -> String {
    B64.encode(rand::random::<[u8; 32]>())
}

/// A token we issued has exactly this shape; anything else is refused before hashing.
pub fn looks_like_token(s: &str) -> bool {
    s.len() == 43
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Who is asking: address (through the trusted proxy header when configured) and agent.
#[derive(Debug, Clone, Default)]
pub struct ClientInfo {
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
}

impl FromRequestParts<PortalState> for ClientInfo {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        st: &PortalState,
    ) -> Result<Self, Self::Rejection> {
        Ok(client_info(st, &parts.headers, &parts.extensions))
    }
}

fn client_info(
    st: &PortalState,
    headers: &HeaderMap,
    extensions: &axum::http::Extensions,
) -> ClientInfo {
    ClientInfo {
        ip: crate::client_ip(&st.app, headers, extensions),
        user_agent: headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.chars().take(MAX_USER_AGENT).collect()),
    }
}

/// The signed-in person. Extracting it answers 401 without a live sign-in.
#[derive(Debug, Clone)]
pub struct CurrentUser {
    pub id: OperatorId,
    pub email: Option<String>,
    pub name: Option<String>,
    /// SHA-256 of this browser's session id.
    pub session_sha256: String,
    pub client: ClientInfo,
}

impl FromRequestParts<PortalState> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, st: &PortalState) -> Result<Self, ApiError> {
        if let Some(u) = parts.extensions.get::<CurrentUser>() {
            return Ok(u.clone());
        }
        let raw = cookie(&parts.headers, SESSION_COOKIE)
            .filter(|c| looks_like_token(c))
            .ok_or(ApiError::Unauthorized)?;
        let hash = sha256_hex(&raw);
        let now = crate::db::now();
        let idle = st.config.session_idle.as_secs() as i64;
        let row = sqlx::query(
            "SELECT s.user_id, s.last_seen, u.email, u.display_name FROM sessions s
             JOIN users u ON u.id = s.user_id
             WHERE s.id_sha256 = ? AND s.revoked_at IS NULL AND s.expires_at > ?
               AND s.last_seen > ? AND u.disabled_at IS NULL",
        )
        .bind(&hash)
        .bind(now)
        .bind(now - idle)
        .fetch_optional(st.db.pool())
        .await?
        .ok_or(ApiError::Unauthorized)?;
        let last_seen: i64 = row.get("last_seen");
        if now - last_seen >= TOUCH_EVERY {
            sqlx::query("UPDATE sessions SET last_seen = ? WHERE id_sha256 = ?")
                .bind(now)
                .bind(&hash)
                .execute(st.db.pool())
                .await?;
        }
        let id: String = row.get("user_id");
        let user = CurrentUser {
            id: OperatorId(uuid::Uuid::parse_str(&id)?),
            email: row.get("email"),
            name: row.get("display_name"),
            session_sha256: hash,
            client: client_info(st, &parts.headers, &parts.extensions),
        };
        parts.extensions.insert(user.clone());
        Ok(user)
    }
}

/// Start a sign-in for `user`: revoke the session this browser had (`headers`), store a
/// new one, and return its `Set-Cookie`.
pub async fn start_session(
    st: &PortalState,
    user: OperatorId,
    client: &ClientInfo,
    headers: &HeaderMap,
) -> anyhow::Result<HeaderValue> {
    let now = crate::db::now();
    if let Some(old) = cookie(headers, SESSION_COOKIE).filter(|c| looks_like_token(c)) {
        revoke(st, &sha256_hex(&old)).await?;
    }
    let id = random_token();
    let ttl = st.config.session_ttl.as_secs();
    sqlx::query(
        "INSERT INTO sessions (id_sha256, user_id, created_at, last_seen, expires_at, ip, user_agent)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(sha256_hex(&id))
    .bind(user.to_string())
    .bind(now)
    .bind(now)
    .bind(now + ttl as i64)
    .bind(client.ip.map(|i| i.to_string()))
    .bind(client.user_agent.as_deref())
    .execute(st.db.pool())
    .await?;
    Ok(set_cookie(SESSION_COOKIE, &id, ttl))
}

/// End a sign-in by the SHA-256 of its id.
pub async fn revoke(st: &PortalState, session_sha256: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE sessions SET revoked_at = ? WHERE id_sha256 = ? AND revoked_at IS NULL")
        .bind(crate::db::now())
        .bind(session_sha256)
        .execute(st.db.pool())
        .await?;
    Ok(())
}

/// End every sign-in of a person (removed, disabled, password changed).
pub async fn revoke_all(st: &PortalState, user: OperatorId) -> anyhow::Result<()> {
    sqlx::query("UPDATE sessions SET revoked_at = ? WHERE user_id = ? AND revoked_at IS NULL")
        .bind(crate::db::now())
        .bind(user.to_string())
        .execute(st.db.pool())
        .await?;
    Ok(())
}
