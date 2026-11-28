//! `fmtbench cw project` and `fmtbench cw layout`: cetra projects written by `cetra-storage`
//! (CW-21), their data files laid out as Parquet and as Strata (CW-25), and the layout report
//! (`plans/08-m3-many-files.md`, M3.1).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use arrow::array::{Array, RecordBatch, StringArray};
use arrow::ipc::reader::FileReader;
use bytesize::ByteSize;
use cetra_core::batch::{scores_to_record_batch, spans_to_record_batch};
use cetra_core::enums::{PayloadFormat, ScoreSource, SpanKind};
use cetra_core::score::{Score, ScoreTarget, ScoreValue};
use cetra_core::span::Span;
use cetra_storage::Storage;
use cetra_storage::config::StorageConfig;
use cetra_storage::error::AppendError;
use fmt_testdata::datagen;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use strata::{Config, StrataWriter};
use uuid::Uuid;

use crate::cw::cases;
use crate::{R, strip_payloads};

/// Share of traces that get real payloads (CW-23).
pub const PAYLOAD_SHARE: f64 = 0.02;
/// Spans per `append_spans` call, as cetra's `trace_bench`. A trace may be split between calls.
pub const APPEND_SPANS: usize = 10_000;
/// Real payload pairs kept from the `--payloads` cases, and their byte budget.
pub const PAYLOAD_POOL: usize = 100_000;
pub const PAYLOAD_POOL_BYTES: usize = 512 << 20;
/// Strata conversions running at once for files under `CONVERT_ALONE_BYTES`; larger files are
/// converted one at a time (`StrataWriter` keeps every batch until `finish`).
pub const CONVERT_AT_ONCE: usize = 4;
pub const CONVERT_ALONE_BYTES: u64 = 64 << 20;
/// The first day of a project (2027-01-04T00:00:00Z), in the future so that the storage's
/// wall-clock background compaction never closes a day (CW-23).
pub const PROJECT_START: i64 = 1_799_020_800 * 1_000_000_000;

const NS: i64 = 1_000_000_000;
const HOUR_NS: i64 = 3_600 * NS;
const DAY_NS: i64 = 86_400 * NS;
/// Trace ids and root times kept for the lookup queries.
const SAMPLE_TRACES: usize = 20;
/// Compaction passes before a project build is refused (each pass runs at one `now`).
const MAX_COMPACTION_PASSES: usize = 100;
const BACKPRESSURE_WAIT: Duration = Duration::from_millis(100);
const FLUSH_TIMEOUT: Duration = Duration::from_secs(600);
const USAGE_PROJECT: &str = "usage: fmtbench cw project <dir> --days D --traces-per-day N [--live-hours H] [--payloads <case-dir>,<case-dir>] | cw project <dir> --small R --flushes K | cw layout <dir>";
const TABLES: [&str; 3] = ["spans", "traces", "scores"];
/// The queries of `cw_queries.txt`, in run order, per shape (CW-24, step 5 and step 6 of M3.1).
const DAYS_QUERIES: [&str; 8] = [
    "open:spans", "open:traces", "open:scores", "lookup_cold", "lookup_around", "rollup_30d",
    "rollup_30d_cold", "anchor_rebuild",
];
const SMALL_QUERIES: [&str; 5] = ["open:spans", "open:traces", "open:scores", "lookup_cold", "rollup_30d_cold"];

// ------------------------------------------------------------------ options

/// The two shapes of a project.
#[derive(Debug)]
enum Shape {
    /// `--days D --traces-per-day N [--live-hours H] [--payloads ...]`.
    Days {
        days: usize,
        traces_per_day: usize,
        live_hours: usize,
        payloads: Vec<PathBuf>,
    },
    /// `--small R --flushes K`.
    Small { spans: usize, flushes: usize },
}

/// `cw project <dir> …`, parsed.
#[derive(Debug)]
struct ProjectOpts {
    dir: PathBuf,
    shape: Shape,
}

fn parse_project(args: &[String]) -> R<ProjectOpts> {
    let dir = PathBuf::from(args.first().ok_or(USAGE_PROJECT)?);
    let mut days = None;
    let mut traces_per_day = None;
    let mut live_hours = None;
    let mut payloads = None;
    let mut small = None;
    let mut flushes = None;
    let mut rest = args[1..].iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or(USAGE_PROJECT)?;
        match flag.as_str() {
            "--days" => days = Some(value.parse::<usize>()?),
            "--traces-per-day" => traces_per_day = Some(value.parse::<usize>()?),
            "--live-hours" => live_hours = Some(value.parse::<usize>()?),
            "--payloads" => {
                payloads = Some(value.split(',').filter(|s| !s.is_empty()).map(PathBuf::from).collect::<Vec<_>>())
            }
            "--small" => small = Some(value.parse::<usize>()?),
            "--flushes" => flushes = Some(value.parse::<usize>()?),
            _ => return Err(USAGE_PROJECT.into()),
        }
    }
    if dir.exists() {
        return Err(format!("{} exists: a project directory must not exist", dir.display()).into());
    }
    let shape = match (small, flushes) {
        (Some(spans), Some(flushes)) => {
            if days.is_some() || traces_per_day.is_some() || live_hours.is_some() || payloads.is_some() {
                return Err(USAGE_PROJECT.into());
            }
            if spans == 0 || flushes == 0 {
                return Err("--small and --flushes must be at least 1".into());
            }
            Shape::Small { spans, flushes }
        }
        (None, None) => {
            let days = days.ok_or(USAGE_PROJECT)?;
            let traces_per_day = traces_per_day.ok_or(USAGE_PROJECT)?;
            if days == 0 || traces_per_day == 0 {
                return Err("--days and --traces-per-day must be at least 1".into());
            }
            Shape::Days {
                days,
                traces_per_day,
                live_hours: live_hours.unwrap_or(6),
                payloads: payloads.unwrap_or_default(),
            }
        }
        _ => return Err(USAGE_PROJECT.into()),
    };
    Ok(ProjectOpts { dir, shape })
}

