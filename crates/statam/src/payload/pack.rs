//! Immutable content-addressed `.stpk` chunk pack builder and reader.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::hash::BuildHasherDefault;
use std::io::Write;
use std::ops::Range;
use std::sync::{Arc, OnceLock};

use bytes::Bytes;

use crate::StatamError;
use crate::format::{
    CHUNK_LOC_LEN, ChunkLoc, Codec, DEFAULT_TAIL_HINT, FORMAT_VERSION, FRAME_ENTRY_LEN, FrameEntry,
    MAX_BLOCK_RAW_BYTES, MAX_BLOOM_BYTES, MAX_FRAME_RAW_BYTES, MAX_HOT_TAIL_BYTES, MAX_PACK_CHUNKS,
    MAX_PACK_FRAMES, PACK_FLAG_HAS_DICT, PACK_FLAG_HAS_TRIGRAM, PACK_FLAG_TOC_LZ4,
    PACK_FLAG_TOC_ZSTD, PACK_FRAME_FLAG_USES_DICT, PACK_FRAME_HEADER_LEN, PACK_FRAME_MAGIC,
    PACK_HEADER_LEN, PACK_POSTSCRIPT_LEN, PackHeader, PackPostscript, compress_guarded,
    decompress_checked, read_framed_pack_frame, write_framed_pack_frame,
};
use crate::io::{RangeReader, S3_CACHE_BLOCK_BYTES};
use crate::simd::trigram::{MAX_TRIGRAM_BLOOM_BYTES, MIN_TRIGRAM_BLOOM_BYTES, TrigramBloom};

/// Maximum head/tail edge bytes stored per chunk in `EdgeTable`.
pub const EDGE_LEN: usize = 63;

/// `(head_63, tail_63)` edge bytes for one chunk.
pub type ChunkEdge = (Vec<u8>, Vec<u8>);

/// Magic bytes at offset 0 of a `TS02` TrigramSkipSection header.
pub const TRIGRAM_SECTION_MAGIC: [u8; 4] = *b"TS02";
/// Fixed 12-byte header size preceding the two 4 KiB edge trigram blooms in `TS02`.
const TS02_HEADER_LEN: usize = 12;
/// Byte size of each edge trigram bloom (`edge_tail_bloom` and `edge_head_bloom`).
const EDGE_TRIGRAM_BLOOM_BYTES: usize = MAX_TRIGRAM_BLOOM_BYTES;
/// Total byte length of the `TS02` prefix before per-frame trigram blooms (`12 + 4096 + 4096`).
const TS02_PREFIX_LEN: usize = TS02_HEADER_LEN + 2 * EDGE_TRIGRAM_BLOOM_BYTES;

/// Minimum compressed frame length required to emit a trigram bloom (`< 1024` omitted).
const MIN_FRAME_FOR_TRIGRAM: usize = 1024;
/// Maximum trigram bloom bit density (`> 50%` saturated blooms are omitted).
const MAX_TRIGRAM_DENSITY: f64 = 0.50;
/// 1 MiB `ReadCache` boundary guard threshold: pad if `< 8 KiB` left in current 1 MiB block.
const BOUNDARY_GUARD_THRESHOLD: usize = 8_192;
/// 1 MiB `ReadCache` boundary guard max frame size (`< 32 KiB`).
const BOUNDARY_GUARD_MAX_FRAME: usize = 32_768;
/// Default target raw bytes per `.stpk` frame (1 MiB).
const DEFAULT_TARGET_FRAME_RAW_BYTES: usize = 1024 * 1024;

/// Configuration options for [`ChunkPackBuilder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackWriteOptions {
    /// Compression codec for pack frames (default: `Codec::Zstd`).
    pub codec: Codec,
    /// Zstd compression level when `codec == Codec::Zstd` (default: `3`).
    pub zstd_level: i32,
    /// Target uncompressed frame size in bytes (default: `1 MiB`).
    pub target_frame_raw_bytes: usize,
    /// Whether to emit adaptive per-frame trigram skip filters (default: `true`).
    pub enable_trigram_skip: bool,
    /// Whether to compress the Pack TOC body (`Zstd` delta-coded or `Lz4`) when smaller (default: `true`).
    pub compress_toc_lz4: bool,
}

impl Default for PackWriteOptions {
    fn default() -> Self {
        Self {
            codec: Codec::Zstd,
            zstd_level: 3,
            target_frame_raw_bytes: DEFAULT_TARGET_FRAME_RAW_BYTES,
            enable_trigram_skip: true,
            compress_toc_lz4: true,
        }
    }
}

/// Summary statistics emitted by [`ChunkPackBuilder::finish`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PackWriteStats {
    /// 128-bit pack identifier.
    pub pack_id: [u8; 16],
    /// Total serialized `.stpk` byte length.
    pub total_bytes: u64,
    /// Number of frames written.
    pub frame_count: u32,
    /// Number of unique chunks stored.
    pub chunk_count: u32,
    /// Number of `intern_chunk` calls satisfied by deduplication.
    pub dedup_hits: u64,
    /// Total uncompressed bytes across unique chunks.
    pub raw_chunk_bytes: u64,
    /// Total stored frame bytes (including `PF` headers, excluding padding).
    pub stored_frame_bytes: u64,
    /// Byte length of `TrigramSkipSection`.
    pub trigram_section_bytes: u32,
    /// Byte length of `EdgeTable`.
    pub edge_table_bytes: u32,
    /// Stored byte length of Pack TOC + 112-byte Postscript.
    pub toc_stored_bytes: u32,
}

#[derive(Default)]
struct Hash16IdentityHasher(u64);

impl std::hash::Hasher for Hash16IdentityHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        if bytes.len() >= 8 {
            self.0 = u64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]);
        } else {
            let mut acc = 0u64;
            for &b in bytes {
                acc = acc.rotate_left(8) ^ u64::from(b);
            }
            self.0 = acc;
        }
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy)]
struct InlineEdge {
    len: u8,
    bytes: [u8; EDGE_LEN],
}

impl InlineEdge {
    #[inline]
    fn empty() -> Self {
        Self {
            len: 0,
            bytes: [0u8; EDGE_LEN],
        }
    }

    #[inline]
    fn from_slice(s: &[u8]) -> Self {
        let len = s.len().min(EDGE_LEN);
        let mut bytes = [0u8; EDGE_LEN];
        bytes[..len].copy_from_slice(&s[..len]);
        Self {
            len: len as u8,
            bytes,
        }
    }

    #[inline]
    fn as_slice(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

#[derive(Clone)]
struct PendingChunk {
    hash16: [u8; 16],
    offset_in_frame: u32,
    raw_len: u32,
    head: InlineEdge,
    tail: InlineEdge,
}

impl fmt::Debug for PendingChunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingChunk")
            .field("offset_in_frame", &self.offset_in_frame)
            .field("raw_len", &self.raw_len)
            .field("head_len", &self.head.len)
            .field("tail_len", &self.tail.len)
            .finish()
    }
}

/// Streaming builder for an immutable `.stpk` chunk pack.
#[derive(Clone)]
struct PackPrefixEntry {
    head_hash: u64,
    prefix_len: u32,
    prefix_blake3: [u8; 32],
    prefix_refs: Vec<(u16, u32)>,
}

#[inline]
fn fast_head_hash_128(bytes: &[u8]) -> u64 {
    let mut h = 0x517c_c1b7_2722_0a95u64;
    for &chunk in bytes[..128].as_chunks::<8>().0 {
        let v = u64::from_le_bytes(chunk);
        h = (h ^ v).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
    h ^= h >> 31;
    h
}

pub struct ChunkPackBuilder {
    pack_id: [u8; 16],
    options: PackWriteOptions,
    /// Holds the 48-byte header placeholder followed by framed data and alignment padding.
    buf: Vec<u8>,
    dedup: HashMap<[u8; 16], u32, BuildHasherDefault<Hash16IdentityHasher>>,
    frames: Vec<FrameEntry>,
    chunk_locs: Vec<ChunkLoc>,
    hashes: Vec<[u8; 16]>,
    edges: Vec<(InlineEdge, InlineEdge)>,
    trigram_buf: Vec<u8>,
    cur_raw: Vec<u8>,
    cur_chunks: Vec<PendingChunk>,
    cur_is_bypass: bool,
    dedup_hits: u64,
    raw_chunk_bytes: u64,
    stored_frame_bytes: u64,
    prefix_cache: Vec<Option<PackPrefixEntry>>,
}

impl fmt::Debug for ChunkPackBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkPackBuilder")
            .field("options", &self.options)
            .field("buf_len", &self.buf.len())
            .field("frame_count", &self.frames.len())
            .field("chunk_count", &self.chunk_locs.len())
            .field("trigram_buf_len", &self.trigram_buf.len())
            .field("cur_raw_len", &self.cur_raw.len())
            .field("cur_chunk_count", &self.cur_chunks.len())
            .field("dedup_hits", &self.dedup_hits)
            .field("raw_chunk_bytes", &self.raw_chunk_bytes)
            .field("stored_frame_bytes", &self.stored_frame_bytes)
            .finish()
    }
}

