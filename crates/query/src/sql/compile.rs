//! Compiles an optimized Shape B [`LogicalPlan`] into M3 push pipelines and
//! formats privacy-safe physical plans for `EXPLAIN`.

use std::collections::{HashMap, VecDeque};
use std::fmt::{self, Write as _};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use cetra_column::{Batch, CancelToken, Column, Reservation, Schema};

use crate::error::QueryError;
use crate::eval::{build_flat_from_sources, eval_expr};
use crate::exec::hash_agg::{AggExpr, AggFunc, HashAggSpec, execute_hash_agg_streams};
use crate::exec::hash_join::{HashJoinSpec, JoinType, execute_hash_join};
use crate::exec::pipeline::{ControlFlow, ExecContext, PushOperator, run_pipeline};
use crate::exec::sort_key::SortKey;
use crate::exec::sort_window::{
    ExternalSort, FrameBound, WindowFunc, WindowSpec, eval_window_group,
};
use crate::exec::source::{BatchSource, PartitionStream, ScanSource, ScanSpec, TimeRangeHint};
use crate::exec::streaming::{FilterOp, LimitOp, ProjectOp, TopKSink, UnionSource, UnnestOp};
use crate::ir::Expr;

use super::catalog::SqlCatalog;
use super::explain::{explain_batch, format_logical_plan};
use super::logical::{LogicalJoinKind, LogicalPlan, explain_schema};

/// Registry mapping canonical lowercase SQL table names to [`ScanSource`]s.
///
/// Manual [`fmt::Debug`] logs only registered table names (never row payloads).
#[derive(Clone, Default)]
pub struct SqlSources {
    tables: HashMap<String, Arc<dyn ScanSource>>,
}

impl fmt::Debug for SqlSources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut names: Vec<&str> = self.tables.keys().map(String::as_str).collect();
        names.sort_unstable();
        f.debug_struct("SqlSources")
            .field("tables", &names)
            .finish()
    }
}

impl SqlSources {
    /// Creates an empty source registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a [`ScanSource`] under canonical lowercase `name`.
    pub fn register(&mut self, name: impl Into<String>, source: Arc<dyn ScanSource>) {
        self.tables.insert(name.into().to_ascii_lowercase(), source);
    }

    /// Registers all [`BatchSource`] tables already present in `catalog`.
    pub fn register_catalog_batches(&mut self, catalog: &SqlCatalog) {
        for name in catalog.registered_names() {
            if let Some(def) = catalog.get(&name)
                && let Some(batch_src) = &def.batch_source
            {
                self.register(name, Arc::clone(batch_src) as Arc<dyn ScanSource>);
            }
        }
    }

    /// Looks up a registered [`ScanSource`] by canonical lowercase name.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn ScanSource>> {
        self.tables.get(&name.to_ascii_lowercase())
    }
}

/// Compiles an optimized [`LogicalPlan`] into a single streaming [`PartitionStream`].
///
/// Pipeline breakers (`Aggregate`, `Distinct`, `Sort`, `TopK`) execute parallel partitions
/// on `ctx.workers`; stateless operators stream without materializing full inputs.
pub fn compile_plan(
    plan: &LogicalPlan,
    sources: &SqlSources,
    ctx: &ExecContext,
) -> Result<Box<dyn PartitionStream>, QueryError> {
    let stage = compile_stage(plan, sources, ctx)?;
    Ok(stage.into_single_stream(ctx))
}

