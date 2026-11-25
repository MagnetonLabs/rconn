//! Access-password connections: someone with a computer's ID and its access password can
//! connect without an account (like AnyDesk's and RustDesk's unattended access).
//!
//! The host checks the password itself, with SPAKE2 (a password-authenticated key
//! exchange) run over the session's encrypted data channel:
//! * the password never leaves the viewer, and the host stores only a derived verifier;
//! * the signaling server (or anyone relaying) learns nothing it could test guesses
//!   against offline: each guess costs a live attempt, which the host rate-limits;
//! * both sides prove they derived the same key, bound to this session's id.

use crate::ids::SessionId;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use thiserror::Error;

const SALT_LEN: usize = 16;
const KEY_LEN: usize = 32;
const VERIFIER_PREFIX: &str = "v1$";
/// The shortest access password accepted.
pub const MIN_PASSWORD_LEN: usize = 8;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum GuestError {
    #[error("the access password must be at least {MIN_PASSWORD_LEN} characters")]
    TooShort,
    #[error("the stored access password is unreadable; set it again")]
    BadVerifier,
    #[error("wrong access password")]
    WrongPassword,
    #[error("malformed access-password message")]
    Malformed,
    #[error("no randomness available: {0}")]
    Random(String),
}

/// Argon2id with OWASP's recommended settings: ~50 ms per guess, 19 MiB each.
fn derive(password: &str, salt: &[u8]) -> Result<[u8; KEY_LEN], GuestError> {
    let params =
        argon2::Params::new(19 * 1024, 2, 1, Some(KEY_LEN)).map_err(|_| GuestError::Malformed)?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = [0u8; KEY_LEN];
    argon
        .hash_password_into(password.as_bytes(), salt, &mut out)
        .map_err(|_| GuestError::Malformed)?;
    Ok(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// What a host stores for its access password: `v1$<salt>$<derived key>` (hex). It is not
/// the password, but it can answer for the host, so keep it admin-only like the password.
pub fn make_verifier(password: &str) -> Result<String, GuestError> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(GuestError::TooShort);
    }
    let mut salt = [0u8; SALT_LEN];
    getrandom::fill(&mut salt).map_err(|e| GuestError::Random(e.to_string()))?;
    let key = derive(password, &salt)?;
    Ok(format!("{VERIFIER_PREFIX}{}${}", hex(&salt), hex(&key)))
}

fn parse_verifier(v: &str) -> Result<(Vec<u8>, Vec<u8>), GuestError> {
    let (salt, key) = v
        .strip_prefix(VERIFIER_PREFIX)
        .and_then(|rest| rest.split_once('$'))
        .ok_or(GuestError::BadVerifier)?;
    match (unhex(salt), unhex(key)) {
        (Some(s), Some(k)) if s.len() == SALT_LEN && k.len() == KEY_LEN => Ok((s, k)),
        _ => Err(GuestError::BadVerifier),
    }
}

fn ids() -> (Identity, Identity) {
    (Identity::new(b"rconn-viewer"), Identity::new(b"rconn-host"))
}

