//! Reading payload columns: the refs pages and frames that hold given rows, and the values
//! rebuilt from their chunks. Whole columns and point reads share these functions, so the
//! pages and frames are decoded in one place.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, OnceLock};

use arrow::array::{ArrayRef, BinaryArray, StringArray, StringViewBuilder};
use arrow::buffer::{Buffer, NullBuffer, OffsetBuffer, ScalarBuffer};
use arrow::datatypes::{DataType, Field};

use super::MAX_READ_IDS;
use super::header::PayloadHeader;
use super::refs::decode_page;
use super::store::{Dict, FrameEntry, PayloadStore, decode_fetched_frame};
use crate::error::{Result, StrataError};
use crate::file::StrataFile;
use crate::hot::DirectoryRecord;
use crate::io::ScanKind;
use crate::segment::{Kind, SegmentRef, Segments, decode_segment_with};

/// The raw body of a frame, and the ranges of its chunks in it.
pub(crate) type FrameBody = (Vec<u8>, Vec<Range<usize>>);

/// Largest total of value bytes in one column: Arrow's string and binary offsets are `i32`.
const MAX_VALUE_BYTES: u64 = i32::MAX as u64;

/// Decodes a payload header segment body. Its row count must be the file's.
pub(crate) fn decode_header(
    file: &StrataFile,
    record: &DirectoryRecord,
    body: &[u8],
    store: Arc<PayloadStore>,
) -> Result<PayloadHeader> {
    let at = record.payload_offset;
    let header = PayloadHeader::decode(body, at, store)?;
    if u64::from(header.n_rows) != file.row_count() {
        return Err(StrataError::Corrupt {
            what: "payload row count",
            offset: at,
        });
    }
    Ok(header)
}

/// The header of payload column `column`. It is read with the column's other metadata on first
/// use, and cached, so after the first call it costs no request.
pub(crate) async fn read_header(file: &StrataFile, column: usize) -> Result<Arc<PayloadHeader>> {
    if let Some(header) = file.cached_header(column) {
        return Ok(header);
    }
    file.load_metadata(&[column], ScanKind::Point).await?;
    file.cached_header(column)
        .ok_or_else(|| StrataError::Schema {
            reason: format!("column {column} is not a payload column"),
        })
}

/// The index of the refs page that holds `row`: the last page whose first row is at most `row`.
fn page_of(header: &PayloadHeader, row: u32) -> Option<usize> {
    header
        .ref_pages
        .partition_point(|page| page.first_row <= row)
        .checked_sub(1)
}

/// The refs pages that hold at least one of `rows` (sorted, unique): their segments, and their
/// indexes in `header.ref_pages`, in order.
pub(crate) fn refs_segments(
    header: &PayloadHeader,
    rows: &[u32],
) -> Result<(Segments, Vec<usize>)> {
    let corrupt = || StrataError::Corrupt {
        what: "payload ref page",
        offset: 0,
    };
    let mut pages: Vec<usize> = Vec::new();
    for &row in rows {
        let page = page_of(header, row).ok_or_else(corrupt)?;
        if pages.last() != Some(&page) {
            pages.push(page);
        }
    }
    let mut segments = Vec::with_capacity(pages.len());
    for &page in &pages {
        let entry = header.ref_pages.get(page).ok_or_else(corrupt)?;
        let segment = SegmentRef {
            offset: entry.offset,
            len: entry.len,
        };
        segments.push((segment, Kind::PayloadRefs));
    }
    Ok((segments, pages))
}

/// The most chunk ids one read may return, summed over its rows. Above it the read fails with
/// `Corrupt { what: "refs ids" }`; the writer refuses a column that would need more.
pub(crate) fn add_ids(total: usize, added: usize, at: u64) -> Result<usize> {
    total
        .checked_add(added)
        .filter(|sum| *sum <= MAX_READ_IDS)
        .ok_or_else(|| StrataError::Corrupt {
            what: "refs ids",
            offset: at,
        })
}

