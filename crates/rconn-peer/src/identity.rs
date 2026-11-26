//! A host's identity folder: `host.id`, `host.cert.pem`, `host.key.pem` and, for
//! computers that enrolled without an account, `host.owner.json` (`null`: owned by
//! nobody). Shared by the desktop app (macOS/Linux) and the host agent (Windows first run).

use crate::signaling::SignalingClient;
use crate::trust::ServerTrust;
use anyhow::{Context, Result};
use rconn_core::fleet::HostKey;
use rconn_core::ids::HostId;
use rconn_core::owner::{HostOwner, OWNER_FILE};
use std::path::Path;

/// Is a complete identity saved in `dir`?
pub fn has_identity(dir: &Path) -> bool {
    ["host.id", "host.cert.pem", "host.key.pem"]
        .iter()
        .all(|f| dir.join(f).exists())
}

/// What this host knows about its owner, from its own folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalOwner {
    /// No owner file: enrolled into an org (token or `enroll-host`); the server decides.
    Org,
    /// Recorded at enrollment (`None`: nobody, every session needs the access password).
    Recorded(Option<HostOwner>),
}

/// Read the owner file. An unreadable or corrupt file counts as "nobody": the safe answer.
pub fn local_owner(dir: &Path) -> LocalOwner {
    let path = dir.join(OWNER_FILE);
    if !path.exists() {
        return LocalOwner::Org;
    }
    LocalOwner::Recorded(
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .flatten(),
    )
}

#[cfg(unix)]
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents.as_bytes())?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Windows: the folder's ACL (SYSTEM and Administrators only) protects the files.
#[cfg(not(unix))]
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    std::fs::write(path, contents)
}

/// Save an identity. `owner`: `Some(None)` writes "owned by nobody"; `None` writes no
/// owner file (an org host).
pub fn save_identity(
    dir: &Path,
    host_id: HostId,
    cert_pem: &str,
    key: &HostKey,
    owner: Option<Option<HostOwner>>,
) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    // Owner first: a crash mid-save never leaves an account-less identity without it.
    match owner {
        Some(o) => write_private(&dir.join(OWNER_FILE), &serde_json::to_string(&o)?)?,
        None => {
            let _ = std::fs::remove_file(dir.join(OWNER_FILE));
        }
    }
    write_private(&dir.join("host.key.pem"), &key.to_pem())?;
    write_private(&dir.join("host.cert.pem"), cert_pem)?;
    // host.id last: `has_identity` only sees a finished identity.
    write_private(&dir.join("host.id"), &host_id.to_string())?;
    Ok(())
}

/// Get an ID from `server` without an account and save it in `dir`.
pub async fn enroll_anonymous_into(
    dir: &Path,
    server: &str,
    trust: &ServerTrust,
) -> Result<HostId> {
    let key = HostKey::generate()?;
    let (host_id, cert) = SignalingClient::enroll_anonymous(server, trust, &key).await?;
    save_identity(dir, host_id, &cert, &key, Some(None))?;
    Ok(host_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_file_round_trips_and_fails_safe() {
        let d = tempfile::tempdir().unwrap();
        let key = HostKey::generate().unwrap();
        let id = HostId::new();
        save_identity(d.path(), id, "cert", &key, None).unwrap();
        assert!(has_identity(d.path()));
        assert_eq!(local_owner(d.path()), LocalOwner::Org);
        save_identity(d.path(), id, "cert", &key, Some(None)).unwrap();
        assert_eq!(local_owner(d.path()), LocalOwner::Recorded(None));
        std::fs::write(d.path().join(OWNER_FILE), "garbage").unwrap();
        assert_eq!(
            local_owner(d.path()),
            LocalOwner::Recorded(None),
            "a corrupt owner file means nobody, never the org"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(d.path().join("host.key.pem"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
