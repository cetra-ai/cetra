//! Per-org SSO types (track P M5, D-P-56). The routes come in later phases.
// Track P M5: P7 and P10 use these outside tests.
#![cfg_attr(not(test), allow(dead_code))]

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::api::account::OrgRole;

/// Role given to people who join through the org's provider (D-P-56).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JitRole {
    /// Read-only.
    Viewer,
    /// Member.
    Member,
}

impl JitRole {
    /// The stored text (`org_sso_providers.jit_role`).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Member => "member",
        }
    }

    /// The role named by a stored value. `None` for anything else.
    pub(crate) fn from_str_opt(value: &str) -> Option<Self> {
        match value {
            "viewer" => Some(Self::Viewer),
            "member" => Some(Self::Member),
            _ => None,
        }
    }

    /// The org role a joining user gets.
    pub(crate) fn org_role(self) -> OrgRole {
        match self {
            Self::Viewer => OrgRole::Viewer,
            Self::Member => OrgRole::Member,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jit_role_round_trips_its_stored_text() {
        for role in [JitRole::Viewer, JitRole::Member] {
            assert_eq!(JitRole::from_str_opt(role.as_str()), Some(role));
        }
        assert_eq!(JitRole::from_str_opt("owner"), None);
        assert_eq!(JitRole::from_str_opt("admin"), None);
        assert_eq!(JitRole::Viewer.org_role(), OrgRole::Viewer);
        assert_eq!(JitRole::Member.org_role(), OrgRole::Member);
    }
}
