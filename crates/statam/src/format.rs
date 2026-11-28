//! Statam v0 binary wire format constants, structs, schema codecs, and framing.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use cetra_column::{DataType, Field, Schema};

use crate::StatamError;

/// Magic bytes at offset 0 of a `.statam` data file.
pub const FILE_MAGIC: [u8; 4] = *b"STM0";
/// Magic bytes at the very end (`[108..112]`) of a `.statam` Postscript.
pub const POSTSCRIPT_MAGIC: [u8; 4] = *b"0MTS";
/// Magic bytes at offset 0 of a `.stpk` chunk pack.
pub const PACK_MAGIC: [u8; 4] = *b"SPK0";
/// Magic bytes at the very end (`[108..112]`) of a `.stpk` PackPostscript.
pub const PACK_POSTSCRIPT_MAGIC: [u8; 4] = *b"0KPS";
/// Magic bytes at offset 0 of a framed `.statam` data block.
pub const BLOCK_MAGIC: [u8; 2] = *b"SB";
/// Magic bytes at offset 0 of a framed `.stpk` pack frame.
pub const PACK_FRAME_MAGIC: [u8; 2] = *b"PF";
/// Magic bytes at offset 0 of a Split-Block Bloom Filter section (`BF01`, single filter).
pub const BLOOM_MAGIC: [u8; 4] = *b"BF01";
/// Magic bytes at offset 0 of a per-Row-Group Split-Block Bloom Filter section (`BF02`).
pub const BLOOM_RG_MAGIC: [u8; 4] = *b"BF02";
/// Magic bytes at offset 0 of a file-level Dictionary section.
pub const DICT_MAGIC: [u8; 4] = *b"DT01";

/// Current `.statam` and `.stpk` wire format version.
pub const FORMAT_VERSION: u16 = 1;
/// Checksum algorithm identifier: CRC-32C.
pub const CHECKSUM_ALG_CRC32C: u8 = 1;
/// Default tail fetch size (192 KiB) for 1-GET cold open covering wide-schema Hot Tails
/// (~170 KiB) while staying inside the final 1 MiB ReadCache block on typical tail remainders;
/// Hot Tail remains hard-capped at `1 MiB`.
pub const DEFAULT_TAIL_HINT: u32 = 192 * 1024;

/// Target rows per PAX Row Group.
pub const ROW_GROUP_SIZE: usize = 131_072;
/// Hard upper bound on rows per PAX Row Group (`RowGroupEntry::row_count`).
pub const MAX_ROW_GROUP_SIZE: usize = 262_144;
/// Target rows per Mini-Block (`L1Compact` and `L0Fast`).
pub const MINI_BLOCK_SIZE: usize = 8_192;
/// Hard upper bound on rows per Mini-Block (`BlockEntry::row_count`).
pub const MAX_MINI_BLOCK_SIZE: usize = u16::MAX as usize;

/// Exact serialized byte length of [`FileHeader`].
pub const FILE_HEADER_LEN: usize = 32;
/// Exact serialized byte length of [`Postscript`].
pub const POSTSCRIPT_LEN: usize = 112;
/// Exact serialized byte length of [`PackHeader`].
pub const PACK_HEADER_LEN: usize = 48;
/// Exact serialized byte length of [`PackPostscript`].
pub const PACK_POSTSCRIPT_LEN: usize = 112;
/// Exact serialized byte length of [`ColumnDirEntry`].
pub const COLUMN_DIR_ENTRY_LEN: usize = 56;
/// Exact serialized byte length of [`RowGroupEntry`].
pub const ROW_GROUP_ENTRY_LEN: usize = 48;
/// Exact serialized byte length of [`BlockEntry`].
pub const BLOCK_ENTRY_LEN: usize = 64;
/// Exact serialized byte length of [`FrameEntry`].
pub const FRAME_ENTRY_LEN: usize = 40;
/// Exact serialized byte length of [`ChunkLoc`].
pub const CHUNK_LOC_LEN: usize = 12;
/// Exact serialized byte length of [`LazyIndexEntry`].
pub const LAZY_INDEX_ENTRY_LEN: usize = 24;
/// Exact serialized byte length of a `.statam` data block header (`SB`).
pub const BLOCK_FRAME_HEADER_LEN: usize = 16;
/// Exact serialized byte length of a `.stpk` frame header (`PF`).
pub const PACK_FRAME_HEADER_LEN: usize = 16;

// --- File & Postscript flags ---

/// AEAD encryption envelope present.
pub const FLAG_ENCRYPTED: u16 = 0x0001;
/// File written with `L1Compact` profile.
pub const FLAG_L1_COMPACT: u16 = 0x0002;
/// File references one or more external `.stpk` packs.
pub const FLAG_HAS_PACK_REFS: u16 = 0x0004;
/// File contains shredded map sub-columns.
pub const FLAG_HAS_SHREDDED_MAPS: u16 = 0x0008;
/// File contains at least one narrow-column trigram index.
pub const FLAG_HAS_TRIGRAM_INDEX: u16 = 0x0010;
/// Hot Tail body is raw-block LZ4 compressed.
pub const FLAG_HOT_TAIL_LZ4: u16 = 0x0020;
/// Mask of all recognized `.statam` file flags in v1.
pub const KNOWN_FILE_FLAGS: u16 = FLAG_ENCRYPTED
    | FLAG_L1_COMPACT
    | FLAG_HAS_PACK_REFS
    | FLAG_HAS_SHREDDED_MAPS
    | FLAG_HAS_TRIGRAM_INDEX
    | FLAG_HOT_TAIL_LZ4;

// --- Pack flags ---

/// Pack AEAD encryption envelope present.
pub const PACK_FLAG_ENCRYPTED: u16 = 0x0001;
/// Pack TOC body is raw-block LZ4 compressed.
pub const PACK_FLAG_TOC_LZ4: u16 = 0x0002;
/// Pack contains a trigram skip section.
pub const PACK_FLAG_HAS_TRIGRAM: u16 = 0x0004;
/// Pack contains a trained dictionary section.
pub const PACK_FLAG_HAS_DICT: u16 = 0x0008;
/// Pack TOC body uses Zstd compression with delta-encoded `ChunkLoc` offsets.
pub const PACK_FLAG_TOC_ZSTD: u16 = 0x0010;
/// Mask of all recognized `.stpk` pack flags in v1.
pub const KNOWN_PACK_FLAGS: u16 = PACK_FLAG_ENCRYPTED
    | PACK_FLAG_TOC_LZ4
    | PACK_FLAG_HAS_TRIGRAM
    | PACK_FLAG_HAS_DICT
    | PACK_FLAG_TOC_ZSTD;

/// Pack frame flag: frame compressed using trained dictionary.
pub const PACK_FRAME_FLAG_USES_DICT: u8 = 0x01;

// --- Column directory flags ---

/// Column has a file-level dictionary.
pub const COL_FLAG_HAS_FILE_DICT: u8 = 0x01;
/// Column file-level dictionary is lexicographically sorted.
pub const COL_FLAG_DICT_SORTED: u8 = 0x02;
/// Column uses `PayloadRefs` external/local chunk encoding.
pub const COL_FLAG_IS_PAYLOAD_REFS: u8 = 0x04;
/// Physical column is a promoted shredded map key sub-column.
pub const COL_FLAG_IS_SHREDDED_SUBCOL: u8 = 0x08;
/// Column dictionary resides inside the Hot Tail.
pub const COL_FLAG_DICT_IN_HOT_TAIL: u8 = 0x10;
/// Column SBBF bloom resides inside the Hot Tail.
pub const COL_FLAG_BLOOM_IN_HOT_TAIL: u8 = 0x20;
/// Column has a narrow trigram index.
pub const COL_FLAG_HAS_TRIGRAM_INDEX: u8 = 0x40;
/// Mask of all recognized column directory flags in v1.
pub const KNOWN_COL_FLAGS: u8 = COL_FLAG_HAS_FILE_DICT
    | COL_FLAG_DICT_SORTED
    | COL_FLAG_IS_PAYLOAD_REFS
    | COL_FLAG_IS_SHREDDED_SUBCOL
    | COL_FLAG_DICT_IN_HOT_TAIL
    | COL_FLAG_BLOOM_IN_HOT_TAIL
    | COL_FLAG_HAS_TRIGRAM_INDEX;

// --- Mini-block flags ---

/// Mini-block contains at least one null value.
pub const BLOCK_FLAG_HAS_NULLS: u8 = 0x01;
/// Every row in the mini-block is null.
pub const BLOCK_FLAG_ALL_NULL: u8 = 0x02;
/// `min_inline` and `max_inline` hold valid bounds.
pub const BLOCK_FLAG_MIN_MAX_VALID: u8 = 0x04;
/// `F64` mini-block contains at least one `NaN`.
pub const BLOCK_FLAG_HAS_NAN: u8 = 0x08;
/// Constant empty `List`/`MapUtf8`/`Events`/`Links` collection.
pub const BLOCK_FLAG_CONST_EMPTY_COLLECTION: u8 = 0x10;
/// `Encoding::Fsst` mini-block uses 256-row adaptive `LZ4`/`Zstd`/`None` sub-chunks
/// (`outer codec = Codec::None`, `raw_len == stored_len`).
pub const BLOCK_FLAG_FSST_SUBCHUNKED: u8 = 0x20;
/// Mask of all recognized mini-block flags in v1.
pub const KNOWN_BLOCK_FLAGS: u8 = BLOCK_FLAG_HAS_NULLS
    | BLOCK_FLAG_ALL_NULL
    | BLOCK_FLAG_MIN_MAX_VALID
    | BLOCK_FLAG_HAS_NAN
    | BLOCK_FLAG_CONST_EMPTY_COLLECTION
    | BLOCK_FLAG_FSST_SUBCHUNKED;

// --- Hard Untrusted-Input Bounds (spec-v0.md §7) ---

/// Maximum rows allowed in a single `.statam` file.
pub const MAX_ROWS_PER_FILE: u64 = u32::MAX as u64;
/// Maximum Hot Tail size (16 MiB). Writers target <= 1 MiB.
pub const MAX_HOT_TAIL_BYTES: usize = 16 * 1024 * 1024;
/// Maximum logical or physical columns per file.
pub const MAX_COLUMNS: usize = 1_024;
/// Maximum `DataType::List` nesting depth.
pub const MAX_NESTING_DEPTH: usize = cetra_column::MAX_NESTING_DEPTH;
/// Maximum Row Groups per file.
pub const MAX_ROW_GROUPS: usize = 262_144;
/// Maximum Mini-Blocks across all columns in a file.
pub const MAX_TOTAL_BLOCKS: usize = 4_194_304;
/// Maximum uncompressed data block size (64 MiB).
pub const MAX_BLOCK_RAW_BYTES: usize = 64 * 1024 * 1024;
/// Maximum uncompressed pack frame size (16 MiB).
pub const MAX_FRAME_RAW_BYTES: usize = 16 * 1024 * 1024;
/// Minimum binary blob length (`32 KiB`) that receives its own dedicated 1-blob-per-frame `PF` frame.
pub const BLOB_DEDICATED_FRAME_BYTES: usize = 32 * 1024;
/// Maximum byte slice per `.stpk` / L0 blob chunk (`4 MiB <= MAX_FRAME_RAW_BYTES`).
pub const BLOB_MAX_CHUNK_BYTES: usize = 4 * 1024 * 1024;
/// Target uncompressed frame size (`64 KiB`) for batched small (`< 32 KiB`) binary blobs.
pub const BLOB_SMALL_FRAME_TARGET_BYTES: usize = 64 * 1024;
/// Maximum raw `.stpk` trained Zstd dictionary size (256 KiB).
pub const MAX_PACK_DICT_BYTES: usize = 256 * 1024;
/// Maximum decompression expansion ratio before rejecting as a zip bomb.
pub const MAX_DECOMPRESS_RATIO: u64 = 1_024;
/// Fixed slack bytes added to ratio check for small blocks.
pub const MAX_DECOMPRESS_SLACK: u64 = 16_384;
/// Maximum promoted keys per shredded `MapUtf8` column.
pub const MAX_SHREDDED_KEYS: usize = 64;
/// Maximum delta tail-sharing chain depth in `PayloadRefs`.
pub const MAX_REF_CHAIN_DEPTH: usize = 15;
/// Maximum chunks referenced by a single row payload (`u16::MAX` to match wire `u16` counts).
pub const MAX_CHUNKS_PER_ROW: usize = u16::MAX as usize;
/// Maximum reconstructed bytes for a single row payload (64 MiB).
pub const MAX_ROW_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
/// Maximum reconstructed bytes across one batch's `PayloadRefs` column (256 MiB).
pub const MAX_BATCH_DECODED_BYTES: usize = 256 * 1024 * 1024;
/// Maximum dictionary section size (32 MiB).
pub const MAX_DICT_BYTES: usize = 32 * 1024 * 1024;
/// Maximum bloom filter section size (32 MiB).
pub const MAX_BLOOM_BYTES: usize = 32 * 1024 * 1024;
/// Maximum frames in a `.stpk` pack.
pub const MAX_PACK_FRAMES: usize = 4_194_304;
/// Maximum chunks in a `.stpk` pack.
pub const MAX_PACK_CHUNKS: usize = 16_777_216;
/// Maximum pack references in a single `.statam` file.
pub const MAX_PACK_REFS: usize = 65_535;

/// Table kind stored in a `.statam` file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TableKind {
    /// Generic `cetra_column::Schema`.
    Generic = 0,
    /// OpenTelemetry spans table.
    Spans = 1,
    /// Traces summary table.
    Traces = 2,
    /// Evaluation scores table.
    Scores = 3,
}

impl TableKind {
    /// Parses a wire `u8` tag into [`TableKind`].
    pub fn from_u8(v: u8) -> Result<Self, StatamError> {
        match v {
            0 => Ok(Self::Generic),
            1 => Ok(Self::Spans),
            2 => Ok(Self::Traces),
            3 => Ok(Self::Scores),
            _ => Err(StatamError::Corrupt {
                reason: "unknown table kind",
            }),
        }
    }
}

/// Compression codec identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Codec {
    /// Uncompressed raw bytes.
    None = 0,
    /// Raw-block LZ4 (`lz4_flex::block`).
    Lz4 = 1,
    /// Zstd bulk block compression.
    Zstd = 2,
}

impl Codec {
    /// Parses a wire `u8` tag into [`Codec`].
    pub fn from_u8(v: u8) -> Result<Self, StatamError> {
        match v {
            0 => Ok(Self::None),
            1 => Ok(Self::Lz4),
            2 => Ok(Self::Zstd),
            _ => Err(StatamError::Corrupt {
                reason: "unknown compression codec",
            }),
        }
    }
}

/// Column mini-block encoding identifier (`0..=14`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Encoding {
    /// All-null mini-block (0 bytes in data region).
    ConstNull = 0,
    /// Single constant scalar or empty collection (0 bytes in data region).
    Const = 1,
    /// Run-length encoding.
    Rle = 2,
    /// Frame-of-reference + FastLanes bitpacking.
    ForBitpack = 3,
    /// FastLanes delta + bitpacking.
    DeltaBitpack = 4,
    /// Adaptive Lossless floating-Point (bit-exact `F64`).
    Alp = 5,
    /// Dictionary codes over a file-level dictionary.
    Dict = 6,
    /// Fast Static Symbol Table string/binary compression.
    Fsst = 7,
    /// Packed boolean bitmap.
    BitpackBool = 8,
    /// Plain fixed or length-prefixed values.
    Plain = 9,
    /// Length-prefixed homogeneous list with recursively encoded child block.
    ListEncoded = 10,
    /// Shredded `MapUtf8` residual block.
    ShreddedMap = 11,
    /// Structured `Events` column encoding.
    EventsStruct = 12,
    /// Structured `Links` column encoding.
    LinksStruct = 13,
    /// Tail-sharing payload references (`L0Fast` local or `L1Compact` `.stpk`).
    PayloadRefs = 14,
}

impl Encoding {
    /// Parses a wire `u8` tag into [`Encoding`].
    pub fn from_u8(v: u8) -> Result<Self, StatamError> {
        match v {
            0 => Ok(Self::ConstNull),
            1 => Ok(Self::Const),
            2 => Ok(Self::Rle),
            3 => Ok(Self::ForBitpack),
            4 => Ok(Self::DeltaBitpack),
            5 => Ok(Self::Alp),
            6 => Ok(Self::Dict),
            7 => Ok(Self::Fsst),
            8 => Ok(Self::BitpackBool),
            9 => Ok(Self::Plain),
            10 => Ok(Self::ListEncoded),
            11 => Ok(Self::ShreddedMap),
            12 => Ok(Self::EventsStruct),
            13 => Ok(Self::LinksStruct),
            14 => Ok(Self::PayloadRefs),
            _ => Err(StatamError::Corrupt {
                reason: "unknown column encoding",
            }),
        }
    }
}

/// Physical PAX column locality group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum LocalityGroup {
    /// Hot narrow columns read on scans and filters.
    Core = 0,
    /// Wide columns materialized after late filtering.
    WideRow = 1,
    /// Heavy payload columns stored as `PayloadRefs` + `.stpk`.
    ExternalPayload = 2,
}

impl LocalityGroup {
    /// Parses a wire `u8` tag into [`LocalityGroup`].
    pub fn from_u8(v: u8) -> Result<Self, StatamError> {
        match v {
            0 => Ok(Self::Core),
            1 => Ok(Self::WideRow),
            2 => Ok(Self::ExternalPayload),
            _ => Err(StatamError::Corrupt {
                reason: "unknown locality group",
            }),
        }
    }
}

/// Kind of entry in the Lazy Index Directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum LazyIndexKind {
    /// SBBF bloom filter stored outside the Hot Tail.
    Bloom = 1,
    /// File-level dictionary stored outside the Hot Tail.
    Dict = 2,
    /// Per-block trigram filter for a narrow `Utf8` column.
    NarrowTrigram = 3,
}

impl LazyIndexKind {
    /// Parses a wire `u8` tag into [`LazyIndexKind`].
    pub fn from_u8(v: u8) -> Result<Self, StatamError> {
        match v {
            1 => Ok(Self::Bloom),
            2 => Ok(Self::Dict),
            3 => Ok(Self::NarrowTrigram),
            _ => Err(StatamError::Corrupt {
                reason: "unknown lazy index kind",
            }),
        }
    }
}

/// 32-byte `.statam` File Header at offset 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileHeader {
    /// Format version (`1`).
    pub format_version: u16,
    /// File flags (identical to [`Postscript::flags`]).
    pub flags: u16,
    /// Table kind.
    pub table_kind: TableKind,
    /// Checksum algorithm (`1 = CRC32C`).
    pub checksum_alg: u8,
    /// Reserved bytes (`0` in v1).
    pub reserved0: u16,
    /// Required reader feature bitmask (`0` in v1).
    pub required_features: u32,
    /// Absolute offset of AEAD encryption envelope (`0` if `!ENCRYPTED`).
    pub enc_envelope_offset: u64,
    /// Byte length of AEAD encryption envelope (`0` if `!ENCRYPTED`).
    pub enc_envelope_len: u32,
}

