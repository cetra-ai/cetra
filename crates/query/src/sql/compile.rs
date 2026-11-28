//! Compiles an optimized Shape B [`LogicalPlan`] into M3 push pipelines and
//! formats privacy-safe physical plans for `EXPLAIN`.

use std::collections::{HashMap, VecDeque};
use std::fmt::{self, Write as _};
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use cetra_column::{Batch, CancelToken, Column, Reservation, Schema, Selection};

use crate::error::QueryError;
use crate::eval::{build_flat_from_sources, eval_expr};
use crate::exec::hash_agg::{HashAggSpec, execute_hash_agg_streams};
use crate::exec::hash_join::{HashJoinSpec, JoinType, execute_hash_join_parallel};
use crate::exec::pipeline::{
    ControlFlow, ExecContext, PushOperator, run_pipeline_with_stop, transfer_or_grow,
};
use crate::exec::sort_window::{ExternalSort, eval_window_group};
use crate::exec::source::{BatchSource, PartitionStream, ScanSource, ScanSpec};
use crate::exec::streaming::{FilterOp, LimitOp, ProjectOp, TopKSink, UnionSource, UnnestOp};
use crate::ir::Expr;

use super::catalog::SqlCatalog;
use super::explain::{
    explain_batch, fmt_agg, fmt_indent, fmt_sort_key, fmt_window_spec, format_logical_plan,
    has_time_range,
};
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
    stop_cancel: Option<CancelToken>,
}

impl CompiledStage {
    fn from_streams(streams: Vec<Box<dyn PartitionStream>>, schema: Arc<Schema>) -> Self {
        let chains = (0..streams.len()).map(|_| Vec::new()).collect();
        Self {
            streams,
            chains,
            schema,
            stop_cancel: None,
        }
    }

