//! A payload store (S4): the unique chunks of one or more payload columns, packed into zstd
//! frames. The frames come in runs. A file write makes one run; a merge copies the runs of its
//! inputs as they are. Each run has its own zstd dictionary (when it is large enough) and its
//! own gram index ([`super::gram`]).
//!
//! The store segment body:
//! - `u32 n_chunks` · `u32 n_frames` · `u32 n_runs`;
//! - per run: `u32 first_frame` · `u32 dict_len` · the dictionary · `u32 index_rows` (0: no
//!   index) · `u32 index_frames` · `u32 rows_per_page` · `u32 n_pages` · per page `u64 offset`,
//!   `u32 len`;
//! - per frame: `u64 offset` · `u32 len` · `u32 raw_len` · `u32 n_chunks`.
//!
//! A frame's raw body is `u32 n` · `u32 end[n]` (end offsets of its chunks) · the chunk bytes.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::ops::Range;
use std::sync::Arc;

use xxhash_rust::xxh3::xxh3_128;
use zstd::dict::{DecoderDictionary, EncoderDictionary};

use super::gram::{
    GRAM, IndexBuilder, MAX_ROWS, WINDOW_HEAD, estimate_distinct, rows_for, rows_per_page, window,
};
use super::{DICT_MAX, DICT_MIN_RAW, DICT_SAMPLE, DICT_SAMPLE_BYTES, FRAME_MAX_RAW, StoreRole};
use crate::error::{Result, StrataError};
use crate::format::{Cursor, Out};
use crate::segment::{Codec, Kind, SegmentCodec, SegmentRef, encode_segment, write_segment};

/// One frame of the frame table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrameEntry {
    /// Absolute file offset of the frame segment.
    pub offset: u64,
    /// Length of the frame segment, header included.
    pub len: u32,
    /// Length of the frame's raw body.
    pub raw_len: u32,
    /// Id of the frame's first chunk.
    pub first_chunk: u32,
    /// Number of chunks in the frame.
    pub n_chunks: u32,
}

/// A zstd dictionary: its bytes, and the prepared decoder.
pub(crate) struct Dict {
    pub raw: Vec<u8>,
    pub decoder: DecoderDictionary<'static>,
}

impl Dict {
    fn new(raw: Vec<u8>) -> Self {
        let decoder = DecoderDictionary::copy(&raw);
        Self { raw, decoder }
    }
}

impl std::fmt::Debug for Dict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Dict({} bytes)", self.raw.len())
    }
}

impl PartialEq for Dict {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

/// The gram index of a run: `rows` rows of one bit per frame of the run, in pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GramIndex {
    pub rows: u32,
    pub frames: u32,
    pub rows_per_page: u32,
    pub pages: Vec<SegmentRef>,
}

impl GramIndex {
    /// Bytes of one row.
    pub(crate) fn row_bytes(&self) -> usize {
        (self.frames as usize).div_ceil(8)
    }

    /// The page that holds `row`, and the row's byte range in it.
    pub(crate) fn locate(&self, row: u32) -> (usize, Range<usize>) {
        let page = (row / self.rows_per_page) as usize;
        let at = (row % self.rows_per_page) as usize * self.row_bytes();
        (page, at..at + self.row_bytes())
    }

    /// The raw length page `page` must have.
    pub(crate) fn page_len(&self, page: usize) -> usize {
        let first = page as u64 * u64::from(self.rows_per_page);
        let rows = u64::from(self.rows)
            .saturating_sub(first)
            .min(u64::from(self.rows_per_page));
        rows as usize * self.row_bytes()
    }
}

/// One run of a store: its frames start at `first_frame` and end where the next run starts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Run {
    pub first_frame: u32,
    pub dict: Option<Arc<Dict>>,
    pub index: Option<GramIndex>,
}

/// A decoded payload store.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PayloadStore {
    pub n_chunks: u32,
    pub frames: Vec<FrameEntry>,
    pub runs: Vec<Run>,
}

/// Largest dictionary a store may hold.
const MAX_DICT_LEN: u32 = 1 << 20;

