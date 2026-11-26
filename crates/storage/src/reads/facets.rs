//! Native query engine implementation of shared-scan `facets` (D1, D-C-24).

use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap, HashSet};
use std::fmt;
use std::sync::Arc;

use cetra_column::{
    Batch, Column, DataType, Field, FlatColumn, FlatData, Reservation, ScalarValue, Schema,
    Selection, Utf8Data,
};
use cetra_core::schema::{spans, traces};
use cetra_query::exec::hash_join::{HashJoinSpec, JoinType, execute_hash_join};
use cetra_query::exec::{
    AggExpr, AggFunc, ControlFlow, DdSketch, ExecContext, HashAgg, HashAggSpec, MapProjection,
    PartitionStream, PushSink, ScanSource, ScanSpec, collect_expr_map_projections,
    run_pipeline as exec_run_pipeline,
};
use cetra_query::{CompiledFilter, Expr, FilterView, QueryError};

use std::time::Duration;

use crate::query_error::RunError;
use crate::reads::ListScope;
use crate::reads::columns::{
    active_rows, internal, raw_bool_at, raw_f64_at, raw_i64_at, raw_string_at,
};
use crate::reads::context::{
    DURATION_MS, EngineContext, OperatorStream, populate_id_lookups, query_err_to_run,
    rewrite_view_expr_for_raw, run_err_to_query, spans_raw_schema, spans_view_projection,
    spans_view_schema, traces_raw_schema, traces_view_projection, traces_view_schema,
};
use crate::reads::present;
use crate::reads::scope::{
    SearchPredicate, StreamSet, scoped_spans_spec, scoped_traces_spec, search_predicate,
    sessions_from_traces, sessions_from_traces_projection,
};
use crate::reads::scores_join::{apply_compiled_filter, compile_optional_filter};
use crate::views::QueryContext;

/// Most values (or keys) one values facet returns.
pub const MAX_FACET_LIMIT: usize = 100;
/// Longest `q`, in characters after trimming.
pub const MAX_FACET_QUERY: usize = 200;
/// Longest map key of a `map['key']` field, in characters.
pub const MAX_FACET_KEY: usize = 256;
/// Equal-width buckets of a histogram below its p99.
pub const HISTOGRAM_BUCKETS: usize = 20;

/// The list a facet reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FacetView {
    /// The traces list.
    Traces,
    /// The spans list. Stored copies of a span count separately.
    Spans,
    /// The sessions list, one row per session.
    Sessions,
}

/// Most fields one facet batch asks.
pub const MAX_FACET_FIELDS: usize = 20;

/// One field of a facet batch.
#[derive(Debug, Clone, Copy)]
pub struct FacetField<'a> {
    /// `column` or `map['key']` (see D1 of the filtering plan).
    pub field: &'a str,
    /// Case-insensitive substring the value or key must contain. Ignored by histograms.
    pub query: Option<&'a str>,
    /// Most values or keys returned, `1..=`[`MAX_FACET_LIMIT`].
    pub limit: usize,
}

/// A facet batch: fields over the rows of one list (D-C-24).
#[derive(Debug, Clone, Copy)]
pub struct FacetsRequest<'a> {
    /// The list the facets describe.
    pub view: FacetView,
    /// Filter text over `view`. Blank means no filter.
    pub filter: Option<&'a str>,
    /// Full-text search. Traces and spans only; blank means no search.
    pub search: Option<&'a str>,
    /// The list's window and default exclusions.
    pub scope: ListScope,
    /// The fields, `1..=`[`MAX_FACET_FIELDS`].
    pub fields: &'a [FacetField<'a>],
    /// Read timeout of the whole batch.
    pub timeout: Duration,
}

/// What a facet describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FacetKind {
    /// Values of a string, boolean, list or `map['key']` field.
    Values,
    /// The keys of a bare map field.
    Keys,
    /// A histogram of a number field.
    Histogram,
}

/// One value of a values or keys facet and the rows under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FacetValue {
    /// The value, or the key. Booleans are `true` or `false`.
    pub value: String,
    /// Rows counted under it.
    pub count: u64,
}

/// One histogram bucket, `[lo, hi)` except the last, which is `[lo, hi]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HistogramBucket {
    /// Lower bound.
    pub lo: f64,
    /// Upper bound.
    pub hi: f64,
    /// Rows in the bucket.
    pub count: u64,
}

/// A histogram of a number field.
#[derive(Debug, Clone, PartialEq)]
pub struct Histogram {
    /// Smallest value, exact.
    pub min: f64,
    /// Largest value, exact.
    pub max: f64,
    /// Median, approximate.
    pub p50: f64,
    /// 95th percentile, approximate.
    pub p95: f64,
    /// 99th percentile, approximate.
    pub p99: f64,
    /// Every bucket, empty ones included.
    pub buckets: Vec<HistogramBucket>,
}

/// The answer of one facet.
#[derive(Debug, Clone, PartialEq)]
pub struct FacetData {
    /// What the answer describes.
    pub kind: FacetKind,
    /// Rows counted.
    pub total: u64,
    /// Rows with no value.
    pub missing: u64,
    /// Values or keys, by count descending then value ascending. Empty for a histogram.
    pub values: Vec<FacetValue>,
    /// True when more distinct values matched than `limit`. Always false for a histogram.
    pub more: bool,
    /// The histogram. `None` for a values facet, and for a histogram with no value.
    pub histogram: Option<Histogram>,
}

const CANNOT_FACET: &str = "field cannot be faceted";
const INTERNAL_KEY_PREFIX: &str = "cetra.";
const VALUE_COL: &str = "facet_value";
const COUNT_COL: &str = "facet_count";

struct FacetColumns {
    values: &'static [&'static str],
    bools: &'static [&'static str],
    lists: &'static [&'static str],
    keys: &'static [&'static str],
    numbers: &'static [&'static str],
}

const TRACES_COLUMNS: FacetColumns = FacetColumns {
    values: &[
        "name",
        "status",
        "environment",
        "release",
        "session_id",
        "user_id",
    ],
    bools: &["has_root"],
    lists: &["tags", "models", "prompts"],
    keys: &["metadata"],
    numbers: &[
        "duration_ms",
        "cost_total",
        "total_tokens",
        "input_tokens",
        "output_tokens",
        "span_count",
        "error_count",
        "llm_count",
    ],
};

const SPANS_COLUMNS: FacetColumns = FacetColumns {
    values: &[
        "name",
        "span_kind",
        "status",
        "service_name",
        "environment",
        "release",
        "provider",
        "model",
        "prompt_name",
        "request_model",
        "response_model",
        "operation",
        "cost_source",
        "scope_name",
        "session_id",
        "user_id",
    ],
    bools: &[],
    lists: &["tags"],
    keys: &["metadata", "attributes", "resource_attributes"],
    numbers: &[
        "duration_ms",
        "cost_total",
        "total_tokens",
        "input_tokens",
        "output_tokens",
    ],
};

const SESSIONS_COLUMNS: FacetColumns = FacetColumns {
    values: &["user_id"],
    bools: &[],
    lists: &["tags"],
    keys: &[],
    numbers: &[
        "duration_ms",
        "trace_count",
        "error_count",
        "total_tokens",
        "cost_total",
    ],
};

fn columns_of(view: FacetView) -> &'static FacetColumns {
    match view {
        FacetView::Traces => &TRACES_COLUMNS,
        FacetView::Spans => &SPANS_COLUMNS,
        FacetView::Sessions => &SESSIONS_COLUMNS,
    }
}

fn filter_view(view: FacetView) -> FilterView {
    match view {
        FacetView::Traces => FilterView::Traces,
        FacetView::Spans => FilterView::Spans,
        FacetView::Sessions => FilterView::Sessions,
    }
}

#[derive(Clone, PartialEq, Eq)]
enum ParsedField {
    Values(&'static str),
    Bool(&'static str),
    List(&'static str),
    Keys(&'static str),
    MapValue(&'static str, String),
    Number(&'static str),
}

impl fmt::Debug for ParsedField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Values(col) => f.debug_tuple("Values").field(col).finish(),
            Self::Bool(col) => f.debug_tuple("Bool").field(col).finish(),
            Self::List(col) => f.debug_tuple("List").field(col).finish(),
            Self::Keys(col) => f.debug_tuple("Keys").field(col).finish(),
            Self::MapValue(col, key) => f
                .debug_struct("MapValue")
                .field("col", col)
                .field("key_len", &key.len())
                .finish_non_exhaustive(),
            Self::Number(col) => f.debug_tuple("Number").field(col).finish(),
        }
    }
}

fn parse_field(view: FacetView, text: &str) -> Result<ParsedField, RunError> {
    let columns = columns_of(view);
    let find = |names: &'static [&'static str]| names.iter().find(|name| **name == text).copied();
    if let Some(name) = find(columns.values) {
        return Ok(ParsedField::Values(name));
    }
    if let Some(name) = find(columns.bools) {
        return Ok(ParsedField::Bool(name));
    }
    if let Some(name) = find(columns.lists) {
        return Ok(ParsedField::List(name));
    }
    if let Some(name) = find(columns.keys) {
        return Ok(ParsedField::Keys(name));
    }
    if let Some(name) = find(columns.numbers) {
        return Ok(ParsedField::Number(name));
    }
    for name in columns.keys {
        let key = text
            .strip_prefix(name)
            .and_then(|rest| rest.strip_prefix("['"))
            .and_then(|rest| rest.strip_suffix("']"))
            .and_then(unescape_key);
        if let Some(key) = key {
            return Ok(ParsedField::MapValue(name, key));
        }
    }
    Err(RunError::InvalidRequest(CANNOT_FACET.to_owned()))
}

fn unescape_key(escaped: &str) -> Option<String> {
    let mut key = String::with_capacity(escaped.len());
    let mut chars = escaped.chars();
    while let Some(character) = chars.next() {
        if character == '\'' && chars.next() != Some('\'') {
            return None;
        }
        key.push(character);
    }
    let length = key.chars().count();
    (1..=MAX_FACET_KEY).contains(&length).then_some(key)
}

#[derive(Clone)]
struct CheckedField {
    field: ParsedField,
    query: Option<String>,
    limit: usize,
}

impl fmt::Debug for CheckedField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CheckedField")
            .field("field", &self.field)
            .field("has_query", &self.query.is_some())
            .field("query_len", &self.query.as_ref().map(String::len))
            .field("limit", &self.limit)
            .finish()
    }
}

