//! The audit log (`/orgs/{org_id}/audit`, `…/audit.csv`).
//!
//! Rows come from the `audit` table, written by [`crate::audit_db::DbAudit`]. Handlers
//! take an [`OrgCtx`], need `Action::ViewAudit`, and read only `WHERE org_id = ctx.org_id`.
//!
//! Agreed with the web app: `cursor` is opaque; `limit` defaults to 50; the CSV header is
//! `at,kind,actor_type,actor,target_type,target,ip,detail`.
//!
//! Newest first (by row id, which follows insertion). `from`/`to` are RFC 3339 and both
//! inclusive; `kind` is exact; `q` matches, ignoring case, the kind, the details, and the
//! names and emails of the person and computer involved.

use super::org::{Action, OrgCtx};
use super::{ApiError, ApiResult, PortalState, rfc3339};
use crate::db::Db;
use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::Row;

/// Page size when the request doesn't say.
pub const DEFAULT_LIMIT: i64 = 50;
/// Largest page.
pub const MAX_LIMIT: i64 = 200;
/// Most rows one CSV export holds (newest first).
pub const MAX_CSV_ROWS: i64 = 100_000;
/// Longest search text.
const MAX_Q: usize = 200;

#[derive(Debug, Default, Deserialize)]
pub struct AuditQuery {
    from: Option<String>,
    to: Option<String>,
    q: Option<String>,
    kind: Option<String>,
    cursor: Option<String>,
    limit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Ref {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditItem {
    pub id: String,
    pub at: String,
    pub kind: String,
    pub actor: Ref,
    pub target: Option<Ref>,
    pub ip: Option<String>,
    pub detail: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditPage {
    pub items: Vec<AuditItem>,
    pub next: Option<String>,
}

/// The filters, checked.
struct Filter {
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    /// A LIKE pattern.
    q: Option<String>,
    kind: Option<String>,
}

fn time_ms(s: &Option<String>, what: &str) -> ApiResult<Option<i64>> {
    match s.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => s
            .parse::<jiff::Timestamp>()
            .map(|t| Some(t.as_millisecond()))
            .map_err(|_| ApiError::Invalid(format!("{what} must be an RFC 3339 time"))),
    }
}

impl Filter {
    fn parse(q: &AuditQuery) -> ApiResult<Self> {
        let text = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
        if text.is_some_and(|t| t.chars().count() > MAX_Q) {
            return Err(ApiError::Invalid(format!(
                "search text is at most {MAX_Q} characters"
            )));
        }
        let like = text.map(|t| {
            let escaped: String = t
                .chars()
                .flat_map(|c| match c {
                    '%' | '_' | '\\' => vec!['\\', c],
                    c => vec![c],
                })
                .collect();
            format!("%{escaped}%")
        });
        Ok(Self {
            from_ms: time_ms(&q.from, "from")?,
            to_ms: time_ms(&q.to, "to")?,
            q: like,
            kind: q
                .kind
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        })
    }
}

fn encode_cursor(id: i64) -> String {
    URL_SAFE_NO_PAD.encode(format!("a1:{id}"))
}

fn decode_cursor(s: &str) -> ApiResult<i64> {
    let bad = || ApiError::Invalid("cursor isn't one this server gave".into());
    let raw = URL_SAFE_NO_PAD.decode(s.trim()).map_err(|_| bad())?;
    let text = String::from_utf8(raw).map_err(|_| bad())?;
    text.strip_prefix("a1:")
        .and_then(|n| n.parse::<i64>().ok())
        .filter(|n| *n > 0)
        .ok_or_else(bad)
}

/// Rows of `ctx`'s org matching `f`, newest first, with ids below `before`.
async fn query_rows(
    db: &Db,
    ctx: &OrgCtx,
    f: &Filter,
    before: Option<i64>,
    limit: i64,
) -> anyhow::Result<Vec<AuditItem>> {
    let rows = sqlx::query(
        "SELECT a.id, a.ts_ms, a.actor_id, a.event, a.detail,
                u.email AS u_email, u.display_name AS u_name,
                COALESCE(h.admin_name, h.hostname) AS h_name
         FROM audit a
         LEFT JOIN users u ON u.id = a.actor_id
         LEFT JOIN hosts h ON h.org_id = a.org_id
              AND h.id = json_extract(a.detail, '$.host_id')
         WHERE a.org_id = ?1
           AND (?2 IS NULL OR a.id < ?2)
           AND (?3 IS NULL OR a.ts_ms >= ?3)
           AND (?4 IS NULL OR a.ts_ms <= ?4)
           AND (?5 IS NULL OR a.event = ?5)
           AND (?6 IS NULL OR a.event LIKE ?6 ESCAPE '\\' OR a.detail LIKE ?6 ESCAPE '\\'
                OR u.email LIKE ?6 ESCAPE '\\' OR u.display_name LIKE ?6 ESCAPE '\\'
                OR h.admin_name LIKE ?6 ESCAPE '\\' OR h.hostname LIKE ?6 ESCAPE '\\')
         ORDER BY a.id DESC
         LIMIT ?7",
    )
    .bind(ctx.org_id.to_string())
    .bind(before)
    .bind(f.from_ms)
    .bind(f.to_ms)
    .bind(f.kind.as_deref())
    .bind(f.q.as_deref())
    .bind(limit)
    .fetch_all(db.pool())
    .await?;
    Ok(rows.iter().map(item).collect())
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_owned)
}

/// Shape a stored row as the API's event.
fn item(r: &sqlx::sqlite::SqliteRow) -> AuditItem {
    let id: i64 = r.get("id");
    let ts: i64 = r.get("ts_ms");
    let kind: String = r.get("event");
    let actor_id: Option<String> = r.get("actor_id");
    let raw: Value = serde_json::from_str(&r.get::<String, _>("detail")).unwrap_or_default();
    let email: Option<String> = r
        .get::<Option<String>, _>("u_email")
        .filter(|e| !e.is_empty());
    let name: Option<String> = r
        .get::<Option<String>, _>("u_name")
        .filter(|n| !n.is_empty());
    let host_name: Option<String> = r.get("h_name");

    let host_id = s(&raw, "host_id");
    let computer = host_id.as_ref().map(|h| Ref {
        kind: "computer".into(),
        id: h.clone(),
        label: host_name.clone().unwrap_or_else(|| short(h)),
    });
    let user = |id: &str| Ref {
        kind: "user".into(),
        id: id.to_owned(),
        label: email
            .clone()
            .or_else(|| name.clone())
            .unwrap_or_else(|| format!("Operator {}", short(id))),
    };
    let system = |label: String| Ref {
        kind: "system".into(),
        id: "auth".into(),
        label,
    };
    let pick = |keys: &[&str]| -> Value {
        let mut m = Map::new();
        for k in keys {
            if let Some(v) = raw.get(*k).filter(|v| !v.is_null()) {
                m.insert((*k).to_owned(), v.clone());
            }
        }
        Value::Object(m)
    };

    let (actor, target, ip, detail) = if raw.get("target").is_some() && raw.get("detail").is_some()
    {
        // A portal action: {target, ip, detail}.
        let target = raw.get("target").filter(|t| t.is_object()).map(|t| Ref {
            kind: s(t, "type").unwrap_or_default(),
            id: s(t, "id").unwrap_or_default(),
            label: s(t, "label").unwrap_or_default(),
        });
        let actor = actor_id
            .as_deref()
            .map_or_else(|| system("system".into()), user);
        (actor, target, s(&raw, "ip"), raw["detail"].clone())
    } else {
        match kind.as_str() {
            "sign_in" => (
                actor_id
                    .as_deref()
                    .map_or_else(|| system("someone".into()), user),
                None,
                s(&raw, "ip"),
                pick(&["method"]),
            ),
            "sign_in.failed" => (
                system(s(&raw, "email").unwrap_or_default()),
                None,
                s(&raw, "ip"),
                pick(&["method", "reason"]),
            ),
            "computer.online" | "computer.offline" => (
                computer.clone().map_or_else(
                    || system("computer".into()),
                    |c| Ref {
                        kind: "host".into(),
                        ..c
                    },
                ),
                computer,
                None,
                json!({}),
            ),
            "session.guest" => (
                Ref {
                    kind: "user".into(),
                    id: actor_id.clone().unwrap_or_default(),
                    label: "Guest (ID and access password)".into(),
                },
                computer,
                None,
                json!({}),
            ),
            _ => {
                let actor = match actor_id.as_deref() {
                    Some(a) => user(a),
                    None if kind == "session.refused" => Ref {
                        kind: "user".into(),
                        id: String::new(),
                        label: "Guest (ID and access password)".into(),
                    },
                    None => system("system".into()),
                };
                let detail = match kind.as_str() {
                    "session.start" => json!({ "session": raw.get("session_id") }),
                    "session.end" => pick(&["duration_s", "reason"]),
                    "session.refused" => pick(&["reason"]),
                    _ => {
                        let mut d = raw.clone();
                        if let Some(m) = d.as_object_mut() {
                            for k in ["org_id", "host_id", "operator_id", "user_id", "orgs"] {
                                m.remove(k);
                            }
                        }
                        d
                    }
                };
                (actor, computer, None, detail)
            }
        }
    };
    AuditItem {
        id: id.to_string(),
        at: rfc3339(ts.div_euclid(1000)),
        kind,
        actor,
        target,
        ip,
        detail,
    }
}

/// The first 8 characters of an id (for things that have no name).
fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

fn parse_limit(s: &Option<String>) -> ApiResult<i64> {
    match s.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(DEFAULT_LIMIT),
        Some(s) => s
            .parse::<i64>()
            .map(|n| n.clamp(1, MAX_LIMIT))
            .map_err(|_| ApiError::Invalid("limit must be a number".into())),
    }
}

