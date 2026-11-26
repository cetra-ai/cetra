//! M3 differential parity tests against DataFusion (`ParquetSource`, `TracesSource`,
//! `ScoresSource`), plus spill, cancellation, and admission integration tests.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use arrow::array::{
    Array, BooleanArray, FixedSizeBinaryArray, Float64Array, Int32Array, Int64Array, ListArray,
    MapArray, RecordBatch, StringArray, TimestampNanosecondArray,
};
use arrow::compute::concat_batches;
use bytes::Bytes;
use cetra_column::{Batch, CancelToken, DataType, GlobalPool, ScalarValue, Schema};
use cetra_core::enums::SpanStatus;
use cetra_core::ids::{ProjectId, SpanId, TraceId};
use cetra_core::schema::{scores, spans, traces};
use cetra_core::score::Score;
use cetra_core::span::Span;
use cetra_query::exec::{
    CollectSink, ExecContext, ExecMetrics, IdLookupHint, ScanSource, ScanSpec, SortKey,
    SpillManager, TimeRangeHint, WorkerPool, run_pipeline,
};
use cetra_query::{BinaryOp, CastMode, EvalContext, Expr, QueryError};
use chrono::NaiveDate;
use datafusion::execution::context::{SessionConfig, SessionContext};
use datafusion::execution::object_store::ObjectStoreUrl;
use datafusion::execution::runtime_env::RuntimeEnv;
use futures::stream::BoxStream;
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use uuid::Uuid;

use crate::QueryOptions;
use crate::catalog::{Catalog, CatalogEdit, FileId, FileMeta, FileTable};
use crate::deletion::{self, PendingDeletion, Tombstones};
use crate::memtable::Memtable;
use crate::objects::OBJECT_STORE_URL;
use crate::parquet::write_file;
use crate::partials::partials_from_spans;
use crate::projects::ProjectStates;
use crate::query::TraceListRule;
use crate::read_state::ReadState;
use crate::test_support::{score_row, score_rows, span_row, span_rows};
use crate::verify::{Suspects, VerifyQueue};
use crate::views::QueryContext;
use crate::wal::Table;

use super::DAY_NS;
use super::admission::AsyncAdmissionPool;
use super::metadata_cache::MetadataCache;
use super::parquet_source::ParquetSource;
use super::scores_source::ScoresSource;
use super::traces_source::{TraceListMode, TracesSource};

fn project() -> ProjectId {
    "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
}

fn trace_id(last: u16) -> TraceId {
    let mut bytes = [0_u8; 16];
    let pair = (last + 1).to_be_bytes();
    for chunk in bytes.as_chunks_mut::<2>().0 {
        chunk.copy_from_slice(&pair);
    }
    TraceId::from_bytes(bytes).unwrap()
}

fn span_id(last: u8) -> SpanId {
    SpanId::from_bytes([last.max(1); 8]).unwrap()
}

fn day_of(ns: i64) -> NaiveDate {
    super::day_of_ns(ns).unwrap()
}

fn make_ctx(cap: usize, spill_cap: u64, partitions: usize) -> (ExecContext, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ExecContext {
        pool: GlobalPool::standalone(cap),
        eval: EvalContext {
            now_ns: 0,
            cancel: CancelToken::new(),
        },
        spill: Arc::new(SpillManager::with_fixed_cap(
            dir.path().to_path_buf(),
            spill_cap,
        )),
        workers: Arc::new(WorkerPool::new(partitions.max(1)).unwrap()),
        batch_size: 128,
        target_partitions: partitions,
        metrics: ExecMetrics::default(),
    };
    (ctx, dir)
}

fn run_source(
    source: &dyn ScanSource,
    spec: &ScanSpec,
    ctx: &ExecContext,
) -> Result<Vec<Batch>, QueryError> {
    let partitions = source.open_partitions(spec, ctx)?;
    if partitions.is_empty() {
        return Ok(Vec::new());
    }
    let pool = Arc::clone(&ctx.pool);
    let sinks = run_pipeline(
        ctx,
        partitions,
        |_| Ok(Vec::new()),
        move |_| Ok(CollectSink::new(&pool)),
    )?;
    Ok(sinks
        .into_iter()
        .flat_map(CollectSink::into_batches)
        .collect())
}

fn write_table_file(
    rt: &tokio::runtime::Runtime,
    store: &Arc<dyn ObjectStore>,
    table: FileTable,
    day: NaiveDate,
    path: &str,
    batch: RecordBatch,
    deletions_applied_seq: u64,
) -> Arc<FileMeta> {
    let written = write_file(table, 0, &[Arc::new(batch)]).unwrap();
    let size = written.bytes.len() as u64;
    rt.block_on(store.put(&ObjectPath::from(path), PutPayload::from(written.bytes)))
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
        deletions_applied_seq,
    })
}

fn build_catalog(files: &[Arc<FileMeta>]) -> Arc<Catalog> {
    let edit = CatalogEdit {
        add: files.iter().map(|f| (**f).clone()).collect(),
        remove: Vec::new(),
        flushed_seq: None,
        sources: Vec::new(),
        raise: Vec::new(),
        quarantine: Vec::new(),
    };
    Arc::new(Catalog::empty().apply(&edit).unwrap().0)
}

fn register_store_runtime(store: &Arc<dyn ObjectStore>) -> Arc<RuntimeEnv> {
    let runtime = Arc::new(RuntimeEnv::default());
    let url = ObjectStoreUrl::parse(OBJECT_STORE_URL).unwrap();
    runtime.register_object_store(url.as_ref(), Arc::clone(store));
    runtime
}

