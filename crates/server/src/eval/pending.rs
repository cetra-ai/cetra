//! The traces the online-rule trigger waits on (D-I-8). Memory only: `commit` updates it, the
//! sweeper reads and removes completed traces, and the flusher persists the changes.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, PoisonError};

use llmobs_core::ids::{ProjectId, TraceId};
use llmobs_core::names::{environment, metadata_key};
use llmobs_core::span::Span;

/// Most traces held at once (D-I-8).
pub(crate) const MAX_PENDING: usize = 200_000;
/// A trace whose root ended is complete after this long without a new span (D-I-7).
pub(crate) const ROOT_IDLE_NS: i64 = 5_000_000_000;
/// Failed sweeps after which a trace is dropped (D-I-9).
// I4.7 calls it.
#[allow(dead_code)]
pub(crate) const MAX_FAILED_READS: u8 = 5;
/// The full-map warning is logged at most this often.
const WARN_EVERY_NS: i64 = 60_000_000_000;

/// One pending trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entry {
    /// Ingest time of the first span seen.
    pub(crate) first_seen: i64,
    /// Ingest time of the last span seen.
    pub(crate) last_seen: i64,
    /// Earliest span `start_time` seen.
    pub(crate) min_start: i64,
    /// A span without a parent was seen.
    pub(crate) root_ended: bool,
    /// Sweeps whose reads failed (memory only).
    pub(crate) failed_reads: u8,
    /// Spans observed, wrapping (memory only, starts at 0 after a load). `finish` compares it
    /// with `last_seen`: a span at an unchanged clock still counts as growth.
    pub(crate) spans: u32,
}

/// A completed trace, as the sweeper saw it.
// I4.7 calls it.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Completed {
    pub(crate) trace_id: TraceId,
    pub(crate) last_seen: i64,
    pub(crate) min_start: i64,
    pub(crate) spans: u32,
}

/// What the flusher writes: entries changed since the last flush, and keys removed since.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Changes {
    pub(crate) upserts: Vec<(ProjectId, TraceId, Entry)>,
    pub(crate) deletes: Vec<(ProjectId, TraceId)>,
}

/// The in-memory map of pending traces (D-I-8). Every method takes the lock once, briefly.
pub(crate) struct PendingTraces {
    max: usize,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    tracked: HashSet<ProjectId>,
    projects: HashMap<ProjectId, HashMap<TraceId, Entry>>,
    len: usize,
    changed: HashSet<(ProjectId, TraceId)>,
    removed: HashSet<(ProjectId, TraceId)>,
    dropped_full: u64,
    warned_at: Option<i64>,
}

impl Default for PendingTraces {
    fn default() -> Self {
        Self::new(MAX_PENDING)
    }
}

impl std::fmt::Debug for PendingTraces {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = lock(&self.inner);
        formatter
            .debug_struct("PendingTraces")
            .field("len", &inner.len)
            .field("tracked", &inner.tracked.len())
            .finish_non_exhaustive()
    }
}

