//! Native `ScoresSource` performing `created_at`-only pushdown, GDPR tombstone
//! filtering, and spillable per-`score_id` last-writer-wins deduplication
//! (`(score_id ASC, updated_at DESC NULLS FIRST, ingest_time DESC NULLS FIRST)`).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use cetra_column::{
    Batch, Bitmap, CancelToken, ColumnError, DataType, Reservation, ScalarValue, Schema, Selection,
};
use cetra_core::ids::ProjectId;
use cetra_core::schema::{scores, scores_schema};
use cetra_query::exec::sort_window::ExternalSort;
use cetra_query::exec::{
    ExecContext, IdLookupHint, PartitionStream, ScanSource, ScanSpec, SortKey, TimeRangeHint,
    run_pipeline,
};
use cetra_query::{BinaryOp, EvalContext, Expr, QueryError, UnaryOp, eval_filter};
use object_store::ObjectStore;

use crate::catalog::{FileMeta, FileTable};
use crate::deletion::{PendingDeletion, ProjectMemSnapshot};
use crate::file_format::{FileMetadataCache, open_table_source};
use crate::verify::Suspects;
use crate::wal::Table;

use super::mem_source::MemtableSource;
use super::{
    HashPartitionedSortSink, TimeBounds, cap_partitions, claim_shared_limit, extract_time_bounds,
    handoff_res, read_bool_at, read_str_at, reconcile_res, rollback_res,
};

/// Native [`ScanSource`] for the `scores` table with LWW deduplication per `score_id`.
#[derive(Clone)]
pub(crate) struct ScoresSource {
    store: Arc<dyn ObjectStore>,
    project: ProjectId,
    schema: Arc<Schema>,
    files: Vec<Arc<FileMeta>>,
    scores_mem: ProjectMemSnapshot,
    deletions: Arc<[Arc<PendingDeletion>]>,
    metadata_cache: Arc<FileMetadataCache>,
    suspects: Suspects,
    runtime: tokio::runtime::Handle,
    visible_from: Option<i64>,
}

impl std::fmt::Debug for ScoresSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mem_batches: usize = self
            .scores_mem
            .as_snapshot()
            .partitions
            .iter()
            .filter(|(key, _)| key.project == self.project)
            .map(|(_, batches)| batches.len())
            .sum();
        f.debug_struct("ScoresSource")
            .field("files", &self.files.len())
            .field("mem_batches", &mem_batches)
            .field("visible_from", &self.visible_from)
            .field("deletions", &self.deletions.len())
            .field("schema_len", &self.schema.len())
            .finish_non_exhaustive()
    }
}

impl ScoresSource {
    /// Creates a [`ScoresSource`] over `files` and pre-filtered `scores_mem` for `project`.
    ///
    /// `scores_mem` is enforced at compile time via [`ProjectMemSnapshot`] to have been projected
    /// by `deletion::project_snapshot(Table::Scores, ...)`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        store: Arc<dyn ObjectStore>,
        project: ProjectId,
        files: Vec<Arc<FileMeta>>,
        scores_mem: ProjectMemSnapshot,
        deletions: Arc<[Arc<PendingDeletion>]>,
        metadata_cache: Arc<FileMetadataCache>,
        suspects: Suspects,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, QueryError> {
        let schema = scores_schema();
        Ok(Self {
            store,
            project,
            schema,
            files,
            scores_mem,
            deletions,
            metadata_cache,
            suspects,
            runtime,
            visible_from: None,
        })
    }

    /// Sets an inclusive `created_at >= visible_from` visibility floor enforced on every scan.
    pub(crate) fn with_visible_from(mut self, visible_from: Option<i64>) -> Self {
        self.visible_from = visible_from;
        self
    }
}