fn check_field(view: FacetView, field: &FacetField<'_>) -> Result<CheckedField, RunError> {
    if !(1..=MAX_FACET_LIMIT).contains(&field.limit) {
        return Err(RunError::InvalidRequest(format!(
            "limit must be between 1 and {MAX_FACET_LIMIT}"
        )));
    }
    let query = match field.query {
        Some(text) => {
            let trimmed = text.trim();
            if !(1..=MAX_FACET_QUERY).contains(&trimmed.chars().count()) {
                return Err(RunError::InvalidRequest(format!(
                    "q must be 1 to {MAX_FACET_QUERY} characters"
                )));
            }
            Some(trimmed.to_owned())
        }
        None => None,
    };
    let parsed = parse_field(view, field.field)?;
    Ok(CheckedField {
        field: parsed,
        query,
        limit: field.limit,
    })
}

/// Shared-scan native query engine implementation of `facets`.
pub async fn facets(
    ctx: &QueryContext,
    request: FacetsRequest<'_>,
) -> Result<Vec<Result<FacetData, RunError>>, RunError> {
    let ctx = ctx.engine();
    if !(1..=MAX_FACET_FIELDS).contains(&request.fields.len()) {
        return Err(RunError::InvalidRequest(format!(
            "fields must hold 1 to {MAX_FACET_FIELDS} fields"
        )));
    }
    if request.view == FacetView::Sessions && present(request.search).is_some() {
        return Err(RunError::InvalidRequest(
            "search is not supported for sessions".to_owned(),
        ));
    }

    let filter = compile_optional_filter(filter_view(request.view), request.filter)?;
    let checked: Vec<Result<CheckedField, RunError>> = request
        .fields
        .iter()
        .map(|field| check_field(request.view, field))
        .collect();

    if checked.iter().all(Result::is_err) {
        return Ok(checked
            .into_iter()
            .map(|entry| match entry {
                Err(error) => Err(error),
                Ok(_) => Err(internal("a valid field reached the error-only path")),
            })
            .collect());
    }

    let search_text = present(request.search).map(str::to_owned);
    if search_text.is_some() && request.scope.window.is_none() {
        return Err(RunError::InvalidRequest(
            "search needs a time range".to_owned(),
        ));
    }

    let deadline = tokio::time::Instant::now() + request.timeout;
    let valid_fields: Vec<(usize, CheckedField)> = checked
        .iter()
        .enumerate()
        .filter_map(|(idx, entry)| entry.as_ref().ok().cloned().map(|f| (idx, f)))
        .collect();

    let mut slots: Vec<Result<FacetData, RunError>> = checked
        .into_iter()
        .map(|entry| match entry {
            Err(e) => Err(e),
            Ok(_) => Err(RunError::Timeout),
        })
        .collect();

    // Pass 1: 1 shared scan across all valid fields.
    let pass1_timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
    let pass1_out = {
        let ctx_c = ctx.clone();
        let view = request.view;
        let scope = request.scope;
        let filter_c = filter.clone();
        let search_c = search_text.clone();
        let fields_c = valid_fields.clone();
        ctx.run_with_ctx(pass1_timeout, move |exec_ctx| {
            run_pass1(
                &ctx_c,
                exec_ctx,
                view,
                scope,
                filter_c,
                search_c.as_deref(),
                &fields_c,
            )
            .map_err(run_err_to_query)
        })
        .await
    };

    let (pass1_done, pending_histograms) = match pass1_out {
        Ok((out, _)) => out,
        Err(RunError::Timeout) => {
            // Preserve pre-validated InvalidRequest slots; mark valid slots Timeout.
            return Ok(slots);
        }
        Err(other) => return Err(other),
    };

    for (slot_idx, res) in pass1_done {
        slots[slot_idx] = res;
    }

    if pending_histograms.is_empty() {
        return Ok(slots);
    }

    // Pass 2: at most 1 narrow scan projecting only histogram numeric columns where p99 > min.
    let pass2_timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
    if pass2_timeout.is_zero() {
        for h in pending_histograms {
            slots[h.slot_idx] = Err(RunError::Timeout);
        }
        return Ok(slots);
    }
    let pass2_out = {
        let ctx_c = ctx.clone();
        let view = request.view;
        let scope = request.scope;
        let filter_c = filter;
        let search_c = search_text;
        let pending_c = pending_histograms.clone();
        ctx.run_with_ctx(pass2_timeout, move |exec_ctx| {
            run_pass2(
                &ctx_c,
                exec_ctx,
                view,
                scope,
                filter_c,
                search_c.as_deref(),
                &pending_c,
            )
            .map_err(run_err_to_query)
        })
        .await
    };

    match pass2_out {
        Ok((completed, _)) => {
            for (slot_idx, res) in completed {
                slots[slot_idx] = res;
            }
            Ok(slots)
        }
        Err(RunError::Timeout) => {
            // Preserve completed Pass 1 Values/Keys/0-or-1-bucket Histograms; mark only Pass 2
            // histograms as Timeout.
            for h in pending_histograms {
                slots[h.slot_idx] = Err(RunError::Timeout);
            }
            Ok(slots)
        }
        Err(other) => Err(other),
    }
}

#[derive(Debug, Clone)]
struct PendingHistogram {
    slot_idx: usize,
    col_name: &'static str,
    total: u64,
    present: u64,
    min: f64,
    max: f64,
    p50: f64,
    p95: f64,
    p99: f64,
}

type FacetSlotResults = Vec<(usize, Result<FacetData, RunError>)>;
type Pass1Result = (FacetSlotResults, Vec<PendingHistogram>);

fn run_pass1(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    view: FacetView,
    scope: ListScope,
    filter: Option<CompiledFilter>,
    search: Option<&str>,
    valid_fields: &[(usize, CheckedField)],
) -> Result<Pass1Result, RunError> {
    let field_refs: Vec<&ParsedField> = valid_fields.iter().map(|(_, f)| &f.field).collect();
    let StreamSet {
        streams,
        schema: input_schema,
    } = open_facets_streams(ctx, exec_ctx, view, scope, filter, search, &field_refs)?;

    let utf8_schema = single_utf8_schema()?;
    let agg_spec = single_utf8_agg_spec(&utf8_schema)?;

    if streams.is_empty() {
        let mut done = Vec::with_capacity(valid_fields.len());
        for (slot_idx, checked) in valid_fields {
            let data = match checked.field {
                ParsedField::Number(_) => empty_histogram(0),
                ParsedField::Keys(_) => FacetData {
                    kind: FacetKind::Keys,
                    total: 0,
                    missing: 0,
                    values: Vec::new(),
                    more: false,
                    histogram: None,
                },
                _ => FacetData {
                    kind: FacetKind::Values,
                    total: 0,
                    missing: 0,
                    values: Vec::new(),
                    more: false,
                    histogram: None,
                },
            };
            done.push((*slot_idx, Ok(data)));
        }
        return Ok((done, Vec::new()));
    }

    // Keep headroom on `ScopedPool` so one runaway spillable `HashAgg` cannot starve siblings'
    // unspillable reservations (`scratch`, `ParquetSource`, or `finalize_value_field` output).
    let has_value_fields = valid_fields
        .iter()
        .any(|(_, f)| !matches!(f.field, ParsedField::Number(_)));
    let _headroom = has_value_fields.then(|| exec_ctx.pool.register("facet_headroom", true));

    let sinks = exec_run_pipeline(exec_ctx, streams, |_| Ok(Vec::new()), {
        let fields_c = valid_fields.to_vec();
        let schema_c = Arc::clone(&input_schema);
        let utf8_c = Arc::clone(&utf8_schema);
        let agg_c = agg_spec.clone();
        let ctx_c = exec_ctx.clone();
        move |_| FacetFanoutSink::new(view, &fields_c, &schema_c, &utf8_c, &agg_c, ctx_c.clone())
    })
    .map_err(query_err_to_run)?;

    combine_pass1_sinks(sinks, valid_fields, &agg_spec, &utf8_schema, exec_ctx)
}

fn combine_pass1_sinks(
    mut sinks: Vec<FacetFanoutSink>,
    valid_fields: &[(usize, CheckedField)],
    agg_spec: &HashAggSpec,
    utf8_schema: &Arc<Schema>,
    exec_ctx: &ExecContext,
) -> Result<Pass1Result, RunError> {
    let mut done = Vec::new();
    let mut pending = Vec::new();

    for (pos, (slot_idx, checked)) in valid_fields.iter().enumerate() {
        match &checked.field {
            ParsedField::Number(col_name) => {
                let mut total = 0u64;
                let mut present_cnt = 0u64;
                let mut min_val = f64::INFINITY;
                let mut max_val = f64::NEG_INFINITY;
                let mut sketch = DdSketch::new();

                for sink in &sinks {
                    if let FieldWorkerState::Histogram(st) = &sink.states[pos] {
                        total = total.saturating_add(st.total);
                        present_cnt = present_cnt.saturating_add(st.present);
                        if st.present > 0 {
                            min_val = min_val.min(st.min);
                            max_val = max_val.max(st.max);
                            sketch.merge(&st.sketch);
                        }
                    }
                }

                if present_cnt == 0 {
                    done.push((*slot_idx, Ok(empty_histogram(total))));
                    continue;
                }

                let p50 = sketch
                    .quantile(0.5)
                    .map_err(query_err_to_run)?
                    .ok_or_else(|| internal("non-empty sketch returned None p50"))?;
                let p95 = sketch
                    .quantile(0.95)
                    .map_err(query_err_to_run)?
                    .ok_or_else(|| internal("non-empty sketch returned None p95"))?;
                let p99 = sketch
                    .quantile(0.99)
                    .map_err(query_err_to_run)?
                    .ok_or_else(|| internal("non-empty sketch returned None p99"))?;

                if p99 <= min_val {
                    let data = FacetData {
                        kind: FacetKind::Histogram,
                        total,
                        missing: total.saturating_sub(present_cnt),
                        values: Vec::new(),
                        more: false,
                        histogram: Some(Histogram {
                            min: min_val,
                            max: max_val,
                            p50,
                            p95,
                            p99,
                            buckets: vec![HistogramBucket {
                                lo: min_val,
                                hi: max_val,
                                count: present_cnt,
                            }],
                        }),
                    };
                    done.push((*slot_idx, Ok(data)));
                } else {
                    pending.push(PendingHistogram {
                        slot_idx: *slot_idx,
                        col_name,
                        total,
                        present: present_cnt,
                        min: min_val,
                        max: max_val,
                        p50,
                        p95,
                        p99,
                    });
                }
            }
            _ => {
                let kind = if matches!(checked.field, ParsedField::Keys(_)) {
                    FacetKind::Keys
                } else {
                    FacetKind::Values
                };
                let mut total = 0u64;
                let mut missing = 0u64;
                let mut any_too_large = false;
                let mut field_aggs = Vec::with_capacity(sinks.len());

                for sink in &mut sinks {
                    if let FieldWorkerState::Values(st) = &mut sink.states[pos] {
                        total = total.saturating_add(st.total);
                        missing = missing.saturating_add(st.missing);
                        if st.failed_too_large {
                            any_too_large = true;
                        }
                        if let Some(agg) = st.agg.take() {
                            field_aggs.push(agg);
                        }
                    }
                }

                if any_too_large {
                    drop(field_aggs);
                    done.push((*slot_idx, Err(RunError::TooLarge)));
                    continue;
                }

                match finalize_value_field(
                    field_aggs,
                    agg_spec,
                    utf8_schema,
                    exec_ctx,
                    ValueFieldMeta {
                        kind,
                        total,
                        missing,
                        limit: checked.limit,
                    },
                ) {
                    Ok(data) => done.push((*slot_idx, Ok(data))),
                    Err(QueryError::TooLarge) => {
                        done.push((*slot_idx, Err(RunError::TooLarge)));
                    }
                    Err(err) => return Err(query_err_to_run(err)),
                }
            }
        }
    }

    Ok((done, pending))
}

