//! Monotonic counters of the storage since open (N M3, D-N-31), read by
//! [`crate::Storage::stats`] for `/metrics`.
//!
//! The counters are plain `AtomicU64`s bumped with `Relaxed` ordering: nothing orders
//! on them. They hang on the [`crate::catalog::Committer`], which flush, compaction and
//! GC already share.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::StorageStats;
use crate::wal::Table;

/// How one compaction job, or one rewrite, ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionOutcome {
    /// Committed.
    Ok,
    /// Failed for a reason other than the two below.
    Failed,
    /// Deferred for lack of free disk; a later pass retries it.
    Deferred,
    /// Lost a commit conflict; a later pass redoes it.
    Conflict,
}

/// The counters of [`StorageStats`]. One field per scalar, and one array per per-table
/// value (index 0 = spans, 1 = scores).
#[derive(Debug, Default)]
pub(crate) struct Counters {
    flushes_ok: [AtomicU64; 2],
    flushes_failed: [AtomicU64; 2],
    flush_nanos: [AtomicU64; 2],
    compactions_ok: AtomicU64,
    compactions_failed: AtomicU64,
    compactions_deferred: AtomicU64,
    compactions_conflict: AtomicU64,
    compaction_bytes_written: AtomicU64,
    gc_objects_deleted: AtomicU64,
}

/// The array index of `table`: 0 for spans, 1 for scores.
fn index(table: Table) -> usize {
    match table {
        Table::Spans => 0,
        Table::Scores => 1,
    }
}

fn add(counter: &AtomicU64, amount: u64) {
    counter.fetch_add(amount, Ordering::Relaxed);
}

impl Counters {
    /// One committed flush of `table` that took `nanos`.
    pub(crate) fn flush_ok(&self, table: Table, nanos: u64) {
        let index = index(table);
        add(&self.flushes_ok[index], 1);
        add(&self.flush_nanos[index], nanos);
    }

    /// One failed flush attempt of `table`.
    pub(crate) fn flush_failed(&self, table: Table) {
        add(&self.flushes_failed[index(table)], 1);
    }

    /// `jobs` compaction jobs that ended as `outcome`.
    pub(crate) fn compaction(&self, outcome: CompactionOutcome, jobs: u64) {
        let counter = match outcome {
            CompactionOutcome::Ok => &self.compactions_ok,
            CompactionOutcome::Failed => &self.compactions_failed,
            CompactionOutcome::Deferred => &self.compactions_deferred,
            CompactionOutcome::Conflict => &self.compactions_conflict,
        };
        add(counter, jobs);
    }

    /// Bytes of Parquet objects written by committed compaction jobs.
    pub(crate) fn compaction_bytes(&self, bytes: u64) {
        add(&self.compaction_bytes_written, bytes);
    }

    /// Obsolete files and staged objects GC deleted.
    pub(crate) fn gc_deleted(&self, objects: u64) {
        add(&self.gc_objects_deleted, objects);
    }

    /// The current values. `wal_bytes` is zero: the caller measures the WAL.
    pub(crate) fn snapshot(&self) -> StorageStats {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        StorageStats {
            wal_bytes: [0; 2],
            flushes_ok: std::array::from_fn(|index| load(&self.flushes_ok[index])),
            flushes_failed: std::array::from_fn(|index| load(&self.flushes_failed[index])),
            flush_nanos: std::array::from_fn(|index| load(&self.flush_nanos[index])),
            compactions_ok: load(&self.compactions_ok),
            compactions_failed: load(&self.compactions_failed),
            compactions_deferred: load(&self.compactions_deferred),
            compactions_conflict: load(&self.compactions_conflict),
            compaction_bytes_written: load(&self.compaction_bytes_written),
            gc_objects_deleted: load(&self.gc_objects_deleted),
        }
    }
}
