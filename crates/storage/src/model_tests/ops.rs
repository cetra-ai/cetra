//! The operations of the model test: what each one does to storage and to the model, the
//! generator, and `settle` (docs/plans/r-hardening/01-m1-model.md § R1.3).
//!
//! Ops are plain values resolved against the model when they run, so a shrunk failure prints
//! a readable op list.

use std::collections::BTreeSet;
use std::time::Duration;

use proptest::prelude::*;
use proptest::strategy::Union;

use super::check::check_settled_files;
use super::harness::Harness;
use super::model::{Model, ModelDeletion, ProjectModel, ScorePush, SpanCopy, SpanPush};
use super::observe::{observe_eval_trace_ids, observe_user_scope};
use super::shape::{
    HOUR_NS, P, ScoreKey, Target, day, deletion_id, hex, midnight, score, score_created_at, span,
    trace_id,
};
use crate::config::WalSync;
use crate::deletion::{Deletion, DeletionScope, DeletionStatus};
use crate::error::AppendError;
use crate::test_support::span_rows;
use crate::wal::{Table, parse_segment_name};
use llmobs_core::batch::scores_to_record_batch;
use llmobs_core::ids::TraceId;
use llmobs_core::span::Span;

/// One microsecond. Every op advances the clock by this much, so ingest times strictly rise.
pub(super) const MICRO_NS: i64 = 1_000;
const MILLI_NS: i64 = 1_000_000;
/// Trace numbers are `0..TRACES`, span numbers `0..SPANS`.
pub(super) const TRACES: u8 = 12;
pub(super) const SPANS: u8 = 12;
/// Session numbers are `0..SESSIONS`. A score target `t` is `Trace(t)` below `TRACES`, else
/// `Session(t - TRACES)`, so targets are `0..TRACES + SESSIONS`.
pub(super) const SESSIONS: u8 = 4;
const SCORE_TARGETS: u8 = TRACES + SESSIONS;
/// Score names are `0..SCORE_NAMES`.
const SCORE_NAMES: u8 = 3;
/// Passes a settle step makes before the case fails.
const SETTLE_PASSES: usize = 10;
/// The retention days a `Retention` op picks from, as offsets from `D`. Never `D + 2`: class-2
/// traces have rows on `D + 1` and `D + 2` whose partition depends on the anchor (D-R-3).
const RETENTION_DAYS: [u8; 3] = [1, 3, 4];

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
    /// Writes the next version of the score `(project, target, name)`: version 1
    /// when it doesn't exist yet. `target` < 12 is Trace(target), else Session(target - 12).
    Score {
        project: P,
        target: u8,
        name: u8,
    },
    /// Writes version `last + 1` with `deleted = true` of one score the model has
    /// written (`pick % len`, in key order). A no-op when the project has none. A dropped project
    /// keeps no scores, so its deletion picks one key of its score space and is refused.
    DeleteScore {
        project: P,
        pick: u8,
    },
    /// Writes an older version of one score the model has written (`pick % len`, in key
    /// order, among the keys whose live winner is not deleted, see `stale_candidates`; a no-op
    /// when there is none). `back % 3 == 0` re-sends the newest
    /// version `last` at a later ingest: a tie on `updated_at`, broken by ingest. Otherwise
    /// it writes `max(1, last - 1)`. Its `updated_at` is `created_at + version`, so the live
    /// score stays the newest version, whatever its ingest.
    StaleScore {
        project: P,
        pick: u8,
        back: u8,
    },
    /// `register_deletion` of the traces `picks` names (each `% TRACES`), plus their
    /// evaluation traces, cut off at the op's ingest time.
    DeleteTraces {
        project: P,
        picks: Vec<u8>,
    },
    /// A user deletion as the server resolves it (D-M-3): through `user_deletion_scope` and
    /// `eval_trace_ids`. `user % 2` picks `"u0"` or `"u1"`.
    DeleteUser {
        project: P,
        user: u8,
    },
    /// Advances the clock by `hours`, then one deletion pass.
    ApplyDeletions {
        hours: u8,
    },
    /// `apply_retention` of `RETENTION_DAYS[keep % 3]`.
    Retention {
        project: P,
        keep: u8,
    },
    /// Drops project B.
    DropB,
    /// Drops the storage without shutdown, applies a lossy crash with `seed`
    /// (`CrashMode::from_seed`) to the data directory, and reopens (R2.9).
    LossyCrash {
        seed: u64,
    },
}