impl ScanSource for ScoresSource {
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
        ctx: &ExecContext,
    ) -> Result<Vec<Box<dyn PartitionStream>>, QueryError> {
        let _ = self.projected_schema(spec)?;
        if ctx.eval.cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        if spec.limit == Some(0) {
            return Ok(Vec::new());
        }

        let created_at_idx = schema_col_idx(&self.schema, scores::CREATED_AT)?;
        let score_id_idx = schema_col_idx(&self.schema, scores::SCORE_ID)?;
        let updated_at_idx = schema_col_idx(&self.schema, scores::UPDATED_AT)?;
        let ingest_time_idx = schema_col_idx(&self.schema, scores::INGEST_TIME)?;
        let deleted_idx = schema_col_idx(&self.schema, scores::DELETED)?;

        // Mandatory pushdown safety invariant (`QE-M15`, H4a): ONLY immutable columns
        // (`created_at`, `score_id`) and their infallible `IdLookupHint`s are pushed below
        // LWW dedup (explicit `score_id` upserts may change `target`/`trace_id`, so `trace_id`
        // must remain post-LWW; still keeping `around_ns: None`).
        let mut pushed_immutable =
            immutable_pushdown_conjuncts(&spec.filters, &[created_at_idx, score_id_idx]);
        let pushed_id_lookups: Vec<IdLookupHint> = spec
            .id_lookups
            .iter()
            .filter(|h| {
                let col = match h {
                    IdLookupHint::Fixed16 { column, .. }
                    | IdLookupHint::Fixed8 { column, .. }
                    | IdLookupHint::Utf8 { column, .. } => column.as_str(),
                };
                col == scores::SCORE_ID
            })
            .cloned()
            .collect();
        let mut hint_bounds = TimeBounds {
            min: spec.time_range.min_ns,
            max: spec.time_range.max_ns,
        };
        if let Some(vf) = self.visible_from {
            hint_bounds.min = Some(hint_bounds.min.map_or(vf, |cur| cur.max(vf)));
            pushed_immutable.push(Expr::Binary {
                left: Box::new(Expr::Column {
                    index: created_at_idx,
                    name: scores::CREATED_AT.to_owned(),
                    data_type: DataType::timestamp_utc(),
                }),
                op: BinaryOp::GtEq,
                right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                    value: vf,
                    utc: true,
                })),
                data_type: DataType::Bool,
            });
        }
        let filter_bounds = extract_time_bounds(&pushed_immutable, scores::CREATED_AT);
        let effective_bounds = hint_bounds.intersect_with(&filter_bounds);
        if effective_bounds.contradicts() {
            return Ok(Vec::new());
        }

        // Underlying scan projection: requested ∪ filter_cols ∪ {score_id, updated_at, ingest_time, deleted}.
        let mut scan_set = BTreeSet::new();
        match &spec.projection {
            Some(indices) => {
                for &idx in indices {
                    if idx >= self.schema.len() {
                        return Err(ColumnError::InvalidBatch {
                            reason: "projection index out of bounds",
                        }
                        .into());
                    }
                    scan_set.insert(idx);
                }
            }
            None => {
                for idx in 0..self.schema.len() {
                    scan_set.insert(idx);
                }
            }
        }
        for filter in &spec.filters {
            filter.referenced_columns(&mut scan_set);
        }
        scan_set.insert(score_id_idx);
        scan_set.insert(updated_at_idx);
        scan_set.insert(ingest_time_idx);
        scan_set.insert(deleted_idx);

        let scan_indices: Vec<usize> = scan_set.into_iter().collect();
        let scan_schema = Arc::new(self.schema.project(&scan_indices)?);

        let mut full_to_scan: Vec<Option<usize>> = vec![None; self.schema.len()];
        for (scan_pos, &full_idx) in scan_indices.iter().enumerate() {
            full_to_scan[full_idx] = Some(scan_pos);
        }

        let scan_score_id_idx = full_to_scan[score_id_idx]
            .ok_or_else(|| QueryError::internal("score_id missing from scan schema"))?;
        let scan_updated_at_idx = full_to_scan[updated_at_idx]
            .ok_or_else(|| QueryError::internal("updated_at missing from scan schema"))?;
        let scan_ingest_time_idx = full_to_scan[ingest_time_idx]
            .ok_or_else(|| QueryError::internal("ingest_time missing from scan schema"))?;
        let scan_deleted_idx = full_to_scan[deleted_idx]
            .ok_or_else(|| QueryError::internal("deleted missing from scan schema"))?;

        let post_filters: Vec<Expr> = spec
            .filters
            .iter()
            .map(|f| f.remap_columns(&full_to_scan))
            .collect::<Result<Vec<_>, _>>()?;

        let scan_to_output: Option<Vec<usize>> = match &spec.projection {
            Some(indices) => {
                let mut out = Vec::with_capacity(indices.len());
                for &full_idx in indices {
                    let scan_pos = full_to_scan[full_idx]
                        .ok_or_else(|| QueryError::internal("projected column missing"))?;
                    out.push(scan_pos);
                }
                Some(out)
            }
            None => {
                // `scan_indices` is already `0..self.schema.len()` in schema order.
                None
            }
        };

        // Push immutable `created_at` bounds + infallible `created_at`/`score_id` filters & hints below LWW dedup.
        let inner_spec = ScanSpec {
            projection: Some(scan_indices),
            map_projections: spec.map_projections.clone(),
            time_range: TimeRangeHint {
                min_ns: effective_bounds.min,
                max_ns: effective_bounds.max,
                around_ns: None,
            },
            id_lookups: pushed_id_lookups,
            filters: pushed_immutable,
            substrings: Vec::new(),
            sort_hints: Vec::new(),
            limit: None,
        };

        let mut raw_streams: Vec<Box<dyn PartitionStream>> = Vec::new();
        if !self.files.is_empty() {
            let parquet_src = open_table_source(
                Arc::clone(&self.store),
                FileTable::Scores,
                Arc::clone(&self.schema),
                self.files.clone(),
                Arc::clone(&self.deletions),
                Arc::clone(&self.metadata_cache),
                self.suspects.clone(),
                self.runtime.clone(),
            );
            raw_streams.extend(parquet_src.open_partitions(&inner_spec, ctx)?);
        }

        let mem_src =
            MemtableSource::from_projected(Table::Scores, self.project, &self.scores_mem)?;
        raw_streams.extend(mem_src.open_partitions(&inner_spec, ctx)?);

        if raw_streams.is_empty() {
            return Ok(Vec::new());
        }
        let target_partitions = ctx.target_partitions.max(1);
        let capped_streams = cap_partitions(raw_streams, target_partitions);

        // Sort globally by (score_id ASC, updated_at DESC NULLS FIRST, ingest_time DESC NULLS FIRST).
        let sort_keys = vec![
            SortKey {
                expr: Expr::Column {
                    index: scan_score_id_idx,
                    name: scores::SCORE_ID.to_owned(),
                    data_type: DataType::Utf8,
                },
                descending: false,
                nulls_first: false,
            },
            SortKey {
                expr: Expr::Column {
                    index: scan_updated_at_idx,
                    name: scores::UPDATED_AT.to_owned(),
                    data_type: DataType::timestamp_utc(),
                },
                descending: true,
                nulls_first: true,
            },
            SortKey {
                expr: Expr::Column {
                    index: scan_ingest_time_idx,
                    name: scores::INGEST_TIME.to_owned(),
                    data_type: DataType::timestamp_utc(),
                },
                descending: true,
                nulls_first: true,
            },
        ];

        let sorted_streams: Vec<Box<dyn PartitionStream>> = if target_partitions == 1 {
            let sort_schema = Arc::clone(&scan_schema);
            let sort_keys_for_sink = sort_keys.clone();
            let ctx_for_sink = ctx.clone();
            let sinks = run_pipeline(
                ctx,
                capped_streams,
                |_| Ok(Vec::new()),
                move |_| {
                    Ok(ExternalSort::new(
                        Arc::clone(&sort_schema),
                        sort_keys_for_sink.clone(),
                        None,
                        ctx_for_sink.clone(),
                    ))
                },
            )?;
            vec![ExternalSort::merge_sinks(
                sinks,
                scan_schema,
                sort_keys,
                None,
                ctx,
            )?]
        } else {
            let part_schema = Arc::clone(&scan_schema);
            let ctx_for_sink = ctx.clone();
            let sinks = run_pipeline(
                ctx,
                capped_streams,
                |_| Ok(Vec::new()),
                move |_| {
                    Ok(HashPartitionedSortSink::new(
                        Arc::clone(&part_schema),
                        scan_score_id_idx,
                        target_partitions,
                        ctx_for_sink.clone(),
                    ))
                },
            )?;
            HashPartitionedSortSink::into_partition_streams(sinks, &scan_schema, &sort_keys, ctx)?
        };

        let shared_limit = spec.limit.map(|l| Arc::new(AtomicUsize::new(l)));
        let streams = sorted_streams
            .into_iter()
            .map(|input| {
                Box::new(ScoresDedupStream {
                    input,
                    scratch_res: ctx.pool.register("scores_dedup", false),
                    score_id_idx: scan_score_id_idx,
                    deleted_idx: scan_deleted_idx,
                    post_filters: post_filters.clone(),
                    projection: scan_to_output.clone(),
                    remaining: shared_limit.clone(),
                    eval: ctx.eval.clone(),
                    prev_score_id: None,
                    done: false,
                }) as Box<dyn PartitionStream>
            })
            .collect();
        Ok(streams)
    }

    fn explain_summary(&self, spec: &ScanSpec) -> String {
        format!(
            "ScoresSource(files={}, filters={}, deletions={})",
            self.files.len(),
            spec.filters.len(),
            self.deletions.len()
        )
    }
}

