//! Companies: the [`OrgCtx`] extractor every `/orgs/{org_id}/…` handler takes, the role
//! table, and `GET/PATCH /orgs/{org_id}`, `POST /orgs`.
//!
//! Tenancy rule: org data is only read or written through an `OrgCtx` (or the `OrgId`
//! it carries), and every query filters by that org. Someone who isn't a member of the
//! org in the path gets 404, exactly like an org that doesn't exist.

use super::{ApiError, ApiJson, ApiResult, CurrentUser, PortalState, clean_name, rfc3339};
use crate::audit::{AuditEvent, AuditTarget};
use crate::db::{Db, Role};
use axum::Json;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use rconn_core::fleet::FleetCa;
use rconn_core::ids::OrgId;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::HashMap;

/// What a member wants to do; [`Action::min_role`] is the contract's role table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// See computers, groups, policies, the company.
    View,
    /// Connect to computers (desktop app).
    Connect,
    /// Rename/move/revoke computers; enrollment tokens, groups, policies.
    ManageFleet,
    /// Invite people, change roles, remove people (admins can't touch owners: see
    /// [`OrgCtx::require_manage_member`]).
    ManagePeople,
    /// Read and export the audit log.
    ViewAudit,
    /// SSO settings, company settings, delete company, transfer ownership.
    ManageCompany,
}

impl Action {
    pub fn min_role(self) -> Role {
        match self {
            Self::View => Role::Viewer,
            Self::Connect => Role::Technician,
            Self::ManageFleet | Self::ManagePeople | Self::ViewAudit => Role::Admin,
            Self::ManageCompany => Role::Owner,
        }
    }
}

/// A signed-in member acting in one org (from the `{org_id}` path segment).
#[derive(Debug, Clone)]
pub struct OrgCtx {
    pub org_id: OrgId,
    pub role: Role,
    pub user: CurrentUser,
}

impl FromRequestParts<PortalState> for OrgCtx {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, st: &PortalState) -> Result<Self, ApiError> {
        let user = CurrentUser::from_request_parts(parts, st).await?;
        let Path(params) = Path::<HashMap<String, String>>::from_request_parts(parts, st)
            .await
            .map_err(|_| ApiError::NotFound)?;
        let org_id = params
            .get("org_id")
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
            .map(OrgId)
            .ok_or(ApiError::NotFound)?;
        let role = st
            .db
            .role(org_id, user.id)
            .await?
            .ok_or(ApiError::NotFound)?;
        Ok(Self { org_id, role, user })
    }
}

impl OrgCtx {
    /// 403 unless the member's role allows `action`.
    pub fn require(&self, action: Action) -> ApiResult<()> {
        self.require_role(action.min_role())
    }

    /// 403 unless the member's role is at least `min`.
    pub fn require_role(&self, min: Role) -> ApiResult<()> {
        if self.role.at_least(min) {
            Ok(())
        } else {
            Err(ApiError::Forbidden("your role doesn't allow this"))
        }
    }

    /// May this member change a person whose role is `current` (to `new`, if changing
    /// the role)? Admins manage everyone but owners, and can't make owners.
    pub fn require_manage_member(&self, current: Role, new: Option<Role>) -> ApiResult<()> {
        self.require(Action::ManagePeople)?;
        if self.role != Role::Owner && (current == Role::Owner || new == Some(Role::Owner)) {
            return Err(ApiError::Forbidden("only an owner can change owners"));
        }
        Ok(())
    }

    /// Record an admin action in this org's audit log (queued; never blocks).
    pub fn audit(
        &self,
        st: &PortalState,
        kind: &str,
        target: Option<AuditTarget>,
        detail: serde_json::Value,
    ) {
        st.audit.record(AuditEvent::PortalAction {
            org_id: self.org_id,
            actor_id: self.user.id,
            kind: kind.to_owned(),
            target,
            ip: self.user.client.ip,
            detail,
        });
    }
}

