//! `POST /api/v1/projects/{project}/issues/{id}/dataset-items` (D-IC-22): copies the newest
//! occurrence traces of an issue group into a dataset, through the bulk-add core of datasets
//! (D-J-25). The group is the issue and the seven merged issues with the most occurrences
//! (`example_issues`, D-IC-9).

use std::collections::HashSet;

use axum::extract::{Extension, State};
use axum::http::{Method, StatusCode};
use axum::routing::post;
use cetra_core::ids::TraceId;
use cetra_core::score::ScoreTarget;
use cetra_storage::TimeWindow;
use serde::Deserialize;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::analytics::actor::Actor;
use crate::analytics::catalog::{Event, ItemSource};
use crate::analytics::record;
use crate::api::common::FieldError;
use crate::api::datasets::{FromTracesResult, IssueDatasetItemsRequest, TraceRef};
use crate::auth::principal::Principal;
use crate::auth::roles::ProjectPerm;
use crate::eval::rule_check::field_error;
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::issues::read::{
    OCCURRENCE_DEFAULT_PERIOD_NS, example_issues, names_of, occurrence_scores, tallies,
};
use crate::meta::issues as store;
use crate::routes::datasets::add_trace_refs;
use crate::routes::read::open_query;

/// The most traces one call adds, and the default (D-IC-22).
const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 100;
/// The span id metadata key of an occurrence (D-IC-18).
const SPAN_KEY: &str = "cetra.issue.span_id";
/// The metadata key every copied item carries: the requested issue (D-IC-22).
const ISSUE_KEY: &str = "cetra.issue_id";

#[derive(Debug, Deserialize)]
struct IssuePath {
    id: Uuid,
}

pub(crate) fn routes() -> Vec<RouteDef> {
    vec![RouteDef {
        method: Method::POST,
        path: "/api/v1/projects/{project}/issues/{id}/dataset-items",
        access: Access::Project {
            perm: ProjectPerm::ManageDatasets,
            session_only: false,
        },
        documented: true,
        handler: post(add_issue_examples),
    }]
}