impl FileHeader {
    /// Encodes the 32-byte header with its trailing CRC-32C.
    pub fn encode(&self) -> [u8; FILE_HEADER_LEN] {
        let mut buf = [0u8; FILE_HEADER_LEN];
        buf[0..4].copy_from_slice(&FILE_MAGIC);
        buf[4..6].copy_from_slice(&self.format_version.to_le_bytes());
        buf[6..8].copy_from_slice(&self.flags.to_le_bytes());
        buf[8] = self.table_kind as u8;
        buf[9] = self.checksum_alg;
        buf[10..12].copy_from_slice(&self.reserved0.to_le_bytes());
        buf[12..16].copy_from_slice(&self.required_features.to_le_bytes());
        buf[16..24].copy_from_slice(&self.enc_envelope_offset.to_le_bytes());
        buf[24..28].copy_from_slice(&self.enc_envelope_len.to_le_bytes());
        let crc = crc32c::crc32c(&buf[0..28]);
        buf[28..32].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    /// Decodes and validates a 32-byte `.statam` file header.
    pub fn decode(buf: &[u8]) -> Result<Self, StatamError> {
        if buf.len() != FILE_HEADER_LEN {
            return Err(StatamError::Corrupt {
                reason: "invalid file header length",
            });
        }
        if buf[0..4] != FILE_MAGIC {
            return Err(StatamError::Corrupt {
                reason: "invalid file header magic",
            });
        }
        let expected_crc = read_u32_le(buf, 28);
        let actual_crc = crc32c::crc32c(&buf[0..28]);
        if expected_crc != actual_crc {
            return Err(StatamError::ChecksumMismatch {
                context: "file header",
            });
        }
        let format_version = read_u16_le(buf, 4);
        if format_version != FORMAT_VERSION {
            return Err(StatamError::UnsupportedVersion {
                found: format_version,
                expected: FORMAT_VERSION,
            });
        }
        let flags = read_u16_le(buf, 6);
        let required_features = read_u32_le(buf, 12);
        if required_features != 0 {
            return Err(StatamError::UnsupportedFeature {
                mask: required_features,
            });
        }
        if flags & !KNOWN_FILE_FLAGS != 0 {
            return Err(StatamError::Corrupt {
                reason: "unknown file header flags",
            });
        }
        if flags & FLAG_ENCRYPTED != 0 {
            return Err(StatamError::EncryptionRequired);
        }
        let table_kind = TableKind::from_u8(buf[8])?;
        let checksum_alg = buf[9];
        if checksum_alg != CHECKSUM_ALG_CRC32C {
            return Err(StatamError::Corrupt {
                reason: "unsupported checksum algorithm",
            });
        }
        let reserved0 = read_u16_le(buf, 10);
        if reserved0 != 0 {
            return Err(StatamError::Corrupt {
                reason: "non-zero reserved bytes in file header",
            });
        }
        let enc_envelope_offset = read_u64_le(buf, 16);
        let enc_envelope_len = read_u32_le(buf, 24);
        if enc_envelope_offset != 0 || enc_envelope_len != 0 {
            return Err(StatamError::Corrupt {
                reason: "non-zero encryption envelope on unencrypted file header",
            });
        }
        Ok(Self {
            format_version,
            flags,
            table_kind,
            checksum_alg,
            reserved0,
            required_features,
            enc_envelope_offset,
            enc_envelope_len,
        })
    }
}

/// 112-byte `.statam` Postscript at the end of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Postscript {
    /// Stored bytes of Hot Tail including this 112-byte Postscript (`>= 112`).
    pub hot_tail_len: u32,
    /// Uncompressed length of `hot_tail_body`.
    pub hot_tail_raw_len: u32,
    /// Format version (`1`).
    pub format_version: u16,
    /// File flags.
    pub flags: u16,
    /// Required reader feature bitmask (`0` in v1).
    pub required_features: u32,
    /// Total rows in file.
    pub row_count: u64,
    /// Logical column count.
    pub num_logical_cols: u32,
    /// Physical column count (logical + shredded map sub-columns).
    pub num_phys_cols: u32,
    /// Count of [`RowGroupEntry`] records.
    pub num_row_groups: u32,
    /// Count of [`BlockEntry`] records.
    pub total_blocks: u32,
    /// Offset of Schema section inside uncompressed Hot Tail body.
    pub schema_off: u32,
    /// Byte length of Schema section.
    pub schema_len: u32,
    /// Offset of `[ColumnDirEntry; num_phys_cols]` inside uncompressed Hot Tail body.
    pub col_dir_off: u32,
    /// Offset of `[RowGroupEntry; num_row_groups]` inside uncompressed Hot Tail body.
    pub rg_dir_off: u32,
    /// Offset of `[BlockEntry; total_blocks]` inside uncompressed Hot Tail body.
    pub block_dir_off: u32,
    /// Offset of Hot Bloom section inside uncompressed Hot Tail body.
    pub hot_bloom_off: u32,
    /// Byte length of Hot Bloom section.
    pub hot_bloom_len: u32,
    /// Offset of Hot Dict section inside uncompressed Hot Tail body.
    pub hot_dict_off: u32,
    /// Byte length of Hot Dict section.
    pub hot_dict_len: u32,
    /// Offset of Lazy Index Directory inside uncompressed Hot Tail body.
    pub lazy_dir_off: u32,
    /// Byte length of Lazy Index Directory.
    pub lazy_dir_len: u32,
    /// Byte length of AEAD encryption envelope (`0` if `!ENCRYPTED`).
    pub enc_envelope_len: u32,
    /// Absolute file offset of AEAD encryption envelope (`0` if `!ENCRYPTED`).
    pub enc_envelope_offset: u64,
    /// CRC-32C over stored `hot_tail_body` (`[0..hot_tail_len - 112]`).
    pub hot_tail_crc32c: u32,
    /// Reserved (`0` in v1).
    pub reserved: u32,
}

impl Postscript {
    /// Encodes the 112-byte Postscript with its CRC-32C and trailing magic `b"0MTS"`.
    pub fn encode(&self) -> [u8; POSTSCRIPT_LEN] {
        let mut buf = [0u8; POSTSCRIPT_LEN];
        buf[0..4].copy_from_slice(&self.hot_tail_len.to_le_bytes());
        buf[4..8].copy_from_slice(&self.hot_tail_raw_len.to_le_bytes());
        buf[8..10].copy_from_slice(&self.format_version.to_le_bytes());
        buf[10..12].copy_from_slice(&self.flags.to_le_bytes());
        buf[12..16].copy_from_slice(&self.required_features.to_le_bytes());
        buf[16..24].copy_from_slice(&self.row_count.to_le_bytes());
        buf[24..28].copy_from_slice(&self.num_logical_cols.to_le_bytes());
        buf[28..32].copy_from_slice(&self.num_phys_cols.to_le_bytes());
        buf[32..36].copy_from_slice(&self.num_row_groups.to_le_bytes());
        buf[36..40].copy_from_slice(&self.total_blocks.to_le_bytes());
        buf[40..44].copy_from_slice(&self.schema_off.to_le_bytes());
        buf[44..48].copy_from_slice(&self.schema_len.to_le_bytes());
        buf[48..52].copy_from_slice(&self.col_dir_off.to_le_bytes());
        buf[52..56].copy_from_slice(&self.rg_dir_off.to_le_bytes());
        buf[56..60].copy_from_slice(&self.block_dir_off.to_le_bytes());
        buf[60..64].copy_from_slice(&self.hot_bloom_off.to_le_bytes());
        buf[64..68].copy_from_slice(&self.hot_bloom_len.to_le_bytes());
        buf[68..72].copy_from_slice(&self.hot_dict_off.to_le_bytes());
        buf[72..76].copy_from_slice(&self.hot_dict_len.to_le_bytes());
        buf[76..80].copy_from_slice(&self.lazy_dir_off.to_le_bytes());
        buf[80..84].copy_from_slice(&self.lazy_dir_len.to_le_bytes());
        buf[84..88].copy_from_slice(&self.enc_envelope_len.to_le_bytes());
        buf[88..96].copy_from_slice(&self.enc_envelope_offset.to_le_bytes());
        buf[96..100].copy_from_slice(&self.hot_tail_crc32c.to_le_bytes());
        buf[100..104].copy_from_slice(&self.reserved.to_le_bytes());
        let ps_crc = crc32c::crc32c(&buf[0..104]);
        buf[104..108].copy_from_slice(&ps_crc.to_le_bytes());
        buf[108..112].copy_from_slice(&POSTSCRIPT_MAGIC);
        buf
    }

    /// Decodes and validates a 112-byte `.statam` Postscript, enforcing all hard bounds.
    pub fn decode(buf: &[u8]) -> Result<Self, StatamError> {
        if buf.len() != POSTSCRIPT_LEN {
            return Err(StatamError::Corrupt {
                reason: "invalid postscript length",
            });
        }
        if buf[108..112] != POSTSCRIPT_MAGIC {
            return Err(StatamError::Corrupt {
                reason: "invalid postscript magic",
            });
        }
        let expected_crc = read_u32_le(buf, 104);
        let actual_crc = crc32c::crc32c(&buf[0..104]);
        if expected_crc != actual_crc {
            return Err(StatamError::ChecksumMismatch {
                context: "postscript",
            });
        }
        let format_version = read_u16_le(buf, 8);
        if format_version != FORMAT_VERSION {
            return Err(StatamError::UnsupportedVersion {
                found: format_version,
                expected: FORMAT_VERSION,
            });
        }
        let flags = read_u16_le(buf, 10);
        let required_features = read_u32_le(buf, 12);
        if required_features != 0 {
            return Err(StatamError::UnsupportedFeature {
                mask: required_features,
            });
        }
        if flags & !KNOWN_FILE_FLAGS != 0 {
            return Err(StatamError::Corrupt {
                reason: "unknown postscript flags",
            });
        }
        if flags & FLAG_ENCRYPTED != 0 {
            return Err(StatamError::EncryptionRequired);
        }

        let hot_tail_len = read_u32_le(buf, 0);
        let hot_tail_raw_len = read_u32_le(buf, 4);
        if (hot_tail_len as usize) < POSTSCRIPT_LEN {
            return Err(StatamError::Corrupt {
                reason: "hot_tail_len smaller than postscript",
            });
        }
        if (hot_tail_len as usize) > MAX_HOT_TAIL_BYTES
            || (hot_tail_raw_len as usize) > MAX_HOT_TAIL_BYTES
        {
            return Err(StatamError::BoundsExceeded {
                reason: "hot tail exceeds MAX_HOT_TAIL_BYTES",
            });
        }
        let stored_body_len = (hot_tail_len as usize) - POSTSCRIPT_LEN;
        if flags & FLAG_HOT_TAIL_LZ4 == 0 && (hot_tail_raw_len as usize) != stored_body_len {
            return Err(StatamError::Corrupt {
                reason: "uncompressed hot_tail_raw_len mismatch",
            });
        }
        if flags & FLAG_HOT_TAIL_LZ4 != 0 {
            check_decompress_bounds(
                hot_tail_raw_len as usize,
                stored_body_len,
                MAX_HOT_TAIL_BYTES,
            )?;
        }

        let row_count = read_u64_le(buf, 16);
        if row_count > MAX_ROWS_PER_FILE {
            return Err(StatamError::BoundsExceeded {
                reason: "row_count exceeds MAX_ROWS_PER_FILE",
            });
        }
        let num_logical_cols = read_u32_le(buf, 24);
        let num_phys_cols = read_u32_le(buf, 28);
        if (num_logical_cols as usize) > MAX_COLUMNS || (num_phys_cols as usize) > MAX_COLUMNS {
            return Err(StatamError::BoundsExceeded {
                reason: "column count exceeds MAX_COLUMNS",
            });
        }
        if num_logical_cols > num_phys_cols {
            return Err(StatamError::Corrupt {
                reason: "num_logical_cols exceeds num_phys_cols",
            });
        }
        let num_row_groups = read_u32_le(buf, 32);
        if (num_row_groups as usize) > MAX_ROW_GROUPS {
            return Err(StatamError::BoundsExceeded {
                reason: "num_row_groups exceeds MAX_ROW_GROUPS",
            });
        }
        let total_blocks = read_u32_le(buf, 36);
        if (total_blocks as usize) > MAX_TOTAL_BLOCKS {
            return Err(StatamError::BoundsExceeded {
                reason: "total_blocks exceeds MAX_TOTAL_BLOCKS",
            });
        }

        let schema_off = read_u32_le(buf, 40);
        let schema_len = read_u32_le(buf, 44);
        let col_dir_off = read_u32_le(buf, 48);
        let rg_dir_off = read_u32_le(buf, 52);
        let block_dir_off = read_u32_le(buf, 56);
        let hot_bloom_off = read_u32_le(buf, 60);
        let hot_bloom_len = read_u32_le(buf, 64);
        let hot_dict_off = read_u32_le(buf, 68);
        let hot_dict_len = read_u32_le(buf, 72);
        let lazy_dir_off = read_u32_le(buf, 76);
        let lazy_dir_len = read_u32_le(buf, 80);
        let enc_envelope_len = read_u32_le(buf, 84);
        let enc_envelope_offset = read_u64_le(buf, 88);
        let hot_tail_crc32c = read_u32_le(buf, 96);
        let reserved = read_u32_le(buf, 100);

        if reserved != 0 {
            return Err(StatamError::Corrupt {
                reason: "non-zero reserved bytes in postscript",
            });
        }
        if enc_envelope_len != 0 || enc_envelope_offset != 0 {
            return Err(StatamError::Corrupt {
                reason: "non-zero encryption envelope on unencrypted postscript",
            });
        }

        // Validate every Hot Tail section lies strictly within hot_tail_raw_len.
        let raw_limit = hot_tail_raw_len as usize;
        check_slice_range(schema_off, schema_len, raw_limit)?;
        let col_dir_bytes = (num_phys_cols as usize)
            .checked_mul(COLUMN_DIR_ENTRY_LEN)
            .ok_or(StatamError::BoundsExceeded {
                reason: "column directory byte length overflow",
            })?;
        check_range_usize(col_dir_off as usize, col_dir_bytes, raw_limit)?;
        let rg_dir_bytes = (num_row_groups as usize)
            .checked_mul(ROW_GROUP_ENTRY_LEN)
            .ok_or(StatamError::BoundsExceeded {
                reason: "row group directory byte length overflow",
            })?;
        check_range_usize(rg_dir_off as usize, rg_dir_bytes, raw_limit)?;
        let block_dir_bytes = (total_blocks as usize).checked_mul(BLOCK_ENTRY_LEN).ok_or(
            StatamError::BoundsExceeded {
                reason: "block directory byte length overflow",
            },
        )?;
        check_range_usize(block_dir_off as usize, block_dir_bytes, raw_limit)?;

        if (hot_bloom_len as usize) > MAX_BLOOM_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "hot_bloom_len exceeds MAX_BLOOM_BYTES",
            });
        }
        if hot_bloom_len == 0 && hot_bloom_off != 0 {
            return Err(StatamError::Corrupt {
                reason: "non-zero hot_bloom_off when hot_bloom_len is 0",
            });
        }
        check_slice_range(hot_bloom_off, hot_bloom_len, raw_limit)?;

        if (hot_dict_len as usize) > MAX_DICT_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "hot_dict_len exceeds MAX_DICT_BYTES",
            });
        }
        if hot_dict_len == 0 && hot_dict_off != 0 {
            return Err(StatamError::Corrupt {
                reason: "non-zero hot_dict_off when hot_dict_len is 0",
            });
        }
        check_slice_range(hot_dict_off, hot_dict_len, raw_limit)?;

        if !(lazy_dir_len as usize).is_multiple_of(LAZY_INDEX_ENTRY_LEN) {
            return Err(StatamError::Corrupt {
                reason: "lazy_dir_len is not a multiple of entry size",
            });
        }
        if lazy_dir_len == 0 && lazy_dir_off != 0 {
            return Err(StatamError::Corrupt {
                reason: "non-zero lazy_dir_off when lazy_dir_len is 0",
            });
        }
        check_slice_range(lazy_dir_off, lazy_dir_len, raw_limit)?;

        Ok(Self {
            hot_tail_len,
            hot_tail_raw_len,
            format_version,
            flags,
            required_features,
            row_count,
            num_logical_cols,
            num_phys_cols,
            num_row_groups,
            total_blocks,
            schema_off,
            schema_len,
            col_dir_off,
            rg_dir_off,
            block_dir_off,
            hot_bloom_off,
            hot_bloom_len,
            hot_dict_off,
            hot_dict_len,
            lazy_dir_off,
            lazy_dir_len,
            enc_envelope_len,
            enc_envelope_offset,
            hot_tail_crc32c,
            reserved,
        })
    }
}

/// 48-byte `.stpk` Pack Header at offset 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackHeader {
    /// Format version (`1`).
    pub format_version: u16,
    /// Pack flags.
    pub flags: u16,
    /// Required reader feature bitmask (`0` in v1).
    pub required_features: u32,
    /// BLAKE3-128 / unique identifier of this pack.
    pub pack_id: [u8; 16],
    /// Absolute offset of AEAD encryption envelope (`0` if `!ENCRYPTED`).
    pub enc_envelope_offset: u64,
    /// Byte length of AEAD encryption envelope (`0` if `!ENCRYPTED`).
    pub enc_envelope_len: u32,
    /// Reserved (`0` in v1).
    pub reserved: u32,
}

impl PackHeader {
    /// Encodes the 48-byte pack header with trailing CRC-32C.
    pub fn encode(&self) -> [u8; PACK_HEADER_LEN] {
        let mut buf = [0u8; PACK_HEADER_LEN];
        buf[0..4].copy_from_slice(&PACK_MAGIC);
        buf[4..6].copy_from_slice(&self.format_version.to_le_bytes());
        buf[6..8].copy_from_slice(&self.flags.to_le_bytes());
        buf[8..12].copy_from_slice(&self.required_features.to_le_bytes());
        buf[12..28].copy_from_slice(&self.pack_id);
        buf[28..36].copy_from_slice(&self.enc_envelope_offset.to_le_bytes());
        buf[36..40].copy_from_slice(&self.enc_envelope_len.to_le_bytes());
        buf[40..44].copy_from_slice(&self.reserved.to_le_bytes());
        let crc = crc32c::crc32c(&buf[0..44]);
        buf[44..48].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    /// Decodes and validates a 48-byte `.stpk` pack header.
    pub fn decode(buf: &[u8]) -> Result<Self, StatamError> {
        if buf.len() != PACK_HEADER_LEN {
            return Err(StatamError::Corrupt {
                reason: "invalid pack header length",
            });
        }
        if buf[0..4] != PACK_MAGIC {
            return Err(StatamError::Corrupt {
                reason: "invalid pack header magic",
            });
        }
        let expected_crc = read_u32_le(buf, 44);
        let actual_crc = crc32c::crc32c(&buf[0..44]);
        if expected_crc != actual_crc {
            return Err(StatamError::ChecksumMismatch {
                context: "pack header",
            });
        }
        let format_version = read_u16_le(buf, 4);
        if format_version != FORMAT_VERSION {
            return Err(StatamError::UnsupportedVersion {
                found: format_version,
                expected: FORMAT_VERSION,
            });
        }
        let flags = read_u16_le(buf, 6);
        let required_features = read_u32_le(buf, 8);
        if required_features != 0 {
            return Err(StatamError::UnsupportedFeature {
                mask: required_features,
            });
        }
        if flags & !KNOWN_PACK_FLAGS != 0 {
            return Err(StatamError::Corrupt {
                reason: "unknown pack header flags",
            });
        }
        if flags & PACK_FLAG_ENCRYPTED != 0 {
            return Err(StatamError::EncryptionRequired);
        }
        if (flags & PACK_FLAG_TOC_LZ4 != 0) && (flags & PACK_FLAG_TOC_ZSTD != 0) {
            return Err(StatamError::Corrupt {
                reason: "conflicting pack TOC compression flags",
            });
        }
        let mut pack_id = [0u8; 16];
        pack_id.copy_from_slice(&buf[12..28]);
        let enc_envelope_offset = read_u64_le(buf, 28);
        let enc_envelope_len = read_u32_le(buf, 36);
        let reserved = read_u32_le(buf, 40);
        if reserved != 0 {
            return Err(StatamError::Corrupt {
                reason: "non-zero reserved bytes in pack header",
            });
        }
        if enc_envelope_offset != 0 || enc_envelope_len != 0 {
            return Err(StatamError::Corrupt {
                reason: "non-zero encryption envelope on unencrypted pack header",
            });
        }
        Ok(Self {
            format_version,
            flags,
            required_features,
            pack_id,
            enc_envelope_offset,
            enc_envelope_len,
            reserved,
        })
    }
}

/// 112-byte `.stpk` Pack Postscript at the end of a pack file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackPostscript {
    /// Stored bytes of TOC including this 112-byte PackPostscript (`>= 112`).
    pub toc_stored_len: u32,
    /// Uncompressed byte length of the Pack TOC body.
    pub toc_raw_len: u32,
    /// Format version (`1`).
    pub format_version: u16,
    /// Pack flags.
    pub flags: u16,
    /// Required reader feature bitmask (`0` in v1).
    pub required_features: u32,
    /// 128-bit pack identifier.
    pub pack_id: [u8; 16],
    /// Number of [`FrameEntry`] records in TOC.
    pub frame_count: u32,
    /// Number of [`ChunkLoc`] records in TOC.
    pub chunk_count: u32,
    /// Absolute offset of compaction-only `[[u8; 16]; chunk_count]` hash table.
    pub hash_table_off: u64,
    /// Absolute offset of Trigram Skip section (`0` if absent).
    pub trigram_off: u64,
    /// Byte length of Trigram Skip section (`0` if absent).
    pub trigram_len: u32,
    /// Byte length of EdgeTable section (`0` if absent).
    pub edge_table_len: u32,
    /// Absolute offset of EdgeTable section (`0` if absent).
    pub edge_table_off: u64,
    /// Absolute offset of optional DictSection (`0` if absent).
    pub dict_off: u64,
    /// Byte length of optional DictSection (`0` if absent).
    pub dict_len: u32,
    /// Absolute offset of AEAD encryption envelope (`0` if `!ENCRYPTED`).
    pub enc_envelope_offset: u64,
    /// Byte length of AEAD encryption envelope (`0` if `!ENCRYPTED`).
    pub enc_envelope_len: u32,
    /// Reserved (`0` in v1).
    pub reserved: u32,
    /// CRC-32C over stored Pack TOC body.
    pub toc_crc32c: u32,
}

