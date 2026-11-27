//! Bounded parallel map helpers (`std::thread::scope` and persistent zero-`unsafe` worker pool)
//! with deterministic ordering.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use crate::StatamError;

/// Pluggable task executor allowing callers (e.g. `cetra-storage` / `cetra-query`)
/// to execute Statam's internal parallel batches on an existing shared worker pool
/// instead of spawning `statam-pool-*` threads.
pub trait TaskExecutor: Send + Sync {
    /// Maximum concurrency supported by this executor.
    fn max_threads(&self) -> usize;

    /// Executes all `tasks` to completion (may run work on the calling thread and/or
    /// pool worker threads). Returning `Err(_)` aborts waiting and propagates the error.
    fn run_batch(&self, tasks: Vec<Box<dyn FnOnce() + Send + 'static>>) -> Result<(), StatamError>;
}

/// Maximum worker threads spawned by [`parallel_map_in_order_static`].
pub(crate) const MAX_PARALLEL_THREADS: usize = 32;

/// Maximum persistent background worker threads kept in the global [`WorkerPool`]
/// (caller thread always participates as worker #0).
const MAX_POOL_WORKERS: usize = MAX_PARALLEL_THREADS - 1;

/// Brief spin iterations after finishing a job before sleeping on the pool condvar,
/// amortizing back-to-back parallel barriers (e.g. Phase 1 -> Stage 1 -> Stage B).
const WORKER_SPIN_ITERS: usize = 256;

thread_local! {
    static IN_PARALLEL_SCOPE: Cell<bool> = const { Cell::new(false) };
    static ACTIVE_EXECUTOR: RefCell<Option<Arc<dyn TaskExecutor>>> = const { RefCell::new(None) };
}

/// RAII guard restoring the thread-local [`TaskExecutor`] on drop.
pub(crate) struct ExecutorScopeGuard(Option<Arc<dyn TaskExecutor>>);

impl Drop for ExecutorScopeGuard {
    fn drop(&mut self) {
        let prev = self.0.take();
        ACTIVE_EXECUTOR.with(|cell| {
            *cell.borrow_mut() = prev;
        });
    }
}

/// Scopes `exec` as the active [`TaskExecutor`] for the current thread until the returned guard drops.
pub(crate) fn enter_executor_scope(exec: Option<&Arc<dyn TaskExecutor>>) -> ExecutorScopeGuard {
    let next = exec.cloned();
    let prev = ACTIVE_EXECUTOR.with(|cell| std::mem::replace(&mut *cell.borrow_mut(), next));
    ExecutorScopeGuard(prev)
}

#[inline]
pub(crate) fn active_executor() -> Option<Arc<dyn TaskExecutor>> {
    ACTIVE_EXECUTOR.with(|cell| cell.borrow().clone())
}

/// Returns `true` if the calling thread is already inside a parallel map scope.
#[inline]
pub(crate) fn in_parallel_scope() -> bool {
    IN_PARALLEL_SCOPE.with(Cell::get)
}

struct ParallelScopeGuard(bool);

impl ParallelScopeGuard {
    fn enter() -> Self {
        let prev = IN_PARALLEL_SCOPE.with(|flag| flag.replace(true));
        Self(prev)
    }
}

impl Drop for ParallelScopeGuard {
    fn drop(&mut self) {
        IN_PARALLEL_SCOPE.with(|flag| flag.set(self.0));
    }
}

type PoolJobFn = Arc<dyn Fn() + Send + Sync + 'static>;

struct PoolJob {
    remaining_helpers: usize,
    stopped: Arc<AtomicBool>,
    in_queue: Arc<AtomicBool>,
    run: PoolJobFn,
}

struct PoolInner {
    queue: VecDeque<PoolJob>,
    spawned: usize,
    /// Strict invariant under `mu`: `busy + idle_workers == spawned`.
    idle_workers: usize,
}

struct WorkerPool {
    mu: Mutex<PoolInner>,
    cv: Condvar,
    has_work: AtomicBool,
}

static POOL: OnceLock<WorkerPool> = OnceLock::new();

fn global_pool() -> &'static WorkerPool {
    POOL.get_or_init(|| WorkerPool {
        mu: Mutex::new(PoolInner {
            queue: VecDeque::new(),
            spawned: 0,
            idle_workers: 0,
        }),
        cv: Condvar::new(),
        has_work: AtomicBool::new(false),
    })
}

impl WorkerPool {
    fn prune_front_dead(inner: &mut PoolInner, has_work: &AtomicBool) -> Vec<PoolJob> {
        let mut dead_jobs = Vec::new();
        while let Some(front) = inner.queue.front() {
            if front.remaining_helpers == 0 || front.stopped.load(Ordering::Acquire) {
                if let Some(dead) = inner.queue.pop_front() {
                    dead.in_queue.store(false, Ordering::Release);
                    dead_jobs.push(dead);
                }
            } else {
                break;
            }
        }
        has_work.store(!inner.queue.is_empty(), Ordering::Release);
        dead_jobs
    }

    fn claim_next_job(
        inner: &mut PoolInner,
        has_work: &AtomicBool,
    ) -> (Option<PoolJobFn>, Vec<PoolJob>) {
        let mut dead_jobs = Self::prune_front_dead(inner, has_work);
        let Some(front) = inner.queue.front_mut() else {
            return (None, dead_jobs);
        };
        front.remaining_helpers -= 1;
        let run = if front.remaining_helpers == 0 {
            let Some(job) = inner.queue.pop_front() else {
                return (None, dead_jobs);
            };
            job.in_queue.store(false, Ordering::Release);
            let mut more_dead = Self::prune_front_dead(inner, has_work);
            dead_jobs.append(&mut more_dead);
            job.run
        } else {
            Arc::clone(&front.run)
        };
        (Some(run), dead_jobs)
    }

