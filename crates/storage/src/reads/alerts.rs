//! Native query engine implementation of `alert_value` (spec 07, D-K-2, D-K-11).

use std::sync::Arc;
use std::time::Duration;

use cetra_column::{Batch, DataType, ScalarValue, Schema};
use cetra_core::enums::{ScoreDataType, ScoreTargetKind, SpanKind};
use cetra_core::schema::{scores, spans, traces};
use cetra_query::exec::streaming::ProjectOp;
use cetra_query::exec::{
    AggExpr, AggFunc, ExecContext, HashAgg, HashAggSpec, PartitionStream, ScanSource, ScanSpec,
    TimeRangeHint, run_pipeline as exec_run_pipeline,
};
use cetra_query::{BinaryOp, CompiledFilter, Expr, FilterView, QueryError, UnaryOp};

use crate::query_error::RunError;
use crate::reads::columns::{decimal_to_f64_at, f64_at, i64_at, internal, require_col};
use crate::reads::context::{
    DURATION_NS, EngineContext, OperatorStream, duration_ns_from_bounds_expr, populate_id_lookups,
    query_err_to_run, rewrite_view_expr_for_raw, run_err_to_query, scores_raw_schema,
    spans_raw_schema, traces_raw_schema, traces_view_projection, traces_view_schema,
};
use crate::reads::scope::{
    StreamSet, TraceSemiJoinParams, apply_trace_id_semi_join, scoped_traces_spec,
};
use crate::reads::scores_join::{apply_compiled_filter, compile_optional_filter};
use crate::reads::{ListScope, MAX_COUNT_NAMES, present};
use crate::views::{QueryContext, TimeWindow};

const COUNT: &str = "alert_count";
const ERRORS: &str = "alert_errors";
const VALUE: &str = "alert_value";
const NS_PER_MS: f64 = 1_000_000.0;
const DAY_NS: i64 = 86_400_000_000_000;
/// How long before the window a trace may have started and still have its scores
/// counted (D-K-17). Span measures look back one day (D-K-3).
const SCORE_TRACE_LOOKBACK: i64 = 7 * DAY_NS;

/// Which value an alert measures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlertMeasure {
    /// Traces with `error_count > 0` divided by traces.
    ErrorRate,
    /// `sum(cost_total)` on traces, USD.
    Cost,
    /// Trace `duration_ns` p95, in milliseconds.
    TraceLatencyP95,
    /// `duration_ns` p95 of `llm` spans that ended in the window, in milliseconds.
    LlmLatencyP95,
    /// `avg(value_num)` of the live trace and span scores named `name` whose
    /// `value_num` is not null: numeric, boolean (1/0), and categorical with a value.
    ScoreAvg {
        /// Score name.
        name: String,
    },
    /// `avg(value_num)` of the live boolean scores named `name`.
    ScorePassRate {
        /// Score name.
        name: String,
    },
    /// The number of live scores whose name is in `names` and whose `created_at` is in the
    /// window, whatever their target (D-IC-20). `AlertValueRequest::filter` must be absent.
    ScoreCount {
        /// Score names, at most [`MAX_COUNT_NAMES`]; empty counts 0.
        names: Vec<String>,
    },
}

/// One alert evaluation.
#[derive(Debug, Clone)]
pub struct AlertValueRequest<'a> {
    /// `start_time ∈ [from, to)` for traces, `end_time ∈ [from, to)` for `llm` spans,
    /// `created_at ∈ [from, to)` for scores.
    pub window: TimeWindow,
    /// Filter text over `traces`. `None`, empty or all-whitespace means no filter.
    pub filter: Option<&'a str>,
    /// What to measure.
    pub measure: AlertMeasure,
    /// Time allowed for planning and running.
    pub timeout: Duration,
}

/// The sample size and the value over the window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlertValue {
    /// Rows the value was computed over: traces, `llm` spans, or scores.
    pub count: i64,
    /// `None` when `count` is 0, except for [`AlertMeasure::Cost`], which is always
    /// `Some` (0.0 when no trace or no cost).
    pub value: Option<f64>,
}