// ------------------------------------------------------------------ command

/// `cw project …`.
pub async fn project_cmd(args: &[String]) -> R<()> {
    let opts = parse_project(args)?;
    let t0 = Instant::now();
    let dir = opts.dir;
    fs::create_dir_all(dir.join("engine"))?;
    let storage = Storage::open(engine_config(&dir.join("engine"))).await?;
    let mut generator = Gen::new(Vec::new());
    let mut pending = Pending::default();
    let small = matches!(opts.shape, Shape::Small { .. });
    let (sampler, queries): (Sampler, &[&str]) = match opts.shape {
        Shape::Days { days, traces_per_day, live_hours, payloads } => {
            let pool = read_pool(&payloads)?;
            println!("payload pool: {} pairs", pool.len());
            generator.pool = pool;
            let mut sampler = Sampler::new(days);
            let t = Instant::now();
            ingest_days(&storage, &mut generator, &mut pending, &mut sampler, days, traces_per_day).await?;
            let ingest = t.elapsed();
            let t = Instant::now();
            let (passes, jobs) = compact_until_settled(&storage, day_start(days) + 3 * HOUR_NS).await?;
            storage.collect_obsolete(Duration::ZERO).await?;
            let compact = t.elapsed();
            let t = Instant::now();
            ingest_live(&storage, &mut generator, &mut pending, traces_per_day, live_hours, days).await?;
            // Background passes of the live part's count-based merges leave obsolete files.
            storage.collect_obsolete(Duration::ZERO).await?;
            let live = t.elapsed();
            println!(
                "ingest {ingest:.2?} (30 days), compaction {compact:.2?} ({passes} passes, {jobs} jobs), live part {live:.2?}"
            );
            (sampler, &DAYS_QUERIES[..])
        }
        Shape::Small { spans, flushes } => {
            let mut sampler = Sampler::new(1);
            let t = Instant::now();
            ingest_small(&storage, &mut generator, &mut pending, &mut sampler, spans, flushes).await?;
            let ingest = t.elapsed();
            storage.collect_obsolete(Duration::ZERO).await?;
            println!("ingest {ingest:.2?} ({flushes} flushes of {spans} spans, no compaction)");
            (sampler, &SMALL_QUERIES[..])
        }
    };
    println!(
        "{} traces, {} spans, {} scores, {:.2?} so far",
        generator.traces,
        generator.spans,
        generator.scores,
        t0.elapsed()
    );
    write_texts(&dir, &sampler.into_samples(), generator.clock, queries)?;
    storage.shutdown().await?;
    let t = Instant::now();
    let jobs = plan_layout(&dir)?;
    if small {
        check_small_levels(&jobs)?;
    }
    let converted = convert_all(&jobs)?;
    check_layout_rows(&jobs, &converted, generator.spans, generator.scores)?;
    println!("conversion {:.2?}", t.elapsed());
    print_build_stats(&jobs, &converted);
    Ok(())
}

/// `cw layout <dir>`: prints `layout_report`.
pub fn layout_cmd(args: &[String]) -> R<()> {
    let dir = PathBuf::from(args.first().ok_or(USAGE_PROJECT)?);
    print!("{}", layout_report(&dir)?);
    Ok(())
}

fn engine_config(data_dir: &Path) -> StorageConfig {
    StorageConfig {
        delete_objects: true,
        data_dir: data_dir.to_path_buf(),
        min_free_bytes: ByteSize::b(0),
        object_store: None,
        s3: Default::default(),
        wal: Default::default(),
        flush: Default::default(),
        memtable: Default::default(),
        query: Default::default(),
        anchor: Default::default(),
        gc: Default::default(),
        compaction: Default::default(),
        deletion: Default::default(),
    }
}

/// Start of day `d` of a project.
fn day_start(d: usize) -> i64 {
    PROJECT_START + d as i64 * DAY_NS
}

// ------------------------------------------------------------------ generation

/// The payload pool: `(input, output)` pairs from the `--payloads` cases, in order.
fn read_pool(cases_dirs: &[PathBuf]) -> R<Vec<(String, String)>> {
    let mut pool = Vec::new();
    let mut bytes = 0usize;
    'files: for case in cases_dirs {
        for path in cases::input_paths(case)? {
            let reader = FileReader::try_new(fs::File::open(&path)?, None)?;
            for batch in reader {
                let batch = batch?;
                let input = batch.column_by_name("input").ok_or("a case input file has no input column")?;
                let output = batch.column_by_name("output").ok_or("a case input file has no output column")?;
                let input = input.as_any().downcast_ref::<StringArray>().ok_or("input is not a string column")?;
                let output = output.as_any().downcast_ref::<StringArray>().ok_or("output is not a string column")?;
                for i in 0..batch.num_rows() {
                    if input.is_null(i) || output.is_null(i) {
                        continue;
                    }
                    let (a, b) = (input.value(i), output.value(i));
                    bytes += a.len() + b.len();
                    pool.push((a.to_string(), b.to_string()));
                    if pool.len() >= PAYLOAD_POOL || bytes >= PAYLOAD_POOL_BYTES {
                        break 'files;
                    }
                }
            }
        }
    }
    Ok(pool)
}

