//! The payload round trip: 3,000 traces from `fmt_testdata` are written with their payload
//! fields intact, read back column by column, and sized against Parquet with the `pq_default`
//! properties. The payload columns are reported with their payload statistics.
//!
//! Always ignored. Run with:
//! `cargo test --release -- --ignored --nocapture roundtrip_payload`

use std::collections::BTreeMap;
use std::sync::Arc;

use arrow::array::{Array, AsArray, RecordBatch};
use arrow::compute::concat;
use bytes::Bytes;
use fmt_testdata::datagen;
use llmobs_core::batch::spans_to_record_batch;
use llmobs_core::schema::spans_schema;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use parquet::file::reader::FileReader;
use parquet::file::serialized_reader::SerializedFileReader;
use strata::{Config, StrataFile, StrataWriter, read_column};

/// Total length of the values of a Utf8 or Binary array, nulls skipped.
fn raw_value_bytes(array: &dyn Array) -> u64 {
    if let Some(strings) = array.as_string_opt::<i32>() {
        strings.iter().flatten().map(|v| v.len() as u64).sum()
    } else if let Some(binaries) = array.as_binary_opt::<i32>() {
        binaries.iter().flatten().map(|v| v.len() as u64).sum()
    } else {
        0
    }
}

#[tokio::test]
#[ignore]
async fn roundtrip_payload() {
    let world = datagen::World::new(7);
    let mut rng = datagen::Rng::new(42);
    let mut spans = Vec::new();
    for _ in 0..3_000 {
        world.trace(&mut rng, &mut spans);
    }
    spans.sort_by_key(|s| (s.trace_id, s.start_time));
    let batches: Vec<RecordBatch> = spans
        .chunks(8192)
        .map(|chunk| spans_to_record_batch(chunk).unwrap())
        .collect();

    let schema = spans_schema();
    let mut writer = StrataWriter::new(
        Arc::clone(&schema),
        &["trace_id", "start_time"],
        Config::default(),
    )
    .unwrap();
    for batch in &batches {
        writer.write(batch).unwrap();
    }
    let (bytes, summary) = writer.finish().unwrap();

    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let path = Path::from("spans.strata");
    store.put(&path, PutPayload::from(bytes)).await.unwrap();
    let file = StrataFile::open(Arc::clone(&store), path, None, &Config::default())
        .await
        .unwrap();
    assert_eq!(file.row_count(), spans.len() as u64);

    let input = schema.index_of("input").unwrap();
    assert!(
        summary.columns[input].payload.is_some(),
        "input must be a payload column"
    );

    let mut expected_columns = Vec::with_capacity(schema.fields().len());
    for (column, field) in schema.fields().iter().enumerate() {
        let decoded = read_column(&file, column).await.unwrap();
        let arrays: Vec<&dyn Array> = batches.iter().map(|b| b.column(column).as_ref()).collect();
        let expected = concat(&arrays).unwrap();
        assert!(
            decoded.to_data() == expected.to_data(),
            "column {} differs",
            field.name()
        );
        expected_columns.push(expected);
    }

    // The same batches with the bench's `pq_default` properties: zstd level 3, defaults otherwise.
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3).unwrap()))
        .build();
    let mut parquet = Vec::new();
    {
        let mut w = ArrowWriter::try_new(&mut parquet, Arc::clone(&schema), Some(props)).unwrap();
        for batch in &batches {
            w.write(batch).unwrap();
        }
        w.close().unwrap();
    }
    let reader = SerializedFileReader::new(Bytes::from(parquet.clone())).unwrap();
    let mut parquet_columns: BTreeMap<String, i64> = BTreeMap::new();
    for row_group in reader.metadata().row_groups() {
        for column in row_group.columns() {
            let name = column.column_path().parts()[0].clone();
            *parquet_columns.entry(name).or_default() += column.compressed_size();
        }
    }

    println!(
        "rows {}, strata hot tail {} B, strata chunks {}",
        summary.rows,
        summary.hot_len,
        summary.columns.iter().map(|c| c.chunks).sum::<u32>()
    );
    println!(
        "{:<22} {:<8} {:>12} {:>12} {:>12} {:>8} {:>8} {:>12} {:>8}",
        "column",
        "layer",
        "strata B",
        "pq_default B",
        "raw value B",
        "uniq",
        "frames",
        "chunk refs",
        "ratio"
    );
    for (summary_column, expected) in summary.columns.iter().zip(&expected_columns) {
        let pq = parquet_columns
            .get(&summary_column.name)
            .copied()
            .unwrap_or(0);
        let ratio = summary_column.bytes as f64 / pq.max(1) as f64;
        match &summary_column.payload {
            Some(stats) => println!(
                "{:<22} {:<8} {:>12} {:>12} {:>12} {:>8} {:>8} {:>12} {:>8.2}",
                summary_column.name,
                "payload",
                summary_column.bytes,
                pq,
                raw_value_bytes(expected.as_ref()),
                stats.unique_chunks,
                stats.frames,
                stats.chunk_refs,
                ratio
            ),
            None => println!(
                "{:<22} {:<8} {:>12} {:>12} {:>12} {:>8} {:>8} {:>12} {:>8.2}",
                summary_column.name, "narrow", summary_column.bytes, pq, "-", "-", "-", "-", ratio
            ),
        }
    }
    let pq_total = parquet.len() as u64;
    println!(
        "{:<22} {:>12} {:>12} {:>7.2}",
        "TOTAL (file bytes)",
        summary.bytes,
        pq_total,
        summary.bytes as f64 / pq_total as f64
    );
}
