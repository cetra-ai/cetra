//! Native query engine implementation of `prompt_metrics` (D-V-17).

use std::sync::Arc;
use std::time::Duration;

use cetra_column::{Batch, DataType, Field, ScalarValue, Schema};
use cetra_core::enums::{ScoreDataType, ScoreTargetKind, SpanKind, SpanStatus};
use cetra_core::schema::{scores, spans};
use cetra_query::exec::hash_agg::execute_hash_agg_streams;
use cetra_query::exec::hash_join::{HashJoinSpec, JoinType, execute_hash_join, join_output_schema};
use cetra_query::exec::streaming::ProjectOp;
use cetra_query::exec::{
    AggExpr, AggFunc, CollectSink, ControlFlow, ExecContext, HashAgg, HashAggSpec, PartitionStream,
    PushSink, ScanSource, ScanSpec, TimeRangeHint, run_pipeline as exec_run_pipeline,
};
use cetra_query::{BinaryOp, Expr, QueryError, UnaryOp};

use crate::query_error::RunError;
use crate::reads::ListScope;
use crate::reads::columns::{
    active_rows, raw_decimal_to_f64_at, raw_f64_at, raw_i64_at, raw_string_at, require_col,
};
use crate::reads::context::{
    EngineContext, OperatorStream, run_err_to_query, scores_raw_schema, spans_raw_schema,
};
use crate::reads::scope::{StreamSet, TraceSemiJoinParams, trace_id_semi_join};
use crate::views::{QueryContext, TimeWindow};

/// Most versions one request may name.
pub const MAX_PROMPT_VERSIONS: usize = 50;

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

const DAY_NS: i64 = 86_400_000_000_000;
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
const PAIR_TRACE: &str = "__pair_trace";
const PAIR_SPAN: &str = "__pair_span";
const PAIR_VERSION: &str = "__pair_version";
const TRACE_VERSION_TRACE: &str = "__tv_trace";
const TRACE_VERSION: &str = "__tv_version";