impl PayloadStore {
    /// Writes the store body.
    pub(crate) fn encode(&self, out: &mut Out) -> Result<()> {
        let count = |n: usize| {
            u32::try_from(n).map_err(|_| StrataError::Unsupported {
                what: "more than 4 billion payload entries".to_string(),
            })
        };
        out.u32(self.n_chunks);
        out.u32(count(self.frames.len())?);
        out.u32(count(self.runs.len())?);
        for run in &self.runs {
            out.u32(run.first_frame);
            let dict = run.dict.as_ref().map_or(&[][..], |d| d.raw.as_slice());
            out.u32(count(dict.len())?);
            out.bytes(dict);
            match &run.index {
                None => {
                    out.u32(0);
                    out.u32(0);
                    out.u32(0);
                    out.u32(0);
                }
                Some(index) => {
                    out.u32(index.rows);
                    out.u32(index.frames);
                    out.u32(index.rows_per_page);
                    out.u32(count(index.pages.len())?);
                    for page in &index.pages {
                        out.u64(page.offset);
                        out.u32(page.len);
                    }
                }
            }
        }
        for frame in &self.frames {
            out.u64(frame.offset);
            out.u32(frame.len);
            out.u32(frame.raw_len);
            out.u32(frame.n_chunks);
        }
        Ok(())
    }

    /// Decodes and checks a store body. `offset` is the store segment's file offset.
    pub(crate) fn decode(bytes: &[u8], offset: u64) -> Result<Self> {
        let corrupt = || StrataError::Corrupt {
            what: "payload store",
            offset,
        };
        let mut c = Cursor::new(bytes, offset);
        let n_chunks = c.u32("payload store")?;
        let n_frames = c.u32("payload store")?;
        let n_runs = c.u32("payload store")?;
        // Every run and frame takes at least 20 bytes: checked before allocating.
        if (n_runs as usize).saturating_add(n_frames as usize) > bytes.len() / 20 {
            return Err(corrupt());
        }
        if (n_runs == 0) != (n_frames == 0) {
            return Err(corrupt());
        }
        let mut runs: Vec<Run> = Vec::with_capacity(n_runs as usize);
        for _ in 0..n_runs {
            let first_frame = c.u32("payload store")?;
            let valid_start = match runs.last() {
                None => first_frame == 0,
                Some(prev) => first_frame > prev.first_frame,
            };
            if !valid_start || first_frame >= n_frames {
                return Err(corrupt());
            }
            let dict_len = c.u32("payload store")?;
            if dict_len > MAX_DICT_LEN {
                return Err(corrupt());
            }
            let dict = match dict_len {
                0 => None,
                n => Some(Arc::new(Dict::new(
                    c.bytes(n as usize, "payload store")?.to_vec(),
                ))),
            };
            let rows = c.u32("payload store")?;
            let frames = c.u32("payload store")?;
            let rpp = c.u32("payload store")?;
            let n_pages = c.u32("payload store")?;
            let index = if rows == 0 {
                if frames != 0 || rpp != 0 || n_pages != 0 {
                    return Err(corrupt());
                }
                None
            } else {
                // The run's frames: up to the next run's first frame, which is read later, so
                // the frame count is checked once every run is known.
                if rows > MAX_ROWS || frames == 0 || rpp != rows_per_page(frames as usize) {
                    return Err(corrupt());
                }
                if n_pages != rows.div_ceil(rpp) {
                    return Err(corrupt());
                }
                let mut pages = Vec::with_capacity(n_pages as usize);
                for _ in 0..n_pages {
                    pages.push(SegmentRef {
                        offset: c.u64("payload store")?,
                        len: c.u32("payload store")?,
                    });
                }
                Some(GramIndex {
                    rows,
                    frames,
                    rows_per_page: rpp,
                    pages,
                })
            };
            runs.push(Run {
                first_frame,
                dict,
                index,
            });
        }
        let mut frames = Vec::with_capacity(n_frames as usize);
        let mut first_chunk = 0u32;
        let mut end = 0u64;
        for _ in 0..n_frames {
            let frame_offset = c.u64("payload store")?;
            let len = c.u32("payload store")?;
            let raw_len = c.u32("payload store")?;
            let n = c.u32("payload store")?;
            if n == 0 || frame_offset < end {
                return Err(corrupt());
            }
            end = frame_offset
                .checked_add(u64::from(len))
                .ok_or_else(corrupt)?;
            frames.push(FrameEntry {
                offset: frame_offset,
                len,
                raw_len,
                first_chunk,
                n_chunks: n,
            });
            first_chunk = first_chunk.checked_add(n).ok_or_else(corrupt)?;
        }
        if first_chunk != n_chunks || c.remaining() != 0 {
            return Err(corrupt());
        }
        let store = PayloadStore {
            n_chunks,
            frames,
            runs,
        };
        for r in 0..store.runs.len() {
            if let Some(Run {
                index: Some(index), ..
            }) = store.runs.get(r)
            {
                let n = store.run_frames(r).len();
                if index.frames as usize != n {
                    return Err(corrupt());
                }
            }
        }
        Ok(store)
    }