impl ChunkPackBuilder {
    /// Creates a new `.stpk` pack builder for `pack_id`.
    pub fn new(pack_id: [u8; 16], mut options: PackWriteOptions) -> Self {
        options.target_frame_raw_bytes =
            options.target_frame_raw_bytes.clamp(1, MAX_FRAME_RAW_BYTES);
        Self {
            pack_id,
            options,
            buf: vec![0u8; PACK_HEADER_LEN],
            dedup: HashMap::default(),
            frames: Vec::new(),
            chunk_locs: Vec::new(),
            hashes: Vec::new(),
            edges: Vec::new(),
            trigram_buf: Vec::new(),
            cur_raw: Vec::new(),
            cur_chunks: Vec::new(),
            cur_is_bypass: false,
            dedup_hits: 0,
            raw_chunk_bytes: 0,
            stored_frame_bytes: 0,
            prefix_cache: Vec::new(),
        }
    }

    /// Looks up a previously interned cumulative JSON-array prefix across batches/files.
    pub(crate) fn lookup_prefix(&self, bytes: &[u8]) -> Option<(usize, &[(u16, u32)])> {
        if self.prefix_cache.is_empty() || bytes.len() < 128 {
            return None;
        }
        let head_hash = fast_head_hash_128(bytes);
        let slot = (head_hash as usize) & 4095;
        let entry = self.prefix_cache[slot].as_ref()?;
        let plen = entry.prefix_len as usize;
        if entry.head_hash != head_hash || bytes.len() <= plen + 1 || bytes[plen] != b',' {
            return None;
        }
        let digest = blake3::hash(&bytes[..plen]);
        if *digest.as_bytes() == entry.prefix_blake3 {
            Some((plen, &entry.prefix_refs))
        } else {
            None
        }
    }

    /// Stores a verified cumulative JSON-array prefix in the cross-file cache.
    pub(crate) fn insert_prefix(&mut self, prefix: &[u8], prefix_refs: &[(u16, u32)]) {
        if prefix.len() < 128 {
            return;
        }
        let Ok(prefix_len) = u32::try_from(prefix.len()) else {
            return;
        };
        if self.prefix_cache.is_empty() {
            self.prefix_cache = vec![None; 4096];
        }
        let head_hash = fast_head_hash_128(prefix);
        let slot = (head_hash as usize) & 4095;
        let prefix_blake3 = *blake3::hash(prefix).as_bytes();
        self.prefix_cache[slot] = Some(PackPrefixEntry {
            head_hash,
            prefix_len,
            prefix_blake3,
            prefix_refs: prefix_refs.to_vec(),
        });
    }

    /// Returns the 128-bit pack identifier.
    pub fn pack_id(&self) -> [u8; 16] {
        self.pack_id
    }

