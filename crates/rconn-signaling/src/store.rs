//! File-backed state for a self-hosted server (single org).
//!
//! ```text
//! <data-dir>/ca.pem          fleet CA certificate
//! <data-dir>/ca.key.pem      fleet CA private key (0600)
//! <data-dir>/operators.json  { "<sha256(token) hex>": "<operator id>" }
//! <data-dir>/revoked.json    [ "<host id>", ... ]
//! <data-dir>/public-ca.pem, public-ca.key.pem   CA for computers without an account
//! <data-dir>/public-hosts.jsonl                 their registry (id, owner)
//! <data-dir>/relay-usage.json                   built-in relay's bytes this month
//! ```

use anyhow::{Context, Result, bail};
use rconn_core::fleet::FleetCa;
use rconn_core::ids::{HostId, OperatorId};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub struct DataDir(PathBuf);

impl DataDir {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    fn p(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// Create the org CA and the server's TLS identity. Refuses to overwrite an existing CA.
    /// `hostnames` go into the TLS certificate's SANs (informational for pinned clients).
    pub fn init(&self, hostnames: &[String]) -> Result<FleetCa> {
        if self.p("ca.pem").exists() {
            bail!("{} already holds a fleet CA", self.0.display());
        }
        fs::create_dir_all(&self.0).with_context(|| format!("creating {}", self.0.display()))?;
        let ca = FleetCa::generate(rconn_core::ids::OrgId::new())?;
        fs::write(self.p("ca.pem"), ca.cert_pem())?;
        write_private(&self.p("ca.key.pem"), ca.key_pem())?;
        fs::write(self.p("operators.json"), "{}")?;
        fs::write(self.p("revoked.json"), "[]")?;
        let tls = rconn_core::tls::generate_server_tls(hostnames)?;
        fs::write(self.p("tls.cert.pem"), &tls.cert_pem)?;
        write_private(&self.p("tls.key.pem"), &tls.key_pem)?;
        Ok(ca)
    }

    /// Server TLS certificate and key (PEM), if `init` created them.
    pub fn tls_pem(&self) -> Result<Option<(String, String)>> {
        let cert = self.p("tls.cert.pem");
        if !cert.exists() {
            return Ok(None);
        }
        let key = fs::read_to_string(self.p("tls.key.pem")).context("reading tls.key.pem")?;
        Ok(Some((fs::read_to_string(cert)?, key)))
    }

    /// The pin clients put in their server profile: `sha256:<hex>` of the TLS public key.
    pub fn tls_fingerprint(&self) -> Result<Option<String>> {
        Ok(match self.tls_pem()? {
            Some((cert, _)) => Some(rconn_core::tls::spki_fingerprint_pem(&cert)?),
            None => None,
        })
    }

    pub fn load_ca(&self) -> Result<FleetCa> {
        let cert =
            fs::read_to_string(self.p("ca.pem")).context("reading ca.pem (run `init` first?)")?;
        let key = fs::read_to_string(self.p("ca.key.pem")).context("reading ca.key.pem")?;
        Ok(FleetCa::from_pem(&cert, &key)?)
    }

    /// The CA for computers that enroll without an account (a separate org no operator
    /// belongs to). Created on first use when `create`; otherwise `None` if absent.
    pub fn public_ca(&self, create: bool) -> Result<Option<FleetCa>> {
        let (cert, key) = (self.p("public-ca.pem"), self.p("public-ca.key.pem"));
        if cert.exists() {
            let ca = FleetCa::from_pem(
                &fs::read_to_string(&cert).context("reading public-ca.pem")?,
                &fs::read_to_string(&key).context("reading public-ca.key.pem")?,
            )?;
            return Ok(Some(ca));
        }
        if !create {
            return Ok(None);
        }
        let ca = FleetCa::generate(rconn_core::ids::OrgId::new())?;
        write_private(&key, ca.key_pem())?;
        fs::write(&cert, ca.cert_pem())?;
        Ok(Some(ca))
    }

    pub fn operators(&self) -> Result<BTreeMap<String, OperatorId>> {
        read_json(&self.p("operators.json"))
    }

    /// Create an operator and return its one-time-visible token.
    pub fn add_operator(&self) -> Result<(OperatorId, String)> {
        let mut ops = self.operators()?;
        let token = hex(&rand::random::<[u8; 32]>());
        let id = OperatorId::new();
        ops.insert(hex(&Sha256::digest(token.as_bytes())), id);
        write_private(
            &self.p("operators.json"),
            &serde_json::to_string_pretty(&ops)?,
        )?;
        Ok((id, token))
    }

    /// Shared secret between signaling and the TURN relay, created on first use.
    pub fn turn_secret(&self) -> Result<String> {
        let path = self.p("turn.secret");
        if let Ok(s) = fs::read_to_string(&path) {
            return Ok(s.trim().to_owned());
        }
        let secret = hex(&rand::random::<[u8; 32]>());
        write_private(&path, &secret)?;
        Ok(secret)
    }

    pub fn accounts(&self) -> Result<crate::accounts::Accounts> {
        let path = self.p("accounts.json");
        if !path.exists() {
            return Ok(Default::default());
        }
        read_json(&path)
    }

    pub fn add_account(&self, username: &str, password: &str) -> Result<OperatorId> {
        let mut accounts = self.accounts()?;
        let id = accounts.add(username, password)?;
        write_private(
            &self.p("accounts.json"),
            &serde_json::to_string_pretty(&accounts)?,
        )?;
        Ok(id)
    }

    /// Key that signs session tokens, created on first use. Delete it to log everyone out.
    pub fn session_key(&self) -> Result<[u8; 32]> {
        let path = self.p("session.key");
        if let Ok(s) = fs::read_to_string(&path) {
            return unhex32(s.trim()).context("corrupt session.key");
        }
        let key: [u8; 32] = rand::random();
        write_private(&path, &hex(&key))?;
        Ok(key)
    }

    pub fn revoked(&self) -> Result<Vec<HostId>> {
        read_json(&self.p("revoked.json"))
    }

    pub fn revoke(&self, host: HostId) -> Result<()> {
        let mut list = self.revoked()?;
        if !list.contains(&host) {
            list.push(host);
        }
        fs::write(self.p("revoked.json"), serde_json::to_string_pretty(&list)?)?;
        Ok(())
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Write a file readable only by its owner (on Unix; on Windows the data dir's ACL applies).
pub fn write_private(path: &Path, contents: &str) -> Result<()> {
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_then_reload_and_manage_operators_and_revocations() {
        let dir = tempfile::tempdir().unwrap();
        let store = DataDir::new(dir.path());
        let ca = store.init(&[]).unwrap();
        assert!(
            store.init(&[]).is_err(),
            "init must not overwrite an existing CA"
        );
        assert_eq!(store.load_ca().unwrap().org_id(), ca.org_id());
        assert!(
            store
                .tls_fingerprint()
                .unwrap()
                .unwrap()
                .starts_with("sha256:")
        );

        let (id, token) = store.add_operator().unwrap();
        let ops = store.operators().unwrap();
        let digest = hex(&Sha256::digest(token.as_bytes()));
        assert_eq!(ops.get(&digest), Some(&id));
        assert!(
            !ops.keys().any(|k| k == &token),
            "plaintext token must not be stored"
        );

        assert!(store.public_ca(false).unwrap().is_none());
        let public = store.public_ca(true).unwrap().unwrap();
        assert_ne!(
            public.org_id(),
            ca.org_id(),
            "its own org, never the operators'"
        );
        let again = store.public_ca(false).unwrap().unwrap();
        assert_eq!(again.org_id(), public.org_id());

        let h = HostId::new();
        store.revoke(h).unwrap();
        store.revoke(h).unwrap();
        assert_eq!(store.revoked().unwrap(), vec![h]);
    }

    #[test]
    fn hex_round_trip() {
        let b = [0xab_u8; 32];
        assert_eq!(unhex32(&hex(&b)), Some(b));
        assert_eq!(unhex32("zz"), None);
    }
}
