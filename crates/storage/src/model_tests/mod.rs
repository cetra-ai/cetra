//! Track R M1: random operations against `Storage` and an in-memory model of
//! every read (docs/plans/r-hardening/01-m1-model.md).

mod check;
mod concurrent;
mod harness;
mod hook_crash;
mod model;
mod observe;
mod ops;
mod resources;
mod shape;
mod stress;

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use proptest::prelude::*;

use check::{check_deletions, check_dropped, check_full, check_no_leaked_uploads, check_quick};
use harness::{
    Harness, HarnessOptions, MODEL_PUT_SIZES, REMOTE_PUT_SIZES, memory_store, s3_tweaks,
};
use model::Model;
use observe::{ALL_ROWS, context, observe_scores, observe_spans, observe_traces};
use ops::{Op, apply, new_model, settle};
use shape::{HOUR_NS, P, day, midnight, trace_id};
use uuid::Uuid;

use crate::config::{StorageConfig, WalSync};
use crate::deletion::{Deletion, DeletionScope, DeletionStatus};
use crate::fs::lossy::CrashMode;
use crate::objects::PutSizes;
use crate::reads::MAX_PAGE;
use crate::test_support::{Call, ChaosStore, Rates};
use bytes::Bytes;
use futures::TryStreamExt;
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::prefix::PrefixStore;
use object_store::{ObjectStore, ObjectStoreExt};

/// Cases per run: `CETRA_MODEL_CASES`, else 8 (D-R-14).
fn cases() -> u32 {
    std::env::var("CETRA_MODEL_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8)
}

/// Cases of the interval variant: half of `cases()`, at least 2 (R2.9).
fn interval_cases() -> u32 {
    (cases() / 2).max(2)
}

/// Cases of the S3 mode: `CETRA_MODEL_S3_CASES`, else 8 (R4.5). A
/// restart's shutdown switches the fault schedule off 3 s in, so a flush that keeps failing
/// commits on its next retry. Measured in debug at 8 cases over six runs: 11 s to 32 s.
fn s3_cases() -> u32 {
    std::env::var("CETRA_MODEL_S3_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    /// Random ops on two projects read back exactly as the model predicts, with lossy crashes.
    #[test]
    fn storage_matches_the_model(ops in ops::ops_for(P::A, P::B)) {
        run_case(&ops, (P::A, P::B));
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(interval_cases()))]

    /// `wal.sync = "1h"`: a lossy crash may drop a suffix of each WAL (R2.9).
    #[test]
    fn storage_matches_the_model_in_interval_mode(ops in ops::ops_for(P::A, P::B)) {
        run_case_with(&ops, interval_mode, (P::A, P::B));
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(s3_cases()))]

    /// The model test on a remote store behind the read cache, with injected S3 faults (R4.4,
    /// R4.5). The seed is not shrunk: a failure is reported with its journal, and R4.10 scripts it.
    ///
    /// A `no_shrink` seed does not reproduce after shrinking: the schedule draws in the order of the
    /// calls, and a shorter op list makes other calls, so the same seed injects other faults. To
    /// replay a failure, script the faults of the printed journal (R4.10), not the seed.
    #[test]
    fn storage_matches_the_model_on_s3(ops in ops::ops_s3(), seed in any::<u64>().no_shrink()) {
        if remote_mode() {
            run_case_s3_remote(&ops, seed, (P::A, P::B));
        } else {
            let harness = run_case_s3(
                &ops,
                Some((seed, Rates::MODEL)),
                Arc::new(InMemory::new()),
                MODEL_PUT_SIZES,
             (P::A, P::B),);
            record_stats(&harness);
        }
    }
}

/// `wal.sync` one hour: the writer syncs only when it must (rotation, shutdown, open), which
/// maximizes what a lossy crash may drop.
fn interval_mode(config: &mut StorageConfig) {
    config.wal.sync = WalSync::Interval(Duration::from_secs(3_600));
}

