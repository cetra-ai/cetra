//! M3.8 — Benchmark (`tests/trace_bench.rs`, ignored).
//!
//! The M3 exit criterion: the S1 latency targets, measured through the real
//! engine and A2's reads. Run by hand on the same server as S1b:
//!
//! ```bash
//! CETRA_TRACE_BENCH_DIR=/path/to/data cargo test --release \
//!   -p cetra-storage --test trace_bench -- --ignored --nocapture
//! ```
//!
//! `CETRA_TRACE_BENCH_DIR` is generated once (if it holds no `catalog.db` with
//! a format marker) and reused afterwards. `CETRA_TRACE_BENCH_TRACES` overrides the trace
//! count (default 5,000,000). The data directory is never committed.
//!
//! With `CETRA_TEST_S3_ENDPOINT` (and `CETRA_TEST_S3_BUCKET`, `AWS_*`) the
//! data files go to `s3://<bucket>/trace-bench` and the catalog stays in the
//! directory. `CETRA_TRACE_BENCH_CACHE` is `cold` (default), `warm` or `off`:
//! `cold` empties `<dir>/cache` before the open, `warm` runs the whole query
//! set once untimed first, `off` sets `query.read_cache_bytes = 0`.
//! Spans carry the realistic payloads of `common::payload` (track R M7). The
//! corpus comes from `just fetch-corpus`. `CETRA_TRACE_BENCH_MAX_BYTES` (for
//! example `384MiB`) replaces the first attempt's 128 MiB flush size, which a
//! trace of tens of KiB of Arrow needs larger.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytesize::ByteSize;
use cetra_column::{Batch, ScalarValue, Selection};
use cetra_core::batch::spans_to_batch;
use cetra_core::enums::{SpanKind, SpanStatus};
use cetra_core::ids::{ProjectId, SpanId, TraceId};
use cetra_core::span::Span;
use cetra_storage::config::{
    AnchorConfig, FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
    WalSync,
};
use cetra_storage::{
    Agg, Bucket, BucketWidth, ListScope, Measure, Page, QueryContext, QueryOptions, Shape,
    SortOrder, TimeWindow, TraceListRequest, TraceSort, WidgetQuery, WidgetRequest, WidgetSource,
    trace_detail, trace_list, trace_list_plan, widget_query,
};
use common::corpus::{CORPUS_SHA256, Corpus};
use common::machine::{dir_bytes, machine};
use common::payload::{Payloads, span_payload_bytes};
use common::rng::{Rng, mix};
use common::s3::S3Env;

// ---------------------------------------------------------------- constants

#[allow(dead_code)] // each test target uses part of the shared helpers
mod common;

const SEED: u64 = 1;
const DEFAULT_TRACES: usize = 5_000_000;
const SPANS_PER_TRACE: usize = 10;
const PARTITIONS: usize = 8;
const WARM_RUNS: usize = 20;
const BATCH_SPANS: usize = 10_000;
const TIMEOUT: Duration = Duration::from_secs(120);
const NS: i64 = 1_000_000_000;
const MIN: i64 = 60 * NS;
const HOUR: i64 = 60 * MIN;
const DAY: i64 = 24 * HOUR;
const TAIL: i64 = 2 * MIN;
const DAY_SECONDS: i64 = 86_400;

const ROOT_NAMES: [&str; 30] = [
    "chat",
    "rag",
    "agent",
    "tools",
    "summarize",
    "extract",
    "classify",
    "embed",
    "search",
    "route",
    "plan",
    "reflect",
    "grade",
    "rewrite",
    "translate",
    "review",
    "index",
    "fetch",
    "parse",
    "rank",
    "guard",
    "score",
    "format",
    "validate",
    "merge",
    "split",
    "stream",
    "retry",
    "cache",
    "audit",
];
const RELEASES: [&str; 5] = ["v1.0", "v1.1", "v1.2", "v2.0", "v2.1"];
const TAGS: [&str; 10] = [
    "beta", "canary", "internal", "external", "gpu", "cpu", "batch", "realtime", "prod", "test",
];
const MODELS: [&str; 6] = [
    "gpt-4o",
    "gpt-4o-mini",
    "claude-3-5-sonnet",
    "claude-3-haiku",
    "llama-3-70b",
    "mistral-large",
];
const ENVIRONMENTS: [&str; 3] = ["production", "staging", "cetra-eval"];

fn trace_id(index: usize) -> TraceId {
    let mut bytes = [0_u8; 16];
    bytes[8..].copy_from_slice(&((index as u64) + 1).to_be_bytes());
    TraceId::from_bytes(bytes).unwrap()
}

fn span_id(index: usize) -> SpanId {
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&((index as u64) + 1).to_be_bytes());
    SpanId::from_bytes(bytes).unwrap()
}

fn project() -> ProjectId {
    "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
}

fn range_start() -> i64 {
    Utc.with_ymd_and_hms(2026, 3, 2, 0, 0, 0)
        .unwrap()
        .timestamp_nanos_opt()
        .unwrap()
}

fn range_end() -> i64 {
    range_start() + 7 * DAY
}

// ---------------------------------------------------------------- data plan

struct SpanPlan {
    start: i64,
    end: i64,
    span_index: usize,
    is_llm: bool,
    is_root: bool,
    tag_mask: u16,
    tokens: Option<i64>,
    model: Option<usize>,
    error: bool,
    cost_share: Option<f64>,
}

struct GroupPlan {
    ingest: i64,
    spans: Vec<SpanPlan>,
}

struct TracePlan {
    id: TraceId,
    /// The trace's position in the generated dataset: its seed comes from it.
    index: usize,
    start: i64,
    env: usize,
    session: Option<u32>,
    user: Option<u32>,
    release: usize,
    name: usize,
    split: bool,
    groups: Vec<GroupPlan>,
}

