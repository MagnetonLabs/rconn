//! rconn signaling server: authenticates hosts and operators over a WebSocket, brokers
//! sessions between them, and relays SDP/ICE. Media never passes through here.

pub mod accounts;
pub mod audit;
pub mod auth;
pub mod fleet_auth;
pub mod oidc;
pub mod reload;
pub mod store;
pub mod turn_relay;

use audit::{AuditEvent, AuditSink, AuthRole, NoAudit};
use auth::{Authenticator, HostIdentity, HostProofInput, OperatorIdentity};
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use rconn_core::PROTOCOL_VERSION;
use rconn_core::ids::{HostId, SessionId};
use rconn_core::protocol::{
    CHALLENGE_LEN, ClientMessage, EndReason, ErrorCode, SERVER_CAPABILITIES, ServerMessage,
    SignalPayload,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};

type Outbox = mpsc::UnboundedSender<ServerMessage>;

/// Largest signaling frame accepted. SDP is a few KB; this leaves room without letting
/// one client make the server buffer megabytes.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// A connection must finish authenticating within this long, or it is dropped.
pub const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    auth: Arc<dyn Authenticator>,
    audit: Arc<dyn AuditSink>,
    registry: Mutex<Registry>,
    /// The org's fleet CA, when this server may enroll hosts itself.
    enroller: std::sync::OnceLock<(rconn_core::ids::OrgId, rconn_core::fleet::FleetCa)>,
}

#[derive(Default)]
struct Registry {
    hosts: HashMap<HostId, (HostIdentity, Outbox)>,
    labels: HashMap<HostId, String>,
    sessions: HashMap<SessionId, SessionEntry>,
    /// Recent access-password requests per computer (rate limit).
    guest_requests: HashMap<HostId, Vec<std::time::Instant>>,
}

/// Access-password requests allowed per computer per minute. Each one is a live guess the
/// host checks (and locks out after repeated failures); this caps the attempt rate early.
pub const GUEST_REQUESTS_PER_MINUTE: usize = 10;

struct SessionEntry {
    host: HostIdentity,
    host_tx: Outbox,
    operator: OperatorIdentity,
    operator_tx: Outbox,
}

impl AppState {
    pub fn new(auth: Arc<dyn Authenticator>) -> Self {
        Self::with_audit(auth, Arc::new(NoAudit))
    }

    pub fn with_audit(auth: Arc<dyn Authenticator>, audit: Arc<dyn AuditSink>) -> Self {
        Self {
            inner: Arc::new(Inner {
                auth,
                audit,
                registry: Mutex::default(),
                enroller: std::sync::OnceLock::new(),
            }),
        }
    }

    /// Let signed-in operators of `org` turn their own computers into hosts
    /// ("Allow connections to this computer" in the app).
    pub fn allow_enrollment(&self, org: rconn_core::ids::OrgId, ca: rconn_core::fleet::FleetCa) {
        let _ = self.inner.enroller.set((org, ca));
    }

    fn audit(&self, event: AuditEvent) {
        self.inner.audit.record(event);
    }

    /// Evict a host now (after revocation): it gets no new sessions and its open sessions
    /// end with `Revoked`. Its next reconnect is refused by the authenticator.
    pub async fn evict_host(&self, host_id: HostId) {
        let mut reg = self.inner.registry.lock().await;
        if let Some((h, tx)) = reg.hosts.remove(&host_id) {
            self.audit(AuditEvent::HostOffline {
                org_id: h.org_id,
                host_id,
            });
            let _ = tx.send(ServerMessage::Error {
                code: ErrorCode::Forbidden,
                message: "host revoked".into(),
            });
        }
        let gone: Vec<SessionId> = reg
            .sessions
            .iter()
            .filter(|(_, e)| e.host.host_id == host_id)
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            if let Some(e) = reg.sessions.remove(&id) {
                notify_ended(self, &e, id, EndReason::Revoked);
            }
        }
    }

    /// Number of hosts currently online. For health checks and tests.
    pub async fn online_hosts(&self) -> usize {
        self.inner.registry.lock().await.hosts.len()
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/ws", get(ws_upgrade))
        .with_state(state)
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.max_frame_size(MAX_FRAME_BYTES)
        .max_message_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| handle_socket(socket, state))
}

/// Who is on the other end of a socket, once authenticated.
#[derive(Clone, Copy)]
enum Peer {
    Host(HostIdentity),
    Operator(OperatorIdentity),
    /// Connecting with a computer's ID and access password: may open sessions to that
    /// one computer only, and the host checks the password before anything else.
    Guest {
        op: OperatorIdentity,
        host_id: HostId,
    },
}

