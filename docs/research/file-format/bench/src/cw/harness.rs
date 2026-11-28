//! `fmtbench cw run` and `cw one`: each query of a case runs in fresh processes, the two formats
//! alternating round by round, and the results are summarised with a verdict per query
//! (plans/08-m1-likely-failures.md § M1.2 and § M1.4). Every query runs through `one`: `write`
//! through `write_format`, `compact` and `flush:<n>` through `cw::writes`, the reads through
//! `cw::read`.

use std::fs::OpenOptions;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use arrow::array::RecordBatch;
use arrow::compute::concat_batches;
use arrow::datatypes::SchemaRef;
use arrow::ipc::reader::FileReader;
use cpu_time::ProcessTime;
use parquet::file::metadata::{PageIndexPolicy, ParquetMetaDataReader};
use serde_json::{Value, json};
use xxhash_rust::xxh3::xxh3_64;

use crate::cw::load;
use crate::cw::many;
use crate::cw::read::{self, Files};
use crate::cw::shapes::{self, ShapeCase};
use crate::cw::writes;
use crate::profile::unhex;
use crate::{location, narrow_table, open_pq_fast, open_strata, parse_id, payload_table, read_text, write_parquet, write_strata, R};

/// The two formats, Parquet first.
const FORMATS: [&str; 2] = ["pq_cetra", "st"];
/// Usage of `cw run`, in the error for bad arguments.
const RUN_USAGE: &str = "usage: fmtbench cw run <dir|s3://bucket/key> [--rounds R] [--runs K] [--queries a,b] [--out FILE]";
/// Usage of `cw one`, in the error for bad arguments.
const ONE_USAGE: &str = "usage: fmtbench cw one <dir|s3://bucket/key> <format> <query> <K>";

/// One process's result: the JSON line `cw one` prints (plans/08-m1-likely-failures.md § M1.2).
#[derive(Debug, Clone, PartialEq)]
pub struct ProcResult {
    /// The query's name.
    pub query: String,
    /// `pq_cetra` or `st`.
    pub format: String,
    /// Wall time of each measured run, in ms.
    pub runs_ms: Vec<f64>,
    /// Process CPU time of each measured run, in ms.
    pub cpu_ms: Vec<f64>,
    /// Object GETs after the first measured run (zero for `write`).
    pub gets: u64,
    /// Bytes read after the first measured run (zero for `write`).
    pub bytes: u64,
    /// Strata I/O waves; `None` for Parquet and for `write`.
    pub waves: Option<u64>,
    /// Rows the query found, or written.
    pub rows: u64,
    /// The answer's hash as 16 lowercase hex digits.
    pub answer: String,
    /// `VmRSS` in bytes when the process started.
    pub rss_start: u64,
    /// `VmHWM` in bytes when the process ended.
    pub vmhwm: u64,
    /// Total bytes of the files `write` wrote; `None` for the reads.
    pub bytes_written: Option<u64>,
    /// The metadata each format reads at open, summed over the files; `None` for the reads.
    pub tail_bytes: Option<u64>,
}

impl ProcResult {
    /// The JSON line (`serde_json::json!(…).to_string()`).
    pub fn to_line(&self) -> String {
        json!({
            "query": self.query,
            "format": self.format,
            "runs_ms": self.runs_ms,
            "cpu_ms": self.cpu_ms,
            "gets": self.gets,
            "bytes": self.bytes,
            "waves": self.waves,
            "rows": self.rows,
            "answer": self.answer,
            "rss_start": self.rss_start,
            "vmhwm": self.vmhwm,
            "bytes_written": self.bytes_written,
            "tail_bytes": self.tail_bytes,
        })
        .to_string()
    }

    /// Parses a line written by `to_line`. Extra keys (such as `round`) are ignored.
    pub fn from_line(line: &str) -> R<Self> {
        let v: Value = serde_json::from_str(line)?;
        Ok(Self {
            query: text(&v, "query")?,
            format: text(&v, "format")?,
            runs_ms: floats(&v, "runs_ms")?,
            cpu_ms: floats(&v, "cpu_ms")?,
            gets: uint(&v, "gets")?,
            bytes: uint(&v, "bytes")?,
            waves: opt_uint(&v, "waves")?,
            rows: uint(&v, "rows")?,
            answer: text(&v, "answer")?,
            rss_start: uint(&v, "rss_start")?,
            vmhwm: uint(&v, "vmhwm")?,
            bytes_written: opt_uint(&v, "bytes_written")?,
            tail_bytes: opt_uint(&v, "tail_bytes")?,
        })
    }
}

/// The value of `key` in a result line.
fn field<'a>(v: &'a Value, key: &str) -> R<&'a Value> {
    v.get(key).ok_or_else(|| format!("result line has no `{key}`").into())
}

/// The string at `key`.
fn text(v: &Value, key: &str) -> R<String> {
    field(v, key)?.as_str().map(str::to_owned).ok_or_else(|| format!("`{key}` is not a string").into())
}

/// The unsigned integer at `key`.
fn uint(v: &Value, key: &str) -> R<u64> {
    field(v, key)?.as_u64().ok_or_else(|| format!("`{key}` is not an unsigned integer").into())
}

/// The unsigned integer at `key`, or `None` for `null`.
fn opt_uint(v: &Value, key: &str) -> R<Option<u64>> {
    if field(v, key)?.is_null() { Ok(None) } else { uint(v, key).map(Some) }
}

/// The array of numbers at `key`.
fn floats(v: &Value, key: &str) -> R<Vec<f64>> {
    let arr = field(v, key)?.as_array().ok_or_else(|| format!("`{key}` is not an array"))?;
    arr.iter()
        .map(|x| x.as_f64().ok_or_else(|| format!("`{key}` holds a non-number")))
        .collect::<Result<Vec<f64>, String>>()
        .map_err(Into::into)
}

