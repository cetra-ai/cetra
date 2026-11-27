//! Native Statam `ScanSource` with metadata caching, SBBF/zone-map/trigram/shredded-map
//! pushdown, CNF OR substring pushdown, 3VL `FilterExpr` per-file exact-pushed vs residual
//! splitting, RG partitioning with `WorkerPoolExecutor`, zero-I/O `COUNT(*)`, direct `take_rows`,
//! mini-block streaming, inline tombstones, cooperative cancellation, exact memory reservation
//! accounting, and corrupt-file `Suspects` reporting.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;

use cetra_column::{
    Batch, CancelToken, Column, ColumnError, DataType, Field, FlatColumn, FlatData, Reservation,
    ScalarValue, Schema, Selection,
};
use cetra_core::schema::traces;
use cetra_query::exec::source::collect_expr_map_projections;
use cetra_query::exec::{
    ExecContext, IdLookupHint, MapProjection, PartitionStream, ScanSource, ScanSpec,
};
use cetra_query::{BinaryOp, EvalContext, Expr, QueryError, ScalarFn, UnaryOp, covered_predicate};
use cetra_statam::codec::dict::FileDict;
use cetra_statam::format::BLOCK_FLAG_HAS_NAN;
use cetra_statam::simd::bloom::BloomIndex;
use cetra_statam::{
    BlockEntry, ChunkPackReader, CoalesceMode, FilterExpr, IdLookup, LocalityGroup, MapReadMode,
    PackResolver, PackToc, Predicate, ScanRequest, StatamError, StatamFileMeta, StatamReader,
    StatamScanIter, SubstringQuery, TaskExecutor, TimeRange, ZonePred, ZoneValue,
    statam_bloom_hash, zone_map_may_match,
};
use chrono::NaiveDate;

use crate::catalog::{FileId, FileMeta, FileTable, PackMeta};
use crate::deletion::{PendingDeletion, applicable};
use crate::objects::StorageIo;
use crate::statam_io::io::open_range_reader;
use crate::statam_io::time_column;
use crate::unwind::guard_sync;
use crate::verify::Suspects;

use super::metadata_cache::MetadataCache;
use super::{
    DAY_NS, WorkerPoolExecutor, apply_tombstones_and_filters, day_of_ns, reconcile_res,
    rollback_res, to_tombstone_specs, tombstone_table,
};

/// Return type of [`StatamSource::open_with_row_coords`].
pub(crate) type PartitionStreamsWithSchema = (Vec<Box<dyn PartitionStream>>, Arc<Schema>);

/// Virtual column name appended by [`StatamSource::open_with_row_coords`] carrying the
/// 0-based `u32` file ordinal (as `I64`) into `StatamSource::files`.
pub(crate) const ROW_COORD_FILE_COL: &str = "__cetra_file_ord";
/// Virtual column name appended by [`StatamSource::open_with_row_coords`] carrying the
/// 0-based file-global `u64` row index (as `I64`).
pub(crate) const ROW_COORD_ROW_COL: &str = "__cetra_row_idx";

/// Minimum uncached file byte size before `open_partitions` eagerly reads Hot Tail metadata
/// to split Row Groups across worker partitions (`16 MiB`).
const RG_SPLIT_OPEN_THRESHOLD_BYTES: u64 = 16 * 1024 * 1024;
/// Maximum rows emitted per 0-column batch on the zero-I/O `COUNT(*)` fast path.
const ZERO_IO_COUNT_MAX_CHUNK: usize = 65_536;

/// Native Statam [`ScanSource`].
#[derive(Clone)]
pub(crate) struct StatamSource {
    io: StorageIo,
    table: FileTable,
    schema: Arc<Schema>,
    files: Vec<Arc<FileMeta>>,
    deletions: Arc<[Arc<PendingDeletion>]>,
    metadata_cache: Arc<MetadataCache>,
    suspects: Suspects,
    runtime: tokio::runtime::Handle,
    /// When `true`, files whose mini-blocks exceed `exec_ctx.batch_size` are read in
    /// `exec_ctx.batch_size` row windows via `take_rows` (used by k-way compaction merge so
    /// many concurrent input streams stay within `compaction.max_memory`).
    chunk_to_batch_size: bool,
}

impl fmt::Debug for StatamSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatamSource")
            .field("table", &self.table.as_str())
            .field("files", &self.files.len())
            .field("deletions", &self.deletions.len())
            .field("schema_len", &self.schema.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct FileWork {
    file_ordinal: u32,
    file: Arc<FileMeta>,
    rg_indices: Option<Vec<usize>>,
}

impl StatamSource {
    /// Creates a [`StatamSource`] over `files` backed by `io`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        io: StorageIo,
        table: FileTable,
        schema: Arc<Schema>,
        files: Vec<Arc<FileMeta>>,
        deletions: Arc<[Arc<PendingDeletion>]>,
        metadata_cache: Arc<MetadataCache>,
        suspects: Suspects,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            io,
            table,
            schema,
            files,
            deletions,
            metadata_cache,
            suspects,
            runtime,
            chunk_to_batch_size: false,
        }
    }

    /// Bounds per-batch decoded rows to `exec_ctx.batch_size` when a file's mini-blocks
    /// are larger than `exec_ctx.batch_size` (used by compaction's k-way merge).
    pub(crate) fn with_chunk_to_batch_size(mut self) -> Self {
        self.chunk_to_batch_size = true;
        self
    }

    fn prune_files_indexed(
        &self,
        spec: &ScanSpec,
    ) -> Result<Vec<(u32, Arc<FileMeta>)>, QueryError> {
        let min_ns = spec.time_range.min_ns;
        let max_ns = spec.time_range.max_ns;
        if let (Some(lo), Some(hi)) = (min_ns, max_ns)
            && lo > hi
        {
            return Ok(Vec::new());
        }
        if spec.id_lookups.iter().any(id_lookup_is_empty) {
            return Ok(Vec::new());
        }

        let (day_start, day_end) = file_day_bounds(self.table, min_ns, max_ns);

        // `around_ns` applies ONLY to `Spans`/`Traces` when `spec.id_lookups` contains a
        // `trace_id` `Fixed16` lookup and `time_range` is open (D154).
        let has_trace_lookup = spec.id_lookups.iter().any(|h| {
            matches!(
                h,
                IdLookupHint::Fixed16 { column, .. } if column == traces::TRACE_ID
            )
        });
        let around_window = if self.table != FileTable::Scores
            && has_trace_lookup
            && min_ns.is_none()
            && max_ns.is_none()
        {
            spec.time_range
                .around_ns
                .map(|t| (t.saturating_sub(DAY_NS), t.saturating_add(DAY_NS)))
        } else {
            None
        };

        let mut kept = Vec::new();
        for (idx, file) in self.files.iter().enumerate() {
            if file.rows == 0 {
                continue;
            }
            if file.day < day_start || file.day > day_end {
                continue;
            }
            if !file_overlaps(file, min_ns, max_ns) {
                continue;
            }
            if let Some((lo, hi)) = around_window
                && !file_overlaps(file, Some(lo), Some(hi))
            {
                continue;
            }
            let ord = u32::try_from(idx).map_err(|_| QueryError::TooLarge)?;
            kept.push((ord, Arc::clone(file)));
        }
        Ok(kept)
    }

    #[cfg(test)]
    fn prune_files(&self, spec: &ScanSpec) -> Vec<Arc<FileMeta>> {
        self.prune_files_indexed(spec)
            .unwrap_or_default()
            .into_iter()
            .map(|(_, f)| f)
            .collect()
    }

    /// Opens partition streams emitting the projected columns from `spec` PLUS two trailing
    /// `I64` coordinate columns ([`ROW_COORD_FILE_COL`], [`ROW_COORD_ROW_COL`]) suitable for
    /// a subsequent [`Self::take_rows`] Phase-2 fetch.
    pub(crate) fn open_with_row_coords(
        &self,
        spec: &ScanSpec,
        ctx: &ExecContext,
    ) -> Result<PartitionStreamsWithSchema, QueryError> {
        let base_schema = self.projected_schema(spec)?;
        let mut fields = base_schema.fields().to_vec();
        fields.push(Field::new(ROW_COORD_FILE_COL, DataType::I64, false));
        fields.push(Field::new(ROW_COORD_ROW_COL, DataType::I64, false));
        let coord_schema = Schema::try_new_arc(fields)?;
        let streams = self.open_partitions_inner(spec, ctx, Arc::clone(&coord_schema), true)?;
        Ok((streams, coord_schema))
    }

    /// Directly materializes the requested `(file_ordinal, row_idx)` coordinates with
    /// `projection` using [`StatamReader::take_rows_with_limits`], respecting cancellation,
    /// `Reservation` accounting, schema evolution, metadata caching, and corrupt-file reporting.
    pub(crate) fn take_rows(
        &self,
        coords: &[(u32, u64)],
        projection: &[usize],
        ctx: &ExecContext,
        res: &mut Reservation,
    ) -> Result<Vec<Batch>, QueryError> {
        for &idx in projection {
            if idx >= self.schema.len() {
                return Err(ColumnError::InvalidBatch {
                    reason: "projection index out of bounds",
                }
                .into());
            }
        }
        let out_schema = Arc::new(self.schema.project(projection)?);
        if coords.is_empty() {
            return Ok(Vec::new());
        }
        if ctx.eval.cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }

        let mut by_file: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
        for &(file_ord, row_idx) in coords {
            if (file_ord as usize) >= self.files.len() {
                return Err(QueryError::internal("take_rows file ordinal out of bounds"));
            }
            by_file.entry(file_ord).or_default().push(row_idx);
        }

        let touched_files: Vec<Arc<FileMeta>> = by_file
            .keys()
            .map(|&ord| Arc::clone(&self.files[ord as usize]))
            .collect();
        self.suspects.record(&touched_files);

        let res_before = res.size();
        let (read_threads, executor) = make_query_executor(ctx);
        let mut out_batches: Vec<Batch> = Vec::new();

        for (file_ord, row_ids) in by_file {
            if ctx.eval.cancel.is_cancelled() {
                rollback_res(Some(res), res_before);
                return Err(QueryError::Cancelled);
            }
            let file = &self.files[file_ord as usize];
            let rr = open_range_reader(
                &self.io,
                &file.path,
                file.size,
                self.runtime.clone(),
                ctx.eval.cancel.clone(),
            )
            .map_err(|e| {
                rollback_res(Some(res), res_before);
                map_statam_err(&self.suspects, file.id, e)
            })?;

            let meta = if let Some(cached) = self.metadata_cache.get_statam(file.id) {
                cached
            } else {
                let loaded = guard_sync(|| {
                    StatamFileMeta::read_from_with_hint(rr.as_ref(), Some(file.footer_size))
                })
                .map_err(|e| {
                    rollback_res(Some(res), res_before);
                    map_statam_err(&self.suspects, file.id, e)
                })?;
                let meta = Arc::new(loaded);
                self.metadata_cache
                    .insert_statam(file.id, Arc::clone(&meta));
                meta
            };

            let file_schema = &meta.schema_meta.schema;
            let mut file_proj = Vec::with_capacity(projection.len());
            let mut slots = Vec::with_capacity(projection.len());
            let mut has_missing_col = false;
            for &full_idx in projection {
                let field = &self.schema.fields()[full_idx];
                match file_schema.index_of(&field.name) {
                    Some(file_idx) => {
                        if file_schema.fields()[file_idx].data_type != field.data_type {
                            self.suspects.report_corrupt(file.id);
                            rollback_res(Some(res), res_before);
                            return Err(QueryError::CorruptData(
                                "column type mismatch in statam file".to_owned(),
                            ));
                        }
                        let file_pos = file_proj.len();
                        file_proj.push(file_idx);
                        slots.push(ScanColSlot::FromFile(file_pos));
                    }
                    None => {
                        if !field.nullable {
                            self.suspects.report_corrupt(file.id);
                            rollback_res(Some(res), res_before);
                            return Err(QueryError::CorruptData(
                                "missing non-nullable column in statam file".to_owned(),
                            ));
                        }
                        has_missing_col = true;
                        slots.push(ScanColSlot::FillNull(field.data_type.clone()));
                    }
                }
            }

            let packs: Option<Arc<dyn PackResolver>> = file.pack.as_ref().map(|pack| {
                Arc::new(SingleFilePackResolver {
                    pack: pack.clone(),
                    io: self.io.clone(),
                    metadata_cache: Arc::clone(&self.metadata_cache),
                    runtime: self.runtime.clone(),
                    cancel: ctx.eval.cancel.clone(),
                }) as Arc<dyn PackResolver>
            });

            let mut reader =
                guard_sync(|| StatamReader::open_with_shared_meta(rr, Arc::clone(&meta), packs))
                    .map_err(|e| {
                        rollback_res(Some(res), res_before);
                        map_statam_err(&self.suspects, file.id, e)
                    })?;
            if let Some(exec) = &executor {
                reader = reader.with_executor(Arc::clone(exec));
            }

            let batches = guard_sync(|| {
                reader.take_rows_with_limits(
                    &row_ids,
                    Some(&file_proj),
                    read_threads,
                    Some(&ctx.eval.cancel),
                    Some(res),
                )
            })
            .map_err(|e| {
                rollback_res(Some(res), res_before);
                map_statam_err(&self.suspects, file.id, e)
            })?;

            if has_missing_col {
                let fill_layout = ScanBatchFill {
                    schema: Arc::clone(&out_schema),
                    slots,
                };
                for batch in batches {
                    if batch.is_empty() {
                        continue;
                    }
                    let filled = fill_missing_columns(&batch, &fill_layout)
                        .inspect_err(|_| rollback_res(Some(res), res_before))?;
                    out_batches.push(filled);
                }
            } else {
                for batch in batches {
                    if !batch.is_empty() {
                        out_batches.push(batch);
                    }
                }
            }
        }

        let total_bytes: usize = out_batches
            .iter()
            .map(Batch::memory_bytes)
            .fold(0usize, usize::saturating_add);
        reconcile_res(Some(res), res_before, total_bytes)?;
        Ok(out_batches)
    }

    fn open_partitions_inner(
        &self,
        spec: &ScanSpec,
        ctx: &ExecContext,
        projected_schema: Arc<Schema>,
        emit_row_coords: bool,
    ) -> Result<Vec<Box<dyn PartitionStream>>, QueryError> {
        if spec.limit == Some(0) {
            return Ok(Vec::new());
        }
        let kept_files = self.prune_files_indexed(spec)?;
        if kept_files.is_empty() {
            return Ok(Vec::new());
        }
        let kept_metas: Vec<Arc<FileMeta>> =
            kept_files.iter().map(|(_, f)| Arc::clone(f)).collect();
        self.suspects.record(&kept_metas);

        let plan = Arc::new(CompiledScanSpec::compile(
            self.table,
            &self.schema,
            projected_schema,
            spec,
            emit_row_coords,
        )?);

        let target = ctx.target_partitions.max(1);
        let allow_rg_split = kept_files.len() < target
            && !(spec.limit.is_some() && plan.eligible_sort_desc)
            && !self.chunk_to_batch_size;

        let groups: Vec<Vec<FileWork>> = if allow_rg_split {
            self.build_rg_partitioned_groups(kept_files, &plan, ctx, target)?
        } else {
            distribute_file_work(
                kept_files
                    .into_iter()
                    .map(|(file_ordinal, file)| FileWork {
                        file_ordinal,
                        file,
                        rg_indices: None,
                    })
                    .collect(),
                target,
            )
        };

        if groups.is_empty() {
            return Ok(Vec::new());
        }

        // M3.5.3: When only 1 partition is produced (1-RG heavy decode / 1-RG `.stpk` search),
        // dispatch Statam's internal parallel decode/search onto `ctx.workers` via `run_helping`.
        let (read_threads, executor) = if groups.len() == 1 && !self.chunk_to_batch_size {
            make_query_executor(ctx)
        } else {
            (1, None)
        };

        let streams = groups
            .into_iter()
            .map(|files| {
                Box::new(StatamPartitionStream {
                    io: self.io.clone(),
                    table: self.table,
                    schema: Arc::clone(&self.schema),
                    files: files.into(),
                    deletions: Arc::clone(&self.deletions),
                    metadata_cache: Arc::clone(&self.metadata_cache),
                    suspects: self.suspects.clone(),
                    runtime: self.runtime.clone(),
                    plan: Arc::clone(&plan),
                    remaining: plan.stream_limit,
                    eval: ctx.eval.clone(),
                    batch_size: ctx.batch_size.max(1),
                    chunk_to_batch_size: self.chunk_to_batch_size,
                    read_threads,
                    executor: executor.clone(),
                    active_file: None,
                }) as Box<dyn PartitionStream>
            })
            .collect();
        Ok(streams)
    }

    /// Splits multi-RG files across `target` partitions using strictly in-memory metadata
    /// (cached `StatamFileMeta`, or eagerly opening Hot Tails only when `total_bytes >= 16 MiB`
    /// and the file does not qualify for Zero-I/O `COUNT(*)`).
    fn build_rg_partitioned_groups(
        &self,
        kept_files: Vec<(u32, Arc<FileMeta>)>,
        plan: &CompiledScanSpec,
        ctx: &ExecContext,
        target: usize,
    ) -> Result<Vec<Vec<FileWork>>, QueryError> {
        let mut buckets: Vec<Vec<FileWork>> = (0..target).map(|_| Vec::new()).collect();
        let mut rr_cursor: usize = 0;

        for (file_ordinal, file) in kept_files {
            if ctx.eval.cancel.is_cancelled() {
                return Err(QueryError::Cancelled);
            }
            // Never open uncached metadata for a file that qualifies for Zero-I/O COUNT(*).
            if can_zero_io_count(&file, None, &self.deletions, self.table, &self.schema, plan) {
                buckets[rr_cursor % target].push(FileWork {
                    file_ordinal,
                    file,
                    rg_indices: None,
                });
                rr_cursor += 1;
                continue;
            }

            let meta_opt = if let Some(cached) = self.metadata_cache.get_statam(file.id) {
                Some(cached)
            } else if file.total_bytes() >= RG_SPLIT_OPEN_THRESHOLD_BYTES {
                let rr = open_range_reader(
                    &self.io,
                    &file.path,
                    file.size,
                    self.runtime.clone(),
                    ctx.eval.cancel.clone(),
                )
                .map_err(|e| map_statam_err(&self.suspects, file.id, e))?;
                let loaded = guard_sync(|| {
                    StatamFileMeta::read_from_with_hint(rr.as_ref(), Some(file.footer_size))
                })
                .map_err(|e| map_statam_err(&self.suspects, file.id, e))?;
                let meta = Arc::new(loaded);
                self.metadata_cache
                    .insert_statam(file.id, Arc::clone(&meta));
                Some(meta)
            } else {
                None
            };

            let Some(meta) = meta_opt else {
                buckets[rr_cursor % target].push(FileWork {
                    file_ordinal,
                    file,
                    rg_indices: None,
                });
                rr_cursor += 1;
                continue;
            };

            if meta.rg_dir.len() <= 1 {
                buckets[rr_cursor % target].push(FileWork {
                    file_ordinal,
                    file,
                    rg_indices: None,
                });
                rr_cursor += 1;
                continue;
            }

            // Strictly in-memory RG pre-pruning (NEVER fetches Lazy blooms or Lazy dicts).
            let surviving_rgs = pre_prune_rgs_in_memory(&meta, self.table, &self.schema, plan)
                .map_err(|e| map_statam_err(&self.suspects, file.id, e))?;

            if surviving_rgs.is_empty() {
                continue;
            }

            if surviving_rgs.len() == 1 {
                buckets[rr_cursor % target].push(FileWork {
                    file_ordinal,
                    file,
                    rg_indices: Some(surviving_rgs),
                });
                rr_cursor += 1;
            } else {
                let mut per_bucket: Vec<Vec<usize>> = (0..target).map(|_| Vec::new()).collect();
                for rg_idx in surviving_rgs {
                    per_bucket[rr_cursor % target].push(rg_idx);
                    rr_cursor += 1;
                }
                for (b_idx, rgs) in per_bucket.into_iter().enumerate() {
                    if !rgs.is_empty() {
                        buckets[b_idx].push(FileWork {
                            file_ordinal,
                            file: Arc::clone(&file),
                            rg_indices: Some(rgs),
                        });
                    }
                }
            }
        }

        Ok(buckets.into_iter().filter(|b| !b.is_empty()).collect())
    }
}

