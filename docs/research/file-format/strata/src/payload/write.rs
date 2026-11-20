//! Writing payload columns: each value is cut into chunks, the chunks go into one store
//! ([`super::store`]), each row's chunk ids go into refs pages, and a header per column ties
//! its pages to the store.

use std::collections::HashMap;
use std::rc::Rc;

use arrow::array::{Array, ArrayRef, BinaryArray, StringArray};
use arrow::datatypes::DataType;
use xxhash_rust::xxh3::xxh3_128;

use super::chunk::{Chunk, chunk_value_from};
use super::header::{RefPage, encode_header};
use super::refs::{RowRefs, encode_page};
use super::store::{KeyBuild, StoreBuilder};
use super::{MAX_PAGE_IDS, MAX_READ_IDS, REFS_PAGE_ROWS};
use crate::config::Config;
use crate::error::{Result, StrataError};
use crate::format::Out;
use crate::segment::{Codec, Kind, SegmentCodec, SegmentRef, encode_segment};

/// Statistics of one payload column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadStats {
    /// Number of unique chunks in the column's store.
    pub unique_chunks: u32,
    /// Number of frames in the column's store.
    pub frames: u32,
    /// Total number of chunk references over all rows.
    pub chunk_refs: u64,
    /// Bytes written for the column: its refs pages and header, and for the first column of a
    /// store, the store's frames, index pages and store segment.
    pub bytes: u64,
    /// Bytes of the frames (first column of a store only).
    pub frame_bytes: u64,
    /// Bytes of the refs pages.
    pub ref_bytes: u64,
    /// Bytes of the gram index pages (first column of a store only).
    pub index_bytes: u64,
    /// Bytes of the store segment, dictionary included (first column of a store only).
    pub store_bytes: u64,
    /// Bytes of the store's zstd dictionary (first column of a store only).
    pub dict_bytes: u64,
}

/// Whether a column, given in parts, is stored with the payload layer: a Utf8 or Binary column
/// with at least one non-null value, whose average non-null length is at least `config.payload_min_avg`.
pub(crate) fn is_payload(parts: &[ArrayRef], config: &Config) -> bool {
    let mut total = Some((0u64, 0u64));
    for part in parts {
        total = match (total, non_null_sizes(part.as_ref())) {
            (Some((s, c)), Some((ps, pc))) => Some((s + ps, c + pc)),
            _ => None,
        };
    }
    match total {
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

/// The length of the common prefix of `a` and `b`: compared in 4 KiB blocks, then 64-byte
/// blocks, then bytes.
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    for block in [4096usize, 64, 1] {
        while i + block <= n && a.get(i..i + block) == b.get(i..i + block) {
            i += block;
        }
    }
    i
}

/// A value's chunks and their ids.
struct Cut {
    chunks: Vec<Chunk>,
    ids: Vec<u32>,
}

/// The last value of a column, with its cut.
struct Previous<'a> {
    value: &'a [u8],
    cut: Rc<Cut>,
}

/// Values of at least this many bytes are remembered by hash, so a value seen before anywhere
/// in the store (a system prompt or tool list shared by many traces) is not cut again.
const SEEN_MIN: usize = 256;

/// Cuts `value` into chunks. When it equals the previous value of its column, its chunks are
/// reused whole. When it starts with the previous value up to a chunk that ends at a top-level
/// JSON cut (a conversation that grew), those chunks are reused and only the rest is cut.
/// Returns the chunks and how many were reused.
fn chunk_with_reuse(value: &[u8], previous: Option<&Previous<'_>>) -> (Vec<Chunk>, usize) {
    if let Some(p) = previous {
        let lcp = common_prefix(p.value, value);
        // The same value again (a system prompt on every call of a trace): all its chunks.
        if lcp == value.len() && lcp == p.value.len() {
            return (p.cut.chunks.clone(), p.cut.chunks.len());
        }
        if let Some(k) = p
            .cut
            .chunks
            .iter()
            .rposition(|c| c.at_cut && c.range.end <= lcp)
            && let Some(from) = p.cut.chunks.get(k).map(|c| c.range.end)
            && let Some(rest) = chunk_value_from(value, from)
        {
            let mut chunks = p.cut.chunks.get(..=k).unwrap_or_default().to_vec();
            chunks.extend(rest);
            return (chunks, k + 1);
        }
    }
    (chunk_value_from(value, 0).unwrap_or_default(), 0)
}

