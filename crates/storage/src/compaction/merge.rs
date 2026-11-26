//! The streaming k-way merge of one time slice (spec 03 § During a merge).
//!
//! Inputs are sorted runs (one `ParquetSource` stream per input file, ordered
//! by `file.id` ascending), merged via a k-way min-heap with one active batch
//! per input in memory under `compaction.max_memory`. Duplicates are collapsed
//! per group (a trace for spans, a `created_at` value for scores), so a group
//! is never split across chunks.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use cetra_column::{
    Batch, CancelToken, Column, DataType, FlatData, GlobalPool, Reservation, ScalarValue, Schema,
    ScopedPool, Selection,
};
use cetra_core::schema::{scores, spans};
use cetra_query::exec::{
    ExecContext, ExecMetrics, PartitionStream, ScanSource, ScanSpec, SortKey, SpillManager,
    TimeRangeHint, WorkerPool, compare_rows_across,
};
use cetra_query::tombstone::filter_uncovered;
use cetra_query::{BinaryOp, EvalContext, Expr, QueryError};

use crate::catalog::{FileMeta, FileTable};
use crate::compaction::slice::Slice;
use crate::dedup;
use crate::error::CompactionError;
use crate::file_format::{BatchFileWriter, sort_columns, time_column};
use crate::partials::partials_from_spans_batch;
use crate::verify::Suspects;

/// Process-wide settings of the merge, built once at open (M4.6) and by tests.
#[derive(Debug)]
pub(crate) struct MergeContext {
    /// Rows per batch read from the inputs, at most. A merge of large rows reads
    /// fewer ([`batch_rows`]).
    pub(crate) batch_size: usize,
    /// Jobs that may merge at the same time (`compaction.threads`): each gets
    /// an equal share of the pool when its batch rows are chosen.
    pub(crate) jobs: usize,
    /// Dedicated compaction memory pool (`compaction.max_memory`).
    pub(crate) pool: Arc<GlobalPool>,
    /// Persistent native query runtime (`workers` + `metadata_cache`).
    pub(crate) qe_runtime: Arc<crate::QeRuntime>,
    /// Object store handle for native Parquet scans.
    pub(crate) store: Arc<dyn object_store::ObjectStore>,
    /// Suspect file queue for native Parquet scans.
    pub(crate) verify: Arc<crate::verify::VerifyQueue>,
    /// Tokio runtime handle for async object-store reads from worker threads.
    pub(crate) runtime_handle: tokio::runtime::Handle,
}

impl MergeContext {
    /// Constructs a [`MergeContext`] with native engine handles.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        batch_size: usize,
        jobs: usize,
        pool: Arc<GlobalPool>,
        qe_runtime: Arc<crate::QeRuntime>,
        store: Arc<dyn object_store::ObjectStore>,
        verify: Arc<crate::verify::VerifyQueue>,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            batch_size,
            jobs,
            pool,
            qe_runtime,
            store,
            verify,
            runtime_handle,
        }
    }
}

/// What `merge_slice` produced.
pub(crate) struct MergedSlice<W: Write + Send> {
    /// The writer the rows went into.
    pub(crate) writer: BatchFileWriter<W>,
    /// `spans` only: one partial batch per emitted chunk (`traces` schema).
    pub(crate) partials: Vec<Arc<Batch>>,
    /// Rows written to `writer`.
    pub(crate) rows: u64,
}

/// Per-input stream state held in the k-way merge.
struct StreamState {
    input_index: usize,
    stream: Box<dyn PartitionStream>,
    res: Reservation,
    batch: Option<Batch>,
    key_cols: Arc<Vec<Column>>,
    active_rows: Vec<u32>,
    cursor: usize,
}

impl StreamState {
    fn current_phys_row(&self) -> Option<u32> {
        self.active_rows.get(self.cursor).copied()
    }
}

/// Min-heap entry for one active stream's head row.
struct HeapEntry {
    stream_idx: usize,
    input_index: usize,
    phys_row: u32,
    key_cols: Arc<Vec<Column>>,
    sort_keys: Arc<Vec<SortKey>>,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for HeapEntry {}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // `BinaryHeap` is a max-heap; reverse so the smallest `(sort_keys, input_index)` pops first.
        let ord = compare_rows_across(
            &self.key_cols,
            self.phys_row as usize,
            &other.key_cols,
            other.phys_row as usize,
            &self.sort_keys,
        )
        .then_with(|| self.input_index.cmp(&other.input_index));
        ord.reverse()
    }
}

