//! Production [`Authenticator`] for hosts: fleet-CA certificates plus a signed handshake
//! transcript. Operator auth here is a static token table, which bridges until the
//! accounts/passkey/OIDC store lands; tokens are compared by SHA-256 digest.

use crate::accounts::{Accounts, SESSION_TOKEN_TTL, SessionTokens};
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
    accounts: Option<(Accounts, OrgId)>,
    tokens: Option<SessionTokens>,
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
            accounts: None,
            tokens: None,
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

    /// Enable password login for `org`'s accounts, issuing tokens signed with `tokens`.
    pub fn with_accounts(mut self, org: OrgId, accounts: Accounts, tokens: SessionTokens) -> Self {
        self.accounts = Some((accounts, org));
        self.tokens = Some(tokens);
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
        if let Some(who) = self.operator_tokens.get(&digest(token)) {
            return Ok(*who);
        }
        match &self.tokens {
            Some(tokens) if token.starts_with("v1.") => tokens.verify(token),
            _ => Err(AuthError::Invalid),
        }
    }

    async fn password_login(
        &self,
        username: &str,
        password: &str,
    ) -> Result<(OperatorIdentity, String, u64), AuthError> {
        let (Some((accounts, org)), Some(tokens)) = (&self.accounts, &self.tokens) else {
            return Err(AuthError::Invalid);
        };
        let (accounts, org, username, password) = (
            accounts.clone(),
            *org,
            username.to_owned(),
            password.to_owned(),
        );
        // argon2 is deliberately slow; keep it off the async workers.
        let id = tokio::task::spawn_blocking(move || accounts.verify(&username, &password))
            .await
            .map_err(|_| AuthError::Invalid)?
            .ok_or(AuthError::Invalid)?;
        let who = OperatorIdentity {
            operator_id: id,
            org_id: org,
        };
        let (token, expires) = tokens
            .issue(who, SESSION_TOKEN_TTL)
            .map_err(|_| AuthError::Invalid)?;
        Ok((who, token, expires))
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