/// Writes the payload columns of a file with one store shared by all of them (S4).
///
/// Rows are walked in order and, within a row, the columns in the given order, so a value and
/// a near copy of it in a later column of the same trace land in the same frames, where zstd
/// matches them; an identical chunk is stored once whatever its column. Each column then gets
/// its own refs pages and header.
///
/// Segments are written in this order: the frames, the index pages and the store segment, then
/// per column its refs pages and its header. Returns the store segment and, per column, the
/// header segment and the statistics. The store's bytes count in the first column. The frames
/// use zstd `level`.
pub(crate) fn write_payload_columns(
    columns: &[&[ArrayRef]],
    level: i32,
    codec: &mut SegmentCodec,
    out: &mut Out,
) -> Result<(SegmentRef, Vec<(SegmentRef, PayloadStats)>)> {
    let mut values: Vec<Vec<Option<&[u8]>>> = Vec::with_capacity(columns.len());
    for parts in columns {
        let mut column = Vec::new();
        for part in *parts {
            column.extend(value_bytes(part.as_ref())?);
        }
        values.push(column);
    }
    let n_rows = values.first().map_or(0, Vec::len);
    if values.iter().any(|column| column.len() != n_rows) {
        return Err(StrataError::Schema {
            reason: "payload columns of different lengths".to_string(),
        });
    }
    let value_bytes: usize = values
        .iter()
        .flatten()
        .map(|v| v.map_or(0, <[u8]>::len))
        .sum();
    let mut store = StoreBuilder::with_capacity(value_bytes);
    let mut seen: HashMap<u128, Rc<Cut>, KeyBuild> = HashMap::default();
    let mut rows: Vec<Vec<Option<Vec<u32>>>> =
        values.iter().map(|_| Vec::with_capacity(n_rows)).collect();
    let mut chunk_refs = vec![0u64; values.len()];
    let mut previous: Vec<Option<Previous<'_>>> = values.iter().map(|_| None).collect();
    for row in 0..n_rows {
        for (c, column) in values.iter().enumerate() {
            let value = column.get(row).copied().flatten();
            let (Some(out_rows), Some(refs), Some(prev)) =
                (rows.get_mut(c), chunk_refs.get_mut(c), previous.get_mut(c))
            else {
                continue;
            };
            let Some(bytes) = value else {
                out_rows.push(None);
                continue;
            };
            let same = prev
                .as_ref()
                .filter(|p| p.value == bytes)
                .map(|p| Rc::clone(&p.cut));
            let key = (same.is_none() && bytes.len() >= SEEN_MIN).then(|| xxh3_128(bytes));
            let cut = match same.or_else(|| key.and_then(|k| seen.get(&k)).map(Rc::clone)) {
                // The previous value again, or a value seen before: its pairs were recorded then.
                Some(cut) => cut,
                None => {
                    let (chunks, reused) = chunk_with_reuse(bytes, prev.as_ref());
                    let mut ids = Vec::with_capacity(chunks.len());
                    if let Some(p) = prev.as_ref() {
                        ids.extend_from_slice(p.cut.ids.get(..reused).unwrap_or_default());
                    }
                    for chunk in chunks.get(reused..).unwrap_or_default() {
                        ids.push(store.add(bytes.get(chunk.range.clone()).unwrap_or_default())?);
                    }
                    // The pairs inside the reused prefix were recorded with the previous value.
                    for pair in ids
                        .get(reused.saturating_sub(1)..)
                        .unwrap_or_default()
                        .windows(2)
                    {
                        if let &[left, right] = pair {
                            store.pair(left, right);
                        }
                    }
                    let cut = Rc::new(Cut { chunks, ids });
                    if let Some(k) = key {
                        seen.insert(k, Rc::clone(&cut));
                    }
                    cut
                }
            };
            *refs += cut.ids.len() as u64;
            let ids = cut.ids.clone();
            *prev = Some(Previous { value: bytes, cut });
            out_rows.push(Some(ids));
        }
    }
    for refs in &chunk_refs {
        check_chunk_refs(*refs)?;
    }
    let (store_seg, store_stats) = store.write(level, codec, out)?;

    let n_rows_u32 = u32::try_from(n_rows).map_err(|_| StrataError::TooManyRows {
        rows: n_rows as u64,
    })?;
    let mut results = Vec::with_capacity(rows.len());
    for (c, column_rows) in rows.iter().enumerate() {
        let (ref_pages, ref_bytes) = write_refs(column_rows, codec, out)?;
        let mut body = Out(Vec::new());
        encode_header(n_rows_u32, &ref_pages, &mut body)?;
        let header_seg = encode_segment(Kind::PayloadHeader, &body.0, Codec::Zstd, codec, out)?;
        // The store counts once, in the first column.
        let shared = |v: u64| if c == 0 { v } else { 0 };
        let stats = PayloadStats {
            unique_chunks: store_stats.unique_chunks,
            frames: store_stats.frames,
            chunk_refs: chunk_refs.get(c).copied().unwrap_or(0),
            bytes: shared(
                store_stats.frame_bytes + store_stats.index_bytes + store_stats.store_bytes,
            ) + ref_bytes
                + u64::from(header_seg.len),
            frame_bytes: shared(store_stats.frame_bytes),
            ref_bytes,
            index_bytes: shared(store_stats.index_bytes),
            store_bytes: shared(store_stats.store_bytes),
            dict_bytes: shared(store_stats.dict_bytes),
        };
        results.push((header_seg, stats));
    }
    Ok((store_seg, results))
}