/// Formats the compiled physical plan tree for `EXPLAIN` using `Expr::fmt_plan()` and
/// `ScanSource::explain_summary()` (never exposing literal payloads or erased GDPR IDs).
pub fn format_physical_plan(plan: &LogicalPlan, sources: &SqlSources) -> String {
    let mut out = String::new();
    fmt_physical_node(plan, sources, 0, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Stage compilation
// ---------------------------------------------------------------------------

struct CompiledStage {
    streams: Vec<Box<dyn PartitionStream>>,
    chains: Vec<Vec<Box<dyn PushOperator>>>,
    schema: Arc<Schema>,
}

impl CompiledStage {
    fn from_streams(streams: Vec<Box<dyn PartitionStream>>, schema: Arc<Schema>) -> Self {
        let chains = (0..streams.len()).map(|_| Vec::new()).collect();
        Self {
            streams,
            chains,
            schema,
        }
    }

    fn empty(schema: Arc<Schema>) -> Self {
        Self {
            streams: Vec::new(),
            chains: Vec::new(),
            schema,
        }
    }

    fn push_op(
        &mut self,
        mut make_op: impl FnMut() -> Result<Box<dyn PushOperator>, QueryError>,
    ) -> Result<(), QueryError> {
        for chain in &mut self.chains {
            chain.push(make_op()?);
        }
        Ok(())
    }

    fn into_streams(self, ctx: &ExecContext) -> Vec<Box<dyn PartitionStream>> {
        self.streams
            .into_iter()
            .zip(self.chains)
            .map(|(stream, ops)| {
                if ops.is_empty() {
                    stream
                } else {
                    Box::new(OpStream::new(stream, ops, ctx)) as Box<dyn PartitionStream>
                }
            })
            .collect()
    }

    fn into_single_stream(self, ctx: &ExecContext) -> Box<dyn PartitionStream> {
        let mut streams = self.into_streams(ctx);
        if streams.is_empty() {
            Box::new(EmptyStream)
        } else if streams.len() == 1 {
            streams.remove(0)
        } else {
            Box::new(ChainedStream::new(streams))
        }
    }
}

fn compile_stage(
    plan: &LogicalPlan,
    sources: &SqlSources,
    ctx: &ExecContext,
) -> Result<CompiledStage, QueryError> {
    if ctx.eval.cancel.is_cancelled() {
        return Err(QueryError::Cancelled);
    }

    match plan {
        LogicalPlan::Scan {
            table,
            projection,
            filters,
            time_range,
            id_lookups,
            substrings,
            map_keys,
            limit,
            schema,
            ..
        } => {
            let Some(source) = sources.get(table) else {
                return Err(QueryError::internal(format!(
                    "missing scan source for table `{table}`"
                )));
            };
            let spec = ScanSpec {
                projection: projection.clone(),
                map_keys: map_keys.clone(),
                time_range: *time_range,
                id_lookups: id_lookups.clone(),
                filters: filters.clone(),
                substrings: substrings.clone(),
                sort_hints: Vec::new(),
                limit: *limit,
            };
            let streams = source.open_partitions(&spec, ctx)?;
            let actual_schema = source.projected_schema(&spec)?;
            let streams = retag_streams_if_needed(streams, &actual_schema, schema);
            Ok(CompiledStage::from_streams(streams, Arc::clone(schema)))
        }

        LogicalPlan::EmptyRow { schema } => {
            let batch = Batch::try_new_all(Arc::clone(schema), Vec::new(), 1)?;
            let src = BatchSource::new(Arc::clone(schema), vec![batch])?;
            let streams = src.open_partitions(&ScanSpec::default(), ctx)?;
            Ok(CompiledStage::from_streams(streams, Arc::clone(schema)))
        }

        LogicalPlan::Values { schema, rows } => compile_values(schema, rows, ctx),

        LogicalPlan::Filter { input, predicate } => {
            let mut stage = compile_stage(input, sources, ctx)?;
            stage.push_op(|| Ok(Box::new(FilterOp::new(predicate.clone(), ctx.clone()))))?;
            Ok(stage)
        }

        LogicalPlan::Project {
            input,
            exprs,
            schema,
        } => {
            let mut stage = compile_stage(input, sources, ctx)?;
            stage.push_op(|| {
                Ok(Box::new(ProjectOp::with_schema(
                    exprs.clone(),
                    Arc::clone(schema),
                    ctx.clone(),
                )?))
            })?;
            stage.schema = Arc::clone(schema);
            Ok(stage)
        }

        LogicalPlan::Unnest {
            input,
            columns,
            preserve_nulls,
            schema,
        } => {
            let mut stage = compile_stage(input, sources, ctx)?;
            stage.push_op(|| {
                Ok(Box::new(UnnestOp::new(
                    columns.clone(),
                    *preserve_nulls,
                    ctx.clone(),
                )))
            })?;
            stage.schema = Arc::clone(schema);
            Ok(stage)
        }

        LogicalPlan::Limit { input, skip, fetch } => {
            let mut stage = compile_stage(input, sources, ctx)?;
            if *fetch == Some(0) || stage.streams.is_empty() {
                return Ok(CompiledStage::empty(stage.schema));
            }
            if *skip == 0 && fetch.is_none() {
                return Ok(stage);
            }
            if *skip > 0 {
                // Coalesce to 1 stream BEFORE LimitOp whenever skip > 0 so per-partition
                // offset counters never skip `skip` rows on every partition.
                let schema = Arc::clone(&stage.schema);
                let single = stage.into_single_stream(ctx);
                let op = Box::new(LimitOp::new(*skip, *fetch, None)) as Box<dyn PushOperator>;
                return Ok(CompiledStage {
                    streams: vec![single],
                    chains: vec![vec![op]],
                    schema,
                });
            }
            // skip == 0 && fetch.is_some()
            if stage.streams.len() == 1 {
                stage.chains[0]
                    .push(Box::new(LimitOp::new(0, *fetch, None)) as Box<dyn PushOperator>);
            } else if let Some(limit_val) = *fetch {
                let shared = Arc::new(AtomicUsize::new(limit_val));
                stage.push_op(|| {
                    Ok(Box::new(LimitOp::new(
                        0,
                        Some(limit_val),
                        Some(Arc::clone(&shared)),
                    )))
                })?;
            }
            Ok(stage)
        }

        LogicalPlan::Sort { input, keys } => {
            let stage = compile_stage(input, sources, ctx)?;
            if stage.streams.is_empty() || keys.is_empty() {
                return Ok(stage);
            }
            let schema = Arc::clone(&stage.schema);
            let make_chain = shared_chains(stage.chains);
            let sinks = run_pipeline(ctx, stage.streams, make_chain, {
                let schema_c = Arc::clone(&schema);
                let keys_c = keys.clone();
                let ctx_c = ctx.clone();
                move |_| {
                    Ok(ExternalSort::new(
                        Arc::clone(&schema_c),
                        keys_c.clone(),
                        None,
                        ctx_c.clone(),
                    ))
                }
            })?;
            let merged =
                ExternalSort::merge_sinks(sinks, Arc::clone(&schema), keys.clone(), None, ctx)?;
            Ok(CompiledStage::from_streams(vec![merged], schema))
        }

        LogicalPlan::TopK {
            input,
            keys,
            limit,
            offset,
        } => {
            let stage = compile_stage(input, sources, ctx)?;
            if *limit == 0 || stage.streams.is_empty() {
                return Ok(CompiledStage::empty(stage.schema));
            }
            if keys.is_empty() {
                let schema = Arc::clone(&stage.schema);
                let single = stage.into_single_stream(ctx);
                let op =
                    Box::new(LimitOp::new(*offset, Some(*limit), None)) as Box<dyn PushOperator>;
                return Ok(CompiledStage {
                    streams: vec![single],
                    chains: vec![vec![op]],
                    schema,
                });
            }
            let schema = Arc::clone(&stage.schema);
            let make_chain = shared_chains(stage.chains);
            let sinks = run_pipeline(ctx, stage.streams, make_chain, {
                let schema_c = Arc::clone(&schema);
                let keys_c = keys.clone();
                let limit_v = *limit;
                let offset_v = *offset;
                let ctx_c = ctx.clone();
                move |_| {
                    Ok(TopKSink::new(
                        Arc::clone(&schema_c),
                        keys_c.clone(),
                        limit_v,
                        offset_v,
                        ctx_c.clone(),
                    ))
                }
            })?;
            let batches = TopKSink::merge(sinks, Arc::clone(&schema), keys, *offset, *limit, ctx)?;
            if batches.is_empty() {
                return Ok(CompiledStage::empty(schema));
            }
            let stream = Box::new(VecBatchStream::new(batches)) as Box<dyn PartitionStream>;
            Ok(CompiledStage::from_streams(vec![stream], schema))
        }

        LogicalPlan::Aggregate {
            input,
            group_by,
            aggs,
            schema,
        } => {
            let stage = compile_stage(input, sources, ctx)?;
            let input_schema = Arc::clone(&stage.schema);
            let partitions = stage.into_streams(ctx);
            let spec = HashAggSpec {
                group_by: group_by.clone(),
                aggs: aggs.clone(),
            };
            let stream = execute_hash_agg_streams(partitions, input_schema, spec, ctx)?;
            let streams = retag_stream_to_schema(stream, schema);
            Ok(CompiledStage::from_streams(
                vec![streams],
                Arc::clone(schema),
            ))
        }

        LogicalPlan::Distinct { input } => {
            let stage = compile_stage(input, sources, ctx)?;
            let schema = Arc::clone(&stage.schema);
            if stage.streams.is_empty() {
                return Ok(CompiledStage::empty(schema));
            }
            if schema.is_empty() {
                let single = stage.into_single_stream(ctx);
                let op = Box::new(LimitOp::new(0, Some(1), None)) as Box<dyn PushOperator>;
                return Ok(CompiledStage {
                    streams: vec![single],
                    chains: vec![vec![op]],
                    schema,
                });
            }
            let partitions = stage.into_streams(ctx);
            let stream = compile_distinct(partitions, &schema, ctx)?;
            Ok(CompiledStage::from_streams(vec![stream], schema))
        }

        LogicalPlan::Window {
            input,
            partition_by,
            order_by,
            specs,
            schema,
        } => {
            let stage = compile_stage(input, sources, ctx)?;
            let input_schema = Arc::clone(&stage.schema);
            let single = stage.into_single_stream(ctx);
            let stream = eval_window_group(
                single,
                input_schema,
                partition_by.clone(),
                order_by.clone(),
                specs.clone(),
                ctx,
            )?;
            let stream = retag_stream_to_schema(stream, schema);
            Ok(CompiledStage::from_streams(
                vec![stream],
                Arc::clone(schema),
            ))
        }

        LogicalPlan::Join {
            left,
            right,
            kind,
            left_keys,
            right_keys,
            null_equals_null,
            residual,
            schema,
        } => {
            let left_stage = compile_stage(left, sources, ctx)?;
            let right_stage = compile_stage(right, sources, ctx)?;
            let left_schema = Arc::clone(&left_stage.schema);
            let right_schema = Arc::clone(&right_stage.schema);
            let left_streams = left_stage.into_streams(ctx);
            let right_streams = right_stage.into_streams(ctx);

            let join_type = match kind {
                LogicalJoinKind::Inner => JoinType::Inner,
                LogicalJoinKind::Left => JoinType::Left,
                LogicalJoinKind::Right => JoinType::Right,
                LogicalJoinKind::Full => JoinType::Full,
                LogicalJoinKind::Semi => JoinType::LeftSemi,
                LogicalJoinKind::Anti => JoinType::LeftAnti,
                LogicalJoinKind::AntiNullAware => JoinType::LeftAntiNullAware,
                LogicalJoinKind::Cross => JoinType::Cross,
            };
            let spec = HashJoinSpec {
                join_type,
                left_keys: left_keys.clone(),
                right_keys: right_keys.clone(),
                null_equals_null: *null_equals_null,
                residual_filter: residual.clone(),
            };
            let joined = execute_hash_join(
                left_streams,
                left_schema,
                right_streams,
                right_schema,
                spec,
                ctx,
            )?;
            let joined = retag_stream_to_schema(joined, schema);
            Ok(CompiledStage::from_streams(
                vec![joined],
                Arc::clone(schema),
            ))
        }

        LogicalPlan::Union {
            inputs,
            all,
            schema,
        } => {
            let mut branch_sources: Vec<Arc<dyn ScanSource>> = Vec::with_capacity(inputs.len());
            for branch in inputs {
                let stage = compile_stage(branch, sources, ctx)?;
                let branch_schema = Arc::clone(&stage.schema);
                let streams = stage.into_streams(ctx);
                branch_sources.push(Arc::new(StreamsScanSource::new(branch_schema, streams)));
            }
            let union_src = UnionSource::new(Arc::clone(schema), branch_sources)?;
            let partitions = union_src.open_partitions(&ScanSpec::default(), ctx)?;
            if *all {
                Ok(CompiledStage::from_streams(partitions, Arc::clone(schema)))
            } else if partitions.is_empty() {
                Ok(CompiledStage::empty(Arc::clone(schema)))
            } else if schema.is_empty() {
                let single = CompiledStage::from_streams(partitions, Arc::clone(schema))
                    .into_single_stream(ctx);
                let op = Box::new(LimitOp::new(0, Some(1), None)) as Box<dyn PushOperator>;
                Ok(CompiledStage {
                    streams: vec![single],
                    chains: vec![vec![op]],
                    schema: Arc::clone(schema),
                })
            } else {
                let distinct_stream = compile_distinct(partitions, schema, ctx)?;
                Ok(CompiledStage::from_streams(
                    vec![distinct_stream],
                    Arc::clone(schema),
                ))
            }
        }

        LogicalPlan::Explain { inner, .. } => {
            let logical_text = format_logical_plan(inner);
            let physical_text = format_physical_plan(inner, sources);
            let batch = explain_batch(&logical_text, &physical_text)?;
            let schema = explain_schema();
            let src = BatchSource::new(Arc::clone(&schema), vec![batch])?;
            let streams = src.open_partitions(&ScanSpec::default(), ctx)?;
            Ok(CompiledStage::from_streams(streams, schema))
        }
    }
}

fn compile_distinct(
    partitions: Vec<Box<dyn PartitionStream>>,
    schema: &Arc<Schema>,
    ctx: &ExecContext,
) -> Result<Box<dyn PartitionStream>, QueryError> {
    // Use unique synthetic names in HashAggSpec so even subqueries with duplicate field
    // names never fail HashAgg's internal schema construction, then retag to `schema`.
    let group_by: Vec<(String, Expr)> = schema
        .fields()
        .iter()
        .enumerate()
        .map(|(index, field)| {
            (
                format!("__distinct_{index}"),
                Expr::Column {
                    index,
                    name: field.name.clone(),
                    data_type: field.data_type.clone(),
                },
            )
        })
        .collect();
    let spec = HashAggSpec {
        group_by,
        aggs: Vec::new(),
    };
    let stream = execute_hash_agg_streams(partitions, Arc::clone(schema), spec, ctx)?;
    Ok(retag_stream_to_schema(stream, schema))
}

fn compile_values(
    schema: &Arc<Schema>,
    rows: &[Vec<Expr>],
    ctx: &ExecContext,
) -> Result<CompiledStage, QueryError> {
    if rows.is_empty() {
        return Ok(CompiledStage::empty(Arc::clone(schema)));
    }
    if schema.is_empty() {
        let batch = Batch::try_new_all(Arc::clone(schema), Vec::new(), rows.len())?;
        let src = BatchSource::new(Arc::clone(schema), vec![batch])?;
        let streams = src.open_partitions(&ScanSpec::default(), ctx)?;
        return Ok(CompiledStage::from_streams(streams, Arc::clone(schema)));
    }

    let empty_schema = Schema::try_new_arc(Vec::new())?;
    let one_row_empty = Batch::try_new_all(empty_schema, Vec::new(), 1)?;
    let mut res = ctx.pool.register("values", false);

    let built = (|| -> Result<Batch, QueryError> {
        let mut cols = Vec::with_capacity(schema.len());
        for (col_idx, field) in schema.fields().iter().enumerate() {
            if ctx.eval.cancel.is_cancelled() {
                return Err(QueryError::Cancelled);
            }
            let mut row_cols = Vec::with_capacity(rows.len());
            for row in rows {
                let expr = row.get(col_idx).ok_or_else(|| {
                    QueryError::internal("VALUES row length does not match schema")
                })?;
                let evaluated = eval_expr(expr, &one_row_empty, &ctx.eval, Some(&mut res))?;
                row_cols.push(evaluated);
            }
            if row_cols.len() == 1 {
                let single = row_cols.remove(0);
                if single.data_type() == field.data_type {
                    cols.push(single);
                    continue;
                }
            }
            let sources: Vec<Option<(&Column, usize)>> = row_cols
                .iter()
                .map(|c| if c.is_null(0) { None } else { Some((c, 0)) })
                .collect();
            let before = res.size();
            let flat = build_flat_from_sources(
                &field.data_type,
                &sources,
                &mut Some(&mut res),
                &ctx.eval,
                before,
            )?;
            cols.push(flat);
        }
        Ok(Batch::try_new_all(Arc::clone(schema), cols, rows.len())?)
    })();

    res.free();
    let batch = built?;
    let src = BatchSource::new(Arc::clone(schema), vec![batch])?;
    let streams = src.open_partitions(&ScanSpec::default(), ctx)?;
    Ok(CompiledStage::from_streams(streams, Arc::clone(schema)))
}

type PartitionChainSlots = Arc<Mutex<Vec<Option<Vec<Box<dyn PushOperator>>>>>>;

fn shared_chains(
    chains: Vec<Vec<Box<dyn PushOperator>>>,
) -> impl Fn(usize) -> Result<Vec<Box<dyn PushOperator>>, QueryError> + Send + Sync + 'static {
    let slots: PartitionChainSlots = Arc::new(Mutex::new(chains.into_iter().map(Some).collect()));
    move |partition_idx: usize| {
        let mut guard = slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = guard
            .get_mut(partition_idx)
            .ok_or_else(|| QueryError::internal("partition chain index out of bounds"))?;
        slot.take()
            .ok_or_else(|| QueryError::internal("partition chain already consumed"))
    }
}

// ---------------------------------------------------------------------------
// Stream helpers
// ---------------------------------------------------------------------------

struct EmptyStream;

impl PartitionStream for EmptyStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        if let Some(r) = res {
            let _ = r;
        }
        if cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        Ok(None)
    }
}

