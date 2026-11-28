//! Native spillable `NativeTraceMerger` and `TracesSource` (`mode=lookup`, `mode=list`,
//! `mode=range`, and generalized `TwoPhase` rank→fetch for non-time sorts).

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use cetra_column::{
    Batch, Bitmap, CancelToken, Column, ColumnError, DataType, FlatColumn, FlatData, MapData,
    Reservation, ScalarValue, Schema, Selection, Utf8Data,
};
use cetra_core::enums::SpanStatus;
use cetra_core::ids::ProjectId;
use cetra_core::schema::traces;
use cetra_query::exec::sort_window::ExternalSort;
use cetra_query::exec::streaming::TopKSink;
use cetra_query::exec::{
    ControlFlow, ExecContext, IdLookupHint, PartitionStream, PushSink, ScanSource, ScanSpec,
    SortKey, TimeRangeHint, run_pipeline,
};
use cetra_query::{BinaryOp, EvalContext, Expr, QueryError, eval_filter};
use chrono::NaiveDate;

use crate::catalog::{FileMeta, FileTable};
use crate::deletion::{PendingDeletion, ProjectMemSnapshot, applicable};
use crate::memtable::{MemBatch, MemSnapshot};
use crate::objects::StorageIo;
use crate::partials::MergeColumns;
use crate::verify::Suspects;

use super::metadata_cache::MetadataCache;
use super::statam_source::StatamSource;

use super::{
    BufferedBatchStream, DAY_NS, HashPartitionedSortSink, TimeBounds, apply_tombstones_and_filters,
    cap_partitions, claim_shared_limit, day_of_ns, extract_time_bounds, handoff_res, read_bool_at,
    read_fixed16_at, read_str_at, reconcile_res, rollback_res, transfer_or_grow,
};

const MERGE_CHUNK_ROWS: usize = 1024;
const USD_UNITS: f64 = 1e18;

const MIN15: i64 = 15 * 60 * 1_000_000_000;
const HOUR1: i64 = 60 * 60 * 1_000_000_000;
const HOUR6: i64 = 6 * HOUR1;
const HOUR24: i64 = 24 * HOUR1;

/// Execution mode override for [`TracesSource`].
#[derive(Debug, Clone)]
pub(crate) enum TraceListMode {
    /// Widening-window 5-slice fast path (`15m / 1h / 6h / 24h / whole-range`)
    /// returning at most `fetch` traces ordered by `(start_time DESC, trace_id DESC)`.
    Widening {
        /// Maximum traces to return.
        fetch: usize,
    },
    /// Two-phase rank→fetch for non-time sorts (`cost_total`, `duration_ns`, `total_tokens`, etc.).
    TwoPhase {
        /// Sort keys bound against `TracesSource::schema()`.
        sort_keys: Vec<SortKey>,
        /// Maximum traces to emit after `offset`.
        limit: usize,
        /// Number of leading ranked traces to skip before Phase 2 fetch.
        offset: usize,
    },
}

/// Native [`ScanSource`] for the `traces` view over `traces` Statam files and
/// pre-filtered `spans` memtable partials.
#[derive(Clone)]
pub(crate) struct TracesSource {
    io: StorageIo,
    project: ProjectId,
    schema: Arc<Schema>,
    files: Vec<Arc<FileMeta>>,
    spans_mem: ProjectMemSnapshot,
    lookback: i64,
    deletions: Arc<[Arc<PendingDeletion>]>,
    metadata_cache: Arc<MetadataCache>,
    suspects: Suspects,
    runtime: tokio::runtime::Handle,
    list_mode: Option<TraceListMode>,
    visible_from: Option<i64>,
}

impl std::fmt::Debug for TracesSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mode = match &self.list_mode {
            Some(TraceListMode::Widening { .. }) => "widening",
            Some(TraceListMode::TwoPhase { .. }) => "two_phase",
            None => "range",
        };
        let mem_batches: usize = self
            .spans_mem
            .as_snapshot()
            .partitions
            .iter()
            .filter(|(key, _)| key.project == self.project)
            .map(|(_, batches)| batches.len())
            .sum();
        f.debug_struct("TracesSource")
            .field("mode", &mode)
            .field("files", &self.files.len())
            .field("mem_batches", &mem_batches)
            .field("deletions", &self.deletions.len())
            .field("lookback", &self.lookback)
            .field("visible_from", &self.visible_from)
            .field("schema_len", &self.schema.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
struct RangeScanArgs<'a> {
    bounds: TimeBounds,
    around_ns: Option<i64>,
    trace_ids: Option<&'a BTreeSet<[u8; 16]>>,
    merge_columns: &'a MergeColumns,
    post_filters_full: &'a [Expr],
    final_projection: Option<&'a [usize]>,
    limit: Option<usize>,
}

impl TracesSource {
    /// Creates a [`TracesSource`] over `files` and pre-filtered `spans_mem` for `project`.
    ///
    /// `spans_mem` is enforced at compile time via [`ProjectMemSnapshot`] to have been projected
    /// by `deletion::project_snapshot(Table::Spans, ...)` (`QE-M16`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        io: StorageIo,
        project: ProjectId,
        files: Vec<Arc<FileMeta>>,
        spans_mem: ProjectMemSnapshot,
        lookback: i64,
        deletions: Arc<[Arc<PendingDeletion>]>,
        metadata_cache: Arc<MetadataCache>,
        suspects: Suspects,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, QueryError> {
        let schema = FileTable::Traces.column_schema();
        Ok(Self {
            io,
            project,
            schema,
            files,
            spans_mem,
            lookback,
            deletions,
            metadata_cache,
            suspects,
            runtime,
            list_mode: None,
            visible_from: None,
        })
    }

    /// Configures a widening-window or two-phase rank→fetch list mode on this source.
    pub(crate) fn with_list_mode(mut self, mode: TraceListMode) -> Self {
        self.list_mode = Some(mode);
        self
    }

    /// Sets an inclusive `start_time >= visible_from` visibility floor enforced on every scan.
    pub(crate) fn with_visible_from(mut self, visible_from: Option<i64>) -> Self {
        self.visible_from = visible_from;
        self
    }

    fn effective_bounds(&self, spec: &ScanSpec) -> TimeBounds {
        let hint = TimeBounds {
            min: spec.time_range.min_ns,
            max: spec.time_range.max_ns,
        };
        let from_filters = extract_time_bounds(&spec.filters, traces::START_TIME);
        hint.intersect_with(&from_filters)
    }

    fn effective_trace_ids(&self, spec: &ScanSpec) -> Option<BTreeSet<[u8; 16]>> {
        let mut found: Option<BTreeSet<[u8; 16]>> = None;
        for hint in &spec.id_lookups {
            if let IdLookupHint::Fixed16 { column, values } = hint
                && column == traces::TRACE_ID
            {
                let set: BTreeSet<[u8; 16]> = values.iter().copied().collect();
                found = Some(match found {
                    None => set,
                    Some(prev) => prev.intersection(&set).copied().collect(),
                });
            }
        }
        if let Some(filter_ids) = lookup_trace_ids_in_filters(&spec.filters) {
            found = Some(match found {
                None => filter_ids,
                Some(prev) => prev.intersection(&filter_ids).copied().collect(),
            });
        }
        found
    }

    fn data_bounds(&self, bounds: TimeBounds) -> Option<(i64, i64)> {
        let active_files: Vec<&Arc<FileMeta>> = self.files.iter().filter(|f| f.rows > 0).collect();
        let mut mem_batches: Vec<&MemBatch> = Vec::new();
        for (key, batches) in &self.spans_mem.as_snapshot().partitions {
            if key.project == self.project {
                for b in batches {
                    if b.batch.num_rows() > 0 {
                        mem_batches.push(b);
                    }
                }
            }
        }
        if active_files.is_empty() && mem_batches.is_empty() {
            return None;
        }
        let min = active_files
            .iter()
            .map(|f| f.min_time)
            .chain(mem_batches.iter().map(|b| b.min_time))
            .min()?;
        let max = active_files
            .iter()
            .map(|f| f.max_time)
            .chain(mem_batches.iter().map(|b| b.max_time))
            .max()?;
        Some((bounds.min.unwrap_or(min), bounds.max.unwrap_or(max)))
    }

    fn open_range_streams(
        &self,
        args: RangeScanArgs<'_>,
        ctx: &ExecContext,
    ) -> Result<Vec<Box<dyn PartitionStream>>, QueryError> {
        let RangeScanArgs {
            bounds,
            around_ns,
            trace_ids,
            merge_columns,
            post_filters_full,
            final_projection,
            limit,
        } = args;
        if bounds.contradicts() || limit == Some(0) {
            return Ok(Vec::new());
        }
        if let Some(ids) = &trace_ids
            && ids.is_empty()
        {
            return Ok(Vec::new());
        }

        let padded = padded_bounds(bounds, self.lookback);
        let input_names = merge_columns.input();
        let output_names = merge_columns.output();
        let input_indices = indices_of_names(&self.schema, &input_names)?;
        let output_indices = indices_of_names(&self.schema, &output_names)?;
        let input_schema = Arc::new(self.schema.project(&input_indices)?);
        let merged_schema = Arc::new(self.schema.project(&output_indices)?);

        // Push ONLY padded `start_time` bounds and `trace_id` lookups to partial scans.
        let mut full_to_input: Vec<Option<usize>> = vec![None; self.schema.len()];
        for (pos, &full_idx) in input_indices.iter().enumerate() {
            full_to_input[full_idx] = Some(pos);
        }
        let partial_filters_full = build_partial_filters(&self.schema, padded, trace_ids)?;
        let partial_filters_input: Vec<Expr> = partial_filters_full
            .iter()
            .map(|f| f.remap_columns(&full_to_input))
            .collect::<Result<Vec<_>, _>>()?;

        let lookup_without_bounds =
            trace_ids.is_some() && bounds.min.is_none() && bounds.max.is_none();
        let effective_around = if lookup_without_bounds {
            around_ns
        } else {
            None
        };

        let mut id_lookups = Vec::new();
        if let Some(ids) = &trace_ids {
            id_lookups.push(IdLookupHint::Fixed16 {
                column: traces::TRACE_ID.to_owned(),
                values: ids.iter().copied().collect(),
            });
        }

        let file_spec = ScanSpec {
            projection: Some(input_indices.clone()),
            map_projections: BTreeMap::new(),
            time_range: TimeRangeHint {
                min_ns: padded.min,
                max_ns: padded.max,
                around_ns: effective_around,
            },
            id_lookups: id_lookups.clone(),
            filters: partial_filters_full,
            substrings: Vec::new(),
            sort_hints: Vec::new(),
            limit: None,
        };

        // Prune `traces` files and group gated files by their exact pending `Vec<u64>` seqs.
        let surviving_files = prune_trace_files(&self.files, padded, effective_around);
        let mut clean_files: Vec<Arc<FileMeta>> = Vec::new();
        type GatedGroup = (Vec<Arc<PendingDeletion>>, Vec<Arc<FileMeta>>);
        let mut gated: BTreeMap<Vec<u64>, GatedGroup> = BTreeMap::new();
        for file in surviving_files {
            let pending = applicable(&self.deletions, Some(file.deletions_applied_seq), i64::MIN);
            if pending.is_empty() {
                clean_files.push(file);
            } else {
                let key: Vec<u64> = pending.iter().map(|d| d.seq).collect();
                gated
                    .entry(key)
                    .or_insert_with(|| (pending, Vec::new()))
                    .1
                    .push(file);
            }
        }

        let mut partial_streams: Vec<Box<dyn PartitionStream>> = Vec::new();
        if !clean_files.is_empty() {
            let statam_src = Arc::new(StatamSource::new(
                self.io.clone(),
                FileTable::Traces,
                Arc::clone(&self.schema),
                clean_files,
                Arc::from([]),
                Arc::clone(&self.metadata_cache),
                self.suspects.clone(),
                self.runtime.clone(),
            ));
            partial_streams.extend(statam_src.open_partitions(&file_spec, ctx)?);
        }
        for (pending, group_files) in gated.into_values() {
            let statam_src = Arc::new(StatamSource::new(
                self.io.clone(),
                FileTable::Traces,
                Arc::clone(&self.schema),
                group_files,
                Arc::from(pending),
                Arc::clone(&self.metadata_cache),
                self.suspects.clone(),
                self.runtime.clone(),
            ));
            partial_streams.extend(statam_src.open_partitions(&file_spec, ctx)?);
        }

        // Lazily stream memtable spans into cached partials (projected to `input_indices`).
        let mem_batches = select_mem_batches(self.spans_mem.as_snapshot(), self.project, padded);
        if !mem_batches.is_empty() {
            let slots = ctx.target_partitions.max(1).min(mem_batches.len());
            let mut grouped: Vec<VecDeque<MemBatch>> =
                (0..slots).map(|_| VecDeque::new()).collect();
            for (idx, mem_batch) in mem_batches.into_iter().enumerate() {
                grouped[idx % slots].push_back(mem_batch);
            }
            for batches in grouped {
                if batches.is_empty() {
                    continue;
                }
                partial_streams.push(Box::new(MemPartialsStream {
                    batches,
                    input_indices: input_indices.clone(),
                    filters: partial_filters_input.clone(),
                    eval: ctx.eval.clone(),
                }));
            }
        }

        if partial_streams.is_empty() {
            return Ok(Vec::new());
        }

        let target_partitions = ctx.target_partitions.max(1);
        let capped = cap_partitions(partial_streams, target_partitions);
        let trace_id_input_idx = input_schema
            .index_of(traces::TRACE_ID)
            .ok_or_else(|| QueryError::internal("trace_id missing from merge input schema"))?;
        let sort_keys = vec![SortKey {
            expr: Expr::Column {
                index: trace_id_input_idx,
                name: traces::TRACE_ID.to_owned(),
                data_type: DataType::FixedBytes16,
            },
            descending: false,
            nulls_first: false,
        }];

        let sorted_streams: Vec<Box<dyn PartitionStream>> = if target_partitions == 1 {
            let sort_schema = Arc::clone(&input_schema);
            let sort_keys_for_sink = sort_keys.clone();
            let ctx_for_sink = ctx.clone();
            let sinks = run_pipeline(
                ctx,
                capped,
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
                input_schema,
                sort_keys,
                None,
                ctx,
            )?]
        } else {
            let part_schema = Arc::clone(&input_schema);
            let ctx_for_sink = ctx.clone();
            let sinks = run_pipeline(
                ctx,
                capped,
                |_| Ok(Vec::new()),
                move |_| {
                    Ok(HashPartitionedSortSink::new(
                        Arc::clone(&part_schema),
                        trace_id_input_idx,
                        target_partitions,
                        ctx_for_sink.clone(),
                    ))
                },
            )?;
            HashPartitionedSortSink::into_partition_streams(sinks, &input_schema, &sort_keys, ctx)?
        };

