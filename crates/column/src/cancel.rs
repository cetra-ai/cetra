//! Cooperative cancellation token with lock-free polling and wakeup hooks.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

type WakerFn = Arc<dyn Fn() + Send + Sync>;

struct CancelInner {
    cancelled: AtomicBool,
    listeners: Mutex<Vec<(u64, WakerFn)>>,
    next_id: AtomicU64,
}

impl fmt::Debug for CancelInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancelInner")
            .field("cancelled", &self.cancelled.load(Ordering::Relaxed))
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
        let mut guard = self
            .inner
            .listeners
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        guard.retain(|(id, _)| *id != self.id);
    }
}

impl CancelToken {
    /// Creates an active (non-cancelled) token.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(CancelInner {
                cancelled: AtomicBool::new(false),
                listeners: Mutex::new(Vec::new()),
                next_id: AtomicU64::new(0),
            }),
        }
    }

    /// Marks the token cancelled; idempotent and visible across threads immediately.
    /// Invokes all currently registered wakers outside the internal listener lock.
    pub fn cancel(&self) {
        let wakers = {
            let mut guard = self
                .inner
                .listeners
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if self.inner.cancelled.swap(true, Ordering::Release) {
                return;
            }
            std::mem::take(&mut *guard)
        };
        for (_, waker) in wakers {
            waker();
        }
    }

    /// True once [`Self::cancel`] has been called (lock-free).
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Relaxed)
    }

    /// Registers a wakeup callback invoked when [`Self::cancel`] is called (or immediately
    /// if already cancelled). Dropping the returned [`CancelGuard`] deregisters the callback.
    pub fn register_waker(&self, waker: impl Fn() + Send + Sync + 'static) -> CancelGuard {
        let waker: WakerFn = Arc::new(waker);
        let id = {
            let mut guard = self
                .inner
                .listeners
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if self.inner.cancelled.load(Ordering::Acquire) {
                u64::MAX
            } else {
                let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
                guard.push((id, Arc::clone(&waker)));
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
}
