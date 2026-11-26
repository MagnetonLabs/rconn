//! Live reload: org CAs, operator tokens, accounts and revocations follow the database
//! without a restart. The server polls the database's config version (bumped by
//! triggers on every relevant change) and reloads everything when it moves. Hosts that
//! became revoked are evicted at once.

use crate::AppState;
use crate::db::Db;
use crate::fleet_auth::FleetAuthenticator;
use anyhow::Result;

/// The config version last applied.
#[derive(Default)]
pub struct Seen(Option<i64>);

/// Apply the database to `auth` if it changed since the last call. Returns whether it did.
pub async fn reload_once(
    db: &Db,
    auth: &FleetAuthenticator,
    state: &AppState,
    seen: &mut Seen,
) -> Result<bool> {
    let version = db.config_version().await?;
    if seen.0 == Some(version) {
        return Ok(false);
    }
    let cas = db.org_cas().await?;
    let tokens = db.operator_tokens().await?;
    let revoked = db.revoked_hosts().await?;
    // Password accounts belong to the legacy org until people sign in through the portal.
    let accounts = match db.legacy_org().await? {
        Some(org) => Some((org.id, db.accounts(org.id).await?)),
        None => None,
    };
    tracing::info!(
        version,
        orgs = cas.len(),
        operator_tokens = tokens.len(),
        accounts = accounts.as_ref().map_or(0, |(_, a)| a.0.len()),
        revoked = revoked.len(),
        "loaded configuration from the database"
    );
    auth.replace_org_cas(cas);
    auth.replace_operator_digests(tokens);
    if let Some((org, accounts)) = accounts {
        auth.replace_accounts(org, accounts);
    }
    for host in auth.replace_revoked(revoked) {
        tracing::warn!(%host, "host revoked; evicting");
        state.evict_host(host).await;
    }
    seen.0 = Some(version);
    Ok(true)
}
