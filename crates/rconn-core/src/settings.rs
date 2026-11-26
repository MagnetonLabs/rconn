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

/// Classify a machine from its SMBIOS strings and the hypervisor flag. Errs toward
/// `Physical` when unsure, because that default shows the session indicator.
pub fn classify_host(manufacturer: &str, product: &str, hypervisor_flag: bool) -> HostKind {
    const VM_MARKERS: &[&str] = &[
        "parallels",
        "vmware",
        "virtualbox",
        "innotek",
        "qemu",
        "kvm",
        "xen",
        "bochs",
        "amazon ec2",
        "google compute engine",
        "virtual machine",
        "hvm domu",
        "utm",
        "apple virtualization",
    ];
    let text = format!("{manufacturer} {product}").to_ascii_lowercase();
    // "Microsoft Corporation" + "Virtual Machine" is Hyper-V/Azure; a Surface is physical.
    if hypervisor_flag || VM_MARKERS.iter().any(|m| text.contains(m)) {
        HostKind::Vm
    } else {
        HostKind::Physical
    }
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
    /// Verifier for the access password (see [`crate::guest::make_verifier`]). Empty: no
    /// password, so whoever connects with the ID needs the person at the computer to accept.
    pub access_password: String,
    /// With an access password set, also let someone without it ask the person at the
    /// computer to accept. Off by default: then the password is the only way in.
    pub ask_with_password: bool,
    /// Let viewers browse this computer's files and copy them both ways (the signed-in
    /// user's home folder; uploads land in Downloads/Rconn). Fleet policy can lock it.
    pub file_transfer: bool,
    /// Let viewers change this screen's resolution to fit their window (put back when
    /// the session ends). Fleet policy can lock it.
    pub allow_resize: bool,
}

