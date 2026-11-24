//! Layered host settings.
//!
//! Precedence, lowest to highest: built-in defaults → user file → system file →
//! **fleet policy**. Policy wins, and any key it sets is *locked*: local edits are refused
//! and the UI shows "locked by fleet policy". While a session is active every write is
//! refused, whatever its source.

use figment::providers::{Format, Serialized, Toml};
use figment::value::{Dict, Map};
use figment::{Figment, Metadata, Profile, Provider};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// What the host believes it is running on. Drives the hidden-mode default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostKind {
    Vm,
    Physical,
}

/// Whether the app shows its own "session active" indicator on the host.
/// OS privacy indicators are never affected by this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HiddenMode {
    /// Hidden on VMs, indicator on physical machines.
    #[default]
    Auto,
    Hidden,
    Indicator,
}

/// Which signaling server this host is enrolled with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerProfile {
    /// `wss://` URL of the signaling server (hosted or self-hosted).
    pub url: String,
    /// Pinned SHA-256 fingerprint of the server's public key, for self-hosted servers.
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostSettings {
    /// `None` until the host is enrolled.
    pub server: Option<ServerProfile>,
    pub hidden_mode: HiddenMode,
    /// Overrides auto-detection when set.
    pub host_kind: Option<HostKind>,
    pub clipboard: bool,
    pub max_bitrate_kbps: u32,
    pub log_level: String,
}

impl Default for HostSettings {
    fn default() -> Self {
        Self {
            server: None,
            hidden_mode: HiddenMode::Auto,
            host_kind: None,
            clipboard: true,
            max_bitrate_kbps: 8_000,
            log_level: "info".into(),
        }
    }
}

impl HostSettings {
    /// Resolve whether the indicator shows, given what detection found.
    pub fn shows_indicator(&self, detected: HostKind) -> bool {
        match self.hidden_mode {
            HiddenMode::Hidden => false,
            HiddenMode::Indicator => true,
            HiddenMode::Auto => self.host_kind.unwrap_or(detected) == HostKind::Physical,
        }
    }
}

/// Where a setting's effective value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Default,
    UserFile,
    SystemFile,
    FleetPolicy,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WriteDenied {
    #[error("settings are read-only while a session is active")]
    SessionActive,
    #[error("`{0}` is locked by fleet policy")]
    LockedByPolicy(String),
    #[error("unknown setting `{0}`")]
    UnknownKey(String),
}

