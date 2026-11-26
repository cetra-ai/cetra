//! Range-based `.statam` reader with 1-GET cold open, SBBF/zone-map pruning,
//! two-phase late materialization, `map_keys` shredded projection, and exact substring search.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use cetra_column::{
    Batch, Bitmap, Column, DataType, FlatColumn, FlatData, ScalarValue, Schema, Selection, Utf8Data,
};
use memchr::memmem;

use crate::StatamError;
use crate::codec::dict::FileDict;
use crate::codec::nested::{
    ShreddedSubCol, decode_shredded_residual_block, decode_shredded_subcol, decode_single_map_key,
    unshred_map,
};
use crate::codec::{ZonePred, ZoneValue, decode_mini_block, zone_map_may_match};
use crate::format::{
    BLOCK_ENTRY_LEN, BLOCK_FLAG_HAS_NAN, BLOCK_FLAG_MIN_MAX_VALID, BLOCK_FRAME_HEADER_LEN,
    BlockEntry, CHECKSUM_ALG_CRC32C, COL_FLAG_BLOOM_IN_HOT_TAIL, COL_FLAG_DICT_IN_HOT_TAIL,
    COL_FLAG_DICT_SORTED, COL_FLAG_HAS_FILE_DICT, COL_FLAG_HAS_TRIGRAM_INDEX,
    COL_FLAG_IS_SHREDDED_SUBCOL, COLUMN_DIR_ENTRY_LEN, ChunkLoc, Codec, ColumnDirEntry,
    DEFAULT_TAIL_HINT, Encoding, FILE_HEADER_LEN, FLAG_HOT_TAIL_LZ4, FileHeader, FileSchemaMeta,
    LAZY_INDEX_ENTRY_LEN, LazyIndexEntry, LazyIndexKind, LocalityGroup, MAX_BATCH_DECODED_BYTES,
    MAX_BLOCK_RAW_BYTES, MAX_FRAME_RAW_BYTES, MAX_HOT_TAIL_BYTES, MAX_ROW_PAYLOAD_BYTES,
    POSTSCRIPT_LEN, Postscript, ROW_GROUP_ENTRY_LEN, RowGroupEntry, decode_schema,
    decompress_checked, read_framed_block, read_framed_pack_frame,
};
use crate::io::RangeReader;
use crate::payload::pack::ChunkPackReader;
use crate::payload::search::search_pack_rows;
use crate::payload::{PayloadRefRow, decode_payload_refs};
use crate::simd::bloom::{Sbbf, statam_bloom_hash};
use crate::simd::filter::{FilterOp, filter_f64, filter_i64, filter_i64_between, filter_u32};
use crate::simd::trigram::{TrigramBloom, ascii_lowercase_into, extract_trigram_hashes};
use crate::writer::{L0_LOCAL_PACK_IDX, L0_PAYLOAD_DIR_MAGIC, NARROW_TRIGRAM_MAGIC};

/// Maximum gap (64 KiB) between blocks coalesced into a single `Scan` range read.
const SCAN_COALESCE_MAX_GAP: u64 = 64 * 1024;
/// Maximum total span (4 MiB) of a single coalesced `Scan` range read.
const SCAN_COALESCE_MAX_SPAN: u64 = 4 * 1024 * 1024;

/// Resolves external `.stpk` pack IDs to opened [`ChunkPackReader`] instances.
pub trait PackResolver: Send + Sync {
    /// Returns the opened [`ChunkPackReader`] for `pack_id`, or `StatamError::MissingPack`.
    fn resolve(&self, pack_id: [u8; 16]) -> Result<Arc<ChunkPackReader>, StatamError>;
}

/// Simple in-memory [`PackResolver`] backed by a map of `pack_id -> Arc<ChunkPackReader>`.
#[derive(Debug, Default, Clone)]
pub struct InMemoryPackResolver {
    packs: HashMap<[u8; 16], Arc<ChunkPackReader>>,
}

impl InMemoryPackResolver {
    /// Creates an empty [`InMemoryPackResolver`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an already-opened [`ChunkPackReader`].
    pub fn insert(&mut self, pack_id: [u8; 16], pack: Arc<ChunkPackReader>) {
        self.packs.insert(pack_id, pack);
    }

    /// Opens an in-memory `.stpk` byte buffer via [`crate::SliceReader`] and registers it.
    pub fn insert_bytes(
        &mut self,
        pack_id: [u8; 16],
        bytes: impl Into<Bytes>,
    ) -> Result<Arc<ChunkPackReader>, StatamError> {
        let reader = Arc::new(crate::SliceReader::new(bytes));
        let pack = Arc::new(ChunkPackReader::open(reader)?);
        self.packs.insert(pack_id, Arc::clone(&pack));
        Ok(pack)
    }

    /// Looks up a pack if registered.
    pub fn get(&self, pack_id: &[u8; 16]) -> Option<&Arc<ChunkPackReader>> {
        self.packs.get(pack_id)
    }
}

impl PackResolver for InMemoryPackResolver {
    fn resolve(&self, pack_id: [u8; 16]) -> Result<Arc<ChunkPackReader>, StatamError> {
        self.packs
            .get(&pack_id)
            .cloned()
            .ok_or(StatamError::MissingPack { pack_id })
    }
}

/// I/O coalescing strategy for [`StatamReader::scan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CoalesceMode {
    /// Automatically uses `Point` when `id_lookups` is non-empty, else `Scan`.
    #[default]
    Auto,
    /// Single contiguous RG slice read (`core..wide` + optional `l0_payload`) for <= 2-GET point lookups.
    Point,
    /// Two-phase late materialization (Phase 1 filters coalesced <= 64 KiB / 4 MiB, Phase 2 surviving blocks).
    Scan,
}

/// Inclusive nanosecond timestamp range filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRange {
    /// Minimum timestamp (UTC ns, inclusive).
    pub min_ns: i64,
    /// Maximum timestamp (UTC ns, inclusive).
    pub max_ns: i64,
}

impl TimeRange {
    /// Unbounded timestamp range matching all rows.
    pub const ALL: Self = Self {
        min_ns: i64::MIN,
        max_ns: i64::MAX,
    };

    /// Creates an inclusive `[min_ns, max_ns]` range.
    pub const fn new(min_ns: i64, max_ns: i64) -> Self {
        Self { min_ns, max_ns }
    }

    /// True when `min_ns > max_ns` (matches zero rows).
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.min_ns > self.max_ns
    }

    /// True when `[i64::MIN, i64::MAX]`.
    #[inline]
    pub const fn is_all(&self) -> bool {
        self.min_ns == i64::MIN && self.max_ns == i64::MAX
    }
}

impl Default for TimeRange {
    fn default() -> Self {
        Self::ALL
    }
}

/// Exact ID equality / `IN (...)` lookup evaluated against SBBF blooms, zone maps, and rows.
#[derive(Clone, PartialEq, Eq)]
pub struct IdLookup {
    /// Logical column name (`"trace_id"`, `"span_id"`, `"session_id"`, `"user_id"`, `"score_id"`, etc.).
    pub column: String,
    /// Candidate raw ID byte representations (`16 B` for `FixedBytes16`, `8 B` for `FixedBytes8`, UTF-8 for `Utf8`, etc.).
    pub values: Vec<Vec<u8>>,
}

impl fmt::Debug for IdLookup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IdLookup")
            .field("column", &self.column)
            .field("value_count", &self.values.len())
            .finish()
    }
}

impl IdLookup {
    /// Single-ID equality lookup (`col = val`).
    pub fn eq(column: impl Into<String>, value: impl AsRef<[u8]>) -> Self {
        Self {
            column: column.into(),
            values: vec![value.as_ref().to_vec()],
        }
    }

    /// Multi-ID `IN (...)` lookup.
    pub fn in_list(column: impl Into<String>, values: Vec<Vec<u8>>) -> Self {
        Self {
            column: column.into(),
            values,
        }
    }
}

/// Column or shredded-map-key predicate pushed down into pruning and SIMD evaluation.
#[derive(Debug, Clone, PartialEq)]
pub struct Predicate {
    /// Logical column name.
    pub column: String,
    /// Optional map key when filtering `col['key']` on a `MapUtf8` column.
    pub map_key: Option<String>,
    /// Predicate operation and literal.
    pub pred: ZonePred,
}

impl Predicate {
    /// Builds a predicate on a regular logical column.
    pub fn new(column: impl Into<String>, pred: ZonePred) -> Self {
        Self {
            column: column.into(),
            map_key: None,
            pred,
        }
    }

    /// Builds a predicate on `column['key']` for a `MapUtf8` column.
    pub fn map_key(column: impl Into<String>, key: impl Into<String>, pred: ZonePred) -> Self {
        Self {
            column: column.into(),
            map_key: Some(key.into()),
            pred,
        }
    }
}

/// Exact substring search query (`LIKE '%needle%'` / `ILIKE '%needle%'`).
#[derive(Clone, PartialEq, Eq)]
pub struct SubstringQuery {
    /// Logical `Utf8` column name.
    pub column: String,
    /// Literal substring bytes to search for.
    pub needle: Vec<u8>,
    /// Case-insensitive `ILIKE` if `true`, exact `LIKE` if `false`.
    pub case_insensitive: bool,
}

impl fmt::Debug for SubstringQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubstringQuery")
            .field("column", &self.column)
            .field("needle_len", &self.needle.len())
            .field("case_insensitive", &self.case_insensitive)
            .finish()
    }
}

impl SubstringQuery {
    /// Case-sensitive substring match (`LIKE '%needle%'`).
    pub fn like(column: impl Into<String>, needle: impl AsRef<[u8]>) -> Self {
        Self {
            column: column.into(),
            needle: needle.as_ref().to_vec(),
            case_insensitive: false,
        }
    }

    /// Case-insensitive substring match (`ILIKE '%needle%'`).
    pub fn ilike(column: impl Into<String>, needle: impl AsRef<[u8]>) -> Self {
        Self {
            column: column.into(),
            needle: needle.as_ref().to_vec(),
            case_insensitive: true,
        }
    }
}

/// Scan specification passed to [`StatamReader::scan`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScanRequest {
    /// Logical column indices to project (`None` = all columns, `Some(vec![])` = 0-col row count batches).
    pub projection: Option<Vec<usize>>,
    /// Selective `(map_col_name, map_key)` projections pruning `MapUtf8` columns to requested keys.
    pub map_keys: Vec<(String, String)>,
    /// Inclusive timestamp filter.
    pub time_range: TimeRange,
    /// Exact ID lookups (checked against Hot/Lazy SBBF blooms + zone maps).
    pub id_lookups: Vec<IdLookup>,
    /// Column & map-key filter predicates.
    pub predicates: Vec<Predicate>,
    /// Exact `LIKE` / `ILIKE` substring filters.
    pub substrings: Vec<SubstringQuery>,
    /// When `true` and `limit` is `Some`, iterates Row Groups in reverse order (`start_time DESC LIMIT k`).
    pub sort_desc_hint: bool,
    /// Maximum active rows to return.
    pub limit: Option<usize>,
    /// I/O coalescing mode (`Auto`, `Point`, or `Scan`).
    pub coalesce_mode: CoalesceMode,
}

/// Parsed Hot Tail metadata for a `.statam` file (opened in 1 GET).
#[derive(Clone)]
pub struct StatamFileMeta {
    /// Total file byte length.
    pub file_len: u64,
    /// Decoded 112-byte Postscript.
    pub postscript: Postscript,
    /// Decoded Schema, sort key, pack IDs, locality groups, and shredded map metadata.
    pub schema_meta: FileSchemaMeta,
    /// Physical column directory (`num_phys_cols` entries).
    pub col_dir: Vec<ColumnDirEntry>,
    /// Row-group directory (`num_row_groups` entries).
    pub rg_dir: Vec<RowGroupEntry>,
    /// Mini-block directory (`total_blocks` entries).
    pub block_dir: Vec<BlockEntry>,
    /// Raw Hot Bloom bytes from the Hot Tail.
    pub hot_bloom_bytes: Bytes,
    /// Raw Hot Dict bytes from the Hot Tail.
    pub hot_dict_bytes: Bytes,
    /// Lazy Index directory entries (`lazy_dir_len / 24` entries).
    pub lazy_dir: Vec<LazyIndexEntry>,
    /// Absolute byte offset where the stored Hot Tail begins.
    pub hot_tail_start: u64,
}

impl fmt::Debug for StatamFileMeta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatamFileMeta")
            .field("file_len", &self.file_len)
            .field("row_count", &self.postscript.row_count)
            .field("num_logical_cols", &self.postscript.num_logical_cols)
            .field("num_phys_cols", &self.postscript.num_phys_cols)
            .field("num_row_groups", &self.postscript.num_row_groups)
            .field("total_blocks", &self.postscript.total_blocks)
            .finish()
    }
}

impl StatamFileMeta {
    /// Reads and validates the Hot Tail in **1 GET** (with at most 1 follow-up read if `hot_tail_len > tail_hint`).
    pub fn read_from(
        reader: &dyn RangeReader,
        tail_hint: Option<u32>,
    ) -> Result<Self, StatamError> {
        let file_len = reader.file_len();
        let min_len = (FILE_HEADER_LEN + POSTSCRIPT_LEN) as u64;
        if file_len < min_len {
            return Err(StatamError::Corrupt {
                reason: "statam file smaller than header + postscript",
            });
        }

        let hint = u64::from(tail_hint.unwrap_or(DEFAULT_TAIL_HINT)).max(POSTSCRIPT_LEN as u64);
        let tail_start = file_len.saturating_sub(hint);
        let expected_tail_len = file_len
            .checked_sub(tail_start)
            .ok_or(StatamError::Corrupt {
                reason: "invalid tail_start",
            })?;
        let tail = reader.read_range(tail_start..file_len)?;
        if tail.len() as u64 != expected_tail_len || tail.len() < POSTSCRIPT_LEN {
            return Err(StatamError::Corrupt {
                reason: "short read on statam postscript",
            });
        }

        let ps = Postscript::decode(&tail[tail.len() - POSTSCRIPT_LEN..])?;
        if ps.num_row_groups > 0 && ps.num_logical_cols == 0 {
            return Err(StatamError::Corrupt {
                reason: "non-empty row groups with zero logical columns",
            });
        }
        let hot_tail_u64 = u64::from(ps.hot_tail_len);
        if hot_tail_u64 > file_len - FILE_HEADER_LEN as u64 {
            return Err(StatamError::Corrupt {
                reason: "hot_tail_len overlaps file header",
            });
        }
        let hot_tail_start = file_len - hot_tail_u64;

        let header_opt = if tail_start == 0 && tail.len() >= FILE_HEADER_LEN {
            let hdr = FileHeader::decode(&tail[0..FILE_HEADER_LEN])?;
            if hdr.flags != ps.flags || hdr.required_features != ps.required_features {
                return Err(StatamError::Corrupt {
                    reason: "file header and postscript flags mismatch",
                });
            }
            Some(hdr)
        } else {
            None
        };

        let stored_body_len = (ps.hot_tail_len as usize) - POSTSCRIPT_LEN;
        let owned_body: Bytes;
        let stored_body: &[u8] = if tail.len() >= ps.hot_tail_len as usize {
            let start = tail.len() - ps.hot_tail_len as usize;
            &tail[start..start + stored_body_len]
        } else {
            let body_end = file_len - POSTSCRIPT_LEN as u64;
            owned_body = reader.read_range(hot_tail_start..body_end)?;
            if owned_body.len() != stored_body_len {
                return Err(StatamError::Corrupt {
                    reason: "short follow-up read on hot tail body",
                });
            }
            &owned_body
        };

        if crc32c::crc32c(stored_body) != ps.hot_tail_crc32c {
            return Err(StatamError::ChecksumMismatch {
                context: "hot tail",
            });
        }

        let hot_codec = if ps.flags & FLAG_HOT_TAIL_LZ4 != 0 {
            Codec::Lz4
        } else {
            Codec::None
        };
        let raw_body = decompress_checked(
            stored_body,
            hot_codec,
            ps.hot_tail_raw_len as usize,
            MAX_HOT_TAIL_BYTES,
        )?;

        let schema_slice =
            &raw_body[ps.schema_off as usize..ps.schema_off as usize + ps.schema_len as usize];
        let schema_meta = decode_schema(schema_slice)?;
        if schema_meta.schema.len() != ps.num_logical_cols as usize {
            return Err(StatamError::Corrupt {
                reason: "schema logical field count mismatch with postscript",
            });
        }
        if let Some(hdr) = header_opt
            && hdr.table_kind != schema_meta.table_kind
        {
            return Err(StatamError::Corrupt {
                reason: "file header and schema table_kind mismatch",
            });
        }

        let num_phys_cols = ps.num_phys_cols as usize;
        let mut col_dir = Vec::with_capacity(num_phys_cols);
        for idx in 0..num_phys_cols {
            let off = ps.col_dir_off as usize + idx * COLUMN_DIR_ENTRY_LEN;
            let entry = ColumnDirEntry::decode(&raw_body[off..off + COLUMN_DIR_ENTRY_LEN])?;
            if entry.phys_col_id as usize != idx {
                return Err(StatamError::Corrupt {
                    reason: "column directory phys_col_id out of sequence",
                });
            }
            if (entry.logical_col_id as usize) >= ps.num_logical_cols as usize {
                return Err(StatamError::Corrupt {
                    reason: "column directory logical_col_id out of bounds",
                });
            }
            if idx < ps.num_logical_cols as usize && entry.logical_col_id as usize != idx {
                return Err(StatamError::Corrupt {
                    reason: "logical column directory logical_col_id mismatch",
                });
            }
            let blk_end = entry.first_block_idx.checked_add(entry.num_blocks).ok_or(
                StatamError::BoundsExceeded {
                    reason: "column block range overflow",
                },
            )?;
            if blk_end > ps.total_blocks {
                return Err(StatamError::Corrupt {
                    reason: "column block range exceeds total_blocks",
                });
            }
            col_dir.push(entry);
        }

        for sm in &schema_meta.shredded_maps {
            if (sm.logical_col_idx as usize) >= ps.num_logical_cols as usize {
                return Err(StatamError::Corrupt {
                    reason: "shredded map logical_col_idx out of bounds",
                });
            }
            if schema_meta.schema.fields()[sm.logical_col_idx as usize].data_type
                != DataType::MapUtf8
            {
                return Err(StatamError::Corrupt {
                    reason: "shredded map logical_col_idx is not MapUtf8",
                });
            }
            for k in &sm.keys {
                let sidx = k.sub_col_idx as usize;
                if sidx < ps.num_logical_cols as usize || sidx >= num_phys_cols {
                    return Err(StatamError::Corrupt {
                        reason: "shredded map sub_col_idx out of physical column range",
                    });
                }
                if col_dir[sidx].logical_col_id != sm.logical_col_idx {
                    return Err(StatamError::Corrupt {
                        reason: "shredded map sub_col_idx logical_col_id mismatch",
                    });
                }
            }
        }

        let num_row_groups = ps.num_row_groups as usize;
        let mut rg_dir = Vec::with_capacity(num_row_groups);
        let mut expected_first_row = 0u64;
        for idx in 0..num_row_groups {
            let off = ps.rg_dir_off as usize + idx * ROW_GROUP_ENTRY_LEN;
            let rg = RowGroupEntry::decode(&raw_body[off..off + ROW_GROUP_ENTRY_LEN])?;
            if u64::from(rg.first_row) != expected_first_row {
                return Err(StatamError::Corrupt {
                    reason: "row group first_row not contiguous",
                });
            }
            expected_first_row = expected_first_row
                .checked_add(u64::from(rg.row_count))
                .ok_or(StatamError::BoundsExceeded {
                    reason: "row group row_count sum overflow",
                })?;
            validate_rg_bounds(&rg, hot_tail_start)?;
            rg_dir.push(rg);
        }
        if expected_first_row != ps.row_count {
            return Err(StatamError::Corrupt {
                reason: "row group row_counts do not sum to postscript row_count",
            });
        }

        let total_blocks = ps.total_blocks as usize;
        let mut block_dir = Vec::with_capacity(total_blocks);
        for idx in 0..total_blocks {
            let off = ps.block_dir_off as usize + idx * BLOCK_ENTRY_LEN;
            let blk = BlockEntry::decode(&raw_body[off..off + BLOCK_ENTRY_LEN])?;
            if blk.stored_len > 0 {
                let total_blk = (BLOCK_FRAME_HEADER_LEN as u64)
                    .checked_add(u64::from(blk.stored_len))
                    .ok_or(StatamError::BoundsExceeded {
                        reason: "block byte span overflow",
                    })?;
                let blk_end =
                    blk.offset
                        .checked_add(total_blk)
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "block end offset overflow",
                        })?;
                if blk.offset < FILE_HEADER_LEN as u64 || blk_end > hot_tail_start {
                    return Err(StatamError::Corrupt {
                        reason: "block offset lies outside file data region",
                    });
                }
            }
            block_dir.push(blk);
        }

        // Validate every physical column's blocks partition each RG's exact row range
        // with identical mini-block count and identical (first_row, row_count), and every
        // non-inline block lies within its owning RG's [core_offset .. wide_offset + wide_len].
        let mut cursors = vec![0usize; num_phys_cols];
        for rg in &rg_dir {
            let rg_start = u64::from(rg.first_row);
            let rg_end = rg_start.checked_add(u64::from(rg.row_count)).ok_or(
                StatamError::BoundsExceeded {
                    reason: "row group row end overflow",
                },
            )?;
            let rg_wide_end = rg.wide_offset.checked_add(u64::from(rg.wide_len)).ok_or(
                StatamError::BoundsExceeded {
                    reason: "row group wide end overflow",
                },
            )?;

            if num_phys_cols == 0 {
                continue;
            }

            let mut expected_mbs: Vec<(u32, u16)> = Vec::new();
            {
                let cd0 = &col_dir[0];
                let col0_start = cd0.first_block_idx as usize;
                let col0_end = col0_start + cd0.num_blocks as usize;
                let mut cursor = col0_start + cursors[0];
                let mut expected_row = rg_start;
                while expected_row < rg_end {
                    if cursor >= col0_end {
                        return Err(StatamError::Corrupt {
                            reason: "column 0 blocks do not cover row group row range",
                        });
                    }
                    let blk = &block_dir[cursor];
                    if blk.row_count == 0 || blk.null_count > blk.row_count {
                        return Err(StatamError::Corrupt {
                            reason: "invalid mini-block row_count or null_count",
                        });
                    }
                    if u64::from(blk.first_row) != expected_row {
                        return Err(StatamError::Corrupt {
                            reason: "mini-block first_row not contiguous within row group",
                        });
                    }
                    let next_row = expected_row + u64::from(blk.row_count);
                    if next_row > rg_end {
                        return Err(StatamError::Corrupt {
                            reason: "mini-block extends past row group row range",
                        });
                    }
                    validate_block_in_rg(blk, rg, rg_wide_end)?;
                    expected_mbs.push((blk.first_row, blk.row_count));
                    expected_row = next_row;
                    cursor += 1;
                    cursors[0] += 1;
                }
            }

            for (pidx, cd) in col_dir.iter().enumerate().skip(1) {
                let col_start = cd.first_block_idx as usize;
                let col_end = col_start + cd.num_blocks as usize;
                let start_cursor = col_start + cursors[pidx];
                for (cursor, &(exp_first, exp_count)) in (start_cursor..).zip(expected_mbs.iter()) {
                    if cursor >= col_end {
                        return Err(StatamError::Corrupt {
                            reason: "physical column missing mini-blocks for row group",
                        });
                    }
                    let blk = &block_dir[cursor];
                    if blk.first_row != exp_first
                        || blk.row_count != exp_count
                        || blk.null_count > blk.row_count
                    {
                        return Err(StatamError::Corrupt {
                            reason: "physical column mini-block layout mismatch in row group",
                        });
                    }
                    validate_block_in_rg(blk, rg, rg_wide_end)?;
                    cursors[pidx] += 1;
                }
            }
        }
        for (pidx, cd) in col_dir.iter().enumerate() {
            if cursors[pidx] != cd.num_blocks as usize {
                return Err(StatamError::Corrupt {
                    reason: "column directory has blocks outside row groups",
                });
            }
        }

        let hot_bloom_bytes = Bytes::copy_from_slice(
            &raw_body
                [ps.hot_bloom_off as usize..ps.hot_bloom_off as usize + ps.hot_bloom_len as usize],
        );
        let hot_dict_bytes = Bytes::copy_from_slice(
            &raw_body
                [ps.hot_dict_off as usize..ps.hot_dict_off as usize + ps.hot_dict_len as usize],
        );

        let lazy_count = (ps.lazy_dir_len as usize) / LAZY_INDEX_ENTRY_LEN;
        let mut lazy_dir = Vec::with_capacity(lazy_count);
        for idx in 0..lazy_count {
            let off = ps.lazy_dir_off as usize + idx * LAZY_INDEX_ENTRY_LEN;
            let entry = LazyIndexEntry::decode(&raw_body[off..off + LAZY_INDEX_ENTRY_LEN])?;
            if (entry.phys_col_id as usize) >= num_phys_cols {
                return Err(StatamError::Corrupt {
                    reason: "lazy index phys_col_id out of bounds",
                });
            }
            let end = entry.offset.checked_add(u64::from(entry.len)).ok_or(
                StatamError::BoundsExceeded {
                    reason: "lazy index end offset overflow",
                },
            )?;
            if entry.offset < FILE_HEADER_LEN as u64 || end > hot_tail_start {
                return Err(StatamError::Corrupt {
                    reason: "lazy index section lies outside file data region",
                });
            }
            lazy_dir.push(entry);
        }

        for cd in &col_dir {
            if cd.col_flags & COL_FLAG_BLOOM_IN_HOT_TAIL != 0 && cd.bloom_len > 0 {
                let end = cd.bloom_offset.checked_add(u64::from(cd.bloom_len)).ok_or(
                    StatamError::BoundsExceeded {
                        reason: "hot bloom offset + len overflow",
                    },
                )?;
                if end > hot_bloom_bytes.len() as u64 {
                    return Err(StatamError::Corrupt {
                        reason: "column hot bloom out of bounds",
                    });
                }
            }
            if cd.col_flags & COL_FLAG_DICT_IN_HOT_TAIL != 0 && cd.dict_len > 0 {
                let end = cd.dict_offset.checked_add(u64::from(cd.dict_len)).ok_or(
                    StatamError::BoundsExceeded {
                        reason: "hot dict offset + len overflow",
                    },
                )?;
                if end > hot_dict_bytes.len() as u64 {
                    return Err(StatamError::Corrupt {
                        reason: "column hot dict out of bounds",
                    });
                }
            }
        }

        Ok(Self {
            file_len,
            postscript: ps,
            schema_meta,
            col_dir,
            rg_dir,
            block_dir,
            hot_bloom_bytes,
            hot_dict_bytes,
            lazy_dir,
            hot_tail_start,
        })
    }

    /// Returns the logical [`Schema`].
    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema_meta.schema
    }

    /// Total row count in the file.
    pub fn row_count(&self) -> u64 {
        self.postscript.row_count
    }

    /// Number of PAX Row Groups.
    pub fn num_row_groups(&self) -> usize {
        self.rg_dir.len()
    }

    /// Decodes a Hot-Tail-resident SBBF bloom for physical column `pidx`, if present.
    pub fn hot_bloom(&self, pidx: usize) -> Result<Option<Sbbf>, StatamError> {
        let Some(cd) = self.col_dir.get(pidx) else {
            return Ok(None);
        };
        if cd.bloom_len == 0 || (cd.col_flags & COL_FLAG_BLOOM_IN_HOT_TAIL) == 0 {
            return Ok(None);
        }
        let start = cd.bloom_offset as usize;
        let end = start + cd.bloom_len as usize;
        Ok(Some(Sbbf::from_bytes(&self.hot_bloom_bytes[start..end])?))
    }

    /// Decodes a Hot-Tail-resident `FileDict` for physical column `pidx`, if present.
    pub fn hot_dict(&self, pidx: usize) -> Result<Option<FileDict>, StatamError> {
        let Some(cd) = self.col_dir.get(pidx) else {
            return Ok(None);
        };
        if cd.dict_len == 0
            || (cd.col_flags & COL_FLAG_HAS_FILE_DICT) == 0
            || (cd.col_flags & COL_FLAG_DICT_IN_HOT_TAIL) == 0
        {
            return Ok(None);
        }
        let start = cd.dict_offset as usize;
        let end = start + cd.dict_len as usize;
        let sorted = (cd.col_flags & COL_FLAG_DICT_SORTED) != 0;
        Ok(Some(FileDict::decode(
            &self.hot_dict_bytes[start..end],
            sorted,
        )?))
    }

    /// Returns the `BlockEntry` slice belonging to Row Group `rg_idx` for physical column `pidx`.
    pub(crate) fn rg_blocks(&self, pidx: usize, rg_idx: usize) -> &[BlockEntry] {
        let cd = &self.col_dir[pidx];
        let rg = &self.rg_dir[rg_idx];
        let col_blocks = &self.block_dir
            [cd.first_block_idx as usize..(cd.first_block_idx + cd.num_blocks) as usize];
        let rg_start = rg.first_row;
        let rg_end = rg.first_row + rg.row_count;
        let lo = col_blocks.partition_point(|b| b.first_row < rg_start);
        let hi = col_blocks.partition_point(|b| b.first_row < rg_end);
        &col_blocks[lo..hi]
    }
}