/// `POST …/issues/{id}/dataset-items` (D-IC-22): the newest occurrence of each distinct trace of
/// the issue group, up to `limit`, copied into the dataset as one new version. A group without a
/// trace in the data window is a 400 (Q3).
async fn add_issue_examples(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IssuePath { id }): error::Path<IssuePath>,
    error::Json(body): error::Json<IssueDatasetItemsRequest>,
) -> Result<(StatusCode, error::Json<FromTracesResult>), ApiError> {
    let limit = body.limit.unwrap_or(DEFAULT_LIMIT);
    let mut errors = Vec::new();
    if !(1..=MAX_LIMIT).contains(&limit) {
        errors.push(field_error("limit", "must be between 1 and 100"));
    }
    if body.dataset.trim().is_empty() {
        errors.push(field_error("dataset", "must not be empty"));
    }
    if !errors.is_empty() {
        return Err(bad_request(errors));
    }

    let project = access.project.id;
    let org = access.project.org_id;
    store::get_issue(state.meta.reader(), project, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let now = state.clock.now_ns();
    let window = TimeWindow::new(now.saturating_sub(OCCURRENCE_DEFAULT_PERIOD_NS), now)?;
    let merged = store::merged_ids(state.meta.reader(), project, id).await?;
    let names = names_of(id, &merged);
    let counts = tallies(&state, org, project, &names, window).await?;
    let group = example_issues(id, &merged, &counts);

    let per_issue = usize::try_from(limit).unwrap_or(usize::MAX);
    let ctx = open_query(&state, org, project, None).await?;
    let scored = occurrence_scores(&ctx, project, &group, per_issue, Some(window)).await?;
    let mut found: Vec<(i64, TraceId, Option<String>)> = Vec::new();
    for (_, score) in scored {
        if let ScoreTarget::Trace { trace_id } = score.target {
            found.push((
                score.created_at,
                trace_id,
                score.metadata.get(SPAN_KEY).cloned(),
            ));
        }
    }
    // The newest occurrence of each trace, newest first (the trace id breaks ties).
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut seen = HashSet::new();
    let refs: Vec<TraceRef> = found
        .into_iter()
        .filter(|(_, trace, _)| seen.insert(*trace))
        .take(per_issue)
        .map(|(_, trace, span)| TraceRef {
            trace_id: trace.to_string(),
            span_id: span,
        })
        .collect();
    if refs.is_empty() {
        return Err(bad_request(vec![field_error(
            "issue",
            "the issue has no occurrences in the data window",
        )]));
    }

    let mut metadata = Map::new();
    metadata.insert(ISSUE_KEY.to_owned(), Value::String(id.to_string()));
    let result =
        add_trace_refs(&state, &principal, &access, &body.dataset, &refs, &metadata).await?;
    record(
        &state,
        &Event::DatasetItemsAdded {
            source: ItemSource::Issue,
        },
        &Actor::project(&principal, &access),
    );
    Ok((StatusCode::CREATED, error::Json(result)))
}

fn bad_request(errors: Vec<FieldError>) -> ApiError {
    ApiError::BadRequest {
        detail: None,
        errors,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use axum::http::Method;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use crate::auth::roles::OrgRole;
    use crate::meta::issues::{IssueRow, IssueState, SourceKind};
    use crate::test_support::issues::{issue_row, occurrence, put_issue, put_source, source_row};
    use crate::test_support::{Seeded, TestApp, TestResponse, dataset_span};

    const HOUR_NS: i64 = 3_600_000_000_000;

    /// A trace id of `n`: 32 hex digits, distinct for each `n`.
    fn trace(n: u32) -> String {
        format!("a1{n:030x}")
    }

    /// A span id of `n`: 16 hex digits.
    fn span(n: u32) -> String {
        format!("b1{n:014x}")
    }

    fn path(seeded: &Seeded, issue: Uuid) -> String {
        format!(
            "/api/v1/projects/{}/issues/{issue}/dataset-items",
            seeded.project.id
        )
    }

    async fn add(app: &TestApp, seeded: &Seeded, issue: Uuid, body: Value) -> TestResponse {
        app.request(Method::POST, &path(seeded, issue))
            .header("cookie", &seeded.cookie)
            .json(&body)
            .send()
            .await
    }

    async fn add_ok(app: &TestApp, seeded: &Seeded, issue: Uuid, body: Value) -> Value {
        let response = add(app, seeded, issue, body).await;
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        response.json()
    }

    async fn create_dataset(app: &TestApp, seeded: &Seeded, name: &str) {
        let response = app
            .request(
                Method::POST,
                &format!("/api/v1/projects/{}/datasets", seeded.project.id),
            )
            .header("cookie", &seeded.cookie)
            .json(&json!({ "name": name }))
            .send()
            .await;
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
    }

    /// The `(field, message)` pairs of a 400 answer.
    fn field_errors(response: &TestResponse) -> Vec<(String, String)> {
        assert_eq!(
            response.status,
            400,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let body: Value = response.json();
        body["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|error| {
                (
                    error["field"].as_str().unwrap().to_owned(),
                    error["message"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    /// The source trace ids of the items of an answer, in order.
    fn trace_ids(result: &Value) -> Vec<String> {
        result["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["source_trace_id"].as_str().unwrap().to_owned())
            .collect()
    }

    /// An error source of the project, returned with its id.
    async fn source(app: &TestApp, seeded: &Seeded) -> Uuid {
        let row = source_row(
            seeded.project.id,
            "Errors",
            SourceKind::Error,
            true,
            app.state.clock.now_ns(),
        );
        let id = row.id;
        put_source(app, row).await;
        id
    }

    async fn issue(app: &TestApp, seeded: &Seeded, source: Uuid, state: IssueState) -> Uuid {
        let id = Uuid::now_v7();
        let now = app.state.clock.now_ns();
        put_issue(
            app,
            issue_row(seeded.project.id, source, id, "issue", state, now),
        )
        .await;
        id
    }

    /// An issue merged into `into`, folded into it (its occurrences count on the target).
    async fn merged(app: &TestApp, seeded: &Seeded, source: Uuid, into: Uuid) -> Uuid {
        let id = Uuid::now_v7();
        let now = app.state.clock.now_ns();
        let row = IssueRow {
            merged_into: Some(into),
            ..issue_row(
                seeded.project.id,
                source,
                id,
                "merged",
                IssueState::Merged,
                now,
            )
        };
        put_issue(app, row).await;
        id
    }

    /// One occurrence of `issue` on `trace`, `hours_ago` hours before now.
    async fn occur(
        app: &TestApp,
        seeded: &Seeded,
        source: Uuid,
        issue: Uuid,
        trace: &str,
        hours_ago: i64,
        span: Option<&str>,
    ) {
        let at = app.state.clock.now_ns() - hours_ago * HOUR_NS;
        occurrence(app, seeded, source, issue, trace, at, None, span).await;
    }

    /// Commits a one-span trace whose root is `span_hex`, so the dataset route can load it.
    async fn commit_trace(app: &TestApp, seeded: &Seeded, trace_hex: &str, span_hex: &str) {
        let now = app.state.clock.now_ns();
        let mut span = dataset_span(0);
        span.trace_id = trace_hex.parse().unwrap();
        span.span_id = span_hex.parse().unwrap();
        span.parent_span_id = None;
        span.start_time = now;
        span.end_time = now + 1_000_000_000;
        span.ingest_time = now;
        app.commit_spans(&seeded.project, vec![span]).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_occurrence_older_than_the_default_period_is_not_copied() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_dataset(&app, &seeded, "faq").await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        commit_trace(&app, &seeded, &trace(1), &span(1)).await;
        commit_trace(&app, &seeded, &trace(2), &span(2)).await;
        occur(&app, &seeded, src, target, &trace(1), 20 * 24, None).await;
        occur(&app, &seeded, src, target, &trace(2), 2, None).await;

        let result = add_ok(&app, &seeded, target, json!({ "dataset": "faq" })).await;

        assert_eq!(trace_ids(&result), vec![trace(2)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn adds_the_newest_distinct_traces() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_dataset(&app, &seeded, "faq").await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        let other = merged(&app, &seeded, src, target).await;
        commit_trace(&app, &seeded, &trace(1), &span(1)).await;
        commit_trace(&app, &seeded, &trace(2), &span(2)).await;
        // Trace 1 has two occurrences, of the target and of a merged issue; trace 2 has one.
        occur(&app, &seeded, src, target, &trace(1), 3, None).await;
        occur(&app, &seeded, src, other, &trace(1), 1, None).await;
        occur(&app, &seeded, src, target, &trace(2), 2, None).await;

        let result = add_ok(&app, &seeded, target, json!({ "dataset": "faq" })).await;

        assert_eq!(result["version"]["version"], 1);
        assert_eq!(result["version"]["added"], 2);
        assert_eq!(trace_ids(&result), vec![trace(1), trace(2)]);
        assert_eq!(result["items"][0]["source_span_id"], span(1));
        assert_eq!(result["skipped"], json!([]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn limit_caps_the_items() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_dataset(&app, &seeded, "faq").await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        for n in 1..=5 {
            commit_trace(&app, &seeded, &trace(n), &span(n)).await;
            occur(&app, &seeded, src, target, &trace(n), i64::from(n), None).await;
        }

        let two = add_ok(
            &app,
            &seeded,
            target,
            json!({ "dataset": "faq", "limit": 2 }),
        )
        .await;
        assert_eq!(trace_ids(&two), vec![trace(1), trace(2)]);
        let all = add_ok(&app, &seeded, target, json!({ "dataset": "faq" })).await;
        assert_eq!(trace_ids(&all).len(), 5);

        let low = add(
            &app,
            &seeded,
            target,
            json!({ "dataset": "faq", "limit": 0 }),
        )
        .await;
        assert_eq!(
            field_errors(&low),
            vec![("limit".to_owned(), "must be between 1 and 100".to_owned())]
        );
        let high = add(
            &app,
            &seeded,
            target,
            json!({ "dataset": "faq", "limit": 101 }),
        )
        .await;
        assert_eq!(
            field_errors(&high),
            vec![("limit".to_owned(), "must be between 1 and 100".to_owned())]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn uses_the_occurrence_span_when_present() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.load_example_dataset(seeded.project.id).await;
        create_dataset(&app, &seeded, "faq").await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        // Trace 1 of the example dataset has a child span with an input; the occurrence names it.
        occur(
            &app,
            &seeded,
            src,
            target,
            &trace(1),
            1,
            Some("b100000000000012"),
        )
        .await;
        commit_trace(&app, &seeded, &trace(7), &span(7)).await;
        occur(&app, &seeded, src, target, &trace(7), 2, None).await;

        let result = add_ok(&app, &seeded, target, json!({ "dataset": "faq" })).await;

        assert_eq!(trace_ids(&result), vec![trace(1), trace(7)]);
        assert_eq!(result["items"][0]["source_span_id"], "b100000000000012");
        assert_eq!(result["items"][1]["source_span_id"], span(7));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn items_carry_the_issue_id_metadata() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_dataset(&app, &seeded, "faq").await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        let other = merged(&app, &seeded, src, target).await;
        commit_trace(&app, &seeded, &trace(1), &span(1)).await;
        commit_trace(&app, &seeded, &trace(2), &span(2)).await;
        occur(&app, &seeded, src, other, &trace(1), 1, None).await;
        occur(&app, &seeded, src, target, &trace(2), 2, None).await;

        let result = add_ok(&app, &seeded, target, json!({ "dataset": "faq" })).await;

        // The occurrence of the merged issue is added for the target, with the target's id.
        assert_eq!(trace_ids(&result), vec![trace(1), trace(2)]);
        let expected = json!({ "cetra.issue_id": target.to_string() });
        assert_eq!(result["items"][0]["metadata"], expected);
        assert_eq!(result["items"][1]["metadata"], expected);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_group_is_the_issue_and_the_seven_busiest_merged_issues() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_dataset(&app, &seeded, "faq").await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        commit_trace(&app, &seeded, &trace(1000), &span(1000)).await;
        occur(&app, &seeded, src, target, &trace(1000), 1, None).await;

        // Merged issue `i` (0 to 8) has `i + 1` occurrences in the window, each on its own trace.
        let mut expected: HashSet<String> = HashSet::from([trace(1000)]);
        let mut merged_ids = Vec::new();
        for i in 0..9u32 {
            let issue_id = merged(&app, &seeded, src, target).await;
            merged_ids.push(issue_id);
            for k in 0..=i {
                let n = 2000 + i * 100 + k;
                commit_trace(&app, &seeded, &trace(n), &span(n)).await;
                occur(
                    &app,
                    &seeded,
                    src,
                    issue_id,
                    &trace(n),
                    i64::from(k) + 1,
                    None,
                )
                .await;
                if i >= 2 {
                    expected.insert(trace(n));
                }
            }
        }
        // The least busy merged issue has ten more occurrences, all older than the window: they
        // do not count toward the ranking, so it stays out of the group.
        for hours in 0..10 {
            occur(
                &app,
                &seeded,
                src,
                merged_ids[0],
                &trace(3000),
                400 * 24 + hours,
                None,
            )
            .await;
        }

        let result = add_ok(&app, &seeded, target, json!({ "dataset": "faq" })).await;

        let got: HashSet<String> = trace_ids(&result).into_iter().collect();
        assert_eq!(result["items"].as_array().unwrap().len(), 43);
        assert_eq!(got, expected);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn skipped_traces_are_listed() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_dataset(&app, &seeded, "faq").await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        commit_trace(&app, &seeded, &trace(3001), &span(3001)).await;
        // The newest occurrence names a trace that has no spans in storage.
        occur(&app, &seeded, src, target, &trace(3000), 1, None).await;
        occur(&app, &seeded, src, target, &trace(3001), 2, None).await;

        let result = add_ok(&app, &seeded, target, json!({ "dataset": "faq" })).await;

        assert_eq!(trace_ids(&result), vec![trace(3001)]);
        let skipped = result["skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0]["index"], 0);
        assert_eq!(skipped[0]["trace_id"], trace(3000));
        assert_eq!(skipped[0]["message"], "the trace or span was not found");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn when_every_trace_is_skipped_the_answer_is_the_bulk_add_400() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_dataset(&app, &seeded, "faq").await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        occur(&app, &seeded, src, target, &trace(3000), 1, None).await;

        let response = add(&app, &seeded, target, json!({ "dataset": "faq" })).await;

        assert_eq!(
            field_errors(&response),
            vec![(
                "items[0]".to_owned(),
                "the trace or span was not found".to_owned()
            )]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn any_state_is_allowed() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_dataset(&app, &seeded, "faq").await;
        let src = source(&app, &seeded).await;
        commit_trace(&app, &seeded, &trace(1), &span(1)).await;
        for state in [IssueState::Resolved, IssueState::Ignored] {
            let target = issue(&app, &seeded, src, state).await;
            occur(&app, &seeded, src, target, &trace(1), 1, None).await;

            let result = add_ok(&app, &seeded, target, json!({ "dataset": "faq" })).await;

            assert_eq!(trace_ids(&result), vec![trace(1)]);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_issue_is_404() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_dataset(&app, &seeded, "faq").await;

        let response = add(&app, &seeded, Uuid::now_v7(), json!({ "dataset": "faq" })).await;

        assert_eq!(response.status, 404);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_issue_of_another_project_is_404() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_dataset(&app, &seeded, "faq").await;
        let (other, _key) = app.add_project(&seeded, "playground", None).await;
        let now = app.state.clock.now_ns();
        let src = source_row(other.id, "Errors", SourceKind::Error, true, now);
        let src_id = src.id;
        put_source(&app, src).await;
        let id = Uuid::now_v7();
        put_issue(
            &app,
            issue_row(other.id, src_id, id, "elsewhere", IssueState::Open, now),
        )
        .await;

        let response = add(&app, &seeded, id, json!({ "dataset": "faq" })).await;

        assert_eq!(response.status, 404);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_issue_without_occurrences_is_400() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_dataset(&app, &seeded, "faq").await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;

        let response = add(&app, &seeded, target, json!({ "dataset": "faq" })).await;

        assert_eq!(
            field_errors(&response),
            vec![(
                "issue".to_owned(),
                "the issue has no occurrences in the data window".to_owned()
            )]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_null_limit_is_400() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_dataset(&app, &seeded, "faq").await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;

        let response = add(
            &app,
            &seeded,
            target,
            json!({ "dataset": "faq", "limit": null }),
        )
        .await;

        assert_eq!(response.status, 400);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_dataset_by_name_works_and_an_unknown_dataset_is_404() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_dataset(&app, &seeded, "faq").await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        commit_trace(&app, &seeded, &trace(1), &span(1)).await;
        occur(&app, &seeded, src, target, &trace(1), 1, None).await;

        let blank = add(&app, &seeded, target, json!({ "dataset": "  " })).await;
        assert_eq!(
            field_errors(&blank),
            vec![("dataset".to_owned(), "must not be empty".to_owned())]
        );
        let unknown = add(&app, &seeded, target, json!({ "dataset": "nope" })).await;
        assert_eq!(unknown.status, 404);
        add_ok(&app, &seeded, target, json!({ "dataset": "faq" })).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_viewer_and_a_read_key_cannot_add() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_dataset(&app, &seeded, "faq").await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        commit_trace(&app, &seeded, &trace(1), &span(1)).await;
        occur(&app, &seeded, src, target, &trace(1), 1, None).await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;

        let as_viewer = app
            .request(Method::POST, &path(&seeded, target))
            .header("cookie", &viewer.cookie)
            .json(&json!({ "dataset": "faq" }))
            .send()
            .await;
        assert_eq!(as_viewer.status, 403);
        let as_key = app
            .request(Method::POST, &path(&seeded, target))
            .header("authorization", &format!("Bearer {}", seeded.read_key))
            .json(&json!({ "dataset": "faq" }))
            .send()
            .await;
        assert_eq!(as_key.status, 403);
    }
}
