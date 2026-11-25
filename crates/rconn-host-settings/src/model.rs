//! What the settings window shows and edits, independent of the UI toolkit.

use rconn_core::settings::{
    HiddenMode, HostKind, LoadedSettings, SettingsLayers, Writer, write_setting,
};
use std::path::{Path, PathBuf};

/// The editable values, as the form holds them.
#[derive(Debug, Clone, PartialEq)]
pub struct Form {
    pub server_url: String,
    pub server_pin: String,
    pub hidden_mode: HiddenMode,
    pub host_kind: Option<HostKind>,
    pub clipboard: bool,
    pub max_bitrate_kbps: u32,
    pub log_level: String,
    /// The stored verifier ("" = off). Never the password itself.
    pub access_password: String,
    /// A new password being typed (not saved; turned into a verifier by "Set").
    pub new_access_password: String,
}

impl Form {
    /// Turn the typed password into the stored verifier (saved with the other changes).
    pub fn set_access_password(&mut self) -> Result<(), String> {
        let v = rconn_core::guest::make_verifier(&self.new_access_password)
            .map_err(|e| e.to_string())?;
        self.access_password = v;
        self.new_access_password.clear();
        Ok(())
    }

    fn from(s: &rconn_core::settings::HostSettings) -> Self {
        Self {
            server_url: s.server.as_ref().map(|p| p.url.clone()).unwrap_or_default(),
            server_pin: s
                .server
                .as_ref()
                .and_then(|p| p.fingerprint.clone())
                .unwrap_or_default(),
            hidden_mode: s.hidden_mode,
            host_kind: s.host_kind,
            clipboard: s.clipboard,
            max_bitrate_kbps: s.max_bitrate_kbps,
            log_level: s.log_level.clone(),
            access_password: s.access_password.clone(),
            new_access_password: String::new(),
        }
    }

    /// `(key, value)` pairs, in the string form `write_setting` takes ("" clears).
    fn pairs(&self) -> Vec<(&'static str, String)> {
        let mode = match self.hidden_mode {
            HiddenMode::Auto => "auto",
            HiddenMode::Hidden => "hidden",
            HiddenMode::Indicator => "indicator",
        };
        let kind = match self.host_kind {
            None => "",
            Some(HostKind::Vm) => "vm",
            Some(HostKind::Physical) => "physical",
        };
        vec![
            ("server.url", self.server_url.trim().to_owned()),
            ("server.fingerprint", self.server_pin.trim().to_owned()),
            ("hidden_mode", mode.into()),
            ("host_kind", kind.into()),
            ("clipboard", self.clipboard.to_string()),
            ("max_bitrate_kbps", self.max_bitrate_kbps.to_string()),
            ("log_level", self.log_level.clone()),
            ("access_password", self.access_password.clone()),
        ]
    }
}

pub enum Load {
    Ok(Box<LoadedSettings>),
    /// Typically "Access is denied": the folder is admin-only.
    Failed(String),
}

pub struct Model {
    pub data_dir: PathBuf,
    pub load: Load,
    /// What's saved on disk (to detect changes).
    pub saved: Option<Form>,
    pub form: Option<Form>,
    pub session_active: bool,
    pub host_id: Option<String>,
    pub service: String,
    /// `(text, is_error)` from the last action.
    pub messages: Vec<(String, bool)>,
}

impl Model {
    pub fn load(data_dir: &Path) -> Self {
        let layers = SettingsLayers {
            system_file: Some(data_dir.join("host.toml")),
            ..Default::default()
        };
        // Distinguish "no access" from "no file": LoadedSettings treats a missing file as defaults.
        let readable = std::fs::metadata(data_dir.join("host.toml"))
            .and_then(|_| std::fs::read(data_dir.join("host.toml")));
        let load = match (readable, LoadedSettings::load(layers)) {
            (Err(e), _) if e.kind() == std::io::ErrorKind::PermissionDenied => Load::Failed(
                "This folder is admin-only. Restart the settings window as administrator.".into(),
            ),
            (_, Ok(l)) => Load::Ok(Box::new(l)),
            (_, Err(e)) => Load::Failed(e.to_string()),
        };
        let saved = match &load {
            Load::Ok(l) => Some(Form::from(l.settings())),
            Load::Failed(_) => None,
        };
        Self {
            data_dir: data_dir.to_owned(),
            form: saved.clone(),
            saved,
            load,
            session_active: data_dir.join("session.active").exists(),
            host_id: std::fs::read_to_string(data_dir.join("identity").join("host.id"))
                .ok()
                .map(|s| s.trim().to_owned()),
            service: crate::platform::service_state(),
            messages: Vec::new(),
        }
    }

    /// Why `key` can't be edited right now, if it can't.
    pub fn lock_reason(&self, key: &str) -> Option<&'static str> {
        if self.session_active {
            return Some("Locked while a remote session is active");
        }
        match &self.load {
            Load::Ok(l) if l.is_locked(key) => Some("Set by fleet policy"),
            Load::Ok(_) => None,
            Load::Failed(_) => Some("Not readable"),
        }
    }

    /// Changed `(key, value)` pairs, form vs. disk.
    pub fn changes(&self) -> Vec<(&'static str, String)> {
        let (Some(saved), Some(form)) = (&self.saved, &self.form) else {
            return Vec::new();
        };
        let before = saved.pairs();
        form.pairs()
            .into_iter()
            .filter(|(k, v)| {
                before
                    .iter()
                    .find(|(bk, _)| bk == k)
                    .is_some_and(|(_, bv)| bv != v)
            })
            .collect()
    }

    /// Write every change (each one validated and lock-checked), then reload from disk.
    pub fn save(&mut self) {
        let changes = self.changes();
        let mut messages = Vec::new();
        let mut ok = 0;
        for (key, value) in &changes {
            let layers = SettingsLayers {
                system_file: Some(self.data_dir.join("host.toml")),
                ..Default::default()
            };
            match write_setting(layers, key, value, Writer::Admin, self.session_active) {
                Ok(()) => ok += 1,
                Err(e) => messages.push((format!("{key}: {e}"), true)),
            }
        }
        let form = self.form.clone();
        *self = Self::load(&self.data_dir);
        if !messages.is_empty() {
            // Keep the user's unsaved edits on failure so they can fix them.
            self.form = form;
        }
        if ok > 0 {
            messages.insert(
                0,
                (
                    format!("Saved {ok} change(s). Click \"Apply now\" to use them."),
                    false,
                ),
            );
        }
        self.messages = messages;
    }

    pub fn revert(&mut self) {
        self.form = self.saved.clone();
        self.messages.clear();
    }
}
