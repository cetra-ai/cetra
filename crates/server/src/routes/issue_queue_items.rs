//! `POST /api/v1/projects/{project}/issues/{id}/queue-items` (D-IC-27 to D-IC-31): sends the newest
//! occurrences of an issue group to an annotation queue of any kind, through AQ's add core
//! (`add_found_targets`). The group is the one the dataset route copies (D-IC-22, D-IC-28).

use std::collections::HashSet;

use axum::extract::{Extension, State};
use axum::http::{Method, StatusCode};
use axum::routing::post;
use cetra_core::ids::{SpanId, TraceId};
use cetra_core::score::{Score, ScoreTarget};
use cetra_storage::TimeWindow;
use serde::Deserialize;
use uuid::Uuid;

use crate::api::annotation_queues::{AddedQueueItems, IssueQueueItemsRequest, SkippedQueueItem};
use crate::api::common::FieldError;
use crate::auth::principal::Principal;
use crate::auth::roles::ProjectPerm;
use crate::eval::rule_check::field_error;
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::issues::read::{
    OCCURRENCE_DEFAULT_PERIOD_NS, example_issues, names_of, occurrence_scores, tallies,
};
use crate::meta::annotation_queues::{self as queues, Target};
use crate::meta::issues as issues_store;
use crate::routes::annotation_queues::{
    MAX_SESSION_ID_BYTES, Parsed, add_found_targets, added_by_of, skip,
};
use crate::routes::read::open_query;

/// Default and most targets one call adds (D-IC-28). The most is AQ's by-id cap.
const DEFAULT_LIMIT: u32 = 100;
/// Equal to `cetra_storage::MAX_TARGET_IDS` and the storage page cap (D-AQ-8).
const MAX_LIMIT: u32 = 1_000;
/// The span id metadata key of an occurrence (D-IC-18).
const SPAN_KEY: &str = "cetra.issue.span_id";

#[derive(Debug, Deserialize)]
struct IssuePath {
    id: Uuid,
}

pub(crate) fn routes() -> Vec<RouteDef> {
    vec![RouteDef {
        method: Method::POST,
        path: "/api/v1/projects/{project}/issues/{id}/queue-items",
        access: Access::Project {
            perm: ProjectPerm::ManageQueues,
            session_only: false,
        },
        documented: true,
        handler: post(send_issue_to_queue),
    }]
}

/// `POST …/issues/{id}/queue-items` (D-IC-30): the targets of the newest occurrences of the issue
/// group, up to `limit`, added to the queue `queue` of the project. Answers 201 with AQ's counts
/// whatever they are. A group with no occurrence in the window is a 400 on `issue`.
async fn send_issue_to_queue(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IssuePath { id }): error::Path<IssuePath>,
    error::Json(body): error::Json<IssueQueueItemsRequest>,
) -> Result<(StatusCode, error::Json<AddedQueueItems>), ApiError> {
    let limit = body.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(bad_request(vec![field_error(
            "limit",
            "must be between 1 and 1000",
        )]));
    }

    let project = access.project.id;
    let org = access.project.org_id;
    issues_store::get_issue(state.meta.reader(), project, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let queue = queues::get(state.meta.reader(), project, body.queue)
        .await?
        .ok_or(ApiError::NotFound)?;
    let kind = queue.target;
    let added_by = added_by_of(&principal)?;

    let now = state.clock.now_ns();
    let window = TimeWindow::new(now.saturating_sub(OCCURRENCE_DEFAULT_PERIOD_NS), now)?;
    let merged = issues_store::merged_ids(state.meta.reader(), project, id).await?;
    let names = names_of(id, &merged);
    let counts = tallies(&state, org, project, &names, window).await?;
    let group = example_issues(id, &merged, &counts);

    let per_issue = usize::try_from(limit).unwrap_or(usize::MAX);
    let ctx = open_query(&state, org, project, None).await?;
    let pairs = occurrence_scores(&ctx, project, &group, per_issue, Some(window)).await?;
    let scored: Vec<Score> = pairs.into_iter().map(|(_, score)| score).collect();
    if scored.is_empty() {
        return Err(bad_request(vec![field_error(
            "issue",
            "the issue has no occurrences in the data window",
        )]));
    }

    let selection = select(kind, scored, per_issue);
    tracing::info!(
        project_id = %project,
        issue_id = %id,
        queue_id = %body.queue,
        scanned = selection.scanned,
        targets = selection.targets.len(),
        skipped = selection.skipped.len(),
        "issue occurrences sent to a queue"
    );
    // Nothing fits the queue's kind: AQ's answer with no insert (Q2).
    if selection.targets.is_empty() {
        return Ok((
            StatusCode::CREATED,
            error::Json(AddedQueueItems {
                added: 0,
                existing: 0,
                skipped: selection.skipped,
            }),
        ));
    }
    let answer = add_found_targets(
        &state,
        org,
        project,
        body.queue,
        kind,
        added_by,
        selection.targets,
        selection.skipped,
    )
    .await?;
    Ok((StatusCode::CREATED, error::Json(answer)))
}

fn bad_request(errors: Vec<FieldError>) -> ApiError {
    ApiError::BadRequest {
        detail: None,
        errors,
    }
}

