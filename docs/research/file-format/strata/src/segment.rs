//! Segments: the framed, checksummed unit that every stored body lives in (S1).
//!
//! A segment is a 16-byte header (`u16` magic, `u8` kind, `u8` codec,
//! `u32` raw length, `u32` stored length, `u32` checksum) followed by the stored bytes.

use std::cell::RefCell;

use crate::error::{Result, StrataError};
use crate::format::{Cursor, MAX_SEGMENT_RAW, Out, SEGMENT_HEADER_LEN, SEGMENT_MAGIC, checksum32};

thread_local! {
    /// The decompressor of this thread, created on first use and reused.
    static DECOMPRESSOR: RefCell<Option<zstd::bulk::Decompressor<'static>>> =
        const { RefCell::new(None) };
}

/// A zstd compressor at one level, reused for every segment of a file.
pub(crate) struct SegmentCodec {
    compressor: zstd::bulk::Compressor<'static>,
}

impl SegmentCodec {
    /// A codec that compresses at `level`.
    pub(crate) fn new(level: i32) -> Result<Self> {
        let compressor = zstd::bulk::Compressor::new(level).map_err(|source| StrataError::Io {
            action: "zstd init",
            source,
        })?;
        Ok(Self { compressor })
    }
}

/// What a segment holds (S1 table).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A narrow column chunk.
    NarrowChunk = 1,
    /// A payload frame.
    PayloadFrame = 2,
    /// A payload refs page.
    PayloadRefs = 3,
    /// A payload header (frame table, refs page table).
    PayloadHeader = 4,
    /// Payload filters.
    PayloadFilters = 5,
    /// A payload edge group.
    PayloadEdges = 6,
    /// A column block-table page.
    BlockTablePage = 7,
    /// A dictionary.
    Dictionary = 8,
    /// A file key filter.
    KeyFilter = 9,
}

impl Kind {
    /// The kind for a stored byte, or `None` if the value is unknown.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::NarrowChunk),
            2 => Some(Self::PayloadFrame),
            3 => Some(Self::PayloadRefs),
            4 => Some(Self::PayloadHeader),
            5 => Some(Self::PayloadFilters),
            6 => Some(Self::PayloadEdges),
            7 => Some(Self::BlockTablePage),
            8 => Some(Self::Dictionary),
            9 => Some(Self::KeyFilter),
            _ => None,
        }
    }
}

/// How a segment body is stored.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// Stored as is.
    Raw = 0,
    /// Compressed with zstd.
    Zstd = 1,
}

impl Codec {
    /// The codec for a stored byte, or `None` if the value is unknown.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Raw),
            1 => Some(Self::Zstd),
            _ => None,
        }
    }
}

/// Where a segment lives in the file: its header offset and its total length
/// (header plus stored bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentRef {
    /// Absolute file offset of the segment header.
    pub offset: u64,
    /// Header plus stored bytes, in bytes.
    pub len: u32,
}

/// Appends one segment to `out` and returns its reference.
///
/// The body is compressed with `codec` when `prefer` is [`Codec::Zstd`] and the result is
/// strictly smaller; otherwise it is stored raw.
pub(crate) fn encode_segment(
    kind: Kind,
    body: &[u8],
    prefer: Codec,
    codec: &mut SegmentCodec,
    out: &mut Out,
) -> Result<SegmentRef> {
    if body.len() > MAX_SEGMENT_RAW as usize {
        return Err(StrataError::Unsupported {
            what: format!("segment body of {} bytes", body.len()),
        });
    }
    let compressed = match prefer {
        Codec::Zstd => codec
            .compressor
            .compress(body)
            .ok()
            .filter(|c| c.len() < body.len()),
        Codec::Raw => None,
    };
    let (codec, stored) = match &compressed {
        Some(c) => (Codec::Zstd, c.as_slice()),
        None => (Codec::Raw, body),
    };
    let too_big = |n: usize| StrataError::Unsupported {
        what: format!("segment body of {n} bytes"),
    };
    let raw_len = u32::try_from(body.len()).map_err(|_| too_big(body.len()))?;
    let stored_len = u32::try_from(stored.len()).map_err(|_| too_big(stored.len()))?;
    let len = stored_len
        .checked_add(SEGMENT_HEADER_LEN as u32)
        .ok_or_else(|| too_big(stored.len()))?;

    let mut prefix = Out(Vec::with_capacity(12));
    prefix.u16(SEGMENT_MAGIC);
    prefix.u8(kind as u8);
    prefix.u8(codec as u8);
    prefix.u32(raw_len);
    prefix.u32(stored_len);
    let checksum = checksum32(&[prefix.0.as_slice(), stored]);

    let offset = out.len() as u64;
    out.bytes(&prefix.0);
    out.u32(checksum);
    out.bytes(stored);
    Ok(SegmentRef { offset, len })
}

