//! Storage soak (track R M7, D-R-11, `tests/it/soak.rs`, ignored).
//!
//! Drives `Storage` at a steady rate for hours: ingest, scores, deletions,
//! retention and queries run at the same time, and background compaction stays
//! on. Every minute one JSON line of resource and lag samples goes to
//! `<out>/samples.jsonl`. Run by `just soak`, configured by `CETRA_SOAK_*`.
//!
//! R7.5 writes the samples and the failures. The pass/fail rules, the
//! sampled-trace oracle and the settle step come in R7.6.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::print_stdout
)]

mod config;
mod driver;
mod failures;
mod redact;
mod registry;
mod rules;
mod sample;

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::common::corpus::{CORPUS_FILE, CORPUS_SHA256, Corpus};
use crate::common::machine::machine;
use crate::common::payload::Payloads;
use crate::common::payload::TraceSummary;
use crate::common::rng::{Rng, mix};
use crate::common::s3::S3Env;
use cetra_core::ids::TraceId;
use cetra_storage::Storage;
use config::{QueryKind, SoakConfig, parse_mix, prepare_data_dir};
use driver::{Shared, is_sampled};
use failures::FailureKind;
use redact::Redactor;
use registry::check_sample;
use rules::{EndChecks, Limits, Verdict};
use sample::{Sample, Sampler};
use tokio::sync::watch;

const HOUR_NS: i64 = 3_600 * 1_000_000_000;

/// The run description written to `run.json` before the soak starts.
#[derive(serde::Serialize)]
struct RunInfo<'a> {
    label: &'a str,
    started_unix: i64,
    machine: crate::common::machine::Machine,
    config: &'a SoakConfig,
    store: serde_json::Value,
    corpus_file: &'static str,
    corpus_sha256: &'static str,
    storage_config: String,
}

/// `failures.json`: what went wrong, by kind.
#[derive(serde::Serialize)]
struct FailuresOut {
    counts: BTreeMap<String, u64>,
    examples: BTreeMap<String, Vec<String>>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "run by hand: CETRA_SOAK_DIR and CETRA_SOAK_OUT, see `just soak`"]
