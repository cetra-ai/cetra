//! Statam v0 native columnar storage format, `.stpk` chunk packs, and range I/O.

pub mod codec;
pub mod error;
pub mod format;
pub mod gdpr;
pub mod io;
pub(crate) mod parallel;
pub mod payload;
pub mod reader;
pub mod simd;
pub mod writer;

/// Fuzz entry points. Not part of the public API.
#[cfg(any(test, fuzzing))]
#[doc(hidden)]
pub mod fuzz_entry;

pub use codec::{
    DICT_PASSTHROUGH_MAX_BYTES, DictBuilder, EncodedMiniBlock, FsstBlockView, FsstTable,
    ShreddedMapBlocks, ShreddedSubCol, WriteProfile, ZoneMapSummary, ZonePred, ZoneValue,
    block_zone_map, decode_mini_block, decode_shredded_residual_block, decode_shredded_subcol,
    decode_single_map_key, encode_mini_block, encode_shredded_residual_block,
    encode_shredded_subcol, select_shredded_keys, shred_map, unshred_map, zone_map_may_match,
};
pub use error::StatamError;
pub use format::{
    BLOCK_ENTRY_LEN, BLOCK_FRAME_HEADER_LEN, BLOCK_MAGIC, BLOOM_MAGIC, CHECKSUM_ALG_CRC32C,
    CHUNK_LOC_LEN, COLUMN_DIR_ENTRY_LEN, Codec, ColumnDirEntry, DEFAULT_TAIL_HINT, DICT_MAGIC,
    Encoding, FILE_HEADER_LEN, FILE_MAGIC, FORMAT_VERSION, FRAME_ENTRY_LEN, FileHeader,
    FileSchemaMeta, FrameEntry, LAZY_INDEX_ENTRY_LEN, LazyIndexEntry, LazyIndexKind, LocalityGroup,
    MAX_BATCH_DECODED_BYTES, MAX_BLOCK_RAW_BYTES, MAX_BLOOM_BYTES, MAX_CHUNKS_PER_ROW, MAX_COLUMNS,
    MAX_DECOMPRESS_RATIO, MAX_DECOMPRESS_SLACK, MAX_DICT_BYTES, MAX_FRAME_RAW_BYTES,
    MAX_HOT_TAIL_BYTES, MAX_NESTING_DEPTH, MAX_REF_CHAIN_DEPTH, MAX_ROW_GROUPS,
    MAX_ROW_PAYLOAD_BYTES, MAX_ROWS_PER_FILE, MAX_SHREDDED_KEYS, MAX_TOTAL_BLOCKS, MINI_BLOCK_SIZE,
    PACK_FRAME_HEADER_LEN, PACK_FRAME_MAGIC, PACK_HEADER_LEN, PACK_MAGIC, PACK_POSTSCRIPT_LEN,
    PACK_POSTSCRIPT_MAGIC, POSTSCRIPT_LEN, POSTSCRIPT_MAGIC, PackHeader, PackPostscript,
    Postscript, ROW_GROUP_ENTRY_LEN, ROW_GROUP_SIZE, RowGroupEntry, ShreddedMapDir,
    ShreddedMapKeyEntry, TableKind,
};
pub use gdpr::{
    GdprFileInput, GdprRewriteReport, GdprRewriteResult, GdprRewriteStats, RewrittenPackFile,
    RewrittenStatamFile, gdpr_rewrite_day,
};
pub use io::{
    FileReader, IoCounters, RangeReader, S3_CACHE_BLOCK_BYTES, SimulatedS3Config,
    SimulatedS3Reader, SliceReader,
};
pub use payload::{
    PayloadRefRow,
    chunker::{
        CDC_AVG, CDC_MIN, DedupBypassDetector, INLINE_PAYLOAD_THRESHOLD, MAX_CHUNK, MIN_CHUNK,
        chunk_payload,
    },
    decode_payload_refs, encode_payload_refs,
    pack::{ChunkPackBuilder, ChunkPackReader, PackToc, PackWriteOptions, PackWriteStats},
    search::{search_pack_chunks, search_pack_rows},
};
pub use reader::{
    CoalesceMode, IdLookup, InMemoryPackResolver, PackResolver, Predicate, ScanRequest,
    StatamFileMeta, StatamReader, SubstringQuery, TimeRange,
};
pub use simd::{FilterOp, Sbbf, TrigramBloom, statam_bloom_hash};
pub use writer::{L0_LOCAL_PACK_IDX, StatamWriter, WriteOptions, WriteStats};
