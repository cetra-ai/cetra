//! Full differential verification of Shape B SQL (`SqlEngineContext`) against
//! the DataFusion oracle (`crates/storage/src/sql/`).

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use arrow::array::{
    ArrayRef, Decimal128Array, Float64Array, Int64Array, ListBuilder, RecordBatch, StringArray,
    StringBuilder,
};
use arrow::datatypes::{DataType as ArrowDataType, Field as ArrowField, Schema as ArrowSchema};
use bytes::Bytes;
use cetra_core::enums::{ScoreSource, SpanKind};
use cetra_core::ids::{ProjectId, SpanId, TraceId};
use cetra_core::score::ScoreValue;
use cetra_core::time_serde::parse_rfc3339;
use cetra_query::encode::SqlJsonResult;
use cetra_query::exec::streaming::UnionSource;
use cetra_query::exec::{ScanSource, SpillManager, WorkerPool};
use cetra_query::sql::{TimeWindow as QeTimeWindow, raw_spans_schema};
use datafusion::datasource::memory::MemTable;
use datafusion::execution::object_store::ObjectStoreUrl;
use datafusion::execution::runtime_env::{RuntimeEnv, RuntimeEnvBuilder};
use futures::StreamExt;
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::QueryOptions;
use crate::catalog::{Catalog, CatalogEdit, FileId, FileMeta, FileTable};
use crate::column_bridge::{arrow_schema_to_column, record_batch_to_column};
use crate::deletion::{PendingDeletion, Tombstones};
use crate::memtable::Memtable;
use crate::objects::OBJECT_STORE_URL;
use crate::parquet::write_file;
use crate::partials::partials_from_spans;
use crate::projects::ProjectStates;
use crate::query_error::RunError;
use crate::read_state::ReadState;
use crate::sql::{ExportFormat, SQL_MAX_ROWS, SqlJson, SqlRequest};
use crate::test_support::{
    dataset_scores, dataset_spans, score_row, score_rows, span_row, span_rows,
};
use crate::verify::{Suspects, VerifyQueue};
use crate::views::{QueryContext, SqliteView, TimeWindow as DfTimeWindow};
use crate::wal::Table;

use super::admission::AsyncAdmissionPool;
use super::mem_source::MemtableSource;
use super::metadata_cache::MetadataCache;
use super::parquet_source::ParquetSource;
use super::scores_source::ScoresSource;
use super::sql::SqlEngineContext;
use super::traces_source::TracesSource;

fn project() -> ProjectId {
    "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
}

fn tid(n: u16) -> TraceId {
    let mut bytes = [0u8; 16];
    let pair = (n + 1).to_be_bytes();
    for chunk in bytes.as_chunks_mut::<2>().0 {
        chunk.copy_from_slice(&pair);
    }
    TraceId::from_bytes(bytes).unwrap()
}

fn sid(n: u8) -> SpanId {
    SpanId::from_bytes([n.max(1); 8]).unwrap()
}

struct DiffHarness {
    df: QueryContext,
    qe: SqlEngineContext,
    _spill_dir: tempfile::TempDir,
    rt_runtime: Arc<RuntimeEnv>,
}