#[derive(Clone, Copy)]
struct ValueFieldMeta {
    kind: FacetKind,
    total: u64,
    missing: u64,
    limit: usize,
}

fn finalize_value_field(
    field_aggs: Vec<HashAgg>,
    agg_spec: &HashAggSpec,
    utf8_schema: &Arc<Schema>,
    exec_ctx: &ExecContext,
    meta: ValueFieldMeta,
) -> Result<FacetData, QueryError> {
    let ValueFieldMeta {
        kind,
        total,
        missing,
        limit,
    } = meta;
    if field_aggs.is_empty() {
        return Ok(FacetData {
            kind,
            total,
            missing,
            values: Vec::new(),
            more: false,
            histogram: None,
        });
    }

    let mut stream = HashAgg::combine_sinks_to_stream(field_aggs, agg_spec, utf8_schema, exec_ctx)?;
    let mut heap: BinaryHeap<HeapFacetEntry> = BinaryHeap::with_capacity(limit + 1);
    let mut res = exec_ctx.pool.register("facet_topk", false);

    while let Some(batch) = stream.next_batch(&exec_ctx.eval.cancel, Some(&mut res))? {
        let val_col = batch
            .column_by_name(VALUE_COL)
            .ok_or_else(|| QueryError::internal("missing facet_value column"))?;
        let cnt_col = batch
            .column_by_name(COUNT_COL)
            .ok_or_else(|| QueryError::internal("missing facet_count column"))?;
        for phys_row in active_rows(&batch) {
            let val = raw_string_at(val_col, phys_row)
                .map_err(|e| QueryError::internal(e.to_string()))?
                .ok_or_else(|| QueryError::internal("facet value is null"))?;
            let cnt = u64::try_from(
                raw_i64_at(cnt_col, phys_row).map_err(|e| QueryError::internal(e.to_string()))?,
            )
            .map_err(|_| QueryError::internal("facet count is negative"))?;

            let entry = HeapFacetEntry {
                value: val,
                count: cnt,
            };
            if heap.len() <= limit {
                heap.push(entry);
            } else if let Some(worst) = heap.peek()
                && entry < *worst
            {
                let _ = heap.pop();
                heap.push(entry);
            }
        }
        drop(batch);
        res.free();
    }

    let more = heap.len() > limit;
    let mut entries: Vec<HeapFacetEntry> = heap.into_vec();
    entries.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.value.cmp(&b.value)));
    entries.truncate(limit);

    let values = entries
        .into_iter()
        .map(|e| FacetValue {
            value: e.value,
            count: e.count,
        })
        .collect();

    Ok(FacetData {
        kind,
        total,
        missing,
        values,
        more,
        histogram: None,
    })
}

/// Ordered so `Ord::max` is the WORST entry (smallest count, or equal count and largest value)
/// so a bounded max-heap of size `limit + 1` keeps the top `limit + 1` best entries.
#[derive(Eq, PartialEq)]
struct HeapFacetEntry {
    value: String,
    count: u64,
}

impl Ord for HeapFacetEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Worse entry compares Greater so it stays at the max-heap root.
        other
            .count
            .cmp(&self.count)
            .then_with(|| self.value.cmp(&other.value))
    }
}

impl PartialOrd for HeapFacetEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn run_pass2(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    view: FacetView,
    scope: ListScope,
    filter: Option<CompiledFilter>,
    search: Option<&str>,
    pending: &[PendingHistogram],
) -> Result<FacetSlotResults, RunError> {
    let parsed: Vec<ParsedField> = pending
        .iter()
        .map(|h| ParsedField::Number(h.col_name))
        .collect();
    let field_refs: Vec<&ParsedField> = parsed.iter().collect();

    let StreamSet {
        streams,
        schema: input_schema,
    } = open_facets_streams(ctx, exec_ctx, view, scope, filter, search, &field_refs)?;

    let mut tallies = vec![[0u64; HISTOGRAM_BUCKETS + 1]; pending.len()];
    if !streams.is_empty() {
        let sinks = exec_run_pipeline(exec_ctx, streams, |_| Ok(Vec::new()), {
            let pending_c = pending.to_vec();
            let schema_c = Arc::clone(&input_schema);
            let cancel_c = exec_ctx.eval.cancel.clone();
            move |_| BucketFanoutSink::new(view, &pending_c, &schema_c, cancel_c.clone())
        })
        .map_err(query_err_to_run)?;

        for sink in sinks {
            for (i, counts) in sink.tallies.iter().enumerate() {
                for (b, &cnt) in counts.iter().enumerate() {
                    tallies[i][b] = tallies[i][b].saturating_add(cnt);
                }
            }
        }
    }

    let mut out = Vec::with_capacity(pending.len());
    for (i, h) in pending.iter().enumerate() {
        let width = (h.p99 / HISTOGRAM_BUCKETS as f64) - (h.min / HISTOGRAM_BUCKETS as f64);
        let length = if h.max >= h.p99 {
            HISTOGRAM_BUCKETS + 1
        } else {
            HISTOGRAM_BUCKETS
        };
        let buckets = tallies[i]
            .iter()
            .take(length)
            .enumerate()
            .map(|(index, count)| {
                if index < HISTOGRAM_BUCKETS {
                    let lo = h.min + index as f64 * width;
                    let hi = if index + 1 == HISTOGRAM_BUCKETS {
                        h.p99
                    } else {
                        h.min + (index + 1) as f64 * width
                    };
                    HistogramBucket {
                        lo,
                        hi,
                        count: *count,
                    }
                } else {
                    HistogramBucket {
                        lo: h.p99,
                        hi: h.max,
                        count: *count,
                    }
                }
            })
            .collect();

        out.push((
            h.slot_idx,
            Ok(FacetData {
                kind: FacetKind::Histogram,
                total: h.total,
                missing: h.total.saturating_sub(h.present),
                values: Vec::new(),
                more: false,
                histogram: Some(Histogram {
                    min: h.min,
                    max: h.max,
                    p50: h.p50,
                    p95: h.p95,
                    p99: h.p99,
                    buckets,
                }),
            }),
        ));
    }

    Ok(out)
}

