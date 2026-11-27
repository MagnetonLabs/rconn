//! Company SSO: `/orgs/{org_id}/sso…` (owner) and the SSO sign-in round trip
//! `/auth/sso/…`. Passkeys are in [`super::passkeys`].
//!
//! Org handlers take an [`OrgCtx`] and need `Action::ManageCompany`. A finished SSO
//! sign-in starts its session with `session::start_session` and records
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
//! - A failed SSO round trip lands on `/admin/signin?sso_error=<code>`, code one of
//!   `unavailable`, `expired`, `domain`, `failed`, `rate_limited`.
//!
//! The server is a confidential OIDC client of each company's IdP (code flow, PKCE S256,
//! `state` bound to the browser by the `__Host-rconn_sso` cookie, `nonce` checked in the
//! ID token). The redirect URI is `<portal>/api/v1/auth/sso/callback` for every company.
//!
//! Who an IdP account is: once linked, (company, issuer, subject). The first sign-in links
//! by email, and only for an email at a domain the company VERIFIED: the company's IdP
//! could otherwise claim anyone's address. The same rule decides auto-join, with the
//! company's `auto_join_role`.

use super::auth::{record_sign_in, sign_in_failed};
use super::org::{Action, OrgCtx};
use super::ratelimit::{Limit, ip_key};
use super::security::{clear_cookie, cookie, set_cookie};
use super::session::{looks_like_token, random_token, sha256_hex, start_session};
use super::{ApiError, ApiJson, ApiResult, ClientInfo, PortalConfig, PortalState};
use crate::audit::AuditTarget;
use crate::db::{Db, Role};
use crate::oidc::{OidcClaims, OidcHttp, OidcProvider};
use crate::secrets::MasterKey;
use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use rconn_core::ids::{OperatorId, OrgId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Binds an SSO round trip to the browser that started it.
pub const SSO_COOKIE: &str = "__Host-rconn_sso";
/// An SSO round trip must finish within this (seconds).
const FLOW_TTL: i64 = 600;
/// Discovery documents and keys are fetched again after this.
const PROVIDER_TTL: Duration = Duration::from_secs(600);
const DNS_TIMEOUT: Duration = Duration::from_secs(10);
const SECRET_PURPOSE: &str = "sso-client-secret";
const TXT_PREFIX: &str = "rconn-verify=";
const MAX_DOMAINS: i64 = 50;
/// SSO round trips started or finished per client address.
const FLOW_PER_IP: Limit = Limit::new(30, Duration::from_secs(600));
/// Domain verification lookups per company.
const VERIFY_PER_ORG: Limit = Limit::new(30, Duration::from_secs(3600));

// -- DNS -------------------------------------------------------------------------------

/// Looks up the TXT records of a name (each record's strings joined).
#[async_trait::async_trait]
pub trait TxtResolver: Send + Sync + 'static {
    async fn txt(&self, name: &str) -> anyhow::Result<Vec<String>>;
}

/// The resolver domain verification uses.
#[derive(Clone)]
pub struct Dns(Arc<dyn TxtResolver>);

impl Dns {
    pub fn new(r: Arc<dyn TxtResolver>) -> Self {
        Self(r)
    }

    /// The system's resolver (`/etc/resolv.conf` and friends).
    pub fn system() -> Self {
        Self(Arc::new(SystemDns(std::sync::OnceLock::new())))
    }

    /// Finds no records, ever (tests that don't verify domains).
    pub fn none() -> Self {
        Self(Arc::new(StaticTxt::default()))
    }
}

impl Default for Dns {
    fn default() -> Self {
        Self::system()
    }
}

impl std::fmt::Debug for Dns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Dns(..)")
    }
}

struct SystemDns(std::sync::OnceLock<Result<hickory_resolver::TokioResolver, String>>);

