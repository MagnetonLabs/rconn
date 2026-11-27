//! Production [`Authenticator`] for hosts: fleet-CA certificates plus a signed handshake
//! transcript. Operator auth here is a static token table, which bridges until the
//! accounts/passkey/OIDC store lands; tokens are compared by SHA-256 digest.
//!
//! Org CAs, tokens, accounts and revocations come from the database and are swapped in
//! whole by [`crate::reload`]; CAs given to the builder (the public CA, tests) stay.

use crate::accounts::{Accounts, SESSION_TOKEN_TTL, SessionTokens};
use crate::auth::{
    AuthError, Authenticator, HostIdentity, HostProofInput, OperatorIdentity, OrgAccess,
};
use crate::oidc::OidcVerifier;
use crate::turn_relay::TurnMinter;
use async_trait::async_trait;
use rconn_core::fleet::{self, FleetError};
use rconn_core::ids::{HostId, OrgId};
use rconn_core::protocol::OidcLoginInfo;
use rconn_core::protocol::{IceServer, SessionPolicy};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

pub struct FleetAuthenticator {
    /// Canonical server identity hosts bind into their proofs (their profile URL).
    server_id: String,
    /// Fleet CA certificate (PEM) per org, given at build time (e.g. the public CA).
    fixed_cas: HashMap<OrgId, String>,
    /// Fleet CA certificate (PEM) per org, from the database.
    loaded_cas: RwLock<HashMap<OrgId, String>>,
    revoked: RwLock<HashSet<HostId>>,
    operator_tokens: RwLock<HashMap<[u8; 32], OperatorIdentity>>,
    ice_servers: Vec<IceServer>,
    turn: Option<TurnMinter>,
    accounts: RwLock<Option<(Accounts, OrgId)>>,
    tokens: Option<SessionTokens>,
    oidc: Option<OidcVerifier>,
    policy: SessionPolicy,
    /// People and roles: who may list and connect (see [`Authenticator::org_access`]).
    db: Option<crate::db::Db>,
}

impl FleetAuthenticator {
    pub fn new(server_id: impl Into<String>) -> Self {
        Self {
            server_id: server_id.into(),
            fixed_cas: HashMap::new(),
            loaded_cas: RwLock::default(),
            revoked: RwLock::default(),
            operator_tokens: RwLock::default(),
            ice_servers: Vec::new(),
            turn: None,
            accounts: RwLock::default(),
            tokens: None,
            oidc: None,
            policy: SessionPolicy {
                guest: false,
                show_indicator: false,
                clipboard: true,
                file_transfer: true,
                audio: true,
                require_approval: false,
            },
            db: None,
        }
    }

    /// Decide who may list and connect to org computers from the database's memberships,
    /// on every request (without it, every operator of an org connects).
    pub fn with_db(mut self, db: crate::db::Db) -> Self {
        self.db = Some(db);
        self
    }

    pub fn with_org_ca(mut self, org: OrgId, ca_pem: impl Into<String>) -> Self {
        self.fixed_cas.insert(org, ca_pem.into());
        self
    }

    /// Replace the CAs loaded from the database (the builder's stay).
    pub fn replace_org_cas(&self, cas: HashMap<OrgId, String>) {
        *self.loaded_cas.write().expect("auth lock") = cas;
    }

    /// The CA certificate hosts of `org` are checked against.
    fn org_ca(&self, org: OrgId) -> Option<String> {
        if let Some(ca) = self.fixed_cas.get(&org) {
            return Some(ca.clone());
        }
        self.loaded_cas
            .read()
            .expect("auth lock")
            .get(&org)
            .cloned()
    }

    pub fn with_operator_token(mut self, token: &str, identity: OperatorIdentity) -> Self {
        self.operator_tokens
            .get_mut()
            .expect("auth lock")
            .insert(digest(token), identity);
        self
    }

    /// Register an operator by the SHA-256 of its token (as stored on disk).
    pub fn with_operator_token_digest(
        mut self,
        digest: [u8; 32],
        identity: OperatorIdentity,
    ) -> Self {
        self.operator_tokens
            .get_mut()
            .expect("auth lock")
            .insert(digest, identity);
        self
    }