/// The generator's state between traces.
struct Gen {
    world: datagen::World,
    rng: datagen::Rng,
    pool: Vec<(String, String)>,
    next_pair: usize,
    /// The largest `ingest_time` written so far (`cw_clock.txt`).
    clock: i64,
    traces: u64,
    spans: u64,
    scores: u64,
}

/// One trace, placed in time: its root, its spans and its scores.
struct Placed {
    trace_id: [u8; 16],
    root_start: i64,
    spans: Vec<Span>,
    scores: Vec<Score>,
}

impl Gen {
    fn new(pool: Vec<(String, String)>) -> Self {
        Self {
            world: datagen::World::new(7),
            rng: datagen::Rng::new(42),
            pool,
            next_pair: 0,
            clock: 0,
            traces: 0,
            spans: 0,
            scores: 0,
        }
    }

    /// One trace of the generator, stripped, with a payload share (CW-23), placed so that its
    /// root starts at `at`, with its scores.
    fn trace(&mut self, at: i64) -> Placed {
        let mut spans = Vec::new();
        self.world.trace(&mut self.rng, &mut spans);
        spans.iter_mut().for_each(strip_payloads);
        if self.rng.unit() < PAYLOAD_SHARE && !self.pool.is_empty() {
            for s in spans.iter_mut().filter(|s| matches!(s.span_kind, SpanKind::Llm)) {
                let (input, output) = &self.pool[self.next_pair % self.pool.len()];
                self.next_pair += 1;
                s.input = Some(input.clone());
                s.output = Some(output.clone());
                s.input_format = Some(PayloadFormat::Text);
                s.output_format = Some(PayloadFormat::Text);
                s.input_preview = Some(datagen::preview(input));
                s.output_preview = Some(datagen::preview(output));
            }
        }
        let delta = at - spans[0].start_time;
        for s in &mut spans {
            s.start_time += delta;
            s.end_time += delta;
            for e in &mut s.events {
                e.time += delta;
            }
        }
        let ingest = spans.iter().map(|s| s.end_time).max().unwrap_or(at) + NS;
        for s in &mut spans {
            s.ingest_time = ingest;
        }
        self.clock = self.clock.max(ingest);
        let root = &spans[0];
        let trace_id = *root.trace_id.as_bytes();
        let root_start = root.start_time;
        let names = ["relevance", "toxicity", "helpfulness"];
        let n_scores = self.rng.range(1, 3);
        let mut scores = Vec::with_capacity(n_scores);
        for k in 0..n_scores {
            let value = ScoreValue::Numeric(self.rng.unit());
            scores.push(Score {
                project_id: root.project_id,
                score_id: Uuid::now_v7(),
                target: ScoreTarget::Trace { trace_id: root.trace_id },
                session_id: root.session_id.clone(),
                user_id: root.user_id.clone(),
                name: names[k % names.len()].to_string(),
                value,
                reason: None,
                source: ScoreSource::LlmJudge,
                evaluator_id: None,
                rule_id: None,
                experiment_id: None,
                dataset_item_id: None,
                author_user_id: None,
                metadata: BTreeMap::new(),
                created_at: ingest + NS,
                updated_at: ingest + NS,
                ingest_time: ingest + NS,
                deleted: false,
            });
        }
        self.traces += 1;
        self.spans += spans.len() as u64;
        self.scores += scores.len() as u64;
        Placed { trace_id, root_start, spans, scores }
    }
}

/// Spans and scores waiting for their `append_*` calls.
#[derive(Default)]
struct Pending {
    spans: Vec<Span>,
    scores: Vec<Score>,
}

impl Pending {
    fn push(&mut self, placed: Placed) {
        self.spans.extend(placed.spans);
        self.scores.extend(placed.scores);
    }
}

/// Appends every full `APPEND_SPANS` of each buffer as one batch, and with `all` the rest too.
async fn append_ready(storage: &Storage, pending: &mut Pending, all: bool) -> R<()> {
    while pending.spans.len() >= APPEND_SPANS || (all && !pending.spans.is_empty()) {
        let n = pending.spans.len().min(APPEND_SPANS);
        let head: Vec<Span> = pending.spans.drain(..n).collect();
        append_spans(storage, &head).await?;
    }
    while pending.scores.len() >= APPEND_SPANS || (all && !pending.scores.is_empty()) {
        let n = pending.scores.len().min(APPEND_SPANS);
        let head: Vec<Score> = pending.scores.drain(..n).collect();
        append_scores(storage, &head).await?;
    }
    Ok(())
}