fn validate_rg_bounds(rg: &RowGroupEntry, hot_tail_start: u64) -> Result<(), StatamError> {
    if rg.core_len as usize > MAX_BATCH_DECODED_BYTES
        || rg.wide_len as usize > MAX_BATCH_DECODED_BYTES
        || rg.l0_payload_len as usize > MAX_BATCH_DECODED_BYTES
    {
        return Err(StatamError::BoundsExceeded {
            reason: "row group region exceeds MAX_BATCH_DECODED_BYTES",
        });
    }
    if rg.core_offset < FILE_HEADER_LEN as u64 {
        return Err(StatamError::Corrupt {
            reason: "row group core_offset overlaps file header",
        });
    }
    let core_end =
        rg.core_offset
            .checked_add(u64::from(rg.core_len))
            .ok_or(StatamError::BoundsExceeded {
                reason: "row group core_end overflow",
            })?;
    if rg.wide_offset != core_end {
        return Err(StatamError::Corrupt {
            reason: "row group wide_offset not contiguous with core_offset + core_len",
        });
    }
    let wide_end =
        rg.wide_offset
            .checked_add(u64::from(rg.wide_len))
            .ok_or(StatamError::BoundsExceeded {
                reason: "row group wide end overflow",
            })?;
    if wide_end > hot_tail_start {
        return Err(StatamError::Corrupt {
            reason: "row group wide region overlaps hot tail",
        });
    }
    if rg.l0_payload_len > 0 {
        if rg.l0_payload_offset != wide_end {
            return Err(StatamError::Corrupt {
                reason: "row group l0_payload_offset not contiguous with wide_offset + wide_len",
            });
        }
        let l0_end = rg
            .l0_payload_offset
            .checked_add(u64::from(rg.l0_payload_len))
            .ok_or(StatamError::BoundsExceeded {
                reason: "row group l0_payload end overflow",
            })?;
        if l0_end > hot_tail_start {
            return Err(StatamError::Corrupt {
                reason: "row group l0_payload overlaps hot tail",
            });
        }
    }
    let total_end = core_end
        .checked_add(u64::from(rg.wide_len))
        .and_then(|v| v.checked_add(u64::from(rg.l0_payload_len)))
        .ok_or(StatamError::BoundsExceeded {
            reason: "row group total extent overflow",
        })?;
    if total_end > hot_tail_start {
        return Err(StatamError::Corrupt {
            reason: "row group total extent overlaps hot tail",
        });
    }
    Ok(())
}

fn validate_block_in_rg(
    blk: &BlockEntry,
    rg: &RowGroupEntry,
    rg_wide_end: u64,
) -> Result<(), StatamError> {
    let is_inline = matches!(blk.encoding, Encoding::Const | Encoding::ConstNull);
    if blk.stored_len > 0 || !is_inline {
        let total_blk = (BLOCK_FRAME_HEADER_LEN as u64)
            .checked_add(u64::from(blk.stored_len))
            .ok_or(StatamError::BoundsExceeded {
                reason: "block byte span overflow",
            })?;
        let blk_end = blk
            .offset
            .checked_add(total_blk)
            .ok_or(StatamError::BoundsExceeded {
                reason: "block end offset overflow",
            })?;
        if blk.offset < rg.core_offset || blk_end > rg_wide_end {
            return Err(StatamError::Corrupt {
                reason: "non-inline block lies outside owning row group bounds",
            });
        }
    }
    Ok(())
}

#[derive(Default)]
struct ReaderCaches {
    blooms: HashMap<usize, Sbbf>,
    dicts: HashMap<usize, Arc<FileDict>>,
    narrow_trigrams: HashMap<usize, Vec<Option<TrigramBloom>>>,
    packs: HashMap<[u8; 16], Arc<ChunkPackReader>>,
}

/// Range-based reader for `.statam` files.
pub struct StatamReader {
    reader: Arc<dyn RangeReader>,
    meta: StatamFileMeta,
    packs: Option<Arc<dyn PackResolver>>,
    caches: Mutex<ReaderCaches>,
}

impl fmt::Debug for StatamReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatamReader")
            .field("meta", &self.meta)
            .field("has_pack_resolver", &self.packs.is_some())
            .finish()
    }
}

impl StatamReader {
    /// Opens a `.statam` file by fetching its Hot Tail in 1 GET.
    pub fn open(
        reader: Arc<dyn RangeReader>,
        packs: Option<Arc<dyn PackResolver>>,
    ) -> Result<Self, StatamError> {
        let meta = StatamFileMeta::read_from(reader.as_ref(), None)?;
        Self::open_with_meta(reader, meta, packs)
    }

    /// Opens a `.statam` file using pre-read [`StatamFileMeta`] (0 extra GETs).
    pub fn open_with_meta(
        reader: Arc<dyn RangeReader>,
        meta: StatamFileMeta,
        packs: Option<Arc<dyn PackResolver>>,
    ) -> Result<Self, StatamError> {
        let mut caches = ReaderCaches::default();
        for pidx in 0..meta.col_dir.len() {
            if let Some(bloom) = meta.hot_bloom(pidx)? {
                caches.blooms.insert(pidx, bloom);
            }
            if let Some(dict) = meta.hot_dict(pidx)? {
                caches.dicts.insert(pidx, Arc::new(dict));
            }
        }
        Ok(Self {
            reader,
            meta,
            packs,
            caches: Mutex::new(caches),
        })
    }

    /// Returns the file's [`StatamFileMeta`].
    pub fn meta(&self) -> &StatamFileMeta {
        &self.meta
    }