/// Start time uniform over the 7 days, at twice the rate 08:00–20:00 UTC.
fn start_time(rng: &mut Rng, start: i64) -> i64 {
    // 24 weights per day: 1 outside [8,20), 2 inside. Total 36/day, 252/week.
    let w = rng.unit() * 252.0;
    let day = (w / 36.0).floor() as i64;
    let mut rest = w - day as f64 * 36.0;
    let mut hour = 0_i64;
    while hour < 24 {
        let weight = if (8..20).contains(&hour) { 2.0 } else { 1.0 };
        if rest < weight {
            break;
        }
        rest -= weight;
        hour += 1;
    }
    let frac = rng.unit();
    start + (day * DAY_SECONDS + hour * 3600) * NS + (frac * 3600.0 * NS as f64) as i64
}

/// A bitmask with 0–3 of the 10 tag bits set (spec 06 § Data: 0–3 tags/span).
/// The plan's preview draws (length and fill byte). They are still drawn, in
/// the same order, so the data plan's stream is unchanged (R7.4). The previews
/// themselves now come from the payload content.
fn draw_preview(rng: &mut Rng) {
    rng.below(301);
    rng.below(26);
}

fn tag_mask(rng: &mut Rng) -> u16 {
    let count = rng.below(4);
    let mut mask = 0_u16;
    for _ in 0..count {
        mask |= 1_u16 << rng.below(TAGS.len());
    }
    mask
}

fn trace_plan(rng: &mut Rng, index: usize, start: i64, end: i64) -> TracePlan {
    let root_start = start_time(rng, start);
    let duration = (rng.lognormal(2.0, 60.0) * NS as f64).clamp(1.0, 600.0 * NS as f64) as i64;
    let root_end = root_start + duration;

    let env = if rng.unit() < 0.80 {
        0
    } else if rng.unit() < 0.75 {
        1
    } else {
        2
    };
    let session = (rng.unit() < 0.30).then(|| rng.below(50_000) as u32);
    let user = (rng.unit() < 0.60).then(|| rng.below(20_000) as u32);
    let release = rng.below(RELEASES.len());
    let name = rng.below(ROOT_NAMES.len());

    let llm_count = 1 + rng.below(5);
    let cost = rng.lognormal(0.002, 0.20);
    let cost_share = cost / llm_count as f64;
    let has_error = rng.unit() < 0.03;
    let error_span = rng.below(SPANS_PER_TRACE);

    let split = rng.unit() < 0.40;
    let group_count = if split { 2 + rng.below(2) } else { 1 };

    let children: Vec<usize> = (1..SPANS_PER_TRACE).collect();
    // Divide the children among ALL groups — the root group and the late ones —
    // so a split trace still keeps children next to its root (spec 06 § Data).
    let per_group = children.len().div_ceil(group_count);
    let mut child_groups: Vec<Vec<usize>> = Vec::new();
    for g in 0..group_count - 1 {
        let from = g * per_group;
        if from >= children.len() {
            child_groups.push(Vec::new());
        } else {
            let to = (from + per_group).min(children.len());
            child_groups.push(children[from..to].to_vec());
        }
    }

    let mut groups = Vec::with_capacity(group_count);
    // Exactly `llm_count` spans across the whole trace carry the cost, whether
    // the trace is split or not. A shared budget avoids counting the per-trace
    // cost once per group (spec 06 § Data).
    let mut llm_left = llm_count;
    // Root group: the root span plus the children not handed to late groups.
    {
        let mut spans = Vec::new();
        let root_tags = tag_mask(rng);
        draw_preview(rng);
        spans.push(SpanPlan {
            start: root_start,
            end: root_end,
            span_index: 0,
            is_llm: false,
            is_root: true,
            tag_mask: root_tags,
            tokens: None,
            model: None,
            error: has_error && error_span == 0,
            cost_share: None,
        });
        let used: usize = child_groups.iter().map(Vec::len).sum();
        for child in children[used..].iter() {
            let is_llm = llm_left > 0;
            if is_llm {
                llm_left -= 1;
            }
            let cs = root_start + (rng.unit() * duration as f64) as i64;
            let ce = (cs + duration / 10).min(root_end);
            let tokens = is_llm.then(|| rng.lognormal(800.0, 20_000.0) as i64);
            let tags = tag_mask(rng);
            if is_llm {
                draw_preview(rng);
            }
            let model = is_llm.then(|| rng.below(MODELS.len()));
            spans.push(SpanPlan {
                start: cs,
                end: ce,
                span_index: *child,
                is_llm,
                is_root: false,
                tag_mask: tags,
                tokens,
                model,
                error: has_error && error_span == *child,
                cost_share: is_llm.then_some(cost_share),
            });
        }
        let ingest = spans.iter().map(|s| s.end).max().unwrap() + NS;
        groups.push(GroupPlan { ingest, spans });
    }

    // Late groups: their spans start `delay` after the root.
    for child_group in child_groups {
        let delay = {
            let r = rng.unit();
            if r < 0.80 {
                (rng.unit() * MIN as f64) as i64
            } else if r < 0.95 {
                MIN + (rng.unit() * 29.0 * MIN as f64) as i64
            } else {
                HOUR + (rng.unit() * 2.0 * HOUR as f64) as i64
            }
        };
        let base = root_start + delay;
        if base >= end {
            continue; // a group that would start after the range end is dropped
        }
        let mut spans = Vec::new();
        for (j, child) in child_group.iter().enumerate() {
            let is_llm = llm_left > 0;
            if is_llm {
                llm_left -= 1;
            }
            let cs = base + (j as i64 + 1) * (duration / SPANS_PER_TRACE as i64);
            let ce = cs + duration / SPANS_PER_TRACE as i64;
            let tokens = is_llm.then(|| rng.lognormal(800.0, 20_000.0) as i64);
            let tags = tag_mask(rng);
            if is_llm {
                draw_preview(rng);
            }
            let model = is_llm.then(|| rng.below(MODELS.len()));
            spans.push(SpanPlan {
                start: cs,
                end: ce,
                span_index: *child,
                is_llm,
                is_root: false,
                tag_mask: tags,
                tokens,
                model,
                error: has_error && error_span == *child,
                cost_share: is_llm.then_some(cost_share),
            });
        }
        if spans.is_empty() {
            continue;
        }
        let ingest = spans.iter().map(|s| s.end).max().unwrap() + NS;
        groups.push(GroupPlan { ingest, spans });
    }

    TracePlan {
        id: trace_id(index),
        index,
        start: root_start,
        env,
        session,
        user,
        release,
        name,
        split,
        groups,
    }
}

