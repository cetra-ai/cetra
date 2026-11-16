//! DataFusion over Strata files against DataFusion over Parquet: every bench query returns the
//! same rows, and the plans show the pushdown and lookup modes that M5.1 promises.
//!
//! `datafusion_equivalence` is always ignored. Run with:
//! `cargo test --release -- --ignored datafusion_equivalence --nocapture`
//!
//! `pushdown_edge_cases` runs in a plain `cargo test`: it compares the rows of filters that the
//! simplifier rewrites and that involve floats, against DataFusion over the same rows in memory.

#[path = "common/dataset.rs"]
mod dataset;

use std::sync::Arc;

use arrow::array::{
    AsArray, FixedSizeBinaryArray, Float64Array, Int64Array, RecordBatch, TimestampNanosecondArray,
};
use arrow::datatypes::{DataType, Field, Int64Type, Schema, TimeUnit};
use datafusion::datasource::MemTable;
use datafusion::physical_plan::displayable;
use datafusion::prelude::{ParquetReadOptions, SessionContext};
use dataset::rows;
use cetra_core::schema::spans_schema;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use parquet::arrow::ArrowWriter;
use strata::{Config, StrataTable, StrataWriter};

/// The first value of the first column of a one-row `count(*)` result.
fn first_count(batches: &[RecordBatch]) -> i64 {
    batches[0].column(0).as_primitive::<Int64Type>().value(0)
}

/// The physical plan of `sql`, indented.
async fn plan_text(ctx: &SessionContext, sql: &str) -> String {
    let plan = ctx
        .sql(sql)
        .await
        .unwrap()
        .create_physical_plan()
        .await
        .unwrap();
    displayable(plan.as_ref()).indent(true).to_string()
}

/// The `files=N` of a `StrataExec` line of a plan.
fn files_in(plan: &str) -> usize {
    plan.split("files=")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(usize::MAX)
}