impl PackPostscript {
    /// Encodes the 112-byte PackPostscript with CRC-32C and trailing magic `b"0KPS"`.
    pub fn encode(&self) -> [u8; PACK_POSTSCRIPT_LEN] {
        let mut buf = [0u8; PACK_POSTSCRIPT_LEN];
        buf[0..4].copy_from_slice(&self.toc_stored_len.to_le_bytes());
        buf[4..8].copy_from_slice(&self.toc_raw_len.to_le_bytes());
        buf[8..10].copy_from_slice(&self.format_version.to_le_bytes());
        buf[10..12].copy_from_slice(&self.flags.to_le_bytes());
        buf[12..16].copy_from_slice(&self.required_features.to_le_bytes());
        buf[16..32].copy_from_slice(&self.pack_id);
        buf[32..36].copy_from_slice(&self.frame_count.to_le_bytes());
        buf[36..40].copy_from_slice(&self.chunk_count.to_le_bytes());
        buf[40..48].copy_from_slice(&self.hash_table_off.to_le_bytes());
        buf[48..56].copy_from_slice(&self.trigram_off.to_le_bytes());
        buf[56..60].copy_from_slice(&self.trigram_len.to_le_bytes());
        buf[60..64].copy_from_slice(&self.edge_table_len.to_le_bytes());
        buf[64..72].copy_from_slice(&self.edge_table_off.to_le_bytes());
        buf[72..80].copy_from_slice(&self.dict_off.to_le_bytes());
        buf[80..84].copy_from_slice(&self.dict_len.to_le_bytes());
        buf[84..92].copy_from_slice(&self.enc_envelope_offset.to_le_bytes());
        buf[92..96].copy_from_slice(&self.enc_envelope_len.to_le_bytes());
        buf[96..100].copy_from_slice(&self.reserved.to_le_bytes());
        buf[100..104].copy_from_slice(&self.toc_crc32c.to_le_bytes());
        let ps_crc = crc32c::crc32c(&buf[0..104]);
        buf[104..108].copy_from_slice(&ps_crc.to_le_bytes());
        buf[108..112].copy_from_slice(&PACK_POSTSCRIPT_MAGIC);
        buf
    }

    /// Decodes and validates a 112-byte `.stpk` PackPostscript.
    pub fn decode(buf: &[u8]) -> Result<Self, StatamError> {
        if buf.len() != PACK_POSTSCRIPT_LEN {
            return Err(StatamError::Corrupt {
                reason: "invalid pack postscript length",
            });
        }
        if buf[108..112] != PACK_POSTSCRIPT_MAGIC {
            return Err(StatamError::Corrupt {
                reason: "invalid pack postscript magic",
            });
        }
        let expected_crc = read_u32_le(buf, 104);
        let actual_crc = crc32c::crc32c(&buf[0..104]);
        if expected_crc != actual_crc {
            return Err(StatamError::ChecksumMismatch {
                context: "pack postscript",
            });
        }
        let format_version = read_u16_le(buf, 8);
        if format_version != FORMAT_VERSION {
            return Err(StatamError::UnsupportedVersion {
                found: format_version,
                expected: FORMAT_VERSION,
            });
        }
        let flags = read_u16_le(buf, 10);
        let required_features = read_u32_le(buf, 12);
        if required_features != 0 {
            return Err(StatamError::UnsupportedFeature {
                mask: required_features,
            });
        }
        if flags & !KNOWN_PACK_FLAGS != 0 {
            return Err(StatamError::Corrupt {
                reason: "unknown pack postscript flags",
            });
        }
        if flags & PACK_FLAG_ENCRYPTED != 0 {
            return Err(StatamError::EncryptionRequired);
        }
        if (flags & PACK_FLAG_TOC_LZ4 != 0) && (flags & PACK_FLAG_TOC_ZSTD != 0) {
            return Err(StatamError::Corrupt {
                reason: "conflicting pack TOC compression flags",
            });
        }

        let toc_stored_len = read_u32_le(buf, 0);
        let toc_raw_len = read_u32_le(buf, 4);
        if (toc_stored_len as usize) < PACK_POSTSCRIPT_LEN {
            return Err(StatamError::Corrupt {
                reason: "toc_stored_len smaller than pack postscript",
            });
        }
        if (toc_stored_len as usize) > MAX_HOT_TAIL_BYTES
            || (toc_raw_len as usize) > MAX_HOT_TAIL_BYTES
        {
            return Err(StatamError::BoundsExceeded {
                reason: "pack TOC exceeds MAX_HOT_TAIL_BYTES",
            });
        }
        let stored_body_len = (toc_stored_len as usize) - PACK_POSTSCRIPT_LEN;
        let toc_compressed = flags & (PACK_FLAG_TOC_LZ4 | PACK_FLAG_TOC_ZSTD) != 0;
        if !toc_compressed && (toc_raw_len as usize) != stored_body_len {
            return Err(StatamError::Corrupt {
                reason: "uncompressed toc_raw_len mismatch",
            });
        }
        if toc_compressed {
            check_decompress_bounds(toc_raw_len as usize, stored_body_len, MAX_HOT_TAIL_BYTES)?;
        }

        let frame_count = read_u32_le(buf, 32);
        let chunk_count = read_u32_le(buf, 36);
        if (frame_count as usize) > MAX_PACK_FRAMES {
            return Err(StatamError::BoundsExceeded {
                reason: "frame_count exceeds MAX_PACK_FRAMES",
            });
        }
        if (chunk_count as usize) > MAX_PACK_CHUNKS {
            return Err(StatamError::BoundsExceeded {
                reason: "chunk_count exceeds MAX_PACK_CHUNKS",
            });
        }

        let frames_bytes = (frame_count as usize).checked_mul(FRAME_ENTRY_LEN).ok_or(
            StatamError::BoundsExceeded {
                reason: "pack frame table size overflow",
            },
        )?;
        let chunks_bytes = (chunk_count as usize).checked_mul(CHUNK_LOC_LEN).ok_or(
            StatamError::BoundsExceeded {
                reason: "pack chunk table size overflow",
            },
        )?;
        let expected_toc_raw =
            frames_bytes
                .checked_add(chunks_bytes)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "pack TOC total size overflow",
                })?;
        if (toc_raw_len as usize) != expected_toc_raw {
            return Err(StatamError::Corrupt {
                reason: "pack toc_raw_len does not match frame_count and chunk_count",
            });
        }

        let mut pack_id = [0u8; 16];
        pack_id.copy_from_slice(&buf[16..32]);
        let hash_table_off = read_u64_le(buf, 40);
        let trigram_off = read_u64_le(buf, 48);
        let trigram_len = read_u32_le(buf, 56);
        let edge_table_len = read_u32_le(buf, 60);
        let edge_table_off = read_u64_le(buf, 64);
        let dict_off = read_u64_le(buf, 72);
        let dict_len = read_u32_le(buf, 80);
        let enc_envelope_offset = read_u64_le(buf, 84);
        let enc_envelope_len = read_u32_le(buf, 92);
        let reserved = read_u32_le(buf, 96);
        let toc_crc32c = read_u32_le(buf, 100);

        if reserved != 0 {
            return Err(StatamError::Corrupt {
                reason: "non-zero reserved bytes in pack postscript",
            });
        }
        if enc_envelope_offset != 0 || enc_envelope_len != 0 {
            return Err(StatamError::Corrupt {
                reason: "non-zero encryption envelope on unencrypted pack postscript",
            });
        }
        if chunk_count == 0 && hash_table_off != 0 {
            return Err(StatamError::Corrupt {
                reason: "non-zero hash_table_off when chunk_count is 0",
            });
        }
        if trigram_len == 0 && trigram_off != 0 {
            return Err(StatamError::Corrupt {
                reason: "non-zero trigram_off when trigram_len is 0",
            });
        }
        if edge_table_len == 0 && edge_table_off != 0 {
            return Err(StatamError::Corrupt {
                reason: "non-zero edge_table_off when edge_table_len is 0",
            });
        }
        if dict_len == 0 && dict_off != 0 {
            return Err(StatamError::Corrupt {
                reason: "non-zero dict_off when dict_len is 0",
            });
        }
        if (dict_len as usize) > MAX_PACK_DICT_BYTES + 1024 {
            return Err(StatamError::BoundsExceeded {
                reason: "pack dict_len exceeds MAX_PACK_DICT_BYTES + 1024",
            });
        }
        if (trigram_len as usize) > MAX_BLOOM_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "pack trigram_len exceeds MAX_BLOOM_BYTES",
            });
        }
        if (edge_table_len as usize) > MAX_BLOCK_RAW_BYTES + PACK_FRAME_HEADER_LEN {
            return Err(StatamError::BoundsExceeded {
                reason: "pack edge_table_len exceeds MAX_BLOCK_RAW_BYTES + PACK_FRAME_HEADER_LEN",
            });
        }

        Ok(Self {
            toc_stored_len,
            toc_raw_len,
            format_version,
            flags,
            required_features,
            pack_id,
            frame_count,
            chunk_count,
            hash_table_off,
            trigram_off,
            trigram_len,
            edge_table_len,
            edge_table_off,
            dict_off,
            dict_len,
            enc_envelope_offset,
            enc_envelope_len,
            reserved,
            toc_crc32c,
        })
    }
}

/// 56-byte Column Directory Entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnDirEntry {
    /// Physical column index (`0..num_phys_cols`).
    pub phys_col_id: u16,
    /// Logical column index (`0..num_logical_cols`).
    pub logical_col_id: u16,
    /// PAX locality group.
    pub locality_group: LocalityGroup,
    /// Column flags (`COL_FLAG_*`).
    pub col_flags: u8,
    /// Reserved (`0`).
    pub reserved: u16,
    /// Number of mini-blocks for this column.
    pub num_blocks: u32,
    /// Starting index in the global `BlockEntry` array.
    pub first_block_idx: u32,
    /// Dictionary offset (Hot-Tail-relative if `COL_FLAG_DICT_IN_HOT_TAIL`, else file-absolute).
    pub dict_offset: u64,
    /// Dictionary byte length (`0` if none).
    pub dict_len: u32,
    /// Number of entries in dictionary.
    pub dict_entry_count: u32,
    /// Bloom offset (Hot-Tail-relative if `COL_FLAG_BLOOM_IN_HOT_TAIL`, else file-absolute).
    pub bloom_offset: u64,
    /// Bloom byte length (`0` if none).
    pub bloom_len: u32,
    /// Total nulls across all row groups in this column.
    pub null_count_total: u32,
    /// File-absolute offset of narrow-column trigram section (`0` if none).
    pub trigram_offset: u64,
}

impl ColumnDirEntry {
    /// Encodes a 56-byte [`ColumnDirEntry`].
    pub fn encode(&self) -> [u8; COLUMN_DIR_ENTRY_LEN] {
        let mut buf = [0u8; COLUMN_DIR_ENTRY_LEN];
        buf[0..2].copy_from_slice(&self.phys_col_id.to_le_bytes());
        buf[2..4].copy_from_slice(&self.logical_col_id.to_le_bytes());
        buf[4] = self.locality_group as u8;
        buf[5] = self.col_flags;
        buf[6..8].copy_from_slice(&self.reserved.to_le_bytes());
        buf[8..12].copy_from_slice(&self.num_blocks.to_le_bytes());
        buf[12..16].copy_from_slice(&self.first_block_idx.to_le_bytes());
        buf[16..24].copy_from_slice(&self.dict_offset.to_le_bytes());
        buf[24..28].copy_from_slice(&self.dict_len.to_le_bytes());
        buf[28..32].copy_from_slice(&self.dict_entry_count.to_le_bytes());
        buf[32..40].copy_from_slice(&self.bloom_offset.to_le_bytes());
        buf[40..44].copy_from_slice(&self.bloom_len.to_le_bytes());
        buf[44..48].copy_from_slice(&self.null_count_total.to_le_bytes());
        buf[48..56].copy_from_slice(&self.trigram_offset.to_le_bytes());
        buf
    }

    /// Decodes a 56-byte [`ColumnDirEntry`].
    pub fn decode(buf: &[u8]) -> Result<Self, StatamError> {
        if buf.len() != COLUMN_DIR_ENTRY_LEN {
            return Err(StatamError::Corrupt {
                reason: "invalid column directory entry length",
            });
        }
        let dict_len = read_u32_le(buf, 24);
        if (dict_len as usize) > MAX_DICT_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "column dict_len exceeds MAX_DICT_BYTES",
            });
        }
        let bloom_len = read_u32_le(buf, 40);
        if (bloom_len as usize) > MAX_BLOOM_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "column bloom_len exceeds MAX_BLOOM_BYTES",
            });
        }
        let col_flags = buf[5];
        if col_flags & !KNOWN_COL_FLAGS != 0 {
            return Err(StatamError::Corrupt {
                reason: "unknown column directory flags",
            });
        }
        let reserved = read_u16_le(buf, 6);
        if reserved != 0 {
            return Err(StatamError::Corrupt {
                reason: "non-zero reserved bytes in column directory entry",
            });
        }
        Ok(Self {
            phys_col_id: read_u16_le(buf, 0),
            logical_col_id: read_u16_le(buf, 2),
            locality_group: LocalityGroup::from_u8(buf[4])?,
            col_flags,
            reserved,
            num_blocks: read_u32_le(buf, 8),
            first_block_idx: read_u32_le(buf, 12),
            dict_offset: read_u64_le(buf, 16),
            dict_len,
            dict_entry_count: read_u32_le(buf, 28),
            bloom_offset: read_u64_le(buf, 32),
            bloom_len,
            null_count_total: read_u32_le(buf, 44),
            trigram_offset: read_u64_le(buf, 48),
        })
    }
}

/// 48-byte Row-Group Directory Entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowGroupEntry {
    /// Starting global row index in file.
    pub first_row: u32,
    /// Number of rows in this row group (`<= MAX_ROW_GROUP_SIZE`).
    pub row_count: u32,
    /// Absolute offset of Core locality group slice.
    pub core_offset: u64,
    /// Byte length of Core locality group slice.
    pub core_len: u32,
    /// Byte length of WideRow locality group slice.
    pub wide_len: u32,
    /// Absolute offset of WideRow locality group slice (`== core_offset + core_len`).
    pub wide_offset: u64,
    /// Absolute offset of local L0 payload frames (`0` in `L1Compact`).
    pub l0_payload_offset: u64,
    /// Byte length of local L0 payload frames (`0` in `L1Compact`).
    pub l0_payload_len: u32,
    /// Row-group flags (`0` in v1).
    pub rg_flags: u32,
}

impl RowGroupEntry {
    /// Encodes a 48-byte [`RowGroupEntry`].
    pub fn encode(&self) -> [u8; ROW_GROUP_ENTRY_LEN] {
        let mut buf = [0u8; ROW_GROUP_ENTRY_LEN];
        buf[0..4].copy_from_slice(&self.first_row.to_le_bytes());
        buf[4..8].copy_from_slice(&self.row_count.to_le_bytes());
        buf[8..16].copy_from_slice(&self.core_offset.to_le_bytes());
        buf[16..20].copy_from_slice(&self.core_len.to_le_bytes());
        buf[20..24].copy_from_slice(&self.wide_len.to_le_bytes());
        buf[24..32].copy_from_slice(&self.wide_offset.to_le_bytes());
        buf[32..40].copy_from_slice(&self.l0_payload_offset.to_le_bytes());
        buf[40..44].copy_from_slice(&self.l0_payload_len.to_le_bytes());
        buf[44..48].copy_from_slice(&self.rg_flags.to_le_bytes());
        buf
    }

    /// Decodes a 48-byte [`RowGroupEntry`].
    pub fn decode(buf: &[u8]) -> Result<Self, StatamError> {
        if buf.len() != ROW_GROUP_ENTRY_LEN {
            return Err(StatamError::Corrupt {
                reason: "invalid row group directory entry length",
            });
        }
        let row_count = read_u32_le(buf, 4);
        if (row_count as usize) > MAX_ROW_GROUP_SIZE {
            return Err(StatamError::BoundsExceeded {
                reason: "row group row_count exceeds MAX_ROW_GROUP_SIZE",
            });
        }
        let core_offset = read_u64_le(buf, 8);
        let core_len = read_u32_le(buf, 16);
        let wide_len = read_u32_le(buf, 20);
        let wide_offset = read_u64_le(buf, 24);
        let expected_wide_offset =
            core_offset
                .checked_add(u64::from(core_len))
                .ok_or(StatamError::BoundsExceeded {
                    reason: "row group core_offset + core_len overflow",
                })?;
        if wide_offset != expected_wide_offset {
            return Err(StatamError::Corrupt {
                reason: "row group wide_offset not contiguous with core_offset + core_len",
            });
        }
        let rg_flags = read_u32_le(buf, 44);
        if rg_flags != 0 {
            return Err(StatamError::Corrupt {
                reason: "non-zero row group flags",
            });
        }
        Ok(Self {
            first_row: read_u32_le(buf, 0),
            row_count,
            core_offset,
            core_len,
            wide_len,
            wide_offset,
            l0_payload_offset: read_u64_le(buf, 32),
            l0_payload_len: read_u32_le(buf, 40),
            rg_flags,
        })
    }
}

/// 64-byte Mini-Block Directory Entry (including inline 16-byte min/max zone map).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct BlockEntry {
    /// Starting row index relative to the file.
    pub first_row: u32,
    /// Number of rows in this mini-block (`<= MAX_MINI_BLOCK_SIZE`).
    pub row_count: u16,
    /// Number of null rows (`<= row_count`).
    pub null_count: u16,
    /// File-absolute byte offset (`0` for inline `Const`/`ConstNull`).
    pub offset: u64,
    /// Stored byte length in data region (`0` for inline `Const`/`ConstNull`).
    pub stored_len: u32,
    /// Uncompressed block body length.
    pub raw_len: u32,
    /// Block encoding.
    pub encoding: Encoding,
    /// Block compression codec.
    pub codec: Codec,
    /// Block flags (`BLOCK_FLAG_*`).
    pub block_flags: u8,
    /// Active prefix length of `min_inline` (`0..=16`).
    pub min_inline_len: u8,
    /// Active prefix length of `max_inline` (`0..=16`).
    pub max_inline_len: u8,
    /// Reserved (`[0; 3]`).
    pub reserved: [u8; 3],
    /// Inline lower bound / constant payload (`0..min_inline_len`).
    pub min_inline: [u8; 16],
    /// Inline upper bound (`0..max_inline_len`).
    pub max_inline: [u8; 16],
}

impl fmt::Debug for BlockEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlockEntry")
            .field("first_row", &self.first_row)
            .field("row_count", &self.row_count)
            .field("null_count", &self.null_count)
            .field("offset", &self.offset)
            .field("stored_len", &self.stored_len)
            .field("raw_len", &self.raw_len)
            .field("encoding", &self.encoding)
            .field("codec", &self.codec)
            .field("block_flags", &self.block_flags)
            .field("min_inline_len", &self.min_inline_len)
            .field("max_inline_len", &self.max_inline_len)
            .finish()
    }
}

impl BlockEntry {
    /// Encodes a 64-byte [`BlockEntry`].
    pub fn encode(&self) -> [u8; BLOCK_ENTRY_LEN] {
        let mut buf = [0u8; BLOCK_ENTRY_LEN];
        buf[0..4].copy_from_slice(&self.first_row.to_le_bytes());
        buf[4..6].copy_from_slice(&self.row_count.to_le_bytes());
        buf[6..8].copy_from_slice(&self.null_count.to_le_bytes());
        buf[8..16].copy_from_slice(&self.offset.to_le_bytes());
        buf[16..20].copy_from_slice(&self.stored_len.to_le_bytes());
        buf[20..24].copy_from_slice(&self.raw_len.to_le_bytes());
        buf[24] = self.encoding as u8;
        buf[25] = self.codec as u8;
        buf[26] = self.block_flags;
        buf[27] = self.min_inline_len;
        buf[28] = self.max_inline_len;
        buf[29..32].copy_from_slice(&self.reserved);
        buf[32..48].copy_from_slice(&self.min_inline);
        buf[48..64].copy_from_slice(&self.max_inline);
        buf
    }

