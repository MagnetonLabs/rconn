//! The database side of the audit log (the `audit` table the portal shows).
//!
//! [`AuditSink::record`] runs in connection handlers, some under the broker's registry
//! lock, so [`DbAudit`] only puts events on a bounded queue. One writer task takes them
//! off in batches and inserts each batch in a single transaction. A full queue drops the
//! event and logs it loudly (the JSONL file still has it).
//!
//! Left out of the database: events of computers without an account (the public CA's
//! org, see [`DbAudit::ignoring_org`]) and unauthenticated noise (failed handshakes,
//! refused guests, account-less enrollment), which stay in the JSONL file.

use crate::audit::{AuditEvent, AuditSink};
use crate::db::Db;
use rconn_core::ids::{OperatorId, OrgId, SessionId};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};

/// Events queued for the writer before new ones are dropped.
pub const AUDIT_QUEUE: usize = 8192;
/// Most events inserted in one transaction.
pub const AUDIT_BATCH: usize = 256;
/// Open sessions remembered so `session.end` lands in the same org as `session.start`.
const OPEN_SESSIONS_CAP: usize = 100_000;

enum Msg {
    Event(i64, AuditEvent),
    Flush(oneshot::Sender<()>),
}

/// Writes audit events to the database from a background task.
pub struct DbAudit {
    tx: mpsc::Sender<Msg>,
    ignore_org: Option<OrgId>,
}

/// One row of the `audit` table.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditRow {
    pub org_id: Option<OrgId>,
    pub actor_id: Option<String>,
    /// The portal's kind (`session.start`, `sign_in`, `computer.renamed`, ...).
    pub kind: String,
    pub detail: serde_json::Value,
}

impl DbAudit {
    /// Start the writer task (needs a Tokio runtime).
    pub fn spawn(db: Db) -> Self {
        let (tx, mut rx) = mpsc::channel::<Msg>(AUDIT_QUEUE);
        tokio::spawn(async move {
            let mut open: HashMap<SessionId, (OrgId, OperatorId)> = HashMap::new();
            while let Some(first) = rx.recv().await {
                let mut batch = vec![first];
                while batch.len() < AUDIT_BATCH {
                    match rx.try_recv() {
                        Ok(m) => batch.push(m),
                        Err(_) => break,
                    }
                }
                let mut rows = Vec::with_capacity(batch.len());
                let mut flushes = Vec::new();
                for m in batch {
                    match m {
                        Msg::Event(ts, ev) => {
                            rows.extend(rows_for(&ev, &mut open).into_iter().map(|r| (ts, r)))
                        }
                        Msg::Flush(done) => flushes.push(done),
                    }
                }
                if !rows.is_empty()
                    && let Err(e) = insert(&db, &rows).await
                {
                    tracing::error!(error = %format!("{e:#}"), n = rows.len(), "AUDIT DB WRITE FAILED");
                }
                for done in flushes {
                    let _ = done.send(());
                }
            }
        });
        Self {
            tx,
            ignore_org: None,
        }
    }

    /// Leave this org's events out (the public CA's org: computers without an account).
    pub fn ignoring_org(mut self, org: OrgId) -> Self {
        self.ignore_org = Some(org);
        self
    }

    /// Wait until everything recorded so far is in the database.
    pub async fn flush(&self) {
        let (done, wait) = oneshot::channel();
        if self.tx.send(Msg::Flush(done)).await.is_ok() {
            let _ = wait.await;
        }
    }

    fn ignored(&self, ev: &AuditEvent) -> bool {
        let Some(skip) = self.ignore_org else {
            return false;
        };
        match ev {
            AuditEvent::HostOnline { org_id, .. }
            | AuditEvent::HostOffline { org_id, .. }
            | AuditEvent::OperatorLogin { org_id, .. }
            | AuditEvent::SessionOpened { org_id, .. }
            | AuditEvent::GuestConnect { org_id, .. }
            | AuditEvent::HostEnrolled { org_id, .. }
            | AuditEvent::AnonymousEnrolled { org_id, .. }
            | AuditEvent::PortalAction { org_id, .. } => *org_id == skip,
            _ => false,
        }
    }
}

impl AuditSink for DbAudit {
    fn record(&self, event: AuditEvent) {
        if self.ignored(&event) {
            return;
        }
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        if let Err(e) = self.tx.try_send(Msg::Event(ts, event)) {
            tracing::error!(error = %e, "AUDIT DB QUEUE FULL; event dropped (still in audit.jsonl)");
        }
    }
}

