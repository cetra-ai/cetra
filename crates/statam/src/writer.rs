//! Streaming PAX Row-Group writer for `.statam` files (`L0Fast` & `L1Compact`).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::Write;
use std::sync::Arc;

use cetra_column::{
    Batch, BinaryData, Column, DataType, FlatColumn, FlatData, ScalarValue, Schema, Utf8Data,
};

use crate::StatamError;
use crate::codec::dict::{DictBuilder, DictValueKind, FileDict};
use crate::codec::nested::{encode_shredded_subcol, select_shredded_keys, shred_map};
use crate::codec::{EncodedMiniBlock, WriteProfile, encode_mini_block};
use crate::format::{
    BLOCK_ENTRY_LEN, BLOCK_FLAG_ALL_NULL, BLOCK_FLAG_HAS_NULLS, BlockEntry, CHECKSUM_ALG_CRC32C,
    COL_FLAG_BLOOM_IN_HOT_TAIL, COL_FLAG_DICT_IN_HOT_TAIL, COL_FLAG_DICT_SORTED,
    COL_FLAG_HAS_FILE_DICT, COL_FLAG_HAS_TRIGRAM_INDEX, COL_FLAG_IS_PAYLOAD_REFS,
    COL_FLAG_IS_SHREDDED_SUBCOL, COLUMN_DIR_ENTRY_LEN, ChunkLoc, Codec, ColumnDirEntry, Encoding,
    FILE_HEADER_LEN, FLAG_HAS_PACK_REFS, FLAG_HAS_SHREDDED_MAPS, FLAG_HAS_TRIGRAM_INDEX,
    FLAG_HOT_TAIL_LZ4, FLAG_L1_COMPACT, FORMAT_VERSION, FileHeader, FileSchemaMeta,
    LAZY_INDEX_ENTRY_LEN, LazyIndexEntry, LazyIndexKind, LocalityGroup, MAX_BLOCK_RAW_BYTES,
    MAX_COLUMNS, MAX_FRAME_RAW_BYTES, MAX_HOT_TAIL_BYTES, MAX_PACK_REFS, MAX_ROW_GROUPS,
    MAX_ROWS_PER_FILE, MAX_TOTAL_BLOCKS, MINI_BLOCK_SIZE, POSTSCRIPT_LEN, Postscript,
    ROW_GROUP_ENTRY_LEN, ROW_GROUP_SIZE, RowGroupEntry, ShreddedMapDir, ShreddedMapKeyEntry,
    TableKind, encode_schema, write_framed_block, write_framed_pack_frame,
};
use crate::payload::chunker::{DedupBypassDetector, INLINE_PAYLOAD_THRESHOLD, chunk_payload};
use crate::payload::pack::ChunkPackBuilder;
use crate::payload::{PayloadRefRow, encode_payload_refs};
use crate::simd::bloom::{Sbbf, statam_bloom_hash};
use crate::simd::trigram::{
    MAX_TRIGRAM_BLOOM_BYTES, MIN_TRIGRAM_BLOOM_BYTES, TrigramBloom, extract_trigram_hashes,
};

/// Hot Tail target size cap (768 KiB) before spilling secondary blooms/dicts to Lazy Index.
const HOT_TAIL_BUDGET_BYTES: usize = 768 * 1024;
/// Maximum size of a single FileDict kept inside the Hot Tail (8 KiB).
const HOT_DICT_MAX_BYTES: usize = 8 * 1024;
/// Target SBBF false-positive probability.
const BLOOM_FPP: f64 = 0.01;
/// Target uncompressed bytes per L0 local payload `PF` frame (64 KiB).
const L0_PAYLOAD_FRAME_TARGET_BYTES: usize = 64 * 1024;
/// Magic bytes for the local L0 payload directory inside `l0_payload_offset`.
pub(crate) const L0_PAYLOAD_DIR_MAGIC: [u8; 4] = *b"L0P1";
/// Magic bytes for a per-block narrow-column trigram section.
pub(crate) const NARROW_TRIGRAM_MAGIC: [u8; 4] = *b"NT01";
/// Maximum bit density for narrow-column trigram blooms.
const MAX_NARROW_TRIGRAM_DENSITY: f64 = 0.50;

/// Pack index sentinel indicating local L0 payload frames inside the Row Group.
pub const L0_LOCAL_PACK_IDX: u16 = 0xFFFF;

type PrechunkedRefs<'a> = (
    &'a HashMap<usize, Vec<PayloadRefRow>>,
    &'a HashMap<(usize, u32), Vec<u8>>,
);

const WIDE_ROW_COL_NAMES: &[&str] = &[
    "input_preview",
    "output_preview",
    "status_message",
    "request_params",
    "attributes",
    "resource_attributes",
    "metadata",
    "events",
    "links",
    "tags",
    "media_refs",
    "truncated",
    "conventions",
    "reason",
];

const HIGH_CARDINALITY_ID_COLS: &[&str] = &["trace_id", "span_id", "parent_span_id", "score_id"];

const KNOWN_LOW_CARD_DICT_COLS: &[&str] = &[
    "name",
    "kind",
    "status_code",
    "model",
    "provider",
    "operation",
    "finish_reason",
    "service_name",
    "span_kind",
    "environment",
    "release",
    "session_id",
    "user_id",
    "prompt_name",
    "prompt_version",
    "deployment",
    "framework",
    "sdk_name",
    "data_type",
    "source",
];

/// Configuration options for [`StatamWriter`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOptions {
    /// Encoding and compression profile (`L0Fast` vs `L1Compact`).
    pub profile: WriteProfile,
    /// Table kind (`Generic`, `Spans`, `Traces`, `Scores`).
    pub table_kind: TableKind,
    /// Logical column indices forming the sort key.
    pub sort_cols: Vec<u16>,
    /// Column names that receive SBBF bloom filters.
    pub bloom_cols: Vec<String>,
    /// Primary bloom column placed directly inside the Hot Tail (default: `Some("trace_id")`).
    pub hot_bloom_col: Option<String>,
    /// Heavy `Utf8` column names stored as `ExternalPayload` (`PayloadRefs`).
    pub payload_cols: Vec<String>,
    /// Narrow `Utf8` column names receiving per-block `TrigramBloom` in `L1Compact` when not dict-encoded.
    pub narrow_trigram_cols: Vec<String>,
    /// Rows per PAX Row Group (default: `ROW_GROUP_SIZE = 16_384`).
    pub row_group_size: usize,
    /// Rows per Mini-Block (default: `MINI_BLOCK_SIZE = 4_096`).
    pub mini_block_size: usize,
    /// Zstd compression level when `profile == L1Compact` (default: `3`).
    pub zstd_level: i32,
    /// Whether to LZ4-compress the Hot Tail body when smaller (default: `true`).
    pub compress_hot_tail_lz4: bool,
    /// External `.stpk` pack ID when writing `L1Compact` with a [`ChunkPackBuilder`].
    pub pack_id: Option<[u8; 16]>,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self::for_table(TableKind::Generic, WriteProfile::L1Compact)
    }
}

impl WriteOptions {
    /// Builds [`WriteOptions`] populated with table-specific defaults per `spec-v0.md`.
    pub fn for_table(table_kind: TableKind, profile: WriteProfile) -> Self {
        let bloom_cols = match table_kind {
            TableKind::Spans => vec![
                "trace_id".to_owned(),
                "span_id".to_owned(),
                "session_id".to_owned(),
                "user_id".to_owned(),
            ],
            TableKind::Traces => vec![
                "trace_id".to_owned(),
                "session_id".to_owned(),
                "user_id".to_owned(),
            ],
            TableKind::Scores => vec!["trace_id".to_owned(), "score_id".to_owned()],
            TableKind::Generic => Vec::new(),
        };
        let payload_cols = match table_kind {
            TableKind::Spans => vec![
                "input".to_owned(),
                "output".to_owned(),
                "system_instructions".to_owned(),
                "tool_definitions".to_owned(),
            ],
            TableKind::Traces | TableKind::Scores | TableKind::Generic => Vec::new(),
        };
        let narrow_trigram_cols = match (table_kind, profile) {
            (TableKind::Spans, WriteProfile::L1Compact) => vec!["name".to_owned()],
            _ => Vec::new(),
        };

        Self {
            profile,
            table_kind,
            sort_cols: Vec::new(),
            bloom_cols,
            hot_bloom_col: Some("trace_id".to_owned()),
            payload_cols,
            narrow_trigram_cols,
            row_group_size: ROW_GROUP_SIZE,
            mini_block_size: MINI_BLOCK_SIZE,
            zstd_level: 3,
            compress_hot_tail_lz4: true,
            pack_id: None,
        }
    }
}

/// Summary statistics returned by [`StatamWriter::finish`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteStats {
    /// Total serialized `.statam` file length in bytes.
    pub total_bytes: u64,
    /// Total rows written.
    pub row_count: u64,
    /// Number of Row Groups written.
    pub num_row_groups: u32,
    /// Total Mini-Blocks written across all physical columns.
    pub total_blocks: u32,
    /// Stored byte length of the Hot Tail (including the 112 B Postscript).
    pub hot_tail_bytes: u32,
    /// Uncompressed byte length of the Hot Tail body.
    pub hot_tail_raw_bytes: u32,
    /// Total bytes written in `Core` locality group regions.
    pub core_bytes: u64,
    /// Total bytes written in `WideRow` locality group regions.
    pub wide_bytes: u64,
    /// Total bytes written in local `L0Payload` regions.
    pub l0_payload_bytes: u64,
    /// Total bytes written in Lazy Index sections.
    pub lazy_index_bytes: u64,
    /// Number of shredded map sub-columns promoted on RG0.
    pub shredded_subcols: u16,
    /// External `.stpk` pack IDs referenced by this file.
    pub pack_ids: Vec<[u8; 16]>,
}

