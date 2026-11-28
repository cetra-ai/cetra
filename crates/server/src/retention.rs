//! Per-project retention (spec 03 § 8, D-M-10, D-M-15, D-P-45).

use std::collections::HashMap;
use std::time::Duration;

use cetra_core::ids::{OrgId, ProjectId};
use chrono::{DateTime, Days, NaiveDate};
use tracing::Instrument as _;

use crate::http::AppState;
use crate::meta::MetaError;
use crate::observability::spans;

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
///
/// Runs in a `retention.pass` span.
pub(crate) async fn retention_once(state: &AppState) -> Result<RetentionRun, MetaError> {
    let span = spans::retention_pass();
    let result = retention_pass(state).instrument(span.clone()).await;
    match &result {
        Ok(run) => {
            span.record(spans::PROJECTS, spans::count(run.projects));
            span.record(spans::FILES, spans::count(run.files));
            span.record(spans::FAILURES, spans::count(run.failures));
            span.record(spans::OUTCOME, "ok");
        }
        Err(_) => {
            span.record(spans::OUTCOME, "error");
        }
    }
    result
}

/// Applies the effective retention of every active project of an active org.
/// A storage error on one project is logged and counted; the others still run.
///
/// # Errors
///
/// [`MetaError`] when the projects cannot be listed.
async fn retention_pass(state: &AppState) -> Result<RetentionRun, MetaError> {
    let now = state.clock.now_ns();
    let (projects, cap_failures) = effective_retention(state, now).await?;
    let mut run = RetentionRun {
        failures: cap_failures,
        ..RetentionRun::default()
    };
    for (project, days) in projects {
        let Some(days) = days else {
            continue;
        };
        let keep = keep_from(now, days);
        // Annotation queue items age out with the project's data (D-AQ-9). This runs whatever the
        // storage result is.
        if drop_queue_items(state, project, keep).await.is_err() {
            run.failures += 1;
            tracing::warn!(project_id = %project, "annotation queue retention failed");
        }
        // A project dropped by storage is a no-op there (`committer::apply_retention` finds it
        // in `projects`), so a drop that races this list writes nothing.
        match state.storage.apply_retention(project, keep).await {
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

/// Each active project of an active org with its effective retention (`None` when it has
/// none), and how many org caps could not be read (logged; D-P-45). The cap of each org is
/// read once per call.
///
/// # Errors
///
/// [`MetaError`] when the projects cannot be listed.
pub(crate) async fn effective_retention(
    state: &AppState,
    now: i64,
) -> Result<(Vec<(ProjectId, Option<u32>)>, u64), MetaError> {
    let mut projects = Vec::new();
    let mut cap_failures: u64 = 0;
    // The cap of each org, read once per call. A failed read counts once and skips only
    // the cap: a project's own retention still applies (D-P-45).
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
                        cap_failures += 1;
                        tracing::warn!(org_id = %org, error = %error, "retention window unavailable");
                        Err(())
                    }
                };
                caps.insert(org, cap);
                cap
            }
        };
        // D-P-45: the largest window of the last 30 days, plus 30 days (D175). When the org's
        // window read failed there is no cap: the project's own retention still applies and a
        // project that relies on the cap is skipped by the caller (deleting nothing is the safe side).
        let cap = cap.unwrap_or(None);
        projects.push((project, effective_days(own, cap)));
    }
    Ok((projects, cap_failures))
}

/// Applies `days` to one project now (a `PATCH`). Errors are logged; the hourly run retries.
pub(crate) async fn apply_now(state: &AppState, project: ProjectId, days: u32) {
    let keep = keep_from(state.clock.now_ns(), days);
    if drop_queue_items(state, project, keep).await.is_err() {
        tracing::warn!(project_id = %project, "annotation queue retention failed");
    }
    if let Err(error) = state.storage.apply_retention(project, keep).await {
        tracing::warn!(project_id = %project, error = %error, "retention failed");
    }
}

