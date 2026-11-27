//! The org home's per-project activity (spec 09, D-C-27).

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::time::Duration;

use axum::extract::{Extension, State};
use axum::routing::get;
use cetra_core::ids::ProjectId;
use cetra_storage as storage;
use futures::StreamExt;

use crate::api::project_activity::{
    ActivityBucket, ProjectActivity, ProjectActivityParams, ProjectsActivity,
};
use crate::auth::principal::{self, Principal};
use crate::auth::roles::OrgPerm;
use crate::http::AppState;
use crate::http::access::{Access, OrgAccess, RouteDef};
use crate::http::error::{self, ApiError, field_error};
use crate::meta::projects::ProjectRow;
use crate::routes::projects::visible_projects;
use crate::routes::read::{Deadline, open_query};

/// Most project ids in one request: one page of the project list.
const MAX_IDS: usize = 50;
/// Hourly buckets in the window (OH-1).
const BUCKETS: usize = 24;
/// One hour in nanoseconds.
const HOUR_NS: i64 = 3_600_000_000_000;
/// Projects queried at once (OH-3).
const CONCURRENCY: usize = 4;
/// The error traces, a traces-view filter (the UI's `ERROR` preset).
const ERROR_CONDITION: &str = "status = 'error'";
/// The 400 message on `ids`. It names no value.
const IDS_MESSAGE: &str = "ids must be 1 to 50 distinct project ids";
/// The longest one project's query may run, so one slow project cannot use up the request's
/// 30 s deadline and leave the projects queued behind it `unavailable` (review, D-C-27).
const PROJECT_TIMEOUT: Duration = Duration::from_secs(10);

/// `GET /api/v1/orgs/{org}/projects/activity`.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![RouteDef {
        method: axum::http::Method::GET,
        path: "/api/v1/orgs/{org}/projects/activity",
        access: Access::Org {
            perm: OrgPerm::View,
            session_only: false,
        },
        documented: true,
        handler: get(get_activity),
    }]
}

/// Comma-separated project ids, in request order: distinct, and a count of 1 to [`MAX_IDS`].
/// Any other input is a 400 on `ids`.
fn parse_ids(raw: &str) -> Result<Vec<ProjectId>, ApiError> {
    let invalid = || field_error("ids", IDS_MESSAGE);
    let ids = raw
        .split(',')
        .map(|part| ProjectId::from_str(part.trim()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid())?;
    let distinct: HashSet<ProjectId> = ids.iter().copied().collect();
    if (1..=MAX_IDS).contains(&ids.len()) && distinct.len() == ids.len() {
        Ok(ids)
    } else {
        Err(invalid())
    }
}

/// The activity window at `now` (OH-1): the 24 epoch-aligned hours whose last one holds `now`.
/// `to` is one nanosecond after `now`, so the current hour is always the last bucket.
fn activity_window(now: i64) -> Result<storage::TimeWindow, ApiError> {
    let hour = now.div_euclid(HOUR_NS) * HOUR_NS;
    let from = hour.saturating_sub(23 * HOUR_NS);
    let to = now.saturating_add(1);
    Ok(storage::TimeWindow::new(from, to)?)
}

/// Traces, error traces and cost, in this measure order (OH-2).
fn activity_query() -> storage::WidgetQuery {
    use storage::{Agg, Measure, WidgetQuery, WidgetSource};
    WidgetQuery {
        source: WidgetSource::Traces,
        filter: None,
        measures: vec![
            Measure {
                agg: Agg::Count,
                field: None,
                condition: None,
            },
            Measure {
                agg: Agg::Count,
                field: None,
                condition: Some(ERROR_CONDITION.to_owned()),
            },
            Measure {
                agg: Agg::Sum,
                field: Some(cetra_core::schema::traces::COST_TOTAL.to_owned()),
                condition: None,
            },
        ],
        breakdown: None,
    }
}

/// One project's answer from its series (OH-2). Buckets without a point stay at zero.
fn activity_of(
    project_id: ProjectId,
    from: i64,
    data: &storage::WidgetData,
    last_ingest_at: Option<i64>,
) -> ProjectActivity {
    let mut buckets = vec![
        ActivityBucket {
            traces: 0,
            errors: 0
        };
        BUCKETS
    ];
    let mut cost_usd = 0.0;
    if let storage::WidgetData::Series { series, .. } = data {
        for line in series {
            for point in &line.points {
                let offset = point.time.saturating_sub(from);
                if offset < 0 || offset % HOUR_NS != 0 {
                    continue;
                }
                let Ok(index) = usize::try_from(offset / HOUR_NS) else {
                    continue;
                };
                let Some(bucket) = buckets.get_mut(index) else {
                    continue;
                };
                let value = point.value.unwrap_or(0.0);
                match line.measure {
                    0 => bucket.traces = whole(value),
                    1 => bucket.errors = whole(value),
                    2 => cost_usd += value,
                    _ => {}
                }
            }
        }
    }
    ProjectActivity {
        project_id,
        traces: buckets.iter().map(|bucket| bucket.traces).sum(),
        errors: buckets.iter().map(|bucket| bucket.errors).sum(),
        cost_usd,
        buckets,
        last_ingest_at,
    }
}

/// A count from an aggregate value: rounded, and 0 for a negative or non-finite value.
fn whole(value: f64) -> u64 {
    if value.is_finite() && value > 0.0 {
        // Positive and finite, so the cast cannot wrap; Rust's float-to-int cast saturates above
        // `u64::MAX`, and a count never gets near it.
        value.round() as u64
    } else {
        0
    }
}

/// One project's activity, or the reason it failed: `timeout`, `open` or `query`.
async fn project_activity(
    state: &AppState,
    project: &ProjectRow,
    window: storage::TimeWindow,
    deadline: Deadline,
) -> Result<ProjectActivity, &'static str> {
    let timeout = deadline
        .remaining()
        .map_err(|_| "timeout")?
        .min(PROJECT_TIMEOUT);
    let ctx = open_query(state, project.org_id, project.id, None)
        .await
        .map_err(|_| "open")?;
    let query = activity_query();
    let data = storage::widget_query(
        &ctx,
        storage::WidgetRequest {
            window,
            bucket: storage::Bucket::Fixed(storage::BucketWidth::H1),
            dashboard_filter: None,
            group_by: None,
            include_eval: false,
            include_experiments: false,
            query: &query,
            shape: storage::Shape::Series,
            compare: false,
            timeout,
        },
    )
    .await
    .map_err(|error| match error {
        storage::WidgetError::Run(storage::RunError::Timeout) => "timeout",
        storage::WidgetError::Run(storage::RunError::Busy) => "busy",
        _ => "query",
    })?;
    Ok(activity_of(
        project.id,
        window.from(),
        &data,
        state.last_ingest.get(project.id),
    ))
}