        // Prepare post-merge filter remapping and final projection indices.
        let mut full_to_merged: Vec<Option<usize>> = vec![None; self.schema.len()];
        for (merged_pos, &full_idx) in output_indices.iter().enumerate() {
            full_to_merged[full_idx] = Some(merged_pos);
        }
        let post_filters: Vec<Expr> = post_filters_full
            .iter()
            .map(|f| f.remap_columns(&full_to_merged))
            .collect::<Result<Vec<_>, _>>()?;

        let merged_to_output: Option<Vec<usize>> = match final_projection {
            Some(indices) => {
                let mut out = Vec::with_capacity(indices.len());
                for &full_idx in indices {
                    let pos = full_to_merged[full_idx].ok_or_else(|| {
                        QueryError::internal("projected column missing from merged schema")
                    })?;
                    out.push(pos);
                }
                Some(out)
            }
            None => {
                // `output_indices` is already `0..self.schema.len()` in schema order.
                None
            }
        };

        let merged_start_idx = merged_schema
            .index_of(traces::START_TIME)
            .ok_or_else(|| QueryError::internal("start_time missing from merged schema"))?;

        let active_p = sorted_streams.len().max(1);
        let budget_bytes = (ctx.pool.spill_share() / (4 * active_p)).max(4096);
        let max_traces_per_chunk = (ctx.pool.cap() / (4096 * active_p)).clamp(8, MERGE_CHUNK_ROWS);
        let shared_limit = limit.map(|l| Arc::new(AtomicUsize::new(l)));

