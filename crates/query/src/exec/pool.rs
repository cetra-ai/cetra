//! Process-wide bounded CPU worker pool with 8 MiB worker stacks and fair round-robin
//! scheduling across concurrent queries (`D-QE-5`, `design.md` § Contracts 3).

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use cetra_column::CancelToken;

use crate::error::QueryError;

/// Fixed OS stack size for every query worker thread (8 MiB, matching `cetra-cli`'s Tokio stack).
const WORKER_STACK_BYTES: usize = 8 * 1024 * 1024;
/// Polling interval when waiting on an active query whose `CancelToken` may be triggered externally.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_micros(200);

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
            if guard.aborted {
                guard = scope.wait(guard);
            } else {
                guard = scope.wait_timeout(guard, CANCEL_POLL_INTERVAL);
            }
        }

        if let Some(err) = guard.first_err.take() {
            return Err(err);
        }
        if cancel.is_cancelled() || guard.aborted {
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
        let mut inner = self.shared.lock();
        inner.order.retain(|&id| id != query_id);
        inner.queues.remove(&query_id).map_or(0, |q| q.len())
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        {
            let mut inner = self.shared.lock();
            inner.shutdown = true;
            inner.order.clear();
            inner.queues.clear();
        }
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

    fn wait_timeout<'a>(
        &self,
        guard: MutexGuard<'a, ScopeInner<T>>,
        dur: Duration,
    ) -> MutexGuard<'a, ScopeInner<T>> {
        let (next, _) = self
            .cv
            .wait_timeout(guard, dur)
            .unwrap_or_else(PoisonError::into_inner);
        next
    }

    fn is_aborted(&self) -> bool {
        self.lock().aborted
    }
}

struct TaskCompletionGuard<'a, T> {
    scope: &'a Arc<ScopeShared<T>>,
    completed: bool,
}

impl<T> Drop for TaskCompletionGuard<'_, T> {
    fn drop(&mut self) {
        if !self.completed {
            let mut guard = self.scope.lock();
            if guard.first_err.is_none() {
                guard.first_err = Some(QueryError::internal("query worker panicked"));
            }
            guard.aborted = true;
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
    drop(in_flight_guard);

    let mut guard = scope.lock();
    match outcome {
        Outcome::Skipped => {}
        Outcome::Ok(val) => {
            if let Some(slot) = guard.results.get_mut(idx) {
                *slot = Some(val);
            }
        }
        Outcome::Err(err) => {
            if guard.first_err.is_none() {
                guard.first_err = Some(err);
            }
            guard.aborted = true;
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
        let _ = catch_unwind(AssertUnwindSafe(|| task(guard)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::time::Instant;

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
}
