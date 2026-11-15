//! Reading storage the way the server does: the public `reads` functions over a `QueryContext`
//! of the project's snapshot, every page followed (docs/plans/r-hardening/01-m1-model.md § R1.3).

use std::time::Duration;

use arrow::array::{Array, AsArray, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow::datatypes::{Float64Type, Int64Type, TimestampNanosecondType};

use super::harness::Harness;
use super::model::{ExpectedScore, ExpectedTrace};
use super::ops::{SESSIONS, TRACES};
use super::shape::{P, hex, span_id, trace_id};
use crate::reads::metrics::metrics;
use crate::reads::{
    AlertMeasure, AlertValue, AlertValueRequest, Bucket, BucketWidth, FilterDeletionRequest, Kpi,
    ListScope, MAX_FILTER_DELETION_TRACES, MAX_PAGE, MetricsRequest, ScoreListRequest,
    ScoreTargets, SessionListRequest, SessionSort, SortOrder, SpanListRequest, SpanSort,
    TraceListRequest, TraceSort, UserDeletionScope, UserListRequest, UserSort, alert_value,
    eval_trace_ids, filter_trace_ids, latest_scores, score_list, scores_by_id, session_list,
    span_detail, span_list, trace_detail, trace_list, user_deletion_scope, user_list,
};
use crate::views::{QueryContext, TimeWindow};
use llmobs_core::ids::TraceId;
use llmobs_core::schema::{scores, spans, traces};
use std::collections::HashSet;

/// Time allowed for each read request.
const TIMEOUT: Duration = Duration::from_secs(30);

/// One row of `latest_scores`: the live score of one `(target_key, name)`.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct LatestScore {
    /// The trace id (hex) of a trace score, the session id of a session score.
    pub(super) target_key: String,
    pub(super) name: String,
    pub(super) score_id: String,
    pub(super) value_num: f64,
}

/// Every row: no window, no default exclusions.
pub(super) const ALL_ROWS: ListScope = ListScope {
    window: None,
    include_eval: true,
    include_experiments: true,
};

/// The scope of user `user` as `user_deletion_scope` returns it, for a cutoff of `cutoff`. A failed
/// read is returned as its message (R4.5: a read may fail under injected faults).
pub(super) fn try_observe_user_scope(
    harness: &Harness,
    project: P,
    user: &str,
    cutoff: i64,
) -> Result<UserDeletionScope, String> {
    harness.block_on(async {
        let ctx = try_context(harness, project).await?;
        user_deletion_scope(&ctx, user, cutoff, TIMEOUT)
            .await
            .map_err(|error| format!("user_deletion_scope on a project: {error:?}"))
    })
}

/// The evaluation traces `eval_trace_ids` returns for `targets`, in its order. A failed read is
/// returned as its message (R4.5).
pub(super) fn try_observe_eval_trace_ids(
    harness: &Harness,
    project: P,
    targets: &[TraceId],
) -> Result<Vec<TraceId>, String> {
    harness.block_on(async {
        let ctx = try_context(harness, project).await?;
        let targets: HashSet<TraceId> = targets.iter().cloned().collect();
        eval_trace_ids(&ctx, &targets, TIMEOUT)
            .await
            .map_err(|error| format!("eval_trace_ids on a project: {error:?}"))
    })
}

/// A query context on `project`'s snapshot, as the server's read handlers build one.
async fn context(harness: &Harness, project: P) -> QueryContext {
    try_context(harness, project)
        .await
        .expect("a query context on a project")
}

/// [`context`], with a failure returned rather than panicking: a read under faults may fail.
async fn try_context(harness: &Harness, project: P) -> Result<QueryContext, String> {
    let query = harness
        .storage()
        .query(project.id(), Default::default())
        .await
        .map_err(|error| format!("query on a project: {error}"))?;
    QueryContext::new(query, None).map_err(|error| format!("query context on a project: {error}"))
}

/// `(start_time, trace_id hex, span_id hex)` of every `span_list` row, duplicates kept, in list
/// order. Pages of `limit` rows each.
pub(super) fn observe_spans(
    harness: &Harness,
    project: P,
    limit: usize,
) -> Vec<(i64, String, String)> {
    try_observe_spans(harness, project, limit).expect("span_list on a project")
}

