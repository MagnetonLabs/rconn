//! Built-in operator accounts (argon2id password hashes) and short-lived session tokens.
//!
//! Token format: `v1.<base64url(payload)>.<base64url(HMAC-SHA256(key, payload))>`, where
//! payload = operator id (16 bytes) | org id (16) | expiry Unix seconds (8, big-endian).
//! Tokens are stateless; rotating `session.key` revokes every outstanding token.

use crate::auth::{AuthError, OperatorIdentity};
use anyhow::{Context, Result};
use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{Hmac, KeyInit, Mac};
use rconn_core::ids::{OperatorId, OrgId};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// How long a login lasts before the viewer must log in again.
pub const SESSION_TOKEN_TTL: Duration = Duration::from_secs(12 * 3600);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub operator_id: OperatorId,
    /// PHC-format argon2id hash.
    pub password_hash: String,
}

/// Username -> account, as loaded from the database (`accounts.json` before 0.2).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Accounts(pub BTreeMap<String, Account>);

pub const MIN_PASSWORD_LEN: usize = 12;

impl Accounts {
    pub fn add(&mut self, username: &str, password: &str) -> Result<OperatorId> {
        anyhow::ensure!(!username.trim().is_empty(), "username must not be empty");
        anyhow::ensure!(
            !self.0.contains_key(username),
            "account {username} already exists"
        );
        anyhow::ensure!(
            password.chars().count() >= MIN_PASSWORD_LEN,
            "password must be at least {MIN_PASSWORD_LEN} characters"
        );
        let operator_id = OperatorId::new();
        self.0.insert(
            username.to_owned(),
            Account {
                operator_id,
                password_hash: hash_password(password)?,
            },
        );
        Ok(operator_id)
    }

    /// Check a password. Unknown users cost the same argon2 work as known ones, so response
    /// time doesn't reveal which usernames exist.
    pub fn verify(&self, username: &str, password: &str) -> Option<OperatorId> {
        let (hash, id) = match self.0.get(username) {
            Some(a) => (a.password_hash.as_str(), Some(a.operator_id)),
            None => (dummy_hash(), None),
        };
        let parsed = PasswordHash::new(hash).ok()?;
        let ok = Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok();
        if ok { id } else { None }
    }
}

/// An argon2id PHC hash of `password` with a random salt.
pub fn hash_password(password: &str) -> Result<String> {
    // argon2id with the crate's recommended parameters and a random salt.
    Ok(Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(|e| anyhow::anyhow!("hashing: {e}"))?
        .to_string())
}

fn dummy_hash() -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| hash_password("dummy password for timing").expect("argon2 works"))
}

/// Issues and checks session tokens with a server-held key.
pub struct SessionTokens {
    key: [u8; 32],
}

impl SessionTokens {
    pub fn new(key: [u8; 32]) -> Self {
        Self { key }
    }

    fn mac(&self, payload: &[u8]) -> Hmac<Sha256> {
        let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.key)
            .expect("HMAC takes any key length");
        m.update(payload);
        m
    }

    /// Returns the token and its expiry (Unix seconds).
    pub fn issue(&self, who: OperatorIdentity, ttl: Duration) -> Result<(String, u64)> {
        let expires = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + ttl.as_secs();
        let mut payload = Vec::with_capacity(40);
        payload.extend_from_slice(who.operator_id.0.as_bytes());
        payload.extend_from_slice(who.org_id.0.as_bytes());
        payload.extend_from_slice(&expires.to_be_bytes());
        let tag = self.mac(&payload).finalize().into_bytes();
        Ok((
            format!("v1.{}.{}", B64.encode(&payload), B64.encode(tag)),
            expires,
        ))
    }

    pub fn verify(&self, token: &str) -> Result<OperatorIdentity, AuthError> {
        let mut parts = token.split('.');
        let (Some("v1"), Some(p), Some(t), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(AuthError::Invalid);
        };
        let payload = B64.decode(p).map_err(|_| AuthError::Invalid)?;
        let tag = B64.decode(t).map_err(|_| AuthError::Invalid)?;
        // Constant-time comparison.
        self.mac(&payload)
            .verify_slice(&tag)
            .map_err(|_| AuthError::Invalid)?;
        let payload: [u8; 40] = payload.try_into().map_err(|_| AuthError::Invalid)?;
        let expires = u64::from_be_bytes(payload[32..40].try_into().expect("8 bytes"));
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| AuthError::Invalid)?
            .as_secs();
        if now >= expires {
            return Err(AuthError::Expired);
        }
        let id = |b: &[u8]| Uuid::from_slice(b).map_err(|_| AuthError::Invalid);
        Ok(OperatorIdentity {
            operator_id: OperatorId(id(&payload[..16])?),
            org_id: OrgId(id(&payload[16..32])?),
        })
    }
}

/// Read a password from `RCONN_PASSWORD`, or the first line of stdin.
pub fn read_password() -> Result<String> {
    if let Ok(p) = std::env::var("RCONN_PASSWORD") {
        return Ok(p);
    }
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading password from stdin")?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_round_trip_and_rules() {
        let mut a = Accounts::default();
        let id = a.add("alice", "correct horse battery").unwrap();
        assert_eq!(a.verify("alice", "correct horse battery"), Some(id));
        assert_eq!(a.verify("alice", "wrong password here"), None);
        assert_eq!(a.verify("bob", "correct horse battery"), None);
        assert!(
            a.add("alice", "another long password").is_err(),
            "duplicate"
        );
        assert!(a.add("carol", "short").is_err(), "too short");
        assert!(
            !a.0["alice"].password_hash.contains("correct horse"),
            "plaintext must not be stored"
        );
    }

    #[test]
    fn tokens_verify_expire_and_resist_tampering() {
        let tokens = SessionTokens::new([7; 32]);
        let who = OperatorIdentity {
            operator_id: OperatorId::new(),
            org_id: OrgId::new(),
        };
        let (tok, _) = tokens.issue(who, Duration::from_secs(60)).unwrap();
        assert_eq!(tokens.verify(&tok).unwrap(), who);

        let (expired, _) = tokens.issue(who, Duration::ZERO).unwrap();
        assert_eq!(tokens.verify(&expired), Err(AuthError::Expired));

        let other_key = SessionTokens::new([8; 32]);
        assert_eq!(other_key.verify(&tok), Err(AuthError::Invalid));

        let mut forged = tok.clone();
        let last = forged.pop().unwrap();
        forged.push(if last == 'A' { 'B' } else { 'A' });
        assert_eq!(tokens.verify(&forged), Err(AuthError::Invalid));
        assert_eq!(tokens.verify("v2.x.y"), Err(AuthError::Invalid));
    }
}