    /// Decodes a 64-byte [`BlockEntry`].
    pub fn decode(buf: &[u8]) -> Result<Self, StatamError> {
        if buf.len() != BLOCK_ENTRY_LEN {
            return Err(StatamError::Corrupt {
                reason: "invalid block directory entry length",
            });
        }
        let row_count = read_u16_le(buf, 4);
        let null_count = read_u16_le(buf, 6);
        if null_count > row_count {
            return Err(StatamError::Corrupt {
                reason: "mini-block null_count exceeds row_count",
            });
        }
        let stored_len = read_u32_le(buf, 16);
        if (stored_len as usize) > MAX_BLOCK_RAW_BYTES + 1024 {
            return Err(StatamError::BoundsExceeded {
                reason: "mini-block stored_len exceeds MAX_BLOCK_RAW_BYTES + 1024",
            });
        }
        let raw_len = read_u32_le(buf, 20);
        if (raw_len as usize) > MAX_BLOCK_RAW_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "mini-block raw_len exceeds MAX_BLOCK_RAW_BYTES",
            });
        }
        let encoding = Encoding::from_u8(buf[24])?;
        let codec = Codec::from_u8(buf[25])?;
        let block_flags = buf[26];
        if block_flags & !KNOWN_BLOCK_FLAGS != 0 {
            return Err(StatamError::Corrupt {
                reason: "unknown block directory flags",
            });
        }
        if (block_flags & BLOCK_FLAG_FSST_SUBCHUNKED) != 0 && encoding != Encoding::Fsst {
            return Err(StatamError::Corrupt {
                reason: "BLOCK_FLAG_FSST_SUBCHUNKED set on non-Fsst encoding",
            });
        }
        let min_inline_len = buf[27];
        let max_inline_len = buf[28];
        if min_inline_len > 16 || max_inline_len > 16 {
            return Err(StatamError::Corrupt {
                reason: "inline zone-map length exceeds 16 bytes",
            });
        }
        let mut reserved = [0u8; 3];
        reserved.copy_from_slice(&buf[29..32]);
        if reserved != [0; 3] {
            return Err(StatamError::Corrupt {
                reason: "non-zero reserved bytes in block directory entry",
            });
        }
        let mut min_inline = [0u8; 16];
        min_inline.copy_from_slice(&buf[32..48]);
        let mut max_inline = [0u8; 16];
        max_inline.copy_from_slice(&buf[48..64]);

        Ok(Self {
            first_row: read_u32_le(buf, 0),
            row_count,
            null_count,
            offset: read_u64_le(buf, 8),
            stored_len,
            raw_len,
            encoding,
            codec,
            block_flags,
            min_inline_len,
            max_inline_len,
            reserved,
            min_inline,
            max_inline,
        })
    }
}

/// 40-byte `.stpk` Pack Frame Directory Entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameEntry {
    /// Absolute offset of the `PF` frame in the `.stpk` file.
    pub offset: u64,
    /// Stored payload byte length (excluding the 16-byte `PF` header).
    pub stored_len: u32,
    /// Uncompressed frame payload bytes.
    pub raw_len: u32,
    /// Frame compression codec.
    pub codec: Codec,
    /// Frame flags (`PACK_FRAME_FLAG_*`).
    pub flags: u8,
    /// Dictionary ID if `PACK_FRAME_FLAG_USES_DICT` is set.
    pub dict_id: u16,
    /// First chunk ID inside this frame.
    pub first_chunk_id: u32,
    /// Number of consecutive chunks stored in this frame.
    pub chunk_count: u32,
    /// Offset of this frame's trigram filter within TrigramSkipSection.
    pub filter_off: u32,
    /// Byte length of this frame's trigram filter (`0` if omitted).
    pub filter_len: u32,
    /// Header-inclusive CRC-32C over `PF` header prefix `[0..12]` and compressed payload.
    pub crc32c: u32,
}

impl FrameEntry {
    /// Encodes a 40-byte [`FrameEntry`].
    pub fn encode(&self) -> [u8; FRAME_ENTRY_LEN] {
        let mut buf = [0u8; FRAME_ENTRY_LEN];
        buf[0..8].copy_from_slice(&self.offset.to_le_bytes());
        buf[8..12].copy_from_slice(&self.stored_len.to_le_bytes());
        buf[12..16].copy_from_slice(&self.raw_len.to_le_bytes());
        buf[16] = self.codec as u8;
        buf[17] = self.flags;
        buf[18..20].copy_from_slice(&self.dict_id.to_le_bytes());
        buf[20..24].copy_from_slice(&self.first_chunk_id.to_le_bytes());
        buf[24..28].copy_from_slice(&self.chunk_count.to_le_bytes());
        buf[28..32].copy_from_slice(&self.filter_off.to_le_bytes());
        buf[32..36].copy_from_slice(&self.filter_len.to_le_bytes());
        buf[36..40].copy_from_slice(&self.crc32c.to_le_bytes());
        buf
    }

    /// Decodes a 40-byte [`FrameEntry`].
    pub fn decode(buf: &[u8]) -> Result<Self, StatamError> {
        if buf.len() != FRAME_ENTRY_LEN {
            return Err(StatamError::Corrupt {
                reason: "invalid pack frame entry length",
            });
        }
        let stored_len = read_u32_le(buf, 8);
        if (stored_len as usize) > MAX_FRAME_RAW_BYTES + 1024 {
            return Err(StatamError::BoundsExceeded {
                reason: "pack frame stored_len exceeds MAX_FRAME_RAW_BYTES + 1024",
            });
        }
        let raw_len = read_u32_le(buf, 12);
        if (raw_len as usize) > MAX_FRAME_RAW_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "pack frame raw_len exceeds MAX_FRAME_RAW_BYTES",
            });
        }
        let codec = Codec::from_u8(buf[16])?;
        let flags = buf[17];
        if flags & !PACK_FRAME_FLAG_USES_DICT != 0 {
            return Err(StatamError::Corrupt {
                reason: "unknown pack frame entry flags",
            });
        }
        let dict_id = read_u16_le(buf, 18);
        if (flags & PACK_FRAME_FLAG_USES_DICT) == 0 && dict_id != 0 {
            return Err(StatamError::Corrupt {
                reason: "non-zero dict_id when PACK_FRAME_FLAG_USES_DICT is clear",
            });
        }
        Ok(Self {
            offset: read_u64_le(buf, 0),
            stored_len,
            raw_len,
            codec,
            flags,
            dict_id,
            first_chunk_id: read_u32_le(buf, 20),
            chunk_count: read_u32_le(buf, 24),
            filter_off: read_u32_le(buf, 28),
            filter_len: read_u32_le(buf, 32),
            crc32c: read_u32_le(buf, 36),
        })
    }
}

/// 12-byte `.stpk` Chunk Location Entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkLoc {
    /// Owning frame index (`0..frame_count`).
    pub frame_id: u32,
    /// Byte offset inside the decompressed frame buffer.
    pub offset_in_frame: u32,
    /// Uncompressed chunk byte length.
    pub raw_len: u32,
}

impl ChunkLoc {
    /// Encodes a 12-byte [`ChunkLoc`].
    pub fn encode(&self) -> [u8; CHUNK_LOC_LEN] {
        let mut buf = [0u8; CHUNK_LOC_LEN];
        buf[0..4].copy_from_slice(&self.frame_id.to_le_bytes());
        buf[4..8].copy_from_slice(&self.offset_in_frame.to_le_bytes());
        buf[8..12].copy_from_slice(&self.raw_len.to_le_bytes());
        buf
    }

    /// Decodes a 12-byte [`ChunkLoc`].
    pub fn decode(buf: &[u8]) -> Result<Self, StatamError> {
        if buf.len() != CHUNK_LOC_LEN {
            return Err(StatamError::Corrupt {
                reason: "invalid chunk loc entry length",
            });
        }
        let raw_len = read_u32_le(buf, 8);
        if (raw_len as usize) > MAX_FRAME_RAW_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "chunk raw_len exceeds MAX_FRAME_RAW_BYTES",
            });
        }
        Ok(Self {
            frame_id: read_u32_le(buf, 0),
            offset_in_frame: read_u32_le(buf, 4),
            raw_len,
        })
    }
}

/// 24-byte Lazy Index Directory Entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LazyIndexEntry {
    /// Entry kind (`Bloom`, `Dict`, or `NarrowTrigram`).
    pub kind: LazyIndexKind,
    /// Reserved (`[0; 3]`).
    pub reserved: [u8; 3],
    /// Physical column ID owning this lazy section.
    pub phys_col_id: u32,
    /// File-absolute offset of the lazy section.
    pub offset: u64,
    /// Byte length of the lazy section.
    pub len: u32,
    /// CRC-32C of the lazy section bytes.
    pub crc32c: u32,
}

impl LazyIndexEntry {
    /// Encodes a 24-byte [`LazyIndexEntry`].
    pub fn encode(&self) -> [u8; LAZY_INDEX_ENTRY_LEN] {
        let mut buf = [0u8; LAZY_INDEX_ENTRY_LEN];
        buf[0] = self.kind as u8;
        buf[1..4].copy_from_slice(&self.reserved);
        buf[4..8].copy_from_slice(&self.phys_col_id.to_le_bytes());
        buf[8..16].copy_from_slice(&self.offset.to_le_bytes());
        buf[16..20].copy_from_slice(&self.len.to_le_bytes());
        buf[20..24].copy_from_slice(&self.crc32c.to_le_bytes());
        buf
    }

    /// Decodes a 24-byte [`LazyIndexEntry`].
    pub fn decode(buf: &[u8]) -> Result<Self, StatamError> {
        if buf.len() != LAZY_INDEX_ENTRY_LEN {
            return Err(StatamError::Corrupt {
                reason: "invalid lazy index entry length",
            });
        }
        let kind = LazyIndexKind::from_u8(buf[0])?;
        let mut reserved = [0u8; 3];
        reserved.copy_from_slice(&buf[1..4]);
        if reserved != [0; 3] {
            return Err(StatamError::Corrupt {
                reason: "non-zero reserved bytes in lazy index entry",
            });
        }
        let len = read_u32_le(buf, 16);
        let max_len = match kind {
            LazyIndexKind::Bloom => MAX_BLOOM_BYTES,
            LazyIndexKind::Dict => MAX_DICT_BYTES,
            LazyIndexKind::NarrowTrigram => MAX_BLOCK_RAW_BYTES,
        };
        if (len as usize) > max_len {
            return Err(StatamError::BoundsExceeded {
                reason: "lazy index entry length exceeds bound",
            });
        }
        Ok(Self {
            kind,
            reserved,
            phys_col_id: read_u32_le(buf, 4),
            offset: read_u64_le(buf, 8),
            len,
            crc32c: read_u32_le(buf, 20),
        })
    }
}

/// One promoted key entry in a [`ShreddedMapDir`].
#[derive(Clone, PartialEq, Eq)]
pub struct ShreddedMapKeyEntry {
    /// Promoted map key name.
    pub key: String,
    /// Physical column index storing this key's sub-column.
    pub sub_col_idx: u16,
    /// Number of rows where this key spilled to residual (e.g. duplicate keys or late RG).
    pub residual_count: u32,
}

impl fmt::Debug for ShreddedMapKeyEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShreddedMapKeyEntry")
            .field("key_len", &self.key.len())
            .field("sub_col_idx", &self.sub_col_idx)
            .field("residual_count", &self.residual_count)
            .finish()
    }
}

/// Shredded map metadata for one logical `MapUtf8` column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShreddedMapDir {
    /// Logical column index of the `MapUtf8` column.
    pub logical_col_idx: u16,
    /// Promoted keys (`<= MAX_SHREDDED_KEYS`).
    pub keys: Vec<ShreddedMapKeyEntry>,
}

/// Complete decoded Schema & Table Metadata section (`schema_off..schema_off + schema_len`).
#[derive(Clone, PartialEq, Eq)]
pub struct FileSchemaMeta {
    /// Table kind (`Generic`, `Spans`, `Traces`, `Scores`).
    pub table_kind: TableKind,
    /// Logical column indices forming the sort key.
    pub sort_cols: Vec<u16>,
    /// External `.stpk` pack IDs referenced by this file.
    pub pack_ids: Vec<[u8; 16]>,
    /// Logical `cetra-column` schema.
    pub schema: Arc<Schema>,
    /// Locality group for each logical field (same length as `schema.fields()`).
    pub locality_groups: Vec<LocalityGroup>,
    /// Shredded map metadata for promoted `MapUtf8` columns.
    pub shredded_maps: Vec<ShreddedMapDir>,
}

impl fmt::Debug for FileSchemaMeta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileSchemaMeta")
            .field("table_kind", &self.table_kind)
            .field("sort_cols_len", &self.sort_cols.len())
            .field("pack_ids_len", &self.pack_ids.len())
            .field("fields_len", &self.schema.fields().len())
            .field("locality_groups_len", &self.locality_groups.len())
            .field("shredded_maps_len", &self.shredded_maps.len())
            .finish()
    }
}

/// Encodes [`FileSchemaMeta`] into the Hot Tail Schema section wire format.
pub fn encode_schema(meta: &FileSchemaMeta) -> Result<Vec<u8>, StatamError> {
    let fields = meta.schema.fields();
    if fields.len() > MAX_COLUMNS {
        return Err(StatamError::BoundsExceeded {
            reason: "schema column count exceeds MAX_COLUMNS",
        });
    }
    if meta.locality_groups.len() != fields.len() {
        return Err(StatamError::SchemaMismatch {
            reason: "locality_groups length does not match schema field count",
        });
    }
    if meta.sort_cols.len() > u8::MAX as usize || meta.sort_cols.len() > fields.len() {
        return Err(StatamError::SchemaMismatch {
            reason: "invalid sort_cols count",
        });
    }
    let mut seen_sort = HashSet::with_capacity(meta.sort_cols.len());
    for &sc in &meta.sort_cols {
        if (sc as usize) >= fields.len() {
            return Err(StatamError::SchemaMismatch {
                reason: "sort column index out of bounds",
            });
        }
        if !seen_sort.insert(sc) {
            return Err(StatamError::SchemaMismatch {
                reason: "duplicate sort column index",
            });
        }
    }
    if meta.pack_ids.len() > MAX_PACK_REFS {
        return Err(StatamError::BoundsExceeded {
            reason: "pack_ids exceeds MAX_PACK_REFS",
        });
    }
    if meta.shredded_maps.len() > MAX_COLUMNS {
        return Err(StatamError::BoundsExceeded {
            reason: "shredded_maps exceeds MAX_COLUMNS",
        });
    }

    let mut out = Vec::with_capacity(256);
    out.push(meta.table_kind as u8);
    out.push(meta.sort_cols.len() as u8);
    for &col in &meta.sort_cols {
        out.extend_from_slice(&col.to_le_bytes());
    }
    out.extend_from_slice(&(meta.pack_ids.len() as u16).to_le_bytes());
    for pack_id in &meta.pack_ids {
        out.extend_from_slice(pack_id);
    }
    out.extend_from_slice(&(fields.len() as u16).to_le_bytes());
    for (field, &lg) in fields.iter().zip(meta.locality_groups.iter()) {
        field.data_type.validate(MAX_NESTING_DEPTH)?;
        let name_bytes = field.name.as_bytes();
        let name_len =
            u16::try_from(name_bytes.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "field name exceeds u16::MAX bytes",
            })?;
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(name_bytes);
        encode_data_type(&field.data_type, &mut out, MAX_NESTING_DEPTH)?;
        out.push(u8::from(field.nullable));
        out.push(lg as u8);
    }

    out.extend_from_slice(&(meta.shredded_maps.len() as u16).to_le_bytes());
    let mut seen_shredded_cols = HashSet::with_capacity(meta.shredded_maps.len());
    let mut seen_sub_cols = HashSet::new();
    for sm in &meta.shredded_maps {
        let Some(field) = fields.get(sm.logical_col_idx as usize) else {
            return Err(StatamError::SchemaMismatch {
                reason: "shredded map logical_col_idx out of bounds",
            });
        };
        if !seen_shredded_cols.insert(sm.logical_col_idx) {
            return Err(StatamError::SchemaMismatch {
                reason: "duplicate shredded map logical_col_idx",
            });
        }
        if field.data_type != DataType::MapUtf8 {
            return Err(StatamError::SchemaMismatch {
                reason: "shredded map logical column is not MapUtf8",
            });
        }
        if sm.keys.len() > MAX_SHREDDED_KEYS {
            return Err(StatamError::BoundsExceeded {
                reason: "shredded map key count exceeds MAX_SHREDDED_KEYS",
            });
        }
        out.extend_from_slice(&sm.logical_col_idx.to_le_bytes());
        out.extend_from_slice(&(sm.keys.len() as u16).to_le_bytes());
        let mut seen_keys = HashSet::with_capacity(sm.keys.len());
        for k in &sm.keys {
            if !seen_keys.insert(k.key.as_str()) {
                return Err(StatamError::SchemaMismatch {
                    reason: "duplicate promoted shredded map key",
                });
            }
            if !seen_sub_cols.insert(k.sub_col_idx) {
                return Err(StatamError::SchemaMismatch {
                    reason: "duplicate shredded map sub_col_idx",
                });
            }
            let kb = k.key.as_bytes();
            let klen = u16::try_from(kb.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "shredded key name exceeds u16::MAX bytes",
            })?;
            out.extend_from_slice(&klen.to_le_bytes());
            out.extend_from_slice(kb);
            out.extend_from_slice(&k.sub_col_idx.to_le_bytes());
            out.extend_from_slice(&k.residual_count.to_le_bytes());
        }
    }

    Ok(out)
}

/// Decodes [`FileSchemaMeta`] from a Hot Tail Schema section slice.
pub fn decode_schema(bytes: &[u8]) -> Result<FileSchemaMeta, StatamError> {
    let mut cur = Cursor::new(bytes);

    let table_kind = TableKind::from_u8(cur.read_u8()?)?;
    let sort_col_count = cur.read_u8()? as usize;
    cur.require_bytes(sort_col_count.saturating_mul(2), "truncated sort_cols")?;
    let mut sort_cols = Vec::with_capacity(sort_col_count);
    for _ in 0..sort_col_count {
        sort_cols.push(cur.read_u16()?);
    }

    let pack_ref_count = cur.read_u16()? as usize;
    if pack_ref_count > MAX_PACK_REFS {
        return Err(StatamError::BoundsExceeded {
            reason: "pack_ref_count exceeds MAX_PACK_REFS",
        });
    }
    let pack_bytes = pack_ref_count
        .checked_mul(16)
        .ok_or(StatamError::BoundsExceeded {
            reason: "pack_ids byte size overflow",
        })?;
    cur.require_bytes(pack_bytes, "truncated pack_ids")?;
    let mut pack_ids = Vec::with_capacity(pack_ref_count);
    for _ in 0..pack_ref_count {
        let mut id = [0u8; 16];
        id.copy_from_slice(cur.read_exact(16)?);
        pack_ids.push(id);
    }

    let field_count = cur.read_u16()? as usize;
    if field_count > MAX_COLUMNS {
        return Err(StatamError::BoundsExceeded {
            reason: "logical_field_count exceeds MAX_COLUMNS",
        });
    }
    // Each field occupies at least 6 bytes (name_len:2 + name>=1 + tag:1 + nullable:1 + lg:1).
    let min_field_bytes = field_count
        .checked_mul(6)
        .ok_or(StatamError::BoundsExceeded {
            reason: "field table size overflow",
        })?;
    cur.require_bytes(min_field_bytes, "truncated schema fields")?;

    let mut fields = Vec::with_capacity(field_count);
    let mut locality_groups = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        let name_len = cur.read_u16()? as usize;
        let name_slice = cur.read_exact(name_len)?;
        let name = std::str::from_utf8(name_slice)
            .map_err(|_| StatamError::Corrupt {
                reason: "invalid utf-8 field name in schema",
            })?
            .to_owned();
        let data_type = decode_data_type(&mut cur, MAX_NESTING_DEPTH)?;
        data_type.validate(MAX_NESTING_DEPTH)?;
        let nullable = match cur.read_u8()? {
            0 => false,
            1 => true,
            _ => {
                return Err(StatamError::Corrupt {
                    reason: "invalid nullable byte in schema",
                });
            }
        };
        let lg = LocalityGroup::from_u8(cur.read_u8()?)?;
        fields.push(Field::new(name, data_type, nullable));
        locality_groups.push(lg);
    }

    let mut seen_sort = HashSet::with_capacity(sort_cols.len());
    for &sc in &sort_cols {
        if (sc as usize) >= fields.len() {
            return Err(StatamError::SchemaMismatch {
                reason: "sort column index out of bounds",
            });
        }
        if !seen_sort.insert(sc) {
            return Err(StatamError::Corrupt {
                reason: "duplicate sort column index in schema",
            });
        }
    }

    let schema = Schema::try_new_arc(fields)?;

    let shredded_map_count = cur.read_u16()? as usize;
    if shredded_map_count > MAX_COLUMNS {
        return Err(StatamError::BoundsExceeded {
            reason: "shredded_map_count exceeds MAX_COLUMNS",
        });
    }
    // Each shredded map header occupies at least 4 bytes (logical_col_idx:2 + key_count:2).
    let min_sm_bytes = shredded_map_count
        .checked_mul(4)
        .ok_or(StatamError::BoundsExceeded {
            reason: "shredded map table size overflow",
        })?;
    cur.require_bytes(min_sm_bytes, "truncated shredded maps")?;

    let mut shredded_maps = Vec::with_capacity(shredded_map_count);
    let mut seen_shredded_cols = HashSet::with_capacity(shredded_map_count);
    let mut seen_sub_cols = HashSet::new();
    for _ in 0..shredded_map_count {
        let logical_col_idx = cur.read_u16()?;
        let key_count = cur.read_u16()? as usize;
        if key_count > MAX_SHREDDED_KEYS {
            return Err(StatamError::BoundsExceeded {
                reason: "shredded map key_count exceeds MAX_SHREDDED_KEYS",
            });
        }
        let Some(field) = schema.fields().get(logical_col_idx as usize) else {
            return Err(StatamError::SchemaMismatch {
                reason: "shredded map logical_col_idx out of bounds",
            });
        };
        if !seen_shredded_cols.insert(logical_col_idx) {
            return Err(StatamError::Corrupt {
                reason: "duplicate shredded map logical_col_idx in schema",
            });
        }
        if field.data_type != DataType::MapUtf8 {
            return Err(StatamError::SchemaMismatch {
                reason: "shredded map logical column is not MapUtf8",
            });
        }
        // Each key entry is at least 8 bytes (key_len:2 + sub_col_idx:2 + residual_count:4).
        let min_key_bytes = key_count
            .checked_mul(8)
            .ok_or(StatamError::BoundsExceeded {
                reason: "shredded map key table size overflow",
            })?;
        cur.require_bytes(min_key_bytes, "truncated shredded map keys")?;

        let mut keys = Vec::with_capacity(key_count);
        let mut seen_keys = HashSet::with_capacity(key_count);
        for _ in 0..key_count {
            let key_len = cur.read_u16()? as usize;
            let key_slice = cur.read_exact(key_len)?;
            let key = std::str::from_utf8(key_slice)
                .map_err(|_| StatamError::Corrupt {
                    reason: "invalid utf-8 shredded key name",
                })?
                .to_owned();
            if !seen_keys.insert(key.clone()) {
                return Err(StatamError::Corrupt {
                    reason: "duplicate promoted shredded map key in schema",
                });
            }
            let sub_col_idx = cur.read_u16()?;
            if !seen_sub_cols.insert(sub_col_idx) {
                return Err(StatamError::Corrupt {
                    reason: "duplicate shredded map sub_col_idx in schema",
                });
            }
            let residual_count = cur.read_u32()?;
            keys.push(ShreddedMapKeyEntry {
                key,
                sub_col_idx,
                residual_count,
            });
        }
        shredded_maps.push(ShreddedMapDir {
            logical_col_idx,
            keys,
        });
    }

    if !cur.is_eof() {
        return Err(StatamError::Corrupt {
            reason: "trailing bytes after schema section",
        });
    }

    Ok(FileSchemaMeta {
        table_kind,
        sort_cols,
        pack_ids,
        schema,
        locality_groups,
        shredded_maps,
    })
}