/// Proof that the sender derived `key` in `session`, from one side's point of view.
fn confirmation(key: &[u8], side: &[u8], session: &SessionId) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes any key");
    mac.update(b"rconn access password v1 ");
    mac.update(side);
    mac.update(session.0.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

fn confirmation_matches(key: &[u8], side: &[u8], session: &SessionId, got: &[u8]) -> bool {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes any key");
    mac.update(b"rconn access password v1 ");
    mac.update(side);
    mac.update(session.0.as_bytes());
    mac.verify_slice(got).is_ok() // constant time
}

/// The host's half of one attempt.
pub struct HostCheck {
    spake: Spake2<Ed25519Group>,
    session: SessionId,
}

/// Sent by the host to start: the salt to derive with, and its SPAKE2 message.
pub struct HostChallenge {
    pub salt: Vec<u8>,
    pub msg: Vec<u8>,
}

impl HostCheck {
    pub fn start(verifier: &str, session: SessionId) -> Result<(Self, HostChallenge), GuestError> {
        let (salt, key) = parse_verifier(verifier)?;
        let (a, b) = ids();
        let (spake, msg) = Spake2::<Ed25519Group>::start_b(&Password::new(&key), &a, &b);
        Ok((Self { spake, session }, HostChallenge { salt, msg }))
    }

    /// Check the viewer's answer. On success, returns the host's own confirmation for the
    /// viewer (proving the host knows the password too, i.e. is the real computer).
    pub fn finish(self, viewer_msg: &[u8], viewer_confirm: &[u8]) -> Result<Vec<u8>, GuestError> {
        let key = self
            .spake
            .finish(viewer_msg)
            .map_err(|_| GuestError::Malformed)?;
        if !confirmation_matches(&key, b"viewer", &self.session, viewer_confirm) {
            return Err(GuestError::WrongPassword);
        }
        Ok(confirmation(&key, b"host", &self.session))
    }
}

/// The viewer's answer to a [`HostChallenge`].
pub struct ViewerAnswer {
    pub msg: Vec<u8>,
    pub confirm: Vec<u8>,
    key: Vec<u8>,
    session: SessionId,
}

impl ViewerAnswer {
    pub fn new(
        password: &str,
        salt: &[u8],
        host_msg: &[u8],
        session: SessionId,
    ) -> Result<Self, GuestError> {
        let derived = derive(password, salt)?;
        let (a, b) = ids();
        let (spake, msg) = Spake2::<Ed25519Group>::start_a(&Password::new(derived), &a, &b);
        let key = spake.finish(host_msg).map_err(|_| GuestError::Malformed)?;
        Ok(Self {
            msg,
            confirm: confirmation(&key, b"viewer", &session),
            key,
            session,
        })
    }

    /// Is the host's confirmation genuine (the computer knows the password as well)?
    pub fn host_is_genuine(&self, host_confirm: &[u8]) -> bool {
        confirmation_matches(&self.key, b"host", &self.session, host_confirm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(stored: &str, typed: &str) -> (Result<Vec<u8>, GuestError>, ViewerAnswer) {
        let session = SessionId::new();
        let (host, ch) = HostCheck::start(stored, session).unwrap();
        let answer = ViewerAnswer::new(typed, &ch.salt, &ch.msg, session).unwrap();
        (host.finish(&answer.msg, &answer.confirm), answer)
    }

    #[test]
    fn the_right_password_connects_and_both_sides_are_proven() {
        let v = make_verifier("correct horse").unwrap();
        let (result, answer) = attempt(&v, "correct horse");
        let host_confirm = result.expect("accepted");
        assert!(answer.host_is_genuine(&host_confirm));
    }

    #[test]
    fn a_wrong_password_is_refused() {
        let v = make_verifier("correct horse").unwrap();
        assert_eq!(
            attempt(&v, "correct hors").0,
            Err(GuestError::WrongPassword)
        );
    }

    #[test]
    fn a_proof_from_another_session_is_refused() {
        let v = make_verifier("correct horse").unwrap();
        let (host, ch) = HostCheck::start(&v, SessionId::new()).unwrap();
        let replay =
            ViewerAnswer::new("correct horse", &ch.salt, &ch.msg, SessionId::new()).unwrap();
        assert_eq!(
            host.finish(&replay.msg, &replay.confirm),
            Err(GuestError::WrongPassword)
        );
    }

    #[test]
    fn verifiers_hide_the_password_and_differ_per_salt() {
        let a = make_verifier("correct horse").unwrap();
        let b = make_verifier("correct horse").unwrap();
        assert_ne!(a, b, "random salt");
        assert!(!a.contains("correct"));
        assert_eq!(make_verifier("short"), Err(GuestError::TooShort));
        assert!(HostCheck::start("garbage", SessionId::new()).is_err());
    }
}
