//! Per-computer viewer choices, kept between sessions: `viewer-prefs.json` in the config
//! folder, keyed by the computer's ID.

use rconn_core::ids::HostId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostPrefs {
    /// Change the remote screen's resolution to fit the window. Off: the picture is
    /// scaled and the remote screen keeps its own resolution.
    pub match_resolution: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct File {
    hosts: BTreeMap<String, HostPrefs>,
}

/// Where the viewer keeps them (`None` without a home folder).
pub fn default_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "rconn")
        .map(|d| d.config_dir().join("viewer-prefs.json"))
}

fn read(path: &Path) -> File {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// This computer's choices (the defaults if none were saved or the file is unreadable).
pub fn load(path: &Path, host: HostId) -> HostPrefs {
    read(path)
        .hosts
        .remove(&host.to_string())
        .unwrap_or_default()
}

/// Save this computer's choices, keeping every other computer's.
pub fn save(path: &Path, host: HostId, prefs: &HostPrefs) -> std::io::Result<()> {
    let mut file = read(path);
    file.hosts.insert(host.to_string(), prefs.clone());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?;
    // Written whole, then renamed over: a crash never leaves half a file.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_are_kept_per_computer_and_default_off() {
        let dir = std::env::temp_dir().join(format!("rconn-prefs-{}", uuid::Uuid::new_v4()));
        let path = dir.join("viewer-prefs.json");
        let (a, b) = (HostId::new(), HostId::new());
        assert!(!load(&path, a).match_resolution, "off until chosen");
        save(
            &path,
            a,
            &HostPrefs {
                match_resolution: true,
            },
        )
        .unwrap();
        save(&path, b, &HostPrefs::default()).unwrap();
        assert!(load(&path, a).match_resolution);
        assert!(!load(&path, b).match_resolution);
        std::fs::write(&path, "not json").unwrap();
        assert!(!load(&path, a).match_resolution, "unreadable: defaults");
        let _ = std::fs::remove_dir_all(dir);
    }
}
