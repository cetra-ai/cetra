//! The operations of the model test: what each one does to storage and to the model, the
//! generator, and `settle` (docs/plans/r-hardening/01-m1-model.md § R1.3).
//!
//! Ops are plain values resolved against the model when they run, so a shrunk failure prints
//! a readable op list.

use std::time::Duration;

use proptest::prelude::*;

use super::harness::Harness;
use super::model::{Model, ProjectModel, SpanCopy};
use super::shape::{HOUR_NS, P, day, midnight, span};
use crate::deletion::DeletionStatus;
use crate::test_support::span_rows;
use llmobs_core::span::Span;

/// One microsecond. Every op advances the clock by this much, so ingest times strictly rise.
const MICRO_NS: i64 = 1_000;
const MILLI_NS: i64 = 1_000_000;
/// Trace numbers are `0..TRACES`, span numbers `0..SPANS`.
pub(super) const TRACES: u8 = 12;
pub(super) const SPANS: u8 = 12;
/// Passes a settle step makes before the case fails.
const SETTLE_PASSES: usize = 10;

#[derive(Debug, Clone)]
pub(super) enum Op {
    /// Appends the next `count` spans (1..=6) of `trace`, in one batch, or in
    /// two batches when `split`. Spans past 11 are not appended.
    Append {
        project: P,
        trace: u8,
        count: u8,
        split: bool,
    },
    /// Appends again one span of `trace` (picked by `pick % len`) with a new
    /// ingest time. The candidates are `0..next_span[trace]`, whether covered
    /// by a deletion or not. A class-2 trace (`trace % 4 == 2`) is never
    /// re-sent: its anchor can move between flushes (`anchor.rs`), so a copy
    /// could land in another partition and never be merged (known limit).
    /// The generator never picks such a trace.
    Resend {
        project: P,
        trace: u8,
        pick: u8,
    },
    Restart,
    Crash,
    /// Advances the clock by `hours` (0..72), then one compaction pass.
    Compact {
        hours: u8,
    },
    /// `collect_obsolete(Duration::ZERO)`.
    Gc,
    /// Everything flushed, every deletion applied, every partition compacted,
    /// nothing obsolete (below).
    Settle,
}

impl Op {
    /// The project an op works on; `None` for ops that touch both.
    pub(super) fn project(&self) -> Option<P> {
        match *self {
            Self::Append { project, .. } | Self::Resend { project, .. } => Some(project),
            Self::Restart | Self::Crash | Self::Compact { .. } | Self::Gc | Self::Settle => None,
        }
    }
}

/// A sequence of 1 to 24 ops.
pub(super) fn ops() -> impl Strategy<Value = Vec<Op>> {
    proptest::collection::vec(op(), 1..=24)
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        30 => (project(), 0..TRACES, 1..=6_u8, any::<bool>()).prop_map(
            |(project, trace, count, split)| Op::Append { project, trace, count, split }
        ),
        4 => (project(), resendable_trace(), any::<u8>()).prop_map(
            |(project, trace, pick)| Op::Resend { project, trace, pick }
        ),
        3 => Just(Op::Restart),
        3 => Just(Op::Crash),
        3 => (0..72_u8).prop_map(|hours| Op::Compact { hours }),
        1 => Just(Op::Gc),
        1 => Just(Op::Settle),
    ]
}

/// Project A with weight 3, project B with weight 1.
fn project() -> impl Strategy<Value = P> {
    prop_oneof![3 => Just(P::A), 1 => Just(P::B)]
}

/// Traces a re-send may pick: every trace but class 2 (`anchor.rs`, known limit).
fn resendable_trace() -> impl Strategy<Value = u8> {
    prop::sample::select(
        (0..TRACES)
            .filter(|trace| trace % 4 != 2)
            .collect::<Vec<u8>>(),
    )
}

/// Runs one op against storage and the model. `clock` advances first (by one microsecond,
/// and by the op's own hours for `Compact`).
pub(super) fn apply(op: &Op, harness: &mut Harness, model: &mut Model, clock: &mut i64) {
    *clock += MICRO_NS;
    match *op {
        Op::Append {
            project,
            trace,
            count,
            split,
        } => append(harness, model, project, trace, count, split, *clock),
        Op::Resend {
            project,
            trace,
            pick,
        } => resend(harness, model, project, trace, pick, *clock),
        Op::Restart => harness.restart(),
        Op::Crash => harness.crash(),
        Op::Compact { hours } => {
            *clock += i64::from(hours) * HOUR_NS;
            compact(harness, *clock);
        }
        Op::Gc => collect_obsolete(harness),
        Op::Settle => settle(harness, model, clock),
    }
}

/// Sends one batch of spans to storage, expecting success. Never called with an empty batch.
fn send(harness: &Harness, project: P, batch: &[Span]) {
    harness
        .block_on(
            harness
                .storage()
                .append_spans(project.id(), span_rows(batch)),
        )
        .expect("append_spans of a span batch");
}