/// Decodes the refs pages `pages` (indexes into `header.ref_pages`, with their fetched bodies in
/// the same order), and returns the chunk ids of each row in `rows` (sorted and unique), `None`
/// for a null row. `at` is the header's offset, used in errors.
///
/// Pages are decoded one at a time, in order. Each row's ids are moved out of its page, and the
/// page is dropped before the next one is decoded.
pub(crate) fn ids_of(
    header: &PayloadHeader,
    pages: &[usize],
    bodies: &[Vec<u8>],
    rows: &[u32],
    at: u64,
) -> Result<Vec<Option<Vec<u32>>>> {
    let corrupt = |what: &'static str| StrataError::Corrupt { what, offset: at };
    if pages.len() != bodies.len() {
        return Err(corrupt("segment range"));
    }
    // Moving ids out of a page takes each row once, so a repeated row would read as null.
    if rows.windows(2).any(|pair| match pair {
        [a, b] => a >= b,
        _ => false,
    }) {
        return Err(StrataError::Schema {
            reason: "internal: rows out of order".to_string(),
        });
    }
    let mut out: Vec<Option<Vec<u32>>> = vec![None; rows.len()];
    let mut total = 0usize;
    // The first position of `rows` not answered yet. Rows are sorted, so the rows of a page
    // are the run of `rows` below the next page's first row.
    let mut next = 0usize;
    for (&page_index, body) in pages.iter().zip(bodies) {
        let page = header
            .ref_pages
            .get(page_index)
            .ok_or_else(|| corrupt("payload ref page rows"))?;
        let next_first = match header.ref_pages.get(page_index + 1) {
            Some(next) => next.first_row,
            None => header.n_rows,
        };
        let want = next_first
            .checked_sub(page.first_row)
            .ok_or_else(|| corrupt("payload ref page rows"))?;
        // The page is decoded up to the last row wanted from it.
        let limit = rows
            .get(next..)
            .unwrap_or_default()
            .iter()
            .take_while(|&&row| row < next_first)
            .last()
            .map_or(0, |&row| (row - page.first_row) as usize + 1);
        // Page i holds exactly the rows from its first row up to the next page's first row (or
        // to the end of the column). The row count is checked before its rows are used.
        let (n_rows, mut page_rows) = decode_page(body, page.offset, limit)?;
        if usize::try_from(want).ok() != Some(n_rows) {
            return Err(corrupt("payload ref page rows"));
        }
        while let Some(&row) = rows.get(next) {
            if row >= next_first {
                break;
            }
            let local = usize::try_from(
                row.checked_sub(page.first_row)
                    .ok_or_else(|| corrupt("payload chunk id"))?,
            )
            .map_err(|_| corrupt("payload chunk id"))?;
            let ids = page_rows
                .get_mut(local)
                .map(std::mem::take)
                .ok_or_else(|| corrupt("payload ref page rows"))?;
            total = add_ids(total, ids.as_ref().map_or(0, Vec::len), at)?;
            if let Some(slot) = out.get_mut(next) {
                *slot = ids;
            }
            next += 1;
        }
    }
    if next != rows.len() {
        return Err(corrupt("payload chunk id"));
    }
    Ok(out)
}

/// The segments of the frames `indexes` (indexes into `header.frames`), in order.
pub(crate) fn frame_segments_of(
    header: &PayloadHeader,
    indexes: &[usize],
) -> Result<Vec<(SegmentRef, Kind)>> {
    let corrupt = || StrataError::Corrupt {
        what: "payload frame",
        offset: 0,
    };
    indexes
        .iter()
        .map(|&index| {
            let entry: &FrameEntry = header.store.frames.get(index).ok_or_else(corrupt)?;
            let segment = SegmentRef {
                offset: entry.offset,
                len: entry.len,
            };
            Ok((segment, Kind::PayloadFrame))
        })
        .collect()
}