/// The spans of one group. Content comes from `payloads`, seeded by the trace's
/// index; the plan keeps the tokens, cost, models, status and times.
fn build_spans(plan: &TracePlan, group: &GroupPlan, payloads: &Payloads) -> Vec<Span> {
    let seed = mix(SEED ^ mix(plan.index as u64));
    // An LLM span's call number is its rank among the trace's LLM spans.
    let mut llm_indices: Vec<usize> = plan
        .groups
        .iter()
        .flat_map(|g| g.spans.iter())
        .filter(|s| s.is_llm)
        .map(|s| s.span_index)
        .collect();
    llm_indices.sort_unstable();
    let calls = payloads.llm_calls(seed, llm_indices.len());
    let session = plan.session.map(|s| format!("session-{s}"));
    let user = plan.user.map(|u| format!("user-{u}"));
    let mut out = Vec::with_capacity(group.spans.len());
    for s in &group.spans {
        let mut tags = Vec::new();
        for (bit, tag) in TAGS.iter().enumerate() {
            if s.tag_mask & (1 << bit) != 0 {
                tags.push((*tag).to_owned());
            }
        }
        let content = if s.is_root {
            payloads.root(seed)
        } else if s.is_llm {
            let rank = llm_indices
                .binary_search(&s.span_index)
                .expect("an LLM span is among the trace's LLM spans");
            calls[rank].clone()
        } else {
            payloads.step(seed, s.span_index)
        };
        let mut span = Span {
            project_id: project(),
            trace_id: plan.id,
            span_id: span_id(s.span_index),
            parent_span_id: (!s.is_root).then(|| span_id(0)),
            name: if s.is_root {
                ROOT_NAMES[plan.name].to_owned()
            } else {
                format!("child-{}", s.span_index)
            },
            span_kind: if s.is_llm {
                SpanKind::Llm
            } else {
                SpanKind::Span
            },
            start_time: s.start,
            end_time: s.end,
            status: if s.error {
                SpanStatus::Error
            } else {
                SpanStatus::Unset
            },
            status_message: None,
            service_name: None,
            session_id: session.clone(),
            user_id: user.clone(),
            environment: Some(ENVIRONMENTS[plan.env].to_owned()),
            release: Some(RELEASES[plan.release].to_owned()),
            tags,
            metadata: Default::default(),
            provider: s.model.map(|_| "openai".to_owned()),
            request_model: s.model.map(|m| MODELS[m].to_owned()),
            response_model: None,
            prompt_name: None,
            prompt_version: None,
            operation: None,
            input_tokens: s.tokens,
            output_tokens: s.tokens.map(|t| t / 2),
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            total_tokens: s.tokens.map(|t| t + t / 2),
            cost_input: None,
            cost_output: None,
            cost_total: s.cost_share,
            cost_source: None,
            ttft_ns: None,
            request_params: None,
            input: None,
            input_format: None,
            output: None,
            output_format: None,
            input_preview: None,
            output_preview: None,
            system_instructions: None,
            tool_definitions: None,
            attributes: Default::default(),
            resource_attributes: Default::default(),
            scope_name: None,
            scope_version: None,
            events: Vec::new(),
            links: Vec::new(),
            media_refs: Vec::new(),
            truncated: Vec::new(),
            conventions: Vec::new(),
            ingest_time: group.ingest,
            schema_version: 1,
        };
        content.apply(&mut span);
        out.push(span);
    }
    out
}

// ---------------------------------------------------------------- config

fn config(backend: &Backend, max_bytes: ByteSize, read_cache: Option<ByteSize>) -> StorageConfig {
    let (object_store, s3) = match &backend.s3 {
        Some(t) => {
            let (url, s3) = t.config(S3_PREFIX);
            (Some(url), s3)
        }
        None => (None, S3Config::default()),
    };
    StorageConfig {
        delete_objects: true,
        data_dir: backend.dir.clone(),
        min_free_bytes: ByteSize::b(0),
        object_store,
        s3,
        wal: WalConfig {
            sync: WalSync::Interval(Duration::from_millis(100)),
            ..WalConfig::default()
        },
        flush: FlushConfig {
            max_bytes,
            max_age: Duration::from_secs(3600),
            ..FlushConfig::default()
        },
        memtable: MemtableConfig::default(),
        query: QueryConfig {
            target_partitions: PARTITIONS,
            read_cache_bytes: read_cache,
            ..QueryConfig::default()
        },
        anchor: AnchorConfig::default(),
        gc: Default::default(),
        compaction: Default::default(),
        deletion: Default::default(),
    }
}

fn count_spans_files(dir: &Path) -> usize {
    fn walk(path: &Path, count: &mut usize) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, count);
            } else if p.extension().is_some_and(|e| e == "parquet")
                && p.to_string_lossy().contains("tables/spans")
            {
                *count += 1;
            }
        }
    }
    let mut count = 0;
    walk(dir, &mut count);
    count
}