/// `GET /audit` → one page and the cursor of the next (null on the last page).
async fn list(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    Query(q): Query<AuditQuery>,
) -> ApiResult<Json<AuditPage>> {
    ctx.require(Action::ViewAudit)?;
    let f = Filter::parse(&q)?;
    let limit = parse_limit(&q.limit)?;
    let before = match q.cursor.as_deref().filter(|c| !c.trim().is_empty()) {
        Some(c) => Some(decode_cursor(c)?),
        None => None,
    };
    let mut items = query_rows(&st.db, &ctx, &f, before, limit + 1).await?;
    let next = if items.len() as i64 > limit {
        items.truncate(limit as usize);
        items
            .last()
            .and_then(|i| i.id.parse().ok())
            .map(encode_cursor)
    } else {
        None
    };
    Ok(Json(AuditPage { items, next }))
}

/// One CSV cell: quoted when needed, and never read as a formula by a spreadsheet.
fn csv_cell(v: &str) -> String {
    let v = if v.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{v}")
    } else {
        v.to_owned()
    };
    if v.contains([',', '"', '\n', '\r']) || v.starts_with(' ') || v.ends_with(' ') {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v
    }
}

pub const CSV_HEADER: &str = "at,kind,actor_type,actor,target_type,target,ip,detail";