#[derive(Debug, Error)]
#[error("invalid settings: {0}")]
pub struct LoadError(#[from] Box<figment::Error>);

/// Inputs to [`LoadedSettings::load`]. Missing files are fine; they contribute nothing.
#[derive(Debug, Clone, Default)]
pub struct SettingsLayers {
    pub user_file: Option<PathBuf>,
    pub system_file: Option<PathBuf>,
    /// Policy pushed by the signaling server, as a JSON object of setting keys.
    pub fleet_policy: Option<serde_json::Value>,
}

const POLICY_NAME: &str = "fleet policy";

/// Fleet policy as a figment provider with its own name, so provenance can tell it
/// apart from the (also serialized) defaults.
struct PolicyProvider(serde_json::Value);

impl Provider for PolicyProvider {
    fn metadata(&self) -> Metadata {
        Metadata::named(POLICY_NAME)
    }
    fn data(&self) -> Result<Map<Profile, Dict>, figment::Error> {
        Serialized::globals(&self.0).data()
    }
}

#[derive(Debug)]
pub struct LoadedSettings {
    pub settings: HostSettings,
    figment: Figment,
    layers: SettingsLayers,
}

impl LoadedSettings {
    pub fn load(layers: SettingsLayers) -> Result<Self, LoadError> {
        let mut fig = Figment::from(Serialized::defaults(HostSettings::default()));
        if let Some(p) = &layers.user_file {
            fig = fig.merge(Toml::file_exact(p));
        }
        if let Some(p) = &layers.system_file {
            fig = fig.merge(Toml::file_exact(p));
        }
        if let Some(policy) = &layers.fleet_policy {
            fig = fig.merge(PolicyProvider(policy.clone()));
        }
        let settings = fig.extract().map_err(Box::new)?;
        Ok(Self {
            settings,
            figment: fig,
            layers,
        })
    }

    /// Source of a dotted key such as `"server.url"` or `"clipboard"`.
    pub fn source(&self, key: &str) -> Source {
        let Some(meta) = self.figment.find_metadata(key) else {
            return Source::Default;
        };
        if meta.name == POLICY_NAME {
            return Source::FleetPolicy;
        }
        let from_file = |file: &Option<PathBuf>| matches!((&meta.source, file), (Some(figment::Source::File(src)), Some(f)) if same_path(src, f));
        if from_file(&self.layers.system_file) {
            Source::SystemFile
        } else if from_file(&self.layers.user_file) {
            Source::UserFile
        } else {
            Source::Default
        }
    }

    pub fn is_locked(&self, key: &str) -> bool {
        self.source(key) == Source::FleetPolicy
    }

    /// Gate for every local settings write. Enforced by the host service, not just the UI.
    pub fn check_write(&self, key: &str, session_active: bool) -> Result<(), WriteDenied> {
        if !is_known_key(key) {
            return Err(WriteDenied::UnknownKey(key.into()));
        }
        if session_active {
            return Err(WriteDenied::SessionActive);
        }
        if self.is_locked(key) {
            return Err(WriteDenied::LockedByPolicy(key.into()));
        }
        Ok(())
    }
}

/// Keys a local write may target (dotted for nested fields).
pub const KNOWN_KEYS: &[&str] = &[
    "server",
    "server.url",
    "server.fingerprint",
    "hidden_mode",
    "host_kind",
    "clipboard",
    "max_bitrate_kbps",
    "log_level",
];

fn is_known_key(key: &str) -> bool {
    KNOWN_KEYS.contains(&key)
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn write(dir: &tempfile::TempDir, name: &str, body: &str) -> PathBuf {
        let p = dir.path().join(name);
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn defaults_when_nothing_is_configured() {
        let s = LoadedSettings::load(SettingsLayers::default()).unwrap();
        assert_eq!(s.settings, HostSettings::default());
        assert_eq!(s.source("clipboard"), Source::Default);
    }

    #[test]
    fn precedence_is_default_user_system_policy() {
        let dir = tempfile::tempdir().unwrap();
        let user = write(
            &dir,
            "user.toml",
            "max_bitrate_kbps = 1000\nlog_level = \"debug\"\nclipboard = false\n",
        );
        let system = write(
            &dir,
            "system.toml",
            "max_bitrate_kbps = 2000\nclipboard = true\n",
        );
        let s = LoadedSettings::load(SettingsLayers {
            user_file: Some(user),
            system_file: Some(system),
            fleet_policy: Some(json!({ "clipboard": false, "hidden_mode": "indicator" })),
        })
        .unwrap();

        assert_eq!(s.settings.log_level, "debug");
        assert_eq!(s.source("log_level"), Source::UserFile);
        assert_eq!(s.settings.max_bitrate_kbps, 2000);
        assert_eq!(s.source("max_bitrate_kbps"), Source::SystemFile);
        assert!(!s.settings.clipboard);
        assert_eq!(s.source("clipboard"), Source::FleetPolicy);
        assert_eq!(s.settings.hidden_mode, HiddenMode::Indicator);
        assert!(s.is_locked("hidden_mode"));
        assert!(!s.is_locked("log_level"));
    }

    #[test]
    fn nested_policy_keys_are_locked() {
        let s = LoadedSettings::load(SettingsLayers {
            fleet_policy: Some(
                json!({ "server": { "url": "wss://fleet.example", "fingerprint": null } }),
            ),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            s.settings.server.as_ref().unwrap().url,
            "wss://fleet.example"
        );
        assert_eq!(
            s.check_write("server.url", false),
            Err(WriteDenied::LockedByPolicy("server.url".into()))
        );
    }

    #[test]
    fn writes_are_refused_during_sessions_and_for_locked_or_unknown_keys() {
        let s = LoadedSettings::load(SettingsLayers {
            fleet_policy: Some(json!({ "clipboard": false })),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(s.check_write("log_level", false), Ok(()));
        assert_eq!(
            s.check_write("log_level", true),
            Err(WriteDenied::SessionActive)
        );
        assert_eq!(
            s.check_write("clipboard", false),
            Err(WriteDenied::LockedByPolicy("clipboard".into()))
        );
        assert_eq!(
            s.check_write("nope", false),
            Err(WriteDenied::UnknownKey("nope".into()))
        );
    }

    #[test]
    fn invalid_file_is_an_error_not_a_silent_default() {
        let dir = tempfile::tempdir().unwrap();
        let bad = write(&dir, "user.toml", "max_bitrate_kbps = \"lots\"\n");
        assert!(
            LoadedSettings::load(SettingsLayers {
                user_file: Some(bad),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn indicator_resolution() {
        let mut s = HostSettings::default();
        assert!(!s.shows_indicator(HostKind::Vm));
        assert!(s.shows_indicator(HostKind::Physical));
        s.host_kind = Some(HostKind::Vm);
        assert!(
            !s.shows_indicator(HostKind::Physical),
            "override beats detection"
        );
        s.hidden_mode = HiddenMode::Indicator;
        assert!(s.shows_indicator(HostKind::Vm));
        s.hidden_mode = HiddenMode::Hidden;
        assert!(!s.shows_indicator(HostKind::Physical));
    }
}
