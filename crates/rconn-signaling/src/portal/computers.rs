//! Computers and enrollment tokens (`/orgs/{org_id}/computers…`, `…/enrollment-tokens…`).
//!
//! Handlers take an [`OrgCtx`] (404 for non-members), call `ctx.require(Action::…)`,
//! and reach the database only through functions that take `&OrgCtx` and filter by
//! `ctx.org_id`. Admin actions call `ctx.audit(…)`; revoking also evicts the host from
//! the broker (`st.app.evict_host`).
//!
//! Agreed with the web app: `status=offline` leaves revoked computers out; listing
//! enrollment tokens needs admin; `expires_in_hours` is one of 1, 24, 168, 720; a missing
//! `max_uses` means unlimited.
//!
//! Where the server differs from the first contract draft: `hostname`, `os`, `version`
//! and `enrolled_at` are `null` when unknown (hosts from 0.1.x report no OS or version;
//! computers enrolled before the database have no enrollment date). `name` is never
//! null: the admin's name, else the hostname, else "Computer <short code>". `PATCH`
//! with `name: null` clears the admin's name. Renaming or moving a revoked computer is
//! 409. Creating a token answers 201.
//!
//! Enrollment: [`DbTokenEnroller`] serves the broker's `EnrollWithToken` (a host agent
//! started with `--enroll-token-file`): it counts a use, issues the certificate from the
//! token's org CA and records the computer in the token's group, in one transaction.

use super::org::{Action, OrgCtx};
use super::{ApiError, ApiJson, ApiResult, PortalState, clean_name, rfc3339};
use crate::audit::{AuditEvent, AuditSink, AuditTarget};
use crate::db::Db;
use crate::hosts::{TokenEnroller, TokenRefusal};
use crate::secrets::MasterKey;
use crate::store::hex;
use async_trait::async_trait;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use rconn_core::ids::{HostId, OperatorId, OrgId};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Arc;
use uuid::Uuid;

/// Longest admin-given computer name.
pub const MAX_COMPUTER_NAME: usize = 64;
/// Longest token label.
pub const MAX_TOKEN_LABEL: usize = 100;
/// Token lifetimes the portal offers, in hours.
pub const TOKEN_EXPIRY_HOURS: [i64; 4] = [1, 24, 168, 720];
/// Largest `max_uses`.
pub const MAX_TOKEN_USES: i64 = 100_000;
/// Prefix of enrollment token secrets (so they are recognisable in a file or a leak scan).
pub const TOKEN_PREFIX: &str = "rcet_";

/// Routes under `/orgs/{org_id}`.
pub fn routes() -> Router<PortalState> {
    Router::new()
        .route("/computers", get(list_computers))
        .route("/computers/{id}", patch(patch_computer))
        .route("/computers/{id}/revoke", post(revoke_computer))
        .route("/enrollment-tokens", get(list_tokens).post(create_token))
        .route("/enrollment-tokens/{id}", delete(delete_token))
}

// -- views ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ComputerView {
    pub id: String,
    pub short_code: String,
    pub name: String,
    pub hostname: Option<String>,
    pub os: Option<String>,
    pub version: Option<String>,
    pub group_id: Option<String>,
    pub online: bool,
    pub last_seen: Option<String>,
    pub enrolled_at: Option<String>,
    pub enrolled_by: Option<String>,
    pub revoked_at: Option<String>,
    pub needs_update: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TokenView {
    pub id: String,
    pub label: String,
    pub group_id: Option<String>,
    pub expires_at: String,
    pub uses: i64,
    pub max_uses: Option<i64>,
    pub created_by: String,
    pub created_at: String,
}

/// A token as created: the only time its secret is returned.
#[derive(Debug, Clone, Serialize)]
pub struct NewTokenView {
    #[serde(flatten)]
    pub view: TokenView,
    pub token: String,
}

/// One inventory row with the enrolling person's label.
struct ComputerRow {
    id: HostId,
    admin_name: Option<String>,
    hostname: Option<String>,
    os: Option<String>,
    version: Option<String>,
    group_id: Option<String>,
    last_seen: Option<i64>,
    enrolled_at: Option<i64>,
    enrolled_by: Option<String>,
    revoked_at: Option<i64>,
}

