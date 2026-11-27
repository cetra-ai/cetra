//! Bounded `std::thread::scope` parallel map helpers with deterministic ordering.

use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::StatamError;

/// Maximum worker threads spawned by [`parallel_map_in_order`].
pub(crate) const MAX_PARALLEL_THREADS: usize = 32;

thread_local! {
    static IN_PARALLEL_SCOPE: Cell<bool> = const { Cell::new(false) };
}

/// Returns `true` if the calling thread is already inside a [`parallel_map_in_order`] scope.
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

/// Applies `f` across `items` using up to `threads.clamp(1, MAX_PARALLEL_THREADS)` worker threads,
/// returning outputs in input order or the first [`StatamError`] in input order.
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
}