    /// Interns `bytes` using BLAKE3-128 content addressing, returning its unique `chunk_id`.
    pub fn intern_chunk(&mut self, bytes: &[u8]) -> Result<u32, StatamError> {
        if bytes.len() > MAX_FRAME_RAW_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "chunk length exceeds MAX_FRAME_RAW_BYTES",
            });
        }
        if self.cur_is_bypass && !self.cur_raw.is_empty() {
            self.flush_current_frame()?;
        }
        self.cur_is_bypass = false;

        let hash = blake3::hash(bytes);
        let mut hash16 = [0u8; 16];
        hash16.copy_from_slice(&hash.as_bytes()[..16]);

        if let Some(&existing_id) = self.dedup.get(&hash16) {
            self.dedup_hits = self.dedup_hits.saturating_add(1);
            return Ok(existing_id);
        }

        let total_chunks = self.chunk_locs.len() + self.cur_chunks.len();
        if total_chunks >= MAX_PACK_CHUNKS {
            return Err(StatamError::BoundsExceeded {
                reason: "pack chunk count exceeds MAX_PACK_CHUNKS",
            });
        }
        let chunk_id = u32::try_from(total_chunks).map_err(|_| StatamError::BoundsExceeded {
            reason: "chunk_id overflows u32",
        })?;

        let target = self.options.target_frame_raw_bytes.max(1);
        if !self.cur_raw.is_empty() && self.cur_raw.len().saturating_add(bytes.len()) > target {
            self.flush_current_frame()?;
        }

        let offset_in_frame =
            u32::try_from(self.cur_raw.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "chunk offset_in_frame overflows u32",
            })?;
        let raw_len = u32::try_from(bytes.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "chunk raw_len overflows u32",
        })?;

        self.cur_raw.extend_from_slice(bytes);
        let head = InlineEdge::from_slice(&bytes[..bytes.len().min(EDGE_LEN)]);
        let tail = InlineEdge::from_slice(&bytes[bytes.len().saturating_sub(EDGE_LEN)..]);

        self.cur_chunks.push(PendingChunk {
            hash16,
            offset_in_frame,
            raw_len,
            head,
            tail,
        });
        self.dedup.insert(hash16, chunk_id);
        self.raw_chunk_bytes = self.raw_chunk_bytes.saturating_add(bytes.len() as u64);

        if self.cur_raw.len() >= target {
            self.flush_current_frame()?;
        }

        Ok(chunk_id)
    }

    /// Appends a raw chunk directly in `W4` bypass mode: skips CDC chunking,
    /// deduplication map lookup, and trigram bloom extraction, while compressing
    /// frames with `Codec::Zstd` (automatically falling back to `Codec::None` if incompressible).
    pub fn append_raw_bypass(&mut self, bytes: &[u8]) -> Result<u32, StatamError> {
        self.append_raw_bypass_with_edge(bytes, true)
    }

    pub(crate) fn append_raw_bypass_with_edge(
        &mut self,
        bytes: &[u8],
        record_edge: bool,
    ) -> Result<u32, StatamError> {
        if bytes.len() > MAX_FRAME_RAW_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "chunk length exceeds MAX_FRAME_RAW_BYTES",
            });
        }
        if !self.cur_is_bypass && !self.cur_raw.is_empty() {
            self.flush_current_frame()?;
        }
        self.cur_is_bypass = true;

        let total_chunks = self.chunk_locs.len() + self.cur_chunks.len();
        if total_chunks >= MAX_PACK_CHUNKS {
            return Err(StatamError::BoundsExceeded {
                reason: "pack chunk count exceeds MAX_PACK_CHUNKS",
            });
        }
        let chunk_id = u32::try_from(total_chunks).map_err(|_| StatamError::BoundsExceeded {
            reason: "chunk_id overflows u32",
        })?;

        let target = self.options.target_frame_raw_bytes.max(1);
        if !self.cur_raw.is_empty() && self.cur_raw.len().saturating_add(bytes.len()) > target {
            self.flush_current_frame()?;
        }

        let offset_in_frame =
            u32::try_from(self.cur_raw.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "chunk offset_in_frame overflows u32",
            })?;
        let raw_len = u32::try_from(bytes.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "chunk raw_len overflows u32",
        })?;

        let hash = blake3::hash(bytes);
        let mut hash16 = [0u8; 16];
        hash16.copy_from_slice(&hash.as_bytes()[..16]);

        self.cur_raw.extend_from_slice(bytes);
        let (head, tail) = if record_edge {
            (
                InlineEdge::from_slice(&bytes[..bytes.len().min(EDGE_LEN)]),
                InlineEdge::from_slice(&bytes[bytes.len().saturating_sub(EDGE_LEN)..]),
            )
        } else {
            (InlineEdge::empty(), InlineEdge::empty())
        };

        self.cur_chunks.push(PendingChunk {
            hash16,
            offset_in_frame,
            raw_len,
            head,
            tail,
        });
        self.raw_chunk_bytes = self.raw_chunk_bytes.saturating_add(bytes.len() as u64);

        if self.cur_raw.len() >= target {
            self.flush_current_frame()?;
        }

        Ok(chunk_id)
    }

    /// Appends a pre-compressed `PF` frame verbatim without decompressing or recompressing.
    ///
    /// Used by `gdpr_rewrite_day` for 100%-live frames. Returns the newly assigned
    /// `chunk_id`s in the same order as `chunks`.
    pub fn append_raw_frame(
        &mut self,
        raw_frame_bytes: &[u8],
        entry: FrameEntry,
        chunks: &[([u8; 16], u32, u32)],
        filter_bytes: &[u8],
        edges: &[(Vec<u8>, Vec<u8>)],
    ) -> Result<Vec<u32>, StatamError> {
        if !self.cur_raw.is_empty() {
            self.flush_current_frame()?;
        }
        self.cur_is_bypass = false;
        if chunks.len() != entry.chunk_count as usize || edges.len() != chunks.len() {
            return Err(StatamError::Corrupt {
                reason: "append_raw_frame chunk count mismatch",
            });
        }
        if entry.flags & PACK_FRAME_FLAG_USES_DICT != 0 {
            return Err(StatamError::Corrupt {
                reason: "cannot append raw dict-compressed frame without dict",
            });
        }
        if raw_frame_bytes.len() < PACK_FRAME_HEADER_LEN {
            return Err(StatamError::Corrupt {
                reason: "raw frame shorter than PF header",
            });
        }
        if raw_frame_bytes[0..2] != PACK_FRAME_MAGIC {
            return Err(StatamError::Corrupt {
                reason: "invalid PF magic in append_raw_frame",
            });
        }
        let header_codec = Codec::from_u8(raw_frame_bytes[2])?;
        let header_flags = raw_frame_bytes[3];
        let header_raw_len = read_u32_le(raw_frame_bytes, 4);
        let header_stored_len = read_u32_le(raw_frame_bytes, 8);
        let header_crc = read_u32_le(raw_frame_bytes, 12);

        if header_codec != entry.codec
            || header_flags != entry.flags
            || header_raw_len != entry.raw_len
            || header_stored_len != entry.stored_len
            || header_crc != entry.crc32c
        {
            return Err(StatamError::Corrupt {
                reason: "raw frame PF header does not match FrameEntry",
            });
        }
        let expected_total = PACK_FRAME_HEADER_LEN
            .checked_add(entry.stored_len as usize)
            .ok_or(StatamError::BoundsExceeded {
                reason: "raw frame length overflow",
            })?;
        if raw_frame_bytes.len() != expected_total {
            return Err(StatamError::Corrupt {
                reason: "raw frame length does not match FrameEntry stored_len",
            });
        }
        let actual_crc = crc32c::crc32c(&raw_frame_bytes[PACK_FRAME_HEADER_LEN..]);
        if actual_crc != entry.crc32c {
            return Err(StatamError::ChecksumMismatch {
                context: "append_raw_frame payload",
            });
        }
        if (entry.raw_len as usize) > MAX_FRAME_RAW_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "raw frame exceeds MAX_FRAME_RAW_BYTES",
            });
        }

        if self.frames.len() >= MAX_PACK_FRAMES {
            return Err(StatamError::BoundsExceeded {
                reason: "pack frame count exceeds MAX_PACK_FRAMES",
            });
        }
        if self.chunk_locs.len().saturating_add(chunks.len()) > MAX_PACK_CHUNKS {
            return Err(StatamError::BoundsExceeded {
                reason: "pack chunk count exceeds MAX_PACK_CHUNKS",
            });
        }

        for (&(_, off, len), (head, tail)) in chunks.iter().zip(edges.iter()) {
            let end = off.checked_add(len).ok_or(StatamError::BoundsExceeded {
                reason: "chunk span overflows u32",
            })?;
            if end > entry.raw_len {
                return Err(StatamError::Corrupt {
                    reason: "chunk offset + len exceeds frame raw_len",
                });
            }
            if head.len() > EDGE_LEN || tail.len() > EDGE_LEN {
                return Err(StatamError::Corrupt {
                    reason: "edge table entry exceeds EDGE_LEN",
                });
            }
        }

        self.apply_1mib_boundary_guard(raw_frame_bytes.len());
        let frame_offset = self.buf.len() as u64;
        self.buf.extend_from_slice(raw_frame_bytes);
        self.stored_frame_bytes = self
            .stored_frame_bytes
            .saturating_add(raw_frame_bytes.len() as u64);

        let (filter_off, filter_len) = if !filter_bytes.is_empty()
            && self.options.enable_trigram_skip
        {
            let bloom = TrigramBloom::from_bytes(filter_bytes.to_vec())?;
            let off =
                u32::try_from(self.trigram_buf.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "trigram section offset overflows u32",
                })?;
            let len =
                u32::try_from(bloom.as_bytes().len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "trigram filter length overflows u32",
                })?;
            self.trigram_buf.extend_from_slice(bloom.as_bytes());
            (off, len)
        } else {
            (0, 0)
        };

        let frame_id =
            u32::try_from(self.frames.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "frame_id overflows u32",
            })?;
        let first_chunk_id =
            u32::try_from(self.chunk_locs.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "first_chunk_id overflows u32",
            })?;

        self.frames.push(FrameEntry {
            offset: frame_offset,
            stored_len: entry.stored_len,
            raw_len: entry.raw_len,
            codec: entry.codec,
            flags: entry.flags,
            dict_id: entry.dict_id,
            first_chunk_id,
            chunk_count: entry.chunk_count,
            filter_off,
            filter_len,
            crc32c: entry.crc32c,
        });

        let mut remapped = Vec::with_capacity(chunks.len());
        for (&(hash16, offset_in_frame, raw_len), (head, tail)) in chunks.iter().zip(edges.iter()) {
            let chunk_id =
                u32::try_from(self.chunk_locs.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "chunk_id overflows u32",
                })?;
            self.chunk_locs.push(ChunkLoc {
                frame_id,
                offset_in_frame,
                raw_len,
            });
            self.hashes.push(hash16);
            self.dedup.entry(hash16).or_insert(chunk_id);
            self.edges
                .push((InlineEdge::from_slice(head), InlineEdge::from_slice(tail)));
            self.raw_chunk_bytes = self.raw_chunk_bytes.saturating_add(u64::from(raw_len));
            remapped.push(chunk_id);
        }

        Ok(remapped)
    }

    /// Finalizes and writes the complete `.stpk` file into `out`.
    pub fn finish<W: Write>(mut self, mut out: W) -> Result<PackWriteStats, StatamError> {
        if !self.cur_raw.is_empty() {
            self.flush_current_frame()?;
        }
        self.cur_is_bypass = false;

        let frame_count =
            u32::try_from(self.frames.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "frame_count overflows u32",
            })?;
        let chunk_count =
            u32::try_from(self.chunk_locs.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "chunk_count overflows u32",
            })?;

        let mut cur_off = self.buf.len() as u64;

        // 1. ChunkHashTable (compaction-only: [[u8; 16]; chunk_count])
        let hash_table_off = cur_off;
        let mut hash_table_bytes = Vec::with_capacity(self.hashes.len().saturating_mul(16));
        for h in &self.hashes {
            hash_table_bytes.extend_from_slice(h);
        }
        cur_off = cur_off.saturating_add(hash_table_bytes.len() as u64);

        // 2. TrigramSkipSection (`TS02`: 12 B header + 4 KiB tail bloom + 4 KiB head bloom + per-frame blooms)
        let mut trigram_section = Vec::new();
        let (trigram_off, trigram_len) = if !self.trigram_buf.is_empty() {
            let mut edge_tail_bloom = TrigramBloom::new(EDGE_TRIGRAM_BLOOM_BYTES);
            let mut edge_head_bloom = TrigramBloom::new(EDGE_TRIGRAM_BLOOM_BYTES);
            for (head, tail) in &self.edges {
                edge_tail_bloom.insert_bytes(tail.as_slice());
                edge_head_bloom.insert_bytes(head.as_slice());
            }

            let prefix_u32 =
                u32::try_from(TS02_PREFIX_LEN).map_err(|_| StatamError::BoundsExceeded {
                    reason: "TS02 prefix length overflows u32",
                })?;
            for f in &mut self.frames {
                if f.filter_len > 0 {
                    f.filter_off = f.filter_off.checked_add(prefix_u32).ok_or(
                        StatamError::BoundsExceeded {
                            reason: "frame filter_off overflows u32",
                        },
                    )?;
                }
            }

            let total_len = TS02_PREFIX_LEN.checked_add(self.trigram_buf.len()).ok_or(
                StatamError::BoundsExceeded {
                    reason: "trigram section size overflow",
                },
            )?;
            let len = u32::try_from(total_len).map_err(|_| StatamError::BoundsExceeded {
                reason: "trigram section exceeds u32",
            })?;
            trigram_section.reserve(total_len);
            trigram_section.extend_from_slice(&TRIGRAM_SECTION_MAGIC);
            trigram_section.extend_from_slice(&(EDGE_TRIGRAM_BLOOM_BYTES as u32).to_le_bytes());
            trigram_section.extend_from_slice(&0u32.to_le_bytes());
            trigram_section.extend_from_slice(edge_tail_bloom.as_bytes());
            trigram_section.extend_from_slice(edge_head_bloom.as_bytes());
            trigram_section.extend_from_slice(&self.trigram_buf);

            let off = cur_off;
            cur_off = cur_off.saturating_add(u64::from(len));
            (off, len)
        } else {
            (0u64, 0u32)
        };

        // 3. EdgeTable (Zstd-3 compressed `PF`-framed payload)
        let mut edge_framed = Vec::new();
        let (edge_table_off, edge_table_len) = if !self.edges.is_empty() {
            let mut raw_edges = Vec::with_capacity(self.edges.len().saturating_mul(16));
            for (head, tail) in &self.edges {
                let hs = head.as_slice();
                let ts = tail.as_slice();
                raw_edges.push(hs.len() as u8);
                raw_edges.extend_from_slice(hs);
                raw_edges.push(ts.len() as u8);
                raw_edges.extend_from_slice(ts);
            }
            let edge_codec = default_edge_codec();
            write_framed_pack_frame(
                &raw_edges,
                edge_codec,
                0,
                self.options.zstd_level,
                &mut edge_framed,
            )?;
            let len =
                u32::try_from(edge_framed.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "edge table exceeds u32",
                })?;
            let off = cur_off;
            cur_off = cur_off.saturating_add(u64::from(len));
            (off, len)
        } else {
            (0u64, 0u32)
        };

        // 4. DictSection (0 B by default)
        let dict_off = 0u64;
        let dict_len = 0u32;

        // 5. Pack TOC Body ([FrameEntry; F] || [ChunkLoc; C])
        let toc_raw_len_usize =
            self.frames.len() * FRAME_ENTRY_LEN + self.chunk_locs.len() * CHUNK_LOC_LEN;
        if toc_raw_len_usize > MAX_HOT_TAIL_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "pack TOC raw size exceeds MAX_HOT_TAIL_BYTES",
            });
        }
        let mut toc_raw = Vec::with_capacity(toc_raw_len_usize);
        for f in &self.frames {
            toc_raw.extend_from_slice(&f.encode());
        }
        for c in &self.chunk_locs {
            toc_raw.extend_from_slice(&c.encode());
        }
        let toc_raw_len =
            u32::try_from(toc_raw.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "toc_raw_len overflows u32",
            })?;

        let (toc_flag, toc_stored) = if self.options.compress_toc_lz4 {
            #[cfg(not(miri))]
            let try_zstd = self.options.codec == Codec::Zstd;
            #[cfg(miri)]
            let try_zstd = false;

            if try_zstd {
                let mut toc_delta = Vec::with_capacity(toc_raw_len_usize);
                for f in &self.frames {
                    toc_delta.extend_from_slice(&f.encode());
                }
                let mut prev_frame = u32::MAX;
                let mut prev_end = 0u32;
                for c in &self.chunk_locs {
                    let off_delta = if c.frame_id == prev_frame {
                        c.offset_in_frame.wrapping_sub(prev_end)
                    } else {
                        c.offset_in_frame
                    };
                    prev_frame = c.frame_id;
                    prev_end = c.offset_in_frame.wrapping_add(c.raw_len);
                    toc_delta.extend_from_slice(
                        &ChunkLoc {
                            frame_id: c.frame_id,
                            offset_in_frame: off_delta,
                            raw_len: c.raw_len,
                        }
                        .encode(),
                    );
                }
                let (c, s) = compress_guarded(&toc_delta, Codec::Zstd, self.options.zstd_level)?;
                if c == Codec::Zstd {
                    (PACK_FLAG_TOC_ZSTD, s)
                } else {
                    (0, toc_raw)
                }
            } else {
                let (c, s) = compress_guarded(&toc_raw, Codec::Lz4, 0)?;
                let flag = if c == Codec::Lz4 {
                    PACK_FLAG_TOC_LZ4
                } else {
                    0
                };
                (flag, s)
            }
        } else {
            (0, toc_raw)
        };
        let toc_stored_len = toc_stored
            .len()
            .checked_add(PACK_POSTSCRIPT_LEN)
            .and_then(|v| u32::try_from(v).ok())
            .ok_or(StatamError::BoundsExceeded {
                reason: "toc_stored_len overflows u32",
            })?;
        if (toc_stored_len as usize) > MAX_HOT_TAIL_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "pack TOC stored size exceeds MAX_HOT_TAIL_BYTES",
            });
        }
        let toc_crc32c = crc32c::crc32c(&toc_stored);

        let mut flags = toc_flag;
        if trigram_len > 0 {
            flags |= PACK_FLAG_HAS_TRIGRAM;
        }

        let header = PackHeader {
            format_version: FORMAT_VERSION,
            flags,
            required_features: 0,
            pack_id: self.pack_id,
            enc_envelope_offset: 0,
            enc_envelope_len: 0,
            reserved: 0,
        };
        self.buf[0..PACK_HEADER_LEN].copy_from_slice(&header.encode());

        let postscript = PackPostscript {
            toc_stored_len,
            toc_raw_len,
            format_version: FORMAT_VERSION,
            flags,
            required_features: 0,
            pack_id: self.pack_id,
            frame_count,
            chunk_count,
            hash_table_off,
            trigram_off,
            trigram_len,
            edge_table_len,
            edge_table_off,
            dict_off,
            dict_len,
            enc_envelope_offset: 0,
            enc_envelope_len: 0,
            reserved: 0,
            toc_crc32c,
        };
        let ps_bytes = postscript.encode();

        out.write_all(&self.buf)?;
        out.write_all(&hash_table_bytes)?;
        if !trigram_section.is_empty() {
            out.write_all(&trigram_section)?;
        }
        if !edge_framed.is_empty() {
            out.write_all(&edge_framed)?;
        }
        out.write_all(&toc_stored)?;
        out.write_all(&ps_bytes)?;

        let total_bytes = cur_off.saturating_add(u64::from(toc_stored_len));

        Ok(PackWriteStats {
            pack_id: self.pack_id,
            total_bytes,
            frame_count,
            chunk_count,
            dedup_hits: self.dedup_hits,
            raw_chunk_bytes: self.raw_chunk_bytes,
            stored_frame_bytes: self.stored_frame_bytes,
            trigram_section_bytes: trigram_len,
            edge_table_bytes: edge_table_len,
            toc_stored_bytes: toc_stored_len,
        })
    }

    fn apply_1mib_boundary_guard(&mut self, total_frame_len: usize) {
        let mib = S3_CACHE_BLOCK_BYTES as usize;
        let cur_off = self.buf.len();
        let pos_in_block = cur_off % mib;
        let remaining_in_block = mib - pos_in_block;
        if remaining_in_block < BOUNDARY_GUARD_THRESHOLD
            && total_frame_len < BOUNDARY_GUARD_MAX_FRAME
            && pos_in_block + total_frame_len > mib
        {
            self.buf.resize(cur_off + remaining_in_block, 0);
        }
    }

    fn flush_current_frame(&mut self) -> Result<(), StatamError> {
        if self.cur_raw.is_empty() {
            return Ok(());
        }
        if self.frames.len() >= MAX_PACK_FRAMES {
            return Err(StatamError::BoundsExceeded {
                reason: "pack frame count exceeds MAX_PACK_FRAMES",
            });
        }

        let mut framed = Vec::new();
        let (frame_codec, zstd_level) = if self.cur_is_bypass {
            (default_bypass_codec(), self.options.zstd_level)
        } else {
            (self.options.codec, self.options.zstd_level)
        };
        let info = write_framed_pack_frame(&self.cur_raw, frame_codec, 0, zstd_level, &mut framed)?;
        self.apply_1mib_boundary_guard(framed.len());

        let frame_offset = self.buf.len() as u64;
        self.buf.extend_from_slice(&framed);
        self.stored_frame_bytes = self.stored_frame_bytes.saturating_add(framed.len() as u64);

        let (filter_off, filter_len) = if !self.cur_is_bypass
            && self.options.enable_trigram_skip
            && (info.stored_len as usize) >= MIN_FRAME_FOR_TRIGRAM
        {
            self.build_frame_trigram_filter(info.stored_len as usize)?
        } else {
            (0, 0)
        };

        let frame_id =
            u32::try_from(self.frames.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "frame_id overflows u32",
            })?;
        let first_chunk_id =
            u32::try_from(self.chunk_locs.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "first_chunk_id overflows u32",
            })?;
        let chunk_count =
            u32::try_from(self.cur_chunks.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "frame chunk_count overflows u32",
            })?;

        self.frames.push(FrameEntry {
            offset: frame_offset,
            stored_len: info.stored_len,
            raw_len: info.raw_len,
            codec: info.codec,
            flags: 0,
            dict_id: 0,
            first_chunk_id,
            chunk_count,
            filter_off,
            filter_len,
            crc32c: info.crc32c,
        });

        for pending in self.cur_chunks.drain(..) {
            self.chunk_locs.push(ChunkLoc {
                frame_id,
                offset_in_frame: pending.offset_in_frame,
                raw_len: pending.raw_len,
            });
            self.hashes.push(pending.hash16);
            self.edges.push((pending.head, pending.tail));
        }
        self.cur_raw.clear();
        Ok(())
    }

    fn build_frame_trigram_filter(&mut self, stored_len: usize) -> Result<(u32, u32), StatamError> {
        let max_bytes = (stored_len / 6).clamp(MIN_TRIGRAM_BLOOM_BYTES, MAX_TRIGRAM_BLOOM_BYTES);
        let cap_pow2 = 1usize << (usize::BITS as usize - 1 - max_bytes.leading_zeros() as usize);
        let mut bloom = TrigramBloom::new(cap_pow2);
        let mut scanned_bytes = 0usize;
        let mut next_check_bytes = 64 * 1024;
        for c in &self.cur_chunks {
            let start = c.offset_in_frame as usize;
            let end = start + c.raw_len as usize;
            bloom.insert_bytes(&self.cur_raw[start..end]);
            scanned_bytes += end - start;
            if scanned_bytes >= next_check_bytes {
                if bloom.bit_density() > MAX_TRIGRAM_DENSITY {
                    return Ok((0, 0));
                }
                next_check_bytes = scanned_bytes + 64 * 1024;
            }
        }

        let mut density = bloom.bit_density();
        if density == 0.0 {
            return Ok((0, 0));
        }
        while bloom.as_bytes().len() > MIN_TRIGRAM_BLOOM_BYTES && density < 0.12 {
            bloom = bloom.fold_half();
            density = bloom.bit_density();
        }
        if density > MAX_TRIGRAM_DENSITY {
            return Ok((0, 0));
        }

        let off =
            u32::try_from(self.trigram_buf.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "trigram section offset overflows u32",
            })?;
        let len =
            u32::try_from(bloom.as_bytes().len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "trigram filter length overflows u32",
            })?;
        self.trigram_buf.extend_from_slice(bloom.as_bytes());
        Ok((off, len))
    }
}

