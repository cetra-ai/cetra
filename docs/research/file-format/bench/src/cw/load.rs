//! `fmtbench cw` load queries (plans/08-m5-memory-concurrency.md § M5.1): `concurrent:<n>` runs `n`
//! of M4's queries at once in one process, on the files of each table opened once for the whole
//! run, and reports each query's latency and the process's peak memory.

use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::Instant;

use cpu_time::ProcessTime;
use tokio::sync::Barrier;

use crate::counting::Counting;
use crate::cw::cache::BlockCache;
use crate::cw::harness::{ProcResult, check_format, proc_status_bytes, read_case};
use crate::cw::many::{self, Listing, Opened, ProjectCase};
use crate::cw::read::Answer;
use crate::cw::shapes::{self, ShapeCase};
#[cfg(test)]
use crate::cw::{cases, project::project_cmd};
use crate::{Location, R, location};
#[cfg(test)]
use std::fs;
#[cfg(test)]
use std::path::PathBuf;

/// The queries `concurrent:<n>` cycles through, in this order.
pub const MIX: [&str; 4] = [
    "trace_list:cost_total",
    "facets:7d",
    "dashboard:30d",
    "search_1d:bug",
];
/// The queries `cache_mix:<MiB>` runs twice, in this order.
pub const CACHE_MIX: [&str; 6] = [
    "trace_list:start_time",
    "facets:7d",
    "dashboard:30d",
    "search_1d:bug",
    "lookup_around",
    "rollup_30d_cold",
];
/// cetra's process-wide query memory (CW-10, spec 03 § 6).
pub const TOTAL_MEMORY: u64 = 8 << 30;

/// Whether `query` is a load query: it starts with `concurrent:` or `cache_mix:`, whatever follows.
pub(crate) fn is_load(query: &str) -> bool {
    query.starts_with("concurrent:") || query.starts_with("cache_mix:")
}

/// Runs a load query `runs` times after one warm-up, in this process. The number of the query
/// is checked first: `concurrent:<n>` takes `n` from 1 to 64.
pub(crate) async fn load_format(
    dir: &str,
    format: &str,
    query: &str,
    runs: usize,
) -> R<ProcResult> {
    check_format(format)?;
    if runs == 0 {
        return Err("runs must be at least 1".into());
    }
    if let Some(text) = query.strip_prefix("concurrent:") {
        let n = number_of(query, text, 1..=64)? as usize;
        return concurrent_format(dir, format, query, n, runs).await;
    }
    if let Some(text) = query.strip_prefix("cache_mix:") {
        let mib = number_of(query, text, 1..=1_048_576)?;
        return cache_mix_format(dir, format, query, mib, runs).await;
    }
    Err(format!("{query} is not a load query").into())
}

/// The number of a load query, which must be in `range`.
fn number_of(query: &str, text: &str, range: RangeInclusive<u64>) -> R<u64> {
    let n: u64 = text.parse().map_err(|_| format!("{query}: not a number"))?;
    if range.contains(&n) {
        Ok(n)
    } else {
        Err(format!(
            "{query}: {n} out of range {}..={}",
            range.start(),
            range.end()
        )
        .into())
    }
}

/// The answer of several queries run together: `xxh3_64` of their 16-byte encodings (rows `u64`,
/// hash `u64`, little-endian) in the given order; the rows are summed.
fn combine(answers: &[Answer]) -> Answer {
    let mut bytes = Vec::with_capacity(answers.len() * 16);
    let mut rows = 0u64;
    for a in answers {
        rows += a.rows;
        bytes.extend_from_slice(&a.rows.to_le_bytes());
        bytes.extend_from_slice(&a.hash.to_le_bytes());
    }
    many::answer_of(&bytes, rows)
}

/// One query of a `concurrent` run. It waits at the barrier, then runs on the opened files of its
/// table (`shapes::table_of`). Returns the answer and its wall time from the call to its return, ms.
async fn timed_query(
    barrier: Arc<Barrier>,
    query: String,
    listing: Arc<Listing>,
    spans: Arc<Opened>,
    traces: Arc<Opened>,
    case: Arc<ShapeCase>,
) -> R<(Answer, f64)> {
    let (files, opened) = if shapes::table_of(&query) == "traces" {
        (&listing.traces[..], traces.as_ref())
    } else {
        (&listing.spans[..], spans.as_ref())
    };
    barrier.wait().await;
    let t0 = Instant::now();
    let answer = shapes::run(&query, files, opened, &case).await?;
    Ok((answer, t0.elapsed().as_secs_f64() * 1e3))
}