    fn submit(
        &self,
        helpers: usize,
        stopped: Arc<AtomicBool>,
        in_queue: Arc<AtomicBool>,
        run: PoolJobFn,
    ) {
        if helpers == 0 {
            return;
        }

        let mut dead_jobs = Vec::new();
        let (to_wake, spawned_snapshot) = {
            let mut inner = self.mu.lock().unwrap_or_else(|p| p.into_inner());
            let mut i = 0usize;
            while i < inner.queue.len() {
                let live = inner.queue[i].remaining_helpers > 0
                    && !inner.queue[i].stopped.load(Ordering::Acquire);
                if !live {
                    if let Some(dead) = inner.queue.remove(i) {
                        dead.in_queue.store(false, Ordering::Release);
                        dead_jobs.push(dead);
                    }
                } else {
                    i += 1;
                }
            }

            let queued_live_helpers: usize = inner
                .queue
                .iter()
                .map(|j| j.remaining_helpers)
                .fold(0usize, usize::saturating_add);
            let need_more = queued_live_helpers
                .saturating_add(helpers)
                .saturating_sub(inner.idle_workers);
            let can_spawn = MAX_POOL_WORKERS.saturating_sub(inner.spawned);
            let to_spawn = need_more.min(can_spawn);

            for _ in 0..to_spawn {
                let worker_id = inner.spawned;
                let res = std::thread::Builder::new()
                    .name(format!("statam-pool-{worker_id}"))
                    .spawn(|| {
                        global_pool().worker_loop();
                    });
                match res {
                    Ok(_) => {
                        inner.spawned += 1;
                        inner.idle_workers += 1;
                    }
                    Err(_) => break,
                }
            }

            if inner.spawned == 0 {
                self.has_work
                    .store(!inner.queue.is_empty(), Ordering::Release);
                (0, 0)
            } else {
                in_queue.store(true, Ordering::Release);
                inner.queue.push_back(PoolJob {
                    remaining_helpers: helpers,
                    stopped,
                    in_queue,
                    run,
                });
                self.has_work.store(true, Ordering::Release);
                let to_wake = helpers.min(inner.idle_workers);
                (to_wake, inner.spawned)
            }
        };
        drop(dead_jobs);

        if to_wake > 0 {
            if to_wake >= spawned_snapshot {
                self.cv.notify_all();
            } else {
                for _ in 0..to_wake {
                    self.cv.notify_one();
                }
            }
        }
    }

    fn prune_stopped(&self) {
        let mut dead_jobs = Vec::new();
        {
            let mut inner = self.mu.lock().unwrap_or_else(|p| p.into_inner());
            let mut i = 0usize;
            while i < inner.queue.len() {
                let live = inner.queue[i].remaining_helpers > 0
                    && !inner.queue[i].stopped.load(Ordering::Acquire);
                if !live {
                    if let Some(dead) = inner.queue.remove(i) {
                        dead.in_queue.store(false, Ordering::Release);
                        dead_jobs.push(dead);
                    }
                } else {
                    i += 1;
                }
            }
            self.has_work
                .store(!inner.queue.is_empty(), Ordering::Release);
        }
        drop(dead_jobs);
    }

    fn worker_loop(&self) {
        let mut inner = self.mu.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            let (claimed, dead_jobs) = Self::claim_next_job(&mut inner, &self.has_work);
            if let Some(job_run) = claimed {
                inner.idle_workers -= 1;
                drop(inner);
                drop(dead_jobs);

                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    let _scope_guard = ParallelScopeGuard::enter();
                    let job_run = job_run;
                    (job_run)();
                    drop(job_run);
                }));

                inner = self.mu.lock().unwrap_or_else(|p| p.into_inner());
                inner.idle_workers += 1;

                // Fast re-pop while still holding `mu` if another live batch is already queued.
                let fast_dead = Self::prune_front_dead(&mut inner, &self.has_work);
                if !inner.queue.is_empty() {
                    if !fast_dead.is_empty() {
                        // Drop pruned jobs outside `mu` only on the rare path where any were pruned.
                        drop(inner);
                        drop(fast_dead);
                        inner = self.mu.lock().unwrap_or_else(|p| p.into_inner());
                    }
                    continue;
                }

                // Brief spin outside `mu` for back-to-back barriers, then re-check under `mu`
                // before sleeping on `cv`.
                drop(inner);
                drop(fast_dead);
                for _ in 0..WORKER_SPIN_ITERS {
                    if self.has_work.load(Ordering::Acquire) {
                        break;
                    }
                    std::hint::spin_loop();
                }
                inner = self.mu.lock().unwrap_or_else(|p| p.into_inner());
                continue;
            }

            if !dead_jobs.is_empty() {
                drop(inner);
                drop(dead_jobs);
                inner = self.mu.lock().unwrap_or_else(|p| p.into_inner());
                continue;
            }

            inner = self.cv.wait(inner).unwrap_or_else(|p| p.into_inner());
        }
    }
}

struct PoolPruneGuard {
    pool: &'static WorkerPool,
    stopped: Arc<AtomicBool>,
    in_queue: Arc<AtomicBool>,
}

impl Drop for PoolPruneGuard {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if self.in_queue.load(Ordering::Acquire) {
            self.pool.prune_stopped();
        }
    }
}

struct BatchShared<T, R> {
    queue: Mutex<std::iter::Enumerate<std::vec::IntoIter<T>>>,
    slots: Mutex<Vec<Option<Result<R, StatamError>>>>,
    first_err_idx: AtomicUsize,
    panicked: AtomicBool,
    stopped: Arc<AtomicBool>,
    done_mu: Mutex<usize>,
    done_cv: Condvar,
}

