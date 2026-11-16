//! M6.6 — Ingest benchmark (`tests/ingest_bench.rs`, ignored), on realistic
//! payloads since track R M7 (R7.3).
//!
//! Ingest rate, the flush drain and compaction throughput, on local disk or on
//! S3 (`LLMOBS_TEST_S3_ENDPOINT`, as `tests/s3.rs` reads it). The spans are the
//! traces of `common::payload`: real oasst2 text (`just fetch-corpus`), with the
//! structure of the captured fixtures. Run by hand in `--release`:
//!
//! ```bash
//! LLMOBS_INGEST_BENCH_DIR=/path/to/empty/dir cargo test --release \
//!   -p llmobs-storage --test ingest_bench -- --ignored --nocapture
//! ```
//!
//! `LLMOBS_INGEST_BENCH_DIR` is emptied first. `LLMOBS_INGEST_BENCH_SECS`
//! (default 60) is the ingest time and `LLMOBS_INGEST_BENCH_WRITERS` (default 8)
//! the number of appending tasks. With S3 the objects go under a fresh
//! `ingest-bench-<uuidv7>` prefix, which is deleted at the end.
//!
//! Compaction runs only as explicit passes: `compaction.interval` is 1 h (the maximum the config accepts), so the
//! background task (30 s by default) does not compact during the measurement.

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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arrow::array::RecordBatch;
use bytesize::ByteSize;
use common::corpus::Corpus;
use common::machine::{dir_bytes, machine};
use common::parquet_stats::{Sizes, active_spans_files, column_sizes};
use common::payload::{self, Payloads, span_payload_bytes};
use common::rng::mix;
use common::s3::S3Env;
use llmobs_core::batch::spans_to_record_batch;
use llmobs_core::enums::SpanKind;
use llmobs_core::ids::{ProjectId, TraceId};
use llmobs_core::span::Span;
use llmobs_storage::config::{
    CompactionConfig, FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
};
use llmobs_storage::error::AppendError;
use llmobs_storage::{CompactionReport, Storage};

const BATCH_SPANS: usize = 1_000;
const HOUR_NS: i64 = 3_600 * 1_000_000_000;
const MIB: f64 = 1024.0 * 1024.0;

// ---------------------------------------------------------------- data

fn project() -> ProjectId {
    "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
}

fn trace_id(index: u64) -> TraceId {
    let mut bytes = [0_u8; 16];
    bytes[8..].copy_from_slice(&(index + 1).to_be_bytes());
    TraceId::from_bytes(bytes).unwrap()
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64
}

// ---------------------------------------------------------------- config

fn config(dir: &Path, s3: Option<&S3Env>, prefix: &str) -> StorageConfig {
    let (object_store, s3_config) = match s3 {
        Some(target) => {
            let (url, s3_config) = target.config(prefix);
            (Some(url), s3_config)
        }
        None => (None, S3Config::default()),
    };
    StorageConfig {
        delete_objects: true,
        data_dir: dir.to_path_buf(),
        min_free_bytes: ByteSize::b(0),
        object_store,
        s3: s3_config,
        wal: WalConfig::default(),
        flush: FlushConfig {
            max_bytes: ByteSize::mib(256),
            ..FlushConfig::default()
        },
        memtable: MemtableConfig {
            hard_limit: ByteSize::gib(1),
        },
        query: QueryConfig::default(),
        anchor: Default::default(),
        gc: Default::default(),
        compaction: CompactionConfig {
            interval: Duration::from_secs(3600),
            ..CompactionConfig::default()
        },
        deletion: Default::default(),
    }
}

// ---------------------------------------------------------------- ingest

#[derive(Default, Clone, Copy)]
struct Counts {
    spans: u64,
    llm_spans: u64,
    payload_bytes: u64,
    /// Time spent building spans and their `RecordBatch`, summed over writers.
    gen_nanos: u64,
    arrow_bytes: u64,
    backpressure: u64,
    last_ingest: i64,
}

impl Counts {
    fn add(&mut self, other: Counts) {
        self.spans += other.spans;
        self.llm_spans += other.llm_spans;
        self.payload_bytes += other.payload_bytes;
        self.gen_nanos += other.gen_nanos;
        self.arrow_bytes += other.arrow_bytes;
        self.backpressure += other.backpressure;
        self.last_ingest = self.last_ingest.max(other.last_ingest);
    }
}

