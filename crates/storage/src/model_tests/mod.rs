//! Track R M1: random operations against `Storage` and an in-memory model of
//! every read (docs/plans/r-hardening/01-m1-model.md).

mod check;
mod harness;
mod model;
mod observe;
mod ops;
mod shape;

use proptest::prelude::*;

use check::{check_deletions, check_dropped, check_full, check_quick};
use harness::Harness;
use model::Model;
use observe::{ALL_ROWS, observe_scores, observe_spans, observe_traces};
use ops::{Op, apply, settle};
use shape::{HOUR_NS, P, day, midnight, trace_id};
use uuid::Uuid;

use crate::deletion::{Deletion, DeletionScope, DeletionStatus};
use crate::reads::MAX_PAGE;

/// Cases per run: `LLMOBS_MODEL_CASES`, else 8 (D-R-14).
fn cases() -> u32 {
    std::env::var("LLMOBS_MODEL_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// Random ops on two projects read back exactly as the model predicts.
    #[test]
    fn storage_matches_the_model(ops in ops::ops()) {
        run_case(&ops);
    }
}

fn run_case(ops: &[Op]) {
    let mut harness = Harness::new();
    let mut model = Model::new();
    let mut clock = midnight(day(0)) + 12 * HOUR_NS;
    for (index, op) in ops.iter().enumerate() {
        apply(op, &mut harness, &mut model, &mut clock);
        match op {
            Op::Restart | Op::Crash | Op::Settle => check_full(&harness, &mut model, index),
            // The projects share trace ids, so these may hide rows of the other project too.
            Op::DeleteTraces { .. } | Op::DeleteUser { .. } | Op::Retention { .. } => {
                check_both(&harness, &model, index);
            }
            Op::ApplyDeletions { .. } => {
                check_deletions(&harness, &mut model, index);
                check_both(&harness, &model, index);
            }
            Op::DropB => {
                check_deletions(&harness, &mut model, index);
                check_dropped(&harness, &model, index);
                check_both(&harness, &model, index);
            }
            _ => match op.project() {
                Some(project) => check_quick(&harness, &model, project, index),
                None => check_both(&harness, &model, index),
            },
        }
    }
    settle(&mut harness, &mut model, &mut clock);
    check_full(&harness, &mut model, ops.len());
}

/// `check_quick` on both projects.
fn check_both(harness: &Harness, model: &Model, index: usize) {
    check_quick(harness, model, P::A, index);
    check_quick(harness, model, P::B, index);
}

/// Dropping B leaves A's reads as the model has them, and B's reads empty (spec 03 §7). B has a
/// deletion of trace 6 registered before the drop, which covers its evaluation trace 5 (trace 5
/// targets 6). The drop marks that deletion applied. Writes to B after the drop are refused.
#[test]
fn drop_keeps_project_a() {
    let ops = [
        Op::Append {
            project: P::A,
            trace: 0,
            count: 3,
            split: false,
        },
        Op::Append {
            project: P::B,
            trace: 0,
            count: 3,
            split: false,
        },
        Op::Append {
            project: P::A,
            trace: 1,
            count: 2,
            split: true,
        },
        Op::Append {
            project: P::B,
            trace: 1,
            count: 2,
            split: false,
        },
        Op::Append {
            project: P::B,
            trace: 5,
            count: 2,
            split: false,
        },
        Op::Append {
            project: P::B,
            trace: 6,
            count: 2,
            split: false,
        },
        Op::Score {
            project: P::A,
            target: 0,
            name: 0,
        },
        Op::Score {
            project: P::B,
            target: 1,
            name: 0,
        },
        Op::DeleteTraces {
            project: P::B,
            picks: vec![6],
        },
        Op::Score {
            project: P::A,
            target: 0,
            name: 0,
        },
        Op::DropB,
        Op::Append {
            project: P::B,
            trace: 0,
            count: 2,
            split: false,
        },
        Op::Score {
            project: P::B,
            target: 0,
            name: 0,
        },
        Op::DeleteScore {
            project: P::B,
            pick: 0,
        },
        Op::Settle,
    ];
    let mut harness = Harness::new();
    let mut model = Model::new();
    let mut clock = midnight(day(0)) + 12 * HOUR_NS;
    for op in &ops {
        apply(op, &mut harness, &mut model, &mut clock);
    }
    check_full(&harness, &mut model, ops.len());
    check_dropped(&harness, &model, ops.len());
    assert!(
        !model.a.expected_traces().is_empty(),
        "project A has traces to check"
    );
    assert!(
        !model.a.expected_scores().is_empty(),
        "project A has live scores to check"
    );
    assert!(
        !model.b.deletions.is_empty(),
        "project B has a deletion to check after the drop"
    );
    assert!(
        observe_spans(&harness, P::B, MAX_PAGE).is_empty(),
        "dropped project B still lists spans"
    );
    assert!(
        observe_traces(&harness, P::B, ALL_ROWS, MAX_PAGE).is_empty(),
        "dropped project B still lists traces"
    );
    assert!(
        observe_scores(&harness, P::B, MAX_PAGE).is_empty(),
        "dropped project B still lists scores"
    );
}

/// Spec 03 §7, Handoff: a request for a dropped project is stored as already applied. A deletion
/// registered on B after the drop reads `Applied` at once.
#[test]
fn deletion_of_dropped_project_is_applied() {
    let mut harness = Harness::new();
    let mut model = Model::new();
    let mut clock = midnight(day(0)) + 12 * HOUR_NS;
    for op in [
        Op::Append {
            project: P::B,
            trace: 0,
            count: 3,
            split: false,
        },
        Op::DropB,
    ] {
        apply(&op, &mut harness, &mut model, &mut clock);
    }
    let id = Uuid::now_v7();
    harness
        .block_on(harness.storage().register_deletion(Deletion {
            id,
            project: P::B.id(),
            cutoff: clock,
            scope: DeletionScope::Traces {
                trace_ids: vec![trace_id(0)],
            },
        }))
        .expect("register_deletion on a dropped project");
    let status = harness
        .block_on(harness.storage().deletion_status(id))
        .expect("deletion_status of the deletion just registered");
    assert!(
        matches!(status, DeletionStatus::Applied { .. }),
        "a deletion registered on a dropped project reads {status:?}, expected Applied"
    );
}

/// Score ops leave live scores behind, and storage reads them back as the model has them.
/// Without this, the score checks could pass on runs where every score is deleted or hidden.
#[test]
fn score_ops_produce_live_scores() {
    let ops = [
        Op::Score {
            project: P::A,
            target: 0,
            name: 0,
        },
        Op::Score {
            project: P::A,
            target: 0,
            name: 0,
        },
        // Older version, then the newest version again at a later ingest.
        Op::StaleScore {
            project: P::A,
            pick: 0,
            back: 1,
        },
        Op::StaleScore {
            project: P::A,
            pick: 0,
            back: 0,
        },
        Op::Score {
            project: P::A,
            target: 12,
            name: 1,
        },
        // Deletes the first key in key order (trace 0, name 0): version 3, deleted.
        Op::DeleteScore {
            project: P::A,
            pick: 0,
        },
        Op::Score {
            project: P::B,
            target: 1,
            name: 2,
        },
        Op::Restart,
        // Session 0, name 1 again, at version 1 (a tie with the stored version 1).
        Op::StaleScore {
            project: P::A,
            pick: 1,
            back: 2,
        },
        Op::Score {
            project: P::A,
            target: 12,
            name: 1,
        },
        Op::Score {
            project: P::A,
            target: 5,
            name: 2,
        },
        Op::StaleScore {
            project: P::B,
            pick: 0,
            back: 4,
        },
    ];
    run_case(&ops);

    let mut harness = Harness::new();
    let mut model = Model::new();
    let mut clock = midnight(day(0)) + 12 * HOUR_NS;
    for op in &ops {
        apply(op, &mut harness, &mut model, &mut clock);
    }
    let live = model.a.expected_scores().len() + model.b.expected_scores().len();
    assert!(live > 0, "the fixed score ops leave no live score");
    check_quick(&harness, &model, P::A, ops.len());
    check_quick(&harness, &model, P::B, ops.len());
}

/// The shrunk case of finding M1-1: a re-sent span of trace 7 is kept twice, and paging the span
/// list at 7 rows used to drop one copy at a page boundary (spec 01: list views de-duplicate).
#[test]
fn regression_m1_1_span_paging_with_duplicates() {
    run_case(&[
        Op::Append {
            project: P::A,
            trace: 0,
            count: 3,
            split: false,
        },
        Op::Append {
            project: P::A,
            trace: 4,
            count: 5,
            split: false,
        },
        Op::Append {
            project: P::A,
            trace: 0,
            count: 5,
            split: false,
        },
        Op::Append {
            project: P::A,
            trace: 1,
            count: 1,
            split: false,
        },
        Op::Append {
            project: P::A,
            trace: 4,
            count: 1,
            split: false,
        },
        Op::Append {
            project: P::A,
            trace: 3,
            count: 1,
            split: false,
        },
        Op::Append {
            project: P::A,
            trace: 7,
            count: 1,
            split: true,
        },
        Op::Resend {
            project: P::A,
            trace: 7,
            pick: 218,
        },
        Op::Append {
            project: P::A,
            trace: 0,
            count: 4,
            split: true,
        },
        Op::Crash,
    ]);
}

/// The first case of the retired `compaction/model.rs` regression file, on the new op set: a
/// trace re-sent after a restart and after its day was compacted.
#[test]
fn regression_compaction_restart_resend() {
    run_case(&[
        Op::Append {
            project: P::A,
            trace: 1,
            count: 1,
            split: false,
        },
        Op::Restart,
        Op::Compact { hours: 41 },
        Op::Compact { hours: 66 },
        Op::Compact { hours: 34 },
        Op::Resend {
            project: P::A,
            trace: 1,
            pick: 0,
        },
    ]);
}

/// The shrunk case of finding M1-2: a score's tombstone is dropped by the day close, and a stale
/// version of the score sent after it reads as live (spec 03 § 5, known limit). The model drops the
/// key at settle, so the stale version is not written for it.
#[test]
fn stale_score_after_day_close_of_tombstone() {
    run_case(&[
        Op::Score {
            project: P::A,
            target: 0,
            name: 0,
        },
        Op::DeleteScore {
            project: P::A,
            pick: 2,
        },
        Op::Settle,
        Op::StaleScore {
            project: P::A,
            pick: 106,
            back: 127,
        },
    ]);
}

/// M1-2 (spec 03 § 5): a compaction that day-closes the scores partition drops the tombstone of a
/// deleted score, so a stale version of that key would read as live. `StaleScore` leaves such keys
/// out, so this case makes no call. Without that rule the stale version is written and the case
/// fails.
#[test]
fn stale_score_skips_deleted_keys() {
    let ops = [
        Op::Score {
            project: P::A,
            target: 0,
            name: 0,
        },
        Op::DeleteScore {
            project: P::A,
            pick: 0,
        },
        Op::Compact { hours: 72 },
        Op::StaleScore {
            project: P::A,
            pick: 0,
            back: 1,
        },
    ];
    run_case(&ops);

    let mut harness = Harness::new();
    let mut model = Model::new();
    let mut clock = midnight(day(0)) + 12 * HOUR_NS;
    for op in &ops[..3] {
        apply(op, &mut harness, &mut model, &mut clock);
    }
    let versions = model.a.scores.values().next().map(Vec::len);
    apply(&ops[3], &mut harness, &mut model, &mut clock);
    // A call would also write the model, so an unchanged version count means no call.
    assert_eq!(
        model.a.scores.values().next().map(Vec::len),
        versions,
        "StaleScore wrote a version of a deleted key"
    );
}
