//! Computer groups and their session policies (`/orgs/{org_id}/groups…`,
//! `…/policy/default`), and the effective policy the broker applies to a new session.
//!
//! Handlers take an [`OrgCtx`]: reads need `Action::View`, changes `Action::ManageFleet`.
//! Queries go through functions that take `&OrgCtx`; changes call
//! `ctx.audit("policy.changed", …)`.
//!
//! Agreed with the web app (`allowed_hours`): `days` ordered mon..sun, times `HH:MM`,
//! `end` before `start` means the window crosses midnight; bad values are 400 `invalid`.
//!
//! Effective policy of an org computer: its group's, else the company default, else
//! [`Policy::default`]. Enforcement:
//!
//! | Field | Where |
//! |---|---|
//! | `allowed_hours` | the server refuses `RequestSession` outside them (old hosts too) |
//! | `allow_access_password` | the server refuses guest sessions (ID + password) |
//! | `clipboard`, `file_transfer`, `show_indicator` | the host, from `SessionPolicy` |
//! | `audio`, `require_approval` | the host, when it has the capability; else the server refuses |
//!
//! A window's `days` are the days it *starts* on: Fri 22:00–06:00 also covers Saturday
//! until 06:00. Local wall-clock time in the zone counts, so daylight-saving changes move
//! the window with the clock.

use super::org::{Action, OrgCtx};
use super::{ApiError, ApiJson, ApiResult, PortalState, clean_name};
use crate::audit::AuditTarget;
use crate::auth::HostIdentity;
use crate::db::Db;
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch};
use rconn_core::ids::{HostId, OrgId};
use rconn_core::protocol::{HOST_CAP_POLICY_APPROVAL, HOST_CAP_POLICY_AUDIO, SessionPolicy};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;
use std::collections::HashMap;

/// Longest group name.
const MAX_GROUP_NAME: usize = 100;
/// Longest time zone name accepted.
const MAX_TZ_NAME: usize = 64;

/// Days of the week, in the web app's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Day {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Day {
    pub const ALL: [Day; 7] = [
        Day::Mon,
        Day::Tue,
        Day::Wed,
        Day::Thu,
        Day::Fri,
        Day::Sat,
        Day::Sun,
    ];

    fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.key() == s)
    }

    fn key(self) -> &'static str {
        ["mon", "tue", "wed", "thu", "fri", "sat", "sun"][self as usize]
    }

    fn label(self) -> &'static str {
        ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][self as usize]
    }

    fn previous(self) -> Self {
        Self::ALL[(self as usize + 6) % 7]
    }
}

/// When sessions may start, in a time zone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowedHours {
    /// IANA name, e.g. `America/Chicago`.
    pub timezone: String,
    /// The days a window starts on, mon..sun, at least one.
    pub days: Vec<Day>,
    /// `HH:MM`, 24-hour.
    pub start: String,
    /// `HH:MM`; before `start`: the window ends the next day.
    pub end: String,
}

/// A group's (or the company's) session policy, as the API shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub clipboard: bool,
    pub file_transfer: bool,
    pub audio: bool,
    pub require_approval: bool,
    pub show_indicator: bool,
    pub allow_access_password: bool,
    /// None: sessions at any time.
    pub allowed_hours: Option<AllowedHours>,
}

impl Default for Policy {
    /// New companies' default (the web app's `DEFAULT_POLICY`).
    fn default() -> Self {
        Self {
            clipboard: true,
            file_transfer: true,
            audio: true,
            require_approval: false,
            show_indicator: true,
            allow_access_password: true,
            allowed_hours: None,
        }
    }
}

fn invalid(m: impl Into<String>) -> ApiError {
    ApiError::Invalid(m.into())
}

