//! Access-password connections: someone with a computer's ID and its access password can
//! connect without an account (like AnyDesk's and RustDesk's unattended access).
//!
//! The host checks the password itself, with SPAKE2 (a password-authenticated key
//! exchange) run over the session's encrypted data channel:
//! * the password never leaves the viewer, and the host stores only a derived verifier;
//! * the signaling server (or anyone relaying) learns nothing it could test guesses
//!   against offline: each guess costs a live attempt, which the host rate-limits;
//! * both sides prove they derived the same key, bound to this session's id and to the
//!   DTLS certificates of its WebRTC connection ([`Binding`]).
//!
//! The DTLS binding is what stops a malicious signaling server. The server relays the SDP,
//! so it could hand each side its own certificate fingerprint and sit in the middle of the
//! "encrypted" connection, relaying the SPAKE2 messages untouched. Each WebRTC stack only
//! accepts the certificate named in the SDP it received, so in that case the two sides
//! see different fingerprints, bind different values, and the confirmations don't match:
//! the password check fails instead of opening a session the server can watch.
//!
//! Sessions the person at the computer accepts without a password (no access password set)
//! have no shared secret to bind, so they trust the signaling server not to do this.

use crate::ids::SessionId;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use thiserror::Error;

/// What the host agent asks the local app to show (`rconn --ask <json>`): someone wants to
/// connect; accept or decline within `timeout_secs`. The app prints one of the `ANSWER_*`
/// words on stdout and exits.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PromptRequest {
    /// Who is asking, as the viewer described itself (unverified).
    pub viewer: String,
    pub timeout_secs: u64,
}

pub const ANSWER_ACCEPT: &str = "accept";
pub const ANSWER_DECLINE: &str = "decline";
pub const ANSWER_TIMEOUT: &str = "timeout";

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
    #[error("the connection has no DTLS fingerprint to bind the password check to")]
    NoFingerprint,
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

/// The DTLS certificate fingerprints in an SDP, canonical: every `a=fingerprint:` line
/// (session or media level) as lowercase `algorithm value`, sorted, without duplicates,
/// joined by `,`. `None` if there are none.
pub fn sdp_fingerprints(sdp: &str) -> Option<String> {
    let mut fps: Vec<String> = sdp
        .lines()
        .filter_map(|l| l.trim().strip_prefix("a=fingerprint:"))
        .filter_map(|fp| {
            let mut parts = fp.split_whitespace();
            let (alg, value) = (parts.next()?, parts.next()?);
            Some(format!(
                "{} {}",
                alg.to_ascii_lowercase(),
                value.to_ascii_lowercase()
            ))
        })
        .collect();
    fps.sort();
    fps.dedup();
    (!fps.is_empty()).then(|| fps.join(","))
}

/// What a password check is bound to: the session, and the DTLS certificate fingerprints
/// of its WebRTC connection as each side saw them (see the module docs). Both sides must
/// compute the same value, so it is ordered by role, not by local/remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub session: SessionId,
    /// The viewer's fingerprints, as [`sdp_fingerprints`] gives them.
    pub viewer_dtls: String,
    /// The host's fingerprints, likewise.
    pub host_dtls: String,
}

impl Binding {
    /// From the viewer's SDP (its offer) and the host's (its answer).
    pub fn from_sdp(
        session: SessionId,
        viewer_sdp: &str,
        host_sdp: &str,
    ) -> Result<Self, GuestError> {
        Ok(Self {
            session,
            viewer_dtls: sdp_fingerprints(viewer_sdp).ok_or(GuestError::NoFingerprint)?,
            host_dtls: sdp_fingerprints(host_sdp).ok_or(GuestError::NoFingerprint)?,
        })
    }

    fn mac(&self, key: &[u8], side: &[u8]) -> Hmac<Sha256> {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes any key");
        mac.update(b"rconn access password v2\0");
        // Length-prefixed, so no two different inputs produce the same byte string.
        for part in [
            side,
            self.session.0.as_bytes(),
            self.viewer_dtls.as_bytes(),
            self.host_dtls.as_bytes(),
        ] {
            mac.update(&(part.len() as u32).to_be_bytes());
            mac.update(part);
        }
        mac
    }

    /// Proof that the sender derived `key` on this connection, from one side's view.
    fn confirmation(&self, key: &[u8], side: &[u8]) -> Vec<u8> {
        self.mac(key, side).finalize().into_bytes().to_vec()
    }

    fn confirmation_matches(&self, key: &[u8], side: &[u8], got: &[u8]) -> bool {
        self.mac(key, side).verify_slice(got).is_ok() // constant time
    }
}

/// The host's half of one attempt.
pub struct HostCheck {
    spake: Spake2<Ed25519Group>,
    binding: Binding,
}

/// Sent by the host to start: the salt to derive with, and its SPAKE2 message.
pub struct HostChallenge {
    pub salt: Vec<u8>,
    pub msg: Vec<u8>,
}

impl HostCheck {
    pub fn start(verifier: &str, binding: Binding) -> Result<(Self, HostChallenge), GuestError> {
        let (salt, key) = parse_verifier(verifier)?;
        let (a, b) = ids();
        let (spake, msg) = Spake2::<Ed25519Group>::start_b(&Password::new(&key), &a, &b);
        Ok((Self { spake, binding }, HostChallenge { salt, msg }))
    }

    /// Check the viewer's answer. On success, returns the host's own confirmation for the
    /// viewer (proving the host knows the password too, i.e. is the real computer).
    pub fn finish(self, viewer_msg: &[u8], viewer_confirm: &[u8]) -> Result<Vec<u8>, GuestError> {
        let key = self
            .spake
            .finish(viewer_msg)
            .map_err(|_| GuestError::Malformed)?;
        if !self
            .binding
            .confirmation_matches(&key, b"viewer", viewer_confirm)
        {
            return Err(GuestError::WrongPassword);
        }
        Ok(self.binding.confirmation(&key, b"host"))
    }
}

