//! Per-version metrics of a registry prompt (spec 05, D-V-17).
//!
//! The rows are the linked `llm` spans of one prompt name in the window: spans with
//! `prompt_name` and `prompt_version` set (D-V-15), with the default exclusions applied
//! through the same trace-id semi-join as alerts. Span aggregates group them by version.
//! A span score counts for the version of its span; a trace score counts for every
//! version that the trace's linked spans carry.

use std::sync::Arc;
use std::time::Duration;

use arrow::datatypes::DataType;
use arrow::record_batch::RecordBatch;
use cetra_core::enums::{ScoreDataType, ScoreTargetKind, SpanKind, SpanStatus};
use cetra_core::schema::{scores, spans};
use datafusion::common::ScalarValue;
use datafusion::functions_aggregate::expr_fn::{avg, count, count_distinct, sum};
use datafusion::logical_expr::expr_fn::when;
use datafusion::logical_expr::{Expr, JoinType, LogicalPlan, cast, col, in_subquery, lit};
use datafusion::prelude::DataFrame;

use super::alerts::{DAY_NS, trace_id_plan};
use super::columns::{f64_at, i64_at, internal, percentile, require, string_at};
use crate::query_error::{RunError, classify};
use crate::views::{QueryContext, TimeWindow, exact_cost_sum};

/// Most versions one request may name.
pub const MAX_PROMPT_VERSIONS: usize = 50;

const SPAN_COUNT: &str = "span_count";
const TRACE_COUNT: &str = "trace_count";
const ERROR_COUNT: &str = "error_count";
const COST_SUM: &str = "cost_sum";
const COST_AVG: &str = "cost_avg";
const LATENCY_P50: &str = "latency_p50";
const LATENCY_P95: &str = "latency_p95";
const INPUT_AVG: &str = "input_tokens_avg";
const OUTPUT_AVG: &str = "output_tokens_avg";
const VERSION: &str = "version";
const MEAN: &str = "mean";
const COUNT: &str = "count";
/// Linked spans as `(trace, span, version)`.
const PAIR_TRACE: &str = "pair_trace";
const PAIR_SPAN: &str = "pair_span";
const PAIR_VERSION: &str = "pair_version";
/// Linked spans as `(trace, version)`.
const TRACE_VERSION_TRACE: &str = "trace_version_trace";
const TRACE_VERSION: &str = "trace_version";

/// What to aggregate (D-V-17).
#[derive(Debug, Clone)]
pub struct PromptMetricsRequest<'a> {
    /// The prompt name, compared with `spans.prompt_name` exactly.
    pub prompt: &'a str,
    /// Span start time window.
    pub window: TimeWindow,
    /// Only these versions, when non-empty (at most [`MAX_PROMPT_VERSIONS`]).
    pub versions: &'a [i64],
    /// Whole-read timeout.
    pub timeout: Duration,
}

/// One version's numbers. Averages and percentiles are `None` when no span had the value.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptVersionMetrics {
    /// The version (≥ 1).
    pub version: i64,
    /// Linked `llm` spans.
    pub span_count: i64,
    /// Distinct traces of those spans.
    pub trace_count: i64,
    /// Spans with status `error`.
    pub error_count: i64,
    /// Exact sum of `cost_total` (USD).
    pub cost_total: f64,
    /// `cost_total` average over spans that have one.
    pub cost_avg: Option<f64>,
    /// Approximate median of `duration_ns`, in nanoseconds.
    pub latency_p50_ns: Option<i64>,
    /// Approximate 95th percentile of `duration_ns`, in nanoseconds.
    pub latency_p95_ns: Option<i64>,
    /// Average `input_tokens` over spans that have them.
    pub input_tokens_avg: Option<f64>,
    /// Average `output_tokens` over spans that have them.
    pub output_tokens_avg: Option<f64>,
    /// Numeric and boolean scores, by name (ascending).
    pub scores: Vec<PromptScoreMean>,
}

/// One score name's mean for a version.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptScoreMean {
    /// Score name.
    pub name: String,
    /// Average of the score values.
    pub mean: f64,
    /// Number of scores averaged.
    pub count: i64,
}

