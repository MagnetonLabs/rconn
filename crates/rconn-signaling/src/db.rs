//! The server database (SQLite, WAL): orgs, people, computers and tokens.
//!
//! `<data-dir>/rconn.db`, created and migrated on open (`migrations/`). Queries are
//! checked at run time, so building needs no database. Every function that reads or
//! writes org-owned rows takes the org and filters by it.

use crate::accounts::{Account, Accounts, MIN_PASSWORD_LEN};
use crate::auth::OperatorIdentity;
use crate::hosts::HostFacts;
use crate::secrets::MasterKey;
use crate::store::{hex, unhex32};
use anyhow::{Context, Result, bail};
use rconn_core::fleet::FleetCa;
use rconn_core::ids::{HostId, OperatorId, OrgId};
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;
use uuid::Uuid;

/// File name of the database inside the data dir.
pub const DB_FILE: &str = "rconn.db";

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// A role in an org, most to least powerful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Owner,
    Admin,
    Technician,
    Viewer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Technician => "technician",
            Self::Viewer => "viewer",
        }
    }
}

impl FromStr for Role {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "owner" => Self::Owner,
            "admin" => Self::Admin,
            "technician" => Self::Technician,
            "viewer" => Self::Viewer,
            _ => bail!("unknown role {s:?}"),
        })
    }
}

/// Role given to people and tokens carried over from the files (accounts.json and
/// operators.json): they could open sessions to the org's computers, nothing more.
pub const IMPORTED_ROLE: Role = Role::Technician;

#[derive(Debug, Clone)]
pub struct Org {
    pub id: OrgId,
    pub name: String,
    pub legacy: bool,
    pub ca_cert_pem: String,
    pub ca_key_sealed: Option<Vec<u8>>,
}

/// One computer's inventory row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRow {
    pub id: HostId,
    pub org_id: OrgId,
    pub admin_name: Option<String>,
    pub hostname: Option<String>,
    pub os: Option<String>,
    pub version: Option<String>,
    pub capabilities: Vec<String>,
    pub enrolled_at: Option<i64>,
    pub enrolled_by: Option<String>,
    pub last_seen: Option<i64>,
    pub group_id: Option<String>,
    pub revoked_at: Option<i64>,
}

/// What `revoke_host` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revoked {
    /// A computer in an org's inventory.
    OrgHost(OrgId),
    /// A computer without an account.
    PublicHost,
    /// Already revoked.
    Already,
}

#[derive(Clone, Debug)]
pub struct Db {
    pool: SqlitePool,
    path: PathBuf,
}

pub fn now() -> i64 {
    crate::unix_now() as i64
}

fn uuid(s: &str) -> Result<Uuid> {
    Uuid::parse_str(s).with_context(|| format!("bad id {s:?} in the database"))
}

