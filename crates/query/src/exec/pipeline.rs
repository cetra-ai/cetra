//! Push-based pipeline traits, execution context, metrics, and parallel runner.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use cetra_column::{Batch, CancelToken, ColumnError, Reservation, ScopedPool};

use crate::error::QueryError;
use crate::ir::EvalContext;

use super::pool::WorkerPool;
use super::source::PartitionStream;
use super::spill::SpillManager;

/// Default target rows per batch across operators (`views/session.rs:41`).
pub const DEFAULT_BATCH_SIZE: usize = 8192;

/// Flow-control signal returned by push operators and sinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlFlow {
    /// Continue pushing more batches.
    Continue,
    /// Downstream is satisfied (e.g. `LIMIT` reached); stop reading this partition immediately.
    Stop,
}

/// Per-operator execution profile (powers `trace_bench` missed-target diagnostics in M4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OperatorProfile {
    /// Operator display name.
    pub name: String,
    /// Rows emitted.
    pub rows_out: u64,
    /// Batches emitted.
    pub batches_out: u64,
    /// Wall time spent inside this operator (nanoseconds).
    pub elapsed_ns: u64,
}

/// Thread-safe accumulator collecting [`OperatorProfile`] snapshots across workers.
#[derive(Debug, Default, Clone)]
pub struct ExecMetrics {
    profiles: Arc<Mutex<Vec<OperatorProfile>>>,
}

impl ExecMetrics {
    /// Creates an empty metrics accumulator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one operator profile, aggregating counts and elapsed time by operator `name`.
    pub fn record(&self, profile: OperatorProfile) {
        let mut guard = self.profiles.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = guard.iter_mut().find(|p| p.name == profile.name) {
            existing.rows_out = existing.rows_out.saturating_add(profile.rows_out);
            existing.batches_out = existing.batches_out.saturating_add(profile.batches_out);
            existing.elapsed_ns = existing.elapsed_ns.saturating_add(profile.elapsed_ns);
        } else {
            guard.push(profile);
        }
    }

