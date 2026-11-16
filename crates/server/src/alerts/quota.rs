//! The per-org alert rule limit (D178, D-K-14) and the daily alert email cap (D-K-19).

use std::collections::HashMap;
use std::sync::Mutex;

use cetra_core::ids::OrgId;

use crate::http::AppState;
use crate::http::error::ApiError;

/// Alert email recipient messages one org may send per UTC day, test sends included (D-K-19).
pub(crate) const DAILY_EMAIL_LIMIT: u64 = 1000;

const DAY_NS: i64 = 86_400_000_000_000;

/// Recipient messages sent per org on the current UTC day. It lives in memory, so a restart
/// starts every count again (D-K-19).
#[derive(Default)]
pub(crate) struct DailyEmailCount {
    /// The UTC day index of `counts`, and the messages per org that day.
    state: Mutex<(i64, HashMap<OrgId, u64>)>,
}

impl DailyEmailCount {
    /// Reserves `messages` for `org_id` on the UTC day of `now`. Returns `false` and reserves
    /// nothing when that would pass [`DAILY_EMAIL_LIMIT`].
    pub(crate) fn reserve(&self, org_id: OrgId, messages: u64, now: i64) -> bool {
        let day = now.div_euclid(DAY_NS);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Only a later day starts the count again: a stale `now` never wipes today's sends.
        if day > state.0 {
            *state = (day, HashMap::new());
        }
        let count = state.1.entry(org_id).or_insert(0);
        if count.saturating_add(messages) > DAILY_EMAIL_LIMIT {
            return false;
        }
        *count += messages;
        true
    }
}

/// The most enabled alert rules `org_id` may have (D178, D-P-23); `None` means no limit.
pub(crate) fn rule_limit(state: &AppState, org_id: OrgId) -> Option<u64> {
    crate::entitlements::of(state, org_id)
        .limits
        .max_alert_rules
}

/// `Err(ApiError::PlanLimit)` when one more enabled rule would exceed `limit`.
pub(crate) fn check_rule_quota(enabled_rules: i64, limit: Option<u64>) -> Result<(), ApiError> {
    // A count is never negative; treat a corrupt negative count as 0.
    let enabled_rules = u64::try_from(enabled_rules).unwrap_or(0);
    match limit {
        Some(limit) if enabled_rules >= limit => Err(ApiError::PlanLimit {
            detail: format!(
                "Your plan allows {limit} enabled alert {}.",
                if limit == 1 { "rule" } else { "rules" }
            ),
        }),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;
    use crate::api::common::ProblemCode;

    #[test]
    fn no_limit_allows_everything() {
        assert!(check_rule_quota(5, None).is_ok());
    }

    #[test]
    fn limit_one() {
        assert!(check_rule_quota(0, Some(1)).is_ok());
        let Err(ApiError::PlanLimit { detail }) = check_rule_quota(1, Some(1)) else {
            panic!("one enabled rule reaches a limit of one");
        };
        assert_eq!(detail, "Your plan allows 1 enabled alert rule.");
        let Err(ApiError::PlanLimit { detail }) = check_rule_quota(3, Some(3)) else {
            panic!("three enabled rules reach a limit of three");
        };
        assert_eq!(detail, "Your plan allows 3 enabled alert rules.");
    }

    #[test]
    fn daily_email_cap_is_per_org_and_per_utc_day() {
        const DAY: i64 = 86_400_000_000_000;
        let count = DailyEmailCount::default();
        let (acme, other) = (OrgId::new_v7(), OrgId::new_v7());
        let noon = DAY + DAY / 2;
        assert!(count.reserve(acme, DAILY_EMAIL_LIMIT - 1, noon));
        // Two more would pass the cap: refused, and nothing is reserved.
        assert!(!count.reserve(acme, 2, noon));
        assert!(count.reserve(acme, 1, noon));
        assert!(!count.reserve(acme, 1, noon));
        assert!(count.reserve(other, DAILY_EMAIL_LIMIT, noon));
        // The next UTC day starts again.
        assert!(count.reserve(acme, DAILY_EMAIL_LIMIT, 2 * DAY));
    }

    #[test]
    fn an_earlier_day_does_not_reset_the_count() {
        const DAY: i64 = 86_400_000_000_000;
        let count = DailyEmailCount::default();
        let acme = OrgId::new_v7();
        assert!(count.reserve(acme, DAILY_EMAIL_LIMIT, 2 * DAY));
        // A clock that reads an earlier day keeps the later day's count.
        assert!(!count.reserve(acme, 1, DAY));
        assert!(!count.reserve(acme, 1, 2 * DAY));
    }

    #[test]
    fn plan_limit_problem() {
        let problem = ApiError::PlanLimit {
            detail: "x".to_owned(),
        }
        .problem();
        assert_eq!(problem.status, StatusCode::FORBIDDEN.as_u16());
        assert_eq!(problem.code, ProblemCode::PlanLimit);
        assert_eq!(problem.detail.as_deref(), Some("x"));
        let json = serde_json::to_value(&problem).unwrap();
        assert_eq!(json["code"], "plan_limit");
    }
}