/// The queue target of one occurrence, by the queue's kind (D-IC-29).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Pick {
    Trace(TraceId),
    Span(TraceId, SpanId),
    Session(String),
}

/// The queue target of `score` for a queue of `kind`, or the fixed reason it has none (Q8).
fn pick(kind: Target, score: &Score) -> Result<Pick, &'static str> {
    match kind {
        Target::Trace => match &score.target {
            ScoreTarget::Trace { trace_id } | ScoreTarget::Span { trace_id, .. } => {
                Ok(Pick::Trace(*trace_id))
            }
            ScoreTarget::Session { .. } => Err("the occurrence has no trace"),
        },
        Target::Span => match &score.target {
            ScoreTarget::Span { trace_id, span_id } => Ok(Pick::Span(*trace_id, *span_id)),
            ScoreTarget::Trace { trace_id } => score
                .metadata
                .get(SPAN_KEY)
                .and_then(|text| text.parse::<SpanId>().ok())
                .map(|span_id| Pick::Span(*trace_id, span_id))
                .ok_or("the occurrence has no span"),
            ScoreTarget::Session { .. } => Err("the occurrence has no span"),
        },
        Target::Session => {
            let session = match &score.target {
                ScoreTarget::Session { session_id } => Some(session_id.as_str()),
                ScoreTarget::Trace { .. } | ScoreTarget::Span { .. } => score.session_id.as_deref(),
            };
            match session {
                Some(id) if (1..=MAX_SESSION_ID_BYTES).contains(&id.len()) => {
                    Ok(Pick::Session(id.to_owned()))
                }
                _ => Err("the occurrence has no session"),
            }
        }
    }
}

/// What a call adds: the targets (newest first, one per target, at most the limit), the
/// occurrences skipped on the way, and how many occurrences the walk visited.
struct Selection {
    targets: Vec<Parsed>,
    skipped: Vec<SkippedQueueItem>,
    scanned: usize,
}

/// Walks `scored` newest first (`created_at`, then `score_id`, both descending) and picks each
/// occurrence's target for `kind`. An occurrence without one is skipped by its position; a target
/// already picked is dropped silently; the walk stops once `limit` targets are picked. Pure.
fn select(kind: Target, mut scored: Vec<Score>, limit: usize) -> Selection {
    scored.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.score_id.cmp(&a.score_id))
    });
    let mut selection = Selection {
        targets: Vec::new(),
        skipped: Vec::new(),
        scanned: 0,
    };
    let mut seen = HashSet::new();
    for (index, score) in scored.iter().enumerate() {
        if selection.targets.len() == limit {
            break;
        }
        selection.scanned += 1;
        match pick(kind, score) {
            Err(reason) => selection.skipped.push(skip(index, reason)),
            Ok(pick) => {
                if seen.insert(pick.clone()) {
                    selection.targets.push(parsed(index, pick));
                }
            }
        }
    }
    selection
}