/// One run of `concurrent:<n>`: `n` tasks start together and each runs `MIX[i % 4]`. Returns the
/// combined answer, the `n` wall times in ms, and the run's process CPU time in ms.
async fn concurrent_once(
    n: usize,
    listing: &Arc<Listing>,
    spans: &Arc<Opened>,
    traces: &Arc<Opened>,
    case: &Arc<ShapeCase>,
) -> R<(Answer, Vec<f64>, f64)> {
    let barrier = Arc::new(Barrier::new(n));
    let cpu0 = ProcessTime::now();
    let mut handles = Vec::with_capacity(n);
    for i in 0..n {
        handles.push(tokio::spawn(timed_query(
            barrier.clone(),
            MIX[i % MIX.len()].to_owned(),
            listing.clone(),
            spans.clone(),
            traces.clone(),
            case.clone(),
        )));
    }
    let mut answers = Vec::with_capacity(n);
    let mut times = Vec::with_capacity(n);
    for i in 0..handles.len() {
        let result = match (&mut handles[i]).await {
            Ok(result) => result,
            Err(e) => Err(e.into()),
        };
        match result {
            Ok((answer, ms)) => {
                answers.push(answer);
                times.push(ms);
            }
            Err(e) => {
                // Tasks not yet awaited are aborted, so they do not run on after the error.
                for handle in &handles[i + 1..] {
                    handle.abort();
                }
                return Err(e);
            }
        }
    }
    let cpu_ms = cpu0.elapsed().as_secs_f64() * 1e3;
    Ok((combine(&answers), times, cpu_ms))
}

/// `concurrent:<n>`: the format's files are listed, the case read, and both tables opened once
/// (`many::open`), all outside the timed region. One warm-up run gives the reference answer; then
/// the store's counters are reset and `runs` measured runs follow, each of which must give it.
/// `gets` and `bytes` are the first measured run's. Each entry of `cpu_ms` is the run's process
/// CPU time divided by `n`.
async fn concurrent_format(
    dir: &str,
    format: &str,
    query: &str,
    n: usize,
    runs: usize,
) -> R<ProcResult> {
    let rss_start = proc_status_bytes("VmRSS")?;
    let loc = location(dir)?;
    let listing = Arc::new(many::list(&loc, format).await?);
    let clock = many::read_project_case(&loc).await?.clock;
    let case = Arc::new(ShapeCase {
        clock,
        needles: read_case(&loc).await?.needles,
    });
    let spans = Arc::new(many::open(&loc, format, &listing.spans).await?);
    let traces = Arc::new(many::open(&loc, format, &listing.traces).await?);
    let (reference, _, _) = concurrent_once(n, &listing, &spans, &traces, &case).await?;
    loc.store.reset();
    let (mut runs_ms, mut cpu_ms) = (Vec::with_capacity(runs * n), Vec::with_capacity(runs * n));
    let (mut gets, mut bytes): (u64, u64) = (0, 0);
    for k in 0..runs {
        let (answer, times, cpu) = concurrent_once(n, &listing, &spans, &traces, &case).await?;
        if answer != reference {
            return Err(format!("answer changed between runs ({query} {format})").into());
        }
        runs_ms.extend(times);
        cpu_ms.extend(std::iter::repeat_n(cpu / n as f64, n));
        // I/O counters are read after the first measured run only (see ProcResult).
        if k == 0 {
            (gets, bytes) = loc.store.stats();
        }
    }
    let vmhwm = proc_status_bytes("VmHWM")?;
    if vmhwm > TOTAL_MEMORY {
        eprintln!("over cetra's 8 GiB query memory: {vmhwm}");
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
        vmhwm,
        bytes_written: None,
        tail_bytes: None,
    })
}

