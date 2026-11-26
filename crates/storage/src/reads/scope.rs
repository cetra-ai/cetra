//! List-query scoping: time windows, `visible_from`, `around`, default & strict exclusions,
//! full-text search, `trace_id_semi_join`, and `sessions_from_traces`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use cetra_column::{DataType, Field, ScalarValue, Schema};
use cetra_core::names::{environment, metadata_key};
use cetra_core::schema::{spans, traces};
use cetra_query::exec::hash_join::{HashJoinSpec, JoinType, execute_hash_join};
use cetra_query::exec::streaming::{FilterOp, ProjectOp};
use cetra_query::exec::{
    AggExpr, AggFunc, ExecContext, HashAgg, HashAggSpec, IdLookupHint, MapProjection,
    PartitionStream, ScanSource, ScanSpec, SortKey, SubstringHint, TimeRangeHint,
    run_pipeline as exec_run_pipeline,
};
use cetra_query::{BinaryOp, CastMode, Expr, FilterView, QueryError, ScalarFn, UnaryOp};

use crate::query_error::RunError;
use crate::reads::ListScope;
use crate::reads::context::{
    EngineContext, OperatorStream, duration_ms_from_ns_expr, populate_id_lookups,
    rewrite_view_expr_for_raw, run_err_to_query, sessions_view_schema, spans_raw_schema,
    traces_raw_schema, traces_view_projection_with_raw_trace_id,
};
use crate::reads::scores_join::{apply_compiled_filter, compile_optional_filter};
use crate::views::TimeWindow;

/// A set of partition streams paired with their shared `cetra_column::Schema`.
pub(crate) struct StreamSet {
    pub(crate) streams: Vec<Box<dyn PartitionStream>>,
    pub(crate) schema: Arc<Schema>,
}

/// Result of [`search_predicate`] for a list query.
pub(crate) enum SearchPredicate {
    /// Empty or whitespace-only search query.
    None,
    /// Span search predicate and substring hints to attach to a `spans` `ScanSpec`.
    Spans {
        filter: Expr,
        substrings: Vec<SubstringHint>,
    },
    /// Windowed `spans` `trace_id` streams (`FixedBytes16`) to semi-join onto `traces`.
    Traces {
        trace_ids: Vec<Box<dyn PartitionStream>>,
        schema: Arc<Schema>,
    },
}

/// Builds a `ScanSpec` over `ctx.traces` (raw schema) applying `scope.window`, `ctx.visible_from`,
/// `ctx.around`, and default exclusions.
pub(crate) fn scoped_traces_spec(
    ctx: &EngineContext,
    scope: ListScope,
    references_environment: bool,
) -> Result<ScanSpec, RunError> {
    let raw = traces_raw_schema()?;
    scoped_spec_for_table(
        &raw,
        traces::START_TIME,
        traces::ENVIRONMENT,
        traces::METADATA,
        ctx,
        scope,
        references_environment,
    )
}

/// Builds a `ScanSpec` over `ctx.spans` (raw schema) applying `scope.window`, `ctx.visible_from`,
/// `ctx.around`, and default exclusions.
pub(crate) fn scoped_spans_spec(
    ctx: &EngineContext,
    scope: ListScope,
    references_environment: bool,
) -> Result<ScanSpec, RunError> {
    let raw = spans_raw_schema()?;
    scoped_spec_for_table(
        &raw,
        spans::START_TIME,
        spans::ENVIRONMENT,
        spans::METADATA,
        ctx,
        scope,
        references_environment,
    )
}

fn scoped_spec_for_table(
    raw: &Schema,
    time_col_name: &str,
    env_col_name: &str,
    meta_col_name: &str,
    ctx: &EngineContext,
    scope: ListScope,
    references_environment: bool,
) -> Result<ScanSpec, RunError> {
    let time_col = col_expr(raw, time_col_name).map_err(crate::reads::context::query_err_to_run)?;
    let mut filters = Vec::new();
    let mut min_ns = ctx.visible_from;
    let mut max_ns = None;

    if let Some(window) = scope.window {
        min_ns = Some(min_ns.map_or(window.from(), |vf| vf.max(window.from())));
        // `TimeWindow` is `[from, to)`; `TimeRangeHint::max_ns` is inclusive.
        max_ns = window.to().checked_sub(1);
        filters.push(cmp_ts(&time_col, BinaryOp::GtEq, window.from()));
        filters.push(cmp_ts(&time_col, BinaryOp::Lt, window.to()));
    }
    if let Some(vf) = ctx.visible_from {
        filters.push(cmp_ts(&time_col, BinaryOp::GtEq, vf));
    }

    let exclusions = default_exclusions(
        raw,
        env_col_name,
        meta_col_name,
        scope.include_eval,
        scope.include_experiments,
        references_environment,
    )?;
    filters.extend(exclusions);

    let mut map_projections = BTreeMap::new();
    if !scope.include_experiments {
        map_projections.insert(
            meta_col_name.to_owned(),
            MapProjection::Keys(BTreeSet::from([metadata_key::EXPERIMENT_ID.to_owned()])),
        );
    }

    Ok(ScanSpec {
        projection: None,
        map_projections,
        time_range: TimeRangeHint {
            min_ns,
            max_ns,
            around_ns: ctx.around,
        },
        id_lookups: Vec::new(),
        filters,
        substrings: Vec::new(),
        sort_hints: Vec::new(),
        limit: None,
    })
}