/// Minutes after midnight of a strict `HH:MM`.
fn minutes(s: &str) -> Option<u16> {
    let b = s.as_bytes();
    if b.len() != 5 || b[2] != b':' || !s.is_char_boundary(2) {
        return None;
    }
    let (h, m) = (s[..2].parse::<u16>().ok()?, s[3..].parse::<u16>().ok()?);
    let digits = b[..2].iter().chain(&b[3..]).all(u8::is_ascii_digit);
    (digits && h < 24 && m < 60).then_some(h * 60 + m)
}

impl AllowedHours {
    /// Check and normalize what the web app sent.
    fn parse(v: &Value) -> ApiResult<Self> {
        let o = v
            .as_object()
            .ok_or_else(|| invalid("allowed_hours must be null or an object"))?;
        let tz = o
            .get("timezone")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if tz.is_empty() {
            return Err(invalid("Pick a time zone."));
        }
        if tz.len() > MAX_TZ_NAME || !tz.contains('/') && tz != "UTC" {
            return Err(invalid("Unknown time zone."));
        }
        let zone = jiff::tz::TimeZone::get(tz).map_err(|_| invalid("Unknown time zone."))?;
        let timezone = zone.iana_name().unwrap_or(tz).to_owned();
        let days = o
            .get("days")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("Pick at least one day."))?;
        let mut set = Vec::new();
        for d in days {
            let d = d
                .as_str()
                .and_then(Day::parse)
                .ok_or_else(|| invalid("Days must be mon, tue, wed, thu, fri, sat or sun."))?;
            set.push(d);
        }
        if set.is_empty() {
            return Err(invalid("Pick at least one day."));
        }
        set.sort();
        set.dedup();
        let time = |k: &str| {
            let s = o.get(k).and_then(Value::as_str).unwrap_or("").trim();
            minutes(s)
                .map(|_| s.to_owned())
                .ok_or_else(|| invalid("Times must be HH:MM (24-hour)."))
        };
        let (start, end) = (time("start")?, time("end")?);
        if start == end {
            return Err(invalid("Start and end must differ."));
        }
        Ok(Self {
            timezone,
            days: set,
            start,
            end,
        })
    }

    /// May a session start at `at`? An unknown zone or bad time (stored data damaged)
    /// answers no.
    pub fn allows(&self, at: jiff::Timestamp) -> bool {
        let (Ok(tz), Some(start), Some(end)) = (
            jiff::tz::TimeZone::get(&self.timezone),
            minutes(&self.start),
            minutes(&self.end),
        ) else {
            return false;
        };
        let local = at.to_zoned_datetime(tz);
        let day = Day::ALL[local.weekday().to_monday_zero_offset() as usize];
        let now = local.hour() as u16 * 60 + local.minute() as u16;
        let on = |d: Day| self.days.contains(&d);
        if start < end {
            on(day) && start <= now && now < end
        } else {
            (on(day) && now >= start) || (on(day.previous()) && now < end)
        }
    }

    /// "Mon, Tue, Wed 08:00–18:00 (America/Chicago)".
    pub fn describe(&self) -> String {
        let days: Vec<&str> = if self.days.len() == 7 {
            vec!["every day"]
        } else {
            self.days.iter().map(|d| d.label()).collect()
        };
        format!(
            "{} {}–{} ({})",
            days.join(", "),
            self.start,
            self.end,
            self.timezone
        )
    }
}

impl Policy {
    /// Check a body the web app sent: every switch must be a boolean.
    pub fn parse(v: &Value) -> ApiResult<Self> {
        let o = v
            .as_object()
            .ok_or_else(|| invalid("send the policy as a JSON object"))?;
        let flag = |k: &str| {
            o.get(k)
                .and_then(Value::as_bool)
                .ok_or_else(|| invalid(format!("{k} must be true or false.")))
        };
        let allowed_hours = match o.get("allowed_hours") {
            None | Some(Value::Null) => None,
            Some(h) => Some(AllowedHours::parse(h)?),
        };
        Ok(Self {
            clipboard: flag("clipboard")?,
            file_transfer: flag("file_transfer")?,
            audio: flag("audio")?,
            require_approval: flag("require_approval")?,
            show_indicator: flag("show_indicator")?,
            allow_access_password: flag("allow_access_password")?,
            allowed_hours,
        })
    }

