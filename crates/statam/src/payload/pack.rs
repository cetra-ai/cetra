//! Immutable content-addressed `.stpk` chunk pack builder and reader.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::hash::BuildHasherDefault;
use std::io::Write;
use std::ops::Range;
use std::sync::{Arc, OnceLock};

use bytes::Bytes;

use crate::StatamError;
use crate::format::{
    CHUNK_LOC_LEN, ChunkLoc, Codec, DEFAULT_TAIL_HINT, FORMAT_VERSION, FRAME_ENTRY_LEN, FrameEntry,
    FramedWriteInfo, MAX_BLOCK_RAW_BYTES, MAX_BLOOM_BYTES, MAX_FRAME_RAW_BYTES, MAX_HOT_TAIL_BYTES,
    MAX_PACK_CHUNKS, MAX_PACK_DICT_BYTES, MAX_PACK_FRAMES, PACK_FLAG_HAS_DICT,
    PACK_FLAG_HAS_TRIGRAM, PACK_FLAG_TOC_LZ4, PACK_FLAG_TOC_ZSTD, PACK_FRAME_FLAG_USES_DICT,
    PACK_FRAME_HEADER_LEN, PACK_FRAME_MAGIC, PACK_HEADER_LEN, PACK_POSTSCRIPT_LEN, PackDecoderDict,
    PackDictCompressor, PackHeader, PackPostscript, compress_guarded, decompress_checked,
    read_framed_pack_frame, read_framed_pack_frame_with_dict, write_framed_pack_frame,
    write_framed_pack_frame_with_compressor,
};
use crate::io::{RangeReader, S3_CACHE_BLOCK_BYTES};
use crate::parallel::{MAX_WRITE_THREADS, parallel_map_in_order_with_init};
use crate::simd::trigram::{MAX_TRIGRAM_BLOOM_BYTES, MIN_TRIGRAM_BLOOM_BYTES, TrigramBloom};

/// Maximum head/tail edge bytes stored per chunk in `EdgeTable`.
pub const EDGE_LEN: usize = 63;

/// `(head_63, tail_63)` edge bytes for one chunk.
pub type ChunkEdge = (Vec<u8>, Vec<u8>);

/// Magic bytes at offset 0 of a `TS02` TrigramSkipSection header.
pub const TRIGRAM_SECTION_MAGIC: [u8; 4] = *b"TS02";
/// Fixed 12-byte header size preceding the two edge trigram blooms in `TS02`.
const TS02_HEADER_LEN: usize = 12;

/// Minimum compressed frame length required to emit a trigram bloom (`< 1024` omitted).
const MIN_FRAME_FOR_TRIGRAM: usize = 1024;
/// Maximum trigram bloom bit density (`> 72%` saturated blooms are omitted).
const MAX_TRIGRAM_DENSITY: f64 = 0.72;
/// 1 MiB `ReadCache` boundary guard threshold: pad if `< 8 KiB` left in current 1 MiB block.
const BOUNDARY_GUARD_THRESHOLD: usize = 8_192;
/// 1 MiB `ReadCache` boundary guard max frame size (`< 32 KiB`).
const BOUNDARY_GUARD_MAX_FRAME: usize = 32_768;
/// Default target raw bytes per `.stpk` frame (1 MiB).
const DEFAULT_TARGET_FRAME_RAW_BYTES: usize = 1024 * 1024;
/// Minimum unique raw frame bytes buffered before triggering streaming Zstd dict training (2 MiB).
const DICT_TRAIN_MIN_RAW: usize = 2 * 1024 * 1024;
/// Target trained Zstd dictionary size (64 KiB).
const DICT_TARGET_BYTES: usize = 64 * 1024;
/// Maximum chunk samples fed into Zstd dictionary training.
const DICT_MAX_SAMPLES: usize = 1_024;
/// Maximum prefix bytes taken from each sampled chunk during dictionary training.
const DICT_MAX_SAMPLE_SLICE: usize = 2_048;
/// Maximum total raw bytes queued in `pending_frames` before flushing a parallel batch (32 MiB).
const MAX_PENDING_RAW_BYTES: usize = 32 * 1024 * 1024;

/// Configuration options for [`ChunkPackBuilder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackWriteOptions {
    /// Compression codec for pack frames (default: `Codec::Zstd`, or `Codec::Lz4` under Miri).
    pub codec: Codec,
    /// Zstd compression level when `codec == Codec::Zstd` (default: `3`).
    pub zstd_level: i32,
    /// Target uncompressed frame size in bytes (default: `1 MiB`).
    pub target_frame_raw_bytes: usize,
    /// Whether to emit adaptive per-frame trigram skip filters (default: `true`).
    pub enable_trigram_skip: bool,
    /// Whether to compress the Pack TOC body (`Zstd` delta-coded or `Lz4`) when smaller (default: `true`).
    pub compress_toc_lz4: bool,
    /// Number of worker threads for parallel frame compression and trigram building (default: `1`).
    pub write_threads: usize,
}

impl Default for PackWriteOptions {
    fn default() -> Self {
        Self {
            codec: default_pack_codec(),
            zstd_level: 3,
            target_frame_raw_bytes: DEFAULT_TARGET_FRAME_RAW_BYTES,
            enable_trigram_skip: true,
            compress_toc_lz4: true,
            write_threads: 1,
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
    enable_dict: bool,
    dict_decided: bool,
    pre_dict_frames: Vec<(Vec<u8>, Vec<PendingChunk>)>,
    pre_dict_raw_bytes: usize,
    pre_dict_chunk_count: usize,
    pending_frames: Vec<(Vec<u8>, Vec<PendingChunk>, bool)>,
    pending_frame_chunks: usize,
    pending_frame_raw_bytes: usize,
    trained_dict: Option<Vec<u8>>,
    dict_compressor: Option<PackDictCompressor>,
    used_dict_in_any_frame: bool,
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
            .field("enable_dict", &self.enable_dict)
            .field("dict_decided", &self.dict_decided)
            .field("pre_dict_frames", &self.pre_dict_frames.len())
            .field("pre_dict_raw_bytes", &self.pre_dict_raw_bytes)
            .field("pending_frames", &self.pending_frames.len())
            .field("pending_frame_chunks", &self.pending_frame_chunks)
            .field("pending_frame_raw_bytes", &self.pending_frame_raw_bytes)
            .field("used_dict_in_any_frame", &self.used_dict_in_any_frame)
            .field("dedup_hits", &self.dedup_hits)
            .field("raw_chunk_bytes", &self.raw_chunk_bytes)
            .field("stored_frame_bytes", &self.stored_frame_bytes)
            .finish()
    }
}

impl ChunkPackBuilder {
    /// Creates a new `.stpk` pack builder for `pack_id` (Zstd dictionary training disabled by default).
    pub fn new(pack_id: [u8; 16], options: PackWriteOptions) -> Self {
        Self::new_inner(pack_id, options, false)
    }

    /// Creates a new `.stpk` pack builder for `pack_id` with opt-in Zstd dictionary training
    /// enabled whenever `options.codec == Codec::Zstd` under non-Miri builds.
    pub fn new_with_dict(pack_id: [u8; 16], options: PackWriteOptions) -> Self {
        let enable_dict = default_dict_enabled(options.codec);
        Self::new_inner(pack_id, options, enable_dict)
    }

