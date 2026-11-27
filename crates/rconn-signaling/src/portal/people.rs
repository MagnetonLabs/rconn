//! People and invites (`/orgs/{org_id}/people…`, `…/invites…`, and the top-level
//! `/invites/accept`), plus who may use the desktop app on org computers
//! ([`desktop_access`]).
//!
//! Handlers take an [`OrgCtx`], check `ctx.require_manage_member(current, new)` (admins
//! can't touch owners), and query only through functions that take `&OrgCtx`. Invite
//! mail goes out with `st.send_mail`; `invites/accept` answers `me::load`. Removing a
//! person also ends their portal sign-ins (`session::revoke_all`), revokes their operator
//! tokens for the org, and drops their live desktop sockets (`AppState::evict_operator`).
//!
//! Agreed with the web app: any member may list people; listing invites needs admin.
//! Admins can't change or remove owners, nor make anyone owner (403); removing or
//! demoting the last owner is 409 `conflict`. `invites/accept` needs a session (401 sends
//! the app to sign-in) and an invite for another email is 403; the page is
//! `/admin/invite?token=`. Inviting someone already in the company, or an email with a
//! pending invite, is 409.
//!
//! Invite tokens are 32 random bytes; only their SHA-256 is stored. An invite works once,
//! for [`INVITE_TTL`].

use super::auth::plausible_email;
use super::org::{Action, OrgCtx};
use super::ratelimit::Limit;
use super::session::{looks_like_token, random_token, revoke_all, sha256_hex};
use super::{ApiError, ApiJson, ApiResult, CurrentUser, PortalState, me, rfc3339};
use crate::audit::AuditTarget;
use crate::auth::{OperatorIdentity, OrgAccess};
use crate::db::{Db, Role};
use crate::mail::Mail;
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use rconn_core::ids::{OperatorId, OrgId};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::time::Duration;

/// How long an invite link works.
pub const INVITE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// Invites one company may send.
pub const INVITES_PER_ORG: Limit = Limit::new(50, Duration::from_secs(3600));

const GONE: &str = "This invite has expired, was cancelled or was already used.";
const LAST_OWNER: &str = "A company needs at least one owner.";

/// Routes under `/orgs/{org_id}`.
pub fn routes() -> Router<PortalState> {
    Router::new()
        .route("/people", get(list_people))
        .route(
            "/people/{user_id}",
            patch(change_role).delete(remove_person),
        )
        .route("/invites", get(list_invites).post(create_invite))
        .route("/invites/{id}", delete(cancel_invite))
}

/// Routes under `/api/v1` that aren't org-scoped.
pub fn public_routes() -> Router<PortalState> {
    Router::new().route("/invites/accept", post(accept_invite))
}

// -- desktop authorization -----------------------------------------------------------

/// What `op` may do with their org's computers from the desktop app: their role in the
/// org decides (viewer lists; technician and up connect). Someone the database knows but
/// who isn't a member (removed) or is disabled gets nothing. An identity the database has
/// never seen (operators of the server-wide identity provider) keeps the 0.1.x rule in
/// the legacy org: every signed-in operator connects.
pub async fn desktop_access(db: &Db, op: OperatorIdentity) -> anyhow::Result<OrgAccess> {
    let row = sqlx::query(
        "SELECT u.disabled_at,
                (SELECT role FROM memberships WHERE user_id = u.id AND org_id = ?) AS role
         FROM users u WHERE u.id = ?",
    )
    .bind(op.org_id.to_string())
    .bind(op.operator_id.to_string())
    .fetch_optional(db.pool())
    .await?;
    let Some(row) = row else {
        let legacy: Option<i64> = sqlx::query_scalar("SELECT legacy FROM orgs WHERE id = ?")
            .bind(op.org_id.to_string())
            .fetch_optional(db.pool())
            .await?;
        return Ok(if legacy == Some(1) {
            OrgAccess::Connect
        } else {
            OrgAccess::None
        });
    };
    if row.get::<Option<i64>, _>("disabled_at").is_some() {
        return Ok(OrgAccess::None);
    }
    let role: Option<String> = row.get("role");
    Ok(match role.map(|r| r.parse::<Role>()).transpose()? {
        Some(r) if r.at_least(Action::Connect.min_role()) => OrgAccess::Connect,
        Some(r) if r.at_least(Action::View.min_role()) => OrgAccess::List,
        _ => OrgAccess::None,
    })
}

