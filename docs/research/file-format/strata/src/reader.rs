//! Reading one column back as an Arrow array.

use arrow::array::{Array, ArrayRef, new_empty_array};
use arrow::compute::concat;
use arrow::datatypes::Field;
use futures::future::try_join_all;

use crate::error::{Result, StrataError};
use crate::file::StrataFile;
use crate::hot::Layer;
use crate::io::ScanKind;
use crate::narrow::{Dictionary, decode_chunk};
use crate::payload::read_payload_column;
use crate::segment::{Kind, SegmentRef};
use crate::shred::unshred;

/// Decodes every row of column `column` (its schema index).
///
/// Chunk segments are fetched concurrently, decoded in row order and concatenated. A file
/// with no rows gives an empty array of the column's type. A shredded map is rebuilt from its
/// children and residual, with each row's entries sorted bytewise by key.
pub async fn read_column(file: &StrataFile, column: usize) -> Result<ArrayRef> {
    let schema = file.schema();
    let field = schema
        .fields()
        .get(column)
        .ok_or_else(|| StrataError::Schema {
            reason: format!("no column {column}"),
        })?;
    let record = file
        .directory()
        .get(column)
        .ok_or_else(|| StrataError::Schema {
            reason: format!("no column {column}"),
        })?;
    match record.layer {
        Layer::Payload => read_payload_column(file, record, field).await,
        Layer::Narrow => read_narrow(file, column, field).await,
        Layer::MapParent => read_shredded(file, column, field).await,
        Layer::MapChild | Layer::Residual => Err(StrataError::Unsupported {
            what: format!("layer of column {column}"),
        }),
    }
}

/// Reads a map child or residual, by its directory index, as the field [`StrataFile::field_of`]
/// gives it.
async fn read_part(file: &StrataFile, id: u32) -> Result<ArrayRef> {
    let corrupt = || StrataError::Corrupt {
        what: "map layout",
        offset: 0,
    };
    let column = usize::try_from(id).map_err(|_| corrupt())?;
    let field = file.field_of(column)?;
    read_narrow(file, column, &field).await
}

/// Rebuilds a shredded map: its children and residual are read concurrently, then merged by
/// [`unshred`]. The layout comes from hot-tail section 5, which open has checked.
async fn read_shredded(file: &StrataFile, column: usize, field: &Field) -> Result<ArrayRef> {
    let layout = file
        .hot
        .maps
        .iter()
        .find(|layout| usize::try_from(layout.map).ok() == Some(column))
        .ok_or_else(|| StrataError::Corrupt {
            what: "map layout",
            offset: 0,
        })?;
    let keys: Vec<Vec<u8>> = layout.keys.iter().map(|key| key.key.clone()).collect();
    let children = try_join_all(layout.keys.iter().map(|key| read_part(file, key.child)));
    let (children, residual) = futures::try_join!(children, read_part(file, layout.residual))?;
    unshred(field.data_type(), &keys, &children, &residual)
}

