//! Facets: the values, keys or histogram of one field over the rows a list shows (D1 of the
//! filtering plan). The rows are the list's rows before its cursor, sort and limit.

use std::time::Duration;

use futures::StreamExt;

use arrow::array::{Array, AsArray};
use arrow::record_batch::RecordBatch;
use datafusion::common::{NullHandling, UnnestOptions};
use datafusion::functions::string::expr_fn::starts_with;
use datafusion::functions_aggregate::expr_fn::{count, max, min};
use datafusion::functions_nested::expr_fn::{array_distinct, cardinality, map_keys};
use datafusion::logical_expr::expr_fn::when;
use datafusion::logical_expr::{Expr, Like, cast, col, lit};
use datafusion::prelude::DataFrame;
use std::ops::Not;

use super::columns::{f64_at, i64_at, internal, percentile, require};
use super::sessions::session_rows;
use super::{ListScope, present};
use crate::filter::{Filter, FilterView, exclusions, like_pattern, search, time_window};
use crate::query_error::{RunError, classify};
use crate::views::QueryContext;
use arrow::datatypes::DataType;

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
/// Fields of one batch computed at the same time (D-C-23).
const FIELDS_AT_ONCE: usize = 4;

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

/// A facet batch: fields over the rows of one list (D-C-23).
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

/// The columns a view facets, by kind.
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

/// A parsed field. Names are the view's own column names.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedField {
    Values(&'static str),
    Bool(&'static str),
    List(&'static str),
    Keys(&'static str),
    MapValue(&'static str, String),
    Number(&'static str),
}

const CANNOT_FACET: &str = "field cannot be faceted";

/// Column names of the facets: the aliases of the intermediate frames.
const VALUE: &str = "facet_value";
const COUNT: &str = "facet_count";
const TOTAL: &str = "facet_total";
const MISSING: &str = "facet_missing";
const PRESENT: &str = "facet_present";
const X: &str = "facet_x";
const BUCKET: &str = "facet_bucket";
const MIN: &str = "facet_min";
const MAX: &str = "facet_max";
const P50: &str = "facet_p50";
const P95: &str = "facet_p95";
const P99: &str = "facet_p99";
/// The prefix of internal keys, which facets leave out.
const INTERNAL_KEY_PREFIX: &str = "cetra.";

/// Parses `text` as a field of `view`: a column name, or `map['key']` of a keys column.
///
/// # Errors
///
/// [`RunError::InvalidRequest`] when the field is not a facet column of the view.
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

/// The key of `map['key']` from the text between `['` and `']`: each `'` is written `''`.
/// `None` for a bad quote or a key of 0 or more than [`MAX_FACET_KEY`] characters.
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

/// A field that passed its checks: the parsed field, its trimmed `q` and its limit.
type CheckedField<'a> = (ParsedField, Option<&'a str>, usize);

/// The limit and `q` of one field, and the parsed field. The limit is checked first, then `q`,
/// then the field name.
///
/// # Errors
///
/// [`RunError::InvalidRequest`] for a bad limit, `q` or field.
fn check_field<'a>(view: FacetView, field: &FacetField<'a>) -> Result<CheckedField<'a>, RunError> {
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
            Some(trimmed)
        }
        None => None,
    };
    let parsed = parse_field(view, field.field)?;
    Ok((parsed, query, field.limit))
}

/// The facets of `request`, one result per field in request order: each field's values, keys or
/// histogram, over the rows of the list. The rows are built once and the fields run
/// concurrently under one deadline (D-C-23).
///
/// # Errors
///
/// The outer error fails the whole batch: [`RunError::InvalidRequest`] for 0 or more than
/// [`MAX_FACET_FIELDS`] fields, or `search` on sessions; [`RunError::Invalid`] for a bad filter
/// (all checked before any query runs); [`RunError::Timeout`] when building the rows passes
/// `timeout`; DataFusion failures through [`classify`].
///
/// A field's own error is in its slot: [`RunError::InvalidRequest`] for a bad limit, `q` or
/// field; [`RunError::Timeout`] when the field passes the deadline; query failures.
pub async fn facets(
    ctx: &QueryContext,
    request: FacetsRequest<'_>,
) -> Result<Vec<Result<FacetData, RunError>>, RunError> {
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
    let filter = match present(request.filter) {
        Some(text) => Some(ctx.parse_filter(filter_view(request.view), text)?),
        None => None,
    };
    let checked: Vec<Result<CheckedField<'_>, RunError>> = request
        .fields
        .iter()
        .map(|field| check_field(request.view, field))
        .collect();
    if checked.iter().all(Result::is_err) {
        // No field is valid, so no query runs.
        return Ok(checked
            .into_iter()
            .map(|entry| match entry {
                Err(error) => Err(error),
                Ok(_) => Err(internal(
                    "a valid field reached the error-only path".to_owned(),
                )),
            })
            .collect());
    }
    let deadline = tokio::time::Instant::now() + request.timeout;
    let frame = match tokio::time::timeout_at(deadline, rows(ctx, &request, filter)).await {
        Err(_) => return Err(RunError::Timeout),
        Ok(frame) => frame?,
    };
    // Collected first: a lazy `map` inside the stream makes the handler's future not `Send`.
    let futures: Vec<_> = checked
        .into_iter()
        .map(|entry| {
            let frame = frame.clone();
            async move {
                let (field, query, limit) = entry?;
                match tokio::time::timeout_at(deadline, one(ctx, frame, &field, query, limit)).await
                {
                    Err(_) => Err(RunError::Timeout),
                    Ok(result) => result,
                }
            }
        })
        .collect();
    // At most `FIELDS_AT_ONCE` fields run at a time, so one batch does not take the scans of
    // twenty readers; `buffered` keeps the request order.
    Ok(futures::stream::iter(futures)
        .buffered(FIELDS_AT_ONCE)
        .collect()
        .await)
}