fn encode_data_type(
    dt: &DataType,
    out: &mut Vec<u8>,
    remaining_depth: usize,
) -> Result<(), StatamError> {
    match dt {
        DataType::Null => out.push(0),
        DataType::Bool => out.push(1),
        DataType::I64 => out.push(2),
        DataType::F64 => out.push(3),
        DataType::Decimal128 { precision, scale } => {
            out.push(4);
            out.push(*precision);
            out.push(*scale as u8);
        }
        DataType::TimestampNs { utc } => {
            out.push(5);
            out.push(u8::from(*utc));
        }
        DataType::Date => out.push(6),
        DataType::Interval => out.push(7),
        DataType::Utf8 => out.push(8),
        DataType::Binary => out.push(9),
        DataType::FixedBytes8 => out.push(10),
        DataType::FixedBytes16 => out.push(11),
        DataType::List(inner) => {
            if remaining_depth == 0 {
                return Err(StatamError::BoundsExceeded {
                    reason: "DataType::List nesting exceeds MAX_NESTING_DEPTH",
                });
            }
            out.push(12);
            encode_data_type(inner, out, remaining_depth - 1)?;
        }
        DataType::MapUtf8 => out.push(13),
        DataType::Events => out.push(14),
        DataType::Links => out.push(15),
    }
    Ok(())
}

fn decode_data_type(cur: &mut Cursor<'_>, remaining_depth: usize) -> Result<DataType, StatamError> {
    let tag = cur.read_u8()?;
    match tag {
        0 => Ok(DataType::Null),
        1 => Ok(DataType::Bool),
        2 => Ok(DataType::I64),
        3 => Ok(DataType::F64),
        4 => {
            let precision = cur.read_u8()?;
            let scale = cur.read_u8()? as i8;
            Ok(DataType::Decimal128 { precision, scale })
        }
        5 => {
            let utc = match cur.read_u8()? {
                0 => false,
                1 => true,
                _ => {
                    return Err(StatamError::Corrupt {
                        reason: "invalid TimestampNs utc flag",
                    });
                }
            };
            Ok(DataType::TimestampNs { utc })
        }
        6 => Ok(DataType::Date),
        7 => Ok(DataType::Interval),
        8 => Ok(DataType::Utf8),
        9 => Ok(DataType::Binary),
        10 => Ok(DataType::FixedBytes8),
        11 => Ok(DataType::FixedBytes16),
        12 => {
            if remaining_depth == 0 {
                return Err(StatamError::BoundsExceeded {
                    reason: "DataType::List nesting exceeds MAX_NESTING_DEPTH",
                });
            }
            let inner = decode_data_type(cur, remaining_depth - 1)?;
            Ok(DataType::List(Box::new(inner)))
        }
        13 => Ok(DataType::MapUtf8),
        14 => Ok(DataType::Events),
        15 => Ok(DataType::Links),
        _ => Err(StatamError::Corrupt {
            reason: "unknown DataType tag in schema",
        }),
    }
}

/// Compresses `raw` using `codec` only if `compressed.len() + 16 < raw.len()`,
/// the expansion ratio fits within `MAX_DECOMPRESS_RATIO * stored + MAX_DECOMPRESS_SLACK`,
/// and (when `min_savings_pct > 0 && raw.len() >= 1024`) the compressed payload saves at
/// least `min_savings_pct` percent of `raw.len()`.
///
/// Otherwise falls back to `(Codec::None, raw.to_vec())`.
pub fn compress_guarded_min_savings(
    raw: &[u8],
    codec: Codec,
    zstd_level: i32,
    min_savings_pct: u8,
) -> Result<(Codec, Vec<u8>), StatamError> {
    if raw.is_empty() {
        return Ok((Codec::None, Vec::new()));
    }
    let compressed = match codec {
        Codec::None => return Ok((Codec::None, raw.to_vec())),
        Codec::Lz4 => lz4_flex::block::compress(raw),
        Codec::Zstd => compress_zstd(raw, zstd_level)?,
    };
    let ratio_ok = (raw.len() as u64)
        <= (compressed.len() as u64)
            .saturating_mul(MAX_DECOMPRESS_RATIO)
            .saturating_add(MAX_DECOMPRESS_SLACK);
    let savings_ok = if min_savings_pct > 0 && raw.len() >= 1024 {
        let keep_pct = u64::from(100u8.saturating_sub(min_savings_pct));
        (compressed.len() as u64).saturating_mul(100) <= (raw.len() as u64).saturating_mul(keep_pct)
    } else {
        true
    };
    if compressed.len().saturating_add(16) < raw.len() && ratio_ok && savings_ok {
        Ok((codec, compressed))
    } else {
        Ok((Codec::None, raw.to_vec()))
    }
}

/// Compresses `raw` using `codec` only if `compressed.len() + 16 < raw.len()` and
/// the expansion ratio fits within `MAX_DECOMPRESS_RATIO * stored + MAX_DECOMPRESS_SLACK`.
///
/// Otherwise falls back to `(Codec::None, raw.to_vec())`.
pub fn compress_guarded(
    raw: &[u8],
    codec: Codec,
    zstd_level: i32,
) -> Result<(Codec, Vec<u8>), StatamError> {
    compress_guarded_min_savings(raw, codec, zstd_level, 0)
}

/// Decompresses `stored` into `raw_len` bytes after enforcing `max_raw` and the
/// `MAX_DECOMPRESS_RATIO * stored + MAX_DECOMPRESS_SLACK` expansion ratio guard BEFORE allocating.
pub fn decompress_checked(
    stored: &[u8],
    codec: Codec,
    raw_len: usize,
    max_raw: usize,
) -> Result<Vec<u8>, StatamError> {
    check_decompress_bounds(raw_len, stored.len(), max_raw)?;
    match codec {
        Codec::None => {
            if stored.len() != raw_len {
                return Err(StatamError::Corrupt {
                    reason: "uncompressed payload length mismatch",
                });
            }
            Ok(stored.to_vec())
        }
        Codec::Lz4 => {
            let out =
                lz4_flex::block::decompress(stored, raw_len).map_err(|_| StatamError::Corrupt {
                    reason: "lz4 block decompression failed",
                })?;
            if out.len() != raw_len {
                return Err(StatamError::Corrupt {
                    reason: "lz4 decompressed length mismatch",
                });
            }
            Ok(out)
        }
        Codec::Zstd => {
            let out = decompress_zstd(stored, raw_len)?;
            if out.len() != raw_len {
                return Err(StatamError::Corrupt {
                    reason: "zstd decompressed length mismatch",
                });
            }
            Ok(out)
        }
    }
}

/// Checks decompression size and zip-bomb ratio bounds before allocating.
pub fn check_decompress_bounds(
    raw_len: usize,
    stored_len: usize,
    max_raw: usize,
) -> Result<(), StatamError> {
    if raw_len > max_raw {
        return Err(StatamError::BoundsExceeded {
            reason: "decompressed length exceeds maximum allowed",
        });
    }
    let max_by_ratio = (stored_len as u64)
        .saturating_mul(MAX_DECOMPRESS_RATIO)
        .saturating_add(MAX_DECOMPRESS_SLACK);
    if (raw_len as u64) > max_by_ratio {
        return Err(StatamError::BoundsExceeded {
            reason: "decompressed ratio exceeds MAX_DECOMPRESS_RATIO + slack",
        });
    }
    Ok(())
}

#[cfg(not(miri))]
fn compress_zstd(raw: &[u8], level: i32) -> Result<Vec<u8>, StatamError> {
    use std::cell::RefCell;
    thread_local! {
        static COMP: RefCell<Option<(i32, zstd::bulk::Compressor<'static>)>> =
            const { RefCell::new(None) };
    }
    COMP.with(|cell| {
        let mut guard = cell.borrow_mut();
        let needs_new = match guard.as_ref() {
            Some((cached_lvl, _)) => *cached_lvl != level,
            None => true,
        };
        if needs_new {
            let comp = zstd::bulk::Compressor::new(level).map_err(|_| StatamError::Corrupt {
                reason: "zstd compressor init failed",
            })?;
            *guard = Some((level, comp));
        }
        let (_, comp) = guard.as_mut().ok_or(StatamError::Corrupt {
            reason: "zstd compressor missing",
        })?;
        comp.compress(raw).map_err(|_| StatamError::Corrupt {
            reason: "zstd compression failed",
        })
    })
}

#[cfg(miri)]
fn compress_zstd(_raw: &[u8], _level: i32) -> Result<Vec<u8>, StatamError> {
    Err(StatamError::UnsupportedFeature { mask: 0x02 })
}

#[cfg(not(miri))]
fn decompress_zstd(stored: &[u8], raw_len: usize) -> Result<Vec<u8>, StatamError> {
    use std::cell::RefCell;
    thread_local! {
        static DECOMP: RefCell<Option<zstd::bulk::Decompressor<'static>>> =
            const { RefCell::new(None) };
    }
    DECOMP.with(|cell| {
        let mut guard = cell.borrow_mut();
        if guard.is_none() {
            let decomp = zstd::bulk::Decompressor::new().map_err(|_| StatamError::Corrupt {
                reason: "zstd decompressor init failed",
            })?;
            *guard = Some(decomp);
        }
        let decomp = guard.as_mut().ok_or(StatamError::Corrupt {
            reason: "zstd decompressor missing",
        })?;
        decomp
            .decompress(stored, raw_len)
            .map_err(|_| StatamError::Corrupt {
                reason: "zstd decompression failed",
            })
    })
}

#[cfg(miri)]
fn decompress_zstd(_stored: &[u8], _raw_len: usize) -> Result<Vec<u8>, StatamError> {
    Err(StatamError::UnsupportedFeature { mask: 0x02 })
}

/// Prepared Zstd bulk compressor bound to a trained `.stpk` dictionary.
pub struct PackDictCompressor {
    #[cfg(not(miri))]
    inner: zstd::bulk::Compressor<'static>,
    dict_len: usize,
}

impl fmt::Debug for PackDictCompressor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PackDictCompressor")
            .field("dict_len", &self.dict_len)
            .finish()
    }
}

impl PackDictCompressor {
    /// Creates a Zstd bulk compressor initialized with a trained `.stpk` dictionary.
    #[cfg(not(miri))]
    pub fn new(zstd_level: i32, dict_bytes: &[u8]) -> Result<Self, StatamError> {
        if dict_bytes.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "empty pack dictionary",
            });
        }
        if dict_bytes.len() > MAX_PACK_DICT_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "pack dictionary exceeds MAX_PACK_DICT_BYTES",
            });
        }
        let inner =
            zstd::bulk::Compressor::with_dictionary(zstd_level, dict_bytes).map_err(|_| {
                StatamError::Corrupt {
                    reason: "zstd dict compressor init failed",
                }
            })?;
        Ok(Self {
            inner,
            dict_len: dict_bytes.len(),
        })
    }

    /// Returns `StatamError::UnsupportedFeature` under Miri where Zstd C FFI is disabled.
    #[cfg(miri)]
    pub fn new(_zstd_level: i32, _dict_bytes: &[u8]) -> Result<Self, StatamError> {
        Err(StatamError::UnsupportedFeature { mask: 0x02 })
    }
}

/// Prepared Zstd decoder dictionary for `.stpk` pack frames.
pub struct PackDecoderDict {
    #[cfg(not(miri))]
    inner: zstd::dict::DecoderDictionary<'static>,
    dict_len: usize,
}

impl fmt::Debug for PackDecoderDict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PackDecoderDict")
            .field("dict_len", &self.dict_len)
            .finish()
    }
}

impl PackDecoderDict {
    /// Prepares a Zstd decoder dictionary from raw trained dictionary bytes.
    #[cfg(not(miri))]
    pub fn new(dict_bytes: &[u8]) -> Result<Self, StatamError> {
        if dict_bytes.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "empty pack dictionary",
            });
        }
        if dict_bytes.len() > MAX_PACK_DICT_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "pack dictionary exceeds MAX_PACK_DICT_BYTES",
            });
        }
        let inner = zstd::dict::DecoderDictionary::try_copy(dict_bytes).map_err(|_| {
            StatamError::Corrupt {
                reason: "invalid zstd pack dictionary",
            }
        })?;
        Ok(Self {
            inner,
            dict_len: dict_bytes.len(),
        })
    }

    /// Returns `StatamError::UnsupportedFeature` under Miri where Zstd C FFI is disabled.
    #[cfg(miri)]
    pub fn new(_dict_bytes: &[u8]) -> Result<Self, StatamError> {
        Err(StatamError::UnsupportedFeature { mask: 0x02 })
    }
}

/// Reusable per-worker Zstd dictionary decompressor for `.stpk` pack frames.
///
/// Reuses the underlying `ZSTD_DCtx` across frames and switches prepared dictionaries in O(1)
/// via `set_prepared_dictionary` whenever the borrowed [`PackDecoderDict`] pointer changes.
pub(crate) struct PackFrameDecompressor<'a> {
    #[cfg(not(miri))]
    inner: Option<(&'a PackDecoderDict, zstd::bulk::Decompressor<'a>)>,
    #[cfg(miri)]
    _marker: std::marker::PhantomData<&'a PackDecoderDict>,
}

impl fmt::Debug for PackFrameDecompressor<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[cfg(not(miri))]
        let has_dict = self.inner.is_some();
        #[cfg(miri)]
        let has_dict = false;
        f.debug_struct("PackFrameDecompressor")
            .field("has_dict", &has_dict)
            .finish()
    }
}

impl<'a> PackFrameDecompressor<'a> {
    /// Creates an empty reusable dictionary decompressor.
    #[inline]
    pub(crate) fn new() -> Self {
        Self {
            #[cfg(not(miri))]
            inner: None,
            #[cfg(miri)]
            _marker: std::marker::PhantomData,
        }
    }

    /// Decompresses a Zstd dictionary-compressed frame payload using `dict`, reusing or
    /// switching the prepared dictionary on the underlying `ZSTD_DCtx` in O(1).
    #[cfg(not(miri))]
    pub(crate) fn decompress_with_dict(
        &mut self,
        stored: &[u8],
        raw_len: usize,
        dict: &'a PackDecoderDict,
    ) -> Result<Vec<u8>, StatamError> {
        if let Some((prev, decomp)) = &mut self.inner {
            if !std::ptr::eq(*prev, dict) {
                decomp
                    .set_prepared_dictionary(&dict.inner)
                    .map_err(|_| StatamError::Corrupt {
                        reason: "zstd dict decompressor switch failed",
                    })?;
                *prev = dict;
            }
            return decomp
                .decompress(stored, raw_len)
                .map_err(|_| StatamError::Corrupt {
                    reason: "zstd dict decompression failed",
                });
        }
        let mut decomp =
            zstd::bulk::Decompressor::with_prepared_dictionary(&dict.inner).map_err(|_| {
                StatamError::Corrupt {
                    reason: "zstd dict decompressor init failed",
                }
            })?;
        let out = decomp
            .decompress(stored, raw_len)
            .map_err(|_| StatamError::Corrupt {
                reason: "zstd dict decompression failed",
            })?;
        self.inner = Some((dict, decomp));
        Ok(out)
    }

    /// Returns `StatamError::UnsupportedFeature` under Miri where Zstd C FFI is disabled.
    #[cfg(miri)]
    pub(crate) fn decompress_with_dict(
        &mut self,
        _stored: &[u8],
        _raw_len: usize,
        _dict: &'a PackDecoderDict,
    ) -> Result<Vec<u8>, StatamError> {
        Err(StatamError::UnsupportedFeature { mask: 0x02 })
    }
}

/// Metadata returned when writing a framed block or pack frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramedWriteInfo {
    /// Actual codec chosen after the incompressibility guard.
    pub codec: Codec,
    /// Stored payload byte length (excluding the 16-byte header).
    pub stored_len: u32,
    /// Uncompressed payload byte length.
    pub raw_len: u32,
    /// Header-inclusive CRC-32C over the 12-byte header prefix and stored payload bytes.
    pub crc32c: u32,
}

/// Computes the header-inclusive CRC-32C for a `.statam` `SB` data block frame.
#[inline]
pub(crate) fn block_frame_crc(header_prefix_12: &[u8], stored: &[u8]) -> u32 {
    crc32c::crc32c_append(crc32c::crc32c(header_prefix_12), stored)
}

/// Computes the header-inclusive CRC-32C for a `.stpk` `PF` pack frame.
#[inline]
pub(crate) fn pack_frame_crc(header_prefix_12: &[u8], stored: &[u8]) -> u32 {
    crc32c::crc32c_append(crc32c::crc32c(header_prefix_12), stored)
}

/// Writes a 16-byte `SB` block header followed by the guarded compressed payload,
/// requiring at least `min_savings_pct` compression savings when `min_savings_pct > 0 && raw.len() >= 1024`.
pub fn write_framed_block_with_min_savings(
    encoding: Encoding,
    raw: &[u8],
    codec: Codec,
    zstd_level: i32,
    min_savings_pct: u8,
    out: &mut Vec<u8>,
) -> Result<FramedWriteInfo, StatamError> {
    if raw.len() > MAX_BLOCK_RAW_BYTES {
        return Err(StatamError::BoundsExceeded {
            reason: "block raw payload exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    let raw_len = u32::try_from(raw.len()).map_err(|_| StatamError::BoundsExceeded {
        reason: "block raw_len overflows u32",
    })?;
    let (actual_codec, stored) =
        compress_guarded_min_savings(raw, codec, zstd_level, min_savings_pct)?;
    let stored_len = u32::try_from(stored.len()).map_err(|_| StatamError::BoundsExceeded {
        reason: "block stored_len overflows u32",
    })?;
    let mut header_prefix = [0u8; 12];
    header_prefix[0..2].copy_from_slice(&BLOCK_MAGIC);
    header_prefix[2] = encoding as u8;
    header_prefix[3] = actual_codec as u8;
    header_prefix[4..8].copy_from_slice(&raw_len.to_le_bytes());
    header_prefix[8..12].copy_from_slice(&stored_len.to_le_bytes());
    let crc = block_frame_crc(&header_prefix, &stored);

    out.reserve(BLOCK_FRAME_HEADER_LEN + stored.len());
    out.extend_from_slice(&header_prefix);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&stored);

    Ok(FramedWriteInfo {
        codec: actual_codec,
        stored_len,
        raw_len,
        crc32c: crc,
    })
}

/// Writes a 16-byte `SB` block header followed by the guarded compressed payload.
pub fn write_framed_block(
    encoding: Encoding,
    raw: &[u8],
    codec: Codec,
    zstd_level: i32,
    out: &mut Vec<u8>,
) -> Result<FramedWriteInfo, StatamError> {
    write_framed_block_with_min_savings(encoding, raw, codec, zstd_level, 0, out)
}