struct VecBatchStream {
    batches: VecDeque<Batch>,
}

impl VecBatchStream {
    fn new(batches: Vec<Batch>) -> Self {
        Self {
            batches: batches.into(),
        }
    }
}

impl PartitionStream for VecBatchStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_ref().map_or(0, |r| r.size());
        if cancel.is_cancelled() {
            rollback_res(res, before);
            return Err(QueryError::Cancelled);
        }
        let Some(batch) = self.batches.pop_front() else {
            rollback_res(res, before);
            return Ok(None);
        };
        reconcile_res(res, before, batch.memory_bytes())?;
        Ok(Some(batch))
    }
}

struct ChainedStream {
    streams: VecDeque<Box<dyn PartitionStream>>,
}

impl ChainedStream {
    fn new(streams: Vec<Box<dyn PartitionStream>>) -> Self {
        Self {
            streams: streams.into(),
        }
    }
}

impl PartitionStream for ChainedStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_ref().map_or(0, |r| r.size());
        while let Some(front) = self.streams.front_mut() {
            if cancel.is_cancelled() {
                rollback_res(res, before);
                return Err(QueryError::Cancelled);
            }
            match front.next_batch(cancel, res.as_deref_mut()) {
                Ok(Some(batch)) => return Ok(Some(batch)),
                Ok(None) => {
                    self.streams.pop_front();
                }
                Err(err) => {
                    rollback_res(res, before);
                    return Err(err);
                }
            }
        }
        rollback_res(res, before);
        Ok(None)
    }
}