/// A format's results for one query, merged over all rounds.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    /// Every measured run of every round.
    pub runs_ms: Vec<f64>,
    /// Process CPU time of every measured run of every round.
    pub cpu_ms: Vec<f64>,
    /// From the first round.
    pub gets: u64,
    /// From the first round.
    pub bytes: u64,
    /// From the first round.
    pub waves: Option<u64>,
    /// From the first round.
    pub rows: u64,
    /// From the first round.
    pub answer: String,
    /// From the first round.
    pub bytes_written: Option<u64>,
    /// From the first round.
    pub tail_bytes: Option<u64>,
    /// The largest `vmhwm` of all rounds.
    pub vmhwm_max: u64,
    /// The largest `vmhwm - rss_start` of all rounds: for the write-like queries, the memory the
    /// runs added to what the process held when timing started.
    pub rss_delta_max: u64,
}

impl Summary {
    /// Merges the results of one format, in round order. `results` is not empty.
    pub fn from_results(results: &[ProcResult]) -> Summary {
        let first = &results[0];
        Summary {
            runs_ms: results.iter().flat_map(|r| r.runs_ms.iter().copied()).collect(),
            cpu_ms: results.iter().flat_map(|r| r.cpu_ms.iter().copied()).collect(),
            gets: first.gets,
            bytes: first.bytes,
            waves: first.waves,
            rows: first.rows,
            answer: first.answer.clone(),
            bytes_written: first.bytes_written,
            tail_bytes: first.tail_bytes,
            vmhwm_max: results.iter().map(|r| r.vmhwm).max().unwrap_or(0),
            rss_delta_max: results.iter().map(|r| r.vmhwm.saturating_sub(r.rss_start)).max().unwrap_or(0),
        }
    }
}

/// The p-th percentile of `v` (0 < p <= 1) by nearest rank: the value at sorted
/// position `ceil(p * n) - 1`. `v` must not be empty.
fn percentile(v: &[f64], p: f64) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let rank = ((p * s.len() as f64).ceil() as usize).clamp(1, s.len());
    s[rank - 1]
}

/// Queries that write files: `write`, `compact` and `flush:<n>`. They run locally only, take no
/// warm-up, and are judged on CPU time and bytes written too (CW-16, CW-12).
pub(crate) fn is_write_like(query: &str) -> bool {
    matches!(query, "write" | "compact") || query.starts_with("flush:")
}

/// The writer count of `flush:<n>`: n from 1 to 64.
fn flush_writers(query: &str) -> R<usize> {
    let n = query.strip_prefix("flush:").ok_or("not a flush query")?;
    let n: usize = n.parse().map_err(|e| format!("{query}: {e}"))?;
    if (1..=64).contains(&n) { Ok(n) } else { Err(format!("{query}: n must be from 1 to 64").into()) }
}

/// Whether Strata wins: every compared figure strictly lower than Parquet's.
/// Figures: median and p95 latency and max VmHWM; for the write queries, the memory used
/// (`rss_delta_max`) in place of VmHWM, and also the median CPU time and the bytes written.
/// Returns the names of the figures Strata loses on (empty = win).
fn losses(query: &str, st: &Summary, pq: &Summary) -> Vec<&'static str> {
    let mut lost = Vec::new();
    if percentile(&st.runs_ms, 0.50) >= percentile(&pq.runs_ms, 0.50) {
        lost.push("median");
    }
    if percentile(&st.runs_ms, 0.95) >= percentile(&pq.runs_ms, 0.95) {
        lost.push("p95");
    }
    let (st_rss, pq_rss) = if is_write_like(query) {
        (st.rss_delta_max, pq.rss_delta_max)
    } else {
        (st.vmhwm_max, pq.vmhwm_max)
    };
    if st_rss >= pq_rss {
        lost.push("rss");
    }
    if is_write_like(query) {
        if percentile(&st.cpu_ms, 0.50) >= percentile(&pq.cpu_ms, 0.50) {
            lost.push("cpu");
        }
        if st.bytes_written.unwrap_or(0) >= pq.bytes_written.unwrap_or(0) {
            lost.push("size");
        }
    }
    if query.starts_with("cache_mix:") {
        // A tie at zero is not a loss: both formats served pass 2 from the cache alone.
        if st.gets >= pq.gets && !(st.gets == 0 && pq.gets == 0) {
            lost.push("gets");
        }
        if st.bytes >= pq.bytes && !(st.bytes == 0 && pq.bytes == 0) {
            lost.push("bytes");
        }
    }
    lost
}

/// The two formats in the order round `r` runs them: Parquet first in even rounds.
pub fn formats_for_round(r: usize) -> [&'static str; 2] {
    if r % 2 == 0 { ["pq_cetra", "st"] } else { ["st", "pq_cetra"] }
}

/// The arguments of `cw run`.
struct RunArgs {
    /// The case directory, or `s3://bucket/key`.
    dir: String,
    /// Rounds per query (`--rounds`, default 4: even, so each format runs first as often).
    rounds: usize,
    /// Measured runs per process (`--runs`, default 3).
    runs: usize,
    /// `--queries`; `None` reads `cw_queries.txt`.
    queries: Option<Vec<String>>,
    /// `--out`; `None` writes `cw-<unix seconds>.jsonl` in the temp directory.
    out: Option<PathBuf>,
}

