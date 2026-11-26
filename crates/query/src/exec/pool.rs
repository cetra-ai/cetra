//! Process-wide bounded CPU worker pool with 8 MiB worker stacks and fair round-robin
//! scheduling across concurrent queries (`D-QE-5`, `design.md` § Contracts 3).

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

use cetra_column::CancelToken;

use crate::error::QueryError;

/// Fixed OS stack size for every query worker thread (8 MiB, matching `cetra-cli`'s Tokio stack).
const WORKER_STACK_BYTES: usize = 8 * 1024 * 1024;

thread_local! {
    static IN_WORKER_POOL: Cell<bool> = const { Cell::new(false) };
}

struct InWorkerPoolGuard {
    prev: bool,
}

impl InWorkerPoolGuard {
    fn enter() -> Self {
        let prev = IN_WORKER_POOL.with(|flag| {
            let prev = flag.get();
            flag.set(true);
            prev
        });
        Self { prev }
    }
}

impl Drop for InWorkerPoolGuard {
    fn drop(&mut self) {
        IN_WORKER_POOL.with(|flag| flag.set(self.prev));
    }
}

type TaskFn = Box<dyn FnOnce(InFlightGuard) + Send>;

struct InFlightGuard {
    counter: Arc<AtomicUsize>,
}

impl InFlightGuard {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self { counter }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

struct PoolInner {
    order: VecDeque<u64>,
    queues: HashMap<u64, VecDeque<TaskFn>>,
    next_query_id: u64,
    shutdown: bool,
}

struct PoolShared {
    inner: Mutex<PoolInner>,
    cv: Condvar,
    in_flight: Arc<AtomicUsize>,
    num_workers: usize,
}

impl fmt::Debug for PoolShared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.lock();
        f.debug_struct("PoolShared")
            .field("num_workers", &self.num_workers)
            .field("in_flight", &self.in_flight.load(Ordering::Acquire))
            .field("queued_queries", &inner.queues.len())
            .field("shutdown", &inner.shutdown)
            .finish()
    }
}

impl PoolShared {
    fn lock(&self) -> MutexGuard<'_, PoolInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Process-wide bounded CPU worker pool with 8 MiB worker stacks and fair round-robin
/// scheduling across concurrent queries (`D-QE-5`, `design.md` § Contracts 3).
#[derive(Debug)]
pub struct WorkerPool {
    shared: Arc<PoolShared>,
    threads: Vec<JoinHandle<()>>,
}

impl WorkerPool {
    /// Creates a worker pool with `num_workers.max(1)` OS threads, each with an 8 MiB stack
    /// (`std::thread::Builder::new().name(...).stack_size(8 * 1024 * 1024)`).
    pub fn new(num_workers: usize) -> Result<Self, QueryError> {
        let count = num_workers.max(1);
        let shared = Arc::new(PoolShared {
            inner: Mutex::new(PoolInner {
                order: VecDeque::new(),
                queues: HashMap::new(),
                next_query_id: 1,
                shutdown: false,
            }),
            cv: Condvar::new(),
            in_flight: Arc::new(AtomicUsize::new(0)),
            num_workers: count,
        });

        let mut threads = Vec::with_capacity(count);
        for idx in 0..count {
            let worker_shared = Arc::clone(&shared);
            match thread::Builder::new()
                .name(format!("cetra-query-worker-{idx}"))
                .stack_size(WORKER_STACK_BYTES)
                .spawn(move || worker_loop(&worker_shared))
            {
                Ok(handle) => threads.push(handle),
                Err(err) => {
                    {
                        let mut guard = shared.lock();
                        guard.shutdown = true;
                        guard.order.clear();
                        guard.queues.clear();
                    }
                    shared.cv.notify_all();
                    for handle in threads {
                        let _ = handle.join();
                    }
                    return Err(QueryError::internal(format!(
                        "failed to spawn query worker thread: {err}"
                    )));
                }
            }
        }

        Ok(Self { shared, threads })
    }

    /// Returns the number of CPU worker threads in this pool.
    pub fn num_workers(&self) -> usize {
        self.shared.num_workers
    }

    #[cfg(test)]
    pub(crate) fn in_flight_count(&self) -> usize {
        self.shared.in_flight.load(Ordering::Acquire)
    }

