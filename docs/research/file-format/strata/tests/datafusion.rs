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
use cetra_core::schema::spans_schema;
use datafusion::datasource::MemTable;
use datafusion::physical_plan::displayable;
use datafusion::prelude::{ParquetReadOptions, SessionContext};
use dataset::rows;
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

    // Pushdown: the exact filters, the substring included, leave no FilterExec; ORDER BY trace_id merges the sorted files without a sort.
    for name in [
        "count_errors_6h",
        "user_filter",
        "metadata_filter",
        "substring_input",
    ] {
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
    // NOT LIKE is not pushed (it differs on null rows) and gives Parquet's rows.
    let sql = "SELECT count(*) FROM t WHERE NOT (input LIKE '%zqxjneedle%')";
    assert!(plan_text(&st, sql).await.contains("FilterExec"));
    let st_not = st.sql(sql).await.unwrap().collect().await.unwrap();
    let pq_not = pq.sql(sql).await.unwrap().collect().await.unwrap();
    assert_eq!(rows(&st_not), rows(&pq_not), "NOT LIKE");
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

/// Ranges on dates and on timestamps of every unit push down exactly, with no `FilterExec`, and
/// keep the rows DataFusion keeps in memory.
#[tokio::test]
async fn date_and_timestamp_ranges_push_down() {
    use arrow::array::{
        Date32Array, Date64Array, TimestampMicrosecondArray, TimestampMillisecondArray,
        TimestampSecondArray,
    };
    let n: i64 = 3_000;
    let day_ms = 86_400_000i64;
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("s", DataType::Timestamp(TimeUnit::Second, None), true),
        Field::new(
            "ms",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            true,
        ),
        Field::new("us", DataType::Timestamp(TimeUnit::Microsecond, None), true),
        Field::new("d32", DataType::Date32, true),
        Field::new("d64", DataType::Date64, true),
    ]));
    // A day a row from 2013-01-01 (15706 days after the epoch), a null every 17 rows.
    let day = |i: i64| (i % 17 != 0).then_some(15_706 + i);
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Int64Array::from_iter_values(0..n)),
            Arc::new(TimestampSecondArray::from_iter(
                (0..n).map(|i| day(i).map(|d| d * 86_400 + 5)),
            )),
            Arc::new(
                TimestampMillisecondArray::from_iter((0..n).map(|i| day(i).map(|d| d * day_ms)))
                    .with_timezone("UTC"),
            ),
            Arc::new(TimestampMicrosecondArray::from_iter(
                (0..n).map(|i| day(i).map(|d| d * day_ms * 1_000 + 7)),
            )),
            Arc::new(Date32Array::from_iter(
                (0..n).map(|i| day(i).map(|d| d as i32)),
            )),
            Arc::new(Date64Array::from_iter(
                (0..n).map(|i| day(i).map(|d| d * day_ms)),
            )),
        ],
    )
    .unwrap();
    let mut writer = StrataWriter::new(
        schema.clone(),
        &[],
        Config {
            chunk_max_rows: 256,
            ..Config::default()
        },
    )
    .unwrap();
    writer.write(&batch).unwrap();
    let (bytes, _) = writer.finish().unwrap();
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let path = Path::from("dates.strata");
    store.put(&path, PutPayload::from(bytes)).await.unwrap();
    let table = StrataTable::open(store, vec![path], &Config::default())
        .await
        .unwrap();
    let ctx = SessionContext::new();
    ctx.register_table("t", Arc::new(table)).unwrap();
    ctx.register_table(
        "m",
        Arc::new(MemTable::try_new(schema, vec![vec![batch]]).unwrap()),
    )
    .unwrap();

    for filter in [
        "s >= TIMESTAMP '2014-03-01 00:00:00' AND s < TIMESTAMP '2014-04-01 00:00:00'",
        "ms > TIMESTAMP '2015-06-30T12:00:00Z'",
        "us <= TIMESTAMP '2013-02-01 00:00:00.000007'",
        "us >= TIMESTAMP '2013-07-14' AND us <= TIMESTAMP '2013-07-15'",
        "d32 BETWEEN DATE '2013-07-01' AND DATE '2013-07-31'",
        "d32 = DATE '2014-01-01'",
        "d64 < DATE '2013-01-20'",
    ] {
        let strata = sorted_rows(&ctx, &format!("SELECT id FROM t WHERE {filter}")).await;
        let memtable = sorted_rows(&ctx, &format!("SELECT id FROM m WHERE {filter}")).await;
        assert!(
            !memtable.is_empty() && memtable.len() < n as usize,
            "`{filter}` keeps some rows"
        );
        assert_eq!(strata, memtable, "rows of `{filter}`");
        let plan = plan_text(&ctx, &format!("SELECT id FROM t WHERE {filter}")).await;
        assert!(plan.contains("predicate="), "`{filter}` is pushed:\n{plan}");
        assert!(
            !plan.contains("FilterExec"),
            "`{filter}` keeps a FilterExec:\n{plan}"
        );
    }
}

