//! Shared types for rconn: identifiers, the signaling and data-channel protocol,
//! and layered host settings with fleet-policy locking.

pub mod fleet;
pub mod guest;
pub mod hid;
pub mod ids;
pub mod owner;
pub mod protocol;
pub mod settings;
pub mod status;
pub mod tls;

/// Bumped on any incompatible change to [`protocol`]. Peers refuse a mismatched major.
pub const PROTOCOL_VERSION: u16 = 1;

/// The public server apps use when nobody configured one (self-hosters override it in
/// Settings). It has an ordinary public certificate: clients use WebPKI trust, no pin.
pub const DEFAULT_SERVER: &str = "wss://connect.rconn.app/v1/ws";