    /// The frame indexes of run `run`.
    pub(crate) fn run_frames(&self, run: usize) -> Range<usize> {
        let start = self.runs.get(run).map_or(0, |r| r.first_frame as usize);
        let end = self
            .runs
            .get(run + 1)
            .map_or(self.frames.len(), |r| r.first_frame as usize);
        start..end
    }

    /// The run that holds frame `frame`.
    pub(crate) fn run_of(&self, frame: usize) -> usize {
        self.runs
            .partition_point(|r| r.first_frame as usize <= frame)
            .saturating_sub(1)
    }

    /// The index of the frame that holds chunk `id`, or `None` when no frame holds it.
    pub(crate) fn frame_of(&self, id: u32) -> Option<usize> {
        let f = self
            .frames
            .partition_point(|frame| frame.first_chunk <= id)
            .checked_sub(1)?;
        let entry = self.frames.get(f)?;
        (id.checked_sub(entry.first_chunk)? < entry.n_chunks).then_some(f)
    }

    /// The dictionary to decode the segment at file offset `offset` with: that of the run
    /// whose frame starts there.
    pub(crate) fn dict_at(&self, offset: u64) -> Option<&Arc<Dict>> {
        let f = self.frames.partition_point(|frame| frame.offset < offset);
        let frame = self.frames.get(f)?;
        if frame.offset != offset {
            return None;
        }
        self.runs.get(self.run_of(f))?.dict.as_ref()
    }

    /// The segment of frame `frame`.
    pub(crate) fn frame_segment(&self, frame: usize) -> Option<SegmentRef> {
        self.frames.get(frame).map(|e| SegmentRef {
            offset: e.offset,
            len: e.len,
        })
    }
}

/// Decodes the raw body of a frame into the byte ranges of its chunks. The ranges index into
/// `raw` itself. `offset` is the frame's file offset, used in errors.
pub(crate) fn decode_frame(
    raw: &[u8],
    offset: u64,
    expect_chunks: u32,
) -> Result<Vec<Range<usize>>> {
    let corrupt = |what: &'static str| StrataError::Corrupt { what, offset };
    let mut c = Cursor::new(raw, offset);
    let n = c.u32("payload frame chunks")?;
    if n != expect_chunks {
        return Err(corrupt("payload frame chunks"));
    }
    let n = n as usize;
    let data_start = n
        .checked_mul(4)
        .and_then(|v| v.checked_add(4))
        .ok_or_else(|| corrupt("payload frame chunks"))?;
    if data_start > raw.len() {
        return Err(corrupt("payload frame chunks"));
    }
    let mut ranges = Vec::with_capacity(n);
    let mut prev = 0usize;
    for _ in 0..n {
        let end = c.u32("payload frame end")? as usize;
        if end < prev {
            return Err(corrupt("payload frame end"));
        }
        ranges.push(data_start + prev..data_start + end);
        prev = end;
    }
    if prev != raw.len() - data_start {
        return Err(corrupt("payload frame end"));
    }
    Ok(ranges)
}

/// Checks a fetched frame body against its entry, then decodes its chunk ranges.
pub(crate) fn decode_fetched_frame(
    entry: &FrameEntry,
    raw: Vec<u8>,
) -> Result<(Vec<u8>, Vec<Range<usize>>)> {
    if usize::try_from(entry.raw_len).ok() != Some(raw.len()) {
        return Err(StrataError::Corrupt {
            what: "payload frame length",
            offset: entry.offset,
        });
    }
    let ranges = decode_frame(&raw, entry.offset, entry.n_chunks)?;
    Ok((raw, ranges))
}