// -- repository ----------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Person {
    pub user_id: String,
    /// Empty for people without one (logins imported from 0.1.x accounts).
    pub email: String,
    pub name: String,
    pub role: Role,
    pub joined_at: String,
    /// Their latest portal activity.
    pub last_active: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InviteView {
    pub id: String,
    pub email: String,
    pub role: Role,
    /// The inviter's email (or name), empty if they are gone.
    pub invited_by: String,
    pub expires_at: String,
}

/// The org's members, or one of them.
async fn people(db: &Db, ctx: &OrgCtx, only: Option<OperatorId>) -> anyhow::Result<Vec<Person>> {
    let rows = sqlx::query(
        "SELECT u.id, u.email, u.display_name, m.role, m.created_at,
                (SELECT MAX(s.last_seen) FROM sessions s WHERE s.user_id = u.id) AS last_active
         FROM memberships m JOIN users u ON u.id = m.user_id
         WHERE m.org_id = ?1 AND (?2 IS NULL OR m.user_id = ?2)
         ORDER BY COALESCE(u.display_name, u.email, ''), u.id",
    )
    .bind(ctx.org_id.to_string())
    .bind(only.map(|u| u.to_string()))
    .fetch_all(db.pool())
    .await?;
    rows.iter()
        .map(|r| {
            Ok(Person {
                user_id: r.get("id"),
                email: r.get::<Option<String>, _>("email").unwrap_or_default(),
                name: r
                    .get::<Option<String>, _>("display_name")
                    .unwrap_or_default(),
                role: r.get::<String, _>("role").parse()?,
                joined_at: rfc3339(r.get("created_at")),
                last_active: r.get::<Option<i64>, _>("last_active").map(rfc3339),
            })
        })
        .collect()
}

async fn person(db: &Db, ctx: &OrgCtx, user: OperatorId) -> anyhow::Result<Option<Person>> {
    Ok(people(db, ctx, Some(user)).await?.pop())
}

/// Set a member's role, unless that would leave the org without an owner (checked in the
/// same statement, so two owners demoting each other can't both win). False if refused.
async fn set_role(db: &Db, ctx: &OrgCtx, user: OperatorId, role: Role) -> anyhow::Result<bool> {
    let n = sqlx::query(
        "UPDATE memberships SET role = ?3 WHERE org_id = ?1 AND user_id = ?2
           AND (role != 'owner' OR ?3 = 'owner'
                OR (SELECT COUNT(*) FROM memberships WHERE org_id = ?1 AND role = 'owner') > 1)",
    )
    .bind(ctx.org_id.to_string())
    .bind(user.to_string())
    .bind(role.as_str())
    .execute(db.pool())
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// Remove a member and revoke their operator tokens for the org, unless they are the
/// last owner. False if refused.
async fn remove_member(db: &Db, ctx: &OrgCtx, user: OperatorId) -> anyhow::Result<bool> {
    let mut tx = db.pool().begin().await?;
    let n = sqlx::query(
        "DELETE FROM memberships WHERE org_id = ?1 AND user_id = ?2
           AND (role != 'owner'
                OR (SELECT COUNT(*) FROM memberships WHERE org_id = ?1 AND role = 'owner') > 1)",
    )
    .bind(ctx.org_id.to_string())
    .bind(user.to_string())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n != 1 {
        return Ok(false);
    }
    // Revoked rather than left to the membership join, so inviting them back later
    // doesn't bring old tokens back to life.
    sqlx::query(
        "UPDATE api_tokens SET revoked_at = ? WHERE org_id = ? AND user_id = ?
           AND revoked_at IS NULL",
    )
    .bind(crate::db::now())
    .bind(ctx.org_id.to_string())
    .bind(user.to_string())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

fn invite_view(r: &sqlx::sqlite::SqliteRow) -> anyhow::Result<InviteView> {
    Ok(InviteView {
        id: r.get("id"),
        email: r.get("email"),
        role: r.get::<String, _>("role").parse()?,
        invited_by: r.get("invited_by"),
        expires_at: rfc3339(r.get("expires_at")),
    })
}

/// Pending invites of the org (not used, cancelled or expired), or one of them.
async fn invites(db: &Db, ctx: &OrgCtx, only: Option<&str>) -> anyhow::Result<Vec<InviteView>> {
    let rows = sqlx::query(
        "SELECT i.id, i.email, i.role, i.expires_at,
                COALESCE(NULLIF(u.email, ''), u.display_name, '') AS invited_by
         FROM invites i LEFT JOIN users u ON u.id = i.invited_by
         WHERE i.org_id = ?1 AND i.accepted_at IS NULL AND i.revoked_at IS NULL
           AND i.expires_at > ?2 AND (?3 IS NULL OR i.id = ?3)
         ORDER BY i.created_at, i.id",
    )
    .bind(ctx.org_id.to_string())
    .bind(crate::db::now())
    .bind(only)
    .fetch_all(db.pool())
    .await?;
    rows.iter().map(invite_view).collect()
}

/// Whether someone with this email is already in the org.
async fn is_member_email(db: &Db, ctx: &OrgCtx, email: &str) -> anyhow::Result<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memberships m JOIN users u ON u.id = m.user_id
         WHERE m.org_id = ? AND u.email = ?",
    )
    .bind(ctx.org_id.to_string())
    .bind(email)
    .fetch_one(db.pool())
    .await?;
    Ok(n > 0)
}