/// Streaming bounded-memory PAX writer producing `.statam` files.
pub struct StatamWriter<'a> {
    schema: Arc<Schema>,
    options: WriteOptions,
    pack_builder: Option<&'a mut ChunkPackBuilder>,
    logical_locality: Vec<LocalityGroup>,
    bloom_col_set: HashSet<usize>,
    hot_bloom_col_idx: Option<usize>,
    narrow_trigram_col_set: HashSet<usize>,
    pack_ids: Vec<[u8; 16]>,
    uses_external_pack: bool,

    // Bounded 1-RG input buffer (`<= row_group_size` rows).
    rg_buf_cols: Vec<Vec<Column>>,
    rg_buf_rows: usize,

    // RG0-initialized file-level schema/dict state.
    rg0_initialized: bool,
    shredded_maps: Vec<ShreddedMapDir>,
    logical_to_shredded: HashMap<usize, usize>,
    phys_col_meta: Vec<PhysColWriteState>,
    phys_blocks: Vec<Vec<BlockEntry>>,

    // Accumulated encoded RG payloads (written in order on finish).
    rg_entries: Vec<RowGroupEntry>,
    rg_data_blobs: Vec<Vec<u8>>,
    cur_data_offset: u64,
    total_rows: u64,
    core_bytes: u64,
    wide_bytes: u64,
    l0_payload_bytes: u64,

    // Per-column bloom hash accumulators (keyed by logical column index).
    bloom_hashes: HashMap<usize, Vec<u64>>,
    // Per-column W4 bypass detectors (keyed by logical column index).
    bypass_detectors: HashMap<usize, DedupBypassDetector>,
    // Per-column trigram blooms for narrow_trigram_cols (keyed by logical column index).
    narrow_trigrams: HashMap<usize, Vec<Option<Vec<u8>>>>,
    // Highest chunk_id observed per logical payload col for dedup hit tracking.
    max_chunk_id_seen: HashMap<usize, u32>,
}

struct PhysColWriteState {
    phys_col_id: u16,
    logical_col_id: u16,
    locality_group: LocalityGroup,
    is_shredded_subcol: bool,
    is_payload_refs: bool,
    dict: Option<FileDict>,
    used_dict_encoding: bool,
    has_non_dict_data_block: bool,
    null_count_total: u32,
}

impl fmt::Debug for StatamWriter<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatamWriter")
            .field("profile", &self.options.profile)
            .field("table_kind", &self.options.table_kind)
            .field("logical_cols", &self.schema.len())
            .field("phys_cols", &self.phys_col_meta.len())
            .field("total_rows", &self.total_rows)
            .field("rg_buf_rows", &self.rg_buf_rows)
            .field("num_row_groups", &self.rg_entries.len())
            .finish()
    }
}

impl<'a> StatamWriter<'a> {
    /// Creates a new [`StatamWriter`] for `schema` and `options`.
    pub fn new(
        schema: Arc<Schema>,
        options: WriteOptions,
        pack_builder: Option<&'a mut ChunkPackBuilder>,
    ) -> Result<Self, StatamError> {
        let fields = schema.fields();
        if fields.len() > MAX_COLUMNS {
            return Err(StatamError::BoundsExceeded {
                reason: "schema column count exceeds MAX_COLUMNS",
            });
        }
        if options.row_group_size == 0 || options.row_group_size > ROW_GROUP_SIZE {
            return Err(StatamError::BoundsExceeded {
                reason: "row_group_size must be in 1..=ROW_GROUP_SIZE",
            });
        }
        if options.mini_block_size == 0 || options.mini_block_size > MINI_BLOCK_SIZE {
            return Err(StatamError::BoundsExceeded {
                reason: "mini_block_size must be in 1..=MINI_BLOCK_SIZE",
            });
        }
        if options.sort_cols.len() > u8::MAX as usize || options.sort_cols.len() > fields.len() {
            return Err(StatamError::SchemaMismatch {
                reason: "invalid sort_cols count",
            });
        }
        for &sc in &options.sort_cols {
            if (sc as usize) >= fields.len() {
                return Err(StatamError::SchemaMismatch {
                    reason: "sort column index out of bounds",
                });
            }
        }

        let payload_names: HashSet<&str> =
            options.payload_cols.iter().map(String::as_str).collect();
        let mut logical_locality = Vec::with_capacity(fields.len());
        let mut phys_col_meta = Vec::with_capacity(fields.len());

        for (idx, field) in fields.iter().enumerate() {
            field.data_type.validate(crate::MAX_NESTING_DEPTH)?;
            let lg = classify_locality_group(field, &payload_names);
            logical_locality.push(lg);
            phys_col_meta.push(PhysColWriteState {
                phys_col_id: idx as u16,
                logical_col_id: idx as u16,
                locality_group: lg,
                is_shredded_subcol: false,
                is_payload_refs: lg == LocalityGroup::ExternalPayload,
                dict: None,
                used_dict_encoding: false,
                has_non_dict_data_block: false,
                null_count_total: 0,
            });
        }

        let mut bloom_col_set = HashSet::new();
        let mut bloom_hashes = HashMap::new();
        for name in &options.bloom_cols {
            if let Some(idx) = schema.index_of(name) {
                bloom_col_set.insert(idx);
                bloom_hashes.insert(idx, Vec::new());
            }
        }
        let hot_bloom_col_idx = options
            .hot_bloom_col
            .as_deref()
            .and_then(|name| schema.index_of(name));
        if let Some(idx) = hot_bloom_col_idx {
            bloom_col_set.insert(idx);
            bloom_hashes.entry(idx).or_default();
        }

        let mut narrow_trigram_col_set = HashSet::new();
        let mut narrow_trigrams = HashMap::new();
        if options.profile == WriteProfile::L1Compact {
            for name in &options.narrow_trigram_cols {
                if let Some(idx) = schema.index_of(name)
                    && fields[idx].data_type == DataType::Utf8
                    && logical_locality[idx] != LocalityGroup::ExternalPayload
                {
                    narrow_trigram_col_set.insert(idx);
                    narrow_trigrams.insert(idx, Vec::new());
                }
            }
        }

        let pack_ids = match (&pack_builder, options.pack_id) {
            (Some(_), Some(id)) => vec![id],
            _ => Vec::new(),
        };

        let num_logical = fields.len();
        Ok(Self {
            schema,
            options,
            pack_builder,
            logical_locality,
            bloom_col_set,
            hot_bloom_col_idx,
            narrow_trigram_col_set,
            pack_ids,
            uses_external_pack: false,
            rg_buf_cols: vec![Vec::new(); num_logical],
            rg_buf_rows: 0,
            rg0_initialized: false,
            shredded_maps: Vec::new(),
            logical_to_shredded: HashMap::new(),
            phys_col_meta,
            phys_blocks: vec![Vec::new(); num_logical],
            rg_entries: Vec::new(),
            rg_data_blobs: Vec::new(),
            cur_data_offset: FILE_HEADER_LEN as u64,
            total_rows: 0,
            core_bytes: 0,
            wide_bytes: 0,
            l0_payload_bytes: 0,
            bloom_hashes,
            bypass_detectors: HashMap::new(),
            narrow_trigrams,
            max_chunk_id_seen: HashMap::new(),
        })
    }

    /// Sets or updates the external `.stpk` `pack_id` recorded in `FileSchemaMeta`.
    pub fn set_pack_id(&mut self, pack_id: [u8; 16]) {
        if self.pack_ids.is_empty() {
            self.pack_ids.push(pack_id);
        } else {
            self.pack_ids[0] = pack_id;
        }
    }

    pub(crate) fn set_pack_ids(&mut self, pack_ids: Vec<[u8; 16]>) {
        self.pack_ids = pack_ids;
    }

    /// Appends a [`Batch`] to the stream, flushing full Row Groups incrementally.
    pub fn write_batch(&mut self, batch: &Batch) -> Result<(), StatamError> {
        if batch.schema().as_ref() != self.schema.as_ref() {
            return Err(StatamError::SchemaMismatch {
                reason: "batch schema does not match writer schema",
            });
        }
        if batch.is_empty() {
            return Ok(());
        }

        let compacted;
        let active_batch = if batch.selection().is_all(batch.raw_len()) {
            batch
        } else {
            compacted = batch.compact(None)?;
            &compacted
        };

        let total = active_batch.raw_len();
        let mut offset = 0usize;
        while offset < total {
            let space = self.options.row_group_size - self.rg_buf_rows;
            let take = (total - offset).min(space);
            for (col_idx, col) in active_batch.columns().iter().enumerate() {
                let piece = if offset == 0 && take == total {
                    col.clone()
                } else {
                    col.slice(offset, take)?
                };
                self.rg_buf_cols[col_idx].push(piece);
            }
            self.rg_buf_rows += take;
            offset += take;

            if self.rg_buf_rows == self.options.row_group_size {
                self.flush_row_group()?;
            }
        }

        Ok(())
    }

    /// Rewrites pre-encoded `PayloadRefRow` slices for `ExternalPayload` columns (used by GDPR).
    pub(crate) fn write_batch_with_prechunked_refs(
        &mut self,
        batch: &Batch,
        prechunked: &HashMap<usize, Vec<PayloadRefRow>>,
        l0_local_chunks: &HashMap<(usize, u32), Vec<u8>>,
    ) -> Result<(), StatamError> {
        if batch.schema().as_ref() != self.schema.as_ref() {
            return Err(StatamError::SchemaMismatch {
                reason: "batch schema does not match writer schema",
            });
        }
        if batch.is_empty() {
            return Ok(());
        }
        if !batch.selection().is_all(batch.raw_len()) {
            return Err(StatamError::SchemaMismatch {
                reason: "write_batch_with_prechunked_refs requires Selection::All",
            });
        }

        if self.rg_buf_rows > 0 && !prechunked.is_empty() {
            self.flush_row_group()?;
        }

        let n = batch.raw_len();
        let mut offset = 0usize;
        while offset < n {
            let take = (n - offset).min(self.options.row_group_size - self.rg_buf_rows);
            for (col_idx, col) in batch.columns().iter().enumerate() {
                let piece = if offset == 0 && take == n {
                    col.clone()
                } else {
                    col.slice(offset, take)?
                };
                self.rg_buf_cols[col_idx].push(piece);
            }
            if prechunked.is_empty() {
                self.rg_buf_rows += take;
                offset += take;
                if self.rg_buf_rows == self.options.row_group_size {
                    self.flush_row_group()?;
                }
            } else {
                self.rg_buf_rows += take;
                let sliced_refs: HashMap<usize, Vec<PayloadRefRow>> = prechunked
                    .iter()
                    .map(|(&cidx, rows)| (cidx, rows[offset..offset + take].to_vec()))
                    .collect();
                offset += take;
                self.flush_row_group_with_refs(Some((&sliced_refs, l0_local_chunks)))?;
            }
        }
        Ok(())
    }