fn df_sql_raw(
    rt: &tokio::runtime::Runtime,
    state: &ReadState,
    runtime: &Arc<RuntimeEnv>,
    options: QueryOptions,
    lookback: i64,
    partitions: usize,
    sql: &str,
) -> (Vec<RecordBatch>, String) {
    let verify = Arc::new(VerifyQueue::default());
    let pq = crate::project_query(
        state,
        project(),
        options,
        runtime,
        NonZeroUsize::new(partitions.max(1)).unwrap(),
        lookback,
        &verify,
    )
    .unwrap();
    let explain_sql = format!("EXPLAIN {sql}");
    rt.block_on(async move {
        let qctx = QueryContext::new(pq, None).unwrap();
        let explain_batches = qctx
            .session()
            .sql(&explain_sql)
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let explain = explain_batches
            .iter()
            .flat_map(|b| {
                b.column(1)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .map(|a| {
                        (0..a.len())
                            .map(|i| a.value(i).to_owned())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let rows = qctx
            .session()
            .sql(sql)
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        (rows, explain)
    })
}

fn df_provider_sql(
    rt: &tokio::runtime::Runtime,
    state: &ReadState,
    runtime: &Arc<RuntimeEnv>,
    options: QueryOptions,
    lookback: i64,
    table_name: &str,
    sql: &str,
) -> Vec<RecordBatch> {
    const PARTITIONS: usize = 2;
    let verify = Arc::new(VerifyQueue::default());
    let pq = crate::project_query(
        state,
        project(),
        options,
        runtime,
        NonZeroUsize::new(PARTITIONS).unwrap(),
        lookback,
        &verify,
    )
    .unwrap();
    let table = match table_name {
        "spans" => pq.spans,
        "traces" => pq.traces,
        "scores" => pq.scores,
        other => panic!("unexpected table {other}"),
    };
    rt.block_on(async move {
        let mut session_cfg = SessionConfig::new().with_target_partitions(PARTITIONS);
        session_cfg.options_mut().execution.target_partitions = PARTITIONS;
        let session_state = datafusion::execution::SessionStateBuilder::new()
            .with_config(session_cfg)
            .with_runtime_env(Arc::clone(&pq.runtime))
            .with_default_features()
            .with_physical_optimizer_rule(Arc::new(TraceListRule))
            .build();
        let ctx = SessionContext::new_with_state(session_state);
        ctx.register_table(table_name, table).unwrap();
        ctx.sql(sql).await.unwrap().collect().await.unwrap()
    })
}

#[derive(Debug, Clone, PartialEq)]
enum CellVal {
    Null,
    Bool(bool),
    I64(i64),
    F64(u64),
    Utf8(String),
    Bytes(Vec<u8>),
    List(Vec<String>),
    Map(Vec<(String, Option<String>)>),
}

fn round_cost_key(v: f64) -> u64 {
    // Round to 1e-12 precision for order-independent comparison matching partials.rs.
    let quantized = (v * 1e12).round();
    quantized.to_bits()
}

fn batch_rows_from_column(batches: &[Batch]) -> Vec<Vec<CellVal>> {
    let mut rows = Vec::new();
    for batch in batches {
        let compacted = batch.compact(None).unwrap();
        for r in 0..compacted.num_rows() {
            let mut row = Vec::with_capacity(compacted.columns().len());
            for col in compacted.columns() {
                let sv = col.get_scalar(r).unwrap();
                let cv = match sv {
                    ScalarValue::Null(_) => CellVal::Null,
                    ScalarValue::Bool(b) => CellVal::Bool(b),
                    ScalarValue::I64(v) | ScalarValue::TimestampNs { value: v, .. } => {
                        CellVal::I64(v)
                    }
                    ScalarValue::F64(v) => CellVal::F64(round_cost_key(v)),
                    ScalarValue::Utf8(s) => CellVal::Utf8(s),
                    ScalarValue::Binary(b) => CellVal::Bytes(b),
                    ScalarValue::FixedBytes8(b) => CellVal::Bytes(b.to_vec()),
                    ScalarValue::FixedBytes16(b) => CellVal::Bytes(b.to_vec()),
                    ScalarValue::List { values, .. } => CellVal::List(
                        values
                            .into_iter()
                            .filter_map(|v| match v {
                                ScalarValue::Utf8(s) => Some(s),
                                _ => None,
                            })
                            .collect(),
                    ),
                    ScalarValue::MapUtf8(entries) => CellVal::Map(entries),
                    other => panic!("unexpected scalar in diff_tests: {other:?}"),
                };
                row.push(cv);
            }
            rows.push(row);
        }
    }
    rows
}

fn batch_rows_from_arrow(batches: &[RecordBatch]) -> Vec<Vec<CellVal>> {
    if batches.is_empty() {
        return Vec::new();
    }
    let schema = batches[0].schema();
    let combined = concat_batches(&schema, batches).unwrap();
    let mut rows = Vec::with_capacity(combined.num_rows());
    for r in 0..combined.num_rows() {
        let mut row = Vec::with_capacity(combined.num_columns());
        for col in combined.columns() {
            if col.is_null(r) {
                row.push(CellVal::Null);
                continue;
            }
            let cv = match col.data_type() {
                arrow::datatypes::DataType::Boolean => CellVal::Bool(
                    col.as_any()
                        .downcast_ref::<BooleanArray>()
                        .unwrap()
                        .value(r),
                ),
                arrow::datatypes::DataType::Int32 => CellVal::I64(i64::from(
                    col.as_any().downcast_ref::<Int32Array>().unwrap().value(r),
                )),
                arrow::datatypes::DataType::Int64 => {
                    CellVal::I64(col.as_any().downcast_ref::<Int64Array>().unwrap().value(r))
                }
                arrow::datatypes::DataType::Timestamp(_, _) => CellVal::I64(
                    col.as_any()
                        .downcast_ref::<TimestampNanosecondArray>()
                        .unwrap()
                        .value(r),
                ),
                arrow::datatypes::DataType::Float64 => CellVal::F64(round_cost_key(
                    col.as_any()
                        .downcast_ref::<Float64Array>()
                        .unwrap()
                        .value(r),
                )),
                arrow::datatypes::DataType::Utf8 => CellVal::Utf8(
                    col.as_any()
                        .downcast_ref::<StringArray>()
                        .unwrap()
                        .value(r)
                        .to_owned(),
                ),
                arrow::datatypes::DataType::FixedSizeBinary(_) => CellVal::Bytes(
                    col.as_any()
                        .downcast_ref::<FixedSizeBinaryArray>()
                        .unwrap()
                        .value(r)
                        .to_vec(),
                ),
                arrow::datatypes::DataType::List(_) => {
                    let list = col.as_any().downcast_ref::<ListArray>().unwrap();
                    let val = list.value(r);
                    let strs = val.as_any().downcast_ref::<StringArray>().unwrap();
                    CellVal::List((0..strs.len()).map(|i| strs.value(i).to_owned()).collect())
                }
                arrow::datatypes::DataType::Map(_, _) => {
                    let map = col.as_any().downcast_ref::<MapArray>().unwrap();
                    let entries = map.value(r);
                    let keys = entries
                        .column(0)
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .unwrap();
                    let vals = entries
                        .column(1)
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .unwrap();
                    CellVal::Map(
                        (0..entries.len())
                            .map(|i| {
                                (
                                    keys.value(i).to_owned(),
                                    (!vals.is_null(i)).then(|| vals.value(i).to_owned()),
                                )
                            })
                            .collect(),
                    )
                }
                other => panic!("unexpected arrow type {other:?}"),
            };
            row.push(cv);
        }
        rows.push(row);
    }
    rows
}

fn sort_cell_rows(rows: &mut [Vec<CellVal>]) {
    rows.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
}

fn assert_unordered_parity(native: &[Batch], df: &[RecordBatch], label: &str) {
    let mut left = batch_rows_from_column(native);
    let mut right = batch_rows_from_arrow(df);
    sort_cell_rows(&mut left);
    sort_cell_rows(&mut right);
    assert_eq!(left, right, "parity mismatch for {label}");
}

fn assert_ordered_parity(native: &[Batch], df: &[RecordBatch], label: &str) {
    let left = batch_rows_from_column(native);
    let right = batch_rows_from_arrow(df);
    assert_eq!(left, right, "ordered parity mismatch for {label}");
}

fn col_expr(schema: &Schema, name: &str) -> Expr {
    let idx = schema.index_of(name).unwrap();
    Expr::Column {
        index: idx,
        name: name.to_owned(),
        data_type: schema.fields()[idx].data_type.clone(),
    }
}

// ---------------------------------------------------------------------------
// 1. ParquetSource parity across spans, traces, scores with tombstones & bloom
// ---------------------------------------------------------------------------

#[test]
fn parquet_source_parity_against_datafusion_spans_traces_scores() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let runtime = register_store_runtime(&store);
    let cache = Arc::new(MetadataCache::new(4 * 1024 * 1024));
    let suspects = Suspects::new(Arc::new(VerifyQueue::default()));

    let base = 20_000 * DAY_NS;
    let day0 = day_of(base);
    let day1 = day_of(base + DAY_NS);

    let mut s1 = span_row(1, 1, base + 100, base + 500);
    s1.user_id = Some("alice".to_owned());
    s1.input = Some("prompt-1".to_owned());
    s1.status = SpanStatus::Ok;

    let mut s2 = span_row(2, 1, base + 200, base + 500);
    s2.user_id = Some("bob".to_owned());
    s2.input = Some("prompt-2".to_owned());
    s2.status = SpanStatus::Error;

    let mut s3 = span_row(3, 1, base + DAY_NS + 300, base + 900);
    s3.user_id = Some("carol".to_owned());
    s3.input = Some("prompt-3".to_owned());
    s3.status = SpanStatus::Ok;

    let f_span0 = write_table_file(
        &rt,
        &store,
        FileTable::Spans,
        day0,
        "tables/spans/d0.parquet",
        span_rows(&[s1.clone(), s2.clone()]),
        0,
    );
    let f_span1 = write_table_file(
        &rt,
        &store,
        FileTable::Spans,
        day1,
        "tables/spans/d1.parquet",
        span_rows(&[s3.clone()]),
        1, // seq 1 already applied on file 1
    );

    let p_trace0 = partials_from_spans(&span_rows(&[s1, s2])).unwrap();
    let p_trace1 = partials_from_spans(&span_rows(&[s3])).unwrap();
    let f_trace0 = write_table_file(
        &rt,
        &store,
        FileTable::Traces,
        day0,
        "tables/traces/d0.parquet",
        p_trace0,
        0,
    );
    let f_trace1 = write_table_file(
        &rt,
        &store,
        FileTable::Traces,
        day1,
        "tables/traces/d1.parquet",
        p_trace1,
        1,
    );

    let sc1 = score_row(1, 1, base + 100, base + 200, base + 500, false, "acc");
    let sc2 = score_row(2, 2, base + 100, base + 200, base + 500, false, "rel");
    let mut sc3 = score_row(
        3,
        3,
        base + DAY_NS + 100,
        base + 200,
        base + 900,
        false,
        "help",
    );
    sc3.session_id = Some("sess-erased".to_owned());
    let f_score0 = write_table_file(
        &rt,
        &store,
        FileTable::Scores,
        day0,
        "tables/scores/d0.parquet",
        score_rows(&[sc1, sc2]),
        0,
    );
    let f_score1 = write_table_file(
        &rt,
        &store,
        FileTable::Scores,
        day1,
        "tables/scores/d1.parquet",
        score_rows(&[sc3]),
        0,
    );

    // Deletion seq 1: erases user "bob" (trace 2) with cutoff base + 600.
    // Deletion seq 2: erases session "sess-erased" with cutoff base + 1000.
    let d1 = PendingDeletion {
        id: Uuid::now_v7(),
        seq: 1,
        project: project(),
        cutoff: base + 600,
        registered_at: base + 600,
        trace_ids: [trace_id(2).as_bytes().to_owned()].into_iter().collect(),
        users: ["bob".to_owned()].into_iter().collect(),
        sessions: HashSet::new(),
    };
    let d2 = PendingDeletion {
        id: Uuid::now_v7(),
        seq: 2,
        project: project(),
        cutoff: base + 1000,
        registered_at: base + 1000,
        trace_ids: HashSet::new(),
        users: HashSet::new(),
        sessions: ["sess-erased".to_owned()].into_iter().collect(),
    };
    let tombstones = Tombstones::default().with(d1).with(d2);
    let deletions = tombstones.for_project(project());

    let catalog = build_catalog(&[
        Arc::clone(&f_span0),
        Arc::clone(&f_span1),
        Arc::clone(&f_trace0),
        Arc::clone(&f_trace1),
        Arc::clone(&f_score0),
        Arc::clone(&f_score1),
    ]);
    let state = ReadState {
        spans: Memtable::new(Table::Spans).snapshot(),
        scores: Memtable::new(Table::Scores).snapshot(),
        catalog,
        tombstones: Arc::new(tombstones),
        projects: Arc::new(ProjectStates::default()),
    };

    let (exec, _dir) = make_ctx(8 * 1024 * 1024, 16 * 1024 * 1024, 2);

    // Spans parity: bloom lookup + late materialization of `input` + tombstones.
    let span_schema = Arc::new(
        crate::column_bridge::arrow_schema_to_column(&cetra_core::schema::spans_schema()).unwrap(),
    );
    let span_src = ParquetSource::new(
        Arc::clone(&store),
        FileTable::Spans,
        Arc::clone(&span_schema),
        vec![f_span0, f_span1],
        Arc::clone(&deletions),
        Arc::clone(&cache),
        suspects.clone(),
        rt.handle().clone(),
    );

    let span_proj = vec![
        span_schema.index_of(spans::TRACE_ID).unwrap(),
        span_schema.index_of(spans::USER_ID).unwrap(),
        span_schema.index_of(spans::INPUT).unwrap(),
        span_schema.index_of(spans::STATUS).unwrap(),
    ];
    let span_spec = ScanSpec {
        projection: Some(span_proj),
        ..Default::default()
    };
    let native_spans = run_source(&span_src, &span_spec, &exec).unwrap();
    let df_spans = df_provider_sql(
        &rt,
        &state,
        &runtime,
        QueryOptions::new(None, None),
        0,
        "spans",
        "SELECT trace_id, user_id, input, status FROM spans",
    );
    assert_unordered_parity(&native_spans, &df_spans, "spans all");

    // Bloom lookup on trace 3 with `around_ns` on day 1.
    let t3_bytes = *trace_id(3).as_bytes();
    let span_lookup_spec = ScanSpec {
        projection: Some(vec![
            span_schema.index_of(spans::TRACE_ID).unwrap(),
            span_schema.index_of(spans::INPUT).unwrap(),
        ]),
        time_range: TimeRangeHint {
            min_ns: None,
            max_ns: None,
            around_ns: Some(base + DAY_NS + 300),
        },
        id_lookups: vec![IdLookupHint::Fixed16 {
            column: spans::TRACE_ID.to_owned(),
            values: vec![t3_bytes],
        }],
        filters: vec![Expr::Binary {
            left: Box::new(col_expr(&span_schema, spans::TRACE_ID)),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::FixedBytes16(t3_bytes))),
            data_type: DataType::Bool,
        }],
        ..Default::default()
    };
    let native_lookup = run_source(&span_src, &span_lookup_spec, &exec).unwrap();
    let df_lookup = df_provider_sql(
        &rt,
        &state,
        &runtime,
        QueryOptions::new(Some(base + DAY_NS + 300), None),
        0,
        "spans",
        &format!(
            "SELECT trace_id, input FROM spans WHERE trace_id = arrow_cast(X'{}', 'FixedSizeBinary(16)')",
            trace_id(3).to_hex()
        ),
    );
    assert_unordered_parity(&native_lookup, &df_lookup, "spans bloom + around_ns");

    // Empty projection `Some(vec![])` row count parity.
    let empty_proj_spec = ScanSpec {
        projection: Some(Vec::new()),
        ..Default::default()
    };
    let empty_batches = run_source(&span_src, &empty_proj_spec, &exec).unwrap();
    let empty_count: usize = empty_batches.iter().map(Batch::num_rows).sum();
    assert_eq!(empty_count, 2);

    // Scores ParquetSource raw parity (session tombstone + day pruning without around_ns).
    let score_schema = Arc::new(
        crate::column_bridge::arrow_schema_to_column(&cetra_core::schema::scores_schema()).unwrap(),
    );
    let score_src = ParquetSource::new(
        Arc::clone(&store),
        FileTable::Scores,
        Arc::clone(&score_schema),
        vec![f_score0, f_score1],
        Arc::clone(&deletions),
        Arc::clone(&cache),
        suspects.clone(),
        rt.handle().clone(),
    );
    let score_spec = ScanSpec {
        projection: Some(vec![
            score_schema.index_of(scores::SCORE_ID).unwrap(),
            score_schema.index_of(scores::NAME).unwrap(),
        ]),
        time_range: TimeRangeHint {
            min_ns: None,
            max_ns: None,
            around_ns: Some(base + DAY_NS),
        },
        id_lookups: vec![IdLookupHint::Fixed16 {
            column: scores::TRACE_ID.to_owned(),
            values: vec![*trace_id(1).as_bytes()],
        }],
        ..Default::default()
    };
    let native_raw_scores = run_source(&score_src, &score_spec, &exec).unwrap();
    let df_scores = df_provider_sql(
        &rt,
        &state,
        &runtime,
        QueryOptions::new(Some(base + DAY_NS), None),
        0,
        "scores",
        "SELECT score_id, name FROM scores",
    );
    assert_unordered_parity(
        &native_raw_scores,
        &df_scores,
        "scores with session tombstone",
    );

    // Traces ParquetSource raw parity (`f_trace0` / `f_trace1`, tombstones + around_ns lookup).
    let trace_schema = Arc::new(
        crate::column_bridge::arrow_schema_to_column(&cetra_core::schema::traces_schema()).unwrap(),
    );
    let trace_src = ParquetSource::new(
        Arc::clone(&store),
        FileTable::Traces,
        Arc::clone(&trace_schema),
        vec![f_trace0, f_trace1],
        Arc::clone(&deletions),
        Arc::clone(&cache),
        suspects,
        rt.handle().clone(),
    );
    let trace_spec = ScanSpec {
        projection: Some(vec![
            trace_schema.index_of(traces::TRACE_ID).unwrap(),
            trace_schema.index_of(traces::USER_ID).unwrap(),
            trace_schema.index_of(traces::STATUS).unwrap(),
            trace_schema.index_of(traces::SPAN_COUNT).unwrap(),
        ]),
        ..Default::default()
    };
    let native_raw_traces = run_source(&trace_src, &trace_spec, &exec).unwrap();
    let df_raw_traces = df_provider_sql(
        &rt,
        &state,
        &runtime,
        QueryOptions::new(None, None),
        0,
        "traces",
        "SELECT trace_id, user_id, status, span_count FROM traces",
    );
    assert_unordered_parity(
        &native_raw_traces,
        &df_raw_traces,
        "traces parquet raw with tombstone",
    );

    let trace_lookup_spec = ScanSpec {
        projection: Some(vec![
            trace_schema.index_of(traces::TRACE_ID).unwrap(),
            trace_schema.index_of(traces::USER_ID).unwrap(),
        ]),
        time_range: TimeRangeHint {
            min_ns: None,
            max_ns: None,
            around_ns: Some(base + DAY_NS + 300),
        },
        id_lookups: vec![IdLookupHint::Fixed16 {
            column: traces::TRACE_ID.to_owned(),
            values: vec![t3_bytes],
        }],
        filters: vec![Expr::Binary {
            left: Box::new(col_expr(&trace_schema, traces::TRACE_ID)),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::FixedBytes16(t3_bytes))),
            data_type: DataType::Bool,
        }],
        ..Default::default()
    };
    let native_trace_lookup = run_source(&trace_src, &trace_lookup_spec, &exec).unwrap();
    let df_trace_lookup = df_provider_sql(
        &rt,
        &state,
        &runtime,
        QueryOptions::new(Some(base + DAY_NS + 300), None),
        0,
        "traces",
        &format!(
            "SELECT trace_id, user_id FROM traces WHERE trace_id = arrow_cast(X'{}', 'FixedSizeBinary(16)')",
            trace_id(3).to_hex()
        ),
    );
    assert_unordered_parity(
        &native_trace_lookup,
        &df_trace_lookup,
        "traces parquet lookup + around_ns",
    );
}

