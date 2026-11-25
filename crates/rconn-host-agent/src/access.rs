//! Access-password sessions: someone connecting with this computer's ID and its access
//! password, without an account. Nothing is shown, played or accepted until the password
//! checks out (see `rconn_core::guest` for the exchange itself).

use rconn_core::guest::HostCheck;
use rconn_core::ids::SessionId;
use rconn_core::protocol::{GuestMessage, GuestRefusal};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// Wrong passwords allowed per [`LOCKOUT`] before every attempt is refused until it passes.
const MAX_FAILURES: usize = 5;
const LOCKOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Waiting,
    Open,
    Refused,
}

/// Whether this session may see and control the computer yet.
pub struct Gate {
    state: Mutex<State>,
    changed: Notify,
}

impl Gate {
    /// Sessions from signed-in operators start open; guest sessions wait for the password.
    pub fn new(guest: bool) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(if guest { State::Waiting } else { State::Open }),
            changed: Notify::new(),
        })
    }

    fn get(&self) -> State {
        *self.state.lock().expect("gate")
    }

    fn set(&self, s: State) {
        *self.state.lock().expect("gate") = s;
        self.changed.notify_waiters();
    }

    pub fn is_open(&self) -> bool {
        self.get() == State::Open
    }

    pub fn needs_password(&self) -> bool {
        self.get() == State::Waiting
    }

    /// Wait (up to `limit`) for the password check. `true` once the session may start.
    pub async fn wait(&self, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let changed = self.changed.notified();
            match self.get() {
                State::Open => return true,
                State::Refused => return false,
                State::Waiting => {}
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                self.set(State::Refused);
                return false;
            }
        }
    }
}

/// Recent wrong passwords, for every session on this computer.
static FAILURES: Mutex<Vec<Instant>> = Mutex::new(Vec::new());

/// Seconds until attempts are accepted again, while locked out.
fn locked_for() -> Option<u32> {
    let mut f = FAILURES.lock().expect("failures");
    f.retain(|t| t.elapsed() < LOCKOUT);
    (f.len() >= MAX_FAILURES).then(|| {
        let oldest = f.iter().min().copied().unwrap_or_else(Instant::now);
        LOCKOUT.saturating_sub(oldest.elapsed()).as_secs().max(1) as u32
    })
}

fn record_failure() {
    FAILURES.lock().expect("failures").push(Instant::now());
}

/// The host's first message to a guest: a challenge, or why there won't be one.
pub fn open(gate: &Gate, verifier: &str, session: SessionId) -> (Option<HostCheck>, GuestMessage) {
    let refuse = |why: GuestRefusal| {
        tracing::warn!(%session, ?why, "access-password session refused");
        gate.set(State::Refused);
        (None, GuestMessage::Refused(why))
    };
    if let Some(retry_after_secs) = locked_for() {
        return refuse(GuestRefusal::TooManyAttempts { retry_after_secs });
    }
    if verifier.trim().is_empty() {
        return refuse(GuestRefusal::NotEnabled);
    }
    match HostCheck::start(verifier, session) {
        Ok((check, ch)) => (
            Some(check),
            GuestMessage::Challenge {
                salt: ch.salt,
                msg: ch.msg,
            },
        ),
        Err(e) => {
            tracing::warn!(error = %e, "stored access password unusable");
            refuse(GuestRefusal::NotEnabled)
        }
    }
}

/// Check the guest's proof; opens or refuses the gate.
pub fn answer(gate: &Gate, check: HostCheck, msg: &[u8], confirm: &[u8]) -> GuestMessage {
    match check.finish(msg, confirm) {
        Ok(host_confirm) => {
            tracing::info!("access password accepted; session starting");
            gate.set(State::Open);
            GuestMessage::Accepted {
                confirm: host_confirm,
            }
        }
        Err(e) => {
            record_failure();
            tracing::warn!(error = %e, "access password refused");
            gate.set(State::Refused);
            GuestMessage::Refused(GuestRefusal::WrongPassword)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rconn_core::guest::{ViewerAnswer, make_verifier};

    fn try_password(verifier: &str, typed: &str) -> (Arc<Gate>, GuestMessage) {
        let gate = Gate::new(true);
        let session = SessionId::new();
        let (check, first) = open(&gate, verifier, session);
        let GuestMessage::Challenge { salt, msg } = first else {
            return (gate, first);
        };
        let a = ViewerAnswer::new(typed, &salt, &msg, session).unwrap();
        let reply = answer(&gate, check.unwrap(), &a.msg, &a.confirm);
        (gate, reply)
    }

    // One test: the lockout is shared by every session in the process.
    #[tokio::test]
    async fn password_opens_the_gate_and_repeated_failures_lock_out() {
        assert!(
            Gate::new(false).is_open(),
            "signed-in operators need no password"
        );

        let v = make_verifier("correct horse").unwrap();
        let (gate, reply) = try_password(&v, "correct horse");
        assert!(matches!(reply, GuestMessage::Accepted { .. }));
        assert!(gate.wait(Duration::from_millis(10)).await);

        let (_, reply) = try_password("", "anything");
        assert_eq!(reply, GuestMessage::Refused(GuestRefusal::NotEnabled));

        for _ in 0..MAX_FAILURES {
            let (gate, reply) = try_password(&v, "wrong guess");
            assert_eq!(reply, GuestMessage::Refused(GuestRefusal::WrongPassword));
            assert!(!gate.wait(Duration::from_millis(10)).await);
        }
        // Locked out now, even with the right password.
        let (_, reply) = try_password(&v, "correct horse");
        assert!(
            matches!(
                reply,
                GuestMessage::Refused(GuestRefusal::TooManyAttempts { .. })
            ),
            "{reply:?}"
        );
    }

    #[tokio::test]
    async fn no_answer_in_time_refuses() {
        let gate = Gate::new(true);
        assert!(!gate.wait(Duration::from_millis(20)).await);
        assert!(!gate.is_open());
    }
}