    /// Returns a snapshot of all recorded operator profiles.
    pub fn snapshot(&self) -> Vec<OperatorProfile> {
        self.profiles
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Shared per-query execution context passed to every pipeline and operator.
#[derive(Debug, Clone)]
pub struct ExecContext {
    /// Query-scoped memory pool (`FairSpill` + query cap).
    pub pool: Arc<ScopedPool>,
    /// Evaluation context (`now_ns`, `CancelToken`).
    pub eval: EvalContext,
    /// Spill manager for out-of-core buffering operators.
    pub spill: Arc<SpillManager>,
    /// Bounded process-wide CPU worker pool.
    pub workers: Arc<WorkerPool>,
    /// Target rows per batch (`DEFAULT_BATCH_SIZE`).
    pub batch_size: usize,
    /// Maximum parallel partitions for this query (`storage.query.target_partitions`).
    pub target_partitions: usize,
    /// Per-operator profile recorder.
    pub metrics: ExecMetrics,
}

/// Moves `bytes` from `src` into `dst` without double-counting; if `src.size() < bytes`:
/// - when `src.can_spill()`, grows `dst` by the shortfall first then transfers `src.size()`
///   (rolling back via infallible `dst.shrink` on error);
/// - otherwise transfers `src.size()` first and grows `dst` by the remainder (rolling back on error).
pub(crate) fn transfer_or_grow(
    src: &mut Reservation,
    dst: &mut Reservation,
    bytes: usize,
) -> Result<(), ColumnError> {
    if bytes == 0 {
        return Ok(());
    }
    let avail = src.size();
    if avail >= bytes {
        src.transfer_to(dst, bytes)
    } else if src.can_spill() {
        let extra = bytes - avail;
        dst.try_grow(extra)?;
        if avail > 0
            && let Err(err) = src.transfer_to(dst, avail)
        {
            dst.shrink(extra);
            return Err(err);
        }
        Ok(())
    } else {
        if avail > 0 {
            src.transfer_to(dst, avail)?;
        }
        if let Err(err) = dst.try_grow(bytes - avail) {
            if avail > 0 {
                let _ = dst.transfer_to(src, avail);
            }
            return Err(err);
        }
        Ok(())
    }
}

/// Push-based stateless/stateful pipeline operator transforming one `Batch` into zero or more `Batch`es.
pub trait PushOperator: Send {
    /// Operator name for `EXPLAIN` and [`ExecMetrics`].
    fn name(&self) -> &'static str;
    /// Processes `batch` (whose memory reservation is held in `batch_res`) and forwards zero or
    /// more `(out_batch, out_res)` pairs to `emit`.
    /// Scratch `Reservation`s held for an output batch MUST be freed right after `emit` returns.
    fn push(
        &mut self,
        batch: Batch,
        batch_res: &mut Reservation,
        emit: &mut dyn FnMut(Batch, &mut Reservation) -> Result<ControlFlow, QueryError>,
    ) -> Result<ControlFlow, QueryError>;
    /// Called once when upstream finishes cleanly (not cancelled/stopped) to flush buffered output.
    fn finish(
        &mut self,
        emit: &mut dyn FnMut(Batch, &mut Reservation) -> Result<ControlFlow, QueryError>,
    ) -> Result<(), QueryError> {
        let _ = emit;
        Ok(())
    }
}

/// Terminal consumer of one pipeline partition.
pub trait PushSink: Send {
    /// Consumes one batch (whose memory reservation is held in `batch_res`); returning
    /// `ControlFlow::Stop` terminates the partition early.
    fn consume(
        &mut self,
        batch: Batch,
        batch_res: &mut Reservation,
    ) -> Result<ControlFlow, QueryError>;
    /// Called once when the partition finishes cleanly.
    fn finish(&mut self) -> Result<(), QueryError> {
        Ok(())
    }
}

/// Runs `partitions` in parallel on `ctx.workers`, pushing each partition through a fresh
/// operator chain built by `make_chain(partition_idx)` into `make_sink(partition_idx)`.
/// Frees each partition's `scan_res` immediately after pushing each batch (and on error/stop),
/// and synchronously drains all workers before returning finalized sinks in partition index order.
pub fn run_pipeline<S, FChain, FSink>(
    ctx: &ExecContext,
    partitions: Vec<Box<dyn PartitionStream>>,
    make_chain: FChain,
    make_sink: FSink,
) -> Result<Vec<S>, QueryError>
where
    S: PushSink + Send + 'static,
    FChain: Fn(usize) -> Result<Vec<Box<dyn PushOperator>>, QueryError> + Send + Sync + 'static,
    FSink: Fn(usize) -> Result<S, QueryError> + Send + Sync + 'static,
{
    run_pipeline_with_stop(ctx, None, partitions, make_chain, make_sink)
}

/// Like [`run_pipeline`], wiring an optional pipeline-local `stop_cancel` token linked to
/// `ctx.eval.cancel` so `LimitOp` early-stop can halt sibling partitions without returning
/// `Err(QueryError::Cancelled)`.
pub fn run_pipeline_with_stop<S, FChain, FSink>(
    ctx: &ExecContext,
    stop_cancel: Option<CancelToken>,
    partitions: Vec<Box<dyn PartitionStream>>,
    make_chain: FChain,
    make_sink: FSink,
) -> Result<Vec<S>, QueryError>
where
    S: PushSink + Send + 'static,
    FChain: Fn(usize) -> Result<Vec<Box<dyn PushOperator>>, QueryError> + Send + Sync + 'static,
    FSink: Fn(usize) -> Result<S, QueryError> + Send + Sync + 'static,
{
    if partitions.is_empty() {
        return Ok(Vec::new());
    }

    let stop_cancel = stop_cancel.unwrap_or_default();
    let _cancel_link = ctx.eval.cancel.register_waker({
        let sc = stop_cancel.clone();
        move || sc.cancel()
    });

    let make_chain = Arc::new(make_chain);
    let make_sink = Arc::new(make_sink);
    let tasks: Vec<_> = partitions
        .into_iter()
        .enumerate()
        .map(|(partition_idx, mut stream)| {
            let make_chain = Arc::clone(&make_chain);
            let make_sink = Arc::clone(&make_sink);
            let pool = Arc::clone(&ctx.pool);
            let parent_cancel = ctx.eval.cancel.clone();
            let stop_cancel = stop_cancel.clone();
            let metrics = ctx.metrics.clone();

            move |_worker_task_idx: usize| -> Result<S, QueryError> {
                let mut chain = make_chain(partition_idx)?;
                let mut sink = make_sink(partition_idx)?;
                let mut profiles: Vec<OperatorProfile> = chain
                    .iter()
                    .map(|op| OperatorProfile {
                        name: op.name().to_owned(),
                        rows_out: 0,
                        batches_out: 0,
                        elapsed_ns: 0,
                    })
                    .collect();

                let mut scan_res = pool.register("scan", false);
                let mut stopped = false;

                while !stopped {
                    if parent_cancel.is_cancelled() {
                        scan_res.free();
                        return Err(QueryError::Cancelled);
                    }
                    if stop_cancel.is_cancelled() {
                        scan_res.free();
                        stopped = true;
                        break;
                    }
                    let next = stream.next_batch(&stop_cancel, Some(&mut scan_res));
                    let batch = match next {
                        Ok(Some(b)) => b,
                        Ok(None) => {
                            scan_res.free();
                            break;
                        }
                        Err(QueryError::Cancelled)
                            if !parent_cancel.is_cancelled() && stop_cancel.is_cancelled() =>
                        {
                            scan_res.free();
                            stopped = true;
                            break;
                        }
                        Err(err) => {
                            scan_res.free();
                            return Err(err);
                        }
                    };

                    let push_res = push_through_slice(
                        &mut chain,
                        &mut profiles,
                        batch,
                        &mut scan_res,
                        &mut sink,
                    );
                    scan_res.free();
                    match push_res {
                        Ok(ControlFlow::Continue) => {}
                        Ok(ControlFlow::Stop) => {
                            stopped = true;
                        }
                        Err(QueryError::Cancelled)
                            if !parent_cancel.is_cancelled() && stop_cancel.is_cancelled() =>
                        {
                            stopped = true;
                        }
                        Err(err) => return Err(err),
                    }
                }

                if parent_cancel.is_cancelled() {
                    return Err(QueryError::Cancelled);
                }

                if !stopped {
                    for i in 0..chain.len() {
                        if parent_cancel.is_cancelled() {
                            return Err(QueryError::Cancelled);
                        }
                        if stop_cancel.is_cancelled() {
                            break;
                        }
                        let mut downstream_flow = ControlFlow::Continue;
                        let mut emit_ns: u64 = 0;
                        let (head, tail) = chain.split_at_mut(i + 1);
                        let (prof_head, prof_tail) = profiles.split_at_mut(i + 1);
                        let op = &mut head[i];
                        let op_prof = &mut prof_head[i];

                        let t0 = Instant::now();
                        let finish_res = op.finish(&mut |out_batch, out_res| {
                            op_prof.rows_out =
                                op_prof.rows_out.saturating_add(out_batch.num_rows() as u64);
                            op_prof.batches_out = op_prof.batches_out.saturating_add(1);
                            let te = Instant::now();
                            let f =
                                push_through_slice(tail, prof_tail, out_batch, out_res, &mut sink);
                            emit_ns = emit_ns.saturating_add(
                                te.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                            );
                            match f {
                                Ok(ControlFlow::Continue) => Ok(ControlFlow::Continue),
                                Ok(ControlFlow::Stop) => {
                                    downstream_flow = ControlFlow::Stop;
                                    Ok(ControlFlow::Stop)
                                }
                                Err(QueryError::Cancelled)
                                    if !parent_cancel.is_cancelled()
                                        && stop_cancel.is_cancelled() =>
                                {
                                    downstream_flow = ControlFlow::Stop;
                                    Ok(ControlFlow::Stop)
                                }
                                Err(err) => Err(err),
                            }
                        });
                        let total_ns = t0.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                        op_prof.elapsed_ns = op_prof
                            .elapsed_ns
                            .saturating_add(total_ns.saturating_sub(emit_ns));

                        match finish_res {
                            Ok(()) => {}
                            Err(QueryError::Cancelled)
                                if !parent_cancel.is_cancelled() && stop_cancel.is_cancelled() =>
                            {
                                break;
                            }
                            Err(err) => return Err(err),
                        }

                        if downstream_flow == ControlFlow::Stop {
                            break;
                        }
                    }
                }

                if parent_cancel.is_cancelled() {
                    return Err(QueryError::Cancelled);
                }
                match sink.finish() {
                    Ok(()) => {}
                    Err(QueryError::Cancelled)
                        if !parent_cancel.is_cancelled() && stop_cancel.is_cancelled() => {}
                    Err(err) => return Err(err),
                }
                for prof in profiles {
                    metrics.record(prof);
                }
                Ok(sink)
            }
        })
        .collect();

    ctx.workers.run_scoped(&ctx.eval.cancel, tasks)
}

fn push_through_slice<S: PushSink>(
    chain: &mut [Box<dyn PushOperator>],
    profiles: &mut [OperatorProfile],
    batch: Batch,
    batch_res: &mut Reservation,
    sink: &mut S,
) -> Result<ControlFlow, QueryError> {
    let Some((op, rest)) = chain.split_first_mut() else {
        return sink.consume(batch, batch_res);
    };
    let Some((prof, rest_prof)) = profiles.split_first_mut() else {
        return Err(QueryError::internal("operator profile count mismatch"));
    };

    let mut downstream_flow = ControlFlow::Continue;
    let mut emit_ns: u64 = 0;
    let t0 = Instant::now();
    let op_flow = op.push(batch, batch_res, &mut |out_batch, out_res| {
        prof.rows_out = prof.rows_out.saturating_add(out_batch.num_rows() as u64);
        prof.batches_out = prof.batches_out.saturating_add(1);
        let te = Instant::now();
        let f = push_through_slice(rest, rest_prof, out_batch, out_res, sink);
        emit_ns = emit_ns.saturating_add(te.elapsed().as_nanos().min(u64::MAX as u128) as u64);
        let f = f?;
        if f == ControlFlow::Stop {
            downstream_flow = ControlFlow::Stop;
        }
        Ok(f)
    })?;
    let total_ns = t0.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    prof.elapsed_ns = prof
        .elapsed_ns
        .saturating_add(total_ns.saturating_sub(emit_ns));

    if op_flow == ControlFlow::Stop || downstream_flow == ControlFlow::Stop {
        Ok(ControlFlow::Stop)
    } else {
        Ok(ControlFlow::Continue)
    }
}

/// Collects all batches emitted by one pipeline partition (tracked in `Reservation`).
#[derive(Debug)]
pub struct CollectSink {
    batches: Vec<Batch>,
    res: Reservation,
}

impl CollectSink {
    /// Creates an empty [`CollectSink`] backed by an unspillable reservation on `pool`.
    pub fn new(pool: &Arc<ScopedPool>) -> Self {
        Self {
            batches: Vec::new(),
            res: pool.register("collect", false),
        }
    }

    /// Collected batches in emission order.
    pub fn batches(&self) -> &[Batch] {
        &self.batches
    }

    /// Bytes currently reserved for collected batches.
    pub fn reserved_bytes(&self) -> usize {
        self.res.size()
    }

    /// Consumes the sink, returning collected batches and releasing its reservation.
    pub fn into_batches(self) -> Vec<Batch> {
        self.batches
    }
}

impl PushSink for CollectSink {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{BatchSource, ScanSource, ScanSpec};
    use cetra_column::{
        CancelToken, Column, DataType, Field, FlatColumn, FlatData, GlobalPool, Schema,
    };

    struct StopAfterOneOp {
        seen: bool,
    }

    impl PushOperator for StopAfterOneOp {
        fn name(&self) -> &'static str {
            "StopAfterOne"
        }

        fn push(
            &mut self,
            batch: Batch,
            batch_res: &mut Reservation,
            emit: &mut dyn FnMut(Batch, &mut Reservation) -> Result<ControlFlow, QueryError>,
        ) -> Result<ControlFlow, QueryError> {
            self.seen = true;
            let _ = emit(batch, batch_res)?;
            Ok(ControlFlow::Stop)
        }

        fn finish(
            &mut self,
            _emit: &mut dyn FnMut(Batch, &mut Reservation) -> Result<ControlFlow, QueryError>,
        ) -> Result<(), QueryError> {
            panic!("finish must not be called after ControlFlow::Stop");
        }
    }

    struct PassThroughAndFlushOp {
        flushed: Option<Batch>,
        scratch: Reservation,
    }

    impl PushOperator for PassThroughAndFlushOp {
        fn name(&self) -> &'static str {
            "PassThroughAndFlush"
        }

        fn push(
            &mut self,
            batch: Batch,
            batch_res: &mut Reservation,
            emit: &mut dyn FnMut(Batch, &mut Reservation) -> Result<ControlFlow, QueryError>,
        ) -> Result<ControlFlow, QueryError> {
            emit(batch, batch_res)
        }

        fn finish(
            &mut self,
            emit: &mut dyn FnMut(Batch, &mut Reservation) -> Result<ControlFlow, QueryError>,
        ) -> Result<(), QueryError> {
            if let Some(b) = self.flushed.take() {
                self.scratch.try_grow(b.memory_bytes())?;
                let res = emit(b, &mut self.scratch);
                self.scratch.free();
                let _ = res?;
            }
            Ok(())
        }
    }