/// Default list exclusion predicates against `schema` (`environment` and `metadata['cetra.experiment_id']`).
pub(crate) fn default_exclusions(
    schema: &Schema,
    env_col_name: &str,
    meta_col_name: &str,
    include_eval: bool,
    include_experiments: bool,
    references_environment: bool,
) -> Result<Vec<Expr>, RunError> {
    let mut clauses = Vec::new();
    if !(include_eval || references_environment) {
        let env_col =
            col_expr(schema, env_col_name).map_err(crate::reads::context::query_err_to_run)?;
        for &reserved in environment::RESERVED {
            clauses.push(Expr::Binary {
                left: Box::new(env_col.clone()),
                op: BinaryOp::IsDistinctFrom,
                right: Box::new(Expr::Literal(ScalarValue::Utf8(reserved.to_owned()))),
                data_type: DataType::Bool,
            });
        }
    }
    if !include_experiments {
        let meta_col =
            col_expr(schema, meta_col_name).map_err(crate::reads::context::query_err_to_run)?;
        let access = Expr::MapIndex {
            map: Box::new(meta_col),
            key: Box::new(Expr::Literal(ScalarValue::Utf8(
                metadata_key::EXPERIMENT_ID.to_owned(),
            ))),
        };
        clauses.push(Expr::Unary {
            op: UnaryOp::IsNull,
            expr: Box::new(access),
            data_type: DataType::Bool,
        });
    }
    Ok(clauses)
}

/// Strict exclusions predicate list for `traces` (both flags off, `references_environment = false`).
pub(crate) fn strict_traces_exclusions(traces_schema: &Schema) -> Result<Vec<Expr>, RunError> {
    default_exclusions(
        traces_schema,
        traces::ENVIRONMENT,
        traces::METADATA,
        false,
        false,
        false,
    )
}

/// Applies strict exclusions (D-I-9) to `streams` of `view` (`Traces` or `Spans`).
/// Anti-joins `spans` streams against `TracesSource` for non-clean traces (reserved environments
/// or experiments), restricting the `TracesSource` scan to `trace_ids` when `Some(...)`.
pub(crate) fn strict_exclusions_spans(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    spans_streams: Vec<Box<dyn PartitionStream>>,
    spans_schema: Arc<Schema>,
    trace_ids: Option<&[[u8; 16]]>,
) -> Result<Vec<Box<dyn PartitionStream>>, QueryError> {
    if spans_streams.is_empty() {
        return Ok(Vec::new());
    }
    if let Some(ids) = trace_ids
        && ids.is_empty()
    {
        return Ok(spans_streams);
    }

    let traces_raw = traces_raw_schema().map_err(run_err_to_query)?;
    let trace_id_idx = traces_raw
        .index_of(traces::TRACE_ID)
        .ok_or_else(|| QueryError::internal("trace_id missing from traces schema"))?;
    let clean_clauses = strict_traces_exclusions(&traces_raw).map_err(run_err_to_query)?;
    let Some(clean_expr) = reduce_and(clean_clauses) else {
        return Err(QueryError::internal(
            "strict exclusions of traces are empty",
        ));
    };
    let not_clean = Expr::Unary {
        op: UnaryOp::Not,
        expr: Box::new(clean_expr),
        data_type: DataType::Bool,
    };

    let mut filters = vec![not_clean];
    if let Some(vf) = ctx.visible_from {
        let start_col = col_expr(&traces_raw, traces::START_TIME)?;
        filters.push(cmp_ts(&start_col, BinaryOp::GtEq, vf));
    }
    let mut id_lookups = Vec::new();
    if let Some(ids) = trace_ids {
        let trace_col = col_expr(&traces_raw, traces::TRACE_ID)?;
        let list = ids
            .iter()
            .map(|id| Expr::Literal(ScalarValue::FixedBytes16(*id)))
            .collect();
        filters.push(Expr::InList {
            expr: Box::new(trace_col),
            list,
            negated: false,
            redacted: false,
        });
        id_lookups.push(IdLookupHint::Fixed16 {
            column: traces::TRACE_ID.to_owned(),
            values: ids.to_vec(),
        });
    }

    let mut map_projections = BTreeMap::new();
    map_projections.insert(
        traces::METADATA.to_owned(),
        MapProjection::Keys(BTreeSet::from([metadata_key::EXPERIMENT_ID.to_owned()])),
    );
    let spec = ScanSpec {
        projection: Some(vec![trace_id_idx]),
        map_projections,
        time_range: TimeRangeHint {
            min_ns: ctx.visible_from,
            max_ns: None,
            around_ns: ctx.around,
        },
        id_lookups,
        filters,
        substrings: Vec::new(),
        sort_hints: Vec::new(),
        limit: None,
    };

    let bad_traces = ctx.traces.open_partitions(&spec, exec_ctx)?;
    if bad_traces.is_empty() {
        return Ok(spans_streams);
    }

    let left_trace_col = col_expr(&spans_schema, spans::TRACE_ID)?;
    let right = harmonize_trace_id_side(bad_traces, &left_trace_col.data_type(), exec_ctx)?;
    let right_trace_col = col_expr(&right.schema, traces::TRACE_ID)?;

    let joined = execute_hash_join(
        spans_streams,
        spans_schema,
        right.streams,
        right.schema,
        HashJoinSpec {
            join_type: JoinType::LeftAnti,
            left_keys: vec![left_trace_col],
            right_keys: vec![right_trace_col],
            null_equals_null: false,
            residual_filter: None,
        },
        exec_ctx,
    )?;
    Ok(vec![joined])
}

/// Shared `trace_id` semi-join source for `widgets`, `alerts`, and `prompts`:
/// traces starting in `[from - lookback_ns, to)` (and optionally ending `>= from`)
/// that pass `scope` exclusions and `dashboard_filter`.
///
/// Returns `Ok(None)` when `dashboard_filter` is absent/blank AND `scope.include_eval &&
/// scope.include_experiments` are both true (after still checking `from - lookback_ns` for
/// `i64` underflow -> `RunError::InvalidRequest("from is out of range")`).
/// Parameters for [`trace_id_semi_join`] and [`apply_trace_id_semi_join`].
pub(crate) struct TraceSemiJoinParams<'a> {
    pub(crate) lookback_ns: i64,
    pub(crate) ends_after_from: bool,
    pub(crate) scope: ListScope,
    pub(crate) references_environment: bool,
    pub(crate) dashboard_filter: Option<&'a str>,
}