struct BatchState<C, T, R> {
    ctx: C,
    shared: BatchShared<T, R>,
}

/// Token proving a [`BatchWorker`] finished running its work loop.
pub(crate) struct BatchWorkerDone {
    _private: (),
}

enum BatchWorkerInner<'a, T, R> {
    Sequential {
        items: &'a mut Vec<T>,
        result: &'a mut Option<Result<Vec<R>, StatamError>>,
    },
    Parallel {
        shared: &'a BatchShared<T, R>,
    },
}

/// Per-worker handle passed to [`parallel_map_in_order_static_with_ctx`] closures so
/// per-worker state `S` can borrow directly from `&C` on the worker's stack with zero `unsafe`.
pub(crate) struct BatchWorker<'a, T, R> {
    inner: BatchWorkerInner<'a, T, R>,
}

struct WorkerFlushGuard<'a, T, R> {
    shared: &'a BatchShared<T, R>,
    local: Vec<(usize, Result<R, StatamError>)>,
    joined_active: bool,
}

impl<T, R> Drop for WorkerFlushGuard<'_, T, R> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.shared.panicked.store(true, Ordering::Release);
            self.shared.stopped.store(true, Ordering::Release);
        } else if !self.local.is_empty() {
            let mut slots = self.shared.slots.lock().unwrap_or_else(|p| p.into_inner());
            for (idx, res) in self.local.drain(..) {
                if let Some(slot) = slots.get_mut(idx) {
                    *slot = Some(res);
                }
            }
        }
        if self.joined_active {
            let mut active = self
                .shared
                .done_mu
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            *active -= 1;
            if *active == 0 {
                self.shared.done_cv.notify_one();
            }
        }
    }
}

impl<T, R> BatchWorker<'_, T, R> {
    /// Processes items without per-worker state.
    pub(crate) fn run(self, mut f: impl FnMut(T) -> Result<R, StatamError>) -> BatchWorkerDone {
        self.run_with_init(|| Ok(()), |(), item| f(item))
    }

    /// Processes items with lazy per-worker state `S` initialized at most once per worker.
    pub(crate) fn run_with_init<S>(
        self,
        init: impl FnOnce() -> Result<S, StatamError>,
        mut f: impl FnMut(&mut S, T) -> Result<R, StatamError>,
    ) -> BatchWorkerDone {
        match self.inner {
            BatchWorkerInner::Sequential { items, result } => {
                let mut state: Option<S> = None;
                let mut init = Some(init);
                let mut out = Vec::with_capacity(items.len());
                for item in items.drain(..) {
                    let st = match state.as_mut() {
                        Some(st) => st,
                        None => {
                            let Some(init_fn) = init.take() else {
                                *result = Some(Err(StatamError::Corrupt {
                                    reason: "parallel worker missing init closure",
                                }));
                                return BatchWorkerDone { _private: () };
                            };
                            match init_fn() {
                                Ok(s) => state.insert(s),
                                Err(e) => {
                                    *result = Some(Err(e));
                                    return BatchWorkerDone { _private: () };
                                }
                            }
                        }
                    };
                    match f(st, item) {
                        Ok(val) => out.push(val),
                        Err(e) => {
                            *result = Some(Err(e));
                            return BatchWorkerDone { _private: () };
                        }
                    }
                }
                *result = Some(Ok(out));
                BatchWorkerDone { _private: () }
            }
            BatchWorkerInner::Parallel { shared } => {
                // Exact drop order (reverse of declaration):
                //   1. `state` drops FIRST (if `drop(state)` panics, `flush_guard` observes it!)
                //   2. `flush_guard` drops SECOND (records panic/results and decrements `done_mu`)
                //   3. `_scope_guard` drops LAST (restores `IN_PARALLEL_SCOPE`)
                let _scope_guard = ParallelScopeGuard::enter();
                let mut flush_guard = WorkerFlushGuard {
                    shared,
                    local: Vec::new(),
                    joined_active: false,
                };
                let mut state: Option<S> = None;
                let mut init = Some(init);

                loop {
                    if shared.stopped.load(Ordering::Acquire) {
                        break;
                    }
                    let next = {
                        let mut q = shared.queue.lock().unwrap_or_else(|p| p.into_inner());
                        if shared.stopped.load(Ordering::Acquire)
                            || shared.first_err_idx.load(Ordering::Acquire) != usize::MAX
                            || shared.panicked.load(Ordering::Acquire)
                        {
                            shared.stopped.store(true, Ordering::Release);
                            None
                        } else if let Some(pair) = q.next() {
                            if !flush_guard.joined_active {
                                // Lock hierarchy: `queue` -> `done_mu`.
                                let mut active =
                                    shared.done_mu.lock().unwrap_or_else(|p| p.into_inner());
                                *active += 1;
                                drop(active);
                                flush_guard.joined_active = true;
                            }
                            Some(pair)
                        } else {
                            shared.stopped.store(true, Ordering::Release);
                            None
                        }
                    };

                    let Some((idx, item)) = next else {
                        break;
                    };

                    let st = match state.as_mut() {
                        Some(st) => st,
                        None => {
                            let Some(init_fn) = init.take() else {
                                shared.first_err_idx.fetch_min(idx, Ordering::AcqRel);
                                shared.stopped.store(true, Ordering::Release);
                                flush_guard.local.push((
                                    idx,
                                    Err(StatamError::Corrupt {
                                        reason: "parallel worker missing init closure",
                                    }),
                                ));
                                break;
                            };
                            match init_fn() {
                                Ok(s) => state.insert(s),
                                Err(e) => {
                                    shared.first_err_idx.fetch_min(idx, Ordering::AcqRel);
                                    shared.stopped.store(true, Ordering::Release);
                                    flush_guard.local.push((idx, Err(e)));
                                    break;
                                }
                            }
                        }
                    };

                    let res = f(st, item);
                    let is_err = res.is_err();
                    if is_err {
                        shared.first_err_idx.fetch_min(idx, Ordering::AcqRel);
                        shared.stopped.store(true, Ordering::Release);
                    }
                    flush_guard.local.push((idx, res));
                    if is_err {
                        break;
                    }
                }

                BatchWorkerDone { _private: () }
            }
        }
    }
}

