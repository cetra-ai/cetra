//! Comprehensive 39-function differential oracle suite comparing `crates/storage/src/reads_qe/`
//! against `crates/storage/src/reads/` over a unified realistic dataset (L1 compacted Parquet +
//! L0 uncompacted Parquet + live memtable + split traces + duplicate span copies + GDPR tombstones
//! + LWW/deleted/issue scores + experiments + reserved environments + `visible_from` + `around`
//! + `score()` filters + error parity).

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use arrow::json::{ArrayWriter, WriterBuilder};
use arrow::record_batch::RecordBatch;
use bytesize::ByteSize;
use cetra_column::Batch;
use cetra_core::enums::{ScoreSource, SpanKind, SpanStatus};
use cetra_core::ids::ProjectId;
use cetra_core::names::{environment, metadata_key};
use cetra_core::score::{ScoreTarget, ScoreValue};
use chrono::NaiveDate;
use serde_json::Value;
use uuid::Uuid;

use crate::catalog::FileTable;
use crate::column_bridge::column_to_record_batch;
use crate::config::{
    CompactionConfig, FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
};
use crate::deletion::{Deletion, DeletionScope};
use crate::query_error::RunError;
use crate::reads::{
    self as df, Agg, AlertMeasure, AlertValue, AlertValueRequest, Breakdown, Bucket, BucketWidth,
    ErrorSpansRequest, FacetData, FacetField, FacetView, FacetsRequest, FilterDeletionRequest,
    GroupBy, ListScope, MatchCountRequest, Measure, PromptMetricsRequest, PromptVersionMetrics,
    ScoreListRequest, ScoreNameCountsRequest, ScoreTargets, SessionExtrasRequest,
    SessionListRequest, SessionSort, SessionTargetFilter, Shape, SortOrder, SpanListRequest,
    SpanRefreshRequest, SpanSort, SpanTargetFilter, TraceListRequest, TraceRefreshRequest,
    TraceSort, TraceSpansRequest, UserExtrasRequest, UserListRequest, UserSort, WidgetData,
    WidgetError, WidgetQuery, WidgetRequest, WidgetSource,
};
use crate::reads_qe::{
    alerts, deletions, experiments, facets, prompts, scores, scores_join, sessions, targets,
    traces, users, widgets,
};
use crate::test_support::{score_row, score_rows, span_row, span_rows};
use crate::views::{QueryContext, TimeWindow, ViewName};
use crate::{QueryOptions, Storage};

const T0: i64 = 1_700_000_000_000_000_000;
const MIN: i64 = 60_000_000_000;
const HOUR: i64 = 60 * MIN;
const DAY: i64 = 24 * HOUR;
const TIMEOUT: Duration = Duration::from_secs(30);

fn pid() -> ProjectId {
    "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().expect("pid")
}

fn test_config(dir: &std::path::Path) -> StorageConfig {
    StorageConfig {
        delete_objects: true,
        data_dir: dir.to_path_buf(),
        min_free_bytes: ByteSize::b(0),
        object_store: None,
        s3: S3Config::default(),
        wal: WalConfig::default(),
        flush: FlushConfig::default(),
        memtable: MemtableConfig::default(),
        query: QueryConfig {
            target_partitions: 2,
            ..QueryConfig::default()
        },
        anchor: Default::default(),
        gc: Default::default(),
        compaction: CompactionConfig {
            l0_files: 2,
            ..CompactionConfig::default()
        },
        deletion: Default::default(),
    }
}

fn arrow_rows(batches: &[RecordBatch]) -> Vec<Value> {
    if batches.iter().all(|b| b.num_rows() == 0) {
        return Vec::new();
    }
    let mut buffer = Vec::new();
    let mut writer: ArrayWriter<&mut Vec<u8>> = WriterBuilder::new()
        .with_explicit_nulls(true)
        .with_timestamp_tz_format("%Y-%m-%dT%H:%M:%S%.9fZ".to_owned())
        .with_timestamp_format("%Y-%m-%dT%H:%M:%S%.9f".to_owned())
        .build(&mut buffer);
    for batch in batches {
        if batch.num_rows() > 0 {
            writer.write(batch).expect("encode");
        }
    }
    writer.finish().expect("finish");
    drop(writer);
    if buffer.is_empty() {
        return Vec::new();
    }
    serde_json::from_slice(&buffer).expect("rows")
}

fn qe_rows(batches: &[Batch]) -> Vec<Value> {
    let arrow: Vec<RecordBatch> = batches
        .iter()
        .filter(|b| b.num_rows() > 0)
        .map(|b| column_to_record_batch(b, None).expect("to arrow"))
        .collect();
    arrow_rows(&arrow)
}

fn values_match(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Number(l), Value::Number(r)) => match (l.as_i64(), r.as_i64()) {
            (Some(a), Some(b)) => a == b,
            _ => {
                let (Some(a), Some(b)) = (l.as_f64(), r.as_f64()) else {
                    return false;
                };
                (a - b).abs() < 1e-9 || (a - b).abs() <= b.abs() * 1e-9
            }
        },
        (Value::Array(l), Value::Array(r)) => {
            l.len() == r.len() && l.iter().zip(r).all(|(a, b)| values_match(a, b))
        }
        (Value::Object(l), Value::Object(r)) => {
            l.len() == r.len()
                && l.iter()
                    .all(|(k, v)| r.get(k).is_some_and(|rv| values_match(v, rv)))
        }
        _ => actual == expected,
    }
}

fn assert_batches_eq(qe: &[Batch], df: &[RecordBatch], context: &str) {
    let q = qe_rows(qe);
    let d = arrow_rows(df);
    assert_eq!(
        q.len(),
        d.len(),
        "{context}: row count mismatch qe={} df={}\nqe={q:#?}\ndf={d:#?}",
        q.len(),
        d.len()
    );
    for (i, (qr, dr)) in q.iter().zip(d.iter()).enumerate() {
        assert!(
            values_match(qr, dr),
            "{context}: row {i} mismatch\nqe={qr:#?}\ndf={dr:#?}"
        );
    }
}

fn assert_batches_unordered_eq(qe: &[Batch], df: &[RecordBatch], key: &str, context: &str) {
    let mut q = qe_rows(qe);
    let mut d = arrow_rows(df);
    let sort_fn = |a: &Value, b: &Value| {
        let ka = serde_json::to_string(&a[key]).unwrap_or_default();
        let kb = serde_json::to_string(&b[key]).unwrap_or_default();
        ka.cmp(&kb).then_with(|| {
            serde_json::to_string(a)
                .unwrap_or_default()
                .cmp(&serde_json::to_string(b).unwrap_or_default())
        })
    };
    q.sort_by(sort_fn);
    d.sort_by(sort_fn);
    assert_eq!(q.len(), d.len(), "{context}: row count mismatch");
    for (i, (qr, dr)) in q.iter().zip(d.iter()).enumerate() {
        assert!(
            values_match(qr, dr),
            "{context}: unordered row {i} mismatch\nqe={qr:#?}\ndf={dr:#?}"
        );
    }
}

