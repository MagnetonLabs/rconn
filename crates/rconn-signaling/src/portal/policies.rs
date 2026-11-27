//! Computer groups and their session policies (`/orgs/{org_id}/groups…`,
//! `…/policy/default`).
//!
//! Handlers take an [`OrgCtx`]: reads need `Action::View`, changes `Action::ManageFleet`.
//! Queries go through functions that take `&OrgCtx`; changes call
//! `ctx.audit("policy.changed", …)`.
//!
//! Agreed with the web app (`allowed_hours`): `days` ordered mon..sun, times `HH:MM`,
//! `end` before `start` means the window crosses midnight; bad values are 400 `invalid`.

use super::PortalState;
use super::org::{Action, OrgCtx, pending};
use axum::Router;
use axum::routing::{get, patch};

/// Routes under `/orgs/{org_id}`.
pub fn routes() -> Router<PortalState> {
    Router::new()
        .route(
            "/groups",
            get(|c: OrgCtx| pending(c, Action::View))
                .post(|c: OrgCtx| pending(c, Action::ManageFleet)),
        )
        .route(
            "/groups/{id}",
            patch(|c: OrgCtx| pending(c, Action::ManageFleet))
                .delete(|c: OrgCtx| pending(c, Action::ManageFleet)),
        )
        .route(
            "/groups/{id}/policy",
            get(|c: OrgCtx| pending(c, Action::View))
                .put(|c: OrgCtx| pending(c, Action::ManageFleet)),
        )
        .route(
            "/policy/default",
            get(|c: OrgCtx| pending(c, Action::View))
                .put(|c: OrgCtx| pending(c, Action::ManageFleet)),
        )
}