fn schema_col_idx(schema: &Schema, name: &str) -> Result<usize, QueryError> {
    schema
        .index_of(name)
        .ok_or_else(|| QueryError::internal(format!("scores schema missing column {name}")))
}

fn immutable_pushdown_conjuncts(filters: &[Expr], allowed_cols: &[usize]) -> Vec<Expr> {
    let mut out = Vec::new();
    for filter in filters {
        collect_allowed_conjuncts(filter, allowed_cols, &mut out);
    }
    out
}

fn is_infallible_pushdown(expr: &Expr) -> bool {
    let mut stack = vec![expr];
    while let Some(node) = stack.pop() {
        match node {
            Expr::Column { .. } | Expr::Literal(_) => {}
            Expr::Unary {
                op:
                    UnaryOp::Not
                    | UnaryOp::IsNull
                    | UnaryOp::IsNotNull
                    | UnaryOp::IsTrue
                    | UnaryOp::IsFalse
                    | UnaryOp::IsNotTrue
                    | UnaryOp::IsNotFalse,
                expr,
                ..
            } => {
                stack.push(expr);
            }
            Expr::Binary {
                left,
                op:
                    BinaryOp::Eq
                    | BinaryOp::NotEq
                    | BinaryOp::Lt
                    | BinaryOp::LtEq
                    | BinaryOp::Gt
                    | BinaryOp::GtEq
                    | BinaryOp::IsDistinctFrom
                    | BinaryOp::IsNotDistinctFrom
                    | BinaryOp::And
                    | BinaryOp::Or,
                right,
                ..
            } => {
                if matches!(left.data_type(), DataType::Decimal128 { .. })
                    || matches!(right.data_type(), DataType::Decimal128 { .. })
                {
                    return false;
                }
                stack.push(right);
                stack.push(left);
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                if matches!(expr.data_type(), DataType::Decimal128 { .. })
                    || matches!(low.data_type(), DataType::Decimal128 { .. })
                    || matches!(high.data_type(), DataType::Decimal128 { .. })
                {
                    return false;
                }
                stack.push(high);
                stack.push(low);
                stack.push(expr);
            }
            Expr::InList { expr, list, .. } => {
                if matches!(expr.data_type(), DataType::Decimal128 { .. })
                    || list
                        .iter()
                        .any(|e| matches!(e.data_type(), DataType::Decimal128 { .. }))
                {
                    return false;
                }
                for item in list {
                    stack.push(item);
                }
                stack.push(expr);
            }
            _ => return false,
        }
    }
    true
}

