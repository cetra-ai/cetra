//! `lookup_scan` and `scan` on the 3,000-trace file from `fmt_testdata`: the wave counts of the
//! lookups (printed as the first G1 data point), and every result against brute force over
//! `read_column`.
//!
//! Always ignored. Run with:
//! `cargo test --release -- --ignored scan_datagen --nocapture`

use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, AsArray, BooleanArray, RecordBatch, StringArray, new_empty_array,
};
use arrow::compute::{concat, filter};
use arrow::datatypes::TimestampNanosecondType;
use cetra_core::batch::spans_to_record_batch;
use cetra_core::schema::spans_schema;
use fmt_testdata::datagen;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use strata::{
    CmpOp, Config, Lit, Predicate, ScanKind, ScanRequest, StrataFile, StrataWriter, read_column,
};

/// The 3,000 traces, sorted by `(trace_id, start_time)`, in batches of 8,192 spans.
fn datagen_batches() -> Vec<RecordBatch> {
    let world = datagen::World::new(7);
    let mut rng = datagen::Rng::new(42);
    let mut spans = Vec::new();
    for _ in 0..3_000 {
        world.trace(&mut rng, &mut spans);
    }
    spans.sort_by_key(|s| (s.trace_id, s.start_time));
    spans
        .chunks(8192)
        .map(|chunk| spans_to_record_batch(chunk).unwrap())
        .collect()
}

/// Writes `batches` with the default config and opens the result in memory.
async fn write_and_open(batches: &[RecordBatch]) -> StrataFile {
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
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let path = Path::from("datagen.strata");
    store.put(&path, PutPayload::from(bytes)).await.unwrap();
    StrataFile::open(store, path, None, &Config::default())
        .await
        .unwrap()
}

/// Column `column` of `batches`, concatenated.
fn column_of(batches: &[RecordBatch], column: usize) -> ArrayRef {
    let parts: Vec<&dyn Array> = batches.iter().map(|b| b.column(column).as_ref()).collect();
    concat(&parts).unwrap()
}

/// Every column of `file`, read whole: the oracle.
async fn oracle_columns(file: &StrataFile) -> Vec<ArrayRef> {
    let mut out = Vec::new();
    for column in 0..file.schema().fields().len() {
        out.push(read_column(file, column).await.unwrap());
    }
    out
}

/// The rows of `columns` picked by `keep`, for the columns of `projection`.
fn picked(columns: &[ArrayRef], keep: &BooleanArray, projection: &[usize]) -> Vec<ArrayRef> {
    projection
        .iter()
        .map(|&c| filter(columns[c].as_ref(), keep).unwrap())
        .collect()
}

/// Asserts that `got` holds exactly the rows `want` holds, column by column.
fn assert_rows(got: &[RecordBatch], want: &[ArrayRef]) {
    for (i, want) in want.iter().enumerate() {
        let parts: Vec<&dyn Array> = got.iter().map(|b| b.column(i).as_ref()).collect();
        let have = if parts.is_empty() {
            new_empty_array(want.data_type())
        } else {
            concat(&parts).unwrap()
        };
        assert_eq!(have.to_data(), want.to_data(), "column {i} differs");
    }
}

/// Rows whose `input` contains `needle`, by brute force. A null input is not a match.
fn brute_contains(inputs: &StringArray, needle: &[u8]) -> Vec<bool> {
    (0..inputs.len())
        .map(|r| {
            inputs.is_valid(r)
                && inputs
                    .value(r)
                    .as_bytes()
                    .windows(needle.len())
                    .any(|w| w == needle)
        })
        .collect()
}

#[tokio::test]
#[ignore]
async fn scan_datagen_lookup() {
    let batches = datagen_batches();
    let file = write_and_open(&batches).await;
    assert!(file.row_count() > 12_000, "{} rows", file.row_count());
    let schema = file.schema();
    let trace_col = schema.index_of("trace_id").unwrap();
    let input_col = schema.index_of("input").unwrap();
    let all: Vec<usize> = (0..schema.fields().len()).collect();

    // The lookup rows need a non-null `input`, so that the payload waves run. Take the next row
    // that has one, and say so.
    let inputs = column_of(&batches, input_col);
    let row_with_input = |from: usize| {
        let row = (from..inputs.len()).find(|&r| inputs.is_valid(r)).unwrap();
        if row != from {
            println!("row {from} has a null input; using row {row}");
        }
        row
    };
    let traces = column_of(&batches, trace_col);
    let key_of =
        |row: usize| -> [u8; 16] { traces.as_fixed_size_binary().value(row).try_into().unwrap() };
    let cold_row = row_with_input(5_000);
    let warm_row = row_with_input(12_000);
    let (cold_key, warm_key) = (key_of(cold_row), key_of(warm_row));

    // Cold: the first lookup on a freshly opened file.
    file.reset_io_stats();
    let cold = file.lookup_scan(&cold_key, &all, false).await.unwrap();
    let cold_io = file.io_stats();

    // Warm: the metadata is cached, so the lookup of another trace takes fewer waves.
    file.reset_io_stats();
    let warm = file.lookup_scan(&warm_key, &all, false).await.unwrap();
    let warm_io = file.io_stats();

    // An absent key is above no zone: [0; 16] is below every trace id, as their first byte's low
    // bit is always set.
    file.reset_io_stats();
    let absent = file.lookup_scan(&[0u8; 16], &all, false).await.unwrap();
    let absent_io = file.io_stats();

    println!("file {} B, rows {}", file.size(), file.row_count());
    println!(
        "lookup_scan cold (row {cold_row}): waves {}, requests {}, bytes {}",
        cold_io.waves, cold_io.requests, cold_io.bytes
    );
    println!(
        "lookup_scan warm (row {warm_row}): waves {}, requests {}, bytes {}",
        warm_io.waves, warm_io.requests, warm_io.bytes
    );
    println!(
        "lookup_scan absent: waves {}, requests {}, bytes {}",
        absent_io.waves, absent_io.requests, absent_io.bytes
    );
    assert_eq!(cold_io.waves, 3, "cold waves");
    assert_eq!(warm_io.waves, 2, "warm waves");
    assert_eq!(absent_io.waves, 0, "absent waves");
    assert!(absent.is_empty());

    // The results against the oracle: every column, filtered on `trace_id`.
    let columns = oracle_columns(&file).await;
    for (key, got) in [(cold_key, &cold), (warm_key, &warm)] {
        let keep: BooleanArray = columns[trace_col]
            .as_fixed_size_binary()
            .iter()
            .map(|v| Some(v == Some(&key[..])))
            .collect();
        assert!(keep.true_count() > 0);
        assert_rows(got, &picked(&columns, &keep, &all));
    }
}

