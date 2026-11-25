//! Async query memory admission pool wrapping `cetra_column::GlobalPool`.

use std::sync::Arc;
use std::time::Duration;

use cetra_column::{GlobalPool, ScopedPool};
use cetra_query::QueryError;
use tokio::sync::Notify;

/// Process-wide async admission pool backed by [`GlobalPool`] and [`Notify`].
#[derive(Debug, Clone)]
pub(crate) struct AsyncAdmissionPool {
    pool: Arc<GlobalPool>,
    freed: Arc<Notify>,
}

impl AsyncAdmissionPool {
    /// Creates an admission pool capped at `limit` bytes.
    pub(crate) fn new(limit: usize) -> Self {
        let freed = Arc::new(Notify::new());
        let hook_notify = Arc::clone(&freed);
        let pool = GlobalPool::with_release_hook(limit, move || {
            hook_notify.notify_waiters();
        });
        Self { pool, freed }
    }

    /// Underlying synchronous global pool.
    pub(crate) fn pool(&self) -> &Arc<GlobalPool> {
        &self.pool
    }

    /// Bytes currently held across all admitted scopes.
    pub(crate) fn used(&self) -> usize {
        self.pool.used()
    }

    /// Maximum bytes allowed across all queries.
    pub(crate) fn limit(&self) -> usize {
        self.pool.limit()
    }

    /// Admits a query with `initial` bytes and per-query `cap`, waiting up to `wait`
    /// before failing with [`QueryError::Busy`].
    pub(crate) async fn admit(
        &self,
        initial: usize,
        cap: usize,
        wait: Duration,
    ) -> Result<Arc<ScopedPool>, QueryError> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            // Register for wake-up BEFORE checking `try_admit` so a release between
            // the check and the await cannot be missed.
            let notified = self.freed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(scoped) = self.pool.try_admit(initial, cap) {
                return Ok(scoped);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Err(QueryError::Busy);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1024 * 1024;

    #[tokio::test]
    async fn admit_waits_for_release_and_times_out_with_busy() {
        let pool = AsyncAdmissionPool::new(2 * MIB);
        assert_eq!(pool.limit(), 2 * MIB);
        assert_eq!(pool.used(), 0);

        let first = pool
            .admit(2 * MIB, 2 * MIB, Duration::from_millis(100))
            .await
            .expect("first admission fits");
        assert_eq!(pool.used(), 2 * MIB);

        let err = pool
            .admit(2 * MIB, 2 * MIB, Duration::from_millis(30))
            .await
            .unwrap_err();
        assert!(
            matches!(err, QueryError::Busy),
            "expected Busy, got {err:?}"
        );

        let waiting_pool = pool.clone();
        let waiter = tokio::spawn(async move {
            waiting_pool
                .admit(2 * MIB, 2 * MIB, Duration::from_secs(5))
                .await
        });

        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(first);

        let second = waiter
            .await
            .expect("waiter task should not panic")
            .expect("waiter should be admitted after release");
        assert_eq!(pool.used(), 2 * MIB);
        drop(second);
        assert_eq!(pool.used(), 0);
    }
}
