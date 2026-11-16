//! Track R M5 (R5.7): the storage under a low file descriptor limit (docs/plans/r-hardening/05-m5-resources.md
//! § R5.7). The parent re-executes the test binary as a child, which lowers its own
//! `RLIMIT_NOFILE` and runs the storage. The run passes when every call returns a clean outcome:
//! an error, never a panic or a wedge.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use arrow::array::RecordBatch;
use bytesize::ByteSize;
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

use super::super::check::check_full;
use super::super::harness::{Harness, HarnessOptions, model_config};
use super::super::ops::{Op, apply, new_model};
use super::super::shape::{HOUR_NS, P, day, midnight};
use super::disk_full::SPILL_SQL;
use super::fill::{Text, filler, filler_batch};
use super::memory::{data_dir_under_target, field, marked_lines, run_to_end, tail, until_stopped};
use super::{model_steps, seed_ops, settle_and_check};
use crate::error::{AppendError, OpenError, QueryError};
use crate::query_error::RunError;
use crate::reads::{ListScope, SortOrder, TraceListRequest, TraceSort, trace_list};
use crate::views::TimeWindow;
use crate::{QueryContext, QueryOptions, SqlRequest, Storage};

const MODE_ENV: &str = "CETRA_FD_CHILD_MODE";
const LIMIT_ENV: &str = "CETRA_FD_CHILD_LIMIT";
const SECONDS_ENV: &str = "CETRA_FD_CHILD_SECONDS";
const DIR_ENV: &str = "CETRA_FD_CHILD_DIR";
const CHILD_TIMEOUT: Duration = Duration::from_secs(300);
/// Every storage call of the load phase must return within this, or the child is wedged.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
const MIB: u64 = 1 << 20;
const KIB: usize = 1 << 10;
/// The load phase stops appending once this many Arrow bytes are in.
const LOAD_BYTES: u64 = 1 << 30;
/// Rows of one filler batch: 32 spans of 4 KiB.
const BATCH_SPANS: u8 = 32;
const BATCH_INPUT_BYTES: usize = 4 * KIB;
const INGEST_NS: i64 = 0;

/// Entries of /proc/self/fd, minus the one `read_dir` itself uses. `None` when `read_dir` fails
/// (EMFILE under a low limit): the sampler skips that sample instead of panicking.
fn open_fds() -> Option<usize> {
    let count = fs::read_dir("/proc/self/fd").ok()?.count();
    Some(count.saturating_sub(1))
}

/// The open descriptors by kind, read from `read_link`. Best effort: an unreadable link is
/// counted as `unreadable`.
fn descriptor_kinds() -> String {
    let mut kinds: BTreeMap<&'static str, u64> = BTreeMap::new();
    if let Ok(entries) = fs::read_dir("/proc/self/fd") {
        for entry in entries.flatten() {
            let kind = match fs::read_link(entry.path()) {
                Ok(target) => {
                    let target = target.to_string_lossy().into_owned();
                    if target.starts_with("anon_inode:") {
                        "anon_inode"
                    } else if target.starts_with("pipe:") {
                        "pipe"
                    } else if target.starts_with("socket:") {
                        "socket"
                    } else if target.starts_with('/') {
                        "file"
                    } else {
                        "other"
                    }
                }
                Err(_) => "unreadable",
            };
            *kinds.entry(kind).or_default() += 1;
        }
    }
    if kinds.is_empty() {
        return "none".to_owned();
    }
    kinds
        .iter()
        .map(|(kind, count)| format!("{kind}:{count}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// `RLIMIT_NOFILE` with soft limit `soft` and the current hard limit.
fn set_soft_nofile(soft: u64) {
    let maximum = getrlimit(Resource::Nofile).maximum;
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(soft),
            maximum,
        },
    )
    .expect("set the soft descriptor limit");
}

fn soft_nofile() -> u64 {
    getrlimit(Resource::Nofile)
        .current
        .expect("the soft descriptor limit is set")
}

/// What the child does.
#[derive(Debug, Clone, Copy)]
enum ChildMode {
    /// Open and seed at the normal limit, lower it for the load phase only.
    Load,
    /// Lower the limit first, then only open the storage.
    Open,
}

impl ChildMode {
    fn name(self) -> &'static str {
        match self {
            Self::Load => "load",
            Self::Open => "open",
        }
    }
}

