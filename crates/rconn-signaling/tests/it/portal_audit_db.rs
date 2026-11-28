//! The database audit sink: queued, batched, org-scoped rows; the public org and
//! unauthenticated noise stay out.

use rconn_core::fleet::FleetCa;
use rconn_core::ids::{HostId, OperatorId, OrgId, SessionId};
use rconn_core::protocol::EndReason;
use rconn_signaling::audit::{AuditEvent, AuditSink, AuthRole};
use rconn_signaling::audit_db::DbAudit;
use rconn_signaling::db::Db;

async fn db_with_orgs(orgs: &[OrgId]) -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("rconn.db")).await.unwrap();
    for o in orgs {
        sqlx::query("INSERT INTO orgs (id, name, created_at, ca_cert_pem) VALUES (?, 'o', 0, ?)")
            .bind(o.to_string())
            .bind(FleetCa::generate(*o).unwrap().cert_pem())
            .execute(db.pool())
            .await
            .unwrap();
    }
    (dir, db)
}

async fn rows(db: &Db) -> Vec<(Option<String>, Option<String>, String)> {
    sqlx::query_as("SELECT org_id, actor_id, event FROM audit ORDER BY id")
        .fetch_all(db.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn events_land_in_their_org_in_order() {
    let (org, public) = (OrgId::new(), OrgId::new());
    let (_dir, db) = db_with_orgs(&[org]).await;
    let audit = DbAudit::spawn(db.clone()).ignoring_org(public);
    let (host, op, session) = (HostId::new(), OperatorId::new(), SessionId::new());
    audit.record(AuditEvent::HostOnline {
        org_id: org,
        host_id: host,
    });
    audit.record(AuditEvent::SessionOpened {
        org_id: org,
        session_id: session,
        host_id: host,
        operator_id: op,
    });
    audit.record(AuditEvent::SessionEnded {
        session_id: session,
        reason: EndReason::Normal,
    });
    // Left out: the public org's computers and unauthenticated noise.
    audit.record(AuditEvent::HostOnline {
        org_id: public,
        host_id: HostId::new(),
    });
    audit.record(AuditEvent::AuthFailed {
        role: AuthRole::Host,
        reason: "bad".into(),
    });
    audit.record(AuditEvent::GuestRefused {
        reason: "rate".into(),
    });
    // An org that no longer exists doesn't break the batch.
    audit.record(AuditEvent::HostOffline {
        org_id: OrgId::new(),
        host_id: host,
    });
    audit.record(AuditEvent::HostOffline {
        org_id: org,
        host_id: host,
    });
    audit.flush().await;
    let o = Some(org.to_string());
    let a = Some(op.to_string());
    assert_eq!(
        rows(&db).await,
        vec![
            (o.clone(), None, "computer.online".into()),
            (o.clone(), a.clone(), "session.start".into()),
            (o.clone(), a, "session.end".into()),
            (o, None, "computer.offline".into()),
        ]
    );
}

#[tokio::test]
async fn bursts_are_written_in_batches_without_loss() {
    let org = OrgId::new();
    let (_dir, db) = db_with_orgs(&[org]).await;
    let audit = DbAudit::spawn(db.clone());
    for _ in 0..2000 {
        audit.record(AuditEvent::HostOnline {
            org_id: org,
            host_id: HostId::new(),
        });
    }
    audit.flush().await;
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit WHERE org_id = ?")
        .bind(org.to_string())
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(n, 2000);
}