        let streams = sorted_streams
            .into_iter()
            .map(|input| {
                Ok(Box::new(NativeTraceMergeStream {
                    input,
                    input_res: ctx.pool.register("trace_merge_in", false),
                    merger: NativeTraceMerger::new(merge_columns, &self.schema)?,
                    merger_res: ctx.pool.register("trace_merger", false),
                    scratch_res: ctx.pool.register("trace_merge_scratch", false),
                    merge_columns: merge_columns.clone(),
                    full_schema: Arc::clone(&self.schema),
                    merged_start_idx,
                    bounds,
                    post_filters: post_filters.clone(),
                    projection: merged_to_output.clone(),
                    remaining: shared_limit.clone(),
                    eval: ctx.eval.clone(),
                    active_input: None,
                    current_trace_id: None,
                    budget_bytes,
                    max_traces_per_chunk,
                    done: false,
                }) as Box<dyn PartitionStream>)
            })
            .collect::<Result<Vec<_>, QueryError>>()?;
        Ok(streams)
    }

    fn open_widening_list(
        &self,
        spec: &ScanSpec,
        fetch: usize,
        bounds: TimeBounds,
        trace_ids: Option<&BTreeSet<[u8; 16]>>,
        ctx: &ExecContext,
    ) -> Result<Vec<Box<dyn PartitionStream>>, QueryError> {
        let effective_fetch = spec.limit.map_or(fetch, |l| l.min(fetch));
        if effective_fetch == 0 || bounds.contradicts() {
            return Ok(Vec::new());
        }
        if let Some(ids) = &trace_ids
            && ids.is_empty()
        {
            return Ok(Vec::new());
        }

        let Some((a, b)) = self.data_bounds(bounds) else {
            return Ok(Vec::new());
        };
        if a > b {
            return Ok(Vec::new());
        }
        let windows = widening_slices(a, b);

        let filter_names = filter_col_names(&self.schema, &spec.filters);
        let phase1_cols = MergeColumns::new(&str_refs(&filter_names));
        let phase1_indices = indices_of_names(&self.schema, &phase1_cols.output())?;
        let phase1_schema = Arc::new(self.schema.project(&phase1_indices)?);
        let p1_start_idx = phase1_schema
            .index_of(traces::START_TIME)
            .ok_or_else(|| QueryError::internal("phase 1 start_time missing"))?;
        let p1_trace_idx = phase1_schema
            .index_of(traces::TRACE_ID)
            .ok_or_else(|| QueryError::internal("phase 1 trace_id missing"))?;

        let list_sort_keys = vec![
            SortKey {
                expr: Expr::Column {
                    index: p1_start_idx,
                    name: traces::START_TIME.to_owned(),
                    data_type: DataType::timestamp_utc(),
                },
                descending: true,
                nulls_first: false,
            },
            SortKey {
                expr: Expr::Column {
                    index: p1_trace_idx,
                    name: traces::TRACE_ID.to_owned(),
                    data_type: DataType::FixedBytes16,
                },
                descending: true,
                nulls_first: false,
            },
        ];

        struct Candidate {
            start: i64,
            id: [u8; 16],
            slice: usize,
        }

        let mut p1_res = ctx.pool.register("widening_p1", false);
        let mut seen: BTreeSet<[u8; 16]> = BTreeSet::new();
        let mut candidates: Vec<Candidate> = Vec::new();
        let widening_p1_bytes = |seen: &BTreeSet<[u8; 16]>, candidates: &[Candidate]| -> usize {
            seen.len() * (size_of::<[u8; 16]>() + 16) + size_of_val(candidates)
        };

        for (slice_idx, window) in windows.iter().copied().enumerate() {
            if ctx.eval.cancel.is_cancelled() {
                p1_res.free();
                return Err(QueryError::Cancelled);
            }
            let streams = self.open_range_streams(
                RangeScanArgs {
                    bounds: window,
                    around_ns: None,
                    trace_ids,
                    merge_columns: &phase1_cols,
                    post_filters_full: &spec.filters,
                    final_projection: None,
                    limit: None,
                },
                ctx,
            )?;
            if streams.is_empty() {
                continue;
            }

            let sink_schema = Arc::clone(&phase1_schema);
            let sink_keys = list_sort_keys.clone();
            let sink_ctx = ctx.clone();
            let sinks = run_pipeline(
                ctx,
                streams,
                |_| Ok(Vec::new()),
                move |_| {
                    Ok(TopKSink::new(
                        Arc::clone(&sink_schema),
                        sink_keys.clone(),
                        effective_fetch,
                        0,
                        sink_ctx.clone(),
                    ))
                },
            )?;
            let batches = TopKSink::merge(
                sinks,
                Arc::clone(&phase1_schema),
                &list_sort_keys,
                0,
                effective_fetch,
                ctx,
            )?;

            for batch in batches {
                let start_col = batch
                    .column(p1_start_idx)
                    .ok_or_else(|| QueryError::internal("phase 1 start_time column missing"))?;
                let id_col = batch
                    .column(p1_trace_idx)
                    .ok_or_else(|| QueryError::internal("phase 1 trace_id column missing"))?;
                for_each_active_row(&batch, |row| {
                    let start = read_ts_at(start_col, row)
                        .ok_or_else(|| QueryError::internal("phase 1 start_time null"))?;
                    let id = read_fixed16_at(id_col, row)
                        .ok_or_else(|| QueryError::internal("phase 1 trace_id null"))?;
                    if seen.insert(id) {
                        candidates.push(Candidate {
                            start,
                            id,
                            slice: slice_idx,
                        });
                    }
                    Ok(())
                })?;
                reconcile_res(Some(&mut p1_res), 0, widening_p1_bytes(&seen, &candidates))?;
            }

            candidates.sort_by(|l, r| r.start.cmp(&l.start).then_with(|| r.id.cmp(&l.id)));
            candidates.truncate(effective_fetch);
            seen = candidates.iter().map(|c| c.id).collect();
            reconcile_res(Some(&mut p1_res), 0, widening_p1_bytes(&seen, &candidates))?;
            if candidates.len() >= effective_fetch {
                break;
            }
        }

        if candidates.is_empty() {
            p1_res.free();
            return Ok(Vec::new());
        }

        // Phase 2: fetch full requested columns per winning `slice` under exact padded bounds.
        let mut groups: BTreeMap<usize, BTreeSet<[u8; 16]>> = BTreeMap::new();
        for c in &candidates {
            groups.entry(c.slice).or_default().insert(c.id);
        }
        drop(candidates);
        drop(seen);
        let groups_bytes: usize = groups.len() * (size_of::<usize>() + 32)
            + groups
                .values()
                .map(|ids| ids.len() * (size_of::<[u8; 16]>() + 16))
                .sum::<usize>();
        reconcile_res(Some(&mut p1_res), 0, groups_bytes)?;

        let mut phase2_names = requested_col_names(&self.schema, spec.projection.as_deref())?;
        phase2_names.extend(filter_names);
        let phase2_cols = MergeColumns::new(&str_refs(&phase2_names));
        let phase2_indices = indices_of_names(&self.schema, &phase2_cols.output())?;
        let phase2_schema = Arc::new(self.schema.project(&phase2_indices)?);
        let p2_start_idx = phase2_schema
            .index_of(traces::START_TIME)
            .ok_or_else(|| QueryError::internal("phase 2 start_time missing"))?;
        let p2_trace_idx = phase2_schema
            .index_of(traces::TRACE_ID)
            .ok_or_else(|| QueryError::internal("phase 2 trace_id missing"))?;

        let p2_sort_keys = vec![
            SortKey {
                expr: Expr::Column {
                    index: p2_start_idx,
                    name: traces::START_TIME.to_owned(),
                    data_type: DataType::timestamp_utc(),
                },
                descending: true,
                nulls_first: false,
            },
            SortKey {
                expr: Expr::Column {
                    index: p2_trace_idx,
                    name: traces::TRACE_ID.to_owned(),
                    data_type: DataType::FixedBytes16,
                },
                descending: true,
                nulls_first: false,
            },
        ];

        let mut phase2_streams: Vec<Box<dyn PartitionStream>> = Vec::with_capacity(groups.len());
        for (slice_idx, ids) in groups {
            if ctx.eval.cancel.is_cancelled() {
                p1_res.free();
                return Err(QueryError::Cancelled);
            }
            phase2_streams.extend(self.open_range_streams(
                RangeScanArgs {
                    bounds: windows[slice_idx],
                    around_ns: None,
                    trace_ids: Some(&ids),
                    merge_columns: &phase2_cols,
                    post_filters_full: &spec.filters,
                    final_projection: None,
                    limit: None,
                },
                ctx,
            )?);
        }
        p1_res.free();
        if phase2_streams.is_empty() {
            return Ok(Vec::new());
        }

        let capped = cap_partitions(phase2_streams, ctx.target_partitions.max(1));
        let sink_schema = Arc::clone(&phase2_schema);
        let sink_keys = p2_sort_keys.clone();
        let sink_ctx = ctx.clone();
        let sinks = run_pipeline(
            ctx,
            capped,
            |_| Ok(Vec::new()),
            move |_| {
                Ok(TopKSink::new(
                    Arc::clone(&sink_schema),
                    sink_keys.clone(),
                    effective_fetch,
                    0,
                    sink_ctx.clone(),
                ))
            },
        )?;
        let sorted_batches = TopKSink::merge(
            sinks,
            Arc::clone(&phase2_schema),
            &p2_sort_keys,
            0,
            effective_fetch,
            ctx,
        )?;

        project_and_buffer_batches(
            sorted_batches,
            &self.schema,
            &phase2_indices,
            spec.projection.as_deref(),
            ctx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn open_two_phase_list(
        &self,
        spec: &ScanSpec,
        sort_keys: &[SortKey],
        limit: usize,
        offset: usize,
        bounds: TimeBounds,
        trace_ids: Option<&BTreeSet<[u8; 16]>>,
        ctx: &ExecContext,
    ) -> Result<Vec<Box<dyn PartitionStream>>, QueryError> {
        let effective_limit = spec.limit.map_or(limit, |l| l.min(limit));
        if effective_limit == 0 || bounds.contradicts() {
            return Ok(Vec::new());
        }
        if let Some(ids) = &trace_ids
            && ids.is_empty()
        {
            return Ok(Vec::new());
        }

        // Phase 1: extract columns referenced by `spec.filters` and `sort_keys`,
        // remap `sort_keys` onto Phase 1's narrower merged schema, and rank via TopK(offset + limit).
        let mut p1_col_indices = BTreeSet::new();
        for filter in &spec.filters {
            filter.referenced_columns(&mut p1_col_indices);
        }
        for sk in sort_keys {
            sk.expr.referenced_columns(&mut p1_col_indices);
        }
        let mut p1_names = Vec::with_capacity(p1_col_indices.len());
        for idx in p1_col_indices {
            let field = self
                .schema
                .fields()
                .get(idx)
                .ok_or(ColumnError::InvalidBatch {
                    reason: "sort key or filter column index out of bounds",
                })?;
            p1_names.push(field.name.clone());
        }
        let phase1_cols = MergeColumns::new(&str_refs(&p1_names));
        let phase1_indices = indices_of_names(&self.schema, &phase1_cols.output())?;
        let phase1_schema = Arc::new(self.schema.project(&phase1_indices)?);

        let mut full_to_p1: Vec<Option<usize>> = vec![None; self.schema.len()];
        for (p1_pos, &full_idx) in phase1_indices.iter().enumerate() {
            full_to_p1[full_idx] = Some(p1_pos);
        }
        let p1_sort_keys: Vec<SortKey> = sort_keys
            .iter()
            .map(|sk| {
                Ok(SortKey {
                    expr: sk.expr.remap_columns(&full_to_p1)?,
                    descending: sk.descending,
                    nulls_first: sk.nulls_first,
                })
            })
            .collect::<Result<Vec<_>, QueryError>>()?;

        let p1_streams = self.open_range_streams(
            RangeScanArgs {
                bounds,
                around_ns: spec.time_range.around_ns,
                trace_ids,
                merge_columns: &phase1_cols,
                post_filters_full: &spec.filters,
                final_projection: None,
                limit: None,
            },
            ctx,
        )?;
        if p1_streams.is_empty() {
            return Ok(Vec::new());
        }

        let sink_schema = Arc::clone(&phase1_schema);
        let sink_keys = p1_sort_keys.clone();
        let sink_ctx = ctx.clone();
        let sinks = run_pipeline(
            ctx,
            p1_streams,
            |_| Ok(Vec::new()),
            move |_| {
                Ok(TopKSink::new(
                    Arc::clone(&sink_schema),
                    sink_keys.clone(),
                    effective_limit,
                    offset,
                    sink_ctx.clone(),
                ))
            },
        )?;
        let ranked_batches = TopKSink::merge(
            sinks,
            Arc::clone(&phase1_schema),
            &p1_sort_keys,
            offset,
            effective_limit,
            ctx,
        )?;

        let p1_trace_idx = phase1_schema
            .index_of(traces::TRACE_ID)
            .ok_or_else(|| QueryError::internal("phase 1 trace_id missing"))?;
        let mut meta_res = ctx.pool.register("two_phase_meta", false);
        let mut winning_ids: Vec<[u8; 16]> = Vec::new();
        for batch in &ranked_batches {
            let id_col = batch
                .column(p1_trace_idx)
                .ok_or_else(|| QueryError::internal("phase 1 trace_id column missing"))?;
            for_each_active_row(batch, |row| {
                let id = read_fixed16_at(id_col, row)
                    .ok_or_else(|| QueryError::internal("phase 1 trace_id null"))?;
                winning_ids.push(id);
                Ok(())
            })?;
            reconcile_res(
                Some(&mut meta_res),
                0,
                winning_ids.capacity() * size_of::<[u8; 16]>(),
            )?;
        }
        drop(ranked_batches);
        if winning_ids.is_empty() {
            meta_res.free();
            return Ok(Vec::new());
        }

        // Phase 2: fetch full requested columns ONLY for `<= limit` winning trace_ids.
        let mut phase2_names = requested_col_names(&self.schema, spec.projection.as_deref())?;
        phase2_names.extend(filter_col_names(&self.schema, &spec.filters));
        let phase2_cols = MergeColumns::new(&str_refs(&phase2_names));
        let phase2_indices = indices_of_names(&self.schema, &phase2_cols.output())?;
        let phase2_schema = Arc::new(self.schema.project(&phase2_indices)?);
        let p2_trace_idx = phase2_schema
            .index_of(traces::TRACE_ID)
            .ok_or_else(|| QueryError::internal("phase 2 trace_id missing"))?;

        let winning_set: BTreeSet<[u8; 16]> = winning_ids.iter().copied().collect();
        let winning_ids_bytes = winning_ids.capacity() * size_of::<[u8; 16]>();
        let winning_set_bytes = winning_set.len() * (size_of::<[u8; 16]>() + 16);
        reconcile_res(
            Some(&mut meta_res),
            0,
            winning_ids_bytes + winning_set_bytes,
        )?;
        let p2_around = if trace_ids.is_some() {
            spec.time_range.around_ns
        } else {
            None
        };
        let p2_streams = self.open_range_streams(
            RangeScanArgs {
                bounds,
                around_ns: p2_around,
                trace_ids: Some(&winning_set),
                merge_columns: &phase2_cols,
                post_filters_full: &spec.filters,
                final_projection: None,
                limit: None,
            },
            ctx,
        )?;
        drop(winning_set);
        reconcile_res(Some(&mut meta_res), 0, winning_ids_bytes)?;
        if p2_streams.is_empty() {
            meta_res.free();
            return Ok(Vec::new());
        }

        let p2_pool = Arc::clone(&ctx.pool);
        let p2_sinks = match run_pipeline(
            ctx,
            p2_streams,
            |_| Ok(Vec::new()),
            move |_| Ok(CollectedWithRes::new(&p2_pool)),
        ) {
            Ok(s) => s,
            Err(err) => {
                meta_res.free();
                return Err(err);
            }
        };

        let mut p2_batches: Vec<Batch> = Vec::new();
        let mut p2_sink_res: Vec<Reservation> = Vec::with_capacity(p2_sinks.len());
        for s in p2_sinks {
            for b in s.batches {
                if !b.is_empty() {
                    p2_batches.push(b);
                }
            }
            p2_sink_res.push(s.res);
        }
        if p2_batches.is_empty() {
            drop(p2_sink_res);
            meta_res.free();
            return Ok(Vec::new());
        }

        let mut p2_res = ctx.pool.register("two_phase_p2", false);
        let refs: Vec<&Batch> = p2_batches.iter().collect();
        let combined = match Batch::concat(Arc::clone(&phase2_schema), &refs, Some(&mut p2_res)) {
            Ok(c) => c,
            Err(err) => {
                drop(p2_batches);
                drop(p2_sink_res);
                p2_res.free();
                meta_res.free();
                return Err(err.into());
            }
        };
        drop(p2_batches);
        drop(p2_sink_res);
        reconcile_res(Some(&mut p2_res), 0, combined.memory_bytes())?;

        let id_col = combined
            .column(p2_trace_idx)
            .ok_or_else(|| QueryError::internal("phase 2 trace_id column missing"))?;
        let mut row_by_id: HashMap<[u8; 16], u32> = HashMap::with_capacity(combined.num_rows());
        let row_by_id_bytes = row_by_id.capacity() * (size_of::<[u8; 16]>() + size_of::<u32>() + 8);
        reconcile_res(Some(&mut meta_res), 0, winning_ids_bytes + row_by_id_bytes)?;
        for row in 0..combined.num_rows() {
            if (row & 1023) == 0 && ctx.eval.cancel.is_cancelled() {
                drop(combined);
                p2_res.free();
                meta_res.free();
                return Err(QueryError::Cancelled);
            }
            if let Some(id) = read_fixed16_at(id_col, row) {
                row_by_id.insert(id, row as u32);
            }
        }

        let mut gather_indices = Vec::with_capacity(winning_ids.len());
        let gather_bytes = gather_indices.capacity() * size_of::<u32>();
        reconcile_res(
            Some(&mut meta_res),
            0,
            winning_ids_bytes + row_by_id_bytes + gather_bytes,
        )?;
        for (i, id) in winning_ids.iter().enumerate() {
            if (i & 1023) == 0 && ctx.eval.cancel.is_cancelled() {
                drop(combined);
                p2_res.free();
                meta_res.free();
                return Err(QueryError::Cancelled);
            }
            if let Some(&row_idx) = row_by_id.get(id) {
                gather_indices.push(row_idx);
            }
        }
        drop(row_by_id);
        drop(winning_ids);
        reconcile_res(Some(&mut meta_res), 0, gather_bytes)?;

        if gather_indices.is_empty() {
            drop(gather_indices);
            meta_res.free();
            drop(combined);
            p2_res.free();
            return Ok(Vec::new());
        }

        let combined = project_batch_to_requested(
            combined,
            &self.schema,
            &phase2_indices,
            spec.projection.as_deref(),
        )?;
        reconcile_res(Some(&mut p2_res), 0, combined.memory_bytes())?;

        let out_schema = combined.schema();
        let chunk_size = ctx.batch_size.max(1);
        let mut chunks = Vec::new();
        let mut out_res = ctx.pool.register("two_phase_out", false);
        for chunk_indices in gather_indices.chunks(chunk_size) {
            if ctx.eval.cancel.is_cancelled() {
                drop(chunks);
                out_res.free();
                drop(gather_indices);
                meta_res.free();
                drop(combined);
                p2_res.free();
                return Err(QueryError::Cancelled);
            }
            let chunk_start_res = out_res.size();
            let mut cols = Vec::with_capacity(combined.columns().len());
            for col in combined.columns() {
                cols.push(Column::Flat(col.take(chunk_indices, Some(&mut out_res))?));
            }
            let chunk = Batch::try_new_all(Arc::clone(out_schema), cols, chunk_indices.len())?;
            reconcile_res(Some(&mut out_res), chunk_start_res, chunk.memory_bytes())?;
            chunks.push(chunk);
        }
        drop(gather_indices);
        meta_res.free();
        drop(combined);
        p2_res.free();

        if chunks.is_empty() {
            out_res.free();
            return Ok(Vec::new());
        }
        Ok(vec![Box::new(BufferedBatchStream::new(chunks, out_res))])
    }
}

impl ScanSource for TracesSource {
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
        let scoped_spec;
        let spec = if let Some(vf) = self.visible_from {
            scoped_spec = super::apply_visible_from(spec, &self.schema, traces::START_TIME, vf)?;
            &scoped_spec
        } else {
            spec
        };
        let bounds = self.effective_bounds(spec);
        let trace_ids = self.effective_trace_ids(spec);

        match &self.list_mode {
            Some(TraceListMode::Widening { fetch }) => {
                self.open_widening_list(spec, *fetch, bounds, trace_ids.as_ref(), ctx)
            }
            Some(TraceListMode::TwoPhase {
                sort_keys,
                limit,
                offset,
            }) => self.open_two_phase_list(
                spec,
                sort_keys,
                *limit,
                *offset,
                bounds,
                trace_ids.as_ref(),
                ctx,
            ),
            None => {
                let mut requested = requested_col_names(&self.schema, spec.projection.as_deref())?;
                requested.extend(filter_col_names(&self.schema, &spec.filters));
                let merge_columns = MergeColumns::new(&str_refs(&requested));
                self.open_range_streams(
                    RangeScanArgs {
                        bounds,
                        around_ns: spec.time_range.around_ns,
                        trace_ids: trace_ids.as_ref(),
                        merge_columns: &merge_columns,
                        post_filters_full: &spec.filters,
                        final_projection: spec.projection.as_deref(),
                        limit: spec.limit,
                    },
                    ctx,
                )
            }
        }
    }

    fn explain_summary(&self, spec: &ScanSpec) -> String {
        let mode = match &self.list_mode {
            Some(TraceListMode::Widening { .. }) => "list",
            Some(TraceListMode::TwoPhase { .. }) => "list",
            None => {
                if self.effective_trace_ids(spec).is_some() {
                    "lookup"
                } else {
                    "range"
                }
            }
        };
        format!(
            "TracesSource(mode={mode}, files={}, filters={}, deletions={})",
            self.files.len(),
            spec.filters.len(),
            self.deletions.len()
        )
    }
}