    /// Performs full integrity verification: reads & checks `FileHeader`, every data block CRC,
    /// every `L0Payload` frame CRC, and every Lazy Index CRC.
    pub fn verify_full(&self) -> Result<(), StatamError> {
        let hdr_bytes = self.reader.read_range(0..FILE_HEADER_LEN as u64)?;
        if hdr_bytes.len() != FILE_HEADER_LEN {
            return Err(StatamError::Corrupt {
                reason: "short read on file header",
            });
        }
        let hdr = FileHeader::decode(&hdr_bytes)?;
        if hdr.flags != self.meta.postscript.flags
            || hdr.table_kind != self.meta.schema_meta.table_kind
            || hdr.checksum_alg != CHECKSUM_ALG_CRC32C
        {
            return Err(StatamError::Corrupt {
                reason: "file header metadata mismatch during verify_full",
            });
        }

        for blk in &self.meta.block_dir {
            if blk.stored_len == 0 {
                continue;
            }
            let total = (BLOCK_FRAME_HEADER_LEN as u64)
                .checked_add(u64::from(blk.stored_len))
                .ok_or(StatamError::BoundsExceeded {
                    reason: "block length overflow",
                })?;
            let end = blk
                .offset
                .checked_add(total)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "block end overflow",
                })?;
            let frame = self.reader.read_range(blk.offset..end)?;
            if frame.len() as u64 != total {
                return Err(StatamError::Corrupt {
                    reason: "short read on data block during verify_full",
                });
            }
            let (enc, codec, body) = read_framed_block(&frame, MAX_BLOCK_RAW_BYTES)?;
            if enc != blk.encoding || codec != blk.codec || body.len() != blk.raw_len as usize {
                return Err(StatamError::Corrupt {
                    reason: "block header mismatch during verify_full",
                });
            }
        }

        for rg in &self.meta.rg_dir {
            if rg.l0_payload_len > 0 {
                let end = rg
                    .l0_payload_offset
                    .checked_add(u64::from(rg.l0_payload_len))
                    .ok_or(StatamError::BoundsExceeded {
                        reason: "L0 payload end overflow",
                    })?;
                let bytes = self.reader.read_range(rg.l0_payload_offset..end)?;
                if bytes.len() != rg.l0_payload_len as usize {
                    return Err(StatamError::Corrupt {
                        reason: "short read on L0 payload during verify_full",
                    });
                }
                let l0 = ParsedL0Payload::parse(&bytes)?;
                l0.verify_all_frames()?;
            }
        }

        for lazy in &self.meta.lazy_dir {
            let end = lazy.offset.checked_add(u64::from(lazy.len)).ok_or(
                StatamError::BoundsExceeded {
                    reason: "lazy index end overflow",
                },
            )?;
            let bytes = self.reader.read_range(lazy.offset..end)?;
            if bytes.len() != lazy.len as usize {
                return Err(StatamError::Corrupt {
                    reason: "short read on lazy index during verify_full",
                });
            }
            if crc32c::crc32c(&bytes) != lazy.crc32c {
                return Err(StatamError::ChecksumMismatch {
                    context: "lazy index section",
                });
            }
        }

        Ok(())
    }

    /// Scans the file according to `req`, applying bloom pruning, zone-map/sort-key pruning,
    /// query-aware I/O coalescing (`Point` vs `Scan`), two-phase late materialization,
    /// `map_keys` selective projection, and exact substring search.
    pub fn scan(&self, req: &ScanRequest) -> Result<Vec<Batch>, StatamError> {
        let schema = &self.meta.schema_meta.schema;
        let num_logical = schema.len();

        let proj_indices: Vec<usize> = match &req.projection {
            Some(v) => {
                for &idx in v {
                    if idx >= num_logical {
                        return Err(StatamError::SchemaMismatch {
                            reason: "projection column index out of bounds",
                        });
                    }
                }
                v.clone()
            }
            None => (0..num_logical).collect(),
        };
        let proj_schema = Arc::new(schema.project(&proj_indices)?);

        if req.time_range.is_empty() || self.meta.postscript.row_count == 0 || req.limit == Some(0)
        {
            return Ok(Vec::new());
        }

        // Resolve query columns & early-exit on empty IN lists / contradictory bounds.
        let time_col_idx = if req.time_range.is_all() {
            None
        } else {
            find_time_column(schema)
        };

        let mut resolved_lookups = Vec::with_capacity(req.id_lookups.len());
        for lk in &req.id_lookups {
            if lk.values.is_empty() {
                return Ok(Vec::new());
            }
            let cidx = schema
                .index_of(&lk.column)
                .ok_or(StatamError::SchemaMismatch {
                    reason: "id_lookup column not found in schema",
                })?;
            resolved_lookups.push((cidx, lk));
        }

        let mut resolved_preds = Vec::with_capacity(req.predicates.len());
        for pred in &req.predicates {
            let cidx = schema
                .index_of(&pred.column)
                .ok_or(StatamError::SchemaMismatch {
                    reason: "predicate column not found in schema",
                })?;
            if pred.map_key.is_some() && schema.fields()[cidx].data_type != DataType::MapUtf8 {
                return Err(StatamError::SchemaMismatch {
                    reason: "map_key predicate on non-MapUtf8 column",
                });
            }
            if is_always_false_pred(&pred.pred) {
                return Ok(Vec::new());
            }
            resolved_preds.push((cidx, pred));
        }

        let mut resolved_subs = Vec::with_capacity(req.substrings.len());
        for sq in &req.substrings {
            let cidx = schema
                .index_of(&sq.column)
                .ok_or(StatamError::SchemaMismatch {
                    reason: "substring query column not found in schema",
                })?;
            if schema.fields()[cidx].data_type != DataType::Utf8 {
                return Err(StatamError::SchemaMismatch {
                    reason: "substring query on non-Utf8 column",
                });
            }
            resolved_subs.push((cidx, sq));
        }

        let mut map_keys_by_col: HashMap<usize, Vec<String>> = HashMap::new();
        for (col_name, key) in &req.map_keys {
            let cidx = schema
                .index_of(col_name)
                .ok_or(StatamError::SchemaMismatch {
                    reason: "map_keys column not found in schema",
                })?;
            if schema.fields()[cidx].data_type != DataType::MapUtf8 {
                return Err(StatamError::SchemaMismatch {
                    reason: "map_keys specified for non-MapUtf8 column",
                });
            }
            map_keys_by_col.entry(cidx).or_default().push(key.clone());
        }
        for keys in map_keys_by_col.values_mut() {
            keys.sort_unstable();
            keys.dedup();
        }

        // Step 1: File-level SBBF bloom check for id_lookups.
        if !self.check_blooms(&resolved_lookups)? {
            return Ok(Vec::new());
        }

        // Step 2: Determine RG iteration order & prune via sort key + zone maps.
        let num_rgs = self.meta.rg_dir.len();
        let mut rg_order: Vec<usize> = self.candidate_rg_range(
            time_col_idx,
            req.time_range,
            &resolved_lookups,
            &resolved_preds,
        );
        if req.sort_desc_hint && req.limit.is_some() {
            rg_order.reverse();
        }
        if rg_order.is_empty() && num_rgs > 0 {
            return Ok(Vec::new());
        }

        let mode = match req.coalesce_mode {
            CoalesceMode::Auto => {
                if !req.id_lookups.is_empty() {
                    CoalesceMode::Point
                } else {
                    CoalesceMode::Scan
                }
            }
            other => other,
        };

        // Pre-load narrow trigram indexes for any substring query columns that have one.
        let sub_cols_with_trigram: Vec<usize> = resolved_subs
            .iter()
            .filter_map(|&(cidx, _)| {
                ((self.meta.col_dir[cidx].col_flags & COL_FLAG_HAS_TRIGRAM_INDEX) != 0)
                    .then_some(cidx)
            })
            .collect();
        if !sub_cols_with_trigram.is_empty() {
            self.ensure_narrow_trigrams(&sub_cols_with_trigram)?;
        }

        // Pre-load lazy dictionaries needed for pruning & Phase 1 filter/substring evaluation ONCE.
        let mut preload_dict_cols = Vec::new();
        if let Some(tc) = time_col_idx {
            preload_dict_cols.push(tc);
        }
        for &(cidx, _) in &resolved_lookups {
            preload_dict_cols.push(cidx);
        }
        for &(cidx, pred) in &resolved_preds {
            preload_dict_cols.push(cidx);
            if let Some(key) = &pred.map_key
                && let Some((sub_pidx, _)) = self.find_shredded_subcol(cidx, key)
            {
                preload_dict_cols.push(sub_pidx);
            }
        }
        for &(cidx, _) in &resolved_subs {
            preload_dict_cols.push(cidx);
        }
        if !preload_dict_cols.is_empty() {
            self.ensure_dicts_loaded(&preload_dict_cols)?;
        }

        // Precompute matching-code Bitmap once per dict-encoded substring query.
        let dict_sub_bitmaps: Vec<Option<Bitmap>> = if resolved_subs.is_empty() {
            Vec::new()
        } else {
            let caches = self.caches.lock().unwrap_or_else(|p| p.into_inner());
            resolved_subs
                .iter()
                .map(|&(cidx, sq)| {
                    if sq.needle.is_empty() {
                        return None;
                    }
                    let dict = caches.dicts.get(&cidx)?;
                    let mut matcher = Utf8Matcher::new(&sq.needle, sq.case_insensitive);
                    let mut bm = Bitmap::new_zeroed(dict.len());
                    if let FlatData::Utf8(u) = dict.values().data() {
                        for i in 0..u.len() {
                            if let Some(s) = u.get(i)
                                && matcher.matches(s.as_bytes())
                            {
                                let _ = bm.set(i, true);
                            }
                        }
                    }
                    Some(bm)
                })
                .collect()
        };

        let desc_limit = req.sort_desc_hint && req.limit.is_some();
        let has_filters = time_col_idx.is_some()
            || !resolved_lookups.is_empty()
            || !resolved_preds.is_empty()
            || !resolved_subs.is_empty();

        let mut out_batches = Vec::new();
        let mut emitted_rows = 0usize;

        for rg_idx in rg_order {
            if let Some(lim) = req.limit
                && emitted_rows >= lim
            {
                break;
            }

            let rg = &self.meta.rg_dir[rg_idx];
            let mb_count = self.meta.rg_blocks(0, rg_idx).len();
            if mb_count == 0 {
                continue;
            }

            // Metadata pruning across mini-blocks of RG `rg_idx`.
            let mut surviving_mbs = self.prune_rg_mini_blocks(
                rg_idx,
                mb_count,
                time_col_idx,
                req.time_range,
                &resolved_lookups,
                &resolved_preds,
                &resolved_subs,
                &dict_sub_bitmaps,
            )?;
            if surviving_mbs.is_empty() {
                continue;
            }

            if desc_limit {
                surviving_mbs.reverse();
            } else if let Some(lim) = req.limit
                && !has_filters
            {
                let remaining = lim.saturating_sub(emitted_rows);
                let mut covered = 0usize;
                let mut keep_cnt = 0usize;
                let blks0 = self.meta.rg_blocks(0, rg_idx);
                for &mb in &surviving_mbs {
                    keep_cnt += 1;
                    covered += blks0[mb].row_count as usize;
                    if covered >= remaining {
                        break;
                    }
                }
                surviving_mbs.truncate(keep_cnt);
            }

            if desc_limit && mode != CoalesceMode::Point {
                // Iterate mini-blocks one by one in reverse order and stop immediately on limit.
                for &mb_idx in &surviving_mbs {
                    let Some(lim) = req.limit else {
                        break;
                    };
                    let remaining = lim.saturating_sub(emitted_rows);
                    if remaining == 0 {
                        break;
                    }
                    let mb_batches = self.scan_rg_late_mat(
                        rg_idx,
                        rg,
                        &[mb_idx],
                        &proj_indices,
                        &proj_schema,
                        &map_keys_by_col,
                        time_col_idx,
                        req.time_range,
                        &resolved_lookups,
                        &resolved_preds,
                        &resolved_subs,
                        &dict_sub_bitmaps,
                    )?;
                    for batch in mb_batches {
                        if batch.is_empty() {
                            continue;
                        }
                        let rem = lim.saturating_sub(emitted_rows);
                        if rem == 0 {
                            break;
                        }
                        if batch.num_rows() > rem {
                            let truncated = truncate_batch_selection_tail(&batch, rem)?;
                            emitted_rows += truncated.num_rows();
                            out_batches.push(truncated);
                            break;
                        }
                        emitted_rows += batch.num_rows();
                        out_batches.push(batch);
                    }
                    if emitted_rows >= lim {
                        break;
                    }
                }
                continue;
            }

            let rg_batches = match mode {
                CoalesceMode::Point => self.scan_rg_point(
                    rg_idx,
                    rg,
                    &surviving_mbs,
                    &proj_indices,
                    &proj_schema,
                    &map_keys_by_col,
                    time_col_idx,
                    req.time_range,
                    &resolved_lookups,
                    &resolved_preds,
                    &resolved_subs,
                    &dict_sub_bitmaps,
                    req.limit.map(|lim| lim.saturating_sub(emitted_rows)),
                )?,
                CoalesceMode::Scan | CoalesceMode::Auto => self.scan_rg_late_mat(
                    rg_idx,
                    rg,
                    &surviving_mbs,
                    &proj_indices,
                    &proj_schema,
                    &map_keys_by_col,
                    time_col_idx,
                    req.time_range,
                    &resolved_lookups,
                    &resolved_preds,
                    &resolved_subs,
                    &dict_sub_bitmaps,
                )?,
            };

            for batch in rg_batches {
                if batch.is_empty() {
                    continue;
                }
                if let Some(lim) = req.limit {
                    let remaining = lim.saturating_sub(emitted_rows);
                    if remaining == 0 {
                        break;
                    }
                    if batch.num_rows() > remaining {
                        let truncated = if desc_limit {
                            truncate_batch_selection_tail(&batch, remaining)?
                        } else {
                            truncate_batch_selection(&batch, remaining)?
                        };
                        emitted_rows += truncated.num_rows();
                        out_batches.push(truncated);
                        break;
                    }
                }
                emitted_rows += batch.num_rows();
                out_batches.push(batch);
            }
        }

        Ok(out_batches)
    }

    fn check_blooms(&self, lookups: &[(usize, &IdLookup)]) -> Result<bool, StatamError> {
        let mut missing_lazy: Vec<(usize, LazyIndexEntry)> = Vec::new();
        {
            let guard = self.caches.lock().unwrap_or_else(|p| p.into_inner());
            for &(cidx, _) in lookups {
                let cd = &self.meta.col_dir[cidx];
                if cd.bloom_len > 0
                    && !guard.blooms.contains_key(&cidx)
                    && let Some(entry) =
                        self.meta.lazy_dir.iter().find(|e| {
                            e.kind == LazyIndexKind::Bloom && e.phys_col_id as usize == cidx
                        })
                {
                    missing_lazy.push((cidx, *entry));
                }
            }
        }

        if !missing_lazy.is_empty() {
            let ranges: Vec<Range<u64>> = missing_lazy
                .iter()
                .map(|(_, e)| e.offset..e.offset + u64::from(e.len))
                .collect();
            let parts = self.reader.read_ranges(&ranges)?;
            if parts.len() != missing_lazy.len() {
                return Err(StatamError::Corrupt {
                    reason: "read_ranges returned unexpected bloom count",
                });
            }
            let mut guard = self.caches.lock().unwrap_or_else(|p| p.into_inner());
            for ((cidx, entry), bytes) in missing_lazy.into_iter().zip(parts) {
                if bytes.len() != entry.len as usize {
                    return Err(StatamError::Corrupt {
                        reason: "short read on lazy bloom filter",
                    });
                }
                if crc32c::crc32c(&bytes) != entry.crc32c {
                    return Err(StatamError::ChecksumMismatch {
                        context: "lazy bloom filter",
                    });
                }
                let sbbf = Sbbf::from_bytes(&bytes)?;
                guard.blooms.insert(cidx, sbbf);
            }
        }

        let guard = self.caches.lock().unwrap_or_else(|p| p.into_inner());
        for &(cidx, lk) in lookups {
            if let Some(sbbf) = guard.blooms.get(&cidx) {
                let any_hit = lk
                    .values
                    .iter()
                    .any(|v| sbbf.contains_hash(statam_bloom_hash(v)));
                if !any_hit {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    fn ensure_narrow_trigrams(&self, cols: &[usize]) -> Result<(), StatamError> {
        let mut missing: Vec<(usize, LazyIndexEntry)> = Vec::new();
        {
            let guard = self.caches.lock().unwrap_or_else(|p| p.into_inner());
            for &cidx in cols {
                if !guard.narrow_trigrams.contains_key(&cidx)
                    && let Some(entry) = self.meta.lazy_dir.iter().find(|e| {
                        e.kind == LazyIndexKind::NarrowTrigram && e.phys_col_id as usize == cidx
                    })
                {
                    missing.push((cidx, *entry));
                }
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        let ranges: Vec<Range<u64>> = missing
            .iter()
            .map(|(_, e)| e.offset..e.offset + u64::from(e.len))
            .collect();
        let parts = self.reader.read_ranges(&ranges)?;
        if parts.len() != missing.len() {
            return Err(StatamError::Corrupt {
                reason: "read_ranges returned unexpected narrow trigram count",
            });
        }
        let mut guard = self.caches.lock().unwrap_or_else(|p| p.into_inner());
        for ((cidx, entry), bytes) in missing.into_iter().zip(parts) {
            if bytes.len() != entry.len as usize {
                return Err(StatamError::Corrupt {
                    reason: "short read on narrow trigram index",
                });
            }
            if crc32c::crc32c(&bytes) != entry.crc32c {
                return Err(StatamError::ChecksumMismatch {
                    context: "narrow trigram index",
                });
            }
            let blooms =
                decode_narrow_trigram_section(&bytes, self.meta.col_dir[cidx].num_blocks as usize)?;
            guard.narrow_trigrams.insert(cidx, blooms);
        }
        Ok(())
    }

    fn ensure_dicts_loaded(&self, phys_cols: &[usize]) -> Result<(), StatamError> {
        let mut missing: Vec<(usize, LazyIndexEntry, bool)> = Vec::new();
        {
            let guard = self.caches.lock().unwrap_or_else(|p| p.into_inner());
            for &pidx in phys_cols {
                let cd = &self.meta.col_dir[pidx];
                if (cd.col_flags & COL_FLAG_HAS_FILE_DICT) != 0
                    && !guard.dicts.contains_key(&pidx)
                    && !missing.iter().any(|&(m_idx, _, _)| m_idx == pidx)
                    && let Some(entry) =
                        self.meta.lazy_dir.iter().find(|e| {
                            e.kind == LazyIndexKind::Dict && e.phys_col_id as usize == pidx
                        })
                {
                    let sorted = (cd.col_flags & COL_FLAG_DICT_SORTED) != 0;
                    missing.push((pidx, *entry, sorted));
                }
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        let ranges: Vec<Range<u64>> = missing
            .iter()
            .map(|(_, e, _)| e.offset..e.offset + u64::from(e.len))
            .collect();
        let parts = self.reader.read_ranges(&ranges)?;
        if parts.len() != missing.len() {
            return Err(StatamError::Corrupt {
                reason: "read_ranges returned unexpected lazy dict count",
            });
        }
        let mut guard = self.caches.lock().unwrap_or_else(|p| p.into_inner());
        for ((pidx, entry, sorted), bytes) in missing.into_iter().zip(parts) {
            if bytes.len() != entry.len as usize {
                return Err(StatamError::Corrupt {
                    reason: "short read on lazy dictionary",
                });
            }
            if crc32c::crc32c(&bytes) != entry.crc32c {
                return Err(StatamError::ChecksumMismatch {
                    context: "lazy dictionary",
                });
            }
            let dict = Arc::new(FileDict::decode(&bytes, sorted)?);
            guard.dicts.insert(pidx, dict);
        }
        Ok(())
    }

    fn get_cached_dict(&self, pidx: usize) -> Option<Arc<FileDict>> {
        self.caches
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .dicts
            .get(&pidx)
            .map(Arc::clone)
    }

    fn resolve_pack(&self, pack_id: [u8; 16]) -> Result<Arc<ChunkPackReader>, StatamError> {
        {
            let guard = self.caches.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(p) = guard.packs.get(&pack_id) {
                return Ok(Arc::clone(p));
            }
        }
        let resolver = self
            .packs
            .as_ref()
            .ok_or(StatamError::MissingPack { pack_id })?;
        let pack = resolver.resolve(pack_id)?;
        let mut guard = self.caches.lock().unwrap_or_else(|p| p.into_inner());
        guard.packs.insert(pack_id, Arc::clone(&pack));
        Ok(pack)
    }

    fn candidate_rg_range(
        &self,
        time_col_idx: Option<usize>,
        time_range: TimeRange,
        lookups: &[(usize, &IdLookup)],
        preds: &[(usize, &Predicate)],
    ) -> Vec<usize> {
        let n = self.meta.rg_dir.len();
        if n == 0 {
            return Vec::new();
        }
        let Some(&sc0) = self.meta.schema_meta.sort_cols.first() else {
            return (0..n).collect();
        };
        let sc0_idx = sc0 as usize;
        if n <= 16 {
            return (0..n).collect();
        }

        // Binary-search RG bounds on sort_cols[0] when it is an I64/TimestampNs column
        // with strictly valid non-null min/max across every RG.
        let dt = &self.meta.schema_meta.schema.fields()[sc0_idx].data_type;
        if !matches!(dt, DataType::I64 | DataType::TimestampNs { .. }) {
            return (0..n).collect();
        }

        let mut lo_bound = i64::MIN;
        let mut hi_bound = i64::MAX;
        if time_col_idx == Some(sc0_idx) {
            lo_bound = lo_bound.max(time_range.min_ns);
            hi_bound = hi_bound.min(time_range.max_ns);
        }
        for &(cidx, p) in preds {
            if cidx == sc0_idx && p.map_key.is_none() {
                match &p.pred {
                    ZonePred::Eq(ZoneValue::I64(v) | ZoneValue::TimestampNs(v)) => {
                        lo_bound = lo_bound.max(*v);
                        hi_bound = hi_bound.min(*v);
                    }
                    ZonePred::Between(
                        ZoneValue::I64(a) | ZoneValue::TimestampNs(a),
                        ZoneValue::I64(b) | ZoneValue::TimestampNs(b),
                    ) => {
                        lo_bound = lo_bound.max(*a);
                        hi_bound = hi_bound.min(*b);
                    }
                    ZonePred::Ge(ZoneValue::I64(v) | ZoneValue::TimestampNs(v)) => {
                        lo_bound = lo_bound.max(*v);
                    }
                    ZonePred::Gt(ZoneValue::I64(v) | ZoneValue::TimestampNs(v)) => {
                        lo_bound = lo_bound.max(v.saturating_add(1));
                    }
                    ZonePred::Le(ZoneValue::I64(v) | ZoneValue::TimestampNs(v)) => {
                        hi_bound = hi_bound.min(*v);
                    }
                    ZonePred::Lt(ZoneValue::I64(v) | ZoneValue::TimestampNs(v)) => {
                        hi_bound = hi_bound.min(v.saturating_sub(1));
                    }
                    _ => {}
                }
            }
        }
        let _ = lookups;
        if lo_bound == i64::MIN && hi_bound == i64::MAX {
            return (0..n).collect();
        }
        if lo_bound > hi_bound {
            return Vec::new();
        }

        // Verify all RGs are non-empty, have valid non-null min/max on sc0, and are
        // strictly non-decreasing non-overlapping (`rg_min >= prev_max`) before binary searching.
        let mut rg_ranges = Vec::with_capacity(n);
        for rg_idx in 0..n {
            let blks = self.meta.rg_blocks(sc0_idx, rg_idx);
            if blks.is_empty() {
                return (0..n).collect();
            }
            let mut rg_min = i64::MAX;
            let mut rg_max = i64::MIN;
            for b in blks {
                if b.null_count > 0
                    || (b.block_flags & BLOCK_FLAG_MIN_MAX_VALID) == 0
                    || b.min_inline_len < 8
                    || b.max_inline_len < 8
                {
                    return (0..n).collect();
                }
                let bmin = i64::from_le_bytes(b.min_inline[0..8].try_into().unwrap_or([0; 8]));
                let bmax = i64::from_le_bytes(b.max_inline[0..8].try_into().unwrap_or([0; 8]));
                if bmin > bmax {
                    return (0..n).collect();
                }
                rg_min = rg_min.min(bmin);
                rg_max = rg_max.max(bmax);
            }
            if rg_idx > 0 {
                let prev: (i64, i64) = rg_ranges[rg_idx - 1];
                if rg_min < prev.1 {
                    return (0..n).collect();
                }
            }
            rg_ranges.push((rg_min, rg_max));
        }

        let start = rg_ranges.partition_point(|&(_, mx)| mx < lo_bound);
        let end = rg_ranges.partition_point(|&(mn, _)| mn <= hi_bound);
        if start >= end {
            Vec::new()
        } else {
            (start..end).collect()
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn prune_rg_mini_blocks(
        &self,
        rg_idx: usize,
        mb_count: usize,
        time_col_idx: Option<usize>,
        time_range: TimeRange,
        lookups: &[(usize, &IdLookup)],
        preds: &[(usize, &Predicate)],
        subs: &[(usize, &SubstringQuery)],
        dict_sub_bitmaps: &[Option<Bitmap>],
    ) -> Result<Vec<usize>, StatamError> {
        let fields = self.meta.schema_meta.schema.fields();
        let caches = self.caches.lock().unwrap_or_else(|p| p.into_inner());
        let mut surviving = Vec::with_capacity(mb_count);

        for mb_idx in 0..mb_count {
            let mut keep = true;

            if let Some(tc_idx) = time_col_idx {
                let blk = &self.meta.rg_blocks(tc_idx, rg_idx)[mb_idx];
                let zp = ZonePred::Between(
                    ZoneValue::TimestampNs(time_range.min_ns),
                    ZoneValue::TimestampNs(time_range.max_ns),
                );
                if !zone_map_may_match(
                    blk,
                    &fields[tc_idx].data_type,
                    self.meta.col_dir[tc_idx].col_flags,
                    &zp,
                    caches.dicts.get(&tc_idx).map(Arc::as_ref),
                ) {
                    keep = false;
                }
            }

            if keep {
                for &(cidx, lk) in lookups {
                    let blk = &self.meta.rg_blocks(cidx, rg_idx)[mb_idx];
                    let zp = id_lookup_zone_pred(&fields[cidx].data_type, &lk.values);
                    if let Some(pred) = zp
                        && !zone_map_may_match(
                            blk,
                            &fields[cidx].data_type,
                            self.meta.col_dir[cidx].col_flags,
                            &pred,
                            caches.dicts.get(&cidx).map(Arc::as_ref),
                        )
                    {
                        keep = false;
                        break;
                    }
                }
            }

            if keep {
                for &(cidx, pred) in preds {
                    if pred.map_key.is_some() {
                        continue;
                    }
                    let blk = &self.meta.rg_blocks(cidx, rg_idx)[mb_idx];
                    if !zone_map_may_match(
                        blk,
                        &fields[cidx].data_type,
                        self.meta.col_dir[cidx].col_flags,
                        &pred.pred,
                        caches.dicts.get(&cidx).map(Arc::as_ref),
                    ) {
                        keep = false;
                        break;
                    }
                }
            }

            if keep {
                for (sq_idx, &(cidx, sq)) in subs.iter().enumerate() {
                    let blk = &self.meta.rg_blocks(cidx, rg_idx)[mb_idx];
                    if blk.null_count == blk.row_count {
                        keep = false;
                        break;
                    }
                    if !sq.needle.is_empty() {
                        if blk.encoding == Encoding::Dict
                            && let Some(Some(code_bm)) = dict_sub_bitmaps.get(sq_idx)
                        {
                            if code_bm.all_zeros() {
                                keep = false;
                                break;
                            }
                            if (blk.block_flags & BLOCK_FLAG_MIN_MAX_VALID) != 0
                                && blk.min_inline_len >= 4
                                && blk.max_inline_len >= 4
                            {
                                let min_code = u32::from_le_bytes([
                                    blk.min_inline[0],
                                    blk.min_inline[1],
                                    blk.min_inline[2],
                                    blk.min_inline[3],
                                ]) as usize;
                                let max_code = u32::from_le_bytes([
                                    blk.max_inline[0],
                                    blk.max_inline[1],
                                    blk.max_inline[2],
                                    blk.max_inline[3],
                                ]) as usize;
                                let hi = max_code.min(code_bm.len().saturating_sub(1));
                                let any_in_range =
                                    min_code <= hi && (min_code..=hi).any(|c| code_bm.get(c));
                                if !any_in_range {
                                    keep = false;
                                    break;
                                }
                            }
                        } else if let Some(tri_vec) = caches.narrow_trigrams.get(&cidx) {
                            let global_mb_idx = self.global_mb_idx(cidx, rg_idx, mb_idx);
                            if let Some(Some(bloom)) = tri_vec.get(global_mb_idx)
                                && (!sq.case_insensitive || sq.needle.is_ascii())
                            {
                                let mut hashes = Vec::new();
                                extract_trigram_hashes(
                                    &sq.needle,
                                    sq.case_insensitive,
                                    &mut hashes,
                                );
                                if !hashes.is_empty() && !bloom.may_contain_hashes(&hashes) {
                                    keep = false;
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            if keep {
                surviving.push(mb_idx);
            }
        }
        Ok(surviving)
    }

    fn global_mb_idx(&self, pidx: usize, rg_idx: usize, mb_in_rg: usize) -> usize {
        let cd = &self.meta.col_dir[pidx];
        let rg = &self.meta.rg_dir[rg_idx];
        let col_blocks = &self.meta.block_dir
            [cd.first_block_idx as usize..(cd.first_block_idx + cd.num_blocks) as usize];
        let lo = col_blocks.partition_point(|b| b.first_row < rg.first_row);
        lo + mb_in_rg
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_rg_point(
        &self,
        rg_idx: usize,
        rg: &RowGroupEntry,
        surviving_mbs: &[usize],
        proj_indices: &[usize],
        proj_schema: &Arc<Schema>,
        map_keys_by_col: &HashMap<usize, Vec<String>>,
        time_col_idx: Option<usize>,
        time_range: TimeRange,
        lookups: &[(usize, &IdLookup)],
        preds: &[(usize, &Predicate)],
        subs: &[(usize, &SubstringQuery)],
        dict_sub_bitmaps: &[Option<Bitmap>],
        limit_remaining: Option<usize>,
    ) -> Result<Vec<Batch>, StatamError> {
        let plan = self.build_rg_io_plan(
            rg_idx,
            surviving_mbs,
            proj_indices,
            map_keys_by_col,
            time_col_idx,
            lookups,
            preds,
            subs,
        );

        // Single contiguous RG byte range fetch!
        let mut need_wide = false;
        let mut need_l0_payload = false;
        for &pidx in plan.all_phys_cols() {
            if self.meta.col_dir[pidx].locality_group != LocalityGroup::Core {
                need_wide = true;
            }
            if self.meta.col_dir[pidx].locality_group == LocalityGroup::ExternalPayload
                && rg.l0_payload_len > 0
            {
                need_l0_payload = true;
            }
        }

        // Load only lazy dicts actually referenced by Encoding::Dict blocks in surviving MBs.
        let dict_cols: Vec<usize> = plan
            .all_phys_cols()
            .copied()
            .filter(|&pidx| {
                (self.meta.col_dir[pidx].col_flags & COL_FLAG_HAS_FILE_DICT) != 0
                    && surviving_mbs
                        .iter()
                        .any(|&mb| self.meta.rg_blocks(pidx, rg_idx)[mb].encoding == Encoding::Dict)
            })
            .collect();
        if !dict_cols.is_empty() {
            self.ensure_dicts_loaded(&dict_cols)?;
        }

        let range_start = rg.core_offset;
        let range_end = if need_l0_payload {
            rg.l0_payload_offset
                .checked_add(u64::from(rg.l0_payload_len))
                .ok_or(StatamError::BoundsExceeded {
                    reason: "point RG l0_payload end overflow",
                })?
        } else if need_wide {
            rg.wide_offset.checked_add(u64::from(rg.wide_len)).ok_or(
                StatamError::BoundsExceeded {
                    reason: "point RG wide end overflow",
                },
            )?
        } else {
            rg.core_offset.checked_add(u64::from(rg.core_len)).ok_or(
                StatamError::BoundsExceeded {
                    reason: "point RG core end overflow",
                },
            )?
        };
        let expected_range_len =
            range_end
                .checked_sub(range_start)
                .ok_or(StatamError::Corrupt {
                    reason: "invalid Point RG byte range",
                })?;

        let rg_slice = if range_end > range_start {
            let bytes = self.reader.read_range(range_start..range_end)?;
            if bytes.len() as u64 != expected_range_len {
                return Err(StatamError::Corrupt {
                    reason: "short read on Point RG range",
                });
            }
            bytes
        } else {
            Bytes::new()
        };

        let l0_parsed = if need_l0_payload && rg.l0_payload_len > 0 {
            let rel_start = rg
                .l0_payload_offset
                .checked_sub(range_start)
                .and_then(|v| usize::try_from(v).ok())
                .ok_or(StatamError::Corrupt {
                    reason: "L0 payload offset before Point RG range",
                })?;
            let rel_end = rel_start.checked_add(rg.l0_payload_len as usize).ok_or(
                StatamError::BoundsExceeded {
                    reason: "L0 payload end overflows usize",
                },
            )?;
            let l0_slice = rg_slice
                .get(rel_start..rel_end)
                .ok_or(StatamError::Corrupt {
                    reason: "L0 payload slice out of Point RG range",
                })?;
            Some(ParsedL0Payload::parse(l0_slice)?)
        } else {
            None
        };

        let block_lookup = |blk: &BlockEntry| -> Result<Vec<u8>, StatamError> {
            if blk.stored_len == 0 {
                return Ok(Vec::new());
            }
            let rel = blk
                .offset
                .checked_sub(range_start)
                .and_then(|v| usize::try_from(v).ok())
                .ok_or(StatamError::Corrupt {
                    reason: "block offset before Point RG range",
                })?;
            let total = BLOCK_FRAME_HEADER_LEN
                .checked_add(blk.stored_len as usize)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "block total length overflow",
                })?;
            let end = rel.checked_add(total).ok_or(StatamError::BoundsExceeded {
                reason: "block end offset overflow",
            })?;
            let frame = rg_slice.get(rel..end).ok_or(StatamError::Corrupt {
                reason: "block slice out of Point RG range",
            })?;
            let (_, _, body) = read_framed_block(frame, MAX_BLOCK_RAW_BYTES)?;
            Ok(body)
        };

        let mut active_mbs: Vec<(usize, Bitmap, HashMap<usize, DecodedPhase1>)> = Vec::new();
        let mut active_rows = 0usize;
        for &mb_idx in surviving_mbs {
            let (mask, decoded_p1) = self.eval_phase1_mb(
                rg_idx,
                mb_idx,
                &plan,
                time_col_idx,
                time_range,
                lookups,
                preds,
                subs,
                dict_sub_bitmaps,
                l0_parsed.as_ref(),
                &block_lookup,
            )?;
            let hits = mask.count_ones();
            if hits > 0 {
                active_rows = active_rows.saturating_add(hits);
                active_mbs.push((mb_idx, mask, decoded_p1));
                if let Some(lim) = limit_remaining
                    && active_rows >= lim
                {
                    break;
                }
            }
        }
        if active_mbs.is_empty() {
            return Ok(Vec::new());
        }

        let rg_resolved_chunks = self.resolve_rg_payload_chunks(
            rg_idx,
            &mut active_mbs,
            proj_indices,
            l0_parsed.as_ref(),
            &block_lookup,
        )?;

        let mut batches = Vec::with_capacity(active_mbs.len());
        for (mb_idx, mask, decoded_p1) in active_mbs {
            let batch = self.materialize_phase2_mb(
                rg_idx,
                mb_idx,
                &mask,
                decoded_p1,
                &plan,
                proj_indices,
                proj_schema,
                map_keys_by_col,
                &rg_resolved_chunks,
                &block_lookup,
            )?;
            batches.push(batch);
        }
        Ok(batches)
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_rg_late_mat(
        &self,
        rg_idx: usize,
        rg: &RowGroupEntry,
        surviving_mbs: &[usize],
        proj_indices: &[usize],
        proj_schema: &Arc<Schema>,
        map_keys_by_col: &HashMap<usize, Vec<String>>,
        time_col_idx: Option<usize>,
        time_range: TimeRange,
        lookups: &[(usize, &IdLookup)],
        preds: &[(usize, &Predicate)],
        subs: &[(usize, &SubstringQuery)],
        dict_sub_bitmaps: &[Option<Bitmap>],
    ) -> Result<Vec<Batch>, StatamError> {
        let plan = self.build_rg_io_plan(
            rg_idx,
            surviving_mbs,
            proj_indices,
            map_keys_by_col,
            time_col_idx,
            lookups,
            preds,
            subs,
        );

        // Phase 1: Coalesce and fetch ONLY filter/substring columns.
        let mut p1_ranges = Vec::new();
        for &mb_idx in surviving_mbs {
            for &pidx in &plan.phase1_phys_cols {
                let blk = &self.meta.rg_blocks(pidx, rg_idx)[mb_idx];
                if blk.stored_len > 0 {
                    let total = (BLOCK_FRAME_HEADER_LEN as u64)
                        .checked_add(u64::from(blk.stored_len))
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "Phase 1 block length overflow",
                        })?;
                    let end = blk
                        .offset
                        .checked_add(total)
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "Phase 1 block end overflow",
                        })?;
                    p1_ranges.push((blk.offset, end));
                }
            }
        }

        let mut p1_l0_needed = false;
        if rg.l0_payload_len > 0 {
            for &pidx in &plan.phase1_phys_cols {
                if self.meta.col_dir[pidx].locality_group == LocalityGroup::ExternalPayload {
                    p1_l0_needed = true;
                    break;
                }
            }
        }
        let p1_l0_end = rg
            .l0_payload_offset
            .checked_add(u64::from(rg.l0_payload_len))
            .ok_or(StatamError::BoundsExceeded {
                reason: "Phase 1 L0 payload end overflow",
            })?;
        if p1_l0_needed {
            p1_ranges.push((rg.l0_payload_offset, p1_l0_end));
        }

        let p1_cache = fetch_coalesced_blocks(self.reader.as_ref(), &p1_ranges)?;
        let l0_from_p1 = if p1_l0_needed {
            p1_cache
                .get(&(rg.l0_payload_offset, p1_l0_end))
                .map(|b| ParsedL0Payload::parse(b))
                .transpose()?
        } else {
            None
        };

        // Evaluate Phase 1 on each surviving mini-block.
        let mut active_mbs: Vec<(usize, Bitmap, HashMap<usize, DecodedPhase1>)> = Vec::new();
        for &mb_idx in surviving_mbs {
            let p1_lookup = |blk: &BlockEntry| -> Result<Vec<u8>, StatamError> {
                if blk.stored_len == 0 {
                    return Ok(Vec::new());
                }
                let end = blk
                    .offset
                    .checked_add(BLOCK_FRAME_HEADER_LEN as u64 + u64::from(blk.stored_len))
                    .ok_or(StatamError::BoundsExceeded {
                        reason: "Phase 1 block key overflow",
                    })?;
                let key = (blk.offset, end);
                let frame = p1_cache.get(&key).ok_or(StatamError::Corrupt {
                    reason: "missing Phase 1 fetched block",
                })?;
                let (_, _, body) = read_framed_block(frame, MAX_BLOCK_RAW_BYTES)?;
                Ok(body)
            };

            let (mask, decoded_p1) = self.eval_phase1_mb(
                rg_idx,
                mb_idx,
                &plan,
                time_col_idx,
                time_range,
                lookups,
                preds,
                subs,
                dict_sub_bitmaps,
                l0_from_p1.as_ref(),
                &p1_lookup,
            )?;
            if mask.count_ones() > 0 {
                active_mbs.push((mb_idx, mask, decoded_p1));
            }
        }

        if active_mbs.is_empty() {
            return Ok(Vec::new());
        }

        // Load lazy dicts for Phase 2 columns ONLY if an active mini-block actually uses Encoding::Dict.
        let p2_dict_cols: Vec<usize> = plan
            .phase2_only_phys_cols
            .iter()
            .copied()
            .filter(|&pidx| {
                (self.meta.col_dir[pidx].col_flags & COL_FLAG_HAS_FILE_DICT) != 0
                    && active_mbs.iter().any(|(mb, _, _)| {
                        self.meta.rg_blocks(pidx, rg_idx)[*mb].encoding == Encoding::Dict
                    })
            })
            .collect();
        if !p2_dict_cols.is_empty() {
            self.ensure_dicts_loaded(&p2_dict_cols)?;
        }

        // Phase 2: Coalesce and fetch ONLY remaining projected columns for active mini-blocks!
        let mut p2_ranges = Vec::new();
        let mut p2_l0_needed = false;
        for (mb_idx, _, _) in &active_mbs {
            for &pidx in &plan.phase2_only_phys_cols {
                // Skip residual blocks on mini-blocks where Phase 2 projection skips residual!
                if self
                    .meta
                    .schema_meta
                    .schema
                    .fields()
                    .get(pidx)
                    .is_some_and(|f| f.data_type == DataType::MapUtf8)
                    && plan.phase2_map_fast_path.contains(&(*mb_idx, pidx))
                {
                    continue;
                }
                let blk = &self.meta.rg_blocks(pidx, rg_idx)[*mb_idx];
                if blk.stored_len > 0 {
                    let total = (BLOCK_FRAME_HEADER_LEN as u64)
                        .checked_add(u64::from(blk.stored_len))
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "Phase 2 block length overflow",
                        })?;
                    let end = blk
                        .offset
                        .checked_add(total)
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "Phase 2 block end overflow",
                        })?;
                    p2_ranges.push((blk.offset, end));
                }
                if self.meta.col_dir[pidx].locality_group == LocalityGroup::ExternalPayload
                    && rg.l0_payload_len > 0
                    && !p1_l0_needed
                {
                    p2_l0_needed = true;
                }
            }
        }
        if p2_l0_needed {
            p2_ranges.push((rg.l0_payload_offset, p1_l0_end));
        }

        let p2_cache = fetch_coalesced_blocks(self.reader.as_ref(), &p2_ranges)?;
        let l0_from_p2 = if p2_l0_needed {
            p2_cache
                .get(&(rg.l0_payload_offset, p1_l0_end))
                .map(|b| ParsedL0Payload::parse(b))
                .transpose()?
        } else {
            None
        };
        let l0_ref = l0_from_p1.as_ref().or(l0_from_p2.as_ref());

        let p2_lookup =
            |blk: &BlockEntry| -> Result<Vec<u8>, StatamError> {
                if blk.stored_len == 0 {
                    return Ok(Vec::new());
                }
                let end = blk
                    .offset
                    .checked_add(BLOCK_FRAME_HEADER_LEN as u64 + u64::from(blk.stored_len))
                    .ok_or(StatamError::BoundsExceeded {
                        reason: "Phase 2 block key overflow",
                    })?;
                let key = (blk.offset, end);
                let frame = p2_cache.get(&key).or_else(|| p1_cache.get(&key)).ok_or(
                    StatamError::Corrupt {
                        reason: "missing Phase 2 fetched block",
                    },
                )?;
                let (_, _, body) = read_framed_block(frame, MAX_BLOCK_RAW_BYTES)?;
                Ok(body)
            };

        // Coalesce all .stpk / L0 chunk reads across ALL active mini-blocks and ALL projected
        // ExternalPayload columns in this RG into ONE `read_chunks` call per pack!
        let rg_resolved_chunks = self.resolve_rg_payload_chunks(
            rg_idx,
            &mut active_mbs,
            proj_indices,
            l0_ref,
            &p2_lookup,
        )?;

        let mut batches = Vec::with_capacity(active_mbs.len());
        for (mb_idx, mask, decoded_p1) in active_mbs {
            let batch = self.materialize_phase2_mb(
                rg_idx,
                mb_idx,
                &mask,
                decoded_p1,
                &plan,
                proj_indices,
                proj_schema,
                map_keys_by_col,
                &rg_resolved_chunks,
                &p2_lookup,
            )?;
            batches.push(batch);
        }

        Ok(batches)
    }

    fn resolve_rg_payload_chunks(
        &self,
        rg_idx: usize,
        active_mbs: &mut [(usize, Bitmap, HashMap<usize, DecodedPhase1>)],
        proj_indices: &[usize],
        l0_local: Option<&ParsedL0Payload<'_>>,
        read_block: &dyn Fn(&BlockEntry) -> Result<Vec<u8>, StatamError>,
    ) -> Result<HashMap<(u16, u32), Vec<u8>>, StatamError> {
        let mut proj_payload_cols: Vec<usize> = proj_indices
            .iter()
            .copied()
            .filter(|&cidx| {
                self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload
            })
            .collect();
        if proj_payload_cols.is_empty() {
            return Ok(HashMap::new());
        }
        proj_payload_cols.sort_unstable();
        proj_payload_cols.dedup();

        let mut by_pack: HashMap<u16, BTreeSet<u32>> = HashMap::new();
        for (mb_idx, mask, decoded_p1) in active_mbs.iter_mut() {
            let mb_row_count = self.meta.rg_blocks(0, rg_idx)[*mb_idx].row_count as usize;
            for &cidx in &proj_payload_cols {
                if let std::collections::hash_map::Entry::Vacant(e) = decoded_p1.entry(cidx) {
                    let blk = &self.meta.rg_blocks(cidx, rg_idx)[*mb_idx];
                    let refs = if blk.encoding == Encoding::ConstNull {
                        vec![PayloadRefRow::Null; mb_row_count]
                    } else {
                        let raw = read_block(blk)?;
                        decode_payload_refs(&raw, mb_row_count, MAX_BATCH_DECODED_BYTES)?
                    };
                    e.insert(DecodedPhase1::PayloadRefs(refs));
                }
                if let Some(DecodedPhase1::PayloadRefs(refs)) = decoded_p1.get(&cidx) {
                    for r in mask.iter_ones() {
                        if let PayloadRefRow::Chunks(chunks) = &refs[r] {
                            for &(pidx, cid) in chunks {
                                by_pack.entry(pidx).or_default().insert(cid);
                            }
                        }
                    }
                }
            }
        }

        let mut resolved_chunks: HashMap<(u16, u32), Vec<u8>> = HashMap::new();
        for (pidx, cids_set) in by_pack {
            let cids: Vec<u32> = cids_set.into_iter().collect();
            if pidx == L0_LOCAL_PACK_IDX {
                let l0 = l0_local.ok_or(StatamError::Corrupt {
                    reason: "missing L0 local payload region",
                })?;
                let map = l0.read_chunks(&cids)?;
                for (cid, bytes) in map {
                    resolved_chunks.insert((pidx, cid), bytes);
                }
            } else {
                let pack_id = *self.meta.schema_meta.pack_ids.get(pidx as usize).ok_or(
                    StatamError::Corrupt {
                        reason: "payload ref pack_idx out of bounds",
                    },
                )?;
                let pack = self.resolve_pack(pack_id)?;
                let map = pack.read_chunks(&cids)?;
                for (cid, bytes) in map {
                    resolved_chunks.insert((pidx, cid), bytes);
                }
            }
        }
        Ok(resolved_chunks)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_rg_io_plan(
        &self,
        rg_idx: usize,
        surviving_mbs: &[usize],
        proj_indices: &[usize],
        map_keys_by_col: &HashMap<usize, Vec<String>>,
        time_col_idx: Option<usize>,
        lookups: &[(usize, &IdLookup)],
        preds: &[(usize, &Predicate)],
        subs: &[(usize, &SubstringQuery)],
    ) -> RgIoPlan {
        let mut phase1_set = BTreeSet::new();
        let mut phase2_set = BTreeSet::new();
        let mut phase1_map_fast_path = HashSet::new();
        let mut phase2_map_fast_path = HashSet::new();

        if let Some(tc) = time_col_idx {
            phase1_set.insert(tc);
        }
        for &(cidx, _) in lookups {
            phase1_set.insert(cidx);
        }
        for &(cidx, pred) in preds {
            if let Some(key) = &pred.map_key {
                if let Some((sub_pidx, res_count)) = self.find_shredded_subcol(cidx, key) {
                    phase1_set.insert(sub_pidx);
                    if res_count == 0 {
                        let mut any_p1_needs_res = false;
                        for &mb in surviving_mbs {
                            if self.meta.rg_blocks(cidx, rg_idx)[mb].null_count == 0 {
                                phase1_map_fast_path.insert((mb, cidx));
                            } else {
                                any_p1_needs_res = true;
                            }
                        }
                        if any_p1_needs_res {
                            phase1_set.insert(cidx);
                        }
                    } else {
                        phase1_set.insert(cidx);
                    }
                } else {
                    phase1_set.insert(cidx);
                }
            } else {
                phase1_set.insert(cidx);
            }
        }
        for &(cidx, _) in subs {
            phase1_set.insert(cidx);
        }

        for &cidx in proj_indices {
            let dt = &self.meta.schema_meta.schema.fields()[cidx].data_type;
            if *dt == DataType::MapUtf8
                && let Some(sm) = self
                    .meta
                    .schema_meta
                    .shredded_maps
                    .iter()
                    .find(|s| s.logical_col_idx as usize == cidx)
            {
                if let Some(req_keys) = map_keys_by_col.get(&cidx) {
                    let mut all_shredded_zero_res = !req_keys.is_empty();
                    for rk in req_keys {
                        if let Some(ke) = sm.keys.iter().find(|k| &k.key == rk) {
                            phase2_set.insert(ke.sub_col_idx as usize);
                            if ke.residual_count != 0 {
                                all_shredded_zero_res = false;
                            }
                        } else {
                            all_shredded_zero_res = false;
                        }
                    }
                    if all_shredded_zero_res {
                        let mut any_needs_residual = false;
                        for &mb in surviving_mbs {
                            if self.meta.rg_blocks(cidx, rg_idx)[mb].null_count == 0 {
                                phase2_map_fast_path.insert((mb, cidx));
                            } else {
                                any_needs_residual = true;
                            }
                        }
                        if any_needs_residual {
                            phase2_set.insert(cidx);
                        }
                    } else {
                        phase2_set.insert(cidx);
                    }
                } else {
                    phase2_set.insert(cidx);
                    for ke in &sm.keys {
                        phase2_set.insert(ke.sub_col_idx as usize);
                    }
                }
            } else {
                phase2_set.insert(cidx);
            }
        }

        let phase1_phys_cols: Vec<usize> = phase1_set.iter().copied().collect();
        let phase2_only_phys_cols: Vec<usize> = phase2_set
            .iter()
            .filter(|c| !phase1_set.contains(c))
            .copied()
            .collect();

        RgIoPlan {
            phase1_phys_cols,
            phase2_only_phys_cols,
            phase1_map_fast_path,
            phase2_map_fast_path,
        }
    }

    fn find_shredded_subcol(&self, logical_col: usize, key: &str) -> Option<(usize, u32)> {
        let sm = self
            .meta
            .schema_meta
            .shredded_maps
            .iter()
            .find(|s| s.logical_col_idx as usize == logical_col)?;
        let ke = sm.keys.iter().find(|k| k.key == key)?;
        Some((ke.sub_col_idx as usize, ke.residual_count))
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_phase1_mb(
        &self,
        rg_idx: usize,
        mb_idx: usize,
        plan: &RgIoPlan,
        time_col_idx: Option<usize>,
        time_range: TimeRange,
        lookups: &[(usize, &IdLookup)],
        preds: &[(usize, &Predicate)],
        subs: &[(usize, &SubstringQuery)],
        dict_sub_bitmaps: &[Option<Bitmap>],
        l0_local: Option<&ParsedL0Payload<'_>>,
        read_block: &dyn Fn(&BlockEntry) -> Result<Vec<u8>, StatamError>,
    ) -> Result<(Bitmap, HashMap<usize, DecodedPhase1>), StatamError> {
        let fields = self.meta.schema_meta.schema.fields();
        let mb_row_count = self.meta.rg_blocks(0, rg_idx)[mb_idx].row_count as usize;
        let mut mask = Bitmap::new_ones(mb_row_count);
        let mut decoded_p1: HashMap<usize, DecodedPhase1> = HashMap::new();

        if let Some(tc) = time_col_idx {
            self.ensure_p1_col(
                tc,
                rg_idx,
                mb_idx,
                mb_row_count,
                read_block,
                &mut decoded_p1,
            )?;
            let DecodedPhase1::Col(col) = &decoded_p1[&tc] else {
                return Err(StatamError::Corrupt {
                    reason: "unexpected Phase 1 decoded type for time column",
                });
            };
            let blk = &self.meta.rg_blocks(tc, rg_idx)[mb_idx];
            let bm = eval_column_pred(
                col,
                &ZonePred::Between(
                    ZoneValue::TimestampNs(time_range.min_ns),
                    ZoneValue::TimestampNs(time_range.max_ns),
                ),
                (blk.block_flags & BLOCK_FLAG_HAS_NAN) != 0,
            )?;
            mask = mask.and(&bm)?;
            if mask.all_zeros() {
                return Ok((mask, decoded_p1));
            }
        }

        for &(cidx, lk) in lookups {
            self.ensure_p1_col(
                cidx,
                rg_idx,
                mb_idx,
                mb_row_count,
                read_block,
                &mut decoded_p1,
            )?;
            let DecodedPhase1::Col(col) = &decoded_p1[&cidx] else {
                return Err(StatamError::Corrupt {
                    reason: "unexpected Phase 1 decoded type for id_lookup",
                });
            };
            let bm = eval_id_lookup(col, &fields[cidx].data_type, &lk.values)?;
            mask = mask.and(&bm)?;
            if mask.all_zeros() {
                return Ok((mask, decoded_p1));
            }
        }

        for &(cidx, pred) in preds {
            if let Some(key) = &pred.map_key {
                let bm = if let Some((sub_pidx, res_count)) = self.find_shredded_subcol(cidx, key) {
                    let fast_no_res =
                        res_count == 0 && plan.phase1_map_fast_path.contains(&(mb_idx, cidx));
                    if fast_no_res {
                        self.ensure_p1_col(
                            sub_pidx,
                            rg_idx,
                            mb_idx,
                            mb_row_count,
                            read_block,
                            &mut decoded_p1,
                        )?;
                        let DecodedPhase1::ShreddedSub(sub) = &decoded_p1[&sub_pidx] else {
                            return Err(StatamError::Corrupt {
                                reason: "expected ShreddedSub in Phase 1",
                            });
                        };
                        eval_shredded_subcol_pred(sub, &pred.pred)?
                    } else {
                        self.ensure_p1_col(
                            sub_pidx,
                            rg_idx,
                            mb_idx,
                            mb_row_count,
                            read_block,
                            &mut decoded_p1,
                        )?;
                        self.ensure_p1_col(
                            cidx,
                            rg_idx,
                            mb_idx,
                            mb_row_count,
                            read_block,
                            &mut decoded_p1,
                        )?;
                        let DecodedPhase1::ShreddedSub(sub) = &decoded_p1[&sub_pidx] else {
                            return Err(StatamError::Corrupt {
                                reason: "expected ShreddedSub in Phase 1",
                            });
                        };
                        let DecodedPhase1::Col(res_col) = &decoded_p1[&cidx] else {
                            return Err(StatamError::Corrupt {
                                reason: "expected MapUtf8 residual in Phase 1",
                            });
                        };
                        let res_flat = res_col.materialize(None)?;
                        let single_map = decode_single_map_key(
                            mb_row_count,
                            res_flat.nulls(),
                            key,
                            sub,
                            res_count,
                            Some(&res_flat),
                        )?;
                        eval_map_key_pred(&single_map, key, &pred.pred)?
                    }
                } else {
                    self.ensure_p1_col(
                        cidx,
                        rg_idx,
                        mb_idx,
                        mb_row_count,
                        read_block,
                        &mut decoded_p1,
                    )?;
                    let DecodedPhase1::Col(col) = &decoded_p1[&cidx] else {
                        return Err(StatamError::Corrupt {
                            reason: "expected MapUtf8 in Phase 1",
                        });
                    };
                    let flat = col.materialize(None)?;
                    eval_map_key_pred(&flat, key, &pred.pred)?
                };
                mask = mask.and(&bm)?;
                if mask.all_zeros() {
                    return Ok((mask, decoded_p1));
                }
            } else {
                self.ensure_p1_col(
                    cidx,
                    rg_idx,
                    mb_idx,
                    mb_row_count,
                    read_block,
                    &mut decoded_p1,
                )?;
                let DecodedPhase1::Col(col) = &decoded_p1[&cidx] else {
                    return Err(StatamError::Corrupt {
                        reason: "expected scalar/nested Column in Phase 1",
                    });
                };
                let blk = &self.meta.rg_blocks(cidx, rg_idx)[mb_idx];
                let bm =
                    eval_column_pred(col, &pred.pred, (blk.block_flags & BLOCK_FLAG_HAS_NAN) != 0)?;
                mask = mask.and(&bm)?;
                if mask.all_zeros() {
                    return Ok((mask, decoded_p1));
                }
            }
        }

        // Evaluate non-payload substring queries BEFORE ExternalPayload substrings so
        // `mask` is as narrow as possible before touching `.stpk` or L0 payload chunks.
        for pass_payload in [false, true] {
            for (sq_idx, &(cidx, sq)) in subs.iter().enumerate() {
                let is_payload =
                    self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload;
                if is_payload != pass_payload {
                    continue;
                }
                self.ensure_p1_col(
                    cidx,
                    rg_idx,
                    mb_idx,
                    mb_row_count,
                    read_block,
                    &mut decoded_p1,
                )?;
                let item = &decoded_p1[&cidx];
                let bm = match item {
                    DecodedPhase1::Col(col) => eval_utf8_substring(
                        col,
                        &sq.needle,
                        sq.case_insensitive,
                        dict_sub_bitmaps.get(sq_idx).and_then(Option::as_ref),
                    )?,
                    DecodedPhase1::PayloadRefs(refs) => self.eval_payload_refs_substring(
                        refs,
                        &mask,
                        &sq.needle,
                        sq.case_insensitive,
                        l0_local,
                    )?,
                    DecodedPhase1::ShreddedSub(_) => {
                        return Err(StatamError::Corrupt {
                            reason: "unexpected ShreddedSub for substring query",
                        });
                    }
                };
                mask = mask.and(&bm)?;
                if mask.all_zeros() {
                    return Ok((mask, decoded_p1));
                }
            }
        }

        Ok((mask, decoded_p1))
    }

    fn ensure_p1_col(
        &self,
        pidx: usize,
        rg_idx: usize,
        mb_idx: usize,
        mb_row_count: usize,
        read_block: &dyn Fn(&BlockEntry) -> Result<Vec<u8>, StatamError>,
        decoded_p1: &mut HashMap<usize, DecodedPhase1>,
    ) -> Result<(), StatamError> {
        if decoded_p1.contains_key(&pidx) {
            return Ok(());
        }
        let fields = self.meta.schema_meta.schema.fields();
        let blk = &self.meta.rg_blocks(pidx, rg_idx)[mb_idx];
        let cd = &self.meta.col_dir[pidx];
        let item = if (cd.col_flags & COL_FLAG_IS_SHREDDED_SUBCOL) != 0 {
            let raw = read_block(blk)?;
            let sub = decode_shredded_subcol(&raw, mb_row_count)?;
            DecodedPhase1::ShreddedSub(sub)
        } else if cd.locality_group == LocalityGroup::ExternalPayload {
            if blk.encoding == Encoding::ConstNull {
                DecodedPhase1::PayloadRefs(vec![PayloadRefRow::Null; mb_row_count])
            } else {
                let raw = read_block(blk)?;
                let refs = decode_payload_refs(&raw, mb_row_count, MAX_BATCH_DECODED_BYTES)?;
                DecodedPhase1::PayloadRefs(refs)
            }
        } else if self
            .meta
            .schema_meta
            .shredded_maps
            .iter()
            .any(|s| s.logical_col_idx as usize == pidx)
        {
            let raw = if blk.stored_len > 0 {
                read_block(blk)?
            } else {
                Vec::new()
            };
            let flat = decode_shredded_residual_block(&raw, blk)?;
            DecodedPhase1::Col(Column::Flat(flat))
        } else {
            let raw = if blk.stored_len > 0 {
                read_block(blk)?
            } else {
                Vec::new()
            };
            let dict = if blk.encoding == Encoding::Dict {
                self.get_cached_dict(pidx)
            } else {
                None
            };
            let dt = &fields[cd.logical_col_id as usize].data_type;
            let col = decode_mini_block(&raw, blk, dt, dict.as_deref())?;
            DecodedPhase1::Col(col)
        };
        decoded_p1.insert(pidx, item);
        Ok(())
    }

    fn eval_payload_refs_substring(
        &self,
        refs: &[PayloadRefRow],
        active_mask: &Bitmap,
        needle: &[u8],
        case_insensitive: bool,
        l0_local: Option<&ParsedL0Payload<'_>>,
    ) -> Result<Bitmap, StatamError> {
        let mut results = Bitmap::new_zeroed(refs.len());
        if active_mask.all_zeros() {
            return Ok(results);
        }
        if needle.is_empty() {
            for i in active_mask.iter_ones() {
                if !matches!(refs[i], PayloadRefRow::Null) {
                    let _ = results.set(i, true);
                }
            }
            return Ok(results);
        }

        // Handle active inline rows + track external pack_idxs + L0 local chunks on active rows only.
        let mut matcher = Utf8Matcher::new(needle, case_insensitive);
        let mut external_pack_idxs = BTreeSet::new();
        let mut has_l0_chunks = false;
        let mut active_chunk_rows = Vec::new();

        for i in active_mask.iter_ones() {
            match &refs[i] {
                PayloadRefRow::Null => {}
                PayloadRefRow::Inline(bytes) => {
                    if matcher.matches(bytes) {
                        let _ = results.set(i, true);
                    }
                }
                PayloadRefRow::Chunks(chunks) => {
                    active_chunk_rows.push(i);
                    for &(pidx, _) in chunks {
                        if pidx == L0_LOCAL_PACK_IDX {
                            has_l0_chunks = true;
                        } else {
                            external_pack_idxs.insert(pidx);
                        }
                    }
                }
            }
        }

        for pack_idx in external_pack_idxs {
            let mut orig_rows = Vec::new();
            let mut filtered_refs = Vec::new();
            for &r in &active_chunk_rows {
                if results.get(r) {
                    continue;
                }
                if let PayloadRefRow::Chunks(chunks) = &refs[r]
                    && chunks.iter().any(|&(p, _)| p == pack_idx)
                {
                    orig_rows.push(r);
                    filtered_refs.push(refs[r].clone());
                }
            }
            if filtered_refs.is_empty() {
                continue;
            }
            let pack_id = *self
                .meta
                .schema_meta
                .pack_ids
                .get(pack_idx as usize)
                .ok_or(StatamError::Corrupt {
                    reason: "payload ref pack_idx out of bounds",
                })?;
            let pack = self.resolve_pack(pack_id)?;
            let bm = search_pack_rows(&pack, &filtered_refs, pack_idx, needle, case_insensitive)?;
            for bit in bm.iter_ones() {
                let _ = results.set(orig_rows[bit], true);
            }
        }

        if has_l0_chunks {
            let l0 = l0_local.ok_or(StatamError::Corrupt {
                reason: "missing L0 local payload region for chunked L0 row",
            })?;
            let mut needed_cids = BTreeSet::new();
            for &r in &active_chunk_rows {
                if results.get(r) {
                    continue;
                }
                if let PayloadRefRow::Chunks(chunks) = &refs[r] {
                    for &(pidx, cid) in chunks {
                        if pidx == L0_LOCAL_PACK_IDX {
                            needed_cids.insert(cid);
                        }
                    }
                }
            }
            if !needed_cids.is_empty() {
                let cids: Vec<u32> = needed_cids.into_iter().collect();
                let chunk_map = l0.read_chunks(&cids)?;
                let mut buf = Vec::new();
                for &r in &active_chunk_rows {
                    if results.get(r) {
                        continue;
                    }
                    if let PayloadRefRow::Chunks(chunks) = &refs[r] {
                        buf.clear();
                        for &(pidx, cid) in chunks {
                            if pidx == L0_LOCAL_PACK_IDX
                                && let Some(c) = chunk_map.get(&cid)
                            {
                                if buf.len().saturating_add(c.len()) > MAX_ROW_PAYLOAD_BYTES {
                                    return Err(StatamError::BoundsExceeded {
                                        reason: "L0 payload exceeds MAX_ROW_PAYLOAD_BYTES",
                                    });
                                }
                                buf.extend_from_slice(c);
                            }
                        }
                        if matcher.matches(&buf) {
                            let _ = results.set(r, true);
                        }
                    }
                }
            }
        }

        Ok(results)
    }

    #[allow(clippy::too_many_arguments)]
    fn materialize_phase2_mb(
        &self,
        rg_idx: usize,
        mb_idx: usize,
        mask: &Bitmap,
        mut decoded_p1: HashMap<usize, DecodedPhase1>,
        plan: &RgIoPlan,
        proj_indices: &[usize],
        proj_schema: &Arc<Schema>,
        map_keys_by_col: &HashMap<usize, Vec<String>>,
        resolved_chunks: &HashMap<(u16, u32), Vec<u8>>,
        read_block: &dyn Fn(&BlockEntry) -> Result<Vec<u8>, StatamError>,
    ) -> Result<Batch, StatamError> {
        let fields = self.meta.schema_meta.schema.fields();
        let mb_row_count = self.meta.rg_blocks(0, rg_idx)[mb_idx].row_count as usize;

        if proj_indices.is_empty() {
            let sel = if mask.all_ones() {
                Selection::All
            } else {
                Selection::Mask(mask.clone())
            };
            return Ok(Batch::try_new(
                Arc::clone(proj_schema),
                Vec::new(),
                mb_row_count,
                sel,
            )?);
        }

        // Decode any needed shredded subcols for projected MapUtf8 columns (moving from decoded_p1).
        let mut decoded_subs: HashMap<usize, ShreddedSubCol> = HashMap::new();
        for &cidx in proj_indices {
            if fields[cidx].data_type == DataType::MapUtf8
                && let Some(sm) = self
                    .meta
                    .schema_meta
                    .shredded_maps
                    .iter()
                    .find(|s| s.logical_col_idx as usize == cidx)
            {
                let keys_needed: Vec<&str> = match map_keys_by_col.get(&cidx) {
                    Some(req_keys) => req_keys.iter().map(String::as_str).collect(),
                    None => sm.keys.iter().map(|k| k.key.as_str()).collect(),
                };
                for ke in &sm.keys {
                    if !keys_needed.contains(&ke.key.as_str()) {
                        continue;
                    }
                    let sub_pidx = ke.sub_col_idx as usize;
                    if decoded_subs.contains_key(&sub_pidx) {
                        continue;
                    }
                    if let Some(DecodedPhase1::ShreddedSub(sub)) = decoded_p1.remove(&sub_pidx) {
                        decoded_subs.insert(sub_pidx, sub);
                    } else {
                        let blk = &self.meta.rg_blocks(sub_pidx, rg_idx)[mb_idx];
                        let raw = read_block(blk)?;
                        let sub = decode_shredded_subcol(&raw, mb_row_count)?;
                        decoded_subs.insert(sub_pidx, sub);
                    }
                }
            }
        }

        let mut out_cols = Vec::with_capacity(proj_indices.len());
        for &cidx in proj_indices {
            let dt = &fields[cidx].data_type;
            let cd = &self.meta.col_dir[cidx];

            if cd.locality_group == LocalityGroup::ExternalPayload {
                let ref_rows_owned;
                let ref_rows: &[PayloadRefRow] =
                    if let Some(DecodedPhase1::PayloadRefs(r)) = decoded_p1.remove(&cidx) {
                        ref_rows_owned = r;
                        &ref_rows_owned
                    } else {
                        let blk = &self.meta.rg_blocks(cidx, rg_idx)[mb_idx];
                        if blk.encoding == Encoding::ConstNull {
                            ref_rows_owned = vec![PayloadRefRow::Null; mb_row_count];
                        } else {
                            let raw = read_block(blk)?;
                            ref_rows_owned =
                                decode_payload_refs(&raw, mb_row_count, MAX_BATCH_DECODED_BYTES)?;
                        }
                        &ref_rows_owned
                    };
                let col = reconstruct_payload_column_from_map(ref_rows, mask, resolved_chunks)?;
                out_cols.push(col);
                continue;
            }

            if *dt == DataType::MapUtf8
                && let Some(sm) = self
                    .meta
                    .schema_meta
                    .shredded_maps
                    .iter()
                    .find(|s| s.logical_col_idx as usize == cidx)
            {
                let skip_residual = plan.phase2_map_fast_path.contains(&(mb_idx, cidx));
                let residual_flat: Option<FlatColumn> = if skip_residual {
                    None
                } else if let Some(DecodedPhase1::Col(c)) = decoded_p1.remove(&cidx) {
                    Some(c.materialize(None)?)
                } else {
                    let blk = &self.meta.rg_blocks(cidx, rg_idx)[mb_idx];
                    let raw = if blk.stored_len > 0 {
                        read_block(blk)?
                    } else {
                        Vec::new()
                    };
                    Some(decode_shredded_residual_block(&raw, blk)?)
                };

                let flat_map = if let Some(req_keys) = map_keys_by_col.get(&cidx) {
                    if req_keys.len() == 1
                        && let Some(ke) = sm.keys.iter().find(|k| k.key == req_keys[0])
                    {
                        let sub = &decoded_subs[&(ke.sub_col_idx as usize)];
                        decode_single_map_key(
                            mb_row_count,
                            residual_flat.as_ref().and_then(FlatColumn::nulls),
                            &ke.key,
                            sub,
                            if skip_residual { 0 } else { ke.residual_count },
                            residual_flat.as_ref(),
                        )?
                    } else {
                        let mut subpairs: Vec<(&str, &ShreddedSubCol)> = Vec::new();
                        for ke in &sm.keys {
                            if req_keys.iter().any(|rk| rk == &ke.key)
                                && let Some(sub) = decoded_subs.get(&(ke.sub_col_idx as usize))
                            {
                                subpairs.push((ke.key.as_str(), sub));
                            }
                        }
                        let pruned_residual = if skip_residual {
                            None
                        } else {
                            residual_flat
                                .as_ref()
                                .map(|res| filter_residual_to_keys(res, req_keys))
                                .transpose()?
                        };
                        unshred_map(
                            mb_row_count,
                            pruned_residual.as_ref().and_then(FlatColumn::nulls),
                            pruned_residual.as_ref(),
                            &subpairs,
                        )?
                    }
                } else {
                    let mut subpairs: Vec<(&str, &ShreddedSubCol)> =
                        Vec::with_capacity(sm.keys.len());
                    for ke in &sm.keys {
                        if let Some(sub) = decoded_subs.get(&(ke.sub_col_idx as usize)) {
                            subpairs.push((ke.key.as_str(), sub));
                        }
                    }
                    unshred_map(
                        mb_row_count,
                        residual_flat.as_ref().and_then(FlatColumn::nulls),
                        residual_flat.as_ref(),
                        &subpairs,
                    )?
                };
                out_cols.push(Column::Flat(flat_map));
                continue;
            }

            if let Some(DecodedPhase1::Col(col)) = decoded_p1.remove(&cidx) {
                if *dt == DataType::MapUtf8
                    && let Some(req_keys) = map_keys_by_col.get(&cidx)
                {
                    let flat = col.materialize(None)?;
                    out_cols.push(Column::Flat(filter_residual_to_keys(&flat, req_keys)?));
                } else {
                    out_cols.push(col);
                }
                continue;
            }

            let blk = &self.meta.rg_blocks(cidx, rg_idx)[mb_idx];
            let raw = if blk.stored_len > 0 {
                read_block(blk)?
            } else {
                Vec::new()
            };
            let dict = if blk.encoding == Encoding::Dict {
                self.get_cached_dict(cidx)
            } else {
                None
            };
            let col = decode_mini_block(&raw, blk, dt, dict.as_deref())?;
            if *dt == DataType::MapUtf8
                && let Some(req_keys) = map_keys_by_col.get(&cidx)
            {
                let flat = col.materialize(None)?;
                out_cols.push(Column::Flat(filter_residual_to_keys(&flat, req_keys)?));
            } else {
                out_cols.push(col);
            }
        }

        let sel = if mask.all_ones() {
            Selection::All
        } else {
            Selection::Mask(mask.clone())
        };
        Ok(Batch::try_new(
            Arc::clone(proj_schema),
            out_cols,
            mb_row_count,
            sel,
        )?)
    }

    // --- Internal helpers exposed to gdpr.rs ---

    pub(crate) fn read_block_body(&self, blk: &BlockEntry) -> Result<Vec<u8>, StatamError> {
        if blk.stored_len == 0 {
            return Ok(Vec::new());
        }
        let total = (BLOCK_FRAME_HEADER_LEN as u64)
            .checked_add(u64::from(blk.stored_len))
            .ok_or(StatamError::BoundsExceeded {
                reason: "block length overflow",
            })?;
        let end = blk
            .offset
            .checked_add(total)
            .ok_or(StatamError::BoundsExceeded {
                reason: "block end overflow",
            })?;
        let frame = self.reader.read_range(blk.offset..end)?;
        if frame.len() as u64 != total {
            return Err(StatamError::Corrupt {
                reason: "short read on block",
            });
        }
        let (_, _, body) = read_framed_block(&frame, MAX_BLOCK_RAW_BYTES)?;
        Ok(body)
    }

    pub(crate) fn read_l0_payload_chunks_for_rg(
        &self,
        rg_idx: usize,
        cids: &[u32],
    ) -> Result<HashMap<u32, Vec<u8>>, StatamError> {
        if cids.is_empty() {
            return Ok(HashMap::new());
        }
        let rg = &self.meta.rg_dir[rg_idx];
        if rg.l0_payload_len == 0 {
            return Err(StatamError::Corrupt {
                reason: "missing L0 payload region for L0 chunk reference",
            });
        }
        let end = rg
            .l0_payload_offset
            .checked_add(u64::from(rg.l0_payload_len))
            .ok_or(StatamError::BoundsExceeded {
                reason: "L0 payload end overflow",
            })?;
        let bytes = self.reader.read_range(rg.l0_payload_offset..end)?;
        if bytes.len() != rg.l0_payload_len as usize {
            return Err(StatamError::Corrupt {
                reason: "short read on L0 payload region",
            });
        }
        let parsed = ParsedL0Payload::parse(&bytes)?;
        parsed.read_chunks(cids)
    }

    pub(crate) fn read_non_payload_rg_batch(&self, rg_idx: usize) -> Result<Batch, StatamError> {
        let rg = &self.meta.rg_dir[rg_idx];
        let schema = &self.meta.schema_meta.schema;
        let fields = schema.fields();
        let rg_rows = rg.row_count as usize;

        let mut phys_cols = Vec::new();
        for (cidx, _f) in fields.iter().enumerate() {
            if self.meta.col_dir[cidx].locality_group != LocalityGroup::ExternalPayload {
                phys_cols.push(cidx);
                if let Some(sm) = self
                    .meta
                    .schema_meta
                    .shredded_maps
                    .iter()
                    .find(|s| s.logical_col_idx as usize == cidx)
                {
                    for ke in &sm.keys {
                        phys_cols.push(ke.sub_col_idx as usize);
                    }
                }
            }
        }
        let dict_cols: Vec<usize> = phys_cols
            .iter()
            .copied()
            .filter(|&pidx| {
                (self.meta.col_dir[pidx].col_flags & COL_FLAG_HAS_FILE_DICT) != 0
                    && self
                        .meta
                        .rg_blocks(pidx, rg_idx)
                        .iter()
                        .any(|b| b.encoding == Encoding::Dict)
            })
            .collect();
        if !dict_cols.is_empty() {
            self.ensure_dicts_loaded(&dict_cols)?;
        }

        // Coalesce all non-payload reads in this RG into 1 `read_range` over `core_offset .. wide_offset + wide_len`.
        let range_start = rg.core_offset;
        let range_end = rg.wide_offset.checked_add(u64::from(rg.wide_len)).ok_or(
            StatamError::BoundsExceeded {
                reason: "RG wide end overflow during GDPR read",
            },
        )?;
        let expected_len = range_end
            .checked_sub(range_start)
            .ok_or(StatamError::Corrupt {
                reason: "invalid RG range during GDPR read",
            })?;
        let rg_slice = if range_end > range_start {
            let b = self.reader.read_range(range_start..range_end)?;
            if b.len() as u64 != expected_len {
                return Err(StatamError::Corrupt {
                    reason: "short read on RG range during GDPR read",
                });
            }
            b
        } else {
            Bytes::new()
        };

        let read_from_rg = |blk: &BlockEntry| -> Result<Vec<u8>, StatamError> {
            if blk.stored_len == 0 {
                return Ok(Vec::new());
            }
            let rel = blk
                .offset
                .checked_sub(range_start)
                .and_then(|v| usize::try_from(v).ok())
                .ok_or(StatamError::Corrupt {
                    reason: "block offset before RG range during GDPR read",
                })?;
            let total = BLOCK_FRAME_HEADER_LEN
                .checked_add(blk.stored_len as usize)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "block total length overflow during GDPR read",
                })?;
            let end = rel.checked_add(total).ok_or(StatamError::BoundsExceeded {
                reason: "block end offset overflow during GDPR read",
            })?;
            let frame = rg_slice.get(rel..end).ok_or(StatamError::Corrupt {
                reason: "block slice out of RG range during GDPR read",
            })?;
            let (_, _, body) = read_framed_block(frame, MAX_BLOCK_RAW_BYTES)?;
            Ok(body)
        };

        let mut cols = Vec::with_capacity(fields.len());
        for (cidx, field) in fields.iter().enumerate() {
            let cd = &self.meta.col_dir[cidx];
            if cd.locality_group == LocalityGroup::ExternalPayload {
                cols.push(Column::try_new_const(
                    ScalarValue::Utf8(String::new()),
                    rg_rows,
                )?);
                continue;
            }

            let blks = self.meta.rg_blocks(cidx, rg_idx);
            if field.data_type == DataType::MapUtf8
                && let Some(sm) = self
                    .meta
                    .schema_meta
                    .shredded_maps
                    .iter()
                    .find(|s| s.logical_col_idx as usize == cidx)
            {
                let mut flat_parts = Vec::with_capacity(blks.len());
                for (mb_idx, blk) in blks.iter().enumerate() {
                    let raw = read_from_rg(blk)?;
                    let res = decode_shredded_residual_block(&raw, blk)?;
                    let mut sub_owned = Vec::with_capacity(sm.keys.len());
                    for ke in &sm.keys {
                        let sblk = &self.meta.rg_blocks(ke.sub_col_idx as usize, rg_idx)[mb_idx];
                        let sraw = read_from_rg(sblk)?;
                        sub_owned.push((
                            ke.key.as_str(),
                            decode_shredded_subcol(&sraw, blk.row_count as usize)?,
                        ));
                    }
                    let sub_refs: Vec<(&str, &ShreddedSubCol)> =
                        sub_owned.iter().map(|(k, s)| (*k, s)).collect();
                    let unshred =
                        unshred_map(blk.row_count as usize, res.nulls(), Some(&res), &sub_refs)?;
                    flat_parts.push(unshred);
                }
                let refs: Vec<&FlatColumn> = flat_parts.iter().collect();
                let merged = crate::writer::concat_flat_columns(&field.data_type, &refs, rg_rows)?;
                cols.push(Column::Flat(merged));
            } else {
                let has_dict_blk = blks.iter().any(|b| b.encoding == Encoding::Dict);
                let dict = if has_dict_blk {
                    self.get_cached_dict(cidx)
                } else {
                    None
                };
                let mut flat_parts = Vec::with_capacity(blks.len());
                for blk in blks {
                    let raw = read_from_rg(blk)?;
                    let col = decode_mini_block(&raw, blk, &field.data_type, dict.as_deref())?;
                    flat_parts.push(col.materialize(None)?);
                }
                let refs: Vec<&FlatColumn> = flat_parts.iter().collect();
                let merged = crate::writer::concat_flat_columns(&field.data_type, &refs, rg_rows)?;
                cols.push(Column::Flat(merged));
            }
        }

        Ok(Batch::try_new_all(Arc::clone(schema), cols, rg_rows)?)
    }
}

fn reconstruct_payload_column_from_map(
    refs: &[PayloadRefRow],
    active_mask: &Bitmap,
    resolved_chunks: &HashMap<(u16, u32), Vec<u8>>,
) -> Result<Column, StatamError> {
    let row_count = refs.len();
    let all_active_null = active_mask
        .iter_ones()
        .all(|r| matches!(refs[r], PayloadRefRow::Null));
    if all_active_null {
        return Ok(Column::try_new_const(
            ScalarValue::Null(DataType::Utf8),
            row_count,
        )?);
    }

    let mut offsets = Vec::with_capacity(row_count + 1);
    offsets.push(0u32);
    let mut data_bytes = Vec::new();
    let mut nulls = Bitmap::new_ones(row_count);
    let mut total_decoded = 0usize;

    for (r, row) in refs.iter().enumerate() {
        if !active_mask.get(r) {
            offsets.push(data_bytes.len() as u32);
            let _ = nulls.set(r, false);
            continue;
        }
        match row {
            PayloadRefRow::Null => {
                let _ = nulls.set(r, false);
                offsets.push(data_bytes.len() as u32);
            }
            PayloadRefRow::Inline(bytes) => {
                total_decoded =
                    total_decoded
                        .checked_add(bytes.len())
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "decoded payload column overflows usize",
                        })?;
                if total_decoded > MAX_BATCH_DECODED_BYTES {
                    return Err(StatamError::BoundsExceeded {
                        reason: "decoded payload column exceeds MAX_BATCH_DECODED_BYTES",
                    });
                }
                let next_off = u32::try_from(data_bytes.len() + bytes.len()).map_err(|_| {
                    StatamError::BoundsExceeded {
                        reason: "utf8 payload column exceeds u32::MAX",
                    }
                })?;
                data_bytes.extend_from_slice(bytes);
                offsets.push(next_off);
            }
            PayloadRefRow::Chunks(chunks) => {
                let mut row_len = 0usize;
                let start_pos = data_bytes.len();
                for &(pidx, cid) in chunks {
                    let chunk = resolved_chunks
                        .get(&(pidx, cid))
                        .ok_or(StatamError::Corrupt {
                            reason: "missing resolved payload chunk",
                        })?;
                    row_len =
                        row_len
                            .checked_add(chunk.len())
                            .ok_or(StatamError::BoundsExceeded {
                                reason: "row payload byte size overflow",
                            })?;
                    if row_len > MAX_ROW_PAYLOAD_BYTES {
                        return Err(StatamError::BoundsExceeded {
                            reason: "reconstructed row payload exceeds MAX_ROW_PAYLOAD_BYTES",
                        });
                    }
                    total_decoded = total_decoded.checked_add(chunk.len()).ok_or(
                        StatamError::BoundsExceeded {
                            reason: "decoded payload column overflows usize",
                        },
                    )?;
                    if total_decoded > MAX_BATCH_DECODED_BYTES {
                        return Err(StatamError::BoundsExceeded {
                            reason: "decoded payload column exceeds MAX_BATCH_DECODED_BYTES",
                        });
                    }
                    data_bytes.extend_from_slice(chunk);
                }
                let next_off = u32::try_from(start_pos + row_len).map_err(|_| {
                    StatamError::BoundsExceeded {
                        reason: "utf8 payload column exceeds u32::MAX",
                    }
                })?;
                offsets.push(next_off);
            }
        }
    }

    let utf8 = Utf8Data::try_new(offsets, data_bytes)?;
    let nulls_opt = if nulls.all_ones() { None } else { Some(nulls) };
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(utf8),
        nulls_opt,
    )?))
}

struct RgIoPlan {
    phase1_phys_cols: Vec<usize>,
    phase2_only_phys_cols: Vec<usize>,
    phase1_map_fast_path: HashSet<(usize, usize)>,
    phase2_map_fast_path: HashSet<(usize, usize)>,
}

impl RgIoPlan {
    fn all_phys_cols(&self) -> impl Iterator<Item = &usize> {
        self.phase1_phys_cols
            .iter()
            .chain(self.phase2_only_phys_cols.iter())
    }
}

enum DecodedPhase1 {
    Col(Column),
    ShreddedSub(ShreddedSubCol),
    PayloadRefs(Vec<PayloadRefRow>),
}

struct ParsedL0Payload<'a> {
    buf: &'a [u8],
    frames: Vec<(u64, u32, u32)>,
    chunks: Vec<ChunkLoc>,
}

impl<'a> ParsedL0Payload<'a> {
    fn parse(buf: &'a [u8]) -> Result<Self, StatamError> {
        if buf.len() < 12 || buf[0..4] != L0_PAYLOAD_DIR_MAGIC {
            return Err(StatamError::Corrupt {
                reason: "invalid L0 payload directory header",
            });
        }
        let frame_count = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        let chunk_count = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]) as usize;
        let frames_bytes = frame_count
            .checked_mul(16)
            .ok_or(StatamError::BoundsExceeded {
                reason: "L0 payload frame table overflow",
            })?;
        let chunks_bytes = chunk_count
            .checked_mul(12)
            .ok_or(StatamError::BoundsExceeded {
                reason: "L0 payload chunk table overflow",
            })?;
        let dir_bytes = 12usize
            .checked_add(frames_bytes)
            .and_then(|v| v.checked_add(chunks_bytes))
            .ok_or(StatamError::BoundsExceeded {
                reason: "L0 payload directory size overflow",
            })?;
        if dir_bytes > buf.len() {
            return Err(StatamError::Corrupt {
                reason: "truncated L0 payload directory",
            });
        }

        let mut frames = Vec::with_capacity(frame_count);
        let mut pos = 12usize;
        for _ in 0..frame_count {
            let rel_off = u64::from_le_bytes(buf[pos..pos + 8].try_into().unwrap_or([0; 8]));
            let stored_len =
                u32::from_le_bytes([buf[pos + 8], buf[pos + 9], buf[pos + 10], buf[pos + 11]]);
            let raw_len =
                u32::from_le_bytes([buf[pos + 12], buf[pos + 13], buf[pos + 14], buf[pos + 15]]);
            let total = (crate::format::PACK_FRAME_HEADER_LEN as u64)
                .checked_add(u64::from(stored_len))
                .ok_or(StatamError::BoundsExceeded {
                    reason: "L0 frame length overflow",
                })?;
            let end = rel_off
                .checked_add(total)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "L0 frame end overflow",
                })?;
            if rel_off < dir_bytes as u64 || end > buf.len() as u64 {
                return Err(StatamError::Corrupt {
                    reason: "L0 frame out of region bounds",
                });
            }
            frames.push((rel_off, stored_len, raw_len));
            pos += 16;
        }

        let mut chunks = Vec::with_capacity(chunk_count);
        for _ in 0..chunk_count {
            let loc = ChunkLoc::decode(&buf[pos..pos + 12])?;
            let Some(&(_, _, f_raw)) = frames.get(loc.frame_id as usize) else {
                return Err(StatamError::Corrupt {
                    reason: "L0 chunk frame_id out of bounds",
                });
            };
            let end = loc.offset_in_frame.checked_add(loc.raw_len).ok_or(
                StatamError::BoundsExceeded {
                    reason: "L0 chunk end overflow",
                },
            )?;
            if end > f_raw {
                return Err(StatamError::Corrupt {
                    reason: "L0 chunk exceeds frame raw_len",
                });
            }
            chunks.push(loc);
            pos += 12;
        }

        Ok(Self {
            buf,
            frames,
            chunks,
        })
    }

    fn verify_all_frames(&self) -> Result<(), StatamError> {
        for &(rel_off, stored_len, raw_len) in &self.frames {
            let start = rel_off as usize;
            let end = start + crate::format::PACK_FRAME_HEADER_LEN + stored_len as usize;
            let (_, _, decoded) =
                read_framed_pack_frame(&self.buf[start..end], MAX_FRAME_RAW_BYTES)?;
            if decoded.len() != raw_len as usize {
                return Err(StatamError::Corrupt {
                    reason: "L0 frame raw_len mismatch",
                });
            }
        }
        Ok(())
    }

    fn read_chunks(&self, cids: &[u32]) -> Result<HashMap<u32, Vec<u8>>, StatamError> {
        let mut needed_frames = BTreeSet::new();
        for &cid in cids {
            let loc = self.chunks.get(cid as usize).ok_or(StatamError::Corrupt {
                reason: "L0 chunk_id out of bounds",
            })?;
            needed_frames.insert(loc.frame_id);
        }

        let mut decoded_frames = HashMap::with_capacity(needed_frames.len());
        for fid in needed_frames {
            let (rel_off, stored_len, raw_len) = self.frames[fid as usize];
            let start = rel_off as usize;
            let end = start + crate::format::PACK_FRAME_HEADER_LEN + stored_len as usize;
            let (_, _, decoded) =
                read_framed_pack_frame(&self.buf[start..end], MAX_FRAME_RAW_BYTES)?;
            if decoded.len() != raw_len as usize {
                return Err(StatamError::Corrupt {
                    reason: "L0 frame raw_len mismatch",
                });
            }
            decoded_frames.insert(fid, decoded);
        }

        let mut out = HashMap::with_capacity(cids.len());
        for &cid in cids {
            let loc = self.chunks[cid as usize];
            let frame_buf = &decoded_frames[&loc.frame_id];
            let s = loc.offset_in_frame as usize;
            let e = s + loc.raw_len as usize;
            out.insert(cid, frame_buf[s..e].to_vec());
        }
        Ok(out)
    }
}