    struct CheckingSink {
        pool: Arc<ScopedPool>,
        inner: CollectSink,
        batch_bytes_seen: Vec<usize>,
        finished: bool,
    }

    impl PushSink for CheckingSink {
        fn consume(
            &mut self,
            batch: Batch,
            batch_res: &mut Reservation,
        ) -> Result<ControlFlow, QueryError> {
            // `scan_res` holds `batch.memory_bytes()`, and `CollectSink` transfers from it
            // without double-counting, so `pool.reserved()` never spikes by an extra copy.
            let expected_before = batch.memory_bytes() + self.inner.reserved_bytes();
            assert!(
                self.pool.reserved() >= expected_before,
                "expected reserved >= {expected_before}, got {}",
                self.pool.reserved()
            );
            let bytes = batch.memory_bytes();
            self.batch_bytes_seen.push(bytes);
            let flow = self.inner.consume(batch, batch_res)?;
            assert_eq!(self.pool.reserved(), self.inner.reserved_bytes());
            Ok(flow)
        }

        fn finish(&mut self) -> Result<(), QueryError> {
            self.finished = true;
            Ok(())
        }
    }

    fn make_int_batch(schema: &Arc<Schema>, vals: Vec<i64>) -> Batch {
        let len = vals.len();
        Batch::try_new_all(
            Arc::clone(schema),
            vec![Column::Flat(
                FlatColumn::try_new(FlatData::I64(vals), None).unwrap(),
            )],
            len,
        )
        .unwrap()
    }

