//! Production [`Authenticator`] for hosts: fleet-CA certificates plus a signed handshake
//! transcript. Operator auth here is a static token table, which bridges until the
//! accounts/passkey/OIDC store lands; tokens are compared by SHA-256 digest.

use crate::auth::{AuthError, Authenticator, HostIdentity, HostProofInput, OperatorIdentity};
use crate::turn_relay::TurnMinter;
use async_trait::async_trait;
use rconn_core::fleet::{self, FleetError};
use rconn_core::ids::{HostId, OrgId};
use rconn_core::protocol::{IceServer, SessionPolicy};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

pub struct FleetAuthenticator {
    /// Canonical server identity hosts bind into their proofs (their profile URL).
    server_id: String,
    /// Fleet CA certificate (PEM) per org.
    org_cas: HashMap<OrgId, String>,
    revoked: RwLock<HashSet<HostId>>,
    operator_tokens: HashMap<[u8; 32], OperatorIdentity>,
    ice_servers: Vec<IceServer>,
    turn: Option<TurnMinter>,
    policy: SessionPolicy,
}

impl FleetAuthenticator {
    pub fn new(server_id: impl Into<String>) -> Self {
        Self {
            server_id: server_id.into(),
            org_cas: HashMap::new(),
            revoked: RwLock::default(),
            operator_tokens: HashMap::new(),
            ice_servers: Vec::new(),
            turn: None,
            policy: SessionPolicy {
                show_indicator: false,
                clipboard: true,
            },
        }
    }

    pub fn with_org_ca(mut self, org: OrgId, ca_pem: impl Into<String>) -> Self {
        self.org_cas.insert(org, ca_pem.into());
        self
    }

    pub fn with_operator_token(mut self, token: &str, identity: OperatorIdentity) -> Self {
        self.operator_tokens.insert(digest(token), identity);
        self
    }

    /// Register an operator by the SHA-256 of its token (as stored on disk).
    pub fn with_operator_token_digest(
        mut self,
        digest: [u8; 32],
        identity: OperatorIdentity,
    ) -> Self {
        self.operator_tokens.insert(digest, identity);
        self
    }

    pub fn with_ice_servers(mut self, servers: Vec<IceServer>) -> Self {
        self.ice_servers = servers;
        self
    }

    /// Mint TURN credentials into every `Welcome`.
    pub fn with_turn(mut self, minter: TurnMinter) -> Self {
        self.turn = Some(minter);
        self
    }

    /// Refuse this host from now on (takes effect at its next connect).
    pub fn revoke(&self, host: HostId) {
        self.revoked
            .write()
            .expect("revocation lock poisoned")
            .insert(host);
    }
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn invalid(e: FleetError) -> AuthError {
    tracing::debug!(error = %e, "host auth failed");
    AuthError::Invalid
}

#[async_trait]
impl Authenticator for FleetAuthenticator {
    async fn verify_host(&self, p: HostProofInput<'_>) -> Result<HostIdentity, AuthError> {
        let org = fleet::claimed_org(p.cert_pem).map_err(invalid)?;
        let ca = self.org_cas.get(&org).ok_or(AuthError::Invalid)?;
        let claims = fleet::verify_host_cert(ca, p.cert_pem).map_err(invalid)?;
        if claims.host_id != p.host_id {
            return Err(AuthError::Invalid);
        }
        if self
            .revoked
            .read()
            .expect("revocation lock poisoned")
            .contains(&claims.host_id)
        {
            return Err(AuthError::Revoked);
        }
        let transcript = fleet::host_proof_transcript(
            &self.server_id,
            claims.host_id,
            p.cert_pem,
            p.client_nonce,
            p.server_nonce,
        );
        fleet::verify_host_proof(&claims, &transcript, p.signature).map_err(invalid)?;
        Ok(HostIdentity {
            host_id: claims.host_id,
            org_id: claims.org_id,
        })
    }

    async fn verify_operator(&self, token: &str) -> Result<OperatorIdentity, AuthError> {
        self.operator_tokens
            .get(&digest(token))
            .copied()
            .ok_or(AuthError::Invalid)
    }

    async fn ice_servers(&self, _org: OrgId) -> Vec<IceServer> {
        let mut servers = self.ice_servers.clone();
        if let Some(turn) = &self.turn {
            servers.extend(turn.mint());
        }
        servers
    }

    async fn session_policy(&self, _host: HostIdentity) -> SessionPolicy {
        self.policy
    }
}
