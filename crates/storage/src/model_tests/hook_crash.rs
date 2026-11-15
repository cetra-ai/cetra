//! Track R M2, R2.11: a model case crashed inside its last operation, at a hook point, in a child
//! process (docs/plans/r-hardening/02-m2-durability.md § R2.11).
//!
//! The child re-executes a case's ops, built from its number, and aborts at a hook point of the
//! last op. The parent replays the same prefix on a scratch harness to build the oracle, reopens
//! the child's directory, and checks every read against the model.

use std::ffi::OsStr;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;

use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

use super::check::check_full;
use super::harness::Harness;
use super::ops::{self, MICRO_NS, Op, apply, new_model, settle};
use super::shape::{HOUR_NS, P, day, midnight};
use crate::crash_tests::{DELETION_POINTS, spawn_child};
use crate::hooks::HookPoint;

/// `SIGABRT` on Linux and macOS: what `process::abort` ends the child with.
const SIGABRT: i32 = 6;

/// The exit code of a child whose last op returned without reaching its hook point.
const COMPLETED: i32 = 3;

/// Cases per run: `LLMOBS_HOOK_CRASH_CASES`, else 8.
fn hook_cases() -> u64 {
    std::env::var("LLMOBS_HOOK_CRASH_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8)
}

/// A case: its prefix, and its last op with the hook point the child aborts at.
struct HookCase {
    prefix: Vec<Op>,
    last: Op,
    point: HookPoint,
}

/// Every last op with each hook point it reaches. A point the op never reaches (no job to run)
/// lets the child exit with `COMPLETED` instead, which the parent accepts.
fn last_ops() -> Vec<(Op, HookPoint)> {
    let restart = [
        HookPoint::FlushAfterFreeze,
        HookPoint::FlushAfterStage,
        HookPoint::ObjectSync,
        HookPoint::ObjectRename,
        HookPoint::ObjectDirSync,
        HookPoint::FlushAfterWrite,
        HookPoint::CatalogCommit,
        HookPoint::FlushAfterCommit,
        HookPoint::FlushAfterTruncate,
    ];
    let compact = [
        HookPoint::CompactionAfterStage,
        HookPoint::ObjectSync,
        HookPoint::ObjectRename,
        HookPoint::ObjectDirSync,
        HookPoint::CompactionAfterWrite,
        HookPoint::CatalogCommit,
        HookPoint::CompactionAfterCommit,
    ];
    let gc = [
        HookPoint::GcObsoleteBeforeDelete,
        HookPoint::GcObsoleteAfterDelete,
    ];
    let mut last = Vec::new();
    last.extend(restart.map(|point| (Op::Restart, point)));
    last.extend(compact.map(|point| (Op::Compact { hours: 30 }, point)));
    last.extend(
        DELETION_POINTS
            .iter()
            .map(|(_, point)| (Op::ApplyDeletions { hours: 1 }, *point)),
    );
    last.extend(gc.map(|point| (Op::Gc, point)));
    last.push((
        Op::Retention {
            project: P::A,
            keep: 1,
        },
        HookPoint::CatalogCommit,
    ));
    last.push((Op::DropB, HookPoint::CatalogCommit));
    last
}

/// Case `n`, built with proptest's ChaCha RNG seeded from `n`, so the parent and the child build the
/// same case. Nothing shrinks: the cases are a list of seeds.
fn case(n: u64) -> HookCase {
    let mut seed = [0_u8; 32];
    seed[..8].copy_from_slice(&n.to_le_bytes());
    let rng = TestRng::from_seed(RngAlgorithm::ChaCha, &seed);
    let mut runner = TestRunner::new_with_rng(Config::default(), rng);
    let strategy = (ops::ops(), prop::sample::select(last_ops()));
    let (prefix, (last, point)) = strategy
        .new_tree(&mut runner)
        .expect("a case from its seed")
        .current();
    HookCase {
        prefix,
        last,
        point,
    }
}

/// The clock of a case before its first op (as `model_tests::run_case_with`).
fn start() -> i64 {
    midnight(day(0)) + 12 * HOUR_NS
}

/// The child of `every_model_hook_crash_reads_back`: runs case `LLMOBS_HOOK_CRASH_CASE` in the
/// directory `LLMOBS_HOOK_CRASH_DIR`, then aborts at the hook point of its last op. With
/// `LLMOBS_LOSSY_SEED` set, the abort first applies that lossy crash to the directory (R2.2).
#[test]
#[ignore = "run by every_model_hook_crash_reads_back"]
fn hook_crash_child() {
    let Ok(dir) = std::env::var("LLMOBS_HOOK_CRASH_DIR") else {
        return;
    };
    let number: u64 = std::env::var("LLMOBS_HOOK_CRASH_CASE")
        .expect("LLMOBS_HOOK_CRASH_CASE is set with the directory")
        .parse()
        .expect("LLMOBS_HOOK_CRASH_CASE is a case number");
    let shadow = std::env::var_os("LLMOBS_LOSSY_SHADOW").expect("LLMOBS_LOSSY_SHADOW is set");
    let seed = std::env::var("LLMOBS_LOSSY_SEED")
        .ok()
        .map(|seed| seed.parse().expect("LLMOBS_LOSSY_SEED is a number"));
    let HookCase {
        prefix,
        last,
        point,
    } = case(number);
    let mut harness = Harness::attach(Path::new(&dir), Path::new(&shadow), seed);
    let mut model = new_model(&harness);
    let mut clock = start();
    for op in &prefix {
        apply(op, &mut harness, &mut model, &mut clock);
    }
    harness.storage().hooks().abort_at(point);
    apply(&last, &mut harness, &mut model, &mut clock);
    // The last op returned without reaching `point`. Exit without shutdown, as a crash would.
    std::process::exit(COMPLETED);
}

/// Every case crashes a child inside its last op, and the directory reads back as the model
/// predicts, before and after settling (R2.11). Ignored: it spawns processes, and `just test`
/// runs it in its own test process.
#[test]
#[ignore = "spawns processes; `just test` runs it in its own test process"]
fn every_model_hook_crash_reads_back() {
    for number in 0..hook_cases() {
        check_case(number);
    }
}

/// Prints a case's description when the test panics while the guard lives, so a failure names the
/// case, its last op and point, and whether a lossy crash ran.
struct OnPanic(String);

impl Drop for OnPanic {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("hook crash {}", self.0);
        }
    }
}