impl DiffHarness {
    async fn new(window: Option<(i64, i64)>, visible_from: Option<i64>, pool_bytes: usize) -> Self {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let cache = Arc::new(MetadataCache::new(1024 * 1024));
        let verify = Arc::new(VerifyQueue::default());
        let suspects = Suspects::new(Arc::clone(&verify));

        let base = parse_rfc3339("2026-01-01T10:00:00Z").unwrap();

        // Start with the standard API dataset spans, then add rich spans covering
        // tools, LLM models, releases, multiple sessions, and an erased user/session/trace.
        let mut all_spans = dataset_spans();

        let make_span = |trace_num: u16,
                         span_num: u8,
                         start_offset_s: i64,
                         dur_s: i64,
                         name: &str,
                         kind: SpanKind,
                         model: Option<&str>,
                         release: Option<&str>,
                         session: Option<&str>,
                         user: Option<&str>,
                         cost: Option<f64>,
                         tags: &[&str]| {
            let mut s = span_row(
                trace_num,
                span_num,
                base + start_offset_s * 1_000_000_000,
                base + 100,
            );
            s.name = name.to_owned();
            s.span_kind = kind;
            s.end_time = s.start_time + dur_s * 1_000_000_000;
            s.request_model = model.map(str::to_owned);
            s.response_model = model.map(str::to_owned);
            s.release = release.map(str::to_owned);
            s.session_id = session.map(str::to_owned);
            s.user_id = user.map(str::to_owned);
            s.cost_total = cost;
            s.total_tokens = Some(100);
            s.tags = tags.iter().map(|t| (*t).to_owned()).collect();
            s.metadata.insert("k1".to_owned(), "v1".to_owned());
            s.metadata.insert("channel".to_owned(), "email".to_owned());
            s
        };

        // Day-old span (hidden when visible_from >= base).
        all_spans.push(make_span(
            100,
            10,
            -86_400,
            2,
            "old-span",
            SpanKind::Llm,
            Some("gpt-4o"),
            Some("v0.9"),
            Some("sess-old"),
            Some("u-old"),
            Some(0.05),
            &["legacy"],
        ));

        // Tool calls & LLM spans across releases & sessions.
        all_spans.push(make_span(
            101,
            11,
            60,
            3,
            "llm-chat",
            SpanKind::Llm,
            Some("gpt-4o"),
            Some("v1.0"),
            Some("sess-extra-1"),
            Some("u-alice"),
            Some(0.25),
            &["prod", "fast"],
        ));
        all_spans.push(make_span(
            101,
            12,
            65,
            1,
            "web-search",
            SpanKind::Tool,
            None,
            Some("v1.0"),
            Some("sess-extra-1"),
            Some("u-alice"),
            Some(0.0),
            &["tool"],
        ));
        all_spans.push(make_span(
            101,
            13,
            70,
            2,
            "db-lookup",
            SpanKind::Tool,
            None,
            Some("v1.0"),
            Some("sess-extra-1"),
            Some("u-alice"),
            None,
            &["tool"],
        ));
        all_spans.push(make_span(
            102,
            14,
            120,
            4,
            "llm-reason",
            SpanKind::Llm,
            Some("claude-3-5-sonnet"),
            Some("v1.1"),
            Some("sess-extra-1"),
            Some("u-bob"),
            Some(0.45),
            &["prod", "reasoning"],
        ));
        all_spans.push(make_span(
            103,
            15,
            180,
            5,
            "llm-erased",
            SpanKind::Llm,
            Some("gpt-4o"),
            Some("v1.1"),
            Some("erased-session-1"),
            Some("erased-user@example.com"),
            Some(9.99),
            &["erased"],
        ));

        // Add dense LLM latency samples for every model present (600..4182ms) so approx_percentile_cont (DdSketch vs t-digest) agrees <= 1%
        let models = [
            "gpt-4o",
            "claude-3-5-sonnet",
            "claude-sonnet-4-5",
            "gpt-4o-mini",
        ];
        for i in 0..800_u16 {
            let model = models[usize::from(i % 4)];
            let mut s = make_span(
                104 + i,
                20,
                300 + i64::from(i),
                1,
                "llm-dense",
                SpanKind::Llm,
                Some(model),
                Some("v1.0"),
                Some("sess-extra-1"),
                Some("u-alice"),
                Some(0.01),
                &["prod"],
            );
            s.span_id =
                SpanId::from_bytes([0, 0, 0, 0, 0, 1, (i >> 8) as u8, (i & 0xff) as u8]).unwrap();
            s.end_time = s.start_time + (600 + i64::from(i / 4) * 18) * 1_000_000;
            all_spans.push(s);
        }

        // Split spans across Parquet (first half) and Memtable (second half).
        let split_idx = all_spans.len() / 2;
        let parquet_spans = &all_spans[..split_idx];
        let mem_spans = &all_spans[split_idx..];

        let day = super::day_of_ns(base).unwrap();
        let f_span = write_parquet_obj(
            &store,
            FileTable::Spans,
            day,
            "tables/spans/0.parquet",
            span_rows(parquet_spans),
        )
        .await;
        let f_trace = write_parquet_obj(
            &store,
            FileTable::Traces,
            day,
            "tables/traces/0.parquet",
            partials_from_spans(&span_rows(parquet_spans)).unwrap(),
        )
        .await;

        let mut spans_mt = Memtable::new(Table::Spans);
        spans_mt.insert(project(), span_rows(mem_spans), 1).unwrap();

        // Scores: dataset_scores + evaluator/rule/experiment/LWW/tombstone scores.
        let mut all_scores = dataset_scores();
        let eval_a = Uuid::from_u128(0x0100);
        let eval_b = Uuid::from_u128(0x0101);
        let exp_a = Uuid::from_u128(0x0200);

        let mut sc1 = score_row(
            101,
            11,
            base + 60_000_000_000,
            base + 60_000_000_000,
            base + 60_000_000_000,
            false,
            "judge-quality",
        );
        sc1.source = ScoreSource::LlmJudge;
        sc1.evaluator_id = Some(eval_a);
        sc1.experiment_id = Some(exp_a);
        sc1.value = ScoreValue::Numeric(0.9);
        all_scores.push(sc1);

        let mut sc2 = score_row(
            102,
            14,
            base + 120_000_000_000,
            base + 120_000_000_000,
            base + 120_000_000_000,
            false,
            "judge-quality",
        );
        sc2.source = ScoreSource::LlmJudge;
        sc2.evaluator_id = Some(eval_b);
        sc2.experiment_id = Some(exp_a);
        sc2.value = ScoreValue::Numeric(0.7);
        all_scores.push(sc2);

        // Categorical scores for eval/stats.rs query 2 (nested derived + row_number)
        for (i, label) in ["good", "good", "fair"].iter().enumerate() {
            let mut cat = score_row(
                101,
                11,
                base + (i as i64 + 1) * 1_000_000_000,
                base + 100,
                base + 100,
                false,
                "tone",
            );
            cat.score_id = Uuid::now_v7();
            cat.source = ScoreSource::LlmJudge;
            cat.evaluator_id = Some(eval_a);
            cat.value = ScoreValue::Categorical {
                label: (*label).to_owned(),
                value: None,
            };
            all_scores.push(cat);
        }

        // LWW override & deleted tombstone
        let lww_id = Uuid::now_v7();
        let mut lww_old = score_row(
            101,
            11,
            base + 10_000_000_000,
            base + 10_000_000_000,
            base + 10_000_000_000,
            false,
            "judge-quality",
        );
        lww_old.score_id = lww_id;
        lww_old.source = ScoreSource::LlmJudge;
        lww_old.value = ScoreValue::Numeric(0.1);
        let mut lww_new = lww_old.clone();
        lww_new.updated_at = base + 50_000_000_000;
        lww_new.value = ScoreValue::Numeric(0.95);
        all_scores.push(lww_old);
        all_scores.push(lww_new);

        let del_id = Uuid::now_v7();
        let mut del_live = score_row(
            102,
            14,
            base + 20_000_000_000,
            base + 20_000_000_000,
            base + 20_000_000_000,
            false,
            "judge-quality",
        );
        del_live.score_id = del_id;
        del_live.source = ScoreSource::LlmJudge;
        del_live.value = ScoreValue::Numeric(0.2);
        let mut del_tomb = del_live.clone();
        del_tomb.deleted = true;
        del_tomb.updated_at = base + 60_000_000_000;
        all_scores.push(del_live);
        all_scores.push(del_tomb);

        let sc_split = all_scores.len() / 2;
        let parquet_scores = &all_scores[..sc_split];
        let mem_scores = &all_scores[sc_split..];

        let f_score = write_parquet_obj(
            &store,
            FileTable::Scores,
            day,
            "tables/scores/0.parquet",
            score_rows(parquet_scores),
        )
        .await;
        let mut scores_mt = Memtable::new(Table::Scores);
        scores_mt
            .insert(project(), score_rows(mem_scores), 1)
            .unwrap();

        // GDPR tombstones
        let deletion = PendingDeletion {
            id: Uuid::now_v7(),
            seq: 1,
            project: project(),
            cutoff: base + 1_000_000_000_000,
            registered_at: base + 1_000_000_000_000,
            trace_ids: [*tid(103).as_bytes()].into_iter().collect(),
            users: ["erased-user@example.com".to_owned()].into_iter().collect(),
            sessions: ["erased-session-1".to_owned()].into_iter().collect(),
        };
        let tombstones = Tombstones::default().with(deletion);
        let deletions = tombstones.for_project(project());

        let catalog = {
            let edit = CatalogEdit {
                add: vec![(*f_span).clone(), (*f_trace).clone(), (*f_score).clone()],
                remove: Vec::new(),
                flushed_seq: None,
                sources: Vec::new(),
                raise: Vec::new(),
                quarantine: Vec::new(),
            };
            Arc::new(Catalog::empty().apply(&edit).unwrap().0)
        };

        let spans_snap = spans_mt.snapshot();
        let scores_snap = scores_mt.snapshot();

        // Filter memtable snapshots for M3 sources identically to `project_query`
        let qe_spans_snap = crate::deletion::project_snapshot(
            Table::Spans,
            &spans_snap,
            project(),
            None,
            &deletions,
        )
        .unwrap();
        let qe_scores_snap = crate::deletion::project_snapshot(
            Table::Scores,
            &scores_snap,
            project(),
            None,
            &deletions,
        )
        .unwrap();

        // DataFusion ReadState + QueryContext
        let state = ReadState {
            spans: spans_snap,
            scores: scores_snap,
            catalog,
            tombstones: Arc::new(tombstones),
            projects: Arc::new(ProjectStates::default()),
        };
        let rt_runtime = RuntimeEnvBuilder::new()
            .with_memory_limit(pool_bytes, 1.0)
            .build_arc()
            .unwrap();
        let url = ObjectStoreUrl::parse(OBJECT_STORE_URL).unwrap();
        rt_runtime.register_object_store(url.as_ref(), Arc::clone(&store));

        let pq = crate::project_query(
            &state,
            project(),
            QueryOptions::new(None, visible_from),
            &rt_runtime,
            NonZeroUsize::new(2).unwrap(),
            60_000_000_000,
            &verify,
        )
        .unwrap();
        let df_window = window.map(|(f, t)| DfTimeWindow::new(f, t).unwrap());
        let df = QueryContext::new(pq, df_window).unwrap();

        // Shape B M3 sources + SqlEngineContext
        let span_schema = raw_spans_schema();
        let span_parquet = Arc::new(ParquetSource::new(
            Arc::clone(&store),
            FileTable::Spans,
            Arc::clone(&span_schema),
            vec![f_span],
            Arc::clone(&deletions),
            Arc::clone(&cache),
            suspects.clone(),
            tokio::runtime::Handle::current(),
        ));
        let span_mem = Arc::new(
            MemtableSource::new(
                Table::Spans,
                project(),
                None,
                Arc::clone(&qe_spans_snap),
                Arc::clone(&deletions),
            )
            .unwrap(),
        );
        let spans_src: Arc<dyn ScanSource> =
            Arc::new(UnionSource::new(span_schema, vec![span_parquet, span_mem]).unwrap());

        let traces_src = Arc::new(
            TracesSource::new(
                Arc::clone(&store),
                project(),
                vec![f_trace],
                qe_spans_snap,
                60_000_000_000,
                Arc::clone(&deletions),
                Arc::clone(&cache),
                suspects.clone(),
                tokio::runtime::Handle::current(),
            )
            .unwrap(),
        );
        let scores_src = Arc::new(
            ScoresSource::new(
                Arc::clone(&store),
                project(),
                vec![f_score],
                qe_scores_snap,
                None,
                deletions,
                cache,
                suspects.clone(),
                tokio::runtime::Handle::current(),
            )
            .unwrap(),
        );

        let spill_dir = tempfile::tempdir().unwrap();
        let admission = Arc::new(AsyncAdmissionPool::new(pool_bytes));
        let workers = Arc::new(WorkerPool::new(2).unwrap());
        let spill = Arc::new(SpillManager::with_fixed_cap(
            spill_dir.path().to_path_buf(),
            32 * 1024 * 1024,
        ));
        let qe_window = window.map(|(f, t)| QeTimeWindow::new(f, t).unwrap());
        let mut qe = SqlEngineContext::new(
            spans_src,
            traces_src,
            scores_src,
            admission,
            workers,
            spill,
            visible_from,
            qe_window,
            HashMap::new(),
            suspects,
        );

        // Register extra tables (dataset_items & experiment_items) in both DataFusion and Shape B
        let (ds_arrow_schema, ds_arrow_batch) = build_dataset_items_batch();
        let (exp_arrow_schema, exp_arrow_batch) = build_experiment_items_batch();

        df.register_sqlite_view(
            SqliteView::DatasetItems,
            Arc::new(
                MemTable::try_new(
                    Arc::clone(&ds_arrow_schema),
                    vec![vec![ds_arrow_batch.clone()]],
                )
                .unwrap(),
            ),
        )
        .unwrap();
        df.register_sqlite_view(
            SqliteView::ExperimentItems,
            Arc::new(
                MemTable::try_new(
                    Arc::clone(&exp_arrow_schema),
                    vec![vec![exp_arrow_batch.clone()]],
                )
                .unwrap(),
            ),
        )
        .unwrap();

        qe.register_extra_arrow_table("dataset_items", &ds_arrow_schema, &[ds_arrow_batch])
            .unwrap();
        qe.register_extra_arrow_table("experiment_items", &exp_arrow_schema, &[exp_arrow_batch])
            .unwrap();

        Self {
            df,
            qe,
            _spill_dir: spill_dir,
            rt_runtime,
        }
    }
}