/// Builds a reusable `trace_id` semi-join stream (`FixedBytes16`) from `traces` over
/// `[from - lookback_ns, to)` (optionally requiring `end_time >= from`).
/// Returns `Ok(None)` when `dashboard_filter` is absent/blank and both `include_eval` and
/// `include_experiments` are `true` (skipping the lookback subtraction just like the oracle).
pub(crate) fn trace_id_semi_join(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    params: &TraceSemiJoinParams<'_>,
) -> Result<Option<StreamSet>, RunError> {
    let TraceSemiJoinParams {
        lookback_ns,
        ends_after_from,
        scope,
        references_environment,
        dashboard_filter,
    } = *params;
    let Some(window) = scope.window else {
        return Err(RunError::InvalidRequest(
            "time window is required".to_owned(),
        ));
    };
    let filter_text = crate::reads::present(dashboard_filter);
    if filter_text.is_none() && scope.include_eval && scope.include_experiments {
        return Ok(None);
    }
    let start = window
        .from()
        .checked_sub(lookback_ns)
        .ok_or_else(|| RunError::InvalidRequest("from is out of range".to_owned()))?;
    let wide = TimeWindow::new(start, window.to())?;

    let compiled = compile_optional_filter(FilterView::Traces, filter_text)?;
    let references_environment = references_environment
        || compiled
            .as_ref()
            .is_some_and(|filter| filter.references_environment);
    let wide_scope = ListScope {
        window: Some(wide),
        include_eval: scope.include_eval,
        include_experiments: scope.include_experiments,
    };
    let mut spec = scoped_traces_spec(ctx, wide_scope, references_environment)?;
    // Lookback windows define explicit bounds and should not inherit caller `around`.
    spec.time_range.around_ns = None;

    let traces_raw = traces_raw_schema()?;
    if ends_after_from {
        let end_col = col_expr(&traces_raw, traces::END_TIME)
            .map_err(crate::reads::context::query_err_to_run)?;
        spec.filters
            .push(cmp_ts(&end_col, BinaryOp::GtEq, window.from()));
    }

    let trace_id_idx = traces_raw
        .index_of(traces::TRACE_ID)
        .ok_or_else(|| crate::reads::columns::internal("trace_id missing from traces schema"))?;

    match compiled {
        None => {
            spec.projection = Some(vec![trace_id_idx]);
            let streams = ctx
                .traces
                .open_partitions(&spec, exec_ctx)
                .map_err(crate::reads::context::query_err_to_run)?;
            let out_schema = Arc::new(
                traces_raw
                    .project(&[trace_id_idx])
                    .map_err(|e| crate::reads::context::query_err_to_run(e.into()))?,
            );
            Ok(Some(StreamSet {
                streams,
                schema: out_schema,
            }))
        }
        Some(filter) if filter.scores.is_empty() => {
            let rewritten = rewrite_view_expr_for_raw(
                &filter.expr,
                &traces_raw,
                &[traces::TRACE_ID, traces::ROOT_SPAN_ID],
                true,
            )
            .map_err(crate::reads::context::query_err_to_run)?;
            spec.filters.push(rewritten);
            populate_id_lookups(&mut spec);
            spec.projection = Some(vec![trace_id_idx]);
            let streams = ctx
                .traces
                .open_partitions(&spec, exec_ctx)
                .map_err(crate::reads::context::query_err_to_run)?;
            let out_schema = Arc::new(
                traces_raw
                    .project(&[trace_id_idx])
                    .map_err(|e| crate::reads::context::query_err_to_run(e.into()))?,
            );
            Ok(Some(StreamSet {
                streams,
                schema: out_schema,
            }))
        }
        Some(filter) => {
            const RAW_TID: &str = "__raw_trace_id";
            let raw_streams = ctx
                .traces
                .open_partitions(&spec, exec_ctx)
                .map_err(crate::reads::context::query_err_to_run)?;
            let (_, view_plus_raw_schema) =
                traces_view_projection_with_raw_trace_id(&traces_raw, RAW_TID, exec_ctx.clone())
                    .map_err(crate::reads::context::query_err_to_run)?;
            let view_streams: Vec<Box<dyn PartitionStream>> = raw_streams
                .into_iter()
                .map(|s| {
                    let (proj, _) = traces_view_projection_with_raw_trace_id(
                        &traces_raw,
                        RAW_TID,
                        exec_ctx.clone(),
                    )?;
                    Ok(
                        Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                            as Box<dyn PartitionStream>,
                    )
                })
                .collect::<Result<Vec<_>, QueryError>>()
                .map_err(crate::reads::context::query_err_to_run)?;

            let filtered =
                apply_compiled_filter(ctx, exec_ctx, view_streams, &view_plus_raw_schema, &filter)
                    .map_err(crate::reads::context::query_err_to_run)?;

            let raw_tid_idx = view_plus_raw_schema.index_of(RAW_TID).ok_or_else(|| {
                crate::reads::columns::internal("__raw_trace_id missing from semi-join schema")
            })?;
            let id_schema = Schema::try_new_arc(vec![Field::new(
                traces::TRACE_ID,
                DataType::FixedBytes16,
                false,
            )])
            .map_err(|e| crate::reads::context::query_err_to_run(e.into()))?;
            let id_col = Expr::Column {
                index: raw_tid_idx,
                name: RAW_TID.to_owned(),
                data_type: DataType::FixedBytes16,
            };
            let id_streams: Vec<Box<dyn PartitionStream>> = filtered
                .into_iter()
                .map(|s| {
                    let proj = ProjectOp::with_schema(
                        vec![id_col.clone()],
                        Arc::clone(&id_schema),
                        exec_ctx.clone(),
                    )?;
                    Ok(
                        Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                            as Box<dyn PartitionStream>,
                    )
                })
                .collect::<Result<Vec<_>, QueryError>>()
                .map_err(crate::reads::context::query_err_to_run)?;

            Ok(Some(StreamSet {
                streams: id_streams,
                schema: id_schema,
            }))
        }
    }
}