/// Measures one alert over its window.
pub async fn alert_value(
    ctx: &QueryContext,
    request: AlertValueRequest<'_>,
) -> Result<AlertValue, RunError> {
    let ctx = ctx.engine();
    if matches!(request.measure, AlertMeasure::ScoreCount { .. })
        && present(request.filter).is_some()
    {
        return Err(RunError::InvalidRequest(
            "a score count takes no filter".to_owned(),
        ));
    }

    let filter = compile_optional_filter(FilterView::Traces, request.filter)?;
    let filter_text = present(request.filter).map(str::to_owned);
    let window = request.window;

    if let AlertMeasure::ScoreCount { names } = &request.measure {
        if names.len() > MAX_COUNT_NAMES {
            return Err(RunError::InvalidRequest(format!(
                "at most {MAX_COUNT_NAMES} names"
            )));
        }
        if names.is_empty() {
            return Ok(AlertValue {
                count: 0,
                value: Some(0.0),
            });
        }
    }

    let measure = request.measure.clone();
    let ctx_clone = ctx.clone();
    let (value, _) = ctx
        .run_with_ctx(request.timeout, move |exec_ctx| {
            run_alert(
                &ctx_clone,
                exec_ctx,
                window,
                filter,
                filter_text.as_deref(),
                &measure,
            )
            .map_err(run_err_to_query)
        })
        .await?;
    Ok(value)
}

fn run_alert(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    window: TimeWindow,
    filter: Option<CompiledFilter>,
    filter_text: Option<&str>,
    measure: &AlertMeasure,
) -> Result<AlertValue, RunError> {
    match measure {
        AlertMeasure::ErrorRate => eval_error_rate(ctx, exec_ctx, window, filter),
        AlertMeasure::Cost => eval_cost(ctx, exec_ctx, window, filter),
        AlertMeasure::TraceLatencyP95 => eval_trace_latency_p95(ctx, exec_ctx, window, filter),
        AlertMeasure::LlmLatencyP95 => eval_llm_latency_p95(ctx, exec_ctx, window, filter_text),
        AlertMeasure::ScoreAvg { name } => {
            eval_score_value(ctx, exec_ctx, window, filter_text, name, false)
        }
        AlertMeasure::ScorePassRate { name } => {
            eval_score_value(ctx, exec_ctx, window, filter_text, name, true)
        }
        AlertMeasure::ScoreCount { names } => eval_score_count(ctx, exec_ctx, window, names),
    }
}

fn eval_error_rate(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    window: TimeWindow,
    filter: Option<CompiledFilter>,
) -> Result<AlertValue, RunError> {
    let StreamSet { streams, schema } =
        open_scoped_traces(ctx, exec_ctx, window, filter, &[traces::ERROR_COUNT])?;
    let err_col = col_expr(&schema, traces::ERROR_COUNT)?;
    let has_err = Expr::Binary {
        left: Box::new(err_col),
        op: BinaryOp::Gt,
        right: Box::new(Expr::Literal(ScalarValue::I64(0))),
        data_type: DataType::Bool,
    };
    let agg_spec = HashAggSpec {
        group_by: Vec::new(),
        aggs: vec![
            AggExpr {
                func: AggFunc::CountStar,
                filter: None,
                output_name: COUNT.to_owned(),
            },
            AggExpr {
                func: AggFunc::CountStar,
                filter: Some(has_err),
                output_name: ERRORS.to_owned(),
            },
        ],
    };
    let batches = run_global_agg(streams, schema, agg_spec, exec_ctx)?;
    let batch = single_row(&batches)?;
    let count = i64_at(batch, require_col(batch, COUNT)?, 0)?;
    let errors = i64_at(batch, require_col(batch, ERRORS)?, 0)?;
    Ok(AlertValue {
        count,
        value: (count > 0).then(|| errors as f64 / count as f64),
    })
}

fn eval_cost(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    window: TimeWindow,
    filter: Option<CompiledFilter>,
) -> Result<AlertValue, RunError> {
    let StreamSet { streams, schema } =
        open_scoped_traces(ctx, exec_ctx, window, filter, &[traces::COST_TOTAL])?;
    let cost_col = col_expr(&schema, traces::COST_TOTAL)?;
    let agg_spec = HashAggSpec {
        group_by: Vec::new(),
        aggs: vec![
            AggExpr {
                func: AggFunc::CountStar,
                filter: None,
                output_name: COUNT.to_owned(),
            },
            AggExpr {
                func: AggFunc::SumCostDecimal(cost_col),
                filter: None,
                output_name: VALUE.to_owned(),
            },
        ],
    };
    let batches = run_global_agg(streams, schema, agg_spec, exec_ctx)?;
    let batch = single_row(&batches)?;
    let count = i64_at(batch, require_col(batch, COUNT)?, 0)?;
    let cost = decimal_to_f64_at(batch, require_col(batch, VALUE)?, 0)?.unwrap_or(0.0);
    Ok(AlertValue {
        count,
        value: Some(cost),
    })
}

