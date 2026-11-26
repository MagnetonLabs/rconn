//! The viewer's side of a guest session (connecting with a computer's ID, no account):
//! with its access password (the exchange in `rconn_core::guest`), or, with none typed, by
//! asking the person at the computer to accept. Runs over the data channel; shared by the
//! desktop and mobile viewers.

use crate::session::ViewerSession;
use anyhow::Result;
use rconn_core::guest::{Binding, ViewerAnswer};
use rconn_core::protocol::{DataMessage, GuestMessage, GuestRefusal, MAX_VIEWER_NAME};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// What to do after a message from the host.
#[derive(Debug, PartialEq)]
pub enum Step {
    Send(DataMessage),
    /// Nothing to send; show this while waiting (the person at the computer is deciding).
    Waiting(String),
    /// The session may start: the password was right and the computer proved it knows it
    /// too, or the person at the computer accepted.
    Accepted,
    Refused(String),
}

/// Shown while the person at the computer decides.
pub const WAITING_TEXT: &str = "Waiting for the person at the computer to accept…";

type BindingFuture = Pin<Box<dyn Future<Output = Result<Binding>> + Send>>;

/// One session's guest exchange.
pub struct Exchange {
    password: String,
    name: String,
    /// Gets the connection's binding (once the data channel is open, the offer/answer is
    /// done).
    binding: Arc<dyn Fn() -> BindingFuture + Send + Sync>,
    answer: Option<ViewerAnswer>,
    asked: bool,
}

impl Exchange {
    /// `password` empty: no password, ask the person at the computer instead.
    pub fn new(password: String, session: &ViewerSession) -> Self {
        let pc = session.peer.pc.clone();
        let id = session.session_id;
        Self::with_binding(
            password,
            Arc::new(move || {
                let pc = pc.clone();
                Box::pin(async move {
                    crate::session::binding(&*pc, id, crate::session::Role::Viewer).await
                })
            }),
        )
    }

    /// As [`new`](Self::new), with the binding from elsewhere (tests).
    pub fn with_binding(
        password: String,
        binding: Arc<dyn Fn() -> BindingFuture + Send + Sync>,
    ) -> Self {
        Self {
            password,
            name: default_name(),
            binding,
            answer: None,
            asked: false,
        }
    }

    /// Who is asking, as the prompt on the computer shows it (unverified there).
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = name.into().chars().take(MAX_VIEWER_NAME).collect();
        self
    }

    fn ask(&mut self) -> Step {
        self.asked = true;
        Step::Send(DataMessage::Guest(GuestMessage::AskUser {
            name: self.name.clone(),
        }))
    }

    pub async fn handle(&mut self, msg: GuestMessage) -> Step {
        match msg {
            GuestMessage::Challenge { .. } if self.password.is_empty() => self.ask(),
            GuestMessage::Challenge { salt, msg } => {
                let binding = match (self.binding)().await {
                    Ok(b) => b,
                    Err(e) => {
                        return Step::Refused(format!("can't secure the password check: {e:#}"));
                    }
                };
                // Deliberately slow (Argon2id, ~50 ms): off the async threads.
                let password = self.password.clone();
                let answer = tokio::task::spawn_blocking(move || {
                    ViewerAnswer::new(&password, &salt, &msg, binding)
                })
                .await;
                match answer {
                    Ok(Ok(a)) => {
                        let proof = DataMessage::Guest(GuestMessage::Proof {
                            msg: a.msg.clone(),
                            confirm: a.confirm.clone(),
                        });
                        self.answer = Some(a);
                        Step::Send(proof)
                    }
                    _ => Step::Refused("the computer's password check was malformed".into()),
                }
            }
            // A password was typed: never fall back to just asking. Otherwise a server in
            // the middle could swap the password check (which it can't pass) for a request
            // it relays to the person at the computer.
            GuestMessage::ApprovalNeeded if !self.password.is_empty() => Step::Refused(
                "this computer has no access password. To ask the person at it to let you in, \
                 connect with just its ID (leave the password empty)"
                    .into(),
            ),
            GuestMessage::ApprovalNeeded => self.ask(),
            GuestMessage::AskingUser { .. } if self.asked => Step::Waiting(WAITING_TEXT.into()),
            GuestMessage::Approved if self.asked => Step::Accepted,
            GuestMessage::Accepted { confirm } => match &self.answer {
                Some(a) if a.host_is_genuine(&confirm) => Step::Accepted,
                // Something accepted without knowing the password, or on another
                // connection (a server in the middle): not the real computer.
                _ => Step::Refused(
                    "the computer couldn't prove it knows the access password; not connecting"
                        .into(),
                ),
            },
            GuestMessage::Refused(why) => Step::Refused(refusal_text(why)),
            GuestMessage::Proof { .. }
            | GuestMessage::AskUser { .. }
            | GuestMessage::AskingUser { .. }
            | GuestMessage::Approved => {
                Step::Refused("unexpected message from the computer".into())
            }
        }
    }
}

