//! Native query engine implementations of `trace_targets`, `span_targets`, `session_targets`,
//! `filter_span_targets`, and `filter_session_targets`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use cetra_column::{Batch, Column, DataType, Field, FlatColumn, FlatData, ScalarValue, Schema};
use cetra_core::ids::{SpanId, TraceId};
use cetra_core::schema::{spans, traces};
use cetra_query::exec::streaming::ProjectOp;
use cetra_query::exec::{
    ControlFlow, ExecContext, IdLookupHint, PartitionStream, PushSink, ScanSource, ScanSpec,
    TimeRangeHint, run_pipeline as exec_run_pipeline,
};
use cetra_query::{BinaryOp, Expr, FilterView, QueryError};

use crate::query_error::RunError;
use crate::reads::ListScope;
use crate::reads::columns::{active_rows, internal, raw_string_at, raw_time_at, require_col};
use crate::reads::context::{
    EngineContext, OperatorStream, populate_id_lookups, query_err_to_run,
    rewrite_view_expr_for_raw, run_err_to_query, spans_raw_schema, spans_view_projection,
    spans_view_schema, traces_raw_schema,
};
use crate::reads::scope::{
    SearchPredicate, scoped_spans_spec, scoped_traces_spec, search_predicate, sessions_from_traces,
    sessions_from_traces_projection,
};
use crate::reads::scores_join::{apply_compiled_filter, compile_optional_filter};
use crate::views::{QueryContext, TimeWindow};

/// Most ids one lookup takes (D-AQ-8: an add takes 1–1 000 targets).
pub const MAX_TARGET_IDS: usize = 1_000;

/// A visible trace target and its start time (its target time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceTarget {
    /// The trace id.
    pub trace_id: TraceId,
    /// The trace's merged `start_time`, nanoseconds since the Unix epoch.
    pub start_time: i64,
}

/// A visible span target: its pair and its trace's start time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanTarget {
    /// The trace id.
    pub trace_id: TraceId,
    /// The span id.
    pub span_id: SpanId,
    /// The trace's `start_time`, nanoseconds since the Unix epoch.
    pub trace_start: i64,
}

/// A visible session target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTarget {
    /// The session id.
    pub session_id: String,
    /// Start of the session's earliest visible trace.
    pub start_time: i64,
    /// The session's visible trace ids, sorted, for the deletion-guard check.
    pub trace_ids: Vec<TraceId>,
}

/// A span query for [`filter_span_targets`]: the spans list's filter, search and window.
#[derive(Debug, Clone, Copy)]
pub struct SpanTargetFilter<'a> {
    /// Filter text over `spans`. Empty or whitespace means no filter.
    pub filter: Option<&'a str>,
    /// Full-text search over the spans. Empty or whitespace means no search.
    pub search: Option<&'a str>,
    /// `start_time ∈ [from, to)` of the span itself.
    pub window: TimeWindow,
    /// Keep spans of experiment traces, as the spans list does.
    pub include_experiments: bool,
    /// Most targets returned. The server passes its cap; one more match is reported as `capped`.
    pub limit: usize,
    /// Time allowed for the whole call.
    pub timeout: Duration,
}

/// A session query for [`filter_session_targets`]: the sessions list's filter and window. The
/// sessions list has no search, so this request has none.
#[derive(Debug, Clone, Copy)]
pub struct SessionTargetFilter<'a> {
    /// Filter text over `sessions`. Empty or whitespace means no filter.
    pub filter: Option<&'a str>,
    /// The sessions of traces that start in `[from, to)`, as the sessions list selects them.
    pub window: TimeWindow,
    /// Keep sessions of experiment traces, as the sessions list does.
    pub include_experiments: bool,
    /// Most targets returned. The server passes its cap; one more match is reported as `capped`.
    pub limit: usize,
    /// Time allowed for the whole call.
    pub timeout: Duration,
}

/// Targets found, and whether more than `limit` matched before any target was dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilteredTargets<T> {
    /// At most `limit` targets, sorted by id.
    pub targets: Vec<T>,
    /// True when more than `limit` rows matched the filter.
    pub capped: bool,
}

/// Visible, non-evaluation traces among `ids`, sorted by `TraceId`. Experiments included.
pub async fn trace_targets(
    ctx: &QueryContext,
    ids: &[TraceId],
    timeout: Duration,
) -> Result<Vec<TraceTarget>, RunError> {
    let ctx = ctx.engine();
    check_len(ids.len())?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let ctx_c = ctx.clone();
    let ids_vec = ids.to_vec();
    let (targets, _) = ctx
        .run_with_ctx(timeout, move |exec_ctx| {
            trace_scan_sync(&ctx_c, exec_ctx, &ids_vec).map_err(run_err_to_query)
        })
        .await?;
    Ok(targets)
}