// ---------------------------------------------------------------------------
// 2. TracesSource parity: range, lookup, widening-window list, and TwoPhase
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
#[test]
fn traces_source_parity_range_lookup_list_and_two_phase() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let runtime = register_store_runtime(&store);
    let cache = Arc::new(MetadataCache::new(4 * 1024 * 1024));
    let suspects = Suspects::new(Arc::new(VerifyQueue::default()));

    let lookback = 60 * 1_000_000_000; // 60s
    let base = 20_000 * DAY_NS;
    let day0 = day_of(base);
    let day1 = day_of(base + DAY_NS);

    // Trace 1: split across file 0 (non-root partial), file 1 (root partial + error), and memtable!
    let mut t1_s1 = span_row(1, 1, base - 2 * 60 * 1_000_000_000, base); // start_time = base - 120s
    t1_s1.parent_span_id = Some(span_id(99));
    t1_s1.name = "early-child".to_owned();
    t1_s1.cost_total = Some(0.10);
    t1_s1.total_tokens = Some(100);
    t1_s1.tags = vec!["alpha".to_owned()];

    let mut t1_s2 = span_row(1, 2, base + 10_000, base + 50_000);
    t1_s2.parent_span_id = None; // root!
    t1_s2.name = "t1-root".to_owned();
    t1_s2.status = SpanStatus::Ok;
    t1_s2.cost_total = Some(0.25);
    t1_s2.total_tokens = Some(200);
    t1_s2.end_time = base + 10_000 + 500;

    let mut t1_s3 = span_row(1, 3, base + 20_000, base + 60_000);
    t1_s3.parent_span_id = Some(span_id(2));
    t1_s3.status = SpanStatus::Error; // forces status = "error"!
    t1_s3.cost_total = Some(0.05);
    t1_s3.total_tokens = Some(50);
    t1_s3.tags = vec!["beta".to_owned()];
    t1_s3.end_time = base + 20_000 + 9_000;

    // Trace 2: two non-root partials with same `start_time` (tie-broken lexicographically by value).
    let mut t2_s1 = span_row(2, 1, base - 10_000_000_000, base); // within lookback of `base`
    t2_s1.parent_span_id = Some(span_id(9));
    t2_s1.name = "z-second".to_owned();
    t2_s1.cost_total = Some(1.50);
    t2_s1.total_tokens = Some(1000);
    t2_s1.end_time = base - 10_000_000_000 + 2_000;

    let mut t2_s2 = span_row(2, 2, base - 10_000_000_000, base);
    t2_s2.parent_span_id = Some(span_id(9));
    t2_s2.name = "a-first".to_owned();
    t2_s2.cost_total = Some(0.50);
    t2_s2.total_tokens = Some(500);
    t2_s2.end_time = base - 10_000_000_000 + 5_000;

    // Trace 3: deleted by pending deletion seq 1 (file 0 has deletions_applied_seq = 0).
    let t3_s1 = span_row(3, 1, base + 100_000, base + 100);

    // Trace 4..=8: spread across the 5 widening-window slices relative to `anchor = base + 2 * DAY_NS`.
    let anchor = base + 2 * DAY_NS;
    let mut t4 = span_row(4, 1, anchor - 5 * 60 * 1_000_000_000, anchor); // 5m ago (slice 0)
    t4.cost_total = Some(0.20);
    t4.total_tokens = Some(40);
    t4.end_time = t4.start_time + 100;

    let mut t5 = span_row(5, 1, anchor - 30 * 60 * 1_000_000_000, anchor); // 30m ago (slice 1)
    t5.cost_total = Some(3.00);
    t5.total_tokens = Some(3000);
    t5.end_time = t5.start_time + 80_000;

    let mut t6 = span_row(6, 1, anchor - 3 * 3600 * 1_000_000_000, anchor); // 3h ago (slice 2)
    t6.cost_total = Some(0.80);
    t6.total_tokens = Some(800);
    t6.end_time = t6.start_time + 40_000;

    let mut t7 = span_row(7, 1, anchor - 12 * 3600 * 1_000_000_000, anchor); // 12h ago (slice 3)
    t7.cost_total = None; // NULL cost_total!
    t7.total_tokens = Some(10);
    t7.end_time = t7.start_time + 5_000;

    let mut t8 = span_row(8, 1, anchor - 36 * 3600 * 1_000_000_000, anchor); // 36h ago (slice 4)
    t8.cost_total = Some(5.00);
    t8.total_tokens = Some(5000);
    t8.end_time = t8.start_time + 200_000;

    // Trace 9: in file 2 with `deletions_applied_seq = 1` (re-ingested after deletion 1's cutoff!).
    // Wait: trace 3 was erased by deletion 1, trace 9 is NOT in deletion 1, or trace 10 IS in deletion 1
    // AND re-ingested in file 2 with deletions_applied_seq = 1 so file 2 keeps trace 10!
    let mut t10_old = span_row(10, 1, base + 5_000, base + 100);
    t10_old.name = "erased-old".to_owned();
    let mut t10_new = span_row(10, 2, base + DAY_NS + 5_000, base + 2_000);
    t10_new.name = "reingested-live".to_owned();
    t10_new.cost_total = Some(0.42);

    let f0 = write_table_file(
        &rt,
        &store,
        FileTable::Traces,
        day0,
        "tables/traces/f0.parquet",
        partials_from_spans(&span_rows(&[t1_s1, t2_s1, t3_s1, t10_old])).unwrap(),
        0,
    );
    let f1 = write_table_file(
        &rt,
        &store,
        FileTable::Traces,
        day1,
        "tables/traces/f1.parquet",
        partials_from_spans(&span_rows(&[t1_s2, t2_s2, t7, t8])).unwrap(),
        0,
    );
    let f2 = write_table_file(
        &rt,
        &store,
        FileTable::Traces,
        day_of(anchor),
        "tables/traces/f2.parquet",
        partials_from_spans(&span_rows(&[t4, t5, t6, t10_new])).unwrap(),
        1, // deletion seq 1 already applied!
    );

    // Put `t1_s3` and a span for erased user in spans memtable, then project_snapshot!
    let mut t_mem_erased = span_row(11, 1, anchor, base + 100);
    t_mem_erased.user_id = Some("erased-user".to_owned());

    let mut spans_mt = Memtable::new(Table::Spans);
    spans_mt
        .insert(project(), span_rows(&[t1_s3, t_mem_erased]), 1)
        .unwrap();
    let raw_spans_mem = spans_mt.snapshot();

    let d1 = PendingDeletion {
        id: Uuid::now_v7(),
        seq: 1,
        project: project(),
        cutoff: base + 1_000,
        registered_at: base + 1_000,
        trace_ids: [
            trace_id(3).as_bytes().to_owned(),
            trace_id(10).as_bytes().to_owned(),
        ]
        .into_iter()
        .collect(),
        users: ["erased-user".to_owned()].into_iter().collect(),
        sessions: HashSet::new(),
    };
    let tombstones = Tombstones::default().with(d1);
    let deletions = tombstones.for_project(project());

    let filtered_spans_mem =
        deletion::project_snapshot(Table::Spans, &raw_spans_mem, project(), None, &deletions)
            .unwrap();

    let files = vec![f0, f1, f2];
    let catalog = build_catalog(&files);
    let state = ReadState {
        spans: Arc::clone(&filtered_spans_mem),
        scores: Memtable::new(Table::Scores).snapshot(),
        catalog,
        tombstones: Arc::new(tombstones),
        projects: Arc::new(ProjectStates::default()),
    };

    let (exec, _dir) = make_ctx(8 * 1024 * 1024, 16 * 1024 * 1024, 2);
    let traces_src = TracesSource::new(
        Arc::clone(&store),
        project(),
        files,
        Arc::clone(&filtered_spans_mem),
        lookback,
        Arc::clone(&deletions),
        Arc::clone(&cache),
        suspects,
        rt.handle().clone(),
    )
    .unwrap();
    let schema = traces_src.schema();

    // 2a. Full range scan (all 30 columns) parity!
    let full_spec = ScanSpec::default();
    assert!(
        traces_src
            .explain_summary(&full_spec)
            .contains("mode=range")
    );
    let native_all = run_source(&traces_src, &full_spec, &exec).unwrap();
    let df_all = df_provider_sql(
        &rt,
        &state,
        &runtime,
        QueryOptions::new(None, None),
        lookback,
        "traces",
        "SELECT * FROM traces",
    );
    assert_unordered_parity(&native_all, &df_all, "traces full range all 30 cols");

    // 2b. Lookback boundary check:
    // Query `start_time >= base - 15s AND start_time <= anchor`.
    // Trace 1 has a span at `base - 120s`, so merged start_time = `base - 120s` (< `base - 15s`) -> MUST be excluded!
    // Trace 2 has merged start_time = `base - 10s` (>= `base - 15s`) -> MUST be included!
    let lo_bound = base - 15 * 1_000_000_000;
    let hi_bound = anchor;
    let range_spec = ScanSpec {
        projection: Some(vec![
            schema.index_of(traces::TRACE_ID).unwrap(),
            schema.index_of(traces::NAME).unwrap(),
            schema.index_of(traces::STATUS).unwrap(),
            schema.index_of(traces::COST_TOTAL).unwrap(),
            schema.index_of(traces::TAGS).unwrap(),
        ]),
        time_range: TimeRangeHint {
            min_ns: Some(lo_bound),
            max_ns: Some(hi_bound),
            around_ns: None,
        },
        ..Default::default()
    };
    let native_window = run_source(&traces_src, &range_spec, &exec).unwrap();
    let df_window = df_provider_sql(
        &rt,
        &state,
        &runtime,
        QueryOptions::new(None, None),
        lookback,
        "traces",
        &format!(
            "SELECT trace_id, name, status, cost_total, tags FROM traces WHERE start_time >= arrow_cast({lo_bound}, 'Timestamp(Nanosecond, Some(\"UTC\"))') AND start_time <= arrow_cast({hi_bound}, 'Timestamp(Nanosecond, Some(\"UTC\"))')"
        ),
    );
    assert_unordered_parity(
        &native_window,
        &df_window,
        "traces lookback boundary window",
    );

    // 2c. Post-merge filter parity (`status = 'error'`).
    let status_spec = ScanSpec {
        projection: Some(vec![
            schema.index_of(traces::TRACE_ID).unwrap(),
            schema.index_of(traces::COST_TOTAL).unwrap(),
        ]),
        filters: vec![Expr::Binary {
            left: Box::new(col_expr(&schema, traces::STATUS)),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::Utf8("error".to_owned()))),
            data_type: DataType::Bool,
        }],
        ..Default::default()
    };
    let native_err = run_source(&traces_src, &status_spec, &exec).unwrap();
    let df_err = df_provider_sql(
        &rt,
        &state,
        &runtime,
        QueryOptions::new(None, None),
        lookback,
        "traces",
        "SELECT trace_id, cost_total FROM traces WHERE status = 'error'",
    );
    assert_unordered_parity(&native_err, &df_err, "traces post-merge status=error");

    // 2d. Direct `mode=lookup` parity (`trace_id = t1` with `around_ns`).
    let t1_bytes = *trace_id(1).as_bytes();
    let lookup_spec = ScanSpec {
        projection: Some(vec![
            schema.index_of(traces::TRACE_ID).unwrap(),
            schema.index_of(traces::ROOT_SPAN_ID).unwrap(),
            schema.index_of(traces::HAS_ROOT).unwrap(),
            schema.index_of(traces::STATUS).unwrap(),
            schema.index_of(traces::SPAN_COUNT).unwrap(),
            schema.index_of(traces::COST_TOTAL).unwrap(),
        ]),
        time_range: TimeRangeHint {
            min_ns: None,
            max_ns: None,
            around_ns: Some(base),
        },
        id_lookups: vec![IdLookupHint::Fixed16 {
            column: traces::TRACE_ID.to_owned(),
            values: vec![t1_bytes],
        }],
        filters: vec![Expr::Binary {
            left: Box::new(col_expr(&schema, traces::TRACE_ID)),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::FixedBytes16(t1_bytes))),
            data_type: DataType::Bool,
        }],
        ..Default::default()
    };
    assert!(
        traces_src
            .explain_summary(&lookup_spec)
            .contains("mode=lookup")
    );
    let native_lookup = run_source(&traces_src, &lookup_spec, &exec).unwrap();
    let df_lookup = df_provider_sql(
        &rt,
        &state,
        &runtime,
        QueryOptions::new(Some(base), None),
        lookback,
        "traces",
        &format!(
            "SELECT trace_id, root_span_id, has_root, status, span_count, cost_total FROM traces WHERE trace_id = arrow_cast(X'{}', 'FixedSizeBinary(16)')",
            trace_id(1).to_hex()
        ),
    );
    assert_unordered_parity(&native_lookup, &df_lookup, "traces lookup mode");

    // 2e. Widening-window 5-slice `mode=list` parity (`fetch = 3` and `fetch = 6`)!
    for fetch in [3usize, 6usize] {
        let list_src = traces_src
            .clone()
            .with_list_mode(TraceListMode::Widening { fetch });
        let list_spec = ScanSpec {
            projection: Some(vec![
                schema.index_of(traces::TRACE_ID).unwrap(),
                schema.index_of(traces::START_TIME).unwrap(),
                schema.index_of(traces::NAME).unwrap(),
            ]),
            ..Default::default()
        };
        assert!(list_src.explain_summary(&list_spec).contains("mode=list"));
        let native_list = run_source(&list_src, &list_spec, &exec).unwrap();
        let (_, df_explain) = df_sql_raw(
            &rt,
            &state,
            &runtime,
            QueryOptions::new(None, None),
            lookback,
            2,
            &format!(
                "SELECT trace_id, start_time, name FROM traces ORDER BY start_time DESC, trace_id DESC LIMIT {fetch}"
            ),
        );
        assert!(
            df_explain.contains("mode=list"),
            "expected DataFusion TraceListRule to fire: {df_explain}"
        );
        let df_list = df_provider_sql(
            &rt,
            &state,
            &runtime,
            QueryOptions::new(None, None),
            lookback,
            "traces",
            &format!(
                "SELECT trace_id, start_time, name FROM traces ORDER BY start_time DESC, trace_id DESC LIMIT {fetch}"
            ),
        );
        assert_ordered_parity(
            &native_list,
            &df_list,
            &format!("widening list fetch={fetch}"),
        );
    }

    // 2f. `TraceListMode::TwoPhase` parity for `cost_total`, `duration_ns`, `total_tokens` with `offset` & `limit`!
    let sort_cases: Vec<(&str, Vec<SortKey>, &str)> = vec![
        (
            "cost_total DESC NULLS LAST",
            vec![
                SortKey {
                    expr: col_expr(&schema, traces::COST_TOTAL),
                    descending: true,
                    nulls_first: false,
                },
                SortKey {
                    expr: col_expr(&schema, traces::TRACE_ID),
                    descending: true,
                    nulls_first: false,
                },
            ],
            "SELECT trace_id, name, cost_total FROM traces ORDER BY cost_total DESC NULLS LAST, trace_id DESC LIMIT 3 OFFSET 1",
        ),
        (
            "duration_ns DESC NULLS LAST",
            vec![
                SortKey {
                    expr: Expr::Binary {
                        left: Box::new(Expr::Cast {
                            expr: Box::new(col_expr(&schema, traces::END_TIME)),
                            target: DataType::I64,
                            mode: CastMode::Strict,
                        }),
                        op: BinaryOp::Sub,
                        right: Box::new(Expr::Cast {
                            expr: Box::new(col_expr(&schema, traces::START_TIME)),
                            target: DataType::I64,
                            mode: CastMode::Strict,
                        }),
                        data_type: DataType::I64,
                    },
                    descending: true,
                    nulls_first: false,
                },
                SortKey {
                    expr: col_expr(&schema, traces::TRACE_ID),
                    descending: true,
                    nulls_first: false,
                },
            ],
            "SELECT trace_id, name, cost_total FROM traces ORDER BY (CAST(end_time AS BIGINT) - CAST(start_time AS BIGINT)) DESC NULLS LAST, trace_id DESC LIMIT 3 OFFSET 1",
        ),
        (
            "total_tokens DESC NULLS LAST",
            vec![
                SortKey {
                    expr: col_expr(&schema, traces::TOTAL_TOKENS),
                    descending: true,
                    nulls_first: false,
                },
                SortKey {
                    expr: col_expr(&schema, traces::TRACE_ID),
                    descending: true,
                    nulls_first: false,
                },
            ],
            "SELECT trace_id, name, cost_total FROM traces ORDER BY total_tokens DESC NULLS LAST, trace_id DESC LIMIT 3 OFFSET 1",
        ),
    ];

    for (label, keys, df_sql) in sort_cases {
        let tp_src = traces_src.clone().with_list_mode(TraceListMode::TwoPhase {
            sort_keys: keys,
            limit: 3,
            offset: 1,
        });
        let tp_spec = ScanSpec {
            projection: Some(vec![
                schema.index_of(traces::TRACE_ID).unwrap(),
                schema.index_of(traces::NAME).unwrap(),
                schema.index_of(traces::COST_TOTAL).unwrap(),
            ]),
            ..Default::default()
        };
        assert!(tp_src.explain_summary(&tp_spec).contains("mode=list"));
        let native_tp = run_source(&tp_src, &tp_spec, &exec).unwrap();
        let df_tp = df_provider_sql(
            &rt,
            &state,
            &runtime,
            QueryOptions::new(None, None),
            lookback,
            "traces",
            df_sql,
        );
        assert_ordered_parity(&native_tp, &df_tp, label);
    }
}