enum Phase {
    New,
    AwaitingProof {
        host_id: HostId,
        cert_pem: String,
        client_nonce: Vec<u8>,
        nonce: [u8; CHALLENGE_LEN],
        label: Option<String>,
    },
    Ready(Peer),
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();

    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let Ok(text) = msg.to_json() else { continue };
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let mut phase = Phase::New;
    let auth_deadline = tokio::time::Instant::now() + AUTH_TIMEOUT;
    loop {
        let next = if matches!(phase, Phase::Ready(_)) {
            stream.next().await
        } else {
            match tokio::time::timeout_at(auth_deadline, stream.next()).await {
                Ok(n) => n,
                Err(_) => {
                    let role = if matches!(phase, Phase::AwaitingProof { .. }) {
                        AuthRole::Host
                    } else {
                        AuthRole::Operator
                    };
                    state.audit(AuditEvent::AuthFailed {
                        role,
                        reason: "authentication timed out".into(),
                    });
                    send_err(
                        &tx,
                        ErrorCode::Unauthenticated,
                        "authentication timed out".into(),
                    );
                    break;
                }
            }
        };
        let Some(Ok(frame)) = next else { break };
        let text = match frame {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let msg = match ClientMessage::from_json(&text) {
            Ok(m) => m,
            Err(e) => {
                send_err(
                    &tx,
                    ErrorCode::BadRequest,
                    format!("unparseable message: {e}"),
                );
                continue;
            }
        };
        match step(&state, &tx, phase, msg).await {
            Next::Continue(next) => phase = next,
            // Fatal (auth failure, protocol violation): keep the phase so cleanup still runs.
            Next::Close(last) => {
                phase = last;
                break;
            }
        }
    }

    if let Phase::Ready(peer) = phase {
        disconnect(&state, peer, &tx).await;
    }
    drop(tx);
    let _ = writer.await;
}

enum Next {
    Continue(Phase),
    Close(Phase),
}

/// Advance one connection by one message.
async fn step(state: &AppState, tx: &Outbox, phase: Phase, msg: ClientMessage) -> Next {
    let auth = &state.inner.auth;
    match (phase, msg) {
        (
            Phase::New,
            ClientMessage::HostHello {
                protocol_version,
                host_id,
                cert_pem,
                client_nonce,
                label,
            },
        ) => {
            if !check_version(tx, protocol_version) {
                return Next::Close(Phase::New);
            }
            if client_nonce.len() != CHALLENGE_LEN {
                send_err(
                    tx,
                    ErrorCode::BadRequest,
                    format!("client_nonce must be {CHALLENGE_LEN} bytes"),
                );
                return Next::Close(Phase::New);
            }
            let nonce: [u8; CHALLENGE_LEN] = rand::random();
            let _ = tx.send(ServerMessage::Challenge {
                nonce: nonce.to_vec(),
            });
            Next::Continue(Phase::AwaitingProof {
                host_id,
                cert_pem,
                client_nonce,
                nonce,
                label,
            })
        }
        (
            Phase::AwaitingProof {
                host_id,
                cert_pem,
                client_nonce,
                nonce,
                label,
            },
            ClientMessage::HostProof { signature },
        ) => {
            // The nonce lives only in this connection's state and is consumed here: one try.
            let proof = HostProofInput {
                host_id,
                cert_pem: &cert_pem,
                client_nonce: &client_nonce,
                server_nonce: &nonce,
                signature: &signature,
            };
            match auth.verify_host(proof).await {
                Ok(identity) => {
                    let ice_servers = auth.ice_servers(identity.org_id).await;
                    // A host reconnecting replaces its previous socket.
                    state
                        .inner
                        .registry
                        .lock()
                        .await
                        .hosts
                        .insert(host_id, (identity, tx.clone()));
                    {
                        let mut reg = state.inner.registry.lock().await;
                        match label.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
                            Some(l) => reg.labels.insert(host_id, l.chars().take(64).collect()),
                            None => reg.labels.remove(&host_id),
                        };
                    }
                    state.audit(AuditEvent::HostOnline {
                        org_id: identity.org_id,
                        host_id,
                    });
                    let _ = tx.send(welcome(identity.org_id, ice_servers));
                    tracing::info!(%host_id, org = %identity.org_id, "host online");
                    Next::Continue(Phase::Ready(Peer::Host(identity)))
                }
                Err(e) => {
                    state.audit(AuditEvent::AuthFailed {
                        role: AuthRole::Host,
                        reason: format!("{host_id}: {e}"),
                    });
                    send_err(tx, ErrorCode::Unauthenticated, e.to_string());
                    Next::Close(Phase::New)
                }
            }
        }
        (
            Phase::New,
            ClientMessage::OperatorHello {
                protocol_version,
                token,
            },
        ) => {
            if !check_version(tx, protocol_version) {
                return Next::Close(Phase::New);
            }
            match auth.verify_operator(&token).await {
                Ok(identity) => {
                    let ice_servers = auth.ice_servers(identity.org_id).await;
                    state.audit(AuditEvent::OperatorLogin {
                        org_id: identity.org_id,
                        operator_id: identity.operator_id,
                    });
                    let _ = tx.send(welcome(identity.org_id, ice_servers));
                    Next::Continue(Phase::Ready(Peer::Operator(identity)))
                }
                Err(e) => {
                    state.audit(AuditEvent::AuthFailed {
                        role: AuthRole::Operator,
                        reason: e.to_string(),
                    });
                    send_err(tx, ErrorCode::Unauthenticated, e.to_string());
                    Next::Close(Phase::New)
                }
            }
        }
        (
            Phase::New,
            ClientMessage::PasswordLogin {
                protocol_version,
                username,
                password,
            },
        ) => {
            if !check_version(tx, protocol_version) {
                return Next::Close(Phase::New);
            }
            // One attempt per connection: brute force has to pay a TLS handshake each try.
            match auth.password_login(&username, &password).await {
                Ok((who, token, expires_at)) => {
                    state.audit(AuditEvent::OperatorLogin {
                        org_id: who.org_id,
                        operator_id: who.operator_id,
                    });
                    let _ = tx.send(ServerMessage::LoginToken { token, expires_at });
                }
                Err(e) => {
                    state.audit(AuditEvent::AuthFailed {
                        role: AuthRole::Operator,
                        reason: format!("password login for {username:?}: {e}"),
                    });
                    send_err(
                        tx,
                        ErrorCode::Unauthenticated,
                        "invalid username or password".into(),
                    );
                }
            }
            Next::Close(Phase::New)
        }
        (Phase::New, ClientMessage::GetLoginOptions { protocol_version }) => {
            if check_version(tx, protocol_version) {
                let (password, oidc) = auth.login_options();
                let _ = tx.send(ServerMessage::LoginOptions { password, oidc });
            }
            Next::Close(Phase::New)
        }
        (
            Phase::New,
            ClientMessage::OidcLogin {
                protocol_version,
                id_token,
            },
        ) => {
            if !check_version(tx, protocol_version) {
                return Next::Close(Phase::New);
            }
            match auth.oidc_login(&id_token).await {
                Ok((who, token, expires_at)) => {
                    state.audit(AuditEvent::OperatorLogin {
                        org_id: who.org_id,
                        operator_id: who.operator_id,
                    });
                    let _ = tx.send(ServerMessage::LoginToken { token, expires_at });
                }
                Err(e) => {
                    state.audit(AuditEvent::AuthFailed {
                        role: AuthRole::Operator,
                        reason: format!("oidc login: {e}"),
                    });
                    send_err(
                        tx,
                        ErrorCode::Unauthenticated,
                        "identity provider login was not accepted".into(),
                    );
                }
            }
            Next::Close(Phase::New)
        }
        (Phase::Ready(Peer::Operator(op)), ClientMessage::EnrollHost { public_key_der }) => {
            let issued = match state.inner.enroller.get() {
                Some((org, ca)) if *org == op.org_id => {
                    let host_id = HostId::new();
                    ca.issue_host_cert_for_public_key(host_id, &public_key_der)
                        .map(|cert| (host_id, cert))
                        .map_err(|_| "not a valid Ed25519 public key")
                }
                _ => Err("this server doesn't enroll hosts from the app"),
            };
            match issued {
                Ok((host_id, cert_pem)) => {
                    state.audit(AuditEvent::HostEnrolled {
                        org_id: op.org_id,
                        host_id,
                        operator_id: op.operator_id,
                    });
                    let _ = tx.send(ServerMessage::HostEnrolled { host_id, cert_pem });
                }
                Err(message) => {
                    let _ = tx.send(ServerMessage::Error {
                        code: ErrorCode::BadRequest,
                        message: message.into(),
                    });
                }
            }
            Next::Continue(Phase::Ready(Peer::Operator(op)))
        }
        (Phase::Ready(Peer::Operator(op)), ClientMessage::ListHosts) => {
            let reg = state.inner.registry.lock().await;
            let mut hosts: Vec<_> = reg
                .hosts
                .values()
                .filter(|(h, _)| h.org_id == op.org_id)
                .map(|(h, _)| rconn_core::protocol::HostInfo {
                    host_id: h.host_id,
                    label: reg.labels.get(&h.host_id).cloned(),
                })
                .collect();
            hosts.sort_by(|a, b| a.label.cmp(&b.label));
            let _ = tx.send(ServerMessage::Hosts { hosts });
            Next::Continue(Phase::Ready(Peer::Operator(op)))
        }
        (Phase::Ready(Peer::Operator(op)), ClientMessage::RequestSession { host_id }) => {
            request_session(state, tx, op, host_id, false).await;
            Next::Continue(Phase::Ready(Peer::Operator(op)))
        }
        (
            Phase::New,
            ClientMessage::GuestHello {
                protocol_version,
                host,
            },
        ) => {
            if !check_version(tx, protocol_version) {
                return Next::Close(Phase::New);
            }
            guest_hello(state, tx, &host).await
        }
        (
            Phase::Ready(Peer::Guest { op, host_id }),
            ClientMessage::RequestSession { host_id: h },
        ) => {
            if h == host_id {
                request_session(state, tx, op, host_id, true).await;
            } else {
                send_err(tx, ErrorCode::NotFound, "host not found or offline".into());
            }
            Next::Continue(Phase::Ready(Peer::Guest { op, host_id }))
        }
        (
            Phase::Ready(peer),
            ClientMessage::Signal {
                session_id,
                payload,
            },
        ) => {
            relay_signal(state, tx, peer, session_id, payload).await;
            Next::Continue(Phase::Ready(peer))
        }
        (Phase::Ready(peer), ClientMessage::EndSession { session_id }) => {
            end_session(state, tx, peer, session_id).await;
            Next::Continue(Phase::Ready(peer))
        }
        (phase, _) => {
            send_err(
                tx,
                ErrorCode::BadRequest,
                "message not valid in this state".into(),
            );
            Next::Close(phase)
        }
    }
}