/// The result of one `cache_mix` run.
pub(crate) struct CacheMixRun {
    /// Pass 2's answer.
    pub answer: Answer,
    /// Remote (gets, bytes) of `loc.store` during each pass. Pass 1 is read by the tests only.
    #[cfg_attr(not(test), allow(dead_code))]
    pub pass1: (u64, u64),
    pub pass2: (u64, u64),
    /// Cache (hits, misses) in blocks during pass 2.
    pub hits: u64,
    pub misses: u64,
    /// Pass 2's six query times, ms.
    pub times: Vec<f64>,
    /// The process CPU time of pass 2, ms (as `concurrent`, the average per query is this over six).
    /// It includes `many::open` of both tables, which runs inside pass 2 but outside the six timed
    /// queries, so it is not the queries' CPU alone.
    pub cpu_ms: f64,
}

/// One pass of `cache_mix`: the six answers, the six wall times in ms, the remote counts of
/// `loc.store` and the cache's (hits, misses).
struct Pass {
    answers: Vec<Answer>,
    times: Vec<f64>,
    remote: (u64, u64),
    cache: (u64, u64),
}

/// One pass of `cache_mix` over the cached location: the files of both tables are opened through it
/// inside the pass, and each query of `CACHE_MIX` runs in order.
async fn cache_pass(
    loc: &Location,
    cached: &Location,
    cache: &BlockCache,
    listing: &Listing,
    case: &ShapeCase,
    project: &ProjectCase,
    format: &str,
) -> R<Pass> {
    loc.store.reset();
    cache.reset_stats();
    let spans = many::open(cached, format, &listing.spans).await?;
    let traces = many::open(cached, format, &listing.traces).await?;
    let mut answers = Vec::with_capacity(CACHE_MIX.len());
    let mut times = Vec::with_capacity(CACHE_MIX.len());
    for query in CACHE_MIX {
        let t0 = Instant::now();
        let answer = if many::is_many(query) {
            many::run(query, format, cached, listing, None, project)
                .await?
                .0
        } else {
            let (files, opened) = if shapes::table_of(query) == "traces" {
                (&listing.traces[..], &traces)
            } else {
                (&listing.spans[..], &spans)
            };
            shapes::run(query, files, opened, case).await?
        };
        times.push(t0.elapsed().as_secs_f64() * 1e3);
        answers.push(answer);
    }
    Ok(Pass {
        answers,
        times,
        remote: loc.store.stats(),
        cache: cache.stats(),
    })
}

/// One `cache_mix` run (plans/08-m5-memory-concurrency.md § M5.2): a new `BlockCache` of `budget`
/// bytes over `loc.store`, then two passes through it. Pass 2's answers must equal pass 1's.
pub(crate) async fn cache_mix_run(
    loc: &Location,
    listing: &Listing,
    case: &ShapeCase,
    project: &ProjectCase,
    format: &str,
    budget: u64,
) -> R<CacheMixRun> {
    let cache = Arc::new(BlockCache::new(loc.store.clone(), budget));
    let cached = Location {
        store: Arc::new(Counting::new(cache.clone())),
        url: loc.url.clone(),
        prefix: loc.prefix.clone(),
    };
    let first = cache_pass(loc, &cached, &cache, listing, case, project, format).await?;
    // Pass 2's CPU includes `many::open` of both tables (inside `cache_pass`, outside the timed
    // queries), as `CacheMixRun::cpu_ms` says.
    let cpu0 = ProcessTime::now();
    let second = cache_pass(loc, &cached, &cache, listing, case, project, format).await?;
    let cpu_ms = cpu0.elapsed().as_secs_f64() * 1e3;
    if let Some(i) = (0..CACHE_MIX.len()).find(|&i| first.answers[i] != second.answers[i]) {
        return Err(format!("{} answer changed between passes ({format})", CACHE_MIX[i]).into());
    }
    Ok(CacheMixRun {
        answer: combine(&second.answers),
        pass1: first.remote,
        pass2: second.remote,
        hits: second.cache.0,
        misses: second.cache.1,
        times: second.times,
        cpu_ms,
    })
}

