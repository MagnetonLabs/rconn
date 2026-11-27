//! Computer inventory: what org hosts report when they come online, and when they were
//! last seen. Computers without an account (the public CA) are never recorded.
//!
//! The broker reports events from its connection handlers, some while it holds the
//! registry lock, so [`DbInventory`] only puts them on a bounded queue; one writer task
//! applies them in order. When the queue is full an event is dropped (and logged): the
//! inventory is informational, and a slow disk must never stall signaling.

use crate::db::Db;
use async_trait::async_trait;
use rconn_core::ids::{HostId, OperatorId, OrgId};
use rconn_core::protocol::{MAX_HOST_CAPABILITIES, MAX_HOST_FACT_CHARS, tidy_host_name};
use std::net::IpAddr;
use tokio::sync::mpsc;

/// Events queued for the writer before new ones are dropped.
pub const INVENTORY_QUEUE: usize = 4096;

/// What a host said about itself in `HostHello`, tidied for storage and display.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostFacts {
    pub label: Option<String>,
    pub os: Option<String>,
    pub version: Option<String>,
    pub capabilities: Vec<String>,
}

fn tidy_fact(s: &str) -> Option<String> {
    let clean: String = s
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_HOST_FACT_CHARS)
        .collect();
    Some(clean.trim().to_owned()).filter(|s| !s.is_empty())
}

impl HostFacts {
    /// Bound and clean everything a host sent (it is untrusted input).
    pub fn tidy(
        label: Option<&str>,
        os: Option<&str>,
        version: Option<&str>,
        capabilities: &[String],
    ) -> Self {
        let mut caps: Vec<String> = capabilities
            .iter()
            .filter_map(|c| tidy_fact(c))
            .take(MAX_HOST_CAPABILITIES)
            .collect();
        caps.sort();
        caps.dedup();
        Self {
            label: label.and_then(tidy_host_name),
            os: os.and_then(tidy_fact),
            version: version.and_then(tidy_fact),
            capabilities: caps,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryEvent {
    Online {
        org: OrgId,
        host: HostId,
        facts: HostFacts,
        at: i64,
    },
    Offline {
        org: OrgId,
        host: HostId,
        at: i64,
    },
    Enrolled {
        org: OrgId,
        host: HostId,
        by: Option<OperatorId>,
        at: i64,
    },
}

/// Where the broker reports org hosts. Must not block.
pub trait HostInventory: Send + Sync + 'static {
    fn record(&self, event: InventoryEvent);
}

/// Writes inventory events to the database from a background task.
pub struct DbInventory {
    tx: mpsc::Sender<InventoryEvent>,
}

impl DbInventory {
    /// Start the writer task (needs a Tokio runtime).
    pub fn spawn(db: Db) -> Self {
        let (tx, mut rx) = mpsc::channel::<InventoryEvent>(INVENTORY_QUEUE);
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if let Err(e) = apply(&db, &ev).await {
                    tracing::warn!(error = %format!("{e:#}"), ?ev, "inventory write failed");
                }
            }
        });
        Self { tx }
    }
}

async fn apply(db: &Db, ev: &InventoryEvent) -> anyhow::Result<()> {
    match ev {
        InventoryEvent::Online {
            org,
            host,
            facts,
            at,
        } => db.host_online(*org, *host, facts, *at).await,
        InventoryEvent::Offline { org, host, at } => db.host_offline(*org, *host, *at).await,
        InventoryEvent::Enrolled { org, host, by, at } => {
            db.host_enrolled(*org, *host, *by, *at).await
        }
    }
}

impl HostInventory for DbInventory {
    fn record(&self, event: InventoryEvent) {
        if let Err(e) = self.tx.try_send(event) {
            tracing::warn!(error = %e, "inventory queue full; event dropped");
        }
    }
}

/// Why an enrollment token was not accepted. The computer gets [`TokenRefusal::message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenRefusal {
    /// Unknown, deleted, expired or used up (one answer, so tokens can't be probed).
    Invalid,
    /// The public key isn't an Ed25519 key.
    BadKey,
    /// The server can't issue for the token's company, or the database failed.
    Internal,
}

impl TokenRefusal {
    pub fn message(self) -> &'static str {
        match self {
            Self::Invalid => "this enrollment token is unknown, expired or used up",
            Self::BadKey => "not a valid Ed25519 public key",
            Self::Internal => "the server could not enroll this computer; try again later",
        }
    }
}

/// Turns enrollment tokens into host certificates (the portal's tokens, in the database).
#[async_trait]
pub trait TokenEnroller: Send + Sync + 'static {
    /// Certify `public_key_der` as `host_id` in the token's company, counting one use of
    /// the token. Returns the company and the certificate.
    async fn enroll(
        &self,
        token: &str,
        host_id: HostId,
        public_key_der: &[u8],
        source: Option<IpAddr>,
    ) -> Result<(OrgId, String), TokenRefusal>;
}