struct RetagStream {
    inner: Box<dyn PartitionStream>,
    target_schema: Arc<Schema>,
}

impl PartitionStream for RetagStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_ref().map_or(0, |r| r.size());
        let Some(batch) = self.inner.next_batch(cancel, res.as_deref_mut())? else {
            return Ok(None);
        };
        if batch.schema().as_ref() == self.target_schema.as_ref() {
            return Ok(Some(batch));
        }
        let (_old_schema, columns, raw_len, selection) = batch.into_parts();
        match Batch::try_new(Arc::clone(&self.target_schema), columns, raw_len, selection) {
            Ok(retagged) => Ok(Some(retagged)),
            Err(err) => {
                rollback_res(res, before);
                Err(err.into())
            }
        }
    }
}

fn retag_stream_to_schema(
    stream: Box<dyn PartitionStream>,
    target_schema: &Arc<Schema>,
) -> Box<dyn PartitionStream> {
    Box::new(RetagStream {
        inner: stream,
        target_schema: Arc::clone(target_schema),
    })
}

fn retag_streams_if_needed(
    streams: Vec<Box<dyn PartitionStream>>,
    actual: &Arc<Schema>,
    target: &Arc<Schema>,
) -> Vec<Box<dyn PartitionStream>> {
    if actual.as_ref() == target.as_ref() {
        return streams;
    }
    streams
        .into_iter()
        .map(|s| retag_stream_to_schema(s, target))
        .collect()
}

struct OpStream {
    inner: Box<dyn PartitionStream>,
    ops: Vec<Box<dyn PushOperator>>,
    queued: VecDeque<Batch>,
    pull_res: Reservation,
    queue_res: Reservation,
    finished: bool,
}

impl OpStream {
    fn new(
        inner: Box<dyn PartitionStream>,
        ops: Vec<Box<dyn PushOperator>>,
        ctx: &ExecContext,
    ) -> Self {
        Self {
            inner,
            ops,
            queued: VecDeque::new(),
            pull_res: ctx.pool.register("op_stream_pull", false),
            queue_res: ctx.pool.register("op_stream_queue", false),
            finished: false,
        }
    }
}