/// Merges `inputs` (one partition of `table`, `Spans` or `Scores`) restricted
/// to `slice` into `writer`.
///
/// `covered` is the covered-row predicate of the deletions pending when the
/// job started (spec 03 §7): covered rows are dropped before the dedup. For
/// `spans`, the returned partials are the trace partials of the emitted rows,
/// one batch per chunk. `drop_deleted` is ignored for spans.
pub(crate) async fn merge_slice<W: Write + Send + 'static>(
    ctx: &MergeContext,
    table: FileTable,
    inputs: &[Arc<FileMeta>],
    slice: Slice,
    drop_deleted: bool,
    covered: Option<&Expr>,
    writer: BatchFileWriter<W>,
) -> Result<MergedSlice<W>, CompactionError> {
    if table == FileTable::Traces {
        return Err(CompactionError::Input {
            reason: "traces are not merged",
        });
    }
    if slice.upper == Some(i64::MIN)
        || matches!((slice.lower, slice.upper), (Some(lo), Some(hi)) if lo >= hi)
        || inputs.is_empty()
    {
        return Ok(MergedSlice {
            writer,
            partials: Vec::new(),
            rows: 0,
        });
    }

    let mut files: Vec<&Arc<FileMeta>> = inputs.iter().collect();
    // UUIDv7, so oldest first; ties between equal sort keys keep input order.
    files.sort_unstable_by_key(|file| file.id);

    let schema = table.column_schema();
    let sort_keys = Arc::new(sort_keys_for(table, &schema)?);
    let spec = slice_scan_spec(table, &schema, slice)?;
    let batch_size = batch_rows(ctx, &files);

    let scoped_pool = ctx
        .pool
        .try_admit(0, ctx.pool.limit())
        .ok_or_else(|| CompactionError::from(QueryError::TooLarge))?;
    let workers = ctx.qe_runtime.workers().map_err(CompactionError::from)?;
    let cancel = CancelToken::new();
    let _cancel_guard = super::CancelOnDrop(cancel.clone());
    let eval_ctx = EvalContext {
        now_ns: crate::catalog::now_ns(),
        cancel: cancel.clone(),
    };
    let exec_ctx = ExecContext {
        pool: Arc::clone(&scoped_pool),
        eval: eval_ctx.clone(),
        spill: Arc::new(SpillManager::with_fixed_cap(PathBuf::new(), 0)),
        workers: Arc::clone(&workers),
        batch_size,
        target_partitions: 1,
        metrics: ExecMetrics::new(),
    };

    // Open one independent `ParquetSource` stream per input file in `file.id` ascending order.
    let mut raw_streams: Vec<(usize, Box<dyn PartitionStream>)> = Vec::with_capacity(files.len());
    for (input_index, file) in files.iter().enumerate() {
        let source = crate::engine::parquet_source::ParquetSource::new(
            Arc::clone(&ctx.store),
            table,
            Arc::clone(&schema),
            vec![Arc::clone(file)],
            Arc::from([]),
            Arc::clone(&ctx.qe_runtime.metadata_cache),
            Suspects::new(Arc::clone(&ctx.verify)),
            ctx.runtime_handle.clone(),
        );
        let partitions = source
            .open_partitions(&spec, &exec_ctx)
            .map_err(CompactionError::from)?;
        for stream in partitions {
            raw_streams.push((input_index, stream));
        }
    }

    if raw_streams.is_empty() {
        return Ok(MergedSlice {
            writer,
            partials: Vec::new(),
            rows: 0,
        });
    }

    let initial = pull_all_initial(&workers, &scoped_pool, &cancel, raw_streams).await?;

    let mut streams: Vec<StreamState> = Vec::with_capacity(initial.len());
    let mut heap: BinaryHeap<HeapEntry> = BinaryHeap::with_capacity(initial.len());
    for (input_index, stream, mut res, batch_opt) in initial {
        let Some(batch) = batch_opt else {
            res.free();
            continue;
        };
        if batch.num_rows() == 0 {
            res.free();
            continue;
        }
        let key_cols = Arc::new(extract_key_cols(table, &batch)?);
        let active_rows = batch
            .selection()
            .to_indices(batch.raw_len())
            .map_err(CompactionError::from)?;
        let Some(&first_phys) = active_rows.first() else {
            res.free();
            continue;
        };
        let stream_idx = streams.len();
        heap.push(HeapEntry {
            stream_idx,
            input_index,
            phys_row: first_phys,
            key_cols: Arc::clone(&key_cols),
            sort_keys: Arc::clone(&sort_keys),
        });
        streams.push(StreamState {
            input_index,
            stream,
            res,
            batch: Some(batch),
            key_cols,
            active_rows,
            cursor: 0,
        });
    }

    let mut writer = writer;
    let mut partials: Vec<Arc<Batch>> = Vec::new();
    let mut rows = 0_u64;

    let mut pending: Option<Batch> = None;
    let mut pending_res = scoped_pool.register("merge_pending", false);
    let mut chunk_pieces: Vec<Batch> = Vec::new();
    let mut chunk_res = scoped_pool.register("merge_chunk", false);
    let mut scratch_res = scoped_pool.register("merge_scratch", false);
    let mut chunk_picks: Vec<(usize, u32)> = Vec::with_capacity(batch_size);
    let mut buffered_rows = 0_usize;

    while let Some(top) = heap.pop() {
        let stream_idx = top.stream_idx;
        chunk_picks.push((stream_idx, top.phys_row));
        buffered_rows += 1;

        streams[stream_idx].cursor += 1;
        if let Some(next_phys) = streams[stream_idx].current_phys_row() {
            heap.push(HeapEntry {
                stream_idx,
                input_index: streams[stream_idx].input_index,
                phys_row: next_phys,
                key_cols: Arc::clone(&streams[stream_idx].key_cols),
                sort_keys: Arc::clone(&sort_keys),
            });
        } else {
            // Stream's current batch is exhausted: materialize any picks referencing it
            // before dropping its batch and pulling the next one.
            if let Some(piece) = flush_picks(
                &schema,
                &streams,
                &mut chunk_picks,
                &mut chunk_res,
                &mut scratch_res,
            )? {
                chunk_pieces.push(piece);
            }
            let st = &mut streams[stream_idx];
            st.batch = None;
            st.active_rows.clear();
            st.cursor = 0;
            let pulled = pull_stream_next(
                &workers,
                std::mem::replace(&mut st.stream, Box::new(EmptyStream)),
                cancel.clone(),
                std::mem::replace(&mut st.res, scoped_pool.register("merge_empty", false)),
            )
            .await?;
            st.stream = pulled.0;
            st.res = pulled.1;
            if let Some(next_batch) = pulled.2
                && next_batch.num_rows() > 0
            {
                let key_cols = Arc::new(extract_key_cols(table, &next_batch)?);
                let active_rows = next_batch
                    .selection()
                    .to_indices(next_batch.raw_len())
                    .map_err(CompactionError::from)?;
                if let Some(&first_phys) = active_rows.first() {
                    st.key_cols = Arc::clone(&key_cols);
                    st.active_rows = active_rows;
                    st.batch = Some(next_batch);
                    heap.push(HeapEntry {
                        stream_idx,
                        input_index: st.input_index,
                        phys_row: first_phys,
                        key_cols,
                        sort_keys: Arc::clone(&sort_keys),
                    });
                } else {
                    st.res.free();
                }
            }
        }

        if buffered_rows >= batch_size || heap.is_empty() {
            if let Some(piece) = flush_picks(
                &schema,
                &streams,
                &mut chunk_picks,
                &mut chunk_res,
                &mut scratch_res,
            )? {
                chunk_pieces.push(piece);
            }
            if chunk_pieces.is_empty() && pending.is_none() {
                buffered_rows = 0;
                continue;
            }
            let combined = combine_pending_and_pieces(
                &schema,
                pending.take(),
                &mut pending_res,
                &mut chunk_pieces,
                &mut chunk_res,
                &mut scratch_res,
            )?;
            buffered_rows = 0;
            if combined.num_rows() == 0 {
                scratch_res.free();
                continue;
            }

            let last = combined.num_rows() - 1;
            let same_as_next = match heap.peek() {
                Some(next) => same_group_with_heap_top(table, &combined, last, next)?,
                None => false,
            };

            if !same_as_next {
                // Entire `combined` ends on a group boundary: transfer its reservation
                // from `scratch_res` to `chunk_res` without double-charging the pool.
                std::mem::swap(&mut chunk_res, &mut scratch_res);
                let (next_writer, next_partials, written) = emit_chunk(
                    table,
                    drop_deleted,
                    covered,
                    writer,
                    combined,
                    partials,
                    &eval_ctx,
                    &mut chunk_res,
                    &mut scratch_res,
                )
                .await?;
                writer = next_writer;
                partials = next_partials;
                rows = rows.saturating_add(written);
                chunk_res.free();
                scratch_res.free();
            } else {
                // Current group continues into `heap.peek()`: split off completed prefix groups.
                let mut split = last;
                while split > 0 && same_group(table, &combined, split - 1, last)? {
                    split -= 1;
                }
                if split > 0 {
                    let chunk = combined
                        .slice(0, split, None)
                        .map_err(CompactionError::from)?;
                    let tail = combined
                        .slice(split, combined.num_rows() - split, None)
                        .map_err(CompactionError::from)?;
                    drop(combined);
                    scratch_res.free();

                    let tail_bytes = tail.memory_bytes();
                    pending_res
                        .resize(tail_bytes)
                        .map_err(CompactionError::from)?;
                    pending = Some(tail);

                    let chunk_bytes = chunk.memory_bytes();
                    chunk_res
                        .resize(chunk_bytes)
                        .map_err(CompactionError::from)?;
                    let (next_writer, next_partials, written) = emit_chunk(
                        table,
                        drop_deleted,
                        covered,
                        writer,
                        chunk,
                        partials,
                        &eval_ctx,
                        &mut chunk_res,
                        &mut scratch_res,
                    )
                    .await?;
                    writer = next_writer;
                    partials = next_partials;
                    rows = rows.saturating_add(written);
                    chunk_res.free();
                    scratch_res.free();
                } else {
                    // Entire `combined` belongs to one straddling group.
                    let pending_bytes = combined.memory_bytes();
                    drop(scratch_res);
                    scratch_res = scoped_pool.register("merge_scratch", false);
                    pending_res
                        .resize(pending_bytes)
                        .map_err(CompactionError::from)?;
                    pending = Some(combined);
                }
            }
        }
    }

    if let Some(tail) = pending.take()
        && tail.num_rows() > 0
    {
        let (next_writer, next_partials, written) = emit_chunk(
            table,
            drop_deleted,
            covered,
            writer,
            tail,
            partials,
            &eval_ctx,
            &mut pending_res,
            &mut scratch_res,
        )
        .await?;
        writer = next_writer;
        partials = next_partials;
        rows = rows.saturating_add(written);
    }

    pending_res.free();
    chunk_res.free();
    scratch_res.free();
    for st in &mut streams {
        st.batch = None;
        st.res.free();
    }

    Ok(MergedSlice {
        writer,
        partials,
        rows,
    })
}

/// Sentinel empty stream used during `std::mem::replace`.
struct EmptyStream;

impl PartitionStream for EmptyStream {
    fn next_batch(
        &mut self,
        _cancel: &CancelToken,
        _res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        Ok(None)
    }
}

type InitialStreamOut = (usize, Box<dyn PartitionStream>, Reservation, Option<Batch>);

