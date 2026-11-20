//! Issue fixtures for the tests of M4 and later: sources, issues, events, channels and
//! occurrences, written through the stores as the product writes them (D-IC-10, D-IC-18).
// Later phases use the rest of this file; the Closing section removes this allow.
#![allow(dead_code)]

use std::collections::BTreeMap;

use cetra_core::ids::{ProjectId, TraceId};
use cetra_core::score::{ScoreTarget, ScoreValue};
use uuid::Uuid;

use super::{Seeded, TestApp};
use crate::api::scores::ScoreCreate;
use crate::meta::issues::{
    self as store, EventKind, EventRow, IssueRow, IssueState, SourceKind, SourceRow,
};
use crate::scores::{self as score_writes, Writer};

/// A source with every optional field `None` or empty, `created_at = updated_at = now`, no creator.
pub(crate) fn source_row(
    project: ProjectId,
    name: &str,
    kind: SourceKind,
    enabled: bool,
    now: i64,
) -> SourceRow {
    SourceRow {
        id: Uuid::now_v7(),
        project_id: project,
        name: name.to_owned(),
        kind,
        filter: None,
        enabled,
        paused_reason: None,
        paused_at: None,
        created_by: None,
        created_at: now,
        updated_at: now,
        sample_rate: None,
        daily_cap: None,
        score_names: Vec::new(),
        feedback_sources: Vec::new(),
        chat_connection_id: None,
        chat_model: None,
        embed_connection_id: None,
        embed_model: None,
        embed_dimension: None,
        floor: None,
        min_issue_size: None,
        budget_usd: None,
        budget_period: None,
        paused_detail: None,
        next_refit_at: None,
    }
}

/// Stores `row`.
pub(crate) async fn put_source(app: &TestApp, row: SourceRow) {
    app.state
        .meta
        .write(move |conn| Box::pin(async move { store::insert_source(conn, &row).await }))
        .await
        .unwrap();
}

/// An issue of `source` with one time for its first and last occurrence.
pub(crate) fn issue_row(
    project: ProjectId,
    source: Uuid,
    id: Uuid,
    title: &str,
    state: IssueState,
    seen: i64,
) -> IssueRow {
    IssueRow {
        id,
        project_id: project,
        source_id: source,
        fingerprint: None,
        title: title.to_owned(),
        description: None,
        state,
        regressed: false,
        merged_into: None,
        first_seen: seen,
        last_seen: seen,
        resolved_at: None,
        resolved_by: None,
        created_at: seen,
        updated_at: seen,
        centroid: None,
        member_count: 0,
    }
}

/// Stores `row`.
pub(crate) async fn put_issue(app: &TestApp, row: IssueRow) {
    app.state
        .meta
        .write(move |conn| Box::pin(async move { store::insert_issue(conn, &row).await }))
        .await
        .unwrap();
}

/// Stores one event of `issue` at `at`, with a fresh id, no actor and no from or to value.
pub(crate) async fn put_event(app: &TestApp, issue: Uuid, kind: EventKind, at: i64) {
    let row = EventRow {
        id: Uuid::now_v7(),
        issue_id: issue,
        kind,
        actor_user_id: None,
        from_value: None,
        to_value: None,
        at,
    };
    app.state
        .meta
        .write(move |conn| Box::pin(async move { store::insert_event(conn, &row).await }))
        .await
        .unwrap();
}

/// Stores a notification channel that is never delivered to (its config is not sealed
/// data). Returns its id. Tests that deliver create channels through the channels route.
pub(crate) async fn put_channel(app: &TestApp, project: ProjectId, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    let now = app.state.clock.now_ns();
    let project = project.to_string();
    let stored = id.to_string();
    let name = name.to_owned();
    app.state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                sqlx::query(
                    "INSERT INTO notification_channels \
                     (id, project_id, kind, name, config_enc, target_hint, created_at, updated_at) \
                     VALUES (?, ?, 'slack', ?, ?, 'test', ?, ?)",
                )
                .bind(stored)
                .bind(project)
                .bind(name)
                .bind(vec![0u8])
                .bind(now)
                .bind(now)
                .execute(&mut *conn)
                .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
    id
}

/// Writes one occurrence of `issue` on `trace` at `at` (absolute nanoseconds), as the issue
/// writer does (D-IC-18). `span` is the value of the metadata key `cetra.issue.span_id` (16 hex);
/// `None` omits the key.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn occurrence(
    app: &TestApp,
    seeded: &Seeded,
    source: Uuid,
    issue: Uuid,
    trace: &str,
    at: i64,
    user: Option<&str>,
    span: Option<&str>,
) {
    let trace_id: TraceId = trace.parse().unwrap();
    let mut metadata = BTreeMap::new();
    if let Some(span) = span {
        metadata.insert("cetra.issue.span_id".to_owned(), span.to_owned());
    }
    let item = ScoreCreate {
        score_id: Some(score_writes::occurrence_id(
            seeded.project.id,
            trace_id,
            issue,
            at,
        )),
        target: ScoreTarget::Trace { trace_id },
        session_id: None,
        user_id: user.map(str::to_owned),
        name: format!("issue:{issue}"),
        value: ScoreValue::Boolean(true),
        reason: Some("boom".to_owned()),
        source: None,
        metadata,
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