/// Parses `cw run`'s arguments (after `run`).
fn parse_run_args(args: &[String]) -> R<RunArgs> {
    let dir = args.first().ok_or(RUN_USAGE)?.clone();
    let mut a = RunArgs { dir, rounds: 4, runs: 3, queries: None, out: None };
    let mut i = 1;
    while i < args.len() {
        let flag = args[i].as_str();
        let value = args.get(i + 1).ok_or_else(|| format!("{flag} needs a value"))?;
        match flag {
            "--rounds" => a.rounds = value.parse()?,
            "--runs" => a.runs = value.parse()?,
            "--queries" => {
                let list: Vec<String> = value.split(',').map(str::trim).filter(|q| !q.is_empty()).map(str::to_owned).collect();
                if list.is_empty() {
                    return Err("--queries names no query".into());
                }
                a.queries = Some(list);
            }
            "--out" => a.out = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown option {flag}\n{RUN_USAGE}").into()),
        }
        i += 2;
    }
    if a.rounds == 0 || a.runs == 0 {
        return Err("--rounds and --runs must be at least 1".into());
    }
    Ok(a)
}

/// Seconds since the Unix epoch.
fn unix_seconds() -> R<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

/// Runs `cw one` in a child process and returns its result line. The child's other stdout
/// lines and its stderr are printed; its exit status must be success.
fn spawn_one(exe: &Path, dir: &str, format: &str, query: &str, runs: usize) -> R<String> {
    let k = runs.to_string();
    let out = Command::new(exe).args(["cw", "one", dir, format, query, k.as_str()]).output()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut results = Vec::new();
    for line in stdout.lines() {
        if line.starts_with('{') {
            results.push(line.to_owned());
        } else {
            println!("{line}");
        }
    }
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        return Err(format!("cw one {dir} {format} {query} failed: {}", out.status).into());
    }
    if results.len() != 1 {
        return Err(format!("cw one {dir} {format} {query} printed {} result lines, want 1", results.len()).into());
    }
    Ok(results.remove(0))
}

/// One summary line: the medians, p95, p99, CPU, I/O, peak RSS and rows of one format.
fn print_line(query: &str, format: &str, s: &Summary) {
    // A write-like query's memory is what its runs used (`rss_delta_max`), not the process peak.
    let (rss_label, rss) = if is_write_like(query) { ("rss_used", s.rss_delta_max) } else { ("rss_max", s.vmhwm_max) };
    let mut line = format!(
        "{query:<14}{format:<10}median {:>8.2}  p95 {:>8.2}  p99 {:>8.2}  cpu {:>8.1}  gets {:>6}  KiB {:>9.1}  waves {}  {rss_label} {:>7.1} MiB  rows {}",
        percentile(&s.runs_ms, 0.50),
        percentile(&s.runs_ms, 0.95),
        percentile(&s.runs_ms, 0.99),
        percentile(&s.cpu_ms, 0.50),
        s.gets,
        s.bytes as f64 / 1024.0,
        s.waves.map_or("-".to_owned(), |w| w.to_string()),
        rss as f64 / 1048576.0,
        s.rows,
    );
    if is_write_like(query) {
        line.push_str(&format!("  size {:.1} MiB", s.bytes_written.unwrap_or(0) as f64 / 1048576.0));
    }
    println!("{line}");
}

/// `cw run`: spawns the processes, checks the answers, prints the summaries and verdicts.
/// Returns an error after all queries if any answer mismatched.
pub async fn run_cmd(args: &[String]) -> R<()> {
    let a = parse_run_args(args)?;
    let is_s3 = a.dir.starts_with("s3://");
    let loc = location(&a.dir)?;
    let queries = match a.queries {
        Some(q) => q,
        None => read_text(&loc, "cw_queries.txt")
            .await?
            .ok_or("no cw_queries.txt in the case: pass --queries")?
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect(),
    };
    let out_path = match a.out {
        Some(p) => p,
        None => std::env::temp_dir().join(format!("cw-{}.jsonl", unix_seconds()?)),
    };
    let mut out = BufWriter::new(OpenOptions::new().create(true).append(true).open(&out_path)?);
    println!("results: {}", out_path.display());
    let exe = std::env::current_exe()?;
    let mut mismatch = false;
    for query in &queries {
        if is_s3 && is_write_like(query) {
            println!("{query}: skipped, write queries are local only");
            continue;
        }
        // Results per format: [pq_cetra, st].
        let mut results: [Vec<ProcResult>; 2] = [Vec::new(), Vec::new()];
        for round in 0..a.rounds {
            for format in formats_for_round(round) {
                let line = spawn_one(&exe, &a.dir, format, query, a.runs)?;
                let mut record: Value = serde_json::from_str(&line)?;
                record["round"] = json!(round);
                writeln!(out, "{record}")?;
                results[usize::from(format == "st")].push(ProcResult::from_line(&line)?);
            }
        }
        let pq_first = &results[0][0];
        let st_first = &results[1][0];
        // The answers of the write queries are constant (`write`, `flush:<n>`) or not compared.
        if query != "write" && !query.starts_with("flush:") {
            let same = results.iter().flatten().all(|r| r.answer == pq_first.answer && r.rows == pq_first.rows);
            if !same {
                mismatch = true;
                println!(
                    "{query} ANSWER MISMATCH  pq_cetra rows {} answer {}  st rows {} answer {}",
                    pq_first.rows, pq_first.answer, st_first.rows, st_first.answer
                );
            }
        }
        let pq = Summary::from_results(&results[0]);
        let st = Summary::from_results(&results[1]);
        print_line(query, "pq_cetra", &pq);
        print_line(query, "st", &st);
        let lost = losses(query, &st, &pq);
        if lost.is_empty() {
            let compared = if is_write_like(query) {
                "median, p95, rss, cpu, size"
            } else if query.starts_with("cache_mix:") {
                "median, p95, rss, gets, bytes"
            } else {
                "median, p95, rss"
            };
            println!("{query:<14}verdict   WIN ({compared})");
        } else {
            println!("{query:<14}verdict   LOSS ({})", lost.join(", "));
        }
    }
    out.flush()?;
    if mismatch {
        return Err("answers differ between the formats (see ANSWER MISMATCH above)".into());
    }
    Ok(())
}