/// Per-version metrics, newest version first.
///
/// # Errors
///
/// [`RunError::InvalidRequest`] when more than [`MAX_PROMPT_VERSIONS`] versions are named,
/// [`RunError::Timeout`] when the read exceeds `request.timeout`, and the other
/// [`RunError`] variants from planning and running.
pub async fn prompt_metrics(
    ctx: &QueryContext,
    request: PromptMetricsRequest<'_>,
) -> Result<Vec<PromptVersionMetrics>, RunError> {
    let timeout = request.timeout;
    match tokio::time::timeout(timeout, compute(ctx, request)).await {
        Err(_) => Err(RunError::Timeout),
        Ok(result) => result,
    }
}

async fn compute(
    ctx: &QueryContext,
    request: PromptMetricsRequest<'_>,
) -> Result<Vec<PromptVersionMetrics>, RunError> {
    if request.versions.len() > MAX_PROMPT_VERSIONS {
        return Err(RunError::InvalidRequest(format!(
            "at most {MAX_PROMPT_VERSIONS} versions"
        )));
    }
    // The traces the linked spans may belong to: the default exclusions, and traces that
    // started up to a day before the window and end at or after its start.
    let ids = trace_id_plan(ctx, request.window, DAY_NS, true, None, false).await?;
    let mut versions = span_aggregates(ctx, &request, &ids).await?;
    for (version, mean) in score_means(ctx, &request, &ids).await? {
        if let Some(entry) = versions.iter_mut().find(|entry| entry.version == version) {
            entry.scores.push(mean);
        }
    }
    for entry in &mut versions {
        entry.scores.sort_by(|a, b| a.name.cmp(&b.name));
    }
    versions.sort_by_key(|a| std::cmp::Reverse(a.version));
    Ok(versions)
}

/// The linked `llm` spans of the request: the prompt name, a version, the window, the
/// requested versions, and the trace ids `ids` (default exclusions).
async fn linked_spans(
    ctx: &QueryContext,
    request: &PromptMetricsRequest<'_>,
    ids: &LogicalPlan,
) -> Result<DataFrame, RunError> {
    let mut predicate = request
        .window
        .predicate(spans::START_TIME)
        .and(col(spans::SPAN_KIND).eq(lit(SpanKind::Llm.as_str())))
        .and(col(spans::PROMPT_NAME).eq(lit(request.prompt)))
        .and(col(spans::PROMPT_VERSION).is_not_null())
        .and(in_subquery(col(spans::TRACE_ID), Arc::new(ids.clone())));
    if !request.versions.is_empty() {
        let versions = request
            .versions
            .iter()
            .map(|version| lit(*version))
            .collect();
        predicate = predicate.and(col(spans::PROMPT_VERSION).in_list(versions, false));
    }
    ctx.session()
        .table("spans")
        .await
        .map_err(classify)?
        .filter(predicate)
        .map_err(classify)
}

/// Span aggregates per version, with no score yet. A version with no linked span in the
/// window is absent.
async fn span_aggregates(
    ctx: &QueryContext,
    request: &PromptMetricsRequest<'_>,
    ids: &LogicalPlan,
) -> Result<Vec<PromptVersionMetrics>, RunError> {
    let batches = linked_spans(ctx, request, ids)
        .await?
        .aggregate(
            vec![col(spans::PROMPT_VERSION)],
            vec![
                count(lit(1_i64)).alias(SPAN_COUNT),
                count_distinct(col(spans::TRACE_ID)).alias(TRACE_COUNT),
                sum(error_flag()?).alias(ERROR_COUNT),
                exact_cost_sum(col(spans::COST_TOTAL)).alias(COST_SUM),
                avg(col(spans::COST_TOTAL)).alias(COST_AVG),
                percentile(spans::DURATION_NS, 0.5).alias(LATENCY_P50),
                percentile(spans::DURATION_NS, 0.95).alias(LATENCY_P95),
                avg(col(spans::INPUT_TOKENS)).alias(INPUT_AVG),
                avg(col(spans::OUTPUT_TOKENS)).alias(OUTPUT_AVG),
            ],
        )
        .map_err(classify)?
        .collect()
        .await
        .map_err(|error| ctx.fail(error))?;
    let mut found = Vec::new();
    for batch in &batches {
        for row in 0..batch.num_rows() {
            found.push(PromptVersionMetrics {
                version: i64_at(require(batch, spans::PROMPT_VERSION)?, row)?,
                span_count: i64_at(require(batch, SPAN_COUNT)?, row)?,
                trace_count: i64_at(require(batch, TRACE_COUNT)?, row)?,
                error_count: i64_at(require(batch, ERROR_COUNT)?, row)?,
                cost_total: f64_at(require(batch, COST_SUM)?, row)?.unwrap_or(0.0),
                cost_avg: f64_at(require(batch, COST_AVG)?, row)?,
                latency_p50_ns: nanos_at(batch, LATENCY_P50, row)?,
                latency_p95_ns: nanos_at(batch, LATENCY_P95, row)?,
                input_tokens_avg: f64_at(require(batch, INPUT_AVG)?, row)?,
                output_tokens_avg: f64_at(require(batch, OUTPUT_AVG)?, row)?,
                scores: Vec::new(),
            });
        }
    }
    Ok(found)
}

