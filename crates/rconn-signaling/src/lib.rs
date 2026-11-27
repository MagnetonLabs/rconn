//! rconn signaling server: authenticates hosts and operators over a WebSocket, brokers
//! sessions between them, and relays SDP/ICE. Media never passes through here.

pub mod accounts;
pub mod audit;
pub mod audit_db;
pub mod auth;
pub mod db;
pub mod fleet_auth;
pub mod hosts;
pub mod import;
pub mod mail;
pub mod oidc;
pub mod portal;
pub mod public;
pub mod reload;
pub mod secrets;
pub mod store;
pub mod turn_relay;

use audit::{AuditEvent, AuditSink, AuthRole, NoAudit};
use auth::{Authenticator, HostIdentity, HostProofInput, OperatorIdentity, OrgAccess};
use axum::Router;
use axum::extract::State;
use axum::extract::connect_info::ConnectInfo;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{Extensions, HeaderMap, HeaderName};
use axum::response::IntoResponse;
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use hosts::{HostFacts, HostInventory, InventoryEvent};
use rconn_core::PROTOCOL_VERSION;
use rconn_core::ids::{HostId, OperatorId, OrgId, SessionId};
use rconn_core::owner::{HostOwner, owner_admits};
use rconn_core::protocol::{
    CHALLENGE_LEN, ClientMessage, EndReason, ErrorCode, SERVER_CAPABILITIES, ServerMessage,
    SignalPayload,
};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify, mpsc};

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
    /// Hosts without an account (public CA), when this server has one.
    public: std::sync::OnceLock<public::PublicHosts>,
    /// Behind a proxy: the header carrying the client's address (its last entry is used).
    client_ip_header: std::sync::OnceLock<HeaderName>,
    /// Where org hosts' online/offline/enrollment are recorded, when configured.
    inventory: std::sync::OnceLock<Arc<dyn HostInventory>>,
}

struct OnlineHost {
    identity: HostIdentity,
    tx: Outbox,
    /// Who may connect without the access password (and sees it in their list).
    owner: Option<HostOwner>,
}

/// A signed-in operator's socket, so removing the person can drop it.
struct OperatorConn {
    identity: OperatorIdentity,
    tx: Outbox,
    /// Ends the socket's read loop.
    kick: Arc<Notify>,
}

#[derive(Default)]
struct Registry {
    hosts: HashMap<HostId, OnlineHost>,
    operators: Vec<OperatorConn>,
    labels: HashMap<HostId, String>,
    sessions: HashMap<SessionId, SessionEntry>,
    /// Recent access-password requests per computer (rate limit).
    guest_requests: HashMap<HostId, Vec<std::time::Instant>>,
    /// Recent presence questions per source address (rate limit).
    presence_requests: HashMap<Option<IpAddr>, Vec<std::time::Instant>>,
}

/// Access-password requests allowed per computer per minute. Each one is a live guess the
/// host checks (and locks out after repeated failures); this caps the attempt rate early.
pub const GUEST_REQUESTS_PER_MINUTE: usize = 10;

