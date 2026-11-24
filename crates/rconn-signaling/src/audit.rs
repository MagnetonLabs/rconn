//! Audit log: who connected to what, when, and what was refused.
//!
//! Self-hosted servers append JSON lines to `<data-dir>/audit.jsonl`; the hosted service
//! will implement [`AuditSink`] on Postgres. Records are written before the action they
//! describe is acknowledged to the client, so a session never exists without its record.

use rconn_core::ids::{HostId, OperatorId, OrgId, SessionId};
use rconn_core::protocol::EndReason;
use serde::Serialize;
use std::fs::{File, OpenOptions};
use std::io::Write;
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