fn open_facets_streams(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    view: FacetView,
    scope: ListScope,
    filter: Option<CompiledFilter>,
    search: Option<&str>,
    fields: &[&ParsedField],
) -> Result<StreamSet, RunError> {
    if view == FacetView::Sessions {
        let mut spec = scoped_traces_spec(ctx, scope, false)?;
        let traces_raw = traces_raw_schema()?;
        spec.projection = Some(sessions_from_traces_projection(&traces_raw)?);
        let proj_schema = ctx
            .traces
            .projected_schema(&spec)
            .map_err(query_err_to_run)?;
        let raw_streams = ctx
            .traces
            .open_partitions(&spec, exec_ctx)
            .map_err(query_err_to_run)?;
        let sess =
            sessions_from_traces(raw_streams, &proj_schema, exec_ctx).map_err(query_err_to_run)?;
        let streams = match filter {
            Some(f) => apply_compiled_filter(ctx, exec_ctx, sess.streams, &sess.schema, &f)
                .map_err(query_err_to_run)?,
            None => sess.streams,
        };
        return Ok(StreamSet {
            streams,
            schema: sess.schema,
        });
    }

    let references_environment = filter.as_ref().is_some_and(|f| f.references_environment);
    let uses_scores = filter.as_ref().is_some_and(|f| !f.scores.is_empty());

    let search_pred = match search {
        Some(text) => search_predicate(ctx, exec_ctx, filter_view(view), text, scope.window)?,
        None => SearchPredicate::None,
    };

    if uses_scores {
        let mut spec = match view {
            FacetView::Traces => scoped_traces_spec(ctx, scope, references_environment)?,
            FacetView::Spans => scoped_spans_spec(ctx, scope, references_environment)?,
            FacetView::Sessions => return Err(internal("unreachable sessions branch")),
        };
        if let SearchPredicate::Spans {
            filter: s_expr,
            substrings,
        } = &search_pred
        {
            spec.filters.push(s_expr.clone());
            if !substrings.is_empty() {
                spec.substrings.push(substrings.clone());
            }
        }

        let (source, raw_schema, view_schema) = match view {
            FacetView::Traces => (
                Arc::clone(&ctx.traces) as Arc<dyn ScanSource>,
                traces_raw_schema()?,
                traces_view_schema()?,
            ),
            FacetView::Spans => (
                Arc::clone(&ctx.spans),
                spans_raw_schema()?,
                spans_view_schema()?,
            ),
            FacetView::Sessions => return Err(internal("unreachable sessions branch")),
        };

        let mut raw_streams = source
            .open_partitions(&spec, exec_ctx)
            .map_err(query_err_to_run)?;

        if let SearchPredicate::Traces {
            trace_ids,
            schema: ids_schema,
        } = search_pred
        {
            if raw_streams.is_empty() {
                return Ok(StreamSet {
                    streams: Vec::new(),
                    schema: view_schema,
                });
            }
            let left_key = col_expr(&raw_schema, traces::TRACE_ID)?;
            let right_key = col_expr(&ids_schema, spans::TRACE_ID)?;
            let joined = execute_hash_join(
                raw_streams,
                Arc::clone(&raw_schema),
                trace_ids,
                ids_schema,
                HashJoinSpec {
                    join_type: JoinType::LeftSemi,
                    left_keys: vec![left_key],
                    right_keys: vec![right_key],
                    null_equals_null: false,
                    residual_filter: None,
                },
                exec_ctx,
            )
            .map_err(query_err_to_run)?;
            raw_streams = vec![joined];
        }

        let view_streams: Vec<Box<dyn PartitionStream>> = raw_streams
            .into_iter()
            .map(|s| {
                let proj = match view {
                    FacetView::Traces => traces_view_projection(&raw_schema, exec_ctx.clone())?,
                    FacetView::Spans => spans_view_projection(&raw_schema, exec_ctx.clone())?,
                    FacetView::Sessions => {
                        return Err(QueryError::internal("unreachable sessions branch"));
                    }
                };
                Ok(
                    Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                        as Box<dyn PartitionStream>,
                )
            })
            .collect::<Result<Vec<_>, QueryError>>()
            .map_err(query_err_to_run)?;

        let filtered = match filter {
            Some(f) => apply_compiled_filter(ctx, exec_ctx, view_streams, &view_schema, &f)
                .map_err(query_err_to_run)?,
            None => view_streams,
        };
        return Ok(StreamSet {
            streams: filtered,
            schema: view_schema,
        });
    }

    // Fast narrow projection path (no score joins).
    let (source, raw_schema, hex_cols, compute_trace_duration, mut spec): (
        Arc<dyn ScanSource>,
        Arc<Schema>,
        &'static [&'static str],
        bool,
        ScanSpec,
    ) = match view {
        FacetView::Traces => (
            Arc::clone(&ctx.traces) as Arc<dyn ScanSource>,
            traces_raw_schema()?,
            &[traces::TRACE_ID, traces::ROOT_SPAN_ID],
            true,
            scoped_traces_spec(ctx, scope, references_environment)?,
        ),
        FacetView::Spans => (
            Arc::clone(&ctx.spans),
            spans_raw_schema()?,
            &[spans::TRACE_ID, spans::SPAN_ID, spans::PARENT_SPAN_ID],
            false,
            scoped_spans_spec(ctx, scope, references_environment)?,
        ),
        FacetView::Sessions => return Err(internal("unreachable sessions branch")),
    };

    if let Some(f) = filter {
        let rewritten =
            rewrite_view_expr_for_raw(&f.expr, &raw_schema, hex_cols, compute_trace_duration)
                .map_err(query_err_to_run)?;
        spec.filters.push(rewritten);
        populate_id_lookups(&mut spec);
    }

    if let SearchPredicate::Spans {
        filter: s_expr,
        substrings,
    } = &search_pred
    {
        spec.filters.push(s_expr.clone());
        if !substrings.is_empty() {
            spec.substrings.push(substrings.clone());
        }
    }

    for filter_expr in &spec.filters {
        collect_expr_map_projections(filter_expr, &raw_schema, &mut spec.map_projections);
    }

    let mut needed = BTreeSet::new();
    for field in fields {
        match field {
            ParsedField::Values(c) | ParsedField::Bool(c) | ParsedField::List(c) => {
                needed.insert(col_index(&raw_schema, c)?);
            }
            ParsedField::Keys(c) => {
                needed.insert(col_index(&raw_schema, c)?);
                spec.map_projections
                    .entry((*c).to_owned())
                    .and_modify(|existing| existing.merge(MapProjection::KeysOnly))
                    .or_insert(MapProjection::KeysOnly);
            }
            ParsedField::MapValue(c, key) => {
                needed.insert(col_index(&raw_schema, c)?);
                let proj = MapProjection::Keys(BTreeSet::from([key.clone()]));
                spec.map_projections
                    .entry((*c).to_owned())
                    .and_modify(|existing| existing.merge(proj.clone()))
                    .or_insert(proj);
            }
            ParsedField::Number(c) => {
                if *c == DURATION_MS {
                    match view {
                        FacetView::Traces => {
                            needed.insert(col_index(&raw_schema, traces::START_TIME)?);
                            needed.insert(col_index(&raw_schema, traces::END_TIME)?);
                        }
                        FacetView::Spans => {
                            needed.insert(col_index(&raw_schema, spans::DURATION_NS)?);
                        }
                        FacetView::Sessions => {
                            needed.insert(col_index(&raw_schema, DURATION_MS)?);
                        }
                    }
                } else {
                    needed.insert(col_index(&raw_schema, c)?);
                }
            }
        }
    }

    if matches!(search_pred, SearchPredicate::Traces { .. }) {
        needed.insert(col_index(&raw_schema, traces::TRACE_ID)?);
    }

    let proj_indices: Vec<usize> = needed.into_iter().collect();
    spec.projection = Some(proj_indices);
    let proj_schema = source.projected_schema(&spec).map_err(query_err_to_run)?;
    let streams = source
        .open_partitions(&spec, exec_ctx)
        .map_err(query_err_to_run)?;

    if let SearchPredicate::Traces {
        trace_ids,
        schema: ids_schema,
    } = search_pred
    {
        if streams.is_empty() {
            return Ok(StreamSet {
                streams: Vec::new(),
                schema: proj_schema,
            });
        }
        let left_key = col_expr(&proj_schema, traces::TRACE_ID)?;
        let right_key = col_expr(&ids_schema, spans::TRACE_ID)?;
        let joined = execute_hash_join(
            streams,
            Arc::clone(&proj_schema),
            trace_ids,
            ids_schema,
            HashJoinSpec {
                join_type: JoinType::LeftSemi,
                left_keys: vec![left_key],
                right_keys: vec![right_key],
                null_equals_null: false,
                residual_filter: None,
            },
            exec_ctx,
        )
        .map_err(query_err_to_run)?;
        return Ok(StreamSet {
            streams: vec![joined],
            schema: proj_schema,
        });
    }

    Ok(StreamSet {
        streams,
        schema: proj_schema,
    })
}

#[derive(Clone, Copy)]
enum NumericSource {
    Column(usize),
    SpanDurationMs(usize),
    TraceDurationMs { start_idx: usize, end_idx: usize },
}

impl NumericSource {
    fn resolve(view: FacetView, schema: &Schema, col_name: &str) -> Result<Self, QueryError> {
        if let Some(idx) = schema.index_of(col_name) {
            return Ok(Self::Column(idx));
        }
        if col_name == DURATION_MS {
            match view {
                FacetView::Spans => {
                    let idx = schema
                        .index_of(spans::DURATION_NS)
                        .ok_or_else(|| QueryError::internal("missing duration_ns on spans"))?;
                    return Ok(Self::SpanDurationMs(idx));
                }
                FacetView::Traces => {
                    let start_idx = schema
                        .index_of(traces::START_TIME)
                        .ok_or_else(|| QueryError::internal("missing start_time on traces"))?;
                    let end_idx = schema
                        .index_of(traces::END_TIME)
                        .ok_or_else(|| QueryError::internal("missing end_time on traces"))?;
                    return Ok(Self::TraceDurationMs { start_idx, end_idx });
                }
                FacetView::Sessions => {}
            }
        }
        Err(QueryError::internal(format!(
            "missing numeric facet column {col_name}"
        )))
    }

    fn read_finite_at(self, batch: &Batch, phys_row: usize) -> Result<Option<f64>, QueryError> {
        let val = match self {
            Self::Column(idx) => {
                let col = batch
                    .column(idx)
                    .ok_or_else(|| QueryError::internal("missing numeric column"))?;
                raw_f64_at(col, phys_row).map_err(|e| QueryError::internal(e.to_string()))?
            }
            Self::SpanDurationMs(idx) => {
                let col = batch
                    .column(idx)
                    .ok_or_else(|| QueryError::internal("missing duration_ns column"))?;
                raw_f64_at(col, phys_row)
                    .map_err(|e| QueryError::internal(e.to_string()))?
                    .map(|ns| ns / 1_000_000.0)
            }
            Self::TraceDurationMs { start_idx, end_idx } => {
                let sc = batch
                    .column(start_idx)
                    .ok_or_else(|| QueryError::internal("missing start_time column"))?;
                let ec = batch
                    .column(end_idx)
                    .ok_or_else(|| QueryError::internal("missing end_time column"))?;
                let start = crate::reads::columns::raw_time_at(sc, phys_row)
                    .map_err(|e| QueryError::internal(e.to_string()))?;
                let end = crate::reads::columns::raw_time_at(ec, phys_row)
                    .map_err(|e| QueryError::internal(e.to_string()))?;
                Some((end.saturating_sub(start)) as f64 / 1_000_000.0)
            }
        };
        Ok(val.filter(|v| v.is_finite()))
    }
}

struct HistogramWorkerState {
    source: NumericSource,
    total: u64,
    present: u64,
    min: f64,
    max: f64,
    sketch: DdSketch,
}

#[derive(Clone)]
enum ValueSource {
    Values(usize),
    Bool(usize),
    List(usize),
    Keys(usize),
    MapValue(usize, String),
}

struct ValueWorkerState {
    source: ValueSource,
    query: Option<(String, String)>,
    total: u64,
    missing: u64,
    agg: Option<HashAgg>,
    failed_too_large: bool,
    utf8_schema: Arc<Schema>,
    scratch: Reservation,
    buf_bytes: usize,
    chunk_buf: Vec<String>,
    row_dedup: HashSet<String>,
}

enum FieldWorkerState {
    Histogram(HistogramWorkerState),
    Values(Box<ValueWorkerState>),
}

struct FacetFanoutSink {
    states: Vec<FieldWorkerState>,
    exec_ctx: ExecContext,
}

