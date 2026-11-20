//! Score list and the latest score of each name on a trace or session.
//!
//! Default exclusions do not apply (spec 09). The list is the `scores` view
//! with equality conjuncts and a `created_at` window. Latest scores follow
//! D97: greatest `updated_at`, then greatest `score_id`.

use std::time::Duration;

use arrow::array::{Array, ArrayRef, AsArray, Int64Array};
use arrow::datatypes::{DataType, Int64Type};
use arrow::error::ArrowError;
use arrow::record_batch::RecordBatch;
use cetra_core::enums::{ScoreSource, ScoreTargetKind};
use cetra_core::schema::scores;
use datafusion::common::ScalarValue;
use datafusion::error::DataFusionError;
use datafusion::functions_aggregate::expr_fn::{count, count_distinct, max};
use datafusion::functions_window::expr_fn::row_number;
use datafusion::logical_expr::{Expr, ExprFunctionExt, cast, col, lit};
use datafusion::prelude::DataFrame;

use super::cursor::{after, assemble, decode, order_by};
use super::{MAX_PAGE, Page, SortOrder};
use crate::query_error::{RunError, classify};
use crate::views::{QueryContext, TimeWindow};

const LIST: &str = "scores";
const TARGET_KEY: &str = "target_key";
const RANK: &str = "__rn";
const TIES: &[&str] = &[scores::SCORE_ID];
const DAY_NS: i64 = 86_400_000_000_000;
const BUCKET: &str = "bucket";
const COUNT: &str = "count";
const USERS: &str = "users";
const LAST: &str = "last_created_at";

/// Most names one [`score_name_counts`] takes (D-IC-9).
pub const MAX_COUNT_NAMES: usize = 1_000;

/// Live scores of one name, in one UTC day or in the whole window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoreNameCount {
    /// Score name.
    pub name: String,
    /// Start of the UTC day (ns), or `window.from` when `by_day` is false.
    pub bucket_start: i64,
    /// Live scores.
    pub count: u64,
    /// Distinct non-null `user_id`s.
    pub users: u64,
    /// Greatest `created_at` among them (ns).
    pub last_created_at: i64,
}

/// Names and window of one [`score_name_counts`].
#[derive(Debug, Clone, Copy)]
pub struct ScoreNameCountsRequest<'a> {
    /// Score names; empty returns no rows. At most [`MAX_COUNT_NAMES`].
    pub names: &'a [String],
    /// `created_at ∈ [from, to)`.
    pub window: TimeWindow,
    /// `true`: one row per (name, UTC day); `false`: one row per name for the whole window.
    pub by_day: bool,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// Live scores per name, in the day or the window of the request (D-IC-9). Rows are sorted by
