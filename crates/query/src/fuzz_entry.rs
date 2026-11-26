//! Fuzz entry point for `cetra-query` expression parsing, binding, evaluation, time parsing,
//! and tombstone predicates (D-QE-7). Must never panic.

use std::sync::{Arc, OnceLock};

use cetra_column::{
    Batch, BinaryData, Bitmap, COST_DECIMAL_PRECISION, COST_DECIMAL_SCALE, CancelToken, Column,
    DataType, DictColumn, EventItem, Field, FlatColumn, FlatData, GlobalPool, Interval, LinkItem,
    MapData, RleColumn, ScalarValue, Schema, Selection, Utf8Data,
};
use serde_json::{Map, Value};

use crate::bind::{bind_expr, compile_filter};
use crate::encode::{
    EXPORT_PIECE_BYTES, encode_csv_chunk, encode_csv_header, encode_ndjson_chunk,
    export_piece_ranges,
};
use crate::eval::{eval_expr, eval_filter};
use crate::exec::{BatchSource, ExecContext, ExecMetrics, SpillManager, WorkerPool};
use crate::filter::{FilterView, like_pattern, parse_filter_ast};
use crate::fn_time::{parse_date_days, parse_interval, parse_timestamp_ns};
use crate::ir::{EvalContext, Expr};
use crate::sql::{
    BoundParams, LogicalPlan, ParamKind, SqlCatalog, SqlSources, bind_statement,
    columns_from_schema, execute_plan_json, optimize, parse_and_validate_sql, parse_param_values,
    raw_scores_schema, raw_spans_schema, raw_traces_schema, sql_export_plan_stream,
};
use crate::text::{FILTER_LIMITS, TextKind, expect_end, parser, tokenize};
use crate::tombstone::{TombstoneSpec, TombstoneTable, covered_predicate, filter_uncovered};

const FUZZ_POOL_LIMIT: usize = 256 * 1024;
const FUZZ_SQL_POOL_LIMIT: usize = 1024 * 1024;
const FUZZ_EXPORT_CHUNK_CAP: usize = 256 * 1024;
const FUZZ_NOW_NS: i64 = 1_767_225_700_000_000_000;

static FUZZ_BATCHES: OnceLock<Option<(Batch, Batch)>> = OnceLock::new();
static FUZZ_SQL_ENV: OnceLock<Option<FuzzSqlEnv>> = OnceLock::new();

struct FuzzScratchDir(std::path::PathBuf);

impl FuzzScratchDir {
    fn new() -> Result<Self, crate::error::QueryError> {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "cetra-query-fuzz-{}-{nanos}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path)
            .map_err(|e| crate::error::QueryError::internal(e.to_string()))?;
        Ok(Self(path))
    }
}

impl Drop for FuzzScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct FuzzSqlEnv {
    catalog: SqlCatalog,
    sources: SqlSources,
    workers: Arc<WorkerPool>,
    spill: Arc<SpillManager>,
    _spill_dir: FuzzScratchDir,
}

/// Exercises Filter DSL parsing & compilation, SQL expression parsing & binding, vectorized
/// evaluation with `Reservation` accounting, `fn_time` parsers, and `tombstone` predicates.
pub fn fuzz_query_expr(data: &[u8]) {
    let Some((batch_all, batch_masked)) = FUZZ_BATCHES.get_or_init(build_fuzz_batches).as_ref()
    else {
        return;
    };

    let ctx = EvalContext::new(FUZZ_NOW_NS, CancelToken::new());
    let schema = batch_all.schema();
    let text_cow = String::from_utf8_lossy(data);
    let text = text_cow.as_ref();
    let bounded = cap_utf8(text, FILTER_LIMITS.max_bytes);

    let _ = like_pattern(bounded);

    // 1. Filter DSL parsing & compilation across all 4 views, plus evaluation on valid filters.
    for view in [
        FilterView::Spans,
        FilterView::Traces,
        FilterView::Sessions,
        FilterView::Scores,
    ] {
        let _ = parse_filter_ast(view, schema, text);
        if let Ok(compiled) = compile_filter(view, schema, text) {
            let _ = compiled.expr.fmt_plan();
            let _ = format!("{:?}", compiled.expr);
            if compiled.scores.is_empty() {
                eval_and_check(&compiled.expr, batch_all, &ctx);
                eval_and_check(&compiled.expr, batch_masked, &ctx);
            } else {
                if let Some(ext_all) =
                    with_score_columns(batch_all, &compiled.planning_schema, compiled.scores.len())
                {
                    eval_and_check(&compiled.expr, &ext_all, &ctx);
                }
                if let Some(ext_masked) = with_score_columns(
                    batch_masked,
                    &compiled.planning_schema,
                    compiled.scores.len(),
                ) {
                    eval_and_check(&compiled.expr, &ext_masked, &ctx);
                }
            }
        }
    }

    // 2. Arbitrary SQL expression tokenization, parsing, binding, and evaluation.
    let _ = tokenize(text, &FILTER_LIMITS, TextKind::Filter);
    if let Ok(tokens) = tokenize(bounded, &FILTER_LIMITS, TextKind::Filter) {
        let mut p = parser(tokens);
        if let Ok(ast) = p.parse_expr() {
            let _ = expect_end(&mut p, bounded);
            if let Ok(expr) = bind_expr(&ast, schema) {
                let _ = expr.fmt_plan();
                let _ = format!("{expr:?}");
                eval_and_check(&expr, batch_all, &ctx);
                eval_and_check(&expr, batch_masked, &ctx);
            }
        }
    }

    // 3. Direct time, interval, and date parsers.
    exercise_time_parsers(bounded);
    for segment in bounded.split('\'').take(16) {
        exercise_time_parsers(segment);
    }

    // 4. Tombstone predicate building and batch filtering.
    let specs = derive_tombstones(data, bounded);
    for table in [
        TombstoneTable::Spans,
        TombstoneTable::Scores,
        TombstoneTable::Traces,
    ] {
        if let Ok(Some(pred)) = covered_predicate(table, schema, &specs) {
            let _ = pred.fmt_plan();
            let _ = format!("{pred:?}");
            filter_uncovered_and_check(&pred, batch_all, &ctx);
            filter_uncovered_and_check(&pred, batch_masked, &ctx);
        }
    }
}