    /// Finishes the `.statam` file (flushing any partial Row Group, writing Lazy Indexes,
    /// Hot Tail, and 112-byte Postscript) into `out`.
    pub fn finish<W: Write>(mut self, mut out: W) -> Result<WriteStats, StatamError> {
        if self.rg_buf_rows > 0 {
            self.flush_row_group()?;
        }

        let zstd_level = self.options.zstd_level;
        let dict_codec = match self.options.profile {
            WriteProfile::L0Fast => Codec::Lz4,
            WriteProfile::L1Compact => default_l1_codec(),
        };

        let num_logical_cols = self.schema.len() as u32;
        let num_phys_cols = self.phys_col_meta.len() as u32;
        let num_row_groups =
            u32::try_from(self.rg_entries.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "num_row_groups exceeds u32::MAX",
            })?;

        let mut total_blocks_usize = 0usize;
        for blocks in &self.phys_blocks {
            total_blocks_usize = total_blocks_usize.checked_add(blocks.len()).ok_or(
                StatamError::BoundsExceeded {
                    reason: "total_blocks overflows usize",
                },
            )?;
        }
        if total_blocks_usize > MAX_TOTAL_BLOCKS {
            return Err(StatamError::BoundsExceeded {
                reason: "total_blocks exceeds MAX_TOTAL_BLOCKS",
            });
        }
        let total_blocks = total_blocks_usize as u32;

        // Build Directory arrays + Schema section first to measure exact Hot Tail budget.
        let mut col_dirs = Vec::with_capacity(self.phys_col_meta.len());
        let mut running_block_idx = 0u32;
        for (pidx, state) in self.phys_col_meta.iter().enumerate() {
            let nblocks = self.phys_blocks[pidx].len() as u32;
            let mut col_flags = 0u8;
            if state.is_shredded_subcol {
                col_flags |= COL_FLAG_IS_SHREDDED_SUBCOL;
            }
            if state.is_payload_refs {
                col_flags |= COL_FLAG_IS_PAYLOAD_REFS;
            }
            if state.used_dict_encoding
                && let Some(dict) = &state.dict
            {
                col_flags |= COL_FLAG_HAS_FILE_DICT;
                if dict.is_sorted() {
                    col_flags |= COL_FLAG_DICT_SORTED;
                }
            }
            col_dirs.push(ColumnDirEntry {
                phys_col_id: state.phys_col_id,
                logical_col_id: state.logical_col_id,
                locality_group: state.locality_group,
                col_flags,
                reserved: 0,
                num_blocks: nblocks,
                first_block_idx: running_block_idx,
                dict_offset: 0,
                dict_len: 0,
                dict_entry_count: if state.used_dict_encoding {
                    state.dict.as_ref().map_or(0, |d| d.len() as u32)
                } else {
                    0
                },
                bloom_offset: 0,
                bloom_len: 0,
                null_count_total: state.null_count_total,
                trigram_offset: 0,
            });
            running_block_idx = running_block_idx.saturating_add(nblocks);
        }

        let pack_ids = if self.uses_external_pack {
            if self.pack_ids.is_empty() {
                vec![[0u8; 16]]
            } else {
                self.pack_ids.clone()
            }
        } else {
            Vec::new()
        };
        if pack_ids.len() > MAX_PACK_REFS {
            return Err(StatamError::BoundsExceeded {
                reason: "pack_ids exceeds MAX_PACK_REFS",
            });
        }

        let schema_meta = FileSchemaMeta {
            table_kind: self.options.table_kind,
            sort_cols: self.options.sort_cols.clone(),
            pack_ids: pack_ids.clone(),
            schema: Arc::clone(&self.schema),
            locality_groups: self.logical_locality.clone(),
            shredded_maps: self.shredded_maps.clone(),
        };
        let schema_bytes = encode_schema(&schema_meta)?;

        let col_dir_bytes_len = col_dirs.len() * COLUMN_DIR_ENTRY_LEN;
        let rg_dir_bytes_len = self.rg_entries.len() * ROW_GROUP_ENTRY_LEN;
        let block_dir_bytes_len = total_blocks_usize * BLOCK_ENTRY_LEN;

        let mut cur_hot_tail_size = POSTSCRIPT_LEN
            + schema_bytes.len()
            + col_dir_bytes_len
            + rg_dir_bytes_len
            + block_dir_bytes_len;

        let mut hot_bloom_buf = Vec::new();
        let mut hot_dict_buf = Vec::new();
        let mut lazy_data_buf = Vec::new();
        let mut lazy_entries = Vec::new();
        let mut cur_file_off = self.cur_data_offset;

        // 1. Primary Hot Bloom (e.g. `trace_id`) goes into Hot Tail when present.
        if let Some(hot_idx) = self.hot_bloom_col_idx
            && let Some(bloom_bytes) = self.build_bloom_for_col(hot_idx)?
        {
            if cur_hot_tail_size + bloom_bytes.len() <= MAX_HOT_TAIL_BYTES {
                let off = hot_bloom_buf.len() as u64;
                let len = bloom_bytes.len() as u32;
                hot_bloom_buf.extend_from_slice(&bloom_bytes);
                cur_hot_tail_size += bloom_bytes.len();
                col_dirs[hot_idx].col_flags |= COL_FLAG_BLOOM_IN_HOT_TAIL;
                col_dirs[hot_idx].bloom_offset = off;
                col_dirs[hot_idx].bloom_len = len;
            } else {
                append_lazy_entry(
                    LazyIndexKind::Bloom,
                    hot_idx as u32,
                    &bloom_bytes,
                    &mut cur_file_off,
                    &mut lazy_data_buf,
                    &mut lazy_entries,
                    &mut col_dirs[hot_idx],
                )?;
                cur_hot_tail_size += LAZY_INDEX_ENTRY_LEN;
            }
        }

        // 2. Small file-level dictionaries (<= 8 KiB each while Hot Tail <= 768 KiB).
        for (pidx, state) in self.phys_col_meta.iter().enumerate() {
            if !state.used_dict_encoding {
                continue;
            }
            let Some(dict) = &state.dict else {
                continue;
            };
            let dict_bytes = dict.encode(dict_codec, zstd_level)?;
            if dict_bytes.len() <= HOT_DICT_MAX_BYTES
                && cur_hot_tail_size + dict_bytes.len() <= HOT_TAIL_BUDGET_BYTES
            {
                let off = hot_dict_buf.len() as u64;
                let len = dict_bytes.len() as u32;
                hot_dict_buf.extend_from_slice(&dict_bytes);
                cur_hot_tail_size += dict_bytes.len();
                col_dirs[pidx].col_flags |= COL_FLAG_DICT_IN_HOT_TAIL;
                col_dirs[pidx].dict_offset = off;
                col_dirs[pidx].dict_len = len;
            } else {
                append_lazy_entry(
                    LazyIndexKind::Dict,
                    pidx as u32,
                    &dict_bytes,
                    &mut cur_file_off,
                    &mut lazy_data_buf,
                    &mut lazy_entries,
                    &mut col_dirs[pidx],
                )?;
                cur_hot_tail_size += LAZY_INDEX_ENTRY_LEN;
            }
        }

        // 3. Secondary blooms (Hot Tail while total <= 768 KiB, else Lazy Index).
        let mut bloom_indices: Vec<usize> = self.bloom_col_set.iter().copied().collect();
        bloom_indices.sort_unstable();
        for cidx in bloom_indices {
            if Some(cidx) == self.hot_bloom_col_idx {
                continue;
            }
            if let Some(bloom_bytes) = self.build_bloom_for_col(cidx)? {
                if cur_hot_tail_size + bloom_bytes.len() + LAZY_INDEX_ENTRY_LEN
                    <= HOT_TAIL_BUDGET_BYTES
                {
                    let off = hot_bloom_buf.len() as u64;
                    let len = bloom_bytes.len() as u32;
                    hot_bloom_buf.extend_from_slice(&bloom_bytes);
                    cur_hot_tail_size += bloom_bytes.len();
                    col_dirs[cidx].col_flags |= COL_FLAG_BLOOM_IN_HOT_TAIL;
                    col_dirs[cidx].bloom_offset = off;
                    col_dirs[cidx].bloom_len = len;
                } else {
                    append_lazy_entry(
                        LazyIndexKind::Bloom,
                        cidx as u32,
                        &bloom_bytes,
                        &mut cur_file_off,
                        &mut lazy_data_buf,
                        &mut lazy_entries,
                        &mut col_dirs[cidx],
                    )?;
                    cur_hot_tail_size += LAZY_INDEX_ENTRY_LEN;
                }
            }
        }

        // 4. Narrow-column trigram index sections (L1Compact, when not purely Dict/ConstNull).
        let mut has_any_trigram = false;
        let mut trigram_indices: Vec<usize> = self.narrow_trigram_col_set.iter().copied().collect();
        trigram_indices.sort_unstable();
        for cidx in trigram_indices {
            if !self.phys_col_meta[cidx].has_non_dict_data_block {
                continue;
            }
            if let Some(filters) = self.narrow_trigrams.get(&cidx)
                && let Some(sec_bytes) = encode_narrow_trigram_section(filters)?
            {
                append_lazy_entry(
                    LazyIndexKind::NarrowTrigram,
                    cidx as u32,
                    &sec_bytes,
                    &mut cur_file_off,
                    &mut lazy_data_buf,
                    &mut lazy_entries,
                    &mut col_dirs[cidx],
                )?;
                col_dirs[cidx].col_flags |= COL_FLAG_HAS_TRIGRAM_INDEX;
                has_any_trigram = true;
            }
        }