/// "<user> on <computer>", for the prompt on the computer being connected to.
pub fn default_name() -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    let host = std::env::var("COMPUTERNAME").ok().or_else(|| {
        std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().trim_end_matches(".local").to_owned())
    });
    let name = match (user.trim(), host.as_deref().map(str::trim)) {
        ("", Some(h)) if !h.is_empty() => h.to_owned(),
        (u, Some(h)) if !h.is_empty() => format!("{u} on {h}"),
        ("", _) => "Someone".into(),
        (u, _) => u.to_owned(),
    };
    name.chars().take(MAX_VIEWER_NAME).collect()
}

pub fn refusal_text(why: GuestRefusal) -> String {
    match why {
        GuestRefusal::WrongPassword => "wrong access password".into(),
        GuestRefusal::NotEnabled => {
            "this computer has no access password set, and its Rconn can't ask the person at it (it needs an update)".into()
        }
        GuestRefusal::TooManyAttempts { retry_after_secs } => format!(
            "too many wrong passwords or declined requests; try again in {retry_after_secs} s"
        ),
        GuestRefusal::Declined => "the person at the computer declined".into(),
        GuestRefusal::NoAnswer => "nobody at the computer answered in time".into(),
        GuestRefusal::PasswordRequired => "this computer needs its access password".into(),
        GuestRefusal::Busy => {
            "the computer is already asking about another connection; try again in a minute".into()
        }
        GuestRefusal::CannotAsk => {
            "nobody can accept at the computer right now (no one logged in?); it needs its access password"
                .into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rconn_core::guest::{HostCheck, make_verifier};
    use rconn_core::ids::SessionId;

    fn fixed(b: Binding) -> Arc<dyn Fn() -> BindingFuture + Send + Sync> {
        Arc::new(move || {
            let b = b.clone();
            Box::pin(async move { Ok(b) })
        })
    }

    fn binding(viewer: &str) -> Binding {
        Binding {
            session: SessionId(uuid::Uuid::nil()),
            viewer_dtls: viewer.into(),
            host_dtls: "sha-256 host".into(),
        }
    }

    #[tokio::test]
    async fn without_a_password_the_viewer_asks_and_waits() {
        let b = binding("sha-256 viewer");
        let mut ex = Exchange::with_binding(String::new(), fixed(b.clone())).named("Ana");
        let ask = Step::Send(DataMessage::Guest(GuestMessage::AskUser {
            name: "Ana".into(),
        }));
        assert_eq!(ex.handle(GuestMessage::ApprovalNeeded).await, ask);
        assert_eq!(
            ex.handle(GuestMessage::AskingUser { timeout_secs: 30 })
                .await,
            Step::Waiting(WAITING_TEXT.into())
        );
        assert_eq!(ex.handle(GuestMessage::Approved).await, Step::Accepted);

        // A computer with a password that also lets people ask: no password typed, ask.
        let v = make_verifier("correct horse").unwrap();
        let (_, ch) = HostCheck::start(&v, b.clone()).unwrap();
        let mut ex = Exchange::with_binding(String::new(), fixed(b)).named("Ana");
        let challenge = GuestMessage::Challenge {
            salt: ch.salt,
            msg: ch.msg,
        };
        assert_eq!(ex.handle(challenge).await, ask);
        assert_eq!(
            ex.handle(GuestMessage::Refused(GuestRefusal::Declined))
                .await,
            Step::Refused("the person at the computer declined".into())
        );
    }

    #[tokio::test]
    async fn a_typed_password_never_turns_into_just_asking() {
        let b = binding("sha-256 viewer");
        let mut ex = Exchange::with_binding("correct horse".into(), fixed(b.clone()));
        assert!(matches!(
            ex.handle(GuestMessage::ApprovalNeeded).await,
            Step::Refused(_)
        ));
        let mut ex = Exchange::with_binding("correct horse".into(), fixed(b));
        assert!(
            matches!(ex.handle(GuestMessage::Approved).await, Step::Refused(_)),
            "an approval nobody asked for"
        );
    }

    #[tokio::test]
    async fn the_password_proof_uses_the_connection_binding() {
        let v = make_verifier("correct horse").unwrap();
        let host_view = binding("sha-256 server");
        let (check, ch) = HostCheck::start(&v, host_view).unwrap();
        let mut ex = Exchange::with_binding("correct horse".into(), fixed(binding("sha-256 me")));
        let Step::Send(DataMessage::Guest(GuestMessage::Proof { msg, confirm })) = ex
            .handle(GuestMessage::Challenge {
                salt: ch.salt,
                msg: ch.msg,
            })
            .await
        else {
            panic!("expected a proof")
        };
        assert!(
            check.finish(&msg, &confirm).is_err(),
            "different DTLS views: refused"
        );
    }
}
