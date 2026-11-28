//! Native spillable `SortedTraceMerger` and `TracesSource` (`mode=lookup`, `mode=list`,
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

        let active_p = sorted_streams.len().max(1);
        let budget_bytes = (ctx.pool.spill_share() / (4 * active_p)).max(4096);
        let max_traces_per_chunk = (ctx.pool.cap() / (4096 * active_p)).clamp(8, MERGE_CHUNK_ROWS);
        let shared_limit = limit.map(|l| Arc::new(AtomicUsize::new(l)));

        let streams = sorted_streams
            .into_iter()
            .map(|input| {
                let merger = SortedTraceMerger::new(merge_columns, &self.schema)?
                    .with_bounds(bounds)
                    .with_initial_cap(max_traces_per_chunk);
                Ok(Box::new(NativeTraceMergeStream {
                    input,
                    input_res: ctx.pool.register("trace_merge_in", false),
                    merger,
                    merger_res: ctx.pool.register("trace_merger", false),
                    scratch_res: ctx.pool.register("trace_merge_scratch", false),
                    post_filters: post_filters.clone(),
                    projection: merged_to_output.clone(),
                    remaining: shared_limit.clone(),
                    eval: ctx.eval.clone(),
                    active_input: None,
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
    merger: SortedTraceMerger,
    merger_res: Reservation,
    scratch_res: Reservation,
    post_filters: Vec<Expr>,
    projection: Option<Vec<usize>>,
    remaining: Option<Arc<AtomicUsize>>,
    eval: EvalContext,
    active_input: Option<(Batch, usize)>,
    budget_bytes: usize,
    max_traces_per_chunk: usize,
    done: bool,
}

impl NativeTraceMergeStream {
    fn abort_and_free(&mut self, res: Option<&mut Reservation>, before: usize) {
        self.done = true;
        self.active_input = None;
        self.merger.clear();
        self.scratch_res.free();
        self.merger_res.free();
        self.input_res.free();
        rollback_res(res, before);
    }

    fn finish_and_filter_merger(
        &mut self,
        eval_ctx: &EvalContext,
    ) -> Result<Option<Batch>, QueryError> {
        if self.merger.flushed_rows() == 0 {
            self.merger_res.free();
            return Ok(None);
        }
        let mut merged = self.merger.take_batch()?;
        let merged_bytes = merged.memory_bytes();
        let transfer = transfer_or_grow(&mut self.merger_res, &mut self.scratch_res, merged_bytes);
        self.merger_res.free();
        transfer?;

        // 1. Evaluate all post-merge user filters (bounds already filtered pre-builder).
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

        // 2. Apply shared `limit`.
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
                self.merger.clear();
            }
            if take == 0 {
                self.done = true;
                self.active_input = None;
                self.input_res.free();
                self.merger.clear();
                self.scratch_res.free();
                return Ok(None);
            }
            if take < merged.num_rows() {
                self.done = true;
                self.active_input = None;
                self.input_res.free();
                self.merger.clear();
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

        // 3. Project to requested output schema.
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
                self.abort_and_free(res, before);
                return Err(QueryError::Cancelled);
            }
            if self.done
                || self
                    .remaining
                    .as_ref()
                    .is_some_and(|r| r.load(Ordering::Acquire) == 0)
            {
                self.abort_and_free(res, before);
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
                                        self.abort_and_free(res, before);
                                        return Err(err);
                                    }
                                    c
                                }
                                Err(err) => {
                                    self.abort_and_free(res, before);
                                    return Err(err.into());
                                }
                            }
                        };
                        self.active_input = Some((compacted, 0));
                    }
                    Ok(None) => {
                        self.done = true;
                        self.input_res.free();
                        if let Err(err) = self.merger.flush_pending() {
                            self.abort_and_free(res, before);
                            return Err(err);
                        }
                        if self.merger.is_empty() {
                            self.abort_and_free(res, before);
                            return Ok(None);
                        }
                        match self.finish_and_filter_merger(&eval_ctx) {
                            Ok(Some(out)) => {
                                let out_bytes = out.memory_bytes();
                                if let Err(err) = handoff_res(
                                    &mut self.scratch_res,
                                    res.as_deref_mut(),
                                    before,
                                    out_bytes,
                                ) {
                                    self.abort_and_free(res, before);
                                    return Err(err);
                                }
                                self.scratch_res.free();
                                return Ok(Some(out));
                            }
                            Ok(None) => {
                                self.scratch_res.free();
                                rollback_res(res, before);
                                return Ok(None);
                            }
                            Err(err) => {
                                self.abort_and_free(res, before);
                                return Err(err);
                            }
                        }
                    }
                    Err(err) => {
                        self.abort_and_free(res, before);
                        return Err(err);
                    }
                }
            }

            // Process contiguous trace_id runs from `active_input` up to chunk/memory budget.
            let Some((batch, mut offset)) = self.active_input.take() else {
                continue;
            };
            let total = batch.raw_len();
            let reconcile_threshold = self.budget_bytes.min(64 * 1024);
            let ready_to_emit = match self.merger.consume_batch_chunk(
                &batch,
                &mut offset,
                total,
                Some(ChunkBudget {
                    budget_bytes: self.budget_bytes,
                    max_traces: self.max_traces_per_chunk,
                    merger_res: &mut self.merger_res,
                    reconcile_threshold,
                    cancel,
                }),
            ) {
                Ok(ready) => ready,
                Err(err) => {
                    drop(batch);
                    self.abort_and_free(res, before);
                    return Err(err);
                }
            };

            if self.merger.size() != self.merger_res.size()
                && let Err(err) = reconcile_res(Some(&mut self.merger_res), 0, self.merger.size())
            {
                drop(batch);
                self.abort_and_free(res, before);
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
                        if let Err(err) = handoff_res(
                            &mut self.scratch_res,
                            res.as_deref_mut(),
                            before,
                            out_bytes,
                        ) {
                            self.abort_and_free(res, before);
                            return Err(err);
                        }
                        self.scratch_res.free();
                        return Ok(Some(out));
                    }
                    Ok(None) => {
                        self.scratch_res.free();
                    }
                    Err(err) => {
                        self.abort_and_free(res, before);
                        return Err(err);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// SortedTraceMerger: streaming 30-column rollup matching `crates/storage/src/partials.rs`
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Rank {
    root: u8,
    start_time: i64,
    tie: Option<[u8; 8]>,
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

    fn reset(&mut self) {
        self.slots.fill(None);
    }

    fn update_from_col(
        &mut self,
        slot_idx: usize,
        col_kind: TraceColumn,
        col: &Column,
        row: usize,
        ctx: &RowCtx,
        size: &mut usize,
    ) -> Result<(), QueryError> {
        if col.is_null(row) {
            return Ok(());
        }
        let slot = &mut self.slots[slot_idx];
        match col_kind {
            TraceColumn::TraceId => {
                let id = read_fixed16_at(col, row)
                    .ok_or_else(|| QueryError::internal("trace_id column type mismatch"))?;
                *slot = Some(Acc::Fixed16(id));
            }
            TraceColumn::ProjectId
            | TraceColumn::Name
            | TraceColumn::SessionId
            | TraceColumn::UserId
            | TraceColumn::Environment
            | TraceColumn::Release => {
                let text = read_str_at(col, row)
                    .ok_or_else(|| QueryError::internal("string column type mismatch"))?;
                ranked_str_slot(slot, text, ctx.rank, size);
            }
            TraceColumn::InputPreview | TraceColumn::OutputPreview => {
                if ctx.preview {
                    let text = read_str_at(col, row)
                        .ok_or_else(|| QueryError::internal("preview column type mismatch"))?;
                    ranked_str_slot(slot, text, ctx.rank, size);
                }
            }
            TraceColumn::Status => {
                if ctx.root {
                    let text = read_str_at(col, row)
                        .ok_or_else(|| QueryError::internal("status column type mismatch"))?;
                    ranked_str_slot(slot, text, ctx.rank, size);
                }
            }
            TraceColumn::RootSpanId => {
                if ctx.root {
                    let bytes = read_fixed8_at(col, row)
                        .ok_or_else(|| QueryError::internal("root_span_id column type mismatch"))?;
                    let candidate = Ranked {
                        rank: ctx.rank,
                        value: bytes,
                    };
                    match slot {
                        Some(Acc::RankedFixed8(kept)) => {
                            if candidate < *kept {
                                *kept = candidate;
                            }
                        }
                        _ => {
                            *slot = Some(Acc::RankedFixed8(candidate));
                            *size = size.saturating_add(8);
                        }
                    }
                }
            }
            TraceColumn::HasRoot => {
                let val = read_bool_at(col, row)
                    .ok_or_else(|| QueryError::internal("has_root column type mismatch"))?;
                match slot {
                    Some(Acc::Bool(kept)) => *kept |= val,
                    _ => *slot = Some(Acc::Bool(val)),
                }
            }
            TraceColumn::StartTime => {
                let ts = read_ts_at(col, row)
                    .ok_or_else(|| QueryError::internal("start_time column type mismatch"))?;
                match slot {
                    Some(Acc::I64(kept)) => *kept = (*kept).min(ts),
                    _ => *slot = Some(Acc::I64(ts)),
                }
            }
            TraceColumn::EndTime | TraceColumn::LastIngestTime => {
                let ts = read_ts_at(col, row)
                    .ok_or_else(|| QueryError::internal("timestamp column type mismatch"))?;
                match slot {
                    Some(Acc::I64(kept)) => *kept = (*kept).max(ts),
                    _ => *slot = Some(Acc::I64(ts)),
                }
            }
            TraceColumn::Tags | TraceColumn::Models | TraceColumn::Prompts => {
                let visited = visit_str_list(col, row, |s| match slot {
                    Some(Acc::StrSet(set)) => {
                        if !set.contains(s) {
                            *size = size.saturating_add(s.len());
                            set.insert(s.to_owned());
                        }
                    }
                    _ => {
                        let mut set = BTreeSet::new();
                        *size = size.saturating_add(s.len());
                        set.insert(s.to_owned());
                        *slot = Some(Acc::StrSet(set));
                    }
                })?;
                if visited && slot.is_none() {
                    *slot = Some(Acc::StrSet(BTreeSet::new()));
                }
            }
            TraceColumn::Metadata => {
                let visited = visit_str_map(col, row, |key, value| {
                    if !matches!(slot, Some(Acc::StrMap(_))) {
                        *slot = Some(Acc::StrMap(BTreeMap::new()));
                    }
                    let Some(Acc::StrMap(map)) = slot else {
                        return;
                    };
                    match map.get_mut(key) {
                        Some(kept) => {
                            if (ctx.rank, value) < (kept.rank, kept.value.as_str()) {
                                let old_len = kept.value.len();
                                kept.rank = ctx.rank;
                                kept.value = value.to_owned();
                                *size = size.saturating_sub(old_len).saturating_add(value.len());
                            }
                        }
                        None => {
                            *size = size.saturating_add(key.len()).saturating_add(value.len());
                            map.insert(
                                key.to_owned(),
                                Ranked {
                                    rank: ctx.rank,
                                    value: value.to_owned(),
                                },
                            );
                        }
                    }
                })?;
                if visited && slot.is_none() {
                    *slot = Some(Acc::StrMap(BTreeMap::new()));
                }
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
            }
            TraceColumn::CostTotal => {
                let val = read_f64_at(col, row)
                    .ok_or_else(|| QueryError::internal("cost_total column type mismatch"))?;
                let units = usd_units(val);
                match slot {
                    Some(Acc::Usd(kept)) => *kept = kept.saturating_add(units),
                    _ => *slot = Some(Acc::Usd(units)),
                }
            }
        }
        Ok(())
    }
}

fn ranked_str_slot(slot: &mut Option<Acc>, text: &str, rank: Rank, size: &mut usize) {
    match slot {
        Some(Acc::RankedStr(kept)) => {
            if (rank, text) < (kept.rank, kept.value.as_str()) {
                let old_len = kept.value.len();
                kept.rank = rank;
                kept.value = text.to_owned();
                *size = size.saturating_sub(old_len).saturating_add(text.len());
            }
        }
        _ => {
            *slot = Some(Acc::RankedStr(Ranked {
                rank,
                value: text.to_owned(),
            }));
            *size = size.saturating_add(text.len());
        }
    }
}

fn usd_units(value: f64) -> i128 {
    (value * USD_UNITS).round() as i128
}

fn usd_from_units(units: i128) -> f64 {
    units as f64 / USD_UNITS
}

fn col_matches_trace_type(col: TraceColumn, dt: &DataType) -> bool {
    match col {
        TraceColumn::TraceId => *dt == DataType::FixedBytes16,
        TraceColumn::StartTime => matches!(dt, DataType::TimestampNs { .. } | DataType::I64),
        TraceColumn::HasRoot => *dt == DataType::Bool,
        TraceColumn::RootSpanId => matches!(dt, DataType::FixedBytes8 | DataType::Null),
        TraceColumn::EndTime | TraceColumn::LastIngestTime => {
            matches!(
                dt,
                DataType::TimestampNs { .. } | DataType::I64 | DataType::Null
            )
        }
        TraceColumn::ProjectId
        | TraceColumn::Name
        | TraceColumn::Status
        | TraceColumn::SessionId
        | TraceColumn::UserId
        | TraceColumn::Environment
        | TraceColumn::Release
        | TraceColumn::InputPreview
        | TraceColumn::OutputPreview => matches!(dt, DataType::Utf8 | DataType::Null),
        TraceColumn::Tags | TraceColumn::Models | TraceColumn::Prompts => {
            matches!(dt, DataType::List(inner) if **inner == DataType::Utf8)
                || *dt == DataType::Null
        }
        TraceColumn::Metadata => matches!(dt, DataType::MapUtf8 | DataType::Null),
        TraceColumn::SpanCount
        | TraceColumn::ErrorCount
        | TraceColumn::LlmCount
        | TraceColumn::InputTokens
        | TraceColumn::OutputTokens
        | TraceColumn::CacheReadTokens
        | TraceColumn::CacheWriteTokens
        | TraceColumn::ReasoningTokens
        | TraceColumn::TotalTokens
        | TraceColumn::PartialCount => matches!(dt, DataType::I64 | DataType::Null),
        TraceColumn::CostTotal => matches!(dt, DataType::F64 | DataType::Null),
    }
}

struct BatchAccessors<'a> {
    cols: [Option<&'a Column>; TraceColumn::COUNT],
    start_col: &'a Column,
    has_root_col: &'a Column,
    root_span_col: &'a Column,
    trace_id_col: &'a Column,
    flat_trace_ids: Option<&'a [[u8; 16]]>,
    flat_start_times: Option<&'a [i64]>,
    flat_has_roots: Option<&'a Bitmap>,
}

impl<'a> BatchAccessors<'a> {
    fn resolve(batch: &'a Batch, input_cols: &[TraceColumn]) -> Result<Self, QueryError> {
        if !batch.selection().is_all(batch.raw_len()) {
            return Err(ColumnError::InvalidBatch {
                reason: "SortedTraceMerger requires Selection::All",
            }
            .into());
        }
        let mut cols: [Option<&'a Column>; TraceColumn::COUNT] = [None; TraceColumn::COUNT];
        for &col_kind in input_cols {
            let name = col_kind.name();
            let col = batch.column_by_name(name).ok_or_else(|| {
                QueryError::internal(format!("partial batch missing column {name}"))
            })?;
            if !col_matches_trace_type(col_kind, &col.data_type()) {
                return Err(QueryError::internal(format!(
                    "partial batch column {name} type mismatch"
                )));
            }
            cols[col_kind as usize] = Some(col);
        }

        let resolve_key = |kind: TraceColumn| -> Result<&'a Column, QueryError> {
            if let Some(col) = cols[kind as usize] {
                return Ok(col);
            }
            let name = kind.name();
            let col = batch.column_by_name(name).ok_or_else(|| {
                QueryError::internal(format!("partial batch missing column {name}"))
            })?;
            if !col_matches_trace_type(kind, &col.data_type()) {
                return Err(QueryError::internal(format!(
                    "partial batch column {name} type mismatch"
                )));
            }
            Ok(col)
        };

        let start_col = resolve_key(TraceColumn::StartTime)?;
        let has_root_col = resolve_key(TraceColumn::HasRoot)?;
        let root_span_col = resolve_key(TraceColumn::RootSpanId)?;
        let trace_id_col = resolve_key(TraceColumn::TraceId)?;

        let flat_trace_ids = match trace_id_col {
            Column::Flat(f) if f.nulls().is_none() => match f.data() {
                FlatData::FixedBytes16(v) => Some(v.as_slice()),
                _ => None,
            },
            _ => None,
        };
        let flat_start_times = match start_col {
            Column::Flat(f) if f.nulls().is_none() => match f.data() {
                FlatData::TimestampNs { values, .. } | FlatData::I64(values) => {
                    Some(values.as_slice())
                }
                _ => None,
            },
            _ => None,
        };
        let flat_has_roots = match has_root_col {
            Column::Flat(f) if f.nulls().is_none() => match f.data() {
                FlatData::Bool(bm) => Some(bm),
                _ => None,
            },
            _ => None,
        };

        Ok(Self {
            cols,
            start_col,
            has_root_col,
            root_span_col,
            trace_id_col,
            flat_trace_ids,
            flat_start_times,
            flat_has_roots,
        })
    }

    #[inline]
    fn read_trace_id(&self, row: usize) -> Result<[u8; 16], QueryError> {
        if let Some(ids) = self.flat_trace_ids {
            ids.get(row)
                .copied()
                .ok_or_else(|| QueryError::internal("partial trace_id out of bounds"))
        } else {
            read_fixed16_at(self.trace_id_col, row)
                .ok_or_else(|| QueryError::internal("partial trace_id missing"))
        }
    }

    #[inline]
    fn read_start_time(&self, row: usize) -> Result<i64, QueryError> {
        if let Some(ts) = self.flat_start_times {
            ts.get(row)
                .copied()
                .ok_or_else(|| QueryError::internal("partial start_time out of bounds"))
        } else {
            read_ts_at(self.start_col, row)
                .ok_or_else(|| QueryError::internal("partial start_time missing"))
        }
    }

    #[inline]
    fn read_has_root(&self, row: usize) -> Result<bool, QueryError> {
        if let Some(bm) = self.flat_has_roots {
            if row < bm.len() {
                Ok(bm.get(row))
            } else {
                Err(QueryError::internal("partial has_root out of bounds"))
            }
        } else {
            read_bool_at(self.has_root_col, row)
                .ok_or_else(|| QueryError::internal("partial has_root missing"))
        }
    }
}

fn push_valid_null_bit(nulls: &mut Option<Bitmap>, row_idx: usize, is_valid: bool) {
    if is_valid {
        if let Some(bm) = nulls {
            bm.push(true);
        }
    } else {
        nulls
            .get_or_insert_with(|| Bitmap::new_ones(row_idx))
            .push(false);
    }
}

fn push_list_item_buf(
    bytes: &mut Vec<u8>,
    item_offsets: &mut Vec<u32>,
    s: &str,
) -> Result<(), QueryError> {
    let next_bytes = bytes
        .len()
        .checked_add(s.len())
        .ok_or(ColumnError::Overflow {
            reason: "list utf8 byte length overflow",
        })?;
    let byte_end = u32::try_from(next_bytes).map_err(|_| ColumnError::Overflow {
        reason: "list utf8 byte length exceeds u32::MAX",
    })?;
    let _ = u32::try_from(item_offsets.len()).map_err(|_| ColumnError::Overflow {
        reason: "list items exceed u32::MAX",
    })?;
    bytes.extend_from_slice(s.as_bytes());
    item_offsets.push(byte_end);
    Ok(())
}

fn push_map_entry_buf(
    key_bytes: &mut Vec<u8>,
    key_offsets: &mut Vec<u32>,
    val_bytes: &mut Vec<u8>,
    val_offsets: &mut Vec<u32>,
    k: &str,
    v: &str,
) -> Result<(), QueryError> {
    let next_k = key_bytes
        .len()
        .checked_add(k.len())
        .ok_or(ColumnError::Overflow {
            reason: "map key byte length overflow",
        })?;
    let k_end = u32::try_from(next_k).map_err(|_| ColumnError::Overflow {
        reason: "map key byte length exceeds u32::MAX",
    })?;
    let next_v = val_bytes
        .len()
        .checked_add(v.len())
        .ok_or(ColumnError::Overflow {
            reason: "map value byte length overflow",
        })?;
    let v_end = u32::try_from(next_v).map_err(|_| ColumnError::Overflow {
        reason: "map value byte length exceeds u32::MAX",
    })?;
    let _ = u32::try_from(key_offsets.len()).map_err(|_| ColumnError::Overflow {
        reason: "map entries exceed u32::MAX",
    })?;
    key_bytes.extend_from_slice(k.as_bytes());
    key_offsets.push(k_end);
    val_bytes.extend_from_slice(v.as_bytes());
    val_offsets.push(v_end);
    Ok(())
}

enum ColBuilder {
    Fixed16(Vec<[u8; 16]>),
    Fixed8 {
        vals: Vec<[u8; 8]>,
        nulls: Option<Bitmap>,
    },
    Bool(Bitmap),
    TimestampNs {
        vals: Vec<i64>,
        nulls: Option<Bitmap>,
    },
    I64 {
        vals: Vec<i64>,
        nulls: Option<Bitmap>,
    },
    F64 {
        vals: Vec<f64>,
        nulls: Option<Bitmap>,
    },
    Utf8 {
        offsets: Vec<u32>,
        bytes: Vec<u8>,
        nulls: Option<Bitmap>,
    },
    ListUtf8 {
        offsets: Vec<u32>,
        item_offsets: Vec<u32>,
        bytes: Vec<u8>,
    },
    MapUtf8 {
        offsets: Vec<u32>,
        key_offsets: Vec<u32>,
        key_bytes: Vec<u8>,
        val_offsets: Vec<u32>,
        val_bytes: Vec<u8>,
    },
}

impl ColBuilder {
    fn new(col: TraceColumn) -> Self {
        match col {
            TraceColumn::TraceId => Self::Fixed16(Vec::new()),
            TraceColumn::RootSpanId => Self::Fixed8 {
                vals: Vec::new(),
                nulls: None,
            },
            TraceColumn::HasRoot => Self::Bool(Bitmap::new_zeroed(0)),
            TraceColumn::StartTime | TraceColumn::EndTime | TraceColumn::LastIngestTime => {
                Self::TimestampNs {
                    vals: Vec::new(),
                    nulls: None,
                }
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
            | TraceColumn::PartialCount => Self::I64 {
                vals: Vec::new(),
                nulls: None,
            },
            TraceColumn::CostTotal => Self::F64 {
                vals: Vec::new(),
                nulls: None,
            },
            TraceColumn::ProjectId
            | TraceColumn::Name
            | TraceColumn::Status
            | TraceColumn::SessionId
            | TraceColumn::UserId
            | TraceColumn::Environment
            | TraceColumn::Release
            | TraceColumn::InputPreview
            | TraceColumn::OutputPreview => Self::Utf8 {
                offsets: Vec::new(),
                bytes: Vec::new(),
                nulls: None,
            },
            TraceColumn::Tags | TraceColumn::Models | TraceColumn::Prompts => Self::ListUtf8 {
                offsets: Vec::new(),
                item_offsets: Vec::new(),
                bytes: Vec::new(),
            },
            TraceColumn::Metadata => Self::MapUtf8 {
                offsets: Vec::new(),
                key_offsets: Vec::new(),
                key_bytes: Vec::new(),
                val_offsets: Vec::new(),
                val_bytes: Vec::new(),
            },
        }
    }

    fn clear(&mut self) {
        match self {
            Self::Fixed16(v) => *v = Vec::new(),
            Self::Fixed8 { vals, nulls } => {
                *vals = Vec::new();
                *nulls = None;
            }
            Self::Bool(bm) => *bm = Bitmap::new_zeroed(0),
            Self::TimestampNs { vals, nulls } | Self::I64 { vals, nulls } => {
                *vals = Vec::new();
                *nulls = None;
            }
            Self::F64 { vals, nulls } => {
                *vals = Vec::new();
                *nulls = None;
            }
            Self::Utf8 {
                offsets,
                bytes,
                nulls,
            } => {
                *offsets = Vec::new();
                *bytes = Vec::new();
                *nulls = None;
            }
            Self::ListUtf8 {
                offsets,
                item_offsets,
                bytes,
            } => {
                *offsets = Vec::new();
                *item_offsets = Vec::new();
                *bytes = Vec::new();
            }
            Self::MapUtf8 {
                offsets,
                key_offsets,
                key_bytes,
                val_offsets,
                val_bytes,
            } => {
                *offsets = Vec::new();
                *key_offsets = Vec::new();
                *key_bytes = Vec::new();
                *val_offsets = Vec::new();
                *val_bytes = Vec::new();
            }
        }
    }

    fn memory_bytes(&self) -> usize {
        match self {
            Self::Fixed16(v) => v.len() * 16,
            Self::Fixed8 { vals, nulls } => {
                vals.len() * 8 + nulls.as_ref().map_or(0, Bitmap::memory_bytes)
            }
            Self::Bool(bm) => bm.memory_bytes(),
            Self::TimestampNs { vals, nulls } | Self::I64 { vals, nulls } => {
                vals.len() * 8 + nulls.as_ref().map_or(0, Bitmap::memory_bytes)
            }
            Self::F64 { vals, nulls } => {
                vals.len() * 8 + nulls.as_ref().map_or(0, Bitmap::memory_bytes)
            }
            Self::Utf8 {
                offsets,
                bytes,
                nulls,
            } => {
                if offsets.is_empty() {
                    0
                } else {
                    offsets.len() * 4 + bytes.len() + nulls.as_ref().map_or(0, Bitmap::memory_bytes)
                }
            }
            Self::ListUtf8 {
                offsets,
                item_offsets,
                bytes,
            } => {
                if offsets.is_empty() {
                    0
                } else {
                    offsets.len() * 4 + item_offsets.len() * 4 + bytes.len()
                }
            }
            Self::MapUtf8 {
                offsets,
                key_offsets,
                key_bytes,
                val_offsets,
                val_bytes,
            } => {
                if offsets.is_empty() {
                    0
                } else {
                    offsets.len() * 4
                        + key_offsets.len() * 4
                        + key_bytes.len()
                        + val_offsets.len() * 4
                        + val_bytes.len()
                }
            }
        }
    }

    fn push_fixed16(&mut self, val: [u8; 16], cap: usize) -> Result<(), QueryError> {
        let Self::Fixed16(vals) = self else {
            return Err(QueryError::internal("builder variant mismatch for fixed16"));
        };
        if vals.is_empty() {
            vals.reserve(cap.min(256));
        }
        vals.push(val);
        Ok(())
    }

    fn push_fixed8(&mut self, val: Option<[u8; 8]>, cap: usize) -> Result<(), QueryError> {
        let Self::Fixed8 { vals, nulls } = self else {
            return Err(QueryError::internal("builder variant mismatch for fixed8"));
        };
        let row_idx = vals.len();
        if vals.is_empty() {
            vals.reserve(cap.min(256));
        }
        match val {
            Some(b) => {
                push_valid_null_bit(nulls, row_idx, true);
                vals.push(b);
            }
            None => {
                push_valid_null_bit(nulls, row_idx, false);
                vals.push([0u8; 8]);
            }
        }
        Ok(())
    }

    fn push_bool(&mut self, val: bool) -> Result<(), QueryError> {
        let Self::Bool(bm) = self else {
            return Err(QueryError::internal("builder variant mismatch for bool"));
        };
        bm.push(val);
        Ok(())
    }

    fn push_ts(&mut self, val: Option<i64>, cap: usize) -> Result<(), QueryError> {
        let Self::TimestampNs { vals, nulls } = self else {
            return Err(QueryError::internal(
                "builder variant mismatch for timestamp_ns",
            ));
        };
        let row_idx = vals.len();
        if vals.is_empty() {
            vals.reserve(cap.min(256));
        }
        match val {
            Some(v) => {
                push_valid_null_bit(nulls, row_idx, true);
                vals.push(v);
            }
            None => {
                push_valid_null_bit(nulls, row_idx, false);
                vals.push(0);
            }
        }
        Ok(())
    }

    fn push_i64(&mut self, val: Option<i64>, cap: usize) -> Result<(), QueryError> {
        let Self::I64 { vals, nulls } = self else {
            return Err(QueryError::internal("builder variant mismatch for i64"));
        };
        let row_idx = vals.len();
        if vals.is_empty() {
            vals.reserve(cap.min(256));
        }
        match val {
            Some(v) => {
                push_valid_null_bit(nulls, row_idx, true);
                vals.push(v);
            }
            None => {
                push_valid_null_bit(nulls, row_idx, false);
                vals.push(0);
            }
        }
        Ok(())
    }

    fn push_f64(&mut self, val: Option<f64>, cap: usize) -> Result<(), QueryError> {
        let Self::F64 { vals, nulls } = self else {
            return Err(QueryError::internal("builder variant mismatch for f64"));
        };
        let row_idx = vals.len();
        if vals.is_empty() {
            vals.reserve(cap.min(256));
        }
        match val {
            Some(v) => {
                push_valid_null_bit(nulls, row_idx, true);
                vals.push(v);
            }
            None => {
                push_valid_null_bit(nulls, row_idx, false);
                vals.push(0.0);
            }
        }
        Ok(())
    }

    fn push_str(&mut self, val: Option<&str>, cap: usize) -> Result<(), QueryError> {
        let Self::Utf8 {
            offsets,
            bytes,
            nulls,
        } = self
        else {
            return Err(QueryError::internal("builder variant mismatch for utf8"));
        };
        let row_idx = if offsets.is_empty() {
            let cap = cap.min(256);
            offsets.reserve(cap + 1);
            offsets.push(0);
            0
        } else {
            offsets.len() - 1
        };
        match val {
            Some(s) => {
                let next_len = bytes
                    .len()
                    .checked_add(s.len())
                    .ok_or(ColumnError::Overflow {
                        reason: "utf8 byte length overflow",
                    })?;
                let end = u32::try_from(next_len).map_err(|_| ColumnError::Overflow {
                    reason: "utf8 byte length exceeds u32::MAX",
                })?;
                bytes.extend_from_slice(s.as_bytes());
                offsets.push(end);
                push_valid_null_bit(nulls, row_idx, true);
            }
            None => {
                let end = u32::try_from(bytes.len()).map_err(|_| ColumnError::Overflow {
                    reason: "utf8 byte length exceeds u32::MAX",
                })?;
                offsets.push(end);
                push_valid_null_bit(nulls, row_idx, false);
            }
        }
        Ok(())
    }

    fn push_list_row_from_set(
        &mut self,
        set: Option<&BTreeSet<String>>,
        cap: usize,
    ) -> Result<(), QueryError> {
        let Self::ListUtf8 {
            offsets,
            item_offsets,
            bytes,
        } = self
        else {
            return Err(QueryError::internal("builder variant mismatch for list"));
        };
        if offsets.is_empty() {
            let cap = cap.min(256);
            offsets.reserve(cap + 1);
            offsets.push(0);
            item_offsets.reserve(cap + 1);
            item_offsets.push(0);
        }
        if let Some(items) = set {
            for s in items {
                push_list_item_buf(bytes, item_offsets, s.as_str())?;
            }
        }
        let item_count = u32::try_from(item_offsets.len().saturating_sub(1)).map_err(|_| {
            ColumnError::Overflow {
                reason: "list items exceed u32::MAX",
            }
        })?;
        offsets.push(item_count);
        Ok(())
    }

    fn push_list_row_from_col<'a>(
        &mut self,
        col: &'a Column,
        row: usize,
        cap: usize,
        scratch: &mut Vec<&'a str>,
    ) -> Result<(), QueryError> {
        let Self::ListUtf8 {
            offsets,
            item_offsets,
            bytes,
        } = self
        else {
            return Err(QueryError::internal("builder variant mismatch for list"));
        };
        if offsets.is_empty() {
            let cap = cap.min(256);
            offsets.reserve(cap + 1);
            offsets.push(0);
            item_offsets.reserve(cap + 1);
            item_offsets.push(0);
        }
        let cp_bytes = bytes.len();
        let cp_items = item_offsets.len();
        let mut prev_range: Option<(usize, usize)> = None;
        let mut unordered = false;
        let mut overflow_err: Option<QueryError> = None;

        let non_null = visit_str_list(col, row, |s| {
            if unordered || overflow_err.is_some() {
                return;
            }
            if let Some((ps, pe)) = prev_range
                && &bytes[ps..pe] >= s.as_bytes()
            {
                unordered = true;
                return;
            }
            let start = bytes.len();
            if let Err(err) = push_list_item_buf(bytes, item_offsets, s) {
                overflow_err = Some(err);
                return;
            }
            prev_range = Some((start, bytes.len()));
        })?;
        if let Some(err) = overflow_err {
            bytes.truncate(cp_bytes);
            item_offsets.truncate(cp_items);
            return Err(err);
        }
        if non_null && unordered {
            bytes.truncate(cp_bytes);
            item_offsets.truncate(cp_items);
            scratch.clear();
            let _ = visit_str_list(col, row, |s| scratch.push(s))?;
            scratch.sort_unstable();
            scratch.dedup();
            for &s in scratch.iter() {
                push_list_item_buf(bytes, item_offsets, s)?;
            }
        }
        let item_count = u32::try_from(item_offsets.len().saturating_sub(1)).map_err(|_| {
            ColumnError::Overflow {
                reason: "list items exceed u32::MAX",
            }
        })?;
        offsets.push(item_count);
        Ok(())
    }

    fn push_map_row_from_map(
        &mut self,
        map: Option<&BTreeMap<String, Ranked<String>>>,
        cap: usize,
    ) -> Result<(), QueryError> {
        let Self::MapUtf8 {
            offsets,
            key_offsets,
            key_bytes,
            val_offsets,
            val_bytes,
        } = self
        else {
            return Err(QueryError::internal("builder variant mismatch for map"));
        };
        if offsets.is_empty() {
            let cap = cap.min(256);
            offsets.reserve(cap + 1);
            offsets.push(0);
            key_offsets.reserve(cap + 1);
            key_offsets.push(0);
            val_offsets.reserve(cap + 1);
            val_offsets.push(0);
        }
        if let Some(entries) = map {
            for (k, r) in entries {
                push_map_entry_buf(
                    key_bytes,
                    key_offsets,
                    val_bytes,
                    val_offsets,
                    k.as_str(),
                    r.value.as_str(),
                )?;
            }
        }
        let entry_count = u32::try_from(key_offsets.len().saturating_sub(1)).map_err(|_| {
            ColumnError::Overflow {
                reason: "map entries exceed u32::MAX",
            }
        })?;
        offsets.push(entry_count);
        Ok(())
    }

    fn push_map_row_from_col<'a>(
        &mut self,
        col: &'a Column,
        row: usize,
        cap: usize,
        scratch: &mut Vec<(&'a str, &'a str)>,
    ) -> Result<(), QueryError> {
        let Self::MapUtf8 {
            offsets,
            key_offsets,
            key_bytes,
            val_offsets,
            val_bytes,
        } = self
        else {
            return Err(QueryError::internal("builder variant mismatch for map"));
        };
        if offsets.is_empty() {
            let cap = cap.min(256);
            offsets.reserve(cap + 1);
            offsets.push(0);
            key_offsets.reserve(cap + 1);
            key_offsets.push(0);
            val_offsets.reserve(cap + 1);
            val_offsets.push(0);
        }
        let cp_k_bytes = key_bytes.len();
        let cp_k_off = key_offsets.len();
        let cp_v_bytes = val_bytes.len();
        let cp_v_off = val_offsets.len();
        let mut prev_key_range: Option<(usize, usize)> = None;
        let mut unordered = false;
        let mut overflow_err: Option<QueryError> = None;

        let non_null = visit_str_map(col, row, |k, v| {
            if unordered || overflow_err.is_some() {
                return;
            }
            if let Some((ps, pe)) = prev_key_range
                && &key_bytes[ps..pe] >= k.as_bytes()
            {
                unordered = true;
                return;
            }
            let start = key_bytes.len();
            if let Err(err) =
                push_map_entry_buf(key_bytes, key_offsets, val_bytes, val_offsets, k, v)
            {
                overflow_err = Some(err);
                return;
            }
            prev_key_range = Some((start, key_bytes.len()));
        })?;
        if let Some(err) = overflow_err {
            key_bytes.truncate(cp_k_bytes);
            key_offsets.truncate(cp_k_off);
            val_bytes.truncate(cp_v_bytes);
            val_offsets.truncate(cp_v_off);
            return Err(err);
        }
        if non_null && unordered {
            key_bytes.truncate(cp_k_bytes);
            key_offsets.truncate(cp_k_off);
            val_bytes.truncate(cp_v_bytes);
            val_offsets.truncate(cp_v_off);
            scratch.clear();
            let _ = visit_str_map(col, row, |k, v| scratch.push((k, v)))?;
            scratch.sort_unstable();
            scratch.dedup_by(|a, b| a.0 == b.0);
            for &(k, v) in scratch.iter() {
                push_map_entry_buf(key_bytes, key_offsets, val_bytes, val_offsets, k, v)?;
            }
        }
        let entry_count = u32::try_from(key_offsets.len().saturating_sub(1)).map_err(|_| {
            ColumnError::Overflow {
                reason: "map entries exceed u32::MAX",
            }
        })?;
        offsets.push(entry_count);
        Ok(())
    }

    fn take_column(&mut self) -> Result<Column, QueryError> {
        match self {
            Self::Fixed16(vals) => {
                let vals = std::mem::take(vals);
                Ok(Column::Flat(FlatColumn::try_new(
                    FlatData::FixedBytes16(vals),
                    None,
                )?))
            }
            Self::Fixed8 { vals, nulls } => {
                let vals = std::mem::take(vals);
                let nulls = nulls.take();
                Ok(Column::Flat(FlatColumn::try_new(
                    FlatData::FixedBytes8(vals),
                    nulls,
                )?))
            }
            Self::Bool(bm) => {
                let bm = std::mem::replace(bm, Bitmap::new_zeroed(0));
                Ok(Column::Flat(FlatColumn::try_new(FlatData::Bool(bm), None)?))
            }
            Self::TimestampNs { vals, nulls } => {
                let vals = std::mem::take(vals);
                let nulls = nulls.take();
                Ok(Column::Flat(FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: vals,
                        utc: true,
                    },
                    nulls,
                )?))
            }
            Self::I64 { vals, nulls } => {
                let vals = std::mem::take(vals);
                let nulls = nulls.take();
                Ok(Column::Flat(FlatColumn::try_new(
                    FlatData::I64(vals),
                    nulls,
                )?))
            }
            Self::F64 { vals, nulls } => {
                let vals = std::mem::take(vals);
                let nulls = nulls.take();
                Ok(Column::Flat(FlatColumn::try_new(
                    FlatData::F64(vals),
                    nulls,
                )?))
            }
            Self::Utf8 {
                offsets,
                bytes,
                nulls,
            } => {
                if offsets.is_empty() {
                    return Ok(Column::Flat(FlatColumn::empty(&DataType::Utf8)));
                }
                let offsets = std::mem::take(offsets);
                let bytes = std::mem::take(bytes);
                let nulls = nulls.take();
                let data = Utf8Data::try_new(offsets, bytes)?;
                Ok(Column::Flat(FlatColumn::try_new(
                    FlatData::Utf8(data),
                    nulls,
                )?))
            }
            Self::ListUtf8 {
                offsets,
                item_offsets,
                bytes,
            } => {
                if offsets.is_empty() {
                    return Ok(Column::Flat(FlatColumn::empty(&DataType::List(Box::new(
                        DataType::Utf8,
                    )))));
                }
                let offsets = std::mem::take(offsets);
                let item_offsets = std::mem::take(item_offsets);
                let bytes = std::mem::take(bytes);
                let child = FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_new(item_offsets, bytes)?),
                    None,
                )?;
                Ok(Column::Flat(FlatColumn::try_new(
                    FlatData::List {
                        item_type: DataType::Utf8,
                        offsets,
                        values: Box::new(Column::Flat(child)),
                    },
                    None,
                )?))
            }
            Self::MapUtf8 {
                offsets,
                key_offsets,
                key_bytes,
                val_offsets,
                val_bytes,
            } => {
                if offsets.is_empty() {
                    return Ok(Column::Flat(FlatColumn::empty(&DataType::MapUtf8)));
                }
                let offsets = std::mem::take(offsets);
                let key_offsets = std::mem::take(key_offsets);
                let key_bytes = std::mem::take(key_bytes);
                let val_offsets = std::mem::take(val_offsets);
                let val_bytes = std::mem::take(val_bytes);
                let keys_data = Utf8Data::try_new(key_offsets, key_bytes)?;
                let vals_data = Utf8Data::try_new(val_offsets, val_bytes)?;
                let map_data = MapData::try_new(offsets, keys_data, vals_data, None)?;
                Ok(Column::Flat(FlatColumn::try_new(
                    FlatData::MapUtf8(Box::new(map_data)),
                    None,
                )?))
            }
        }
    }
}