async fn write_parquet_obj(
    store: &Arc<dyn ObjectStore>,
    table: FileTable,
    day: chrono::NaiveDate,
    path: &str,
    batch: RecordBatch,
) -> Arc<FileMeta> {
    let written = write_file(table, 0, &[Arc::new(batch)]).unwrap();
    let size = written.bytes.len() as u64;
    store
        .put(&ObjectPath::from(path), PutPayload::from(written.bytes))
        .await
        .unwrap();
    Arc::new(FileMeta {
        id: FileId::new_v7(),
        table,
        project: project(),
        day,
        level: 0,
        path: path.to_owned(),
        size,
        footer_size: written.footer_size.max(8),
        rows: written.rows,
        min_time: written.min_time,
        max_time: written.max_time,
        min_ingest_time: written.min_ingest_time,
        max_ingest_time: written.max_ingest_time,
        fully_merged: false,
        deletions_applied_seq: 0,
    })
}

fn build_dataset_items_batch() -> (Arc<ArrowSchema>, RecordBatch) {
    let tag_field = Arc::new(ArrowField::new("item", ArrowDataType::Utf8, true));
    let schema = Arc::new(ArrowSchema::new(vec![
        ArrowField::new("id", ArrowDataType::Int64, true),
        ArrowField::new("trace_id", ArrowDataType::Utf8, false),
        ArrowField::new("label", ArrowDataType::Utf8, true),
        ArrowField::new("weight", ArrowDataType::Float64, true),
        ArrowField::new("tags", ArrowDataType::List(Arc::clone(&tag_field)), false),
    ]));
    let mut lb = ListBuilder::new(StringBuilder::new()).with_field(tag_field);
    lb.values().append_value("t1");
    lb.values().append_value("t2");
    lb.append(true);
    lb.values().append_value("t3");
    lb.append(true);
    lb.append(true); // empty list
    lb.values().append_value("t4");
    lb.append(true);

    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(Int64Array::from(vec![Some(1), Some(2), None, Some(4)])) as ArrayRef,
            Arc::new(StringArray::from(vec![
                tid(101).to_hex(),
                tid(102).to_hex(),
                tid(101).to_hex(),
                "0000000000000000000000000000dead".to_owned(),
            ])),
            Arc::new(StringArray::from(vec![
                Some("=cmd"),
                Some("ok"),
                None,
                Some("say \"hi\", ok"),
            ])),
            Arc::new(Float64Array::from(vec![
                Some(1.5),
                Some(2.5),
                Some(f64::NAN),
                None,
            ])),
            Arc::new(lb.finish()),
        ],
    )
    .unwrap();
    (schema, batch)
}