/// The frames that hold at least one id of `ids`: their segments, and their indexes in
/// `header.frames`, ascending.
pub(crate) fn frame_segments(
    header: &PayloadHeader,
    ids: &[Option<Vec<u32>>],
) -> Result<(Segments, Vec<usize>)> {
    let corrupt = || StrataError::Corrupt {
        what: "payload chunk id",
        offset: 0,
    };
    let store = &header.store;
    let mut used = vec![false; store.frames.len()];
    // Most ids fall in the frame of the id before them: that frame is tried first.
    let mut last: Option<(u32, u32, usize)> = None;
    for &id in ids.iter().flatten().flatten() {
        let index = match last {
            Some((first, n, index)) if id >= first && id - first < n => index,
            _ => {
                let index = store.frame_of(id).ok_or_else(corrupt)?;
                let entry = store.frames.get(index).ok_or_else(corrupt)?;
                last = Some((entry.first_chunk, entry.n_chunks, index));
                index
            }
        };
        if let Some(flag) = used.get_mut(index) {
            *flag = true;
        }
    }
    let frames: Vec<usize> = used
        .iter()
        .enumerate()
        .filter_map(|(index, &u)| u.then_some(index))
        .collect();
    let segments = frame_segments_of(header, &frames)?;
    Ok((segments, frames))
}

/// The frames of one read, by the file offset of their segment, which is unique in a file:
/// columns that share a store share its frames. A frame is either decoded already, or kept as
/// fetched and decoded on first use, by whichever thread needs it first, so that its bytes are
/// still in cache when its chunks are copied.
#[derive(Default)]
pub(crate) struct FrameBodies {
    slots: HashMap<u64, FrameSlot>,
}

struct FrameSlot {
    entry: FrameEntry,
    /// The fetched segment, header included; empty once decoded up front.
    stored: Vec<u8>,
    dict: Option<Arc<Dict>>,
    /// The decoded frame; `None` when it failed to decode.
    body: OnceLock<Option<FrameBody>>,
}

impl FrameBodies {
    /// Checks and decodes the fetched bodies of the frames `indexes` (in the same order).
    pub(crate) fn decoded(
        header: &PayloadHeader,
        indexes: &[usize],
        bodies: Vec<Vec<u8>>,
    ) -> Result<Self> {
        let corrupt = || StrataError::Corrupt {
            what: "payload frame",
            offset: 0,
        };
        if indexes.len() != bodies.len() {
            return Err(corrupt());
        }
        let mut slots = HashMap::with_capacity(indexes.len());
        for (&index, raw) in indexes.iter().zip(bodies) {
            let entry = header.store.frames.get(index).ok_or_else(corrupt)?;
            let body = OnceLock::new();
            let _ = body.set(Some(decode_fetched_frame(entry, raw)?));
            let slot = FrameSlot {
                entry: entry.clone(),
                stored: Vec::new(),
                dict: None,
                body,
            };
            slots.insert(entry.offset, slot);
        }
        Ok(Self { slots })
    }

    /// The fetched, still encoded segments `stored` of the frames `entries` (in the same
    /// order), each with the dictionary of its run, decoded on first use.
    pub(crate) fn stored(
        entries: Vec<FrameEntry>,
        stored: Vec<Vec<u8>>,
        dicts: Vec<Option<Arc<Dict>>>,
    ) -> Result<Self> {
        if entries.len() != stored.len() || entries.len() != dicts.len() {
            return Err(StrataError::Corrupt {
                what: "payload frame",
                offset: 0,
            });
        }
        let slots = entries
            .into_iter()
            .zip(stored)
            .zip(dicts)
            .map(|((entry, stored), dict)| {
                let slot = FrameSlot {
                    entry,
                    stored,
                    dict,
                    body: OnceLock::new(),
                };
                (slot.entry.offset, slot)
            })
            .collect();
        Ok(Self { slots })
    }

    /// The decoded frame whose segment starts at `offset`, decoded now if it is not yet.
    fn get(&self, offset: u64) -> Result<&FrameBody> {
        let corrupt = || StrataError::Corrupt {
            what: "payload frame",
            offset,
        };
        let slot = self.slots.get(&offset).ok_or_else(corrupt)?;
        slot.body
            .get_or_init(|| {
                let raw = decode_segment_with(
                    &slot.stored,
                    slot.entry.offset,
                    Kind::PayloadFrame,
                    slot.dict.as_deref(),
                )
                .ok()?;
                decode_fetched_frame(&slot.entry, raw).ok()
            })
            .as_ref()
            .ok_or_else(corrupt)
    }
}

/// The chunk bytes of `ids`, looked up in the decoded frames. A row's chunks mostly sit in the
/// same frame as the chunk before, so the frame of the last chunk is tried first.
struct ChunkReader<'a> {
    header: &'a PayloadHeader,
    frames: &'a FrameBodies,
    /// The last frame used: its first chunk id, its chunk count and its decoded body.
    last: Option<(u32, u32, &'a FrameBody)>,
    at: u64,
}

