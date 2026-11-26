//! Push-based pipeline traits, execution context, metrics, and parallel runner.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use cetra_column::{Batch, Reservation, ScopedPool};

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

/// Push-based stateless/stateful pipeline operator transforming one `Batch` into zero or more `Batch`es.
pub trait PushOperator: Send {
    /// Operator name for `EXPLAIN` and [`ExecMetrics`].
    fn name(&self) -> &'static str;
    /// Processes `batch` and forwards zero or more output batches to `emit`.
    /// Scratch `Reservation`s held for an output batch MUST be freed right after `emit(out)` returns.
    fn push(
        &mut self,
        batch: Batch,
        emit: &mut dyn FnMut(Batch) -> Result<ControlFlow, QueryError>,
    ) -> Result<ControlFlow, QueryError>;
    /// Called once when upstream finishes cleanly (not cancelled/stopped) to flush buffered output.
    fn finish(
        &mut self,
        emit: &mut dyn FnMut(Batch) -> Result<ControlFlow, QueryError>,
    ) -> Result<(), QueryError> {
        let _ = emit;
        Ok(())
    }
}

/// Terminal consumer of one pipeline partition.
pub trait PushSink: Send {
    /// Consumes one batch; returning `ControlFlow::Stop` terminates the partition early.
    fn consume(&mut self, batch: Batch) -> Result<ControlFlow, QueryError>;
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
    if partitions.is_empty() {
        return Ok(Vec::new());
    }

    let make_chain = Arc::new(make_chain);
    let make_sink = Arc::new(make_sink);
    let tasks: Vec<_> = partitions
        .into_iter()
        .enumerate()
        .map(|(partition_idx, mut stream)| {
            let make_chain = Arc::clone(&make_chain);
            let make_sink = Arc::clone(&make_sink);
            let pool = Arc::clone(&ctx.pool);
            let cancel = ctx.eval.cancel.clone();
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
                    if cancel.is_cancelled() {
                        scan_res.free();
                        return Err(QueryError::Cancelled);
                    }
                    let next = stream.next_batch(&cancel, Some(&mut scan_res));
                    let batch = match next {
                        Ok(Some(b)) => b,
                        Ok(None) => {
                            scan_res.free();
                            break;
                        }
                        Err(err) => {
                            scan_res.free();
                            return Err(err);
                        }
                    };

                    let push_res = push_through_slice(&mut chain, &mut profiles, batch, &mut sink);
                    scan_res.free();
                    match push_res? {
                        ControlFlow::Continue => {}
                        ControlFlow::Stop => {
                            stopped = true;
                        }
                    }
                }

                if cancel.is_cancelled() {
                    return Err(QueryError::Cancelled);
                }

                if !stopped {
                    for i in 0..chain.len() {
                        if cancel.is_cancelled() {
                            return Err(QueryError::Cancelled);
                        }
                        let mut downstream_flow = ControlFlow::Continue;
                        let mut emit_ns: u64 = 0;
                        let (head, tail) = chain.split_at_mut(i + 1);
                        let (prof_head, prof_tail) = profiles.split_at_mut(i + 1);
                        let op = &mut head[i];
                        let op_prof = &mut prof_head[i];

                        let t0 = Instant::now();
                        op.finish(&mut |out_batch| {
                            op_prof.rows_out =
                                op_prof.rows_out.saturating_add(out_batch.num_rows() as u64);
                            op_prof.batches_out = op_prof.batches_out.saturating_add(1);
                            let te = Instant::now();
                            let f = push_through_slice(tail, prof_tail, out_batch, &mut sink);
                            emit_ns = emit_ns.saturating_add(
                                te.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                            );
                            let f = f?;
                            if f == ControlFlow::Stop {
                                downstream_flow = ControlFlow::Stop;
                            }
                            Ok(f)
                        })?;
                        let total_ns = t0.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                        op_prof.elapsed_ns = op_prof
                            .elapsed_ns
                            .saturating_add(total_ns.saturating_sub(emit_ns));

                        if downstream_flow == ControlFlow::Stop {
                            break;
                        }
                    }
                }

                if cancel.is_cancelled() {
                    return Err(QueryError::Cancelled);
                }
                sink.finish()?;
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
    sink: &mut S,
) -> Result<ControlFlow, QueryError> {
    let Some((op, rest)) = chain.split_first_mut() else {
        return sink.consume(batch);
    };
    let Some((prof, rest_prof)) = profiles.split_first_mut() else {
        return Err(QueryError::internal("operator profile count mismatch"));
    };

    let mut downstream_flow = ControlFlow::Continue;
    let mut emit_ns: u64 = 0;
    let t0 = Instant::now();
    let op_flow = op.push(batch, &mut |out_batch| {
        prof.rows_out = prof.rows_out.saturating_add(out_batch.num_rows() as u64);
        prof.batches_out = prof.batches_out.saturating_add(1);
        let te = Instant::now();
        let f = push_through_slice(rest, rest_prof, out_batch, sink);
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
    fn consume(&mut self, batch: Batch) -> Result<ControlFlow, QueryError> {
        self.res.try_grow(batch.memory_bytes())?;
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
            emit: &mut dyn FnMut(Batch) -> Result<ControlFlow, QueryError>,
        ) -> Result<ControlFlow, QueryError> {
            self.seen = true;
            let _ = emit(batch)?;
            Ok(ControlFlow::Stop)
        }

        fn finish(
            &mut self,
            _emit: &mut dyn FnMut(Batch) -> Result<ControlFlow, QueryError>,
        ) -> Result<(), QueryError> {
            panic!("finish must not be called after ControlFlow::Stop");
        }
    }

    struct PassThroughAndFlushOp {
        flushed: Option<Batch>,
    }

    impl PushOperator for PassThroughAndFlushOp {
        fn name(&self) -> &'static str {
            "PassThroughAndFlush"
        }

        fn push(
            &mut self,
            batch: Batch,
            emit: &mut dyn FnMut(Batch) -> Result<ControlFlow, QueryError>,
        ) -> Result<ControlFlow, QueryError> {
            emit(batch)
        }

        fn finish(
            &mut self,
            emit: &mut dyn FnMut(Batch) -> Result<ControlFlow, QueryError>,
        ) -> Result<(), QueryError> {
            if let Some(b) = self.flushed.take() {
                let _ = emit(b)?;
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
        fn consume(&mut self, batch: Batch) -> Result<ControlFlow, QueryError> {
            // While `consume` runs, `run_pipeline` still holds `scan_res == batch.memory_bytes()`.
            let expected_min = batch.memory_bytes() + self.inner.reserved_bytes();
            assert!(
                self.pool.reserved() >= expected_min,
                "expected reserved >= {expected_min}, got {}",
                self.pool.reserved()
            );
            self.batch_bytes_seen.push(batch.memory_bytes());
            self.inner.consume(batch)
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
        let sinks2 = run_pipeline(
            &ctx2,
            parts2,
            move |_| {
                Ok(vec![Box::new(PassThroughAndFlushOp {
                    flushed: Some(extra.clone()),
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
}