fn eval_and_check(expr: &Expr, batch: &Batch, ctx: &EvalContext) {
    let pool = GlobalPool::standalone(FUZZ_POOL_LIMIT);
    let mut res = pool.register("fuzz_eval", true);

    let before = res.size();
    match eval_expr(expr, batch, ctx, Some(&mut res)) {
        Ok(col) => {
            let expected = if batch.columns().iter().any(|c| Arc::ptr_eq(c, &col)) {
                0
            } else {
                col.memory_bytes()
            };
            assert_eq!(res.size(), before + expected);
        }
        Err(_) => assert_eq!(res.size(), before),
    }

    let before_filter = res.size();
    match eval_filter(expr, batch, ctx, Some(&mut res)) {
        Ok(bm) => assert_eq!(res.size(), before_filter + bm.memory_bytes()),
        Err(_) => assert_eq!(res.size(), before_filter),
    }
}

fn filter_uncovered_and_check(pred: &Expr, batch: &Batch, ctx: &EvalContext) {
    let pool = GlobalPool::standalone(FUZZ_POOL_LIMIT);
    let mut res = pool.register("fuzz_tombstone", true);
    let before = res.size();
    match filter_uncovered(batch, pred, ctx, Some(&mut res)) {
        Ok(out) => assert_eq!(res.size(), before + out.selection().memory_bytes()),
        Err(_) => assert_eq!(res.size(), before),
    }
}

fn exercise_time_parsers(s: &str) {
    let _ = parse_interval(s);
    let _ = parse_timestamp_ns(s, true);
    let _ = parse_timestamp_ns(s, false);
    let _ = parse_date_days(s);
}

fn cap_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn derive_tombstones(data: &[u8], text: &str) -> Vec<TombstoneSpec> {
    let mut cutoff_bytes = [0u8; 8];
    for (dst, src) in cutoff_bytes.iter_mut().zip(data.iter()) {
        *dst = *src;
    }
    let cutoff_from_data = i64::from_le_bytes(cutoff_bytes);

    let flags = data.first().copied().unwrap_or(0);
    let mut trace_ids = Vec::new();
    if let Some(slice) = data.get(..16) {
        let mut id = [0u8; 16];
        id.copy_from_slice(slice);
        trace_ids.push(id);
    }
    if (flags & 1) != 0 {
        trace_ids.push([0x11; 16]);
        trace_ids.push([0x11; 16]);
    }

    let mut users = Vec::new();
    if !text.is_empty() {
        users.push(text.to_owned());
        users.push(text.to_owned());
    }
    if (flags & 2) != 0 {
        users.push("alice".to_owned());
    }

    let mut sessions = Vec::new();
    if !text.is_empty() {
        sessions.push(text.to_owned());
    }
    if (flags & 4) != 0 {
        sessions.push("sess-1".to_owned());
        sessions.push("sess-1".to_owned());
    }

    vec![
        TombstoneSpec {
            cutoff: cutoff_from_data,
            trace_ids: trace_ids.clone(),
            users: users.clone(),
            sessions: sessions.clone(),
        },
        TombstoneSpec {
            cutoff: 1_767_225_650_000_000_000,
            trace_ids,
            users,
            sessions,
        },
    ]
}

fn with_score_columns(
    batch: &Batch,
    planning_schema: &Schema,
    score_count: usize,
) -> Option<Batch> {
    let n = batch.raw_len();
    let mut cols = batch.columns().to_vec();
    for _ in 0..score_count {
        cols.push(Arc::new(
            Column::try_new_const(ScalarValue::F64(0.75), n).ok()?,
        ));
        cols.push(Arc::new(
            Column::try_new_const(ScalarValue::Utf8("good".to_owned()), n).ok()?,
        ));
        cols.push(Arc::new(
            Column::try_new_const(ScalarValue::Bool(true), n).ok()?,
        ));
    }
    Batch::try_new_shared(
        Arc::new(planning_schema.clone()),
        cols,
        n,
        batch.selection().clone(),
    )
    .ok()
}

