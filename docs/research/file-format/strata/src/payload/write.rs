//! Writing one payload column: each value is cut into chunks, the chunks go through the frame
//! builder, each row's chunk ids go into refs pages, and the header ties the parts together.

use arrow::array::{Array, BinaryArray, StringArray};
use arrow::datatypes::DataType;

use super::REFS_PAGE_ROWS;
use super::chunk::chunk_value;
use super::frames::FrameBuilder;
use super::header::{PayloadHeader, RefPage};
use super::refs::{RowRefs, encode_page};
use crate::config::Config;
use crate::error::{Result, StrataError};
use crate::format::Out;
use crate::segment::{Codec, Kind, SegmentCodec, SegmentRef, encode_segment};

/// Statistics of one payload column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadStats {
    /// Number of unique chunks stored.
    pub unique_chunks: u32,
    /// Number of frames written.
    pub frames: u32,
    /// Total number of chunk references over all rows.
    pub chunk_refs: u64,
    /// Bytes written for the column: frames, refs pages and the header.
    pub bytes: u64,
}

/// Whether a column is stored with the payload layer: a Utf8 or Binary column with at least
/// one non-null value, whose average non-null length is at least `config.payload_min_avg`.
pub(crate) fn is_payload(array: &dyn Array, config: &Config) -> bool {
    match non_null_sizes(array) {
        Some((sum, count)) if count > 0 => {
            sum >= (config.payload_min_avg as u64).saturating_mul(count)
        }
        _ => false,
    }
}

/// The total length and the count of the non-null values of a Utf8 or Binary array.
fn non_null_sizes(array: &dyn Array) -> Option<(u64, u64)> {
    fn sizes<'a>(values: impl Iterator<Item = Option<&'a [u8]>>) -> (u64, u64) {
        values.flatten().fold((0, 0), |(sum, count), value| {
            (sum + value.len() as u64, count + 1)
        })
    }
    match array.data_type() {
        DataType::Utf8 => array
            .as_any()
            .downcast_ref::<StringArray>()
            .map(|strings| sizes(strings.iter().map(|v| v.map(str::as_bytes)))),
        DataType::Binary => array
            .as_any()
            .downcast_ref::<BinaryArray>()
            .map(|binaries| sizes(binaries.iter())),
        _ => None,
    }
}

/// The bytes of every value of a Utf8 or Binary array, `None` for a null.
fn value_bytes(array: &dyn Array) -> Result<Vec<Option<&[u8]>>> {
    let schema_error = || StrataError::Schema {
        reason: format!("payload column of type {}", array.data_type()),
    };
    match array.data_type() {
        DataType::Utf8 => {
            let strings = array
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(schema_error)?;
            Ok(strings.iter().map(|v| v.map(str::as_bytes)).collect())
        }
        DataType::Binary => {
            let binaries = array
                .as_any()
                .downcast_ref::<BinaryArray>()
                .ok_or_else(schema_error)?;
            Ok(binaries.iter().collect())
        }
        _ => Err(schema_error()),
    }
}