#[cfg(not(miri))]
fn default_bypass_codec() -> Codec {
    Codec::Zstd
}

#[cfg(miri)]
fn default_bypass_codec() -> Codec {
    Codec::Lz4
}

#[cfg(not(miri))]
fn default_edge_codec() -> Codec {
    Codec::Zstd
}

#[cfg(miri)]
fn default_edge_codec() -> Codec {
    Codec::Lz4
}

/// Decoded read-path Table of Contents for a `.stpk` chunk pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackToc {
    /// Decoded 112-byte PackPostscript.
    pub postscript: PackPostscript,
    /// Frame directory (`frame_count` entries).
    pub frames: Vec<FrameEntry>,
    /// Chunk location table (`chunk_count` entries).
    pub chunks: Vec<ChunkLoc>,
}

#[derive(Clone)]
pub(crate) struct TrigramSectionData {
    pub(crate) edge_blooms: Option<(TrigramBloom, TrigramBloom)>,
    pub(crate) frame_blooms: Vec<Option<TrigramBloom>>,
}

/// Range-based reader for an immutable `.stpk` chunk pack.
pub struct ChunkPackReader {
    reader: Arc<dyn RangeReader>,
    toc: PackToc,
    data_end: u64,
    trigram_cache: OnceLock<TrigramSectionData>,
    edge_cache: OnceLock<Vec<ChunkEdge>>,
}