fn assert_opt_f64_close(a: Option<f64>, b: Option<f64>, rel_tol: f64, ctx: &str) {
    match (a, b) {
        (None, None) => {}
        (Some(va), Some(vb)) => {
            let bound = (vb.abs() * rel_tol).max(1e-9);
            assert!(
                (va - vb).abs() <= bound,
                "{ctx}: f64 mismatch qe={va} df={vb} (bound={bound})"
            );
        }
        other => panic!("{ctx}: presence mismatch {other:?}"),
    }
}

fn assert_run_err_kind(qe: &RunError, df: &RunError, ctx: &str) {
    match (qe, df) {
        (RunError::InvalidRequest(a), RunError::InvalidRequest(b)) => {
            assert_eq!(a, b, "{ctx}: InvalidRequest message mismatch");
        }
        (RunError::Invalid(_), RunError::Invalid(_)) => {}
        (RunError::Failed(a), RunError::Failed(b)) => {
            assert_eq!(a, b, "{ctx}: Failed message mismatch");
        }
        (RunError::Timeout, RunError::Timeout) => {}
        (RunError::TooLarge, RunError::TooLarge) => {}
        other => panic!("{ctx}: RunError variant mismatch: {other:?}"),
    }
}

struct FixtureData {
    storage: Storage,
    exp_id: Uuid,
    score_ids: Vec<String>,
    t1_hex: String,
    t1_root_hex: String,
    t4_hex: String,
    deleted_tid: cetra_core::ids::TraceId,
}