impl Db {
    /// Open (creating and migrating) the database at `path`.
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        restrict(path);
        MIGRATOR
            .run(&pool)
            .await
            .with_context(|| format!("migrating {}", path.display()))?;
        Ok(Self {
            pool,
            path: path.to_owned(),
        })
    }

    /// The database in a data dir.
    pub async fn open_in(data_dir: &Path) -> Result<Self> {
        Self::open(&data_dir.join(DB_FILE)).await
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Changes whenever something the running server caches changes (see the triggers in
    /// the migrations): CAs, tokens, accounts, memberships, revocations.
    pub async fn config_version(&self) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT version FROM config_version WHERE id = 1")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    // -- orgs ---------------------------------------------------------------------------

    pub async fn orgs(&self) -> Result<Vec<Org>> {
        let rows = sqlx::query(
            "SELECT id, name, legacy, ca_cert_pem, ca_key_sealed FROM orgs ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(Org {
                    id: OrgId(uuid(r.get("id"))?),
                    name: r.get("name"),
                    legacy: r.get::<i64, _>("legacy") == 1,
                    ca_cert_pem: r.get("ca_cert_pem"),
                    ca_key_sealed: r.get("ca_key_sealed"),
                })
            })
            .collect()
    }

    /// The org that existed before the portal (org #1), if any.
    pub async fn legacy_org(&self) -> Result<Option<Org>> {
        Ok(self.orgs().await?.into_iter().find(|o| o.legacy))
    }

    /// The legacy org, or an error telling the admin to run `init`.
    pub async fn require_legacy_org(&self) -> Result<Org> {
        self.legacy_org()
            .await?
            .context("no org on this server yet (run `init` first)")
    }

    /// Every org's fleet CA certificate.
    pub async fn org_cas(&self) -> Result<HashMap<OrgId, String>> {
        Ok(self
            .orgs()
            .await?
            .into_iter()
            .map(|o| (o.id, o.ca_cert_pem))
            .collect())
    }

    /// Orgs whose CA key is sealed in the database.
    pub async fn sealed_key_orgs(&self) -> Result<Vec<OrgId>> {
        Ok(self
            .orgs()
            .await?
            .into_iter()
            .filter(|o| o.ca_key_sealed.is_some())
            .map(|o| o.id)
            .collect())
    }

    /// `org`'s fleet CA with its private key, opened with the master key.
    pub async fn org_ca(&self, org: OrgId, master: &MasterKey) -> Result<FleetCa> {
        let o = self
            .orgs()
            .await?
            .into_iter()
            .find(|o| o.id == org)
            .with_context(|| format!("no org {org}"))?;
        let sealed = o
            .ca_key_sealed
            .with_context(|| format!("org {org} has no CA key on this server"))?;
        let key = String::from_utf8(master.open(org, &sealed)?).context("CA key is not PEM")?;
        let ca = FleetCa::from_pem(&o.ca_cert_pem, &key)?;
        anyhow::ensure!(ca.org_id() == org, "CA certificate belongs to another org");
        Ok(ca)
    }

    /// Replace `org`'s sealed CA key (after re-keying the master key).
    pub async fn reseal_org_ca(&self, org: OrgId, sealed: &[u8]) -> Result<()> {
        sqlx::query("UPDATE orgs SET ca_key_sealed = ? WHERE id = ?")
            .bind(sealed)
            .bind(org.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // -- people -------------------------------------------------------------------------

    /// Username/password accounts of `org`'s members (disabled people left out).
    pub async fn accounts(&self, org: OrgId) -> Result<Accounts> {
        let rows = sqlx::query(
            "SELECT a.username, a.user_id, a.password_hash FROM accounts a
             JOIN memberships m ON m.user_id = a.user_id AND m.org_id = ?
             JOIN users u ON u.id = a.user_id
             WHERE u.disabled_at IS NULL",
        )
        .bind(org.to_string())
        .fetch_all(&self.pool)
        .await?;
        let mut accounts = Accounts::default();
        for r in rows {
            accounts.0.insert(
                r.get("username"),
                Account {
                    operator_id: OperatorId(uuid(r.get("user_id"))?),
                    password_hash: r.get("password_hash"),
                },
            );
        }
        Ok(accounts)
    }

    /// Create a person with a username and password in `org` (role technician).
    pub async fn add_account(
        &self,
        org: OrgId,
        username: &str,
        password: &str,
    ) -> Result<OperatorId> {
        anyhow::ensure!(!username.trim().is_empty(), "username must not be empty");
        anyhow::ensure!(
            password.chars().count() >= MIN_PASSWORD_LEN,
            "password must be at least {MIN_PASSWORD_LEN} characters"
        );
        let taken: Option<String> =
            sqlx::query_scalar("SELECT username FROM accounts WHERE username = ?")
                .bind(username)
                .fetch_optional(&self.pool)
                .await?;
        anyhow::ensure!(taken.is_none(), "account {username} already exists");
        let password = password.to_owned();
        let hash = tokio::task::spawn_blocking(move || crate::accounts::hash_password(&password))
            .await??;
        let id = OperatorId::new();
        let mut tx = self.pool.begin().await?;
        insert_user(&mut tx, id, None, Some(username)).await?;
        insert_membership(&mut tx, id, org, IMPORTED_ROLE).await?;
        sqlx::query(
            "INSERT INTO accounts (username, user_id, password_hash, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(username)
        .bind(id.to_string())
        .bind(hash)
        .bind(now())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Make the person with `email` the org's owner, creating them if needed. With
    /// `account`, that username's person gets the email (so their login is the owner's).
    /// Other owners become admins. Returns the owner's id and the demoted people.
    pub async fn set_owner(
        &self,
        org: OrgId,
        email: &str,
        account: Option<&str>,
    ) -> Result<(OperatorId, Vec<OperatorId>)> {
        let email = email.trim();
        anyhow::ensure!(
            email.contains('@') && !email.contains(char::is_whitespace),
            "{email:?} is not an email address"
        );
        let mut tx = self.pool.begin().await?;
        let by_email: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE email = ?")
            .bind(email)
            .fetch_optional(&mut *tx)
            .await?;
        let user = match account {
            Some(username) => {
                let id: String =
                    sqlx::query_scalar("SELECT user_id FROM accounts WHERE username = ?")
                        .bind(username)
                        .fetch_optional(&mut *tx)
                        .await?
                        .with_context(|| format!("no account {username}"))?;
                if let Some(other) = by_email.filter(|o| *o != id) {
                    bail!("{email} already belongs to another person ({other})");
                }
                sqlx::query("UPDATE users SET email = ? WHERE id = ?")
                    .bind(email)
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                OperatorId(uuid(&id)?)
            }
            None => match by_email {
                Some(id) => OperatorId(uuid(&id)?),
                None => {
                    let id = OperatorId::new();
                    insert_user(&mut tx, id, Some(email), None).await?;
                    id
                }
            },
        };
        let demoted: Vec<String> = sqlx::query_scalar(
            "SELECT user_id FROM memberships WHERE org_id = ? AND role = 'owner' AND user_id != ?",
        )
        .bind(org.to_string())
        .bind(user.to_string())
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE memberships SET role = 'admin' WHERE org_id = ? AND role = 'owner' AND user_id != ?",
        )
        .bind(org.to_string())
        .bind(user.to_string())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO memberships (user_id, org_id, role, created_at) VALUES (?, ?, 'owner', ?)
             ON CONFLICT (user_id, org_id) DO UPDATE SET role = 'owner'",
        )
        .bind(user.to_string())
        .bind(org.to_string())
        .bind(now())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        let demoted = demoted
            .iter()
            .map(|d| Ok(OperatorId(uuid(d)?)))
            .collect::<Result<_>>()?;
        Ok((user, demoted))
    }

    /// `user`'s role in `org`, if they are a member.
    pub async fn role(&self, org: OrgId, user: OperatorId) -> Result<Option<Role>> {
        let role: Option<String> =
            sqlx::query_scalar("SELECT role FROM memberships WHERE org_id = ? AND user_id = ?")
                .bind(org.to_string())
                .bind(user.to_string())
                .fetch_optional(&self.pool)
                .await?;
        role.map(|r| r.parse()).transpose()
    }

    /// A person's email, if they have one.
    pub async fn user_email(&self, user: OperatorId) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT email FROM users WHERE id = ?")
            .bind(user.to_string())
            .fetch_optional(&self.pool)
            .await?
            .flatten())
    }

    // -- tokens -------------------------------------------------------------------------

    /// Create an operator (a person with no login of their own) in `org` and its token.
    /// Returns the token once; only its SHA-256 is stored.
    pub async fn add_operator(&self, org: OrgId) -> Result<(OperatorId, String)> {
        let token = hex(&rand::random::<[u8; 32]>());
        let id = OperatorId::new();
        let mut tx = self.pool.begin().await?;
        insert_user(&mut tx, id, None, Some("operator token")).await?;
        insert_membership(&mut tx, id, org, IMPORTED_ROLE).await?;
        insert_static_token(&mut tx, org, id, &hex(&Sha256::digest(token.as_bytes()))).await?;
        tx.commit().await?;
        Ok((id, token))
    }

    /// Live operator tokens by SHA-256: not revoked or expired, their person enabled and
    /// still a member of the token's org.
    pub async fn operator_tokens(&self) -> Result<HashMap<[u8; 32], OperatorIdentity>> {
        let rows = sqlx::query(
            "SELECT t.token_sha256, t.user_id, t.org_id FROM api_tokens t
             JOIN users u ON u.id = t.user_id
             JOIN memberships m ON m.user_id = t.user_id AND m.org_id = t.org_id
             WHERE t.revoked_at IS NULL AND (t.expires_at IS NULL OR t.expires_at > ?)
               AND u.disabled_at IS NULL",
        )
        .bind(now())
        .fetch_all(&self.pool)
        .await?;
        let mut out = HashMap::new();
        for r in rows {
            let digest: String = r.get("token_sha256");
            let digest = unhex32(&digest).context("corrupt token digest in the database")?;
            out.insert(
                digest,
                OperatorIdentity {
                    operator_id: OperatorId(uuid(r.get("user_id"))?),
                    org_id: OrgId(uuid(r.get("org_id"))?),
                },
            );
        }
        Ok(out)
    }

    // -- computers ----------------------------------------------------------------------

    /// Refuse a computer from now on. A computer in no inventory is recorded as one
    /// without an account when `is_public` says so, else in the legacy org (enrolled
    /// before the database existed).
    pub async fn revoke_host(&self, host: HostId, is_public: bool) -> Result<Revoked> {
        let t = now();
        let mut tx = self.pool.begin().await?;
        let row: Option<(String, Option<i64>)> =
            sqlx::query_as("SELECT org_id, revoked_at FROM hosts WHERE id = ?")
                .bind(host.to_string())
                .fetch_optional(&mut *tx)
                .await?;
        let done = match row {
            Some((_, Some(_))) => Revoked::Already,
            Some((org, None)) => {
                sqlx::query("UPDATE hosts SET revoked_at = ? WHERE id = ?")
                    .bind(t)
                    .bind(host.to_string())
                    .execute(&mut *tx)
                    .await?;
                Revoked::OrgHost(OrgId(uuid(&org)?))
            }
            None if is_public => {
                let n = sqlx::query(
                    "INSERT INTO revoked_public_hosts (host_id, revoked_at) VALUES (?, ?)
                     ON CONFLICT (host_id) DO NOTHING",
                )
                .bind(host.to_string())
                .bind(t)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                if n == 0 {
                    Revoked::Already
                } else {
                    Revoked::PublicHost
                }
            }
            None => {
                let org: String = sqlx::query_scalar("SELECT id FROM orgs WHERE legacy = 1")
                    .fetch_optional(&mut *tx)
                    .await?
                    .context("no org on this server yet (run `init` first)")?;
                sqlx::query("INSERT INTO hosts (id, org_id, revoked_at) VALUES (?, ?, ?)")
                    .bind(host.to_string())
                    .bind(&org)
                    .bind(t)
                    .execute(&mut *tx)
                    .await?;
                Revoked::OrgHost(OrgId(uuid(&org)?))
            }
        };
        tx.commit().await?;
        Ok(done)
    }

    /// Every revoked computer (org inventories and computers without an account).
    pub async fn revoked_hosts(&self) -> Result<HashSet<HostId>> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM hosts WHERE revoked_at IS NOT NULL
             UNION SELECT host_id FROM revoked_public_hosts",
        )
        .fetch_all(&self.pool)
        .await?;
        ids.iter().map(|s| Ok(HostId(uuid(s)?))).collect()
    }

    /// Record a newly enrolled computer of `org`.
    pub async fn host_enrolled(
        &self,
        org: OrgId,
        host: HostId,
        by: Option<OperatorId>,
        at: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO hosts (id, org_id, enrolled_at, enrolled_by)
             SELECT ?1, ?2, ?3, ?4 WHERE EXISTS (SELECT 1 FROM orgs WHERE id = ?2)
             ON CONFLICT (id) DO UPDATE SET
               enrolled_at = COALESCE(hosts.enrolled_at, excluded.enrolled_at),
               enrolled_by = COALESCE(hosts.enrolled_by, excluded.enrolled_by)
             WHERE hosts.org_id = excluded.org_id",
        )
        .bind(host.to_string())
        .bind(org.to_string())
        .bind(at)
        .bind(by.map(|b| b.to_string()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A computer of `org` authenticated: record what it reported. Does nothing for an
    /// org that isn't in the database (computers without an account).
    pub async fn host_online(
        &self,
        org: OrgId,
        host: HostId,
        facts: &HostFacts,
        at: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO hosts (id, org_id, hostname, os, version, capabilities, last_seen)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7 WHERE EXISTS (SELECT 1 FROM orgs WHERE id = ?2)
             ON CONFLICT (id) DO UPDATE SET
               hostname = excluded.hostname, os = excluded.os, version = excluded.version,
               capabilities = excluded.capabilities, last_seen = excluded.last_seen
             WHERE hosts.org_id = excluded.org_id",
        )
        .bind(host.to_string())
        .bind(org.to_string())
        .bind(facts.label.as_deref())
        .bind(facts.os.as_deref())
        .bind(facts.version.as_deref())
        .bind(serde_json::to_string(&facts.capabilities)?)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// A computer of `org` went offline.
    pub async fn host_offline(&self, org: OrgId, host: HostId, at: i64) -> Result<()> {
        sqlx::query("UPDATE hosts SET last_seen = ? WHERE id = ? AND org_id = ?")
            .bind(at)
            .bind(host.to_string())
            .bind(org.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// One computer of `org`. Another org's computer answers `None`.
    pub async fn host(&self, org: OrgId, host: HostId) -> Result<Option<HostRow>> {
        let row = sqlx::query("SELECT * FROM hosts WHERE org_id = ? AND id = ?")
            .bind(org.to_string())
            .bind(host.to_string())
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(host_row).transpose()
    }

    /// `org`'s computers.
    pub async fn hosts(&self, org: OrgId) -> Result<Vec<HostRow>> {
        let rows = sqlx::query("SELECT * FROM hosts WHERE org_id = ? ORDER BY id")
            .bind(org.to_string())
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(host_row).collect()
    }

    /// Number of rows in the inventory of all orgs (for tests and logs).
    pub async fn host_count(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM hosts")
            .fetch_one(&self.pool)
            .await?)
    }
}

fn host_row(r: &sqlx::sqlite::SqliteRow) -> Result<HostRow> {
    let caps: String = r.get("capabilities");
    Ok(HostRow {
        id: HostId(uuid(r.get("id"))?),
        org_id: OrgId(uuid(r.get("org_id"))?),
        admin_name: r.get("admin_name"),
        hostname: r.get("hostname"),
        os: r.get("os"),
        version: r.get("version"),
        capabilities: serde_json::from_str(&caps).unwrap_or_default(),
        enrolled_at: r.get("enrolled_at"),
        enrolled_by: r.get("enrolled_by"),
        last_seen: r.get("last_seen"),
        group_id: r.get("group_id"),
        revoked_at: r.get("revoked_at"),
    })
}

pub(crate) type Tx<'a> = sqlx::Transaction<'a, sqlx::Sqlite>;

/// Insert a person unless one with this id exists. Returns whether it was inserted.
pub(crate) async fn insert_user(
    tx: &mut Tx<'_>,
    id: OperatorId,
    email: Option<&str>,
    display_name: Option<&str>,
) -> Result<bool> {
    let n = sqlx::query(
        "INSERT INTO users (id, email, display_name, created_at) VALUES (?, ?, ?, ?)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(id.to_string())
    .bind(email)
    .bind(display_name)
    .bind(now())
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// Add a membership unless the person is already in the org (their role is kept).
pub(crate) async fn insert_membership(
    tx: &mut Tx<'_>,
    user: OperatorId,
    org: OrgId,
    role: Role,
) -> Result<bool> {
    let n = sqlx::query(
        "INSERT INTO memberships (user_id, org_id, role, created_at) VALUES (?, ?, ?, ?)
         ON CONFLICT (user_id, org_id) DO NOTHING",
    )
    .bind(user.to_string())
    .bind(org.to_string())
    .bind(role.as_str())
    .bind(now())
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// Add a static token by digest unless it exists. Returns whether it was inserted.
pub(crate) async fn insert_static_token(
    tx: &mut Tx<'_>,
    org: OrgId,
    user: OperatorId,
    digest_hex: &str,
) -> Result<bool> {
    let n = sqlx::query(
        "INSERT INTO api_tokens (id, org_id, user_id, token_sha256, kind, created_at)
         VALUES (?, ?, ?, ?, 'static', ?) ON CONFLICT (token_sha256) DO NOTHING",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(org.to_string())
    .bind(user.to_string())
    .bind(digest_hex)
    .bind(now())
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// The database holds password hashes and sealed keys: owner-only, like the other secrets.
fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            tracing::warn!(path = %path.display(), error = %e, "couldn't restrict the database's permissions");
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}
