//! Wire protocol.
//!
//! * **Signaling** ([`ClientMessage`] / [`ServerMessage`]) is JSON over a WebSocket to the
//!   signaling server. JSON keeps it debuggable and lets non-Rust tooling speak it.
//! * **Data channel** ([`DataMessage`]) is `postcard` over the WebRTC data channel between
//!   viewer and host. Compact, because pointer moves are sent at display refresh rate.

use crate::ids::{HostId, OperatorId, OrgId, SessionId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Upper bound on a clipboard payload. Anything larger is rejected on decode, so a
/// misbehaving peer can't make the other side allocate without limit.
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;

/// Length of each random nonce (client's and server's) in the host auth handshake.
pub const CHALLENGE_LEN: usize = 32;

/// WebRTC data-channel `protocol` string. Viewer and host refuse a channel whose protocol
/// differs, so a [`DataMessage`] schema change bumps this, never edits v1 in place.
pub const DATA_CHANNEL_PROTOCOL: &str = "rconn-data/1";

/// Capabilities this build's server advertises in [`ServerMessage::Welcome`]. Clients
/// only use optional features the server lists; unknown entries are ignored.
pub const SERVER_CAPABILITIES: &[&str] = &["sessions", "ice-relay"];

// ---------------------------------------------------------------------------
// Signaling: client -> server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// First message from a host. `cert_pem` is the host certificate issued by the
    /// org's fleet CA. `client_nonce` is fresh random bytes ([`CHALLENGE_LEN`]).
    /// The server answers with [`ServerMessage::Challenge`].
    HostHello {
        protocol_version: u16,
        host_id: HostId,
        cert_pem: String,
        client_nonce: Vec<u8>,
        /// Human-readable name shown in viewers' computer lists (e.g. the hostname).
        #[serde(default)]
        label: Option<String>,
    },
    /// Signature, with the host cert's private key, over
    /// [`crate::fleet::host_proof_transcript`]. Proves possession at the application layer,
    /// so it works behind proxies that terminate TLS (Cloudflare, cloud load balancers).
    HostProof { signature: Vec<u8> },
    /// First message from a viewer. `token` comes from operator login (accounts,
    /// passkey or OIDC); the socket itself carries no identity.
    OperatorHello {
        protocol_version: u16,
        token: String,
    },
    /// Exchange account credentials for a session token. Only sent over TLS. The server
    /// answers [`ServerMessage::LoginToken`] (or an error) and closes the socket; the
    /// viewer then reconnects with `OperatorHello { token }`.
    PasswordLogin {
        protocol_version: u16,
        username: String,
        password: String,
    },
    /// Ask how operators may log in here. Answered with [`ServerMessage::LoginOptions`].
    GetLoginOptions { protocol_version: u16 },
    /// Exchange an ID token from the server's IdP (obtained by the viewer with PKCE) for a
    /// session token. Answered with [`ServerMessage::LoginToken`] or an error.
    OidcLogin {
        protocol_version: u16,
        id_token: String,
    },
    /// Operator turns the computer it runs on into a host: the server issues an id and a
    /// certificate for this public key (the private key never leaves the computer).
    /// Answered with [`ServerMessage::HostEnrolled`].
    EnrollHost {
        /// Ed25519 SubjectPublicKeyInfo, DER.
        public_key_der: Vec<u8>,
    },
    /// First message from a computer with no account that wants an ID (like AnyDesk): the
    /// server issues an id and a certificate for this public key, owned by nobody. Such a
    /// host only ever accepts sessions that know its access password. Answered with
    /// [`ServerMessage::HostEnrolled`] (or an error), then the socket closes.
    EnrollAnonymous {
        protocol_version: u16,
        /// Ed25519 SubjectPublicKeyInfo, DER.
        public_key_der: Vec<u8>,
    },
    /// Operator asks which of the org's hosts are online. Answered with [`ServerMessage::Hosts`].
    ListHosts,
    /// Operator asks to open a session to a host.
    RequestSession { host_id: HostId },
    /// SDP / ICE relayed to the other side of a session.
    Signal {
        session_id: SessionId,
        payload: SignalPayload,
    },
    /// Either side ends a session.
    EndSession { session_id: SessionId },
    /// First message from someone connecting with a computer's ID and access password
    /// (no account). `host` is the 9-digit ID as typed. The server answers `Welcome`, then
    /// [`ServerMessage::GuestHost`]; the password itself is checked by the host, later, on
    /// the session's encrypted data channel (see [`crate::guest`]).
    GuestHello { protocol_version: u16, host: String },
}