/// Reads, checksum-verifies, and decompresses a framed `.statam` `SB` data block,
/// returning `Cow::Borrowed` without copying when `codec == Codec::None`.
pub fn read_framed_block_cow<'a>(
    frame: &'a [u8],
    max_raw: usize,
) -> Result<(Encoding, Codec, Cow<'a, [u8]>), StatamError> {
    if frame.len() < BLOCK_FRAME_HEADER_LEN {
        return Err(StatamError::Corrupt {
            reason: "truncated block frame header",
        });
    }
    if frame[0..2] != BLOCK_MAGIC {
        return Err(StatamError::Corrupt {
            reason: "invalid block frame magic",
        });
    }
    let encoding = Encoding::from_u8(frame[2])?;
    let codec = Codec::from_u8(frame[3])?;
    let raw_len = read_u32_le(frame, 4) as usize;
    let stored_len = read_u32_le(frame, 8) as usize;
    let expected_crc = read_u32_le(frame, 12);

    let total_len =
        BLOCK_FRAME_HEADER_LEN
            .checked_add(stored_len)
            .ok_or(StatamError::BoundsExceeded {
                reason: "block frame total length overflow",
            })?;
    if frame.len() != total_len {
        return Err(StatamError::Corrupt {
            reason: "block frame length mismatch",
        });
    }
    let stored = &frame[BLOCK_FRAME_HEADER_LEN..total_len];
    let actual_crc = block_frame_crc(&frame[0..12], stored);
    if actual_crc != expected_crc {
        return Err(StatamError::ChecksumMismatch {
            context: "data block",
        });
    }
    check_decompress_bounds(raw_len, stored.len(), max_raw)?;
    let body = match codec {
        Codec::None => {
            if stored.len() != raw_len {
                return Err(StatamError::Corrupt {
                    reason: "uncompressed payload length mismatch",
                });
            }
            Cow::Borrowed(stored)
        }
        Codec::Lz4 | Codec::Zstd => {
            Cow::Owned(decompress_checked(stored, codec, raw_len, max_raw)?)
        }
    };
    Ok((encoding, codec, body))
}

/// Reads, checksum-verifies, decompresses, and cross-checks a framed `.statam` `SB`
/// data block against its [`BlockEntry`] metadata (`encoding`, `codec`, `raw_len`).
pub(crate) fn read_framed_block_checked_cow<'a>(
    frame: &'a [u8],
    blk: &BlockEntry,
    max_raw: usize,
) -> Result<Cow<'a, [u8]>, StatamError> {
    let (enc, codec, body) = read_framed_block_cow(frame, max_raw)?;
    if enc != blk.encoding || codec != blk.codec || body.len() != blk.raw_len as usize {
        return Err(StatamError::Corrupt {
            reason: "block frame header does not match BlockEntry",
        });
    }
    Ok(body)
}

/// Reads, checksum-verifies, and decompresses a framed `.statam` `SB` data block.
pub fn read_framed_block(
    frame: &[u8],
    max_raw: usize,
) -> Result<(Encoding, Codec, Vec<u8>), StatamError> {
    let (encoding, codec, body) = read_framed_block_cow(frame, max_raw)?;
    Ok((encoding, codec, body.into_owned()))
}

/// Writes a 16-byte `PF` pack frame header followed by the guarded compressed payload,
/// enforcing `max_raw` as the upper bound on `raw.len()` and requiring at least
/// `min_savings_pct` compression savings when `min_savings_pct > 0 && raw.len() >= 1024`.
pub fn write_framed_pack_frame_with_limit_min_savings(
    raw: &[u8],
    codec: Codec,
    flags: u8,
    zstd_level: i32,
    max_raw: usize,
    min_savings_pct: u8,
    out: &mut Vec<u8>,
) -> Result<FramedWriteInfo, StatamError> {
    if flags != 0 {
        return Err(StatamError::Corrupt {
            reason: "pack frame flags invalid without dictionary compressor",
        });
    }
    if raw.len() > max_raw {
        return Err(StatamError::BoundsExceeded {
            reason: "pack frame raw payload exceeds max_raw",
        });
    }
    let raw_len = u32::try_from(raw.len()).map_err(|_| StatamError::BoundsExceeded {
        reason: "pack frame raw_len overflows u32",
    })?;
    let (actual_codec, stored) =
        compress_guarded_min_savings(raw, codec, zstd_level, min_savings_pct)?;
    emit_pack_frame_bytes(raw_len, actual_codec, flags, &stored, out)
}

/// Writes a 16-byte `PF` pack frame header followed by the guarded compressed payload,
/// enforcing `max_raw` as the upper bound on `raw.len()`.
pub fn write_framed_pack_frame_with_limit(
    raw: &[u8],
    codec: Codec,
    flags: u8,
    zstd_level: i32,
    max_raw: usize,
    out: &mut Vec<u8>,
) -> Result<FramedWriteInfo, StatamError> {
    write_framed_pack_frame_with_limit_min_savings(raw, codec, flags, zstd_level, max_raw, 0, out)
}

/// Writes a 16-byte `PF` pack frame header followed by the guarded compressed payload,
/// requiring at least `min_savings_pct` compression savings when `min_savings_pct > 0 && raw.len() >= 1024`.
pub fn write_framed_pack_frame_with_min_savings(
    raw: &[u8],
    codec: Codec,
    flags: u8,
    zstd_level: i32,
    min_savings_pct: u8,
    out: &mut Vec<u8>,
) -> Result<FramedWriteInfo, StatamError> {
    write_framed_pack_frame_with_limit_min_savings(
        raw,
        codec,
        flags,
        zstd_level,
        MAX_FRAME_RAW_BYTES,
        min_savings_pct,
        out,
    )
}

/// Writes a 16-byte `PF` pack frame header followed by the guarded compressed payload.
pub fn write_framed_pack_frame(
    raw: &[u8],
    codec: Codec,
    flags: u8,
    zstd_level: i32,
    out: &mut Vec<u8>,
) -> Result<FramedWriteInfo, StatamError> {
    write_framed_pack_frame_with_limit(raw, codec, flags, zstd_level, MAX_FRAME_RAW_BYTES, out)
}

/// Writes a 16-byte `PF` pack frame header, optionally compressing with `dict_compressor`
/// and requiring at least `min_savings_pct` compression savings when `min_savings_pct > 0 && raw.len() >= 1024`.
pub fn write_framed_pack_frame_with_compressor_min_savings(
    raw: &[u8],
    codec: Codec,
    flags: u8,
    zstd_level: i32,
    min_savings_pct: u8,
    dict_compressor: Option<&mut PackDictCompressor>,
    out: &mut Vec<u8>,
) -> Result<FramedWriteInfo, StatamError> {
    if flags & !PACK_FRAME_FLAG_USES_DICT != 0 {
        return Err(StatamError::Corrupt {
            reason: "unknown pack frame flags",
        });
    }
    if raw.len() > MAX_FRAME_RAW_BYTES {
        return Err(StatamError::BoundsExceeded {
            reason: "pack frame raw payload exceeds MAX_FRAME_RAW_BYTES",
        });
    }
    #[cfg(not(miri))]
    if codec == Codec::Zstd
        && let Some(comp) = dict_compressor
    {
        let raw_len = u32::try_from(raw.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "pack frame raw_len overflows u32",
        })?;
        let compressed = comp.inner.compress(raw).map_err(|_| StatamError::Corrupt {
            reason: "zstd dict compression failed",
        })?;
        let ratio_ok = (raw.len() as u64)
            <= (compressed.len() as u64)
                .saturating_mul(MAX_DECOMPRESS_RATIO)
                .saturating_add(MAX_DECOMPRESS_SLACK);
        let savings_ok = if min_savings_pct > 0 && raw.len() >= 1024 {
            let keep_pct = u64::from(100u8.saturating_sub(min_savings_pct));
            (compressed.len() as u64).saturating_mul(100)
                <= (raw.len() as u64).saturating_mul(keep_pct)
        } else {
            true
        };
        return if compressed.len().saturating_add(16) < raw.len() && ratio_ok && savings_ok {
            emit_pack_frame_bytes(
                raw_len,
                Codec::Zstd,
                flags | PACK_FRAME_FLAG_USES_DICT,
                &compressed,
                out,
            )
        } else {
            emit_pack_frame_bytes(
                raw_len,
                Codec::None,
                flags & !PACK_FRAME_FLAG_USES_DICT,
                raw,
                out,
            )
        };
    }
    #[cfg(miri)]
    let _ = dict_compressor;

    write_framed_pack_frame_with_min_savings(
        raw,
        codec,
        flags & !PACK_FRAME_FLAG_USES_DICT,
        zstd_level,
        min_savings_pct,
        out,
    )
}

/// Writes a 16-byte `PF` pack frame header, optionally compressing with `dict_compressor`.
///
/// When `dict_compressor` is `Some` and `codec == Codec::Zstd` (non-Miri), compresses `raw`
/// using the dictionary and sets `PACK_FRAME_FLAG_USES_DICT` if smaller than raw (and within
/// the decompression ratio guard); otherwise falls back to `Codec::None` with
/// `PACK_FRAME_FLAG_USES_DICT` cleared.
pub fn write_framed_pack_frame_with_compressor(
    raw: &[u8],
    codec: Codec,
    flags: u8,
    zstd_level: i32,
    dict_compressor: Option<&mut PackDictCompressor>,
    out: &mut Vec<u8>,
) -> Result<FramedWriteInfo, StatamError> {
    write_framed_pack_frame_with_compressor_min_savings(
        raw,
        codec,
        flags,
        zstd_level,
        0,
        dict_compressor,
        out,
    )
}

fn emit_pack_frame_bytes(
    raw_len: u32,
    actual_codec: Codec,
    actual_flags: u8,
    stored: &[u8],
    out: &mut Vec<u8>,
) -> Result<FramedWriteInfo, StatamError> {
    let stored_len = u32::try_from(stored.len()).map_err(|_| StatamError::BoundsExceeded {
        reason: "pack frame stored_len overflows u32",
    })?;
    let mut header_prefix = [0u8; 12];
    header_prefix[0..2].copy_from_slice(&PACK_FRAME_MAGIC);
    header_prefix[2] = actual_codec as u8;
    header_prefix[3] = actual_flags;
    header_prefix[4..8].copy_from_slice(&raw_len.to_le_bytes());
    header_prefix[8..12].copy_from_slice(&stored_len.to_le_bytes());
    let crc = pack_frame_crc(&header_prefix, stored);

    out.reserve(PACK_FRAME_HEADER_LEN + stored.len());
    out.extend_from_slice(&header_prefix);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(stored);

    Ok(FramedWriteInfo {
        codec: actual_codec,
        stored_len,
        raw_len,
        crc32c: crc,
    })
}

fn parse_verified_pack_frame(frame: &[u8]) -> Result<(Codec, u8, usize, &[u8]), StatamError> {
    if frame.len() < PACK_FRAME_HEADER_LEN {
        return Err(StatamError::Corrupt {
            reason: "truncated pack frame header",
        });
    }
    if frame[0..2] != PACK_FRAME_MAGIC {
        return Err(StatamError::Corrupt {
            reason: "invalid pack frame magic",
        });
    }
    let codec = Codec::from_u8(frame[2])?;
    let flags = frame[3];
    if flags & !PACK_FRAME_FLAG_USES_DICT != 0 {
        return Err(StatamError::Corrupt {
            reason: "unknown pack frame flags",
        });
    }
    let raw_len = read_u32_le(frame, 4) as usize;
    let stored_len = read_u32_le(frame, 8) as usize;
    let expected_crc = read_u32_le(frame, 12);

    let total_len =
        PACK_FRAME_HEADER_LEN
            .checked_add(stored_len)
            .ok_or(StatamError::BoundsExceeded {
                reason: "pack frame total length overflow",
            })?;
    if frame.len() != total_len {
        return Err(StatamError::Corrupt {
            reason: "pack frame length mismatch",
        });
    }
    let stored = &frame[PACK_FRAME_HEADER_LEN..total_len];
    let actual_crc = pack_frame_crc(&frame[0..12], stored);
    if actual_crc != expected_crc {
        return Err(StatamError::ChecksumMismatch {
            context: "pack frame",
        });
    }
    Ok((codec, flags, raw_len, stored))
}

/// Reads, checksum-verifies, and decompresses a framed `.stpk` `PF` pack frame.
pub fn read_framed_pack_frame(
    frame: &[u8],
    max_raw: usize,
) -> Result<(Codec, u8, Vec<u8>), StatamError> {
    read_framed_pack_frame_with_dict(frame, max_raw, None)
}

/// Reads, checksum-verifies, and decompresses a framed `.stpk` `PF` pack frame,
/// using `dict` and a reusable [`PackFrameDecompressor`] when `PACK_FRAME_FLAG_USES_DICT` is set.
pub(crate) fn read_framed_pack_frame_with_decompressor<'a>(
    frame: &[u8],
    max_raw: usize,
    dict: Option<&'a PackDecoderDict>,
    dict_decomp: &mut PackFrameDecompressor<'a>,
) -> Result<(Codec, u8, Vec<u8>), StatamError> {
    let (codec, flags, raw_len, stored) = parse_verified_pack_frame(frame)?;
    check_decompress_bounds(raw_len, stored.len(), max_raw)?;
    if (flags & PACK_FRAME_FLAG_USES_DICT) != 0 {
        if codec != Codec::Zstd {
            return Err(StatamError::Corrupt {
                reason: "pack frame dict flag requires zstd codec",
            });
        }
        let dict = dict.ok_or(StatamError::Corrupt {
            reason: "pack frame requires dictionary but none provided",
        })?;
        let decoded = dict_decomp.decompress_with_dict(stored, raw_len, dict)?;
        if decoded.len() != raw_len {
            return Err(StatamError::Corrupt {
                reason: "zstd dict decompressed length mismatch",
            });
        }
        Ok((codec, flags, decoded))
    } else {
        let decompressed = decompress_checked(stored, codec, raw_len, max_raw)?;
        Ok((codec, flags, decompressed))
    }
}

/// Reads, checksum-verifies, and decompresses a framed `.stpk` `PF` pack frame,
/// using `dict` when `PACK_FRAME_FLAG_USES_DICT` is set.
pub fn read_framed_pack_frame_with_dict(
    frame: &[u8],
    max_raw: usize,
    dict: Option<&PackDecoderDict>,
) -> Result<(Codec, u8, Vec<u8>), StatamError> {
    let mut dict_decomp = PackFrameDecompressor::new();
    read_framed_pack_frame_with_decompressor(frame, max_raw, dict, &mut dict_decomp)
}

fn check_slice_range(off: u32, len: u32, limit: usize) -> Result<(), StatamError> {
    check_range_usize(off as usize, len as usize, limit)
}