#[async_trait::async_trait]
impl TxtResolver for SystemDns {
    async fn txt(&self, name: &str) -> anyhow::Result<Vec<String>> {
        let resolver = self
            .0
            .get_or_init(|| {
                hickory_resolver::TokioResolver::builder_tokio()
                    .and_then(|b| b.build())
                    .map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|e| anyhow::anyhow!("DNS resolver: {e}"))?;
        // Fully qualified: never searched under the server's own domain.
        let lookup = match resolver.txt_lookup(format!("{name}.")).await {
            Ok(l) => l,
            Err(e) if e.is_no_records_found() || e.is_nx_domain() => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        Ok(lookup
            .answers()
            .iter()
            .filter_map(|r| match &r.data {
                hickory_resolver::proto::rr::RData::TXT(t) => Some(
                    t.txt_data
                        .iter()
                        .map(|s| String::from_utf8_lossy(s))
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect())
    }
}

/// TXT records held in memory (tests).
#[derive(Default)]
pub struct StaticTxt(Mutex<HashMap<String, Vec<String>>>);

impl StaticTxt {
    pub fn set(&self, name: &str, records: Vec<String>) {
        self.0
            .lock()
            .expect("txt lock")
            .insert(name.to_ascii_lowercase(), records);
    }
}

#[async_trait::async_trait]
impl TxtResolver for StaticTxt {
    async fn txt(&self, name: &str) -> anyhow::Result<Vec<String>> {
        Ok(self
            .0
            .lock()
            .expect("txt lock")
            .get(&name.to_ascii_lowercase())
            .cloned()
            .unwrap_or_default())
    }
}

// -- runtime state ---------------------------------------------------------------------

struct Cached {
    issuer: String,
    client_id: String,
    fetched: Instant,
    provider: Arc<OidcProvider>,
}

/// SSO state that lives with the server: the IdP HTTP client and discovered IdPs.
pub struct SsoRuntime {
    http: OidcHttp,
    providers: Mutex<HashMap<OrgId, Cached>>,
}

impl SsoRuntime {
    pub fn new(config: &PortalConfig) -> Self {
        let http = if config.sso_private_issuers {
            OidcHttp::trusted()
        } else {
            OidcHttp::public_only()
        };
        Self {
            http: http.expect("building the IdP HTTP client"),
            providers: Mutex::default(),
        }
    }

    fn forget(&self, org: OrgId) {
        self.providers.lock().expect("sso lock").remove(&org);
    }

    /// The company's IdP, discovered (cached for a while).
    async fn provider(
        &self,
        org: OrgId,
        cfg: &SsoRow,
        master: &MasterKey,
    ) -> anyhow::Result<Arc<OidcProvider>> {
        if let Some(c) = self.providers.lock().expect("sso lock").get(&org)
            && c.issuer == cfg.issuer
            && c.client_id == cfg.client_id
            && c.fetched.elapsed() < PROVIDER_TTL
        {
            return Ok(c.provider.clone());
        }
        let secret = cfg.client_secret(org, master)?;
        let provider = Arc::new(
            OidcProvider::discover(&self.http, &cfg.issuer, &cfg.client_id, secret.as_deref())
                .await?,
        );
        self.providers.lock().expect("sso lock").insert(
            org,
            Cached {
                issuer: cfg.issuer.clone(),
                client_id: cfg.client_id.clone(),
                fetched: Instant::now(),
                provider: provider.clone(),
            },
        );
        Ok(provider)
    }
}

// -- repository ------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DomainView {
    pub domain: String,
    pub verified: bool,
    /// What to publish as a TXT record at the domain apex.
    pub txt_record: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SsoView {
    pub enabled: bool,
    pub issuer: String,
    pub client_id: String,
    pub domains: Vec<DomainView>,
    pub auto_join_role: Role,
}

struct SsoRow {
    enabled: bool,
    issuer: String,
    client_id: String,
    client_secret_sealed: Option<Vec<u8>>,
    auto_join_role: Role,
}

impl SsoRow {
    fn client_secret(&self, org: OrgId, master: &MasterKey) -> anyhow::Result<Option<String>> {
        self.client_secret_sealed
            .as_deref()
            .map(|s| {
                Ok(String::from_utf8(master.open_for(
                    org,
                    SECRET_PURPOSE,
                    s,
                )?)?)
            })
            .transpose()
    }
}

async fn sso_row(db: &Db, org: OrgId) -> anyhow::Result<Option<SsoRow>> {
    let r = sqlx::query(
        "SELECT enabled, issuer, client_id, client_secret_sealed, auto_join_role
         FROM org_sso WHERE org_id = ?",
    )
    .bind(org.to_string())
    .fetch_optional(db.pool())
    .await?;
    r.map(|r| {
        Ok(SsoRow {
            enabled: r.get::<i64, _>("enabled") == 1,
            issuer: r.get("issuer"),
            client_id: r.get("client_id"),
            client_secret_sealed: r.get("client_secret_sealed"),
            auto_join_role: r.get::<String, _>("auto_join_role").parse()?,
        })
    })
    .transpose()
}

fn domain_view(domain: String, token: &str, verified: bool) -> DomainView {
    DomainView {
        domain,
        verified,
        txt_record: format!("{TXT_PREFIX}{token}"),
    }
}

async fn domains(db: &Db, ctx: &OrgCtx) -> anyhow::Result<Vec<DomainView>> {
    let rows = sqlx::query(
        "SELECT domain, token, verified_at FROM org_domains WHERE org_id = ?
         ORDER BY created_at, domain",
    )
    .bind(ctx.org_id.to_string())
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            domain_view(
                r.get("domain"),
                &r.get::<String, _>("token"),
                r.get::<Option<i64>, _>("verified_at").is_some(),
            )
        })
        .collect())
}

async fn load_view(db: &Db, ctx: &OrgCtx) -> anyhow::Result<SsoView> {
    let row = sso_row(db, ctx.org_id).await?;
    Ok(SsoView {
        enabled: row.as_ref().is_some_and(|r| r.enabled),
        issuer: row.as_ref().map(|r| r.issuer.clone()).unwrap_or_default(),
        client_id: row
            .as_ref()
            .map(|r| r.client_id.clone())
            .unwrap_or_default(),
        domains: domains(db, ctx).await?,
        auto_join_role: row.map_or(Role::Viewer, |r| r.auto_join_role),
    })
}

/// A domain as people type it, normalized: lowercase ASCII (IDNs in punycode), no
/// trailing dot, at least two labels.
pub fn clean_domain(s: &str) -> ApiResult<String> {
    let d = s.trim().trim_end_matches('.').to_ascii_lowercase();
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    let labels: Vec<&str> = d.split('.').collect();
    let tld_ok = labels
        .last()
        .is_some_and(|t| !t.bytes().all(|b| b.is_ascii_digit()));
    if d.len() > 253 || labels.len() < 2 || !labels.iter().all(|l| label_ok(l)) || !tld_ok {
        return Err(ApiError::Invalid("enter a domain like example.com".into()));
    }
    Ok(d)
}

/// The domain part of an email address, normalized.
fn email_domain(email: &str) -> Option<String> {
    let (_, d) = email.trim().rsplit_once('@')?;
    clean_domain(d).ok()
}

/// The company that signs in people at `email`'s domain: SSO on and the domain verified.
pub(super) async fn sso_for_email(
    db: &Db,
    email: &str,
) -> anyhow::Result<Option<super::auth::SsoOption>> {
    let Some(domain) = email_domain(email) else {
        return Ok(None);
    };
    let r = sqlx::query(
        "SELECT o.id, o.name FROM org_domains d
         JOIN org_sso s ON s.org_id = d.org_id AND s.enabled = 1
         JOIN orgs o ON o.id = d.org_id
         WHERE d.domain = ? AND d.verified_at IS NOT NULL",
    )
    .bind(&domain)
    .fetch_optional(db.pool())
    .await?;
    Ok(r.map(|r| super::auth::SsoOption {
        org_id: r.get("id"),
        name: r.get("name"),
    }))
}

/// Is `domain` verified by `org`?
async fn verified_for(db: &Db, org: OrgId, domain: &str) -> anyhow::Result<bool> {
    let v: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM org_domains WHERE org_id = ? AND domain = ? AND verified_at IS NOT NULL",
    )
    .bind(org.to_string())
    .bind(domain)
    .fetch_optional(db.pool())
    .await?;
    Ok(v.is_some())
}

fn sso_target(ctx: &OrgCtx) -> Option<AuditTarget> {
    Some(AuditTarget {
        kind: "org".into(),
        id: ctx.org_id.to_string(),
        label: None,
    })
}

// -- org handlers ----------------------------------------------------------------------

/// Routes under `/orgs/{org_id}`.
pub fn routes() -> Router<PortalState> {
    Router::new()
        .route("/sso", get(get_sso).put(put_sso))
        .route("/sso/domains", post(add_domain))
        .route("/sso/domains/{domain}/verify", post(verify_domain))
}

/// Routes under `/api/v1` that aren't org-scoped.
pub fn public_routes() -> Router<PortalState> {
    Router::new()
        .route("/auth/sso/start", get(start))
        .route("/auth/sso/callback", get(callback))
}

async fn get_sso(State(st): State<PortalState>, ctx: OrgCtx) -> ApiResult<Json<SsoView>> {
    ctx.require(Action::ManageCompany)?;
    Ok(Json(load_view(&st.db, &ctx).await?))
}

#[derive(Deserialize)]
struct PutSso {
    issuer: String,
    client_id: String,
    client_secret: Option<String>,
    auto_join_role: Role,
    #[serde(default)]
    enabled: bool,
}

fn clean_issuer(st: &PortalState, s: &str) -> ApiResult<String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(String::new());
    }
    let bad = || {
        ApiError::Invalid(
            "the issuer must be an https:// URL, like https://login.example.com".into(),
        )
    };
    let u = url::Url::parse(s).map_err(|_| bad())?;
    let scheme_ok =
        u.scheme() == "https" || (st.config.sso_private_issuers && u.scheme() == "http");
    if s.len() > 512
        || !scheme_ok
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err(bad());
    }
    Ok(s.to_owned())
}