#[tokio::test]
#[ignore]
async fn scan_datagen_scan() {
    let batches = datagen_batches();
    let file = write_and_open(&batches).await;
    assert!(file.row_count() > 12_000, "{} rows", file.row_count());
    let schema = file.schema();
    let col = |name: &str| schema.index_of(name).unwrap();
    let (trace_id, input, output, start_time, span_kind) = (
        col("trace_id"),
        col("input"),
        col("output"),
        col("start_time"),
        col("span_kind"),
    );
    let columns = oracle_columns(&file).await;
    let starts = columns[start_time].as_primitive::<TimestampNanosecondType>();
    let inputs = columns[input].as_string::<i32>();
    let kinds = columns[span_kind].as_string::<i32>();
    let projection = [trace_id, input, output];

    // Payload projection: the spans of a 10-minute window from row 0's start time.
    let t0 = starts.value(0);
    let t1 = t0 + 600_000_000_000;
    let window = Predicate::And(vec![
        Predicate::Cmp(start_time, CmpOp::Gt, Lit::Int(t0 - 1)),
        Predicate::Cmp(start_time, CmpOp::Lt, Lit::Int(t1)),
    ]);
    let keep: BooleanArray = (0..starts.len())
        .map(|r| Some(starts.is_valid(r) && starts.value(r) >= t0 && starts.value(r) < t1))
        .collect();
    assert!(keep.true_count() > 0, "the window holds no span");
    let got = file
        .scan(&ScanRequest {
            projection: projection.to_vec(),
            predicate: Some(window),
            row_ranges: None,
            limit: None,
            kind: ScanKind::Scan,
            views: false,
        })
        .await
        .unwrap();
    assert_rows(&got, &picked(&columns, &keep, &projection));

    // Contains: 20 needles taken from random inputs, and 10 that no input holds.
    let mut rng = datagen::Rng::new(9);
    let mut needles: Vec<Vec<u8>> = Vec::new();
    let mut first_row = 0;
    while needles.len() < 20 {
        let row = rng.below(inputs.len());
        if !inputs.is_valid(row) {
            continue;
        }
        let value = inputs.value(row).as_bytes();
        let len = 8 + rng.below(25);
        if value.len() < len {
            continue;
        }
        let start = rng.below(value.len() - len + 1);
        if needles.is_empty() {
            first_row = row;
        }
        needles.push(value[start..start + len].to_vec());
    }
    let first_present = needles[0].clone();
    for i in 0..10 {
        needles.push(format!("\u{1}absent-{i}\u{2}").into_bytes());
    }
    for (i, needle) in needles.iter().enumerate() {
        let matches = brute_contains(inputs, needle);
        if i >= 20 {
            assert!(!matches.iter().any(|&m| m), "absent needle {i} matches");
        }
        let keep = BooleanArray::from(matches);
        let got = file
            .scan(&ScanRequest {
                projection: vec![trace_id],
                predicate: Some(Predicate::Contains(input, needle.clone())),
                row_ranges: None,
                limit: None,
                kind: ScanKind::Scan,
                views: false,
            })
            .await
            .unwrap();
        assert_rows(&got, &picked(&columns, &keep, &[trace_id]));
    }

    // Combined: `Contains` and an equality on the narrow `span_kind` of the row the first
    // needle comes from, so the result holds at least that row.
    let kind0 = kinds.value(first_row).to_string();
    let contains = brute_contains(inputs, &first_present);
    let keep: BooleanArray = contains
        .iter()
        .enumerate()
        .map(|(r, &m)| Some(m && kinds.is_valid(r) && kinds.value(r) == kind0))
        .collect();
    println!("combined predicate: {} rows", keep.true_count());
    assert!(keep.true_count() > 0);
    let got = file
        .scan(&ScanRequest {
            projection: projection.to_vec(),
            predicate: Some(Predicate::And(vec![
                Predicate::Contains(input, first_present),
                Predicate::Cmp(span_kind, CmpOp::Eq, Lit::Utf8(kind0)),
            ])),
            row_ranges: None,
            limit: None,
            kind: ScanKind::Scan,
            views: false,
        })
        .await
        .unwrap();
    assert_rows(&got, &picked(&columns, &keep, &projection));
}
