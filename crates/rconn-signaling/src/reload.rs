//! Live reload of the self-hosted data dir: operators, accounts and revocations take
//! effect without restarting the server. Revoked hosts are evicted immediately.

use crate::AppState;
use crate::auth::OperatorIdentity;
use crate::fleet_auth::FleetAuthenticator;
use crate::store::{DataDir, unhex32};
use anyhow::{Context, Result};
use rconn_core::ids::OrgId;
use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

/// Last-seen modification times, so unchanged files aren't re-read.
#[derive(Default)]
pub struct Seen(HashMap<&'static str, SystemTime>);

impl Seen {
    fn changed(&mut self, store: &DataDir, name: &'static str) -> bool {
        let Ok(m) = std::fs::metadata(store.path().join(name)).and_then(|m| m.modified()) else {
            return false;
        };
        self.0.insert(name, m) != Some(m)
    }
}

/// Apply whatever changed since the last call.
pub async fn reload_once(
    store: &DataDir,
    auth: &FleetAuthenticator,
    state: &AppState,
    org: OrgId,
    seen: &mut Seen,
) -> Result<()> {
    if seen.changed(store, "operators.json") {
        let mut table = HashMap::new();
        for (digest_hex, operator_id) in store.operators()? {
            let digest = unhex32(&digest_hex).context("corrupt operators.json")?;
            table.insert(
                digest,
                OperatorIdentity {
                    operator_id,
                    org_id: org,
                },
            );
        }
        tracing::info!(operators = table.len(), "reloaded operator tokens");
        auth.replace_operator_digests(table);
    }
    if seen.changed(store, "accounts.json") {
        let accounts = store.accounts()?;
        tracing::info!(accounts = accounts.0.len(), "reloaded accounts");
        auth.replace_accounts(org, accounts);
    }
    if seen.changed(store, "revoked.json") {
        let set: HashSet<_> = store.revoked()?.into_iter().collect();
        for host in auth.replace_revoked(set) {
            tracing::warn!(%host, "host revoked; evicting");
            state.evict_host(host).await;
        }
    }
    Ok(())
}