/// A hasher for keys that are already uniform hashes: it keeps their low 64 bits.
#[derive(Default)]
pub(crate) struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }

    fn write_u64(&mut self, v: u64) {
        self.0 = v.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }

    fn write_u128(&mut self, v: u128) {
        self.0 = v as u64;
    }
}

pub(crate) type KeyBuild = BuildHasherDefault<KeyHasher>;

/// What writing a store wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StoreStats {
    pub unique_chunks: u32,
    pub frames: u32,
    /// Bytes of the frame segments.
    pub frame_bytes: u64,
    /// Bytes of the index pages.
    pub index_bytes: u64,
    /// Bytes of the store segment (dictionary included).
    pub store_bytes: u64,
    /// Bytes of the dictionary, 0 for none.
    pub dict_bytes: u64,
}

/// Collects the unique chunks of a store as rows are chunked, then writes it.
#[derive(Default)]
pub(crate) struct StoreBuilder {
    ids: HashMap<u128, u32, KeyBuild>,
    data: Vec<u8>,
    /// End of each chunk in `data`, by id.
    ends: Vec<usize>,
    /// Pairs `(left << 32) | right` of chunks that follow each other in some value, when
    /// `right` is not `left + 1` (those are indexed for every chunk).
    pairs: HashSet<u64, KeyBuild>,
}

impl StoreBuilder {
    /// A builder whose chunk bytes have room for `bytes` without growing: growing a buffer of
    /// hundreds of MiB copies it each time. The unique chunks never exceed the values' bytes,
    /// and pages of the reserve that stay unused are never touched.
    pub(crate) fn with_capacity(bytes: usize) -> Self {
        Self {
            data: Vec::with_capacity(bytes),
            ..Self::default()
        }
    }

    /// Returns the id of `chunk`, storing it when it is new.
    pub(crate) fn add(&mut self, chunk: &[u8]) -> Result<u32> {
        let hash = xxh3_128(chunk);
        if let Some(&id) = self.ids.get(&hash) {
            return Ok(id);
        }
        let id = u32::try_from(self.ends.len()).map_err(|_| StrataError::Unsupported {
            what: "more than 4 billion chunks".to_string(),
        })?;
        self.ids.insert(hash, id);
        self.data.extend_from_slice(chunk);
        self.ends.push(self.data.len());
        Ok(id)
    }

    /// Records that chunk `right` follows chunk `left` in some value.
    pub(crate) fn pair(&mut self, left: u32, right: u32) {
        if Some(right) != left.checked_add(1) {
            self.pairs
                .insert((u64::from(left) << 32) | u64::from(right));
        }
    }

    /// The bytes of chunk `id`.
    fn chunk(&self, id: usize) -> &[u8] {
        let start = if id == 0 {
            0
        } else {
            self.ends.get(id - 1).copied().unwrap_or(0)
        };
        let end = self.ends.get(id).copied().unwrap_or(start);
        self.data.get(start..end).unwrap_or_default()
    }

    /// Number of unique chunks so far.
    pub(crate) fn n_chunks(&self) -> u32 {
        u32::try_from(self.ends.len()).unwrap_or(u32::MAX)
    }