async fn append_spans(storage: &Storage, spans: &[Span]) -> R<()> {
    let batch: RecordBatch = spans_to_record_batch(spans)?;
    let project = spans[0].project_id;
    loop {
        match storage.append_spans(project, batch.clone()).await {
            Ok(_) => return Ok(()),
            Err(AppendError::Backpressure) => tokio::time::sleep(BACKPRESSURE_WAIT).await,
            Err(e) => return Err(e.into()),
        }
    }
}

async fn append_scores(storage: &Storage, scores: &[Score]) -> R<()> {
    let batch: RecordBatch = scores_to_record_batch(scores)?;
    let project = scores[0].project_id;
    loop {
        match storage.append_scores(project, batch.clone()).await {
            Ok(_) => return Ok(()),
            Err(AppendError::Backpressure) => tokio::time::sleep(BACKPRESSURE_WAIT).await,
            Err(e) => return Err(e.into()),
        }
    }
}

/// Compaction passes at `now` until one reports no job and no failure. Returns the passes run
/// and the jobs committed.
async fn compact_until_settled(storage: &Storage, now: i64) -> R<(usize, u64)> {
    let mut jobs = 0;
    let mut last = String::new();
    for pass in 1..=MAX_COMPACTION_PASSES {
        let report = storage.compact(now).await?;
        jobs += report.jobs;
        if report.jobs + report.failed == 0 {
            return Ok((pass, jobs));
        }
        last = format!("{report:?}");
    }
    Err(format!("compaction still busy after {MAX_COMPACTION_PASSES} passes; last report {last}").into())
}

/// The 30-day form, up to its flush (step 5, first half).
async fn ingest_days(
    storage: &Storage,
    generator: &mut Gen,
    pending: &mut Pending,
    sampler: &mut Sampler,
    days: usize,
    traces_per_day: usize,
) -> R<()> {
    for d in 0..days {
        let base = day_start(d);
        for _ in 0..traces_per_day {
            let at = base + generator.rng.below(86_400) as i64 * NS + generator.rng.below(1_000_000_000) as i64;
            let placed = generator.trace(at);
            sampler.see(d, placed.trace_id, placed.root_start);
            pending.push(placed);
            append_ready(storage, pending, false).await?;
        }
    }
    append_ready(storage, pending, true).await?;
    storage.flush_now(FLUSH_TIMEOUT).await?;
    Ok(())
}

/// The live part of the 30-day form (step 5): `traces_per_day * live_hours / 24` traces across
/// the midnight that starts day `days`. No compaction: they stay level-0 files.
async fn ingest_live(
    storage: &Storage,
    generator: &mut Gen,
    pending: &mut Pending,
    traces_per_day: usize,
    live_hours: usize,
    days: usize,
) -> R<()> {
    let live = traces_per_day * live_hours / 24;
    let window_start = day_start(days) - (live_hours as i64 - 1) * HOUR_NS;
    for _ in 0..live {
        let at = window_start + generator.rng.below(live_hours * 3_600) as i64 * NS + generator.rng.below(1_000_000_000) as i64;
        pending.push(generator.trace(at));
        append_ready(storage, pending, false).await?;
    }
    append_ready(storage, pending, true).await?;
    storage.flush_now(FLUSH_TIMEOUT).await?;
    Ok(())
}

/// The small-files form (step 6): `flushes` flushes of `spans_per_flush` spans, each a level-0
/// file per table, all on the first day.
async fn ingest_small(
    storage: &Storage,
    generator: &mut Gen,
    pending: &mut Pending,
    sampler: &mut Sampler,
    spans_per_flush: usize,
    flushes: usize,
) -> R<()> {
    let mut i: i64 = 0;
    for _ in 0..flushes {
        let mut spans = 0;
        while spans < spans_per_flush {
            if i > 86_399 {
                return Err("too many spans for one day: lower --small or --flushes".into());
            }
            let jitter = generator.rng.below(1_000_000_000) as i64;
            let placed = generator.trace(PROJECT_START + i * NS + jitter);
            spans += placed.spans.len();
            sampler.see(0, placed.trace_id, placed.root_start);
            pending.push(placed);
            i += 1;
        }
        append_ready(storage, pending, true).await?;
        storage.flush_now(FLUSH_TIMEOUT).await?;
    }
    Ok(())
}

/// Picks the traces that the lookup queries use (step 7): on `m = min(10, days)` days spread
/// evenly over the project, the first `ceil(20 / m)` traces of each.
struct Sampler {
    chosen: Vec<usize>,
    per_day: usize,
    taken: Vec<usize>,
    out: Vec<([u8; 16], i64)>,
}

impl Sampler {
    fn new(days: usize) -> Self {
        let m = days.min(10);
        Self {
            chosen: (0..m).map(|i| i * days / m).collect(),
            per_day: SAMPLE_TRACES.div_ceil(m),
            taken: vec![0; m],
            out: Vec::new(),
        }
    }

    /// Offers the trace of day `day` (the `small` form uses day 0).
    fn see(&mut self, day: usize, trace_id: [u8; 16], root_start: i64) {
        if let Some(pos) = self.chosen.iter().position(|&c| c == day) {
            if self.taken[pos] < self.per_day {
                self.taken[pos] += 1;
                self.out.push((trace_id, root_start));
            }
        }
    }

    fn into_samples(self) -> Vec<([u8; 16], i64)> {
        let mut out = self.out;
        out.truncate(SAMPLE_TRACES);
        out
    }
}