        // 5. Assemble uncompressed Hot Tail body.
        let mut hot_tail_raw = Vec::with_capacity(cur_hot_tail_size);
        let schema_off = 0u32;
        let schema_len =
            u32::try_from(schema_bytes.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "schema_len overflows u32",
            })?;
        hot_tail_raw.extend_from_slice(&schema_bytes);

        let col_dir_off = hot_tail_raw.len() as u32;
        for cd in &col_dirs {
            hot_tail_raw.extend_from_slice(&cd.encode());
        }

        let rg_dir_off = hot_tail_raw.len() as u32;
        for rg in &self.rg_entries {
            hot_tail_raw.extend_from_slice(&rg.encode());
        }

        let block_dir_off = hot_tail_raw.len() as u32;
        for col_blocks in &self.phys_blocks {
            for blk in col_blocks {
                hot_tail_raw.extend_from_slice(&blk.encode());
            }
        }

        let (hot_bloom_off, hot_bloom_len) = if hot_bloom_buf.is_empty() {
            (0u32, 0u32)
        } else {
            let off = hot_tail_raw.len() as u32;
            let len = hot_bloom_buf.len() as u32;
            hot_tail_raw.extend_from_slice(&hot_bloom_buf);
            (off, len)
        };

        let (hot_dict_off, hot_dict_len) = if hot_dict_buf.is_empty() {
            (0u32, 0u32)
        } else {
            let off = hot_tail_raw.len() as u32;
            let len = hot_dict_buf.len() as u32;
            hot_tail_raw.extend_from_slice(&hot_dict_buf);
            (off, len)
        };

        let (lazy_dir_off, lazy_dir_len) = if lazy_entries.is_empty() {
            (0u32, 0u32)
        } else {
            let off = hot_tail_raw.len() as u32;
            for entry in &lazy_entries {
                hot_tail_raw.extend_from_slice(&entry.encode());
            }
            let len = (lazy_entries.len() * LAZY_INDEX_ENTRY_LEN) as u32;
            (off, len)
        };

        if hot_tail_raw.len() > MAX_HOT_TAIL_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "uncompressed hot tail exceeds MAX_HOT_TAIL_BYTES",
            });
        }
        let hot_tail_raw_len = hot_tail_raw.len() as u32;

        let (hot_tail_stored, is_lz4) = if self.options.compress_hot_tail_lz4 {
            let compressed = lz4_flex::block::compress(&hot_tail_raw);
            if compressed.len().saturating_add(16) < hot_tail_raw.len() {
                (compressed, true)
            } else {
                (hot_tail_raw, false)
            }
        } else {
            (hot_tail_raw, false)
        };

        let hot_tail_len = hot_tail_stored
            .len()
            .checked_add(POSTSCRIPT_LEN)
            .and_then(|v| u32::try_from(v).ok())
            .ok_or(StatamError::BoundsExceeded {
                reason: "hot_tail_len overflows u32",
            })?;
        if (hot_tail_len as usize) > MAX_HOT_TAIL_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "stored hot tail exceeds MAX_HOT_TAIL_BYTES",
            });
        }

        let mut flags = 0u16;
        if self.options.profile == WriteProfile::L1Compact {
            flags |= FLAG_L1_COMPACT;
        }
        if !pack_ids.is_empty() {
            flags |= FLAG_HAS_PACK_REFS;
        }
        if !self.shredded_maps.is_empty() {
            flags |= FLAG_HAS_SHREDDED_MAPS;
        }
        if has_any_trigram {
            flags |= FLAG_HAS_TRIGRAM_INDEX;
        }
        if is_lz4 {
            flags |= FLAG_HOT_TAIL_LZ4;
        }

        let header = FileHeader {
            format_version: FORMAT_VERSION,
            flags,
            table_kind: self.options.table_kind,
            checksum_alg: CHECKSUM_ALG_CRC32C,
            reserved0: 0,
            required_features: 0,
            enc_envelope_offset: 0,
            enc_envelope_len: 0,
        };
        let hot_tail_crc32c = crc32c::crc32c(&hot_tail_stored);
        let postscript = Postscript {
            hot_tail_len,
            hot_tail_raw_len,
            format_version: FORMAT_VERSION,
            flags,
            required_features: 0,
            row_count: self.total_rows,
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
            enc_envelope_len: 0,
            enc_envelope_offset: 0,
            hot_tail_crc32c,
            reserved: 0,
        };

        out.write_all(&header.encode())?;
        for blob in &self.rg_data_blobs {
            out.write_all(blob)?;
        }
        if !lazy_data_buf.is_empty() {
            out.write_all(&lazy_data_buf)?;
        }
        out.write_all(&hot_tail_stored)?;
        out.write_all(&postscript.encode())?;

        let total_bytes = cur_file_off.saturating_add(u64::from(hot_tail_len));
        let shredded_subcols = (self.phys_col_meta.len() - self.schema.len()) as u16;

        Ok(WriteStats {
            total_bytes,
            row_count: self.total_rows,
            num_row_groups,
            total_blocks,
            hot_tail_bytes: hot_tail_len,
            hot_tail_raw_bytes: hot_tail_raw_len,
            core_bytes: self.core_bytes,
            wide_bytes: self.wide_bytes,
            l0_payload_bytes: self.l0_payload_bytes,
            lazy_index_bytes: lazy_data_buf.len() as u64,
            shredded_subcols,
            pack_ids,
        })
    }

    fn flush_row_group(&mut self) -> Result<(), StatamError> {
        self.flush_row_group_with_refs(None)
    }

    fn flush_row_group_with_refs(
        &mut self,
        prechunked: Option<PrechunkedRefs<'_>>,
    ) -> Result<(), StatamError> {
        let rg_rows = self.rg_buf_rows;
        if rg_rows == 0 {
            return Ok(());
        }
        if self.rg_entries.len() >= MAX_ROW_GROUPS {
            return Err(StatamError::BoundsExceeded {
                reason: "row group count exceeds MAX_ROW_GROUPS",
            });
        }
        let next_total = self.total_rows.saturating_add(rg_rows as u64);
        if next_total > MAX_ROWS_PER_FILE {
            return Err(StatamError::BoundsExceeded {
                reason: "total row count exceeds MAX_ROWS_PER_FILE",
            });
        }

        let first_row_u32 =
            u32::try_from(self.total_rows).map_err(|_| StatamError::BoundsExceeded {
                reason: "row_group first_row overflows u32",
            })?;

        // Concatenate each logical column's buffered slices for this RG.
        let mut rg_cols = Vec::with_capacity(self.schema.len());
        for (cidx, pieces) in self.rg_buf_cols.iter_mut().enumerate() {
            let dt = &self.schema.fields()[cidx].data_type;
            let combined = concat_rg_column(dt, pieces, rg_rows)?;
            pieces.clear();
            rg_cols.push(combined);
        }
        self.rg_buf_rows = 0;

        // Collect SBBF bloom hashes across this RG.
        let bloom_indices: Vec<usize> = self.bloom_col_set.iter().copied().collect();
        for cidx in bloom_indices {
            if let Some(hashes) = self.bloom_hashes.get_mut(&cidx) {
                collect_column_bloom_hashes(&rg_cols[cidx], hashes)?;
            }
        }

        // RG0 initialization: promote ShreddedMap keys (L1Compact) & seed FileDicts.
        if !self.rg0_initialized {
            self.init_rg0(&rg_cols)?;
            self.rg0_initialized = true;
        }

        let zstd_level = self.options.zstd_level;
        let preferred_codec = match self.options.profile {
            WriteProfile::L0Fast => Codec::Lz4,
            WriteProfile::L1Compact => default_l1_codec(),
        };

        let mut core_buf = Vec::new();
        let mut wide_buf = Vec::new();
        let mut l0_local_builder = LocalL0PayloadBuilder::default();

        let core_offset = self.cur_data_offset;
        let mb_size = self.options.mini_block_size;

        // Phase A: Encode Core logical columns.
        for (cidx, rg_col) in rg_cols.iter().enumerate() {
            if self.logical_locality[cidx] != LocalityGroup::Core {
                continue;
            }
            self.encode_rg_scalar_or_nested_col(
                cidx,
                rg_col,
                first_row_u32,
                mb_size,
                core_offset,
                zstd_level,
                &mut core_buf,
            )?;
        }

        // Phase B: Encode ShreddedMap sub-columns (in Core!) and residual blocks (in WideRow!).
        let num_shredded = self.shredded_maps.len();
        let mut deferred_wide_residuals: Vec<(usize, Vec<EncodedMiniBlock>, Vec<u16>)> = Vec::new();
        for s_idx in 0..num_shredded {
            let logical_col = self.shredded_maps[s_idx].logical_col_idx as usize;
            let promoted_keys: Vec<String> = self.shredded_maps[s_idx]
                .keys
                .iter()
                .map(|k| k.key.clone())
                .collect();
            let sub_col_indices: Vec<usize> = self.shredded_maps[s_idx]
                .keys
                .iter()
                .map(|k| k.sub_col_idx as usize)
                .collect();

            let mut mb_residuals = Vec::new();
            let mut mb_row_counts = Vec::new();

            let mut row_off = 0usize;
            while row_off < rg_rows {
                let mb_len = (rg_rows - row_off).min(mb_size);
                let mb_first_row = first_row_u32 + row_off as u32;
                let sliced = rg_cols[logical_col].slice(row_off, mb_len)?;
                let flat = sliced.materialize(None)?;
                let shredded = shred_map(&flat, &promoted_keys)?;

                for (k_idx, &sub_pidx) in sub_col_indices.iter().enumerate() {
                    self.shredded_maps[s_idx].keys[k_idx].residual_count =
                        self.shredded_maps[s_idx].keys[k_idx]
                            .residual_count
                            .saturating_add(shredded.residual_counts[k_idx]);

                    let sub_raw =
                        encode_shredded_subcol(&shredded.sub_cols[k_idx], self.options.profile)?;
                    let blk_off = core_offset + core_buf.len() as u64;
                    let info = write_framed_block(
                        Encoding::ShreddedMap,
                        &sub_raw,
                        preferred_codec,
                        zstd_level,
                        &mut core_buf,
                    )?;
                    self.phys_blocks[sub_pidx].push(BlockEntry {
                        first_row: mb_first_row,
                        row_count: mb_len as u16,
                        null_count: 0,
                        offset: blk_off,
                        stored_len: info.stored_len,
                        raw_len: info.raw_len,
                        encoding: Encoding::ShreddedMap,
                        codec: info.codec,
                        block_flags: 0,
                        min_inline_len: 0,
                        max_inline_len: 0,
                        reserved: [0; 3],
                        min_inline: [0; 16],
                        max_inline: [0; 16],
                    });
                }

                let res_enc = crate::codec::encode_shredded_residual_block(
                    &shredded.residual,
                    self.options.profile,
                )?;
                self.phys_col_meta[logical_col].null_count_total = self.phys_col_meta[logical_col]
                    .null_count_total
                    .saturating_add(u32::from(res_enc.null_count));
                mb_residuals.push(res_enc);
                mb_row_counts.push(mb_len as u16);

                row_off += mb_len;
            }

            deferred_wide_residuals.push((logical_col, mb_residuals, mb_row_counts));
        }

        let core_len = u32::try_from(core_buf.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "row group core_len overflows u32",
        })?;
        let wide_offset =
            core_offset
                .checked_add(u64::from(core_len))
                .ok_or(StatamError::BoundsExceeded {
                    reason: "wide_offset overflow",
                })?;

        // Phase C: Write WideRow blocks (regular WideRow cols, deferred shredded residuals, and PayloadRefs).
        for (logical_col, mb_residuals, mb_row_counts) in deferred_wide_residuals {
            let mut cur_first = first_row_u32;
            for (enc, rc) in mb_residuals.iter().zip(mb_row_counts.iter()) {
                let entry =
                    enc.write_framed_entry(cur_first, *rc, wide_offset, zstd_level, &mut wide_buf)?;
                self.phys_blocks[logical_col].push(entry);
                cur_first += u32::from(*rc);
            }
        }

        for (cidx, rg_col) in rg_cols.iter().enumerate() {
            match self.logical_locality[cidx] {
                LocalityGroup::Core => {}
                LocalityGroup::WideRow => {
                    if self.logical_to_shredded.contains_key(&cidx) {
                        continue;
                    }
                    self.encode_rg_scalar_or_nested_col(
                        cidx,
                        rg_col,
                        first_row_u32,
                        mb_size,
                        wide_offset,
                        zstd_level,
                        &mut wide_buf,
                    )?;
                }
                LocalityGroup::ExternalPayload => {
                    self.encode_rg_payload_col(
                        cidx,
                        rg_col,
                        first_row_u32,
                        mb_size,
                        wide_offset,
                        preferred_codec,
                        zstd_level,
                        prechunked,
                        &mut wide_buf,
                        &mut l0_local_builder,
                    )?;
                }
            }
        }

        let wide_len = u32::try_from(wide_buf.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "row group wide_len overflows u32",
        })?;
        let after_wide =
            wide_offset
                .checked_add(u64::from(wide_len))
                .ok_or(StatamError::BoundsExceeded {
                    reason: "after_wide offset overflow",
                })?;

        let l0_bytes = l0_local_builder.finish()?;
        let (l0_payload_offset, l0_payload_len) = if l0_bytes.is_empty() {
            (0u64, 0u32)
        } else {
            let len = u32::try_from(l0_bytes.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "l0_payload_len overflows u32",
            })?;
            (after_wide, len)
        };

        self.rg_entries.push(RowGroupEntry {
            first_row: first_row_u32,
            row_count: rg_rows as u32,
            core_offset,
            core_len,
            wide_len,
            wide_offset,
            l0_payload_offset,
            l0_payload_len,
            rg_flags: 0,
        });

        let mut rg_blob = Vec::with_capacity(core_buf.len() + wide_buf.len() + l0_bytes.len());
        rg_blob.extend_from_slice(&core_buf);
        rg_blob.extend_from_slice(&wide_buf);
        rg_blob.extend_from_slice(&l0_bytes);

        self.cur_data_offset = after_wide.checked_add(u64::from(l0_payload_len)).ok_or(
            StatamError::BoundsExceeded {
                reason: "cur_data_offset overflow",
            },
        )?;
        self.total_rows = next_total;
        self.core_bytes = self.core_bytes.saturating_add(u64::from(core_len));
        self.wide_bytes = self.wide_bytes.saturating_add(u64::from(wide_len));
        self.l0_payload_bytes = self
            .l0_payload_bytes
            .saturating_add(u64::from(l0_payload_len));
        self.rg_data_blobs.push(rg_blob);

        Ok(())
    }

    fn init_rg0(&mut self, rg_cols: &[Column]) -> Result<(), StatamError> {
        let fields = self.schema.fields();

        // 1. Shredded maps in L1Compact.
        if self.options.profile == WriteProfile::L1Compact {
            for (cidx, field) in fields.iter().enumerate() {
                if field.data_type != DataType::MapUtf8 {
                    continue;
                }
                let flat = rg_cols[cidx].materialize(None)?;
                let FlatData::MapUtf8(map) = flat.data() else {
                    continue;
                };
                let promoted = select_shredded_keys(map);
                if promoted.is_empty() {
                    continue;
                }

                let mut key_entries = Vec::with_capacity(promoted.len());
                for key in promoted {
                    let sub_idx = self.phys_col_meta.len();
                    if sub_idx >= MAX_COLUMNS {
                        return Err(StatamError::BoundsExceeded {
                            reason: "physical column count with shredded maps exceeds MAX_COLUMNS",
                        });
                    }
                    let sub_u16 = sub_idx as u16;
                    self.phys_col_meta.push(PhysColWriteState {
                        phys_col_id: sub_u16,
                        logical_col_id: cidx as u16,
                        locality_group: LocalityGroup::Core,
                        is_shredded_subcol: true,
                        is_payload_refs: false,
                        dict: None,
                        used_dict_encoding: false,
                        has_non_dict_data_block: false,
                        null_count_total: 0,
                    });
                    self.phys_blocks.push(Vec::new());
                    key_entries.push(ShreddedMapKeyEntry {
                        key,
                        sub_col_idx: sub_u16,
                        residual_count: 0,
                    });
                }

                let sm_idx = self.shredded_maps.len();
                self.shredded_maps.push(ShreddedMapDir {
                    logical_col_idx: cidx as u16,
                    keys: key_entries,
                });
                self.logical_to_shredded.insert(cidx, sm_idx);
            }
        }

        // 2. Seed file-level dictionaries from RG0 on low-cardinality columns.
        for (cidx, field) in fields.iter().enumerate() {
            if self.logical_locality[cidx] == LocalityGroup::ExternalPayload {
                continue;
            }
            if DictValueKind::from_data_type(&field.data_type).is_none() {
                continue;
            }
            if HIGH_CARDINALITY_ID_COLS.contains(&field.name.as_str()) {
                continue;
            }
            let flat = rg_cols[cidx].materialize(None)?;
            let builder = match self.options.profile {
                WriteProfile::L0Fast => DictBuilder::l0_fast(),
                WriteProfile::L1Compact => DictBuilder::l1_compact(),
            };
            let Some((dict, _)) = builder.build(&flat)? else {
                continue;
            };
            let non_null_rows = flat
                .len()
                .saturating_sub(flat.nulls().map_or(0, |bm| bm.count_zeros()));
            let is_known_low_card = KNOWN_LOW_CARD_DICT_COLS.contains(&field.name.as_str());
            let low_ndv = if is_known_low_card {
                dict.len() <= 4_096 && (non_null_rows < 16 || dict.len() * 2 <= non_null_rows + 4)
            } else {
                dict.len() <= 4_096
                    && ((non_null_rows >= 16 && dict.len() * 2 <= non_null_rows) || dict.len() <= 8)
            };
            if low_ndv {
                self.phys_col_meta[cidx].dict = Some(dict);
            }
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_rg_scalar_or_nested_col(
        &mut self,
        cidx: usize,
        col: &Column,
        first_row_u32: u32,
        mb_size: usize,
        base_offset: u64,
        zstd_level: i32,
        out_buf: &mut Vec<u8>,
    ) -> Result<(), StatamError> {
        let dt = self.schema.fields()[cidx].data_type.clone();
        let rg_rows = col.len();
        let track_trigram = self.narrow_trigram_col_set.contains(&cidx);
        let dict_clone = self.phys_col_meta[cidx].dict.clone();

        let mut row_off = 0usize;
        while row_off < rg_rows {
            let mb_len = (rg_rows - row_off).min(mb_size);
            let mb_first_row = first_row_u32 + row_off as u32;
            let sliced = col.slice(row_off, mb_len)?;
            let enc = encode_mini_block(&sliced, &dt, self.options.profile, dict_clone.as_ref())?;

            if enc.encoding == Encoding::Dict {
                self.phys_col_meta[cidx].used_dict_encoding = true;
            } else if enc.encoding != Encoding::ConstNull && enc.encoding != Encoding::Const {
                self.phys_col_meta[cidx].has_non_dict_data_block = true;
            }
            self.phys_col_meta[cidx].null_count_total = self.phys_col_meta[cidx]
                .null_count_total
                .saturating_add(u32::from(enc.null_count));

            let entry = enc.write_framed_entry(
                mb_first_row,
                mb_len as u16,
                base_offset,
                zstd_level,
                out_buf,
            )?;
            self.phys_blocks[cidx].push(entry);

            if track_trigram {
                let filter = build_narrow_trigram_for_mb(&sliced, entry.stored_len as usize);
                if let Some(list) = self.narrow_trigrams.get_mut(&cidx) {
                    list.push(filter);
                }
            }

            row_off += mb_len;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_rg_payload_col(
        &mut self,
        cidx: usize,
        col: &Column,
        first_row_u32: u32,
        mb_size: usize,
        base_offset: u64,
        preferred_codec: Codec,
        zstd_level: i32,
        prechunked: Option<PrechunkedRefs<'_>>,
        wide_buf: &mut Vec<u8>,
        l0_local: &mut LocalL0PayloadBuilder,
    ) -> Result<(), StatamError> {
        let rg_rows = col.len();
        let flat = if prechunked.is_some() {
            None
        } else {
            Some(col.materialize(None)?)
        };

        let mut row_off = 0usize;
        while row_off < rg_rows {
            let mb_len = (rg_rows - row_off).min(mb_size);
            let mb_first_row = first_row_u32 + row_off as u32;

            let ref_rows = if let Some((ref_map, local_chunks)) = prechunked {
                if let Some(rows) = ref_map.get(&cidx) {
                    let slice = &rows[row_off..row_off + mb_len];
                    let mut remapped = Vec::with_capacity(slice.len());
                    for row in slice {
                        match row {
                            PayloadRefRow::Null => remapped.push(PayloadRefRow::Null),
                            PayloadRefRow::Inline(b) => {
                                remapped.push(PayloadRefRow::Inline(b.clone()));
                            }
                            PayloadRefRow::Chunks(refs) => {
                                let mut out_refs = Vec::with_capacity(refs.len());
                                for &(pidx, cid) in refs {
                                    if pidx == L0_LOCAL_PACK_IDX {
                                        let bytes = local_chunks.get(&(cidx, cid)).ok_or(
                                            StatamError::Corrupt {
                                                reason: "missing L0 local chunk during rewrite",
                                            },
                                        )?;
                                        let new_cid = l0_local.add_chunk(bytes)?;
                                        out_refs.push((L0_LOCAL_PACK_IDX, new_cid));
                                    } else {
                                        self.uses_external_pack = true;
                                        out_refs.push((pidx, cid));
                                    }
                                }
                                remapped.push(PayloadRefRow::Chunks(out_refs));
                            }
                        }
                    }
                    remapped
                } else {
                    vec![PayloadRefRow::Null; mb_len]
                }
            } else {
                let fc = flat.as_ref().ok_or(StatamError::Corrupt {
                    reason: "missing materialized payload column",
                })?;
                self.build_payload_ref_rows(cidx, fc, row_off, mb_len, l0_local)?
            };

            let null_count_usize = ref_rows
                .iter()
                .filter(|r| matches!(r, PayloadRefRow::Null))
                .count();
            let null_count = u16::try_from(null_count_usize).unwrap_or(u16::MAX);
            self.phys_col_meta[cidx].null_count_total = self.phys_col_meta[cidx]
                .null_count_total
                .saturating_add(u32::from(null_count));

            if null_count_usize == mb_len {
                self.phys_blocks[cidx].push(BlockEntry {
                    first_row: mb_first_row,
                    row_count: mb_len as u16,
                    null_count,
                    offset: 0,
                    stored_len: 0,
                    raw_len: 0,
                    encoding: Encoding::ConstNull,
                    codec: Codec::None,
                    block_flags: BLOCK_FLAG_ALL_NULL | BLOCK_FLAG_HAS_NULLS,
                    min_inline_len: 0,
                    max_inline_len: 0,
                    reserved: [0; 3],
                    min_inline: [0; 16],
                    max_inline: [0; 16],
                });
            } else {
                let mut raw_body = Vec::new();
                encode_payload_refs(&ref_rows, &mut raw_body)?;
                let blk_off = base_offset + wide_buf.len() as u64;
                let info = write_framed_block(
                    Encoding::PayloadRefs,
                    &raw_body,
                    preferred_codec,
                    zstd_level,
                    wide_buf,
                )?;
                let block_flags = if null_count > 0 {
                    BLOCK_FLAG_HAS_NULLS
                } else {
                    0
                };
                self.phys_blocks[cidx].push(BlockEntry {
                    first_row: mb_first_row,
                    row_count: mb_len as u16,
                    null_count,
                    offset: blk_off,
                    stored_len: info.stored_len,
                    raw_len: info.raw_len,
                    encoding: Encoding::PayloadRefs,
                    codec: info.codec,
                    block_flags,
                    min_inline_len: 0,
                    max_inline_len: 0,
                    reserved: [0; 3],
                    min_inline: [0; 16],
                    max_inline: [0; 16],
                });
            }

            row_off += mb_len;
        }
        Ok(())
    }

    fn build_payload_ref_rows(
        &mut self,
        cidx: usize,
        flat: &FlatColumn,
        row_off: usize,
        mb_len: usize,
        l0_local: &mut LocalL0PayloadBuilder,
    ) -> Result<Vec<PayloadRefRow>, StatamError> {
        let FlatData::Utf8(u) = flat.data() else {
            return Err(StatamError::SchemaMismatch {
                reason: "ExternalPayload column must be DataType::Utf8",
            });
        };
        let nulls = flat.nulls();
        let mut out = Vec::with_capacity(mb_len);
        let mut chunk_scratch = Vec::new();

        for r in row_off..row_off + mb_len {
            if nulls.is_some_and(|bm| !bm.get(r)) {
                out.push(PayloadRefRow::Null);
                continue;
            }
            let s = u.get(r).unwrap_or("");
            let bytes = s.as_bytes();
            if bytes.len() <= INLINE_PAYLOAD_THRESHOLD {
                out.push(PayloadRefRow::Inline(bytes.to_vec()));
                continue;
            }

            if self.options.profile == WriteProfile::L1Compact
                && let Some(pb) = self.pack_builder.as_deref_mut()
            {
                self.uses_external_pack = true;
                let bypassed = self
                    .bypass_detectors
                    .get(&cidx)
                    .is_some_and(DedupBypassDetector::should_bypass);
                let mut refs = Vec::new();

                if bypassed {
                    // W4 incompressible bypass: skip CDC hashing and intern direct slices.
                    for slice in bytes.chunks(MAX_FRAME_RAW_BYTES.min(64 * 1024)) {
                        let cid = pb.intern_chunk(slice)?;
                        refs.push((0u16, cid));
                    }
                } else {
                    chunk_payload(bytes, &mut chunk_scratch);
                    let detector = self.bypass_detectors.entry(cidx).or_default();
                    let need_sample = !detector.is_decided();
                    for &chunk in &chunk_scratch {
                        let cid = pb.intern_chunk(chunk)?;
                        if need_sample {
                            let is_hit = match self.max_chunk_id_seen.get(&cidx).copied() {
                                Some(max_cid) => cid <= max_cid,
                                None => false,
                            };
                            let max_entry = self.max_chunk_id_seen.entry(cidx).or_insert(cid);
                            if cid > *max_entry {
                                *max_entry = cid;
                            }
                            let stored_len = if chunk.len() > 32 {
                                lz4_flex::block::compress(chunk).len().min(chunk.len())
                            } else {
                                chunk.len()
                            };
                            detector.record(is_hit, chunk.len(), stored_len);
                        }
                        refs.push((0u16, cid));
                    }
                }
                out.push(PayloadRefRow::Chunks(refs));
            } else {
                // L0Fast (or L1Compact with pack_builder == None): self-contained local frames.
                let mut refs = Vec::new();
                for slice in bytes.chunks(MAX_FRAME_RAW_BYTES) {
                    let cid = l0_local.add_chunk(slice)?;
                    refs.push((L0_LOCAL_PACK_IDX, cid));
                }
                out.push(PayloadRefRow::Chunks(refs));
            }
        }

        Ok(out)
    }

    fn build_bloom_for_col(&self, cidx: usize) -> Result<Option<Vec<u8>>, StatamError> {
        let Some(hashes) = self.bloom_hashes.get(&cidx) else {
            return Ok(None);
        };
        if hashes.is_empty() {
            return Ok(None);
        }
        let mut unique = hashes.clone();
        unique.sort_unstable();
        unique.dedup();
        let mut sbbf = Sbbf::new_for_ndv(unique.len(), BLOOM_FPP);
        for h in unique {
            sbbf.insert_hash(h);
        }
        Ok(Some(sbbf.to_bytes()))
    }
}

#[derive(Default)]
struct LocalL0PayloadBuilder {
    frames: Vec<(u64, u32, u32, Vec<u8>)>, // (rel_frame_off, stored_len, raw_len, framed_bytes)
    chunks: Vec<ChunkLoc>,
    cur_raw: Vec<u8>,
    cur_chunks: Vec<(u32, u32)>, // (offset_in_frame, raw_len)
}

impl LocalL0PayloadBuilder {
    fn add_chunk(&mut self, bytes: &[u8]) -> Result<u32, StatamError> {
        if bytes.len() > MAX_FRAME_RAW_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "L0 payload chunk exceeds MAX_FRAME_RAW_BYTES",
            });
        }
        if !self.cur_raw.is_empty()
            && self.cur_raw.len().saturating_add(bytes.len()) > L0_PAYLOAD_FRAME_TARGET_BYTES
        {
            self.flush_frame()?;
        }
        let cid = u32::try_from(self.chunks.len() + self.cur_chunks.len()).map_err(|_| {
            StatamError::BoundsExceeded {
                reason: "L0 payload chunk_id overflows u32",
            }
        })?;
        let off_in_frame =
            u32::try_from(self.cur_raw.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "L0 payload offset_in_frame overflows u32",
            })?;
        let raw_len = u32::try_from(bytes.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "L0 payload chunk raw_len overflows u32",
        })?;
        self.cur_raw.extend_from_slice(bytes);
        self.cur_chunks.push((off_in_frame, raw_len));

        if self.cur_raw.len() >= L0_PAYLOAD_FRAME_TARGET_BYTES {
            self.flush_frame()?;
        }
        Ok(cid)
    }

    fn flush_frame(&mut self) -> Result<(), StatamError> {
        if self.cur_raw.is_empty() {
            return Ok(());
        }
        let mut framed = Vec::new();
        let info = write_framed_pack_frame(&self.cur_raw, Codec::Lz4, 0, 0, &mut framed)?;
        let fid = u32::try_from(self.frames.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "L0 payload frame_id overflows u32",
        })?;
        for (off_in_frame, raw_len) in self.cur_chunks.drain(..) {
            self.chunks.push(ChunkLoc {
                frame_id: fid,
                offset_in_frame: off_in_frame,
                raw_len,
            });
        }
        self.frames.push((0, info.stored_len, info.raw_len, framed));
        self.cur_raw.clear();
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<u8>, StatamError> {
        if !self.cur_raw.is_empty() {
            self.flush_frame()?;
        }
        if self.chunks.is_empty() {
            return Ok(Vec::new());
        }

        let header_len = 12 + self.frames.len() * 16 + self.chunks.len() * 12;
        let mut rel_off = header_len as u64;
        for (off, _, _, framed) in &mut self.frames {
            *off = rel_off;
            rel_off =
                rel_off
                    .checked_add(framed.len() as u64)
                    .ok_or(StatamError::BoundsExceeded {
                        reason: "L0 payload region size overflow",
                    })?;
        }
        let total_len = usize::try_from(rel_off).map_err(|_| StatamError::BoundsExceeded {
            reason: "L0 payload region overflows usize",
        })?;
        let mut out = Vec::with_capacity(total_len);
        out.extend_from_slice(&L0_PAYLOAD_DIR_MAGIC);
        out.extend_from_slice(&(self.frames.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.chunks.len() as u32).to_le_bytes());
        for (off, stored_len, raw_len, _) in &self.frames {
            out.extend_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&stored_len.to_le_bytes());
            out.extend_from_slice(&raw_len.to_le_bytes());
        }
        for c in &self.chunks {
            out.extend_from_slice(&c.encode());
        }
        for (_, _, _, framed) in &self.frames {
            out.extend_from_slice(framed);
        }
        Ok(out)
    }
}