fn check_case(number: u64) {
    let HookCase {
        prefix,
        last,
        point,
    } = case(number);
    let lossy = number % 2 == 1;
    let number_text = number.to_string();
    let tmp = tempfile::TempDir::new().expect("temp dir for a hook crash case");
    let data = tmp.path().join("data");
    let shadow = tmp.path().join("shadow");
    let mut extra: Vec<(&str, &OsStr)> = vec![("LLMOBS_LOSSY_SHADOW", shadow.as_os_str())];
    if lossy {
        extra.push(("LLMOBS_LOSSY_SEED", OsStr::new(&number_text)));
    }
    let status = spawn_child(
        "model_tests::hook_crash::hook_crash_child",
        "LLMOBS_HOOK_CRASH_DIR",
        "LLMOBS_HOOK_CRASH_CASE",
        &number_text,
        &data,
        &extra,
    );
    let reached = match (status.code(), status.signal()) {
        (None, Some(SIGABRT)) => true,
        (Some(COMPLETED), None) => false,
        _ => panic!(
            "hook crash case {number}: child ended with {status}, expected an abort or exit code {COMPLETED}"
        ),
    };
    let _context = OnPanic(format!(
        "case {number}: last op {last:?} at {point:?}, lossy crash {}",
        lossy && reached
    ));

    // The oracle: the same prefix on a scratch harness, then the last op's model rule. An op that
    // aborted before its catalog commit (`Retention`, `DropB`) did not happen; every other op
    // leaves the model as it is whether it finished or not (R1.3, R1.5), so it is simply run.
    let mut oracle = Harness::new();
    let mut model = new_model(&oracle);
    let mut clock = start();
    for op in &prefix {
        apply(op, &mut oracle, &mut model, &mut clock);
    }
    if reached && matches!(last, Op::Retention { .. } | Op::DropB) {
        clock += MICRO_NS;
    } else {
        apply(&last, &mut oracle, &mut model, &mut clock);
    }
    drop(oracle);

    // The reopen gets a shadow of its own: the child's shadow names are not free in a new root.
    let mut harness = Harness::attach(&data, &tmp.path().join("reopen-shadow"), None);
    check_full(&harness, &mut model, prefix.len());
    settle(&mut harness, &mut model, &mut clock);
    check_full(&harness, &mut model, prefix.len() + 1);
}
