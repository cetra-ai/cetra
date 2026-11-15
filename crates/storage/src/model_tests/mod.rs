//! Track R M1: random operations against `Storage` and an in-memory model of
//! every read (docs/plans/r-hardening/01-m1-model.md).

mod check;
mod harness;
mod hook_crash;
mod model;
mod observe;
mod ops;
mod shape;

use std::sync::Arc;
use std::time::Duration;

use proptest::prelude::*;

use check::{check_deletions, check_dropped, check_full, check_quick};
use harness::{Harness, MODEL_PUT_SIZES};
use model::Model;
use observe::{ALL_ROWS, observe_scores, observe_spans, observe_traces};
use ops::{Op, apply, new_model, settle};
use shape::{HOUR_NS, P, day, midnight, trace_id};
use uuid::Uuid;

use crate::config::{StorageConfig, WalSync};
use crate::deletion::{Deletion, DeletionScope, DeletionStatus};
use crate::fs::lossy::CrashMode;
use crate::reads::MAX_PAGE;
use crate::test_support::{Call, Rates};
use object_store::memory::InMemory;

/// Cases per run: `LLMOBS_MODEL_CASES`, else 8 (D-R-14).
fn cases() -> u32 {
    std::env::var("LLMOBS_MODEL_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8)
}

/// Cases of the interval variant: half of `cases()`, at least 2 (R2.9).
fn interval_cases() -> u32 {
    (cases() / 2).max(2)
}

/// Cases of the S3 mode: `LLMOBS_MODEL_S3_CASES`, else 4 (R4.4).
fn s3_cases() -> u32 {
    std::env::var("LLMOBS_MODEL_S3_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(4)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// Random ops on two projects read back exactly as the model predicts, with lossy crashes.
    #[test]
    fn storage_matches_the_model(ops in ops::ops()) {
        run_case(&ops);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(interval_cases()))]

    /// `wal.sync = "1h"`: a lossy crash may drop a suffix of each WAL (R2.9).
    #[test]
    fn storage_matches_the_model_in_interval_mode(ops in ops::ops()) {
        run_case_with(&ops, interval_mode);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(s3_cases()))]

    /// The model test on a remote store behind the read cache (R4.4).
    #[test]
    fn storage_matches_the_model_on_s3(ops in ops::ops_s3()) {
        run_case_s3(&ops, None);
    }
}

/// `wal.sync` one hour: the writer syncs only when it must (rotation, shutdown, open), which
/// maximizes what a lossy crash may drop.
fn interval_mode(config: &mut StorageConfig) {
    config.wal.sync = WalSync::Interval(Duration::from_secs(3_600));
}

fn run_case(ops: &[Op]) {
    run_case_with(ops, |_| {});
}

/// Runs `ops` and checks every read. Returns the harness, so a test can read what its lossy
/// crashes did.
fn run_case_with(ops: &[Op], change: impl FnOnce(&mut StorageConfig)) -> Harness {
    run_with(Harness::with_config(change), ops)
}

/// Runs `ops` in S3 mode (R4.4): the store is an in-memory bucket behind a chaos store, with
/// `MODEL_PUT_SIZES`. `faults`: `Some((seed, rates))` sets the fault schedule (`set_schedule`),
/// left inactive, so no fault fires until R4.5 activates it. Returns the harness.
fn run_case_s3(ops: &[Op], faults: Option<(u64, Rates)>) -> Harness {
    let harness = Harness::new_s3(Arc::new(InMemory::new()), MODEL_PUT_SIZES);
    if let Some((seed, rates)) = faults {
        harness
            .chaos()
            .expect("an S3-mode harness has a chaos store")
            .set_schedule(seed, rates);
    }
    run_with(harness, ops)
}