fn clean_field(s: &str, what: &str, max: usize) -> ApiResult<String> {
    let t = s.trim();
    if t.chars().count() > max || t.chars().any(char::is_control) {
        return Err(ApiError::Invalid(format!(
            "the {what} must be at most {max} characters"
        )));
    }
    Ok(t.to_owned())
}

/// `PUT /sso` `{issuer, client_id, client_secret?, auto_join_role, enabled}` (owner).
/// Turning SSO on checks that the IdP's discovery document loads.
async fn put_sso(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    ApiJson(body): ApiJson<PutSso>,
) -> ApiResult<Json<SsoView>> {
    ctx.require(Action::ManageCompany)?;
    if body.auto_join_role == Role::Owner {
        return Err(ApiError::Invalid(
            "people who join through SSO can be admins, technicians or viewers".into(),
        ));
    }
    let issuer = clean_issuer(&st, &body.issuer)?;
    let client_id = clean_field(&body.client_id, "client ID", 256)?;
    let new_secret = match body.client_secret.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => Some(clean_field(s, "client secret", 1024)?),
        _ => None,
    };
    let before = sso_row(&st.db, ctx.org_id).await?;
    let sealed = match &new_secret {
        Some(s) => Some(
            st.master
                .seal_for(ctx.org_id, SECRET_PURPOSE, s.as_bytes())?,
        ),
        None => before.as_ref().and_then(|b| b.client_secret_sealed.clone()),
    };
    let cfg = SsoRow {
        enabled: body.enabled,
        issuer,
        client_id,
        client_secret_sealed: sealed,
        auto_join_role: body.auto_join_role,
    };
    if cfg.enabled {
        if st.config.origin().is_none() {
            return Err(ApiError::Invalid(
                "single sign-on needs the server's portal URL (--portal-url)".into(),
            ));
        }
        if cfg.issuer.is_empty() || cfg.client_id.is_empty() || cfg.client_secret_sealed.is_none() {
            return Err(ApiError::Invalid(
                "enter the issuer, client ID and secret before turning single sign-on on".into(),
            ));
        }
        st.sso.forget(ctx.org_id);
        if let Err(e) = st.sso.provider(ctx.org_id, &cfg, &st.master).await {
            tracing::info!(org = %ctx.org_id, error = %format!("{e:#}"), "SSO discovery failed");
            return Err(ApiError::Invalid(format!(
                "couldn't load the identity provider's settings from \
                 {}/.well-known/openid-configuration",
                cfg.issuer.trim_end_matches('/')
            )));
        }
    }
    sqlx::query(
        "INSERT INTO org_sso (org_id, enabled, issuer, client_id, client_secret_sealed,
           auto_join_role, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (org_id) DO UPDATE SET enabled = ?2, issuer = ?3, client_id = ?4,
           client_secret_sealed = ?5, auto_join_role = ?6, updated_at = ?7",
    )
    .bind(ctx.org_id.to_string())
    .bind(i64::from(cfg.enabled))
    .bind(&cfg.issuer)
    .bind(&cfg.client_id)
    .bind(&cfg.client_secret_sealed)
    .bind(cfg.auto_join_role.as_str())
    .bind(crate::db::now())
    .execute(st.db.pool())
    .await?;
    if !cfg.enabled {
        st.sso.forget(ctx.org_id);
    }
    ctx.audit(
        &st,
        "sso.changed",
        sso_target(&ctx),
        serde_json::json!({
            "enabled": cfg.enabled,
            "issuer": cfg.issuer,
            "client_id": cfg.client_id,
            "auto_join_role": cfg.auto_join_role,
            "client_secret_changed": new_secret.is_some(),
            "was_enabled": before.is_some_and(|b| b.enabled),
        }),
    );
    Ok(Json(load_view(&st.db, &ctx).await?))
}

