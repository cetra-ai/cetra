//! Immutable content-addressed `.stpk` chunk pack builder and reader.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::hash::BuildHasherDefault;
use std::io::{Cursor, Seek, SeekFrom, Write};
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};

use bytes::Bytes;

use crate::StatamError;
use crate::format::{
    BLOB_DEDICATED_FRAME_BYTES, BLOB_MAX_CHUNK_BYTES, BLOB_SMALL_FRAME_TARGET_BYTES, CHUNK_LOC_LEN,
    ChunkLoc, Codec, FORMAT_VERSION, FRAME_ENTRY_LEN, FrameEntry, FramedWriteInfo,
    MAX_BATCH_DECODED_BYTES, MAX_BLOCK_RAW_BYTES, MAX_BLOOM_BYTES, MAX_FRAME_RAW_BYTES,
    MAX_HOT_TAIL_BYTES, MAX_PACK_CHUNKS, MAX_PACK_DICT_BYTES, MAX_PACK_FRAMES,
    MAX_ROW_PAYLOAD_BYTES, PACK_FLAG_HAS_DICT, PACK_FLAG_HAS_TRIGRAM, PACK_FLAG_TOC_LZ4,
    PACK_FLAG_TOC_ZSTD, PACK_FRAME_FLAG_USES_DICT, PACK_FRAME_HEADER_LEN, PACK_FRAME_MAGIC,
    PACK_HEADER_LEN, PACK_POSTSCRIPT_LEN, PackDecoderDict, PackDictCompressor,
    PackFrameDecompressor, PackHeader, PackPostscript, compress_guarded, decompress_checked,
    pack_frame_crc, read_framed_pack_frame, read_framed_pack_frame_with_decompressor,
    read_framed_pack_frame_with_dict, write_framed_pack_frame,
    write_framed_pack_frame_with_compressor_min_savings, write_framed_pack_frame_with_limit,
};
use crate::io::{RangeReader, S3_CACHE_BLOCK_BYTES};
use crate::parallel::{
    MAX_PARALLEL_THREADS, parallel_map_in_order_static_with_ctx, parallel_map_in_order_with_init,
};
use crate::simd::trigram::{MAX_TRIGRAM_BLOOM_BYTES, MIN_TRIGRAM_BLOOM_BYTES, TrigramBloom};

/// Maximum head/tail edge bytes stored per chunk in `EdgeTable`.
pub const EDGE_LEN: usize = 63;

/// `(head_63, tail_63)` edge bytes for one chunk.
pub type ChunkEdge = (Vec<u8>, Vec<u8>);

/// Compact, zero-per-chunk-allocation representation of a pack's `EdgeTable`.
#[derive(Clone)]
pub struct FlatEdgeTable {
    head_data: Vec<u8>,
    head_offs: Vec<u32>,
    tail_data: Vec<u8>,
    tail_offs: Vec<u32>,
}

impl fmt::Debug for FlatEdgeTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlatEdgeTable")
            .field("chunk_count", &self.len())
            .field("head_bytes", &self.head_data.len())
            .field("tail_bytes", &self.tail_data.len())
            .finish()
    }
}

impl FlatEdgeTable {
    #[inline]
    pub fn len(&self) -> usize {
        self.head_offs.len().saturating_sub(1)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn head(&self, cid: u32) -> &[u8] {
        let idx = cid as usize;
        if idx.checked_add(1).is_none_or(|n| n >= self.head_offs.len()) {
            return &[];
        }
        &self.head_data[self.head_offs[idx] as usize..self.head_offs[idx + 1] as usize]
    }

    #[inline]
    pub fn tail(&self, cid: u32) -> &[u8] {
        let idx = cid as usize;
        if idx.checked_add(1).is_none_or(|n| n >= self.tail_offs.len()) {
            return &[];
        }
        &self.tail_data[self.tail_offs[idx] as usize..self.tail_offs[idx + 1] as usize]
    }
}

/// Magic bytes at offset 0 of a `TS02` TrigramSkipSection header.
pub const TRIGRAM_SECTION_MAGIC: [u8; 4] = *b"TS02";
/// Fixed 12-byte header size preceding the two edge trigram blooms in `TS02`.
const TS02_HEADER_LEN: usize = 12;

/// Minimum compressed frame length required to emit a trigram bloom (`< 1024` omitted).
const MIN_FRAME_FOR_TRIGRAM: usize = 1024;
/// Maximum trigram bloom bit density (`> 72%` saturated blooms are omitted).
const MAX_TRIGRAM_DENSITY: f64 = 0.72;
/// Trigram density threshold for non-max blooms indicating high-entropy prose frames.
const HIGH_TRIGRAM_DENSITY_THRESHOLD: f64 = 0.55;
/// Trigram density threshold for max-sized (`16 KiB`, `k = 2`) blooms (`> 8,300` distinct trigrams).
const HIGH_TRIGRAM_MAX_BLOOM_DENSITY: f64 = 0.12;
/// Adapted target raw bytes per `.stpk` frame for unique high-entropy prose streams (64 KiB).
const ADAPTIVE_PROSE_FRAME_RAW_BYTES: usize = 64 * 1024;
/// Minimum compression savings required for binary blob frames before falling back to `Codec::None`.
const BLOB_MIN_SAVINGS_PCT: u8 = 12;
/// Maximum gap (`192 KiB`) between `.stpk` frames coalesced into one remote range read.
const REMOTE_PACK_COALESCE_MAX_GAP: u64 = 192 * 1024;
/// Maximum total span (`4 MiB`) of a single coalesced remote `.stpk` range read.
const REMOTE_PACK_COALESCE_MAX_SPAN: u64 = 4 * 1024 * 1024;
/// 1 MiB `ReadCache` boundary guard threshold: pad if `< 8 KiB` left in current 1 MiB block.
const BOUNDARY_GUARD_THRESHOLD: usize = 8_192;
/// 1 MiB `ReadCache` boundary guard max frame size (`< 32 KiB`).
const BOUNDARY_GUARD_MAX_FRAME: usize = 32_768;
/// Default target raw bytes per `.stpk` frame (512 KiB).
const DEFAULT_TARGET_FRAME_RAW_BYTES: usize = 512 * 1024;
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
/// Default tail prefetch size for `.stpk` chunk packs (256 KiB), capturing TOC + dict + edge table + trigram section in 1 GET.
const PACK_DEFAULT_TAIL_HINT: u32 = 256 * 1024;
/// Maximum `.stpk` file length (`4 MiB`) eligible for the adaptive remote `384 KiB` tail prefetch.
const PACK_REMOTE_ADAPTIVE_TAIL_MAX_FILE_LEN: u64 = 4 * 1024 * 1024;
/// Adaptive tail prefetch size (`384 KiB`) used on remote readers for small `.stpk` packs (`<= 4 MiB`).
const PACK_REMOTE_SMALL_TAIL_HINT: u32 = 384 * 1024;

/// Configuration options for [`ChunkPackBuilder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackWriteOptions {
    /// Compression codec for pack frames (default: `Codec::Zstd`, or `Codec::Lz4` under Miri).
    pub codec: Codec,
    /// Zstd compression level when `codec == Codec::Zstd` (default: `3`).
    pub zstd_level: i32,
    /// Target uncompressed frame size in bytes (default: `512 KiB`).
    pub target_frame_raw_bytes: usize,
    /// Whether to emit adaptive per-frame trigram skip filters (default: `true`).
    pub enable_trigram_skip: bool,
    /// Whether to compress the Pack TOC body (`Zstd` delta-coded or `Lz4`) when smaller (default: `true`).
    pub compress_toc_lz4: bool,
    /// Number of worker threads for parallel frame compression and trigram building (default: `1`).
    pub write_threads: usize,
    /// Whether to emit the compaction-only `ChunkHashTable` (default: `true`).
    pub emit_hash_table: bool,
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
            emit_hash_table: true,
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

pub struct ChunkPackBuilder<W: Write + Seek = Cursor<Vec<u8>>> {
    pack_id: [u8; 16],
    options: PackWriteOptions,
    /// Streaming sink receiving the 48-byte header placeholder, framed data, padding, and tail.
    out: W,
    /// Initial stream position in `out` where the 48-byte `PackHeader` placeholder was written.
    start_offset: u64,
    /// Bytes written to `out` relative to `start_offset` (starts at `PACK_HEADER_LEN`).
    written_len: u64,
    dedup: HashMap<[u8; 16], u32, BuildHasherDefault<Hash16IdentityHasher>>,
    frames: Vec<FrameEntry>,
    chunk_locs: Vec<ChunkLoc>,
    hashes: Vec<[u8; 16]>,
    edges: Vec<(InlineEdge, InlineEdge)>,
    trigram_buf: Vec<u8>,
    cur_raw: Vec<u8>,
    cur_chunks: Vec<PendingChunk>,
    cur_is_bypass: bool,
    cur_min_savings_pct: u8,
    effective_target_frame_raw_bytes: usize,
    frame_size_decided: bool,
    observed_non_bypass_raw: u64,
    observed_non_bypass_chunks: u64,
    observed_trigram_frames: u32,
    observed_high_density_frames: u32,
    enable_dict: bool,
    dict_decided: bool,
    pre_dict_frames: Vec<(Vec<u8>, Vec<PendingChunk>)>,
    pre_dict_raw_bytes: usize,
    pre_dict_chunk_count: usize,
    pending_frames: Vec<(Vec<u8>, Vec<PendingChunk>, bool, u8)>,
    pending_frame_chunks: usize,
    pending_frame_raw_bytes: usize,
    trained_dict: Option<Vec<u8>>,
    dict_compressor: Option<PackDictCompressor>,
    used_dict_in_any_frame: bool,
    dedup_hits: u64,
    raw_chunk_bytes: u64,
    stored_frame_bytes: u64,
    prefix_cache: Vec<Option<PackPrefixEntry>>,
    track_boundary_chunks: bool,
    has_any_boundary: bool,
    boundary_chunks: Vec<bool>,
}

impl<W: Write + Seek> fmt::Debug for ChunkPackBuilder<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkPackBuilder")
            .field("options", &self.options)
            .field("written_len", &self.written_len)
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
            .field("track_boundary_chunks", &self.track_boundary_chunks)
            .field("has_any_boundary", &self.has_any_boundary)
            .finish()
    }
}

impl ChunkPackBuilder<Cursor<Vec<u8>>> {
    /// Creates a new in-memory `.stpk` pack builder for `pack_id` (Zstd dictionary training disabled by default).
    pub fn new(pack_id: [u8; 16], options: PackWriteOptions) -> Self {
        let mut out = Cursor::new(vec![0u8; PACK_HEADER_LEN]);
        out.set_position(PACK_HEADER_LEN as u64);
        Self::new_inner(pack_id, options, false, out, 0)
    }

    /// Creates a new in-memory `.stpk` pack builder for `pack_id` with opt-in Zstd dictionary training
    /// enabled whenever `options.codec == Codec::Zstd` under non-Miri builds.
    pub fn new_with_dict(pack_id: [u8; 16], options: PackWriteOptions) -> Self {
        let enable_dict = default_dict_enabled(options.codec);
        let mut out = Cursor::new(vec![0u8; PACK_HEADER_LEN]);
        out.set_position(PACK_HEADER_LEN as u64);
        Self::new_inner(pack_id, options, enable_dict, out, 0)
    }

    /// Finalizes and writes the complete `.stpk` file into `out`.
    pub fn finish<Out: Write>(self, mut out: Out) -> Result<PackWriteStats, StatamError> {
        let (cursor, stats) = self.finish_into_inner()?;
        out.write_all(cursor.get_ref())?;
        Ok(stats)
    }
}

impl<W: Write + Seek> ChunkPackBuilder<W> {
    /// Creates a new streaming `.stpk` pack builder writing each finished frame directly to `out`.
    ///
    /// Writes a 48-byte `PackHeader` placeholder at `out.stream_position()?` that
    /// [`Self::finish_into_inner`] patches in place once all frames and trailer sections are written.
    pub fn new_streaming(
        pack_id: [u8; 16],
        options: PackWriteOptions,
        mut out: W,
    ) -> Result<Self, StatamError> {
        let start_offset = out.stream_position()?;
        out.write_all(&[0u8; PACK_HEADER_LEN])?;
        Ok(Self::new_inner(pack_id, options, false, out, start_offset))
    }

    fn new_inner(
        pack_id: [u8; 16],
        mut options: PackWriteOptions,
        enable_dict: bool,
        out: W,
        start_offset: u64,
    ) -> Self {
        options.target_frame_raw_bytes =
            options.target_frame_raw_bytes.clamp(1, MAX_FRAME_RAW_BYTES);
        Self {
            pack_id,
            options,
            out,
            start_offset,
            written_len: PACK_HEADER_LEN as u64,
            dedup: HashMap::default(),
            frames: Vec::new(),
            chunk_locs: Vec::new(),
            hashes: Vec::new(),
            edges: Vec::new(),
            trigram_buf: Vec::new(),
            cur_raw: Vec::new(),
            cur_chunks: Vec::new(),
            cur_is_bypass: false,
            cur_min_savings_pct: 0,
            effective_target_frame_raw_bytes: options.target_frame_raw_bytes,
            frame_size_decided: false,
            observed_non_bypass_raw: 0,
            observed_non_bypass_chunks: 0,
            observed_trigram_frames: 0,
            observed_high_density_frames: 0,
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
            track_boundary_chunks: false,
            has_any_boundary: false,
            boundary_chunks: Vec::new(),
        }
    }

    /// Returns the total number of unique chunks currently recorded in this pack
    /// (including chunks buffered in the active or pending frames).
    pub fn chunk_count(&self) -> u32 {
        let total = self.chunk_locs.len()
            + self.pre_dict_chunk_count
            + self.pending_frame_chunks
            + self.cur_chunks.len();
        u32::try_from(total).unwrap_or(u32::MAX)
    }

    /// Enables boundary-chunk tracking so single-chunk-only packs omit `EdgeTable`
    /// and mixed packs zero non-boundary chunk edges.
    pub fn enable_boundary_tracking(&mut self) {
        if !self.track_boundary_chunks {
            let existing = self.chunk_locs.len()
                + self.pre_dict_chunk_count
                + self.pending_frame_chunks
                + self.cur_chunks.len();
            if existing > 0 {
                self.has_any_boundary = true;
                self.boundary_chunks.fill(true);
                self.boundary_chunks.resize(existing, true);
            }
            self.track_boundary_chunks = true;
        }
    }

    /// Marks all `chunk_ids` as belonging to a multi-chunk row (`chunk_ids.len() > 1`).
    pub fn mark_boundary_chunks(&mut self, chunk_ids: &[u32]) {
        if chunk_ids.len() <= 1 {
            return;
        }
        self.has_any_boundary = true;
        for &cid in chunk_ids {
            let idx = cid as usize;
            if idx >= MAX_PACK_CHUNKS {
                continue;
            }
            if idx >= self.boundary_chunks.len() {
                self.boundary_chunks.resize(idx + 1, false);
            }
            self.boundary_chunks[idx] = true;
        }
    }

    /// Unconditionally marks a single `chunk_id` as belonging to a multi-chunk row.
    pub(crate) fn mark_boundary_chunk(&mut self, chunk_id: u32) {
        self.has_any_boundary = true;
        let idx = chunk_id as usize;
        if idx >= MAX_PACK_CHUNKS {
            return;
        }
        if idx >= self.boundary_chunks.len() {
            self.boundary_chunks.resize(idx + 1, false);
        }
        self.boundary_chunks[idx] = true;
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
        if (self.cur_is_bypass || self.cur_min_savings_pct != 0) && !self.cur_chunks.is_empty() {
            self.flush_current_frame()?;
        }
        self.cur_is_bypass = false;
        self.cur_min_savings_pct = 0;

        let hash = blake3::hash(bytes);
        let mut hash16 = [0u8; 16];
        hash16.copy_from_slice(&hash.as_bytes()[..16]);

        if let Some(&existing_id) = self.dedup.get(&hash16) {
            if !bytes.is_empty()
                && let Some((head, tail)) = self.edges.get_mut(existing_id as usize)
                && head.len == 0
                && tail.len == 0
            {
                *head = InlineEdge::from_slice(&bytes[..bytes.len().min(EDGE_LEN)]);
                *tail = InlineEdge::from_slice(&bytes[bytes.len().saturating_sub(EDGE_LEN)..]);
            }
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

        let target = self.effective_target_frame_raw_bytes.max(1);
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
        if (!self.cur_is_bypass || self.cur_min_savings_pct != 0) && !self.cur_chunks.is_empty() {
            self.flush_current_frame()?;
        }
        if !self.dict_decided {
            self.dict_decided = true;
        }
        if !self.pre_dict_frames.is_empty() {
            self.flush_pre_dict_without_dict()?;
        }
        self.cur_is_bypass = true;
        self.cur_min_savings_pct = 0;

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

    /// Appends a binary blob using bypass framing (`min_savings_pct = 12`, no CDC/dedup/trigrams/edges).
    ///
    /// Blobs `< BLOB_DEDICATED_FRAME_BYTES` (`32 KiB`) batch into `BLOB_SMALL_FRAME_TARGET_BYTES`
    /// (`64 KiB`) frames. Blobs `>= 32 KiB` are split into slices of at most `BLOB_MAX_CHUNK_BYTES`
    /// (`4 MiB`), each emitted in its own dedicated 1-chunk `PF` frame for single-GET random access.
    pub fn append_blob(&mut self, bytes: &[u8]) -> Result<Vec<u32>, StatamError> {
        if bytes.len() > MAX_ROW_PAYLOAD_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "blob payload exceeds MAX_ROW_PAYLOAD_BYTES",
            });
        }