fn classify_locality_group(
    field: &cetra_column::Field,
    payload_names: &HashSet<&str>,
) -> LocalityGroup {
    if field.data_type == DataType::Utf8 && payload_names.contains(field.name.as_str()) {
        return LocalityGroup::ExternalPayload;
    }
    if WIDE_ROW_COL_NAMES.contains(&field.name.as_str())
        || matches!(
            field.data_type,
            DataType::MapUtf8
                | DataType::Events
                | DataType::Links
                | DataType::List(_)
                | DataType::Binary
        )
    {
        return LocalityGroup::WideRow;
    }
    LocalityGroup::Core
}

fn concat_rg_column(
    dt: &DataType,
    pieces: &[Column],
    total_rows: usize,
) -> Result<Column, StatamError> {
    if pieces.len() == 1 {
        return Ok(pieces[0].clone());
    }
    // Preserve Column::Const if every piece is an identical Const.
    if let Some(Column::Const {
        value: first_val, ..
    }) = pieces.first()
        && pieces
            .iter()
            .all(|p| matches!(p, Column::Const { value, .. } if value.semantic_eq(first_val)))
    {
        return Ok(Column::try_new_const(first_val.clone(), total_rows)?);
    }

    let mut flats = Vec::with_capacity(pieces.len());
    for p in pieces {
        flats.push(p.materialize(None)?);
    }
    let flat_refs: Vec<&FlatColumn> = flats.iter().collect();
    let merged = concat_flat_columns(dt, &flat_refs, total_rows)?;
    Ok(Column::Flat(merged))
}