impl<'a> ChunkReader<'a> {
    fn chunk(&mut self, id: u32) -> Result<&'a [u8]> {
        let corrupt = || StrataError::Corrupt {
            what: "payload chunk id",
            offset: self.at,
        };
        let (first, body) = match self.last {
            Some((first, n, body)) if id >= first && id - first < n => (first, body),
            _ => {
                let f = self.header.store.frame_of(id).ok_or_else(corrupt)?;
                let entry = self.header.store.frames.get(f).ok_or_else(corrupt)?;
                let body = self.frames.get(entry.offset)?;
                self.last = Some((entry.first_chunk, entry.n_chunks, body));
                (entry.first_chunk, body)
            }
        };
        let (raw, ranges) = body;
        let range = ranges.get((id - first) as usize).ok_or_else(corrupt)?;
        raw.get(range.clone()).ok_or_else(corrupt)
    }
}

/// Rebuilds the values of `ids` (one entry per row, `None` for a null row) from the decoded
/// frames. `field` gives the output type, Utf8 or Binary. `at` is the header's offset.
///
/// The value bytes are sized first, then copied once into the array's buffer.
pub(crate) fn build_values(
    header: &PayloadHeader,
    frames: &FrameBodies,
    ids: &[Option<Vec<u32>>],
    field: &Field,
    at: u64,
) -> Result<ArrayRef> {
    let corrupt = |what: &'static str| StrataError::Corrupt { what, offset: at };
    let utf8 = match field.data_type() {
        DataType::Utf8 => true,
        DataType::Binary => false,
        _ => return Err(corrupt("payload column type")),
    };
    let mut reader = ChunkReader {
        header,
        frames,
        last: None,
        at,
    };
    let mut total: u64 = 0;
    for &id in ids.iter().flatten().flatten() {
        total += reader.chunk(id)?.len() as u64;
        if total > MAX_VALUE_BYTES {
            return Err(corrupt("payload size"));
        }
    }
    let mut values: Vec<u8> = Vec::with_capacity(total as usize);
    let mut offsets: Vec<i32> = Vec::with_capacity(ids.len() + 1);
    offsets.push(0);
    for row in ids {
        // A null row repeats the offset before it.
        for &id in row.iter().flatten() {
            values.extend_from_slice(reader.chunk(id)?);
        }
        // `total` is at most `MAX_VALUE_BYTES` (`i32::MAX`), so every offset fits.
        offsets.push(i32::try_from(values.len()).map_err(|_| corrupt("payload size"))?);
    }
    let nulls = ids
        .iter()
        .any(Option::is_none)
        .then(|| NullBuffer::from_iter(ids.iter().map(Option::is_some)));
    // `OffsetBuffer::new` panics on a decreasing offset; these only grow, from 0.
    let offsets = OffsetBuffer::new(ScalarBuffer::from(offsets));
    let values = Buffer::from_vec(values);
    let array: ArrayRef = if utf8 {
        Arc::new(StringArray::try_new(offsets, values, nulls).map_err(|_| corrupt("payload utf8"))?)
    } else {
        Arc::new(BinaryArray::try_new(offsets, values, nulls).map_err(|_| corrupt("payload size"))?)
    };
    Ok(array)
}

/// Bytes of one data block of a `Utf8View` array built by [`build_string_views`]: small
/// enough that a block is still in cache when its values are checked.
const VIEW_BLOCK_BYTES: usize = 256 << 10;