struct FastScratch<'a> {
    strs: Vec<&'a str>,
    map: Vec<(&'a str, &'a str)>,
}

impl<'a> FastScratch<'a> {
    fn new() -> Self {
        Self {
            strs: Vec::new(),
            map: Vec::new(),
        }
    }
}

struct ChunkBudget<'a, 'b> {
    budget_bytes: usize,
    max_traces: usize,
    merger_res: &'a mut Reservation,
    reconcile_threshold: usize,
    cancel: &'b CancelToken,
}

/// Streaming columnar trace partial merger for `trace_id ASC` sorted input,
/// matching `partials.rs` accumulation rules.
pub(crate) struct SortedTraceMerger {
    input_cols: Vec<TraceColumn>,
    output_cols: Vec<TraceColumn>,
    slot_of: [Option<usize>; TraceColumn::COUNT],
    output_schema: Arc<Schema>,
    builders: Vec<ColBuilder>,
    row_count: usize,
    builders_bytes: usize,
    pending_id: Option<[u8; 16]>,
    pending_acc: TraceAcc,
    pending_size: usize,
    last_flushed_id: Option<[u8; 16]>,
    bounds: TimeBounds,
    initial_cap: usize,
}

impl std::fmt::Debug for SortedTraceMerger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SortedTraceMerger")
            .field("input_cols", &self.input_cols.len())
            .field("output_cols", &self.output_cols.len())
            .field("row_count", &self.row_count)
            .field("has_pending", &self.pending_id.is_some())
            .field("size_bytes", &self.size())
            .finish()
    }
}

