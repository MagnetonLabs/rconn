//! OpenID Connect: talking to an identity provider (IdP).
//!
//! Two users:
//! - Operator login for self-hosted servers ([`OidcVerifier`], bring your own IdP). The
//!   viewer runs Authorization Code + PKCE against the IdP itself (loopback redirect) and
//!   hands the resulting ID token to the server, which verifies it: signature against the
//!   IdP's JWKS, issuer, audience (our client id) and expiry. The operator's identity is a
//!   stable UUIDv5 of `issuer + subject`, so it survives email changes. Access control is
//!   the IdP's job; optionally, `allowed_domains` additionally requires a verified email
//!   in those domains.
//! - Company SSO in the admin portal (`portal::sso`): the server is a confidential client
//!   and runs the code flow itself ([`OidcProvider::authorize_url`],
//!   [`OidcProvider::exchange_code`]).
//!
//! Both fetch the IdP's documents through [`OidcHttp`], which follows no redirects, caps
//! response size, and (for issuers that a company owner typed in) refuses to reach
//! loopback, private or link-local addresses.

use crate::auth::{AuthError, OperatorIdentity};
use anyhow::{Context, Result, anyhow, bail};
use openidconnect::core::{
    CoreAuthenticationFlow, CoreClient, CoreClientAuthMethod, CoreIdToken, CoreProviderMetadata,
};
use openidconnect::reqwest;
use openidconnect::{
    AsyncHttpClient, AuthType, AuthorizationCode, ClientId, ClientSecret, CsrfToken,
    EndpointMaybeSet, EndpointNotSet, EndpointSet, HttpClientError, HttpRequest, HttpResponse,
    IssuerUrl, Nonce, NonceVerifier, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
    TokenResponse,
};
use rconn_core::ids::{OperatorId, OrgId};
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// Largest IdP response read (discovery, JWKS, token).
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

// -- HTTP ------------------------------------------------------------------------------

/// HTTP client for IdP requests.
#[derive(Clone)]
pub struct OidcHttp {
    client: reqwest::Client,
    /// Only `https://` URLs, and only to public addresses.
    public_only: bool,
}

impl OidcHttp {
    /// For an IdP the server operator configured: any address, any scheme.
    pub fn trusted() -> Result<Self> {
        Self::build(false)
    }

    /// For an IdP a company owner typed in: `https` only, and never an address inside the
    /// server's network (loopback, private, link-local, ...), checked on every connection.
    pub fn public_only() -> Result<Self> {
        Self::build(true)
    }

    fn build(public_only: bool) -> Result<Self> {
        let mut b = reqwest::ClientBuilder::new()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(HTTP_TIMEOUT)
            .connect_timeout(HTTP_TIMEOUT);
        if public_only {
            b = b.dns_resolver(Arc::new(PublicResolver));
        }
        Ok(Self {
            client: b.build().context("building HTTP client")?,
            public_only,
        })
    }

    fn check(&self, url: &reqwest::Url) -> std::result::Result<(), String> {
        if !self.public_only {
            return Ok(());
        }
        if url.scheme() != "https" {
            return Err(format!("{url} is not https"));
        }
        match url.host() {
            Some(url::Host::Ipv4(ip)) if !is_public(IpAddr::V4(ip)) => {
                Err(format!("{url} is not a public address"))
            }
            Some(url::Host::Ipv6(ip)) if !is_public(IpAddr::V6(ip)) => {
                Err(format!("{url} is not a public address"))
            }
            Some(url::Host::Domain(d)) if d.eq_ignore_ascii_case("localhost") => {
                Err(format!("{url} is not a public address"))
            }
            Some(_) => Ok(()),
            None => Err(format!("{url} has no host")),
        }
    }
}

type HttpFuture<'c> = Pin<
    Box<
        dyn Future<Output = std::result::Result<HttpResponse, HttpClientError<reqwest::Error>>>
            + Send
            + 'c,
    >,
>;

impl<'c> AsyncHttpClient<'c> for OidcHttp {
    type Error = HttpClientError<reqwest::Error>;
    type Future = HttpFuture<'c>;

    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(async move {
            let request: reqwest::Request = request.try_into().map_err(Box::new)?;
            self.check(request.url()).map_err(HttpClientError::Other)?;
            let mut response = self.client.execute(request).await.map_err(Box::new)?;
            let mut builder = openidconnect::http::Response::builder()
                .status(response.status())
                .version(response.version());
            for (name, value) in response.headers() {
                builder = builder.header(name, value);
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(Box::new)? {
                if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                    return Err(HttpClientError::Other("response too large".into()));
                }
                body.extend_from_slice(&chunk);
            }
            builder.body(body).map_err(HttpClientError::Http)
        })
    }
}