    /// Executes `tasks` in parallel on the worker pool under fair round-robin scheduling,
    /// returning results in **original task index order** (empty `tasks` returns `Ok(Vec::new())` immediately).
    ///
    /// On first error (or cancellation), drains unstarted tasks, calls `cancel.cancel()`, and
    /// **synchronously blocks until all in-flight tasks finish** before returning.
    /// Panics inside tasks are caught via `std::panic::catch_unwind` and mapped to
    /// `QueryError::internal("query worker panicked")` without killing or poisoning worker threads.
    pub fn run_scoped<F, T>(
        &self,
        cancel: &CancelToken,
        tasks: Vec<F>,
    ) -> Result<Vec<T>, QueryError>
    where
        F: FnOnce(usize) -> Result<T, QueryError> + Send + 'static,
        T: Send + 'static,
    {
        if tasks.is_empty() {
            return Ok(Vec::new());
        }
        if cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }

        if IN_WORKER_POOL.with(Cell::get) {
            let mut out = Vec::with_capacity(tasks.len());
            for (idx, task) in tasks.into_iter().enumerate() {
                if cancel.is_cancelled() {
                    return Err(QueryError::Cancelled);
                }
                match catch_unwind(AssertUnwindSafe(|| task(idx))) {
                    Ok(Ok(val)) => out.push(val),
                    Ok(Err(err)) => {
                        cancel.cancel();
                        return Err(err);
                    }
                    Err(_) => {
                        cancel.cancel();
                        return Err(QueryError::internal("query worker panicked"));
                    }
                }
            }
            return Ok(out);
        }

        let total = tasks.len();
        let scope = Arc::new(ScopeShared::<T>::new(total));
        let mut queue = VecDeque::with_capacity(total);

        for (idx, task) in tasks.into_iter().enumerate() {
            let task_scope = Arc::clone(&scope);
            let task_cancel = cancel.clone();
            let boxed: TaskFn = Box::new(move |in_flight_guard| {
                run_scoped_task(idx, task, &task_cancel, &task_scope, in_flight_guard);
            });
            queue.push_back(boxed);
        }

        let query_id = {
            let mut inner = self.shared.lock();
            if inner.shutdown {
                return Err(QueryError::internal("worker pool is shut down"));
            }
            let qid = inner.next_query_id;
            inner.next_query_id = inner.next_query_id.wrapping_add(1).max(1);
            inner.queues.insert(qid, queue);
            inner.order.push_back(qid);
            qid
        };
        self.shared.cv.notify_all();

        let _cancel_guard = cancel.register_waker({
            let s = Arc::clone(&scope);
            move || {
                let _g = s.lock();
                s.cv.notify_all();
            }
        });

        let mut guard = scope.lock();
        loop {
            if !guard.drained_pool && (guard.first_err.is_some() || cancel.is_cancelled()) {
                cancel.cancel();
                guard.aborted = true;
                drop(guard);
                let drained = self.drain_query(query_id);
                guard = scope.lock();
                guard.drained_pool = true;
                guard.drained += drained;
            }
            if guard.finished + guard.drained >= total {
                break;
            }
            guard = scope.wait(guard);
        }

        if let Some(err) = guard.first_err.take() {
            let discard = std::mem::take(&mut guard.results);
            drop(guard);
            drop(discard);
            return Err(err);
        }
        if cancel.is_cancelled() || guard.aborted {
            let discard = std::mem::take(&mut guard.results);
            drop(guard);
            drop(discard);
            return Err(QueryError::Cancelled);
        }

        let mut out = Vec::with_capacity(total);
        for slot in guard.results.iter_mut() {
            match slot.take() {
                Some(val) => out.push(val),
                None => return Err(QueryError::internal("missing worker task result")),
            }
        }
        Ok(out)
    }