fn filter_view(view: FacetView) -> FilterView {
    match view {
        FacetView::Traces => FilterView::Traces,
        FacetView::Spans => FilterView::Spans,
        FacetView::Sessions => FilterView::Sessions,
    }
}

/// The one set of rows every field of the batch is counted over: the list's rows before its
/// cursor, sort and limit, with the filter applied.
async fn rows(
    ctx: &QueryContext,
    request: &FacetsRequest<'_>,
    filter: Option<Filter>,
) -> Result<DataFrame, RunError> {
    match request.view {
        FacetView::Traces => {
            window_rows(
                ctx,
                "traces",
                FilterView::Traces,
                request.scope,
                request.search,
                filter,
            )
            .await
        }
        FacetView::Spans => {
            window_rows(
                ctx,
                "spans",
                FilterView::Spans,
                request.scope,
                request.search,
                filter,
            )
            .await
        }
        FacetView::Sessions => session_rows(ctx, request.scope, filter).await,
    }
}

/// The facet of one parsed field over `frame`: a histogram for a number, else its values.
async fn one(
    ctx: &QueryContext,
    frame: DataFrame,
    field: &ParsedField,
    query: Option<&str>,
    limit: usize,
) -> Result<FacetData, RunError> {
    match field {
        ParsedField::Number(name) => histogram(ctx, frame, name).await,
        _ => values(ctx, frame, field, query, limit).await,
    }
}

/// The rows of the traces or spans list before its cursor, sort and limit: the window, the
/// default exclusions, the search and the filter, in the order `list_frame` applies them.
async fn window_rows(
    ctx: &QueryContext,
    table: &str,
    view: FilterView,
    scope: ListScope,
    text_search: Option<&str>,
    filter: Option<Filter>,
) -> Result<DataFrame, RunError> {
    let references_environment = filter
        .as_ref()
        .is_some_and(|filter| filter.references_environment);
    let mut frame = ctx.session().table(table).await.map_err(classify)?;
    let mut conjuncts = Vec::new();
    if let Some(window) = scope.window {
        conjuncts.push(time_window(window));
    }
    if let Some(expr) = exclusions(
        ctx,
        view,
        scope.include_eval,
        scope.include_experiments,
        references_environment,
    )? {
        conjuncts.push(expr);
    }
    if let Some(text) = present(text_search)
        && let Some(expr) = search(view, text, scope.window, ctx).await?
    {
        conjuncts.push(expr);
    }
    if let Some(predicate) = conjuncts.into_iter().reduce(Expr::and) {
        frame = frame.filter(predicate).map_err(classify)?;
    }
    if let Some(filter) = filter {
        frame = filter.apply(frame, ctx).await.map_err(classify)?;
    }
    Ok(frame)
}

/// The expression of the field's value, one per element for a list and one per key for keys.
fn value_expr(frame: &DataFrame, field: &ParsedField) -> Result<Expr, RunError> {
    match field {
        ParsedField::Values(name) | ParsedField::Bool(name) => Ok(col(*name)),
        ParsedField::List(name) => Ok(array_distinct(col(*name))),
        ParsedField::Keys(name) => Ok(map_keys(col(*name))),
        ParsedField::MapValue(name, key) => {
            let key = key.replace('\'', "''");
            frame
                .parse_sql_expr(&format!("{name}['{key}']"))
                .map_err(classify)
        }
        ParsedField::Number(_) => Err(internal("a number field has no values".to_owned())),
    }
}