/// `observe_spans`, with a failed read returned as its message (R4.5).
pub(super) fn try_observe_spans(
    harness: &Harness,
    project: P,
    limit: usize,
) -> Result<Vec<(i64, String, String)>, String> {
    harness.block_on(async {
        let ctx = try_context(harness, project).await?;
        let mut keys = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = span_list(
                &ctx,
                SpanListRequest {
                    filter: None,
                    search: None,
                    scope: ALL_ROWS,
                    sort: SpanSort::StartTime,
                    order: SortOrder::Asc,
                    limit,
                    cursor: cursor.as_deref(),
                    timeout: TIMEOUT,
                },
            )
            .await
            .map_err(|error| format!("span_list on a project: {error:?}"))?;
            for batch in &page.batches {
                let trace_ids = utf8(batch, spans::TRACE_ID);
                let span_ids = utf8(batch, spans::SPAN_ID);
                for row in 0..batch.num_rows() {
                    keys.push((
                        timestamp(batch, spans::START_TIME, row),
                        trace_ids.value(row).to_owned(),
                        span_ids.value(row).to_owned(),
                    ));
                }
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(keys)
    })
}

/// Every `traces` row `trace_list` returns for `scope`, in list order (`start_time DESC`),
/// read in pages of `limit` rows.
pub(super) fn observe_traces(
    harness: &Harness,
    project: P,
    scope: ListScope,
    limit: usize,
) -> Vec<ExpectedTrace> {
    try_observe_traces(harness, project, scope, limit).expect("trace_list on a project")
}

/// `observe_traces`, with a failed read returned as its message (R4.5: `FaultyRead` may fail
/// under faults, and only then).
pub(super) fn try_observe_traces(
    harness: &Harness,
    project: P,
    scope: ListScope,
    limit: usize,
) -> Result<Vec<ExpectedTrace>, String> {
    harness.block_on(async {
        let ctx = try_context(harness, project).await?;
        let mut rows = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = trace_list(
                &ctx,
                TraceListRequest {
                    filter: None,
                    search: None,
                    scope,
                    sort: TraceSort::StartTime,
                    order: SortOrder::Desc,
                    limit,
                    cursor: cursor.as_deref(),
                    timeout: TIMEOUT,
                },
            )
            .await
            .map_err(|error| format!("trace_list on a project: {error}"))?;
            for batch in &page.batches {
                for row in 0..batch.num_rows() {
                    rows.push(trace_from_batch(batch, row));
                }
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(rows)
    })
}

/// The `traces` row of `trace` and the `span_id`s of its spans, in the returned order.
/// `None` for an unknown trace.
pub(super) fn observe_trace_detail(
    harness: &Harness,
    project: P,
    trace: u8,
) -> Option<(ExpectedTrace, Vec<String>)> {
    harness.block_on(async {
        let ctx = context(harness, project).await;
        let detail = trace_detail(&ctx, &hex(trace_id(trace).as_bytes()), TIMEOUT)
            .await
            .expect("trace_detail on a project");
        let detail = detail?;
        assert!(
            !detail.truncated,
            "a trace of at most 12 spans is not truncated"
        );
        assert_eq!(detail.trace.num_rows(), 1, "one traces row per trace");
        let row = trace_from_batch(&detail.trace, 0);
        let mut span_ids = Vec::new();
        for batch in &detail.spans {
            let ids = utf8(batch, spans::SPAN_ID);
            for index in 0..batch.num_rows() {
                span_ids.push(ids.value(index).to_owned());
            }
        }
        Some((row, span_ids))
    })
}

/// The `ingest_time` of the newest copy of span `(trace, span_no)`, or `None` when no
/// visible copy exists.
pub(super) fn observe_span_detail(
    harness: &Harness,
    project: P,
    trace: u8,
    span_no: u8,
) -> Option<i64> {
    harness.block_on(async {
        let ctx = context(harness, project).await;
        let batch = span_detail(
            &ctx,
            &hex(trace_id(trace).as_bytes()),
            &hex(span_id(span_no).as_bytes()),
            TIMEOUT,
        )
        .await
        .expect("span_detail on a project");
        let batch = batch?;
        assert_eq!(batch.num_rows(), 1, "one spans row per span key");
        Some(timestamp(&batch, spans::INGEST_TIME, 0))
    })
}

/// Every live `scores` row of `project`, in list order, read in pages of `limit` rows.
pub(super) fn observe_scores(harness: &Harness, project: P, limit: usize) -> Vec<ExpectedScore> {
    try_observe_scores(harness, project, limit).expect("score_list on a project")
}

/// `observe_scores`, with a failed read returned as its message (R4.5).
pub(super) fn try_observe_scores(
    harness: &Harness,
    project: P,
    limit: usize,
) -> Result<Vec<ExpectedScore>, String> {
    harness.block_on(async {
        let ctx = try_context(harness, project).await?;
        let mut rows = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = score_list(
                &ctx,
                ScoreListRequest {
                    target: None,
                    trace_id: None,
                    span_id: None,
                    session_id: None,
                    name: None,
                    source: None,
                    window: None,
                    limit,
                    cursor: cursor.as_deref(),
                    timeout: TIMEOUT,
                },
            )
            .await
            .map_err(|error| format!("score_list on a project: {error:?}"))?;
            for batch in &page.batches {
                for row in 0..batch.num_rows() {
                    rows.push(score_from_batch(batch, row));
                }
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(rows)
    })
}

/// The live rows of `score_id`s (UUID strings) that `project` has. Unknown and other-project
/// ids return nothing.
pub(super) fn observe_scores_by_id(
    harness: &Harness,
    project: P,
    ids: &[String],
) -> Vec<ExpectedScore> {
    harness.block_on(async {
        let ctx = context(harness, project).await;
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let batches = scores_by_id(&ctx, &refs, TIMEOUT)
            .await
            .expect("scores_by_id on a project");
        let mut rows = Vec::new();
        for batch in &batches {
            for row in 0..batch.num_rows() {
                rows.push(score_from_batch(batch, row));
            }
        }
        rows
    })
}

/// `latest_scores` for every trace number and every session (`s0`..), both target kinds.
pub(super) fn observe_latest(harness: &Harness, project: P) -> Vec<LatestScore> {
    harness.block_on(async {
        let ctx = context(harness, project).await;
        let trace_keys: Vec<String> = (0..TRACES)
            .map(|trace| hex(trace_id(trace).as_bytes()))
            .collect();
        let trace_refs: Vec<&str> = trace_keys.iter().map(String::as_str).collect();
        let session_keys: Vec<String> =
            (0..SESSIONS).map(|session| format!("s{session}")).collect();
        let session_refs: Vec<&str> = session_keys.iter().map(String::as_str).collect();
        let mut rows = Vec::new();
        for targets in [
            ScoreTargets::Traces(&trace_refs),
            ScoreTargets::Sessions(&session_refs),
        ] {
            let batches = latest_scores(&ctx, targets, TIMEOUT)
                .await
                .expect("latest_scores on a project");
            for batch in &batches {
                for row in 0..batch.num_rows() {
                    rows.push(latest_from_batch(batch, row));
                }
            }
        }
        rows
    })
}

/// One row of `session_list` (`reads/sessions.rs`), the columns the checks compare.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct SessionRow {
    pub(super) session_id: String,
    pub(super) trace_count: i64,
    pub(super) error_count: i64,
    pub(super) total_tokens: Option<i64>,
    pub(super) cost_total: Option<f64>,
}

/// A window `[from, to)` of nanoseconds, as the reads take it.
pub(super) fn window_of((from, to): (i64, i64)) -> TimeWindow {
    TimeWindow::new(from, to).expect("a window with from before to")
}

/// Every `session_list` row over all traces: no window, evaluation and experiment traces kept.
pub(super) fn observe_sessions(harness: &Harness, project: P) -> Vec<SessionRow> {
    harness.block_on(async {
        let ctx = context(harness, project).await;
        let mut rows = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = session_list(
                &ctx,
                SessionListRequest {
                    filter: None,
                    scope: ALL_ROWS,
                    sort: SessionSort::StartTime,
                    order: SortOrder::Desc,
                    limit: MAX_PAGE,
                    cursor: cursor.as_deref(),
                    timeout: TIMEOUT,
                },
            )
            .await
            .expect("session_list on a project");
            for batch in &page.batches {
                let sessions = utf8(batch, traces::SESSION_ID);
                for row in 0..batch.num_rows() {
                    rows.push(SessionRow {
                        session_id: sessions.value(row).to_owned(),
                        trace_count: int(batch, "trace_count", row),
                        error_count: int(batch, traces::ERROR_COUNT, row),
                        total_tokens: opt_i64(
                            column(batch, traces::TOTAL_TOKENS).as_primitive::<Int64Type>(),
                            row,
                        ),
                        cost_total: opt_f64(
                            column(batch, traces::COST_TOTAL).as_primitive::<Float64Type>(),
                            row,
                        ),
                    });
                }
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        rows
    })
}

/// `(user_id, trace_count)` of every `user_list` row over all traces.
pub(super) fn observe_users(harness: &Harness, project: P) -> Vec<(String, i64)> {
    harness.block_on(async {
        let ctx = context(harness, project).await;
        let mut rows = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = user_list(
                &ctx,
                UserListRequest {
                    filter: None,
                    scope: ALL_ROWS,
                    sort: UserSort::TraceCount,
                    order: SortOrder::Desc,
                    limit: MAX_PAGE,
                    cursor: cursor.as_deref(),
                    timeout: TIMEOUT,
                },
            )
            .await
            .expect("user_list on a project");
            for batch in &page.batches {
                let users = utf8(batch, traces::USER_ID);
                for row in 0..batch.num_rows() {
                    rows.push((users.value(row).to_owned(), int(batch, "trace_count", row)));
                }
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        rows
    })
}

/// The KPIs of the dashboard metrics over days D..D+4, as the dashboard requests them: one
/// day per bucket, no filter, no grouping, evaluation traces excluded, experiments kept.
pub(super) fn observe_kpis(harness: &Harness, project: P, window: (i64, i64)) -> Vec<Kpi> {
    try_observe_kpis(harness, project, window).expect("metrics on a project")
}

/// `observe_kpis`, with a failed read returned as its message (R4.5).
pub(super) fn try_observe_kpis(
    harness: &Harness,
    project: P,
    window: (i64, i64),
) -> Result<Vec<Kpi>, String> {
    harness.block_on(async {
        let ctx = try_context(harness, project).await?;
        let response = metrics(
            &ctx,
            MetricsRequest {
                window: window_of(window),
                bucket: Bucket::Fixed(BucketWidth::D1),
                filter: None,
                group_by: None,
                include_eval: false,
                include_experiments: true,
                timeout: TIMEOUT,
            },
        )
        .await
        .map_err(|error| format!("metrics on a project: {error:?}"))?;
        Ok(response.kpis)
    })
}

/// The hex trace ids `filter_trace_ids` returns for `window`, evaluation traces excluded.
pub(super) fn observe_filter_trace_ids(
    harness: &Harness,
    project: P,
    window: (i64, i64),
) -> Vec<String> {
    harness.block_on(async {
        let ctx = context(harness, project).await;
        let ids = filter_trace_ids(
            &ctx,
            FilterDeletionRequest {
                filter: None,
                search: None,
                window: window_of(window),
                include_eval: false,
                include_experiments: true,
                limit: MAX_FILTER_DELETION_TRACES + 1,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("filter_trace_ids on a project");
        ids.iter().map(|id| hex(id.as_bytes())).collect()
    })
}

/// `alert_value` for `measure` over `window`, with no filter.
pub(super) fn observe_alert(
    harness: &Harness,
    project: P,
    window: (i64, i64),
    measure: AlertMeasure,
) -> AlertValue {
    harness.block_on(async {
        let ctx = context(harness, project).await;
        alert_value(
            &ctx,
            AlertValueRequest {
                window: window_of(window),
                filter: None,
                measure,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("alert_value on a project")
    })
}

/// One `int64` cell, by column name.
fn int(batch: &RecordBatch, name: &str, row: usize) -> i64 {
    column(batch, name).as_primitive::<Int64Type>().value(row)
}

/// One row of the `scores` view, read by column name.
fn score_from_batch(batch: &RecordBatch, row: usize) -> ExpectedScore {
    let target_kind = match utf8(batch, scores::TARGET).value(row) {
        "trace" => "trace",
        "session" => "session",
        other => panic!("the scores view has no target {other:?}"),
    };
    ExpectedScore {
        score_id: utf8(batch, scores::SCORE_ID).value(row).to_owned(),
        name: utf8(batch, scores::NAME).value(row).to_owned(),
        target_kind,
        trace_id: opt_string(utf8(batch, scores::TRACE_ID), row),
        session_id: opt_string(utf8(batch, scores::SESSION_ID), row),
        value_num: opt_f64(
            column(batch, scores::VALUE_NUM).as_primitive::<Float64Type>(),
            row,
        )
        .expect("a generated score is numeric"),
        created_at: timestamp(batch, scores::CREATED_AT, row),
        user_id: opt_string(utf8(batch, scores::USER_ID), row),
    }
}

/// One row of `latest_scores`. Its key column is `target_key`
/// (`reads/scores.rs`, `LATEST_SCORE_COLUMNS`).
fn latest_from_batch(batch: &RecordBatch, row: usize) -> LatestScore {
    LatestScore {
        target_key: utf8(batch, "target_key").value(row).to_owned(),
        name: utf8(batch, scores::NAME).value(row).to_owned(),
        score_id: utf8(batch, scores::SCORE_ID).value(row).to_owned(),
        value_num: opt_f64(
            column(batch, scores::VALUE_NUM).as_primitive::<Float64Type>(),
            row,
        )
        .expect("a generated score is numeric"),
    }
}

/// A column of `batch` by name. The view names are the constants of `llmobs_core::schema`.
fn column<'a>(batch: &'a RecordBatch, name: &str) -> &'a arrow::array::ArrayRef {
    batch
        .column_by_name(name)
        .unwrap_or_else(|| panic!("the view has a {name} column"))
}

fn utf8<'a>(batch: &'a RecordBatch, name: &str) -> &'a StringArray {
    column(batch, name).as_string::<i32>()
}

fn timestamp(batch: &RecordBatch, name: &str, row: usize) -> i64 {
    column(batch, name)
        .as_primitive::<TimestampNanosecondType>()
        .value(row)
}

fn opt_string(array: &StringArray, row: usize) -> Option<String> {
    (!array.is_null(row)).then(|| array.value(row).to_owned())
}

fn opt_i64(array: &Int64Array, row: usize) -> Option<i64> {
    (!array.is_null(row)).then(|| array.value(row))
}

fn opt_f64(array: &Float64Array, row: usize) -> Option<f64> {
    (!array.is_null(row)).then(|| array.value(row))
}

/// A `List<Utf8>` cell, sorted.
fn string_list(batch: &RecordBatch, name: &str, row: usize) -> Vec<String> {
    let list = column(batch, name).as_list::<i32>().value(row);
    let items = list.as_string::<i32>();
    let mut values: Vec<String> = (0..items.len())
        .map(|index| items.value(index).to_owned())
        .collect();
    values.sort();
    values
}

/// Maps a status read back to the model's name for it. Any other text is a storage bug.
fn static_status(text: &str) -> &'static str {
    match text {
        "error" => "error",
        "ok" => "ok",
        "unset" => "unset",
        other => panic!("the traces view has no status {other:?}"),
    }
}

/// One row of the `traces` view, read by column name.
fn trace_from_batch(batch: &RecordBatch, row: usize) -> ExpectedTrace {
    let count = |name: &str| column(batch, name).as_primitive::<Int64Type>().value(row);
    ExpectedTrace {
        trace_id: utf8(batch, traces::TRACE_ID).value(row).to_owned(),
        start_time: timestamp(batch, traces::START_TIME, row),
        end_time: timestamp(batch, traces::END_TIME, row),
        name: utf8(batch, traces::NAME).value(row).to_owned(),
        root_span_id: opt_string(utf8(batch, traces::ROOT_SPAN_ID), row),
        has_root: column(batch, traces::HAS_ROOT).as_boolean().value(row),
        status: static_status(utf8(batch, traces::STATUS).value(row)),
        session_id: opt_string(utf8(batch, traces::SESSION_ID), row),
        user_id: opt_string(utf8(batch, traces::USER_ID), row),
        environment: opt_string(utf8(batch, traces::ENVIRONMENT), row),
        tags: string_list(batch, traces::TAGS, row),
        models: string_list(batch, traces::MODELS, row),
        span_count: count(traces::SPAN_COUNT),
        error_count: count(traces::ERROR_COUNT),
        llm_count: count(traces::LLM_COUNT),
        total_tokens: opt_i64(
            column(batch, traces::TOTAL_TOKENS).as_primitive::<Int64Type>(),
            row,
        ),
        cost_total: opt_f64(
            column(batch, traces::COST_TOTAL).as_primitive::<Float64Type>(),
            row,
        ),
    }
}
