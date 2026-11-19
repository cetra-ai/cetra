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

/// The raw body of a frame, and the ranges of its chunks in it. The body is an Arrow buffer so
/// that a `Utf8View` array can point into it instead of copying its values.
pub(crate) type FrameBody = (Buffer, Vec<Range<usize>>);

/// Checks and decodes a fetched frame body into a [`FrameBody`].
fn frame_body(entry: &FrameEntry, raw: Vec<u8>) -> Result<FrameBody> {
    let (raw, ranges) = decode_fetched_frame(entry, raw)?;
    Ok((Buffer::from_vec(raw), ranges))
}

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
/// `header.frames`, ascending. Scans plan their frames with the batches that use them; tests
/// read whole columns with this.
#[cfg(test)]
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
    stored: bytes::Bytes,
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
            let _ = body.set(Some(frame_body(entry, raw)?));
            let slot = FrameSlot {
                entry: entry.clone(),
                stored: bytes::Bytes::new(),
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
        stored: Vec<bytes::Bytes>,
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

    /// The frames held.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    /// Drops the frame whose segment starts at `offset`. Arrays that share its body keep it
    /// alive until they are dropped.
    pub(crate) fn release(&mut self, offset: u64) {
        self.slots.remove(&offset);
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
                frame_body(&slot.entry, raw).ok()
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
        raw.as_slice().get(range.clone()).ok_or_else(corrupt)
    }

    /// The frame body and byte range of a value made of `ids`, when they are consecutive chunks
    /// of one frame: a frame's chunks are stored back to back, so their bytes are contiguous.
    /// `None` for an empty value or one whose chunks are elsewhere.
    fn span(&mut self, ids: &[u32]) -> Result<Option<(&'a Buffer, Range<usize>)>> {
        let (Some(&first_id), Some(&last_id)) = (ids.first(), ids.last()) else {
            return Ok(None);
        };
        let consecutive = ids
            .windows(2)
            .all(|pair| matches!(pair, [a, b] if a.checked_add(1) == Some(*b)));
        if !consecutive {
            return Ok(None);
        }
        // Resolves the frame of the first id into `last`.
        self.chunk(first_id)?;
        let Some((first, n, (raw, ranges))) = self.last else {
            return Ok(None);
        };
        if last_id - first >= n {
            return Ok(None);
        }
        let corrupt = || StrataError::Corrupt {
            what: "payload chunk id",
            offset: self.at,
        };
        let start = ranges
            .get((first_id - first) as usize)
            .ok_or_else(corrupt)?
            .start;
        let end = ranges
            .get((last_id - first) as usize)
            .ok_or_else(corrupt)?
            .end;
        if start > end || end > raw.len() {
            return Err(corrupt());
        }
        Ok(Some((raw, start..end)))
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

/// Largest data block of a `Utf8View` array built by [`build_string_views`] for the values
/// it copies.
const VIEW_BLOCK_BYTES: usize = 256 << 10;

/// How [`build_string_views`] adds one row.
enum ViewRow<'a> {
    Null,
    /// A view into the shared frame body with this block id: (block, start, length).
    Shared(u32, u32, u32),
    /// Bytes consecutive in one frame body, copied.
    Span(&'a [u8]),
    /// A value rebuilt from these chunk ids, copied.
    Chunks(&'a [u32]),
}

/// The values of `ids` (one entry per row, `None` for a null row) of a Utf8 column, as a
/// `Utf8View` array. An ASCII value whose chunks are consecutive in one frame is a view into
/// that frame's body, which the array then shares; arrow checks its UTF-8 when the view is
/// added. Other values are rebuilt from their chunks, checked with `simdutf8`, and copied
/// into blocks of at most `VIEW_BLOCK_BYTES`: arrow's own check of a view is
/// `std::str::from_utf8`, fast on ASCII but several times slower than `simdutf8` on other
/// text.
///
/// The shared bodies are all added before any value: adding a block closes the block being
/// filled, so adding them as they come would leave most of each copy block empty.
fn build_string_views(
    header: &PayloadHeader,
    frames: &FrameBodies,
    ids: &[Option<Vec<u32>>],
    at: u64,
) -> Result<ArrayRef> {
    let corrupt = |what: &'static str| StrataError::Corrupt { what, offset: at };
    let to_u32 = |v: usize| u32::try_from(v).map_err(|_| corrupt("payload size"));
    let mut reader = ChunkReader {
        header,
        frames,
        last: None,
        at,
    };
    let mut builder = StringViewBuilder::with_capacity(ids.len());
    // The block id of each frame body the array shares, by the body's address.
    let mut shared: HashMap<*const u8, u32> = HashMap::new();
    let mut plan: Vec<ViewRow<'_>> = Vec::with_capacity(ids.len());
    // The bytes of the copied values.
    let mut copied = 0usize;
    for row in ids {
        let Some(row) = row else {
            plan.push(ViewRow::Null);
            continue;
        };
        let Some((body, range)) = reader.span(row)? else {
            for &id in row {
                copied += reader.chunk(id)?.len();
            }
            plan.push(ViewRow::Chunks(row));
            continue;
        };
        let bytes = body
            .get(range.clone())
            .ok_or_else(|| corrupt("payload span"))?;
        if !bytes.is_ascii() {
            copied += bytes.len();
            plan.push(ViewRow::Span(bytes));
            continue;
        }
        let id = match shared.get(&body.as_ptr()) {
            Some(&id) => id,
            None => {
                if body.len() >= u32::MAX as usize {
                    return Err(corrupt("payload size"));
                }
                let id = builder.append_block(body.clone());
                shared.insert(body.as_ptr(), id);
                id
            }
        };
        plan.push(ViewRow::Shared(
            id,
            to_u32(range.start)?,
            to_u32(range.len())?,
        ));
    }
    // A small read allocates no more than its values need.
    let block = to_u32(copied.clamp(1, VIEW_BLOCK_BYTES))?;
    let mut builder = builder.with_fixed_block_size(block);
    // The bytes of a value made of chunks that are not consecutive in one frame.
    let mut scratch: Vec<u8> = Vec::new();
    for row in plan {
        let bytes = match row {
            ViewRow::Null => {
                builder.append_null();
                continue;
            }
            ViewRow::Shared(id, start, len) => {
                builder
                    .try_append_view(id, start, len)
                    .map_err(|_| corrupt("payload utf8"))?;
                continue;
            }
            ViewRow::Span(bytes) => bytes,
            ViewRow::Chunks(row) => {
                scratch.clear();
                for &id in row {
                    scratch.extend_from_slice(reader.chunk(id)?);
                }
                scratch.as_slice()
            }
        };
        let value = simdutf8::basic::from_utf8(bytes).map_err(|_| corrupt("payload utf8"))?;
        builder
            .try_append_value(value)
            .map_err(|_| corrupt("payload size"))?;
    }
    Ok(Arc::new(builder.finish()))
}

/// The values of `ids` (one entry per row, `None` for a null row). With `views`, a Utf8 column
/// comes out as `Utf8View` ([`build_string_views`]); otherwise as [`build_values`] builds it.
pub(crate) fn build_payload_values(
    header: &PayloadHeader,
    frames: &FrameBodies,
    ids: &[Option<Vec<u32>>],
    field: &Field,
    at: u64,
    views: bool,
) -> Result<ArrayRef> {
    if views && field.data_type() == &DataType::Utf8 {
        build_string_views(header, frames, ids, at)
    } else {
        build_values(header, frames, ids, field, at)
    }
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
    fn string_views_mix_shared_frames_and_copied_values() {
        mixed_string_views(true);
    }

    #[test]
    fn string_views_copy_the_values_of_non_ascii_frames() {
        mixed_string_views(false);
    }

    /// New values, repeats and extensions of earlier values (whose chunk ids are not
    /// consecutive), empty values and nulls, in a mixed order. With `ascii`, the text is all
    /// ASCII, and the array shares frame bodies; otherwise it shares none.
    fn mixed_string_views(ascii: bool) {
        let (unique, accent, short) = if ascii {
            ("unique", "e", "short")
        } else {
            ("ünïque", "é", "shört")
        };
        runtime().block_on(async {
            let n = 2_000usize;
            let schema = Arc::new(arrow::datatypes::Schema::new(vec![
                Field::new("trace_id", DataType::FixedSizeBinary(16), false),
                Field::new("text", DataType::Utf8, true),
            ]));
            let ids: Vec<[u8; 16]> = (0..n).map(|i| (i as u128).to_be_bytes()).collect();
            let mut text: Vec<Option<String>> = Vec::with_capacity(n);
            for i in 0..n {
                let value = match i % 6 {
                    0 => Some(format!("{unique} {i} {}", "ab".repeat(i % 900))),
                    1 if i > 40 => text[i - 37].clone(),
                    2 if i > 40 => text[i - 29]
                        .clone()
                        .map(|v| format!("{v} then {i} {}", accent.repeat(400))),
                    3 => Some(String::new()),
                    4 => None,
                    _ => Some(format!("{short} {i}")),
                };
                text.push(value);
            }
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
            let path = Path::from("mixed.strata");
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

            let mut reader = ChunkReader {
                header: &header,
                frames: &decoded,
                last: None,
                at: 0,
            };
            let (mut shared, mut copied) = (0, 0);
            for row in ids.iter().flatten() {
                match reader.span(row).unwrap() {
                    Some(_) => shared += 1,
                    None if !row.is_empty() => copied += 1,
                    None => {}
                }
            }
            assert!(
                shared > 100 && copied > 100,
                "{shared} shared, {copied} copied"
            );

            // The whole column: the array's data blocks include frame bodies only for ASCII.
            let bodies: Vec<*const u8> = decoded
                .slots
                .keys()
                .map(|&offset| decoded.get(offset).unwrap().0.as_ptr())
                .collect();
            let views = build_string_views(&header, &decoded, &ids, 0).unwrap();
            let views = arrow::array::AsArray::as_string_view(views.as_ref());
            let shares = views
                .data_buffers()
                .iter()
                .filter(|b| bodies.contains(&b.as_ptr()))
                .count();
            assert_eq!(shares > 0, ascii, "{shares} frame bodies shared");

            // Pieces that start anywhere: with a null, a shared or a copied value first.
            for piece in [n, 7, 64, 333] {
                let mut got: Vec<Option<String>> = Vec::with_capacity(n);
                for part in ids.chunks(piece) {
                    let views = build_string_views(&header, &decoded, part, 0).unwrap();
                    let views = arrow::array::AsArray::as_string_view(views.as_ref());
                    got.extend(views.iter().map(|v| v.map(str::to_string)));
                }
                assert_eq!(got, text, "pieces of {piece}");
            }
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