struct CollectedWithRes {
    batches: Vec<Batch>,
    res: Reservation,
}

impl CollectedWithRes {
    fn new(pool: &Arc<cetra_column::ScopedPool>) -> Self {
        Self {
            batches: Vec::new(),
            res: pool.register("two_phase_collect", false),
        }
    }
}

impl PushSink for CollectedWithRes {
    fn consume(
        &mut self,
        batch: Batch,
        batch_res: &mut Reservation,
    ) -> Result<ControlFlow, QueryError> {
        transfer_or_grow(batch_res, &mut self.res, batch.memory_bytes())?;
        self.batches.push(batch);
        Ok(ControlFlow::Continue)
    }
}

fn project_batch_to_requested(
    batch: Batch,
    full_schema: &Schema,
    merged_full_indices: &[usize],
    projection: Option<&[usize]>,
) -> Result<Batch, QueryError> {
    let mut full_to_merged: Vec<Option<usize>> = vec![None; full_schema.len()];
    for (pos, &full_idx) in merged_full_indices.iter().enumerate() {
        full_to_merged[full_idx] = Some(pos);
    }
    match projection {
        None => Ok(batch),
        Some(indices) => {
            let mut proj = Vec::with_capacity(indices.len());
            for &full_idx in indices {
                let pos = full_to_merged[full_idx].ok_or_else(|| {
                    QueryError::internal("projected column missing from merged batch")
                })?;
                proj.push(pos);
            }
            Ok(batch.into_project(&proj)?)
        }
    }
}

fn project_and_buffer_batches(
    batches: Vec<Batch>,
    full_schema: &Schema,
    merged_full_indices: &[usize],
    projection: Option<&[usize]>,
    ctx: &ExecContext,
) -> Result<Vec<Box<dyn PartitionStream>>, QueryError> {
    let mut out = Vec::with_capacity(batches.len());
    let mut res = ctx.pool.register("trace_list_out", false);
    for b in batches {
        if b.is_empty() {
            continue;
        }
        let proj = project_batch_to_requested(b, full_schema, merged_full_indices, projection)?;
        res.try_grow(proj.memory_bytes())?;
        out.push(proj);
    }
    if out.is_empty() {
        res.free();
        return Ok(Vec::new());
    }
    Ok(vec![Box::new(BufferedBatchStream::new(out, res))])
}

// ---------------------------------------------------------------------------
// Stream wrapper: MemPartialsStream & NativeTraceMergeStream
// ---------------------------------------------------------------------------

struct MemPartialsStream {
    batches: VecDeque<MemBatch>,
    input_indices: Vec<usize>,
    filters: Vec<Expr>,
    eval: EvalContext,
}

impl PartitionStream for MemPartialsStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_ref().map_or(0, |r| r.size());
        let eval_ctx = self.eval.with_cancel(cancel.clone());

        while let Some(mem_batch) = self.batches.pop_front() {
            if cancel.is_cancelled() {
                rollback_res(res, before);
                return Err(QueryError::Cancelled);
            }
            if mem_batch.batch.num_rows() == 0 {
                continue;
            }

            let partials = match mem_batch.trace_partials() {
                Ok(p) => p,
                Err(e) => {
                    rollback_res(res, before);
                    return Err(QueryError::internal(format!("trace partials error: {e}")));
                }
            };
            if partials.num_rows() == 0 {
                continue;
            }
            let batch = match partials.project(&self.input_indices) {
                Ok(b) => b,
                Err(err) => {
                    rollback_res(res, before);
                    return Err(err.into());
                }
            };
            drop(partials);

            reconcile_res(res.as_deref_mut(), before, batch.memory_bytes())?;

            let filtered = apply_tombstones_and_filters(
                batch,
                None,
                &self.filters,
                &eval_ctx,
                res.as_deref_mut(),
                before,
            )?;
            if filtered.is_empty() {
                rollback_res(res.as_deref_mut(), before);
                continue;
            }

            reconcile_res(res, before, filtered.memory_bytes())?;
            return Ok(Some(filtered));
        }

        rollback_res(res, before);
        Ok(None)
    }
}

struct NativeTraceMergeStream {
    input: Box<dyn PartitionStream>,
    input_res: Reservation,
    merger: NativeTraceMerger,
    merger_res: Reservation,
    scratch_res: Reservation,
    merge_columns: MergeColumns,
    full_schema: Arc<Schema>,
    merged_start_idx: usize,
    bounds: TimeBounds,
    post_filters: Vec<Expr>,
    projection: Option<Vec<usize>>,
    remaining: Option<Arc<AtomicUsize>>,
    eval: EvalContext,
    active_input: Option<(Batch, usize)>,
    current_trace_id: Option<[u8; 16]>,
    budget_bytes: usize,
    max_traces_per_chunk: usize,
    done: bool,
}

impl NativeTraceMergeStream {
    fn finish_and_filter_merger(
        &mut self,
        eval_ctx: &EvalContext,
    ) -> Result<Option<Batch>, QueryError> {
        let merger = std::mem::replace(
            &mut self.merger,
            NativeTraceMerger::new(&self.merge_columns, &self.full_schema)?,
        );
        self.current_trace_id = None;
        if merger.is_empty() {
            self.merger_res.free();
            return Ok(None);
        }
        let mut merged = merger.finish()?;
        let merged_bytes = merged.memory_bytes();
        let transfer = transfer_or_grow(&mut self.merger_res, &mut self.scratch_res, merged_bytes);
        self.merger_res.free();
        transfer?;

        // 1. Evaluate unpadded `start_time` bounds `[a, b]`.
        if self.bounds.min.is_some() || self.bounds.max.is_some() {
            let start_col = merged
                .column(self.merged_start_idx)
                .ok_or_else(|| QueryError::internal("merged start_time missing"))?;
            let raw_len = merged.raw_len();
            let mut mask = Bitmap::new_ones(raw_len);
            for row in 0..raw_len {
                let keep = read_ts_at(start_col, row).is_some_and(|ts| {
                    self.bounds.min.is_none_or(|lo| ts >= lo)
                        && self.bounds.max.is_none_or(|hi| ts <= hi)
                });
                if !keep {
                    let _ = mask.set(row, false);
                }
            }
            merged = merged.into_filter_mask(&mask)?;
            drop(mask);
            reconcile_res(Some(&mut self.scratch_res), 0, merged.memory_bytes())?;
            if merged.is_empty() {
                self.scratch_res.free();
                return Ok(None);
            }
        }

        // 2. Evaluate all post-merge user filters.
        for filter in &self.post_filters {
            let mask = eval_filter(filter, &merged, eval_ctx, Some(&mut self.scratch_res))?;
            merged = merged.into_filter_mask(&mask)?;
            drop(mask);
            reconcile_res(Some(&mut self.scratch_res), 0, merged.memory_bytes())?;
            if merged.is_empty() {
                self.scratch_res.free();
                return Ok(None);
            }
        }

        // 3. Apply shared `limit`.
        if self.remaining.is_some() {
            let take = claim_shared_limit(self.remaining.as_ref(), merged.num_rows());
            if self
                .remaining
                .as_ref()
                .is_some_and(|r| r.load(Ordering::Acquire) == 0)
            {
                self.done = true;
                self.active_input = None;
                self.input_res.free();
            }
            if take == 0 {
                self.done = true;
                self.active_input = None;
                self.input_res.free();
                self.scratch_res.free();
                return Ok(None);
            }
            if take < merged.num_rows() {
                self.done = true;
                self.active_input = None;
                self.input_res.free();
                let end = u32::try_from(take).map_err(|_| ColumnError::Overflow {
                    reason: "traces limit exceeds u32::MAX",
                })?;
                let sel = match merged.selection() {
                    Selection::All => Selection::Indices((0..end).collect()),
                    Selection::Mask(m) => {
                        Selection::Indices(m.iter_ones().take(take).map(|i| i as u32).collect())
                    }
                    Selection::Indices(idx) => Selection::Indices(idx[..take].to_vec()),
                };
                merged = merged.into_with_selection(sel)?;
                reconcile_res(Some(&mut self.scratch_res), 0, merged.memory_bytes())?;
            }
        }

        // 4. Project to requested output schema.
        if let Some(indices) = &self.projection {
            merged = merged.into_project(indices)?;
            reconcile_res(Some(&mut self.scratch_res), 0, merged.memory_bytes())?;
        }

        if merged.num_rows().saturating_mul(4) < merged.raw_len() {
            let compacted = merged.compact(Some(&mut self.scratch_res))?;
            drop(merged);
            merged = compacted;
            reconcile_res(Some(&mut self.scratch_res), 0, merged.memory_bytes())?;
        }

        Ok(Some(merged))
    }
}