/// Visible spans among `(TraceId, SpanId)` pairs whose trace is a target of [`trace_targets`].
pub async fn span_targets(
    ctx: &QueryContext,
    ids: &[(TraceId, SpanId)],
    timeout: Duration,
) -> Result<Vec<SpanTarget>, RunError> {
    let ctx = ctx.engine();
    check_len(ids.len())?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let ctx_c = ctx.clone();
    let pairs = ids.to_vec();
    let (targets, _) = ctx
        .run_with_ctx(timeout, move |exec_ctx| {
            span_scan_sync(&ctx_c, exec_ctx, &pairs).map_err(run_err_to_query)
        })
        .await?;
    Ok(targets)
}

/// Visible sessions among `ids`, with earliest visible trace start and sorted visible trace IDs.
pub async fn session_targets(
    ctx: &QueryContext,
    ids: &[&str],
    timeout: Duration,
) -> Result<Vec<SessionTarget>, RunError> {
    let ctx = ctx.engine();
    check_len(ids.len())?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let ctx_c = ctx.clone();
    let owned: Vec<String> = ids.iter().map(|s| (*s).to_owned()).collect();
    let (targets, _) = ctx
        .run_with_ctx(timeout, move |exec_ctx| {
            let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
            session_scan_sync(&ctx_c, exec_ctx, &refs).map_err(run_err_to_query)
        })
        .await?;
    Ok(targets)
}

/// Visible spans matching the spans-list filter, search, and window, with trace start times.
pub async fn filter_span_targets(
    ctx: &QueryContext,
    request: SpanTargetFilter<'_>,
) -> Result<FilteredTargets<SpanTarget>, RunError> {
    let ctx = ctx.engine();
    let compiled = compile_optional_filter(FilterView::Spans, request.filter)?;
    let references_environment = compiled.as_ref().is_some_and(|f| f.references_environment);
    let scope = ListScope {
        window: Some(request.window),
        include_eval: false,
        include_experiments: request.include_experiments,
    };
    let search_owned = request.search.map(str::to_owned);
    let window = request.window;
    let limit = request.limit;

    let ctx_c = ctx.clone();
    let (res, _) = ctx
        .run_with_ctx(request.timeout, move |exec_ctx| {
            let mut spec = scoped_spans_spec(&ctx_c, scope, references_environment)
                .map_err(run_err_to_query)?;
            if let Some(text) = search_owned.as_deref() {
                match search_predicate(&ctx_c, exec_ctx, FilterView::Spans, text, Some(window))
                    .map_err(run_err_to_query)?
                {
                    SearchPredicate::None => {}
                    SearchPredicate::Spans { filter, substrings } => {
                        spec.filters.push(filter);
                        if !substrings.is_empty() {
                            spec.substrings.push(substrings);
                        }
                    }
                    SearchPredicate::Traces { .. } => {
                        return Err(QueryError::internal("unexpected trace search on spans"));
                    }
                }
            }

            let spans_raw = spans_raw_schema().map_err(run_err_to_query)?;
            let needs_score_join = compiled.as_ref().is_some_and(|f| !f.scores.is_empty());
            let (streams, is_hex) = if !needs_score_join {
                if let Some(filter) = &compiled {
                    let rewritten = rewrite_view_expr_for_raw(
                        &filter.expr,
                        &spans_raw,
                        &[spans::TRACE_ID, spans::SPAN_ID, spans::PARENT_SPAN_ID],
                        false,
                    )?;
                    spec.filters.push(rewritten);
                    populate_id_lookups(&mut spec);
                }
                let trace_idx = spans_raw
                    .index_of(spans::TRACE_ID)
                    .ok_or_else(|| QueryError::internal("trace_id missing from spans schema"))?;
                let span_idx = spans_raw
                    .index_of(spans::SPAN_ID)
                    .ok_or_else(|| QueryError::internal("span_id missing from spans schema"))?;
                spec.projection = Some(vec![trace_idx, span_idx]);
                let streams = ctx_c.spans.open_partitions(&spec, exec_ctx)?;
                (streams, false)
            } else {
                let raw_streams = ctx_c.spans.open_partitions(&spec, exec_ctx)?;
                if raw_streams.is_empty() {
                    (Vec::new(), true)
                } else {
                    let view_schema = spans_view_schema().map_err(run_err_to_query)?;
                    let view_streams: Vec<Box<dyn PartitionStream>> = raw_streams
                        .into_iter()
                        .map(|s| {
                            let proj = spans_view_projection(&spans_raw, exec_ctx.clone())?;
                            Ok(
                                Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                                    as Box<dyn PartitionStream>,
                            )
                        })
                        .collect::<Result<Vec<_>, QueryError>>()?;
                    let filtered = match &compiled {
                        Some(f) => {
                            apply_compiled_filter(&ctx_c, exec_ctx, view_streams, &view_schema, f)?
                        }
                        None => view_streams,
                    };
                    let pair_schema = Schema::try_new_arc(vec![
                        Field::new(spans::TRACE_ID, DataType::Utf8, false),
                        Field::new(spans::SPAN_ID, DataType::Utf8, false),
                    ])?;
                    let pair_exprs = vec![
                        col_expr(&view_schema, spans::TRACE_ID)?,
                        col_expr(&view_schema, spans::SPAN_ID)?,
                    ];
                    let projected = filtered
                        .into_iter()
                        .map(|s| {
                            let proj = ProjectOp::with_schema(
                                pair_exprs.clone(),
                                Arc::clone(&pair_schema),
                                exec_ctx.clone(),
                            )?;
                            Ok(
                                Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                                    as Box<dyn PartitionStream>,
                            )
                        })
                        .collect::<Result<Vec<_>, QueryError>>()?;
                    (projected, true)
                }
            };

            let mut pairs = BTreeSet::new();
            if !streams.is_empty() {
                let sinks = exec_run_pipeline(
                    exec_ctx,
                    streams,
                    |_| Ok(Vec::new()),
                    move |_| Ok(SpanPairSink::new(limit, is_hex)),
                )?;
                for sink in sinks {
                    pairs.extend(sink.pairs);
                }
            }

            let capped = pairs.len() > limit;
            let kept: Vec<(TraceId, SpanId)> = pairs.into_iter().take(limit).collect();
            let trace_ids: Vec<TraceId> = kept
                .iter()
                .map(|(trace_id, _)| *trace_id)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();

            // Second-stage `trace_scan_sync` ALWAYS forces `include_eval = false,
            // include_experiments = true` even if the span filter named `environment`.
            let mut starts = BTreeMap::new();
            for chunk in trace_ids.chunks(MAX_TARGET_IDS) {
                for target in trace_scan_sync(&ctx_c, exec_ctx, chunk).map_err(run_err_to_query)? {
                    starts.insert(target.trace_id, target.start_time);
                }
            }
            let targets = kept
                .into_iter()
                .filter_map(|(trace_id, span_id)| {
                    starts.get(&trace_id).map(|&trace_start| SpanTarget {
                        trace_id,
                        span_id,
                        trace_start,
                    })
                })
                .collect();
            Ok(FilteredTargets { targets, capped })
        })
        .await?;

    Ok(res)
}