fn build_fuzz_batches() -> Option<(Batch, Batch)> {
    try_build_fuzz_batches().ok()
}

fn try_build_fuzz_batches() -> Result<(Batch, Batch), cetra_column::ColumnError> {
    let schema = Schema::try_new_arc(vec![
        Field::new("trace_id", DataType::FixedBytes16, true),
        Field::new("user_id", DataType::Utf8, true),
        Field::new("session_id", DataType::Utf8, false),
        Field::new("name", DataType::Utf8, true),
        Field::new("status", DataType::Utf8, true),
        Field::new("model", DataType::Utf8, false),
        Field::new("environment", DataType::Utf8, true),
        Field::new("duration_ns", DataType::I64, true),
        Field::new("cost_total", DataType::cost_decimal(), true),
        Field::new("latency_ms", DataType::F64, true),
        Field::new("has_root", DataType::Bool, true),
        Field::new("start_time", DataType::timestamp_utc(), true),
        Field::new("ingest_time", DataType::timestamp_utc(), false),
        Field::new("tags", DataType::List(Box::new(DataType::Utf8)), true),
        Field::new("metadata", DataType::MapUtf8, true),
        Field::new("date_col", DataType::Date, true),
        Field::new("iv_col", DataType::Interval, false),
        Field::new("bin_col", DataType::Binary, true),
        Field::new("fb8_col", DataType::FixedBytes8, true),
    ])?;

    let n = 4usize;
    let nulls = Some(Bitmap::from_bools(&[true, false, true, true]));

    let trace_id = Column::Flat(FlatColumn::try_new(
        FlatData::FixedBytes16(vec![[0x11; 16], [0x00; 16], [0x22; 16], [0x33; 16]]),
        nulls.clone(),
    )?);

    let user_dict_vals = FlatColumn::try_new(
        FlatData::Utf8(Utf8Data::try_from_slices(["alice", "bob"])?),
        None,
    )?;
    let user_id = Column::Dict(DictColumn::try_new(
        user_dict_vals,
        vec![0, 1, 0, 1],
        Some(Bitmap::from_bools(&[true, true, false, true])),
    )?);

    let session_id = Column::try_new_const(ScalarValue::Utf8("sess-1".to_owned()), n)?;

    let name = Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(Utf8Data::try_from_slices([
            "chat",
            "",
            "é🚀",
            "{\"k\":\"v\"}",
        ])?),
        nulls.clone(),
    )?);

    let status_dict_vals = FlatColumn::try_new(
        FlatData::Utf8(Utf8Data::try_from_slices(["ok", "error"])?),
        None,
    )?;
    let status = Column::Dict(DictColumn::try_new(
        status_dict_vals,
        vec![0, 1, 0, 0],
        Some(Bitmap::from_bools(&[true, true, true, false])),
    )?);

    let model = Column::try_new_const(ScalarValue::Utf8("gpt-4o".to_owned()), n)?;

    let environment = Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(Utf8Data::try_from_slices(["prod", "staging", "", "eval"])?),
        nulls.clone(),
    )?);

    // Row 1 is non-null 0 so masked evaluation verifies inactive rows never fault on division.
    let duration_vals = FlatColumn::try_new(FlatData::I64(vec![100, 0, -5]), None)?;
    let duration_ns = Column::Rle(RleColumn::try_new(duration_vals, vec![1, 2, 4])?);

    let cost_total = Column::Flat(FlatColumn::try_new(
        FlatData::Decimal128 {
            values: vec![1_500_000_000, 0, -42, 999],
            precision: COST_DECIMAL_PRECISION,
            scale: COST_DECIMAL_SCALE,
        },
        nulls.clone(),
    )?);

    let latency_ms = Column::Flat(FlatColumn::try_new(
        FlatData::F64(vec![1.25, 0.0, f64::NAN, -0.0]),
        Some(Bitmap::from_bools(&[true, true, true, false])),
    )?);

    let has_root = Column::Flat(FlatColumn::try_new(
        FlatData::Bool(Bitmap::from_bools(&[true, false, false, true])),
        nulls.clone(),
    )?);

    let start_time = Column::Flat(FlatColumn::try_new(
        FlatData::TimestampNs {
            values: vec![
                1_767_225_600_000_000_000,
                0,
                1_767_225_605_000_000_000,
                1_767_225_610_000_000_000,
            ],
            utc: true,
        },
        nulls.clone(),
    )?);

    let ingest_vals = FlatColumn::try_new(
        FlatData::TimestampNs {
            values: vec![1_767_225_600_000_000_000, 1_767_225_700_000_000_000],
            utc: true,
        },
        None,
    )?;
    let ingest_time = Column::Rle(RleColumn::try_new(ingest_vals, vec![2, 4])?);

    let tag_items = FlatColumn::try_new(
        FlatData::Utf8(Utf8Data::try_from_slices([
            "alpha", "beta", "gamma", "beta",
        ])?),
        None,
    )?;
    let tags = Column::Flat(FlatColumn::try_new(
        FlatData::List {
            item_type: DataType::Utf8,
            offsets: vec![0, 2, 2, 3, 4],
            values: Box::new(Column::Flat(tag_items)),
        },
        nulls.clone(),
    )?);

    let metadata = Column::Flat(FlatColumn::try_new(
        FlatData::MapUtf8(Box::new(MapData::try_new(
            vec![0, 2, 2, 3, 4],
            Utf8Data::try_from_slices(["k1", "k2", "k3", "channel"])?,
            Utf8Data::try_from_slices(["v1", "", "v3", "email"])?,
            Some(Bitmap::from_bools(&[true, false, true, true])),
        )?)),
        nulls.clone(),
    )?);

    let date_col = Column::Flat(FlatColumn::try_new(
        FlatData::Date(vec![20_454, 0, -1, 20_455]),
        nulls.clone(),
    )?);

    let iv_col = Column::try_new_const(
        ScalarValue::Interval(Interval {
            months: 1,
            days: 2,
            nanos: 3,
        }),
        n,
    )?;

    let bin_col = Column::Flat(FlatColumn::try_new(
        FlatData::Binary(BinaryData::try_from_slices([
            &b"ab"[..],
            &b""[..],
            &b"\x00\xff"[..],
            &b"z"[..],
        ])?),
        nulls.clone(),
    )?);

    let fb8_col = Column::Flat(FlatColumn::try_new(
        FlatData::FixedBytes8(vec![[1; 8], [0; 8], [2; 8], [3; 8]]),
        nulls,
    )?);

    let columns = vec![
        trace_id,
        user_id,
        session_id,
        name,
        status,
        model,
        environment,
        duration_ns,
        cost_total,
        latency_ms,
        has_root,
        start_time,
        ingest_time,
        tags,
        metadata,
        date_col,
        iv_col,
        bin_col,
        fb8_col,
    ];

    let batch_all = Batch::try_new_all(Arc::clone(&schema), columns.clone(), n)?;
    let mask = Selection::Mask(Bitmap::from_bools(&[true, false, true, false]));
    let batch_masked = Batch::try_new(schema, columns, n, mask)?;
    Ok((batch_all, batch_masked))
}