fn build_experiment_items_batch() -> (Arc<ArrowSchema>, RecordBatch) {
    let schema = Arc::new(ArrowSchema::new(vec![
        ArrowField::new("id", ArrowDataType::Int64, true),
        ArrowField::new("experiment_id", ArrowDataType::Utf8, false),
        ArrowField::new("cost_dec", ArrowDataType::Decimal128(10, 2), true),
    ]));
    let dec = Decimal128Array::from(vec![Some(150), Some(-25), None])
        .with_precision_and_scale(10, 2)
        .unwrap();
    let batch = RecordBatch::try_new(
        Arc::clone(&schema),
        vec![
            Arc::new(Int64Array::from(vec![Some(1), Some(3), None])) as ArrayRef,
            Arc::new(StringArray::from(vec![
                Uuid::from_u128(0x0200).to_string(),
                Uuid::from_u128(0x0200).to_string(),
                Uuid::from_u128(0x0201).to_string(),
            ])),
            Arc::new(dec),
        ],
    )
    .unwrap();
    (schema, batch)
}

fn req<'a>(query: &'a str, params: &'a Map<String, Value>) -> SqlRequest<'a> {
    SqlRequest {
        query,
        params,
        limit: SQL_MAX_ROWS,
        timeout: Duration::from_secs(30),
    }
}

fn assert_value_parity(actual: &Value, expected: &Value, ctx_msg: &str, rel_tol: f64) {
    match (actual, expected) {
        (Value::Null, Value::Null) => {}
        (Value::Bool(a), Value::Bool(b)) => assert_eq!(a, b, "{ctx_msg}: bool mismatch"),
        (Value::Number(a), Value::Number(b)) => {
            if let (Some(ia), Some(ib)) = (a.as_i64(), b.as_i64()) {
                assert_eq!(ia, ib, "{ctx_msg}: i64 {ia} vs {ib}");
            } else {
                let fa = a.as_f64().unwrap();
                let fb = b.as_f64().unwrap();
                let scale = fb.abs().max(1.0);
                assert!(
                    (fa - fb).abs() <= rel_tol * scale + 1e-12,
                    "{ctx_msg}: float {fa} vs {fb} (tol={rel_tol})"
                );
            }
        }
        (Value::String(a), Value::String(b)) => assert_eq!(a, b, "{ctx_msg}: string mismatch"),
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "{ctx_msg}: array len");
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                assert_value_parity(x, y, &format!("{ctx_msg}[{i}]"), rel_tol);
            }
        }
        (Value::Object(a), Value::Object(b)) => {
            assert_eq!(a.len(), b.len(), "{ctx_msg}: obj len");
            for (k, va) in a {
                let vb = b
                    .get(k)
                    .unwrap_or_else(|| panic!("{ctx_msg}: missing key {k}"));
                assert_value_parity(va, vb, &format!("{ctx_msg}.{k}"), rel_tol);
            }
        }
        _ => panic!("{ctx_msg}: value mismatch actual={actual:?} vs expected={expected:?}"),
    }
}

fn assert_json_parity(qe: &SqlJsonResult, df: &SqlJson, ordered: bool, rel_tol: f64, label: &str) {
    let qe_names: Vec<&str> = qe.columns.iter().map(|c| c.name.as_str()).collect();
    let df_names: Vec<&str> = df.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(qe_names, df_names, "{label}: column names");
    assert_eq!(qe.truncated, df.truncated, "{label}: truncated flag");
    assert_eq!(qe.rows.len(), df.rows.len(), "{label}: row count");

    if ordered {
        for (idx, (qr, dr)) in qe.rows.iter().zip(&df.rows).enumerate() {
            assert_eq!(
                qr.keys().collect::<Vec<_>>(),
                dr.keys().collect::<Vec<_>>(),
                "{label} row {idx} key order"
            );
            for k in &qe_names {
                assert_value_parity(&qr[*k], &dr[*k], &format!("{label} row {idx}.{k}"), rel_tol);
            }
        }
    } else {
        let mut q_sorted: Vec<String> = qe
            .rows
            .iter()
            .map(|r| Value::Object(r.clone()).to_string())
            .collect();
        let mut d_sorted: Vec<String> = df
            .rows
            .iter()
            .map(|r| Value::Object(r.clone()).to_string())
            .collect();
        q_sorted.sort();
        d_sorted.sort();
        // Parse back and compare per-field with float tolerance.
        assert_eq!(q_sorted.len(), d_sorted.len(), "{label}");
        for (idx, (qs, ds)) in q_sorted.iter().zip(&d_sorted).enumerate() {
            let qv: Value = serde_json::from_str(qs).unwrap();
            let dv: Value = serde_json::from_str(ds).unwrap();
            assert_value_parity(&qv, &dv, &format!("{label} sorted_row {idx}"), rel_tol);
        }
    }
}

async fn assert_parity(
    h: &DiffHarness,
    sql: &str,
    params: &Map<String, Value>,
    ordered: bool,
    rel_tol: f64,
    label: &str,
) {
    let df =
        h.df.sql_json(req(sql, params))
            .await
            .unwrap_or_else(|e| panic!("DF `{label}` failed: {e}"));
    let qe =
        h.qe.sql_json(req(sql, params))
            .await
            .unwrap_or_else(|e| panic!("QE `{label}` failed: {e}"));
    assert_json_parity(&qe, &df, ordered, rel_tol, label);
    assert_eq!(h.qe.admission().used(), 0, "{label}: leaked QE admission");
    assert_eq!(
        h.rt_runtime.memory_pool.reserved(),
        0,
        "{label}: leaked DF pool"
    );
}

