//! Connecting with a computer's ID and access password (no account): the viewer's side of
//! the check in `rconn_core::guest`. The exchange itself lives in `rconn_peer::guest`.

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