    /// What the host enforces (`guest` is the broker's to set).
    pub fn session_policy(&self) -> SessionPolicy {
        SessionPolicy {
            show_indicator: self.show_indicator,
            clipboard: self.clipboard,
            guest: false,
            file_transfer: self.file_transfer,
            audio: self.audio,
            require_approval: self.require_approval,
        }
    }
}

/// Host capabilities this policy needs that `caps` lacks (the host can't enforce it).
pub fn missing_capabilities(policy: &Policy, caps: &[String]) -> Vec<&'static str> {
    let has = |c: &str| caps.iter().any(|x| x == c);
    let mut missing = Vec::new();
    if !policy.audio && !has(HOST_CAP_POLICY_AUDIO) {
        missing.push(HOST_CAP_POLICY_AUDIO);
    }
    if policy.require_approval && !has(HOST_CAP_POLICY_APPROVAL) {
        missing.push(HOST_CAP_POLICY_APPROVAL);
    }
    missing
}

/// The session policy for a new session under `policy`, or why it is refused.
/// `guest`: the viewer connects with the ID and access password.
pub fn decide(
    policy: &Policy,
    guest: bool,
    caps: &[String],
    at: jiff::Timestamp,
) -> Result<SessionPolicy, String> {
    if let Some(h) = &policy.allowed_hours
        && !h.allows(at)
    {
        return Err(format!(
            "outside the hours your organization allows for this computer: {}",
            h.describe()
        ));
    }
    if guest && !policy.allow_access_password {
        return Err(
            "this computer's organization doesn't allow connecting with its ID and access \
             password"
                .into(),
        );
    }
    let missing = missing_capabilities(policy, caps);
    if !missing.is_empty() {
        let what: Vec<&str> = missing
            .iter()
            .map(|c| match *c {
                HOST_CAP_POLICY_AUDIO => "sound off",
                _ => "ask before connecting",
            })
            .collect();
        return Err(format!(
            "Rconn on this computer is too old to apply your organization's policy ({}); \
             update it",
            what.join(", ")
        ));
    }
    let mut p = policy.session_policy();
    p.guest = guest;
    Ok(p)
}

// -- repository ----------------------------------------------------------------------

fn stored(json: &str) -> anyhow::Result<Policy> {
    Ok(serde_json::from_str(json)?)
}

/// The company default (stored, else [`Policy::default`]).
async fn default_policy(db: &Db, org: OrgId) -> anyhow::Result<Policy> {
    let p: Option<String> = sqlx::query_scalar("SELECT policy FROM org_policies WHERE org_id = ?")
        .bind(org.to_string())
        .fetch_optional(db.pool())
        .await?;
    p.map_or_else(|| Ok(Policy::default()), |s| stored(&s))
}

/// A group's own policy row, if it has one.
async fn group_policy_row(db: &Db, org: OrgId, group: &str) -> anyhow::Result<Option<Policy>> {
    let p: Option<String> = sqlx::query_scalar(
        "SELECT policy FROM host_group_policies WHERE org_id = ? AND group_id = ?",
    )
    .bind(org.to_string())
    .bind(group)
    .fetch_optional(db.pool())
    .await?;
    p.map(|s| stored(&s)).transpose()
}

/// The policy that applies to a new session to `host` of `org`: its group's, else the
/// company default. None: `org` isn't a company in the database (static configuration
/// applies).
pub async fn effective_policy(db: &Db, org: OrgId, host: HostId) -> anyhow::Result<Option<Policy>> {
    let row = sqlx::query(
        "SELECT gp.policy AS gp, op.policy AS dp FROM orgs o
         LEFT JOIN hosts h ON h.id = ?2 AND h.org_id = o.id
         LEFT JOIN host_group_policies gp ON gp.org_id = o.id AND gp.group_id = h.group_id
         LEFT JOIN org_policies op ON op.org_id = o.id
         WHERE o.id = ?1",
    )
    .bind(org.to_string())
    .bind(host.to_string())
    .fetch_optional(db.pool())
    .await?;
    let Some(r) = row else { return Ok(None) };
    let (gp, dp): (Option<String>, Option<String>) = (r.get("gp"), r.get("dp"));
    match gp.or(dp) {
        Some(s) => stored(&s).map(Some),
        None => Ok(Some(Policy::default())),
    }
}

