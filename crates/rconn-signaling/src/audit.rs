//! Audit log: who connected to what, when, and what was refused.
//!
//! Servers append JSON lines to `<data-dir>/audit.jsonl` and, through
//! [`crate::audit_db::DbAudit`], to the database's `audit` table that the portal shows.
//! The file record is written before the action it describes is acknowledged to the
//! client, so a session never exists without its record.

use rconn_core::ids::{HostId, OperatorId, OrgId, SessionId};
use rconn_core::protocol::EndReason;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AuditEvent {
    HostOnline {
        org_id: OrgId,
        host_id: HostId,
    },
    HostOffline {
        org_id: OrgId,
        host_id: HostId,
    },
    OperatorLogin {
        org_id: OrgId,
        operator_id: OperatorId,
    },
    AuthFailed {
        role: AuthRole,
        reason: String,
    },
    SessionOpened {
        org_id: OrgId,
        session_id: SessionId,
        host_id: HostId,
        operator_id: OperatorId,
    },
    SessionEnded {
        session_id: SessionId,
        reason: EndReason,
    },
    /// Someone without an account asked for a computer by its ID (access password).
    /// Whether the password was right is decided, and logged, by the host.
    GuestConnect {
        org_id: OrgId,
        host_id: HostId,
        /// The temporary id this guest's session carries as its operator.
        operator_id: OperatorId,
    },
    /// A guest request refused before reaching the host (unknown ID, rate limit).
    GuestRefused {
        reason: String,
    },
    /// An operator turned a computer into a host (self-service enrollment).
    HostEnrolled {
        org_id: OrgId,
        host_id: HostId,
        operator_id: OperatorId,
    },
    /// A computer without an account got an ID (owned by nobody).
    AnonymousEnrolled {
        org_id: OrgId,
        host_id: HostId,
        source: Option<IpAddr>,
    },
    /// An account-less enrollment refused (disabled, rate limit, bad key).
    AnonymousEnrollRefused {
        source: Option<IpAddr>,
        reason: String,
    },
    /// A signed-in operator asked for a session to a host they don't own (or that is
    /// offline). Hosts owned by nobody always need their access password.
    SessionRefused {
        host_id: HostId,
        operator_id: OperatorId,
        reason: String,
    },
    /// A session to an org computer refused by the organization's policy (outside the
    /// allowed hours, access password not allowed, host too old to enforce it).
    PolicyRefused {
        org_id: OrgId,
        host_id: HostId,
        operator_id: OperatorId,
        guest: bool,
        reason: String,
    },
    /// Someone signed in to the admin portal. `orgs`: the orgs they are a member of (the
    /// event shows in each one's log).
    PortalSignIn {
        user_id: OperatorId,
        /// `password`, `magic_link`, `sso`, `passkey`.
        method: String,
        ip: Option<IpAddr>,
        orgs: Vec<OrgId>,
    },
    /// A portal sign-in refused. `orgs` is empty when nobody has this email.
    PortalSignInFailed {
        email: String,
        method: String,
        reason: String,
        ip: Option<IpAddr>,
        orgs: Vec<OrgId>,
    },
    /// An admin action in the portal. `kind` is the portal's audit kind
    /// (`computer.renamed`, `person.invited`, `policy.changed`, ...).
    PortalAction {
        org_id: OrgId,
        actor_id: OperatorId,
        kind: String,
        target: Option<AuditTarget>,
        ip: Option<IpAddr>,
        /// A JSON object with the action's fields (old and new values, ...).
        detail: serde_json::Value,
    },
}

/// What a portal action was done to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditTarget {
    /// `computer`, `person`, `invite`, `group`, `policy`, `token`, `org`, `sso`.
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthRole {
    Host,
    Operator,
}

pub trait AuditSink: Send + Sync + 'static {
    fn record(&self, event: AuditEvent);
}

/// Discards everything. For tests that don't care.
pub struct NoAudit;

impl AuditSink for NoAudit {
    fn record(&self, _event: AuditEvent) {}
}

#[derive(Serialize)]
struct Line<'a> {
    /// Unix milliseconds.
    ts: u128,
    #[serde(flatten)]
    event: &'a AuditEvent,
}

/// Append-only JSON-lines file, flushed per record.
pub struct FileAudit(Mutex<File>);

impl FileAudit {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        Ok(Self(Mutex::new(
            OpenOptions::new().create(true).append(true).open(path)?,
        )))
    }
}

impl AuditSink for FileAudit {
    fn record(&self, event: AuditEvent) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let Ok(mut json) = serde_json::to_string(&Line { ts, event: &event }) else {
            return;
        };
        json.push('\n');
        let mut f = self.0.lock().expect("audit lock poisoned");
        if let Err(e) = f.write_all(json.as_bytes()).and_then(|_| f.flush()) {
            // Losing audit records silently would be worse than noisy logs.
            tracing::error!(error = %e, ?event, "AUDIT WRITE FAILED");
        }
    }
}

/// Sends every event to several sinks (the JSONL file and the database).
pub struct TeeAudit(pub Vec<std::sync::Arc<dyn AuditSink>>);

impl AuditSink for TeeAudit {
    fn record(&self, event: AuditEvent) {
        if let Some((last, rest)) = self.0.split_last() {
            for sink in rest {
                sink.record(event.clone());
            }
            last.record(event);
        }
    }
}

/// Collects events in memory. For tests.
#[derive(Default)]
pub struct MemoryAudit(pub Mutex<Vec<AuditEvent>>);

impl AuditSink for MemoryAudit {
    fn record(&self, event: AuditEvent) {
        self.0.lock().expect("audit lock poisoned").push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_audit_appends_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let audit = FileAudit::open(&path).unwrap();
        let (org_id, host_id) = (OrgId::new(), HostId::new());
        audit.record(AuditEvent::HostOnline { org_id, host_id });
        audit.record(AuditEvent::AuthFailed {
            role: AuthRole::Operator,
            reason: "invalid credentials".into(),
        });
        drop(audit);
        let again = FileAudit::open(&path).unwrap();
        again.record(AuditEvent::HostOffline { org_id, host_id });

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 3, "reopening must append, not truncate");
        assert_eq!(lines[0]["event"], "host_online");
        assert_eq!(lines[1]["role"], "operator");
        assert!(lines[2]["ts"].as_u64().unwrap() > 1_700_000_000_000);
    }
}
