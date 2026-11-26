//! Reading storage the way the server does: the public `reads` functions over a `QueryContext`
//! of the project's snapshot, every page followed (docs/plans/r-hardening/01-m1-model.md § R1.3).
//!
//! Each function runs its query on the [`QueryContext`] it is given (or, for the ones that take a
//! storage, on a context it builds), and returns a [`ReadFailure`] instead of panicking, so the
//! checks and the reader threads of the concurrent mode can use the same functions
//! (docs/plans/r-hardening/06-m6-concurrency.md § R6.3). Reads that must agree with each other
//! share one context, so they see one snapshot.

use std::time::Duration;

use cetra_column::{Batch, Column, ScalarValue};

use super::model::{ExpectedScore, ExpectedTrace};
use super::ops::{SESSIONS, TRACES};
use super::shape::{P, hex, span_id, trace_id};
use crate::error::QueryError;
use crate::reads::{
    AlertMeasure, AlertValue, AlertValueRequest, FilterDeletionRequest, ListScope,
    MAX_FILTER_DELETION_TRACES, MAX_PAGE, ScoreListRequest, ScoreTargets, SessionListRequest,
    SessionSort, SortOrder, SpanListRequest, SpanSort, TraceListRequest, TraceSort,
    UserDeletionScope, UserListRequest, UserSort, alert_value, eval_trace_ids, filter_trace_ids,
    latest_scores, score_list, scores_by_id, session_list, span_detail, span_list, trace_detail,
    trace_list, user_deletion_scope, user_list,
};
use crate::test_support::headline_kpis;
use crate::views::{QueryContext, TimeWindow};
use crate::{RunError, Storage};
use cetra_core::ids::TraceId;
use cetra_core::schema::{scores, spans, traces};
use std::collections::HashSet;

/// Time allowed for each read request.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Why a read failed: storage refused the query under load, or the read failed.
#[derive(Debug, thiserror::Error)]
pub(super) enum ReadFailure {
    /// `QueryError::Busy` from `storage.query`.
    #[error("too many queries are running")]
    Busy,
    /// Any other error, as its `Display` text.
    #[error("{0}")]
    Error(String),
}

impl From<QueryError> for ReadFailure {
    fn from(error: QueryError) -> Self {
        match error {
            QueryError::Busy => Self::Busy,
            other => Self::Error(other.to_string()),
        }
    }
}

impl From<RunError> for ReadFailure {
    fn from(error: RunError) -> Self {
        Self::Error(error.to_string())
    }
}

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

/// The scope of user `user` as `user_deletion_scope` returns it, for a cutoff of `cutoff`.
pub(super) async fn observe_user_scope(
    storage: &Storage,
    project: P,
    user: &str,
    cutoff: i64,
) -> Result<UserDeletionScope, ReadFailure> {
    let ctx = context(storage, project).await?;
    Ok(user_deletion_scope(&ctx, user, cutoff, TIMEOUT).await?)
}

/// The evaluation traces `eval_trace_ids` returns for `targets`, in its order.
pub(super) async fn observe_eval_trace_ids(
    storage: &Storage,
    project: P,
    targets: &[TraceId],
) -> Result<Vec<TraceId>, ReadFailure> {
    let ctx = context(storage, project).await?;
    let targets: HashSet<TraceId> = targets.iter().cloned().collect();
    Ok(eval_trace_ids(&ctx, &targets, TIMEOUT).await?)
}

/// A query context on `project`'s snapshot, as the server's read handlers build one. A read takes
/// one, and every page it reads comes from it.
pub(super) async fn context(storage: &Storage, project: P) -> Result<QueryContext, ReadFailure> {
    let query = storage
        .query(project.id(), crate::QueryOptions::new(None, None))
        .await?;
    Ok(QueryContext::new(query, None)?)
}