/// Store a new invite (only its token's hash). Returns its id.
async fn insert_invite(
    db: &Db,
    ctx: &OrgCtx,
    email: &str,
    role: Role,
    token: &str,
) -> anyhow::Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = crate::db::now();
    sqlx::query(
        "INSERT INTO invites (id, org_id, email, role, token_sha256, invited_by, created_at,
                              expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(ctx.org_id.to_string())
    .bind(email)
    .bind(role.as_str())
    .bind(sha256_hex(token))
    .bind(ctx.user.id.to_string())
    .bind(now)
    .bind(now + INVITE_TTL.as_secs() as i64)
    .execute(db.pool())
    .await?;
    Ok(id)
}

async fn org_name(db: &Db, org: OrgId) -> anyhow::Result<String> {
    Ok(sqlx::query_scalar("SELECT name FROM orgs WHERE id = ?")
        .bind(org.to_string())
        .fetch_one(db.pool())
        .await?)
}

// -- handlers ------------------------------------------------------------------------

fn person_target(p: &Person) -> AuditTarget {
    AuditTarget {
        kind: "person".into(),
        id: p.user_id.clone(),
        label: Some(if p.email.is_empty() {
            p.name.clone()
        } else {
            p.email.clone()
        }),
    }
}

fn parse_user(s: &str) -> ApiResult<OperatorId> {
    uuid::Uuid::parse_str(s)
        .map(OperatorId)
        .map_err(|_| ApiError::NotFound)
}

/// `GET /orgs/{org_id}/people` (any member).
async fn list_people(State(st): State<PortalState>, ctx: OrgCtx) -> ApiResult<Json<Vec<Person>>> {
    ctx.require(Action::View)?;
    Ok(Json(people(&st.db, &ctx, None).await?))
}

#[derive(Deserialize)]
struct RoleBody {
    role: Role,
}

/// `PATCH /orgs/{org_id}/people/{user_id}` `{role}` → the person.
async fn change_role(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    Path((_, user_id)): Path<(String, String)>,
    ApiJson(body): ApiJson<RoleBody>,
) -> ApiResult<Json<Person>> {
    ctx.require(Action::ManagePeople)?;
    let user = parse_user(&user_id)?;
    let before = person(&st.db, &ctx, user)
        .await?
        .ok_or(ApiError::NotFound)?;
    ctx.require_manage_member(before.role, Some(body.role))?;
    if before.role == body.role {
        return Ok(Json(before));
    }
    if !set_role(&st.db, &ctx, user, body.role).await? {
        return Err(ApiError::Conflict(LAST_OWNER.into()));
    }
    let after = person(&st.db, &ctx, user)
        .await?
        .ok_or(ApiError::NotFound)?;
    ctx.audit(
        &st,
        "person.role_changed",
        Some(person_target(&after)),
        serde_json::json!({ "from": before.role, "to": after.role }),
    );
    let connect = Action::Connect.min_role();
    if before.role.at_least(connect) && !after.role.at_least(connect) {
        st.app.end_operator_sessions(ctx.org_id, user).await;
    }
    Ok(Json(after))
}

