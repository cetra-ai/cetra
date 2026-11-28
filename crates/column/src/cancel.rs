//! Cooperative cancellation token with lock-free polling and wakeup hooks.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

type WakerFn = Arc<dyn Fn() + Send + Sync>;

struct CancelState {
    next_id: u64,
    listeners: Vec<(u64, WakerFn)>,
}

struct CancelInner {
    cancelled: AtomicBool,
    state: Mutex<CancelState>,
}

impl fmt::Debug for CancelInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancelInner")
            .field("cancelled", &self.cancelled.load(Ordering::Acquire))
            .finish()
    }
}

/// Cooperative cancellation flag shared across I/O prefetchers and CPU workers.
#[derive(Debug, Clone)]
pub struct CancelToken {
    inner: Arc<CancelInner>,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

/// Guard that deregisters a wakeup callback from [`CancelToken`] on drop.
#[must_use = "dropping CancelGuard immediately deregisters the waker"]
pub struct CancelGuard {
    inner: Arc<CancelInner>,
    id: u64,
}

impl fmt::Debug for CancelGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancelGuard").field("id", &self.id).finish()
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if self.id == u64::MAX {
            return;
        }
        let removed = {
            let mut guard = self
                .inner
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            guard
                .listeners
                .iter()
                .position(|(id, _)| *id == self.id)
                .map(|idx| guard.listeners.remove(idx))
        };
        // Drop the captured waker closure outside the lock so a Drop impl that
        // calls `token.cancel()` or drops another `CancelGuard` never deadlocks.
        drop(removed);
    }
}

impl CancelToken {
    /// Creates an active (non-cancelled) token.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(CancelInner {
                cancelled: AtomicBool::new(false),
                state: Mutex::new(CancelState {
                    next_id: 0,
                    listeners: Vec::new(),
                }),
            }),
        }
    }

    /// Marks the token cancelled; idempotent and visible across threads immediately.
    /// Invokes all currently registered wakers outside the internal listener lock.
    pub fn cancel(&self) {
        let wakers = {
            let mut guard = self
                .inner
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if self.inner.cancelled.swap(true, Ordering::Release) {
                return;
            }
            std::mem::take(&mut guard.listeners)
        };
        for (_, waker) in wakers {
            waker();
        }
    }

    /// True once [`Self::cancel`] has been called (lock-free).
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    /// Registers a wakeup callback invoked when [`Self::cancel`] is called (or immediately
    /// if already cancelled). Dropping the returned [`CancelGuard`] deregisters the callback.
    pub fn register_waker(&self, waker: impl Fn() + Send + Sync + 'static) -> CancelGuard {
        let waker: WakerFn = Arc::new(waker);
        let id = {
            let mut guard = self
                .inner
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if self.inner.cancelled.load(Ordering::Acquire) {
                u64::MAX
            } else {
                let id = guard.next_id;
                guard.next_id += 1;
                guard.listeners.push((id, Arc::clone(&waker)));
                id
            }
        };
        if id == u64::MAX {
            waker();
        }
        CancelGuard {
            inner: Arc::clone(&self.inner),
            id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    #[test]
    fn clone_observes_cancel_across_threads() {
        let token = CancelToken::new();
        let token_clone = token.clone();
        assert!(!token.is_cancelled());
        assert!(!token_clone.is_cancelled());

        let handle = thread::spawn(move || {
            token_clone.cancel();
        });
        handle.join().expect("thread join failed");

        assert!(token.is_cancelled());
    }

    #[test]
    fn waker_fires_on_cancel_and_when_registered_after_cancel() {
        let token = CancelToken::new();
        let hits = Arc::new(AtomicUsize::new(0));

        let hits_before = Arc::clone(&hits);
        let _guard1 = token.register_waker(move || {
            hits_before.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(hits.load(Ordering::SeqCst), 0);

        token.cancel();
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // Idempotent second cancel does not re-fire already drained waker.
        token.cancel();
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // Registering after cancel fires immediately.
        let hits_after = Arc::clone(&hits);
        let _guard2 = token.register_waker(move || {
            hits_after.fetch_add(10, Ordering::SeqCst);
        });
        assert_eq!(hits.load(Ordering::SeqCst), 11);
    }

    #[test]
    fn dropping_guard_before_cancel_deregisters_waker() {
        let token = CancelToken::new();
        let hits = Arc::new(AtomicUsize::new(0));

        let hits_dropped = Arc::clone(&hits);
        let guard = token.register_waker(move || {
            hits_dropped.fetch_add(1, Ordering::SeqCst);
        });
        drop(guard);

        token.cancel();
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn concurrent_cancel_and_register_never_misses_wakeup() {
        for _ in 0..200 {
            let token = CancelToken::new();
            let hits = Arc::new(AtomicUsize::new(0));

            let t_cancel = token.clone();
            let h1 = thread::spawn(move || {
                t_cancel.cancel();
            });

            let hits_reg = Arc::clone(&hits);
            let t_reg = token.clone();
            let h2 = thread::spawn(move || {
                t_reg.register_waker(move || {
                    hits_reg.fetch_add(1, Ordering::SeqCst);
                })
            });

            h1.join().expect("join cancel");
            let _guard = h2.join().expect("join register");
            assert_eq!(hits.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn dropping_guard_whose_waker_reenters_token_does_not_deadlock() {
        struct ReentrantDrop {
            inner_guard: Option<CancelGuard>,
            token: CancelToken,
            dropped: Arc<AtomicUsize>,
        }

        impl Drop for ReentrantDrop {
            fn drop(&mut self) {
                drop(self.inner_guard.take());
                self.token.cancel();
                self.dropped.fetch_add(1, Ordering::SeqCst);
            }
        }

        let token = CancelToken::new();
        let inner_guard = token.register_waker(|| {});
        let dropped = Arc::new(AtomicUsize::new(0));
        let reentrant = ReentrantDrop {
            inner_guard: Some(inner_guard),
            token: token.clone(),
            dropped: Arc::clone(&dropped),
        };
        let outer_guard = token.register_waker(move || {
            let _keep = &reentrant;
        });

        drop(outer_guard);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(token.is_cancelled());
    }
}