impl PartitionStream for NativeTraceMergeStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_ref().map_or(0, |r| r.size());
        let eval_ctx = self.eval.with_cancel(cancel.clone());

        loop {
            if cancel.is_cancelled() {
                self.scratch_res.free();
                self.merger_res.free();
                self.input_res.free();
                rollback_res(res, before);
                return Err(QueryError::Cancelled);
            }
            if self.done
                || self
                    .remaining
                    .as_ref()
                    .is_some_and(|r| r.load(Ordering::Acquire) == 0)
            {
                self.done = true;
                self.active_input = None;
                self.scratch_res.free();
                self.merger_res.free();
                self.input_res.free();
                rollback_res(res, before);
                return Ok(None);
            }

            if self.active_input.is_none() {
                match self.input.next_batch(cancel, Some(&mut self.input_res)) {
                    Ok(Some(b)) => {
                        if b.is_empty() {
                            self.input_res.free();
                            continue;
                        }
                        let compacted = if b.selection().is_all(b.raw_len()) {
                            b
                        } else {
                            match b.compact(Some(&mut self.input_res)) {
                                Ok(c) => {
                                    drop(b);
                                    if let Err(err) = reconcile_res(
                                        Some(&mut self.input_res),
                                        0,
                                        c.memory_bytes(),
                                    ) {
                                        self.scratch_res.free();
                                        self.merger_res.free();
                                        self.input_res.free();
                                        rollback_res(res, before);
                                        return Err(err);
                                    }
                                    c
                                }
                                Err(err) => {
                                    self.scratch_res.free();
                                    self.merger_res.free();
                                    self.input_res.free();
                                    rollback_res(res, before);
                                    return Err(err.into());
                                }
                            }
                        };
                        self.active_input = Some((compacted, 0));
                    }
                    Ok(None) => {
                        self.done = true;
                        self.input_res.free();
                        match self.finish_and_filter_merger(&eval_ctx) {
                            Ok(Some(out)) => {
                                let out_bytes = out.memory_bytes();
                                let handoff = handoff_res(
                                    &mut self.scratch_res,
                                    res.as_deref_mut(),
                                    before,
                                    out_bytes,
                                );
                                self.scratch_res.free();
                                handoff?;
                                return Ok(Some(out));
                            }
                            Ok(None) => {
                                self.scratch_res.free();
                                rollback_res(res, before);
                                return Ok(None);
                            }
                            Err(err) => {
                                self.scratch_res.free();
                                self.merger_res.free();
                                rollback_res(res, before);
                                return Err(err);
                            }
                        }
                    }
                    Err(err) => {
                        self.scratch_res.free();
                        self.merger_res.free();
                        self.input_res.free();
                        rollback_res(res, before);
                        return Err(err);
                    }
                }
            }

            // Process contiguous trace_id runs from `active_input` up to chunk/memory budget.
            let Some((batch, mut offset)) = self.active_input.take() else {
                continue;
            };
            let total = batch.raw_len();
            let trace_id_col = match batch.column_by_name(traces::TRACE_ID) {
                Some(c) => c,
                None => {
                    self.scratch_res.free();
                    self.merger_res.free();
                    self.input_res.free();
                    rollback_res(res, before);
                    return Err(QueryError::internal(
                        "trace_id missing from sorted partials",
                    ));
                }
            };

            let mut ready_to_emit = false;
            let reconcile_threshold = self.budget_bytes.min(64 * 1024);
            while offset < total {
                if cancel.is_cancelled() {
                    self.scratch_res.free();
                    self.merger_res.free();
                    self.input_res.free();
                    rollback_res(res, before);
                    return Err(QueryError::Cancelled);
                }
                let run_id = match read_fixed16_at(trace_id_col, offset) {
                    Some(id) => id,
                    None => {
                        self.scratch_res.free();
                        self.merger_res.free();
                        self.input_res.free();
                        rollback_res(res, before);
                        return Err(QueryError::internal("null trace_id in sorted partials"));
                    }
                };
                if let Some(prev_id) = self.current_trace_id
                    && prev_id != run_id
                    && (self.merger.size() >= self.budget_bytes
                        || self.merger.len() >= self.max_traces_per_chunk)
                {
                    ready_to_emit = true;
                    break;
                }

                let mut run_end = offset + 1;
                while run_end < total && read_fixed16_at(trace_id_col, run_end) == Some(run_id) {
                    run_end += 1;
                }

                if let Err(err) = self.merger.push_range(&batch, offset, run_end) {
                    self.scratch_res.free();
                    self.merger_res.free();
                    self.input_res.free();
                    rollback_res(res, before);
                    return Err(err);
                }
                if self.merger.size().abs_diff(self.merger_res.size()) >= reconcile_threshold
                    && let Err(err) =
                        reconcile_res(Some(&mut self.merger_res), 0, self.merger.size())
                {
                    self.scratch_res.free();
                    self.merger_res.free();
                    self.input_res.free();
                    rollback_res(res, before);
                    return Err(err);
                }
                self.current_trace_id = Some(run_id);
                offset = run_end;
            }

            if self.merger.size() != self.merger_res.size()
                && let Err(err) = reconcile_res(Some(&mut self.merger_res), 0, self.merger.size())
            {
                self.scratch_res.free();
                self.merger_res.free();
                self.input_res.free();
                rollback_res(res, before);
                return Err(err);
            }

            if offset < total {
                self.active_input = Some((batch, offset));
            } else {
                drop(batch);
                self.input_res.free();
            }

            if ready_to_emit {
                match self.finish_and_filter_merger(&eval_ctx) {
                    Ok(Some(out)) => {
                        let out_bytes = out.memory_bytes();
                        let handoff = handoff_res(
                            &mut self.scratch_res,
                            res.as_deref_mut(),
                            before,
                            out_bytes,
                        );
                        self.scratch_res.free();
                        handoff?;
                        return Ok(Some(out));
                    }
                    Ok(None) => {
                        self.scratch_res.free();
                    }
                    Err(err) => {
                        self.scratch_res.free();
                        self.merger_res.free();
                        self.input_res.free();
                        rollback_res(res, before);
                        return Err(err);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// NativeTraceMerger: exact 30-column rollup matching `crates/storage/src/partials.rs`
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TraceColumn {
    ProjectId = 0,
    TraceId = 1,
    StartTime = 2,
    EndTime = 3,
    Name = 4,
    RootSpanId = 5,
    HasRoot = 6,
    Status = 7,
    SessionId = 8,
    UserId = 9,
    Environment = 10,
    Release = 11,
    Tags = 12,
    Metadata = 13,
    InputPreview = 14,
    OutputPreview = 15,
    SpanCount = 16,
    ErrorCount = 17,
    LlmCount = 18,
    InputTokens = 19,
    OutputTokens = 20,
    CacheReadTokens = 21,
    CacheWriteTokens = 22,
    ReasoningTokens = 23,
    TotalTokens = 24,
    CostTotal = 25,
    Models = 26,
    Prompts = 27,
    LastIngestTime = 28,
    PartialCount = 29,
}

impl TraceColumn {
    const COUNT: usize = 30;

    fn name(self) -> &'static str {
        match self {
            Self::ProjectId => traces::PROJECT_ID,
            Self::TraceId => traces::TRACE_ID,
            Self::StartTime => traces::START_TIME,
            Self::EndTime => traces::END_TIME,
            Self::Name => traces::NAME,
            Self::RootSpanId => traces::ROOT_SPAN_ID,
            Self::HasRoot => traces::HAS_ROOT,
            Self::Status => traces::STATUS,
            Self::SessionId => traces::SESSION_ID,
            Self::UserId => traces::USER_ID,
            Self::Environment => traces::ENVIRONMENT,
            Self::Release => traces::RELEASE,
            Self::Tags => traces::TAGS,
            Self::Metadata => traces::METADATA,
            Self::InputPreview => traces::INPUT_PREVIEW,
            Self::OutputPreview => traces::OUTPUT_PREVIEW,
            Self::SpanCount => traces::SPAN_COUNT,
            Self::ErrorCount => traces::ERROR_COUNT,
            Self::LlmCount => traces::LLM_COUNT,
            Self::InputTokens => traces::INPUT_TOKENS,
            Self::OutputTokens => traces::OUTPUT_TOKENS,
            Self::CacheReadTokens => traces::CACHE_READ_TOKENS,
            Self::CacheWriteTokens => traces::CACHE_WRITE_TOKENS,
            Self::ReasoningTokens => traces::REASONING_TOKENS,
            Self::TotalTokens => traces::TOTAL_TOKENS,
            Self::CostTotal => traces::COST_TOTAL,
            Self::Models => traces::MODELS,
            Self::Prompts => traces::PROMPTS,
            Self::LastIngestTime => traces::LAST_INGEST_TIME,
            Self::PartialCount => traces::PARTIAL_COUNT,
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            traces::PROJECT_ID => Some(Self::ProjectId),
            traces::TRACE_ID => Some(Self::TraceId),
            traces::START_TIME => Some(Self::StartTime),
            traces::END_TIME => Some(Self::EndTime),
            traces::NAME => Some(Self::Name),
            traces::ROOT_SPAN_ID => Some(Self::RootSpanId),
            traces::HAS_ROOT => Some(Self::HasRoot),
            traces::STATUS => Some(Self::Status),
            traces::SESSION_ID => Some(Self::SessionId),
            traces::USER_ID => Some(Self::UserId),
            traces::ENVIRONMENT => Some(Self::Environment),
            traces::RELEASE => Some(Self::Release),
            traces::TAGS => Some(Self::Tags),
            traces::METADATA => Some(Self::Metadata),
            traces::INPUT_PREVIEW => Some(Self::InputPreview),
            traces::OUTPUT_PREVIEW => Some(Self::OutputPreview),
            traces::SPAN_COUNT => Some(Self::SpanCount),
            traces::ERROR_COUNT => Some(Self::ErrorCount),
            traces::LLM_COUNT => Some(Self::LlmCount),
            traces::INPUT_TOKENS => Some(Self::InputTokens),
            traces::OUTPUT_TOKENS => Some(Self::OutputTokens),
            traces::CACHE_READ_TOKENS => Some(Self::CacheReadTokens),
            traces::CACHE_WRITE_TOKENS => Some(Self::CacheWriteTokens),
            traces::REASONING_TOKENS => Some(Self::ReasoningTokens),
            traces::TOTAL_TOKENS => Some(Self::TotalTokens),
            traces::COST_TOTAL => Some(Self::CostTotal),
            traces::MODELS => Some(Self::Models),
            traces::PROMPTS => Some(Self::Prompts),
            traces::LAST_INGEST_TIME => Some(Self::LastIngestTime),
            traces::PARTIAL_COUNT => Some(Self::PartialCount),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Rank {
    root: u8,
    start_time: i64,
    tie: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Ranked<T> {
    rank: Rank,
    value: T,
}

#[derive(Debug, Clone, PartialEq)]
enum Acc {
    Fixed16([u8; 16]),
    I64(i64),
    Usd(i128),
    Bool(bool),
    RankedStr(Ranked<String>),
    RankedFixed8(Ranked<[u8; 8]>),
    StrSet(BTreeSet<String>),
    StrMap(BTreeMap<String, Ranked<String>>),
}

struct RowCtx {
    rank: Rank,
    root: bool,
    preview: bool,
}

struct TraceAcc {
    slots: Vec<Option<Acc>>,
}

impl TraceAcc {
    fn new(slot_count: usize) -> Self {
        Self {
            slots: vec![None; slot_count],
        }
    }

    fn update_from_col(
        &mut self,
        slot_idx: usize,
        col_kind: TraceColumn,
        col: &Column,
        row: usize,
        ctx: &RowCtx,
    ) -> Result<usize, QueryError> {
        if col.is_null(row) {
            return Ok(0);
        }
        let slot = &mut self.slots[slot_idx];
        let grown = match col_kind {
            TraceColumn::TraceId => {
                let id = read_fixed16_at(col, row)
                    .ok_or_else(|| QueryError::internal("trace_id column type mismatch"))?;
                *slot = Some(Acc::Fixed16(id));
                0
            }
            TraceColumn::ProjectId
            | TraceColumn::Name
            | TraceColumn::SessionId
            | TraceColumn::UserId
            | TraceColumn::Environment
            | TraceColumn::Release => {
                let text = read_str_at(col, row)
                    .ok_or_else(|| QueryError::internal("string column type mismatch"))?;
                ranked_str_slot(slot, text, ctx)
            }
            TraceColumn::InputPreview | TraceColumn::OutputPreview => {
                if ctx.preview {
                    let text = read_str_at(col, row)
                        .ok_or_else(|| QueryError::internal("preview column type mismatch"))?;
                    ranked_str_slot(slot, text, ctx)
                } else {
                    0
                }
            }
            TraceColumn::Status => {
                if ctx.root {
                    let text = read_str_at(col, row)
                        .ok_or_else(|| QueryError::internal("status column type mismatch"))?;
                    ranked_str_slot(slot, text, ctx)
                } else {
                    0
                }
            }
            TraceColumn::RootSpanId => {
                if ctx.root {
                    let bytes = read_fixed8_at(col, row)
                        .ok_or_else(|| QueryError::internal("root_span_id column type mismatch"))?;
                    let candidate = Ranked {
                        rank: ctx.rank.clone(),
                        value: bytes,
                    };
                    match slot {
                        Some(Acc::RankedFixed8(kept)) => {
                            if candidate < *kept {
                                *kept = candidate;
                            }
                        }
                        _ => *slot = Some(Acc::RankedFixed8(candidate)),
                    }
                    8
                } else {
                    0
                }
            }
            TraceColumn::HasRoot => {
                let val = read_bool_at(col, row)
                    .ok_or_else(|| QueryError::internal("has_root column type mismatch"))?;
                match slot {
                    Some(Acc::Bool(kept)) => *kept |= val,
                    _ => *slot = Some(Acc::Bool(val)),
                }
                0
            }
            TraceColumn::StartTime => {
                let ts = read_ts_at(col, row)
                    .ok_or_else(|| QueryError::internal("start_time column type mismatch"))?;
                match slot {
                    Some(Acc::I64(kept)) => *kept = (*kept).min(ts),
                    _ => *slot = Some(Acc::I64(ts)),
                }
                0
            }
            TraceColumn::EndTime | TraceColumn::LastIngestTime => {
                let ts = read_ts_at(col, row)
                    .ok_or_else(|| QueryError::internal("timestamp column type mismatch"))?;
                match slot {
                    Some(Acc::I64(kept)) => *kept = (*kept).max(ts),
                    _ => *slot = Some(Acc::I64(ts)),
                }
                0
            }
            TraceColumn::Tags | TraceColumn::Models | TraceColumn::Prompts => {
                let list = read_str_list_at(col, row)
                    .ok_or_else(|| QueryError::internal("string list column type mismatch"))?;
                match slot {
                    Some(Acc::StrSet(set)) => {
                        let mut added = 0usize;
                        for s in list {
                            if set.insert(s.clone()) {
                                added += s.len();
                            }
                        }
                        added
                    }
                    _ => {
                        let set: BTreeSet<String> = list.into_iter().collect();
                        let added = set.iter().map(String::len).sum();
                        *slot = Some(Acc::StrSet(set));
                        added
                    }
                }
            }
            TraceColumn::Metadata => {
                let entries = read_str_map_at(col, row)
                    .ok_or_else(|| QueryError::internal("metadata map column type mismatch"))?;
                if !matches!(slot, Some(Acc::StrMap(_))) {
                    *slot = Some(Acc::StrMap(BTreeMap::new()));
                }
                let Some(Acc::StrMap(map)) = slot else {
                    return Ok(0);
                };
                let mut added = 0usize;
                for (key, value) in entries {
                    let candidate = Ranked {
                        rank: ctx.rank.clone(),
                        value,
                    };
                    added += key.len() + candidate.value.len();
                    match map.get_mut(&key) {
                        Some(kept) => {
                            if candidate < *kept {
                                *kept = candidate;
                            }
                        }
                        None => {
                            map.insert(key, candidate);
                        }
                    }
                }
                added
            }
            TraceColumn::SpanCount
            | TraceColumn::ErrorCount
            | TraceColumn::LlmCount
            | TraceColumn::InputTokens
            | TraceColumn::OutputTokens
            | TraceColumn::CacheReadTokens
            | TraceColumn::CacheWriteTokens
            | TraceColumn::ReasoningTokens
            | TraceColumn::TotalTokens
            | TraceColumn::PartialCount => {
                let val = read_i64_at(col, row)
                    .ok_or_else(|| QueryError::internal("integer column type mismatch"))?;
                match slot {
                    Some(Acc::I64(kept)) => *kept = kept.saturating_add(val),
                    _ => *slot = Some(Acc::I64(val)),
                }
                0
            }
            TraceColumn::CostTotal => {
                let val = read_f64_at(col, row)
                    .ok_or_else(|| QueryError::internal("cost_total column type mismatch"))?;
                let units = usd_units(val);
                match slot {
                    Some(Acc::Usd(kept)) => *kept = kept.saturating_add(units),
                    _ => *slot = Some(Acc::Usd(units)),
                }
                0
            }
        };
        Ok(grown)
    }
}

fn ranked_str_slot(slot: &mut Option<Acc>, text: &str, ctx: &RowCtx) -> usize {
    let grown = text.len();
    let candidate = Ranked {
        rank: ctx.rank.clone(),
        value: text.to_owned(),
    };
    match slot {
        Some(Acc::RankedStr(kept)) => {
            if candidate < *kept {
                *kept = candidate;
            }
        }
        _ => *slot = Some(Acc::RankedStr(candidate)),
    }
    grown
}

fn usd_units(value: f64) -> i128 {
    (value * USD_UNITS).round() as i128
}

fn usd_from_units(units: i128) -> f64 {
    units as f64 / USD_UNITS
}

/// Spill-aware columnar trace partial merger matching `partials.rs::TraceMerger`.
pub(crate) struct NativeTraceMerger {
    input_cols: Vec<TraceColumn>,
    output_cols: Vec<TraceColumn>,
    slot_of: [Option<usize>; TraceColumn::COUNT],
    output_schema: Arc<Schema>,
    traces: BTreeMap<[u8; 16], TraceAcc>,
    size: usize,
}

impl std::fmt::Debug for NativeTraceMerger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeTraceMerger")
            .field("input_cols", &self.input_cols.len())
            .field("output_cols", &self.output_cols.len())
            .field("trace_count", &self.traces.len())
            .field("size_bytes", &self.size)
            .finish()
    }
}

impl NativeTraceMerger {
    /// Creates a [`NativeTraceMerger`] for `columns` against the 30-column `full_schema`.
    pub(crate) fn new(columns: &MergeColumns, full_schema: &Schema) -> Result<Self, QueryError> {
        let input_names = columns.input();
        let output_names = columns.output();
        let mut input_cols = Vec::with_capacity(input_names.len());
        let mut slot_of = [None; TraceColumn::COUNT];
        for (slot_idx, name) in input_names.iter().enumerate() {
            let col = TraceColumn::from_name(name)
                .ok_or_else(|| QueryError::internal(format!("unknown trace column {name}")))?;
            slot_of[col as usize] = Some(slot_idx);
            input_cols.push(col);
        }

        let mut output_cols = Vec::with_capacity(output_names.len());
        for name in &output_names {
            let col = TraceColumn::from_name(name)
                .ok_or_else(|| QueryError::internal(format!("unknown trace column {name}")))?;
            output_cols.push(col);
        }
        let output_indices = indices_of_names(full_schema, &output_names)?;
        let output_schema = Arc::new(full_schema.project(&output_indices)?);

        Ok(Self {
            input_cols,
            output_cols,
            slot_of,
            output_schema,
            traces: BTreeMap::new(),
            size: 0,
        })
    }

    /// Number of distinct traces currently accumulated.
    pub(crate) fn len(&self) -> usize {
        self.traces.len()
    }

    /// True when no traces have been pushed.
    pub(crate) fn is_empty(&self) -> bool {
        self.traces.is_empty()
    }

    /// Tracked heap byte size of accumulated trace states.
    pub(crate) fn size(&self) -> usize {
        self.size
    }

    /// Folds rows in `start..end` of `partials` (`Selection::All`) into the trace
    /// accumulators without copying column buffers.
    pub(crate) fn push_range(
        &mut self,
        partials: &Batch,
        start: usize,
        end: usize,
    ) -> Result<(), QueryError> {
        if start >= end || partials.is_empty() {
            return Ok(());
        }
        if end > partials.raw_len() || !partials.selection().is_all(partials.raw_len()) {
            return Err(ColumnError::InvalidBatch {
                reason: "push_range requires Selection::All and valid start..end bounds",
            }
            .into());
        }
        let mut col_refs: Vec<(usize, TraceColumn, &Column)> =
            Vec::with_capacity(self.input_cols.len());
        for (slot_idx, &col_kind) in self.input_cols.iter().enumerate() {
            let name = col_kind.name();
            let col = partials.column_by_name(name).ok_or_else(|| {
                QueryError::internal(format!("partial batch missing column {name}"))
            })?;
            col_refs.push((slot_idx, col_kind, col));
        }

        let start_col = partials
            .column_by_name(traces::START_TIME)
            .ok_or_else(|| QueryError::internal("partial batch missing start_time"))?;
        let has_root_col = partials
            .column_by_name(traces::HAS_ROOT)
            .ok_or_else(|| QueryError::internal("partial batch missing has_root"))?;
        let root_span_col = partials
            .column_by_name(traces::ROOT_SPAN_ID)
            .ok_or_else(|| QueryError::internal("partial batch missing root_span_id"))?;
        let trace_id_col = partials
            .column_by_name(traces::TRACE_ID)
            .ok_or_else(|| QueryError::internal("partial batch missing trace_id"))?;

        let slot_count = self.input_cols.len();
        let traces = &mut self.traces;
        let size = &mut self.size;

        for row in start..end {
            let start_time = read_ts_at(start_col, row)
                .ok_or_else(|| QueryError::internal("partial start_time missing"))?;
            let root = read_bool_at(has_root_col, row)
                .ok_or_else(|| QueryError::internal("partial has_root missing"))?;
            let tie = read_fixed8_at(root_span_col, row).map(|b| b.to_vec());
            let trace_id = read_fixed16_at(trace_id_col, row)
                .ok_or_else(|| QueryError::internal("partial trace_id missing"))?;

            let ctx = RowCtx {
                rank: Rank {
                    root: u8::from(!root),
                    start_time,
                    tie,
                },
                root,
                preview: true,
            };

            let fresh = !traces.contains_key(&trace_id);
            let acc = traces
                .entry(trace_id)
                .or_insert_with(|| TraceAcc::new(slot_count));
            if fresh {
                *size += size_of::<TraceAcc>() + slot_count * size_of::<Option<Acc>>() + 16 + 32;
            }
            for &(slot_idx, col_kind, col) in &col_refs {
                *size += acc.update_from_col(slot_idx, col_kind, col, row, &ctx)?;
            }
        }
        Ok(())
    }

    /// Finalizes all accumulated traces into a compacted `cetra_column::Batch`.
    pub(crate) fn finish(self) -> Result<Batch, QueryError> {
        let row_count = self.traces.len();
        if row_count == 0 {
            return Ok(Batch::empty(self.output_schema));
        }
        let rows: Vec<&TraceAcc> = self.traces.values().collect();
        let mut columns = Vec::with_capacity(self.output_cols.len());
        for &col_kind in &self.output_cols {
            columns.push(build_merged_column(col_kind, &self.slot_of, &rows)?);
        }
        Ok(Batch::try_new_all(self.output_schema, columns, row_count)?)
    }
}

fn build_merged_column(
    col_kind: TraceColumn,
    slot_of: &[Option<usize>; TraceColumn::COUNT],
    rows: &[&TraceAcc],
) -> Result<Column, QueryError> {
    let n = rows.len();
    if col_kind == TraceColumn::Status {
        let error_slot = slot_of[TraceColumn::ErrorCount as usize];
        let status_slot = slot_of[TraceColumn::Status as usize];
        let mut strs = Vec::with_capacity(n);
        for acc in rows {
            let errors = error_slot
                .and_then(|s| acc.slots[s].as_ref())
                .and_then(|a| match a {
                    Acc::I64(v) => Some(*v),
                    _ => None,
                })
                .unwrap_or(0);
            if errors > 0 {
                strs.push(SpanStatus::Error.as_str());
            } else if let Some(Acc::RankedStr(r)) = status_slot.and_then(|s| acc.slots[s].as_ref())
            {
                strs.push(r.value.as_str());
            } else {
                strs.push(SpanStatus::Unset.as_str());
            }
        }
        let data = Utf8Data::try_from_slices(strs)?;
        return Ok(Column::Flat(FlatColumn::try_new(
            FlatData::Utf8(data),
            None,
        )?));
    }

    let slot_idx = slot_of[col_kind as usize]
        .ok_or_else(|| QueryError::internal(format!("missing slot for {}", col_kind.name())))?;

    match col_kind {
        TraceColumn::TraceId => {
            let mut vals = Vec::with_capacity(n);
            for acc in rows {
                let Some(Acc::Fixed16(id)) = &acc.slots[slot_idx] else {
                    return Err(QueryError::internal("missing trace_id in merger"));
                };
                vals.push(*id);
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::FixedBytes16(vals),
                None,
            )?))
        }
        TraceColumn::ProjectId
        | TraceColumn::Name
        | TraceColumn::SessionId
        | TraceColumn::UserId
        | TraceColumn::Environment
        | TraceColumn::Release
        | TraceColumn::InputPreview
        | TraceColumn::OutputPreview => {
            let mut nulls = Bitmap::new_ones(n);
            let mut vals = Vec::with_capacity(n);
            for (i, acc) in rows.iter().enumerate() {
                match &acc.slots[slot_idx] {
                    Some(Acc::RankedStr(r)) => vals.push(r.value.as_str()),
                    _ => {
                        let _ = nulls.set(i, false);
                        vals.push("");
                    }
                }
            }
            let data = Utf8Data::try_from_slices(vals)?;
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::Utf8(data),
                Some(nulls),
            )?))
        }
        TraceColumn::RootSpanId => {
            let mut nulls = Bitmap::new_ones(n);
            let mut vals = Vec::with_capacity(n);
            for (i, acc) in rows.iter().enumerate() {
                match &acc.slots[slot_idx] {
                    Some(Acc::RankedFixed8(r)) => vals.push(r.value),
                    _ => {
                        let _ = nulls.set(i, false);
                        vals.push([0u8; 8]);
                    }
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::FixedBytes8(vals),
                Some(nulls),
            )?))
        }
        TraceColumn::HasRoot => {
            let mut bm = Bitmap::new_zeroed(n);
            for (i, acc) in rows.iter().enumerate() {
                if let Some(Acc::Bool(true)) = &acc.slots[slot_idx] {
                    let _ = bm.set(i, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(FlatData::Bool(bm), None)?))
        }
        TraceColumn::StartTime | TraceColumn::EndTime | TraceColumn::LastIngestTime => {
            let mut nulls = Bitmap::new_ones(n);
            let mut vals = Vec::with_capacity(n);
            for (i, acc) in rows.iter().enumerate() {
                match &acc.slots[slot_idx] {
                    Some(Acc::I64(v)) => vals.push(*v),
                    _ => {
                        let _ = nulls.set(i, false);
                        vals.push(0);
                    }
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: vals,
                    utc: true,
                },
                Some(nulls),
            )?))
        }
        TraceColumn::SpanCount
        | TraceColumn::ErrorCount
        | TraceColumn::LlmCount
        | TraceColumn::InputTokens
        | TraceColumn::OutputTokens
        | TraceColumn::CacheReadTokens
        | TraceColumn::CacheWriteTokens
        | TraceColumn::ReasoningTokens
        | TraceColumn::TotalTokens
        | TraceColumn::PartialCount => {
            let mut nulls = Bitmap::new_ones(n);
            let mut vals = Vec::with_capacity(n);
            for (i, acc) in rows.iter().enumerate() {
                match &acc.slots[slot_idx] {
                    Some(Acc::I64(v)) => vals.push(*v),
                    _ => {
                        let _ = nulls.set(i, false);
                        vals.push(0);
                    }
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::I64(vals),
                Some(nulls),
            )?))
        }
        TraceColumn::CostTotal => {
            let mut nulls = Bitmap::new_ones(n);
            let mut vals = Vec::with_capacity(n);
            for (i, acc) in rows.iter().enumerate() {
                match &acc.slots[slot_idx] {
                    Some(Acc::Usd(units)) => vals.push(usd_from_units(*units)),
                    _ => {
                        let _ = nulls.set(i, false);
                        vals.push(0.0);
                    }
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::F64(vals),
                Some(nulls),
            )?))
        }
        TraceColumn::Tags | TraceColumn::Models | TraceColumn::Prompts => {
            let mut offsets = Vec::with_capacity(n + 1);
            offsets.push(0);
            let mut items: Vec<&str> = Vec::new();
            for acc in rows {
                if let Some(Acc::StrSet(set)) = &acc.slots[slot_idx] {
                    for s in set {
                        items.push(s.as_str());
                    }
                }
                let end = u32::try_from(items.len()).map_err(|_| ColumnError::Overflow {
                    reason: "list items exceed u32::MAX",
                })?;
                offsets.push(end);
            }
            let child =
                FlatColumn::try_new(FlatData::Utf8(Utf8Data::try_from_slices(items)?), None)?;
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets,
                    values: Box::new(Column::Flat(child)),
                },
                None,
            )?))
        }
        TraceColumn::Metadata => {
            let mut offsets = Vec::with_capacity(n + 1);
            offsets.push(0);
            let mut keys: Vec<&str> = Vec::new();
            let mut vals: Vec<&str> = Vec::new();
            for acc in rows {
                if let Some(Acc::StrMap(map)) = &acc.slots[slot_idx] {
                    for (k, r) in map {
                        keys.push(k.as_str());
                        vals.push(r.value.as_str());
                    }
                }
                let end = u32::try_from(keys.len()).map_err(|_| ColumnError::Overflow {
                    reason: "map entries exceed u32::MAX",
                })?;
                offsets.push(end);
            }
            let keys_data = Utf8Data::try_from_slices(keys)?;
            let vals_data = Utf8Data::try_from_slices(vals)?;
            let map_data = MapData::try_new(offsets, keys_data, vals_data, None)?;
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::MapUtf8(Box::new(map_data)),
                None,
            )?))
        }
        TraceColumn::Status => unreachable!("handled above"),
    }
}

