//! Frame building and chunk dedup (S4 step 3). Unique chunks are appended to an open frame;
//! a frame is closed when it is full, and written as one `PayloadFrame` segment.

use std::collections::HashMap;
use std::ops::Range;

use xxhash_rust::xxh3::xxh3_128;

use super::{FRAME_INITIAL_RATIO, FRAME_MAX_EST, FRAME_MAX_RAW};
use crate::error::{Result, StrataError};
use crate::format::{Cursor, Out};
use crate::segment::{Codec, Kind, SegmentCodec, encode_segment};

/// One frame of the frame table: where it is, its raw size and the ids it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FrameEntry {
    /// Absolute file offset of the frame segment.
    pub offset: u64,
    /// Length of the frame segment, header included.
    pub len: u32,
    /// Length of the frame's raw body (count, end offsets and chunk bytes).
    pub raw_len: u32,
    /// Id of the frame's first chunk.
    pub first_chunk: u32,
    /// Number of chunks in the frame.
    pub n_chunks: u32,
}

/// Builds the frames of one payload column.
pub(crate) struct FrameBuilder {
    /// xxh3-128 of a chunk, to its id.
    ids: HashMap<u128, u32>,
    next_id: u32,
    /// Chunk bytes of the open frame.
    cur: Vec<u8>,
    /// End offsets of the open frame's chunks, within `cur`.
    cur_ends: Vec<u32>,
    /// Id of the open frame's first chunk.
    cur_first: u32,
    /// Running raw/stored ratio of the closed frames.
    ratio: f64,
    raw_total: u64,
    stored_total: u64,
    /// The closed frames, in file order.
    pub frames: Vec<FrameEntry>,
    /// Sum of the frame segments' lengths.
    pub stored_bytes: u64,
}

impl FrameBuilder {
    /// An empty builder.
    pub(crate) fn new() -> Self {
        Self {
            ids: HashMap::new(),
            next_id: 0,
            cur: Vec::new(),
            cur_ends: Vec::new(),
            cur_first: 0,
            ratio: FRAME_INITIAL_RATIO,
            raw_total: 0,
            stored_total: 0,
            frames: Vec::new(),
            stored_bytes: 0,
        }
    }

    /// Returns the chunk's id. A new chunk is appended to the open frame, which is closed when
    /// it is full.
    pub(crate) fn add(
        &mut self,
        chunk: &[u8],
        codec: &mut SegmentCodec,
        out: &mut Out,
    ) -> Result<u32> {
        let hash = xxh3_128(chunk);
        if let Some(id) = self.ids.get(&hash) {
            return Ok(*id);
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| StrataError::Unsupported {
                what: "more than 4 billion chunks".to_string(),
            })?;
        self.ids.insert(hash, id);
        self.cur.extend_from_slice(chunk);
        let end = u32::try_from(self.cur.len()).map_err(|_| StrataError::Unsupported {
            what: "a payload frame of more than 4 GiB".to_string(),
        })?;
        self.cur_ends.push(end);
        if self.cur.len() >= FRAME_MAX_RAW || self.cur.len() as f64 / self.ratio >= FRAME_MAX_EST {
            self.close(codec, out)?;
        }
        Ok(id)
    }

    /// Closes the open frame, if it holds any chunk.
    pub(crate) fn finish(&mut self, codec: &mut SegmentCodec, out: &mut Out) -> Result<()> {
        if self.cur_ends.is_empty() {
            return Ok(());
        }
        self.close(codec, out)
    }

    /// Number of unique chunks so far.
    pub(crate) fn n_chunks(&self) -> u32 {
        self.next_id
    }

