//! The active issue source limit of an org (D-IC-13).

#![cfg_attr(not(test), allow(dead_code))] // no non-test caller until the sources API lands (M1.7)

use cetra_core::ids::OrgId;

use crate::http::AppState;
use crate::http::error::ApiError;

/// More enabled issue sources than the plan allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the plan allows {limit} active issue sources")]
pub(crate) struct SourceLimitError {
    pub(crate) limit: u64,
}

/// `Ok` when `active_after` (the enabled sources of the org, counted with the one being written)
/// fits `limit`.
pub(crate) fn check(active_after: u64, limit: Option<u64>) -> Result<(), SourceLimitError> {
    match limit {
        Some(limit) if active_after > limit => Err(SourceLimitError { limit }),
        _ => Ok(()),
    }
}

/// The org's active-source limit from its entitlements (D-IC-13, D-P-10). `None` is unlimited.
pub(crate) fn limit(state: &AppState, org: OrgId) -> Option<u64> {
    crate::entitlements::of(state, org)
        .limits
        .max_active_issue_sources
}

impl From<SourceLimitError> for ApiError {
    fn from(error: SourceLimitError) -> Self {
        Self::PlanLimit {
            detail: format!(
                "Your plan allows {} active issue {}.",
                error.limit,
                if error.limit == 1 {
                    "source"
                } else {
                    "sources"
                }
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
        assert_eq!(check(2, Some(1)), Err(SourceLimitError { limit: 1 }));
    }

    #[test]
    fn the_error_is_a_plan_limit_naming_the_limit() {
        let ApiError::PlanLimit { detail } = ApiError::from(SourceLimitError { limit: 1 }) else {
            panic!("a plan limit");
        };
        assert_eq!(detail, "Your plan allows 1 active issue source.");
        let ApiError::PlanLimit { detail } = ApiError::from(SourceLimitError { limit: 3 }) else {
            panic!("a plan limit");
        };
        assert_eq!(detail, "Your plan allows 3 active issue sources.");
    }
}