// ---------------------------------------------------------------------------
// Shape B SQL fuzzer (`fuzz_sql`)
// ---------------------------------------------------------------------------

/// Exercises Shape B SQL tokenization, parameter scanning/coercion, parsing, validation,
/// binding, optimization, compilation, and execution on small synthetic `BatchSource` tables
/// under a tight 1 MiB `ScopedPool`. Must never panic.
pub fn fuzz_sql(data: &[u8]) {
    let Some(env) = FUZZ_SQL_ENV.get_or_init(build_fuzz_sql_env).as_ref() else {
        return;
    };

    let text_cow = String::from_utf8_lossy(data);
    let text = text_cow.as_ref();
    let registered = env.catalog.registered_names();

    let Ok((stmt, placeholders)) = parse_and_validate_sql(text, &registered) else {
        return;
    };

    let mut params = Map::new();
    for (idx, ph) in placeholders.iter().enumerate() {
        let use_bad = data.get(idx).is_some_and(|b| (*b & 0x80) != 0);
        let val = if use_bad {
            Value::String("bad-param".to_owned())
        } else {
            match ph.kind {
                ParamKind::String => Value::String("llm".to_owned()),
                ParamKind::Int => Value::from(1),
                ParamKind::Float => Value::from(0.5),
                ParamKind::Bool => Value::from(true),
                ParamKind::Timestamp => Value::String("2026-01-01T00:00:00Z".to_owned()),
            }
        };
        params.insert(ph.name.clone(), val);
    }

    let Ok(param_values) = parse_param_values(&placeholders, &params) else {
        return;
    };
    let bound = BoundParams::Values(&param_values);
    let Ok(plan) = bind_statement(&stmt, &env.catalog, &bound).and_then(optimize) else {
        return;
    };
    let _ = columns_from_schema(plan.schema());

    // Guard against exponential multi-way cartesian products timing out libFuzzer.
    if count_joins(&plan) > 4 {
        return;
    }

    let pool = GlobalPool::standalone(FUZZ_SQL_POOL_LIMIT);
    let ctx = ExecContext {
        pool: Arc::clone(&pool),
        eval: EvalContext::new(FUZZ_NOW_NS, CancelToken::new()),
        spill: Arc::clone(&env.spill),
        workers: Arc::clone(&env.workers),
        batch_size: 16,
        target_partitions: 2,
        metrics: ExecMetrics::default(),
    };

    let _ = execute_plan_json(&plan, &env.sources, &ctx, 16, FUZZ_EXPORT_CHUNK_CAP);

    if let Ok((schema, mut stream)) = sql_export_plan_stream(&plan, &env.sources, &ctx, 16) {
        let _ = encode_csv_header(&schema);
        let mut pull_res = pool.register("fuzz_export_pull", false);
        let mut chunk_res = pool.register("fuzz_export_chunk", false);
        for _ in 0..4 {
            match stream.next_batch(&ctx.eval.cancel, Some(&mut pull_res)) {
                Ok(Some(batch)) => {
                    if !batch.is_empty()
                        && let Ok(ranges) =
                            export_piece_ranges(&batch, EXPORT_PIECE_BYTES, FUZZ_EXPORT_CHUNK_CAP)
                    {
                        for range in ranges {
                            let _ = encode_csv_chunk(
                                &batch,
                                range.clone(),
                                FUZZ_EXPORT_CHUNK_CAP,
                                Some(&mut chunk_res),
                            );
                            chunk_res.free();
                            let _ = encode_ndjson_chunk(
                                &batch,
                                range,
                                FUZZ_EXPORT_CHUNK_CAP,
                                Some(&mut chunk_res),
                            );
                            chunk_res.free();
                        }
                    }
                    drop(batch);
                    pull_res.free();
                }
                Ok(None) | Err(_) => {
                    pull_res.free();
                    break;
                }
            }
        }
        drop(stream);
        pull_res.free();
        chunk_res.free();
    }
    assert_eq!(pool.reserved(), 0, "fuzz_sql leaked pool reservation");
    assert_eq!(env.spill.spilled_bytes(), 0, "fuzz_sql leaked spill bytes");
}

