//! M6.6 — Ingest benchmark (`tests/ingest_bench.rs`, ignored).
//!
//! Ingest rate, the flush drain and compaction throughput, on local disk or on
//! S3 (`LLMOBS_TEST_S3_ENDPOINT`, as `tests/s3.rs` reads it). Run by hand in
//! `--release`:
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
    clippy::too_many_lines
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
use common::machine::{dir_bytes, machine};
use common::rng::Rng;
use common::s3::S3Env;
use llmobs_core::batch::spans_to_record_batch;
use llmobs_core::enums::{SpanKind, SpanStatus};
use llmobs_core::ids::{ProjectId, SpanId, TraceId};
use llmobs_core::span::Span;
use llmobs_storage::config::{
    CompactionConfig, FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
};
use llmobs_storage::error::AppendError;
use llmobs_storage::{CompactionReport, Storage};

const SEED: u64 = 1;
const BATCH_SPANS: usize = 1_000;
const SPANS_PER_TRACE: usize = 10;
const LLM_IO_BYTES: usize = 10 * 1024;
const ATTRIBUTE_BYTES: usize = 300;
const POOL: usize = 16;
const SPAN_NS: i64 = 1_000_000;
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

fn span_id(index: u64) -> SpanId {
    SpanId::from_bytes((index + 1).to_be_bytes()).unwrap()
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64
}

/// Random lowercase words with spaces: compresses like prompt text.
fn text(rng: &mut Rng, len: usize) -> String {
    let mut out = String::with_capacity(len);
    while out.len() < len {
        let byte = if rng.below(6) == 0 {
            b' '
        } else {
            b'a' + rng.below(26) as u8
        };
        out.push(char::from(byte));
    }
    out
}

/// One batch of `BATCH_SPANS` spans, `SPANS_PER_TRACE` per trace. 10% are LLM
/// spans with 10 KiB input and output; the rest carry a 300-byte attribute and
/// no input or output (spec 03's mix).
fn build_batch(rng: &mut Rng, pool: &[String], traces: &AtomicU64, now: i64) -> Vec<Span> {
    let per_batch = BATCH_SPANS / SPANS_PER_TRACE;
    let first = traces.fetch_add(per_batch as u64, Ordering::Relaxed);
    let mut spans = Vec::with_capacity(BATCH_SPANS);
    for t in 0..per_batch {
        let trace = trace_id(first + t as u64);
        for s in 0..SPANS_PER_TRACE {
            let llm = rng.below(10) == 0;
            let (input, output, attributes) = if llm {
                (
                    Some(pool[rng.below(POOL)].clone()),
                    Some(pool[rng.below(POOL)].clone()),
                    BTreeMap::new(),
                )
            } else {
                let mut attributes = BTreeMap::new();
                attributes.insert(
                    "bench.payload".to_owned(),
                    pool[rng.below(POOL)][..ATTRIBUTE_BYTES].to_owned(),
                );
                (None, None, attributes)
            };
            spans.push(Span {
                project_id: project(),
                trace_id: trace,
                span_id: span_id(s as u64),
                parent_span_id: (s > 0).then(|| span_id(0)),
                name: format!("span-{s}"),
                span_kind: if llm { SpanKind::Llm } else { SpanKind::Span },
                start_time: now,
                end_time: now + SPAN_NS,
                status: SpanStatus::Unset,
                status_message: None,
                service_name: None,
                session_id: None,
                user_id: None,
                environment: None,
                release: None,
                tags: Vec::new(),
                metadata: Default::default(),
                provider: None,
                request_model: None,
                response_model: None,
                operation: None,
                input_tokens: None,
                output_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
                total_tokens: None,
                cost_input: None,
                cost_output: None,
                cost_total: None,
                cost_source: None,
                ttft_ns: None,
                request_params: None,
                input,
                input_format: None,
                output,
                output_format: None,
                input_preview: None,
                output_preview: None,
                system_instructions: None,
                tool_definitions: None,
                attributes,
                resource_attributes: Default::default(),
                scope_name: None,
                scope_version: None,
                events: Vec::new(),
                links: Vec::new(),
                media_refs: Vec::new(),
                truncated: Vec::new(),
                conventions: Vec::new(),
                ingest_time: now,
                schema_version: 1,
            });
        }
    }
    spans
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
    arrow_bytes: u64,
    backpressure: u64,
    last_ingest: i64,
}

impl Counts {
    fn add(&mut self, other: Counts) {
        self.spans += other.spans;
        self.arrow_bytes += other.arrow_bytes;
        self.backpressure += other.backpressure;
        self.last_ingest = self.last_ingest.max(other.last_ingest);
    }
}

async fn write_until(
    storage: Arc<Storage>,
    deadline: Instant,
    seed: u64,
    pool: Arc<Vec<String>>,
    traces: Arc<AtomicU64>,
) -> Counts {
    let mut rng = Rng::new(seed);
    let mut counts = Counts::default();
    while Instant::now() < deadline {
        let now = now_ns();
        let spans = build_batch(&mut rng, &pool, &traces, now);
        let batch: RecordBatch = spans_to_record_batch(&spans).unwrap();
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

    let mut rng = Rng::new(SEED);
    let pool: Arc<Vec<String>> = Arc::new(
        (0..POOL)
            .map(|_| text(&mut rng, LLM_IO_BYTES))
            .collect::<Vec<_>>(),
    );
    let traces = Arc::new(AtomicU64::new(0));

    // 1. Ingest.
    let begun = Instant::now();
    let deadline = begun + Duration::from_secs(secs);
    let handles: Vec<_> = (0..writers)
        .map(|w| {
            tokio::spawn(write_until(
                Arc::clone(&storage),
                deadline,
                SEED + w as u64,
                Arc::clone(&pool),
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
        "Store: {store}, ingest {secs} s, {writers} writers, batches of {BATCH_SPANS} spans, {SPANS_PER_TRACE} spans per trace, flush.max_bytes 256 MiB, memtable.hard_limit 1 GiB, min_free_bytes 0, compaction.interval 1 h",
    );
}