// ---------------------------------------------------------------- manifest

#[derive(serde::Serialize, serde::Deserialize)]
struct Manifest {
    traces: usize,
    files: Vec<usize>,
    gen_secs: f64,
    max_bytes: u64,
    sample: Vec<(String, i64)>,
    late_1_3h: usize,
    /// The S3 endpoint and bucket the data files were written to; absent for local mode.
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    bucket: Option<String>,
    /// The payload generator the data was written with (`payload_tag`); absent
    /// in datasets written before track R M7.
    #[serde(default)]
    payload: Option<String>,
    /// Arrow bytes appended and payload bytes generated (`span_payload_bytes`).
    #[serde(default)]
    arrow_bytes: u64,
    #[serde(default)]
    payload_bytes: u64,
}

/// Identifies the payload generator: a dataset is reused only with the same one.
fn payload_tag() -> String {
    format!("m7:{}", &CORPUS_SHA256[..12])
}

/// Where the dataset lives: the local directory, and an S3 prefix when
/// `CETRA_TEST_S3_ENDPOINT` is set (D119).
struct Backend {
    dir: PathBuf,
    s3: Option<S3Env>,
}

/// The fixed S3 prefix, so the dataset is reused across runs.
const S3_PREFIX: &str = "trace-bench";

impl Backend {
    fn from_env(dir: PathBuf) -> Self {
        let s3 = S3Env::from_env();
        Self { dir, s3 }
    }
}

fn peak_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb = rest.split_whitespace().next()?.parse::<u64>().ok()?;
            return Some(kb.saturating_mul(1024));
        }
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CacheMode {
    Cold,
    Warm,
    Off,
}

impl CacheMode {
    fn from_env() -> Self {
        match std::env::var("CETRA_TRACE_BENCH_CACHE").as_deref() {
            Ok("warm") => Self::Warm,
            Ok("off") => Self::Off,
            Ok("cold") | Err(_) => Self::Cold,
            Ok(other) => panic!("CETRA_TRACE_BENCH_CACHE must be cold, warm or off, not {other}"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Warm => "warm",
            Self::Off => "off",
        }
    }
}

/// Spans Parquet files and bytes of the dataset: the directory's for local
/// mode, the listed objects' for S3.
async fn dataset_files(backend: &Backend) -> (usize, u64) {
    match &backend.s3 {
        None => (count_spans_files(&backend.dir), dir_bytes(&backend.dir)),
        Some(s3) => {
            let metas = s3.list(S3_PREFIX).await.unwrap();
            let count = metas
                .iter()
                .filter(|meta| {
                    let key = meta.location.to_string();
                    key.contains("tables/spans") && key.ends_with(".parquet")
                })
                .count();
            (count, metas.iter().map(|meta| meta.size).sum())
        }
    }
}

/// The dataset to query: the stored one when its catalog carries a format
/// marker, its manifest matches the target, the payload generator and (with S3)
/// the endpoint and bucket; otherwise a generated one. A catalog without a
/// matching manifest is regenerated, since its payloads are unknown. A catalog
/// with no format marker (`PRAGMA user_version` 0) predates format versions and
/// is regenerated too: the storage refuses to open it (`PreFormat`).
async fn dataset(backend: &Backend, traces: usize) -> Manifest {
    let has_catalog = backend.dir.join("catalog.db").is_file();
    let reusable = has_catalog && catalog_marker(&backend.dir).await != 0;
    let stored = std::fs::read_to_string(manifest_path(&backend.dir))
        .ok()
        .and_then(|text| serde_json::from_str::<Manifest>(&text).ok());
    let payload = Some(payload_tag());
    match (reusable, &backend.s3, stored) {
        (true, Some(s3), Some(manifest))
            if manifest.endpoint.as_deref() == Some(s3.endpoint.as_str())
                && manifest.bucket.as_deref() == Some(s3.bucket.as_str())
                && manifest.payload == payload =>
        {
            manifest
        }
        (true, None, Some(manifest)) if manifest.payload == payload => manifest,
        _ => generate_dataset(backend, traces).await,
    }
}

/// `PRAGMA user_version` of the stored `<dir>/catalog.db`, read through a
/// read-only connection (`0` when unset).
async fn catalog_marker(dir: &Path) -> i64 {
    use sqlx::{ConnectOptions, Connection};
    let mut connection = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(dir.join("catalog.db"))
        .read_only(true)
        .connect()
        .await
        .expect("open the stored catalog.db read-only");
    let marker = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut connection)
        .await
        .expect("read the stored catalog.db format marker");
    connection.close().await.ok();
    marker
}

// The bench reports on stdout like the rest of the file.
#[allow(clippy::print_stdout)]
async fn generate_dataset(backend: &Backend, traces: usize) -> Manifest {
    let max_bytes = match std::env::var("CETRA_TRACE_BENCH_MAX_BYTES") {
        Ok(value) => value
            .parse::<ByteSize>()
            .expect("CETRA_TRACE_BENCH_MAX_BYTES is a size such as 384MiB"),
        Err(_) => ByteSize::mib(128),
    };
    let (count, _, generated) = generate(backend, traces, max_bytes).await;
    if (900..=1100).contains(&count) {
        return generated;
    }
    // `validate` requires `flush.max_bytes` in [1 MiB, hard_limit).
    let scaled = ((max_bytes.0 as f64 * count as f64 / 1000.0).max(ByteSize::mib(1).0 as f64)
        as u64)
        .min(ByteSize::gib(1).0 - 1);
    let max_bytes = ByteSize::b(scaled);
    let (count2, _, generated2) = generate(backend, traces, max_bytes).await;
    println!("regenerated once: spans files {count} → {count2}, max_bytes {max_bytes}");
    generated2
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("bench-manifest.json")
}