    #[test]
    fn early_stop_reservation_handoff_and_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let pool = GlobalPool::standalone(1024 * 1024);
        let ctx = ExecContext {
            pool: Arc::clone(&pool),
            eval: EvalContext::new(0, CancelToken::new()),
            spill: Arc::new(SpillManager::with_fixed_cap(
                dir.path().to_path_buf(),
                1024 * 1024,
            )),
            workers: Arc::new(WorkerPool::new(2).unwrap()),
            batch_size: DEFAULT_BATCH_SIZE,
            target_partitions: 1,
            metrics: ExecMetrics::new(),
        };

        let schema = Schema::try_new_arc(vec![Field::new("v", DataType::I64, false)]).unwrap();
        let b1 = make_int_batch(&schema, vec![1, 2, 3]);
        let b2 = make_int_batch(&schema, vec![4, 5, 6]);
        let b3 = make_int_batch(&schema, vec![7, 8, 9]);
        let source = BatchSource::new(Arc::clone(&schema), vec![b1.clone(), b2, b3]).unwrap();

        // 1. Early stop: StopAfterOneOp stops after b1; finish() is NOT called on StopAfterOneOp;
        //    scan_res is freed immediately after push; sink.finish() IS called.
        let partitions = source.open_partitions(&ScanSpec::default(), &ctx).unwrap();
        let pool_for_sink = Arc::clone(&pool);
        let sinks = run_pipeline(
            &ctx,
            partitions,
            |_| Ok(vec![Box::new(StopAfterOneOp { seen: false })]),
            move |_| {
                Ok(CheckingSink {
                    pool: Arc::clone(&pool_for_sink),
                    inner: CollectSink::new(&pool_for_sink),
                    batch_bytes_seen: Vec::new(),
                    finished: false,
                })
            },
        )
        .unwrap();
        assert_eq!(sinks.len(), 1);
        assert!(sinks[0].finished);
        assert_eq!(sinks[0].inner.batches().len(), 1);
        assert!(sinks[0].inner.batches()[0].semantic_eq(&b1));
        // scan_res was freed, so pool.reserved() equals only CollectSink's reservation
        assert_eq!(pool.reserved(), sinks[0].inner.reserved_bytes());
        drop(sinks);
        assert_eq!(pool.reserved(), 0);