// ---------------------------------------------------------------------------
// Helpers: pruning, bounds, filter analysis, column readers
// ---------------------------------------------------------------------------

fn padded_bounds(bounds: TimeBounds, lookback: i64) -> TimeBounds {
    if bounds.min.is_none() && bounds.max.is_none() {
        return TimeBounds::open();
    }
    TimeBounds {
        min: bounds.min.map(|a| a.saturating_sub(lookback)),
        max: bounds.max.map(|b| b.saturating_add(lookback)),
    }
}

fn widening_slices(a: i64, b: i64) -> Vec<TimeBounds> {
    let raw = [
        (b.saturating_sub(MIN15), b),
        (
            b.saturating_sub(HOUR1),
            b.saturating_sub(MIN15).saturating_sub(1),
        ),
        (
            b.saturating_sub(HOUR6),
            b.saturating_sub(HOUR1).saturating_sub(1),
        ),
        (
            b.saturating_sub(HOUR24),
            b.saturating_sub(HOUR6).saturating_sub(1),
        ),
        (a, b.saturating_sub(HOUR24).saturating_sub(1)),
    ];
    let mut out = Vec::with_capacity(raw.len());
    for (lo, hi) in raw {
        let lo = lo.max(a);
        let hi = hi.min(b);
        if lo > hi {
            break;
        }
        out.push(TimeBounds {
            min: Some(lo),
            max: Some(hi),
        });
    }
    out
}