// ---------------------------------------------------------------- generation

/// Ordered groups (ingest time, then stable trace/group order).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Pending {
    ingest: i64,
    trace: usize,
    group: usize,
}

fn q4_sample(plans: &[TracePlan]) -> Vec<(String, i64)> {
    let mut sample = Vec::new();
    let mut split = 0;
    let mut single = 0;
    for plan in plans {
        if plan.split && split < 50 {
            sample.push((plan.id.to_hex(), plan.start));
            split += 1;
        } else if !plan.split && single < 49 {
            sample.push((plan.id.to_hex(), plan.start));
            single += 1;
        }
        if sample.len() == 99 {
            break;
        }
    }
    sample
}

/// Appends a generated batch, waiting out `Backpressure` (the memtable is full
/// until a flush makes room). Other errors fail the run.
async fn append_retry(storage: &cetra_storage::Storage, batch: Batch) {
    loop {
        match storage.append_spans(project(), batch.clone()).await {
            Ok(_) => return,
            Err(cetra_storage::error::AppendError::Backpressure) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => panic!("append failed: {error}"),
        }
    }
}

/// Column batch and payload bytes written by one generation.
#[derive(Default)]
struct Written {
    arrow_bytes: u64,
    payload_bytes: u64,
}

async fn append_batch(storage: &cetra_storage::Storage, batch: &[Span], written: &mut Written) {
    let rb = spans_to_batch(batch).unwrap();
    written.arrow_bytes += rb.memory_bytes() as u64;
    append_retry(storage, rb).await;
}

/// Builds the spans of `groups` in order and appends them in batches of about
/// `BATCH_SPANS` spans.
async fn append_groups(
    storage: &cetra_storage::Storage,
    plans: &[TracePlan],
    payloads: &Payloads,
    groups: &[Pending],
    written: &mut Written,
) {
    let mut batch: Vec<Span> = Vec::with_capacity(BATCH_SPANS + SPANS_PER_TRACE);
    for p in groups {
        let spans = build_spans(&plans[p.trace], &plans[p.trace].groups[p.group], payloads);
        written.payload_bytes += spans
            .iter()
            .map(|s| span_payload_bytes(s) as u64)
            .sum::<u64>();
        batch.extend(spans);
        if batch.len() >= BATCH_SPANS {
            append_batch(storage, &batch, written).await;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        append_batch(storage, &batch, written).await;
    }
}

/// Generate into `dir` (wiping it first) and return the file count and elapsed
/// time. Appends in ingest-time order, batches of ~10,000 spans. The last two
/// minutes of ingest time stay in the WAL: append everything else, shutdown,
/// reopen, append the tail, drop without shutdown.
async fn generate(
    backend: &Backend,
    traces: usize,
    max_bytes: ByteSize,
) -> (usize, Duration, Manifest) {
    let begun = Instant::now();
    let dir = backend.dir.as_path();
    if let Some(s3) = &backend.s3 {
        s3.delete_prefix(S3_PREFIX).await.unwrap();
    }
    if dir.exists() {
        std::fs::remove_dir_all(dir).unwrap();
    }
    std::fs::create_dir_all(dir).unwrap();

    let payloads = Payloads::new(Corpus::from_env());
    let start = range_start();
    let end = range_end();
    let mut rng = Rng::new(SEED);
    let mut plans = Vec::with_capacity(traces);
    for index in 0..traces {
        plans.push(trace_plan(&mut rng, index, start, end));
    }
    let late_1_3h = plans
        .iter()
        .filter(|p| p.groups.iter().any(|g| g.ingest - p.start > HOUR))
        .count();

    let mut pending: Vec<Pending> = Vec::new();
    for (t, plan) in plans.iter().enumerate() {
        for (g, group) in plan.groups.iter().enumerate() {
            pending.push(Pending {
                ingest: group.ingest,
                trace: t,
                group: g,
            });
        }
    }
    pending.sort();
    let max_ingest = pending.last().map_or(end, |p| p.ingest);
    let tail_start = max_ingest.saturating_sub(TAIL);

    let mut manifest = Manifest {
        traces,
        files: Vec::new(),
        gen_secs: 0.0,
        max_bytes: max_bytes.0,
        sample: q4_sample(&plans),
        late_1_3h,
        endpoint: backend.s3.as_ref().map(|s3| s3.endpoint.clone()),
        bucket: backend.s3.as_ref().map(|s3| s3.bucket.clone()),
        payload: Some(payload_tag()),
        arrow_bytes: 0,
        payload_bytes: 0,
    };

    let mut written = Written::default();
    let files = {
        let storage = cetra_storage::Storage::open(config(backend, max_bytes, None))
            .await
            .unwrap();
        let early: Vec<Pending> = pending
            .iter()
            .copied()
            .filter(|p| p.ingest < tail_start)
            .collect();
        append_groups(&storage, &plans, &payloads, &early, &mut written).await;
        storage.shutdown().await.unwrap();
        drop(storage);
        let files = dataset_files(backend).await.0;

        let storage = cetra_storage::Storage::open(config(backend, max_bytes, None))
            .await
            .unwrap();
        let late: Vec<Pending> = pending
            .iter()
            .copied()
            .filter(|p| p.ingest >= tail_start)
            .collect();
        append_groups(&storage, &plans, &payloads, &late, &mut written).await;
        // Drop without shutdown: the tail stays in the WAL / memtable.
        drop(storage);
        files
    };

    manifest.files.push(files);
    manifest.gen_secs = begun.elapsed().as_secs_f64();
    manifest.arrow_bytes = written.arrow_bytes;
    manifest.payload_bytes = written.payload_bytes;
    std::fs::write(
        manifest_path(dir),
        serde_json::to_string(&manifest).unwrap(),
    )
    .unwrap();
    (files, begun.elapsed(), manifest)
}

// ---------------------------------------------------------------- timing

#[derive(Clone, Copy)]
struct Stats {
    cold: f64,
    p50: f64,
    p95: f64,
    max: f64,
}

async fn bench<F, Fut>(mut op: F) -> Stats
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    let t = Instant::now();
    op().await;
    let cold = t.elapsed().as_secs_f64() * 1000.0;
    let mut warm = Vec::with_capacity(WARM_RUNS);
    for _ in 0..WARM_RUNS {
        let t = Instant::now();
        op().await;
        warm.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    warm.sort_by(f64::total_cmp);
    let pick = |p: f64| -> f64 {
        let idx = ((p * WARM_RUNS as f64).ceil() as usize)
            .saturating_sub(1)
            .min(WARM_RUNS - 1);
        warm[idx]
    };
    Stats {
        cold,
        p50: pick(0.50),
        p95: pick(0.95),
        max: warm[WARM_RUNS - 1],
    }
}

/// `bench`, or one untimed run when `untimed` (the warm-up pass).
async fn measure<F, Fut>(untimed: bool, mut op: F) -> Stats
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    if untimed {
        op().await;
        return Stats {
            cold: 0.0,
            p50: 0.0,
            p95: 0.0,
            max: 0.0,
        };
    }
    bench(op).await
}