        let snap = ctx.metrics.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].name, "StopAfterOne");
        assert_eq!(snap[0].rows_out, 3);
        assert_eq!(snap[0].batches_out, 1);

        // 2. Clean completion calls finish() and flushes trailing batch, recording metrics across partitions
        let ctx2 = ExecContext {
            target_partitions: 2,
            metrics: ExecMetrics::new(),
            ..ctx.clone()
        };
        let extra = make_int_batch(&schema, vec![99]);
        let parts2 = source.open_partitions(&ScanSpec::default(), &ctx2).unwrap();
        assert_eq!(parts2.len(), 2);
        let pool2 = Arc::clone(&pool);
        let pool2_op = Arc::clone(&pool);
        let sinks2 = run_pipeline(
            &ctx2,
            parts2,
            move |_| {
                Ok(vec![Box::new(PassThroughAndFlushOp {
                    flushed: Some(extra.clone()),
                    scratch: pool2_op.register("flush_test", false),
                })])
            },
            move |_| Ok(CollectSink::new(&pool2)),
        )
        .unwrap();
        let total_rows: usize = sinks2
            .iter()
            .flat_map(|s| s.batches())
            .map(Batch::num_rows)
            .sum();
        assert_eq!(total_rows, 9 + 2);
        drop(sinks2);
        assert_eq!(pool.reserved(), 0);

        let snap2 = ctx2.metrics.snapshot();
        assert_eq!(snap2.len(), 1);
        assert_eq!(snap2[0].name, "PassThroughAndFlush");
        assert_eq!(snap2[0].rows_out, 11);
        assert_eq!(snap2[0].batches_out, 5);
    }

    #[test]
    fn run_pipeline_scan_res_transfer_stays_within_single_batch_cap() {
        let dir = tempfile::tempdir().unwrap();
        let schema = Schema::try_new_arc(vec![Field::new("v", DataType::I64, false)]).unwrap();
        let vals: Vec<i64> = (0..4096).collect();
        let batch = make_int_batch(&schema, vals);
        let batch_bytes = batch.memory_bytes();
        // Cap is strictly smaller than 2x batch_bytes, proving zero-double-count transfer.
        let tight_cap = batch_bytes + 256;
        assert!(tight_cap < batch_bytes * 2);

        let pool = GlobalPool::standalone(tight_cap);
        let ctx = ExecContext {
            pool: Arc::clone(&pool),
            eval: EvalContext::new(0, CancelToken::new()),
            spill: Arc::new(SpillManager::with_fixed_cap(
                dir.path().to_path_buf(),
                1024 * 1024,
            )),
            workers: Arc::new(WorkerPool::new(1).unwrap()),
            batch_size: DEFAULT_BATCH_SIZE,
            target_partitions: 1,
            metrics: ExecMetrics::new(),
        };

        let source = BatchSource::new(Arc::clone(&schema), vec![batch.clone()]).unwrap();
        let partitions = source.open_partitions(&ScanSpec::default(), &ctx).unwrap();

        #[derive(Debug)]
        struct PeakCheckSink {
            pool: Arc<ScopedPool>,
            inner: CollectSink,
            peak_reserved: usize,
        }

        impl PushSink for PeakCheckSink {
            fn consume(
                &mut self,
                batch: Batch,
                batch_res: &mut Reservation,
            ) -> Result<ControlFlow, QueryError> {
                self.peak_reserved = self.peak_reserved.max(self.pool.reserved());
                let flow = self.inner.consume(batch, batch_res)?;
                self.peak_reserved = self.peak_reserved.max(self.pool.reserved());
                Ok(flow)
            }
        }

        let p_sink = Arc::clone(&pool);
        let sinks = run_pipeline(
            &ctx,
            partitions,
            |_| Ok(Vec::new()),
            move |_| {
                Ok(PeakCheckSink {
                    pool: Arc::clone(&p_sink),
                    inner: CollectSink::new(&p_sink),
                    peak_reserved: 0,
                })
            },
        )
        .unwrap();

        assert_eq!(sinks.len(), 1);
        assert!(
            sinks[0].peak_reserved <= batch_bytes,
            "peak_reserved={} exceeded 1x batch_bytes={batch_bytes}",
            sinks[0].peak_reserved
        );
        assert_eq!(sinks[0].inner.batches().len(), 1);
        assert!(sinks[0].inner.batches()[0].semantic_eq(&batch));
        drop(sinks);
        assert_eq!(pool.reserved(), 0);
    }

    #[test]
    fn transfer_or_grow_spillable_src_grows_before_transfer_and_rolls_back_cleanly() {
        let pool = GlobalPool::standalone(100);
        let mut src = pool.register("spillable_src", true);
        let mut dst = pool.register("unspillable_dst", false);
        src.try_grow(40).unwrap();
        assert_eq!(src.size(), 40);
        assert_eq!(dst.size(), 0);

        // Request 120 bytes (> pool cap 100): dst.try_grow(80) fails BEFORE src.transfer_to(dst, 40),
        // leaving both reservations untouched.
        let err = transfer_or_grow(&mut src, &mut dst, 120).unwrap_err();
        assert!(matches!(err, ColumnError::MemoryLimit { .. }));
        assert_eq!(src.size(), 40);
        assert_eq!(dst.size(), 0);
        assert_eq!(pool.reserved(), 40);

        // Request 90 bytes (<= pool cap 100): dst grows by 50, then receives 40 from src.
        transfer_or_grow(&mut src, &mut dst, 90).unwrap();
        assert_eq!(src.size(), 0);
        assert_eq!(dst.size(), 90);
        assert_eq!(pool.reserved(), 90);
        drop(src);
        drop(dst);
        assert_eq!(pool.reserved(), 0);
    }

    #[test]
    fn run_pipeline_with_stop_absorbs_stop_cancel_during_finish_phases() {
        let dir = tempfile::tempdir().unwrap();
        let pool = GlobalPool::standalone(1024 * 1024);
        let ctx = ExecContext {
            pool: Arc::clone(&pool),
            eval: EvalContext::new(0, CancelToken::new()),
            spill: Arc::new(SpillManager::with_fixed_cap(
                dir.path().to_path_buf(),
                1024 * 1024,
            )),
            workers: Arc::new(WorkerPool::new(1).unwrap()),
            batch_size: DEFAULT_BATCH_SIZE,
            target_partitions: 1,
            metrics: ExecMetrics::new(),
        };
        let schema = Schema::try_new_arc(vec![Field::new("v", DataType::I64, false)]).unwrap();
        let b1 = make_int_batch(&schema, vec![1, 2]);

        struct CancelDuringOpFinish {
            stop_cancel: CancelToken,
            emit_batch: Option<Batch>,
            return_cancelled: bool,
            scratch: Reservation,
        }

        impl PushOperator for CancelDuringOpFinish {
            fn name(&self) -> &'static str {
                "CancelDuringOpFinish"
            }

            fn push(
                &mut self,
                batch: Batch,
                batch_res: &mut Reservation,
                emit: &mut dyn FnMut(Batch, &mut Reservation) -> Result<ControlFlow, QueryError>,
            ) -> Result<ControlFlow, QueryError> {
                emit(batch, batch_res)
            }

            fn finish(
                &mut self,
                emit: &mut dyn FnMut(Batch, &mut Reservation) -> Result<ControlFlow, QueryError>,
            ) -> Result<(), QueryError> {
                if let Some(b) = self.emit_batch.take() {
                    self.scratch.try_grow(b.memory_bytes())?;
                    let res = emit(b, &mut self.scratch);
                    self.scratch.free();
                    let _ = res?;
                }
                if self.return_cancelled {
                    self.stop_cancel.cancel();
                    return Err(QueryError::Cancelled);
                }
                Ok(())
            }
        }

        struct CancelInSinkFinish {
            inner: CollectSink,
            stop_cancel: CancelToken,
            cancel_on_consume: bool,
            cancel_on_finish: bool,
            seen_batches: usize,
        }

        impl PushSink for CancelInSinkFinish {
            fn consume(
                &mut self,
                batch: Batch,
                batch_res: &mut Reservation,
            ) -> Result<ControlFlow, QueryError> {
                self.seen_batches += 1;
                if self.cancel_on_consume && self.seen_batches >= 2 {
                    self.stop_cancel.cancel();
                    return Err(QueryError::Cancelled);
                }
                self.inner.consume(batch, batch_res)
            }

            fn finish(&mut self) -> Result<(), QueryError> {
                if self.cancel_on_finish {
                    self.stop_cancel.cancel();
                    return Err(QueryError::Cancelled);
                }
                Ok(())
            }
        }

        // Case 1: stop_cancel fires inside op.finish emit closure and sink.finish
        let stop1 = CancelToken::new();
        let src1 = BatchSource::new(Arc::clone(&schema), vec![b1.clone()]).unwrap();
        let parts1 = src1.open_partitions(&ScanSpec::default(), &ctx).unwrap();
        let stop1_op = stop1.clone();
        let stop1_sink = stop1.clone();
        let b_flush = make_int_batch(&schema, vec![3, 4]);
        let pool_op = Arc::clone(&pool);
        let pool_sink = Arc::clone(&pool);
        let sinks1 = run_pipeline_with_stop(
            &ctx,
            Some(stop1),
            parts1,
            move |_| {
                Ok(vec![Box::new(CancelDuringOpFinish {
                    stop_cancel: stop1_op.clone(),
                    emit_batch: Some(b_flush.clone()),
                    return_cancelled: false,
                    scratch: pool_op.register("test_op", false),
                }) as Box<dyn PushOperator>])
            },
            move |_| {
                Ok(CancelInSinkFinish {
                    inner: CollectSink::new(&pool_sink),
                    stop_cancel: stop1_sink.clone(),
                    cancel_on_consume: true,
                    cancel_on_finish: true,
                    seen_batches: 0,
                })
            },
        )
        .unwrap();
        assert_eq!(sinks1.len(), 1);
        assert!(!ctx.eval.cancel.is_cancelled());
        drop(sinks1);
        assert_eq!(pool.reserved(), 0);

        // Case 2: op.finish returns Err(Cancelled) when stop_cancel fires
        let stop2 = CancelToken::new();
        let src2 = BatchSource::new(Arc::clone(&schema), vec![b1]).unwrap();
        let parts2 = src2.open_partitions(&ScanSpec::default(), &ctx).unwrap();
        let stop2_op = stop2.clone();
        let stop2_sink = stop2.clone();
        let pool_op2 = Arc::clone(&pool);
        let pool_sink2 = Arc::clone(&pool);
        let sinks2 = run_pipeline_with_stop(
            &ctx,
            Some(stop2),
            parts2,
            move |_| {
                Ok(vec![Box::new(CancelDuringOpFinish {
                    stop_cancel: stop2_op.clone(),
                    emit_batch: None,
                    return_cancelled: true,
                    scratch: pool_op2.register("test_op2", false),
                }) as Box<dyn PushOperator>])
            },
            move |_| {
                Ok(CancelInSinkFinish {
                    inner: CollectSink::new(&pool_sink2),
                    stop_cancel: stop2_sink.clone(),
                    cancel_on_consume: false,
                    cancel_on_finish: false,
                    seen_batches: 0,
                })
            },
        )
        .unwrap();
        assert_eq!(sinks2.len(), 1);
        assert!(!ctx.eval.cancel.is_cancelled());
        drop(sinks2);
        assert_eq!(pool.reserved(), 0);
    }
}
