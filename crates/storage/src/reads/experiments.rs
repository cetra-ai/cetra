//! The scores and traces of one experiment, for its results and comparison (D-J-13).
//!
//! The `scores` view holds live scores only. Experiment scores carry `experiment_id`
//! (D-J-12); experiment traces carry `metadata['llmobs.experiment_id']` (spec 02 § Context
//! fields). Default exclusions do not apply.

use std::time::Duration;

use arrow::array::{Array, ArrayRef, AsArray, Float64Array, Int64Array, StringArray};
use arrow::compute::cast;
use arrow::datatypes::{DataType, Float64Type, Int64Type};
use arrow::error::ArrowError;
use datafusion::common::ScalarValue;
use datafusion::error::DataFusionError;
use datafusion::logical_expr::{Expr, col, lit};
use futures::StreamExt;
use llmobs_core::names::metadata_key;
use llmobs_core::schema::{scores, traces};

use crate::query_error::{RunError, classify};
use crate::views::{QueryContext, ViewName};

/// Most score rows [`experiment_scores`] returns (CG-J2).
pub const MAX_EXPERIMENT_SCORES: usize = 500_000;
/// Most trace rows [`experiment_traces`] returns.
pub const MAX_EXPERIMENT_TRACES: usize = 100_000;

/// The `duration_ns` column of the `traces` view (a computed column of the view).
const DURATION_NS: &str = "duration_ns";

/// One live score of an experiment.
#[derive(Debug, Clone, PartialEq)]
pub struct ExperimentScore {
    /// Lowercase hex.
    pub trace_id: String,
    /// The dataset item (stable id) the score is about, hyphenated.
    pub dataset_item_id: Option<String>,
    /// Score name.
    pub name: String,
    /// `numeric`, `boolean`, `categorical` or `text`.
    pub data_type: String,
    /// The numeric value (or the `categorical` mapping).
    pub value_num: Option<f64>,
    /// The label of a `categorical` score, or the text of a `text` score.
    pub value_str: Option<String>,
}

/// One trace of an experiment.
#[derive(Debug, Clone, PartialEq)]
pub struct ExperimentTrace {
    /// Lowercase hex.
    pub trace_id: String,
    /// The merged trace cost, when any span has one.
    pub cost_total: Option<f64>,
    /// The trace's end minus its start, in nanoseconds.
    pub duration_ns: i64,
    /// Spans of the trace with an error status.
    pub error_count: i64,
}