/// The AQ add input of a picked target: only the fields of its kind are set (AQ's shape rule).
fn parsed(index: usize, pick: Pick) -> Parsed {
    match pick {
        Pick::Trace(trace) => Parsed {
            index,
            trace: Some(trace),
            span: None,
            session: None,
        },
        Pick::Span(trace, span) => Parsed {
            index,
            trace: Some(trace),
            span: Some(span),
            session: None,
        },
        Pick::Session(session) => Parsed {
            index,
            trace: None,
            span: None,
            session: Some(session),
        },
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use cetra_core::enums::{ScoreDataType, ScoreSource};
    use cetra_core::ids::ProjectId;
    use cetra_core::score::ScoreValue;
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

    use super::*;
    use crate::api::annotation_queues::{AnnotationQueue, AnnotationQueueItem, QueueItemAddedBy};
    use crate::api::common::Page;
    use crate::api::score_definitions::ScoreDirection;
    use crate::api::scores::ScoreCreate;
    use crate::auth::roles::OrgRole;
    use crate::meta::issues::{IssueRow, IssueState, SourceKind};
    use crate::meta::score_definitions::{DefinitionFields, insert_definition};
    use crate::scores::{self as score_writes, Writer};
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

    fn tid(n: u32) -> TraceId {
        trace(n).parse().unwrap()
    }

    fn sid(n: u32) -> SpanId {
        span(n).parse().unwrap()
    }

    /// A score for `select` and `pick`: the fields those read are the arguments, the others fixed.
    fn score(
        target: ScoreTarget,
        session: Option<&str>,
        metadata: &[(&str, &str)],
        created_at: i64,
        score_id: u128,
    ) -> Score {
        Score {
            project_id: ProjectId::new_v7(),
            score_id: Uuid::from_u128(score_id),
            target,
            session_id: session.map(str::to_owned),
            user_id: None,
            name: "issue:test".to_owned(),
            value: ScoreValue::Boolean(true),
            reason: None,
            source: ScoreSource::Issue,
            evaluator_id: None,
            rule_id: None,
            experiment_id: None,
            dataset_item_id: None,
            queue_id: None,
            author_user_id: None,
            metadata: metadata
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            created_at,
            updated_at: created_at,
            deleted: false,
            ingest_time: created_at,
        }
    }

    fn trace_score(n: u32, created_at: i64, score_id: u128) -> Score {
        score(
            ScoreTarget::Trace { trace_id: tid(n) },
            None,
            &[],
            created_at,
            score_id,
        )
    }

    #[test]
    fn pick_by_queue_kind() {
        let trace_occurrence = trace_score(1, 0, 1);
        let span_occurrence = score(
            ScoreTarget::Span {
                trace_id: tid(1),
                span_id: sid(1),
            },
            None,
            &[],
            0,
            1,
        );
        let session_occurrence = score(
            ScoreTarget::Session {
                session_id: "s1".to_owned(),
            },
            None,
            &[],
            0,
            1,
        );
        let keyed = score(
            ScoreTarget::Trace { trace_id: tid(1) },
            None,
            &[(SPAN_KEY, "b100000000000007")],
            0,
            1,
        );
        let unparsable = score(
            ScoreTarget::Trace { trace_id: tid(1) },
            None,
            &[(SPAN_KEY, "not-a-span")],
            0,
            1,
        );
        let in_session = score(
            ScoreTarget::Trace { trace_id: tid(1) },
            Some("s1"),
            &[],
            0,
            1,
        );
        let session_of =
            |session_id: String| score(ScoreTarget::Session { session_id }, None, &[], 0, 1);

        // A trace queue: a trace or span occurrence gives its trace; a session one has none.
        assert_eq!(
            pick(Target::Trace, &trace_occurrence),
            Ok(Pick::Trace(tid(1)))
        );
        assert_eq!(
            pick(Target::Trace, &span_occurrence),
            Ok(Pick::Trace(tid(1)))
        );
        assert_eq!(
            pick(Target::Trace, &session_occurrence),
            Err("the occurrence has no trace")
        );

        // A span queue: a span occurrence gives its span; a trace one the span key, when valid.
        assert_eq!(
            pick(Target::Span, &span_occurrence),
            Ok(Pick::Span(tid(1), sid(1)))
        );
        assert_eq!(pick(Target::Span, &keyed), Ok(Pick::Span(tid(1), sid(7))));
        assert_eq!(
            pick(Target::Span, &trace_occurrence),
            Err("the occurrence has no span")
        );
        assert_eq!(
            pick(Target::Span, &unparsable),
            Err("the occurrence has no span")
        );
        assert_eq!(
            pick(Target::Span, &session_occurrence),
            Err("the occurrence has no span")
        );

        // A session queue: a session occurrence gives its session; a trace or span one its
        // `session_id`, which must be 1 to 255 bytes.
        assert_eq!(
            pick(Target::Session, &session_occurrence),
            Ok(Pick::Session("s1".to_owned()))
        );
        assert_eq!(
            pick(Target::Session, &in_session),
            Ok(Pick::Session("s1".to_owned()))
        );
        assert_eq!(
            pick(Target::Session, &trace_occurrence),
            Err("the occurrence has no session")
        );
        assert_eq!(
            pick(Target::Session, &session_of(String::new())),
            Err("the occurrence has no session")
        );
        assert_eq!(
            pick(Target::Session, &session_of("s".repeat(256))),
            Err("the occurrence has no session")
        );
        assert_eq!(
            pick(Target::Session, &session_of("s".repeat(255))),
            Ok(Pick::Session("s".repeat(255)))
        );
    }

    #[test]
    fn select_takes_the_newest_per_target_and_stops_at_the_limit() {
        let scored: Vec<Score> = (1..=5)
            .map(|n| trace_score(n, i64::from(n), u128::from(n)))
            .collect();

        let selection = select(Target::Trace, scored, 2);

        let traces: Vec<Option<TraceId>> = selection
            .targets
            .iter()
            .map(|target| target.trace)
            .collect();
        assert_eq!(traces, vec![Some(tid(5)), Some(tid(4))]);
        assert_eq!(selection.scanned, 2);
        assert!(selection.skipped.is_empty());

        // Two occurrences of one trace give one target; the duplicate is not skipped.
        let duplicate = vec![
            trace_score(1, 2, 1),
            trace_score(1, 1, 2),
            trace_score(2, 0, 3),
        ];
        let selection = select(Target::Trace, duplicate, 10);
        let indexes: Vec<usize> = selection
            .targets
            .iter()
            .map(|target| target.index)
            .collect();
        assert_eq!(indexes, vec![0, 2]);
        assert!(selection.skipped.is_empty());
        assert_eq!(selection.scanned, 3);
    }

    #[test]
    fn select_lists_occurrences_without_the_target_by_position() {
        let occurrences = || {
            vec![
                score(
                    ScoreTarget::Span {
                        trace_id: tid(10),
                        span_id: sid(10),
                    },
                    None,
                    &[],
                    4,
                    4,
                ),
                trace_score(11, 3, 3),
                score(
                    ScoreTarget::Span {
                        trace_id: tid(12),
                        span_id: sid(12),
                    },
                    None,
                    &[],
                    2,
                    2,
                ),
                trace_score(13, 1, 1),
            ]
        };

        // Newest first: span (0), none (1), span (2), none (3). Two targets; 1 and 3 skipped.
        let selection = select(Target::Span, occurrences(), 10);
        let targets: Vec<Option<SpanId>> =
            selection.targets.iter().map(|target| target.span).collect();
        assert_eq!(targets, vec![Some(sid(10)), Some(sid(12))]);
        let skipped: Vec<u64> = selection.skipped.iter().map(|item| item.index).collect();
        assert_eq!(skipped, vec![1, 3]);
        assert_eq!(selection.skipped[0].message, "the occurrence has no span");

        // At a limit of 2 the walk stops at the second target: the occurrence after it is not
        // visited, so it is neither a target nor skipped.
        let selection = select(Target::Span, occurrences(), 2);
        assert_eq!(selection.targets.len(), 2);
        let skipped: Vec<u64> = selection.skipped.iter().map(|item| item.index).collect();
        assert_eq!(skipped, vec![1]);
        assert_eq!(selection.scanned, 3);
    }

    #[test]
    fn select_default_limit_is_the_callers() {
        let scored: Vec<Score> = (1..=101)
            .map(|n| trace_score(n, i64::from(n), u128::from(n)))
            .collect();

        let selection = select(Target::Trace, scored, 100);

        assert_eq!(selection.targets.len(), 100);
        assert_eq!(selection.scanned, 100);
    }

    #[test]
    fn select_orders_ties_by_score_id() {
        // Same creation time: the larger score id is the newer occurrence.
        let scored = vec![trace_score(1, 5, 1), trace_score(2, 5, 2)];

        let selection = select(Target::Trace, scored, 1);

        assert_eq!(selection.targets.len(), 1);
        assert_eq!(selection.targets[0].trace, Some(tid(2)));
    }

    fn base_path(seeded: &Seeded, issue: Uuid) -> String {
        format!(
            "/api/v1/projects/{}/issues/{issue}/queue-items",
            seeded.project.id
        )
    }

    fn queues_path(project: ProjectId) -> String {
        format!("/api/v1/projects/{project}/annotation-queues")
    }

    /// Sends `body` to the issue route of the seeded project as the owner.
    async fn send(app: &TestApp, seeded: &Seeded, issue: Uuid, body: Value) -> TestResponse {
        app.request(Method::POST, &base_path(seeded, issue))
            .header("cookie", &seeded.cookie)
            .json(&body)
            .send()
            .await
    }

    /// Sends `body` as the owner and checks the answer is 201.
    async fn send_ok(app: &TestApp, seeded: &Seeded, issue: Uuid, body: Value) -> AddedQueueItems {
        let response = send(app, seeded, issue, body).await;
        assert_eq!(
            response.status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        response.json()
    }

    /// The `(field, message)` pairs of a 400 answer.
    fn field_errors(response: &TestResponse) -> Vec<(String, String)> {
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
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

    /// Inserts a numeric score definition in `project` and returns its id.
    async fn define(app: &TestApp, project: ProjectId, name: &str) -> Uuid {
        let id = Uuid::now_v7();
        let now = app.state.clock.now_ns();
        let fields = DefinitionFields {
            name: name.to_owned(),
            categories: Vec::new(),
            min_value: Some(0.0),
            max_value: Some(1.0),
            description: None,
            direction: ScoreDirection::None,
            archived_at: None,
        };
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_definition(conn, id, project, ScoreDataType::Numeric, fields, now).await
                })
            })
            .await
            .unwrap();
        id
    }

    /// Creates a queue `name` of `target` (`trace`, `span` or `session`) in `project` as `cookie`.
    async fn create_queue(
        app: &TestApp,
        project: ProjectId,
        cookie: &str,
        name: &str,
        target: &str,
        definition: Uuid,
    ) -> AnnotationQueue {
        let response = app
            .request(Method::POST, &queues_path(project))
            .header("cookie", cookie)
            .json(&json!({"name": name, "target": target, "definition_ids": [definition]}))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::CREATED);
        response.json()
    }

    /// The items of `queue`, as the owner lists them.
    async fn items_of(app: &TestApp, seeded: &Seeded, queue: Uuid) -> Vec<AnnotationQueueItem> {
        let path = format!("{}/{queue}/items", queues_path(seeded.project.id));
        let response = app
            .request(Method::GET, &path)
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        let page: Page<AnnotationQueueItem> = response.json();
        page.data
    }

    /// Commits one span `span_hex` of trace `trace_hex`, starting now, in `session` when given.
    async fn commit_trace(
        app: &TestApp,
        seeded: &Seeded,
        trace_hex: &str,
        span_hex: &str,
        session: Option<&str>,
    ) {
        let now = app.state.clock.now_ns();
        let mut span = dataset_span(0);
        span.trace_id = trace_hex.parse().unwrap();
        span.span_id = span_hex.parse().unwrap();
        span.parent_span_id = None;
        span.start_time = now;
        span.end_time = now + 1_000_000_000;
        span.ingest_time = now;
        span.session_id = session.map(str::to_owned);
        app.commit_spans(&seeded.project, vec![span]).await;
    }

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

    /// One occurrence of `issue` on `trace`, `hours_ago` hours before now, with the span key
    /// `span` when given.
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

    /// One occurrence of `issue` on `trace`, `hours_ago` hours before now, in `session`. The
    /// shared `occurrence` fixture writes no session, so the session tests write it here.
    async fn occur_in(
        app: &TestApp,
        seeded: &Seeded,
        source: Uuid,
        issue: Uuid,
        trace: &str,
        hours_ago: i64,
        session: Option<&str>,
    ) {
        let at = app.state.clock.now_ns() - hours_ago * HOUR_NS;
        let trace_id: TraceId = trace.parse().unwrap();
        let item = ScoreCreate {
            score_id: Some(score_writes::occurrence_id(
                seeded.project.id,
                trace_id,
                issue,
                at,
            )),
            target: ScoreTarget::Trace { trace_id },
            session_id: session.map(str::to_owned),
            user_id: None,
            name: format!("issue:{issue}"),
            value: ScoreValue::Boolean(true),
            reason: Some("boom".to_owned()),
            source: None,
            metadata: BTreeMap::new(),
            experiment_id: None,
            dataset_item_id: None,
            queue_id: None,
        };
        let written = score_writes::write(
            &app.state,
            &seeded.project,
            Writer::Issue { source_id: source },
            vec![item],
        )
        .await;
        assert!(written.is_ok(), "occurrence write refused");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_trace_queue_gets_one_item_per_trace() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance").await;
        let queue = create_queue(
            &app,
            seeded.project.id,
            &seeded.cookie,
            "Traces",
            "trace",
            definition,
        )
        .await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        let other = merged(&app, &seeded, src, target).await;
        commit_trace(&app, &seeded, &trace(1), &span(1), None).await;
        commit_trace(&app, &seeded, &trace(2), &span(2), None).await;
        // Trace 1 has two occurrences, of the target and of a merged issue; trace 2 has one.
        occur(&app, &seeded, src, target, &trace(1), 3, None).await;
        occur(&app, &seeded, src, other, &trace(1), 1, None).await;
        occur(&app, &seeded, src, target, &trace(2), 2, None).await;

        let answer = send_ok(&app, &seeded, target, json!({"queue": queue.id})).await;

        assert_eq!(answer.added, 2);
        assert_eq!(answer.existing, 0);
        assert!(answer.skipped.is_empty());
        let items = items_of(&app, &seeded, queue.id).await;
        let traces: HashSet<String> = items
            .iter()
            .filter_map(|item| item.trace_id.clone())
            .collect();
        assert_eq!(traces, HashSet::from([trace(1), trace(2)]));
        assert!(items.iter().all(|item| item.span_id.is_none()));
        assert!(items.iter().all(|item| item.session_id.is_none()));
        assert!(
            items
                .iter()
                .all(|item| item.added_by == QueueItemAddedBy::User)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn limit_caps_the_targets_newest_first() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance").await;
        let queue = create_queue(
            &app,
            seeded.project.id,
            &seeded.cookie,
            "Traces",
            "trace",
            definition,
        )
        .await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        for n in 1..=5 {
            commit_trace(&app, &seeded, &trace(n), &span(n), None).await;
            occur(&app, &seeded, src, target, &trace(n), i64::from(n), None).await;
        }

        let two = send_ok(
            &app,
            &seeded,
            target,
            json!({"queue": queue.id, "limit": 2}),
        )
        .await;
        assert_eq!(two.added, 2);
        let traces: HashSet<String> = items_of(&app, &seeded, queue.id)
            .await
            .iter()
            .filter_map(|item| item.trace_id.clone())
            .collect();
        assert_eq!(traces, HashSet::from([trace(1), trace(2)]));

        // The default limit adds the other three; the largest limit is accepted.
        let rest = send_ok(&app, &seeded, target, json!({"queue": queue.id})).await;
        assert_eq!((rest.added, rest.existing), (3, 2));
        let all = send_ok(
            &app,
            &seeded,
            target,
            json!({"queue": queue.id, "limit": 1000}),
        )
        .await;
        assert_eq!((all.added, all.existing), (0, 5));

        for limit in [json!(0), json!(1001)] {
            let response = send(
                &app,
                &seeded,
                target,
                json!({"queue": queue.id, "limit": limit}),
            )
            .await;
            assert_eq!(
                field_errors(&response),
                vec![("limit".to_owned(), "must be between 1 and 1000".to_owned())]
            );
        }
        let null = send(
            &app,
            &seeded,
            target,
            json!({"queue": queue.id, "limit": null}),
        )
        .await;
        assert_eq!(null.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_span_queue_gets_the_occurrence_spans() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance").await;
        let queue = create_queue(
            &app,
            seeded.project.id,
            &seeded.cookie,
            "Spans",
            "span",
            definition,
        )
        .await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        let other = merged(&app, &seeded, src, target).await;
        commit_trace(&app, &seeded, &trace(1), &span(1), None).await;
        // Newest first: the span (0), an occurrence without a span key (1), and the same span of
        // a merged issue (2), which adds no second item.
        occur(
            &app,
            &seeded,
            src,
            target,
            &trace(1),
            1,
            Some(span(1).as_str()),
        )
        .await;
        occur(&app, &seeded, src, target, &trace(2), 2, None).await;
        occur(
            &app,
            &seeded,
            src,
            other,
            &trace(1),
            3,
            Some(span(1).as_str()),
        )
        .await;

        let answer = send_ok(&app, &seeded, target, json!({"queue": queue.id})).await;

        assert_eq!(answer.added, 1);
        assert_eq!(answer.existing, 0);
        assert_eq!(
            answer.skipped,
            vec![SkippedQueueItem {
                index: 1,
                message: "the occurrence has no span".to_owned(),
            }]
        );
        let items = items_of(&app, &seeded, queue.id).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].trace_id.as_deref(), Some(trace(1).as_str()));
        assert_eq!(items[0].span_id.as_deref(), Some(span(1).as_str()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_queue_gets_the_distinct_sessions() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance").await;
        let queue = create_queue(
            &app,
            seeded.project.id,
            &seeded.cookie,
            "Sessions",
            "session",
            definition,
        )
        .await;
        let newest = create_queue(
            &app,
            seeded.project.id,
            &seeded.cookie,
            "Newest",
            "session",
            definition,
        )
        .await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        commit_trace(&app, &seeded, &trace(1), &span(1), Some("s-a")).await;
        commit_trace(&app, &seeded, &trace(2), &span(2), Some("s-a")).await;
        commit_trace(&app, &seeded, &trace(3), &span(3), Some("s-b")).await;
        commit_trace(&app, &seeded, &trace(4), &span(4), None).await;
        occur_in(&app, &seeded, src, target, &trace(1), 1, Some("s-a")).await;
        occur_in(&app, &seeded, src, target, &trace(2), 2, Some("s-a")).await;
        occur_in(&app, &seeded, src, target, &trace(3), 3, Some("s-b")).await;
        occur_in(&app, &seeded, src, target, &trace(4), 4, None).await;

        let answer = send_ok(&app, &seeded, target, json!({"queue": queue.id})).await;

        assert_eq!(answer.added, 2);
        assert_eq!(
            answer.skipped,
            vec![SkippedQueueItem {
                index: 3,
                message: "the occurrence has no session".to_owned(),
            }]
        );
        let sessions: HashSet<String> = items_of(&app, &seeded, queue.id)
            .await
            .iter()
            .filter_map(|item| item.session_id.clone())
            .collect();
        assert_eq!(
            sessions,
            HashSet::from(["s-a".to_owned(), "s-b".to_owned()])
        );

        // The newest occurrence is in s-a: a limit of one adds that session only.
        let one = send_ok(
            &app,
            &seeded,
            target,
            json!({"queue": newest.id, "limit": 1}),
        )
        .await;
        assert_eq!(one.added, 1);
        let items = items_of(&app, &seeded, newest.id).await;
        assert_eq!(items[0].session_id.as_deref(), Some("s-a"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn merged_issues_count_for_their_target() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance").await;
        let queue = create_queue(
            &app,
            seeded.project.id,
            &seeded.cookie,
            "Traces",
            "trace",
            definition,
        )
        .await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        let other = merged(&app, &seeded, src, target).await;
        commit_trace(&app, &seeded, &trace(1), &span(1), None).await;
        occur(&app, &seeded, src, other, &trace(1), 1, None).await;

        let answer = send_ok(&app, &seeded, target, json!({"queue": queue.id})).await;

        assert_eq!(answer.added, 1);
        let items = items_of(&app, &seeded, queue.id).await;
        assert_eq!(items[0].trace_id.as_deref(), Some(trace(1).as_str()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_group_is_the_issue_and_the_seven_busiest_merged_issues() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance").await;
        let queue = create_queue(
            &app,
            seeded.project.id,
            &seeded.cookie,
            "Traces",
            "trace",
            definition,
        )
        .await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        commit_trace(&app, &seeded, &trace(1000), &span(1000), None).await;
        occur(&app, &seeded, src, target, &trace(1000), 1, None).await;

        // Merged issue `i` (0 to 8) has `i + 1` occurrences in the window, each on its own trace.
        let mut merged_ids = Vec::new();
        for i in 0..9u32 {
            let issue_id = merged(&app, &seeded, src, target).await;
            merged_ids.push(issue_id);
            for k in 0..=i {
                let n = 2000 + i * 100 + k;
                commit_trace(&app, &seeded, &trace(n), &span(n), None).await;
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

        // The target's trace, and seven merged issues of 3 to 9 occurrences: 1 + 42 targets.
        let answer = send_ok(&app, &seeded, target, json!({"queue": queue.id})).await;

        assert_eq!(answer.added, 43);
        assert!(answer.skipped.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_occurrence_older_than_the_default_period_is_not_read() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance").await;
        let queue = create_queue(
            &app,
            seeded.project.id,
            &seeded.cookie,
            "Traces",
            "trace",
            definition,
        )
        .await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        commit_trace(&app, &seeded, &trace(1), &span(1), None).await;
        commit_trace(&app, &seeded, &trace(2), &span(2), None).await;
        occur(&app, &seeded, src, target, &trace(1), 20 * 24, None).await;
        occur(&app, &seeded, src, target, &trace(2), 2, None).await;

        let answer = send_ok(&app, &seeded, target, json!({"queue": queue.id})).await;

        assert_eq!(answer.added, 1);
        assert!(answer.skipped.is_empty());
        let items = items_of(&app, &seeded, queue.id).await;
        assert_eq!(items[0].trace_id.as_deref(), Some(trace(2).as_str()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn any_issue_state_is_allowed() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance").await;
        let queue = create_queue(
            &app,
            seeded.project.id,
            &seeded.cookie,
            "Traces",
            "trace",
            definition,
        )
        .await;
        let src = source(&app, &seeded).await;
        for (n, state) in [(1, IssueState::Resolved), (2, IssueState::Ignored)] {
            commit_trace(&app, &seeded, &trace(n), &span(n), None).await;
            let target = issue(&app, &seeded, src, state).await;
            occur(&app, &seeded, src, target, &trace(n), 1, None).await;

            let answer = send_ok(&app, &seeded, target, json!({"queue": queue.id})).await;

            assert_eq!(answer.added, 1);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn existing_targets_are_counted() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance").await;
        let queue = create_queue(
            &app,
            seeded.project.id,
            &seeded.cookie,
            "Traces",
            "trace",
            definition,
        )
        .await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        commit_trace(&app, &seeded, &trace(1), &span(1), None).await;
        commit_trace(&app, &seeded, &trace(2), &span(2), None).await;
        occur(&app, &seeded, src, target, &trace(1), 1, None).await;
        occur(&app, &seeded, src, target, &trace(2), 2, None).await;

        let first = send_ok(&app, &seeded, target, json!({"queue": queue.id})).await;
        assert_eq!((first.added, first.existing), (2, 0));
        let second = send_ok(&app, &seeded, target, json!({"queue": queue.id})).await;
        assert_eq!((second.added, second.existing), (0, 2));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_targets_are_skipped_by_the_core() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance").await;
        let queue = create_queue(
            &app,
            seeded.project.id,
            &seeded.cookie,
            "Traces",
            "trace",
            definition,
        )
        .await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        commit_trace(&app, &seeded, &trace(2), &span(2), None).await;
        // The newest occurrence names a trace that was never committed.
        occur(&app, &seeded, src, target, &trace(3000), 1, None).await;
        occur(&app, &seeded, src, target, &trace(2), 2, None).await;

        let answer = send_ok(&app, &seeded, target, json!({"queue": queue.id})).await;

        assert_eq!(answer.added, 1);
        assert_eq!(
            answer.skipped,
            vec![SkippedQueueItem {
                index: 0,
                message: "the trace was not found".to_owned(),
            }]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_deletion_guard_blocks_a_target() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance").await;
        let traces = create_queue(
            &app,
            seeded.project.id,
            &seeded.cookie,
            "Traces",
            "trace",
            definition,
        )
        .await;
        let sessions = create_queue(
            &app,
            seeded.project.id,
            &seeded.cookie,
            "Sessions",
            "session",
            definition,
        )
        .await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        commit_trace(&app, &seeded, &trace(1), &span(1), None).await;
        commit_trace(&app, &seeded, &trace(2), &span(2), None).await;
        occur(&app, &seeded, src, target, &trace(1), 1, None).await;
        occur(&app, &seeded, src, target, &trace(2), 2, None).await;
        let now = app.state.clock.now_ns();
        app.state
            .deletions
            .guard
            .add_traces(seeded.project.id, &[tid(1)], Uuid::now_v7(), now);

        let answer = send_ok(&app, &seeded, target, json!({"queue": traces.id})).await;

        assert_eq!(answer.added, 1);
        assert_eq!(
            answer.skipped,
            vec![SkippedQueueItem {
                index: 0,
                message: "a deletion blocks this target".to_owned(),
            }]
        );

        // A session that holds a deleted trace is blocked too.
        commit_trace(&app, &seeded, &trace(4), &span(4), Some("s-guard")).await;
        occur_in(&app, &seeded, src, target, &trace(4), 1, Some("s-guard")).await;
        app.state
            .deletions
            .guard
            .add_traces(seeded.project.id, &[tid(4)], Uuid::now_v7(), now);
        let session = send_ok(&app, &seeded, target, json!({"queue": sessions.id})).await;
        assert_eq!(session.added, 0);
        assert_eq!(session.skipped[0].message, "a deletion blocks this target");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_full_queue_is_422() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance").await;
        let queue = create_queue(
            &app,
            seeded.project.id,
            &seeded.cookie,
            "Traces",
            "trace",
            definition,
        )
        .await;
        let queue_id = queue.id.to_string();
        let project_id = seeded.project.id.to_string();
        let user_id = seeded.user_id.to_string();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query(
                        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 100000) \
                         INSERT INTO annotation_queue_items \
                         (id, queue_id, project_id, trace_id, target_time, added_by_kind, added_by_id, \
                          created_at, updated_at) \
                         SELECT printf('01900000-0000-7000-8000-%012x', i), ?, ?, printf('%032x', i), \
                                1000, 'user', ?, 1000, 1000 FROM n",
                    )
                    .bind(queue_id)
                    .bind(project_id)
                    .bind(user_id)
                    .execute(&mut *conn)
                    .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        commit_trace(&app, &seeded, &trace(1), &span(1), None).await;
        occur(&app, &seeded, src, target, &trace(1), 1, None).await;

        let response = send(&app, &seeded, target, json!({"queue": queue.id})).await;

        assert_eq!(response.status, StatusCode::UNPROCESSABLE_ENTITY);
        let body: Value = response.json();
        assert_eq!(body["code"], "too_many_items");
        let read = app
            .request(
                Method::GET,
                &format!("{}/{}", queues_path(seeded.project.id), queue.id),
            )
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        let body: Value = read.json();
        assert_eq!(body["counts"]["pending"], 100_000);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn occurrences_that_fit_no_target_answer_201_with_all_skipped() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance").await;
        let queue = create_queue(
            &app,
            seeded.project.id,
            &seeded.cookie,
            "Spans",
            "span",
            definition,
        )
        .await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        occur(&app, &seeded, src, target, &trace(1), 1, None).await;
        occur(&app, &seeded, src, target, &trace(2), 2, None).await;

        let response = send(&app, &seeded, target, json!({"queue": queue.id})).await;

        assert_eq!(response.status, StatusCode::CREATED);
        let answer: AddedQueueItems = response.json();
        assert_eq!((answer.added, answer.existing), (0, 0));
        assert_eq!(
            answer.skipped,
            vec![
                SkippedQueueItem {
                    index: 0,
                    message: "the occurrence has no span".to_owned(),
                },
                SkippedQueueItem {
                    index: 1,
                    message: "the occurrence has no span".to_owned(),
                },
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_issue_without_occurrences_is_400() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance").await;
        let queue = create_queue(
            &app,
            seeded.project.id,
            &seeded.cookie,
            "Traces",
            "trace",
            definition,
        )
        .await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;

        let response = send(&app, &seeded, target, json!({"queue": queue.id})).await;

        assert_eq!(
            field_errors(&response),
            vec![(
                "issue".to_owned(),
                "the issue has no occurrences in the data window".to_owned()
            )]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_issue_is_404() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance").await;
        let queue = create_queue(
            &app,
            seeded.project.id,
            &seeded.cookie,
            "Traces",
            "trace",
            definition,
        )
        .await;

        let response = send(&app, &seeded, Uuid::now_v7(), json!({"queue": queue.id})).await;

        assert_eq!(response.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_queue_is_404() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;

        let response = send(&app, &seeded, target, json!({"queue": Uuid::now_v7()})).await;

        assert_eq!(response.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_issue_of_another_project_is_404() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance").await;
        let queue = create_queue(
            &app,
            seeded.project.id,
            &seeded.cookie,
            "Traces",
            "trace",
            definition,
        )
        .await;
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

        let response = send(&app, &seeded, id, json!({"queue": queue.id})).await;

        assert_eq!(response.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_queue_of_another_project_is_404() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (other, _key) = app.add_project(&seeded, "playground", None).await;
        let definition = define(&app, other.id, "relevance").await;
        let foreign = create_queue(
            &app,
            other.id,
            &seeded.cookie,
            "Foreign",
            "trace",
            definition,
        )
        .await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;

        let response = send(&app, &seeded, target, json!({"queue": foreign.id})).await;

        assert_eq!(response.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_queue_that_is_not_a_uuid_is_400() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;

        let response = send(&app, &seeded, target, json!({"queue": "traces"})).await;

        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn permissions() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance").await;
        let queue = create_queue(
            &app,
            seeded.project.id,
            &seeded.cookie,
            "Traces",
            "trace",
            definition,
        )
        .await;
        let src = source(&app, &seeded).await;
        let target = issue(&app, &seeded, src, IssueState::Open).await;
        commit_trace(&app, &seeded, &trace(1), &span(1), None).await;
        occur(&app, &seeded, src, target, &trace(1), 1, None).await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let member = app
            .add_member(&seeded, "member@example.com", OrgRole::Member)
            .await;
        let body = json!({"queue": queue.id});
        let path = base_path(&seeded, target);

        let as_viewer = app
            .request(Method::POST, &path)
            .header("cookie", &viewer.cookie)
            .json(&body)
            .send()
            .await;
        assert_eq!(as_viewer.status, StatusCode::FORBIDDEN);
        let as_read_key = app
            .request(Method::POST, &path)
            .header("authorization", &format!("Bearer {}", seeded.read_key))
            .json(&body)
            .send()
            .await;
        assert_eq!(as_read_key.status, StatusCode::FORBIDDEN);
        let read_only = app.personal_token(member.id, false).await;
        let as_read_token = app
            .request(Method::POST, &path)
            .header("authorization", &format!("Bearer {read_only}"))
            .json(&body)
            .send()
            .await;
        assert_eq!(as_read_token.status, StatusCode::FORBIDDEN);

        let as_member = app
            .request(Method::POST, &path)
            .header("cookie", &member.cookie)
            .json(&body)
            .send()
            .await;
        assert_eq!(as_member.status, StatusCode::CREATED);
        let items = items_of(&app, &seeded, queue.id).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].added_by, QueueItemAddedBy::User);

        // A write key adds the target of a new occurrence, so its item shows the key as the adder.
        commit_trace(&app, &seeded, &trace(2), &span(2), None).await;
        occur(&app, &seeded, src, target, &trace(2), 2, None).await;
        let write_key = app.write_key(&seeded).await;
        let as_write_key = app
            .request(Method::POST, &path)
            .header("authorization", &format!("Bearer {write_key}"))
            .json(&body)
            .send()
            .await;
        assert_eq!(as_write_key.status, StatusCode::CREATED);
        let items = items_of(&app, &seeded, queue.id).await;
        let by_key: Vec<&AnnotationQueueItem> = items
            .iter()
            .filter(|item| item.trace_id.as_deref() == Some(trace(2).as_str()))
            .collect();
        assert_eq!(by_key.len(), 1);
        assert_eq!(by_key[0].added_by, QueueItemAddedBy::Key);
    }
}