/// Filters Strata does not translate run in the scan as row filters: functions of columns,
/// fields of a struct column, floats. Their rows match DataFusion's over the same rows in memory,
/// with a limit, a pushed sort and `count(*)` too. DataFusion's `FilterExec` stays above the
/// scan, so the plan's row estimates are those of the filter.
#[tokio::test(flavor = "multi_thread")]
async fn row_filters_match_datafusion() {
    use arrow::array::{
        ArrayRef, DictionaryArray, ListArray, StringArray, StructArray, UInt32Array,
    };
    use arrow::datatypes::{Fields, Int64Type, UInt32Type};
    let n: usize = 5_000;
    let label_fields = Fields::from(vec![
        Field::new(
            "comm",
            DataType::Dictionary(Box::new(DataType::UInt32), Box::new(DataType::Utf8)),
            true,
        ),
        Field::new("cpu", DataType::Utf8, true),
    ]);
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("s", DataType::Utf8, true),
        Field::new("labels", DataType::Struct(label_fields.clone()), false),
        Field::new(
            "stack",
            DataType::List(Arc::new(Field::new("item", DataType::Int64, true))),
            true,
        ),
        Field::new("f", DataType::Float64, true),
        Field::new("body", DataType::Utf8, false),
    ]));
    let ids = Int64Array::from_iter_values(0..n as i64);
    let s =
        StringArray::from_iter((0..n).map(|i| (i % 11 != 0).then(|| format!("Name_{}", i % 37))));
    let comm_values = StringArray::from_iter_values((0..50).map(|c| format!("comm_{c}")));
    let comm = DictionaryArray::<UInt32Type>::try_new(
        UInt32Array::from_iter((0..n).map(|i| (i % 97 != 0).then_some(((i * 7) % 50) as u32))),
        Arc::new(comm_values),
    )
    .unwrap();
    let cpu =
        StringArray::from_iter((0..n).map(|i| (i % 5 != 0).then(|| format!("CPU_{}", i % 16))));
    let labels = StructArray::try_new(
        label_fields,
        vec![Arc::new(comm) as ArrayRef, Arc::new(cpu)],
        None,
    )
    .unwrap();
    let stack = ListArray::from_iter_primitive::<Int64Type, _, _>((0..n).map(|i| {
        (i % 13 != 0).then(|| {
            (0..(i % 6) as i64)
                .map(|v| Some(v * i as i64))
                .collect::<Vec<_>>()
        })
    }));
    let f =
        Float64Array::from_iter((0..n).map(|i| (i % 9 != 0).then_some((i % 100) as f64 / 100.0)));
    // A wide column, 200 pseudo-random hex digits a row, so that a row filter pays for
    // queries that project it.
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let body = StringArray::from_iter_values((0..n).map(|_| {
        (0..25)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                format!("{:08x}", state >> 32)
            })
            .collect::<String>()
    }));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(ids),
            Arc::new(s),
            Arc::new(labels),
            Arc::new(stack),
            Arc::new(f),
            Arc::new(body),
        ],
    )
    .unwrap();

    // Small chunks, so that the filter and the projection see many of them.
    let mut writer = StrataWriter::new(
        schema.clone(),
        &[],
        Config {
            chunk_max_rows: 256,
            ..Config::default()
        },
    )
    .unwrap();
    writer.write(&batch).unwrap();
    let (bytes, _) = writer.finish().unwrap();
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let path = Path::from("rows.strata");
    store.put(&path, PutPayload::from(bytes)).await.unwrap();
    let table = StrataTable::open(Arc::clone(&store), vec![path], &Config::default())
        .await
        .unwrap();
    let ctx = SessionContext::new();
    ctx.register_table("t", Arc::new(table)).unwrap();
    let memory = MemTable::try_new(schema.clone(), vec![vec![batch.clone()]]).unwrap();
    ctx.register_table("m", Arc::new(memory)).unwrap();

    let filters = [
        "lower(s) = 'name_3'",
        "labels.comm = 'comm_5'",
        "labels.comm ILIKE 'COMM\\_1%' ESCAPE '\\'",
        "length(s) > 6 AND id > 1000",
        "starts_with(lower(labels.cpu), 'cpu_1') OR s IS NULL",
        "coalesce(labels.cpu, 'none') = 'none'",
        "id % 13 = 4 AND f > 0.5",
        "array_length(stack) > 3",
        "NOT (lower(s) = 'name_3')",
    ];
    for filter in filters {
        let strata = sorted_rows(&ctx, &format!("SELECT * FROM t WHERE {filter}")).await;
        let memtable = sorted_rows(&ctx, &format!("SELECT * FROM m WHERE {filter}")).await;
        println!(
            "row_filters [{filter}]: strata {} rows, memtable {} rows",
            strata.len(),
            memtable.len()
        );
        assert!(
            !memtable.is_empty() && memtable.len() < n,
            "`{filter}` keeps some rows"
        );
        assert_eq!(strata, memtable, "rows of `{filter}`");
    }
    for sql in [
        "SELECT id, labels FROM {} WHERE lower(s) = 'name_3' ORDER BY id LIMIT 5",
        "SELECT id FROM {} WHERE labels.comm = 'comm_5' ORDER BY f DESC, id LIMIT 7",
        "SELECT count(*) FROM {} WHERE lower(s) = 'name_3'",
        "SELECT count(*), sum(id) FROM {} WHERE labels.comm = 'comm_5' AND id > 100",
    ] {
        let strata = sorted_rows(&ctx, &sql.replace("{}", "t")).await;
        let memtable = sorted_rows(&ctx, &sql.replace("{}", "m")).await;
        assert_eq!(strata, memtable, "`{sql}`");
    }
    // A LIMIT without an order keeps any rows the filter keeps.
    let limited = sorted_rows(
        &ctx,
        "SELECT id, s FROM t WHERE lower(s) = 'name_3' LIMIT 4",
    )
    .await;
    let all = sorted_rows(&ctx, "SELECT id, s FROM m WHERE lower(s) = 'name_3'").await;
    assert_eq!(limited.len(), 4);
    assert!(limited.iter().all(|row| all.contains(row)));

    // The row filter runs when the projection holds a column much wider than the filter's
    // (`body`), and takes the `FilterExec` over; it is left to `FilterExec` otherwise.
    let plan = plan_text(
        &ctx,
        "SELECT id, body FROM t WHERE lower(s) = 'name_3' AND id > 10",
    )
    .await;
    assert!(plan.contains("row_filter="), "{plan}");
    assert!(!plan.contains("FilterExec"), "{plan}");
    // With no `FilterExec` left, a sort goes into the scan, which runs the row filter.
    let sql = "SELECT id, body FROM {} WHERE lower(s) = 'name_3' ORDER BY id DESC LIMIT 6";
    let plan = plan_text(&ctx, &sql.replace("{}", "t")).await;
    assert!(
        plan.contains("order=[") && plan.contains("row_filter="),
        "{plan}"
    );
    let strata = sorted_rows(&ctx, &sql.replace("{}", "t")).await;
    let memtable = sorted_rows(&ctx, &sql.replace("{}", "m")).await;
    assert_eq!(strata.len(), 6);
    assert_eq!(strata, memtable, "`{sql}`");
    // Over two files, the sort and its limit go into the scan, which reads the files in stages
    // with the row filter. (A projection above the scan would keep the limit out of it.)
    let half = batch.num_rows() / 2;
    let mut paths = Vec::new();
    for (name, part) in [
        ("a.strata", batch.slice(0, half)),
        ("b.strata", batch.slice(half, batch.num_rows() - half)),
    ] {
        let mut writer = StrataWriter::new(
            schema.clone(),
            &[],
            Config {
                chunk_max_rows: 256,
                ..Config::default()
            },
        )
        .unwrap();
        writer.write(&part).unwrap();
        let (bytes, _) = writer.finish().unwrap();
        let path = Path::from(name);
        store.put(&path, PutPayload::from(bytes)).await.unwrap();
        paths.push(path);
    }
    let two = StrataTable::open(Arc::clone(&store), paths, &Config::default())
        .await
        .unwrap();
    ctx.register_table("t2", Arc::new(two)).unwrap();
    for sql in [
        "SELECT * FROM {} WHERE lower(s) = 'name_3' ORDER BY id DESC LIMIT 6",
        "SELECT * FROM {} WHERE lower(s) LIKE '%3%' ORDER BY id LIMIT 9",
    ] {
        let plan = plan_text(&ctx, &sql.replace("{}", "t2")).await;
        assert!(
            plan.contains("staged") && plan.contains("row_filter="),
            "{plan}"
        );
        let strata = sorted_rows(&ctx, &sql.replace("{}", "t2")).await;
        let memtable = sorted_rows(&ctx, &sql.replace("{}", "m")).await;
        assert!(!strata.is_empty());
        assert_eq!(strata, memtable, "`{sql}`");
    }
    let plan = plan_text(
        &ctx,
        "SELECT id, s FROM t WHERE lower(s) = 'name_3' AND id > 10",
    )
    .await;
    assert!(!plan.contains("row_filter="), "{plan}");
    assert!(plan.contains("FilterExec"), "{plan}");
}
