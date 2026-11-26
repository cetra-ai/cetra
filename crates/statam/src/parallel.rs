//! Bounded `std::thread::scope` parallel map helpers with deterministic ordering.

use std::sync::Mutex;

use crate::StatamError;

/// Maximum worker threads spawned by [`parallel_map_in_order`].
pub(crate) const MAX_WRITE_THREADS: usize = 32;

/// Applies `f` across `items` using up to `threads.clamp(1, MAX_WRITE_THREADS)` worker threads,
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
/// `threads.clamp(1, MAX_WRITE_THREADS)` threads, returning outputs or the first error in input order.
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
    let threads = threads.clamp(1, MAX_WRITE_THREADS);
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
    let run_worker = || -> Vec<(usize, Result<R, StatamError>)> {
        let mut local = Vec::new();
        let mut state: Option<S> = None;
        loop {
            let next = {
                let mut guard = queue.lock().unwrap_or_else(|p| p.into_inner());
                guard.next()
            };
            let Some((idx, item)) = next else {
                break;
            };
            let st = match state.as_mut() {
                Some(st) => st,
                None => match init() {
                    Ok(s) => state.insert(s),
                    Err(e) => {
                        local.push((idx, Err(e)));
                        continue;
                    }
                },
            };
            local.push((idx, f(st, item)));
        }
        local
    };

    let mut slots: Vec<Option<Result<R, StatamError>>> = Vec::with_capacity(len);
    slots.resize_with(len, || None);

    let mut thread_panicked = false;
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers - 1);
        for _ in 1..workers {
            handles.push(scope.spawn(run_worker));
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
}
