//! Track R M5 (R5.6): the storage under a memory limit (docs/plans/r-hardening/05-m5-resources.md
//! § R5.6). A child process runs a heavy workload, and a parent re-executes it under
//! `systemd-run --user --scope` with `MemoryMax`. The run passes when the child is not killed by
//! the kernel and every check holds.

use std::fs;
use std::future::Future;
use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use arrow::array::RecordBatch;
use bytesize::ByteSize;

use super::super::check::check_full;
use super::super::harness::{Harness, HarnessOptions};
use super::super::ops::new_model;
use super::super::shape::{HOUR_NS, P, day, midnight};
use super::disk_full::SPILL_SQL;
use super::fill::{Text, filler, filler_batch};
use super::{model_steps, seed_ops};
use crate::config::{
    CompactionConfig, FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
};
use crate::error::{AppendError, QueryError};
use crate::hooks::HookPoint;
use crate::query_error::RunError;
use crate::reads::{ListScope, SortOrder, TraceListRequest, TraceSort, trace_list};
use crate::views::TimeWindow;
use crate::{QueryContext, QueryOptions, SqlRequest, Storage};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
const KIB: usize = 1 << 10;

/// `TMPFS_MAGIC` from `statfs(2)`: a tmpfs data directory would charge its pages to the cgroup.
const TMPFS_MAGIC: i128 = 0x0102_1994;

/// Parent-to-child environment: the data directory, the limit in bytes, the profile.
const DIR_ENV: &str = "LLMOBS_MEMORY_CHILD_DIR";
const LIMIT_ENV: &str = "LLMOBS_MEMORY_CHILD_LIMIT";
const PROFILE_ENV: &str = "LLMOBS_MEMORY_CHILD_PROFILE";
/// Set by the user of `minimum_memory`: the profile it searches.
const SEARCH_PROFILE_ENV: &str = "LLMOBS_MEMORY_PROFILE";
const CHILD_TIMEOUT: Duration = Duration::from_secs(600);
const FILL_LIMIT: Duration = Duration::from_secs(60);
const LOAD_LIMIT: Duration = Duration::from_secs(60);
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
/// The load phase stops appending once this many Arrow bytes are in, to bound the disk use.
const LOAD_BYTES: u64 = 6 * GIB;
/// Rows of one filler batch: 64 spans of 16 KiB, about 1 MiB of Arrow.
const BATCH_SPANS: u8 = 64;
const BATCH_INPUT_BYTES: usize = 16 * KIB;
/// The filler's ingest time: the model's clock at its start.
const INGEST_NS: i64 = 0;

/// Provisional, until `minimum_memory` measures the defaults profile: the worst-case budget of the
/// defaults (about 8.4 GiB with the query pool at half the limit) times 1.25, rounded up to 256 MiB.
/// Set it from the measured minimum before relying on `defaults_at_the_documented_minimum`.
const MIN_MEMORY_DEFAULTS: u64 = 10_752 * MIB;
/// Provisional, until `minimum_memory` measures the small profile: its budget (about 968 MiB) times
/// 1.25, rounded up to 256 MiB. Set it from the measured minimum before relying on
/// `small_profile_at_its_minimum`.
const MIN_MEMORY_SMALL: u64 = 1_280 * MIB;

/// What the child configures. `Probe` only reports the limit it sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Profile {
    Defaults,
    Small,
    Probe,
}

impl Profile {
    fn name(self) -> &'static str {
        match self {
            Self::Defaults => "defaults",
            Self::Small => "small",
            Self::Probe => "probe",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "defaults" => Some(Self::Defaults),
            "small" => Some(Self::Small),
            "probe" => Some(Self::Probe),
            _ => None,
        }
    }
}

/// Every default budget, for `data_dir`: what `llmobs serve` runs with. Only the timers change,
/// so the run exercises everything within a minute.
fn defaults_config(data_dir: &Path) -> StorageConfig {
    StorageConfig {
        data_dir: data_dir.to_path_buf(),
        min_free_bytes: ByteSize::gib(2),
        object_store: None,
        s3: S3Config::default(),
        wal: WalConfig::default(),
        flush: FlushConfig {
            max_age: Duration::from_secs(10),
            upload_concurrency: 8,
            ..FlushConfig::default()
        },
        memtable: MemtableConfig {
            hard_limit: ByteSize::gib(1),
        },
        query: QueryConfig {
            max_memory: ByteSize::gib(2),
            metadata_cache_bytes: ByteSize::mib(512),
            ..QueryConfig::default()
        },
        anchor: Default::default(),
        gc: Default::default(),
        compaction: CompactionConfig {
            interval: Duration::from_secs(5),
            l0_files: 2,
            max_memory: ByteSize::gib(1),
            ..CompactionConfig::default()
        },
        deletion: Default::default(),
    }
}