/// Deletes the annotation queue items of `project` whose `target_time` is before the start of
/// `keep` (D-AQ-9). Returns the number deleted. Nothing is deleted when `keep` has no start.
async fn drop_queue_items(
    state: &AppState,
    project: ProjectId,
    keep: NaiveDate,
) -> Result<u64, MetaError> {
    // A real day always has a 00:00 start in i64 ns; the `None` arm only keeps the function total.
    let Some(cutoff) = keep
        .and_hms_opt(0, 0, 0)
        .and_then(|start| start.and_utc().timestamp_nanos_opt())
    else {
        return Ok(0);
    };
    state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                crate::meta::annotation_queues::delete_items_before(conn, project, cutoff).await
            })
        })
        .await
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use cetra_core::ids::{OrgId, ProjectId, SpanId, TraceId};
    use cetra_core::span::Span;

    use uuid::Uuid;

    use super::*;
    use crate::meta::annotation_queues::{
        AddedByKind, NewItem, NewQueue, QueueCreate, Target, insert_items_tx, insert_queue_tx,
    };
    use crate::meta::entitlements::record_window_end;
    use crate::meta::orgs::insert_org;
    use crate::meta::projects::{ProjectRow, insert_project};
    use crate::observability::otlp::receiver::{int_attribute, string_attribute};
    use crate::observability::subscriber::capture_spans;
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_retention_pass_runs_in_its_span() {
        let app = TestApp::new().await;
        let (result, captured) = capture_spans(retention_once(&app.state)).await;
        assert!(result.is_ok());
        let spans = captured.named("retention.pass");
        assert_eq!(spans.len(), 1, "{spans:?}");
        let span = &spans[0];
        assert!(int_attribute(&span.attributes, "projects").is_some());
        assert!(int_attribute(&span.attributes, "files").is_some());
        assert!(int_attribute(&span.attributes, "failures").is_some());
        assert_eq!(
            string_attribute(&span.attributes, "outcome").as_deref(),
            Some("ok")
        );
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

    /// D-P-45: a window that cannot be read skips only the cap (counted once for the org):
    /// a project's own retention still applies, one relying on the cap is skipped.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_window_read_error_skips_only_the_cap() {
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
                    let p1 = insert_project(conn, org, "A1", "a1", Some(7), now).await?;
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
        // A1's own 7 days still apply; A2 relies on the cap and is skipped.
        assert_eq!(raw_span_count(&app.state.storage, a1.id).await, 0);
        assert_eq!(raw_span_count(&app.state.storage, a2.id).await, 1);
        assert_eq!(raw_span_count(&app.state.storage, short.id).await, 0);
    }

    /// A trace queue of `project`, created at `NOW`, holding one trace item per `(trace, time)`.
    /// Returns its id.
    async fn trace_queue(app: &TestApp, project: &ProjectRow, items: Vec<(String, i64)>) -> Uuid {
        let (org, project_id) = (project.org_id, project.id);
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let created = insert_queue_tx(
                        conn,
                        project_id,
                        org,
                        NewQueue {
                            name: "traces".to_owned(),
                            description: None,
                            instructions: None,
                            target: Target::Trace,
                            definition_ids: Vec::new(),
                            created_by: None,
                        },
                        None,
                        NOW,
                    )
                    .await?;
                    let QueueCreate::Created(row) = created else {
                        panic!("the queue is created");
                    };
                    let items: Vec<NewItem> = items
                        .into_iter()
                        .map(|(trace, target_time)| NewItem {
                            trace_id: Some(trace),
                            span_id: None,
                            session_id: None,
                            target_time,
                        })
                        .collect();
                    insert_items_tx(
                        conn,
                        row.id,
                        project_id,
                        &items,
                        (AddedByKind::User, Uuid::now_v7()),
                        NOW,
                    )
                    .await?;
                    Ok(row.id)
                })
            })
            .await
            .unwrap()
    }

    /// The number of items of `queue` on `trace`.
    async fn queue_items_of(app: &TestApp, queue: Uuid, trace: &str) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM annotation_queue_items WHERE queue_id = ? AND trace_id = ?",
        )
        .bind(queue.to_string())
        .bind(trace.to_owned())
        .fetch_one(app.state.meta.reader())
        .await
        .unwrap()
    }

    /// D-AQ-9: an item whose target starts before the first kept day goes with the hourly run;
    /// a newer one stays.
    #[tokio::test(flavor = "multi_thread")]
    async fn hourly_run_drops_old_queue_items() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        let (short, _) = app.add_project(&seeded, "short", Some(1)).await;
        let queue = trace_queue(
            &app,
            &short,
            vec![
                (trace_id(1), NOW - 3 * DAY + 12 * HOUR),
                (trace_id(2), NOW - 12 * HOUR),
            ],
        )
        .await;

        let run = retention_once(&app.state).await.unwrap();
        assert_eq!(run.failures, 0);
        assert_eq!(queue_items_of(&app, queue, &trace_id(1)).await, 0);
        assert_eq!(queue_items_of(&app, queue, &trace_id(2)).await, 1);
    }

    /// D-AQ-9: a `PATCH` that sets retention drops the queue items it makes old at once.
    #[tokio::test(flavor = "multi_thread")]
    async fn apply_now_drops_old_queue_items() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        let queue = trace_queue(
            &app,
            &seeded.project,
            vec![
                (trace_id(1), NOW - 3 * DAY + 12 * HOUR),
                (trace_id(2), NOW - 12 * HOUR),
            ],
        )
        .await;

        apply_now(&app.state, seeded.project.id, 1).await;
        assert_eq!(queue_items_of(&app, queue, &trace_id(1)).await, 0);
        assert_eq!(queue_items_of(&app, queue, &trace_id(2)).await, 1);
    }
}
