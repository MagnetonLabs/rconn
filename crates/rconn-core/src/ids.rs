//! Strongly typed identifiers so a host id can't be passed where an org id belongs.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(
    /// A tenant. Self-hosted servers have exactly one.
    OrgId
);
id_type!(
    /// An enrolled host machine.
    HostId
);
id_type!(
    /// A human operator account.
    OperatorId
);
id_type!(
    /// One viewer-to-host session.
    SessionId
);

impl HostId {
    /// A 9-digit code for typing ("123 456 789"), derived from the id. Viewers resolve it
    /// against the computers list; collisions within one org are negligible and, if they
    /// happen, the viewer asks the user to pick from the list.
    pub fn short_code(&self) -> String {
        let n = self.0.as_u128() % 1_000_000_000;
        let s = format!("{n:09}");
        format!("{} {} {}", &s[0..3], &s[3..6], &s[6..9])
    }

    /// Does `typed` (any spacing) match this host's short code or full id?
    pub fn matches(&self, typed: &str) -> bool {
        let digits: String = typed
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .collect();
        digits.eq_ignore_ascii_case(&self.short_code().replace(' ', ""))
            || typed.trim().eq_ignore_ascii_case(&self.0.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_code_is_stable_9_digits_and_matches_loosely() {
        let id = HostId(Uuid::parse_str("1e5ede94-60e3-408a-9cc4-b1c92e0ca86b").unwrap());
        let code = id.short_code();
        assert_eq!(code.len(), 11);
        assert_eq!(code, id.short_code());
        assert!(id.matches(&code));
        assert!(id.matches(&code.replace(' ', "")));
        assert!(id.matches("1e5ede94-60e3-408a-9cc4-b1c92e0ca86b"));
        assert!(!id.matches("000 000 000"));
    }
}