    /// Writes the open frame as one segment and records it.
    fn close(&mut self, codec: &mut SegmentCodec, out: &mut Out) -> Result<()> {
        let n_chunks =
            u32::try_from(self.cur_ends.len()).map_err(|_| StrataError::Unsupported {
                what: "more than 4 billion chunks in a frame".to_string(),
            })?;
        let mut raw = Vec::with_capacity(4 + 4 * self.cur_ends.len() + self.cur.len());
        raw.extend_from_slice(&n_chunks.to_le_bytes());
        for end in &self.cur_ends {
            raw.extend_from_slice(&end.to_le_bytes());
        }
        raw.extend_from_slice(&self.cur);

        let seg = encode_segment(Kind::PayloadFrame, &raw, Codec::Zstd, codec, out)?;
        let raw_len = u32::try_from(raw.len()).map_err(|_| StrataError::Unsupported {
            what: "a payload frame of more than 4 GiB".to_string(),
        })?;
        self.frames.push(FrameEntry {
            offset: seg.offset,
            len: seg.len,
            raw_len,
            first_chunk: self.cur_first,
            n_chunks,
        });
        self.stored_bytes += u64::from(seg.len);
        self.raw_total += raw.len() as u64;
        self.stored_total += u64::from(seg.len).saturating_sub(16);
        if self.stored_total > 0 {
            self.ratio = self.raw_total as f64 / self.stored_total as f64;
        }
        self.cur.clear();
        self.cur_ends.clear();
        self.cur_first = self.next_id;
        Ok(())
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
    // The end offsets must fit in `raw` before anything is allocated for them.
    let data_start = n
        .checked_mul(4)
        .and_then(|v| v.checked_add(4))
        .ok_or_else(|| corrupt("payload frame chunks"))?;
    if data_start > raw.len() {
        return Err(corrupt("payload frame chunks"));
    }
    let mut ends = Vec::with_capacity(n);
    let mut prev = 0usize;
    for _ in 0..n {
        let end = c.u32("payload frame end")? as usize;
        if end < prev {
            return Err(corrupt("payload frame end"));
        }
        ends.push(end);
        prev = end;
    }
    let data_len = raw.len() - data_start;
    if ends.last().copied().unwrap_or(0) != data_len {
        return Err(corrupt("payload frame end"));
    }
    let mut ranges = Vec::with_capacity(n);
    let mut start = 0usize;
    for end in ends {
        ranges.push(data_start + start..data_start + end);
        start = end;
    }
    Ok(ranges)
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
    use crate::format::Out;

    fn codec() -> SegmentCodec {
        SegmentCodec::new(3).unwrap()
    }

    /// Seeded xorshift bytes: incompressible.
    fn noise(seed: u64, n: usize) -> Vec<u8> {
        let mut state = seed;
        (0..n)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn raw_data_size(f: &FrameEntry) -> usize {
        f.raw_len as usize - 4 - 4 * f.n_chunks as usize
    }

    fn check_contiguous(frames: &[FrameEntry]) {
        let mut next = 0u32;
        for f in frames {
            assert_eq!(f.first_chunk, next);
            next += f.n_chunks;
        }
    }

    #[test]
    fn same_chunk_twice_gets_the_same_id_and_is_stored_once() {
        let mut b = FrameBuilder::new();
        let (mut c, mut out) = (codec(), Out(Vec::new()));
        let first = b.add(b"hello chunk", &mut c, &mut out).unwrap();
        let again = b.add(b"hello chunk", &mut c, &mut out).unwrap();
        assert_eq!(first, again);
        b.finish(&mut c, &mut out).unwrap();
        assert_eq!(b.n_chunks(), 1);
        assert_eq!(b.frames.len(), 1);
        assert_eq!(b.frames[0].n_chunks, 1);
    }

    #[test]
    fn incompressible_chunks_close_frames_at_the_estimate() {
        let mut b = FrameBuilder::new();
        let (mut c, mut out) = (codec(), Out(Vec::new()));
        for k in 0..400u64 {
            let chunk = noise(0x9E37_79B9_7F4A_7C15 ^ (k + 1), 1024);
            let id = b.add(&chunk, &mut c, &mut out).unwrap();
            assert_eq!(u64::from(id), k, "ids are consecutive");
        }
        b.finish(&mut c, &mut out).unwrap();
        let frames = &b.frames;
        assert_eq!(frames[0].n_chunks, 64);
        assert_eq!(raw_data_size(&frames[0]), 64 * 1024);
        for f in &frames[1..frames.len() - 1] {
            let size = raw_data_size(f);
            assert!(
                (15 * 1024..=18 * 1024).contains(&size),
                "raw data size {size}"
            );
        }
        check_contiguous(frames);
        let total: u32 = frames.iter().map(|f| f.n_chunks).sum();
        assert_eq!(total, 400);
    }

    #[test]
    fn compressible_chunks_close_frames_at_the_raw_limit() {
        let mut b = FrameBuilder::new();
        let (mut c, mut out) = (codec(), Out(Vec::new()));
        for k in 0..400u32 {
            let mut chunk = format!("{k:08}").into_bytes();
            chunk.extend(std::iter::repeat_n(b'a', 1016));
            b.add(&chunk, &mut c, &mut out).unwrap();
        }
        b.finish(&mut c, &mut out).unwrap();
        let frames = &b.frames;
        for f in &frames[..frames.len() - 1] {
            let size = raw_data_size(f);
            assert!(
                (64 * 1024..65 * 1024).contains(&size),
                "raw data size {size}"
            );
        }
        check_contiguous(frames);
    }

    /// Builds the raw body of a frame by hand: count, end offsets, then the data.
    fn raw_frame(chunks: &[&[u8]]) -> Vec<u8> {
        let mut raw = (chunks.len() as u32).to_le_bytes().to_vec();
        let mut end = 0u32;
        for chunk in chunks {
            end += chunk.len() as u32;
            raw.extend_from_slice(&end.to_le_bytes());
        }
        for chunk in chunks {
            raw.extend_from_slice(chunk);
        }
        raw
    }

    #[test]
    fn decode_frame_round_trips() {
        let raw = raw_frame(&[b"ab", b"cde"]);
        let ranges = decode_frame(&raw, 0, 2).unwrap();
        assert_eq!(ranges.len(), 2);
        assert_eq!(&raw[ranges[0].clone()], b"ab");
        assert_eq!(&raw[ranges[1].clone()], b"cde");
    }

    #[test]
    fn decode_frame_rejects_bad_bodies() {
        let raw = raw_frame(&[b"ab", b"cde"]);
        // A wrong chunk count.
        assert!(matches!(
            decode_frame(&raw, 0, 3),
            Err(StrataError::Corrupt { .. })
        ));
        // Decreasing end offsets.
        let mut decreasing = raw.clone();
        decreasing[8..12].copy_from_slice(&1u32.to_le_bytes());
        assert!(matches!(
            decode_frame(&decreasing, 0, 2),
            Err(StrataError::Corrupt { .. })
        ));
        // Short data: the last end offset no longer matches.
        assert!(matches!(
            decode_frame(&raw[..raw.len() - 1], 0, 2),
            Err(StrataError::Corrupt { .. })
        ));
        // A huge count in a four-byte body is rejected before anything is allocated.
        assert!(matches!(
            decode_frame(&[0xff; 4], 0, u32::MAX),
            Err(StrataError::Corrupt { .. })
        ));
        assert!(matches!(
            decode_frame(&[0xff; 4], 0, 0xffff_fffe),
            Err(StrataError::Corrupt { .. })
        ));
    }
}