/// Concatenates flat columns of identical `DataType` into one `FlatColumn`.
pub(crate) fn concat_flat_columns(
    dt: &DataType,
    parts: &[&FlatColumn],
    total_rows: usize,
) -> Result<FlatColumn, StatamError> {
    if parts.len() == 1 {
        return Ok(parts[0].clone());
    }
    if matches!(dt, DataType::Null) {
        return Ok(FlatColumn::try_new(FlatData::Null(total_rows), None)?);
    }

    let has_nulls = parts.iter().any(|p| p.has_nulls());
    let nulls = if has_nulls {
        let mut bm = cetra_column::Bitmap::new_ones(total_rows);
        let mut pos = 0usize;
        for p in parts {
            if let Some(p_nulls) = p.nulls() {
                for i in 0..p.len() {
                    if !p_nulls.get(i) {
                        let _ = bm.set(pos + i, false);
                    }
                }
            }
            pos += p.len();
        }
        if bm.all_ones() { None } else { Some(bm) }
    } else {
        None
    };

    let data = match dt {
        DataType::Null => FlatData::Null(total_rows),
        DataType::Bool => {
            let mut bm = cetra_column::Bitmap::new_zeroed(total_rows);
            let mut pos = 0usize;
            for p in parts {
                if let FlatData::Bool(src) = p.data() {
                    for i in 0..p.len() {
                        if src.get(i) {
                            let _ = bm.set(pos + i, true);
                        }
                    }
                }
                pos += p.len();
            }
            FlatData::Bool(bm)
        }
        DataType::I64 => {
            let mut out = Vec::with_capacity(total_rows);
            for p in parts {
                if let FlatData::I64(v) = p.data() {
                    out.extend_from_slice(v);
                }
            }
            FlatData::I64(out)
        }
        DataType::F64 => {
            let mut out = Vec::with_capacity(total_rows);
            for p in parts {
                if let FlatData::F64(v) = p.data() {
                    out.extend_from_slice(v);
                }
            }
            FlatData::F64(out)
        }
        DataType::Decimal128 { precision, scale } => {
            let mut out = Vec::with_capacity(total_rows);
            for p in parts {
                if let FlatData::Decimal128 { values, .. } = p.data() {
                    out.extend_from_slice(values);
                }
            }
            FlatData::Decimal128 {
                values: out,
                precision: *precision,
                scale: *scale,
            }
        }
        DataType::TimestampNs { utc } => {
            let mut out = Vec::with_capacity(total_rows);
            for p in parts {
                if let FlatData::TimestampNs { values, .. } = p.data() {
                    out.extend_from_slice(values);
                }
            }
            FlatData::TimestampNs {
                values: out,
                utc: *utc,
            }
        }
        DataType::Date => {
            let mut out = Vec::with_capacity(total_rows);
            for p in parts {
                if let FlatData::Date(v) = p.data() {
                    out.extend_from_slice(v);
                }
            }
            FlatData::Date(out)
        }
        DataType::Interval => {
            let mut out = Vec::with_capacity(total_rows);
            for p in parts {
                if let FlatData::Interval(v) = p.data() {
                    out.extend_from_slice(v);
                }
            }
            FlatData::Interval(out)
        }
        DataType::Utf8 => {
            let mut offsets = Vec::with_capacity(total_rows + 1);
            offsets.push(0u32);
            let mut total_bytes = 0usize;
            for p in parts {
                if let FlatData::Utf8(u) = p.data() {
                    total_bytes = total_bytes.checked_add(u.bytes().len()).ok_or(
                        StatamError::BoundsExceeded {
                            reason: "utf8 concat byte length overflow",
                        },
                    )?;
                }
            }
            let _ = u32::try_from(total_bytes).map_err(|_| StatamError::BoundsExceeded {
                reason: "utf8 concat exceeds u32::MAX",
            })?;
            let mut bytes = Vec::with_capacity(total_bytes);
            for p in parts {
                if let FlatData::Utf8(u) = p.data() {
                    let base = bytes.len() as u32;
                    bytes.extend_from_slice(u.bytes());
                    for &o in &u.offsets()[1..] {
                        offsets.push(base + o);
                    }
                }
            }
            FlatData::Utf8(Utf8Data::try_new(offsets, bytes)?)
        }
        DataType::Binary => {
            let mut offsets = Vec::with_capacity(total_rows + 1);
            offsets.push(0u32);
            let mut total_bytes = 0usize;
            for p in parts {
                if let FlatData::Binary(b) = p.data() {
                    total_bytes = total_bytes.checked_add(b.bytes().len()).ok_or(
                        StatamError::BoundsExceeded {
                            reason: "binary concat byte length overflow",
                        },
                    )?;
                }
            }
            let _ = u32::try_from(total_bytes).map_err(|_| StatamError::BoundsExceeded {
                reason: "binary concat exceeds u32::MAX",
            })?;
            let mut bytes = Vec::with_capacity(total_bytes);
            for p in parts {
                if let FlatData::Binary(b) = p.data() {
                    let base = bytes.len() as u32;
                    bytes.extend_from_slice(b.bytes());
                    for &o in &b.offsets()[1..] {
                        offsets.push(base + o);
                    }
                }
            }
            FlatData::Binary(BinaryData::try_new(offsets, bytes)?)
        }
        DataType::FixedBytes8 => {
            let mut out = Vec::with_capacity(total_rows);
            for p in parts {
                if let FlatData::FixedBytes8(v) = p.data() {
                    out.extend_from_slice(v);
                }
            }
            FlatData::FixedBytes8(out)
        }
        DataType::FixedBytes16 => {
            let mut out = Vec::with_capacity(total_rows);
            for p in parts {
                if let FlatData::FixedBytes16(v) = p.data() {
                    out.extend_from_slice(v);
                }
            }
            FlatData::FixedBytes16(out)
        }
        DataType::List(inner) => {
            let mut offsets = Vec::with_capacity(total_rows + 1);
            offsets.push(0u32);
            let mut total_child = 0usize;
            let mut child_flats = Vec::with_capacity(parts.len());
            for p in parts {
                if let FlatData::List {
                    offsets: p_off,
                    values,
                    ..
                } = p.data()
                {
                    let base = total_child as u32;
                    for &o in &p_off[1..] {
                        offsets.push(base + o);
                    }
                    total_child = total_child.checked_add(values.len()).ok_or(
                        StatamError::BoundsExceeded {
                            reason: "list concat child length overflow",
                        },
                    )?;
                    child_flats.push(values.materialize(None)?);
                }
            }
            let child_refs: Vec<&FlatColumn> = child_flats.iter().collect();
            let merged_child = concat_flat_columns(inner, &child_refs, total_child)?;
            FlatData::List {
                item_type: (**inner).clone(),
                offsets,
                values: Box::new(Column::Flat(merged_child)),
            }
        }
        DataType::MapUtf8 => {
            let mut offsets = Vec::with_capacity(total_rows + 1);
            offsets.push(0u32);
            let mut total_entries = 0usize;
            let mut key_parts = Vec::with_capacity(parts.len());
            let mut val_parts = Vec::with_capacity(parts.len());
            for p in parts {
                if let FlatData::MapUtf8(m) = p.data() {
                    let base = total_entries as u32;
                    for &o in &m.offsets()[1..] {
                        offsets.push(base + o);
                    }
                    total_entries += m.entry_count();
                    key_parts.push(m.keys());
                    val_parts.push(m.values());
                }
            }
            let keys = concat_utf8_data(&key_parts)?;
            let values = concat_utf8_data(&val_parts)?;
            let val_nulls = concat_entry_bitmaps(
                parts.iter().filter_map(|p| match p.data() {
                    FlatData::MapUtf8(m) => Some((m.entry_count(), m.value_nulls())),
                    _ => None,
                }),
                total_entries,
            );
            FlatData::MapUtf8(Box::new(cetra_column::MapData::try_new(
                offsets, keys, values, val_nulls,
            )?))
        }
        DataType::Events => {
            let mut offsets = Vec::with_capacity(total_rows + 1);
            offsets.push(0u32);
            let mut total_events = 0usize;
            let mut times = Vec::new();
            let mut name_parts = Vec::with_capacity(parts.len());
            let mut attr_parts = Vec::with_capacity(parts.len());
            for p in parts {
                if let FlatData::Events(e) = p.data() {
                    let base = total_events as u32;
                    for &o in &e.offsets()[1..] {
                        offsets.push(base + o);
                    }
                    total_events += e.event_count();
                    times.extend_from_slice(e.times());
                    name_parts.push(e.names());
                    attr_parts.push(e.attributes());
                }
            }
            let names = concat_utf8_data(&name_parts)?;
            let attributes = concat_utf8_data(&attr_parts)?;
            let attr_nulls = concat_entry_bitmaps(
                parts.iter().filter_map(|p| match p.data() {
                    FlatData::Events(e) => Some((e.event_count(), e.attribute_nulls())),
                    _ => None,
                }),
                total_events,
            );
            FlatData::Events(Box::new(cetra_column::EventsData::try_new(
                offsets, times, names, attributes, attr_nulls,
            )?))
        }
        DataType::Links => {
            let mut offsets = Vec::with_capacity(total_rows + 1);
            offsets.push(0u32);
            let mut total_links = 0usize;
            let mut trace_ids = Vec::new();
            let mut span_ids = Vec::new();
            let mut attr_parts = Vec::with_capacity(parts.len());
            for p in parts {
                if let FlatData::Links(l) = p.data() {
                    let base = total_links as u32;
                    for &o in &l.offsets()[1..] {
                        offsets.push(base + o);
                    }
                    total_links += l.link_count();
                    trace_ids.extend_from_slice(l.trace_ids());
                    span_ids.extend_from_slice(l.span_ids());
                    attr_parts.push(l.attributes());
                }
            }
            let attributes = concat_utf8_data(&attr_parts)?;
            let attr_nulls = concat_entry_bitmaps(
                parts.iter().filter_map(|p| match p.data() {
                    FlatData::Links(l) => Some((l.link_count(), l.attribute_nulls())),
                    _ => None,
                }),
                total_links,
            );
            FlatData::Links(Box::new(cetra_column::LinksData::try_new(
                offsets, trace_ids, span_ids, attributes, attr_nulls,
            )?))
        }
    };

    Ok(FlatColumn::try_new(data, nulls)?)
}

