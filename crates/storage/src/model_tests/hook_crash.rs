//! Track R M2, R2.11: a model case crashed inside its last operation, at a hook point, in a child
//! process (docs/plans/r-hardening/02-m2-durability.md § R2.11).
//!
//! The child re-executes a case's ops, built from its number, and aborts at a hook point of the
//! last op. The parent replays the same prefix on a scratch harness to build the oracle, reopens
//! the child's directory, and checks every read against the model.

use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

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

/// The default case count: the largest that runs in under 60 s in debug (52.8 s; 26 cases take
/// 63.8 s, so the last pair of `last_ops` is not in the default run). Pairs are stratified over
/// the cases, so a lossy crash needs `last_ops().len()` cases or more.
const DEFAULT_CASES: u64 = 25;

/// Cases per run: `LLMOBS_HOOK_CRASH_CASES`, else [`DEFAULT_CASES`]. A value that is not a count
/// panics.
fn hook_cases() -> u64 {
    match std::env::var("LLMOBS_HOOK_CRASH_CASES") {
        Ok(value) => value.parse().unwrap_or_else(|error| {
            panic!("LLMOBS_HOOK_CRASH_CASES is not a case count ({error}): {value:?}")
        }),
        Err(std::env::VarError::NotPresent) => DEFAULT_CASES,
        Err(error) => panic!("LLMOBS_HOOK_CRASH_CASES: {error}"),
    }
}

/// A case: its prefix (the random ops, then the primer of its last op), and its last op with the
/// hook point the child aborts at.
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
    // The GC points of `DELETION_POINTS` belong to `Op::Gc`: a deletion pass never collects
    // obsolete files, so an ApplyDeletions op cannot reach them.
    last.extend(
        DELETION_POINTS
            .iter()
            .filter(|(_, point)| {
                !matches!(
                    point,
                    HookPoint::GcObsoleteBeforeDelete | HookPoint::GcObsoleteAfterDelete
                )
            })
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

/// The ops a case runs after its random prefix, so that its last op has work at its hook point
/// (a compaction needs closed L0 files, a deletion pass a registered deletion, GC an obsolete
/// file). It depends only on the last op, so the parent and the child build the same tail.
fn primer(last: &Op) -> Vec<Op> {
    let append = |trace| Op::Append {
        project: P::A,
        trace,
        count: 6,
        split: false,
    };
    match last {
        Op::Compact { .. } => vec![append(0), Op::Restart, append(1), Op::Restart],
        Op::ApplyDeletions { .. } => vec![
            append(0),
            Op::Restart,
            Op::DeleteTraces {
                project: P::A,
                picks: vec![0],
            },
        ],
        Op::Gc => vec![
            append(0),
            Op::Restart,
            Op::Compact { hours: 72 },
            Op::Restart,
        ],
        Op::Restart => vec![append(0)],
        Op::DropB => vec![Op::Append {
            project: P::B,
            trace: 0,
            count: 3,
            split: false,
        }],
        _ => Vec::new(),
    }
}

/// Whether case `n` gets a lossy crash: pairs are stratified, so each pair gets both kinds over the
/// cases (`(n / len) % 2`).
fn is_lossy(n: u64) -> bool {
    let len = u64::try_from(last_ops().len()).expect("a small number of pairs");
    (n / len) % 2 == 1
}

/// Case `n`. The pair is `last_ops()[n % len]`, so every pair is covered; the prefix is drawn from
/// proptest's ChaCha RNG seeded from `n`, so the parent and the child build the same case. Nothing
/// shrinks: the cases are a list of seeds.
fn case(n: u64) -> HookCase {
    let pairs = last_ops();
    let index = usize::try_from(n % u64::try_from(pairs.len()).expect("a small count"))
        .expect("an index into the pairs");
    let (last, point) = pairs[index].clone();
    let mut seed = [0_u8; 32];
    seed[..8].copy_from_slice(&n.to_le_bytes());
    let rng = TestRng::from_seed(RngAlgorithm::ChaCha, &seed);
    let mut runner = TestRunner::new_with_rng(Config::default(), rng);
    let mut prefix = ops::ops()
        .new_tree(&mut runner)
        .expect("a prefix from its seed")
        .current();
    prefix.extend(primer(&last));
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
/// runs it in its own test process. It fails when no case aborted at its hook point.
#[test]
#[ignore = "spawns processes; `just test` runs it in its own test process"]
fn every_model_hook_crash_reads_back() {
    let cases = hook_cases();
    let mut completed = Vec::new();
    for number in 0..cases {
        if !check_case(number) {
            let HookCase { last, point, .. } = case(number);
            completed.push(format!("{number} ({last:?} at {point:?})"));
        }
    }
    let reached = cases - u64::try_from(completed.len()).expect("a small count");
    eprintln!(
        "hook crash: {cases} cases, {reached} aborted at their hook point, {} completed without reaching it: {}",
        completed.len(),
        completed.join(", ")
    );
    assert!(
        reached > 0,
        "no case aborted at its hook point: every last op completed"
    );
}

/// Prints a case's description when the test panics while the guard lives, with the directory the
/// case ran in, which is kept for inspection (the child's stderr is in it).
struct OnPanic {
    context: String,
    dir: PathBuf,
}

impl Drop for OnPanic {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "hook crash {}; its directory is kept at {}",
                self.context,
                self.dir.display()
            );
        }
    }
}