/// Per-version metrics of a registry prompt, newest version first.
pub async fn prompt_metrics(
    ctx: &QueryContext,
    request: PromptMetricsRequest<'_>,
) -> Result<Vec<PromptVersionMetrics>, RunError> {
    let ctx = ctx.engine();
    if request.versions.len() > MAX_PROMPT_VERSIONS {
        return Err(RunError::InvalidRequest(format!(
            "at most {MAX_PROMPT_VERSIONS} versions"
        )));
    }
    let prompt = request.prompt.to_owned();
    let window = request.window;
    let versions_filter = request.versions.to_vec();

    let ctx_c = ctx.clone();
    let (metrics, _) = ctx
        .run_with_ctx(request.timeout, move |exec_ctx| {
            // 1. Shared 1-day lookback trace-id semi-join (ends_after_from = true).
            let scope = ListScope {
                window: Some(window),
                include_eval: false,
                include_experiments: false,
            };
            let Some(semi) = trace_id_semi_join(
                &ctx_c,
                exec_ctx,
                &TraceSemiJoinParams {
                    lookback_ns: DAY_NS,
                    ends_after_from: true,
                    scope,
                    references_environment: false,
                    dashboard_filter: None,
                },
            )
            .map_err(run_err_to_query)?
            else {
                return Err(QueryError::internal(
                    "prompt_metrics trace_id_semi_join unexpectedly returned None",
                ));
            };
            if semi.streams.is_empty() {
                return Ok(Vec::new());
            }

            // 2. Linked `llm` spans in `window`, semi-joined with `semi` on `trace_id`.
            let spans_raw = spans_raw_schema().map_err(run_err_to_query)?;
            let mut min_ns = window.from();
            if let Some(vf) = ctx_c.visible_from {
                min_ns = min_ns.max(vf);
            }
            let max_ns = window.to().checked_sub(1);

            let start_col = col_expr(&spans_raw, spans::START_TIME)?;
            let mut filters = vec![
                Expr::Binary {
                    left: Box::new(start_col.clone()),
                    op: BinaryOp::GtEq,
                    right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                        value: window.from(),
                        utc: true,
                    })),
                    data_type: DataType::Bool,
                },
                Expr::Binary {
                    left: Box::new(start_col.clone()),
                    op: BinaryOp::Lt,
                    right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                        value: window.to(),
                        utc: true,
                    })),
                    data_type: DataType::Bool,
                },
                Expr::Binary {
                    left: Box::new(col_expr(&spans_raw, spans::SPAN_KIND)?),
                    op: BinaryOp::Eq,
                    right: Box::new(Expr::Literal(ScalarValue::Utf8(
                        SpanKind::Llm.as_str().to_owned(),
                    ))),
                    data_type: DataType::Bool,
                },
                Expr::Binary {
                    left: Box::new(col_expr(&spans_raw, spans::PROMPT_NAME)?),
                    op: BinaryOp::Eq,
                    right: Box::new(Expr::Literal(ScalarValue::Utf8(prompt))),
                    data_type: DataType::Bool,
                },
                Expr::Unary {
                    op: UnaryOp::IsNotNull,
                    expr: Box::new(col_expr(&spans_raw, spans::PROMPT_VERSION)?),
                    data_type: DataType::Bool,
                },
            ];
            if let Some(vf) = ctx_c.visible_from {
                filters.push(Expr::Binary {
                    left: Box::new(start_col),
                    op: BinaryOp::GtEq,
                    right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                        value: vf,
                        utc: true,
                    })),
                    data_type: DataType::Bool,
                });
            }
            if !versions_filter.is_empty() {
                filters.push(Expr::InList {
                    expr: Box::new(col_expr(&spans_raw, spans::PROMPT_VERSION)?),
                    list: versions_filter
                        .iter()
                        .map(|v| Expr::Literal(ScalarValue::I64(*v)))
                        .collect(),
                    negated: false,
                    redacted: false,
                });
            }

            let needed = [
                spans::TRACE_ID,
                spans::SPAN_ID,
                spans::PROMPT_VERSION,
                spans::STATUS,
                spans::COST_TOTAL,
                spans::DURATION_NS,
                spans::INPUT_TOKENS,
                spans::OUTPUT_TOKENS,
            ];
            let mut proj_indices = Vec::with_capacity(needed.len());
            for name in needed {
                let idx = spans_raw.index_of(name).ok_or_else(|| {
                    QueryError::internal(format!("missing {name} in spans schema"))
                })?;
                proj_indices.push(idx);
            }

            let span_spec = ScanSpec {
                projection: Some(proj_indices),
                map_keys: Vec::new(),
                time_range: TimeRangeHint {
                    min_ns: Some(min_ns),
                    max_ns,
                    around_ns: None,
                },
                id_lookups: Vec::new(),
                filters,
                substrings: Vec::new(),
                sort_hints: Vec::new(),
                limit: None,
            };

            let span_proj_schema = ctx_c.spans.projected_schema(&span_spec)?;
            let span_streams = ctx_c.spans.open_partitions(&span_spec, exec_ctx)?;
            if span_streams.is_empty() {
                return Ok(Vec::new());
            }

            let joined_spans = execute_hash_join(
                span_streams,
                Arc::clone(&span_proj_schema),
                semi.streams,
                Arc::clone(&semi.schema),
                HashJoinSpec {
                    join_type: JoinType::LeftSemi,
                    left_keys: vec![col_expr(&span_proj_schema, spans::TRACE_ID)?],
                    right_keys: vec![col_expr(&semi.schema, spans::TRACE_ID)?],
                    null_equals_null: false,
                    residual_filter: None,
                },
                exec_ctx,
            )?;

            // 3. Single pass over `joined_spans` computing:
            //    - span aggregates per prompt_version
            //    - distinct (trace_id, span_id, prompt_version)
            //    - distinct (trace_id, prompt_version)
            let span_agg_spec = build_span_agg_spec(&span_proj_schema)?;
            let pairs_spec = HashAggSpec {
                group_by: vec![
                    (
                        PAIR_TRACE.to_owned(),
                        col_expr(&span_proj_schema, spans::TRACE_ID)?,
                    ),
                    (
                        PAIR_SPAN.to_owned(),
                        col_expr(&span_proj_schema, spans::SPAN_ID)?,
                    ),
                    (
                        PAIR_VERSION.to_owned(),
                        col_expr(&span_proj_schema, spans::PROMPT_VERSION)?,
                    ),
                ],
                aggs: Vec::new(),
            };
            let tv_spec = HashAggSpec {
                group_by: vec![
                    (
                        TRACE_VERSION_TRACE.to_owned(),
                        col_expr(&span_proj_schema, spans::TRACE_ID)?,
                    ),
                    (
                        TRACE_VERSION.to_owned(),
                        col_expr(&span_proj_schema, spans::PROMPT_VERSION)?,
                    ),
                ],
                aggs: Vec::new(),
            };

            let span_agg_c = span_agg_spec.clone();
            let pairs_c = pairs_spec.clone();
            let tv_c = tv_spec.clone();
            let schema_c = Arc::clone(&span_proj_schema);
            let ctx_sink = exec_ctx.clone();
            let fan_sinks = exec_run_pipeline(
                exec_ctx,
                vec![joined_spans],
                |_| Ok(Vec::new()),
                move |_| {
                    Ok(PromptFanSink {
                        span_agg: HashAgg::new(
                            span_agg_c.clone(),
                            Arc::clone(&schema_c),
                            ctx_sink.clone(),
                        )?,
                        pairs: HashAgg::new(
                            pairs_c.clone(),
                            Arc::clone(&schema_c),
                            ctx_sink.clone(),
                        )?,
                        trace_versions: HashAgg::new(
                            tv_c.clone(),
                            Arc::clone(&schema_c),
                            ctx_sink.clone(),
                        )?,
                    })
                },
            )?;

            let mut agg_sinks = Vec::with_capacity(fan_sinks.len());
            let mut pair_sinks = Vec::with_capacity(fan_sinks.len());
            let mut tv_sinks = Vec::with_capacity(fan_sinks.len());
            for s in fan_sinks {
                agg_sinks.push(s.span_agg);
                pair_sinks.push(s.pairs);
                tv_sinks.push(s.trace_versions);
            }

            let agg_batches =
                HashAgg::merge_sinks(agg_sinks, &span_agg_spec, &span_proj_schema, exec_ctx)?;
            let mut versions = decode_version_metrics(&agg_batches).map_err(run_err_to_query)?;
            if versions.is_empty() {
                return Ok(Vec::new());
            }

            let pairs_stream = HashAgg::combine_sinks_to_stream(
                pair_sinks,
                &pairs_spec,
                &span_proj_schema,
                exec_ctx,
            )?;
            let tv_stream =
                HashAgg::combine_sinks_to_stream(tv_sinks, &tv_spec, &span_proj_schema, exec_ctx)?;

            // 4. Score means across deduplicated span pairs and trace versions.
            let score_means =
                compute_score_means(&ctx_c, exec_ctx, window, pairs_stream, tv_stream)?;
            for (version, mean) in score_means {
                if let Some(entry) = versions.iter_mut().find(|v| v.version == version) {
                    entry.scores.push(mean);
                }
            }
            for entry in &mut versions {
                entry.scores.sort_by(|a, b| a.name.cmp(&b.name));
            }
            versions.sort_by_key(|a| std::cmp::Reverse(a.version));
            Ok(versions)
        })
        .await?;

    Ok(metrics)
}