async fn soak() {
    let config = SoakConfig::from_env().expect("the soak configuration");
    // The corpus is read before any directory is touched or the clock starts.
    let payloads = Arc::new(Payloads::new(Corpus::from_env()));
    let out = config.out.clone();
    if out.exists() {
        assert!(
            out.read_dir().unwrap().next().is_none(),
            "CETRA_SOAK_OUT must be missing or empty"
        );
    } else {
        std::fs::create_dir_all(&out).unwrap();
    }
    prepare_data_dir(&config.dir).expect("the soak data directory");

    let s3 = S3Env::from_env();
    let prefix = match &s3 {
        Some(_) => format!("soak-{}", uuid::Uuid::now_v7()),
        None => "local".to_owned(),
    };
    let events_path = out.join("events.jsonl");
    let events = std::fs::File::create(&events_path).expect("the events file");
    let _ = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::sync::Mutex::new(events))
        .try_init();

    let redactor = Redactor::for_run(s3.as_ref());
    let storage_config = config::storage_config(&config.dir, s3.as_ref(), &prefix);
    let limits = Limits {
        wal_segment_bytes: storage_config.wal.segment_bytes.0 as f64,
        flush_max_bytes: storage_config.flush.max_bytes.0 as f64,
        l0_files: storage_config.compaction.l0_files as f64,
    };
    let flush_wait = storage_config.flush.max_age + Duration::from_secs(10);
    let start = Instant::now();
    let run = RunInfo {
        label: &config.label,
        started_unix: sample::now_ns() / 1_000_000_000,
        machine: machine(&config.dir),
        config: &config,
        store: match &s3 {
            Some(target) => serde_json::json!({
                "endpoint": target.endpoint,
                "region": target.region,
                "bucket": target.bucket,
                "prefix": prefix,
            }),
            None => serde_json::json!("local"),
        },
        corpus_file: CORPUS_FILE,
        corpus_sha256: CORPUS_SHA256,
        storage_config: format!("{storage_config:#?}"),
    };
    let run_text = redactor.apply(&serde_json::to_string_pretty(&run).unwrap());
    std::fs::write(out.join("run.json"), run_text).unwrap();

    let storage = Arc::new(
        Storage::open(storage_config.clone())
            .await
            .expect("the storage opens"),
    );
    let shared = Arc::new(Shared::new(storage, Arc::clone(&payloads), config.clone()));

    // Project i gets the share 2^(n-1-i) / (2^n - 1) of the rate.
    let (stop_tx, stop_rx) = watch::channel(false);
    let projects = config.projects;
    let mut tasks = Vec::new();
    for index in 0..projects {
        let share = (1_u64 << (projects - 1 - index)) as f64 / ((1_u64 << projects) - 1) as f64;
        tasks.push(tokio::spawn(driver::ingest(
            Arc::clone(&shared),
            index,
            config.rate as f64 * share,
            stop_rx.clone(),
        )));
    }
    tasks.push(tokio::spawn(driver::scores(
        Arc::clone(&shared),
        stop_rx.clone(),
    )));
    tasks.push(tokio::spawn(driver::deletions(
        Arc::clone(&shared),
        stop_rx.clone(),
    )));
    tasks.push(tokio::spawn(driver::retention(
        Arc::clone(&shared),
        stop_rx.clone(),
    )));
    tasks.push(tokio::spawn(driver::queries(
        Arc::clone(&shared),
        stop_rx.clone(),
    )));
    tasks.push(tokio::spawn(driver::oracle(
        Arc::clone(&shared),
        stop_rx.clone(),
    )));

    let mut sampler = Sampler::open(
        &config.dir,
        s3.clone().map(|target| (target, prefix.clone())),
        start,
    )
    .await
    .expect("the catalog opens read-only");
    let mut samples_file =
        std::fs::File::create(out.join("samples.jsonl")).expect("the samples file");
    let mut samples: Vec<Sample> = Vec::new();
    let end = start + Duration::from_secs_f64(config.hours * 3600.0);
    while Instant::now() < end {
        tokio::time::sleep(Duration::from_secs(config.sample_secs)).await;
        check_tasks(&mut tasks).await;
        if let Some(sample) = take_sample(&mut sampler, &shared).await {
            writeln!(samples_file, "{}", serde_json::to_string(&sample).unwrap()).unwrap();
            samples_file.flush().unwrap();
            samples.push(sample);
        }
    }

    // 1. Stop: the ingest tasks append their pending late groups first.
    stop_tx.send(true).expect("the tasks are listening");
    for task in tasks {
        task.await.unwrap();
    }
    if let Some(last) = take_sample(&mut sampler, &shared).await {
        writeln!(samples_file, "{}", serde_json::to_string(&last).unwrap()).unwrap();
        samples.push(last);
    }
    drop(sampler);

    // 2. Restart: shut down, drop, and open again, timed. WAL replay runs in the open.
    let Shared {
        storage,
        payloads,
        config: shared_config,
        failures,
        counters,
        candidates,
        gate,
        queries,
        deletions,
        retention,
        registry,
    } = Arc::try_unwrap(shared)
        .ok()
        .expect("the tasks have stopped");
    let shutdown_started = Instant::now();
    storage.shutdown().await.expect("the storage shuts down");
    let shutdown_s = shutdown_started.elapsed().as_secs_f64();
    drop(storage);
    let reopen_started = Instant::now();
    let reopened = Arc::new(
        Storage::open(storage_config.clone())
            .await
            .expect("the storage reopens"),
    );
    let reopen_s = reopen_started.elapsed().as_secs_f64();
    let shared = Shared {
        storage: reopened,
        payloads,
        config: shared_config,
        failures,
        counters,
        candidates,
        gate,
        queries,
        deletions,
        retention,
        registry,
    };
    let mut sampler = Sampler::open(
        &config.dir,
        s3.clone().map(|target| (target, prefix.clone())),
        start,
    )
    .await
    .expect("the catalog opens read-only");

    // 3. Check at full size.
    check_sample(
        &shared.storage,
        &shared.registry,
        &shared.gate,
        &shared.failures,
        2_000,
        500,
        0x51,
    )
    .await;

    // 4. Settle: flush, compact and apply deletions at now + 3 days, then GC; at most three rounds.
    let mut settled: Vec<String> = Vec::new();
    let mut compaction_passes = 0_u64;
    for _ in 0..3 {
        let round = settle_round(&shared, flush_wait, &mut settled, &mut compaction_passes).await;
        if !round {
            break;
        }
    }

    // 5. Settled state: one more sample, with the conditions of R7.6.
    if let Some(settled_sample) = take_sample(&mut sampler, &shared).await {
        if settled_sample.staged_objects != 0 {
            settled.push(format!("{} staged objects", settled_sample.staged_objects));
        }
        if settled_sample.overdue_partitions != 0 {
            settled.push(format!(
                "{} overdue partitions",
                settled_sample.overdue_partitions
            ));
        }
        if settled_sample.files.get("spans").map_or(0, |f| f.l0) != 0 {
            settled.push("a spans partition still has L0 files".to_owned());
        }
        if settled_sample.memtable_bytes != 0 {
            settled.push(format!(
                "{} memtable bytes left",
                settled_sample.memtable_bytes
            ));
        }
        if sampler.suspect_count() != 0 {
            settled.push(format!("{} orphan suspects", sampler.suspect_count()));
        }
        writeln!(
            samples_file,
            "{}",
            serde_json::to_string(&settled_sample).unwrap()
        )
        .unwrap();
        samples.push(settled_sample);
    } else {
        settled.push("the settled sample could not be taken".to_owned());
    }

    // 6. Check again.
    check_sample(
        &shared.storage,
        &shared.registry,
        &shared.gate,
        &shared.failures,
        2_000,
        500,
        0x52,
    )
    .await;

    // The events log: ERROR lines and WARN messages (counts only).
    let events_raw = std::fs::read_to_string(&events_path).unwrap();
    let mut error_lines = 0_u64;
    let mut warn_messages: BTreeMap<String, u64> = BTreeMap::new();
    for line in events_raw.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match value["level"].as_str() {
            Some("ERROR") => error_lines += 1,
            Some("WARN") => {
                let message = value["fields"]["message"].as_str().unwrap_or("").to_owned();
                *warn_messages.entry(message).or_default() += 1;
            }
            _ => {}
        }
    }

    let pending_deletions = shared
        .deletions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pending
        .len() as u64;
    if let Err(error) = shared.storage.shutdown().await {
        settled.push(format!("shutdown failed: {error}"));
    }
    let end_checks = EndChecks {
        settled,
        pending_deletions,
        error_lines,
    };
    let counts = shared.failures.counts();
    let verdicts = rules::evaluate(&samples, &counts, &config, &limits, &end_checks);
    let passed = verdicts
        .iter()
        .all(|(_, verdict)| !matches!(verdict, Verdict::Fail(_)));

    // 7. Clean up: drop the S3 prefix of a passing run.
    if passed
        && !config.keep
        && let Some(target) = &s3
        && let Err(error) = target.delete_prefix(&prefix).await
    {
        println!("could not delete the S3 prefix: {error}");
    }

    // 8. Write and fail.
    let failures_out = FailuresOut {
        counts: counts
            .iter()
            .map(|(kind, count)| (kind.name(), *count))
            .collect(),
        examples: shared
            .failures
            .examples()
            .into_iter()
            .map(|(kind, examples)| (kind.name(), examples))
            .collect(),
    };
    std::fs::write(
        out.join("failures.json"),
        redactor.apply(&serde_json::to_string_pretty(&failures_out).unwrap()),
    )
    .unwrap();
    let scrubbed: String = events_raw
        .lines()
        .map(|line| format!("{}\n", redactor.apply(line)))
        .collect();
    std::fs::write(out.join("events.scrubbed.jsonl"), scrubbed).unwrap();

    let summary = summary_json(
        &config,
        &samples,
        &shared,
        &verdicts,
        passed,
        shutdown_s,
        reopen_s,
        compaction_passes,
        &warn_messages,
        start.elapsed().as_secs_f64(),
    );
    std::fs::write(
        out.join("summary.json"),
        redactor.apply(&serde_json::to_string_pretty(&summary).unwrap()),
    )
    .unwrap();
    println!(
        "soak {}: {} rules, passed {passed}, {:.0} s",
        config.label,
        verdicts.len(),
        start.elapsed().as_secs_f64()
    );
    assert!(passed, "soak failed rules: {:?}", failing_rules(&verdicts));
}