    fn new_inner(pack_id: [u8; 16], mut options: PackWriteOptions, enable_dict: bool) -> Self {
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
            enable_dict,
            dict_decided: !enable_dict,
            pre_dict_frames: Vec::new(),
            pre_dict_raw_bytes: 0,
            pre_dict_chunk_count: 0,
            pending_frames: Vec::new(),
            pending_frame_chunks: 0,
            pending_frame_raw_bytes: 0,
            trained_dict: None,
            dict_compressor: None,
            used_dict_in_any_frame: false,
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

        let total_chunks = self.chunk_locs.len()
            + self.pre_dict_chunk_count
            + self.pending_frame_chunks
            + self.cur_chunks.len();
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
        if !self.dict_decided {
            self.dict_decided = true;
        }
        if !self.pre_dict_frames.is_empty() {
            self.flush_pre_dict_without_dict()?;
        }
        self.cur_is_bypass = true;

        let total_chunks = self.chunk_locs.len()
            + self.pre_dict_chunk_count
            + self.pending_frame_chunks
            + self.cur_chunks.len();
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
        if !self.dict_decided {
            self.dict_decided = true;
        }
        if !self.pre_dict_frames.is_empty() {
            self.flush_pre_dict_without_dict()?;
        }
        if !self.pending_frames.is_empty() {
            self.flush_pending_frames()?;
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
        if !self.dict_decided {
            self.dict_decided = true;
        }
        if !self.pre_dict_frames.is_empty() {
            self.flush_pre_dict_without_dict()?;
        }
        if !self.pending_frames.is_empty() {
            self.flush_pending_frames()?;
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

        // 2. TrigramSkipSection (`TS02`: 12 B header + 2 * edge_bloom_bytes + per-frame blooms)
        let mut trigram_section = Vec::new();
        let (trigram_off, trigram_len) = if !self.trigram_buf.is_empty() {
            let edge_bloom_bytes = if self.stored_frame_bytes >= 2 * 1024 * 1024 {
                8_192
            } else {
                4_096
            };
            let prefix_len = TS02_HEADER_LEN + 2 * edge_bloom_bytes;
            let mut edge_tail_bloom = TrigramBloom::new(edge_bloom_bytes);
            let mut edge_head_bloom = TrigramBloom::new(edge_bloom_bytes);
            for (head, tail) in &self.edges {
                edge_tail_bloom.insert_bytes(tail.as_slice());
                edge_head_bloom.insert_bytes(head.as_slice());
            }

            let prefix_u32 =
                u32::try_from(prefix_len).map_err(|_| StatamError::BoundsExceeded {
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

            let total_len = prefix_len.checked_add(self.trigram_buf.len()).ok_or(
                StatamError::BoundsExceeded {
                    reason: "trigram section size overflow",
                },
            )?;
            let len = u32::try_from(total_len).map_err(|_| StatamError::BoundsExceeded {
                reason: "trigram section exceeds u32",
            })?;
            trigram_section.reserve(total_len);
            trigram_section.extend_from_slice(&TRIGRAM_SECTION_MAGIC);
            trigram_section.extend_from_slice(&(edge_bloom_bytes as u32).to_le_bytes());
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

        // 4. DictSection (optional `PF`-framed Zstd dictionary)
        let mut dict_framed = Vec::new();
        let (dict_off, dict_len) = if self.used_dict_in_any_frame
            && let Some(ref dict_bytes) = self.trained_dict
        {
            write_framed_pack_frame(
                dict_bytes,
                Codec::Zstd,
                0,
                self.options.zstd_level,
                &mut dict_framed,
            )?;
            let len =
                u32::try_from(dict_framed.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "dict section exceeds u32",
                })?;
            let off = cur_off;
            cur_off = cur_off.saturating_add(u64::from(len));
            (off, len)
        } else {
            (0u64, 0u32)
        };

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
        if dict_len > 0 {
            flags |= PACK_FLAG_HAS_DICT;
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
        if !dict_framed.is_empty() {
            out.write_all(&dict_framed)?;
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
        if self.frames.len() + self.pre_dict_frames.len() + self.pending_frames.len()
            >= MAX_PACK_FRAMES
        {
            return Err(StatamError::BoundsExceeded {
                reason: "pack frame count exceeds MAX_PACK_FRAMES",
            });
        }

        if !self.cur_is_bypass && self.enable_dict && !self.dict_decided {
            let raw = std::mem::take(&mut self.cur_raw);
            let chunks = std::mem::take(&mut self.cur_chunks);
            self.pre_dict_raw_bytes = self.pre_dict_raw_bytes.saturating_add(raw.len());
            self.pre_dict_chunk_count = self.pre_dict_chunk_count.saturating_add(chunks.len());
            self.pre_dict_frames.push((raw, chunks));

            if self.pre_dict_raw_bytes >= DICT_TRAIN_MIN_RAW {
                self.train_and_flush_pre_dict_frames()?;
            }
            return Ok(());
        }

        let raw = std::mem::take(&mut self.cur_raw);
        let chunks = std::mem::take(&mut self.cur_chunks);
        if self.options.write_threads > 1 {
            let batch_cap = self.options.write_threads.clamp(1, MAX_WRITE_THREADS);
            if !self.pending_frames.is_empty()
                && (self.pending_frames.len() >= batch_cap
                    || self.pending_frame_raw_bytes.saturating_add(raw.len())
                        > MAX_PENDING_RAW_BYTES)
            {
                self.flush_pending_frames()?;
            }
            self.pending_frame_chunks = self.pending_frame_chunks.saturating_add(chunks.len());
            self.pending_frame_raw_bytes = self.pending_frame_raw_bytes.saturating_add(raw.len());
            self.pending_frames.push((raw, chunks, self.cur_is_bypass));
            if self.pending_frames.len() >= batch_cap
                || self.pending_frame_raw_bytes >= MAX_PENDING_RAW_BYTES
            {
                self.flush_pending_frames()?;
            }
            Ok(())
        } else {
            self.encode_and_push_frame(&raw, chunks, self.cur_is_bypass)
        }
    }

    fn train_and_flush_pre_dict_frames(&mut self) -> Result<(), StatamError> {
        let total_chunks = self.pre_dict_chunk_count;
        #[cfg(not(miri))]
        if total_chunks >= 16 {
            let mut sample_buf =
                Vec::with_capacity(DICT_MAX_SAMPLES.min(total_chunks) * DICT_MAX_SAMPLE_SLICE);
            let mut sample_sizes = Vec::with_capacity(DICT_MAX_SAMPLES.min(total_chunks));
            let mut next_k = 0usize;
            let mut global_idx = 0usize;

            'collect: for (raw, chunks) in &self.pre_dict_frames {
                for c in chunks {
                    let target_idx = if total_chunks <= DICT_MAX_SAMPLES {
                        next_k
                    } else {
                        (next_k * total_chunks) / DICT_MAX_SAMPLES
                    };
                    if global_idx == target_idx {
                        let start = c.offset_in_frame as usize;
                        let slice_len = (c.raw_len as usize).min(DICT_MAX_SAMPLE_SLICE);
                        if slice_len > 0
                            && let Some(slice) = raw.get(start..start + slice_len)
                        {
                            sample_buf.extend_from_slice(slice);
                            sample_sizes.push(slice_len);
                        }
                        next_k += 1;
                        if sample_sizes.len() >= DICT_MAX_SAMPLES || next_k >= total_chunks {
                            break 'collect;
                        }
                    }
                    global_idx += 1;
                }
            }

            if sample_sizes.len() >= 16
                && sample_buf.len() >= 64 * 1024
                && let Ok(dict_bytes) =
                    zstd::dict::from_continuous(&sample_buf, &sample_sizes, DICT_TARGET_BYTES)
                && !dict_bytes.is_empty()
                && dict_bytes.len() <= MAX_PACK_DICT_BYTES
                && let Ok(comp) = PackDictCompressor::new(self.options.zstd_level, &dict_bytes)
            {
                self.trained_dict = Some(dict_bytes);
                self.dict_compressor = Some(comp);
            }
        }
        #[cfg(miri)]
        let _ = total_chunks;

        self.dict_decided = true;
        let pending = std::mem::take(&mut self.pre_dict_frames);
        self.pre_dict_raw_bytes = 0;
        self.pre_dict_chunk_count = 0;
        self.flush_pre_dict_batch(pending)
    }

    fn flush_pre_dict_without_dict(&mut self) -> Result<(), StatamError> {
        self.dict_decided = true;
        let pending = std::mem::take(&mut self.pre_dict_frames);
        self.pre_dict_raw_bytes = 0;
        self.pre_dict_chunk_count = 0;
        self.flush_pre_dict_batch(pending)
    }

    fn flush_pre_dict_batch(
        &mut self,
        pending: Vec<(Vec<u8>, Vec<PendingChunk>)>,
    ) -> Result<(), StatamError> {
        if self.options.write_threads > 1 {
            for (raw, chunks) in pending {
                self.pending_frame_chunks = self.pending_frame_chunks.saturating_add(chunks.len());
                self.pending_frame_raw_bytes =
                    self.pending_frame_raw_bytes.saturating_add(raw.len());
                self.pending_frames.push((raw, chunks, false));
            }
            if !self.pending_frames.is_empty() {
                self.flush_pending_frames()?;
            }
        } else {
            for (raw, chunks) in pending {
                self.encode_and_push_frame(&raw, chunks, false)?;
            }
        }
        Ok(())
    }

    fn flush_pending_frames(&mut self) -> Result<(), StatamError> {
        let batch = std::mem::take(&mut self.pending_frames);
        self.pending_frame_chunks = 0;
        self.pending_frame_raw_bytes = 0;
        if batch.is_empty() {
            return Ok(());
        }

        let options = self.options;
        let trained_dict = self.trained_dict.as_deref();
        let encoded = parallel_map_in_order_with_init(
            batch,
            options.write_threads,
            || Ok(None::<PackDictCompressor>),
            |worker_comp, (raw, chunks, is_bypass)| {
                if raw.is_empty() {
                    return Ok(None);
                }
                if !is_bypass
                    && worker_comp.is_none()
                    && let Some(dict_bytes) = trained_dict
                {
                    *worker_comp = Some(PackDictCompressor::new(options.zstd_level, dict_bytes)?);
                }
                let mut framed = Vec::new();
                let frame_codec = if is_bypass {
                    default_bypass_codec()
                } else {
                    options.codec
                };
                let dict_comp = if is_bypass {
                    None
                } else {
                    worker_comp.as_mut()
                };
                let info = write_framed_pack_frame_with_compressor(
                    &raw,
                    frame_codec,
                    0,
                    options.zstd_level,
                    dict_comp,
                    &mut framed,
                )?;
                let frame_flags = framed.get(3).copied().unwrap_or(0);
                let filter_bytes = if !is_bypass
                    && options.enable_trigram_skip
                    && (info.stored_len as usize) >= MIN_FRAME_FOR_TRIGRAM
                {
                    build_frame_trigram_bytes(&raw, &chunks, info.stored_len as usize)
                } else {
                    None
                };
                drop(raw);
                Ok(Some((framed, info, frame_flags, filter_bytes, chunks)))
            },
        )?;

        for item in encoded {
            let Some((framed, info, frame_flags, filter_bytes, chunks)) = item else {
                continue;
            };
            self.commit_encoded_frame(&framed, info, frame_flags, filter_bytes.as_deref(), chunks)?;
        }
        Ok(())
    }

    fn encode_and_push_frame(
        &mut self,
        raw: &[u8],
        chunks: Vec<PendingChunk>,
        is_bypass: bool,
    ) -> Result<(), StatamError> {
        if raw.is_empty() {
            return Ok(());
        }
        if self.frames.len() >= MAX_PACK_FRAMES {
            return Err(StatamError::BoundsExceeded {
                reason: "pack frame count exceeds MAX_PACK_FRAMES",
            });
        }

        let mut framed = Vec::new();
        let (frame_codec, zstd_level) = if is_bypass {
            (default_bypass_codec(), self.options.zstd_level)
        } else {
            (self.options.codec, self.options.zstd_level)
        };
        let dict_comp = if is_bypass {
            None
        } else {
            self.dict_compressor.as_mut()
        };
        let info = write_framed_pack_frame_with_compressor(
            raw,
            frame_codec,
            0,
            zstd_level,
            dict_comp,
            &mut framed,
        )?;
        let frame_flags = framed.get(3).copied().unwrap_or(0);
        let filter_bloom = if !is_bypass
            && self.options.enable_trigram_skip
            && (info.stored_len as usize) >= MIN_FRAME_FOR_TRIGRAM
        {
            build_frame_trigram_bloom(raw, &chunks, info.stored_len as usize)
        } else {
            None
        };
        self.commit_encoded_frame(
            &framed,
            info,
            frame_flags,
            filter_bloom.as_ref().map(TrigramBloom::as_bytes),
            chunks,
        )
    }

    fn commit_encoded_frame(
        &mut self,
        framed: &[u8],
        info: FramedWriteInfo,
        frame_flags: u8,
        filter_bytes: Option<&[u8]>,
        chunks: Vec<PendingChunk>,
    ) -> Result<(), StatamError> {
        if self.frames.len() >= MAX_PACK_FRAMES {
            return Err(StatamError::BoundsExceeded {
                reason: "pack frame count exceeds MAX_PACK_FRAMES",
            });
        }
        if (frame_flags & PACK_FRAME_FLAG_USES_DICT) != 0 {
            self.used_dict_in_any_frame = true;
        }
        self.apply_1mib_boundary_guard(framed.len());

        let frame_offset = self.buf.len() as u64;
        self.buf.extend_from_slice(framed);
        self.stored_frame_bytes = self.stored_frame_bytes.saturating_add(framed.len() as u64);

        let (filter_off, filter_len) = if let Some(fb) = filter_bytes {
            let off =
                u32::try_from(self.trigram_buf.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "trigram section offset overflows u32",
                })?;
            let len = u32::try_from(fb.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "trigram filter length overflows u32",
            })?;
            self.trigram_buf.extend_from_slice(fb);
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
        let chunk_count = u32::try_from(chunks.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "frame chunk_count overflows u32",
        })?;

        self.frames.push(FrameEntry {
            offset: frame_offset,
            stored_len: info.stored_len,
            raw_len: info.raw_len,
            codec: info.codec,
            flags: frame_flags,
            dict_id: 0,
            first_chunk_id,
            chunk_count,
            filter_off,
            filter_len,
            crc32c: info.crc32c,
        });

        for pending in chunks {
            self.chunk_locs.push(ChunkLoc {
                frame_id,
                offset_in_frame: pending.offset_in_frame,
                raw_len: pending.raw_len,
            });
            self.hashes.push(pending.hash16);
            self.edges.push((pending.head, pending.tail));
        }
        Ok(())
    }
}

fn build_frame_trigram_bytes(
    raw: &[u8],
    chunks: &[PendingChunk],
    stored_len: usize,
) -> Option<Vec<u8>> {
    build_frame_trigram_bloom(raw, chunks, stored_len).map(|b| b.as_bytes().to_vec())
}

fn build_frame_trigram_bloom(
    raw: &[u8],
    chunks: &[PendingChunk],
    stored_len: usize,
) -> Option<TrigramBloom> {
    let max_bytes = (stored_len / 6).clamp(MIN_TRIGRAM_BLOOM_BYTES, MAX_TRIGRAM_BLOOM_BYTES);
    let cap_pow2 = 1usize << (usize::BITS as usize - 1 - max_bytes.leading_zeros() as usize);
    let mut bloom = TrigramBloom::new(cap_pow2);
    let mut scanned_bytes = 0usize;
    let mut next_check_bytes = 64 * 1024;
    for c in chunks {
        let start = c.offset_in_frame as usize;
        let end = start + c.raw_len as usize;
        bloom.insert_bytes(&raw[start..end]);
        scanned_bytes += end - start;
        if scanned_bytes >= next_check_bytes {
            if bloom.bit_density() > MAX_TRIGRAM_DENSITY {
                return None;
            }
            next_check_bytes = scanned_bytes + 64 * 1024;
        }
    }

    let mut density = bloom.bit_density();
    if density == 0.0 {
        return None;
    }
    while bloom.as_bytes().len() > MIN_TRIGRAM_BLOOM_BYTES && density < 0.12 {
        bloom = bloom.fold_half();
        density = bloom.bit_density();
    }
    if density > MAX_TRIGRAM_DENSITY {
        return None;
    }

    Some(bloom)
}

#[cfg(not(miri))]
fn default_dict_enabled(codec: Codec) -> bool {
    codec == Codec::Zstd
}

#[cfg(miri)]
fn default_dict_enabled(_codec: Codec) -> bool {
    false
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
fn default_pack_codec() -> Codec {
    Codec::Zstd
}

#[cfg(miri)]
fn default_pack_codec() -> Codec {
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

type PreparedFrameGroups = (
    Vec<(u32, Vec<u32>)>,
    Vec<Range<u64>>,
    Option<Arc<PackDecoderDict>>,
);

/// Range-based reader for an immutable `.stpk` chunk pack.
pub struct ChunkPackReader {
    reader: Arc<dyn RangeReader>,
    toc: PackToc,
    data_end: u64,
    inline_dict_framed: Option<Bytes>,
    dict_cache: OnceLock<Option<Arc<PackDecoderDict>>>,
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
    /// Opens a `.stpk` pack by reading its tail (`DEFAULT_TAIL_HINT` = 192 KiB) in 1 GET.
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
        if tail.len() as u64 != file_len - tail_start {
            return Err(StatamError::Corrupt {
                reason: "short read on pack tail",
            });
        }
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

        let trailer_sections = validate_trailer_sections(&ps, data_end)?;

        let inline_dict_framed = if (ps.flags & PACK_FLAG_HAS_DICT) != 0
            && ps.dict_len > 0
            && ps.dict_off >= tail_start
            && ps.dict_off.saturating_add(u64::from(ps.dict_len)) <= file_len
        {
            usize::try_from(ps.dict_off - tail_start)
                .ok()
                .and_then(|start| {
                    start
                        .checked_add(ps.dict_len as usize)
                        .filter(|&end| tail.get(start..end).is_some())
                        .map(|end| tail.slice(start..end))
                })
        } else {
            None
        };

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
            validate_frame_entry(&entry, &ps, data_end, &trailer_sections)?;
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
            inline_dict_framed,
            dict_cache: OnceLock::new(),
            trigram_cache: OnceLock::new(),
            edge_cache: OnceLock::new(),
        })
    }

    fn load_decoder_dict(&self) -> Result<Option<Arc<PackDecoderDict>>, StatamError> {
        let ps = &self.toc.postscript;
        if (ps.flags & PACK_FLAG_HAS_DICT) == 0 || ps.dict_len == 0 {
            return Ok(None);
        }
        if let Some(cached) = self.dict_cache.get() {
            return Ok(cached.clone());
        }
        let framed = if let Some(ref inline) = self.inline_dict_framed {
            inline.clone()
        } else {
            let start = ps.dict_off;
            let end =
                start
                    .checked_add(u64::from(ps.dict_len))
                    .ok_or(StatamError::BoundsExceeded {
                        reason: "pack dict range overflow",
                    })?;
            let file_bound = self.data_end.min(self.reader.file_len());
            if start < PACK_HEADER_LEN as u64 || end > file_bound {
                return Err(StatamError::Corrupt {
                    reason: "pack dict section out of bounds",
                });
            }
            let bytes = self.reader.read_range(start..end)?;
            if bytes.len() != ps.dict_len as usize {
                return Err(StatamError::Corrupt {
                    reason: "short read on pack dict section",
                });
            }
            bytes
        };
        let (_, flags, dict_bytes) = read_framed_pack_frame(&framed, MAX_PACK_DICT_BYTES)?;
        if flags != 0 {
            return Err(StatamError::Corrupt {
                reason: "pack dict section frame has unexpected flags",
            });
        }
        let decoder_dict = Arc::new(PackDecoderDict::new(&dict_bytes)?);
        let _ = self.dict_cache.set(Some(Arc::clone(&decoder_dict)));
        Ok(Some(decoder_dict))
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
        let dict = if (entry.flags & PACK_FRAME_FLAG_USES_DICT) != 0 {
            self.load_decoder_dict()?
        } else {
            None
        };
        let raw = self.read_frame_raw(frame_id)?;
        let (_, _, decoded) =
            read_framed_pack_frame_with_dict(&raw, MAX_FRAME_RAW_BYTES, dict.as_deref())?;
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
        let mut needs_dict = false;
        for &fid in &frame_ids {
            let entry = &self.toc.frames[fid as usize];
            if (entry.flags & PACK_FRAME_FLAG_USES_DICT) != 0 {
                needs_dict = true;
            }
            ranges.push(frame_byte_range(entry)?);
        }

        let dict = if needs_dict {
            self.load_decoder_dict()?
        } else {
            None
        };

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
            let (_, _, decoded) =
                read_framed_pack_frame_with_dict(raw, MAX_FRAME_RAW_BYTES, dict.as_deref())?;
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

    fn prepare_frame_chunk_groups(
        &self,
        chunk_ids: &[u32],
    ) -> Result<PreparedFrameGroups, StatamError> {
        let mut by_frame: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for &cid in chunk_ids {
            let loc = self
                .toc
                .chunks
                .get(cid as usize)
                .ok_or(StatamError::Corrupt {
                    reason: "chunk_id out of bounds",
                })?;
            by_frame.entry(loc.frame_id).or_default().push(cid);
        }

        let mut frame_groups = Vec::with_capacity(by_frame.len());
        let mut ranges = Vec::with_capacity(by_frame.len());
        let mut needs_dict = false;
        for (fid, mut cids) in by_frame {
            cids.sort_unstable();
            cids.dedup();
            let entry = &self.toc.frames[fid as usize];
            if (entry.flags & PACK_FRAME_FLAG_USES_DICT) != 0 {
                needs_dict = true;
            }
            ranges.push(frame_byte_range(entry)?);
            frame_groups.push((fid, cids));
        }

        let dict = if needs_dict {
            self.load_decoder_dict()?
        } else {
            None
        };

        Ok((frame_groups, ranges, dict))
    }

    fn decompress_and_visit_frame<F>(
        &self,
        fid: u32,
        raw: &[u8],
        cids: &[u32],
        dict: Option<&PackDecoderDict>,
        mut visit: F,
    ) -> Result<(), StatamError>
    where
        F: FnMut(u32, &[u8]),
    {
        let entry = &self.toc.frames[fid as usize];
        verify_raw_frame_matches_entry(raw, entry)?;
        let (_, _, decoded) = read_framed_pack_frame_with_dict(raw, MAX_FRAME_RAW_BYTES, dict)?;
        if decoded.len() != entry.raw_len as usize {
            return Err(StatamError::Corrupt {
                reason: "decompressed frame length mismatch",
            });
        }
        for &cid in cids {
            let loc = &self.toc.chunks[cid as usize];
            let start = loc.offset_in_frame as usize;
            let end = start + loc.raw_len as usize;
            let slice = decoded.get(start..end).ok_or(StatamError::Corrupt {
                reason: "chunk slice out of decoded frame bounds",
            })?;
            visit(cid, slice);
        }
        Ok(())
    }

    /// Scans all requested unique `chunk_ids` in a single `read_ranges` wave, passing each
    /// chunk slice directly from the decompressed frame buffer with zero per-chunk clones.
    pub fn scan_chunks_in_frames<F>(
        &self,
        chunk_ids: &[u32],
        mut visit: F,
    ) -> Result<(), StatamError>
    where
        F: FnMut(u32, &[u8]),
    {
        if chunk_ids.is_empty() {
            return Ok(());
        }

        let (frame_groups, ranges, dict) = self.prepare_frame_chunk_groups(chunk_ids)?;
        let frame_bytes_list = self.reader.read_ranges(&ranges)?;
        if frame_bytes_list.len() != frame_groups.len() {
            return Err(StatamError::Corrupt {
                reason: "read_ranges returned wrong frame count",
            });
        }

        for ((fid, cids), raw) in frame_groups.iter().zip(frame_bytes_list.iter()) {
            self.decompress_and_visit_frame(*fid, raw, cids, dict.as_deref(), &mut visit)?;
        }

        Ok(())
    }

    /// Parallel variant of [`Self::scan_chunks_in_frames`], fetching all needed frames in one
    /// coalesced `read_ranges` wave and decompressing/visiting frames across up to `read_threads`
    /// workers (falling back to sequential execution when `read_threads <= 1` or `<= 1` frame).
    pub fn scan_chunks_in_frames_parallel<S, T, Init, F>(
        &self,
        chunk_ids: &[u32],
        read_threads: usize,
        init: Init,
        f: F,
    ) -> Result<Vec<T>, StatamError>
    where
        T: Send,
        Init: Fn() -> Result<S, StatamError> + Sync,
        F: Fn(&mut S, u32, &[u8]) -> Option<T> + Sync,
    {
        if chunk_ids.is_empty() {
            return Ok(Vec::new());
        }

        let (frame_groups, ranges, dict) = self.prepare_frame_chunk_groups(chunk_ids)?;
        let frame_bytes_list = self.reader.read_ranges(&ranges)?;
        if frame_bytes_list.len() != frame_groups.len() {
            return Err(StatamError::Corrupt {
                reason: "read_ranges returned wrong frame count",
            });
        }

        let threads = read_threads.clamp(1, MAX_WRITE_THREADS);
        if threads <= 1 || frame_groups.len() <= 1 {
            let mut state = init()?;
            let mut out = Vec::new();
            for ((fid, cids), raw) in frame_groups.iter().zip(frame_bytes_list.iter()) {
                self.decompress_and_visit_frame(*fid, raw, cids, dict.as_deref(), |cid, slice| {
                    if let Some(item) = f(&mut state, cid, slice) {
                        out.push(item);
                    }
                })?;
            }
            return Ok(out);
        }

        let frame_tasks: Vec<(u32, Bytes, Vec<u32>)> = frame_groups
            .into_iter()
            .zip(frame_bytes_list)
            .map(|((fid, cids), raw)| (fid, raw, cids))
            .collect();

        let per_frame = parallel_map_in_order_with_init(
            frame_tasks,
            threads,
            init,
            |state, (fid, raw, cids)| {
                let mut frame_out = Vec::new();
                self.decompress_and_visit_frame(
                    fid,
                    &raw,
                    &cids,
                    dict.as_deref(),
                    |cid, slice| {
                        if let Some(item) = f(state, cid, slice) {
                            frame_out.push(item);
                        }
                    },
                )?;
                Ok(frame_out)
            },
        )?;

        let total: usize = per_frame.iter().map(Vec::len).sum();
        let mut out = Vec::with_capacity(total);
        for mut items in per_frame {
            out.append(&mut items);
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

        if section.len() < TS02_HEADER_LEN || section[0..4] != TRIGRAM_SECTION_MAGIC {
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

fn validate_trailer_sections(
    ps: &PackPostscript,
    data_end: u64,
) -> Result<Vec<(u64, u64)>, StatamError> {
    let min_off = PACK_HEADER_LEN as u64;
    let mut sections: Vec<(u64, u64)> = Vec::with_capacity(4);
    let hash_bytes =
        (ps.chunk_count as u64)
            .checked_mul(16)
            .ok_or(StatamError::BoundsExceeded {
                reason: "hash table size overflow",
            })?;
    if ps.chunk_count > 0 {
        let end = check_pack_range(ps.hash_table_off, hash_bytes, min_off, data_end)?;
        sections.push((ps.hash_table_off, end));
    }
    if ps.trigram_len > 0 {
        if (ps.trigram_len as usize) > MAX_BLOOM_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "pack trigram_len exceeds MAX_BLOOM_BYTES",
            });
        }
        let end = check_pack_range(ps.trigram_off, u64::from(ps.trigram_len), min_off, data_end)?;
        sections.push((ps.trigram_off, end));
    }
    if ps.edge_table_len > 0 {
        if (ps.edge_table_len as usize) > MAX_BLOCK_RAW_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "pack edge_table_len exceeds MAX_BLOCK_RAW_BYTES",
            });
        }
        let end = check_pack_range(
            ps.edge_table_off,
            u64::from(ps.edge_table_len),
            min_off,
            data_end,
        )?;
        sections.push((ps.edge_table_off, end));
    }
    if (ps.flags & PACK_FLAG_HAS_DICT) != 0 {
        if ps.dict_len == 0 {
            return Err(StatamError::Corrupt {
                reason: "pack dict flag set with empty dict_len",
            });
        }
        if (ps.dict_len as usize) > MAX_PACK_DICT_BYTES + 1024 {
            return Err(StatamError::BoundsExceeded {
                reason: "pack dict_len exceeds MAX_PACK_DICT_BYTES + 1024",
            });
        }
        let end = check_pack_range(ps.dict_off, u64::from(ps.dict_len), min_off, data_end)?;
        sections.push((ps.dict_off, end));
    } else if ps.dict_len != 0 {
        return Err(StatamError::Corrupt {
            reason: "non-zero dict_len without PACK_FLAG_HAS_DICT",
        });
    }

    sections.sort_unstable_by_key(|&(start, _)| start);
    for w in sections.windows(2) {
        if w[0].1 > w[1].0 {
            return Err(StatamError::Corrupt {
                reason: "pack trailer sections overlap",
            });
        }
    }

    Ok(sections)
}

fn check_pack_range(off: u64, len: u64, min_off: u64, max_end: u64) -> Result<u64, StatamError> {
    let end = off.checked_add(len).ok_or(StatamError::BoundsExceeded {
        reason: "pack section range overflow",
    })?;
    if off < min_off || end > max_end {
        return Err(StatamError::Corrupt {
            reason: "pack section out of bounds",
        });
    }
    Ok(end)
}

fn validate_frame_entry(
    entry: &FrameEntry,
    ps: &PackPostscript,
    data_end: u64,
    trailer_sections: &[(u64, u64)],
) -> Result<(), StatamError> {
    if (entry.stored_len as usize) > MAX_FRAME_RAW_BYTES + 1024 {
        return Err(StatamError::BoundsExceeded {
            reason: "pack frame stored_len exceeds MAX_FRAME_RAW_BYTES + 1024",
        });
    }
    if entry.flags & !PACK_FRAME_FLAG_USES_DICT != 0 {
        return Err(StatamError::Corrupt {
            reason: "unknown pack frame flags",
        });
    }
    if entry.dict_id != 0 {
        return Err(StatamError::Corrupt {
            reason: "unsupported non-zero pack frame dict_id",
        });
    }
    if (entry.flags & PACK_FRAME_FLAG_USES_DICT) != 0 {
        if (ps.flags & PACK_FLAG_HAS_DICT) == 0 || ps.dict_len == 0 {
            return Err(StatamError::Corrupt {
                reason: "frame uses dict on pack without dict section",
            });
        }
        if entry.codec != Codec::Zstd {
            return Err(StatamError::Corrupt {
                reason: "pack frame dict flag requires zstd codec",
            });
        }
    }
    let total_len = (PACK_FRAME_HEADER_LEN as u64)
        .checked_add(u64::from(entry.stored_len))
        .ok_or(StatamError::BoundsExceeded {
            reason: "frame stored length overflow",
        })?;
    let frame_end = check_pack_range(entry.offset, total_len, PACK_HEADER_LEN as u64, data_end)?;
    for &(sec_start, sec_end) in trailer_sections {
        if entry.offset < sec_end && sec_start < frame_end {
            return Err(StatamError::Corrupt {
                reason: "pack frame overlaps trailer section",
            });
        }
    }

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
    let header_codec = Codec::from_u8(raw[2])?;
    let header_flags = raw[3];
    let header_raw_len = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
    let header_stored_len = u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]);
    let header_crc = u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]);
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
            write_threads: 1,
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
            target_frame_raw_bytes: 128 * 1024,
            enable_trigram_skip: true,
            compress_toc_lz4: false,
            write_threads: 1,
        };
        let mut b = ChunkPackBuilder::new([4u8; 16], opts);

        // Frame 0: tiny chunk (< 1024 B stored) -> filter_len must be 0.
        let tiny = b"tiny frame payload".repeat(20);
        b.intern_chunk(&tiny).unwrap();

        // Force flush into Frame 1 by adding a 130 KiB random chunk (saturates max 16 KiB bloom > 72%).
        let mut state: u64 = 0xdead_c0de_cafe_1234;
        let mut saturated = vec![0u8; 130 * 1024];
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

    #[test]
    fn ts02_trigram_section_uses_8kib_edge_blooms_and_reads_4kib_edge_blooms_dynamically() {
        let mut b = ChunkPackBuilder::new([12u8; 16], PackWriteOptions::default());
        assert_eq!(b.options.target_frame_raw_bytes, 1024 * 1024);

        // Emit enough distinct chunks so compressed frame >= 1024 B (`MIN_FRAME_FOR_TRIGRAM`)
        // while keeping total raw < 256 KiB (1 frame), trigram density < 72%, and
        // stored_frame_bytes < 2 MiB so TS02 uses 4 KiB edge blooms.
        for c in 0..180u32 {
            let mut chunk =
                format!("hdr_{c:04}_{:08x}_", c.wrapping_mul(2_654_435_761)).into_bytes();
            for j in 0..32u32 {
                chunk.extend_from_slice(
                    format!("token_{:02}_alpha_beta_gamma_", (c + j) % 25).as_bytes(),
                );
            }
            b.intern_chunk(&chunk).unwrap();
        }
        let mut pack_bytes = Vec::new();
        b.finish(&mut pack_bytes).unwrap();

        let r = ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes))).unwrap();
        assert_eq!(r.toc().frames.len(), 1);
        let (tail_b, head_b) = r.read_edge_trigram_blooms().unwrap().unwrap();
        assert_eq!(tail_b.as_bytes().len(), 4_096);
        assert_eq!(head_b.as_bytes().len(), 4_096);

        // Verify TS02 decoder also handles 8 KiB edge blooms dynamically (as emitted for >= 2 MiB packs).
        let mut toc_body = Vec::new();
        let mut fe = r.toc().frames[0];
        let old_filter = r.read_trigram_section().unwrap()[0].clone().unwrap();
        let mut ts_8k = Vec::new();
        ts_8k.extend_from_slice(&TRIGRAM_SECTION_MAGIC);
        ts_8k.extend_from_slice(&8192u32.to_le_bytes());
        ts_8k.extend_from_slice(&0u32.to_le_bytes());
        ts_8k.extend_from_slice(tail_b.as_bytes());
        ts_8k.extend_from_slice(&[0u8; 4096]);
        ts_8k.extend_from_slice(head_b.as_bytes());
        ts_8k.extend_from_slice(&[0u8; 4096]);
        fe.filter_off = (TS02_HEADER_LEN + 2 * 8192) as u32;
        fe.filter_len = old_filter.as_bytes().len() as u32;
        ts_8k.extend_from_slice(old_filter.as_bytes());

        toc_body.extend_from_slice(&fe.encode());
        for c in &r.toc().chunks {
            toc_body.extend_from_slice(&c.encode());
        }
        let mut ps = r.toc().postscript;
        let hash_bytes = (ps.chunk_count as usize) * 16;
        let hash_end = ps.hash_table_off as usize + hash_bytes;
        let mut crafted = vec![0u8; hash_end + ts_8k.len()];
        let mut hdr = PackHeader {
            format_version: FORMAT_VERSION,
            flags: ps.flags & !(PACK_FLAG_TOC_LZ4 | PACK_FLAG_TOC_ZSTD),
            required_features: 0,
            pack_id: ps.pack_id,
            enc_envelope_offset: 0,
            enc_envelope_len: 0,
            reserved: 0,
        };
        crafted[..PACK_HEADER_LEN].copy_from_slice(&hdr.encode());
        // Copy frame 0 bytes cleanly into crafted.
        let frame_total = PACK_FRAME_HEADER_LEN + fe.stored_len as usize;
        let frame_raw = r.read_frame_raw(0).unwrap();
        let frame_off = fe.offset as usize;
        crafted[frame_off..frame_off + frame_total].copy_from_slice(&frame_raw);
        let ts_off = hash_end;
        crafted[ts_off..ts_off + ts_8k.len()].copy_from_slice(&ts_8k);

        ps.flags = hdr.flags;
        ps.trigram_off = ts_off as u64;
        ps.trigram_len = ts_8k.len() as u32;
        ps.edge_table_off = 0;
        ps.edge_table_len = 0;
        ps.toc_raw_len = toc_body.len() as u32;
        ps.toc_stored_len = (toc_body.len() + PACK_POSTSCRIPT_LEN) as u32;
        ps.toc_crc32c = crc32c::crc32c(&toc_body);
        hdr.flags = ps.flags;
        crafted[..PACK_HEADER_LEN].copy_from_slice(&hdr.encode());
        crafted.extend_from_slice(&toc_body);
        crafted.extend_from_slice(&ps.encode());

        let r_8k = ChunkPackReader::open(Arc::new(SliceReader::new(crafted))).unwrap();
        let (t8, h8) = r_8k.read_edge_trigram_blooms().unwrap().unwrap();
        assert_eq!(t8.as_bytes().len(), 8192);
        assert_eq!(h8.as_bytes().len(), 8192);
    }

    #[cfg(not(miri))]
    #[test]
    fn pack_dict_training_triggers_at_2mib_skips_under_2mib_and_scans_zero_copy() {
        // 1. Under 2 MiB: must NOT emit DictSection or set PACK_FLAG_HAS_DICT.
        let mut small_builder =
            ChunkPackBuilder::new_with_dict([0x21u8; 16], PackWriteOptions::default());
        for i in 0..200u32 {
            let chunk = format!(
                "{{\"role\":\"user\",\"id\":{i:04},\"body\":\"{}\"}}",
                "small_pack_token_".repeat(40)
            );
            small_builder.intern_chunk(chunk.as_bytes()).unwrap();
        }
        let mut small_bytes = Vec::new();
        small_builder.finish(&mut small_bytes).unwrap();
        let small_reader = ChunkPackReader::open(Arc::new(SliceReader::new(small_bytes))).unwrap();
        assert_eq!(small_reader.postscript().flags & PACK_FLAG_HAS_DICT, 0);
        assert_eq!(small_reader.postscript().dict_len, 0);
        assert_eq!(small_reader.postscript().dict_off, 0);

        // 2. >= 2 MiB with `new_with_dict`: trains Zstd dictionary, sets PACK_FLAG_HAS_DICT, preserves chunk_id
        //    monotonicity, supports 0-extra-GET inline dict tail slice, read_chunks, and
        //    zero-copy scan_chunks_in_frames.
        let mut big_builder =
            ChunkPackBuilder::new_with_dict([0x22u8; 16], PackWriteOptions::default());
        let mut expected = Vec::new();
        let mut assigned_ids = Vec::new();
        for i in 0..1_200u32 {
            let chunk = format!(
                "{{\"role\":\"assistant\",\"model\":\"claude-3-7-sonnet\",\"span\":\"{i:06}\",\"payload\":\"alpha beta gamma delta epsilon {}\"}}",
                "structured_observability_json_segment_".repeat(52)
            )
            .into_bytes();
            let cid = big_builder.intern_chunk(&chunk).unwrap();
            assert_eq!(cid as usize, expected.len());
            assigned_ids.push(cid);
            expected.push(chunk);
        }
        // Also append one bypass chunk after dict training to verify mixed-mode chunk_id monotonicity.
        let bypass_chunk = vec![0x7Au8; 1024];
        let bypass_cid = big_builder.append_raw_bypass(&bypass_chunk).unwrap();
        assert_eq!(bypass_cid as usize, expected.len());
        expected.push(bypass_chunk);

        let mut big_bytes = Vec::new();
        big_builder.finish(&mut big_bytes).unwrap();

        let s3 = Arc::new(SimulatedS3Reader::from_bytes(
            big_bytes,
            SimulatedS3Config::default(),
        ));
        let reader = ChunkPackReader::open_with_tail_hint(s3.clone(), Some(512 * 1024)).unwrap();
        let ps = reader.postscript();
        assert_ne!(ps.flags & PACK_FLAG_HAS_DICT, 0);
        assert!(ps.dict_len > 0);
        assert!(ps.dict_off >= PACK_HEADER_LEN as u64);
        assert!(
            reader
                .toc()
                .frames
                .iter()
                .any(|f| (f.flags & PACK_FRAME_FLAG_USES_DICT) != 0)
        );
        assert!(reader.inline_dict_framed.is_some());

        // Because inline_dict_framed was sliced from tail, read_chunks takes strictly 1 wave!
        s3.reset_counters();
        let sample_cids: Vec<u32> = (0..expected.len() as u32).step_by(37).collect();
        let got = reader.read_chunks(&sample_cids).unwrap();
        assert_eq!(s3.counters().waves, 1);
        for &cid in &sample_cids {
            assert_eq!(got.get(&cid).unwrap(), &expected[cid as usize]);
        }

        // Verify scan_chunks_in_frames visits every unique requested chunk with zero clones.
        let mut visited = Vec::new();
        let mut dup_query = sample_cids.clone();
        dup_query.extend_from_slice(&sample_cids);
        reader
            .scan_chunks_in_frames(&dup_query, |cid, bytes| {
                visited.push((cid, bytes.to_vec()));
            })
            .unwrap();
        visited.sort_by_key(|(cid, _)| *cid);
        assert_eq!(visited.len(), sample_cids.len());
        for (cid, bytes) in visited {
            assert_eq!(bytes, expected[cid as usize]);
        }

        // Verify cold open with small tail hint (where dict is NOT in tail) falls back cleanly.
        let cold_reader = ChunkPackReader::open_with_tail_hint(s3, Some(1024)).unwrap();
        assert!(cold_reader.inline_dict_framed.is_none());
        let f0 = cold_reader.read_frame_decompressed(0).unwrap();
        assert!(!f0.is_empty());
    }

    #[test]
    fn pack_reader_rejects_corrupt_dict_trailer_and_frame_entries() {
        let mut b = ChunkPackBuilder::new([0x31u8; 16], PackWriteOptions::default());
        b.intern_chunk(&vec![b'a'; 1024]).unwrap();
        let mut bytes = Vec::new();
        b.finish(&mut bytes).unwrap();

        let ps_start = bytes.len() - PACK_POSTSCRIPT_LEN;
        let base_ps = PackPostscript::decode(&bytes[ps_start..]).unwrap();

        // 1. PACK_FLAG_HAS_DICT with dict_len == 0
        let mut bad1 = bytes.clone();
        let ps1 = PackPostscript {
            flags: base_ps.flags | PACK_FLAG_HAS_DICT,
            dict_len: 0,
            ..base_ps
        };
        bad1[ps_start..].copy_from_slice(&ps1.encode());
        assert!(matches!(
            ChunkPackReader::open(Arc::new(SliceReader::new(bad1))),
            Err(StatamError::Corrupt { .. })
        ));

        // 2. Non-zero dict_len without PACK_FLAG_HAS_DICT
        let mut bad2 = bytes.clone();
        let ps2 = PackPostscript {
            flags: base_ps.flags & !PACK_FLAG_HAS_DICT,
            dict_off: PACK_HEADER_LEN as u64,
            dict_len: 64,
            ..base_ps
        };
        bad2[ps_start..].copy_from_slice(&ps2.encode());
        assert!(matches!(
            ChunkPackReader::open(Arc::new(SliceReader::new(bad2))),
            Err(StatamError::Corrupt { .. })
        ));

        // 3. dict_len > MAX_PACK_DICT_BYTES + 1024 (use small tail hint so trailer check runs)
        let mut bad3 = bytes.clone();
        let ps3 = PackPostscript {
            flags: base_ps.flags | PACK_FLAG_HAS_DICT,
            dict_off: PACK_HEADER_LEN as u64,
            dict_len: (MAX_PACK_DICT_BYTES + 1025) as u32,
            ..base_ps
        };
        bad3[ps_start..].copy_from_slice(&ps3.encode());
        assert!(matches!(
            ChunkPackReader::open_with_tail_hint(Arc::new(SliceReader::new(bad3)), Some(128)),
            Err(StatamError::BoundsExceeded { .. })
        ));

        // 4. Overlapping DictSection and EdgeTable
        let mut bad_overlap = bytes.clone();
        let ps_overlap = PackPostscript {
            flags: base_ps.flags | PACK_FLAG_HAS_DICT,
            dict_off: base_ps.edge_table_off,
            dict_len: 16,
            ..base_ps
        };
        bad_overlap[ps_start..].copy_from_slice(&ps_overlap.encode());
        assert!(matches!(
            ChunkPackReader::open_with_tail_hint(
                Arc::new(SliceReader::new(bad_overlap)),
                Some(128)
            ),
            Err(StatamError::Corrupt { .. })
        ));

        // Helper to rewrite the TOC with a modified FrameEntry (uncompressed TOC).
        let r_ok = ChunkPackReader::open(Arc::new(SliceReader::new(bytes.clone()))).unwrap();
        let base_fe = r_ok.toc().frames[0];
        let base_chunks = r_ok.toc().chunks.clone();
        let data_end = bytes.len() - base_ps.toc_stored_len as usize;
        let rebuild_with_fe = |fe: FrameEntry, extra_flags: u16, dict_off: u64, dict_len: u32| {
            let mut toc_body = Vec::new();
            toc_body.extend_from_slice(&fe.encode());
            for c in &base_chunks {
                toc_body.extend_from_slice(&c.encode());
            }
            let mut out = bytes[..data_end].to_vec();
            let mut ps = PackPostscript {
                flags: (base_ps.flags & !(PACK_FLAG_TOC_LZ4 | PACK_FLAG_TOC_ZSTD)) | extra_flags,
                dict_off,
                dict_len,
                toc_raw_len: toc_body.len() as u32,
                toc_stored_len: (toc_body.len() + PACK_POSTSCRIPT_LEN) as u32,
                toc_crc32c: crc32c::crc32c(&toc_body),
                ..base_ps
            };
            if extra_flags == 0 {
                ps.dict_off = 0;
                ps.dict_len = 0;
            }
            let mut hdr = PackHeader::decode(&out[..PACK_HEADER_LEN]).unwrap();
            hdr.flags = ps.flags;
            out[..PACK_HEADER_LEN].copy_from_slice(&hdr.encode());
            out.extend_from_slice(&toc_body);
            out.extend_from_slice(&ps.encode());
            out
        };

        // 5. PACK_FRAME_FLAG_USES_DICT set without PACK_FLAG_HAS_DICT
        let pack_uses_dict_no_section = rebuild_with_fe(
            FrameEntry {
                flags: PACK_FRAME_FLAG_USES_DICT,
                codec: Codec::Zstd,
                ..base_fe
            },
            0,
            0,
            0,
        );
        assert!(matches!(
            ChunkPackReader::open(Arc::new(SliceReader::new(pack_uses_dict_no_section))),
            Err(StatamError::Corrupt { .. })
        ));

        // 6. Non-zero dict_id without PACK_FRAME_FLAG_USES_DICT
        let pack_nonzero_dict_id = rebuild_with_fe(
            FrameEntry {
                dict_id: 1,
                ..base_fe
            },
            0,
            0,
            0,
        );
        assert!(matches!(
            ChunkPackReader::open(Arc::new(SliceReader::new(pack_nonzero_dict_id))),
            Err(StatamError::Corrupt { .. })
        ));

        // 7. Non-zero dict_id WITH PACK_FRAME_FLAG_USES_DICT (place dummy non-overlapping dict before TOC)
        let mut dict_prefix = bytes[..data_end].to_vec();
        let mut dummy_dict_framed = Vec::new();
        write_framed_pack_frame(&[1, 2, 3, 4], Codec::None, 0, 0, &mut dummy_dict_framed).unwrap();
        let dummy_dict_off = dict_prefix.len() as u64;
        let dummy_dict_len = dummy_dict_framed.len() as u32;
        dict_prefix.extend_from_slice(&dummy_dict_framed);
        let rebuild_with_dict_prefix = |fe: FrameEntry| {
            let mut toc_body = Vec::new();
            toc_body.extend_from_slice(&fe.encode());
            for c in &base_chunks {
                toc_body.extend_from_slice(&c.encode());
            }
            let mut out = dict_prefix.clone();
            let ps = PackPostscript {
                flags: (base_ps.flags & !(PACK_FLAG_TOC_LZ4 | PACK_FLAG_TOC_ZSTD))
                    | PACK_FLAG_HAS_DICT,
                dict_off: dummy_dict_off,
                dict_len: dummy_dict_len,
                toc_raw_len: toc_body.len() as u32,
                toc_stored_len: (toc_body.len() + PACK_POSTSCRIPT_LEN) as u32,
                toc_crc32c: crc32c::crc32c(&toc_body),
                ..base_ps
            };
            let mut hdr = PackHeader::decode(&out[..PACK_HEADER_LEN]).unwrap();
            hdr.flags = ps.flags;
            out[..PACK_HEADER_LEN].copy_from_slice(&hdr.encode());
            out.extend_from_slice(&toc_body);
            out.extend_from_slice(&ps.encode());
            out
        };
        let pack_dict_id_with_flag = rebuild_with_dict_prefix(FrameEntry {
            flags: PACK_FRAME_FLAG_USES_DICT,
            codec: Codec::Zstd,
            dict_id: 7,
            ..base_fe
        });
        assert!(matches!(
            ChunkPackReader::open(Arc::new(SliceReader::new(pack_dict_id_with_flag))),
            Err(StatamError::Corrupt { .. })
        ));

        // 8. PACK_FRAME_FLAG_USES_DICT with codec != Codec::Zstd
        let pack_dict_bad_codec = rebuild_with_dict_prefix(FrameEntry {
            flags: PACK_FRAME_FLAG_USES_DICT,
            codec: Codec::Lz4,
            ..base_fe
        });
        assert!(matches!(
            ChunkPackReader::open(Arc::new(SliceReader::new(pack_dict_bad_codec))),
            Err(StatamError::Corrupt { .. })
        ));

        // 9. FrameEntry vs PF header flags mismatch
        let frame_off = base_fe.offset as usize;
        let mut bad_pf_flags = bytes.clone();
        bad_pf_flags[frame_off + 3] = PACK_FRAME_FLAG_USES_DICT;
        let r_bad_pf_flags =
            ChunkPackReader::open(Arc::new(SliceReader::new(bad_pf_flags))).unwrap();
        assert!(matches!(
            r_bad_pf_flags.read_chunks(&[0]),
            Err(StatamError::Corrupt { .. })
        ));

        // 10. FrameEntry vs PF header codec mismatch
        let mut bad_pf_codec = bytes.clone();
        bad_pf_codec[frame_off + 2] = if base_fe.codec == Codec::Lz4 {
            Codec::Zstd as u8
        } else {
            Codec::Lz4 as u8
        };
        let r_bad_pf_codec =
            ChunkPackReader::open(Arc::new(SliceReader::new(bad_pf_codec))).unwrap();
        assert!(matches!(
            r_bad_pf_codec.read_chunks(&[0]),
            Err(StatamError::Corrupt { .. })
        ));

        // 11. Unknown PF frame flags rejected by read_framed_pack_frame_with_dict
        let mut framed_unknown = Vec::new();
        write_framed_pack_frame(b"hello", Codec::None, 0, 0, &mut framed_unknown).unwrap();
        framed_unknown[3] = 0x02;
        assert!(matches!(
            read_framed_pack_frame_with_dict(&framed_unknown, MAX_FRAME_RAW_BYTES, None),
            Err(StatamError::Corrupt { .. })
        ));

        // 12. DictSection frame with unexpected flags rejected on load_decoder_dict
        let mut bad_dict_flags = dict_prefix;
        bad_dict_flags[dummy_dict_off as usize + 3] = PACK_FRAME_FLAG_USES_DICT;
        let mut toc_body = Vec::new();
        toc_body.extend_from_slice(
            &FrameEntry {
                flags: PACK_FRAME_FLAG_USES_DICT,
                codec: Codec::Zstd,
                ..base_fe
            }
            .encode(),
        );
        for c in &base_chunks {
            toc_body.extend_from_slice(&c.encode());
        }
        let ps_bad_dict = PackPostscript {
            flags: (base_ps.flags & !(PACK_FLAG_TOC_LZ4 | PACK_FLAG_TOC_ZSTD)) | PACK_FLAG_HAS_DICT,
            dict_off: dummy_dict_off,
            dict_len: dummy_dict_len,
            toc_raw_len: toc_body.len() as u32,
            toc_stored_len: (toc_body.len() + PACK_POSTSCRIPT_LEN) as u32,
            toc_crc32c: crc32c::crc32c(&toc_body),
            ..base_ps
        };
        let mut hdr_bad_dict = PackHeader::decode(&bad_dict_flags[..PACK_HEADER_LEN]).unwrap();
        hdr_bad_dict.flags = ps_bad_dict.flags;
        bad_dict_flags[..PACK_HEADER_LEN].copy_from_slice(&hdr_bad_dict.encode());
        bad_dict_flags.extend_from_slice(&toc_body);
        bad_dict_flags.extend_from_slice(&ps_bad_dict.encode());
        let r_bad_dict = ChunkPackReader::open(Arc::new(SliceReader::new(bad_dict_flags))).unwrap();
        assert!(matches!(
            r_bad_dict.read_chunks(&[0]),
            Err(StatamError::Corrupt { .. })
        ));
    }

    #[test]
    fn pack_multithreaded_is_bit_exact_with_single_threaded() {
        let pack_id = [0xE1u8; 16];

        // 1. Mixed normal frames (with trigram blooms + dedup + 1 MiB guard) + bypass frames.
        #[cfg(miri)]
        let (
            mixed_target,
            mixed_chunks,
            mixed_chunk_len,
            mixed_bypass,
            mixed_bypass_len,
            min_frames,
        ) = (2 * 1024, 12usize, 512usize, 3usize, 512usize, 4usize);
        #[cfg(not(miri))]
        let (
            mixed_target,
            mixed_chunks,
            mixed_chunk_len,
            mixed_bypass,
            mixed_bypass_len,
            min_frames,
        ) = (16 * 1024, 80usize, 4 * 1024, 12usize, 8 * 1024, 10usize);

        let build_mixed = |threads: usize| -> (Vec<u8>, PackWriteStats) {
            let opts = PackWriteOptions {
                codec: test_codec(),
                zstd_level: 3,
                target_frame_raw_bytes: mixed_target,
                enable_trigram_skip: true,
                compress_toc_lz4: true,
                write_threads: threads,
            };
            let mut pb = ChunkPackBuilder::new(pack_id, opts);
            let mut state: u64 = 0x1234_5678_9abc_def0;
            for i in 0..mixed_chunks {
                let mut chunk = Vec::with_capacity(mixed_chunk_len);
                let prefix = format!("{{\"msg\":\"item-{i:04}-payload\",\"body\":\"");
                chunk.extend_from_slice(prefix.as_bytes());
                while chunk.len() < mixed_chunk_len {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    chunk.push(b'a' + ((state >> 24) as u8 % 8));
                }
                let id = pb.intern_chunk(&chunk).unwrap();
                if i % 4 == 0 {
                    let dup_id = pb.intern_chunk(&chunk).unwrap();
                    assert_eq!(dup_id, id);
                }
            }
            for b in 0..mixed_bypass {
                let mut bypass = vec![0u8; mixed_bypass_len];
                for byte in &mut bypass {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    *byte = (state >> 24) as u8;
                }
                bypass[0] = b as u8;
                pb.append_raw_bypass(&bypass).unwrap();
            }
            let mut out = Vec::new();
            let stats = pb.finish(&mut out).unwrap();
            (out, stats)
        };

        let (bytes_0t, stats_0t) = build_mixed(0);
        let (bytes_1t, stats_1t) = build_mixed(1);
        let (bytes_4t, stats_4t) = build_mixed(4);
        assert_eq!(stats_1t, stats_0t);
        assert_eq!(
            bytes_1t, bytes_0t,
            "mixed normal+bypass pack must be bit-exact between 1T and 0T"
        );
        assert_eq!(stats_1t, stats_4t);
        assert_eq!(
            bytes_1t, bytes_4t,
            "mixed normal+bypass pack must be bit-exact between 1T and 4T"
        );
        let reader = ChunkPackReader::open(Arc::new(SliceReader::new(bytes_4t))).unwrap();
        assert!(reader.toc().frames.len() >= min_frames);

        // 1b. Interleaving append_raw_frame while pending_frames is non-empty.
        let hashes_src = reader.read_hash_table().unwrap();
        let trigram_src = reader.read_trigram_section().unwrap();
        let edges_src = reader.read_edge_table().unwrap();
        let f0_entry = reader.toc().frames[0];
        let raw_f0 = reader.read_frame_raw(0).unwrap();
        let f0_first = f0_entry.first_chunk_id as usize;
        let f0_cnt = f0_entry.chunk_count as usize;
        let f0_meta: Vec<([u8; 16], u32, u32)> = (f0_first..f0_first + f0_cnt)
            .map(|cid| {
                let loc = reader.toc().chunks[cid];
                (hashes_src[cid], loc.offset_in_frame, loc.raw_len)
            })
            .collect();
        let f0_filter = trigram_src[0].as_ref().map(|b| b.as_bytes()).unwrap_or(&[]);
        let f0_edges = &edges_src[f0_first..f0_first + f0_cnt];

        let build_interleaved = |threads: usize| -> (Vec<u8>, PackWriteStats) {
            let opts = PackWriteOptions {
                codec: test_codec(),
                zstd_level: 3,
                target_frame_raw_bytes: 16 * 1024,
                enable_trigram_skip: true,
                compress_toc_lz4: true,
                write_threads: threads,
            };
            let mut pb = ChunkPackBuilder::new(pack_id, opts);
            for i in 0..10usize {
                let c = format!("{{\"pre_append\":{i:04},\"pad\":\"{}\"}}", "P".repeat(4096));
                pb.intern_chunk(c.as_bytes()).unwrap();
            }
            if threads > 1 {
                assert!(!pb.pending_frames.is_empty());
            }
            pb.append_raw_frame(&raw_f0, f0_entry, &f0_meta, f0_filter, f0_edges)
                .unwrap();
            assert!(pb.pending_frames.is_empty());
            for i in 0..10usize {
                let c = format!(
                    "{{\"post_append\":{i:04},\"pad\":\"{}\"}}",
                    "Q".repeat(4096)
                );
                pb.intern_chunk(c.as_bytes()).unwrap();
            }
            let mut out = Vec::new();
            let stats = pb.finish(&mut out).unwrap();
            (out, stats)
        };
        let (inter_0t, inter_s0) = build_interleaved(0);
        let (inter_1t, inter_s1) = build_interleaved(1);
        let (inter_4t, inter_s4) = build_interleaved(4);
        assert_eq!(inter_s1, inter_s0);
        assert_eq!(inter_1t, inter_0t);
        assert_eq!(inter_s1, inter_s4);
        assert_eq!(
            inter_1t, inter_4t,
            "interleaved append_raw_frame with pending_frames must be bit-exact 1T vs 4T"
        );

        #[cfg(not(miri))]
        {
            // 2. new_with_dict under 2 MiB (flush_pre_dict_without_dict).
            let build_dict_pack = |threads: usize, count: usize| -> (Vec<u8>, PackWriteStats) {
                let opts = PackWriteOptions {
                    codec: test_codec(),
                    zstd_level: 3,
                    target_frame_raw_bytes: 128 * 1024,
                    enable_trigram_skip: true,
                    compress_toc_lz4: true,
                    write_threads: threads,
                };
                let mut pb = ChunkPackBuilder::new_with_dict(pack_id, opts);
                for i in 0..count {
                    let payload = format!(
                        "{{\"idx\":{i:06},\"role\":\"assistant\",\"model\":\"claude-test\",\
                     \"content\":\"Repeated structured JSON payload prefix for dict training \
                     and multi-frame compression verification iteration={i:06} suffix_padding_0123456789\"}}"
                    );
                    let repeated = payload.repeat(12);
                    pb.intern_chunk(repeated.as_bytes()).unwrap();
                }
                let mut out = Vec::new();
                let stats = pb.finish(&mut out).unwrap();
                (out, stats)
            };

            let (small_1t, small_s1) = build_dict_pack(1, 200);
            let (small_4t, small_s4) = build_dict_pack(4, 200);
            assert_eq!(small_s1, small_s4);
            assert_eq!(
                small_1t, small_4t,
                "pre-dict <2MiB pack must be bit-exact between 1T and 4T"
            );

            // 3. new_with_dict over 2 MiB (triggers dict training + multi-worker PackDictCompressor).
            let (dict_1t, dict_s1) = build_dict_pack(1, 2_200);
            let (dict_4t, dict_s4) = build_dict_pack(4, 2_200);
            assert_eq!(dict_s1, dict_s4);
            assert_eq!(
                dict_1t, dict_4t,
                "dict-trained >2MiB pack must be bit-exact between 1T and 4T"
            );
            let dict_reader = ChunkPackReader::open(Arc::new(SliceReader::new(dict_4t))).unwrap();
            assert!(dict_reader.toc().frames.len() >= 16);
            let all_ids: Vec<u32> = (0..dict_reader.toc().chunks.len() as u32).collect();
            let got = dict_reader.read_chunks(&all_ids).unwrap();
            assert_eq!(got.len(), all_ids.len());
        }
    }
}