/// `1` for a linked span with status `error`, `0` otherwise.
fn error_flag() -> Result<Expr, RunError> {
    when(
        col(spans::STATUS).eq(lit(SpanStatus::Error.as_str())),
        lit(1_i64),
    )
    .otherwise(lit(0_i64))
    .map_err(classify)
}

/// A percentile in nanoseconds, rounded. `None` when the value is null.
fn nanos_at(batch: &RecordBatch, name: &str, row: usize) -> Result<Option<i64>, RunError> {
    Ok(f64_at(require(batch, name)?, row)?.map(|ns| ns.round() as i64))
}

/// The mean of the numeric and boolean scores attributed to each version, as
/// `(version, mean)`. A span score is attributed to its span's version; a trace score to
/// every version of the trace's linked spans. Session scores are ignored.
async fn score_means(
    ctx: &QueryContext,
    request: &PromptMetricsRequest<'_>,
    ids: &LogicalPlan,
) -> Result<Vec<(i64, PromptScoreMean)>, RunError> {
    let pairs = linked_spans(ctx, request, ids)
        .await?
        .select(vec![
            col(spans::TRACE_ID).alias(PAIR_TRACE),
            col(spans::SPAN_ID).alias(PAIR_SPAN),
            col(spans::PROMPT_VERSION).alias(PAIR_VERSION),
        ])
        .map_err(classify)?
        .distinct()
        .map_err(classify)?;
    let trace_versions = linked_spans(ctx, request, ids)
        .await?
        .select(vec![
            col(spans::TRACE_ID).alias(TRACE_VERSION_TRACE),
            col(spans::PROMPT_VERSION).alias(TRACE_VERSION),
        ])
        .map_err(classify)?
        .distinct()
        .map_err(classify)?;

    let span_scores = live_scores(ctx, request.window, ScoreTargetKind::Span)
        .await?
        .join(
            pairs,
            JoinType::Inner,
            &[scores::TRACE_ID, scores::SPAN_ID],
            &[PAIR_TRACE, PAIR_SPAN],
            None,
        )
        .map_err(classify)?
        .select(versioned_score_columns(PAIR_VERSION))
        .map_err(classify)?;
    let trace_scores = live_scores(ctx, request.window, ScoreTargetKind::Trace)
        .await?
        .join(
            trace_versions,
            JoinType::Inner,
            &[scores::TRACE_ID],
            &[TRACE_VERSION_TRACE],
            None,
        )
        .map_err(classify)?
        .select(versioned_score_columns(TRACE_VERSION))
        .map_err(classify)?;

    let batches = span_scores
        .union(trace_scores)
        .map_err(classify)?
        .aggregate(
            vec![col(VERSION), col(scores::NAME)],
            vec![
                avg(col(scores::VALUE_NUM)).alias(MEAN),
                count(lit(1_i64)).alias(COUNT),
            ],
        )
        .map_err(classify)?
        .collect()
        .await
        .map_err(|error| ctx.fail(error))?;
    let mut found = Vec::new();
    for batch in &batches {
        for row in 0..batch.num_rows() {
            let Some(mean) = f64_at(require(batch, MEAN)?, row)? else {
                continue;
            };
            let name = string_at(require(batch, scores::NAME)?, row)?
                .ok_or_else(|| internal("score name is null".to_owned()))?;
            found.push((
                i64_at(require(batch, VERSION)?, row)?,
                PromptScoreMean {
                    name,
                    mean,
                    count: i64_at(require(batch, COUNT)?, row)?,
                },
            ));
        }
    }
    Ok(found)
}

