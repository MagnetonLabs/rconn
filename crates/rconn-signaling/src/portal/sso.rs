//! Company SSO and passkeys: `/orgs/{org_id}/sso…` (owner), the SSO sign-in round trip
//! `/auth/sso/…`, and passkeys `/auth/passkey/…`, `/me/passkeys…`.
//!
//! Org handlers take an [`OrgCtx`] and need `Action::ManageCompany`. A finished SSO or
//! passkey sign-in starts its session with `session::start_session` and records
//! `AuditEvent::PortalSignIn`, like the password and magic-link sign-ins.
//!
//! Agreed with the web app:
//! - `GET /sso` needs owner. `PUT /sso` also sends `enabled: bool`; a missing
//!   `client_secret` keeps the stored one; `auto_join_role` is never owner.
//! - Domain verify answers 200 with the domain object (`verified: false` while the TXT
//!   record at the domain apex isn't found yet). There is no delete-domain endpoint.
//! - `auth::options`: `sso` comes only from a VERIFIED email domain and `passkey` means
//!   "this server supports passkeys", so known and unknown emails get identical answers.
//! - Passkeys (binary fields base64url): register start `{name}` → `{publicKey}`; finish
//!   `{name, credential}` → `{id, name, created_at, last_used}`; login start `{email}` →
//!   `{publicKey}`; finish `{credential}` → `Me`.

use super::org::{Action, OrgCtx, pending};
use super::{ApiError, CurrentUser, PortalState};
use axum::Router;
use axum::routing::{delete, get, post};

/// Routes under `/orgs/{org_id}`.
pub fn routes() -> Router<PortalState> {
    Router::new()
        .route(
            "/sso",
            get(|c: OrgCtx| pending(c, Action::ManageCompany))
                .put(|c: OrgCtx| pending(c, Action::ManageCompany)),
        )
        .route(
            "/sso/domains",
            post(|c: OrgCtx| pending(c, Action::ManageCompany)),
        )
        .route(
            "/sso/domains/{domain}/verify",
            post(|c: OrgCtx| pending(c, Action::ManageCompany)),
        )
}

async fn not_yet() -> ApiError {
    ApiError::NotImplemented
}

async fn not_yet_signed_in(_u: CurrentUser) -> ApiError {
    ApiError::NotImplemented
}

/// Routes under `/api/v1` that aren't org-scoped.
pub fn public_routes() -> Router<PortalState> {
    Router::new()
        .route("/auth/sso/start", get(not_yet))
        .route("/auth/sso/callback", get(not_yet))
        .route("/auth/passkey/login/start", post(not_yet))
        .route("/auth/passkey/login/finish", post(not_yet))
        .route("/auth/passkey/register/start", post(not_yet_signed_in))
        .route("/auth/passkey/register/finish", post(not_yet_signed_in))
        .route("/me/passkeys", get(not_yet_signed_in))
        .route("/me/passkeys/{id}", delete(not_yet_signed_in))
}