impl Default for HostSettings {
    fn default() -> Self {
        Self {
            server: None,
            hidden_mode: HiddenMode::Auto,
            host_kind: None,
            clipboard: true,
            max_bitrate_kbps: 25_000,
            log_level: "info".into(),
            access_password: String::new(),
            ask_with_password: false,
            file_transfer: true,
            allow_resize: true,
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
    #[error("`{0}` can only be changed by an administrator")]
    AdminOnly(String),
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
const USER_FILE_NAME: &str = "user file";

/// Keys only an administrator may set: the system file, fleet policy, or an admin write.
/// The per-user file can't change them, so a user can't hide the session indicator or
/// repoint the host at another server.
pub const ADMIN_ONLY_KEYS: &[&str] = &[
    "server",
    "hidden_mode",
    "host_kind",
    "access_password",
    "ask_with_password",
];

/// Who is asking to change a setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writer {
    User,
    Admin,
}

/// A JSON object as a figment provider with a fixed name, so provenance can tell the
/// layers apart (defaults are also serialized).
struct NamedProvider {
    name: &'static str,
    value: serde_json::Value,
}

impl Provider for NamedProvider {
    fn metadata(&self) -> Metadata {
        Metadata::named(self.name)
    }
    fn data(&self) -> Result<Map<Profile, Dict>, figment::Error> {
        // Default profile, like the files: figment's Global profile would outrank later layers.
        Serialized::defaults(&self.value).data()
    }
}

#[derive(Debug)]
pub struct LoadedSettings {
    settings: HostSettings,
    figment: Figment,
    layers: SettingsLayers,
    /// Every dotted path the fleet policy sets, including ones set to null.
    policy_paths: Vec<String>,
    /// Admin-only keys found (and dropped) in the user file. Worth a warning in logs.
    pub ignored_user_keys: Vec<String>,
}

impl LoadedSettings {
    pub fn load(layers: SettingsLayers) -> Result<Self, LoadError> {
        let mut fig = Figment::from(Serialized::defaults(HostSettings::default()));
        let mut ignored_user_keys = Vec::new();
        // A settings file that doesn't exist yet (a fresh install) is an empty one.
        if let Some(p) = layers.user_file.as_ref().filter(|p| p.exists()) {
            let mut value: serde_json::Value = Figment::from(Toml::file_exact(p))
                .extract()
                .map_err(Box::new)?;
            if let Some(obj) = value.as_object_mut() {
                for key in ADMIN_ONLY_KEYS {
                    if obj.remove(*key).is_some() {
                        ignored_user_keys.push((*key).to_string());
                    }
                }
            }
            fig = fig.merge(NamedProvider {
                name: USER_FILE_NAME,
                value,
            });
        }
        if let Some(p) = layers.system_file.as_ref().filter(|p| p.exists()) {
            fig = fig.merge(Toml::file_exact(p));
        }
        let mut policy_paths = Vec::new();
        if let Some(policy) = &layers.fleet_policy {
            collect_paths(policy, "", &mut policy_paths);
            fig = fig.merge(NamedProvider {
                name: POLICY_NAME,
                value: policy.clone(),
            });
        }
        let settings = fig.extract().map_err(Box::new)?;
        Ok(Self {
            settings,
            figment: fig,
            layers,
            policy_paths,
            ignored_user_keys,
        })
    }

    /// The effective settings. Read-only: changes go through the host service, which
    /// calls [`check_write`](Self::check_write) under its session lock and reloads.
    pub fn settings(&self) -> &HostSettings {
        &self.settings
    }

    /// Source of a dotted key such as `"server.url"` or `"clipboard"`.
    pub fn source(&self, key: &str) -> Source {
        if self.is_locked(key) {
            return Source::FleetPolicy;
        }
        let Some(meta) = self.figment.find_metadata(key) else {
            return Source::Default;
        };
        if meta.name == USER_FILE_NAME {
            return Source::UserFile;
        }
        let from_system = matches!(
            (&meta.source, &self.layers.system_file),
            (Some(figment::Source::File(src)), Some(f)) if same_path(src, f)
        );
        if from_system {
            Source::SystemFile
        } else {
            Source::Default
        }
    }

    /// A key is locked if policy sets it, an ancestor of it (`server` locks `server.url`,
    /// even when set to null), or a descendant of it (`server.url` locks `server`).
    pub fn is_locked(&self, key: &str) -> bool {
        self.policy_paths.iter().any(|p| overlaps(p, key))
    }

    /// Gate for every local settings write. The caller (host service) must hold its
    /// session lock across this check and the write, so a session can't start in between.
    pub fn check_write(
        &self,
        key: &str,
        writer: Writer,
        session_active: bool,
    ) -> Result<(), WriteDenied> {
        if !KNOWN_KEYS.contains(&key) {
            return Err(WriteDenied::UnknownKey(key.into()));
        }
        if session_active {
            return Err(WriteDenied::SessionActive);
        }
        if self.is_locked(key) {
            return Err(WriteDenied::LockedByPolicy(key.into()));
        }
        if writer == Writer::User && ADMIN_ONLY_KEYS.iter().any(|a| overlaps(a, key)) {
            return Err(WriteDenied::AdminOnly(key.into()));
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum SetError {
    #[error(transparent)]
    Denied(#[from] WriteDenied),
    #[error("invalid value: {0}")]
    Invalid(String),
    #[error("settings file: {0}")]
    Io(#[from] std::io::Error),
}

/// Change one setting in `layers.system_file`, enforcing every lock: unknown keys,
/// sessions in progress, fleet-policy locks and admin-only keys. The new file is
/// validated before it replaces the old one, so a bad value never lands on disk.
/// `value` is parsed as a bool, then an integer, else kept as a string.
pub fn write_setting(
    layers: SettingsLayers,
    key: &str,
    value: &str,
    writer: Writer,
    session_active: bool,
) -> Result<(), SetError> {
    let file = layers
        .system_file
        .clone()
        .ok_or_else(|| SetError::Invalid("no system settings file".into()))?;
    let loaded =
        LoadedSettings::load(layers.clone()).map_err(|e| SetError::Invalid(e.to_string()))?;
    loaded.check_write(key, writer, session_active)?;

    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let mut table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| SetError::Invalid(e.to_string()))?;
    // An empty value clears the setting (back to its default / auto).
    let parsed = if value.is_empty() {
        None
    } else if let Ok(b) = value.parse::<bool>() {
        Some(toml::Value::Boolean(b))
    } else if let Ok(i) = value.parse::<i64>() {
        Some(toml::Value::Integer(i))
    } else {
        Some(toml::Value::String(value.to_owned()))
    };
    let mut parts: Vec<&str> = key.split('.').collect();
    let leaf = parts.pop().expect("split yields at least one part");
    let mut node = &mut table;
    for p in parts {
        node = node
            .entry(p)
            .or_insert_with(|| toml::Value::Table(Default::default()))
            .as_table_mut()
            .ok_or_else(|| SetError::Invalid(format!("{p} is not a table")))?;
    }
    match parsed {
        Some(v) => {
            node.insert(leaf.to_owned(), v);
        }
        None => {
            node.remove(leaf);
        }
    }
    let new_text = toml::to_string(&table).map_err(|e| SetError::Invalid(e.to_string()))?;

    // Validate the candidate exactly as it would be loaded.
    let candidate = file.with_extension("toml.new");
    std::fs::write(&candidate, &new_text)?;
    let check = LoadedSettings::load(SettingsLayers {
        system_file: Some(candidate.clone()),
        ..layers
    });
    if let Err(e) = check {
        let _ = std::fs::remove_file(&candidate);
        return Err(SetError::Invalid(e.to_string()));
    }
    std::fs::rename(&candidate, &file)?;
    Ok(())
}

/// True if one dotted path is equal to, an ancestor of, or a descendant of the other.
fn overlaps(a: &str, b: &str) -> bool {
    let nested = |outer: &str, inner: &str| {
        inner.len() > outer.len()
            && inner.starts_with(outer)
            && inner.as_bytes()[outer.len()] == b'.'
    };
    a == b || nested(a, b) || nested(b, a)
}

fn collect_paths(v: &serde_json::Value, prefix: &str, out: &mut Vec<String>) {
    let Some(obj) = v.as_object() else { return };
    for (k, child) in obj {
        let path = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        collect_paths(child, &path, out);
        out.push(path);
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
    "access_password",
    "ask_with_password",
    "file_transfer",
    "allow_resize",
];

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

        assert_eq!(s.settings().log_level, "debug");
        assert_eq!(s.source("log_level"), Source::UserFile);
        assert_eq!(s.settings().max_bitrate_kbps, 2000);
        assert_eq!(s.source("max_bitrate_kbps"), Source::SystemFile);
        assert!(!s.settings().clipboard);
        assert_eq!(s.source("clipboard"), Source::FleetPolicy);
        assert_eq!(s.settings().hidden_mode, HiddenMode::Indicator);
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
            s.settings().server.as_ref().unwrap().url,
            "wss://fleet.example"
        );
        assert_eq!(
            s.check_write("server.url", Writer::Admin, false),
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
        assert_eq!(s.check_write("log_level", Writer::Admin, false), Ok(()));
        assert_eq!(
            s.check_write("log_level", Writer::Admin, true),
            Err(WriteDenied::SessionActive)
        );
        assert_eq!(
            s.check_write("clipboard", Writer::Admin, false),
            Err(WriteDenied::LockedByPolicy("clipboard".into()))
        );
        assert_eq!(
            s.check_write("nope", Writer::Admin, false),
            Err(WriteDenied::UnknownKey("nope".into()))
        );
    }

    /// File transfer follows clipboard's rules: on by default, the local user may change
    /// it, fleet policy locks it.
    #[test]
    fn file_transfer_defaults_on_and_policy_locks_it() {
        let s = LoadedSettings::load(SettingsLayers::default()).unwrap();
        assert!(s.settings().file_transfer);
        assert_eq!(s.check_write("file_transfer", Writer::User, false), Ok(()));
        let dir = tempfile::tempdir().unwrap();
        let user = write(&dir, "user.toml", "file_transfer = false\n");
        let s = LoadedSettings::load(SettingsLayers {
            user_file: Some(user.clone()),
            ..Default::default()
        })
        .unwrap();
        assert!(!s.settings().file_transfer);
        assert_eq!(s.source("file_transfer"), Source::UserFile);
        let s = LoadedSettings::load(SettingsLayers {
            user_file: Some(user),
            fleet_policy: Some(json!({ "file_transfer": true })),
            ..Default::default()
        })
        .unwrap();
        assert!(s.settings().file_transfer, "policy wins");
        assert_eq!(
            s.check_write("file_transfer", Writer::Admin, false),
            Err(WriteDenied::LockedByPolicy("file_transfer".into()))
        );
    }

    /// Resizing the screen follows the same rules: on by default, the local user may turn
    /// it off, fleet policy locks it.
    #[test]
    fn allow_resize_defaults_on_and_policy_locks_it() {
        let s = LoadedSettings::load(SettingsLayers::default()).unwrap();
        assert!(s.settings().allow_resize);
        assert_eq!(s.check_write("allow_resize", Writer::User, false), Ok(()));
        let dir = tempfile::tempdir().unwrap();
        let user = write(&dir, "user.toml", "allow_resize = false\n");
        let s = LoadedSettings::load(SettingsLayers {
            user_file: Some(user.clone()),
            ..Default::default()
        })
        .unwrap();
        assert!(!s.settings().allow_resize);
        let s = LoadedSettings::load(SettingsLayers {
            user_file: Some(user),
            fleet_policy: Some(json!({ "allow_resize": true })),
            ..Default::default()
        })
        .unwrap();
        assert!(s.settings().allow_resize, "policy wins");
        assert_eq!(
            s.check_write("allow_resize", Writer::Admin, false),
            Err(WriteDenied::LockedByPolicy("allow_resize".into()))
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

    #[test]
    fn null_parent_in_policy_locks_children() {
        let s = LoadedSettings::load(SettingsLayers {
            fleet_policy: Some(json!({ "server": null })),
            ..Default::default()
        })
        .unwrap();
        assert!(s.is_locked("server"));
        assert!(s.is_locked("server.url"));
        assert!(
            !s.is_locked("serverless"),
            "prefix match must respect the dot"
        );
        assert_eq!(
            s.check_write("server.url", Writer::Admin, false),
            Err(WriteDenied::LockedByPolicy("server.url".into()))
        );
    }

    #[test]
    fn child_in_policy_locks_parent() {
        let s = LoadedSettings::load(SettingsLayers {
            fleet_policy: Some(json!({ "server": { "url": "wss://a" } })),
            ..Default::default()
        })
        .unwrap();
        assert!(s.is_locked("server"));
    }

    #[test]
    fn user_file_cannot_set_admin_only_keys() {
        let dir = tempfile::tempdir().unwrap();
        let user = write(
            &dir,
            "user.toml",
            "hidden_mode = \"hidden\"\nhost_kind = \"vm\"\nlog_level = \"debug\"\n[server]\nurl = \"wss://evil\"\n",
        );
        let s = LoadedSettings::load(SettingsLayers {
            user_file: Some(user),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(s.settings().hidden_mode, HiddenMode::Auto);
        assert_eq!(s.settings().host_kind, None);
        assert_eq!(s.settings().server, None);
        assert_eq!(s.settings().log_level, "debug", "ordinary keys still apply");
        let mut ignored = s.ignored_user_keys.clone();
        ignored.sort();
        assert_eq!(ignored, ["hidden_mode", "host_kind", "server"]);
        assert!(s.shows_indicator_for_physical());
    }

    #[test]
    fn system_file_can_set_admin_only_keys() {
        let dir = tempfile::tempdir().unwrap();
        let system = write(&dir, "system.toml", "hidden_mode = \"hidden\"\n");
        let s = LoadedSettings::load(SettingsLayers {
            system_file: Some(system),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(s.settings().hidden_mode, HiddenMode::Hidden);
        assert_eq!(s.source("hidden_mode"), Source::SystemFile);
    }

    #[test]
    fn users_cannot_write_admin_only_keys() {
        let s = LoadedSettings::load(SettingsLayers::default()).unwrap();
        assert_eq!(
            s.check_write("hidden_mode", Writer::User, false),
            Err(WriteDenied::AdminOnly("hidden_mode".into()))
        );
        assert_eq!(
            s.check_write("server.url", Writer::User, false),
            Err(WriteDenied::AdminOnly("server.url".into()))
        );
        assert_eq!(s.check_write("hidden_mode", Writer::Admin, false), Ok(()));
        assert_eq!(s.check_write("log_level", Writer::User, false), Ok(()));
    }

    impl LoadedSettings {
        fn shows_indicator_for_physical(&self) -> bool {
            self.settings().shows_indicator(HostKind::Physical)
        }
    }

    #[test]
    fn host_classification() {
        assert_eq!(
            classify_host(
                "Parallels International GmbH.",
                "Parallels ARM Virtual Machine",
                false
            ),
            HostKind::Vm
        );
        assert_eq!(
            classify_host("Microsoft Corporation", "Virtual Machine", false),
            HostKind::Vm
        );
        assert_eq!(classify_host("Amazon EC2", "t3.large", false), HostKind::Vm);
        assert_eq!(
            classify_host("Microsoft Corporation", "Surface Laptop 5", false),
            HostKind::Physical
        );
        assert_eq!(
            classify_host("Dell Inc.", "OptiPlex 7090", false),
            HostKind::Physical
        );
        assert_eq!(
            classify_host("Apple Inc.", "Mac17,6", false),
            HostKind::Physical
        );
        assert_eq!(
            classify_host("Apple Inc.", "VirtualMac2,1", true),
            HostKind::Vm
        );
    }

    #[test]
    /// A fresh install has no host.toml yet: the first write creates it.
    fn first_write_creates_a_missing_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("host.toml");
        let layers = || SettingsLayers {
            system_file: Some(file.clone()),
            user_file: Some(dir.path().join("user.toml")),
            ..Default::default()
        };
        assert!(LoadedSettings::load(layers()).is_ok());
        write_setting(
            layers(),
            "server.url",
            "wss://a/v1/ws",
            Writer::Admin,
            false,
        )
        .unwrap();
        write_setting(layers(), "server.fingerprint", "", Writer::Admin, false).unwrap();
        let loaded = LoadedSettings::load(layers()).unwrap();
        assert_eq!(loaded.settings.server.unwrap().url, "wss://a/v1/ws");
    }

    #[test]
    fn write_setting_enforces_locks_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("host.toml");
        std::fs::write(&file, "[server]\nurl = \"wss://a\"\n").unwrap();
        let layers = |policy: Option<serde_json::Value>| SettingsLayers {
            system_file: Some(file.clone()),
            fleet_policy: policy,
            ..Default::default()
        };

        write_setting(
            layers(None),
            "max_bitrate_kbps",
            "4000",
            Writer::Admin,
            false,
        )
        .unwrap();
        write_setting(
            layers(None),
            "hidden_mode",
            "indicator",
            Writer::Admin,
            false,
        )
        .unwrap();
        let s = LoadedSettings::load(layers(None)).unwrap();
        assert_eq!(s.settings().max_bitrate_kbps, 4000);
        assert_eq!(s.settings().hidden_mode, HiddenMode::Indicator);
        assert_eq!(
            s.settings().server.as_ref().unwrap().url,
            "wss://a",
            "other keys survive"
        );

        assert!(matches!(
            write_setting(layers(None), "log_level", "debug", Writer::Admin, true),
            Err(SetError::Denied(WriteDenied::SessionActive))
        ));
        assert!(matches!(
            write_setting(
                layers(Some(json!({"clipboard": false}))),
                "clipboard",
                "true",
                Writer::Admin,
                false
            ),
            Err(SetError::Denied(WriteDenied::LockedByPolicy(_)))
        ));
        assert!(matches!(
            write_setting(layers(None), "hidden_mode", "hidden", Writer::User, false),
            Err(SetError::Denied(WriteDenied::AdminOnly(_)))
        ));

        let before = std::fs::read_to_string(&file).unwrap();
        assert!(matches!(
            write_setting(
                layers(None),
                "max_bitrate_kbps",
                "lots",
                Writer::Admin,
                false
            ),
            Err(SetError::Invalid(_))
        ));
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            before,
            "a bad value must not touch the file"
        );
    }
}