    fn empty(schema: Arc<Schema>) -> Self {
        Self {
            streams: Vec::new(),
            chains: Vec::new(),
            schema,
            stop_cancel: None,
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
        let stop_cancel = self.stop_cancel;
        self.streams
            .into_iter()
            .zip(self.chains)
            .map(|(stream, ops)| {
                if ops.is_empty() && stop_cancel.is_none() {
                    stream
                } else {
                    Box::new(OpStream::new(stream, ops, stop_cancel.clone(), ctx))
                        as Box<dyn PartitionStream>
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
            map_projections,
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
                map_projections: map_projections.clone(),
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
                    stop_cancel: None,
                });
            }
            // skip == 0 && fetch.is_some()
            if stage.streams.len() == 1 {
                stage.chains[0]
                    .push(Box::new(LimitOp::new(0, *fetch, None)) as Box<dyn PushOperator>);
            } else if let Some(limit_val) = *fetch {
                let shared = Arc::new(AtomicUsize::new(limit_val));
                let stop_cancel = stage.stop_cancel.clone().unwrap_or_default();
                stage.push_op(|| {
                    Ok(Box::new(
                        LimitOp::new(0, Some(limit_val), Some(Arc::clone(&shared)))
                            .with_stop_cancel(stop_cancel.clone()),
                    ))
                })?;
                stage.stop_cancel = Some(stop_cancel);
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
            let sinks =
                run_pipeline_with_stop(ctx, stage.stop_cancel, stage.streams, make_chain, {
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
                    stop_cancel: None,
                });
            }
            let schema = Arc::clone(&stage.schema);
            let make_chain = shared_chains(stage.chains);
            let sinks =
                run_pipeline_with_stop(ctx, stage.stop_cancel, stage.streams, make_chain, {
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
            let stream = Box::new(VecBatchStream::new(batches, ctx)?) as Box<dyn PartitionStream>;
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
                    stop_cancel: None,
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
            let joined = execute_hash_join_parallel(
                left_streams,
                left_schema,
                right_streams,
                right_schema,
                spec,
                ctx,
            )?;
            let streams = joined
                .into_iter()
                .map(|s| retag_stream_to_schema(s, schema))
                .collect();
            Ok(CompiledStage::from_streams(streams, Arc::clone(schema)))
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
                    stop_cancel: None,
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
        let mut cols: Vec<Arc<Column>> = Vec::with_capacity(schema.len());
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
                .map(|c| {
                    if c.is_null(0) {
                        None
                    } else {
                        Some((c.as_ref(), 0))
                    }
                })
                .collect();
            let before = res.size();
            let flat = build_flat_from_sources(
                &field.data_type,
                &sources,
                &mut Some(&mut res),
                &ctx.eval,
                before,
            )?;
            cols.push(Arc::new(flat));
        }
        Ok(Batch::try_new_shared(
            Arc::clone(schema),
            cols,
            rows.len(),
            Selection::All,
        )?)
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
    queue_res: Reservation,
}

impl VecBatchStream {
    fn new(batches: Vec<Batch>, ctx: &ExecContext) -> Result<Self, QueryError> {
        let mut queue_res = ctx.pool.register("topk_queue", false);
        let total_bytes: usize = batches
            .iter()
            .try_fold(0usize, |acc, b| acc.checked_add(b.memory_bytes()))
            .ok_or(QueryError::TooLarge)?;
        if total_bytes > 0 {
            queue_res.try_grow(total_bytes)?;
        }
        Ok(Self {
            batches: batches.into(),
            queue_res,
        })
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
            self.batches.clear();
            self.queue_res.free();
            rollback_res(res, before);
            return Err(QueryError::Cancelled);
        }
        let Some(batch) = self.batches.pop_front() else {
            self.queue_res.free();
            rollback_res(res, before);
            return Ok(None);
        };
        let bytes = batch.memory_bytes();
        self.queue_res.shrink(self.queue_res.size().min(bytes));
        if let Err(err) = reconcile_res(res, before, bytes) {
            self.batches.clear();
            self.queue_res.free();
            return Err(err);
        }
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
        match Batch::try_new_shared(Arc::clone(&self.target_schema), columns, raw_len, selection) {
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
    stop_cancel: Option<CancelToken>,
    finished: bool,
}

impl OpStream {
    fn new(
        inner: Box<dyn PartitionStream>,
        ops: Vec<Box<dyn PushOperator>>,
        stop_cancel: Option<CancelToken>,
        ctx: &ExecContext,
    ) -> Self {
        Self {
            inner,
            ops,
            queued: VecDeque::new(),
            pull_res: ctx.pool.register("op_stream_pull", false),
            queue_res: ctx.pool.register("op_stream_queue", false),
            stop_cancel,
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
            if self
                .stop_cancel
                .as_ref()
                .is_some_and(CancelToken::is_cancelled)
            {
                self.finished = true;
                self.pull_res.free();
                self.queue_res.free();
                rollback_res(res, before);
                return Ok(None);
            }

            let next = if let Some(sc) = &self.stop_cancel {
                let eff = CancelToken::new();
                if cancel.is_cancelled() || sc.is_cancelled() {
                    eff.cancel();
                }
                let _g1 = cancel.register_waker({
                    let e = eff.clone();
                    move || e.cancel()
                });
                let _g2 = sc.register_waker({
                    let e = eff.clone();
                    move || e.cancel()
                });
                self.inner.next_batch(&eff, Some(&mut self.pull_res))
            } else {
                self.inner.next_batch(cancel, Some(&mut self.pull_res))
            };
            match next {
                Ok(Some(batch)) => {
                    let queue_res = &mut self.queue_res;
                    let queued = &mut self.queued;
                    let flow = push_through_slice(
                        &mut self.ops,
                        batch,
                        &mut self.pull_res,
                        &mut |out, out_res| {
                            transfer_or_grow(out_res, queue_res, out.memory_bytes())?;
                            queued.push_back(out);
                            Ok(ControlFlow::Continue)
                        },
                    );
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
                    if let Err(err) = finish_ops(&mut self.ops, &mut |out, out_res| {
                        transfer_or_grow(out_res, queue_res, out.memory_bytes())?;
                        queued.push_back(out);
                        Ok(ControlFlow::Continue)
                    }) {
                        self.queue_res.free();
                        self.queued.clear();
                        rollback_res(res, before);
                        return Err(err);
                    }
                }
                Err(QueryError::Cancelled)
                    if !cancel.is_cancelled()
                        && self
                            .stop_cancel
                            .as_ref()
                            .is_some_and(CancelToken::is_cancelled) =>
                {
                    self.pull_res.free();
                    self.finished = true;
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
    batch_res: &mut Reservation,
    sink: &mut impl FnMut(Batch, &mut Reservation) -> Result<ControlFlow, QueryError>,
) -> Result<ControlFlow, QueryError> {
    let Some((op, rest)) = ops.split_first_mut() else {
        return sink(batch, batch_res);
    };
    let mut downstream_flow = ControlFlow::Continue;
    let op_flow = op.push(batch, batch_res, &mut |out_batch, out_res| {
        let f = push_through_slice(rest, out_batch, out_res, sink)?;
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
    sink: &mut impl FnMut(Batch, &mut Reservation) -> Result<ControlFlow, QueryError>,
) -> Result<(), QueryError> {
    for i in 0..ops.len() {
        let mut downstream_flow = ControlFlow::Continue;
        let (head, tail) = ops.split_at_mut(i + 1);
        head[i].finish(&mut |out_batch, out_res| {
            let f = push_through_slice(tail, out_batch, out_res, sink)?;
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

fn fmt_physical_node(plan: &LogicalPlan, sources: &SqlSources, depth: usize, out: &mut String) {
    let mut stack = vec![(plan, depth)];
    while let Some((cur, d)) = stack.pop() {
        if !out.is_empty() {
            out.push('\n');
        }
        fmt_indent(d, out);

        match cur {
            LogicalPlan::Scan {
                table,
                projection,
                filters,
                time_range,
                id_lookups,
                substrings,
                map_projections,
                limit,
                schema,
                ..
            } => {
                let spec = ScanSpec {
                    projection: projection.clone(),
                    map_projections: map_projections.clone(),
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
                let proj_names: Vec<&str> =
                    schema.fields().iter().map(|f| f.name.as_str()).collect();
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
                if !map_projections.is_empty() {
                    let _ = write!(out, " map_projections={}", map_projections.len());
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
                stack.push((input, d + 1));
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
                stack.push((input, d + 1));
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
                stack.push((input, d + 1));
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
                stack.push((input, d + 1));
            }
            LogicalPlan::Sort { input, keys } => {
                let key_strs: Vec<String> = keys.iter().map(fmt_sort_key).collect();
                let _ = write!(out, "ExternalSortExec: [{}]", key_strs.join(", "));
                stack.push((input, d + 1));
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
                stack.push((input, d + 1));
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
                stack.push((input, d + 1));
            }
            LogicalPlan::Distinct { input } => {
                out.push_str("DistinctHashAggExec");
                stack.push((input, d + 1));
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
                stack.push((right, d + 1));
                stack.push((left, d + 1));
            }
            LogicalPlan::Union { inputs, all, .. } => {
                if *all {
                    out.push_str("UnionExec (all=true)");
                } else {
                    out.push_str("DistinctHashAggExec -> UnionExec (all=false)");
                }
                for branch in inputs.iter().rev() {
                    stack.push((branch, d + 1));
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
                stack.push((input, d + 1));
            }
            LogicalPlan::Explain { inner, .. } => {
                out.push_str("ExplainExec");
                stack.push((inner, d + 1));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use cetra_column::{
        DataType, Field, FlatColumn, FlatData, GlobalPool, ScalarValue, Schema, Utf8Data,
    };

    use super::*;
    use crate::eval::{read_i64, read_str};
    use crate::exec::source::TimeRangeHint;
    use crate::exec::{ExecMetrics, SpillManager, WorkerPool};
    use crate::sql::catalog::raw_scores_schema;
    use crate::sql::optimize::optimize;
    use crate::tombstone::{TombstoneSpec, TombstoneTable, covered_predicate};

    fn test_ctx(cap: usize) -> (ExecContext, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ExecContext {
            pool: GlobalPool::standalone(cap),
            eval: crate::ir::EvalContext::new(0, CancelToken::new()),
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
                map_projections: BTreeMap::new(),
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
                substrings: vec![vec![crate::exec::SubstringHint {
                    column: "reason".to_owned(),
                    needle: "secret-needle".to_owned(),
                    case_insensitive: true,
                }]],
                map_projections: BTreeMap::new(),
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
            map_projections: BTreeMap::new(),
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

    #[test]
    fn vec_batch_stream_tracks_and_releases_queued_memory() {
        let schema = Schema::try_new_arc(vec![Field::new("id", DataType::I64, false)]).unwrap();
        let b1 = Batch::try_new_all(
            Arc::clone(&schema),
            vec![Column::Flat(
                FlatColumn::try_new(FlatData::I64(vec![1, 2]), None).unwrap(),
            )],
            2,
        )
        .unwrap();
        let b2 = Batch::try_new_all(
            Arc::clone(&schema),
            vec![Column::Flat(
                FlatColumn::try_new(FlatData::I64(vec![3, 4]), None).unwrap(),
            )],
            2,
        )
        .unwrap();
        let bytes1 = b1.memory_bytes();
        let bytes2 = b2.memory_bytes();

        let (ctx, _dir) = test_ctx(1024 * 1024);
        let mut stream = VecBatchStream::new(vec![b1, b2], &ctx).unwrap();
        assert_eq!(ctx.pool.reserved(), bytes1 + bytes2);

        let mut pull_res = ctx.pool.register("pull", false);
        let got1 = stream
            .next_batch(&ctx.eval.cancel, Some(&mut pull_res))
            .unwrap()
            .unwrap();
        assert_eq!(got1.num_rows(), 2);
        assert_eq!(pull_res.size(), bytes1);
        assert_eq!(stream.queue_res.size(), bytes2);
        assert_eq!(ctx.pool.reserved(), bytes1 + bytes2);

        drop(got1);
        pull_res.free();
        assert_eq!(ctx.pool.reserved(), bytes2);

        // Dropping stream early frees remaining queued memory.
        drop(stream);
        assert_eq!(ctx.pool.reserved(), 0);
    }

    #[test]
    fn sql_inner_and_left_join_parallel_execution() {
        let left_schema = Schema::try_new_arc(vec![
            Field::new("k", DataType::Utf8, true),
            Field::new("lv", DataType::I64, false),
        ])
        .unwrap();
        let right_schema = Schema::try_new_arc(vec![
            Field::new("k", DataType::Utf8, true),
            Field::new("rv", DataType::I64, false),
        ])
        .unwrap();

        let mk_batch = |schema: &Arc<Schema>, keys: &[Option<&str>], vals: &[i64]| -> Batch {
            let mut raw = Vec::with_capacity(keys.len());
            let mut valid = cetra_column::Bitmap::new_ones(keys.len());
            for (i, k) in keys.iter().enumerate() {
                match k {
                    Some(s) => raw.push(*s),
                    None => {
                        raw.push("");
                        valid.set(i, false).unwrap();
                    }
                }
            }
            let nulls = if valid.all_ones() { None } else { Some(valid) };
            Batch::try_new_all(
                Arc::clone(schema),
                vec![
                    Column::Flat(
                        FlatColumn::try_new(
                            FlatData::Utf8(Utf8Data::try_from_slices(raw).unwrap()),
                            nulls,
                        )
                        .unwrap(),
                    ),
                    Column::Flat(FlatColumn::try_new(FlatData::I64(vals.to_vec()), None).unwrap()),
                ],
                keys.len(),
            )
            .unwrap()
        };

        let left_src = Arc::new(
            BatchSource::new(
                Arc::clone(&left_schema),
                vec![
                    mk_batch(&left_schema, &[Some("a"), None], &[1, 2]),
                    mk_batch(&left_schema, &[Some("b"), Some("c")], &[3, 4]),
                    mk_batch(&left_schema, &[Some("a"), Some("d")], &[5, 6]),
                ],
            )
            .unwrap(),
        );
        let right_src = Arc::new(
            BatchSource::new(
                Arc::clone(&right_schema),
                vec![
                    mk_batch(&right_schema, &[Some("a"), None], &[10, 20]),
                    mk_batch(&right_schema, &[Some("b"), Some("a")], &[30, 40]),
                ],
            )
            .unwrap(),
        );

        let mut catalog = SqlCatalog::new(None, None);
        catalog.register_batch_source("l_tbl", left_src).unwrap();
        catalog.register_batch_source("r_tbl", right_src).unwrap();
        let mut sources = SqlSources::new();
        sources.register_catalog_batches(&catalog);

        let (ctx, _dir) = test_ctx(8 * 1024 * 1024);
        let ctx = ExecContext {
            target_partitions: 3,
            batch_size: 2,
            ..ctx
        };

        let params = serde_json::Map::new();
        for (sql_no_order, sql, expected_rows) in [
            (
                "SELECT l.k, l.lv, r.rv FROM l_tbl l INNER JOIN r_tbl r ON l.k = r.k",
                "SELECT l.k, l.lv, r.rv FROM l_tbl l INNER JOIN r_tbl r ON l.k = r.k ORDER BY l.lv, r.rv",
                vec![
                    serde_json::json!({"k": "a", "lv": 1, "rv": 10}),
                    serde_json::json!({"k": "a", "lv": 1, "rv": 40}),
                    serde_json::json!({"k": "b", "lv": 3, "rv": 30}),
                    serde_json::json!({"k": "a", "lv": 5, "rv": 10}),
                    serde_json::json!({"k": "a", "lv": 5, "rv": 40}),
                ],
            ),
            (
                "SELECT l.k, l.lv, r.rv FROM l_tbl l LEFT JOIN r_tbl r ON l.k = r.k AND l.lv < 5",
                "SELECT l.k, l.lv, r.rv FROM l_tbl l LEFT JOIN r_tbl r ON l.k = r.k AND l.lv < 5 ORDER BY l.lv, r.rv",
                vec![
                    serde_json::json!({"k": "a", "lv": 1, "rv": 10}),
                    serde_json::json!({"k": "a", "lv": 1, "rv": 40}),
                    serde_json::json!({"k": null, "lv": 2, "rv": null}),
                    serde_json::json!({"k": "b", "lv": 3, "rv": 30}),
                    serde_json::json!({"k": "c", "lv": 4, "rv": null}),
                    serde_json::json!({"k": "a", "lv": 5, "rv": null}),
                    serde_json::json!({"k": "d", "lv": 6, "rv": null}),
                ],
            ),
        ] {
            let plan = crate::sql::prepare_sql_plan(&catalog, sql_no_order, &params).unwrap();
            let stage = compile_stage(&plan, &sources, &ctx).unwrap();
            assert_eq!(
                stage.streams.len(),
                3,
                "expected parallel streams for `{sql_no_order}`"
            );
            drop(stage);

            let res = crate::sql::sql_json(&catalog, &sources, &ctx, sql, &params, 100).unwrap();
            let rows: Vec<serde_json::Value> = res
                .rows
                .into_iter()
                .map(serde_json::Value::Object)
                .collect();
            assert_eq!(rows, expected_rows, "SQL output mismatch for `{sql}`");
            assert_eq!(ctx.pool.reserved(), 0);
        }
    }
}
