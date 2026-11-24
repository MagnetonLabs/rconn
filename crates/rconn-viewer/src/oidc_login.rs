//! `login --oidc`: Authorization Code + PKCE in the user's browser, loopback redirect
//! (RFC 8252). The ID token (nonce checked here) is exchanged with the server for a
//! session token.

use anyhow::{Context, Result, anyhow, bail};
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, CsrfToken, IssuerUrl, Nonce, OAuth2TokenResponse,
    PkceCodeChallenge, RedirectUrl, Scope, TokenResponse,
};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::Duration;

/// Run the browser flow and return a verified ID token.
pub async fn id_token(issuer: &str, client_id: &str, port: u16) -> Result<String> {
    id_token_with(issuer, client_id, port, |url| {
        println!("Opening your browser to sign in. If it doesn't open, visit:\n{url}\n");
        open_browser(&url);
    })
    .await
}

/// As [`id_token`], with the "open a browser" step supplied (tests drive the IdP directly).
pub async fn id_token_with(
    issuer: &str,
    client_id: &str,
    port: u16,
    open: impl FnOnce(String),
) -> Result<String> {
    let http = openidconnect::reqwest::ClientBuilder::new()
        .redirect(openidconnect::reqwest::redirect::Policy::none())
        .build()?;
    let metadata = CoreProviderMetadata::discover_async(IssuerUrl::new(issuer.to_owned())?, &http)
        .await
        .with_context(|| format!("OIDC discovery at {issuer}"))?;
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let client =
        CoreClient::from_provider_metadata(metadata, ClientId::new(client_id.to_owned()), None)
            .set_redirect_uri(RedirectUrl::new(redirect.clone())?);

    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("email".into()))
        .set_pkce_challenge(challenge)
        .url();

    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("listening on {redirect}"))?;
    open(url.to_string());
    let (code, got_state) =
        tokio::task::spawn_blocking(move || wait_for_callback(&listener)).await??;
    if got_state != *state.secret() {
        bail!("login state mismatch (possible CSRF); try again");
    }

    let token = client
        .exchange_code(AuthorizationCode::new(code))?
        .set_pkce_verifier(verifier)
        .request_async(&http)
        .await
        .context("exchanging the authorization code")?;
    let _ = token.access_token(); // not used: the server only needs the ID token
    let id = token
        .id_token()
        .ok_or_else(|| anyhow!("the IdP returned no ID token"))?;
    id.claims(&client.id_token_verifier(), &nonce)
        .context("ID token failed verification")?;
    Ok(id.to_string())
}

fn wait_for_callback(listener: &TcpListener) -> Result<(String, String)> {
    listener.set_nonblocking(false)?;
    let (mut stream, _) = listener.accept()?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let target = line.split_whitespace().nth(1).unwrap_or("");
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let param = |k: &str| {
        query.split('&').find_map(|kv| {
            kv.split_once('=')
                .filter(|(key, _)| *key == k)
                .map(|(_, v)| v.to_owned())
        })
    };
    let body = "Signed in. You can close this window and return to rconn.";
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    if let Some(err) = param("error") {
        bail!("sign-in failed: {err}");
    }
    Ok((
        param("code").ok_or_else(|| anyhow!("no code in callback"))?,
        param("state").unwrap_or_default(),
    ))
}

fn open_browser(url: &str) {
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()
    } else if cfg!(windows) {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .status()
    } else {
        std::process::Command::new("xdg-open").arg(url).status()
    };
    if let Err(e) = result {
        tracing::warn!(error = %e, "could not open a browser");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives Dex's real login form over HTTP in place of a browser. Needs
    /// RCONN_TEST_DEX (see scripts/dev-dex.yaml); skipped otherwise.
    #[tokio::test]
    async fn pkce_flow_against_dex() {
        let Ok(issuer) = std::env::var("RCONN_TEST_DEX") else {
            eprintln!("skipped: set RCONN_TEST_DEX to run against Dex");
            return;
        };
        let fake_browser = |url: String| {
            tokio::spawn(async move {
                let web = openidconnect::reqwest::Client::new();
                // Authorize URL redirects to the local-password login form.
                let form = web.get(&url).send().await.expect("authorize");
                let login_url = form.url().clone();
                // Submitting it redirects (via Dex) to our loopback callback.
                let _ = web
                    .post(login_url)
                    .form(&[("login", "ops@example.com"), ("password", "password")])
                    .send()
                    .await;
            });
        };
        let token = id_token_with(&issuer, "rconn-viewer", 8765, fake_browser)
            .await
            .expect("PKCE flow");
        assert_eq!(token.split('.').count(), 3, "a JWT");
    }
}