fn count_joins(plan: &LogicalPlan) -> usize {
    match plan {
        LogicalPlan::Scan { .. } | LogicalPlan::Values { .. } | LogicalPlan::EmptyRow { .. } => 0,
        LogicalPlan::Filter { input, .. }
        | LogicalPlan::Project { input, .. }
        | LogicalPlan::Aggregate { input, .. }
        | LogicalPlan::Window { input, .. }
        | LogicalPlan::Sort { input, .. }
        | LogicalPlan::Limit { input, .. }
        | LogicalPlan::TopK { input, .. }
        | LogicalPlan::Distinct { input }
        | LogicalPlan::Unnest { input, .. }
        | LogicalPlan::Explain { inner: input, .. } => count_joins(input),
        LogicalPlan::Join { left, right, .. } => 1 + count_joins(left) + count_joins(right),
        LogicalPlan::Union { inputs, .. } => inputs.iter().map(count_joins).sum(),
    }
}

fn build_fuzz_sql_env() -> Option<FuzzSqlEnv> {
    try_build_fuzz_sql_env().ok()
}

fn try_build_fuzz_sql_env() -> Result<FuzzSqlEnv, crate::error::QueryError> {
    let mut catalog = SqlCatalog::new(None, None);
    let mut sources = SqlSources::new();

    let spans_schema = raw_spans_schema();
    let spans_batch = build_synthetic_2row_batch(&spans_schema)?;
    sources.register(
        "spans",
        Arc::new(BatchSource::new(spans_schema, vec![spans_batch])?),
    );

    let traces_schema = raw_traces_schema();
    let traces_batch = build_synthetic_2row_batch(&traces_schema)?;
    sources.register(
        "traces",
        Arc::new(BatchSource::new(traces_schema, vec![traces_batch])?),
    );

    let scores_schema = raw_scores_schema();
    let scores_batch = build_synthetic_2row_batch(&scores_schema)?;
    sources.register(
        "scores",
        Arc::new(BatchSource::new(scores_schema, vec![scores_batch])?),
    );

    let ds_schema = Schema::try_new_arc(vec![
        Field::new("id", DataType::I64, false),
        Field::new("label", DataType::Utf8, true),
    ])?;
    let ds_batch = Batch::try_new_all(
        Arc::clone(&ds_schema),
        vec![
            Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1, 2]), None)?),
            Column::Flat(FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(["pass", "fail"])?),
                None,
            )?),
        ],
        2,
    )?;
    let ds_src = Arc::new(BatchSource::new(ds_schema, vec![ds_batch])?);
    catalog.register_batch_source("dataset_items", Arc::clone(&ds_src))?;
    sources.register("dataset_items", ds_src);

    let spill_dir = FuzzScratchDir::new()?;
    let spill = Arc::new(SpillManager::with_fixed_cap(
        spill_dir.0.clone(),
        1024 * 1024,
    ));
    let workers = Arc::new(WorkerPool::new(1)?);

    Ok(FuzzSqlEnv {
        catalog,
        sources,
        workers,
        spill,
        _spill_dir: spill_dir,
    })
}