/// `cw one`'s arguments (after `one`): runs `one` and prints its result line.
pub async fn one_cmd(args: &[String]) -> R<()> {
    let [dir, format, query, runs] = args else {
        return Err(ONE_USAGE.into());
    };
    let runs: usize = runs.parse().map_err(|e| format!("K: {e}"))?;
    let result = one(dir, format, query, runs).await?;
    println!("{}", result.to_line());
    Ok(())
}

/// Runs `query` K times in this process, as `cw one` does, and returns its result.
pub async fn one(dir: &str, format: &str, query: &str, runs: usize) -> R<ProcResult> {
    check_format(format)?;
    if many::is_many(query) {
        return many_format(dir, format, query, runs).await;
    }
    if shapes::is_shape(query) {
        return shape_format(dir, format, query, runs).await;
    }
    if load::is_load(query) {
        return load::load_format(dir, format, query, runs).await;
    }
    if is_write_like(query) {
        if dir.starts_with("s3://") {
            return Err(format!("{query} is local only: give a local case directory").into());
        }
        let dir = Path::new(dir);
        return match query {
            "write" => write_format(dir, format, runs),
            "compact" => writes::compact_format(dir, format, runs).await,
            _ => writes::flush_format(dir, format, flush_writers(query)?, runs),
        };
    }
    read_format(dir, format, query, runs).await
}

/// The non-empty lines of a text file, trimmed; a missing file has none.
fn lines_of(text: Option<String>) -> Vec<String> {
    text.unwrap_or_default().lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect()
}

/// The case's text files (plans/08-m1-likely-failures.md § M1.1), read through `loc`'s store:
/// hex ids parsed with `parse_id` (a span's 8-byte id from its 16 hex digits), needles decoded
/// from hex to UTF-8. A missing file gives an empty list or `None`.
pub(crate) async fn read_case(loc: &crate::Location) -> R<read::Case> {
    let traces = lines_of(read_text(loc, "cw_traces.txt").await?).iter().map(|l| parse_id(l)).collect::<R<Vec<_>>>()?;
    let spans = lines_of(read_text(loc, "cw_spans.txt").await?)
        .iter()
        .map(|l| -> R<([u8; 16], [u8; 8])> {
            let (trace, span) = l.split_once(' ').ok_or("cw_spans.txt: want '<trace> <span>'")?;
            let span: [u8; 8] = unhex(span).try_into().map_err(|_| "cw_spans.txt: bad span id")?;
            Ok((parse_id(trace)?, span))
        })
        .collect::<R<Vec<_>>>()?;
    let needles = lines_of(read_text(loc, "cw_needles.txt").await?)
        .iter()
        .map(|l| -> R<(String, String)> {
            let (name, hex) = l.split_once('\t').ok_or("cw_needles.txt: want '<name>\\t<hex>'")?;
            Ok((name.to_owned(), String::from_utf8(unhex(hex))?))
        })
        .collect::<R<Vec<_>>>()?;
    let attr = match lines_of(read_text(loc, "cw_attr.txt").await?).first() {
        Some(l) => {
            let (key, value) = l.split_once('\t').ok_or("cw_attr.txt: want '<key>\\t<value>'")?;
            Some((key.to_owned(), value.to_owned()))
        }
        None => None,
    };
    Ok(read::Case { traces, spans, needles, attr })
}

/// A read query for `cw one`: the format's files are opened once, the case is read, the query
/// runs once as a warm-up, then K measured runs, each of which must give the warm-up's answer.
async fn read_format(dir: &str, format: &str, query: &str, runs: usize) -> R<ProcResult> {
    if runs == 0 {
        return Err("runs must be at least 1".into());
    }
    let rss_start = proc_status_bytes("VmRSS")?;
    let loc = location(dir)?;
    let files = if format == "pq_cetra" {
        Files::Parquet(open_pq_fast(&loc, "pq_cetra").await?)
    } else {
        Files::Strata(open_strata(&loc, "st").await?)
    };
    let case = read_case(&loc).await?;
    let warm = read::run(query, &files, &case).await?;
    loc.store.reset();
    if let Files::Strata(fs) = &files {
        for f in fs {
            f.reset_io_stats();
        }
    }
    let (mut runs_ms, mut cpu_ms) = (Vec::with_capacity(runs), Vec::with_capacity(runs));
    let (mut gets, mut bytes): (u64, u64) = (0, 0);
    let mut waves: Option<u64> = None;
    for k in 0..runs {
        let cpu0 = ProcessTime::now();
        let t0 = Instant::now();
        let answer = read::run(query, &files, &case).await?;
        runs_ms.push(t0.elapsed().as_secs_f64() * 1e3);
        cpu_ms.push(cpu0.elapsed().as_secs_f64() * 1e3);
        if answer != warm {
            return Err(format!("answer changed between runs ({query} {format})").into());
        }
        // I/O counters are read after the first measured run only (see ProcResult).
        if k == 0 {
            (gets, bytes) = loc.store.stats();
            if let Files::Strata(fs) = &files {
                waves = Some(fs.iter().map(|f| f.io_stats().waves).sum::<u64>());
            }
        }
    }
    Ok(ProcResult {
        query: query.to_owned(),
        format: format.to_owned(),
        runs_ms,
        cpu_ms,
        gets,
        bytes,
        waves,
        rows: warm.rows,
        answer: format!("{:016x}", warm.hash),
        rss_start,
        vmhwm: proc_status_bytes("VmHWM")?,
        bytes_written: None,
        tail_bytes: None,
    })
}