/// Values of a non-number field: totals, then the groups by value.
async fn values(
    ctx: &QueryContext,
    frame: DataFrame,
    field: &ParsedField,
    query: Option<&str>,
    limit: usize,
) -> Result<FacetData, RunError> {
    let list = matches!(field, ParsedField::List(_) | ParsedField::Keys(_));
    let value = value_expr(&frame, field)?;
    let base = frame.select(vec![value.alias(VALUE)]).map_err(classify)?;
    let missing = if list {
        col(VALUE)
            .is_null()
            .or(cardinality(col(VALUE)).eq(lit(0_u64)))
    } else {
        col(VALUE).is_null()
    };
    let missing_count = when(missing, lit(1_i64)).end().map_err(classify)?;
    let totals = base
        .clone()
        .aggregate(
            vec![],
            vec![
                count(lit(1_i64)).alias(TOTAL),
                count(missing_count).alias(MISSING),
            ],
        )
        .map_err(classify)?
        .collect()
        .await
        .map_err(|error| ctx.fail(error))?;
    let (total, missing) = match first_row(&totals) {
        Some(batch) => (count_at(batch, TOTAL)?, count_at(batch, MISSING)?),
        None => (0, 0),
    };

    let mut groups = if list {
        base.unnest_columns_with_options(
            &[VALUE],
            UnnestOptions::new().with_null_handling(NullHandling::Drop),
        )
        .map_err(classify)?
    } else {
        base
    };
    groups = groups.filter(col(VALUE).is_not_null()).map_err(classify)?;
    if matches!(field, ParsedField::Keys(_)) {
        groups = groups
            .filter(starts_with(col(VALUE), lit(INTERNAL_KEY_PREFIX)).not())
            .map_err(classify)?;
    }
    groups = groups
        .select(vec![cast(col(VALUE), DataType::Utf8).alias(VALUE)])
        .map_err(classify)?;
    if let Some(query) = query {
        let pattern = like_pattern(query);
        groups = groups
            .filter(Expr::Like(Like::new(
                false,
                Box::new(col(VALUE)),
                Box::new(lit(pattern)),
                Some('\\'),
                true,
            )))
            .map_err(classify)?;
    }
    let rows = groups
        .aggregate(vec![col(VALUE)], vec![count(lit(1_i64)).alias(COUNT)])
        .map_err(classify)?
        .sort(vec![
            col(COUNT).sort(false, false),
            col(VALUE).sort(true, false),
        ])
        .map_err(classify)?
        .limit(0, Some(limit + 1))
        .map_err(classify)?
        .collect()
        .await
        .map_err(|error| ctx.fail(error))?;

    let mut values = Vec::new();
    for batch in &rows {
        let strings = require(batch, VALUE)?
            .as_string_opt::<i32>()
            .ok_or_else(|| internal("a facet value is not text".to_owned()))?;
        let counts = require(batch, COUNT)?;
        for index in 0..batch.num_rows() {
            values.push(FacetValue {
                value: strings.value(index).to_owned(),
                count: count_value(counts, index)?,
            });
        }
    }
    let more = values.len() > limit;
    values.truncate(limit);
    Ok(FacetData {
        kind: if matches!(field, ParsedField::Keys(_)) {
            FacetKind::Keys
        } else {
            FacetKind::Values
        },
        total,
        missing,
        values,
        more,
        histogram: None,
    })
}

/// The number of `name` as `Float64`, or null when it is NaN or infinite. Such values are
/// missing: they count in `missing` and in no bucket (D1). A NaN fails both comparisons.
fn finite_number(name: &str) -> Result<Expr, RunError> {
    let x = cast(col(name), DataType::Float64);
    when(
        x.clone()
            .gt_eq(lit(-f64::MAX))
            .and(x.clone().lt_eq(lit(f64::MAX))),
        x,
    )
    .end()
    .map_err(classify)
}