/// Applies `run_worker` across `items` on the persistent zero-`unsafe` [`WorkerPool`] with shared
/// `'static` context `ctx`, allowing per-worker state `S` to borrow `&C` across the worker loop.
///
/// Zero-cost fast path: empty inputs or sequential/nested calls execute directly on the caller
/// thread without initializing the global worker pool or spawning any OS threads.
pub(crate) fn parallel_map_in_order_static_with_ctx<C, T, R, W>(
    ctx: C,
    items: Vec<T>,
    threads: usize,
    run_worker: W,
) -> Result<Vec<R>, StatamError>
where
    C: Send + Sync + 'static,
    T: Send + 'static,
    R: Send + 'static,
    W: Fn(&C, BatchWorker<'_, T, R>) -> BatchWorkerDone + Send + Sync + 'static,
{
    let len = items.len();
    if len == 0 {
        return Ok(Vec::new());
    }
    let nested = in_parallel_scope();
    let threads = if nested {
        1
    } else {
        threads.clamp(1, MAX_PARALLEL_THREADS)
    };
    if threads <= 1 || len <= 1 {
        let mut items = items;
        let mut result = None;
        let _done = run_worker(
            &ctx,
            BatchWorker {
                inner: BatchWorkerInner::Sequential {
                    items: &mut items,
                    result: &mut result,
                },
            },
        );
        return result.unwrap_or(Err(StatamError::Corrupt {
            reason: "parallel worker missing sequential result",
        }));
    }

    let custom_exec = active_executor();
    let workers = if let Some(ref exec) = custom_exec {
        threads.min(exec.max_threads().max(1)).min(len)
    } else {
        threads.min(len)
    };
    if workers <= 1 {
        let mut items = items;
        let mut result = None;
        let _done = run_worker(
            &ctx,
            BatchWorker {
                inner: BatchWorkerInner::Sequential {
                    items: &mut items,
                    result: &mut result,
                },
            },
        );
        return result.unwrap_or(Err(StatamError::Corrupt {
            reason: "parallel worker missing sequential result",
        }));
    }

    let mut slots = Vec::with_capacity(len);
    slots.resize_with(len, || None);

    let stopped = Arc::new(AtomicBool::new(false));

    let batch = Arc::new(BatchState {
        ctx,
        shared: BatchShared {
            queue: Mutex::new(items.into_iter().enumerate()),
            slots: Mutex::new(slots),
            first_err_idx: AtomicUsize::new(usize::MAX),
            panicked: AtomicBool::new(false),
            stopped: Arc::clone(&stopped),
            done_mu: Mutex::new(0),
            done_cv: Condvar::new(),
        },
    });

    let run_worker = Arc::new(run_worker);

    if let Some(exec) = custom_exec {
        struct ExecBatchDrainGuard<'a, I, O> {
            shared: &'a BatchShared<I, O>,
        }

        impl<I, O> Drop for ExecBatchDrainGuard<'_, I, O> {
            fn drop(&mut self) {
                let mut q = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
                self.shared.stopped.store(true, Ordering::Release);
                for _ in q.by_ref() {}
            }
        }

        let mut tasks: Vec<Box<dyn FnOnce() + Send + 'static>> = Vec::with_capacity(workers);
        for _ in 0..workers {
            let batch_for_task = Arc::clone(&batch);
            let run_worker_for_task = Arc::clone(&run_worker);
            tasks.push(Box::new(move || {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _done = (run_worker_for_task)(
                        &batch_for_task.ctx,
                        BatchWorker {
                            inner: BatchWorkerInner::Parallel {
                                shared: &batch_for_task.shared,
                            },
                        },
                    );
                }));
                if res.is_err() {
                    batch_for_task
                        .shared
                        .panicked
                        .store(true, Ordering::Release);
                    batch_for_task.shared.stopped.store(true, Ordering::Release);
                }
            }));
        }
        let _drain_guard = ExecBatchDrainGuard {
            shared: &batch.shared,
        };
        exec.run_batch(tasks)?;
    } else {
        let helpers = workers - 1;
        let in_queue = Arc::new(AtomicBool::new(false));
        let pool = global_pool();

        let _prune_guard = PoolPruneGuard {
            pool,
            stopped: Arc::clone(&stopped),
            in_queue: Arc::clone(&in_queue),
        };

        if helpers > 0 {
            let batch_for_pool = Arc::clone(&batch);
            let run_worker_for_pool = Arc::clone(&run_worker);
            let job_run: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _done = (run_worker_for_pool)(
                        &batch_for_pool.ctx,
                        BatchWorker {
                            inner: BatchWorkerInner::Parallel {
                                shared: &batch_for_pool.shared,
                            },
                        },
                    );
                }));
                if res.is_err() {
                    batch_for_pool
                        .shared
                        .panicked
                        .store(true, Ordering::Release);
                    batch_for_pool.shared.stopped.store(true, Ordering::Release);
                }
            });
            pool.submit(helpers, stopped, in_queue, job_run);
        }

        // Caller participates as worker #0.
        let _caller_done = (run_worker)(
            &batch.ctx,
            BatchWorker {
                inner: BatchWorkerInner::Parallel {
                    shared: &batch.shared,
                },
            },
        );

        // Mandatory caller queue fence: any helper that pops an item increments `done_mu` while
        // holding `queue`, so acquiring `queue`, setting `stopped = true`, and draining unpopped
        // items makes it impossible for any helper to pop or become active afterwards.
        {
            let mut q = batch.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
            batch.shared.stopped.store(true, Ordering::Release);
            for _ in q.by_ref() {}
        }
        {
            let mut active = batch
                .shared
                .done_mu
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            while *active > 0 {
                active = batch
                    .shared
                    .done_cv
                    .wait(active)
                    .unwrap_or_else(|p| p.into_inner());
            }
        }

        drop(_prune_guard);
    }

    if batch.shared.panicked.load(Ordering::Acquire) {
        return Err(StatamError::Corrupt {
            reason: "parallel worker thread panicked",
        });
    }

    let slots = {
        let mut guard = batch.shared.slots.lock().unwrap_or_else(|p| p.into_inner());
        std::mem::take(&mut *guard)
    };

    let mut out = Vec::with_capacity(len);
    for slot in slots {
        match slot {
            Some(Ok(val)) => out.push(val),
            Some(Err(e)) => return Err(e),
            None => {
                return Err(StatamError::Corrupt {
                    reason: "parallel worker missing output slot",
                });
            }
        }
    }
    Ok(out)
}