/// Handler for an endpoint of the contract that isn't built yet: checks membership and
/// the role it will need, then answers 501. Replace it when the endpoint lands.
pub async fn pending(ctx: OrgCtx, action: Action) -> ApiError {
    match ctx.require(action) {
        Ok(()) => ApiError::NotImplemented,
        Err(e) => e,
    }
}

// -- repository ----------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OrgView {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub plan: String,
}

/// The caller's org.
pub async fn load_org(db: &Db, ctx: &OrgCtx) -> anyhow::Result<OrgView> {
    let r = sqlx::query("SELECT id, name, created_at, plan FROM orgs WHERE id = ?")
        .bind(ctx.org_id.to_string())
        .fetch_one(db.pool())
        .await?;
    Ok(OrgView {
        id: r.get("id"),
        name: r.get("name"),
        created_at: rfc3339(r.get("created_at")),
        plan: r.get("plan"),
    })
}

async fn rename_org(db: &Db, ctx: &OrgCtx, name: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE orgs SET name = ? WHERE id = ?")
        .bind(name)
        .bind(ctx.org_id.to_string())
        .execute(db.pool())
        .await?;
    Ok(())
}

// -- handlers ------------------------------------------------------------------------

pub async fn get_org(State(st): State<PortalState>, ctx: OrgCtx) -> ApiResult<Json<OrgView>> {
    ctx.require(Action::View)?;
    Ok(Json(load_org(&st.db, &ctx).await?))
}

#[derive(Deserialize)]
pub struct NameBody {
    name: String,
}

/// `PATCH /orgs/{org_id}` `{name}` (owner).
pub async fn patch_org(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    ApiJson(body): ApiJson<NameBody>,
) -> ApiResult<Json<OrgView>> {
    ctx.require(Action::ManageCompany)?;
    let name = clean_name(&body.name, 100)?;
    let before = load_org(&st.db, &ctx).await?;
    rename_org(&st.db, &ctx, &name).await?;
    ctx.audit(
        &st,
        "org.renamed",
        Some(AuditTarget {
            kind: "org".into(),
            id: ctx.org_id.to_string(),
            label: Some(name.clone()),
        }),
        serde_json::json!({ "from": before.name, "to": name }),
    );
    Ok(Json(load_org(&st.db, &ctx).await?))
}

/// `POST /orgs` `{name}` → 201 the new company, with the caller as owner. Only when the
/// server allows sign-up.
pub async fn create_org(
    State(st): State<PortalState>,
    user: CurrentUser,
    ApiJson(body): ApiJson<NameBody>,
) -> ApiResult<Response> {
    if !st.config.allow_signup {
        return Err(ApiError::Forbidden(
            "this server doesn't allow creating companies",
        ));
    }
    let name = clean_name(&body.name, 100)?;
    if !st.limiter.hit(
        &format!("org-create:{}", user.id),
        st.config.limits.org_create_per_user,
    ) {
        return Err(ApiError::RateLimited);
    }
    let org_id = OrgId::new();
    let ca = FleetCa::generate(org_id)?;
    let sealed = st.master.seal(org_id, ca.key_pem().as_bytes())?;
    let mut tx = st.db.pool().begin().await?;
    sqlx::query(
        "INSERT INTO orgs (id, name, legacy, created_at, ca_cert_pem, ca_key_sealed, plan)
         VALUES (?, ?, 0, ?, ?, ?, 'free')",
    )
    .bind(org_id.to_string())
    .bind(&name)
    .bind(crate::db::now())
    .bind(ca.cert_pem())
    .bind(sealed)
    .execute(&mut *tx)
    .await?;
    crate::db::insert_membership(&mut tx, user.id, org_id, Role::Owner).await?;
    tx.commit().await?;
    let ctx = OrgCtx {
        org_id,
        role: Role::Owner,
        user,
    };
    ctx.audit(
        &st,
        "org.created",
        Some(AuditTarget {
            kind: "org".into(),
            id: org_id.to_string(),
            label: Some(name),
        }),
        serde_json::json!({}),
    );
    Ok((StatusCode::CREATED, Json(load_org(&st.db, &ctx).await?)).into_response())
}