struct PromptFanSink {
    span_agg: HashAgg,
    pairs: HashAgg,
    trace_versions: HashAgg,
}

impl PushSink for PromptFanSink {
    fn consume(&mut self, batch: Batch) -> Result<ControlFlow, QueryError> {
        let f1 = self.span_agg.consume(batch.clone())?;
        let f2 = self.pairs.consume(batch.clone())?;
        let f3 = self.trace_versions.consume(batch)?;
        if f1 == ControlFlow::Stop && f2 == ControlFlow::Stop && f3 == ControlFlow::Stop {
            Ok(ControlFlow::Stop)
        } else {
            Ok(ControlFlow::Continue)
        }
    }

    fn finish(&mut self) -> Result<(), QueryError> {
        self.span_agg.finish()?;
        self.pairs.finish()?;
        self.trace_versions.finish()?;
        Ok(())
    }
}

fn build_span_agg_spec(schema: &Schema) -> Result<HashAggSpec, QueryError> {
    let version_col = col_expr(schema, spans::PROMPT_VERSION)?;
    let trace_col = col_expr(schema, spans::TRACE_ID)?;
    let status_col = col_expr(schema, spans::STATUS)?;
    let cost_col = col_expr(schema, spans::COST_TOTAL)?;
    let dur_col = col_expr(schema, spans::DURATION_NS)?;
    let in_tok_col = col_expr(schema, spans::INPUT_TOKENS)?;
    let out_tok_col = col_expr(schema, spans::OUTPUT_TOKENS)?;

    let error_flag = Expr::Case {
        operand: None,
        when_then: vec![(
            Expr::Binary {
                left: Box::new(status_col),
                op: BinaryOp::Eq,
                right: Box::new(Expr::Literal(ScalarValue::Utf8(
                    SpanStatus::Error.as_str().to_owned(),
                ))),
                data_type: DataType::Bool,
            },
            Expr::Literal(ScalarValue::I64(1)),
        )],
        else_expr: Some(Box::new(Expr::Literal(ScalarValue::I64(0)))),
        data_type: DataType::I64,
    };

    Ok(HashAggSpec {
        group_by: vec![(spans::PROMPT_VERSION.to_owned(), version_col)],
        aggs: vec![
            AggExpr {
                func: AggFunc::Count(Expr::Literal(ScalarValue::I64(1))),
                filter: None,
                output_name: SPAN_COUNT.to_owned(),
            },
            AggExpr {
                func: AggFunc::CountDistinct(trace_col),
                filter: None,
                output_name: TRACE_COUNT.to_owned(),
            },
            AggExpr {
                func: AggFunc::Sum(error_flag),
                filter: None,
                output_name: ERROR_COUNT.to_owned(),
            },
            AggExpr {
                func: AggFunc::SumCostDecimal(cost_col.clone()),
                filter: None,
                output_name: COST_SUM.to_owned(),
            },
            AggExpr {
                func: AggFunc::Avg(cost_col),
                filter: None,
                output_name: COST_AVG.to_owned(),
            },
            AggExpr {
                func: AggFunc::ApproxPercentileCont {
                    expr: dur_col.clone(),
                    quantile: 0.5,
                },
                filter: None,
                output_name: LATENCY_P50.to_owned(),
            },
            AggExpr {
                func: AggFunc::ApproxPercentileCont {
                    expr: dur_col,
                    quantile: 0.95,
                },
                filter: None,
                output_name: LATENCY_P95.to_owned(),
            },
            AggExpr {
                func: AggFunc::Avg(in_tok_col),
                filter: None,
                output_name: INPUT_AVG.to_owned(),
            },
            AggExpr {
                func: AggFunc::Avg(out_tok_col),
                filter: None,
                output_name: OUTPUT_AVG.to_owned(),
            },
        ],
    })
}