impl FacetFanoutSink {
    fn new(
        view: FacetView,
        fields: &[(usize, CheckedField)],
        input_schema: &Schema,
        utf8_schema: &Arc<Schema>,
        agg_spec: &HashAggSpec,
        exec_ctx: ExecContext,
    ) -> Result<Self, QueryError> {
        let mut states = Vec::with_capacity(fields.len());
        for (_, checked) in fields {
            match &checked.field {
                ParsedField::Number(name) => {
                    let source = NumericSource::resolve(view, input_schema, name)?;
                    states.push(FieldWorkerState::Histogram(HistogramWorkerState {
                        source,
                        total: 0,
                        present: 0,
                        min: f64::INFINITY,
                        max: f64::NEG_INFINITY,
                        sketch: DdSketch::new(),
                    }));
                }
                other => {
                    let source = match other {
                        ParsedField::Values(name) => ValueSource::Values(
                            input_schema
                                .index_of(name)
                                .ok_or_else(|| QueryError::internal(format!("missing {name}")))?,
                        ),
                        ParsedField::Bool(name) => ValueSource::Bool(
                            input_schema
                                .index_of(name)
                                .ok_or_else(|| QueryError::internal(format!("missing {name}")))?,
                        ),
                        ParsedField::List(name) => ValueSource::List(
                            input_schema
                                .index_of(name)
                                .ok_or_else(|| QueryError::internal(format!("missing {name}")))?,
                        ),
                        ParsedField::Keys(name) => ValueSource::Keys(
                            input_schema
                                .index_of(name)
                                .ok_or_else(|| QueryError::internal(format!("missing {name}")))?,
                        ),
                        ParsedField::MapValue(name, key) => ValueSource::MapValue(
                            input_schema
                                .index_of(name)
                                .ok_or_else(|| QueryError::internal(format!("missing {name}")))?,
                            key.clone(),
                        ),
                        ParsedField::Number(_) => {
                            return Err(QueryError::internal("unexpected Number in ValueSource"));
                        }
                    };
                    let agg =
                        HashAgg::new(agg_spec.clone(), Arc::clone(utf8_schema), exec_ctx.clone())?;
                    let scratch = exec_ctx.pool.register("facet_field_scratch", false);
                    let query = checked
                        .query
                        .as_ref()
                        .map(|q| (q.clone(), q.to_lowercase()));
                    states.push(FieldWorkerState::Values(Box::new(ValueWorkerState {
                        source,
                        query,
                        total: 0,
                        missing: 0,
                        agg: Some(agg),
                        failed_too_large: false,
                        utf8_schema: Arc::clone(utf8_schema),
                        scratch,
                        buf_bytes: 0,
                        chunk_buf: Vec::new(),
                        row_dedup: HashSet::new(),
                    })));
                }
            }
        }
        Ok(Self { states, exec_ctx })
    }
}

impl PushSink for FacetFanoutSink {
    fn consume(&mut self, batch: Batch) -> Result<ControlFlow, QueryError> {
        if self.exec_ctx.eval.cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        let num_rows = batch.num_rows();
        if num_rows == 0 {
            return Ok(ControlFlow::Continue);
        }

        let batch_limit = self.exec_ctx.batch_size.max(1);
        for state in &mut self.states {
            if self.exec_ctx.eval.cancel.is_cancelled() {
                return Err(QueryError::Cancelled);
            }
            match state {
                FieldWorkerState::Histogram(st) => {
                    st.total = st.total.saturating_add(num_rows as u64);
                    for phys_row in active_rows(&batch) {
                        if let Some(v) = st.source.read_finite_at(&batch, phys_row)? {
                            st.present = st.present.saturating_add(1);
                            if v < st.min {
                                st.min = v;
                            }
                            if v > st.max {
                                st.max = v;
                            }
                            st.sketch.add(v);
                        }
                    }
                }
                FieldWorkerState::Values(st) => {
                    if st.failed_too_large {
                        continue;
                    }
                    st.total = st.total.saturating_add(num_rows as u64);
                    if let Err(err) = push_value_field_batch(st, &batch, batch_limit) {
                        st.scratch.free();
                        st.buf_bytes = 0;
                        st.chunk_buf = Vec::new();
                        st.row_dedup = HashSet::new();
                        match err {
                            QueryError::TooLarge => {
                                // Drop ONLY this field's HashAgg state so sibling fields proceed.
                                st.agg = None;
                                st.failed_too_large = true;
                            }
                            other => return Err(other),
                        }
                    }
                }
            }
        }
        Ok(ControlFlow::Continue)
    }

    fn finish(&mut self) -> Result<(), QueryError> {
        for state in &mut self.states {
            if let FieldWorkerState::Values(st) = state {
                st.scratch.free();
                st.buf_bytes = 0;
                st.chunk_buf = Vec::new();
                st.row_dedup = HashSet::new();
                if st.failed_too_large {
                    continue;
                }
                if let Some(agg) = st.agg.as_mut()
                    && let Err(err) = agg.finish()
                {
                    st.scratch.free();
                    match err {
                        QueryError::TooLarge => {
                            st.agg = None;
                            st.failed_too_large = true;
                        }
                        other => return Err(other),
                    }
                }
            }
        }
        Ok(())
    }
}

struct ChunkFlusher<'a> {
    utf8_schema: &'a Arc<Schema>,
    scratch: &'a mut Reservation,
    buf_bytes: &'a mut usize,
    chunk_buf: &'a mut Vec<String>,
    agg: &'a mut Option<HashAgg>,
    batch_limit: usize,
}

impl ChunkFlusher<'_> {
    fn push(&mut self, value: String) -> Result<(), QueryError> {
        let delta = size_of::<String>().saturating_add(value.len());
        self.scratch.try_grow(delta)?;
        *self.buf_bytes = self.buf_bytes.saturating_add(delta);
        self.chunk_buf.push(value);
        if self.chunk_buf.len() >= self.batch_limit {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), QueryError> {
        if self.chunk_buf.is_empty() {
            return Ok(());
        }
        let len = self.chunk_buf.len();
        let utf8 = Utf8Data::try_from_slices(self.chunk_buf.iter().map(String::as_str))?;
        self.chunk_buf.clear();
        let release = *self.buf_bytes;
        *self.buf_bytes = 0;
        self.scratch
            .shrink(self.scratch.size().saturating_sub(release));
        let flat = FlatColumn::try_new(FlatData::Utf8(utf8), None)?;
        let mini = Batch::try_new(
            Arc::clone(self.utf8_schema),
            vec![Column::Flat(flat)],
            len,
            Selection::All,
        )?;
        let bytes = mini.memory_bytes();
        self.scratch.try_grow(bytes)?;
        let res = match self.agg.as_mut() {
            Some(a) => a.consume(mini),
            None => Err(QueryError::internal("missing facet HashAgg")),
        };
        self.scratch
            .shrink(self.scratch.size().saturating_sub(bytes));
        res?;
        Ok(())
    }
}

fn push_value_field_batch(
    st: &mut ValueWorkerState,
    batch: &Batch,
    batch_limit: usize,
) -> Result<(), QueryError> {
    let ValueWorkerState {
        source,
        query,
        missing,
        agg,
        utf8_schema,
        scratch,
        buf_bytes,
        chunk_buf,
        row_dedup,
        ..
    } = st;
    chunk_buf.clear();
    if *buf_bytes > 0 {
        scratch.shrink(scratch.size().saturating_sub(*buf_bytes));
        *buf_bytes = 0;
    }
    let query_ref = query.as_ref().map(|(q, ql)| (q.as_str(), ql.as_str()));

    match source {
        ValueSource::Values(col_idx) => {
            let col = batch
                .column(*col_idx)
                .ok_or_else(|| QueryError::internal("missing values column"))?;
            let mut flusher = ChunkFlusher {
                utf8_schema,
                scratch,
                buf_bytes,
                chunk_buf,
                agg,
                batch_limit,
            };
            for phys_row in active_rows(batch) {
                let val_opt = raw_string_at(col, phys_row)
                    .map_err(|e| QueryError::internal(e.to_string()))?;
                match val_opt {
                    None => *missing = missing.saturating_add(1),
                    Some(v) => {
                        if matches_query(&v, query_ref) {
                            flusher.push(v)?;
                        }
                    }
                }
            }
            flusher.flush()?;
        }
        ValueSource::Bool(col_idx) => {
            let col = batch
                .column(*col_idx)
                .ok_or_else(|| QueryError::internal("missing bool column"))?;
            let mut flusher = ChunkFlusher {
                utf8_schema,
                scratch,
                buf_bytes,
                chunk_buf,
                agg,
                batch_limit,
            };
            for phys_row in active_rows(batch) {
                let val_opt =
                    raw_bool_at(col, phys_row).map_err(|e| QueryError::internal(e.to_string()))?;
                match val_opt {
                    None => *missing = missing.saturating_add(1),
                    Some(b) => {
                        let s = if b { "true" } else { "false" };
                        if matches_query(s, query_ref) {
                            flusher.push(s.to_owned())?;
                        }
                    }
                }
            }
            flusher.flush()?;
        }
        ValueSource::List(col_idx) => {
            let col = batch
                .column(*col_idx)
                .ok_or_else(|| QueryError::internal("missing list column"))?;
            for phys_row in active_rows(batch) {
                row_dedup.clear();
                let mut dedup_bytes = 0usize;
                let is_empty_or_null =
                    read_list_utf8_row(col, phys_row, row_dedup, scratch, &mut dedup_bytes)?;
                if is_empty_or_null {
                    *missing = missing.saturating_add(1);
                } else {
                    let mut flusher = ChunkFlusher {
                        utf8_schema,
                        scratch,
                        buf_bytes,
                        chunk_buf,
                        agg,
                        batch_limit,
                    };
                    for elem in row_dedup.drain() {
                        if matches_query(&elem, query_ref) {
                            flusher.push(elem)?;
                        }
                    }
                }
                if dedup_bytes > 0 {
                    scratch.shrink(scratch.size().saturating_sub(dedup_bytes));
                }
                if row_dedup.capacity() > batch_limit.saturating_mul(2) {
                    *row_dedup = HashSet::new();
                }
            }
            let mut flusher = ChunkFlusher {
                utf8_schema,
                scratch,
                buf_bytes,
                chunk_buf,
                agg,
                batch_limit,
            };
            flusher.flush()?;
        }
        ValueSource::Keys(col_idx) => {
            let col = batch
                .column(*col_idx)
                .ok_or_else(|| QueryError::internal("missing map column"))?;
            let mut flusher = ChunkFlusher {
                utf8_schema,
                scratch,
                buf_bytes,
                chunk_buf,
                agg,
                batch_limit,
            };
            for phys_row in active_rows(batch) {
                let had_entries = visit_map_row(col, phys_row, |k, _v| {
                    if !k.starts_with(INTERNAL_KEY_PREFIX) && matches_query(k, query_ref) {
                        flusher.push(k.to_owned())?;
                    }
                    Ok(())
                })?;
                if !had_entries {
                    *missing = missing.saturating_add(1);
                }
            }
            flusher.flush()?;
        }
        ValueSource::MapValue(col_idx, target_key) => {
            let col = batch
                .column(*col_idx)
                .ok_or_else(|| QueryError::internal("missing map column"))?;
            let mut flusher = ChunkFlusher {
                utf8_schema,
                scratch,
                buf_bytes,
                chunk_buf,
                agg,
                batch_limit,
            };
            for phys_row in active_rows(batch) {
                let mut found: Option<String> = None;
                let _ = visit_map_row(col, phys_row, |k, v_opt| {
                    if k == target_key.as_str() {
                        found = v_opt.map(str::to_owned);
                    }
                    Ok(())
                })?;
                match found {
                    None => *missing = missing.saturating_add(1),
                    Some(v) => {
                        if matches_query(&v, query_ref) {
                            flusher.push(v)?;
                        }
                    }
                }
            }
            flusher.flush()?;
        }
    }

    Ok(())
}

