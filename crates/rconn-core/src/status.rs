//! Host status, published by the agent for the local app ("This computer" panel).
//! Readable by ordinary users (no secrets): admin-only settings stay in host.toml.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStatus {
    pub host_id: String,
    pub short_code: String,
    pub label: String,
    pub server: String,
    /// The pinned server key (`sha256:...`), if any. Public: lets the local app connect too.
    #[serde(default)]
    pub fingerprint: Option<String>,
    /// Connected to the server and reachable.
    pub online: bool,
    /// An operator is connected right now.
    pub session_active: bool,
    /// OS permissions the host still needs (macOS: "Screen Recording", "Accessibility").
    #[serde(default)]
    pub missing_permissions: Vec<String>,
    /// An access password is set: anyone with the ID and the password can connect.
    #[serde(default)]
    pub access_password: bool,
    /// Unix seconds of the last update (stale = agent not running).
    pub updated: u64,
}

impl HostStatus {
    pub fn read(path: &Path) -> Option<Self> {
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    }

    /// Write atomically (temp file + rename) so readers never see half a file.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?,
        )?;
        std::fs::rename(tmp, path)
    }

    /// Older than this, the agent is probably not running.
    pub fn is_fresh(&self, now_unix: u64) -> bool {
        now_unix.saturating_sub(self.updated) < 90
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_ages() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("status.json");
        let s = HostStatus {
            short_code: "067 425 387".into(),
            online: true,
            updated: 100,
            ..Default::default()
        };
        s.write(&p).unwrap();
        assert_eq!(HostStatus::read(&p), Some(s.clone()));
        assert!(s.is_fresh(150));
        assert!(!s.is_fresh(1000));
    }
}