/// Applies `f` across `items` on the persistent [`WorkerPool`], returning outputs in input order
/// or the first [`StatamError`] in input order.
pub(crate) fn parallel_map_in_order_static<T, R, F>(
    items: Vec<T>,
    threads: usize,
    f: F,
) -> Result<Vec<R>, StatamError>
where
    T: Send + 'static,
    R: Send + 'static,
    F: Fn(T) -> Result<R, StatamError> + Send + Sync + 'static,
{
    parallel_map_in_order_static_with_ctx(f, items, threads, |f, worker| worker.run(f))
}

/// Applies `f` with lazy per-worker state initialized once by `init` on the persistent
/// [`WorkerPool`], returning outputs or the first error in input order.
#[cfg(test)]
pub(crate) fn parallel_map_in_order_static_with_init<T, S, R, Init, F>(
    items: Vec<T>,
    threads: usize,
    init: Init,
    f: F,
) -> Result<Vec<R>, StatamError>
where
    T: Send + 'static,
    R: Send + 'static,
    Init: Fn() -> Result<S, StatamError> + Send + Sync + 'static,
    F: Fn(&mut S, T) -> Result<R, StatamError> + Send + Sync + 'static,
{
    parallel_map_in_order_static_with_ctx((init, f), items, threads, |(init, f), worker| {
        worker.run_with_init(init, f)
    })
}

/// Applies `f` across `items` using up to `threads.clamp(1, MAX_PARALLEL_THREADS)` worker threads,
/// returning outputs in input order or the first [`StatamError`] in input order.
#[cfg(test)]
pub(crate) fn parallel_map_in_order<T, R, F>(
    items: Vec<T>,
    threads: usize,
    f: F,
) -> Result<Vec<R>, StatamError>
where
    T: Send,
    R: Send,
    F: Fn(T) -> Result<R, StatamError> + Sync,
{
    parallel_map_in_order_with_init(items, threads, || Ok(()), |(), item| f(item))
}