/// Keeps the request order. A failed project is listed in `unavailable` (OH-4) and logged at debug
/// with its id and reason; the request logs once at warn with the count of such projects.
fn split(
    results: Vec<(ProjectId, Result<ProjectActivity, &'static str>)>,
) -> (Vec<ProjectActivity>, Vec<ProjectId>) {
    let mut data = Vec::new();
    let mut unavailable = Vec::new();
    for (id, outcome) in results {
        match outcome {
            Ok(activity) => data.push(activity),
            Err(reason) => {
                tracing::debug!(project_id = %id, reason, "project activity failed");
                unavailable.push(id);
            }
        }
    }
    if !unavailable.is_empty() {
        tracing::warn!(
            unavailable = unavailable.len(),
            "project activity unavailable for some projects"
        );
    }
    (data, unavailable)
}

/// `GET /orgs/{org}/projects/activity` (D-C-27).
async fn get_activity(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    error::Query(params): error::Query<ProjectActivityParams>,
) -> Result<error::Json<ProjectsActivity>, ApiError> {
    let user = principal::session_user(&principal)?;
    let ids = parse_ids(&params.ids)?;
    let mut visible: HashMap<ProjectId, ProjectRow> = visible_projects(&state, &access, user.id)
        .await?
        .into_iter()
        .map(|(project, _)| (project.id, project))
        .collect();
    let wanted: Vec<ProjectRow> = ids.iter().filter_map(|id| visible.remove(id)).collect();
    let window = activity_window(state.clock.now_ns())?;
    let deadline = Deadline::start();
    let state_ref = &state;
    let results = futures::stream::iter(wanted)
        .map(|project| {
            Box::pin(async move {
                let outcome = project_activity(state_ref, &project, window, deadline).await;
                (project.id, outcome)
            })
        })
        .buffered(CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    let (data, unavailable) = split(results);
    Ok(error::Json(ProjectsActivity {
        from: window.from(),
        to: window.to(),
        data,
        unavailable,
    }))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use axum::http::{Method, header};
    use serde_json::{Value, json};

    use super::*;
    use crate::api::account::OrgRole;
    use crate::auth::session::{SessionOrigin, start_session};
    use crate::meta::users::{EmailVerifiedVia, NewUser, insert_user};
    use crate::test_support::{TestApp, TestResponse, dataset_span, second_org};
    use cetra_core::enums::SpanStatus;
    use cetra_core::ids::{SpanId, TraceId};
    use cetra_storage::{WidgetPoint, WidgetSeries};

    /// 2026-01-01T11:30:00Z.
    const T: i64 = 1_767_267_000_000_000_000;
    /// 2025-12-31T12:00:00Z, the window's start.
    const FROM: i64 = 1_767_182_400_000_000_000;
    const MIN: i64 = 60_000_000_000;

    /// Commits one trace's root span. The environment is not reserved and the metadata has no
    /// experiment id, so the default exclusions keep it (spec 05 § Default exclusions).
    async fn commit(
        app: &TestApp,
        project: &ProjectRow,
        trace: &str,
        start: i64,
        error: bool,
        cost: Option<f64>,
    ) {
        let mut span = dataset_span(0);
        span.trace_id = TraceId::from_str(trace).unwrap();
        span.span_id = SpanId::from_str("c400000000000001").unwrap();
        span.parent_span_id = None;
        span.start_time = start;
        span.end_time = start + 1_000_000_000;
        span.ingest_time = app.state.clock.now_ns();
        span.environment = Some("production".to_owned());
        span.metadata.clear();
        span.session_id = None;
        span.status = if error {
            SpanStatus::Error
        } else {
            SpanStatus::Ok
        };
        span.cost_input = None;
        span.cost_output = None;
        span.cost_total = cost;
        app.commit_spans(project, vec![span]).await;
    }

    async fn get(app: &TestApp, cookie: &str, query: &str) -> TestResponse {
        app.request(
            Method::GET,
            &format!("/api/v1/orgs/acme/projects/activity{query}"),
        )
        .header(header::COOKIE.as_str(), cookie)
        .send()
        .await
    }

    fn random_ids(count: usize) -> Vec<String> {
        (0..count)
            .map(|_| ProjectId::new_v7().to_string())
            .collect()
    }

    fn activity(id: ProjectId) -> ProjectActivity {
        ProjectActivity {
            project_id: id,
            traces: 0,
            errors: 0,
            cost_usd: 0.0,
            buckets: vec![
                ActivityBucket {
                    traces: 0,
                    errors: 0
                };
                BUCKETS
            ],
            last_ingest_at: None,
        }
    }

    fn point_series(measure: usize, time: i64, value: Option<f64>) -> WidgetSeries {
        WidgetSeries {
            measure,
            group: None,
            other: false,
            points: vec![WidgetPoint {
                time,
                value,
                rows: 1,
            }],
        }
    }

    /// A signed-in user who is not a member of any org; returns the session cookie.
    async fn outsider(app: &TestApp) -> String {
        let now = app.state.clock.now_ns();
        let user = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_user(
                        conn,
                        NewUser {
                            email: "outsider@example.com".to_owned(),
                            name: None,
                            password_hash: None,
                            email_verified_at: Some(now),
                            email_verified_via: Some(EmailVerifiedVia::MailToken),
                            is_instance_admin: false,
                        },
                        now,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        let set_cookie = start_session(&app.state, user.id, None, None, SessionOrigin::Password)
            .await
            .unwrap();
        set_cookie
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }

    fn ids_of(body: &Value) -> Vec<String> {
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["project_id"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn activity_counts_traces_errors_and_cost_by_hour() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.state.clock.set(T);
        let project = &seeded.project;
        commit(
            &app,
            project,
            "a7000000000000000000000000000001",
            FROM + 10 * MIN,
            false,
            Some(0.5),
        )
        .await;
        commit(
            &app,
            project,
            "a7000000000000000000000000000002",
            FROM + 21 * 60 * MIN + 5 * MIN,
            true,
            Some(0.25),
        )
        .await;
        commit(
            &app,
            project,
            "a7000000000000000000000000000003",
            FROM + 21 * 60 * MIN + 40 * MIN,
            false,
            None,
        )
        .await;
        commit(
            &app,
            project,
            "a7000000000000000000000000000004",
            T - 1_000_000_000,
            true,
            Some(1.0),
        )
        .await;
        commit(
            &app,
            project,
            "a7000000000000000000000000000005",
            FROM - 1_000_000_000,
            false,
            Some(9.0),
        )
        .await;

        let id = project.id.to_string();
        let response = get(&app, &seeded.cookie, &format!("?ids={id}")).await;
        assert_eq!(response.status.as_u16(), 200);
        let body: Value = response.json();
        assert_eq!(body["from"], "2025-12-31T12:00:00.000000000Z");
        assert_eq!(body["to"], "2026-01-01T11:30:00.000000001Z");
        let data = &body["data"][0];
        assert_eq!(data["traces"], 4);
        assert_eq!(data["errors"], 2);
        assert_eq!(data["cost_usd"].as_f64(), Some(1.75));
        let buckets = data["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 24);
        for (index, bucket) in buckets.iter().enumerate() {
            let expected = match index {
                0 => json!({"traces": 1, "errors": 0}),
                21 => json!({"traces": 2, "errors": 1}),
                23 => json!({"traces": 1, "errors": 1}),
                _ => json!({"traces": 0, "errors": 0}),
            };
            assert_eq!(bucket, &expected, "bucket {index}");
        }
        assert_eq!(body["unavailable"], json!([]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_current_hour_is_the_last_bucket_even_on_the_hour() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = T + 30 * MIN;
        app.state.clock.set(now);
        commit(
            &app,
            &seeded.project,
            "a7000000000000000000000000000010",
            now,
            false,
            None,
        )
        .await;

        let id = seeded.project.id.to_string();
        let response = get(&app, &seeded.cookie, &format!("?ids={id}")).await;
        assert_eq!(response.status.as_u16(), 200);
        let body: Value = response.json();
        assert_eq!(body["from"], "2025-12-31T13:00:00.000000000Z");
        assert_eq!(body["data"][0]["buckets"][23]["traces"], 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answers_follow_request_order_and_skip_unknown_ids() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.state.clock.set(T);
        let (playground, _) = app.add_project(&seeded, "playground", None).await;
        let (other_org, _) = second_org(&app, "other", "owner@other.test").await;
        let now = app.state.clock.now_ns();
        let foreign = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    crate::meta::projects::insert_project(
                        conn, other_org, "Other", "other", None, now,
                    )
                    .await
                })
            })
            .await
            .unwrap();

        let query = format!(
            "?ids={},{},{},{}",
            playground.id,
            ProjectId::new_v7(),
            foreign.id,
            seeded.project.id
        );
        let response = get(&app, &seeded.cookie, &query).await;
        assert_eq!(response.status.as_u16(), 200);
        let body: Value = response.json();
        assert_eq!(
            ids_of(&body),
            vec![playground.id.to_string(), seeded.project.id.to_string()]
        );
        assert_eq!(body["unavailable"], json!([]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hidden_projects_are_left_out() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.state.clock.set(T);
        let bob = app
            .add_member(&seeded, "bob@example.com", OrgRole::Member)
            .await;
        let id = seeded.project.id.to_string();

        let response = get(&app, &bob.cookie, &format!("?ids={id}")).await;
        assert_eq!(response.status.as_u16(), 200);
        assert_eq!(ids_of(&response.json()), vec![id.clone()]);

        let (project_id, user_id) = (seeded.project.id, bob.id);
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    crate::meta::members::set_project_override(
                        conn,
                        project_id,
                        user_id,
                        crate::auth::roles::ProjectOverride::None,
                    )
                    .await
                })
            })
            .await
            .unwrap();

        let response = get(&app, &bob.cookie, &format!("?ids={id}")).await;
        assert_eq!(response.status.as_u16(), 200);
        assert_eq!(ids_of(&response.json()), Vec::<String>::new());

        let listed = app
            .request(Method::GET, "/api/v1/orgs/acme/projects")
            .header(header::COOKIE.as_str(), &bob.cookie)
            .send()
            .await;
        let page: Value = listed.json();
        assert_eq!(page["data"], json!([]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn last_ingest_at_is_present_only_when_known() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.state.clock.set(T);
        let (playground, _) = app.add_project(&seeded, "playground", None).await;
        app.state.last_ingest.record(seeded.project.id, T - 5 * MIN);

        let query = format!("?ids={},{}", seeded.project.id, playground.id);
        let response = get(&app, &seeded.cookie, &query).await;
        assert_eq!(response.status.as_u16(), 200);
        let body: Value = response.json();
        assert_eq!(
            body["data"][0]["last_ingest_at"],
            "2026-01-01T11:25:00.000000000Z"
        );
        assert!(body["data"][1].get("last_ingest_at").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_project_without_traces_has_zero_buckets() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.state.clock.set(T);
        let (empty, _) = app.add_project(&seeded, "empty", None).await;

        let response = get(&app, &seeded.cookie, &format!("?ids={}", empty.id)).await;
        assert_eq!(response.status.as_u16(), 200);
        let body: Value = response.json();
        let data = &body["data"][0];
        assert_eq!(data["traces"], 0);
        assert_eq!(data["cost_usd"].as_f64(), Some(0.0));
        let buckets = data["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 24);
        assert!(
            buckets
                .iter()
                .all(|b| b == &json!({"traces": 0, "errors": 0}))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bad_ids_are_refused() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let id = seeded.project.id.to_string();
        let too_many = random_ids(51).join(",");
        let cases = [
            "?ids=".to_owned(),
            "?ids=nope".to_owned(),
            format!("?ids={id},{id}"),
            format!("?ids={too_many}"),
            format!("?ids={id}&foo=1"),
            String::new(),
        ];
        for query in cases {
            let response = get(&app, &seeded.cookie, &query).await;
            assert_eq!(response.status.as_u16(), 400, "query {query:.40}");
            if query.starts_with("?ids=") && !query.contains("foo") {
                let problem = response.problem();
                assert_eq!(problem.errors.len(), 1);
                assert_eq!(problem.errors[0].field, "ids");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fifty_ids_are_allowed() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let mut ids = random_ids(49);
        ids.push(seeded.project.id.to_string());
        let response = get(&app, &seeded.cookie, &format!("?ids={}", ids.join(","))).await;
        assert_eq!(response.status.as_u16(), 200);
        assert_eq!(ids_of(&response.json()).len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn outsiders_get_404() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let cookie = outsider(&app).await;
        let response = get(&app, &cookie, &format!("?ids={}", seeded.project.id)).await;
        assert_eq!(response.status.as_u16(), 404);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_api_key_gets_404() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let response = app
            .request(
                Method::GET,
                &format!(
                    "/api/v1/orgs/acme/projects/activity?ids={}",
                    seeded.project.id
                ),
            )
            .header(
                header::AUTHORIZATION.as_str(),
                &format!("Bearer {}", seeded.read_key),
            )
            .send()
            .await;
        assert_eq!(response.status.as_u16(), 404);
    }

    #[test]
    fn failed_projects_are_listed_as_unavailable_in_request_order() {
        let (a, b, c) = (
            ProjectId::new_v7(),
            ProjectId::new_v7(),
            ProjectId::new_v7(),
        );
        let (data, unavailable) = split(vec![
            (a, Ok(activity(a))),
            (b, Err("timeout")),
            (c, Ok(activity(c))),
        ]);
        let ids: Vec<ProjectId> = data.iter().map(|entry| entry.project_id).collect();
        assert_eq!(ids, vec![a, c]);
        assert_eq!(unavailable, vec![b]);
    }

    #[test]
    fn the_project_timeout_is_under_the_request_deadline() {
        assert!(PROJECT_TIMEOUT < crate::routes::read::READ_TIMEOUT);
        assert!(PROJECT_TIMEOUT > Duration::ZERO);
    }

    #[test]
    fn points_off_the_grid_are_ignored() {
        let from = FROM;
        let data = storage::WidgetData::Series {
            bucket_ns: HOUR_NS,
            series: vec![
                point_series(0, from - HOUR_NS, Some(9.0)),
                point_series(0, from + 24 * HOUR_NS, Some(9.0)),
                point_series(0, from + HOUR_NS / 2, Some(9.0)),
                point_series(0, from + 2 * HOUR_NS, None),
                point_series(1, from + 2 * HOUR_NS, Some(3.0)),
                point_series(0, from + 5 * HOUR_NS, Some(4.0)),
                point_series(0, from + 6 * HOUR_NS, Some(2.0)),
                point_series(1, from + 6 * HOUR_NS, Some(0.0)),
            ],
        };
        let activity = activity_of(ProjectId::new_v7(), from, &data, None);
        assert_eq!(
            activity.buckets[2],
            ActivityBucket {
                traces: 0,
                errors: 3
            }
        );
        assert_eq!(
            activity.buckets[5],
            ActivityBucket {
                traces: 4,
                errors: 0
            }
        );
        assert_eq!(
            activity.buckets[6],
            ActivityBucket {
                traces: 2,
                errors: 0
            }
        );
        assert_eq!(activity.traces, 6);
        assert_eq!(activity.errors, 3);
    }
}