/// `GET /audit.csv` → every matching event (newest first, up to [`MAX_CSV_ROWS`]).
async fn csv(
    State(st): State<PortalState>,
    ctx: OrgCtx,
    Query(q): Query<AuditQuery>,
) -> ApiResult<Response> {
    ctx.require(Action::ViewAudit)?;
    let f = Filter::parse(&q)?;
    let items = query_rows(&st.db, &ctx, &f, None, MAX_CSV_ROWS).await?;
    let mut out = String::with_capacity(64 * (items.len() + 1));
    out.push_str(CSV_HEADER);
    out.push('\n');
    for i in &items {
        let (tt, tl) = i
            .target
            .as_ref()
            .map_or(("", ""), |t| (t.kind.as_str(), t.label.as_str()));
        let cells = [
            i.at.as_str(),
            i.kind.as_str(),
            i.actor.kind.as_str(),
            i.actor.label.as_str(),
            tt,
            tl,
            i.ip.as_deref().unwrap_or(""),
            &i.detail.to_string(),
        ];
        let line: Vec<String> = cells.iter().map(|c| csv_cell(c)).collect();
        out.push_str(&line.join(","));
        out.push('\n');
    }
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"rconn-audit.csv\"",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        out,
    )
        .into_response())
}

/// Routes under `/orgs/{org_id}`.
pub fn routes() -> Router<PortalState> {
    Router::new()
        .route("/audit", get(list))
        .route("/audit.csv", get(csv))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_junk_is_refused() {
        assert_eq!(decode_cursor(&encode_cursor(42)).unwrap(), 42);
        for bad in [
            "",
            "42",
            "!!",
            &URL_SAFE_NO_PAD.encode("a1:-3"),
            &URL_SAFE_NO_PAD.encode("b1:3"),
        ] {
            assert!(decode_cursor(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn csv_cells_are_quoted_and_defused() {
        assert_eq!(csv_cell("plain"), "plain");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        assert_eq!(csv_cell("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_cell("=HYPERLINK(1)"), "'=HYPERLINK(1)");
        assert_eq!(csv_cell("-1+1,"), "\"'-1+1,\"");
        assert_eq!(csv_cell("line\nbreak"), "\"line\nbreak\"");
    }

    #[test]
    fn search_text_is_a_literal() {
        let f = Filter::parse(&AuditQuery {
            q: Some(" 100%_done ".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(f.q.as_deref(), Some("%100\\%\\_done%"));
        assert!(
            Filter::parse(&AuditQuery {
                from: Some("yesterday".into()),
                ..Default::default()
            })
            .is_err()
        );
        let f = Filter::parse(&AuditQuery {
            from: Some("2026-11-27T08:15:00.123Z".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(f.from_ms, Some(1_795_767_300_123));
    }
}
