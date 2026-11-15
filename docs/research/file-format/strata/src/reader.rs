//! Reading one column back as an Arrow array.

use arrow::array::{Array, ArrayRef, new_empty_array};
use arrow::compute::concat;
use futures::future::try_join_all;

use crate::error::{Result, StrataError};
use crate::file::StrataFile;
use crate::narrow::decode_chunk;
use crate::segment::Kind;

/// Decodes every row of column `column` (its schema index).
///
/// Chunk segments are fetched concurrently, decoded in row order and concatenated. A file
/// with no rows gives an empty array of the column's type.
pub async fn read_column(file: &StrataFile, column: usize) -> Result<ArrayRef> {
    let schema = file.schema();
    let field = schema
        .fields()
        .get(column)
        .ok_or_else(|| StrataError::Schema {
            reason: format!("no column {column}"),
        })?;
    let entries = file.block_entries(column).await?;
    if entries.is_empty() {
        return Ok(new_empty_array(field.data_type()));
    }

    let bodies = try_join_all(entries.iter().map(|entry| async move {
        if entry.segment_len == 0 {
            return Ok::<Option<Vec<u8>>, StrataError>(None);
        }
        let segment = crate::segment::SegmentRef {
            offset: entry.segment_offset,
            len: entry.segment_len,
        };
        file.read_segment(&segment, Kind::NarrowChunk)
            .await
            .map(Some)
    }))
    .await?;

    let mut arrays = Vec::with_capacity(entries.len());
    for (entry, body) in entries.iter().zip(&bodies) {
        arrays.push(decode_chunk(entry, body.as_deref(), field)?);
    }
    let refs: Vec<&dyn Array> = arrays.iter().map(|array| array.as_ref()).collect();
    Ok(concat(&refs)?)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::narrow::{Encoding, flags};
    use crate::testgen::{assert_same, batch, schema};
    use crate::writer::StrataWriter;
    use arrow::array::{
        Array, FixedSizeBinaryArray, RecordBatch, StringArray, UInt32Array, new_null_array,
    };
    use arrow::compute::take_record_batch;
    use arrow::datatypes::{DataType, Field, Schema};
    use object_store::memory::InMemory;
    use object_store::path::Path;
    use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
    use std::sync::Arc;

    const SORT: [&str; 2] = ["trace_id", "start_time"];

    async fn open(bytes: Vec<u8>) -> StrataFile {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("table.strata");
        store.put(&path, PutPayload::from(bytes)).await.unwrap();
        StrataFile::open(store, path, None, &Config::default())
            .await
            .unwrap()
    }

    fn writer(schema: arrow::datatypes::SchemaRef) -> StrataWriter {
        StrataWriter::new(schema, &SORT, Config::default()).unwrap()
    }

    #[tokio::test]
    async fn twenty_thousand_rows_round_trip_in_batches() {
        let full = batch(20_000);
        let mut w = writer(schema());
        for start in (0..20_000usize).step_by(3_000) {
            w.write(&full.slice(start, (20_000 - start).min(3_000)))
                .unwrap();
        }
        let (bytes, summary) = w.finish().unwrap();
        assert_eq!(summary.rows, 20_000);
        let file = open(bytes).await;
        assert_eq!(file.row_count(), 20_000);
        for column in 0..full.num_columns() {
            let decoded = read_column(&file, column).await.unwrap();
            assert_same(decoded.as_ref(), full.column(column).as_ref());
        }
    }

    #[tokio::test]
    async fn empty_file_round_trips() {
        let (bytes, _) = writer(schema()).finish().unwrap();
        let file = open(bytes).await;
        assert_eq!(file.row_count(), 0);
        for column in 0..schema().fields().len() {
            let decoded = read_column(&file, column).await.unwrap();
            assert_eq!(decoded.len(), 0);
            assert_eq!(decoded.data_type(), schema().field(column).data_type());
        }
    }

    #[tokio::test]
    async fn out_of_order_row_inside_a_batch_is_reported() {
        let full = batch(10);
        let mut order: Vec<u32> = (0..10).collect();
        order.swap(8, 9);
        let indices = UInt32Array::from(order);
        let reordered = take_record_batch(&full, &indices).unwrap();
        let mut w = writer(schema());
        match w.write(&reordered) {
            Err(StrataError::Unsorted { row }) => assert_eq!(row, 9),
            other => panic!("expected Unsorted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn out_of_order_row_at_a_batch_boundary_is_reported() {
        let mut w = writer(schema());
        w.write(&batch(3_000)).unwrap();
        match w.write(&batch(3_000)) {
            Err(StrataError::Unsorted { row }) => assert_eq!(row, 3_000),
            other => panic!("expected Unsorted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn utf8_column_is_unsupported_until_m1_3() {
        let utf8_schema = Arc::new(Schema::new(vec![
            Field::new("trace_id", DataType::FixedSizeBinary(16), false),
            Field::new("name", DataType::Utf8, true),
        ]));
        let mut w =
            StrataWriter::new(Arc::clone(&utf8_schema), &["trace_id"], Config::default()).unwrap();
        let ids =
            FixedSizeBinaryArray::try_new(16, arrow::buffer::Buffer::from_vec(vec![0u8; 16]), None)
                .unwrap();
        let names = StringArray::from(vec![Some("a")]);
        let batch =
            RecordBatch::try_new(utf8_schema, vec![Arc::new(ids), Arc::new(names)]).unwrap();
        w.write(&batch).unwrap();
        assert!(matches!(w.finish(), Err(StrataError::Unsupported { .. })));
    }

    #[tokio::test]
    async fn constant_chunk_has_no_segment() {
        let constant_schema = Arc::new(Schema::new(vec![
            Field::new("trace_id", DataType::FixedSizeBinary(16), false),
            Field::new("flag", DataType::Boolean, false),
        ]));
        let ids = FixedSizeBinaryArray::try_new(
            16,
            arrow::buffer::Buffer::from_vec(vec![0u8; 16 * 10]),
            None,
        )
        .unwrap();
        let flags = arrow::array::BooleanArray::from(vec![true; 10]);
        let batch = RecordBatch::try_new(
            Arc::clone(&constant_schema),
            vec![Arc::new(ids), Arc::new(flags)],
        )
        .unwrap();
        let mut w = StrataWriter::new(constant_schema, &["trace_id"], Config::default()).unwrap();
        w.write(&batch).unwrap();
        let (bytes, _) = w.finish().unwrap();
        let file = open(bytes).await;
        let entries = file.block_entries(1).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].encoding, Encoding::Const);
        assert_eq!(entries[0].segment_len, 0);
        assert_eq!(entries[0].flags & flags::HAS_NULLS, 0);
        let decoded = read_column(&file, 1).await.unwrap();
        assert_same(decoded.as_ref(), batch.column(1).as_ref());
    }

    #[tokio::test]
    async fn hot_tail_and_directory_sizes_match_the_chunks() {
        let full = batch(20_000);
        let mut w = writer(schema());
        w.write(&full).unwrap();
        let (bytes, _) = w.finish().unwrap();
        let file = open(bytes).await;
        let directory = file.directory();
        let n_chunks = directory[0].n_chunks as usize;
        assert_eq!(file.hot.sort_key_entries.len(), n_chunks * 56);
        for record in directory {
            let chunks = record.n_chunks as usize;
            assert_eq!(record.n_pages as usize, chunks.div_ceil(64));
        }
        let page_refs: usize = directory.iter().map(|r| r.n_pages as usize).sum();
        assert_eq!(file.hot.pages.len(), page_refs * 16);
    }

    #[tokio::test]
    async fn block_entries_of_a_null_column_round_trip() {
        let null_schema = Arc::new(Schema::new(vec![
            Field::new("trace_id", DataType::FixedSizeBinary(16), false),
            Field::new("cost", DataType::Float64, true),
        ]));
        let ids = FixedSizeBinaryArray::try_new(
            16,
            arrow::buffer::Buffer::from_vec(vec![0u8; 16 * 4]),
            None,
        )
        .unwrap();
        let cost = new_null_array(&DataType::Float64, 4);
        let batch =
            RecordBatch::try_new(Arc::clone(&null_schema), vec![Arc::new(ids), cost]).unwrap();
        let mut w = StrataWriter::new(null_schema, &["trace_id"], Config::default()).unwrap();
        w.write(&batch).unwrap();
        let (bytes, _) = w.finish().unwrap();
        let file = open(bytes).await;
        let decoded = read_column(&file, 1).await.unwrap();
        assert_eq!(decoded.null_count(), 4);
    }
}