/// Refuses a column with more chunk references than a read can return.
pub(crate) fn check_chunk_refs(chunk_refs: u64) -> Result<()> {
    if chunk_refs > MAX_READ_IDS as u64 {
        return Err(StrataError::Unsupported {
            what: "more than 2^28 chunk references in one column".to_string(),
        });
    }
    Ok(())
}

/// Writes the refs pages of one column, `REFS_PAGE_ROWS` rows each. Returns the page table
/// and the bytes written.
pub(crate) fn write_refs(
    rows: &[Option<Vec<u32>>],
    codec: &mut SegmentCodec,
    out: &mut Out,
) -> Result<(Vec<RefPage>, u64)> {
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
    Ok((ref_pages, ref_bytes))
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

    /// Non-repeating text of `words` words from `seed`.
    fn prose(words: usize, seed: u64) -> String {
        let mut x = seed.max(1);
        (0..words)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                format!("w{} ", x % 100_000)
            })
            .collect()
    }

    #[tokio::test]
    async fn payload_columns_share_one_chunk_store() {
        // `input` of row k repeats `output` of row k - 1 inside a larger JSON array.
        let n = 60usize;
        let answers: Vec<String> = (0..n).map(|k| prose(3_000, k as u64 + 1)).collect();
        let outputs: Vec<Option<String>> = answers
            .iter()
            .map(|a| Some(serde_json::json!([{"role": "assistant", "content": a}]).to_string()))
            .collect();
        let inputs: Vec<Option<String>> = (0..n)
            .map(|k| {
                let mut messages =
                    vec![serde_json::json!({"role": "user", "content": format!("q{k}")})];
                if k > 0 {
                    messages
                        .push(serde_json::json!({"role": "assistant", "content": answers[k - 1]}));
                }
                Some(serde_json::Value::Array(messages).to_string())
            })
            .collect();
        let schema = Arc::new(Schema::new(vec![
            Field::new("trace_id", DataType::FixedSizeBinary(16), false),
            Field::new("input", DataType::Utf8, true),
            Field::new("output", DataType::Utf8, true),
        ]));
        let ids =
            FixedSizeBinaryArray::try_new(16, Buffer::from_vec(vec![0u8; 16 * n]), None).unwrap();
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(ids),
                Arc::new(arrow::array::StringArray::from(inputs)),
                Arc::new(arrow::array::StringArray::from(outputs)),
            ],
        )
        .unwrap();
        let (file, summary) = write_and_open(&batch, Config::default()).await;
        for c in [1, 2] {
            assert_eq!(file.directory()[c].layer, Layer::Payload);
            let decoded = read_column(&file, c).await.unwrap();
            assert_same(decoded.as_ref(), batch.column(c).as_ref());
        }
        let input = summary.columns[1].payload.as_ref().unwrap();
        let output = summary.columns[2].payload.as_ref().unwrap();
        // One store: the same chunk count and frames in both columns, counted once.
        assert_eq!(input.unique_chunks, output.unique_chunks);
        assert_eq!(input.frames, output.frames);
        assert!(input.frame_bytes > 0);
        assert_eq!(output.frame_bytes, 0);
        // The answers are stored about once, though each appears twice.
        let raw_answers: u64 = answers.iter().map(|a| a.len() as u64).sum();
        assert!(
            input.frame_bytes < raw_answers,
            "{} frame bytes for {raw_answers} bytes of answers",
            input.frame_bytes
        );
    }

    #[tokio::test]
    async fn contains_matches_on_prose() {
        let values: Vec<Option<String>> = (0..200)
            .map(|k| (k % 7 != 3).then(|| prose(200, k as u64 + 1)))
            .collect();
        let batch = table(&values);
        let (file, summary) = write_and_open(&batch, Config::default()).await;
        let stats = summary.columns[1].payload.as_ref().unwrap();
        assert!(stats.index_bytes > 0);
        for needle in ["w1", "w12345 w", "absent needle", ""] {
            let got = crate::payload::payload_contains(&file, 1, needle.as_bytes())
                .await
                .unwrap();
            let want: Vec<bool> = values
                .iter()
                .map(|v| v.as_deref().is_some_and(|v| v.contains(needle)))
                .collect();
            assert_eq!(got.rows, want, "{needle:?}");
        }
    }

    #[tokio::test]
    async fn an_own_store_is_indexed_and_searched_exactly() {
        let values: Vec<Option<String>> = (0..200)
            .map(|k| (k % 7 != 3).then(|| prose(200, k as u64 + 1)))
            .collect();
        let batch = table(&values);
        let config = Config {
            own_store_columns: vec!["v".into()],
            ..Config::default()
        };
        let (file, summary) = write_and_open(&batch, config).await;
        let stats = summary.columns[1].payload.as_ref().unwrap();
        assert!(stats.index_bytes > 0);
        for needle in ["w1", "w12345 w", "absent needle", ""] {
            let got = crate::payload::payload_contains(&file, 1, needle.as_bytes())
                .await
                .unwrap();
            let want: Vec<bool> = values
                .iter()
                .map(|v| v.as_deref().is_some_and(|v| v.contains(needle)))
                .collect();
            assert_eq!(got.rows, want, "{needle:?}");
        }
    }

    #[tokio::test]
    async fn shared_and_own_stores_train_a_dictionary_from_dict_min_raw() {
        // Over `DICT_MIN_RAW` of distinct text, the shared store and a column's own store both
        // train a dictionary, and the column reads back. Under it, neither does.
        for (rows, trained) in [(150, true), (20, false)] {
            let values: Vec<Option<String>> =
                (0..rows).map(|k| Some(prose(5_000, k + 1))).collect();
            let batch = table(&values);
            for own in [vec![], vec!["v".to_string()]] {
                let config = Config {
                    own_store_columns: own,
                    ..Config::default()
                };
                let (file, summary) = write_and_open(&batch, config).await;
                let stats = summary.columns[1].payload.as_ref().unwrap();
                assert_eq!(stats.dict_bytes > 0, trained, "{rows} rows");
                let decoded = read_column(&file, 1).await.unwrap();
                assert_same(decoded.as_ref(), batch.column(1).as_ref());
            }
        }
    }

    #[tokio::test]
    async fn a_higher_payload_level_writes_smaller_frames_that_read_back() {
        let values: Vec<Option<String>> = (0..150).map(|k| Some(prose(5_000, k + 1))).collect();
        let batch = table(&values);
        let mut frames = Vec::new();
        for level in [3, 19] {
            let config = Config {
                payload_zstd_level: level,
                ..Config::default()
            };
            let (file, summary) = write_and_open(&batch, config).await;
            frames.push(summary.columns[1].payload.as_ref().unwrap().frame_bytes);
            let decoded = read_column(&file, 1).await.unwrap();
            assert_same(decoded.as_ref(), batch.column(1).as_ref());
        }
        assert!(frames[1] < frames[0], "{frames:?}");
    }

    #[tokio::test]
    async fn values_seen_before_reuse_their_chunks() {
        // Three values in turn: each is cut once, then found by its hash.
        let distinct: Vec<String> = (0..3).map(|k| prose(2_000, k + 1)).collect();
        let values: Vec<Option<String>> = (0..90).map(|k| Some(distinct[k % 3].clone())).collect();
        let batch = table(&values);
        let (file, summary) = write_and_open(&batch, Config::default()).await;
        let stats = summary.columns[1].payload.as_ref().unwrap();
        // Each of the three values appears 30 times with the same chunks; no chunk is stored
        // beyond those of one appearance of each.
        assert_eq!(stats.chunk_refs % 30, 0);
        assert!(
            u64::from(stats.unique_chunks) <= stats.chunk_refs / 30,
            "{} unique chunks for {} refs",
            stats.unique_chunks,
            stats.chunk_refs
        );
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

    #[tokio::test]
    async fn grown_conversations_reuse_the_previous_chunks() {
        // Each value is the previous one with one more message: the chunks of the shared
        // prefix are reused, so the refs grow by a few ids per row and no chunk is stored twice.
        let mut messages: Vec<String> = Vec::new();
        let mut values = Vec::new();
        for k in 0..40u64 {
            messages.push(
                serde_json::json!({"role": "user", "content": prose(150, k + 1)}).to_string(),
            );
            values.push(Some(format!("[{}]", messages.join(","))));
        }
        let batch = table(&values);
        let (file, summary) = write_and_open(&batch, Config::default()).await;
        let stats = summary.columns[1].payload.as_ref().unwrap();
        let last: Vec<std::ops::Range<usize>> =
            crate::payload::chunk_value(values[39].as_ref().unwrap().as_bytes());
        // Every chunk of the last value is unique, and the earlier values add only their tails.
        assert!(
            stats.unique_chunks as usize <= last.len() + 80,
            "{} unique, {} in the last value",
            stats.unique_chunks,
            last.len()
        );
        let decoded = read_column(&file, 1).await.unwrap();
        assert_same(decoded.as_ref(), batch.column(1).as_ref());
    }

    #[test]
    fn common_prefix_counts_equal_bytes() {
        assert_eq!(common_prefix(b"", b"abc"), 0);
        assert_eq!(common_prefix(b"abc", b"abd"), 2);
        let a = vec![7u8; 1_000];
        let mut b = a.clone();
        b[777] = 0;
        assert_eq!(common_prefix(&a, &b), 777);
        assert_eq!(common_prefix(&a, &a[..600]), 600);
    }

    #[test]
    fn short_columns_are_narrow() {
        let short = table(&[Some("abc".to_string()), None]);
        assert!(!is_payload(
            &[Arc::clone(short.column(1))],
            &Config::default()
        ));
        let all_null = table(&[None, None]);
        assert!(!is_payload(
            &[Arc::clone(all_null.column(1))],
            &Config::default()
        ));
        // The parts of a column count together.
        let long = table(&[Some("x".repeat(1_000))]);
        let parts = [Arc::clone(short.column(1)), Arc::clone(long.column(1))];
        assert!(is_payload(&parts, &Config::default()));
    }
}
