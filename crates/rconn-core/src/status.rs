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
    /// Connected to the server and reachable.
    pub online: bool,
    /// An operator is connected right now.
    pub session_active: bool,
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
