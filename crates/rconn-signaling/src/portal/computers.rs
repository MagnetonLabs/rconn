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

use super::PortalState;
use super::org::{Action, OrgCtx, pending};
use axum::Router;
use axum::routing::{delete, get, patch, post};

/// Routes under `/orgs/{org_id}`.
pub fn routes() -> Router<PortalState> {
    Router::new()
        .route("/computers", get(|c: OrgCtx| pending(c, Action::View)))
        .route(
            "/computers/{id}",
            patch(|c: OrgCtx| pending(c, Action::ManageFleet)),
        )
        .route(
            "/computers/{id}/revoke",
            post(|c: OrgCtx| pending(c, Action::ManageFleet)),
        )
        .route(
            "/enrollment-tokens",
            get(|c: OrgCtx| pending(c, Action::ManageFleet))
                .post(|c: OrgCtx| pending(c, Action::ManageFleet)),
        )
        .route(
            "/enrollment-tokens/{id}",
            delete(|c: OrgCtx| pending(c, Action::ManageFleet)),
        )
}