/// An M3 query for `cw one` (plans/08-m3-many-files.md § M3.2). The format's files are listed and
/// the case read outside the timed region. `rollup_30d` opens its `spans` files once and warms up
/// on them; the cold queries open their files inside each run (CW-24). The store's counters are
/// reset before each run, so `gets` and `bytes` are one cold run's (warm: the first measured run's).
/// Every run's answer must equal the reference: the warm-up's, else the first run's.
async fn many_format(dir: &str, format: &str, query: &str, runs: usize) -> R<ProcResult> {
    if runs == 0 {
        return Err("runs must be at least 1".into());
    }
    let rss_start = proc_status_bytes("VmRSS")?;
    let loc = location(dir)?;
    let listing = many::list(&loc, format).await?;
    let case = many::read_project_case(&loc).await?;
    let warm = if many::is_cold(query) { None } else { Some(many::open(&loc, format, &listing.spans).await?) };
    let mut reference: Option<read::Answer> = None;
    if let Some(w) = &warm {
        let (answer, _) = many::run(query, format, &loc, &listing, Some(w), &case).await?;
        reference = Some(answer);
    }
    let (mut runs_ms, mut cpu_ms) = (Vec::new(), Vec::new());
    let (mut gets, mut bytes): (u64, u64) = (0, 0);
    for k in 0..runs {
        loc.store.reset();
        let cpu0 = ProcessTime::now();
        let t0 = Instant::now();
        let (answer, per_id) = many::run(query, format, &loc, &listing, warm.as_ref(), &case).await?;
        if per_id.is_empty() {
            runs_ms.push(t0.elapsed().as_secs_f64() * 1e3);
            cpu_ms.push(cpu0.elapsed().as_secs_f64() * 1e3);
        } else {
            for (wall, cpu) in per_id {
                runs_ms.push(wall);
                cpu_ms.push(cpu);
            }
        }
        match reference {
            Some(r) if r != answer => return Err(format!("answer changed between runs ({query} {format})").into()),
            Some(_) => {}
            None => reference = Some(answer),
        }
        // I/O counters are read after the first measured run only (see ProcResult).
        if k == 0 {
            (gets, bytes) = loc.store.stats();
        }
    }
    let answer = reference.ok_or("no measured run")?;
    Ok(ProcResult {
        query: query.to_owned(),
        format: format.to_owned(),
        runs_ms,
        cpu_ms,
        gets,
        bytes,
        waves: None,
        rows: answer.rows,
        answer: format!("{:016x}", answer.hash),
        rss_start,
        vmhwm: proc_status_bytes("VmHWM")?,
        bytes_written: None,
        tail_bytes: None,
    })
}

/// An M4 trace or rollup query for `cw one` (plans/08-m4-query-shapes.md § M4.2). The format's
/// files of the query's table are listed and opened, and the case read, outside the timed region.
/// One warm-up run gives the reference answer; then the store's counters are reset and K measured
/// runs follow, each of which must give that answer. `gets` and `bytes` are the first measured run's.
async fn shape_format(dir: &str, format: &str, query: &str, runs: usize) -> R<ProcResult> {
    if runs == 0 {
        return Err("runs must be at least 1".into());
    }
    let rss_start = proc_status_bytes("VmRSS")?;
    let loc = location(dir)?;
    let listing = many::list(&loc, format).await?;
    let clock = many::read_project_case(&loc).await?.clock;
    let needles = read_case(&loc).await?.needles;
    let case = ShapeCase { clock, needles };
    let files = if shapes::table_of(query) == "traces" { &listing.traces } else { &listing.spans };
    let opened = many::open(&loc, format, files).await?;
    let reference = shapes::run(query, files, &opened, &case).await?;
    loc.store.reset();
    let (mut runs_ms, mut cpu_ms) = (Vec::with_capacity(runs), Vec::with_capacity(runs));
    let (mut gets, mut bytes): (u64, u64) = (0, 0);
    for k in 0..runs {
        let cpu0 = ProcessTime::now();
        let t0 = Instant::now();
        let answer = shapes::run(query, files, &opened, &case).await?;
        runs_ms.push(t0.elapsed().as_secs_f64() * 1e3);
        cpu_ms.push(cpu0.elapsed().as_secs_f64() * 1e3);
        if answer != reference {
            return Err(format!("answer changed between runs ({query} {format})").into());
        }
        // I/O counters are read after the first measured run only (see ProcResult).
        if k == 0 {
            (gets, bytes) = loc.store.stats();
        }
    }
    Ok(ProcResult {
        query: query.to_owned(),
        format: format.to_owned(),
        runs_ms,
        cpu_ms,
        gets,
        bytes,
        waves: None,
        rows: reference.rows,
        answer: format!("{:016x}", reference.hash),
        rss_start,
        vmhwm: proc_status_bytes("VmHWM")?,
        bytes_written: None,
        tail_bytes: None,
    })
}

/// Fails unless `format` is `pq_cetra` or `st`.
pub(crate) fn check_format(format: &str) -> R<()> {
    if FORMATS.contains(&format) { Ok(()) } else { Err(format!("unknown format {format}: want pq_cetra or st").into()) }
}

/// Reads `VmRSS` or `VmHWM` of this process from `/proc/self/status`, in bytes.
pub(crate) fn proc_status_bytes(key: &str) -> R<u64> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix(key).and_then(|r| r.strip_prefix(':')) {
            let kb: u64 = rest.trim().trim_end_matches("kB").trim().parse()?;
            return Ok(kb * 1024);
        }
    }
    Err(format!("no {key} in /proc/self/status").into())
}

/// Resets the kernel's peak-RSS mark to the current RSS (writing `5` to `/proc/self/clear_refs`),
/// then returns `VmRSS`. The write-like queries call it right before their first timed run, so
/// that `VmHWM` read after the runs is the peak during the runs (see `Summary::rss_delta_max`).
pub(crate) fn start_peak_rss() -> R<u64> {
    std::fs::write("/proc/self/clear_refs", "5")?;
    proc_status_bytes("VmRSS")
}