impl fmt::Debug for ChunkPackReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkPackReader")
            .field("file_len", &self.reader.file_len())
            .field("frame_count", &self.toc.frames.len())
            .field("chunk_count", &self.toc.chunks.len())
            .finish()
    }
}

impl ChunkPackReader {
    /// Opens a `.stpk` pack by reading its tail (`DEFAULT_TAIL_HINT` = 1 MiB) in 1 GET.
    pub fn open(reader: Arc<dyn RangeReader>) -> Result<Self, StatamError> {
        Self::open_with_tail_hint(reader, None)
    }

    /// Opens a `.stpk` pack with a custom tail-read hint.
    pub fn open_with_tail_hint(
        reader: Arc<dyn RangeReader>,
        tail_hint: Option<u32>,
    ) -> Result<Self, StatamError> {
        let file_len = reader.file_len();
        let min_len = (PACK_HEADER_LEN + PACK_POSTSCRIPT_LEN) as u64;
        if file_len < min_len {
            return Err(StatamError::Corrupt {
                reason: "pack file smaller than header + postscript",
            });
        }

        let hint =
            u64::from(tail_hint.unwrap_or(DEFAULT_TAIL_HINT)).max(PACK_POSTSCRIPT_LEN as u64);
        let tail_start = file_len.saturating_sub(hint);
        let tail = reader.read_range(tail_start..file_len)?;
        if tail.len() < PACK_POSTSCRIPT_LEN {
            return Err(StatamError::Corrupt {
                reason: "short read on pack postscript",
            });
        }

        let ps = PackPostscript::decode(&tail[tail.len() - PACK_POSTSCRIPT_LEN..])?;
        if ps.flags & PACK_FLAG_HAS_DICT != 0 && ps.dict_len == 0 {
            return Err(StatamError::Corrupt {
                reason: "pack dict flag set with empty dict_len",
            });
        }
        let toc_stored_u64 = u64::from(ps.toc_stored_len);
        if toc_stored_u64 > file_len - PACK_HEADER_LEN as u64 {
            return Err(StatamError::Corrupt {
                reason: "pack toc_stored_len exceeds file data bounds",
            });
        }
        let data_end = file_len - toc_stored_u64;

        if tail_start == 0 && tail.len() >= PACK_HEADER_LEN {
            let header = PackHeader::decode(&tail[0..PACK_HEADER_LEN])?;
            if header.pack_id != ps.pack_id || header.flags != ps.flags {
                return Err(StatamError::Corrupt {
                    reason: "pack header and postscript metadata mismatch",
                });
            }
        }

        validate_trailer_sections(&ps, data_end)?;

        let toc_body_stored_len = (ps.toc_stored_len as usize) - PACK_POSTSCRIPT_LEN;
        let owned_toc_body: Bytes;
        let toc_body: &[u8] = if tail.len() >= ps.toc_stored_len as usize {
            let start = tail.len() - (ps.toc_stored_len as usize);
            &tail[start..start + toc_body_stored_len]
        } else {
            let toc_off = file_len - toc_stored_u64;
            owned_toc_body = reader.read_range(toc_off..toc_off + toc_body_stored_len as u64)?;
            &owned_toc_body
        };

        if crc32c::crc32c(toc_body) != ps.toc_crc32c {
            return Err(StatamError::ChecksumMismatch {
                context: "pack TOC",
            });
        }

        let is_zstd_toc = (ps.flags & PACK_FLAG_TOC_ZSTD) != 0;
        let toc_codec = if is_zstd_toc {
            Codec::Zstd
        } else if ps.flags & PACK_FLAG_TOC_LZ4 != 0 {
            Codec::Lz4
        } else {
            Codec::None
        };
        let toc_raw = decompress_checked(
            toc_body,
            toc_codec,
            ps.toc_raw_len as usize,
            MAX_HOT_TAIL_BYTES,
        )?;

        let frame_count = ps.frame_count as usize;
        let chunk_count = ps.chunk_count as usize;
        let frames_bytes =
            frame_count
                .checked_mul(FRAME_ENTRY_LEN)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "pack frame directory byte size overflow",
                })?;
        let chunks_bytes =
            chunk_count
                .checked_mul(CHUNK_LOC_LEN)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "pack chunk directory byte size overflow",
                })?;
        let expected_len =
            frames_bytes
                .checked_add(chunks_bytes)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "pack TOC byte size overflow",
                })?;
        if toc_raw.len() != expected_len {
            return Err(StatamError::Corrupt {
                reason: "decompressed pack TOC length mismatch",
            });
        }

        let mut frames = Vec::with_capacity(frame_count);
        for idx in 0..frame_count {
            let off = idx * FRAME_ENTRY_LEN;
            let entry = FrameEntry::decode(&toc_raw[off..off + FRAME_ENTRY_LEN])?;
            validate_frame_entry(&entry, &ps, data_end)?;
            frames.push(entry);
        }

        let mut chunks = Vec::with_capacity(chunk_count);
        let mut prev_frame = u32::MAX;
        let mut prev_end = 0u32;
        for idx in 0..chunk_count {
            let off = frames_bytes + idx * CHUNK_LOC_LEN;
            let mut loc = ChunkLoc::decode(&toc_raw[off..off + CHUNK_LOC_LEN])?;
            if is_zstd_toc && loc.frame_id == prev_frame {
                loc.offset_in_frame = prev_end.wrapping_add(loc.offset_in_frame);
            }
            let frame = frames
                .get(loc.frame_id as usize)
                .ok_or(StatamError::Corrupt {
                    reason: "chunk frame_id out of bounds",
                })?;
            let end = loc.offset_in_frame.checked_add(loc.raw_len).ok_or(
                StatamError::BoundsExceeded {
                    reason: "chunk offset_in_frame + raw_len overflow",
                },
            )?;
            if end > frame.raw_len {
                return Err(StatamError::Corrupt {
                    reason: "chunk span exceeds owning frame raw_len",
                });
            }
            prev_frame = loc.frame_id;
            prev_end = end;
            chunks.push(loc);
        }

        Ok(Self {
            reader,
            toc: PackToc {
                postscript: ps,
                frames,
                chunks,
            },
            data_end,
            trigram_cache: OnceLock::new(),
            edge_cache: OnceLock::new(),
        })
    }

    /// Returns the decoded [`PackToc`].
    pub fn toc(&self) -> &PackToc {
        &self.toc
    }

    /// Returns the 128-bit pack identifier.
    pub fn pack_id(&self) -> [u8; 16] {
        self.toc.postscript.pack_id
    }

    /// Returns the pack's [`PackPostscript`].
    pub fn postscript(&self) -> &PackPostscript {
        &self.toc.postscript
    }

    /// Reads and CRC-verifies the raw `PF` frame bytes (without decompressing).
    pub fn read_frame_raw(&self, frame_id: u32) -> Result<Bytes, StatamError> {
        let entry = self
            .toc
            .frames
            .get(frame_id as usize)
            .ok_or(StatamError::Corrupt {
                reason: "frame_id out of bounds",
            })?;
        let range = frame_byte_range(entry)?;
        let raw = self.reader.read_range(range)?;
        verify_raw_frame_matches_entry(&raw, entry)?;
        Ok(raw)
    }

    /// Reads, CRC-verifies, and decompresses frame `frame_id`.
    pub fn read_frame_decompressed(&self, frame_id: u32) -> Result<Vec<u8>, StatamError> {
        let entry = self
            .toc
            .frames
            .get(frame_id as usize)
            .ok_or(StatamError::Corrupt {
                reason: "frame_id out of bounds",
            })?;
        let raw = self.read_frame_raw(frame_id)?;
        let (_, _, decoded) = read_framed_pack_frame(&raw, MAX_FRAME_RAW_BYTES)?;
        if decoded.len() != entry.raw_len as usize {
            return Err(StatamError::Corrupt {
                reason: "decompressed frame length does not match FrameEntry",
            });
        }
        Ok(decoded)
    }

    /// Reads all requested `chunk_ids`, coalescing frame reads into a single `read_ranges` wave.
    pub fn read_chunks(&self, chunk_ids: &[u32]) -> Result<HashMap<u32, Vec<u8>>, StatamError> {
        if chunk_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut needed_frames = BTreeSet::new();
        let mut unique_chunks = BTreeSet::new();
        for &cid in chunk_ids {
            let loc = self
                .toc
                .chunks
                .get(cid as usize)
                .ok_or(StatamError::Corrupt {
                    reason: "chunk_id out of bounds",
                })?;
            needed_frames.insert(loc.frame_id);
            unique_chunks.insert(cid);
        }

        let frame_ids: Vec<u32> = needed_frames.into_iter().collect();
        let mut ranges = Vec::with_capacity(frame_ids.len());
        for &fid in &frame_ids {
            let entry = &self.toc.frames[fid as usize];
            ranges.push(frame_byte_range(entry)?);
        }

        let frame_bytes_list = self.reader.read_ranges(&ranges)?;
        if frame_bytes_list.len() != frame_ids.len() {
            return Err(StatamError::Corrupt {
                reason: "read_ranges returned wrong frame count",
            });
        }

        let mut decoded_frames: HashMap<u32, Vec<u8>> = HashMap::with_capacity(frame_ids.len());
        for (&fid, raw) in frame_ids.iter().zip(frame_bytes_list.iter()) {
            let entry = &self.toc.frames[fid as usize];
            verify_raw_frame_matches_entry(raw, entry)?;
            let (_, _, decoded) = read_framed_pack_frame(raw, MAX_FRAME_RAW_BYTES)?;
            if decoded.len() != entry.raw_len as usize {
                return Err(StatamError::Corrupt {
                    reason: "decompressed frame length mismatch",
                });
            }
            decoded_frames.insert(fid, decoded);
        }

        let mut out = HashMap::with_capacity(unique_chunks.len());
        for cid in unique_chunks {
            let loc = &self.toc.chunks[cid as usize];
            let frame_buf = decoded_frames
                .get(&loc.frame_id)
                .ok_or(StatamError::Corrupt {
                    reason: "missing decoded frame for chunk",
                })?;
            let start = loc.offset_in_frame as usize;
            let end = start + loc.raw_len as usize;
            let slice = frame_buf.get(start..end).ok_or(StatamError::Corrupt {
                reason: "chunk slice out of decoded frame bounds",
            })?;
            out.insert(cid, slice.to_vec());
        }

        Ok(out)
    }

    /// Reads and validates the per-frame trigram bloom filters (`None` where omitted), cached per reader.
    pub fn read_trigram_section(&self) -> Result<Vec<Option<TrigramBloom>>, StatamError> {
        Ok(self.trigram_section_cached()?.frame_blooms.clone())
    }

    /// Reads and validates the `TS02` boundary edge trigram blooms `(edge_tail_bloom, edge_head_bloom)`,
    /// returning `None` if absent or legacy raw format.
    pub fn read_edge_trigram_blooms(
        &self,
    ) -> Result<Option<(TrigramBloom, TrigramBloom)>, StatamError> {
        Ok(self.trigram_section_cached()?.edge_blooms.clone())
    }

    pub(crate) fn trigram_section_cached(&self) -> Result<&TrigramSectionData, StatamError> {
        if let Some(cached) = self.trigram_cache.get() {
            return Ok(cached);
        }
        let data = self.read_trigram_section_uncached()?;
        let _ = self.trigram_cache.set(data);
        self.trigram_cache.get().ok_or(StatamError::Corrupt {
            reason: "trigram cache uninitialized",
        })
    }

    fn read_trigram_section_uncached(&self) -> Result<TrigramSectionData, StatamError> {
        let ps = &self.toc.postscript;
        if ps.trigram_len == 0 {
            return Ok(TrigramSectionData {
                edge_blooms: None,
                frame_blooms: vec![None; self.toc.frames.len()],
            });
        }
        if (ps.trigram_len as usize) > MAX_BLOOM_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "pack trigram_len exceeds MAX_BLOOM_BYTES",
            });
        }

        let start = ps.trigram_off;
        let end =
            start
                .checked_add(u64::from(ps.trigram_len))
                .ok_or(StatamError::BoundsExceeded {
                    reason: "trigram section range overflow",
                })?;
        let file_bound = self.data_end.min(self.reader.file_len());
        if start < PACK_HEADER_LEN as u64 || end > file_bound {
            return Err(StatamError::Corrupt {
                reason: "trigram section out of pack bounds",
            });
        }
        let section = self.reader.read_range(start..end)?;
        if section.len() != ps.trigram_len as usize {
            return Err(StatamError::Corrupt {
                reason: "short read on pack trigram section",
            });
        }

        if section.len() < TS02_PREFIX_LEN || section[0..4] != TRIGRAM_SECTION_MAGIC {
            return Err(StatamError::Corrupt {
                reason: "invalid trigram section magic",
            });
        }
        let edge_bytes = read_u32_le(&section, 4) as usize;
        let reserved = read_u32_le(&section, 8);
        if reserved != 0 {
            return Err(StatamError::Corrupt {
                reason: "non-zero reserved field in TS02 trigram header",
            });
        }
        if !(MIN_TRIGRAM_BLOOM_BYTES..=MAX_TRIGRAM_BLOOM_BYTES).contains(&edge_bytes)
            || !edge_bytes.is_power_of_two()
        {
            return Err(StatamError::Corrupt {
                reason: "invalid TS02 edge bloom length",
            });
        }
        let prefix_len = TS02_HEADER_LEN
            .checked_add(edge_bytes.saturating_mul(2))
            .ok_or(StatamError::BoundsExceeded {
                reason: "TS02 prefix length overflow",
            })?;
        if section.len() < prefix_len {
            return Err(StatamError::Corrupt {
                reason: "truncated TS02 edge trigram blooms",
            });
        }
        let tail_bloom = TrigramBloom::from_bytes(
            section[TS02_HEADER_LEN..TS02_HEADER_LEN + edge_bytes].to_vec(),
        )?;
        let head_bloom =
            TrigramBloom::from_bytes(section[TS02_HEADER_LEN + edge_bytes..prefix_len].to_vec())?;
        let edge_blooms = Some((tail_bloom, head_bloom));
        let min_frame_off = prefix_len;

        let mut frame_blooms = Vec::with_capacity(self.toc.frames.len());
        for frame in &self.toc.frames {
            if frame.filter_len == 0 {
                frame_blooms.push(None);
            } else {
                let f_start = frame.filter_off as usize;
                if f_start < min_frame_off {
                    return Err(StatamError::Corrupt {
                        reason: "frame trigram filter overlaps TS02 header",
                    });
                }
                let f_end = f_start.checked_add(frame.filter_len as usize).ok_or(
                    StatamError::BoundsExceeded {
                        reason: "frame trigram filter slice overflow",
                    },
                )?;
                let slice = section.get(f_start..f_end).ok_or(StatamError::Corrupt {
                    reason: "frame trigram filter out of section bounds",
                })?;
                frame_blooms.push(Some(TrigramBloom::from_bytes(slice.to_vec())?));
            }
        }
        Ok(TrigramSectionData {
            edge_blooms,
            frame_blooms,
        })
    }

    /// Reads and decodes the `EdgeTable` returning `(head_63, tail_63)` per unique chunk, cached per reader.
    pub fn read_edge_table(&self) -> Result<Vec<ChunkEdge>, StatamError> {
        if let Some(cached) = self.edge_cache.get() {
            return Ok(cached.clone());
        }
        let edges = self.read_edge_table_uncached()?;
        let _ = self.edge_cache.set(edges.clone());
        Ok(edges)
    }

    fn read_edge_table_uncached(&self) -> Result<Vec<ChunkEdge>, StatamError> {
        let ps = &self.toc.postscript;
        let chunk_count = ps.chunk_count as usize;
        if chunk_count == 0 {
            return Ok(Vec::new());
        }
        if ps.edge_table_len == 0 {
            return Err(StatamError::Corrupt {
                reason: "missing edge table for non-empty chunk pack",
            });
        }
        if (ps.edge_table_len as usize) > MAX_BLOCK_RAW_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "pack edge_table_len exceeds MAX_BLOCK_RAW_BYTES",
            });
        }

        let start = ps.edge_table_off;
        let end =
            start
                .checked_add(u64::from(ps.edge_table_len))
                .ok_or(StatamError::BoundsExceeded {
                    reason: "edge table range overflow",
                })?;
        let file_bound = self.data_end.min(self.reader.file_len());
        if start < PACK_HEADER_LEN as u64 || end > file_bound {
            return Err(StatamError::Corrupt {
                reason: "edge table out of pack bounds",
            });
        }
        let framed = self.reader.read_range(start..end)?;
        let (_, _, raw) = read_framed_pack_frame(&framed, MAX_BLOCK_RAW_BYTES)?;

        // Each chunk entry needs at least 2 bytes (head_len: u8 + tail_len: u8).
        let min_bytes = chunk_count
            .checked_mul(2)
            .ok_or(StatamError::BoundsExceeded {
                reason: "edge table minimum byte count overflow",
            })?;
        if raw.len() < min_bytes {
            return Err(StatamError::Corrupt {
                reason: "truncated edge table",
            });
        }

        let mut edges = Vec::with_capacity(chunk_count);
        let mut pos = 0usize;
        for _ in 0..chunk_count {
            let head_len = *raw.get(pos).ok_or(StatamError::Corrupt {
                reason: "unexpected end of edge table head_len",
            })? as usize;
            pos += 1;
            if head_len > EDGE_LEN {
                return Err(StatamError::Corrupt {
                    reason: "edge table head_len exceeds 63",
                });
            }
            let head_end = pos.checked_add(head_len).ok_or(StatamError::Corrupt {
                reason: "edge table head slice overflow",
            })?;
            let head = raw
                .get(pos..head_end)
                .ok_or(StatamError::Corrupt {
                    reason: "truncated edge table head bytes",
                })?
                .to_vec();
            pos = head_end;

            let tail_len = *raw.get(pos).ok_or(StatamError::Corrupt {
                reason: "unexpected end of edge table tail_len",
            })? as usize;
            pos += 1;
            if tail_len > EDGE_LEN {
                return Err(StatamError::Corrupt {
                    reason: "edge table tail_len exceeds 63",
                });
            }
            let tail_end = pos.checked_add(tail_len).ok_or(StatamError::Corrupt {
                reason: "edge table tail slice overflow",
            })?;
            let tail = raw
                .get(pos..tail_end)
                .ok_or(StatamError::Corrupt {
                    reason: "truncated edge table tail bytes",
                })?
                .to_vec();
            pos = tail_end;

            edges.push((head, tail));
        }

        if pos != raw.len() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in edge table",
            });
        }
        Ok(edges)
    }

    /// Reads the compaction-only BLAKE3-128 `ChunkHashTable`.
    pub fn read_hash_table(&self) -> Result<Vec<[u8; 16]>, StatamError> {
        let ps = &self.toc.postscript;
        let chunk_count = ps.chunk_count as usize;
        if chunk_count == 0 {
            return Ok(Vec::new());
        }
        let table_bytes = chunk_count
            .checked_mul(16)
            .ok_or(StatamError::BoundsExceeded {
                reason: "hash table byte size overflow",
            })?;
        let end = ps.hash_table_off.checked_add(table_bytes as u64).ok_or(
            StatamError::BoundsExceeded {
                reason: "hash table end offset overflow",
            },
        )?;
        if ps.hash_table_off < PACK_HEADER_LEN as u64 || end > self.data_end {
            return Err(StatamError::Corrupt {
                reason: "hash table out of pack data bounds",
            });
        }

        let bytes = self.reader.read_range(ps.hash_table_off..end)?;
        if bytes.len() != table_bytes {
            return Err(StatamError::Corrupt {
                reason: "short read on pack hash table",
            });
        }

        let (chunks, _) = bytes.as_chunks::<16>();
        Ok(chunks.to_vec())
    }
}

