//! Authentication seam. The broker only knows "is this host/operator who they claim,
//! and which org are they in". Concrete schemes (fleet-CA host certs, accounts,
//! passkeys, OIDC) implement [`Authenticator`].

use async_trait::async_trait;
use rconn_core::ids::{HostId, OperatorId, OrgId};
use rconn_core::protocol::{IceServer, SessionPolicy};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AuthError {
    #[error("invalid credentials")]
    Invalid,
    #[error("credential revoked")]
    Revoked,
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

    /// STUN/TURN servers (with short-lived credentials) for a member of `org`.
    async fn ice_servers(&self, org: OrgId) -> Vec<IceServer>;

    /// Effective policy for a new session to `host`.
    async fn session_policy(&self, host: HostIdentity) -> SessionPolicy;
}
