//! Guest sessions: someone connecting with this computer's ID, without an account. With an
//! access password set they prove it (see `rconn_core::guest`); with none, the person at
//! the computer accepts or declines (and, if the setting allows, also when one is set).
//! Nothing is shown, played or accepted until then.

use crate::prompt::Answer;
use rconn_core::guest::{Binding, HostCheck};
use rconn_core::ids::OperatorId;
use rconn_core::owner::HostOwner;
use rconn_core::protocol::{DATA_CHANNEL_LABEL, GuestMessage, GuestRefusal, MAX_VIEWER_NAME};
use rconn_peer::identity::LocalOwner;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// Wrong passwords (and declined requests) allowed per [`LOCKOUT`] before every attempt is
/// refused until it passes.
const MAX_FAILURES: usize = 5;
const LOCKOUT: Duration = Duration::from_secs(60);
/// How long the person at the computer has to answer; no answer declines.
pub const ASK_TIMEOUT: Duration = Duration::from_secs(30);
/// Prompts shown per [`ASK_WINDOW`], at most (one at a time besides).
const MAX_ASKS: usize = 3;
const ASK_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Waiting,
    /// The person at the computer is being asked.
    Asking,
    Open,
    Refused,
}

/// Whether this session may see and control the computer yet.
pub struct Gate {
    state: Mutex<State>,
    changed: Notify,
}

impl Gate {
    /// Sessions from signed-in operators start open; guest sessions wait for the check.
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

    /// Move `from` -> `to`; false (and no change) if the gate is elsewhere by now.
    fn advance(&self, from: State, to: State) -> bool {
        let mut s = self.state.lock().expect("gate");
        if *s != from {
            return false;
        }
        *s = to;
        drop(s);
        self.changed.notify_waiters();
        true
    }

    pub fn is_open(&self) -> bool {
        self.get() == State::Open
    }

    /// The guest check hasn't started yet.
    pub fn needs_check(&self) -> bool {
        self.get() == State::Waiting
    }

    /// Wait (up to `limit`) for the guest check. `true` once the session may start.
    pub async fn wait(&self, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let changed = self.changed.notified();
            match self.get() {
                State::Open => return true,
                State::Refused => return false,
                State::Waiting | State::Asking => {}
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                self.set(State::Refused);
                return false;
            }
        }
    }
}

/// Must this session pass the guest check before anything else? Always for guests, and
/// for a host owned by nobody whatever the server says: only the owner may skip it. (A host
/// enrolled into an org has no owner file; the server checked the operator's org.)
pub fn needs_password(policy_guest: bool, owner: LocalOwner, operator: OperatorId) -> bool {
    let owner_admits = match owner {
        LocalOwner::Org | LocalOwner::Recorded(Some(HostOwner::Org { .. })) => true,
        LocalOwner::Recorded(Some(HostOwner::Operator { operator_id })) => operator_id == operator,
        LocalOwner::Recorded(None) => false,
    };
    policy_guest || !owner_admits
}

/// Who the prompt says is asking, for a signed-in operator of the computer's organization
/// (the host knows only their id).
pub const ORG_VIEWER: &str = "Someone from your organization";

/// What has to happen before a session may see and control the computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checks {
    /// The viewer takes part: it proves the access password or asks the person here.
    pub viewer: bool,
    /// The host asks the person here by itself (a signed-in operator under a policy that
    /// requires approval; their viewer doesn't speak the guest exchange).
    pub host_asks: bool,
    /// The organization's policy requires the person here to accept.
    pub approval: bool,
}

impl Checks {
    /// `password_first`: from [`needs_password`]; `require_approval`: the session policy.
    pub fn new(password_first: bool, require_approval: bool) -> Self {
        Self {
            viewer: password_first,
            host_asks: require_approval && !password_first,
            approval: require_approval,
        }
    }

    /// Nothing is shown or accepted until a check passes.
    pub fn gated(&self) -> bool {
        self.viewer || self.host_asks
    }
}

/// How this computer lets guests in (from its settings).
#[derive(Clone, Default)]
pub struct Policy {
    /// The access password's verifier; empty: none set, the person at the computer decides.
    pub verifier: String,
    /// With a password set, someone without it may still ask the person at the computer.
    pub ask_with_password: bool,
}

/// Where a guest's check stands after the host's first message.
pub enum Pending {
    None,
    /// Waiting for the password proof; `may_ask`: or for a request to ask instead.
    Password {
        check: Box<HostCheck>,
        may_ask: bool,
    },
    /// No password here: waiting for the viewer's request to ask.
    Ask,
}