fn request<'a>(
    filter: Option<&'a str>,
    limit: usize,
    window: Option<TimeWindow>,
) -> TraceListRequest<'a> {
    TraceListRequest {
        filter,
        search: None,
        strict_exclusions: false,
        scope: ListScope {
            window,
            include_eval: false,
            include_experiments: false,
        },
        sort: TraceSort::StartTime,
        order: SortOrder::Desc,
        limit,
        cursor: None,
        timeout: TIMEOUT,
    }
}

fn active_phys_rows(batch: &Batch) -> Vec<usize> {
    match batch.selection() {
        Selection::All => (0..batch.raw_len()).collect(),
        Selection::Mask(mask) => mask.iter_ones().collect(),
        Selection::Indices(indices) => indices.iter().map(|&idx| idx as usize).collect(),
    }
}

fn hex_ids(page: &Page) -> Vec<String> {
    let mut ids = Vec::new();
    for batch in &page.batches {
        if batch.num_rows() == 0 {
            continue;
        }
        let Some(col) = batch.column_by_name("trace_id") else {
            continue;
        };
        for phys in active_phys_rows(batch) {
            if let Ok(ScalarValue::Utf8(id)) = col.get_scalar(phys) {
                ids.push(id);
            }
        }
    }
    ids
}

async fn cost_threshold(range: &QueryContext, lookback: i64) -> f64 {
    let cutoff = range_end() - lookback;
    let window = TimeWindow::new(cutoff, range_end() + DAY).unwrap();
    let req = TraceListRequest {
        filter: None,
        search: None,
        strict_exclusions: false,
        scope: ListScope {
            window: Some(window),
            include_eval: true,
            include_experiments: true,
        },
        sort: TraceSort::CostTotal,
        order: SortOrder::Desc,
        limit: 50,
        cursor: None,
        timeout: TIMEOUT,
    };
    let page = trace_list(range, req).await.unwrap();
    let mut costs = Vec::new();
    for batch in &page.batches {
        if batch.num_rows() == 0 {
            continue;
        }
        let col = batch.column_by_name("cost_total").unwrap();
        for phys in active_phys_rows(batch) {
            match col.get_scalar(phys).unwrap() {
                ScalarValue::F64(v) => costs.push(v),
                ScalarValue::Decimal128 { value, scale, .. } => {
                    let v = if scale >= 0 {
                        (value as f64) / 10f64.powi(i32::from(scale))
                    } else {
                        (value as f64) * 10f64.powi(-i32::from(scale))
                    };
                    costs.push(v);
                }
                ScalarValue::Null(_) => {}
                other => panic!("unexpected cost_total scalar: {other:?}"),
            }
        }
    }
    costs.sort_by(|a, b| b.total_cmp(a));
    costs.get(49).copied().unwrap_or(0.05).max(1e-6)
}

async fn plan_info(ctx: &QueryContext, filter: Option<&str>, window: Option<TimeWindow>) -> String {
    let req = request(filter, 50, window);
    match trace_list_plan(ctx, req).await {
        Ok(plan) if plan.contains("mode=list") => "list".to_owned(),
        _ => "range".to_owned(),
    }
}

fn differing(a: &[String], b: &[String]) -> usize {
    (0..a.len().max(b.len()))
        .filter(|i| a.get(*i) != b.get(*i))
        .count()
}

const Q5_SQL: &str = "SELECT date_bin(INTERVAL '1 hour', start_time, TIMESTAMP '1970-01-01T00:00:00Z') AS bucket, count(*) AS traces, sum(CASE WHEN error_count > 0 THEN 1 ELSE 0 END) AS errors, sum(cost_total) AS cost, approx_percentile_cont(duration_ns, 0.5) AS p50, approx_percentile_cont(duration_ns, 0.95) AS p95, approx_percentile_cont(duration_ns, 0.99) AS p99 FROM traces GROUP BY bucket ORDER BY bucket";