#[tokio::test]
async fn diff_all_five_shipped_web_ui_examples() {
    let h = DiffHarness::new(None, None, 64 * 1024 * 1024).await;
    let examples_ts = include_str!("../../../../web/src/lib/sql/examples.ts");
    let params = Map::new();

    let ex1 = "SELECT date_trunc('day', start_time) AS day, model, sum(cost_total) AS cost\nFROM spans\nWHERE span_kind = 'llm' AND model IS NOT NULL\nGROUP BY day, model\nORDER BY day, cost DESC";
    let ex2 = "SELECT model,\n  approx_percentile_cont(duration_ms, 0.5) AS p50_ms,\n  approx_percentile_cont(duration_ms, 0.95) AS p95_ms,\n  approx_percentile_cont(duration_ms, 0.99) AS p99_ms,\n  count(*) AS calls\nFROM spans\nWHERE span_kind = 'llm' AND model IS NOT NULL\nGROUP BY model\nORDER BY calls DESC";
    let ex3 = "SELECT trace_id, count(*) AS tool_calls\nFROM spans\nWHERE span_kind = 'tool'\nGROUP BY trace_id\nORDER BY tool_calls DESC\nLIMIT 50";
    let ex4 = "SELECT t.release, s.name, avg(s.value_num) AS avg_score, count(*) AS scores\nFROM scores s\nJOIN traces t ON s.trace_id = t.trace_id\nWHERE s.source = 'llm_judge' AND s.value_num IS NOT NULL\nGROUP BY t.release, s.name\nORDER BY t.release, s.name";
    let ex5 = "SELECT user_id, count(*) AS traces, sum(cost_total) AS cost\nFROM traces\nWHERE user_id IS NOT NULL\nGROUP BY user_id\nORDER BY cost DESC\nLIMIT 50";

    for (idx, q) in [ex1, ex2, ex3, ex4, ex5].iter().enumerate() {
        for line in q.lines() {
            assert!(
                examples_ts.contains(line),
                "example {idx} drifted from web/src/lib/sql/examples.ts: {line}"
            );
        }
    }

    assert_parity(&h, ex1, &params, true, 1e-12, "web example 1").await;
    // approx_percentile_cont: DdSketch vs t-digest within <= 1% relative tolerance
    assert_parity(&h, ex2, &params, true, 0.01, "web example 2").await;
    assert_parity(&h, ex3, &params, true, 1e-12, "web example 3").await;
    assert_parity(&h, ex4, &params, true, 1e-12, "web example 4").await;
    assert_parity(&h, ex5, &params, true, 1e-12, "web example 5").await;
}

#[tokio::test]
async fn diff_all_three_eval_stats_queries() {
    let h = DiffHarness::new(None, None, 64 * 1024 * 1024).await;

    let mut params = Map::new();
    params.insert("e0".to_owned(), json!(Uuid::from_u128(0x0100).to_string()));
    params.insert("e1".to_owned(), json!(Uuid::from_u128(0x0101).to_string()));

    // Query 1: bucketed score aggs with repeated date_trunc expression
    let q1 = "SELECT evaluator_id, date_trunc('day', created_at) AS bucket, count(*) AS n, \
              count(value_num) AS num_n, sum(value_num) AS num_sum FROM scores \
              WHERE evaluator_id IS NOT NULL AND deleted = false AND evaluator_id IN ({e0:String}, {e1:String}) \
              GROUP BY evaluator_id, date_trunc('day', created_at)";
    assert_parity(&h, q1, &params, false, 1e-12, "eval stats q1").await;

    // Query 2: unaliased nested derived table + row_number()
    let q2 = "SELECT evaluator_id, value_str, n FROM (SELECT evaluator_id, value_str, n, \
              row_number() OVER (PARTITION BY evaluator_id ORDER BY n DESC, value_str ASC) AS rn \
              FROM (SELECT evaluator_id, value_str, count(*) AS n FROM scores \
              WHERE evaluator_id IS NOT NULL AND deleted = false AND data_type = 'categorical' \
              AND value_str IS NOT NULL AND evaluator_id IN ({e0:String}, {e1:String}) \
              GROUP BY evaluator_id, value_str)) WHERE rn = 1";
    assert_parity(&h, q2, &params, false, 1e-12, "eval stats q2").await;

    // Query 3: scored experiments
    let mut exp_params = Map::new();
    exp_params.insert("n0".to_owned(), json!("judge-quality"));
    let q3 = "SELECT name, experiment_id, max(created_at) AS last_at FROM scores \
              WHERE experiment_id IS NOT NULL AND deleted = false AND name IN ({n0:String}) \
              GROUP BY name, experiment_id";
    assert_parity(&h, q3, &exp_params, false, 1e-12, "eval stats q3").await;
}