    pub fn with_ice_servers(mut self, servers: Vec<IceServer>) -> Self {
        self.ice_servers = servers;
        self
    }

    /// Enable password login for `org`'s accounts, issuing tokens signed with `tokens`.
    pub fn with_accounts(mut self, org: OrgId, accounts: Accounts, tokens: SessionTokens) -> Self {
        *self.accounts.get_mut().expect("auth lock") = Some((accounts, org));
        self.tokens = Some(tokens);
        self
    }

    /// Accept ID tokens from this IdP (requires `with_accounts` for the token key).
    pub fn with_oidc(mut self, verifier: OidcVerifier) -> Self {
        self.oidc = Some(verifier);
        self
    }

    /// Mint TURN credentials into every `Welcome`.
    pub fn with_turn(mut self, minter: TurnMinter) -> Self {
        self.turn = Some(minter);
        self
    }

    /// The policy sent to hosts with every session (the default allows clipboard and
    /// file transfer, no indicator).
    pub fn with_session_policy(mut self, policy: SessionPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Replace the static operator tokens (by digest), e.g. after the database changed.
    pub fn replace_operator_digests(&self, table: HashMap<[u8; 32], OperatorIdentity>) {
        *self.operator_tokens.write().expect("auth lock") = table;
    }

    /// Replace the account table, e.g. after the database changed.
    pub fn replace_accounts(&self, org: OrgId, accounts: Accounts) {
        *self.accounts.write().expect("auth lock") = Some((accounts, org));
    }

    /// Replace the revocation set. Returns hosts that are newly revoked.
    pub fn replace_revoked(&self, set: HashSet<HostId>) -> Vec<HostId> {
        let mut cur = self.revoked.write().expect("revocation lock poisoned");
        let added = set.difference(&cur).copied().collect();
        *cur = set;
        added
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
        let ca = self.org_ca(org).ok_or(AuthError::Invalid)?;
        let claims = fleet::verify_host_cert(&ca, p.cert_pem).map_err(invalid)?;
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
        if let Some(who) = self
            .operator_tokens
            .read()
            .expect("auth lock")
            .get(&digest(token))
        {
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
        let Some(tokens) = &self.tokens else {
            return Err(AuthError::Invalid);
        };
        let Some((accounts, org)) = self.accounts.read().expect("auth lock").clone() else {
            return Err(AuthError::Invalid);
        };
        let (username, password) = (username.to_owned(), password.to_owned());
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

    fn login_options(&self) -> (bool, Option<OidcLoginInfo>) {
        let password = self
            .accounts
            .read()
            .expect("auth lock")
            .as_ref()
            .is_some_and(|(a, _)| !a.0.is_empty());
        let oidc = self.oidc.as_ref().map(|v| OidcLoginInfo {
            issuer: v.client_info().issuer.clone(),
            client_id: v.client_info().client_id.clone(),
        });
        (password, oidc)
    }

    async fn oidc_login(
        &self,
        id_token: &str,
    ) -> Result<(OperatorIdentity, String, u64), AuthError> {
        let (Some(verifier), Some(tokens)) = (&self.oidc, &self.tokens) else {
            return Err(AuthError::Invalid);
        };
        let who = verifier.verify(id_token)?;
        let (token, expires) = tokens
            .issue(who, SESSION_TOKEN_TTL)
            .map_err(|_| AuthError::Invalid)?;
        Ok((who, token, expires))
    }

    async fn org_access(&self, op: OperatorIdentity) -> OrgAccess {
        let Some(db) = &self.db else {
            return OrgAccess::Connect;
        };
        match crate::portal::people::desktop_access(db, op).await {
            Ok(access) => access,
            Err(e) => {
                // Fail closed: a database problem never widens access.
                tracing::error!(error = %format!("{e:#}"), "reading an operator's role failed");
                OrgAccess::None
            }
        }
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

    fn revoke_host(&self, host: HostId) {
        self.revoke(host);
    }
}