fn validate_trailer_sections(ps: &PackPostscript, data_end: u64) -> Result<(), StatamError> {
    let min_off = PACK_HEADER_LEN as u64;
    let hash_bytes =
        (ps.chunk_count as u64)
            .checked_mul(16)
            .ok_or(StatamError::BoundsExceeded {
                reason: "hash table size overflow",
            })?;
    if ps.chunk_count > 0 {
        check_pack_range(ps.hash_table_off, hash_bytes, min_off, data_end)?;
    }
    if ps.trigram_len > 0 {
        if (ps.trigram_len as usize) > MAX_BLOOM_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "pack trigram_len exceeds MAX_BLOOM_BYTES",
            });
        }
        check_pack_range(ps.trigram_off, u64::from(ps.trigram_len), min_off, data_end)?;
    }
    if ps.edge_table_len > 0 {
        if (ps.edge_table_len as usize) > MAX_BLOCK_RAW_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "pack edge_table_len exceeds MAX_BLOCK_RAW_BYTES",
            });
        }
        check_pack_range(
            ps.edge_table_off,
            u64::from(ps.edge_table_len),
            min_off,
            data_end,
        )?;
    }
    if ps.dict_len > 0 {
        check_pack_range(ps.dict_off, u64::from(ps.dict_len), min_off, data_end)?;
    }
    Ok(())
}