    /// Writes the frames, the index pages and the store segment, in that order. Returns the
    /// store segment and what was written. A store of at least `DICT_MIN_RAW` bytes trains a
    /// dictionary of its own: one trained on another store's text fits it badly, and is slower.
    pub(crate) fn write(
        self,
        role: StoreRole,
        codec: &mut SegmentCodec,
        out: &mut Out,
    ) -> Result<(SegmentRef, StoreStats)> {
        let n = self.ends.len();
        // Frames: consecutive chunks up to `FRAME_MAX_RAW` raw bytes.
        let mut frame_chunks: Vec<Range<usize>> = Vec::new();
        let mut start = 0usize;
        let mut start_byte = 0usize;
        for (id, &end) in self.ends.iter().enumerate() {
            if end - start_byte >= FRAME_MAX_RAW || id + 1 == n {
                frame_chunks.push(start..id + 1);
                start = id + 1;
                start_byte = end;
            }
        }
        let byte_range = |r: &Range<usize>| -> Range<usize> {
            let from = if r.start == 0 {
                0
            } else {
                self.ends.get(r.start - 1).copied().unwrap_or(0)
            };
            let to = r
                .end
                .checked_sub(1)
                .and_then(|i| self.ends.get(i))
                .copied()
                .unwrap_or(from);
            from..to
        };

        // The dictionary, trained on the first `DICT_SAMPLE` bytes of evenly spread frames.
        let dict = if role.trained() && self.data.len() >= DICT_MIN_RAW {
            let step = (frame_chunks.len() * DICT_SAMPLE / DICT_SAMPLE_BYTES).max(1);
            let samples: Vec<&[u8]> = frame_chunks
                .iter()
                .step_by(step)
                .filter_map(|r| {
                    let b = byte_range(r);
                    self.data.get(b.start..b.end.min(b.start + DICT_SAMPLE))
                })
                .collect();
            zstd::dict::from_samples(&samples, DICT_MAX)
                .ok()
                .map(|raw| Arc::new(Dict::new(raw)))
        } else {
            None
        };

        let zstd_error = |source| StrataError::Io {
            action: "zstd init",
            source,
        };
        let encoder = dict
            .as_ref()
            .map(|d| EncoderDictionary::copy(&d.raw, role.level()));
        let mut compressor = match &encoder {
            Some(e) => {
                let mut c =
                    zstd::bulk::Compressor::with_prepared_dictionary(e).map_err(zstd_error)?;
                // Attach the dictionary's tables instead of copying them into every frame.
                c.set_parameter(zstd::zstd_safe::CParameter::ForceAttachDict(
                    zstd::zstd_safe::DictAttachPref::ForceAttach,
                ))
                .map_err(zstd_error)?;
                c
            }
            None => zstd::bulk::Compressor::new(role.level()).map_err(zstd_error)?,
        };
        let packed_codec = if dict.is_some() {
            Codec::ZstdDict
        } else {
            Codec::Zstd
        };

        let mut stats = StoreStats {
            unique_chunks: self.n_chunks(),
            frames: u32::try_from(frame_chunks.len()).unwrap_or(u32::MAX),
            dict_bytes: dict.as_ref().map_or(0, |d| d.raw.len() as u64),
            ..StoreStats::default()
        };
        let mut frames = Vec::with_capacity(frame_chunks.len());
        let mut raw = Vec::new();
        for r in &frame_chunks {
            let bytes = byte_range(r);
            raw.clear();
            raw.extend_from_slice(&(r.len() as u32).to_le_bytes());
            for id in r.clone() {
                let end = self.ends.get(id).copied().unwrap_or(0) - bytes.start;
                raw.extend_from_slice(&(end as u32).to_le_bytes());
            }
            raw.extend_from_slice(self.data.get(bytes).unwrap_or_default());
            let packed = compressor
                .compress(&raw)
                .map_err(|source| StrataError::Io {
                    action: "zstd compress",
                    source,
                })?;
            let seg = if packed.len() < raw.len() {
                write_segment(Kind::PayloadFrame, packed_codec, raw.len(), &packed, out)?
            } else {
                write_segment(Kind::PayloadFrame, Codec::Raw, raw.len(), &raw, out)?
            };
            stats.frame_bytes += u64::from(seg.len);
            frames.push(FrameEntry {
                offset: seg.offset,
                len: seg.len,
                raw_len: u32::try_from(raw.len()).map_err(|_| StrataError::Unsupported {
                    what: "a payload frame of more than 4 GiB".to_string(),
                })?,
                first_chunk: r.start as u32,
                n_chunks: r.len() as u32,
            });
        }

        // The gram index: the row count from the distinct grams of up to three frames.
        let index = if frame_chunks.is_empty() {
            None
        } else {
            let n_frames = frame_chunks.len();
            let frame_bytes = |f: usize| -> Vec<&[u8]> {
                frame_chunks
                    .get(f)
                    .map(|r| r.clone().map(|id| self.chunk(id)).collect())
                    .unwrap_or_default()
            };
            let probes: Vec<usize> = {
                let mut p = vec![0, n_frames / 2, n_frames - 1];
                p.dedup();
                p
            };
            let distinct = probes
                .iter()
                .map(|&f| estimate_distinct(frame_bytes(f).into_iter()))
                .sum::<f64>()
                / probes.len() as f64;
            let mut builder = IndexBuilder::new(rows_for(distinct), n_frames);
            let mut w = Vec::new();
            for (f, r) in frame_chunks.iter().enumerate() {
                for id in r.clone() {
                    builder.add(f, self.chunk(id));
                    if id + 1 < n {
                        // When the next chunk is in the same frame, its head grams are there
                        // already: only the grams that cross the boundary are added.
                        let head = if id + 1 < r.end {
                            GRAM - 1
                        } else {
                            WINDOW_HEAD
                        };
                        window(self.chunk(id), self.chunk(id + 1), head, &mut w);
                        builder.add(f, &w);
                    }
                }
            }
            let frame_of_chunk =
                |id: usize| -> usize { frame_chunks.partition_point(|r| r.end <= id) };
            for &pair in &self.pairs {
                let (left, right) = ((pair >> 32) as usize, (pair & 0xFFFF_FFFF) as usize);
                window(self.chunk(left), self.chunk(right), WINDOW_HEAD, &mut w);
                builder.add_scattered(frame_of_chunk(left), &w);
            }
            let mut pages = Vec::new();
            for page in builder.pages() {
                let seg = write_segment(Kind::PayloadIndex, Codec::Raw, page.len(), &page, out)?;
                stats.index_bytes += u64::from(seg.len);
                pages.push(seg);
            }
            Some(GramIndex {
                rows: builder.rows(),
                frames: u32::try_from(n_frames).unwrap_or(u32::MAX),
                rows_per_page: builder.rows_per_page(),
                pages,
            })
        };

        let store = PayloadStore {
            n_chunks: self.n_chunks(),
            frames,
            runs: if frame_chunks.is_empty() {
                Vec::new()
            } else {
                vec![Run {
                    first_frame: 0,
                    dict,
                    index,
                }]
            },
        };
        let mut body = Out(Vec::new());
        store.encode(&mut body)?;
        let seg = encode_segment(Kind::PayloadStore, &body.0, Codec::Zstd, codec, out)?;
        stats.store_bytes = u64::from(seg.len);
        Ok((seg, stats))
    }
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
    use crate::segment::decode_segment_with;

