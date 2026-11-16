//! The monthly units cap (D-P-17, D-P-37).

use cetra_core::ids::OrgId;
use chrono::NaiveTime;

use crate::api::entitlements::UsageNotice;
use crate::http::AppState;
use crate::http::error::ApiError;
use crate::usage::month_bounds;

const NS_PER_SEC: i64 = 1_000_000_000;

/// The org is at its cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapExceeded {
    pub(crate) detail: String,
    pub(crate) retry_after_s: u64,
}

impl From<CapExceeded> for ApiError {
    fn from(cap: CapExceeded) -> Self {
        Self::QuotaExceeded {
            detail: cap.detail,
            retry_after_s: cap.retry_after_s,
        }
    }
}

/// `Err(CapExceeded)` when the org's units this month are at or above its `units_cap`.
/// No cap: `Ok` without touching the database. A load error fails open: `warn` with the
/// org id, `Ok`.
pub(crate) async fn check_cap(state: &AppState, org: OrgId) -> Result<(), CapExceeded> {
    let now = state.clock.now_ns();
    let Some(cap) = crate::entitlements::of(state, org).limits.units_cap else {
        return Ok(());
    };
    match state.usage.month_units(&state.meta, org, now).await {
        Ok(units) => decide(units, cap, now).map_or(Ok(()), Err),
        Err(_) => {
            tracing::warn!(org_id = %org, "monthly units unavailable; the cap is not enforced");
            Ok(())
        }
    }
}

/// 00:00 UTC on the 1st of the month after the UTC month of `now_ns`, in nanoseconds.
pub(crate) fn month_reset_ns(now_ns: i64) -> i64 {
    let (_, next) = month_bounds(now_ns);
    next.and_time(NaiveTime::MIN)
        .and_utc()
        .timestamp()
        .saturating_mul(NS_PER_SEC)
}

/// The usage level of `units` against `included` units (D-P-38): `Warning` from 80 %,
/// `Reached` from 100 %. No included units, or zero, is always `None`.
pub(crate) fn notice(units: u64, included: Option<u64>) -> UsageNotice {
    let Some(included) = included.filter(|included| *included > 0) else {
        return UsageNotice::None;
    };
    // Whole percent, rounded down. u64 times 100 fits in u128.
    let percent = u128::from(units) * 100 / u128::from(included);
    if percent >= 100 {
        UsageNotice::Reached
    } else if percent >= 80 {
        UsageNotice::Warning
    } else {
        UsageNotice::None
    }
}

/// The decision, pure: `Some` when `units >= cap`. The reset is 00:00 UTC on the 1st of the
/// next month; `Retry-After` rounds up to whole seconds and is at least 1.
fn decide(units: u64, cap: u64, now_ns: i64) -> Option<CapExceeded> {
    if units < cap {
        return None;
    }
    let (_, next) = month_bounds(now_ns);
    let remaining_ns = month_reset_ns(now_ns).saturating_sub(now_ns).max(0);
    let retry_after_s = u64::try_from(remaining_ns.saturating_add(NS_PER_SEC - 1) / NS_PER_SEC)
        .unwrap_or(0)
        .max(1);
    Some(CapExceeded {
        detail: format!(
            "This organization reached its monthly limit of {cap} units. Ingest resumes on {} (UTC).",
            next.format("%Y-%m-%d")
        ),
        retry_after_s,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nanoseconds of an RFC 3339 instant.
    fn at(rfc3339: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .timestamp_nanos_opt()
            .unwrap()
    }

    #[test]
    fn decide_refuses_at_the_cap_with_the_seconds_to_the_reset() {
        let now = at("2026-01-15T12:00:00.5Z");
        let cap = decide(10, 10, now).expect("at the cap");
        // 1 425 599.5 s to 2026-02-01T00:00Z, rounded up.
        assert_eq!(cap.retry_after_s, 1_425_600);
        assert!(cap.detail.contains("10 units"), "{}", cap.detail);
        assert!(cap.detail.contains("2026-02-01"), "{}", cap.detail);
    }

    #[test]
    fn decide_rolls_over_into_the_next_year() {
        let now = at("2026-12-31T23:59:59Z");
        let cap = decide(5, 5, now).expect("at the cap");
        assert_eq!(cap.retry_after_s, 1);
        assert!(cap.detail.contains("2027-01-01"), "{}", cap.detail);
    }

    #[test]
    fn notice_levels() {
        assert_eq!(notice(79, Some(100)), UsageNotice::None);
        assert_eq!(notice(80, Some(100)), UsageNotice::Warning);
        assert_eq!(notice(99, Some(100)), UsageNotice::Warning);
        assert_eq!(notice(100, Some(100)), UsageNotice::Reached);
        assert_eq!(notice(250, Some(100)), UsageNotice::Reached);
        // 79.9 % rounds down to 79 %.
        assert_eq!(notice(799, Some(1000)), UsageNotice::None);
        assert_eq!(notice(800, Some(1000)), UsageNotice::Warning);
    }

    #[test]
    fn notice_without_included_units_is_none() {
        assert_eq!(notice(u64::MAX, None), UsageNotice::None);
        assert_eq!(notice(0, None), UsageNotice::None);
        assert_eq!(notice(u64::MAX, Some(0)), UsageNotice::None);
    }

    #[test]
    fn notice_does_not_overflow() {
        assert_eq!(notice(u64::MAX, Some(u64::MAX)), UsageNotice::Reached);
        assert_eq!(notice(u64::MAX / 2, Some(u64::MAX)), UsageNotice::None);
        assert_eq!(notice(u64::MAX, Some(1)), UsageNotice::Reached);
    }

    #[test]
    fn decide_allows_below_the_cap() {
        assert_eq!(decide(9, 10, at("2026-01-15T12:00:00Z")), None);
    }

    #[test]
    fn decide_retry_is_at_least_one_second() {
        let now = at("2026-01-31T23:59:59.999999999Z");
        let cap = decide(1, 1, now).expect("at the cap");
        assert_eq!(cap.retry_after_s, 1);
    }

    #[tokio::test]
    async fn check_cap_without_a_cap_is_ok_and_reads_nothing() {
        let app = crate::test_support::TestApp::new().await;
        let seeded = app.seed().await;
        // Break the usage table: without a cap the check must not query it.
        app.state
            .meta
            .write(|conn| {
                Box::pin(async move {
                    sqlx::query("ALTER TABLE usage_daily RENAME TO usage_daily_gone")
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        assert_eq!(check_cap(&app.state, seeded.org_id).await, Ok(()));
    }

    #[tokio::test]
    async fn a_load_error_fails_open_with_a_warning() {
        let app = crate::test_support::TestApp::new().await;
        let seeded = app.seed().await;
        crate::test_support::set_limits(
            &app,
            seeded.org_id,
            "2025-12-01T00:00:00Z",
            &serde_json::json!({"units_cap": 10}),
        );
        app.state
            .meta
            .write(|conn| {
                Box::pin(async move {
                    sqlx::query("ALTER TABLE usage_daily RENAME TO usage_daily_gone")
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let (result, logs) =
            crate::test_support::capture_logs(check_cap(&app.state, seeded.org_id)).await;
        assert_eq!(result, Ok(()));
        assert!(logs.contains("WARN"), "{logs}");
        assert!(logs.contains("the cap is not enforced"), "{logs}");
    }
}
