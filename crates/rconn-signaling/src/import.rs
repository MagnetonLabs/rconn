//! Moving a data dir from files to the database.
//!
//! Before the portal, a server kept its state in files (see [`crate::store`]). On every
//! start (and every CLI command), [`open`] imports whatever of them is still there:
//!
//! | File             | Becomes                                                          |
//! |------------------|------------------------------------------------------------------|
//! | `ca.pem`, `ca.key.pem` | the legacy org (id from the certificate), key sealed. The files stay, for older tools. |
//! | `accounts.json`  | users (operator id = user id), accounts, technician memberships  |
//! | `operators.json` | users (operator id = user id), static api_tokens, memberships     |
//! | `revoked.json`   | `hosts.revoked_at` (computers without an account: `revoked_public_hosts`) |
//!
//! `public-hosts.jsonl` stays a file: computers without an account are not inventory.
//! Each import is one transaction, and only after it commits are the files renamed to
//! `*.imported`. Rows that already exist are left alone, so importing again (after a
//! crash before the rename, or a file written by an old script) changes nothing twice.

use crate::accounts::Accounts;
use crate::db::{self, Db, IMPORTED_ROLE};
use crate::secrets::MasterKey;
use crate::store::{DataDir, unhex32};
use anyhow::{Context, Result, bail};
use rconn_core::ids::{HostId, OperatorId, OrgId};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// Files imported and then renamed.
pub const LEGACY_FILES: [&str; 3] = ["accounts.json", "operators.json", "revoked.json"];

/// Name given to the org imported from `ca.pem`.
pub const LEGACY_ORG_NAME: &str = "Default organization";

/// What one import did. All zero (and nothing renamed) when there was nothing to do.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportReport {
    pub org_created: Option<OrgId>,
    pub users: usize,
    pub accounts: usize,
    pub tokens: usize,
    pub revoked: usize,
    pub renamed: Vec<PathBuf>,
}

impl ImportReport {
    pub fn is_noop(&self) -> bool {
        *self == Self::default()
    }
}

/// Any legacy file still waiting to be imported?
pub fn pending(store: &DataDir) -> bool {
    LEGACY_FILES.iter().any(|f| store.path().join(f).exists())
}

/// Open the data dir's database and the master key, then import the legacy files.
pub async fn open(
    store: &DataDir,
    master_key_path: &Path,
) -> Result<(Db, MasterKey, ImportReport)> {
    let db = Db::open_in(store.path()).await?;
    let master = master_key(&db, store, master_key_path).await?;
    let report = import(&db, store, &master).await?;
    if !report.is_noop() {
        tracing::info!(?report, "imported data-dir files into the database");
    }
    Ok((db, master, report))
}

/// Load the master key, or create it while nothing is sealed with it yet.
async fn master_key(db: &Db, store: &DataDir, path: &Path) -> Result<MasterKey> {
    let sealed = db.sealed_key_orgs().await?;
    let loaded = MasterKey::load(path)?;
    if loaded.is_none() && sealed.is_empty() {
        let key = MasterKey::create(path)?;
        tracing::info!(path = %path.display(), "created the server master key; keep it out of data-dir backups");
        return Ok(key);
    }
    // The only sealed key is the legacy org's and its plaintext file is still in the data
    // dir: if the master key is lost (or a different one, e.g. a container started without
    // its key volume), nothing is lost by sealing the CA key again under the key at hand.
    let legacy = db.legacy_org().await?;
    let reseal = match &legacy {
        Some(org) if sealed == [org.id] => store
            .load_ca()
            .ok()
            .filter(|ca| ca.org_id() == org.id && ca.cert_pem() == org.ca_cert_pem),
        _ => None,
    };
    if let Some(key) = loaded {
        if let (Some(org), Some(ca)) = (&legacy, &reseal)
            && db.org_ca(org.id, &key).await.is_err()
        {
            db.reseal_org_ca(org.id, &key.seal(org.id, ca.key_pem().as_bytes())?)
                .await?;
            tracing::warn!(path = %path.display(), "the CA key was sealed with another master key; sealed it again from ca.key.pem");
        }
        return Ok(key);
    }
    if let (Some(org), Some(ca)) = (&legacy, &reseal) {
        let key = MasterKey::create(path)?;
        db.reseal_org_ca(org.id, &key.seal(org.id, ca.key_pem().as_bytes())?)
            .await?;
        tracing::warn!(path = %path.display(), "master key was missing; created a new one and sealed the CA key from ca.key.pem again");
        return Ok(key);
    }
    bail!(
        "the master key {} is missing, but the database holds CA keys sealed with it; \
         restore that file (or point {} at it)",
        path.display(),
        crate::secrets::MASTER_KEY_ENV
    )
}