/// The values of `ids` (one entry per row, `None` for a null row) of a Utf8 column, as a
/// `Utf8View` array. Values are copied into blocks of about `VIEW_BLOCK_BYTES`, and each value
/// is checked as UTF-8 when its view is added, right after its block is filled.
fn build_string_views(
    header: &PayloadHeader,
    frames: &FrameBodies,
    ids: &[Option<Vec<u32>>],
    at: u64,
) -> Result<ArrayRef> {
    let corrupt = |what: &'static str| StrataError::Corrupt { what, offset: at };
    let mut reader = ChunkReader {
        header,
        frames,
        last: None,
        at,
    };
    let mut builder = StringViewBuilder::with_capacity(ids.len());
    // The bytes of the values not copied yet.
    let mut left = 0usize;
    for &id in ids.iter().flatten().flatten() {
        left += reader.chunk(id)?.len();
    }
    let mut block: Vec<u8> = Vec::new();
    // The rows of the block being filled: `(start, len)` in the block, `None` for a null row.
    let mut rows: Vec<Option<(u32, u32)>> = Vec::new();
    let flush = |block: &mut Vec<u8>,
                 rows: &mut Vec<Option<(u32, u32)>>,
                 builder: &mut StringViewBuilder|
     -> Result<()> {
        if block.len() >= u32::MAX as usize {
            return Err(corrupt("payload size"));
        }
        let id = builder.append_block(Buffer::from_vec(std::mem::take(block)));
        for row in rows.drain(..) {
            match row {
                Some((start, len)) => builder
                    .try_append_view(id, start, len)
                    .map_err(|_| corrupt("payload utf8"))?,
                None => builder.append_null(),
            }
        }
        Ok(())
    };
    for row in ids {
        let Some(row) = row else {
            rows.push(None);
            continue;
        };
        // The value is sized first, so that a block is allocated once at its final size and
        // never grows: a value that does not fit closes the block and starts the next one. A
        // block is no bigger than the values left need, so a small read allocates little.
        let mut value_len = 0usize;
        for &id in row {
            value_len += reader.chunk(id)?.len();
        }
        if block.capacity() - block.len() < value_len {
            if !block.is_empty() {
                flush(&mut block, &mut rows, &mut builder)?;
            }
            block = Vec::with_capacity(value_len.max(VIEW_BLOCK_BYTES.min(left)));
        }
        left = left.saturating_sub(value_len);
        let start = block.len();
        for &id in row {
            block.extend_from_slice(reader.chunk(id)?);
        }
        let len = block.len() - start;
        let to_u32 = |v: usize| u32::try_from(v).map_err(|_| corrupt("payload size"));
        rows.push(Some((to_u32(start)?, to_u32(len)?)));
        if block.len() >= VIEW_BLOCK_BYTES {
            flush(&mut block, &mut rows, &mut builder)?;
        }
    }
    flush(&mut block, &mut rows, &mut builder)?;
    Ok(Arc::new(builder.finish()))
}

/// The values of `ids` in pieces of `rows` rows each (the last one may be shorter), built on
/// several threads. With `views`, a Utf8 column comes out as `Utf8View`
/// ([`build_string_views`]); otherwise as [`build_values`] builds it.
pub(crate) fn build_value_pieces(
    header: &PayloadHeader,
    frames: &FrameBodies,
    ids: &[Option<Vec<u32>>],
    field: &Field,
    at: u64,
    rows: usize,
    views: bool,
) -> Result<Vec<ArrayRef>> {
    let pieces: Vec<&[Option<Vec<u32>>]> = ids.chunks(rows.max(1)).collect();
    let as_views = views && field.data_type() == &DataType::Utf8;
    crate::parallel::map_in_order(
        &pieces,
        |piece| piece.iter().flatten().map(Vec::len).sum::<usize>() + 1,
        |piece| {
            if as_views {
                build_string_views(header, frames, piece, at)
            } else {
                build_values(header, frames, piece, field, at)
            }
        },
    )
}