async fn build_combined_fixture(dir: &std::path::Path) -> FixtureData {
    let storage = Storage::open(test_config(dir)).await.expect("open storage");
    let project = pid();
    let exp_id = Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0e01);
    let eval_id = Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0e02);

    // Group 1 (flushed -> L0 #1 -> compacted into L1):
    // Trace 1: clean, user u1, session s1, split across L1 + L0 + memtable, prompt v1
    let mut t1_root = span_row(1, 0, T0 + 10 * MIN, T0 + 10 * MIN);
    t1_root.name = "chat".to_owned();
    t1_root.end_time = t1_root.start_time + 20 * 1_000_000;
    t1_root.session_id = Some("s1".to_owned());
    t1_root.user_id = Some("u1".to_owned());
    t1_root.environment = Some("production".to_owned());
    t1_root.release = Some("v1.0".to_owned());
    t1_root.tags = vec!["alpha".to_owned(), "beta".to_owned()];
    t1_root.metadata = BTreeMap::from([
        ("team".to_owned(), "core".to_owned()),
        ("cetra.internal".to_owned(), "secret".to_owned()),
    ]);
    t1_root.input_preview = Some("hello world".to_owned());
    t1_root.output_preview = Some("hi there".to_owned());
    t1_root.input = Some("needle prompt one".to_owned());
    t1_root.cost_total = Some(0.15);
    t1_root.input_tokens = Some(100);
    t1_root.output_tokens = Some(50);
    t1_root.total_tokens = Some(150);

    let mut t1_llm = span_row(1, 1, T0 + 11 * MIN, T0 + 11 * MIN);
    t1_llm.parent_span_id = Some(t1_root.span_id);
    t1_llm.span_kind = SpanKind::Llm;
    t1_llm.end_time = t1_llm.start_time + 5 * 1_000_000;
    t1_llm.request_model = Some("gpt-4o".to_owned());
    t1_llm.prompt_name = Some("summarize".to_owned());
    t1_llm.prompt_version = Some(1);
    t1_llm.cost_total = Some(0.25);
    t1_llm.input_tokens = Some(200);
    t1_llm.output_tokens = Some(100);
    t1_llm.total_tokens = Some(300);

    // Trace 2: clean, user u2, session s1 (shared session without u1 on t2 -> user_detail whole-session check!),
    // error status, prompt v2
    let mut t2_root = span_row(2, 0, T0 + 20 * MIN, T0 + 20 * MIN);
    t2_root.name = "rag".to_owned();
    t2_root.end_time = t2_root.start_time + 40 * 1_000_000;
    t2_root.status = SpanStatus::Error;
    t2_root.session_id = Some("s1".to_owned());
    t2_root.user_id = Some("u2".to_owned());
    t2_root.environment = Some("staging".to_owned());
    t2_root.tags = vec!["beta".to_owned()];
    t2_root.input_preview = None; // NULL preview on latest trace in s1? No, t3 will be latest in s1!
    t2_root.output_preview = None;
    t2_root.cost_total = Some(0.50);
    t2_root.total_tokens = Some(500);

    let mut t2_llm = span_row(2, 1, T0 + 21 * MIN, T0 + 21 * MIN);
    t2_llm.parent_span_id = Some(t2_root.span_id);
    t2_llm.span_kind = SpanKind::Llm;
    t2_llm.status = SpanStatus::Error;
    t2_llm.end_time = t2_llm.start_time + 15 * 1_000_000;
    t2_llm.request_model = Some("claude-3".to_owned());
    t2_llm.prompt_name = Some("summarize".to_owned());
    t2_llm.prompt_version = Some(2);
    t2_llm.cost_total = Some(0.30);
    t2_llm.input_tokens = Some(300);
    t2_llm.output_tokens = Some(150);
    t2_llm.total_tokens = Some(450);

    // Trace 3: deleted via GDPR tombstone (lives in L1 + memtable)
    let mut t3_del = span_row(3, 0, T0 + 25 * MIN, T0 + 25 * MIN);
    t3_del.user_id = Some("u_deleted".to_owned());
    t3_del.session_id = Some("s_deleted".to_owned());
    t3_del.cost_total = Some(9.99);
    let deleted_tid = t3_del.trace_id;

    storage
        .append_spans(
            project,
            span_rows(&[
                t1_root.clone(),
                t1_llm.clone(),
                t2_root.clone(),
                t2_llm,
                t3_del,
            ]),
        )
        .await
        .expect("append g1 spans");

    let mut sc1 = score_row(
        1,
        1,
        T0 + 12 * MIN,
        T0 + 12 * MIN,
        T0 + 12 * MIN,
        false,
        "quality",
    );
    sc1.value = ScoreValue::Numeric(0.4);
    sc1.user_id = Some("u1".to_owned());
    let mut sc2 = score_row(
        2,
        2,
        T0 + 22 * MIN,
        T0 + 22 * MIN,
        T0 + 22 * MIN,
        false,
        "quality",
    );
    sc2.value = ScoreValue::Numeric(0.9);
    sc2.user_id = Some("u2".to_owned());
    let mut sc_ok = score_row(
        3,
        1,
        T0 + 12 * MIN,
        T0 + 12 * MIN,
        T0 + 12 * MIN,
        false,
        "ok",
    );
    sc_ok.value = ScoreValue::Boolean(true);
    let mut sc_issue = score_row(
        4,
        1,
        T0 + 13 * MIN,
        T0 + 13 * MIN,
        T0 + 13 * MIN,
        false,
        "issue_flag",
    );
    sc_issue.source = ScoreSource::Issue;
    sc_issue.value = ScoreValue::Numeric(1.0);
    let mut sc_lww_del = score_row(
        5,
        2,
        T0 + 22 * MIN,
        T0 + 22 * MIN,
        T0 + 22 * MIN,
        false,
        "ephemeral",
    );
    sc_lww_del.value = ScoreValue::Numeric(0.2);

    let sc1_id = sc1.score_id.to_string();
    let sc2_id = sc2.score_id.to_string();
    let sc_ok_id = sc_ok.score_id.to_string();
    let sc_del_uuid = sc_lww_del.score_id;

    storage
        .append_scores(
            project,
            score_rows(&[sc1.clone(), sc2, sc_ok, sc_issue, sc_lww_del.clone()]),
        )
        .await
        .expect("append g1 scores");
    storage.flush_now(TIMEOUT).await.expect("flush #1");

    // Group 2 (flushed -> L0 #2, then compacted with L0 #1 into L1!):
    // Trace 4: multi-day split trace (starts at T0 + 30*MIN, late span on Day 2!)
    let mut t4_root = span_row(4, 0, T0 + 30 * MIN, T0 + 30 * MIN);
    t4_root.name = "agent".to_owned();
    t4_root.end_time = t4_root.start_time + 5 * 1_000_000;
    t4_root.user_id = Some("u1".to_owned());
    t4_root.session_id = Some("s2".to_owned());
    t4_root.environment = Some("production".to_owned());
    t4_root.input_preview = None; // NULL preview on earliest trace in s2!
    t4_root.output_preview = Some("out4".to_owned());
    t4_root.cost_total = Some(0.10);

    // Clock-skew trace 5 (end_time < start_time -> negative duration_ns) in session s_skew
    let mut t5_skew = span_row(5, 0, T0 + 40 * MIN, T0 + 40 * MIN);
    t5_skew.end_time = t5_skew.start_time - 5_000;
    t5_skew.session_id = Some("s_skew".to_owned());
    t5_skew.user_id = Some("u3".to_owned());
    t5_skew.environment = Some("staging".to_owned());

    // Eval trace 6 targeting trace 1 (for eval_trace_ids + reserved env exclusion)
    let mut t6_eval = span_row(6, 0, T0 + 45 * MIN, T0 + 45 * MIN);
    t6_eval.environment = Some(environment::EVAL.to_owned());
    t6_eval.metadata.insert(
        metadata_key::EVAL_TARGET_TRACE_ID.to_owned(),
        t1_root.trace_id.to_hex(),
    );
    t6_eval.cost_total = Some(50.0);
    let mut t6_eval_child = span_row(6, 1, T0 + 46 * MIN, T0 + 46 * MIN);
    t6_eval_child.parent_span_id = Some(t6_eval.span_id);
    t6_eval_child.environment = None; // child without eval env -> tests strict_exclusions!

    // Experiment trace 7 (for experiment_* reads + default exclusion)
    let mut t7_exp = span_row(7, 0, T0 + 50 * MIN, T0 + 50 * MIN);
    t7_exp
        .metadata
        .insert(metadata_key::EXPERIMENT_ID.to_owned(), exp_id.to_string());
    t7_exp.cost_total = Some(0.05);

    storage
        .append_spans(
            project,
            span_rows(&[
                t4_root.clone(),
                t5_skew,
                t6_eval,
                t6_eval_child,
                t7_exp.clone(),
            ]),
        )
        .await
        .expect("append g2 spans");

    let mut sc_exp = score_row(
        6,
        7,
        T0 + 51 * MIN,
        T0 + 51 * MIN,
        T0 + 51 * MIN,
        false,
        "exp_acc",
    );
    sc_exp.experiment_id = Some(exp_id);
    sc_exp.evaluator_id = Some(eval_id);
    sc_exp.source = ScoreSource::LlmJudge;
    sc_exp.value = ScoreValue::Numeric(0.85);
    let sc_exp_id = sc_exp.score_id.to_string();

    let mut sc_sess = score_row(
        7,
        1,
        T0 + 15 * MIN,
        T0 + 15 * MIN,
        T0 + 15 * MIN,
        false,
        "sess_score",
    );
    sc_sess.target = ScoreTarget::Session {
        session_id: "s1".to_owned(),
    };
    sc_sess.value = ScoreValue::Categorical {
        label: "good".to_owned(),
        value: Some(0.75),
    };

    storage
        .append_scores(project, score_rows(&[sc_exp, sc_sess]))
        .await
        .expect("append g2 scores");
    storage.flush_now(TIMEOUT).await.expect("flush #2");

    // Compact L0 #1 + L0 #2 -> L1!
    let report = storage.compact(T0 + 100 * MIN).await.expect("compact");
    assert!(
        report.jobs > 0,
        "expected compaction to produce L1 file(s), got {report:?}"
    );

    // Group 3 (flushed -> fresh L0 Parquet alongside L1!):
    // Duplicate span copy of t1_root with higher ingest_time (tests span_list / span_detail dedup)
    let mut t1_root_dup = t1_root.clone();
    t1_root_dup.ingest_time = T0 + 60 * MIN;
    t1_root_dup.output_preview = Some("updated out".to_owned());

    // Early trace 8 starting 2h before T0 + 2*DAY, ending inside [T0+2*DAY, T0+3*DAY)
    let mut t8_early = span_row(8, 0, T0 + 2 * DAY - 2 * HOUR, T0 + 2 * DAY - 2 * HOUR);
    t8_early.end_time = T0 + 2 * DAY + 5 * MIN;
    t8_early.user_id = Some("u1".to_owned());
    t8_early.session_id = Some("s3".to_owned());
    t8_early.cost_total = Some(0.42);
    let mut t8_llm = span_row(8, 1, T0 + 2 * DAY + 2 * MIN, T0 + 2 * DAY + 2 * MIN);
    t8_llm.parent_span_id = Some(t8_early.span_id);
    t8_llm.span_kind = SpanKind::Llm;
    t8_llm.end_time = t8_llm.start_time + 5 * 1_000_000;
    t8_llm.prompt_name = Some("summarize".to_owned());
    t8_llm.prompt_version = Some(1);

    let mut extra_prompt_spans = vec![t1_root_dup, t8_early, t8_llm];
    for i in 0..100_u16 {
        let mut ps = span_row(200 + i, 1, T0 + 2 * DAY + 3 * MIN, T0 + 2 * DAY + 3 * MIN);
        ps.span_kind = SpanKind::Llm;
        ps.end_time = ps.start_time + (i64::from(i) + 1) * 1_000_000;
        ps.prompt_name = Some("summarize".to_owned());
        ps.prompt_version = Some(3);
        ps.user_id = Some("u1".to_owned());
        extra_prompt_spans.push(ps);
    }

    storage
        .append_spans(project, span_rows(&extra_prompt_spans))
        .await
        .expect("append g3 spans");
    storage.flush_now(TIMEOUT).await.expect("flush #3 (L0)");

    // Group 4 (unflushed -> live memtable!):
    // Late span for split trace 4 on Day 2 (far apart in time!)
    let mut t4_late = span_row(4, 1, T0 + 2 * DAY + 10 * MIN, T0 + 2 * DAY + 10 * MIN);
    t4_late.parent_span_id = Some(t4_root.span_id);
    t4_late.span_kind = SpanKind::Llm;
    t4_late.end_time = t4_late.start_time + 8 * 1_000_000;
    t4_late.session_id = Some("s2".to_owned());
    t4_late.user_id = Some("u1".to_owned());
    t4_late.input = Some("needle late child".to_owned());

    // Trace 9 in session s2 (NULL output_preview -> tests NULL-preserving FirstByOrder on latest trace!)
    let mut t9 = span_row(9, 0, T0 + 2 * DAY + 20 * MIN, T0 + 2 * DAY + 20 * MIN);
    t9.end_time = t9.start_time + 3 * 1_000_000;
    t9.session_id = Some("s2".to_owned());
    t9.user_id = Some("u1".to_owned());
    t9.input_preview = Some("in9".to_owned());
    t9.output_preview = None;
    t9.cost_total = Some(0.08);

    storage
        .append_spans(project, span_rows(&[t4_late, t9]))
        .await
        .expect("append memtable spans");

    // LWW override on sc1 (0.4 -> 0.95) and tombstone delete on sc_lww_del
    let mut sc1_v2 = sc1;
    sc1_v2.updated_at = T0 + 70 * MIN;
    sc1_v2.ingest_time = T0 + 70 * MIN;
    sc1_v2.value = ScoreValue::Numeric(0.95);
    let mut sc_del_tomb = sc_lww_del;
    sc_del_tomb.score_id = sc_del_uuid;
    sc_del_tomb.deleted = true;
    sc_del_tomb.updated_at = T0 + 75 * MIN;
    sc_del_tomb.ingest_time = T0 + 75 * MIN;

    storage
        .append_scores(project, score_rows(&[sc1_v2, sc_del_tomb]))
        .await
        .expect("append memtable scores");

    // Register GDPR deletion for trace 3
    storage
        .register_deletion(Deletion {
            id: Uuid::now_v7(),
            project,
            cutoff: T0 + 100 * MIN,
            scope: DeletionScope::Traces {
                trace_ids: vec![deleted_tid],
            },
        })
        .await
        .expect("register deletion");

    // Verify we really have L1 + L0 + memtable in the catalog/storage.
    let state = storage.published.load();
    let days = NaiveDate::MIN..=NaiveDate::MAX;
    let has_l1 = state
        .catalog
        .files(FileTable::Spans, project, days.clone())
        .any(|f| f.level > 0);
    let has_l0 = state
        .catalog
        .files(FileTable::Spans, project, days)
        .any(|f| f.level == 0);
    assert!(
        has_l1 && has_l0,
        "fixture must contain both L1 and L0 files"
    );

    FixtureData {
        storage,
        exp_id,
        score_ids: vec![sc1_id, sc2_id, sc_ok_id, sc_exp_id, sc_del_uuid.to_string()],
        t1_hex: t1_root.trace_id.to_hex(),
        t1_root_hex: t1_root.span_id.to_hex(),
        t4_hex: t4_root.trace_id.to_hex(),
        deleted_tid,
    }
}

