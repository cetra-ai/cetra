//! Storage soak (track R M7, D-R-11, `tests/soak.rs`, ignored).
//!
//! Drives `Storage` at a steady rate for hours: ingest, scores, deletions,
//! retention and queries run at the same time, and background compaction stays
//! on. Every minute one JSON line of resource and lag samples goes to
//! `<out>/samples.jsonl`. Run by `just soak`, configured by `LLMOBS_SOAK_*`.
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

#[allow(dead_code)] // each test target uses part of the shared helpers
mod common;

#[path = "soak/config.rs"]
mod config;
#[path = "soak/driver.rs"]
mod driver;
#[path = "soak/failures.rs"]
mod failures;
#[path = "soak/redact.rs"]
mod redact;
#[path = "soak/sample.rs"]
mod sample;

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::corpus::{CORPUS_FILE, CORPUS_SHA256, Corpus};
use common::machine::machine;
use common::payload::Payloads;
use common::rng::mix;
use common::s3::S3Env;
use config::{SoakConfig, parse_mix, prepare_data_dir};
use driver::{Shared, is_sampled};

use llmobs_storage::Storage;
use redact::Redactor;
use sample::Sampler;
use tokio::sync::watch;

/// The run description written to `run.json` before the soak starts.
#[derive(serde::Serialize)]
struct RunInfo<'a> {
    label: &'a str,
    started_unix: i64,
    machine: common::machine::Machine,
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
#[ignore = "run by hand: LLMOBS_SOAK_DIR and LLMOBS_SOAK_OUT, see `just soak`"]
async fn soak() {
    let config = SoakConfig::from_env().expect("the soak configuration");
    let out = config.out.clone();
    if out.exists() {
        assert!(
            out.read_dir().unwrap().next().is_none(),
            "LLMOBS_SOAK_OUT must be missing or empty"
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
    let events = std::fs::File::create(out.join("events.jsonl")).expect("the events file");
    let _ = tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::sync::Mutex::new(events))
        .try_init();

    let redactor = Redactor::for_run(s3.as_ref());
    let storage_config = config::storage_config(&config.dir, s3.as_ref(), &prefix);
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
        Storage::open(storage_config)
            .await
            .expect("the storage opens"),
    );
    let payloads = Arc::new(Payloads::new(Corpus::from_env()));
    let shared = Arc::new(Shared::new(
        Arc::clone(&storage),
        Arc::clone(&payloads),
        config.clone(),
    ));

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

    let mut sampler = Sampler::open(
        &config.dir,
        s3.clone().map(|target| (target, prefix.clone())),
        start,
    )
    .await
    .expect("the catalog opens read-only");
    let mut samples = std::fs::File::create(out.join("samples.jsonl")).expect("the samples file");
    let end = start + Duration::from_secs_f64(config.hours * 3600.0);
    while Instant::now() < end {
        tokio::time::sleep(Duration::from_secs(config.sample_secs)).await;
        let sample = sampler.sample(&shared).await;
        writeln!(samples, "{}", serde_json::to_string(&sample).unwrap()).unwrap();
        samples.flush().unwrap();
    }

    // Stop: the ingest tasks append their pending late groups first.
    stop_tx.send(true).expect("the tasks are listening");
    for task in tasks {
        task.await.unwrap();
    }
    let last = sampler.sample(&shared).await;
    writeln!(samples, "{}", serde_json::to_string(&last).unwrap()).unwrap();
    samples.flush().unwrap();
    storage.shutdown().await.expect("the storage shuts down");

    let counts = shared.failures.counts();
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
    let events_raw = std::fs::read_to_string(out.join("events.jsonl")).unwrap();
    let scrubbed: String = events_raw
        .lines()
        .map(|line| format!("{}\n", redactor.apply(line)))
        .collect();
    std::fs::write(out.join("events.scrubbed.jsonl"), scrubbed).unwrap();

    let elapsed = start.elapsed().as_secs_f64();
    let appended = last.appended_spans_total;
    let generated = shared
        .counters
        .generated_spans
        .load(std::sync::atomic::Ordering::Relaxed)
        .max(1);
    let payload = shared
        .counters
        .payload_bytes
        .load(std::sync::atomic::Ordering::Relaxed);
    let registered = shared
        .deletions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .registered;
    println!(
        "soak {}: {:.0} s, {} spans appended ({:.0} spans/s), {} payload bytes per span, {} scores, {} deletions registered",
        config.label,
        elapsed,
        appended,
        appended as f64 / elapsed.max(1.0),
        payload / generated,
        last.appended_scores_total,
        registered,
    );
    assert!(
        counts.is_empty(),
        "soak recorded failures: {:?}",
        counts
            .iter()
            .map(|(kind, count)| (kind.name(), count))
            .collect::<BTreeMap<_, _>>()
    );
}

// ---------------------------------------------------------------- unit tests

#[test]
fn parse_mix_reads_weights() {
    let default =
        parse_mix("list=30,filter=15,detail=25,sessions=10,metrics=10,scores=10").unwrap();
    assert_eq!(default.len(), 6);
    assert_eq!(default[0], (config::QueryKind::List, 30));
    assert!(parse_mix("list=0").is_err(), "every weight is at least 1");
    assert!(parse_mix("nope=3").is_err(), "unknown kinds are refused");
}

#[test]
fn config_defaults() {
    let lookup = |name: &str| match name {
        "LLMOBS_SOAK_DIR" => Some("/home/tester/soak".to_owned()),
        "LLMOBS_SOAK_OUT" => Some("/home/tester/soak-out".to_owned()),
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
        "LLMOBS_SOAK_DIR" => Some("/tmp/x".to_owned()),
        "LLMOBS_SOAK_OUT" => Some("/home/tester/soak-out".to_owned()),
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
    let excerpt = "Name:\tllmobs\nVmPeak:\t 900000 kB\nVmRSS:\t  123456 kB\nVmHWM:\t  200000 kB\nThreads:\t42\n";
    assert_eq!(
        sample::parse_status(excerpt),
        (123_456 * 1024, 200_000 * 1024, 42)
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
    let s3 = S3Env {
        endpoint: "https://fsn1.your-objectstorage.com".to_owned(),
        bucket: "acme-prod".to_owned(),
        region: "fsn1".to_owned(),
        path_style: false,
    };
    let redactor = Redactor::for_run(Some(&s3));
    let home = std::env::var("HOME").unwrap();
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap()
        .trim()
        .to_owned();
    let text = format!(
        r#"{{"bucket":"acme-prod","endpoint":"fsn1.your-objectstorage.com","home":"{home}/soak","host":"{host}","local":"http://127.0.0.1:7070"}}"#
    );
    let out = redactor.apply(&text);
    assert!(
        out.contains("<bucket>") && out.contains("<endpoint>"),
        "{out}"
    );
    assert!(out.contains("~/soak"), "{out}");
    assert!(out.contains("<host>"), "{out}");
    assert!(out.contains("127.0.0.1:7070"), "loopback stays: {out}");
    assert!(!out.contains("acme-prod"), "{out}");
    assert!(!out.contains("fsn1.your-objectstorage.com"), "{out}");
    assert!(!out.contains(&host), "{out}");
    // A secret inside a longer word is left alone.
    assert_eq!(
        redactor.apply("acme-prod-2 and acme-prod"),
        "acme-prod-2 and <bucket>"
    );
}

#[test]
fn cpu_fields_follow_the_command_name() {
    let stat = "123 (llmobs (soak)) S 1 1 1 0 -1 0 0 0 0 0 250 50 0 0 20 0 42";
    assert!((sample::parse_cpu(stat) - 3.0).abs() < 1e-9);
}