#[tokio::test]
async fn diff_mcp_patterns_window_visible_from_and_params() {
    let from = parse_rfc3339("2026-01-01T10:00:00Z").unwrap();
    let to = parse_rfc3339("2026-01-01T10:30:00Z").unwrap();
    let visible_from = parse_rfc3339("2026-01-01T10:00:30Z").unwrap();
    let h = DiffHarness::new(Some((from, to)), Some(visible_from), 64 * 1024 * 1024).await;
    let empty = Map::new();

    // Window + visible_from apply inside CTEs, subqueries, and all base views
    assert_parity(
        &h,
        "SELECT count(*) AS n FROM spans",
        &empty,
        true,
        1e-12,
        "windowed spans",
    )
    .await;
    assert_parity(
        &h,
        "WITH t AS (SELECT trace_id FROM traces) SELECT count(*) AS n FROM t",
        &empty,
        true,
        1e-12,
        "windowed cte traces",
    )
    .await;
    assert_parity(
        &h,
        "SELECT count(*) AS n FROM scores",
        &empty,
        true,
        1e-12,
        "windowed scores",
    )
    .await;
    assert_parity(
        &h,
        "SELECT * FROM sessions ORDER BY session_id",
        &empty,
        true,
        1e-12,
        "windowed sessions",
    )
    .await;

    // CTE shadowing base view
    assert_parity(
        &h,
        "WITH spans AS (SELECT 1 AS x UNION ALL SELECT 2 AS x) SELECT sum(x) AS total FROM spans",
        &empty,
        true,
        1e-12,
        "cte shadowing spans",
    )
    .await;

    // CROSS JOIN + dataset_items / experiment_items join
    assert_parity(
        &h,
        "SELECT d.id AS did, e.id AS eid FROM dataset_items d CROSS JOIN experiment_items e ORDER BY did NULLS LAST, eid NULLS LAST",
        &empty,
        true,
        1e-12,
        "cross join extra tables",
    )
    .await;

    // All 5 parameter kinds (including Timestamp RFC3339 & epoch-ms and repeated param)
    let mut p_all = Map::new();
    p_all.insert("k".to_owned(), json!("llm"));
    p_all.insert("n".to_owned(), json!(1));
    p_all.insert("c".to_owned(), json!(0.1));
    p_all.insert("r".to_owned(), json!(true));
    p_all.insert("t".to_owned(), json!("2026-01-01T10:01:00Z"));
    assert_parity(
        &h,
        "SELECT count(*) AS n FROM spans s JOIN traces t ON s.trace_id = t.trace_id \
         WHERE (s.span_kind = {k:String} OR s.span_kind = {k:String}) \
         AND t.span_count >= {n:Int} AND coalesce(s.cost_total, 0.0) >= {c:Float} \
         AND t.has_root = {r:Bool} AND s.start_time >= {t:Timestamp}",
        &p_all,
        true,
        1e-12,
        "all 5 param kinds",
    )
    .await;

    let ms = parse_rfc3339("2026-01-01T10:01:00Z").unwrap() / 1_000_000;
    let mut p_ms = Map::new();
    p_ms.insert("t".to_owned(), json!(ms));
    assert_parity(
        &h,
        "SELECT trace_id FROM traces WHERE start_time >= {t:Timestamp} ORDER BY trace_id",
        &p_ms,
        true,
        1e-12,
        "timestamp epoch-ms param",
    )
    .await;
}

#[tokio::test]
async fn diff_sql_features_subqueries_windows_set_ops_and_hex_pruning() {
    let h = DiffHarness::new(None, None, 64 * 1024 * 1024).await;
    let empty = Map::new();

    // Sessions all 11 columns
    assert_parity(
        &h,
        "SELECT * FROM sessions ORDER BY session_id",
        &empty,
        true,
        1e-12,
        "sessions full",
    )
    .await;

    // UNNEST
    assert_parity(
        &h,
        "SELECT id, unnest(tags) AS tag FROM dataset_items ORDER BY id NULLS LAST, tag",
        &empty,
        true,
        1e-12,
        "unnest",
    )
    .await;

    // VALUES
    assert_parity(
        &h,
        "SELECT a, b FROM (VALUES (1, 'x'), (2, 'y')) AS t(a, b) ORDER BY a",
        &empty,
        true,
        1e-12,
        "values",
    )
    .await;

    // UNION / UNION ALL / INTERSECT / EXCEPT
    assert_parity(
        &h,
        "SELECT id FROM dataset_items UNION SELECT id FROM experiment_items ORDER BY id NULLS LAST",
        &empty,
        true,
        1e-12,
        "union distinct",
    )
    .await;
    assert_parity(
        &h,
        "SELECT id FROM dataset_items UNION ALL SELECT id FROM experiment_items ORDER BY id NULLS LAST",
        &empty,
        true,
        1e-12,
        "union all",
    )
    .await;
    assert_parity(
        &h,
        "SELECT id FROM dataset_items INTERSECT SELECT id FROM experiment_items ORDER BY id NULLS LAST",
        &empty,
        true,
        1e-12,
        "intersect with null",
    )
    .await;
    assert_parity(
        &h,
        "SELECT id FROM dataset_items EXCEPT SELECT id FROM experiment_items ORDER BY id NULLS LAST",
        &empty,
        true,
        1e-12,
        "except with null",
    )
    .await;

    // IN / NOT IN with NULLs on LHS and RHS!
    assert_parity(
        &h,
        "SELECT id FROM dataset_items WHERE id IN (SELECT id FROM experiment_items) ORDER BY id",
        &empty,
        true,
        1e-12,
        "in subquery with nulls",
    )
    .await;
    assert_parity(
        &h,
        "SELECT id FROM dataset_items WHERE id NOT IN (SELECT id FROM experiment_items) ORDER BY id",
        &empty,
        true,
        1e-12,
        "not in subquery with rhs null emits 0 rows",
    )
    .await;
    assert_parity(
        &h,
        "SELECT id FROM dataset_items WHERE id NOT IN (SELECT id FROM experiment_items WHERE id IS NOT NULL) ORDER BY id",
        &empty,
        true,
        1e-12,
        "not in subquery without rhs null drops lhs null",
    )
    .await;
    assert_parity(
        &h,
        "SELECT id FROM dataset_items WHERE id NOT IN (SELECT id FROM experiment_items WHERE id = 999) ORDER BY id NULLS LAST",
        &empty,
        true,
        1e-12,
        "not in empty subquery keeps lhs null",
    )
    .await;

    // EXISTS / NOT EXISTS
    assert_parity(
        &h,
        "SELECT id FROM dataset_items WHERE EXISTS (SELECT 1 FROM experiment_items WHERE id = 1) ORDER BY id NULLS LAST",
        &empty,
        true,
        1e-12,
        "exists true",
    )
    .await;
    assert_parity(
        &h,
        "SELECT id FROM dataset_items WHERE NOT EXISTS (SELECT 1 FROM experiment_items WHERE id = 999) ORDER BY id NULLS LAST",
        &empty,
        true,
        1e-12,
        "not exists true",
    )
    .await;

    // Exact percentile_cont ASC & DESC + FILTER (WHERE ...)
    assert_parity(
        &h,
        "SELECT percentile_cont(0.25) WITHIN GROUP (ORDER BY duration_ms ASC) AS p25_asc, \
                percentile_cont(0.25) WITHIN GROUP (ORDER BY duration_ms DESC) AS p25_desc, \
                count(*) FILTER (WHERE span_kind = 'tool') AS tool_cnt \
         FROM spans",
        &empty,
        true,
        1e-12,
        "exact percentile_cont + filter where",
    )
    .await;

    // Window functions (row_number, rank, dense_rank, lag, lead, first_value, last_value, running sum/count/avg/min/max)
    assert_parity(
        &h,
        "SELECT span_id, trace_id, \
                row_number() OVER (PARTITION BY trace_id ORDER BY start_time ASC, span_id ASC) AS rn, \
                rank() OVER (PARTITION BY trace_id ORDER BY duration_ns ASC) AS rnk, \
                dense_rank() OVER (PARTITION BY trace_id ORDER BY duration_ns ASC) AS drnk, \
                lag(name, 1, 'none') OVER (PARTITION BY trace_id ORDER BY start_time ASC, span_id ASC) AS prev_name, \
                lead(name, 1) OVER (PARTITION BY trace_id ORDER BY start_time ASC, span_id ASC) AS next_name, \
                first_value(name) OVER (PARTITION BY trace_id ORDER BY start_time ASC, span_id ASC ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS fv, \
                last_value(name) OVER (PARTITION BY trace_id ORDER BY start_time ASC, span_id ASC ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS lv, \
                sum(duration_ns) OVER (PARTITION BY trace_id ORDER BY start_time ASC, span_id ASC ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) AS win_sum, \
                count(*) OVER (PARTITION BY trace_id ORDER BY start_time ASC, span_id ASC ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS win_cnt \
         FROM spans ORDER BY span_id",
        &empty,
        true,
        1e-12,
        "window functions",
    )
    .await;

    // Hex-ID pruning
    let hex_tid = tid(101).to_hex();
    let hex_sid = sid(11).to_hex();
    assert_parity(
        &h,
        &format!("SELECT span_id, name FROM spans WHERE trace_id = '{hex_tid}' AND span_id IN ('{hex_sid}', 'ffffffffffffffff') ORDER BY span_id"),
        &empty,
        true,
        1e-12,
        "hex-id pruning",
    )
    .await;
}