impl Op {
    /// The project an op works on; `None` for ops that touch both.
    pub(super) fn project(&self) -> Option<P> {
        match *self {
            Self::Append { project, .. }
            | Self::Resend { project, .. }
            | Self::Score { project, .. }
            | Self::DeleteScore { project, .. }
            | Self::StaleScore { project, .. }
            | Self::DeleteTraces { project, .. }
            | Self::DeleteUser { project, .. }
            | Self::Retention { project, .. } => Some(project),
            Self::Restart
            | Self::Crash
            | Self::Compact { .. }
            | Self::Gc
            | Self::Settle
            | Self::ApplyDeletions { .. }
            | Self::DropB
            | Self::LossyCrash { .. } => None,
        }
    }
}

/// A sequence of 1 to 24 ops.
pub(super) fn ops() -> impl Strategy<Value = Vec<Op>> {
    proptest::collection::vec(op(), 1..=24)
}

/// Every op with its weight. One weighted union, since `prop_oneof!` takes at most ten arms.
fn op() -> impl Strategy<Value = Op> {
    Union::new_weighted(vec![
        (
            30,
            (project(), 0..TRACES, 1..=6_u8, any::<bool>())
                .prop_map(|(project, trace, count, split)| Op::Append {
                    project,
                    trace,
                    count,
                    split,
                })
                .boxed(),
        ),
        (
            4,
            (project(), resendable_trace(), any::<u8>())
                .prop_map(|(project, trace, pick)| Op::Resend {
                    project,
                    trace,
                    pick,
                })
                .boxed(),
        ),
        (3, Just(Op::Restart).boxed()),
        (3, Just(Op::Crash).boxed()),
        (
            3,
            (0..72_u8).prop_map(|hours| Op::Compact { hours }).boxed(),
        ),
        (1, Just(Op::Gc).boxed()),
        (1, Just(Op::Settle).boxed()),
        (
            8,
            (project(), 0..SCORE_TARGETS, 0..SCORE_NAMES)
                .prop_map(|(project, target, name)| Op::Score {
                    project,
                    target,
                    name,
                })
                .boxed(),
        ),
        (
            2,
            (project(), any::<u8>())
                .prop_map(|(project, pick)| Op::DeleteScore { project, pick })
                .boxed(),
        ),
        (
            2,
            (project(), any::<u8>(), any::<u8>())
                .prop_map(|(project, pick, back)| Op::StaleScore {
                    project,
                    pick,
                    back,
                })
                .boxed(),
        ),
        (
            3,
            (project(), prop::collection::vec(any::<u8>(), 1..=3))
                .prop_map(|(project, picks)| Op::DeleteTraces { project, picks })
                .boxed(),
        ),
        (
            2,
            (project(), any::<u8>())
                .prop_map(|(project, user)| Op::DeleteUser { project, user })
                .boxed(),
        ),
        (
            2,
            (0..24_u8)
                .prop_map(|hours| Op::ApplyDeletions { hours })
                .boxed(),
        ),
        (
            1,
            (project(), 0..3_u8)
                .prop_map(|(project, keep)| Op::Retention { project, keep })
                .boxed(),
        ),
        (1, Just(Op::DropB).boxed()),
        (
            3,
            any::<u64>()
                .prop_map(|seed| Op::LossyCrash { seed })
                .boxed(),
        ),
    ])
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
    model.ops_applied += 1;
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
        Op::Restart => {
            harness.restart();
            flushed(harness, model);
        }
        Op::Crash => {
            harness.crash();
            crashed(harness, model);
        }
        Op::LossyCrash { seed } => lossy_crash(harness, model, seed),
        Op::Compact { hours } => {
            *clock += i64::from(hours) * HOUR_NS;
            compact(harness, *clock);
        }
        Op::Gc => collect_obsolete(harness),
        Op::Settle => settle(harness, model, clock),
        Op::Score {
            project,
            target,
            name,
        } => put_score(harness, model, project, target, name, *clock),
        Op::DeleteScore { project, pick } => delete_score(harness, model, project, pick, *clock),
        Op::StaleScore {
            project,
            pick,
            back,
        } => stale_score(harness, model, project, pick, back, *clock),
        Op::DeleteTraces { project, ref picks } => {
            delete_traces(harness, model, project, picks, *clock);
        }
        Op::DeleteUser { project, user } => delete_user(harness, model, project, user, *clock),
        Op::ApplyDeletions { hours } => {
            *clock += i64::from(hours) * HOUR_NS;
            apply_deletions(harness, *clock);
        }
        Op::Retention { project, keep } => retention(harness, model, project, keep),
        Op::DropB => drop_b(harness, model),
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

/// Sends one batch of spans to a dropped project: storage refuses it, and the model keeps nothing.
fn send_refused(harness: &Harness, project: P, batch: &[Span]) {
    let result = harness.block_on(
        harness
            .storage()
            .append_spans(project.id(), span_rows(batch)),
    );
    assert!(
        matches!(result, Err(AppendError::ProjectDropped)),
        "append_spans on a dropped project is refused as ProjectDropped"
    );
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
    if model.project(project).dropped {
        // Storage refuses every batch of a dropped project, so one is always sent, even when the
        // trace has no span left to append. Its span numbers do not matter: nothing is stored.
        let batch: Vec<Span> = (0..count.clamp(1, SPANS))
            .map(|span_no| span(project, trace, span_no, clock))
            .collect();
        send_refused(harness, project, &batch);
        return;
    }
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
        let middle = first + u8::try_from(head.len()).expect("a batch of at most 6 spans");
        send(harness, project, head);
        record_spans(harness, model, project, trace, first..middle, clock);
        send(harness, project, tail);
        record_spans(harness, model, project, trace, middle..end, clock);
    } else {
        send(harness, project, &spans);
        record_spans(harness, model, project, trace, first..end, clock);
    }
    model.project_mut(project).next_span.insert(trace, end);
}