fn collect_allowed_conjuncts(expr: &Expr, allowed_cols: &[usize], out: &mut Vec<Expr>) {
    if let Expr::Binary {
        left,
        op: BinaryOp::And,
        right,
        ..
    } = expr
    {
        collect_allowed_conjuncts(left, allowed_cols, out);
        collect_allowed_conjuncts(right, allowed_cols, out);
        return;
    }
    let mut cols = BTreeSet::new();
    expr.referenced_columns(&mut cols);
    if !cols.is_empty()
        && cols.iter().all(|idx| allowed_cols.contains(idx))
        && is_infallible_pushdown(expr)
    {
        out.push(expr.clone());
    }
}

struct ScoresDedupStream {
    input: Box<dyn PartitionStream>,
    scratch_res: Reservation,
    score_id_idx: usize,
    deleted_idx: usize,
    post_filters: Vec<Expr>,
    projection: Option<Vec<usize>>,
    remaining: Option<Arc<AtomicUsize>>,
    eval: EvalContext,
    prev_score_id: Option<String>,
    done: bool,
}

impl PartitionStream for ScoresDedupStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_ref().map_or(0, |r| r.size());
        let eval_ctx = self.eval.with_cancel(cancel.clone());

        while !self.done {
            if cancel.is_cancelled() {
                self.scratch_res.free();
                rollback_res(res, before);
                return Err(QueryError::Cancelled);
            }
            if self
                .remaining
                .as_ref()
                .is_some_and(|r| r.load(Ordering::Acquire) == 0)
            {
                self.done = true;
                break;
            }

            let Some(batch) = (match self.input.next_batch(cancel, Some(&mut self.scratch_res)) {
                Ok(opt) => opt,
                Err(err) => {
                    self.scratch_res.free();
                    rollback_res(res, before);
                    return Err(err);
                }
            }) else {
                self.done = true;
                break;
            };
            if batch.is_empty() {
                self.scratch_res.free();
                continue;
            }

            let keep_res = self.build_lww_mask(&batch, cancel);
            let keep_mask = match keep_res {
                Ok(m) => m,
                Err(err) => {
                    self.scratch_res.free();
                    rollback_res(res, before);
                    return Err(err);
                }
            };

            let mut filtered = match batch.into_filter_mask(&keep_mask) {
                Ok(b) => b,
                Err(err) => {
                    self.scratch_res.free();
                    rollback_res(res, before);
                    return Err(err.into());
                }
            };
            drop(keep_mask);
            if let Err(err) = reconcile_res(Some(&mut self.scratch_res), 0, filtered.memory_bytes())
            {
                self.scratch_res.free();
                rollback_res(res, before);
                return Err(err);
            }
            if filtered.is_empty() {
                self.scratch_res.free();
                continue;
            }

            for filter in &self.post_filters {
                let mask =
                    match eval_filter(filter, &filtered, &eval_ctx, Some(&mut self.scratch_res)) {
                        Ok(m) => m,
                        Err(err) => {
                            self.scratch_res.free();
                            rollback_res(res, before);
                            return Err(err);
                        }
                    };
                filtered = match filtered.into_filter_mask(&mask) {
                    Ok(b) => b,
                    Err(err) => {
                        self.scratch_res.free();
                        rollback_res(res, before);
                        return Err(err.into());
                    }
                };
                drop(mask);
                if let Err(err) =
                    reconcile_res(Some(&mut self.scratch_res), 0, filtered.memory_bytes())
                {
                    self.scratch_res.free();
                    rollback_res(res, before);
                    return Err(err);
                }
                if filtered.is_empty() {
                    break;
                }
            }
            if filtered.is_empty() {
                self.scratch_res.free();
                continue;
            }

            if self.remaining.is_some() {
                let take = claim_shared_limit(self.remaining.as_ref(), filtered.num_rows());
                if take == 0 {
                    self.done = true;
                    self.scratch_res.free();
                    rollback_res(res, before);
                    return Ok(None);
                }
                if self
                    .remaining
                    .as_ref()
                    .is_some_and(|r| r.load(Ordering::Acquire) == 0)
                {
                    self.done = true;
                }
                if take < filtered.num_rows() {
                    self.done = true;
                    filtered = match truncate_batch_rows(filtered, take) {
                        Ok(b) => b,
                        Err(err) => {
                            self.scratch_res.free();
                            rollback_res(res, before);
                            return Err(err);
                        }
                    };
                    if let Err(err) =
                        reconcile_res(Some(&mut self.scratch_res), 0, filtered.memory_bytes())
                    {
                        self.scratch_res.free();
                        rollback_res(res, before);
                        return Err(err);
                    }
                }
            }

            if let Some(indices) = &self.projection {
                filtered = match filtered.into_project(indices) {
                    Ok(b) => b,
                    Err(err) => {
                        self.scratch_res.free();
                        rollback_res(res, before);
                        return Err(err.into());
                    }
                };
                if let Err(err) =
                    reconcile_res(Some(&mut self.scratch_res), 0, filtered.memory_bytes())
                {
                    self.scratch_res.free();
                    rollback_res(res, before);
                    return Err(err);
                }
            }

            if filtered.num_rows().saturating_mul(4) < filtered.raw_len() {
                let compacted = match filtered.compact(Some(&mut self.scratch_res)) {
                    Ok(b) => b,
                    Err(err) => {
                        self.scratch_res.free();
                        rollback_res(res, before);
                        return Err(err.into());
                    }
                };
                drop(filtered);
                filtered = compacted;
            }

            let out_bytes = filtered.memory_bytes();
            let handoff = handoff_res(&mut self.scratch_res, res.as_deref_mut(), before, out_bytes);
            self.scratch_res.free();
            handoff?;
            return Ok(Some(filtered));
        }

        self.scratch_res.free();
        rollback_res(res, before);
        Ok(None)
    }
}