/// Visible sessions matching the sessions-list filter and window.
pub async fn filter_session_targets(
    ctx: &QueryContext,
    request: SessionTargetFilter<'_>,
) -> Result<FilteredTargets<SessionTarget>, RunError> {
    let ctx = ctx.engine();
    let compiled = compile_optional_filter(FilterView::Sessions, request.filter)?;
    let scope = ListScope {
        window: Some(request.window),
        include_eval: false,
        include_experiments: request.include_experiments,
    };
    let mut spec = scoped_traces_spec(ctx, scope, false)?;
    let traces_raw = traces_raw_schema()?;
    spec.projection = Some(sessions_from_traces_projection(&traces_raw)?);
    let limit = request.limit;

    let ctx_c = ctx.clone();
    let (res, _) = ctx
        .run_with_ctx(request.timeout, move |exec_ctx| {
            let proj_schema = ctx_c.traces.projected_schema(&spec)?;
            let raw_streams = ctx_c.traces.open_partitions(&spec, exec_ctx)?;
            let mut ids = Vec::new();
            if !raw_streams.is_empty() {
                let sessions = sessions_from_traces(raw_streams, &proj_schema, exec_ctx)?;
                let streams = match &compiled {
                    Some(f) => apply_compiled_filter(
                        &ctx_c,
                        exec_ctx,
                        sessions.streams,
                        &sessions.schema,
                        f,
                    )?,
                    None => sessions.streams,
                };
                if !streams.is_empty() {
                    let sid_schema = Schema::try_new_arc(vec![Field::new(
                        traces::SESSION_ID,
                        DataType::Utf8,
                        false,
                    )])?;
                    let sid_expr = col_expr(&sessions.schema, traces::SESSION_ID)?;
                    let projected: Vec<Box<dyn PartitionStream>> = streams
                        .into_iter()
                        .map(|s| {
                            let proj = ProjectOp::with_schema(
                                vec![sid_expr.clone()],
                                Arc::clone(&sid_schema),
                                exec_ctx.clone(),
                            )?;
                            Ok(
                                Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                                    as Box<dyn PartitionStream>,
                            )
                        })
                        .collect::<Result<Vec<_>, QueryError>>()?;
                    let sinks = exec_run_pipeline(
                        exec_ctx,
                        projected,
                        |_| Ok(Vec::new()),
                        move |_| Ok(DistinctSessionIdSink::new(limit.saturating_add(1))),
                    )?;
                    let mut found = BTreeSet::new();
                    for sink in sinks {
                        found.extend(sink.ids);
                    }
                    ids = found.into_iter().collect();
                }
            }

            let capped = ids.len() > limit;
            ids.truncate(limit);
            let mut targets = Vec::new();
            for chunk in ids.chunks(MAX_TARGET_IDS) {
                let refs: Vec<&str> = chunk.iter().map(String::as_str).collect();
                targets
                    .extend(session_scan_sync(&ctx_c, exec_ctx, &refs).map_err(run_err_to_query)?);
            }
            targets.sort_by(|a, b| a.session_id.cmp(&b.session_id));
            Ok(FilteredTargets { targets, capped })
        })
        .await?;

    Ok(res)
}