fn prune_trace_files(
    files: &[Arc<FileMeta>],
    padded: TimeBounds,
    around_ns: Option<i64>,
) -> Vec<Arc<FileMeta>> {
    let day_start = padded
        .min
        .and_then(day_of_ns)
        .unwrap_or(NaiveDate::MIN)
        .pred_opt()
        .unwrap_or(NaiveDate::MIN);
    let day_end = padded
        .max
        .and_then(day_of_ns)
        .unwrap_or(NaiveDate::MAX)
        .succ_opt()
        .unwrap_or(NaiveDate::MAX);
    let (day_start, day_end) = if day_start > day_end {
        (NaiveDate::MIN, NaiveDate::MAX)
    } else {
        (day_start, day_end)
    };

    let around_window = around_ns.map(|t| (t.saturating_sub(DAY_NS), t.saturating_add(DAY_NS)));

    files
        .iter()
        .filter(|file| {
            if file.rows == 0 {
                return false;
            }
            if file.day < day_start || file.day > day_end {
                return false;
            }
            if let Some(min) = padded.min
                && file.max_time < min
            {
                return false;
            }
            if let Some(max) = padded.max
                && file.min_time > max
            {
                return false;
            }
            if let Some((lo, hi)) = around_window
                && (file.max_time < lo || file.min_time > hi)
            {
                return false;
            }
            true
        })
        .cloned()
        .collect()
}

fn select_mem_batches(
    snapshot: &MemSnapshot,
    project: ProjectId,
    padded: TimeBounds,
) -> Vec<MemBatch> {
    let mut selected = Vec::new();
    for (key, batches) in &snapshot.partitions {
        if key.project != project {
            continue;
        }
        for batch in batches {
            if let Some(min) = padded.min
                && batch.max_time < min
            {
                continue;
            }
            if let Some(max) = padded.max
                && batch.min_time > max
            {
                continue;
            }
            selected.push(batch.clone());
        }
    }
    selected
}

fn build_partial_filters(
    schema: &Schema,
    padded: TimeBounds,
    trace_ids: Option<&BTreeSet<[u8; 16]>>,
) -> Result<Vec<Expr>, QueryError> {
    let mut exprs = Vec::new();
    let start_idx = schema
        .index_of(traces::START_TIME)
        .ok_or_else(|| QueryError::internal("traces schema missing start_time"))?;
    let start_col = Expr::Column {
        index: start_idx,
        name: traces::START_TIME.to_owned(),
        data_type: DataType::timestamp_utc(),
    };
    if let Some(min) = padded.min {
        exprs.push(Expr::Binary {
            left: Box::new(start_col.clone()),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                value: min,
                utc: true,
            })),
            data_type: DataType::Bool,
        });
    }
    if let Some(max) = padded.max {
        exprs.push(Expr::Binary {
            left: Box::new(start_col),
            op: BinaryOp::LtEq,
            right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                value: max,
                utc: true,
            })),
            data_type: DataType::Bool,
        });
    }
    if let Some(ids) = trace_ids {
        let trace_idx = schema
            .index_of(traces::TRACE_ID)
            .ok_or_else(|| QueryError::internal("traces schema missing trace_id"))?;
        let trace_col = Expr::Column {
            index: trace_idx,
            name: traces::TRACE_ID.to_owned(),
            data_type: DataType::FixedBytes16,
        };
        if ids.is_empty() {
            exprs.push(Expr::Literal(ScalarValue::Bool(false)));
        } else if ids.len() == 1
            && let Some(only) = ids.iter().next()
        {
            exprs.push(Expr::Binary {
                left: Box::new(trace_col),
                op: BinaryOp::Eq,
                right: Box::new(Expr::Literal(ScalarValue::FixedBytes16(*only))),
                data_type: DataType::Bool,
            });
        } else {
            let list = ids
                .iter()
                .map(|id| Expr::Literal(ScalarValue::FixedBytes16(*id)))
                .collect();
            exprs.push(Expr::InList {
                expr: Box::new(trace_col),
                list,
                negated: false,
                redacted: false,
            });
        }
    }
    Ok(exprs)
}

fn lookup_trace_ids_in_filters(filters: &[Expr]) -> Option<BTreeSet<[u8; 16]>> {
    let mut found: Option<BTreeSet<[u8; 16]>> = None;
    for filter in filters {
        if let Some(ids) = conjunct_trace_ids(filter) {
            found = Some(match found {
                None => ids,
                Some(prev) => prev.intersection(&ids).copied().collect(),
            });
        }
    }
    found
}

fn conjunct_trace_ids(expr: &Expr) -> Option<BTreeSet<[u8; 16]>> {
    match expr {
        Expr::Binary {
            left,
            op: BinaryOp::And,
            right,
            ..
        } => match (conjunct_trace_ids(left), conjunct_trace_ids(right)) {
            (Some(l), Some(r)) => Some(l.intersection(&r).copied().collect()),
            (Some(ids), None) | (None, Some(ids)) => Some(ids),
            (None, None) => None,
        },
        Expr::Binary {
            left,
            op: BinaryOp::Eq,
            right,
            ..
        } => {
            let lit = match (is_trace_id_col(left), is_trace_id_col(right)) {
                (true, false) => right,
                (false, true) => left,
                _ => return None,
            };
            let id = expr_fixed16(lit)?;
            Some(std::iter::once(id).collect())
        }
        Expr::InList {
            expr,
            list,
            negated: false,
            ..
        } if is_trace_id_col(expr) => {
            let mut ids = BTreeSet::new();
            for item in list {
                ids.insert(expr_fixed16(item)?);
            }
            Some(ids)
        }
        _ => None,
    }
}

fn is_trace_id_col(expr: &Expr) -> bool {
    matches!(expr, Expr::Column { name, .. } if name == traces::TRACE_ID)
}

fn expr_fixed16(expr: &Expr) -> Option<[u8; 16]> {
    match expr {
        Expr::Literal(ScalarValue::FixedBytes16(b)) => Some(*b),
        Expr::Literal(ScalarValue::Binary(b)) if b.len() == 16 => b.as_slice().try_into().ok(),
        Expr::Cast {
            expr,
            target: DataType::FixedBytes16,
            ..
        } => expr_fixed16(expr),
        _ => None,
    }
}

fn requested_col_names(
    schema: &Schema,
    projection: Option<&[usize]>,
) -> Result<Vec<String>, QueryError> {
    match projection {
        None => Ok(schema.fields().iter().map(|f| f.name.clone()).collect()),
        Some(indices) => {
            let mut out = Vec::with_capacity(indices.len());
            for &idx in indices {
                let field = schema.fields().get(idx).ok_or(ColumnError::InvalidBatch {
                    reason: "projection index out of bounds",
                })?;
                out.push(field.name.clone());
            }
            Ok(out)
        }
    }
}

fn filter_col_names(schema: &Schema, filters: &[Expr]) -> Vec<String> {
    let mut set = BTreeSet::new();
    for filter in filters {
        filter.referenced_columns(&mut set);
    }
    set.into_iter()
        .filter_map(|idx| schema.fields().get(idx).map(|f| f.name.clone()))
        .collect()
}

fn str_refs(names: &[String]) -> Vec<&str> {
    names.iter().map(String::as_str).collect()
}

fn indices_of_names(schema: &Schema, names: &[&str]) -> Result<Vec<usize>, QueryError> {
    names
        .iter()
        .map(|name| {
            schema
                .index_of(name)
                .ok_or_else(|| QueryError::internal(format!("column {name} missing from schema")))
        })
        .collect()
}

fn for_each_active_row(
    batch: &Batch,
    mut f: impl FnMut(usize) -> Result<(), QueryError>,
) -> Result<(), QueryError> {
    let raw_len = batch.raw_len();
    match batch.selection() {
        Selection::All => {
            for row in 0..raw_len {
                f(row)?;
            }
        }
        Selection::Mask(mask) => {
            for row in mask.iter_ones() {
                f(row)?;
            }
        }
        Selection::Indices(indices) => {
            for &row in indices {
                f(row as usize)?;
            }
        }
    }
    Ok(())
}

fn read_i64_at(col: &Column, row: usize) -> Option<i64> {
    match col {
        Column::Flat(f) => {
            if f.is_null(row) {
                return None;
            }
            match f.data() {
                FlatData::I64(v) => v.get(row).copied(),
                _ => None,
            }
        }
        Column::Const { value, len } => {
            if row >= *len {
                return None;
            }
            match value {
                ScalarValue::I64(v) => Some(*v),
                _ => None,
            }
        }
        Column::Dict(d) => {
            if d.is_null(row) {
                return None;
            }
            let code = *d.codes().get(row)? as usize;
            if d.values().is_null(code) {
                return None;
            }
            match d.values().data() {
                FlatData::I64(v) => v.get(code).copied(),
                _ => None,
            }
        }
        Column::Rle(r) => {
            if row >= r.len() {
                return None;
            }
            let phys = r.run_ends().partition_point(|&e| (e as usize) <= row);
            if r.values().is_null(phys) {
                return None;
            }
            match r.values().data() {
                FlatData::I64(v) => v.get(phys).copied(),
                _ => None,
            }
        }
    }
}

fn read_ts_at(col: &Column, row: usize) -> Option<i64> {
    match col {
        Column::Flat(f) => {
            if f.is_null(row) {
                return None;
            }
            match f.data() {
                FlatData::TimestampNs { values, .. } => values.get(row).copied(),
                FlatData::I64(v) => v.get(row).copied(),
                _ => None,
            }
        }
        Column::Const { value, len } => {
            if row >= *len {
                return None;
            }
            match value {
                ScalarValue::TimestampNs { value, .. } => Some(*value),
                ScalarValue::I64(v) => Some(*v),
                _ => None,
            }
        }
        Column::Dict(d) => {
            if d.is_null(row) {
                return None;
            }
            let code = *d.codes().get(row)? as usize;
            if d.values().is_null(code) {
                return None;
            }
            match d.values().data() {
                FlatData::TimestampNs { values, .. } => values.get(code).copied(),
                FlatData::I64(v) => v.get(code).copied(),
                _ => None,
            }
        }
        Column::Rle(r) => {
            if row >= r.len() {
                return None;
            }
            let phys = r.run_ends().partition_point(|&e| (e as usize) <= row);
            if r.values().is_null(phys) {
                return None;
            }
            match r.values().data() {
                FlatData::TimestampNs { values, .. } => values.get(phys).copied(),
                FlatData::I64(v) => v.get(phys).copied(),
                _ => None,
            }
        }
    }
}