impl ScoresDedupStream {
    fn build_lww_mask(
        &mut self,
        batch: &Batch,
        cancel: &CancelToken,
    ) -> Result<Bitmap, QueryError> {
        let raw_len = batch.raw_len();
        let score_id_col = batch.column(self.score_id_idx).ok_or_else(|| {
            QueryError::internal("score_id column missing from sorted scores batch")
        })?;
        let deleted_col = batch.column(self.deleted_idx).ok_or_else(|| {
            QueryError::internal("deleted column missing from sorted scores batch")
        })?;

        let mut keep = Bitmap::new_zeroed(raw_len);
        let mut check_row = |row: usize, prev: &mut Option<String>| -> Result<(), QueryError> {
            let sid = read_str_at(score_id_col, row)
                .ok_or_else(|| QueryError::internal("score_id must be a non-null Utf8 string"))?;
            if prev.as_deref() == Some(sid) {
                return Ok(());
            }
            *prev = Some(sid.to_owned());
            if read_bool_at(deleted_col, row) == Some(false) {
                let _ = keep.set(row, true);
            }
            Ok(())
        };

        match batch.selection() {
            Selection::All => {
                for row in 0..raw_len {
                    if (row & 1023) == 0 && cancel.is_cancelled() {
                        return Err(QueryError::Cancelled);
                    }
                    check_row(row, &mut self.prev_score_id)?;
                }
            }
            Selection::Mask(mask) => {
                for (i, row) in mask.iter_ones().enumerate() {
                    if (i & 1023) == 0 && cancel.is_cancelled() {
                        return Err(QueryError::Cancelled);
                    }
                    check_row(row, &mut self.prev_score_id)?;
                }
            }
            Selection::Indices(indices) => {
                for (i, &row) in indices.iter().enumerate() {
                    if (i & 1023) == 0 && cancel.is_cancelled() {
                        return Err(QueryError::Cancelled);
                    }
                    check_row(row as usize, &mut self.prev_score_id)?;
                }
            }
        }

        Ok(keep)
    }
}