fn decode_version_metrics(batches: &[Batch]) -> Result<Vec<PromptVersionMetrics>, RunError> {
    let mut found = Vec::new();
    for batch in batches {
        if batch.num_rows() == 0 {
            continue;
        }
        let ver_col = require_col(batch, spans::PROMPT_VERSION)?;
        let span_cnt_col = require_col(batch, SPAN_COUNT)?;
        let trace_cnt_col = require_col(batch, TRACE_COUNT)?;
        let err_cnt_col = require_col(batch, ERROR_COUNT)?;
        let cost_sum_col = require_col(batch, COST_SUM)?;
        let cost_avg_col = require_col(batch, COST_AVG)?;
        let p50_col = require_col(batch, LATENCY_P50)?;
        let p95_col = require_col(batch, LATENCY_P95)?;
        let in_avg_col = require_col(batch, INPUT_AVG)?;
        let out_avg_col = require_col(batch, OUTPUT_AVG)?;

        for phys in active_rows(batch) {
            found.push(PromptVersionMetrics {
                version: raw_i64_at(ver_col, phys)?,
                span_count: raw_i64_at(span_cnt_col, phys)?,
                trace_count: raw_i64_at(trace_cnt_col, phys)?,
                error_count: raw_i64_at(err_cnt_col, phys)?,
                cost_total: raw_decimal_to_f64_at(cost_sum_col, phys)?.unwrap_or(0.0),
                cost_avg: raw_f64_at(cost_avg_col, phys)?,
                latency_p50_ns: raw_f64_at(p50_col, phys)?.map(|ns| ns.round() as i64),
                latency_p95_ns: raw_f64_at(p95_col, phys)?.map(|ns| ns.round() as i64),
                input_tokens_avg: raw_f64_at(in_avg_col, phys)?,
                output_tokens_avg: raw_f64_at(out_avg_col, phys)?,
                scores: Vec::new(),
            });
        }
    }
    Ok(found)
}