/// `cache_mix:<MiB>`: the format's files are listed, the case read, all outside the timed region.
/// Each run is one `cache_mix_run` with a cache of `mib` MiB; the first run's answer is the
/// reference. `gets` and `bytes` are the first run's pass 2. Each run prints its pass 2's cache
/// hits and misses to stderr.
async fn cache_mix_format(
    dir: &str,
    format: &str,
    query: &str,
    mib: u64,
    runs: usize,
) -> R<ProcResult> {
    let rss_start = proc_status_bytes("VmRSS")?;
    let loc = location(dir)?;
    let listing = many::list(&loc, format).await?;
    let project = many::read_project_case(&loc).await?;
    let case = ShapeCase {
        clock: project.clock,
        needles: read_case(&loc).await?.needles,
    };
    let mut runs_ms = Vec::with_capacity(runs * CACHE_MIX.len());
    let mut cpu_ms = Vec::with_capacity(runs * CACHE_MIX.len());
    let mut reference: Option<Answer> = None;
    let mut remote = (0u64, 0u64);
    for k in 0..runs {
        let run = cache_mix_run(&loc, &listing, &case, &project, format, mib << 20).await?;
        eprintln!("cache hits {} misses {}", run.hits, run.misses);
        if let Some(first) = reference {
            if first != run.answer {
                return Err(format!("answer changed between runs ({query} {format})").into());
            }
        } else {
            reference = Some(run.answer);
        }
        if k == 0 {
            remote = run.pass2;
        }
        // As `concurrent`: one CPU entry per query, the pass's CPU divided by its queries.
        cpu_ms.extend(std::iter::repeat_n(
            run.cpu_ms / CACHE_MIX.len() as f64,
            CACHE_MIX.len(),
        ));
        runs_ms.extend(run.times);
    }
    let reference = reference.ok_or("runs must be at least 1")?;
    let vmhwm = proc_status_bytes("VmHWM")?;
    Ok(ProcResult {
        query: query.to_owned(),
        format: format.to_owned(),
        runs_ms,
        cpu_ms,
        gets: remote.0,
        bytes: remote.1,
        waves: None,
        rows: reference.rows,
        answer: format!("{:016x}", reference.hash),
        rss_start,
        vmhwm,
        bytes_written: None,
        tail_bytes: None,
    })
}

/// A project written by `cw project` for the tests: 9 days of 400 traces a day with payloads, in
/// `cw-test-load-{test}-{pid}` under the temp directory, removed on drop. `dir` is the project.
#[cfg(test)]
pub(crate) struct TestProject {
    pub dir: PathBuf,
}