async fn pull_all_initial(
    workers: &Arc<WorkerPool>,
    scoped_pool: &Arc<ScopedPool>,
    cancel: &CancelToken,
    raw_streams: Vec<(usize, Box<dyn PartitionStream>)>,
) -> Result<Vec<InitialStreamOut>, CompactionError> {
    let workers = Arc::clone(workers);
    let scoped_pool = Arc::clone(scoped_pool);
    let cancel = cancel.clone();
    tokio::task::spawn_blocking(move || {
        let tasks: Vec<_> = raw_streams
            .into_iter()
            .map(|(input_index, mut stream)| {
                let pool = Arc::clone(&scoped_pool);
                let cancel = cancel.clone();
                move |_worker_idx: usize| -> Result<InitialStreamOut, QueryError> {
                    let mut res = pool.register("merge_stream", false);
                    let mut batch_out = None;
                    loop {
                        res.free();
                        match stream.next_batch(&cancel, Some(&mut res))? {
                            Some(b) if b.is_empty() => continue,
                            Some(b) => {
                                batch_out = Some(b);
                                break;
                            }
                            None => {
                                res.free();
                                break;
                            }
                        }
                    }
                    Ok((input_index, stream, res, batch_out))
                }
            })
            .collect();
        workers
            .run_scoped(&cancel, tasks)
            .map_err(CompactionError::from)
    })
    .await
    .map_err(join_error)?
}

async fn pull_stream_next(
    workers: &Arc<WorkerPool>,
    mut stream: Box<dyn PartitionStream>,
    cancel: CancelToken,
    mut res: Reservation,
) -> Result<(Box<dyn PartitionStream>, Reservation, Option<Batch>), CompactionError> {
    let workers = Arc::clone(workers);
    tokio::task::spawn_blocking(move || {
        let task_cancel = cancel.clone();
        let mut out = workers
            .run_scoped(
                &cancel,
                vec![move |_worker_idx| {
                    let mut batch_out = None;
                    loop {
                        res.free();
                        match stream.next_batch(&task_cancel, Some(&mut res))? {
                            Some(b) if b.is_empty() => continue,
                            Some(b) => {
                                batch_out = Some(b);
                                break;
                            }
                            None => {
                                res.free();
                                break;
                            }
                        }
                    }
                    Ok((stream, res, batch_out))
                }],
            )
            .map_err(CompactionError::from)?;
        out.pop().ok_or(CompactionError::Task)
    })
    .await
    .map_err(join_error)?
}

/// Gathers `indices` directly from `batch.columns()` into a `Selection::All` batch,
/// charging every allocated byte to `res` (and rolling back to `before` on error).
fn gather_batch_rows(
    schema: &Arc<Schema>,
    batch: &Batch,
    indices: &[u32],
    res: &mut Reservation,
) -> Result<Batch, CompactionError> {
    let before = res.size();
    let rollback = |res: &mut Reservation| {
        let cur = res.size();
        if cur > before {
            res.shrink(cur - before);
        }
    };
    let mut cols = Vec::with_capacity(batch.columns().len());
    for col in batch.columns() {
        match col.take(indices, Some(res)) {
            Ok(flat) => cols.push(Column::Flat(flat)),
            Err(err) => {
                rollback(res);
                return Err(CompactionError::from(err));
            }
        }
    }
    let piece = Batch::try_new_all(Arc::clone(schema), cols, indices.len()).map_err(|err| {
        rollback(res);
        CompactionError::from(err)
    })?;
    let Some(target) = before.checked_add(piece.memory_bytes()) else {
        rollback(res);
        return Err(CompactionError::from(QueryError::TooLarge));
    };
    match target.cmp(&res.size()) {
        Ordering::Greater => {
            if let Err(err) = res.try_grow(target - res.size()) {
                rollback(res);
                return Err(CompactionError::from(err));
            }
        }
        Ordering::Less => res.shrink(res.size() - target),
        Ordering::Equal => {}
    }
    Ok(piece)
}

fn flush_picks(
    schema: &Arc<Schema>,
    streams: &[StreamState],
    picks: &mut Vec<(usize, u32)>,
    chunk_res: &mut Reservation,
    scratch_res: &mut Reservation,
) -> Result<Option<Batch>, CompactionError> {
    if picks.is_empty() {
        return Ok(None);
    }
    let first_stream = picks[0].0;
    if picks.iter().all(|&(s, _)| s == first_stream) {
        let batch = streams[first_stream]
            .batch
            .as_ref()
            .ok_or(CompactionError::Input {
                reason: "missing stream batch",
            })?;
        let indices: Vec<u32> = picks.drain(..).map(|(_, phys)| phys).collect();
        let piece = gather_batch_rows(schema, batch, &indices, chunk_res)?;
        return Ok(Some(piece));
    }

    let mut per_stream: Vec<(usize, Vec<u32>)> = Vec::new();
    let mut stream_pos: Vec<(usize, u32)> = Vec::with_capacity(picks.len());
    for &(s, phys) in picks.iter() {
        let slot = match per_stream.iter().position(|(id, _)| *id == s) {
            Some(pos) => pos,
            None => {
                per_stream.push((s, Vec::new()));
                per_stream.len() - 1
            }
        };
        let local_idx = u32::try_from(per_stream[slot].1.len())
            .map_err(|_| CompactionError::from(QueryError::TooLarge))?;
        per_stream[slot].1.push(phys);
        stream_pos.push((slot, local_idx));
    }

    let mut subs: Vec<Batch> = Vec::with_capacity(per_stream.len());
    let mut bases: Vec<u32> = Vec::with_capacity(per_stream.len());
    let mut offset = 0_u32;
    let mut sub_res = scratch_res.new_empty("merge_sub");
    for (s, indices) in per_stream {
        let batch = streams[s].batch.as_ref().ok_or(CompactionError::Input {
            reason: "missing stream batch",
        })?;
        bases.push(offset);
        offset = offset.saturating_add(
            u32::try_from(indices.len())
                .map_err(|_| CompactionError::from(QueryError::TooLarge))?,
        );
        let sub = gather_batch_rows(schema, batch, &indices, &mut sub_res)?;
        subs.push(sub);
    }

    let perm: Vec<u32> = stream_pos
        .into_iter()
        .map(|(slot, local_idx)| bases[slot] + local_idx)
        .collect();
    let sub_refs: Vec<&Batch> = subs.iter().collect();
    let combined = Batch::concat(Arc::clone(schema), &sub_refs, Some(scratch_res))
        .map_err(CompactionError::from)?;
    drop(subs);
    sub_res.free();

    let reordered = combined
        .into_with_selection(Selection::Indices(perm))
        .map_err(CompactionError::from)?;
    let piece = reordered
        .compact(Some(chunk_res))
        .map_err(CompactionError::from)?;
    drop(reordered);
    scratch_res.free();
    picks.clear();
    Ok(Some(piece))
}

fn combine_pending_and_pieces(
    schema: &Arc<Schema>,
    pending: Option<Batch>,
    pending_res: &mut Reservation,
    chunk_pieces: &mut Vec<Batch>,
    chunk_res: &mut Reservation,
    scratch_res: &mut Reservation,
) -> Result<Batch, CompactionError> {
    let combined = {
        let mut refs: Vec<&Batch> =
            Vec::with_capacity(usize::from(pending.is_some()) + chunk_pieces.len());
        if let Some(ref p) = pending {
            refs.push(p);
        }
        for piece in chunk_pieces.iter() {
            refs.push(piece);
        }
        Batch::concat(Arc::clone(schema), &refs, Some(scratch_res))
            .map_err(CompactionError::from)?
    };
    drop(pending);
    pending_res.free();
    chunk_pieces.clear();
    chunk_res.free();
    Ok(combined)
}

/// Fewest rows per input batch. Below it the per-batch overhead dominates.
const MIN_BATCH_ROWS: usize = 64;
/// Decoded bytes per stored byte assumed for a row. Text compresses about 4×
/// with zstd (track R M7); twice that leaves room for Arrow's offsets and buffers.
const DECODED_PER_STORED: u64 = 8;
/// Batches of each input the merge may hold at once (the current batch and the
/// one being read), plus the output it builds.
const BATCHES_PER_INPUT: u64 = 4;