fn compute_score_means(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    window: TimeWindow,
    pairs_stream: Box<dyn PartitionStream>,
    tv_stream: Box<dyn PartitionStream>,
) -> Result<Vec<(i64, PromptScoreMean)>, QueryError> {
    let branch_schema = Schema::try_new_arc(vec![
        Field::new(VERSION, DataType::I64, false),
        Field::new(scores::NAME, DataType::Utf8, false),
        Field::new(scores::VALUE_NUM, DataType::F64, true),
    ])?;

    let mut union_streams: Vec<Box<dyn PartitionStream>> = Vec::new();

    // Span score branch
    let pairs_schema = Schema::try_new_arc(vec![
        Field::new(PAIR_TRACE, DataType::FixedBytes16, false),
        Field::new(PAIR_SPAN, DataType::FixedBytes8, false),
        Field::new(PAIR_VERSION, DataType::I64, true),
    ])?;
    let span_scores_set = open_live_scores(ctx, exec_ctx, window, ScoreTargetKind::Span)?;
    if !span_scores_set.streams.is_empty() {
        let joined_schema =
            join_output_schema(JoinType::Inner, &span_scores_set.schema, &pairs_schema);
        let joined = execute_hash_join(
            span_scores_set.streams,
            Arc::clone(&span_scores_set.schema),
            vec![pairs_stream],
            Arc::clone(&pairs_schema),
            HashJoinSpec {
                join_type: JoinType::Inner,
                left_keys: vec![
                    col_expr(&span_scores_set.schema, scores::TRACE_ID)?,
                    col_expr(&span_scores_set.schema, scores::SPAN_ID)?,
                ],
                right_keys: vec![
                    col_expr(&pairs_schema, PAIR_TRACE)?,
                    col_expr(&pairs_schema, PAIR_SPAN)?,
                ],
                null_equals_null: false,
                residual_filter: None,
            },
            exec_ctx,
        )?;
        let proj = ProjectOp::with_schema(
            vec![
                col_expr(&joined_schema, PAIR_VERSION)?,
                col_expr(&joined_schema, scores::NAME)?,
                col_expr(&joined_schema, scores::VALUE_NUM)?,
            ],
            Arc::clone(&branch_schema),
            exec_ctx.clone(),
        )?;
        union_streams.push(Box::new(OperatorStream::new(
            joined,
            vec![Box::new(proj)],
            exec_ctx,
        )));
    }

    // Trace score branch
    let tv_schema = Schema::try_new_arc(vec![
        Field::new(TRACE_VERSION_TRACE, DataType::FixedBytes16, false),
        Field::new(TRACE_VERSION, DataType::I64, true),
    ])?;
    let trace_scores_set = open_live_scores(ctx, exec_ctx, window, ScoreTargetKind::Trace)?;
    if !trace_scores_set.streams.is_empty() {
        let joined_schema =
            join_output_schema(JoinType::Inner, &trace_scores_set.schema, &tv_schema);
        let joined = execute_hash_join(
            trace_scores_set.streams,
            Arc::clone(&trace_scores_set.schema),
            vec![tv_stream],
            Arc::clone(&tv_schema),
            HashJoinSpec {
                join_type: JoinType::Inner,
                left_keys: vec![col_expr(&trace_scores_set.schema, scores::TRACE_ID)?],
                right_keys: vec![col_expr(&tv_schema, TRACE_VERSION_TRACE)?],
                null_equals_null: false,
                residual_filter: None,
            },
            exec_ctx,
        )?;
        let proj = ProjectOp::with_schema(
            vec![
                col_expr(&joined_schema, TRACE_VERSION)?,
                col_expr(&joined_schema, scores::NAME)?,
                col_expr(&joined_schema, scores::VALUE_NUM)?,
            ],
            Arc::clone(&branch_schema),
            exec_ctx.clone(),
        )?;
        union_streams.push(Box::new(OperatorStream::new(
            joined,
            vec![Box::new(proj)],
            exec_ctx,
        )));
    }

    if union_streams.is_empty() {
        return Ok(Vec::new());
    }

    let agg_spec = HashAggSpec {
        group_by: vec![
            (VERSION.to_owned(), col_expr(&branch_schema, VERSION)?),
            (
                scores::NAME.to_owned(),
                col_expr(&branch_schema, scores::NAME)?,
            ),
        ],
        aggs: vec![
            AggExpr {
                func: AggFunc::Avg(col_expr(&branch_schema, scores::VALUE_NUM)?),
                filter: None,
                output_name: MEAN.to_owned(),
            },
            AggExpr {
                func: AggFunc::Count(Expr::Literal(ScalarValue::I64(1))),
                filter: None,
                output_name: COUNT.to_owned(),
            },
        ],
    };

    let agg_stream = execute_hash_agg_streams(union_streams, branch_schema, agg_spec, exec_ctx)?;
    let pool_c = Arc::clone(&exec_ctx.pool);
    let sinks = exec_run_pipeline(
        exec_ctx,
        vec![agg_stream],
        |_| Ok(Vec::new()),
        move |_| Ok(CollectSink::new(&pool_c)),
    )?;

    let mut out = Vec::new();
    for sink in sinks {
        for batch in sink.into_batches() {
            if batch.num_rows() == 0 {
                continue;
            }
            let ver_col = require_col(&batch, VERSION).map_err(run_err_to_query)?;
            let name_col = require_col(&batch, scores::NAME).map_err(run_err_to_query)?;
            let mean_col = require_col(&batch, MEAN).map_err(run_err_to_query)?;
            let count_col = require_col(&batch, COUNT).map_err(run_err_to_query)?;
            for phys in active_rows(&batch) {
                let Some(mean) = raw_f64_at(mean_col, phys).map_err(run_err_to_query)? else {
                    continue;
                };
                let name = raw_string_at(name_col, phys)
                    .map_err(run_err_to_query)?
                    .ok_or_else(|| QueryError::internal("score name is null"))?;
                let version = raw_i64_at(ver_col, phys).map_err(run_err_to_query)?;
                let count = raw_i64_at(count_col, phys).map_err(run_err_to_query)?;
                out.push((version, PromptScoreMean { name, mean, count }));
            }
        }
    }
    Ok(out)
}