/// Applies `f` with lazy per-worker state initialized once by `init`, using up to
/// `threads.clamp(1, MAX_PARALLEL_THREADS)` threads, returning outputs or the first error in input order.
pub(crate) fn parallel_map_in_order_with_init<T, S, R, Init, F>(
    items: Vec<T>,
    threads: usize,
    init: Init,
    f: F,
) -> Result<Vec<R>, StatamError>
where
    T: Send,
    R: Send,
    Init: Fn() -> Result<S, StatamError> + Sync,
    F: Fn(&mut S, T) -> Result<R, StatamError> + Sync,
{
    let nested = in_parallel_scope();
    let threads = if nested {
        1
    } else {
        threads.clamp(1, MAX_PARALLEL_THREADS)
    };
    let len = items.len();
    if len == 0 {
        return Ok(Vec::new());
    }
    if threads <= 1 || len <= 1 {
        let mut state: Option<S> = None;
        let mut out = Vec::with_capacity(len);
        for item in items {
            let st = match state.as_mut() {
                Some(st) => st,
                None => state.insert(init()?),
            };
            out.push(f(st, item)?);
        }
        return Ok(out);
    }

    let workers = threads.min(len);
    let queue = Mutex::new(items.into_iter().enumerate());
    let first_err_idx = AtomicUsize::new(usize::MAX);
    let run_worker = || -> Vec<(usize, Result<R, StatamError>)> {
        let _scope_guard = ParallelScopeGuard::enter();
        let mut local = Vec::new();
        let mut state: Option<S> = None;
        loop {
            let next = {
                let mut guard = queue.lock().unwrap_or_else(|p| p.into_inner());
                if first_err_idx.load(Ordering::Acquire) != usize::MAX {
                    None
                } else {
                    guard.next()
                }
            };
            let Some((idx, item)) = next else {
                break;
            };
            let st = match state.as_mut() {
                Some(st) => st,
                None => match init() {
                    Ok(s) => state.insert(s),
                    Err(e) => {
                        first_err_idx.fetch_min(idx, Ordering::AcqRel);
                        local.push((idx, Err(e)));
                        break;
                    }
                },
            };
            let res = f(st, item);
            let is_err = res.is_err();
            if is_err {
                first_err_idx.fetch_min(idx, Ordering::AcqRel);
            }
            local.push((idx, res));
            if is_err {
                break;
            }
        }
        local
    };

    let mut slots: Vec<Option<Result<R, StatamError>>> = Vec::with_capacity(len);
    slots.resize_with(len, || None);

    let mut thread_panicked = false;
    std::thread::scope(|scope| {
        let _main_scope_guard = ParallelScopeGuard::enter();
        let mut handles = Vec::with_capacity(workers - 1);
        for _ in 1..workers {
            match std::thread::Builder::new().spawn_scoped(scope, run_worker) {
                Ok(handle) => handles.push(handle),
                Err(_) => break,
            }
        }
        let main_results = run_worker();
        for (idx, res) in main_results {
            if let Some(slot) = slots.get_mut(idx) {
                *slot = Some(res);
            }
        }
        for handle in handles {
            match handle.join() {
                Ok(worker_results) => {
                    for (idx, res) in worker_results {
                        if let Some(slot) = slots.get_mut(idx) {
                            *slot = Some(res);
                        }
                    }
                }
                Err(_) => {
                    thread_panicked = true;
                }
            }
        }
    });

    if thread_panicked {
        return Err(StatamError::Corrupt {
            reason: "parallel worker thread panicked",
        });
    }

    let mut out = Vec::with_capacity(len);
    for slot in slots {
        match slot {
            Some(Ok(val)) => out.push(val),
            Some(Err(e)) => return Err(e),
            None => {
                return Err(StatamError::Corrupt {
                    reason: "parallel worker missing output slot",
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn parallel_map_preserves_order_and_first_error() {
        let empty: Vec<u32> = Vec::new();
        assert_eq!(
            parallel_map_in_order(empty, 4, |x| Ok(x * 2)).unwrap(),
            Vec::<u32>::new()
        );

        let inputs: Vec<u32> = (0..64).collect();
        let expected: Vec<u32> = inputs.iter().map(|&x| x * 3 + 1).collect();
        for threads in [0, 1, 2, 4, 64] {
            let got = parallel_map_in_order(inputs.clone(), threads, |x| Ok(x * 3 + 1)).unwrap();
            assert_eq!(got, expected);
        }

        // First error in input order is returned even if a later index finishes faster.
        let err = parallel_map_in_order(vec![0u32, 1, 2, 3, 4, 5], 4, |x| {
            if x == 2 {
                Err(StatamError::Corrupt { reason: "err_at_2" })
            } else if x == 4 {
                Err(StatamError::BoundsExceeded { reason: "err_at_4" })
            } else {
                Ok(x)
            }
        })
        .unwrap_err();
        assert!(matches!(err, StatamError::Corrupt { reason: "err_at_2" }));
    }

    #[test]
    fn parallel_map_with_init_initializes_at_most_once_per_worker() {
        let init_calls = AtomicUsize::new(0);
        let inputs: Vec<usize> = (0..20).collect();
        let out = parallel_map_in_order_with_init(
            inputs,
            4,
            || {
                init_calls.fetch_add(1, Ordering::Relaxed);
                Ok(10usize)
            },
            |bias, x| Ok(*bias + x),
        )
        .unwrap();
        let expected: Vec<usize> = (10..30).collect();
        assert_eq!(out, expected);
        let calls = init_calls.load(Ordering::Relaxed);
        assert!(
            (1..=4).contains(&calls),
            "expected 1..=4 inits, got {calls}"
        );
    }

    #[test]
    fn panic_inside_f_does_not_poison_in_parallel_scope() {
        assert!(!IN_PARALLEL_SCOPE.with(Cell::get));

        // Every item panics so both the main thread and workers unwind out of `run_worker`.
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = parallel_map_in_order(vec![0usize, 1, 2, 3], 4, |_x| -> Result<usize, _> {
                panic!("intentional worker/main panic");
            });
        }));
        assert!(panicked.is_err());
        assert!(
            !IN_PARALLEL_SCOPE.with(Cell::get),
            "IN_PARALLEL_SCOPE remained poisoned after panic"
        );

        // Subsequent parallel call must still run in parallel across > 1 OS thread.
        let barrier = Barrier::new(2);
        let tids = parallel_map_in_order(vec![0usize, 1], 2, |_x| {
            assert!(IN_PARALLEL_SCOPE.with(Cell::get));
            barrier.wait();
            Ok(std::thread::current().id())
        })
        .unwrap();
        let unique: HashSet<_> = tids.into_iter().collect();
        assert_eq!(unique.len(), 2);
        assert!(!IN_PARALLEL_SCOPE.with(Cell::get));
    }

    #[test]
    fn init_error_propagates_earliest_error_without_missing_slot() {
        let init_attempt = AtomicUsize::new(0);
        let init_barrier = Barrier::new(2);
        let err = parallel_map_in_order_with_init(
            (0usize..16).collect(),
            2,
            || {
                let n = init_attempt.fetch_add(1, Ordering::AcqRel);
                init_barrier.wait();
                if n == 0 {
                    Ok(())
                } else {
                    Err(StatamError::Corrupt {
                        reason: "init_failed",
                    })
                }
            },
            |(), x| Ok(x),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            StatamError::Corrupt {
                reason: "init_failed"
            }
        ));

        // All workers failing `init()` also returns cleanly with the earliest error.
        let err_all = parallel_map_in_order_with_init(
            (0usize..8).collect(),
            4,
            || -> Result<(), StatamError> {
                Err(StatamError::BoundsExceeded {
                    reason: "all_init_failed",
                })
            },
            |(), x| Ok(x),
        )
        .unwrap_err();
        assert!(matches!(
            err_all,
            StatamError::BoundsExceeded {
                reason: "all_init_failed"
            }
        ));
    }

    #[test]
    fn later_error_finishing_before_earlier_error_returns_lowest_index_deterministically() {
        // Synchronize items 1 and 2 on a barrier so item 2 finishes with Err *before* item 1
        // finishes with Err, while items 3..16 should short-circuit without creating holes.
        let both_started = Barrier::new(2);
        let late_errored = AtomicUsize::new(0);
        let err = parallel_map_in_order((0usize..16).collect(), 4, |x| {
            if x == 1 {
                both_started.wait();
                while late_errored.load(Ordering::Acquire) == 0 {
                    std::thread::yield_now();
                }
                Err(StatamError::Corrupt { reason: "err_at_1" })
            } else if x == 2 {
                both_started.wait();
                late_errored.store(1, Ordering::Release);
                Err(StatamError::BoundsExceeded { reason: "err_at_2" })
            } else {
                Ok(x)
            }
        })
        .unwrap_err();
        assert!(matches!(err, StatamError::Corrupt { reason: "err_at_1" }));
    }

    #[test]
    fn pass24_persistent_pool_order_errors_panics_concurrency_and_ctx() {
        // 1. Empty input & order preservation across thread counts (0, 1, 2, 4, 32).
        let empty: Vec<u32> = Vec::new();
        assert_eq!(
            parallel_map_in_order_static(empty, 4, |x| Ok(x * 2)).unwrap(),
            Vec::<u32>::new()
        );
        let inputs: Vec<u32> = (0..64).collect();
        let expected: Vec<u32> = inputs.iter().map(|&x| x * 3 + 7).collect();
        for threads in [0usize, 1, 2, 4, 32] {
            let got =
                parallel_map_in_order_static(inputs.clone(), threads, |x| Ok(x * 3 + 7)).unwrap();
            assert_eq!(got, expected, "mismatch for threads={threads}");
        }

        // 2. Context-borrowing API: per-worker state `BorrowingState<'c>` borrows `&'c C`.
        struct SharedCtx {
            weights: Vec<usize>,
        }
        struct BorrowingState<'c> {
            weights: &'c [usize],
            calls: usize,
        }
        let ctx_init_calls = Arc::new(AtomicUsize::new(0));
        let ctx_init_clone = Arc::clone(&ctx_init_calls);
        let ctx = SharedCtx {
            weights: vec![10, 20, 30, 40],
        };
        let ctx_out = parallel_map_in_order_static_with_ctx(
            (ctx, ctx_init_clone),
            (0usize..32).collect(),
            4,
            |(ctx, init_cnt), worker| {
                worker.run_with_init(
                    || {
                        init_cnt.fetch_add(1, Ordering::Relaxed);
                        Ok(BorrowingState {
                            weights: &ctx.weights,
                            calls: 0,
                        })
                    },
                    |st, item| {
                        st.calls += 1;
                        Ok(st.weights[item % st.weights.len()] + item)
                    },
                )
            },
        )
        .unwrap();
        let ctx_expected: Vec<usize> = (0usize..32).map(|i| [10, 20, 30, 40][i % 4] + i).collect();
        assert_eq!(ctx_out, ctx_expected);
        let inits = ctx_init_calls.load(Ordering::Relaxed);
        assert!(
            (1..=4).contains(&inits),
            "expected 1..=4 inits, got {inits}"
        );

        // 3. `parallel_map_in_order_static_with_init`: at-most-once init & init error propagation.
        let init_cnt = Arc::new(AtomicUsize::new(0));
        let init_cnt_c = Arc::clone(&init_cnt);
        let init_out = parallel_map_in_order_static_with_init(
            (0usize..24).collect(),
            4,
            move || {
                init_cnt_c.fetch_add(1, Ordering::Relaxed);
                Ok(100usize)
            },
            |bias, x| Ok(*bias + x),
        )
        .unwrap();
        assert_eq!(init_out, (100usize..124).collect::<Vec<_>>());
        assert!((1..=4).contains(&init_cnt.load(Ordering::Relaxed)));

        let attempt = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(2));
        let attempt_c = Arc::clone(&attempt);
        let barrier_c = Arc::clone(&barrier);
        let init_err = parallel_map_in_order_static_with_init(
            (0usize..16).collect(),
            2,
            move || {
                let n = attempt_c.fetch_add(1, Ordering::AcqRel);
                barrier_c.wait();
                if n == 0 {
                    Ok(())
                } else {
                    Err(StatamError::Corrupt {
                        reason: "static_init_failed",
                    })
                }
            },
            |(), x| Ok(x),
        )
        .unwrap_err();
        assert!(matches!(
            init_err,
            StatamError::Corrupt {
                reason: "static_init_failed"
            }
        ));

        // 4. Earliest-index error determinism: racing fast error at index 1 vs slow error at index 0.
        let both_started = Arc::new(Barrier::new(2));
        let idx1_errored = Arc::new(AtomicBool::new(false));
        let bs_c = Arc::clone(&both_started);
        let ie_c = Arc::clone(&idx1_errored);
        let first_err = parallel_map_in_order_static((0usize..16).collect(), 4, move |x| {
            if x == 0 {
                bs_c.wait();
                while !ie_c.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                Err(StatamError::Corrupt { reason: "err_at_0" })
            } else if x == 1 {
                bs_c.wait();
                ie_c.store(true, Ordering::Release);
                Err(StatamError::BoundsExceeded { reason: "err_at_1" })
            } else {
                Ok(x)
            }
        })
        .unwrap_err();
        assert!(matches!(
            first_err,
            StatamError::Corrupt { reason: "err_at_0" }
        ));

        // 5. Worker panic recovery (pool helper panics, caller succeeds) & `drop(state)` panic recovery.
        let caller_tid = std::thread::current().id();
        let panic_barrier = Arc::new(Barrier::new(2));
        let pb_c = Arc::clone(&panic_barrier);
        let worker_panic_res = parallel_map_in_order_static(vec![0usize, 1], 2, move |x| {
            pb_c.wait();
            if std::thread::current().id() != caller_tid {
                panic!("intentional pool worker panic");
            }
            Ok(x)
        });
        assert!(matches!(
            worker_panic_res,
            Err(StatamError::Corrupt {
                reason: "parallel worker thread panicked"
            })
        ));
        assert!(!in_parallel_scope());

        // `drop(state)` panic on a helper thread must be observed by `WorkerFlushGuard::drop`
        // (because `state` drops before `flush_guard`), returning Err without hanging or poisoning pool.
        struct PanicOnDropIfHelper {
            caller_tid: std::thread::ThreadId,
        }
        impl Drop for PanicOnDropIfHelper {
            fn drop(&mut self) {
                if std::thread::current().id() != self.caller_tid {
                    panic!("intentional state drop panic");
                }
            }
        }
        let drop_barrier = Arc::new(Barrier::new(2));
        let db_c = Arc::clone(&drop_barrier);
        let drop_panic_res = parallel_map_in_order_static_with_init(
            vec![0usize, 1],
            2,
            move || Ok(PanicOnDropIfHelper { caller_tid }),
            move |_st, x| {
                db_c.wait();
                Ok(x)
            },
        );
        assert!(matches!(
            drop_panic_res,
            Err(StatamError::Corrupt {
                reason: "parallel worker thread panicked"
            })
        ));
        assert!(!in_parallel_scope());

        // Verify pool remains completely healthy and serves a 4-way Barrier immediately after panics!
        let b4 = Arc::new(Barrier::new(4));
        let b4_c = Arc::clone(&b4);
        let tids = parallel_map_in_order_static(vec![0usize, 1, 2, 3], 4, move |_x| {
            assert!(in_parallel_scope());
            b4_c.wait();
            Ok(std::thread::current().id())
        })
        .unwrap();
        let unique_tids: HashSet<_> = tids.into_iter().collect();
        assert_eq!(unique_tids.len(), 4);
        assert!(!in_parallel_scope());

        // 6. Caller panic recovery: `IN_PARALLEL_SCOPE` unpoisoned & `PoolJob` pruned.
        let sentinel = Arc::new(());
        let weak = Arc::downgrade(&sentinel);
        let caller_panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _ = parallel_map_in_order_static(
                vec![0usize, 1, 2, 3],
                4,
                move |_x| -> Result<usize, _> {
                    let _keep = &sentinel;
                    panic!("intentional caller+worker panic");
                },
            );
        }));
        assert!(caller_panicked.is_err());
        assert!(
            !in_parallel_scope(),
            "IN_PARALLEL_SCOPE remained poisoned after caller panic"
        );
        // Any helper that already claimed `job_run` finishes unwinding within microseconds;
        // once it drops its `Arc`, `weak.upgrade()` must become `None` (proving `PoolPruneGuard`
        // pruned our `PoolJob` from `global_pool()` rather than leaking it in the queue).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while weak.upgrade().is_some() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(
            weak.upgrade().is_none(),
            "expected PoolPruneGuard to prune and drop stopped PoolJob on unwind"
        );
        {
            let inner = global_pool().mu.lock().unwrap_or_else(|p| p.into_inner());
            assert!(
                !inner
                    .queue
                    .iter()
                    .any(|j| j.stopped.load(Ordering::Acquire)),
                "expected no stopped zombie PoolJobs left in global_pool queue"
            );
        }

        // 7. Nested re-entry executes sequentially without deadlocking or touching the pool.
        let nested_out = parallel_map_in_order_static(vec![0usize, 1, 2, 3], 4, |outer| {
            assert!(in_parallel_scope());
            let outer_tid = std::thread::current().id();
            let inner = parallel_map_in_order_static(vec![10usize, 20, 30], 4, move |v| {
                assert_eq!(std::thread::current().id(), outer_tid);
                Ok(v + outer)
            })?;
            Ok(inner)
        })
        .unwrap();
        assert_eq!(
            nested_out,
            vec![
                vec![10, 20, 30],
                vec![11, 21, 31],
                vec![12, 22, 32],
                vec![13, 23, 33],
            ]
        );
        assert!(!in_parallel_scope());

        // 8. 8 concurrent external threads calling `parallel_map_in_order_static` simultaneously,
        //    including concurrent `Barrier::new(4)` callers (8 * 3 = 24 <= 31 pool workers).
        let start_all = Arc::new(Barrier::new(8));
        let mut handles = Vec::with_capacity(8);
        for t in 0..8usize {
            let sa = Arc::clone(&start_all);
            handles.push(std::thread::spawn(move || {
                sa.wait();
                // Concurrent 4-way barrier batch (verifies pool growth under simultaneous demand).
                let local_b = Arc::new(Barrier::new(4));
                let lb_c = Arc::clone(&local_b);
                let btids = parallel_map_in_order_static(vec![0usize, 1, 2, 3], 4, move |x| {
                    lb_c.wait();
                    Ok((t * 10 + x, std::thread::current().id()))
                })
                .unwrap();
                assert_eq!(btids.len(), 4);
                let distinct: HashSet<_> = btids.iter().map(|(_, id)| *id).collect();
                assert_eq!(distinct.len(), 4);

                // Additional back-to-back compute waves.
                for round in 0..4usize {
                    let res = parallel_map_in_order_static((0usize..32).collect(), 4, move |x| {
                        Ok(t * 1000 + round * 100 + x)
                    })
                    .unwrap();
                    let exp: Vec<usize> =
                        (0usize..32).map(|x| t * 1000 + round * 100 + x).collect();
                    assert_eq!(res, exp);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert!(!in_parallel_scope());
    }
}