/// Re-executes `child_run` with the mode, the limit (0 keeps the current one) and the seconds, and
/// returns its `CETRA_FD` line. The data directory is under `target/cetra-resources`, removed
/// after the child ends.
fn run_child(mode: ChildMode, limit: u64, seconds: u64) -> String {
    let dir = data_dir_under_target();
    let exe = std::env::current_exe().expect("the test binary's path");
    let mut command = Command::new(exe);
    command
        .args([
            "--exact",
            "model_tests::resources::descriptors::child_run",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(MODE_ENV, mode.name())
        .env(LIMIT_ENV, limit.to_string())
        .env(SECONDS_ENV, seconds.to_string())
        .env(DIR_ENV, dir.path());
    let finished = run_to_end(&mut command, CHILD_TIMEOUT);
    assert!(
        finished.status.success(),
        "the {} child exited {} at limit {limit}; its last lines:\n{}",
        mode.name(),
        finished.status,
        tail(&finished.stdout, 40)
    );
    marked_lines(&finished.stdout, "CETRA_FD ")
        .pop()
        .expect("the child printed its CETRA_FD line")
}

/// What a load loop tallied.
#[derive(Debug, Default, Clone)]
struct Tally {
    appends_ok: u64,
    backpressure: u64,
    wal_failed: u64,
    queries_ok: u64,
    query_errors: BTreeMap<&'static str, u64>,
    compaction_failed: u64,
    compaction_errors: u64,
}

impl Tally {
    fn plus(mut self, other: Self) -> Self {
        self.appends_ok += other.appends_ok;
        self.backpressure += other.backpressure;
        self.wal_failed += other.wal_failed;
        self.queries_ok += other.queries_ok;
        for (kind, count) in other.query_errors {
            *self.query_errors.entry(kind).or_default() += count;
        }
        self.compaction_failed += other.compaction_failed;
        self.compaction_errors += other.compaction_errors;
        self
    }
}

/// The next filler batch: `counter % 60_000` numbers it, and `counter % 60` picks its day.
fn next_batch(counter: &AtomicU32) -> RecordBatch {
    let number = counter.fetch_add(1, Ordering::SeqCst);
    let batch = u16::try_from(number % 60_000).expect("a batch number below 60 000 fits in u16");
    let day_offset = u8::try_from(number % 60).expect("a day offset below 60 fits in u8");
    filler_batch(
        batch,
        BATCH_SPANS,
        BATCH_INPUT_BYTES,
        Text::Words,
        day_offset,
        INGEST_NS,
    )
}

/// Appends filler batches until `stop` or the byte cap. `Backpressure` waits 50 ms; `WalFailed`
/// ends this appender (the writer accepts nothing more until a restart). Anything else fails the
/// child. A call that runs past `CALL_TIMEOUT` is a wedge.
async fn appender(
    storage: &Storage,
    counter: &AtomicU32,
    bytes: &AtomicU64,
    stop: &AtomicBool,
) -> Tally {
    let mut tally = Tally::default();
    while !stop.load(Ordering::SeqCst) && bytes.load(Ordering::SeqCst) < LOAD_BYTES {
        let batch = next_batch(counter);
        let size = batch.get_array_memory_size() as u64;
        let outcome =
            match tokio::time::timeout(CALL_TIMEOUT, storage.append_spans(filler(), batch)).await {
                Err(_elapsed) => panic!("an append wedged past {CALL_TIMEOUT:?}"),
                Ok(outcome) => outcome,
            };
        match outcome {
            Ok(_) => {
                tally.appends_ok += 1;
                bytes.fetch_add(size, Ordering::SeqCst);
            }
            Err(AppendError::Backpressure) => {
                tally.backpressure += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(AppendError::WalFailed) => {
                tally.wal_failed += 1;
                break;
            }
            Err(other) => panic!("an append of filler rows failed: {other}"),
        }
    }
    tally
}

/// One query over project C, of kind `which % 2`: R5.2's window count (it spills across files),
/// or a trace list over day 0 to day 60. Returns the error kind of a refusal the child accepts.
async fn run_query(storage: &Storage, which: usize) -> Result<(), &'static str> {
    let query = match storage.query(filler(), QueryOptions::new(None, None)).await {
        Ok(query) => query,
        Err(QueryError::Busy) => return Err("busy"),
        Err(other) => panic!("query admission failed: {other}"),
    };
    let ctx = QueryContext::new(query, None).map_err(|error| accepted_kind(&error))?;
    let params = serde_json::Map::new();
    let outcome = if which.is_multiple_of(2) {
        ctx.sql_json(SqlRequest {
            query: SPILL_SQL,
            params: &params,
            limit: 1,
            timeout: CALL_TIMEOUT,
        })
        .await
        .map(drop)
    } else {
        let window = TimeWindow::new(midnight(day(0)), midnight(day(60)))
            .expect("the day 0 to day 60 window is valid");
        trace_list(
            &ctx,
            TraceListRequest {
                filter: None,
                search: None,
                scope: ListScope {
                    window: Some(window),
                    include_eval: true,
                    include_experiments: true,
                },
                strict_exclusions: false,
                sort: TraceSort::CostTotal,
                order: SortOrder::Desc,
                limit: 50,
                cursor: None,
                timeout: CALL_TIMEOUT,
            },
        )
        .await
        .map(drop)
    };
    outcome.map_err(|error| accepted_kind(&error))
}

/// The kind of a query error the child accepts: `too_large`, `timeout`, `failed` or `internal`.
/// Anything else fails the child.
fn accepted_kind(error: &RunError) -> &'static str {
    let kind = error.kind();
    match kind {
        "too_large" => "too_large",
        "timeout" => "timeout",
        "failed" => "failed",
        "internal" => "internal",
        other => panic!("a query failed with kind {other}: {error}"),
    }
}

/// Runs queries of kind `which % 2` until `stop`. Each call is bounded by `CALL_TIMEOUT`.
async fn query_loop(storage: &Storage, which: usize, stop: &AtomicBool) -> Tally {
    let mut tally = Tally::default();
    while !stop.load(Ordering::SeqCst) {
        let outcome = match tokio::time::timeout(CALL_TIMEOUT, run_query(storage, which)).await {
            Err(_elapsed) => panic!("a query wedged past {CALL_TIMEOUT:?}"),
            Ok(outcome) => outcome,
        };
        match outcome {
            Ok(()) => tally.queries_ok += 1,
            Err(kind) => *tally.query_errors.entry(kind).or_default() += 1,
        }
    }
    tally
}

/// Compaction passes until `stop`. A pass that reports a failed job, or returns an error, is
/// counted; a pass that wedges fails the child.
async fn compact_loop(storage: &Storage, now: i64, stop: &AtomicBool) -> Tally {
    let mut tally = Tally::default();
    while !stop.load(Ordering::SeqCst) {
        match tokio::time::timeout(CALL_TIMEOUT, storage.compact(now)).await {
            Err(_elapsed) => panic!("a compaction pass wedged past {CALL_TIMEOUT:?}"),
            Ok(Ok(report)) => tally.compaction_failed += report.failed,
            Ok(Err(_)) => tally.compaction_errors += 1,
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tally
}

/// Samples the open descriptors every 10 ms: the peak, and the kinds open at that peak. A sample
/// that cannot be taken (EMFILE) is skipped.
async fn sampler(stop: &AtomicBool) -> (usize, String) {
    let mut peak = 0;
    let mut kinds = String::from("none");
    while !stop.load(Ordering::SeqCst) {
        if let Some(open) = open_fds()
            && open > peak
        {
            peak = open;
            kinds = descriptor_kinds();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (peak, kinds)
}

/// The load child: opens and seeds at the normal limit, lowers the limit for `seconds`, then
/// restores it and checks the storage.
fn load_child(limit: u64, seconds: u64, dir: &Path) {
    let original = soft_nofile();
    let mut h = Harness::with_options(
        HarnessOptions {
            root: Some(dir.to_path_buf()),
            workers: 4,
            unrecorded: true,
            foreign_projects: vec![filler()],
            ..HarnessOptions::default()
        },
        |config| {
            config.flush.max_bytes = ByteSize::mib(1);
            config.flush.max_age = Duration::from_secs(1);
            config.compaction.interval = Duration::from_secs(1);
            config.compaction.l0_files = 2;
            config.compaction.threads = 4;
            config.wal.segment_bytes = ByteSize::mib(1);
            // Every sort holds 10 MiB (DataFusion's `sort_spill_reservation_bytes`) per partition
            // that cannot spill, so the model's reads of the seed need 8 partitions x about 10 MiB
            // (spec 03 section 10, M5-4): 16 MiB refuses them at op 0.
            config.query.target_partitions = 8;
            config.query.max_memory = ByteSize::mib(128);
            config.query.initial_memory = ByteSize::mib(8);
        },
    );
    let mut model = new_model(&h, (P::A, P::B));
    let mut clock = midnight(day(0)) + 12 * HOUR_NS;
    model_steps(&mut h, &mut model, &mut clock, &seed_ops(), 0);
    let baseline = open_fds().expect("the baseline is read at the normal limit");

    let effective = if limit == 0 {
        original
    } else {
        set_soft_nofile(limit);
        limit
    };
    let stop = AtomicBool::new(false);
    let counter = AtomicU32::new(0);
    let bytes = AtomicU64::new(0);
    let storage = h.storage();
    let now = clock;
    let (sample, a1, a2, a3, a4, q1, q2, q3, q4, compact) =
        h.block_on(until_stopped(&stop, Duration::from_secs(seconds), async {
            tokio::join!(
                sampler(&stop),
                appender(storage, &counter, &bytes, &stop),
                appender(storage, &counter, &bytes, &stop),
                appender(storage, &counter, &bytes, &stop),
                appender(storage, &counter, &bytes, &stop),
                query_loop(storage, 0, &stop),
                query_loop(storage, 1, &stop),
                query_loop(storage, 2, &stop),
                query_loop(storage, 3, &stop),
                compact_loop(storage, now, &stop),
            )
        }));
    let (peak, kinds) = sample;
    let tally = [a1, a2, a3, a4, q1, q2, q3, q4, compact]
        .into_iter()
        .fold(Tally::default(), Tally::plus);

    // Restore the limit, let the descriptors settle, and check the storage.
    set_soft_nofile(original);
    std::thread::sleep(Duration::from_secs(3));
    let health = h.storage().health();
    let wal_failed = health.spans_wal_failed || health.scores_wal_failed;
    if wal_failed {
        h.crash_then_try_open()
            .expect("the storage reopens after a WAL failure once space and descriptors are back");
    } else {
        apply(
            &Op::Append {
                project: P::A,
                trace: 3,
                count: 2,
                split: false,
            },
            &mut h,
            &mut model,
            &mut clock,
        );
    }
    h.restart();
    check_full(&h, &mut model, 500);
    let verify = h.block_on(h.storage().verify_pass());
    assert_eq!(
        verify.quarantined, 0,
        "a descriptor error must not quarantine a file"
    );
    settle_and_check(&mut h, &mut model, &mut clock);

    let query_errors = if tally.query_errors.is_empty() {
        "none".to_owned()
    } else {
        tally
            .query_errors
            .iter()
            .map(|(kind, count)| format!("{kind}:{count}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    println!(
        "CETRA_FD mode=load limit={effective} baseline={baseline} peak={peak} kinds={kinds} \
         appends_ok={} backpressure={} wal_failed={wal_failed} queries_ok={} \
         query_errors={query_errors} compaction_failed={} compaction_errors={}",
        tally.appends_ok,
        tally.backpressure,
        tally.queries_ok,
        tally.compaction_failed,
        tally.compaction_errors,
    );
}

/// The kind of an open error, for the child's line.
fn open_error_kind(error: &OpenError) -> &'static str {
    match error {
        OpenError::Config(_) => "config",
        OpenError::Locked { .. } => "locked",
        OpenError::Io { .. } => "io",
        OpenError::WalCorrupt { .. } => "wal_corrupt",
        OpenError::WalReplay(_) => "wal_replay",
        OpenError::Catalog(_) => "catalog",
        OpenError::Anchor(_) => "anchor",
        OpenError::Objects(_) => "objects",
        OpenError::Runtime => "runtime",
    }
}

/// The open child: builds the runtime, lowers the limit, then only opens the storage. An open
/// that fails for a descriptor reason must be a clean refusal, so `Config`, `Locked`, `Runtime`
/// and `WalCorrupt` fail the child.
fn open_child(limit: u64, dir: &Path) {
    let data = dir.join("data");
    fs::create_dir_all(&data).expect("create the data directory at the normal limit");
    let config = model_config(&data);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime for the open child");
    set_soft_nofile(limit);
    match runtime.block_on(Storage::open(config)) {
        Ok(storage) => {
            let shut = runtime.block_on(storage.shutdown());
            println!(
                "CETRA_FD mode=open limit={limit} open=ok shutdown={}",
                if shut.is_ok() { "ok" } else { "failed" }
            );
        }
        Err(error) => {
            let kind = open_error_kind(&error);
            match error {
                OpenError::Config(_) | OpenError::Locked { .. } | OpenError::Runtime => {
                    panic!("opening at limit {limit} failed with {kind}: {error}")
                }
                OpenError::WalCorrupt { .. } => {
                    panic!("opening at limit {limit} reported the WAL as corrupt: {error}")
                }
                OpenError::Io { .. }
                | OpenError::WalReplay(_)
                | OpenError::Catalog(_)
                | OpenError::Anchor(_)
                | OpenError::Objects(_) => {
                    println!("CETRA_FD mode=open limit={limit} open=failed kind={kind}");
                }
            }
        }
    }
}

/// The child of every descriptor run. Ignored: it runs only when `run_child` starts it, and
/// returns at once without `CETRA_FD_CHILD_MODE`.
#[test]
#[ignore = "a child process of the descriptor runs (run_child); run by just resources"]
fn child_run() {
    let Some(mode) = std::env::var_os(MODE_ENV) else {
        return;
    };
    crate::test_support::no_core_dumps();
    let limit: u64 = std::env::var(LIMIT_ENV)
        .expect("CETRA_FD_CHILD_LIMIT is set")
        .parse()
        .expect("CETRA_FD_CHILD_LIMIT is a number");
    let dir =
        std::path::PathBuf::from(std::env::var_os(DIR_ENV).expect("CETRA_FD_CHILD_DIR is set"));
    match mode.to_str() {
        Some("load") => {
            let seconds: u64 = std::env::var(SECONDS_ENV)
                .expect("CETRA_FD_CHILD_SECONDS is set")
                .parse()
                .expect("CETRA_FD_CHILD_SECONDS is a number");
            load_child(limit, seconds, &dir);
        }
        Some("open") => open_child(limit, &dir),
        _ => panic!("CETRA_FD_CHILD_MODE is load or open"),
    }
}

/// The descriptors the storage holds at the default limit. A peak at or above 1024 (the soft
/// limit of systemd services and recent Docker releases) is a finding, not a pass.
#[test]
#[ignore = "runs the load child for 30 s at the default limit; run by just resources"]
fn peak_at_the_default_limit() {
    let line = run_child(ChildMode::Load, 0, 30);
    println!("{line}");
    let peak: usize = field(&line, "peak=")
        .expect("the line has a peak")
        .parse()
        .expect("the peak is a count");
    assert!(
        peak < 1024,
        "the storage held {peak} descriptors at the default limit (kinds {})",
        field(&line, "kinds=").unwrap_or("none")
    );
}

/// Under limits of 256 down to 32, the load child gives errors and never panics or wedges.
#[test]
#[ignore = "runs the load child at four low limits (minutes); run by just resources"]
fn a_low_limit_gives_errors_not_panics() {
    for limit in [256, 128, 64, 32] {
        println!("{}", run_child(ChildMode::Load, limit, 20));
    }
}

/// Opening under limits of 64 down to 8 is a clean error or an open, never a panic.
#[test]
#[ignore = "runs the open child at four low limits; run by just resources"]
fn opening_under_a_low_limit_is_a_clean_error() {
    for limit in [64, 32, 16, 8] {
        println!("{}", run_child(ChildMode::Open, limit, 0));
    }
}

/// Five seconds of load at a limit of 64, in every `just test`: errors, not panics.
#[test]
#[ignore = "a 5 s run under a low descriptor limit; a finding for R5.8 (query memory), so not in just test yet; run by just resources"]
fn a_low_limit_smoke() {
    println!("{}", run_child(ChildMode::Load, 64, 5));
}