fn decode_narrow_trigram_section(
    bytes: &[u8],
    expected_blocks: usize,
) -> Result<Vec<Option<TrigramBloom>>, StatamError> {
    if bytes.len() < 8 || bytes[0..4] != NARROW_TRIGRAM_MAGIC {
        return Err(StatamError::Corrupt {
            reason: "invalid narrow trigram section header",
        });
    }
    let count = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    if count != expected_blocks {
        return Err(StatamError::Corrupt {
            reason: "narrow trigram block count mismatch",
        });
    }
    let table_bytes = count.checked_mul(8).ok_or(StatamError::BoundsExceeded {
        reason: "narrow trigram table overflow",
    })?;
    let body_start = 8usize
        .checked_add(table_bytes)
        .ok_or(StatamError::BoundsExceeded {
            reason: "narrow trigram header overflow",
        })?;
    if body_start > bytes.len() {
        return Err(StatamError::Corrupt {
            reason: "truncated narrow trigram offset table",
        });
    }
    let body = &bytes[body_start..];
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let p = 8 + i * 8;
        let off = u32::from_le_bytes([bytes[p], bytes[p + 1], bytes[p + 2], bytes[p + 3]]) as usize;
        let len =
            u32::from_le_bytes([bytes[p + 4], bytes[p + 5], bytes[p + 6], bytes[p + 7]]) as usize;
        if len == 0 {
            out.push(None);
        } else {
            let end = off.checked_add(len).ok_or(StatamError::BoundsExceeded {
                reason: "narrow trigram filter slice overflow",
            })?;
            let slice = body.get(off..end).ok_or(StatamError::Corrupt {
                reason: "narrow trigram filter out of section bounds",
            })?;
            out.push(Some(TrigramBloom::from_bytes(slice.to_vec())?));
        }
    }
    Ok(out)
}