/// Rows per batch read from each input of one merge. The merge holds a batch
/// per input, so with large rows `ctx.batch_size` rows of every input can pass
/// the compaction pool, and the job fails every pass (soak finding M7-1). This
/// lowers the rows so that `BATCHES_PER_INPUT` batches of every input fit in
/// the job's share of the pool (`limit / ctx.jobs`), estimating a row's decoded
/// size from the largest stored bytes per row among the inputs. Between
/// `MIN_BATCH_ROWS` and `ctx.batch_size`; `ctx.batch_size` without a finite pool.
fn batch_rows(ctx: &MergeContext, inputs: &[&Arc<FileMeta>]) -> usize {
    let limit = ctx.pool.limit();
    if limit == usize::MAX {
        return ctx.batch_size;
    }
    let stored_per_row = inputs
        .iter()
        .filter(|file| file.rows > 0)
        .map(|file| file.size.div_ceil(file.rows))
        .max()
        .unwrap_or(1)
        .max(1);
    let share = u64::try_from(limit / ctx.jobs.max(1)).unwrap_or(u64::MAX);
    let per_batch = stored_per_row
        .saturating_mul(DECODED_PER_STORED)
        .saturating_mul(BATCHES_PER_INPUT)
        .saturating_mul(u64::try_from(inputs.len().max(1)).unwrap_or(u64::MAX));
    let rows = usize::try_from(share / per_batch).unwrap_or(usize::MAX);
    let floor = MIN_BATCH_ROWS.min(ctx.batch_size.max(1));
    rows.clamp(floor, ctx.batch_size.max(floor))
}

fn sort_keys_for(table: FileTable, schema: &Schema) -> Result<Vec<SortKey>, CompactionError> {
    let mut keys = Vec::new();
    for name in sort_columns(table) {
        let index = schema.index_of(name).ok_or(CompactionError::Input {
            reason: "sort column is missing",
        })?;
        let data_type = schema.fields()[index].data_type.clone();
        keys.push(SortKey {
            expr: Expr::Column {
                index,
                name: (*name).to_owned(),
                data_type,
            },
            descending: false,
            nulls_first: false,
        });
    }
    if keys.is_empty() {
        return Err(CompactionError::Input {
            reason: "empty ordering",
        });
    }
    Ok(keys)
}

fn extract_key_cols(table: FileTable, batch: &Batch) -> Result<Vec<Column>, CompactionError> {
    let mut cols = Vec::new();
    for name in sort_columns(table) {
        let col = batch
            .column_by_name(name)
            .ok_or(CompactionError::Input {
                reason: "sort column is missing",
            })?
            .clone();
        cols.push(col);
    }
    Ok(cols)
}

fn slice_scan_spec(
    table: FileTable,
    schema: &Schema,
    slice: Slice,
) -> Result<ScanSpec, CompactionError> {
    let col_name = time_column(table);
    let col_idx = schema.index_of(col_name).ok_or(CompactionError::Input {
        reason: "time column is missing",
    })?;
    let DataType::TimestampNs { utc } = schema.fields()[col_idx].data_type else {
        return Err(CompactionError::Input {
            reason: "time column is missing",
        });
    };
    let col_expr = Expr::Column {
        index: col_idx,
        name: col_name.to_owned(),
        data_type: DataType::TimestampNs { utc },
    };
    let mut filters = Vec::new();
    if let Some(lower) = slice.lower {
        filters.push(Expr::Binary {
            left: Box::new(col_expr.clone()),
            op: BinaryOp::GtEq,
            right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                value: lower,
                utc,
            })),
            data_type: DataType::Bool,
        });
    }
    if let Some(upper) = slice.upper {
        filters.push(Expr::Binary {
            left: Box::new(col_expr),
            op: BinaryOp::Lt,
            right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                value: upper,
                utc,
            })),
            data_type: DataType::Bool,
        });
    }
    let time_range = TimeRangeHint {
        min_ns: slice.lower,
        max_ns: slice.upper.and_then(|u| u.checked_sub(1)),
        around_ns: None,
    };
    Ok(ScanSpec {
        time_range,
        filters,
        ..ScanSpec::default()
    })
}

/// Whether rows `left` and `right` of a `Selection::All` batch belong to one group:
/// the same `trace_id` for spans, the same `created_at` for scores.
fn same_group(
    table: FileTable,
    batch: &Batch,
    left: usize,
    right: usize,
) -> Result<bool, CompactionError> {
    match table {
        FileTable::Spans => {
            let col = batch
                .column_by_name(spans::TRACE_ID)
                .filter(|c| c.data_type() == DataType::FixedBytes16)
                .ok_or(CompactionError::Input {
                    reason: "missing group column",
                })?;
            let (Some(a), Some(b)) = (read_fixed16(col, left), read_fixed16(col, right)) else {
                return Err(CompactionError::Input {
                    reason: "missing group column",
                });
            };
            Ok(a == b)
        }
        FileTable::Scores => {
            let col = batch
                .column_by_name(scores::CREATED_AT)
                .filter(|c| matches!(c.data_type(), DataType::TimestampNs { .. }))
                .ok_or(CompactionError::Input {
                    reason: "missing group column",
                })?;
            let (Some(a), Some(b)) = (read_ts(col, left), read_ts(col, right)) else {
                return Err(CompactionError::Input {
                    reason: "missing group column",
                });
            };
            Ok(a == b)
        }
        FileTable::Traces => Err(CompactionError::Input {
            reason: "traces are not merged",
        }),
    }
}

fn same_group_with_heap_top(
    table: FileTable,
    batch: &Batch,
    row: usize,
    heap_top: &HeapEntry,
) -> Result<bool, CompactionError> {
    let top_phys = heap_top.phys_row as usize;
    match table {
        FileTable::Spans => {
            let col = batch
                .column_by_name(spans::TRACE_ID)
                .filter(|c| c.data_type() == DataType::FixedBytes16)
                .ok_or(CompactionError::Input {
                    reason: "missing group column",
                })?;
            let top_col = heap_top.key_cols.first().ok_or(CompactionError::Input {
                reason: "missing group column",
            })?;
            let (Some(a), Some(b)) = (read_fixed16(col, row), read_fixed16(top_col, top_phys))
            else {
                return Err(CompactionError::Input {
                    reason: "missing group column",
                });
            };
            Ok(a == b)
        }
        FileTable::Scores => {
            let col = batch
                .column_by_name(scores::CREATED_AT)
                .filter(|c| matches!(c.data_type(), DataType::TimestampNs { .. }))
                .ok_or(CompactionError::Input {
                    reason: "missing group column",
                })?;
            let top_col = heap_top.key_cols.first().ok_or(CompactionError::Input {
                reason: "missing group column",
            })?;
            let (Some(a), Some(b)) = (read_ts(col, row), read_ts(top_col, top_phys)) else {
                return Err(CompactionError::Input {
                    reason: "missing group column",
                });
            };
            Ok(a == b)
        }
        FileTable::Traces => Err(CompactionError::Input {
            reason: "traces are not merged",
        }),
    }
}

fn read_fixed16(col: &Column, row: usize) -> Option<[u8; 16]> {
    match col {
        Column::Flat(flat) => {
            if flat.is_null(row) {
                return None;
            }
            match flat.data() {
                FlatData::FixedBytes16(values) => values.get(row).copied(),
                _ => None,
            }
        }
        Column::Const { value, len } => (row < *len).then_some(value).and_then(|val| match val {
            ScalarValue::FixedBytes16(bytes) => Some(*bytes),
            _ => None,
        }),
        Column::Dict(dict) => {
            if dict.is_null(row) {
                return None;
            }
            let code = *dict.codes().get(row)? as usize;
            if dict.values().is_null(code) {
                return None;
            }
            match dict.values().data() {
                FlatData::FixedBytes16(values) => values.get(code).copied(),
                _ => None,
            }
        }
        Column::Rle(rle) => {
            if row >= rle.len() {
                return None;
            }
            let run = rle.run_ends().partition_point(|&end| (end as usize) <= row);
            if rle.values().is_null(run) {
                return None;
            }
            match rle.values().data() {
                FlatData::FixedBytes16(values) => values.get(run).copied(),
                _ => None,
            }
        }
    }
}