/// Stores the spans `span_nos` of `trace` in the model after a successful append of them, which
/// is one WAL record (R2.9).
fn record_spans(
    harness: &Harness,
    model: &mut Model,
    project: P,
    trace: u8,
    span_nos: std::ops::Range<u8>,
    clock: i64,
) {
    let state = model.project_mut(project);
    let pushed: Vec<SpanPush> = span_nos
        .filter(|&span_no| state.write_span(trace, span_no, clock))
        .map(|span_no| SpanPush {
            project,
            trace,
            span_no,
            ingest: clock,
        })
        .collect();
    let segments = segment_count(harness, Table::Spans);
    model
        .spans_wal
        .appended(pushed, segments, always_synced(harness));
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
    let batch = [span(project, trace, span_no, clock)];
    if model.project(project).dropped {
        send_refused(harness, project, &batch);
        return;
    }
    send(harness, project, &batch);
    record_spans(harness, model, project, trace, span_no..span_no + 1, clock);
}

/// The key a score target number names (see `Op::Score`).
fn target_of(target: u8) -> Target {
    if target < TRACES {
        Target::Trace(target)
    } else {
        Target::Session(target - TRACES)
    }
}

/// Every score key the ops can write: both projects, every target, every name (96 keys).
/// Keys the model has dropped or hides still have ids, so reads by id can check them.
pub(super) fn score_key_space() -> Vec<ScoreKey> {
    let mut keys = Vec::new();
    for project in [P::A, P::B] {
        for target in 0..SCORE_TARGETS {
            for name in 0..SCORE_NAMES {
                keys.push(ScoreKey {
                    project,
                    target: target_of(target),
                    name,
                });
            }
        }
    }
    keys
}

/// Writes the next version of `(project, target, name)`: version 1 when the key has none.
fn put_score(harness: &Harness, model: &mut Model, project: P, target: u8, name: u8, clock: i64) {
    let key = ScoreKey {
        project,
        target: target_of(target),
        name,
    };
    let last = model
        .project(project)
        .scores
        .get(&key)
        .and_then(|versions| versions.iter().map(|version| version.version).max());
    send_score(
        harness,
        model,
        key,
        last.map_or(1, |last| last + 1),
        false,
        clock,
    );
}

/// The score `pick % len` of `project` in key order, or `None` when the project has none.
fn picked_score(model: &Model, project: P, pick: u8) -> Option<ScoreKey> {
    let keys: Vec<ScoreKey> = model.project(project).scores.keys().copied().collect();
    if keys.is_empty() {
        return None;
    }
    Some(keys[usize::from(pick) % keys.len()])
}