        if bytes.len() < BLOB_DEDICATED_FRAME_BYTES {
            if (!self.cur_is_bypass || self.cur_min_savings_pct != BLOB_MIN_SAVINGS_PCT)
                && !self.cur_chunks.is_empty()
            {
                self.flush_current_frame()?;
            }
            if !self.dict_decided {
                self.dict_decided = true;
            }
            if !self.pre_dict_frames.is_empty() {
                self.flush_pre_dict_without_dict()?;
            }
            self.cur_is_bypass = true;
            self.cur_min_savings_pct = BLOB_MIN_SAVINGS_PCT;

            let total_chunks = self.chunk_locs.len()
                + self.pre_dict_chunk_count
                + self.pending_frame_chunks
                + self.cur_chunks.len();
            if total_chunks >= MAX_PACK_CHUNKS {
                return Err(StatamError::BoundsExceeded {
                    reason: "pack chunk count exceeds MAX_PACK_CHUNKS",
                });
            }
            let chunk_id =
                u32::try_from(total_chunks).map_err(|_| StatamError::BoundsExceeded {
                    reason: "chunk_id overflows u32",
                })?;

            if !self.cur_raw.is_empty()
                && self.cur_raw.len().saturating_add(bytes.len()) > BLOB_SMALL_FRAME_TARGET_BYTES
            {
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
            self.cur_chunks.push(PendingChunk {
                hash16,
                offset_in_frame,
                raw_len,
                head: InlineEdge::empty(),
                tail: InlineEdge::empty(),
            });
            self.raw_chunk_bytes = self.raw_chunk_bytes.saturating_add(bytes.len() as u64);

            if self.cur_raw.len() >= BLOB_SMALL_FRAME_TARGET_BYTES {
                self.flush_current_frame()?;
            }
            return Ok(vec![chunk_id]);
        }

        // Large blob (>= 32 KiB): flush any open frame/pre-dict frames first, then emit
        // one dedicated 1-chunk bypass PF frame per <= 4 MiB slice.
        if !self.cur_chunks.is_empty() {
            self.flush_current_frame()?;
        }
        if !self.dict_decided {
            self.dict_decided = true;
        }
        if !self.pre_dict_frames.is_empty() {
            self.flush_pre_dict_without_dict()?;
        }
        self.cur_is_bypass = true;
        self.cur_min_savings_pct = BLOB_MIN_SAVINGS_PCT;

        let mut cids = Vec::with_capacity(bytes.len().div_ceil(BLOB_MAX_CHUNK_BYTES));
        for slice in bytes.chunks(BLOB_MAX_CHUNK_BYTES) {
            let total_chunks = self.chunk_locs.len()
                + self.pre_dict_chunk_count
                + self.pending_frame_chunks
                + self.cur_chunks.len();
            if total_chunks >= MAX_PACK_CHUNKS {
                return Err(StatamError::BoundsExceeded {
                    reason: "pack chunk count exceeds MAX_PACK_CHUNKS",
                });
            }
            let chunk_id =
                u32::try_from(total_chunks).map_err(|_| StatamError::BoundsExceeded {
                    reason: "chunk_id overflows u32",
                })?;
            let raw_len = u32::try_from(slice.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "chunk raw_len overflows u32",
            })?;

            let hash = blake3::hash(slice);
            let mut hash16 = [0u8; 16];
            hash16.copy_from_slice(&hash.as_bytes()[..16]);

            self.cur_raw.extend_from_slice(slice);
            self.cur_chunks.push(PendingChunk {
                hash16,
                offset_in_frame: 0,
                raw_len,
                head: InlineEdge::empty(),
                tail: InlineEdge::empty(),
            });
            self.raw_chunk_bytes = self.raw_chunk_bytes.saturating_add(slice.len() as u64);
            self.flush_current_frame()?;
            cids.push(chunk_id);
        }
        Ok(cids)
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
        if !self.cur_chunks.is_empty() {
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
        self.cur_min_savings_pct = 0;
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
        let actual_crc = pack_frame_crc(
            &raw_frame_bytes[0..12],
            &raw_frame_bytes[PACK_FRAME_HEADER_LEN..],
        );
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

        self.apply_1mib_boundary_guard(raw_frame_bytes.len())?;
        let frame_offset = self.written_len;
        self.out.write_all(raw_frame_bytes)?;
        self.written_len = self
            .written_len
            .saturating_add(raw_frame_bytes.len() as u64);
        self.stored_frame_bytes = self
            .stored_frame_bytes
            .saturating_add(raw_frame_bytes.len() as u64);

        let (filter_off, filter_len) =
            if !filter_bytes.is_empty() && self.options.enable_trigram_skip {
                let bloom = TrigramBloom::from_bytes(filter_bytes.to_vec())?;
                let projected_ts02 = TS02_HEADER_LEN
                    .saturating_add(2 * 8_192)
                    .saturating_add(self.trigram_buf.len())
                    .saturating_add(bloom.as_bytes().len());
                if projected_ts02 > MAX_BLOOM_BYTES {
                    (0, 0)
                } else {
                    let off = u32::try_from(self.trigram_buf.len()).map_err(|_| {
                        StatamError::BoundsExceeded {
                            reason: "trigram section offset overflows u32",
                        }
                    })?;
                    let len = u32::try_from(bloom.as_bytes().len()).map_err(|_| {
                        StatamError::BoundsExceeded {
                            reason: "trigram filter length overflows u32",
                        }
                    })?;
                    self.trigram_buf.extend_from_slice(bloom.as_bytes());
                    (off, len)
                }
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
            if raw_len == 0 || (!head.is_empty() && !tail.is_empty()) {
                self.dedup.entry(hash16).or_insert(chunk_id);
            }
            self.edges
                .push((InlineEdge::from_slice(head), InlineEdge::from_slice(tail)));
            self.raw_chunk_bytes = self.raw_chunk_bytes.saturating_add(u64::from(raw_len));
            remapped.push(chunk_id);
        }

        Ok(remapped)
    }

    /// Finalizes the `.stpk` stream, patches the 48-byte `PackHeader` at `start_offset`,
    /// seeks to EOF, flushes `out`, and returns `(out, PackWriteStats)`.
    pub fn finish_into_inner(mut self) -> Result<(W, PackWriteStats), StatamError> {
        if !self.cur_chunks.is_empty() {
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
        self.cur_min_savings_pct = 0;

        let frame_count =
            u32::try_from(self.frames.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "frame_count overflows u32",
            })?;
        let chunk_count =
            u32::try_from(self.chunk_locs.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "chunk_count overflows u32",
            })?;

        let mut cur_off = self.written_len;

        // 1. ChunkHashTable (compaction-only: [[u8; 16]; chunk_count] || crc32c: u32)
        let (hash_table_off, hash_table_bytes) =
            if self.options.emit_hash_table && !self.hashes.is_empty() {
                let off = cur_off;
                let hash_bytes_len = self.hashes.len().saturating_mul(16);
                let mut bytes = Vec::with_capacity(hash_bytes_len.saturating_add(4));
                for h in &self.hashes {
                    bytes.extend_from_slice(h);
                }
                let crc = crc32c::crc32c(&bytes);
                bytes.extend_from_slice(&crc.to_le_bytes());
                cur_off = cur_off.saturating_add(bytes.len() as u64);
                (off, bytes)
            } else {
                (0u64, Vec::new())
            };

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
            for (cid, (head, tail)) in self.edges.iter().enumerate() {
                if self.track_boundary_chunks
                    && !self.boundary_chunks.get(cid).copied().unwrap_or(false)
                {
                    continue;
                }
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
            trigram_section.extend_from_slice(&0u32.to_le_bytes()); // crc32c placeholder
            trigram_section.extend_from_slice(edge_tail_bloom.as_bytes());
            trigram_section.extend_from_slice(edge_head_bloom.as_bytes());
            trigram_section.extend_from_slice(&self.trigram_buf);
            let ts_crc = crc32c::crc32c_append(
                crc32c::crc32c(&trigram_section[0..8]),
                &trigram_section[TS02_HEADER_LEN..],
            );
            trigram_section[8..12].copy_from_slice(&ts_crc.to_le_bytes());

            let off = cur_off;
            cur_off = cur_off.saturating_add(u64::from(len));
            (off, len)
        } else {
            (0u64, 0u32)
        };

        // 3. EdgeTable (Zstd-3 compressed `PF`-framed payload)
        let mut edge_framed = Vec::new();
        let emit_edge_table =
            !self.edges.is_empty() && (!self.track_boundary_chunks || self.has_any_boundary);
        let (edge_table_off, edge_table_len) = if emit_edge_table {
            let cap = if self.track_boundary_chunks {
                self.edges.len().saturating_mul(2)
            } else {
                self.edges.len().saturating_mul(16)
            };
            let mut raw_edges = Vec::with_capacity(cap);
            for (cid, (head, tail)) in self.edges.iter().enumerate() {
                if self.track_boundary_chunks
                    && !self.boundary_chunks.get(cid).copied().unwrap_or(false)
                {
                    raw_edges.push(0);
                    raw_edges.push(0);
                } else {
                    let hs = head.as_slice();
                    let ts = tail.as_slice();
                    raw_edges.push(hs.len() as u8);
                    raw_edges.extend_from_slice(hs);
                    raw_edges.push(ts.len() as u8);
                    raw_edges.extend_from_slice(ts);
                }
            }
            let edge_codec = default_edge_codec();
            write_framed_pack_frame_with_limit(
                &raw_edges,
                edge_codec,
                0,
                self.options.zstd_level,
                MAX_BLOCK_RAW_BYTES,
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

        if !hash_table_bytes.is_empty() {
            self.out.write_all(&hash_table_bytes)?;
        }
        if !trigram_section.is_empty() {
            self.out.write_all(&trigram_section)?;
        }
        if !edge_framed.is_empty() {
            self.out.write_all(&edge_framed)?;
        }
        if !dict_framed.is_empty() {
            self.out.write_all(&dict_framed)?;
        }
        self.out.write_all(&toc_stored)?;
        self.out.write_all(&ps_bytes)?;

        let total_bytes = cur_off.saturating_add(u64::from(toc_stored_len));
        let eof_pos =
            self.start_offset
                .checked_add(total_bytes)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "pack output stream EOF offset overflow",
                })?;
        self.out.seek(SeekFrom::Start(self.start_offset))?;
        self.out.write_all(&header.encode())?;
        self.out.seek(SeekFrom::Start(eof_pos))?;
        self.out.flush()?;

        Ok((
            self.out,
            PackWriteStats {
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
            },
        ))
    }

    fn apply_1mib_boundary_guard(&mut self, total_frame_len: usize) -> Result<(), StatamError> {
        let mib = S3_CACHE_BLOCK_BYTES;
        let cur_off = self.written_len;
        let pos_in_block = cur_off % mib;
        let remaining_in_block = (mib - pos_in_block) as usize;
        if remaining_in_block < BOUNDARY_GUARD_THRESHOLD
            && total_frame_len < BOUNDARY_GUARD_MAX_FRAME
            && (pos_in_block as usize) + total_frame_len > mib as usize
        {
            const ZEROS: [u8; 4096] = [0u8; 4096];
            let mut left = remaining_in_block;
            while left > 0 {
                let n = left.min(ZEROS.len());
                self.out.write_all(&ZEROS[..n])?;
                left -= n;
            }
            self.written_len = self.written_len.saturating_add(remaining_in_block as u64);
        }
        Ok(())
    }

    fn flush_current_frame(&mut self) -> Result<(), StatamError> {
        if self.cur_chunks.is_empty() {
            return Ok(());
        }
        if self.frames.len() + self.pre_dict_frames.len() + self.pending_frames.len()
            >= MAX_PACK_FRAMES
        {
            return Err(StatamError::BoundsExceeded {
                reason: "pack frame count exceeds MAX_PACK_FRAMES",
            });
        }

        let is_bypass = self.cur_is_bypass;
        let min_savings_pct = self.cur_min_savings_pct;
        if !is_bypass {
            self.observed_non_bypass_raw = self
                .observed_non_bypass_raw
                .saturating_add(self.cur_raw.len() as u64);
            self.observed_non_bypass_chunks = self
                .observed_non_bypass_chunks
                .saturating_add(self.cur_chunks.len() as u64);
        }

        if !is_bypass && self.enable_dict && !self.dict_decided {
            let raw = std::mem::take(&mut self.cur_raw);
            let chunks = std::mem::take(&mut self.cur_chunks);
            self.pre_dict_raw_bytes = self.pre_dict_raw_bytes.saturating_add(raw.len());
            self.pre_dict_chunk_count = self.pre_dict_chunk_count.saturating_add(chunks.len());
            self.pre_dict_frames.push((raw, chunks));

            if self.pre_dict_raw_bytes >= DICT_TRAIN_MIN_RAW {
                self.train_and_flush_pre_dict_frames()?;
                self.maybe_decide_adaptive_frame_size()?;
            }
            return Ok(());
        }

        let raw = std::mem::take(&mut self.cur_raw);
        let chunks = std::mem::take(&mut self.cur_chunks);
        if self.options.write_threads > 1 {
            let batch_cap = self.options.write_threads.clamp(1, MAX_PARALLEL_THREADS);
            if !self.pending_frames.is_empty()
                && (self.pending_frames.len() >= batch_cap
                    || self.pending_frame_raw_bytes.saturating_add(raw.len())
                        > MAX_PENDING_RAW_BYTES)
            {
                self.flush_pending_frames()?;
            }
            self.pending_frame_chunks = self.pending_frame_chunks.saturating_add(chunks.len());
            self.pending_frame_raw_bytes = self.pending_frame_raw_bytes.saturating_add(raw.len());
            self.pending_frames
                .push((raw, chunks, is_bypass, min_savings_pct));
            if self.pending_frames.len() >= batch_cap
                || self.pending_frame_raw_bytes >= MAX_PENDING_RAW_BYTES
            {
                self.flush_pending_frames()?;
            }
        } else {
            self.encode_and_push_frame(&raw, chunks, is_bypass, min_savings_pct)?;
        }

        if !is_bypass {
            self.maybe_decide_adaptive_frame_size()?;
        }
        Ok(())
    }

    /// Deterministically decides once after `DICT_TRAIN_MIN_RAW` (`2 MiB`) of non-bypass frames
    /// whether to adapt `effective_target_frame_raw_bytes` from `512 KiB` down to `64 KiB` for
    /// unique high-entropy prose streams. Flushes `pending_frames` synchronously at this exact
    /// boundary so `1T` and `8T` inspect the identical committed frame set.
    fn maybe_decide_adaptive_frame_size(&mut self) -> Result<(), StatamError> {
        if self.frame_size_decided
            || self.options.target_frame_raw_bytes != DEFAULT_TARGET_FRAME_RAW_BYTES
            || self.observed_non_bypass_raw < DICT_TRAIN_MIN_RAW as u64
        {
            return Ok(());
        }
        if !self.pending_frames.is_empty() {
            self.flush_pending_frames()?;
        }
        self.frame_size_decided = true;
        if self.observed_non_bypass_chunks == 0 || self.observed_trigram_frames == 0 {
            return Ok(());
        }
        let low_dedup = self.dedup_hits.saturating_mul(20) < self.observed_non_bypass_chunks;
        let mean_chunk_ok =
            (self.observed_non_bypass_raw / self.observed_non_bypass_chunks) >= 1_024;
        let high_density =
            self.observed_high_density_frames.saturating_mul(2) >= self.observed_trigram_frames;
        if low_dedup && mean_chunk_ok && high_density {
            self.effective_target_frame_raw_bytes = ADAPTIVE_PROSE_FRAME_RAW_BYTES;
        }
        Ok(())
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
                self.pending_frames.push((raw, chunks, false, 0));
            }
            if !self.pending_frames.is_empty() {
                self.flush_pending_frames()?;
            }
        } else {
            for (raw, chunks) in pending {
                self.encode_and_push_frame(&raw, chunks, false, 0)?;
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
        let frame_target = self.effective_target_frame_raw_bytes;
        let trained_dict = self.trained_dict.as_deref();
        let encoded = parallel_map_in_order_with_init(
            batch,
            options.write_threads,
            || Ok(None::<PackDictCompressor>),
            |worker_comp, (raw, chunks, is_bypass, min_savings_pct)| {
                if chunks.is_empty() {
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
                let info = write_framed_pack_frame_with_compressor_min_savings(
                    &raw,
                    frame_codec,
                    0,
                    options.zstd_level,
                    min_savings_pct,
                    dict_comp,
                    &mut framed,
                )?;
                let frame_flags = framed.get(3).copied().unwrap_or(0);
                let trigram_evaluated = !is_bypass
                    && chunks.len() > 1
                    && options.enable_trigram_skip
                    && (info.stored_len as usize) >= MIN_FRAME_FOR_TRIGRAM;
                let (filter_bytes, high_density) = if !is_bypass
                    && options.enable_trigram_skip
                    && (info.stored_len as usize) >= MIN_FRAME_FOR_TRIGRAM
                {
                    build_frame_trigram_bytes(&raw, &chunks, info.stored_len as usize, frame_target)
                } else {
                    (None, false)
                };
                drop(raw);
                Ok(Some((
                    framed,
                    info,
                    frame_flags,
                    filter_bytes,
                    trigram_evaluated,
                    high_density,
                    chunks,
                )))
            },
        )?;

        for item in encoded {
            let Some((
                framed,
                info,
                frame_flags,
                filter_bytes,
                trigram_evaluated,
                high_density,
                chunks,
            )) = item
            else {
                continue;
            };
            self.commit_encoded_frame(
                &framed,
                info,
                frame_flags,
                filter_bytes.as_deref(),
                trigram_evaluated,
                high_density,
                chunks,
            )?;
        }
        Ok(())
    }

    fn encode_and_push_frame(
        &mut self,
        raw: &[u8],
        chunks: Vec<PendingChunk>,
        is_bypass: bool,
        min_savings_pct: u8,
    ) -> Result<(), StatamError> {
        if chunks.is_empty() {
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
        let info = write_framed_pack_frame_with_compressor_min_savings(
            raw,
            frame_codec,
            0,
            zstd_level,
            min_savings_pct,
            dict_comp,
            &mut framed,
        )?;
        let frame_flags = framed.get(3).copied().unwrap_or(0);
        let trigram_evaluated = !is_bypass
            && chunks.len() > 1
            && self.options.enable_trigram_skip
            && (info.stored_len as usize) >= MIN_FRAME_FOR_TRIGRAM;
        let (filter_bloom, high_density) = if !is_bypass
            && self.options.enable_trigram_skip
            && (info.stored_len as usize) >= MIN_FRAME_FOR_TRIGRAM
        {
            build_frame_trigram_bloom(
                raw,
                &chunks,
                info.stored_len as usize,
                self.effective_target_frame_raw_bytes,
            )
        } else {
            (None, false)
        };
        self.commit_encoded_frame(
            &framed,
            info,
            frame_flags,
            filter_bloom.as_ref().map(TrigramBloom::as_bytes),
            trigram_evaluated,
            high_density,
            chunks,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_encoded_frame(
        &mut self,
        framed: &[u8],
        info: FramedWriteInfo,
        frame_flags: u8,
        filter_bytes: Option<&[u8]>,
        trigram_evaluated: bool,
        high_density: bool,
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
        if trigram_evaluated {
            self.observed_trigram_frames = self.observed_trigram_frames.saturating_add(1);
            if high_density {
                self.observed_high_density_frames =
                    self.observed_high_density_frames.saturating_add(1);
            }
        }
        self.apply_1mib_boundary_guard(framed.len())?;

        let frame_offset = self.written_len;
        self.out.write_all(framed)?;
        self.written_len = self.written_len.saturating_add(framed.len() as u64);
        self.stored_frame_bytes = self.stored_frame_bytes.saturating_add(framed.len() as u64);

        let (filter_off, filter_len) = if let Some(fb) = filter_bytes {
            let projected_ts02 = TS02_HEADER_LEN
                .saturating_add(2 * 8_192)
                .saturating_add(self.trigram_buf.len())
                .saturating_add(fb.len());
            if projected_ts02 > MAX_BLOOM_BYTES {
                (0, 0)
            } else {
                let off = u32::try_from(self.trigram_buf.len()).map_err(|_| {
                    StatamError::BoundsExceeded {
                        reason: "trigram section offset overflows u32",
                    }
                })?;
                let len = u32::try_from(fb.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "trigram filter length overflows u32",
                })?;
                self.trigram_buf.extend_from_slice(fb);
                (off, len)
            }
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
    frame_target: usize,
) -> (Option<Vec<u8>>, bool) {
    let (bloom, high_density) = build_frame_trigram_bloom(raw, chunks, stored_len, frame_target);
    (bloom.map(|b| b.as_bytes().to_vec()), high_density)
}

fn build_frame_trigram_bloom(
    raw: &[u8],
    chunks: &[PendingChunk],
    stored_len: usize,
    frame_target: usize,
) -> (Option<TrigramBloom>, bool) {
    let frame_divisor = if frame_target <= ADAPTIVE_PROSE_FRAME_RAW_BYTES {
        12
    } else {
        16
    };
    let frame_cap =
        (raw.len() / frame_divisor).clamp(MIN_TRIGRAM_BLOOM_BYTES, MAX_TRIGRAM_BLOOM_BYTES);
    let max_bytes = (stored_len / 6)
        .min(frame_cap)
        .clamp(MIN_TRIGRAM_BLOOM_BYTES, MAX_TRIGRAM_BLOOM_BYTES);
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
                return (None, true);
            }
            next_check_bytes = scanned_bytes + 64 * 1024;
        }
    }

    let mut density = bloom.bit_density();
    if density == 0.0 {
        return (None, false);
    }
    while bloom.as_bytes().len() > MIN_TRIGRAM_BLOOM_BYTES && density < 0.12 {
        bloom = bloom.fold_half();
        density = bloom.bit_density();
    }
    if density > MAX_TRIGRAM_DENSITY {
        return (None, true);
    }

    let high_density = density > HIGH_TRIGRAM_DENSITY_THRESHOLD
        || (bloom.as_bytes().len() == MAX_TRIGRAM_BLOOM_BYTES
            && density > HIGH_TRIGRAM_MAX_BLOOM_DENSITY);
    (Some(bloom), high_density)
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
    /// True iff `frames` partition `0..chunks.len()` contiguously and every `chunks[cid].frame_id`
    /// matches its owning frame index.
    pub(crate) frames_contiguous: bool,
}

impl PackToc {
    /// Reads and validates a `.stpk` [`PackToc`] using an optional catalog-stored TOC size hint.
    ///
    /// On remote readers, expands the tail fetch ONLY upward from the default pack tail hint
    /// (`PACK_REMOTE_SMALL_TAIL_HINT` for packs `<= 4 MiB`, else `PACK_DEFAULT_TAIL_HINT`),
    /// clamped to `MAX_HOT_TAIL_BYTES`, and returns the retained `(tail_start, tail_bytes)` slice
    /// when eligible. On local readers, uses `stored_toc_bytes` directly (clamped to
    /// `MAX_HOT_TAIL_BYTES`).
    #[allow(clippy::type_complexity)]
    pub fn read_from_with_hint(
        reader: &dyn RangeReader,
        stored_toc_bytes: Option<u64>,
    ) -> Result<(Arc<Self>, Option<(u64, Bytes)>), StatamError> {
        let file_len = reader.file_len();
        let default_hint =
            if reader.is_remote() && file_len <= PACK_REMOTE_ADAPTIVE_TAIL_MAX_FILE_LEN {
                PACK_REMOTE_SMALL_TAIL_HINT
            } else {
                PACK_DEFAULT_TAIL_HINT
            };
        let tail_hint = stored_toc_bytes.map(|h| {
            let bytes = if reader.is_remote() {
                h.max(u64::from(default_hint))
            } else {
                h
            };
            bytes.min(MAX_HOT_TAIL_BYTES as u64) as u32
        });
        let (toc, tail_start, tail, data_end) = read_toc_and_tail(reader, tail_hint)?;
        let retain_tail = reader.is_remote()
            && tail_start < data_end
            && tail.len() <= crate::io::MAX_RETAINED_TAIL_BYTES;
        Ok((toc, retain_tail.then_some((tail_start, tail))))
    }

    /// Returns a deterministic non-zero heap memory estimate in bytes for external metadata caches.
    pub fn memory_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.frames.len().saturating_mul(size_of::<FrameEntry>()))
            .saturating_add(self.chunks.len().saturating_mul(size_of::<ChunkLoc>()))
    }
}