/// `cw_traces.txt`, `cw_around.txt`, `cw_clock.txt`, `cw_queries.txt` and `cw_needles.txt`.
fn write_texts(dir: &Path, samples: &[([u8; 16], i64)], clock: i64, queries: &[&str]) -> R<()> {
    let hex = |b: &[u8]| -> String { b.iter().map(|x| format!("{x:02x}")).collect() };
    let traces: String = samples.iter().map(|(id, _)| hex(id) + "\n").collect();
    let around: String = samples.iter().map(|(_, t)| format!("{t}\n")).collect();
    fs::write(dir.join("cw_traces.txt"), traces)?;
    fs::write(dir.join("cw_around.txt"), around)?;
    fs::write(dir.join("cw_clock.txt"), format!("{clock}\n"))?;
    let mut q = String::new();
    for name in queries {
        q.push_str(name);
        q.push('\n');
    }
    fs::write(dir.join("cw_queries.txt"), q)?;
    write_needles(dir)
}

/// Writes `cw_needles.txt` into `dir`: `cases::PROJECT_NEEDLES`, one `name<TAB>hex` line each
/// (M1's format), replacing the file if present.
fn write_needles(dir: &Path) -> R<()> {
    let text: String = cases::PROJECT_NEEDLES
        .iter()
        .map(|(name, needle)| format!("{name}\t{}\n", crate::hex(needle.as_bytes())))
        .collect();
    fs::write(dir.join("cw_needles.txt"), text)?;
    Ok(())
}

/// `cw needles <dir>`: writes `cw_needles.txt` into an existing project directory, for the
/// projects built before M4.1.
pub fn needles_cmd(args: &[String]) -> R<()> {
    let [dir] = args else {
        return Err(crate::USAGE.into());
    };
    let dir = Path::new(dir);
    if !dir.is_dir() {
        return Err(format!("{} is not a project directory", dir.display()).into());
    }
    write_needles(dir)?;
    println!("wrote {}", dir.join("cw_needles.txt").display());
    Ok(())
}

// ------------------------------------------------------------------ layout

/// A data file to convert: its engine path, its Strata output path, its sort key, its size.
struct Job {
    table: String,
    src: PathBuf,
    st: PathBuf,
    sort: &'static [&'static str],
    size: u64,
}

/// The sort key of a table's files, as `crates/storage/src/parquet.rs` sorts them.
fn sort_key(table: &str) -> R<&'static [&'static str]> {
    match table {
        "spans" => Ok(&["trace_id", "start_time"]),
        "traces" => Ok(&["start_time", "trace_id"]),
        "scores" => Ok(&["created_at"]),
        other => Err(format!("unknown table {other}").into()),
    }
}

/// The path components of `path` below `root`.
fn below(root: &Path, path: &Path) -> R<Vec<String>> {
    Ok(path
        .strip_prefix(root)?
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect())
}

/// Every file with extension `ext` under `root`, sorted.
fn files_with_ext(root: &Path, ext: &str) -> R<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == ext) {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Shuts the engine's files into `pq_cetra/` (hard links) and lists the conversions: every
/// `*.parquet` under `engine/objects/tables/<table>/project=<id>/day=<d>/`.
fn plan_layout(dir: &Path) -> R<Vec<Job>> {
    let tables = dir.join("engine/objects/tables");
    let mut jobs = Vec::new();
    for src in files_with_ext(&tables, "parquet")? {
        let parts = below(&tables, &src)?;
        let [table, _project, day, name] = parts.as_slice() else {
            return Err(format!("unexpected object path {}", src.display()).into());
        };
        if !day.starts_with("day=") {
            return Err(format!("unexpected object path {}", src.display()).into());
        }
        let size = fs::metadata(&src)?.len();
        let pq_dir = dir.join("pq_cetra").join(table).join(day);
        let st_dir = dir.join("st").join(table).join(day);
        fs::create_dir_all(&pq_dir)?;
        fs::create_dir_all(&st_dir)?;
        fs::hard_link(&src, pq_dir.join(name))?;
        jobs.push(Job {
            table: table.clone(),
            src,
            st: st_dir.join(Path::new(name).with_extension("strata")),
            sort: sort_key(table)?,
            size,
        });
    }
    Ok(jobs)
}

/// Converts one Parquet file to Strata: rows and the Strata file's size.
fn convert(job: &Job) -> R<(u64, u64)> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(&job.src)?)?.with_batch_size(8_192);
    let schema = builder.schema().clone();
    let mut writer = StrataWriter::new(schema, job.sort, Config::default())?;
    let mut rows = 0u64;
    for batch in builder.build()? {
        let batch = batch?;
        rows += batch.num_rows() as u64;
        writer.write(&batch)?;
    }
    let (bytes, _summary) = writer.finish()?;
    fs::write(&job.st, &bytes)?;
    Ok((rows, bytes.len() as u64))
}