fn fetch_coalesced_blocks(
    reader: &dyn RangeReader,
    raw_ranges: &[(u64, u64)],
) -> Result<HashMap<(u64, u64), Bytes>, StatamError> {
    if raw_ranges.is_empty() {
        return Ok(HashMap::new());
    }
    let mut sorted: Vec<(u64, u64)> = raw_ranges.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut coalesced: Vec<Range<u64>> = Vec::new();
    let mut groups: Vec<Vec<(u64, u64)>> = Vec::new();
    for &(s, e) in &sorted {
        if s > e {
            return Err(StatamError::Corrupt {
                reason: "invalid block range with start > end",
            });
        }
        if let Some(last_r) = coalesced.last_mut()
            && s <= last_r.end.saturating_add(SCAN_COALESCE_MAX_GAP)
            && e.saturating_sub(last_r.start) <= SCAN_COALESCE_MAX_SPAN
        {
            last_r.end = last_r.end.max(e);
            if let Some(g) = groups.last_mut() {
                g.push((s, e));
            }
            continue;
        }
        coalesced.push(s..e);
        groups.push(vec![(s, e)]);
    }

    let fetched = reader.read_ranges(&coalesced)?;
    if fetched.len() != coalesced.len() {
        return Err(StatamError::Corrupt {
            reason: "read_ranges returned unexpected span count",
        });
    }

    let mut map = HashMap::with_capacity(sorted.len());
    for ((span, items), bytes) in coalesced.iter().zip(groups).zip(fetched) {
        let expected_span_len = span
            .end
            .checked_sub(span.start)
            .ok_or(StatamError::Corrupt {
                reason: "invalid coalesced span",
            })?;
        if bytes.len() as u64 != expected_span_len {
            return Err(StatamError::Corrupt {
                reason: "short read on coalesced block range",
            });
        }
        for (s, e) in items {
            let rel_s = s
                .checked_sub(span.start)
                .and_then(|v| usize::try_from(v).ok())
                .ok_or(StatamError::Corrupt {
                    reason: "coalesced subrange start out of bounds",
                })?;
            let rel_e = e
                .checked_sub(span.start)
                .and_then(|v| usize::try_from(v).ok())
                .ok_or(StatamError::Corrupt {
                    reason: "coalesced subrange end out of bounds",
                })?;
            if rel_s > rel_e || rel_e > bytes.len() {
                return Err(StatamError::Corrupt {
                    reason: "coalesced subrange slice out of bounds",
                });
            }
            map.insert((s, e), bytes.slice(rel_s..rel_e));
        }
    }
    Ok(map)
}

fn find_time_column(schema: &Schema) -> Option<usize> {
    schema
        .index_of("start_time")
        .or_else(|| schema.index_of("timestamp"))
        .or_else(|| {
            schema
                .fields()
                .iter()
                .position(|f| matches!(f.data_type, DataType::TimestampNs { .. }))
        })
}

#[inline]
fn f64_total_cmp(a: f64, b: f64) -> std::cmp::Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        (false, false) => {
            let ca = if a == 0.0 { 0.0 } else { a };
            let cb = if b == 0.0 { 0.0 } else { b };
            ca.total_cmp(&cb)
        }
    }
}

fn filter_f64_total_cmp(vals: &[f64], op: FilterOp, rhs: f64) -> Bitmap {
    let mut out = Bitmap::new_zeroed(vals.len());
    for (r, &v) in vals.iter().enumerate() {
        let ord = f64_total_cmp(v, rhs);
        let hit = match op {
            FilterOp::Eq => ord == std::cmp::Ordering::Equal,
            FilterOp::Ne => ord != std::cmp::Ordering::Equal,
            FilterOp::Lt => ord == std::cmp::Ordering::Less,
            FilterOp::Le => ord != std::cmp::Ordering::Greater,
            FilterOp::Gt => ord == std::cmp::Ordering::Greater,
            FilterOp::Ge => ord != std::cmp::Ordering::Less,
        };
        if hit {
            let _ = out.set(r, true);
        }
    }
    out
}

fn is_always_false_pred(pred: &ZonePred) -> bool {
    match pred {
        ZonePred::In(vs) => vs.is_empty(),
        ZonePred::Between(a, b) => match (a, b) {
            (ZoneValue::I64(x), ZoneValue::I64(y))
            | (ZoneValue::TimestampNs(x), ZoneValue::TimestampNs(y)) => x > y,
            (ZoneValue::Date(x), ZoneValue::Date(y)) => x > y,
            (ZoneValue::Decimal128(x), ZoneValue::Decimal128(y)) => x > y,
            (ZoneValue::Bytes(x), ZoneValue::Bytes(y)) => x > y,
            (ZoneValue::F64(x), ZoneValue::F64(y)) => {
                f64_total_cmp(*x, *y) == std::cmp::Ordering::Greater
            }
            _ => false,
        },
        _ => false,
    }
}

fn id_lookup_zone_pred(dt: &DataType, values: &[Vec<u8>]) -> Option<ZonePred> {
    let to_zval = |b: &[u8]| -> Option<ZoneValue> {
        match dt {
            DataType::Utf8 | DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16 => {
                Some(ZoneValue::Bytes(b.to_vec()))
            }
            DataType::I64 if b.len() == 8 => {
                Some(ZoneValue::I64(i64::from_le_bytes(b.try_into().ok()?)))
            }
            DataType::TimestampNs { .. } if b.len() == 8 => Some(ZoneValue::TimestampNs(
                i64::from_le_bytes(b.try_into().ok()?),
            )),
            _ => None,
        }
    };
    if values.len() == 1 {
        to_zval(&values[0]).map(ZonePred::Eq)
    } else {
        let mut zvals = Vec::with_capacity(values.len());
        for v in values {
            zvals.push(to_zval(v)?);
        }
        Some(ZonePred::In(zvals))
    }
}

fn eval_id_lookup(col: &Column, dt: &DataType, values: &[Vec<u8>]) -> Result<Bitmap, StatamError> {
    if let Some(pred) = id_lookup_zone_pred(dt, values) {
        eval_column_pred(col, &pred, false)
    } else {
        Ok(Bitmap::new_zeroed(col.len()))
    }
}

fn eval_column_pred(col: &Column, pred: &ZonePred, has_nan: bool) -> Result<Bitmap, StatamError> {
    let n = col.len();
    if n == 0 {
        return Ok(Bitmap::new_zeroed(0));
    }

    match col {
        Column::Const { value, .. } => {
            let hit = eval_scalar_pred(value, pred);
            Ok(if hit {
                Bitmap::new_ones(n)
            } else {
                Bitmap::new_zeroed(n)
            })
        }
        Column::Rle(r) => {
            let run_mask = eval_flat_pred(r.values(), pred, has_nan)?;
            let mut out = Bitmap::new_zeroed(n);
            let mut prev = 0u32;
            for (run_idx, &end) in r.run_ends().iter().enumerate() {
                if run_mask.get(run_idx) {
                    for row in prev as usize..end as usize {
                        let _ = out.set(row, true);
                    }
                }
                prev = end;
            }
            Ok(out)
        }
        Column::Dict(d) => eval_dict_pred(d, pred),
        Column::Flat(f) => eval_flat_pred(f, pred, has_nan),
    }
}

fn eval_dict_pred(d: &cetra_column::DictColumn, pred: &ZonePred) -> Result<Bitmap, StatamError> {
    let n = d.len();
    match pred {
        ZonePred::IsNull => {
            let mut bm = Bitmap::new_zeroed(n);
            for r in 0..n {
                if d.is_null(r) {
                    let _ = bm.set(r, true);
                }
            }
            Ok(bm)
        }
        ZonePred::IsNotNull => {
            let mut bm = Bitmap::new_zeroed(n);
            for r in 0..n {
                if !d.is_null(r) {
                    let _ = bm.set(r, true);
                }
            }
            Ok(bm)
        }
        _ => {
            // Evaluate predicate on the small dictionary values table, then filter u32 codes via SIMD!
            let val_mask = eval_flat_pred(d.values(), pred, false)?;
            let ones = val_mask.count_ones();
            if ones == 0 {
                return Ok(Bitmap::new_zeroed(n));
            }
            if ones == d.values().len() {
                return Ok(match d.nulls() {
                    Some(code_nulls) => code_nulls.clone(),
                    None => Bitmap::new_ones(n),
                });
            }
            let mut bm = if ones == 1
                && let Some(target_code) = val_mask.iter_ones().next()
            {
                filter_u32(d.codes(), FilterOp::Eq, target_code as u32)
            } else {
                let mut out = Bitmap::new_zeroed(n);
                for (r, &c) in d.codes().iter().enumerate() {
                    if val_mask.get(c as usize) {
                        let _ = out.set(r, true);
                    }
                }
                out
            };
            if let Some(code_nulls) = d.nulls() {
                bm = bm.and(code_nulls)?;
            }
            Ok(bm)
        }
    }
}