struct SpanPairSink {
    pairs: BTreeSet<(TraceId, SpanId)>,
    limit: usize,
    is_hex: bool,
}

impl SpanPairSink {
    fn new(limit: usize, is_hex: bool) -> Self {
        Self {
            pairs: BTreeSet::new(),
            limit,
            is_hex,
        }
    }
}

impl PushSink for SpanPairSink {
    fn consume(&mut self, batch: Batch) -> Result<ControlFlow, QueryError> {
        if batch.num_rows() == 0 {
            return Ok(ControlFlow::Continue);
        }
        let trace_col = require_col(&batch, spans::TRACE_ID).map_err(run_err_to_query)?;
        let span_col = require_col(&batch, spans::SPAN_ID).map_err(run_err_to_query)?;
        for phys in active_rows(&batch) {
            let (trace_id, span_id) = if self.is_hex {
                let t = raw_string_at(trace_col, phys)
                    .map_err(run_err_to_query)?
                    .ok_or_else(|| QueryError::internal("null trace_id in spans view"))?;
                let s = raw_string_at(span_col, phys)
                    .map_err(run_err_to_query)?
                    .ok_or_else(|| QueryError::internal("null span_id in spans view"))?;
                (
                    TraceId::from_str(&t).map_err(|_| bad_id())?,
                    SpanId::from_str(&s).map_err(|_| bad_id())?,
                )
            } else {
                let t = raw_fixed16_at(trace_col, phys).ok_or_else(bad_id)?;
                let s = raw_fixed8_at(span_col, phys).ok_or_else(bad_id)?;
                (
                    TraceId::from_bytes(t).map_err(|_| bad_id())?,
                    SpanId::from_bytes(s).map_err(|_| bad_id())?,
                )
            };
            self.pairs.insert((trace_id, span_id));
            if self.pairs.len() > self.limit {
                return Ok(ControlFlow::Stop);
            }
        }
        Ok(ControlFlow::Continue)
    }

    fn finish(&mut self) -> Result<(), QueryError> {
        Ok(())
    }
}

struct DistinctSessionIdSink {
    ids: BTreeSet<String>,
    stop_at: usize,
}

impl DistinctSessionIdSink {
    fn new(stop_at: usize) -> Self {
        Self {
            ids: BTreeSet::new(),
            stop_at,
        }
    }
}

impl PushSink for DistinctSessionIdSink {
    fn consume(&mut self, batch: Batch) -> Result<ControlFlow, QueryError> {
        if batch.num_rows() == 0 {
            return Ok(ControlFlow::Continue);
        }
        let col = require_col(&batch, traces::SESSION_ID).map_err(run_err_to_query)?;
        for phys in active_rows(&batch) {
            if let Some(sid) = raw_string_at(col, phys).map_err(run_err_to_query)? {
                self.ids.insert(sid);
                if self.ids.len() >= self.stop_at {
                    return Ok(ControlFlow::Stop);
                }
            }
        }
        Ok(ControlFlow::Continue)
    }

    fn finish(&mut self) -> Result<(), QueryError> {
        Ok(())
    }
}

