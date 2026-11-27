//! People and invites (`/orgs/{org_id}/people…`, `…/invites…`, and the top-level
//! `/invites/accept`).
//!
//! Handlers take an [`OrgCtx`], check `ctx.require_manage_member(current, new)` (admins
//! can't touch owners), and query only through functions that take `&OrgCtx`. Invite
//! mail goes out with `st.send_mail`; `invites/accept` answers `me::load`. Removing a
//! person also ends their portal sign-ins (`session::revoke_all`).
//!
//! Agreed with the web app: any member may list people; listing invites needs admin.
//! Admins can't change or remove owners, nor make anyone owner (403); removing or
//! demoting the last owner is 409 `conflict`. `invites/accept` needs a session (401 sends
//! the app to sign-in) and an invite for another email is 403; the page is
//! `/admin/invite?token=`.

use super::org::{Action, OrgCtx, pending};
use super::{ApiError, CurrentUser, PortalState};
use axum::Router;
use axum::routing::{delete, get, patch, post};

/// Routes under `/orgs/{org_id}`.
pub fn routes() -> Router<PortalState> {
    Router::new()
        .route("/people", get(|c: OrgCtx| pending(c, Action::View)))
        .route(
            "/people/{user_id}",
            patch(|c: OrgCtx| pending(c, Action::ManagePeople))
                .delete(|c: OrgCtx| pending(c, Action::ManagePeople)),
        )
        .route(
            "/invites",
            get(|c: OrgCtx| pending(c, Action::ManagePeople))
                .post(|c: OrgCtx| pending(c, Action::ManagePeople)),
        )
        .route(
            "/invites/{id}",
            delete(|c: OrgCtx| pending(c, Action::ManagePeople)),
        )
}

/// Routes under `/api/v1` that aren't org-scoped.
pub fn public_routes() -> Router<PortalState> {
    Router::new().route(
        "/invites/accept",
        post(|_u: CurrentUser| async { ApiError::NotImplemented }),
    )
}
