//! Reading one payload column back as an Arrow array.

use std::ops::Range;
use std::sync::Arc;

use arrow::array::{ArrayRef, BinaryBuilder, StringBuilder};
use arrow::datatypes::{DataType, Field};
use futures::future::{try_join, try_join_all};

use super::frames::{decode_frame, frame_of};
use super::header::PayloadHeader;
use super::refs::decode_page;
use crate::error::{Result, StrataError};
use crate::file::StrataFile;
use crate::hot::DirectoryRecord;
use crate::segment::{Kind, SegmentRef};

/// Largest total of value bytes in one column: Arrow's string and binary offsets are `i32`.
const MAX_VALUE_BYTES: u64 = i32::MAX as u64;

/// Reads and checks the payload header of a column: its segment must decode, and its row count
/// must be the file's.
pub(crate) async fn read_header(
    file: &StrataFile,
    record: &DirectoryRecord,
) -> Result<PayloadHeader> {
    let at = record.payload_offset;
    let header_ref = SegmentRef {
        offset: record.payload_offset,
        len: record.payload_len,
    };
    let header_body = file.read_segment(&header_ref, Kind::PayloadHeader).await?;
    let header = PayloadHeader::decode(&header_body, at)?;
    if u64::from(header.n_rows) != file.row_count() {
        return Err(StrataError::Corrupt {
            what: "payload row count",
            offset: at,
        });
    }
    Ok(header)
}

/// Reads the refs pages of a column and returns each row's chunk ids, `None` for a null row.
/// `at` is the header's offset, used in errors.
pub(crate) async fn read_rows(
    file: &StrataFile,
    header: &PayloadHeader,
    at: u64,
) -> Result<Vec<Option<Vec<u32>>>> {
    let corrupt = |what: &'static str| StrataError::Corrupt { what, offset: at };
    let pages = try_join_all(header.ref_pages.iter().map(|page| async move {
        let segment = SegmentRef {
            offset: page.offset,
            len: page.len,
        };
        let body = file.read_segment(&segment, Kind::PayloadRefs).await?;
        decode_page(&body, page.offset)
    }))
    .await?;

    // Page i holds exactly the rows from its first row up to the next page's first row (or to
    // the end of the column).
    // Not sized from `n_rows`: rows are added page by page, after each page's count is checked.
    let mut rows: Vec<Option<Vec<u32>>> = Vec::new();
    for (i, (page, page_rows)) in header.ref_pages.iter().zip(pages).enumerate() {
        let next_first = match header.ref_pages.get(i + 1) {
            Some(next) => next.first_row,
            None => header.n_rows,
        };
        let want = next_first
            .checked_sub(page.first_row)
            .ok_or_else(|| corrupt("payload ref page rows"))?;
        if usize::try_from(want).ok() != Some(page_rows.len()) {
            return Err(corrupt("payload ref page rows"));
        }
        rows.extend(page_rows);
    }
    if rows.len() != header.n_rows as usize {
        return Err(corrupt("payload row count"));
    }
    Ok(rows)
}

/// Reads every row of a payload column. The header is read first; then the refs pages and the
/// frames are fetched concurrently. The frames are decoded and each row's values are rebuilt
/// from its chunk ids.
pub(crate) async fn read_payload_column(
    file: &StrataFile,
    record: &DirectoryRecord,
    field: &Field,
) -> Result<ArrayRef> {
    let at = record.payload_offset;
    let corrupt = |what: &'static str| StrataError::Corrupt { what, offset: at };

    let header = read_header(file, record).await?;

    let frames_fetch = try_join_all(header.frames.iter().map(|frame| async move {
        let segment = SegmentRef {
            offset: frame.offset,
            len: frame.len,
        };
        file.read_segment(&segment, Kind::PayloadFrame).await
    }));
    let (rows, raws) = try_join(read_rows(file, &header, at), frames_fetch).await?;

    let mut frames: Vec<(Vec<u8>, Vec<Range<usize>>)> = Vec::with_capacity(header.frames.len());
    for (entry, raw) in header.frames.iter().zip(raws) {
        if usize::try_from(entry.raw_len).ok() != Some(raw.len()) {
            return Err(corrupt("payload frame length"));
        }
        let ranges = decode_frame(&raw, entry.offset, entry.n_chunks)?;
        frames.push((raw, ranges));
    }

    let utf8 = match field.data_type() {
        DataType::Utf8 => true,
        DataType::Binary => false,
        _ => return Err(corrupt("payload column type")),
    };
    let mut strings = StringBuilder::new();
    let mut binaries = BinaryBuilder::new();
    let mut scratch: Vec<u8> = Vec::new();
    let mut total: u64 = 0;
    for row in &rows {
        let Some(ids) = row else {
            if utf8 {
                strings.append_null();
            } else {
                binaries.append_null();
            }
            continue;
        };
        scratch.clear();
        for &id in ids {
            let f = frame_of(&header.frames, id).ok_or_else(|| corrupt("payload chunk id"))?;
            let entry = header
                .frames
                .get(f)
                .ok_or_else(|| corrupt("payload chunk id"))?;
            let local = id
                .checked_sub(entry.first_chunk)
                .ok_or_else(|| corrupt("payload chunk id"))?;
            let (raw, ranges) = frames.get(f).ok_or_else(|| corrupt("payload chunk id"))?;
            let range = ranges
                .get(local as usize)
                .ok_or_else(|| corrupt("payload chunk id"))?;
            let bytes = raw
                .get(range.clone())
                .ok_or_else(|| corrupt("payload chunk id"))?;
            let next_total = total + bytes.len() as u64;
            if next_total > MAX_VALUE_BYTES {
                return Err(corrupt("payload size"));
            }
            total = next_total;
            scratch.extend_from_slice(bytes);
        }
        if utf8 {
            let text = std::str::from_utf8(&scratch).map_err(|_| corrupt("payload utf8"))?;
            strings.append_value(text);
        } else {
            binaries.append_value(&scratch);
        }
    }

    let array: ArrayRef = if utf8 {
        Arc::new(strings.finish())
    } else {
        Arc::new(binaries.finish())
    };
    Ok(array)
}