fn trace_scan_sync(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    ids: &[TraceId],
) -> Result<Vec<TraceTarget>, RunError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let scope = ListScope {
        window: None,
        include_eval: false,
        include_experiments: true,
    };
    let mut spec = scoped_traces_spec(ctx, scope, false)?;
    let traces_raw = traces_raw_schema()?;
    let trace_idx = traces_raw
        .index_of(traces::TRACE_ID)
        .ok_or_else(|| internal("trace_id missing from traces schema"))?;
    let start_idx = traces_raw
        .index_of(traces::START_TIME)
        .ok_or_else(|| internal("start_time missing from traces schema"))?;

    let distinct: Vec<[u8; 16]> = ids
        .iter()
        .map(|id| *id.as_bytes())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let trace_col = col_expr(&traces_raw, traces::TRACE_ID).map_err(query_err_to_run)?;
    spec.filters.push(Expr::InList {
        expr: Box::new(trace_col),
        list: distinct
            .iter()
            .map(|b| Expr::Literal(ScalarValue::FixedBytes16(*b)))
            .collect(),
        negated: false,
        redacted: false,
    });
    spec.id_lookups.push(IdLookupHint::Fixed16 {
        column: traces::TRACE_ID.to_owned(),
        values: distinct,
    });
    spec.projection = Some(vec![trace_idx, start_idx]);

    let streams = ctx
        .traces
        .open_partitions(&spec, exec_ctx)
        .map_err(query_err_to_run)?;
    let batches = collect_streams(streams, exec_ctx).map_err(query_err_to_run)?;

    let mut found = BTreeMap::new();
    for batch in &batches {
        if batch.num_rows() == 0 {
            continue;
        }
        let id_col = require_col(batch, traces::TRACE_ID)?;
        let start_col = require_col(batch, traces::START_TIME)?;
        for phys in active_rows(batch) {
            let bytes = raw_fixed16_at(id_col, phys).ok_or_else(bad_id_run)?;
            let trace_id = TraceId::from_bytes(bytes).map_err(|_| bad_id_run())?;
            let start_time = raw_time_at(start_col, phys)?;
            found.insert(trace_id, start_time);
        }
    }
    Ok(found
        .into_iter()
        .map(|(trace_id, start_time)| TraceTarget {
            trace_id,
            start_time,
        })
        .collect())
}