fn read_toc_and_tail(
    reader: &dyn RangeReader,
    tail_hint: Option<u32>,
) -> Result<(Arc<PackToc>, u64, Bytes, u64), StatamError> {
    let file_len = reader.file_len();
    let min_len = (PACK_HEADER_LEN + PACK_POSTSCRIPT_LEN) as u64;
    if file_len < min_len {
        return Err(StatamError::Corrupt {
            reason: "pack file smaller than header + postscript",
        });
    }

    let default_hint = if reader.is_remote() && file_len <= PACK_REMOTE_ADAPTIVE_TAIL_MAX_FILE_LEN {
        PACK_REMOTE_SMALL_TAIL_HINT
    } else {
        PACK_DEFAULT_TAIL_HINT
    };
    let hint = u64::from(tail_hint.unwrap_or(default_hint)).max(PACK_POSTSCRIPT_LEN as u64);
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

    let toc_body_stored_len = (ps.toc_stored_len as usize) - PACK_POSTSCRIPT_LEN;
    let owned_toc_body: Bytes;
    let toc_body: &[u8] = if tail.len() >= ps.toc_stored_len as usize {
        let start = tail.len() - (ps.toc_stored_len as usize);
        &tail[start..start + toc_body_stored_len]
    } else {
        let toc_off = file_len - toc_stored_u64;
        owned_toc_body = reader.read_range(toc_off..toc_off + toc_body_stored_len as u64)?;
        if owned_toc_body.len() != toc_body_stored_len {
            return Err(StatamError::Corrupt {
                reason: "short read on pack TOC body",
            });
        }
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
    let mut expected_cid = 0u32;
    let mut frames_contiguous = frame_count > 0;
    for idx in 0..frame_count {
        let off = idx * FRAME_ENTRY_LEN;
        let entry = FrameEntry::decode(&toc_raw[off..off + FRAME_ENTRY_LEN])?;
        validate_frame_entry(&entry, &ps, data_end, &trailer_sections)?;
        if frames_contiguous {
            if entry.first_chunk_id != expected_cid {
                frames_contiguous = false;
            } else if let Some(next) = expected_cid.checked_add(entry.chunk_count) {
                expected_cid = next;
            } else {
                frames_contiguous = false;
            }
        }
        frames.push(entry);
    }
    if expected_cid as usize != chunk_count {
        frames_contiguous = false;
    }

    let mut chunks = Vec::with_capacity(chunk_count);
    let mut prev_frame = u32::MAX;
    let mut prev_end = 0u32;
    let mut expected_fid = 0usize;
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
        let end =
            loc.offset_in_frame
                .checked_add(loc.raw_len)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "chunk offset_in_frame + raw_len overflow",
                })?;
        if end > frame.raw_len {
            return Err(StatamError::Corrupt {
                reason: "chunk span exceeds owning frame raw_len",
            });
        }
        if frames_contiguous {
            while expected_fid < frames.len()
                && (idx as u64)
                    >= u64::from(frames[expected_fid].first_chunk_id)
                        + u64::from(frames[expected_fid].chunk_count)
            {
                expected_fid += 1;
            }
            if expected_fid >= frames.len() || loc.frame_id as usize != expected_fid {
                frames_contiguous = false;
            }
        }
        prev_frame = loc.frame_id;
        prev_end = end;
        chunks.push(loc);
    }

    let toc = Arc::new(PackToc {
        postscript: ps,
        frames,
        chunks,
        frames_contiguous,
    });
    Ok((toc, tail_start, tail, data_end))
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
    bool,
);

pub(crate) type FetchedFrameGroups = (
    Vec<(u32, Vec<u32>)>,
    Vec<Bytes>,
    Option<Arc<PackDecoderDict>>,
);

/// Range-based reader for an immutable `.stpk` chunk pack.
pub struct ChunkPackReader {
    reader: Arc<dyn RangeReader>,
    toc: Arc<PackToc>,
    data_end: u64,
    retained_tail_start: Option<u64>,
    inline_dict_framed: Option<Bytes>,
    inline_trigram: Option<Bytes>,
    inline_edge_table: Option<Bytes>,
    dict_cache: OnceLock<Option<Arc<PackDecoderDict>>>,
    dict_init: Mutex<()>,
    trigram_cache: OnceLock<TrigramSectionData>,
    trigram_init: Mutex<()>,
    edge_table_cache: OnceLock<Arc<FlatEdgeTable>>,
    edge_table_init: Mutex<()>,
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
    /// Opens a `.stpk` pack by reading its tail in 1 GET (`384 KiB` on remote readers for packs
    /// `<= 4 MiB`, else `PACK_DEFAULT_TAIL_HINT` = `256 KiB`).
    pub fn open(reader: Arc<dyn RangeReader>) -> Result<Self, StatamError> {
        Self::open_with_tail_hint(reader, None)
    }

    /// Opens a `.stpk` pack with a custom tail-read hint.
    pub fn open_with_tail_hint(
        reader: Arc<dyn RangeReader>,
        tail_hint: Option<u32>,
    ) -> Result<Self, StatamError> {
        let (toc, tail_start, tail, data_end) = read_toc_and_tail(reader.as_ref(), tail_hint)?;
        let ps = &toc.postscript;

        let retain_tail = reader.is_remote()
            && tail_start < data_end
            && tail.len() <= crate::io::MAX_RETAINED_TAIL_BYTES;

        let slice_inline = |off: u64, len: u32| -> Option<Bytes> {
            if len == 0
                || off < tail_start
                || off
                    .checked_add(u64::from(len))
                    .is_none_or(|end| end > data_end)
            {
                return None;
            }
            usize::try_from(off - tail_start).ok().and_then(|start| {
                let end = start.checked_add(len as usize)?;
                if end > tail.len() {
                    return None;
                }
                if retain_tail {
                    Some(tail.slice(start..end))
                } else {
                    Some(Bytes::copy_from_slice(&tail[start..end]))
                }
            })
        };

        let inline_dict_framed = if (ps.flags & PACK_FLAG_HAS_DICT) != 0 {
            slice_inline(ps.dict_off, ps.dict_len)
        } else {
            None
        };
        let inline_trigram = slice_inline(ps.trigram_off, ps.trigram_len);
        let inline_edge_table = slice_inline(ps.edge_table_off, ps.edge_table_len);

        let retained_tail_start = retain_tail.then_some(tail_start);
        let reader: Arc<dyn RangeReader> = if retain_tail {
            Arc::new(crate::io::TailCachedReader::new(reader, tail_start, tail)?)
        } else {
            reader
        };

        Ok(Self {
            reader,
            toc,
            data_end,
            retained_tail_start,
            inline_dict_framed,
            inline_trigram,
            inline_edge_table,
            dict_cache: OnceLock::new(),
            dict_init: Mutex::new(()),
            trigram_cache: OnceLock::new(),
            trigram_init: Mutex::new(()),
            edge_table_cache: OnceLock::new(),
            edge_table_init: Mutex::new(()),
        })
    }

    /// Zero-I/O constructor from an externally cached [`Arc<PackToc>`].
    pub fn open_with_toc(
        reader: Arc<dyn RangeReader>,
        toc: Arc<PackToc>,
    ) -> Result<Self, StatamError> {
        let file_len = reader.file_len();
        let min_len = (PACK_HEADER_LEN + PACK_POSTSCRIPT_LEN) as u64;
        if file_len < min_len {
            return Err(StatamError::Corrupt {
                reason: "pack file smaller than header + postscript",
            });
        }
        let ps = &toc.postscript;
        let toc_stored_u64 = u64::from(ps.toc_stored_len);
        if toc_stored_u64 > file_len - PACK_HEADER_LEN as u64 {
            return Err(StatamError::Corrupt {
                reason: "pack toc_stored_len exceeds file data bounds",
            });
        }
        let data_end = file_len - toc_stored_u64;
        let _ = validate_trailer_sections(ps, data_end)?;

        Ok(Self {
            reader,
            toc,
            data_end,
            retained_tail_start: None,
            inline_dict_framed: None,
            inline_trigram: None,
            inline_edge_table: None,
            dict_cache: OnceLock::new(),
            dict_init: Mutex::new(()),
            trigram_cache: OnceLock::new(),
            trigram_init: Mutex::new(()),
            edge_table_cache: OnceLock::new(),
            edge_table_init: Mutex::new(()),
        })
    }

    fn validated_dict_range(&self) -> Result<Option<Range<u64>>, StatamError> {
        let ps = &self.toc.postscript;
        if (ps.flags & PACK_FLAG_HAS_DICT) == 0 || ps.dict_len == 0 {
            return Ok(None);
        }
        let start = ps.dict_off;
        let end = start
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
        Ok(Some(start..end))
    }

    fn install_decoder_dict_locked(
        &self,
        _guard: &std::sync::MutexGuard<'_, ()>,
        framed: &[u8],
    ) -> Result<Arc<PackDecoderDict>, StatamError> {
        let ps = &self.toc.postscript;
        if framed.len() != ps.dict_len as usize {
            return Err(StatamError::Corrupt {
                reason: "short read on pack dict section",
            });
        }
        if let Some(Some(cached)) = self.dict_cache.get() {
            return Ok(Arc::clone(cached));
        }
        let (_, flags, dict_bytes) = read_framed_pack_frame(framed, MAX_PACK_DICT_BYTES)?;
        if flags != 0 {
            return Err(StatamError::Corrupt {
                reason: "pack dict section frame has unexpected flags",
            });
        }
        let decoder_dict = Arc::new(PackDecoderDict::new(&dict_bytes)?);
        let _ = self.dict_cache.set(Some(Arc::clone(&decoder_dict)));
        Ok(decoder_dict)
    }

    fn install_decoder_dict_from_framed(
        &self,
        framed: &[u8],
    ) -> Result<Arc<PackDecoderDict>, StatamError> {
        let ps = &self.toc.postscript;
        if framed.len() != ps.dict_len as usize {
            return Err(StatamError::Corrupt {
                reason: "short read on pack dict section",
            });
        }
        if let Some(Some(cached)) = self.dict_cache.get() {
            return Ok(Arc::clone(cached));
        }
        let guard = self.dict_init.lock().unwrap_or_else(|p| p.into_inner());
        self.install_decoder_dict_locked(&guard, framed)
    }

    fn resolve_or_append_dict_range(
        &self,
        needs_dict: bool,
        ranges: &mut Vec<Range<u64>>,
    ) -> Result<(Option<Arc<PackDecoderDict>>, bool), StatamError> {
        if !needs_dict {
            return Ok((None, false));
        }
        if let Some(cached) = self.dict_cache.get() {
            return Ok((cached.clone(), false));
        }
        if self.inline_dict_framed.is_some() {
            return Ok((self.load_decoder_dict()?, false));
        }
        if let Some(dict_range) = self.validated_dict_range()? {
            ranges.push(dict_range);
            Ok((None, true))
        } else {
            Ok((None, false))
        }
    }

    fn read_ranges_remote_coalesced(
        &self,
        ranges: &[Range<u64>],
    ) -> Result<Vec<Bytes>, StatamError> {
        for r in ranges {
            if r.start > r.end {
                return Err(StatamError::Corrupt {
                    reason: "inverted pack read range",
                });
            }
        }
        let mut order: Vec<usize> = (0..ranges.len())
            .filter(|&i| ranges[i].start < ranges[i].end)
            .collect();
        if order.is_empty() {
            return Ok(vec![Bytes::new(); ranges.len()]);
        }
        order.sort_by_key(|&i| (ranges[i].start, ranges[i].end));

        let mut spans: Vec<(Range<u64>, Vec<usize>)> = Vec::new();
        for idx in order {
            let r = &ranges[idx];
            let can_merge = if let Some((last_span, _)) = spans.last() {
                let crosses_tail = self
                    .retained_tail_start
                    .is_some_and(|ts| last_span.end <= ts && r.start >= ts);
                let merged_end = last_span.end.max(r.end);
                !crosses_tail
                    && r.start <= last_span.end.saturating_add(REMOTE_PACK_COALESCE_MAX_GAP)
                    && merged_end.saturating_sub(last_span.start) <= REMOTE_PACK_COALESCE_MAX_SPAN
            } else {
                false
            };
            if can_merge {
                if let Some((last_span, members)) = spans.last_mut() {
                    last_span.end = last_span.end.max(r.end);
                    members.push(idx);
                }
            } else {
                spans.push((r.clone(), vec![idx]));
            }
        }

        let span_ranges: Vec<Range<u64>> = spans.iter().map(|(s, _)| s.clone()).collect();
        let fetched_spans = self.reader.read_ranges(&span_ranges)?;
        if fetched_spans.len() != spans.len() {
            return Err(StatamError::Corrupt {
                reason: "read_ranges returned wrong coalesced span count",
            });
        }

        let mut out = vec![Bytes::new(); ranges.len()];
        for ((span, members), span_bytes) in spans.into_iter().zip(fetched_spans) {
            let expected = span.end - span.start;
            if span_bytes.len() as u64 != expected {
                return Err(StatamError::Corrupt {
                    reason: "short read on coalesced pack frame span",
                });
            }
            for idx in members {
                let r = &ranges[idx];
                let rel_start = usize::try_from(r.start - span.start).map_err(|_| {
                    StatamError::BoundsExceeded {
                        reason: "coalesced pack range start overflows usize",
                    }
                })?;
                let rel_end = usize::try_from(r.end - span.start).map_err(|_| {
                    StatamError::BoundsExceeded {
                        reason: "coalesced pack range end overflows usize",
                    }
                })?;
                if rel_start > rel_end || rel_end > span_bytes.len() {
                    return Err(StatamError::Corrupt {
                        reason: "coalesced pack slice out of bounds",
                    });
                }
                out[idx] = span_bytes.slice(rel_start..rel_end);
            }
        }
        Ok(out)
    }

    fn fetch_frames_and_dict(
        &self,
        ranges: &[Range<u64>],
        expected_frames: usize,
        dict: Option<Arc<PackDecoderDict>>,
        has_trailing_dict: bool,
    ) -> Result<(Vec<Bytes>, Option<Arc<PackDecoderDict>>), StatamError> {
        let expected_total = expected_frames + usize::from(has_trailing_dict);
        if ranges.len() != expected_total {
            return Err(StatamError::Corrupt {
                reason: "read_ranges input range count mismatch",
            });
        }
        let mut fetched = if !self.reader.is_remote() || ranges.len() <= 1 {
            let parts = self.reader.read_ranges(ranges)?;
            if parts.len() != expected_total {
                return Err(StatamError::Corrupt {
                    reason: "read_ranges returned wrong frame count",
                });
            }
            parts
        } else {
            self.read_ranges_remote_coalesced(ranges)?
        };
        let dict = if has_trailing_dict {
            let Some(dict_framed) = fetched.pop() else {
                return Err(StatamError::Corrupt {
                    reason: "read_ranges missing trailing pack dict range",
                });
            };
            Some(self.install_decoder_dict_from_framed(&dict_framed)?)
        } else {
            dict
        };
        Ok((fetched, dict))
    }