/// `DELETE /orgs/{org_id}/people/{user_id}` → 204. Their sign-ins end and their desktop
/// sockets for this org are dropped.
async fn remove_person(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    Path((_, user_id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    ctx.require(Action::ManagePeople)?;
    let user = parse_user(&user_id)?;
    let p = person(&st.db, &ctx, user)
        .await?
        .ok_or(ApiError::NotFound)?;
    ctx.require_manage_member(p.role, None)?;
    if !remove_member(&st.db, &ctx, user).await? {
        return Err(ApiError::Conflict(LAST_OWNER.into()));
    }
    revoke_all(&st, user).await?;
    let sockets = st.app.evict_operator(ctx.org_id, user).await;
    ctx.audit(
        &st,
        "person.removed",
        Some(person_target(&p)),
        serde_json::json!({ "role": p.role, "desktop_sockets_dropped": sockets }),
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /orgs/{org_id}/invites` (admin) → pending invites.
async fn list_invites(
    State(st): State<PortalState>,
    ctx: OrgCtx,
) -> ApiResult<Json<Vec<InviteView>>> {
    ctx.require(Action::ManagePeople)?;
    Ok(Json(invites(&st.db, &ctx, None).await?))
}

#[derive(Deserialize)]
struct InviteBody {
    email: String,
    role: Role,
}

/// `POST /orgs/{org_id}/invites` `{email, role}` → 201 the invite; the link is emailed.
async fn create_invite(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    headers: HeaderMap,
    ApiJson(body): ApiJson<InviteBody>,
) -> ApiResult<Response> {
    ctx.require(Action::ManagePeople)?;
    // Inviting someone as owner is making an owner.
    ctx.require_manage_member(body.role, Some(body.role))?;
    let email = body.email.trim().to_owned();
    if !plausible_email(&email) {
        return Err(ApiError::Invalid("that is not an email address".into()));
    }
    // Links point at the portal; without a configured URL, at the origin the request came
    // from (the security guard already checked it against the Host).
    let origin = match st.config.origin() {
        Some(o) => o.to_owned(),
        None => headers
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .map(|o| o.trim_end_matches('/').to_owned())
            .ok_or(ApiError::Forbidden("this server has no portal URL"))?,
    };
    if is_member_email(&st.db, &ctx, &email).await? {
        return Err(ApiError::Conflict(format!(
            "{email} is already in this company."
        )));
    }
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM invites WHERE org_id = ? AND email = ? AND accepted_at IS NULL
           AND revoked_at IS NULL AND expires_at > ?",
    )
    .bind(ctx.org_id.to_string())
    .bind(&email)
    .bind(crate::db::now())
    .fetch_one(st.db.pool())
    .await?;
    if pending > 0 {
        return Err(ApiError::Conflict(format!(
            "{email} already has an invite. Cancel it to send a new one."
        )));
    }
    if !st
        .limiter
        .hit(&format!("invite:{}", ctx.org_id), INVITES_PER_ORG)
    {
        return Err(ApiError::RateLimited);
    }
    let token = random_token();
    let id = insert_invite(&st.db, &ctx, &email, body.role, &token).await?;
    let company = org_name(&st.db, ctx.org_id).await?;
    let inviter = ctx
        .user
        .name
        .clone()
        .filter(|n| !n.is_empty())
        .or_else(|| ctx.user.email.clone())
        .unwrap_or_else(|| "Someone".into());
    st.send_mail(invite_mail(
        &email, &origin, &token, &company, &inviter, body.role,
    ));
    let inv = invites(&st.db, &ctx, Some(&id))
        .await?
        .pop()
        .ok_or_else(|| anyhow::anyhow!("invite {id} vanished"))?;
    ctx.audit(
        &st,
        "person.invited",
        Some(AuditTarget {
            kind: "invite".into(),
            id: inv.id.clone(),
            label: Some(inv.email.clone()),
        }),
        serde_json::json!({ "role": inv.role }),
    );
    Ok((StatusCode::CREATED, Json(inv)).into_response())
}

fn invite_mail(to: &str, origin: &str, token: &str, company: &str, by: &str, role: Role) -> Mail {
    let days = INVITE_TTL.as_secs() / 86_400;
    Mail {
        to: to.to_owned(),
        subject: format!("Join {company} on Rconn"),
        text: format!(
            "{by} invited you to {company} on Rconn as {}.\n\n\
             Open this link to accept, signing in as {to}:\n\n\
             {origin}/admin/invite?token={token}\n\n\
             The link works once, for {days} days. If you weren't expecting this, ignore \
             this email.\n",
            role.as_str()
        ),
    }
}

/// `DELETE /orgs/{org_id}/invites/{id}` → 204; the emailed link stops working.
async fn cancel_invite(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    Path((_, id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    ctx.require(Action::ManagePeople)?;
    let inv = invites(&st.db, &ctx, Some(&id))
        .await?
        .pop()
        .ok_or(ApiError::NotFound)?;
    ctx.require_manage_member(inv.role, None)?;
    sqlx::query(
        "UPDATE invites SET revoked_at = ? WHERE org_id = ? AND id = ? AND revoked_at IS NULL",
    )
    .bind(crate::db::now())
    .bind(ctx.org_id.to_string())
    .bind(&inv.id)
    .execute(st.db.pool())
    .await?;
    ctx.audit(
        &st,
        "invite.cancelled",
        Some(AuditTarget {
            kind: "invite".into(),
            id: inv.id.clone(),
            label: Some(inv.email.clone()),
        }),
        serde_json::json!({ "role": inv.role }),
    );
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct AcceptBody {
    token: String,
}

/// `POST /invites/accept` `{token}` → `Me` with the new membership. Needs a sign-in with
/// the invited email. Someone already in the company keeps their role.
async fn accept_invite(
    State(st): State<PortalState>,
    user: CurrentUser,
    ApiJson(body): ApiJson<AcceptBody>,
) -> ApiResult<Json<me::Me>> {
    if !looks_like_token(&body.token) {
        return Err(ApiError::Invalid(GONE.into()));
    }
    let now = crate::db::now();
    let row = sqlx::query(
        "SELECT id, org_id, email, role FROM invites
         WHERE token_sha256 = ? AND accepted_at IS NULL AND revoked_at IS NULL
           AND expires_at > ?",
    )
    .bind(sha256_hex(&body.token))
    .bind(now)
    .fetch_optional(st.db.pool())
    .await?
    .ok_or_else(|| ApiError::Invalid(GONE.into()))?;
    let invited: String = row.get("email");
    if !user
        .email
        .as_deref()
        .is_some_and(|e| e.eq_ignore_ascii_case(&invited))
    {
        return Err(ApiError::Forbidden(
            "This invite was sent to another email address. Sign in with that one.",
        ));
    }
    let id: String = row.get("id");
    let org = OrgId(uuid::Uuid::parse_str(row.get("org_id"))?);
    let role: Role = row.get::<String, _>("role").parse()?;
    let mut tx = st.db.pool().begin().await?;
    // Single use, even when two requests race: only one update matches.
    let n = sqlx::query(
        "UPDATE invites SET accepted_at = ?, accepted_by = ?
         WHERE id = ? AND accepted_at IS NULL AND revoked_at IS NULL AND expires_at > ?",
    )
    .bind(now)
    .bind(user.id.to_string())
    .bind(&id)
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n != 1 {
        return Err(ApiError::Invalid(GONE.into()));
    }
    let joined = crate::db::insert_membership(&mut tx, user.id, org, role).await?;
    tx.commit().await?;
    if joined {
        let ctx = OrgCtx {
            org_id: org,
            role,
            user: user.clone(),
        };
        ctx.audit(
            &st,
            "person.joined",
            Some(AuditTarget {
                kind: "person".into(),
                id: user.id.to_string(),
                label: user.email.clone(),
            }),
            serde_json::json!({ "role": role, "invite": id }),
        );
    }
    Ok(Json(me::load(&st.db, user.id).await?))
}