impl ComputerRow {
    fn name(&self) -> String {
        self.admin_name
            .clone()
            .or_else(|| self.hostname.clone())
            .unwrap_or_else(|| format!("Computer {}", self.id.short_code()))
    }

    fn view(&self, online: &HashSet<HostId>, release: &str) -> ComputerView {
        let revoked = self.revoked_at.is_some();
        ComputerView {
            id: self.id.to_string(),
            short_code: self.id.short_code(),
            name: self.name(),
            hostname: self.hostname.clone(),
            os: self.os.clone(),
            version: self.version.clone(),
            group_id: self.group_id.clone(),
            online: !revoked && online.contains(&self.id),
            last_seen: self.last_seen.map(rfc3339),
            enrolled_at: self.enrolled_at.map(rfc3339),
            enrolled_by: self.enrolled_by.clone(),
            revoked_at: self.revoked_at.map(rfc3339),
            needs_update: !revoked
                && match &self.version {
                    Some(v) => is_older(v, release),
                    // Connected without saying its version: a host from before 0.1.12.
                    None => self.last_seen.is_some(),
                },
        }
    }

    fn matches(&self, q: &str) -> bool {
        let q = q.to_lowercase();
        let digits: String = q
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .collect();
        [&self.admin_name, &self.hostname]
            .into_iter()
            .flatten()
            .any(|s| s.to_lowercase().contains(&q))
            || self.id.to_string().contains(&q)
            || (!digits.is_empty()
                && digits.chars().all(|c| c.is_ascii_digit())
                && self.id.short_code().replace(' ', "").contains(&digits))
    }

    fn target(&self) -> AuditTarget {
        AuditTarget {
            kind: "computer".into(),
            id: self.id.to_string(),
            label: Some(self.name()),
        }
    }
}

/// Is `version` older than `release`? Dotted numbers (`0.1.13`), a leading `v` and any
/// `-pre`/`+build` suffix ignored. Anything unparseable is never "older".
pub fn is_older(version: &str, release: &str) -> bool {
    fn parse(s: &str) -> Option<Vec<u64>> {
        let s = s.trim().trim_start_matches(['v', 'V']);
        let core = s.split(['-', '+', ' ']).next()?;
        core.split('.').map(|p| p.parse().ok()).collect()
    }
    let (Some(mut a), Some(mut b)) = (parse(version), parse(release)) else {
        return false;
    };
    let n = a.len().max(b.len());
    a.resize(n, 0);
    b.resize(n, 0);
    a < b
}

// -- repository ----------------------------------------------------------------------

macro_rules! computer_select {
    () => {
        "SELECT h.id, h.admin_name, h.hostname, h.os, h.version, h.group_id,
        h.last_seen, h.enrolled_at, h.revoked_at,
        COALESCE(u.email, u.display_name, h.enrolled_by) AS enrolled_by_label
     FROM hosts h LEFT JOIN users u ON u.id = h.enrolled_by
     WHERE h.org_id = ?"
    };
}

fn computer_row(r: &sqlx::sqlite::SqliteRow) -> anyhow::Result<ComputerRow> {
    Ok(ComputerRow {
        id: HostId(Uuid::parse_str(r.get("id"))?),
        admin_name: r.get("admin_name"),
        hostname: r.get("hostname"),
        os: r.get("os"),
        version: r.get("version"),
        group_id: r.get("group_id"),
        last_seen: r.get("last_seen"),
        enrolled_at: r.get("enrolled_at"),
        enrolled_by: r.get("enrolled_by_label"),
        revoked_at: r.get("revoked_at"),
    })
}

async fn computers(db: &Db, ctx: &OrgCtx) -> anyhow::Result<Vec<ComputerRow>> {
    let rows = sqlx::query(computer_select!())
        .bind(ctx.org_id.to_string())
        .fetch_all(db.pool())
        .await?;
    rows.iter().map(computer_row).collect()
}

async fn computer(db: &Db, ctx: &OrgCtx, id: HostId) -> anyhow::Result<Option<ComputerRow>> {
    let row = sqlx::query(concat!(computer_select!(), " AND h.id = ?"))
        .bind(ctx.org_id.to_string())
        .bind(id.to_string())
        .fetch_optional(db.pool())
        .await?;
    row.as_ref().map(computer_row).transpose()
}