fn open_live_scores(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    window: TimeWindow,
    target: ScoreTargetKind,
) -> Result<StreamSet, QueryError> {
    let scores_raw = scores_raw_schema().map_err(run_err_to_query)?;
    let mut min_ns = window.from();
    if let Some(vf) = ctx.visible_from {
        min_ns = min_ns.max(vf);
    }

    let created_col = col_expr(&scores_raw, scores::CREATED_AT)?;
    let mut filters = vec![
        Expr::Binary {
            left: Box::new(col_expr(&scores_raw, scores::TARGET)?),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::Utf8(target.as_str().to_owned()))),
            data_type: DataType::Bool,
        },
        Expr::InList {
            expr: Box::new(col_expr(&scores_raw, scores::DATA_TYPE)?),
            list: vec![
                Expr::Literal(ScalarValue::Utf8(
                    ScoreDataType::Numeric.as_str().to_owned(),
                )),
                Expr::Literal(ScalarValue::Utf8(
                    ScoreDataType::Boolean.as_str().to_owned(),
                )),
            ],
            negated: false,
            redacted: false,
        },
        Expr::Unary {
            op: UnaryOp::IsNotNull,
            expr: Box::new(col_expr(&scores_raw, scores::VALUE_NUM)?),
            data_type: DataType::Bool,
        },
        Expr::Binary {
            left: Box::new(created_col.clone()),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                value: window.from(),
                utc: true,
            })),
            data_type: DataType::Bool,
        },
        Expr::Unary {
            op: UnaryOp::IsNotNull,
            expr: Box::new(col_expr(&scores_raw, scores::TRACE_ID)?),
            data_type: DataType::Bool,
        },
    ];
    if target == ScoreTargetKind::Span {
        filters.push(Expr::Unary {
            op: UnaryOp::IsNotNull,
            expr: Box::new(col_expr(&scores_raw, scores::SPAN_ID)?),
            data_type: DataType::Bool,
        });
    }
    if let Some(vf) = ctx.visible_from {
        filters.push(Expr::Binary {
            left: Box::new(created_col),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                value: vf,
                utc: true,
            })),
            data_type: DataType::Bool,
        });
    }

    let mut needed = vec![scores::TRACE_ID, scores::NAME, scores::VALUE_NUM];
    if target == ScoreTargetKind::Span {
        needed.push(scores::SPAN_ID);
    }
    let mut proj_indices = Vec::with_capacity(needed.len());
    for name in needed {
        let idx = scores_raw
            .index_of(name)
            .ok_or_else(|| QueryError::internal(format!("missing {name} in scores schema")))?;
        proj_indices.push(idx);
    }

    let spec = ScanSpec {
        projection: Some(proj_indices),
        map_keys: Vec::new(),
        time_range: TimeRangeHint {
            min_ns: Some(min_ns),
            max_ns: None,
            around_ns: None,
        },
        id_lookups: Vec::new(),
        filters,
        substrings: Vec::new(),
        sort_hints: Vec::new(),
        limit: None,
    };

    let proj_schema = ctx.scores.projected_schema(&spec)?;
    let streams = ctx.scores.open_partitions(&spec, exec_ctx)?;
    Ok(StreamSet {
        streams,
        schema: proj_schema,
    })
}