fn eval_trace_latency_p95(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    window: TimeWindow,
    filter: Option<CompiledFilter>,
) -> Result<AlertValue, RunError> {
    let uses_scores = filter.as_ref().is_some_and(|f| !f.scores.is_empty());
    let cols: &[&str] = if uses_scores {
        &[DURATION_NS]
    } else {
        &[traces::START_TIME, traces::END_TIME]
    };
    let StreamSet { streams, schema } = open_scoped_traces(ctx, exec_ctx, window, filter, cols)?;
    let dur_expr = if schema.index_of(DURATION_NS).is_some() {
        col_expr(&schema, DURATION_NS)?
    } else {
        duration_ns_from_bounds_expr(&schema).map_err(query_err_to_run)?
    };
    let agg_spec = HashAggSpec {
        group_by: Vec::new(),
        aggs: vec![
            AggExpr {
                func: AggFunc::CountStar,
                filter: None,
                output_name: COUNT.to_owned(),
            },
            AggExpr {
                func: AggFunc::ApproxPercentileCont {
                    expr: dur_expr,
                    quantile: 0.95,
                },
                filter: None,
                output_name: VALUE.to_owned(),
            },
        ],
    };
    let batches = run_global_agg(streams, schema, agg_spec, exec_ctx)?;
    let batch = single_row(&batches)?;
    let count = i64_at(batch, require_col(batch, COUNT)?, 0)?;
    Ok(AlertValue {
        count,
        value: latency_ms(batch, count)?,
    })
}

fn eval_llm_latency_p95(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    window: TimeWindow,
    filter_text: Option<&str>,
) -> Result<AlertValue, RunError> {
    let spans_raw = spans_raw_schema()?;
    let trace_id_idx = col_index(&spans_raw, spans::TRACE_ID)?;
    let duration_idx = col_index(&spans_raw, spans::DURATION_NS)?;

    let end_col = col_expr(&spans_raw, spans::END_TIME)?;
    let kind_col = col_expr(&spans_raw, spans::SPAN_KIND)?;
    let mut filters = vec![
        cmp_ts(&end_col, BinaryOp::GtEq, window.from()),
        cmp_ts(&end_col, BinaryOp::Lt, window.to()),
        Expr::Binary {
            left: Box::new(kind_col),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::Utf8(
                SpanKind::Llm.as_str().to_owned(),
            ))),
            data_type: DataType::Bool,
        },
    ];
    if let Some(vf) = ctx.visible_from {
        let start_col = col_expr(&spans_raw, spans::START_TIME)?;
        filters.push(cmp_ts(&start_col, BinaryOp::GtEq, vf));
    }

    let spec = ScanSpec {
        projection: Some(vec![trace_id_idx, duration_idx]),
        map_keys: Vec::new(),
        time_range: TimeRangeHint {
            min_ns: ctx.visible_from,
            max_ns: None,
            around_ns: None,
        },
        id_lookups: Vec::new(),
        filters,
        substrings: Vec::new(),
        sort_hints: Vec::new(),
        limit: None,
    };
    let proj_schema = ctx
        .spans
        .projected_schema(&spec)
        .map_err(query_err_to_run)?;
    let raw_streams = ctx
        .spans
        .open_partitions(&spec, exec_ctx)
        .map_err(query_err_to_run)?;

    let joined_streams = apply_trace_id_semi_join(
        ctx,
        exec_ctx,
        StreamSet {
            streams: raw_streams,
            schema: Arc::clone(&proj_schema),
        },
        &TraceSemiJoinParams {
            lookback_ns: DAY_NS,
            ends_after_from: true,
            scope: ListScope {
                window: Some(window),
                include_eval: false,
                include_experiments: false,
            },
            references_environment: false,
            dashboard_filter: filter_text,
        },
    )?;

    let dur_col = col_expr(&proj_schema, spans::DURATION_NS)?;
    let agg_spec = HashAggSpec {
        group_by: Vec::new(),
        aggs: vec![
            AggExpr {
                func: AggFunc::CountStar,
                filter: None,
                output_name: COUNT.to_owned(),
            },
            AggExpr {
                func: AggFunc::ApproxPercentileCont {
                    expr: dur_col,
                    quantile: 0.95,
                },
                filter: None,
                output_name: VALUE.to_owned(),
            },
        ],
    };
    let batches = run_global_agg(joined_streams, proj_schema, agg_spec, exec_ctx)?;
    let batch = single_row(&batches)?;
    let count = i64_at(batch, require_col(batch, COUNT)?, 0)?;
    Ok(AlertValue {
        count,
        value: latency_ms(batch, count)?,
    })
}