/// A histogram of a number field: min, max and percentiles, then the bucket counts (D1).
async fn histogram(
    ctx: &QueryContext,
    frame: DataFrame,
    name: &str,
) -> Result<FacetData, RunError> {
    let stats = frame
        .clone()
        .select(vec![finite_number(name)?.alias(X)])
        .map_err(classify)?
        .aggregate(
            vec![],
            vec![
                count(lit(1_i64)).alias(TOTAL),
                count(col(X)).alias(PRESENT),
                min(col(X)).alias(MIN),
                max(col(X)).alias(MAX),
                percentile(X, 0.5).alias(P50),
                percentile(X, 0.95).alias(P95),
                percentile(X, 0.99).alias(P99),
            ],
        )
        .map_err(classify)?
        .collect()
        .await
        .map_err(|error| ctx.fail(error))?;
    let Some(row) = first_row(&stats) else {
        return Ok(empty_histogram(0));
    };
    let total = count_at(row, TOTAL)?;
    let present = count_at(row, PRESENT)?;
    if present == 0 {
        return Ok(empty_histogram(total));
    }
    let stat = |name: &str| -> Result<f64, RunError> {
        f64_at(require(row, name)?, 0)?.ok_or_else(|| internal(format!("{name} is null")))
    };
    let (min, max) = (stat(MIN)?, stat(MAX)?);
    let p99 = stat(P99)?;
    let histogram = Histogram {
        min,
        max,
        p50: stat(P50)?,
        p95: stat(P95)?,
        p99,
        buckets: buckets(ctx, frame, name, present, (min, max, p99)).await?,
    };
    Ok(FacetData {
        kind: FacetKind::Histogram,
        total,
        missing: total - present,
        values: Vec::new(),
        more: false,
        histogram: Some(histogram),
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

/// The buckets of D1: `HISTOGRAM_BUCKETS` equal ones over `[min, p99)`, then `[p99, max]` when
/// `max >= p99`. With `p99 <= min` there is one bucket, `[min, max]`.
async fn buckets(
    ctx: &QueryContext,
    frame: DataFrame,
    name: &str,
    present: u64,
    (min, max, p99): (f64, f64, f64),
) -> Result<Vec<HistogramBucket>, RunError> {
    if p99 <= min {
        return Ok(vec![HistogramBucket {
            lo: min,
            hi: max,
            count: present,
        }]);
    }
    let width = (p99 - min) / HISTOGRAM_BUCKETS as f64;
    let x = col(X);
    let raw = cast((x.clone() - lit(min)) / lit(width), DataType::Int64);
    let clamped = when(
        raw.clone().gt_eq(lit(HISTOGRAM_BUCKETS as i64 - 1)),
        lit(HISTOGRAM_BUCKETS as i64 - 1),
    )
    .otherwise(raw)
    .map_err(classify)?;
    let index = when(x.clone().gt_eq(lit(p99)), lit(HISTOGRAM_BUCKETS as i64))
        .otherwise(clamped)
        .map_err(classify)?;
    let counted = frame
        .select(vec![finite_number(name)?.alias(X)])
        .map_err(classify)?
        .filter(col(X).is_not_null())
        .map_err(classify)?
        .select(vec![index.alias(BUCKET)])
        .map_err(classify)?
        .aggregate(vec![col(BUCKET)], vec![count(lit(1_i64)).alias(COUNT)])
        .map_err(classify)?
        .collect()
        .await
        .map_err(|error| ctx.fail(error))?;

    let mut tally = [0_u64; HISTOGRAM_BUCKETS + 1];
    for batch in &counted {
        let indexes = require(batch, BUCKET)?;
        let counts = require(batch, COUNT)?;
        for row in 0..batch.num_rows() {
            let index = usize::try_from(i64_at(indexes, row)?)
                .map_err(|_| internal("a bucket index is negative".to_owned()))?;
            let slot = tally
                .get_mut(index)
                .ok_or_else(|| internal("a bucket index is out of range".to_owned()))?;
            *slot += count_value(counts, row)?;
        }
    }
    let length = if max >= p99 {
        HISTOGRAM_BUCKETS + 1
    } else {
        HISTOGRAM_BUCKETS
    };
    Ok(tally
        .iter()
        .take(length)
        .enumerate()
        .map(|(index, count)| {
            if index < HISTOGRAM_BUCKETS {
                let lo = min + index as f64 * width;
                let hi = if index + 1 == HISTOGRAM_BUCKETS {
                    p99
                } else {
                    min + (index + 1) as f64 * width
                };
                HistogramBucket {
                    lo,
                    hi,
                    count: *count,
                }
            } else {
                HistogramBucket {
                    lo: p99,
                    hi: max,
                    count: *count,
                }
            }
        })
        .collect())
}

/// The first row of the batches, if any has one.
fn first_row(batches: &[RecordBatch]) -> Option<&RecordBatch> {
    batches.iter().find(|batch| batch.num_rows() > 0)
}

/// The count in `name` of row 0 of an aggregate.
fn count_at(batch: &RecordBatch, name: &str) -> Result<u64, RunError> {
    count_value(require(batch, name)?, 0)
}

/// A count as `u64`. Counts are never negative.
fn count_value(column: &dyn Array, index: usize) -> Result<u64, RunError> {
    u64::try_from(i64_at(column, index)?)
        .map_err(|_| internal("aggregate count is negative".to_owned()))
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::Arc;

    use cetra_core::span::Span;
    use cetra_core::time_serde::parse_rfc3339;
    use datafusion::execution::runtime_env::RuntimeEnv;

    use super::*;
    use crate::reads::Histogram;
    use crate::test_support::{
        example_query, live_scores_table, span_row, spans_table, traces_table,
    };
    use crate::views::{ProjectQuery, TimeWindow};

    const TIMEOUT: Duration = Duration::from_secs(30);

    fn context() -> QueryContext {
        QueryContext::new(example_query(), None).expect("context")
    }

    /// A context over `spans` (the example dataset's shape, with the given rows).
    fn context_with(spans: &[Span]) -> QueryContext {
        QueryContext::new(
            ProjectQuery {
                spans: spans_table(spans),
                traces: traces_table(spans),
                scores: live_scores_table(&[]),
                runtime: Arc::new(RuntimeEnv::default()),
                target_partitions: NonZeroUsize::new(2).expect("two partitions"),
                optimizer_rules: Vec::new(),
                suspects: crate::verify::Suspects::detached(),
                visible_from: None,
            },
            None,
        )
        .expect("context")
    }

    fn nanos(text: &str) -> i64 {
        parse_rfc3339(text).expect(text)
    }

    /// The example dataset's window: 10:00 to 11:00 on 2026-01-01.
    fn hour() -> Option<TimeWindow> {
        Some(
            TimeWindow::new(nanos("2026-01-01T10:00:00Z"), nanos("2026-01-01T11:00:00Z"))
                .expect("window"),
        )
    }

    /// One field asked alone, the shape the single-field tests were written for.
    #[derive(Debug, Clone, Copy)]
    struct Ask<'a> {
        view: FacetView,
        field: &'a str,
        filter: Option<&'a str>,
        search: Option<&'a str>,
        scope: ListScope,
        query: Option<&'a str>,
        limit: usize,
    }

    fn request(view: FacetView, field: &str) -> Ask<'_> {
        Ask {
            view,
            field,
            filter: None,
            search: None,
            scope: ListScope {
                window: None,
                include_eval: false,
                include_experiments: false,
            },
            query: None,
            limit: 10,
        }
    }

    /// `ask` as a batch of one; the batch's error or the field's.
    async fn facet(ctx: &QueryContext, ask: Ask<'_>) -> Result<FacetData, RunError> {
        let fields = [FacetField {
            field: ask.field,
            query: ask.query,
            limit: ask.limit,
        }];
        let mut results = facets(
            ctx,
            FacetsRequest {
                view: ask.view,
                filter: ask.filter,
                search: ask.search,
                scope: ask.scope,
                fields: &fields,
                timeout: TIMEOUT,
            },
        )
        .await?;
        results.pop().expect("one result per field")
    }

    /// A batch over `fields` with no window, filter or search.
    fn batch<'a>(view: FacetView, fields: &'a [FacetField<'a>]) -> FacetsRequest<'a> {
        FacetsRequest {
            view,
            filter: None,
            search: None,
            scope: ListScope {
                window: None,
                include_eval: false,
                include_experiments: false,
            },
            fields,
            timeout: TIMEOUT,
        }
    }

    fn field(name: &str) -> FacetField<'_> {
        FacetField {
            field: name,
            query: None,
            limit: 10,
        }
    }

    fn pairs(data: &FacetData) -> Vec<(&str, u64)> {
        data.values
            .iter()
            .map(|value| (value.value.as_str(), value.count))
            .collect()
    }

    /// The traces of the example window, without the eval trace: 5 traces.
    #[tokio::test]
    async fn status_values_count_traces_of_the_window() {
        let ctx = context();
        let mut traces = request(FacetView::Traces, "status");
        traces.scope.window = hour();
        let data = facet(&ctx, traces).await.expect("facet");
        assert_eq!(data.kind, FacetKind::Values);
        assert_eq!(data.total, 5);
        assert_eq!(data.missing, 0);
        // Trace 6 has no root span, so its status is "unset".
        assert_eq!(pairs(&data), [("ok", 3), ("error", 1), ("unset", 1)]);
        assert!(!data.more);
        assert!(data.histogram.is_none());
    }

    #[tokio::test]
    async fn filter_and_search_narrow_counts() {
        let ctx = context();
        let mut filtered = request(FacetView::Traces, "status");
        filtered.filter = Some("user_id = 'user-42'");
        let data = facet(&ctx, filtered).await.expect("facet");
        assert_eq!(data.total, 3);
        assert_eq!(pairs(&data), [("ok", 3)]);

        let mut searched = request(FacetView::Traces, "status");
        searched.scope.window = hour();
        searched.search = Some("order");
        let data = facet(&ctx, searched).await.expect("facet");
        assert_eq!(data.total, 1);
        assert_eq!(pairs(&data), [("ok", 1)]);
    }

    #[tokio::test]
    async fn query_keeps_matching_values_case_insensitive() {
        let ctx = context();
        let mut upper = request(FacetView::Traces, "name");
        upper.query = Some("SUPPORT");
        let data = facet(&ctx, upper).await.expect("facet");
        assert_eq!(pairs(&data), [("support-agent", 2)]);
        assert_eq!(data.total, 5);
    }

    #[tokio::test]
    async fn query_escapes_like_wildcards() {
        let ctx = context();
        let mut percent = request(FacetView::Traces, "name");
        percent.query = Some("%");
        let data = facet(&ctx, percent).await.expect("facet");
        assert!(data.values.is_empty(), "{:?}", pairs(&data));
        assert_eq!(data.total, 5);

        let mut underscore = request(FacetView::Spans, "name");
        underscore.query = Some("_");
        let data = facet(&ctx, underscore).await.expect("facet");
        assert_eq!(pairs(&data), [("get_order_status", 1)]);
    }

    #[tokio::test]
    async fn limit_sets_more() {
        let ctx = context();
        let all = facet(&ctx, request(FacetView::Traces, "name"))
            .await
            .expect("facet");
        assert_eq!(all.values.len(), 4, "{:?}", pairs(&all));
        assert!(!all.more);
        let mut one = request(FacetView::Traces, "name");
        one.limit = 1;
        let data = facet(&ctx, one).await.expect("facet");
        assert_eq!(pairs(&data), [("support-agent", 2)]);
        assert!(data.more);
    }

    #[tokio::test]
    async fn tags_count_elements() {
        let ctx = context();
        let data = facet(&ctx, request(FacetView::Traces, "tags"))
            .await
            .expect("facet");
        assert_eq!(data.kind, FacetKind::Values);
        assert_eq!(pairs(&data), [("beta", 2), ("refund", 1)]);
        assert_eq!(data.total, 5);
        assert_eq!(data.missing, 3);
    }

    #[tokio::test]
    async fn metadata_keys_and_values() {
        let ctx = context();
        let keys = facet(&ctx, request(FacetView::Traces, "metadata"))
            .await
            .expect("facet");
        assert_eq!(keys.kind, FacetKind::Keys);
        assert_eq!(pairs(&keys), [("user.name", 1), ("user.plan", 1)]);
        assert_eq!(keys.missing, 4);

        let plan = facet(&ctx, request(FacetView::Traces, "metadata['user.plan']"))
            .await
            .expect("facet");
        assert_eq!(plan.kind, FacetKind::Values);
        assert_eq!(pairs(&plan), [("\"pro\"", 1)]);
        assert_eq!(plan.missing, 4);
    }

    #[tokio::test]
    async fn bool_values_are_true_false() {
        let ctx = context();
        let data = facet(&ctx, request(FacetView::Traces, "has_root"))
            .await
            .expect("facet");
        assert_eq!(pairs(&data), [("true", 4), ("false", 1)]);
    }

    #[tokio::test]
    async fn keys_leave_out_cetra_prefix() {
        let mut span = span_row(1, 0, 1_000, 1_000);
        span.metadata
            .insert("cetra.internal".to_owned(), "1".to_owned());
        span.metadata
            .insert("user.plan".to_owned(), "\"pro\"".to_owned());
        let ctx = context_with(&[span]);
        let data = facet(&ctx, request(FacetView::Spans, "metadata"))
            .await
            .expect("facet");
        assert_eq!(pairs(&data), [("user.plan", 1)]);
        assert_eq!(data.total, 1);
    }

    #[tokio::test]
    async fn histogram_buckets_cover_all_rows() {
        let ctx = context();
        let data = facet(&ctx, request(FacetView::Traces, "duration_ms"))
            .await
            .expect("facet");
        assert_eq!(data.kind, FacetKind::Histogram);
        assert_eq!(data.total, 5);
        assert_eq!(data.missing, 0);
        let histogram = data.histogram.expect("histogram");
        let sum: u64 = histogram.buckets.iter().map(|bucket| bucket.count).sum();
        assert_eq!(sum, 5);
        assert!(matches!(histogram.buckets.len(), 1 | 20 | 21));
        assert!(
            histogram
                .buckets
                .windows(2)
                .all(|pair| pair[0].lo <= pair[1].lo)
        );
        assert!(histogram.min <= histogram.p50 && histogram.p99 <= histogram.max + 1e-9);
        assert!(data.values.is_empty());
        assert!(!data.more);
    }

    #[tokio::test]
    async fn histogram_of_no_rows_is_none() {
        let ctx = context();
        let mut empty = request(FacetView::Traces, "duration_ms");
        empty.filter = Some("name = 'no such trace'");
        let data = facet(&ctx, empty).await.expect("facet");
        assert_eq!(data.kind, FacetKind::Histogram);
        assert_eq!(data.total, 0);
        assert!(data.histogram.is_none());
    }

    #[tokio::test]
    async fn sessions_facets_use_session_rows() {
        let ctx = context();
        let data = facet(&ctx, request(FacetView::Sessions, "user_id"))
            .await
            .expect("facet");
        assert_eq!(data.total, 2);
        assert_eq!(data.missing, 0);
        assert_eq!(pairs(&data), [("user-42", 1), ("user-7", 1)]);
    }

    #[tokio::test]
    async fn bad_field_is_invalid_request() {
        let ctx = context();
        for field in [
            "input",
            "trace_id",
            "start_time",
            "nope",
            "metadata[",
            "metadata['']",
        ] {
            let result = facet(&ctx, request(FacetView::Traces, field)).await;
            assert!(
                matches!(&result, Err(RunError::InvalidRequest(message)) if message == CANNOT_FACET),
                "{field}: {result:?}"
            );
        }
    }

    #[test]
    fn parse_field_accepts_columns_and_map_keys() {
        assert_eq!(
            parse_field(FacetView::Traces, "status").expect("status"),
            ParsedField::Values("status")
        );
        assert_eq!(
            parse_field(FacetView::Traces, "has_root").expect("has_root"),
            ParsedField::Bool("has_root")
        );
        assert_eq!(
            parse_field(FacetView::Traces, "models").expect("models"),
            ParsedField::List("models")
        );
        assert_eq!(
            parse_field(FacetView::Traces, "metadata").expect("metadata"),
            ParsedField::Keys("metadata")
        );
        assert_eq!(
            parse_field(FacetView::Traces, "metadata['user.plan']").expect("map"),
            ParsedField::MapValue("metadata", "user.plan".to_owned())
        );
        assert_eq!(
            parse_field(FacetView::Traces, "metadata['it''s']").expect("quote"),
            ParsedField::MapValue("metadata", "it's".to_owned())
        );
        assert_eq!(
            parse_field(FacetView::Traces, "duration_ms").expect("duration"),
            ParsedField::Number("duration_ms")
        );
        assert!(matches!(
            parse_field(FacetView::Spans, "attributes['x']"),
            Ok(ParsedField::MapValue("attributes", _))
        ));
    }

    #[test]
    fn malformed_map_fields_are_rejected() {
        for text in [
            "metadata['a'b']",
            "metadata['a'",
            "metadata[']",
            "tags['x']",
            "attributes['x']",
        ] {
            assert!(
                parse_field(FacetView::Traces, text).is_err(),
                "{text} should be rejected"
            );
        }
    }

    #[test]
    fn key_longer_than_the_limit_is_rejected() {
        let long = "k".repeat(MAX_FACET_KEY + 1);
        assert!(parse_field(FacetView::Traces, &format!("metadata['{long}']")).is_err());
        let exact = "k".repeat(MAX_FACET_KEY);
        assert!(parse_field(FacetView::Traces, &format!("metadata['{exact}']")).is_ok());
    }

    #[tokio::test]
    async fn bad_limit_and_query_are_invalid_requests() {
        let ctx = context();
        let mut zero = request(FacetView::Traces, "status");
        zero.limit = 0;
        assert!(matches!(
            facet(&ctx, zero).await,
            Err(RunError::InvalidRequest(_))
        ));
        let mut too_many = request(FacetView::Traces, "status");
        too_many.limit = MAX_FACET_LIMIT + 1;
        assert!(matches!(
            facet(&ctx, too_many).await,
            Err(RunError::InvalidRequest(_))
        ));
        for query in ["   ", ""] {
            let mut blank = request(FacetView::Traces, "status");
            blank.query = Some(query);
            assert!(matches!(
                facet(&ctx, blank).await,
                Err(RunError::InvalidRequest(_))
            ));
        }
        let long = "q".repeat(MAX_FACET_QUERY + 1);
        let mut too_long = request(FacetView::Traces, "status");
        too_long.query = Some(&long);
        assert!(matches!(
            facet(&ctx, too_long).await,
            Err(RunError::InvalidRequest(_))
        ));
    }

    #[tokio::test]
    async fn bad_filter_is_invalid() {
        let ctx = context();
        let mut bad = request(FacetView::Traces, "status");
        bad.filter = Some("error_count >");
        assert!(matches!(facet(&ctx, bad).await, Err(RunError::Invalid(_))));
    }

    #[tokio::test]
    async fn sessions_reject_search() {
        let ctx = context();
        let mut bad = request(FacetView::Sessions, "user_id");
        bad.search = Some("refund");
        assert!(matches!(
            facet(&ctx, bad).await,
            Err(RunError::InvalidRequest(_))
        ));
    }

    /// One span per value, each in its own trace, with `cost_total` set to the value.
    fn cost_spans(values: &[Option<f64>]) -> Vec<Span> {
        values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let mut span = span_row(index as u16, 0, 1_000, 1_000);
                span.cost_total = *value;
                span
            })
            .collect()
    }

    /// The `cost_total` histogram of the spans that hold `values`.
    async fn cost_histogram(values: &[Option<f64>]) -> FacetData {
        let ctx = context_with(&cost_spans(values));
        facet(&ctx, request(FacetView::Spans, "cost_total"))
            .await
            .expect("facet")
    }

    fn bucket_sum(histogram: &Histogram) -> u64 {
        histogram.buckets.iter().map(|bucket| bucket.count).sum()
    }

    #[tokio::test]
    async fn histogram_of_one_value_is_one_bucket() {
        let data = cost_histogram(&[Some(5.0), Some(5.0), Some(5.0)]).await;
        let histogram = data.histogram.expect("histogram");
        assert_eq!((histogram.min, histogram.max), (5.0, 5.0));
        assert_eq!(
            histogram.buckets,
            [HistogramBucket {
                lo: 5.0,
                hi: 5.0,
                count: 3
            }]
        );
    }

    #[tokio::test]
    async fn histogram_with_p99_at_min_is_one_bucket() {
        // 200 rows at 1.0 and one at 10.0: the 99th percentile is the minimum.
        let mut values = vec![Some(1.0); 200];
        values.push(Some(10.0));
        let data = cost_histogram(&values).await;
        let histogram = data.histogram.expect("histogram");
        assert_eq!(histogram.p99, histogram.min);
        assert!(histogram.max > histogram.min);
        assert_eq!(
            histogram.buckets,
            [HistogramBucket {
                lo: 1.0,
                hi: 10.0,
                count: 201
            }]
        );
    }

    #[tokio::test]
    async fn outlier_lands_in_the_overflow_bucket() {
        let mut values: Vec<Option<f64>> = (0..100).map(|n| Some(f64::from(n))).collect();
        values.push(Some(1_000.0));
        let data = cost_histogram(&values).await;
        let histogram = data.histogram.expect("histogram");
        assert_eq!(histogram.max, 1_000.0);
        assert!(histogram.p99 < histogram.max);
        assert_eq!(histogram.buckets.len(), 21);
        assert_eq!(histogram.buckets[19].hi, histogram.p99);
        let overflow = histogram.buckets[20];
        assert_eq!((overflow.lo, overflow.hi), (histogram.p99, histogram.max));
        assert!(overflow.count >= 1);
        assert!(
            histogram
                .buckets
                .windows(2)
                .all(|pair| pair[0].lo <= pair[1].lo)
        );
        assert_eq!(bucket_sum(&histogram), data.total - data.missing);
        assert_eq!(bucket_sum(&histogram), 101);
    }

    #[tokio::test]
    async fn negative_values_are_bucketed() {
        let values = [-5.0, -1.0, 0.0, 2.5, 5.0].map(Some);
        let data = cost_histogram(&values).await;
        let histogram = data.histogram.expect("histogram");
        assert_eq!((histogram.min, histogram.max), (-5.0, 5.0));
        assert_eq!(histogram.buckets[0].lo, -5.0);
        assert_eq!(bucket_sum(&histogram), 5);
    }

    #[tokio::test]
    async fn null_and_non_finite_numbers_are_missing() {
        let values = [
            Some(1.0),
            Some(2.0),
            Some(f64::NAN),
            Some(f64::INFINITY),
            Some(f64::NEG_INFINITY),
            None,
        ];
        let data = cost_histogram(&values).await;
        assert_eq!(data.total, 6);
        assert_eq!(data.missing, 4);
        let histogram = data.histogram.expect("histogram");
        assert_eq!((histogram.min, histogram.max), (1.0, 2.0));
        assert_eq!(bucket_sum(&histogram), 2);
    }

    #[tokio::test]
    async fn only_non_finite_numbers_make_no_histogram() {
        let data = cost_histogram(&[Some(f64::NAN), None]).await;
        assert_eq!((data.total, data.missing), (2, 2));
        assert!(data.histogram.is_none());
    }

    #[tokio::test]
    async fn sessions_check_search_before_the_filter() {
        let ctx = context();
        let mut bad = request(FacetView::Sessions, "user_id");
        bad.search = Some("refund");
        bad.filter = Some("error_count >");
        assert!(matches!(
            facet(&ctx, bad).await,
            Err(RunError::InvalidRequest(_))
        ));
    }

    #[tokio::test]
    async fn spans_values_count_spans_of_the_window() {
        let ctx = context();
        let data = facet(&ctx, request(FacetView::Spans, "span_kind"))
            .await
            .expect("facet");
        assert_eq!(data.total, 12);
        assert_eq!(
            pairs(&data),
            [
                ("llm", 6),
                ("agent", 2),
                ("chain", 2),
                ("retriever", 1),
                ("tool", 1)
            ]
        );
    }

    #[tokio::test]
    async fn include_eval_adds_the_eval_rows() {
        let ctx = context();
        let mut spans = request(FacetView::Spans, "span_kind");
        spans.scope.include_eval = true;
        let data = facet(&ctx, spans).await.expect("facet");
        assert_eq!(data.total, 14);
        assert_eq!(
            pairs(&data),
            [
                ("llm", 7),
                ("agent", 2),
                ("chain", 2),
                ("evaluator", 1),
                ("retriever", 1),
                ("tool", 1)
            ]
        );

        let mut traces = request(FacetView::Traces, "status");
        traces.scope.include_eval = true;
        let data = facet(&ctx, traces).await.expect("facet");
        assert_eq!(data.total, 6);
        assert_eq!(pairs(&data), [("ok", 4), ("error", 1), ("unset", 1)]);
    }

    #[tokio::test]
    async fn metadata_key_with_a_quote_is_read_back() {
        let mut span = span_row(1, 0, 1_000, 1_000);
        span.metadata.insert("it's".to_owned(), "v1".to_owned());
        let ctx = context_with(&[span]);
        let keys = facet(&ctx, request(FacetView::Spans, "metadata"))
            .await
            .expect("facet");
        assert_eq!(pairs(&keys), [("it's", 1)]);
        let value = facet(&ctx, request(FacetView::Spans, "metadata['it''s']"))
            .await
            .expect("facet");
        assert_eq!(pairs(&value), [("v1", 1)]);
        assert_eq!(value.missing, 0);
    }

    #[tokio::test]
    async fn batch_answers_each_field_in_order() {
        let ctx = context();
        let fields = [field("status"), field("duration_ms"), field("metadata")];
        let mut request = batch(FacetView::Traces, &fields);
        request.scope.window = hour();
        let results = facets(&ctx, request).await.expect("batch");
        assert_eq!(results.len(), 3);
        let status = results[0].as_ref().expect("status");
        assert_eq!(pairs(status), [("ok", 3), ("error", 1), ("unset", 1)]);
        assert_eq!(
            results[1].as_ref().expect("duration").kind,
            FacetKind::Histogram
        );
        assert_eq!(results[2].as_ref().expect("metadata").kind, FacetKind::Keys);
    }

    /// More fields than run at once still come back in request order.
    #[tokio::test]
    async fn more_fields_than_run_at_once_keep_their_order() {
        let ctx = context();
        let names = [
            "status",
            "duration_ms",
            "metadata",
            "name",
            "cost_total",
            "tags",
            "input",
        ];
        assert!(names.len() > FIELDS_AT_ONCE);
        let fields: Vec<FacetField<'_>> = names.iter().map(|name| field(name)).collect();
        let results = facets(&ctx, batch(FacetView::Traces, &fields))
            .await
            .expect("batch");
        let kinds: Vec<Option<FacetKind>> = results
            .iter()
            .map(|result| result.as_ref().ok().map(|data| data.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                Some(FacetKind::Values),
                Some(FacetKind::Histogram),
                Some(FacetKind::Keys),
                Some(FacetKind::Values),
                Some(FacetKind::Histogram),
                Some(FacetKind::Values),
                None,
            ]
        );
    }

    #[tokio::test]
    async fn one_bad_field_does_not_fail_the_others() {
        let ctx = context();
        let zero_limit = FacetField {
            field: "status",
            query: None,
            limit: 0,
        };
        let fields = [field("status"), field("input"), zero_limit];
        let results = facets(&ctx, batch(FacetView::Traces, &fields))
            .await
            .expect("batch");
        assert!(results[0].is_ok());
        assert!(matches!(&results[1], Err(RunError::InvalidRequest(_))));
        assert!(matches!(&results[2], Err(RunError::InvalidRequest(_))));
    }

    #[tokio::test]
    async fn duplicate_fields_are_answered_twice() {
        let ctx = context();
        let fields = [field("status"), field("status")];
        let results = facets(&ctx, batch(FacetView::Traces, &fields))
            .await
            .expect("batch");
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].as_ref().expect("first"),
            results[1].as_ref().expect("second")
        );
    }

    #[tokio::test]
    async fn no_field_or_too_many_fail_the_batch() {
        let ctx = context();
        let none: [FacetField<'_>; 0] = [];
        assert!(matches!(
            facets(&ctx, batch(FacetView::Traces, &none)).await,
            Err(RunError::InvalidRequest(_))
        ));
        let many = [field("status"); MAX_FACET_FIELDS + 1];
        assert!(matches!(
            facets(&ctx, batch(FacetView::Traces, &many)).await,
            Err(RunError::InvalidRequest(_))
        ));
    }

    #[tokio::test]
    async fn bad_filter_fails_the_batch_before_the_fields() {
        let ctx = context();
        let fields = [field("input")];
        let mut request = batch(FacetView::Traces, &fields);
        request.filter = Some("error_count >");
        assert!(matches!(
            facets(&ctx, request).await,
            Err(RunError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn only_bad_fields_answer_their_errors() {
        let ctx = context();
        let fields = [field("input"), field("output")];
        let results = facets(&ctx, batch(FacetView::Traces, &fields))
            .await
            .expect("batch");
        assert_eq!(results.len(), 2);
        for result in &results {
            assert!(
                matches!(result, Err(RunError::InvalidRequest(_))),
                "{result:?}"
            );
        }
    }
}