/// Runs `ops` on the lane of projects `pair` (lane 0 is `(P::A, P::B)`).
fn run_case(ops: &[Op], pair: (P, P)) {
    run_case_with(ops, |_| {}, pair);
}

/// Runs `ops` and checks every read. Returns the harness, so a test can read what its lossy
/// crashes did.
fn run_case_with(ops: &[Op], change: impl FnOnce(&mut StorageConfig), pair: (P, P)) -> Harness {
    run_with(
        Harness::with_options(HarnessOptions::default(), change),
        ops,
        None,
        pair,
    )
}

/// `CETRA_MODEL_S3_REMOTE=1` (R4.8): the S3-mode case runs against the bucket of the
/// `CETRA_TEST_S3_*` variables instead of an in-memory one.
fn remote_mode() -> bool {
    std::env::var("CETRA_MODEL_S3_REMOTE").is_ok_and(|value| value == "1")
}

/// One S3-mode case against the bucket (R4.8), in a prefix of its own, with `REMOTE_PUT_SIZES`.
/// The case's objects are deleted afterwards, whether the case passed or not. Its abandoned uploads
/// (a failed abort, a lost create answer, a cancelled task) cannot be listed through `object_store`:
/// the bucket's lifecycle rule removes them (`docs/self-hosting/s3.md`).
fn run_case_s3_remote(ops: &[Op], seed: u64, pair: (P, P)) {
    let prefix = format!("model-{}-{seed:016x}", now_ms());
    let inner: Arc<dyn ObjectStore> = Arc::new(PrefixStore::new(
        remote_bucket(),
        ObjectPath::from(prefix.as_str()),
    ));
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let harness = run_case_s3(
            ops,
            Some((seed, Rates::MODEL)),
            inner,
            REMOTE_PUT_SIZES,
            pair,
        );
        record_stats(&harness);
    }));
    let deleted = block_on_fresh(delete_prefix(&remote_bucket(), &prefix));
    if let Err(panic) = outcome {
        resume_unwind(panic);
    }
    deleted.expect("delete the objects of an S3 model case");
}

/// The bucket of a`CETRA_MODEL_S3_REMOTE=1` run: `CETRA_TEST_S3_ENDPOINT` and
/// `CETRA_TEST_S3_BUCKET`, with the region and addressing style `tests/s3.rs` reads, and the
/// `AWS_*` credentials from the environment.
fn remote_bucket() -> AmazonS3 {
    let endpoint = std::env::var("CETRA_TEST_S3_ENDPOINT")
        .expect("CETRA_TEST_S3_ENDPOINT is set when CETRA_MODEL_S3_REMOTE=1");
    let bucket = std::env::var("CETRA_TEST_S3_BUCKET")
        .expect("CETRA_TEST_S3_BUCKET is set when CETRA_MODEL_S3_REMOTE=1");
    let region = std::env::var("CETRA_TEST_S3_REGION").unwrap_or_else(|_| "us-east-1".to_owned());
    let path_style = !matches!(
        std::env::var("CETRA_TEST_S3_PATH_STYLE").as_deref(),
        Ok("false")
    );
    AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .with_endpoint(&endpoint)
        .with_region(region)
        .with_virtual_hosted_style_request(!path_style)
        .with_allow_http(endpoint.starts_with("http://"))
        .build()
        .expect("an S3 client for the CETRA_TEST_S3_* bucket")
}

