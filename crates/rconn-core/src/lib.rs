//! Shared types for rconn: identifiers, the signaling and data-channel protocol,
//! and layered host settings with fleet-policy locking.

pub mod fleet;
pub mod ids;
pub mod protocol;
pub mod settings;
pub mod tls;

/// Bumped on any incompatible change to [`protocol`]. Peers refuse a mismatched major.
pub const PROTOCOL_VERSION: u16 = 1;