    fn load_decoder_dict(&self) -> Result<Option<Arc<PackDecoderDict>>, StatamError> {
        let ps = &self.toc.postscript;
        if (ps.flags & PACK_FLAG_HAS_DICT) == 0 || ps.dict_len == 0 {
            return Ok(None);
        }
        if let Some(cached) = self.dict_cache.get() {
            return Ok(cached.clone());
        }
        let guard = self.dict_init.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(cached) = self.dict_cache.get() {
            return Ok(cached.clone());
        }
        if let Some(ref inline) = self.inline_dict_framed {
            return Ok(Some(self.install_decoder_dict_locked(&guard, inline)?));
        }
        let Some(range) = self.validated_dict_range()? else {
            return Ok(None);
        };
        let bytes = self.reader.read_range(range)?;
        Ok(Some(self.install_decoder_dict_locked(&guard, &bytes)?))
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

    /// Validates that reading the unique `chunk_ids` (and their unique owning frames)
    /// stays within `MAX_BATCH_DECODED_BYTES`, accumulating into caller-owned running totals.
    pub(crate) fn check_chunks_budget(
        &self,
        chunk_ids: &[u32],
        total_chunk_bytes: &mut usize,
        total_frame_bytes: &mut usize,
    ) -> Result<(), StatamError> {
        let mut seen_chunks = BTreeSet::new();
        let mut seen_frames = BTreeSet::new();
        for &cid in chunk_ids {
            let loc = self
                .toc
                .chunks
                .get(cid as usize)
                .ok_or(StatamError::Corrupt {
                    reason: "chunk_id out of bounds",
                })?;
            if seen_chunks.insert(cid) {
                *total_chunk_bytes = total_chunk_bytes.checked_add(loc.raw_len as usize).ok_or(
                    StatamError::BoundsExceeded {
                        reason: "pack chunk decoded bytes overflow",
                    },
                )?;
                if *total_chunk_bytes > MAX_BATCH_DECODED_BYTES {
                    return Err(StatamError::BoundsExceeded {
                        reason: "requested pack chunk bytes exceed MAX_BATCH_DECODED_BYTES",
                    });
                }
            }
            if seen_frames.insert(loc.frame_id) {
                let entry = &self.toc.frames[loc.frame_id as usize];
                *total_frame_bytes = total_frame_bytes
                    .checked_add(entry.raw_len as usize)
                    .ok_or(StatamError::BoundsExceeded {
                        reason: "pack frame decoded bytes overflow",
                    })?;
                if *total_frame_bytes > MAX_BATCH_DECODED_BYTES {
                    return Err(StatamError::BoundsExceeded {
                        reason: "requested pack frame bytes exceed MAX_BATCH_DECODED_BYTES",
                    });
                }
            }
        }
        Ok(())
    }

    /// Reads all requested `chunk_ids`, coalescing frame reads into a single `read_ranges` wave.
    pub fn read_chunks(&self, chunk_ids: &[u32]) -> Result<HashMap<u32, Vec<u8>>, StatamError> {
        if chunk_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut needed_frames = BTreeSet::new();
        let mut unique_chunks = BTreeSet::new();
        let mut total_chunk_bytes = 0usize;
        let mut total_frame_bytes = 0usize;
        for &cid in chunk_ids {
            let loc = self
                .toc
                .chunks
                .get(cid as usize)
                .ok_or(StatamError::Corrupt {
                    reason: "chunk_id out of bounds",
                })?;
            if unique_chunks.insert(cid) {
                total_chunk_bytes = total_chunk_bytes.checked_add(loc.raw_len as usize).ok_or(
                    StatamError::BoundsExceeded {
                        reason: "pack chunk decoded bytes overflow",
                    },
                )?;
                if total_chunk_bytes > MAX_BATCH_DECODED_BYTES {
                    return Err(StatamError::BoundsExceeded {
                        reason: "requested pack chunk bytes exceed MAX_BATCH_DECODED_BYTES",
                    });
                }
            }
            if needed_frames.insert(loc.frame_id) {
                let entry = &self.toc.frames[loc.frame_id as usize];
                total_frame_bytes = total_frame_bytes
                    .checked_add(entry.raw_len as usize)
                    .ok_or(StatamError::BoundsExceeded {
                        reason: "pack frame decoded bytes overflow",
                    })?;
                if total_frame_bytes > MAX_BATCH_DECODED_BYTES {
                    return Err(StatamError::BoundsExceeded {
                        reason: "requested pack frame bytes exceed MAX_BATCH_DECODED_BYTES",
                    });
                }
            }
        }

        let frame_ids: Vec<u32> = needed_frames.into_iter().collect();
        let mut ranges = Vec::with_capacity(frame_ids.len() + 1);
        let mut needs_dict = false;
        for &fid in &frame_ids {
            let entry = &self.toc.frames[fid as usize];
            if (entry.flags & PACK_FRAME_FLAG_USES_DICT) != 0 {
                needs_dict = true;
            }
            ranges.push(frame_byte_range(entry)?);
        }

        let (dict, has_trailing_dict) =
            self.resolve_or_append_dict_range(needs_dict, &mut ranges)?;
        let (frame_bytes_list, dict) =
            self.fetch_frames_and_dict(&ranges, frame_ids.len(), dict, has_trailing_dict)?;

        let mut decoded_frames: HashMap<u32, Vec<u8>> = HashMap::with_capacity(frame_ids.len());
        let mut dict_decomp = PackFrameDecompressor::new();
        for (&fid, raw) in frame_ids.iter().zip(frame_bytes_list.iter()) {
            let entry = &self.toc.frames[fid as usize];
            verify_raw_frame_matches_entry(raw, entry)?;
            let (_, _, decoded) = read_framed_pack_frame_with_decompressor(
                raw,
                MAX_FRAME_RAW_BYTES,
                dict.as_deref(),
                &mut dict_decomp,
            )?;
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
        let mut ranges = Vec::with_capacity(by_frame.len() + 1);
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

        let (dict, has_trailing_dict) =
            self.resolve_or_append_dict_range(needs_dict, &mut ranges)?;

        Ok((frame_groups, ranges, dict, has_trailing_dict))
    }

    /// Fetches all frames (and any needed dictionary) required for `chunk_ids` in one coalesced wave.
    pub(crate) fn prepare_and_fetch_frames(
        &self,
        chunk_ids: &[u32],
    ) -> Result<FetchedFrameGroups, StatamError> {
        self.fetch_frame_chunk_groups(chunk_ids)
    }

    fn fetch_frame_chunk_groups(
        &self,
        chunk_ids: &[u32],
    ) -> Result<FetchedFrameGroups, StatamError> {
        let (frame_groups, ranges, dict, has_trailing_dict) =
            self.prepare_frame_chunk_groups(chunk_ids)?;
        let (frame_bytes_list, dict) =
            self.fetch_frames_and_dict(&ranges, frame_groups.len(), dict, has_trailing_dict)?;
        Ok((frame_groups, frame_bytes_list, dict))
    }

    /// Verifies and decompresses a single `PF` frame, returning requested chunk slices as
    /// zero-copy [`Bytes`] slices when requested bytes cover at least half of the frame (or
    /// waste `<= 16 KiB`), and tightly copied [`Bytes`] otherwise so unused frame memory is freed.
    pub(crate) fn decompress_frame_chunks_as_bytes<'a>(
        &self,
        fid: u32,
        raw: &[u8],
        cids: &[u32],
        dict: Option<&'a PackDecoderDict>,
        dict_decomp: &mut PackFrameDecompressor<'a>,
    ) -> Result<Vec<(u32, Bytes)>, StatamError> {
        decompress_frame_chunks_as_bytes_with_toc(&self.toc, fid, raw, cids, dict, dict_decomp)
    }

    /// Fetches and decompresses all requested unique `chunk_ids` as guarded zero-copy [`Bytes`],
    /// reusing a per-worker [`PackFrameDecompressor`] and sizing worker threads from compressed work.
    pub(crate) fn scan_chunks_as_bytes_parallel(
        &self,
        chunk_ids: &[u32],
        read_threads: usize,
    ) -> Result<Vec<(u32, Bytes)>, StatamError> {
        if chunk_ids.is_empty() {
            return Ok(Vec::new());
        }

        let (frame_groups, frame_bytes_list, dict) = self.fetch_frame_chunk_groups(chunk_ids)?;
        let threads = read_threads.clamp(1, MAX_PARALLEL_THREADS);
        let mut compressed_frames = 0usize;
        let mut compressed_raw_bytes = 0usize;
        for &(fid, _) in &frame_groups {
            let entry = &self.toc.frames[fid as usize];
            if entry.codec != Codec::None {
                compressed_frames += 1;
                compressed_raw_bytes = compressed_raw_bytes.saturating_add(entry.raw_len as usize);
            }
        }

        if threads <= 1
            || frame_groups.len() <= 1
            || (compressed_frames < 2 && compressed_raw_bytes < 128 * 1024)
        {
            let mut dict_decomp = PackFrameDecompressor::new();
            let mut out = Vec::with_capacity(chunk_ids.len());
            for ((fid, cids), raw) in frame_groups.iter().zip(frame_bytes_list.iter()) {
                let mut pairs = self.decompress_frame_chunks_as_bytes(
                    *fid,
                    raw,
                    cids,
                    dict.as_deref(),
                    &mut dict_decomp,
                )?;
                out.append(&mut pairs);
            }
            return Ok(out);
        }

        let workers = compressed_frames
            .max(compressed_raw_bytes / (128 * 1024))
            .clamp(1, threads)
            .min(frame_groups.len());

        let mut frame_tasks: Vec<(u32, Bytes, Vec<u32>)> = frame_groups
            .into_iter()
            .zip(frame_bytes_list)
            .map(|((fid, cids), raw)| (fid, raw, cids))
            .collect();
        // LPT scheduling: start the largest compressed frames first so skewed packs finish sooner
        // (`Codec::None` frames need no decompression and carry `work_len = 0`).
        frame_tasks.sort_unstable_by_key(|(fid, _, _)| {
            let entry = &self.toc.frames[*fid as usize];
            let work_len = if entry.codec == Codec::None {
                0
            } else {
                entry.raw_len
            };
            std::cmp::Reverse(work_len)
        });

        let per_frame = parallel_map_in_order_static_with_ctx(
            (Arc::clone(&self.toc), dict),
            frame_tasks,
            workers,
            |(toc, dict), worker| {
                worker.run_with_init(
                    || Ok(PackFrameDecompressor::new()),
                    |dict_decomp, (fid, raw, cids)| {
                        decompress_frame_chunks_as_bytes_with_toc(
                            toc,
                            fid,
                            &raw,
                            &cids,
                            dict.as_deref(),
                            dict_decomp,
                        )
                    },
                )
            },
        )?;

        let total: usize = per_frame.iter().map(Vec::len).sum();
        let mut out = Vec::with_capacity(total);
        for mut items in per_frame {
            out.append(&mut items);
        }
        Ok(out)
    }

    fn decompress_and_visit_frame<'a, F>(
        &self,
        fid: u32,
        raw: &[u8],
        cids: &[u32],
        dict: Option<&'a PackDecoderDict>,
        dict_decomp: &mut PackFrameDecompressor<'a>,
        mut visit: F,
    ) -> Result<(), StatamError>
    where
        F: FnMut(u32, &[u8]),
    {
        let entry = &self.toc.frames[fid as usize];
        verify_raw_frame_matches_entry(raw, entry)?;
        let (_, _, decoded) =
            read_framed_pack_frame_with_decompressor(raw, MAX_FRAME_RAW_BYTES, dict, dict_decomp)?;
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

        let (frame_groups, frame_bytes_list, dict) = self.fetch_frame_chunk_groups(chunk_ids)?;
        let mut dict_decomp = PackFrameDecompressor::new();

        for ((fid, cids), raw) in frame_groups.iter().zip(frame_bytes_list.iter()) {
            self.decompress_and_visit_frame(
                *fid,
                raw,
                cids,
                dict.as_deref(),
                &mut dict_decomp,
                &mut visit,
            )?;
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

        let (frame_groups, frame_bytes_list, dict) = self.fetch_frame_chunk_groups(chunk_ids)?;

        let threads = read_threads.clamp(1, MAX_PARALLEL_THREADS);
        let mut compressed_frames = 0usize;
        let mut total_raw_bytes = 0usize;
        for &(fid, _) in &frame_groups {
            let entry = &self.toc.frames[fid as usize];
            if entry.codec != Codec::None {
                compressed_frames += 1;
            }
            total_raw_bytes = total_raw_bytes.saturating_add(entry.raw_len as usize);
        }
        if threads <= 1
            || frame_groups.len() <= 1
            || (compressed_frames < 4 && total_raw_bytes < 2 * 1024 * 1024)
        {
            let mut state = init()?;
            let mut dict_decomp = PackFrameDecompressor::new();
            let mut out = Vec::new();
            for ((fid, cids), raw) in frame_groups.iter().zip(frame_bytes_list.iter()) {
                self.decompress_and_visit_frame(
                    *fid,
                    raw,
                    cids,
                    dict.as_deref(),
                    &mut dict_decomp,
                    |cid, slice| {
                        if let Some(item) = f(&mut state, cid, slice) {
                            out.push(item);
                        }
                    },
                )?;
            }
            return Ok(out);
        }

        let workers = threads.min(frame_groups.len());

        let frame_tasks: Vec<(u32, Bytes, Vec<u32>)> = frame_groups
            .into_iter()
            .zip(frame_bytes_list)
            .map(|((fid, cids), raw)| (fid, raw, cids))
            .collect();

        let per_frame = parallel_map_in_order_with_init(
            frame_tasks,
            workers,
            || Ok((init()?, PackFrameDecompressor::new())),
            |(state, dict_decomp), (fid, raw, cids)| {
                let mut frame_out = Vec::new();
                self.decompress_and_visit_frame(
                    fid,
                    &raw,
                    &cids,
                    dict.as_deref(),
                    dict_decomp,
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

    #[allow(clippy::too_many_arguments)]
    fn decompress_and_run_frame<'a, S, T, F>(
        &self,
        fid: u32,
        raw: &[u8],
        cids: &[u32],
        dict: Option<&'a PackDecoderDict>,
        dict_decomp: &mut PackFrameDecompressor<'a>,
        state: &mut S,
        f: &F,
    ) -> Result<Option<T>, StatamError>
    where
        F: Fn(&mut S, u32, &[u8], &[u32]) -> Result<Option<T>, StatamError>,
    {
        let entry = &self.toc.frames[fid as usize];
        verify_raw_frame_matches_entry(raw, entry)?;
        let (_, _, decoded) =
            read_framed_pack_frame_with_decompressor(raw, MAX_FRAME_RAW_BYTES, dict, dict_decomp)?;
        if decoded.len() != entry.raw_len as usize {
            return Err(StatamError::Corrupt {
                reason: "decompressed frame length mismatch",
            });
        }
        f(state, fid, &decoded, cids)
    }

    /// Frame-level variant of [`Self::scan_chunks_in_frames_parallel`] that passes the full
    /// decompressed frame buffer and its requested `chunk_ids` to `f`, enabling callers (e.g.
    /// Phase-1 substring search) to retain matched rows' chunks (including same-frame siblings)
    /// without re-reading the frame in Phase 2.
    pub(crate) fn scan_frames_parallel<S, T, Init, F>(
        &self,
        chunk_ids: &[u32],
        read_threads: usize,
        init: Init,
        f: F,
    ) -> Result<Vec<T>, StatamError>
    where
        T: Send,
        Init: Fn() -> Result<S, StatamError> + Sync,
        F: Fn(&mut S, u32, &[u8], &[u32]) -> Result<Option<T>, StatamError> + Sync,
    {
        if chunk_ids.is_empty() {
            return Ok(Vec::new());
        }

        let (frame_groups, frame_bytes_list, dict) = self.fetch_frame_chunk_groups(chunk_ids)?;

        let threads = read_threads.clamp(1, MAX_PARALLEL_THREADS);
        let mut compressed_frames = 0usize;
        let mut total_raw_bytes = 0usize;
        for &(fid, _) in &frame_groups {
            let entry = &self.toc.frames[fid as usize];
            if entry.codec != Codec::None {
                compressed_frames += 1;
            }
            total_raw_bytes = total_raw_bytes.saturating_add(entry.raw_len as usize);
        }

        if threads <= 1
            || frame_groups.len() <= 1
            || (compressed_frames < 4 && total_raw_bytes < 2 * 1024 * 1024)
        {
            let mut state = init()?;
            let mut dict_decomp = PackFrameDecompressor::new();
            let mut out = Vec::new();
            for ((fid, cids), raw) in frame_groups.iter().zip(frame_bytes_list.iter()) {
                if let Some(item) = self.decompress_and_run_frame(
                    *fid,
                    raw,
                    cids,
                    dict.as_deref(),
                    &mut dict_decomp,
                    &mut state,
                    &f,
                )? {
                    out.push(item);
                }
            }
            return Ok(out);
        }

        let workers = threads.min(frame_groups.len());

        let frame_tasks: Vec<(u32, Bytes, Vec<u32>)> = frame_groups
            .into_iter()
            .zip(frame_bytes_list)
            .map(|((fid, cids), raw)| (fid, raw, cids))
            .collect();

        let per_frame = parallel_map_in_order_with_init(
            frame_tasks,
            workers,
            || Ok((init()?, PackFrameDecompressor::new())),
            |(state, dict_decomp), (fid, raw, cids)| {
                self.decompress_and_run_frame(
                    fid,
                    &raw,
                    &cids,
                    dict.as_deref(),
                    dict_decomp,
                    state,
                    &f,
                )
            },
        )?;

        Ok(per_frame.into_iter().flatten().collect())
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
        let _guard = self.trigram_init.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(cached) = self.trigram_cache.get() {
            return Ok(cached);
        }
        let data = self.read_trigram_section_uncached(false)?;
        let _ = self.trigram_cache.set(data);
        self.trigram_cache.get().ok_or(StatamError::Corrupt {
            reason: "trigram cache uninitialized",
        })
    }

    fn read_trigram_section_uncached(
        &self,
        bypass_inline: bool,
    ) -> Result<TrigramSectionData, StatamError> {
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
        let section = if !bypass_inline && let Some(ref inline) = self.inline_trigram {
            inline.clone()
        } else {
            self.reader.read_range(start..end)?
        };
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
        let expected_crc = read_u32_le(&section, 8);
        let actual_crc =
            crc32c::crc32c_append(crc32c::crc32c(&section[0..8]), &section[TS02_HEADER_LEN..]);
        if actual_crc != expected_crc {
            return Err(StatamError::ChecksumMismatch {
                context: "pack trigram section",
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

    /// Reads and decodes the `EdgeTable` into a compact [`FlatEdgeTable`], cached per reader.
    pub fn read_edge_table_flat(&self) -> Result<Arc<FlatEdgeTable>, StatamError> {
        if let Some(cached) = self.edge_table_cache.get() {
            return Ok(Arc::clone(cached));
        }
        let _guard = self
            .edge_table_init
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(cached) = self.edge_table_cache.get() {
            return Ok(Arc::clone(cached));
        }
        let table = Arc::new(self.read_edge_table_flat_uncached(false)?);
        let _ = self.edge_table_cache.set(Arc::clone(&table));
        Ok(self.edge_table_cache.get().map_or(table, Arc::clone))
    }

    fn read_edge_table_flat_uncached(
        &self,
        bypass_inline: bool,
    ) -> Result<FlatEdgeTable, StatamError> {
        let ps = &self.toc.postscript;
        if ps.edge_table_len == 0 {
            return Ok(FlatEdgeTable {
                head_data: Vec::new(),
                head_offs: vec![0],
                tail_data: Vec::new(),
                tail_offs: vec![0],
            });
        }
        if (ps.edge_table_len as usize) > MAX_BLOCK_RAW_BYTES + PACK_FRAME_HEADER_LEN {
            return Err(StatamError::BoundsExceeded {
                reason: "pack edge_table_len exceeds MAX_BLOCK_RAW_BYTES + PACK_FRAME_HEADER_LEN",
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
        let framed = if !bypass_inline && let Some(ref inline) = self.inline_edge_table {
            inline.clone()
        } else {
            self.reader.read_range(start..end)?
        };
        if framed.len() != ps.edge_table_len as usize {
            return Err(StatamError::Corrupt {
                reason: "short read on pack edge table",
            });
        }
        let (_, _, raw) = read_framed_pack_frame(&framed, MAX_BLOCK_RAW_BYTES)?;

        let chunk_count = ps.chunk_count as usize;
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

        let data_bytes_est = raw.len().saturating_sub(min_bytes) / 2;
        let mut head_data = Vec::with_capacity(data_bytes_est);
        let mut tail_data = Vec::with_capacity(data_bytes_est);
        let mut head_offs = Vec::with_capacity(chunk_count.saturating_add(1));
        let mut tail_offs = Vec::with_capacity(chunk_count.saturating_add(1));
        head_offs.push(0);
        tail_offs.push(0);

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
            let head_slice = raw.get(pos..head_end).ok_or(StatamError::Corrupt {
                reason: "truncated edge table head bytes",
            })?;
            head_data.extend_from_slice(head_slice);
            let head_off =
                u32::try_from(head_data.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "edge table head_data exceeds u32",
                })?;
            head_offs.push(head_off);
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
            let tail_slice = raw.get(pos..tail_end).ok_or(StatamError::Corrupt {
                reason: "truncated edge table tail bytes",
            })?;
            tail_data.extend_from_slice(tail_slice);
            let tail_off =
                u32::try_from(tail_data.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "edge table tail_data exceeds u32",
                })?;
            tail_offs.push(tail_off);
            pos = tail_end;
        }

        if pos != raw.len() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in edge table",
            });
        }

        Ok(FlatEdgeTable {
            head_data,
            head_offs,
            tail_data,
            tail_offs,
        })
    }

    /// Reads and decodes the `EdgeTable` returning `(head_63, tail_63)` per unique chunk, cached per reader.
    pub fn read_edge_table(&self) -> Result<Vec<ChunkEdge>, StatamError> {
        let flat = self.read_edge_table_flat()?;
        if flat.is_empty() {
            return Ok(vec![
                (Vec::new(), Vec::new());
                self.toc.postscript.chunk_count as usize
            ]);
        }
        let mut edges = Vec::with_capacity(flat.len());
        for cid in 0..flat.len() as u32 {
            edges.push((flat.head(cid).to_vec(), flat.tail(cid).to_vec()));
        }
        Ok(edges)
    }

    /// Reads the compaction-only BLAKE3-128 `ChunkHashTable`.
    pub fn read_hash_table(&self) -> Result<Vec<[u8; 16]>, StatamError> {
        let ps = &self.toc.postscript;
        let chunk_count = ps.chunk_count as usize;
        if chunk_count == 0 || ps.hash_table_off == 0 {
            return Ok(Vec::new());
        }
        let table_bytes = chunk_count
            .checked_mul(16)
            .ok_or(StatamError::BoundsExceeded {
                reason: "hash table byte size overflow",
            })?;
        let total_bytes = table_bytes
            .checked_add(4)
            .ok_or(StatamError::BoundsExceeded {
                reason: "hash table total size overflow",
            })?;
        let end = ps.hash_table_off.checked_add(total_bytes as u64).ok_or(
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
        if bytes.len() != total_bytes {
            return Err(StatamError::Corrupt {
                reason: "short read on pack hash table",
            });
        }
        let expected_crc = read_u32_le(&bytes, table_bytes);
        let actual_crc = crc32c::crc32c(&bytes[..table_bytes]);
        if actual_crc != expected_crc {
            return Err(StatamError::ChecksumMismatch {
                context: "pack hash table",
            });
        }

        let (chunks, _) = bytes[..table_bytes].as_chunks::<16>();
        Ok(chunks.to_vec())
    }

    /// Performs full `.stpk` integrity verification: validates `PackHeader` at offset 0,
    /// verifies CRCs and structure of every present optional trailer section (`ChunkHashTable`,
    /// `TrigramSkipSection`, `EdgeTable`, `DictSection`), and decompresses and verifies CRC +
    /// chunk bounds of every frame.
    pub fn verify_full(&self, expected_pack_id: [u8; 16]) -> Result<(), StatamError> {
        let ps = &self.toc.postscript;
        if ps.pack_id != expected_pack_id {
            return Err(StatamError::Corrupt {
                reason: "pack postscript pack_id mismatch during verify_full",
            });
        }

        // 1. Explicitly read and verify PackHeader at 0..PACK_HEADER_LEN.
        let hdr_bytes = self.reader.read_range(0..PACK_HEADER_LEN as u64)?;
        if hdr_bytes.len() != PACK_HEADER_LEN {
            return Err(StatamError::Corrupt {
                reason: "short read on pack header during verify_full",
            });
        }
        let hdr = PackHeader::decode(&hdr_bytes)?;
        if hdr.pack_id != expected_pack_id
            || hdr.flags != ps.flags
            || hdr.format_version != ps.format_version
            || hdr.required_features != ps.required_features
        {
            return Err(StatamError::Corrupt {
                reason: "pack header metadata mismatch during verify_full",
            });
        }

        // 2. Verify every present optional trailer section directly from `self.reader`.
        if ps.chunk_count > 0 && ps.hash_table_off != 0 {
            let hashes = self.read_hash_table()?;
            if hashes.len() != ps.chunk_count as usize {
                return Err(StatamError::Corrupt {
                    reason: "pack hash table length mismatch during verify_full",
                });
            }
        }
        if ps.trigram_len > 0 {
            let _ = self.read_trigram_section_uncached(true)?;
        }
        if ps.edge_table_len > 0 {
            let _ = self.read_edge_table_flat_uncached(true)?;
        }
        let dict = if (ps.flags & PACK_FLAG_HAS_DICT) != 0 || ps.dict_len > 0 {
            let range = self.validated_dict_range()?.ok_or(StatamError::Corrupt {
                reason: "invalid pack dict range during verify_full",
            })?;
            let framed = self.reader.read_range(range)?;
            if framed.len() != ps.dict_len as usize {
                return Err(StatamError::Corrupt {
                    reason: "short read on pack dict section during verify_full",
                });
            }
            let (_, flags, dict_bytes) = read_framed_pack_frame(&framed, MAX_PACK_DICT_BYTES)?;
            if flags != 0 {
                return Err(StatamError::Corrupt {
                    reason: "pack dict section frame has unexpected flags",
                });
            }
            Some(PackDecoderDict::new(&dict_bytes)?)
        } else {
            None
        };

        // 3. Decompress and verify CRC + decoded length of every frame.
        let mut dict_decomp = PackFrameDecompressor::new();
        for (fid, entry) in self.toc.frames.iter().enumerate() {
            let raw = self.read_frame_raw(fid as u32)?;
            let frame_dict = if (entry.flags & PACK_FRAME_FLAG_USES_DICT) != 0 {
                Some(dict.as_ref().ok_or(StatamError::Corrupt {
                    reason: "frame requires dict on pack without dict section",
                })?)
            } else {
                None
            };
            let (_, _, decoded) = read_framed_pack_frame_with_decompressor(
                &raw,
                MAX_FRAME_RAW_BYTES,
                frame_dict,
                &mut dict_decomp,
            )?;
            if decoded.len() != entry.raw_len as usize {
                return Err(StatamError::Corrupt {
                    reason: "decompressed frame length mismatch during verify_full",
                });
            }
        }

        // 4. Verify every chunk location lies within its owning frame's raw_len.
        for loc in &self.toc.chunks {
            let frame = self
                .toc
                .frames
                .get(loc.frame_id as usize)
                .ok_or(StatamError::Corrupt {
                    reason: "chunk frame_id out of bounds during verify_full",
                })?;
            let end = loc.offset_in_frame.checked_add(loc.raw_len).ok_or(
                StatamError::BoundsExceeded {
                    reason: "chunk span overflow during verify_full",
                },
            )?;
            if end > frame.raw_len {
                return Err(StatamError::Corrupt {
                    reason: "chunk span exceeds owning frame raw_len during verify_full",
                });
            }
        }

        Ok(())
    }
}

fn validate_trailer_sections(
    ps: &PackPostscript,
    data_end: u64,
) -> Result<Vec<(u64, u64)>, StatamError> {
    let min_off = PACK_HEADER_LEN as u64;
    let mut sections: Vec<(u64, u64)> = Vec::with_capacity(4);
    if ps.chunk_count > 0 && ps.hash_table_off != 0 {
        let hash_bytes = (ps.chunk_count as u64)
            .checked_mul(16)
            .and_then(|n| n.checked_add(4))
            .ok_or(StatamError::BoundsExceeded {
                reason: "hash table size overflow",
            })?;
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
        if (ps.edge_table_len as usize) > MAX_BLOCK_RAW_BYTES + PACK_FRAME_HEADER_LEN {
            return Err(StatamError::BoundsExceeded {
                reason: "pack edge_table_len exceeds MAX_BLOCK_RAW_BYTES + PACK_FRAME_HEADER_LEN",
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

fn decompress_frame_chunks_as_bytes_with_toc<'a>(
    toc: &PackToc,
    fid: u32,
    raw: &[u8],
    cids: &[u32],
    dict: Option<&'a PackDecoderDict>,
    dict_decomp: &mut PackFrameDecompressor<'a>,
) -> Result<Vec<(u32, Bytes)>, StatamError> {
    let entry = &toc.frames[fid as usize];
    verify_raw_frame_matches_entry(raw, entry)?;
    let (_, _, decoded) =
        read_framed_pack_frame_with_decompressor(raw, MAX_FRAME_RAW_BYTES, dict, dict_decomp)?;
    if decoded.len() != entry.raw_len as usize {
        return Err(StatamError::Corrupt {
            reason: "decompressed frame length mismatch",
        });
    }
    let mut used_bytes = 0usize;
    for &cid in cids {
        let loc = toc.chunks.get(cid as usize).ok_or(StatamError::Corrupt {
            reason: "chunk_id out of bounds",
        })?;
        let start = loc.offset_in_frame as usize;
        let end = start
            .checked_add(loc.raw_len as usize)
            .ok_or(StatamError::Corrupt {
                reason: "chunk slice offset overflow",
            })?;
        if end > decoded.len() {
            return Err(StatamError::Corrupt {
                reason: "chunk slice out of decoded frame bounds",
            });
        }
        used_bytes = used_bytes.saturating_add(loc.raw_len as usize);
    }

    let mut out = Vec::with_capacity(cids.len());
    if used_bytes.saturating_mul(2) >= decoded.len()
        || decoded.len().saturating_sub(used_bytes) <= 16 * 1024
    {
        let frame_bytes = Bytes::from(decoded);
        for &cid in cids {
            let loc = &toc.chunks[cid as usize];
            let start = loc.offset_in_frame as usize;
            let end = start + loc.raw_len as usize;
            out.push((cid, frame_bytes.slice(start..end)));
        }
    } else {
        for &cid in cids {
            let loc = &toc.chunks[cid as usize];
            let start = loc.offset_in_frame as usize;
            let end = start + loc.raw_len as usize;
            out.push((cid, Bytes::copy_from_slice(&decoded[start..end])));
        }
    }
    Ok(out)
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
    let crc = pack_frame_crc(&raw[0..12], &raw[PACK_FRAME_HEADER_LEN..]);
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
            emit_hash_table: true,
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
            emit_hash_table: true,
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
        assert_eq!(b.options.target_frame_raw_bytes, 512 * 1024);

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
        let ts_crc = crc32c::crc32c_append(crc32c::crc32c(&ts_8k[0..8]), &ts_8k[TS02_HEADER_LEN..]);
        ts_8k[8..12].copy_from_slice(&ts_crc.to_le_bytes());

        toc_body.extend_from_slice(&fe.encode());
        for c in &r.toc().chunks {
            toc_body.extend_from_slice(&c.encode());
        }
        let mut ps = r.toc().postscript;
        let frame_total = PACK_FRAME_HEADER_LEN + fe.stored_len as usize;
        let frame_off = fe.offset as usize;
        let ts_off = frame_off + frame_total;
        let mut crafted = vec![0u8; ts_off + ts_8k.len()];
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
        let frame_raw = r.read_frame_raw(0).unwrap();
        crafted[frame_off..frame_off + frame_total].copy_from_slice(&frame_raw);
        crafted[ts_off..ts_off + ts_8k.len()].copy_from_slice(&ts_8k);

        ps.flags = hdr.flags;
        ps.hash_table_off = 0;
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

        // 9. FrameEntry vs PF header flags mismatch (without CRC update -> PF decode ChecksumMismatch;
        //    with recomputed PF header CRC -> Corrupt mismatch against FrameEntry).
        let frame_off = base_fe.offset as usize;
        let frame_end = frame_off + PACK_FRAME_HEADER_LEN + base_fe.stored_len as usize;
        let mut bad_pf_flags = bytes.clone();
        bad_pf_flags[frame_off + 3] = PACK_FRAME_FLAG_USES_DICT;
        assert!(matches!(
            read_framed_pack_frame(&bad_pf_flags[frame_off..frame_end], MAX_FRAME_RAW_BYTES),
            Err(StatamError::ChecksumMismatch { .. })
        ));
        let recomputed_flags_crc = pack_frame_crc(
            &bad_pf_flags[frame_off..frame_off + 12],
            &bad_pf_flags[frame_off + PACK_FRAME_HEADER_LEN..frame_end],
        );
        bad_pf_flags[frame_off + 12..frame_off + 16]
            .copy_from_slice(&recomputed_flags_crc.to_le_bytes());
        let r_bad_pf_flags =
            ChunkPackReader::open(Arc::new(SliceReader::new(bad_pf_flags))).unwrap();
        assert!(matches!(
            r_bad_pf_flags.read_chunks(&[0]),
            Err(StatamError::Corrupt { .. })
        ));

        // 10. FrameEntry vs PF header codec mismatch (with recomputed PF header CRC).
        let mut bad_pf_codec = bytes.clone();
        bad_pf_codec[frame_off + 2] = if base_fe.codec == Codec::Lz4 {
            Codec::Zstd as u8
        } else {
            Codec::Lz4 as u8
        };
        let recomputed_codec_crc = pack_frame_crc(
            &bad_pf_codec[frame_off..frame_off + 12],
            &bad_pf_codec[frame_off + PACK_FRAME_HEADER_LEN..frame_end],
        );
        bad_pf_codec[frame_off + 12..frame_off + 16]
            .copy_from_slice(&recomputed_codec_crc.to_le_bytes());
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
        let dict_start = dummy_dict_off as usize;
        let dict_end = dict_start + dummy_dict_len as usize;
        bad_dict_flags[dict_start + 3] = PACK_FRAME_FLAG_USES_DICT;
        let bad_dict_crc = pack_frame_crc(
            &bad_dict_flags[dict_start..dict_start + 12],
            &bad_dict_flags[dict_start + PACK_FRAME_HEADER_LEN..dict_end],
        );
        bad_dict_flags[dict_start + 12..dict_start + 16]
            .copy_from_slice(&bad_dict_crc.to_le_bytes());
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
                emit_hash_table: true,
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
                emit_hash_table: true,
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
                    emit_hash_table: true,
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

    #[test]
    fn flat_edge_table_caches_matches_edges_and_rejects_corruption() {
        let opts = PackWriteOptions {
            codec: test_codec(),
            target_frame_raw_bytes: 16 * 1024,
            ..PackWriteOptions::default()
        };
        let mut pb = ChunkPackBuilder::new([0x41u8; 16], opts);
        let chunks = [
            b"short_chunk".to_vec(),
            b"HEAD_0123456789_middle_content_abcdefghijklmnopqrstuvwxyz_TAIL_9876543210".repeat(4),
            vec![b'M'; 128],
        ];
        for c in &chunks {
            pb.intern_chunk(c).unwrap();
        }
        let mut pack_bytes = Vec::new();
        pb.finish(&mut pack_bytes).unwrap();

        let reader = ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes.clone()))).unwrap();
        let flat1 = reader.read_edge_table_flat().unwrap();
        let flat2 = reader.read_edge_table_flat().unwrap();
        assert!(Arc::ptr_eq(&flat1, &flat2));
        assert_eq!(flat1.len(), chunks.len());
        assert!(!flat1.is_empty());
        assert!(flat1.head(999).is_empty());
        assert!(flat1.tail(999).is_empty());

        let legacy = reader.read_edge_table().unwrap();
        assert_eq!(legacy.len(), chunks.len());
        for (cid, (exp_h, exp_t)) in legacy.iter().enumerate() {
            assert_eq!(flat1.head(cid as u32), exp_h.as_slice());
            assert_eq!(flat1.tail(cid as u32), exp_t.as_slice());
        }

        // Corrupt edge_table_len out of bounds.
        let mut bad_oob = pack_bytes.clone();
        let ps_start = bad_oob.len() - PACK_POSTSCRIPT_LEN;
        let mut ps = PackPostscript::decode(&bad_oob[ps_start..]).unwrap();
        ps.edge_table_off = bad_oob.len() as u64;
        ps.edge_table_len = 64;
        bad_oob[ps_start..].copy_from_slice(&ps.encode());
        assert!(matches!(
            ChunkPackReader::open(Arc::new(SliceReader::new(bad_oob))),
            Err(StatamError::Corrupt { .. })
        ));

        // Corrupt EdgeTable PF frame CRC -> read_edge_table_flat returns ChecksumMismatch.
        let mut bad_crc = pack_bytes;
        let ps_ok = reader.postscript();
        bad_crc[ps_ok.edge_table_off as usize + PACK_FRAME_HEADER_LEN] ^= 0xFF;
        let r_bad_crc = ChunkPackReader::open(Arc::new(SliceReader::new(bad_crc))).unwrap();
        assert!(matches!(
            r_bad_crc.read_edge_table_flat(),
            Err(StatamError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn boundary_tracking_omits_dead_edge_table_and_zeros_non_boundary_chunks() {
        let opts = PackWriteOptions {
            codec: Codec::None,
            target_frame_raw_bytes: 32 * 1024,
            enable_trigram_skip: true,
            ..PackWriteOptions::default()
        };

        // 1. Single-chunk-only pack with boundary tracking -> edge_table_len == 0.
        let mut single_pb = ChunkPackBuilder::new([0x51u8; 16], opts);
        single_pb.enable_boundary_tracking();
        for i in 0..80u32 {
            let chunk = format!(
                "{{\"single\":{i:04},\"body\":\"{}\"}}",
                "single_chunk_only_text_".repeat(20)
            );
            let cid = single_pb.intern_chunk(chunk.as_bytes()).unwrap();
            single_pb.mark_boundary_chunks(&[cid]);
        }
        let mut single_bytes = Vec::new();
        let single_stats = single_pb.finish(&mut single_bytes).unwrap();
        assert_eq!(single_stats.edge_table_bytes, 0);

        let single_reader = ChunkPackReader::open_with_tail_hint(
            Arc::new(SliceReader::new(single_bytes)),
            Some(256),
        )
        .unwrap();
        assert_eq!(single_reader.postscript().edge_table_len, 0);
        assert_eq!(single_reader.postscript().edge_table_off, 0);
        let flat_empty = single_reader.read_edge_table_flat().unwrap();
        assert!(flat_empty.is_empty());
        assert_eq!(flat_empty.len(), 0);
        assert!(flat_empty.head(0).is_empty());
        assert!(flat_empty.tail(0).is_empty());
        assert!(Arc::ptr_eq(
            &flat_empty,
            &single_reader.read_edge_table_flat().unwrap()
        ));
        assert_eq!(
            single_reader.read_edge_table().unwrap().len(),
            single_stats.chunk_count as usize
        );
        let bloom_may_match = |b: &TrigramBloom, needle: &[u8]| -> bool {
            let mut hashes = Vec::new();
            crate::simd::trigram::extract_trigram_hashes(needle, false, &mut hashes);
            b.may_contain_hashes(&hashes)
        };
        if let Some((tail_b, head_b)) = single_reader.read_edge_trigram_blooms().unwrap() {
            assert!(tail_b.as_bytes().iter().all(|&b| b == 0));
            assert!(head_b.as_bytes().iter().all(|&b| b == 0));
            assert!(!bloom_may_match(&tail_b, b"single_chunk"));
            assert!(!bloom_may_match(&head_b, b"single_chunk"));
        }

        // 2. Mixed single-chunk + multi-chunk pack -> non-boundary edges zeroed, boundary preserved,
        //    and edge trigram blooms exclude non-boundary-only trigrams.
        let mut mixed_pb = ChunkPackBuilder::new([0x52u8; 16], opts);
        mixed_pb.enable_boundary_tracking();
        let mut non_boundary_cids = Vec::new();
        let words = [
            "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
            "juliet", "kilo", "lima", "mike", "november", "oscar", "papa",
        ];
        for i in 0..80u32 {
            let mut body = String::with_capacity(420);
            for w in 0..50usize {
                body.push_str(words[(i as usize + w * 3) % words.len()]);
                body.push(' ');
            }
            let chunk = format!("NONBND_{i:04}_{body}_ZQX");
            let cid = mixed_pb.intern_chunk(chunk.as_bytes()).unwrap();
            non_boundary_cids.push(cid);
        }
        let b_left = format!("{}BND_TAIL_EDGE", "left_pad_".repeat(20));
        let b_right = format!("BND_HEAD_EDGE{}", "right_pad_".repeat(20));
        let cid_l = mixed_pb.intern_chunk(b_left.as_bytes()).unwrap();
        let cid_r = mixed_pb.intern_chunk(b_right.as_bytes()).unwrap();
        mixed_pb.mark_boundary_chunks(&[cid_l, cid_r]);

        let mut mixed_bytes = Vec::new();
        let mixed_stats = mixed_pb.finish(&mut mixed_bytes).unwrap();
        assert!(mixed_stats.edge_table_bytes > 0);

        let mixed_reader = ChunkPackReader::open(Arc::new(SliceReader::new(mixed_bytes))).unwrap();
        let flat_mixed = mixed_reader.read_edge_table_flat().unwrap();
        assert_eq!(flat_mixed.len(), mixed_stats.chunk_count as usize);
        for &cid in &non_boundary_cids {
            assert!(flat_mixed.head(cid).is_empty());
            assert!(flat_mixed.tail(cid).is_empty());
        }
        assert_eq!(flat_mixed.head(cid_l), &b_left.as_bytes()[..EDGE_LEN]);
        assert_eq!(
            flat_mixed.tail(cid_l),
            &b_left.as_bytes()[b_left.len() - EDGE_LEN..]
        );
        assert_eq!(flat_mixed.head(cid_r), &b_right.as_bytes()[..EDGE_LEN]);
        assert_eq!(
            flat_mixed.tail(cid_r),
            &b_right.as_bytes()[b_right.len() - EDGE_LEN..]
        );

        let (tail_b, head_b) = mixed_reader.read_edge_trigram_blooms().unwrap().unwrap();
        assert!(!bloom_may_match(&head_b, b"NONBND"));
        assert!(!bloom_may_match(&tail_b, b"ZQX"));
        assert!(bloom_may_match(&tail_b, b"BND_TAIL_EDGE"));
        assert!(bloom_may_match(&head_b, b"BND_HEAD_EDGE"));
    }

    #[test]
    fn emit_hash_table_false_omits_hash_table_and_rejects_corrupt_nonzero_offset() {
        let pack_id = [0x61u8; 16];
        let base_opts = PackWriteOptions {
            codec: test_codec(),
            emit_hash_table: true,
            ..PackWriteOptions::default()
        };
        let no_hash_opts = PackWriteOptions {
            emit_hash_table: false,
            ..base_opts
        };

        let mut pb_with = ChunkPackBuilder::new(pack_id, base_opts);
        let mut pb_without = ChunkPackBuilder::new(pack_id, no_hash_opts);
        for i in 0..32u32 {
            let chunk = format!("hash_opt_chunk_{i:04}_{}", "data_".repeat(40));
            pb_with.intern_chunk(chunk.as_bytes()).unwrap();
            pb_without.intern_chunk(chunk.as_bytes()).unwrap();
        }
        let mut bytes_with = Vec::new();
        let stats_with = pb_with.finish(&mut bytes_with).unwrap();
        let mut bytes_without = Vec::new();
        let stats_without = pb_without.finish(&mut bytes_without).unwrap();

        assert_eq!(
            stats_with.total_bytes - stats_without.total_bytes,
            u64::from(stats_with.chunk_count) * 16 + 4
        );

        let reader = ChunkPackReader::open_with_tail_hint(
            Arc::new(SliceReader::new(bytes_without.clone())),
            Some(128),
        )
        .unwrap();
        assert_eq!(reader.postscript().hash_table_off, 0);
        assert!(reader.read_hash_table().unwrap().is_empty());

        let got = reader.read_chunks(&[0, 15, 31]).unwrap();
        assert_eq!(got.len(), 3);

        // Corrupt non-zero hash_table_off (< PACK_HEADER_LEN or out of bounds) must still be rejected.
        let mut bad_low = bytes_without.clone();
        let ps_start = bad_low.len() - PACK_POSTSCRIPT_LEN;
        let mut ps = PackPostscript::decode(&bad_low[ps_start..]).unwrap();
        ps.hash_table_off = 8;
        bad_low[ps_start..].copy_from_slice(&ps.encode());
        assert!(matches!(
            ChunkPackReader::open(Arc::new(SliceReader::new(bad_low))),
            Err(StatamError::Corrupt { .. })
        ));

        let mut bad_high = bytes_without;
        ps.hash_table_off = bad_high.len() as u64;
        bad_high[ps_start..].copy_from_slice(&ps.encode());
        assert!(matches!(
            ChunkPackReader::open(Arc::new(SliceReader::new(bad_high))),
            Err(StatamError::Corrupt { .. })
        ));
    }

    #[test]
    fn flat_edge_table_debug_is_private_and_midstream_boundary_tracking_preserves_existing_edges() {
        let mut pb = ChunkPackBuilder::new(
            [0x73u8; 16],
            PackWriteOptions {
                codec: test_codec(),
                enable_trigram_skip: true,
                ..PackWriteOptions::default()
            },
        );
        let pre_chunk = format!("SECRET_PRE_HEAD_{}_SECRET_PRE_TAIL", "payload_".repeat(40));
        let cid0 = pb.intern_chunk(pre_chunk.as_bytes()).unwrap();
        // Enable boundary tracking mid-stream after chunks have already been interned.
        pb.enable_boundary_tracking();
        let post_chunk = format!("POST_ONLY_CHUNK_{}", "data_".repeat(40));
        let cid1 = pb.intern_chunk(post_chunk.as_bytes()).unwrap();

        let mut bytes = Vec::new();
        let stats = pb.finish(&mut bytes).unwrap();
        assert!(stats.edge_table_bytes > 0);

        let reader = ChunkPackReader::open(Arc::new(SliceReader::new(bytes))).unwrap();
        let flat = reader.read_edge_table_flat().unwrap();
        assert_eq!(flat.head(cid0), &pre_chunk.as_bytes()[..EDGE_LEN]);
        assert_eq!(
            flat.tail(cid0),
            &pre_chunk.as_bytes()[pre_chunk.len() - EDGE_LEN..]
        );
        assert!(flat.head(cid1).is_empty());
        assert!(flat.tail(cid1).is_empty());
        assert!(flat.head(u32::MAX).is_empty());
        assert!(flat.tail(u32::MAX).is_empty());

        let dbg = format!("{flat:?}");
        assert!(dbg.contains("chunk_count"));
        assert!(dbg.contains("head_bytes"));
        assert!(dbg.contains("tail_bytes"));
        assert!(!dbg.contains("SECRET_PRE_HEAD"));
        assert!(!dbg.contains("SECRET_PRE_TAIL"));
    }

    #[test]
    fn m10_empty_chunk_alone_and_followed_by_append_raw_frame_roundtrips() {
        // 1. intern_chunk(b"") alone.
        let mut pb = ChunkPackBuilder::new(
            [0x10u8; 16],
            PackWriteOptions {
                codec: test_codec(),
                enable_trigram_skip: false,
                ..PackWriteOptions::default()
            },
        );
        let cid0 = pb.intern_chunk(b"").unwrap();
        assert_eq!(cid0, 0);
        let mut pack_bytes = Vec::new();
        let stats = pb.finish(&mut pack_bytes).unwrap();
        assert_eq!(stats.chunk_count, 1);
        assert_eq!(stats.frame_count, 1);
        let reader = ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes))).unwrap();
        let got = reader.read_chunks(&[cid0]).unwrap();
        assert_eq!(got[&cid0], Vec::<u8>::new());

        // 2. Build a donor single-chunk pack to supply a raw frame.
        let mut donor = ChunkPackBuilder::new(
            [0x11u8; 16],
            PackWriteOptions {
                codec: test_codec(),
                enable_trigram_skip: false,
                ..PackWriteOptions::default()
            },
        );
        let donor_payload = b"non-empty-raw-frame-payload-bytes".repeat(16);
        let dcid = donor.intern_chunk(&donor_payload).unwrap();
        let mut donor_bytes = Vec::new();
        donor.finish(&mut donor_bytes).unwrap();
        let donor_reader = ChunkPackReader::open(Arc::new(SliceReader::new(donor_bytes))).unwrap();
        let raw_frame = donor_reader.read_frame_raw(0).unwrap();
        let f_entry = donor_reader.toc().frames[0];
        let loc = donor_reader.toc().chunks[dcid as usize];
        let hashes = donor_reader.read_hash_table().unwrap();
        let edges = donor_reader.read_edge_table().unwrap();

        // intern_chunk(b"") followed by append_raw_frame must yield distinct chunk_ids (0 and 1).
        let mut pb2 = ChunkPackBuilder::new(
            [0x12u8; 16],
            PackWriteOptions {
                codec: test_codec(),
                enable_trigram_skip: false,
                ..PackWriteOptions::default()
            },
        );
        let empty_cid = pb2.intern_chunk(b"").unwrap();
        assert_eq!(empty_cid, 0);
        let appended = pb2
            .append_raw_frame(
                &raw_frame,
                f_entry,
                &[(hashes[0], loc.offset_in_frame, loc.raw_len)],
                &[],
                &edges,
            )
            .unwrap();
        assert_eq!(appended, vec![1]);

        let mut bytes2 = Vec::new();
        let stats2 = pb2.finish(&mut bytes2).unwrap();
        assert_eq!(stats2.chunk_count, 2);
        let reader2 = ChunkPackReader::open(Arc::new(SliceReader::new(bytes2))).unwrap();
        let got2 = reader2.read_chunks(&[0, 1]).unwrap();
        assert_eq!(got2[&0], Vec::<u8>::new());
        assert_eq!(got2[&1], donor_payload);
    }

    #[test]
    fn b2_dedup_hit_on_uncommitted_chunk_and_append_raw_frame_empty_edge_guard() {
        let mut pb = ChunkPackBuilder::new(
            [0x22u8; 16],
            PackWriteOptions {
                codec: test_codec(),
                target_frame_raw_bytes: 64 * 1024,
                enable_trigram_skip: false,
                ..PackWriteOptions::default()
            },
        );
        // Dedup hit on uncommitted chunk still in cur_chunks (existing_id >= self.edges.len()).
        let chunk_a = b"uncommitted-chunk-A-payload-content".repeat(8);
        let cid0 = pb.intern_chunk(&chunk_a).unwrap();
        let cid0_dup = pb.intern_chunk(&chunk_a).unwrap();
        assert_eq!(cid0, cid0_dup);

        // Append a raw frame with empty edges for non-empty chunk X; it must NOT poison self.dedup.
        let chunk_x = b"duplicate-chunk-X-for-edge-guard-content".repeat(8);
        let mut donor = ChunkPackBuilder::new(
            [0x23u8; 16],
            PackWriteOptions {
                codec: test_codec(),
                enable_trigram_skip: false,
                ..PackWriteOptions::default()
            },
        );
        donor.append_raw_bypass_with_edge(&chunk_x, false).unwrap();
        let mut donor_bytes = Vec::new();
        donor.finish(&mut donor_bytes).unwrap();
        let dr = ChunkPackReader::open(Arc::new(SliceReader::new(donor_bytes))).unwrap();
        let raw_frame = dr.read_frame_raw(0).unwrap();
        let f_entry = dr.toc().frames[0];
        let loc = dr.toc().chunks[0];
        let hashes = dr.read_hash_table().unwrap();

        let appended = pb
            .append_raw_frame(
                &raw_frame,
                f_entry,
                &[(hashes[0], loc.offset_in_frame, loc.raw_len)],
                &[],
                &[(Vec::new(), Vec::new())],
            )
            .unwrap();
        let x_empty_cid = appended[0];
        let x_interned_cid = pb.intern_chunk(&chunk_x).unwrap();
        assert_ne!(x_empty_cid, x_interned_cid);

        let mut out_bytes = Vec::new();
        pb.finish(&mut out_bytes).unwrap();
        let reader = ChunkPackReader::open(Arc::new(SliceReader::new(out_bytes))).unwrap();
        let edges = reader.read_edge_table().unwrap();
        assert_eq!(edges[x_interned_cid as usize].0, &chunk_x[..EDGE_LEN]);
        assert_eq!(
            edges[x_interned_cid as usize].1,
            &chunk_x[chunk_x.len() - EDGE_LEN..]
        );
    }

    #[test]
    fn b7_pack_read_chunks_rejects_aliased_chunks_before_frame_io() {
        // Build a synthetic .stpk pack with 1 compressed 4 MiB frame and 65 ChunkLocs aliasing [0..4 MiB] (260 MiB > 256 MiB).
        let pack_id = [0xB7u8; 16];
        let raw_len = 4 * 1024 * 1024usize;
        let frame_raw = vec![b'A'; raw_len];
        let mut framed = Vec::new();
        let info = write_framed_pack_frame(&frame_raw, test_codec(), 0, 3, &mut framed).unwrap();

        let mut buf = Vec::new();
        buf.extend_from_slice(&[0u8; PACK_HEADER_LEN]);
        let frame_off = buf.len() as u64;
        buf.extend_from_slice(&framed);

        let chunk_count = 65u32;
        let f_entry = FrameEntry {
            offset: frame_off,
            stored_len: info.stored_len,
            raw_len: info.raw_len,
            codec: info.codec,
            flags: 0,
            dict_id: 0,
            first_chunk_id: 0,
            chunk_count,
            filter_off: 0,
            filter_len: 0,
            crc32c: info.crc32c,
        };
        let mut toc_raw = Vec::new();
        toc_raw.extend_from_slice(&f_entry.encode());
        for _ in 0..chunk_count {
            toc_raw.extend_from_slice(
                &ChunkLoc {
                    frame_id: 0,
                    offset_in_frame: 0,
                    raw_len: raw_len as u32,
                }
                .encode(),
            );
        }
        let toc_crc = crc32c::crc32c(&toc_raw);
        let toc_stored_len = (toc_raw.len() + PACK_POSTSCRIPT_LEN) as u32;
        buf.extend_from_slice(&toc_raw);

        let ps = PackPostscript {
            toc_stored_len,
            toc_raw_len: toc_raw.len() as u32,
            format_version: FORMAT_VERSION,
            flags: 0,
            required_features: 0,
            pack_id,
            frame_count: 1,
            chunk_count,
            hash_table_off: 0,
            trigram_off: 0,
            trigram_len: 0,
            edge_table_len: 0,
            edge_table_off: 0,
            dict_off: 0,
            dict_len: 0,
            enc_envelope_offset: 0,
            enc_envelope_len: 0,
            reserved: 0,
            toc_crc32c: toc_crc,
        };
        let header = PackHeader {
            format_version: FORMAT_VERSION,
            flags: 0,
            required_features: 0,
            pack_id,
            enc_envelope_offset: 0,
            enc_envelope_len: 0,
            reserved: 0,
        };
        buf[0..PACK_HEADER_LEN].copy_from_slice(&header.encode());
        buf.extend_from_slice(&ps.encode());

        let reader = ChunkPackReader::open(Arc::new(SimulatedS3Reader::from_bytes(
            buf,
            SimulatedS3Config::default(),
        )))
        .unwrap();
        let all_cids: Vec<u32> = (0..chunk_count).collect();
        let err = reader.read_chunks(&all_cids).unwrap_err();
        assert!(matches!(err, StatamError::BoundsExceeded { .. }));
    }

    #[test]
    fn m19_pack_open_rejects_short_fallback_toc_read() {
        let mut pb = ChunkPackBuilder::new(
            [0x19u8; 16],
            PackWriteOptions {
                codec: test_codec(),
                enable_trigram_skip: false,
                compress_toc_lz4: false,
                ..PackWriteOptions::default()
            },
        );
        for i in 0..4u32 {
            pb.intern_chunk(format!("chunk-payload-{i}-data").as_bytes())
                .unwrap();
        }
        let mut pack_bytes = Vec::new();
        pb.finish(&mut pack_bytes).unwrap();

        struct TruncatingFallbackReader {
            data: Bytes,
        }
        impl RangeReader for TruncatingFallbackReader {
            fn file_len(&self) -> u64 {
                self.data.len() as u64
            }
            fn read_range(&self, range: Range<u64>) -> Result<Bytes, StatamError> {
                let s = range.start as usize;
                let e = range.end as usize;
                if e == self.data.len() {
                    Ok(self.data.slice(s..e))
                } else {
                    // Return 1 byte short on the fallback TOC body read.
                    Ok(self.data.slice(s..e.saturating_sub(1)))
                }
            }
        }

        // tail_hint = PACK_POSTSCRIPT_LEN forces fallback TOC body read.
        let r = Arc::new(TruncatingFallbackReader {
            data: Bytes::from(pack_bytes),
        });
        let err =
            ChunkPackReader::open_with_tail_hint(r, Some(PACK_POSTSCRIPT_LEN as u32)).unwrap_err();
        assert!(matches!(
            err,
            StatamError::Corrupt {
                reason: "short read on pack TOC body"
            }
        ));
    }

    #[test]
    fn m2_ts02_and_hash_table_crcs_and_empty_pack_hash_table_off_zero() {
        // 1. Empty pack with emit_hash_table=true must emit hash_table_off == 0.
        let empty_pb = ChunkPackBuilder::new(
            [0x71u8; 16],
            PackWriteOptions {
                codec: test_codec(),
                emit_hash_table: true,
                ..PackWriteOptions::default()
            },
        );
        let mut empty_bytes = Vec::new();
        empty_pb.finish(&mut empty_bytes).unwrap();
        let empty_r = ChunkPackReader::open(Arc::new(SliceReader::new(empty_bytes))).unwrap();
        assert_eq!(empty_r.postscript().chunk_count, 0);
        assert_eq!(empty_r.postscript().hash_table_off, 0);
        assert!(empty_r.read_hash_table().unwrap().is_empty());

        // 2. Non-empty pack with TS02 + ChunkHashTable: single-bit flips trigger ChecksumMismatch.
        let mut pb = ChunkPackBuilder::new(
            [0x72u8; 16],
            PackWriteOptions {
                codec: test_codec(),
                enable_trigram_skip: true,
                emit_hash_table: true,
                ..PackWriteOptions::default()
            },
        );
        for c in 0..180u32 {
            let mut chunk =
                format!("hdr_{c:04}_{:08x}_", c.wrapping_mul(2_654_435_761)).into_bytes();
            for j in 0..32u32 {
                chunk.extend_from_slice(
                    format!("token_{:02}_alpha_beta_gamma_", (c + j) % 25).as_bytes(),
                );
            }
            pb.intern_chunk(&chunk).unwrap();
        }
        let mut pack_bytes = Vec::new();
        pb.finish(&mut pack_bytes).unwrap();
        let ok_r = ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes.clone()))).unwrap();
        let ps = ok_r.postscript();
        assert!(ps.hash_table_off > 0);
        assert!(ps.trigram_len > 0);
        assert_eq!(
            ok_r.read_hash_table().unwrap().len(),
            ps.chunk_count as usize
        );
        assert!(ok_r.read_edge_trigram_blooms().unwrap().is_some());

        // Flip 1 bit in TS02 body -> ChecksumMismatch.
        let mut bad_ts = pack_bytes.clone();
        bad_ts[ps.trigram_off as usize + TS02_HEADER_LEN + 10] ^= 0x01;
        let r_bad_ts = ChunkPackReader::open(Arc::new(SliceReader::new(bad_ts))).unwrap();
        assert!(matches!(
            r_bad_ts.read_trigram_section(),
            Err(StatamError::ChecksumMismatch {
                context: "pack trigram section"
            })
        ));

        // Flip 1 bit in ChunkHashTable body -> ChecksumMismatch.
        let mut bad_ht = pack_bytes.clone();
        bad_ht[ps.hash_table_off as usize + 5] ^= 0x01;
        let r_bad_ht = ChunkPackReader::open(Arc::new(SliceReader::new(bad_ht))).unwrap();
        assert!(matches!(
            r_bad_ht.read_hash_table(),
            Err(StatamError::ChecksumMismatch {
                context: "pack hash table"
            })
        ));

        // Flip 1 bit in PF header -> append_raw_frame rejects ChecksumMismatch.
        let fe0 = ok_r.toc().frames[0];
        let mut raw_f0 = ok_r.read_frame_raw(0).unwrap().to_vec();
        let hashes = ok_r.read_hash_table().unwrap();
        let edges = ok_r.read_edge_table().unwrap();
        let frame_chunks: Vec<([u8; 16], u32, u32)> = ok_r.toc().chunks
            [fe0.first_chunk_id as usize..(fe0.first_chunk_id + fe0.chunk_count) as usize]
            .iter()
            .enumerate()
            .map(|(i, loc)| {
                (
                    hashes[fe0.first_chunk_id as usize + i],
                    loc.offset_in_frame,
                    loc.raw_len,
                )
            })
            .collect();
        raw_f0[2] = Codec::None as u8;
        let mut fe_mismatched = fe0;
        fe_mismatched.codec = Codec::None;
        let mut dst_pb = ChunkPackBuilder::new([0x73u8; 16], PackWriteOptions::default());
        assert!(matches!(
            dst_pb.append_raw_frame(&raw_f0, fe_mismatched, &frame_chunks, &[], &edges),
            Err(StatamError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn upgrade2_adaptive_prose_frame_sizing_1t_eq_4t_and_trigram_pruning() {
        // Generate > 4.5 MiB of unique high-entropy English-like prose (~1,200 x ~4 KiB chunks)
        // so 512 KiB frames saturate/exceed 0.55 trigram density and trigger 64 KiB adaptation at 2 MiB.
        const NUM_CHUNKS: usize = 1_200;
        let alpha = b"abcdefghijklmnopqrstuvwxyz";
        let mut chunks: Vec<Vec<u8>> = Vec::with_capacity(NUM_CHUNKS);
        let mut rng_state = 0x1234_5678_9abc_def0u64;
        let mut next_u64 = || {
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 7;
            rng_state ^= rng_state << 17;
            rng_state
        };

        for idx in 0..NUM_CHUNKS {
            let mut doc = Vec::with_capacity(4_100);
            doc.extend_from_slice(format!("article_{idx:05}: ").as_bytes());
            while doc.len() < 4_000 {
                let wlen = 4 + (next_u64() as usize % 5);
                for _ in 0..wlen {
                    let b = alpha[(next_u64() as usize) % alpha.len()];
                    doc.push(b);
                }
                doc.push(b' ');
            }
            if idx == 700 || idx == 1_050 {
                doc.extend_from_slice(b" quantum_flux_calibration_sentinel ");
            }
            chunks.push(doc);
        }

        let build_pack = |threads: usize| -> Vec<u8> {
            let opts = PackWriteOptions {
                codec: test_codec(),
                zstd_level: 1,
                write_threads: threads,
                ..PackWriteOptions::default()
            };
            let mut pb = ChunkPackBuilder::new([0xA2u8; 16], opts);
            for ch in &chunks {
                pb.intern_chunk(ch).unwrap();
            }
            let mut out = Vec::new();
            pb.finish(&mut out).unwrap();
            out
        };

        let bytes_1t = build_pack(1);
        let bytes_4t = build_pack(4);
        assert_eq!(
            bytes_1t, bytes_4t,
            "1T and 4T adaptive prose packs must be bit-for-bit identical"
        );

        let reader = ChunkPackReader::open(Arc::new(SliceReader::new(bytes_1t))).unwrap();
        let toc = reader.toc();
        assert!(
            toc.frames.len() >= 30,
            "expected adapted 64 KiB frames after 2 MiB, got only {} frames",
            toc.frames.len()
        );
        // First 4 frames (~2 MiB) stay near 512 KiB; subsequent adapted frames are <= 64 KiB + 1 chunk.
        assert!(toc.frames[0].raw_len as usize >= 400 * 1024);
        let adapted_frames = &toc.frames[5..toc.frames.len() - 1];
        assert!(!adapted_frames.is_empty());
        for f in adapted_frames {
            assert!(
                (f.raw_len as usize) <= ADAPTIVE_PROSE_FRAME_RAW_BYTES,
                "adapted frame raw_len {} exceeded 64 KiB",
                f.raw_len
            );
            assert!(
                f.filter_len > 0,
                "adapted prose frame should emit non-saturated trigram bloom"
            );
        }

        let trigrams = reader.read_trigram_section().unwrap();
        let needle = b"quantum_flux_calibration_sentinel";
        let mut needle_hashes = Vec::new();
        crate::simd::trigram::extract_trigram_hashes(needle, false, &mut needle_hashes);
        let mut pruned = 0usize;
        for (fid, f) in toc.frames.iter().enumerate() {
            if let Some(bloom) = &trigrams[fid] {
                assert!(bloom.bit_density() <= MAX_TRIGRAM_DENSITY);
                if !bloom.may_contain_hashes(&needle_hashes) {
                    pruned += 1;
                }
            } else {
                assert_eq!(f.filter_len, 0);
            }
        }
        assert!(
            pruned * 100 >= toc.frames.len() * 80,
            "expected >= 80% trigram frame pruning on rare needle, pruned {pruned}/{}",
            toc.frames.len()
        );

        let hits = crate::payload::search::search_pack_chunks(&reader, needle, false).unwrap();
        assert_eq!(hits, vec![700, 1_050]);
    }

    #[test]
    fn upgrade2_high_dedup_stream_stays_at_512k_frames() {
        let mut unique_chunks: Vec<Vec<u8>> = Vec::with_capacity(700);
        let mut rng_state = 0xfeed_face_cafe_beefu64;
        let alpha = b"abcdefghijklmnopqrstuvwxyz";
        for idx in 0..700usize {
            let mut doc = Vec::with_capacity(4_100);
            doc.extend_from_slice(format!("agent_{idx:04}: ").as_bytes());
            while doc.len() < 4_000 {
                rng_state ^= rng_state << 13;
                rng_state ^= rng_state >> 7;
                rng_state ^= rng_state << 17;
                doc.push(alpha[(rng_state as usize) % alpha.len()]);
            }
            unique_chunks.push(doc);
        }

        let opts = PackWriteOptions {
            codec: test_codec(),
            zstd_level: 1,
            ..PackWriteOptions::default()
        };
        let mut pb = ChunkPackBuilder::new([0xA3u8; 16], opts);
        // Emit each unique chunk twice -> 50% dedup hit rate (>> 5% threshold).
        for ch in &unique_chunks {
            pb.intern_chunk(ch).unwrap();
            pb.intern_chunk(ch).unwrap();
        }
        assert_eq!(
            pb.effective_target_frame_raw_bytes, DEFAULT_TARGET_FRAME_RAW_BYTES,
            "high-dedup stream must stay at 512 KiB target frame size"
        );
        let mut out = Vec::new();
        pb.finish(&mut out).unwrap();
        let reader = ChunkPackReader::open(Arc::new(SliceReader::new(out))).unwrap();
        // ~2.8 MiB unique chunks at 512 KiB/frame -> ~6 frames (never downsized to ~45 frames).
        assert!(
            reader.toc().frames.len() <= 8,
            "expected <= 8 frames at 512 KiB, got {}",
            reader.toc().frames.len()
        );
    }

    #[test]
    fn upgrade2_max_bloom_bytes_writer_guard_prevents_oversized_ts02() {
        let opts = PackWriteOptions {
            codec: test_codec(),
            zstd_level: 1,
            target_frame_raw_bytes: 8 * 1024,
            enable_trigram_skip: true,
            ..PackWriteOptions::default()
        };
        let mut pb = ChunkPackBuilder::new([0xA4u8; 16], opts);
        // Simulate a pack whose TS02 trigram_buf is already within 256 bytes of MAX_BLOOM_BYTES.
        let near_cap = MAX_BLOOM_BYTES - TS02_HEADER_LEN - 2 * 8_192 - 128;
        pb.trigram_buf = vec![0u8; near_cap];

        for i in 0..8u32 {
            let chunk = format!("overflow_guard_chunk_{i:02}_").repeat(120);
            pb.intern_chunk(chunk.as_bytes()).unwrap();
        }
        let mut out = Vec::new();
        let stats = pb.finish(&mut out).unwrap();
        assert!(
            (stats.trigram_section_bytes as usize) <= MAX_BLOOM_BYTES,
            "TS02 section {} exceeded MAX_BLOOM_BYTES {}",
            stats.trigram_section_bytes,
            MAX_BLOOM_BYTES
        );
        let reader = ChunkPackReader::open(Arc::new(SliceReader::new(out))).unwrap();
        for f in &reader.toc().frames {
            assert_eq!(
                f.filter_len, 0,
                "frame exceeding MAX_BLOOM_BYTES guard must omit trigram filter"
            );
        }
        assert_eq!(
            reader.read_trigram_section().unwrap().len(),
            reader.toc().frames.len()
        );
    }

    #[test]
    fn pass13a_inline_trigram_and_edge_table_slicing_zero_extra_reads() {
        // Build a pack with both TrigramSkipSection and EdgeTable sitting right before the TOC
        // inside the default 256 KiB tail prefetch window.
        let opts = PackWriteOptions {
            codec: test_codec(),
            zstd_level: 1,
            target_frame_raw_bytes: 64 * 1024,
            enable_trigram_skip: true,
            compress_toc_lz4: true,
            write_threads: 1,
            emit_hash_table: true,
        };
        let mut pb = ChunkPackBuilder::new([0xA5u8; 16], opts);
        pb.enable_boundary_tracking();
        let mut cids = Vec::new();
        let words = [
            "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
            "juliet", "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo",
            "sierra", "tango", "uniform", "victor", "whiskey", "xray", "yankee", "zulu",
        ];
        let mut rng = 0x1234_5678_9abc_def0u64;
        for i in 0..48u32 {
            let mut chunk = format!("doc_{i:04}:");
            for _ in 0..240 {
                rng = rng
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                chunk.push(' ');
                chunk.push_str(words[((rng >> 56) as usize) % words.len()]);
            }
            cids.push(pb.intern_chunk(chunk.as_bytes()).unwrap());
        }
        pb.mark_boundary_chunks(&cids[0..4]);

        let mut pack_bytes = Vec::new();
        let stats = pb.finish(&mut pack_bytes).unwrap();
        assert!(stats.trigram_section_bytes > 0);
        assert!(stats.edge_table_bytes > 0);
        assert!(
            (stats.trigram_section_bytes + stats.edge_table_bytes + stats.toc_stored_bytes)
                < PACK_DEFAULT_TAIL_HINT,
            "pack trailers must fit inside 256 KiB default tail hint"
        );

        // Disable S3 block cache so any extra read_range would immediately show up in waves & gets.
        let s3 = Arc::new(SimulatedS3Reader::from_bytes(
            pack_bytes.clone(),
            SimulatedS3Config {
                cache_capacity_blocks: 0,
                ..SimulatedS3Config::default()
            },
        ));
        let reader = ChunkPackReader::open(s3.clone()).unwrap();
        assert_eq!(s3.counters().waves, 1);
        assert_eq!(s3.counters().gets, 1);
        assert!(reader.inline_trigram.is_some());
        assert!(reader.inline_edge_table.is_some());

        // Reading trigram blooms AND edge table must issue 0 extra range reads!
        let blooms = reader.read_trigram_section().unwrap();
        assert_eq!(blooms.len(), reader.toc().frames.len());
        let edge_blooms = reader.read_edge_trigram_blooms().unwrap();
        assert!(edge_blooms.is_some());
        let edges = reader.read_edge_table_flat().unwrap();
        assert_eq!(edges.len(), cids.len());
        assert_eq!(
            s3.counters().waves,
            1,
            "inline trigram + edge table must incur 0 extra waves"
        );
        assert_eq!(
            s3.counters().gets,
            1,
            "inline trigram + edge table must incur 0 extra GETs"
        );

        // Custom small tail hint must still work identically by fetching on demand.
        let s3_small = Arc::new(SimulatedS3Reader::from_bytes(
            pack_bytes,
            SimulatedS3Config {
                cache_capacity_blocks: 0,
                ..SimulatedS3Config::default()
            },
        ));
        let cold = ChunkPackReader::open_with_tail_hint(s3_small, Some(256)).unwrap();
        assert_eq!(cold.read_trigram_section().unwrap().len(), blooms.len());
        assert_eq!(cold.read_edge_table_flat().unwrap().len(), edges.len());
    }

    #[cfg(not(miri))]
    #[test]
    fn pass14c_cold_zstd_dict_and_frames_coalesced_into_single_wave() {
        let mut pb = ChunkPackBuilder::new_with_dict(
            [0xC4u8; 16],
            PackWriteOptions {
                codec: Codec::Zstd,
                zstd_level: 1,
                target_frame_raw_bytes: 128 * 1024,
                enable_trigram_skip: true,
                compress_toc_lz4: true,
                write_threads: 1,
                emit_hash_table: false,
            },
        );
        let mut expected = Vec::new();
        let mut cids = Vec::new();
        for i in 0..1_200u32 {
            let chunk = format!(
                "{{\"role\":\"assistant\",\"model\":\"claude-3-7-sonnet\",\"span\":\"{i:06}\",\"payload\":\"alpha beta gamma delta epsilon {}\"}}",
                "structured_observability_json_segment_".repeat(52)
            )
            .into_bytes();
            let cid = pb.intern_chunk(&chunk).unwrap();
            cids.push(cid);
            expected.push(chunk);
        }
        let mut pack_bytes = Vec::new();
        pb.finish(&mut pack_bytes).unwrap();

        let probe_reader =
            ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes.clone()))).unwrap();
        assert_ne!(probe_reader.postscript().flags & PACK_FLAG_HAS_DICT, 0);
        assert!(probe_reader.postscript().dict_len > 0);
        assert!(
            probe_reader
                .toc()
                .frames
                .iter()
                .any(|f| (f.flags & PACK_FRAME_FLAG_USES_DICT) != 0),
            "expected at least one Zstd dict-compressed frame"
        );

        let dict_cids: Vec<u32> = probe_reader
            .toc()
            .frames
            .iter()
            .filter(|f| (f.flags & PACK_FRAME_FLAG_USES_DICT) != 0)
            .take(3)
            .map(|f| f.first_chunk_id)
            .collect();
        assert!(!dict_cids.is_empty());

        let open_cold = || {
            let s3 = Arc::new(SimulatedS3Reader::from_bytes(
                pack_bytes.clone(),
                SimulatedS3Config {
                    cache_capacity_blocks: 0,
                    ..SimulatedS3Config::default()
                },
            ));
            // Small tail hint forces Zstd dict to sit OUTSIDE the inline tail window.
            let reader = ChunkPackReader::open_with_tail_hint(s3.clone(), Some(256)).unwrap();
            assert!(
                reader.inline_dict_framed.is_none(),
                "Zstd dict must NOT be inline under 256 B tail hint"
            );
            assert!(reader.dict_cache.get().is_none());
            (s3, reader)
        };

        // 1. scan_chunks_in_frames: 1 wave for cold dict + data frames.
        {
            let (s3, reader) = open_cold();
            let waves_before = s3.counters().waves;
            let mut got = HashMap::new();
            reader
                .scan_chunks_in_frames(&dict_cids, |cid, slice| {
                    got.insert(cid, slice.to_vec());
                })
                .unwrap();
            assert_eq!(
                s3.counters().waves - waves_before,
                1,
                "cold scan_chunks_in_frames must fetch Zstd dict + frames in 1 wave"
            );
            assert!(reader.dict_cache.get().is_some());
            for &cid in &dict_cids {
                assert_eq!(got.get(&cid).unwrap(), &expected[cid as usize]);
            }
        }

        // 2. scan_chunks_in_frames_parallel: 1 wave for cold dict + data frames.
        {
            let (s3, reader) = open_cold();
            let waves_before = s3.counters().waves;
            let items = reader
                .scan_chunks_in_frames_parallel(
                    &dict_cids,
                    2,
                    || Ok(()),
                    |_, cid, slice| Some((cid, slice.to_vec())),
                )
                .unwrap();
            assert_eq!(
                s3.counters().waves - waves_before,
                1,
                "cold scan_chunks_in_frames_parallel must fetch Zstd dict + frames in 1 wave"
            );
            assert!(reader.dict_cache.get().is_some());
            let got: HashMap<u32, Vec<u8>> = items.into_iter().collect();
            for &cid in &dict_cids {
                assert_eq!(got.get(&cid).unwrap(), &expected[cid as usize]);
            }
        }

        // 3. scan_frames_parallel: 1 wave for cold dict + data frames.
        {
            let (s3, reader) = open_cold();
            let waves_before = s3.counters().waves;
            let per_frame = reader
                .scan_frames_parallel(
                    &dict_cids,
                    2,
                    || Ok(()),
                    |_, _fid, decoded, frame_cids| {
                        let mut out = Vec::with_capacity(frame_cids.len());
                        for &cid in frame_cids {
                            let loc = &reader.toc().chunks[cid as usize];
                            let s = loc.offset_in_frame as usize;
                            let e = s + loc.raw_len as usize;
                            out.push((cid, decoded[s..e].to_vec()));
                        }
                        Ok(Some(out))
                    },
                )
                .unwrap();
            assert_eq!(
                s3.counters().waves - waves_before,
                1,
                "cold scan_frames_parallel must fetch Zstd dict + frames in 1 wave"
            );
            assert!(reader.dict_cache.get().is_some());
            let got: HashMap<u32, Vec<u8>> = per_frame.into_iter().flatten().collect();
            for &cid in &dict_cids {
                assert_eq!(got.get(&cid).unwrap(), &expected[cid as usize]);
            }
        }

        // 4. read_chunks: 1 wave for cold dict + data frames.
        {
            let (s3, reader) = open_cold();
            let waves_before = s3.counters().waves;
            let got = reader.read_chunks(&dict_cids).unwrap();
            assert_eq!(
                s3.counters().waves - waves_before,
                1,
                "cold read_chunks must fetch Zstd dict + frames in 1 wave"
            );
            assert!(reader.dict_cache.get().is_some());
            for &cid in &dict_cids {
                assert_eq!(got.get(&cid).unwrap(), &expected[cid as usize]);
            }
        }
    }

    #[cfg(not(miri))]
    #[test]
    fn pass19_remote_frame_coalescing_gap_cap_tail_boundary_dict_short_read_and_local_passthrough()
    {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct RecordingPackReader {
            data: Bytes,
            remote: bool,
            short_next: AtomicBool,
            calls: Mutex<Vec<Vec<Range<u64>>>>,
        }
        impl RecordingPackReader {
            fn new(data: Vec<u8>, remote: bool) -> Self {
                Self {
                    data: Bytes::from(data),
                    remote,
                    short_next: AtomicBool::new(false),
                    calls: Mutex::new(Vec::new()),
                }
            }
            fn take_calls(&self) -> Vec<Vec<Range<u64>>> {
                std::mem::take(&mut *self.calls.lock().unwrap())
            }
        }
        impl RangeReader for RecordingPackReader {
            fn file_len(&self) -> u64 {
                self.data.len() as u64
            }
            fn is_remote(&self) -> bool {
                self.remote
            }
            fn read_range(&self, range: Range<u64>) -> Result<Bytes, StatamError> {
                self.calls.lock().unwrap().push(vec![range.clone()]);
                Ok(self.data.slice(range.start as usize..range.end as usize))
            }
            fn read_ranges(&self, ranges: &[Range<u64>]) -> Result<Vec<Bytes>, StatamError> {
                self.calls.lock().unwrap().push(ranges.to_vec());
                let short = self.short_next.swap(false, Ordering::SeqCst);
                Ok(ranges
                    .iter()
                    .enumerate()
                    .map(|(i, r)| {
                        let end = if short && i == 0 && r.end > r.start {
                            r.end - 1
                        } else {
                            r.end
                        };
                        self.data.slice(r.start as usize..end as usize)
                    })
                    .collect())
            }
        }

        // Build a ~5.2 MiB uncompressed `.stpk` so we can exercise exact 192 KiB gap, 4 MiB cap,
        // > 4 MiB single frame, tail-boundary guard, short-read error, and non-remote passthrough.
        let mut pb = ChunkPackBuilder::new(
            [0x19u8; 16],
            PackWriteOptions {
                codec: Codec::None,
                zstd_level: 1,
                target_frame_raw_bytes: 256 * 1024,
                enable_trigram_skip: false,
                compress_toc_lz4: false,
                write_threads: 1,
                emit_hash_table: false,
            },
        );
        for i in 0..21u32 {
            let mut chunk = vec![((i * 37) & 0xFF) as u8; 256 * 1024];
            chunk[0..4].copy_from_slice(&i.to_le_bytes());
            pb.intern_chunk(&chunk).unwrap();
        }
        let mut pack_bytes = Vec::new();
        pb.finish(&mut pack_bytes).unwrap();
        assert!(pack_bytes.len() > 5 * 1024 * 1024);

        // Open remote with 256 KiB tail hint so `retained_tail_start = Some(file_len - 256 KiB)`.
        let rec_remote = Arc::new(RecordingPackReader::new(pack_bytes.clone(), true));
        let remote_pack =
            ChunkPackReader::open_with_tail_hint(rec_remote.clone(), Some(256 * 1024)).unwrap();
        let tail_start = remote_pack
            .retained_tail_start
            .expect("remote pack must retain tail_start");
        rec_remote.take_calls();

        // 1. Gap <= 192 KiB merges; gap > 192 KiB splits.
        let gap = REMOTE_PACK_COALESCE_MAX_GAP;
        let r_a = 1_000..5_000u64;
        let r_b = (5_000 + gap)..(5_000 + gap + 4_000); // exact 192 KiB gap -> merges with r_a
        let r_c = (r_b.end + gap + 1)..(r_b.end + gap + 1 + 4_000); // 192 KiB + 1 -> splits
        let out = remote_pack
            .read_ranges_remote_coalesced(&[r_c.clone(), r_a.clone(), r_b.clone()])
            .unwrap();
        assert_eq!(
            out[0].as_ref(),
            &pack_bytes[r_c.start as usize..r_c.end as usize]
        );
        assert_eq!(
            out[1].as_ref(),
            &pack_bytes[r_a.start as usize..r_a.end as usize]
        );
        assert_eq!(
            out[2].as_ref(),
            &pack_bytes[r_b.start as usize..r_b.end as usize]
        );
        assert_eq!(
            rec_remote.take_calls(),
            vec![vec![r_a.start..r_b.end, r_c]],
            "gap <= 192 KiB must merge and gap > 192 KiB must split"
        );

        // 2. 4 MiB max span cap splits consecutive small ranges once merged span exceeds 4 MiB.
        let mut cap_ranges = Vec::new();
        let mut off = 1_000u64;
        for _ in 0..30 {
            cap_ranges.push(off..(off + 32 * 1024));
            off += 32 * 1024 + 128 * 1024; // 160 KiB stride (128 KiB gap <= 192 KiB), 30 * 160 KiB > 4 MiB
        }
        let cap_out = remote_pack
            .read_ranges_remote_coalesced(&cap_ranges)
            .unwrap();
        for (r, got) in cap_ranges.iter().zip(cap_out.iter()) {
            assert_eq!(got.as_ref(), &pack_bytes[r.start as usize..r.end as usize]);
        }
        let cap_calls = rec_remote.take_calls();
        assert_eq!(cap_calls.len(), 1);
        assert!(
            cap_calls[0].len() >= 2,
            "expected > 4 MiB total span to split across multiple coalesced spans"
        );
        for span in &cap_calls[0] {
            assert!(
                span.end - span.start <= REMOTE_PACK_COALESCE_MAX_SPAN,
                "coalesced span {} exceeded 4 MiB cap",
                span.end - span.start
            );
        }

        // 3. Single frame > 4 MiB passes through cleanly as its own span (not dropped/truncated).
        let huge = 64..(64 + REMOTE_PACK_COALESCE_MAX_SPAN + 64 * 1024);
        let tiny = (huge.end + 1024)..(huge.end + 2048);
        let huge_out = remote_pack
            .read_ranges_remote_coalesced(&[huge.clone(), tiny.clone()])
            .unwrap();
        assert_eq!(huge_out[0].len() as u64, huge.end - huge.start);
        assert_eq!(
            huge_out[1].as_ref(),
            &pack_bytes[tiny.start as usize..tiny.end as usize]
        );
        assert_eq!(
            rec_remote.take_calls(),
            vec![vec![huge, tiny]],
            "> 4 MiB single frame must pass through as its own span"
        );

        // 4. Tail-boundary guard: pre-tail range within 192 KiB of an in-tail range MUST NOT merge
        //    across `retained_tail_start`, so the in-tail range stays a 0-I/O TailCachedReader hit!
        let pre_tail = (tail_start - 16 * 1024)..(tail_start - 8 * 1024);
        let in_tail = (tail_start + 1024)..(tail_start + 5 * 1024);
        assert!(in_tail.start - pre_tail.end <= REMOTE_PACK_COALESCE_MAX_GAP);
        let tail_out = remote_pack
            .read_ranges_remote_coalesced(&[pre_tail.clone(), in_tail.clone()])
            .unwrap();
        assert_eq!(
            tail_out[0].as_ref(),
            &pack_bytes[pre_tail.start as usize..pre_tail.end as usize]
        );
        assert_eq!(
            tail_out[1].as_ref(),
            &pack_bytes[in_tail.start as usize..in_tail.end as usize]
        );
        assert_eq!(
            rec_remote.take_calls(),
            vec![vec![pre_tail]],
            "tail-boundary guard must NOT merge pre-tail + in-tail ranges; in-tail must stay 0-GET hit"
        );

        // 5. Short-read on coalesced span returns Err(StatamError::Corrupt).
        rec_remote.short_next.store(true, Ordering::SeqCst);
        assert!(matches!(
            remote_pack.read_ranges_remote_coalesced(&[r_a.clone(), r_b.clone()]),
            Err(StatamError::Corrupt {
                reason: "short read on coalesced pack frame span"
            })
        ));
        rec_remote.take_calls();

        // 6. Non-remote (!is_remote()) passes ranges straight through unmodified (0 coalescing).
        let rec_local = Arc::new(RecordingPackReader::new(pack_bytes.clone(), false));
        let local_pack =
            ChunkPackReader::open_with_tail_hint(rec_local.clone(), Some(256 * 1024)).unwrap();
        assert!(local_pack.retained_tail_start.is_none());
        rec_local.take_calls();
        let (local_out, _) = local_pack
            .fetch_frames_and_dict(&[r_a.clone(), r_b.clone()], 2, None, false)
            .unwrap();
        assert_eq!(local_out.len(), 2);
        assert_eq!(
            rec_local.take_calls(),
            vec![vec![r_a, r_b]],
            "non-remote pack reader must pass ranges through unmodified"
        );

        // 7. Trailing `dict_range` scattering on a real Zstd-dict pack under remote coalescing.
        let mut dict_pb = ChunkPackBuilder::new_with_dict(
            [0xD9u8; 16],
            PackWriteOptions {
                codec: Codec::Zstd,
                zstd_level: 1,
                target_frame_raw_bytes: 64 * 1024,
                enable_trigram_skip: true,
                compress_toc_lz4: true,
                write_threads: 1,
                emit_hash_table: false,
            },
        );
        let mut dict_expected = Vec::new();
        for i in 0..1_200u32 {
            let chunk = format!(
                "{{\"role\":\"assistant\",\"model\":\"gpt-4o\",\"idx\":\"{i:05}\",\"body\":\"{}\"}}",
                "remote_coalesced_dict_test_payload_segment_".repeat(52)
            )
            .into_bytes();
            let cid = dict_pb.intern_chunk(&chunk).unwrap();
            assert_eq!(cid, i);
            dict_expected.push(chunk);
        }
        let mut dict_pack_bytes = Vec::new();
        dict_pb.finish(&mut dict_pack_bytes).unwrap();

        let rec_dict = Arc::new(RecordingPackReader::new(dict_pack_bytes, true));
        // Small tail hint forces Zstd dict to sit outside inline tail so `has_trailing_dict == true`.
        let dict_pack = ChunkPackReader::open_with_tail_hint(rec_dict.clone(), Some(256)).unwrap();
        assert!(dict_pack.inline_dict_framed.is_none());
        let dict_cids: Vec<u32> = dict_pack
            .toc()
            .frames
            .iter()
            .filter(|f| (f.flags & PACK_FRAME_FLAG_USES_DICT) != 0)
            .take(3)
            .map(|f| f.first_chunk_id)
            .collect();
        assert!(dict_cids.len() >= 2);
        rec_dict.take_calls();
        let got_dict_chunks = dict_pack.read_chunks(&dict_cids).unwrap();
        assert!(dict_pack.dict_cache.get().is_some());
        for &cid in &dict_cids {
            assert_eq!(
                got_dict_chunks.get(&cid).unwrap().as_slice(),
                dict_expected[cid as usize].as_slice()
            );
        }
        let dict_calls = rec_dict.take_calls();
        assert_eq!(
            dict_calls.len(),
            1,
            "cold remote dict + frames must execute in 1 coalesced wave"
        );
        assert!(
            dict_calls[0].len() < dict_cids.len() + 1,
            "nearby remote frames + trailing dict should coalesce into fewer spans: got {} spans for {} ranges",
            dict_calls[0].len(),
            dict_cids.len() + 1
        );
    }

    #[test]
    fn pass20_adaptive_remote_tail_hint_4mib_threshold() {
        struct TailRecorder {
            data: Bytes,
            remote: bool,
            calls: Mutex<Vec<Range<u64>>>,
        }
        impl TailRecorder {
            fn new(data: Vec<u8>, remote: bool) -> Self {
                Self {
                    data: Bytes::from(data),
                    remote,
                    calls: Mutex::new(Vec::new()),
                }
            }
            fn take_calls(&self) -> Vec<Range<u64>> {
                std::mem::take(&mut *self.calls.lock().unwrap())
            }
        }
        impl RangeReader for TailRecorder {
            fn file_len(&self) -> u64 {
                self.data.len() as u64
            }
            fn is_remote(&self) -> bool {
                self.remote
            }
            fn read_range(&self, range: Range<u64>) -> Result<Bytes, StatamError> {
                self.calls.lock().unwrap().push(range.clone());
                Ok(self.data.slice(range.start as usize..range.end as usize))
            }
        }

        let build_uncompressed_pack = |chunks: u32, chunk_len: usize| -> Vec<u8> {
            let mut pb = ChunkPackBuilder::new(
                [0x20u8; 16],
                PackWriteOptions {
                    codec: Codec::None,
                    zstd_level: 1,
                    target_frame_raw_bytes: 256 * 1024,
                    enable_trigram_skip: false,
                    compress_toc_lz4: false,
                    write_threads: 1,
                    emit_hash_table: false,
                },
            );
            for i in 0..chunks {
                let mut chunk = vec![((i * 29) & 0xFF) as u8; chunk_len];
                chunk[0..4].copy_from_slice(&i.to_le_bytes());
                pb.intern_chunk(&chunk).unwrap();
            }
            let mut out = Vec::new();
            pb.finish(&mut out).unwrap();
            out
        };

        // Small pack (~1.0 MiB, strictly > 384 KiB and <= 4 MiB).
        let small_pack = build_uncompressed_pack(4, 256 * 1024);
        let small_len = small_pack.len() as u64;
        assert!(small_len > u64::from(PACK_REMOTE_SMALL_TAIL_HINT));
        assert!(small_len <= PACK_REMOTE_ADAPTIVE_TAIL_MAX_FILE_LEN);

        // 1. Remote + tail_hint=None + file_len <= 4 MiB -> uses adaptive 384 KiB tail hint!
        let rec_rem_small = Arc::new(TailRecorder::new(small_pack.clone(), true));
        let p_rem_small = ChunkPackReader::open(rec_rem_small.clone()).unwrap();
        let expected_384 = (small_len - u64::from(PACK_REMOTE_SMALL_TAIL_HINT))..small_len;
        assert_eq!(rec_rem_small.take_calls(), vec![expected_384.clone()]);
        assert_eq!(p_rem_small.retained_tail_start, Some(expected_384.start));

        // 2. Non-remote + tail_hint=None + file_len <= 4 MiB -> keeps 256 KiB default hint!
        let rec_loc_small = Arc::new(TailRecorder::new(small_pack.clone(), false));
        let p_loc_small = ChunkPackReader::open(rec_loc_small.clone()).unwrap();
        let expected_256_small = (small_len - u64::from(PACK_DEFAULT_TAIL_HINT))..small_len;
        assert_eq!(rec_loc_small.take_calls(), vec![expected_256_small]);
        assert_eq!(p_loc_small.retained_tail_start, None);

        // 3. Remote + explicit Some(128 KiB) + file_len <= 4 MiB -> honors explicit 128 KiB hint!
        let rec_rem_explicit = Arc::new(TailRecorder::new(small_pack, true));
        let p_rem_explicit =
            ChunkPackReader::open_with_tail_hint(rec_rem_explicit.clone(), Some(128 * 1024))
                .unwrap();
        let expected_128 = (small_len - 128 * 1024)..small_len;
        assert_eq!(rec_rem_explicit.take_calls(), vec![expected_128.clone()]);
        assert_eq!(p_rem_explicit.retained_tail_start, Some(expected_128.start));

        // 4. Remote + tail_hint=None + file_len > 4 MiB -> keeps 256 KiB default hint!
        let large_pack = build_uncompressed_pack(17, 256 * 1024);
        let large_len = large_pack.len() as u64;
        assert!(large_len > PACK_REMOTE_ADAPTIVE_TAIL_MAX_FILE_LEN);
        let rec_rem_large = Arc::new(TailRecorder::new(large_pack, true));
        let p_rem_large = ChunkPackReader::open(rec_rem_large.clone()).unwrap();
        let expected_256_large = (large_len - u64::from(PACK_DEFAULT_TAIL_HINT))..large_len;
        assert_eq!(rec_rem_large.take_calls(), vec![expected_256_large.clone()]);
        assert_eq!(
            p_rem_large.retained_tail_start,
            Some(expected_256_large.start)
        );
    }

    #[test]
    fn pass22_reusable_dict_decompressor_and_zero_copy_chunks() {
        // 1. PackFrameDecompressor: switch between TWO distinct PackDecoderDicts + plain Zstd + None,
        //    and verify rejection of corrupt CRC32C / missing dict / wrong dict.
        #[cfg(not(miri))]
        {
            let mut samples_a = Vec::new();
            let mut samples_b = Vec::new();
            for i in 0..256usize {
                samples_a.push(
                    format!(
                        "{{\"service\":\"alpha-gateway\",\"trace\":\"{i:06}\",\"payload\":\"alpha_dict_corpus_entry_{i:04}\"}}"
                    )
                    .into_bytes(),
                );
                samples_b.push(
                    format!(
                        "{{\"service\":\"beta-worker\",\"span\":\"{i:06}\",\"payload\":\"beta_dict_corpus_entry_{i:04}\"}}"
                    )
                    .into_bytes(),
                );
            }
            let dict_bytes_a = zstd::dict::from_samples(&samples_a, 8 * 1024).unwrap();
            let dict_bytes_b = zstd::dict::from_samples(&samples_b, 8 * 1024).unwrap();
            let mut comp_a = PackDictCompressor::new(3, &dict_bytes_a).unwrap();
            let mut comp_b = PackDictCompressor::new(3, &dict_bytes_b).unwrap();
            let dec_a = PackDecoderDict::new(&dict_bytes_a).unwrap();
            let dec_b = PackDecoderDict::new(&dict_bytes_b).unwrap();

            let raw_a1 = samples_a[0..16].concat();
            let raw_a2 = samples_a[16..32].concat();
            let raw_b1 = samples_b[0..16].concat();
            let raw_b2 = samples_b[16..32].concat();
            let raw_plain = b"plain-zstd-frame-content-repeated-".repeat(64);
            let raw_none = b"uncompressed-frame-bytes-0123456789".to_vec();

            let mut f_a1 = Vec::new();
            let info_a1 = write_framed_pack_frame_with_compressor_min_savings(
                &raw_a1,
                Codec::Zstd,
                0,
                3,
                0,
                Some(&mut comp_a),
                &mut f_a1,
            )
            .unwrap();
            assert_eq!(info_a1.codec, Codec::Zstd);
            assert_ne!(f_a1[3] & PACK_FRAME_FLAG_USES_DICT, 0);

            let mut f_a2 = Vec::new();
            write_framed_pack_frame_with_compressor_min_savings(
                &raw_a2,
                Codec::Zstd,
                0,
                3,
                0,
                Some(&mut comp_a),
                &mut f_a2,
            )
            .unwrap();
            assert_ne!(f_a2[3] & PACK_FRAME_FLAG_USES_DICT, 0);

            let mut f_b1 = Vec::new();
            write_framed_pack_frame_with_compressor_min_savings(
                &raw_b1,
                Codec::Zstd,
                0,
                3,
                0,
                Some(&mut comp_b),
                &mut f_b1,
            )
            .unwrap();
            assert_ne!(f_b1[3] & PACK_FRAME_FLAG_USES_DICT, 0);

            let mut f_b2 = Vec::new();
            write_framed_pack_frame_with_compressor_min_savings(
                &raw_b2,
                Codec::Zstd,
                0,
                3,
                0,
                Some(&mut comp_b),
                &mut f_b2,
            )
            .unwrap();
            assert_ne!(f_b2[3] & PACK_FRAME_FLAG_USES_DICT, 0);

            let mut f_plain = Vec::new();
            write_framed_pack_frame(&raw_plain, Codec::Zstd, 0, 3, &mut f_plain).unwrap();
            assert_eq!(f_plain[3] & PACK_FRAME_FLAG_USES_DICT, 0);

            let mut f_none = Vec::new();
            write_framed_pack_frame(&raw_none, Codec::None, 0, 0, &mut f_none).unwrap();

            // Interleave dict A -> dict B -> plain -> dict A -> none -> dict B on ONE decompressor.
            let mut decomp = PackFrameDecompressor::new();
            assert_eq!(
                read_framed_pack_frame_with_decompressor(
                    &f_a1,
                    MAX_FRAME_RAW_BYTES,
                    Some(&dec_a),
                    &mut decomp,
                )
                .unwrap()
                .2,
                raw_a1
            );
            assert_eq!(
                read_framed_pack_frame_with_decompressor(
                    &f_b1,
                    MAX_FRAME_RAW_BYTES,
                    Some(&dec_b),
                    &mut decomp,
                )
                .unwrap()
                .2,
                raw_b1
            );
            assert_eq!(
                read_framed_pack_frame_with_decompressor(
                    &f_plain,
                    MAX_FRAME_RAW_BYTES,
                    None,
                    &mut decomp,
                )
                .unwrap()
                .2,
                raw_plain
            );
            assert_eq!(
                read_framed_pack_frame_with_decompressor(
                    &f_a2,
                    MAX_FRAME_RAW_BYTES,
                    Some(&dec_a),
                    &mut decomp,
                )
                .unwrap()
                .2,
                raw_a2
            );
            assert_eq!(
                read_framed_pack_frame_with_decompressor(
                    &f_none,
                    MAX_FRAME_RAW_BYTES,
                    Some(&dec_a),
                    &mut decomp,
                )
                .unwrap()
                .2,
                raw_none
            );
            assert_eq!(
                read_framed_pack_frame_with_decompressor(
                    &f_b2,
                    MAX_FRAME_RAW_BYTES,
                    Some(&dec_b),
                    &mut decomp,
                )
                .unwrap()
                .2,
                raw_b2
            );

            // Corrupt CRC32C rejected.
            let mut bad_crc = f_a1.clone();
            bad_crc[PACK_FRAME_HEADER_LEN] ^= 0xFF;
            assert!(matches!(
                read_framed_pack_frame_with_decompressor(
                    &bad_crc,
                    MAX_FRAME_RAW_BYTES,
                    Some(&dec_a),
                    &mut decomp,
                ),
                Err(StatamError::ChecksumMismatch { .. })
            ));
            // Missing dict rejected.
            assert!(matches!(
                read_framed_pack_frame_with_decompressor(
                    &f_a1,
                    MAX_FRAME_RAW_BYTES,
                    None,
                    &mut decomp,
                ),
                Err(StatamError::Corrupt { .. })
            ));
            // Wrong dict rejected.
            assert!(matches!(
                read_framed_pack_frame_with_decompressor(
                    &f_a1,
                    MAX_FRAME_RAW_BYTES,
                    Some(&dec_b),
                    &mut decomp,
                ),
                Err(StatamError::Corrupt { .. })
            ));
        }

        // 2. Dense (zero-copy Bytes::slice) vs sparse (< 50% copy-out) parity across 1T and 8T.
        let mut pb = ChunkPackBuilder::new(
            [0x22u8; 16],
            PackWriteOptions {
                codec: test_codec(),
                zstd_level: 1,
                target_frame_raw_bytes: 64 * 1024,
                enable_trigram_skip: false,
                compress_toc_lz4: true,
                write_threads: 1,
                emit_hash_table: false,
            },
        );
        let mut expected = Vec::new();
        // Create 8 frames of ~64 KiB each (16 chunks of 4 KiB per frame -> 128 chunks).
        for i in 0..128u32 {
            let pattern = format!("chunk-{i:04}-payload-");
            let mut data = Vec::with_capacity(4096);
            while data.len() < 4096 {
                data.extend_from_slice(pattern.as_bytes());
            }
            data.truncate(4096);
            let cid = pb.intern_chunk(&data).unwrap();
            assert_eq!(cid, i);
            expected.push(data);
        }
        let mut pack_bytes = Vec::new();
        pb.finish(&mut pack_bytes).unwrap();
        let reader = ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes))).unwrap();
        assert!(reader.toc().frames.len() >= 8);

        let all_cids: Vec<u32> = (0..128u32).collect();
        let sparse_cids: Vec<u32> = (0..128u32).step_by(16).collect(); // 1 chunk (4 KiB) per 64 KiB frame (< 50% & > 16 KiB unused)

        let ref_all = reader.read_chunks(&all_cids).unwrap();
        let ref_sparse = reader.read_chunks(&sparse_cids).unwrap();

        for threads in [1usize, 8] {
            let dense_got: HashMap<u32, Bytes> = reader
                .scan_chunks_as_bytes_parallel(&all_cids, threads)
                .unwrap()
                .into_iter()
                .collect();
            assert_eq!(dense_got.len(), all_cids.len());
            for &cid in &all_cids {
                assert_eq!(dense_got[&cid].as_ref(), ref_all[&cid].as_slice());
            }

            let sparse_got: HashMap<u32, Bytes> = reader
                .scan_chunks_as_bytes_parallel(&sparse_cids, threads)
                .unwrap()
                .into_iter()
                .collect();
            assert_eq!(sparse_got.len(), sparse_cids.len());
            for &cid in &sparse_cids {
                assert_eq!(sparse_got[&cid].as_ref(), ref_sparse[&cid].as_slice());
            }
        }

        // 3. `scan_frames_parallel` on `Codec::None` frames >= 2 MiB still runs in parallel (> 1 worker).
        let mut pb_none = ChunkPackBuilder::new(
            [0x23u8; 16],
            PackWriteOptions {
                codec: Codec::None,
                zstd_level: 0,
                target_frame_raw_bytes: 512 * 1024,
                enable_trigram_skip: false,
                compress_toc_lz4: false,
                write_threads: 1,
                emit_hash_table: false,
            },
        );
        let mut none_cids = Vec::new();
        for i in 0..6u32 {
            let mut chunk = vec![(i as u8).wrapping_add(1); 512 * 1024];
            chunk[0..4].copy_from_slice(&i.to_le_bytes());
            none_cids.push(pb_none.intern_chunk(&chunk).unwrap());
        }
        let mut none_bytes = Vec::new();
        pb_none.finish(&mut none_bytes).unwrap();
        let none_reader = ChunkPackReader::open(Arc::new(SliceReader::new(none_bytes))).unwrap();
        assert_eq!(none_reader.toc().frames.len(), 6);
        assert!(
            none_reader
                .toc()
                .frames
                .iter()
                .all(|f| f.codec == Codec::None)
        );

        let barrier = std::sync::Barrier::new(2);
        let first_two = std::sync::atomic::AtomicUsize::new(0);
        let tids = none_reader
            .scan_frames_parallel(
                &none_cids,
                4,
                || Ok(()),
                |(), fid, buf, _cids| {
                    assert_eq!(buf.len(), 512 * 1024);
                    if first_two.fetch_add(1, std::sync::atomic::Ordering::AcqRel) < 2 {
                        barrier.wait();
                    }
                    Ok(Some((fid, std::thread::current().id())))
                },
            )
            .unwrap();
        assert_eq!(tids.len(), 6);
        let unique_tids: std::collections::HashSet<_> = tids.iter().map(|&(_, tid)| tid).collect();
        assert!(
            unique_tids.len() >= 2,
            "expected >= 2 worker threads on >= 2 MiB Codec::None frames, got {}",
            unique_tids.len()
        );
    }
}
