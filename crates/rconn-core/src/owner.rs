//! Who owns a host, and so who may open a session to it without its access password.
//!
//! * Hosts enrolled into an org (an operator's token, `enroll-host`) belong to that org:
//!   its signed-in operators connect without the password.
//! * Hosts that enrolled themselves without an account (the public server, AnyDesk-style)
//!   belong to nobody (`None`): every session must know the access password, and a host
//!   with no password set accepts nobody.
//! * An account may later claim a host ([`HostOwner::Operator`]); then only that account
//!   skips the password.

use crate::ids::{OperatorId, OrgId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HostOwner {
    /// Every signed-in operator of this org.
    Org { org_id: OrgId },
    /// One account.
    Operator { operator_id: OperatorId },
}

/// May a signed-in operator (`org`, `operator`) open a session to a host owned by `owner`
/// without its access password, and see it in their computer list? Never for `None`.
pub fn owner_admits(owner: Option<HostOwner>, org: OrgId, operator: OperatorId) -> bool {
    match owner {
        Some(HostOwner::Org { org_id }) => org_id == org,
        Some(HostOwner::Operator { operator_id }) => operator_id == operator,
        None => false,
    }
}

/// Name of the file in a host's identity folder recording its owner as JSON (`null` for
/// nobody). Written when the host enrolls without an account. Hosts enrolled before this
/// file existed have none and belong to the org that issued their certificate.
pub const OWNER_FILE: &str = "host.owner.json";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nobody_admits_nobody_and_owners_admit_only_themselves() {
        let (org, other_org) = (OrgId::new(), OrgId::new());
        let (op, other_op) = (OperatorId::new(), OperatorId::new());
        assert!(!owner_admits(None, org, op));
        let by_org = Some(HostOwner::Org { org_id: org });
        assert!(owner_admits(by_org, org, op));
        assert!(!owner_admits(by_org, other_org, op));
        let by_op = Some(HostOwner::Operator { operator_id: op });
        assert!(owner_admits(by_op, other_org, op));
        assert!(!owner_admits(by_op, org, other_op));
        assert_eq!(serde_json::to_string(&None::<HostOwner>).unwrap(), "null");
        let json = serde_json::to_string(&by_org).unwrap();
        assert_eq!(
            serde_json::from_str::<Option<HostOwner>>(&json).unwrap(),
            by_org
        );
    }
}
