//! What the app remembers: the server to use, recent connections, and the login token
//! (shared with `rconn-viewer`, which reads the same `token.json`).

use rconn_core::ids::HostId;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    /// Empty: the public server ([`rconn_core::DEFAULT_SERVER`]).
    pub server: String,
    pub fingerprint: String,
    pub username: String,
    #[serde(default)]
    pub recent: Vec<Recent>,
    /// Incoming connections to this computer: `None` until decided (the first launch
    /// turns them on), `Some(false)` once turned off here.
    #[serde(default)]
    pub incoming: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recent {
    /// Known for computers in an account's list; connecting by ID and password only knows
    /// the typed `code`.
    #[serde(default)]
    pub host_id: Option<HostId>,
    /// The 9-digit ID ("123 456 789").
    #[serde(default)]
    pub code: String,
    pub label: Option<String>,
    /// The name was typed here (Rename): the computer's own name doesn't replace it.
    #[serde(default)]
    pub renamed: bool,
}

impl Recent {
    pub fn code(&self) -> String {
        match (&self.host_id, self.code.is_empty()) {
            (Some(id), true) => id.short_code(),
            _ => self.code.clone(),
        }
    }
}

/// "123456789" or "123-456 789" as "123 456 789"; anything else as typed (trimmed).
pub fn tidy_code(typed: &str) -> String {
    let digits: String = typed.chars().filter(|c| c.is_ascii_digit()).collect();
    let only_digits = typed
        .chars()
        .all(|c| c.is_ascii_digit() || c.is_whitespace() || c == '-');
    if digits.len() == 9 && only_digits {
        format!("{} {} {}", &digits[0..3], &digits[3..6], &digits[6..9])
    } else {
        typed.trim().to_owned()
    }
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
        let code = host_id.short_code();
        let own = self
            .recent
            .iter()
            .find(|r| r.renamed && (r.host_id == Some(host_id) || r.code() == code))
            .and_then(|r| r.label.clone());
        self.recent
            .retain(|r| r.host_id != Some(host_id) && r.code() != code);
        self.recent.insert(
            0,
            Recent {
                host_id: Some(host_id),
                code,
                renamed: own.is_some(),
                label: own.or(label),
            },
        );
        self.recent.truncate(MAX_RECENT);
    }

    /// Move a computer connected to by its typed ID to the front of the recent list.
    pub fn remember_code(&mut self, typed: &str) {
        let code = tidy_code(typed);
        let old = self.recent.iter().position(|r| r.code() == code);
        let (host_id, label, renamed) = old
            .map(|i| self.recent.remove(i))
            .map_or((None, None, false), |r| (r.host_id, r.label, r.renamed));
        self.recent.insert(
            0,
            Recent {
                host_id,
                code,
                label,
                renamed,
            },
        );
        self.recent.truncate(MAX_RECENT);
    }

    /// Name a recent computer here; empty goes back to the computer's own name.
    pub fn rename(&mut self, code: &str, label: &str) {
        let label = rconn_core::protocol::tidy_host_name(label);
        for r in self.recent.iter_mut().filter(|r| r.code() == code) {
            r.renamed = label.is_some();
            if label.is_some() {
                r.label = label.clone();
            }
        }
    }

    /// The computer told us its name: use it unless it was renamed here.
    pub fn learn_name(&mut self, code: &str, name: &str) {
        let Some(name) = rconn_core::protocol::tidy_host_name(name) else {
            return;
        };
        for r in self
            .recent
            .iter_mut()
            .filter(|r| r.code() == code && !r.renamed)
        {
            r.label = Some(name.clone());
        }
    }

    pub fn fingerprint(&self) -> Option<&str> {
        Some(self.fingerprint.trim()).filter(|f| !f.is_empty())
    }

    /// The server to use: the configured one, else the public one.
    pub fn server_url(&self) -> &str {
        match self.server.trim() {
            "" => rconn_core::DEFAULT_SERVER,
            s => s,
        }
    }

    /// Using the built-in public server (no pin, no account needed)?
    pub fn on_public_server(&self) -> bool {
        self.server_url() == rconn_core::DEFAULT_SERVER
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