#[derive(Deserialize)]
struct DomainBody {
    domain: String,
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.is_unique_violation())
}

/// `POST /sso/domains` `{domain}` → 201 the domain, unverified, with its TXT record.
async fn add_domain(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    ApiJson(body): ApiJson<DomainBody>,
) -> ApiResult<Response> {
    ctx.require(Action::ManageCompany)?;
    let domain = clean_domain(&body.domain)?;
    let taken: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM org_domains WHERE domain = ? AND org_id != ? AND verified_at IS NOT NULL",
    )
    .bind(&domain)
    .bind(ctx.org_id.to_string())
    .fetch_optional(st.db.pool())
    .await?;
    if taken.is_some() {
        return Err(ApiError::Conflict(
            "another company has verified that domain".into(),
        ));
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM org_domains WHERE org_id = ?")
        .bind(ctx.org_id.to_string())
        .fetch_one(st.db.pool())
        .await?;
    if count >= MAX_DOMAINS {
        return Err(ApiError::Invalid(format!(
            "a company can have at most {MAX_DOMAINS} domains"
        )));
    }
    let token = B64.encode(rand::random::<[u8; 16]>());
    let r = sqlx::query(
        "INSERT INTO org_domains (org_id, domain, token, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(ctx.org_id.to_string())
    .bind(&domain)
    .bind(&token)
    .bind(crate::db::now())
    .execute(st.db.pool())
    .await;
    match r {
        Err(e) if is_unique_violation(&e) => {
            return Err(ApiError::Conflict("that domain is already added".into()));
        }
        r => r?,
    };
    ctx.audit(
        &st,
        "sso.changed",
        sso_target(&ctx),
        serde_json::json!({ "domain": domain, "action": "domain_added" }),
    );
    Ok((
        StatusCode::CREATED,
        Json(domain_view(domain, &token, false)),
    )
        .into_response())
}

/// `POST /sso/domains/{domain}/verify` → the domain; verified once the TXT record shows.
async fn verify_domain(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    Path(params): Path<HashMap<String, String>>,
) -> ApiResult<Json<DomainView>> {
    ctx.require(Action::ManageCompany)?;
    let domain = params
        .get("domain")
        .and_then(|d| clean_domain(d).ok())
        .ok_or(ApiError::NotFound)?;
    let row = sqlx::query(
        "SELECT domain, token, verified_at FROM org_domains WHERE org_id = ? AND domain = ?",
    )
    .bind(ctx.org_id.to_string())
    .bind(&domain)
    .fetch_optional(st.db.pool())
    .await?
    .ok_or(ApiError::NotFound)?;
    let token: String = row.get("token");
    if row.get::<Option<i64>, _>("verified_at").is_some() {
        return Ok(Json(domain_view(domain, &token, true)));
    }
    if !st
        .limiter
        .hit(&format!("sso-verify:{}", ctx.org_id), VERIFY_PER_ORG)
    {
        return Err(ApiError::RateLimited);
    }
    let expected = format!("{TXT_PREFIX}{token}");
    let found = match tokio::time::timeout(DNS_TIMEOUT, st.config.dns.0.txt(&domain)).await {
        Ok(Ok(records)) => records.iter().any(|r| r.trim() == expected),
        Ok(Err(e)) => {
            tracing::info!(%domain, error = %format!("{e:#}"), "TXT lookup failed");
            false
        }
        Err(_) => false,
    };
    if !found {
        return Ok(Json(domain_view(domain, &token, false)));
    }
    let r = sqlx::query(
        "UPDATE org_domains SET verified_at = ? WHERE org_id = ? AND domain = ?
           AND verified_at IS NULL",
    )
    .bind(crate::db::now())
    .bind(ctx.org_id.to_string())
    .bind(&domain)
    .execute(st.db.pool())
    .await;
    match r {
        Err(e) if is_unique_violation(&e) => {
            return Err(ApiError::Conflict(
                "another company has verified that domain".into(),
            ));
        }
        r => r?,
    };
    ctx.audit(
        &st,
        "sso.changed",
        sso_target(&ctx),
        serde_json::json!({ "domain": domain, "action": "domain_verified" }),
    );
    Ok(Json(domain_view(domain, &token, true)))
}

// -- sign-in round trip ----------------------------------------------------------------

fn redirect(to: &str) -> Response {
    let mut resp = StatusCode::FOUND.into_response();
    resp.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(to).unwrap_or(HeaderValue::from_static("/admin/")),
    );
    resp
}