/// Reads every row of payload column `column`. The header is read first (a wave of its own,
/// unless cached); then the refs pages and every frame are one scan wave. The frames are
/// decoded and each row's values are rebuilt from its chunk ids.
pub(crate) async fn read_payload_column(
    file: &StrataFile,
    column: usize,
    record: &DirectoryRecord,
    field: &Field,
) -> Result<ArrayRef> {
    let at = record.payload_offset;
    let header = read_header(file, column).await?;

    let rows: Vec<u32> = (0..header.n_rows).collect();
    let (mut segments, pages) = refs_segments(&header, &rows)?;
    let n_pages = pages.len();
    let frame_indexes: Vec<usize> = (0..header.store.frames.len()).collect();
    segments.extend(frame_segments_of(&header, &frame_indexes)?);

    let mut bodies = file.fetch(&segments, ScanKind::Scan).await?;
    // `fetch` returns one body per segment, so the page bodies come first.
    let frame_bodies = bodies.split_off(n_pages.min(bodies.len()));
    let ids = ids_of(&header, &pages, &bodies, &rows, at)?;
    let frames = FrameBodies::decoded(&header, &frame_indexes, frame_bodies)?;
    build_values(&header, &frames, &ids, field, at)
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
    use crate::testgen::wide_batch;
    use crate::writer::StrataWriter;
    use object_store::memory::InMemory;
    use object_store::path::Path;
    use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    /// The `blob` column of `wide_batch(2_000)`, written as a payload column, and its index.
    async fn blob_file(rows: usize) -> (StrataFile, usize) {
        let schema = crate::testgen::wide_schema();
        let column = schema.index_of("blob").unwrap();
        let config = Config {
            payload_min_avg: 0,
            ..Config::default()
        };
        let mut w = StrataWriter::new(schema, &["trace_id", "start_time"], config).unwrap();
        w.write(&wide_batch(rows)).unwrap();
        let (bytes, _) = w.finish().unwrap();
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("blob.strata");
        store.put(&path, PutPayload::from(bytes)).await.unwrap();
        let file = StrataFile::open(store, path, None, &Config::default())
            .await
            .unwrap();
        (file, column)
    }

    #[test]
    fn string_views_match_the_plain_values_over_several_blocks() {
        runtime().block_on(async {
            // 3,000 rows of about 450 B, every seventh null: several 256 KiB blocks.
            let n = 3_000usize;
            let schema = Arc::new(arrow::datatypes::Schema::new(vec![
                Field::new("trace_id", DataType::FixedSizeBinary(16), false),
                Field::new("text", DataType::Utf8, true),
            ]));
            let ids: Vec<[u8; 16]> = (0..n).map(|i| (i as u128).to_be_bytes()).collect();
            let text: Vec<Option<String>> = (0..n)
                .map(|i| (i % 7 != 3).then(|| format!("é{i} ünïcode {} ", i * 31).repeat(25)))
                .collect();
            let batch = arrow::record_batch::RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(
                        arrow::array::FixedSizeBinaryArray::try_from_iter(ids.iter()).unwrap(),
                    ),
                    Arc::new(StringArray::from(text.clone())),
                ],
            )
            .unwrap();
            let mut w = StrataWriter::new(schema, &["trace_id"], Config::default()).unwrap();
            w.write(&batch).unwrap();
            let (bytes, _) = w.finish().unwrap();
            let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            let path = Path::from("text.strata");
            store.put(&path, PutPayload::from(bytes)).await.unwrap();
            let file = StrataFile::open(store, path, None, &Config::default())
                .await
                .unwrap();

            let rows: Vec<u32> = (0..n as u32).collect();
            let header = read_header(&file, 1).await.unwrap();
            let (segments, pages) = refs_segments(&header, &rows).unwrap();
            let bodies = file.fetch(&segments, ScanKind::Scan).await.unwrap();
            let ids = ids_of(&header, &pages, &bodies, &rows, 0).unwrap();
            let (segments, frames) = frame_segments(&header, &ids).unwrap();
            let bodies = file.fetch(&segments, ScanKind::Scan).await.unwrap();
            let decoded = FrameBodies::decoded(&header, &frames, bodies).unwrap();
            let field = schema_field(&file, 1);

            let views = build_string_views(&header, &decoded, &ids, 0).unwrap();
            assert_eq!(views.data_type(), &DataType::Utf8View);
            let views = arrow::array::AsArray::as_string_view(views.as_ref());
            assert!(views.data_buffers().len() > 1);
            let plain = build_values(&header, &decoded, &ids, &field, 0).unwrap();
            let as_utf8 = arrow::compute::cast(views, &DataType::Utf8).unwrap();
            assert_eq!(as_utf8.as_ref(), plain.as_ref());
            assert_eq!(
                plain.as_ref(),
                &StringArray::from(text) as &dyn arrow::array::Array
            );
        });
    }

    #[test]
    fn point_reads_give_the_rows_of_read_payload_column() {
        runtime().block_on(async {
            let (file, column) = blob_file(2_000).await;
            let whole = crate::reader::read_column(&file, column).await.unwrap();
            let rows: Vec<u32> = {
                let mut rows: Vec<u32> = (0..50u32).map(|i| (i * 37) % 2_000).collect();
                rows.sort_unstable();
                rows.dedup();
                rows
            };
            let header = read_header(&file, column).await.unwrap();
            let (segments, pages) = refs_segments(&header, &rows).unwrap();
            let bodies = file.fetch(&segments, ScanKind::Point).await.unwrap();
            let ids = ids_of(&header, &pages, &bodies, &rows, 0).unwrap();
            let (segments, frames) = frame_segments(&header, &ids).unwrap();
            let bodies = file.fetch(&segments, ScanKind::Point).await.unwrap();
            let decoded = FrameBodies::decoded(&header, &frames, bodies).unwrap();
            let field = schema_field(&file, column);
            let got = build_values(&header, &decoded, &ids, &field, 0).unwrap();
            let want = arrow::compute::take(
                whole.as_ref(),
                &arrow::array::UInt32Array::from(rows.clone()),
                None,
            )
            .unwrap();
            assert_eq!(got.as_ref(), want.as_ref());
        });
    }

    #[test]
    fn ids_of_over_two_pages_matches_the_whole_column() {
        runtime().block_on(async {
            // 2,200 rows make three refs pages (REFS_PAGE_ROWS is 1,024).
            let (file, column) = blob_file(2_200).await;
            let whole = crate::reader::read_column(&file, column).await.unwrap();
            // Rows on both sides of each page boundary, and the last row.
            let rows: Vec<u32> = vec![0, 1_023, 1_024, 1_025, 2_047, 2_048, 2_199];
            let header = read_header(&file, column).await.unwrap();
            assert!(header.ref_pages.len() >= 3);
            let (segments, pages) = refs_segments(&header, &rows).unwrap();
            assert_eq!(pages.len(), 3);
            let bodies = file.fetch(&segments, ScanKind::Point).await.unwrap();
            let ids = ids_of(&header, &pages, &bodies, &rows, 0).unwrap();
            let (segments, frames) = frame_segments(&header, &ids).unwrap();
            let bodies = file.fetch(&segments, ScanKind::Point).await.unwrap();
            let decoded = FrameBodies::decoded(&header, &frames, bodies).unwrap();
            let field = schema_field(&file, column);
            let got = build_values(&header, &decoded, &ids, &field, 0).unwrap();
            let want = arrow::compute::take(
                whole.as_ref(),
                &arrow::array::UInt32Array::from(rows.clone()),
                None,
            )
            .unwrap();
            assert_eq!(got.as_ref(), want.as_ref());
        });
    }

    #[test]
    fn add_ids_fails_only_above_the_cap() {
        assert_eq!(add_ids(0, MAX_READ_IDS, 7).unwrap(), MAX_READ_IDS);
        assert_eq!(add_ids(MAX_READ_IDS - 1, 1, 7).unwrap(), MAX_READ_IDS);
        for (total, added) in [(MAX_READ_IDS, 1), (MAX_READ_IDS / 2 + 1, MAX_READ_IDS / 2)] {
            match add_ids(total, added, 7) {
                Err(StrataError::Corrupt { what, offset }) => {
                    assert_eq!(what, "refs ids");
                    assert_eq!(offset, 7);
                }
                other => panic!("expected Corrupt, got {other:?}"),
            }
        }
        assert!(add_ids(usize::MAX, 1, 0).is_err());
    }

    #[test]
    fn no_rows_give_no_segments_and_an_empty_array() {
        runtime().block_on(async {
            let (file, column) = blob_file(2_000).await;
            let header = read_header(&file, column).await.unwrap();
            let (segments, pages) = refs_segments(&header, &[]).unwrap();
            assert!(segments.is_empty() && pages.is_empty());
            let (segments, frames) = frame_segments(&header, &[]).unwrap();
            assert!(segments.is_empty() && frames.is_empty());
            let field = schema_field(&file, column);
            let array = build_values(&header, &FrameBodies::default(), &[], &field, 0).unwrap();
            assert_eq!(array.len(), 0);
        });
    }

    /// The schema field of column `column` of `file`.
    fn schema_field(file: &StrataFile, column: usize) -> Field {
        file.schema().field(column).clone()
    }
}