/// Resolves names like the system does, minus every non-public address.
struct PublicResolver;

impl reqwest::dns::Resolve for PublicResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| is_public(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!("{host} has no public address").into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// An address on the public internet (not loopback, private, link-local, shared,
/// documentation, multicast, ...).
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10 shared
                || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24
                || (o[0] == 198 && (o[1] & 0xfe) == 18) // 198.18.0.0/15 benchmarking
                || o[0] >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link-local
                || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
                || (s[0] == 0x0064 && s[1] == 0xff9b)) // NAT64 (maps to IPv4)
        }
    }
}

// -- provider --------------------------------------------------------------------------

/// What an IdP said about someone, from a verified ID token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcClaims {
    pub issuer: String,
    pub subject: String,
    pub email: Option<String>,
    /// `None` when the IdP doesn't say.
    pub email_verified: Option<bool>,
    pub name: Option<String>,
}

/// One IdP and our client registration at it (discovery document and keys loaded).
#[derive(Debug, Clone)]
pub struct OidcProvider {
    metadata: CoreProviderMetadata,
    client_id: String,
    client_secret: Option<String>,
}

impl OidcProvider {
    /// Fetch the IdP's discovery document and keys. The document must name `issuer`
    /// exactly as given.
    pub async fn discover(
        http: &OidcHttp,
        issuer: &str,
        client_id: &str,
        client_secret: Option<&str>,
    ) -> Result<Self> {
        let metadata =
            CoreProviderMetadata::discover_async(IssuerUrl::new(issuer.to_owned())?, http)
                .await
                .map_err(|e| anyhow!("{}", error_chain(&e)))
                .with_context(|| format!("OIDC discovery at {issuer}"))?;
        Ok(Self {
            metadata,
            client_id: client_id.to_owned(),
            client_secret: client_secret.map(str::to_owned),
        })
    }

    pub fn issuer(&self) -> &str {
        self.metadata.issuer().as_str()
    }

    fn client(
        &self,
    ) -> CoreClient<
        EndpointSet,
        EndpointNotSet,
        EndpointNotSet,
        EndpointNotSet,
        EndpointMaybeSet,
        EndpointMaybeSet,
    > {
        let client = CoreClient::from_provider_metadata(
            self.metadata.clone(),
            ClientId::new(self.client_id.clone()),
            self.client_secret.clone().map(ClientSecret::new),
        );
        // HTTP Basic is the default; use the form body when that's all the IdP takes.
        let methods = self.metadata.token_endpoint_auth_methods_supported();
        let post_only = methods.is_some_and(|m| {
            m.contains(&CoreClientAuthMethod::ClientSecretPost)
                && !m.contains(&CoreClientAuthMethod::ClientSecretBasic)
        });
        if post_only {
            client.set_auth_type(AuthType::RequestBody)
        } else {
            client
        }
    }

    /// Verify an ID token (signature, issuer, audience, expiry) and, when `nonce` is
    /// given, that it carries exactly that nonce.
    pub fn verify_id_token(
        &self,
        id_token: &str,
        nonce: Option<&str>,
    ) -> Result<OidcClaims, AuthError> {
        let token: CoreIdToken = id_token.parse().map_err(|_| AuthError::Invalid)?;
        self.claims_of(&token, nonce)
    }

    fn claims_of(&self, token: &CoreIdToken, nonce: Option<&str>) -> Result<OidcClaims, AuthError> {
        let client = self.client();
        let verifier = client.id_token_verifier();
        let checked = match nonce {
            Some(n) => token.claims(&verifier, &Nonce::new(n.to_owned())),
            // The viewer chose and checked the nonce itself (it ran the flow); the server
            // relies on audience + expiry + signature.
            None => token.claims(&verifier, AnyNonce),
        };
        let claims = checked.map_err(|e| {
            tracing::debug!(error = %e, "OIDC token rejected");
            AuthError::Invalid
        })?;
        Ok(OidcClaims {
            issuer: claims.issuer().as_str().to_owned(),
            subject: claims.subject().as_str().to_owned(),
            email: claims.email().map(|e| e.as_str().to_owned()),
            email_verified: claims.email_verified(),
            name: claims
                .name()
                .and_then(|n| n.get(None))
                .map(|n| n.as_str().to_owned()),
        })
    }

