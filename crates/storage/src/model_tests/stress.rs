//! Track R M6 (R6.7): the concurrent model test for minutes, with many lanes, readers and workers
//! (docs/plans/r-hardening/06-m6-concurrency.md § R6.7). Run it with `just stress [secs]`.
//!
//! The run is cut into epochs of `EPOCH_ROUNDS` rounds. An epoch starts from an empty life: every
//! lane writes its own pair of projects while the readers check every read, and the barriers
//! between rounds restart and crash the life. Each epoch ends with `settle_concurrent`, which
//! checks the files and the engine's clean state. The op runners carry over between epochs, so
//! every epoch draws new op lists.

use std::sync::atomic::Ordering::SeqCst;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use proptest::strategy::{Strategy, ValueTree};
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

use super::check::check_full;
use super::concurrent::{
    ConcurrentOptions, KINDS, Lane, LaneState, Stats, concurrent_remote_tweaks, is_barrier,
    run_segment, settle_concurrent,
};
use super::harness::{Harness, HarnessOptions, concurrent_tweaks, memory_store};
use super::ops::{MICRO_NS, Op, crashed, flushed, new_model, ops_for};
use super::shape::{HOUR_NS, P, day, midnight};
use crate::config::StorageConfig;

/// Rounds per epoch. Round 2 restarts the life, round 4 crashes it, and round 5 ends the epoch
/// with `settle_concurrent`.
const EPOCH_ROUNDS: usize = 5;

/// Runs of at least this long must exercise every read kind, GC against live readers and reads
/// that overlap a step.
const LONG_RUN: Duration = Duration::from_secs(60);

/// The seed of a lane's op runner: the run's seed mixed with the lane number, as 32 bytes.
fn seed_bytes(seed: u64, lane: u8) -> [u8; 32] {
    let word = (seed ^ u64::from(lane).wrapping_mul(0x9E37_79B9_7F4A_7C15)).to_le_bytes();
    let mut bytes = [0u8; 32];
    for chunk in bytes.chunks_exact_mut(word.len()) {
        chunk.copy_from_slice(&word);
    }
    bytes
}

/// One op list for a lane's pair, without barriers (the rounds place those).
fn draw_ops(runner: &mut TestRunner, pair: (P, P)) -> Vec<Op> {
    let ops = ops_for(pair.0, pair.1)
        .new_tree(runner)
        .expect("op tree")
        .current();
    ops.into_iter().filter(|op| !is_barrier(op)).collect()
}

/// Reads one `LLMOBS_STRESS_*` variable, or `default` when it is unset. A value that does not
/// parse fails the test.
fn setting<T: std::str::FromStr>(name: &str, default: T) -> T {
    match std::env::var(name) {
        Ok(text) => text
            .parse()
            .unwrap_or_else(|_| panic!("{name} must be a valid value, got {text:?}")),
        Err(_) => default,
    }
}

/// The wall-clock time in nanoseconds: the default seed.
fn wall_clock_ns() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    u64::try_from(nanos).unwrap_or(0)
}

/// The barriers of one round count, from 1: restart in round 2, crash in round 4.
fn barrier(round: usize, harness: &mut Harness, lanes: &mut [LaneState]) {
    match round {
        2 => {
            harness.restart();
            for lane in lanes.iter_mut() {
                lane.clock += MICRO_NS;
                flushed(harness, &mut lane.model);
            }
        }
        4 => {
            harness.crash();
            for lane in lanes.iter_mut() {
                lane.clock += MICRO_NS;
                crashed(harness, &mut lane.model);
            }
        }
        _ => {}
    }
}