/// Live, not deleted scores whose `experiment_id` is `experiment` (a UUID, hyphenated; it is
/// compared as lowercase text), in no order.
///
/// `since` is a lower bound on `created_at`; it lets DataFusion prune files.
///
/// # Errors
///
/// [`RunError::InvalidRequest`] when `experiment` is not a UUID or more than
/// [`MAX_EXPERIMENT_SCORES`] rows match (`experiment has more than 500000 scores`);
/// [`RunError::Timeout`]; other errors as `ctx.fail` classifies them.
pub async fn experiment_scores(
    ctx: &QueryContext,
    experiment: &str,
    since: i64,
    timeout: Duration,
) -> Result<Vec<ExperimentScore>, RunError> {
    let experiment = experiment_key(experiment)?;
    match tokio::time::timeout(timeout, scores_of(ctx, &experiment, since)).await {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

/// Traces whose merged `metadata['llmobs.experiment_id']` is `experiment`, in no order.
///
/// `since` is a lower bound on `start_time`, as for [`experiment_scores`].
///
/// # Errors
///
/// As [`experiment_scores`], with [`MAX_EXPERIMENT_TRACES`]
/// (`experiment has more than 100000 traces`).
pub async fn experiment_traces(
    ctx: &QueryContext,
    experiment: &str,
    since: i64,
    timeout: Duration,
) -> Result<Vec<ExperimentTrace>, RunError> {
    let experiment = experiment_key(experiment)?;
    match tokio::time::timeout(timeout, traces_of(ctx, &experiment, since)).await {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

fn experiment_key(experiment: &str) -> Result<String, RunError> {
    uuid::Uuid::parse_str(experiment)
        .map(|id| id.to_string())
        .map_err(|_| RunError::InvalidRequest("invalid experiment id".to_owned()))
}

fn since_literal(since: i64) -> Expr {
    lit(ScalarValue::TimestampNanosecond(
        Some(since),
        Some("UTC".into()),
    ))
}

async fn scores_of(
    ctx: &QueryContext,
    experiment: &str,
    since: i64,
) -> Result<Vec<ExperimentScore>, RunError> {
    let predicate = col(scores::EXPERIMENT_ID)
        .eq(lit(experiment))
        .and(col(scores::DELETED).eq(lit(false)))
        .and(col(scores::CREATED_AT).gt_eq(since_literal(since)));
    let frame = ctx
        .session()
        .table("scores")
        .await
        .map_err(classify)?
        .filter(predicate)
        .map_err(classify)?
        .select(vec![
            col(scores::TRACE_ID),
            col(scores::DATASET_ITEM_ID),
            col(scores::NAME),
            col(scores::DATA_TYPE),
            col(scores::VALUE_NUM),
            col(scores::VALUE_STR),
        ])
        .map_err(classify)?
        .limit(0, Some(MAX_EXPERIMENT_SCORES + 1))
        .map_err(classify)?;
    let mut stream = frame
        .execute_stream()
        .await
        .map_err(|error| ctx.fail(error))?;
    let mut found = Vec::new();
    let mut seen = 0;
    while let Some(batch) = stream.next().await {
        let batch = batch.map_err(|error| ctx.fail(error))?;
        seen += batch.num_rows();
        if seen > MAX_EXPERIMENT_SCORES {
            return Err(RunError::InvalidRequest(format!(
                "experiment has more than {MAX_EXPERIMENT_SCORES} scores"
            )));
        }
        let trace = texts(batch.column(0))?;
        let trace = trace.as_string::<i32>();
        let item = texts(batch.column(1))?;
        let item = item.as_string::<i32>();
        let name = texts(batch.column(2))?;
        let name = name.as_string::<i32>();
        let data_type = texts(batch.column(3))?;
        let data_type = data_type.as_string::<i32>();
        let value_num = floats(batch.column(4))?;
        let value_num = value_num.as_primitive::<Float64Type>();
        let value_str = texts(batch.column(5))?;
        let value_str = value_str.as_string::<i32>();
        for row in 0..batch.num_rows() {
            if trace.is_null(row) {
                continue;
            }
            found.push(ExperimentScore {
                trace_id: trace.value(row).to_owned(),
                dataset_item_id: text_at(item, row),
                name: required_text(name, row)?,
                data_type: required_text(data_type, row)?,
                value_num: float_at(value_num, row),
                value_str: text_at(value_str, row),
            });
        }
    }
    Ok(found)
}

async fn traces_of(
    ctx: &QueryContext,
    experiment: &str,
    since: i64,
) -> Result<Vec<ExperimentTrace>, RunError> {
    let key = metadata_key::EXPERIMENT_ID.replace('\'', "''");
    let target = ctx
        .session()
        .state()
        .create_logical_expr(
            &format!("metadata['{key}']"),
            ctx.view_schema(ViewName::Traces),
        )
        .map_err(RunError::Internal)?;
    let predicate = target
        .eq(lit(experiment))
        .and(col(traces::START_TIME).gt_eq(since_literal(since)));
    let frame = ctx
        .session()
        .table("traces")
        .await
        .map_err(classify)?
        .filter(predicate)
        .map_err(classify)?
        .select(vec![
            col(traces::TRACE_ID),
            col(traces::COST_TOTAL),
            col(DURATION_NS),
            col(traces::ERROR_COUNT),
        ])
        .map_err(classify)?
        .limit(0, Some(MAX_EXPERIMENT_TRACES + 1))
        .map_err(classify)?;
    let mut stream = frame
        .execute_stream()
        .await
        .map_err(|error| ctx.fail(error))?;
    let mut found = Vec::new();
    let mut seen = 0;
    while let Some(batch) = stream.next().await {
        let batch = batch.map_err(|error| ctx.fail(error))?;
        seen += batch.num_rows();
        if seen > MAX_EXPERIMENT_TRACES {
            return Err(RunError::InvalidRequest(format!(
                "experiment has more than {MAX_EXPERIMENT_TRACES} traces"
            )));
        }
        let trace = texts(batch.column(0))?;
        let trace = trace.as_string::<i32>();
        let cost = floats(batch.column(1))?;
        let cost = cost.as_primitive::<Float64Type>();
        let duration = ints(batch.column(2))?;
        let duration = duration.as_primitive::<Int64Type>();
        let errors = ints(batch.column(3))?;
        let errors = errors.as_primitive::<Int64Type>();
        for row in 0..batch.num_rows() {
            if trace.is_null(row) {
                continue;
            }
            found.push(ExperimentTrace {
                trace_id: trace.value(row).to_owned(),
                cost_total: float_at(cost, row),
                duration_ns: int_at(duration, row)?,
                error_count: int_at(errors, row)?,
            });
        }
    }
    Ok(found)
}

fn arrow_failure(error: ArrowError) -> RunError {
    RunError::Internal(DataFusionError::ArrowError(Box::new(error), None))
}

/// The column as `Utf8`, whether the view yields `Utf8` or `Utf8View`.
fn texts(column: &ArrayRef) -> Result<ArrayRef, RunError> {
    cast(column, &DataType::Utf8).map_err(arrow_failure)
}

/// The column as `Float64`.
fn floats(column: &ArrayRef) -> Result<ArrayRef, RunError> {
    cast(column, &DataType::Float64).map_err(arrow_failure)
}

/// The column as `Int64`.
fn ints(column: &ArrayRef) -> Result<ArrayRef, RunError> {
    cast(column, &DataType::Int64).map_err(arrow_failure)
}

fn text_at(column: &StringArray, row: usize) -> Option<String> {
    (!column.is_null(row)).then(|| column.value(row).to_owned())
}

fn required_text(column: &StringArray, row: usize) -> Result<String, RunError> {
    text_at(column, row).ok_or_else(bad_row)
}

fn float_at(column: &Float64Array, row: usize) -> Option<f64> {
    (!column.is_null(row)).then(|| column.value(row))
}

fn int_at(column: &Int64Array, row: usize) -> Result<i64, RunError> {
    if column.is_null(row) {
        return Err(bad_row());
    }
    Ok(column.value(row))
}

fn bad_row() -> RunError {
    RunError::Internal(DataFusionError::Internal(
        "a view returned a null where a value is required".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use llmobs_core::enums::SpanStatus;
    use llmobs_core::names::metadata_key;
    use llmobs_core::score::{Score, ScoreValue};
    use llmobs_core::span::Span;

    use super::{ExperimentScore, ExperimentTrace, RunError, experiment_scores, experiment_traces};
    use crate::test_support::{
        example_query, live_scores_table, score_row, span_row, spans_table, traces_table,
    };
    use crate::views::{ProjectQuery, QueryContext};

    const TIMEOUT: Duration = Duration::from_secs(30);
    const E1: &str = "01900000-0000-7000-8000-000000000e01";
    const E2: &str = "01900000-0000-7000-8000-000000000e02";
    const ITEM: &str = "01900000-0000-7000-8000-000000000d01";

    fn context_of(spans: &[Span], scores: &[Score]) -> QueryContext {
        let query = ProjectQuery {
            spans: spans_table(spans),
            traces: traces_table(spans),
            scores: live_scores_table(scores),
            ..example_query()
        };
        QueryContext::new(query, None).expect("context")
    }

    /// The 32-hex text of trace `trace` as the views write it.
    fn trace_hex(trace: u16) -> String {
        span_row(trace, 0, 0, 0).trace_id.to_hex()
    }

    /// Trace 0 of E1 (two spans, the second an error, cost 0.25 on the first), trace 1 of E2,
    /// trace 2 with no experiment.
    fn spans() -> Vec<Span> {
        let mut first = span_row(0, 0, 1_000, 0);
        first
            .metadata
            .insert(metadata_key::EXPERIMENT_ID.to_owned(), E1.to_owned());
        first.cost_total = Some(0.25);
        let mut failed = span_row(0, 1, 1_050, 0);
        failed.parent_span_id = Some(first.span_id);
        failed.status = SpanStatus::Error;
        let mut other = span_row(1, 0, 2_000, 0);
        other
            .metadata
            .insert(metadata_key::EXPERIMENT_ID.to_owned(), E2.to_owned());
        let plain = span_row(2, 0, 3_000, 0);
        vec![first, failed, other, plain]
    }

    /// One score of E1 on trace 0 (created at 100), one of E2 on trace 1 (created at 200), one
    /// with no experiment on trace 2 (created at 300).
    fn scores() -> Vec<Score> {
        let mut first = score_row(1, 0, 100, 100, 100, false, "accuracy");
        first.experiment_id = Some(E1.parse().expect("experiment"));
        first.dataset_item_id = Some(ITEM.parse().expect("item"));
        first.value = ScoreValue::Numeric(0.5);
        let mut second = score_row(2, 1, 200, 200, 200, false, "accuracy");
        second.experiment_id = Some(E2.parse().expect("experiment"));
        let plain = score_row(3, 2, 300, 300, 300, false, "length");
        vec![first, second, plain]
    }

    #[tokio::test]
    async fn scores_of_one_experiment() {
        let ctx = context_of(&[], &scores());
        let found = experiment_scores(&ctx, E1, 0, TIMEOUT)
            .await
            .expect("scores");
        assert_eq!(
            found,
            [ExperimentScore {
                trace_id: trace_hex(0),
                dataset_item_id: Some(ITEM.to_owned()),
                name: "accuracy".to_owned(),
                data_type: "numeric".to_owned(),
                value_num: Some(0.5),
                value_str: None,
            }]
        );
    }

    #[tokio::test]
    async fn deleted_and_replaced_scores_are_not_returned() {
        let mut first = score_row(1, 0, 100, 100, 100, false, "accuracy");
        first.experiment_id = Some(E1.parse().expect("experiment"));
        let mut deleted = first.clone();
        deleted.updated_at = 200;
        deleted.deleted = true;
        let ctx = context_of(&[], &[first, deleted]);
        let found = experiment_scores(&ctx, E1, 0, TIMEOUT)
            .await
            .expect("scores");
        assert_eq!(found, []);
    }

    #[tokio::test]
    async fn categorical_scores_carry_the_label() {
        let mut score = score_row(1, 0, 100, 100, 100, false, "quality");
        score.experiment_id = Some(E1.parse().expect("experiment"));
        score.value = ScoreValue::Categorical {
            label: "good".to_owned(),
            value: Some(1.0),
        };
        let ctx = context_of(&[], &[score]);
        let found = experiment_scores(&ctx, E1, 0, TIMEOUT)
            .await
            .expect("scores");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].data_type, "categorical");
        assert_eq!(found[0].value_str.as_deref(), Some("good"));
        assert_eq!(found[0].value_num, Some(1.0));
    }

    #[tokio::test]
    async fn traces_of_one_experiment() {
        let ctx = context_of(&spans(), &[]);
        let found = experiment_traces(&ctx, E1, 0, TIMEOUT)
            .await
            .expect("traces");
        assert_eq!(
            found,
            [ExperimentTrace {
                trace_id: trace_hex(0),
                cost_total: Some(0.25),
                duration_ns: 150,
                error_count: 1,
            }]
        );
    }

    #[tokio::test]
    async fn bad_id_is_invalid_request() {
        let ctx = context_of(&spans(), &scores());
        match experiment_scores(&ctx, "nope", 0, TIMEOUT).await {
            Err(RunError::InvalidRequest(message)) => {
                assert_eq!(message, "invalid experiment id");
            }
            other => panic!("expected an invalid experiment id, got {other:?}"),
        }
        match experiment_traces(&ctx, "nope", 0, TIMEOUT).await {
            Err(RunError::InvalidRequest(message)) => {
                assert_eq!(message, "invalid experiment id");
            }
            other => panic!("expected an invalid experiment id, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_match_is_empty() {
        let ctx = context_of(&spans(), &scores());
        let unknown = "01900000-0000-7000-8000-0000000000ff";
        assert_eq!(
            experiment_scores(&ctx, unknown, 0, TIMEOUT)
                .await
                .expect("scores"),
            []
        );
        assert_eq!(
            experiment_traces(&ctx, unknown, 0, TIMEOUT)
                .await
                .expect("traces"),
            []
        );
    }

    #[tokio::test]
    async fn since_bounds_both_reads() {
        let ctx = context_of(&spans(), &scores());
        // The score of E1 is created at 100 and its trace starts at 1_000.
        assert_eq!(
            experiment_scores(&ctx, E1, 101, TIMEOUT)
                .await
                .expect("late scores"),
            []
        );
        assert_eq!(
            experiment_traces(&ctx, E1, 1_001, TIMEOUT)
                .await
                .expect("late traces"),
            []
        );
        assert_eq!(
            experiment_scores(&ctx, E1, 100, TIMEOUT)
                .await
                .expect("scores")
                .len(),
            1
        );
        assert_eq!(
            experiment_traces(&ctx, E1, 1_000, TIMEOUT)
                .await
                .expect("traces")
                .len(),
            1
        );
    }
}