/// Applies [`trace_id_semi_join`] to `left` using `JoinType::LeftSemi` on `trace_id`,
/// always preserving `left.schema` (harmonizing `right` to `left`'s `trace_id` type).
pub(crate) fn apply_trace_id_semi_join(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    left: StreamSet,
    params: &TraceSemiJoinParams<'_>,
) -> Result<Vec<Box<dyn PartitionStream>>, RunError> {
    let Some(right) = trace_id_semi_join(ctx, exec_ctx, params)? else {
        return Ok(left.streams);
    };
    if left.streams.is_empty() {
        return Ok(Vec::new());
    }
    let left_trace_col = col_expr(&left.schema, traces::TRACE_ID)
        .map_err(crate::reads::context::query_err_to_run)?;
    let right_trace_col = col_expr(&right.schema, traces::TRACE_ID)
        .map_err(crate::reads::context::query_err_to_run)?;

    let (right_streams, right_schema, right_key) =
        if left_trace_col.data_type() == right_trace_col.data_type() {
            (right.streams, right.schema, right_trace_col)
        } else {
            let rhs = harmonize_trace_id_side(right.streams, &left_trace_col.data_type(), exec_ctx)
                .map_err(crate::reads::context::query_err_to_run)?;
            let rk = col_expr(&rhs.schema, traces::TRACE_ID)
                .map_err(crate::reads::context::query_err_to_run)?;
            (rhs.streams, rhs.schema, rk)
        };

    let joined = execute_hash_join(
        left.streams,
        left.schema,
        right_streams,
        right_schema,
        HashJoinSpec {
            join_type: JoinType::LeftSemi,
            left_keys: vec![left_trace_col],
            right_keys: vec![right_key],
            null_equals_null: false,
            residual_filter: None,
        },
        exec_ctx,
    )
    .map_err(crate::reads::context::query_err_to_run)?;
    Ok(vec![joined])
}

/// Builds the full-text search predicate for `Spans` (`spec.filters` + `spec.substrings`) or
/// windowed `spans` `trace_id` semi-join streams for `Traces`.
pub(crate) fn search_predicate(
    ctx: &EngineContext,
    exec_ctx: &ExecContext,
    view: FilterView,
    query: &str,
    window: Option<TimeWindow>,
) -> Result<SearchPredicate, RunError> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(SearchPredicate::None);
    }
    if !matches!(view, FilterView::Spans | FilterView::Traces) {
        return Err(RunError::InvalidRequest(
            "search is not available on this list".to_owned(),
        ));
    }
    let Some(window) = window else {
        return Err(RunError::InvalidRequest(
            "search needs a time range".to_owned(),
        ));
    };
    let spans_raw = spans_raw_schema()?;
    let pattern = cetra_query::like_pattern(query);
    let search_cols = [
        spans::INPUT,
        spans::OUTPUT,
        spans::NAME,
        spans::SYSTEM_INSTRUCTIONS,
    ];
    let mut disjuncts = Vec::with_capacity(search_cols.len());
    let mut substrings = Vec::with_capacity(search_cols.len());
    for col_name in search_cols {
        let col =
            col_expr(&spans_raw, col_name).map_err(crate::reads::context::query_err_to_run)?;
        disjuncts.push(Expr::Like {
            expr: Box::new(col),
            pattern: Box::new(Expr::Literal(ScalarValue::Utf8(pattern.clone()))),
            escape: Some('\\'),
            case_insensitive: true,
            negated: false,
        });
        substrings.push(SubstringHint {
            column: col_name.to_owned(),
            needle: query.to_owned(),
            case_insensitive: true,
        });
    }
    let Some(ilike_expr) = reduce_or(disjuncts) else {
        return Err(crate::reads::columns::internal("empty search column list"));
    };

    if view == FilterView::Spans {
        return Ok(SearchPredicate::Spans {
            filter: ilike_expr,
            substrings,
        });
    }

    let trace_id_idx = spans_raw
        .index_of(spans::TRACE_ID)
        .ok_or_else(|| crate::reads::columns::internal("trace_id missing from spans schema"))?;
    let start_col =
        col_expr(&spans_raw, spans::START_TIME).map_err(crate::reads::context::query_err_to_run)?;
    let mut min_ns = window.from();
    if let Some(vf) = ctx.visible_from {
        min_ns = min_ns.max(vf);
    }
    let max_ns = window.to().checked_sub(1);
    let mut filters = vec![
        cmp_ts(&start_col, BinaryOp::GtEq, window.from()),
        cmp_ts(&start_col, BinaryOp::Lt, window.to()),
        ilike_expr,
    ];
    if let Some(vf) = ctx.visible_from {
        filters.push(cmp_ts(&start_col, BinaryOp::GtEq, vf));
    }

    let spec = ScanSpec {
        projection: Some(vec![trace_id_idx]),
        map_projections: BTreeMap::new(),
        time_range: TimeRangeHint {
            min_ns: Some(min_ns),
            max_ns,
            around_ns: None,
        },
        id_lookups: Vec::new(),
        filters,
        substrings: if substrings.is_empty() {
            Vec::new()
        } else {
            vec![substrings]
        },
        sort_hints: Vec::new(),
        limit: None,
    };

    let streams = ctx
        .spans
        .open_partitions(&spec, exec_ctx)
        .map_err(crate::reads::context::query_err_to_run)?;
    let out_schema = Arc::new(
        spans_raw
            .project(&[trace_id_idx])
            .map_err(|e| crate::reads::context::query_err_to_run(e.into()))?,
    );
    Ok(SearchPredicate::Traces {
        trace_ids: streams,
        schema: out_schema,
    })
}