/// name, then by bucket. Names with no live score in the window have no row.
///
/// # Errors
///
/// [`RunError::InvalidRequest`] for more than [`MAX_COUNT_NAMES`] names; [`RunError::Timeout`];
/// DataFusion failures through [`classify`] and `ctx.fail`.
pub async fn score_name_counts(
    ctx: &QueryContext,
    request: ScoreNameCountsRequest<'_>,
) -> Result<Vec<ScoreNameCount>, RunError> {
    if request.names.len() > MAX_COUNT_NAMES {
        return Err(RunError::InvalidRequest(format!(
            "at most {MAX_COUNT_NAMES} names"
        )));
    }
    if request.names.is_empty() {
        return Ok(Vec::new());
    }
    match tokio::time::timeout(request.timeout, counts(ctx, &request)).await {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

async fn counts(
    ctx: &QueryContext,
    request: &ScoreNameCountsRequest<'_>,
) -> Result<Vec<ScoreNameCount>, RunError> {
    let names: Vec<Expr> = request
        .names
        .iter()
        .map(|name| lit(name.as_str()))
        .collect();
    let created = cast(col(scores::CREATED_AT), DataType::Int64);
    let predicate = col(scores::NAME)
        .in_list(names, false)
        .and(request.window.predicate(scores::CREATED_AT));
    let mut keys = vec![col(scores::NAME)];
    let mut order = vec![col(scores::NAME).sort(true, false)];
    if request.by_day {
        keys.push((created.clone() - created.clone() % lit(DAY_NS)).alias(BUCKET));
        order.push(col(BUCKET).sort(true, false));
    }
    let batches = ctx
        .session()
        .table(LIST)
        .await
        .map_err(classify)?
        .filter(predicate)
        .map_err(classify)?
        .aggregate(
            keys,
            vec![
                count(lit(1_i64)).alias(COUNT),
                count_distinct(col(scores::USER_ID)).alias(USERS),
                max(created).alias(LAST),
            ],
        )
        .map_err(classify)?
        .sort(order)
        .map_err(classify)?
        .collect()
        .await
        .map_err(|error| ctx.fail(error))?;
    counts_of(&batches, request)
}

/// Converts the aggregate rows: name, then the bucket when `by_day`, then count, users and last.
fn counts_of(
    batches: &[RecordBatch],
    request: &ScoreNameCountsRequest<'_>,
) -> Result<Vec<ScoreNameCount>, RunError> {
    let mut found = Vec::new();
    let offset = usize::from(request.by_day);
    for batch in batches {
        let names =
            arrow::compute::cast(batch.column(0), &DataType::Utf8).map_err(arrow_failure)?;
        let names = names.as_string::<i32>();
        let buckets = if request.by_day {
            Some(int64_column(batch.column(1))?)
        } else {
            None
        };
        let buckets = buckets
            .as_ref()
            .map(|column| column.as_primitive::<Int64Type>());
        let counts = int64_column(batch.column(1 + offset))?;
        let counts = counts.as_primitive::<Int64Type>();
        let users = int64_column(batch.column(2 + offset))?;
        let users = users.as_primitive::<Int64Type>();
        let lasts = int64_column(batch.column(3 + offset))?;
        let lasts = lasts.as_primitive::<Int64Type>();
        for row in 0..batch.num_rows() {
            let bucket_start = match buckets {
                Some(buckets) => required_int(buckets, row)?,
                None => request.window.from(),
            };
            found.push(ScoreNameCount {
                name: names
                    .is_valid(row)
                    .then(|| names.value(row).to_owned())
                    .ok_or_else(bad_row)?,
                bucket_start,
                count: non_negative(required_int(counts, row)?)?,
                users: non_negative(required_int(users, row)?)?,
                last_created_at: required_int(lasts, row)?,
            });
        }
    }
    Ok(found)
}

fn int64_column(column: &ArrayRef) -> Result<ArrayRef, RunError> {
    arrow::compute::cast(column, &DataType::Int64).map_err(arrow_failure)
}

fn required_int(column: &Int64Array, row: usize) -> Result<i64, RunError> {
    if column.is_null(row) {
        return Err(bad_row());
    }
    Ok(column.value(row))
}

fn non_negative(value: i64) -> Result<u64, RunError> {
    u64::try_from(value).map_err(|_| bad_row())
}

fn arrow_failure(error: ArrowError) -> RunError {
    RunError::Internal(DataFusionError::ArrowError(Box::new(error), None))
}

fn bad_row() -> RunError {
    RunError::Internal(DataFusionError::Internal(
        "a view returned a null where a value is required".to_owned(),
    ))
}

/// Columns of [`latest_scores`], in order.
pub const LATEST_SCORE_COLUMNS: &[&str] = &[
    TARGET_KEY,
    scores::NAME,
    scores::SCORE_ID,
    scores::DATA_TYPE,
    scores::VALUE_NUM,
    scores::VALUE_STR,
    scores::SOURCE,
];

/// One page of the scores view.
///
/// Every set field is an equality conjunct. `window` applies to `created_at`.
#[derive(Debug, Clone)]
pub struct ScoreListRequest<'a> {
    /// `target` column. Compared with [`ScoreTargetKind::as_str`].
    pub target: Option<ScoreTargetKind>,
    /// `trace_id`. Uppercase hex is accepted.
    pub trace_id: Option<&'a str>,
    /// `span_id`. Uppercase hex is accepted.
    pub span_id: Option<&'a str>,
    /// `session_id`.
    pub session_id: Option<&'a str>,
    /// `name`.
    pub name: Option<&'a str>,
    /// `source` column. Compared with [`ScoreSource::as_str`].
    pub source: Option<ScoreSource>,
    /// `evaluator_id` column, compared with the hyphenated lowercase UUID text.
    pub evaluator_id: Option<&'a str>,
    /// `rule_id` column, compared with the hyphenated lowercase UUID text.
    pub rule_id: Option<&'a str>,
    /// `created_at ∈ [from, to)`.
    pub window: Option<TimeWindow>,
    /// Rows to return, from 1 to [`MAX_PAGE`].
    pub limit: usize,
    /// Cursor from the previous page.
    pub cursor: Option<&'a str>,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// Trace or session ids for [`latest_scores`].
#[derive(Debug, Clone, Copy)]
pub enum ScoreTargets<'a> {
    /// Latest scores whose `target` is a trace.
    Traces(&'a [&'a str]),
    /// Latest scores whose `target` is a session.
    Sessions(&'a [&'a str]),
}

