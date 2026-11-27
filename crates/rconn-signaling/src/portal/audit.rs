//! The audit log (`/orgs/{org_id}/audit`, `…/audit.csv`).
//!
//! Rows come from the `audit` table, written by [`crate::audit_db::DbAudit`]. Handlers
//! take an [`OrgCtx`], need `Action::ViewAudit`, and read only `WHERE org_id = ctx.org_id`.
//!
//! Agreed with the web app: `cursor` is opaque; `limit` defaults to 50; the CSV header is
//! `at,kind,actor_type,actor,target_type,target,ip,detail`.

use super::PortalState;
use super::org::{Action, OrgCtx, pending};
use axum::Router;
use axum::routing::get;

/// Routes under `/orgs/{org_id}`.
pub fn routes() -> Router<PortalState> {
    Router::new()
        .route("/audit", get(|c: OrgCtx| pending(c, Action::ViewAudit)))
        .route("/audit.csv", get(|c: OrgCtx| pending(c, Action::ViewAudit)))
}