/// Runs `convert` on every job: large files one at a time, smaller ones `CONVERT_AT_ONCE` at once.
/// Returns (rows, Strata bytes) per job, in job order.
fn convert_all(jobs: &[Job]) -> R<Vec<(u64, u64)>> {
    let mut out = Vec::with_capacity(jobs.len());
    let mut i = 0;
    while i < jobs.len() {
        if jobs[i].size >= CONVERT_ALONE_BYTES {
            out.push(convert(&jobs[i])?);
            i += 1;
            continue;
        }
        let mut group = Vec::new();
        while i < jobs.len() && jobs[i].size < CONVERT_ALONE_BYTES && group.len() < CONVERT_AT_ONCE {
            group.push(&jobs[i]);
            i += 1;
        }
        let results: Vec<R<(u64, u64)>> = std::thread::scope(|s| {
            let handles: Vec<_> = group.iter().map(|job| s.spawn(move || convert(job))).collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err("a conversion thread panicked".into())))
                .collect()
        });
        for r in results {
            out.push(r?);
        }
    }
    Ok(out)
}

/// The small form's files must all be level 0 (`0-` prefix): a background merge would change the
/// layout the form is for.
fn check_small_levels(jobs: &[Job]) -> R<()> {
    for job in jobs {
        let name = job.src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if name.split('-').next().unwrap_or_default() != "0" {
            return Err("small form: a background merge produced level-1 files; lower --flushes".into());
        }
    }
    Ok(())
}

/// The converted files' rows, per table, must equal the rows the generator appended for `spans`
/// and `scores`; otherwise an obsolete file was left in the layout.
fn check_layout_rows(jobs: &[Job], converted: &[(u64, u64)], spans: u64, scores: u64) -> R<()> {
    for (table, generated) in [("spans", spans), ("scores", scores)] {
        let rows: u64 = jobs.iter().zip(converted).filter(|(job, _)| job.table == table).map(|(_, (r, _))| *r).sum();
        if rows != generated {
            return Err(format!(
                "{table}: {rows} rows in the layout for {generated} generated: an obsolete file was left in the layout"
            )
            .into());
        }
    }
    Ok(())
}

fn print_build_stats(jobs: &[Job], converted: &[(u64, u64)]) {
    println!("{:<8} {:>7} {:>14} {:>12} {:>12}", "table", "files", "rows", "pq_cetra MiB", "st MiB");
    for table in TABLES {
        let (mut files, mut rows, mut pq, mut st) = (0u64, 0u64, 0u64, 0u64);
        for (job, (r, b)) in jobs.iter().zip(converted) {
            if job.table == table {
                files += 1;
                rows += r;
                pq += job.size;
                st += b;
            }
        }
        println!(
            "{:<8} {:>7} {:>14} {:>12.2} {:>12.2}",
            table,
            files,
            rows,
            pq as f64 / 1_048_576.0,
            st as f64 / 1_048_576.0
        );
    }
}

/// One row of the layout report: the file sizes of one table and level, per format.
#[derive(Default)]
struct Cell {
    pq: Vec<u64>,
    st: Vec<u64>,
}

/// The Parquet and Strata files of a project, by (table, level).
fn layout_cells(dir: &Path) -> R<BTreeMap<(String, String), Cell>> {
    let mut cells: BTreeMap<(String, String), Cell> = BTreeMap::new();
    for (root, ext) in [("pq_cetra", "parquet"), ("st", "strata")] {
        let root = dir.join(root);
        for path in files_with_ext(&root, ext)? {
            let parts = below(&root, &path)?;
            let [table, _day, name] = parts.as_slice() else {
                return Err(format!("unexpected file {}", path.display()).into());
            };
            let level = name.split('-').next().unwrap_or_default().to_string();
            let size = fs::metadata(&path)?.len();
            let cell = cells.entry((table.clone(), level)).or_default();
            if ext == "parquet" {
                cell.pq.push(size);
            } else {
                cell.st.push(size);
            }
        }
    }
    Ok(cells)
}

/// The median of `sizes`, or `None` when empty.
fn median(sizes: &mut [u64]) -> Option<u64> {
    sizes.sort_unstable();
    sizes.get(sizes.len() / 2).copied()
}