fn read_ts(col: &Column, row: usize) -> Option<i64> {
    match col {
        Column::Flat(flat) => {
            if flat.is_null(row) {
                return None;
            }
            match flat.data() {
                FlatData::TimestampNs { values, .. } => values.get(row).copied(),
                _ => None,
            }
        }
        Column::Const { value, len } => (row < *len).then_some(value).and_then(|val| match val {
            ScalarValue::TimestampNs { value, .. } => Some(*value),
            _ => None,
        }),
        Column::Dict(dict) => {
            if dict.is_null(row) {
                return None;
            }
            let code = *dict.codes().get(row)? as usize;
            if dict.values().is_null(code) {
                return None;
            }
            match dict.values().data() {
                FlatData::TimestampNs { values, .. } => values.get(code).copied(),
                _ => None,
            }
        }
        Column::Rle(rle) => {
            if row >= rle.len() {
                return None;
            }
            let run = rle.run_ends().partition_point(|&end| (end as usize) <= row);
            if rle.values().is_null(run) {
                return None;
            }
            match rle.values().data() {
                FlatData::TimestampNs { values, .. } => values.get(run).copied(),
                _ => None,
            }
        }
    }
}

/// Deduplicates one chunk and writes it, computing the trace partials for
/// spans. Covered rows are dropped first (spec 03 §7): a chunk left with no
/// row writes nothing and pushes no partial. The blocking write runs on
/// `spawn_blocking`.
#[allow(clippy::too_many_arguments)]
async fn emit_chunk<W: Write + Send + 'static>(
    table: FileTable,
    drop_deleted: bool,
    covered: Option<&Expr>,
    writer: BatchFileWriter<W>,
    chunk: Batch,
    mut partials: Vec<Arc<Batch>>,
    eval_ctx: &EvalContext,
    chunk_res: &mut Reservation,
    scratch_res: &mut Reservation,
) -> Result<(BatchFileWriter<W>, Vec<Arc<Batch>>, u64), CompactionError> {
    let filtered = match covered {
        Some(predicate) => {
            let kept = filter_uncovered(&chunk, predicate, eval_ctx, Some(scratch_res)).map_err(
                |err| {
                    if matches!(err, QueryError::TooLarge | QueryError::Cancelled) {
                        CompactionError::from(err)
                    } else {
                        CompactionError::Deletion {
                            reason: "applying the deletion filter",
                        }
                    }
                },
            )?;
            drop(chunk);
            if kept.num_rows() == 0 {
                return Ok((writer, partials, 0));
            }
            kept
        }
        None => chunk,
    };
    match table {
        FileTable::Spans => {
            let rows = dedup::latest_spans_batch(&filtered)?;
            drop(filtered);
            chunk_res.free();
            scratch_res
                .resize(rows.memory_bytes())
                .map_err(CompactionError::from)?;
            let partial = partials_from_spans_batch(&rows)?;
            let written = u64::try_from(rows.num_rows()).unwrap_or(u64::MAX);
            let writer = blocking_write(writer, rows).await?;
            scratch_res.free();
            partials.push(Arc::new(partial));
            Ok((writer, partials, written))
        }
        FileTable::Scores => {
            let rows = dedup::latest_scores_batch(&filtered, drop_deleted)?;
            drop(filtered);
            chunk_res.free();
            if rows.num_rows() == 0 {
                scratch_res.free();
                return Ok((writer, partials, 0));
            }
            scratch_res
                .resize(rows.memory_bytes())
                .map_err(CompactionError::from)?;
            let written = u64::try_from(rows.num_rows()).unwrap_or(u64::MAX);
            let writer = blocking_write(writer, rows).await?;
            scratch_res.free();
            Ok((writer, partials, written))
        }
        FileTable::Traces => Err(CompactionError::Input {
            reason: "traces are not merged",
        }),
    }
}

async fn blocking_write<W: Write + Send + 'static>(
    mut writer: BatchFileWriter<W>,
    rows: Batch,
) -> Result<BatchFileWriter<W>, CompactionError> {
    tokio::task::spawn_blocking(move || writer.write(&rows).map(|()| writer))
        .await
        .map_err(join_error)?
        .map_err(CompactionError::Write)
}

fn join_error(_: tokio::task::JoinError) -> CompactionError {
    CompactionError::Task
}

#[cfg(test)]
mod tests {
    use bytesize::ByteSize;
    use std::io::{Read, Seek, SeekFrom};
    use std::sync::Arc;

    use cetra_column::{Batch, ScalarValue, Selection};
    use cetra_core::batch::scores_to_batch;
    use cetra_core::enums::ScoreSource;
    use cetra_core::ids::ProjectId;
    use cetra_core::schema::{scores, spans, traces};
    use cetra_core::score::{Score, ScoreTarget, ScoreValue};
    use chrono::NaiveDate;
    use uuid::Uuid;

    use super::{MergeContext, merge_slice};
    use crate::catalog::{FileId, FileMeta, FileTable};
    use crate::compaction::slice::{Slice, slices, time_values};
    use crate::config::{
        FlushConfig, MemtableConfig, ObjectStoreUrl, QueryConfig, S3Config, StorageConfig,
        WalConfig,
    };
    use crate::error::CompactionError;
    use crate::file_format::{BatchFileWriter, write_file_batch};
    use crate::hooks::Hooks;
    use crate::objects::Objects;
    use crate::partials::partials_from_spans_batch;
    use crate::reads::columns::{bool_at, i64_at, require_col, string_at, time_at};
    use crate::test_support::{parquet_rows, span_row, span_rows};
    use crate::verify::may_be_corrupt;