// ---------------------------------------------------------------- main

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "run by hand on the S1b server with CETRA_TRACE_BENCH_DIR set"]
async fn trace_benchmark() {
    let traces: usize = std::env::var("CETRA_TRACE_BENCH_TRACES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_TRACES);
    let tmp = tempfile::tempdir().unwrap();
    let dir: PathBuf = std::env::var("CETRA_TRACE_BENCH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| tmp.path().to_path_buf());
    let backend = Backend::from_env(dir);

    let manifest = dataset(&backend, traces).await;
    let max_bytes = ByteSize::b(manifest.max_bytes);

    // The read cache applies to S3 only; local runs keep the default.
    let cache = backend.s3.as_ref().map(|_| CacheMode::from_env());
    let cache_dir = backend.dir.join("cache");
    if cache == Some(CacheMode::Cold) && cache_dir.exists() {
        std::fs::remove_dir_all(&cache_dir).unwrap();
    }
    let read_cache = (cache == Some(CacheMode::Off)).then(|| ByteSize::b(0));
    // Warm: the whole query set runs once untimed, then again measured.
    let passes = if cache == Some(CacheMode::Warm) { 2 } else { 1 };

    // Open for queries: the WAL tail replays into the memtable.
    let storage = cetra_storage::Storage::open(config(&backend, max_bytes, read_cache))
        .await
        .unwrap();

    run_benchmark(&storage, &backend, &manifest, cache, passes, PARTITIONS).await;
    drop(storage);
    drop(tmp);
}

#[allow(clippy::too_many_arguments)]
async fn run_benchmark(
    storage: &cetra_storage::Storage,
    backend: &Backend,
    manifest: &Manifest,
    cache: Option<CacheMode>,
    passes: usize,
    partitions: usize,
) {
    let ctx = QueryContext::new(
        storage
            .query(project(), QueryOptions::new(None, None))
            .await
            .unwrap(),
        None,
    )
    .unwrap();
    let mut range = QueryContext::new(
        storage
            .query(project(), QueryOptions::new(None, None))
            .await
            .unwrap(),
        None,
    )
    .unwrap();
    range.set_force_range_mode(true);

    let default_plan = trace_list_plan(&ctx, request(None, 50, None))
        .await
        .unwrap_or_default();
    assert!(
        default_plan.contains("mode=list"),
        "trace_list did not get list mode:\n{default_plan}"
    );

    enum MissKind {
        List {
            filter: Option<String>,
            window: Option<TimeWindow>,
        },
        Detail {
            is_q4b: bool,
        },
        Sql,
    }

    struct BenchRow {
        name: String,
        mode: String,
        stats: Stats,
        target: f64,
        rows: usize,
        differing: usize,
        miss: MissKind,
    }

    let mut rows: Vec<BenchRow> = Vec::new();
    for pass in 0..passes {
        let untimed = pass + 1 < passes;
        rows.clear();

        macro_rules! list_query {
            ($name:expr, $target:expr, $filter:expr, $window:expr) => {{
                let filter: Option<&str> = $filter;
                let window: Option<TimeWindow> = $window;
                let mode = plan_info(&ctx, filter, window).await;
                let req = || request(filter, 50, window);
                let stats = measure(untimed, || async {
                    let page = trace_list(&ctx, req()).await.unwrap();
                    std::hint::black_box(page.batches.iter().map(Batch::num_rows).sum::<usize>());
                })
                .await;
                let a = hex_ids(&trace_list(&ctx, req()).await.unwrap());
                let b = hex_ids(&trace_list(&range, req()).await.unwrap());
                let diff = differing(&a, &b);
                assert_eq!(
                    diff, 0,
                    "{} differing ids against range mode: {diff}",
                    $name
                );
                rows.push(BenchRow {
                    name: $name.to_owned(),
                    mode,
                    stats,
                    target: $target,
                    rows: a.len(),
                    differing: diff,
                    miss: MissKind::List {
                        filter: filter.map(str::to_owned),
                        window,
                    },
                });
            }};
        }

        list_query!("Q1", 200.0, None, None);
        list_query!("Q2", 2000.0, Some("cost_total > 0.05"), None);
        let t2 = format!("cost_total > {:.6}", cost_threshold(&range, 2 * HOUR).await);
        let t15 = format!(
            "cost_total > {:.6}",
            cost_threshold(&range, 15 * HOUR).await
        );
        list_query!("Q2r-2h", 2000.0, Some(t2.as_str()), None);
        list_query!("Q2r-15h", 2000.0, Some(t15.as_str()), None);
        list_query!("Q3", 500.0, Some("environment = 'production'"), None);
        let window = TimeWindow::new(range_start() + 3 * DAY, range_start() + 4 * DAY).unwrap();
        list_query!("Q6", 200.0, None, Some(window));

        for (label, use_around, target) in [("Q4a", true, 100.0), ("Q4b", false, 1000.0)] {
            let mut samples = Vec::with_capacity(manifest.sample.len());
            for (id, start) in &manifest.sample {
                let one = Instant::now();
                if use_around {
                    let q = storage
                        .query(project(), QueryOptions::new(Some(*start), None))
                        .await
                        .unwrap();
                    let c = QueryContext::new(q, None).unwrap();
                    let found = trace_detail(&c, id, TIMEOUT).await.unwrap();
                    std::hint::black_box(found.is_some());
                } else {
                    let found = trace_detail(&ctx, id, TIMEOUT).await.unwrap();
                    std::hint::black_box(found.is_some());
                }
                samples.push(one.elapsed().as_secs_f64() * 1000.0);
            }
            samples.sort_by(f64::total_cmp);
            let pick = |p: f64| -> f64 {
                if samples.is_empty() {
                    return 0.0;
                }
                let idx = ((p * samples.len() as f64).ceil() as usize)
                    .saturating_sub(1)
                    .min(samples.len() - 1);
                samples[idx]
            };
            rows.push(BenchRow {
                name: label.to_owned(),
                mode: "lookup".to_owned(),
                stats: Stats {
                    cold: samples.first().copied().unwrap_or(0.0),
                    p50: pick(0.50),
                    p95: pick(0.95),
                    max: samples.last().copied().unwrap_or(0.0),
                },
                target,
                rows: manifest.sample.len(),
                differing: 0,
                miss: MissKind::Detail {
                    is_q4b: !use_around,
                },
            });
        }

        // Q5 — SQL dashboard over the 7 days.
        let stats = measure(untimed, || async {
            let res = ctx.sql_json(Q5_SQL, 30_000, TIMEOUT).await.unwrap();
            std::hint::black_box(res.rows.len());
        })
        .await;
        let q5_rows = ctx
            .sql_json(Q5_SQL, 30_000, TIMEOUT)
            .await
            .unwrap()
            .rows
            .len();
        rows.push(BenchRow {
            name: "Q5".to_owned(),
            mode: "sql".to_owned(),
            stats,
            target: 2000.0,
            rows: q5_rows,
            differing: 0,
            miss: MissKind::Sql,
        });

        println!(
            "read cache after pass {pass} ({}): {:?}",
            if untimed { "untimed" } else { "measured" },
            storage.read_cache_stats()
        );
    }

    let window = TimeWindow::new(range_start(), range_end()).unwrap();
    let query = WidgetQuery {
        source: WidgetSource::Traces,
        filter: None,
        measures: vec![
            Measure {
                agg: Agg::Count,
                field: None,
                condition: None,
            },
            Measure {
                agg: Agg::Rate,
                field: None,
                condition: Some("error_count > 0".to_owned()),
            },
            Measure {
                agg: Agg::P95,
                field: Some("duration_ns".to_owned()),
                condition: None,
            },
            Measure {
                agg: Agg::Sum,
                field: Some("cost_total".to_owned()),
                condition: None,
            },
            Measure {
                agg: Agg::Sum,
                field: Some("total_tokens".to_owned()),
                condition: None,
            },
        ],
        breakdown: None,
    };
    let info = bench(|| async {
        let data = widget_query(
            &ctx,
            WidgetRequest {
                window,
                bucket: Bucket::Fixed(BucketWidth::H1),
                dashboard_filter: None,
                group_by: None,
                include_eval: true,
                include_experiments: true,
                query: &query,
                shape: Shape::Series,
                compare: false,
                timeout: TIMEOUT,
            },
        )
        .await
        .unwrap();
        std::hint::black_box(data);
    })
    .await;
    println!(
        "informational metrics: cold {:.1} ms p50 {:.1} ms p95 {:.1} ms max {:.1} ms",
        info.cold, info.p50, info.p95, info.max
    );

    let (_, dataset_bytes) = dataset_files(backend).await;
    let store = match (&backend.s3, cache) {
        (Some(s3), Some(mode)) => {
            format!("S3 {}, read cache {}", s3.describe(S3_PREFIX), mode.name())
        }
        _ => "local disk".to_owned(),
    };
    println!("\n## Query engine benchmark (cetra-column)\n");
    println!("Store: {store}");
    println!("Machine: {}", machine(&backend.dir));
    println!(
        "Dataset: {} traces, {} spans files ({}), {} bytes, generated in {:.1} s, target_partitions {partitions}, {} traces with a 1–3 h late group",
        manifest.traces,
        manifest.files.last().copied().unwrap_or(0),
        manifest
            .files
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(" → "),
        dataset_bytes,
        manifest.gen_secs,
        manifest.late_1_3h,
    );
    println!(
        "Payloads: {} batch bytes, {} payload bytes, {} batch bytes per trace",
        manifest.arrow_bytes,
        manifest.payload_bytes,
        manifest.arrow_bytes / manifest.traces.max(1) as u64,
    );
    if let Some(rss) = peak_rss_bytes() {
        println!("Peak RSS (VmHWM): {} ({rss} bytes)", ByteSize::b(rss));
    }
    println!();
    println!("| query | mode | cold | p50 | p95 | max | target | pass | rows | differing ids |");
    println!("|---|---|---:|---:|---:|---:|---:|---|---:|---:|");
    for row in &rows {
        println!(
            "| {} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.0} | {} | {} | {} |",
            row.name,
            row.mode,
            row.stats.cold,
            row.stats.p50,
            row.stats.p95,
            row.stats.max,
            row.target,
            if row.stats.p95 <= row.target {
                "pass"
            } else {
                "miss"
            },
            row.rows,
            row.differing,
        );
    }
    for row in &rows {
        if row.stats.p95 > row.target {
            match &row.miss {
                MissKind::Detail { is_q4b } => {
                    if let Some((id, start)) = manifest.sample.first() {
                        if *is_q4b {
                            let _ = trace_detail(&ctx, id, TIMEOUT).await;
                            if let Some(metrics) = ctx.format_last_metrics() {
                                println!("\n{} missed; ExecMetrics:\n{}", row.name, metrics);
                            }
                        } else {
                            let q = storage
                                .query(project(), QueryOptions::new(Some(*start), None))
                                .await
                                .unwrap();
                            let c = QueryContext::new(q, None).unwrap();
                            let _ = trace_detail(&c, id, TIMEOUT).await;
                            if let Some(metrics) = c.format_last_metrics() {
                                println!("\n{} missed; ExecMetrics:\n{}", row.name, metrics);
                            }
                        }
                    }
                }
                MissKind::List { filter, window } => {
                    let _ = trace_list(&ctx, request(filter.as_deref(), 50, *window)).await;
                    if let Some(metrics) = ctx.format_last_metrics() {
                        println!("\n{} missed; ExecMetrics:\n{}", row.name, metrics);
                    }
                }
                MissKind::Sql => {
                    let _ = ctx.sql_json(Q5_SQL, 30_000, TIMEOUT).await;
                }
            }
        }
    }

    drop(ctx);
    drop(range);
}
