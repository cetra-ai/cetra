//! Cooperative cancellation token.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Cooperative cancellation flag shared across I/O prefetchers and CPU workers.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    /// Creates an active (non-cancelled) token.
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Marks the token cancelled; idempotent and visible across threads immediately.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// True once [`Self::cancel`] has been called.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
