//! Merging payload columns without decoding them (S10): the runs of the inputs' stores are
//! copied byte for byte (frames, index pages and dictionaries), chunk ids are shifted by the
//! chunks of the earlier inputs, and only the refs pages are written again, in the merged row
//! order.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::header::encode_header;
use super::read::{ids_of, read_header, refs_segments};
use super::store::{GramIndex, PayloadStore, Run};
use super::write::{PayloadStats, check_chunk_refs, write_refs};
use crate::error::{Result, StrataError};
use crate::file::StrataFile;
use crate::format::Out;
use crate::hot::Layer;
use crate::io::ScanKind;
use crate::segment::{Codec, Kind, SegmentCodec, SegmentRef, encode_segment};

/// The payload columns of a merge and where their rows come from.
pub(crate) struct MergeCopy<'a> {
    /// The inputs, none of them empty.
    pub inputs: &'a [Arc<StrataFile>],
    /// For each output row, its input and its row there.
    pub sources: &'a [(usize, usize)],
    /// The columns of each store, the same in every input.
    pub stores: Vec<Vec<usize>>,
}

/// One copied payload column: its store and header segments, statistics and null count.
pub(crate) struct CopiedColumn {
    pub column: usize,
    pub store: SegmentRef,
    pub header: SegmentRef,
    pub stats: PayloadStats,
    pub null_count: u64,
}

/// The stores of `inputs` when every input has the same payload columns grouped in the same
/// stores: the columns of each store, in column order. `None` when they differ, or when there is
/// no input.
pub(crate) fn same_stores(inputs: &[Arc<StrataFile>]) -> Option<Vec<Vec<usize>>> {
    let layout = |file: &StrataFile| -> Vec<Vec<usize>> {
        let mut stores: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
        for (column, record) in file.directory().iter().enumerate() {
            if record.layer == Layer::Payload {
                stores.entry(record.dict_offset).or_default().push(column);
            }
        }
        let mut out: Vec<Vec<usize>> = stores.into_values().collect();
        out.sort();
        out
    };
    let first = layout(inputs.first()?);
    inputs
        .iter()
        .skip(1)
        .all(|file| layout(file) == first)
        .then_some(first)
}

/// Fetches the raw bytes of `range` from `file` and appends them to `out`. Returns the shift to
/// add to an offset in the range to get its offset in `out`.
async fn copy_range(file: &StrataFile, range: std::ops::Range<u64>, out: &mut Out) -> Result<i64> {
    let bytes = file.fetch_raw(range.clone()).await?;
    let shift = out.len() as i64 - range.start as i64;
    out.bytes(&bytes);
    Ok(shift)
}

/// `offset` moved by `shift`.
fn shifted(offset: u64, shift: i64) -> Result<u64> {
    offset
        .checked_add_signed(shift)
        .ok_or(StrataError::Corrupt {
            what: "payload copy offset",
            offset,
        })
}