fn fail(code: &'static str) -> Response {
    redirect(&format!("/admin/signin?sso_error={code}"))
}

/// The PKCE verifier of a round trip: derived from the browser's cookie, so it is never
/// stored and only that browser can finish.
fn pkce_verifier(browser: &str, state: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"rconn/sso-pkce/v1\0");
    h.update(browser.as_bytes());
    h.update([0]);
    h.update(state.as_bytes());
    B64.encode(h.finalize())
}

fn redirect_uri(origin: &str) -> String {
    format!("{origin}/api/v1/auth/sso/callback")
}

#[derive(Deserialize)]
struct StartQuery {
    org: Option<String>,
}

/// `GET /auth/sso/start?org=` → 302 to the company's IdP.
async fn start(
    State(st): State<PortalState>,
    client: ClientInfo,
    headers: HeaderMap,
    Query(q): Query<StartQuery>,
) -> Response {
    if !st
        .limiter
        .hit(&format!("sso-ip:{}", ip_key(client.ip)), FLOW_PER_IP)
    {
        return fail("rate_limited");
    }
    match start_inner(&st, &headers, q).await {
        Ok(Ok(r)) => r,
        Ok(Err(code)) => fail(code),
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "SSO start failed");
            fail("failed")
        }
    }
}

async fn start_inner(
    st: &PortalState,
    headers: &HeaderMap,
    q: StartQuery,
) -> anyhow::Result<Result<Response, &'static str>> {
    let Some(org) = q
        .org
        .as_deref()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(OrgId)
    else {
        return Ok(Err("unavailable"));
    };
    let (Some(origin), Some(cfg)) = (st.config.origin(), sso_row(&st.db, org).await?) else {
        return Ok(Err("unavailable"));
    };
    if !cfg.enabled {
        return Ok(Err("unavailable"));
    }
    let provider = match st.sso.provider(org, &cfg, &st.master).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(%org, error = %format!("{e:#}"), "SSO discovery failed");
            return Ok(Err("unavailable"));
        }
    };
    // Keep this browser's token if it has one, so two tabs don't unbind each other.
    let browser = cookie(headers, SSO_COOKIE)
        .filter(|c| looks_like_token(c))
        .unwrap_or_else(random_token);
    let (state, nonce) = (random_token(), random_token());
    let now = crate::db::now();
    sqlx::query("DELETE FROM sso_logins WHERE expires_at < ?")
        .bind(now)
        .execute(st.db.pool())
        .await?;
    sqlx::query(
        "INSERT INTO sso_logins (state_sha256, org_id, browser_sha256, nonce, issuer,
           created_at, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(sha256_hex(&state))
    .bind(org.to_string())
    .bind(sha256_hex(&browser))
    .bind(&nonce)
    .bind(&cfg.issuer)
    .bind(now)
    .bind(now + FLOW_TTL)
    .execute(st.db.pool())
    .await?;
    let url = provider.authorize_url(
        &redirect_uri(origin),
        &state,
        &nonce,
        &pkce_verifier(&browser, &state),
    )?;
    let mut resp = redirect(&url);
    resp.headers_mut().append(
        header::SET_COOKIE,
        set_cookie(SSO_COOKIE, &browser, FLOW_TTL as u64),
    );
    Ok(Ok(resp))
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// A refused round trip: what to tell the browser, and what to put in the audit log.
struct Refused {
    code: &'static str,
    reason: String,
    org: Option<OrgId>,
    email: Option<String>,
}

fn refused(code: &'static str, reason: impl Into<String>) -> Refused {
    Refused {
        code,
        reason: reason.into(),
        org: None,
        email: None,
    }
}

/// `GET /auth/sso/callback?code=&state=` → session cookie and 302 `/admin/`.
async fn callback(
    State(st): State<PortalState>,
    client: ClientInfo,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Response {
    if !st
        .limiter
        .hit(&format!("sso-ip:{}", ip_key(client.ip)), FLOW_PER_IP)
    {
        return fail("rate_limited");
    }
    match callback_inner(&st, &client, &headers, q).await {
        Ok(Ok(r)) => r,
        Ok(Err(r)) => {
            let orgs = r.org.into_iter().collect();
            sign_in_failed(
                &st,
                r.email.as_deref().unwrap_or(""),
                "sso",
                &r.reason,
                orgs,
                &client,
            );
            fail(r.code)
        }
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "SSO callback failed");
            fail("failed")
        }
    }
}