    fn project() -> ProjectId {
        "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
    }

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 11).unwrap()
    }

    async fn objects(dir: &tempfile::TempDir) -> Arc<Objects> {
        let config = StorageConfig {
            delete_objects: true,
            data_dir: dir.path().to_path_buf(),
            min_free_bytes: ByteSize::b(0),
            object_store: Some(ObjectStoreUrl::Local(dir.path().join("objects"))),
            s3: S3Config::default(),
            wal: WalConfig::default(),
            flush: FlushConfig::default(),
            memtable: MemtableConfig::default(),
            query: QueryConfig::default(),
            anchor: Default::default(),
            gc: Default::default(),
            compaction: Default::default(),
            deletion: Default::default(),
        };
        Arc::new(Objects::new(&config, Hooks::default()).await.unwrap())
    }

    /// The merge context of the plan: a 64 MiB pool and the store.
    fn ctx(objects: &Arc<Objects>, batch_size: usize) -> MergeContext {
        MergeContext::new(
            batch_size,
            1,
            cetra_column::GlobalPool::new(64 << 20),
            crate::QeRuntime::for_test(),
            objects.store(),
            Arc::new(crate::verify::VerifyQueue::default()),
            tokio::runtime::Handle::current(),
        )
    }

    async fn write_input(objects: &Arc<Objects>, table: FileTable, rows: Batch) -> Arc<FileMeta> {
        let col_batch = Arc::new(rows);
        let written = write_file_batch(table, 0, &[col_batch]).unwrap();
        let file_day = chrono::DateTime::from_timestamp_nanos(written.min_time).date_naive();
        let (path, id) = Objects::table_file_path(table, project(), file_day, 0);
        objects
            .put_durable(&path, written.bytes.clone())
            .await
            .unwrap();
        Arc::new(FileMeta {
            id,
            table,
            project: project(),
            day: file_day,
            level: 0,
            path,
            size: u64::try_from(written.bytes.len()).unwrap_or(u64::MAX),
            footer_size: written.footer_size,
            rows: written.rows,
            min_time: written.min_time,
            max_time: written.max_time,
            min_ingest_time: written.min_ingest_time,
            max_ingest_time: written.max_ingest_time,
            fully_merged: false,
            deletions_applied_seq: 0,
        })
    }

    /// One merge of one slice into a temp file, read back whole.
    async fn merge_read(
        ctx: &MergeContext,
        table: FileTable,
        inputs: &[Arc<FileMeta>],
        slice: Slice,
        drop_deleted: bool,
    ) -> (Batch, Vec<Batch>) {
        let sink = tempfile::tempfile().unwrap();
        let writer = BatchFileWriter::new(table, 1, sink, 0).unwrap();
        let merged = merge_slice(ctx, table, inputs, slice, drop_deleted, None, writer)
            .await
            .unwrap();
        assert!(merged.rows > 0);
        let (file, summary) = merged.writer.finish().unwrap();
        assert_eq!(summary.rows, merged.rows);
        let mut file = file;
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer).unwrap();
        (
            parquet_rows(bytes::Bytes::from(buffer)),
            merged
                .partials
                .into_iter()
                .map(Arc::unwrap_or_clone)
                .collect(),
        )
    }

    /// `(trace_id, span_id, start_time, ingest_time)` of every row.
    fn span_tuples(batch: &Batch) -> Vec<(Vec<u8>, Vec<u8>, i64, i64)> {
        let ids = require_col(batch, spans::TRACE_ID).unwrap();
        let span_ids = require_col(batch, spans::SPAN_ID).unwrap();
        let starts = require_col(batch, spans::START_TIME).unwrap();
        let ingests = require_col(batch, spans::INGEST_TIME).unwrap();
        (0..batch.num_rows())
            .map(|row| {
                let tid = match ids.get_scalar(row).unwrap() {
                    ScalarValue::FixedBytes16(b) => b.to_vec(),
                    other => panic!("unexpected trace_id: {other:?}"),
                };
                let sid = match span_ids.get_scalar(row).unwrap() {
                    ScalarValue::FixedBytes8(b) => b.to_vec(),
                    other => panic!("unexpected span_id: {other:?}"),
                };
                (
                    tid,
                    sid,
                    time_at(batch, starts, row).unwrap(),
                    time_at(batch, ingests, row).unwrap(),
                )
            })
            .collect()
    }

    /// Sorts `batch` by `trace_id` alone, the order partials are compared in.
    fn sorted_by_trace(batch: &Batch) -> Batch {
        let tid_col = batch.column_by_name(traces::TRACE_ID).unwrap();
        let mut keyed: Vec<([u8; 16], u32)> = (0..batch.num_rows())
            .map(|row| {
                let bytes = match tid_col.get_scalar(row).unwrap() {
                    ScalarValue::FixedBytes16(b) => b,
                    other => panic!("unexpected trace_id: {other:?}"),
                };
                (bytes, u32::try_from(row).unwrap())
            })
            .collect();
        keyed.sort_by_key(|(bytes, _)| *bytes);
        let indices: Vec<u32> = keyed.into_iter().map(|(_, idx)| idx).collect();
        batch
            .clone()
            .into_with_selection(Selection::Indices(indices))
            .unwrap()
            .compact(None)
            .unwrap()
    }

    fn assert_columns_equal(actual: &Batch, expected: &Batch) {
        assert_eq!(actual.num_rows(), expected.num_rows());
        for field in expected.schema().fields() {
            let name = field.name.as_str();
            let a_col = actual.column_by_name(name).unwrap();
            let e_col = expected.column_by_name(name).unwrap();
            for row in 0..expected.num_rows() {
                assert_eq!(
                    a_col.get_scalar(row).unwrap(),
                    e_col.get_scalar(row).unwrap(),
                    "{name} row {row}"
                );
            }
        }
    }

    fn trace_bytes(trace: u16) -> Vec<u8> {
        let mut bytes = [0_u8; 16];
        let pair = (trace + 1).to_be_bytes();
        for chunk in bytes.as_chunks_mut::<2>().0 {
            chunk.copy_from_slice(&pair);
        }
        bytes.to_vec()
    }

    fn span_bytes(span: u8) -> Vec<u8> {
        vec![span + 1; 8]
    }

    /// Three inputs: traces `A` (0) and `B` (1) have spans in all three, and
    /// `A/1` appears in input 1 with `ingest_time` 10 and in input 3 with 20.
    async fn three_span_inputs(objects: &Arc<Objects>) -> Vec<Arc<FileMeta>> {
        let one = span_rows(&[
            span_row(0, 1, 100, 10),
            span_row(0, 2, 200, 11),
            span_row(1, 1, 150, 12),
        ]);
        let two = span_rows(&[span_row(0, 3, 300, 13), span_row(1, 2, 250, 14)]);
        let three = span_rows(&[span_row(0, 1, 100, 20), span_row(1, 3, 350, 15)]);
        vec![
            write_input(objects, FileTable::Spans, one).await,
            write_input(objects, FileTable::Spans, two).await,
            write_input(objects, FileTable::Spans, three).await,
        ]
    }

    fn open_slice() -> Slice {
        Slice {
            lower: None,
            upper: None,
        }
    }

    /// `rows` spans of trace block `block`, each with `len` bytes of
    /// incompressible printable input (SplitMix64, as `big_span_batch`).
    fn big_spans(block: u16, rows: u16, len: usize) -> Batch {
        let mut state = 0x5eed_u64.wrapping_add(u64::from(block));
        let spans: Vec<_> = (0..rows)
            .map(|row| {
                let input: String = (0..len)
                    .map(|_| {
                        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
                        let mut z = state;
                        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                        z ^= z >> 31;
                        char::from(b'!' + u8::try_from(z % 94).unwrap())
                    })
                    .collect();
                let start = 1_000 + i64::from(row);
                let mut span = span_row(block * rows + row, 0, start, start);
                span.input = Some(input);
                span
            })
            .collect();
        span_rows(&spans)
    }

    fn sized(size: u64, rows: u64) -> Arc<FileMeta> {
        Arc::new(FileMeta {
            id: FileId(Uuid::now_v7()),
            table: FileTable::Spans,
            project: project(),
            day: day(),
            level: 0,
            path: String::new(),
            size,
            footer_size: 0,
            rows,
            min_time: 0,
            max_time: 0,
            min_ingest_time: 0,
            max_ingest_time: 0,
            fully_merged: false,
            deletions_applied_seq: 0,
        })
    }

    #[test]
    fn batch_rows_follow_the_pool_share_and_the_row_size() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let store: Arc<dyn object_store::ObjectStore> =
            Arc::new(object_store::memory::InMemory::new());
        let ctx = MergeContext::new(
            8192,
            3,
            cetra_column::GlobalPool::new(1 << 30),
            crate::QeRuntime::for_test(),
            Arc::clone(&store),
            Arc::new(crate::verify::VerifyQueue::default()),
            rt.handle().clone(),
        );
        // Small rows: the configured batch size.
        let small = sized(100 << 20, 1_000_000);
        assert_eq!(super::batch_rows(&ctx, &[&small, &small]), 8192);
        // The soak's day close: 17 inputs of about 1.1 KB stored per row.
        let soak = sized(1_150 << 20, 1 << 20);
        let inputs: Vec<&Arc<FileMeta>> = std::iter::repeat_n(&soak, 17).collect();
        let rows = super::batch_rows(&ctx, &inputs);
        assert!((512..=1024).contains(&rows), "{rows}");
        // Huge rows: never below the floor.
        let huge = sized(1 << 30, 10);
        assert_eq!(
            super::batch_rows(&ctx, &[&huge, &huge]),
            super::MIN_BATCH_ROWS
        );
        // A file without rows does not divide by zero.
        let empty = sized(0, 0);
        assert_eq!(super::batch_rows(&ctx, &[&empty]), 8192);
        // No finite pool: the configured batch size.
        let unbounded = MergeContext::new(
            8192,
            3,
            cetra_column::GlobalPool::new(usize::MAX),
            crate::QeRuntime::for_test(),
            store,
            Arc::new(crate::verify::VerifyQueue::default()),
            rt.handle().clone(),
        );
        assert_eq!(super::batch_rows(&unbounded, &inputs), 8192);
    }

    /// Soak finding M7-1: a day close of 17 inputs of realistic spans failed
    /// every pass with `ResourcesExhausted`, because each input's batch of
    /// 8,192 rows passed its share of the compaction pool. The batch rows now
    /// follow the inputs' bytes per row, so the same shape fits a small pool.
    #[tokio::test]
    async fn a_merge_of_large_rows_fits_the_compaction_pool() {
        let dir = tempfile::tempdir().unwrap();
        let objects = objects(&dir).await;
        let pool = cetra_column::GlobalPool::new(16 << 20);
        let ctx = MergeContext::new(
            8192,
            1,
            Arc::clone(&pool),
            crate::QeRuntime::for_test(),
            objects.store(),
            Arc::new(crate::verify::VerifyQueue::default()),
            tokio::runtime::Handle::current(),
        );
        let mut inputs = Vec::new();
        for block in 0..8 {
            inputs.push(
                write_input(&objects, FileTable::Spans, big_spans(block, 512, 8 << 10)).await,
            );
        }

        let (output, _) = merge_read(&ctx, FileTable::Spans, &inputs, open_slice(), true).await;

        assert_eq!(output.num_rows(), 8 * 512, "every span is merged once");
        assert_eq!(pool.used(), 0, "compaction pool drained cleanly");
    }

    #[tokio::test]
    async fn exceeding_compaction_max_memory_fails_with_resources_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let objects = objects(&dir).await;
        // 1 MiB pool is smaller than 8 active batches of 64 * 8 KiB (~4 MiB).
        let pool = cetra_column::GlobalPool::new(1 << 20);
        let ctx = MergeContext::new(
            8192,
            1,
            Arc::clone(&pool),
            crate::QeRuntime::for_test(),
            objects.store(),
            Arc::new(crate::verify::VerifyQueue::default()),
            tokio::runtime::Handle::current(),
        );
        let mut inputs = Vec::new();
        for block in 0..8 {
            inputs.push(
                write_input(&objects, FileTable::Spans, big_spans(block, 128, 8 << 10)).await,
            );
        }

        let sink = tempfile::tempfile().unwrap();
        let writer = BatchFileWriter::new(FileTable::Spans, 1, sink, 0).unwrap();
        let err = match merge_slice(
            &ctx,
            FileTable::Spans,
            &inputs,
            open_slice(),
            true,
            None,
            writer,
        )
        .await
        {
            Ok(_) => panic!("expected compaction.max_memory limit error"),
            Err(err) => err,
        };
        assert_eq!(err.read_source(), Some("resources_exhausted"));
        if let CompactionError::Read { ref source } = err {
            assert!(!may_be_corrupt(source));
        } else {
            panic!("expected CompactionError::Read, got {err:?}");
        }
        assert_eq!(pool.used(), 0, "compaction pool rolled back cleanly");
    }

    #[tokio::test]
    async fn spans_merge_sorted_deduped_with_partials() {
        let dir = tempfile::tempdir().unwrap();
        let objects = objects(&dir).await;
        let ctx = ctx(&objects, 8192);
        let inputs = three_span_inputs(&objects).await;

        let (output, partials) =
            merge_read(&ctx, FileTable::Spans, &inputs, open_slice(), true).await;

        let tuples = span_tuples(&output);
        assert_eq!(tuples.len(), 6, "distinct spans");
        assert!(
            tuples
                .windows(2)
                .all(|pair| { (pair[0].0.clone(), pair[0].2) <= (pair[1].0.clone(), pair[1].2) })
        );
        let a_one = tuples
            .iter()
            .find(|(trace, span, _, _)| *trace == trace_bytes(0) && *span == span_bytes(1))
            .unwrap();
        assert_eq!(a_one.3, 20, "the newer duplicate of A/1 wins");

        // The partials of all chunks equal the partials of the whole output.
        let refs: Vec<&Batch> = partials.iter().collect();
        let combined = Batch::concat(Arc::clone(partials[0].schema()), &refs, None).unwrap();
        let actual = sorted_by_trace(&combined);
        let expected_col = partials_from_spans_batch(&output).unwrap();
        let expected = sorted_by_trace(&expected_col);
        assert_columns_equal(&actual, &expected);
        assert_eq!(actual.num_rows(), 2, "one partial per trace");
    }

    #[tokio::test]
    async fn uuidv7_tie_breaking_preserves_oldest_input_first_order() {
        let dir = tempfile::tempdir().unwrap();
        let objects = objects(&dir).await;
        let ctx = ctx(&objects, 8192);

        // Same (trace_id, span_id, start_time, ingest_time) across two files,
        // distinguished by `name`. Equal ingest_time keeps the later row in
        // `(input_index ASC, row_index ASC)` order -> newer UUIDv7 wins even
        // when inputs are passed in reverse UUIDv7 order.
        let mut older_span = span_row(0, 1, 1_000, 500);
        older_span.name = "older-uuidv7".to_owned();
        let mut newer_span = span_row(0, 1, 1_000, 500);
        newer_span.name = "newer-uuidv7".to_owned();

        let mut older =
            (*write_input(&objects, FileTable::Spans, span_rows(&[older_span])).await).clone();
        let mut newer =
            (*write_input(&objects, FileTable::Spans, span_rows(&[newer_span])).await).clone();
        older.id = FileId(Uuid::from_u128(0x018f_0000_0000_7000_8000_0000_0000_0001));
        newer.id = FileId(Uuid::from_u128(0x018f_0000_0000_7000_8000_0000_0000_0002));
        let reversed_inputs = vec![Arc::new(newer), Arc::new(older)];

        let (output, _) =
            merge_read(&ctx, FileTable::Spans, &reversed_inputs, open_slice(), true).await;
        assert_eq!(output.num_rows(), 1);
        let names = require_col(&output, spans::NAME).unwrap();
        assert_eq!(
            string_at(&output, names, 0).unwrap().as_deref(),
            Some("newer-uuidv7")
        );

        // Also check distinct spans sharing `(trace_id, start_time)` across two files:
        // rows from the older file (`input_index = 0`) must precede rows from the newer
        // file (`input_index = 1`) in output order.
        let f0 = write_input(
            &objects,
            FileTable::Spans,
            span_rows(&[span_row(0, 9, 1_000, 500), span_row(0, 8, 1_000, 500)]),
        )
        .await;
        let f1 = write_input(
            &objects,
            FileTable::Spans,
            span_rows(&[span_row(0, 1, 1_000, 500)]),
        )
        .await;
        let mut f0_meta = (*f0).clone();
        let mut f1_meta = (*f1).clone();
        f0_meta.id = FileId(Uuid::from_u128(0x018f_0000_0000_7000_8000_0000_0000_0010));
        f1_meta.id = FileId(Uuid::from_u128(0x018f_0000_0000_7000_8000_0000_0000_0020));
        let (tied_out, _) = merge_read(
            &ctx,
            FileTable::Spans,
            &[Arc::new(f1_meta), Arc::new(f0_meta)],
            open_slice(),
            true,
        )
        .await;
        let span_ids: Vec<Vec<u8>> = span_tuples(&tied_out).into_iter().map(|t| t.1).collect();
        // Inside f0, `BatchFileWriter` sorted by `(trace_id, start_time)` preserving span 9 before 8
        // or stable sort; crucially BOTH spans from f0 (`input_index = 0`) precede span 1 (`input_index = 1`).
        assert_eq!(span_ids.len(), 3);
        assert_eq!(span_ids[2], span_bytes(1));
    }

    #[tokio::test]
    async fn two_slices_partition_the_same_rows() {
        let dir = tempfile::tempdir().unwrap();
        let objects = objects(&dir).await;
        let ctx = ctx(&objects, 8192);
        let inputs = three_span_inputs(&objects).await;

        let times = time_values(objects.store(), &inputs, spans::START_TIME)
            .await
            .unwrap();
        let parts = slices(&times, 2);
        assert_eq!(parts.len(), 2, "{parts:?}");
        let (first, first_partials) =
            merge_read(&ctx, FileTable::Spans, &inputs, parts[0], true).await;
        let (second, second_partials) =
            merge_read(&ctx, FileTable::Spans, &inputs, parts[1], true).await;

        let boundary = parts[0].upper.unwrap();
        let first_starts: Vec<i64> = span_tuples(&first).iter().map(|tuple| tuple.2).collect();
        let second_starts: Vec<i64> = span_tuples(&second).iter().map(|tuple| tuple.2).collect();
        assert!(first_starts.iter().all(|start| *start < boundary));
        assert!(second_starts.iter().all(|start| *start >= boundary));

        // The two outputs together equal the one-slice output.
        let mut together = span_tuples(&first);
        together.extend(span_tuples(&second));
        together.sort();
        let (whole, _) = merge_read(&ctx, FileTable::Spans, &inputs, open_slice(), true).await;
        let mut expected = span_tuples(&whole);
        expected.sort();
        assert_eq!(together, expected);

        // Trace B has spans on both sides of the boundary, so each slice's
        // partials hold one row for it.
        for partials in [&first_partials, &second_partials] {
            let refs: Vec<&Batch> = partials.iter().collect();
            let combined = Batch::concat(Arc::clone(partials[0].schema()), &refs, None).unwrap();
            let sorted = sorted_by_trace(&combined);
            let ids = sorted.column_by_name(traces::TRACE_ID).unwrap();
            let expected = trace_bytes(1);
            assert_eq!(
                (0..sorted.num_rows())
                    .filter(|row| matches!(
                        ids.get_scalar(*row).unwrap(),
                        ScalarValue::FixedBytes16(b) if b == expected.as_slice()
                    ))
                    .count(),
                1
            );
        }
    }

    #[tokio::test]
    async fn slice_upper_i64_min_short_circuits_empty() {
        let dir = tempfile::tempdir().unwrap();
        let objects = objects(&dir).await;
        let ctx = ctx(&objects, 8192);
        let inputs = three_span_inputs(&objects).await;
        let sink = tempfile::tempfile().unwrap();
        let writer = BatchFileWriter::new(FileTable::Spans, 1, sink, 0).unwrap();
        let merged = merge_slice(
            &ctx,
            FileTable::Spans,
            &inputs,
            Slice {
                lower: None,
                upper: Some(i64::MIN),
            },
            true,
            None,
            writer,
        )
        .await
        .unwrap();
        assert_eq!(merged.rows, 0);
        assert!(merged.partials.is_empty());
    }

    #[tokio::test]
    async fn a_trace_straddling_batches_keeps_one_partial() {
        let dir = tempfile::tempdir().unwrap();
        let objects = objects(&dir).await;
        let ctx = ctx(&objects, 3);
        let spans: Vec<_> = (0..7)
            .map(|index| {
                span_row(
                    0,
                    index,
                    1000 + i64::from(index) * 1000,
                    1200 + i64::from(index) * 1000,
                )
            })
            .collect();
        let one = span_rows(&spans);
        let two = span_rows(&[span_row(0, 3, 4000, 5000)]);
        let inputs = vec![
            write_input(&objects, FileTable::Spans, one).await,
            write_input(&objects, FileTable::Spans, two).await,
        ];

        let (output, partials) =
            merge_read(&ctx, FileTable::Spans, &inputs, open_slice(), true).await;

        let tuples = span_tuples(&output);
        assert_eq!(tuples.len(), 7);
        let duplicate = tuples
            .iter()
            .find(|(_, span, _, _)| *span == span_bytes(3))
            .unwrap();
        assert_eq!(duplicate.3, 5000, "the newer duplicate wins");

        assert_eq!(partials.len(), 1, "one partial batch for one group");
        let partial = &partials[0];
        assert_eq!(partial.num_rows(), 1);
        let counts_col = require_col(partial, traces::SPAN_COUNT).unwrap();
        assert_eq!(i64_at(partial, counts_col, 0).unwrap(), 7);
    }

    #[tokio::test]
    async fn a_group_boundary_splits_prefix_from_straddling_tail() {
        let dir = tempfile::tempdir().unwrap();
        let objects = objects(&dir).await;
        // Batch size 3: chunk 1 has 2 rows of trace 0 + 1 row of trace 1 (split = 2 > 0),
        // chunk 2 holds 4 rows of trace 1 (split = 0), chunk 3 finishes trace 1.
        let ctx = ctx(&objects, 3);
        let mut rows = vec![span_row(0, 0, 1000, 1100), span_row(0, 1, 2000, 2100)];
        for index in 0..5 {
            rows.push(span_row(
                1,
                index,
                3000 + i64::from(index) * 1000,
                3200 + i64::from(index) * 1000,
            ));
        }
        let one = span_rows(&rows[..4]);
        let two = span_rows(&rows[4..]);
        let inputs = vec![
            write_input(&objects, FileTable::Spans, one).await,
            write_input(&objects, FileTable::Spans, two).await,
        ];

        let (output, partials) =
            merge_read(&ctx, FileTable::Spans, &inputs, open_slice(), true).await;

        assert_eq!(span_tuples(&output).len(), 7);
        assert_eq!(
            partials.len(),
            2,
            "two partial batches: trace 0 then trace 1"
        );
        let counts: Vec<i64> = partials
            .iter()
            .map(|p| {
                assert_eq!(p.num_rows(), 1);
                let col = require_col(p, traces::SPAN_COUNT).unwrap();
                i64_at(p, col, 0).unwrap()
            })
            .collect();
        assert_eq!(counts, vec![2, 5]);
    }

    fn score_version(
        score_id: Uuid,
        created_at: i64,
        updated_at: i64,
        ingest: i64,
        deleted: bool,
        name: &str,
    ) -> Score {
        Score {
            project_id: project(),
            score_id,
            target: ScoreTarget::Trace {
                trace_id: "4bf92f3577b34da6a3ce929d0e0e4736".parse().unwrap(),
            },
            session_id: None,
            user_id: None,
            name: name.to_owned(),
            value: ScoreValue::Numeric(0.5),
            reason: None,
            source: ScoreSource::Api,
            evaluator_id: None,
            rule_id: None,
            experiment_id: None,
            dataset_item_id: None,
            queue_id: None,
            author_user_id: None,
            metadata: Default::default(),
            created_at,
            updated_at,
            deleted,
            ingest_time: ingest,
        }
    }

    /// Score `s` has versions in three inputs (`updated_at` 1, 3 and 2, the
    /// one at 3 deleted); score `t` has one live version.
    async fn three_score_inputs(objects: &Arc<Objects>) -> Vec<Arc<FileMeta>> {
        let s = Uuid::from_u128(0x11);
        let t = Uuid::from_u128(0x22);
        let one = scores_to_batch(&[
            score_version(s, 1000, 1, 100, false, "s-v1"),
            score_version(t, 2000, 5, 500, false, "t"),
        ])
        .unwrap();
        let two = scores_to_batch(&[score_version(s, 1000, 3, 300, true, "s-v3")]).unwrap();
        let three = scores_to_batch(&[score_version(s, 1000, 2, 200, false, "s-v2")]).unwrap();
        vec![
            write_input(objects, FileTable::Scores, one).await,
            write_input(objects, FileTable::Scores, two).await,
            write_input(objects, FileTable::Scores, three).await,
        ]
    }

    fn score_tuples(batch: &Batch) -> Vec<(String, i64, bool, String)> {
        let ids = require_col(batch, scores::SCORE_ID).unwrap();
        let updated = require_col(batch, scores::UPDATED_AT).unwrap();
        let deleted = require_col(batch, scores::DELETED).unwrap();
        let names = require_col(batch, scores::NAME).unwrap();
        (0..batch.num_rows())
            .map(|row| {
                (
                    string_at(batch, ids, row).unwrap().unwrap(),
                    time_at(batch, updated, row).unwrap(),
                    bool_at(batch, deleted, row).unwrap().unwrap(),
                    string_at(batch, names, row).unwrap().unwrap(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn scores_merge_last_writer_wins() {
        let dir = tempfile::tempdir().unwrap();
        let objects = objects(&dir).await;
        let ctx = ctx(&objects, 8192);
        let inputs = three_score_inputs(&objects).await;
        let s = Uuid::from_u128(0x11).to_string();
        let t = Uuid::from_u128(0x22).to_string();

        let (kept, _) = merge_read(&ctx, FileTable::Scores, &inputs, open_slice(), false).await;
        let kept = score_tuples(&kept);
        assert_eq!(kept.len(), 2, "{kept:?}");
        let s_row = kept.iter().find(|(id, _, _, _)| *id == s).unwrap();
        assert_eq!((s_row.1, s_row.2, s_row.3.as_str()), (3, true, "s-v3"));

        let (dropped, _) = merge_read(&ctx, FileTable::Scores, &inputs, open_slice(), true).await;
        let dropped = score_tuples(&dropped);
        assert_eq!(dropped.len(), 1, "{dropped:?}");
        assert_eq!(dropped[0].0, t);
    }
}