/// Import whatever legacy files are in `store` (see the module docs).
pub async fn import(db: &Db, store: &DataDir, master: &MasterKey) -> Result<ImportReport> {
    let mut report = ImportReport::default();
    let ca = read_ca(store)?;
    let present: Vec<&str> = LEGACY_FILES
        .into_iter()
        .filter(|f| store.path().join(f).exists())
        .collect();

    let mut tx = db.pool().begin().await?;
    let legacy: Option<(String, Option<Vec<u8>>)> =
        sqlx::query_as("SELECT id, ca_key_sealed FROM orgs WHERE legacy = 1")
            .fetch_optional(&mut *tx)
            .await?;
    let org = match (legacy, &ca) {
        (Some((id, sealed)), ca) => {
            let org = OrgId(id.parse().context("bad org id in the database")?);
            if let Some(ca) = ca {
                anyhow::ensure!(
                    ca.org == org,
                    "ca.pem is for org {}, but this server's org is {org}",
                    ca.org
                );
                if let (None, Some(key)) = (sealed, &ca.key_pem) {
                    sqlx::query("UPDATE orgs SET ca_key_sealed = ? WHERE id = ?")
                        .bind(master.seal(org, key.as_bytes())?)
                        .bind(org.to_string())
                        .execute(&mut *tx)
                        .await?;
                }
            }
            org
        }
        (None, Some(ca)) => {
            let org = ca.org;
            let sealed = match &ca.key_pem {
                Some(key) => Some(master.seal(org, key.as_bytes())?),
                None => None,
            };
            sqlx::query(
                "INSERT INTO orgs (id, name, legacy, created_at, ca_cert_pem, ca_key_sealed)
                 VALUES (?, ?, 1, ?, ?, ?)",
            )
            .bind(org.to_string())
            .bind(LEGACY_ORG_NAME)
            .bind(db::now())
            .bind(&ca.cert_pem)
            .bind(sealed)
            .execute(&mut *tx)
            .await?;
            report.org_created = Some(org);
            org
        }
        (None, None) => {
            if !present.is_empty() {
                tracing::warn!(files = ?present, "no ca.pem: nothing to import these into (run `init`)");
            }
            return Ok(report);
        }
    };

    let path = |f: &str| store.path().join(f);
    if present.contains(&"accounts.json") {
        let accounts: Accounts = read_json(&path("accounts.json"))?;
        for (username, a) in &accounts.0 {
            import_user(&mut tx, &mut report, org, a.operator_id, username).await?;
            let n = sqlx::query(
                "INSERT INTO accounts (username, user_id, password_hash, created_at)
                 VALUES (?, ?, ?, ?) ON CONFLICT (username) DO NOTHING",
            )
            .bind(username)
            .bind(a.operator_id.to_string())
            .bind(&a.password_hash)
            .bind(db::now())
            .execute(&mut *tx)
            .await?
            .rows_affected();
            report.accounts += n as usize;
        }
    }
    if present.contains(&"operators.json") {
        let ops: BTreeMap<String, OperatorId> = read_json(&path("operators.json"))?;
        for (digest_hex, operator) in &ops {
            unhex32(digest_hex).context("corrupt operators.json")?;
            import_user(&mut tx, &mut report, org, *operator, "operator token").await?;
            if db::insert_static_token(&mut tx, org, *operator, digest_hex).await? {
                report.tokens += 1;
            }
        }
    }
    if present.contains(&"revoked.json") {
        let revoked: Vec<HostId> = read_json(&path("revoked.json"))?;
        let public = crate::public::registry_ids(&path("public-hosts.jsonl"))?;
        report.revoked += import_revoked(&mut tx, org, &revoked, &public).await?;
    }
    tx.commit().await?;

    for f in present {
        let from = path(f);
        let to = free_name(&from);
        match std::fs::rename(&from, &to) {
            Ok(()) => report.renamed.push(to),
            // Imported already; the next start imports it again, which changes nothing.
            Err(e) => {
                tracing::warn!(file = %from.display(), error = %e, "imported but couldn't rename")
            }
        }
    }
    Ok(report)
}

