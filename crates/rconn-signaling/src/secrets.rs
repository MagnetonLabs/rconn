//! The server master key and the envelope that seals org secrets (fleet CA private keys)
//! in the database.
//!
//! The master key lives in its own file, outside the data dir (and so outside its
//! backups): by default `<data-dir>/../master.key`, or `RCONN_MASTER_KEY_FILE`. A copy of
//! the database without it reveals no CA key.
//!
//! Envelope: `0x01 | nonce (24 bytes) | XChaCha20-Poly1305 ciphertext and tag`. The
//! associated data is a fixed label plus the org id, so a sealed key copied into another
//! org's row does not open.

use anyhow::{Context, Result, bail};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rconn_core::ids::OrgId;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Environment variable naming the master key file.
pub const MASTER_KEY_ENV: &str = "RCONN_MASTER_KEY_FILE";

const ENVELOPE_V1: u8 = 1;
const NONCE_LEN: usize = 24;
const AAD_LABEL: &[u8] = b"rconn/org-secret/v1\0";

/// Where the master key lives when nothing says otherwise: next to the data dir.
pub fn default_master_key_path(data_dir: &Path) -> PathBuf {
    let abs = std::path::absolute(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    match abs.parent() {
        Some(parent) => parent.join("master.key"),
        None => abs.join("master.key"),
    }
}

/// 32 random bytes that seal every org secret on this server.
pub struct MasterKey([u8; 32]);

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(..)")
    }
}

impl MasterKey {
    pub fn from_bytes(key: [u8; 32]) -> Self {
        Self(key)
    }

    /// A fresh random key (not saved anywhere).
    pub fn generate() -> Self {
        Self(rand::random())
    }

    /// Read the key file; `None` if it doesn't exist.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        warn_if_shared(path);
        let key = crate::store::unhex32(text.trim())
            .with_context(|| format!("{} is not a master key (64 hex digits)", path.display()))?;
        Ok(Some(Self(key)))
    }

    /// Create a new key file, readable only by its owner. Refuses to overwrite one.
    pub fn create(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let key = Self::generate();
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(path)
            .with_context(|| format!("creating master key {}", path.display()))?;
        f.write_all(format!("{}\n", crate::store::hex(&key.0)).as_bytes())?;
        f.sync_all()?;
        Ok(key)
    }

    /// Seal `plaintext` for `org`.
    pub fn seal(&self, org: OrgId, plaintext: &[u8]) -> Result<Vec<u8>> {
        let nonce: [u8; NONCE_LEN] = rand::random();
        let aad = aad(org);
        let ct = self
            .cipher()
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("sealing failed"))?;
        let mut out = Vec::with_capacity(1 + NONCE_LEN + ct.len());
        out.push(ENVELOPE_V1);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Open an envelope sealed for `org`. Fails for another org, another key, or any
    /// change to the bytes.
    pub fn open(&self, org: OrgId, sealed: &[u8]) -> Result<Vec<u8>> {
        let Some((&ENVELOPE_V1, rest)) = sealed.split_first() else {
            bail!("unknown secret envelope version");
        };
        if rest.len() < NONCE_LEN {
            bail!("secret envelope too short");
        }
        let (nonce, ct) = rest.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().expect("split at NONCE_LEN");
        let aad = aad(org);
        self.cipher()
            .decrypt(&XNonce::from(nonce), Payload { msg: ct, aad: &aad })
            .map_err(|_| anyhow::anyhow!("secret for org {org} does not open with this master key"))
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(&Key::from(self.0))
    }
}

fn aad(org: OrgId) -> Vec<u8> {
    let mut aad = AAD_LABEL.to_vec();
    aad.extend_from_slice(org.0.as_bytes());
    aad
}

fn warn_if_shared(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = fs::metadata(path)
            && m.permissions().mode() & 0o077 != 0
        {
            tracing::warn!(path = %path.display(), "master key is readable by other users; chmod 600 it");
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}
