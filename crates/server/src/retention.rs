//! Per-project retention (spec 03 § 8, D-M-10, D-M-15, D-P-45).

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Days, NaiveDate};
use llmobs_core::ids::{OrgId, ProjectId};

use crate::http::AppState;
use crate::meta::MetaError;

/// Time between two runs.
pub(crate) const EVERY: Duration = Duration::from_secs(3600);
/// Delay of the first run after startup.
pub(crate) const FIRST_RUN: Duration = Duration::from_secs(60);

/// The first UTC day to keep: the day of `now_ns`, minus `days`.
pub(crate) fn keep_from(now_ns: i64, days: u32) -> NaiveDate {
    let today = DateTime::from_timestamp_nanos(now_ns).date_naive();
    // i64 ns covers 1677–2262 and days ≤ 36_500, so this cannot leave chrono's range;
    // `NaiveDate::MIN` only keeps the function total.
    today
        .checked_sub_days(Days::new(u64::from(days)))
        .unwrap_or(NaiveDate::MIN)
}

/// Effective retention: the project's own and the org's cap, whichever is shorter;
/// the one that is set when only one is; none when neither is (D-M-15).
pub(crate) fn effective_days(project: Option<u32>, cap: Option<u32>) -> Option<u32> {
    match (project, cap) {
        (Some(days), Some(cap)) => Some(days.min(cap)),
        (Some(days), None) | (None, Some(days)) => Some(days),
        (None, None) => None,
    }
}

/// Totals of one run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RetentionRun {
    /// Projects whose call succeeded.
    pub(crate) projects: u64,
    /// Projects whose call failed (logged).
    pub(crate) failures: u64,
    /// Files obsoleted, all projects.
    pub(crate) files: u64,
}

/// Applies the effective retention of every active project of an active org.
/// A storage error on one project is logged and counted; the others still run.
///
/// # Errors
///
/// [`MetaError`] when the projects cannot be listed.
pub(crate) async fn retention_once(state: &AppState) -> Result<RetentionRun, MetaError> {
    let now = state.clock.now_ns();
    let mut run = RetentionRun::default();
    // The cap of each org, read once per run. A failed read counts once and skips the org's
    // projects: deleting nothing is the safe side (D-P-45).
    let mut caps: HashMap<OrgId, Result<Option<u32>, ()>> = HashMap::new();
    for (org, project, own) in
        crate::meta::projects::projects_for_retention(state.meta.reader()).await?
    {
        let cap = match caps.get(&org) {
            Some(cap) => *cap,
            None => {
                let cap = match state
                    .entitlements
                    .retention_window_at(&state.meta, org, now)
                    .await
                {
                    Ok(window) => Ok(crate::entitlements::retention_cap(window)),
                    Err(error) => {
                        run.failures += 1;
                        tracing::warn!(org_id = %org, error = %error, "retention window unavailable");
                        Err(())
                    }
                };
                caps.insert(org, cap);
                cap
            }
        };
        // D-P-45: the largest window of the last 30 days, plus 30 days (D175).
        let Ok(cap) = cap else {
            continue;
        };
        let Some(days) = effective_days(own, cap) else {
            continue;
        };
        // A project dropped by storage is a no-op there (`committer::apply_retention` finds it
        // in `projects`), so a drop that races this list writes nothing.
        match state
            .storage
            .apply_retention(project, keep_from(now, days))
            .await
        {
            Ok(report) => {
                run.projects += 1;
                run.files += report.files;
            }
            Err(error) => {
                run.failures += 1;
                tracing::warn!(project_id = %project, error = %error, "retention failed");
            }
        }
    }
    Ok(run)
}

