//! The admin portal: a JSON API at `/api/v1` and the web app at `/admin`, served by the
//! signaling server on the same port. `/v1/ws` never reads the portal's cookies.
//!
//! Layout (one module per area, so areas can grow independently):
//!
//! | Module | Routes |
//! |---|---|
//! | [`auth`] | `/auth/password`, `/auth/magic-link`, `/auth/magic`, `/auth/logout`, `/auth/options` |
//! | [`me`] | `/me` |
//! | [`org`] | `/orgs`, `/orgs/{org_id}`, and [`OrgCtx`], the extractor every org route uses |
//! | [`computers`] | `/orgs/{org_id}/computers…`, `/orgs/{org_id}/enrollment-tokens…` |
//! | [`people`] | `/orgs/{org_id}/people…`, `/orgs/{org_id}/invites…`, `/invites/accept` |
//! | [`policies`] | `/orgs/{org_id}/groups…`, `/orgs/{org_id}/policy/default` |
//! | [`audit`] | `/orgs/{org_id}/audit`, `/orgs/{org_id}/audit.csv` |
//! | [`sso`] | `/orgs/{org_id}/sso…`, `/auth/sso/…`, `/auth/passkey/…`, `/me/passkeys…` |
//!
//! Every request passes [`security::guard`] (Origin check and JSON-only bodies on
//! non-GET requests). Org data is only reached through an [`OrgCtx`], which answers 404
//! to anyone who is not a member of the org in the path.

pub mod audit;
pub mod auth;
pub mod computers;
pub mod error;
pub mod me;
pub mod org;
pub mod people;
pub mod policies;
pub mod ratelimit;
pub mod security;
pub mod session;
pub mod sso;
pub mod ui;

pub use error::{ApiError, ApiJson, ApiResult};
pub use org::{Action, OrgCtx};
pub use session::{ClientInfo, CurrentUser};

use crate::AppState;
use crate::audit::AuditSink;
use crate::db::Db;
use crate::mail::Mailer;
use crate::secrets::MasterKey;
use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use ratelimit::{Limit, RateLimiter};
use std::sync::Arc;
use std::time::Duration;