/// The small-machine profile (spec 03, documented in R5.8) for a machine with about 1 GiB.
fn small_config(data_dir: &Path) -> StorageConfig {
    StorageConfig {
        data_dir: data_dir.to_path_buf(),
        min_free_bytes: ByteSize::gib(2),
        object_store: None,
        s3: S3Config::default(),
        wal: WalConfig::default(),
        flush: FlushConfig {
            max_bytes: ByteSize::mib(32),
            max_age: Duration::from_secs(10),
            upload_concurrency: 2,
        },
        memtable: MemtableConfig {
            hard_limit: ByteSize::mib(128),
        },
        query: QueryConfig {
            max_memory_total: Some(ByteSize::mib(256)),
            max_memory: ByteSize::mib(128),
            initial_memory: ByteSize::mib(16),
            metadata_cache_bytes: ByteSize::mib(64),
            target_partitions: 2,
            ..QueryConfig::default()
        },
        anchor: Default::default(),
        gc: Default::default(),
        compaction: CompactionConfig {
            interval: Duration::from_secs(5),
            l0_files: 2,
            threads: 1,
            target_file_size: ByteSize::mib(128),
            l0_bytes: ByteSize::mib(128),
            max_memory: ByteSize::mib(128),
            ..CompactionConfig::default()
        },
        deletion: Default::default(),
    }
}

fn profile_config(profile: Profile, data_dir: &Path) -> StorageConfig {
    match profile {
        Profile::Defaults => defaults_config(data_dir),
        Profile::Small => small_config(data_dir),
        Profile::Probe => panic!("the probe profile opens no storage"),
    }
}

/// The child was killed by the kernel (the out-of-memory killer, or a cgroup kill).
#[derive(Debug)]
struct Killed;

/// `target/`, from the test binary at `target/<profile>/deps/<exe>`.
pub(super) fn target_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary's path");
    exe.ancestors()
        .nth(3)
        .expect("the test binary lives under target/")
        .to_path_buf()
}

/// Runs the child under `MemoryMax=limit`, `MemorySwapMax=0`. `Ok` holds every `LLMOBS_MEMORY`
/// line the child printed, when it finished; `Err(Killed)` when the kernel killed it.
fn run_limited(profile: Profile, limit: u64) -> Result<String, Killed> {
    // The data directory is on disk, not tmpfs: tmpfs pages would count against the cgroup and
    // could not be reclaimed without swap, while a disk file's page cache can be.
    let dir = data_dir_under_target();
    let exe = std::env::current_exe().expect("the test binary's path");
    let mut command = Command::new("systemd-run");
    command
        .args(["--user", "--scope", "--quiet", "--collect"])
        .arg("-p")
        .arg(format!("MemoryMax={limit}"))
        .args(["-p", "MemorySwapMax=0"])
        .arg(exe)
        .args([
            "--exact",
            "model_tests::resources::memory::child_run",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(DIR_ENV, dir.path())
        .env(LIMIT_ENV, limit.to_string())
        .env(PROFILE_ENV, profile.name());
    let Finished {
        status,
        stdout,
        stderr,
    } = run_to_end(&mut command, CHILD_TIMEOUT);
    let lines = marked_lines(&stdout, "LLMOBS_MEMORY");
    if status.signal() == Some(9) || status.code() == Some(137) {
        println!(
            "LLMOBS_MEMORY profile={} limit={limit} killed=true",
            profile.name()
        );
        return Err(Killed);
    }
    if !status.success() {
        if stderr.contains("Failed to connect to bus") {
            panic!("needs a systemd user session (systemd-run --user): {stderr}");
        }
        panic!(
            "the memory child exited {status}; its last lines:\n{}",
            tail(&stdout, 40)
        );
    }
    assert!(
        !lines.is_empty(),
        "the memory child printed no LLMOBS_MEMORY line:\n{}",
        tail(&stdout, 40)
    );
    Ok(lines.join("\n"))
}

/// A fresh directory under `target/llmobs-resources`, removed when the returned guard drops. It is
/// on disk (not tmpfs), so its pages are reclaimable (R5.6).
pub(super) fn data_dir_under_target() -> tempfile::TempDir {
    let root = target_dir().join("llmobs-resources");
    fs::create_dir_all(&root).expect("create target/llmobs-resources");
    let dir = tempfile::TempDir::new_in(&root).expect("a data directory under target/");
    let stat = rustix::fs::statfs(dir.path()).expect("statfs of the data directory");
    assert_ne!(
        i128::from(stat.f_type),
        TMPFS_MAGIC,
        "the resource runs' data directory must not be on tmpfs"
    );
    dir
}

/// How a child process ended and what it printed.
pub(super) struct Finished {
    pub(super) status: ExitStatus,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

/// Runs `command` with stdin closed and stdout and stderr piped, and waits for it. stderr is also
/// copied to this process's stderr. Panics when the child runs past `timeout`; the child is killed.
pub(super) fn run_to_end(command: &mut Command, timeout: Duration) -> Finished {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start a child process");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let out_reader = thread::spawn(move || read_all(stdout));
    let err_reader = thread::spawn(move || read_all(stderr));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll the child process") {
            break status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the child process ran past {timeout:?}");
        }
        thread::sleep(Duration::from_millis(100));
    };
    let stdout = out_reader.join().expect("stdout reader");
    let stderr = err_reader.join().expect("stderr reader");
    eprint!("{stderr}");
    Finished {
        status,
        stdout,
        stderr,
    }
}

fn read_all(mut reader: impl Read) -> String {
    let mut text = String::new();
    let _ = reader.read_to_string(&mut text);
    text
}

/// The text from `marker` to the end of each line that holds it. With `--nocapture`, libtest writes
/// the test's name before a test's first output on the same line, so a marker is not always at the
/// start of its line.
pub(super) fn marked_lines(output: &str, marker: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.find(marker).map(|at| line[at..].to_owned()))
        .collect()
}