/// One page of scores, ordered `created_at DESC, score_id DESC`.
///
/// The cursor's sort name is `created_at` and its direction is descending.
///
/// # Errors
///
/// [`RunError::InvalidRequest`] for a bad limit, cursor, or id.
/// [`RunError::Timeout`] when the query exceeds `timeout`.
pub async fn score_list(
    ctx: &QueryContext,
    request: ScoreListRequest<'_>,
) -> Result<Page, RunError> {
    if !(1..=MAX_PAGE).contains(&request.limit) {
        return Err(RunError::InvalidRequest(
            "limit must be between 1 and 1000".to_owned(),
        ));
    }
    match tokio::time::timeout(request.timeout, list(ctx, &request)).await {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

/// Latest live score of each name for the given traces or sessions.
///
/// Rows are sorted by `target_key`, then `name`. An empty key list returns
/// no batches and does not run a query. More than [`MAX_PAGE`] keys is
/// [`RunError::InvalidRequest`].
///
/// # Errors
///
/// [`RunError::InvalidRequest`] for too many keys or a trace id that is not
/// 32 hex digits. [`RunError::Timeout`] when the query exceeds `timeout`.
pub async fn latest_scores(
    ctx: &QueryContext,
    targets: ScoreTargets<'_>,
    timeout: Duration,
) -> Result<Vec<RecordBatch>, RunError> {
    let Some(prepared) = prepare_keys(targets)? else {
        return Ok(Vec::new());
    };
    match tokio::time::timeout(
        timeout,
        latest(ctx, prepared.kind, prepared.column, &prepared.keys),
    )
    .await
    {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

/// The live version of each score in `ids` (scores-view columns), in no particular order.
///
/// Ids are UUID strings, either case. An empty list returns no batches and runs no query.
///
/// # Errors
///
/// [`RunError::InvalidRequest`] for more than [`MAX_PAGE`] ids or an id that is not a UUID.
/// [`RunError::Timeout`] when the query exceeds `timeout`.
pub async fn scores_by_id(
    ctx: &QueryContext,
    ids: &[&str],
    timeout: Duration,
) -> Result<Vec<RecordBatch>, RunError> {
    let Some(keys) = prepare_score_ids(ids)? else {
        return Ok(Vec::new());
    };
    match tokio::time::timeout(timeout, by_id(ctx, &keys)).await {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

/// `None` is an empty id list: the caller returns no batches and runs nothing.
fn prepare_score_ids(ids: &[&str]) -> Result<Option<Vec<String>>, RunError> {
    if ids.len() > MAX_PAGE {
        return Err(RunError::InvalidRequest("at most 1000 ids".to_owned()));
    }
    if ids.is_empty() {
        return Ok(None);
    }
    let mut keys = Vec::with_capacity(ids.len());
    for id in ids {
        let uuid = uuid::Uuid::parse_str(id)
            .map_err(|_| RunError::InvalidRequest("invalid score_id".to_owned()))?;
        keys.push(uuid.to_string());
    }
    Ok(Some(keys))
}

async fn by_id(ctx: &QueryContext, keys: &[String]) -> Result<Vec<RecordBatch>, RunError> {
    let keys: Vec<Expr> = keys.iter().map(|key| lit(key.as_str())).collect();
    ctx.session()
        .table(LIST)
        .await
        .map_err(classify)?
        .filter(col(scores::SCORE_ID).in_list(keys, false))
        .map_err(classify)?
        .collect()
        .await
        .map_err(|error| ctx.fail(error))
}

struct PreparedKeys {
    kind: ScoreTargetKind,
    column: &'static str,
    keys: Vec<String>,
}

/// `None` is an empty key list: the caller returns no batches and runs nothing.
fn prepare_keys(targets: ScoreTargets<'_>) -> Result<Option<PreparedKeys>, RunError> {
    let (kind, column, ids, hex_width) = match targets {
        ScoreTargets::Traces(ids) => (ScoreTargetKind::Trace, scores::TRACE_ID, ids, Some(32)),
        ScoreTargets::Sessions(ids) => (ScoreTargetKind::Session, scores::SESSION_ID, ids, None),
    };
    if ids.len() > MAX_PAGE {
        return Err(RunError::InvalidRequest("at most 1000 keys".to_owned()));
    }
    if ids.is_empty() {
        return Ok(None);
    }
    let mut keys = Vec::with_capacity(ids.len());
    for id in ids {
        keys.push(match hex_width {
            Some(width) => hex_id(id, width, "trace_id")?,
            None => (*id).to_owned(),
        });
    }
    Ok(Some(PreparedKeys { kind, column, keys }))
}

async fn list(ctx: &QueryContext, request: &ScoreListRequest<'_>) -> Result<Page, RunError> {
    let cursor = match request.cursor {
        Some(text) => {
            let cursor = decode(text, LIST, scores::CREATED_AT, SortOrder::Desc)?;
            if cursor.tie_len() != 1 {
                return Err(RunError::InvalidRequest("invalid cursor".to_owned()));
            }
            Some(cursor)
        }
        None => None,
    };
    let mut frame = ctx.session().table(LIST).await.map_err(classify)?;
    if let Some(predicate) = filters(request)? {
        frame = frame.filter(predicate).map_err(classify)?;
    }
    if let Some(cursor) = &cursor {
        frame = frame
            .filter(after(cursor, scores::CREATED_AT, TIES))
            .map_err(classify)?;
    }
    let frame = frame
        .sort(order_by(scores::CREATED_AT, TIES, SortOrder::Desc))
        .map_err(classify)?
        .limit(0, Some(request.limit + 1))
        .map_err(classify)?;
    let batches = frame.collect().await.map_err(|error| ctx.fail(error))?;
    assemble(
        batches,
        request.limit,
        LIST,
        scores::CREATED_AT,
        SortOrder::Desc,
        TIES,
    )
}

fn filters(request: &ScoreListRequest<'_>) -> Result<Option<Expr>, RunError> {
    let mut conjuncts = Vec::new();
    if let Some(target) = request.target {
        conjuncts.push(col(scores::TARGET).eq(lit(target.as_str())));
    }
    if let Some(trace_id) = request.trace_id {
        conjuncts.push(col(scores::TRACE_ID).eq(lit(hex_id(trace_id, 32, "trace_id")?)));
    }
    if let Some(span_id) = request.span_id {
        conjuncts.push(col(scores::SPAN_ID).eq(lit(hex_id(span_id, 16, "span_id")?)));
    }
    if let Some(session_id) = request.session_id {
        conjuncts.push(col(scores::SESSION_ID).eq(lit(session_id)));
    }
    if let Some(name) = request.name {
        conjuncts.push(col(scores::NAME).eq(lit(name)));
    }
    if let Some(source) = request.source {
        conjuncts.push(col(scores::SOURCE).eq(lit(source.as_str())));
    }
    if let Some(evaluator_id) = request.evaluator_id {
        conjuncts.push(col(scores::EVALUATOR_ID).eq(lit(evaluator_id)));
    }
    if let Some(rule_id) = request.rule_id {
        conjuncts.push(col(scores::RULE_ID).eq(lit(rule_id)));
    }
    if let Some(window) = request.window {
        conjuncts.push(window.predicate(scores::CREATED_AT));
    }
    Ok(conjuncts.into_iter().reduce(Expr::and))
}

async fn latest(
    ctx: &QueryContext,
    kind: ScoreTargetKind,
    key_column: &str,
    keys: &[String],
) -> Result<Vec<RecordBatch>, RunError> {
    let keys: Vec<Expr> = keys.iter().map(|key| lit(key.as_str())).collect();
    let frame = ctx
        .session()
        .table(LIST)
        .await
        .map_err(classify)?
        .filter(
            col(scores::TARGET)
                .eq(lit(kind.as_str()))
                .and(col(key_column).in_list(keys, false))
                // Issue occurrences never show as score columns or briefs (D-IC-18).
                .and(col(scores::SOURCE).not_eq(lit(ScoreSource::Issue.as_str()))),
        )
        .map_err(classify)?
        .select(vec![
            col(key_column).alias(TARGET_KEY),
            col(scores::NAME),
            col(scores::SCORE_ID),
            col(scores::DATA_TYPE),
            col(scores::VALUE_NUM),
            col(scores::VALUE_STR),
            col(scores::SOURCE),
            col(scores::UPDATED_AT),
        ])
        .map_err(classify)?;
    let ranked = rank_latest(frame)?;
    let batches = ranked
        .filter(col(RANK).eq(lit(ScalarValue::UInt64(Some(1)))))
        .map_err(classify)?
        .select(
            LATEST_SCORE_COLUMNS
                .iter()
                .copied()
                .map(col)
                .collect::<Vec<_>>(),
        )
        .map_err(classify)?
        .sort(vec![
            col(TARGET_KEY).sort(true, false),
            col(scores::NAME).sort(true, false),
        ])
        .map_err(classify)?
        .collect()
        .await
        .map_err(|error| ctx.fail(error))?;
    Ok(batches)
}

fn rank_latest(frame: DataFrame) -> Result<DataFrame, RunError> {
    let rank = row_number()
        .partition_by(vec![col(TARGET_KEY), col(scores::NAME)])
        .order_by(vec![
            col(scores::UPDATED_AT).sort(false, true),
            col(scores::SCORE_ID).sort(false, true),
        ])
        .build()
        .map_err(classify)?
        .alias(RANK);
    frame.window(vec![rank]).map_err(classify)
}

fn hex_id(text: &str, width: usize, name: &str) -> Result<String, RunError> {
    if text.len() == width && text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(text.to_ascii_lowercase());
    }
    Err(RunError::InvalidRequest(format!(
        "{name} must be {width} hex digits"
    )))
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::Duration;

    use arrow::json::{ArrayWriter, WriterBuilder};
    use arrow::record_batch::RecordBatch;
    use cetra_core::enums::{ScoreSource, ScoreTargetKind};
    use cetra_core::ids::TraceId;
    use cetra_core::score::{ScoreTarget, ScoreValue};
    use cetra_core::time_serde::parse_rfc3339;
    use datafusion::execution::runtime_env::RuntimeEnv;
    use serde_json::{Value, json};

    use super::{
        LATEST_SCORE_COLUMNS, MAX_COUNT_NAMES, ScoreListRequest, ScoreNameCount,
        ScoreNameCountsRequest, ScoreTargets, latest_scores, score_list, score_name_counts,
        scores_by_id,
    };
    use crate::query_error::RunError;
    use crate::reads::Page;
    use crate::test_support::{
        dataset_scores, dataset_spans, example_query, live_scores_table, score_row, spans_table,
        traces_table,
    };
    use crate::views::{ProjectQuery, QueryContext, TimeWindow};

    const TIMEOUT: Duration = Duration::from_secs(30);

    fn context() -> QueryContext {
        QueryContext::new(example_query(), None).expect("context")
    }

    fn request() -> ScoreListRequest<'static> {
        ScoreListRequest {
            target: None,
            trace_id: None,
            span_id: None,
            session_id: None,
            name: None,
            source: None,
            evaluator_id: None,
            rule_id: None,
            window: None,
            limit: 50,
            cursor: None,
            timeout: TIMEOUT,
        }
    }

    fn fixture(name: &str) -> Value {
        let text = match name {
            "scores.json" => include_str!("../../../../fixtures/api/responses/scores.json"),
            "traces-include-eval.json" => {
                include_str!("../../../../fixtures/api/responses/traces-include-eval.json")
            }
            "sessions-include-eval.json" => {
                include_str!("../../../../fixtures/api/responses/sessions-include-eval.json")
            }
            other => panic!("unknown fixture {other}"),
        };
        serde_json::from_str(text).expect(name)
    }

    fn json_rows(batches: &[RecordBatch]) -> Vec<Value> {
        if batches.iter().all(|batch| batch.num_rows() == 0) {
            return Vec::new();
        }
        let mut buffer = Vec::new();
        let mut writer: ArrayWriter<&mut Vec<u8>> = WriterBuilder::new()
            .with_explicit_nulls(true)
            .with_timestamp_tz_format("%Y-%m-%dT%H:%M:%S%.9fZ".to_owned())
            .with_timestamp_format("%Y-%m-%dT%H:%M:%S%.9f".to_owned())
            .build(&mut buffer);
        for batch in batches {
            writer.write(batch).expect("encode");
        }
        writer.finish().expect("finish");
        drop(writer);
        serde_json::from_slice(&buffer).expect("rows")
    }

    fn rows(page: &Page) -> Vec<Value> {
        json_rows(&page.batches)
    }

    fn values_match(actual: &Value, expected: &Value) -> bool {
        match (actual, expected) {
            (Value::Number(left), Value::Number(right)) => match (left.as_i64(), right.as_i64()) {
                (Some(left), Some(right)) => left == right,
                _ => {
                    let left = left.as_f64().expect("left");
                    let right = right.as_f64().expect("right");
                    (left - right).abs() < 1e-12
                }
            },
            (Value::Array(left), Value::Array(right)) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right)
                        .all(|(left, right)| values_match(left, right))
            }
            (Value::Object(left), Value::Object(right)) => {
                left.len() == right.len()
                    && left.iter().all(|(key, value)| {
                        right
                            .get(key)
                            .is_some_and(|expected| values_match(value, expected))
                    })
            }
            _ => actual == expected,
        }
    }

    fn ids(rows: &[Value]) -> Vec<String> {
        rows.iter()
            .map(|row| row["score_id"].as_str().expect("score_id").to_owned())
            .collect()
    }

    fn or_null(value: &Value, key: &str) -> Value {
        value.get(key).cloned().unwrap_or(Value::Null)
    }

    /// One API score object as a `scores` view row.
    fn view_score(score: &Value) -> Value {
        let target = &score["target"];
        let value = &score["value"];
        let data_type = value["data_type"].as_str().expect("data_type");
        let (value_num, value_str) = match data_type {
            "numeric" => (value["value"].clone(), Value::Null),
            "boolean" => (
                json!(if value["value"].as_bool().expect("bool") {
                    1.0
                } else {
                    0.0
                }),
                Value::Null,
            ),
            "text" => (Value::Null, value["value"].clone()),
            "categorical" => {
                let inner = &value["value"];
                (
                    inner.get("value").cloned().unwrap_or(Value::Null),
                    inner["label"].clone(),
                )
            }
            other => panic!("unknown data_type {other}"),
        };
        let session_id = if target["type"] == "session" {
            target["session_id"].clone()
        } else {
            or_null(score, "session_id")
        };
        json!({
            "score_id": score["score_id"],
            "target": target["type"],
            "trace_id": or_null(target, "trace_id"),
            "span_id": or_null(target, "span_id"),
            "session_id": session_id,
            "user_id": or_null(score, "user_id"),
            "name": score["name"],
            "data_type": data_type,
            "value_num": value_num,
            "value_str": value_str,
            "reason": or_null(score, "reason"),
            "source": score["source"],
            "evaluator_id": or_null(score, "evaluator_id"),
            "rule_id": or_null(score, "rule_id"),
            "experiment_id": or_null(score, "experiment_id"),
            "dataset_item_id": or_null(score, "dataset_item_id"),
            "author_user_id": or_null(score, "author_user_id"),
            "metadata": score.get("metadata").cloned().unwrap_or_else(|| json!({})),
            "created_at": score["created_at"],
            "updated_at": score["updated_at"],
            "deleted": score["deleted"],
            "ingest_time": score["ingest_time"],
        })
    }

    fn assert_view_rows(actual: &[Value], expected: &[Value]) {
        assert_eq!(actual.len(), expected.len());
        for (row, fixture) in actual.iter().zip(expected) {
            let expected = view_score(fixture);
            let object = expected.as_object().expect("object");
            for (key, value) in object {
                assert!(
                    row.get(key)
                        .is_some_and(|actual| values_match(actual, value)),
                    "{key}: {} vs {value}",
                    row.get(key).unwrap_or(&Value::Null)
                );
            }
        }
    }

    #[tokio::test]
    async fn score_list_matches_the_fixture() {
        let page = score_list(&context(), request()).await.expect("list");
        let listed = fixture("scores.json");
        let expected = listed["data"].as_array().expect("data");
        let actual = rows(&page);
        assert_eq!(ids(&actual), ids(expected));
        assert_view_rows(&actual, expected);
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn each_filter_field_narrows_the_list() {
        let ctx = context();
        let mut target = request();
        target.target = Some(ScoreTargetKind::Session);
        let page = score_list(&ctx, target).await.expect("target");
        assert_eq!(ids(&rows(&page)), ["01900000-0000-7000-8000-000000000305"]);

        let mut trace = request();
        let upper = "A1000000000000000000000000000001";
        trace.trace_id = Some(upper);
        let page = score_list(&ctx, trace).await.expect("trace");
        assert_eq!(
            ids(&rows(&page)),
            [
                "01900000-0000-7000-8000-000000000303",
                "01900000-0000-7000-8000-000000000302",
                "01900000-0000-7000-8000-000000000301",
            ]
        );

        let mut span = request();
        span.span_id = Some("B100000000000014");
        let page = score_list(&ctx, span).await.expect("span");
        assert_eq!(ids(&rows(&page)), ["01900000-0000-7000-8000-000000000303"]);

        let mut session = request();
        session.session_id = Some("sess-002");
        let page = score_list(&ctx, session).await.expect("session");
        assert_eq!(ids(&rows(&page)), ["01900000-0000-7000-8000-000000000306"]);

        let mut name = request();
        name.name = Some("helpfulness");
        let page = score_list(&ctx, name).await.expect("name");
        assert_eq!(
            ids(&rows(&page)),
            [
                "01900000-0000-7000-8000-000000000302",
                "01900000-0000-7000-8000-000000000301",
            ]
        );

        let mut source = request();
        source.source = Some(ScoreSource::EndUser);
        let page = score_list(&ctx, source).await.expect("source");
        assert_eq!(ids(&rows(&page)), ["01900000-0000-7000-8000-000000000304"]);

        let mut window = request();
        window.window = Some(
            TimeWindow::new(
                parse_rfc3339("2026-01-01T10:07:00Z").expect("from"),
                parse_rfc3339("2026-01-01T10:11:00Z").expect("to"),
            )
            .expect("window"),
        );
        let page = score_list(&ctx, window).await.expect("window");
        assert_eq!(
            ids(&rows(&page)),
            [
                "01900000-0000-7000-8000-000000000305",
                "01900000-0000-7000-8000-000000000304",
            ]
        );

        match score_list(
            &ctx,
            ScoreListRequest {
                trace_id: Some("abc"),
                ..request()
            },
        )
        .await
        {
            Err(RunError::InvalidRequest(message)) => {
                assert_eq!(message, "trace_id must be 32 hex digits");
            }
            other => panic!("expected an invalid trace id, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn score_pages_walk_the_full_list() {
        let ctx = context();
        let full = score_list(&ctx, request()).await.expect("full");
        let expected = ids(&rows(&full));
        assert!(full.next_cursor.is_none());
        let mut walked = Vec::new();
        let mut cursor = None;
        loop {
            let mut page_request = request();
            page_request.limit = 2;
            page_request.cursor = cursor.as_deref();
            let page = score_list(&ctx, page_request).await.expect("page");
            let page_ids = ids(&rows(&page));
            assert!(page_ids.len() <= 2);
            walked.extend(page_ids);
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(walked, expected);
    }

    #[tokio::test]
    async fn latest_scores_match_the_row_fixtures() {
        let ctx = context();
        let traces = fixture("traces-include-eval.json");
        let traces = traces["data"].as_array().expect("traces");
        let trace_ids: Vec<&str> = traces
            .iter()
            .map(|trace| trace["trace_id"].as_str().expect("id"))
            .collect();
        let batches = latest_scores(&ctx, ScoreTargets::Traces(&trace_ids), TIMEOUT)
            .await
            .expect("traces");
        let actual = json_rows(&batches);
        assert_eq!(
            actual[0]
                .as_object()
                .expect("row")
                .keys()
                .collect::<Vec<_>>(),
            LATEST_SCORE_COLUMNS
        );
        for trace in traces {
            let id = trace["trace_id"].as_str().expect("id");
            let rows: Vec<&Value> = actual
                .iter()
                .filter(|row| row["target_key"] == id)
                .collect();
            assert_briefs(&rows, trace["scores"].as_array().expect("scores"));
        }

        let sessions = fixture("sessions-include-eval.json");
        let sessions = sessions["data"].as_array().expect("sessions");
        let session_ids: Vec<&str> = sessions
            .iter()
            .map(|session| session["session_id"].as_str().expect("id"))
            .collect();
        let batches = latest_scores(&ctx, ScoreTargets::Sessions(&session_ids), TIMEOUT)
            .await
            .expect("sessions");
        let actual = json_rows(&batches);
        for session in sessions {
            let id = session["session_id"].as_str().expect("id");
            let rows: Vec<&Value> = actual
                .iter()
                .filter(|row| row["target_key"] == id)
                .collect();
            assert_briefs(&rows, session["scores"].as_array().expect("scores"));
        }
    }

    fn assert_briefs(actual: &[&Value], expected: &[Value]) {
        assert_eq!(actual.len(), expected.len());
        for (row, fixture) in actual.iter().zip(expected) {
            assert_eq!(row["name"], fixture["name"]);
            assert_eq!(row["score_id"], fixture["score_id"]);
            assert_eq!(row["source"], fixture["source"]);
            assert_eq!(row["data_type"], fixture["value"]["data_type"]);
            let value = &fixture["value"]["value"];
            match row["data_type"].as_str() {
                Some("boolean") => {
                    let number = if value.as_bool().expect("bool") {
                        1.0
                    } else {
                        0.0
                    };
                    assert!(values_match(&row["value_num"], &json!(number)));
                    assert_eq!(row["value_str"], Value::Null);
                }
                Some("numeric") => {
                    assert!(values_match(&row["value_num"], value));
                    assert_eq!(row["value_str"], Value::Null);
                }
                Some("text") => {
                    assert_eq!(row["value_num"], Value::Null);
                    assert_eq!(&row["value_str"], value);
                }
                Some("categorical") => {
                    assert_eq!(row["value_str"], value["label"]);
                    assert!(values_match(
                        &row["value_num"],
                        value.get("value").unwrap_or(&Value::Null)
                    ));
                }
                other => panic!("unexpected data_type {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn scores_by_id_returns_the_live_rows() {
        let ctx = context();
        let listed = fixture("scores.json");
        let expected: Vec<Value> = listed["data"]
            .as_array()
            .expect("data")
            .iter()
            .filter(|score| {
                matches!(
                    score["score_id"].as_str(),
                    Some(
                        "01900000-0000-7000-8000-000000000301"
                            | "01900000-0000-7000-8000-000000000302"
                    )
                )
            })
            .cloned()
            .collect();
        assert_eq!(expected.len(), 2);

        let wanted = [
            "01900000-0000-7000-8000-000000000301",
            "01900000-0000-7000-8000-000000000302",
        ];
        let batches = scores_by_id(&ctx, &wanted, TIMEOUT).await.expect("by id");
        let actual = json_rows(&batches);
        assert_eq!(actual.len(), 2);
        let mut found = ids(&actual);
        found.sort();
        assert_eq!(found, wanted);
        for score in &expected {
            let row = actual
                .iter()
                .find(|row| row["score_id"] == score["score_id"])
                .expect("row");
            assert_view_rows(std::slice::from_ref(row), std::slice::from_ref(score));
        }

        let upper = wanted.map(str::to_ascii_uppercase);
        let upper: Vec<&str> = upper.iter().map(String::as_str).collect();
        let batches = scores_by_id(&ctx, &upper, TIMEOUT).await.expect("upper");
        assert_eq!(json_rows(&batches).len(), 2);

        let unknown = ["0190aaaa-0000-7000-8000-000000000001"];
        let batches = scores_by_id(&ctx, &unknown, TIMEOUT)
            .await
            .expect("unknown");
        assert!(json_rows(&batches).is_empty());

        let empty = scores_by_id(&ctx, &[], TIMEOUT).await.expect("empty");
        assert!(empty.is_empty());

        match scores_by_id(&ctx, &["nope"], TIMEOUT).await {
            Err(RunError::InvalidRequest(message)) => assert_eq!(message, "invalid score_id"),
            other => panic!("expected an invalid score_id, got {other:?}"),
        }
        let many = vec!["01900000-0000-7000-8000-000000000301"; 1001];
        match scores_by_id(&ctx, &many, TIMEOUT).await {
            Err(RunError::InvalidRequest(message)) => assert_eq!(message, "at most 1000 ids"),
            other => panic!("expected too many ids, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn latest_scores_leave_out_issue_occurrences() {
        // Trace 0 is `00010001000100010001000100010001` (see `trace_id_of` in test_support).
        let trace = "00010001000100010001000100010001";
        let mut occurrence = score_row(1, 0, 100, 100, 100, false, "issue:a");
        occurrence.source = ScoreSource::Issue;
        let mut api = score_row(2, 0, 100, 100, 100, false, "quality");
        api.source = ScoreSource::Api;
        let query = ProjectQuery {
            scores: live_scores_table(&[occurrence, api]),
            ..example_query()
        };
        let ctx = QueryContext::new(query, None).expect("context");
        let batches = latest_scores(&ctx, ScoreTargets::Traces(&[trace]), TIMEOUT)
            .await
            .expect("latest");
        let rows = json_rows(&batches);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0]["name"], "quality");
        assert_eq!(rows[0]["source"], "api");
    }

    #[tokio::test]
    async fn the_greater_score_id_wins_a_latest_tie() {
        let template = dataset_scores().into_iter().next().expect("score");
        let mut low = template.clone();
        let mut high = template.clone();
        low.score_id = "01900000-0000-7000-8000-0000000003a1".parse().expect("low");
        high.score_id = "01900000-0000-7000-8000-0000000003a2"
            .parse()
            .expect("high");
        low.name = "tie".to_owned();
        high.name = "tie".to_owned();
        low.value = ScoreValue::Numeric(0.1);
        high.value = ScoreValue::Numeric(0.9);
        let trace_id = "a2000000000000000000000000000001";
        let trace = TraceId::from_str(trace_id).expect("trace");
        low.target = ScoreTarget::Trace { trace_id: trace };
        high.target = ScoreTarget::Trace { trace_id: trace };
        high.updated_at = low.updated_at;
        let ctx = QueryContext::new(
            ProjectQuery {
                spans: spans_table(&dataset_spans()),
                traces: traces_table(&dataset_spans()),
                scores: live_scores_table(&[low, high]),
                runtime: Arc::new(RuntimeEnv::default()),
                target_partitions: NonZeroUsize::new(1).expect("one partition"),
                optimizer_rules: Vec::new(),

                suspects: crate::verify::Suspects::detached(),
                visible_from: None,
            },
            None,
        )
        .expect("context");
        let batches = latest_scores(&ctx, ScoreTargets::Traces(&[trace_id]), TIMEOUT)
            .await
            .expect("latest");
        let rows = json_rows(&batches);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["score_id"], "01900000-0000-7000-8000-0000000003a2");
        assert!(values_match(&rows[0]["value_num"], &json!(0.9)));
    }

    #[tokio::test]
    async fn key_limits_are_enforced_before_a_query() {
        let ctx = context();
        let empty = latest_scores(&ctx, ScoreTargets::Traces(&[]), TIMEOUT)
            .await
            .expect("empty traces");
        assert!(empty.is_empty());
        let empty = latest_scores(&ctx, ScoreTargets::Sessions(&[]), TIMEOUT)
            .await
            .expect("empty sessions");
        assert!(empty.is_empty());

        let keys = vec!["a1000000000000000000000000000001"; 1001];
        match latest_scores(&ctx, ScoreTargets::Traces(&keys), TIMEOUT).await {
            Err(RunError::InvalidRequest(message)) => assert_eq!(message, "at most 1000 keys"),
            other => panic!("expected too many keys, got {other:?}"),
        }
        match latest_scores(&ctx, ScoreTargets::Traces(&["abc"]), TIMEOUT).await {
            Err(RunError::InvalidRequest(message)) => {
                assert_eq!(message, "trace_id must be 32 hex digits");
            }
            other => panic!("expected an invalid trace id, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn score_list_filters_by_evaluator_and_rule() {
        const EVALUATOR: &str = "018f6b4e-7c1a-7c1a-8c1a-000000000003";
        const RULE: &str = "018f6b4e-7c1a-7c1a-8c1a-000000000004";
        let mut by_rule = score_row(1, 0, 100, 100, 100, false, "relevance");
        by_rule.evaluator_id = Some(EVALUATOR.parse().expect("evaluator"));
        by_rule.rule_id = Some(RULE.parse().expect("rule"));
        let mut by_evaluator = score_row(2, 1, 200, 200, 200, false, "tone");
        by_evaluator.evaluator_id = Some(EVALUATOR.parse().expect("evaluator"));
        let unlinked = score_row(3, 2, 300, 300, 300, false, "length");
        let query = ProjectQuery {
            scores: live_scores_table(&[by_rule, by_evaluator, unlinked]),
            ..example_query()
        };
        let ctx = QueryContext::new(query, None).expect("context");

        let mut evaluator = request();
        evaluator.evaluator_id = Some(EVALUATOR);
        let page = score_list(&ctx, evaluator).await.expect("evaluator");
        assert_eq!(
            ids(&rows(&page)),
            [
                "00000000-0000-0000-0000-000000000002",
                "00000000-0000-0000-0000-000000000001",
            ]
        );

        let mut rule = request();
        rule.rule_id = Some(RULE);
        let page = score_list(&ctx, rule).await.expect("rule");
        assert_eq!(ids(&rows(&page)), ["00000000-0000-0000-0000-000000000001"]);
    }

    const DAY: i64 = 86_400_000_000_000;

    /// The live scores of `scores`, in a context of the test project.
    fn counts_context(scores: &[cetra_core::score::Score]) -> QueryContext {
        let query = ProjectQuery {
            scores: live_scores_table(scores),
            ..example_query()
        };
        QueryContext::new(query, None).expect("context")
    }

    /// Three scores of `issue:a` on two days (two users, one without a user) and one of
    /// `issue:b`. Score 1 is `day1 + 100`, score 2 `day1 + 200`, score 3 `day2 + 50`, and
    /// score 4 `day1 + 400`.
    fn counted_scores(day1: i64, day2: i64) -> Vec<cetra_core::score::Score> {
        let mut first = score_row(1, 0, day1 + 100, 100, 100, false, "issue:a");
        first.user_id = Some("u1".to_owned());
        let mut second = score_row(2, 1, day1 + 200, 200, 200, false, "issue:a");
        second.user_id = Some("u2".to_owned());
        let third = score_row(3, 2, day2 + 50, 300, 300, false, "issue:a");
        let mut other = score_row(4, 3, day1 + 400, 400, 400, false, "issue:b");
        other.user_id = Some("u1".to_owned());
        vec![first, second, third, other]
    }

    fn count_row(
        name: &str,
        bucket_start: i64,
        count: u64,
        users: u64,
        last: i64,
    ) -> ScoreNameCount {
        ScoreNameCount {
            name: name.to_owned(),
            bucket_start,
            count,
            users,
            last_created_at: last,
        }
    }

    #[tokio::test]
    async fn score_name_counts_by_day() {
        let (day1, day2) = (10 * DAY, 11 * DAY);
        let ctx = counts_context(&counted_scores(day1, day2));
        let names = ["issue:a".to_owned(), "issue:b".to_owned()];
        let window = TimeWindow::new(0, 100 * DAY).expect("window");
        let request = ScoreNameCountsRequest {
            names: &names,
            window,
            by_day: true,
            timeout: TIMEOUT,
        };
        let counts = score_name_counts(&ctx, request).await.expect("by day");
        assert_eq!(
            counts,
            [
                count_row("issue:a", day1, 2, 2, day1 + 200),
                count_row("issue:a", day2, 1, 0, day2 + 50),
                count_row("issue:b", day1, 1, 1, day1 + 400),
            ]
        );
    }

    #[tokio::test]
    async fn score_name_counts_whole_window() {
        let (day1, day2) = (10 * DAY, 11 * DAY);
        let ctx = counts_context(&counted_scores(day1, day2));
        let names = ["issue:a".to_owned(), "issue:b".to_owned()];
        let window = TimeWindow::new(0, 100 * DAY).expect("window");
        let request = ScoreNameCountsRequest {
            names: &names,
            window,
            by_day: false,
            timeout: TIMEOUT,
        };
        let counts = score_name_counts(&ctx, request)
            .await
            .expect("whole window");
        assert_eq!(
            counts,
            [
                count_row("issue:a", 0, 3, 2, day2 + 50),
                count_row("issue:b", 0, 1, 1, day1 + 400),
            ]
        );
    }

    #[tokio::test]
    async fn score_name_counts_respects_the_window() {
        let (day1, day2) = (10 * DAY, 11 * DAY);
        let ctx = counts_context(&counted_scores(day1, day2));
        let names = ["issue:a".to_owned(), "issue:b".to_owned()];
        // `from` is inclusive (score 1 starts there) and `to` is exclusive (score 3 is after it).
        let window = TimeWindow::new(day1 + 100, day2).expect("window");
        let request = ScoreNameCountsRequest {
            names: &names,
            window,
            by_day: false,
            timeout: TIMEOUT,
        };
        let counts = score_name_counts(&ctx, request).await.expect("window");
        assert_eq!(
            counts,
            [
                count_row("issue:a", day1 + 100, 2, 2, day1 + 200),
                count_row("issue:b", day1 + 100, 1, 1, day1 + 400),
            ]
        );
    }

    #[tokio::test]
    async fn score_name_counts_ignores_deleted_scores() {
        let (day1, day2) = (10 * DAY, 11 * DAY);
        let mut scores = counted_scores(day1, day2);
        // A later deleted version of score 1 replaces it, so only score 2 is live for `issue:a`.
        scores.push(score_row(1, 0, day1 + 100, 500, 500, true, "issue:a"));
        let ctx = counts_context(&scores);
        let names = ["issue:a".to_owned()];
        let window = TimeWindow::new(0, 100 * DAY).expect("window");
        let request = ScoreNameCountsRequest {
            names: &names,
            window,
            by_day: true,
            timeout: TIMEOUT,
        };
        let counts = score_name_counts(&ctx, request).await.expect("live");
        assert_eq!(
            counts,
            [
                count_row("issue:a", day1, 1, 1, day1 + 200),
                count_row("issue:a", day2, 1, 0, day2 + 50),
            ]
        );
    }

    #[tokio::test]
    async fn score_name_counts_rejects_1001_names_and_is_empty_without_names() {
        let ctx = counts_context(&counted_scores(10 * DAY, 11 * DAY));
        let window = TimeWindow::new(0, 100 * DAY).expect("window");
        let many = vec!["issue:a".to_owned(); MAX_COUNT_NAMES + 1];
        let too_many = ScoreNameCountsRequest {
            names: &many,
            window,
            by_day: true,
            timeout: TIMEOUT,
        };
        assert!(matches!(
            score_name_counts(&ctx, too_many).await,
            Err(RunError::InvalidRequest(_))
        ));
        let none: Vec<String> = Vec::new();
        let empty = ScoreNameCountsRequest {
            names: &none,
            window,
            by_day: true,
            timeout: TIMEOUT,
        };
        assert!(
            score_name_counts(&ctx, empty)
                .await
                .expect("no names")
                .is_empty()
        );
    }
}