fn make_query_executor(ctx: &ExecContext) -> (usize, Option<Arc<dyn TaskExecutor>>) {
    let threads = ctx.target_partitions.clamp(1, 4);
    if threads > 1 && ctx.workers.num_workers() > 1 {
        let exec: Arc<dyn TaskExecutor> = Arc::new(WorkerPoolExecutor::new(
            Arc::clone(&ctx.workers),
            ctx.eval.cancel.clone(),
            threads,
        ));
        (threads, Some(exec))
    } else {
        (1, None)
    }
}

fn map_statam_err(suspects: &Suspects, file_id: FileId, err: StatamError) -> QueryError {
    if err.is_corrupt() {
        suspects.report_corrupt(file_id);
        return QueryError::CorruptData("corrupt statam file".to_owned());
    }
    if err.is_store_error() {
        return QueryError::StorageIo("storage read error".to_owned());
    }
    match err {
        StatamError::Cancelled => QueryError::Cancelled,
        StatamError::MemoryExceeded => QueryError::TooLarge,
        StatamError::Column(c) => QueryError::Column(c),
        _ => QueryError::internal("statam read failed"),
    }
}

impl ScanSource for StatamSource {
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
        let projected_schema = self.projected_schema(spec)?;
        self.open_partitions_inner(spec, ctx, projected_schema, false)
    }

    fn explain_summary(&self, spec: &ScanSpec) -> String {
        format!(
            "StatamSource(table={}, files={}, filters={}, deletions={})",
            self.table.as_str(),
            self.files.len(),
            spec.filters.len(),
            self.deletions.len()
        )
    }
}

fn id_lookup_is_empty(hint: &IdLookupHint) -> bool {
    match hint {
        IdLookupHint::Fixed16 { values, .. } => values.is_empty(),
        IdLookupHint::Fixed8 { values, .. } => values.is_empty(),
        IdLookupHint::Utf8 { values, .. } => values.is_empty(),
    }
}

fn file_day_bounds(
    table: FileTable,
    min_ns: Option<i64>,
    max_ns: Option<i64>,
) -> (NaiveDate, NaiveDate) {
    let start = min_ns.and_then(day_of_ns).unwrap_or(NaiveDate::MIN);
    let end = max_ns.and_then(day_of_ns).unwrap_or(NaiveDate::MAX);
    if start > end {
        return (NaiveDate::MIN, NaiveDate::MAX);
    }
    match table {
        FileTable::Spans | FileTable::Traces => (
            start.pred_opt().unwrap_or(NaiveDate::MIN),
            end.succ_opt().unwrap_or(NaiveDate::MAX),
        ),
        FileTable::Scores => (start, end),
    }
}

fn file_overlaps(file: &FileMeta, min_ns: Option<i64>, max_ns: Option<i64>) -> bool {
    if let Some(min) = min_ns
        && file.max_time < min
    {
        return false;
    }
    if let Some(max) = max_ns
        && file.min_time > max
    {
        return false;
    }
    true
}

fn distribute_file_work(mut files: Vec<FileWork>, target: usize) -> Vec<Vec<FileWork>> {
    let slots = target.max(1).min(files.len().max(1));
    files.sort_by(|left, right| {
        right
            .file
            .total_bytes()
            .cmp(&left.file.total_bytes())
            .then_with(|| left.file.path.cmp(&right.file.path))
    });
    let mut groups: Vec<(u64, Vec<FileWork>)> = (0..slots).map(|_| (0, Vec::new())).collect();
    for work in files {
        let mut best = 0;
        for (idx, (sz, _)) in groups.iter().enumerate().skip(1) {
            if *sz < groups[best].0 {
                best = idx;
            }
        }
        groups[best].0 = groups[best].0.saturating_add(work.file.total_bytes());
        groups[best].1.push(work);
    }
    groups
        .into_iter()
        .filter(|(_, g)| !g.is_empty())
        .map(|(_, g)| g)
        .collect()
}

/// Precompiled table-level scan plan shared by all partitions of a scan.
struct CompiledScanSpec {
    projection: Option<Vec<usize>>,
    projected_schema: Arc<Schema>,
    output_cols: BTreeSet<usize>,
    /// Original filters (used if compaction `TakeRows` fallback runs without Statam pushdown).
    raw_filters: Vec<Expr>,
    /// Top-level `AND`-flattened conjuncts for per-file `exact_pushed` vs `residual` classification.
    flat_filters: Vec<Expr>,
    /// Output-only map projections (projected `MapUtf8` columns; absent means `Full`).
    output_map_projections: BTreeMap<String, MapProjection>,
    time_range: TimeRange,
    id_lookups: Vec<IdLookup>,
    /// Pushdown-safe CNF substring clauses from `spec.substrings` (every disjunct targets `Utf8`
    /// and every `case_insensitive` needle is ASCII).
    substrings: Vec<Vec<SubstringQuery>>,
    /// True when `spec.substrings` had any clause skipped (e.g. non-ASCII `ILIKE` or non-Utf8).
    has_unpushed_substrings: bool,
    /// True only when the query's sort hint matches the single-key descending
    /// time-column fast path on `Traces` / `Scores` (Plan B4).
    eligible_sort_desc: bool,
    has_sort_hints: bool,
    limit: Option<usize>,
    /// Stream-level row cap enforced only when `sort_hints` is empty (unordered limit).
    stream_limit: Option<usize>,
    /// True when opened via [`StatamSource::open_with_row_coords`].
    emit_row_coords: bool,
}

impl CompiledScanSpec {
    fn compile(
        table: FileTable,
        schema: &Arc<Schema>,
        projected_schema: Arc<Schema>,
        spec: &ScanSpec,
        emit_row_coords: bool,
    ) -> Result<Self, QueryError> {
        let mut output_cols = BTreeSet::new();
        match &spec.projection {
            Some(indices) => {
                for &idx in indices {
                    if idx >= schema.len() {
                        return Err(ColumnError::InvalidBatch {
                            reason: "projection index out of bounds",
                        }
                        .into());
                    }
                    output_cols.insert(idx);
                }
            }
            None => {
                for idx in 0..schema.len() {
                    output_cols.insert(idx);
                }
            }
        }

        let mut flat_filters = Vec::with_capacity(spec.filters.len());
        for filter in &spec.filters {
            flatten_top_and(filter, &mut flat_filters);
        }

        // Output-only map projections: any projected `MapUtf8` column absent from
        // `spec.map_projections` defaults to `Full`. Per-file residual + tombstone
        // map projections are merged later in `open_next_file`.
        let mut output_map_projections: BTreeMap<String, MapProjection> = BTreeMap::new();
        for &cidx in &output_cols {
            let field = &schema.fields()[cidx];
            if field.data_type == DataType::MapUtf8 {
                let proj = spec
                    .map_projections
                    .get(&field.name)
                    .cloned()
                    .unwrap_or(MapProjection::Full);
                output_map_projections.insert(field.name.clone(), proj);
            }
        }

        // Time range hint.
        let time_range = TimeRange {
            min_ns: spec.time_range.min_ns.unwrap_or(i64::MIN),
            max_ns: spec.time_range.max_ns.unwrap_or(i64::MAX),
        };

        // ID lookups (schema-validated; ExternalPayload columns filtered per-file).
        let mut id_lookups = Vec::with_capacity(spec.id_lookups.len());
        for hint in &spec.id_lookups {
            let (column, values): (&str, Vec<Vec<u8>>) = match hint {
                IdLookupHint::Fixed16 { column, values } => {
                    (column, values.iter().map(|v| v.to_vec()).collect())
                }
                IdLookupHint::Fixed8 { column, values } => {
                    (column, values.iter().map(|v| v.to_vec()).collect())
                }
                IdLookupHint::Utf8 { column, values } => (
                    column,
                    values.iter().map(|s| s.as_bytes().to_vec()).collect(),
                ),
            };
            if let Some(cidx) = schema.index_of(column)
                && is_supported_id_lookup_type(&schema.fields()[cidx].data_type)
            {
                id_lookups.push(IdLookup::in_list(column.to_owned(), values));
            }
        }

        // M3.5.1: Forward ALL CNF `spec.substrings` clauses whose disjuncts all target
        // `DataType::Utf8` columns AND whose `case_insensitive` needles are ASCII (non-ASCII
        // `ILIKE` stays in `residual` so `regex (?i)` simple case folding has zero false negatives).
        let mut substrings = Vec::new();
        let mut has_unpushed_substrings = false;
        for clause in &spec.substrings {
            if clause.is_empty() {
                substrings.push(Vec::new());
                continue;
            }
            let mut translated = Vec::with_capacity(clause.len());
            let mut clause_ok = true;
            for hint in clause {
                let valid_col = schema
                    .index_of(&hint.column)
                    .is_some_and(|cidx| schema.fields()[cidx].data_type == DataType::Utf8);
                let safe_case = !hint.case_insensitive || hint.needle.is_ascii();
                if !valid_col || !safe_case {
                    clause_ok = false;
                    break;
                }
                translated.push(SubstringQuery {
                    column: hint.column.clone(),
                    needle: hint.needle.as_bytes().to_vec(),
                    case_insensitive: hint.case_insensitive,
                });
            }
            if clause_ok {
                substrings.push(translated);
            } else {
                has_unpushed_substrings = true;
            }
        }

        // B4: `sort_desc_hint` eligibility (exact single-key descending time column on Traces/Scores).
        let eligible_sort_desc = matches!(table, FileTable::Traces | FileTable::Scores)
            && spec.sort_hints.len() == 1
            && spec.sort_hints[0].descending
            && spec.sort_hints[0].column == time_column(table);
        let has_sort_hints = !spec.sort_hints.is_empty();
        // Enforce a stream-level cap only when there are no sort hints (unordered LIMIT)
        // so ordered scans with residual filters/tombstones never drop qualifying top rows.
        let stream_limit = if !has_sort_hints { spec.limit } else { None };

        Ok(Self {
            projection: spec.projection.clone(),
            projected_schema,
            output_cols,
            raw_filters: spec.filters.clone(),
            flat_filters,
            output_map_projections,
            time_range,
            id_lookups,
            substrings,
            has_unpushed_substrings,
            eligible_sort_desc,
            has_sort_hints,
            limit: spec.limit,
            stream_limit,
            emit_row_coords,
        })
    }
}

fn flatten_top_and(expr: &Expr, out: &mut Vec<Expr>) {
    match expr {
        Expr::Binary {
            left,
            op: BinaryOp::And,
            right,
            ..
        } => {
            flatten_top_and(left, out);
            flatten_top_and(right, out);
        }
        other => out.push(other.clone()),
    }
}

fn flatten_or<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    match expr {
        Expr::Binary {
            left,
            op: BinaryOp::Or,
            right,
            ..
        } => {
            flatten_or(left, out);
            flatten_or(right, out);
        }
        other => out.push(other),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum LikeToken {
    Literal(String),
    AnySeq,
    AnyChar,
}

/// Escape-aware `LIKE` pattern tokenizer matching `crates/query/src/fn_string.rs:285-318`.
fn tokenize_like_pattern(pattern: &str, escape: Option<char>) -> Vec<LikeToken> {
    let mut tokens: Vec<LikeToken> = Vec::new();
    let mut chars = pattern.chars();
    let mut lit_buf = String::new();

    let flush_lit = |buf: &mut String, out: &mut Vec<LikeToken>| {
        if !buf.is_empty() {
            out.push(LikeToken::Literal(std::mem::take(buf)));
        }
    };

    while let Some(ch) = chars.next() {
        if escape == Some(ch) {
            if let Some(escaped) = chars.next() {
                lit_buf.push(escaped);
            } else {
                lit_buf.push(ch);
            }
        } else if ch == '%' {
            flush_lit(&mut lit_buf, &mut tokens);
            if !matches!(tokens.last(), Some(LikeToken::AnySeq)) {
                tokens.push(LikeToken::AnySeq);
            }
        } else if ch == '_' {
            flush_lit(&mut lit_buf, &mut tokens);
            tokens.push(LikeToken::AnyChar);
        } else {
            lit_buf.push(ch);
        }
    }
    flush_lit(&mut lit_buf, &mut tokens);
    tokens
}

fn is_supported_id_lookup_type(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::FixedBytes16 | DataType::FixedBytes8 | DataType::Utf8 | DataType::Binary
    )
}

fn scalar_to_zone_value(sv: &ScalarValue, col_dt: &DataType) -> Option<ZoneValue> {
    match (sv, col_dt) {
        (ScalarValue::Bool(b), DataType::Bool) => Some(ZoneValue::Bool(*b)),
        (ScalarValue::I64(v), DataType::I64) => Some(ZoneValue::I64(*v)),
        (ScalarValue::F64(f), DataType::F64) if !f.is_nan() => Some(ZoneValue::F64(*f)),
        (
            ScalarValue::Decimal128 {
                value,
                precision,
                scale,
            },
            DataType::Decimal128 {
                precision: cp,
                scale: cs,
            },
        ) if precision == cp && scale == cs => Some(ZoneValue::Decimal128(*value)),
        (ScalarValue::TimestampNs { value, .. }, DataType::TimestampNs { .. }) => {
            Some(ZoneValue::TimestampNs(*value))
        }
        (ScalarValue::Date(d), DataType::Date) => Some(ZoneValue::Date(*d)),
        (ScalarValue::Utf8(s), DataType::Utf8) => Some(ZoneValue::Bytes(s.as_bytes().to_vec())),
        (ScalarValue::Binary(b), DataType::Binary) => Some(ZoneValue::Bytes(b.clone())),
        (ScalarValue::FixedBytes8(b), DataType::FixedBytes8) => Some(ZoneValue::Bytes(b.to_vec())),
        (ScalarValue::FixedBytes16(b), DataType::FixedBytes16) => {
            Some(ZoneValue::Bytes(b.to_vec()))
        }
        _ => None,
    }
}

fn flip_cmp(op: BinaryOp) -> Option<BinaryOp> {
    match op {
        BinaryOp::Eq => Some(BinaryOp::Eq),
        BinaryOp::NotEq => Some(BinaryOp::NotEq),
        BinaryOp::Lt => Some(BinaryOp::Gt),
        BinaryOp::LtEq => Some(BinaryOp::GtEq),
        BinaryOp::Gt => Some(BinaryOp::Lt),
        BinaryOp::GtEq => Some(BinaryOp::LtEq),
        BinaryOp::IsDistinctFrom => Some(BinaryOp::IsDistinctFrom),
        BinaryOp::IsNotDistinctFrom => Some(BinaryOp::IsNotDistinctFrom),
        _ => None,
    }
}

fn cmp_to_zone_pred(op: BinaryOp, val: ZoneValue) -> Option<ZonePred> {
    match op {
        BinaryOp::Eq => Some(ZonePred::Eq(val)),
        BinaryOp::NotEq => Some(ZonePred::Ne(val)),
        BinaryOp::Lt => Some(ZonePred::Lt(val)),
        BinaryOp::LtEq => Some(ZonePred::Le(val)),
        BinaryOp::Gt => Some(ZonePred::Gt(val)),
        BinaryOp::GtEq => Some(ZonePred::Ge(val)),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum PredTarget<'a> {
    Col { name: &'a str, dt: &'a DataType },
    MapKey { name: &'a str, key: &'a str },
}

impl<'a> PredTarget<'a> {
    fn col_name(&self) -> &'a str {
        match self {
            Self::Col { name, .. } | Self::MapKey { name, .. } => name,
        }
    }

    fn is_utf8_or_map_key(&self) -> bool {
        match self {
            Self::Col { dt, .. } => **dt == DataType::Utf8,
            Self::MapKey { .. } => true,
        }
    }
}

fn extract_target<'a>(expr: &'a Expr, schema: &'a Schema) -> Option<PredTarget<'a>> {
    match expr {
        Expr::Column { index, .. } => {
            let field = schema.fields().get(*index)?;
            Some(PredTarget::Col {
                name: field.name.as_str(),
                dt: &field.data_type,
            })
        }
        Expr::MapIndex { map, key } => {
            let Expr::Column { index, .. } = map.as_ref() else {
                return None;
            };
            let Expr::Literal(ScalarValue::Utf8(k)) = key.as_ref() else {
                return None;
            };
            let field = schema.fields().get(*index)?;
            if field.data_type != DataType::MapUtf8 {
                return None;
            }
            Some(PredTarget::MapKey {
                name: field.name.as_str(),
                key: k.as_str(),
            })
        }
        _ => None,
    }
}

fn make_target_pred(target: PredTarget<'_>, pred: ZonePred) -> Predicate {
    match target {
        PredTarget::Col { name, .. } => Predicate::new(name.to_owned(), pred),
        PredTarget::MapKey { name, key } => {
            Predicate::map_key(name.to_owned(), key.to_owned(), pred)
        }
    }
}

fn target_value(target: &PredTarget<'_>, sv: &ScalarValue) -> Option<ZoneValue> {
    match target {
        PredTarget::Col { dt, .. } => scalar_to_zone_value(sv, dt),
        PredTarget::MapKey { .. } => scalar_to_zone_value(sv, &DataType::Utf8),
    }
}