/// The event's fields as a JSON object, without the `event` tag.
fn detail(ev: &AuditEvent) -> serde_json::Value {
    let mut v = serde_json::to_value(ev).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.remove("event");
    }
    v
}

fn row(org: Option<OrgId>, actor: Option<String>, kind: &str, d: serde_json::Value) -> AuditRow {
    AuditRow {
        org_id: org,
        actor_id: actor,
        kind: kind.to_owned(),
        detail: d,
    }
}

/// The rows an event becomes (none for events kept out of the database; one per org for
/// a person's sign-in).
fn rows_for(ev: &AuditEvent, open: &mut HashMap<SessionId, (OrgId, OperatorId)>) -> Vec<AuditRow> {
    let d = detail(ev);
    match ev {
        AuditEvent::HostOnline { org_id, .. } => {
            vec![row(Some(*org_id), None, "computer.online", d)]
        }
        AuditEvent::HostOffline { org_id, .. } => {
            vec![row(Some(*org_id), None, "computer.offline", d)]
        }
        AuditEvent::OperatorLogin {
            org_id,
            operator_id,
        } => vec![row(
            Some(*org_id),
            Some(operator_id.to_string()),
            "sign_in",
            serde_json::json!({ "method": "desktop" }),
        )],
        AuditEvent::SessionOpened {
            org_id,
            session_id,
            operator_id,
            ..
        } => {
            if open.len() < OPEN_SESSIONS_CAP {
                open.insert(*session_id, (*org_id, *operator_id));
            }
            vec![row(
                Some(*org_id),
                Some(operator_id.to_string()),
                "session.start",
                d,
            )]
        }
        AuditEvent::GuestConnect {
            org_id,
            operator_id,
            ..
        } => vec![row(
            Some(*org_id),
            Some(operator_id.to_string()),
            "session.guest",
            d,
        )],
        AuditEvent::SessionEnded { session_id, .. } => {
            let (org, actor) = match open.remove(session_id) {
                Some((o, a)) => (Some(o), Some(a.to_string())),
                None => (None, None),
            };
            vec![row(org, actor, "session.end", d)]
        }
        AuditEvent::HostEnrolled {
            org_id,
            operator_id,
            ..
        } => vec![row(
            Some(*org_id),
            Some(operator_id.to_string()),
            "computer.enrolled",
            d,
        )],
        AuditEvent::SessionRefused { operator_id, .. } => {
            vec![row(
                None,
                Some(operator_id.to_string()),
                "session.refused",
                d,
            )]
        }
        AuditEvent::PortalSignIn { user_id, orgs, .. } => {
            per_org(orgs, Some(user_id), "sign_in", d)
        }
        AuditEvent::PortalSignInFailed { orgs, .. } => per_org(orgs, None, "sign_in.failed", d),
        AuditEvent::PortalAction {
            org_id,
            actor_id,
            kind,
            target,
            ip,
            detail,
        } => vec![row(
            Some(*org_id),
            Some(actor_id.to_string()),
            kind,
            serde_json::json!({ "target": target, "ip": ip, "detail": detail }),
        )],
        AuditEvent::AuthFailed { .. }
        | AuditEvent::GuestRefused { .. }
        | AuditEvent::AnonymousEnrolled { .. }
        | AuditEvent::AnonymousEnrollRefused { .. } => vec![],
    }
}

fn per_org(
    orgs: &[OrgId],
    actor: Option<&OperatorId>,
    kind: &str,
    d: serde_json::Value,
) -> Vec<AuditRow> {
    let actor = actor.map(|a| a.to_string());
    if orgs.is_empty() {
        return vec![row(None, actor, kind, d)];
    }
    orgs.iter()
        .map(|o| row(Some(*o), actor.clone(), kind, d.clone()))
        .collect()
}

async fn insert(db: &Db, rows: &[(i64, AuditRow)]) -> anyhow::Result<()> {
    let mut tx = db.pool().begin().await?;
    for (ts, r) in rows {
        sqlx::query(
            "INSERT INTO audit (org_id, ts_ms, actor_id, event, detail)
             SELECT ?1, ?2, ?3, ?4, ?5
             WHERE ?1 IS NULL OR EXISTS (SELECT 1 FROM orgs WHERE id = ?1)",
        )
        .bind(r.org_id.map(|o| o.to_string()))
        .bind(ts)
        .bind(r.actor_id.as_deref())
        .bind(&r.kind)
        .bind(serde_json::to_string(&r.detail)?)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}
