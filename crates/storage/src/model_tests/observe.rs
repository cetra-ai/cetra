//! Reading storage the way the server does: the public `reads` functions over a `QueryContext`
//! of the project's snapshot, every page followed (docs/plans/r-hardening/01-m1-model.md § R1.3).

use std::time::Duration;

use arrow::array::{Array, AsArray, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow::datatypes::{Float64Type, Int64Type, TimestampNanosecondType};

use super::harness::Harness;
use super::model::ExpectedTrace;
use super::shape::{P, hex, span_id, trace_id};
use crate::reads::{
    ListScope, SortOrder, SpanListRequest, SpanSort, TraceListRequest, TraceSort, span_detail,
    span_list, trace_detail, trace_list,
};
use crate::views::QueryContext;
use llmobs_core::schema::{spans, traces};

/// Time allowed for each read request.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Every row: no window, no default exclusions.
pub(super) const ALL_ROWS: ListScope = ListScope {
    window: None,
    include_eval: true,
    include_experiments: true,
};

/// A query context on `project`'s snapshot, as the server's read handlers build one.
async fn context(harness: &Harness, project: P) -> QueryContext {
    let query = harness
        .storage()
        .query(project.id(), Default::default())
        .await
        .expect("query on a project");
    QueryContext::new(query, None).expect("query context on a project")
}

/// `(trace_id hex, span_id hex)` of every `span_list` row, duplicates kept.
/// Pages of `limit` rows each.
pub(super) fn observe_spans(harness: &Harness, project: P, limit: usize) -> Vec<(String, String)> {
    harness.block_on(async {
        let ctx = context(harness, project).await;
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
            .expect("span_list on a project");
            for batch in &page.batches {
                let trace_ids = utf8(batch, spans::TRACE_ID);
                let span_ids = utf8(batch, spans::SPAN_ID);
                for row in 0..batch.num_rows() {
                    keys.push((
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
        keys
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
    harness.block_on(async {
        let ctx = context(harness, project).await;
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
            .expect("trace_list on a project");
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
        rows
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
