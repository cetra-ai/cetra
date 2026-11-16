//! `trace_detail` and `trace_list` against DataFusion over the same Strata files: the same rows
//! in the same order, and an early stop that reads fewer requests for `k = 1`.
//!
//! The DataFusion comparisons are always ignored. Run with:
//! `cargo test --release -- --ignored fast_paths --nocapture`
//!
//! `trace_list_with_no_column_counts_the_rows` runs in a plain `cargo test`.

#[path = "common/dataset.rs"]
mod dataset;

use std::ops::Range;
use std::sync::Arc;

use arrow::array::RecordBatch;
use arrow::compute::{SortColumn, concat_batches, lexsort_to_indices, take_record_batch};
use datafusion::prelude::SessionContext;
use fmt_testdata::datagen::{DAY_START, NS};
use cetra_core::schema::spans_schema;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use strata::{
    CmpOp, Config, Lit, Predicate, StrataError, StrataFile, StrataTable, StrataWriter,
    trace_detail, trace_list,
};

use dataset::rows;

/// The columns of the `trace_list` queries.
const LIST_COLUMNS: [&str; 7] = [
    "trace_id",
    "span_id",
    "name",
    "start_time",
    "duration_ns",
    "status",
    "model",
];

/// The 16 bytes of a lowercase hex trace id.
fn trace_bytes(id: &str) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&id[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

/// The `to_timestamp_nanos` SQL call of `ns`.
fn ts(ns: i64) -> String {
    format!("to_timestamp_nanos({ns})")
}

/// The DataFusion query of `trace_list`: the latest LLM spans of `window`, with S9's tie-break.
fn list_sql(table: &str, window: &Range<i64>, k: usize) -> String {
    format!(
        "SELECT {} FROM {table} WHERE span_kind = 'llm' AND start_time >= {} AND start_time < {} ORDER BY start_time DESC, trace_id DESC, span_id DESC LIMIT {k}",
        LIST_COLUMNS.join(", "),
        ts(window.start),
        ts(window.end),
    )
}

/// `span_kind = 'llm'`, by the schema index of `span_kind`.
fn llm(files: &[Arc<StrataFile>]) -> Predicate {
    let index = files
        .first()
        .unwrap()
        .schema()
        .index_of("span_kind")
        .unwrap();
    Predicate::Cmp(index, CmpOp::Eq, Lit::Utf8("llm".to_string()))
}

/// Writes each slice as a Strata file with `sort_key`, in `store`, and opens it.
async fn write_files(
    store: &Arc<dyn ObjectStore>,
    slices: &[Vec<RecordBatch>],
    sort_key: &[&str],
    config: Config,
    prefix: &str,
) -> Vec<Arc<StrataFile>> {
    let mut files = Vec::new();
    for (i, batches) in slices.iter().enumerate() {
        let mut writer = StrataWriter::new(spans_schema(), sort_key, config.clone()).unwrap();
        for batch in batches {
            writer.write(batch).unwrap();
        }
        let (bytes, _) = writer.finish().unwrap();
        let path = Path::from(format!("{prefix}-{i:03}.strata"));
        store.put(&path, PutPayload::from(bytes)).await.unwrap();
        let file = StrataFile::open(Arc::clone(store), path, None, &Config::default())
            .await
            .unwrap();
        files.push(Arc::new(file));
    }
    files
}

/// The batches of one slice, sorted by `(start_time, trace_id, span_id)` and cut into pieces of
/// 8,192 rows.
fn sort_by_time(batches: &[RecordBatch]) -> Vec<RecordBatch> {
    let batch = concat_batches(&spans_schema(), batches).unwrap();
    let columns: Vec<SortColumn> = ["start_time", "trace_id", "span_id"]
        .iter()
        .map(|name| SortColumn {
            values: batch.column_by_name(name).unwrap().clone(),
            options: None,
        })
        .collect();
    let order = lexsort_to_indices(&columns, None).unwrap();
    let sorted = take_record_batch(&batch, &order).unwrap();
    let mut pieces = Vec::new();
    let mut offset = 0;
    while offset < sorted.num_rows() {
        let len = (sorted.num_rows() - offset).min(8_192);
        pieces.push(sorted.slice(offset, len));
        offset += len;
    }
    pieces
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn fast_paths_match_datafusion() {
    let data = dataset::spans_dataset(2_000, 3);
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

    // The same spans in the same time slices: sorted by trace, then by time, and by time only.
    let files = write_files(
        &store,
        &data.files,
        &["trace_id", "start_time"],
        Config::default(),
        "by-trace",
    )
    .await;
    let sorted: Vec<Vec<RecordBatch>> = data.files.iter().map(|b| sort_by_time(b)).collect();
    let timed = write_files(
        &store,
        &sorted,
        &["start_time", "trace_id", "span_id"],
        Config {
            chunk_max_rows: 256,
            ..Config::default()
        },
        "by-time",
    )
    .await;

    let ctx = SessionContext::new();
    ctx.register_table("t", Arc::new(StrataTable::new(files.clone()).unwrap()))
        .unwrap();
    ctx.register_table("t2", Arc::new(StrataTable::new(timed.clone()).unwrap()))
        .unwrap();

    // trace_detail: every sampled trace, as a sorted row list.
    let detail_columns = [
        "span_id",
        "parent_span_id",
        "name",
        "start_time",
        "end_time",
        "status",
        "model",
        "total_tokens",
        "cost_total",
        "input",
    ];
    for id in &data.trace_ids {
        let mut got = rows(&[trace_detail(&files, trace_bytes(id), &detail_columns)
            .await
            .unwrap()]);
        let sql = format!(
            "SELECT {} FROM t WHERE trace_id = X'{id}'",
            detail_columns.join(", ")
        );
        let batches = ctx.sql(&sql).await.unwrap().collect().await.unwrap();
        let mut expected = rows(&batches);
        assert!(!expected.is_empty(), "trace {id} has no span");
        got.sort();
        expected.sort();
        assert_eq!(got, expected, "trace_detail {id}");
    }
    println!("trace_detail: {} traces equal", data.trace_ids.len());

    // trace_list: ten one-hour windows, in order, against DataFusion over the same files.
    let hour = 3_600 * NS;
    let t0 = DAY_START;
    let windows: Vec<Range<i64>> = (8..=17)
        .map(|start_hour| t0 + start_hour * hour..t0 + (start_hour + 1) * hour)
        .collect();
    let mut total = 0;
    for window in &windows {
        let got = trace_list(&files, window.clone(), Some(llm(&files)), 50, &LIST_COLUMNS)
            .await
            .unwrap();
        let batches = ctx
            .sql(&list_sql("t", window, 50))
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let expected = rows(&batches);
        total += expected.len();
        assert_eq!(rows(&[got]), expected, "trace_list window {window:?}");
    }
    assert!(total > 0, "no window has an LLM span");
    println!("trace_list: 10 windows equal, {total} rows");

    // k = 0, and a window with no span, give empty batches of the table schema.
    let first = windows.first().unwrap().clone();
    let none = trace_list(&files, first, Some(llm(&files)), 0, &LIST_COLUMNS)
        .await
        .unwrap();
    assert_eq!(none.num_rows(), 0);
    assert_eq!(none.num_columns(), LIST_COLUMNS.len());
    let before = t0 - 2 * hour..t0 - hour;
    let empty = trace_list(&files, before, Some(llm(&files)), 50, &LIST_COLUMNS)
        .await
        .unwrap();
    assert_eq!(empty.num_rows(), 0);
    assert_eq!(empty.num_columns(), LIST_COLUMNS.len());

    // Unknown column names are Schema errors, for both functions.
    let err = trace_list(&files, windows[0].clone(), None, 5, &["no_such_column"])
        .await
        .unwrap_err();
    assert!(matches!(err, StrataError::Schema { .. }), "{err}");
    let err = trace_detail(&files, [0; 16], &["no_such_column"])
        .await
        .unwrap_err();
    assert!(matches!(err, StrataError::Schema { .. }), "{err}");

    // The same windows over the time-sorted files: DataFusion agrees.
    for window in &windows {
        let got = trace_list(&timed, window.clone(), Some(llm(&timed)), 50, &LIST_COLUMNS)
            .await
            .unwrap();
        let batches = ctx
            .sql(&list_sql("t2", window, 50))
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(
            rows(&[got]),
            rows(&batches),
            "trace_list time-sorted window {window:?}"
        );
    }

    // Early stop: with narrow start_time zones, k = 1 reads fewer requests than k = 1,000,000.
    let window = t0 + 12 * hour..t0 + 13 * hour;
    // Warm the metadata, so that the counts below differ only by the chunks read.
    trace_list(
        &timed,
        window.clone(),
        Some(llm(&timed)),
        1_000_000,
        &LIST_COLUMNS,
    )
    .await
    .unwrap();
    let mut requests = Vec::new();
    let mut outputs = Vec::new();
    for k in [1, 1_000_000] {
        for file in &timed {
            file.reset_io_stats();
        }
        let out = trace_list(&timed, window.clone(), Some(llm(&timed)), k, &LIST_COLUMNS)
            .await
            .unwrap();
        let total: u64 = timed.iter().map(|file| file.io_stats().requests).sum();
        requests.push(total);
        outputs.push(out);
    }
    println!(
        "trace_list early stop: k=1 {} requests, k=1000000 {} requests",
        requests[0], requests[1]
    );
    assert!(
        requests[0] < requests[1],
        "k=1 reads {} requests, k=1000000 reads {}",
        requests[0],
        requests[1]
    );
    assert_eq!(outputs[0].num_rows(), 1, "k=1 row count");
    assert_eq!(
        rows(&[outputs[0].clone()]),
        rows(&[outputs[1].slice(0, 1)]),
        "k=1 row is the first row of k=1,000,000"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn trace_list_with_no_column_counts_the_rows() {
    let data = dataset::spans_dataset(200, 1);
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let files = write_files(
        &store,
        &data.files,
        &["trace_id", "start_time"],
        Config::default(),
        "no-column",
    )
    .await;
    let hour = 3_600 * NS;
    let window = DAY_START..DAY_START + 24 * hour;
    let with_key = trace_list(&files, window.clone(), Some(llm(&files)), 5, &["trace_id"])
        .await
        .unwrap();
    assert!(with_key.num_rows() > 0, "the window has llm spans");
    let none = trace_list(&files, window, Some(llm(&files)), 5, &[])
        .await
        .unwrap();
    assert_eq!(none.num_columns(), 0, "no column is requested");
    assert_eq!(none.num_rows(), with_key.num_rows(), "row count");
}

/// `trace_list` over many chunks, with a `k` far below the window: the kept batches are
/// compacted along the way, and the rows still equal DataFusion's top rows.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn fast_paths_trace_list_compacts_a_long_window() {
    // Sorted by trace, so every chunk spans the whole day and the early stop never fires.
    let data = dataset::spans_dataset(8_000, 1);
    let total: usize = data.files.iter().flatten().map(|b| b.num_rows()).sum();
    assert!(
        total > 20_000,
        "the window needs more than 20,000 rows, got {total}"
    );
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let files = write_files(
        &store,
        &data.files,
        &["trace_id", "start_time"],
        Config {
            chunk_max_rows: 256,
            ..Config::default()
        },
        "long-window",
    )
    .await;
    let hour = 3_600 * NS;
    let window = DAY_START..DAY_START + 24 * hour;

    let got = trace_list(&files, window.clone(), Some(llm(&files)), 3, &LIST_COLUMNS)
        .await
        .unwrap();
    let ctx = SessionContext::new();
    ctx.register_table("t", Arc::new(StrataTable::new(files.clone()).unwrap()))
        .unwrap();
    let batches = ctx
        .sql(&list_sql("t", &window, 3))
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(
        rows(&[got]),
        rows(&batches),
        "trace_list k = 3 over {total} spans"
    );
}