fn check_pack_range(off: u64, len: u64, min_off: u64, max_end: u64) -> Result<(), StatamError> {
    let end = off.checked_add(len).ok_or(StatamError::BoundsExceeded {
        reason: "pack section range overflow",
    })?;
    if off < min_off || end > max_end {
        return Err(StatamError::Corrupt {
            reason: "pack section out of bounds",
        });
    }
    Ok(())
}

fn validate_frame_entry(
    entry: &FrameEntry,
    ps: &PackPostscript,
    data_end: u64,
) -> Result<(), StatamError> {
    if (entry.stored_len as usize) > MAX_FRAME_RAW_BYTES + 1024 {
        return Err(StatamError::BoundsExceeded {
            reason: "pack frame stored_len exceeds MAX_FRAME_RAW_BYTES + 1024",
        });
    }
    let total_len = (PACK_FRAME_HEADER_LEN as u64)
        .checked_add(u64::from(entry.stored_len))
        .ok_or(StatamError::BoundsExceeded {
            reason: "frame stored length overflow",
        })?;
    check_pack_range(entry.offset, total_len, PACK_HEADER_LEN as u64, data_end)?;

    let chunk_end =
        entry
            .first_chunk_id
            .checked_add(entry.chunk_count)
            .ok_or(StatamError::BoundsExceeded {
                reason: "frame chunk range overflow",
            })?;
    if chunk_end > ps.chunk_count {
        return Err(StatamError::Corrupt {
            reason: "frame chunk range exceeds pack chunk_count",
        });
    }

    if entry.filter_len > 0 {
        let f_len = entry.filter_len as usize;
        if !(MIN_TRIGRAM_BLOOM_BYTES..=MAX_TRIGRAM_BLOOM_BYTES).contains(&f_len)
            || !f_len.is_power_of_two()
        {
            return Err(StatamError::Corrupt {
                reason: "invalid frame trigram filter length",
            });
        }
        let f_end =
            entry
                .filter_off
                .checked_add(entry.filter_len)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "frame trigram filter offset overflow",
                })?;
        if f_end > ps.trigram_len {
            return Err(StatamError::Corrupt {
                reason: "frame trigram filter exceeds trigram section",
            });
        }
    }
    Ok(())
}

fn frame_byte_range(entry: &FrameEntry) -> Result<Range<u64>, StatamError> {
    let total = (PACK_FRAME_HEADER_LEN as u64)
        .checked_add(u64::from(entry.stored_len))
        .ok_or(StatamError::BoundsExceeded {
            reason: "frame byte range length overflow",
        })?;
    let end = entry
        .offset
        .checked_add(total)
        .ok_or(StatamError::BoundsExceeded {
            reason: "frame byte range end overflow",
        })?;
    Ok(entry.offset..end)
}

fn verify_raw_frame_matches_entry(raw: &[u8], entry: &FrameEntry) -> Result<(), StatamError> {
    let expected_total = PACK_FRAME_HEADER_LEN
        .checked_add(entry.stored_len as usize)
        .ok_or(StatamError::BoundsExceeded {
            reason: "raw frame expected length overflow",
        })?;
    if raw.len() != expected_total {
        return Err(StatamError::Corrupt {
            reason: "raw frame byte length mismatch",
        });
    }
    if raw[0..2] != PACK_FRAME_MAGIC {
        return Err(StatamError::Corrupt {
            reason: "invalid PF magic",
        });
    }
    let crc = crc32c::crc32c(&raw[PACK_FRAME_HEADER_LEN..]);
    if crc != entry.crc32c {
        return Err(StatamError::ChecksumMismatch {
            context: "pack frame",
        });
    }
    Ok(())
}