/// Session policies from the database, for the broker.
pub struct DbPolicies(Db);

impl DbPolicies {
    pub fn new(db: Db) -> Self {
        Self(db)
    }
}

#[async_trait::async_trait]
impl crate::PolicySource for DbPolicies {
    async fn policy(&self, host: HostIdentity) -> anyhow::Result<Option<Policy>> {
        effective_policy(&self.0, host.org_id, host.host_id).await
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupView {
    pub id: String,
    pub name: String,
    /// Computers in the group (revoked ones don't count).
    pub computers: i64,
}

/// Groups with their computer counts (a literal, so queries stay static strings).
macro_rules! group_select {
    () => {
        "SELECT g.id, g.name,
            (SELECT COUNT(*) FROM hosts h
             WHERE h.org_id = g.org_id AND h.group_id = g.id AND h.revoked_at IS NULL) AS n
         FROM host_groups g"
    };
}

fn group_view(r: &sqlx::sqlite::SqliteRow) -> GroupView {
    GroupView {
        id: r.get("id"),
        name: r.get("name"),
        computers: r.get("n"),
    }
}

async fn list_groups_db(db: &Db, ctx: &OrgCtx) -> anyhow::Result<Vec<GroupView>> {
    let rows = sqlx::query(concat!(
        group_select!(),
        " WHERE g.org_id = ? ORDER BY g.name COLLATE NOCASE, g.id"
    ))
    .bind(ctx.org_id.to_string())
    .fetch_all(db.pool())
    .await?;
    Ok(rows.iter().map(group_view).collect())
}

/// The org's group `id` (404 for anything else, including other orgs' groups).
async fn load_group(db: &Db, ctx: &OrgCtx, id: &str) -> ApiResult<GroupView> {
    let id = uuid::Uuid::parse_str(id).map_err(|_| ApiError::NotFound)?;
    let row = sqlx::query(concat!(group_select!(), " WHERE g.org_id = ? AND g.id = ?"))
        .bind(ctx.org_id.to_string())
        .bind(id.to_string())
        .fetch_optional(db.pool())
        .await?;
    row.as_ref().map(group_view).ok_or(ApiError::NotFound)
}

/// 409 when another group of the org already has this name (any case).
async fn name_free(db: &Db, ctx: &OrgCtx, name: &str, except: Option<&str>) -> ApiResult<()> {
    let taken: Option<String> = sqlx::query_scalar(
        "SELECT id FROM host_groups WHERE org_id = ? AND lower(name) = lower(?) AND id IS NOT ?",
    )
    .bind(ctx.org_id.to_string())
    .bind(name)
    .bind(except)
    .fetch_optional(db.pool())
    .await?;
    match taken {
        Some(_) => Err(ApiError::Conflict(format!(
            "A group called {name} already exists."
        ))),
        None => Ok(()),
    }
}

fn group_target(g: &GroupView) -> AuditTarget {
    AuditTarget {
        kind: "group".into(),
        id: g.id.clone(),
        label: Some(g.name.clone()),
    }
}

fn default_target() -> AuditTarget {
    AuditTarget {
        kind: "group".into(),
        id: "default".into(),
        label: Some("Company default".into()),
    }
}

// -- handlers ------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct NameBody {
    name: String,
}

async fn list_groups(
    State(st): State<PortalState>,
    ctx: OrgCtx,
) -> ApiResult<Json<Vec<GroupView>>> {
    ctx.require(Action::View)?;
    Ok(Json(list_groups_db(&st.db, &ctx).await?))
}

/// `POST /groups` `{name}` → 201 the group; it starts with a copy of the company default.
async fn create_group(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    ApiJson(body): ApiJson<NameBody>,
) -> ApiResult<Response> {
    ctx.require(Action::ManageFleet)?;
    let name = clean_name(&body.name, MAX_GROUP_NAME)?;
    name_free(&st.db, &ctx, &name, None).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let policy = default_policy(&st.db, ctx.org_id).await?;
    let now = crate::db::now();
    let mut tx = st.db.pool().begin().await?;
    let inserted =
        sqlx::query("INSERT INTO host_groups (id, org_id, name, created_at) VALUES (?, ?, ?, ?)")
            .bind(&id)
            .bind(ctx.org_id.to_string())
            .bind(&name)
            .bind(now)
            .execute(&mut *tx)
            .await;
    if let Err(e) = inserted {
        // Two requests raced for the name.
        return match e.as_database_error() {
            Some(d) if d.is_unique_violation() => Err(ApiError::Conflict(format!(
                "A group called {name} already exists."
            ))),
            _ => Err(e.into()),
        };
    }
    sqlx::query(
        "INSERT INTO host_group_policies (org_id, group_id, policy, updated_at, updated_by)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(ctx.org_id.to_string())
    .bind(&id)
    .bind(serde_json::to_string(&policy)?)
    .bind(now)
    .bind(ctx.user.id.to_string())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let g = load_group(&st.db, &ctx, &id).await?;
    ctx.audit(
        &st,
        "group.created",
        Some(group_target(&g)),
        serde_json::json!({ "name": g.name }),
    );
    Ok((StatusCode::CREATED, Json(g)).into_response())
}

async fn rename_group(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    Path(p): Path<HashMap<String, String>>,
    ApiJson(body): ApiJson<NameBody>,
) -> ApiResult<Json<GroupView>> {
    ctx.require(Action::ManageFleet)?;
    let before = load_group(&st.db, &ctx, p.get("id").map_or("", |s| s)).await?;
    let name = clean_name(&body.name, MAX_GROUP_NAME)?;
    name_free(&st.db, &ctx, &name, Some(&before.id)).await?;
    sqlx::query("UPDATE host_groups SET name = ? WHERE org_id = ? AND id = ?")
        .bind(&name)
        .bind(ctx.org_id.to_string())
        .bind(&before.id)
        .execute(st.db.pool())
        .await?;
    let g = load_group(&st.db, &ctx, &before.id).await?;
    if g.name != before.name {
        ctx.audit(
            &st,
            "group.renamed",
            Some(group_target(&g)),
            serde_json::json!({ "from": before.name, "to": g.name }),
        );
    }
    Ok(Json(g))
}

/// `DELETE /groups/{id}` → 204; its computers become ungrouped (company default).
async fn delete_group(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    Path(p): Path<HashMap<String, String>>,
) -> ApiResult<StatusCode> {
    ctx.require(Action::ManageFleet)?;
    let g = load_group(&st.db, &ctx, p.get("id").map_or("", |s| s)).await?;
    let org = ctx.org_id.to_string();
    let mut tx = st.db.pool().begin().await?;
    sqlx::query("UPDATE hosts SET group_id = NULL WHERE org_id = ? AND group_id = ?")
        .bind(&org)
        .bind(&g.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM host_groups WHERE org_id = ? AND id = ?")
        .bind(&org)
        .bind(&g.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    ctx.audit(
        &st,
        "group.deleted",
        Some(group_target(&g)),
        serde_json::json!({ "computers": g.computers }),
    );
    Ok(StatusCode::NO_CONTENT)
}

async fn get_group_policy(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    Path(p): Path<HashMap<String, String>>,
) -> ApiResult<Json<Policy>> {
    ctx.require(Action::View)?;
    let g = load_group(&st.db, &ctx, p.get("id").map_or("", |s| s)).await?;
    let policy = match group_policy_row(&st.db, ctx.org_id, &g.id).await? {
        Some(p) => p,
        None => default_policy(&st.db, ctx.org_id).await?,
    };
    Ok(Json(policy))
}

async fn put_group_policy(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    Path(p): Path<HashMap<String, String>>,
    ApiJson(body): ApiJson<Value>,
) -> ApiResult<Json<Policy>> {
    ctx.require(Action::ManageFleet)?;
    let g = load_group(&st.db, &ctx, p.get("id").map_or("", |s| s)).await?;
    let policy = Policy::parse(&body)?;
    let before = match group_policy_row(&st.db, ctx.org_id, &g.id).await? {
        Some(p) => p,
        None => default_policy(&st.db, ctx.org_id).await?,
    };
    sqlx::query(
        "INSERT INTO host_group_policies (org_id, group_id, policy, updated_at, updated_by)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT (org_id, group_id) DO UPDATE SET policy = excluded.policy,
           updated_at = excluded.updated_at, updated_by = excluded.updated_by",
    )
    .bind(ctx.org_id.to_string())
    .bind(&g.id)
    .bind(serde_json::to_string(&policy)?)
    .bind(crate::db::now())
    .bind(ctx.user.id.to_string())
    .execute(st.db.pool())
    .await?;
    audit_change(&st, &ctx, group_target(&g), &before, &policy);
    Ok(Json(policy))
}

async fn get_default_policy(State(st): State<PortalState>, ctx: OrgCtx) -> ApiResult<Json<Policy>> {
    ctx.require(Action::View)?;
    Ok(Json(default_policy(&st.db, ctx.org_id).await?))
}

async fn put_default_policy(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    ApiJson(body): ApiJson<Value>,
) -> ApiResult<Json<Policy>> {
    ctx.require(Action::ManageFleet)?;
    let policy = Policy::parse(&body)?;
    let before = default_policy(&st.db, ctx.org_id).await?;
    sqlx::query(
        "INSERT INTO org_policies (org_id, policy, updated_at, updated_by) VALUES (?, ?, ?, ?)
         ON CONFLICT (org_id) DO UPDATE SET policy = excluded.policy,
           updated_at = excluded.updated_at, updated_by = excluded.updated_by",
    )
    .bind(ctx.org_id.to_string())
    .bind(serde_json::to_string(&policy)?)
    .bind(crate::db::now())
    .bind(ctx.user.id.to_string())
    .execute(st.db.pool())
    .await?;
    audit_change(&st, &ctx, default_target(), &before, &policy);
    Ok(Json(policy))
}

/// `policy.changed` with the whole policy before and after (and which fields changed).
fn audit_change(
    st: &PortalState,
    ctx: &OrgCtx,
    target: AuditTarget,
    before: &Policy,
    after: &Policy,
) {
    let (b, a) = (
        serde_json::to_value(before).unwrap_or_default(),
        serde_json::to_value(after).unwrap_or_default(),
    );
    let changed: Vec<String> = a
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, v)| b.get(k.as_str()) != Some(v))
        .map(|(k, _)| k.clone())
        .collect();
    ctx.audit(
        st,
        "policy.changed",
        Some(target),
        serde_json::json!({ "before": b, "after": a, "changed": changed }),
    );
}

/// Routes under `/orgs/{org_id}`.
pub fn routes() -> Router<PortalState> {
    Router::new()
        .route("/groups", get(list_groups).post(create_group))
        .route("/groups/{id}", patch(rename_group).delete(delete_group))
        .route(
            "/groups/{id}/policy",
            get(get_group_policy).put(put_group_policy),
        )
        .route(
            "/policy/default",
            get(get_default_policy).put(put_default_policy),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(s: &str) -> jiff::Timestamp {
        s.parse().unwrap()
    }

    fn hours(tz: &str, days: &[Day], start: &str, end: &str) -> AllowedHours {
        AllowedHours {
            timezone: tz.into(),
            days: days.to_vec(),
            start: start.into(),
            end: end.into(),
        }
    }

    fn body(h: Value) -> Value {
        let mut v = serde_json::to_value(Policy::default()).unwrap();
        v["allowed_hours"] = h;
        v
    }

    #[test]
    fn strict_times() {
        assert_eq!(minutes("00:00"), Some(0));
        assert_eq!(minutes("23:59"), Some(23 * 60 + 59));
        for bad in [
            "24:00",
            "8:00",
            "08:60",
            "08-00",
            "0800",
            "+8:00",
            "08:0a",
            "",
            "０８:00",
        ] {
            assert_eq!(minutes(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_valid_policy_is_normalized() {
        let p = Policy::parse(&body(json!({
            "timezone": "America/Chicago", "days": ["fri", "mon", "mon"],
            "start": "08:00", "end": "18:00"
        })))
        .unwrap();
        let h = p.allowed_hours.unwrap();
        assert_eq!(
            h.days,
            vec![Day::Mon, Day::Fri],
            "ordered mon..sun, no repeats"
        );
        assert_eq!(h.timezone, "America/Chicago");
        assert_eq!(
            Policy::parse(&body(Value::Null)).unwrap().allowed_hours,
            None
        );
    }

    #[test]
    fn invalid_policies_are_refused() {
        let hours = |tz: &str, days: Value, s: &str, e: &str| {
            body(json!({ "timezone": tz, "days": days, "start": s, "end": e }))
        };
        let cases = [
            hours("Mars/Base", json!(["mon"]), "08:00", "17:00"),
            hours("", json!(["mon"]), "08:00", "17:00"),
            hours("EST5EDT", json!(["mon"]), "08:00", "17:00"),
            hours("America/Chicago", json!([]), "08:00", "17:00"),
            hours("America/Chicago", json!(["monday"]), "08:00", "17:00"),
            hours("America/Chicago", json!("mon"), "08:00", "17:00"),
            hours("America/Chicago", json!(["mon"]), "8:00", "17:00"),
            hours("America/Chicago", json!(["mon"]), "08:00", "24:00"),
            hours("America/Chicago", json!(["mon"]), "08:00", "08:00"),
            body(json!("always")),
        ];
        for c in cases {
            assert!(
                matches!(Policy::parse(&c), Err(ApiError::Invalid(_))),
                "{c}"
            );
        }
        let mut missing = serde_json::to_value(Policy::default()).unwrap();
        missing.as_object_mut().unwrap().remove("audio");
        assert!(
            matches!(Policy::parse(&missing), Err(ApiError::Invalid(m)) if m.contains("audio"))
        );
        let mut wrong = serde_json::to_value(Policy::default()).unwrap();
        wrong["clipboard"] = json!("yes");
        assert!(Policy::parse(&wrong).is_err());
        assert!(Policy::parse(&json!([])).is_err());
    }

    #[test]
    fn office_hours_follow_daylight_saving() {
        use Day::*;
        let h = hours(
            "America/Chicago",
            &[Mon, Tue, Wed, Thu, Fri],
            "08:00",
            "18:00",
        );
        // Mon 9 Mar 2026, the day after clocks went forward: 13:30 UTC is 08:30 CDT
        // (it was 07:30 the week before, in CST).
        assert!(h.allows(at("2026-03-09T13:30:00Z")));
        assert!(!h.allows(at("2026-03-02T13:30:00Z")));
        // Mon 2 Nov 2026, the day after clocks went back: 13:30 UTC is 07:30 CST.
        assert!(!h.allows(at("2026-11-02T13:30:00Z")));
        assert!(h.allows(at("2026-10-26T13:30:00Z")));
        // End is exclusive, start inclusive.
        assert!(h.allows(at("2026-11-02T14:00:00Z")));
        assert!(!h.allows(at("2026-11-03T00:00:00Z")), "18:00 CST");
        assert!(h.allows(at("2026-11-02T23:59:00Z")));
        // Saturday never.
        assert!(!h.allows(at("2026-11-07T16:00:00Z")));
    }

    #[test]
    fn a_window_can_cross_midnight() {
        use Day::*;
        // Friday night 22:00 to Saturday 06:00, Berlin.
        let h = hours("Europe/Berlin", &[Fri], "22:00", "06:00");
        assert!(h.allows(at("2026-11-27T21:30:00Z")), "Fri 22:30 CET");
        assert!(h.allows(at("2026-11-28T04:59:00Z")), "Sat 05:59 CET");
        assert!(!h.allows(at("2026-11-28T05:00:00Z")), "Sat 06:00 CET");
        assert!(!h.allows(at("2026-11-27T20:59:00Z")), "Fri 21:59 CET");
        // Thursday night isn't in it (the window starts on Fridays only).
        assert!(!h.allows(at("2026-11-26T22:30:00Z")));
        assert!(
            !h.allows(at("2026-11-27T04:00:00Z")),
            "Fri 05:00: Thursday's window"
        );
        // Sunday's window runs into Monday morning (week wraps).
        let s = hours("UTC", &[Sun], "23:00", "01:00");
        assert!(s.allows(at("2026-11-30T00:30:00Z")), "Mon 00:30");
        // The night clocks go back in Berlin (Sun 25 Oct 2026, 03:00 CEST -> 02:00 CET):
        // the window follows the wall clock, so 02:30 counts both times it happens.
        let n = hours("Europe/Berlin", &[Sat], "23:00", "03:00");
        assert!(n.allows(at("2026-10-24T23:30:00Z")), "01:30 CEST");
        assert!(n.allows(at("2026-10-25T00:30:00Z")), "02:30 CEST");
        assert!(n.allows(at("2026-10-25T01:30:00Z")), "02:30 CET");
        assert!(!n.allows(at("2026-10-25T02:00:00Z")), "03:00 CET");
    }

    #[test]
    fn damaged_hours_allow_nothing() {
        let h = hours("Nowhere/Gone", &Day::ALL, "00:00", "23:59");
        assert!(!h.allows(at("2026-11-27T12:00:00Z")));
    }

    #[test]
    fn the_decision_refuses_with_a_clear_reason() {
        use Day::*;
        let caps: Vec<String> = rconn_core::protocol::HOST_CAPABILITIES
            .iter()
            .map(|c| (*c).to_owned())
            .collect();
        let mut p = Policy {
            allowed_hours: Some(hours("America/Chicago", &[Mon, Tue], "08:00", "18:00")),
            ..Policy::default()
        };
        let why = decide(&p, false, &caps, at("2026-11-07T16:00:00Z")).unwrap_err();
        assert!(
            why.contains("Mon, Tue 08:00–18:00 (America/Chicago)"),
            "{why}"
        );
        let ok = decide(&p, false, &caps, at("2026-11-02T16:00:00Z")).unwrap();
        assert!(ok.show_indicator && ok.audio && !ok.guest);

        p.allowed_hours = None;
        p.allow_access_password = false;
        assert!(decide(&p, true, &caps, at("2026-11-02T16:00:00Z")).is_err());
        assert!(decide(&p, false, &caps, at("2026-11-02T16:00:00Z")).is_ok());

        // Old hosts can't keep sound off or ask first: refused, not silently ignored.
        p.audio = false;
        p.require_approval = true;
        let why = decide(&p, false, &[], at("2026-11-02T16:00:00Z")).unwrap_err();
        assert!(
            why.contains("too old") && why.contains("sound off"),
            "{why}"
        );
        let sp = decide(&p, false, &caps, at("2026-11-02T16:00:00Z")).unwrap();
        assert!(!sp.audio && sp.require_approval);
        assert_eq!(
            missing_capabilities(&p, &[HOST_CAP_POLICY_AUDIO.into()]),
            vec![HOST_CAP_POLICY_APPROVAL]
        );
    }
}