/// `(start_time, trace_id hex, span_id hex)` of every `span_list` row, duplicates kept, in list
/// order. Pages of `limit` rows each.
pub(super) async fn observe_spans(
    ctx: &QueryContext,
    limit: usize,
) -> Result<Vec<(i64, String, String)>, ReadFailure> {
    let mut keys = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = span_list(
            ctx,
            SpanListRequest {
                filter: None,
                search: None,
                scope: ALL_ROWS,
                strict_exclusions: false,
                sort: SpanSort::StartTime,
                order: SortOrder::Asc,
                limit,
                cursor: cursor.as_deref(),
                timeout: TIMEOUT,
            },
        )
        .await?;
        for batch in &page.batches {
            for row in 0..batch.num_rows() {
                keys.push((
                    timestamp(batch, spans::START_TIME, row),
                    req_string(batch, spans::TRACE_ID, row),
                    req_string(batch, spans::SPAN_ID, row),
                ));
            }
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(keys)
}

/// Every `traces` row `trace_list` returns for `scope`, in list order (`start_time DESC`),
/// read in pages of `limit` rows.
pub(super) async fn observe_traces(
    ctx: &QueryContext,
    scope: ListScope,
    limit: usize,
) -> Result<Vec<ExpectedTrace>, ReadFailure> {
    let mut rows = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = trace_list(
            ctx,
            TraceListRequest {
                filter: None,
                search: None,
                scope,
                strict_exclusions: false,
                sort: TraceSort::StartTime,
                order: SortOrder::Desc,
                limit,
                cursor: cursor.as_deref(),
                timeout: TIMEOUT,
            },
        )
        .await?;
        for batch in &page.batches {
            for row in 0..batch.num_rows() {
                rows.push(trace_from_batch(batch, row)?);
            }
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(rows)
}

/// The `traces` row of `trace` and the `span_id`s of its spans, in the returned order.
/// `None` for an unknown trace.
pub(super) async fn observe_trace_detail(
    storage: &Storage,
    project: P,
    trace: u8,
) -> Result<Option<(ExpectedTrace, Vec<String>)>, ReadFailure> {
    let ctx = context(storage, project).await?;
    let Some(detail) = trace_detail(&ctx, &hex(trace_id(trace).as_bytes()), TIMEOUT).await? else {
        return Ok(None);
    };
    if detail.truncated {
        return Err(ReadFailure::Error(
            "trace_detail truncated a trace of at most 12 spans".to_owned(),
        ));
    }
    if detail.trace.num_rows() != 1 {
        return Err(ReadFailure::Error(format!(
            "trace_detail returned {} traces rows for one trace",
            detail.trace.num_rows()
        )));
    }
    let row = trace_from_batch(&detail.trace, 0)?;
    let mut span_ids = Vec::new();
    for batch in &detail.spans {
        for index in 0..batch.num_rows() {
            span_ids.push(req_string(batch, spans::SPAN_ID, index));
        }
    }
    Ok(Some((row, span_ids)))
}

/// The `ingest_time` of the newest copy of span `(trace, span_no)`, or `None` when no
/// visible copy exists.
pub(super) async fn observe_span_detail(
    storage: &Storage,
    project: P,
    trace: u8,
    span_no: u8,
) -> Result<Option<i64>, ReadFailure> {
    let ctx = context(storage, project).await?;
    let batch = span_detail(
        &ctx,
        &hex(trace_id(trace).as_bytes()),
        &hex(span_id(span_no).as_bytes()),
        TIMEOUT,
    )
    .await?;
    let Some(batch) = batch else {
        return Ok(None);
    };
    if batch.num_rows() != 1 {
        return Err(ReadFailure::Error(format!(
            "span_detail returned {} spans rows for one span key",
            batch.num_rows()
        )));
    }
    Ok(Some(timestamp(&batch, spans::INGEST_TIME, 0)))
}

/// Every live `scores` row of `project`, in list order, read in pages of `limit` rows.
pub(super) async fn observe_scores(
    ctx: &QueryContext,
    limit: usize,
) -> Result<Vec<ExpectedScore>, ReadFailure> {
    let mut rows = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = score_list(
            ctx,
            ScoreListRequest {
                target: None,
                trace_id: None,
                span_id: None,
                session_id: None,
                name: None,
                source: None,
                evaluator_id: None,
                rule_id: None,
                queue_id: None,
                window: None,
                limit,
                cursor: cursor.as_deref(),
                timeout: TIMEOUT,
            },
        )
        .await?;
        for batch in &page.batches {
            for row in 0..batch.num_rows() {
                rows.push(score_from_batch(batch, row)?);
            }
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(rows)
}

/// The live rows of `score_id`s (UUID strings) that `project` has. Unknown and other-project
/// ids return nothing.
pub(super) async fn observe_scores_by_id(
    storage: &Storage,
    project: P,
    ids: &[String],
) -> Result<Vec<ExpectedScore>, ReadFailure> {
    let ctx = context(storage, project).await?;
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let batches = scores_by_id(&ctx, &refs, TIMEOUT).await?;
    let mut rows = Vec::new();
    for batch in &batches {
        for row in 0..batch.num_rows() {
            rows.push(score_from_batch(batch, row)?);
        }
    }
    Ok(rows)
}

/// `latest_scores` for every trace number and every session (`s0`..), both target kinds.
pub(super) async fn observe_latest(
    storage: &Storage,
    project: P,
) -> Result<Vec<LatestScore>, ReadFailure> {
    let ctx = context(storage, project).await?;
    let trace_keys: Vec<String> = (0..TRACES)
        .map(|trace| hex(trace_id(trace).as_bytes()))
        .collect();
    let trace_refs: Vec<&str> = trace_keys.iter().map(String::as_str).collect();
    let session_keys: Vec<String> = (0..SESSIONS).map(|session| format!("s{session}")).collect();
    let session_refs: Vec<&str> = session_keys.iter().map(String::as_str).collect();
    let mut rows = Vec::new();
    for targets in [
        ScoreTargets::Traces(&trace_refs),
        ScoreTargets::Sessions(&session_refs),
    ] {
        let batches = latest_scores(&ctx, targets, TIMEOUT).await?;
        for batch in &batches {
            for row in 0..batch.num_rows() {
                rows.push(latest_from_batch(batch, row)?);
            }
        }
    }
    Ok(rows)
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
pub(super) async fn observe_sessions(
    storage: &Storage,
    project: P,
) -> Result<Vec<SessionRow>, ReadFailure> {
    let ctx = context(storage, project).await?;
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
        .await?;
        for batch in &page.batches {
            for row in 0..batch.num_rows() {
                rows.push(SessionRow {
                    session_id: req_string(batch, traces::SESSION_ID, row),
                    trace_count: int(batch, "trace_count", row),
                    error_count: int(batch, traces::ERROR_COUNT, row),
                    total_tokens: opt_i64(batch, traces::TOTAL_TOKENS, row),
                    cost_total: opt_f64(batch, traces::COST_TOTAL, row),
                });
            }
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(rows)
}

/// `(user_id, trace_count)` of every `user_list` row over all traces.
pub(super) async fn observe_users(
    storage: &Storage,
    project: P,
) -> Result<Vec<(String, i64)>, ReadFailure> {
    let ctx = context(storage, project).await?;
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
        .await?;
        for batch in &page.batches {
            for row in 0..batch.num_rows() {
                rows.push((
                    req_string(batch, traces::USER_ID, row),
                    int(batch, "trace_count", row),
                ));
            }
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(rows)
}

/// One headline KPI of the model test: its value over the window and over the previous window
/// of the same length. The dashboard's totals give them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Kpi {
    pub(super) name: KpiName,
    pub(super) value: Option<f64>,
    pub(super) previous: Option<f64>,
}

/// The headline KPIs the model test compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KpiName {
    Traces,
    ErrorRate,
    LatencyP95,
    CostTotal,
    TotalTokens,
}

/// The KPIs of `window`, as the dashboard's totals give them (see `headline_kpis`).
pub(super) async fn observe_kpis(
    storage: &Storage,
    project: P,
    window: (i64, i64),
) -> Result<Vec<Kpi>, ReadFailure> {
    const NAMES: [KpiName; 5] = [
        KpiName::Traces,
        KpiName::ErrorRate,
        KpiName::LatencyP95,
        KpiName::CostTotal,
        KpiName::TotalTokens,
    ];
    let ctx = context(storage, project).await?;
    let values = headline_kpis(&ctx, window_of(window))
        .await
        .map_err(ReadFailure::Error)?;
    Ok(NAMES
        .into_iter()
        .zip(values)
        .map(|(name, (value, previous))| Kpi {
            name,
            value,
            previous,
        })
        .collect())
}

/// The hex trace ids `filter_trace_ids` returns for `window`, evaluation traces excluded.
pub(super) async fn observe_filter_trace_ids(
    storage: &Storage,
    project: P,
    window: (i64, i64),
) -> Result<Vec<String>, ReadFailure> {
    let ctx = context(storage, project).await?;
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
    .await?;
    Ok(ids
        .iter()
        .map(|target| hex(target.trace_id.as_bytes()))
        .collect())
}

/// `alert_value` for `measure` over `window`, with no filter.
pub(super) async fn observe_alert(
    storage: &Storage,
    project: P,
    window: (i64, i64),
    measure: AlertMeasure,
) -> Result<AlertValue, ReadFailure> {
    let ctx = context(storage, project).await?;
    Ok(alert_value(
        &ctx,
        AlertValueRequest {
            window: window_of(window),
            filter: None,
            measure,
            timeout: TIMEOUT,
        },
    )
    .await?)
}

/// One `int64` cell, by column name.
fn int(batch: &Batch, name: &str, row: usize) -> i64 {
    crate::reads::columns::i64_at(batch, column(batch, name), row)
        .unwrap_or_else(|e| panic!("read int64 column {name}: {e}"))
}

/// One row of the `scores` view, read by column name.
fn score_from_batch(batch: &Batch, row: usize) -> Result<ExpectedScore, ReadFailure> {
    let target_kind = match req_string(batch, scores::TARGET, row).as_str() {
        "trace" => "trace",
        "session" => "session",
        other => {
            return Err(ReadFailure::Error(format!(
                "the scores view has a target that is neither trace nor session: {other:?}"
            )));
        }
    };
    let value_num = opt_f64(batch, scores::VALUE_NUM, row)
        .ok_or_else(|| ReadFailure::Error("a scores row has no numeric value".to_owned()))?;
    Ok(ExpectedScore {
        score_id: req_string(batch, scores::SCORE_ID, row),
        name: req_string(batch, scores::NAME, row),
        target_kind,
        trace_id: opt_string(batch, scores::TRACE_ID, row),
        session_id: opt_string(batch, scores::SESSION_ID, row),
        value_num,
        created_at: timestamp(batch, scores::CREATED_AT, row),
        user_id: opt_string(batch, scores::USER_ID, row),
    })
}

/// One row of `latest_scores`. Its key column is `target_key`
/// (`reads/scores.rs`, `LATEST_SCORE_COLUMNS`).
fn latest_from_batch(batch: &Batch, row: usize) -> Result<LatestScore, ReadFailure> {
    let value_num = opt_f64(batch, scores::VALUE_NUM, row)
        .ok_or_else(|| ReadFailure::Error("a latest_scores row has no numeric value".to_owned()))?;
    Ok(LatestScore {
        target_key: req_string(batch, "target_key", row),
        name: req_string(batch, scores::NAME, row),
        score_id: req_string(batch, scores::SCORE_ID, row),
        value_num,
    })
}

/// A column of `batch` by name. The view names are the constants of `cetra_core::schema`.
fn column<'a>(batch: &'a Batch, name: &str) -> &'a Column {
    crate::reads::columns::require_col(batch, name)
        .unwrap_or_else(|e| panic!("the view has a {name} column: {e}"))
}

fn opt_string(batch: &Batch, name: &str, row: usize) -> Option<String> {
    crate::reads::columns::string_at(batch, column(batch, name), row)
        .unwrap_or_else(|e| panic!("read string column {name}: {e}"))
}

fn req_string(batch: &Batch, name: &str, row: usize) -> String {
    opt_string(batch, name, row).unwrap_or_else(|| panic!("column {name} is unexpectedly null"))
}

fn timestamp(batch: &Batch, name: &str, row: usize) -> i64 {
    crate::reads::columns::time_at(batch, column(batch, name), row)
        .unwrap_or_else(|e| panic!("read timestamp column {name}: {e}"))
}

fn opt_i64(batch: &Batch, name: &str, row: usize) -> Option<i64> {
    let col = column(batch, name);
    let phys = crate::reads::columns::physical_row(batch, row).unwrap_or_else(|e| panic!("{e}"));
    match col.get_scalar(phys).unwrap_or_else(|e| panic!("{e}")) {
        ScalarValue::Null(_) => None,
        ScalarValue::I64(v) => Some(v),
        other => panic!("column {name} has unexpected type {other:?}"),
    }
}

fn opt_f64(batch: &Batch, name: &str, row: usize) -> Option<f64> {
    crate::reads::columns::decimal_to_f64_at(batch, column(batch, name), row)
        .unwrap_or_else(|e| panic!("read float/decimal column {name}: {e}"))
}

/// A `List<Utf8>` cell, sorted.
fn string_list(batch: &Batch, name: &str, row: usize) -> Vec<String> {
    let col = column(batch, name);
    let phys = crate::reads::columns::physical_row(batch, row).unwrap_or_else(|e| panic!("{e}"));
    let mut values = match col.get_scalar(phys).unwrap_or_else(|e| panic!("{e}")) {
        ScalarValue::Null(_) => Vec::new(),
        ScalarValue::List { values, .. } => values
            .into_iter()
            .filter_map(|item| match item {
                ScalarValue::Utf8(s) => Some(s),
                ScalarValue::Null(_) => None,
                other => panic!("column {name} list item has unexpected type {other:?}"),
            })
            .collect(),
        other => panic!("column {name} has unexpected type {other:?}"),
    };
    values.sort();
    values
}

/// Maps a status read back to the model's name for it. Any other text is a storage bug.
fn static_status(text: &str) -> Result<&'static str, ReadFailure> {
    match text {
        "error" => Ok("error"),
        "ok" => Ok("ok"),
        "unset" => Ok("unset"),
        other => Err(ReadFailure::Error(format!(
            "the traces view has a status that is not error, ok or unset: {other:?}"
        ))),
    }
}

/// One row of the `traces` view, read by column name.
fn trace_from_batch(batch: &Batch, row: usize) -> Result<ExpectedTrace, ReadFailure> {
    let status = static_status(&req_string(batch, traces::STATUS, row))?;
    let has_root = crate::reads::columns::bool_at(batch, column(batch, traces::HAS_ROOT), row)
        .unwrap_or_else(|e| panic!("read has_root: {e}"))
        .expect("has_root is non-null");
    Ok(ExpectedTrace {
        trace_id: req_string(batch, traces::TRACE_ID, row),
        start_time: timestamp(batch, traces::START_TIME, row),
        end_time: timestamp(batch, traces::END_TIME, row),
        name: req_string(batch, traces::NAME, row),
        root_span_id: opt_string(batch, traces::ROOT_SPAN_ID, row),
        has_root,
        status,
        session_id: opt_string(batch, traces::SESSION_ID, row),
        user_id: opt_string(batch, traces::USER_ID, row),
        environment: opt_string(batch, traces::ENVIRONMENT, row),
        tags: string_list(batch, traces::TAGS, row),
        models: string_list(batch, traces::MODELS, row),
        prompts: string_list(batch, traces::PROMPTS, row),
        span_count: int(batch, traces::SPAN_COUNT, row),
        error_count: int(batch, traces::ERROR_COUNT, row),
        llm_count: int(batch, traces::LLM_COUNT, row),
        total_tokens: opt_i64(batch, traces::TOTAL_TOKENS, row),
        cost_total: opt_f64(batch, traces::COST_TOTAL, row),
    })
}