// ---------------------------------------------------------------------------
// Signaling: server -> client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Sent after [`ClientMessage::HostHello`]. Host must answer with `HostProof`.
    Challenge {
        nonce: Vec<u8>,
    },
    /// Authentication succeeded.
    Welcome {
        org_id: OrgId,
        /// STUN/TURN servers with short-lived credentials.
        ice_servers: Vec<IceServer>,
        /// Optional features this server supports. See [`SERVER_CAPABILITIES`].
        #[serde(default)]
        capabilities: Vec<String>,
    },
    /// Answer to `EnrollHost`.
    HostEnrolled {
        host_id: HostId,
        cert_pem: String,
    },
    /// Answer to `ListHosts`: the org's online hosts.
    Hosts {
        hosts: Vec<HostInfo>,
    },
    /// Answer to `GuestHello`: the computer the ID belongs to. The guest may then request
    /// a session with it (and only with it).
    GuestHost {
        host_id: HostId,
    },
    /// Answer to `GetLoginOptions`.
    LoginOptions {
        password: bool,
        oidc: Option<OidcLoginInfo>,
    },
    /// Answer to `PasswordLogin`: a session token and its expiry (Unix seconds).
    LoginToken {
        token: String,
        expires_at: u64,
    },
    /// To a host: an operator wants a session. Carries the effective policy for it.
    IncomingSession {
        session_id: SessionId,
        operator_id: OperatorId,
        policy: SessionPolicy,
    },
    /// To an operator: the host accepted; start the WebRTC offer.
    SessionReady {
        session_id: SessionId,
    },
    Signal {
        session_id: SessionId,
        payload: SignalPayload,
    },
    SessionEnded {
        session_id: SessionId,
        reason: EndReason,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
    /// A message type this build doesn't know (sent by a newer server). Clients ignore
    /// it; anything that changes auth or policy semantics bumps `PROTOCOL_VERSION` instead.
    #[serde(other)]
    Unknown,
}

/// One entry in a viewer's computer list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    pub host_id: HostId,
    pub label: Option<String>,
}