async fn callback_inner(
    st: &PortalState,
    client: &ClientInfo,
    headers: &HeaderMap,
    q: CallbackQuery,
) -> anyhow::Result<Result<Response, Refused>> {
    let Some(state) = q.state.filter(|s| looks_like_token(s)) else {
        return Ok(Err(refused("failed", "no state")));
    };
    // Single use: the row goes, whatever happens next.
    let Some(flow) = sqlx::query(
        "DELETE FROM sso_logins WHERE state_sha256 = ? AND expires_at > ?
         RETURNING org_id, browser_sha256, nonce, issuer",
    )
    .bind(sha256_hex(&state))
    .bind(crate::db::now())
    .fetch_optional(st.db.pool())
    .await?
    else {
        return Ok(Err(refused("expired", "unknown, used or expired state")));
    };
    let org = OrgId(uuid::Uuid::parse_str(&flow.get::<String, _>("org_id"))?);
    let refused_in = |code, reason: &str| {
        Err(Refused {
            code,
            reason: reason.to_owned(),
            org: Some(org),
            email: None,
        })
    };
    let Some(browser) = cookie(headers, SSO_COOKIE).filter(|c| {
        looks_like_token(c) && sha256_hex(c) == flow.get::<String, _>("browser_sha256")
    }) else {
        return Ok(refused_in("failed", "finished in another browser"));
    };
    if let Some(e) = q.error {
        let e: String = e.chars().take(100).collect();
        return Ok(refused_in("failed", &format!("identity provider: {e}")));
    }
    let Some(code) = q.code else {
        return Ok(refused_in("failed", "no code"));
    };
    let (Some(origin), Some(cfg)) = (st.config.origin(), sso_row(&st.db, org).await?) else {
        return Ok(refused_in("unavailable", "single sign-on is off"));
    };
    if !cfg.enabled || cfg.issuer != flow.get::<String, _>("issuer") {
        return Ok(refused_in("unavailable", "single sign-on changed"));
    }
    let provider = st.sso.provider(org, &cfg, &st.master).await?;
    let nonce: String = flow.get("nonce");
    let claims = match provider
        .exchange_code(
            &st.sso.http,
            &redirect_uri(origin),
            &code,
            &pkce_verifier(&browser, &state),
            &nonce,
        )
        .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::info!(%org, error = %format!("{e:#}"), "SSO code exchange refused");
            return Ok(refused_in(
                "failed",
                "the identity provider's answer didn't verify",
            ));
        }
    };
    let user = match sso_user(st, org, &cfg, &claims).await? {
        Ok(u) => u,
        Err((code, reason)) => {
            return Ok(Err(Refused {
                code,
                reason: reason.to_owned(),
                org: Some(org),
                email: claims.email,
            }));
        }
    };
    let session = start_session(st, user, client, headers).await?;
    record_sign_in(st, user, "sso", client).await?;
    let mut resp = redirect("/admin/");
    resp.headers_mut().append(header::SET_COOKIE, session);
    resp.headers_mut()
        .append(header::SET_COOKIE, clear_cookie(SSO_COOKIE));
    Ok(Ok(resp))
}