/// Reads every `input/*.arrow` file of the case, in name order, as (name without extension,
/// schema, batches).
fn read_inputs(dir: &Path) -> R<Vec<(String, SchemaRef, Vec<RecordBatch>)>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir.join("input"))? {
        paths.push(entry?.path());
    }
    paths.retain(|p| p.extension().and_then(|e| e.to_str()) == Some("arrow"));
    paths.sort();
    if paths.is_empty() {
        return Err(format!("no input/*.arrow files in {}", dir.display()).into());
    }
    let mut out = Vec::new();
    for p in paths {
        let stem = p.file_stem().and_then(|s| s.to_str()).ok_or("input file name is not UTF-8")?.to_owned();
        let reader = FileReader::try_new(std::fs::File::open(&p)?, None)?;
        let schema = reader.schema();
        let batches = reader.collect::<Result<Vec<_>, _>>()?;
        out.push((stem, schema, batches));
    }
    Ok(out)
}

/// One batch holding an input file's rows, for the writers that take one batch.
fn concat_file(stem: &str, schema: &SchemaRef, batches: &[RecordBatch]) -> R<RecordBatch> {
    match batches {
        [] => Ok(RecordBatch::new_empty(schema.clone())),
        [one] => Ok(one.clone()),
        many => concat_batches(schema, many).map_err(|e| {
            format!("input file {stem} is too large for one batch: write the dataset with more, smaller files (fmtbench gen … <files>) and import it again ({e})").into()
        }),
    }
}

/// Bytes of the metadata a reader loads when it opens a Parquet file: the footer (the 4-byte
/// little-endian length before `PAR1`, plus 8) and the column and offset indexes of every
/// column chunk.
fn parquet_tail_bytes(path: &Path) -> R<u64> {
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::End(-8))?;
    let mut tail = [0u8; 8];
    f.read_exact(&mut tail)?;
    if &tail[4..] != b"PAR1" {
        return Err(format!("{} does not end with PAR1", path.display()).into());
    }
    let footer = u64::from(u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]])) + 8;
    let md = ParquetMetaDataReader::new().with_page_index_policy(PageIndexPolicy::Required).parse_and_finish(&f)?;
    let mut indexes = 0u64;
    for rg in md.row_groups() {
        for col in rg.columns() {
            indexes += col.column_index_length().unwrap_or(0).max(0) as u64;
            indexes += col.offset_index_length().unwrap_or(0).max(0) as u64;
        }
    }
    Ok(footer + indexes)
}