/// Deletes every object under `prefix`. It lists the bucket itself, not a chaos store, so the
/// listing is not counted as a bucket listing (`check_full`, R4.4).
async fn delete_prefix(bucket: &AmazonS3, prefix: &str) -> Result<(), String> {
    let root = ObjectPath::from(prefix);
    let locations: Vec<ObjectPath> = bucket
        .list(Some(&root))
        .map_ok(|meta| meta.location)
        .try_collect()
        .await
        .map_err(|error| error.to_string())?;
    for location in locations {
        bucket
            .delete(&location)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn block_on_fresh<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime for the cleanup")
        .block_on(future)
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

/// Runs `ops` in S3 mode (R4.4): the store is `inner` behind a chaos store, with `put_sizes`
/// (`MODEL_PUT_SIZES` in memory, `REMOTE_PUT_SIZES` on a bucket). `faults`: `Some((seed, rates))`
/// sets the fault schedule (`set_schedule`). The schedule is on during each op but `Settle`, and off
/// during the checks (R4.5). Returns the harness, with the schedule off.
fn run_case_s3(
    ops: &[Op],
    faults: Option<(u64, Rates)>,
    inner: Arc<dyn ObjectStore>,
    put_sizes: PutSizes,
    pair: (P, P),
) -> Harness {
    let harness = Harness::with_options(
        HarnessOptions {
            store: Some((inner, put_sizes)),
            ..HarnessOptions::default()
        },
        s3_tweaks,
    );
    let seed = faults.map(|(seed, _)| seed);
    if let Some((seed, rates)) = faults {
        harness
            .chaos()
            .expect("an S3-mode harness has a chaos store")
            .set_schedule(seed, rates);
    }
    run_with(harness, ops, seed, pair)
}

/// Switches the chaos schedule of an S3-mode harness on or off. Local mode has none.
fn set_faults(harness: &Harness, on: bool) {
    if let Some(chaos) = harness.chaos() {
        chaos.set_active(on);
    }
}

/// The fault rates of a case's boosted ops (R4.5): each rate times `1 + seed % 4`, and `before`
/// capped at 0.25, so that an op with many calls still gets through.
fn boosted_rates(base: Rates, seed: u64) -> Rates {
    let mut rates = base.scaled(1.0 + (seed % 4) as f64);
    rates.before = rates.before.min(0.25);
    rates
}

/// Whether an op runs at the boosted rates (R4.5): the ones that read or rewrite many objects.
/// Every other op, Restart and Crash included, runs at the case's base rates.
fn boosts(op: &Op) -> bool {
    matches!(
        op,
        Op::Compact { .. }
            | Op::ApplyDeletions { .. }
            | Op::Gc
            | Op::Verify { .. }
            | Op::FaultyRead { .. }
    )
}

/// The loop shared by both modes: each op, then the checks its kind needs, then `settle`. In S3
/// mode the schedule is on during each op but `Settle`, and off during the checks and the final
/// settle. The rates of an op are the boosted ones for `boosts` ops, the base ones otherwise. A
/// panic turns the schedule off, prints the journal and the seed, and resumes (R4.5).
fn run_with(mut harness: Harness, ops: &[Op], seed: Option<u64>, pair: (P, P)) -> Harness {
    let base = harness.chaos().map(ChaosStore::rates);
    let boosted = seed.zip(base).map(|(seed, base)| boosted_rates(base, seed));
    let mut model = new_model(&harness, pair);
    let mut clock = midnight(day(0)) + 12 * HOUR_NS;
    let mut index = 0_usize;
    let ran = catch_unwind(AssertUnwindSafe(|| {
        for (number, op) in ops.iter().enumerate() {
            index = number;
            if let (Some(chaos), Some(base)) = (harness.chaos(), base) {
                chaos.set_rates(boosted.filter(|_| boosts(op)).unwrap_or(base));
            }
            set_faults(&harness, !matches!(op, Op::Settle));
            apply(op, &mut harness, &mut model, &mut clock);
            set_faults(&harness, false);
            match op {
                Op::Restart | Op::Crash | Op::LossyCrash { .. } | Op::Settle => {
                    check_full(&harness, &mut model, number);
                }
                // The projects share trace ids, so these may hide rows of the other project too.
                Op::DeleteTraces { .. } | Op::DeleteUser { .. } | Op::Retention { .. } => {
                    check_both(&harness, &model, number);
                }
                Op::ApplyDeletions { .. } => {
                    check_deletions(&harness, &mut model, number);
                    check_both(&harness, &model, number);
                }
                Op::DropB => {
                    check_deletions(&harness, &mut model, number);
                    check_dropped(&harness, &model, number);
                    check_both(&harness, &model, number);
                }
                _ => match op.project() {
                    Some(project) => check_quick(&harness, &model, project, number),
                    None => check_both(&harness, &model, number),
                },
            }
        }
        index = ops.len();
        set_faults(&harness, false);
        settle(&mut harness, &mut model, &mut clock);
        check_full(&harness, &mut model, ops.len());
    }));
    if let Err(panic) = ran {
        set_faults(&harness, false);
        if let Some(chaos) = harness.chaos() {
            eprintln!(
                "S3 model case failed at op {index} with seed {seed:?}; the chaos journal \
                 (last {} entries, oldest first):\n{}",
                chaos.journal().len(),
                chaos.journal().join("\n")
            );
        }
        resume_unwind(panic);
    }
    harness
}

/// Process-wide S3-mode statistics, for `docs/research/storage/hardening.md` (R4.5).
#[derive(Debug)]
struct S3Stats {
    cases: u64,
    faults: u64,
    puts: u64,
    completes: u64,
    aborts: u64,
    abandoned_uploads: u64,
    faulty_read_errors: u64,
}

static STATS: Mutex<S3Stats> = Mutex::new(S3Stats {
    cases: 0,
    faults: 0,
    puts: 0,
    completes: 0,
    aborts: 0,
    abandoned_uploads: 0,
    faulty_read_errors: 0,
});

/// Adds one finished S3 case to `STATS`. With `CETRA_MODEL_S3_STATS=1`, prints them every 50
/// cases.
fn record_stats(harness: &Harness) {
    let Some(chaos) = harness.chaos() else {
        return;
    };
    let mut stats = STATS.lock().unwrap_or_else(PoisonError::into_inner);
    stats.cases += 1;
    stats.faults += chaos.injected();
    stats.puts += chaos.count(Call::Put);
    stats.completes += chaos.count(Call::Complete);
    stats.aborts += chaos.count(Call::Abort);
    stats.abandoned_uploads +=
        u64::try_from(chaos.abandoned_uploads().len()).expect("a count of uploads fits in u64");
    stats.faulty_read_errors += harness.faulty_read_errors.load(Ordering::Relaxed);
    if std::env::var("CETRA_MODEL_S3_STATS").is_ok_and(|value| value == "1")
        && stats.cases.is_multiple_of(50)
    {
        eprintln!("S3 model statistics: {stats:?}");
    }
}

/// `check_quick` on both projects of the lane.
fn check_both(harness: &Harness, model: &Model, index: usize) {
    for project in model.projects() {
        check_quick(harness, model, project, index);
    }
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
    let mut model = new_model(&harness, (P::A, P::B));
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
    let storage = harness.storage();
    let ctx = harness
        .block_on(context(storage, P::B))
        .expect("query context on the dropped project");
    assert!(
        harness
            .block_on(observe_spans(&ctx, MAX_PAGE))
            .expect("span_list on the dropped project")
            .is_empty(),
        "dropped project B still lists spans"
    );
    assert!(
        harness
            .block_on(observe_traces(&ctx, ALL_ROWS, MAX_PAGE))
            .expect("trace_list on the dropped project")
            .is_empty(),
        "dropped project B still lists traces"
    );
    assert!(
        harness
            .block_on(observe_scores(&ctx, MAX_PAGE))
            .expect("score_list on the dropped project")
            .is_empty(),
        "dropped project B still lists scores"
    );
}

/// Spec 03 §7, Handoff: a request for a dropped project is stored as already applied. A deletion
/// registered on B after the drop reads `Applied` at once.
#[test]
fn deletion_of_dropped_project_is_applied() {
    let mut harness = Harness::new();
    let mut model = new_model(&harness, (P::A, P::B));
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
    run_case(&ops, (P::A, P::B));

    let mut harness = Harness::new();
    let mut model = new_model(&harness, (P::A, P::B));
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
    run_case(
        &[
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
        ],
        (P::A, P::B),
    );
}

/// The first case of the retired `compaction/model.rs` regression file, on the new op set: a
/// trace re-sent after a restart and after its day was compacted.
#[test]
fn regression_compaction_restart_resend() {
    run_case(
        &[
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
        ],
        (P::A, P::B),
    );
}

/// Lane 1 (projects 2 and 3) runs M1's `regression_compaction_restart_resend` ops on its first
/// project, then writes scores, deletes them and traces and a user, and applies retention on both
/// projects, before it drops its second project and settles (R6.1). Retention never picks `D + 2`
/// (`RETENTION_DAYS`), and trace 2 (class 2) is never re-sent.
#[test]
fn a_lane_one_case_runs() {
    let lane = (P(2), P(3));
    run_case(
        &[
            Op::Append {
                project: lane.0,
                trace: 1,
                count: 1,
                split: false,
            },
            Op::Restart,
            Op::Compact { hours: 41 },
            Op::Compact { hours: 66 },
            Op::Compact { hours: 34 },
            Op::Resend {
                project: lane.0,
                trace: 1,
                pick: 0,
            },
            Op::Append {
                project: lane.1,
                trace: 0,
                count: 3,
                split: false,
            },
            Op::Append {
                project: lane.1,
                trace: 6,
                count: 2,
                split: true,
            },
            Op::Score {
                project: lane.0,
                target: 1,
                name: 0,
            },
            Op::Score {
                project: lane.0,
                target: 13,
                name: 0,
            },
            Op::Score {
                project: lane.1,
                target: 0,
                name: 1,
            },
            Op::StaleScore {
                project: lane.0,
                pick: 0,
                back: 0,
            },
            Op::DeleteScore {
                project: lane.0,
                pick: 1,
            },
            Op::DeleteTraces {
                project: lane.1,
                picks: vec![6],
            },
            Op::DeleteUser {
                project: lane.0,
                user: 0,
            },
            Op::Retention {
                project: lane.0,
                keep: 0,
            },
            Op::Retention {
                project: lane.1,
                keep: 1,
            },
            Op::DropB,
            Op::Settle,
        ],
        lane,
    );
}

/// The shrunk case of finding M1-2: a score's tombstone is dropped by the day close, and a stale
/// version of the score sent after it reads as live (spec 03 § 5, known limit). The model drops the
/// key at settle, so the stale version is not written for it.
#[test]
fn stale_score_after_day_close_of_tombstone() {
    run_case(
        &[
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
        ],
        (P::A, P::B),
    );
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
    run_case(&ops, (P::A, P::B));

    let mut harness = Harness::new();
    let mut model = new_model(&harness, (P::A, P::B));
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
    run_case(
        &[
            append(),
            Op::Restart,
            append(),
            Op::Restart,
            append(),
            Op::Restart,
            Op::Compact { hours: 30 },
            Op::Gc,
            Op::LossyCrash { seed: 0 },
        ],
        (P::A, P::B),
    );
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
        (P::A, P::B),
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

/// Found by `just model 200` (interval mode). Deletion A covers trace 11 and is applied to the
/// traces file flushed after it, which records A's sequence. Trace 11 is then re-appended after
/// A's cut-off, and deletion B (of trace 0) gates that file. The file gate must not apply A again:
/// trace 11 is visible (spec 03 §7).
#[test]
fn regression_applied_deletion_does_not_hide_a_later_trace() {
    run_case(
        &[
            Op::DeleteTraces {
                project: P::A,
                picks: vec![11],
            },
            Op::Append {
                project: P::A,
                trace: 11,
                count: 1,
                split: false,
            },
            Op::Restart,
            Op::DeleteTraces {
                project: P::A,
                picks: vec![0],
            },
        ],
        (P::A, P::B),
    );
}

/// S3 mode with a schedule (R4.5): the schedule is off once the run returns, whatever it injected.
/// A run whose boosted ops (R4.5) reach `before` 0.24 injects faults over a fixed list of Restart,
/// Compact, Verify and FaultyRead ops. The base rates stay the model's, since a shutdown retries
/// its flush until the 30 s grace period ends: at a higher base rate a Restart takes that long.
/// Crash stands in for the later restarts.
#[test]
fn a_high_rate_run_injects_faults() {
    let rates = Rates::MODEL;
    let mut ops: Vec<Op> = (0..2)
        .map(|trace| Op::Append {
            project: P::A,
            trace,
            count: 6,
            split: true,
        })
        .collect();
    // The Restart flushes the appends, so the files exist for the reads and the compaction below.
    ops.extend([
        Op::Restart,
        Op::Compact { hours: 60 },
        Op::Verify { pick: 0 },
        Op::FaultyRead {
            project: P::A,
            pick: 0,
        },
        Op::Crash,
        Op::Verify { pick: 1 },
        Op::FaultyRead {
            project: P::A,
            pick: 1,
        },
        Op::FaultyRead {
            project: P::A,
            pick: 3,
        },
        Op::Compact { hours: 60 },
    ]);
    // Seed 3 has factor 4: the boosted ops run at `before` 0.24 (0.06 times 4).
    let harness = run_case_s3(
        &ops,
        Some((3, rates)),
        Arc::new(InMemory::new()),
        MODEL_PUT_SIZES,
        (P::A, P::B),
    );
    let chaos = harness
        .chaos()
        .expect("an S3-mode harness has a chaos store");
    assert!(
        chaos.injected() > 0,
        "a run at before 0.5 injected no fault"
    );
}

/// `allow_failure` (R4.5): a failed call is allowed only when a fault was injected during it. A
/// held call fails with its fault counted, so its failure is allowed.
#[test]
fn allow_failure_needs_an_injected_fault() {
    let harness = Harness::with_options(
        HarnessOptions {
            store: Some(memory_store()),
            ..HarnessOptions::default()
        },
        s3_tweaks,
    );
    let chaos = harness
        .chaos()
        .expect("an S3-mode harness has a chaos store")
        .clone();
    let path = ObjectPath::from("tables/spans/held.parquet");

    let before = chaos.injected();
    assert!(
        !ops::allow_failure(&harness, before),
        "nothing was injected, so no failure is allowed"
    );

    chaos.hold(&[Call::Put]);
    let put = harness.block_on(chaos.put(&path, Bytes::from_static(b"x").into()));
    assert!(put.is_err(), "a held put must fail");
    assert!(
        ops::allow_failure(&harness, before),
        "the held put's fault was injected, so its failure is allowed"
    );
    chaos.release();
}

/// A multipart upload dropped outside an ending window is a leak, and the check fires on it. This
/// is what a clean shutdown's flush would leave if it dropped an upload (R4.5).
#[test]
#[should_panic(expected = "multipart uploads storage left open")]
fn check_no_leaked_uploads_fires_on_a_leak() {
    let harness = Harness::with_options(
        HarnessOptions {
            store: Some(memory_store()),
            ..HarnessOptions::default()
        },
        s3_tweaks,
    );
    let chaos = harness
        .chaos()
        .expect("an S3-mode harness has a chaos store")
        .clone();
    let upload = harness
        .block_on(chaos.put_multipart(&ObjectPath::from("tables/spans/leak.parquet")))
        .expect("start a multipart upload");
    drop(upload);
    check_no_leaked_uploads(&harness, 0);
}

#[test]
fn a_run_leaves_the_schedule_off() {
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
        Arc::new(InMemory::new()),
        MODEL_PUT_SIZES,
        (P::A, P::B),
    );
    let chaos = harness
        .chaos()
        .expect("an S3-mode harness has a chaos store");
    assert!(!chaos.is_active(), "the schedule is still on after the run");
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
    let harness = run_case_s3(
        &ops,
        None,
        Arc::new(InMemory::new()),
        MODEL_PUT_SIZES,
        (P::A, P::B),
    );
    let chaos = harness
        .chaos()
        .expect("an S3-mode harness has a chaos store");
    assert!(chaos.count(Call::Put) > 0, "no single put was made");
    assert!(
        chaos.count(Call::Complete) > 0,
        "no multipart upload was completed"
    );
}