/// Appends batches of whole traces until the deadline. Each trace is generated
/// with `payload::trace`; its first and late groups are both appended at once,
/// since the bench measures throughput, not lateness.
async fn write_until(
    storage: Arc<Storage>,
    deadline: Instant,
    payloads: Arc<Payloads>,
    traces: Arc<AtomicU64>,
) -> Counts {
    let mut counts = Counts::default();
    while Instant::now() < deadline {
        let generating = Instant::now();
        let mut spans: Vec<Span> = Vec::with_capacity(BATCH_SPANS + 64);
        while spans.len() < BATCH_SPANS {
            let index = traces.fetch_add(1, Ordering::Relaxed);
            let generated =
                payload::trace(&payloads, project(), trace_id(index), mix(index), now_ns());
            for span in generated.first.iter().chain(&generated.late) {
                counts.payload_bytes += span_payload_bytes(span) as u64;
                counts.llm_spans += u64::from(span.span_kind == SpanKind::Llm);
            }
            spans.extend(generated.first);
            spans.extend(generated.late);
        }
        let now = now_ns();
        for span in &mut spans {
            span.ingest_time = now;
        }
        let batch: RecordBatch = spans_to_record_batch(&spans).unwrap();
        counts.gen_nanos += generating.elapsed().as_nanos() as u64;
        drop(spans);
        counts.arrow_bytes += batch.get_array_memory_size() as u64;
        loop {
            match storage.append_spans(project(), batch.clone()).await {
                Ok(_) => break,
                Err(AppendError::Backpressure) => {
                    counts.backpressure += 1;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("append failed: {error}"),
            }
        }
        counts.spans += batch.num_rows() as u64;
        counts.last_ingest = counts.last_ingest.max(now);
    }
    counts
}

/// Sums of the columns named by `matches`, for one level's files.
fn sum_matching(columns: &BTreeMap<String, Sizes>, matches: impl Fn(&str) -> bool) -> Sizes {
    columns.iter().filter(|(path, _)| matches(path)).fold(
        Sizes::default(),
        |mut total, (_, sizes)| {
            total.uncompressed += sizes.uncompressed;
            total.compressed += sizes.compressed;
            total
        },
    )
}

/// Compression of the active spans files, per level, for local disk. `when`
/// names the point of the run; `arrow_bytes` is the Arrow size ingested.
async fn print_compression(dir: &Path, arrow_bytes: u64, when: &str) {
    let objects = dir.join("objects");
    let files = active_spans_files(dir).await.unwrap();
    let sizes = column_sizes(&objects, &files).unwrap();
    let mut file_bytes: BTreeMap<String, u64> = BTreeMap::new();
    for (level, path) in &files {
        let len = std::fs::metadata(objects.join(path)).map_or(0, |meta| meta.len());
        *file_bytes.entry(format!("L{level}")).or_default() += len;
    }
    println!("compression, {when}:");
    let ratio = |s: Sizes| s.uncompressed as f64 / s.compressed.max(1) as f64;
    for (level, columns) in &sizes {
        let groups: [(&str, Sizes); 6] = [
            (
                "input",
                sum_matching(columns, |p| p == "input" || p.starts_with("input.")),
            ),
            (
                "output",
                sum_matching(columns, |p| p == "output" || p.starts_with("output.")),
            ),
            (
                "system_instructions",
                sum_matching(columns, |p| {
                    p == "system_instructions" || p.starts_with("system_instructions.")
                }),
            ),
            (
                "tool_definitions",
                sum_matching(columns, |p| {
                    p == "tool_definitions" || p.starts_with("tool_definitions.")
                }),
            ),
            (
                "attributes",
                sum_matching(columns, |p| {
                    p == "attributes" || p.starts_with("attributes.")
                }),
            ),
            ("all columns", sum_matching(columns, |_| true)),
        ];
        println!("  {level}:");
        for (label, sizes) in groups {
            println!(
                "    {label}: {} → {} = {:.2}×",
                sizes.uncompressed,
                sizes.compressed,
                ratio(sizes)
            );
        }
        let files = file_bytes.get(level).copied().unwrap_or(0);
        println!(
            "    Arrow / file bytes = {:.2}×",
            arrow_bytes as f64 / files.max(1) as f64
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "run by hand: LLMOBS_INGEST_BENCH_DIR=<empty dir> cargo test --release --test ingest_bench -- --ignored --nocapture"]
async fn ingest_benchmark() {
    let dir =
        PathBuf::from(std::env::var("LLMOBS_INGEST_BENCH_DIR").expect("LLMOBS_INGEST_BENCH_DIR"));
    let secs: u64 = std::env::var("LLMOBS_INGEST_BENCH_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let writers: usize = std::env::var("LLMOBS_INGEST_BENCH_WRITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);

    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();

    let s3 = S3Env::from_env();
    let prefix = format!("ingest-bench-{}", uuid::Uuid::now_v7());
    let config = config(&dir, s3.as_ref(), &prefix);
    let storage = Arc::new(Storage::open(config.clone()).await.unwrap());

    // The corpus is read once, outside the timing.
    let payloads = Arc::new(Payloads::new(Corpus::from_env()));
    let traces = Arc::new(AtomicU64::new(0));

    // 1. Ingest.
    let begun = Instant::now();
    let deadline = begun + Duration::from_secs(secs);
    let handles: Vec<_> = (0..writers)
        .map(|_| {
            tokio::spawn(write_until(
                Arc::clone(&storage),
                deadline,
                Arc::clone(&payloads),
                Arc::clone(&traces),
            ))
        })
        .collect();
    let mut total = Counts::default();
    for handle in handles {
        total.add(handle.await.unwrap());
    }
    let ingest_secs = begun.elapsed().as_secs_f64();
    println!(
        "ingest: {} spans in {:.1} s = {:.0} spans/s, {:.1} MiB/s Arrow, {:.0} Arrow bytes per span, {} backpressure retries",
        total.spans,
        ingest_secs,
        total.spans as f64 / ingest_secs,
        total.arrow_bytes as f64 / MIB / ingest_secs,
        total.arrow_bytes as f64 / total.spans.max(1) as f64,
        total.backpressure,
    );
    let generation_share = total.gen_nanos as f64 / 1e9 / (writers as f64 * ingest_secs) * 100.0;
    println!(
        "payload: {:.0} payload bytes per span, {:.1}% LLM spans, generation {generation_share:.1}% of writer time",
        total.payload_bytes as f64 / total.spans.max(1) as f64,
        total.llm_spans as f64 / total.spans.max(1) as f64 * 100.0,
    );

    // 2. Drain: flush everything, then reopen.
    let drained = Instant::now();
    storage.shutdown().await.unwrap();
    let drain_secs = drained.elapsed().as_secs_f64();
    drop(storage);
    let storage = Storage::open(config.clone()).await.unwrap();
    let flushed = match &s3 {
        Some(t) => t
            .list(&prefix)
            .await
            .unwrap()
            .iter()
            .map(|meta| meta.size)
            .sum(),
        None => dir_bytes(&dir.join("objects")),
    };
    println!("drain: shutdown flushed in {drain_secs:.2} s; flushed object bytes {flushed}");
    match &s3 {
        Some(_) => println!(
            "Arrow / flushed bytes = {:.2}×",
            total.arrow_bytes as f64 / flushed.max(1) as f64
        ),
        None => print_compression(&dir, total.arrow_bytes, "L0, the flush output").await,
    }

    // 3. Compaction: explicit passes until one commits nothing.
    let now = total.last_ingest + 3 * HOUR_NS;
    let begun = Instant::now();
    let mut passes = 0_u64;
    let mut jobs = 0_u64;
    let mut failed = 0_u64;
    let mut input_bytes = 0_u64;
    let mut output_bytes = 0_u64;
    loop {
        let report: CompactionReport = storage.compact(now).await.unwrap();
        passes += 1;
        jobs += report.jobs;
        failed += report.failed;
        input_bytes += report.input_bytes;
        output_bytes += report.output_bytes;
        if report.jobs == 0 {
            break;
        }
    }
    let compact_secs = begun.elapsed().as_secs_f64();
    storage.collect_obsolete(Duration::ZERO).await.unwrap();
    if s3.is_none() {
        print_compression(
            &dir,
            total.arrow_bytes,
            "L1 and any L0 left, after compaction and GC",
        )
        .await;
    }
    storage.shutdown().await.unwrap();
    drop(storage);

    println!(
        "compaction: {passes} passes, {jobs} jobs, {failed} failed, {compact_secs:.1} s, input {input_bytes} bytes ({:.1} MiB/s), output {output_bytes} bytes",
        input_bytes as f64 / MIB / compact_secs,
    );
    println!(
        "write amplification: {:.2} = (flush {flushed} + compaction output {output_bytes}) / flush {flushed}",
        (flushed as f64 + output_bytes as f64) / flushed.max(1) as f64,
    );

    if let Some(t) = &s3 {
        t.delete_prefix(&prefix).await.unwrap();
        println!("deleted {}", t.describe(&prefix));
    }

    let store = match &s3 {
        Some(t) => format!("S3 {}", t.describe(&prefix)),
        None => "local disk".to_owned(),
    };
    println!("\n## M6.6 ingest benchmark\n");
    println!("Machine: {}", machine(&dir));
    println!(
        "Store: {store}, ingest {secs} s, {writers} writers, batches of {BATCH_SPANS} spans, realistic traces (track R M7 payloads), flush.max_bytes 256 MiB, memtable.hard_limit 1 GiB, min_free_bytes 0, compaction.interval 1 h",
    );
}