fn eval_score_value(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    window: TimeWindow,
    filter_text: Option<&str>,
    name: &str,
    pass_rate: bool,
) -> Result<AlertValue, RunError> {
    let scores_raw = scores_raw_schema()?;
    let trace_id_idx = col_index(&scores_raw, scores::TRACE_ID)?;
    let val_idx = col_index(&scores_raw, scores::VALUE_NUM)?;

    let created_col = col_expr(&scores_raw, scores::CREATED_AT)?;
    let name_col = col_expr(&scores_raw, scores::NAME)?;
    let target_col = col_expr(&scores_raw, scores::TARGET)?;
    let val_col = col_expr(&scores_raw, scores::VALUE_NUM)?;

    let mut min_ns = window.from();
    if let Some(vf) = ctx.visible_from {
        min_ns = min_ns.max(vf);
    }
    let mut filters = vec![
        cmp_ts(&created_col, BinaryOp::GtEq, window.from()),
        cmp_ts(&created_col, BinaryOp::Lt, window.to()),
        Expr::Binary {
            left: Box::new(name_col),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::Utf8(name.to_owned()))),
            data_type: DataType::Bool,
        },
        Expr::InList {
            expr: Box::new(target_col),
            list: vec![
                Expr::Literal(ScalarValue::Utf8(
                    ScoreTargetKind::Trace.as_str().to_owned(),
                )),
                Expr::Literal(ScalarValue::Utf8(ScoreTargetKind::Span.as_str().to_owned())),
            ],
            negated: false,
            redacted: false,
        },
        Expr::Unary {
            op: UnaryOp::IsNotNull,
            expr: Box::new(val_col),
            data_type: DataType::Bool,
        },
    ];
    if let Some(vf) = ctx.visible_from {
        filters.push(cmp_ts(&created_col, BinaryOp::GtEq, vf));
    }
    if pass_rate {
        let dt_col = col_expr(&scores_raw, scores::DATA_TYPE)?;
        filters.push(Expr::Binary {
            left: Box::new(dt_col),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::Utf8(
                ScoreDataType::Boolean.as_str().to_owned(),
            ))),
            data_type: DataType::Bool,
        });
    }

    let spec = ScanSpec {
        projection: Some(vec![trace_id_idx, val_idx]),
        map_keys: Vec::new(),
        time_range: TimeRangeHint {
            min_ns: Some(min_ns),
            max_ns: window.to().checked_sub(1),
            around_ns: None,
        },
        id_lookups: Vec::new(),
        filters,
        substrings: Vec::new(),
        sort_hints: Vec::new(),
        limit: None,
    };
    let proj_schema = ctx
        .scores
        .projected_schema(&spec)
        .map_err(query_err_to_run)?;
    let raw_streams = ctx
        .scores
        .open_partitions(&spec, exec_ctx)
        .map_err(query_err_to_run)?;

    let joined_streams = apply_trace_id_semi_join(
        ctx,
        exec_ctx,
        StreamSet {
            streams: raw_streams,
            schema: Arc::clone(&proj_schema),
        },
        &TraceSemiJoinParams {
            lookback_ns: SCORE_TRACE_LOOKBACK,
            ends_after_from: false,
            scope: ListScope {
                window: Some(window),
                include_eval: false,
                include_experiments: false,
            },
            references_environment: false,
            dashboard_filter: filter_text,
        },
    )?;

    let value_col = col_expr(&proj_schema, scores::VALUE_NUM)?;
    let agg_spec = HashAggSpec {
        group_by: Vec::new(),
        aggs: vec![
            AggExpr {
                func: AggFunc::CountStar,
                filter: None,
                output_name: COUNT.to_owned(),
            },
            AggExpr {
                func: AggFunc::Avg(value_col),
                filter: None,
                output_name: VALUE.to_owned(),
            },
        ],
    };
    let batches = run_global_agg(joined_streams, proj_schema, agg_spec, exec_ctx)?;
    let batch = single_row(&batches)?;
    let count = i64_at(batch, require_col(batch, COUNT)?, 0)?;
    let value = if count == 0 {
        None
    } else {
        f64_at(batch, require_col(batch, VALUE)?, 0)?
    };
    Ok(AlertValue { count, value })
}