// ---------------------------------------------------------------------------
// 3. ScoresSource parity: LWW dedup, deleted suppression, GDPR tombstones
// ---------------------------------------------------------------------------

#[test]
fn scores_source_parity_against_datafusion() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let runtime = register_store_runtime(&store);
    let cache = Arc::new(MetadataCache::new(4 * 1024 * 1024));
    let suspects = Suspects::new(Arc::new(VerifyQueue::default()));

    let base = 20_000 * DAY_NS;
    let day0 = day_of(base);
    let day1 = day_of(base + DAY_NS);

    // Score 1: v1 in file 0, v2 (tie on updated_at, higher ingest_time) in memtable -> v2 wins!
    let s1_v1 = score_row(1, 1, base + 100, base + 500, base + 500, false, "s1-old");
    let s1_v2 = score_row(
        1,
        1,
        base + 100,
        base + 500,
        base + 800,
        false,
        "s1-tie-winner",
    );

    // Score 2: v1 (deleted=false) in file 0, v2 (deleted=true, newer updated_at) in file 1 -> hidden!
    // Must NOT resurrect v1 even when filtering `deleted = false` or `name = 's2-live'`!
    let s2_v1 = score_row(2, 2, base + 200, base + 300, base + 300, false, "s2-live");
    let s2_v2 = score_row(2, 2, base + 200, base + 400, base + 400, true, "s2-deleted");

    // Score 3: v1 in file 0 (name = "old"), v2 in file 1 (name = "new").
    // Filtering `name = 'old'` MUST return 0 rows (never push non-created_at filter below LWW!).
    let s3_v1 = score_row(3, 3, base + 300, base + 400, base + 400, false, "old");
    let s3_v2 = score_row(3, 3, base + 300, base + 900, base + 900, false, "new");

    // Score 4: GDPR tombstone covers user "erased-u" with cutoff base + 600.
    // v1 ingested at base + 500 is erased by GDPR; v2 ingested at base + 900 survives!
    let mut s4_v1 = score_row(
        4,
        4,
        base + 400,
        base + 1000,
        base + 500,
        false,
        "s4-erased",
    );
    s4_v1.user_id = Some("erased-u".to_owned());
    let mut s4_v2 = score_row(4, 4, base + 400, base + 950, base + 900, false, "s4-kept");
    s4_v2.user_id = Some("erased-u".to_owned());

    // Score 5: on day 1, tests `created_at` filter pushdown.
    let s5 = score_row(
        5,
        5,
        base + DAY_NS + 100,
        base + DAY_NS + 100,
        base + DAY_NS + 100,
        false,
        "day1",
    );

    let f0 = write_table_file(
        &rt,
        &store,
        FileTable::Scores,
        day0,
        "tables/scores/f0.parquet",
        score_rows(&[s1_v1, s2_v1, s3_v1, s4_v1]),
        0,
    );
    let f1 = write_table_file(
        &rt,
        &store,
        FileTable::Scores,
        day1,
        "tables/scores/f1.parquet",
        score_rows(&[s2_v2, s3_v2, s4_v2, s5]),
        0,
    );

    let mut scores_mt = Memtable::new(Table::Scores);
    scores_mt
        .insert(project(), score_rows(&[s1_v2]), 1)
        .unwrap();
    let raw_scores_mem = scores_mt.snapshot();

    let d1 = PendingDeletion {
        id: Uuid::now_v7(),
        seq: 1,
        project: project(),
        cutoff: base + 600,
        registered_at: base + 600,
        trace_ids: HashSet::new(),
        users: ["erased-u".to_owned()].into_iter().collect(),
        sessions: HashSet::new(),
    };
    let tombstones = Tombstones::default().with(d1);
    let deletions = tombstones.for_project(project());

    let filtered_scores_mem =
        deletion::project_snapshot(Table::Scores, &raw_scores_mem, project(), None, &deletions)
            .unwrap();

    let files = vec![f0, f1];
    let catalog = build_catalog(&files);
    let state = ReadState {
        spans: Memtable::new(Table::Spans).snapshot(),
        scores: Arc::clone(&filtered_scores_mem),
        catalog,
        tombstones: Arc::new(tombstones),
        projects: Arc::new(ProjectStates::default()),
    };

    let (exec, _dir) = make_ctx(8 * 1024 * 1024, 16 * 1024 * 1024, 2);
    let scores_src = ScoresSource::new(
        Arc::clone(&store),
        project(),
        files,
        filtered_scores_mem,
        None,
        deletions,
        cache,
        suspects,
        rt.handle().clone(),
    )
    .unwrap();
    let schema = scores_src.schema();

    // 3a. Full live scores parity.
    let spec_all = ScanSpec {
        projection: Some(vec![
            schema.index_of(scores::SCORE_ID).unwrap(),
            schema.index_of(scores::NAME).unwrap(),
            schema.index_of(scores::CREATED_AT).unwrap(),
            schema.index_of(scores::UPDATED_AT).unwrap(),
        ]),
        ..Default::default()
    };
    let native_all = run_source(&scores_src, &spec_all, &exec).unwrap();
    let df_all = df_provider_sql(
        &rt,
        &state,
        &runtime,
        QueryOptions::new(None, None),
        0,
        "scores",
        "SELECT score_id, name, created_at, updated_at FROM scores",
    );
    assert_unordered_parity(&native_all, &df_all, "scores live LWW parity");

    // 3b. Non-`created_at` filter must NOT resurrect superseded versions (`name = 'old'` -> 0 rows).
    let spec_old = ScanSpec {
        projection: Some(vec![schema.index_of(scores::SCORE_ID).unwrap()]),
        filters: vec![Expr::Binary {
            left: Box::new(col_expr(&schema, scores::NAME)),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::Utf8("old".to_owned()))),
            data_type: DataType::Bool,
        }],
        ..Default::default()
    };
    let native_old = run_source(&scores_src, &spec_old, &exec).unwrap();
    assert!(
        native_old.is_empty(),
        "superseded version must not resurrect"
    );

    // 3c. `created_at` pushdown + empty projection `Some(vec![])` parity.
    let day1_filter = Expr::Binary {
        left: Box::new(col_expr(&schema, scores::CREATED_AT)),
        op: BinaryOp::GtEq,
        right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
            value: base + DAY_NS,
            utc: true,
        })),
        data_type: DataType::Bool,
    };
    let spec_day1 = ScanSpec {
        projection: Some(Vec::new()),
        filters: vec![day1_filter],
        ..Default::default()
    };
    let native_day1 = run_source(&scores_src, &spec_day1, &exec).unwrap();
    let count: usize = native_day1.iter().map(Batch::num_rows).sum();
    assert_eq!(count, 1);
}