/// The rules that failed, by name.
fn failing_rules(verdicts: &[(String, Verdict)]) -> Vec<&str> {
    verdicts
        .iter()
        .filter(|(_, verdict)| matches!(verdict, Verdict::Fail(_)))
        .map(|(name, _)| name.as_str())
        .collect()
}

/// One settle round: flush, explicit compaction and deletion passes, GC. Returns
/// whether the round changed anything. A failed call is recorded in `settled`.
async fn settle_round(
    shared: &Shared,
    flush_wait: Duration,
    settled: &mut Vec<String>,
    compaction_passes: &mut u64,
) -> bool {
    let mut changed = false;
    // No `flush_now` in this storage: the timed flush writes the replayed rows.
    tokio::time::sleep(flush_wait).await;
    let now = sample::now_ns() + 3 * 24 * HOUR_NS;
    let mut last_report = None;
    for _ in 0..50 {
        let report = match shared.storage.compact(now).await {
            Ok(report) => report,
            Err(error) => {
                settled.push(format!("compaction failed: {error}"));
                return changed;
            }
        };
        *compaction_passes += 1;
        changed |= report.jobs > 0;
        last_report = Some((report.jobs, report.failed));
        if report.jobs == 0 && report.failed == 0 {
            break;
        }
    }
    if let Some((jobs, failed)) = last_report
        && (jobs != 0 || failed != 0)
    {
        settled.push(format!(
            "compaction still busy after 50 passes: {jobs} jobs, {failed} failed"
        ));
    }
    for _ in 0..20 {
        let report = match shared.storage.apply_deletions(now).await {
            Ok(report) => report,
            Err(error) => {
                settled.push(format!("deletion pass failed: {error}"));
                return changed;
            }
        };
        changed |= report.applied > 0;
        driver::poll_status(shared).await;
        let pending = shared
            .deletions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .len();
        if pending == 0 {
            break;
        }
    }
    if let Err(error) = shared.storage.collect_obsolete(Duration::ZERO).await {
        settled.push(format!("GC of obsolete files failed: {error}"));
    }
    if let Err(error) = shared.storage.collect_staged(Duration::ZERO).await {
        settled.push(format!("GC of staged objects failed: {error}"));
    }
    changed
}