#[tokio::test]
async fn diff_exports_dqe15_decimal_and_explain_gdpr_redaction() {
    let h = DiffHarness::new(None, None, 64 * 1024 * 1024).await;
    let empty = Map::new();

    // 1. D-QE-15 columns[].type + Decimal128 JSON string vs Arrow JSON number
    let dec_sql = "SELECT id, cost_dec FROM experiment_items ORDER BY id NULLS LAST";
    let df_dec = h.df.sql_json(req(dec_sql, &empty)).await.unwrap();
    let qe_dec = h.qe.sql_json(req(dec_sql, &empty)).await.unwrap();
    assert_eq!(qe_dec.columns[0].type_name(), "integer");
    assert_eq!(qe_dec.columns[1].type_name(), "decimal");
    assert_eq!(qe_dec.rows.len(), df_dec.rows.len());
    // Shape B encodes Decimal128 as exact string ("1.50", "-0.25", null) vs DataFusion's JSON number (1.5, -0.25, null)
    assert_eq!(qe_dec.rows[0]["cost_dec"], json!("1.50"));
    assert_eq!(qe_dec.rows[1]["cost_dec"], json!("-0.25"));
    assert_eq!(qe_dec.rows[2]["cost_dec"], Value::Null);
    assert!(df_dec.rows[0]["cost_dec"].is_number());

    // Verify D-QE-15 types on spans rich columns
    let span_cols = h
        .qe
        .sql_check("SELECT trace_id, start_time, duration_ns, duration_ms, tags, metadata, events, links FROM spans", &empty)
        .await
        .unwrap();
    let type_names: Vec<&str> = span_cols.iter().map(|c| c.type_name()).collect();
    assert_eq!(
        type_names,
        vec![
            "string",
            "timestamp",
            "integer",
            "float",
            "string_list",
            "string_map",
            "json",
            "json"
        ]
    );

    // 2. CSV / NDJSON / Parquet export parity
    let export_sql =
        "SELECT id, trace_id, label, weight, tags FROM dataset_items ORDER BY id NULLS LAST";
    let df_csv = collect_export(&h.df, export_sql, ExportFormat::Csv).await;
    let qe_csv = collect_qe_export(&h.qe, export_sql, ExportFormat::Csv).await;
    assert_eq!(
        String::from_utf8(qe_csv).unwrap(),
        String::from_utf8(df_csv).unwrap()
    );

    let df_nd = collect_export(&h.df, export_sql, ExportFormat::Ndjson).await;
    let qe_nd = collect_qe_export(&h.qe, export_sql, ExportFormat::Ndjson).await;
    let qe_nd_rows: Vec<Value> = String::from_utf8(qe_nd)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let df_nd_rows: Vec<Value> = String::from_utf8(df_nd)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(qe_nd_rows.len(), df_nd_rows.len());
    for (i, (q, d)) in qe_nd_rows.iter().zip(&df_nd_rows).enumerate() {
        assert_value_parity(q, d, &format!("ndjson row {i}"), 1e-12);
    }

    let pq_sql = "SELECT trace_id, span_id, name, start_time, duration_ns, tags, metadata, events, links FROM spans ORDER BY span_id";
    let qe_pq_bytes = collect_qe_export(&h.qe, pq_sql, ExportFormat::Parquet).await;
    let df_pq_bytes = collect_export(&h.df, pq_sql, ExportFormat::Parquet).await;
    let qe_pq_batches: Vec<RecordBatch> =
        ParquetRecordBatchReaderBuilder::try_new(Bytes::from(qe_pq_bytes))
            .unwrap()
            .build()
            .unwrap()
            .map(|b| b.unwrap())
            .collect();
    let df_pq_batches: Vec<RecordBatch> =
        ParquetRecordBatchReaderBuilder::try_new(Bytes::from(df_pq_bytes))
            .unwrap()
            .build()
            .unwrap()
            .map(|b| b.unwrap())
            .collect();
    let qe_rows_total: usize = qe_pq_batches.iter().map(RecordBatch::num_rows).sum();
    let df_rows_total: usize = df_pq_batches.iter().map(RecordBatch::num_rows).sum();
    assert_eq!(qe_rows_total, df_rows_total);
    // Compare normalized cetra_column representations via JSON
    let qe_col_batches: Vec<_> = qe_pq_batches
        .iter()
        .map(|b| record_batch_to_column(b).unwrap())
        .collect();
    let df_col_batches: Vec<_> = df_pq_batches
        .iter()
        .map(|b| record_batch_to_column(b).unwrap())
        .collect();
    let col_schema = Arc::new(arrow_schema_to_column(&df_pq_batches[0].schema()).unwrap());
    let qe_pq_json = cetra_query::encode::encode_json_result(
        &col_schema,
        &qe_col_batches,
        SQL_MAX_ROWS,
        usize::MAX,
        false,
        None,
    )
    .unwrap();
    let df_pq_json = cetra_query::encode::encode_json_result(
        &col_schema,
        &df_col_batches,
        SQL_MAX_ROWS,
        usize::MAX,
        false,
        None,
    )
    .unwrap();
    let df_cols = df_pq_json
        .columns
        .iter()
        .map(|c| crate::sql::SqlColumn {
            name: c.name.clone(),
            data_type: c.type_name().to_owned(),
        })
        .collect();
    assert_json_parity(
        &qe_pq_json,
        &SqlJson {
            columns: df_cols,
            rows: df_pq_json.rows,
            truncated: false,
        },
        true,
        1e-12,
        "parquet export parity",
    );

    // 3. EXPLAIN 2-col schema + GDPR redaction
    let exp =
        h.qe.sql_json(req(
            "EXPLAIN SELECT span_id, name FROM spans WHERE name = 'secret-prompt-literal'",
            &empty,
        ))
        .await
        .unwrap();
    assert_eq!(exp.columns.len(), 2);
    assert_eq!(exp.columns[0].name, "plan_type");
    assert_eq!(exp.columns[1].name, "plan");
    assert_eq!(exp.rows.len(), 2);
    assert_eq!(exp.rows[0]["plan_type"], json!("logical_plan"));
    assert_eq!(exp.rows[1]["plan_type"], json!("physical_plan"));
    let all_plan_text = format!("{}\n{}", exp.rows[0]["plan"], exp.rows[1]["plan"]);
    assert!(
        !all_plan_text.contains("secret-prompt-literal"),
        "literal leaked in EXPLAIN:\n{all_plan_text}"
    );
    assert!(
        !all_plan_text.contains("erased-user@example.com"),
        "GDPR user leaked in EXPLAIN:\n{all_plan_text}"
    );
    assert!(
        !all_plan_text.contains("erased-session-1"),
        "GDPR session leaked in EXPLAIN:\n{all_plan_text}"
    );
}