// ---------------------------------------------------------------------------
// 4. Spill under tight ScopedPool, sub-ms CancelToken drain, and Admission
// ---------------------------------------------------------------------------

struct HangingStore {
    inner: Arc<dyn ObjectStore>,
    hang: AtomicBool,
}

impl std::fmt::Debug for HangingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HangingStore")
            .field("hang", &self.hang.load(Ordering::Relaxed))
            .finish()
    }
}

impl std::fmt::Display for HangingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HangingStore")
    }
}

#[async_trait::async_trait]
impl ObjectStore for HangingStore {
    async fn put_opts(
        &self,
        location: &ObjectPath,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &ObjectPath,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &ObjectPath,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.inner.get_opts(location, options).await
    }

    async fn get_ranges(
        &self,
        location: &ObjectPath,
        ranges: &[Range<u64>],
    ) -> object_store::Result<Vec<Bytes>> {
        if self.hang.load(Ordering::Relaxed) {
            futures::future::pending::<()>().await;
        }
        self.inner.get_ranges(location, ranges).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<ObjectPath>>,
    ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &ObjectPath,
        to: &ObjectPath,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

#[test]
fn spill_under_tight_memory_traces_and_scores() {
    use cetra_query::exec::{ControlFlow, PushSink};

    struct RowCountSink {
        rows: usize,
    }

    impl PushSink for RowCountSink {
        fn consume(&mut self, batch: Batch) -> Result<ControlFlow, QueryError> {
            self.rows += batch.num_rows();
            Ok(ControlFlow::Continue)
        }
    }

    fn count_source_rows(
        source: &dyn ScanSource,
        spec: &ScanSpec,
        ctx: &ExecContext,
    ) -> Result<(usize, u64), QueryError> {
        let partitions = source.open_partitions(spec, ctx)?;
        if partitions.is_empty() {
            return Ok((0, 0));
        }
        let spilled_after_open = ctx.spill.spilled_bytes();
        let sinks = run_pipeline(
            ctx,
            partitions,
            |_| Ok(Vec::new()),
            |_| Ok(RowCountSink { rows: 0 }),
        )?;
        Ok((sinks.into_iter().map(|s| s.rows).sum(), spilled_after_open))
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let cache = Arc::new(MetadataCache::new(1024 * 1024));
    let suspects = Suspects::new(Arc::new(VerifyQueue::default()));

    let base = 20_000 * DAY_NS;
    let day = day_of(base);

    // Build 200 traces & scores across two files each (< 128 distinct dict
    // entries per file). Using 16 distinct ~700B values keeps each batch's
    // dictionary small (~11.5 KiB) while accumulating > 160 KiB across batches,
    // forcing `ExternalSort` to spill under a tight 96 KiB pool cap.
    let mut spans_vec: Vec<Span> = Vec::with_capacity(200);
    let mut scores_vec: Vec<Score> = Vec::with_capacity(200);
    for i in 1..=200_u16 {
        let mut s = span_row(i, 1, base + i64::from(i) * 100, base + 10);
        s.name = format!("trace-name-{:02}-{}", i % 16, "x".repeat(700));
        spans_vec.push(s);

        let mut sc = score_row(
            u128::from(i),
            i,
            base + 100,
            base + 200,
            base + 200,
            false,
            &format!("score-{:02}-{}", i % 16, "y".repeat(700)),
        );
        sc.reason = Some(format!("reason-{:02}-{}", i % 16, "z".repeat(700)));
        scores_vec.push(sc);
    }

    let f_trace_a = write_table_file(
        &rt,
        &store,
        FileTable::Traces,
        day,
        "tables/traces/spill_a.parquet",
        partials_from_spans(&span_rows(&spans_vec[..100])).unwrap(),
        0,
    );
    let f_trace_b = write_table_file(
        &rt,
        &store,
        FileTable::Traces,
        day,
        "tables/traces/spill_b.parquet",
        partials_from_spans(&span_rows(&spans_vec[100..])).unwrap(),
        0,
    );
    let f_score_a = write_table_file(
        &rt,
        &store,
        FileTable::Scores,
        day,
        "tables/scores/spill_a.parquet",
        score_rows(&scores_vec[..100]),
        0,
    );
    let f_score_b = write_table_file(
        &rt,
        &store,
        FileTable::Scores,
        day,
        "tables/scores/spill_b.parquet",
        score_rows(&scores_vec[100..]),
        0,
    );

    // 1. Tight pool for TracesSource (96 KiB, 16-row batches).
    let (mut tight_trace_ctx, _tdir) = make_ctx(96 * 1024, 16 * 1024 * 1024, 1);
    tight_trace_ctx.batch_size = 16;
    let traces_src = TracesSource::new(
        Arc::clone(&store),
        project(),
        vec![f_trace_a, f_trace_b],
        Memtable::new(Table::Spans).snapshot(),
        0,
        Arc::from([]),
        Arc::clone(&cache),
        suspects.clone(),
        rt.handle().clone(),
    )
    .unwrap();
    let t_schema = traces_src.schema();
    let t_spec = ScanSpec {
        projection: Some(vec![
            t_schema.index_of(traces::TRACE_ID).unwrap(),
            t_schema.index_of(traces::NAME).unwrap(),
        ]),
        ..Default::default()
    };
    let (total_traces, trace_spilled_bytes) =
        count_source_rows(&traces_src, &t_spec, &tight_trace_ctx).unwrap();
    assert_eq!(total_traces, 200);
    assert!(
        trace_spilled_bytes > 0,
        "expected TracesSource ExternalSort to spill under tight 96 KiB pool"
    );
    assert_eq!(tight_trace_ctx.spill.spilled_bytes(), 0);
    assert_eq!(tight_trace_ctx.pool.reserved(), 0);

    // 2. Tight pool for ScoresSource (96 KiB, 16-row batches).
    let (mut tight_score_ctx, _sdir) = make_ctx(96 * 1024, 16 * 1024 * 1024, 1);
    tight_score_ctx.batch_size = 16;
    let scores_src = ScoresSource::new(
        Arc::clone(&store),
        project(),
        vec![f_score_a, f_score_b],
        Memtable::new(Table::Scores).snapshot(),
        None,
        Arc::from([]),
        cache,
        suspects,
        rt.handle().clone(),
    )
    .unwrap();
    let s_schema = scores_src.schema();
    let s_spec = ScanSpec {
        projection: Some(vec![
            s_schema.index_of(scores::SCORE_ID).unwrap(),
            s_schema.index_of(scores::NAME).unwrap(),
            s_schema.index_of(scores::REASON).unwrap(),
        ]),
        ..Default::default()
    };
    let (total_scores, score_spilled_bytes) =
        count_source_rows(&scores_src, &s_spec, &tight_score_ctx).unwrap();
    assert_eq!(total_scores, 200);
    assert!(
        score_spilled_bytes > 0,
        "expected ScoresSource ExternalSort to spill under tight 96 KiB pool"
    );
    assert_eq!(tight_score_ctx.spill.spilled_bytes(), 0);
    assert_eq!(tight_score_ctx.pool.reserved(), 0);
}

#[test]
fn cancel_token_aborts_and_drains_synchronously_with_admission() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let inner_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let hanging = Arc::new(HangingStore {
        inner: inner_store,
        hang: AtomicBool::new(false),
    });
    let store: Arc<dyn ObjectStore> = hanging.clone();
    let cache = Arc::new(MetadataCache::new(0));
    let suspects = Suspects::new(Arc::new(VerifyQueue::default()));

    let base = 20_000 * DAY_NS;
    let day = day_of(base);
    let f_trace = write_table_file(
        &rt,
        &store,
        FileTable::Traces,
        day,
        "tables/traces/cancel.parquet",
        partials_from_spans(&span_rows(&[span_row(1, 1, base, base)])).unwrap(),
        0,
    );

    // Admission pool integration: exhaust pool -> `Busy`, release -> admits, then run cancel test.
    let admission = AsyncAdmissionPool::new(1024 * 1024);
    let held = rt
        .block_on(admission.admit(1024 * 1024, 1024 * 1024, Duration::from_millis(50)))
        .unwrap();
    let busy_err = rt
        .block_on(admission.admit(512 * 1024, 512 * 1024, Duration::from_millis(15)))
        .unwrap_err();
    assert!(matches!(busy_err, QueryError::Busy));
    drop(held);

    let scoped = rt
        .block_on(admission.admit(64 * 1024, 1024 * 1024, Duration::from_millis(50)))
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cancel_ctx = ExecContext {
        pool: Arc::clone(&scoped),
        eval: EvalContext {
            now_ns: 0,
            cancel: CancelToken::new(),
        },
        spill: Arc::new(SpillManager::with_fixed_cap(
            dir.path().to_path_buf(),
            16 * 1024 * 1024,
        )),
        workers: Arc::new(WorkerPool::new(2).unwrap()),
        batch_size: 128,
        target_partitions: 2,
        metrics: ExecMetrics::default(),
    };

    let traces_src = TracesSource::new(
        store,
        project(),
        vec![f_trace],
        Memtable::new(Table::Spans).snapshot(),
        0,
        Arc::from([]),
        cache,
        suspects,
        rt.handle().clone(),
    )
    .unwrap();

    hanging.hang.store(true, Ordering::Relaxed);
    let cancel = cancel_ctx.eval.cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(10));
        cancel.cancel();
    });

    let t0 = Instant::now();
    let err = run_source(&traces_src, &ScanSpec::default(), &cancel_ctx).unwrap_err();
    assert!(matches!(err, QueryError::Cancelled));
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "cancellation took {:?}",
        t0.elapsed()
    );
    assert_eq!(cancel_ctx.spill.spilled_bytes(), 0);
    assert_eq!(cancel_ctx.pool.reserved(), 0);
    drop(cancel_ctx);
    drop(scoped);
    assert_eq!(admission.used(), 0);
}