/// Recent wrong passwords and declined requests, for every session on this computer.
static FAILURES: Mutex<Vec<Instant>> = Mutex::new(Vec::new());
/// When recent prompts were shown.
static ASKS: Mutex<Vec<Instant>> = Mutex::new(Vec::new());
/// A prompt is showing (only one at a time).
static ASKING: AtomicBool = AtomicBool::new(false);

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

fn refuse(gate: &Gate, why: GuestRefusal) -> GuestMessage {
    tracing::warn!(?why, "guest session refused");
    gate.set(State::Refused);
    GuestMessage::Refused(why)
}

/// The host's first message to a guest whose data channel has `label`: a password
/// challenge, "the person here decides", or why neither.
pub fn open(
    gate: &Gate,
    policy: &Policy,
    binding: &Binding,
    label: &str,
) -> (Pending, GuestMessage) {
    if let Some(retry_after_secs) = locked_for() {
        return (
            Pending::None,
            refuse(gate, GuestRefusal::TooManyAttempts { retry_after_secs }),
        );
    }
    if label != DATA_CHANNEL_LABEL {
        // An older viewer: its password proof isn't bound to the connection (a server in
        // the middle could relay it) and it can't ask. "No access password" is the refusal
        // it understands that is least wrong; it needs an update either way.
        tracing::warn!(label, "viewer too old for guest sessions; refusing");
        return (Pending::None, refuse(gate, GuestRefusal::NotEnabled));
    }
    if policy.verifier.trim().is_empty() {
        return (Pending::Ask, GuestMessage::ApprovalNeeded);
    }
    match HostCheck::start(&policy.verifier, binding.clone()) {
        Ok((check, ch)) => (
            Pending::Password {
                check: Box::new(check),
                may_ask: policy.ask_with_password,
            },
            GuestMessage::Challenge {
                salt: ch.salt,
                msg: ch.msg,
            },
        ),
        Err(e) => {
            tracing::warn!(error = %e, "stored access password unusable");
            (Pending::None, refuse(gate, GuestRefusal::PasswordRequired))
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

/// Permission to show one prompt; the next may start once this is dropped.
pub struct AskTicket {
    /// Who is asking, cleaned up for display.
    pub viewer: String,
}

impl Drop for AskTicket {
    fn drop(&mut self) {
        ASKING.store(false, Ordering::SeqCst);
    }
}

/// The viewer's name made safe to show: no control characters, trimmed, capped.
pub fn clean_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let cut: String = cleaned.chars().take(MAX_VIEWER_NAME).collect();
    if cut.is_empty() {
        "Someone".into()
    } else {
        cut
    }
}

/// The guest asked for the person at the computer. `Ok`: show the prompt (and tell the
/// viewer [`GuestMessage::AskingUser`]); `Err`: the refusal to send instead.
pub fn ask(gate: &Gate, pending: Pending, name: &str) -> Result<AskTicket, GuestMessage> {
    match pending {
        Pending::Ask | Pending::Password { may_ask: true, .. } => {}
        Pending::Password { may_ask: false, .. } => {
            return Err(refuse(gate, GuestRefusal::PasswordRequired));
        }
        Pending::None => return Err(refuse(gate, GuestRefusal::PasswordRequired)),
    }
    if let Some(retry_after_secs) = locked_for() {
        return Err(refuse(
            gate,
            GuestRefusal::TooManyAttempts { retry_after_secs },
        ));
    }
    {
        let mut asks = ASKS.lock().expect("asks");
        asks.retain(|t| t.elapsed() < ASK_WINDOW);
        if asks.len() >= MAX_ASKS {
            let oldest = asks.iter().min().copied().unwrap_or_else(Instant::now);
            let retry_after_secs =
                ASK_WINDOW.saturating_sub(oldest.elapsed()).as_secs().max(1) as u32;
            drop(asks);
            return Err(refuse(
                gate,
                GuestRefusal::TooManyAttempts { retry_after_secs },
            ));
        }
        if ASKING.swap(true, Ordering::SeqCst) {
            drop(asks);
            return Err(refuse(gate, GuestRefusal::Busy));
        }
        asks.push(Instant::now());
    }
    if !gate.advance(State::Waiting, State::Asking) {
        ASKING.store(false, Ordering::SeqCst);
        return Err(GuestMessage::Refused(GuestRefusal::NoAnswer));
    }
    let viewer = clean_name(name);
    tracing::info!(%viewer, "asking the person at the computer");
    Ok(AskTicket { viewer })
}

/// What the person at the computer answered: opens or refuses the gate. A session that
/// ended (or timed out) meanwhile stays closed whatever the answer.
pub fn decided(gate: &Gate, ticket: AskTicket, answer: Answer) -> GuestMessage {
    let reply = match answer {
        Answer::Accept if gate.advance(State::Asking, State::Open) => {
            tracing::info!(viewer = %ticket.viewer, "accepted at the computer; session starting");
            GuestMessage::Approved
        }
        Answer::Accept => GuestMessage::Refused(GuestRefusal::NoAnswer),
        Answer::Decline => {
            record_failure();
            refuse(gate, GuestRefusal::Declined)
        }
        Answer::TimedOut => refuse(gate, GuestRefusal::NoAnswer),
        Answer::CannotAsk(why) => {
            tracing::warn!(%why, "couldn't ask the person at the computer");
            refuse(gate, GuestRefusal::CannotAsk)
        }
    };
    drop(ticket);
    reply
}

#[cfg(test)]
mod tests {
    use super::*;
    use rconn_core::guest::{ViewerAnswer, make_verifier};
    use rconn_core::ids::SessionId;
    use rconn_core::protocol::LEGACY_DATA_CHANNEL_LABEL;

    fn binding() -> Binding {
        Binding {
            session: SessionId::new(),
            viewer_dtls: "sha-256 aa".into(),
            host_dtls: "sha-256 bb".into(),
        }
    }

    #[test]
    fn an_approval_policy_gates_every_session() {
        // Signed-in operator, no policy: open at once.
        let c = Checks::new(false, false);
        assert!(!c.gated());
        // Signed-in operator, approval required: the host asks by itself.
        let c = Checks::new(false, true);
        assert!(c.gated() && c.host_asks && !c.viewer);
        // Guest, approval required: the viewer asks (no password shortcut).
        let c = Checks::new(true, true);
        assert!(c.gated() && c.viewer && !c.host_asks && c.approval);
        let c = Checks::new(true, false);
        assert!(c.gated() && c.viewer && !c.host_asks && !c.approval);
    }

    #[test]
    fn the_host_asking_for_an_operator_opens_the_gate_only_on_accept() {
        let gate = Gate::new(Checks::new(false, true).gated());
        assert!(!gate.is_open());
        let Ok(ticket) = ask(&gate, Pending::Ask, ORG_VIEWER) else {
            panic!("not asked")
        };
        assert_eq!(ticket.viewer, ORG_VIEWER);
        decided(&gate, ticket, Answer::Accept);
        assert!(gate.is_open());
    }

    fn policy(verifier: &str, ask_with_password: bool) -> Policy {
        Policy {
            verifier: verifier.into(),
            ask_with_password,
        }
    }

    fn try_password(verifier: &str, typed: &str) -> (Arc<Gate>, GuestMessage) {
        let gate = Gate::new(true);
        let b = binding();
        let (pending, first) = open(&gate, &policy(verifier, false), &b, DATA_CHANNEL_LABEL);
        let (Pending::Password { check, .. }, GuestMessage::Challenge { salt, msg }) =
            (pending, first.clone())
        else {
            return (gate, first);
        };
        let a = ViewerAnswer::new(typed, &salt, &msg, b).unwrap();
        let reply = answer(&gate, *check, &a.msg, &a.confirm);
        (gate, reply)
    }

    fn clear_limits() {
        FAILURES.lock().unwrap().clear();
        ASKS.lock().unwrap().clear();
    }

    /// Ask, then answer with `a`.
    fn ask_and(policy: &Policy, a: Answer) -> (Arc<Gate>, GuestMessage) {
        let gate = Gate::new(true);
        let (pending, _) = open(&gate, policy, &binding(), DATA_CHANNEL_LABEL);
        match ask(&gate, pending, "Ana\non\u{7}LAPTOP") {
            Ok(ticket) => {
                assert_eq!(ticket.viewer, "Ana on LAPTOP");
                let reply = decided(&gate, ticket, a);
                (gate, reply)
            }
            Err(reply) => (gate, reply),
        }
    }

    // One test: the lockout and the prompt limits are shared by every session in the
    // process, so these run in order.
    #[tokio::test]
    async fn passwords_prompts_and_their_limits() {
        clear_limits();
        assert!(
            Gate::new(false).is_open(),
            "signed-in operators need no check"
        );

        // A password opens the gate.
        let v = make_verifier("correct horse").unwrap();
        let (gate, reply) = try_password(&v, "correct horse");
        assert!(matches!(reply, GuestMessage::Accepted { .. }));
        assert!(gate.wait(Duration::from_millis(10)).await);

        // Older viewers are refused (their proof isn't bound to the connection).
        let gate = Gate::new(true);
        let (_, first) = open(
            &gate,
            &policy(&v, false),
            &binding(),
            LEGACY_DATA_CHANNEL_LABEL,
        );
        assert_eq!(first, GuestMessage::Refused(GuestRefusal::NotEnabled));
        assert!(!gate.wait(Duration::from_millis(10)).await);

        // No password: the person at the computer decides.
        let none = policy("", false);
        let gate = Gate::new(true);
        let (_, first) = open(&gate, &none, &binding(), DATA_CHANNEL_LABEL);
        assert_eq!(first, GuestMessage::ApprovalNeeded);
        assert!(!gate.is_open(), "nothing before the answer");
        let (gate, reply) = ask_and(&none, Answer::Accept);
        assert_eq!(reply, GuestMessage::Approved);
        assert!(gate.wait(Duration::from_millis(10)).await);
        let (gate, reply) = ask_and(&none, Answer::TimedOut);
        assert_eq!(reply, GuestMessage::Refused(GuestRefusal::NoAnswer));
        assert!(!gate.wait(Duration::from_millis(10)).await);
        let (_, reply) = ask_and(&none, Answer::TimedOut);
        assert_eq!(reply, GuestMessage::Refused(GuestRefusal::NoAnswer));
        // Three prompts a minute at most.
        let (_, reply) = ask_and(&none, Answer::Accept);
        assert!(
            matches!(
                reply,
                GuestMessage::Refused(GuestRefusal::TooManyAttempts { .. })
            ),
            "{reply:?}"
        );
        clear_limits();

        // A password set: asking only if the setting allows it.
        let (_, reply) = ask_and(&policy(&v, false), Answer::Accept);
        assert_eq!(reply, GuestMessage::Refused(GuestRefusal::PasswordRequired));
        let (gate, reply) = ask_and(&policy(&v, true), Answer::Accept);
        assert_eq!(reply, GuestMessage::Approved);
        assert!(gate.is_open());

        // One prompt at a time.
        let gate = Gate::new(true);
        let (pending, _) = open(&gate, &none, &binding(), DATA_CHANNEL_LABEL);
        let ticket = ask(&gate, pending, "first").ok().unwrap();
        let (_, reply) = ask_and(&none, Answer::Accept);
        assert_eq!(reply, GuestMessage::Refused(GuestRefusal::Busy));
        // A session that timed out meanwhile stays closed even if accepted late.
        assert!(!gate.wait(Duration::from_millis(10)).await);
        assert_eq!(
            decided(&gate, ticket, Answer::Accept),
            GuestMessage::Refused(GuestRefusal::NoAnswer)
        );
        assert!(!gate.is_open());
        clear_limits();

        // Declines count toward the lockout, like wrong passwords.
        for _ in 0..2 {
            let (_, reply) = ask_and(&none, Answer::Decline);
            assert_eq!(reply, GuestMessage::Refused(GuestRefusal::Declined));
        }
        for _ in 0..MAX_FAILURES - 2 {
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
        clear_limits();
    }

    #[test]
    fn a_host_owned_by_nobody_always_needs_the_check() {
        let (op, other) = (OperatorId::new(), OperatorId::new());
        // The server claims a signed-in operator; a host owned by nobody refuses that.
        assert!(needs_password(false, LocalOwner::Recorded(None), op));
        assert!(needs_password(true, LocalOwner::Recorded(None), op));
        // Org hosts: the server vouches for the operator's org.
        assert!(!needs_password(false, LocalOwner::Org, op));
        assert!(needs_password(true, LocalOwner::Org, op));
        // A claimed host: only its account skips the check.
        let mine = LocalOwner::Recorded(Some(HostOwner::Operator { operator_id: op }));
        assert!(!needs_password(false, mine, op));
        assert!(needs_password(false, mine, other));
    }

    #[test]
    fn names_are_cleaned_for_display() {
        assert_eq!(clean_name("  "), "Someone");
        assert_eq!(clean_name("a\r\nb\tc"), "a b c");
        assert_eq!(
            clean_name(&"x".repeat(200)).chars().count(),
            MAX_VIEWER_NAME
        );
    }

    #[tokio::test]
    async fn no_answer_in_time_refuses() {
        let gate = Gate::new(true);
        assert!(!gate.wait(Duration::from_millis(20)).await);
        assert!(!gate.is_open());
    }
}