/// Copies the frames and index pages of `copy`'s stores into `out` and writes each column's
/// refs pages there; the stores and headers go to `meta`, with offsets relative to it.
pub(crate) async fn copy_stores(
    copy: &MergeCopy<'_>,
    codec: &mut SegmentCodec,
    out: &mut Out,
    meta: &mut Out,
) -> Result<Vec<CopiedColumn>> {
    let mut copied = Vec::new();
    for columns in &copy.stores {
        let Some(&first_column) = columns.first() else {
            continue;
        };
        let mut store = PayloadStore {
            n_chunks: 0,
            frames: Vec::new(),
            runs: Vec::new(),
        };
        let mut bases = Vec::with_capacity(copy.inputs.len());
        let (mut frame_bytes, mut index_bytes, mut dict_bytes) = (0u64, 0u64, 0u64);
        for file in copy.inputs {
            let input = read_header(file, first_column).await?.store.clone();
            let base = store.n_chunks;
            bases.push(base);
            let frame_base = u32::try_from(store.frames.len()).map_err(|_| too_many())?;
            if let (Some(first), Some(last)) = (input.frames.first(), input.frames.last()) {
                let end = last.offset + u64::from(last.len);
                frame_bytes += end - first.offset;
                let shift = copy_range(file, first.offset..end, out).await?;
                for frame in &input.frames {
                    let mut f = frame.clone();
                    f.offset = shifted(f.offset, shift)?;
                    f.first_chunk = f.first_chunk.checked_add(base).ok_or_else(too_many)?;
                    store.frames.push(f);
                }
            }
            for run in &input.runs {
                let index = match &run.index {
                    None => None,
                    Some(index) => {
                        let start = index.pages.iter().map(|p| p.offset).min();
                        let end = index
                            .pages
                            .iter()
                            .map(|p| p.offset + u64::from(p.len))
                            .max();
                        let pages = match (start, end) {
                            (Some(start), Some(end)) => {
                                index_bytes += end - start;
                                let shift = copy_range(file, start..end, out).await?;
                                index
                                    .pages
                                    .iter()
                                    .map(|p| {
                                        Ok(SegmentRef {
                                            offset: shifted(p.offset, shift)?,
                                            len: p.len,
                                        })
                                    })
                                    .collect::<Result<Vec<_>>>()?
                            }
                            _ => Vec::new(),
                        };
                        Some(GramIndex {
                            pages,
                            ..index.clone()
                        })
                    }
                };
                dict_bytes += run.dict.as_ref().map_or(0, |d| d.raw.len() as u64);
                store.runs.push(Run {
                    first_frame: run
                        .first_frame
                        .checked_add(frame_base)
                        .ok_or_else(too_many)?,
                    dict: run.dict.clone(),
                    index,
                });
            }
            store.n_chunks = store
                .n_chunks
                .checked_add(input.n_chunks)
                .ok_or_else(too_many)?;
        }
        let mut body = Out(Vec::new());
        store.encode(&mut body)?;
        let store_seg = encode_segment(Kind::PayloadStore, &body.0, Codec::Zstd, codec, meta)?;

        let n_rows = u32::try_from(copy.sources.len()).map_err(|_| StrataError::TooManyRows {
            rows: copy.sources.len() as u64,
        })?;
        for (c, &column) in columns.iter().enumerate() {
            // Every input's rows, then the merged order with shifted ids.
            let mut per_input: Vec<Vec<Option<Vec<u32>>>> = Vec::with_capacity(copy.inputs.len());
            let mut null_count = 0u64;
            for file in copy.inputs {
                let header = read_header(file, column).await?;
                let rows: Vec<u32> = (0..header.n_rows).collect();
                let (segments, pages) = refs_segments(&header, &rows)?;
                let bodies = file.fetch(&segments, ScanKind::Scan).await?;
                let record = file.directory().get(column).ok_or_else(too_many)?;
                null_count += record.null_count;
                per_input.push(ids_of(
                    &header,
                    &pages,
                    &bodies,
                    &rows,
                    record.payload_offset,
                )?);
            }
            let mut rows: Vec<Option<Vec<u32>>> = Vec::with_capacity(copy.sources.len());
            let mut chunk_refs = 0u64;
            for &(input, row) in copy.sources {
                let base = bases.get(input).copied().ok_or_else(too_many)?;
                let ids = per_input
                    .get_mut(input)
                    .and_then(|r| r.get_mut(row))
                    .ok_or(StrataError::Corrupt {
                        what: "merge row count",
                        offset: 0,
                    })?
                    .take();
                rows.push(match ids {
                    Some(mut ids) => {
                        for id in &mut ids {
                            *id = id.checked_add(base).ok_or_else(too_many)?;
                        }
                        chunk_refs += ids.len() as u64;
                        Some(ids)
                    }
                    None => None,
                });
            }
            check_chunk_refs(chunk_refs)?;
            drop(per_input);
            let (ref_pages, ref_bytes) = write_refs(&rows, codec, out)?;
            let mut header_body = Out(Vec::new());
            encode_header(n_rows, &ref_pages, &mut header_body)?;
            let header = encode_segment(
                Kind::PayloadHeader,
                &header_body.0,
                Codec::Zstd,
                codec,
                meta,
            )?;
            let shared = |v: u64| if c == 0 { v } else { 0 };
            copied.push(CopiedColumn {
                column,
                store: store_seg,
                header,
                stats: PayloadStats {
                    unique_chunks: store.n_chunks,
                    frames: u32::try_from(store.frames.len()).unwrap_or(u32::MAX),
                    chunk_refs,
                    bytes: shared(frame_bytes + index_bytes + u64::from(store_seg.len))
                        + ref_bytes
                        + u64::from(header.len),
                    frame_bytes: shared(frame_bytes),
                    ref_bytes,
                    index_bytes: shared(index_bytes),
                    store_bytes: shared(u64::from(store_seg.len)),
                    dict_bytes: shared(dict_bytes),
                },
                null_count,
            });
        }
    }
    Ok(copied)
}

fn too_many() -> StrataError {
    StrataError::Unsupported {
        what: "more than 4 billion payload chunks or frames".to_string(),
    }
}