/// Decompresses one zstd frame with this thread's reused decompressor. When the
/// decompressor is busy (a re-entrant call) or cannot be created, it decompresses with a
/// fresh one; the output is the same either way.
fn decompress(stored: &[u8], raw_len: usize) -> std::io::Result<Vec<u8>> {
    DECOMPRESSOR.with(|cell| match cell.try_borrow_mut() {
        Ok(mut slot) => {
            if slot.is_none() {
                *slot = zstd::bulk::Decompressor::new().ok();
            }
            match slot.as_mut() {
                Some(decompressor) => decompressor.decompress(stored, raw_len),
                None => zstd::bulk::decompress(stored, raw_len),
            }
        }
        Err(_) => zstd::bulk::decompress(stored, raw_len),
    })
}

/// Checks and decodes the segment whose `bytes` (exactly its `len` bytes) start at
/// file offset `offset`, and returns the raw body.
pub fn decode_segment(bytes: &[u8], offset: u64, expect: Kind) -> Result<Vec<u8>> {
    let corrupt = |what: &'static str| StrataError::Corrupt { what, offset };
    let mut c = Cursor::new(bytes, offset);
    if c.u16("segment length")? != SEGMENT_MAGIC {
        return Err(corrupt("segment magic"));
    }
    if Kind::from_u8(c.u8("segment length")?) != Some(expect) {
        return Err(corrupt("segment kind"));
    }
    let codec = Codec::from_u8(c.u8("segment length")?).ok_or_else(|| corrupt("segment codec"))?;
    let raw_len = c.u32("segment length")?;
    let stored_len = c.u32("segment length")?;
    let sum = c.u32("segment length")?;
    // The raw length is capped before anything is allocated for it.
    if raw_len > MAX_SEGMENT_RAW {
        return Err(corrupt("segment raw length"));
    }
    let stored_len = stored_len as usize;
    if stored_len.checked_add(SEGMENT_HEADER_LEN) != Some(bytes.len()) {
        return Err(corrupt("segment length"));
    }
    if codec == Codec::Raw && raw_len as usize != stored_len {
        return Err(corrupt("segment raw length"));
    }
    let prefix = bytes.get(..12).ok_or_else(|| corrupt("segment length"))?;
    let stored = c.bytes(stored_len, "segment length")?;
    if checksum32(&[prefix, stored]) != sum {
        return Err(corrupt("segment checksum"));
    }
    match codec {
        Codec::Raw => Ok(stored.to_vec()),
        Codec::Zstd => {
            let body =
                decompress(stored, raw_len as usize).map_err(|_| corrupt("segment decompress"))?;
            if body.len() != raw_len as usize {
                return Err(corrupt("segment decompress"));
            }
            Ok(body)
        }
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
    use proptest::prelude::*;

    fn encode(kind: Kind, body: &[u8], prefer: Codec, prefix: usize) -> (Vec<u8>, SegmentRef) {
        let mut out = Out(vec![0; prefix]);
        let mut codec = SegmentCodec::new(1).unwrap();
        let seg = encode_segment(kind, body, prefer, &mut codec, &mut out).unwrap();
        (out.0, seg)
    }

    fn corrupt_what(err: StrataError) -> &'static str {
        match err {
            StrataError::Corrupt { what, .. } => what,
            other => panic!("expected Corrupt, got {other}"),
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1000))]

        #[test]
        fn round_trip(
            body in proptest::collection::vec(any::<u8>(), 0..70_000),
            prefix in 0usize..100,
        ) {
            for prefer in [Codec::Zstd, Codec::Raw] {
                let (file, seg) = encode(Kind::NarrowChunk, &body, prefer, prefix);
                prop_assert_eq!(seg.offset, prefix as u64);
                let start = prefix;
                let end = prefix + seg.len as usize;
                prop_assert_eq!(end, file.len());
                let decoded = decode_segment(&file[start..end], seg.offset, Kind::NarrowChunk).unwrap();
                prop_assert_eq!(decoded, body.clone());
            }
        }

        #[test]
        fn never_panics_on_random_bytes(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = decode_segment(&bytes, 0, Kind::NarrowChunk);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(500))]

        #[test]
        fn random_headers_with_correct_checksums_never_panic(
            kind in 1u8..=9,
            codec in 0u8..=1,
            huge in 0u8..10,
            small_raw in 0u32..=(1 << 20),
            huge_raw in (MAX_SEGMENT_RAW + 1)..=u32::MAX,
            stored in proptest::collection::vec(any::<u8>(), 0..2048),
        ) {
            // One case in ten has a raw length above the cap; the others stay small so that
            // zstd's reservation stays small.
            let raw_len = if huge == 0 { huge_raw } else { small_raw };
            let mut bytes = Vec::with_capacity(SEGMENT_HEADER_LEN + stored.len());
            bytes.extend_from_slice(&SEGMENT_MAGIC.to_le_bytes());
            bytes.push(kind);
            bytes.push(codec);
            bytes.extend_from_slice(&raw_len.to_le_bytes());
            bytes.extend_from_slice(&(stored.len() as u32).to_le_bytes());
            let sum = checksum32(&[bytes.as_slice(), stored.as_slice()]);
            bytes.extend_from_slice(&sum.to_le_bytes());
            bytes.extend_from_slice(&stored);
            // The header carries `kind`, so the segment is decoded as that kind.
            let expect = Kind::from_u8(kind).unwrap();
            let result = decode_segment(&bytes, 0, expect);
            if raw_len > MAX_SEGMENT_RAW {
                match result {
                    Err(StrataError::Corrupt { what, .. }) => {
                        prop_assert_eq!(what, "segment raw length");
                    }
                    other => prop_assert!(false, "expected Corrupt, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn incompressible_body_is_stored_raw() {
        // A xorshift stream is incompressible, so zstd cannot make it smaller.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let body: Vec<u8> = (0..4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        let (file, seg) = encode(Kind::NarrowChunk, &body, Codec::Zstd, 0);
        assert_eq!(file[3], Codec::Raw as u8);
        assert_eq!(seg.len as usize, 16 + body.len());
    }

    #[test]
    fn compressible_body_is_stored_zstd() {
        let body = vec![7u8; 4096];
        let (file, seg) = encode(Kind::NarrowChunk, &body, Codec::Zstd, 0);
        assert_eq!(file[3], Codec::Zstd as u8);
        assert!((seg.len as usize) < body.len());
    }

    #[test]
    fn flipping_any_byte_is_an_error() {
        let body = b"strata segment body, a few bytes long".to_vec();
        for prefer in [Codec::Zstd, Codec::Raw] {
            let (file, seg) = encode(Kind::NarrowChunk, &body, prefer, 0);
            let bytes = file[..seg.len as usize].to_vec();
            for i in 0..bytes.len() {
                let mut flipped = bytes.clone();
                flipped[i] ^= 0x01;
                let err = decode_segment(&flipped, 0, Kind::NarrowChunk)
                    .expect_err("a flipped byte must not decode");
                if i >= SEGMENT_HEADER_LEN {
                    assert_eq!(corrupt_what(err), "segment checksum", "byte {i}");
                }
            }
        }
    }

    #[test]
    fn wrong_kind_is_an_error() {
        let (file, seg) = encode(Kind::NarrowChunk, b"abc", Codec::Raw, 0);
        let bytes = &file[..seg.len as usize];
        assert_eq!(
            corrupt_what(decode_segment(bytes, 0, Kind::Dictionary).unwrap_err()),
            "segment kind"
        );
    }
}