/// Applies `days` to one project now (a `PATCH`). Errors are logged; the hourly run retries.
pub(crate) async fn apply_now(state: &AppState, project: ProjectId, days: u32) {
    let keep = keep_from(state.clock.now_ns(), days);
    if let Err(error) = state.storage.apply_retention(project, keep).await {
        tracing::warn!(project_id = %project, error = %error, "retention failed");
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use llmobs_core::ids::{OrgId, ProjectId, SpanId, TraceId};
    use llmobs_core::span::Span;

    use super::*;
    use crate::meta::entitlements::record_window_end;
    use crate::meta::orgs::insert_org;
    use crate::meta::projects::{ProjectRow, insert_project};
    use crate::test_support::{Seeded, TestApp, dataset_span, raw_span_count, set_limits};

    /// 2026-01-01T00:00:00Z, the manual clock's start.
    const NOW: i64 = 1_767_225_600_000_000_000;
    const DAY: i64 = 86_400 * 1_000_000_000;
    const HOUR: i64 = 3_600 * 1_000_000_000;

    #[test]
    fn keep_from_counts_utc_days() {
        assert_eq!(
            keep_from(NOW, 30),
            NaiveDate::from_ymd_opt(2025, 12, 2).unwrap()
        );
        assert_eq!(
            keep_from(NOW + DAY - 1, 1),
            NaiveDate::from_ymd_opt(2025, 12, 31).unwrap()
        );
        assert_eq!(
            keep_from(NOW, 36_500),
            NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() - Days::new(36_500)
        );
        let _ = keep_from(i64::MIN, 1);
    }

    #[test]
    fn effective_days_takes_the_shorter() {
        assert_eq!(effective_days(Some(30), Some(7)), Some(7));
        assert_eq!(effective_days(Some(7), Some(30)), Some(7));
        assert_eq!(effective_days(Some(7), None), Some(7));
        assert_eq!(effective_days(None, Some(30)), Some(30));
        assert_eq!(effective_days(None, None), None);
    }

    /// A trace id of the tests: `a3`, then `n` in 30 hex digits.
    fn trace_id(n: u32) -> String {
        format!("a3{n:030x}")
    }

    /// Commits one span of `trace` to `project`, with `start_time` as given.
    async fn commit_at(app: &TestApp, project: &ProjectRow, trace: &str, start_time: i64) {
        let mut row: Span = dataset_span(0);
        row.trace_id = trace.parse::<TraceId>().unwrap();
        row.span_id = "b3000000000000aa".parse::<SpanId>().unwrap();
        row.parent_span_id = None;
        row.start_time = start_time;
        row.end_time = start_time + 1_000;
        row.ingest_time = app.state.clock.now_ns();
        app.commit_spans(project, vec![row]).await;
    }

    /// `GET /api/v1/projects/{project}/traces/{trace}` with the seeded cookie; returns the status.
    async fn trace_status(
        app: &TestApp,
        seeded: &Seeded,
        project: ProjectId,
        trace: &str,
    ) -> StatusCode {
        app.request(
            Method::GET,
            &format!("/api/v1/projects/{project}/traces/{trace}"),
        )
        .header("cookie", &seeded.cookie)
        .send()
        .await
        .status
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hourly_run_drops_old_days() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        let (short, _) = app.add_project(&seeded, "short", Some(1)).await;
        commit_at(&app, &short, &trace_id(1), NOW - 3 * DAY + 12 * HOUR).await;
        commit_at(&app, &short, &trace_id(2), NOW - 12 * HOUR).await;
        let plain = seeded.project.clone();
        commit_at(&app, &plain, &trace_id(3), NOW - 3 * DAY + 12 * HOUR).await;
        commit_at(&app, &plain, &trace_id(4), NOW - 12 * HOUR).await;

        let run = retention_once(&app.state).await.unwrap();
        assert_eq!(run.projects, 1);
        assert_eq!(run.failures, 0);

        assert_eq!(
            trace_status(&app, &seeded, short.id, &trace_id(1)).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            trace_status(&app, &seeded, short.id, &trace_id(2)).await,
            StatusCode::OK
        );
        assert_eq!(
            trace_status(&app, &seeded, plain.id, &trace_id(3)).await,
            StatusCode::OK
        );
        assert_eq!(
            trace_status(&app, &seeded, plain.id, &trace_id(4)).await,
            StatusCode::OK
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn inactive_projects_are_skipped() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        let (deleting, _) = app.add_project(&seeded, "p-del", Some(1)).await;
        let _ = app.add_project(&seeded, "p-ok", Some(1)).await;
        let deleting_id = deleting.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("UPDATE projects SET state = 'deleting' WHERE id = ?")
                        .bind(deleting_id.to_string())
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();

        let run = retention_once(&app.state).await.unwrap();
        assert_eq!(run.projects, 1);

        let org_id = seeded.org_id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("UPDATE orgs SET state = 'deleting' WHERE id = ?")
                        .bind(org_id.to_string())
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let run = retention_once(&app.state).await.unwrap();
        assert_eq!(run.projects, 0);
    }

    /// D-P-45: a plan's window caps retention at its window plus 30 days, so a span 70 days old
    /// goes and one 50 days old stays.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_plan_caps_retention() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2026-01-01T00:00:00Z",
            &serde_json::json!({"data_access_days": 30}),
        );
        let project = seeded.project.id;
        commit_at(&app, &seeded.project, &trace_id(1), NOW - 70 * DAY).await;
        commit_at(&app, &seeded.project, &trace_id(2), NOW - 50 * DAY).await;
        assert_eq!(raw_span_count(&app.state.storage, project).await, 2);

        let run = retention_once(&app.state).await.unwrap();
        assert_eq!(run.failures, 0);
        assert_eq!(raw_span_count(&app.state.storage, project).await, 1);
    }

    /// D-P-45: a downgrade keeps the old window for 30 days: the largest window of the last 30
    /// days, here 1 825 days, plus 30.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_downgrade_waits_30_days() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        let org = seeded.org_id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    record_window_end(conn, org, NOW - DAY, Some(1_825)).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        set_limits(
            &app,
            org,
            "2026-01-01T00:00:00Z",
            &serde_json::json!({"data_access_days": 30}),
        );
        let project = seeded.project.id;
        commit_at(&app, &seeded.project, &trace_id(1), NOW - 70 * DAY).await;
        assert_eq!(raw_span_count(&app.state.storage, project).await, 1);

        retention_once(&app.state).await.unwrap();
        assert_eq!(raw_span_count(&app.state.storage, project).await, 1);
    }

    /// D-P-45: a window that cannot be read skips that org's projects (counted once for the
    /// org); the other orgs still apply their retention.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_window_read_error_skips_the_org() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        let now = NOW;
        let (org_a, a1, a2): (OrgId, ProjectRow, ProjectRow) = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let org = OrgId::new_v7();
                    insert_org(conn, org, "A", "org-a", now).await?;
                    let p1 = insert_project(conn, org, "A1", "a1", None, now).await?;
                    let p2 = insert_project(conn, org, "A2", "a2", None, now).await?;
                    Ok((org, p1, p2))
                })
            })
            .await
            .unwrap();
        set_limits(
            &app,
            org_a,
            "2026-01-01T00:00:00Z",
            &serde_json::json!({"data_access_days": 30}),
        );
        commit_at(&app, &a1, &trace_id(1), NOW - 70 * DAY).await;
        commit_at(&app, &a2, &trace_id(2), NOW - 70 * DAY).await;
        // A corrupt window end of org A: its window history cannot be decoded.
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query(
                        "INSERT INTO org_window_ends (org_id, ended_at, data_access_days) \
                         VALUES (?, ?, -1)",
                    )
                    .bind(org_a.to_string())
                    .bind(NOW - DAY)
                    .execute(&mut *conn)
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();

        let (short, _) = app.add_project(&seeded, "short", Some(1)).await;
        commit_at(&app, &short, &trace_id(3), NOW - 3 * DAY).await;

        let run = retention_once(&app.state).await.unwrap();
        assert_eq!(
            run.failures, 1,
            "one failure for the org, not one per project"
        );
        assert_eq!(raw_span_count(&app.state.storage, a1.id).await, 1);
        assert_eq!(raw_span_count(&app.state.storage, a2.id).await, 1);
        assert_eq!(raw_span_count(&app.state.storage, short.id).await, 0);
    }
}