#[tokio::test]
async fn all_39_read_functions_match_datafusion_oracle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fx = Box::pin(build_combined_fixture(dir.path())).await;
    let project = pid();

    let opts = QueryOptions::new(None, None);
    let qe_ctx = fx.storage.qe_query(project, opts).await.expect("qe admit");
    let df_ctx = QueryContext::new(fx.storage.query(project, opts).await.expect("df"), None)
        .expect("df ctx");

    let opts_vf = QueryOptions::new(Some(T0 + 30 * MIN), Some(T0 + 15 * MIN));
    let qe_vf = fx.storage.qe_query(project, opts_vf).await.expect("qe vf");
    let df_vf = QueryContext::new(
        fx.storage.query(project, opts_vf).await.expect("df vf"),
        None,
    )
    .expect("df vf ctx");

    let full_win = TimeWindow::new(T0, T0 + 3 * DAY).expect("full win");
    let day2_win = TimeWindow::new(T0 + 2 * DAY, T0 + 3 * DAY).expect("day2 win");

    // 1. validate_filter
    for (view, text) in [
        (ViewName::Traces, ""),
        (ViewName::Traces, "error_count > 0"),
        (ViewName::Spans, "span_kind = 'llm'"),
        (ViewName::Sessions, "trace_count >= 1"),
        (ViewName::Scores, "name = 'quality'"),
        (ViewName::Traces, "1 + 2"),
        (ViewName::Traces, "bad_col = 1"),
        (ViewName::ExperimentItems, "name = 'x'"),
    ] {
        let q = scores_join::validate_filter(view, text);
        let d = df::validate_filter(&df_ctx, view, text);
        match (&q, &d) {
            (Ok(()), Ok(())) => {}
            (Err(qe), Err(de)) => assert_run_err_kind(qe, de, "validate_filter"),
            other => panic!("validate_filter({view:?}, {text:?}): {other:?}"),
        }
    }

    // 2. trace_list & 3. trace_list_plan
    let default_req = TraceListRequest {
        filter: None,
        search: None,
        strict_exclusions: false,
        scope: ListScope {
            window: None,
            include_eval: false,
            include_experiments: false,
        },
        sort: TraceSort::StartTime,
        order: SortOrder::Desc,
        limit: 10,
        cursor: None,
        timeout: TIMEOUT,
    };
    let plan_list = traces::trace_list_plan(&qe_ctx, default_req.clone())
        .await
        .expect("qe plan");
    let df_plan_list = df::trace_list_plan(&df_ctx, default_req.clone())
        .await
        .expect("df plan");
    assert!(plan_list.contains("mode=list"), "got {plan_list}");
    assert!(df_plan_list.contains("mode=list"), "got {df_plan_list}");

    for sort in [
        TraceSort::StartTime,
        TraceSort::Duration,
        TraceSort::CostTotal,
        TraceSort::TotalTokens,
    ] {
        for order in [SortOrder::Asc, SortOrder::Desc] {
            let req = TraceListRequest {
                filter: Some("score('quality') > 0.5 OR status = 'error'"),
                search: None,
                strict_exclusions: false,
                scope: ListScope {
                    window: Some(full_win),
                    include_eval: false,
                    include_experiments: false,
                },
                sort,
                order,
                limit: 2,
                cursor: None,
                timeout: TIMEOUT,
            };
            let qp1 = traces::trace_list(&qe_ctx, req.clone()).await.expect("qp1");
            let dp1 = df::trace_list(&df_ctx, req.clone()).await.expect("dp1");
            assert_batches_eq(&qp1.batches, &dp1.batches, "trace_list page 1");
            assert_eq!(qp1.next_cursor, dp1.next_cursor, "trace_list cursor");
            if let Some(cur) = qp1.next_cursor.as_deref() {
                let req2 = TraceListRequest {
                    cursor: Some(cur),
                    ..req
                };
                let qp2 = traces::trace_list(&qe_ctx, req2.clone())
                    .await
                    .expect("qp2");
                let dp2 = df::trace_list(&df_ctx, req2).await.expect("dp2");
                assert_batches_eq(&qp2.batches, &dp2.batches, "trace_list page 2");
                assert_eq!(qp2.next_cursor, dp2.next_cursor);
            }
        }
    }
    // trace_list with search + visible_from + error parity
    let search_req = TraceListRequest {
        search: Some("needle"),
        scope: ListScope {
            window: Some(full_win),
            include_eval: false,
            include_experiments: false,
        },
        ..default_req.clone()
    };
    let qp_s = traces::trace_list(&qe_vf, search_req.clone())
        .await
        .expect("qe search");
    let dp_s = df::trace_list(&df_vf, search_req).await.expect("df search");
    assert_batches_eq(&qp_s.batches, &dp_s.batches, "trace_list search+vf");
    let bad_cur_req = TraceListRequest {
        sort: TraceSort::Duration,
        cursor: Some("not-hex"),
        ..default_req
    };
    assert_run_err_kind(
        &traces::trace_list(&qe_ctx, bad_cur_req.clone())
            .await
            .expect_err("qe bad cur"),
        &df::trace_list(&df_ctx, bad_cur_req)
            .await
            .expect_err("df bad cur"),
        "trace_list bad cursor",
    );

    // 4. span_list
    for sort in [
        SpanSort::StartTime,
        SpanSort::Duration,
        SpanSort::CostTotal,
        SpanSort::TotalTokens,
    ] {
        let req = SpanListRequest {
            filter: Some("score('quality') > 0 OR span_kind = 'llm'"),
            search: Some("needle"),
            strict_exclusions: true,
            scope: ListScope {
                window: Some(full_win),
                include_eval: false,
                include_experiments: false,
            },
            sort,
            order: SortOrder::Desc,
            limit: 2,
            cursor: None,
            timeout: TIMEOUT,
        };
        let qp1 = traces::span_list(&qe_ctx, req.clone())
            .await
            .expect("qe span");
        let dp1 = df::span_list(&df_ctx, req.clone()).await.expect("df span");
        assert_batches_eq(&qp1.batches, &dp1.batches, "span_list");
        assert_eq!(qp1.next_cursor, dp1.next_cursor);
    }

    // 5. trace_detail (including split trace across L1+memtable with around=None, and tombstoned -> None)
    for (tid_hex, label) in [
        (fx.t4_hex.as_str(), "split trace t4"),
        (fx.t1_hex.as_str(), "dup trace t1"),
        (&fx.deleted_tid.to_hex(), "tombstoned t3"),
    ] {
        let qd = traces::trace_detail(&qe_ctx, tid_hex, TIMEOUT)
            .await
            .expect("qe detail");
        let dd = df::trace_detail(&df_ctx, tid_hex, TIMEOUT)
            .await
            .expect("df detail");
        match (qd, dd) {
            (None, None) => {}
            (Some(q), Some(d)) => {
                assert_batches_eq(&[q.trace], &[d.trace], label);
                assert_batches_eq(&q.spans, &d.spans, label);
                assert_eq!(q.truncated, d.truncated, "{label}: truncated");
            }
            other => panic!("{label}: detail mismatch {other:?}"),
        }
    }

    // 6. span_detail (duplicate copy LWW by ingest_time)
    let qs = traces::span_detail(&qe_ctx, &fx.t1_hex, &fx.t1_root_hex, TIMEOUT)
        .await
        .expect("qe span_detail")
        .expect("some");
    let ds = df::span_detail(&df_ctx, &fx.t1_hex, &fx.t1_root_hex, TIMEOUT)
        .await
        .expect("df span_detail")
        .expect("some");
    assert_batches_eq(&[qs], &[ds], "span_detail");

    // 7. trace_spans
    let ts_req = TraceSpansRequest {
        trace_id: &fx.t4_hex,
        target_span_id: None,
        max_spans: 1,
        max_required: 2,
        timeout: TIMEOUT,
    };
    let qts = traces::trace_spans(&qe_ctx, ts_req)
        .await
        .expect("qe trace_spans");
    let dts = df::trace_spans(&df_ctx, ts_req)
        .await
        .expect("df trace_spans");
    assert_batches_eq(&qts.required, &dts.required, "trace_spans required");
    assert_batches_eq(&qts.earliest, &dts.earliest, "trace_spans earliest");
    assert_eq!(qts.truncated, dts.truncated);

    // 8. trace_refresh
    let tr_req = TraceRefreshRequest {
        trace_ids: &[&fx.t1_hex, &fx.t4_hex, &fx.deleted_tid.to_hex()],
        filter: Some("status = 'ok' OR status = 'unset'"),
        search: None,
        include_eval: false,
        include_experiments: false,
        strict_exclusions: true,
        timeout: TIMEOUT,
    };
    assert_batches_unordered_eq(
        &traces::trace_refresh(&qe_ctx, tr_req)
            .await
            .expect("qe trace_refresh"),
        &df::trace_refresh(&df_ctx, tr_req)
            .await
            .expect("df trace_refresh"),
        "trace_id",
        "trace_refresh",
    );

    // 9. span_refresh
    let sr_req = SpanRefreshRequest {
        trace_ids: &[&fx.t1_hex, &fx.t4_hex],
        filter: None,
        per_trace: 2,
        strict_exclusions: true,
        timeout: TIMEOUT,
    };
    assert_batches_eq(
        &traces::span_refresh(&qe_ctx, sr_req)
            .await
            .expect("qe span_refresh"),
        &df::span_refresh(&df_ctx, sr_req)
            .await
            .expect("df span_refresh"),
        "span_refresh",
    );

    // 10. error_spans
    let es_req = ErrorSpansRequest {
        filter: None,
        trace_ids: &[&fx.t1_hex, &fx.t4_hex],
        timeout: TIMEOUT,
    };
    assert_batches_eq(
        &traces::error_spans(&qe_ctx, es_req)
            .await
            .expect("qe error_spans"),
        &df::error_spans(&df_ctx, es_req)
            .await
            .expect("df error_spans"),
        "error_spans",
    );

    // 11. match_count
    let mc_req = MatchCountRequest {
        view: ViewName::Traces,
        filter: Some("score('quality') > 0.5"),
        strict_exclusions: false,
        window: full_win,
        cap: 100,
        timeout: TIMEOUT,
    };
    assert_eq!(
        traces::match_count(&qe_ctx, mc_req)
            .await
            .expect("qe match_count"),
        df::match_count(&df_ctx, mc_req)
            .await
            .expect("df match_count")
    );

    // 12. score_list
    let sl_req = ScoreListRequest {
        target: None,
        trace_id: None,
        span_id: None,
        session_id: None,
        name: Some("quality"),
        source: None,
        evaluator_id: None,
        rule_id: None,
        queue_id: None,
        window: Some(full_win),
        limit: 10,
        cursor: None,
        timeout: TIMEOUT,
    };
    let qsl = scores::score_list(&qe_ctx, sl_req.clone())
        .await
        .expect("qe score_list");
    let dsl = df::score_list(&df_ctx, sl_req)
        .await
        .expect("df score_list");
    assert_batches_eq(&qsl.batches, &dsl.batches, "score_list");
    assert_eq!(qsl.next_cursor, dsl.next_cursor);

    // 13. latest_scores (verifies source != 'issue')
    let ls_q = scores::latest_scores(
        &qe_ctx,
        ScoreTargets::Traces(&[&fx.t1_hex, &fx.t4_hex]),
        TIMEOUT,
    )
    .await
    .expect("qe latest_scores");
    let ls_d = df::latest_scores(
        &df_ctx,
        ScoreTargets::Traces(&[&fx.t1_hex, &fx.t4_hex]),
        TIMEOUT,
    )
    .await
    .expect("df latest_scores");
    assert_batches_unordered_eq(&ls_q, &ls_d, "score_id", "latest_scores traces");

    // 14. scores_by_id
    let id_refs: Vec<&str> = fx.score_ids.iter().map(String::as_str).collect();
    assert_batches_unordered_eq(
        &scores::scores_by_id(&qe_ctx, &id_refs, TIMEOUT)
            .await
            .expect("qe scores_by_id"),
        &df::scores_by_id(&df_ctx, &id_refs, TIMEOUT)
            .await
            .expect("df scores_by_id"),
        "score_id",
        "scores_by_id",
    );

    // 15. score_name_counts
    let snc_names = vec![
        "quality".to_owned(),
        "ok".to_owned(),
        "sess_score".to_owned(),
    ];
    let snc_req = ScoreNameCountsRequest {
        names: &snc_names,
        window: full_win,
        by_day: true,
        timeout: TIMEOUT,
    };
    assert_eq!(
        scores::score_name_counts(&qe_ctx, snc_req)
            .await
            .expect("qe snc"),
        df::score_name_counts(&df_ctx, snc_req)
            .await
            .expect("df snc")
    );

    // 16. session_list (includes negative clock-skew duration_ns + score() filter!)
    for sort in [
        SessionSort::StartTime,
        SessionSort::EndTime,
        SessionSort::TraceCount,
        SessionSort::CostTotal,
    ] {
        let req = SessionListRequest {
            filter: None,
            scope: ListScope {
                window: Some(full_win),
                include_eval: false,
                include_experiments: false,
            },
            sort,
            order: SortOrder::Desc,
            limit: 10,
            cursor: None,
            timeout: TIMEOUT,
        };
        let qsl = sessions::session_list(&qe_ctx, req.clone())
            .await
            .expect("qe session_list");
        let dsl = df::session_list(&df_ctx, req)
            .await
            .expect("df session_list");
        assert_batches_eq(&qsl.batches, &dsl.batches, "session_list");
    }

    // 17. session_detail
    let qsd = sessions::session_detail(&qe_ctx, "s2", false, false, TIMEOUT)
        .await
        .expect("qe session_detail")
        .expect("some");
    let dsd = df::session_detail(&df_ctx, "s2", false, false, TIMEOUT)
        .await
        .expect("df session_detail")
        .expect("some");
    assert_batches_eq(&[qsd.session], &[dsd.session], "session_detail row");
    assert_batches_eq(&qsd.turns, &dsd.turns, "session_detail turns");
    assert_batches_unordered_eq(&qsd.roots, &dsd.roots, "trace_id", "session_detail roots");
    assert_eq!(qsd.truncated, dsd.truncated);

    // 18. session_extras (NULL preview preservation on winning earliest/latest trace)
    let se_req = SessionExtrasRequest {
        session_ids: &["s1", "s2", "s_skew"],
        scope: ListScope {
            window: None,
            include_eval: false,
            include_experiments: false,
        },
        timeout: TIMEOUT,
    };
    assert_batches_unordered_eq(
        &sessions::session_extras(&qe_ctx, se_req.clone())
            .await
            .expect("qe session_extras"),
        &df::session_extras(&df_ctx, se_req)
            .await
            .expect("df session_extras"),
        "session_id",
        "session_extras",
    );

    // 19. user_list
    let ul_req = UserListRequest {
        filter: Some("score('quality') > 0"),
        scope: ListScope {
            window: Some(full_win),
            include_eval: false,
            include_experiments: false,
        },
        sort: UserSort::LastSeen,
        order: SortOrder::Desc,
        limit: 10,
        cursor: None,
        timeout: TIMEOUT,
    };
    let qul = users::user_list(&qe_ctx, ul_req.clone())
        .await
        .expect("qe user_list");
    let dul = df::user_list(&df_ctx, ul_req).await.expect("df user_list");
    assert_batches_eq(&qul.batches, &dul.batches, "user_list");

    // 20. user_detail
    let user_detail_scope = ListScope {
        window: None,
        include_eval: false,
        include_experiments: false,
    };
    let qud = users::user_detail(&qe_ctx, "u1", user_detail_scope, TIMEOUT)
        .await
        .expect("qe user_detail")
        .expect("some");
    let dud = df::user_detail(&df_ctx, "u1", user_detail_scope, TIMEOUT)
        .await
        .expect("df user_detail")
        .expect("some");
    assert_batches_eq(&[qud.user], &[dud.user], "user_detail user");
    assert_batches_eq(&qud.sessions, &dud.sessions, "user_detail sessions");

    // 21. user_extras
    let ue_req = UserExtrasRequest {
        user_ids: &["u1", "u2"],
        activity: day2_win,
        filter: None,
        scope: ListScope {
            window: Some(full_win),
            include_eval: false,
            include_experiments: false,
        },
        timeout: TIMEOUT,
    };
    let que = users::user_extras(&qe_ctx, ue_req.clone())
        .await
        .expect("qe user_extras");
    let due = df::user_extras(&df_ctx, ue_req)
        .await
        .expect("df user_extras");
    assert_batches_unordered_eq(
        &que.activity,
        &due.activity,
        "user_id",
        "user_extras activity",
    );
    assert_batches_unordered_eq(&que.scores, &due.scores, "user_id", "user_extras scores");
    assert_batches_unordered_eq(
        &que.metadata,
        &due.metadata,
        "user_id",
        "user_extras metadata",
    );

    // 22. trace_targets, 23. span_targets, 24. session_targets,
    // 25. filter_span_targets, 26. filter_session_targets
    let t1_id: cetra_core::ids::TraceId = fx.t1_hex.parse().expect("t1");
    let t4_id: cetra_core::ids::TraceId = fx.t4_hex.parse().expect("t4");
    let s1_id: cetra_core::ids::SpanId = fx.t1_root_hex.parse().expect("s1");
    assert_eq!(
        targets::trace_targets(&qe_ctx, &[t1_id, t4_id, fx.deleted_tid], TIMEOUT)
            .await
            .expect("qe trace_targets"),
        df::trace_targets(&df_ctx, &[t1_id, t4_id, fx.deleted_tid], TIMEOUT)
            .await
            .expect("df trace_targets"),
    );
    assert_eq!(
        targets::span_targets(&qe_ctx, &[(t1_id, s1_id)], TIMEOUT)
            .await
            .expect("qe span_targets"),
        df::span_targets(&df_ctx, &[(t1_id, s1_id)], TIMEOUT)
            .await
            .expect("df span_targets"),
    );
    assert_eq!(
        targets::session_targets(&qe_ctx, &["s1", "s2", "s_deleted"], TIMEOUT)
            .await
            .expect("qe session_targets"),
        df::session_targets(&df_ctx, &["s1", "s2", "s_deleted"], TIMEOUT)
            .await
            .expect("df session_targets"),
    );
    let fst_req = SpanTargetFilter {
        window: full_win,
        filter: Some("environment = 'cetra-eval' OR span_kind = 'llm'"),
        search: None,
        include_experiments: false,
        limit: 200,
        timeout: TIMEOUT,
    };
    assert_eq!(
        targets::filter_span_targets(&qe_ctx, fst_req)
            .await
            .expect("qe filter_span_targets"),
        df::filter_span_targets(&df_ctx, fst_req)
            .await
            .expect("df filter_span_targets"),
    );
    let fst_capped = SpanTargetFilter {
        limit: 2,
        ..fst_req
    };
    let q_cap = targets::filter_span_targets(&qe_ctx, fst_capped)
        .await
        .expect("qe capped");
    let d_cap = df::filter_span_targets(&df_ctx, fst_capped)
        .await
        .expect("df capped");
    assert!(q_cap.capped && d_cap.capped);
    assert_eq!(q_cap.targets.len(), d_cap.targets.len());
    let fss_req = SessionTargetFilter {
        window: full_win,
        filter: Some("trace_count >= 1"),
        include_experiments: false,
        limit: 100,
        timeout: TIMEOUT,
    };
    assert_eq!(
        targets::filter_session_targets(&qe_ctx, fss_req)
            .await
            .expect("qe filter_session_targets"),
        df::filter_session_targets(&df_ctx, fss_req)
            .await
            .expect("df filter_session_targets"),
    );

    // 27. filter_trace_ids, 28. user_deletion_scope, 29. eval_trace_ids, 30. session_ids_of_traces
    let fti_req = FilterDeletionRequest {
        window: full_win,
        filter: Some("environment = 'production'"),
        search: None,
        include_eval: false,
        include_experiments: false,
        limit: 100,
        timeout: TIMEOUT,
    };
    assert_eq!(
        deletions::filter_trace_ids(&qe_ctx, fti_req)
            .await
            .expect("qe filter_trace_ids"),
        df::filter_trace_ids(&df_ctx, fti_req)
            .await
            .expect("df filter_trace_ids"),
    );
    assert_eq!(
        deletions::user_deletion_scope(&qe_ctx, "u1", T0 + 10 * DAY, TIMEOUT)
            .await
            .expect("qe user_deletion_scope"),
        df::user_deletion_scope(&df_ctx, "u1", T0 + 10 * DAY, TIMEOUT)
            .await
            .expect("df user_deletion_scope"),
    );
    let target_set: HashSet<_> = [t1_id].into_iter().collect();
    assert_eq!(
        deletions::eval_trace_ids(&qe_ctx, &target_set, TIMEOUT)
            .await
            .expect("qe eval_trace_ids"),
        df::eval_trace_ids(&df_ctx, &target_set, TIMEOUT)
            .await
            .expect("df eval_trace_ids"),
    );
    let trace_set: HashSet<_> = [t1_id, t4_id].into_iter().collect();
    assert_eq!(
        deletions::session_ids_of_traces(&qe_ctx, &trace_set, TIMEOUT)
            .await
            .expect("qe session_ids_of_traces"),
        df::session_ids_of_traces(&df_ctx, &trace_set, TIMEOUT)
            .await
            .expect("df session_ids_of_traces"),
    );

    // 31. prompt_metrics
    let pm_req = PromptMetricsRequest {
        prompt: "summarize",
        versions: &[],
        window: full_win,
        timeout: TIMEOUT,
    };
    let qpm: Vec<PromptVersionMetrics> = prompts::prompt_metrics(&qe_ctx, pm_req.clone())
        .await
        .expect("qe prompt_metrics");
    let dpm: Vec<PromptVersionMetrics> = df::prompt_metrics(&df_ctx, pm_req)
        .await
        .expect("df prompt_metrics");
    assert_eq!(qpm.len(), dpm.len(), "prompt_metrics len");
    for (q, d) in qpm.iter().zip(dpm.iter()) {
        assert_eq!(q.version, d.version);
        assert_eq!(q.span_count, d.span_count);
        assert_eq!(q.error_count, d.error_count);
        assert_opt_f64_close(
            q.input_tokens_avg,
            d.input_tokens_avg,
            1e-9,
            "prompt input_tokens_avg",
        );
        assert_opt_f64_close(
            q.output_tokens_avg,
            d.output_tokens_avg,
            1e-9,
            "prompt output_tokens_avg",
        );
        assert!((q.cost_total - d.cost_total).abs() <= 1e-9);
        assert_eq!(q.scores, d.scores);
        let check_lat = |a: Option<i64>, b: Option<i64>| match (a, b) {
            (None, None) => {}
            (Some(va), Some(vb)) => {
                let bound = ((vb as f64).abs() * 0.01).max(1.0);
                assert!(((va - vb) as f64).abs() <= bound, "p: {va} vs {vb}");
            }
            other => panic!("lat presence mismatch {other:?}"),
        };
        check_lat(q.latency_p50_ns, d.latency_p50_ns);
        check_lat(q.latency_p95_ns, d.latency_p95_ns);
    }

    // 32. experiment_scores, 33. experiment_traces, 34. experiment_score_means
    let exp_str = fx.exp_id.to_string();
    assert_eq!(
        experiments::experiment_scores(&qe_ctx, &exp_str, T0, TIMEOUT)
            .await
            .expect("qe exp_scores"),
        df::experiment_scores(&df_ctx, &exp_str, T0, TIMEOUT)
            .await
            .expect("df exp_scores"),
    );
    assert_eq!(
        experiments::experiment_traces(&qe_ctx, &exp_str, T0, TIMEOUT)
            .await
            .expect("qe exp_traces"),
        df::experiment_traces(&df_ctx, &exp_str, T0, TIMEOUT)
            .await
            .expect("df exp_traces"),
    );
    let exp_list = vec![exp_str];
    assert_eq!(
        experiments::experiment_score_means(&qe_ctx, &exp_list, T0, TIMEOUT)
            .await
            .expect("qe exp_means"),
        df::experiment_score_means(&df_ctx, &exp_list, T0, TIMEOUT)
            .await
            .expect("df exp_means"),
    );

    // 35. facets
    let facet_fields = [
        FacetField {
            field: "environment",
            query: None,
            limit: 10,
        },
        FacetField {
            field: "tags",
            query: None,
            limit: 10,
        },
        FacetField {
            field: "metadata",
            query: None,
            limit: 10,
        },
        FacetField {
            field: "metadata['team']",
            query: None,
            limit: 10,
        },
        FacetField {
            field: "duration_ms",
            query: None,
            limit: 10,
        },
        FacetField {
            field: "bad_field",
            query: None,
            limit: 10,
        },
    ];
    let f_req = FacetsRequest {
        view: FacetView::Traces,
        filter: Some("score('quality') > 0"),
        search: None,
        scope: ListScope {
            window: Some(full_win),
            include_eval: false,
            include_experiments: false,
        },
        fields: &facet_fields,
        timeout: TIMEOUT,
    };
    let qf: Vec<Result<FacetData, RunError>> =
        facets::facets(&qe_ctx, f_req).await.expect("qe facets");
    let df_res: Vec<Result<FacetData, RunError>> =
        df::facets(&df_ctx, f_req).await.expect("df facets");
    assert_eq!(qf.len(), df_res.len());
    for (q, d) in qf.iter().zip(df_res.iter()) {
        match (q, d) {
            (Ok(qd), Ok(dd)) => {
                assert_eq!(qd.kind, dd.kind);
                assert_eq!(qd.total, dd.total);
                assert_eq!(qd.missing, dd.missing);
                assert_eq!(qd.values, dd.values);
                assert_eq!(qd.more, dd.more);
                if let (Some(qh), Some(dh)) = (&qd.histogram, &dd.histogram) {
                    assert_opt_f64_close(Some(qh.min), Some(dh.min), 1e-9, "hist min");
                    assert_opt_f64_close(Some(qh.max), Some(dh.max), 1e-9, "hist max");
                    assert_opt_f64_close(Some(qh.p50), Some(dh.p50), 0.01, "hist p50");
                    assert_opt_f64_close(Some(qh.p95), Some(dh.p95), 0.01, "hist p95");
                    assert_opt_f64_close(Some(qh.p99), Some(dh.p99), 0.01, "hist p99");
                }
            }
            (Err(qe), Err(de)) => assert_run_err_kind(qe, de, "facets slot"),
            other => panic!("facets slot mismatch: {other:?}"),
        }
    }

    // 36. validate_shape & 37. validate_expressions & 38. widget_query
    let wq_scores = WidgetQuery {
        source: WidgetSource::Scores,
        filter: None,
        measures: vec![Measure {
            agg: Agg::CountDistinct,
            field: Some("trace_id".to_owned()),
            condition: None,
        }],
        breakdown: None,
    };
    assert!(matches!(
        (
            widgets::validate_shape(&wq_scores),
            df::validate_shape(&wq_scores),
        ),
        (Ok(()), Ok(()))
    ));
    assert!(matches!(
        (
            widgets::validate_expressions(&wq_scores, Some("score('quality') > 0")),
            df::validate_expressions(&df_ctx, &wq_scores, Some("score('quality') > 0")),
        ),
        (Ok(()), Ok(()))
    ));
    let bad_wq = WidgetQuery {
        source: WidgetSource::Traces,
        filter: None,
        measures: vec![Measure {
            agg: Agg::Sum,
            field: None,
            condition: None,
        }],
        breakdown: None,
    };
    match (
        widgets::validate_shape(&bad_wq),
        df::validate_shape(&bad_wq),
    ) {
        (Err(WidgetError::Fields(qf)), Err(WidgetError::Fields(df))) => assert_eq!(qf, df),
        other => panic!("validate_shape mismatch: {other:?}"),
    }
    assert!(matches!(
        (
            widgets::validate_expressions(&wq_scores, Some("unknown_col = 1")),
            df::validate_expressions(&df_ctx, &wq_scores, Some("unknown_col = 1")),
        ),
        (
            Err(WidgetError::Expression { path: qp, .. }),
            Err(WidgetError::Expression { path: dp, .. }),
        ) if qp == dp
    ));

    let w_req_scores = WidgetRequest {
        window: full_win,
        bucket: Bucket::Fixed(BucketWidth::D1),
        dashboard_filter: Some("score('quality') > 0.5"),
        group_by: None,
        include_eval: false,
        include_experiments: false,
        query: &wq_scores,
        shape: Shape::Total,
        compare: true,
        timeout: TIMEOUT,
    };
    let qw_sc = widgets::widget_query(&qe_ctx, w_req_scores)
        .await
        .expect("qe widget scores");
    let dw_sc = df::widget_query(&df_ctx, w_req_scores)
        .await
        .expect("df widget scores");
    assert_eq!(
        qw_sc, dw_sc,
        "widget_query Scores + CountDistinct(trace_id)"
    );

    let wq_traces = WidgetQuery {
        source: WidgetSource::Traces,
        filter: None,
        measures: vec![
            Measure {
                agg: Agg::Count,
                field: None,
                condition: None,
            },
            Measure {
                agg: Agg::Sum,
                field: Some("cost_total".to_owned()),
                condition: None,
            },
            Measure {
                agg: Agg::P95,
                field: Some("duration_ns".to_owned()),
                condition: None,
            },
        ],
        breakdown: Some(Breakdown {
            field: "tags".to_owned(),
            limit: 1,
            other: true,
        }),
    };
    let w_req_series = WidgetRequest {
        window: full_win,
        bucket: Bucket::Fixed(BucketWidth::D1),
        dashboard_filter: None,
        group_by: Some(GroupBy::Environment),
        include_eval: false,
        include_experiments: false,
        query: &wq_traces,
        shape: Shape::Series,
        compare: false,
        timeout: TIMEOUT,
    };
    match (
        widgets::widget_query(&qe_ctx, w_req_series)
            .await
            .expect("qe series"),
        df::widget_query(&df_ctx, w_req_series)
            .await
            .expect("df series"),
    ) {
        (
            WidgetData::Series {
                bucket_ns: qb,
                series: qs,
            },
            WidgetData::Series {
                bucket_ns: db,
                series: ds,
            },
        ) => {
            assert_eq!(qb, db);
            assert_eq!(qs.len(), ds.len());
            for (q, d) in qs.iter().zip(ds.iter()) {
                assert_eq!(q.measure, d.measure);
                assert_eq!(q.group, d.group);
                assert_eq!(q.other, d.other);
                assert_eq!(q.points.len(), d.points.len());
                for (qp, dp) in q.points.iter().zip(d.points.iter()) {
                    assert_eq!(qp.time, dp.time);
                    assert_eq!(qp.rows, dp.rows);
                    assert_opt_f64_close(qp.value, dp.value, 0.01, "widget series point");
                }
            }
        }
        other => panic!("widget series mismatch: {other:?}"),
    }

    // 39. alert_value (all 7 measures including score() filters on LlmLatencyP95/ScoreAvg/ScorePassRate)
    for (measure, filter, tol) in [
        (AlertMeasure::ErrorRate, None, 1e-9),
        (AlertMeasure::Cost, Some("environment = 'production'"), 1e-9),
        (AlertMeasure::TraceLatencyP95, None, 0.01),
        (
            AlertMeasure::LlmLatencyP95,
            Some("score('quality') > 0.5"),
            0.01,
        ),
        (
            AlertMeasure::ScoreAvg {
                name: "quality".to_owned(),
            },
            Some("score('ok') = 1"),
            1e-9,
        ),
        (
            AlertMeasure::ScorePassRate {
                name: "ok".to_owned(),
            },
            Some("score('quality') > 0.5"),
            1e-9,
        ),
        (
            AlertMeasure::ScoreCount {
                names: vec!["quality".to_owned(), "sess_score".to_owned()],
            },
            None,
            1e-9,
        ),
    ] {
        let req = AlertValueRequest {
            window: full_win,
            filter,
            measure,
            timeout: TIMEOUT,
        };
        let qv: AlertValue = alerts::alert_value(&qe_ctx, req.clone())
            .await
            .expect("qe alert");
        let dv: AlertValue = df::alert_value(&df_ctx, req).await.expect("df alert");
        assert_eq!(qv.count, dv.count);
        assert_opt_f64_close(qv.value, dv.value, tol, "alert_value");
    }

    drop(qe_ctx);
    drop(df_ctx);
    drop(qe_vf);
    drop(df_vf);
    let _ = fx.storage.shutdown().await;
}