fn check_version(tx: &Outbox, v: u16) -> bool {
    if v != PROTOCOL_VERSION {
        send_err(
            tx,
            ErrorCode::VersionMismatch,
            format!("server speaks {PROTOCOL_VERSION}, client sent {v}"),
        );
    }
    v == PROTOCOL_VERSION
}

/// Someone typed a computer's ID to connect with its access password: find the computer,
/// apply the rate limit, and admit them for that computer only.
async fn guest_hello(state: &AppState, tx: &Outbox, typed: &str) -> Next {
    let refuse = |reason: &str| {
        state.audit(AuditEvent::GuestRefused {
            reason: reason.to_owned(),
        });
        // One answer for "unknown" and "offline", so IDs can't be probed.
        send_err(
            tx,
            ErrorCode::NotFound,
            "computer not found or offline".into(),
        );
        Next::Close(Phase::New)
    };
    let mut reg = state.inner.registry.lock().await;
    let Some(host) = reg
        .hosts
        .values()
        .map(|(h, _)| *h)
        .find(|h| h.host_id.matches(typed))
    else {
        return refuse("unknown or offline ID");
    };
    let now = std::time::Instant::now();
    let recent = reg.guest_requests.entry(host.host_id).or_default();
    recent.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
    if recent.len() >= GUEST_REQUESTS_PER_MINUTE {
        drop(reg);
        state.audit(AuditEvent::GuestRefused {
            reason: format!("rate limit for {}", host.host_id),
        });
        send_err(
            tx,
            ErrorCode::BadRequest,
            "too many attempts for this computer; try again in a minute".into(),
        );
        return Next::Close(Phase::New);
    }
    recent.push(now);
    drop(reg);

    let op = OperatorIdentity {
        operator_id: rconn_core::ids::OperatorId::new(),
        org_id: host.org_id,
    };
    state.audit(AuditEvent::GuestConnect {
        org_id: host.org_id,
        host_id: host.host_id,
        operator_id: op.operator_id,
    });
    let ice_servers = state.inner.auth.ice_servers(host.org_id).await;
    let _ = tx.send(welcome(host.org_id, ice_servers));
    let _ = tx.send(ServerMessage::GuestHost {
        host_id: host.host_id,
    });
    Next::Continue(Phase::Ready(Peer::Guest {
        op,
        host_id: host.host_id,
    }))
}