/// The output columns of a score branch: `version` (Int64, from `version_column`), the
/// score `name`, and `value_num` (Float64). Both branches of the union use this list.
fn versioned_score_columns(version_column: &str) -> Vec<Expr> {
    vec![
        cast(col(version_column), DataType::Int64).alias(VERSION),
        col(scores::NAME),
        cast(col(scores::VALUE_NUM), DataType::Float64).alias(scores::VALUE_NUM),
    ]
}

/// The live numeric and boolean scores with a value, of one target kind, created at or
/// after the window start. Scores are not bounded above: a score may come after its span.
async fn live_scores(
    ctx: &QueryContext,
    window: TimeWindow,
    target: ScoreTargetKind,
) -> Result<DataFrame, RunError> {
    let from = Expr::Literal(
        ScalarValue::TimestampNanosecond(Some(window.from()), Some("UTC".into())),
        None,
    );
    ctx.session()
        .table("scores")
        .await
        .map_err(classify)?
        .filter(
            col(scores::TARGET)
                .eq(lit(target.as_str()))
                .and(col(scores::DATA_TYPE).in_list(
                    vec![
                        lit(ScoreDataType::Numeric.as_str()),
                        lit(ScoreDataType::Boolean.as_str()),
                    ],
                    false,
                ))
                .and(col(scores::VALUE_NUM).is_not_null())
                .and(col(scores::CREATED_AT).gt_eq(from)),
        )
        .map_err(classify)
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use cetra_core::names::environment;
    use cetra_core::score::{Score, ScoreTarget, ScoreValue};
    use cetra_core::span::Span;
    use datafusion::execution::runtime_env::RuntimeEnv;

    use super::*;
    use crate::test_support::{live_scores_table, score_row, span_row, spans_table, traces_table};
    use crate::views::ProjectQuery;

    const T0: i64 = 1_700_000_000_000_000_000;
    const MIN: i64 = 60_000_000_000;
    const MS: i64 = 1_000_000;

    fn context(spans: &[Span], scores: &[Score]) -> QueryContext {
        let query = ProjectQuery {
            spans: spans_table(spans),
            traces: traces_table(spans),
            scores: live_scores_table(scores),
            runtime: Arc::new(RuntimeEnv::default()),
            target_partitions: NonZeroUsize::new(2).expect("two partitions"),
            optimizer_rules: Vec::new(),
            suspects: crate::verify::Suspects::detached(),
            visible_from: None,
        };
        QueryContext::new(query, None).expect("context")
    }

    fn window() -> TimeWindow {
        TimeWindow::new(T0, T0 + 10 * MIN).expect("window")
    }

    fn request<'a>(prompt: &'a str, versions: &'a [i64]) -> PromptMetricsRequest<'a> {
        PromptMetricsRequest {
            prompt,
            window: window(),
            versions,
            timeout: Duration::from_secs(30),
        }
    }

    /// The root span of `trace`, starting at `start`.
    fn root(trace: u16, start: i64) -> Span {
        span_row(trace, 0, start, 1)
    }

    /// The id of the root span of `trace`, which every `llm` span below names as parent.
    fn root_id(trace: u16) -> cetra_core::ids::SpanId {
        span_row(trace, 0, 0, 1).span_id
    }

    /// An `llm` span of `trace` at `index` that used `name@version`, costing `cost` USD
    /// and lasting `duration` ns.
    fn llm(
        trace: u16,
        index: u8,
        start: i64,
        name: &str,
        version: i64,
        cost: f64,
        duration: i64,
    ) -> Span {
        let mut span = span_row(trace, index, start, 1);
        span.span_kind = SpanKind::Llm;
        span.parent_span_id = Some(root_id(trace));
        span.end_time = start + duration;
        span.prompt_name = Some(name.to_owned());
        span.prompt_version = Some(version);
        span.cost_total = Some(cost);
        span
    }

    /// A trace score created inside the window.
    fn trace_score(id: u128, trace: u16, name: &str, value: ScoreValue) -> Score {
        let mut score = score_row(
            id,
            trace,
            T0 + 2 * MIN,
            T0 + 2 * MIN,
            T0 + 2 * MIN,
            false,
            name,
        );
        score.value = value;
        score
    }

    /// A score on `span`, created inside the window.
    fn span_score(id: u128, span: &Span, name: &str, value: ScoreValue) -> Score {
        let mut score = trace_score(id, 0, name, value);
        score.target = ScoreTarget::Span {
            trace_id: span.trace_id,
            span_id: span.span_id,
        };
        score
    }

    /// Prompt `p` at versions 1 and 2 and prompt `q`, over traces A to G, each described
    /// where it is built.
    fn fixture() -> (Vec<Span>, Vec<Score>) {
        let mut spans = Vec::new();

        // A: two llm spans on p@1; the second one errors.
        spans.push(root(0, T0 + MIN));
        let mut a_first = llm(0, 1, T0 + MIN, "p", 1, 0.01, 100 * MS);
        a_first.input_tokens = Some(10);
        a_first.output_tokens = Some(5);
        let a_score = trace_score(1, 0, "quality", ScoreValue::Numeric(1.0));
        spans.push(a_first);
        let mut a_second = llm(0, 2, T0 + MIN, "p", 1, 0.03, 300 * MS);
        a_second.status = SpanStatus::Error;
        spans.push(a_second);

        // B: one llm span on p@2, with a score on the span and one on the trace.
        spans.push(root(1, T0 + MIN));
        let b_llm = llm(1, 1, T0 + MIN, "p", 2, 0.02, 200 * MS);
        let b_span_score = span_score(2, &b_llm, "quality", ScoreValue::Numeric(0.5));
        spans.push(b_llm);
        let b_trace_score = trace_score(3, 1, "quality", ScoreValue::Numeric(0.0));

        // C: llm spans on p@1 and p@2, with a boolean trace score.
        spans.push(root(2, T0 + MIN));
        spans.push(llm(2, 1, T0 + MIN, "p", 1, 0.05, 100 * MS));
        spans.push(llm(2, 2, T0 + MIN, "p", 2, 0.07, 200 * MS));
        let c_score = trace_score(4, 2, "helpful", ScoreValue::Boolean(true));

        // D: a chain span on p@1, which is not an llm span.
        spans.push(root(3, T0 + MIN));
        let mut chain = llm(3, 1, T0 + MIN, "p", 1, 0.5, 100 * MS);
        chain.span_kind = SpanKind::Chain;
        spans.push(chain);

        // E: an llm span on p@1 in the evaluation environment, which is excluded, with a
        // trace score that must not count either.
        let mut e_root = root(4, T0 + MIN);
        e_root.environment = Some(environment::EVAL.to_owned());
        spans.push(e_root);
        let mut e_llm = llm(4, 1, T0 + MIN, "p", 1, 0.5, 100 * MS);
        e_llm.environment = Some(environment::EVAL.to_owned());
        spans.push(e_llm);
        let e_trace_score = trace_score(5, 4, "quality", ScoreValue::Numeric(0.0));

        // F: an llm span on another prompt.
        spans.push(root(5, T0 + MIN));
        spans.push(llm(5, 1, T0 + MIN, "q", 1, 0.5, 100 * MS));

        // G: an llm span on p@1 that starts before the window; its trace ends inside it.
        let mut g_root = root(6, T0 - MIN);
        g_root.end_time = T0 + 2 * MIN;
        spans.push(g_root);
        spans.push(llm(6, 1, T0 - MIN, "p", 1, 0.5, 100 * MS));

        let scores = vec![a_score, b_span_score, b_trace_score, c_score, e_trace_score];
        (spans, scores)
    }

    fn assert_close(actual: Option<f64>, expected: f64, tolerance: f64) {
        let actual = actual.expect("a value");
        assert!(
            (actual - expected).abs() <= tolerance,
            "{actual} is not {expected} within {tolerance}"
        );
    }

    fn score_named<'a>(metrics: &'a PromptVersionMetrics, name: &str) -> &'a PromptScoreMean {
        metrics
            .scores
            .iter()
            .find(|score| score.name == name)
            .unwrap_or_else(|| panic!("no score {name} on version {}", metrics.version))
    }

    #[tokio::test]
    async fn aggregates_per_version() {
        let (spans, scores) = fixture();
        let ctx = context(&spans, &scores);
        let rows = prompt_metrics(&ctx, request("p", &[]))
            .await
            .expect("metrics");
        let versions: Vec<i64> = rows.iter().map(|row| row.version).collect();
        assert_eq!(versions, vec![2, 1], "newest first");

        let v2 = &rows[0];
        assert_eq!(v2.span_count, 2);
        assert_eq!(v2.trace_count, 2);
        assert_eq!(v2.error_count, 0);
        assert_close(Some(v2.cost_total), 0.09, 1e-9);
        assert_close(v2.cost_avg, 0.045, 1e-9);
        assert_eq!(v2.input_tokens_avg, None);
        assert_eq!(v2.output_tokens_avg, None);

        let v1 = &rows[1];
        assert_eq!(v1.span_count, 3);
        assert_eq!(v1.trace_count, 2);
        assert_eq!(v1.error_count, 1);
        assert_close(Some(v1.cost_total), 0.09, 1e-9);
        assert_close(v1.cost_avg, 0.03, 1e-9);
        assert_close(v1.input_tokens_avg, 10.0, 1e-9);
        assert_close(v1.output_tokens_avg, 5.0, 1e-9);
        let p50 = v1.latency_p50_ns.expect("a median");
        let p95 = v1.latency_p95_ns.expect("a p95");
        assert!(p50 <= p95, "p50 {p50} above p95 {p95}");
    }

    #[tokio::test]
    async fn scores_per_version() {
        let (spans, scores) = fixture();
        let ctx = context(&spans, &scores);
        let rows = prompt_metrics(&ctx, request("p", &[]))
            .await
            .expect("metrics");
        let v2 = &rows[0];
        let names: Vec<&str> = v2.scores.iter().map(|score| score.name.as_str()).collect();
        assert_eq!(names, vec!["helpful", "quality"], "scores by name");
        let helpful = score_named(v2, "helpful");
        assert_close(Some(helpful.mean), 1.0, 1e-9);
        assert_eq!(helpful.count, 1);
        let quality = score_named(v2, "quality");
        assert_close(Some(quality.mean), 0.25, 1e-9);
        assert_eq!(quality.count, 2);

        let v1 = &rows[1];
        let helpful = score_named(v1, "helpful");
        assert_close(Some(helpful.mean), 1.0, 1e-9);
        assert_eq!(helpful.count, 1);
        // Trace A's score; trace E's score is excluded with its trace.
        let quality = score_named(v1, "quality");
        assert_close(Some(quality.mean), 1.0, 1e-9);
        assert_eq!(quality.count, 1);
    }

    #[tokio::test]
    async fn version_filter() {
        let (spans, scores) = fixture();
        let ctx = context(&spans, &scores);
        let rows = prompt_metrics(&ctx, request("p", &[2]))
            .await
            .expect("metrics");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].version, 2);
        assert_eq!(rows[0].span_count, 2);

        let too_many = vec![1_i64; MAX_PROMPT_VERSIONS + 1];
        let result = prompt_metrics(&ctx, request("p", &too_many)).await;
        assert!(
            matches!(result, Err(RunError::InvalidRequest(_))),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn percentiles_are_close() {
        let mut spans = Vec::new();
        for k in 1..=100_i64 {
            let trace = u16::try_from(k).expect("small");
            spans.push(root(trace, T0 + MIN));
            spans.push(llm(trace, 1, T0 + MIN, "p", 1, 0.0, k * MS));
        }
        let ctx = context(&spans, &[]);
        let rows = prompt_metrics(&ctx, request("p", &[]))
            .await
            .expect("metrics");
        assert_eq!(rows.len(), 1);
        let p50 = rows[0].latency_p50_ns.expect("a median") as f64 / 1e6;
        let p95 = rows[0].latency_p95_ns.expect("a p95") as f64 / 1e6;
        assert!((45.0..=55.0).contains(&p50), "p50 {p50} ms");
        assert!((90.0..=100.0).contains(&p95), "p95 {p95} ms");
    }

    #[tokio::test]
    async fn unknown_prompt_is_empty() {
        let (spans, scores) = fixture();
        let ctx = context(&spans, &scores);
        let rows = prompt_metrics(&ctx, request("nope", &[]))
            .await
            .expect("metrics");
        assert!(rows.is_empty());
    }
}