fn eval_score_count(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    window: TimeWindow,
    names: &[String],
) -> Result<AlertValue, RunError> {
    let scores_raw = scores_raw_schema()?;
    let created_idx = col_index(&scores_raw, scores::CREATED_AT)?;
    let created_col = col_expr(&scores_raw, scores::CREATED_AT)?;
    let name_col = col_expr(&scores_raw, scores::NAME)?;

    let mut min_ns = window.from();
    if let Some(vf) = ctx.visible_from {
        min_ns = min_ns.max(vf);
    }
    let list = names
        .iter()
        .map(|n| Expr::Literal(ScalarValue::Utf8(n.clone())))
        .collect();
    let mut filters = vec![
        Expr::InList {
            expr: Box::new(name_col),
            list,
            negated: false,
            redacted: false,
        },
        cmp_ts(&created_col, BinaryOp::GtEq, window.from()),
        cmp_ts(&created_col, BinaryOp::Lt, window.to()),
    ];
    if let Some(vf) = ctx.visible_from {
        filters.push(cmp_ts(&created_col, BinaryOp::GtEq, vf));
    }

    let spec = ScanSpec {
        projection: Some(vec![created_idx]),
        map_keys: Vec::new(),
        time_range: TimeRangeHint {
            min_ns: Some(min_ns),
            max_ns: window.to().checked_sub(1),
            around_ns: None,
        },
        id_lookups: Vec::new(),
        filters,
        substrings: Vec::new(),
        sort_hints: Vec::new(),
        limit: None,
    };
    let proj_schema = ctx
        .scores
        .projected_schema(&spec)
        .map_err(query_err_to_run)?;
    let raw_streams = ctx
        .scores
        .open_partitions(&spec, exec_ctx)
        .map_err(query_err_to_run)?;

    let agg_spec = HashAggSpec {
        group_by: Vec::new(),
        aggs: vec![AggExpr {
            func: AggFunc::CountStar,
            filter: None,
            output_name: COUNT.to_owned(),
        }],
    };
    let batches = run_global_agg(raw_streams, proj_schema, agg_spec, exec_ctx)?;
    let batch = single_row(&batches)?;
    let count = i64_at(batch, require_col(batch, COUNT)?, 0)?;
    Ok(AlertValue {
        count,
        value: Some(count as f64),
    })
}

fn open_scoped_traces(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    window: TimeWindow,
    filter: Option<CompiledFilter>,
    needed_cols: &[&str],
) -> Result<StreamSet, RunError> {
    let references_environment = filter.as_ref().is_some_and(|f| f.references_environment);
    let mut spec = scoped_traces_spec(
        ctx,
        ListScope {
            window: Some(window),
            include_eval: false,
            include_experiments: false,
        },
        references_environment,
    )?;
    // Alerts use explicit sliding windows; avoid caller `around` hints.
    spec.time_range.around_ns = None;
    let traces_raw = traces_raw_schema()?;

    match filter {
        None => {
            let mut indices = Vec::with_capacity(needed_cols.len());
            for &name in needed_cols {
                indices.push(col_index(&traces_raw, name)?);
            }
            spec.projection = Some(indices);
            let proj_schema = ctx
                .traces
                .projected_schema(&spec)
                .map_err(query_err_to_run)?;
            let streams = ctx
                .traces
                .open_partitions(&spec, exec_ctx)
                .map_err(query_err_to_run)?;
            Ok(StreamSet {
                streams,
                schema: proj_schema,
            })
        }
        Some(f) if f.scores.is_empty() => {
            let rewritten = rewrite_view_expr_for_raw(
                &f.expr,
                &traces_raw,
                &[traces::TRACE_ID, traces::ROOT_SPAN_ID],
                true,
            )
            .map_err(query_err_to_run)?;
            spec.filters.push(rewritten);
            populate_id_lookups(&mut spec);
            let mut indices = Vec::with_capacity(needed_cols.len());
            for &name in needed_cols {
                indices.push(col_index(&traces_raw, name)?);
            }
            spec.projection = Some(indices);
            let proj_schema = ctx
                .traces
                .projected_schema(&spec)
                .map_err(query_err_to_run)?;
            let streams = ctx
                .traces
                .open_partitions(&spec, exec_ctx)
                .map_err(query_err_to_run)?;
            Ok(StreamSet {
                streams,
                schema: proj_schema,
            })
        }
        Some(f) => {
            let raw_streams = ctx
                .traces
                .open_partitions(&spec, exec_ctx)
                .map_err(query_err_to_run)?;
            let view_schema = traces_view_schema()?;
            let view_streams: Vec<Box<dyn PartitionStream>> = raw_streams
                .into_iter()
                .map(|s| {
                    let proj = traces_view_projection(&traces_raw, exec_ctx.clone())?;
                    Ok(
                        Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                            as Box<dyn PartitionStream>,
                    )
                })
                .collect::<Result<Vec<_>, QueryError>>()
                .map_err(query_err_to_run)?;
            let filtered = apply_compiled_filter(ctx, exec_ctx, view_streams, &view_schema, &f)
                .map_err(query_err_to_run)?;
            // Project down to only `needed_cols` after score filtering.
            let mut out_fields = Vec::with_capacity(needed_cols.len());
            let mut proj_exprs = Vec::with_capacity(needed_cols.len());
            for &name in needed_cols {
                let idx = col_index(&view_schema, name)?;
                let field = &view_schema.fields()[idx];
                out_fields.push(field.clone());
                proj_exprs.push(Expr::Column {
                    index: idx,
                    name: field.name.clone(),
                    data_type: field.data_type.clone(),
                });
            }
            let out_schema =
                Schema::try_new_arc(out_fields).map_err(|e| query_err_to_run(e.into()))?;
            let narrowed = filtered
                .into_iter()
                .map(|s| {
                    let proj = ProjectOp::with_schema(
                        proj_exprs.clone(),
                        Arc::clone(&out_schema),
                        exec_ctx.clone(),
                    )?;
                    Ok(
                        Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                            as Box<dyn PartitionStream>,
                    )
                })
                .collect::<Result<Vec<_>, QueryError>>()
                .map_err(query_err_to_run)?;
            Ok(StreamSet {
                streams: narrowed,
                schema: out_schema,
            })
        }
    }
}