async fn request_session(
    state: &AppState,
    tx: &Outbox,
    op: OperatorIdentity,
    host_id: HostId,
    guest: bool,
) {
    let mut reg = state.inner.registry.lock().await;
    // Same answer for "offline" and "other org", so operators can't probe other tenants.
    let Some((host, host_tx)) = reg
        .hosts
        .get(&host_id)
        .filter(|(h, _)| h.org_id == op.org_id)
        .cloned()
    else {
        send_err(tx, ErrorCode::NotFound, "host not found or offline".into());
        return;
    };
    let session_id = SessionId::new();
    reg.sessions.insert(
        session_id,
        SessionEntry {
            host,
            host_tx: host_tx.clone(),
            operator: op,
            operator_tx: tx.clone(),
        },
    );
    drop(reg);

    state.audit(AuditEvent::SessionOpened {
        org_id: op.org_id,
        session_id,
        host_id,
        operator_id: op.operator_id,
    });
    let mut policy = state.inner.auth.session_policy(host).await;
    policy.guest = guest;
    let _ = host_tx.send(ServerMessage::IncomingSession {
        session_id,
        operator_id: op.operator_id,
        policy,
    });
    let _ = tx.send(ServerMessage::SessionReady { session_id });
    tracing::info!(%session_id, %host_id, operator = %op.operator_id, "session opened");
}