/// The loop shared by both modes: each op, then the checks its kind needs, then `settle`.
fn run_with(mut harness: Harness, ops: &[Op]) -> Harness {
    let mut model = new_model(&harness);
    let mut clock = midnight(day(0)) + 12 * HOUR_NS;
    for (index, op) in ops.iter().enumerate() {
        apply(op, &mut harness, &mut model, &mut clock);
        match op {
            Op::Restart | Op::Crash | Op::LossyCrash { .. } | Op::Settle => {
                check_full(&harness, &mut model, index);
            }
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
    harness
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
    let mut model = new_model(&harness);
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
    let mut model = new_model(&harness);
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
    let mut model = new_model(&harness);
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
    let mut model = new_model(&harness);
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

/// Audit item 2 in model form (R2.3): GC's unlinks stay undone-proof through a lossy crash. Three
/// flushed lives, a compaction and a GC, then a crash that drops every unsynced change.
#[test]
fn regression_lossy_crash_after_gc() {
    let append = || Op::Append {
        project: P::A,
        trace: 1,
        count: 2,
        split: false,
    };
    run_case(&[
        append(),
        Op::Restart,
        append(),
        Op::Restart,
        append(),
        Op::Restart,
        Op::Compact { hours: 30 },
        Op::Gc,
        Op::LossyCrash { seed: 0 },
    ]);
}

/// A lossy crash in interval mode may tear the WAL tail: the second record may be cut or lost
/// (R2.9). `TearLast` keeps the names and cuts the last unsynced write.
#[test]
fn regression_lossy_crash_tears_the_wal_tail() {
    let harness = run_case_with(
        &[
            Op::Append {
                project: P::A,
                trace: 0,
                count: 3,
                split: false,
            },
            Op::Append {
                project: P::A,
                trace: 0,
                count: 3,
                split: false,
            },
            Op::LossyCrash { seed: 2 },
        ],
        interval_mode,
    );
    // Seed 2 is `TearLast`: the second append's write must have been cut, so the reopen
    // replayed fewer records than were appended. Without this the test could pass on a crash
    // that lost nothing.
    let [crash] = harness.crashes() else {
        panic!("expected one lossy crash, got {}", harness.crashes().len());
    };
    assert_eq!(crash.mode, CrashMode::TearLast);
    assert!(crash.torn_writes > 0, "the crash tore no write: {crash:?}");
}

/// S3 mode without faults (R4.4): a schedule that is set but never activated changes nothing.
#[test]
fn a_schedule_without_activation_changes_nothing() {
    let harness = run_case_s3(
        &[
            Op::Append {
                project: P::A,
                trace: 0,
                count: 4,
                split: true,
            },
            Op::Restart,
            Op::Compact { hours: 60 },
            Op::Score {
                project: P::A,
                target: 0,
                name: 0,
            },
            Op::Crash,
            Op::Settle,
        ],
        Some((1, Rates::MODEL)),
    );
    let chaos = harness
        .chaos()
        .expect("an S3-mode harness has a chaos store");
    assert_eq!(chaos.injected(), 0, "an inactive schedule injected a fault");
    assert!(!chaos.is_active(), "the schedule was activated");
}

/// S3 mode uploads both ways (R4.4): a file of more than `single_max` is multipart, and a
/// smaller one is a single put. Twelve spans of six traces make files of both sizes after the
/// restart and compaction.
#[test]
fn s3_mode_uses_both_upload_paths() {
    let mut ops: Vec<Op> = (0..6)
        .map(|trace| Op::Append {
            project: P::A,
            trace,
            count: 12,
            split: false,
        })
        .collect();
    ops.extend([Op::Restart, Op::Compact { hours: 60 }, Op::Settle]);
    let harness = run_case_s3(&ops, None);
    let chaos = harness
        .chaos()
        .expect("an S3-mode harness has a chaos store");
    assert!(chaos.count(Call::Put) > 0, "no single put was made");
    assert!(
        chaos.count(Call::Complete) > 0,
        "no multipart upload was completed"
    );
}