fn eval_flat_pred(
    flat: &FlatColumn,
    pred: &ZonePred,
    has_nan: bool,
) -> Result<Bitmap, StatamError> {
    let n = flat.len();
    let nulls = flat.nulls();

    match pred {
        ZonePred::IsNull => {
            if matches!(flat.data(), FlatData::Null(_)) {
                return Ok(Bitmap::new_ones(n));
            }
            return Ok(match nulls {
                Some(bm) => bm.not(),
                None => Bitmap::new_zeroed(n),
            });
        }
        ZonePred::IsNotNull => {
            if matches!(flat.data(), FlatData::Null(_)) {
                return Ok(Bitmap::new_zeroed(n));
            }
            return Ok(match nulls {
                Some(bm) => bm.clone(),
                None => Bitmap::new_ones(n),
            });
        }
        _ => {}
    }

    if matches!(flat.data(), FlatData::Null(_)) {
        return Ok(Bitmap::new_zeroed(n));
    }

    let mut bm = match flat.data() {
        FlatData::Null(_) => Bitmap::new_zeroed(n),
        FlatData::I64(vals) | FlatData::TimestampNs { values: vals, .. } => match pred {
            ZonePred::Eq(ZoneValue::I64(x) | ZoneValue::TimestampNs(x)) => {
                filter_i64(vals, FilterOp::Eq, *x)
            }
            ZonePred::Ne(ZoneValue::I64(x) | ZoneValue::TimestampNs(x)) => {
                filter_i64(vals, FilterOp::Ne, *x)
            }
            ZonePred::Lt(ZoneValue::I64(x) | ZoneValue::TimestampNs(x)) => {
                filter_i64(vals, FilterOp::Lt, *x)
            }
            ZonePred::Le(ZoneValue::I64(x) | ZoneValue::TimestampNs(x)) => {
                filter_i64(vals, FilterOp::Le, *x)
            }
            ZonePred::Gt(ZoneValue::I64(x) | ZoneValue::TimestampNs(x)) => {
                filter_i64(vals, FilterOp::Gt, *x)
            }
            ZonePred::Ge(ZoneValue::I64(x) | ZoneValue::TimestampNs(x)) => {
                filter_i64(vals, FilterOp::Ge, *x)
            }
            ZonePred::Between(
                ZoneValue::I64(lo) | ZoneValue::TimestampNs(lo),
                ZoneValue::I64(hi) | ZoneValue::TimestampNs(hi),
            ) => filter_i64_between(vals, *lo, *hi),
            ZonePred::In(vs) => {
                let mut acc = Bitmap::new_zeroed(n);
                for v in vs {
                    if let ZoneValue::I64(x) | ZoneValue::TimestampNs(x) = v {
                        acc = acc.or(&filter_i64(vals, FilterOp::Eq, *x))?;
                    }
                }
                acc
            }
            _ => Bitmap::new_zeroed(n),
        },
        FlatData::F64(vals) => {
            let eval_one = |op: FilterOp, rhs: f64| -> Bitmap {
                if !has_nan && !rhs.is_nan() {
                    filter_f64(vals, op, rhs)
                } else {
                    filter_f64_total_cmp(vals, op, rhs)
                }
            };
            match pred {
                ZonePred::Eq(ZoneValue::F64(x)) => eval_one(FilterOp::Eq, *x),
                ZonePred::Ne(ZoneValue::F64(x)) => eval_one(FilterOp::Ne, *x),
                ZonePred::Lt(ZoneValue::F64(x)) => eval_one(FilterOp::Lt, *x),
                ZonePred::Le(ZoneValue::F64(x)) => eval_one(FilterOp::Le, *x),
                ZonePred::Gt(ZoneValue::F64(x)) => eval_one(FilterOp::Gt, *x),
                ZonePred::Ge(ZoneValue::F64(x)) => eval_one(FilterOp::Ge, *x),
                ZonePred::Between(ZoneValue::F64(lo), ZoneValue::F64(hi)) => {
                    if f64_total_cmp(*lo, *hi) == std::cmp::Ordering::Greater {
                        Bitmap::new_zeroed(n)
                    } else if !has_nan && !lo.is_nan() && !hi.is_nan() {
                        let ge_lo = filter_f64(vals, FilterOp::Ge, *lo);
                        let le_hi = filter_f64(vals, FilterOp::Le, *hi);
                        ge_lo.and(&le_hi)?
                    } else {
                        let mut out = Bitmap::new_zeroed(n);
                        for (r, &v) in vals.iter().enumerate() {
                            if f64_total_cmp(v, *lo) != std::cmp::Ordering::Less
                                && f64_total_cmp(v, *hi) != std::cmp::Ordering::Greater
                            {
                                let _ = out.set(r, true);
                            }
                        }
                        out
                    }
                }
                ZonePred::In(vs) => {
                    let mut acc = Bitmap::new_zeroed(n);
                    for v in vs {
                        if let ZoneValue::F64(x) = v {
                            acc = acc.or(&eval_one(FilterOp::Eq, *x))?;
                        }
                    }
                    acc
                }
                _ => Bitmap::new_zeroed(n),
            }
        }
        FlatData::Date(vals) => eval_ord_slice(vals, pred, |zv| match zv {
            ZoneValue::Date(d) => Some(*d),
            _ => None,
        }),
        FlatData::Decimal128 { values, .. } => eval_ord_slice(values, pred, |zv| match zv {
            ZoneValue::Decimal128(d) => Some(*d),
            _ => None,
        }),
        FlatData::Bool(b_bm) => {
            let mut out = Bitmap::new_zeroed(n);
            for r in 0..n {
                if eval_ord_val(b_bm.get(r), pred, |zv| match zv {
                    ZoneValue::Bool(b) => Some(*b),
                    _ => None,
                }) {
                    let _ = out.set(r, true);
                }
            }
            out
        }
        FlatData::Utf8(u) => eval_bytes_rows(n, |r| u.get(r).map(str::as_bytes), pred),
        FlatData::Binary(b) => eval_bytes_rows(n, |r| b.get(r), pred),
        FlatData::FixedBytes8(v) => eval_bytes_rows(n, |r| Some(v[r].as_slice()), pred),
        FlatData::FixedBytes16(v) => eval_bytes_rows(n, |r| Some(v[r].as_slice()), pred),
        FlatData::Interval(_)
        | FlatData::List { .. }
        | FlatData::MapUtf8(_)
        | FlatData::Events(_)
        | FlatData::Links(_) => Bitmap::new_zeroed(n),
    };

    if let Some(nbm) = nulls {
        bm = bm.and(nbm)?;
    }
    Ok(bm)
}

fn eval_ord_slice<T: Ord + Copy>(
    vals: &[T],
    pred: &ZonePred,
    extract: impl Fn(&ZoneValue) -> Option<T>,
) -> Bitmap {
    let mut out = Bitmap::new_zeroed(vals.len());
    for (r, &v) in vals.iter().enumerate() {
        if eval_ord_val(v, pred, &extract) {
            let _ = out.set(r, true);
        }
    }
    out
}

fn eval_ord_val<T: Ord + Copy>(
    val: T,
    pred: &ZonePred,
    extract: impl Fn(&ZoneValue) -> Option<T>,
) -> bool {
    match pred {
        ZonePred::Eq(v) => extract(v).is_some_and(|x| val == x),
        ZonePred::Ne(v) => extract(v).is_some_and(|x| val != x),
        ZonePred::Lt(v) => extract(v).is_some_and(|x| val < x),
        ZonePred::Le(v) => extract(v).is_some_and(|x| val <= x),
        ZonePred::Gt(v) => extract(v).is_some_and(|x| val > x),
        ZonePred::Ge(v) => extract(v).is_some_and(|x| val >= x),
        ZonePred::Between(lo, hi) => match (extract(lo), extract(hi)) {
            (Some(l), Some(h)) => val >= l && val <= h,
            _ => false,
        },
        ZonePred::In(vs) => vs.iter().any(|v| extract(v).is_some_and(|x| val == x)),
        ZonePred::IsNull | ZonePred::IsNotNull | ZonePred::Prefix(_) => false,
    }
}

fn eval_bytes_rows<'a>(
    n: usize,
    get: impl Fn(usize) -> Option<&'a [u8]>,
    pred: &ZonePred,
) -> Bitmap {
    let mut out = Bitmap::new_zeroed(n);
    for r in 0..n {
        let Some(b) = get(r) else {
            continue;
        };
        let hit = match pred {
            ZonePred::Eq(ZoneValue::Bytes(t)) => b == t.as_slice(),
            ZonePred::Ne(ZoneValue::Bytes(t)) => b != t.as_slice(),
            ZonePred::Lt(ZoneValue::Bytes(t)) => b < t.as_slice(),
            ZonePred::Le(ZoneValue::Bytes(t)) => b <= t.as_slice(),
            ZonePred::Gt(ZoneValue::Bytes(t)) => b > t.as_slice(),
            ZonePred::Ge(ZoneValue::Bytes(t)) => b >= t.as_slice(),
            ZonePred::Between(ZoneValue::Bytes(lo), ZoneValue::Bytes(hi)) => {
                b >= lo.as_slice() && b <= hi.as_slice()
            }
            ZonePred::In(vs) => vs
                .iter()
                .any(|v| matches!(v, ZoneValue::Bytes(t) if b == t.as_slice())),
            ZonePred::Prefix(p) => b.starts_with(p),
            _ => false,
        };
        if hit {
            let _ = out.set(r, true);
        }
    }
    out
}

fn eval_scalar_pred(val: &ScalarValue, pred: &ZonePred) -> bool {
    if val.is_null() {
        return matches!(pred, ZonePred::IsNull);
    }
    if matches!(pred, ZonePred::IsNotNull) {
        return true;
    }
    if matches!(pred, ZonePred::IsNull) {
        return false;
    }
    match val {
        ScalarValue::Bool(b) => eval_ord_val(*b, pred, |zv| match zv {
            ZoneValue::Bool(x) => Some(*x),
            _ => None,
        }),
        ScalarValue::I64(v) | ScalarValue::TimestampNs { value: v, .. } => {
            eval_ord_val(*v, pred, |zv| match zv {
                ZoneValue::I64(x) | ZoneValue::TimestampNs(x) => Some(*x),
                _ => None,
            })
        }
        ScalarValue::Date(v) => eval_ord_val(*v, pred, |zv| match zv {
            ZoneValue::Date(x) => Some(*x),
            _ => None,
        }),
        ScalarValue::Decimal128 { value: v, .. } => eval_ord_val(*v, pred, |zv| match zv {
            ZoneValue::Decimal128(x) => Some(*x),
            _ => None,
        }),
        ScalarValue::F64(v) => match pred {
            ZonePred::Eq(ZoneValue::F64(x)) => f64_total_cmp(*v, *x) == std::cmp::Ordering::Equal,
            ZonePred::Ne(ZoneValue::F64(x)) => f64_total_cmp(*v, *x) != std::cmp::Ordering::Equal,
            ZonePred::Lt(ZoneValue::F64(x)) => f64_total_cmp(*v, *x) == std::cmp::Ordering::Less,
            ZonePred::Le(ZoneValue::F64(x)) => {
                f64_total_cmp(*v, *x) != std::cmp::Ordering::Greater
            }
            ZonePred::Gt(ZoneValue::F64(x)) => {
                f64_total_cmp(*v, *x) == std::cmp::Ordering::Greater
            }
            ZonePred::Ge(ZoneValue::F64(x)) => f64_total_cmp(*v, *x) != std::cmp::Ordering::Less,
            ZonePred::Between(ZoneValue::F64(lo), ZoneValue::F64(hi)) => {
                f64_total_cmp(*v, *lo) != std::cmp::Ordering::Less
                    && f64_total_cmp(*v, *hi) != std::cmp::Ordering::Greater
            }
            ZonePred::In(vs) => vs.iter().any(|z| {
                matches!(z, ZoneValue::F64(x) if f64_total_cmp(*v, *x) == std::cmp::Ordering::Equal)
            }),
            _ => false,
        },
        ScalarValue::Utf8(s) => eval_bytes_val(s.as_bytes(), pred),
        ScalarValue::Binary(b) => eval_bytes_val(b, pred),
        ScalarValue::FixedBytes8(b) => eval_bytes_val(b, pred),
        ScalarValue::FixedBytes16(b) => eval_bytes_val(b, pred),
        _ => false,
    }
}

fn eval_bytes_val(b: &[u8], pred: &ZonePred) -> bool {
    match pred {
        ZonePred::Eq(ZoneValue::Bytes(t)) => b == t.as_slice(),
        ZonePred::Ne(ZoneValue::Bytes(t)) => b != t.as_slice(),
        ZonePred::Lt(ZoneValue::Bytes(t)) => b < t.as_slice(),
        ZonePred::Le(ZoneValue::Bytes(t)) => b <= t.as_slice(),
        ZonePred::Gt(ZoneValue::Bytes(t)) => b > t.as_slice(),
        ZonePred::Ge(ZoneValue::Bytes(t)) => b >= t.as_slice(),
        ZonePred::Between(ZoneValue::Bytes(lo), ZoneValue::Bytes(hi)) => {
            b >= lo.as_slice() && b <= hi.as_slice()
        }
        ZonePred::In(vs) => vs
            .iter()
            .any(|v| matches!(v, ZoneValue::Bytes(t) if b == t.as_slice())),
        ZonePred::Prefix(p) => b.starts_with(p),
        _ => false,
    }
}

fn eval_shredded_subcol_pred(sub: &ShreddedSubCol, pred: &ZonePred) -> Result<Bitmap, StatamError> {
    let n = sub.in_map.len();
    match pred {
        ZonePred::IsNull => {
            // Key absent OR key present with SQL NULL value.
            let present_valid = sub.in_map.and(&sub.value_null)?;
            Ok(present_valid.not())
        }
        ZonePred::IsNotNull => Ok(sub.in_map.and(&sub.value_null)?),
        _ => {
            let val_mask = eval_column_pred(&sub.values, pred, false)?;
            let present_valid = sub.in_map.and(&sub.value_null)?;
            if present_valid.len() != n {
                return Err(StatamError::Corrupt {
                    reason: "shredded subcol bitmap length mismatch",
                });
            }
            Ok(val_mask.and(&present_valid)?)
        }
    }
}

fn eval_map_key_pred(
    map_col: &FlatColumn,
    key: &str,
    pred: &ZonePred,
) -> Result<Bitmap, StatamError> {
    let FlatData::MapUtf8(map) = map_col.data() else {
        return Err(StatamError::SchemaMismatch {
            reason: "eval_map_key_pred expects MapUtf8",
        });
    };
    let n = map_col.len();
    let mut out = Bitmap::new_zeroed(n);
    let offsets = map.offsets();
    let keys = map.keys();
    let vals = map.values();
    let val_nulls = map.value_nulls();
    let row_nulls = map_col.nulls();

    for r in 0..n {
        if row_nulls.is_some_and(|bm| !bm.get(r)) {
            if matches!(pred, ZonePred::IsNull) {
                let _ = out.set(r, true);
            }
            continue;
        }
        let s = offsets[r] as usize;
        let e = offsets[r + 1] as usize;
        let mut found: Option<Option<&[u8]>> = None;
        for idx in s..e {
            if keys.get(idx) == Some(key) {
                if val_nulls.is_some_and(|bm| !bm.get(idx)) {
                    found = Some(None);
                } else {
                    found = Some(vals.get(idx).map(str::as_bytes));
                }
            }
        }
        let hit = match (found, pred) {
            (None | Some(None), ZonePred::IsNull) => true,
            (Some(Some(_)), ZonePred::IsNotNull) => true,
            (Some(Some(b)), _) => eval_bytes_val(b, pred),
            _ => false,
        };
        if hit {
            let _ = out.set(r, true);
        }
    }
    Ok(out)
}

fn filter_residual_to_keys(
    map_col: &FlatColumn,
    keys_keep: &[String],
) -> Result<FlatColumn, StatamError> {
    let FlatData::MapUtf8(map) = map_col.data() else {
        return Err(StatamError::SchemaMismatch {
            reason: "filter_residual_to_keys expects MapUtf8",
        });
    };
    let n = map_col.len();
    let mut out_offsets = Vec::with_capacity(n + 1);
    out_offsets.push(0u32);
    let mut out_keys = Vec::new();
    let mut out_vals = Vec::new();
    let mut out_val_valid = Vec::new();

    let offsets = map.offsets();
    let mkeys = map.keys();
    let mvals = map.values();
    let mval_nulls = map.value_nulls();
    let row_nulls = map_col.nulls();

    for r in 0..n {
        if row_nulls.is_none_or(|bm| bm.get(r)) {
            let s = offsets[r] as usize;
            let e = offsets[r + 1] as usize;
            for idx in s..e {
                let k = mkeys.get(idx).unwrap_or("");
                if keys_keep.iter().any(|keep| keep == k) {
                    let valid = mval_nulls.is_none_or(|bm| bm.get(idx));
                    let v = if valid {
                        mvals.get(idx).unwrap_or("")
                    } else {
                        ""
                    };
                    out_keys.push(k);
                    out_vals.push(v);
                    out_val_valid.push(valid);
                }
            }
        }
        out_offsets.push(out_keys.len() as u32);
    }

    let keys_data = Utf8Data::try_from_slices(out_keys)?;
    let vals_data = Utf8Data::try_from_slices(out_vals)?;
    let val_nulls = if out_val_valid.iter().all(|&b| b) {
        None
    } else {
        Some(Bitmap::from_bools(&out_val_valid))
    };
    let m = cetra_column::MapData::try_new(out_offsets, keys_data, vals_data, val_nulls)?;
    Ok(FlatColumn::try_new(
        FlatData::MapUtf8(Box::new(m)),
        row_nulls.cloned(),
    )?)
}

fn eval_utf8_substring(
    col: &Column,
    needle: &[u8],
    case_insensitive: bool,
    precomputed_dict_bm: Option<&Bitmap>,
) -> Result<Bitmap, StatamError> {
    let n = col.len();
    if needle.is_empty() {
        let mut out = Bitmap::new_zeroed(n);
        for r in 0..n {
            if !col.is_null(r) {
                let _ = out.set(r, true);
            }
        }
        return Ok(out);
    }
    match col {
        Column::Const { value, .. } => {
            let mut matcher = Utf8Matcher::new(needle, case_insensitive);
            let hit = match value {
                ScalarValue::Utf8(s) => matcher.matches(s.as_bytes()),
                _ => false,
            };
            Ok(if hit {
                Bitmap::new_ones(n)
            } else {
                Bitmap::new_zeroed(n)
            })
        }
        Column::Dict(d) => {
            let owned_bm;
            let val_match = if let Some(bm) = precomputed_dict_bm
                && bm.len() == d.values().len()
            {
                bm
            } else {
                let mut matcher = Utf8Matcher::new(needle, case_insensitive);
                let mut vm = Bitmap::new_zeroed(d.values().len());
                if let FlatData::Utf8(u) = d.values().data() {
                    for i in 0..u.len() {
                        if let Some(s) = u.get(i)
                            && matcher.matches(s.as_bytes())
                        {
                            let _ = vm.set(i, true);
                        }
                    }
                }
                owned_bm = vm;
                &owned_bm
            };
            if val_match.all_zeros() {
                return Ok(Bitmap::new_zeroed(n));
            }
            if val_match.all_ones() {
                return Ok(match d.nulls() {
                    Some(nulls) => nulls.clone(),
                    None => Bitmap::new_ones(n),
                });
            }
            let mut out = Bitmap::new_zeroed(n);
            for (r, &c) in d.codes().iter().enumerate() {
                if !d.is_null(r) && val_match.get(c as usize) {
                    let _ = out.set(r, true);
                }
            }
            Ok(out)
        }
        Column::Rle(r) => {
            let mut matcher = Utf8Matcher::new(needle, case_insensitive);
            let mut run_match = Bitmap::new_zeroed(r.values().len());
            if let FlatData::Utf8(u) = r.values().data() {
                for i in 0..u.len() {
                    if !r.values().is_null(i)
                        && let Some(s) = u.get(i)
                        && matcher.matches(s.as_bytes())
                    {
                        let _ = run_match.set(i, true);
                    }
                }
            }
            let mut out = Bitmap::new_zeroed(n);
            let mut prev = 0u32;
            for (run_idx, &end) in r.run_ends().iter().enumerate() {
                if run_match.get(run_idx) {
                    for row in prev as usize..end as usize {
                        let _ = out.set(row, true);
                    }
                }
                prev = end;
            }
            Ok(out)
        }
        Column::Flat(f) => {
            let mut matcher = Utf8Matcher::new(needle, case_insensitive);
            let mut out = Bitmap::new_zeroed(n);
            if let FlatData::Utf8(u) = f.data() {
                let nulls = f.nulls();
                for r in 0..n {
                    if nulls.is_none_or(|bm| bm.get(r))
                        && let Some(s) = u.get(r)
                        && matcher.matches(s.as_bytes())
                    {
                        let _ = out.set(r, true);
                    }
                }
            }
            Ok(out)
        }
    }
}

fn truncate_batch_selection(batch: &Batch, keep: usize) -> Result<Batch, StatamError> {
    let raw_len = batch.raw_len();
    let new_sel = match batch.selection() {
        Selection::All => {
            let mut bm = Bitmap::new_zeroed(raw_len);
            for i in 0..keep.min(raw_len) {
                let _ = bm.set(i, true);
            }
            Selection::Mask(bm)
        }
        Selection::Mask(m) => {
            let mut bm = Bitmap::new_zeroed(raw_len);
            for idx in m.iter_ones().take(keep) {
                let _ = bm.set(idx, true);
            }
            Selection::Mask(bm)
        }
        Selection::Indices(v) => Selection::Indices(v[..keep.min(v.len())].to_vec()),
    };
    Ok(batch.with_selection(new_sel)?)
}

fn truncate_batch_selection_tail(batch: &Batch, keep: usize) -> Result<Batch, StatamError> {
    let raw_len = batch.raw_len();
    let new_sel = match batch.selection() {
        Selection::All => {
            let mut bm = Bitmap::new_zeroed(raw_len);
            let start = raw_len.saturating_sub(keep);
            for i in start..raw_len {
                let _ = bm.set(i, true);
            }
            Selection::Mask(bm)
        }
        Selection::Mask(m) => {
            let total_ones = m.count_ones();
            let skip = total_ones.saturating_sub(keep);
            let mut bm = Bitmap::new_zeroed(raw_len);
            for idx in m.iter_ones().skip(skip) {
                let _ = bm.set(idx, true);
            }
            Selection::Mask(bm)
        }
        Selection::Indices(v) => {
            let start = v.len().saturating_sub(keep);
            Selection::Indices(v[start..].to_vec())
        }
    };
    Ok(batch.with_selection(new_sel)?)
}

enum Utf8Matcher {
    Exact(memmem::Finder<'static>),
    AsciiIlike {
        finder: memmem::Finder<'static>,
        scratch: Vec<u8>,
    },
    UnicodeIlike {
        finder: memmem::Finder<'static>,
        needle_raw_lower: Vec<u8>,
        scratch: Vec<u8>,
    },
}

impl Utf8Matcher {
    fn new(needle: &[u8], case_insensitive: bool) -> Self {
        if !case_insensitive {
            return Self::Exact(memmem::Finder::new(needle).into_owned());
        }
        if needle.is_ascii() {
            let mut lowered = Vec::new();
            ascii_lowercase_into(needle, &mut lowered);
            return Self::AsciiIlike {
                finder: memmem::Finder::new(&lowered).into_owned(),
                scratch: Vec::new(),
            };
        }
        let mut ascii_lowered = Vec::new();
        ascii_lowercase_into(needle, &mut ascii_lowered);
        match std::str::from_utf8(needle) {
            Ok(s) => {
                let uni_lower = s.to_lowercase();
                Self::UnicodeIlike {
                    finder: memmem::Finder::new(uni_lower.as_bytes()).into_owned(),
                    needle_raw_lower: ascii_lowered,
                    scratch: Vec::new(),
                }
            }
            Err(_) => Self::AsciiIlike {
                finder: memmem::Finder::new(&ascii_lowered).into_owned(),
                scratch: Vec::new(),
            },
        }
    }

