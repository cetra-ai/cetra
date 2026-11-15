//! What went wrong during a soak, recorded by every task (track R M7, R7.5).

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The kinds of failure a soak records. Each one fails a rule of R7.6.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub(crate) enum FailureKind {
    /// `append_spans` / `append_scores` returned anything but `Backpressure`.
    Append,
    /// A query returned `QueryError` or `RunError`.
    Query,
    /// `apply_retention` failed.
    Retention,
    /// `register_deletion` or `deletion_status` failed.
    Deletion,
    /// A covered trace was still visible after `register_deletion` returned.
    DeletionVisible,
}

impl FailureKind {
    /// The name used in the samples and in `failures.json`.
    pub(crate) fn name(self) -> String {
        format!("{self:?}")
    }
}

/// Shared by every task (`Arc<Failures>`). Keeps a count per kind and the first 20 messages.
#[derive(Debug, Default)]
pub(crate) struct Failures {
    inner: Mutex<BTreeMap<FailureKind, (u64, Vec<String>)>>,
}

impl Failures {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<FailureKind, (u64, Vec<String>)>> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `message` holds ids, kinds and the error's `Display`, never payload.
    pub(crate) fn record(&self, kind: FailureKind, message: String) {
        let mut inner = self.lock();
        let entry = inner.entry(kind).or_insert((0, Vec::new()));
        entry.0 += 1;
        if entry.1.len() < 20 {
            entry.1.push(message);
        }
    }

    pub(crate) fn counts(&self) -> BTreeMap<FailureKind, u64> {
        self.lock()
            .iter()
            .map(|(kind, (count, _))| (*kind, *count))
            .collect()
    }

    pub(crate) fn examples(&self) -> BTreeMap<FailureKind, Vec<String>> {
        self.lock()
            .iter()
            .map(|(kind, (_, examples))| (*kind, examples.clone()))
            .collect()
    }
}