/// File counts, total and median sizes per table and level, per format, and the Strata / Parquet
/// ratio of the totals.
pub fn layout_report(dir: &Path) -> R<String> {
    let mut cells = layout_cells(dir)?;
    let mut out = String::new();
    writeln!(
        out,
        "{:<8} {:<5} {:>8} {:>10} {:>10} {:>8} {:>10} {:>10} {:>7}",
        "table", "level", "pq_files", "pq MiB", "pq med KiB", "st_files", "st MiB", "st med KiB", "st/pq"
    )?;
    for ((table, level), cell) in cells.iter_mut() {
        let pq_total: u64 = cell.pq.iter().sum();
        let st_total: u64 = cell.st.iter().sum();
        let pq_med = median(&mut cell.pq).map_or("-".to_string(), |m| format!("{:.1}", m as f64 / 1024.0));
        let st_med = median(&mut cell.st).map_or("-".to_string(), |m| format!("{:.1}", m as f64 / 1024.0));
        let ratio = if pq_total > 0 { format!("{:.2}", st_total as f64 / pq_total as f64) } else { "-".to_string() };
        writeln!(
            out,
            "{:<8} {:<5} {:>8} {:>10.2} {:>10} {:>8} {:>10.2} {:>10} {:>7}",
            table,
            level,
            cell.pq.len(),
            pq_total as f64 / 1_048_576.0,
            pq_med,
            cell.st.len(),
            st_total as f64 / 1_048_576.0,
            st_med,
            ratio
        )?;
    }
    Ok(out)
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    use arrow::array::FixedSizeBinaryArray;
    use object_store::ObjectStore;
    use object_store::local::LocalFileSystem;
    use object_store::path::Path as ObjPath;
    use strata::StrataFile;

    use super::*;

    /// An empty scratch directory for one test: a project lives in `<it>/p`.
    fn scratch(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cw-test-project-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn args(dir: &Path, extra: &[&str]) -> Vec<String> {
        let mut v = vec![dir.display().to_string()];
        v.extend(extra.iter().map(|s| s.to_string()));
        v
    }

    /// Row count of every table's files in a directory of `pq_cetra` or `st`, by (table, day, stem).
    fn stems(root: &Path, ext: &str) -> HashSet<(String, String, String)> {
        files_with_ext(root, ext)
            .unwrap()
            .iter()
            .map(|p| {
                let parts = below(root, p).unwrap();
                let stem = Path::new(&parts[2]).with_extension("").display().to_string();
                (parts[0].clone(), parts[1].clone(), stem)
            })
            .collect()
    }

    /// Parquet row count of a file.
    fn parquet_rows(path: &Path) -> u64 {
        ParquetRecordBatchReaderBuilder::try_new(fs::File::open(path).unwrap())
            .unwrap()
            .metadata()
            .file_metadata()
            .num_rows() as u64
    }

    /// Strata row count of a file.
    async fn strata_rows(path: &Path) -> u64 {
        let store: Arc<dyn ObjectStore> = Arc::new(LocalFileSystem::new());
        let obj = ObjPath::from_absolute_path(path).unwrap();
        StrataFile::open(store, obj, None, &Config::default()).await.unwrap().row_count()
    }

    /// Every batch of every `spans` Parquet file of a project.
    fn spans_batches(dir: &Path) -> Vec<RecordBatch> {
        let mut out = Vec::new();
        for path in files_with_ext(&dir.join("pq_cetra/spans"), "parquet").unwrap() {
            let reader = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(path).unwrap()).unwrap().build().unwrap();
            out.extend(reader.map(|b| b.unwrap()));
        }
        out
    }

    /// The 20 trace ids of `cw_traces.txt`, the times of `cw_around.txt`, and the clock.
    fn read_texts(dir: &Path) -> (Vec<String>, Vec<i64>, i64) {
        let ids: Vec<String> = fs::read_to_string(dir.join("cw_traces.txt")).unwrap().lines().map(String::from).collect();
        let around: Vec<i64> = fs::read_to_string(dir.join("cw_around.txt"))
            .unwrap()
            .lines()
            .map(|l| l.parse().unwrap())
            .collect();
        let clock: i64 = fs::read_to_string(dir.join("cw_clock.txt")).unwrap().trim().parse().unwrap();
        (ids, around, clock)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn small_project_round_trips() {
        let base = scratch("round-trip");
        let dir = base.join("p");
        project_cmd(&args(&dir, &["--days", "2", "--traces-per-day", "300"])).await.unwrap();

        let pq = stems(&dir.join("pq_cetra"), "parquet");
        let st = stems(&dir.join("st"), "strata");
        assert_eq!(pq, st, "Parquet and Strata files differ");
        for table in TABLES {
            assert!(pq.iter().any(|(t, _, _)| t == table), "no {table} files");
        }
        // Each Strata file holds the rows of its Parquet file.
        for (table, day, stem) in &pq {
            let pq_path = dir.join("pq_cetra").join(table).join(day).join(format!("{stem}.parquet"));
            let st_path = dir.join("st").join(table).join(day).join(format!("{stem}.strata"));
            assert_eq!(strata_rows(&st_path).await, parquet_rows(&pq_path), "{table}/{stem}");
        }
        // The live part left level-0 spans files.
        assert!(
            files_with_ext(&dir.join("pq_cetra/spans"), "parquet")
                .unwrap()
                .iter()
                .any(|p| p.file_name().unwrap().to_string_lossy().starts_with("0-")),
            "no level-0 spans file"
        );
        // The text files parse.
        let (ids, around, clock) = read_texts(&dir);
        assert_eq!(ids.len(), 20);
        assert!(ids.iter().all(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())));
        assert_eq!(around.len(), 20);
        assert!(clock > PROJECT_START);
        // The layout report names every table and both formats, and counts every file.
        let report = layout_report(&dir).unwrap();
        for table in TABLES {
            assert!(report.lines().any(|l| l.starts_with(table)), "{table} missing from the report");
        }
        let cells = layout_cells(&dir).unwrap();
        let pq_count: usize = cells.values().map(|c| c.pq.len()).sum();
        assert_eq!(pq_count, files_with_ext(&dir.join("pq_cetra"), "parquet").unwrap().len());
        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn layout_counts_every_file() {
        let base = scratch("layout");
        let dir = base.join("p");
        project_cmd(&args(&dir, &["--days", "2", "--traces-per-day", "300"])).await.unwrap();
        let cells = layout_cells(&dir).unwrap();
        let mut per_table: HashMap<String, usize> = HashMap::new();
        for ((table, _), cell) in &cells {
            *per_table.entry(table.clone()).or_default() += cell.pq.len();
            assert_eq!(cell.pq.len(), cell.st.len(), "{table}: formats differ");
        }
        for table in TABLES {
            let n = files_with_ext(&dir.join("pq_cetra").join(table), "parquet").unwrap().len();
            assert_eq!(per_table.get(table).copied().unwrap_or(0), n, "{table}");
        }
        let report = layout_report(&dir).unwrap();
        for table in TABLES {
            assert!(report.lines().any(|l| l.starts_with(table)), "{table} missing from the report");
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn payload_share_is_applied() {
        let base = scratch("payload");
        let case = base.join("case");
        fs::create_dir_all(&case).unwrap();
        let (spans, files) = cases::gen_spans("text_unique", Some(50)).unwrap();
        cases::write_case(&case, spans, &files).unwrap();

        let dir = base.join("p");
        let mut a = args(&dir, &["--days", "1", "--traces-per-day", "2000"]);
        a.push("--payloads".into());
        a.push(case.display().to_string());
        project_cmd(&a).await.unwrap();

        let mut traces = HashSet::new();
        let mut with_payload = HashSet::new();
        for batch in spans_batches(&dir) {
            let ids = batch
                .column_by_name("trace_id")
                .unwrap()
                .as_any()
                .downcast_ref::<FixedSizeBinaryArray>()
                .unwrap();
            let input = batch.column_by_name("input").unwrap();
            for i in 0..batch.num_rows() {
                traces.insert(ids.value(i).to_vec());
                if !input.is_null(i) {
                    with_payload.insert(ids.value(i).to_vec());
                }
            }
        }
        let share = with_payload.len() as f64 / traces.len() as f64;
        assert!((0.01..=0.05).contains(&share), "payload share {share} of {} traces", traces.len());
        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn small_files_project() {
        let base = scratch("small");
        let dir = base.join("p");
        project_cmd(&args(&dir, &["--small", "500", "--flushes", "4"])).await.unwrap();
        for table in TABLES {
            let files = files_with_ext(&dir.join("pq_cetra").join(table), "parquet").unwrap();
            let level0 = files
                .iter()
                .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("0-"))
                .count();
            assert_eq!(level0, 4, "{table}: level-0 files");
            assert_eq!(files.len(), 4, "{table}: files");
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn small_form_refuses_more_than_a_day() {
        let base = scratch("refuse");
        let dir = base.join("p");
        let err = project_cmd(&args(&dir, &["--small", "100000", "--flushes", "10"])).await.unwrap_err();
        assert!(err.to_string().contains("too many spans for one day"), "{err}");
        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn layout_rows_match_the_generator() {
        let base = scratch("layout-rows");
        let dir = base.join("p");
        // The check runs inside project_cmd: the build succeeds only if the rows match.
        project_cmd(&args(&dir, &["--days", "2", "--traces-per-day", "300"])).await.unwrap();

        // A deliberately wrong expected count must fail with the message of the check.
        let job = |table: &str| Job { table: table.into(), src: PathBuf::new(), st: PathBuf::new(), sort: &[], size: 0 };
        let jobs = vec![job("spans")];
        let converted = vec![(10u64, 0u64)];
        check_layout_rows(&jobs, &converted, 10, 0).unwrap();
        let err = check_layout_rows(&jobs, &converted, 11, 0).unwrap_err();
        assert!(
            err.to_string()
                .contains("spans: 10 rows in the layout for 11 generated: an obsolete file was left in the layout"),
            "{err}"
        );

        // The small form refuses a level-1 file.
        let level1 = vec![Job { src: PathBuf::from("1-x.parquet"), ..job("spans") }];
        let err = check_small_levels(&level1).unwrap_err();
        assert!(err.to_string().contains("small form: a background merge produced level-1 files"), "{err}");
        check_small_levels(&[Job { src: PathBuf::from("0-x.parquet"), ..job("spans") }]).unwrap();
        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn projects_write_needles() {
        let base = scratch("needles");
        let dir = base.join("p");
        project_cmd(&args(&dir, &["--days", "2", "--traces-per-day", "300"])).await.unwrap();
        let want: String = cases::PROJECT_NEEDLES
            .iter()
            .map(|(name, needle)| format!("{name}\t{}\n", crate::hex(needle.as_bytes())))
            .collect();
        let text = fs::read_to_string(dir.join("cw_needles.txt")).unwrap();
        assert_eq!(text, want);
        assert_eq!(text.lines().count(), 5);

        fs::remove_file(dir.join("cw_needles.txt")).unwrap();
        needles_cmd(&[dir.display().to_string()]).unwrap();
        assert_eq!(fs::read_to_string(dir.join("cw_needles.txt")).unwrap(), want);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn parse_takes_options_in_any_order() {
        let opts = parse_project(&args(Path::new("/nonexistent-cw-p"), &["--traces-per-day", "5", "--days", "2"])).unwrap();
        match opts.shape {
            Shape::Days { days, traces_per_day, live_hours, payloads } => {
                assert_eq!((days, traces_per_day, live_hours), (2, 5, 6));
                assert!(payloads.is_empty());
            }
            other => panic!("{other:?}"),
        }
        assert!(parse_project(&args(Path::new("/nonexistent-cw-p"), &["--small", "5"])).is_err());
        assert!(parse_project(&args(Path::new("/nonexistent-cw-p"), &["--days", "2", "--small", "5", "--flushes", "1"])).is_err());
    }
}