fn matches_query(value: &str, query: Option<(&str, &str)>) -> bool {
    let Some((raw_q, lower_q)) = query else {
        return true;
    };
    if value.is_ascii() && raw_q.is_ascii() {
        let vb = value.as_bytes();
        let qb = raw_q.as_bytes();
        if qb.len() > vb.len() {
            return false;
        }
        return vb.windows(qb.len()).any(|w| w.eq_ignore_ascii_case(qb));
    }
    value.to_lowercase().contains(lower_q)
}

/// Reads distinct non-null string elements of `col[phys_row]` into `dedup`.
/// Returns `true` iff the list cell is NULL or empty (`cardinality == 0`), BEFORE null element drop.
fn read_list_utf8_row(
    col: &Column,
    phys_row: usize,
    dedup: &mut HashSet<String>,
    scratch: &mut Reservation,
    dedup_bytes: &mut usize,
) -> Result<bool, QueryError> {
    if phys_row >= col.len() || col.is_null(phys_row) {
        return Ok(true);
    }
    match col {
        Column::Const { value, .. } => match value {
            ScalarValue::List { values, .. } => {
                if values.is_empty() {
                    return Ok(true);
                }
                for sv in values {
                    if let ScalarValue::Utf8(s) = sv {
                        insert_dedup(dedup, s.clone(), scratch, dedup_bytes)?;
                    }
                }
                Ok(false)
            }
            ScalarValue::Null(_) => Ok(true),
            _ => Err(QueryError::internal("list scalar mismatch")),
        },
        Column::Flat(fc) => read_list_flat(fc, phys_row, dedup, scratch, dedup_bytes),
        Column::Dict(d) => {
            if d.is_null(phys_row) {
                return Ok(true);
            }
            let code = d.codes()[phys_row] as usize;
            if d.values().is_null(code) {
                return Ok(true);
            }
            read_list_flat(d.values(), code, dedup, scratch, dedup_bytes)
        }
        Column::Rle(r) => {
            let run_idx = r
                .run_ends()
                .partition_point(|&end| (end as usize) <= phys_row);
            if r.values().is_null(run_idx) {
                return Ok(true);
            }
            read_list_flat(r.values(), run_idx, dedup, scratch, dedup_bytes)
        }
    }
}

fn insert_dedup(
    dedup: &mut HashSet<String>,
    s: String,
    scratch: &mut Reservation,
    dedup_bytes: &mut usize,
) -> Result<(), QueryError> {
    if !dedup.contains(&s) {
        let delta = size_of::<String>().saturating_add(s.len());
        scratch.try_grow(delta)?;
        *dedup_bytes = dedup_bytes.saturating_add(delta);
        dedup.insert(s);
    }
    Ok(())
}

fn read_list_flat(
    fc: &FlatColumn,
    flat_row: usize,
    dedup: &mut HashSet<String>,
    scratch: &mut Reservation,
    dedup_bytes: &mut usize,
) -> Result<bool, QueryError> {
    let FlatData::List {
        offsets, values, ..
    } = fc.data()
    else {
        return Err(QueryError::internal("list flat storage mismatch"));
    };
    let start = offsets[flat_row] as usize;
    let end = offsets[flat_row + 1] as usize;
    if start == end {
        return Ok(true);
    }
    for idx in start..end {
        if let Some(s) =
            raw_string_at(values, idx).map_err(|e| QueryError::internal(e.to_string()))?
        {
            insert_dedup(dedup, s, scratch, dedup_bytes)?;
        }
    }
    Ok(false)
}

/// Visits `(key, Option<value>)` pairs of `col[phys_row]`.
/// Returns `true` iff the map cell is non-null AND has at least one entry.
fn visit_map_row(
    col: &Column,
    phys_row: usize,
    mut f: impl FnMut(&str, Option<&str>) -> Result<(), QueryError>,
) -> Result<bool, QueryError> {
    if phys_row >= col.len() || col.is_null(phys_row) {
        return Ok(false);
    }
    match col {
        Column::Const { value, .. } => match value {
            ScalarValue::MapUtf8(entries) => {
                if entries.is_empty() {
                    return Ok(false);
                }
                for (k, v) in entries {
                    f(k.as_str(), v.as_deref())?;
                }
                Ok(true)
            }
            ScalarValue::Null(_) => Ok(false),
            _ => Err(QueryError::internal("map scalar mismatch")),
        },
        Column::Flat(fc) => visit_map_flat(fc, phys_row, f),
        Column::Dict(d) => {
            if d.is_null(phys_row) {
                return Ok(false);
            }
            let code = d.codes()[phys_row] as usize;
            if d.values().is_null(code) {
                return Ok(false);
            }
            visit_map_flat(d.values(), code, f)
        }
        Column::Rle(r) => {
            let run_idx = r
                .run_ends()
                .partition_point(|&end| (end as usize) <= phys_row);
            if r.values().is_null(run_idx) {
                return Ok(false);
            }
            visit_map_flat(r.values(), run_idx, f)
        }
    }
}

fn visit_map_flat(
    fc: &FlatColumn,
    flat_row: usize,
    mut f: impl FnMut(&str, Option<&str>) -> Result<(), QueryError>,
) -> Result<bool, QueryError> {
    let FlatData::MapUtf8(map) = fc.data() else {
        return Err(QueryError::internal("map flat storage mismatch"));
    };
    let start = map.offsets()[flat_row] as usize;
    let end = map.offsets()[flat_row + 1] as usize;
    if start == end {
        return Ok(false);
    }
    let keys = map.keys();
    let values = map.values();
    let val_nulls = map.value_nulls();
    for idx in start..end {
        let k = keys
            .get(idx)
            .ok_or_else(|| QueryError::internal("map key offset out of bounds"))?;
        let v = if val_nulls.is_some_and(|bm| !bm.get(idx)) {
            None
        } else {
            Some(
                values
                    .get(idx)
                    .ok_or_else(|| QueryError::internal("map value offset out of bounds"))?,
            )
        };
        f(k, v)?;
    }
    Ok(true)
}

struct BucketWorkerSpec {
    source: NumericSource,
    min: f64,
    p99: f64,
    width: f64,
}

struct BucketFanoutSink {
    specs: Vec<BucketWorkerSpec>,
    tallies: Vec<[u64; HISTOGRAM_BUCKETS + 1]>,
    cancel: cetra_column::CancelToken,
}

impl BucketFanoutSink {
    fn new(
        view: FacetView,
        pending: &[PendingHistogram],
        schema: &Schema,
        cancel: cetra_column::CancelToken,
    ) -> Result<Self, QueryError> {
        let mut specs = Vec::with_capacity(pending.len());
        for h in pending {
            specs.push(BucketWorkerSpec {
                source: NumericSource::resolve(view, schema, h.col_name)?,
                min: h.min,
                p99: h.p99,
                width: (h.p99 / HISTOGRAM_BUCKETS as f64) - (h.min / HISTOGRAM_BUCKETS as f64),
            });
        }
        let tallies = vec![[0u64; HISTOGRAM_BUCKETS + 1]; pending.len()];
        Ok(Self {
            specs,
            tallies,
            cancel,
        })
    }
}

impl PushSink for BucketFanoutSink {
    fn consume(&mut self, batch: Batch) -> Result<ControlFlow, QueryError> {
        if self.cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        if batch.is_empty() {
            return Ok(ControlFlow::Continue);
        }
        for (i, spec) in self.specs.iter().enumerate() {
            if self.cancel.is_cancelled() {
                return Err(QueryError::Cancelled);
            }
            for phys_row in active_rows(&batch) {
                let Some(x) = spec.source.read_finite_at(&batch, phys_row)? else {
                    continue;
                };
                let bucket_idx = if x >= spec.p99 {
                    HISTOGRAM_BUCKETS
                } else {
                    let diff = x - spec.min;
                    let ratio = if diff.is_finite() {
                        diff / spec.width
                    } else {
                        (x / spec.width) - (spec.min / spec.width)
                    };
                    let raw = ratio as i64;
                    (raw.max(0) as usize).min(HISTOGRAM_BUCKETS - 1)
                };
                self.tallies[i][bucket_idx] = self.tallies[i][bucket_idx].saturating_add(1);
            }
        }
        Ok(ControlFlow::Continue)
    }
}

fn single_utf8_schema() -> Result<Arc<Schema>, RunError> {
    Schema::try_new_arc(vec![Field::new(VALUE_COL, DataType::Utf8, false)])
        .map_err(|e| query_err_to_run(e.into()))
}

fn single_utf8_agg_spec(utf8_schema: &Schema) -> Result<HashAggSpec, RunError> {
    let col = col_expr(utf8_schema, VALUE_COL)?;
    Ok(HashAggSpec {
        group_by: vec![(VALUE_COL.to_owned(), col)],
        aggs: vec![AggExpr {
            func: AggFunc::CountStar,
            filter: None,
            output_name: COUNT_COL.to_owned(),
        }],
    })
}

fn empty_histogram(total: u64) -> FacetData {
    FacetData {
        kind: FacetKind::Histogram,
        total,
        missing: total,
        values: Vec::new(),
        more: false,
        histogram: None,
    }
}

fn col_index(schema: &Schema, name: &str) -> Result<usize, RunError> {
    schema
        .index_of(name)
        .ok_or_else(|| internal(format!("missing column {name} in schema")))
}