/// Presence questions allowed per source address per minute (apps ask about every 15 s).
pub const PRESENCE_REQUESTS_PER_MINUTE: usize = 12;

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
                public: std::sync::OnceLock::new(),
                client_ip_header: std::sync::OnceLock::new(),
                inventory: std::sync::OnceLock::new(),
            }),
        }
    }

    /// Let signed-in operators of `org` turn their own computers into hosts
    /// (the app does this when signed in; computers without an account use the public CA).
    pub fn allow_enrollment(&self, org: rconn_core::ids::OrgId, ca: rconn_core::fleet::FleetCa) {
        let _ = self.inner.enroller.set((org, ca));
    }

    /// Accept hosts certified by this public CA (owned by nobody, unless the registry says
    /// otherwise) and, if its limits allow, enroll new ones without an account. The
    /// authenticator must also trust the CA (`FleetAuthenticator::with_org_ca`).
    pub fn allow_public_hosts(&self, hosts: public::PublicHosts) {
        let _ = self.inner.public.set(hosts);
    }

    /// Read the client's address from this header (e.g. `x-forwarded-for` behind Caddy):
    /// its last entry, the one the proxy itself added. Only set behind a proxy that
    /// always writes it, or clients could pick their own address.
    pub fn trust_client_ip_header(&self, name: HeaderName) {
        let _ = self.inner.client_ip_header.set(name);
    }

    /// Record org hosts (online, offline, enrolled) in this inventory. Computers without an
    /// account are never recorded.
    pub fn record_hosts_in(&self, inventory: Arc<dyn HostInventory>) {
        let _ = self.inner.inventory.set(inventory);
    }

    /// Report to the inventory, unless the host is one without an account. Never blocks.
    fn inventory(&self, event: InventoryEvent) {
        let org = match &event {
            InventoryEvent::Online { org, .. }
            | InventoryEvent::Offline { org, .. }
            | InventoryEvent::Enrolled { org, .. } => *org,
        };
        if self.inner.public.get().is_some_and(|p| p.org_id() == org) {
            return;
        }
        if let Some(inv) = self.inner.inventory.get() {
            inv.record(event);
        }
    }

    /// The owner of a host that just authenticated.
    fn owner_of(&self, h: &HostIdentity) -> Option<HostOwner> {
        match self.inner.public.get() {
            Some(p) if p.org_id() == h.org_id => p.owner(h.host_id),
            _ => Some(HostOwner::Org { org_id: h.org_id }),
        }
    }

    fn audit(&self, event: AuditEvent) {
        self.inner.audit.record(event);
    }

    /// Evict a host now (after revocation): it gets no new sessions and its open sessions
    /// end with `Revoked`. Its next reconnect is refused by the authenticator.
    pub async fn evict_host(&self, host_id: HostId) {
        let mut reg = self.inner.registry.lock().await;
        if let Some(h) = reg.hosts.remove(&host_id) {
            self.audit(AuditEvent::HostOffline {
                org_id: h.identity.org_id,
                host_id,
            });
            self.inventory(InventoryEvent::Offline {
                org: h.identity.org_id,
                host: host_id,
                at: unix_now() as i64,
            });
            let _ = h.tx.send(ServerMessage::Error {
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

    /// A person was removed from `org`: drop their signed-in sockets for it (each gets a
    /// `Forbidden` error first) and end their sessions with `Revoked`. Returns the number
    /// of sockets dropped.
    pub async fn evict_operator(&self, org: OrgId, operator: OperatorId) -> usize {
        let mut reg = self.inner.registry.lock().await;
        let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut reg.operators)
            .into_iter()
            .partition(|c| c.identity.org_id == org && c.identity.operator_id == operator);
        reg.operators = kept;
        for c in &gone {
            let _ = c.tx.send(ServerMessage::Error {
                code: ErrorCode::Forbidden,
                message: "you were removed from this company".into(),
            });
            c.kick.notify_one();
        }
        end_sessions_of(self, &mut reg, org, operator);
        gone.len()
    }

    /// A person's role in `org` no longer allows connecting: end their open sessions there
    /// with `Revoked` (their sockets stay, so they can still list). Returns how many ended.
    pub async fn end_operator_sessions(&self, org: OrgId, operator: OperatorId) -> usize {
        let mut reg = self.inner.registry.lock().await;
        end_sessions_of(self, &mut reg, org, operator)
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

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
) -> impl IntoResponse {
    let source = client_ip(&state, &headers, &extensions);
    ws.max_frame_size(MAX_FRAME_BYTES)
        .max_message_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| handle_socket(socket, state, source))
}

/// The client's address: from the trusted proxy header if configured, else the socket's
/// peer (needs `into_make_service_with_connect_info::<SocketAddr>`).
pub(crate) fn client_ip(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &Extensions,
) -> Option<IpAddr> {
    if let Some(name) = state.inner.client_ip_header.get() {
        // Several headers or a comma list: the proxy appends, so the last entry is its own.
        let last = headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .rfind(|v| !v.is_empty());
        if let Some(ip) = last.and_then(|v| v.parse().ok()) {
            return Some(ip);
        }
    }
    extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(a)| a.ip())
}

/// Unix seconds now.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
        facts: HostFacts,
    },
    Ready(Peer),
}

