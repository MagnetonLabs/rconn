//! Outgoing email (magic links, invites, alerts).
//!
//! [`ResendMailer`] sends through the Resend HTTP API (`RESEND_API_KEY`). Without a key
//! the server uses [`LogMailer`], which writes the message to the log instead, so a
//! self-hoster can still sign in. Tests use [`CaptureMailer`].

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mail {
    pub to: String,
    pub subject: String,
    pub text: String,
}

#[async_trait]
pub trait Mailer: Send + Sync + 'static {
    async fn send(&self, mail: Mail) -> Result<()>;
    /// Whether mail actually leaves the server (false: it only reaches the log).
    fn delivers(&self) -> bool {
        true
    }
}

/// Writes every message to the log. The fallback when no mail service is configured.
pub struct LogMailer;

#[async_trait]
impl Mailer for LogMailer {
    async fn send(&self, mail: Mail) -> Result<()> {
        tracing::warn!(
            to = %mail.to, subject = %mail.subject,
            "no mail service configured (RESEND_API_KEY); message follows\n{}", mail.text
        );
        Ok(())
    }
    fn delivers(&self) -> bool {
        false
    }
}

/// Keeps every message in memory. For tests.
#[derive(Default)]
pub struct CaptureMailer(pub Mutex<Vec<Mail>>);

impl CaptureMailer {
    pub fn sent(&self) -> Vec<Mail> {
        self.0.lock().expect("mail lock poisoned").clone()
    }

    /// Wait until at least `n` messages were sent (they go out from a background task).
    pub async fn wait_for(&self, n: usize) -> Vec<Mail> {
        for _ in 0..200 {
            let sent = self.sent();
            if sent.len() >= n {
                return sent;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("expected {n} mails, got {}", self.sent().len());
    }
}

#[async_trait]
impl Mailer for CaptureMailer {
    async fn send(&self, mail: Mail) -> Result<()> {
        self.0.lock().expect("mail lock poisoned").push(mail);
        Ok(())
    }
}

const RESEND_URL: &str = "https://api.resend.com/emails";

/// Sends through Resend (<https://resend.com/docs/api-reference/emails/send-email>).
pub struct ResendMailer {
    api_key: String,
    from: String,
    http: openidconnect::reqwest::Client,
}

impl ResendMailer {
    /// `from`: a sender on a domain verified in Resend, e.g. `Rconn <login@rconn.app>`.
    pub fn new(api_key: String, from: String) -> Result<Self> {
        let http = openidconnect::reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(15))
            .redirect(openidconnect::reqwest::redirect::Policy::none())
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            api_key,
            from,
            http,
        })
    }
}

impl std::fmt::Debug for ResendMailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResendMailer")
            .field("from", &self.from)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Mailer for ResendMailer {
    async fn send(&self, mail: Mail) -> Result<()> {
        let body = serde_json::json!({
            "from": self.from,
            "to": [mail.to],
            "subject": mail.subject,
            "text": mail.text,
        });
        let resp = self
            .http
            .post(RESEND_URL)
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .body(serde_json::to_vec(&body)?)
            .send()
            .await
            .context("sending mail through Resend")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Resend answered {status}: {}",
                text.chars().take(300).collect::<String>()
            );
        }
        Ok(())
    }
}

/// The mailer from the environment: Resend with `RESEND_API_KEY` (sender from
/// `mail_from`), else the log.
pub fn from_env(mail_from: Option<&str>) -> Result<std::sync::Arc<dyn Mailer>> {
    match std::env::var("RESEND_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
    {
        Some(key) => {
            let from = mail_from.context(
                "RESEND_API_KEY is set but no sender: pass --mail-from (RCONN_MAIL_FROM)",
            )?;
            tracing::info!(%from, "mail goes out through Resend");
            Ok(std::sync::Arc::new(ResendMailer::new(
                key.trim().to_owned(),
                from.to_owned(),
            )?))
        }
        None => {
            tracing::warn!("RESEND_API_KEY not set: sign-in links are written to the log");
            Ok(std::sync::Arc::new(LogMailer))
        }
    }
}