    fn drain_query(&self, query_id: u64) -> usize {
        let removed = {
            let mut inner = self.shared.lock();
            inner.order.retain(|&id| id != query_id);
            inner.queues.remove(&query_id)
        };
        removed.map_or(0, |q| q.len())
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        let drained = {
            let mut inner = self.shared.lock();
            inner.shutdown = true;
            inner.order.clear();
            std::mem::take(&mut inner.queues)
        };
        drop(drained);
        self.shared.cv.notify_all();
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

struct ScopeInner<T> {
    results: Vec<Option<T>>,
    first_err: Option<QueryError>,
    finished: usize,
    drained: usize,
    drained_pool: bool,
    aborted: bool,
}

impl<T> ScopeInner<T> {
    fn record_err(&mut self, err: QueryError) {
        let replace = match &self.first_err {
            None => true,
            Some(QueryError::Cancelled) => !matches!(err, QueryError::Cancelled),
            Some(_) => false,
        };
        if replace {
            self.first_err = Some(err);
        }
        self.aborted = true;
    }
}

struct ScopeShared<T> {
    inner: Mutex<ScopeInner<T>>,
    cv: Condvar,
}

impl<T> ScopeShared<T> {
    fn new(total: usize) -> Self {
        let mut results = Vec::with_capacity(total);
        results.resize_with(total, || None);
        Self {
            inner: Mutex::new(ScopeInner {
                results,
                first_err: None,
                finished: 0,
                drained: 0,
                drained_pool: false,
                aborted: false,
            }),
            cv: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, ScopeInner<T>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'a>(&self, guard: MutexGuard<'a, ScopeInner<T>>) -> MutexGuard<'a, ScopeInner<T>> {
        self.cv.wait(guard).unwrap_or_else(PoisonError::into_inner)
    }

    fn is_aborted(&self) -> bool {
        self.lock().aborted
    }
}

struct TaskCompletionGuard<'a, T> {
    scope: &'a Arc<ScopeShared<T>>,
    in_flight_guard: Option<InFlightGuard>,
    completed: bool,
}

impl<T> Drop for TaskCompletionGuard<'_, T> {
    fn drop(&mut self) {
        drop(self.in_flight_guard.take());
        if !self.completed {
            let mut guard = self.scope.lock();
            guard.record_err(QueryError::internal("query worker panicked"));
            guard.finished += 1;
            drop(guard);
            self.scope.cv.notify_all();
        }
    }
}

fn run_scoped_task<F, T>(
    idx: usize,
    task: F,
    cancel: &CancelToken,
    scope: &Arc<ScopeShared<T>>,
    in_flight_guard: InFlightGuard,
) where
    F: FnOnce(usize) -> Result<T, QueryError> + Send + 'static,
    T: Send + 'static,
{
    let mut completion = TaskCompletionGuard {
        scope,
        in_flight_guard: Some(in_flight_guard),
        completed: false,
    };

    enum Outcome<T> {
        Skipped,
        Ok(T),
        Err(QueryError),
    }

    let outcome = if cancel.is_cancelled() || scope.is_aborted() {
        let _ = catch_unwind(AssertUnwindSafe(|| drop(task)));
        Outcome::Skipped
    } else {
        match catch_unwind(AssertUnwindSafe(|| task(idx))) {
            Ok(Ok(val)) => Outcome::Ok(val),
            Ok(Err(err)) => {
                cancel.cancel();
                Outcome::Err(err)
            }
            Err(_) => {
                cancel.cancel();
                Outcome::Err(QueryError::internal("query worker panicked"))
            }
        }
    };

    // Release `in_flight_guard` BEFORE signaling `finished` so `in_flight_count() == 0`
    // is strictly visible when `run_scoped` wakes up and returns.
    drop(completion.in_flight_guard.take());

    let mut guard = scope.lock();
    match outcome {
        Outcome::Skipped => {}
        Outcome::Ok(val) => {
            if let Some(slot) = guard.results.get_mut(idx) {
                *slot = Some(val);
            }
        }
        Outcome::Err(err) => {
            guard.record_err(err);
        }
    }
    guard.finished += 1;
    completion.completed = true;
    drop(guard);
    scope.cv.notify_all();
}

