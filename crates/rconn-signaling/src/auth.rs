//! Authentication seam. The broker only knows "is this host/operator who they claim,
//! and which org are they in". Concrete schemes (fleet-CA host certs, accounts,
//! passkeys, OIDC) implement [`Authenticator`].

use async_trait::async_trait;
use rconn_core::ids::{HostId, OperatorId, OrgId};
use rconn_core::protocol::{IceServer, OidcLoginInfo, SessionPolicy};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AuthError {
    #[error("invalid credentials")]
    Invalid,
    #[error("credential revoked")]
    Revoked,
    #[error("credential expired")]
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostIdentity {
    pub host_id: HostId,
    pub org_id: OrgId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperatorIdentity {
    pub operator_id: OperatorId,
    pub org_id: OrgId,
}

/// What a signed-in operator may do with their org's computers (from their role).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrgAccess {
    /// Not (or no longer) a member: refused at sign-in, sees nothing.
    None,
    /// Sees the org's computers but can't connect (viewer).
    List,
    /// Sees and connects (technician and up).
    Connect,
}

/// Everything a host sent (plus the server's nonce) for one authentication attempt.
#[derive(Debug, Clone, Copy)]
pub struct HostProofInput<'a> {
    pub host_id: HostId,
    pub cert_pem: &'a str,
    pub client_nonce: &'a [u8],
    pub server_nonce: &'a [u8],
    pub signature: &'a [u8],
}

#[async_trait]
pub trait Authenticator: Send + Sync + 'static {
    /// Verify a host's certificate and its signature over the handshake transcript
    /// (see `rconn_core::fleet::host_proof_transcript`).
    async fn verify_host(&self, proof: HostProofInput<'_>) -> Result<HostIdentity, AuthError>;

    async fn verify_operator(&self, token: &str) -> Result<OperatorIdentity, AuthError>;

    /// Exchange a username and password for a session token and its expiry (Unix seconds).
    async fn password_login(
        &self,
        _username: &str,
        _password: &str,
    ) -> Result<(OperatorIdentity, String, u64), AuthError> {
        Err(AuthError::Invalid)
    }

    /// Login methods offered to operators: (password accounts?, OIDC provider).
    fn login_options(&self) -> (bool, Option<OidcLoginInfo>) {
        (false, None)
    }

    /// Exchange a verified IdP ID token for a session token and its expiry.
    async fn oidc_login(
        &self,
        _id_token: &str,
    ) -> Result<(OperatorIdentity, String, u64), AuthError> {
        Err(AuthError::Invalid)
    }

    /// What `op` may do with their org's computers. Asked on every sign-in, computer list
    /// and session request. Without people and roles (the default), everyone connects.
    async fn org_access(&self, _op: OperatorIdentity) -> OrgAccess {
        OrgAccess::Connect
    }

    /// STUN/TURN servers (with short-lived credentials) for a member of `org`.
    async fn ice_servers(&self, org: OrgId) -> Vec<IceServer>;

    /// Effective policy for a new session to `host`.
    async fn session_policy(&self, host: HostIdentity) -> SessionPolicy;
}