/// The outbox of the *other* party, if `peer` is a party to `session`.
fn counterpart(entry: &SessionEntry, peer: Peer) -> Option<&Outbox> {
    match peer {
        Peer::Host(h) if h.host_id == entry.host.host_id => Some(&entry.operator_tx),
        Peer::Operator(o) | Peer::Guest { op: o, .. }
            if o.operator_id == entry.operator.operator_id =>
        {
            Some(&entry.host_tx)
        }
        _ => None,
    }
}

async fn relay_signal(
    state: &AppState,
    tx: &Outbox,
    peer: Peer,
    session_id: SessionId,
    payload: SignalPayload,
) {
    let reg = state.inner.registry.lock().await;
    match reg
        .sessions
        .get(&session_id)
        .and_then(|e| counterpart(e, peer))
    {
        Some(other) => {
            let _ = other.send(ServerMessage::Signal {
                session_id,
                payload,
            });
        }
        None => send_err(tx, ErrorCode::NotFound, "no such session".into()),
    }
}

async fn end_session(state: &AppState, tx: &Outbox, peer: Peer, session_id: SessionId) {
    let mut reg = state.inner.registry.lock().await;
    let is_party = reg
        .sessions
        .get(&session_id)
        .is_some_and(|e| counterpart(e, peer).is_some());
    if !is_party {
        send_err(tx, ErrorCode::NotFound, "no such session".into());
        return;
    }
    if let Some(e) = reg.sessions.remove(&session_id) {
        notify_ended(state, &e, session_id, EndReason::Normal);
    }
}

fn notify_ended(state: &AppState, e: &SessionEntry, session_id: SessionId, reason: EndReason) {
    state.audit(AuditEvent::SessionEnded { session_id, reason });
    let _ = e
        .host_tx
        .send(ServerMessage::SessionEnded { session_id, reason });
    let _ = e
        .operator_tx
        .send(ServerMessage::SessionEnded { session_id, reason });
}

async fn disconnect(state: &AppState, peer: Peer, tx: &Outbox) {
    let mut reg = state.inner.registry.lock().await;
    if let Peer::Host(h) = peer {
        // Only remove the registration if it is still this socket (not a newer reconnect).
        if reg
            .hosts
            .get(&h.host_id)
            .is_some_and(|(_, t)| t.same_channel(tx))
        {
            reg.hosts.remove(&h.host_id);
            state.audit(AuditEvent::HostOffline {
                org_id: h.org_id,
                host_id: h.host_id,
            });
            tracing::info!(host_id = %h.host_id, "host offline");
        }
    }
    let gone: Vec<SessionId> = reg
        .sessions
        .iter()
        .filter(|(_, e)| e.host_tx.same_channel(tx) || e.operator_tx.same_channel(tx))
        .map(|(id, _)| *id)
        .collect();
    for id in gone {
        if let Some(e) = reg.sessions.remove(&id) {
            let reason = if matches!(peer, Peer::Host(_)) {
                EndReason::HostOffline
            } else {
                EndReason::Normal
            };
            notify_ended(state, &e, id, reason);
        }
    }
}

fn welcome(
    org_id: rconn_core::ids::OrgId,
    ice_servers: Vec<rconn_core::protocol::IceServer>,
) -> ServerMessage {
    let capabilities = SERVER_CAPABILITIES
        .iter()
        .map(|c| (*c).to_string())
        .collect();
    ServerMessage::Welcome {
        org_id,
        ice_servers,
        capabilities,
    }
}

fn send_err(tx: &Outbox, code: ErrorCode, message: String) {
    let _ = tx.send(ServerMessage::Error { code, message });
}