#[cfg(test)]
impl TestProject {
    pub(crate) async fn new(test: &str) -> Self {
        let base = std::env::temp_dir().join(format!("cw-test-load-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let case = base.join("case");
        fs::create_dir_all(&case).unwrap();
        let (spans, files) = cases::gen_spans("text_unique", Some(200)).unwrap();
        cases::write_case(&case, spans, &files).unwrap();
        let dir = base.join("p");
        let args = vec![
            dir.display().to_string(),
            "--days".into(),
            "9".into(),
            "--traces-per-day".into(),
            "400".into(),
            "--payloads".into(),
            case.display().to_string(),
        ];
        project_cmd(&args).await.unwrap();
        Self { dir }
    }
}

#[cfg(test)]
impl Drop for TestProject {
    fn drop(&mut self) {
        // The project's parent holds the case as well.
        let _ = fs::remove_dir_all(self.dir.parent().unwrap_or(&self.dir));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_answers_agree() {
        let p = TestProject::new("answers").await;
        let dir = p.dir.display().to_string();
        for n in [4usize, 8] {
            let query = format!("concurrent:{n}");
            let pq = load_format(&dir, "pq_cetra", &query, 1).await.unwrap();
            let st = load_format(&dir, "st", &query, 1).await.unwrap();
            assert_eq!(pq.answer, st.answer, "{query} differs between the formats");
            assert_eq!(
                pq.rows, st.rows,
                "{query} row counts differ between the formats"
            );
            assert!(pq.rows > 0, "{query} found no rows");
            if n == 4 {
                // The four queries one by one, each on its own table's files.
                let loc = crate::location(&dir).unwrap();
                let listing = many::list(&loc, "pq_cetra").await.unwrap();
                let clock = many::read_project_case(&loc).await.unwrap().clock;
                let needles = read_case(&loc).await.unwrap().needles;
                let case = ShapeCase { clock, needles };
                let mut answers = Vec::new();
                for q in &MIX {
                    let files = if shapes::table_of(q) == "traces" {
                        &listing.traces
                    } else {
                        &listing.spans
                    };
                    let opened = many::open(&loc, "pq_cetra", files).await.unwrap();
                    answers.push(shapes::run(q, files, &opened, &case).await.unwrap());
                }
                let expect = combine(&answers[..4]);
                assert_eq!(
                    pq.answer,
                    format!("{:016x}", expect.hash),
                    "{query} differs from the serial answers"
                );
                assert_eq!(pq.rows, expect.rows);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_reports_one_latency_per_query() {
        let p = TestProject::new("latency").await;
        let dir = p.dir.display().to_string();
        let r = load_format(&dir, "pq_cetra", "concurrent:8", 2)
            .await
            .unwrap();
        assert_eq!(r.runs_ms.len(), 16);
        assert_eq!(r.cpu_ms.len(), 16);
    }

    #[test]
    fn load_queries_check_their_number() {
        assert!(is_load("concurrent:8"));
        assert!(is_load("concurrent:0"));
        assert!(is_load("cache_mix:x"));
        assert!(!is_load("trace_list:cost_total"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cache_mix_agrees_across_formats() {
        let p = TestProject::new("cache").await;
        let loc = location(&p.dir.display().to_string()).unwrap();
        let project = many::read_project_case(&loc).await.unwrap();
        let case = ShapeCase {
            clock: project.clock,
            needles: read_case(&loc).await.unwrap().needles,
        };
        let mut answers = Vec::new();
        for format in ["pq_cetra", "st"] {
            let listing = many::list(&loc, format).await.unwrap();
            let run = cache_mix_run(&loc, &listing, &case, &project, format, 64 << 20)
                .await
                .unwrap();
            assert!(run.pass1.0 >= 1, "{format}: pass 1 made no remote request");
            assert!(run.hits > 0, "{format}: pass 2 hit the cache nowhere");
            assert!(
                run.pass2.0 < run.pass1.0,
                "{format}: pass 2 made no fewer requests than pass 1"
            );
            answers.push(run.answer);
        }
        assert_eq!(
            answers[0], answers[1],
            "cache_mix differs between the formats"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cache_mix_small_budget_agrees() {
        let p = TestProject::new("cache-small").await;
        let loc = location(&p.dir.display().to_string()).unwrap();
        let project = many::read_project_case(&loc).await.unwrap();
        let case = ShapeCase {
            clock: project.clock,
            needles: read_case(&loc).await.unwrap().needles,
        };
        let mut answers = Vec::new();
        for format in ["pq_cetra", "st"] {
            let listing = many::list(&loc, format).await.unwrap();
            let big = cache_mix_run(&loc, &listing, &case, &project, format, 64 << 20)
                .await
                .unwrap();
            // A 1 MiB budget holds one block, so the passes evict through the real readers.
            let small = cache_mix_run(&loc, &listing, &case, &project, format, 1 << 20)
                .await
                .unwrap();
            assert_eq!(
                small.answer, big.answer,
                "{format}: a 1 MiB budget gives another answer than 64 MiB"
            );
            assert!(
                small.misses > big.misses,
                "{format}: the 1 MiB budget evicted nothing"
            );
            answers.push(small.answer);
        }
        assert_eq!(
            answers[0], answers[1],
            "cache_mix differs between the formats at 1 MiB"
        );
    }

    #[tokio::test]
    async fn load_format_rejects_bad_numbers() {
        for (query, want) in [
            ("concurrent:0", "concurrent:0: 0 out of range 1..=64"),
            ("concurrent:65", "concurrent:65: 65 out of range 1..=64"),
            ("concurrent:x", "concurrent:x: not a number"),
        ] {
            let err = load_format("/nonexistent", "pq_cetra", query, 1)
                .await
                .unwrap_err();
            assert_eq!(err.to_string(), want);
        }
    }
}