fn span_scan_sync(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    ids: &[(TraceId, SpanId)],
) -> Result<Vec<SpanTarget>, RunError> {
    let requested: HashSet<(TraceId, SpanId)> = ids.iter().copied().collect();
    let trace_ids: Vec<TraceId> = ids
        .iter()
        .map(|(t, _)| *t)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let targets = trace_scan_sync(ctx, exec_ctx, &trace_ids)?;
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    let starts: BTreeMap<TraceId, i64> =
        targets.iter().map(|t| (t.trace_id, t.start_time)).collect();
    let trace_vec: Vec<[u8; 16]> = targets.iter().map(|t| *t.trace_id.as_bytes()).collect();
    let span_vec: Vec<[u8; 8]> = ids
        .iter()
        .filter(|(t, _)| starts.contains_key(t))
        .map(|(_, s)| *s.as_bytes())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let spans_raw = spans_raw_schema()?;
    let trace_idx = spans_raw
        .index_of(spans::TRACE_ID)
        .ok_or_else(|| internal("trace_id missing from spans schema"))?;
    let span_idx = spans_raw
        .index_of(spans::SPAN_ID)
        .ok_or_else(|| internal("span_id missing from spans schema"))?;

    let mut filters = vec![
        Expr::InList {
            expr: Box::new(col_expr(&spans_raw, spans::TRACE_ID).map_err(query_err_to_run)?),
            list: trace_vec
                .iter()
                .map(|b| Expr::Literal(ScalarValue::FixedBytes16(*b)))
                .collect(),
            negated: false,
            redacted: false,
        },
        Expr::InList {
            expr: Box::new(col_expr(&spans_raw, spans::SPAN_ID).map_err(query_err_to_run)?),
            list: span_vec
                .iter()
                .map(|b| Expr::Literal(ScalarValue::FixedBytes8(*b)))
                .collect(),
            negated: false,
            redacted: false,
        },
    ];
    if let Some(vf) = ctx.visible_from {
        let start_col = col_expr(&spans_raw, spans::START_TIME).map_err(query_err_to_run)?;
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

    let spec = ScanSpec {
        projection: Some(vec![trace_idx, span_idx]),
        map_projections: Default::default(),
        time_range: TimeRangeHint {
            min_ns: ctx.visible_from,
            max_ns: None,
            around_ns: ctx.around,
        },
        id_lookups: vec![
            IdLookupHint::Fixed16 {
                column: spans::TRACE_ID.to_owned(),
                values: trace_vec,
            },
            IdLookupHint::Fixed8 {
                column: spans::SPAN_ID.to_owned(),
                values: span_vec,
            },
        ],
        filters,
        substrings: Vec::new(),
        sort_hints: Vec::new(),
        limit: None,
    };

    let streams = ctx
        .spans
        .open_partitions(&spec, exec_ctx)
        .map_err(query_err_to_run)?;
    let batches = collect_streams(streams, exec_ctx).map_err(query_err_to_run)?;

    let mut found = BTreeMap::new();
    for batch in &batches {
        if batch.num_rows() == 0 {
            continue;
        }
        let trace_col = require_col(batch, spans::TRACE_ID)?;
        let span_col = require_col(batch, spans::SPAN_ID)?;
        for phys in active_rows(batch) {
            let t_bytes = raw_fixed16_at(trace_col, phys).ok_or_else(bad_id_run)?;
            let s_bytes = raw_fixed8_at(span_col, phys).ok_or_else(bad_id_run)?;
            let trace_id = TraceId::from_bytes(t_bytes).map_err(|_| bad_id_run())?;
            let span_id = SpanId::from_bytes(s_bytes).map_err(|_| bad_id_run())?;
            if !requested.contains(&(trace_id, span_id)) {
                continue;
            }
            let Some(&trace_start) = starts.get(&trace_id) else {
                continue;
            };
            found.insert((trace_id, span_id), trace_start);
        }
    }
    Ok(found
        .into_iter()
        .map(|((trace_id, span_id), trace_start)| SpanTarget {
            trace_id,
            span_id,
            trace_start,
        })
        .collect())
}

fn session_scan_sync(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    ids: &[&str],
) -> Result<Vec<SessionTarget>, RunError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let scope = ListScope {
        window: None,
        include_eval: false,
        include_experiments: true,
    };
    let mut spec = scoped_traces_spec(ctx, scope, false)?;
    let traces_raw = traces_raw_schema()?;
    let sid_idx = traces_raw
        .index_of(traces::SESSION_ID)
        .ok_or_else(|| internal("session_id missing from traces schema"))?;
    let tid_idx = traces_raw
        .index_of(traces::TRACE_ID)
        .ok_or_else(|| internal("trace_id missing from traces schema"))?;
    let start_idx = traces_raw
        .index_of(traces::START_TIME)
        .ok_or_else(|| internal("start_time missing from traces schema"))?;

    let distinct: Vec<String> = ids
        .iter()
        .map(|s| (*s).to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let sid_col = col_expr(&traces_raw, traces::SESSION_ID).map_err(query_err_to_run)?;
    spec.filters.push(Expr::InList {
        expr: Box::new(sid_col),
        list: distinct
            .iter()
            .map(|id| Expr::Literal(ScalarValue::Utf8(id.clone())))
            .collect(),
        negated: false,
        redacted: false,
    });
    spec.id_lookups.push(IdLookupHint::Utf8 {
        column: traces::SESSION_ID.to_owned(),
        values: distinct,
    });
    spec.projection = Some(vec![sid_idx, tid_idx, start_idx]);

    let streams = ctx
        .traces
        .open_partitions(&spec, exec_ctx)
        .map_err(query_err_to_run)?;
    let batches = collect_streams(streams, exec_ctx).map_err(query_err_to_run)?;

    let mut found: BTreeMap<String, (i64, BTreeSet<TraceId>)> = BTreeMap::new();
    for batch in &batches {
        if batch.num_rows() == 0 {
            continue;
        }
        let sid_col = require_col(batch, traces::SESSION_ID)?;
        let tid_col = require_col(batch, traces::TRACE_ID)?;
        let start_col = require_col(batch, traces::START_TIME)?;
        for phys in active_rows(batch) {
            let Some(sid) = raw_string_at(sid_col, phys)? else {
                continue;
            };
            let tid_bytes = raw_fixed16_at(tid_col, phys).ok_or_else(bad_id_run)?;
            let trace_id = TraceId::from_bytes(tid_bytes).map_err(|_| bad_id_run())?;
            let start = raw_time_at(start_col, phys)?;
            found
                .entry(sid)
                .and_modify(|(earliest, members)| {
                    *earliest = (*earliest).min(start);
                    members.insert(trace_id);
                })
                .or_insert_with(|| (start, BTreeSet::from([trace_id])));
        }
    }
    Ok(found
        .into_iter()
        .map(|(session_id, (start_time, trace_ids))| SessionTarget {
            session_id,
            start_time,
            trace_ids: trace_ids.into_iter().collect(),
        })
        .collect())
}

fn collect_streams(
    streams: Vec<Box<dyn PartitionStream>>,
    exec_ctx: &ExecContext,
) -> Result<Vec<Batch>, QueryError> {
    if streams.is_empty() {
        return Ok(Vec::new());
    }
    let pool_c = Arc::clone(&exec_ctx.pool);
    let sinks = exec_run_pipeline(
        exec_ctx,
        streams,
        |_| Ok(Vec::new()),
        move |_| Ok(cetra_query::exec::CollectSink::new(&pool_c)),
    )?;
    Ok(sinks
        .into_iter()
        .flat_map(cetra_query::exec::CollectSink::into_batches)
        .collect())
}

fn check_len(len: usize) -> Result<(), RunError> {
    if len > MAX_TARGET_IDS {
        return Err(RunError::InvalidRequest(format!(
            "at most {MAX_TARGET_IDS} target ids"
        )));
    }
    Ok(())
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

fn bad_id() -> QueryError {
    QueryError::internal("a view returned an invalid trace or span id")
}

fn bad_id_run() -> RunError {
    internal("a view returned an invalid trace id")
}

/// Reads an optional 16-byte binary ID from `col` at physical row `phys_row`.
pub(super) fn raw_fixed16_at(col: &Column, phys_row: usize) -> Option<[u8; 16]> {
    if phys_row >= col.len() || col.is_null(phys_row) {
        return None;
    }
    match col {
        Column::Const { value, .. } => match value {
            ScalarValue::FixedBytes16(b) => Some(*b),
            _ => None,
        },
        Column::Flat(fc) => read_flat_fixed16(fc, phys_row),
        Column::Dict(d) => {
            let code = *d.codes().get(phys_row)? as usize;
            read_flat_fixed16(d.values(), code)
        }
        Column::Rle(r) => {
            let run = r
                .run_ends()
                .partition_point(|&end| (end as usize) <= phys_row);
            read_flat_fixed16(r.values(), run)
        }
    }
}

fn read_flat_fixed16(fc: &FlatColumn, idx: usize) -> Option<[u8; 16]> {
    if idx >= fc.len() || fc.is_null(idx) {
        return None;
    }
    match fc.data() {
        FlatData::FixedBytes16(v) => v.get(idx).copied(),
        _ => None,
    }
}

/// Reads an optional 8-byte binary ID from `col` at physical row `phys_row`.
pub(super) fn raw_fixed8_at(col: &Column, phys_row: usize) -> Option<[u8; 8]> {
    if phys_row >= col.len() || col.is_null(phys_row) {
        return None;
    }
    match col {
        Column::Const { value, .. } => match value {
            ScalarValue::FixedBytes8(b) => Some(*b),
            _ => None,
        },
        Column::Flat(fc) => read_flat_fixed8(fc, phys_row),
        Column::Dict(d) => {
            let code = *d.codes().get(phys_row)? as usize;
            read_flat_fixed8(d.values(), code)
        }
        Column::Rle(r) => {
            let run = r
                .run_ends()
                .partition_point(|&end| (end as usize) <= phys_row);
            read_flat_fixed8(r.values(), run)
        }
    }
}

fn read_flat_fixed8(fc: &FlatColumn, idx: usize) -> Option<[u8; 8]> {
    if idx >= fc.len() || fc.is_null(idx) {
        return None;
    }
    match fc.data() {
        FlatData::FixedBytes8(v) => v.get(idx).copied(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytesize::ByteSize;
    use cetra_core::ids::{ProjectId, SpanId, TraceId};
    use cetra_core::names::{environment, metadata_key};
    use cetra_core::span::Span;

    use super::*;
    use crate::QueryOptions;
    use crate::config::{
        FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
    };
    use crate::test_support::{span_row, span_rows};
    use crate::views::{QueryContext, TimeWindow};

    const TIMEOUT: Duration = Duration::from_secs(30);

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

    fn id(trace: u16) -> TraceId {
        span_row(trace, 0, 0, 0).trace_id
    }

    fn sid(trace: u16, span: u8) -> SpanId {
        span_row(trace, span, 0, 0).span_id
    }

    #[tokio::test]
    async fn targets_lookup_and_filter_span_env_second_stage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = crate::Storage::open(test_config(dir.path()))
            .await
            .expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");

        // Trace 1: root at 1_000 + child at 1_600 (session s1)
        let mut r1 = span_row(1, 0, 1_000, 10);
        r1.session_id = Some("s1".to_owned());
        r1.name = "chat".to_owned();
        let mut c1 = span_row(1, 1, 1_600, 20);
        c1.parent_span_id = Some(r1.span_id);
        c1.session_id = Some("s1".to_owned());
        c1.name = "chat".to_owned();
        c1.input = Some("refund please".to_owned());
        // Duplicate copy of c1
        let c1_dup = span_row(1, 1, 1_600, 30);

        // Trace 2: eval trace (must be excluded by trace_targets and second-stage filter_span_targets!)
        let mut r2 = span_row(2, 0, 2_000, 10);
        r2.environment = Some(environment::EVAL.to_owned());
        r2.session_id = Some("s1".to_owned());
        r2.name = "chat".to_owned();

        // Trace 3: experiment trace (included by trace_targets!)
        let mut r3 = span_row(3, 0, 3_000, 10);
        r3.metadata
            .insert(metadata_key::EXPERIMENT_ID.to_owned(), "e1".to_owned());
        r3.session_id = Some("s2".to_owned());

        let spans: Vec<Span> = vec![r1, c1, c1_dup, r2, r3];
        storage
            .append_spans(project, span_rows(&spans))
            .await
            .expect("append");

        let opts = QueryOptions::new(None, None);
        let pq = storage.query(project, opts).await.expect("pq");
        let ctx = QueryContext::new(pq, None).expect("ctx");

        // 1. trace_targets: includes trace 1 and experiment trace 3, excludes eval trace 2.
        let t_ids = [id(3), id(2), id(1), id(99), id(1)];
        let mut expected_traces = vec![
            TraceTarget {
                trace_id: id(1),
                start_time: 1_000,
            },
            TraceTarget {
                trace_id: id(3),
                start_time: 3_000,
            },
        ];
        expected_traces.sort_by_key(|t| *t.trace_id.as_bytes());
        assert_eq!(
            trace_targets(&ctx, &t_ids, TIMEOUT)
                .await
                .expect("trace_targets"),
            expected_traces
        );

        // 2. span_targets: excludes eval trace 2 and nonexistent span 9.
        let s_pairs = [(id(1), sid(1, 1)), (id(2), sid(2, 0)), (id(1), sid(1, 9))];
        assert_eq!(
            span_targets(&ctx, &s_pairs, TIMEOUT)
                .await
                .expect("span_targets"),
            vec![SpanTarget {
                trace_id: id(1),
                span_id: sid(1, 1),
                trace_start: 1_000,
            }]
        );

        // 3. session_targets
        assert_eq!(
            session_targets(&ctx, &["s2", "s1", "nope"], TIMEOUT)
                .await
                .expect("session_targets"),
            vec![
                SessionTarget {
                    session_id: "s1".to_owned(),
                    start_time: 1_000,
                    trace_ids: vec![id(1)],
                },
                SessionTarget {
                    session_id: "s2".to_owned(),
                    start_time: 3_000,
                    trace_ids: vec![id(3)],
                },
            ]
        );

        // 4. filter_span_targets with `environment = 'cetra-eval'` filter:
        // Stage 1 matches r2, Stage 2 trace_scan forces include_eval = false -> r2 dropped!
        let win = TimeWindow::new(0, 10_000).expect("win");
        let eval_dropped = filter_span_targets(
            &ctx,
            SpanTargetFilter {
                filter: Some("environment = 'cetra-eval'"),
                search: None,
                window: win,
                include_experiments: false,
                limit: 10,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("eval_dropped");
        assert!(eval_dropped.targets.is_empty());
        assert!(!eval_dropped.capped);

        let search_hit = filter_span_targets(
            &ctx,
            SpanTargetFilter {
                filter: Some("name = 'chat'"),
                search: Some("refund"),
                window: win,
                include_experiments: false,
                limit: 10,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("search_hit");
        assert_eq!(
            search_hit.targets,
            vec![SpanTarget {
                trace_id: id(1),
                span_id: sid(1, 1),
                trace_start: 1_000,
            }]
        );
        assert!(!search_hit.capped);

        let capped = filter_span_targets(
            &ctx,
            SpanTargetFilter {
                filter: None,
                search: None,
                window: win,
                include_experiments: true,
                limit: 2,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("capped");
        assert_eq!(capped.targets.len(), 2);
        assert!(capped.capped);

        let limit_zero = filter_span_targets(
            &ctx,
            SpanTargetFilter {
                filter: None,
                search: None,
                window: win,
                include_experiments: false,
                limit: 0,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("limit_zero");
        assert!(limit_zero.targets.is_empty());
        assert!(limit_zero.capped);

        // 5. filter_session_targets
        let s_none = filter_session_targets(
            &ctx,
            SessionTargetFilter {
                filter: Some("trace_count >= 1"),
                window: TimeWindow::new(1_500, 10_000).expect("win"),
                include_experiments: false,
                limit: 10,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("s_none");
        assert!(s_none.targets.is_empty());
        assert!(!s_none.capped);

        let s_exp = filter_session_targets(
            &ctx,
            SessionTargetFilter {
                filter: None,
                window: TimeWindow::new(1_500, 10_000).expect("win"),
                include_experiments: true,
                limit: 1,
                timeout: TIMEOUT,
            },
        )
        .await
        .expect("s_exp");
        assert_eq!(
            s_exp.targets,
            vec![SessionTarget {
                session_id: "s2".to_owned(),
                start_time: 3_000,
                trace_ids: vec![id(3)],
            }]
        );
        assert!(!s_exp.capped);

        // Validation limits
        let too_many = vec![id(1); MAX_TARGET_IDS + 1];
        assert!(matches!(
            trace_targets(&ctx, &too_many, TIMEOUT).await,
            Err(RunError::InvalidRequest(_))
        ));

        let _ = storage.shutdown().await;
    }
}