fn truncate_batch_rows(batch: Batch, take: usize) -> Result<Batch, QueryError> {
    let active = batch.num_rows();
    if take >= active {
        return Ok(batch);
    }
    let new_sel = match batch.selection() {
        Selection::All => {
            let end = u32::try_from(take).map_err(|_| ColumnError::Overflow {
                reason: "scores limit exceeds u32::MAX",
            })?;
            Selection::Indices((0..end).collect())
        }
        Selection::Mask(mask) => {
            let indices: Vec<u32> = mask.iter_ones().take(take).map(|i| i as u32).collect();
            Selection::Indices(indices)
        }
        Selection::Indices(idx) => Selection::Indices(idx[..take].to_vec()),
    };
    Ok(batch.into_with_selection(new_sel)?)
}

#[cfg(test)]
mod tests {
    use cetra_column::{Column, FlatColumn, FlatData, GlobalPool, ScalarValue, Utf8Data};
    use cetra_core::ids::TraceId;
    use cetra_query::exec::{CollectSink, ExecMetrics, SpillManager, WorkerPool};
    use cetra_query::{CastMode, ScalarFn};
    use chrono::NaiveDate;
    use object_store::memory::InMemory;

    use super::*;
    use crate::engine::day_start_ns;
    use crate::memtable::Memtable;
    use crate::test_support::{score_row, score_rows};
    use crate::verify::VerifyQueue;

    fn test_project() -> ProjectId {
        "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
    }

    fn test_trace_id(tag: u8) -> TraceId {
        TraceId::from_bytes([tag; 16]).unwrap()
    }