/// Projection indices for the 9 raw `traces` columns required by [`sessions_from_traces`].
pub(crate) fn sessions_from_traces_projection(raw_schema: &Schema) -> Result<Vec<usize>, RunError> {
    let needed = [
        traces::SESSION_ID,
        traces::START_TIME,
        traces::END_TIME,
        traces::TRACE_ID,
        traces::ERROR_COUNT,
        traces::TOTAL_TOKENS,
        traces::COST_TOTAL,
        traces::USER_ID,
        traces::TAGS,
    ];
    let mut indices = Vec::with_capacity(needed.len());
    for name in needed {
        let idx = raw_schema.index_of(name).ok_or_else(|| {
            crate::reads::columns::internal(format!("missing {name} in traces schema"))
        })?;
        indices.push(idx);
    }
    Ok(indices)
}

/// Reusable push-pipeline builder aggregating scoped `traces` partitions into the 11-column
/// `sessions` stream (`SumCostDecimal` cast to `F64`, `FirstNonNullByOrder` for `user_id`,
/// `TagUnion` for `tags`, `duration_ns = end_time - start_time`, `duration_ms`).
pub(crate) fn sessions_from_traces(
    traces_streams: Vec<Box<dyn PartitionStream>>,
    traces_schema: &Arc<Schema>,
    exec_ctx: &ExecContext,
) -> Result<StreamSet, QueryError> {
    let out_schema = sessions_view_schema().map_err(run_err_to_query)?;
    if traces_streams.is_empty() {
        return Ok(StreamSet {
            streams: Vec::new(),
            schema: out_schema,
        });
    }

    let session_col = col_expr(traces_schema, traces::SESSION_ID)?;
    let start_col = col_expr(traces_schema, traces::START_TIME)?;
    let end_col = col_expr(traces_schema, traces::END_TIME)?;
    let trace_col = col_expr(traces_schema, traces::TRACE_ID)?;
    let error_col = col_expr(traces_schema, traces::ERROR_COUNT)?;
    let tokens_col = col_expr(traces_schema, traces::TOTAL_TOKENS)?;
    let cost_col = col_expr(traces_schema, traces::COST_TOTAL)?;
    let user_col = col_expr(traces_schema, traces::USER_ID)?;
    let tags_col = col_expr(traces_schema, traces::TAGS)?;

    let non_null_session = Expr::Unary {
        op: UnaryOp::IsNotNull,
        expr: Box::new(session_col.clone()),
        data_type: DataType::Bool,
    };

    let agg_spec = HashAggSpec {
        group_by: vec![(traces::SESSION_ID.to_owned(), session_col)],
        aggs: vec![
            AggExpr {
                func: AggFunc::Min(start_col.clone()),
                filter: None,
                output_name: traces::START_TIME.to_owned(),
            },
            AggExpr {
                func: AggFunc::Max(end_col),
                filter: None,
                output_name: traces::END_TIME.to_owned(),
            },
            AggExpr {
                func: AggFunc::Count(Expr::Literal(ScalarValue::I64(1))),
                filter: None,
                output_name: "trace_count".to_owned(),
            },
            AggExpr {
                func: AggFunc::Sum(error_col),
                filter: None,
                output_name: traces::ERROR_COUNT.to_owned(),
            },
            AggExpr {
                func: AggFunc::Sum(tokens_col),
                filter: None,
                output_name: traces::TOTAL_TOKENS.to_owned(),
            },
            AggExpr {
                func: AggFunc::SumCostDecimal(cost_col),
                filter: None,
                output_name: traces::COST_TOTAL.to_owned(),
            },
            AggExpr {
                func: AggFunc::FirstNonNullByOrder {
                    value: user_col,
                    order_by: vec![
                        SortKey {
                            expr: start_col,
                            descending: false,
                            nulls_first: true,
                        },
                        SortKey {
                            expr: trace_col,
                            descending: false,
                            nulls_first: true,
                        },
                    ],
                },
                filter: None,
                output_name: traces::USER_ID.to_owned(),
            },
            AggExpr {
                func: AggFunc::TagUnion(tags_col),
                filter: None,
                output_name: traces::TAGS.to_owned(),
            },
        ],
    };

    let sinks = exec_run_pipeline(
        exec_ctx,
        traces_streams,
        {
            let pred = non_null_session;
            let ctx_c = exec_ctx.clone();
            move |_| Ok(vec![Box::new(FilterOp::new(pred.clone(), ctx_c.clone()))])
        },
        {
            let spec_c = agg_spec.clone();
            let schema_c = Arc::clone(traces_schema);
            let ctx_c = exec_ctx.clone();
            move |_| HashAgg::new(spec_c.clone(), Arc::clone(&schema_c), ctx_c.clone())
        },
    )?;

    let agg_stream = HashAgg::combine_sinks_to_stream(sinks, &agg_spec, traces_schema, exec_ctx)?;

    // Post-agg projection producing the 11-column sessions view schema.
    let epoch = Expr::Literal(ScalarValue::TimestampNs {
        value: 0,
        utc: true,
    });
    let agg_session = Expr::Column {
        index: 0,
        name: traces::SESSION_ID.to_owned(),
        data_type: DataType::Utf8,
    };
    let agg_start = Expr::Column {
        index: 1,
        name: traces::START_TIME.to_owned(),
        data_type: DataType::timestamp_utc(),
    };
    let agg_end = Expr::Column {
        index: 2,
        name: traces::END_TIME.to_owned(),
        data_type: DataType::timestamp_utc(),
    };
    let agg_trace_count = Expr::Column {
        index: 3,
        name: "trace_count".to_owned(),
        data_type: DataType::I64,
    };
    let agg_error_count = Expr::Column {
        index: 4,
        name: traces::ERROR_COUNT.to_owned(),
        data_type: DataType::I64,
    };
    let agg_total_tokens = Expr::Column {
        index: 5,
        name: traces::TOTAL_TOKENS.to_owned(),
        data_type: DataType::I64,
    };
    let agg_cost_total = Expr::Column {
        index: 6,
        name: traces::COST_TOTAL.to_owned(),
        data_type: DataType::cost_decimal(),
    };
    let agg_user_id = Expr::Column {
        index: 7,
        name: traces::USER_ID.to_owned(),
        data_type: DataType::Utf8,
    };
    let agg_tags = Expr::Column {
        index: 8,
        name: traces::TAGS.to_owned(),
        data_type: DataType::List(Box::new(DataType::Utf8)),
    };

    let duration_ns = Expr::Binary {
        left: Box::new(Expr::Cast {
            expr: Box::new(agg_end.clone()),
            target: DataType::I64,
            mode: CastMode::Strict,
        }),
        op: BinaryOp::Sub,
        right: Box::new(Expr::Cast {
            expr: Box::new(agg_start.clone()),
            target: DataType::I64,
            mode: CastMode::Strict,
        }),
        data_type: DataType::I64,
    };
    let duration_ms = duration_ms_from_ns_expr(duration_ns.clone());

    let proj_exprs = vec![
        coalesce_expr(agg_session, Expr::Literal(ScalarValue::Utf8(String::new()))),
        coalesce_expr(agg_start, epoch.clone()),
        coalesce_expr(agg_end, epoch),
        duration_ns,
        duration_ms,
        agg_trace_count,
        coalesce_expr(agg_error_count, Expr::Literal(ScalarValue::I64(0))),
        agg_total_tokens,
        Expr::Cast {
            expr: Box::new(agg_cost_total),
            target: DataType::F64,
            mode: CastMode::Strict,
        },
        agg_user_id,
        agg_tags,
    ];

    let project_op = ProjectOp::with_schema(proj_exprs, Arc::clone(&out_schema), exec_ctx.clone())?;
    let out_stream = Box::new(OperatorStream::new(
        agg_stream,
        vec![Box::new(project_op)],
        exec_ctx,
    )) as Box<dyn PartitionStream>;
    Ok(StreamSet {
        streams: vec![out_stream],
        schema: out_schema,
    })
}