/// Runs case `number` and checks it. Returns whether the child aborted at its hook point (`false`:
/// the last op completed without reaching it).
fn check_case(number: u64) -> bool {
    let HookCase {
        prefix,
        last,
        point,
    } = case(number);
    let lossy = is_lossy(number);
    let number_text = number.to_string();
    // Kept on a panic, so the failure can be inspected; removed at the end of a passing case.
    let dir = tempfile::TempDir::new()
        .expect("temp dir for a hook crash case")
        .keep();
    let mut context = OnPanic {
        context: format!(
            "case {number}: last op {last:?} at {point:?}, lossy seed requested {lossy}"
        ),
        dir: dir.clone(),
    };
    let data = dir.join("data");
    let shadow = dir.join("shadow");
    let stderr_path = dir.join("child.stderr");
    let stderr = File::create(&stderr_path).expect("a file for the child's stderr");
    let mut extra: Vec<(&str, &OsStr)> = vec![
        ("LLMOBS_LOSSY_SHADOW", shadow.as_os_str()),
        ("LLMOBS_ABORT_MARKER", OsStr::new("1")),
    ];
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
        Stdio::from(stderr),
    );
    let reached = match (status.code(), status.signal()) {
        (None, Some(SIGABRT)) => true,
        (Some(COMPLETED), None) => false,
        _ => panic!(
            "hook crash case {number}: child ended with {status}, expected an abort or exit code {COMPLETED}"
        ),
    };
    if reached {
        let marker = format!("aborting at {point:?}");
        let stderr = std::fs::read_to_string(&stderr_path).expect("the child's stderr");
        assert!(
            stderr.lines().any(|line| line.trim() == marker),
            "hook crash case {number}: the child aborted without its marker `{marker}` (stderr in {})",
            stderr_path.display()
        );
    }
    context
        .context
        .push_str(&format!(", lossy crash applied: {}", lossy && reached));

    // The oracle: the same prefix (random ops and primer) on a scratch harness, then the last op's
    // model rule. An op that aborted before its catalog commit (`Retention`, `DropB`) did not happen;
    // every other op leaves the model as it is whether it finished or not (R1.3, R1.5), so it is
    // simply run.
    let mut oracle = Harness::new();
    let mut model = new_model(&oracle);
    let mut clock = start();
    for op in &prefix {
        apply(op, &mut oracle, &mut model, &mut clock);
    }
    // `DropB` does nothing on a project that is already dropped, so it cannot reach its hook.
    if matches!(last, Op::Retention { .. }) || (matches!(last, Op::DropB) && !model.b.dropped) {
        assert!(
            reached,
            "hook crash case {number}: the last op {last:?} did not reach {point:?}"
        );
    }
    if reached && matches!(last, Op::Retention { .. } | Op::DropB) {
        clock += MICRO_NS;
    } else {
        apply(&last, &mut oracle, &mut model, &mut clock);
    }
    drop(oracle);

    // The reopen gets a shadow of its own: the child's shadow names are not free in a new root.
    let applied = prefix.len();
    let mut harness = Harness::attach(&data, &dir.join("reopen-shadow"), None);
    check_full(&harness, &mut model, applied);
    settle(&mut harness, &mut model, &mut clock);
    check_full(&harness, &mut model, applied + 1);
    drop(harness);
    std::fs::remove_dir_all(&dir).expect("remove a passing case's directory");
    reached
}