fn col_expr(schema: &Schema, name: &str) -> Result<Expr, RunError> {
    let idx = col_index(schema, name)?;
    Ok(Expr::Column {
        index: idx,
        name: name.to_owned(),
        data_type: schema.fields()[idx].data_type.clone(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use bytesize::ByteSize;
    use cetra_core::enums::{SpanKind, SpanStatus};
    use cetra_core::ids::ProjectId;
    use cetra_core::names::{environment, metadata_key};
    use std::time::Duration;

    use cetra_core::score::{Score, ScoreValue};
    use cetra_core::span::Span;
    use cetra_query::exec::SpillManager;

    use super::*;
    use crate::config::{
        FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
    };
    use crate::test_support::{score_row, score_rows, span_row, span_rows};
    use crate::views::{QueryContext, TimeWindow};
    use crate::{QeRuntime, QueryMemory, QueryOptions};

    const T0: i64 = 1_700_000_000_000_000_000;
    const MIN: i64 = 60_000_000_000;

    fn test_config(dir: &std::path::Path) -> StorageConfig {
        StorageConfig {
            delete_objects: true,
            data_dir: dir.to_path_buf(),
            min_free_bytes: ByteSize::b(0),
            object_store: None,
            s3: S3Config::default(),
            wal: WalConfig::default(),
            flush: FlushConfig::default(),
            memtable: MemtableConfig::default(),
            query: QueryConfig {
                target_partitions: 2,
                ..QueryConfig::default()
            },
            anchor: Default::default(),
            gc: Default::default(),
            compaction: Default::default(),
            deletion: Default::default(),
        }
    }

    #[derive(Debug)]
    struct CountingSource {
        inner: Arc<dyn ScanSource>,
        opens: Arc<AtomicUsize>,
    }

    impl ScanSource for CountingSource {
        fn schema(&self) -> Arc<Schema> {
            self.inner.schema()
        }
        fn projected_schema(&self, spec: &ScanSpec) -> Result<Arc<Schema>, QueryError> {
            self.inner.projected_schema(spec)
        }
        fn open_partitions(
            &self,
            spec: &ScanSpec,
            ctx: &ExecContext,
        ) -> Result<Vec<Box<dyn PartitionStream>>, QueryError> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            self.inner.open_partitions(spec, ctx)
        }
        fn explain_summary(&self, spec: &ScanSpec) -> String {
            self.inner.explain_summary(spec)
        }
    }

    #[derive(Debug)]
    struct SlowStream {
        delay: Duration,
    }

    impl PartitionStream for SlowStream {
        fn next_batch(
            &mut self,
            cancel: &cetra_column::CancelToken,
            _res: Option<&mut Reservation>,
        ) -> Result<Option<Batch>, QueryError> {
            let start = std::time::Instant::now();
            while start.elapsed() < self.delay {
                if cancel.is_cancelled() {
                    return Err(QueryError::Cancelled);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None)
        }
    }

    #[derive(Debug)]
    struct Pass2SlowSource {
        inner: Arc<dyn ScanSource>,
        opens: Arc<AtomicUsize>,
        slow_on_open: usize,
        delay: Duration,
    }

    impl ScanSource for Pass2SlowSource {
        fn schema(&self) -> Arc<Schema> {
            self.inner.schema()
        }
        fn projected_schema(&self, spec: &ScanSpec) -> Result<Arc<Schema>, QueryError> {
            self.inner.projected_schema(spec)
        }
        fn open_partitions(
            &self,
            spec: &ScanSpec,
            ctx: &ExecContext,
        ) -> Result<Vec<Box<dyn PartitionStream>>, QueryError> {
            let n = self.opens.fetch_add(1, Ordering::SeqCst) + 1;
            if n == self.slow_on_open {
                return Ok(vec![Box::new(SlowStream { delay: self.delay })]);
            }
            self.inner.open_partitions(spec, ctx)
        }
        fn explain_summary(&self, spec: &ScanSpec) -> String {
            self.inner.explain_summary(spec)
        }
    }

    fn build_sample_data() -> (Vec<Span>, Vec<Score>) {
        let mut spans = Vec::new();
        for t in 0..200_u16 {
            let start = T0 + i64::from(t) * 10_000_000;
            let mut s = span_row(t, 0, start, start);
            s.name = format!("agent-{}", t % 3);
            s.end_time = s.start_time + (i64::from(t) + 1) * 1_000_000;
            s.environment = Some(if t % 2 == 0 { "prod" } else { "staging" }.to_owned());
            s.user_id = if t < 180 {
                Some(format!("u-{}", t % 2))
            } else {
                None
            };
            s.session_id = Some(format!("sess-{}", t % 3));
            s.tags = match t % 3 {
                0 => vec!["alpha".to_owned(), "beta".to_owned()],
                1 => vec!["alpha".to_owned()],
                _ => Vec::new(),
            };
            s.metadata = match t % 4 {
                0 => BTreeMap::from([
                    ("team".to_owned(), "core".to_owned()),
                    ("cetra.internal".to_owned(), "hidden".to_owned()),
                ]),
                1 => BTreeMap::from([("cetra.only".to_owned(), "x".to_owned())]),
                2 => BTreeMap::from([("team".to_owned(), "ml".to_owned())]),
                _ => BTreeMap::new(),
            };
            s.cost_total = Some(1.0 + 0.01 * f64::from(t + 1));
            s.total_tokens = Some(100 * i64::from(t + 1));
            if t == 2 {
                s.status = SpanStatus::Error;
                s.input = Some("needle order".to_owned());
            }
            if t == 4 {
                // Non-finite cost must count as missing on histogram.
                s.cost_total = Some(f64::NAN);
            }
            let mut child = span_row(t, 1, s.start_time + 1000, s.start_time + 1000);
            child.span_kind = SpanKind::Llm;
            child.parent_span_id = Some(s.span_id);
            child.end_time = child.start_time + (i64::from(t) + 1) * 1_000_000 + 500_000;
            child.response_model = Some(if t % 2 == 0 { "gpt-5" } else { "claude" }.to_owned());
            child.attributes = BTreeMap::from([
                ("k1".to_owned(), "v1".to_owned()),
                ("cetra.secret".to_owned(), "skip".to_owned()),
            ]);
            spans.push(s);
            spans.push(child);
        }
        // Eval & experiment traces (excluded by default).
        let mut eval_s = span_row(900, 0, T0 + MIN, T0 + MIN);
        eval_s.environment = Some(environment::EVAL.to_owned());
        spans.push(eval_s);

        let mut exp_s = span_row(901, 0, T0 + MIN, T0 + MIN);
        exp_s
            .metadata
            .insert(metadata_key::EXPERIMENT_ID.to_owned(), "e1".to_owned());
        spans.push(exp_s);

        let mut sc = score_row(1, 0, T0 + MIN, T0 + MIN, T0 + MIN, false, "quality");
        sc.value = ScoreValue::Numeric(0.95);
        (spans, vec![sc])
    }

    #[tokio::test]
    async fn facets_all_views_and_scan_count_le_2() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = crate::Storage::open(test_config(dir.path()))
            .await
            .expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");
        let (spans, scores) = build_sample_data();
        storage
            .append_spans(project, span_rows(&spans))
            .await
            .expect("append spans");
        storage
            .append_scores(project, score_rows(&scores))
            .await
            .expect("append scores");

        let opts = QueryOptions::new(None, None);
        let qe_ctx = EngineContext::from_storage(&storage, project, &opts).expect("qe ctx");
        let ctx = QueryContext::from_engine(qe_ctx.clone());

        // 10 mixed fields on Spans in <= 2 scans.
        let opens = Arc::new(AtomicUsize::new(0));
        let mut counted_engine = qe_ctx.clone();
        counted_engine.spans = Arc::new(CountingSource {
            inner: Arc::clone(&qe_ctx.spans),
            opens: Arc::clone(&opens),
        });
        let counted_ctx = QueryContext::from_engine(counted_engine);

        let span_fields = [
            FacetField {
                field: "name",
                query: None,
                limit: 10,
            },
            FacetField {
                field: "span_kind",
                query: None,
                limit: 10,
            },
            FacetField {
                field: "status",
                query: None,
                limit: 10,
            },
            FacetField {
                field: "model",
                query: Some("GP"),
                limit: 1,
            },
            FacetField {
                field: "user_id",
                query: None,
                limit: 10,
            },
            FacetField {
                field: "tags",
                query: None,
                limit: 10,
            },
            FacetField {
                field: "metadata",
                query: None,
                limit: 10,
            },
            FacetField {
                field: "metadata['team']",
                query: None,
                limit: 10,
            },
            FacetField {
                field: "duration_ms",
                query: None,
                limit: 10,
            },
            FacetField {
                field: "cost_total",
                query: None,
                limit: 10,
            },
        ];
        let span_req = FacetsRequest {
            view: FacetView::Spans,
            filter: None,
            search: None,
            scope: ListScope {
                window: None,
                include_eval: false,
                include_experiments: false,
            },
            fields: &span_fields,
            timeout: Duration::from_secs(10),
        };
        let span_res = facets(&counted_ctx, span_req).await.expect("span facets");
        assert!(
            opens.load(Ordering::SeqCst) <= 2,
            "expected <= 2 scans for 10 facets, got {}",
            opens.load(Ordering::SeqCst)
        );
        assert_eq!(span_res.len(), 10);
        for res in &span_res {
            let f = res.as_ref().expect("span facet ok");
            assert_eq!(f.total, 400);
        }
        let duration_hist = span_res[8]
            .as_ref()
            .unwrap()
            .histogram
            .as_ref()
            .expect("duration histogram");
        assert_eq!(duration_hist.buckets.len(), HISTOGRAM_BUCKETS + 1);

        // Traces with search + score filter + bool + lists + keys + histogram.
        let win = TimeWindow::new(T0 - MIN, T0 + 20 * MIN).expect("win");
        let trace_fields = [
            FacetField {
                field: "has_root",
                query: None,
                limit: 10,
            },
            FacetField {
                field: "tags",
                query: None,
                limit: 10,
            },
            FacetField {
                field: "metadata",
                query: None,
                limit: 10,
            },
            FacetField {
                field: "duration_ms",
                query: None,
                limit: 10,
            },
            FacetField {
                field: "status",
                query: None,
                limit: 10,
            },
        ];
        let trace_req = FacetsRequest {
            view: FacetView::Traces,
            filter: Some("score('quality') > 0.5 OR status = 'ok'"),
            search: Some("needle"),
            scope: ListScope {
                window: Some(win),
                include_eval: false,
                include_experiments: false,
            },
            fields: &trace_fields,
            timeout: Duration::from_secs(10),
        };
        let trace_res = facets(&ctx, trace_req).await.expect("traces facets");
        assert_eq!(trace_res.len(), 5);
        for res in &trace_res {
            let f = res.as_ref().expect("trace facet ok");
            assert_eq!(f.total, 0);
        }

        // Sessions facets.
        let sess_fields = [
            FacetField {
                field: "user_id",
                query: None,
                limit: 10,
            },
            FacetField {
                field: "tags",
                query: None,
                limit: 10,
            },
            FacetField {
                field: "duration_ms",
                query: None,
                limit: 10,
            },
        ];
        let sess_req = FacetsRequest {
            view: FacetView::Sessions,
            filter: Some("trace_count >= 1"),
            search: None,
            scope: ListScope {
                window: Some(win),
                include_eval: false,
                include_experiments: false,
            },
            fields: &sess_fields,
            timeout: Duration::from_secs(10),
        };
        let sess_res = facets(&ctx, sess_req).await.expect("sess facets");
        assert_eq!(sess_res.len(), 3);
        for res in &sess_res {
            let f = res.as_ref().expect("sess facet ok");
            assert!(f.total > 0);
        }

        // All-invalid early return BEFORE search window check.
        let bad_fields = [
            FacetField {
                field: "nonexistent",
                query: None,
                limit: 10,
            },
            FacetField {
                field: "status",
                query: None,
                limit: 0,
            },
        ];
        let early_req = FacetsRequest {
            view: FacetView::Traces,
            filter: None,
            search: Some("needle"),
            scope: ListScope {
                window: None,
                include_eval: false,
                include_experiments: false,
            },
            fields: &bad_fields,
            timeout: Duration::from_secs(10),
        };
        let early_res = facets(&ctx, early_req)
            .await
            .expect("all-invalid must not check search window");
        assert_eq!(early_res.len(), 2);
        assert!(matches!(early_res[0], Err(RunError::InvalidRequest(_))));
        assert!(matches!(early_res[1], Err(RunError::InvalidRequest(_))));

        // Mixed valid + invalid without window with search -> outer InvalidRequest("search needs a time range").
        let mixed_fields = [
            FacetField {
                field: "status",
                query: None,
                limit: 10,
            },
            FacetField {
                field: "bad",
                query: None,
                limit: 10,
            },
        ];
        let err = facets(
            &ctx,
            FacetsRequest {
                view: FacetView::Traces,
                filter: None,
                search: Some("needle"),
                scope: ListScope {
                    window: None,
                    include_eval: false,
                    include_experiments: false,
                },
                fields: &mixed_fields,
                timeout: Duration::from_secs(10),
            },
        )
        .await
        .expect_err("mixed with search and no window must fail");
        assert!(matches!(err, RunError::InvalidRequest(ref m) if m == "search needs a time range"));

        let _ = storage.shutdown().await;
    }

    #[tokio::test]
    async fn per_field_too_large_isolation_keeps_siblings_ok() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = crate::Storage::open(test_config(dir.path()))
            .await
            .expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");

        // Create many distinct long `name` values across multiple batches and only 2 distinct `status` values.
        for chunk in 0..8_u16 {
            let mut spans = Vec::with_capacity(500);
            for i in 0..500_u16 {
                let t = chunk * 500 + i;
                let mut s = span_row(t, 0, T0 + i64::from(t) * 1000, T0 + i64::from(t) * 1000);
                s.name = format!("unique-long-span-name-{t:06}-{}", "x".repeat(64));
                s.status = if t % 2 == 0 {
                    SpanStatus::Ok
                } else {
                    SpanStatus::Error
                };
                spans.push(s);
            }
            storage
                .append_spans(project, span_rows(&spans))
                .await
                .expect("append");
        }

        let mut ctx =
            EngineContext::from_storage(&storage, project, &QueryOptions::new(None, None))
                .expect("ctx");
        // Tight memory pool + 0-byte spill cap so high-cardinality `name` hits TooLarge while low-cardinality `status` succeeds.
        let cap = 320 * 1024;
        let admission = Arc::new(crate::engine::admission::AsyncAdmissionPool::new(cap));
        let spill = Arc::new(SpillManager::with_fixed_cap(dir.path().join("spill"), 0));
        ctx.runtime = Arc::new(QeRuntime {
            admission,
            workers: Default::default(),
            workers_init: Default::default(),
            metadata_cache: Arc::clone(&ctx.runtime.metadata_cache),
            spill,
            target_partitions: 1,
            query_memory: QueryMemory {
                initial: 0,
                cap,
                wait: Duration::from_secs(1),
            },
        });

        let fields = [
            FacetField {
                field: "status",
                query: None,
                limit: 10,
            },
            FacetField {
                field: "name",
                query: None,
                limit: 10,
            },
        ];
        let res = facets(
            &QueryContext::from_engine(ctx),
            FacetsRequest {
                view: FacetView::Spans,
                filter: None,
                search: None,
                scope: ListScope {
                    window: None,
                    include_eval: false,
                    include_experiments: false,
                },
                fields: &fields,
                timeout: Duration::from_secs(10),
            },
        )
        .await
        .expect("facets");

        let status_res = res[0].as_ref().expect("status sibling must succeed");
        assert_eq!(status_res.total, 4000);
        assert_eq!(status_res.values.len(), 2);
        assert!(
            matches!(res[1], Err(RunError::TooLarge)),
            "high-cardinality field must fail with TooLarge, got {:?}",
            res[1]
        );

        let _ = storage.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pass1_and_pass2_timeout_isolation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = crate::Storage::open(test_config(dir.path()))
            .await
            .expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");

        let mut spans = Vec::new();
        for t in 0..20_u16 {
            let mut s = span_row(t, 0, T0 + i64::from(t) * MIN, T0 + i64::from(t) * MIN);
            s.end_time = s.start_time + (i64::from(t) + 1) * 1_000_000;
            s.total_tokens = Some(100);
            spans.push(s);
        }
        storage
            .append_spans(project, span_rows(&spans))
            .await
            .expect("append");

        let base_ctx =
            EngineContext::from_storage(&storage, project, &QueryOptions::new(None, None))
                .expect("ctx");

        let fields = [
            FacetField {
                field: "status",
                query: None,
                limit: 10,
            },
            FacetField {
                field: "nonexistent",
                query: None,
                limit: 10,
            },
            FacetField {
                field: "duration_ms",
                query: None,
                limit: 10,
            },
        ];

        // Pass 1 timeout: preserves InvalidRequest, marks valid fields Timeout.
        let mut p1_ctx = base_ctx.clone();
        p1_ctx.spans = Arc::new(Pass2SlowSource {
            inner: Arc::clone(&base_ctx.spans),
            opens: Arc::new(AtomicUsize::new(0)),
            slow_on_open: 1,
            delay: Duration::from_millis(400),
        });
        let p1_res = facets(
            &QueryContext::from_engine(p1_ctx),
            FacetsRequest {
                view: FacetView::Spans,
                filter: None,
                search: None,
                scope: ListScope {
                    window: None,
                    include_eval: false,
                    include_experiments: false,
                },
                fields: &fields,
                timeout: Duration::from_millis(50),
            },
        )
        .await
        .expect("pass1 timeout returns per-field results");
        assert!(matches!(p1_res[0], Err(RunError::Timeout)));
        assert!(matches!(p1_res[1], Err(RunError::InvalidRequest(_))));
        assert!(matches!(p1_res[2], Err(RunError::Timeout)));

        // Pass 2 timeout: preserves Pass 1 completed `status` as Ok, preserves `InvalidRequest`,
        // marks only Pass 2 `duration_ms` histogram as Timeout.
        let mut p2_ctx = base_ctx.clone();
        p2_ctx.spans = Arc::new(Pass2SlowSource {
            inner: Arc::clone(&base_ctx.spans),
            opens: Arc::new(AtomicUsize::new(0)),
            slow_on_open: 2,
            delay: Duration::from_millis(400),
        });
        let p2_res = facets(
            &QueryContext::from_engine(p2_ctx),
            FacetsRequest {
                view: FacetView::Spans,
                filter: None,
                search: None,
                scope: ListScope {
                    window: None,
                    include_eval: false,
                    include_experiments: false,
                },
                fields: &fields,
                timeout: Duration::from_millis(120),
            },
        )
        .await
        .expect("pass2 timeout returns per-field results");
        assert!(p2_res[0].as_ref().is_ok_and(|d| d.total == 20));
        assert!(matches!(p2_res[1], Err(RunError::InvalidRequest(_))));
        assert!(matches!(p2_res[2], Err(RunError::Timeout)));

        // Constant numeric field (`p99 <= min`) completes in Pass 1 with 1 bucket and skips Pass 2 (opens == 1).
        let const_opens = Arc::new(AtomicUsize::new(0));
        let mut const_ctx = base_ctx.clone();
        const_ctx.spans = Arc::new(CountingSource {
            inner: Arc::clone(&base_ctx.spans),
            opens: Arc::clone(&const_opens),
        });
        let const_fields = [FacetField {
            field: "total_tokens",
            query: None,
            limit: 10,
        }];
        let const_res = facets(
            &QueryContext::from_engine(const_ctx),
            FacetsRequest {
                view: FacetView::Spans,
                filter: None,
                search: None,
                scope: ListScope {
                    window: None,
                    include_eval: false,
                    include_experiments: false,
                },
                fields: &const_fields,
                timeout: Duration::from_secs(10),
            },
        )
        .await
        .expect("const facets");
        assert_eq!(const_opens.load(Ordering::SeqCst), 1);
        let hist = const_res[0]
            .as_ref()
            .expect("ok")
            .histogram
            .as_ref()
            .expect("hist");
        assert_eq!(hist.buckets.len(), 1);

        // Privacy-safe Debug on ParsedField/CheckedField must not leak map key or query text.
        let pf = ParsedField::MapValue("metadata", "secret_key_xyz".to_owned());
        let cf = CheckedField {
            field: pf.clone(),
            query: Some("secret_query_abc".to_owned()),
            limit: 10,
        };
        let dbg = format!("{pf:?} {cf:?}");
        assert!(!dbg.contains("secret_key_xyz"));
        assert!(!dbg.contains("secret_query_abc"));

        let _ = storage.shutdown().await;
    }
}