fn coalesce_expr(primary: Expr, fallback: Expr) -> Expr {
    let dt = primary.data_type();
    Expr::Call {
        func: ScalarFn::Coalesce,
        args: vec![primary, fallback],
        data_type: dt,
    }
}

fn harmonize_trace_id_side(
    streams: Vec<Box<dyn PartitionStream>>,
    target_type: &DataType,
    exec_ctx: &ExecContext,
) -> Result<StreamSet, QueryError> {
    if *target_type == DataType::FixedBytes16 {
        let schema = Schema::try_new_arc(vec![Field::new(
            traces::TRACE_ID,
            DataType::FixedBytes16,
            false,
        )])?;
        return Ok(StreamSet { streams, schema });
    }
    let out_schema =
        Schema::try_new_arc(vec![Field::new(traces::TRACE_ID, DataType::Utf8, false)])?;
    let col = Expr::Column {
        index: 0,
        name: traces::TRACE_ID.to_owned(),
        data_type: DataType::FixedBytes16,
    };
    let encoded = Expr::Call {
        func: ScalarFn::Encode,
        args: vec![col, Expr::Literal(ScalarValue::Utf8("hex".to_owned()))],
        data_type: DataType::Utf8,
    };
    let out = streams
        .into_iter()
        .map(|s| {
            let proj = ProjectOp::with_schema(
                vec![encoded.clone()],
                Arc::clone(&out_schema),
                exec_ctx.clone(),
            )?;
            Ok(
                Box::new(OperatorStream::new(s, vec![Box::new(proj)], exec_ctx))
                    as Box<dyn PartitionStream>,
            )
        })
        .collect::<Result<Vec<_>, QueryError>>()?;
    Ok(StreamSet {
        streams: out,
        schema: out_schema,
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

fn reduce_and(exprs: Vec<Expr>) -> Option<Expr> {
    exprs.into_iter().reduce(|acc, e| Expr::Binary {
        left: Box::new(acc),
        op: BinaryOp::And,
        right: Box::new(e),
        data_type: DataType::Bool,
    })
}

fn reduce_or(exprs: Vec<Expr>) -> Option<Expr> {
    exprs.into_iter().reduce(|acc, e| Expr::Binary {
        left: Box::new(acc),
        op: BinaryOp::Or,
        right: Box::new(e),
        data_type: DataType::Bool,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytesize::ByteSize;
    use cetra_core::ids::ProjectId;
    use cetra_core::names::environment;
    use cetra_core::span::Span;
    use cetra_query::exec::{CollectSink, run_pipeline};

    use super::*;
    use crate::QueryOptions;
    use crate::config::{
        FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
    };
    use crate::reads::columns::{decimal_to_f64_at, i64_at, require_col, string_at};
    use crate::reads::context::{DURATION_NS, spans_raw_schema};
    use crate::test_support::{span_row, span_rows};

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

    #[tokio::test]
    async fn trace_id_semi_join_underflow_and_search_window_validation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = crate::Storage::open(test_config(dir.path()))
            .await
            .expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");
        let ctx = EngineContext::from_storage(&storage, project, &QueryOptions::new(None, None))
            .expect("ctx");

        let ctx_c = ctx.clone();
        let err = ctx
            .run_with_ctx(Duration::from_secs(5), move |exec_ctx| {
                trace_id_semi_join(
                    &ctx_c,
                    exec_ctx,
                    &TraceSemiJoinParams {
                        lookback_ns: 86_400_000_000_000,
                        ends_after_from: true,
                        scope: ListScope {
                            window: Some(TimeWindow::new(i64::MIN, 0).expect("window")),
                            include_eval: false,
                            include_experiments: false,
                        },
                        references_environment: false,
                        dashboard_filter: None,
                    },
                )
                .map_err(run_err_to_query)?;
                Ok(())
            })
            .await
            .expect_err("underflow");
        match err {
            RunError::InvalidRequest(msg) => assert_eq!(msg, "from is out of range"),
            other => panic!("unexpected {other:?}"),
        }

        let ctx_c = ctx.clone();
        let search_err = ctx
            .run_with_ctx(Duration::from_secs(5), move |exec_ctx| {
                search_predicate(&ctx_c, exec_ctx, FilterView::Spans, "hello", None)
                    .map_err(run_err_to_query)?;
                Ok(())
            })
            .await
            .expect_err("search without window");
        match search_err {
            RunError::InvalidRequest(msg) => assert_eq!(msg, "search needs a time range"),
            other => panic!("unexpected {other:?}"),
        }

        // When semi-join is skipped (include_eval && include_experiments && blank filter),
        // `checked_sub` is skipped too so `i64::MIN` returns `Ok(None)`.
        let ctx_c = ctx.clone();
        let skipped = ctx
            .run_with_ctx(Duration::from_secs(5), move |exec_ctx| {
                let res = trace_id_semi_join(
                    &ctx_c,
                    exec_ctx,
                    &TraceSemiJoinParams {
                        lookback_ns: 86_400_000_000_000,
                        ends_after_from: true,
                        scope: ListScope {
                            window: Some(TimeWindow::new(i64::MIN, 0).expect("window")),
                            include_eval: true,
                            include_experiments: true,
                        },
                        references_environment: false,
                        dashboard_filter: Some("   "),
                    },
                )
                .map_err(run_err_to_query)?;
                Ok(res.is_none())
            })
            .await
            .expect("skipped semi-join succeeds")
            .0;
        assert!(skipped);

        let _ = storage.shutdown().await;
    }

    #[tokio::test]
    async fn apply_trace_id_semi_join_with_score_filter_preserves_left_schema_and_clock_skew() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = crate::Storage::open(test_config(dir.path()))
            .await
            .expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");

        let t0 = 1_700_000_000_000_000_000_i64;
        // Clock-skew trace: end_time < start_time (negative duration_ns)
        let mut s_skew = span_row(1, 0, t0 + 10_000, t0 + 10_000);
        s_skew.end_time = t0 + 2_000;
        s_skew.session_id = Some("sess-skew".to_owned());

        let sc =
            crate::test_support::score_row(1, 1, t0 + 10_000, t0 + 10_000, t0 + 10_000, false, "q");
        storage
            .append_spans(project, span_rows(&[s_skew]))
            .await
            .expect("append spans");
        storage
            .append_scores(project, crate::test_support::score_rows(&[sc]))
            .await
            .expect("append scores");

        let ctx = EngineContext::from_storage(&storage, project, &QueryOptions::new(None, None))
            .expect("ctx");

        // `apply_trace_id_semi_join` with `score(...)` filter must keep `left`'s `FixedBytes16` schema.
        let ctx_c = ctx.clone();
        let (batches, _) = ctx
            .run_with_ctx(Duration::from_secs(5), move |exec_ctx| {
                let raw = spans_raw_schema().map_err(run_err_to_query)?;
                let tid_idx = raw.index_of(spans::TRACE_ID).expect("tid");
                let spec = ScanSpec {
                    projection: Some(vec![tid_idx]),
                    ..ScanSpec::default()
                };
                let left_schema = ctx_c.spans.projected_schema(&spec)?;
                let left_streams = ctx_c.spans.open_partitions(&spec, exec_ctx)?;
                let joined = apply_trace_id_semi_join(
                    &ctx_c,
                    exec_ctx,
                    StreamSet {
                        streams: left_streams,
                        schema: Arc::clone(&left_schema),
                    },
                    &TraceSemiJoinParams {
                        lookback_ns: 86_400_000_000_000,
                        ends_after_from: true,
                        scope: ListScope {
                            window: Some(TimeWindow::new(t0, t0 + 100_000).expect("win")),
                            include_eval: false,
                            include_experiments: false,
                        },
                        references_environment: false,
                        dashboard_filter: Some("score('q') > 0"),
                    },
                )
                .map_err(run_err_to_query)?;
                let pool = Arc::clone(&exec_ctx.pool);
                let sinks = run_pipeline(
                    exec_ctx,
                    joined,
                    |_| Ok(Vec::new()),
                    move |_| Ok(CollectSink::new(&pool)),
                )?;
                let out: Vec<cetra_column::Batch> = sinks
                    .into_iter()
                    .flat_map(CollectSink::into_batches)
                    .collect();
                for b in &out {
                    assert_eq!(
                        b.schema().fields()[0].data_type,
                        DataType::FixedBytes16,
                        "left FixedBytes16 schema must be preserved"
                    );
                }
                Ok(out)
            })
            .await
            .expect("semi-join");
        assert_eq!(
            batches
                .iter()
                .map(cetra_column::Batch::num_rows)
                .sum::<usize>(),
            1
        );

        // `sessions_from_traces` preserves negative duration_ns for clock skew.
        let ctx_c = ctx.clone();
        let (sess_batches, _) = ctx
            .run_with_ctx(Duration::from_secs(5), move |exec_ctx| {
                let raw = traces_raw_schema().map_err(run_err_to_query)?;
                let spec = ScanSpec {
                    projection: Some(
                        sessions_from_traces_projection(&raw).map_err(run_err_to_query)?,
                    ),
                    ..ScanSpec::default()
                };
                let proj = ctx_c.traces.projected_schema(&spec)?;
                let streams = ctx_c.traces.open_partitions(&spec, exec_ctx)?;
                let sess = sessions_from_traces(streams, &proj, exec_ctx)?;
                let pool = Arc::clone(&exec_ctx.pool);
                let sinks = run_pipeline(
                    exec_ctx,
                    sess.streams,
                    |_| Ok(Vec::new()),
                    move |_| Ok(CollectSink::new(&pool)),
                )?;
                Ok(sinks
                    .into_iter()
                    .flat_map(CollectSink::into_batches)
                    .collect::<Vec<_>>())
            })
            .await
            .expect("sessions");
        let b = sess_batches.iter().find(|b| b.num_rows() > 0).expect("row");
        assert_eq!(
            i64_at(b, require_col(b, DURATION_NS).expect("dur"), 0).expect("dur"),
            -8_000
        );

        let _ = storage.shutdown().await;
    }

    #[tokio::test]
    async fn strict_exclusions_with_trace_ids_and_sessions_from_traces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = crate::Storage::open(test_config(dir.path()))
            .await
            .expect("open");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid");

        // Trace 1 (clean, session "s1"): two spans, costs 0.1 + 0.2
        let mut s1: Span = span_row(1, 1, 1_000_000, 2_000_000);
        s1.end_time = 2_000_000;
        s1.session_id = Some("s1".to_owned());
        s1.user_id = Some("u1".to_owned());
        s1.cost_total = Some(0.1);
        s1.tags = vec!["b".to_owned()];

        let mut s1_child: Span = span_row(1, 2, 1_500_000, 3_000_000);
        s1_child.end_time = 3_000_000;
        s1_child.parent_span_id = Some(s1.span_id);
        s1_child.session_id = Some("s1".to_owned());
        s1_child.cost_total = Some(0.2);
        s1_child.tags = vec!["a".to_owned(), "b".to_owned()];

        // Trace 2 (eval trace): root has reserved eval env, child does not -> strict exclusion on Spans
        // must still drop child by anti-joining TracesSource!
        let mut s2_root: Span = span_row(2, 1, 1_000_000, 2_000_000);
        s2_root.environment = Some(environment::EVAL.to_owned());
        s2_root.session_id = Some("s2".to_owned());

        let mut s2_child: Span = span_row(2, 2, 1_500_000, 2_500_000);
        s2_child.parent_span_id = Some(s2_root.span_id);
        s2_child.environment = None;

        let t1_bytes = *s1.trace_id.as_bytes();
        let t2_bytes = *s2_root.trace_id.as_bytes();

        storage
            .append_spans(project, span_rows(&[s1, s1_child, s2_root, s2_child]))
            .await
            .expect("append spans");

        let ctx = EngineContext::from_storage(&storage, project, &QueryOptions::new(None, None))
            .expect("ctx");

        // Test strict_exclusions on Spans restricted to &[t1_bytes, t2_bytes]
        let ctx_c = ctx.clone();
        let (kept_spans, _) = ctx
            .run_with_ctx(Duration::from_secs(5), move |exec_ctx| {
                let raw = spans_raw_schema().map_err(run_err_to_query)?;
                let streams = ctx_c
                    .spans
                    .open_partitions(&ScanSpec::default(), exec_ctx)?;
                let filtered = strict_exclusions_spans(
                    &ctx_c,
                    exec_ctx,
                    streams,
                    raw,
                    Some(&[t1_bytes, t2_bytes]),
                )?;
                let pool = Arc::clone(&exec_ctx.pool);
                let sinks = run_pipeline(
                    exec_ctx,
                    filtered,
                    |_| Ok(Vec::new()),
                    move |_| Ok(CollectSink::new(&pool)),
                )?;
                Ok(sinks
                    .into_iter()
                    .flat_map(CollectSink::into_batches)
                    .collect::<Vec<_>>())
            })
            .await
            .expect("strict spans");
        let kept_rows: usize = kept_spans.iter().map(cetra_column::Batch::num_rows).sum();
        assert_eq!(kept_rows, 2, "only clean trace 1's 2 spans must remain");

        // Test sessions_from_traces with default exclusions (drops s2 eval trace)
        let ctx_c = ctx.clone();
        let scope = ListScope {
            window: None,
            include_eval: false,
            include_experiments: false,
        };
        let (sessions, _) = ctx
            .run_with_ctx(Duration::from_secs(5), move |exec_ctx| {
                let spec = scoped_traces_spec(&ctx_c, scope, false).map_err(run_err_to_query)?;
                let traces_schema = traces_raw_schema().map_err(run_err_to_query)?;
                let streams = ctx_c.traces.open_partitions(&spec, exec_ctx)?;
                let sess = sessions_from_traces(streams, &traces_schema, exec_ctx)?;
                let pool = Arc::clone(&exec_ctx.pool);
                let sinks = run_pipeline(
                    exec_ctx,
                    sess.streams,
                    |_| Ok(Vec::new()),
                    move |_| Ok(CollectSink::new(&pool)),
                )?;
                Ok(sinks
                    .into_iter()
                    .flat_map(CollectSink::into_batches)
                    .collect::<Vec<_>>())
            })
            .await
            .expect("sessions");

        let total_sessions: usize = sessions.iter().map(cetra_column::Batch::num_rows).sum();
        assert_eq!(total_sessions, 1);
        let b = sessions
            .iter()
            .find(|b| b.num_rows() > 0)
            .expect("one batch");
        assert_eq!(
            string_at(b, require_col(b, traces::SESSION_ID).expect("sid"), 0)
                .expect("str")
                .as_deref(),
            Some("s1")
        );
        assert_eq!(
            i64_at(b, require_col(b, "trace_count").expect("tc"), 0).expect("tc"),
            1
        );
        let cost = decimal_to_f64_at(b, require_col(b, traces::COST_TOTAL).expect("cost"), 0)
            .expect("cost")
            .expect("some");
        assert!((cost - 0.3).abs() < 1e-12);
        assert_eq!(
            i64_at(b, require_col(b, DURATION_NS).expect("dur"), 0).expect("dur"),
            2_000_000
        );

        let _ = storage.shutdown().await;
    }
}