/// The keys `StaleScore` may pick: those whose live winner (the visible version with the greatest
/// `(created_at + version, ingest)`, as `expected_scores` orders them) is not a tombstone. A
/// compaction that day-closes the scores partition drops a tombstone, so after it a stale version
/// reads as live while the model still expects the tombstone to win (known limit M1-2, spec 03 § 5).
/// Such keys are left out, and a `StaleScore` with no candidate is a no-op without a call.
///
/// The function also skips keys with no visible version (every copy is covered by a deletion), and
/// stale re-sends to a key whose newest stale version is a tombstone, even before any day close.
/// Both are deliberate because of known limit M1-2: a re-send to such a key would resurrect data
/// that the model can no longer see once a day close drops the tombstone.
fn stale_candidates(model: &Model, project: P) -> Vec<ScoreKey> {
    let state = model.project(project);
    state
        .scores
        .iter()
        .filter(|(key, versions)| {
            versions
                .iter()
                .filter(|version| !state.covers_score(key, version.ingest))
                .max_by_key(|version| {
                    (
                        score_created_at(key) + i64::from(version.version),
                        version.ingest,
                    )
                })
                .is_some_and(|winner| !winner.deleted)
        })
        .map(|(key, _)| *key)
        .collect()
}

/// The newest version the model has of `key`.
fn last_version(model: &Model, key: ScoreKey) -> u32 {
    model.project(key.project).scores[&key]
        .iter()
        .map(|version| version.version)
        .max()
        .expect("a key in the model has a version")
}

/// Writes `last + 1` of a score the model has written, as a deletion. `pick % len` in key order.
/// On a dropped project it writes version 1 of one key of the project's score space, refused.
fn delete_score(harness: &Harness, model: &mut Model, project: P, pick: u8, clock: i64) {
    if model.project(project).dropped {
        let keys: Vec<ScoreKey> = score_key_space()
            .into_iter()
            .filter(|key| key.project == project)
            .collect();
        send_score(
            harness,
            model,
            keys[usize::from(pick) % keys.len()],
            1,
            true,
            clock,
        );
        return;
    }
    let Some(key) = picked_score(model, project, pick) else {
        return;
    };
    let last = last_version(model, key);
    send_score(harness, model, key, last + 1, true, clock);
}

/// Writes an older (or equal, see `Op::StaleScore`) version of a score the model has written.
/// The version repeats the `deleted` flag of the stored version it names, if one exists.
fn stale_score(harness: &Harness, model: &mut Model, project: P, pick: u8, back: u8, clock: i64) {
    let candidates = stale_candidates(model, project);
    if candidates.is_empty() {
        return;
    }
    let key = candidates[usize::from(pick) % candidates.len()];
    let last = last_version(model, key);
    let version = if back.is_multiple_of(3) {
        last
    } else {
        last.saturating_sub(1).max(1)
    };
    let deleted = model.project(project).scores[&key]
        .iter()
        .filter(|stored| stored.version == version)
        .max_by_key(|stored| stored.ingest)
        .is_some_and(|stored| stored.deleted);
    send_score(harness, model, key, version, deleted, clock);
}