#[test]
fn traces_and_scores_source_debug_is_privacy_safe() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let cache = Arc::new(MetadataCache::new(1024));
    let suspects = Suspects::new(Arc::new(VerifyQueue::default()));
    let base = 20_000 * DAY_NS;

    let mut s = span_row(1, 1, base, base + 100);
    s.name = "secret-span-name".to_owned();
    s.user_id = Some("alice-pii@example.com".to_owned());
    s.session_id = Some("secret-session-xyz".to_owned());
    s.input = Some("top-secret-prompt-content".to_owned());
    s.output = Some("top-secret-completion".to_owned());

    let mut spans_mt = Memtable::new(Table::Spans);
    spans_mt.insert(project(), span_rows(&[s]), 1).unwrap();

    let mut sc = score_row(1, 1, base, base, base, false, "secret-score-name");
    sc.user_id = Some("bob-pii@example.com".to_owned());
    sc.session_id = Some("secret-score-session".to_owned());
    sc.reason = Some("secret-score-reason".to_owned());

    let mut scores_mt = Memtable::new(Table::Scores);
    scores_mt.insert(project(), score_rows(&[sc]), 1).unwrap();

    let deletion = PendingDeletion {
        id: Uuid::now_v7(),
        seq: 1,
        project: project(),
        cutoff: base + 1_000,
        registered_at: base + 1_000,
        trace_ids: HashSet::new(),
        users: ["erased-user-pii".to_owned()].into_iter().collect(),
        sessions: ["erased-session-xyz".to_owned()].into_iter().collect(),
    };
    let tombstones = Tombstones::default().with(deletion);
    let deletions = tombstones.for_project(project());

    let traces_src = TracesSource::new(
        Arc::clone(&store),
        project(),
        Vec::new(),
        spans_mt.snapshot(),
        60_000_000_000,
        Arc::clone(&deletions),
        Arc::clone(&cache),
        suspects.clone(),
        rt.handle().clone(),
    )
    .unwrap();
    let widening_src = traces_src
        .clone()
        .with_list_mode(TraceListMode::Widening { fetch: 10 });
    let schema = traces_src.schema();
    let two_phase_src = traces_src.clone().with_list_mode(TraceListMode::TwoPhase {
        sort_keys: vec![SortKey {
            expr: col_expr(&schema, traces::COST_TOTAL),
            descending: true,
            nulls_first: false,
        }],
        limit: 10,
        offset: 0,
    });

    let scores_src = ScoresSource::new(
        store,
        project(),
        Vec::new(),
        scores_mt.snapshot(),
        None,
        deletions,
        cache,
        suspects,
        rt.handle().clone(),
    )
    .unwrap();

    let forbidden = [
        "secret-span-name",
        "alice-pii@example.com",
        "secret-session-xyz",
        "top-secret-prompt-content",
        "top-secret-completion",
        "secret-score-name",
        "bob-pii@example.com",
        "secret-score-session",
        "secret-score-reason",
        "erased-user-pii",
        "erased-session-xyz",
    ];

    for dbg in [
        format!("{traces_src:?}"),
        format!("{widening_src:?}"),
        format!("{two_phase_src:?}"),
    ] {
        assert!(dbg.contains("TracesSource"), "missing struct name: {dbg}");
        assert!(dbg.contains("files: 0"), "missing file count: {dbg}");
        assert!(dbg.contains("mem_batches: 1"), "missing mem count: {dbg}");
        assert!(
            dbg.contains("deletions: 1"),
            "missing deletion count: {dbg}"
        );
        for secret in forbidden {
            assert!(!dbg.contains(secret), "Debug leaked `{secret}`: {dbg}");
        }
    }

    let scores_dbg = format!("{scores_src:?}");
    assert!(
        scores_dbg.contains("ScoresSource"),
        "missing struct name: {scores_dbg}"
    );
    assert!(
        scores_dbg.contains("files: 0"),
        "missing file count: {scores_dbg}"
    );
    assert!(
        scores_dbg.contains("mem_batches: 1"),
        "missing mem count: {scores_dbg}"
    );
    assert!(
        scores_dbg.contains("deletions: 1"),
        "missing deletion count: {scores_dbg}"
    );
    for secret in forbidden {
        assert!(
            !scores_dbg.contains(secret),
            "Debug leaked `{secret}`: {scores_dbg}"
        );
    }
}