    fn make_exec_ctx(dir: &std::path::Path) -> ExecContext {
        ExecContext {
            pool: GlobalPool::standalone(16 * 1024 * 1024),
            eval: EvalContext::new(0, CancelToken::new()),
            spill: Arc::new(SpillManager::with_fixed_cap(dir.to_path_buf(), 0)),
            workers: Arc::new(WorkerPool::new(1).unwrap()),
            batch_size: 16,
            target_partitions: 1,
            metrics: ExecMetrics::default(),
        }
    }

    fn run_scores(
        source: &ScoresSource,
        spec: &ScanSpec,
        ctx: &ExecContext,
    ) -> Result<Vec<Batch>, QueryError> {
        let streams = source.open_partitions(spec, ctx)?;
        let pool = Arc::clone(&ctx.pool);
        let sinks = run_pipeline(
            ctx,
            streams,
            |_| Ok(Vec::new()),
            move |_| Ok(CollectSink::new(&pool)),
        )?;
        Ok(sinks
            .into_iter()
            .flat_map(CollectSink::into_batches)
            .collect())
    }

    #[test]
    fn cast_score_id_stays_post_lww_and_does_not_fail_on_deleted_non_numeric_row() {
        let schema = scores_schema();
        let score_id_idx = schema.index_of(scores::SCORE_ID).unwrap();
        let created_at_idx = schema.index_of(scores::CREATED_AT).unwrap();
        let trace_id_idx = schema.index_of(scores::TRACE_ID).unwrap();

        let cast_filter = Expr::Binary {
            left: Box::new(Expr::Cast {
                expr: Box::new(Expr::Column {
                    index: score_id_idx,
                    name: scores::SCORE_ID.to_owned(),
                    data_type: DataType::Utf8,
                }),
                target: DataType::I64,
                mode: CastMode::Strict,
            }),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::I64(1))),
            data_type: DataType::Bool,
        };
        let trace_filter = Expr::Binary {
            left: Box::new(Expr::Column {
                index: trace_id_idx,
                name: scores::TRACE_ID.to_owned(),
                data_type: DataType::FixedBytes16,
            }),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::FixedBytes16(
                *test_trace_id(1).as_bytes(),
            ))),
            data_type: DataType::Bool,
        };
        let created_filter = Expr::Binary {
            left: Box::new(Expr::Column {
                index: created_at_idx,
                name: scores::CREATED_AT.to_owned(),
                data_type: DataType::timestamp_utc(),
            }),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                value: 0,
                utc: true,
            })),
            data_type: DataType::Bool,
        };
        let call_filter = Expr::Binary {
            left: Box::new(Expr::Call {
                func: ScalarFn::Length,
                args: vec![Expr::Column {
                    index: score_id_idx,
                    name: scores::SCORE_ID.to_owned(),
                    data_type: DataType::Utf8,
                }],
                data_type: DataType::I64,
            }),
            op: BinaryOp::Gt,
            right: Box::new(Expr::Literal(ScalarValue::I64(0))),
            data_type: DataType::Bool,
        };

        let pushed = immutable_pushdown_conjuncts(
            &[
                cast_filter.clone(),
                trace_filter,
                created_filter.clone(),
                call_filter,
            ],
            &[created_at_idx, score_id_idx],
        );
        assert_eq!(pushed, vec![created_filter]);

        let day = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let t0 = day_start_ns(day).unwrap();
        let base = score_rows(&[
            score_row(1, 1, t0, 100, 100, false, "acc"),
            score_row(2, 1, t0, 100, 100, false, "acc"),
            score_row(2, 1, t0, 200, 200, true, "acc"),
        ]);
        let (_, mut cols, raw_len, sel) = base.into_parts();
        cols[score_id_idx] = Arc::new(Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(["1", "not-a-number", "not-a-number"]).unwrap(),
                ),
                None,
            )
            .unwrap(),
        ));
        let batch = Batch::try_new_shared(Arc::clone(&schema), cols, raw_len, sel).unwrap();

        let mut mem = Memtable::new(Table::Scores);
        mem.insert(test_project(), batch, 1).unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let projected = crate::deletion::project_snapshot(
            Table::Scores,
            &mem.snapshot(),
            test_project(),
            None,
            &[],
        )
        .unwrap();
        let source = ScoresSource::new(
            Arc::new(InMemory::new()),
            test_project(),
            Vec::new(),
            projected,
            Arc::from([]),
            Arc::new(FileMetadataCache::new(1024 * 1024)),
            Suspects::new(Arc::new(VerifyQueue::default())),
            rt.handle().clone(),
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_exec_ctx(dir.path());
        let spec = ScanSpec {
            projection: Some(vec![score_id_idx]),
            filters: vec![cast_filter],
            ..Default::default()
        };
        let out = run_scores(&source, &spec, &ctx).unwrap();
        let total: usize = out.iter().map(Batch::num_rows).sum();
        assert_eq!(total, 1);
        let b = out[0].compact(None).unwrap();
        assert_eq!(read_str_at(b.column(0).unwrap(), 0), Some("1"));
    }

    #[test]
    fn trace_id_upsert_is_not_pushed_below_lww() {
        let schema = scores_schema();
        let score_id_idx = schema.index_of(scores::SCORE_ID).unwrap();
        let trace_id_idx = schema.index_of(scores::TRACE_ID).unwrap();

        let day = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let t0 = day_start_ns(day).unwrap();
        let old_trace = *test_trace_id(1).as_bytes();
        let mut old_row = score_row(10, 1, t0, 100, 100, false, "acc");
        old_row.target = cetra_core::score::ScoreTarget::Trace {
            trace_id: test_trace_id(1),
        };
        let mut new_row = score_row(10, 2, t0, 200, 200, false, "acc");
        new_row.target = cetra_core::score::ScoreTarget::Trace {
            trace_id: test_trace_id(2),
        };
        let batch = score_rows(&[old_row, new_row]);

        let mut mem = Memtable::new(Table::Scores);
        mem.insert(test_project(), batch, 1).unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let projected = crate::deletion::project_snapshot(
            Table::Scores,
            &mem.snapshot(),
            test_project(),
            None,
            &[],
        )
        .unwrap();
        let source = ScoresSource::new(
            Arc::new(InMemory::new()),
            test_project(),
            Vec::new(),
            projected,
            Arc::from([]),
            Arc::new(FileMetadataCache::new(1024 * 1024)),
            Suspects::new(Arc::new(VerifyQueue::default())),
            rt.handle().clone(),
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_exec_ctx(dir.path());

        let spec_old = ScanSpec {
            projection: Some(vec![score_id_idx]),
            id_lookups: vec![IdLookupHint::Fixed16 {
                column: scores::TRACE_ID.to_owned(),
                values: vec![old_trace],
            }],
            filters: vec![Expr::Binary {
                left: Box::new(Expr::Column {
                    index: trace_id_idx,
                    name: scores::TRACE_ID.to_owned(),
                    data_type: DataType::FixedBytes16,
                }),
                op: BinaryOp::Eq,
                right: Box::new(Expr::Literal(ScalarValue::FixedBytes16(old_trace))),
                data_type: DataType::Bool,
            }],
            ..Default::default()
        };
        let out_old = run_scores(&source, &spec_old, &ctx).unwrap();
        let total_old: usize = out_old.iter().map(Batch::num_rows).sum();
        assert_eq!(total_old, 0, "superseded trace_id must not match after LWW");

        let new_trace = *test_trace_id(2).as_bytes();
        let spec_new = ScanSpec {
            projection: Some(vec![score_id_idx]),
            id_lookups: vec![IdLookupHint::Fixed16 {
                column: scores::TRACE_ID.to_owned(),
                values: vec![new_trace],
            }],
            filters: vec![Expr::Binary {
                left: Box::new(Expr::Column {
                    index: trace_id_idx,
                    name: scores::TRACE_ID.to_owned(),
                    data_type: DataType::FixedBytes16,
                }),
                op: BinaryOp::Eq,
                right: Box::new(Expr::Literal(ScalarValue::FixedBytes16(new_trace))),
                data_type: DataType::Bool,
            }],
            ..Default::default()
        };
        let out_new = run_scores(&source, &spec_new, &ctx).unwrap();
        let total_new: usize = out_new.iter().map(Batch::num_rows).sum();
        assert_eq!(total_new, 1, "latest upserted trace_id must match");
    }
}