    fn noise(seed: u64, n: usize) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2).wrapping_add(1);
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                b'a' + (x % 26) as u8
            })
            .collect()
    }

    /// Writes a store of `chunks` and returns the output, the decoded store and the stats.
    fn written(chunks: &[Vec<u8>]) -> (Vec<u8>, PayloadStore, StoreStats) {
        let mut b = StoreBuilder::default();
        for c in chunks {
            b.add(c).unwrap();
        }
        let mut out = Out(Vec::new());
        let mut codec = SegmentCodec::new(3).unwrap();
        let (seg, stats) = b.write(StoreRole::Shared, &mut codec, &mut out).unwrap();
        let bytes = &out.0[seg.offset as usize..(seg.offset + u64::from(seg.len)) as usize];
        let body = crate::segment::decode_segment(bytes, seg.offset, Kind::PayloadStore).unwrap();
        let store = PayloadStore::decode(&body, seg.offset).unwrap();
        (out.0, store, stats)
    }

    /// The chunks of every frame of `store`, decoded from `file`.
    fn chunks_back(file: &[u8], store: &PayloadStore) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for (f, e) in store.frames.iter().enumerate() {
            let seg = &file[e.offset as usize..(e.offset + u64::from(e.len)) as usize];
            let dict = store.runs[store.run_of(f)]
                .dict
                .as_ref()
                .map(|d| &d.decoder);
            let raw = decode_segment_with(seg, e.offset, Kind::PayloadFrame, dict).unwrap();
            let (raw, ranges) = decode_fetched_frame(e, raw).unwrap();
            out.extend(ranges.into_iter().map(|r| raw[r].to_vec()));
        }
        out
    }

    #[test]
    fn same_chunk_twice_gets_the_same_id_and_is_stored_once() {
        let mut b = StoreBuilder::default();
        let a = b.add(b"hello world").unwrap();
        let c = b.add(b"other").unwrap();
        assert_eq!(b.add(b"hello world").unwrap(), a);
        assert_ne!(a, c);
        assert_eq!(b.n_chunks(), 2);
    }

    #[test]
    fn small_stores_round_trip_without_a_dictionary() {
        let chunks: Vec<Vec<u8>> = (0..50).map(|i| noise(i, 3_000)).collect();
        let (file, store, stats) = written(&chunks);
        assert_eq!(stats.dict_bytes, 0);
        assert!(store.frames.len() >= 2);
        assert_eq!(chunks_back(&file, &store), chunks);
        // Frames close at the raw limit.
        assert!(
            store
                .frames
                .iter()
                .all(|f| f.raw_len as usize <= FRAME_MAX_RAW + 3_000 + 1_000)
        );
    }

    #[test]
    fn large_stores_get_a_dictionary_and_round_trip() {
        // About 6 MiB of JSON-like chunks sharing structure.
        let chunks: Vec<Vec<u8>> = (0..3_000)
            .map(|i| {
                format!(
                    "{{\"role\":\"assistant\",\"content\":\"{}\",\"id\":{i}}}",
                    String::from_utf8(noise(i, 2_000)).unwrap()
                )
                .into_bytes()
            })
            .collect();
        let (file, store, stats) = written(&chunks);
        assert!(stats.dict_bytes > 0, "{stats:?}");
        assert!(store.runs[0].dict.is_some());
        assert_eq!(chunks_back(&file, &store), chunks);
        let index = store.runs[0].index.as_ref().unwrap();
        assert_eq!(index.frames as usize, store.frames.len());
        assert_eq!(
            index.pages.len(),
            index.rows.div_ceil(index.rows_per_page) as usize
        );
    }

    #[test]
    fn empty_stores_have_no_runs() {
        let (_, store, stats) = written(&[]);
        assert!(store.runs.is_empty() && store.frames.is_empty());
        assert_eq!(stats.frames, 0);
    }

    #[test]
    fn corrupt_store_bodies_are_errors() {
        let chunks: Vec<Vec<u8>> = (0..30).map(|i| noise(i, 5_000)).collect();
        let (_, store, _) = written(&chunks);
        let mut body = Out(Vec::new());
        store.encode(&mut body).unwrap();
        let body = body.0;
        assert_eq!(PayloadStore::decode(&body, 0).unwrap(), store);
        // Truncated, extended, and a wrong chunk count.
        assert!(PayloadStore::decode(&body[..body.len() - 1], 0).is_err());
        let mut longer = body.clone();
        longer.push(0);
        assert!(PayloadStore::decode(&longer, 0).is_err());
        let mut wrong = body.clone();
        wrong[0] ^= 1;
        assert!(PayloadStore::decode(&wrong, 0).is_err());
        for len in 0..body.len().min(64) {
            let _ = PayloadStore::decode(&body[..len], 0);
        }
    }

    #[test]
    fn frame_of_and_dict_at_find_the_frame() {
        let chunks: Vec<Vec<u8>> = (0..40).map(|i| noise(i, 4_000)).collect();
        let (_, store, _) = written(&chunks);
        for id in 0..40u32 {
            let f = store.frame_of(id).unwrap();
            let e = &store.frames[f];
            assert!(e.first_chunk <= id && id < e.first_chunk + e.n_chunks);
        }
        assert_eq!(store.frame_of(40), None);
        assert!(store.dict_at(store.frames[0].offset + 1).is_none());
    }

    #[test]
    fn decode_frame_rejects_bad_bodies() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&2u32.to_le_bytes());
        raw.extend_from_slice(&3u32.to_le_bytes());
        raw.extend_from_slice(&5u32.to_le_bytes());
        raw.extend_from_slice(b"abcde");
        assert_eq!(decode_frame(&raw, 0, 2).unwrap(), vec![12..15, 15..17]);
        assert!(decode_frame(&raw, 0, 3).is_err());
        assert!(decode_frame(&raw[..raw.len() - 1], 0, 2).is_err());
        let mut bad = raw.clone();
        bad[4] = 9;
        assert!(decode_frame(&bad, 0, 2).is_err());
    }
}