fn col_expr(schema: &Schema, name: &str) -> Result<Expr, QueryError> {
    let idx = schema
        .index_of(name)
        .ok_or_else(|| QueryError::internal(format!("missing column {name} in schema")))?;
    Ok(Expr::Column {
        index: idx,
        name: name.to_owned(),
        data_type: schema.fields()[idx].data_type.clone(),
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytesize::ByteSize;
    use cetra_core::enums::{SpanKind, SpanStatus};
    use cetra_core::ids::ProjectId;
    use cetra_core::names::environment;
    use cetra_core::score::{Score, ScoreTarget, ScoreValue};
    use cetra_core::span::Span;

    use super::*;
    use crate::QueryOptions;
    use crate::config::{
        FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
    };
    use crate::test_support::{score_row, score_rows, span_row, span_rows};
    use crate::views::{QueryContext, TimeWindow};

    const TIMEOUT: Duration = Duration::from_secs(30);
    const T0: i64 = 1_700_000_000_000_000_000;
    const MIN: i64 = 60_000_000_000;
    const MS: i64 = 1_000_000;

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

    fn root(trace: u16, start: i64) -> Span {
        span_row(trace, 0, start, 1)
    }

    fn root_id(trace: u16) -> cetra_core::ids::SpanId {
        span_row(trace, 0, 0, 1).span_id
    }

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

    fn span_score(id: u128, span: &Span, name: &str, value: ScoreValue) -> Score {
        let mut score = trace_score(id, 0, name, value);
        score.target = ScoreTarget::Span {
            trace_id: span.trace_id,
            span_id: span.span_id,
        };
        score
    }

    fn assert_rel_close_i64(actual: Option<i64>, expected: Option<i64>, label: &str) {
        match (actual, expected) {
            (None, None) => {}
            (Some(a), Some(e)) => {
                let tol = ((e.abs() as f64) * 0.01).max(1.0);
                assert!(
                    ((a - e).abs() as f64) <= tol,
                    "{label}: actual={a} vs expected={e} (tol={tol})"
                );
            }
            other => panic!("{label}: mismatch {other:?}"),
        }
    }

    #[tokio::test]
    async fn prompt_metrics_aggregates_versions_and_percentiles() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = crate::Storage::open(test_config(dir.path()))
            .await
            .expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");

        let mut spans = Vec::new();
        // A: two llm spans on p@1 (second errors), one trace score -> must count ONCE for p@1.
        spans.push(root(0, T0 + MIN));
        let mut a1 = llm(0, 1, T0 + MIN, "p", 1, 0.01, 100 * MS);
        a1.input_tokens = Some(10);
        a1.output_tokens = Some(5);
        spans.push(a1);
        let mut a2 = llm(0, 2, T0 + MIN, "p", 1, 0.03, 300 * MS);
        a2.status = SpanStatus::Error;
        spans.push(a2);
        let a_score = trace_score(1, 0, "quality", ScoreValue::Numeric(1.0));

        // B: one llm span on p@2 with span score and trace score
        spans.push(root(1, T0 + MIN));
        let b_llm = llm(1, 1, T0 + MIN, "p", 2, 0.02, 200 * MS);
        let b_span_score = span_score(2, &b_llm, "quality", ScoreValue::Numeric(0.5));
        spans.push(b_llm);
        let b_trace_score = trace_score(3, 1, "quality", ScoreValue::Numeric(0.0));

        // C: llm spans on p@1 and p@2 with boolean trace score (counts once for each version!)
        spans.push(root(2, T0 + MIN));
        spans.push(llm(2, 1, T0 + MIN, "p", 1, 0.05, 100 * MS));
        spans.push(llm(2, 2, T0 + MIN, "p", 2, 0.07, 200 * MS));
        let c_score = trace_score(4, 2, "helpful", ScoreValue::Boolean(true));

        // D: eval trace excluded
        let mut d_root = root(3, T0 + MIN);
        d_root.environment = Some(environment::EVAL.to_owned());
        spans.push(d_root);
        let mut d_llm = llm(3, 1, T0 + MIN, "p", 1, 0.5, 100 * MS);
        d_llm.environment = Some(environment::EVAL.to_owned());
        spans.push(d_llm);

        // E: trace starts before window and ends inside it (tests ends_after_from = true)
        let mut e_root = root(4, T0 - MIN);
        e_root.end_time = T0 + 2 * MIN;
        spans.push(e_root);
        spans.push(llm(4, 1, T0 - MIN, "p", 1, 0.5, 100 * MS));

        // F: 500 spans on p@3 with varied latencies to test <= 1% p50/p95 tolerance
        for k in 10..510_u16 {
            spans.push(root(k, T0 + MIN));
            spans.push(llm(k, 1, T0 + MIN, "p", 3, 0.0, i64::from(k - 9) * MS));
        }

        // G: early score before window.from() (ignored)
        let mut early = trace_score(5, 0, "early", ScoreValue::Numeric(1.0));
        early.created_at = T0 - MIN;
        early.updated_at = T0 - MIN;

        let scores = vec![a_score, b_span_score, b_trace_score, c_score, early];

        storage
            .append_spans(project, span_rows(&spans))
            .await
            .expect("spans");
        storage
            .append_scores(project, score_rows(&scores))
            .await
            .expect("scores");

        let opts = QueryOptions::new(None, None);
        let pq = storage.query(project, opts).await.expect("pq");
        let ctx = QueryContext::new(pq, None).expect("ctx");

        let win = TimeWindow::new(T0, T0 + 10 * MIN).expect("win");
        let all = prompt_metrics(
            &ctx,
            PromptMetricsRequest {
                prompt: "p",
                window: win,
                versions: &[],
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("all versions");
        assert_eq!(
            all.iter().map(|m| m.version).collect::<Vec<_>>(),
            vec![3, 2, 1]
        );

        let v3 = &all[0];
        assert_eq!(v3.span_count, 500);
        assert_eq!(v3.trace_count, 500);
        assert_eq!(v3.error_count, 0);
        assert_rel_close_i64(v3.latency_p50_ns, Some(250 * MS), "v3 p50");
        assert_rel_close_i64(v3.latency_p95_ns, Some(475 * MS), "v3 p95");

        let v2 = &all[1];
        assert_eq!(v2.span_count, 2);
        assert_eq!(v2.trace_count, 2);
        assert_eq!(v2.error_count, 0);
        assert!((v2.cost_total - 0.09).abs() < 1e-12);
        assert!((v2.cost_avg.unwrap() - 0.045).abs() < 1e-12);
        assert_eq!(
            v2.scores,
            vec![
                PromptScoreMean {
                    name: "helpful".to_owned(),
                    mean: 1.0,
                    count: 1,
                },
                PromptScoreMean {
                    name: "quality".to_owned(),
                    mean: 0.25,
                    count: 2,
                },
            ]
        );

        let v1 = &all[2];
        assert_eq!(v1.span_count, 3);
        assert_eq!(v1.trace_count, 2);
        assert_eq!(v1.error_count, 1);
        assert!((v1.cost_total - 0.09).abs() < 1e-12);
        assert_eq!(v1.input_tokens_avg, Some(10.0));
        assert_eq!(v1.output_tokens_avg, Some(5.0));

        let filtered = prompt_metrics(
            &ctx,
            PromptMetricsRequest {
                prompt: "p",
                window: win,
                versions: &[2, 1],
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("filtered versions");
        assert_eq!(
            filtered.iter().map(|m| m.version).collect::<Vec<_>>(),
            vec![2, 1]
        );

        // Underflow on window.from() - DAY_NS -> InvalidRequest("from is out of range")
        let underflow_req = PromptMetricsRequest {
            prompt: "p",
            window: TimeWindow::new(i64::MIN, 0).expect("win"),
            versions: &[],
            timeout: TIMEOUT,
        };
        assert!(matches!(
            prompt_metrics(&ctx, underflow_req).await,
            Err(RunError::InvalidRequest(ref msg)) if msg == "from is out of range"
        ));

        let _ = storage.shutdown().await;
    }
}