pub(super) fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// The value of `key=` in the child's output: the first whitespace-separated token that starts
/// with it.
pub(super) fn field<'a>(output: &'a str, key: &str) -> Option<&'a str> {
    output
        .split_whitespace()
        .find_map(|token| token.strip_prefix(key))
}

/// The peak resident set of this process (`VmHWM`), in bytes.
fn peak_rss() -> u64 {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                let kib = line.strip_prefix("VmHWM:")?;
                kib.split_whitespace().next()?.parse::<u64>().ok()
            })
        })
        .map_or(0, |kib| kib * 1024)
}

/// What a loop tallied.
#[derive(Debug, Default, Clone, Copy)]
struct Counts {
    appends_ok: u64,
    backpressure: u64,
    disk_full: u64,
    queries_ok: u64,
    busy: u64,
    too_large: u64,
    timeout: u64,
}

impl Counts {
    fn plus(mut self, other: Self) -> Self {
        self.appends_ok += other.appends_ok;
        self.backpressure += other.backpressure;
        self.disk_full += other.disk_full;
        self.queries_ok += other.queries_ok;
        self.busy += other.busy;
        self.too_large += other.too_large;
        self.timeout += other.timeout;
        self
    }
}

/// Resolves `loops` unless `limit` passes first; then `stop` is set and the loops are awaited to
/// their end, so no call is cut off in flight.
pub(super) async fn until_stopped<T>(
    stop: &AtomicBool,
    limit: Duration,
    loops: impl Future<Output = T>,
) -> T {
    tokio::pin!(loops);
    tokio::select! {
        output = &mut loops => output,
        () = tokio::time::sleep(limit) => {
            stop.store(true, Ordering::SeqCst);
            loops.await
        }
    }
}

/// The next filler batch: `counter % 60_000` numbers it, and `counter % days` picks its day.
fn next_batch(counter: &AtomicU32, days: u32) -> RecordBatch {
    let number = counter.fetch_add(1, Ordering::SeqCst);
    let batch = u16::try_from(number % 60_000).expect("a batch number below 60 000 fits in u16");
    let day_offset = u8::try_from(number % days).expect("a day offset below 60 fits in u8");
    filler_batch(
        batch,
        BATCH_SPANS,
        BATCH_INPUT_BYTES,
        Text::Words,
        day_offset,
        INGEST_NS,
    )
}