/// Whether `group` is a computer group of the caller's org.
async fn group_in_org(db: &Db, ctx: &OrgCtx, group: &str) -> anyhow::Result<bool> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM host_groups WHERE org_id = ? AND id = ?")
        .bind(ctx.org_id.to_string())
        .bind(group)
        .fetch_one(db.pool())
        .await?;
    Ok(n > 0)
}

async fn set_computer(
    db: &Db,
    ctx: &OrgCtx,
    id: HostId,
    name: Option<Option<&str>>,
    group: Option<Option<&str>>,
) -> anyhow::Result<()> {
    let mut tx = db.pool().begin().await?;
    if let Some(name) = name {
        sqlx::query("UPDATE hosts SET admin_name = ? WHERE org_id = ? AND id = ?")
            .bind(name)
            .bind(ctx.org_id.to_string())
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
    }
    if let Some(group) = group {
        sqlx::query("UPDATE hosts SET group_id = ? WHERE org_id = ? AND id = ?")
            .bind(group)
            .bind(ctx.org_id.to_string())
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Mark the computer revoked. Returns whether it was live until now.
async fn mark_revoked(db: &Db, ctx: &OrgCtx, id: HostId) -> anyhow::Result<bool> {
    let n = sqlx::query(
        "UPDATE hosts SET revoked_at = ? WHERE org_id = ? AND id = ? AND revoked_at IS NULL",
    )
    .bind(crate::db::now())
    .bind(ctx.org_id.to_string())
    .bind(id.to_string())
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(n == 1)
}

macro_rules! token_select {
    () => {
        "SELECT t.id, t.label, t.group_id, t.expires_at, t.uses, t.max_uses,
        t.created_at, COALESCE(u.email, u.display_name, t.created_by) AS created_by_label
     FROM enrollment_tokens t LEFT JOIN users u ON u.id = t.created_by
     WHERE t.org_id = ?"
    };
}

fn token_row(r: &sqlx::sqlite::SqliteRow) -> TokenView {
    TokenView {
        id: r.get("id"),
        label: r.get("label"),
        group_id: r.get("group_id"),
        expires_at: rfc3339(r.get("expires_at")),
        uses: r.get("uses"),
        max_uses: r.get("max_uses"),
        created_by: r.get("created_by_label"),
        created_at: rfc3339(r.get("created_at")),
    }
}

async fn tokens(db: &Db, ctx: &OrgCtx) -> anyhow::Result<Vec<TokenView>> {
    let rows = sqlx::query(concat!(
        token_select!(),
        " ORDER BY t.created_at DESC, t.id"
    ))
    .bind(ctx.org_id.to_string())
    .fetch_all(db.pool())
    .await?;
    Ok(rows.iter().map(token_row).collect())
}

async fn token(db: &Db, ctx: &OrgCtx, id: &str) -> anyhow::Result<Option<TokenView>> {
    let row = sqlx::query(concat!(token_select!(), " AND t.id = ?"))
        .bind(ctx.org_id.to_string())
        .bind(id)
        .fetch_optional(db.pool())
        .await?;
    Ok(row.as_ref().map(token_row))
}

fn token_digest(secret: &str) -> String {
    hex(&Sha256::digest(secret.trim().as_bytes()))
}

// -- handlers ------------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub struct ListQuery {
    q: Option<String>,
    group_id: Option<String>,
    status: Option<String>,
}

/// `GET /computers?q=&group_id=&status=online|offline|revoked`, sorted by name.
pub async fn list_computers(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    query: Result<Query<ListQuery>, axum::extract::rejection::QueryRejection>,
) -> ApiResult<Json<Vec<ComputerView>>> {
    ctx.require(Action::View)?;
    let Query(f) = query.map_err(|e| ApiError::Invalid(e.body_text()))?;
    let nonempty = |s: Option<String>| s.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    let (q, group, status) = (nonempty(f.q), nonempty(f.group_id), nonempty(f.status));
    if let Some(s) = &status
        && !["online", "offline", "revoked"].contains(&s.as_str())
    {
        return Err(ApiError::Invalid(
            "status must be online, offline or revoked".into(),
        ));
    }
    let online = st.app.online_in(ctx.org_id).await;
    let release = &st.config.host_release;
    let mut out: Vec<(String, ComputerView)> = computers(&st.db, &ctx)
        .await?
        .into_iter()
        .filter(|c| group.is_none() || c.group_id == group)
        .filter(|c| q.as_deref().is_none_or(|q| c.matches(q)))
        .map(|c| (c.name().to_lowercase(), c.view(&online, release)))
        .filter(|(_, v)| match status.as_deref() {
            Some("online") => v.online,
            Some("offline") => !v.online && v.revoked_at.is_none(),
            Some("revoked") => v.revoked_at.is_some(),
            _ => true,
        })
        .collect();
    out.sort_by(|a, b| (&a.0, &a.1.id).cmp(&(&b.0, &b.1.id)));
    Ok(Json(out.into_iter().map(|(_, v)| v).collect()))
}

/// Present (possibly `null`) vs absent, for PATCH bodies.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

#[derive(Deserialize)]
pub struct PatchComputer {
    #[serde(default, deserialize_with = "present")]
    name: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    group_id: Option<Option<String>>,
}

fn path_id(params: &HashMap<String, String>) -> ApiResult<Uuid> {
    params
        .get("id")
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or(ApiError::NotFound)
}

async fn load_view(st: &PortalState, ctx: &OrgCtx, id: HostId) -> ApiResult<ComputerView> {
    let c = computer(&st.db, ctx, id).await?.ok_or(ApiError::NotFound)?;
    let online = st.app.online_in(ctx.org_id).await;
    Ok(c.view(&online, &st.config.host_release))
}

/// `PATCH /computers/{id}` `{name?, group_id?}` (admin). A group of another org is 404.
pub async fn patch_computer(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    Path(params): Path<HashMap<String, String>>,
    ApiJson(body): ApiJson<PatchComputer>,
) -> ApiResult<Json<ComputerView>> {
    ctx.require(Action::ManageFleet)?;
    let id = HostId(path_id(&params)?);
    let before = computer(&st.db, &ctx, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if before.revoked_at.is_some() {
        return Err(ApiError::Conflict("this computer was revoked".into()));
    }
    let name = match &body.name {
        Some(Some(n)) => Some(Some(clean_name(n, MAX_COMPUTER_NAME)?)),
        Some(None) => Some(None),
        None => None,
    };
    let group = match &body.group_id {
        Some(Some(g)) if !g.is_empty() => {
            if !group_in_org(&st.db, &ctx, g).await? {
                return Err(ApiError::NotFound);
            }
            Some(Some(g.clone()))
        }
        Some(_) => Some(None),
        None => None,
    };
    set_computer(
        &st.db,
        &ctx,
        id,
        name.as_ref().map(|n| n.as_deref()),
        group.as_ref().map(|g| g.as_deref()),
    )
    .await?;
    let after = computer(&st.db, &ctx, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if name.is_some() && after.admin_name != before.admin_name {
        ctx.audit(
            &st,
            "computer.renamed",
            Some(after.target()),
            serde_json::json!({ "from": before.name(), "to": after.name() }),
        );
    }
    if group.is_some() && after.group_id != before.group_id {
        ctx.audit(
            &st,
            "computer.moved",
            Some(after.target()),
            serde_json::json!({ "from": before.group_id, "to": after.group_id }),
        );
    }
    let online = st.app.online_in(ctx.org_id).await;
    Ok(Json(after.view(&online, &st.config.host_release)))
}

/// `POST /computers/{id}/revoke` (admin): refused from now on, live sessions end now.
/// Revoking a revoked computer changes nothing.
pub async fn revoke_computer(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    Path(params): Path<HashMap<String, String>>,
) -> ApiResult<Json<ComputerView>> {
    ctx.require(Action::ManageFleet)?;
    let id = HostId(path_id(&params)?);
    let c = computer(&st.db, &ctx, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if mark_revoked(&st.db, &ctx, id).await? {
        // The reload loop would catch this within seconds; act now instead.
        st.app.refuse_host(id);
        st.app.evict_host(id).await;
        ctx.audit(
            &st,
            "computer.revoked",
            Some(c.target()),
            serde_json::json!({}),
        );
    }
    Ok(Json(load_view(&st, &ctx, id).await?))
}

/// `GET /enrollment-tokens` (admin), newest first, expired and used-up ones included.
pub async fn list_tokens(
    State(st): State<PortalState>,
    ctx: OrgCtx,
) -> ApiResult<Json<Vec<TokenView>>> {
    ctx.require(Action::ManageFleet)?;
    Ok(Json(tokens(&st.db, &ctx).await?))
}

#[derive(Deserialize)]
pub struct CreateToken {
    label: String,
    #[serde(default)]
    group_id: Option<String>,
    expires_in_hours: i64,
    #[serde(default)]
    max_uses: Option<i64>,
}

/// `POST /enrollment-tokens` `{label, group_id?, expires_in_hours, max_uses?}` (admin) →
/// 201 the token with its secret (shown this once).
pub async fn create_token(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    ApiJson(body): ApiJson<CreateToken>,
) -> ApiResult<Response> {
    ctx.require(Action::ManageFleet)?;
    let label = clean_name(&body.label, MAX_TOKEN_LABEL)?;
    if !TOKEN_EXPIRY_HOURS.contains(&body.expires_in_hours) {
        return Err(ApiError::Invalid(
            "expires_in_hours must be 1, 24, 168 or 720".into(),
        ));
    }
    if let Some(n) = body.max_uses
        && !(1..=MAX_TOKEN_USES).contains(&n)
    {
        return Err(ApiError::Invalid(format!(
            "max_uses must be 1 to {MAX_TOKEN_USES}"
        )));
    }
    let group = body.group_id.filter(|g| !g.is_empty());
    if let Some(g) = &group
        && !group_in_org(&st.db, &ctx, g).await?
    {
        return Err(ApiError::NotFound);
    }
    let secret = format!("{TOKEN_PREFIX}{}", hex(&rand::random::<[u8; 32]>()));
    let id = Uuid::new_v4().to_string();
    let now = crate::db::now();
    sqlx::query(
        "INSERT INTO enrollment_tokens
           (id, org_id, label, token_sha256, group_id, expires_at, max_uses, created_by, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(ctx.org_id.to_string())
    .bind(&label)
    .bind(token_digest(&secret))
    .bind(group.as_deref())
    .bind(now + body.expires_in_hours * 3600)
    .bind(body.max_uses)
    .bind(ctx.user.id.to_string())
    .bind(now)
    .execute(st.db.pool())
    .await?;
    ctx.audit(
        &st,
        "token.created",
        Some(AuditTarget {
            kind: "token".into(),
            id: id.clone(),
            label: Some(label),
        }),
        serde_json::json!({
            "group_id": group,
            "expires_in_hours": body.expires_in_hours,
            "max_uses": body.max_uses,
        }),
    );
    let view = token(&st.db, &ctx, &id).await?.ok_or(ApiError::NotFound)?;
    Ok((
        StatusCode::CREATED,
        Json(NewTokenView {
            view,
            token: secret,
        }),
    )
        .into_response())
}

/// `DELETE /enrollment-tokens/{id}` (admin) → 204. Computers enrolled with it stay.
pub async fn delete_token(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    Path(params): Path<HashMap<String, String>>,
) -> ApiResult<StatusCode> {
    ctx.require(Action::ManageFleet)?;
    let id = path_id(&params)?.to_string();
    let t = token(&st.db, &ctx, &id).await?.ok_or(ApiError::NotFound)?;
    sqlx::query("DELETE FROM enrollment_tokens WHERE org_id = ? AND id = ?")
        .bind(ctx.org_id.to_string())
        .bind(&id)
        .execute(st.db.pool())
        .await?;
    ctx.audit(
        &st,
        "token.deleted",
        Some(AuditTarget {
            kind: "token".into(),
            id,
            label: Some(t.label),
        }),
        serde_json::json!({ "uses": t.uses }),
    );
    Ok(StatusCode::NO_CONTENT)
}

// -- enrollment ----------------------------------------------------------------------

/// The broker's [`TokenEnroller`]: the portal's tokens and org CAs, in the database.
pub struct DbTokenEnroller {
    db: Db,
    master: Arc<MasterKey>,
    audit: Arc<dyn AuditSink>,
}

impl DbTokenEnroller {
    pub fn new(db: Db, master: Arc<MasterKey>, audit: Arc<dyn AuditSink>) -> Self {
        Self { db, master, audit }
    }

    async fn try_enroll(
        &self,
        secret: &str,
        host_id: HostId,
        public_key_der: &[u8],
        source: Option<IpAddr>,
    ) -> Result<(OrgId, String), TokenRefusal> {
        let internal = |e: anyhow::Error| {
            tracing::error!(error = %format!("{e:#}"), "token enrollment failed");
            TokenRefusal::Internal
        };
        if secret.len() > 256 {
            return Err(TokenRefusal::Invalid);
        }
        let now = crate::db::now();
        let mut tx = self
            .db
            .pool()
            .begin()
            .await
            .map_err(|e| internal(e.into()))?;
        // Count the use first, in the transaction: concurrent enrollments can't overrun
        // max_uses, and a failure below rolls the count back.
        let row = sqlx::query(
            "UPDATE enrollment_tokens SET uses = uses + 1
             WHERE token_sha256 = ? AND expires_at > ? AND (max_uses IS NULL OR uses < max_uses)
             RETURNING id, org_id, label, group_id, created_by",
        )
        .bind(token_digest(secret))
        .bind(now)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| internal(e.into()))?
        .ok_or(TokenRefusal::Invalid)?;
        let token_id: String = row.get("id");
        let label: String = row.get("label");
        let group: Option<String> = row.get("group_id");
        let created_by: String = row.get("created_by");
        let org = Uuid::parse_str(row.get("org_id"))
            .map(OrgId)
            .map_err(|e| internal(e.into()))?;
        let ca = self.db.org_ca(org, &self.master).await.map_err(internal)?;
        let cert = ca
            .issue_host_cert_for_public_key(host_id, public_key_der)
            .map_err(|_| TokenRefusal::BadKey)?;
        sqlx::query(
            "INSERT INTO hosts (id, org_id, enrolled_at, enrolled_by, group_id)
             VALUES (?1, ?2, ?3, ?4, (SELECT id FROM host_groups WHERE id = ?5 AND org_id = ?2))",
        )
        .bind(host_id.to_string())
        .bind(org.to_string())
        .bind(now)
        .bind(&created_by)
        .bind(group.as_deref())
        .execute(&mut *tx)
        .await
        .map_err(|e| internal(e.into()))?;
        tx.commit().await.map_err(|e| internal(e.into()))?;
        if let Ok(actor) = Uuid::parse_str(&created_by) {
            self.audit.record(AuditEvent::PortalAction {
                org_id: org,
                actor_id: OperatorId(actor),
                kind: "computer.enrolled".into(),
                target: Some(AuditTarget {
                    kind: "computer".into(),
                    id: host_id.to_string(),
                    label: Some(host_id.short_code()),
                }),
                ip: source,
                detail: serde_json::json!({
                    "via": "enrollment_token",
                    "token_id": token_id,
                    "token_label": label,
                    "group_id": group,
                }),
            });
        }
        Ok((org, cert))
    }
}

#[async_trait]
impl TokenEnroller for DbTokenEnroller {
    async fn enroll(
        &self,
        token: &str,
        host_id: HostId,
        public_key_der: &[u8],
        source: Option<IpAddr>,
    ) -> Result<(OrgId, String), TokenRefusal> {
        self.try_enroll(token.trim(), host_id, public_key_der, source)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(is_older("0.1.9", "0.1.14"));
        assert!(is_older("0.1.13", "0.1.14"));
        assert!(is_older("v0.1", "0.1.1"));
        assert!(!is_older("0.1.14", "0.1.14"));
        assert!(!is_older("0.1.14-dev", "0.1.14"));
        assert!(!is_older("0.2.0", "0.1.14"));
        assert!(!is_older("weird", "0.1.14"), "unparseable is never older");
    }
}