fn concat_utf8_data(parts: &[&Utf8Data]) -> Result<Utf8Data, StatamError> {
    let total_rows: usize = parts.iter().map(|p| p.len()).sum();
    let total_bytes = parts.iter().try_fold(0usize, |acc, p| {
        acc.checked_add(p.bytes().len())
            .ok_or(StatamError::BoundsExceeded {
                reason: "utf8 concat byte length overflow",
            })
    })?;
    let _ = u32::try_from(total_bytes).map_err(|_| StatamError::BoundsExceeded {
        reason: "utf8 concat exceeds u32::MAX",
    })?;
    let mut offsets = Vec::with_capacity(total_rows + 1);
    offsets.push(0u32);
    let mut bytes = Vec::with_capacity(total_bytes);
    for p in parts {
        let base = bytes.len() as u32;
        bytes.extend_from_slice(p.bytes());
        for &o in &p.offsets()[1..] {
            offsets.push(base + o);
        }
    }
    Ok(Utf8Data::try_new(offsets, bytes)?)
}

fn concat_entry_bitmaps<'a>(
    parts: impl Iterator<Item = (usize, Option<&'a cetra_column::Bitmap>)> + Clone,
    total_len: usize,
) -> Option<cetra_column::Bitmap> {
    let any_nulls = parts.clone().any(|(_, bm)| bm.is_some());
    if !any_nulls {
        return None;
    }
    let mut out = cetra_column::Bitmap::new_ones(total_len);
    let mut pos = 0usize;
    for (len, bm) in parts {
        if let Some(b) = bm {
            for i in 0..len {
                if !b.get(i) {
                    let _ = out.set(pos + i, false);
                }
            }
        }
        pos += len;
    }
    if out.all_ones() { None } else { Some(out) }
}