/// Largest JSON body the API accepts.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// Rate limits (per window) on sign-in.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Password and magic-link sign-in attempts per client address (IPv6: per /64).
    pub sign_in_per_ip: Limit,
    /// Password sign-in attempts per email.
    pub sign_in_per_email: Limit,
    /// Magic links requested per client address.
    pub magic_link_per_ip: Limit,
    /// Magic links requested per email.
    pub magic_link_per_email: Limit,
    /// Companies created per person (hosted sign-up).
    pub org_create_per_user: Limit,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            sign_in_per_ip: Limit::new(30, Duration::from_secs(10 * 60)),
            sign_in_per_email: Limit::new(10, Duration::from_secs(15 * 60)),
            magic_link_per_ip: Limit::new(10, Duration::from_secs(3600)),
            magic_link_per_email: Limit::new(5, Duration::from_secs(3600)),
            org_create_per_user: Limit::new(5, Duration::from_secs(24 * 3600)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PortalConfig {
    /// The portal's public origin, e.g. `https://app.rconn.app` (no path). Magic links
    /// point here and non-GET requests must come from it. Without it, magic links are off
    /// and the Origin must match the request's Host.
    pub portal_url: Option<String>,
    /// Anyone may create a company (`POST /orgs`), and a magic link may create the person.
    pub allow_signup: bool,
    /// A sign-in lasts at most this long.
    pub session_ttl: Duration,
    /// A sign-in unused for this long ends.
    pub session_idle: Duration,
    pub magic_link_ttl: Duration,
    pub limits: Limits,
}

impl Default for PortalConfig {
    fn default() -> Self {
        Self {
            portal_url: None,
            allow_signup: false,
            session_ttl: Duration::from_secs(7 * 24 * 3600),
            session_idle: Duration::from_secs(24 * 3600),
            magic_link_ttl: Duration::from_secs(15 * 60),
            limits: Limits::default(),
        }
    }
}

impl PortalConfig {
    /// `portal_url` without a trailing slash.
    pub fn origin(&self) -> Option<&str> {
        self.portal_url.as_deref().map(|u| u.trim_end_matches('/'))
    }
}

/// Everything a portal handler can reach.
pub struct PortalInner {
    pub db: Db,
    /// The broker: evict hosts, drop operator sockets.
    pub app: AppState,
    /// Seals new orgs' CA keys.
    pub master: Arc<MasterKey>,
    pub mailer: Arc<dyn Mailer>,
    /// Admin actions and sign-ins (the database's audit table, through a queue).
    pub audit: Arc<dyn AuditSink>,
    pub config: PortalConfig,
    pub limiter: RateLimiter,
}

#[derive(Clone)]
pub struct PortalState(Arc<PortalInner>);

impl std::ops::Deref for PortalState {
    type Target = PortalInner;
    fn deref(&self) -> &PortalInner {
        &self.0
    }
}

impl PortalState {
    pub fn new(
        db: Db,
        app: AppState,
        master: Arc<MasterKey>,
        mailer: Arc<dyn Mailer>,
        audit: Arc<dyn AuditSink>,
        config: PortalConfig,
    ) -> Self {
        Self(Arc::new(PortalInner {
            db,
            app,
            master,
            mailer,
            audit,
            config,
            limiter: RateLimiter::default(),
        }))
    }

    /// Send an email from a background task (so slow mail never shows in response time).
    pub fn send_mail(&self, mail: crate::mail::Mail) {
        let mailer = self.mailer.clone();
        tokio::spawn(async move {
            let to = mail.to.clone();
            if let Err(e) = mailer.send(mail).await {
                tracing::error!(%to, error = %format!("{e:#}"), "sending mail failed");
            }
        });
    }
}

/// The portal's routes: `/api/v1/…` and `/admin/…`. Merge into the signaling router.
pub fn router(state: PortalState) -> Router {
    let org_areas = Router::new()
        .merge(computers::routes())
        .merge(people::routes())
        .merge(policies::routes())
        .merge(audit::routes())
        .merge(sso::routes());
    let api = Router::new()
        .route("/auth/password", post(auth::password))
        .route("/auth/magic-link", post(auth::magic_link))
        .route("/auth/magic", post(auth::magic))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/options", get(auth::options))
        .route("/me", get(me::get_me).patch(me::patch_me))
        .route("/orgs", post(org::create_org))
        .route("/orgs/{org_id}", get(org::get_org).patch(org::patch_org))
        .merge(people::public_routes())
        .merge(sso::public_routes())
        .nest("/orgs/{org_id}", org_areas)
        .fallback(|| async { ApiError::NotFound })
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            security::guard,
        ))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state.clone());
    Router::new().nest("/api/v1", api).merge(ui::routes())
}

/// Delete long-dead sessions and magic links every hour.
pub fn spawn_housekeeping(db: Db) {
    tokio::spawn(async move {
        loop {
            let now = crate::db::now();
            let r = async {
                sqlx::query("DELETE FROM magic_links WHERE expires_at < ?")
                    .bind(now - 24 * 3600)
                    .execute(db.pool())
                    .await?;
                // Kept a while after expiry: "last active" in the people list reads them.
                sqlx::query("DELETE FROM sessions WHERE expires_at < ?")
                    .bind(now - 90 * 24 * 3600)
                    .execute(db.pool())
                    .await?;
                anyhow::Ok(())
            }
            .await;
            if let Err(e) = r {
                tracing::warn!(error = %format!("{e:#}"), "portal housekeeping failed");
            }
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
}

/// Unix seconds as an RFC 3339 UTC string (`2026-11-27T08:15:00Z`).
pub fn rfc3339(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// A name a person typed (company, display name): trimmed, 1 to `max` characters, no
/// control characters.
pub fn clean_name(s: &str, max: usize) -> ApiResult<String> {
    let t = s.trim();
    if t.is_empty() || t.chars().count() > max || t.chars().any(char::is_control) {
        return Err(ApiError::Invalid(format!(
            "a name must be 1 to {max} characters"
        )));
    }
    Ok(t.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_formats_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_795_773_296), "2026-11-27T09:54:56Z");
    }
}