/// Where and as whom the viewer runs the OIDC flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OidcLoginInfo {
    pub issuer: String,
    pub client_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SignalPayload {
    Sdp {
        sdp_type: SdpType,
        sdp: String,
    },
    Ice {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SdpType {
    Offer,
    Answer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

/// Per-session rules the host enforces, resolved by the server from fleet policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPolicy {
    /// Show the app's "session active" indicator on the host.
    pub show_indicator: bool,
    /// Allow two-way text clipboard sync.
    pub clipboard: bool,
    /// The viewer has no account: the host must check its access password before showing
    /// anything or taking input.
    #[serde(default)]
    pub guest: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    Normal,
    HostOffline,
    Revoked,
    Timeout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    VersionMismatch,
    Unauthenticated,
    Forbidden,
    NotFound,
    BadRequest,
    Internal,
}

// ---------------------------------------------------------------------------
// Data channel: viewer <-> host
// ---------------------------------------------------------------------------

/// Frozen for [`DATA_CHANNEL_PROTOCOL`] `rconn-data/1`: postcard encodes variants by
/// index, so variants and fields here (and in nested enums) must never be reordered,
/// removed or inserted. Changes go into a new protocol version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DataMessage {
    Input(InputEvent),
    /// Clipboard text, either direction. Bounded by [`MAX_CLIPBOARD_BYTES`].
    Clipboard(String),
    Control(Control),
    /// Access-password check (see [`crate::guest`]), before a guest session starts.
    Guest(GuestMessage),
}

/// The access-password exchange on the data channel. The host speaks first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GuestMessage {
    /// Host -> viewer: derive with `salt`, answer `msg` (SPAKE2).
    Challenge { salt: Vec<u8>, msg: Vec<u8> },
    /// Viewer -> host.
    Proof { msg: Vec<u8>, confirm: Vec<u8> },
    /// Host -> viewer: the password was right; `confirm` proves the host knows it too.
    Accepted { confirm: Vec<u8> },
    /// Host -> viewer: refused; the session ends.
    Refused(GuestRefusal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GuestRefusal {
    WrongPassword,
    /// This computer has no access password set.
    NotEnabled,
    /// Too many wrong passwords lately; try again later.
    TooManyAttempts {
        retry_after_secs: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    /// Absolute pointer position, normalized to 0.0..=1.0 across the captured display,
    /// so the viewer doesn't need the host's pixel size.
    PointerMove {
        x: f32,
        y: f32,
    },
    PointerButton {
        button: MouseButton,
        pressed: bool,
    },
    /// Scroll delta in "lines"; positive `dy` scrolls content up.
    Wheel {
        dx: f32,
        dy: f32,
    },
    /// Physical key as a USB HID usage (page 0x07), platform-neutral. Each host maps it
    /// to a Windows scan code or a macOS virtual keycode.
    Key {
        hid_usage: u16,
        pressed: bool,
    },
    /// Type this text as keystrokes (layout-independent), e.g. the viewer's clipboard
    /// where pasting isn't possible (login screen). Capped at 4096 characters by the host.
    Text(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Control {
    /// Viewer lost decoder state; host should send an IDR frame next.
    RequestKeyframe,
    /// Round-trip probe; peer echoes `Pong` with the same value.
    Ping(u64),
    Pong(u64),
    /// Host -> viewer: the host's mouse pointer changed shape. The viewer shows it with its
    /// own (instant) pointer, so the pointer never lags behind the video.
    Cursor(CursorShape),
    /// Viewer -> host: a one-off action from the toolbar.
    Action(HostAction),
    /// Viewer -> host: the viewer's picture area in pixels; the host fits its screen to
    /// it (closest display mode) until the session ends.
    Resize {
        width: u32,
        height: u32,
    },
}

/// Toolbar actions the host performs on request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostAction {
    /// Show the OS lock screen.
    LockScreen,
    /// Restart the computer (after a few seconds' notice).
    Restart,
}

/// Standard pointer shapes (custom app cursors show as `Arrow`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CursorShape {
    Arrow,
    Text,
    Hand,
    ResizeHorizontal,
    ResizeVertical,
    ResizeNwSe,
    ResizeNeSw,
    Move,
    Wait,
    Progress,
    NotAllowed,
    Crosshair,
    /// The host hid its pointer (e.g. while typing).
    Hidden,
}

// ---------------------------------------------------------------------------
// Encoding helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("postcard: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("clipboard payload is {0} bytes, limit is {MAX_CLIPBOARD_BYTES}")]
    ClipboardTooLarge(usize),
    #[error("{0} unexpected trailing bytes")]
    TrailingBytes(usize),
}

impl ClientMessage {
    pub fn to_json(&self) -> Result<String, CodecError> {
        Ok(serde_json::to_string(self)?)
    }
    pub fn from_json(s: &str) -> Result<Self, CodecError> {
        Ok(serde_json::from_str(s)?)
    }
}

impl ServerMessage {
    pub fn to_json(&self) -> Result<String, CodecError> {
        Ok(serde_json::to_string(self)?)
    }
    pub fn from_json(s: &str) -> Result<Self, CodecError> {
        Ok(serde_json::from_str(s)?)
    }
}

impl DataMessage {
    pub fn encode(&self) -> Result<Vec<u8>, CodecError> {
        self.check_limits()?;
        Ok(postcard::to_stdvec(self)?)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        // A clipboard string can't be longer than the frame carrying it, so bounding the
        // frame bounds the allocation before postcard touches it.
        if bytes.len() > MAX_CLIPBOARD_BYTES + 16 {
            return Err(CodecError::ClipboardTooLarge(bytes.len()));
        }
        let (msg, rest): (Self, _) = postcard::take_from_bytes(bytes)?;
        if !rest.is_empty() {
            return Err(CodecError::TrailingBytes(rest.len()));
        }
        msg.check_limits()?;
        Ok(msg)
    }

    fn check_limits(&self) -> Result<(), CodecError> {
        match self {
            Self::Clipboard(text) if text.len() > MAX_CLIPBOARD_BYTES => {
                Err(CodecError::ClipboardTooLarge(text.len()))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signaling_round_trips_as_tagged_json() {
        let msgs = vec![
            ClientMessage::HostHello {
                protocol_version: crate::PROTOCOL_VERSION,
                host_id: HostId::new(),
                cert_pem: "-----BEGIN CERTIFICATE-----".into(),
                client_nonce: vec![7; CHALLENGE_LEN],
                label: None,
            },
            ClientMessage::HostProof {
                signature: vec![1, 2, 3],
            },
            ClientMessage::OperatorHello {
                protocol_version: 1,
                token: "t".into(),
            },
            ClientMessage::RequestSession {
                host_id: HostId::new(),
            },
            ClientMessage::Signal {
                session_id: SessionId::new(),
                payload: SignalPayload::Ice {
                    candidate: "candidate:1 1 udp 1 10.0.0.1 5000 typ host".into(),
                    sdp_mid: Some("0".into()),
                    sdp_mline_index: Some(0),
                },
            },
            ClientMessage::EndSession {
                session_id: SessionId::new(),
            },
        ];
        for m in msgs {
            let json = m.to_json().unwrap();
            assert_eq!(ClientMessage::from_json(&json).unwrap(), m);
        }

        let json = ServerMessage::Error {
            code: ErrorCode::Forbidden,
            message: "no".into(),
        }
        .to_json()
        .unwrap();
        assert!(json.contains(r#""type":"error""#), "{json}");
        assert!(json.contains(r#""code":"forbidden""#), "{json}");
    }

    #[test]
    fn data_messages_round_trip_compactly() {
        let msgs = [
            DataMessage::Input(InputEvent::PointerMove { x: 0.25, y: 0.75 }),
            DataMessage::Input(InputEvent::Key {
                hid_usage: 0x04,
                pressed: true,
            }),
            DataMessage::Input(InputEvent::PointerButton {
                button: MouseButton::Right,
                pressed: false,
            }),
            DataMessage::Control(Control::Ping(42)),
            DataMessage::Clipboard("héllo".into()),
        ];
        for m in msgs {
            let bytes = m.encode().unwrap();
            assert_eq!(DataMessage::decode(&bytes).unwrap(), m);
        }
        let mv = DataMessage::Input(InputEvent::PointerMove { x: 0.5, y: 0.5 })
            .encode()
            .unwrap();
        assert!(
            mv.len() <= 10,
            "pointer move should stay tiny, got {} bytes",
            mv.len()
        );
    }

    #[test]
    fn oversized_clipboard_is_rejected_both_ways() {
        let big = "x".repeat(MAX_CLIPBOARD_BYTES + 1);
        assert!(matches!(
            DataMessage::Clipboard(big.clone()).encode(),
            Err(CodecError::ClipboardTooLarge(_))
        ));
        let raw = postcard::to_stdvec(&DataMessage::Clipboard(big)).unwrap();
        assert!(matches!(
            DataMessage::decode(&raw),
            Err(CodecError::ClipboardTooLarge(_))
        ));
    }

    #[test]
    fn unknown_server_message_decodes_as_unknown() {
        let msg = ServerMessage::from_json(r#"{"type":"some_future_thing","x":1}"#).unwrap();
        assert_eq!(msg, ServerMessage::Unknown);
    }

    #[test]
    fn welcome_without_capabilities_still_parses() {
        let org = OrgId::new();
        let json = format!(r#"{{"type":"welcome","org_id":"{org}","ice_servers":[]}}"#);
        let ServerMessage::Welcome { capabilities, .. } = ServerMessage::from_json(&json).unwrap()
        else {
            panic!("not welcome")
        };
        assert!(capabilities.is_empty());
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = DataMessage::Control(Control::Ping(1)).encode().unwrap();
        bytes.push(0);
        assert!(matches!(
            DataMessage::decode(&bytes),
            Err(CodecError::TrailingBytes(1))
        ));
    }
}