async fn handle_socket(socket: WebSocket, state: AppState, source: Option<IpAddr>) {
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
    let kick = Arc::new(Notify::new());
    loop {
        let next = if matches!(phase, Phase::Ready(_)) {
            tokio::select! {
                n = stream.next() => n,
                // Evicted (the person was removed): stop reading; cleanup runs below.
                () = kick.notified() => break,
            }
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
        match step(&state, &tx, &kick, phase, msg, source).await {
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
async fn step(
    state: &AppState,
    tx: &Outbox,
    kick: &Arc<Notify>,
    phase: Phase,
    msg: ClientMessage,
    source: Option<IpAddr>,
) -> Next {
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
                os,
                version,
                capabilities,
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
                facts: HostFacts::tidy(
                    label.as_deref(),
                    os.as_deref(),
                    version.as_deref(),
                    &capabilities,
                ),
            })
        }
        (
            Phase::AwaitingProof {
                host_id,
                cert_pem,
                client_nonce,
                nonce,
                facts,
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
                    let owner = state.owner_of(&identity);
                    {
                        let mut reg = state.inner.registry.lock().await;
                        // A host reconnecting replaces its previous socket.
                        reg.hosts.insert(
                            host_id,
                            OnlineHost {
                                identity,
                                tx: tx.clone(),
                                owner,
                            },
                        );
                        match &facts.label {
                            Some(l) => reg.labels.insert(host_id, l.clone()),
                            None => reg.labels.remove(&host_id),
                        };
                    }
                    state.audit(AuditEvent::HostOnline {
                        org_id: identity.org_id,
                        host_id,
                    });
                    state.inventory(InventoryEvent::Online {
                        org: identity.org_id,
                        host: host_id,
                        facts,
                        at: unix_now() as i64,
                    });
                    let _ = tx.send(welcome(identity.org_id, ice_servers));
                    tracing::info!(%host_id, org = %identity.org_id, owned = owner.is_some(), "host online");
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
                // A valid token of someone no longer in the company (removed, disabled).
                Ok(identity) if auth.org_access(identity).await == OrgAccess::None => {
                    state.audit(AuditEvent::AuthFailed {
                        role: AuthRole::Operator,
                        reason: format!("{} is not a member of the org", identity.operator_id),
                    });
                    send_err(
                        tx,
                        ErrorCode::Unauthenticated,
                        "you are not a member of this company".into(),
                    );
                    Next::Close(Phase::New)
                }
                Ok(identity) => {
                    let ice_servers = auth.ice_servers(identity.org_id).await;
                    state.audit(AuditEvent::OperatorLogin {
                        org_id: identity.org_id,
                        operator_id: identity.operator_id,
                    });
                    state
                        .inner
                        .registry
                        .lock()
                        .await
                        .operators
                        .push(OperatorConn {
                            identity,
                            tx: tx.clone(),
                            kick: kick.clone(),
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
                    let host_id = state.fresh_host_id().await;
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
                    state.inventory(InventoryEvent::Enrolled {
                        org: op.org_id,
                        host: host_id,
                        by: Some(op.operator_id),
                        at: unix_now() as i64,
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
            let access = auth.org_access(op).await;
            let reg = state.inner.registry.lock().await;
            // Only hosts this operator owns: never anyone's account-less computer.
            let mut hosts: Vec<_> = reg
                .hosts
                .values()
                .filter(|h| reaches(h.owner, op, access) != Reach::No)
                .map(|h| rconn_core::protocol::HostInfo {
                    host_id: h.identity.host_id,
                    label: reg.labels.get(&h.identity.host_id).cloned(),
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
            ClientMessage::EnrollAnonymous {
                protocol_version,
                public_key_der,
            },
        ) => {
            if check_version(tx, protocol_version) {
                enroll_anonymous(state, tx, source, &public_key_der).await;
            }
            Next::Close(Phase::New)
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
            Phase::New,
            ClientMessage::Presence {
                protocol_version,
                ids,
            },
        ) => {
            if check_version(tx, protocol_version) {
                presence(state, tx, source, ids).await;
            }
            Next::Close(Phase::New)
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
    let mut found = reg
        .hosts
        .values()
        .map(|h| h.identity)
        .filter(|h| h.host_id.matches(typed));
    let (Some(host), None) = (found.next(), found.next()) else {
        // Two computers sharing a short code (org hosts aren't checked for uniqueness):
        // refuse rather than guess, so a password is never tried on the wrong computer.
        return refuse("unknown, offline or ambiguous ID");
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

/// Which of `ids` are online. An ID counts only when exactly one online computer has it
/// (as for [`guest_hello`]); unknown and offline answer the same.
async fn presence(state: &AppState, tx: &Outbox, source: Option<IpAddr>, ids: Vec<String>) {
    if ids.len() > rconn_core::protocol::MAX_PRESENCE_IDS {
        return send_err(tx, ErrorCode::BadRequest, "too many IDs".into());
    }
    let mut reg = state.inner.registry.lock().await;
    let now = std::time::Instant::now();
    let fresh = |t: &std::time::Instant| now.duration_since(*t) < Duration::from_secs(60);
    if reg.presence_requests.len() > 10_000 {
        reg.presence_requests.retain(|_, v| v.iter().any(fresh));
    }
    let recent = reg.presence_requests.entry(source).or_default();
    recent.retain(fresh);
    if recent.len() >= PRESENCE_REQUESTS_PER_MINUTE {
        drop(reg);
        return send_err(
            tx,
            ErrorCode::BadRequest,
            "too many questions; try again in a minute".into(),
        );
    }
    recent.push(now);
    let online = ids
        .into_iter()
        .filter(|typed| {
            reg.hosts
                .values()
                .filter(|h| h.identity.host_id.matches(typed))
                .take(2)
                .count()
                == 1
        })
        .collect();
    drop(reg);
    let _ = tx.send(ServerMessage::Presence { online });
}

/// A computer without an account asks for an ID.
async fn enroll_anonymous(
    state: &AppState,
    tx: &Outbox,
    source: Option<IpAddr>,
    public_key_der: &[u8],
) {
    let Some(public) = state.inner.public.get() else {
        state.audit(AuditEvent::AnonymousEnrollRefused {
            source,
            reason: "disabled".into(),
        });
        return send_err(
            tx,
            ErrorCode::Forbidden,
            public::Refusal::Disabled.message().into(),
        );
    };
    let online: std::collections::HashSet<u32> = state
        .inner
        .registry
        .lock()
        .await
        .hosts
        .keys()
        .map(|h| public::short_code_number(*h))
        .collect();
    match public.enroll(source, public_key_der, |c| online.contains(&c)) {
        Ok((host_id, cert_pem)) => {
            state.audit(AuditEvent::AnonymousEnrolled {
                org_id: public.org_id(),
                host_id,
                source,
            });
            tracing::info!(%host_id, ?source, "computer enrolled without an account");
            let _ = tx.send(ServerMessage::HostEnrolled { host_id, cert_pem });
        }
        Err(why) => {
            state.audit(AuditEvent::AnonymousEnrollRefused {
                source,
                reason: format!("{why:?}"),
            });
            let code = match why {
                public::Refusal::BadKey => ErrorCode::BadRequest,
                public::Refusal::Internal => ErrorCode::Internal,
                _ => ErrorCode::Forbidden,
            };
            send_err(tx, code, why.message().into());
        }
    }
}

impl AppState {
    /// A new host id whose short code no known host uses.
    async fn fresh_host_id(&self) -> HostId {
        let reg = self.inner.registry.lock().await;
        loop {
            let id = HostId::new();
            let code = public::short_code_number(id);
            let taken = self.inner.public.get().is_some_and(|p| p.code_taken(code))
                || reg
                    .hosts
                    .keys()
                    .any(|h| public::short_code_number(*h) == code);
            if !taken {
                return id;
            }
        }
    }
}

/// How far an operator gets with a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// Not in their list; requests answer "not found".
    No,
    /// In their list, but their role doesn't allow connecting.
    See,
    /// In their list and they may connect without the access password.
    Connect,
}

/// Org computers follow the operator's role in the org (asked on every request); a
/// computer one account claimed is that account's, whatever its org role.
fn reaches(owner: Option<HostOwner>, op: OperatorIdentity, access: OrgAccess) -> Reach {
    if !owner_admits(owner, op.org_id, op.operator_id) {
        return Reach::No;
    }
    match (owner, access) {
        (Some(HostOwner::Operator { .. }), _) | (_, OrgAccess::Connect) => Reach::Connect,
        (_, OrgAccess::List) => Reach::See,
        (_, OrgAccess::None) => Reach::No,
    }
}

async fn request_session(
    state: &AppState,
    tx: &Outbox,
    op: OperatorIdentity,
    host_id: HostId,
    guest: bool,
) {
    let access = if guest {
        OrgAccess::None
    } else {
        state.inner.auth.org_access(op).await
    };
    let mut reg = state.inner.registry.lock().await;
    // A session without the access password only for the host's owner; guests (who must
    // know the password) only for the one computer they typed. Same answer for "offline",
    // "other org" and "not yours", so nobody can probe for computers.
    let reach = |h: &OnlineHost| {
        if guest {
            if h.identity.org_id == op.org_id {
                Reach::Connect
            } else {
                Reach::No
            }
        } else {
            reaches(h.owner, op, access)
        }
    };
    if reg.hosts.get(&host_id).map(reach) == Some(Reach::See) {
        drop(reg);
        state.audit(AuditEvent::SessionRefused {
            host_id,
            operator_id: op.operator_id,
            reason: "the operator's role doesn't allow connecting".into(),
        });
        send_err(
            tx,
            ErrorCode::Forbidden,
            "your role in this company doesn't allow connecting to computers".into(),
        );
        return;
    }
    let Some((host, host_tx)) = reg
        .hosts
        .get(&host_id)
        .filter(|h| reach(h) == Reach::Connect)
        .map(|h| (h.identity, h.tx.clone()))
    else {
        if !guest {
            state.audit(AuditEvent::SessionRefused {
                host_id,
                operator_id: op.operator_id,
                reason: "not the host's owner (or offline)".into(),
            });
        }
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

/// End every session `operator` (signed in to `org`) has open, with `Revoked`.
fn end_sessions_of(
    state: &AppState,
    reg: &mut Registry,
    org: OrgId,
    operator: OperatorId,
) -> usize {
    let gone: Vec<SessionId> = reg
        .sessions
        .iter()
        .filter(|(_, e)| e.operator.org_id == org && e.operator.operator_id == operator)
        .map(|(id, _)| *id)
        .collect();
    for id in &gone {
        if let Some(e) = reg.sessions.remove(id) {
            notify_ended(state, &e, *id, EndReason::Revoked);
        }
    }
    gone.len()
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
            .is_some_and(|o| o.tx.same_channel(tx))
        {
            reg.hosts.remove(&h.host_id);
            state.audit(AuditEvent::HostOffline {
                org_id: h.org_id,
                host_id: h.host_id,
            });
            state.inventory(InventoryEvent::Offline {
                org: h.org_id,
                host: h.host_id,
                at: unix_now() as i64,
            });
            tracing::info!(host_id = %h.host_id, "host offline");
        }
    }
    if let Peer::Operator(_) = peer {
        reg.operators.retain(|c| !c.tx.same_channel(tx));
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