fn run_global_agg(
    streams: Vec<Box<dyn PartitionStream>>,
    schema: Arc<Schema>,
    agg_spec: HashAggSpec,
    exec_ctx: &ExecContext,
) -> Result<Vec<Batch>, RunError> {
    if streams.is_empty() {
        let agg = HashAgg::new(agg_spec, schema, exec_ctx.clone()).map_err(query_err_to_run)?;
        return agg.into_batches().map_err(query_err_to_run);
    }
    let sinks = exec_run_pipeline(exec_ctx, streams, |_| Ok(Vec::new()), {
        let spec_c = agg_spec.clone();
        let schema_c = Arc::clone(&schema);
        let ctx_c = exec_ctx.clone();
        move |_| HashAgg::new(spec_c.clone(), Arc::clone(&schema_c), ctx_c.clone())
    })
    .map_err(query_err_to_run)?;
    HashAgg::merge_sinks(sinks, &agg_spec, &schema, exec_ctx).map_err(query_err_to_run)
}

fn latency_ms(batch: &Batch, count: i64) -> Result<Option<f64>, RunError> {
    if count == 0 {
        return Ok(None);
    }
    Ok(f64_at(batch, require_col(batch, VALUE)?, 0)?.map(|ns| ns / NS_PER_MS))
}

