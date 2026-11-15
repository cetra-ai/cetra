//! Writing one payload column: each value is cut into chunks, the chunks go through the frame
//! builder, each row's chunk ids go into refs pages, and the header ties the parts together.

use std::collections::HashSet;

use arrow::array::{Array, BinaryArray, StringArray};
use arrow::datatypes::DataType;

use super::chunk::chunk_value;
use super::frames::{FrameBuilder, frame_of};
use super::header::{EdgeGroup, PayloadHeader, RefPage};
use super::refs::{RowRefs, encode_page};
use super::search::{Filters, FrameFilter, encode_edge_group};
use super::trigram::{add_trigrams, filter_bit, filter_bits, set_bit};
use super::{
    EDGE_GROUP_CHUNKS, FILTER_MAX_SHARE, MAX_PAGE_IDS, MAX_READ_IDS, REFS_PAGE_ROWS, ROLLUP_BITS,
    ROLLUP_MAX_FILL,
};
use crate::config::Config;
use crate::error::{Result, StrataError};
use crate::format::{Out, SEGMENT_HEADER_LEN};
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
    /// Bytes written for the column: frames, refs pages, edge groups, filters and the header.
    pub bytes: u64,
    /// Bytes of the filters segment.
    pub filter_bytes: u64,
    /// Bytes of the edge group segments.
    pub edge_bytes: u64,
    /// Frames stored without a filter, because a filter would be too large for them.
    pub frames_without_filter: u32,
    /// Whether the file-level rollup was kept.
    pub rollup: bool,
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

/// Writes one payload column. The segments are written in this order: frames, refs pages,
/// edge groups, filters, header. So all of a column's segments are contiguous in the file.
/// Returns the header's reference and the statistics.
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
    if chunk_refs > MAX_READ_IDS as u64 {
        return Err(StrataError::Unsupported {
            what: "more than 2^28 chunk references in one column".to_string(),
        });
    }
    frames.finish(frame_codec, out)?;

    let mut ref_pages = Vec::with_capacity(rows.len().div_ceil(REFS_PAGE_ROWS));
    let mut ref_bytes = 0u64;
    for (group_index, group) in rows.chunks(REFS_PAGE_ROWS).enumerate() {
        let page_ids = group.iter().fold(0usize, |total, row| {
            total.saturating_add(row.as_ref().map_or(0, Vec::len))
        });
        if page_ids > MAX_PAGE_IDS {
            return Err(StrataError::Unsupported {
                what: "more than 2^24 chunk ids in one refs page".to_string(),
            });
        }
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

    // Boundary trigrams. A needle that crosses from chunk `l` into chunk `r` lies in the window
    // made of the tail of `l` and the head of `r`, so the window's trigrams go to the filter of
    // the frame that holds `r`.
    let mut pairs: HashSet<(u32, u32)> = HashSet::new();
    for ids in rows.iter().flatten() {
        for pair in ids.windows(2) {
            if let &[l, r] = pair {
                pairs.insert((l, r));
            }
        }
    }
    let pair_corrupt = || StrataError::Corrupt {
        what: "payload pair",
        offset: 0,
    };
    for &(l, r) in &pairs {
        let f = frame_of(&frame_entries, r).ok_or_else(pair_corrupt)?;
        let tail = frames.tails.get(l as usize).ok_or_else(pair_corrupt)?;
        let head = frames.heads.get(r as usize).ok_or_else(pair_corrupt)?;
        let mut window = Vec::with_capacity(tail.len() + head.len());
        window.extend_from_slice(tail);
        window.extend_from_slice(head);
        let trigrams = frames.frame_trigrams.get_mut(f).ok_or_else(pair_corrupt)?;
        add_trigrams(&window, trigrams);
    }

    // One filter per frame, unless the filter would be more than FILTER_MAX_SHARE of the frame.
    let mut frame_filters: Vec<Option<FrameFilter>> = Vec::with_capacity(frame_entries.len());
    let mut frames_without_filter = 0u32;
    for (entry, trigrams) in frame_entries.iter().zip(&frames.frame_trigrams) {
        let bits = filter_bits(trigrams.len());
        let stored = u64::from(entry.len).saturating_sub(SEGMENT_HEADER_LEN as u64);
        if (u64::from(bits) / 8) as f64 > FILTER_MAX_SHARE * stored as f64 {
            frame_filters.push(None);
            frames_without_filter += 1;
        } else {
            let mut bitmap = vec![0u8; (bits / 8) as usize];
            for &h in trigrams {
                set_bit(&mut bitmap, filter_bit(h, bits));
            }
            frame_filters.push(Some(FrameFilter { bits, bitmap }));
        }
    }

    // The rollup: the trigrams of every frame, one bit each. Dropped when too many bits are set.
    let mut rollup_bits = vec![0u8; (ROLLUP_BITS / 8) as usize];
    for trigrams in &frames.frame_trigrams {
        for &h in trigrams {
            set_bit(&mut rollup_bits, h & (ROLLUP_BITS - 1));
        }
    }
    let set_bits: u32 = rollup_bits.iter().map(|b| b.count_ones()).sum();
    let rollup =
        (f64::from(set_bits) / f64::from(ROLLUP_BITS) <= ROLLUP_MAX_FILL).then_some(rollup_bits);
    let rollup_kept = rollup.is_some();

    // The edge groups: the tails of the unique chunks, EDGE_GROUP_CHUNKS per segment.
    let mut edge_groups = Vec::with_capacity(frames.tails.len().div_ceil(EDGE_GROUP_CHUNKS));
    let mut edge_bytes = 0u64;
    for (g, group_tails) in frames.tails.chunks(EDGE_GROUP_CHUNKS).enumerate() {
        let mut body = Vec::new();
        encode_edge_group(group_tails, &mut body);
        let seg = encode_segment(Kind::PayloadEdges, &body, Codec::Zstd, codec, out)?;
        edge_bytes += u64::from(seg.len);
        let first_chunk =
            u32::try_from(g * EDGE_GROUP_CHUNKS).map_err(|_| StrataError::Unsupported {
                what: "more than 4 billion chunks".to_string(),
            })?;
        edge_groups.push(EdgeGroup {
            offset: seg.offset,
            len: seg.len,
            first_chunk,
        });
    }

    let filters = Filters {
        rollup,
        frames: frame_filters,
    };
    let mut filter_body = Vec::new();
    filters.encode(&mut filter_body)?;
    let filter_seg = encode_segment(Kind::PayloadFilters, &filter_body, Codec::Zstd, codec, out)?;

    let header = PayloadHeader {
        n_rows,
        n_chunks: unique_chunks,
        filters_offset: filter_seg.offset,
        filters_len: filter_seg.len,
        ref_pages,
        edge_groups,
        frames: frame_entries,
    };
    let mut body = Out(Vec::new());
    header.encode(&mut body)?;
    let header_seg = encode_segment(Kind::PayloadHeader, &body.0, Codec::Zstd, codec, out)?;

    let stats = PayloadStats {
        unique_chunks,
        frames: frame_count,
        chunk_refs,
        bytes: frames.stored_bytes
            + ref_bytes
            + edge_bytes
            + u64::from(filter_seg.len)
            + u64::from(header_seg.len),
        filter_bytes: u64::from(filter_seg.len),
        edge_bytes,
        frames_without_filter,
        rollup: rollup_kept,
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