fn collect_column_bloom_hashes(col: &Column, out: &mut Vec<u64>) -> Result<(), StatamError> {
    let n = col.len();
    if n == 0 {
        return Ok(());
    }
    match col {
        Column::Const { value, .. } => {
            if let Some(h) = scalar_bloom_hash(value) {
                out.push(h);
            }
        }
        Column::Dict(d) => {
            let flat_vals = d.values();
            let mut val_hashes = Vec::with_capacity(flat_vals.len());
            for i in 0..flat_vals.len() {
                val_hashes.push(flat_row_bloom_hash(flat_vals, i));
            }
            for (r, &c) in d.codes().iter().enumerate() {
                if !d.is_null(r)
                    && let Some(Some(h)) = val_hashes.get(c as usize)
                {
                    out.push(*h);
                }
            }
        }
        Column::Rle(r) => {
            for run_idx in 0..r.values().len() {
                if let Some(h) = flat_row_bloom_hash(r.values(), run_idx) {
                    out.push(h);
                }
            }
        }
        Column::Flat(f) => {
            for row in 0..n {
                if let Some(h) = flat_row_bloom_hash(f, row) {
                    out.push(h);
                }
            }
        }
    }
    Ok(())
}

fn scalar_bloom_hash(val: &ScalarValue) -> Option<u64> {
    match val {
        ScalarValue::Null(_) => None,
        ScalarValue::FixedBytes16(b) => Some(statam_bloom_hash(b)),
        ScalarValue::FixedBytes8(b) => Some(statam_bloom_hash(b)),
        ScalarValue::Utf8(s) => Some(statam_bloom_hash(s.as_bytes())),
        ScalarValue::Binary(b) => Some(statam_bloom_hash(b)),
        ScalarValue::I64(v) | ScalarValue::TimestampNs { value: v, .. } => {
            Some(statam_bloom_hash(&v.to_le_bytes()))
        }
        ScalarValue::Date(v) => Some(statam_bloom_hash(&v.to_le_bytes())),
        ScalarValue::Decimal128 { value: v, .. } => Some(statam_bloom_hash(&v.to_le_bytes())),
        _ => None,
    }
}

fn flat_row_bloom_hash(flat: &FlatColumn, row: usize) -> Option<u64> {
    if flat.is_null(row) {
        return None;
    }
    match flat.data() {
        FlatData::FixedBytes16(v) => Some(statam_bloom_hash(&v[row])),
        FlatData::FixedBytes8(v) => Some(statam_bloom_hash(&v[row])),
        FlatData::Utf8(u) => u.get(row).map(|s| statam_bloom_hash(s.as_bytes())),
        FlatData::Binary(b) => b.get(row).map(statam_bloom_hash),
        FlatData::I64(v) | FlatData::TimestampNs { values: v, .. } => {
            Some(statam_bloom_hash(&v[row].to_le_bytes()))
        }
        FlatData::Date(v) => Some(statam_bloom_hash(&v[row].to_le_bytes())),
        FlatData::Decimal128 { values, .. } => Some(statam_bloom_hash(&values[row].to_le_bytes())),
        _ => None,
    }
}

fn build_narrow_trigram_for_mb(col: &Column, stored_len: usize) -> Option<Vec<u8>> {
    if stored_len == 0 || col.is_empty() {
        return None;
    }
    let flat = col.materialize(None).ok()?;
    let FlatData::Utf8(u) = flat.data() else {
        return None;
    };
    let mut hashes = Vec::new();
    let nulls = flat.nulls();
    for r in 0..flat.len() {
        if nulls.is_none_or(|bm| bm.get(r))
            && let Some(s) = u.get(r)
        {
            extract_trigram_hashes(s.as_bytes(), false, &mut hashes);
        }
    }
    if hashes.is_empty() {
        return None;
    }
    hashes.sort_unstable();
    hashes.dedup();

    let desired = hashes
        .len()
        .saturating_mul(10)
        .checked_div(8)
        .unwrap_or(MIN_TRIGRAM_BLOOM_BYTES)
        .clamp(MIN_TRIGRAM_BLOOM_BYTES, MAX_TRIGRAM_BLOOM_BYTES)
        .next_power_of_two()
        .min(MAX_TRIGRAM_BLOOM_BYTES);

    let mut bloom = TrigramBloom::new(desired);
    bloom.insert_hashes(&hashes);
    if bloom.bit_density() > MAX_NARROW_TRIGRAM_DENSITY {
        return None;
    }
    Some(bloom.as_bytes().to_vec())
}

fn encode_narrow_trigram_section(
    filters: &[Option<Vec<u8>>],
) -> Result<Option<Vec<u8>>, StatamError> {
    if filters.iter().all(Option::is_none) {
        return Ok(None);
    }
    let count = u32::try_from(filters.len()).map_err(|_| StatamError::BoundsExceeded {
        reason: "narrow trigram block count overflows u32",
    })?;
    let mut body = Vec::new();
    let mut table = Vec::with_capacity(filters.len() * 8);
    for opt in filters {
        match opt {
            None => {
                table.extend_from_slice(&0u32.to_le_bytes());
                table.extend_from_slice(&0u32.to_le_bytes());
            }
            Some(bytes) => {
                let off = u32::try_from(body.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "narrow trigram body offset overflows u32",
                })?;
                let len = u32::try_from(bytes.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "narrow trigram filter len overflows u32",
                })?;
                table.extend_from_slice(&off.to_le_bytes());
                table.extend_from_slice(&len.to_le_bytes());
                body.extend_from_slice(bytes);
            }
        }
    }
    let total = 8 + table.len() + body.len();
    if total > MAX_BLOCK_RAW_BYTES {
        return Err(StatamError::BoundsExceeded {
            reason: "narrow trigram section exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&NARROW_TRIGRAM_MAGIC);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&table);
    out.extend_from_slice(&body);
    Ok(Some(out))
}

fn append_lazy_entry(
    kind: LazyIndexKind,
    phys_col_id: u32,
    bytes: &[u8],
    cur_file_off: &mut u64,
    lazy_data_buf: &mut Vec<u8>,
    lazy_entries: &mut Vec<LazyIndexEntry>,
    col_dir: &mut ColumnDirEntry,
) -> Result<(), StatamError> {
    let len = u32::try_from(bytes.len()).map_err(|_| StatamError::BoundsExceeded {
        reason: "lazy index section overflows u32",
    })?;
    let off = *cur_file_off;
    let crc32c = crc32c::crc32c(bytes);
    lazy_data_buf.extend_from_slice(bytes);
    *cur_file_off =
        cur_file_off
            .checked_add(u64::from(len))
            .ok_or(StatamError::BoundsExceeded {
                reason: "lazy index file offset overflow",
            })?;
    lazy_entries.push(LazyIndexEntry {
        kind,
        reserved: [0; 3],
        phys_col_id,
        offset: off,
        len,
        crc32c,
    });
    match kind {
        LazyIndexKind::Bloom => {
            col_dir.bloom_offset = off;
            col_dir.bloom_len = len;
        }
        LazyIndexKind::Dict => {
            col_dir.dict_offset = off;
            col_dir.dict_len = len;
        }
        LazyIndexKind::NarrowTrigram => {
            col_dir.trigram_offset = off;
        }
    }
    Ok(())
}

#[cfg(not(miri))]
fn default_l1_codec() -> Codec {
    Codec::Zstd
}

#[cfg(miri)]
fn default_l1_codec() -> Codec {
    Codec::Lz4
}
