//! Synchronous hierarchical memory accounting (`GlobalPool`, `ScopedPool`, `Reservation`).

use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::{ColumnError, MemoryLimitKind};

/// Process-wide shared memory counter. `tokio`-free: callers that wait asynchronously
/// for admission pass a release hook (e.g. `tokio::sync::Notify::notify_waiters`).
pub struct GlobalPool {
    limit: usize,
    used: AtomicUsize,
    on_release: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl fmt::Debug for GlobalPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GlobalPool")
            .field("limit", &self.limit)
            .field("used", &self.used.load(Ordering::Acquire))
            .field("has_release_hook", &self.on_release.is_some())
            .finish()
    }
}

impl GlobalPool {
    /// Creates a shared pool with `limit` bytes and no release hook.
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicUsize::new(0),
            on_release: None,
        })
    }

    /// Creates a shared pool that invokes `on_release` whenever bytes return to it.
    pub fn with_release_hook(
        limit: usize,
        on_release: impl Fn() + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicUsize::new(0),
            on_release: Some(Arc::new(on_release)),
        })
    }

    /// Maximum bytes this pool allows.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Bytes currently held across all admitted scopes.
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    /// Takes `bytes` if `used + bytes <= limit`.
    fn try_take(&self, bytes: usize) -> bool {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let Some(next) = used.checked_add(bytes).filter(|next| *next <= self.limit) else {
                return false;
            };
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(actual) => used = actual,
            }
        }
    }

    /// Gives `bytes` back, saturating at zero, and invokes the release hook if any bytes returned.
    fn give_back(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let next = used.saturating_sub(bytes);
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break,
                Err(actual) => used = actual,
            }
        }
        if let Some(hook) = &self.on_release {
            hook();
        }
    }

    /// Atomically takes `initial` bytes and returns a [`ScopedPool`] capped at `cap`.
    /// Returns `None` if `initial > cap` or `used + initial > limit`.
    pub fn try_admit(self: &Arc<Self>, initial: usize, cap: usize) -> Option<Arc<ScopedPool>> {
        if initial > cap {
            return None;
        }
        if self.try_take(initial) {
            Some(ScopedPool::new(Arc::clone(self), initial, cap))
        } else {
            None
        }
    }

    /// Convenience for a standalone pool (e.g. `compaction.max_memory` or tests) with `initial = 0`.
    pub fn standalone(cap: usize) -> Arc<ScopedPool> {
        let global = Self::new(cap);
        ScopedPool::new(global, 0, cap)
    }
}

/// Internal mutable state of a [`ScopedPool`].
#[derive(Debug, Default)]
struct PoolState {
    used: usize,
    unspillable: usize,
    spillable: usize,
    spillable_consumers: usize,
    held: usize,
}

impl PoolState {
    fn add(&mut self, spills: bool, bytes: usize) {
        if spills {
            self.spillable = self.spillable.saturating_add(bytes);
        } else {
            self.unspillable = self.unspillable.saturating_add(bytes);
        }
    }

    fn sub(&mut self, spills: bool, bytes: usize) {
        if spills {
            self.spillable = self.spillable.saturating_sub(bytes);
        } else {
            self.unspillable = self.unspillable.saturating_sub(bytes);
        }
    }
}

/// One query's (or compaction job's) memory pool over a [`GlobalPool`], capped at `cap`.
#[derive(Debug)]
pub struct ScopedPool {
    shared: Arc<GlobalPool>,
    initial: usize,
    cap: usize,
    state: Mutex<PoolState>,
}