fn single_row(batches: &[Batch]) -> Result<&Batch, RunError> {
    let mut found = None;
    for batch in batches {
        if batch.num_rows() == 0 {
            continue;
        }
        if found.is_some() || batch.num_rows() != 1 {
            return Err(internal("alert aggregate row count"));
        }
        found = Some(batch);
    }
    found.ok_or_else(|| internal("alert aggregate row count"))
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

fn cmp_ts(col: &Expr, op: BinaryOp, ns: i64) -> Expr {
    Expr::Binary {
        left: Box::new(col.clone()),
        op,
        right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
            value: ns,
            utc: true,
        })),
        data_type: DataType::Bool,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytesize::ByteSize;
    use cetra_core::enums::{ScoreSource, SpanKind, SpanStatus};
    use cetra_core::ids::ProjectId;
    use cetra_core::names::{environment, metadata_key};
    use cetra_core::score::{Score, ScoreTarget, ScoreValue};
    use cetra_core::span::Span;

    use super::*;
    use crate::QueryOptions;
    use crate::config::{
        FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
    };
    use crate::test_support::{score_row, score_rows, span_row, span_rows};
    use crate::views::QueryContext;

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

    fn window() -> TimeWindow {
        TimeWindow::new(T0, T0 + 10 * MIN).expect("window")
    }

    fn root(trace: u16, start: i64) -> Span {
        span_row(trace, 0, start, start)
    }

    fn in_window(trace: u16) -> Span {
        root(trace, T0 + MIN)
    }

    fn llm_child(parent: &Span, trace: u16, start: i64, duration: i64) -> Span {
        let mut child = span_row(trace, 1, start, start);
        child.span_kind = SpanKind::Llm;
        child.parent_span_id = Some(parent.span_id);
        child.end_time = start + duration;
        child
    }

    fn mk_score(id: u128, trace: u16, created_at: i64, name: &str, value: ScoreValue) -> Score {
        let mut s = score_row(id, trace, created_at, created_at, created_at, false, name);
        s.value = value;
        s
    }

    fn assert_alert(
        actual: AlertValue,
        expected_count: i64,
        expected_value: Option<f64>,
        tol_rel: f64,
    ) {
        assert_eq!(actual.count, expected_count);
        match (actual.value, expected_value) {
            (None, None) => {}
            (Some(a), Some(e)) => {
                let bound = (e.abs() * tol_rel).max(1e-9);
                assert!(
                    (a - e).abs() <= bound,
                    "alert value mismatch: actual={a}, expected={e}, bound={bound}"
                );
            }
            other => panic!("alert value presence mismatch: {other:?}"),
        }
    }

    #[tokio::test]
    async fn all_seven_alert_measures_and_empty_cases() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = crate::Storage::open(test_config(dir.path()))
            .await
            .expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");

        let mut spans = Vec::new();
        // 4 normal traces in window, 1 with error, with costs and varying durations.
        for t in 0..4_u16 {
            let mut s = in_window(t);
            s.end_time = s.start_time + (i64::from(t) + 1) * 10_000_000;
            s.environment = Some(if t < 2 { "prod" } else { "staging" }.to_owned());
            s.cost_total = Some(0.1 * f64::from(t + 1));
            if t == 1 {
                s.status = SpanStatus::Error;
            }
            let child = llm_child(
                &s,
                t,
                T0 + MIN + i64::from(t) * 1_000,
                (i64::from(t) + 1) * 5_000_000,
            );
            spans.push(s);
            spans.push(child);
        }
        // Early trace starting 2h before window, ending inside window, with an llm span inside window.
        let mut early = root(10, T0 - 2 * 60 * MIN);
        early.end_time = T0 + 2 * MIN;
        early.cost_total = Some(1.0);
        let early_llm = llm_child(&early, 10, T0 + 2 * MIN, 20_000_000);
        spans.push(early);
        spans.push(early_llm);

        // Eval trace and experiment trace in window (must be excluded unless filter names environment).
        let mut eval_s = in_window(20);
        eval_s.environment = Some(environment::EVAL.to_owned());
        eval_s.status = SpanStatus::Error;
        eval_s.cost_total = Some(99.0);
        spans.push(eval_s);

        let mut exp_s = in_window(21);
        exp_s
            .metadata
            .insert(metadata_key::EXPERIMENT_ID.to_owned(), "exp-1".to_owned());
        exp_s.cost_total = Some(88.0);
        spans.push(exp_s);

        // Scores: numeric, categorical with value, categorical without value, boolean, issue, and on eval/early traces.
        let mut sc_issue = mk_score(
            9,
            0,
            T0 + MIN,
            "helpfulness",
            ScoreValue::Text("bad".to_owned()),
        );
        sc_issue.source = ScoreSource::Issue;
        let mut sc_session = mk_score(10, 0, T0 + MIN, "session_q", ScoreValue::Numeric(1.0));
        sc_session.target = ScoreTarget::Session {
            session_id: "sess-1".to_owned(),
        };
        let scores = vec![
            mk_score(1, 0, T0 + MIN, "helpfulness", ScoreValue::Numeric(0.2)),
            mk_score(2, 1, T0 + MIN, "helpfulness", ScoreValue::Numeric(0.8)),
            mk_score(
                3,
                2,
                T0 + MIN,
                "helpfulness",
                ScoreValue::Categorical {
                    label: "mid".to_owned(),
                    value: Some(0.5),
                },
            ),
            mk_score(4, 10, T0 + MIN, "helpfulness", ScoreValue::Numeric(0.9)),
            mk_score(5, 20, T0 + MIN, "helpfulness", ScoreValue::Numeric(0.0)), // eval -> excluded
            mk_score(6, 0, T0 + MIN, "ok", ScoreValue::Boolean(true)),
            mk_score(7, 1, T0 + MIN, "ok", ScoreValue::Boolean(false)),
            mk_score(8, 2, T0 + MIN, "ok", ScoreValue::Boolean(true)),
            sc_issue,
            sc_session,
        ];

        storage
            .append_spans(project, span_rows(&spans))
            .await
            .expect("append spans");
        storage
            .append_scores(project, score_rows(&scores))
            .await
            .expect("append scores");

        let opts = QueryOptions::new(None, None);
        let ctx = QueryContext::new(storage.query(project, opts).await.expect("query"), None)
            .expect("ctx");

        let measures = vec![
            (AlertMeasure::ErrorRate, None, 4, Some(0.25), 1e-9),
            (
                AlertMeasure::ErrorRate,
                Some("environment = 'prod'"),
                2,
                Some(0.5),
                1e-9,
            ),
            (
                AlertMeasure::ErrorRate,
                Some("environment = 'cetra-eval'"),
                1,
                Some(1.0),
                1e-9,
            ),
            (AlertMeasure::Cost, None, 4, Some(1.0), 1e-9),
            (
                AlertMeasure::Cost,
                Some("environment = 'prod'"),
                2,
                Some(0.3),
                1e-9,
            ),
            (AlertMeasure::TraceLatencyP95, None, 4, Some(40.0), 0.15),
            (
                AlertMeasure::TraceLatencyP95,
                Some("score('helpfulness') > 0.5"),
                1,
                Some(20.0),
                1e-9,
            ),
            (AlertMeasure::LlmLatencyP95, None, 5, Some(20.0), 0.15),
            (
                AlertMeasure::LlmLatencyP95,
                Some("environment = 'prod'"),
                2,
                Some(10.0),
                0.15,
            ),
            (
                AlertMeasure::LlmLatencyP95,
                Some("score('helpfulness') > 0.5"),
                2,
                Some(20.0),
                0.15,
            ),
            (
                AlertMeasure::ScoreAvg {
                    name: "helpfulness".to_owned(),
                },
                None,
                4,
                Some((0.2 + 0.8 + 0.5 + 0.9) / 4.0),
                1e-9,
            ),
            (
                AlertMeasure::ScoreAvg {
                    name: "helpfulness".to_owned(),
                },
                Some("environment = 'prod'"),
                2,
                Some(0.5),
                1e-9,
            ),
            (
                AlertMeasure::ScoreAvg {
                    name: "helpfulness".to_owned(),
                },
                Some("score('ok') = 1"),
                2,
                Some((0.2 + 0.5) / 2.0),
                1e-9,
            ),
            (
                AlertMeasure::ScorePassRate {
                    name: "ok".to_owned(),
                },
                None,
                3,
                Some(2.0 / 3.0),
                1e-9,
            ),
            (
                AlertMeasure::ScorePassRate {
                    name: "ok".to_owned(),
                },
                Some("score('helpfulness') > 0.5"),
                1,
                Some(0.0),
                1e-9,
            ),
            (
                AlertMeasure::ScoreCount {
                    names: vec!["helpfulness".to_owned(), "session_q".to_owned()],
                },
                None,
                7,
                Some(7.0),
                1e-9,
            ),
            (
                AlertMeasure::ScoreCount { names: vec![] },
                None,
                0,
                Some(0.0),
                1e-9,
            ),
        ];

        for (measure, filter, count, val, tol) in measures {
            let req = AlertValueRequest {
                window: window(),
                filter,
                measure: measure.clone(),
                timeout: Duration::from_secs(10),
            };
            let actual = alert_value(&ctx, req).await.expect("alert");
            assert_alert(actual, count, val, tol);
        }

        // Empty window checks: Cost returns Some(0.0), others return None.
        let empty_win = TimeWindow::new(T0 + 100 * MIN, T0 + 110 * MIN).expect("empty win");
        for (measure, expect_val) in [
            (AlertMeasure::ErrorRate, None),
            (AlertMeasure::Cost, Some(0.0)),
            (AlertMeasure::TraceLatencyP95, None),
            (AlertMeasure::LlmLatencyP95, None),
            (
                AlertMeasure::ScoreAvg {
                    name: "helpfulness".to_owned(),
                },
                None,
            ),
            (
                AlertMeasure::ScorePassRate {
                    name: "ok".to_owned(),
                },
                None,
            ),
            (
                AlertMeasure::ScoreCount {
                    names: vec!["helpfulness".to_owned()],
                },
                Some(0.0),
            ),
        ] {
            let req = AlertValueRequest {
                window: empty_win,
                filter: None,
                measure,
                timeout: Duration::from_secs(10),
            };
            let actual = alert_value(&ctx, req).await.expect("empty");
            assert_eq!(actual.count, 0);
            assert_eq!(actual.value, expect_val);
        }

        // ScoreCount forbids filter.
        let err = alert_value(
            &ctx,
            AlertValueRequest {
                window: window(),
                filter: Some("status = 'ok'"),
                measure: AlertMeasure::ScoreCount {
                    names: vec!["ok".to_owned()],
                },
                timeout: Duration::from_secs(10),
            },
        )
        .await
        .expect_err("ScoreCount with filter must fail");
        assert!(matches!(err, RunError::InvalidRequest(_)));

        // Lookback underflow returns InvalidRequest("from is out of range").
        let min_win = TimeWindow::new(i64::MIN + 1, i64::MIN + 100).expect("min win");
        let err = alert_value(
            &ctx,
            AlertValueRequest {
                window: min_win,
                filter: None,
                measure: AlertMeasure::LlmLatencyP95,
                timeout: Duration::from_secs(10),
            },
        )
        .await
        .expect_err("underflow");
        assert!(matches!(err, RunError::InvalidRequest(ref m) if m == "from is out of range"));

        let _ = storage.shutdown().await;
    }
}