/// One sample, retried once. A second failure is recorded as a `Sample` failure
/// and the run goes on without that sample.
async fn take_sample(sampler: &mut Sampler, shared: &Shared) -> Option<Sample> {
    for attempt in 0..2 {
        match sampler.sample(shared).await {
            Ok(sample) => return Some(sample),
            Err(error) if attempt == 1 => {
                shared.failures.record(FailureKind::Sample, error);
            }
            Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
    None
}

/// Stops the run with the task's panic when a driver task ended before the stop.
async fn check_tasks(tasks: &mut Vec<tokio::task::JoinHandle<()>>) {
    if let Some(index) = tasks.iter().position(|task| task.is_finished()) {
        let dead = tasks.swap_remove(index);
        for task in tasks.iter() {
            task.abort();
        }
        match dead.await {
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            other => panic!("a soak task ended before the stop: {other:?}"),
        }
    }
}

/// One rule as summary.json writes it.
#[derive(serde::Serialize)]
struct RuleOut {
    name: String,
    verdict: &'static str,
    detail: String,
}

/// summary.json: the verdicts and the stats the write-up quotes.
#[allow(clippy::too_many_arguments)]
fn summary_json(
    config: &SoakConfig,
    samples: &[Sample],
    shared: &Shared,
    verdicts: &[(String, Verdict)],
    passed: bool,
    shutdown_s: f64,
    reopen_s: f64,
    compaction_passes: u64,
    warn_messages: &BTreeMap<String, u64>,
    elapsed_s: f64,
) -> serde_json::Value {
    let rules: Vec<RuleOut> = verdicts
        .iter()
        .map(|(name, verdict)| {
            let (kind, detail) = match verdict {
                Verdict::Pass => ("pass", String::new()),
                Verdict::Fail(detail) => ("fail", detail.clone()),
                Verdict::Skipped(detail) => ("skipped", detail.clone()),
                Verdict::Info(detail) => ("info", detail.clone()),
            };
            RuleOut {
                name: name.clone(),
                verdict: kind,
                detail,
            }
        })
        .collect();
    let warmup = config.warmup_hours * 3600.0;
    let post: Vec<&Sample> = samples.iter().filter(|s| s.elapsed_s >= warmup).collect();
    let over = |f: fn(&Sample) -> f64| -> serde_json::Value {
        let mut values: Vec<f64> = post.iter().map(|s| f(s)).collect();
        summary_stats(&mut values)
    };
    let mut lags = shared
        .deletions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .lags_s
        .clone();
    let (registered, pending) = {
        let stats = shared
            .deletions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (stats.registered, stats.pending.len())
    };
    let run_end = config.hours * 3600.0 + config.sample_secs as f64;
    let appended = samples
        .iter()
        .rev()
        .find(|s| s.elapsed_s <= run_end)
        .map_or(0, |s| s.appended_spans_total);
    let generated = shared
        .counters
        .generated_spans
        .load(std::sync::atomic::Ordering::Relaxed)
        .max(1);
    let payload = shared
        .counters
        .payload_bytes
        .load(std::sync::atomic::Ordering::Relaxed);
    let mut queries = serde_json::Map::new();
    for kind in QueryKind::ALL {
        let mut p50 = Vec::new();
        let mut p95 = Vec::new();
        let mut max = 0.0_f64;
        let mut count = 0_u64;
        for s in &post {
            if let Some(q) = s.queries.get(kind.name()) {
                count += q.n;
                p50.push(q.p50_ms);
                p95.push(q.p95_ms);
                max = max.max(q.max_ms);
            }
        }
        queries.insert(
            kind.name().to_owned(),
            serde_json::json!({
                "n": count,
                "p50_ms_median_of_samples": summary_stats(&mut p50)["median"],
                "p95_ms_p95_of_samples": summary_stats(&mut p95)["p95"],
                "max_ms": max,
            }),
        );
    }
    let mut top: Vec<(&String, &u64)> = warn_messages.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    let warn_top: Vec<serde_json::Value> = top
        .into_iter()
        .take(20)
        .map(|(message, count)| serde_json::json!({ "message": message, "count": count }))
        .collect();
    let retention: Vec<(u64, u64)> = shared
        .retention
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    serde_json::json!({
        "label": config.label,
        "passed": passed,
        "rules": rules,
        "stats": {
            "elapsed_s": elapsed_s,
            "spans_per_s": appended as f64 / elapsed_s.max(1.0),
            "payload_bytes_per_span": shared.counters.payload_bytes.load(std::sync::atomic::Ordering::Relaxed) as f64 / generated as f64,
            "payload_total": payload,
            "scores_written": shared.counters.appended_scores.load(std::sync::atomic::Ordering::Relaxed),
            "deletions": {
                "registered": registered,
                "applied": registered as usize - pending,
                "pending_at_end": pending,
                "lag_s": {
                    "p50": percentile_of(&mut lags, 0.50),
                    "p95": percentile_of(&mut lags, 0.95),
                    "max": lags.last().copied().unwrap_or(0.0),
                },
            },
            "retention_reports": retention,
            "shutdown_s": shutdown_s,
            "reopen_s": reopen_s,
            "compaction_passes": compaction_passes,
            "queries": queries,
            "rss_bytes": over(|s| s.rss_bytes as f64),
            "fds": over(|s| s.fds as f64),
            "wal_bytes": over(|s| s.wal_bytes as f64),
            "memtable_bytes": over(|s| s.memtable_bytes as f64),
            "l0_files": over(|s| s.files.get("spans").map_or(0.0, |f| f.l0 as f64)),
            "l0_oldest_age_s": over(|s| s.l0_oldest_age_s),
            "objects_bytes": over(|s| s.objects_bytes as f64),
            "warn_messages_top20": warn_top,
        },
    })
}

/// min, median, 95th percentile and max of `values`.
fn summary_stats(values: &mut [f64]) -> serde_json::Value {
    if values.is_empty() {
        return serde_json::json!({ "min": 0.0, "median": 0.0, "p95": 0.0, "max": 0.0 });
    }
    values.sort_by(f64::total_cmp);
    serde_json::json!({
        "min": values[0],
        "median": percentile_of(values, 0.50),
        "p95": percentile_of(values, 0.95),
        "max": values[values.len() - 1],
    })
}

/// The nearest-rank percentile of sorted `values`; 0 when empty.
fn percentile_of(values: &mut [f64], fraction: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    let index = ((values.len() as f64 * fraction).ceil() as usize).clamp(1, values.len()) - 1;
    values[index]
}

// ---------------------------------------------------------------- unit tests

#[test]
fn parse_mix_reads_weights() {
    let default =
        parse_mix("list=30,filter=15,detail=25,sessions=10,metrics=10,scores=10").unwrap();
    assert_eq!(default.len(), 6);
    assert_eq!(default[0], (QueryKind::List, 30));
    assert!(parse_mix("list=0").is_err(), "every weight is at least 1");
    assert!(parse_mix("nope=3").is_err(), "unknown kinds are refused");
}

#[test]
fn config_defaults() {
    let lookup = |name: &str| match name {
        "CETRA_SOAK_DIR" => Some("/home/tester/soak".to_owned()),
        "CETRA_SOAK_OUT" => Some("/home/tester/soak-out".to_owned()),
        _ => None,
    };
    let config = SoakConfig::from_lookup(lookup).unwrap();
    assert_eq!(config.label, "local");
    assert_eq!(config.hours, 24.0);
    assert_eq!(config.rate, 2_000);
    assert_eq!(config.projects, 4);
    assert_eq!(config.qps, 1.0);
    assert_eq!(config.retention_days, 0);
    assert_eq!(config.sample_secs, 60);
    assert_eq!(config.warmup_hours, 2.4);
    assert_eq!(config.max_rss_gib, 8.0);
    assert_eq!(config.max_l0_age_min, 30);
    assert!(!config.keep);
    assert_eq!(config.mix.len(), 6);

    let tmp = |name: &str| match name {
        "CETRA_SOAK_DIR" => Some("/tmp/x".to_owned()),
        "CETRA_SOAK_OUT" => Some("/home/tester/soak-out".to_owned()),
        _ => None,
    };
    assert!(
        SoakConfig::from_lookup(tmp).is_err(),
        "a /tmp directory is refused"
    );
}

#[test]
fn prepare_refuses_unmarked_dirs() {
    let root = tempfile::tempdir().unwrap();

    let unmarked = root.path().join("unmarked");
    std::fs::create_dir(&unmarked).unwrap();
    std::fs::write(unmarked.join("data.bin"), b"keep me").unwrap();
    assert!(prepare_data_dir(&unmarked).is_err());
    assert!(unmarked.join("data.bin").exists());

    let marked = root.path().join("marked");
    prepare_data_dir(&marked).unwrap();
    std::fs::write(marked.join("junk.bin"), b"x").unwrap();
    prepare_data_dir(&marked).unwrap();
    assert!(
        !marked.join("junk.bin").exists(),
        "a marked directory is emptied"
    );
    assert!(marked.join(config::MARKER).exists(), "and keeps its marker");

    let missing = root.path().join("missing");
    prepare_data_dir(&missing).unwrap();
    assert!(missing.join(config::MARKER).exists());
}

#[test]
fn proc_status_parses() {
    let excerpt = "Name:\tcetra\nVmPeak:\t 900000 kB\nVmHWM:\t  200000 kB\nVmRSS:\t  123456 kB\n\
                   RssAnon:\t  100000 kB\nRssFile:\t   23000 kB\nRssShmem:\t     456 kB\nThreads:\t42\n";
    assert_eq!(
        sample::parse_status(excerpt),
        sample::ProcStatus {
            rss_bytes: 123_456 * 1024,
            hwm_bytes: 200_000 * 1024,
            rss_anon_bytes: 100_000 * 1024,
            rss_file_bytes: 23_000 * 1024,
            threads: 42,
        }
    );
}

#[test]
fn orphans_need_two_samples_and_age() {
    let now = 1_000_000;
    let known = HashSet::new();
    let old = vec![("tables/spans/a.parquet".to_owned(), now - 660)];
    // Unknown in one sample only: not confirmed.
    let (confirmed, suspects) = sample::orphans(&old, &known, &HashSet::new(), now);
    assert_eq!(confirmed, 0);
    assert!(suspects.contains("tables/spans/a.parquet"));

    // Unknown in two samples but 5 minutes old: not confirmed.
    let young = vec![("tables/spans/b.parquet".to_owned(), now - 300)];
    let (_, previous) = sample::orphans(&young, &known, &HashSet::new(), now);
    let (confirmed, _) = sample::orphans(&young, &known, &previous, now);
    assert_eq!(confirmed, 0);

    // Unknown in two samples and 11 minutes old: confirmed.
    let (_, previous) = sample::orphans(&old, &known, &HashSet::new(), now);
    let (confirmed, _) = sample::orphans(&old, &known, &previous, now);
    assert_eq!(confirmed, 1);

    // A temporary write is never a suspect.
    let tmp = vec![(
        "tables/spans/.x.parquet.tmp-0190abcd".to_owned(),
        now - 3600,
    )];
    let (confirmed, suspects) = sample::orphans(&tmp, &known, &suspects, now);
    assert_eq!(confirmed, 0);
    assert!(suspects.is_empty());
}

#[test]
fn sampling_is_independent_of_the_app() {
    let mut seen = [false; 16];
    for seed in 0..2_000_000_u64 {
        if is_sampled(seed) {
            seen[(mix(seed) % 16) as usize] = true;
        }
    }
    assert!(
        seen.iter().all(|&s| s),
        "every app must have a sampled seed: {seen:?}"
    );
}

#[test]
fn redactor_replaces_secrets() {
    let redactor = Redactor::new(vec![
        ("acme-prod".to_owned(), "<bucket>"),
        ("fsn1.your-objectstorage.com".to_owned(), "<endpoint>"),
        ("/home/tester".to_owned(), "~"),
        ("workstation".to_owned(), "<host>"),
    ]);
    let text = r#"{"bucket":"acme-prod","endpoint":"fsn1.your-objectstorage.com","home":"/home/tester/soak","host":"workstation","local":"http://127.0.0.1:7070"}"#;
    let out = redactor.apply(text);
    assert!(
        out.contains("<bucket>") && out.contains("<endpoint>"),
        "{out}"
    );
    assert!(out.contains("~/soak") && out.contains("<host>"), "{out}");
    assert!(out.contains("127.0.0.1:7070"), "loopback stays: {out}");
    assert!(
        !out.contains("acme-prod") && !out.contains("fsn1.your") && !out.contains("workstation"),
        "{out}"
    );
    // A secret inside a longer word is left alone.
    assert_eq!(
        redactor.apply("acme-prod-2 and acme-prod"),
        "acme-prod-2 and <bucket>"
    );
}

#[test]
#[should_panic(expected = "shorter than 4 bytes")]
fn redactor_refuses_short_secrets() {
    let _ = Redactor::new(vec![("ws".to_owned(), "<host>")]);
}

#[test]
fn unacknowledged_trace_is_not_deleted() {
    // Registered before its groups are acknowledged: a deletion must skip it, never mark it deleted.
    let mut registry = registry::Registry::default();
    let id = TraceId::from_bytes([9_u8; 16]).unwrap();
    registry.insert(
        0,
        TraceSummary {
            trace_id: id,
            name: "chat".to_owned(),
            start_time: 1_000,
            end_time: 2_000,
            span_count: 8,
            error_count: 0,
            llm_count: 1,
            total_tokens: Some(10),
            cost_total: Some(0.1),
            session_id: None,
            user_id: Some("u".to_owned()),
        },
        false,
    );
    registry.apply_deletion(0, &[id], None, i64::MAX);
    assert_eq!(registry.state(0, id), Some(registry::Expect::Skip));
}

#[test]
fn cpu_fields_follow_the_command_name() {
    let stat = "123 (cetra (soak)) S 1 1 1 0 -1 0 0 0 0 0 250 50 0 0 20 0 42";
    assert!((sample::parse_cpu(stat) - 3.0).abs() < 1e-9);
}

#[test]
fn flat_noise_passes() {
    let mut rng = Rng::new(7);
    let series: Vec<(f64, f64)> = (0..1000)
        .map(|i| (f64::from(i), 100.0 + (rng.unit() - 0.5) * 10.0))
        .collect();
    assert!(matches!(rules::grows(&series, 0.1, 0.0), Verdict::Pass));
}

#[test]
fn steady_growth_fails() {
    let series: Vec<(f64, f64)> = (0..1000)
        .map(|i| (f64::from(i), 100.0 + 0.05 * f64::from(i)))
        .collect();
    assert!(matches!(rules::grows(&series, 0.1, 0.0), Verdict::Fail(_)));
}

#[test]
fn daily_sawtooth_passes() {
    // 72 h at one sample per minute, a sawtooth from 50 to 100 with a 24 h period.
    let series: Vec<(f64, f64)> = (0..72 * 60)
        .map(|minute| {
            let phase = f64::from(minute % 1440) / 1440.0;
            (f64::from(minute) * 60.0, 50.0 + 50.0 * phase)
        })
        .collect();
    assert!(matches!(rules::grows(&series, 0.1, 0.0), Verdict::Pass));
}

#[test]
fn short_series_is_skipped() {
    let series: Vec<(f64, f64)> = (0..20).map(|i| (f64::from(i), f64::from(i))).collect();
    assert!(matches!(
        rules::grows(&series, 0.1, 0.0),
        Verdict::Skipped(_)
    ));
}

/// A config for the rule tests: the given hours, everything else default.
fn rule_config(hours: &str) -> SoakConfig {
    let hours = hours.to_owned();
    SoakConfig::from_lookup(move |name| match name {
        "CETRA_SOAK_DIR" => Some("/home/tester/soak".to_owned()),
        "CETRA_SOAK_OUT" => Some("/home/tester/soak-out".to_owned()),
        "CETRA_SOAK_HOURS" => Some(hours.clone()),
        _ => None,
    })
    .expect("a config")
}

fn limits() -> Limits {
    Limits {
        wal_segment_bytes: 64.0 * 1024.0 * 1024.0,
        flush_max_bytes: 256.0 * 1024.0 * 1024.0,
        l0_files: 8.0,
    }
}

#[test]
fn short_runs_make_growth_informational() {
    let config = rule_config("24");
    // RSS grows by 500 KB a minute from the start: a leak, if the run were long enough to enforce it.
    let samples: Vec<Sample> = (0..24 * 60_i64)
        .map(|minute| {
            let elapsed = minute as f64 * 60.0;
            Sample {
                t: minute,
                elapsed_s: elapsed,
                rss_bytes: (1 << 30) + (minute as u64) * 500_000,
                target_spans_total: (2_000.0 * elapsed) as u64,
                appended_spans_total: (2_000.0 * elapsed) as u64,
                ..Sample::default()
            }
        })
        .collect();
    let verdicts = rules::evaluate(
        &samples,
        &BTreeMap::new(),
        &config,
        &limits(),
        &EndChecks::default(),
    );
    let rss = verdicts
        .iter()
        .find(|(name, _)| name == "rss_growth")
        .expect("the rule exists");
    assert!(
        matches!(rss.1, Verdict::Info(_)),
        "rss_growth is {:?}",
        rss.1
    );
    for (name, verdict) in &verdicts {
        if name.ends_with("_growth") || name.starts_with("query_growth_") {
            assert!(
                matches!(verdict, Verdict::Info(_) | Verdict::Skipped(_)),
                "{name} must be informational here: {verdict:?}"
            );
        }
    }
    assert!(
        verdicts
            .iter()
            .all(|(_, verdict)| !matches!(verdict, Verdict::Fail(_))),
        "the run passes: only growth rules are informational"
    );
}

/// Samples every minute up to `hours`, with first-group spans appended at `ratio(t)` of the target.
fn ingest_samples(hours: f64, ratio: impl Fn(f64) -> f64) -> Vec<Sample> {
    let total = (hours * 3600.0) as i64;
    let mut appended = 0.0_f64;
    (0..=total / 60)
        .map(|minute| {
            let elapsed = (minute * 60) as f64;
            if minute > 0 {
                appended += 2_000.0 * 60.0 * ratio(elapsed);
            }
            Sample {
                elapsed_s: elapsed,
                target_spans_total: (2_000.0 * elapsed) as u64,
                appended_spans_total: appended as u64,
                ..Sample::default()
            }
        })
        .collect()
}

#[test]
fn ingest_rate_rule_flags_a_shortfall() {
    let verdict = |samples: &[Sample], config: &SoakConfig| {
        rules::evaluate(
            samples,
            &BTreeMap::new(),
            config,
            &limits(),
            &EndChecks::default(),
        )
        .into_iter()
        .find(|(name, _)| name == "ingest_rate")
        .expect("the rule exists")
        .1
    };

    // A 1 h window at 0.95 of target fails.
    let config = rule_config("3");
    let short = ingest_samples(3.0, |_| 0.95);
    assert!(matches!(verdict(&short, &config), Verdict::Fail(_)));

    // A trailing window of 200 s (under half of the 600 s window) at 0.5 is ignored:
    // the run ends at 2,900 s, so only the windows before 2,700 s count.
    let config = rule_config("0.8055555555555556");
    let trailing = ingest_samples(2_900.0 / 3600.0, |t| if t > 2_700.0 { 0.5 } else { 1.0 });
    assert!(matches!(verdict(&trailing, &config), Verdict::Pass));

    // Late groups do not count: a window at 0.95 of first-group spans still fails,
    // whatever the late spans add.
    let config = rule_config("1");
    let mut late = ingest_samples(1.0, |_| 0.95);
    for sample in &mut late {
        sample.late_spans_total = 10 * sample.target_spans_total;
    }
    assert!(matches!(verdict(&late, &config), Verdict::Fail(_)));
}

#[test]
fn coverage_marks_user_traces() {
    fn tid(n: u8) -> TraceId {
        let mut bytes = [0_u8; 16];
        bytes[15] = n;
        TraceId::from_bytes(bytes).unwrap()
    }
    fn summary(n: u8, user: Option<&str>) -> TraceSummary {
        TraceSummary {
            trace_id: tid(n),
            name: "chat".to_owned(),
            start_time: 1_000,
            end_time: 2_000,
            span_count: 8,
            error_count: 0,
            llm_count: 1,
            total_tokens: Some(10),
            cost_total: Some(0.1),
            session_id: None,
            user_id: user.map(str::to_owned),
        }
    }
    let mut registry = registry::Registry::default();
    registry.insert(0, summary(1, Some("u")), true);
    registry.insert(0, summary(2, Some("u")), true);
    registry.insert(0, summary(3, Some("u")), false);
    registry.insert(0, summary(4, Some("v")), true);
    for n in 1..=4 {
        registry.touch(0, tid(n), 500);
    }

    // The deletion of user u lists trace 1 (complete) and trace 3 (incomplete).
    registry.apply_deletion(0, &[tid(1), tid(3)], Some("u"), 1_000);
    assert_eq!(registry.state(0, tid(1)), Some(registry::Expect::Deleted));
    assert_eq!(
        registry.state(0, tid(2)),
        Some(registry::Expect::Skip),
        "not listed"
    );
    assert_eq!(
        registry.state(0, tid(3)),
        Some(registry::Expect::Skip),
        "incomplete"
    );
    assert_eq!(
        registry.state(0, tid(4)),
        Some(registry::Expect::Visible),
        "another user"
    );

    // The deletion task lists every registered trace of the user.
    let mut listed = registry.user_trace_ids(0, "u");
    listed.sort();
    assert_eq!(listed, vec![tid(1), tid(2), tid(3)]);
}