fn worker_loop(shared: &Arc<PoolShared>) {
    loop {
        let next = {
            let mut inner = shared.lock();
            loop {
                if inner.shutdown {
                    return;
                }
                if let Some(qid) = inner.order.pop_front() {
                    if let Some(q) = inner.queues.get_mut(&qid) {
                        if let Some(task) = q.pop_front() {
                            if q.is_empty() {
                                inner.queues.remove(&qid);
                            } else {
                                // Fair round-robin: rotate this query to the back after popping 1 task.
                                inner.order.push_back(qid);
                            }
                            let guard = InFlightGuard::new(Arc::clone(&shared.in_flight));
                            break (task, guard);
                        }
                        inner.queues.remove(&qid);
                    }
                    continue;
                }
                inner = shared
                    .cv
                    .wait(inner)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };

        let (task, guard) = next;
        let _in_pool = InWorkerPoolGuard::enter();
        let _ = catch_unwind(AssertUnwindSafe(|| task(guard)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::time::{Duration, Instant};

    #[test]
    fn fair_round_robin_order_and_cancel_drains_all_workers() {
        let pool = Arc::new(WorkerPool::new(2).unwrap());
        assert_eq!(pool.num_workers(), 2);

        // Empty tasks returns immediately
        let cancel = CancelToken::new();
        let empty: Vec<fn(usize) -> Result<usize, QueryError>> = Vec::new();
        assert_eq!(
            pool.run_scoped(&cancel, empty).unwrap(),
            Vec::<usize>::new()
        );

        // Task ordering + 8 MiB stack check (recurse deeply enough to prove >2 MiB default stack)
        let stack_tasks: Vec<_> = (0..6)
            .map(|i| {
                move |idx: usize| -> Result<usize, QueryError> {
                    assert_eq!(idx, i);
                    // Touch ~3 MiB across recursive frames
                    fn touch_stack(depth: usize) -> usize {
                        let buf = [depth as u8; 64 * 1024];
                        if depth == 0 {
                            buf[0] as usize
                        } else {
                            (buf[0] as usize).wrapping_add(touch_stack(depth - 1))
                        }
                    }
                    let _ = touch_stack(48);
                    Ok(idx * 10 + 1)
                }
            })
            .collect();
        let results = pool.run_scoped(&cancel, stack_tasks).unwrap();
        assert_eq!(results, vec![1, 11, 21, 31, 41, 51]);
        assert_eq!(pool.in_flight_count(), 0);

        // Panic inside a worker maps to QueryError::Internal without poisoning the pool
        let panic_tasks: Vec<_> = (0..4)
            .map(|i| {
                move |_idx: usize| -> Result<usize, QueryError> {
                    if i == 2 {
                        panic!("intentional test panic");
                    }
                    Ok(i)
                }
            })
            .collect();
        let err = pool
            .run_scoped(&CancelToken::new(), panic_tasks)
            .unwrap_err();
        assert!(matches!(err, QueryError::Internal(_)));
        assert_eq!(pool.in_flight_count(), 0);

        // Pool still works normally after worker panic
        let after_panic: Vec<_> = (0..4).map(|i| move |_: usize| Ok(i + 100)).collect();
        assert_eq!(
            pool.run_scoped(&CancelToken::new(), after_panic).unwrap(),
            vec![100, 101, 102, 103]
        );

        // Fair round-robin across two concurrent queries on a 1-worker pool:
        // Query A enqueues 4 tasks, Query B enqueues 4 tasks while A's first task holds a barrier,
        // so after A's task 0 finishes, B gets interleaved rather than waiting for all of A.
        let single_pool = Arc::new(WorkerPool::new(1).unwrap());
        let trace_log = Arc::new(Mutex::new(Vec::<(&'static str, usize)>::new()));
        let a_started = Arc::new(Barrier::new(2));
        let b_queued = Arc::new(Barrier::new(2));

        let p_a = Arc::clone(&single_pool);
        let log_a = Arc::clone(&trace_log);
        let a_started_w = Arc::clone(&a_started);
        let b_queued_w = Arc::clone(&b_queued);
        let h_a = thread::spawn(move || {
            type UnitTask = Box<dyn FnOnce(usize) -> Result<(), QueryError> + Send>;
            let tasks: Vec<UnitTask> = (0..4)
                .map(|i| {
                    let log = Arc::clone(&log_a);
                    let started = Arc::clone(&a_started_w);
                    let queued = Arc::clone(&b_queued_w);
                    let b: UnitTask = Box::new(move |idx| {
                        if i == 0 {
                            started.wait();
                            queued.wait();
                        }
                        log.lock().unwrap().push(("A", idx));
                        Ok(())
                    });
                    b
                })
                .collect();
            p_a.run_scoped(&CancelToken::new(), tasks)
        });

        a_started.wait();
        let p_b = Arc::clone(&single_pool);
        let log_b = Arc::clone(&trace_log);
        let h_b = thread::spawn(move || {
            let tasks: Vec<_> = (0..4)
                .map(|_| {
                    let log = Arc::clone(&log_b);
                    move |idx: usize| {
                        log.lock().unwrap().push(("B", idx));
                        Ok(())
                    }
                })
                .collect();
            p_b.run_scoped(&CancelToken::new(), tasks)
        });
        // Give Query B a brief moment to enqueue before releasing A[0]
        while single_pool.shared.lock().queues.len() < 2 {
            thread::yield_now();
        }
        b_queued.wait();
        h_a.join().unwrap().unwrap();
        h_b.join().unwrap().unwrap();

        let seq: Vec<&'static str> = trace_log.lock().unwrap().iter().map(|(q, _)| *q).collect();
        // A0 runs while B is not yet queued, so A rotates to [A]; when B enqueues, order is [A, B],
        // producing exact round-robin interleaving: A0, A1, B0, A2, B1, A3, B2, B3.
        assert_eq!(seq, vec!["A", "A", "B", "A", "B", "A", "B", "B"]);
        drop(single_pool);

        // Mid-flight cancel drains all workers to in_flight == 0 cleanly and quickly
        let cancel_mid = CancelToken::new();
        let cancel_trigger = cancel_mid.clone();
        let started_flag = Arc::new(Barrier::new(3));
        let cancel_tasks: Vec<_> = (0..16)
            .map(|i| {
                let c = cancel_mid.clone();
                let b = Arc::clone(&started_flag);
                move |_idx: usize| -> Result<(), QueryError> {
                    if i < 2 {
                        b.wait();
                    }
                    while !c.is_cancelled() {
                        thread::sleep(Duration::from_micros(50));
                    }
                    Err(QueryError::Cancelled)
                }
            })
            .collect();

        let p_cancel = Arc::clone(&pool);
        let handle = thread::spawn(move || p_cancel.run_scoped(&cancel_mid, cancel_tasks));
        started_flag.wait();
        let t0 = Instant::now();
        cancel_trigger.cancel();
        let cancel_res = handle.join().unwrap();
        let elapsed = t0.elapsed();
        assert!(matches!(cancel_res, Err(QueryError::Cancelled)));
        assert_eq!(pool.in_flight_count(), 0);
        assert!(
            elapsed < Duration::from_millis(50),
            "cancel+drain took {elapsed:?}"
        );
    }

    #[test]
    fn run_scoped_drops_completed_reservations_before_returning_err() {
        use cetra_column::{GlobalPool, Reservation};

        let global_pool = GlobalPool::standalone(1024);
        let workers = WorkerPool::new(1).unwrap();
        let cancel = CancelToken::new();

        // With 1 worker, task 0 completes and stores Ok(Reservation) in guard.results
        // before task 1 runs and returns Err(TooLarge).
        let gp0 = Arc::clone(&global_pool);
        type ResTask = Box<dyn FnOnce(usize) -> Result<Reservation, QueryError> + Send>;
        let tasks: Vec<ResTask> = vec![
            Box::new(move |_| {
                let mut res = gp0.register("sibling_ok", false);
                res.try_grow(256)?;
                Ok(res)
            }),
            Box::new(|_| Err(QueryError::TooLarge)),
        ];

        let err = workers.run_scoped(&cancel, tasks).unwrap_err();
        assert!(matches!(err, QueryError::TooLarge));
        assert_eq!(global_pool.reserved(), 0);
    }

    #[test]
    fn run_scoped_prioritizes_non_cancelled_error_over_cancelled() {
        let workers = Arc::new(WorkerPool::new(2).unwrap());
        let cancel = CancelToken::new();
        let barrier = Arc::new(Barrier::new(2));

        let b0 = Arc::clone(&barrier);
        let c0 = cancel.clone();
        let b1 = Arc::clone(&barrier);
        let w1 = Arc::clone(&workers);
        type UnitTask = Box<dyn FnOnce(usize) -> Result<(), QueryError> + Send>;
        let tasks: Vec<UnitTask> = vec![
            Box::new(move |_| {
                b0.wait();
                c0.cancel();
                Err(QueryError::Cancelled)
            }),
            Box::new(move |_| {
                b1.wait();
                // Wait until sibling 0 finishes and drops its InFlightGuard so
                // Cancelled has a chance to be recorded first in guard.first_err.
                while w1.in_flight_count() > 1 {
                    thread::yield_now();
                }
                thread::sleep(Duration::from_millis(2));
                Err(QueryError::TooLarge)
            }),
        ];

        let err = workers.run_scoped(&cancel, tasks).unwrap_err();
        assert!(matches!(err, QueryError::TooLarge));
        assert_eq!(workers.in_flight_count(), 0);
    }

    #[test]
    fn reentrant_run_scoped_does_not_deadlock_and_propagates_cancel_error() {
        for num_workers in [1, 4] {
            let pool = Arc::new(WorkerPool::new(num_workers).unwrap());
            let cancel = CancelToken::new();

            let outer_tasks: Vec<_> = (0..3)
                .map(|outer_idx| {
                    let p = Arc::clone(&pool);
                    let c = cancel.clone();
                    move |_idx: usize| -> Result<Vec<usize>, QueryError> {
                        let inner_tasks: Vec<_> = (0..4)
                            .map(|inner_idx| {
                                move |idx: usize| -> Result<usize, QueryError> {
                                    assert_eq!(idx, inner_idx);
                                    Ok(outer_idx * 100 + inner_idx)
                                }
                            })
                            .collect();
                        p.run_scoped(&c, inner_tasks)
                    }
                })
                .collect();

            let res = pool.run_scoped(&cancel, outer_tasks).unwrap();
            assert_eq!(
                res,
                vec![
                    vec![0, 1, 2, 3],
                    vec![100, 101, 102, 103],
                    vec![200, 201, 202, 203],
                ]
            );
            assert_eq!(pool.in_flight_count(), 0);

            // Reentrant error cancels token and stops remaining sequential inner tasks
            let ran_after_err = Arc::new(AtomicUsize::new(0));
            let p_err = Arc::clone(&pool);
            let ran_flag = Arc::clone(&ran_after_err);
            let err = pool
                .run_scoped(
                    &CancelToken::new(),
                    vec![move |_| -> Result<(), QueryError> {
                        let c_inner = CancelToken::new();
                        let c_check = c_inner.clone();
                        let inner_err = p_err
                            .run_scoped(
                                &c_inner,
                                vec![
                                    Box::new(|_| Err(QueryError::TooLarge))
                                        as Box<dyn FnOnce(usize) -> Result<(), QueryError> + Send>,
                                    Box::new(move |_| {
                                        ran_flag.fetch_add(1, Ordering::SeqCst);
                                        Ok(())
                                    }),
                                ],
                            )
                            .unwrap_err();
                        assert!(c_check.is_cancelled());
                        Err(inner_err)
                    }],
                )
                .unwrap_err();
            assert!(matches!(err, QueryError::TooLarge));
            assert_eq!(ran_after_err.load(Ordering::SeqCst), 0);
            assert_eq!(pool.in_flight_count(), 0);
        }
    }

    #[test]
    fn external_cancel_wakes_run_scoped_immediately() {
        let pool = Arc::new(WorkerPool::new(2).unwrap());
        let cancel = CancelToken::new();
        let started = Arc::new(Barrier::new(2));

        let c_task = cancel.clone();
        let b_task = Arc::clone(&started);
        let p_run = Arc::clone(&pool);
        let c_run = cancel.clone();
        let handle = thread::spawn(move || {
            p_run.run_scoped(
                &c_run,
                vec![move |_| -> Result<(), QueryError> {
                    b_task.wait();
                    // Wait cleanly until cancelled via waker-driven sleep/park
                    let pair = Arc::new((Mutex::new(false), Condvar::new()));
                    let _g = c_task.register_waker({
                        let p = Arc::clone(&pair);
                        move || {
                            let mut done = p.0.lock().unwrap();
                            *done = true;
                            p.1.notify_all();
                        }
                    });
                    let mut done = pair.0.lock().unwrap();
                    while !*done && !c_task.is_cancelled() {
                        done = pair.1.wait(done).unwrap();
                    }
                    Err(QueryError::Cancelled)
                }],
            )
        });

        started.wait();
        let t0 = Instant::now();
        cancel.cancel();
        let res = handle.join().unwrap();
        let elapsed = t0.elapsed();
        assert!(matches!(res, Err(QueryError::Cancelled)));
        assert!(
            elapsed < Duration::from_millis(20),
            "cancel wakeup took {elapsed:?}"
        );
        assert_eq!(pool.in_flight_count(), 0);
    }
}