impl PartitionStream for OpStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_ref().map_or(0, |r| r.size());
        loop {
            if cancel.is_cancelled() {
                self.pull_res.free();
                self.queue_res.free();
                self.queued.clear();
                rollback_res(res, before);
                return Err(QueryError::Cancelled);
            }
            if let Some(batch) = self.queued.pop_front() {
                let bytes = batch.memory_bytes();
                self.queue_res.shrink(self.queue_res.size().min(bytes));
                if let Err(err) = reconcile_res(res.as_deref_mut(), before, bytes) {
                    self.pull_res.free();
                    self.queue_res.free();
                    self.queued.clear();
                    return Err(err);
                }
                return Ok(Some(batch));
            }
            if self.finished {
                self.pull_res.free();
                self.queue_res.free();
                rollback_res(res, before);
                return Ok(None);
            }

            let next = self.inner.next_batch(cancel, Some(&mut self.pull_res));
            match next {
                Ok(Some(batch)) => {
                    let queue_res = &mut self.queue_res;
                    let queued = &mut self.queued;
                    let flow = push_through_slice(&mut self.ops, batch, &mut |out| {
                        queue_res.try_grow(out.memory_bytes())?;
                        queued.push_back(out);
                        Ok(ControlFlow::Continue)
                    });
                    self.pull_res.free();
                    match flow {
                        Ok(ControlFlow::Continue) => {}
                        Ok(ControlFlow::Stop) => {
                            self.finished = true;
                        }
                        Err(err) => {
                            self.queue_res.free();
                            self.queued.clear();
                            rollback_res(res, before);
                            return Err(err);
                        }
                    }
                }
                Ok(None) => {
                    self.pull_res.free();
                    self.finished = true;
                    let queue_res = &mut self.queue_res;
                    let queued = &mut self.queued;
                    if let Err(err) = finish_ops(&mut self.ops, &mut |out| {
                        queue_res.try_grow(out.memory_bytes())?;
                        queued.push_back(out);
                        Ok(ControlFlow::Continue)
                    }) {
                        self.queue_res.free();
                        self.queued.clear();
                        rollback_res(res, before);
                        return Err(err);
                    }
                }
                Err(err) => {
                    self.pull_res.free();
                    self.queue_res.free();
                    self.queued.clear();
                    rollback_res(res, before);
                    return Err(err);
                }
            }
        }
    }
}

fn push_through_slice(
    ops: &mut [Box<dyn PushOperator>],
    batch: Batch,
    sink: &mut impl FnMut(Batch) -> Result<ControlFlow, QueryError>,
) -> Result<ControlFlow, QueryError> {
    let Some((op, rest)) = ops.split_first_mut() else {
        return sink(batch);
    };
    let mut downstream_flow = ControlFlow::Continue;
    let op_flow = op.push(batch, &mut |out_batch| {
        let f = push_through_slice(rest, out_batch, sink)?;
        if f == ControlFlow::Stop {
            downstream_flow = ControlFlow::Stop;
        }
        Ok(f)
    })?;
    if op_flow == ControlFlow::Stop || downstream_flow == ControlFlow::Stop {
        Ok(ControlFlow::Stop)
    } else {
        Ok(ControlFlow::Continue)
    }
}

fn finish_ops(
    ops: &mut [Box<dyn PushOperator>],
    sink: &mut impl FnMut(Batch) -> Result<ControlFlow, QueryError>,
) -> Result<(), QueryError> {
    for i in 0..ops.len() {
        let mut downstream_flow = ControlFlow::Continue;
        let (head, tail) = ops.split_at_mut(i + 1);
        head[i].finish(&mut |out_batch| {
            let f = push_through_slice(tail, out_batch, sink)?;
            if f == ControlFlow::Stop {
                downstream_flow = ControlFlow::Stop;
            }
            Ok(f)
        })?;
        if downstream_flow == ControlFlow::Stop {
            break;
        }
    }
    Ok(())
}

struct StreamsScanSource {
    schema: Arc<Schema>,
    streams: Mutex<Option<Vec<Box<dyn PartitionStream>>>>,
}

impl fmt::Debug for StreamsScanSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamsScanSource")
            .field("schema_len", &self.schema.len())
            .finish_non_exhaustive()
    }
}

impl StreamsScanSource {
    fn new(schema: Arc<Schema>, streams: Vec<Box<dyn PartitionStream>>) -> Self {
        Self {
            schema,
            streams: Mutex::new(Some(streams)),
        }
    }
}

impl ScanSource for StreamsScanSource {
    fn schema(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }

    fn projected_schema(&self, spec: &ScanSpec) -> Result<Arc<Schema>, QueryError> {
        match &spec.projection {
            Some(indices) => Ok(Arc::new(self.schema.project(indices)?)),
            None => Ok(Arc::clone(&self.schema)),
        }
    }

    fn open_partitions(
        &self,
        spec: &ScanSpec,
        _ctx: &ExecContext,
    ) -> Result<Vec<Box<dyn PartitionStream>>, QueryError> {
        let _ = self.projected_schema(spec)?;
        let mut guard = self
            .streams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .take()
            .ok_or_else(|| QueryError::internal("Union branch streams already opened"))
    }

    fn explain_summary(&self, _spec: &ScanSpec) -> String {
        "StreamsScanSource".to_owned()
    }
}

fn rollback_res(res: Option<&mut Reservation>, before: usize) {
    if let Some(r) = res {
        let cur = r.size();
        if cur > before {
            r.shrink(cur - before);
        }
    }
}