fn read_f64_at(col: &Column, row: usize) -> Option<f64> {
    match col {
        Column::Flat(f) => {
            if f.is_null(row) {
                return None;
            }
            match f.data() {
                FlatData::F64(v) => v.get(row).copied(),
                _ => None,
            }
        }
        Column::Const { value, len } => {
            if row >= *len {
                return None;
            }
            match value {
                ScalarValue::F64(v) => Some(*v),
                _ => None,
            }
        }
        Column::Dict(d) => {
            if d.is_null(row) {
                return None;
            }
            let code = *d.codes().get(row)? as usize;
            if d.values().is_null(code) {
                return None;
            }
            match d.values().data() {
                FlatData::F64(v) => v.get(code).copied(),
                _ => None,
            }
        }
        Column::Rle(r) => {
            if row >= r.len() {
                return None;
            }
            let phys = r.run_ends().partition_point(|&e| (e as usize) <= row);
            if r.values().is_null(phys) {
                return None;
            }
            match r.values().data() {
                FlatData::F64(v) => v.get(phys).copied(),
                _ => None,
            }
        }
    }
}

fn read_fixed8_at(col: &Column, row: usize) -> Option<[u8; 8]> {
    match col {
        Column::Flat(f) => {
            if f.is_null(row) {
                return None;
            }
            match f.data() {
                FlatData::FixedBytes8(v) => v.get(row).copied(),
                _ => None,
            }
        }
        Column::Const { value, len } => {
            if row >= *len {
                return None;
            }
            match value {
                ScalarValue::FixedBytes8(b) => Some(*b),
                _ => None,
            }
        }
        Column::Dict(d) => {
            if d.is_null(row) {
                return None;
            }
            let code = *d.codes().get(row)? as usize;
            if d.values().is_null(code) {
                return None;
            }
            match d.values().data() {
                FlatData::FixedBytes8(v) => v.get(code).copied(),
                _ => None,
            }
        }
        Column::Rle(r) => {
            if row >= r.len() {
                return None;
            }
            let phys = r.run_ends().partition_point(|&e| (e as usize) <= row);
            if r.values().is_null(phys) {
                return None;
            }
            match r.values().data() {
                FlatData::FixedBytes8(v) => v.get(phys).copied(),
                _ => None,
            }
        }
    }
}

fn read_str_list_at(col: &Column, row: usize) -> Option<Vec<String>> {
    match col {
        Column::Flat(f) => {
            if f.is_null(row) {
                return None;
            }
            match f.data() {
                FlatData::List {
                    offsets, values, ..
                } => {
                    let start = *offsets.get(row)? as usize;
                    let end = *offsets.get(row + 1)? as usize;
                    let mut out = Vec::with_capacity(end.saturating_sub(start));
                    for idx in start..end {
                        if let Some(s) = read_str_at(values, idx) {
                            out.push(s.to_owned());
                        }
                    }
                    Some(out)
                }
                _ => None,
            }
        }
        Column::Const { value, len } => {
            if row >= *len {
                return None;
            }
            match value {
                ScalarValue::List { values, .. } => {
                    let mut out = Vec::new();
                    for v in values {
                        if let ScalarValue::Utf8(s) = v {
                            out.push(s.clone());
                        }
                    }
                    Some(out)
                }
                _ => None,
            }
        }
        Column::Dict(d) => {
            if d.is_null(row) {
                return None;
            }
            let code = *d.codes().get(row)? as usize;
            if d.values().is_null(code) {
                return None;
            }
            match d.values().data() {
                FlatData::List {
                    offsets, values, ..
                } => {
                    let start = *offsets.get(code)? as usize;
                    let end = *offsets.get(code + 1)? as usize;
                    let mut out = Vec::with_capacity(end.saturating_sub(start));
                    for idx in start..end {
                        if let Some(s) = read_str_at(values, idx) {
                            out.push(s.to_owned());
                        }
                    }
                    Some(out)
                }
                _ => None,
            }
        }
        Column::Rle(r) => {
            if row >= r.len() {
                return None;
            }
            let phys = r.run_ends().partition_point(|&e| (e as usize) <= row);
            if r.values().is_null(phys) {
                return None;
            }
            match r.values().data() {
                FlatData::List {
                    offsets, values, ..
                } => {
                    let start = *offsets.get(phys)? as usize;
                    let end = *offsets.get(phys + 1)? as usize;
                    let mut out = Vec::with_capacity(end.saturating_sub(start));
                    for idx in start..end {
                        if let Some(s) = read_str_at(values, idx) {
                            out.push(s.to_owned());
                        }
                    }
                    Some(out)
                }
                _ => None,
            }
        }
    }
}

fn read_str_map_at(col: &Column, row: usize) -> Option<Vec<(String, String)>> {
    let entries = match col {
        Column::Flat(f) => {
            if f.is_null(row) {
                return None;
            }
            match f.data() {
                FlatData::MapUtf8(m) => m.get(row)?,
                _ => return None,
            }
        }
        Column::Const { value, len } => {
            if row >= *len {
                return None;
            }
            match value {
                ScalarValue::MapUtf8(entries) => entries.clone(),
                _ => return None,
            }
        }
        Column::Dict(d) => {
            if d.is_null(row) {
                return None;
            }
            let code = *d.codes().get(row)? as usize;
            if d.values().is_null(code) {
                return None;
            }
            match d.values().data() {
                FlatData::MapUtf8(m) => m.get(code)?,
                _ => return None,
            }
        }
        Column::Rle(r) => {
            if row >= r.len() {
                return None;
            }
            let phys = r.run_ends().partition_point(|&e| (e as usize) <= row);
            if r.values().is_null(phys) {
                return None;
            }
            match r.values().data() {
                FlatData::MapUtf8(m) => m.get(phys)?,
                _ => return None,
            }
        }
    };
    Some(
        entries
            .into_iter()
            .filter_map(|(k, v)| v.map(|val| (k, val)))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use cetra_column::GlobalPool;

    struct VecBatchesStream {
        batches: VecDeque<Batch>,
    }

    impl PartitionStream for VecBatchesStream {
        fn next_batch(
            &mut self,
            _cancel: &CancelToken,
            res: Option<&mut Reservation>,
        ) -> Result<Option<Batch>, QueryError> {
            let Some(batch) = self.batches.pop_front() else {
                return Ok(None);
            };
            if let Some(r) = res {
                r.try_grow(batch.memory_bytes())
                    .map_err(|_| QueryError::TooLarge)?;
            }
            Ok(Some(batch))
        }
    }

    fn make_minimal_partial_batch(schema: &Arc<Schema>, rows: &[([u8; 16], i64)]) -> Batch {
        let mut trace_ids = Vec::with_capacity(rows.len());
        let mut starts = Vec::with_capacity(rows.len());
        let mut has_roots = Bitmap::new_zeroed(rows.len());
        let mut root_span_ids = Vec::with_capacity(rows.len());
        for (i, &(tid, ts)) in rows.iter().enumerate() {
            trace_ids.push(tid);
            starts.push(ts);
            let _ = has_roots.set(i, false);
            root_span_ids.push([0u8; 8]);
        }
        Batch::try_new_all(
            Arc::clone(schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(trace_ids), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: starts,
                            utc: true,
                        },
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(FlatColumn::try_new(FlatData::Bool(has_roots), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::FixedBytes8(root_span_ids),
                        Some(Bitmap::new_zeroed(rows.len())),
                    )
                    .unwrap(),
                ),
            ],
            rows.len(),
        )
        .unwrap()
    }

    #[test]
    fn pass26_native_trace_merge_stream_batched_reconcile_parity_and_tight_oom() {
        let full_schema = Schema::try_new_arc(vec![
            cetra_column::Field::new(traces::TRACE_ID, DataType::FixedBytes16, false),
            cetra_column::Field::new(
                traces::START_TIME,
                DataType::TimestampNs { utc: true },
                false,
            ),
            cetra_column::Field::new(traces::HAS_ROOT, DataType::Bool, false),
            cetra_column::Field::new(traces::ROOT_SPAN_ID, DataType::FixedBytes8, true),
        ])
        .unwrap();
        let merge_columns = MergeColumns::new(&[traces::TRACE_ID, traces::START_TIME]);

        let make_rows = |count: usize, base: u32| -> Vec<([u8; 16], i64)> {
            let mut out = Vec::with_capacity(count * 2);
            for i in 0..count {
                let mut tid = [0u8; 16];
                tid[12..16].copy_from_slice(&(base + i as u32).to_be_bytes());
                // Two partial rows per trace_id with different start_time -> merged min(start_time).
                out.push((tid, 1_000 + (i as i64) * 10 + 5));
                out.push((tid, 1_000 + (i as i64) * 10));
            }
            out
        };

        // 1. Batched reconcile parity across > 64 KiB of trace rows and multiple batches:
        //    verifies exact merged row count, min(start_time) merge, and 0 leaked reservation bytes.
        let global = GlobalPool::new(16 * 1024 * 1024);
        let pool = global.try_admit(0, 16 * 1024 * 1024).unwrap();
        let cancel = CancelToken::new();
        let b1 = make_minimal_partial_batch(&full_schema, &make_rows(200, 0));
        let b2 = make_minimal_partial_batch(&full_schema, &make_rows(200, 200));
        let mut stream = NativeTraceMergeStream {
            input: Box::new(VecBatchesStream {
                batches: VecDeque::from([b1, b2]),
            }),
            input_res: pool.register("in", false),
            merger: NativeTraceMerger::new(&merge_columns, &full_schema).unwrap(),
            merger_res: pool.register("merger", false),
            scratch_res: pool.register("scratch", false),
            merge_columns: merge_columns.clone(),
            full_schema: Arc::clone(&full_schema),
            merged_start_idx: 1,
            bounds: TimeBounds::default(),
            post_filters: Vec::new(),
            projection: None,
            remaining: None,
            eval: EvalContext::new(0, cancel.clone()),
            active_input: None,
            current_trace_id: None,
            budget_bytes: 256 * 1024,
            max_traces_per_chunk: 500,
            done: false,
        };

        let mut out_res = pool.register("out", false);
        let mut total_merged = 0usize;
        while let Some(batch) = stream.next_batch(&cancel, Some(&mut out_res)).unwrap() {
            total_merged += batch.num_rows();
            assert_eq!(out_res.size(), batch.memory_bytes());
            out_res.free();
        }
        assert_eq!(total_merged, 400);
        assert_eq!(stream.merger_res.size(), 0);
        assert_eq!(stream.input_res.size(), 0);
        assert_eq!(stream.scratch_res.size(), 0);
        assert_eq!(global.used(), 0);

        // 2. Tight-budget OOM test: pool cap allows reading one small input batch (~4 KiB)
        //    but cannot fit the merger's post-loop exact reconciliation -> must return
        //    TooLarge and release ALL reservations cleanly (global.used() == 0).
        let small_batch = make_minimal_partial_batch(&full_schema, &make_rows(80, 0));
        let batch_mem = small_batch.memory_bytes();
        let tight_global = GlobalPool::new(batch_mem + 256);
        let tight_pool = tight_global.try_admit(0, batch_mem + 256).unwrap();
        let mut tight_stream = NativeTraceMergeStream {
            input: Box::new(VecBatchesStream {
                batches: VecDeque::from([small_batch]),
            }),
            input_res: tight_pool.register("in", false),
            merger: NativeTraceMerger::new(&merge_columns, &full_schema).unwrap(),
            merger_res: tight_pool.register("merger", false),
            scratch_res: tight_pool.register("scratch", false),
            merge_columns,
            full_schema: Arc::clone(&full_schema),
            merged_start_idx: 1,
            bounds: TimeBounds::default(),
            post_filters: Vec::new(),
            projection: None,
            remaining: None,
            eval: EvalContext::new(0, cancel.clone()),
            active_input: None,
            current_trace_id: None,
            budget_bytes: 256 * 1024,
            max_traces_per_chunk: 500,
            done: false,
        };
        let mut tight_out = tight_pool.register("out", false);
        let err = tight_stream
            .next_batch(&cancel, Some(&mut tight_out))
            .unwrap_err();
        assert!(
            matches!(err, QueryError::TooLarge),
            "expected TooLarge under tight pool, got {err:?}"
        );
        assert_eq!(tight_out.size(), 0);
        assert_eq!(tight_stream.merger_res.size(), 0);
        assert_eq!(tight_stream.input_res.size(), 0);
        assert_eq!(tight_stream.scratch_res.size(), 0);
        assert_eq!(tight_global.used(), 0);
    }
}
