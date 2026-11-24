//! OpenID Connect operator login for self-hosted servers (bring your own IdP).
//!
//! The viewer runs Authorization Code + PKCE against the IdP itself (loopback redirect) and
//! hands the resulting ID token to the server, which verifies it here: signature against
//! the IdP's JWKS, issuer, audience (our client id) and expiry. The operator's identity is
//! a stable UUIDv5 of `issuer + subject`, so it survives email changes.
//!
//! Access control is the IdP's job (assign the app to the people who should use it).
//! Optionally, `allowed_domains` additionally requires a verified email in those domains.

use crate::auth::{AuthError, OperatorIdentity};
use anyhow::{Context, Result};
use openidconnect::core::{CoreClient, CoreIdToken, CoreProviderMetadata};
use openidconnect::{ClientId, IssuerUrl, Nonce};
use rconn_core::ids::{OperatorId, OrgId};
use uuid::Uuid;

/// What the viewer needs to start a login, as advertised by the server.
#[derive(Debug, Clone)]
pub struct OidcClientInfo {
    pub issuer: String,
    pub client_id: String,
}

pub struct OidcVerifier {
    info: OidcClientInfo,
    metadata: CoreProviderMetadata,
    org: OrgId,
    allowed_domains: Vec<String>,
}

impl OidcVerifier {
    /// Fetch the IdP's discovery document and keys.
    pub async fn discover(
        issuer: &str,
        client_id: &str,
        org: OrgId,
        allowed_domains: Vec<String>,
    ) -> Result<Self> {
        let http = openidconnect::reqwest::ClientBuilder::new()
            .redirect(openidconnect::reqwest::redirect::Policy::none())
            .build()
            .context("building HTTP client")?;
        let metadata =
            CoreProviderMetadata::discover_async(IssuerUrl::new(issuer.to_owned())?, &http)
                .await
                .with_context(|| format!("OIDC discovery at {issuer}"))?;
        Ok(Self {
            info: OidcClientInfo {
                issuer: issuer.to_owned(),
                client_id: client_id.to_owned(),
            },
            metadata,
            org,
            allowed_domains: allowed_domains
                .into_iter()
                .map(|d| d.to_ascii_lowercase())
                .collect(),
        })
    }

    pub fn client_info(&self) -> &OidcClientInfo {
        &self.info
    }

    /// Verify an ID token and map it to an operator.
    pub fn verify(&self, id_token: &str) -> Result<OperatorIdentity, AuthError> {
        let client = CoreClient::from_provider_metadata(
            self.metadata.clone(),
            ClientId::new(self.info.client_id.clone()),
            None,
        );
        let token: CoreIdToken = id_token.parse().map_err(|_| AuthError::Invalid)?;
        // The nonce was chosen and checked by the viewer that ran the flow; the server
        // relies on audience + expiry + signature.
        let claims = token
            .claims(&client.id_token_verifier(), |_: Option<&Nonce>| {
                Ok::<(), String>(())
            })
            .map_err(|e| {
                tracing::debug!(error = %e, "OIDC token rejected");
                AuthError::Invalid
            })?;
        if !self.allowed_domains.is_empty() {
            let verified = claims.email_verified().unwrap_or(false);
            let domain = claims.email().and_then(|e| {
                e.as_str()
                    .rsplit_once('@')
                    .map(|(_, d)| d.to_ascii_lowercase())
            });
            if !verified || !domain.is_some_and(|d| self.allowed_domains.contains(&d)) {
                return Err(AuthError::Invalid);
            }
        }
        let stable = format!("{}|{}", claims.issuer().as_str(), claims.subject().as_str());
        let operator_id = OperatorId(Uuid::new_v5(&Uuid::NAMESPACE_URL, stable.as_bytes()));
        Ok(OperatorIdentity {
            operator_id,
            org_id: self.org,
        })
    }
}
