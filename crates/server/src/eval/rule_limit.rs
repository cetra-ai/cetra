//! The active online rule limit of an org (D-I-32).

use llmobs_core::ids::OrgId;

use crate::http::AppState;
use crate::http::error::ApiError;

/// More active online rules than the plan allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the plan allows {limit} active online rules")]
pub(crate) struct RuleLimitError {
    pub(crate) limit: u64,
}

/// `Ok` when `active_after` (the enabled rules of the org, counted with the one being written)
/// fits `limit`.
#[allow(dead_code)] // I3.8 calls it.
pub(crate) fn check(active_after: u64, limit: Option<u64>) -> Result<(), RuleLimitError> {
    match limit {
        Some(limit) if active_after > limit => Err(RuleLimitError { limit }),
        _ => Ok(()),
    }
}

/// The org's active-rule limit. Track P's M4 returns
/// `entitlements::of(state, org).limits.max_active_online_rules`. Until then every org is
/// unlimited (D-I-32), like `alerts::quota::rule_limit`.
#[allow(dead_code)] // I3.8 calls it.
pub(crate) fn limit(_state: &AppState, _org: OrgId) -> Option<u64> {
    None
}

impl From<RuleLimitError> for ApiError {
    fn from(error: RuleLimitError) -> Self {
        Self::PlanLimit {
            detail: format!(
                "Your plan allows {} active online {}.",
                error.limit,
                if error.limit == 1 { "rule" } else { "rules" }
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_allows_up_to_the_limit_and_no_limit_allows_all() {
        assert!(check(5, None).is_ok());
        assert!(check(1, Some(1)).is_ok());
        assert_eq!(check(2, Some(1)), Err(RuleLimitError { limit: 1 }));
    }

    #[test]
    fn the_error_is_a_plan_limit_naming_the_limit() {
        let ApiError::PlanLimit { detail } = ApiError::from(RuleLimitError { limit: 1 }) else {
            panic!("a plan limit");
        };
        assert_eq!(detail, "Your plan allows 1 active online rule.");
        let ApiError::PlanLimit { detail } = ApiError::from(RuleLimitError { limit: 3 }) else {
            panic!("a plan limit");
        };
        assert_eq!(detail, "Your plan allows 3 active online rules.");
    }
}