/// `one` for `write`: writes the input files of the local case `dir` as `format`, `runs` times.
/// The output directory `<dir>/<format>/` is recreated each run; each input file becomes one
/// output file of the same name. No warm-up run (CW-18). Prints the size tables for Strata.
pub fn write_format(dir: &Path, format: &str, runs: usize) -> R<ProcResult> {
    check_format(format)?;
    if runs == 0 {
        return Err("runs must be at least 1".into());
    }
    let inputs = read_inputs(dir)?;
    let rows: u64 = inputs.iter().map(|(_, _, b)| b.iter().map(|x| x.num_rows() as u64).sum::<u64>()).sum();
    let ext = if format == "pq_cetra" { "parquet" } else { "strata" };
    let out_dir = dir.join(format);
    let rss_start = start_peak_rss()?;
    let (mut runs_ms, mut cpu_ms) = (Vec::new(), Vec::new());
    let mut bytes_written = 0u64;
    let mut last_summaries: Vec<strata::FileSummary> = Vec::new();
    for _ in 0..runs {
        if out_dir.exists() {
            std::fs::remove_dir_all(&out_dir)?;
        }
        std::fs::create_dir_all(&out_dir)?;
        let cpu0 = ProcessTime::now();
        let t0 = Instant::now();
        let mut total = 0u64;
        let mut summaries = Vec::new();
        for (stem, schema, batches) in &inputs {
            let batch = concat_file(stem, schema, batches)?;
            let path = out_dir.join(format!("{stem}.{ext}"));
            if format == "pq_cetra" {
                total += write_parquet(format, &batch, &path)?;
            } else {
                let (len, summary) = write_strata(&batch, &path)?;
                total += len;
                summaries.push(summary);
            }
            std::fs::File::open(&path)?.sync_all()?;
        }
        runs_ms.push(t0.elapsed().as_secs_f64() * 1e3);
        cpu_ms.push(cpu0.elapsed().as_secs_f64() * 1e3);
        bytes_written = total;
        last_summaries = summaries;
    }
    // Read before the size tables below, which read Parquet metadata in the Strata process only.
    let vmhwm = proc_status_bytes("VmHWM")?;
    let tail_bytes = if format == "pq_cetra" {
        let mut tail = 0u64;
        for (stem, _, _) in &inputs {
            tail += parquet_tail_bytes(&out_dir.join(format!("{stem}.parquet")))?;
        }
        tail
    } else {
        payload_table(dir, &last_summaries)?;
        narrow_table(&last_summaries);
        last_summaries.iter().map(|s| s.tail_len).sum()
    };
    Ok(ProcResult {
        query: "write".to_owned(),
        format: format.to_owned(),
        runs_ms,
        cpu_ms,
        gets: 0,
        bytes: 0,
        waves: None,
        rows,
        answer: format!("{:016x}", xxh3_64(b"")),
        rss_start,
        vmhwm,
        bytes_written: Some(bytes_written),
        tail_bytes: Some(tail_bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::compute::{SortColumn, lexsort_to_indices, take_record_batch};
    use arrow::ipc::writer::FileWriter;
    use cetra_core::batch::spans_to_record_batch;
    use cetra_core::span::Span;
    use fmt_testdata::datagen::{Rng, World};

    /// Input files per test case, as M1.1 writes them (CW-19).
    const FILES: usize = 4;

    /// A clean directory for a test, under the temp directory.
    fn test_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("cw-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Writes `spans` as the case's `input/NNN.arrow` files: sorted by start time, cut into
    /// `FILES` slices, each slice one batch sorted by (trace_id, start_time).
    fn write_inputs(dir: &Path, mut spans: Vec<Span>) -> R<()> {
        spans.sort_by_key(|s| s.start_time);
        let per = spans.len().div_ceil(FILES).max(1);
        std::fs::create_dir_all(dir.join("input"))?;
        for (i, slice) in spans.chunks(per).enumerate() {
            let batches = slice.chunks(8192).map(spans_to_record_batch).collect::<Result<Vec<_>, _>>()?;
            let batch = concat_batches(&batches[0].schema(), &batches)?;
            let trace = batch.column_by_name("trace_id").ok_or("no trace_id column")?.clone();
            let start = batch.column_by_name("start_time").ok_or("no start_time column")?.clone();
            let order = lexsort_to_indices(
                &[SortColumn { values: trace, options: None }, SortColumn { values: start, options: None }],
                None,
            )?;
            let sorted = take_record_batch(&batch, &order)?;
            let mut w = FileWriter::try_new(std::fs::File::create(dir.join(format!("input/{i:03}.arrow")))?, &sorted.schema())?;
            w.write(&sorted)?;
            w.finish()?;
        }
        Ok(())
    }

    /// A summary with the given runs, CPU times, peak RSS and bytes written.
    fn summary(runs: &[f64], cpu: &[f64], vmhwm: u64, bytes_written: Option<u64>) -> Summary {
        Summary {
            runs_ms: runs.to_vec(),
            cpu_ms: cpu.to_vec(),
            gets: 0,
            bytes: 0,
            waves: None,
            rows: 0,
            answer: String::new(),
            bytes_written,
            tail_bytes: None,
            vmhwm_max: vmhwm,
            rss_delta_max: vmhwm,
        }
    }

    #[test]
    fn percentile_is_nearest_rank() {
        let v: Vec<f64> = (1..=20).map(f64::from).collect();
        assert_eq!(percentile(&v, 0.50), 10.0);
        assert_eq!(percentile(&v, 0.95), 19.0);
        assert_eq!(percentile(&v, 0.99), 20.0);
        assert_eq!(percentile(&[7.0], 0.50), 7.0);
        assert_eq!(percentile(&[7.0], 0.99), 7.0);
    }

    #[test]
    fn losses_lists_each_figure() {
        let pq = summary(&[10.0, 20.0, 30.0], &[10.0, 20.0, 30.0], 100, Some(100));
        let win = summary(&[1.0, 15.0, 25.0], &[1.0, 2.0, 3.0], 50, Some(50));
        assert!(losses("search:bug", &win, &pq).is_empty());
        // A tie on the median is a loss.
        let tie = summary(&[1.0, 20.0, 25.0], &[1.0, 2.0, 3.0], 50, Some(50));
        assert_eq!(losses("search:bug", &tie, &pq), vec!["median"]);
        let slow_tail = summary(&[1.0, 2.0, 40.0], &[1.0, 2.0, 3.0], 50, Some(50));
        assert_eq!(losses("search:bug", &slow_tail, &pq), vec!["p95"]);
        let big = summary(&[1.0, 15.0, 25.0], &[1.0, 2.0, 3.0], 200, Some(50));
        assert_eq!(losses("search:bug", &big, &pq), vec!["rss"]);
        // Write: more bytes, more CPU.
        let more_bytes = summary(&[1.0, 15.0, 25.0], &[1.0, 2.0, 3.0], 50, Some(200));
        assert_eq!(losses("write", &more_bytes, &pq), vec!["size"]);
        let more_cpu = summary(&[1.0, 15.0, 25.0], &[30.0, 30.0, 30.0], 50, Some(50));
        assert_eq!(losses("write", &more_cpu, &pq), vec!["cpu"]);
        // CPU is not compared for the reads.
        assert!(losses("search:bug", &more_cpu, &pq).is_empty());
        // A cache_mix query compares the remote gets and bytes; 0 against 0 is not a loss.
        let mut pq_mix = summary(&[10.0, 20.0, 30.0], &[10.0, 20.0, 30.0], 100, Some(100));
        pq_mix.gets = 10;
        pq_mix.bytes = 100;
        let mut st_mix = summary(&[1.0, 15.0, 25.0], &[1.0, 2.0, 3.0], 50, Some(50));
        st_mix.gets = 4;
        st_mix.bytes = 50;
        assert!(losses("cache_mix:64", &st_mix, &pq_mix).is_empty());
        st_mix.gets = 12;
        assert_eq!(losses("cache_mix:64", &st_mix, &pq_mix), vec!["gets"]);
        st_mix.gets = 4;
        st_mix.bytes = 100;
        assert_eq!(losses("cache_mix:64", &st_mix, &pq_mix), vec!["bytes"]);
        let mut pq_zero = summary(&[10.0, 20.0, 30.0], &[10.0, 20.0, 30.0], 100, Some(100));
        pq_zero.gets = 0;
        pq_zero.bytes = 0;
        let mut st_zero = summary(&[1.0, 15.0, 25.0], &[1.0, 2.0, 3.0], 50, Some(50));
        st_zero.gets = 0;
        st_zero.bytes = 0;
        assert!(losses("cache_mix:64", &st_zero, &pq_zero).is_empty());
    }

    #[test]
    fn write_memory_is_the_delta_over_the_start() {
        let pq = summary(&[10.0, 20.0, 30.0], &[10.0, 20.0, 30.0], 100, Some(100));
        // A high process peak that the runs did not add to: a win for a write query.
        let mut st = summary(&[1.0, 15.0, 25.0], &[1.0, 2.0, 3.0], 500, Some(50));
        st.rss_delta_max = 50;
        assert!(losses("write", &st, &pq).is_empty());
        // The same figures are the process peak for a read query, a loss.
        assert_eq!(losses("search:bug", &st, &pq), vec!["rss"]);
        // A write that adds more than Parquet is a loss, whatever the process peak.
        st.rss_delta_max = 200;
        assert_eq!(losses("write", &st, &pq), vec!["rss"]);
    }

    #[test]
    fn summary_takes_the_largest_delta_over_rounds() {
        let r = |rss_start: u64, vmhwm: u64| ProcResult {
            query: "write".into(),
            format: "st".into(),
            runs_ms: vec![1.0],
            cpu_ms: vec![1.0],
            gets: 0,
            bytes: 0,
            waves: None,
            rows: 1,
            answer: "0000000000000000".into(),
            rss_start,
            vmhwm,
            bytes_written: Some(1),
            tail_bytes: None,
        };
        let s = Summary::from_results(&[r(100, 300), r(150, 200), r(400, 390)]);
        assert_eq!(s.vmhwm_max, 390);
        assert_eq!(s.rss_delta_max, 200);
    }

    #[test]
    fn write_like_queries() {
        assert!(is_write_like("write"));
        assert!(is_write_like("compact"));
        assert!(is_write_like("flush:8"));
        assert!(!is_write_like("scan_payload"));
        assert!(!is_write_like("search:bug"));
        assert_eq!(flush_writers("flush:8").unwrap(), 8);
        assert!(flush_writers("flush:0").is_err());
        assert!(flush_writers("flush:65").is_err());
        // Strata's extra CPU is a loss for a write query, compact included.
        let pq = summary(&[10.0, 20.0, 30.0], &[10.0, 20.0, 30.0], 100, Some(100));
        let more_cpu = summary(&[1.0, 15.0, 25.0], &[30.0, 30.0, 30.0], 50, Some(50));
        assert_eq!(losses("compact", &more_cpu, &pq), vec!["cpu"]);
        assert!(losses("search:bug", &more_cpu, &pq).is_empty());
    }

    #[test]
    fn rounds_alternate_formats() {
        let order: Vec<&str> = (0..3).flat_map(formats_for_round).collect();
        assert_eq!(order, ["pq_cetra", "st", "st", "pq_cetra", "pq_cetra", "st"]);
    }

    #[test]
    fn result_line_round_trips() {
        let st = ProcResult {
            query: "search:bug".into(),
            format: "st".into(),
            runs_ms: vec![12.5, 11.25, 12.0],
            cpu_ms: vec![40.25, 39.5, 40.0],
            gets: 152,
            bytes: 1_048_576,
            waves: Some(3),
            rows: 417,
            answer: "9f1c2a0000000000".into(),
            rss_start: 24_117_248,
            vmhwm: 98_123_776,
            bytes_written: None,
            tail_bytes: None,
        };
        assert_eq!(ProcResult::from_line(&st.to_line()).unwrap(), st);
        let pq = ProcResult {
            query: "write".into(),
            format: "pq_cetra".into(),
            runs_ms: vec![900.5],
            cpu_ms: vec![1200.0],
            gets: 0,
            bytes: 0,
            waves: None,
            rows: 10,
            answer: "0000000000000000".into(),
            rss_start: 1,
            vmhwm: 2,
            bytes_written: Some(7),
            tail_bytes: Some(900),
        };
        assert_eq!(ProcResult::from_line(&pq.to_line()).unwrap(), pq);
    }

    #[test]
    fn write_writes_both_formats() {
        let dir = test_dir("write_writes_both_formats");
        let world = World::new(7);
        let mut rng = Rng::new(42);
        let mut spans = Vec::new();
        for _ in 0..20 {
            world.trace(&mut rng, &mut spans);
        }
        write_inputs(&dir, spans).unwrap();
        for format in ["pq_cetra", "st"] {
            let r = write_format(&dir, format, 1).unwrap();
            let files: Vec<PathBuf> = std::fs::read_dir(dir.join(format)).unwrap().map(|e| e.unwrap().path()).collect();
            assert_eq!(files.len(), 4, "{format} files");
            let total: u64 = files.iter().map(|p| std::fs::metadata(p).unwrap().len()).sum();
            assert_eq!(r.bytes_written, Some(total), "{format} bytes_written");
            let tail = r.tail_bytes.unwrap();
            assert!(tail > 0 && tail < total, "{format} tail_bytes {tail} of {total}");
        }
    }

    #[tokio::test]
    async fn cw_case_round_trips() {
        use crate::cw::cases::{gen_spans, write_case};
        // keys100: the traces, the spans and the attribute pair come back.
        let dir = test_dir("cw_case_round_trips_keys");
        let (spans, files) = gen_spans("keys100", Some(20)).unwrap();
        write_case(&dir, spans, &files).unwrap();
        let loc = location(dir.to_str().unwrap()).unwrap();
        let case = read_case(&loc).await.unwrap();
        assert_eq!(case.traces.len(), 20);
        assert_eq!(case.traces, files.traces);
        assert_eq!(case.spans.len(), 20);
        assert_eq!(case.spans, files.spans);
        assert!(case.attr.is_some());
        assert_eq!(case.attr, files.attr);
        assert!(case.needles.is_empty());
        // text_unique: the needles come back as plain text.
        let dir = test_dir("cw_case_round_trips_text");
        let (spans, files) = gen_spans("text_unique", Some(30)).unwrap();
        write_case(&dir, spans, &files).unwrap();
        let loc = location(dir.to_str().unwrap()).unwrap();
        let case = read_case(&loc).await.unwrap();
        assert!(!files.needles.is_empty());
        assert_eq!(case.needles, files.needles);
        assert_eq!(case.traces, files.traces);
        assert_eq!(case.spans, files.spans);
        assert_eq!(case.attr, None);
    }
}