    fn matches(&mut self, haystack: &[u8]) -> bool {
        match self {
            Self::Exact(finder) => finder.find(haystack).is_some(),
            Self::AsciiIlike { finder, scratch } => {
                ascii_lowercase_into(haystack, scratch);
                finder.find(scratch).is_some()
            }
            Self::UnicodeIlike {
                finder,
                needle_raw_lower,
                scratch,
            } => {
                if let Ok(s) = std::str::from_utf8(haystack) {
                    finder.find(s.to_lowercase().as_bytes()).is_some()
                } else {
                    ascii_lowercase_into(haystack, scratch);
                    memmem::find(scratch, needle_raw_lower).is_some()
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use cetra_column::{BinaryData, EventsData, Field, Interval, LinksData, MapData, RleColumn};

    use super::*;
    use crate::WriteProfile;
    use crate::format::{COL_FLAG_BLOOM_IN_HOT_TAIL, TableKind};
    use crate::io::{SimulatedS3Config, SimulatedS3Reader, SliceReader};
    use crate::payload::pack::{ChunkPackBuilder, PackWriteOptions};
    use crate::writer::{StatamWriter, WriteOptions};

    fn test_pack_opts() -> PackWriteOptions {
        PackWriteOptions {
            #[cfg(miri)]
            codec: Codec::Lz4,
            #[cfg(not(miri))]
            codec: Codec::Zstd,
            zstd_level: 3,
            target_frame_raw_bytes: 16 * 1024,
            enable_trigram_skip: true,
            compress_toc_lz4: true,
        }
    }

    fn make_spans_schema() -> Arc<Schema> {
        Schema::try_new_arc(vec![
            Field::new("trace_id", DataType::FixedBytes16, false),
            Field::new("span_id", DataType::FixedBytes8, false),
            Field::new("start_time", DataType::TimestampNs { utc: true }, false),
            Field::new("name", DataType::Utf8, false),
            Field::new("status_code", DataType::Utf8, false),
            Field::new("model", DataType::Utf8, true),
            Field::new("session_id", DataType::Utf8, true),
            Field::new("user_id", DataType::Utf8, true),
            Field::new("cost_usd", DataType::cost_decimal(), true),
            Field::new("latency_ms", DataType::F64, true),
            Field::new("is_error", DataType::Bool, false),
            Field::new("tags", DataType::List(Box::new(DataType::Utf8)), true),
            Field::new("attributes", DataType::MapUtf8, true),
            Field::new("events", DataType::Events, true),
            Field::new("links", DataType::Links, true),
            Field::new("input", DataType::Utf8, true),
            Field::new("output", DataType::Utf8, true),
        ])
        .unwrap()
    }

    fn make_spans_batch(schema: &Arc<Schema>, start_row: usize, count: usize) -> Batch {
        let mut trace_ids = Vec::with_capacity(count);
        let mut span_ids = Vec::with_capacity(count);
        let mut start_times = Vec::with_capacity(count);
        let mut names = Vec::with_capacity(count);
        let mut status_codes = Vec::with_capacity(count);
        let mut models = Vec::with_capacity(count);
        let mut session_ids = Vec::with_capacity(count);
        let mut user_ids = Vec::with_capacity(count);
        let mut costs = Vec::with_capacity(count);
        let mut latencies = Vec::with_capacity(count);
        let mut is_errors = Vec::with_capacity(count);

        let mut tag_offsets = vec![0u32];
        let mut tag_items = Vec::new();
        let mut map_offsets = vec![0u32];
        let mut map_keys = Vec::new();
        let mut map_vals = Vec::new();
        let mut map_val_valid = Vec::new();
        let mut ev_offsets = vec![0u32];
        let mut ev_times = Vec::new();
        let mut ev_names = Vec::new();
        let mut ev_attrs = Vec::new();
        let mut lk_offsets = vec![0u32];
        let mut lk_tids = Vec::new();
        let mut lk_sids = Vec::new();
        let mut lk_attrs = Vec::new();
        let mut inputs = Vec::with_capacity(count);
        let mut outputs = Vec::with_capacity(count);

        for i in 0..count {
            let r = start_row + i;
            let mut tid = [0u8; 16];
            tid[0..8].copy_from_slice(&((r / 4) as u64).to_be_bytes());
            tid[15] = 0xAA;
            trace_ids.push(tid);

            let mut sid = [0u8; 8];
            sid.copy_from_slice(&(r as u64).to_be_bytes());
            span_ids.push(sid);

            start_times.push(1_700_000_000_000_000_000i64 + (r as i64) * 1_000_000);
            names.push(if r.is_multiple_of(10) {
                "llm.completion".to_owned()
            } else if r.is_multiple_of(3) {
                "tool.execute".to_owned()
            } else {
                "agent.step".to_owned()
            });
            status_codes.push(if r.is_multiple_of(7) { "ERROR" } else { "OK" });
            models.push(if r.is_multiple_of(2) {
                "gpt-4o"
            } else {
                "claude-3-5-sonnet"
            });
            session_ids.push(format!("sess_{}", r / 8));
            user_ids.push(format!("user_{}", r % 5));
            costs.push((r as i128 + 1) * 25_000_000_000_000);
            latencies.push(if r.is_multiple_of(29) {
                f64::NAN
            } else if r.is_multiple_of(17) {
                -0.0
            } else {
                (r % 500) as f64 * 1.25
            });
            is_errors.push(r.is_multiple_of(7));

            tag_items.push("prod");
            if r.is_multiple_of(2) {
                tag_items.push("streaming");
            }
            tag_offsets.push(tag_items.len() as u32);

            // Shredded hot keys (> 50% presence) + rare key.
            map_keys.push("customer_id".to_owned());
            map_vals.push(format!("cust_{}", r % 12));
            map_val_valid.push(true);
            map_keys.push("env".to_owned());
            map_vals.push("production".to_owned());
            map_val_valid.push(true);
            if r.is_multiple_of(50) {
                map_keys.push(format!("rare_{r}"));
                map_vals.push("v".to_owned());
                map_val_valid.push(true);
            }
            map_offsets.push(map_keys.len() as u32);

            if r.is_multiple_of(3) {
                ev_times.push(1_700_000_000_000_000_000i64 + r as i64);
                ev_names.push("token");
                ev_attrs.push("{\"count\":1}");
            }
            ev_offsets.push(ev_times.len() as u32);

            if r.is_multiple_of(5) {
                lk_tids.push(tid);
                lk_sids.push(sid);
                lk_attrs.push("{\"rel\":\"parent\"}");
            }
            lk_offsets.push(lk_tids.len() as u32);

            if r.is_multiple_of(4) {
                inputs.push(format!("short prompt {r}"));
            } else {
                let p0 = format!(
                    "{{\"role\":\"system\",\"content\":\"{}\"}}",
                    "S".repeat(280)
                );
                let p1 = format!(
                    "{{\"role\":\"user\",\"content\":\"row {r} {}\"}}",
                    "U".repeat(300)
                );
                inputs.push(format!("[{p0},{p1}]"));
            }
            outputs.push(format!(
                "{{\"answer\":\"response for row {r} {}\"}}",
                "R".repeat(300)
            ));
        }

        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let sess_refs: Vec<&str> = session_ids.iter().map(String::as_str).collect();
        let user_refs: Vec<&str> = user_ids.iter().map(String::as_str).collect();
        let mk_refs: Vec<&str> = map_keys.iter().map(String::as_str).collect();
        let mv_refs: Vec<&str> = map_vals.iter().map(String::as_str).collect();
        let in_refs: Vec<&str> = inputs.iter().map(String::as_str).collect();
        let out_refs: Vec<&str> = outputs.iter().map(String::as_str).collect();

        let cols = vec![
            Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(trace_ids), None).unwrap()),
            Column::Flat(FlatColumn::try_new(FlatData::FixedBytes8(span_ids), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: start_times,
                        utc: true,
                    },
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(name_refs).unwrap()),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(status_codes).unwrap()),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(models).unwrap()),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(sess_refs).unwrap()),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(user_refs).unwrap()),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Decimal128 {
                        values: costs,
                        precision: 38,
                        scale: 18,
                    },
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(FlatColumn::try_new(FlatData::F64(latencies), None).unwrap()),
            Column::Flat(
                FlatColumn::try_new(FlatData::Bool(Bitmap::from_bools(&is_errors)), None).unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::List {
                        item_type: DataType::Utf8,
                        offsets: tag_offsets,
                        values: Box::new(Column::Flat(
                            FlatColumn::try_new(
                                FlatData::Utf8(Utf8Data::try_from_slices(tag_items).unwrap()),
                                None,
                            )
                            .unwrap(),
                        )),
                    },
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::MapUtf8(Box::new(
                        MapData::try_new(
                            map_offsets,
                            Utf8Data::try_from_slices(mk_refs).unwrap(),
                            Utf8Data::try_from_slices(mv_refs).unwrap(),
                            None,
                        )
                        .unwrap(),
                    )),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Events(Box::new(
                        EventsData::try_new(
                            ev_offsets,
                            ev_times,
                            Utf8Data::try_from_slices(ev_names).unwrap(),
                            Utf8Data::try_from_slices(ev_attrs).unwrap(),
                            None,
                        )
                        .unwrap(),
                    )),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Links(Box::new(
                        LinksData::try_new(
                            lk_offsets,
                            lk_tids,
                            lk_sids,
                            Utf8Data::try_from_slices(lk_attrs).unwrap(),
                            None,
                        )
                        .unwrap(),
                    )),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(in_refs).unwrap()),
                    None,
                )
                .unwrap(),
            ),
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(out_refs).unwrap()),
                    None,
                )
                .unwrap(),
            ),
        ];

        Batch::try_new_all(Arc::clone(schema), cols, count).unwrap()
    }

    fn concat_scanned_batches(schema: &Arc<Schema>, batches: &[Batch]) -> Batch {
        if batches.is_empty() {
            return Batch::empty(Arc::clone(schema));
        }
        let refs: Vec<&Batch> = batches.iter().collect();
        Batch::concat(Arc::clone(schema), &refs, None).unwrap()
    }

    #[test]
    fn end_to_end_roundtrip_spans_traces_scores_l0_and_l1() {
        // 1. Spans (multi-batch, L0Fast & L1Compact with small RG/MB sizes to exercise multi-RG streaming).
        let spans_schema = make_spans_schema();
        let b0 = make_spans_batch(&spans_schema, 0, 140);
        let b1 = make_spans_batch(&spans_schema, 140, 110);
        let expected_spans = Batch::concat(Arc::clone(&spans_schema), &[&b0, &b1], None).unwrap();

        for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
            let mut opts = WriteOptions::for_table(TableKind::Spans, profile);
            opts.sort_cols = vec![2, 0];
            opts.row_group_size = 128;
            opts.mini_block_size = 64;

            let mut resolver = InMemoryPackResolver::new();
            let mut file_bytes = Vec::new();
            let stats = if profile == WriteProfile::L1Compact {
                let pack_id = [0x11u8; 16];
                opts.pack_id = Some(pack_id);
                let mut pb = ChunkPackBuilder::new(pack_id, test_pack_opts());
                let mut writer =
                    StatamWriter::new(Arc::clone(&spans_schema), opts, Some(&mut pb)).unwrap();
                writer.write_batch(&b0).unwrap();
                writer.write_batch(&b1).unwrap();
                let stats = writer.finish(&mut file_bytes).unwrap();
                let mut pack_bytes = Vec::new();
                pb.finish(&mut pack_bytes).unwrap();
                resolver.insert_bytes(pack_id, pack_bytes).unwrap();
                stats
            } else {
                let mut writer = StatamWriter::new(Arc::clone(&spans_schema), opts, None).unwrap();
                writer.write_batch(&b0).unwrap();
                writer.write_batch(&b1).unwrap();
                writer.finish(&mut file_bytes).unwrap()
            };

            assert_eq!(stats.row_count, 250);
            assert_eq!(stats.num_row_groups, 2);

            let reader = StatamReader::open(
                Arc::new(SliceReader::new(file_bytes)),
                Some(Arc::new(resolver)),
            )
            .unwrap();
            reader.verify_full().unwrap();

            let got = reader.scan(&ScanRequest::default()).unwrap();
            let actual = concat_scanned_batches(&spans_schema, &got);
            assert_eq!(actual.num_rows(), expected_spans.num_rows());
            for c in 0..spans_schema.len() {
                assert!(
                    actual
                        .column(c)
                        .unwrap()
                        .semantic_eq(expected_spans.column(c).unwrap()),
                    "spans roundtrip mismatch at col {c} ({}) for {profile:?}",
                    spans_schema.fields()[c].name
                );
            }
        }

        // 2. Traces table roundtrip (L0Fast & L1Compact).
        let traces_schema = Schema::try_new_arc(vec![
            Field::new("trace_id", DataType::FixedBytes16, false),
            Field::new("session_id", DataType::Utf8, true),
            Field::new("user_id", DataType::Utf8, true),
            Field::new("start_time", DataType::TimestampNs { utc: true }, false),
            Field::new("duration_ns", DataType::I64, false),
            Field::new("total_cost", DataType::cost_decimal(), true),
            Field::new("metadata", DataType::MapUtf8, true),
        ])
        .unwrap();
        let n = 64;
        let tids: Vec<[u8; 16]> = (0..n).map(|i| [i as u8; 16]).collect();
        let sess: Vec<&str> = (0..n)
            .map(|i| if i % 2 == 0 { "s0" } else { "s1" })
            .collect();
        let users: Vec<&str> = (0..n)
            .map(|i| if i % 3 == 0 { "u0" } else { "u1" })
            .collect();
        let ts: Vec<i64> = (0..n).map(|i| 1_000_000 + i as i64 * 10).collect();
        let dur: Vec<i64> = (0..n).map(|i| 500 + i as i64).collect();
        let cost: Vec<i128> = (0..n).map(|i| 1000 + i as i128).collect();
        let trace_batch = Batch::try_new_all(
            Arc::clone(&traces_schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(tids), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(sess).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(users).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: ts,
                            utc: true,
                        },
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(FlatColumn::try_new(FlatData::I64(dur), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Decimal128 {
                            values: cost,
                            precision: 38,
                            scale: 18,
                        },
                        None,
                    )
                    .unwrap(),
                ),
                Column::try_new_const(ScalarValue::MapUtf8(Vec::new()), n).unwrap(),
            ],
            n,
        )
        .unwrap();

        for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
            let opts = WriteOptions::for_table(TableKind::Traces, profile);
            let mut buf = Vec::new();
            let mut w = StatamWriter::new(Arc::clone(&traces_schema), opts, None).unwrap();
            w.write_batch(&trace_batch).unwrap();
            w.finish(&mut buf).unwrap();
            let r = StatamReader::open(Arc::new(SliceReader::new(buf)), None).unwrap();
            r.verify_full().unwrap();
            let got =
                concat_scanned_batches(&traces_schema, &r.scan(&ScanRequest::default()).unwrap());
            for c in 0..traces_schema.len() {
                assert!(
                    got.column(c)
                        .unwrap()
                        .semantic_eq(trace_batch.column(c).unwrap())
                );
            }
        }

        // 3. Scores table roundtrip (L0Fast & L1Compact).
        let scores_schema = Schema::try_new_arc(vec![
            Field::new("trace_id", DataType::FixedBytes16, false),
            Field::new("score_id", DataType::FixedBytes16, false),
            Field::new("timestamp", DataType::TimestampNs { utc: true }, false),
            Field::new("name", DataType::Utf8, false),
            Field::new("value", DataType::F64, false),
            Field::new("date", DataType::Date, false),
            Field::new("window", DataType::Interval, true),
            Field::new("raw_bytes", DataType::Binary, true),
        ])
        .unwrap();
        let sc_tids: Vec<[u8; 16]> = (0..n).map(|i| [i as u8; 16]).collect();
        let sc_ids: Vec<[u8; 16]> = (0..n).map(|i| [(i + 100) as u8; 16]).collect();
        let sc_ts: Vec<i64> = (0..n).map(|i| 5_000_000 + i as i64).collect();
        let sc_names = vec!["faithfulness"; n];
        let sc_vals: Vec<f64> = (0..n).map(|i| (i as f64) * 0.01).collect();
        let sc_dates: Vec<i32> = vec![19_800; n];
        let sc_ivs: Vec<Interval> = vec![
            Interval {
                months: 0,
                days: 1,
                nanos: 100,
            };
            n
        ];
        let sc_bins: Vec<&[u8]> = vec![b"\x01\x02\x03"; n];
        let scores_batch = Batch::try_new_all(
            Arc::clone(&scores_schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(sc_tids), None).unwrap()),
                Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(sc_ids), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: sc_ts,
                            utc: true,
                        },
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(sc_names).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(FlatColumn::try_new(FlatData::F64(sc_vals), None).unwrap()),
                Column::Flat(FlatColumn::try_new(FlatData::Date(sc_dates), None).unwrap()),
                Column::Flat(FlatColumn::try_new(FlatData::Interval(sc_ivs), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Binary(BinaryData::try_from_slices(sc_bins).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            n,
        )
        .unwrap();

        for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
            let opts = WriteOptions::for_table(TableKind::Scores, profile);
            let mut buf = Vec::new();
            let mut w = StatamWriter::new(Arc::clone(&scores_schema), opts, None).unwrap();
            w.write_batch(&scores_batch).unwrap();
            w.finish(&mut buf).unwrap();
            let r = StatamReader::open(Arc::new(SliceReader::new(buf)), None).unwrap();
            r.verify_full().unwrap();
            let got =
                concat_scanned_batches(&scores_schema, &r.scan(&ScanRequest::default()).unwrap());
            for c in 0..scores_schema.len() {
                assert!(
                    got.column(c)
                        .unwrap()
                        .semantic_eq(scores_batch.column(c).unwrap())
                );
            }
        }
    }

    #[test]
    fn cold_open_is_1_get_and_point_lookup_is_le_2_gets_on_simulated_s3() {
        // Build a > 2 MiB L0Fast file across multiple RGs using incompressible ASCII payloads
        // so byte 0 and the Hot Tail lie in separate 1 MiB S3 ReadCache blocks!
        let schema = make_spans_schema();
        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L0Fast);
        opts.sort_cols = vec![2, 0];
        opts.row_group_size = 512;
        opts.mini_block_size = 256;

        let mut base_b0 = make_spans_batch(&schema, 0, 1024);
        let mut base_b1 = make_spans_batch(&schema, 1024, 1024);

        let make_incompressible_col = |seed_base: u64, count: usize| -> Column {
            let mut rng = seed_base;
            let mut vals = Vec::with_capacity(count);
            for _ in 0..count {
                let mut s = String::with_capacity(1400);
                for _ in 0..1400 {
                    rng = rng
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    s.push((b'!' + ((rng >> 56) as u8 % 90)) as char);
                }
                vals.push(s);
            }
            let refs: Vec<&str> = vals.iter().map(String::as_str).collect();
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(refs).unwrap()),
                    None,
                )
                .unwrap(),
            )
        };
        let replace_input = |batch: Batch, seed: u64| -> Batch {
            let (sc, mut cols, raw_len, sel) = batch.into_parts();
            cols[15] = make_incompressible_col(seed, raw_len);
            Batch::try_new(sc, cols, raw_len, sel).unwrap()
        };
        base_b0 = replace_input(base_b0, 0x1111_2222_3333_4444);
        base_b1 = replace_input(base_b1, 0x5555_6666_7777_8888);

        let mut writer = StatamWriter::new(Arc::clone(&schema), opts, None).unwrap();
        writer.write_batch(&base_b0).unwrap();
        writer.write_batch(&base_b1).unwrap();
        let mut file_bytes = Vec::new();
        writer.finish(&mut file_bytes).unwrap();
        assert!(
            file_bytes.len() > 2 * 1024 * 1024,
            "expected > 2 MiB file, got {}",
            file_bytes.len()
        );

        let s3 = Arc::new(SimulatedS3Reader::from_bytes(
            file_bytes,
            SimulatedS3Config::default(),
        ));

        // 1. Cold open must be strictly 1 GET!
        let reader = StatamReader::open(s3.clone(), None).unwrap();
        assert_eq!(s3.counters().gets, 1, "cold open must take exactly 1 GET");
        assert!(
            (reader.meta().col_dir[0].col_flags & COL_FLAG_BLOOM_IN_HOT_TAIL) != 0,
            "trace_id bloom must reside in Hot Tail"
        );

        // 2. Absent trace_id lookup must prune via Hot Bloom with 0 extra GETs!
        let absent_tid = [0xFFu8; 16];
        let absent_res = reader
            .scan(&ScanRequest {
                id_lookups: vec![IdLookup::eq("trace_id", absent_tid)],
                ..ScanRequest::default()
            })
            .unwrap();
        assert!(absent_res.is_empty());
        assert_eq!(
            s3.counters().gets,
            1,
            "absent trace_id must use 0 extra GETs"
        );

        // 3. Present trace_id point lookup (full SELECT * on L0Fast) -> total GETs <= 2!
        let target_row = 1200usize;
        let mut target_tid = [0u8; 16];
        target_tid[0..8].copy_from_slice(&((target_row / 4) as u64).to_be_bytes());
        target_tid[15] = 0xAA;

        let res = reader
            .scan(&ScanRequest {
                id_lookups: vec![IdLookup::eq("trace_id", target_tid)],
                coalesce_mode: CoalesceMode::Auto,
                ..ScanRequest::default()
            })
            .unwrap();
        let total_gets = s3.counters().gets;
        assert!(
            total_gets <= 2,
            "cold open + point lookup must be <= 2 GETs, got {total_gets}"
        );
        let out = concat_scanned_batches(&schema, &res);
        assert_eq!(out.num_rows(), 4);
    }

    #[test]
    fn late_materialization_skips_wide_columns_on_empty_filter() {
        let schema = Schema::try_new_arc(vec![
            Field::new("trace_id", DataType::FixedBytes16, false),
            Field::new("start_time", DataType::TimestampNs { utc: true }, false),
            Field::new("status_code", DataType::Utf8, false),
            Field::new("attributes", DataType::MapUtf8, true),
            Field::new("input", DataType::Utf8, true),
        ])
        .unwrap();
        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L0Fast);
        opts.payload_cols = vec!["input".to_owned()];
        opts.row_group_size = 4096;
        opts.mini_block_size = 2048;

        let n = 4096usize;
        let tids: Vec<[u8; 16]> = (0..n).map(|i| [(i & 0xFF) as u8; 16]).collect();
        let ts: Vec<i64> = (0..n).map(|i| 1_000_000 + i as i64).collect();
        let status: Vec<&str> = (0..n)
            .map(|i| if i % 2 == 0 { "OK" } else { "ERR" })
            .collect();
        let mut map_offsets = vec![0u32];
        let mut map_keys = Vec::new();
        let mut map_vals = Vec::new();
        for i in 0..n {
            map_keys.push("k");
            map_vals.push(if i % 2 == 0 { "a" } else { "b" });
            map_offsets.push((i + 1) as u32);
        }
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        let inputs: Vec<String> = (0..n)
            .map(|i| {
                let mut s = format!("payload_{i}_");
                for _ in 0..700 {
                    rng = rng
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    s.push((b'!' + ((rng >> 56) as u8 % 90)) as char);
                }
                s
            })
            .collect();
        let in_refs: Vec<&str> = inputs.iter().map(String::as_str).collect();

        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(tids), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: ts,
                            utc: true,
                        },
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(status).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::MapUtf8(Box::new(
                            MapData::try_new(
                                map_offsets,
                                Utf8Data::try_from_slices(map_keys).unwrap(),
                                Utf8Data::try_from_slices(map_vals).unwrap(),
                                None,
                            )
                            .unwrap(),
                        )),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(in_refs).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            n,
        )
        .unwrap();

        let mut file_bytes = Vec::new();
        let mut w = StatamWriter::new(Arc::clone(&schema), opts, None).unwrap();
        w.write_batch(&batch).unwrap();
        w.finish(&mut file_bytes).unwrap();
        assert!(file_bytes.len() > 2 * 1024 * 1024);

        let s3 = Arc::new(SimulatedS3Reader::from_bytes(
            file_bytes,
            SimulatedS3Config::default(),
        ));
        let reader = StatamReader::open(s3.clone(), None).unwrap();
        let rg0 = &reader.meta().rg_dir[0];
        let core_end = rg0.core_offset + u64::from(rg0.core_len);
        assert!(core_end < 1024 * 1024);

        s3.reset_counters();
        // Phase 1 actually reads and evaluates `start_time == 1_000_000` (even row -> "OK") AND `status_code == "ERR"`,
        // so both mini-blocks pass zone maps on `start_time` (row 1_000_000 in mb0) / or `start_time == 1_000_000` passes mb0,
        // evaluates Phase 1 to 0 rows, and MUST NOT read WideRow `attributes` or ExternalPayload `input`!
        let res = reader
            .scan(&ScanRequest {
                projection: None,
                predicates: vec![
                    Predicate::new(
                        "start_time",
                        ZonePred::Eq(ZoneValue::TimestampNs(1_000_000)),
                    ),
                    Predicate::new(
                        "status_code",
                        ZonePred::Eq(ZoneValue::Bytes(b"ERR".to_vec())),
                    ),
                ],
                coalesce_mode: CoalesceMode::Scan,
                ..ScanRequest::default()
            })
            .unwrap();
        assert!(res.is_empty());
        assert!(
            s3.counters().bytes_requested <= u64::from(rg0.core_len) + 64,
            "late materialization must NOT fetch WideRow/L0Payload when Phase 1 yields 0 rows; requested {}",
            s3.counters().bytes_requested
        );

        // Verify `projection = Some(vec![])` emits exact active row count without reading WideRow/ExternalPayload!
        s3.reset_counters();
        let count_batches = reader
            .scan(&ScanRequest {
                projection: Some(vec![]),
                time_range: TimeRange::new(1_000_010, 1_000_059),
                coalesce_mode: CoalesceMode::Scan,
                ..ScanRequest::default()
            })
            .unwrap();
        let total_active: usize = count_batches.iter().map(Batch::num_rows).sum();
        assert_eq!(total_active, 50);
        assert!(
            s3.counters().bytes_requested <= u64::from(rg0.core_len) + 128,
            "projection=Some(vec![]) must only read Core filter blocks, requested {} bytes",
            s3.counters().bytes_requested
        );
    }

    #[test]
    fn shredded_map_key_query_reads_only_subcolumn_and_skips_residual() {
        let schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("attributes", DataType::MapUtf8, true),
        ])
        .unwrap();

        let n = 128usize;
        let ids: Vec<i64> = (0..n as i64).collect();
        let mut map_offsets = vec![0u32];
        let mut map_keys = Vec::new();
        let mut map_vals = Vec::new();
        let big_rare = "R".repeat(4096);
        for i in 0..n {
            map_keys.push("customer_id".to_owned());
            map_vals.push(format!("cust_{}", i % 8));
            map_keys.push(format!("rare_key_{i}"));
            map_vals.push(big_rare.clone());
            map_offsets.push(map_keys.len() as u32);
        }
        let mk_refs: Vec<&str> = map_keys.iter().map(String::as_str).collect();
        let mv_refs: Vec<&str> = map_vals.iter().map(String::as_str).collect();

        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::MapUtf8(Box::new(
                            MapData::try_new(
                                map_offsets,
                                Utf8Data::try_from_slices(mk_refs).unwrap(),
                                Utf8Data::try_from_slices(mv_refs).unwrap(),
                                None,
                            )
                            .unwrap(),
                        )),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            n,
        )
        .unwrap();

        let opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        let mut file_bytes = Vec::new();
        let mut w = StatamWriter::new(Arc::clone(&schema), opts, None).unwrap();
        w.write_batch(&batch).unwrap();
        w.finish(&mut file_bytes).unwrap();

        let s3 = Arc::new(SimulatedS3Reader::from_bytes(
            file_bytes,
            SimulatedS3Config::default(),
        ));
        let reader = StatamReader::open(s3.clone(), None).unwrap();
        let sm = &reader.meta().schema_meta.shredded_maps[0];
        let cust_entry = sm.keys.iter().find(|k| k.key == "customer_id").unwrap();
        assert_eq!(cust_entry.residual_count, 0);

        let residual_blk = reader.meta().rg_blocks(1, 0)[0];
        let sub_blk = reader.meta().rg_blocks(cust_entry.sub_col_idx as usize, 0)[0];
        assert!(residual_blk.stored_len > 0);
        assert!(sub_blk.offset < reader.meta().rg_dir[0].wide_offset);

        s3.reset_counters();
        let res = reader
            .scan(&ScanRequest {
                projection: Some(vec![0, 1]),
                map_keys: vec![("attributes".to_owned(), "customer_id".to_owned())],
                predicates: vec![Predicate::map_key(
                    "attributes",
                    "customer_id",
                    ZonePred::Eq(ZoneValue::Bytes(b"cust_3".to_vec())),
                )],
                coalesce_mode: CoalesceMode::Scan,
                ..ScanRequest::default()
            })
            .unwrap();

        let expected_max_bytes = u64::from(reader.meta().rg_dir[0].core_len) + 64;
        assert!(
            s3.counters().bytes_requested <= expected_max_bytes,
            "expected <= {expected_max_bytes} requested (skipping residual {} B), got {}",
            residual_blk.stored_len,
            s3.counters().bytes_requested
        );

        let out = concat_scanned_batches(&schema, &res);
        assert_eq!(out.num_rows(), n / 8);
        let FlatData::MapUtf8(got_map) = out
            .column(1)
            .unwrap()
            .materialize(None)
            .unwrap()
            .data()
            .clone()
        else {
            panic!("expected MapUtf8");
        };
        for r in 0..out.num_rows() {
            let entries = got_map.get(r).unwrap();
            assert_eq!(
                entries,
                vec![("customer_id".to_owned(), Some("cust_3".to_owned()))]
            );
        }
    }

    #[test]
    fn substring_search_e2e_inline_pack_boundary_and_spans_name() {
        let schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("name", DataType::Utf8, false),
            Field::new("input", DataType::Utf8, true),
        ])
        .unwrap();

        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        opts.narrow_trigram_cols = vec!["name".to_owned()];
        let pack_id = [0x33u8; 16];
        opts.pack_id = Some(pack_id);

        let n = 2048usize;
        let ids: Vec<i64> = (0..n as i64).collect();
        let mut names = Vec::with_capacity(n);
        let mut inputs = Vec::with_capacity(n);

        // Encode each row's unique token in a base-4 alphabet (`0..3`) so the
        // column stays high-NDV (`>50%` distinct, avoiding Dict) and exceeds the
        // 2560 B compressed size floor for NarrowTrigram, while keeping total
        // unique trigrams small enough that trigram bloom density stays <= 0.50.
        let base4 = |mut v: usize| -> String {
            let mut s = String::with_capacity(8);
            for _ in 0..8 {
                s.push((b'0' + (v & 3) as u8) as char);
                v >>= 2;
            }
            s
        };

        for i in 0..n {
            names.push(if i == 7 {
                format!("agent.SPECIAL_SPAN_TOKEN_{i}_padding_abcdef1234567890")
            } else {
                let a = base4(i.wrapping_mul(2654435761) >> 16);
                let b = base4(i.wrapping_mul(1597334677) >> 16);
                let c = base4(i);
                format!("worker.op_{c}_svc_{a}_reg_{b}_xyz")
            });

            if i == 2 {
                inputs.push("inline TINY_NEEDLE_MATCH ok".to_owned());
            } else if i == 11 {
                let left = format!("\"{}BOUNDARY_\"", "a".repeat(270));
                let right = format!("\"SPLIT_TOKEN{}\"", "b".repeat(270));
                inputs.push(format!("[{left},{right}]"));
            } else {
                inputs.push(format!("[{{\"m\":\"{}noise_{i}\"}}]", "n".repeat(300)));
            }
        }

        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let in_refs: Vec<&str> = inputs.iter().map(String::as_str).collect();
        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(name_refs).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(in_refs).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            n,
        )
        .unwrap();

        let mut pb = ChunkPackBuilder::new(pack_id, test_pack_opts());
        let mut writer = StatamWriter::new(Arc::clone(&schema), opts, Some(&mut pb)).unwrap();
        writer.write_batch(&batch).unwrap();
        let mut file_bytes = Vec::new();
        writer.finish(&mut file_bytes).unwrap();
        let mut pack_bytes = Vec::new();
        pb.finish(&mut pack_bytes).unwrap();

        let mut resolver = InMemoryPackResolver::new();
        resolver.insert_bytes(pack_id, pack_bytes).unwrap();
        let reader = StatamReader::open(
            Arc::new(SliceReader::new(file_bytes)),
            Some(Arc::new(resolver)),
        )
        .unwrap();
        assert!(
            (reader.meta().col_dir[1].col_flags & COL_FLAG_HAS_TRIGRAM_INDEX) != 0,
            "high-NDV spans.name must receive NarrowTrigram index"
        );

        // 1. Inline match
        let res_inline = reader
            .scan(&ScanRequest {
                substrings: vec![SubstringQuery::like("input", "TINY_NEEDLE_MATCH")],
                ..ScanRequest::default()
            })
            .unwrap();
        let out_inline = concat_scanned_batches(&schema, &res_inline);
        assert_eq!(out_inline.num_rows(), 1);
        assert_eq!(
            out_inline.column(0).unwrap().get_scalar(0).unwrap(),
            ScalarValue::I64(2)
        );

        // 2. Cross-chunk boundary match straddling structural JSON comma boundary
        let res_boundary = reader
            .scan(&ScanRequest {
                substrings: vec![SubstringQuery::ilike("input", "boundary_\",\"split_token")],
                ..ScanRequest::default()
            })
            .unwrap();
        let out_boundary = concat_scanned_batches(&schema, &res_boundary);
        assert_eq!(out_boundary.num_rows(), 1);
        assert_eq!(
            out_boundary.column(0).unwrap().get_scalar(0).unwrap(),
            ScalarValue::I64(11)
        );

        // 3. Narrow `spans.name` trigram-indexed substring search
        let res_name = reader
            .scan(&ScanRequest {
                substrings: vec![SubstringQuery::ilike("name", "special_span_token_7")],
                ..ScanRequest::default()
            })
            .unwrap();
        let out_name = concat_scanned_batches(&schema, &res_name);
        assert_eq!(out_name.num_rows(), 1);
        assert_eq!(
            out_name.column(0).unwrap().get_scalar(0).unwrap(),
            ScalarValue::I64(7)
        );
    }

    #[test]
    fn empty_file_desc_limit_and_column_encoding_passthrough() {
        let schema = Schema::try_new_arc(vec![
            Field::new("ts", DataType::TimestampNs { utc: true }, false),
            Field::new("const_col", DataType::Utf8, false),
            Field::new("rle_col", DataType::I64, false),
            Field::new("dict_col", DataType::Utf8, false),
        ])
        .unwrap();

        // 1. Empty 0-row file roundtrip.
        let mut empty_bytes = Vec::new();
        let w_empty =
            StatamWriter::new(Arc::clone(&schema), WriteOptions::default(), None).unwrap();
        let stats_empty = w_empty.finish(&mut empty_bytes).unwrap();
        assert_eq!(stats_empty.row_count, 0);
        let r_empty = StatamReader::open(Arc::new(SliceReader::new(empty_bytes)), None).unwrap();
        r_empty.verify_full().unwrap();
        assert!(r_empty.scan(&ScanRequest::default()).unwrap().is_empty());

        // 2. Multi-RG DESC + LIMIT + Const/Rle/Dict pass-through.
        let mut opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L1Compact);
        opts.sort_cols = vec![0];
        opts.row_group_size = 32;
        opts.mini_block_size = 16;

        let n = 64usize;
        let ts: Vec<i64> = (0..n as i64).map(|i| 10_000 + i).collect();
        let rle = RleColumn::try_new(
            FlatColumn::try_new(FlatData::I64(vec![100, 200]), None).unwrap(),
            vec![32, 64],
        )
        .unwrap();
        let dict_vals: Vec<&str> = (0..n)
            .map(|i| match i % 3 {
                0 => "alpha",
                1 => "beta",
                _ => "gamma",
            })
            .collect();

        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: ts,
                            utc: true,
                        },
                        None,
                    )
                    .unwrap(),
                ),
                Column::try_new_const(ScalarValue::Utf8("constant_tag".to_owned()), n).unwrap(),
                Column::Rle(rle),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(dict_vals).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            n,
        )
        .unwrap();

        let mut file_bytes = Vec::new();
        let mut w = StatamWriter::new(Arc::clone(&schema), opts, None).unwrap();
        w.write_batch(&batch).unwrap();
        w.finish(&mut file_bytes).unwrap();

        let reader = StatamReader::open(Arc::new(SliceReader::new(file_bytes)), None).unwrap();
        let batches = reader
            .scan(&ScanRequest {
                sort_desc_hint: true,
                limit: Some(5),
                ..ScanRequest::default()
            })
            .unwrap();
        let total: usize = batches.iter().map(Batch::num_rows).sum();
        assert_eq!(total, 5);
        assert!(matches!(
            batches[0].column(1).unwrap(),
            Column::Const { .. }
        ));
        assert!(matches!(
            batches[0].column(2).unwrap(),
            Column::Const { .. } | Column::Rle(_)
        ));
        assert!(matches!(batches[0].column(3).unwrap(), Column::Dict(_)));
        let got_desc = concat_scanned_batches(&schema, &batches);
        let got_ts: Vec<i64> = (0..5)
            .map(
                |r| match got_desc.column(0).unwrap().get_scalar(r).unwrap() {
                    ScalarValue::TimestampNs { value, .. } => value,
                    other => panic!("unexpected ts {other:?}"),
                },
            )
            .collect();
        assert_eq!(got_ts, vec![10_059, 10_060, 10_061, 10_062, 10_063]);
    }

    #[test]
    fn desc_limit_returns_exact_newest_timestamps_on_20k_sorted_file() {
        let schema = Schema::try_new_arc(vec![
            Field::new("ts", DataType::TimestampNs { utc: true }, false),
            Field::new("flag", DataType::I64, false),
        ])
        .unwrap();

        let mut opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L1Compact);
        opts.sort_cols = vec![0];
        // Default 16,384 RG size -> 20,000 rows produces 2 RGs (16,384 + 3,616) and multiple MBs.

        let n = 20_000usize;
        let ts: Vec<i64> = (0..n as i64).map(|i| 1_000_000 + i).collect();
        // Sparse flag = 1 at 19_999, 19_990, 16_383, 16_380, 15_000, 100 (spans multiple MBs & RGs)
        let flags: Vec<i64> = (0..n)
            .map(|i| {
                if matches!(i, 100 | 15_000 | 16_380 | 16_383 | 19_990 | 19_999) {
                    1
                } else {
                    0
                }
            })
            .collect();
        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: ts,
                            utc: true,
                        },
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(FlatColumn::try_new(FlatData::I64(flags), None).unwrap()),
            ],
            n,
        )
        .unwrap();

        let mut file_bytes = Vec::new();
        let mut w = StatamWriter::new(Arc::clone(&schema), opts, None).unwrap();
        w.write_batch(&batch).unwrap();
        w.finish(&mut file_bytes).unwrap();

        let s3 = Arc::new(SimulatedS3Reader::from_bytes(
            file_bytes,
            SimulatedS3Config::default(),
        ));
        let reader = StatamReader::open(s3, None).unwrap();
        assert_eq!(reader.meta().num_row_groups(), 2);

        // 1. Unfiltered TOP 5 DESC
        let res = reader
            .scan(&ScanRequest {
                sort_desc_hint: true,
                limit: Some(5),
                ..ScanRequest::default()
            })
            .unwrap();
        let out = concat_scanned_batches(&schema, &res);
        assert_eq!(out.num_rows(), 5);
        let mut got_ts: Vec<i64> = (0..5)
            .map(|r| match out.column(0).unwrap().get_scalar(r).unwrap() {
                ScalarValue::TimestampNs { value, .. } => value,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        got_ts.sort_unstable();
        assert_eq!(
            got_ts,
            vec![1_019_995, 1_019_996, 1_019_997, 1_019_998, 1_019_999]
        );

        // 2. Filtered TOP 5 DESC spanning multiple mini-blocks & both RGs
        let res_filtered = reader
            .scan(&ScanRequest {
                predicates: vec![Predicate::new("flag", ZonePred::Eq(ZoneValue::I64(1)))],
                sort_desc_hint: true,
                limit: Some(5),
                ..ScanRequest::default()
            })
            .unwrap();
        let out_f = concat_scanned_batches(&schema, &res_filtered);
        assert_eq!(out_f.num_rows(), 5);
        let mut got_f: Vec<i64> = (0..5)
            .map(|r| match out_f.column(0).unwrap().get_scalar(r).unwrap() {
                ScalarValue::TimestampNs { value, .. } => value,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        got_f.sort_unstable();
        assert_eq!(
            got_f,
            vec![1_015_000, 1_016_380, 1_016_383, 1_019_990, 1_019_999]
        );
    }

    #[test]
    fn shredded_map_filter_with_full_map_projection_preserves_rare_residual_keys() {
        let schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("attributes", DataType::MapUtf8, true),
        ])
        .unwrap();

        let n = 128usize;
        let ids: Vec<i64> = (0..n as i64).collect();
        let mut map_offsets = vec![0u32];
        let mut map_keys = Vec::new();
        let mut map_vals = Vec::new();
        for i in 0..n {
            map_keys.push("env");
            map_vals.push(if i % 2 == 0 { "prod" } else { "staging" });
            if i == 10 {
                // Rare residual key (< 5% presence -> stays in residual!).
                map_keys.push("rare_tag");
                map_vals.push("vip_payload");
            }
            map_offsets.push(map_keys.len() as u32);
        }

        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::MapUtf8(Box::new(
                            MapData::try_new(
                                map_offsets,
                                Utf8Data::try_from_slices(map_keys).unwrap(),
                                Utf8Data::try_from_slices(map_vals).unwrap(),
                                None,
                            )
                            .unwrap(),
                        )),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            n,
        )
        .unwrap();

        let opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        let mut file_bytes = Vec::new();
        let mut w = StatamWriter::new(Arc::clone(&schema), opts, None).unwrap();
        w.write_batch(&batch).unwrap();
        w.finish(&mut file_bytes).unwrap();

        let reader = StatamReader::open(Arc::new(SliceReader::new(file_bytes)), None).unwrap();
        let sm = &reader.meta().schema_meta.shredded_maps[0];
        let env_entry = sm.keys.iter().find(|k| k.key == "env").unwrap();
        assert_eq!(env_entry.residual_count, 0);

        // Filter on shredded key `"env" == "prod"` (residual_count == 0) WHILE projecting
        // the FULL `"attributes"` map (`map_keys: vec![]`) -> `"rare_tag"` MUST be preserved!
        let res = reader
            .scan(&ScanRequest {
                projection: Some(vec![0, 1]),
                map_keys: Vec::new(),
                predicates: vec![Predicate::map_key(
                    "attributes",
                    "env",
                    ZonePred::Eq(ZoneValue::Bytes(b"prod".to_vec())),
                )],
                coalesce_mode: CoalesceMode::Scan,
                ..ScanRequest::default()
            })
            .unwrap();
        let out = concat_scanned_batches(&schema, &res);
        assert_eq!(out.num_rows(), n / 2);

        let FlatData::MapUtf8(got_map) = out
            .column(1)
            .unwrap()
            .materialize(None)
            .unwrap()
            .data()
            .clone()
        else {
            panic!("expected MapUtf8");
        };
        // Row with id=10 is at output row 5 (even rows 0, 2, 4, 6, 8, 10).
        let row10_entries = got_map.get(5).unwrap();
        assert!(
            row10_entries.contains(&("env".to_owned(), Some("prod".to_owned()))),
            "missing shredded key env"
        );
        assert!(
            row10_entries.contains(&("rare_tag".to_owned(), Some("vip_payload".to_owned()))),
            "rare residual key rare_tag was dropped during full map projection!"
        );
    }

    #[test]
    fn f64_total_order_nan_semantics_on_flat_and_const_columns() {
        let schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("flat_f", DataType::F64, true),
            Field::new("const_nan", DataType::F64, false),
            Field::new("const_neg_zero", DataType::F64, false),
        ])
        .unwrap();

        // Row 0: -0.0, Row 1: 0.5, Row 2: 2.0, Row 3: NaN, Row 4: +inf, Row 5: NULL
        let n = 6usize;
        let ids: Vec<i64> = (0..n as i64).collect();
        let flat_vals = vec![-0.0, 0.5, 2.0, f64::NAN, f64::INFINITY, 0.0];
        let flat_nulls = Bitmap::from_bools(&[true, true, true, true, true, false]);
        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(FlatData::F64(flat_vals), Some(flat_nulls)).unwrap(),
                ),
                Column::try_new_const(ScalarValue::F64(f64::NAN), n).unwrap(),
                Column::try_new_const(ScalarValue::F64(-0.0), n).unwrap(),
            ],
            n,
        )
        .unwrap();

        for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
            let opts = WriteOptions::for_table(TableKind::Generic, profile);
            let mut file_bytes = Vec::new();
            let mut w = StatamWriter::new(Arc::clone(&schema), opts, None).unwrap();
            w.write_batch(&batch).unwrap();
            w.finish(&mut file_bytes).unwrap();
            let reader = StatamReader::open(Arc::new(SliceReader::new(file_bytes)), None).unwrap();

            let proj_schema = Arc::new(schema.project(&[0]).unwrap());
            let scan_ids = |preds: Vec<Predicate>| -> Vec<i64> {
                let res = reader
                    .scan(&ScanRequest {
                        projection: Some(vec![0]),
                        predicates: preds,
                        ..ScanRequest::default()
                    })
                    .unwrap();
                let out = concat_scanned_batches(&proj_schema, &res);
                (0..out.num_rows())
                    .map(|r| match out.column(0).unwrap().get_scalar(r).unwrap() {
                        ScalarValue::I64(v) => v,
                        other => panic!("unexpected {other:?}"),
                    })
                    .collect()
            };

            // Flat F64 total-order checks (NULL row 5 never matches value predicates)
            assert_eq!(
                scan_ids(vec![Predicate::new(
                    "flat_f",
                    ZonePred::Eq(ZoneValue::F64(f64::NAN))
                )]),
                vec![3]
            );
            assert_eq!(
                scan_ids(vec![Predicate::new(
                    "flat_f",
                    ZonePred::Ne(ZoneValue::F64(f64::NAN))
                )]),
                vec![0, 1, 2, 4]
            );
            assert_eq!(
                scan_ids(vec![Predicate::new(
                    "flat_f",
                    ZonePred::Gt(ZoneValue::F64(1.0))
                )]),
                vec![2, 3, 4]
            );
            assert_eq!(
                scan_ids(vec![Predicate::new(
                    "flat_f",
                    ZonePred::Between(ZoneValue::F64(0.0), ZoneValue::F64(f64::NAN))
                )]),
                vec![0, 1, 2, 3, 4]
            );
            assert_eq!(
                scan_ids(vec![Predicate::new(
                    "flat_f",
                    ZonePred::In(vec![ZoneValue::F64(f64::NAN)])
                )]),
                vec![3]
            );
            assert_eq!(
                scan_ids(vec![Predicate::new(
                    "flat_f",
                    ZonePred::Eq(ZoneValue::F64(0.0))
                )]),
                vec![0]
            );

            // Const F64(NaN) total-order checks
            assert_eq!(
                scan_ids(vec![Predicate::new(
                    "const_nan",
                    ZonePred::Eq(ZoneValue::F64(f64::NAN))
                )]),
                vec![0, 1, 2, 3, 4, 5]
            );
            assert!(
                scan_ids(vec![Predicate::new(
                    "const_nan",
                    ZonePred::Ne(ZoneValue::F64(f64::NAN))
                )])
                .is_empty()
            );
            assert_eq!(
                scan_ids(vec![Predicate::new(
                    "const_nan",
                    ZonePred::Gt(ZoneValue::F64(1.0))
                )]),
                vec![0, 1, 2, 3, 4, 5]
            );
            assert_eq!(
                scan_ids(vec![Predicate::new(
                    "const_nan",
                    ZonePred::Between(ZoneValue::F64(0.0), ZoneValue::F64(f64::NAN))
                )]),
                vec![0, 1, 2, 3, 4, 5]
            );
            assert_eq!(
                scan_ids(vec![Predicate::new(
                    "const_nan",
                    ZonePred::In(vec![ZoneValue::F64(f64::NAN)])
                )]),
                vec![0, 1, 2, 3, 4, 5]
            );
            assert_eq!(
                scan_ids(vec![Predicate::new(
                    "const_neg_zero",
                    ZonePred::Eq(ZoneValue::F64(0.0))
                )]),
                vec![0, 1, 2, 3, 4, 5]
            );
        }
    }

    #[test]
    fn read_from_rejects_structural_corruption_and_short_reads() {
        let schema = Schema::try_new_arc(vec![
            Field::new("a", DataType::I64, false),
            Field::new("b", DataType::I64, false),
        ])
        .unwrap();
        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1, 2, 3, 4]), None).unwrap()),
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![5, 6, 7, 8]), None).unwrap()),
            ],
            4,
        )
        .unwrap();
        let mut opts = WriteOptions::for_table(TableKind::Generic, WriteProfile::L0Fast);
        opts.compress_hot_tail_lz4 = false;
        let mut valid_bytes = Vec::new();
        let mut w = StatamWriter::new(Arc::clone(&schema), opts, None).unwrap();
        w.write_batch(&batch).unwrap();
        w.finish(&mut valid_bytes).unwrap();

        let rebuild_uncompressed_tail = |mut raw_tail: Vec<u8>, mut ps: Postscript| -> Vec<u8> {
            let mut out = valid_bytes[..valid_bytes.len() - ps.hot_tail_len as usize].to_vec();
            ps.flags &= !FLAG_HOT_TAIL_LZ4;
            ps.hot_tail_raw_len = raw_tail.len() as u32;
            ps.hot_tail_len = (raw_tail.len() + POSTSCRIPT_LEN) as u32;
            ps.hot_tail_crc32c = crc32c::crc32c(&raw_tail);
            out.append(&mut raw_tail);
            out.extend_from_slice(&ps.encode());
            out
        };

        let meta = StatamFileMeta::read_from(&SliceReader::new(valid_bytes.clone()), None).unwrap();
        let ps = meta.postscript;
        let raw_body_start = valid_bytes.len() - ps.hot_tail_len as usize;
        let raw_body = valid_bytes[raw_body_start..valid_bytes.len() - POSTSCRIPT_LEN].to_vec();

        // 1. Corrupt col_dir[0].logical_col_id != 0
        {
            let mut body = raw_body.clone();
            let off = ps.col_dir_off as usize;
            let mut cd = ColumnDirEntry::decode(&body[off..off + COLUMN_DIR_ENTRY_LEN]).unwrap();
            cd.logical_col_id = 1;
            body[off..off + COLUMN_DIR_ENTRY_LEN].copy_from_slice(&cd.encode());
            let mutated = rebuild_uncompressed_tail(body, ps);
            assert!(StatamFileMeta::read_from(&SliceReader::new(mutated), None).is_err());
        }

        // 2. Corrupt rg.wide_offset != rg.core_offset + rg.core_len
        {
            let mut body = raw_body.clone();
            let off = ps.rg_dir_off as usize;
            let mut rg = RowGroupEntry::decode(&body[off..off + ROW_GROUP_ENTRY_LEN]).unwrap();
            rg.wide_offset += 1;
            rg.wide_len = rg.wide_len.saturating_sub(1);
            body[off..off + ROW_GROUP_ENTRY_LEN].copy_from_slice(&rg.encode());
            let mutated = rebuild_uncompressed_tail(body, ps);
            assert!(StatamFileMeta::read_from(&SliceReader::new(mutated), None).is_err());
        }

        // 3. Corrupt block layout mismatch across physical columns in RG 0
        {
            let mut body = raw_body;
            let b1_off = ps.block_dir_off as usize + BLOCK_ENTRY_LEN;
            let mut blk1 = BlockEntry::decode(&body[b1_off..b1_off + BLOCK_ENTRY_LEN]).unwrap();
            blk1.row_count = 3;
            body[b1_off..b1_off + BLOCK_ENTRY_LEN].copy_from_slice(&blk1.encode());
            let mutated = rebuild_uncompressed_tail(body, ps);
            assert!(StatamFileMeta::read_from(&SliceReader::new(mutated), None).is_err());
        }

        // 4. Short-reading RangeReader must fail cleanly
        struct ShortReader(Vec<u8>);
        impl RangeReader for ShortReader {
            fn file_len(&self) -> u64 {
                self.0.len() as u64
            }
            fn read_range(&self, range: Range<u64>) -> Result<Bytes, StatamError> {
                let s = range.start as usize;
                let e = (range.end as usize).saturating_sub(1).max(s);
                Ok(Bytes::copy_from_slice(&self.0[s..e]))
            }
        }
        assert!(StatamFileMeta::read_from(&ShortReader(valid_bytes), None).is_err());
    }

    #[test]
    fn masked_payload_substring_and_coalesced_stpk_read_chunks() {
        let schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("tag", DataType::Utf8, false),
            Field::new("input", DataType::Utf8, true),
            Field::new("output", DataType::Utf8, true),
        ])
        .unwrap();

        let pack_id = [0x77u8; 16];
        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        opts.payload_cols = vec!["input".to_owned(), "output".to_owned()];
        opts.pack_id = Some(pack_id);
        opts.mini_block_size = 4;

        let n = 8usize;
        let ids: Vec<i64> = (0..n as i64).collect();
        let tags: Vec<&str> = (0..n)
            .map(|i| if i == 5 { "keep" } else { "skip" })
            .collect();
        let inputs: Vec<String> = (0..n)
            .map(|i| {
                format!(
                    "[{{\"role\":\"user\",\"content\":\"IN_{i}_{}\"}}]",
                    "x".repeat(320)
                )
            })
            .collect();
        let outputs: Vec<String> = (0..n)
            .map(|i| {
                format!(
                    "[{{\"role\":\"assistant\",\"content\":\"OUT_{i}_{}\"}}]",
                    "y".repeat(320)
                )
            })
            .collect();
        let in_refs: Vec<&str> = inputs.iter().map(String::as_str).collect();
        let out_refs: Vec<&str> = outputs.iter().map(String::as_str).collect();

        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(tags).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(in_refs).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(out_refs).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            n,
        )
        .unwrap();

        let mut pb = ChunkPackBuilder::new(pack_id, test_pack_opts());
        let mut w = StatamWriter::new(Arc::clone(&schema), opts, Some(&mut pb)).unwrap();
        w.write_batch(&batch).unwrap();
        let mut file_bytes = Vec::new();
        w.finish(&mut file_bytes).unwrap();
        let mut pack_bytes = Vec::new();
        pb.finish(&mut pack_bytes).unwrap();

        let pack_s3 = Arc::new(SimulatedS3Reader::from_bytes(
            pack_bytes,
            SimulatedS3Config::default(),
        ));
        let pack_reader = Arc::new(ChunkPackReader::open(pack_s3.clone()).unwrap());
        let mut resolver = InMemoryPackResolver::new();
        resolver.insert(pack_id, pack_reader);

        let reader = StatamReader::open(
            Arc::new(SliceReader::new(file_bytes)),
            Some(Arc::new(resolver)),
        )
        .unwrap();

        // 1. When Phase 1 narrow predicate eliminates all rows (`tag == "missing"`),
        // payload substring search MUST NOT read the `.stpk` pack!
        pack_s3.reset_counters();
        let empty = reader
            .scan(&ScanRequest {
                predicates: vec![Predicate::new(
                    "tag",
                    ZonePred::Eq(ZoneValue::Bytes(b"missing".to_vec())),
                )],
                substrings: vec![SubstringQuery::like("input", "IN_5")],
                ..ScanRequest::default()
            })
            .unwrap();
        assert!(empty.is_empty());
        assert_eq!(pack_s3.counters().gets, 0);

        // 2. Projecting BOTH `input` and `output` across both mini-blocks reads `.stpk`
        // frames in a single coalesced wave (`gets == 1` when frames fit in 1 MiB).
        pack_s3.reset_counters();
        let res = reader
            .scan(&ScanRequest {
                projection: Some(vec![0, 2, 3]),
                ..ScanRequest::default()
            })
            .unwrap();
        let proj_schema = Arc::new(schema.project(&[0, 2, 3]).unwrap());
        let out = concat_scanned_batches(&proj_schema, &res);
        assert_eq!(out.num_rows(), n);
        assert!(pack_s3.counters().gets <= 1);
    }
}