#[tokio::test]
async fn diff_sandbox_rejections_and_error_classification() {
    let h = DiffHarness::new(None, None, 64 * 1024 * 1024).await;
    let empty = Map::new();

    let invalid_queries = [
        "INSERT INTO spans VALUES (1)",
        "DELETE FROM traces",
        "SELECT 1; SELECT 2",
        "SELECT 1 AS a |> WHERE true",
        "SELECT * FROM raw_spans",
        "SELECT * FROM datafusion.public.spans",
        "SELECT * FROM generate_series(1, 3)",
        "WITH RECURSIVE r AS (SELECT 1) SELECT * FROM r",
        "SELECT score('x') FROM traces",
        "SELECT 1 INTO t",
        "SELECT repeat('x', 3)",
        "EXPLAIN ANALYZE SELECT 1",
        "SELECT a.name, b.name FROM spans a JOIN spans b ON a.span_id = b.span_id",
        "SELECT nope FROM traces",
        "SELECT $1",
        "SELECT {x:Date}",
    ];
    for q in invalid_queries {
        let df_err = h.df.sql_json(req(q, &empty)).await.unwrap_err();
        let qe_err = h.qe.sql_json(req(q, &empty)).await.unwrap_err();
        assert!(
            matches!(df_err, RunError::Invalid(_)),
            "DF expected Invalid for `{q}`, got {df_err:?}"
        );
        assert!(
            matches!(qe_err, RunError::Invalid(_)),
            "QE expected Invalid for `{q}`, got {qe_err:?}"
        );
    }

    // InvalidRequest parity (missing, unknown, bad type, coercion, limit=0, timeout=0)
    let mut bad_int = Map::new();
    bad_int.insert("c".to_owned(), json!("abc"));
    let bad_req_cases: Vec<(&str, Map<String, Value>, usize, Duration)> = vec![
        (
            "SELECT {n:String}",
            empty.clone(),
            10,
            Duration::from_secs(5),
        ),
        ("SELECT 1", bad_int.clone(), 10, Duration::from_secs(5)),
        (
            "SELECT {c:Int}",
            bad_int.clone(),
            10,
            Duration::from_secs(5),
        ),
        (
            "SELECT trace_id FROM traces WHERE span_count >= {c:String}",
            bad_int,
            10,
            Duration::from_secs(5),
        ),
        ("SELECT 1", empty.clone(), 0, Duration::from_secs(5)),
        (
            "SELECT 1",
            empty.clone(),
            SQL_MAX_ROWS + 1,
            Duration::from_secs(5),
        ),
        ("SELECT 1", empty.clone(), 10, Duration::ZERO),
    ];
    for (q, p, limit, timeout) in bad_req_cases {
        let r_df = SqlRequest {
            query: q,
            params: &p,
            limit,
            timeout,
        };
        let r_qe = SqlRequest {
            query: q,
            params: &p,
            limit,
            timeout,
        };
        let df_err = h.df.sql_json(r_df).await.unwrap_err();
        let qe_err = h.qe.sql_json(r_qe).await.unwrap_err();
        assert!(
            matches!(df_err, RunError::InvalidRequest(_)),
            "DF `{q}`: {df_err:?}"
        );
        assert!(
            matches!(qe_err, RunError::InvalidRequest(_)),
            "QE `{q}`: {qe_err:?}"
        );
    }

    // Runtime execution failures -> RunError::Failed
    for fail_q in [
        "SELECT 1 / 0",
        "SELECT regexp_like(name, '(((a{100}){100}){100})') FROM spans",
    ] {
        let df_err = h.df.sql_json(req(fail_q, &empty)).await.unwrap_err();
        let qe_err = h.qe.sql_json(req(fail_q, &empty)).await.unwrap_err();
        assert!(
            matches!(df_err, RunError::Failed(_)),
            "DF `{fail_q}`: {df_err:?}"
        );
        assert!(
            matches!(qe_err, RunError::Failed(_)),
            "QE `{fail_q}`: {qe_err:?}"
        );
    }
}

async fn collect_export(ctx: &QueryContext, query: &str, format: ExportFormat) -> Vec<u8> {
    let empty = Map::new();
    let mut stream = ctx.sql_export(req(query, &empty), format).await.unwrap();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.unwrap());
    }
    out
}

async fn collect_qe_export(ctx: &SqlEngineContext, query: &str, format: ExportFormat) -> Vec<u8> {
    let empty = Map::new();
    let mut stream = ctx.sql_export(req(query, &empty), format).await.unwrap();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.unwrap());
    }
    out
}