/// Sends one score version (a one-row batch). A dropped project refuses it, and the model
/// keeps no version.
fn send_score(
    harness: &Harness,
    model: &mut Model,
    key: ScoreKey,
    version: u32,
    deleted: bool,
    clock: i64,
) {
    let project = key.project;
    let batch = scores_to_record_batch(&[score(&key, version, deleted, clock)])
        .expect("a score batch of one score");
    let result = harness.block_on(harness.storage().append_scores(project.id(), batch));
    if model.project(project).dropped {
        assert!(
            matches!(result, Err(AppendError::ProjectDropped)),
            "append_scores on a dropped project is refused as ProjectDropped"
        );
        return;
    }
    result.expect("append_scores of one score");
    let stored = model
        .project_mut(project)
        .write_score(key, version, deleted, clock);
    let pushed: Vec<ScorePush> = stored
        .then_some(ScorePush {
            key,
            version,
            ingest: clock,
        })
        .into_iter()
        .collect();
    let segments = segment_count(harness, Table::Scores);
    model
        .scores_wal
        .appended(pushed, segments, always_synced(harness));
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

/// The trace numbers `picks` names: each `% TRACES`, as a set.
fn picked_traces(picks: &[u8]) -> BTreeSet<u8> {
    picks.iter().map(|pick| pick % TRACES).collect()
}

/// The trace ids of `traces`, in trace-number order (which is byte order, see `trace_id`).
fn ids_of(traces: &BTreeSet<u8>) -> Vec<TraceId> {
    traces.iter().map(|&trace| trace_id(trace)).collect()
}

/// The hex form of each id, in order. Ids are compared as hex, so an id the model does not know
/// is printed by an `assert_eq!` rather than panicking on the lookup.
fn hex_of(ids: &[TraceId]) -> Vec<String> {
    ids.iter().map(|id| hex(id.as_bytes())).collect()
}

/// The hex ids of the trace numbers in `traces`, in order.
fn hex_numbers(traces: &BTreeSet<u8>) -> Vec<String> {
    hex_of(&ids_of(traces))
}

/// Checks `eval_trace_ids` of `targets` against the model, and returns the evaluation traces
/// the model expects.
fn resolve_evals(harness: &Harness, state: &ProjectModel, targets: &BTreeSet<u8>) -> BTreeSet<u8> {
    let observed = observe_eval_trace_ids(harness, state.project, &ids_of(targets));
    let expected = state.eval_traces_for(targets);
    assert_eq!(
        hex_of(&observed),
        hex_numbers(&expected),
        "eval_trace_ids of targets {targets:?} in project {:?} differ from the model",
        state.project
    );
    expected
}

/// Registers a deletion, expecting success.
fn register(harness: &Harness, deletion: Deletion) {
    harness
        .block_on(harness.storage().register_deletion(deletion))
        .expect("register_deletion on a live project");
}

/// `register_deletion` of the picked traces and their evaluation traces, cut off at `clock`.
fn delete_traces(harness: &Harness, model: &mut Model, project: P, picks: &[u8], clock: i64) {
    if model.project(project).dropped {
        return;
    }
    let targets = picked_traces(picks);
    let evals = resolve_evals(harness, model.project(project), &targets);
    let covered: BTreeSet<u8> = targets.union(&evals).copied().collect();
    let id = deletion_id(clock);
    register(
        harness,
        Deletion {
            id,
            project: project.id(),
            cutoff: clock,
            scope: DeletionScope::Traces {
                trace_ids: ids_of(&covered),
            },
        },
    );
    model.project_mut(project).deletions.push(ModelDeletion {
        id,
        cutoff: clock,
        traces: covered,
        user: None,
        sessions: BTreeSet::new(),
        applied_seen: false,
    });
}

/// A user deletion, resolved as the server does (D-M-3): `user_deletion_scope` first, then
/// `eval_trace_ids` of its traces. Both results are checked against the model before the
/// deletion is registered with them.
fn delete_user(harness: &Harness, model: &mut Model, project: P, user: u8, clock: i64) {
    if model.project(project).dropped {
        return;
    }
    let name = format!("u{}", user % 2);
    let (expected_traces, expected_sessions) = model.project(project).user_scope(&name);
    let observed = observe_user_scope(harness, project, &name, clock);
    assert_eq!(
        hex_of(&observed.trace_ids),
        hex_numbers(&expected_traces),
        "user_deletion_scope of {name} in project {project:?}: traces differ from the model"
    );
    assert_eq!(
        observed.session_ids,
        expected_sessions.iter().cloned().collect::<Vec<String>>(),
        "user_deletion_scope of {name} in project {project:?}: sessions differ from the model"
    );
    let evals = resolve_evals(harness, model.project(project), &expected_traces);
    let covered: BTreeSet<u8> = expected_traces.union(&evals).copied().collect();
    let id = deletion_id(clock);
    register(
        harness,
        Deletion {
            id,
            project: project.id(),
            cutoff: clock,
            scope: DeletionScope::User {
                user_id: name.clone(),
                trace_ids: ids_of(&covered),
                session_ids: observed.session_ids,
            },
        },
    );
    model.project_mut(project).deletions.push(ModelDeletion {
        id,
        cutoff: clock,
        traces: covered,
        user: Some(name),
        sessions: expected_sessions,
        applied_seen: false,
    });
}

/// `apply_retention` of one of `RETENTION_DAYS`. The model keeps the watermark rule of R1.2.
fn retention(harness: &Harness, model: &mut Model, project: P, keep: u8) {
    if model.project(project).dropped {
        return;
    }
    let keep_from = day(RETENTION_DAYS[usize::from(keep % 3)]);
    harness
        .block_on(harness.storage().apply_retention(project.id(), keep_from))
        .expect("apply_retention on a live project");
    model.project_mut(project).apply_retention(keep_from);
}

/// Drops project B. Later drops are no-ops without a call.
fn drop_b(harness: &Harness, model: &mut Model) {
    if model.b.dropped {
        return;
    }
    harness
        .block_on(harness.storage().drop_project(P::B.id()))
        .expect("drop_project of B");
    model.project_mut(P::B).drop_project();
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

/// Mirrors the day close of scores (spec 03 § 5). Per key, the live winner is the visible version
/// with the greatest `(created_at + version, ingest)`, as `expected_scores` orders them. When it is
/// a tombstone, the day close dropped every version of the key, so the model drops them too. A
/// later `StaleScore` then finds no key to pick, and a later `Score` starts at version 1 again.
/// That is the known limit spec 03 § 5 documents: a stale version after the day close reads as live.
fn drop_day_closed_scores(state: &mut ProjectModel) {
    let closed: Vec<ScoreKey> = state
        .scores
        .iter()
        .filter_map(|(key, versions)| {
            let winner = versions
                .iter()
                .filter(|version| !state.covers_score(key, version.ingest))
                .max_by_key(|version| {
                    (
                        score_created_at(key) + i64::from(version.version),
                        version.ingest,
                    )
                })?;
            winner.deleted.then_some(*key)
        })
        .collect();
    for key in closed {
        state.scores.remove(&key);
    }
}

/// Quiesces storage, then makes the model match it (plan § R1.3, steps 1–6).
/// The physical checks of the settled files come in R1.7.
pub(super) fn settle(harness: &mut Harness, model: &mut Model, clock: &mut i64) {
    // 1. Everything in files, so no memtable batch blocks "applied".
    harness.restart();
    flushed(harness, model);

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

    // 5. Nothing obsolete, nothing staged: the outputs of failed jobs stay staged until GC.
    collect_obsolete(harness);
    harness
        .block_on(harness.storage().collect_staged(Duration::ZERO))
        .expect("collect_staged while settling");

    // 6. Compaction collapsed every key to one copy, and day-closed the scores whose tombstone won.
    keep_newest_copies(&mut model.a);
    keep_newest_copies(&mut model.b);
    drop_day_closed_scores(&mut model.a);
    drop_day_closed_scores(&mut model.b);

    check_settled_files(harness, model);
}

/// A model for a fresh case on `harness`: its WALs start with the segments the open found.
pub(super) fn new_model(harness: &Harness) -> Model {
    Model::new(
        segment_count(harness, Table::Spans),
        segment_count(harness, Table::Scores),
    )
}

/// Segment files of `table`'s WAL in this life, read from its directory (R2.9).
fn segment_count(harness: &Harness, table: Table) -> usize {
    let dir = harness.dir().join("wal").join(table.name());
    std::fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("read the {} WAL directory: {error}", table.name()))
        .filter_map(Result::ok)
        .filter(|entry| parse_segment_name(&entry.file_name().to_string_lossy()).is_some())
        .count()
}

/// Whether this case syncs every record before its ack (`wal.sync = "always"`).
fn always_synced(harness: &Harness) -> bool {
    matches!(harness.config().wal.sync, WalSync::Always)
}

/// The clean shutdown of `Restart` and `Settle` flushed this life's records.
fn flushed(harness: &Harness, model: &mut Model) {
    model
        .spans_wal
        .flushed(segment_count(harness, Table::Spans));
    model
        .scores_wal
        .flushed(segment_count(harness, Table::Scores));
}

/// `Crash`: this life's records replay after the reopen.
fn crashed(harness: &Harness, model: &mut Model) {
    model
        .spans_wal
        .crashed(segment_count(harness, Table::Spans));
    model
        .scores_wal
        .crashed(segment_count(harness, Table::Scores));
}

/// `LossyCrash`: applies a lossy crash, reopens, and makes the model forget the rows of the
/// records that did not survive. The surviving count of each WAL is `replayed` minus the records
/// of earlier lives that were still in the WAL (R2.9).
fn lossy_crash(harness: &mut Harness, model: &mut Model, seed: u64) {
    harness.lossy_crash(seed);
    let op = model.ops_applied - 1;
    let spans_replayed = harness.storage().replayed(Table::Spans);
    let scores_replayed = harness.storage().replayed(Table::Scores);
    let lost_spans = model.spans_wal.lossy_crashed(
        spans_replayed,
        segment_count(harness, Table::Spans),
        "spans",
        op,
    );
    for push in &lost_spans {
        model.project_mut(push.project).unstore_span(push);
    }
    let lost_scores = model.scores_wal.lossy_crashed(
        scores_replayed,
        segment_count(harness, Table::Scores),
        "scores",
        op,
    );
    for push in &lost_scores {
        model.project_mut(push.key.project).unstore_score(push);
    }
}