#[inline]
fn read_u32_le(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{SimulatedS3Config, SimulatedS3Reader, SliceReader};

    fn test_codec() -> Codec {
        #[cfg(miri)]
        {
            Codec::Lz4
        }
        #[cfg(not(miri))]
        {
            Codec::Zstd
        }
    }

    #[test]
    fn pack_builder_and_reader_roundtrip_1mib_boundary_guard_and_dedup() {
        let pack_id = [0xA5u8; 16];
        let opts = PackWriteOptions {
            codec: test_codec(),
            zstd_level: 3,
            target_frame_raw_bytes: 16 * 1024,
            enable_trigram_skip: true,
            compress_toc_lz4: true,
        };
        let mut builder = ChunkPackBuilder::new(pack_id, opts);

        // Emit enough distinct chunks so we cross a 1 MiB boundary and verify the guard.
        // Each chunk is 20 KiB of pseudo-random incompressible data so frames stay ~20 KiB
        // (< BOUNDARY_GUARD_MAX_FRAME = 32 KiB), guaranteeing frames cross/hit the last 8 KiB of 1 MiB!
        let mut chunk_ids = Vec::new();
        let mut expected_chunks: Vec<Vec<u8>> = Vec::new();
        let mut state: u64 = 0x1234_5678_9abc_def0;

        for i in 0..64u32 {
            // Every 5th chunk duplicates chunk 0 to verify BLAKE3-128 dedup.
            if i > 0 && i % 5 == 0 {
                let dup_id = builder.intern_chunk(&expected_chunks[0]).unwrap();
                assert_eq!(dup_id, chunk_ids[0]);
                chunk_ids.push(dup_id);
                continue;
            }
            let mut data = vec![0u8; 20 * 1024];
            for b in &mut data {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *b = (state >> 24) as u8;
            }
            let cid = builder.intern_chunk(&data).unwrap();
            expected_chunks.push(data);
            chunk_ids.push(cid);
        }

        let mut bytes = Vec::new();
        let stats = builder.finish(&mut bytes).unwrap();
        assert_eq!(stats.pack_id, pack_id);
        assert_eq!(stats.chunk_count as usize, expected_chunks.len());
        assert_eq!(stats.dedup_hits, 12);

        let reader = ChunkPackReader::open(Arc::new(SliceReader::new(bytes.clone()))).unwrap();
        assert_eq!(reader.pack_id(), pack_id);
        assert_eq!(reader.toc().chunks.len(), expected_chunks.len());

        // Verify 1 MiB boundary guard: no frame < BOUNDARY_GUARD_MAX_FRAME crosses a 1 MiB boundary
        // from within the last BOUNDARY_GUARD_THRESHOLD bytes of a 1 MiB block.
        let mib = S3_CACHE_BLOCK_BYTES;
        for f in &reader.toc().frames {
            let total = PACK_FRAME_HEADER_LEN as u64 + u64::from(f.stored_len);
            if total < BOUNDARY_GUARD_MAX_FRAME as u64 {
                let pos_in_block = f.offset % mib;
                let remaining = mib - pos_in_block;
                if remaining < BOUNDARY_GUARD_THRESHOLD as u64 {
                    let start_block = f.offset / mib;
                    let end_block = (f.offset + total - 1) / mib;
                    assert_eq!(
                        start_block, end_block,
                        "small frame at {} (len {total}, remaining {remaining}) crossed 1 MiB boundary!",
                        f.offset
                    );
                }
            }
        }

        // Verify coalesced read_chunks on SimulatedS3Reader (1 wave!).
        let s3 = Arc::new(SimulatedS3Reader::from_bytes(
            bytes.clone(),
            SimulatedS3Config::default(),
        ));
        let s3_reader = ChunkPackReader::open(s3.clone()).unwrap();
        s3.reset_counters();

        let query_ids: Vec<u32> = (0..expected_chunks.len() as u32).step_by(3).collect();
        let got = s3_reader.read_chunks(&query_ids).unwrap();
        assert_eq!(s3.counters().waves, 1);
        for cid in query_ids {
            assert_eq!(got.get(&cid).unwrap(), &expected_chunks[cid as usize]);
        }

        // Verify hash table matches BLAKE3-128 of every unique chunk.
        let hashes = reader.read_hash_table().unwrap();
        assert_eq!(hashes.len(), expected_chunks.len());
        for (i, data) in expected_chunks.iter().enumerate() {
            let h = blake3::hash(data);
            assert_eq!(&hashes[i], &h.as_bytes()[..16]);
        }

        // Verify edge table entries match head_63 and tail_63.
        let edges = reader.read_edge_table().unwrap();
        assert_eq!(edges.len(), expected_chunks.len());
        for (i, data) in expected_chunks.iter().enumerate() {
            assert_eq!(edges[i].0.as_slice(), &data[..EDGE_LEN]);
            assert_eq!(edges[i].1.as_slice(), &data[data.len() - EDGE_LEN..]);
        }
    }

    #[test]
    fn pack_append_raw_frame_preserves_bytes_and_remaps_chunk_ids() {
        let opts = PackWriteOptions {
            codec: test_codec(),
            target_frame_raw_bytes: 16 * 1024,
            ..PackWriteOptions::default()
        };
        let mut b1 = ChunkPackBuilder::new([1u8; 16], opts);
        let c0 = b"{\"role\":\"user\",\"text\":\"alpha\"}".repeat(400);
        let c1 = b"{\"role\":\"assistant\",\"text\":\"beta\"}".repeat(400);
        let c2 = b"{\"role\":\"user\",\"text\":\"gamma\"}".repeat(400);
        let id0 = b1.intern_chunk(&c0).unwrap();
        let id1 = b1.intern_chunk(&c1).unwrap();
        let id2 = b1.intern_chunk(&c2).unwrap();
        assert_eq!((id0, id1, id2), (0, 1, 2));

        let mut pack1_bytes = Vec::new();
        b1.finish(&mut pack1_bytes).unwrap();
        let r1 = ChunkPackReader::open(Arc::new(SliceReader::new(pack1_bytes))).unwrap();
        let hashes1 = r1.read_hash_table().unwrap();
        let trigram1 = r1.read_trigram_section().unwrap();
        let edges1 = r1.read_edge_table().unwrap();

        // Copy frame 1 verbatim into pack 2 after writing a fresh chunk first.
        let mut b2 = ChunkPackBuilder::new([2u8; 16], opts);
        let fresh = b"fresh_chunk_in_pack2".repeat(50);
        let fresh_id = b2.intern_chunk(&fresh).unwrap();
        assert_eq!(fresh_id, 0);

        let f_entry = r1.toc().frames[1];
        let raw_f1 = r1.read_frame_raw(1).unwrap();
        let first_cid = f_entry.first_chunk_id as usize;
        let cnt = f_entry.chunk_count as usize;
        let chunk_meta: Vec<([u8; 16], u32, u32)> = (first_cid..first_cid + cnt)
            .map(|cid| {
                let loc = r1.toc().chunks[cid];
                (hashes1[cid], loc.offset_in_frame, loc.raw_len)
            })
            .collect();
        let filter_slice = trigram1[1].as_ref().map(|b| b.as_bytes()).unwrap_or(&[]);
        let edge_slice = &edges1[first_cid..first_cid + cnt];

        let remapped = b2
            .append_raw_frame(&raw_f1, f_entry, &chunk_meta, filter_slice, edge_slice)
            .unwrap();
        assert_eq!(remapped, vec![1]);

        let mut pack2_bytes = Vec::new();
        b2.finish(&mut pack2_bytes).unwrap();
        let r2 = ChunkPackReader::open(Arc::new(SliceReader::new(pack2_bytes))).unwrap();
        let raw_f1_in_p2 = r2.read_frame_raw(1).unwrap();
        assert_eq!(raw_f1_in_p2, raw_f1);

        let got = r2.read_chunks(&[0, 1]).unwrap();
        assert_eq!(got.get(&0).unwrap(), &fresh);
        assert_eq!(got.get(&1).unwrap(), &c1);
    }

    #[test]
    fn pack_reader_rejects_corrupt_and_truncated_packs_without_panic() {
        let opts = PackWriteOptions {
            codec: test_codec(),
            ..PackWriteOptions::default()
        };
        let mut b = ChunkPackBuilder::new([3u8; 16], opts);
        b.intern_chunk(&vec![b'x'; 4096]).unwrap();
        let mut bytes = Vec::new();
        b.finish(&mut bytes).unwrap();

        // Corrupt TOC CRC
        let mut bad_toc = bytes.clone();
        bad_toc[bytes.len() - PACK_POSTSCRIPT_LEN - 1] ^= 0xFF;
        assert!(matches!(
            ChunkPackReader::open(Arc::new(SliceReader::new(bad_toc))),
            Err(StatamError::ChecksumMismatch { .. })
        ));

        // Corrupt frame payload CRC
        let r_ok = ChunkPackReader::open(Arc::new(SliceReader::new(bytes.clone()))).unwrap();
        let mut bad_frame = bytes.clone();
        bad_frame[r_ok.toc().frames[0].offset as usize + PACK_FRAME_HEADER_LEN] ^= 0x01;
        let r_bad = ChunkPackReader::open(Arc::new(SliceReader::new(bad_frame))).unwrap();
        assert!(matches!(
            r_bad.read_chunks(&[0]),
            Err(StatamError::ChecksumMismatch { .. })
        ));

        // Too short file
        assert!(matches!(
            ChunkPackReader::open(Arc::new(SliceReader::new(vec![0u8; 100]))),
            Err(StatamError::Corrupt { .. })
        ));
    }

    #[test]
    fn pack_omits_saturated_or_small_trigrams_and_handles_small_tail_hint() {
        let opts = PackWriteOptions {
            codec: test_codec(),
            zstd_level: 3,
            target_frame_raw_bytes: 32 * 1024,
            enable_trigram_skip: true,
            compress_toc_lz4: false,
        };
        let mut b = ChunkPackBuilder::new([4u8; 16], opts);

        // Frame 0: tiny chunk (< 2560 B stored) -> filter_len must be 0.
        let tiny = b"tiny frame payload".repeat(20);
        b.intern_chunk(&tiny).unwrap();

        // Force flush into Frame 1 by adding a 32 KiB random chunk (all trigrams saturated -> >50% density).
        let mut state: u64 = 0xdead_c0de_cafe_1234;
        let mut saturated = vec![0u8; 34 * 1024];
        for byte in &mut saturated {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = (state >> 24) as u8;
        }
        b.intern_chunk(&saturated).unwrap();

        let mut bytes = Vec::new();
        b.finish(&mut bytes).unwrap();

        // Open with tiny tail hint (128 B) so TOC requires a second read_range.
        let r = ChunkPackReader::open_with_tail_hint(Arc::new(SliceReader::new(bytes)), Some(128))
            .unwrap();
        assert_eq!(r.toc().frames.len(), 2);
        assert_eq!(r.toc().frames[0].filter_len, 0);
        assert_eq!(r.toc().frames[1].filter_len, 0);
        let blooms = r.read_trigram_section().unwrap();
        assert_eq!(blooms, vec![None, None]);
    }

    #[test]
    fn pack_clamps_target_frame_bytes_and_rejects_oversized_sections() {
        // target_frame_raw_bytes = 0 is clamped to 1 (flushes each non-empty chunk).
        let opts_zero = PackWriteOptions {
            codec: test_codec(),
            target_frame_raw_bytes: 0,
            ..PackWriteOptions::default()
        };
        let b_zero = ChunkPackBuilder::new([9u8; 16], opts_zero);
        assert_eq!(b_zero.options.target_frame_raw_bytes, 1);

        let opts_huge = PackWriteOptions {
            codec: test_codec(),
            target_frame_raw_bytes: usize::MAX,
            ..PackWriteOptions::default()
        };
        let b_huge = ChunkPackBuilder::new([10u8; 16], opts_huge);
        assert_eq!(b_huge.options.target_frame_raw_bytes, MAX_FRAME_RAW_BYTES);

        // Crafted trailer with trigram_off + trigram_len > file_len rejected on open before read_range.
        let mut b = ChunkPackBuilder::new([11u8; 16], PackWriteOptions::default());
        b.intern_chunk(&vec![b'z'; 512]).unwrap();
        let mut bytes = Vec::new();
        b.finish(&mut bytes).unwrap();

        let ps_start = bytes.len() - PACK_POSTSCRIPT_LEN;
        let mut ps = PackPostscript::decode(&bytes[ps_start..]).unwrap();
        ps.trigram_off = bytes.len() as u64;
        ps.trigram_len = 256;
        bytes[ps_start..].copy_from_slice(&ps.encode());
        assert!(matches!(
            ChunkPackReader::open(Arc::new(SliceReader::new(bytes))),
            Err(StatamError::Corrupt { .. })
        ));
    }
}