/// Appends filler batches until `stop`. A `Backpressure` ends the loop when `stop_at_backpressure`
/// (the fill phase), or waits 50 ms and retries (the load phase). `DiskFull` is counted and the
/// loop waits. Anything else fails the child.
async fn appender(
    storage: &Storage,
    counter: &AtomicU32,
    bytes: &AtomicU64,
    days: u32,
    stop: &AtomicBool,
    stop_at_backpressure: bool,
) -> Counts {
    let mut counts = Counts::default();
    while !stop.load(Ordering::SeqCst) && bytes.load(Ordering::SeqCst) < LOAD_BYTES {
        let batch = next_batch(counter, days);
        let size = batch.get_array_memory_size() as u64;
        match storage.append_spans(filler(), batch).await {
            Ok(_) => {
                counts.appends_ok += 1;
                bytes.fetch_add(size, Ordering::SeqCst);
            }
            Err(AppendError::Backpressure) => {
                counts.backpressure += 1;
                if stop_at_backpressure {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(AppendError::DiskFull) => {
                counts.disk_full += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(other) => panic!("an append of filler rows failed: {other}"),
        }
    }
    counts
}

/// Why a query did not return rows.
enum QueryFailure {
    Busy,
    Run(RunError),
}

/// One query of kind `which % 3` over project C: a trace list over eight days (the widest window
/// the list accepts is reported by the test that uses it), a sort that spills, and R5.2's window
/// count, which spills across many files.
async fn run_query(storage: &Storage, which: usize) -> Result<(), QueryFailure> {
    let query = match storage.query(filler(), QueryOptions::default()).await {
        Ok(query) => query,
        Err(QueryError::Busy) => return Err(QueryFailure::Busy),
        Err(other) => panic!("query admission failed: {other}"),
    };
    let ctx = QueryContext::new(query, None).map_err(QueryFailure::Run)?;
    let params = serde_json::Map::new();
    match which % 3 {
        0 => {
            let window =
                TimeWindow::new(midnight(day(0)), midnight(day(8))).map_err(QueryFailure::Run)?;
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
                    sort: TraceSort::CostTotal,
                    order: SortOrder::Desc,
                    limit: 50,
                    cursor: None,
                    timeout: QUERY_TIMEOUT,
                },
            )
            .await
            .map(drop)
            .map_err(QueryFailure::Run)
        }
        1 => ctx
            .sql_json(SqlRequest {
                query: "SELECT input, count(*) AS n FROM spans GROUP BY input ORDER BY n DESC",
                params: &params,
                limit: 10,
                timeout: QUERY_TIMEOUT,
            })
            .await
            .map(drop)
            .map_err(QueryFailure::Run),
        _ => ctx
            .sql_json(SqlRequest {
                query: SPILL_SQL,
                params: &params,
                limit: 1,
                timeout: QUERY_TIMEOUT,
            })
            .await
            .map(drop)
            .map_err(QueryFailure::Run),
    }
}

/// Runs queries of kind `which` until `stop`. Every outcome is tallied; anything else fails the
/// child.
async fn query_loop(storage: &Storage, which: usize, stop: &AtomicBool) -> Counts {
    let mut counts = Counts::default();
    while !stop.load(Ordering::SeqCst) {
        match tokio::time::timeout(QUERY_TIMEOUT, run_query(storage, which)).await {
            Err(_elapsed) => counts.timeout += 1,
            Ok(Ok(())) => counts.queries_ok += 1,
            Ok(Err(QueryFailure::Busy)) => counts.busy += 1,
            Ok(Err(QueryFailure::Run(RunError::TooLarge))) => counts.too_large += 1,
            Ok(Err(QueryFailure::Run(RunError::Timeout))) => counts.timeout += 1,
            Ok(Err(QueryFailure::Run(other))) => {
                panic!("a query failed with kind {}: {other}", other.kind())
            }
        }
    }
    counts
}

/// The probe: what limit the child sees. Prints `total` (the default query total) and `cgroup`.
fn probe() {
    let total = crate::memory::default_total_memory();
    let cgroup = fs::read_to_string("/proc/self/cgroup")
        .unwrap_or_default()
        .lines()
        .find(|line| line.starts_with("0::"))
        .unwrap_or("none")
        .to_owned();
    println!("LLMOBS_MEMORY profile=probe total={total} cgroup={cgroup}");
}

/// The child of every memory run. Ignored: it runs only when `run_limited` starts it, and returns
/// at once without `LLMOBS_MEMORY_CHILD_DIR`.
#[test]
#[ignore = "a child process of the memory runs (run_limited); run by just resources-memory"]
fn child_run() {
    let Some(dir) = std::env::var_os(DIR_ENV) else {
        return;
    };
    let limit: u64 = std::env::var(LIMIT_ENV)
        .expect("LLMOBS_MEMORY_CHILD_LIMIT is set")
        .parse()
        .expect("LLMOBS_MEMORY_CHILD_LIMIT is a byte count");
    let profile =
        Profile::parse(&std::env::var(PROFILE_ENV).expect("LLMOBS_MEMORY_CHILD_PROFILE is set"))
            .expect("LLMOBS_MEMORY_CHILD_PROFILE is defaults, small or probe");
    if profile == Profile::Probe {
        probe();
        return;
    }
    // A cgroup namespace whose root is not the mounted cgroup hides the limit: the default
    // total then comes from the machine's RAM. Set the query total by the rule the default
    // follows, so the run still tests every other budget.
    let hidden = crate::memory::default_total_memory() > limit / 2 + MIB;
    if hidden {
        println!("LLMOBS_MEMORY note=limit-not-visible");
    }
    let data_root = PathBuf::from(dir);
    let parallelism = thread::available_parallelism().map_or(2, usize::from);
    let mut h = Harness::with_options(
        HarnessOptions {
            root: Some(data_root),
            workers: parallelism,
            unrecorded: true,
            ..HarnessOptions::default()
        },
        |config| {
            let data = config.data_dir.clone();
            *config = profile_config(profile, &data);
            if hidden {
                config.query.max_memory_total = Some(ByteSize(limit / 2));
            }
        },
    );
    let mut model = new_model(&h, (P::A, P::B));
    let mut clock = midnight(day(0)) + 12 * HOUR_NS;
    model_steps(&mut h, &mut model, &mut clock, &seed_ops(), 0);

    let counter = AtomicU32::new(0);
    let bytes = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let storage = h.storage();

    // Fill: hold the flush, so the spans memtable fills to its hard limit with one frozen flush.
    // The phase ends when every appender has seen `Backpressure`, or at 60 s.
    storage.hooks().hold(HookPoint::FlushAfterStage);
    let (f1, f2, f3, f4) = h.block_on(until_stopped(&stop, FILL_LIMIT, async {
        tokio::join!(
            appender(storage, &counter, &bytes, 8, &stop, true),
            appender(storage, &counter, &bytes, 8, &stop, true),
            appender(storage, &counter, &bytes, 8, &stop, true),
            appender(storage, &counter, &bytes, 8, &stop, true),
        )
    }));
    let fill = [f1, f2, f3, f4]
        .into_iter()
        .fold(Counts::default(), Counts::plus);
    assert!(
        fill.backpressure > 0,
        "the fill phase never reached the memtable's hard limit"
    );
    storage.hooks().release(HookPoint::FlushAfterStage);

    // Load: appenders retry on backpressure, queries run beside them, and background compaction
    // runs on its 5 s interval. The phase ends at 60 s, or when the appenders reach 6 GiB.
    stop.store(false, Ordering::SeqCst);
    let (a1, a2, a3, a4, q1, q2, q3, q4) = h.block_on(until_stopped(&stop, LOAD_LIMIT, async {
        tokio::join!(
            appender(storage, &counter, &bytes, 8, &stop, false),
            appender(storage, &counter, &bytes, 8, &stop, false),
            appender(storage, &counter, &bytes, 8, &stop, false),
            appender(storage, &counter, &bytes, 8, &stop, false),
            query_loop(storage, 0, &stop),
            query_loop(storage, 1, &stop),
            query_loop(storage, 2, &stop),
            query_loop(storage, 3, &stop),
        )
    }));
    let load = [a1, a2, a3, a4, q1, q2, q3, q4]
        .into_iter()
        .fold(fill, Counts::plus);

    // Every loop has stopped. The restart must recover, and the model's reads must still match.
    h.restart();
    check_full(&h, &mut model, 500);
    println!(
        "LLMOBS_MEMORY profile={} limit={limit} peak_rss={} appends_ok={} backpressure={} \
         disk_full={} queries_ok={} busy={} too_large={} timeout={}",
        profile.name(),
        peak_rss(),
        load.appends_ok,
        load.backpressure,
        load.disk_full,
        load.queries_ok,
        load.busy,
        load.too_large,
        load.timeout,
    );
}

/// The defaults profile survives its documented minimum memory limit (`MIN_MEMORY_DEFAULTS`).
#[test]
#[ignore = "runs the defaults profile under a memory limit (minutes); run by just resources-memory"]
fn defaults_at_the_documented_minimum() {
    match run_limited(Profile::Defaults, MIN_MEMORY_DEFAULTS) {
        Ok(output) => println!("{output}"),
        Err(Killed) => panic!(
            "the defaults profile was killed at {MIN_MEMORY_DEFAULTS} bytes (its documented minimum)"
        ),
    }
}

/// The small-machine profile survives its minimum memory limit (`MIN_MEMORY_SMALL`).
#[test]
#[ignore = "runs the small profile under a memory limit (minutes); run by just resources-memory"]
fn small_profile_at_its_minimum() {
    match run_limited(Profile::Small, MIN_MEMORY_SMALL) {
        Ok(output) => println!("{output}"),
        Err(Killed) => panic!(
            "the small profile was killed at {MIN_MEMORY_SMALL} bytes (its documented minimum)"
        ),
    }
}

/// Finds the smallest limit a profile survives: runs it at decreasing limits until the first kill,
/// then bisects twice between the last pass and that kill. Takes tens of minutes; check the free
/// RAM (`free -g`) first. The profile is `LLMOBS_MEMORY_PROFILE` (`defaults` by default).
#[test]
#[ignore = "takes tens of minutes and writes gigabytes; run by just resources-memory-min"]
fn minimum_memory() {
    let profile = match std::env::var(SEARCH_PROFILE_ENV) {
        Ok(name) => Profile::parse(&name)
            .filter(|profile| *profile != Profile::Probe)
            .expect("LLMOBS_MEMORY_PROFILE is defaults or small"),
        Err(_) => Profile::Defaults,
    };
    let limits: Vec<u64> = match profile {
        Profile::Defaults => [12, 10, 8, 7, 6, 5, 4, 3].map(|g| g * GIB).to_vec(),
        Profile::Small => vec![2 * GIB, 1536 * MIB, GIB, 768 * MIB, 512 * MIB],
        Profile::Probe => unreachable!("the probe profile is filtered out above"),
    };
    let mut pass: Option<u64> = None;
    let mut kill: Option<u64> = None;
    for limit in limits {
        match run_limited(profile, limit) {
            Ok(output) => {
                println!("{output}");
                pass = Some(limit);
            }
            Err(Killed) => {
                println!(
                    "LLMOBS_MEMORY_SEARCH profile={} limit={limit} killed",
                    profile.name()
                );
                kill = Some(limit);
                break;
            }
        }
    }
    let smallest = match (pass, kill) {
        (None, _) => panic!(
            "the largest limit tried already killed the {} profile",
            profile.name()
        ),
        (Some(passing), None) => passing,
        (Some(passing), Some(killing)) => {
            // `lo` is killed, `hi` passes: the midpoint of the two narrows the interval.
            let (mut lo, mut hi) = (killing, passing);
            for _ in 0..2 {
                let mid = lo + (hi - lo) / 2;
                match run_limited(profile, mid) {
                    Ok(output) => {
                        println!("{output}");
                        hi = mid;
                    }
                    Err(Killed) => {
                        println!(
                            "LLMOBS_MEMORY_SEARCH profile={} limit={mid} killed",
                            profile.name()
                        );
                        lo = mid;
                    }
                }
            }
            hi
        }
    };
    println!(
        "LLMOBS_MEMORY_MIN profile={} smallest_passing_limit={smallest}",
        profile.name()
    );
}

/// The probe sees its scope's limit: a 2 GiB scope gives a total of 1 GiB. A cgroup path that
/// starts with `0::/..` cannot be resolved by the cgroup mount (a cgroup namespace), so the check
/// is skipped there.
#[test]
#[ignore = "runs a probe child under a systemd scope; run by just resources-memory"]
fn child_sees_the_scope_limit() {
    let output = match run_limited(Profile::Probe, 2 * GIB) {
        Ok(output) => output,
        Err(Killed) => panic!("the probe child was killed under a 2 GiB scope"),
    };
    println!("{output}");
    let cgroup = field(&output, "cgroup=").unwrap_or("none");
    if cgroup.starts_with("0::/..") {
        println!("cgroup path not resolvable here (cgroup namespace)");
        return;
    }
    let total: u64 = field(&output, "total=")
        .expect("the probe prints its total")
        .parse()
        .expect("the probe's total is a byte count");
    assert_eq!(
        total, GIB,
        "a 2 GiB scope limit must give a query total of 1 GiB, got {total} (cgroup {cgroup})"
    );
}