/// Writes one payload column. Frames, then refs pages, then the header are written, so the
/// frames of a column are contiguous in the file. Returns the header's reference and the
/// statistics.
pub(crate) fn write_payload_column(
    array: &dyn Array,
    _config: &Config,
    codec: &mut SegmentCodec,
    frame_codec: &mut SegmentCodec,
    out: &mut Out,
) -> Result<(SegmentRef, PayloadStats)> {
    let mut frames = FrameBuilder::new();
    let mut rows: Vec<Option<Vec<u32>>> = Vec::with_capacity(array.len());
    let mut chunk_refs = 0u64;
    for value in value_bytes(array)? {
        let Some(bytes) = value else {
            rows.push(None);
            continue;
        };
        let mut ids = Vec::new();
        for range in chunk_value(bytes) {
            let chunk = bytes.get(range).unwrap_or_default();
            ids.push(frames.add(chunk, frame_codec, out)?);
        }
        chunk_refs += ids.len() as u64;
        rows.push(Some(ids));
    }
    frames.finish(frame_codec, out)?;

    let mut ref_pages = Vec::with_capacity(rows.len().div_ceil(REFS_PAGE_ROWS));
    let mut ref_bytes = 0u64;
    for (group_index, group) in rows.chunks(REFS_PAGE_ROWS).enumerate() {
        let refs: Vec<RowRefs<'_>> = group
            .iter()
            .map(|row| match row {
                Some(ids) => RowRefs::Ids(ids.as_slice()),
                None => RowRefs::Null,
            })
            .collect();
        let mut body = Vec::new();
        encode_page(&refs, &mut body)?;
        let seg = encode_segment(Kind::PayloadRefs, &body, Codec::Zstd, codec, out)?;
        ref_bytes += u64::from(seg.len);
        let first_row =
            u32::try_from(group_index * REFS_PAGE_ROWS).map_err(|_| StrataError::TooManyRows {
                rows: (group_index * REFS_PAGE_ROWS) as u64,
            })?;
        ref_pages.push(RefPage {
            offset: seg.offset,
            len: seg.len,
            first_row,
        });
    }

    let n_rows = u32::try_from(rows.len()).map_err(|_| StrataError::TooManyRows {
        rows: rows.len() as u64,
    })?;
    let unique_chunks = frames.n_chunks();
    let frame_entries = std::mem::take(&mut frames.frames);
    let frame_count = u32::try_from(frame_entries.len()).map_err(|_| StrataError::Unsupported {
        what: "more than 4 billion frames".to_string(),
    })?;
    let header = PayloadHeader {
        n_rows,
        n_chunks: unique_chunks,
        filters_offset: 0,
        filters_len: 0,
        ref_pages,
        edge_groups: Vec::new(),
        frames: frame_entries,
    };
    let mut body = Out(Vec::new());
    header.encode(&mut body)?;
    let header_seg = encode_segment(Kind::PayloadHeader, &body.0, Codec::Zstd, codec, out)?;

    let stats = PayloadStats {
        unique_chunks,
        frames: frame_count,
        chunk_refs,
        bytes: frames.stored_bytes + ref_bytes + u64::from(header_seg.len),
    };
    Ok((header_seg, stats))
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
    use crate::file::StrataFile;
    use crate::hot::Layer;
    use crate::reader::read_column;
    use crate::testgen::assert_same;
    use crate::writer::{FileSummary, StrataWriter};
    use arrow::array::{FixedSizeBinaryArray, RecordBatch};
    use arrow::buffer::Buffer;
    use arrow::datatypes::{Field, Schema, SchemaRef};
    use object_store::memory::InMemory;
    use object_store::path::Path;
    use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
    use std::sync::Arc;

    /// `[trace_id FixedSizeBinary(16) non-null, v Utf8 nullable]`.
    fn schema() -> SchemaRef {
        Arc::new(Schema::new(vec![
            Field::new("trace_id", DataType::FixedSizeBinary(16), false),
            Field::new("v", DataType::Utf8, true),
        ]))
    }

    /// One batch of the schema with all-zero trace ids and the given values.
    fn table(values: &[Option<String>]) -> RecordBatch {
        let ids =
            FixedSizeBinaryArray::try_new(16, Buffer::from_vec(vec![0u8; 16 * values.len()]), None)
                .unwrap();
        let strings: Vec<Option<&str>> = values.iter().map(|v| v.as_deref()).collect();
        let v = arrow::array::StringArray::from(strings);
        RecordBatch::try_new(schema(), vec![Arc::new(ids), Arc::new(v)]).unwrap()
    }

    /// Writes `batch` with `config`, then opens the bytes from an in-memory store.
    async fn write_and_open(batch: &RecordBatch, config: Config) -> (StrataFile, FileSummary) {
        let mut w = StrataWriter::new(batch.schema(), &["trace_id"], config).unwrap();
        w.write(batch).unwrap();
        let (bytes, summary) = w.finish().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("table.strata");
        store.put(&path, PutPayload::from(bytes)).await.unwrap();
        let file = StrataFile::open(store, path, None, &Config::default())
            .await
            .unwrap();
        (file, summary)
    }

    #[tokio::test]
    async fn mixed_column_round_trips_as_payload() {
        let values = vec![
            None,
            Some(String::new()),
            Some("b".to_string()),
            Some("x".repeat(255)),
            Some("y".repeat(256)),
            Some("z".repeat(1 << 20)),
            Some("[1,2".to_string()),
            Some(format!("[\"{}\"]", "q".repeat(100 * 1024))),
            Some("a]b\"c".to_string()),
        ];
        let batch = table(&values);
        let (file, _) = write_and_open(&batch, Config::default()).await;
        assert_eq!(file.directory()[1].layer, Layer::Payload);
        let decoded = read_column(&file, 1).await.unwrap();
        assert_same(decoded.as_ref(), batch.column(1).as_ref());
    }

    #[tokio::test]
    async fn identical_values_are_deduplicated() {
        let value = "abcdefghij".repeat(1024);
        let values = vec![Some(value); 500];
        let batch = table(&values);
        let (file, summary) = write_and_open(&batch, Config::default()).await;
        let stats = summary.columns[1].payload.as_ref().unwrap();
        assert!(
            stats.unique_chunks <= 8,
            "{} unique chunks",
            stats.unique_chunks
        );
        assert!(stats.chunk_refs >= 500, "{} chunk refs", stats.chunk_refs);
        let decoded = read_column(&file, 1).await.unwrap();
        assert_same(decoded.as_ref(), batch.column(1).as_ref());
    }

    #[tokio::test]
    async fn empty_values_have_no_frames() {
        let values = vec![Some(String::new()); 5];
        let batch = table(&values);
        let config = Config {
            payload_min_avg: 0,
            ..Config::default()
        };
        let (file, summary) = write_and_open(&batch, config).await;
        assert_eq!(file.directory()[1].layer, Layer::Payload);
        let stats = summary.columns[1].payload.as_ref().unwrap();
        assert_eq!(stats.frames, 0);
        assert_eq!(stats.unique_chunks, 0);
        let decoded = read_column(&file, 1).await.unwrap();
        assert_same(decoded.as_ref(), batch.column(1).as_ref());
    }

    #[test]
    fn short_columns_are_narrow() {
        let short = table(&[Some("abc".to_string()), None]);
        assert!(!is_payload(short.column(1).as_ref(), &Config::default()));
        let all_null = table(&[None, None]);
        assert!(!is_payload(all_null.column(1).as_ref(), &Config::default()));
    }
}