fn reconcile_res(
    res: Option<&mut Reservation>,
    before: usize,
    target_bytes: usize,
) -> Result<(), QueryError> {
    let Some(r) = res else {
        return Ok(());
    };
    let Some(target) = before.checked_add(target_bytes) else {
        rollback_res(Some(r), before);
        return Err(QueryError::TooLarge);
    };
    let cur = r.size();
    if target > cur {
        if let Err(err) = r.try_grow(target - cur) {
            rollback_res(Some(r), before);
            return Err(err.into());
        }
    } else if target < cur {
        r.shrink(cur - target);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Privacy-safe physical plan formatter
// ---------------------------------------------------------------------------

fn fmt_indent(depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn fmt_physical_node(plan: &LogicalPlan, sources: &SqlSources, depth: usize, out: &mut String) {
    if !out.is_empty() {
        out.push('\n');
    }
    fmt_indent(depth, out);

    match plan {
        LogicalPlan::Scan {
            table,
            projection,
            filters,
            time_range,
            id_lookups,
            substrings,
            map_keys,
            limit,
            schema,
            ..
        } => {
            let spec = ScanSpec {
                projection: projection.clone(),
                map_keys: map_keys.clone(),
                time_range: *time_range,
                id_lookups: id_lookups.clone(),
                filters: filters.clone(),
                substrings: substrings.clone(),
                sort_hints: Vec::new(),
                limit: *limit,
            };
            let source_summary = sources
                .get(table)
                .map_or_else(|| "MissingSource".to_owned(), |s| s.explain_summary(&spec));
            let proj_names: Vec<&str> = schema.fields().iter().map(|f| f.name.as_str()).collect();
            let _ = write!(
                out,
                "ScanExec: {table} ({source_summary}) projection=[{}]",
                proj_names.join(", ")
            );
            if !filters.is_empty() {
                let rendered: Vec<String> = filters.iter().map(Expr::fmt_plan).collect();
                let _ = write!(out, " filters=[{}]", rendered.join(", "));
            }
            if has_time_range(*time_range) {
                let _ = write!(
                    out,
                    " time_range=(min={:?}, max={:?}, around={:?})",
                    time_range.min_ns, time_range.max_ns, time_range.around_ns
                );
            }
            if !id_lookups.is_empty() {
                let _ = write!(out, " id_lookups={id_lookups:?}");
            }
            if !substrings.is_empty() {
                let _ = write!(out, " substrings={substrings:?}");
            }
            if !map_keys.is_empty() {
                let _ = write!(out, " map_keys={}", map_keys.len());
            }
            if let Some(lim) = limit {
                let _ = write!(out, " limit={lim}");
            }
        }
        LogicalPlan::Values { schema, rows } => {
            let _ = write!(
                out,
                "ValuesExec: rows={}, cols={}",
                rows.len(),
                schema.len()
            );
        }
        LogicalPlan::EmptyRow { .. } => {
            out.push_str("EmptyRowExec");
        }
        LogicalPlan::Filter { input, predicate } => {
            let _ = write!(out, "FilterExec: {}", predicate.fmt_plan());
            fmt_physical_node(input, sources, depth + 1, out);
        }
        LogicalPlan::Project {
            input,
            exprs,
            schema,
        } => {
            let items: Vec<String> = exprs
                .iter()
                .zip(schema.fields().iter())
                .map(|(expr, field)| {
                    let rendered = expr.fmt_plan();
                    if rendered == field.name {
                        rendered
                    } else {
                        format!("{rendered} AS {}", field.name)
                    }
                })
                .collect();
            let _ = write!(out, "ProjectExec: [{}]", items.join(", "));
            fmt_physical_node(input, sources, depth + 1, out);
        }
        LogicalPlan::Aggregate {
            input,
            group_by,
            aggs,
            ..
        } => {
            let groups: Vec<String> = group_by
                .iter()
                .map(|(name, expr)| {
                    let rendered = expr.fmt_plan();
                    if rendered == *name {
                        rendered
                    } else {
                        format!("{rendered} AS {name}")
                    }
                })
                .collect();
            let agg_strs: Vec<String> = aggs.iter().map(fmt_agg).collect();
            let _ = write!(
                out,
                "HashAggExec: group_by=[{}], aggs=[{}]",
                groups.join(", "),
                agg_strs.join(", ")
            );
            fmt_physical_node(input, sources, depth + 1, out);
        }
        LogicalPlan::Window {
            input,
            partition_by,
            order_by,
            specs,
            ..
        } => {
            let parts: Vec<String> = partition_by.iter().map(Expr::fmt_plan).collect();
            let ords: Vec<String> = order_by.iter().map(fmt_sort_key).collect();
            let spec_strs: Vec<String> = specs.iter().map(fmt_window_spec).collect();
            let _ = write!(
                out,
                "WindowExec: partition_by=[{}], order_by=[{}], specs=[{}]",
                parts.join(", "),
                ords.join(", "),
                spec_strs.join(", ")
            );
            fmt_physical_node(input, sources, depth + 1, out);
        }
        LogicalPlan::Sort { input, keys } => {
            let key_strs: Vec<String> = keys.iter().map(fmt_sort_key).collect();
            let _ = write!(out, "ExternalSortExec: [{}]", key_strs.join(", "));
            fmt_physical_node(input, sources, depth + 1, out);
        }
        LogicalPlan::Limit { input, skip, fetch } => {
            match fetch {
                Some(n) => {
                    let _ = write!(
                        out,
                        "LimitExec: skip={skip}, fetch={n}, coalesce={}",
                        *skip > 0
                    );
                }
                None => {
                    let _ = write!(
                        out,
                        "LimitExec: skip={skip}, fetch=none, coalesce={}",
                        *skip > 0
                    );
                }
            }
            fmt_physical_node(input, sources, depth + 1, out);
        }
        LogicalPlan::TopK {
            input,
            keys,
            limit,
            offset,
        } => {
            let key_strs: Vec<String> = keys.iter().map(fmt_sort_key).collect();
            let _ = write!(
                out,
                "TopKExec: limit={limit}, offset={offset}, keys=[{}]",
                key_strs.join(", ")
            );
            fmt_physical_node(input, sources, depth + 1, out);
        }
        LogicalPlan::Distinct { input } => {
            out.push_str("DistinctHashAggExec");
            fmt_physical_node(input, sources, depth + 1, out);
        }
        LogicalPlan::Join {
            left,
            right,
            kind,
            left_keys,
            right_keys,
            null_equals_null,
            residual,
            ..
        } => {
            let kind_str = match kind {
                LogicalJoinKind::Inner => "Inner",
                LogicalJoinKind::Left => "Left",
                LogicalJoinKind::Right => "Right",
                LogicalJoinKind::Full => "Full",
                LogicalJoinKind::Semi => "LeftSemi",
                LogicalJoinKind::Anti => "LeftAnti",
                LogicalJoinKind::AntiNullAware => "LeftAntiNullAware",
                LogicalJoinKind::Cross => "Cross",
            };
            let pairs: Vec<String> = left_keys
                .iter()
                .zip(right_keys.iter())
                .map(|(l, r)| format!("{} = {}", l.fmt_plan(), r.fmt_plan()))
                .collect();
            let _ = write!(
                out,
                "HashJoinExec ({kind_str}): on=[{}], null_equals_null={null_equals_null}",
                pairs.join(", ")
            );
            if let Some(res) = residual {
                let _ = write!(out, ", filter={}", res.fmt_plan());
            }
            fmt_physical_node(left, sources, depth + 1, out);
            fmt_physical_node(right, sources, depth + 1, out);
        }
        LogicalPlan::Union { inputs, all, .. } => {
            if *all {
                out.push_str("UnionExec (all=true)");
            } else {
                out.push_str("DistinctHashAggExec -> UnionExec (all=false)");
            }
            for branch in inputs {
                fmt_physical_node(branch, sources, depth + 1, out);
            }
        }
        LogicalPlan::Unnest {
            input,
            columns,
            preserve_nulls,
            ..
        } => {
            let _ = write!(
                out,
                "UnnestExec: columns={columns:?}, preserve_nulls={preserve_nulls}"
            );
            fmt_physical_node(input, sources, depth + 1, out);
        }
        LogicalPlan::Explain { inner, .. } => {
            out.push_str("ExplainExec");
            fmt_physical_node(inner, sources, depth + 1, out);
        }
    }
}

fn has_time_range(tr: TimeRangeHint) -> bool {
    tr.min_ns.is_some() || tr.max_ns.is_some() || tr.around_ns.is_some()
}

fn fmt_sort_key(key: &SortKey) -> String {
    let dir = if key.descending { "DESC" } else { "ASC" };
    let nulls = if key.nulls_first {
        "NULLS FIRST"
    } else {
        "NULLS LAST"
    };
    format!("{} {dir} {nulls}", key.expr.fmt_plan())
}

fn fmt_agg(agg: &AggExpr) -> String {
    let base = match &agg.func {
        AggFunc::CountStar => "count(*)".to_owned(),
        AggFunc::Count(e) => format!("count({})", e.fmt_plan()),
        AggFunc::CountDistinct(e) => format!("count(DISTINCT {})", e.fmt_plan()),
        AggFunc::Sum(e) => format!("sum({})", e.fmt_plan()),
        AggFunc::SumCostDecimal(e) => format!("sum_cost_decimal({})", e.fmt_plan()),
        AggFunc::Avg(e) => format!("avg({})", e.fmt_plan()),
        AggFunc::Min(e) => format!("min({})", e.fmt_plan()),
        AggFunc::Max(e) => format!("max({})", e.fmt_plan()),
        AggFunc::BoolOr(e) => format!("bool_or({})", e.fmt_plan()),
        AggFunc::BoolAnd(e) => format!("bool_and({})", e.fmt_plan()),
        AggFunc::ApproxPercentileCont { expr, quantile } => {
            format!("approx_percentile_cont({}, {quantile})", expr.fmt_plan())
        }
        AggFunc::PercentileCont { expr, quantile } => {
            format!(
                "percentile_cont({quantile}) WITHIN GROUP ({})",
                expr.fmt_plan()
            )
        }
        AggFunc::ArrayAgg { expr, distinct } => {
            if *distinct {
                format!("array_agg(DISTINCT {})", expr.fmt_plan())
            } else {
                format!("array_agg({})", expr.fmt_plan())
            }
        }
        AggFunc::FirstNonNullByOrder { value, order_by } => {
            let ords: Vec<String> = order_by.iter().map(fmt_sort_key).collect();
            format!(
                "first_non_null({}, [{}])",
                value.fmt_plan(),
                ords.join(", ")
            )
        }
        AggFunc::FirstByOrder { value, order_by } => {
            let ords: Vec<String> = order_by.iter().map(fmt_sort_key).collect();
            format!("first({}, [{}])", value.fmt_plan(), ords.join(", "))
        }
        AggFunc::TagUnion(e) => format!("tag_union({})", e.fmt_plan()),
    };
    let with_filter = match &agg.filter {
        Some(f) => format!("{base} FILTER (WHERE {})", f.fmt_plan()),
        None => base,
    };
    format!("{with_filter} AS {}", agg.output_name)
}

fn fmt_window_spec(spec: &WindowSpec) -> String {
    let f = match &spec.func {
        WindowFunc::RowNumber => "row_number()".to_owned(),
        WindowFunc::Rank => "rank()".to_owned(),
        WindowFunc::DenseRank => "dense_rank()".to_owned(),
        WindowFunc::Lag {
            expr,
            offset,
            default,
        } => match default {
            Some(d) => format!("lag({}, {offset}, {})", expr.fmt_plan(), d.fmt_plan()),
            None => format!("lag({}, {offset})", expr.fmt_plan()),
        },
        WindowFunc::Lead {
            expr,
            offset,
            default,
        } => match default {
            Some(d) => format!("lead({}, {offset}, {})", expr.fmt_plan(), d.fmt_plan()),
            None => format!("lead({}, {offset})", expr.fmt_plan()),
        },
        WindowFunc::FirstValue(e) => format!("first_value({})", e.fmt_plan()),
        WindowFunc::LastValue(e) => format!("last_value({})", e.fmt_plan()),
        WindowFunc::Sum(e) => format!("sum({})", e.fmt_plan()),
        WindowFunc::Count(Some(e)) => format!("count({})", e.fmt_plan()),
        WindowFunc::Count(None) => "count(*)".to_owned(),
        WindowFunc::Avg(e) => format!("avg({})", e.fmt_plan()),
        WindowFunc::Min(e) => format!("min({})", e.fmt_plan()),
        WindowFunc::Max(e) => format!("max({})", e.fmt_plan()),
    };
    let frame = match spec.frame {
        Some((start, end)) => format!(" ROWS BETWEEN {} AND {}", fmt_bound(start), fmt_bound(end)),
        None => String::new(),
    };
    format!("{f}{frame} AS {}", spec.output_name)
}

fn fmt_bound(b: FrameBound) -> String {
    match b {
        FrameBound::UnboundedPreceding => "UNBOUNDED PRECEDING".to_owned(),
        FrameBound::Preceding(n) => format!("{n} PRECEDING"),
        FrameBound::CurrentRow => "CURRENT ROW".to_owned(),
        FrameBound::Following(n) => format!("{n} FOLLOWING"),
        FrameBound::UnboundedFollowing => "UNBOUNDED FOLLOWING".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use cetra_column::{
        DataType, Field, FlatColumn, FlatData, GlobalPool, ScalarValue, Schema, Utf8Data,
    };

    use super::*;
    use crate::eval::{read_i64, read_str};
    use crate::exec::{ExecMetrics, SpillManager, WorkerPool};
    use crate::sql::catalog::raw_scores_schema;
    use crate::sql::optimize::optimize;
    use crate::tombstone::{TombstoneSpec, TombstoneTable, covered_predicate};

    fn test_ctx(cap: usize) -> (ExecContext, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ExecContext {
            pool: GlobalPool::standalone(cap),
            eval: crate::ir::EvalContext {
                now_ns: 0,
                cancel: CancelToken::new(),
            },
            spill: Arc::new(SpillManager::with_fixed_cap(
                dir.path().to_path_buf(),
                1024 * 1024,
            )),
            workers: Arc::new(WorkerPool::new(2).unwrap()),
            batch_size: 4,
            target_partitions: 2,
            metrics: ExecMetrics::default(),
        };
        (ctx, dir)
    }

    fn drain_stream(stream: &mut dyn PartitionStream, cancel: &CancelToken) -> Vec<Batch> {
        let mut out = Vec::new();
        while let Some(b) = stream.next_batch(cancel, None).unwrap() {
            if !b.is_empty() {
                out.push(b);
            }
        }
        out
    }

    #[test]
    fn limit_with_offset_coalesces_multi_partition_source() {
        let schema = Schema::try_new_arc(vec![Field::new("id", DataType::I64, false)]).unwrap();
        let b1 = Batch::try_new_all(
            Arc::clone(&schema),
            vec![Column::Flat(
                FlatColumn::try_new(FlatData::I64(vec![1, 2, 3]), None).unwrap(),
            )],
            3,
        )
        .unwrap();
        let b2 = Batch::try_new_all(
            Arc::clone(&schema),
            vec![Column::Flat(
                FlatColumn::try_new(FlatData::I64(vec![4, 5, 6]), None).unwrap(),
            )],
            3,
        )
        .unwrap();
        let src = Arc::new(BatchSource::new(Arc::clone(&schema), vec![b1, b2]).unwrap());
        let mut sources = SqlSources::new();
        sources.register("nums", src);

        let plan = LogicalPlan::Limit {
            input: Box::new(LogicalPlan::Scan {
                table: "nums".to_owned(),
                kind: crate::sql::SqlViewKind::Batch,
                full_schema: Arc::clone(&schema),
                projection: None,
                filters: Vec::new(),
                time_range: TimeRangeHint::default(),
                id_lookups: Vec::new(),
                substrings: Vec::new(),
                map_keys: Vec::new(),
                limit: None,
                schema: Arc::clone(&schema),
            }),
            skip: 2,
            fetch: Some(3),
        };
        let (ctx, _dir) = test_ctx(1024 * 1024);
        let mut stream = compile_plan(&plan, &sources, &ctx).unwrap();
        let batches = drain_stream(stream.as_mut(), &ctx.eval.cancel);
        let mut vals = Vec::new();
        for b in &batches {
            let compacted = b.compact(None).unwrap();
            for r in 0..compacted.num_rows() {
                vals.push(read_i64(compacted.column(0).unwrap(), r).unwrap());
            }
        }
        assert_eq!(vals, vec![3, 4, 5]);
    }

    #[test]
    fn format_physical_plan_redacts_gdpr_and_payloads() {
        let scores_schema = raw_scores_schema();
        let deletions = vec![TombstoneSpec {
            cutoff: 1_000_000,
            trace_ids: vec![],
            users: vec!["erased-alice@example.com".to_owned()],
            sessions: vec!["erased-session-123".to_owned()],
        }];
        let covered = covered_predicate(TombstoneTable::Scores, &scores_schema, &deletions)
            .unwrap()
            .unwrap();
        let plan = LogicalPlan::Filter {
            input: Box::new(LogicalPlan::Scan {
                table: "scores".to_owned(),
                kind: crate::sql::SqlViewKind::Scores,
                full_schema: Arc::clone(&scores_schema),
                projection: None,
                filters: vec![Expr::Unary {
                    op: crate::ir::UnaryOp::Not,
                    expr: Box::new(covered),
                    data_type: DataType::Bool,
                }],
                time_range: TimeRangeHint::default(),
                id_lookups: vec![crate::exec::IdLookupHint::Utf8 {
                    column: "user_id".to_owned(),
                    values: vec!["secret-user-lookup".to_owned()],
                }],
                substrings: vec![crate::exec::SubstringHint {
                    column: "reason".to_owned(),
                    needle: "secret-needle".to_owned(),
                    case_insensitive: true,
                }],
                map_keys: vec![],
                limit: None,
                schema: Arc::clone(&scores_schema),
            }),
            predicate: Expr::Binary {
                left: Box::new(Expr::Column {
                    index: scores_schema.index_of("name").unwrap(),
                    name: "name".to_owned(),
                    data_type: DataType::Utf8,
                }),
                op: crate::ir::BinaryOp::Eq,
                right: Box::new(Expr::Literal(ScalarValue::Utf8(
                    "secret-prompt-text".to_owned(),
                ))),
                data_type: DataType::Bool,
            },
        };
        let optimized = optimize(plan).unwrap();
        let mut sources = SqlSources::new();
        sources.register("scores", Arc::new(BatchSource::empty(scores_schema)));
        let phys = format_physical_plan(&optimized, &sources);
        assert!(phys.contains("user_id IN (<1 redacted>)"), "{phys}");
        assert!(phys.contains("session_id IN (<1 redacted>)"), "{phys}");
        for secret in [
            "erased-alice@example.com",
            "erased-session-123",
            "secret-user-lookup",
            "secret-needle",
            "secret-prompt-text",
        ] {
            assert!(!phys.contains(secret), "leaked `{secret}` in:\n{phys}");
        }
    }

    #[test]
    fn explain_node_produces_two_col_batch() {
        let schema = Schema::try_new_arc(vec![Field::new("s", DataType::Utf8, false)]).unwrap();
        let src = Arc::new(
            BatchSource::new(
                Arc::clone(&schema),
                vec![
                    Batch::try_new_all(
                        Arc::clone(&schema),
                        vec![Column::Flat(
                            FlatColumn::try_new(
                                FlatData::Utf8(Utf8Data::try_from_slices(["hello"]).unwrap()),
                                None,
                            )
                            .unwrap(),
                        )],
                        1,
                    )
                    .unwrap(),
                ],
            )
            .unwrap(),
        );
        let mut sources = SqlSources::new();
        sources.register("items", src);
        let inner = LogicalPlan::Scan {
            table: "items".to_owned(),
            kind: crate::sql::SqlViewKind::Batch,
            full_schema: Arc::clone(&schema),
            projection: None,
            filters: Vec::new(),
            time_range: TimeRangeHint::default(),
            id_lookups: Vec::new(),
            substrings: Vec::new(),
            map_keys: Vec::new(),
            limit: None,
            schema,
        };
        let explain = LogicalPlan::Explain {
            inner: Box::new(inner),
            schema: explain_schema(),
        };
        let (ctx, _dir) = test_ctx(1024 * 1024);
        let mut stream = compile_plan(&explain, &sources, &ctx).unwrap();
        let batches = drain_stream(stream.as_mut(), &ctx.eval.cancel);
        assert_eq!(batches.len(), 1);
        let b = &batches[0];
        assert_eq!(b.num_rows(), 2);
        assert_eq!(read_str(b.column(0).unwrap(), 0), Some("logical_plan"));
        assert_eq!(read_str(b.column(0).unwrap(), 1), Some("physical_plan"));
        assert!(
            read_str(b.column(1).unwrap(), 0)
                .unwrap()
                .contains("Scan: items")
        );
        assert!(
            read_str(b.column(1).unwrap(), 1)
                .unwrap()
                .contains("ScanExec: items")
        );
    }
}