fn ts(ns: i64) -> String {
    format!("to_timestamp_nanos({ns})")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn datafusion_equivalence() {
    let data = dataset::spans_dataset(2_000, 3);

    // Write each slice as a Strata file (in memory) and as a Parquet file (on disk).
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let dir = std::env::temp_dir().join(format!("strata-df-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut paths = Vec::new();
    for (i, batches) in data.files.iter().enumerate() {
        let mut writer = StrataWriter::new(
            spans_schema(),
            &["trace_id", "start_time"],
            Config::default(),
        )
        .unwrap();
        for batch in batches {
            writer.write(batch).unwrap();
        }
        let (bytes, _) = writer.finish().unwrap();
        let path = Path::from(format!("{i:03}.strata"));
        store.put(&path, PutPayload::from(bytes)).await.unwrap();
        paths.push(path);

        let file = std::fs::File::create(dir.join(format!("{i:03}.parquet"))).unwrap();
        let mut parquet = ArrowWriter::try_new(file, spans_schema(), None).unwrap();
        for batch in batches {
            parquet.write(batch).unwrap();
        }
        parquet.close().unwrap();
    }

    let table = Arc::new(
        StrataTable::open(Arc::clone(&store), paths, &Config::default())
            .await
            .unwrap(),
    );
    let st = SessionContext::new();
    st.register_table("t", table.clone()).unwrap();
    let pq = SessionContext::new();
    let dir_str = dir.to_str().expect("the temp dir path is UTF-8");
    pq.register_parquet("t", dir_str, ParquetReadOptions::default())
        .await
        .unwrap();

    // The bench queries (bench/src/main.rs `query`), with the changes of M5.1.
    let t0 = fmt_testdata::datagen::DAY_START;
    let h = 3_600 * fmt_testdata::datagen::NS;
    let mut queries: Vec<(String, String, bool)> = vec![
        (
            "count_errors_6h".into(),
            format!(
                "SELECT count(*) FROM t WHERE status = 'error' AND start_time >= {} AND start_time < {}",
                ts(t0 + 10 * h),
                ts(t0 + 16 * h)
            ),
            false,
        ),
        (
            "model_rollup_day".into(),
            "SELECT model, count(*), sum(total_tokens), sum(cost_total), avg(duration_ns) FROM t WHERE span_kind = 'llm' GROUP BY model ORDER BY model".into(),
            true,
        ),
        (
            "list_llm_1h".into(),
            format!(
                "SELECT trace_id, span_id, name, start_time, duration_ns, status, model, total_tokens, cost_total, input_preview FROM t WHERE span_kind = 'llm' AND start_time >= {} AND start_time < {} ORDER BY start_time DESC, trace_id DESC, span_id DESC LIMIT 50",
                ts(t0 + 14 * h),
                ts(t0 + 15 * h)
            ),
            true,
        ),
        (
            "user_filter".into(),
            format!(
                "SELECT count(*), sum(cost_total) FROM t WHERE user_id = '{}'",
                data.user_id
            ),
            false,
        ),
        (
            "metadata_filter".into(),
            format!(
                "SELECT count(*) FROM t WHERE metadata['customer_id'] = '{}'",
                data.customer_id
            ),
            false,
        ),
        (
            "substring_input".into(),
            "SELECT count(*) FROM t WHERE input LIKE '%zqxjneedle%'".into(),
            false,
        ),
        (
            "scan_all_payload".into(),
            "SELECT sum(length(input)) + sum(length(output)) FROM t".into(),
            false,
        ),
        (
            "order_trace_id".into(),
            "SELECT trace_id FROM t ORDER BY trace_id".into(),
            true,
        ),
        (
            "order_trace_id_limit".into(),
            "SELECT trace_id FROM t ORDER BY trace_id LIMIT 20".into(),
            true,
        ),
        (
            "trace_absent".into(),
            format!("SELECT * FROM t WHERE trace_id = X'{}'", "ff".repeat(16)),
            false,
        ),
    ];
    for (i, id) in data.trace_ids.iter().take(5).enumerate() {
        queries.push((
            format!("trace_lookup_full[{i}]"),
            format!("SELECT * FROM t WHERE trace_id = X'{id}'"),
            false,
        ));
    }
    for (i, id) in data.trace_ids.iter().take(5).enumerate() {
        queries.push((
            format!("trace_lookup_light[{i}]"),
            format!(
                "SELECT span_id, parent_span_id, name, start_time, end_time, status, model, total_tokens, cost_total FROM t WHERE trace_id = X'{id}'"
            ),
            false,
        ));
    }

    // Every query: the same rows from both engines, and non-empty where the bench expects rows.
    for (name, sql, ordered) in &queries {
        let st_batches = st.sql(sql).await.unwrap().collect().await.unwrap();
        let pq_batches = pq.sql(sql).await.unwrap().collect().await.unwrap();
        let mut a = rows(&st_batches);
        let mut b = rows(&pq_batches);
        if !ordered {
            a.sort();
            b.sort();
        }
        if a != b {
            let first = a
                .iter()
                .zip(&b)
                .position(|(x, y)| x != y)
                .unwrap_or(a.len().min(b.len()));
            panic!(
                "{name}: Strata has {} rows, Parquet {}; first difference at row {first}:\n  strata:  {:?}\n  parquet: {:?}",
                a.len(),
                b.len(),
                a.get(first),
                b.get(first)
            );
        }
        match name.as_str() {
            "count_errors_6h" | "user_filter" | "metadata_filter" | "substring_input" => {
                assert!(first_count(&st_batches) > 0, "{name} counts no rows");
            }
            "list_llm_1h" => assert_eq!(a.len(), 50, "list_llm_1h rows"),
            "trace_absent" => assert_eq!(a.len(), 0, "trace_absent rows"),
            _ => {}
        }
        println!("{name}: {} rows, equal", a.len());
    }

    // Pushdown: the exact filters leave no FilterExec; the substring stays as Contains above a
    // FilterExec; ORDER BY trace_id merges the sorted files without a sort.
    for name in ["count_errors_6h", "user_filter", "metadata_filter"] {
        let sql = queries
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, sql, _)| sql.clone())
            .unwrap();
        let plan = plan_text(&st, &sql).await;
        assert!(
            !plan.contains("FilterExec"),
            "{name} keeps a FilterExec:\n{plan}"
        );
    }
    assert!(
        table.exact_map_key("metadata", "customer_id"),
        "metadata['customer_id'] is not an exact map key: the bench query is not exact"
    );
    let plan = plan_text(
        &st,
        "SELECT count(*) FROM t WHERE input LIKE '%zqxjneedle%'",
    )
    .await;
    assert!(
        plan.contains("FilterExec") && plan.contains("Contains"),
        "substring_input plan:\n{plan}"
    );
    let plan = plan_text(&st, "SELECT trace_id FROM t ORDER BY trace_id").await;
    assert!(
        !plan.contains("SortExec") && plan.contains("SortPreservingMergeExec"),
        "order_trace_id plan:\n{plan}"
    );

    // Lookup mode: the trace_id filter is the only filter, so the exec looks up the key in the
    // files that may hold it.
    let id = data.trace_ids.first().expect("a sampled trace id");
    let plan = plan_text(&st, &format!("SELECT * FROM t WHERE trace_id = X'{id}'")).await;
    assert!(plan.contains("lookup=true"), "lookup plan:\n{plan}");
    assert!(files_in(&plan) < 3, "lookup plan reads every file:\n{plan}");

    std::fs::remove_dir_all(&dir).unwrap();
}

/// The rows of `sql`, sorted, so that two plans compare equal when they return the same rows.
async fn sorted_rows(ctx: &SessionContext, sql: &str) -> Vec<String> {
    let batches = ctx.sql(sql).await.unwrap().collect().await.unwrap();
    let mut out = rows(&batches);
    out.sort();
    out
}

/// `count` integer literals starting at `from`, comma separated.
fn literals(from: i64, count: i64) -> String {
    (from..from + count)
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[tokio::test(flavor = "multi_thread")]
async fn pushdown_edge_cases() {
    // 1,000 rows. `i` has nulls, `f` holds -0.0, 0.0, 1.5, NaN and null, and `ts` is in UTC.
    let n: usize = 1_000;
    let schema = Arc::new(Schema::new(vec![
        Field::new("trace_id", DataType::FixedSizeBinary(16), false),
        Field::new("i", DataType::Int64, true),
        Field::new("f", DataType::Float64, true),
        Field::new(
            "ts",
            DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
            false,
        ),
    ]));
    let ids: Vec<[u8; 16]> = (0..n)
        .map(|i| {
            let mut id = [0u8; 16];
            id[8..].copy_from_slice(&((i / 2) as u64).to_be_bytes());
            id
        })
        .collect();
    let ints = Int64Array::from_iter((0..n).map(|i| {
        if i % 5 == 0 {
            None
        } else {
            Some((i as i64 * 37) % 1_000)
        }
    }));
    let floats = Float64Array::from_iter((0..n).map(|i| match i % 6 {
        0 => None,
        1 => Some(f64::NAN),
        2 => Some(-0.0),
        3 => Some(0.0),
        4 => Some(1.5),
        _ => Some(-2.0),
    }));
    let times = TimestampNanosecondArray::from_iter_values((0..n).map(|i| i as i64 * 1_000))
        .with_timezone("UTC");
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(FixedSizeBinaryArray::try_from_iter(ids.iter()).unwrap()),
            Arc::new(ints),
            Arc::new(floats),
            Arc::new(times),
        ],
    )
    .unwrap();

    // One Strata file, sorted by trace_id, with small chunks so that pruning runs.
    let mut writer = StrataWriter::new(
        schema.clone(),
        &["trace_id"],
        Config {
            chunk_max_rows: 128,
            ..Config::default()
        },
    )
    .unwrap();
    writer.write(&batch).unwrap();
    let (bytes, _) = writer.finish().unwrap();
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let path = Path::from("edge.strata");
    store.put(&path, PutPayload::from(bytes)).await.unwrap();
    let table = StrataTable::open(store, vec![path], &Config::default())
        .await
        .unwrap();

    let ctx = SessionContext::new();
    ctx.register_table("t", Arc::new(table)).unwrap();
    let memory = MemTable::try_new(schema, vec![vec![batch]]).unwrap();
    ctx.register_table("m", Arc::new(memory)).unwrap();

    let mut queries: Vec<String> = [
        "i IN (1, NULL)",
        "i NOT IN (1, NULL)",
        "NOT (i IN (1, NULL))",
        "i IN (1, 2, NULL) OR i IS NULL",
        "i IN (NULL)",
        "i NOT IN (NULL)",
        "f = 0",
        "f <> 0",
        "f IN (0.0)",
        "f < 0",
        "f >= 0",
        "f BETWEEN 0 AND 0",
        "f > -0.0",
        "f = 'NaN'::double",
        "ts >= TIMESTAMP '1970-01-01T00:00:00.0005Z'",
        "i > 5 AND NOT (i < 900 OR i IS NULL)",
    ]
    .iter()
    .map(|predicate| predicate.to_string())
    .collect();
    // Two lists of 2,100 literals: the filters must not fail, whatever the size limit does.
    queries.push(format!(
        "i IN ({}) AND i IN ({})",
        literals(0, 2_100),
        literals(2_100, 2_100)
    ));

    for predicate in &queries {
        let strata = sorted_rows(&ctx, &format!("SELECT * FROM t WHERE {predicate}")).await;
        let memtable = sorted_rows(&ctx, &format!("SELECT * FROM m WHERE {predicate}")).await;
        println!(
            "pushdown_edge_cases [{}]: strata {} rows, memtable {} rows",
            if predicate.len() > 60 {
                "IN list".to_string()
            } else {
                predicate.clone()
            },
            strata.len(),
            memtable.len()
        );
        assert_eq!(strata.len(), memtable.len(), "row count of `{predicate}`");
        assert_eq!(strata, memtable, "rows of `{predicate}`");
    }
}
