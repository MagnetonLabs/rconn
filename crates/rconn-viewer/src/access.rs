//! Connecting with a computer's ID and access password (no account): the viewer's side of
//! the check in `rconn_core::guest`.

use rconn_core::guest::ViewerAnswer;
use rconn_core::ids::SessionId;
use rconn_core::protocol::{DataMessage, GuestMessage, GuestRefusal};
use serde::{Deserialize, Serialize};

/// What the user typed to connect without an account.
#[derive(Clone, Serialize, Deserialize)]
pub struct GuestLogin {
    /// The computer's ID as typed ("067 425 387").
    pub id: String,
    pub password: String,
}

/// Environment variable carrying the access password to `rconn-viewer` (never argv, which
/// other local users can read).
pub const PASSWORD_ENV: &str = "RCONN_ACCESS_PASSWORD";

/// What to do after a message from the host.
pub enum Step {
    Send(DataMessage),
    /// The password was right and the computer proved it knows it too.
    Accepted,
    Refused(String),
}

/// One session's password exchange.
pub struct Exchange {
    password: String,
    session: SessionId,
    answer: Option<ViewerAnswer>,
}

impl Exchange {
    pub fn new(password: String, session: SessionId) -> Self {
        Self {
            password,
            session,
            answer: None,
        }
    }

    pub async fn handle(&mut self, msg: GuestMessage) -> Step {
        match msg {
            GuestMessage::Challenge { salt, msg } => {
                // Deliberately slow (Argon2id, ~50 ms): off the async threads.
                let (password, session) = (self.password.clone(), self.session);
                let answer = tokio::task::spawn_blocking(move || {
                    ViewerAnswer::new(&password, &salt, &msg, session)
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
            GuestMessage::Accepted { confirm } => match &self.answer {
                Some(a) if a.host_is_genuine(&confirm) => Step::Accepted,
                // Something accepted without knowing the password: not the real computer.
                _ => Step::Refused(
                    "the computer couldn't prove it knows the access password; not connecting"
                        .into(),
                ),
            },
            GuestMessage::Refused(why) => Step::Refused(refusal_text(why)),
            GuestMessage::Proof { .. } => {
                Step::Refused("unexpected message from the computer".into())
            }
        }
    }
}

pub fn refusal_text(why: GuestRefusal) -> String {
    match why {
        GuestRefusal::WrongPassword => "wrong access password".into(),
        GuestRefusal::NotEnabled => {
            "this computer has no access password set (Host settings → Access password)".into()
        }
        GuestRefusal::TooManyAttempts { retry_after_secs } => {
            format!("too many wrong passwords; try again in {retry_after_secs} s")
        }
    }
}