impl ScopedPool {
    pub(crate) fn new(shared: Arc<GlobalPool>, initial: usize, cap: usize) -> Arc<Self> {
        Arc::new(Self {
            shared,
            initial,
            cap,
            state: Mutex::new(PoolState {
                held: initial,
                ..PoolState::default()
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Per-scope byte cap.
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Bytes currently reserved across active [`Reservation`]s in this scope.
    pub fn reserved(&self) -> usize {
        self.lock().used
    }

    /// Current fair share in bytes for a single spillable consumer.
    pub fn spill_share(&self) -> usize {
        let state = self.lock();
        let remaining = self.cap.saturating_sub(state.unspillable);
        remaining / state.spillable_consumers.max(1)
    }

    /// Registers a new consumer and returns a zero-byte [`Reservation`].
    pub fn register(self: &Arc<Self>, name: &'static str, can_spill: bool) -> Reservation {
        if can_spill {
            let mut state = self.lock();
            state.spillable_consumers += 1;
        }
        Reservation {
            pool: Arc::clone(self),
            name,
            can_spill,
            size: 0,
        }
    }
}

impl Drop for ScopedPool {
    fn drop(&mut self) {
        let held = self
            .state
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .held;
        self.shared.give_back(held);
    }
}

/// Guard for one operator's reserved bytes; releases bytes and unregisters on `Drop`.
#[derive(Debug)]
pub struct Reservation {
    pool: Arc<ScopedPool>,
    name: &'static str,
    can_spill: bool,
    size: usize,
}

impl Reservation {
    /// Static operator name (diagnostics only).
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// True when this consumer participates in `FairSpill` sharing.
    pub fn can_spill(&self) -> bool {
        self.can_spill
    }

    /// Bytes currently held by this reservation.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Attempts to reserve `additional` bytes, enforcing `FairSpill`, `cap`, and `GlobalPool`.
    pub fn try_grow(&mut self, additional: usize) -> Result<(), ColumnError> {
        if additional == 0 {
            return Ok(());
        }

        let mut guard = self.pool.lock();
        let state = &mut *guard;

        if self.can_spill {
            let remaining = self.pool.cap.saturating_sub(state.unspillable);
            let share = remaining / state.spillable_consumers.max(1);
            if self
                .size
                .checked_add(additional)
                .is_none_or(|after| after > share)
            {
                return Err(ColumnError::MemoryLimit {
                    kind: MemoryLimitKind::SpillShare,
                    requested: self.size.saturating_add(additional),
                    limit: share,
                });
            }
        }

        let Some(next) = state.used.checked_add(additional) else {
            return Err(ColumnError::MemoryLimit {
                kind: MemoryLimitKind::QueryCap,
                requested: usize::MAX,
                limit: self.pool.cap,
            });
        };

        if next > self.pool.cap {
            return Err(ColumnError::MemoryLimit {
                kind: MemoryLimitKind::QueryCap,
                requested: next,
                limit: self.pool.cap,
            });
        }

        let delta = next.saturating_sub(state.held);

        if delta > 0 && !self.pool.shared.try_take(delta) {
            return Err(ColumnError::MemoryLimit {
                kind: MemoryLimitKind::GlobalPool,
                requested: delta,
                limit: self.pool.shared.limit(),
            });
        }

        state.held = state.held.max(next);
        state.used = next;
        state.add(self.can_spill, additional);
        self.size = self.size.saturating_add(additional);

        Ok(())
    }

    /// Releases `bytes.min(self.size)` bytes, returning surplus above `initial` to `GlobalPool`.
    pub fn shrink(&mut self, bytes: usize) {
        let actual = bytes.min(self.size);
        if actual == 0 {
            return;
        }

        let freed = {
            let mut guard = self.pool.lock();
            let state = &mut *guard;

            state.sub(self.can_spill, actual);
            state.used = state.used.saturating_sub(actual);
            self.size -= actual;

            let keep = state.used.max(self.pool.initial);
            let freed = state.held.saturating_sub(keep);
            state.held -= freed;
            freed
        };

        if freed > 0 {
            self.pool.shared.give_back(freed);
        }
    }

    /// Releases all bytes currently held by this reservation.
    pub fn free(&mut self) {
        self.shrink(self.size);
    }

    /// Grows or shrinks this reservation to exactly `new_size` bytes.
    pub fn resize(&mut self, new_size: usize) -> Result<(), ColumnError> {
        if new_size > self.size {
            self.try_grow(new_size - self.size)
        } else {
            self.shrink(self.size - new_size);
            Ok(())
        }
    }

    /// Splits off a new independent reservation (same `can_spill`) starting at 0 bytes.
    pub fn new_empty(&self, name: &'static str) -> Self {
        self.pool.register(name, self.can_spill)
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let freed = {
            let mut guard = self.pool.lock();
            let state = &mut *guard;

            state.sub(self.can_spill, self.size);
            state.used = state.used.saturating_sub(self.size);
            if self.can_spill {
                state.spillable_consumers = state.spillable_consumers.saturating_sub(1);
            }

            let keep = state.used.max(self.pool.initial);
            let freed = state.held.saturating_sub(keep);
            state.held -= freed;
            freed
        };

        if freed > 0 {
            self.pool.shared.give_back(freed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn admission_respects_global_limit_and_fires_release_hook() {
        let released_count = Arc::new(AtomicUsize::new(0));
        let rc = Arc::clone(&released_count);
        let global = GlobalPool::with_release_hook(100, move || {
            rc.fetch_add(1, Ordering::SeqCst);
        });

        let scope1 = global.try_admit(60, 100).expect("admission should succeed");
        assert_eq!(global.used(), 60);

        let scope2 = global.try_admit(50, 100);
        assert!(scope2.is_none());

        drop(scope1);
        assert_eq!(global.used(), 0);
        assert_eq!(released_count.load(Ordering::SeqCst), 1);

        let scope3 = global
            .try_admit(50, 100)
            .expect("admission should succeed now");
        assert_eq!(global.used(), 50);
        drop(scope3);
        assert_eq!(global.used(), 0);
    }

    #[test]
    fn initial_bytes_stay_held_until_scope_drops() {
        let global = GlobalPool::new(1000);
        {
            let scope = global.try_admit(100, 500).expect("admit");
            assert_eq!(global.used(), 100);

            let mut res = scope.register("op", true);
            res.try_grow(200).expect("grow");
            assert_eq!(global.used(), 200);
            assert_eq!(scope.reserved(), 200);

            res.shrink(200);
            assert_eq!(scope.reserved(), 0);
            assert_eq!(global.used(), 100);
        }
        assert_eq!(global.used(), 0);
    }

    #[test]
    fn shrink_past_self_size_does_not_corrupt_sibling_reservations() {
        let scope = GlobalPool::standalone(1000);
        let mut a = scope.register("a", true);
        let mut b = scope.register("b", true);

        a.try_grow(100).expect("grow a");
        b.try_grow(200).expect("grow b");
        assert_eq!(scope.reserved(), 300);

        a.shrink(500);
        assert_eq!(a.size(), 0);
        assert_eq!(b.size(), 200);
        assert_eq!(scope.reserved(), 200);
    }

    #[test]
    fn release_hook_may_read_scoped_pool_without_deadlock() {
        use std::sync::{OnceLock, Weak};
        let slot: Arc<OnceLock<Weak<ScopedPool>>> = Arc::new(OnceLock::new());
        let slot_hook = Arc::clone(&slot);
        let global = GlobalPool::with_release_hook(1000, move || {
            if let Some(weak) = slot_hook.get()
                && let Some(scope) = weak.upgrade()
            {
                let _res = scope.reserved();
            }
        });
        let scope = global.try_admit(50, 500).expect("admit");
        slot.set(Arc::downgrade(&scope)).expect("set weak");
        let mut res = scope.register("op", true);
        res.try_grow(100).expect("grow");
        res.shrink(50);
        drop(res);
        drop(scope);
    }

    #[test]
    fn fair_spill_divides_remaining_after_unspillable() {
        let scope = GlobalPool::standalone(300);
        let mut unspillable = scope.register("unspillable", false);
        unspillable.try_grow(100).expect("grow unspillable");

        let mut spill1 = scope.register("spill1", true);
        let mut spill2 = scope.register("spill2", true);

        assert_eq!(scope.spill_share(), 100);

        spill1.try_grow(100).expect("spill1 grow");
        let err = spill1.try_grow(1).unwrap_err();
        match err {
            ColumnError::MemoryLimit {
                kind,
                requested,
                limit,
            } => {
                assert_eq!(kind, MemoryLimitKind::SpillShare);
                assert_eq!(limit, 100);
                assert!(requested > 100);
            }
            other => panic!("unexpected error: {other:?}"),
        }

        spill2.try_grow(50).expect("spill2 grow");
    }

    #[test]
    fn global_pool_exhaustion_leaves_scope_state_unchanged() {
        let global = GlobalPool::new(150);
        let scope = global.try_admit(0, 500).expect("admit");
        let mut res = scope.register("op", true);

        let err = res.try_grow(160).unwrap_err();
        match err {
            ColumnError::MemoryLimit { kind, .. } => {
                assert_eq!(kind, MemoryLimitKind::GlobalPool);
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(res.size(), 0);
        assert_eq!(scope.reserved(), 0);
        assert_eq!(global.used(), 0);
    }

    #[test]
    fn concurrent_reservations_return_to_zero() {
        let global = GlobalPool::new(100_000);
        let mut handles = vec![];
        for _ in 0..8 {
            let g = Arc::clone(&global);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    if let Some(scope) = g.try_admit(10, 1000) {
                        let mut res = scope.register("worker", true);
                        if res.try_grow(500).is_ok() {
                            res.shrink(250);
                        }
                    }
                }
            }));
        }
        for h in handles {
            h.join().expect("join");
        }
        assert_eq!(global.used(), 0);
    }
}