/// Several lanes write at once while readers check every read. Release mode, minutes long:
/// `just stress [secs]`. Every `LLMOBS_STRESS_*` variable is printed at the start, with the seed.
#[test]
#[ignore = "minutes long; run with just stress"]
fn concurrent_lanes_match_the_model() {
    let secs: u64 = setting("LLMOBS_STRESS_SECS", 300);
    let lanes_count: usize = setting("LLMOBS_STRESS_LANES", 6);
    let readers: usize = setting("LLMOBS_STRESS_READERS", 8);
    let workers: usize = setting("LLMOBS_STRESS_WORKERS", 8);
    let remote_flag: String = setting("LLMOBS_STRESS_REMOTE", "0".to_owned());
    let seed: u64 = setting("LLMOBS_STRESS_SEED", wall_clock_ns());
    assert!(
        secs > 0,
        "LLMOBS_STRESS_SECS is {secs}; it must be at least 1"
    );
    assert!(
        readers > 0,
        "LLMOBS_STRESS_READERS is {readers}; it must be at least 1"
    );
    assert!(
        workers > 0,
        "LLMOBS_STRESS_WORKERS is {workers}; it must be at least 1"
    );
    assert!(
        (1..=16).contains(&lanes_count),
        "LLMOBS_STRESS_LANES is {lanes_count}; it must be 1 to 16 (32 projects)"
    );
    let remote = match remote_flag.as_str() {
        "0" => false,
        "1" => true,
        other => panic!(
            "LLMOBS_STRESS_REMOTE is {other:?}; it must be 0 (the local store) or 1 (the \
             in-memory store)"
        ),
    };
    let duration = Duration::from_secs(secs);
    eprintln!(
        "stress: secs {secs}, lanes {lanes_count}, readers {readers}, workers {workers}, \
         remote {remote}, seed {seed}"
    );
    let options = ConcurrentOptions {
        readers,
        pace: Duration::ZERO,
        step_hold: Duration::from_millis(2),
        gc_every: Duration::from_millis(20),
        history_keep: 512,
        remote,
    };
    let tweaks: fn(&mut StorageConfig) = if remote {
        concurrent_remote_tweaks
    } else {
        concurrent_tweaks
    };
    let lane_count = u8::try_from(lanes_count).expect("at most 16 lanes");
    let mut runners: Vec<TestRunner> = (0..lane_count)
        .map(|lane| {
            let rng = TestRng::from_seed(RngAlgorithm::ChaCha, &seed_bytes(seed, lane));
            TestRunner::new_with_rng(Config::default(), rng)
        })
        .collect();
    let stats = Stats::default();
    let mut lane_steps = vec![0u64; lanes_count];
    let started = Instant::now();
    let mut epochs = 0usize;
    let mut rounds = 0u64;
    while started.elapsed() < duration {
        epochs += 1;
        let epoch_started = Instant::now();
        let mut harness = Harness::with_options(
            HarnessOptions {
                workers,
                store: remote.then(memory_store),
                ..HarnessOptions::default()
            },
            tweaks,
        );
        let mut lanes: Vec<LaneState> = (0..lane_count)
            .map(|lane| {
                let model = new_model(&harness, P::pair(lane));
                LaneState {
                    shared: Lane::new(&model),
                    model,
                    clock: midnight(day(0)) + 12 * HOUR_NS,
                }
            })
            .collect();
        let mut round = 0;
        while round < EPOCH_ROUNDS && started.elapsed() < duration {
            round += 1;
            rounds += 1;
            let op_lists: Vec<Vec<Op>> = runners
                .iter_mut()
                .enumerate()
                .map(|(lane, runner)| {
                    draw_ops(
                        runner,
                        P::pair(u8::try_from(lane).expect("lane fits in u8")),
                    )
                })
                .collect();
            let slices: Vec<&[Op]> = op_lists.iter().map(Vec::as_slice).collect();
            run_segment(
                &harness,
                &mut lanes,
                &slices,
                &options,
                seed.wrapping_add(rounds),
                &stats,
            );
            eprintln!(
                "stress: epoch {epochs}, round {round}, seed {seed}, {:?} elapsed",
                started.elapsed()
            );
            if round < EPOCH_ROUNDS {
                barrier(round, &mut harness, &mut lanes);
                for lane in lanes.iter_mut() {
                    check_full(&harness, &mut lane.model, round);
                }
            }
        }
        // The steps of the epoch, before the settle publishes its state.
        for (total, lane) in lane_steps.iter_mut().zip(&lanes) {
            *total += lane.shared.steps() as u64;
        }
        settle_concurrent(&mut harness, &mut lanes, &options, &stats);
        for lane in lanes.iter_mut() {
            check_full(&harness, &mut lane.model, round);
        }
        eprintln!(
            "stress: epoch {epochs} done after {round} rounds, {:?} for the epoch, {:?} elapsed \
             in all, seed {seed}\nstress: stats {stats:?}",
            epoch_started.elapsed(),
            started.elapsed(),
        );
        drop(lanes);
        drop(harness);
    }
    let total_reads = stats.matched_reads();
    let elapsed = started.elapsed();
    eprintln!("stress: steps per lane {lane_steps:?}");
    eprintln!("stress: final stats {stats:?}");
    eprintln!(
        "stress: {epochs} epochs, {rounds} rounds, {total_reads} matched reads, {elapsed:?}, \
         seed {seed}"
    );
    if elapsed >= LONG_RUN {
        for (kind, _) in KINDS {
            assert!(
                stats.reads[kind as usize].load(SeqCst) > 0,
                "read kind {kind:?} never matched in {elapsed:?}: the read mix is broken"
            );
        }
    }
    let stale = stats.stale.load(SeqCst);
    assert!(
        stale * 100 <= total_reads,
        "{stale} reads fell out of the kept history of {} states: raise history_keep",
        options.history_keep
    );
    let busy = stats.busy.load(SeqCst);
    assert!(
        busy * 100 <= total_reads,
        "{busy} reads were refused as busy: report the query memory figures"
    );
    if elapsed >= LONG_RUN {
        assert!(
            stats.gc_deleted.load(SeqCst) > 0,
            "GC deleted no obsolete file in {elapsed:?}"
        );
        assert!(
            stats.matched_later.load(SeqCst) > 0,
            "no read matched a state after its lo in {elapsed:?}: the step hold does not overlap reads"
        );
    }
}
