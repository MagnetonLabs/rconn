//! What the app remembers: the server to use, recent connections, and the login token
//! (shared with `rconn-viewer`, which reads the same `token.json`).

use rconn_core::ids::HostId;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub server: String,
    pub fingerprint: String,
    pub username: String,
    #[serde(default)]
    pub recent: Vec<Recent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recent {
    pub host_id: HostId,
    pub label: Option<String>,
}

/// Same shape and file as the CLI viewer's saved login.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedToken {
    pub server: String,
    pub token: String,
    pub expires_at: u64,
}

pub const MAX_RECENT: usize = 12;

pub fn config_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "rconn").map_or_else(
        || std::env::temp_dir().join("rconn"),
        |d| d.config_dir().to_owned(),
    )
}

impl Profile {
    pub fn load(dir: &Path) -> Self {
        std::fs::read_to_string(dir.join("app.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(
            dir.join("app.json"),
            serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?,
        )
    }

    /// Move `host` to the front of the recent list.
    pub fn remember(&mut self, host_id: HostId, label: Option<String>) {
        self.recent.retain(|r| r.host_id != host_id);
        self.recent.insert(0, Recent { host_id, label });
        self.recent.truncate(MAX_RECENT);
    }

    pub fn fingerprint(&self) -> Option<&str> {
        Some(self.fingerprint.trim()).filter(|f| !f.is_empty())
    }
}

/// A valid saved token for `server`, if any.
pub fn token_for(dir: &Path, server: &str, now_unix: u64) -> Option<String> {
    let t: SavedToken =
        serde_json::from_str(&std::fs::read_to_string(dir.join("token.json")).ok()?).ok()?;
    (t.server == server && now_unix < t.expires_at).then_some(t.token)
}

pub fn save_token(dir: &Path, t: &SavedToken) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("token.json");
    std::fs::write(&path, serde_json::to_vec(t).map_err(std::io::Error::other)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn forget_token(dir: &Path) {
    let _ = std::fs::remove_file(dir.join("token.json"));
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