fn append(
    harness: &Harness,
    model: &mut Model,
    project: P,
    trace: u8,
    count: u8,
    split: bool,
    clock: i64,
) {
    let first = model
        .project(project)
        .next_span
        .get(&trace)
        .copied()
        .unwrap_or(0);
    let end = first.saturating_add(count).min(SPANS);
    if first >= end {
        return;
    }
    let spans: Vec<Span> = (first..end)
        .map(|span_no| span(project, trace, span_no, clock))
        .collect();
    if split && spans.len() >= 2 {
        let (head, tail) = spans.split_at(spans.len() / 2);
        send(harness, project, head);
        send(harness, project, tail);
    } else {
        send(harness, project, &spans);
    }
    let state = model.project_mut(project);
    for span_no in first..end {
        state.write_span(trace, span_no, clock);
    }
    state.next_span.insert(trace, end);
}

fn resend(harness: &Harness, model: &mut Model, project: P, trace: u8, pick: u8, clock: i64) {
    debug_assert!(trace % 4 != 2, "class-2 traces are never re-sent");
    let next = model
        .project(project)
        .next_span
        .get(&trace)
        .copied()
        .unwrap_or(0);
    if next == 0 {
        return;
    }
    let span_no = pick % next;
    send(harness, project, &[span(project, trace, span_no, clock)]);
    model.project_mut(project).write_span(trace, span_no, clock);
}

fn compact(harness: &Harness, clock: i64) {
    let report = harness
        .block_on(harness.storage().compact(clock))
        .expect("compact at the model clock");
    assert_eq!(report.failed, 0, "a compaction job failed");
}

fn apply_deletions(harness: &Harness, clock: i64) {
    let report = harness
        .block_on(harness.storage().apply_deletions(clock))
        .expect("apply_deletions at the model clock");
    assert_eq!(report.failed, 0, "a deletion job failed");
}

fn collect_obsolete(harness: &Harness) {
    harness
        .block_on(harness.storage().collect_obsolete(Duration::ZERO))
        .expect("collect_obsolete with no grace");
}

/// Whether every deletion either project registered reads `Applied`.
fn all_deletions_applied(harness: &Harness, model: &Model) -> bool {
    model
        .a
        .deletions
        .iter()
        .chain(&model.b.deletions)
        .all(|deletion| {
            let status = harness
                .block_on(harness.storage().deletion_status(deletion.id))
                .expect("deletion_status of a registered deletion");
            match status {
                DeletionStatus::Applied { .. } => true,
                DeletionStatus::Pending => false,
            }
        })
}

/// Storage keeps one copy per key after compaction, and the newest one wins (spec 03 §6).
/// A covered copy is never newer than an uncovered one of its key, so the newest copy is
/// the visible one whenever a visible copy exists.
fn keep_newest_copies(state: &mut ProjectModel) {
    for copies in state.spans.values_mut() {
        let newest = copies
            .iter()
            .map(|copy| copy.ingest)
            .max()
            .expect("a key in the model has a copy");
        *copies = vec![SpanCopy { ingest: newest }];
    }
}

/// Quiesces storage, then makes the model match it (plan § R1.3, steps 1–6).
/// The physical checks of the settled files come in R1.7.
pub(super) fn settle(harness: &mut Harness, model: &mut Model, clock: &mut i64) {
    // 1. Everything in files, so no memtable batch blocks "applied".
    harness.restart();

    // 2. The clock must be past the newest ingest by `close_delay` too, or a late L0 file
    //    is legitimately not closed yet and compaction cannot make progress.
    let close_delay =
        i64::try_from(harness.config().compaction.close_delay.as_nanos()).unwrap_or(i64::MAX);
    *clock = (*clock)
        .max(midnight(day(6)))
        .saturating_add(close_delay)
        .saturating_add(MILLI_NS);

    // 3. Every deletion applied: while one is pending, the traces view hides covered
    //    partials whole and the checks would not match.
    for _ in 0..SETTLE_PASSES {
        apply_deletions(harness, *clock);
        if all_deletions_applied(harness, model) {
            break;
        }
    }
    assert!(
        all_deletions_applied(harness, model),
        "a deletion is still pending after {SETTLE_PASSES} passes"
    );

    // 4. Every partition one file.
    let mut compacted = false;
    for _ in 0..SETTLE_PASSES {
        let report = harness
            .block_on(harness.storage().compact(*clock))
            .expect("compact while settling");
        assert_eq!(report.failed, 0, "a compaction job failed while settling");
        if report.jobs == 0 {
            compacted = true;
            break;
        }
    }
    assert!(
        compacted,
        "compaction still has jobs after {SETTLE_PASSES} passes"
    );

    // 5. Nothing obsolete.
    collect_obsolete(harness);

    // 6. Compaction collapsed every key to one copy.
    keep_newest_copies(&mut model.a);
    keep_newest_copies(&mut model.b);

    // R1.7: the physical checks of the settled files go here.
}