    /// Where to send the browser to sign in: authorization code flow with PKCE (S256),
    /// our `state` and `nonce`, scopes `openid email profile`.
    pub fn authorize_url(
        &self,
        redirect_uri: &str,
        state: &str,
        nonce: &str,
        pkce_verifier: &str,
    ) -> Result<String> {
        let challenge = PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(
            pkce_verifier.to_owned(),
        ));
        let (state, nonce) = (state.to_owned(), nonce.to_owned());
        let (url, _, _) = self
            .client()
            .set_redirect_uri(RedirectUrl::new(redirect_uri.to_owned())?)
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                move || CsrfToken::new(state),
                move || Nonce::new(nonce),
            )
            .add_scope(Scope::new("email".into()))
            .add_scope(Scope::new("profile".into()))
            .set_pkce_challenge(challenge)
            .url();
        Ok(url.into())
    }

    /// Trade an authorization code for an ID token and verify it, nonce included.
    pub async fn exchange_code(
        &self,
        http: &OidcHttp,
        redirect_uri: &str,
        code: &str,
        pkce_verifier: &str,
        nonce: &str,
    ) -> Result<OidcClaims> {
        let client = self
            .client()
            .set_redirect_uri(RedirectUrl::new(redirect_uri.to_owned())?);
        let response = client
            .exchange_code(AuthorizationCode::new(code.to_owned()))
            .context("the identity provider has no token endpoint")?
            .set_pkce_verifier(PkceCodeVerifier::new(pkce_verifier.to_owned()))
            .request_async(http)
            .await
            .map_err(|e| anyhow!("token request: {}", error_chain(&e)))?;
        let Some(token) = response.id_token() else {
            bail!("the identity provider sent no ID token");
        };
        self.claims_of(token, Some(nonce))
            .map_err(|_| anyhow!("the ID token did not verify"))
    }
}

/// Accepts any nonce (or none).
struct AnyNonce;

impl NonceVerifier for AnyNonce {
    fn verify(self, _nonce: Option<&Nonce>) -> std::result::Result<(), String> {
        Ok(())
    }
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        s.push_str(": ");
        s.push_str(&c.to_string());
        cur = c.source();
    }
    s
}

// -- operator login (viewer) -----------------------------------------------------------

/// What the viewer needs to start a login, as advertised by the server.
#[derive(Debug, Clone)]
pub struct OidcClientInfo {
    pub issuer: String,
    pub client_id: String,
}

pub struct OidcVerifier {
    info: OidcClientInfo,
    provider: OidcProvider,
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
        let provider =
            OidcProvider::discover(&OidcHttp::trusted()?, issuer, client_id, None).await?;
        Ok(Self {
            info: OidcClientInfo {
                issuer: issuer.to_owned(),
                client_id: client_id.to_owned(),
            },
            provider,
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
        let claims = self.provider.verify_id_token(id_token, None)?;
        if !self.allowed_domains.is_empty() {
            let verified = claims.email_verified.unwrap_or(false);
            let domain = claims
                .email
                .as_deref()
                .and_then(|e| e.rsplit_once('@').map(|(_, d)| d.to_ascii_lowercase()));
            if !verified || !domain.is_some_and(|d| self.allowed_domains.contains(&d)) {
                return Err(AuthError::Invalid);
            }
        }
        let stable = format!("{}|{}", claims.issuer, claims.subject);
        let operator_id = OperatorId(Uuid::new_v5(&Uuid::NAMESPACE_URL, stable.as_bytes()));
        Ok(OperatorIdentity {
            operator_id,
            org_id: self.org,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_addresses() {
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700::1111"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn public_only_refuses_plain_http_and_internal_hosts() {
        let h = OidcHttp::public_only().unwrap();
        for u in [
            "http://idp.example.com/x",
            "https://127.0.0.1/x",
            "https://[::1]/x",
            "https://169.254.169.254/latest",
            "https://localhost/x",
        ] {
            assert!(h.check(&u.parse().unwrap()).is_err(), "{u}");
        }
        assert!(
            h.check(&"https://login.example.com/x".parse().unwrap())
                .is_ok()
        );
        let t = OidcHttp::trusted().unwrap();
        assert!(
            t.check(&"http://127.0.0.1:5556/dex".parse().unwrap())
                .is_ok()
        );
    }
}