/// The viewer's answer to a [`HostChallenge`].
pub struct ViewerAnswer {
    pub msg: Vec<u8>,
    pub confirm: Vec<u8>,
    key: Vec<u8>,
    binding: Binding,
}

impl ViewerAnswer {
    pub fn new(
        password: &str,
        salt: &[u8],
        host_msg: &[u8],
        binding: Binding,
    ) -> Result<Self, GuestError> {
        let derived = derive(password, salt)?;
        let (a, b) = ids();
        let (spake, msg) = Spake2::<Ed25519Group>::start_a(&Password::new(derived), &a, &b);
        let key = spake.finish(host_msg).map_err(|_| GuestError::Malformed)?;
        Ok(Self {
            msg,
            confirm: binding.confirmation(&key, b"viewer"),
            key,
            binding,
        })
    }

    /// Is the host's confirmation genuine (the computer knows the password as well, on
    /// this very connection)?
    pub fn host_is_genuine(&self, host_confirm: &[u8]) -> bool {
        self.binding
            .confirmation_matches(&self.key, b"host", host_confirm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEWER_SDP: &str = "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=fingerprint:sha-256 AA:BB:CC\r\nm=video 9 UDP/TLS/RTP/SAVPF 96\r\na=fingerprint:sha-256 AA:BB:CC\r\n";
    const HOST_SDP: &str = "v=0\r\na=fingerprint:sha-256 11:22:33\r\n";

    fn binding(session: SessionId) -> Binding {
        Binding::from_sdp(session, VIEWER_SDP, HOST_SDP).unwrap()
    }

    fn attempt(stored: &str, typed: &str) -> (Result<Vec<u8>, GuestError>, ViewerAnswer) {
        let b = binding(SessionId::new());
        let (host, ch) = HostCheck::start(stored, b.clone()).unwrap();
        let answer = ViewerAnswer::new(typed, &ch.salt, &ch.msg, b).unwrap();
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
        let (host, ch) = HostCheck::start(&v, binding(SessionId::new())).unwrap();
        let replay = ViewerAnswer::new(
            "correct horse",
            &ch.salt,
            &ch.msg,
            binding(SessionId::new()),
        )
        .unwrap();
        assert_eq!(
            host.finish(&replay.msg, &replay.confirm),
            Err(GuestError::WrongPassword)
        );
    }

    /// A signaling server in the middle relays the SPAKE2 messages untouched (both sides
    /// derive the same key), but each side's DTLS connection ends at the server, so each
    /// sees one of the server's fingerprints instead of the other side's.
    #[test]
    fn a_proof_made_over_different_dtls_keys_is_refused() {
        let v = make_verifier("correct horse").unwrap();
        let session = SessionId::new();
        let host_view =
            Binding::from_sdp(session, "a=fingerprint:sha-256 EE:EE\r\n", HOST_SDP).unwrap();
        let viewer_view =
            Binding::from_sdp(session, VIEWER_SDP, "a=fingerprint:sha-256 FF:FF\r\n").unwrap();
        let (host, ch) = HostCheck::start(&v, host_view.clone()).unwrap();
        let answer = ViewerAnswer::new("correct horse", &ch.salt, &ch.msg, viewer_view).unwrap();
        assert_eq!(
            host.finish(&answer.msg, &answer.confirm),
            Err(GuestError::WrongPassword),
            "right password, wrong connection"
        );

        // The host's confirmation is bound the same way: one made on the host's view of
        // the connection doesn't convince a viewer that sees another one.
        let key = [7u8; 32];
        let host_confirm = host_view.confirmation(&key, b"host");
        assert!(host_view.confirmation_matches(&key, b"host", &host_confirm));
        assert!(!binding(session).confirmation_matches(&key, b"host", &host_confirm));

        // Roles are part of it: swapping which fingerprint is whose changes the binding.
        let b = binding(session);
        let swapped = Binding {
            session,
            viewer_dtls: b.host_dtls.clone(),
            host_dtls: b.viewer_dtls.clone(),
        };
        let (host, ch) = HostCheck::start(&v, b).unwrap();
        let answer = ViewerAnswer::new("correct horse", &ch.salt, &ch.msg, swapped).unwrap();
        assert_eq!(
            host.finish(&answer.msg, &answer.confirm),
            Err(GuestError::WrongPassword)
        );
    }

    #[test]
    fn fingerprints_are_canonical() {
        assert_eq!(
            sdp_fingerprints(VIEWER_SDP).as_deref(),
            Some("sha-256 aa:bb:cc"),
            "one per media section collapses; case is folded"
        );
        assert_eq!(
            sdp_fingerprints("a=fingerprint:sha-512 02\r\na=fingerprint:SHA-256 01\n"),
            Some("sha-256 01,sha-512 02".into())
        );
        assert_eq!(sdp_fingerprints("v=0\r\n"), None);
        assert_eq!(
            Binding::from_sdp(SessionId::new(), "v=0", HOST_SDP),
            Err(GuestError::NoFingerprint)
        );
    }

    #[test]
    fn verifiers_hide_the_password_and_differ_per_salt() {
        let a = make_verifier("correct horse").unwrap();
        let b = make_verifier("correct horse").unwrap();
        assert_ne!(a, b, "random salt");
        assert!(!a.contains("correct"));
        assert_eq!(make_verifier("short"), Err(GuestError::TooShort));
        assert!(HostCheck::start("garbage", binding(SessionId::new())).is_err());
    }
}