/// Reads the narrow column at directory index `column`, which may be a schema column, a map
/// child or a residual. `field` gives its type.
async fn read_narrow(file: &StrataFile, column: usize, field: &Field) -> Result<ArrayRef> {
    let record = file
        .directory()
        .get(column)
        .ok_or_else(|| StrataError::Schema {
            reason: format!("no column {column}"),
        })?;
    let entries = file.block_entries(column).await?;
    if entries.is_empty() {
        return Ok(new_empty_array(field.data_type()));
    }

    // Bit 0 of the flags says the column has a dictionary, and the offset must agree.
    let dict = match (record.flags & 1 != 0, record.dict_offset != 0) {
        (true, true) => {
            let segment = SegmentRef {
                offset: record.dict_offset,
                len: record.dict_len,
            };
            let bytes = file.read_segment(&segment, Kind::Dictionary).await?;
            Some(Dictionary::decode(&bytes, record.dict_offset)?)
        }
        (false, false) => None,
        _ => {
            return Err(StrataError::Corrupt {
                what: "dictionary flag",
                offset: record.dict_offset,
            });
        }
    };

    // The chunks are one scan wave. Constant chunks have no segment and are decoded without one.
    let segments: Vec<(SegmentRef, Kind)> = entries
        .iter()
        .filter(|entry| entry.segment_len > 0)
        .map(|entry| {
            let segment = SegmentRef {
                offset: entry.segment_offset,
                len: entry.segment_len,
            };
            (segment, Kind::NarrowChunk)
        })
        .collect();
    let mut fetched = file.fetch(&segments, ScanKind::Scan).await?.into_iter();
    let mut bodies: Vec<Option<Vec<u8>>> = Vec::with_capacity(entries.len());
    for entry in entries.iter() {
        bodies.push(if entry.segment_len == 0 {
            None
        } else {
            Some(fetched.next().ok_or(StrataError::Corrupt {
                what: "segment range",
                offset: entry.segment_offset,
            })?)
        });
    }

    let mut arrays = Vec::with_capacity(entries.len());
    for (entry, body) in entries.iter().zip(&bodies) {
        arrays.push(decode_chunk(entry, body.as_deref(), field, dict.as_ref())?);
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
    async fn utf8_column_of_65536_distinct_values_round_trips_as_varbin() {
        let utf8_schema = Arc::new(Schema::new(vec![
            Field::new("trace_id", DataType::FixedSizeBinary(16), false),
            Field::new("name", DataType::Utf8, true),
        ]));
        let mut w =
            StrataWriter::new(Arc::clone(&utf8_schema), &["trace_id"], Config::default()).unwrap();
        let ids = FixedSizeBinaryArray::try_new(
            16,
            arrow::buffer::Buffer::from_vec(vec![0u8; 16 * 65_536]),
            None,
        )
        .unwrap();
        let names = StringArray::from((0..65_536).map(|i| format!("v{i}")).collect::<Vec<_>>());
        let batch = RecordBatch::try_new(
            Arc::clone(&utf8_schema),
            vec![Arc::new(ids), Arc::new(names)],
        )
        .unwrap();
        w.write(&batch).unwrap();
        let (bytes, summary) = w.finish().unwrap();
        let name = &summary.columns[1];
        assert_eq!(name.encoding_counts[Encoding::Dict as usize], 0);
        assert_eq!(name.encoding_counts[Encoding::Varbin as usize], name.chunks);
        let file = open(bytes).await;
        let decoded = read_column(&file, 1).await.unwrap();
        assert_same(decoded.as_ref(), batch.column(1).as_ref());
    }

    #[tokio::test]
    async fn string_columns_use_dict_and_list_dict() {
        let full = batch(20_000);
        let mut w = writer(schema());
        w.write(&full).unwrap();
        let (bytes, summary) = w.finish().unwrap();
        // Columns 5 to 9 are span_kind, status, model, environment and tags.
        for column in &summary.columns[5..] {
            let counts = column.encoding_counts;
            for (encoding, count) in counts.iter().enumerate() {
                let allowed = matches!(encoding, 0 | 6 | 8);
                assert!(
                    allowed || *count == 0,
                    "{} has {count} chunks of encoding {encoding}",
                    column.name
                );
            }
        }
        let file = open(bytes).await;
        for column in 0..full.num_columns() {
            let decoded = read_column(&file, column).await.unwrap();
            assert_same(decoded.as_ref(), full.column(column).as_ref());
        }
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

    /// The entries of one row: keys and optional values, or `None` for a null map.
    type MapRows = Vec<Option<Vec<(String, Option<String>)>>>;

    /// Each row of a `Map<Utf8, Utf8>` array as its entries in stored order.
    fn map_rows(array: &dyn Array) -> MapRows {
        use arrow::array::AsArray;
        let map = array.as_map();
        let keys = map.keys().as_string::<i32>();
        let values = map.values().as_string::<i32>();
        let offsets = map.value_offsets();
        (0..map.len())
            .map(|row| {
                if map.is_null(row) {
                    return None;
                }
                let (lo, hi) = (offsets[row] as usize, offsets[row + 1] as usize);
                Some(
                    (lo..hi)
                        .map(|i| {
                            let value = values.is_valid(i).then(|| values.value(i).to_string());
                            (keys.value(i).to_string(), value)
                        })
                        .collect(),
                )
            })
            .collect()
    }

    /// Shredding round trip. Forty rows: `b` is in the even rows once with a value, so it is
    /// promoted. `a` is in every non-empty row once, with a null value in some rows and twice
    /// in others, so it is promoted and its residual rows keep their entries. `d` occurs once,
    /// so it is not promoted. Some maps are null and some are empty, and each row's entries are
    /// written unsorted. Every row reads back with its entries stably sorted by key.
    #[tokio::test]
    async fn shredded_map_round_trips_with_residual_entries() {
        use arrow::array::{MapBuilder, StringBuilder};
        let entries = DataType::Struct(
            vec![
                Field::new("keys", DataType::Utf8, false),
                Field::new("values", DataType::Utf8, true),
            ]
            .into(),
        );
        let map_type = DataType::Map(Arc::new(Field::new("entries", entries, false)), false);
        let schema = Arc::new(Schema::new(vec![
            Field::new("trace_id", DataType::FixedSizeBinary(16), false),
            Field::new("attrs", map_type, true),
        ]));

        let mut builder = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new());
        let mut expected: MapRows = Vec::new();
        for i in 0..40usize {
            if i % 7 == 0 {
                builder.append(false).unwrap();
                expected.push(None);
                continue;
            }
            if i % 5 == 0 {
                builder.append(true).unwrap();
                expected.push(Some(Vec::new()));
                continue;
            }
            let mut entries: Vec<(&str, Option<String>)> = Vec::new();
            if i % 2 == 0 {
                entries.push(("b", Some(format!("b{i}"))));
            }
            entries.push(("a", (i % 3 != 0).then(|| format!("a{i}"))));
            if i % 4 == 1 {
                entries.push(("a", Some("again".to_string())));
            }
            if i == 9 {
                entries.push(("d", Some("once".to_string())));
            }
            for (key, value) in &entries {
                builder.keys().append_value(key);
                match value {
                    Some(value) => builder.values().append_value(value),
                    None => builder.values().append_null(),
                }
            }
            builder.append(true).unwrap();
            let mut sorted: Vec<(String, Option<String>)> = entries
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone()))
                .collect();
            sorted.sort_by(|x, y| x.0.cmp(&y.0));
            expected.push(Some(sorted));
        }

        let ids: Vec<u8> = (0..40u128).flat_map(|i| i.to_be_bytes()).collect();
        let ids =
            FixedSizeBinaryArray::try_new(16, arrow::buffer::Buffer::from_vec(ids), None).unwrap();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![Arc::new(ids), Arc::new(builder.finish())],
        )
        .unwrap();
        let mut w = StrataWriter::new(schema, &["trace_id"], Config::default()).unwrap();
        w.write(&batch).unwrap();
        let (bytes, _) = w.finish().unwrap();

        let file = open(bytes).await;
        let children = file
            .directory()
            .iter()
            .filter(|r| r.layer == Layer::MapChild)
            .count();
        assert_eq!(children, 2, "keys a and b are promoted, d is not");
        let decoded = read_column(&file, 1).await.unwrap();
        assert_eq!(map_rows(decoded.as_ref()), expected);
    }
}