impl SortedTraceMerger {
    /// Creates a [`SortedTraceMerger`] for `columns` against the 30-column `full_schema`.
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
        let mut builders = Vec::with_capacity(output_names.len());
        for name in &output_names {
            let col = TraceColumn::from_name(name)
                .ok_or_else(|| QueryError::internal(format!("unknown trace column {name}")))?;
            output_cols.push(col);
            builders.push(ColBuilder::new(col));
        }
        let output_indices = indices_of_names(full_schema, &output_names)?;
        let output_schema = Arc::new(full_schema.project(&output_indices)?);
        let pending_acc = TraceAcc::new(input_cols.len());

        Ok(Self {
            input_cols,
            output_cols,
            slot_of,
            output_schema,
            builders,
            row_count: 0,
            builders_bytes: 0,
            pending_id: None,
            pending_acc,
            pending_size: 0,
            last_flushed_id: None,
            bounds: TimeBounds::open(),
            initial_cap: 256,
        })
    }

    pub(crate) fn with_bounds(mut self, bounds: TimeBounds) -> Self {
        self.bounds = bounds;
        self
    }

    pub(crate) fn with_initial_cap(mut self, cap: usize) -> Self {
        self.initial_cap = cap.clamp(1, 256);
        self
    }

    /// Number of distinct traces currently accumulated (flushed + pending).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.row_count + usize::from(self.pending_id.is_some())
    }

    /// Number of completed traces currently materialized in output builders.
    fn flushed_rows(&self) -> usize {
        self.row_count
    }

    /// True when no traces have been pushed (neither flushed nor pending).
    pub(crate) fn is_empty(&self) -> bool {
        self.row_count == 0 && self.pending_id.is_none()
    }

    /// Tracked logical byte size of output builders + pending accumulator state.
    pub(crate) fn size(&self) -> usize {
        self.builders_bytes + self.pending_size
    }

    fn clear(&mut self) {
        for builder in &mut self.builders {
            builder.clear();
        }
        self.row_count = 0;
        self.builders_bytes = 0;
        self.pending_id = None;
        self.pending_acc.reset();
        self.pending_size = 0;
        self.last_flushed_id = None;
    }

    fn fold_pending_row(&mut self, acc: &BatchAccessors<'_>, row: usize) -> Result<(), QueryError> {
        let start_time = acc.read_start_time(row)?;
        let root = acc.read_has_root(row)?;
        let tie = if acc.root_span_col.is_null(row) {
            None
        } else {
            Some(
                read_fixed8_at(acc.root_span_col, row)
                    .ok_or_else(|| QueryError::internal("root_span_id column type mismatch"))?,
            )
        };
        let ctx = RowCtx {
            rank: Rank {
                root: u8::from(!root),
                start_time,
                tie,
            },
            root,
            preview: true,
        };
        for (slot_idx, &col_kind) in self.input_cols.iter().enumerate() {
            let col = acc.cols[col_kind as usize]
                .ok_or_else(|| QueryError::internal("missing input column accessor"))?;
            self.pending_acc.update_from_col(
                slot_idx,
                col_kind,
                col,
                row,
                &ctx,
                &mut self.pending_size,
            )?;
        }
        Ok(())
    }

    fn push_single_row<'a>(
        &mut self,
        acc: &BatchAccessors<'a>,
        row: usize,
        run_id: [u8; 16],
        start_time: i64,
        has_root: bool,
        scratch: &mut FastScratch<'a>,
    ) -> Result<(), QueryError> {
        let cap = self.initial_cap;
        for (&col_kind, builder) in self.output_cols.iter().zip(self.builders.iter_mut()) {
            match col_kind {
                TraceColumn::TraceId => builder.push_fixed16(run_id, cap)?,
                TraceColumn::StartTime => builder.push_ts(Some(start_time), cap)?,
                TraceColumn::HasRoot => builder.push_bool(has_root)?,
                TraceColumn::Status => {
                    let err_col = acc.cols[TraceColumn::ErrorCount as usize];
                    let errors = match err_col {
                        Some(c) if !c.is_null(row) => read_i64_at(c, row).ok_or_else(|| {
                            QueryError::internal("error_count column type mismatch")
                        })?,
                        _ => 0,
                    };
                    if errors > 0 {
                        builder.push_str(Some(SpanStatus::Error.as_str()), cap)?;
                    } else if has_root
                        && let Some(status_col) = acc.cols[TraceColumn::Status as usize]
                        && !status_col.is_null(row)
                    {
                        let text = read_str_at(status_col, row)
                            .ok_or_else(|| QueryError::internal("status column type mismatch"))?;
                        builder.push_str(Some(text), cap)?;
                    } else {
                        builder.push_str(Some(SpanStatus::Unset.as_str()), cap)?;
                    }
                }
                TraceColumn::RootSpanId => {
                    let col = acc.cols[TraceColumn::RootSpanId as usize]
                        .ok_or_else(|| QueryError::internal("missing root_span_id accessor"))?;
                    if has_root && !col.is_null(row) {
                        let bytes = read_fixed8_at(col, row).ok_or_else(|| {
                            QueryError::internal("root_span_id column type mismatch")
                        })?;
                        builder.push_fixed8(Some(bytes), cap)?;
                    } else {
                        builder.push_fixed8(None, cap)?;
                    }
                }
                TraceColumn::ProjectId
                | TraceColumn::Name
                | TraceColumn::SessionId
                | TraceColumn::UserId
                | TraceColumn::Environment
                | TraceColumn::Release
                | TraceColumn::InputPreview
                | TraceColumn::OutputPreview => {
                    let col = acc.cols[col_kind as usize]
                        .ok_or_else(|| QueryError::internal("missing string column accessor"))?;
                    if col.is_null(row) {
                        builder.push_str(None, cap)?;
                    } else {
                        let text = read_str_at(col, row)
                            .ok_or_else(|| QueryError::internal("string column type mismatch"))?;
                        builder.push_str(Some(text), cap)?;
                    }
                }
                TraceColumn::EndTime | TraceColumn::LastIngestTime => {
                    let col = acc.cols[col_kind as usize]
                        .ok_or_else(|| QueryError::internal("missing timestamp column accessor"))?;
                    if col.is_null(row) {
                        builder.push_ts(None, cap)?;
                    } else {
                        let ts = read_ts_at(col, row).ok_or_else(|| {
                            QueryError::internal("timestamp column type mismatch")
                        })?;
                        builder.push_ts(Some(ts), cap)?;
                    }
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
                    let col = acc.cols[col_kind as usize]
                        .ok_or_else(|| QueryError::internal("missing integer column accessor"))?;
                    if col.is_null(row) {
                        builder.push_i64(None, cap)?;
                    } else {
                        let val = read_i64_at(col, row)
                            .ok_or_else(|| QueryError::internal("integer column type mismatch"))?;
                        builder.push_i64(Some(val), cap)?;
                    }
                }
                TraceColumn::CostTotal => {
                    let col = acc.cols[col_kind as usize]
                        .ok_or_else(|| QueryError::internal("missing cost_total accessor"))?;
                    if col.is_null(row) {
                        builder.push_f64(None, cap)?;
                    } else {
                        let val = read_f64_at(col, row).ok_or_else(|| {
                            QueryError::internal("cost_total column type mismatch")
                        })?;
                        builder.push_f64(Some(usd_from_units(usd_units(val))), cap)?;
                    }
                }
                TraceColumn::Tags | TraceColumn::Models | TraceColumn::Prompts => {
                    let col = acc.cols[col_kind as usize]
                        .ok_or_else(|| QueryError::internal("missing list column accessor"))?;
                    builder.push_list_row_from_col(col, row, cap, &mut scratch.strs)?;
                }
                TraceColumn::Metadata => {
                    let col = acc.cols[col_kind as usize]
                        .ok_or_else(|| QueryError::internal("missing metadata column accessor"))?;
                    builder.push_map_row_from_col(col, row, cap, &mut scratch.map)?;
                }
            }
        }
        self.row_count += 1;
        self.builders_bytes = self.builders.iter().map(ColBuilder::memory_bytes).sum();
        Ok(())
    }

    fn flush_pending(&mut self) -> Result<(), QueryError> {
        let Some(id) = self.pending_id.take() else {
            return Ok(());
        };
        let start_slot = self.slot_of[TraceColumn::StartTime as usize]
            .ok_or_else(|| QueryError::internal("missing start_time slot in SortedTraceMerger"))?;
        let Some(Acc::I64(start_time)) = self.pending_acc.slots[start_slot] else {
            self.pending_acc.reset();
            self.pending_size = 0;
            return Err(QueryError::internal(
                "missing pending start_time in SortedTraceMerger",
            ));
        };

        if self.bounds.contains(start_time) {
            let cap = self.initial_cap;
            let error_slot = self.slot_of[TraceColumn::ErrorCount as usize];
            let status_slot = self.slot_of[TraceColumn::Status as usize];

            for (&col_kind, builder) in self.output_cols.iter().zip(self.builders.iter_mut()) {
                if col_kind == TraceColumn::Status {
                    let errors = error_slot
                        .and_then(|s| self.pending_acc.slots[s].as_ref())
                        .and_then(|a| match a {
                            Acc::I64(v) => Some(*v),
                            _ => None,
                        })
                        .unwrap_or(0);
                    if errors > 0 {
                        builder.push_str(Some(SpanStatus::Error.as_str()), cap)?;
                    } else if let Some(Acc::RankedStr(r)) =
                        status_slot.and_then(|s| self.pending_acc.slots[s].as_ref())
                    {
                        builder.push_str(Some(r.value.as_str()), cap)?;
                    } else {
                        builder.push_str(Some(SpanStatus::Unset.as_str()), cap)?;
                    }
                    continue;
                }

                let slot_idx = self.slot_of[col_kind as usize].ok_or_else(|| {
                    QueryError::internal(format!("missing slot for {}", col_kind.name()))
                })?;
                let slot = &self.pending_acc.slots[slot_idx];
                match col_kind {
                    TraceColumn::TraceId => {
                        let Some(Acc::Fixed16(tid)) = slot else {
                            return Err(QueryError::internal("missing trace_id in merger"));
                        };
                        builder.push_fixed16(*tid, cap)?;
                    }
                    TraceColumn::ProjectId
                    | TraceColumn::Name
                    | TraceColumn::SessionId
                    | TraceColumn::UserId
                    | TraceColumn::Environment
                    | TraceColumn::Release
                    | TraceColumn::InputPreview
                    | TraceColumn::OutputPreview => match slot {
                        Some(Acc::RankedStr(r)) => {
                            builder.push_str(Some(r.value.as_str()), cap)?;
                        }
                        _ => builder.push_str(None, cap)?,
                    },
                    TraceColumn::RootSpanId => match slot {
                        Some(Acc::RankedFixed8(r)) => builder.push_fixed8(Some(r.value), cap)?,
                        _ => builder.push_fixed8(None, cap)?,
                    },
                    TraceColumn::HasRoot => {
                        builder.push_bool(matches!(slot, Some(Acc::Bool(true))))?;
                    }
                    TraceColumn::StartTime | TraceColumn::EndTime | TraceColumn::LastIngestTime => {
                        match slot {
                            Some(Acc::I64(v)) => builder.push_ts(Some(*v), cap)?,
                            _ => builder.push_ts(None, cap)?,
                        }
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
                    | TraceColumn::PartialCount => match slot {
                        Some(Acc::I64(v)) => builder.push_i64(Some(*v), cap)?,
                        _ => builder.push_i64(None, cap)?,
                    },
                    TraceColumn::CostTotal => match slot {
                        Some(Acc::Usd(units)) => {
                            builder.push_f64(Some(usd_from_units(*units)), cap)?;
                        }
                        _ => builder.push_f64(None, cap)?,
                    },
                    TraceColumn::Tags | TraceColumn::Models | TraceColumn::Prompts => {
                        let set = match slot {
                            Some(Acc::StrSet(set)) => Some(set),
                            _ => None,
                        };
                        builder.push_list_row_from_set(set, cap)?;
                    }
                    TraceColumn::Metadata => {
                        let map = match slot {
                            Some(Acc::StrMap(map)) => Some(map),
                            _ => None,
                        };
                        builder.push_map_row_from_map(map, cap)?;
                    }
                    TraceColumn::Status => unreachable!("handled above"),
                }
            }
            self.row_count += 1;
            self.builders_bytes = self.builders.iter().map(ColBuilder::memory_bytes).sum();
        }

        self.last_flushed_id = Some(id);
        self.pending_acc.reset();
        self.pending_size = 0;
        Ok(())
    }

    fn consume_batch_chunk(
        &mut self,
        partials: &Batch,
        offset: &mut usize,
        end: usize,
        mut chunk_limit: Option<ChunkBudget<'_, '_>>,
    ) -> Result<bool, QueryError> {
        let acc = BatchAccessors::resolve(partials, &self.input_cols)?;
        let mut scratch = FastScratch::new();

        while *offset < end {
            if let Some(ref cb) = chunk_limit
                && cb.cancel.is_cancelled()
            {
                return Err(QueryError::Cancelled);
            }
            let run_id = acc.read_trace_id(*offset)?;
            let mut run_end = *offset + 1;
            if let Some(ids) = acc.flat_trace_ids {
                while run_end < end && ids[run_end] == run_id {
                    run_end += 1;
                }
            } else {
                while run_end < end && read_fixed16_at(acc.trace_id_col, run_end) == Some(run_id) {
                    run_end += 1;
                }
            }

            if let Some(prev_id) = self.pending_id
                && prev_id != run_id
            {
                self.flush_pending()?;
            }

            if self.pending_id.is_none() {
                if let Some(prev_flushed) = self.last_flushed_id
                    && run_id <= prev_flushed
                {
                    return Err(QueryError::internal(
                        "unsorted trace_id in SortedTraceMerger",
                    ));
                }
                if let Some(ref cb) = chunk_limit
                    && self.row_count > 0
                    && (self.size() >= cb.budget_bytes || self.row_count >= cb.max_traces)
                {
                    return Ok(true);
                }
            }

            if self.pending_id.is_none() && run_end == *offset + 1 && run_end < end {
                let start_time = acc.read_start_time(*offset)?;
                let has_root = acc.read_has_root(*offset)?;
                if self.bounds.contains(start_time) {
                    self.push_single_row(
                        &acc,
                        *offset,
                        run_id,
                        start_time,
                        has_root,
                        &mut scratch,
                    )?;
                }
                self.last_flushed_id = Some(run_id);
            } else {
                if self.pending_id.is_none() {
                    self.pending_id = Some(run_id);
                    self.pending_size = self.input_cols.len() * size_of::<Option<Acc>>();
                }
                for row in *offset..run_end {
                    self.fold_pending_row(&acc, row)?;
                }
                if run_end < end {
                    self.flush_pending()?;
                }
            }

            *offset = run_end;

            if let Some(ref mut cb) = chunk_limit
                && self.size().abs_diff(cb.merger_res.size()) >= cb.reconcile_threshold
            {
                reconcile_res(Some(cb.merger_res), 0, self.size())?;
            }
        }

        Ok(false)
    }

    /// Folds `trace_id`-sorted rows in `start..end` of `partials` (`Selection::All`).
    #[cfg(test)]
    pub(crate) fn push_range(
        &mut self,
        partials: &Batch,
        start: usize,
        end: usize,
    ) -> Result<(), QueryError> {
        if start >= end || partials.is_empty() {
            return Ok(());
        }
        if end > partials.raw_len() {
            return Err(ColumnError::InvalidBatch {
                reason: "push_range requires valid start..end bounds",
            }
            .into());
        }
        let mut offset = start;
        let _ = self.consume_batch_chunk(partials, &mut offset, end, None)?;
        Ok(())
    }

    fn take_batch(&mut self) -> Result<Batch, QueryError> {
        let row_count = self.row_count;
        if row_count == 0 {
            return Ok(Batch::empty(Arc::clone(&self.output_schema)));
        }
        let mut columns = Vec::with_capacity(self.builders.len());
        for builder in &mut self.builders {
            columns.push(builder.take_column()?);
        }
        self.row_count = 0;
        self.builders_bytes = 0;
        Ok(Batch::try_new_all(
            Arc::clone(&self.output_schema),
            columns,
            row_count,
        )?)
    }

    /// Flushes any trailing pending trace and returns the finalized `Batch`.
    #[cfg(test)]
    pub(crate) fn finish(mut self) -> Result<Batch, QueryError> {
        self.flush_pending()?;
        self.take_batch()
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

fn visit_flat_str_list<'a>(
    flat: &'a FlatColumn,
    idx: usize,
    mut f: impl FnMut(&'a str),
) -> Result<bool, QueryError> {
    if idx >= flat.len() {
        return Err(QueryError::internal("string list row out of bounds"));
    }
    match flat.data() {
        FlatData::List {
            offsets, values, ..
        } => {
            let start = *offsets
                .get(idx)
                .ok_or_else(|| QueryError::internal("string list start offset out of bounds"))?
                as usize;
            let end = *offsets
                .get(idx + 1)
                .ok_or_else(|| QueryError::internal("string list end offset out of bounds"))?
                as usize;
            if let Column::Flat(vf) = values.as_ref()
                && vf.nulls().is_none()
                && let FlatData::Utf8(u) = vf.data()
            {
                for i in start..end {
                    let s = u.get(i).ok_or_else(|| {
                        QueryError::internal("string list item out of bounds or invalid utf8")
                    })?;
                    f(s);
                }
            } else {
                for i in start..end {
                    if i >= values.len() {
                        return Err(QueryError::internal("string list item index out of bounds"));
                    }
                    if !values.is_null(i) {
                        let s = read_str_at(values, i).ok_or_else(|| {
                            QueryError::internal("string list item type mismatch")
                        })?;
                        f(s);
                    }
                }
            }
            Ok(true)
        }
        _ => Err(QueryError::internal("string list column type mismatch")),
    }
}

fn visit_str_list<'a>(
    col: &'a Column,
    row: usize,
    mut f: impl FnMut(&'a str),
) -> Result<bool, QueryError> {
    if row >= col.len() {
        return Err(QueryError::internal("string list row out of bounds"));
    }
    if col.is_null(row) {
        return Ok(false);
    }
    match col {
        Column::Flat(flat) => visit_flat_str_list(flat, row, f),
        Column::Const { value, .. } => match value {
            ScalarValue::List { values, .. } => {
                for v in values {
                    match v {
                        ScalarValue::Utf8(s) => f(s.as_str()),
                        ScalarValue::Null(_) => {}
                        _ => {
                            return Err(QueryError::internal("string list item type mismatch"));
                        }
                    }
                }
                Ok(true)
            }
            _ => Err(QueryError::internal("string list column type mismatch")),
        },
        Column::Dict(d) => {
            let code = *d
                .codes()
                .get(row)
                .ok_or_else(|| QueryError::internal("string list dict code out of bounds"))?
                as usize;
            visit_flat_str_list(d.values(), code, f)
        }
        Column::Rle(r) => {
            let phys = r.run_ends().partition_point(|&e| (e as usize) <= row);
            visit_flat_str_list(r.values(), phys, f)
        }
    }
}

fn visit_flat_str_map<'a>(
    flat: &'a FlatColumn,
    idx: usize,
    mut f: impl FnMut(&'a str, &'a str),
) -> Result<bool, QueryError> {
    if idx >= flat.len() {
        return Err(QueryError::internal("metadata map row out of bounds"));
    }
    match flat.data() {
        FlatData::MapUtf8(m) => {
            let offsets = m.offsets();
            let start = *offsets
                .get(idx)
                .ok_or_else(|| QueryError::internal("metadata map start offset out of bounds"))?
                as usize;
            let end = *offsets
                .get(idx + 1)
                .ok_or_else(|| QueryError::internal("metadata map end offset out of bounds"))?
                as usize;
            let keys = m.keys();
            let values = m.values();
            let val_nulls = m.value_nulls();
            for i in start..end {
                if val_nulls.is_some_and(|bm| !bm.get(i)) {
                    continue;
                }
                let k = keys.get(i).ok_or_else(|| {
                    QueryError::internal("metadata map key out of bounds or invalid utf8")
                })?;
                let v = values.get(i).ok_or_else(|| {
                    QueryError::internal("metadata map value out of bounds or invalid utf8")
                })?;
                f(k, v);
            }
            Ok(true)
        }
        _ => Err(QueryError::internal("metadata map column type mismatch")),
    }
}

fn visit_str_map<'a>(
    col: &'a Column,
    row: usize,
    mut f: impl FnMut(&'a str, &'a str),
) -> Result<bool, QueryError> {
    if row >= col.len() {
        return Err(QueryError::internal("metadata map row out of bounds"));
    }
    if col.is_null(row) {
        return Ok(false);
    }
    match col {
        Column::Flat(flat) => visit_flat_str_map(flat, row, f),
        Column::Const { value, .. } => match value {
            ScalarValue::MapUtf8(entries) => {
                for (k, v) in entries {
                    if let Some(val) = v {
                        f(k.as_str(), val.as_str());
                    }
                }
                Ok(true)
            }
            _ => Err(QueryError::internal("metadata map column type mismatch")),
        },
        Column::Dict(d) => {
            let code = *d
                .codes()
                .get(row)
                .ok_or_else(|| QueryError::internal("metadata map dict code out of bounds"))?
                as usize;
            visit_flat_str_map(d.values(), code, f)
        }
        Column::Rle(r) => {
            let phys = r.run_ends().partition_point(|&e| (e as usize) <= row);
            visit_flat_str_map(r.values(), phys, f)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cetra_column::{DictColumn, Field, GlobalPool, RleColumn};
    use cetra_core::schema::traces_schema;
    use proptest::prelude::*;

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

    /// Simple `BTreeMap`-backed reference oracle for verifying `SortedTraceMerger` bit-for-bit.
    struct OracleTraceMerger {
        input_cols: Vec<TraceColumn>,
        output_cols: Vec<TraceColumn>,
        slot_of: [Option<usize>; TraceColumn::COUNT],
        output_schema: Arc<Schema>,
        traces: BTreeMap<[u8; 16], TraceAcc>,
    }

    impl OracleTraceMerger {
        fn new(columns: &MergeColumns, full_schema: &Schema) -> Result<Self, QueryError> {
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
            })
        }

        fn push_batch(&mut self, partials: &Batch) -> Result<(), QueryError> {
            if partials.is_empty() {
                return Ok(());
            }
            let acc = BatchAccessors::resolve(partials, &self.input_cols)?;
            let slot_count = self.input_cols.len();
            for row in 0..partials.raw_len() {
                let start_time = acc.read_start_time(row)?;
                let root = acc.read_has_root(row)?;
                let tie = if acc.root_span_col.is_null(row) {
                    None
                } else {
                    Some(
                        read_fixed8_at(acc.root_span_col, row)
                            .ok_or_else(|| QueryError::internal("root_span_id mismatch"))?,
                    )
                };
                let trace_id = acc.read_trace_id(row)?;
                let ctx = RowCtx {
                    rank: Rank {
                        root: u8::from(!root),
                        start_time,
                        tie,
                    },
                    root,
                    preview: true,
                };
                let trace_acc = self
                    .traces
                    .entry(trace_id)
                    .or_insert_with(|| TraceAcc::new(slot_count));
                let mut dummy_size = 0usize;
                for (slot_idx, &col_kind) in self.input_cols.iter().enumerate() {
                    let col = acc.cols[col_kind as usize].unwrap();
                    trace_acc.update_from_col(
                        slot_idx,
                        col_kind,
                        col,
                        row,
                        &ctx,
                        &mut dummy_size,
                    )?;
                }
            }
            Ok(())
        }

        fn finish(self, bounds: TimeBounds) -> Result<Batch, QueryError> {
            let start_slot = self.slot_of[TraceColumn::StartTime as usize].unwrap();
            let rows: Vec<&TraceAcc> = self
                .traces
                .values()
                .filter(|acc| match acc.slots[start_slot] {
                    Some(Acc::I64(ts)) => bounds.contains(ts),
                    _ => false,
                })
                .collect();
            let row_count = rows.len();
            if row_count == 0 {
                return Ok(Batch::empty(self.output_schema));
            }
            let mut columns = Vec::with_capacity(self.output_cols.len());
            for &col_kind in &self.output_cols {
                columns.push(oracle_build_column(col_kind, &self.slot_of, &rows)?);
            }
            Ok(Batch::try_new_all(self.output_schema, columns, row_count)?)
        }
    }

    fn oracle_build_column(
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
                } else if let Some(Acc::RankedStr(r)) =
                    status_slot.and_then(|s| acc.slots[s].as_ref())
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

        let slot_idx = slot_of[col_kind as usize].unwrap();
        match col_kind {
            TraceColumn::TraceId => {
                let vals = rows
                    .iter()
                    .map(|acc| match acc.slots[slot_idx] {
                        Some(Acc::Fixed16(id)) => id,
                        _ => unreachable!(),
                    })
                    .collect();
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
                Ok(Column::Flat(FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(vals)?),
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
                        Some(Acc::Usd(u)) => vals.push(usd_from_units(*u)),
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
                offsets.push(0u32);
                let mut items: Vec<&str> = Vec::new();
                for acc in rows {
                    if let Some(Acc::StrSet(set)) = &acc.slots[slot_idx] {
                        for s in set {
                            items.push(s.as_str());
                        }
                    }
                    offsets.push(items.len() as u32);
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
                offsets.push(0u32);
                let mut keys = Vec::new();
                let mut vals = Vec::new();
                for acc in rows {
                    if let Some(Acc::StrMap(map)) = &acc.slots[slot_idx] {
                        for (k, r) in map {
                            keys.push(k.as_str());
                            vals.push(r.value.as_str());
                        }
                    }
                    offsets.push(keys.len() as u32);
                }
                let map_data = MapData::try_new(
                    offsets,
                    Utf8Data::try_from_slices(keys)?,
                    Utf8Data::try_from_slices(vals)?,
                    None,
                )?;
                Ok(Column::Flat(FlatColumn::try_new(
                    FlatData::MapUtf8(Box::new(map_data)),
                    None,
                )?))
            }
            TraceColumn::Status => unreachable!(),
        }
    }

    const ALL_TRACE_COLUMNS: [TraceColumn; TraceColumn::COUNT] = [
        TraceColumn::ProjectId,
        TraceColumn::TraceId,
        TraceColumn::StartTime,
        TraceColumn::EndTime,
        TraceColumn::Name,
        TraceColumn::RootSpanId,
        TraceColumn::HasRoot,
        TraceColumn::Status,
        TraceColumn::SessionId,
        TraceColumn::UserId,
        TraceColumn::Environment,
        TraceColumn::Release,
        TraceColumn::Tags,
        TraceColumn::Metadata,
        TraceColumn::InputPreview,
        TraceColumn::OutputPreview,
        TraceColumn::SpanCount,
        TraceColumn::ErrorCount,
        TraceColumn::LlmCount,
        TraceColumn::InputTokens,
        TraceColumn::OutputTokens,
        TraceColumn::CacheReadTokens,
        TraceColumn::CacheWriteTokens,
        TraceColumn::ReasoningTokens,
        TraceColumn::TotalTokens,
        TraceColumn::CostTotal,
        TraceColumn::Models,
        TraceColumn::Prompts,
        TraceColumn::LastIngestTime,
        TraceColumn::PartialCount,
    ];

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

    fn tid(n: u32) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[12..16].copy_from_slice(&n.to_be_bytes());
        out
    }

    fn sid(n: u32) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[4..8].copy_from_slice(&n.to_be_bytes());
        out
    }

    fn assert_batches_bit_identical(actual: &Batch, expected: &Batch) {
        assert_eq!(actual.schema(), expected.schema(), "schema mismatch");
        assert_eq!(actual.num_rows(), expected.num_rows(), "row count mismatch");
        for (col_idx, field) in actual.schema().fields().iter().enumerate() {
            let ac = actual.column(col_idx).unwrap();
            let ec = expected.column(col_idx).unwrap();
            for row in 0..actual.num_rows() {
                let av = ac.get_scalar(row).unwrap();
                let ev = ec.get_scalar(row).unwrap();
                match (&av, &ev) {
                    (ScalarValue::F64(a), ScalarValue::F64(b)) => {
                        assert_eq!(
                            a.to_bits(),
                            b.to_bits(),
                            "f64 bit mismatch at col {} row {row}: {a} vs {b}",
                            field.name
                        );
                    }
                    _ => assert_eq!(av, ev, "value mismatch at col {} row {row}", field.name),
                }
            }
        }
    }

    fn encode_col_variant(col: &Column, mode: u8) -> Column {
        let n = col.len();
        if n == 0 {
            return col.clone();
        }
        match mode % 4 {
            0 => col.clone(),
            1 => {
                // Const if all rows have identical scalar value.
                let first = col.get_scalar(0).unwrap();
                if (1..n).all(|r| col.get_scalar(r).unwrap().semantic_eq(&first)) {
                    Column::try_new_const(first, n).unwrap()
                } else {
                    col.clone()
                }
            }
            2 => {
                // Dict encoding over the flat values.
                if let Column::Flat(flat) = col {
                    let codes: Vec<u32> = (0..n as u32).collect();
                    Column::Dict(DictColumn::try_new(flat.clone(), codes, None).unwrap())
                } else {
                    col.clone()
                }
            }
            _ => {
                // Rle encoding with 1-row runs.
                if let Column::Flat(flat) = col {
                    let run_ends: Vec<u32> = (1..=n as u32).collect();
                    Column::Rle(RleColumn::try_new(flat.clone(), run_ends).unwrap())
                } else {
                    col.clone()
                }
            }
        }
    }

    fn build_full_partial_batch(rows: &[PartialRowSpec], enc_seed: &[u8]) -> Batch {
        let schema = traces_schema();
        let n = rows.len();
        let mut columns = Vec::with_capacity(TraceColumn::COUNT);

        let str_col = |extract: fn(&PartialRowSpec) -> Option<&str>| -> Column {
            let mut nulls = Bitmap::new_ones(n);
            let mut vals = Vec::with_capacity(n);
            for (i, r) in rows.iter().enumerate() {
                match extract(r) {
                    Some(s) => vals.push(s),
                    None => {
                        let _ = nulls.set(i, false);
                        vals.push("");
                    }
                }
            }
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(vals).unwrap()),
                    Some(nulls),
                )
                .unwrap(),
            )
        };

        let opt_i64_col = |extract: fn(&PartialRowSpec) -> Option<i64>| -> Column {
            let mut nulls = Bitmap::new_ones(n);
            let mut vals = Vec::with_capacity(n);
            for (i, r) in rows.iter().enumerate() {
                match extract(r) {
                    Some(v) => vals.push(v),
                    None => {
                        let _ = nulls.set(i, false);
                        vals.push(0);
                    }
                }
            }
            Column::Flat(FlatColumn::try_new(FlatData::I64(vals), Some(nulls)).unwrap())
        };

        for (c_idx, &kind) in ALL_TRACE_COLUMNS.iter().enumerate() {
            let flat = match kind {
                TraceColumn::ProjectId => str_col(|r| Some(r.project.as_str())),
                TraceColumn::TraceId => Column::Flat(
                    FlatColumn::try_new(
                        FlatData::FixedBytes16(rows.iter().map(|r| r.trace_id).collect()),
                        None,
                    )
                    .unwrap(),
                ),
                TraceColumn::StartTime => Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: rows.iter().map(|r| r.start_time).collect(),
                            utc: true,
                        },
                        None,
                    )
                    .unwrap(),
                ),
                TraceColumn::EndTime => Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: rows.iter().map(|r| r.end_time).collect(),
                            utc: true,
                        },
                        None,
                    )
                    .unwrap(),
                ),
                TraceColumn::Name => str_col(|r| Some(r.name.as_str())),
                TraceColumn::RootSpanId => {
                    let mut nulls = Bitmap::new_ones(n);
                    let mut vals = Vec::with_capacity(n);
                    for (i, r) in rows.iter().enumerate() {
                        match r.root_span_id {
                            Some(b) => vals.push(b),
                            None => {
                                let _ = nulls.set(i, false);
                                vals.push([0u8; 8]);
                            }
                        }
                    }
                    Column::Flat(
                        FlatColumn::try_new(FlatData::FixedBytes8(vals), Some(nulls)).unwrap(),
                    )
                }
                TraceColumn::HasRoot => Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Bool(Bitmap::from_bools(
                            &rows.iter().map(|r| r.has_root).collect::<Vec<_>>(),
                        )),
                        None,
                    )
                    .unwrap(),
                ),
                TraceColumn::Status => str_col(|r| Some(r.status.as_str())),
                TraceColumn::SessionId => str_col(|r| r.session_id.as_deref()),
                TraceColumn::UserId => str_col(|r| r.user_id.as_deref()),
                TraceColumn::Environment => str_col(|r| r.environment.as_deref()),
                TraceColumn::Release => str_col(|r| r.release.as_deref()),
                TraceColumn::Tags => list_col(rows.iter().map(|r| r.tags.as_slice())),
                TraceColumn::Metadata => {
                    let mut offsets = Vec::with_capacity(n + 1);
                    offsets.push(0u32);
                    let mut keys = Vec::new();
                    let mut vals = Vec::new();
                    let mut val_nulls =
                        Bitmap::new_ones(rows.iter().map(|r| r.metadata.len()).sum());
                    let mut entry_idx = 0usize;
                    for r in rows {
                        for (k, v) in &r.metadata {
                            keys.push(k.as_str());
                            match v {
                                Some(s) => vals.push(s.as_str()),
                                None => {
                                    let _ = val_nulls.set(entry_idx, false);
                                    vals.push("");
                                }
                            }
                            entry_idx += 1;
                        }
                        offsets.push(keys.len() as u32);
                    }
                    let map = MapData::try_new(
                        offsets,
                        Utf8Data::try_from_slices(keys).unwrap(),
                        Utf8Data::try_from_slices(vals).unwrap(),
                        Some(val_nulls),
                    )
                    .unwrap();
                    Column::Flat(
                        FlatColumn::try_new(FlatData::MapUtf8(Box::new(map)), None).unwrap(),
                    )
                }
                TraceColumn::InputPreview => str_col(|r| r.input_preview.as_deref()),
                TraceColumn::OutputPreview => str_col(|r| r.output_preview.as_deref()),
                TraceColumn::SpanCount => opt_i64_col(|r| Some(r.span_count)),
                TraceColumn::ErrorCount => opt_i64_col(|r| Some(r.error_count)),
                TraceColumn::LlmCount => opt_i64_col(|r| Some(r.llm_count)),
                TraceColumn::InputTokens => opt_i64_col(|r| r.input_tokens),
                TraceColumn::OutputTokens => opt_i64_col(|r| r.output_tokens),
                TraceColumn::CacheReadTokens => opt_i64_col(|r| r.cache_read_tokens),
                TraceColumn::CacheWriteTokens => opt_i64_col(|r| r.cache_write_tokens),
                TraceColumn::ReasoningTokens => opt_i64_col(|r| r.reasoning_tokens),
                TraceColumn::TotalTokens => opt_i64_col(|r| r.total_tokens),
                TraceColumn::CostTotal => {
                    let mut nulls = Bitmap::new_ones(n);
                    let mut vals = Vec::with_capacity(n);
                    for (i, r) in rows.iter().enumerate() {
                        match r.cost_total {
                            Some(v) => vals.push(v),
                            None => {
                                let _ = nulls.set(i, false);
                                vals.push(0.0);
                            }
                        }
                    }
                    Column::Flat(FlatColumn::try_new(FlatData::F64(vals), Some(nulls)).unwrap())
                }
                TraceColumn::Models => list_col(rows.iter().map(|r| r.models.as_slice())),
                TraceColumn::Prompts => list_col(rows.iter().map(|r| r.prompts.as_slice())),
                TraceColumn::LastIngestTime => Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: rows.iter().map(|r| r.last_ingest_time).collect(),
                            utc: true,
                        },
                        None,
                    )
                    .unwrap(),
                ),
                TraceColumn::PartialCount => opt_i64_col(|r| Some(r.partial_count)),
            };
            let mode = enc_seed
                .get(c_idx % enc_seed.len().max(1))
                .copied()
                .unwrap_or(0);
            columns.push(encode_col_variant(&flat, mode));
        }

        Batch::try_new_all(schema, columns, n).unwrap()
    }

    fn list_col<'a>(iter: impl Iterator<Item = &'a [String]>) -> Column {
        let mut offsets = vec![0u32];
        let mut items: Vec<&str> = Vec::new();
        for list in iter {
            for s in list {
                items.push(s.as_str());
            }
            offsets.push(items.len() as u32);
        }
        let child = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(items).unwrap()),
            None,
        )
        .unwrap();
        Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets,
                    values: Box::new(Column::Flat(child)),
                },
                None,
            )
            .unwrap(),
        )
    }

    #[derive(Clone)]
    struct PartialRowSpec {
        project: String,
        trace_id: [u8; 16],
        start_time: i64,
        end_time: i64,
        name: String,
        root_span_id: Option<[u8; 8]>,
        has_root: bool,
        status: String,
        session_id: Option<String>,
        user_id: Option<String>,
        environment: Option<String>,
        release: Option<String>,
        tags: Vec<String>,
        metadata: Vec<(String, Option<String>)>,
        input_preview: Option<String>,
        output_preview: Option<String>,
        span_count: i64,
        error_count: i64,
        llm_count: i64,
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
        cache_read_tokens: Option<i64>,
        cache_write_tokens: Option<i64>,
        reasoning_tokens: Option<i64>,
        total_tokens: Option<i64>,
        cost_total: Option<f64>,
        models: Vec<String>,
        prompts: Vec<String>,
        last_ingest_time: i64,
        partial_count: i64,
    }

    #[test]
    fn pass26_native_trace_merge_stream_batched_reconcile_parity_and_tight_oom() {
        let full_schema = Schema::try_new_arc(vec![
            Field::new(traces::TRACE_ID, DataType::FixedBytes16, false),
            Field::new(
                traces::START_TIME,
                DataType::TimestampNs { utc: true },
                false,
            ),
            Field::new(traces::HAS_ROOT, DataType::Bool, false),
            Field::new(traces::ROOT_SPAN_ID, DataType::FixedBytes8, true),
        ])
        .unwrap();
        let merge_columns = MergeColumns::new(&[traces::TRACE_ID, traces::START_TIME]);

        let make_rows = |count: usize, base: u32| -> Vec<([u8; 16], i64)> {
            let mut out = Vec::with_capacity(count * 2);
            for i in 0..count {
                let t = tid(base + i as u32);
                out.push((t, 1_000 + (i as i64) * 10 + 5));
                out.push((t, 1_000 + (i as i64) * 10));
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
            merger: SortedTraceMerger::new(&merge_columns, &full_schema).unwrap(),
            merger_res: pool.register("merger", false),
            scratch_res: pool.register("scratch", false),
            post_filters: Vec::new(),
            projection: None,
            remaining: None,
            eval: EvalContext::new(0, cancel.clone()),
            active_input: None,
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
        //    but cannot fit the merger's reconciliation -> must return TooLarge and
        //    release ALL reservations cleanly (global.used() == 0).
        let small_batch = make_minimal_partial_batch(&full_schema, &make_rows(80, 0));
        let batch_mem = small_batch.memory_bytes();
        let tight_global = GlobalPool::new(batch_mem + 256);
        let tight_pool = tight_global.try_admit(0, batch_mem + 256).unwrap();
        let mut tight_stream = NativeTraceMergeStream {
            input: Box::new(VecBatchesStream {
                batches: VecDeque::from([small_batch]),
            }),
            input_res: tight_pool.register("in", false),
            merger: SortedTraceMerger::new(&merge_columns, &full_schema).unwrap(),
            merger_res: tight_pool.register("merger", false),
            scratch_res: tight_pool.register("scratch", false),
            post_filters: Vec::new(),
            projection: None,
            remaining: None,
            eval: EvalContext::new(0, cancel.clone()),
            active_input: None,
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
        assert!(tight_stream.active_input.is_none());
        assert!(tight_stream.merger.is_empty());
        assert_eq!(tight_global.used(), 0);
    }

    #[test]
    fn sorted_trace_merger_cross_batch_split_and_single_trace_eof() {
        let full_schema = traces_schema();
        let merge_columns = MergeColumns::all();
        let base_row = |t: u32, start: i64, has_root: bool, span: u32, cost: f64| PartialRowSpec {
            project: "proj".to_owned(),
            trace_id: tid(t),
            start_time: start,
            end_time: start + 50,
            name: format!("n-{t}-{start}"),
            root_span_id: has_root.then(|| sid(span)),
            has_root,
            status: if has_root { "ok" } else { "unset" }.to_owned(),
            session_id: Some(format!("s-{start}")),
            user_id: Some("u1".to_owned()),
            environment: Some("prod".to_owned()),
            release: Some("r1".to_owned()),
            tags: vec![format!("tag-{start}")],
            metadata: vec![("k".to_owned(), Some(format!("v-{start}")))],
            input_preview: Some(format!("in-{start}")),
            output_preview: None,
            span_count: 1,
            error_count: 0,
            llm_count: 1,
            input_tokens: Some(10),
            output_tokens: Some(20),
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            total_tokens: Some(30),
            cost_total: Some(cost),
            models: vec![format!("m-{start}")],
            prompts: vec![format!("p@{t}")],
            last_ingest_time: start + 10,
            partial_count: 1,
        };

        // Case A: Trace 2 spans 3 batches (trailing row of B1 + all of B2 + leading row of B3),
        // followed by trailing single-row Trace 3 at EOF.
        let b1 = build_full_partial_batch(
            &[
                base_row(1, 100, true, 1, 0.1),
                base_row(2, 200, false, 2, 0.2),
            ],
            &[0],
        );
        let b2 = build_full_partial_batch(
            &[
                base_row(2, 150, true, 3, 0.3),
                base_row(2, 250, true, 4, 0.4),
            ],
            &[2],
        );
        let b3 = build_full_partial_batch(
            &[
                base_row(2, 180, false, 5, 0.5),
                base_row(3, 300, true, 6, 0.6),
            ],
            &[3],
        );

        let mut oracle = OracleTraceMerger::new(&merge_columns, &full_schema).unwrap();
        oracle.push_batch(&b1).unwrap();
        oracle.push_batch(&b2).unwrap();
        oracle.push_batch(&b3).unwrap();
        let expected = oracle.finish(TimeBounds::open()).unwrap();

        let global = GlobalPool::new(16 * 1024 * 1024);
        let pool = global.try_admit(0, 16 * 1024 * 1024).unwrap();
        let cancel = CancelToken::new();
        let mut stream = NativeTraceMergeStream {
            input: Box::new(VecBatchesStream {
                batches: VecDeque::from([b1, b2, b3]),
            }),
            input_res: pool.register("in", false),
            merger: SortedTraceMerger::new(&merge_columns, &full_schema)
                .unwrap()
                .with_initial_cap(1),
            merger_res: pool.register("merger", false),
            scratch_res: pool.register("scratch", false),
            post_filters: Vec::new(),
            projection: None,
            remaining: None,
            eval: EvalContext::new(0, cancel.clone()),
            active_input: None,
            budget_bytes: 128,
            max_traces_per_chunk: 1,
            done: false,
        };
        let mut out_batches = Vec::new();
        let mut out_res = pool.register("out", false);
        while let Some(batch) = stream.next_batch(&cancel, Some(&mut out_res)).unwrap() {
            assert_eq!(out_res.size(), batch.memory_bytes());
            out_batches.push(batch);
            out_res.free();
        }
        assert!(out_batches.len() >= 2, "expected chunked emissions");
        let refs: Vec<&Batch> = out_batches.iter().collect();
        let actual = Batch::concat(Arc::clone(&full_schema), &refs, None).unwrap();
        assert_batches_bit_identical(&actual, &expected);
        assert_eq!(global.used(), 0);

        // Case B: Single-trace-only stream (1 batch, 1 row -> must NOT be dropped on EOF!).
        let single_b = build_full_partial_batch(&[base_row(7, 500, true, 7, 0.25)], &[0]);
        let mut merger = SortedTraceMerger::new(&merge_columns, &full_schema).unwrap();
        merger.push_range(&single_b, 0, single_b.raw_len()).unwrap();
        assert!(!merger.is_empty());
        assert_eq!(merger.len(), 1);
        assert!(merger.size() > 0);
        let merged_single = merger.finish().unwrap();
        assert_eq!(merged_single.num_rows(), 1);

        let mut single_stream = NativeTraceMergeStream {
            input: Box::new(VecBatchesStream {
                batches: VecDeque::from([single_b]),
            }),
            input_res: pool.register("in", false),
            merger: SortedTraceMerger::new(&merge_columns, &full_schema).unwrap(),
            merger_res: pool.register("merger", false),
            scratch_res: pool.register("scratch", false),
            post_filters: Vec::new(),
            projection: None,
            remaining: None,
            eval: EvalContext::new(0, cancel.clone()),
            active_input: None,
            budget_bytes: 64 * 1024,
            max_traces_per_chunk: 100,
            done: false,
        };
        let only = single_stream
            .next_batch(&cancel, Some(&mut out_res))
            .unwrap()
            .unwrap();
        assert_batches_bit_identical(&only, &merged_single);
        drop(only);
        out_res.free();
        assert!(
            single_stream
                .next_batch(&cancel, Some(&mut out_res))
                .unwrap()
                .is_none()
        );
        assert_eq!(global.used(), 0);
    }

    #[test]
    fn sorted_trace_merger_unsorted_duplicate_tags_and_metadata_single_row() {
        let full_schema = traces_schema();
        let merge_columns = MergeColumns::all();
        let mk = |t: u32, tags: Vec<&str>, meta: Vec<(&str, Option<&str>)>| PartialRowSpec {
            project: "proj".to_owned(),
            trace_id: tid(t),
            start_time: 1_000 + i64::from(t),
            end_time: 1_100 + i64::from(t),
            name: format!("t-{t}"),
            root_span_id: Some(sid(t)),
            has_root: true,
            status: "ok".to_owned(),
            session_id: None,
            user_id: None,
            environment: None,
            release: None,
            tags: tags.into_iter().map(str::to_owned).collect(),
            metadata: meta
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.map(str::to_owned)))
                .collect(),
            input_preview: None,
            output_preview: None,
            span_count: 1,
            error_count: 0,
            llm_count: 0,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            total_tokens: None,
            cost_total: None,
            models: vec![
                "z-model".to_owned(),
                "a-model".to_owned(),
                "z-model".to_owned(),
            ],
            prompts: vec!["p@2".to_owned(), "p@1".to_owned(), "p@1".to_owned()],
            last_ingest_time: 2_000,
            partial_count: 1,
        };

        // Row 1 (trace 2) is an interior single-row run (`run_end == offset + 1 && run_end < total`),
        // so it exercises the streaming fast path's checkpoint-and-sort fallback.
        let batch = build_full_partial_batch(
            &[
                mk(1, vec!["a"], vec![("k", Some("v"))]),
                mk(
                    2,
                    vec!["z", "a", "z", "m", "a"],
                    vec![
                        ("b", Some("2")),
                        ("a", Some("z")),
                        ("c", None),
                        ("a", Some("a")),
                        ("b", Some("1")),
                    ],
                ),
                mk(3, vec!["b"], vec![("x", Some("y"))]),
            ],
            &[0],
        );

        let mut oracle = OracleTraceMerger::new(&merge_columns, &full_schema).unwrap();
        oracle.push_batch(&batch).unwrap();
        let expected = oracle.finish(TimeBounds::open()).unwrap();

        let mut merger = SortedTraceMerger::new(&merge_columns, &full_schema).unwrap();
        merger.push_range(&batch, 0, batch.raw_len()).unwrap();
        let actual = merger.finish().unwrap();

        assert_batches_bit_identical(&actual, &expected);
        assert_eq!(
            actual
                .column_by_name(traces::TAGS)
                .unwrap()
                .get_scalar(1)
                .unwrap(),
            ScalarValue::List {
                item_type: DataType::Utf8,
                values: vec![
                    ScalarValue::Utf8("a".to_owned()),
                    ScalarValue::Utf8("m".to_owned()),
                    ScalarValue::Utf8("z".to_owned()),
                ],
            }
        );
        assert_eq!(
            actual
                .column_by_name(traces::METADATA)
                .unwrap()
                .get_scalar(1)
                .unwrap(),
            ScalarValue::MapUtf8(vec![
                ("a".to_owned(), Some("a".to_owned())),
                ("b".to_owned(), Some("1".to_owned())),
            ])
        );

        // Also check DataType::I64 timestamps and DataType::Null on nullable/list/map columns.
        let null_typed_schema = Schema::try_new_arc(vec![
            Field::new(traces::TRACE_ID, DataType::FixedBytes16, false),
            Field::new(traces::START_TIME, DataType::I64, false),
            Field::new(traces::HAS_ROOT, DataType::Bool, false),
            Field::new(traces::ROOT_SPAN_ID, DataType::Null, true),
            Field::new(traces::SESSION_ID, DataType::Null, true),
            Field::new(traces::TAGS, DataType::Null, true),
            Field::new(traces::METADATA, DataType::Null, true),
        ])
        .unwrap();
        let cols_subset = MergeColumns::new(&[
            traces::TRACE_ID,
            traces::START_TIME,
            traces::SESSION_ID,
            traces::TAGS,
            traces::METADATA,
        ]);
        let null_batch = Batch::try_new_all(
            Arc::clone(&null_typed_schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(FlatData::FixedBytes16(vec![tid(1), tid(2)]), None)
                        .unwrap(),
                ),
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![100, 200]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(FlatData::Bool(Bitmap::from_bools(&[false, true])), None)
                        .unwrap(),
                ),
                Column::Flat(FlatColumn::try_new(FlatData::Null(2), None).unwrap()),
                Column::Flat(FlatColumn::try_new(FlatData::Null(2), None).unwrap()),
                Column::Flat(FlatColumn::try_new(FlatData::Null(2), None).unwrap()),
                Column::Flat(FlatColumn::try_new(FlatData::Null(2), None).unwrap()),
            ],
            2,
        )
        .unwrap();
        let mut null_merger = SortedTraceMerger::new(&cols_subset, &full_schema).unwrap();
        null_merger
            .push_range(&null_batch, 0, null_batch.raw_len())
            .unwrap();
        let null_out = null_merger.finish().unwrap();
        assert_eq!(null_out.num_rows(), 2);
        assert!(
            null_out
                .column_by_name(traces::SESSION_ID)
                .unwrap()
                .is_null(0)
        );
        assert_eq!(
            null_out
                .column_by_name(traces::TAGS)
                .unwrap()
                .get_scalar(0)
                .unwrap(),
            ScalarValue::List {
                item_type: DataType::Utf8,
                values: vec![],
            }
        );
        assert_eq!(
            null_out
                .column_by_name(traces::METADATA)
                .unwrap()
                .get_scalar(0)
                .unwrap(),
            ScalarValue::MapUtf8(vec![])
        );
    }

    #[test]
    fn sorted_trace_merger_rejects_unsorted_trace_id() {
        let full_schema = Schema::try_new_arc(vec![
            Field::new(traces::TRACE_ID, DataType::FixedBytes16, false),
            Field::new(
                traces::START_TIME,
                DataType::TimestampNs { utc: true },
                false,
            ),
            Field::new(traces::HAS_ROOT, DataType::Bool, false),
            Field::new(traces::ROOT_SPAN_ID, DataType::FixedBytes8, true),
        ])
        .unwrap();
        let merge_columns = MergeColumns::new(&[traces::TRACE_ID, traces::START_TIME]);

        // 1. Descending within a batch.
        let desc_batch = make_minimal_partial_batch(&full_schema, &[(tid(2), 10), (tid(1), 20)]);
        let mut merger = SortedTraceMerger::new(&merge_columns, &full_schema).unwrap();
        let err = merger
            .push_range(&desc_batch, 0, desc_batch.raw_len())
            .unwrap_err();
        assert!(matches!(err, QueryError::Internal { .. }));

        // 2. Non-contiguous duplicate trace_id (`A, B, A`).
        let split_dup =
            make_minimal_partial_batch(&full_schema, &[(tid(1), 10), (tid(2), 20), (tid(1), 30)]);
        let mut merger2 = SortedTraceMerger::new(&merge_columns, &full_schema).unwrap();
        let err2 = merger2
            .push_range(&split_dup, 0, split_dup.raw_len())
            .unwrap_err();
        assert!(matches!(err2, QueryError::Internal { .. }));

        // 3. Descending across batch boundary via NativeTraceMergeStream cleans up reservations.
        let global = GlobalPool::new(1024 * 1024);
        let pool = global.try_admit(0, 1024 * 1024).unwrap();
        let cancel = CancelToken::new();
        let b1 = make_minimal_partial_batch(&full_schema, &[(tid(5), 10)]);
        let b2 = make_minimal_partial_batch(&full_schema, &[(tid(3), 20)]);
        let mut stream = NativeTraceMergeStream {
            input: Box::new(VecBatchesStream {
                batches: VecDeque::from([b1, b2]),
            }),
            input_res: pool.register("in", false),
            merger: SortedTraceMerger::new(&merge_columns, &full_schema).unwrap(),
            merger_res: pool.register("merger", false),
            scratch_res: pool.register("scratch", false),
            post_filters: Vec::new(),
            projection: None,
            remaining: None,
            eval: EvalContext::new(0, cancel.clone()),
            active_input: None,
            budget_bytes: 1024 * 1024,
            max_traces_per_chunk: 500,
            done: false,
        };
        let mut out_res = pool.register("out", false);
        let err3 = stream.next_batch(&cancel, Some(&mut out_res)).unwrap_err();
        assert!(matches!(err3, QueryError::Internal { .. }));
        assert_eq!(out_res.size(), 0);
        assert_eq!(stream.merger_res.size(), 0);
        assert_eq!(stream.input_res.size(), 0);
        assert_eq!(stream.scratch_res.size(), 0);
        assert!(stream.active_input.is_none());
        assert_eq!(global.used(), 0);
    }

    #[test]
    fn sorted_trace_merger_tight_pool_and_cancel_cleanup() {
        let full_schema = Schema::try_new_arc(vec![
            Field::new(traces::TRACE_ID, DataType::FixedBytes16, false),
            Field::new(
                traces::START_TIME,
                DataType::TimestampNs { utc: true },
                false,
            ),
            Field::new(traces::HAS_ROOT, DataType::Bool, false),
            Field::new(traces::ROOT_SPAN_ID, DataType::FixedBytes8, true),
        ])
        .unwrap();
        let merge_columns = MergeColumns::new(&[traces::TRACE_ID, traces::START_TIME]);

        let rows: Vec<([u8; 16], i64)> = (0..30).map(|i| (tid(i), 100 + i64::from(i))).collect();
        let b1 = make_minimal_partial_batch(&full_schema, &rows[..15]);
        let b2 = make_minimal_partial_batch(&full_schema, &rows[15..]);

        // 1. Mid-stream cancel after emitting first chunk (while `active_input` is populated).
        let global = GlobalPool::new(1024 * 1024);
        let pool = global.try_admit(0, 1024 * 1024).unwrap();
        let cancel = CancelToken::new();
        let mut stream = NativeTraceMergeStream {
            input: Box::new(VecBatchesStream {
                batches: VecDeque::from([b1.clone(), b2.clone()]),
            }),
            input_res: pool.register("in", false),
            merger: SortedTraceMerger::new(&merge_columns, &full_schema).unwrap(),
            merger_res: pool.register("merger", false),
            scratch_res: pool.register("scratch", false),
            post_filters: Vec::new(),
            projection: None,
            remaining: None,
            eval: EvalContext::new(0, cancel.clone()),
            active_input: None,
            budget_bytes: 1024 * 1024,
            max_traces_per_chunk: 5,
            done: false,
        };
        let mut out_res = pool.register("out", false);
        let first = stream
            .next_batch(&cancel, Some(&mut out_res))
            .unwrap()
            .unwrap();
        assert_eq!(first.num_rows(), 5);
        assert!(stream.active_input.is_some());
        drop(first);
        out_res.free();

        cancel.cancel();
        let err = stream.next_batch(&cancel, Some(&mut out_res)).unwrap_err();
        assert!(matches!(err, QueryError::Cancelled));
        assert_eq!(out_res.size(), 0);
        assert_eq!(stream.merger_res.size(), 0);
        assert_eq!(stream.input_res.size(), 0);
        assert_eq!(stream.scratch_res.size(), 0);
        assert!(stream.active_input.is_none());
        assert!(stream.merger.is_empty());
        assert_eq!(global.used(), 0);

        // 2. Tight-pool TooLarge during mid-batch chunking (`active_input.is_some()` path).
        let tight_cap = b1.memory_bytes() + 48;
        let tight_global = GlobalPool::new(tight_cap);
        let tight_pool = tight_global.try_admit(0, tight_cap).unwrap();
        let fresh_cancel = CancelToken::new();
        let mut tight_stream = NativeTraceMergeStream {
            input: Box::new(VecBatchesStream {
                batches: VecDeque::from([b1, b2]),
            }),
            input_res: tight_pool.register("in", false),
            merger: SortedTraceMerger::new(&merge_columns, &full_schema).unwrap(),
            merger_res: tight_pool.register("merger", false),
            scratch_res: tight_pool.register("scratch", false),
            post_filters: Vec::new(),
            projection: None,
            remaining: None,
            eval: EvalContext::new(0, fresh_cancel.clone()),
            active_input: None,
            budget_bytes: 256,
            max_traces_per_chunk: 8,
            done: false,
        };
        let mut tight_out = tight_pool.register("out", false);
        let err2 = tight_stream
            .next_batch(&fresh_cancel, Some(&mut tight_out))
            .unwrap_err();
        assert!(matches!(err2, QueryError::TooLarge));
        assert_eq!(tight_out.size(), 0);
        assert_eq!(tight_stream.merger_res.size(), 0);
        assert_eq!(tight_stream.input_res.size(), 0);
        assert_eq!(tight_stream.scratch_res.size(), 0);
        assert!(tight_stream.active_input.is_none());
        assert!(tight_stream.merger.is_empty());
        assert_eq!(tight_global.used(), 0);
    }

    #[test]
    fn native_trace_merge_stream_limit_filter_projection_and_limit_1_unsorted() {
        let full_schema = traces_schema();
        let merge_columns =
            MergeColumns::new(&[traces::TRACE_ID, traces::START_TIME, traces::SPAN_COUNT]);
        let input_indices = indices_of_names(&full_schema, &merge_columns.input()).unwrap();
        let output_indices = indices_of_names(&full_schema, &merge_columns.output()).unwrap();
        let merged_schema = Arc::new(full_schema.project(&output_indices).unwrap());
        let trace_id_idx = merged_schema.index_of(traces::TRACE_ID).unwrap();
        let span_count_idx = merged_schema.index_of(traces::SPAN_COUNT).unwrap();
        let projection = vec![trace_id_idx, span_count_idx];
        let projected_schema = Arc::new(merged_schema.project(&projection).unwrap());

        let mk_row = |t: u32, span_count: i64| PartialRowSpec {
            project: "proj".to_owned(),
            trace_id: tid(t),
            start_time: 1_000 + i64::from(t) * 10,
            end_time: 1_100 + i64::from(t) * 10,
            name: format!("t-{t}"),
            root_span_id: Some(sid(t)),
            has_root: true,
            status: "ok".to_owned(),
            session_id: None,
            user_id: None,
            environment: None,
            release: None,
            tags: Vec::new(),
            metadata: Vec::new(),
            input_preview: None,
            output_preview: None,
            span_count,
            error_count: 0,
            llm_count: 0,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            total_tokens: None,
            cost_total: None,
            models: Vec::new(),
            prompts: Vec::new(),
            last_ingest_time: 2_000,
            partial_count: 1,
        };

        // (a) Chunk 1 (traces 1..=2) filters completely to empty (`span_count < 2`);
        //     Chunk 2 (traces 3..=4) emits 2 projected rows;
        //     Chunk 3 (traces 5..=6, stopped at trace 7) exhausts `LIMIT 3` mid-chunk at trace 5;
        //     queued batch `b2` is never read and all reservations return to 0.
        let b1 = build_full_partial_batch(
            &[
                mk_row(1, 1),
                mk_row(2, 1),
                mk_row(3, 2),
                mk_row(4, 2),
                mk_row(5, 3),
                mk_row(6, 4),
                mk_row(7, 5),
            ],
            &[0],
        )
        .project(&input_indices)
        .unwrap();
        let b2 = build_full_partial_batch(&[mk_row(8, 6)], &[0])
            .project(&input_indices)
            .unwrap();

        let post_filters = vec![Expr::Binary {
            left: Box::new(Expr::Column {
                index: span_count_idx,
                name: traces::SPAN_COUNT.to_owned(),
                data_type: DataType::I64,
            }),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(ScalarValue::I64(2))),
            data_type: DataType::Bool,
        }];

        let global = GlobalPool::new(1024 * 1024);
        let pool = global.try_admit(0, 1024 * 1024).unwrap();
        let cancel = CancelToken::new();
        let mut stream = NativeTraceMergeStream {
            input: Box::new(VecBatchesStream {
                batches: VecDeque::from([b1, b2]),
            }),
            input_res: pool.register("in", false),
            merger: SortedTraceMerger::new(&merge_columns, &full_schema)
                .unwrap()
                .with_initial_cap(2),
            merger_res: pool.register("merger", false),
            scratch_res: pool.register("scratch", false),
            post_filters,
            projection: Some(projection),
            remaining: Some(Arc::new(AtomicUsize::new(3))),
            eval: EvalContext::new(0, cancel.clone()),
            active_input: None,
            budget_bytes: 1024 * 1024,
            max_traces_per_chunk: 2,
            done: false,
        };

        let mut out_batches = Vec::new();
        let mut out_res = pool.register("out", false);
        while let Some(batch) = stream.next_batch(&cancel, Some(&mut out_res)).unwrap() {
            assert_eq!(out_res.size(), batch.memory_bytes());
            out_batches.push(batch);
            out_res.free();
        }
        assert_eq!(out_res.size(), 0);
        assert_eq!(stream.merger_res.size(), 0);
        assert_eq!(stream.input_res.size(), 0);
        assert_eq!(stream.scratch_res.size(), 0);
        assert!(stream.active_input.is_none());
        assert!(stream.merger.is_empty());
        assert_eq!(global.used(), 0);

        assert_eq!(out_batches.len(), 2);
        let refs: Vec<&Batch> = out_batches.iter().collect();
        let actual = Batch::concat(projected_schema, &refs, None).unwrap();
        assert_eq!(actual.num_rows(), 3);
        let expected_rows = [(tid(3), 2i64), (tid(4), 2i64), (tid(5), 3i64)];
        for (row, (exp_tid, exp_spans)) in expected_rows.into_iter().enumerate() {
            assert_eq!(
                actual.column(0).unwrap().get_scalar(row).unwrap(),
                ScalarValue::FixedBytes16(exp_tid)
            );
            assert_eq!(
                actual.column(1).unwrap().get_scalar(row).unwrap(),
                ScalarValue::I64(exp_spans)
            );
        }

        // (b) Unsorted `trace_id` immediately following a 1-trace `max_traces_per_chunk == 1` /
        //     `remaining: Some(1)` boundary must return `Err(QueryError::Internal { .. })`
        //     immediately instead of emitting the 1-row limit and ignoring the unsorted row.
        let min_schema = Schema::try_new_arc(vec![
            Field::new(traces::TRACE_ID, DataType::FixedBytes16, false),
            Field::new(
                traces::START_TIME,
                DataType::TimestampNs { utc: true },
                false,
            ),
            Field::new(traces::HAS_ROOT, DataType::Bool, false),
            Field::new(traces::ROOT_SPAN_ID, DataType::FixedBytes8, true),
        ])
        .unwrap();
        let min_cols = MergeColumns::new(&[traces::TRACE_ID, traces::START_TIME]);

        for rows in [
            vec![(tid(2), 100), (tid(1), 200)],
            vec![(tid(2), 100), (tid(2), 110), (tid(1), 200)],
        ] {
            let unsorted = make_minimal_partial_batch(&min_schema, &rows);
            let mut bad_stream = NativeTraceMergeStream {
                input: Box::new(VecBatchesStream {
                    batches: VecDeque::from([unsorted]),
                }),
                input_res: pool.register("in", false),
                merger: SortedTraceMerger::new(&min_cols, &min_schema).unwrap(),
                merger_res: pool.register("merger", false),
                scratch_res: pool.register("scratch", false),
                post_filters: Vec::new(),
                projection: None,
                remaining: Some(Arc::new(AtomicUsize::new(1))),
                eval: EvalContext::new(0, cancel.clone()),
                active_input: None,
                budget_bytes: 1024 * 1024,
                max_traces_per_chunk: 1,
                done: false,
            };
            let err = bad_stream
                .next_batch(&cancel, Some(&mut out_res))
                .unwrap_err();
            assert!(matches!(err, QueryError::Internal { .. }));
            assert_eq!(out_res.size(), 0);
            assert_eq!(bad_stream.merger_res.size(), 0);
            assert_eq!(bad_stream.input_res.size(), 0);
            assert_eq!(bad_stream.scratch_res.size(), 0);
            assert!(bad_stream.active_input.is_none());
            assert!(bad_stream.merger.is_empty());
            assert_eq!(global.used(), 0);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn sorted_trace_merger_matches_oracle(
            seed in prop::collection::vec(any::<u8>(), 48..256),
            col_picks in prop::collection::vec(0..TraceColumn::COUNT, 0..16),
            lo_pick in 0..8usize,
            hi_pick in 0..8usize,
            has_lo in any::<bool>(),
            has_hi in any::<bool>(),
            budget_pick in 0..4usize,
            chunk_rows_pick in 1..4usize,
        ) {
            let mut at = 0usize;
            let mut next_u8 = || -> u8 {
                let b = seed[at % seed.len()];
                at += 1;
                b
            };

            let trace_count = 1 + usize::from(next_u8() % 6);
            let mut all_rows: Vec<PartialRowSpec> = Vec::new();
            let words = ["alpha", "beta", "gamma", "delta"];

            for t_idx in 1..=trace_count {
                let partial_count = 1 + usize::from(next_u8() % 4);
                for _ in 0..partial_count {
                    let start_time = 1_000 + i64::from(next_u8() % 6) * 10;
                    let has_root = next_u8().is_multiple_of(2);
                    let root_span_id = if next_u8().is_multiple_of(3) {
                        None
                    } else {
                        Some(sid(1 + u32::from(next_u8() % 8)))
                    };
                    let status = match next_u8() % 3 {
                        0 => "unset",
                        1 => "ok",
                        _ => "error",
                    }
                    .to_owned();
                    let opt_str = |b: u8| -> Option<String> {
                        b.is_multiple_of(2).then(|| words[usize::from(b) % words.len()].to_owned())
                    };
                    let opt_i64 = |b: u8| -> Option<i64> {
                        b.is_multiple_of(2).then(|| i64::from(b))
                    };
                    let tag_n = usize::from(next_u8() % 4);
                    let tags: Vec<String> = (0..tag_n)
                        .map(|_| words[usize::from(next_u8()) % words.len()].to_owned())
                        .collect();
                    let meta_n = usize::from(next_u8() % 4);
                    let metadata: Vec<(String, Option<String>)> = (0..meta_n)
                        .map(|_| {
                            let k = format!("k{}", next_u8() % 3);
                            let v = opt_str(next_u8());
                            (k, v)
                        })
                        .collect();
                    let cost_total = if next_u8().is_multiple_of(2) {
                        Some(f64::from(next_u8()) / 16.0)
                    } else {
                        None
                    };
                    let models = (0..usize::from(next_u8() % 3))
                        .map(|_| words[usize::from(next_u8()) % words.len()].to_owned())
                        .collect();
                    let prompts = (0..usize::from(next_u8() % 3))
                        .map(|_| format!("p@{}", next_u8() % 3))
                        .collect();

                    all_rows.push(PartialRowSpec {
                        project: "p1".to_owned(),
                        trace_id: tid(t_idx as u32),
                        start_time,
                        end_time: start_time + i64::from(next_u8() % 20),
                        name: words[usize::from(next_u8()) % words.len()].to_owned(),
                        root_span_id,
                        has_root,
                        status,
                        session_id: opt_str(next_u8()),
                        user_id: opt_str(next_u8()),
                        environment: opt_str(next_u8()),
                        release: opt_str(next_u8()),
                        tags,
                        metadata,
                        input_preview: opt_str(next_u8()),
                        output_preview: opt_str(next_u8()),
                        span_count: 1 + i64::from(next_u8() % 3),
                        error_count: i64::from(next_u8() % 2),
                        llm_count: i64::from(next_u8() % 2),
                        input_tokens: opt_i64(next_u8()),
                        output_tokens: opt_i64(next_u8()),
                        cache_read_tokens: opt_i64(next_u8()),
                        cache_write_tokens: opt_i64(next_u8()),
                        reasoning_tokens: opt_i64(next_u8()),
                        total_tokens: opt_i64(next_u8()),
                        cost_total,
                        models,
                        prompts,
                        last_ingest_time: start_time + 5,
                        partial_count: 1,
                    });
                }
            }

            let full_schema = traces_schema();
            let requested: Vec<&str> = col_picks
                .iter()
                .map(|&i| ALL_TRACE_COLUMNS[i].name())
                .collect();
            let merge_columns = MergeColumns::new(&requested);
            let input_indices = indices_of_names(&full_schema, &merge_columns.input()).unwrap();
            let output_indices = indices_of_names(&full_schema, &merge_columns.output()).unwrap();
            let output_schema = Arc::new(full_schema.project(&output_indices).unwrap());

            let bounds = TimeBounds {
                min: has_lo.then(|| 1_000 + (lo_pick as i64) * 10),
                max: has_hi.then(|| 1_000 + (hi_pick as i64) * 10),
            };

            // Split `all_rows` across arbitrary batch boundaries with varied encodings.
            let mut batches = Vec::new();
            let mut pos = 0usize;
            while pos < all_rows.len() {
                let take = (1 + usize::from(next_u8() % 4)).min(all_rows.len() - pos);
                let full_b = build_full_partial_batch(&all_rows[pos..pos + take], &seed[pos % seed.len()..]);
                batches.push(full_b.project(&input_indices).unwrap());
                pos += take;
            }

            let mut oracle = OracleTraceMerger::new(&merge_columns, &full_schema).unwrap();
            for b in &batches {
                oracle.push_batch(b).unwrap();
            }
            let expected = oracle.finish(bounds).unwrap();

            let budgets = [64usize, 256, 1024, 64 * 1024];
            let budget_bytes = budgets[budget_pick];
            let global = GlobalPool::new(16 * 1024 * 1024);
            let pool = global.try_admit(0, 16 * 1024 * 1024).unwrap();
            let cancel = CancelToken::new();
            let mut stream = NativeTraceMergeStream {
                input: Box::new(VecBatchesStream {
                    batches: VecDeque::from(batches),
                }),
                input_res: pool.register("in", false),
                merger: SortedTraceMerger::new(&merge_columns, &full_schema)
                    .unwrap()
                    .with_bounds(bounds)
                    .with_initial_cap(chunk_rows_pick),
                merger_res: pool.register("merger", false),
                scratch_res: pool.register("scratch", false),
                post_filters: Vec::new(),
                projection: None,
                remaining: None,
                eval: EvalContext::new(0, cancel.clone()),
                active_input: None,
                budget_bytes,
                max_traces_per_chunk: chunk_rows_pick,
                done: false,
            };

            let mut out_batches = Vec::new();
            let mut out_res = pool.register("out", false);
            while let Some(batch) = stream.next_batch(&cancel, Some(&mut out_res)).unwrap() {
                prop_assert_eq!(out_res.size(), batch.memory_bytes());
                out_batches.push(batch);
                out_res.free();
            }
            prop_assert_eq!(stream.merger_res.size(), 0);
            prop_assert_eq!(stream.input_res.size(), 0);
            prop_assert_eq!(stream.scratch_res.size(), 0);
            prop_assert!(stream.active_input.is_none());
            prop_assert_eq!(global.used(), 0);

            let actual = if out_batches.is_empty() {
                Batch::empty(output_schema)
            } else {
                let refs: Vec<&Batch> = out_batches.iter().collect();
                Batch::concat(output_schema, &refs, None).unwrap()
            };
            assert_batches_bit_identical(&actual, &expected);
        }
    }
}