/// Locks the map. A poisoned lock still guards consistent data: every change is a few inserts
/// or removals.
fn lock(mutex: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl PendingTraces {
    /// An empty map holding at most `max` traces (tests pass small values).
    pub(crate) fn new(max: usize) -> Self {
        Self {
            max,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Records the spans of one committed batch of `project`, when the project is tracked (D-I-8).
    /// Evaluation and experiment spans are skipped (D-I-17). `now` is the ingest time.
    pub(crate) fn observe(&self, project: ProjectId, spans: &[Span], now: i64) {
        let mut warning = None;
        {
            let mut guard = lock(&self.inner);
            let inner = &mut *guard;
            if !inner.tracked.contains(&project) {
                return;
            }
            let mut dropped_here = false;
            for span in spans {
                if span.environment.as_deref() == Some(environment::EVAL)
                    || span.metadata.contains_key(metadata_key::EXPERIMENT_ID)
                {
                    continue;
                }
                let root = span.parent_span_id.is_none();
                let traces = inner.projects.entry(project).or_default();
                match traces.get_mut(&span.trace_id) {
                    Some(entry) => {
                        // Concurrent commits may pass older clocks: `last_seen` never moves back.
                        entry.last_seen = entry.last_seen.max(now);
                        entry.min_start = entry.min_start.min(span.start_time);
                        entry.root_ended |= root;
                        entry.spans = entry.spans.wrapping_add(1);
                        entry.failed_reads = 0;
                    }
                    None if inner.len < self.max => {
                        traces.insert(
                            span.trace_id,
                            Entry {
                                first_seen: now,
                                last_seen: now,
                                min_start: span.start_time,
                                root_ended: root,
                                failed_reads: 0,
                                spans: 1,
                            },
                        );
                        inner.len += 1;
                    }
                    None => {
                        inner.dropped_full += 1;
                        dropped_here = true;
                        continue;
                    }
                }
                let key = (project, span.trace_id);
                inner.removed.remove(&key);
                inner.changed.insert(key);
            }
            if dropped_here
                && inner
                    .warned_at
                    .is_none_or(|at| now.saturating_sub(at) >= WARN_EVERY_NS)
            {
                inner.warned_at = Some(now);
                warning = Some((inner.dropped_full, inner.len));
            }
        }
        // Logged after the guard is dropped; counts only, never trace content.
        if let Some((dropped, pending)) = warning {
            tracing::warn!(
                dropped,
                pending,
                "online-rule trigger map is full; new traces are not evaluated"
            );
        }
    }

    /// Replaces the tracked set. Every entry of a project that is no longer tracked is removed.
    pub(crate) fn set_tracked(&self, projects: HashSet<ProjectId>) {
        let mut guard = lock(&self.inner);
        let inner = &mut *guard;
        let untracked: Vec<ProjectId> = inner
            .projects
            .keys()
            .filter(|project| !projects.contains(project))
            .copied()
            .collect();
        for project in untracked {
            if let Some(traces) = inner.projects.remove(&project) {
                inner.len -= traces.len();
                for trace in traces.into_keys() {
                    let key = (project, trace);
                    inner.changed.remove(&key);
                    inner.removed.insert(key);
                }
            }
        }
        inner.tracked = projects;
    }

    /// The project's traces that are complete at `now` (D-I-7): a rooted trace idle for
    /// [`ROOT_IDLE_NS`], or any trace idle for `timeout_ns`. At most `limit` of them, the earliest
    /// `min_start` first. The lock covers only the scan, which stops at `4 × limit` matches; the
    /// sort happens after it is released.
    // I4.7 calls it.
    #[allow(dead_code)]
    pub(crate) fn completed(
        &self,
        project: ProjectId,
        now: i64,
        timeout_ns: i64,
        limit: usize,
    ) -> Vec<Completed> {
        let cap = scan_cap(limit);
        let mut found = Vec::with_capacity(cap);
        {
            let inner = lock(&self.inner);
            if let Some(traces) = inner.projects.get(&project) {
                for (trace_id, entry) in traces {
                    if found.len() >= cap {
                        break;
                    }
                    let idle = now.saturating_sub(entry.last_seen);
                    if (entry.root_ended && idle >= ROOT_IDLE_NS) || idle >= timeout_ns {
                        found.push(Completed {
                            trace_id: *trace_id,
                            last_seen: entry.last_seen,
                            min_start: entry.min_start,
                            spans: entry.spans,
                        });
                    }
                }
            }
        }
        if found.len() > limit {
            found.select_nth_unstable_by(limit.saturating_sub(1), order);
            found.truncate(limit);
        }
        found.sort_unstable_by(order);
        found
    }

    /// Removes each trace whose `last_seen` and span count still equal the completed one's: a
    /// trace that grew during the sweep's reads stays (D-I-9). A project left with no traces
    /// loses its map.
    // I4.7 calls it.
    #[allow(dead_code)]
    pub(crate) fn finish(&self, project: ProjectId, done: &[Completed]) {
        let mut guard = lock(&self.inner);
        let inner = &mut *guard;
        let Some(traces) = inner.projects.get_mut(&project) else {
            return;
        };
        for completed in done {
            let unchanged = traces.get(&completed.trace_id).is_some_and(|entry| {
                entry.last_seen == completed.last_seen && entry.spans == completed.spans
            });
            if unchanged {
                traces.remove(&completed.trace_id);
                inner.len -= 1;
                let key = (project, completed.trace_id);
                inner.changed.remove(&key);
                inner.removed.insert(key);
            }
        }
        if traces.is_empty() {
            inner.projects.remove(&project);
        }
    }

    /// Counts a failed sweep for each trace still present. A trace that reaches
    /// [`MAX_FAILED_READS`] is removed. Returns how many were removed.
    // I4.7 calls it.
    #[allow(dead_code)]
    pub(crate) fn read_failed(&self, project: ProjectId, failed: &[Completed]) -> usize {
        let mut guard = lock(&self.inner);
        let inner = &mut *guard;
        let mut removed_count = 0;
        let Some(traces) = inner.projects.get_mut(&project) else {
            return removed_count;
        };
        for completed in failed {
            let Some(entry) = traces.get_mut(&completed.trace_id) else {
                continue;
            };
            entry.failed_reads = entry.failed_reads.saturating_add(1);
            if entry.failed_reads >= MAX_FAILED_READS {
                traces.remove(&completed.trace_id);
                inner.len -= 1;
                let key = (project, completed.trace_id);
                inner.changed.remove(&key);
                inner.removed.insert(key);
                removed_count += 1;
            }
        }
        if traces.is_empty() {
            inner.projects.remove(&project);
        }
        removed_count
    }

    /// Takes the changes since the last take, sorted by key so the flush order is deterministic.
    pub(crate) fn take_changes(&self) -> Changes {
        let mut changes = {
            let mut guard = lock(&self.inner);
            let inner = &mut *guard;
            let changed = std::mem::take(&mut inner.changed);
            let removed = std::mem::take(&mut inner.removed);
            let upserts = changed
                .into_iter()
                .filter_map(|(project, trace)| {
                    let entry = *inner.projects.get(&project)?.get(&trace)?;
                    Some((project, trace, entry))
                })
                .collect();
            let deletes = removed.into_iter().collect();
            Changes { upserts, deletes }
        };
        changes
            .upserts
            .sort_unstable_by_key(|(project, trace, _)| (*project, *trace));
        changes
            .deletes
            .sort_unstable_by_key(|(project, trace)| (*project, *trace));
        changes
    }

    /// Puts back the changes of a failed flush. Upserts whose entry is gone and deletes whose
    /// entry came back are dropped: the map already says what the database must hold.
    pub(crate) fn restore(&self, changes: Changes) {
        let mut guard = lock(&self.inner);
        let inner = &mut *guard;
        for (project, trace, _) in changes.upserts {
            let exists = inner
                .projects
                .get(&project)
                .is_some_and(|traces| traces.contains_key(&trace));
            if exists {
                inner.changed.insert((project, trace));
            }
        }
        for (project, trace) in changes.deletes {
            let exists = inner
                .projects
                .get(&project)
                .is_some_and(|traces| traces.contains_key(&trace));
            if !exists {
                inner.removed.insert((project, trace));
            }
        }
    }

    /// Loads the table at startup. Entries of tracked projects are held while there is room and
    /// are not marked changed. The rest are marked for deletion, so the next flush removes their rows.
    pub(crate) fn load(&self, rows: impl IntoIterator<Item = (ProjectId, TraceId, Entry)>) {
        let mut guard = lock(&self.inner);
        let inner = &mut *guard;
        for (project, trace, entry) in rows {
            let key = (project, trace);
            let tracked = inner.tracked.contains(&project);
            if tracked && inner.len < self.max {
                let traces = inner.projects.entry(project).or_default();
                if let std::collections::hash_map::Entry::Vacant(slot) = traces.entry(trace) {
                    slot.insert(entry);
                    inner.len += 1;
                }
            } else {
                inner.removed.insert(key);
            }
        }
    }

    /// Traces held.
    pub(crate) fn len(&self) -> usize {
        lock(&self.inner).len
    }
}

/// The sweeper's order: earliest `min_start` first, then trace id.
fn order(left: &Completed, right: &Completed) -> std::cmp::Ordering {
    (left.min_start, left.trace_id).cmp(&(right.min_start, right.trace_id))
}

/// How many entries one `completed` scan may collect: four times the limit (D-I-9).
fn scan_cap(limit: usize) -> usize {
    limit.saturating_mul(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{capture_logs, dataset_span};
    use llmobs_core::ids::SpanId;

    const SECOND: i64 = 1_000_000_000;

    fn project_id() -> ProjectId {
        ProjectId::new_v7()
    }

    fn trace_of(byte: u8) -> TraceId {
        let mut bytes = [0u8; 16];
        bytes[15] = byte;
        TraceId::from_bytes(bytes).unwrap()
    }

    /// A span of trace `trace`, starting at `start`. `root` makes it a span without a parent.
    fn span(trace: u8, root: bool, start: i64) -> Span {
        let mut span = dataset_span(0);
        span.trace_id = trace_of(trace);
        span.span_id = SpanId::from_bytes([0, 0, 0, 0, 0, 0, 0, trace]).unwrap();
        span.parent_span_id = if root {
            None
        } else {
            Some(SpanId::from_bytes([0, 0, 0, 0, 0, 0, 0, 0xff]).unwrap())
        };
        span.start_time = start;
        span.environment = None;
        span.metadata.clear();
        span
    }

    fn track(pending: &PendingTraces, project: ProjectId) {
        pending.set_tracked(HashSet::from([project]));
    }

    #[test]
    fn untracked_projects_are_ignored() {
        let pending = PendingTraces::new(10);
        pending.observe(project_id(), &[span(1, true, 100)], 10);
        assert_eq!(pending.len(), 0);
    }

    #[test]
    fn eval_and_experiment_spans_are_ignored() {
        let pending = PendingTraces::new(10);
        let project = project_id();
        track(&pending, project);
        let mut judge = span(1, true, 100);
        judge.environment = Some(environment::EVAL.to_owned());
        let mut experiment = span(2, true, 100);
        experiment
            .metadata
            .insert(metadata_key::EXPERIMENT_ID.to_owned(), "exp-1".to_owned());
        pending.observe(project, &[judge, experiment], 10);
        assert_eq!(pending.len(), 0);
    }

    #[test]
    fn observe_tracks_first_last_min_and_root() {
        let pending = PendingTraces::new(10);
        let project = project_id();
        track(&pending, project);
        pending.observe(project, &[span(1, false, 500)], 10);
        pending.observe(project, &[span(1, true, 300)], 20);
        let rows = pending.take_changes().upserts;
        assert_eq!(rows.len(), 1);
        let (_, trace, entry) = rows[0];
        assert_eq!(trace, trace_of(1));
        assert_eq!(
            entry,
            Entry {
                first_seen: 10,
                last_seen: 20,
                min_start: 300,
                root_ended: true,
                failed_reads: 0,
                spans: 2,
            }
        );
    }

    #[tokio::test]
    async fn the_map_is_capped() {
        let pending = PendingTraces::new(2);
        let project = project_id();
        track(&pending, project);
        pending.observe(project, &[span(1, true, 1)], 0);
        pending.observe(project, &[span(2, true, 1)], 0);
        // Three overflowing calls: the first warns, the second is within a minute of it, and the
        // third is a minute later and warns again.
        let ((), logs) = capture_logs(async {
            pending.observe(project, &[span(3, true, 1)], 0);
            pending.observe(project, &[span(4, true, 1)], 30 * SECOND);
            pending.observe(project, &[span(5, true, 1)], 61 * SECOND);
        })
        .await;
        assert_eq!(pending.len(), 2);
        assert_eq!(logs.matches("online-rule trigger map is full").count(), 2);
        assert!(logs.contains("dropped=1"), "logs: {logs}");
        assert!(logs.contains("dropped=3"), "logs: {logs}");
    }

    #[test]
    fn a_span_of_an_existing_trace_still_updates_a_full_map() {
        let pending = PendingTraces::new(1);
        let project = project_id();
        track(&pending, project);
        pending.observe(project, &[span(1, true, 1)], 10);
        pending.observe(project, &[span(1, false, 0)], 20);
        let rows = pending.take_changes().upserts;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2.last_seen, 20);
        assert_eq!(rows[0].2.min_start, 0);
    }

    #[test]
    fn completed_uses_root_idle_and_timeout() {
        let pending = PendingTraces::new(10);
        let project = project_id();
        track(&pending, project);
        pending.observe(project, &[span(1, true, 500)], 0);
        pending.observe(project, &[span(2, false, 300)], 0);
        let timeout = 60 * SECOND;

        assert!(
            pending
                .completed(project, 5 * SECOND - 1, timeout, 10)
                .is_empty()
        );
        let at_root_idle = pending.completed(project, 5 * SECOND, timeout, 10);
        assert_eq!(
            at_root_idle,
            vec![Completed {
                trace_id: trace_of(1),
                last_seen: 0,
                min_start: 500,
                spans: 1,
            }]
        );
        let at_timeout = pending.completed(project, 60 * SECOND, timeout, 10);
        assert_eq!(at_timeout.len(), 2);
        assert_eq!(at_timeout[0].trace_id, trace_of(2), "sorted by min_start");

        assert_eq!(pending.completed(project, 60 * SECOND, timeout, 1).len(), 1);
    }

    #[test]
    fn scan_cap_is_four_times_the_limit() {
        assert_eq!(scan_cap(5), 20);
        assert_eq!(scan_cap(0), 0);
        assert_eq!(scan_cap(usize::MAX), usize::MAX);
    }

    #[test]
    fn completed_scans_at_most_scan_cap_and_returns_the_limit() {
        // 50 completed traces and limit 5: the scan stops at `scan_cap(5)` = 20 entries, so the
        // result is at most 5 of those 20, sorted by min_start.
        let pending = PendingTraces::new(100);
        let project = project_id();
        track(&pending, project);
        for index in 1..=50_u8 {
            pending.observe(project, &[span(index, true, 1000 - i64::from(index))], 0);
        }
        let done = pending.completed(project, 10 * SECOND, 60 * SECOND, 5);
        assert_eq!(done.len(), 5);
        assert!(
            scan_cap(5) < 50,
            "the cap must bind for this test to mean anything"
        );
        assert!(
            done.windows(2)
                .all(|pair| pair[0].min_start <= pair[1].min_start)
        );
        assert!(done.iter().all(|done| done.last_seen == 0));
    }

    #[test]
    fn finish_keeps_a_trace_observed_at_the_same_clock() {
        let pending = PendingTraces::new(10);
        let project = project_id();
        track(&pending, project);
        pending.observe(project, &[span(1, true, 1)], 0);
        let done = pending.completed(project, 10 * SECOND, 60 * SECOND, 10);
        assert_eq!(done.len(), 1);
        // A concurrent commit with an older clock: last_seen stays 0, but the trace grew.
        pending.observe(project, &[span(1, false, 2)], 0);
        pending.finish(project, &done);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending.take_changes().deletes, Vec::new());
        // The next sweep sees the grown count and completes it.
        let again = pending.completed(project, 10 * SECOND, 60 * SECOND, 10);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].spans, 2);
        pending.finish(project, &again);
        assert_eq!(pending.len(), 0);
    }

    #[test]
    fn a_new_span_resets_failed_reads() {
        let pending = PendingTraces::new(10);
        let project = project_id();
        track(&pending, project);
        pending.observe(project, &[span(1, true, 1)], 0);
        let failed = pending.completed(project, 10 * SECOND, 60 * SECOND, 10);
        for _ in 0..4 {
            assert_eq!(pending.read_failed(project, &failed), 0);
        }
        pending.observe(project, &[span(1, false, 2)], 0);
        for _ in 0..4 {
            assert_eq!(pending.read_failed(project, &failed), 0);
        }
        assert_eq!(pending.len(), 1);
        assert_eq!(pending.read_failed(project, &failed), 1);
        assert_eq!(pending.len(), 0);
    }

    #[test]
    fn a_project_without_traces_loses_its_map() {
        let pending = PendingTraces::new(10);
        let project = project_id();
        track(&pending, project);
        pending.observe(project, &[span(1, true, 1), span(2, true, 1)], 0);
        assert_eq!(lock(&pending.inner).projects.len(), 1);
        let done = pending.completed(project, 10 * SECOND, 60 * SECOND, 10);
        pending.finish(project, &done);
        assert_eq!(pending.len(), 0);
        assert!(lock(&pending.inner).projects.is_empty());

        pending.observe(project, &[span(3, true, 1)], 0);
        let failed = pending.completed(project, 10 * SECOND, 60 * SECOND, 10);
        for _ in 0..MAX_FAILED_READS {
            pending.read_failed(project, &failed);
        }
        assert_eq!(pending.len(), 0);
        assert!(lock(&pending.inner).projects.is_empty());
    }

    #[test]
    fn last_seen_never_moves_back() {
        let pending = PendingTraces::new(10);
        let project = project_id();
        track(&pending, project);
        pending.observe(project, &[span(1, true, 1)], 20);
        pending.observe(project, &[span(1, true, 1)], 10);
        let rows = pending.take_changes().upserts;
        assert_eq!(rows[0].2.last_seen, 20);
    }

    #[test]
    fn finish_keeps_a_trace_that_grew() {
        let pending = PendingTraces::new(10);
        let project = project_id();
        track(&pending, project);
        pending.observe(project, &[span(1, true, 1)], 0);
        pending.observe(project, &[span(2, true, 2)], 0);
        let timeout = 60 * SECOND;
        let done = pending.completed(project, 10 * SECOND, timeout, 10);
        assert_eq!(done.len(), 2);
        pending.observe(project, &[span(1, true, 1)], 9 * SECOND);
        pending.finish(project, &done);
        assert_eq!(pending.len(), 1);
        let changes = pending.take_changes();
        assert_eq!(changes.deletes, vec![(project, trace_of(2))]);
        assert_eq!(changes.upserts.len(), 1);
    }

    #[test]
    fn read_failed_drops_after_five() {
        let pending = PendingTraces::new(10);
        let project = project_id();
        track(&pending, project);
        pending.observe(project, &[span(1, true, 1)], 0);
        let failed = pending.completed(project, 10 * SECOND, 60 * SECOND, 10);
        for _ in 0..4 {
            assert_eq!(pending.read_failed(project, &failed), 0);
        }
        assert_eq!(pending.read_failed(project, &failed), 1);
        assert_eq!(pending.len(), 0);
    }

    #[test]
    fn set_tracked_drops_other_projects() {
        let pending = PendingTraces::new(10);
        let kept = project_id();
        let dropped = project_id();
        pending.set_tracked(HashSet::from([kept, dropped]));
        pending.observe(kept, &[span(1, true, 1)], 0);
        pending.observe(dropped, &[span(2, true, 1)], 0);
        pending.take_changes();
        pending.set_tracked(HashSet::from([kept]));
        assert_eq!(pending.len(), 1);
        assert_eq!(pending.take_changes().deletes, vec![(dropped, trace_of(2))]);
    }

    #[test]
    fn take_changes_then_restore() {
        let pending = PendingTraces::new(10);
        let project = project_id();
        track(&pending, project);
        pending.observe(project, &[span(2, true, 1)], 0);
        pending.observe(project, &[span(1, true, 1)], 0);
        let first = pending.take_changes();
        assert_eq!(first.upserts.len(), 2);
        assert!(
            first.upserts[0].1 < first.upserts[1].1,
            "sorted by trace id"
        );
        assert_eq!(pending.take_changes(), Changes::default());

        // A key observed between the take and the restore is marked once, and the restored
        // changes come back whole.
        let mut again = first.clone();
        pending.observe(project, &[span(1, true, 1)], 0);
        // Trace 1 is the first upsert (sorted by key) and now holds one more span.
        again.upserts[0].2.spans = 2;
        pending.restore(first);
        assert_eq!(pending.take_changes(), again);
    }

    #[test]
    fn load_skips_untracked_projects() {
        let pending = PendingTraces::new(10);
        let tracked = project_id();
        let untracked = project_id();
        track(&pending, tracked);
        let entry = Entry {
            first_seen: 0,
            last_seen: 0,
            min_start: 0,
            root_ended: false,
            failed_reads: 0,
            spans: 0,
        };
        pending.load([
            (tracked, trace_of(1), entry),
            (untracked, trace_of(2), entry),
        ]);
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending.take_changes(),
            Changes {
                upserts: Vec::new(),
                deletes: vec![(untracked, trace_of(2))],
            }
        );
    }

    #[test]
    fn load_past_max_deletes_the_rest() {
        let pending = PendingTraces::new(2);
        let project = project_id();
        track(&pending, project);
        let entry = Entry {
            first_seen: 0,
            last_seen: 0,
            min_start: 0,
            root_ended: false,
            failed_reads: 0,
            spans: 0,
        };
        pending.load((1..=3_u8).map(|index| (project, trace_of(index), entry)));
        assert_eq!(pending.len(), 2);
        assert_eq!(pending.take_changes().deletes, vec![(project, trace_of(3))]);
    }
}