/// The person an IdP account signs in as, joining the company when their email's domain
/// is verified. `Err((code, reason))` refuses.
async fn sso_user(
    st: &PortalState,
    org: OrgId,
    cfg: &SsoRow,
    claims: &OidcClaims,
) -> anyhow::Result<Result<OperatorId, (&'static str, &'static str)>> {
    let now = crate::db::now();
    let domain_ok = match claims.email.as_deref().and_then(email_domain) {
        Some(d) => claims.email_verified != Some(false) && verified_for(&st.db, org, &d).await?,
        None => false,
    };
    let mut tx = st.db.pool().begin().await?;
    let linked: Option<String> = sqlx::query_scalar(
        "SELECT user_id FROM sso_identities WHERE org_id = ? AND issuer = ? AND subject = ?",
    )
    .bind(org.to_string())
    .bind(&claims.issuer)
    .bind(&claims.subject)
    .fetch_optional(&mut *tx)
    .await?;
    let user = match linked {
        Some(id) => OperatorId(uuid::Uuid::parse_str(&id)?),
        None => {
            if !domain_ok {
                return Ok(Err((
                    "domain",
                    "email domain not verified for this company",
                )));
            }
            let email = claims.email.as_deref().unwrap_or_default().trim();
            let existing: Option<String> =
                sqlx::query_scalar("SELECT id FROM users WHERE email = ?")
                    .bind(email)
                    .fetch_optional(&mut *tx)
                    .await?;
            let user = match existing {
                Some(id) => OperatorId(uuid::Uuid::parse_str(&id)?),
                None => {
                    let id = OperatorId::new();
                    let name = claims
                        .name
                        .as_deref()
                        .map(|n| n.trim().chars().take(100).collect::<String>())
                        .filter(|n| !n.is_empty() && !n.chars().any(char::is_control));
                    crate::db::insert_user(&mut tx, id, Some(email), name.as_deref()).await?;
                    id
                }
            };
            sqlx::query(
                "INSERT INTO sso_identities (org_id, issuer, subject, user_id, created_at,
                   last_used)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(org.to_string())
            .bind(&claims.issuer)
            .bind(&claims.subject)
            .bind(user.to_string())
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            user
        }
    };
    let enabled: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM users WHERE id = ? AND disabled_at IS NULL")
            .bind(user.to_string())
            .fetch_optional(&mut *tx)
            .await?;
    if enabled.is_none() {
        return Ok(Err(("failed", "account disabled")));
    }
    let member: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM memberships WHERE user_id = ? AND org_id = ?")
            .bind(user.to_string())
            .bind(org.to_string())
            .fetch_optional(&mut *tx)
            .await?;
    if member.is_none() {
        if !domain_ok {
            return Ok(Err((
                "domain",
                "not a member, and email domain not verified",
            )));
        }
        crate::db::insert_membership(&mut tx, user, org, cfg.auto_join_role).await?;
    }
    sqlx::query(
        "UPDATE sso_identities SET last_used = ? WHERE org_id = ? AND issuer = ? AND subject = ?",
    )
    .bind(now)
    .bind(org.to_string())
    .bind(&claims.issuer)
    .bind(&claims.subject)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Ok(user))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains_are_normalized_and_checked() {
        assert_eq!(clean_domain(" Acme.COM. ").unwrap(), "acme.com");
        assert_eq!(
            clean_domain("xn--bcher-kva.example").unwrap(),
            "xn--bcher-kva.example"
        );
        for bad in [
            "",
            "com",
            "-a.com",
            "a-.com",
            "a..com",
            "a b.com",
            "1.2.3.4",
            "bücher.de",
        ] {
            assert!(clean_domain(bad).is_err(), "{bad}");
        }
        assert_eq!(email_domain("Bob@Acme.com").as_deref(), Some("acme.com"));
        assert_eq!(email_domain("nope"), None);
    }

    #[test]
    fn pkce_verifier_is_a_valid_rfc7636_verifier() {
        let v = pkce_verifier(&random_token(), &random_token());
        assert_eq!(v.len(), 43);
        assert!(looks_like_token(&v));
        assert_ne!(v, pkce_verifier("a", "b"));
    }
}