/// Checks if a top-level filter conjunct is a pure OR-of-substrings (`LIKE`/`ILIKE '%needle%'`
/// or `contains(col, needle)`) with exact Statam parity (ASCII-only for `ILIKE`).
fn extract_exact_substring_clause(expr: &Expr, schema: &Schema) -> Option<Vec<SubstringQuery>> {
    let mut disjuncts = Vec::new();
    flatten_or(expr, &mut disjuncts);
    if disjuncts.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(disjuncts.len());
    for d in disjuncts {
        match d {
            Expr::Like {
                expr: col_expr,
                pattern,
                escape,
                case_insensitive,
                negated: false,
            } => {
                let Expr::Column { index, .. } = col_expr.as_ref() else {
                    return None;
                };
                let field = schema.fields().get(*index)?;
                if field.data_type != DataType::Utf8 {
                    return None;
                }
                let Expr::Literal(ScalarValue::Utf8(pat)) = pattern.as_ref() else {
                    return None;
                };
                let tokens = tokenize_like_pattern(pat, *escape);
                let [
                    LikeToken::AnySeq,
                    LikeToken::Literal(needle),
                    LikeToken::AnySeq,
                ] = tokens.as_slice()
                else {
                    return None;
                };
                if needle.is_empty() {
                    return None;
                }
                if *case_insensitive && !needle.is_ascii() {
                    return None;
                }
                out.push(SubstringQuery {
                    column: field.name.clone(),
                    needle: needle.as_bytes().to_vec(),
                    case_insensitive: *case_insensitive,
                });
            }
            Expr::Call {
                func: ScalarFn::Contains,
                args,
                ..
            } if args.len() == 2 => {
                let Expr::Column { index, .. } = &args[0] else {
                    return None;
                };
                let field = schema.fields().get(*index)?;
                if field.data_type != DataType::Utf8 {
                    return None;
                }
                let Expr::Literal(ScalarValue::Utf8(needle)) = &args[1] else {
                    return None;
                };
                if needle.is_empty() {
                    return None;
                }
                out.push(SubstringQuery {
                    column: field.name.clone(),
                    needle: needle.as_bytes().to_vec(),
                    case_insensitive: false,
                });
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Checks whether a leaf predicate on `target` is accepted and exact for this file's `col_dir`.
fn target_allows_pred_in_file(
    target: &PredTarget<'_>,
    pred: &ZonePred,
    file_schema: &Schema,
    meta: &StatamFileMeta,
) -> bool {
    let Some(cidx) = file_schema.index_of(target.col_name()) else {
        return false;
    };
    let Some(cd) = meta.col_dir.get(cidx) else {
        return false;
    };
    if cd.locality_group == LocalityGroup::ExternalPayload {
        return matches!(pred, ZonePred::IsNull | ZonePred::IsNotNull);
    }
    true
}

/// Translates `expr` into a 100%-exact SQL-3VL [`FilterExpr`] for this opened file.
///
/// Returns `None` for any expression touching a missing schema-evolution column, ANY value
/// predicate touching `LocalityGroup::ExternalPayload`, nullable-list `IN`/`NOT IN`,
/// nullable-bound `BETWEEN`/`NOT BETWEEN`, `NaN` F64 literals, non-ASCII `ILIKE`, casts,
/// arithmetic, cross-column comparisons, regex, or UDFs (keeping them in per-file `residual`).
fn translate_exact_filter_expr(
    expr: &Expr,
    schema: &Schema,
    file_schema: &Schema,
    meta: &StatamFileMeta,
) -> Option<FilterExpr> {
    match expr {
        Expr::Binary {
            left,
            op: BinaryOp::And,
            right,
            ..
        } => {
            let l = translate_exact_filter_expr(left, schema, file_schema, meta)?;
            let r = translate_exact_filter_expr(right, schema, file_schema, meta)?;
            let mut children = Vec::new();
            match l {
                FilterExpr::And(cs) => children.extend(cs),
                other => children.push(other),
            }
            match r {
                FilterExpr::And(cs) => children.extend(cs),
                other => children.push(other),
            }
            Some(FilterExpr::And(children))
        }
        Expr::Binary {
            left,
            op: BinaryOp::Or,
            right,
            ..
        } => {
            let l = translate_exact_filter_expr(left, schema, file_schema, meta)?;
            let r = translate_exact_filter_expr(right, schema, file_schema, meta)?;
            let mut children = Vec::new();
            match l {
                FilterExpr::Or(cs) => children.extend(cs),
                other => children.push(other),
            }
            match r {
                FilterExpr::Or(cs) => children.extend(cs),
                other => children.push(other),
            }
            Some(FilterExpr::Or(children))
        }
        Expr::Unary {
            op: UnaryOp::Not,
            expr: inner,
            ..
        } => {
            let child = translate_exact_filter_expr(inner, schema, file_schema, meta)?;
            Some(FilterExpr::Not(Box::new(child)))
        }
        Expr::Unary {
            op: UnaryOp::IsNull,
            expr: inner,
            ..
        } => {
            let target = extract_target(inner, schema)?;
            if !target_allows_pred_in_file(&target, &ZonePred::IsNull, file_schema, meta) {
                return None;
            }
            Some(FilterExpr::Pred(make_target_pred(target, ZonePred::IsNull)))
        }
        Expr::Unary {
            op: UnaryOp::IsNotNull,
            expr: inner,
            ..
        } => {
            let target = extract_target(inner, schema)?;
            if !target_allows_pred_in_file(&target, &ZonePred::IsNotNull, file_schema, meta) {
                return None;
            }
            Some(FilterExpr::Pred(make_target_pred(
                target,
                ZonePred::IsNotNull,
            )))
        }
        Expr::Binary {
            left, op, right, ..
        } => {
            let (target, cmp_op, sv) = if let (Some(t), Expr::Literal(sv)) =
                (extract_target(left, schema), right.as_ref())
            {
                (t, *op, sv)
            } else if let (Expr::Literal(sv), Some(t)) =
                (left.as_ref(), extract_target(right, schema))
            {
                let flipped = flip_cmp(*op)?;
                (t, flipped, sv)
            } else {
                return None;
            };

            match cmp_op {
                BinaryOp::IsDistinctFrom => {
                    if sv.is_null() {
                        if !target_allows_pred_in_file(
                            &target,
                            &ZonePred::IsNotNull,
                            file_schema,
                            meta,
                        ) {
                            return None;
                        }
                        return Some(FilterExpr::Pred(make_target_pred(
                            target,
                            ZonePred::IsNotNull,
                        )));
                    }
                    let val = target_value(&target, sv)?;
                    let ne_pred = ZonePred::Ne(val);
                    if !target_allows_pred_in_file(&target, &ne_pred, file_schema, meta) {
                        return None;
                    }
                    Some(FilterExpr::Or(vec![
                        FilterExpr::Pred(make_target_pred(target, ZonePred::IsNull)),
                        FilterExpr::Pred(make_target_pred(target, ne_pred)),
                    ]))
                }
                BinaryOp::IsNotDistinctFrom => {
                    if sv.is_null() {
                        if !target_allows_pred_in_file(
                            &target,
                            &ZonePred::IsNull,
                            file_schema,
                            meta,
                        ) {
                            return None;
                        }
                        return Some(FilterExpr::Pred(make_target_pred(target, ZonePred::IsNull)));
                    }
                    let val = target_value(&target, sv)?;
                    let eq_pred = ZonePred::Eq(val);
                    if !target_allows_pred_in_file(&target, &eq_pred, file_schema, meta) {
                        return None;
                    }
                    Some(FilterExpr::And(vec![
                        FilterExpr::Pred(make_target_pred(target, ZonePred::IsNotNull)),
                        FilterExpr::Pred(make_target_pred(target, eq_pred)),
                    ]))
                }
                _ => {
                    if sv.is_null() {
                        return None;
                    }
                    let val = target_value(&target, sv)?;
                    let zpred = cmp_to_zone_pred(cmp_op, val)?;
                    if !target_allows_pred_in_file(&target, &zpred, file_schema, meta) {
                        return None;
                    }
                    Some(FilterExpr::Pred(make_target_pred(target, zpred)))
                }
            }
        }
        Expr::Between {
            expr: inner,
            low,
            high,
            negated,
        } => {
            let target = extract_target(inner, schema)?;
            let (Expr::Literal(lo_sv), Expr::Literal(hi_sv)) = (low.as_ref(), high.as_ref()) else {
                return None;
            };
            if lo_sv.is_null() || hi_sv.is_null() {
                return None;
            }
            let lo = target_value(&target, lo_sv)?;
            let hi = target_value(&target, hi_sv)?;
            let zpred = ZonePred::Between(lo, hi);
            if !target_allows_pred_in_file(&target, &zpred, file_schema, meta) {
                return None;
            }
            let base = FilterExpr::Pred(make_target_pred(target, zpred));
            if *negated {
                Some(FilterExpr::Not(Box::new(base)))
            } else {
                Some(base)
            }
        }
        Expr::InList {
            expr: inner,
            list,
            negated,
            ..
        } => {
            let target = extract_target(inner, schema)?;
            let mut vals = Vec::with_capacity(list.len());
            for item in list {
                let Expr::Literal(sv) = item else {
                    return None;
                };
                if sv.is_null() {
                    return None;
                }
                vals.push(target_value(&target, sv)?);
            }
            let zpred = ZonePred::In(vals);
            if !target_allows_pred_in_file(&target, &zpred, file_schema, meta) {
                return None;
            }
            let base = FilterExpr::Pred(make_target_pred(target, zpred));
            if *negated {
                Some(FilterExpr::Not(Box::new(base)))
            } else {
                Some(base)
            }
        }
        Expr::Like {
            expr: inner,
            pattern,
            escape,
            case_insensitive: false,
            negated: false,
        } => {
            let target = extract_target(inner, schema)?;
            if !target.is_utf8_or_map_key() {
                return None;
            }
            let Expr::Literal(ScalarValue::Utf8(pat)) = pattern.as_ref() else {
                return None;
            };
            let tokens = tokenize_like_pattern(pat, *escape);
            let [LikeToken::Literal(prefix), LikeToken::AnySeq] = tokens.as_slice() else {
                return None;
            };
            let zpred = ZonePred::Prefix(prefix.as_bytes().to_vec());
            if !target_allows_pred_in_file(&target, &zpred, file_schema, meta) {
                return None;
            }
            Some(FilterExpr::Pred(make_target_pred(target, zpred)))
        }
        _ => None,
    }
}

/// Checks whether a top-level conjunct is trivially always-true or always-false on a file
/// because it solely tests a missing nullable schema-evolution column (all-NULL in this file).
enum MissingColOutcome {
    AlwaysTrue,
    AlwaysFalse,
    Unknown,
}

fn classify_missing_col_top_conjunct(
    expr: &Expr,
    schema: &Schema,
    file_schema: &Schema,
) -> MissingColOutcome {
    match expr {
        Expr::Unary {
            op: UnaryOp::IsNull,
            expr: inner,
            ..
        } => {
            if let Some(t) = extract_target(inner, schema)
                && file_schema.index_of(t.col_name()).is_none()
            {
                return MissingColOutcome::AlwaysTrue;
            }
            MissingColOutcome::Unknown
        }
        Expr::Unary {
            op: UnaryOp::IsNotNull,
            expr: inner,
            ..
        } => {
            if let Some(t) = extract_target(inner, schema)
                && file_schema.index_of(t.col_name()).is_none()
            {
                return MissingColOutcome::AlwaysFalse;
            }
            MissingColOutcome::Unknown
        }
        Expr::Binary {
            left,
            op:
                BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq,
            right,
            ..
        } => {
            let missing_target = match (
                extract_target(left, schema),
                right.as_ref(),
                left.as_ref(),
                extract_target(right, schema),
            ) {
                (Some(t), Expr::Literal(_), _, _) | (_, _, Expr::Literal(_), Some(t)) => {
                    file_schema.index_of(t.col_name()).is_none()
                }
                _ => false,
            };
            if missing_target {
                MissingColOutcome::AlwaysFalse
            } else {
                MissingColOutcome::Unknown
            }
        }
        Expr::Between {
            expr: inner,
            negated: false,
            ..
        }
        | Expr::InList {
            expr: inner,
            negated: false,
            ..
        }
        | Expr::Like {
            expr: inner,
            negated: false,
            ..
        } => {
            if let Some(t) = extract_target(inner, schema)
                && file_schema.index_of(t.col_name()).is_none()
            {
                return MissingColOutcome::AlwaysFalse;
            }
            MissingColOutcome::Unknown
        }
        _ => MissingColOutcome::Unknown,
    }
}

struct ClassifiedFileFilters {
    id_lookups: Vec<IdLookup>,
    predicates: Vec<Predicate>,
    filter_exprs: Vec<FilterExpr>,
    substrings: Vec<Vec<SubstringQuery>>,
    residual_full: Vec<Expr>,
}

/// Classifies all pushdowns and `plan.flat_filters` for one opened file using `meta.col_dir`.
/// Returns `Ok(None)` when the file is provably empty (0 matching rows).
fn classify_file_filters(
    plan: &CompiledScanSpec,
    schema: &Schema,
    meta: &StatamFileMeta,
) -> Option<ClassifiedFileFilters> {
    let file_schema = &meta.schema_meta.schema;

    let mut id_lookups = Vec::with_capacity(plan.id_lookups.len());
    for lk in &plan.id_lookups {
        let cidx = file_schema.index_of(&lk.column)?;
        if meta
            .col_dir
            .get(cidx)
            .is_some_and(|cd| cd.locality_group != LocalityGroup::ExternalPayload)
        {
            id_lookups.push(lk.clone());
        }
    }

    let mut substrings: Vec<Vec<SubstringQuery>> = Vec::with_capacity(plan.substrings.len());
    for clause in &plan.substrings {
        if clause.is_empty() {
            return None;
        }
        let filtered: Vec<SubstringQuery> = clause
            .iter()
            .filter(|q| file_schema.index_of(&q.column).is_some())
            .cloned()
            .collect();
        if filtered.is_empty() {
            return None;
        }
        if !substrings.contains(&filtered) {
            substrings.push(filtered);
        }
    }

    let mut predicates = Vec::new();
    let mut filter_exprs = Vec::new();
    let mut residual_full = Vec::new();

    for conjunct in &plan.flat_filters {
        if let Some(clause) = extract_exact_substring_clause(conjunct, schema) {
            let filtered: Vec<SubstringQuery> = clause
                .into_iter()
                .filter(|q| file_schema.index_of(&q.column).is_some())
                .collect();
            if filtered.is_empty() {
                return None;
            }
            if !substrings.contains(&filtered) {
                substrings.push(filtered);
            }
            continue;
        }

        if let Some(fe) = translate_exact_filter_expr(conjunct, schema, file_schema, meta) {
            match fe {
                FilterExpr::Pred(p) => predicates.push(p),
                other => filter_exprs.push(other),
            }
            continue;
        }

        match classify_missing_col_top_conjunct(conjunct, schema, file_schema) {
            MissingColOutcome::AlwaysTrue => continue,
            MissingColOutcome::AlwaysFalse => return None,
            MissingColOutcome::Unknown => residual_full.push(conjunct.clone()),
        }
    }

    Some(ClassifiedFileFilters {
        id_lookups,
        predicates,
        filter_exprs,
        substrings,
        residual_full,
    })
}

/// Returns the `BlockEntry` slice belonging to Row Group `rg_idx` for physical column `pidx`.
fn rg_blocks_in_meta(meta: &StatamFileMeta, pidx: usize, rg_idx: usize) -> &[BlockEntry] {
    let cd = &meta.col_dir[pidx];
    let rg = &meta.rg_dir[rg_idx];
    let col_blocks =
        &meta.block_dir[cd.first_block_idx as usize..(cd.first_block_idx + cd.num_blocks) as usize];
    let rg_start = rg.first_row;
    let rg_end = rg.first_row + rg.row_count;
    let lo = col_blocks.partition_point(|b| b.first_row < rg_start);
    let hi = col_blocks.partition_point(|b| b.first_row < rg_end);
    &col_blocks[lo..hi]
}

fn id_lookup_zone_pred_in_meta(dt: &DataType, values: &[Vec<u8>]) -> Option<ZonePred> {
    let to_zval = |b: &[u8]| -> Option<ZoneValue> {
        match dt {
            DataType::Utf8 | DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16 => {
                Some(ZoneValue::Bytes(b.to_vec()))
            }
            _ => None,
        }
    };
    if values.len() == 1 {
        to_zval(&values[0]).map(ZonePred::Eq)
    } else {
        let mut zvals = Vec::with_capacity(values.len());
        for v in values {
            zvals.push(to_zval(v)?);
        }
        Some(ZonePred::In(zvals))
    }
}

fn invert_zone_pred_for_3vl(pred: &ZonePred) -> Option<ZonePred> {
    match pred {
        ZonePred::Eq(v) => Some(ZonePred::Ne(v.clone())),
        ZonePred::Ne(v) => Some(ZonePred::Eq(v.clone())),
        ZonePred::Lt(v) => Some(ZonePred::Ge(v.clone())),
        ZonePred::Le(v) => Some(ZonePred::Gt(v.clone())),
        ZonePred::Gt(v) => Some(ZonePred::Le(v.clone())),
        ZonePred::Ge(v) => Some(ZonePred::Lt(v.clone())),
        ZonePred::IsNull => Some(ZonePred::IsNotNull),
        ZonePred::IsNotNull => Some(ZonePred::IsNull),
        ZonePred::Between(_, _) | ZonePred::In(_) | ZonePred::Prefix(_) => None,
    }
}

fn filter_expr_may_be_true_in_meta(
    meta: &StatamFileMeta,
    rg_idx: usize,
    mb_idx: usize,
    expr: &FilterExpr,
    dicts: &HashMap<usize, FileDict>,
) -> bool {
    let schema = &meta.schema_meta.schema;
    match expr {
        FilterExpr::Pred(pred) => {
            if pred.map_key.is_some() {
                return true;
            }
            let Some(cidx) = schema.index_of(&pred.column) else {
                return true;
            };
            let blk = &rg_blocks_in_meta(meta, cidx, rg_idx)[mb_idx];
            let dt = &schema.fields()[cidx].data_type;
            zone_map_may_match(
                blk,
                dt,
                meta.col_dir[cidx].col_flags,
                &pred.pred,
                dicts.get(&cidx),
            )
        }
        FilterExpr::And(cs) => cs
            .iter()
            .all(|c| filter_expr_may_be_true_in_meta(meta, rg_idx, mb_idx, c, dicts)),
        FilterExpr::Or(cs) => cs
            .iter()
            .any(|c| filter_expr_may_be_true_in_meta(meta, rg_idx, mb_idx, c, dicts)),
        FilterExpr::Not(inner) => {
            filter_expr_may_be_false_in_meta(meta, rg_idx, mb_idx, inner, dicts)
        }
    }
}

fn filter_expr_may_be_false_in_meta(
    meta: &StatamFileMeta,
    rg_idx: usize,
    mb_idx: usize,
    expr: &FilterExpr,
    dicts: &HashMap<usize, FileDict>,
) -> bool {
    let schema = &meta.schema_meta.schema;
    match expr {
        FilterExpr::Pred(pred) => {
            if pred.map_key.is_some() {
                return true;
            }
            let Some(inv) = invert_zone_pred_for_3vl(&pred.pred) else {
                return true;
            };
            let Some(cidx) = schema.index_of(&pred.column) else {
                return true;
            };
            let blk = &rg_blocks_in_meta(meta, cidx, rg_idx)[mb_idx];
            let dt = &schema.fields()[cidx].data_type;
            if matches!(dt, DataType::F64) && (blk.block_flags & BLOCK_FLAG_HAS_NAN) != 0 {
                return true;
            }
            zone_map_may_match(
                blk,
                dt,
                meta.col_dir[cidx].col_flags,
                &inv,
                dicts.get(&cidx),
            )
        }
        FilterExpr::And(cs) => cs
            .iter()
            .any(|c| filter_expr_may_be_false_in_meta(meta, rg_idx, mb_idx, c, dicts)),
        FilterExpr::Or(cs) => cs
            .iter()
            .all(|c| filter_expr_may_be_false_in_meta(meta, rg_idx, mb_idx, c, dicts)),
        FilterExpr::Not(inner) => {
            filter_expr_may_be_true_in_meta(meta, rg_idx, mb_idx, inner, dicts)
        }
    }
}

/// Pre-prunes Row Groups using **strictly in-memory** `StatamFileMeta` (`time_range`, Hot blooms,
/// Hot dicts, and RG/MB zone maps — NEVER performs any I/O or fetches Lazy blooms/dicts).
fn pre_prune_rgs_in_memory(
    meta: &StatamFileMeta,
    table: FileTable,
    schema: &Schema,
    plan: &CompiledScanSpec,
) -> Result<Vec<usize>, StatamError> {
    let num_rgs = meta.rg_dir.len();
    if num_rgs == 0 || meta.postscript.row_count == 0 || plan.time_range.is_empty() {
        return Ok(Vec::new());
    }
    let file_schema = &meta.schema_meta.schema;
    let Some(classified) = classify_file_filters(plan, schema, meta) else {
        return Ok(Vec::new());
    };

    // Hot blooms ONLY (in-memory Hot Tail slice; returns Ok(None) for Lazy blooms -> 0 I/O).
    let mut hot_blooms: Vec<(usize, BloomIndex, Vec<u64>)> = Vec::new();
    let mut lookup_zpreds: Vec<(usize, ZonePred)> = Vec::new();
    for lk in &classified.id_lookups {
        if lk.values.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(cidx) = file_schema.index_of(&lk.column) {
            let hashes: Vec<u64> = lk.values.iter().map(|v| statam_bloom_hash(v)).collect();
            if let Some(bloom) = meta.hot_bloom(cidx)? {
                if !bloom.contains_any_hash(&hashes) {
                    return Ok(Vec::new());
                }
                hot_blooms.push((cidx, bloom, hashes));
            }
            if let Some(zp) =
                id_lookup_zone_pred_in_meta(&file_schema.fields()[cidx].data_type, &lk.values)
            {
                lookup_zpreds.push((cidx, zp));
            }
        }
    }

    // Hot dicts ONLY for columns referenced by zone-map predicates (in-memory Hot Tail slice).
    let mut hot_dicts: HashMap<usize, FileDict> = HashMap::new();
    let mut try_load_hot_dict = |cidx: usize| -> Result<(), StatamError> {
        if let std::collections::hash_map::Entry::Vacant(e) = hot_dicts.entry(cidx)
            && let Some(dict) = meta.hot_dict(cidx)?
        {
            e.insert(dict);
        }
        Ok(())
    };
    for &(cidx, _) in &lookup_zpreds {
        try_load_hot_dict(cidx)?;
    }
    for p in &classified.predicates {
        if p.map_key.is_none()
            && let Some(cidx) = file_schema.index_of(&p.column)
        {
            try_load_hot_dict(cidx)?;
        }
    }
    fn collect_expr_cols(expr: &FilterExpr, file_schema: &Schema, out: &mut BTreeSet<usize>) {
        match expr {
            FilterExpr::Pred(p) => {
                if p.map_key.is_none()
                    && let Some(cidx) = file_schema.index_of(&p.column)
                {
                    out.insert(cidx);
                }
            }
            FilterExpr::And(cs) | FilterExpr::Or(cs) => {
                for c in cs {
                    collect_expr_cols(c, file_schema, out);
                }
            }
            FilterExpr::Not(inner) => collect_expr_cols(inner, file_schema, out),
        }
    }
    let mut expr_cols = BTreeSet::new();
    for fe in &classified.filter_exprs {
        collect_expr_cols(fe, file_schema, &mut expr_cols);
    }
    for cidx in expr_cols {
        try_load_hot_dict(cidx)?;
    }

    let time_col_idx = if plan.time_range.is_all() {
        None
    } else {
        file_schema.index_of(time_column(table))
    };
    let time_zpred = ZonePred::Between(
        ZoneValue::TimestampNs(plan.time_range.min_ns),
        ZoneValue::TimestampNs(plan.time_range.max_ns),
    );

    let mut surviving_rgs = Vec::with_capacity(num_rgs);
    let fields = file_schema.fields();

    'rg: for rg_idx in 0..num_rgs {
        for (_, bloom, hashes) in &hot_blooms {
            if !bloom.rg_may_contain_any_hash(rg_idx, hashes) {
                continue 'rg;
            }
        }
        let mb_count = rg_blocks_in_meta(meta, 0, rg_idx).len();
        if mb_count == 0 {
            continue;
        }
        let mut any_mb_survives = false;
        'mb: for mb_idx in 0..mb_count {
            if let Some(tc) = time_col_idx {
                let blk = &rg_blocks_in_meta(meta, tc, rg_idx)[mb_idx];
                if !zone_map_may_match(
                    blk,
                    &fields[tc].data_type,
                    meta.col_dir[tc].col_flags,
                    &time_zpred,
                    hot_dicts.get(&tc),
                ) {
                    continue 'mb;
                }
            }
            for (cidx, zp) in &lookup_zpreds {
                let blk = &rg_blocks_in_meta(meta, *cidx, rg_idx)[mb_idx];
                if !zone_map_may_match(
                    blk,
                    &fields[*cidx].data_type,
                    meta.col_dir[*cidx].col_flags,
                    zp,
                    hot_dicts.get(cidx),
                ) {
                    continue 'mb;
                }
            }
            for p in &classified.predicates {
                if p.map_key.is_some() {
                    continue;
                }
                if let Some(cidx) = file_schema.index_of(&p.column) {
                    let blk = &rg_blocks_in_meta(meta, cidx, rg_idx)[mb_idx];
                    if !zone_map_may_match(
                        blk,
                        &fields[cidx].data_type,
                        meta.col_dir[cidx].col_flags,
                        &p.pred,
                        hot_dicts.get(&cidx),
                    ) {
                        continue 'mb;
                    }
                }
            }
            for fe in &classified.filter_exprs {
                if !filter_expr_may_be_true_in_meta(meta, rg_idx, mb_idx, fe, &hot_dicts) {
                    continue 'mb;
                }
            }
            any_mb_survives = true;
            break;
        }
        if any_mb_survives {
            surviving_rgs.push(rg_idx);
        }
    }

    Ok(surviving_rgs)
}

/// Returns `true` iff `file` can be counted directly from `FileMeta::rows` with **0 disk / 0 S3 I/O**.
fn can_zero_io_count(
    file: &FileMeta,
    rg_indices: Option<&[usize]>,
    deletions: &[Arc<PendingDeletion>],
    table: FileTable,
    schema: &Schema,
    plan: &CompiledScanSpec,
) -> bool {
    if plan.emit_row_coords || rg_indices.is_some() {
        return false;
    }
    if plan.projection.as_deref() != Some(&[]) {
        return false;
    }
    if !plan.id_lookups.is_empty() || !plan.substrings.is_empty() || plan.has_unpushed_substrings {
        return false;
    }
    if !applicable(
        deletions,
        Some(file.deletions_applied_seq),
        file.min_ingest_time,
    )
    .is_empty()
    {
        return false;
    }
    if file.min_time > file.max_time
        || file.min_time < plan.time_range.min_ns
        || file.max_time > plan.time_range.max_ns
    {
        return false;
    }
    if plan.flat_filters.is_empty() {
        return true;
    }
    let Some(time_col_idx) = schema.index_of(time_column(table)) else {
        return false;
    };
    let field = &schema.fields()[time_col_idx];
    if field.nullable || !matches!(field.data_type, DataType::TimestampNs { .. }) {
        return false;
    }
    plan.flat_filters.iter().all(|expr| {
        time_expr_always_true_on_interval(expr, time_col_idx, file.min_time, file.max_time)
    })
}

fn is_col_index(expr: &Expr, col_idx: usize) -> bool {
    matches!(expr, Expr::Column { index, .. } if *index == col_idx)
}

/// Returns `true` ONLY when `expr` is a pure predicate on non-nullable timestamp column
/// `time_col_idx` that evaluates to `TRUE` for EVERY `t` in `[min_t, max_t]`.
fn time_expr_always_true_on_interval(
    expr: &Expr,
    time_col_idx: usize,
    min_t: i64,
    max_t: i64,
) -> bool {
    match expr {
        Expr::Binary {
            left,
            op: BinaryOp::And,
            right,
            ..
        } => {
            time_expr_always_true_on_interval(left, time_col_idx, min_t, max_t)
                && time_expr_always_true_on_interval(right, time_col_idx, min_t, max_t)
        }
        Expr::Unary {
            op: UnaryOp::IsNotNull,
            expr: inner,
            ..
        } => is_col_index(inner, time_col_idx),
        Expr::Binary {
            left, op, right, ..
        } => {
            let (cmp_op, lit) = if is_col_index(left, time_col_idx)
                && let Expr::Literal(ScalarValue::TimestampNs { value, .. }) = right.as_ref()
            {
                (*op, *value)
            } else if is_col_index(right, time_col_idx)
                && let Expr::Literal(ScalarValue::TimestampNs { value, .. }) = left.as_ref()
            {
                let Some(flipped) = flip_cmp(*op) else {
                    return false;
                };
                (flipped, *value)
            } else {
                return false;
            };
            match cmp_op {
                BinaryOp::Eq | BinaryOp::IsNotDistinctFrom => min_t == lit && max_t == lit,
                BinaryOp::NotEq | BinaryOp::IsDistinctFrom => max_t < lit || min_t > lit,
                BinaryOp::Lt => max_t < lit,
                BinaryOp::LtEq => max_t <= lit,
                BinaryOp::Gt => min_t > lit,
                BinaryOp::GtEq => min_t >= lit,
                _ => false,
            }
        }
        Expr::Between {
            expr: inner,
            low,
            high,
            negated: false,
        } => {
            if !is_col_index(inner, time_col_idx) {
                return false;
            }
            let (
                Expr::Literal(ScalarValue::TimestampNs { value: lo, .. }),
                Expr::Literal(ScalarValue::TimestampNs { value: hi, .. }),
            ) = (low.as_ref(), high.as_ref())
            else {
                return false;
            };
            lo <= hi && min_t >= *lo && max_t <= *hi
        }
        _ => false,
    }
}

/// Resolves the single sibling `.stpk` pack owned 1:1 by an L1 `spans` file,
/// caching its decoded [`PackToc`] in [`MetadataCache`].
struct SingleFilePackResolver {
    pack: PackMeta,
    io: StorageIo,
    metadata_cache: Arc<MetadataCache>,
    runtime: tokio::runtime::Handle,
    cancel: CancelToken,
}

impl PackResolver for SingleFilePackResolver {
    fn resolve(&self, pack_id: [u8; 16]) -> Result<Arc<ChunkPackReader>, StatamError> {
        if pack_id != self.pack.id {
            return Err(StatamError::MissingPack { pack_id });
        }
        let rr = open_range_reader(
            &self.io,
            &self.pack.path,
            self.pack.size,
            self.runtime.clone(),
            self.cancel.clone(),
        )?;
        let toc = if let Some(cached) = self.metadata_cache.get_pack(pack_id) {
            cached
        } else {
            let (toc, _retained) =
                PackToc::read_from_with_hint(rr.as_ref(), Some(self.pack.toc_size))?;
            self.metadata_cache.insert_pack(pack_id, Arc::clone(&toc));
            toc
        };
        let reader = ChunkPackReader::open_with_toc(rr, toc)?;
        Ok(Arc::new(reader))
    }
}

enum FileScanCursor {
    Iter(Box<StatamScanIter>),
    IterWithCoords {
        iter: Box<StatamScanIter>,
        file_ordinal: u32,
    },
    ZeroIoCount {
        schema: Arc<Schema>,
        remaining_rows: u64,
        chunk_size: usize,
    },
    TakeRows {
        reader: Arc<StatamReader>,
        projection: Option<Vec<usize>>,
        ranges: VecDeque<(u64, u64)>,
        buffered: VecDeque<Batch>,
        batch_size: usize,
    },
}

type CursorNextBatch = (Batch, Option<(u32, Vec<u64>)>);

impl FileScanCursor {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<CursorNextBatch>, StatamError> {
        match self {
            Self::Iter(iter) => {
                Ok(guard_sync(|| iter.next_batch(Some(cancel), res))?.map(|b| (b, None)))
            }
            Self::IterWithCoords { iter, file_ordinal } => {
                let ord = *file_ordinal;
                Ok(
                    guard_sync(|| iter.next_batch_with_row_ids(Some(cancel), res))?
                        .map(|(b, rids)| (b, Some((ord, rids)))),
                )
            }
            Self::ZeroIoCount {
                schema,
                remaining_rows,
                chunk_size,
            } => {
                if cancel.is_cancelled() {
                    return Err(StatamError::Cancelled);
                }
                if *remaining_rows == 0 {
                    return Ok(None);
                }
                let take_u64 = (*remaining_rows).min(*chunk_size as u64);
                let take = usize::try_from(take_u64).unwrap_or(*chunk_size);
                *remaining_rows -= take as u64;
                let batch = Batch::try_new(Arc::clone(schema), Vec::new(), take, Selection::All)?;
                Ok(Some((batch, None)))
            }
            Self::TakeRows {
                reader,
                projection,
                ranges,
                buffered,
                batch_size,
            } => {
                let res_before = res.as_ref().map(|r| r.size());
                loop {
                    if cancel.is_cancelled() {
                        return Err(StatamError::Cancelled);
                    }
                    if let Some(batch) = buffered.pop_front() {
                        if batch.is_empty() {
                            continue;
                        }
                        let mem = batch.memory_bytes();
                        if let Some(r) = res.as_deref_mut()
                            && r.try_grow(mem).is_err()
                        {
                            if let Some(before) = res_before
                                && r.size() > before
                            {
                                r.shrink(r.size() - before);
                            }
                            return Err(StatamError::MemoryExceeded);
                        }
                        return Ok(Some((batch, None)));
                    }
                    let mut row_ids: Vec<u64> = Vec::with_capacity(*batch_size);
                    while row_ids.len() < *batch_size {
                        let Some((lo, hi)) = ranges.front_mut() else {
                            break;
                        };
                        let need = (*batch_size - row_ids.len()) as u64;
                        let avail = hi.saturating_sub(*lo);
                        let take = need.min(avail);
                        for r in *lo..(*lo + take) {
                            row_ids.push(r);
                        }
                        *lo += take;
                        if *lo >= *hi {
                            ranges.pop_front();
                        }
                    }
                    if row_ids.is_empty() {
                        return Ok(None);
                    }
                    let batches = guard_sync(|| {
                        reader.take_rows_with_limits(
                            &row_ids,
                            projection.as_deref(),
                            1,
                            Some(cancel),
                            None,
                        )
                    })?;
                    for b in batches {
                        if !b.is_empty() {
                            buffered.push_back(b);
                        }
                    }
                }
            }
        }
    }
}

enum ScanColSlot {
    FromFile(usize),
    FillNull(DataType),
}

struct ScanBatchFill {
    schema: Arc<Schema>,
    slots: Vec<ScanColSlot>,
}

struct ActiveFileScan {
    file_id: FileId,
    cursor: FileScanCursor,
    fill_layout: Option<ScanBatchFill>,
    coord_scan_schema: Option<Arc<Schema>>,
    tombstone_pred: Option<Expr>,
    remapped_filters: Vec<Expr>,
    /// Maps each output column slot to its position in the scanned batch (`None` if identity).
    scan_to_output: Option<Vec<usize>>,
}

struct StatamPartitionStream {
    io: StorageIo,
    table: FileTable,
    schema: Arc<Schema>,
    files: VecDeque<FileWork>,
    deletions: Arc<[Arc<PendingDeletion>]>,
    metadata_cache: Arc<MetadataCache>,
    suspects: Suspects,
    runtime: tokio::runtime::Handle,
    plan: Arc<CompiledScanSpec>,
    remaining: Option<usize>,
    eval: EvalContext,
    batch_size: usize,
    chunk_to_batch_size: bool,
    read_threads: usize,
    executor: Option<Arc<dyn TaskExecutor>>,
    active_file: Option<ActiveFileScan>,
}

impl StatamPartitionStream {
    fn open_next_file(
        &mut self,
        cancel: &CancelToken,
    ) -> Result<Option<ActiveFileScan>, QueryError> {
        'next_file: while let Some(work) = self.files.pop_front() {
            if cancel.is_cancelled() {
                return Err(QueryError::Cancelled);
            }
            let FileWork {
                file_ordinal,
                file,
                rg_indices,
            } = work;

            // M3.5.4: Zero-I/O COUNT(*) fast path — emits 0-column batches directly from
            // `FileMeta::rows` before touching `open_range_reader` or `MetadataCache`.
            if can_zero_io_count(
                &file,
                rg_indices.as_deref(),
                &self.deletions,
                self.table,
                &self.schema,
                &self.plan,
            ) {
                let remaining_rows = match self.remaining {
                    Some(rem) => file.rows.min(rem as u64),
                    None => file.rows,
                };
                if remaining_rows == 0 {
                    continue 'next_file;
                }
                let chunk_size = self.batch_size.clamp(1, ZERO_IO_COUNT_MAX_CHUNK);
                return Ok(Some(ActiveFileScan {
                    file_id: file.id,
                    cursor: FileScanCursor::ZeroIoCount {
                        schema: Arc::clone(&self.plan.projected_schema),
                        remaining_rows,
                        chunk_size,
                    },
                    fill_layout: None,
                    coord_scan_schema: None,
                    tombstone_pred: None,
                    remapped_filters: Vec::new(),
                    scan_to_output: None,
                }));
            }

            let rr = open_range_reader(
                &self.io,
                &file.path,
                file.size,
                self.runtime.clone(),
                cancel.clone(),
            )
            .map_err(|e| map_statam_err(&self.suspects, file.id, e))?;

            let meta = if let Some(cached) = self.metadata_cache.get_statam(file.id) {
                cached
            } else {
                let loaded = guard_sync(|| {
                    StatamFileMeta::read_from_with_hint(rr.as_ref(), Some(file.footer_size))
                })
                .map_err(|e| map_statam_err(&self.suspects, file.id, e))?;
                let meta = Arc::new(loaded);
                self.metadata_cache
                    .insert_statam(file.id, Arc::clone(&meta));
                meta
            };

            // Resolve every target schema column against the file's schema by name
            // (spec 03 §3 schema evolution: missing nullable columns become `Const(Null)`;
            // missing non-nullable columns or type mismatches are corrupt).
            let file_schema = &meta.schema_meta.schema;
            let mut full_to_file: Vec<Option<usize>> = Vec::with_capacity(self.schema.len());
            for field in self.schema.fields() {
                match file_schema.index_of(&field.name) {
                    Some(file_idx) => {
                        if file_schema.fields()[file_idx].data_type != field.data_type {
                            self.suspects.report_corrupt(file.id);
                            return Err(QueryError::CorruptData(
                                "column type mismatch in statam file".to_owned(),
                            ));
                        }
                        full_to_file.push(Some(file_idx));
                    }
                    None => {
                        if !field.nullable {
                            self.suspects.report_corrupt(file.id);
                            return Err(QueryError::CorruptData(
                                "missing non-nullable column in statam file".to_owned(),
                            ));
                        }
                        full_to_file.push(None);
                    }
                }
            }

            // Per-file exact-pushed vs residual filter classification using `meta.col_dir`.
            let Some(mut classified) = classify_file_filters(&self.plan, &self.schema, &meta)
            else {
                continue 'next_file;
            };

            let file_deletions = applicable(
                &self.deletions,
                Some(file.deletions_applied_seq),
                file.min_ingest_time,
            );
            let tombstone_full = if file_deletions.is_empty() {
                None
            } else {
                let specs = to_tombstone_specs(&file_deletions);
                covered_predicate(tombstone_table(self.table), &self.schema, &specs)?
            };

            // Determine whether compaction's bounded-RAM `TakeRows` windowing applies.
            // Because `TakeRows` does not evaluate Statam filters, keep ALL `raw_filters` in
            // `residual_full` whenever `use_take_rows` is chosen.
            let max_mb_rows = meta
                .block_dir
                .iter()
                .map(|b| usize::from(b.row_count))
                .max()
                .unwrap_or(0);
            let use_take_rows = self.chunk_to_batch_size
                && !self.plan.emit_row_coords
                && rg_indices.is_none()
                && max_mb_rows > self.batch_size
                && classified.id_lookups.is_empty()
                && classified.substrings.is_empty()
                && !self.plan.eligible_sort_desc;

            if use_take_rows {
                classified.predicates.clear();
                classified.filter_exprs.clear();
                classified.residual_full = self.plan.raw_filters.clone();
            }

            let mut scan_set = self.plan.output_cols.clone();
            for residual in &classified.residual_full {
                residual.referenced_columns(&mut scan_set);
            }
            if let Some(ref pred) = tombstone_full {
                pred.referenced_columns(&mut scan_set);
            }

            let scan_indices: Vec<usize> = scan_set.into_iter().collect();
            let mut file_proj = Vec::with_capacity(scan_indices.len());
            let mut slots = Vec::with_capacity(scan_indices.len());
            let mut has_missing_col = false;
            for &full_idx in &scan_indices {
                match full_to_file[full_idx] {
                    Some(file_idx) => {
                        let file_pos = file_proj.len();
                        file_proj.push(file_idx);
                        slots.push(ScanColSlot::FromFile(file_pos));
                    }
                    None => {
                        has_missing_col = true;
                        slots.push(ScanColSlot::FillNull(
                            self.schema.fields()[full_idx].data_type.clone(),
                        ));
                    }
                }
            }

            let is_all_in_order = file_proj.len() == file_schema.len()
                && file_proj.iter().enumerate().all(|(i, &c)| i == c);
            let req_projection = if is_all_in_order {
                None
            } else {
                Some(file_proj)
            };
            let scan_schema = Arc::new(self.schema.project(&scan_indices)?);
            let fill_layout = if has_missing_col {
                Some(ScanBatchFill {
                    schema: Arc::clone(&scan_schema),
                    slots,
                })
            } else {
                None
            };
            let coord_scan_schema = if self.plan.emit_row_coords {
                let mut fields = scan_schema.fields().to_vec();
                fields.push(Field::new(ROW_COORD_FILE_COL, DataType::I64, false));
                fields.push(Field::new(ROW_COORD_ROW_COL, DataType::I64, false));
                Some(Schema::try_new_arc(fields)?)
            } else {
                None
            };

            let mut full_to_scan: Vec<Option<usize>> = vec![None; self.schema.len()];
            for (scan_pos, &full_idx) in scan_indices.iter().enumerate() {
                full_to_scan[full_idx] = Some(scan_pos);
            }

            let tombstone_pred = match tombstone_full {
                Some(ref expr) => Some(expr.remap_columns(&full_to_scan)?),
                None => None,
            };
            let remapped_filters: Vec<Expr> = classified
                .residual_full
                .iter()
                .map(|f| f.remap_columns(&full_to_scan))
                .collect::<Result<Vec<_>, _>>()?;

            let scan_to_output = match &self.plan.projection {
                Some(out_indices) => {
                    let mut map = Vec::with_capacity(
                        out_indices.len() + if self.plan.emit_row_coords { 2 } else { 0 },
                    );
                    for &full_idx in out_indices {
                        let pos = full_to_scan[full_idx]
                            .ok_or_else(|| QueryError::internal("projected column missing"))?;
                        map.push(pos);
                    }
                    if self.plan.emit_row_coords {
                        map.push(scan_indices.len());
                        map.push(scan_indices.len() + 1);
                    }
                    let expected_scan_len =
                        scan_indices.len() + if self.plan.emit_row_coords { 2 } else { 0 };
                    let identity = map.len() == expected_scan_len
                        && map.iter().enumerate().all(|(i, &p)| i == p);
                    if identity { None } else { Some(map) }
                }
                None => None,
            };

            // M3.5.4: Merge output + per-file residual + tombstone `MapProjection` lattice into
            // native `MapReadMode::{Full, KeysOnly, Keys}`.
            let mut merged_map_proj = self.plan.output_map_projections.clone();
            for residual in &classified.residual_full {
                collect_expr_map_projections(residual, &self.schema, &mut merged_map_proj);
            }
            if let Some(ref pred) = tombstone_full {
                collect_expr_map_projections(pred, &self.schema, &mut merged_map_proj);
            }
            let mut map_projections: HashMap<String, MapReadMode> = HashMap::new();
            for (col_name, proj) in &merged_map_proj {
                let is_valid_map = file_schema.index_of(col_name).is_some_and(|cidx| {
                    meta.col_dir.get(cidx).is_some()
                        && file_schema
                            .fields()
                            .get(cidx)
                            .is_some_and(|f| f.data_type == DataType::MapUtf8)
                });
                if !is_valid_map {
                    continue;
                }
                match proj {
                    MapProjection::Full => {
                        map_projections.insert(col_name.clone(), MapReadMode::Full);
                    }
                    MapProjection::KeysOnly => {
                        map_projections.insert(col_name.clone(), MapReadMode::KeysOnly);
                    }
                    MapProjection::Keys(keys) if !keys.is_empty() => {
                        map_projections.insert(
                            col_name.clone(),
                            MapReadMode::Keys(keys.iter().cloned().collect()),
                        );
                    }
                    MapProjection::Keys(_) => {}
                }
            }

            let packs: Option<Arc<dyn PackResolver>> = file.pack.as_ref().map(|pack| {
                Arc::new(SingleFilePackResolver {
                    pack: pack.clone(),
                    io: self.io.clone(),
                    metadata_cache: Arc::clone(&self.metadata_cache),
                    runtime: self.runtime.clone(),
                    cancel: cancel.clone(),
                }) as Arc<dyn PackResolver>
            });

            let reader = Arc::new(
                guard_sync(|| StatamReader::open_with_shared_meta(rr, Arc::clone(&meta), packs))
                    .map_err(|e| map_statam_err(&self.suspects, file.id, e))?,
            );

            // M3.5.2: Exact `sort_desc_hint` and `req.limit` rules — now unlocked whenever
            // per-file `residual` is empty and 0 applicable tombstones exist!
            let no_residual_or_tombstones = remapped_filters.is_empty() && tombstone_pred.is_none();
            let sort_desc_hint = self.plan.eligible_sort_desc
                && no_residual_or_tombstones
                && self.plan.limit.is_some()
                && rg_indices.is_none();
            let req_limit =
                if no_residual_or_tombstones && (!self.plan.has_sort_hints || sort_desc_hint) {
                    if !self.plan.has_sort_hints {
                        self.remaining
                    } else {
                        self.plan.limit
                    }
                } else {
                    None
                };

            let use_take_rows = use_take_rows
                && map_projections
                    .values()
                    .all(|mode| matches!(mode, MapReadMode::Full));

            let cursor = if use_take_rows {
                let mut ranges = VecDeque::new();
                for rg in &meta.rg_dir {
                    let start = u64::from(rg.first_row);
                    let end = start.saturating_add(u64::from(rg.row_count));
                    if start < end {
                        ranges.push_back((start, end));
                    }
                }
                FileScanCursor::TakeRows {
                    reader,
                    projection: req_projection,
                    ranges,
                    buffered: VecDeque::new(),
                    batch_size: self.batch_size,
                }
            } else {
                let req = ScanRequest {
                    projection: req_projection,
                    map_projections,
                    time_range: self.plan.time_range,
                    id_lookups: classified.id_lookups,
                    predicates: classified.predicates,
                    filter_exprs: classified.filter_exprs,
                    substrings: classified.substrings,
                    sort_desc_hint,
                    limit: req_limit,
                    coalesce_mode: CoalesceMode::Auto,
                    read_threads: self.read_threads,
                    rg_indices,
                    executor: self.executor.clone(),
                };
                let iter = guard_sync(|| reader.scan_iter(req))
                    .map_err(|e| map_statam_err(&self.suspects, file.id, e))?;
                if self.plan.emit_row_coords {
                    FileScanCursor::IterWithCoords {
                        iter: Box::new(iter),
                        file_ordinal,
                    }
                } else {
                    FileScanCursor::Iter(Box::new(iter))
                }
            };

            return Ok(Some(ActiveFileScan {
                file_id: file.id,
                cursor,
                fill_layout,
                coord_scan_schema,
                tombstone_pred,
                remapped_filters,
                scan_to_output,
            }));
        }
        Ok(None)
    }
}

fn fill_missing_columns(batch: &Batch, layout: &ScanBatchFill) -> Result<Batch, QueryError> {
    let raw_len = batch.raw_len();
    let src_cols = batch.columns();
    let mut cols = Vec::with_capacity(layout.slots.len());
    for slot in &layout.slots {
        match slot {
            ScanColSlot::FromFile(idx) => {
                let col = src_cols
                    .get(*idx)
                    .ok_or_else(|| QueryError::internal("scanned column index out of bounds"))?;
                cols.push(Arc::clone(col));
            }
            ScanColSlot::FillNull(dt) => {
                cols.push(Arc::new(Column::Const {
                    value: ScalarValue::Null(dt.clone()),
                    len: raw_len,
                }));
            }
        }
    }
    Ok(Batch::try_new_shared(
        Arc::clone(&layout.schema),
        cols,
        raw_len,
        batch.selection().clone(),
    )?)
}

fn attach_row_coords(
    batch: &Batch,
    coord_schema: &Arc<Schema>,
    file_ordinal: u32,
    row_ids: &[u64],
) -> Result<Batch, QueryError> {
    if row_ids.len() != batch.num_rows() {
        return Err(QueryError::internal(
            "row_ids length mismatch in open_with_row_coords",
        ));
    }
    let raw_len = batch.raw_len();
    let mut row_idx_vals = vec![0i64; raw_len];
    match batch.selection() {
        Selection::All => {
            for (pos, &rid) in row_ids.iter().enumerate() {
                row_idx_vals[pos] = i64::try_from(rid).map_err(|_| QueryError::TooLarge)?;
            }
        }
        Selection::Mask(bm) => {
            for (phys, &rid) in bm.iter_ones().zip(row_ids.iter()) {
                row_idx_vals[phys] = i64::try_from(rid).map_err(|_| QueryError::TooLarge)?;
            }
        }
        Selection::Indices(idxs) => {
            for (&phys, &rid) in idxs.iter().zip(row_ids.iter()) {
                row_idx_vals[phys as usize] =
                    i64::try_from(rid).map_err(|_| QueryError::TooLarge)?;
            }
        }
    }
    let mut cols = Vec::with_capacity(batch.columns().len() + 2);
    cols.extend(batch.columns().iter().map(Arc::clone));
    cols.push(Arc::new(Column::Const {
        value: ScalarValue::I64(i64::from(file_ordinal)),
        len: raw_len,
    }));
    cols.push(Arc::new(Column::Flat(FlatColumn::try_new(
        FlatData::I64(row_idx_vals),
        None,
    )?)));
    Ok(Batch::try_new_shared(
        Arc::clone(coord_schema),
        cols,
        raw_len,
        batch.selection().clone(),
    )?)
}

fn project_scanned_batch(
    batch: Batch,
    projected_schema: &Arc<Schema>,
    scan_to_output: Option<&[usize]>,
) -> Result<Batch, QueryError> {
    let Some(indices) = scan_to_output else {
        return Ok(batch);
    };
    let cols: Vec<Arc<Column>> = indices
        .iter()
        .map(|&idx| Arc::clone(&batch.columns()[idx]))
        .collect();
    let out = Batch::try_new_shared(
        Arc::clone(projected_schema),
        cols,
        batch.raw_len(),
        batch.selection().clone(),
    )?;
    Ok(out)
}

fn truncate_active_rows(batch: Batch, keep: usize) -> Result<Batch, QueryError> {
    if batch.num_rows() <= keep {
        return Ok(batch);
    }
    match batch.selection() {
        Selection::All => Ok(batch.slice(0, keep, None)?),
        Selection::Indices(ids) => {
            let trimmed = ids[..keep].to_vec();
            Ok(batch.into_with_selection(Selection::Indices(trimmed))?)
        }
        Selection::Mask(bm) => {
            let mut trimmed = Vec::with_capacity(keep);
            for row in bm.iter_ones().take(keep) {
                let idx = u32::try_from(row).map_err(|_| QueryError::TooLarge)?;
                trimmed.push(idx);
            }
            Ok(batch.into_with_selection(Selection::Indices(trimmed))?)
        }
    }
}

impl PartitionStream for StatamPartitionStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_ref().map_or(0, |r| r.size());

        loop {
            if cancel.is_cancelled() {
                rollback_res(res, before);
                return Err(QueryError::Cancelled);
            }
            if self.remaining == Some(0) {
                rollback_res(res, before);
                return Ok(None);
            }

            if self.active_file.is_none() {
                match self.open_next_file(cancel) {
                    Ok(Some(active)) => self.active_file = Some(active),
                    Ok(None) => {
                        rollback_res(res, before);
                        return Ok(None);
                    }
                    Err(err) => {
                        rollback_res(res, before);
                        return Err(err);
                    }
                }
            }

            let Some(active) = self.active_file.as_mut() else {
                rollback_res(res, before);
                return Ok(None);
            };
            let file_id = active.file_id;
            let pulled = active.cursor.next_batch(cancel, res.as_deref_mut());
            let (raw_batch, coords_opt) = match pulled {
                Ok(Some(pair)) => pair,
                Ok(None) => {
                    self.active_file = None;
                    rollback_res(res.as_deref_mut(), before);
                    continue;
                }
                Err(err) => {
                    self.active_file = None;
                    rollback_res(res, before);
                    return Err(map_statam_err(&self.suspects, file_id, err));
                }
            };

            let raw_batch = match &active.fill_layout {
                Some(layout) => fill_missing_columns(&raw_batch, layout)
                    .inspect_err(|_| rollback_res(res.as_deref_mut(), before))?,
                None => raw_batch,
            };

            let raw_batch = match (&active.coord_scan_schema, coords_opt) {
                (Some(coord_schema), Some((file_ord, row_ids))) => {
                    attach_row_coords(&raw_batch, coord_schema, file_ord, &row_ids)
                        .inspect_err(|_| rollback_res(res.as_deref_mut(), before))?
                }
                _ => raw_batch,
            };

            let filtered = apply_tombstones_and_filters(
                raw_batch,
                active.tombstone_pred.as_ref(),
                &active.remapped_filters,
                &self.eval,
                res.as_deref_mut(),
                before,
            )?;
            if filtered.is_empty() {
                rollback_res(res.as_deref_mut(), before);
                continue;
            }

            let mut out = project_scanned_batch(
                filtered,
                &self.plan.projected_schema,
                active.scan_to_output.as_deref(),
            )
            .inspect_err(|_| rollback_res(res.as_deref_mut(), before))?;

            if let Some(rem) = self.remaining.as_mut() {
                if out.num_rows() > *rem {
                    out = truncate_active_rows(out, *rem)
                        .inspect_err(|_| rollback_res(res.as_deref_mut(), before))?;
                }
                *rem = rem.saturating_sub(out.num_rows());
            }

            reconcile_res(res, before, out.memory_bytes())?;
            return Ok(Some(out));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use cetra_column::{Batch, CancelToken, DataType, GlobalPool, ScalarValue, Schema};
    use cetra_core::ids::ProjectId;
    use cetra_core::schema::{scores, spans, traces};
    use cetra_query::exec::{
        ExecContext, ExecMetrics, IdLookupHint, MapProjection, ScanSource, ScanSpec, SortHint,
        SpillManager, SubstringHint, TimeRangeHint, WorkerPool,
    };
    use cetra_query::{BinaryOp, EvalContext, Expr, QueryError, ScalarFn};
    use cetra_statam::{
        ChunkPackBuilder, InMemoryPackResolver, PackWriteOptions, SliceReader, StatamReader,
    };
    use chrono::NaiveDate;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
    use uuid::Uuid;

    use super::*;
    use crate::catalog::{FileId, FileMeta, FileTable, PackMeta};
    use crate::statam_io::{BatchFileWriter, write_file_batch};
    use crate::test_support::{
        Call, ChaosStore, Fault, score_row, score_rows, span_row, span_rows, trace_batch,
    };
    use crate::verify::{Suspects, VerifyQueue};

    /// Decodes a stored `.statam` file (and optional sibling `.stpk` pack bytes) into one `Batch`.
    pub(crate) fn read_stored_statam(
        bytes: Bytes,
        pack: Option<([u8; 16], Bytes)>,
        schema: Arc<Schema>,
    ) -> Batch {
        let rr = Arc::new(SliceReader::new(bytes));
        let packs: Option<Arc<dyn PackResolver>> = pack.map(|(pack_id, pack_bytes)| {
            let mut resolver = InMemoryPackResolver::new();
            resolver
                .insert_bytes(pack_id, pack_bytes)
                .expect("open test pack");
            Arc::new(resolver) as Arc<dyn PackResolver>
        });
        let reader = Arc::new(StatamReader::open(rr, packs).expect("open test statam"));
        let batches = reader
            .scan(&ScanRequest {
                read_threads: 1,
                ..ScanRequest::default()
            })
            .expect("scan test statam");
        if batches.is_empty() {
            return Batch::empty(schema);
        }
        let refs: Vec<&Batch> = batches.iter().collect();
        Batch::concat(schema, &refs, None).expect("concat statam batches")
    }

    fn project() -> ProjectId {
        "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
    }

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 3, d).unwrap()
    }

    fn ns(day_num: u32, secs: i64) -> i64 {
        let date = day(day_num);
        date.and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_nanos_opt()
            .unwrap()
            + secs * 1_000_000_000
    }

    fn make_exec_ctx(pool_cap: usize, cancel: CancelToken) -> ExecContext {
        let pool = GlobalPool::standalone(pool_cap);
        let workers = Arc::new(WorkerPool::new(1).unwrap());
        ExecContext {
            pool,
            eval: EvalContext::new(0, cancel),
            spill: Arc::new(SpillManager::with_fixed_cap(PathBuf::new(), 0)),
            workers,
            batch_size: 8192,
            target_partitions: 1,
            metrics: ExecMetrics::new(),
        }
    }

    async fn put_statam_file(
        store: &Arc<dyn ObjectStore>,
        table: FileTable,
        level: u8,
        d: NaiveDate,
        batch: &Batch,
    ) -> Arc<FileMeta> {
        let written = write_file_batch(table, level, &[Arc::new(batch.clone())]).unwrap();
        let (path, id) = crate::objects::Objects::table_file_path(table, project(), d, level);
        store
            .put(
                &ObjectPath::from(path.as_str()),
                PutPayload::from(written.bytes.clone()),
            )
            .await
            .unwrap();
        Arc::new(FileMeta {
            id,
            table,
            project: project(),
            day: d,
            level,
            path,
            size: written.bytes.len() as u64,
            footer_size: written.footer_size,
            rows: written.rows,
            min_time: written.min_time,
            max_time: written.max_time,
            min_ingest_time: written.min_ingest_time,
            max_ingest_time: written.max_ingest_time,
            fully_merged: level == 1,
            deletions_applied_seq: 0,
            pack: None,
        })
    }

    async fn put_l1_spans_with_pack(
        store: &Arc<dyn ObjectStore>,
        d: NaiveDate,
        batch: &Batch,
    ) -> Arc<FileMeta> {
        let (path, id) =
            crate::objects::Objects::table_file_path(FileTable::Spans, project(), d, 1);
        let pack_uuid = Uuid::now_v7();
        let pack_id = *pack_uuid.as_bytes();
        let pack_path = crate::objects::Objects::pack_file_path(project(), d, 1, pack_uuid);
        let pb = ChunkPackBuilder::new_streaming(
            pack_id,
            PackWriteOptions::default(),
            Cursor::new(Vec::new()),
        )
        .unwrap();
        let mut writer =
            BatchFileWriter::new_with_pack(FileTable::Spans, 1, Cursor::new(Vec::new()), Some(pb))
                .unwrap();
        writer.write(batch).unwrap();
        let (statam_cursor, summary, pb_opt) = writer.finish().unwrap();
        let statam_bytes = Bytes::from(statam_cursor.into_inner());
        let pb = pb_opt.unwrap();
        let pack_meta = if pb.chunk_count() > 0 {
            let (pack_cursor, pstats) = pb.finish_into_inner().unwrap();
            let pack_bytes = Bytes::from(pack_cursor.into_inner());
            store
                .put(
                    &ObjectPath::from(pack_path.as_str()),
                    PutPayload::from(pack_bytes.clone()),
                )
                .await
                .unwrap();
            Some(PackMeta {
                id: pack_id,
                path: pack_path,
                size: pack_bytes.len() as u64,
                toc_size: u64::from(pstats.toc_stored_bytes),
            })
        } else {
            None
        };
        store
            .put(
                &ObjectPath::from(path.as_str()),
                PutPayload::from(statam_bytes.clone()),
            )
            .await
            .unwrap();
        Arc::new(FileMeta {
            id,
            table: FileTable::Spans,
            project: project(),
            day: d,
            level: 1,
            path,
            size: statam_bytes.len() as u64,
            footer_size: summary.footer_size,
            rows: summary.rows,
            min_time: summary.min_time,
            max_time: summary.max_time,
            min_ingest_time: summary.min_ingest_time,
            max_ingest_time: summary.max_ingest_time,
            fully_merged: true,
            deletions_applied_seq: 0,
            pack: pack_meta,
        })
    }

    fn drain_source(
        source: &StatamSource,
        spec: &ScanSpec,
        ctx: &ExecContext,
    ) -> Result<Vec<Batch>, QueryError> {
        let streams = source.open_partitions(spec, ctx)?;
        let cancel = ctx.eval.cancel.clone();
        let pool = Arc::clone(&ctx.pool);
        let tasks: Vec<_> = streams
            .into_iter()
            .map(|mut s| {
                let c = cancel.clone();
                let p = Arc::clone(&pool);
                move |_idx| {
                    let mut out = Vec::new();
                    let mut res = p.register("test_drain", false);
                    loop {
                        res.free();
                        match s.next_batch(&c, Some(&mut res))? {
                            Some(b) => out.push(b),
                            None => break,
                        }
                    }
                    res.free();
                    Ok(out)
                }
            })
            .collect();
        let parts = ctx.workers.run_scoped(&cancel, tasks)?;
        Ok(parts.into_iter().flatten().collect())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn map_projections_lattice_respects_full_keys_and_filter_merge() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut s1 = span_row(1, 0, ns(5, 10), ns(5, 11));
        s1.metadata.insert("env".to_owned(), "prod".to_owned());
        s1.metadata.insert("team".to_owned(), "core".to_owned());
        s1.metadata.insert("extra".to_owned(), "keep".to_owned());
        let mut s2 = span_row(2, 0, ns(5, 20), ns(5, 21));
        s2.metadata.insert("env".to_owned(), "dev".to_owned());
        s2.metadata.insert("team".to_owned(), "ml".to_owned());
        let batch = span_rows(&[s1, s2]);
        let file = put_statam_file(&store, FileTable::Spans, 1, day(5), &batch).await;

        let schema = FileTable::Spans.column_schema();
        let meta_idx = schema.index_of(spans::METADATA).unwrap();
        let cache = Arc::new(MetadataCache::new(4 * 1024 * 1024));
        let queue = Arc::new(VerifyQueue::default());
        let source = StatamSource::new(
            StorageIo::from_store(store),
            FileTable::Spans,
            Arc::clone(&schema),
            vec![file],
            Arc::from([]),
            cache,
            Suspects::new(queue),
            tokio::runtime::Handle::current(),
        );
        let ctx = make_exec_ctx(64 * 1024 * 1024, CancelToken::new());

        // Case 1: MapProjection::Keys({"env"}) + filter on metadata['team'] = 'core' -> merges both keys!
        let filter = Expr::Binary {
            left: Box::new(Expr::MapIndex {
                map: Box::new(Expr::Column {
                    index: meta_idx,
                    name: spans::METADATA.to_owned(),
                    data_type: DataType::MapUtf8,
                }),
                key: Box::new(Expr::Literal(ScalarValue::Utf8("team".to_owned()))),
            }),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::Utf8("core".to_owned()))),
            data_type: DataType::Bool,
        };
        let mut mp = BTreeMap::new();
        mp.insert(
            spans::METADATA.to_owned(),
            MapProjection::Keys(BTreeSet::from(["env".to_owned()])),
        );
        let spec = ScanSpec {
            projection: Some(vec![meta_idx]),
            map_projections: mp,
            filters: vec![filter],
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &spec, &ctx).unwrap();
        let total: usize = batches.iter().map(Batch::num_rows).sum();
        assert_eq!(total, 1, "filter on metadata['team'] must find s1");

        // Case 2: MapProjection::KeysOnly must NOT emit `map_keys` (must preserve all keys).
        let mut mp_keys_only = BTreeMap::new();
        mp_keys_only.insert(spans::METADATA.to_owned(), MapProjection::KeysOnly);
        let spec_keys_only = ScanSpec {
            projection: Some(vec![meta_idx]),
            map_projections: mp_keys_only,
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &spec_keys_only, &ctx).unwrap();
        let rows: usize = batches.iter().map(Batch::num_rows).sum();
        assert_eq!(rows, 2);
        // Verify `extra` key is still present on s1 (not pruned away).
        let combined = Batch::concat(
            Arc::new(schema.project(&[meta_idx]).unwrap()),
            &batches.iter().collect::<Vec<_>>(),
            None,
        )
        .unwrap();
        let ScalarValue::MapUtf8(entries) = combined.column(0).unwrap().get_scalar(0).unwrap()
        else {
            panic!("expected MapUtf8");
        };
        assert_eq!(entries.len(), 3, "KeysOnly must retain all 3 map keys");

        // Case 3: Projected MapUtf8 absent from `spec.map_projections` + filter on `metadata['env']`:
        // Must treat output column as `Full` (so all 3 keys survive in output!).
        let env_filter = Expr::Binary {
            left: Box::new(Expr::MapIndex {
                map: Box::new(Expr::Column {
                    index: meta_idx,
                    name: spans::METADATA.to_owned(),
                    data_type: DataType::MapUtf8,
                }),
                key: Box::new(Expr::Literal(ScalarValue::Utf8("env".to_owned()))),
            }),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::Utf8("prod".to_owned()))),
            data_type: DataType::Bool,
        };
        let spec_full = ScanSpec {
            projection: Some(vec![meta_idx]),
            filters: vec![env_filter],
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &spec_full, &ctx).unwrap();
        let combined = Batch::concat(
            Arc::new(schema.project(&[meta_idx]).unwrap()),
            &batches.iter().collect::<Vec<_>>(),
            None,
        )
        .unwrap();
        assert_eq!(combined.num_rows(), 1);
        let ScalarValue::MapUtf8(entries) = combined.column(0).unwrap().get_scalar(0).unwrap()
        else {
            panic!("expected MapUtf8");
        };
        assert_eq!(
            entries.len(),
            3,
            "absent from map_projections means Full even with map_key filter"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cnf_substrings_push_single_disjunct_and_preserve_or_disjuncts() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut s1 = span_row(1, 0, ns(5, 10), ns(5, 11));
        s1.input = Some("alpha ".repeat(200));
        s1.output = Some("none".to_owned());
        let mut s2 = span_row(2, 0, ns(5, 20), ns(5, 21));
        s2.input = Some("none".to_owned());
        s2.output = Some("beta ".repeat(200));
        let mut s3 = span_row(3, 0, ns(5, 30), ns(5, 31));
        s3.input = Some("gamma".to_owned());
        s3.output = Some("delta".to_owned());
        let batch = span_rows(&[s1, s2, s3]);
        let file = put_l1_spans_with_pack(&store, day(5), &batch).await;
        assert!(
            file.pack.is_some(),
            "L1 spans with >1KB payloads creates sibling .stpk"
        );

        let schema = FileTable::Spans.column_schema();
        let in_idx = schema.index_of(spans::INPUT).unwrap();
        let out_idx = schema.index_of(spans::OUTPUT).unwrap();
        let cache = Arc::new(MetadataCache::new(4 * 1024 * 1024));
        let source = StatamSource::new(
            StorageIo::from_store(store),
            FileTable::Spans,
            Arc::clone(&schema),
            vec![file],
            Arc::from([]),
            Arc::clone(&cache),
            Suspects::new(Arc::new(VerifyQueue::default())),
            tokio::runtime::Handle::current(),
        );
        let ctx = make_exec_ctx(64 * 1024 * 1024, CancelToken::new());

        // Multi-disjunct OR: (input ILIKE '%alpha%' OR output ILIKE '%beta%').
        // Must return BOTH s1 and s2 (never flattened into AND!).
        let or_expr = Expr::Binary {
            left: Box::new(Expr::Call {
                func: ScalarFn::Contains,
                args: vec![
                    Expr::Column {
                        index: in_idx,
                        name: spans::INPUT.to_owned(),
                        data_type: DataType::Utf8,
                    },
                    Expr::Literal(ScalarValue::Utf8("alpha".to_owned())),
                ],
                data_type: DataType::Bool,
            }),
            op: BinaryOp::Or,
            right: Box::new(Expr::Call {
                func: ScalarFn::Contains,
                args: vec![
                    Expr::Column {
                        index: out_idx,
                        name: spans::OUTPUT.to_owned(),
                        data_type: DataType::Utf8,
                    },
                    Expr::Literal(ScalarValue::Utf8("beta".to_owned())),
                ],
                data_type: DataType::Bool,
            }),
            data_type: DataType::Bool,
        };
        let spec_or = ScanSpec {
            projection: Some(vec![in_idx, out_idx]),
            filters: vec![or_expr],
            substrings: vec![vec![
                SubstringHint {
                    column: spans::INPUT.to_owned(),
                    needle: "alpha".to_owned(),
                    case_insensitive: true,
                },
                SubstringHint {
                    column: spans::OUTPUT.to_owned(),
                    needle: "beta".to_owned(),
                    case_insensitive: true,
                },
            ]],
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &spec_or, &ctx).unwrap();
        let count: usize = batches.iter().map(Batch::num_rows).sum();
        assert_eq!(count, 2, "OR substring clause must return both s1 and s2");
        assert!(cache.bytes() > 0, "both .statam meta and .stpk toc cached");

        // Second query hits cache for both .statam and .stpk!
        let hits_before = cache.hits();
        let spec_single = ScanSpec {
            projection: Some(vec![in_idx]),
            substrings: vec![vec![SubstringHint {
                column: spans::INPUT.to_owned(),
                needle: "alpha".to_owned(),
                case_insensitive: false,
            }]],
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &spec_single, &ctx).unwrap();
        let count: usize = batches.iter().map(Batch::num_rows).sum();
        assert_eq!(count, 1);
        assert!(
            cache.hits() >= hits_before + 2,
            "expected StatamFileMeta + PackToc cache hits"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sort_desc_hint_and_around_ns_and_zero_col_count() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let tb1 = trace_batch(&[
            span_row(1, 0, ns(1, 10), ns(1, 11)),
            span_row(2, 0, ns(1, 20), ns(1, 21)),
            span_row(3, 0, ns(1, 30), ns(1, 31)),
        ]);
        let tb2 = trace_batch(&[span_row(4, 0, ns(10, 10), ns(10, 11))]);
        let f1 = put_statam_file(&store, FileTable::Traces, 1, day(1), &tb1).await;
        let f2 = put_statam_file(&store, FileTable::Traces, 1, day(10), &tb2).await;

        let schema = FileTable::Traces.column_schema();
        let start_idx = schema.index_of(traces::START_TIME).unwrap();
        let source = StatamSource::new(
            StorageIo::from_store(store),
            FileTable::Traces,
            Arc::clone(&schema),
            vec![f1, f2],
            Arc::from([]),
            Arc::new(MetadataCache::new(1024 * 1024)),
            Suspects::new(Arc::new(VerifyQueue::default())),
            tokio::runtime::Handle::current(),
        );
        let ctx = make_exec_ctx(64 * 1024 * 1024, CancelToken::new());

        // 0-column count scan.
        let count_spec = ScanSpec {
            projection: Some(vec![]),
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &count_spec, &ctx).unwrap();
        let total: usize = batches.iter().map(Batch::num_rows).sum();
        assert_eq!(total, 4);
        assert_eq!(batches[0].schema().len(), 0);

        // `around_ns` prunes day(10) when looking up trace 1 around day(1).
        let tid1 = *span_row(1, 0, 0, 0).trace_id.as_bytes();
        let around_spec = ScanSpec {
            projection: Some(vec![start_idx]),
            time_range: TimeRangeHint {
                min_ns: None,
                max_ns: None,
                around_ns: Some(ns(1, 15)),
            },
            id_lookups: vec![IdLookupHint::Fixed16 {
                column: traces::TRACE_ID.to_owned(),
                values: vec![tid1],
            }],
            ..ScanSpec::default()
        };
        let kept = source.prune_files(&around_spec);
        assert_eq!(kept.len(), 1, "around_ns must prune day(10) file");
        let batches = drain_source(&source, &around_spec, &ctx).unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 1);

        // `sort_desc_hint` on Scores with `created_at DESC LIMIT 1`.
        let scores_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let sb = score_rows(&[
            score_row(1, 1, ns(5, 10), ns(5, 10), ns(5, 10), false, "a"),
            score_row(2, 1, ns(5, 20), ns(5, 20), ns(5, 20), false, "b"),
            score_row(3, 1, ns(5, 30), ns(5, 30), ns(5, 30), false, "c"),
        ]);
        let sf = put_statam_file(&scores_store, FileTable::Scores, 1, day(5), &sb).await;
        let sc_schema = FileTable::Scores.column_schema();
        let sc_created = sc_schema.index_of(scores::CREATED_AT).unwrap();
        let sc_source = StatamSource::new(
            StorageIo::from_store(scores_store),
            FileTable::Scores,
            Arc::clone(&sc_schema),
            vec![sf],
            Arc::from([]),
            Arc::new(MetadataCache::new(1024 * 1024)),
            Suspects::new(Arc::new(VerifyQueue::default())),
            tokio::runtime::Handle::current(),
        );
        let desc_spec = ScanSpec {
            projection: Some(vec![sc_created]),
            sort_hints: vec![SortHint {
                column: scores::CREATED_AT.to_owned(),
                descending: true,
            }],
            limit: Some(1),
            ..ScanSpec::default()
        };
        let batches = drain_source(&sc_source, &desc_spec, &ctx).unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 1);
        let active = batches[0]
            .selection()
            .to_indices(batches[0].raw_len())
            .unwrap();
        let ScalarValue::TimestampNs { value, .. } = batches[0]
            .column(0)
            .unwrap()
            .get_scalar(active[0] as usize)
            .unwrap()
        else {
            panic!("expected TimestampNs");
        };
        assert_eq!(value, ns(5, 30), "sort_desc_hint must return newest row");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oom_rollback_cancellation_and_corrupt_suspect_reporting() {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let chaos = Arc::new(ChaosStore::new(Arc::clone(&inner)));
        let batch = span_rows(&[
            span_row(1, 0, ns(5, 10), ns(5, 11)),
            span_row(2, 0, ns(5, 20), ns(5, 21)),
        ]);
        let file = put_statam_file(&inner, FileTable::Spans, 0, day(5), &batch).await;
        let schema = FileTable::Spans.column_schema();

        // 1. Reservation OOM rolls back cleanly.
        let source = StatamSource::new(
            StorageIo::from_store(Arc::clone(&inner)),
            FileTable::Spans,
            Arc::clone(&schema),
            vec![Arc::clone(&file)],
            Arc::from([]),
            Arc::new(MetadataCache::new(0)),
            Suspects::new(Arc::new(VerifyQueue::default())),
            tokio::runtime::Handle::current(),
        );
        let tiny_ctx = make_exec_ctx(16, CancelToken::new());
        let err = drain_source(&source, &ScanSpec::default(), &tiny_ctx).unwrap_err();
        assert!(matches!(err, QueryError::TooLarge), "got {err:?}");
        assert_eq!(
            tiny_ctx.pool.reserved(),
            0,
            "pool must roll back to 0 on OOM"
        );

        // 2. CancelToken mid-slow-S3-GET cancels immediately without hanging.
        chaos.push(Call::Get, Fault::Delay(Duration::from_secs(30)));
        let cancel = CancelToken::new();
        let cancel_source = StatamSource::new(
            StorageIo::from_store(chaos.clone() as Arc<dyn ObjectStore>),
            FileTable::Spans,
            Arc::clone(&schema),
            vec![Arc::clone(&file)],
            Arc::from([]),
            Arc::new(MetadataCache::new(0)),
            Suspects::new(Arc::new(VerifyQueue::default())),
            tokio::runtime::Handle::current(),
        );
        let cancel_ctx = make_exec_ctx(64 * 1024 * 1024, cancel.clone());
        let cancel_trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            cancel_trigger.cancel();
        });
        let started = std::time::Instant::now();
        let res = tokio::task::spawn_blocking(move || {
            drain_source(&cancel_source, &ScanSpec::default(), &cancel_ctx)
        })
        .await
        .unwrap();
        assert!(
            matches!(res, Err(QueryError::Cancelled)),
            "expected Cancelled, got {res:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cancellation must abort delayed GET promptly"
        );

        // 3. Corrupt file reports suspect; transient store error does NOT report suspect.
        let corrupt_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut corrupt_file = (*file).clone();
        corrupt_file.id = FileId(Uuid::now_v7());
        corrupt_store
            .put(
                &ObjectPath::from(corrupt_file.path.as_str()),
                PutPayload::from(Bytes::from_static(b"not a valid statam file at all!!")),
            )
            .await
            .unwrap();
        corrupt_file.size = 32;
        let queue = Arc::new(VerifyQueue::default());
        let bad_source = StatamSource::new(
            StorageIo::from_store(corrupt_store),
            FileTable::Spans,
            Arc::clone(&schema),
            vec![Arc::new(corrupt_file.clone())],
            Arc::from([]),
            Arc::new(MetadataCache::new(0)),
            Suspects::new(Arc::clone(&queue)),
            tokio::runtime::Handle::current(),
        );
        let ok_ctx = make_exec_ctx(64 * 1024 * 1024, CancelToken::new());
        let err = drain_source(&bad_source, &ScanSpec::default(), &ok_ctx).unwrap_err();
        assert!(matches!(err, QueryError::CorruptData(_)), "got {err:?}");
        assert_eq!(queue.take(10), vec![corrupt_file.id]);

        // Transient store error returns StorageIo and does NOT report suspect.
        let transient_chaos = Arc::new(ChaosStore::new(Arc::clone(&inner)));
        transient_chaos.push(
            Call::Get,
            Fault::Before(crate::test_support::Status::Internal),
        );
        let transient_queue = Arc::new(VerifyQueue::default());
        let transient_source = StatamSource::new(
            StorageIo::from_store(transient_chaos as Arc<dyn ObjectStore>),
            FileTable::Spans,
            Arc::clone(&schema),
            vec![Arc::clone(&file)],
            Arc::from([]),
            Arc::new(MetadataCache::new(0)),
            Suspects::new(Arc::clone(&transient_queue)),
            tokio::runtime::Handle::current(),
        );
        let transient_ctx = make_exec_ctx(64 * 1024 * 1024, CancelToken::new());
        let err =
            drain_source(&transient_source, &ScanSpec::default(), &transient_ctx).unwrap_err();
        assert!(
            matches!(err, QueryError::StorageIo(_)),
            "expected StorageIo, got {err:?}"
        );
        assert!(
            transient_queue.take(10).is_empty(),
            "transient store error must not report suspect"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn schema_evolution_fills_missing_nullable_column_with_null() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut s1 = span_row(1, 0, ns(5, 10), ns(5, 11));
        s1.name = "first-span".to_owned();
        s1.metadata.insert("env".to_owned(), "prod".to_owned());
        let mut s2 = span_row(2, 0, ns(5, 20), ns(5, 21));
        s2.name = "second-span".to_owned();
        s2.metadata.insert("env".to_owned(), "dev".to_owned());
        let batch = span_rows(&[s1, s2]);
        // File is written with the base spans schema (subset of the evolved schema below).
        let file = put_statam_file(&store, FileTable::Spans, 1, day(5), &batch).await;

        // Evolved schema inserts a new nullable Utf8 column at index 0 (shifting all
        // subsequent column indices) and appends another nullable Utf8 column at the end.
        let mut evolved_fields = vec![Field::new("evolved_prefix", DataType::Utf8, true)];
        evolved_fields.extend(FileTable::Spans.column_schema().fields().iter().cloned());
        evolved_fields.push(Field::new("evolved_suffix", DataType::Utf8, true));
        let evolved_schema = Schema::try_new_arc(evolved_fields).unwrap();
        let prefix_idx = evolved_schema.index_of("evolved_prefix").unwrap();
        let suffix_idx = evolved_schema.index_of("evolved_suffix").unwrap();
        let name_idx = evolved_schema.index_of(spans::NAME).unwrap();
        let meta_idx = evolved_schema.index_of(spans::METADATA).unwrap();

        let queue = Arc::new(VerifyQueue::default());
        let source = StatamSource::new(
            StorageIo::from_store(Arc::clone(&store)),
            FileTable::Spans,
            Arc::clone(&evolved_schema),
            vec![Arc::clone(&file)],
            Arc::from([]),
            Arc::new(MetadataCache::new(1024 * 1024)),
            Suspects::new(Arc::clone(&queue)),
            tokio::runtime::Handle::current(),
        );
        let ctx = make_exec_ctx(64 * 1024 * 1024, CancelToken::new());

        // 1. Projected missing nullable columns + shifted present column + map_keys projection.
        let mut mp = BTreeMap::new();
        mp.insert(
            spans::METADATA.to_owned(),
            MapProjection::Keys(BTreeSet::from(["env".to_owned()])),
        );
        let proj_spec = ScanSpec {
            projection: Some(vec![prefix_idx, name_idx, meta_idx, suffix_idx]),
            map_projections: mp,
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &proj_spec, &ctx).unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 2);
        for b in &batches {
            assert!(matches!(
                b.column(0).unwrap(),
                Column::Const {
                    value: ScalarValue::Null(DataType::Utf8),
                    ..
                }
            ));
            assert!(matches!(
                b.column(3).unwrap(),
                Column::Const {
                    value: ScalarValue::Null(DataType::Utf8),
                    ..
                }
            ));
        }
        let combined = Batch::concat(
            Arc::new(
                evolved_schema
                    .project(&[prefix_idx, name_idx, meta_idx, suffix_idx])
                    .unwrap(),
            ),
            &batches.iter().collect::<Vec<_>>(),
            None,
        )
        .unwrap();
        assert_eq!(
            combined.column(1).unwrap().get_scalar(0).unwrap(),
            ScalarValue::Utf8("first-span".to_owned())
        );
        assert_eq!(
            combined.column(1).unwrap().get_scalar(1).unwrap(),
            ScalarValue::Utf8("second-span".to_owned())
        );
        assert!(queue.take(10).is_empty(), "zero suspects expected");

        // 2. Filter `evolved_suffix IS NULL` returns all rows with Const(Null).
        let is_null_spec = ScanSpec {
            projection: Some(vec![suffix_idx]),
            filters: vec![Expr::Unary {
                op: UnaryOp::IsNull,
                expr: Box::new(Expr::Column {
                    index: suffix_idx,
                    name: "evolved_suffix".to_owned(),
                    data_type: DataType::Utf8,
                }),
                data_type: DataType::Bool,
            }],
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &is_null_spec, &ctx).unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 2);
        assert!(matches!(
            batches[0].column(0).unwrap(),
            Column::Const {
                value: ScalarValue::Null(DataType::Utf8),
                ..
            }
        ));
        assert!(queue.take(10).is_empty(), "zero suspects expected");

        // 3. Filter `evolved_suffix = 'x'` short-circuits to 0 rows without error or suspect.
        let eq_spec = ScanSpec {
            projection: Some(vec![name_idx, suffix_idx]),
            filters: vec![Expr::Binary {
                left: Box::new(Expr::Column {
                    index: suffix_idx,
                    name: "evolved_suffix".to_owned(),
                    data_type: DataType::Utf8,
                }),
                op: BinaryOp::Eq,
                right: Box::new(Expr::Literal(ScalarValue::Utf8("x".to_owned()))),
                data_type: DataType::Bool,
            }],
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &eq_spec, &ctx).unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 0);
        assert!(queue.take(10).is_empty(), "zero suspects expected");

        // 4. Single-disjunct `SubstringHint` on missing nullable column short-circuits to 0 rows
        // (never hits `SchemaMismatch`).
        let sub_spec = ScanSpec {
            projection: Some(vec![name_idx]),
            substrings: vec![vec![SubstringHint {
                column: "evolved_suffix".to_owned(),
                needle: "needle".to_owned(),
                case_insensitive: true,
            }]],
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &sub_spec, &ctx).unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 0);
        assert!(queue.take(10).is_empty(), "zero suspects expected");

        // 5. `IdLookupHint` on missing nullable column short-circuits to 0 rows without error.
        let id_spec = ScanSpec {
            projection: Some(vec![name_idx]),
            id_lookups: vec![IdLookupHint::Utf8 {
                column: "evolved_suffix".to_owned(),
                values: vec!["x".to_owned()],
            }],
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &id_spec, &ctx).unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 0);
        assert!(queue.take(10).is_empty(), "zero suspects expected");

        // 6. Missing non-nullable column reports corrupt and enqueues suspect.
        let mut bad_fields = FileTable::Spans.column_schema().fields().to_vec();
        bad_fields.push(Field::new("required_missing", DataType::Utf8, false));
        let bad_schema = Schema::try_new_arc(bad_fields).unwrap();
        let bad_queue = Arc::new(VerifyQueue::default());
        let bad_source = StatamSource::new(
            StorageIo::from_store(store),
            FileTable::Spans,
            bad_schema,
            vec![Arc::clone(&file)],
            Arc::from([]),
            Arc::new(MetadataCache::new(0)),
            Suspects::new(Arc::clone(&bad_queue)),
            tokio::runtime::Handle::current(),
        );
        let err = drain_source(&bad_source, &ScanSpec::default(), &ctx).unwrap_err();
        assert!(matches!(err, QueryError::CorruptData(_)), "got {err:?}");
        assert_eq!(bad_queue.take(10), vec![file.id]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn four_way_or_ui_substring_search_and_non_ascii_ilike_residual_safety() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut s1 = span_row(1, 0, ns(5, 10), ns(5, 11));
        s1.name = "ui-TargetName-span".to_owned();
        s1.input = None;
        s1.output = None;
        s1.system_instructions = None;

        let mut s2 = span_row(2, 0, ns(5, 20), ns(5, 21));
        s2.name = "other".to_owned();
        s2.input = Some(format!("{}targetname{}", "x".repeat(1200), "y".repeat(200)));

        let mut s3 = span_row(3, 0, ns(5, 30), ns(5, 31));
        s3.name = "other".to_owned();
        s3.output = Some(format!("{}TARGETNAME{}", "z".repeat(1200), "w".repeat(200)));

        let mut s4 = span_row(4, 0, ns(5, 40), ns(5, 41));
        s4.name = "other".to_owned();
        s4.system_instructions = Some(format!("{}TaRgEtNaMe{}", "q".repeat(1200), "r".repeat(200)));

        let mut s5 = span_row(5, 0, ns(5, 50), ns(5, 51));
        s5.name = "song-of-ascii".to_owned();
        s5.input = Some("no match here".to_owned());

        let mut s6 = span_row(6, 0, ns(5, 60), ns(5, 61));
        s6.name = "completely-unrelated".to_owned();
        s6.input = None;

        let batch = span_rows(&[s1, s2, s3, s4, s5, s6]);
        let file = put_l1_spans_with_pack(&store, day(5), &batch).await;
        assert!(file.pack.is_some());

        let schema = FileTable::Spans.column_schema();
        let name_idx = schema.index_of(spans::NAME).unwrap();
        let in_idx = schema.index_of(spans::INPUT).unwrap();
        let out_idx = schema.index_of(spans::OUTPUT).unwrap();
        let sys_idx = schema.index_of(spans::SYSTEM_INSTRUCTIONS).unwrap();
        let trace_idx = schema.index_of(spans::TRACE_ID).unwrap();

        let source = StatamSource::new(
            StorageIo::from_store(store),
            FileTable::Spans,
            Arc::clone(&schema),
            vec![file],
            Arc::from([]),
            Arc::new(MetadataCache::new(4 * 1024 * 1024)),
            Suspects::new(Arc::new(VerifyQueue::default())),
            tokio::runtime::Handle::current(),
        );
        let ctx = make_exec_ctx(64 * 1024 * 1024, CancelToken::new());

        // 1. 4-way UI OR search: `name ILIKE '%targetname%' OR input ILIKE '%targetname%' OR
        //    output ILIKE '%targetname%' OR system_instructions ILIKE '%targetname%'`.
        let mk_ilike = |idx: usize, col: &str, needle: &str| Expr::Like {
            expr: Box::new(Expr::Column {
                index: idx,
                name: col.to_owned(),
                data_type: DataType::Utf8,
            }),
            pattern: Box::new(Expr::Literal(ScalarValue::Utf8(format!("%{needle}%")))),
            escape: Some('\\'),
            case_insensitive: true,
            negated: false,
        };
        let four_way_or = Expr::Binary {
            left: Box::new(Expr::Binary {
                left: Box::new(mk_ilike(name_idx, spans::NAME, "targetname")),
                op: BinaryOp::Or,
                right: Box::new(mk_ilike(in_idx, spans::INPUT, "targetname")),
                data_type: DataType::Bool,
            }),
            op: BinaryOp::Or,
            right: Box::new(Expr::Binary {
                left: Box::new(mk_ilike(out_idx, spans::OUTPUT, "targetname")),
                op: BinaryOp::Or,
                right: Box::new(mk_ilike(sys_idx, spans::SYSTEM_INSTRUCTIONS, "targetname")),
                data_type: DataType::Bool,
            }),
            data_type: DataType::Bool,
        };
        let spec_4way = ScanSpec {
            projection: Some(vec![trace_idx]),
            filters: vec![four_way_or],
            substrings: vec![vec![
                SubstringHint {
                    column: spans::NAME.to_owned(),
                    needle: "targetname".to_owned(),
                    case_insensitive: true,
                },
                SubstringHint {
                    column: spans::INPUT.to_owned(),
                    needle: "targetname".to_owned(),
                    case_insensitive: true,
                },
                SubstringHint {
                    column: spans::OUTPUT.to_owned(),
                    needle: "targetname".to_owned(),
                    case_insensitive: true,
                },
                SubstringHint {
                    column: spans::SYSTEM_INSTRUCTIONS.to_owned(),
                    needle: "targetname".to_owned(),
                    case_insensitive: true,
                },
            ]],
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &spec_4way, &ctx).unwrap();
        assert_eq!(
            batches.iter().map(Batch::num_rows).sum::<usize>(),
            4,
            "4-way OR must match s1..s4 across name, input, output, system_instructions"
        );

        // 2. Non-ASCII ILIKE needle `"ſ"` (U+017F Latin small letter long s):
        //    `cetra-query`'s `regex (?i)` folds `"ſ"` <-> `"s"`, so `"song-of-ascii"` MUST match!
        //    Because Statam keeps non-ASCII ILIKE in residual (never exact-pushed), zero false negatives.
        let non_ascii_filter = mk_ilike(name_idx, spans::NAME, "\u{017f}ong");
        let spec_non_ascii = ScanSpec {
            projection: Some(vec![name_idx]),
            filters: vec![non_ascii_filter],
            substrings: vec![vec![SubstringHint {
                column: spans::NAME.to_owned(),
                needle: "\u{017f}ong".to_owned(),
                case_insensitive: true,
            }]],
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &spec_non_ascii, &ctx).unwrap();
        assert_eq!(
            batches.iter().map(Batch::num_rows).sum::<usize>(),
            1,
            "non-ASCII ILIKE must stay in residual and match 'song-of-ascii'"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn differential_filter_parity_3vl_prefix_external_payload_and_filtered_limit() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut rows = Vec::new();
        for i in 0..12_u16 {
            let mut s = span_row(i + 1, 0, ns(5, 10 + i64::from(i)), ns(5, 11 + i64::from(i)));
            s.name = match i % 4 {
                0 => "foo%bar_baz".to_owned(),
                1 => "fooXbar_baz".to_owned(),
                2 => "prefix_alpha".to_owned(),
                _ => "other_name".to_owned(),
            };
            s.environment = match i % 3 {
                0 => Some("prod".to_owned()),
                1 => Some("staging".to_owned()),
                _ => None,
            };
            s.total_tokens = match i {
                0 => None,
                1 => Some(1),
                2 => Some(5),
                3 => Some(10),
                4 => Some(15),
                5 => Some(20),
                _ => Some(i64::from(i) * 3),
            };
            s.status = if i % 5 == 0 {
                cetra_core::enums::SpanStatus::Error
            } else {
                cetra_core::enums::SpanStatus::Ok
            };
            // ExternalPayload column with >1KB payloads so it spills into `.stpk`.
            s.input = match i {
                1 | 7 => Some(format!("exact_payload_{}", "p".repeat(1200))),
                2 => None,
                _ => Some(format!("other_payload_{i}_{}", "q".repeat(1200))),
            };
            rows.push(s);
        }
        let batch = span_rows(&rows);
        let file = put_l1_spans_with_pack(&store, day(5), &batch).await;
        assert!(file.pack.is_some());

        let schema = FileTable::Spans.column_schema();
        let name_idx = schema.index_of(spans::NAME).unwrap();
        let env_idx = schema.index_of(spans::ENVIRONMENT).unwrap();
        let tok_idx = schema.index_of(spans::TOTAL_TOKENS).unwrap();
        let status_idx = schema.index_of(spans::STATUS).unwrap();
        let in_idx = schema.index_of(spans::INPUT).unwrap();
        let start_idx = schema.index_of(spans::START_TIME).unwrap();

        let col = |idx: usize| {
            let f = &schema.fields()[idx];
            Expr::Column {
                index: idx,
                name: f.name.clone(),
                data_type: f.data_type.clone(),
            }
        };
        let exact_payload = format!("exact_payload_{}", "p".repeat(1200));

        let test_exprs: Vec<(&str, Expr)> = vec![
            (
                "NOT (a AND b) 3VL with NULLs",
                Expr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(Expr::Binary {
                        left: Box::new(Expr::Binary {
                            left: Box::new(col(env_idx)),
                            op: BinaryOp::Eq,
                            right: Box::new(Expr::Literal(ScalarValue::Utf8("prod".to_owned()))),
                            data_type: DataType::Bool,
                        }),
                        op: BinaryOp::And,
                        right: Box::new(Expr::Binary {
                            left: Box::new(col(tok_idx)),
                            op: BinaryOp::Gt,
                            right: Box::new(Expr::Literal(ScalarValue::I64(5))),
                            data_type: DataType::Bool,
                        }),
                        data_type: DataType::Bool,
                    }),
                    data_type: DataType::Bool,
                },
            ),
            (
                "NOT (a OR b) 3VL with NULLs",
                Expr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(Expr::Binary {
                        left: Box::new(Expr::Binary {
                            left: Box::new(col(env_idx)),
                            op: BinaryOp::Eq,
                            right: Box::new(Expr::Literal(ScalarValue::Utf8("prod".to_owned()))),
                            data_type: DataType::Bool,
                        }),
                        op: BinaryOp::Or,
                        right: Box::new(Expr::Binary {
                            left: Box::new(col(tok_idx)),
                            op: BinaryOp::Eq,
                            right: Box::new(Expr::Literal(ScalarValue::I64(1))),
                            data_type: DataType::Bool,
                        }),
                        data_type: DataType::Bool,
                    }),
                    data_type: DataType::Bool,
                },
            ),
            (
                "IN (1, NULL)",
                Expr::InList {
                    expr: Box::new(col(tok_idx)),
                    list: vec![
                        Expr::Literal(ScalarValue::I64(1)),
                        Expr::Literal(ScalarValue::Null(DataType::I64)),
                    ],
                    negated: false,
                    redacted: false,
                },
            ),
            (
                "NOT IN (1, NULL) -> always empty",
                Expr::InList {
                    expr: Box::new(col(tok_idx)),
                    list: vec![
                        Expr::Literal(ScalarValue::I64(1)),
                        Expr::Literal(ScalarValue::Null(DataType::I64)),
                    ],
                    negated: true,
                    redacted: false,
                },
            ),
            (
                "NOT BETWEEN 10 AND NULL",
                Expr::Between {
                    expr: Box::new(col(tok_idx)),
                    low: Box::new(Expr::Literal(ScalarValue::I64(10))),
                    high: Box::new(Expr::Literal(ScalarValue::Null(DataType::I64))),
                    negated: true,
                },
            ),
            (
                "NOT BETWEEN 10 AND 5 (inverted bounds)",
                Expr::Between {
                    expr: Box::new(col(tok_idx)),
                    low: Box::new(Expr::Literal(ScalarValue::I64(10))),
                    high: Box::new(Expr::Literal(ScalarValue::I64(5))),
                    negated: true,
                },
            ),
            (
                "LIKE escaped prefix 'foo\\%bar%' ESCAPE '\\'",
                Expr::Like {
                    expr: Box::new(col(name_idx)),
                    pattern: Box::new(Expr::Literal(ScalarValue::Utf8("foo\\%bar%".to_owned()))),
                    escape: Some('\\'),
                    case_insensitive: false,
                    negated: false,
                },
            ),
            (
                "ExternalPayload Eq never dropped",
                Expr::Binary {
                    left: Box::new(col(in_idx)),
                    op: BinaryOp::Eq,
                    right: Box::new(Expr::Literal(ScalarValue::Utf8(exact_payload.clone()))),
                    data_type: DataType::Bool,
                },
            ),
            (
                "Or(status='error', ExternalPayload Eq) never dropped",
                Expr::Binary {
                    left: Box::new(Expr::Binary {
                        left: Box::new(col(status_idx)),
                        op: BinaryOp::Eq,
                        right: Box::new(Expr::Literal(ScalarValue::Utf8("error".to_owned()))),
                        data_type: DataType::Bool,
                    }),
                    op: BinaryOp::Or,
                    right: Box::new(Expr::Binary {
                        left: Box::new(col(in_idx)),
                        op: BinaryOp::Eq,
                        right: Box::new(Expr::Literal(ScalarValue::Utf8(exact_payload))),
                        data_type: DataType::Bool,
                    }),
                    data_type: DataType::Bool,
                },
            ),
        ];

        let source = StatamSource::new(
            StorageIo::from_store(store),
            FileTable::Spans,
            Arc::clone(&schema),
            vec![file],
            Arc::from([]),
            Arc::new(MetadataCache::new(4 * 1024 * 1024)),
            Suspects::new(Arc::new(VerifyQueue::default())),
            tokio::runtime::Handle::current(),
        );
        let ctx = make_exec_ctx(64 * 1024 * 1024, CancelToken::new());
        let eval_ctx = EvalContext::new(0, CancelToken::new());

        for (label, expr) in test_exprs {
            // Oracle: unpushed `cetra_query::eval_filter` directly on the full batch.
            let mask = cetra_query::eval_filter(&expr, &batch, &eval_ctx, None).unwrap();
            let mut expected_starts: Vec<i64> = mask
                .iter_ones()
                .map(|r| {
                    let ScalarValue::TimestampNs { value, .. } =
                        batch.column(start_idx).unwrap().get_scalar(r).unwrap()
                    else {
                        panic!("expected TimestampNs");
                    };
                    value
                })
                .collect();
            expected_starts.sort_unstable();

            let spec = ScanSpec {
                projection: Some(vec![start_idx]),
                filters: vec![expr],
                ..ScanSpec::default()
            };
            let got_batches = drain_source(&source, &spec, &ctx).unwrap();
            let mut got_starts = Vec::new();
            for b in got_batches {
                let idxs = b.selection().to_indices(b.raw_len()).unwrap();
                for r in idxs {
                    let ScalarValue::TimestampNs { value, .. } =
                        b.column(0).unwrap().get_scalar(r as usize).unwrap()
                    else {
                        panic!("expected TimestampNs");
                    };
                    got_starts.push(value);
                }
            }
            got_starts.sort_unstable();
            assert_eq!(got_starts, expected_starts, "parity mismatch for: {label}");
        }

        // Filtered `LIMIT` unlocked when exact-pushed (`environment = 'prod' LIMIT 2`).
        let limited_spec = ScanSpec {
            projection: Some(vec![env_idx]),
            filters: vec![Expr::Binary {
                left: Box::new(col(env_idx)),
                op: BinaryOp::Eq,
                right: Box::new(Expr::Literal(ScalarValue::Utf8("prod".to_owned()))),
                data_type: DataType::Bool,
            }],
            limit: Some(2),
            ..ScanSpec::default()
        };
        let limited = drain_source(&source, &limited_spec, &ctx).unwrap();
        assert_eq!(
            limited.iter().map(Batch::num_rows).sum::<usize>(),
            2,
            "filtered exact-pushed LIMIT must return exactly 2 matching rows"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn multi_rg_partitioning_and_single_rg_worker_pool_executor() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let spans_vec: Vec<_> = (0..64_u16)
            .map(|i| span_row(i + 1, 0, ns(5, 10 + i64::from(i)), ns(5, 11 + i64::from(i))))
            .collect();
        let batch = span_rows(&spans_vec);
        let schema = FileTable::Spans.column_schema();

        // Write a 4-RG Statam file (`row_group_size = 16`, `mini_block_size = 8`, 64 rows -> 4 RGs).
        let mut opts = cetra_statam::WriteOptions::for_table(
            cetra_statam::TableKind::Spans,
            cetra_statam::WriteProfile::L1Compact,
        );
        opts.sort_cols = crate::statam_io::sort_col_indices(FileTable::Spans);
        opts.row_group_size = 16;
        opts.mini_block_size = 8;
        let mut writer = cetra_statam::StatamWriter::new(
            Cursor::new(Vec::new()),
            Arc::clone(&schema),
            opts,
            None,
        )
        .unwrap();
        writer.write_batch(&batch).unwrap();
        let (cursor, stats, _) = writer.finish_into_inner().unwrap();
        let bytes = Bytes::from(cursor.into_inner());
        let (path, id) =
            crate::objects::Objects::table_file_path(FileTable::Spans, project(), day(5), 1);
        store
            .put(
                &ObjectPath::from(path.as_str()),
                PutPayload::from(bytes.clone()),
            )
            .await
            .unwrap();
        let meta = Arc::new(
            StatamFileMeta::read_from_with_hint(
                &SliceReader::new(bytes.clone()),
                Some(u64::from(stats.hot_tail_bytes)),
            )
            .unwrap(),
        );
        assert_eq!(meta.rg_dir.len(), 4, "expected 4 Row Groups");

        let file = Arc::new(FileMeta {
            id,
            table: FileTable::Spans,
            project: project(),
            day: day(5),
            level: 1,
            path,
            size: bytes.len() as u64,
            footer_size: u64::from(stats.hot_tail_bytes),
            rows: 64,
            min_time: ns(5, 10),
            max_time: ns(5, 73),
            min_ingest_time: ns(5, 11),
            max_ingest_time: ns(5, 74),
            fully_merged: true,
            deletions_applied_seq: 0,
            pack: None,
        });

        let cache = Arc::new(MetadataCache::new(4 * 1024 * 1024));
        cache.insert_statam(file.id, meta);

        let source = StatamSource::new(
            StorageIo::from_store(Arc::clone(&store)),
            FileTable::Spans,
            Arc::clone(&schema),
            vec![file],
            Arc::from([]),
            cache,
            Suspects::new(Arc::new(VerifyQueue::default())),
            tokio::runtime::Handle::current(),
        );

        let workers = Arc::new(WorkerPool::new(4).unwrap());
        let multi_ctx = ExecContext {
            pool: GlobalPool::standalone(64 * 1024 * 1024),
            eval: EvalContext::new(0, CancelToken::new()),
            spill: Arc::new(SpillManager::with_fixed_cap(PathBuf::new(), 0)),
            workers: Arc::clone(&workers),
            batch_size: 8192,
            target_partitions: 4,
            metrics: ExecMetrics::new(),
        };

        // 1. Multi-RG cached file splits into 4 parallel partition streams.
        let spec = ScanSpec {
            projection: Some(vec![schema.index_of(spans::START_TIME).unwrap()]),
            ..ScanSpec::default()
        };
        let streams = source.open_partitions(&spec, &multi_ctx).unwrap();
        assert_eq!(
            streams.len(),
            4,
            "4-RG file with target_partitions=4 must split into 4 streams"
        );
        drop(streams);
        let batches = drain_source(&source, &spec, &multi_ctx).unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 64);

        // 2. 1-RG file with `target_partitions = 4` produces 1 stream that uses `WorkerPoolExecutor`
        //    on `ctx.workers` (testing `WorkerPool::run_helping` from inside a pool worker without deadlock).
        let one_rg_file = put_statam_file(&store, FileTable::Spans, 1, day(5), &batch).await;
        let one_rg_source = StatamSource::new(
            StorageIo::from_store(store),
            FileTable::Spans,
            Arc::clone(&schema),
            vec![one_rg_file],
            Arc::from([]),
            Arc::new(MetadataCache::new(4 * 1024 * 1024)),
            Suspects::new(Arc::new(VerifyQueue::default())),
            tokio::runtime::Handle::current(),
        );
        let streams_1rg = one_rg_source.open_partitions(&spec, &multi_ctx).unwrap();
        assert_eq!(streams_1rg.len(), 1);
        drop(streams_1rg);
        let batches_1rg = drain_source(&one_rg_source, &spec, &multi_ctx).unwrap();
        assert_eq!(batches_1rg.iter().map(Batch::num_rows).sum::<usize>(), 64);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zero_io_count_uses_zero_store_gets_and_respects_bounds_limit_and_tombstones() {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let b1 = span_rows(&[
            span_row(1, 0, ns(5, 10), ns(5, 11)),
            span_row(2, 0, ns(5, 20), ns(5, 21)),
            span_row(3, 0, ns(5, 30), ns(5, 31)),
        ]);
        let b2 = span_rows(&[
            span_row(4, 0, ns(6, 10), ns(6, 11)),
            span_row(5, 0, ns(6, 20), ns(6, 21)),
        ]);
        let f1 = put_statam_file(&inner, FileTable::Spans, 1, day(5), &b1).await;
        let f2 = put_statam_file(&inner, FileTable::Spans, 1, day(6), &b2).await;

        let chaos = Arc::new(ChaosStore::new(Arc::clone(&inner)));
        // Hold ALL `Get` calls: any disk/S3 read will immediately fail!
        chaos.hold(&[Call::Get]);

        let schema = FileTable::Spans.column_schema();
        let start_idx = schema.index_of(spans::START_TIME).unwrap();
        let source = StatamSource::new(
            StorageIo::from_store(Arc::clone(&chaos) as Arc<dyn ObjectStore>),
            FileTable::Spans,
            Arc::clone(&schema),
            vec![Arc::clone(&f1), Arc::clone(&f2)],
            Arc::from([]),
            Arc::new(MetadataCache::new(0)),
            Suspects::new(Arc::new(VerifyQueue::default())),
            tokio::runtime::Handle::current(),
        );
        let ctx = make_exec_ctx(64 * 1024 * 1024, CancelToken::new());

        // 1. Unfiltered COUNT(*) -> 5 rows, 0 GET calls!
        let spec_all = ScanSpec {
            projection: Some(vec![]),
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &spec_all, &ctx).unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 5);
        assert_eq!(
            chaos.count(Call::Get),
            0,
            "unfiltered COUNT(*) must do 0 GETs"
        );

        // 2. `visible_from`-bounded COUNT(*) covering both files -> 5 rows, 0 GET calls!
        let spec_vf =
            crate::engine::apply_visible_from(&spec_all, &schema, spans::START_TIME, ns(5, 10))
                .unwrap();
        let batches = drain_source(&source, &spec_vf, &ctx).unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 5);
        assert_eq!(
            chaos.count(Call::Get),
            0,
            "visible_from COUNT(*) covering files must do 0 GETs"
        );

        // 3. Window-aligned COUNT(*) covering f1 completely and pruning f2 -> 3 rows, 0 GETs!
        let spec_win = ScanSpec {
            projection: Some(vec![]),
            time_range: TimeRangeHint {
                min_ns: Some(ns(5, 0)),
                max_ns: Some(ns(5, 100)),
                around_ns: None,
            },
            filters: vec![
                Expr::Binary {
                    left: Box::new(Expr::Column {
                        index: start_idx,
                        name: spans::START_TIME.to_owned(),
                        data_type: DataType::TimestampNs { utc: true },
                    }),
                    op: BinaryOp::GtEq,
                    right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                        value: ns(5, 0),
                        utc: true,
                    })),
                    data_type: DataType::Bool,
                },
                Expr::Binary {
                    left: Box::new(Expr::Column {
                        index: start_idx,
                        name: spans::START_TIME.to_owned(),
                        data_type: DataType::TimestampNs { utc: true },
                    }),
                    op: BinaryOp::Lt,
                    right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                        value: ns(5, 100),
                        utc: true,
                    })),
                    data_type: DataType::Bool,
                },
            ],
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &spec_win, &ctx).unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 3);
        assert_eq!(
            chaos.count(Call::Get),
            0,
            "window-aligned COUNT(*) must do 0 GETs"
        );

        // 4. `limit`-capped COUNT(*) -> 4 rows, 0 GET calls!
        let spec_lim = ScanSpec {
            projection: Some(vec![]),
            limit: Some(4),
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &spec_lim, &ctx).unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 4);
        assert_eq!(
            chaos.count(Call::Get),
            0,
            "limit-capped COUNT(*) must do 0 GETs"
        );

        // 5. Partial-overlap window (`start_time >= ns(5, 20)` splits f1) MUST fall back to I/O.
        chaos.release();
        let spec_partial = ScanSpec {
            projection: Some(vec![]),
            time_range: TimeRangeHint {
                min_ns: Some(ns(5, 20)),
                max_ns: Some(ns(5, 100)),
                around_ns: None,
            },
            filters: vec![Expr::Binary {
                left: Box::new(Expr::Column {
                    index: start_idx,
                    name: spans::START_TIME.to_owned(),
                    data_type: DataType::TimestampNs { utc: true },
                }),
                op: BinaryOp::GtEq,
                right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                    value: ns(5, 20),
                    utc: true,
                })),
                data_type: DataType::Bool,
            }],
            ..ScanSpec::default()
        };
        let batches = drain_source(&source, &spec_partial, &ctx).unwrap();
        assert_eq!(batches.iter().map(Batch::num_rows).sum::<usize>(), 2);
        assert!(
            chaos.count(Call::Get) > 0,
            "partial-overlap window must read file"
        );

        // 6. Active tombstone MUST disable Zero-I/O COUNT(*) and filter out deleted trace.
        let tid1 = *span_row(1, 0, 0, 0).trace_id.as_bytes();
        let del = Arc::new(PendingDeletion {
            id: Uuid::now_v7(),
            seq: 1,
            project: project(),
            cutoff: ns(7, 0),
            registered_at: ns(7, 0),
            trace_ids: std::collections::HashSet::from([tid1]),
            users: std::collections::HashSet::new(),
            sessions: std::collections::HashSet::new(),
        });
        let tomb_source = StatamSource::new(
            StorageIo::from_store(Arc::clone(&inner)),
            FileTable::Spans,
            Arc::clone(&schema),
            vec![f1, f2],
            Arc::from([del]),
            Arc::new(MetadataCache::new(0)),
            Suspects::new(Arc::new(VerifyQueue::default())),
            tokio::runtime::Handle::current(),
        );
        let batches = drain_source(&tomb_source, &spec_all, &ctx).unwrap();
        assert_eq!(
            batches.iter().map(Batch::num_rows).sum::<usize>(),
            4,
            "tombstone must exclude deleted trace from COUNT(*)"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_with_row_coords_and_take_rows_with_schema_evolution_and_oom() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let mut s1 = span_row(1, 0, ns(5, 10), ns(5, 11));
        s1.name = "first".to_owned();
        s1.end_time = s1.start_time + 500;
        let mut s2 = span_row(2, 0, ns(5, 20), ns(5, 21));
        s2.name = "second".to_owned();
        s2.end_time = s2.start_time + 900;
        let mut s3 = span_row(3, 0, ns(6, 10), ns(6, 11));
        s3.name = "third".to_owned();
        s3.end_time = s3.start_time + 700;

        let f1 = put_statam_file(&store, FileTable::Spans, 1, day(5), &span_rows(&[s1, s2])).await;
        let f2 = put_statam_file(&store, FileTable::Spans, 1, day(6), &span_rows(&[s3])).await;

        let mut evolved_fields = FileTable::Spans.column_schema().fields().to_vec();
        evolved_fields.push(Field::new("evolved_note", DataType::Utf8, true));
        let evolved_schema = Schema::try_new_arc(evolved_fields).unwrap();
        let dur_idx = evolved_schema.index_of(spans::DURATION_NS).unwrap();
        let name_idx = evolved_schema.index_of(spans::NAME).unwrap();
        let note_idx = evolved_schema.index_of("evolved_note").unwrap();

        let source = StatamSource::new(
            StorageIo::from_store(store),
            FileTable::Spans,
            Arc::clone(&evolved_schema),
            vec![f1, f2],
            Arc::from([]),
            Arc::new(MetadataCache::new(4 * 1024 * 1024)),
            Suspects::new(Arc::new(VerifyQueue::default())),
            tokio::runtime::Handle::current(),
        );
        let ctx = make_exec_ctx(64 * 1024 * 1024, CancelToken::new());

        let spec = ScanSpec {
            projection: Some(vec![dur_idx]),
            ..ScanSpec::default()
        };
        tokio::task::spawn_blocking(move || {
            let (streams, coord_schema) = source.open_with_row_coords(&spec, &ctx).unwrap();
            assert_eq!(coord_schema.len(), 3);
            assert_eq!(coord_schema.fields()[1].name, ROW_COORD_FILE_COL);
            assert_eq!(coord_schema.fields()[2].name, ROW_COORD_ROW_COL);

            let mut winners: Vec<(i64, u32, u64)> = Vec::new();
            let mut res = ctx.pool.register("test_coords", false);
            for mut st in streams {
                while let Some(b) = st.next_batch(&ctx.eval.cancel, Some(&mut res)).unwrap() {
                    let idxs = b.selection().to_indices(b.raw_len()).unwrap();
                    for r in idxs {
                        let ScalarValue::I64(dur) =
                            b.column(0).unwrap().get_scalar(r as usize).unwrap()
                        else {
                            panic!("expected I64 dur");
                        };
                        let ScalarValue::I64(fo) =
                            b.column(1).unwrap().get_scalar(r as usize).unwrap()
                        else {
                            panic!("expected I64 file_ord");
                        };
                        let ScalarValue::I64(ri) =
                            b.column(2).unwrap().get_scalar(r as usize).unwrap()
                        else {
                            panic!("expected I64 row_idx");
                        };
                        winners.push((dur, fo as u32, ri as u64));
                    }
                }
                res.free();
            }
            winners.sort_by_key(|a| std::cmp::Reverse(a.0));
            let top2_coords: Vec<(u32, u64)> =
                winners.iter().take(2).map(|&(_, f, r)| (f, r)).collect();

            let mut take_res = ctx.pool.register("test_take", false);
            let batches = source
                .take_rows(
                    &top2_coords,
                    &[name_idx, dur_idx, note_idx],
                    &ctx,
                    &mut take_res,
                )
                .unwrap();
            let mut names = Vec::new();
            for b in &batches {
                assert_eq!(b.schema().len(), 3);
                for r in b.selection().to_indices(b.raw_len()).unwrap() {
                    let ScalarValue::Utf8(n) = b.column(0).unwrap().get_scalar(r as usize).unwrap()
                    else {
                        panic!("expected Utf8");
                    };
                    assert!(matches!(
                        b.column(2).unwrap().get_scalar(r as usize).unwrap(),
                        ScalarValue::Null(DataType::Utf8)
                    ));
                    names.push(n);
                }
            }
            names.sort();
            assert_eq!(names, vec!["second".to_owned(), "third".to_owned()]);
            drop(batches);
            take_res.free();

            // OOM on take_rows rolls back reservation to 0.
            let tiny_ctx = make_exec_ctx(16, CancelToken::new());
            let mut tiny_res = tiny_ctx.pool.register("tiny_take", false);
            let err = source
                .take_rows(&top2_coords, &[name_idx, dur_idx], &tiny_ctx, &mut tiny_res)
                .unwrap_err();
            assert!(matches!(err, QueryError::TooLarge), "got {err:?}");
            assert_eq!(tiny_res.size(), 0);
            assert_eq!(tiny_ctx.pool.reserved(), 0);
        })
        .await
        .unwrap();
    }
}
