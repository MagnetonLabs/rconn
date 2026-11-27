//! `GET /me`, `PATCH /me`: the signed-in person and the companies they belong to.

use super::{ApiJson, ApiResult, CurrentUser, PortalState, clean_name};
use crate::db::{Db, Role};
use axum::Json;
use axum::extract::State;
use rconn_core::ids::OperatorId;
use serde::{Deserialize, Serialize};
use sqlx::Row;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserView {
    pub id: String,
    /// Empty for people without one (logins imported from 0.1.x accounts).
    pub email: String,
    /// Empty until they set one.
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MembershipView {
    pub org_id: String,
    pub org_name: String,
    pub role: Role,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Me {
    pub user: UserView,
    pub memberships: Vec<MembershipView>,
}

/// `user`'s `Me` (what sign-in, `/me` and accepting an invite answer).
pub async fn load(db: &Db, user: OperatorId) -> anyhow::Result<Me> {
    let u = sqlx::query("SELECT id, email, display_name FROM users WHERE id = ?")
        .bind(user.to_string())
        .fetch_one(db.pool())
        .await?;
    let rows = sqlx::query(
        "SELECT m.org_id, o.name, m.role FROM memberships m JOIN orgs o ON o.id = m.org_id
         WHERE m.user_id = ? ORDER BY o.name, o.id",
    )
    .bind(user.to_string())
    .fetch_all(db.pool())
    .await?;
    let memberships = rows
        .iter()
        .map(|r| {
            Ok(MembershipView {
                org_id: r.get("org_id"),
                org_name: r.get("name"),
                role: r.get::<String, _>("role").parse()?,
            })
        })
        .collect::<anyhow::Result<_>>()?;
    Ok(Me {
        user: UserView {
            id: u.get("id"),
            email: u.get::<Option<String>, _>("email").unwrap_or_default(),
            name: u
                .get::<Option<String>, _>("display_name")
                .unwrap_or_default(),
        },
        memberships,
    })
}

pub async fn get_me(State(st): State<PortalState>, user: CurrentUser) -> ApiResult<Json<Me>> {
    Ok(Json(load(&st.db, user.id).await?))
}

#[derive(Deserialize)]
pub struct PatchMe {
    name: String,
}

pub async fn patch_me(
    State(st): State<PortalState>,
    user: CurrentUser,
    ApiJson(body): ApiJson<PatchMe>,
) -> ApiResult<Json<Me>> {
    let name = clean_name(&body.name, 100)?;
    sqlx::query("UPDATE users SET display_name = ? WHERE id = ?")
        .bind(name)
        .bind(user.id.to_string())
        .execute(st.db.pool())
        .await?;
    Ok(Json(load(&st.db, user.id).await?))
}