/// The legacy CA files: the certificate names the org; the key may already be gone.
struct CaFiles {
    org: OrgId,
    cert_pem: String,
    key_pem: Option<String>,
}

fn read_ca(store: &DataDir) -> Result<Option<CaFiles>> {
    let (cert, key) = (store.path().join("ca.pem"), store.path().join("ca.key.pem"));
    if !cert.exists() {
        return Ok(None);
    }
    if key.exists() {
        // Checks that the key matches the certificate.
        let ca = store.load_ca()?;
        return Ok(Some(CaFiles {
            org: ca.org_id(),
            cert_pem: ca.cert_pem().to_owned(),
            key_pem: Some(ca.key_pem().to_owned()),
        }));
    }
    let cert_pem = std::fs::read_to_string(&cert).context("reading ca.pem")?;
    let org = rconn_core::fleet::claimed_org(&cert_pem).context("ca.pem names no org")?;
    Ok(Some(CaFiles {
        org,
        cert_pem,
        key_pem: None,
    }))
}

async fn import_user(
    tx: &mut db::Tx<'_>,
    report: &mut ImportReport,
    org: OrgId,
    id: OperatorId,
    name: &str,
) -> Result<()> {
    if db::insert_user(tx, id, None, Some(name)).await? {
        report.users += 1;
    }
    db::insert_membership(tx, id, org, IMPORTED_ROLE).await?;
    Ok(())
}

async fn import_revoked(
    tx: &mut db::Tx<'_>,
    org: OrgId,
    revoked: &[HostId],
    public: &HashSet<HostId>,
) -> Result<usize> {
    let now = db::now();
    let mut n = 0;
    for host in revoked {
        let changed = if public.contains(host) {
            sqlx::query(
                "INSERT INTO revoked_public_hosts (host_id, revoked_at) VALUES (?, ?)
                 ON CONFLICT (host_id) DO NOTHING",
            )
            .bind(host.to_string())
            .bind(now)
            .execute(&mut **tx)
            .await?
            .rows_affected()
        } else {
            sqlx::query(
                "INSERT INTO hosts (id, org_id, revoked_at) VALUES (?, ?, ?)
                 ON CONFLICT (id) DO UPDATE SET revoked_at = excluded.revoked_at
                 WHERE hosts.revoked_at IS NULL",
            )
            .bind(host.to_string())
            .bind(org.to_string())
            .bind(now)
            .execute(&mut **tx)
            .await?
            .rows_affected()
        };
        n += changed as usize;
    }
    Ok(n)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// `x.imported`, or `x.imported.2`, `.3`, … if an earlier import left that name.
fn free_name(from: &Path) -> PathBuf {
    let base = format!("{}.imported", from.display());
    let mut candidate = PathBuf::from(&base);
    let mut n = 2;
    while candidate.exists() {
        candidate = PathBuf::from(format!("{base}.{n}"));
        n += 1;
    }
    candidate
}