fn build_synthetic_2row_batch(schema: &Arc<Schema>) -> Result<Batch, cetra_column::ColumnError> {
    let mut cols = Vec::with_capacity(schema.len());
    let tid1 = [
        0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00,
        0x01,
    ];
    let tid2 = [
        0x00, 0x02, 0x00, 0x02, 0x00, 0x02, 0x00, 0x02, 0x00, 0x02, 0x00, 0x02, 0x00, 0x02, 0x00,
        0x02,
    ];
    let sid1 = [0x11; 8];
    let sid2 = [0x22; 8];

    for field in schema.fields() {
        let col = match (field.name.as_str(), &field.data_type) {
            ("trace_id", DataType::FixedBytes16) => Column::Flat(FlatColumn::try_new(
                FlatData::FixedBytes16(vec![tid1, tid2]),
                None,
            )?),
            ("span_id" | "root_span_id", DataType::FixedBytes8) => Column::Flat(
                FlatColumn::try_new(FlatData::FixedBytes8(vec![sid1, sid2]), None)?,
            ),
            ("span_kind", DataType::Utf8) => Column::Flat(FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(["llm", "tool"])?),
                None,
            )?),
            ("status", DataType::Utf8) => Column::Flat(FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(["ok", "error"])?),
                None,
            )?),
            ("source", DataType::Utf8) => Column::Flat(FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(["llm_judge", "api"])?),
                None,
            )?),
            ("name", DataType::Utf8) => Column::Flat(FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(["chat", "search"])?),
                None,
            )?),
            ("model" | "release" | "session_id" | "user_id" | "evaluator_id", DataType::Utf8) => {
                Column::Flat(FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(["v1", "v2"])?),
                    None,
                )?)
            }
            ("start_time" | "created_at", DataType::TimestampNs { utc }) => {
                Column::Flat(FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: vec![1_767_225_600_000_000_000, 1_767_225_660_000_000_000],
                        utc: *utc,
                    },
                    None,
                )?)
            }
            (
                "end_time" | "updated_at" | "ingest_time" | "last_ingest_time",
                DataType::TimestampNs { utc },
            ) => Column::Flat(FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: vec![1_767_225_601_000_000_000, 1_767_225_662_000_000_000],
                    utc: *utc,
                },
                None,
            )?),
            ("duration_ns", DataType::I64) => Column::Flat(FlatColumn::try_new(
                FlatData::I64(vec![1_000_000_000, 2_000_000_000]),
                None,
            )?),
            ("cost_total" | "value_num", DataType::F64) => {
                Column::Flat(FlatColumn::try_new(FlatData::F64(vec![0.1, 0.2]), None)?)
            }
            (_, DataType::Bool) => Column::Flat(FlatColumn::try_new(
                FlatData::Bool(Bitmap::from_bools(&[false, true])),
                None,
            )?),
            (_, DataType::I64) => {
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1, 2]), None)?)
            }
            (_, DataType::F64) => {
                Column::Flat(FlatColumn::try_new(FlatData::F64(vec![1.0, 2.0]), None)?)
            }
            (_, DataType::Utf8) => Column::Flat(FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(["a", "b"])?),
                None,
            )?),
            (_, DataType::TimestampNs { utc }) => Column::Flat(FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: vec![1_767_225_600_000_000_000, 1_767_225_660_000_000_000],
                    utc: *utc,
                },
                None,
            )?),
            (_, DataType::FixedBytes8) => Column::Flat(FlatColumn::try_new(
                FlatData::FixedBytes8(vec![sid1, sid2]),
                None,
            )?),
            (_, DataType::FixedBytes16) => Column::Flat(FlatColumn::try_new(
                FlatData::FixedBytes16(vec![tid1, tid2]),
                None,
            )?),
            (_, DataType::List(inner)) => {
                let sv = match inner.as_ref() {
                    DataType::Utf8 => ScalarValue::List {
                        item_type: DataType::Utf8,
                        values: vec![ScalarValue::Utf8("alpha".to_owned())],
                    },
                    other => ScalarValue::List {
                        item_type: other.clone(),
                        values: Vec::new(),
                    },
                };
                Column::try_new_const(sv, 2)?
            }
            (_, DataType::MapUtf8) => Column::try_new_const(
                ScalarValue::MapUtf8(vec![("k1".to_owned(), Some("v1".to_owned()))]),
                2,
            )?,
            (_, DataType::Events) => Column::try_new_const(
                ScalarValue::Events(vec![EventItem {
                    time: 1_767_225_600_000_000_000,
                    name: "ev".to_owned(),
                    attributes: None,
                }]),
                2,
            )?,
            (_, DataType::Links) => Column::try_new_const(
                ScalarValue::Links(vec![LinkItem {
                    trace_id: tid1,
                    span_id: sid1,
                    attributes: None,
                }]),
                2,
            )?,
            (_, dt) => Column::try_new_const(ScalarValue::Null(dt.clone()), 2)?,
        };
        cols.push(col);
    }
    Batch::try_new_all(Arc::clone(schema), cols, 2)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    /// Runs `entry` on every file of `fuzz/corpus/<target>/` and
    /// `fuzz/regressions/<target>/`. The corpus must not be empty.
    fn replay(target: &str, entry: fn(&[u8])) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz");
        let corpus = sorted_files(&root.join("corpus").join(target));
        assert!(!corpus.is_empty(), "no corpus for {target}");
        let regressions = sorted_files(&root.join("regressions").join(target));
        for path in corpus.into_iter().chain(regressions) {
            let data = std::fs::read(&path).expect("read a corpus file");
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| entry(&data)));
            if outcome.is_err() {
                panic!("{target}: {} panicked", path.display());
            }
        }
    }

    /// The files in `dir`, sorted by name. A missing directory has none.
    fn sorted_files(dir: &Path) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries
            .map(|entry| entry.expect("read a corpus entry").path())
            .filter(|path| path.is_file())
            .collect();
        files.sort();
        files
    }

    const SEEDS: &[(&str, &str)] = &[
        (
            "seed-01",
            "status = 'ok' AND duration_ns BETWEEN 10 AND 200 AND (environment IS DISTINCT FROM 'staging') AND metadata['channel'] = 'email'",
        ),
        (
            "seed-02",
            "name ILIKE '%chat%' AND tags @> ['beta'] AND array_length(tags) >= 1 AND CAST(duration_ns AS DOUBLE) > 0 AND metadata['k1']::VARCHAR = 'v1'",
        ),
        (
            "seed-03",
            "score('quality') >= 0.5 AND score_label('tone') = 'good' AND has_score('quality') AND start_time >= now() - INTERVAL '7 days' AND date_trunc('hour', start_time) >= '2026-01-01T00:00:00Z'",
        ),
        (
            "seed-04",
            "CASE WHEN duration_ns != 0 THEN 100 / duration_ns ELSE 0 END + CAST(TRY_CAST(name AS BIGINT) IS NULL AS BIGINT)",
        ),
        (
            "seed-05",
            "concat_ws(':', lower(name), substr(status, 1, 2), encode(trace_id, 'hex')) || ':' || coalesce(json_extract_text(name, '$.k'), map_keys(metadata)[1], '')",
        ),
        (
            "seed-06",
            "date_bin(INTERVAL '5 minutes', start_time) + INTERVAL '1 hour 30 minutes' >= TIMESTAMP '2026-01-01T00:00:00Z' AND extract('year', date_col) >= 2025 AND abs(latency_ms) >= 0.0",
        ),
        ("seed-07", "2 years 3 days 4 hours 5 minutes 6.5 seconds"),
        ("seed-08", "2026-01-15T12:34:56.789Z"),
        ("seed-09", "2026-01-15"),
        (
            "seed-10",
            "tags @> [lower('BETA')] AND cost_total * 2 > 0.5 AND (isnan(latency_ms) OR isfinite(latency_ms)) AND greatest(duration_ns, 0) != coalesce(nullif(duration_ns, -1), 0)",
        ),
    ];

    const SQL_SEEDS: &[(&str, &str)] = &[
        (
            "seed-01",
            "SELECT date_trunc('day', start_time) AS day, model, sum(cost_total) AS cost FROM spans WHERE span_kind = 'llm' AND model IS NOT NULL GROUP BY day, model ORDER BY day, cost DESC",
        ),
        (
            "seed-02",
            "SELECT model, approx_percentile_cont(duration_ms, 0.5) AS p50, count(*) AS calls FROM spans GROUP BY model ORDER BY calls DESC",
        ),
        (
            "seed-03",
            "SELECT t.release, s.name, avg(s.value_num) AS avg_score FROM scores s JOIN traces t ON s.trace_id = t.trace_id WHERE s.source = 'llm_judge' GROUP BY t.release, s.name ORDER BY t.release, s.name",
        ),
        (
            "seed-04",
            "SELECT session_id, trace_count, cost_total, tags FROM sessions WHERE trace_count >= {min:Int} ORDER BY cost_total DESC NULLS LAST LIMIT 10",
        ),
        (
            "seed-05",
            "WITH cte AS (SELECT trace_id, span_id, duration_ns, row_number() OVER (PARTITION BY trace_id ORDER BY start_time ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS rn FROM spans) SELECT trace_id, span_id FROM cte WHERE rn = 1",
        ),
        (
            "seed-06",
            "SELECT id, label FROM dataset_items WHERE id IN (SELECT 1) AND id NOT IN (SELECT 99) AND EXISTS (SELECT 1 FROM spans)",
        ),
        (
            "seed-07",
            "SELECT name, count(*) FILTER (WHERE status = 'error') AS errs, percentile_cont(0.5) WITHIN GROUP (ORDER BY duration_ms DESC) AS p50 FROM spans GROUP BY name",
        ),
        (
            "seed-08",
            "SELECT unnest(tags) AS tag FROM traces UNION SELECT label FROM dataset_items EXCEPT VALUES ('x')",
        ),
        (
            "seed-09",
            "EXPLAIN SELECT trace_id, name FROM traces WHERE trace_id = '00010001000100010001000100010001' AND metadata['k1'] = 'v1'",
        ),
        (
            "seed-10",
            "SELECT coalesce(nullif(name, ''), 'anon') AS n, CAST(duration_ns AS DOUBLE) / 1000.0 AS us FROM spans WHERE start_time >= {since:Timestamp} AND name ILIKE '%chat%'",
        ),
    ];

    #[test]
    fn replay_query_expr_corpus() {
        replay("query_expr", fuzz_query_expr);
    }

    #[test]
    fn replay_sql_corpus() {
        replay("sql", fuzz_sql);
    }

    #[test]
    fn valid_sql_seeds_execute() {
        let env = try_build_fuzz_sql_env().expect("fuzz sql env");
        let pool = GlobalPool::standalone(FUZZ_SQL_POOL_LIMIT);
        let ctx = ExecContext {
            pool,
            eval: EvalContext::new(FUZZ_NOW_NS, CancelToken::new()),
            spill: Arc::clone(&env.spill),
            workers: Arc::clone(&env.workers),
            batch_size: 16,
            target_partitions: 2,
            metrics: ExecMetrics::default(),
        };
        for (name, sql) in SQL_SEEDS {
            let mut params = Map::new();
            if *name == "seed-04" {
                params.insert("min".to_owned(), Value::from(1));
            } else if *name == "seed-10" {
                params.insert(
                    "since".to_owned(),
                    Value::String("2025-01-01T00:00:00Z".to_owned()),
                );
            }
            let plan = crate::sql::prepare_sql_plan(&env.catalog, sql, &params)
                .unwrap_or_else(|e| panic!("{name} prepare_sql_plan failed: {e}"));
            execute_plan_json(&plan, &env.sources, &ctx, 16, 256 * 1024)
                .unwrap_or_else(|e| panic!("{name} execute_plan_json failed: {e}"));
            assert_eq!(ctx.pool.reserved(), 0, "{name} leaked reservation");
        }
    }

    #[test]
    fn valid_seeds_exercise_engines() {
        let (batch_all, batch_masked) = try_build_fuzz_batches().expect("fuzz batches");
        let schema = batch_all.schema();
        let ctx = EvalContext::new(FUZZ_NOW_NS, CancelToken::new());

        for (name, text) in SEEDS {
            match *name {
                "seed-01" | "seed-02" | "seed-03" => {
                    let compiled = compile_filter(FilterView::Spans, schema, text)
                        .unwrap_or_else(|e| panic!("{name} compile_filter failed: {e}"));
                    let target = if compiled.scores.is_empty() {
                        batch_all.clone()
                    } else {
                        with_score_columns(
                            &batch_all,
                            &compiled.planning_schema,
                            compiled.scores.len(),
                        )
                        .expect("score columns")
                    };
                    eval_filter(&compiled.expr, &target, &ctx, None)
                        .unwrap_or_else(|e| panic!("{name} eval_filter failed: {e}"));
                }
                "seed-04" | "seed-05" | "seed-06" | "seed-10" => {
                    let tokens = tokenize(text, &FILTER_LIMITS, TextKind::Filter)
                        .unwrap_or_else(|e| panic!("{name} tokenize failed: {e}"));
                    let mut p = parser(tokens);
                    let ast = p
                        .parse_expr()
                        .unwrap_or_else(|e| panic!("{name} parse_expr failed: {e}"));
                    expect_end(&mut p, text).expect("expect_end");
                    let expr = bind_expr(&ast, schema)
                        .unwrap_or_else(|e| panic!("{name} bind_expr failed: {e}"));
                    // Masked batch skips row 1 (duration_ns = 0) so division succeeds.
                    eval_expr(&expr, &batch_masked, &ctx, None)
                        .unwrap_or_else(|e| panic!("{name} eval_expr failed: {e}"));
                }
                "seed-07" => {
                    parse_interval(text).expect("seed-07 parse_interval");
                }
                "seed-08" => {
                    parse_timestamp_ns(text, true).expect("seed-08 parse_timestamp_ns");
                }
                "seed-09" => {
                    parse_date_days(text).expect("seed-09 parse_date_days");
                }
                other => panic!("unexpected seed {other}"),
            }
        }
    }

    /// Writes the seed corpora for `query_expr` and `sql` (`just fuzz-seed`).
    #[test]
    #[ignore]
    fn write_seed_corpus() {
        let fuzz_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus");
        let expr_dir = fuzz_dir.join("query_expr");
        std::fs::create_dir_all(&expr_dir).expect("create query_expr corpus directory");
        for (name, text) in SEEDS {
            std::fs::write(expr_dir.join(name), text.as_bytes()).expect("write query_expr seed");
        }

        let sql_dir = fuzz_dir.join("sql");
        std::fs::create_dir_all(&sql_dir).expect("create sql corpus directory");
        for (name, text) in SQL_SEEDS {
            std::fs::write(sql_dir.join(name), text.as_bytes()).expect("write sql seed");
        }
    }
}