fn check_range_usize(off: usize, len: usize, limit: usize) -> Result<(), StatamError> {
    let end = off.checked_add(len).ok_or(StatamError::BoundsExceeded {
        reason: "hot tail section offset + length overflow",
    })?;
    if end > limit {
        return Err(StatamError::Corrupt {
            reason: "hot tail section exceeds hot_tail_raw_len",
        });
    }
    Ok(())
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    fn is_eof(&self) -> bool {
        self.pos == self.buf.len()
    }

    fn require_bytes(&self, needed: usize, reason: &'static str) -> Result<(), StatamError> {
        if self.remaining() < needed {
            return Err(StatamError::Corrupt { reason });
        }
        Ok(())
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], StatamError> {
        let end = self.pos.checked_add(len).ok_or(StatamError::Corrupt {
            reason: "schema cursor overflow",
        })?;
        if end > self.buf.len() {
            return Err(StatamError::Corrupt {
                reason: "unexpected end of schema buffer",
            });
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn read_u8(&mut self) -> Result<u8, StatamError> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16, StatamError> {
        let s = self.read_exact(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }

    fn read_u32(&mut self) -> Result<u32, StatamError> {
        let s = self.read_exact(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
}

#[inline]
fn read_u16_le(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

#[inline]
fn read_u32_le(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

#[inline]
fn read_u64_le(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        buf[off],
        buf[off + 1],
        buf[off + 2],
        buf[off + 3],
        buf[off + 4],
        buf[off + 5],
        buf[off + 6],
        buf[off + 7],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_postscript_exact_sizes_and_roundtrip() {
        let fh = FileHeader {
            format_version: FORMAT_VERSION,
            flags: FLAG_L1_COMPACT | FLAG_HOT_TAIL_LZ4,
            table_kind: TableKind::Spans,
            checksum_alg: CHECKSUM_ALG_CRC32C,
            reserved0: 0,
            required_features: 0,
            enc_envelope_offset: 0,
            enc_envelope_len: 0,
        };
        let fh_bytes = fh.encode();
        assert_eq!(fh_bytes.len(), 32);
        assert_eq!(FileHeader::decode(&fh_bytes).unwrap(), fh);

        let ps = Postscript {
            hot_tail_len: 1024,
            hot_tail_raw_len: 2048,
            format_version: FORMAT_VERSION,
            flags: FLAG_L1_COMPACT | FLAG_HOT_TAIL_LZ4,
            required_features: 0,
            row_count: 16_384,
            num_logical_cols: 4,
            num_phys_cols: 6,
            num_row_groups: 1,
            total_blocks: 24,
            schema_off: 0,
            schema_len: 100,
            col_dir_off: 100,
            rg_dir_off: 100 + 6 * 56,
            block_dir_off: 100 + 6 * 56 + 48,
            hot_bloom_off: 0,
            hot_bloom_len: 0,
            hot_dict_off: 0,
            hot_dict_len: 0,
            lazy_dir_off: 100 + 6 * 56 + 48 + 24 * 64,
            lazy_dir_len: 24,
            enc_envelope_len: 0,
            enc_envelope_offset: 0,
            hot_tail_crc32c: 0xdead_beef,
            reserved: 0,
        };
        let ps_bytes = ps.encode();
        assert_eq!(ps_bytes.len(), 112);
        assert_eq!(Postscript::decode(&ps_bytes).unwrap(), ps);

        let ph = PackHeader {
            format_version: FORMAT_VERSION,
            flags: PACK_FLAG_TOC_LZ4 | PACK_FLAG_HAS_TRIGRAM,
            required_features: 0,
            pack_id: [7u8; 16],
            enc_envelope_offset: 0,
            enc_envelope_len: 0,
            reserved: 0,
        };
        let ph_bytes = ph.encode();
        assert_eq!(ph_bytes.len(), 48);
        assert_eq!(PackHeader::decode(&ph_bytes).unwrap(), ph);

        let pps = PackPostscript {
            toc_stored_len: 200,
            toc_raw_len: 2 * FRAME_ENTRY_LEN as u32 + 5 * CHUNK_LOC_LEN as u32,
            format_version: FORMAT_VERSION,
            flags: PACK_FLAG_TOC_LZ4,
            required_features: 0,
            pack_id: [9u8; 16],
            frame_count: 2,
            chunk_count: 5,
            hash_table_off: 4096,
            trigram_off: 4200,
            trigram_len: 512,
            edge_table_len: 128,
            edge_table_off: 4712,
            dict_off: 0,
            dict_len: 0,
            enc_envelope_offset: 0,
            enc_envelope_len: 0,
            reserved: 0,
            toc_crc32c: 0x1234_5678,
        };
        let pps_bytes = pps.encode();
        assert_eq!(pps_bytes.len(), 112);
        assert_eq!(PackPostscript::decode(&pps_bytes).unwrap(), pps);

        let col_dir = ColumnDirEntry {
            phys_col_id: 3,
            logical_col_id: 2,
            locality_group: LocalityGroup::WideRow,
            col_flags: COL_FLAG_HAS_FILE_DICT | COL_FLAG_DICT_SORTED,
            reserved: 0,
            num_blocks: 4,
            first_block_idx: 12,
            dict_offset: 800,
            dict_len: 256,
            dict_entry_count: 10,
            bloom_offset: 1200,
            bloom_len: 64,
            null_count_total: 5,
            trigram_offset: 2000,
        };
        let col_dir_bytes = col_dir.encode();
        assert_eq!(col_dir_bytes.len(), 56);
        assert_eq!(ColumnDirEntry::decode(&col_dir_bytes).unwrap(), col_dir);

        let rg = RowGroupEntry {
            first_row: 0,
            row_count: 16_384,
            core_offset: 32,
            core_len: 4096,
            wide_len: 8192,
            wide_offset: 32 + 4096,
            l0_payload_offset: 32 + 4096 + 8192,
            l0_payload_len: 1024,
            rg_flags: 0,
        };
        let rg_bytes = rg.encode();
        assert_eq!(rg_bytes.len(), 48);
        assert_eq!(RowGroupEntry::decode(&rg_bytes).unwrap(), rg);

        let blk = BlockEntry {
            first_row: 4096,
            row_count: 4096,
            null_count: 12,
            offset: 1024,
            stored_len: 512,
            raw_len: 2048,
            encoding: Encoding::ForBitpack,
            codec: Codec::Lz4,
            block_flags: BLOCK_FLAG_HAS_NULLS | BLOCK_FLAG_MIN_MAX_VALID,
            min_inline_len: 8,
            max_inline_len: 8,
            reserved: [0; 3],
            min_inline: [1; 16],
            max_inline: [9; 16],
        };
        let blk_bytes = blk.encode();
        assert_eq!(blk_bytes.len(), 64);
        assert_eq!(BlockEntry::decode(&blk_bytes).unwrap(), blk);

        let frame = FrameEntry {
            offset: 48,
            stored_len: 16_384,
            raw_len: 65_536,
            codec: Codec::Zstd,
            flags: PACK_FRAME_FLAG_USES_DICT,
            dict_id: 1,
            first_chunk_id: 0,
            chunk_count: 32,
            filter_off: 0,
            filter_len: 1024,
            crc32c: 0xabcd_ef01,
        };
        let frame_bytes = frame.encode();
        assert_eq!(frame_bytes.len(), 40);
        assert_eq!(FrameEntry::decode(&frame_bytes).unwrap(), frame);

        let chunk = ChunkLoc {
            frame_id: 3,
            offset_in_frame: 2048,
            raw_len: 1500,
        };
        let chunk_bytes = chunk.encode();
        assert_eq!(chunk_bytes.len(), 12);
        assert_eq!(ChunkLoc::decode(&chunk_bytes).unwrap(), chunk);

        let lazy = LazyIndexEntry {
            kind: LazyIndexKind::NarrowTrigram,
            reserved: [0; 3],
            phys_col_id: 4,
            offset: 99_000,
            len: 4096,
            crc32c: 0x55aa_1122,
        };
        let lazy_bytes = lazy.encode();
        assert_eq!(lazy_bytes.len(), 24);
        assert_eq!(LazyIndexEntry::decode(&lazy_bytes).unwrap(), lazy);
    }

    #[test]
    fn schema_roundtrip_all_16_data_types_and_shredded_maps() {
        let fields = vec![
            Field::new("c_null", DataType::Null, true),
            Field::new("c_bool", DataType::Bool, false),
            Field::new("c_i64", DataType::I64, false),
            Field::new("c_f64", DataType::F64, true),
            Field::new("c_dec", DataType::cost_decimal(), true),
            Field::new("c_ts_utc", DataType::TimestampNs { utc: true }, false),
            Field::new("c_ts_naive", DataType::TimestampNs { utc: false }, true),
            Field::new("c_date", DataType::Date, false),
            Field::new("c_interval", DataType::Interval, true),
            Field::new("c_utf8", DataType::Utf8, true),
            Field::new("c_bin", DataType::Binary, true),
            Field::new("c_f8", DataType::FixedBytes8, false),
            Field::new("c_f16", DataType::FixedBytes16, false),
            Field::new(
                "c_list",
                DataType::List(Box::new(DataType::List(Box::new(DataType::Utf8)))),
                true,
            ),
            Field::new("c_map", DataType::MapUtf8, true),
            Field::new("c_events", DataType::Events, true),
            Field::new("c_links", DataType::Links, true),
        ];
        let schema = Schema::try_new_arc(fields).unwrap();
        let locality_groups = vec![
            LocalityGroup::Core,
            LocalityGroup::Core,
            LocalityGroup::Core,
            LocalityGroup::Core,
            LocalityGroup::Core,
            LocalityGroup::Core,
            LocalityGroup::Core,
            LocalityGroup::Core,
            LocalityGroup::WideRow,
            LocalityGroup::ExternalPayload,
            LocalityGroup::WideRow,
            LocalityGroup::Core,
            LocalityGroup::Core,
            LocalityGroup::WideRow,
            LocalityGroup::WideRow,
            LocalityGroup::WideRow,
            LocalityGroup::WideRow,
        ];
        let meta = FileSchemaMeta {
            table_kind: TableKind::Spans,
            sort_cols: vec![5, 12],
            pack_ids: vec![[1u8; 16], [2u8; 16]],
            schema,
            locality_groups,
            shredded_maps: vec![ShreddedMapDir {
                logical_col_idx: 14,
                keys: vec![
                    ShreddedMapKeyEntry {
                        key: "gen_ai.request.model".to_owned(),
                        sub_col_idx: 17,
                        residual_count: 0,
                    },
                    ShreddedMapKeyEntry {
                        key: "customer_id".to_owned(),
                        sub_col_idx: 18,
                        residual_count: 3,
                    },
                ],
            }],
        };

        let encoded = encode_schema(&meta).unwrap();
        let decoded = decode_schema(&encoded).unwrap();
        assert_eq!(decoded, meta);

        // Excessively nested List (> MAX_NESTING_DEPTH = 8) must fail cleanly.
        let mut deep_bytes = Vec::new();
        deep_bytes.push(TableKind::Generic as u8);
        deep_bytes.push(0); // sort_cols
        deep_bytes.extend_from_slice(&0u16.to_le_bytes()); // pack_refs
        deep_bytes.extend_from_slice(&1u16.to_le_bytes()); // 1 field
        deep_bytes.extend_from_slice(&1u16.to_le_bytes()); // name_len = 1
        deep_bytes.push(b'x');
        deep_bytes.extend(std::iter::repeat_n(12u8, MAX_NESTING_DEPTH + 1)); // List tags
        deep_bytes.push(8); // Utf8
        deep_bytes.push(1); // nullable
        deep_bytes.push(0); // Core
        deep_bytes.extend_from_slice(&0u16.to_le_bytes()); // shredded_maps
        let err = decode_schema(&deep_bytes).unwrap_err();
        assert!(matches!(err, StatamError::BoundsExceeded { .. }));
    }

    #[test]
    fn rejects_corrupt_crc_unknown_version_unknown_feature_encrypted_flag() {
        let fh = FileHeader {
            format_version: FORMAT_VERSION,
            flags: 0,
            table_kind: TableKind::Generic,
            checksum_alg: CHECKSUM_ALG_CRC32C,
            reserved0: 0,
            required_features: 0,
            enc_envelope_offset: 0,
            enc_envelope_len: 0,
        };

        // 1. Corrupt CRC
        let mut bad_crc = fh.encode();
        bad_crc[30] ^= 0xFF;
        assert!(matches!(
            FileHeader::decode(&bad_crc).unwrap_err(),
            StatamError::ChecksumMismatch { .. }
        ));

        // 2. Unknown version
        let bad_ver = FileHeader {
            format_version: 99,
            ..fh
        }
        .encode();
        assert!(matches!(
            FileHeader::decode(&bad_ver).unwrap_err(),
            StatamError::UnsupportedVersion {
                found: 99,
                expected: FORMAT_VERSION
            }
        ));

        // 3. Unknown required_features
        let bad_feat = FileHeader {
            required_features: 0x0004_0000,
            ..fh
        }
        .encode();
        assert!(matches!(
            FileHeader::decode(&bad_feat).unwrap_err(),
            StatamError::UnsupportedFeature { mask: 0x0004_0000 }
        ));

        // 4. Encrypted flag without key
        let enc = FileHeader {
            flags: FLAG_ENCRYPTED,
            ..fh
        }
        .encode();
        assert!(matches!(
            FileHeader::decode(&enc).unwrap_err(),
            StatamError::EncryptionRequired
        ));

        // Same 4 checks for Postscript
        let ps = Postscript {
            hot_tail_len: POSTSCRIPT_LEN as u32,
            hot_tail_raw_len: 0,
            format_version: FORMAT_VERSION,
            flags: 0,
            required_features: 0,
            row_count: 0,
            num_logical_cols: 0,
            num_phys_cols: 0,
            num_row_groups: 0,
            total_blocks: 0,
            schema_off: 0,
            schema_len: 0,
            col_dir_off: 0,
            rg_dir_off: 0,
            block_dir_off: 0,
            hot_bloom_off: 0,
            hot_bloom_len: 0,
            hot_dict_off: 0,
            hot_dict_len: 0,
            lazy_dir_off: 0,
            lazy_dir_len: 0,
            enc_envelope_len: 0,
            enc_envelope_offset: 0,
            hot_tail_crc32c: 0,
            reserved: 0,
        };
        let mut bad_ps_crc = ps.encode();
        bad_ps_crc[0] ^= 0x01;
        assert!(matches!(
            Postscript::decode(&bad_ps_crc).unwrap_err(),
            StatamError::ChecksumMismatch { .. }
        ));
        assert!(matches!(
            Postscript::decode(
                &Postscript {
                    format_version: 2,
                    ..ps
                }
                .encode()
            )
            .unwrap_err(),
            StatamError::UnsupportedVersion {
                found: 2,
                expected: 1
            }
        ));
        assert!(matches!(
            Postscript::decode(
                &Postscript {
                    required_features: 1,
                    ..ps
                }
                .encode()
            )
            .unwrap_err(),
            StatamError::UnsupportedFeature { mask: 1 }
        ));
        assert!(matches!(
            Postscript::decode(
                &Postscript {
                    flags: FLAG_ENCRYPTED,
                    ..ps
                }
                .encode()
            )
            .unwrap_err(),
            StatamError::EncryptionRequired
        ));

        // Same for PackPostscript
        let pps = PackPostscript {
            toc_stored_len: PACK_POSTSCRIPT_LEN as u32,
            toc_raw_len: 0,
            format_version: FORMAT_VERSION,
            flags: 0,
            required_features: 0,
            pack_id: [0; 16],
            frame_count: 0,
            chunk_count: 0,
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
            toc_crc32c: 0,
        };
        assert!(matches!(
            PackPostscript::decode(
                &PackPostscript {
                    flags: PACK_FLAG_ENCRYPTED,
                    ..pps
                }
                .encode()
            )
            .unwrap_err(),
            StatamError::EncryptionRequired
        ));
    }

    #[test]
    fn rejects_zip_bomb_and_oversized_counts_without_oom() {
        // 1. Zip bomb: 10-byte stored claim of 10 MiB uncompressed fails ratio guard before alloc.
        let tiny = [0u8; 10];
        let err = decompress_checked(&tiny, Codec::Lz4, 10 * 1024 * 1024, MAX_BLOCK_RAW_BYTES)
            .unwrap_err();
        assert!(matches!(err, StatamError::BoundsExceeded { .. }));

        // 2. Exceeds max_raw even if ratio is OK.
        let fake_large = vec![0u8; 1024];
        let err = decompress_checked(&fake_large, Codec::Lz4, 2048, 1024).unwrap_err();
        assert!(matches!(err, StatamError::BoundsExceeded { .. }));

        // 3. Oversized Postscript counts rejected before any table allocation.
        let bad_ps = Postscript {
            hot_tail_len: POSTSCRIPT_LEN as u32,
            hot_tail_raw_len: 0,
            format_version: FORMAT_VERSION,
            flags: 0,
            required_features: 0,
            row_count: u64::MAX,
            num_logical_cols: 0,
            num_phys_cols: 0,
            num_row_groups: 0,
            total_blocks: 0,
            schema_off: 0,
            schema_len: 0,
            col_dir_off: 0,
            rg_dir_off: 0,
            block_dir_off: 0,
            hot_bloom_off: 0,
            hot_bloom_len: 0,
            hot_dict_off: 0,
            hot_dict_len: 0,
            lazy_dir_off: 0,
            lazy_dir_len: 0,
            enc_envelope_len: 0,
            enc_envelope_offset: 0,
            hot_tail_crc32c: 0,
            reserved: 0,
        }
        .encode();
        assert!(matches!(
            Postscript::decode(&bad_ps).unwrap_err(),
            StatamError::BoundsExceeded { .. }
        ));

        // 4. Schema claiming 1000 fields in a 20-byte buffer rejected before Vec::with_capacity.
        let mut fake_schema = Vec::new();
        fake_schema.push(0); // Generic
        fake_schema.push(0); // sort_cols
        fake_schema.extend_from_slice(&0u16.to_le_bytes()); // pack_refs
        fake_schema.extend_from_slice(&1000u16.to_le_bytes()); // field_count = 1000
        assert!(matches!(
            decode_schema(&fake_schema).unwrap_err(),
            StatamError::Corrupt { .. }
        ));

        // 5. BlockEntry::stored_len > MAX_BLOCK_RAW_BYTES + 1024 rejected.
        let bad_block = BlockEntry {
            first_row: 0,
            row_count: 1,
            null_count: 0,
            offset: 0,
            stored_len: (MAX_BLOCK_RAW_BYTES + 1025) as u32,
            raw_len: 100,
            encoding: Encoding::Plain,
            codec: Codec::None,
            block_flags: 0,
            min_inline_len: 0,
            max_inline_len: 0,
            reserved: [0; 3],
            min_inline: [0; 16],
            max_inline: [0; 16],
        }
        .encode();
        assert!(matches!(
            BlockEntry::decode(&bad_block),
            Err(StatamError::BoundsExceeded { .. })
        ));

        // 6. FrameEntry::stored_len > MAX_FRAME_RAW_BYTES + 1024 rejected.
        let bad_frame = FrameEntry {
            offset: PACK_HEADER_LEN as u64,
            stored_len: (MAX_FRAME_RAW_BYTES + 1025) as u32,
            raw_len: 100,
            codec: Codec::None,
            flags: 0,
            dict_id: 0,
            first_chunk_id: 0,
            chunk_count: 1,
            filter_off: 0,
            filter_len: 0,
            crc32c: 0,
        }
        .encode();
        assert!(matches!(
            FrameEntry::decode(&bad_frame),
            Err(StatamError::BoundsExceeded { .. })
        ));

        // 7. PackPostscript trigram_len > MAX_BLOOM_BYTES and edge_table_len > MAX_BLOCK_RAW_BYTES rejected.
        let base_pps = PackPostscript {
            toc_stored_len: PACK_POSTSCRIPT_LEN as u32,
            toc_raw_len: 0,
            format_version: FORMAT_VERSION,
            flags: 0,
            required_features: 0,
            pack_id: [0; 16],
            frame_count: 0,
            chunk_count: 0,
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
            toc_crc32c: 0,
        };
        assert!(matches!(
            PackPostscript::decode(
                &PackPostscript {
                    trigram_len: (MAX_BLOOM_BYTES + 1) as u32,
                    ..base_pps
                }
                .encode()
            ),
            Err(StatamError::BoundsExceeded { .. })
        ));
        assert!(
            PackPostscript::decode(
                &PackPostscript {
                    edge_table_len: (MAX_BLOCK_RAW_BYTES + PACK_FRAME_HEADER_LEN) as u32,
                    ..base_pps
                }
                .encode()
            )
            .is_ok()
        );
        assert!(matches!(
            PackPostscript::decode(
                &PackPostscript {
                    edge_table_len: (MAX_BLOCK_RAW_BYTES + PACK_FRAME_HEADER_LEN + 1) as u32,
                    ..base_pps
                }
                .encode()
            ),
            Err(StatamError::BoundsExceeded { .. })
        ));

        // 8. RowGroupEntry core_offset + core_len overflow rejected (no saturating_add).
        let overflow_rg = RowGroupEntry {
            first_row: 0,
            row_count: 1,
            core_offset: u64::MAX - 4,
            core_len: 16,
            wide_len: 0,
            wide_offset: u64::MAX,
            l0_payload_offset: 0,
            l0_payload_len: 0,
            rg_flags: 0,
        }
        .encode();
        assert!(matches!(
            RowGroupEntry::decode(&overflow_rg),
            Err(StatamError::BoundsExceeded { .. })
        ));

        // 9. PackHeader rejects conflicting TOC compression flags (`LZ4 | ZSTD`).
        let conflicting_hdr = PackHeader {
            format_version: FORMAT_VERSION,
            flags: PACK_FLAG_TOC_LZ4 | PACK_FLAG_TOC_ZSTD,
            required_features: 0,
            pack_id: [0x11; 16],
            enc_envelope_offset: 0,
            enc_envelope_len: 0,
            reserved: 0,
        }
        .encode();
        assert!(matches!(
            PackHeader::decode(&conflicting_hdr),
            Err(StatamError::Corrupt { .. })
        ));
    }

    #[test]
    fn incompressibility_guard_falls_back_to_raw() {
        // High-entropy pseudo-random bytes cannot shrink by >16 bytes.
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut raw = Vec::with_capacity(4096);
        for _ in 0..4096 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            raw.push((state >> 24) as u8);
        }

        let (codec, stored) = compress_guarded(&raw, Codec::Lz4, 3).unwrap();
        assert_eq!(codec, Codec::None);
        assert_eq!(stored, raw);

        // Highly compressible data does use Lz4.
        let compressible = vec![b'A'; 4096];
        let (codec_c, stored_c) = compress_guarded(&compressible, Codec::Lz4, 3).unwrap();
        assert_eq!(codec_c, Codec::Lz4);
        assert!(stored_c.len() + 16 < compressible.len());

        // Framed SB block roundtrip.
        let mut framed = Vec::new();
        let info =
            write_framed_block(Encoding::Plain, &compressible, Codec::Lz4, 3, &mut framed).unwrap();
        assert_eq!(info.codec, Codec::Lz4);
        let (enc, c, decoded) = read_framed_block(&framed, MAX_BLOCK_RAW_BYTES).unwrap();
        assert_eq!(enc, Encoding::Plain);
        assert_eq!(c, Codec::Lz4);
        assert_eq!(decoded, compressible);

        // Framed PF pack frame roundtrip with LZ4.
        let mut pack_framed = Vec::new();
        let pinfo = write_framed_pack_frame(&raw, Codec::Lz4, 0, 3, &mut pack_framed).unwrap();
        assert_eq!(pinfo.codec, Codec::None);
        let (pc, pflags, pdecoded) =
            read_framed_pack_frame(&pack_framed, MAX_FRAME_RAW_BYTES).unwrap();
        assert_eq!(pc, Codec::None);
        assert_eq!(pflags, 0);
        assert_eq!(pdecoded, raw);
    }

    #[cfg(not(miri))]
    #[test]
    fn zstd_framed_block_and_pack_frame_roundtrip() {
        let payload = b"{\"role\":\"assistant\",\"content\":\"hello world\"}".repeat(128);
        let mut framed = Vec::new();
        let info =
            write_framed_block(Encoding::Fsst, &payload, Codec::Zstd, 3, &mut framed).unwrap();
        assert_eq!(info.codec, Codec::Zstd);
        let (enc, codec, decoded) = read_framed_block(&framed, MAX_BLOCK_RAW_BYTES).unwrap();
        assert_eq!(enc, Encoding::Fsst);
        assert_eq!(codec, Codec::Zstd);
        assert_eq!(decoded, payload);

        let mut pframed = Vec::new();
        let pinfo = write_framed_pack_frame(&payload, Codec::Zstd, 0, 3, &mut pframed).unwrap();
        assert_eq!(pinfo.codec, Codec::Zstd);
        let (pc, pflags, pdecoded) = read_framed_pack_frame(&pframed, MAX_FRAME_RAW_BYTES).unwrap();
        assert_eq!(pc, Codec::Zstd);
        assert_eq!(pflags, 0);
        assert_eq!(pdecoded, payload);

        // write_framed_pack_frame rejects PACK_FRAME_FLAG_USES_DICT and unknown flags.
        let mut bad = Vec::new();
        assert!(matches!(
            write_framed_pack_frame(
                &payload,
                Codec::Zstd,
                PACK_FRAME_FLAG_USES_DICT,
                3,
                &mut bad
            ),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            write_framed_pack_frame(&payload, Codec::Zstd, 0x80, 3, &mut bad),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            write_framed_pack_frame_with_compressor(&payload, Codec::Zstd, 0x80, 3, None, &mut bad),
            Err(StatamError::Corrupt { .. })
        ));
    }

    #[cfg(not(miri))]
    #[test]
    fn zstd_dict_framed_pack_frame_roundtrip_and_guards() {
        let mut samples_buf = Vec::new();
        let mut sample_sizes = Vec::new();
        for i in 0..128u32 {
            let s = format!(
                "{{\"role\":\"assistant\",\"model\":\"gpt-4o\",\"trace\":\"{i:04}\",\"content\":\"structured llm response token alpha beta gamma\"}}"
            );
            sample_sizes.push(s.len());
            samples_buf.extend_from_slice(s.as_bytes());
        }
        let dict_bytes =
            zstd::dict::from_continuous(&samples_buf, &sample_sizes, 32 * 1024).unwrap();
        let mut comp = PackDictCompressor::new(3, &dict_bytes).unwrap();
        let dec_dict = PackDecoderDict::new(&dict_bytes).unwrap();

        let payload = samples_buf[..2048].to_vec();
        let mut framed = Vec::new();
        let info = write_framed_pack_frame_with_compressor(
            &payload,
            Codec::Zstd,
            0,
            3,
            Some(&mut comp),
            &mut framed,
        )
        .unwrap();
        assert_eq!(info.codec, Codec::Zstd);
        assert_ne!(framed[3] & PACK_FRAME_FLAG_USES_DICT, 0);

        let (codec, flags, decoded) =
            read_framed_pack_frame_with_dict(&framed, MAX_FRAME_RAW_BYTES, Some(&dec_dict))
                .unwrap();
        assert_eq!(codec, Codec::Zstd);
        assert_eq!(flags, PACK_FRAME_FLAG_USES_DICT);
        assert_eq!(decoded, payload);

        // Dict flag without dict must return Corrupt.
        let err = read_framed_pack_frame_with_dict(&framed, MAX_FRAME_RAW_BYTES, None).unwrap_err();
        assert!(matches!(err, StatamError::Corrupt { .. }));

        // Dict flag with Codec::None must return Corrupt (even with recomputed header-inclusive CRC).
        let mut bad_codec = framed.clone();
        bad_codec[2] = Codec::None as u8;
        // Without recomputing CRC, flipping the header codec byte triggers ChecksumMismatch.
        let err_crc =
            read_framed_pack_frame_with_dict(&bad_codec, MAX_FRAME_RAW_BYTES, Some(&dec_dict))
                .unwrap_err();
        assert!(matches!(err_crc, StatamError::ChecksumMismatch { .. }));
        let new_crc = pack_frame_crc(&bad_codec[0..12], &bad_codec[PACK_FRAME_HEADER_LEN..]);
        bad_codec[12..16].copy_from_slice(&new_crc.to_le_bytes());
        let err =
            read_framed_pack_frame_with_dict(&bad_codec, MAX_FRAME_RAW_BYTES, Some(&dec_dict))
                .unwrap_err();
        assert!(matches!(err, StatamError::Corrupt { .. }));

        // Incompressible payload falls back to Codec::None and clears PACK_FRAME_FLAG_USES_DICT.
        let mut state: u64 = 0xfeed_cafe_1234_5678;
        let mut random_raw = vec![0u8; 4096];
        for b in &mut random_raw {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *b = (state >> 24) as u8;
        }
        let mut raw_framed = Vec::new();
        let raw_info = write_framed_pack_frame_with_compressor(
            &random_raw,
            Codec::Zstd,
            PACK_FRAME_FLAG_USES_DICT,
            3,
            Some(&mut comp),
            &mut raw_framed,
        )
        .unwrap();
        assert_eq!(raw_info.codec, Codec::None);
        assert_eq!(raw_framed[3] & PACK_FRAME_FLAG_USES_DICT, 0);
        let (rc, rflags, rdecoded) =
            read_framed_pack_frame_with_dict(&raw_framed, MAX_FRAME_RAW_BYTES, Some(&dec_dict))
                .unwrap();
        assert_eq!(rc, Codec::None);
        assert_eq!(rflags, 0);
        assert_eq!(rdecoded, random_raw);
    }

    #[test]
    fn compress_guarded_min_savings_and_cow_zero_copy() {
        // Construct a 4 KiB payload where Zstd saves ~5-9% (>16 bytes, <12% savings).
        let mut mild_entropy = vec![0u8; 4096];
        let mut state: u64 = 0x1234_5678_9abc_def0;
        for b in &mut mild_entropy[..3750] {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *b = (state >> 16) as u8;
        }
        // Remaining 346 bytes are 0u8 (~8.4% compressible).
        let (codec_0, stored_0) =
            compress_guarded_min_savings(&mild_entropy, Codec::Zstd, 3, 0).unwrap();
        assert_eq!(codec_0, Codec::Zstd);
        assert!(stored_0.len() + 16 < mild_entropy.len());
        assert!(stored_0.len() * 100 > mild_entropy.len() * 88);

        // With 12% minimum savings guard, falls back to Codec::None.
        let (codec_12, stored_12) =
            compress_guarded_min_savings(&mild_entropy, Codec::Zstd, 3, 12).unwrap();
        assert_eq!(codec_12, Codec::None);
        assert_eq!(stored_12, mild_entropy);

        // Truly compressible payload (>= 12% savings) keeps Codec::Zstd.
        let compressible = vec![b'A'; 4096];
        let (codec_comp, stored_comp) =
            compress_guarded_min_savings(&compressible, Codec::Zstd, 3, 12).unwrap();
        assert_eq!(codec_comp, Codec::Zstd);
        assert!(stored_comp.len() * 100 <= compressible.len() * 88);

        // Frame with min_savings_pct=12 writes Codec::None, and read_framed_block_cow borrows zero-copy.
        let mut raw_frame = Vec::new();
        let info_none = write_framed_block_with_min_savings(
            Encoding::Plain,
            &mild_entropy,
            Codec::Zstd,
            3,
            12,
            &mut raw_frame,
        )
        .unwrap();
        assert_eq!(info_none.codec, Codec::None);
        let (enc, c, cow_none) = read_framed_block_cow(&raw_frame, MAX_BLOCK_RAW_BYTES).unwrap();
        assert_eq!(enc, Encoding::Plain);
        assert_eq!(c, Codec::None);
        assert!(matches!(cow_none, Cow::Borrowed(_)));
        assert_eq!(cow_none.as_ref(), mild_entropy.as_slice());
        assert_eq!(
            cow_none.as_ptr(),
            raw_frame[BLOCK_FRAME_HEADER_LEN..].as_ptr()
        );

        // Frame with Codec::Zstd returns Cow::Owned.
        let mut zstd_frame = Vec::new();
        let info_zstd = write_framed_block_with_min_savings(
            Encoding::Plain,
            &compressible,
            Codec::Zstd,
            3,
            12,
            &mut zstd_frame,
        )
        .unwrap();
        assert_eq!(info_zstd.codec, Codec::Zstd);
        let (enc_z, c_z, cow_z) = read_framed_block_cow(&zstd_frame, MAX_BLOCK_RAW_BYTES).unwrap();
        assert_eq!(enc_z, Encoding::Plain);
        assert_eq!(c_z, Codec::Zstd);
        assert!(matches!(cow_z, Cow::Owned(_)));
        assert_eq!(cow_z.as_ref(), compressible.as_slice());
    }

    #[test]
    fn m3_privacy_safe_debug_for_shredded_map_and_schema_meta() {
        let secret_key = "SECRET_USER_MAP_KEY_123";
        let entry = ShreddedMapKeyEntry {
            key: secret_key.to_owned(),
            sub_col_idx: 7,
            residual_count: 2,
        };
        let entry_dbg = format!("{entry:?}");
        assert!(entry_dbg.contains("key_len"));
        assert!(!entry_dbg.contains(secret_key));

        let dir = ShreddedMapDir {
            logical_col_idx: 3,
            keys: vec![entry],
        };
        let dir_dbg = format!("{dir:?}");
        assert!(!dir_dbg.contains(secret_key));

        let schema = Arc::new(
            Schema::try_new(vec![
                Field::new("id", DataType::I64, false),
                Field::new("attrs", DataType::MapUtf8, true),
            ])
            .unwrap(),
        );
        let meta = FileSchemaMeta {
            table_kind: TableKind::Spans,
            sort_cols: vec![0],
            pack_ids: vec![[0xAB; 16]],
            schema,
            locality_groups: vec![LocalityGroup::Core, LocalityGroup::WideRow],
            shredded_maps: vec![dir],
        };
        let meta_dbg = format!("{meta:?}");
        assert!(meta_dbg.contains("fields_len"));
        assert!(meta_dbg.contains("shredded_maps_len"));
        assert!(!meta_dbg.contains(secret_key));
    }

    #[test]
    fn m1_m9_pack_frame_flags_and_custom_max_raw_limit() {
        let raw = vec![b'x'; 256];
        let mut framed = Vec::new();
        let info =
            write_framed_pack_frame_with_limit(&raw, Codec::Lz4, 0, 3, 256, &mut framed).unwrap();
        assert_eq!(info.raw_len, 256);
        let (_, flags, dec) = read_framed_pack_frame(&framed, 256).unwrap();
        assert_eq!(flags, 0);
        assert_eq!(dec, raw);

        // Exceeding custom max_raw returns BoundsExceeded.
        let mut over = Vec::new();
        assert!(matches!(
            write_framed_pack_frame_with_limit(&raw, Codec::Lz4, 0, 3, 255, &mut over),
            Err(StatamError::BoundsExceeded { .. })
        ));

        // Flipping PF flags without updating header-inclusive CRC returns ChecksumMismatch;
        // after recomputing CRC, read_framed_pack_frame rejects PACK_FRAME_FLAG_USES_DICT as Corrupt.
        let mut dict_flagged = framed.clone();
        dict_flagged[3] = PACK_FRAME_FLAG_USES_DICT;
        assert!(matches!(
            read_framed_pack_frame(&dict_flagged, MAX_FRAME_RAW_BYTES),
            Err(StatamError::ChecksumMismatch { .. })
        ));
        let recomputed =
            pack_frame_crc(&dict_flagged[0..12], &dict_flagged[PACK_FRAME_HEADER_LEN..]);
        dict_flagged[12..16].copy_from_slice(&recomputed.to_le_bytes());
        assert!(matches!(
            read_framed_pack_frame(&dict_flagged, MAX_FRAME_RAW_BYTES),
            Err(StatamError::Corrupt { .. })
        ));
    }

    #[test]
    fn m2_m5_header_inclusive_crcs_strict_entries_and_schema_uniqueness() {
        assert_eq!(MINI_BLOCK_SIZE, 8_192);

        // 1. SB block header-inclusive CRC catches single-bit flips in header bytes [2..12].
        let raw = vec![b'z'; 512];
        let mut sb = Vec::new();
        let sb_info = write_framed_block(Encoding::Plain, &raw, Codec::Lz4, 3, &mut sb).unwrap();
        let blk = BlockEntry {
            first_row: 0,
            row_count: 1,
            null_count: 0,
            offset: 0,
            stored_len: sb_info.stored_len,
            raw_len: sb_info.raw_len,
            encoding: Encoding::Plain,
            codec: sb_info.codec,
            block_flags: 0,
            min_inline_len: 0,
            max_inline_len: 0,
            reserved: [0; 3],
            min_inline: [0; 16],
            max_inline: [0; 16],
        };
        assert_eq!(
            read_framed_block_checked_cow(&sb, &blk, MAX_BLOCK_RAW_BYTES)
                .unwrap()
                .as_ref(),
            raw.as_slice()
        );
        for hdr_byte in [2usize, 3, 4] {
            let mut tampered = sb.clone();
            tampered[hdr_byte] ^= 0x01;
            // Make sure encoding/codec byte stays valid enum so CRC check runs.
            if hdr_byte == 2 {
                tampered[2] = Encoding::Rle as u8;
            } else if hdr_byte == 3 {
                tampered[3] = Codec::None as u8;
            }
            assert!(matches!(
                read_framed_block_cow(&tampered, MAX_BLOCK_RAW_BYTES),
                Err(StatamError::ChecksumMismatch { .. })
            ));
        }
        // Recomputing SB CRC with mismatched encoding triggers read_framed_block_checked_cow Corrupt.
        let mut wrong_enc_sb = sb.clone();
        wrong_enc_sb[2] = Encoding::Rle as u8;
        let new_crc = block_frame_crc(
            &wrong_enc_sb[0..12],
            &wrong_enc_sb[BLOCK_FRAME_HEADER_LEN..],
        );
        wrong_enc_sb[12..16].copy_from_slice(&new_crc.to_le_bytes());
        assert!(matches!(
            read_framed_block_checked_cow(&wrong_enc_sb, &blk, MAX_BLOCK_RAW_BYTES),
            Err(StatamError::Corrupt { .. })
        ));

        // 2. ColumnDirEntry / BlockEntry / LazyIndexEntry / FrameEntry unknown flags & reserved.
        let mut bad_col = ColumnDirEntry {
            phys_col_id: 0,
            logical_col_id: 0,
            locality_group: LocalityGroup::Core,
            col_flags: 0x80,
            reserved: 0,
            num_blocks: 0,
            first_block_idx: 0,
            dict_offset: 0,
            dict_len: 0,
            dict_entry_count: 0,
            bloom_offset: 0,
            bloom_len: 0,
            null_count_total: 0,
            trigram_offset: 0,
        };
        assert!(matches!(
            ColumnDirEntry::decode(&bad_col.encode()),
            Err(StatamError::Corrupt { .. })
        ));
        bad_col.col_flags = 0;
        bad_col.reserved = 1;
        assert!(matches!(
            ColumnDirEntry::decode(&bad_col.encode()),
            Err(StatamError::Corrupt { .. })
        ));

        let mut bad_blk = blk;
        bad_blk.block_flags = 0x80;
        assert!(matches!(
            BlockEntry::decode(&bad_blk.encode()),
            Err(StatamError::Corrupt { .. })
        ));
        bad_blk.block_flags = 0;
        bad_blk.reserved = [0, 1, 0];
        assert!(matches!(
            BlockEntry::decode(&bad_blk.encode()),
            Err(StatamError::Corrupt { .. })
        ));

        let bad_lazy = LazyIndexEntry {
            kind: LazyIndexKind::Bloom,
            reserved: [0, 0, 1],
            phys_col_id: 0,
            offset: 32,
            len: 64,
            crc32c: 0,
        };
        assert!(matches!(
            LazyIndexEntry::decode(&bad_lazy.encode()),
            Err(StatamError::Corrupt { .. })
        ));

        let base_fe = FrameEntry {
            offset: PACK_HEADER_LEN as u64,
            stored_len: 16,
            raw_len: 16,
            codec: Codec::None,
            flags: 0,
            dict_id: 0,
            first_chunk_id: 0,
            chunk_count: 1,
            filter_off: 0,
            filter_len: 0,
            crc32c: 0,
        };
        assert!(matches!(
            FrameEntry::decode(
                &FrameEntry {
                    flags: 0x02,
                    ..base_fe
                }
                .encode()
            ),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            FrameEntry::decode(
                &FrameEntry {
                    dict_id: 1,
                    ..base_fe
                }
                .encode()
            ),
            Err(StatamError::Corrupt { .. })
        ));

        // 3. Postscript & PackPostscript enforce off == 0 when len == 0 and dict_len cap.
        let base_ps = Postscript {
            hot_tail_len: POSTSCRIPT_LEN as u32,
            hot_tail_raw_len: 0,
            format_version: FORMAT_VERSION,
            flags: 0,
            required_features: 0,
            row_count: 0,
            num_logical_cols: 0,
            num_phys_cols: 0,
            num_row_groups: 0,
            total_blocks: 0,
            schema_off: 0,
            schema_len: 0,
            col_dir_off: 0,
            rg_dir_off: 0,
            block_dir_off: 0,
            hot_bloom_off: 0,
            hot_bloom_len: 0,
            hot_dict_off: 0,
            hot_dict_len: 0,
            lazy_dir_off: 0,
            lazy_dir_len: 0,
            enc_envelope_len: 0,
            enc_envelope_offset: 0,
            hot_tail_crc32c: 0,
            reserved: 0,
        };
        for mutated in [
            Postscript {
                hot_bloom_off: 1,
                ..base_ps
            },
            Postscript {
                hot_dict_off: 1,
                ..base_ps
            },
            Postscript {
                lazy_dir_off: 1,
                ..base_ps
            },
        ] {
            assert!(matches!(
                Postscript::decode(&mutated.encode()),
                Err(StatamError::Corrupt { .. })
            ));
        }

        let base_pps = PackPostscript {
            toc_stored_len: PACK_POSTSCRIPT_LEN as u32,
            toc_raw_len: 0,
            format_version: FORMAT_VERSION,
            flags: 0,
            required_features: 0,
            pack_id: [0; 16],
            frame_count: 0,
            chunk_count: 0,
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
            toc_crc32c: 0,
        };
        for mutated in [
            PackPostscript {
                hash_table_off: 48,
                ..base_pps
            },
            PackPostscript {
                trigram_off: 48,
                ..base_pps
            },
            PackPostscript {
                edge_table_off: 48,
                ..base_pps
            },
            PackPostscript {
                dict_off: 48,
                ..base_pps
            },
        ] {
            assert!(matches!(
                PackPostscript::decode(&mutated.encode()),
                Err(StatamError::Corrupt { .. })
            ));
        }
        assert!(matches!(
            PackPostscript::decode(
                &PackPostscript {
                    dict_off: 48,
                    dict_len: (MAX_PACK_DICT_BYTES + 1025) as u32,
                    ..base_pps
                }
                .encode()
            ),
            Err(StatamError::BoundsExceeded { .. })
        ));

        // 4. Schema uniqueness checks in both encode_schema and decode_schema.
        let schema = Schema::try_new_arc(vec![
            Field::new("ts", DataType::I64, false),
            Field::new("m1", DataType::MapUtf8, true),
            Field::new("m2", DataType::MapUtf8, true),
        ])
        .unwrap();
        let valid_meta = FileSchemaMeta {
            table_kind: TableKind::Generic,
            sort_cols: vec![0],
            pack_ids: Vec::new(),
            schema: Arc::clone(&schema),
            locality_groups: vec![
                LocalityGroup::Core,
                LocalityGroup::WideRow,
                LocalityGroup::WideRow,
            ],
            shredded_maps: vec![
                ShreddedMapDir {
                    logical_col_idx: 1,
                    keys: vec![
                        ShreddedMapKeyEntry {
                            key: "k1".to_owned(),
                            sub_col_idx: 3,
                            residual_count: 0,
                        },
                        ShreddedMapKeyEntry {
                            key: "k2".to_owned(),
                            sub_col_idx: 4,
                            residual_count: 0,
                        },
                    ],
                },
                ShreddedMapDir {
                    logical_col_idx: 2,
                    keys: vec![ShreddedMapKeyEntry {
                        key: "k1".to_owned(),
                        sub_col_idx: 5,
                        residual_count: 0,
                    }],
                },
            ],
        };
        let encoded_valid = encode_schema(&valid_meta).unwrap();
        assert_eq!(decode_schema(&encoded_valid).unwrap(), valid_meta);

        // Duplicate sort_cols rejected by encode_schema.
        assert!(matches!(
            encode_schema(&FileSchemaMeta {
                sort_cols: vec![0, 0],
                ..valid_meta.clone()
            }),
            Err(StatamError::SchemaMismatch { .. })
        ));
        // Duplicate shredded logical_col_idx rejected by encode_schema.
        assert!(matches!(
            encode_schema(&FileSchemaMeta {
                shredded_maps: vec![
                    ShreddedMapDir {
                        logical_col_idx: 1,
                        keys: vec![],
                    },
                    ShreddedMapDir {
                        logical_col_idx: 1,
                        keys: vec![],
                    },
                ],
                ..valid_meta.clone()
            }),
            Err(StatamError::SchemaMismatch { .. })
        ));
        // Duplicate promoted key within one map rejected by encode_schema.
        assert!(matches!(
            encode_schema(&FileSchemaMeta {
                shredded_maps: vec![ShreddedMapDir {
                    logical_col_idx: 1,
                    keys: vec![
                        ShreddedMapKeyEntry {
                            key: "dup".to_owned(),
                            sub_col_idx: 3,
                            residual_count: 0,
                        },
                        ShreddedMapKeyEntry {
                            key: "dup".to_owned(),
                            sub_col_idx: 4,
                            residual_count: 0,
                        },
                    ],
                }],
                ..valid_meta.clone()
            }),
            Err(StatamError::SchemaMismatch { .. })
        ));
        // Duplicate sub_col_idx across shredded maps rejected by encode_schema.
        assert!(matches!(
            encode_schema(&FileSchemaMeta {
                shredded_maps: vec![
                    ShreddedMapDir {
                        logical_col_idx: 1,
                        keys: vec![ShreddedMapKeyEntry {
                            key: "a".to_owned(),
                            sub_col_idx: 3,
                            residual_count: 0,
                        }],
                    },
                    ShreddedMapDir {
                        logical_col_idx: 2,
                        keys: vec![ShreddedMapKeyEntry {
                            key: "b".to_owned(),
                            sub_col_idx: 3,
                            residual_count: 0,
                        }],
                    },
                ],
                ..valid_meta
            }),
            Err(StatamError::SchemaMismatch { .. })
        ));

        // Craft raw schema bytes with duplicate sort_cols / duplicate shredded entries to verify decode_schema.
        let mut raw_dup_sort = Vec::new();
        raw_dup_sort.push(TableKind::Generic as u8);
        raw_dup_sort.push(2); // sort_col_count = 2
        raw_dup_sort.extend_from_slice(&0u16.to_le_bytes());
        raw_dup_sort.extend_from_slice(&0u16.to_le_bytes()); // duplicate
        raw_dup_sort.extend_from_slice(&0u16.to_le_bytes()); // pack_refs
        raw_dup_sort.extend_from_slice(&1u16.to_le_bytes()); // 1 field
        raw_dup_sort.extend_from_slice(&2u16.to_le_bytes());
        raw_dup_sort.extend_from_slice(b"ts");
        raw_dup_sort.push(2); // I64
        raw_dup_sort.push(0); // nullable = false
        raw_dup_sort.push(0); // Core
        raw_dup_sort.extend_from_slice(&0u16.to_le_bytes()); // shredded_maps
        assert!(matches!(
            decode_schema(&raw_dup_sort),
            Err(StatamError::Corrupt { .. })
        ));
    }
}
