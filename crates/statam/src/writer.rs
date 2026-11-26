//! Streaming PAX Row-Group writer for `.statam` files (`L0Fast` & `L1Compact`).

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::Write;
use std::sync::Arc;

use cetra_column::{
    Batch, BinaryData, Column, DataType, FlatColumn, FlatData, ScalarValue, Schema, Utf8Data,
};

use crate::StatamError;
use crate::codec::dict::{DICT_PASSTHROUGH_MAX_BYTES, DictBuilder, DictValueKind, FileDict};
use crate::codec::nested::{encode_shredded_subcol_with_codec, select_shredded_keys, shred_map};
use crate::codec::{
    EncodedMiniBlock, WriteProfile, encode_dict_codes_precomputed, encode_mini_block_with_locality,
};
use crate::format::{
    BLOCK_ENTRY_LEN, BLOCK_FLAG_ALL_NULL, BLOCK_FLAG_HAS_NULLS, BlockEntry, CHECKSUM_ALG_CRC32C,
    COL_FLAG_BLOOM_IN_HOT_TAIL, COL_FLAG_DICT_IN_HOT_TAIL, COL_FLAG_DICT_SORTED,
    COL_FLAG_HAS_FILE_DICT, COL_FLAG_HAS_TRIGRAM_INDEX, COL_FLAG_IS_PAYLOAD_REFS,
    COL_FLAG_IS_SHREDDED_SUBCOL, COLUMN_DIR_ENTRY_LEN, ChunkLoc, Codec, ColumnDirEntry,
    DEFAULT_TAIL_HINT, Encoding, FILE_HEADER_LEN, FLAG_HAS_PACK_REFS, FLAG_HAS_SHREDDED_MAPS,
    FLAG_HAS_TRIGRAM_INDEX, FLAG_HOT_TAIL_LZ4, FLAG_L1_COMPACT, FORMAT_VERSION, FileHeader,
    FileSchemaMeta, LAZY_INDEX_ENTRY_LEN, LazyIndexEntry, LazyIndexKind, LocalityGroup,
    MAX_BLOCK_RAW_BYTES, MAX_BLOOM_BYTES, MAX_COLUMNS, MAX_FRAME_RAW_BYTES, MAX_HOT_TAIL_BYTES,
    MAX_MINI_BLOCK_SIZE, MAX_PACK_REFS, MAX_ROW_GROUP_SIZE, MAX_ROW_GROUPS, MAX_ROWS_PER_FILE,
    MAX_TOTAL_BLOCKS, POSTSCRIPT_LEN, Postscript, ROW_GROUP_ENTRY_LEN, ROW_GROUP_SIZE,
    RowGroupEntry, ShreddedMapDir, ShreddedMapKeyEntry, TableKind, encode_schema,
    write_framed_block, write_framed_pack_frame,
};
use crate::parallel::{MAX_WRITE_THREADS, parallel_map_in_order};
use crate::payload::chunker::{
    DedupBypassDetector, INLINE_PAYLOAD_THRESHOLD, chunk_json_array_continuation, chunk_payload,
};
use crate::payload::pack::ChunkPackBuilder;
use crate::payload::{
    PayloadRefRow, encode_payload_refs, encode_payload_refs_l0_direct,
    encode_payload_refs_l0_from_utf8,
};
use crate::simd::bloom::{BloomIndex, Sbbf, statam_bloom_hash};
use crate::simd::trigram::{MAX_TRIGRAM_BLOOM_BYTES, MIN_TRIGRAM_BLOOM_BYTES, TrigramBloom};

/// Compressed Hot Tail soft budget (120 KiB) for keeping small FileDicts and secondary blooms hot
/// within `DEFAULT_TAIL_HINT` (128 KiB).
const HOT_TAIL_SOFT_BUDGET_BYTES: usize = 120 * 1024;
/// Hard Hot Tail budget (1 MiB) so large files with >128 KiB directories still keep primary bloom hot.
const HOT_TAIL_HARD_BUDGET_BYTES: usize = 1 << 20;
/// Minimum compressed mini-block bytes required to emit a narrow-column trigram filter.
const MIN_NARROW_TRIGRAM_STORED_BYTES: usize = 2560;
/// Maximum size of a single FileDict kept inside the Hot Tail (8 KiB).
const HOT_DICT_MAX_BYTES: usize = 8 * 1024;
/// Target SBBF false-positive probability.
const BLOOM_FPP: f64 = 0.01;
/// Target uncompressed bytes per L0 local payload `PF` frame (512 KiB).
const L0_PAYLOAD_FRAME_TARGET_BYTES: usize = 512 * 1024;
/// Minimum Row Group rows required before spawning parallel RG worker threads.
const MIN_PARALLEL_RG_ROWS: usize = 1_024;
/// Maximum total raw bytes queued in `LocalL0PayloadBuilder::pending_frames` before flushing (32 MiB).
const MAX_PENDING_RAW_BYTES: usize = 32 * 1024 * 1024;
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
type PayloadPrefixCacheEntry = (usize, usize, Vec<(u16, u32)>);
type PayloadExactCacheEntry = (usize, Vec<(u16, u32)>);
type Rg0DictResult = (FileDict, Vec<u32>, Option<cetra_column::Bitmap>);
type PendingL0Frame = (Vec<u8>, Vec<(u32, u32)>);

/// Tier-1A small Hot Analytics `Core` columns written FIRST at `core_offset`.
const HOT_ANALYTICS_TIER1_SMALL_COLS: &[&str] = &[
    "status",
    "span_kind",
    "model",
    "name",
    "data_type",
    "value_num",
    "value_str",
];

/// Tier-1B Timestamp `Core` columns placed immediately before `metadata` shredded sub-columns
/// so `start_time`/`created_at`, `metadata.*`, and `trace_id` pack contiguously in the first block.
const HOT_ANALYTICS_TIER1_TIME_COLS: &[&str] = &["start_time", "created_at"];

/// Analytics numeric columns placed immediately after `trace_id` and before `span_id`
/// so `map_key_lookup` skips them while `time_bucket_agg` still avoids reading `span_id`.
const HOT_ANALYTICS_POST_TRACE_COLS: &[&str] = &["cost_total", "total_tokens"];

/// Tier-2 Hot Analytics `Core` columns written immediately after `ID_CORE_COLS`.
const HOT_ANALYTICS_TIER2_COLS: &[&str] = &[
    "duration_ns",
    "environment",
    "provider",
    "service_name",
    "release",
];

/// High-frequency ID `Core` columns written immediately after Tier-2 (before cold `Core` cols
/// and non-`metadata` shredded sub-columns) so `map_key_lookup`, `top_k_list`, and point lookups
/// stay contiguous near the front of each Row Group.
const ID_CORE_COLS: &[&str] = &[
    "trace_id",
    "span_id",
    "score_id",
    "parent_span_id",
    "user_id",
    "session_id",
    "end_time",
    "ingest_time",
];

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
    "status",
    "status_code",
    "target",
    "model",
    "request_model",
    "response_model",
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
    "cost_source",
    "input_format",
    "output_format",
    "scope_name",
    "scope_version",
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
    /// Rows per PAX Row Group (default: `131_072`).
    pub row_group_size: usize,
    /// Rows per Mini-Block (default: `8_192`).
    pub mini_block_size: usize,
    /// Zstd compression level (`-5` for `L0Fast`, `3` for `L1Compact`).
    pub zstd_level: i32,
    /// Whether to LZ4-compress the Hot Tail body when smaller (default: `true`).
    pub compress_hot_tail_lz4: bool,
    /// External `.stpk` pack ID when writing `L1Compact` with a [`ChunkPackBuilder`].
    pub pack_id: Option<[u8; 16]>,
    /// Number of worker threads for parallel Row Group encoding (default: `1`).
    pub write_threads: usize,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self::for_table(TableKind::Generic, WriteProfile::L1Compact)
    }
}

impl WriteOptions {
    /// Builds [`WriteOptions`] populated with table-specific defaults per `spec-v0.md`.
    pub fn for_table(table_kind: TableKind, profile: WriteProfile) -> Self {
        let bloom_cols = match (table_kind, profile) {
            (TableKind::Spans, WriteProfile::L0Fast) => vec![
                "trace_id".to_owned(),
                "session_id".to_owned(),
                "user_id".to_owned(),
            ],
            (TableKind::Spans, WriteProfile::L1Compact) => vec![
                "trace_id".to_owned(),
                "span_id".to_owned(),
                "session_id".to_owned(),
                "user_id".to_owned(),
            ],
            (TableKind::Traces, _) => vec![
                "trace_id".to_owned(),
                "session_id".to_owned(),
                "user_id".to_owned(),
            ],
            (TableKind::Scores, _) => vec!["trace_id".to_owned(), "score_id".to_owned()],
            (TableKind::Generic, _) => Vec::new(),
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
        let row_group_size = ROW_GROUP_SIZE;
        let mini_block_size = 8_192;
        let zstd_level = match profile {
            WriteProfile::L0Fast => -5,
            WriteProfile::L1Compact => 3,
        };

        Self {
            profile,
            table_kind,
            sort_cols: Vec::new(),
            bloom_cols,
            hot_bloom_col: Some("trace_id".to_owned()),
            payload_cols,
            narrow_trigram_cols,
            row_group_size,
            mini_block_size,
            zstd_level,
            compress_hot_tail_lz4: true,
            pack_id: None,
            write_threads: 1,
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
    // Zero-copy L0Fast ExternalPayload accumulators for the current partial RG.
    pending_l0_local: LocalL0PayloadBuilder,
    pending_l0_mbs: HashMap<usize, Vec<(u16, u16, Vec<u8>)>>,
    pending_l0_partial: HashMap<usize, Vec<PayloadRefRow>>,

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

    // Per-column per-RG SBBF bloom accumulators (keyed by logical column index).
    bloom_rgs: HashMap<usize, Vec<Sbbf>>,
    bloom_has_values: HashMap<usize, bool>,
    // Per-column W4 bypass detectors (keyed by logical column index).
    bypass_detectors: HashMap<usize, DedupBypassDetector>,
    // Per-column trigram blooms for narrow_trigram_cols (keyed by logical column index).
    narrow_trigrams: HashMap<usize, Vec<Option<Vec<u8>>>>,
    // Highest chunk_id observed per logical payload col for dedup hit tracking.
    max_chunk_id_seen: HashMap<usize, u32>,
    // Precomputed RG0 FileDict codes + column null bitmaps from `init_rg0` to avoid double-hashing.
    rg0_dict_codes: HashMap<usize, (Vec<u32>, Option<cetra_column::Bitmap>)>,
}

struct PhysColWriteState {
    phys_col_id: u16,
    logical_col_id: u16,
    locality_group: LocalityGroup,
    is_shredded_subcol: bool,
    is_payload_refs: bool,
    dict: Option<Arc<FileDict>>,
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
        if options.row_group_size == 0 || options.row_group_size > MAX_ROW_GROUP_SIZE {
            return Err(StatamError::BoundsExceeded {
                reason: "row_group_size must be in 1..=MAX_ROW_GROUP_SIZE",
            });
        }
        if options.mini_block_size == 0 || options.mini_block_size > MAX_MINI_BLOCK_SIZE {
            return Err(StatamError::BoundsExceeded {
                reason: "mini_block_size must be in 1..=MAX_MINI_BLOCK_SIZE",
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
        let mut bloom_rgs = HashMap::new();
        let mut bloom_has_values = HashMap::new();
        for name in &options.bloom_cols {
            if let Some(idx) = schema.index_of(name)
                && logical_locality[idx] != LocalityGroup::ExternalPayload
            {
                bloom_col_set.insert(idx);
                bloom_rgs.insert(idx, Vec::new());
                bloom_has_values.insert(idx, false);
            }
        }
        let hot_bloom_col_idx = options
            .hot_bloom_col
            .as_deref()
            .and_then(|name| schema.index_of(name))
            .filter(|&idx| logical_locality[idx] != LocalityGroup::ExternalPayload);
        if let Some(idx) = hot_bloom_col_idx {
            bloom_col_set.insert(idx);
            bloom_rgs.entry(idx).or_default();
            bloom_has_values.entry(idx).or_insert(false);
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

        let mut pack_builder = pack_builder;
        if let Some(pb) = pack_builder.as_deref_mut() {
            pb.enable_boundary_tracking();
        }
        let pack_ids = match (&pack_builder, options.pack_id) {
            (Some(_), Some(id)) => vec![id],
            _ => Vec::new(),
        };

        let num_logical = fields.len();
        let pending_l0_local =
            LocalL0PayloadBuilder::new_with_threads(options.zstd_level, options.write_threads);
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
            pending_l0_local,
            pending_l0_mbs: HashMap::new(),
            pending_l0_partial: HashMap::new(),
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
            bloom_rgs,
            bloom_has_values,
            bypass_detectors: HashMap::new(),
            narrow_trigrams,
            max_chunk_id_seen: HashMap::new(),
            rg0_dict_codes: HashMap::new(),
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
        let rg_size = self.options.row_group_size;
        let is_l0_fast = self.options.profile == WriteProfile::L0Fast;
        let is_l1_pack = !is_l0_fast && self.pack_builder.is_some();
        let mut offset = 0usize;
        while offset < total {
            if self.rg_buf_rows == 0 && total - offset >= rg_size {
                if is_l0_fast {
                    for (col_idx, col) in active_batch.columns().iter().enumerate() {
                        if self.logical_locality[col_idx] == LocalityGroup::ExternalPayload {
                            self.append_l0_payload_slice(col_idx, col, offset, rg_size)?;
                        }
                    }
                }
                self.flush_rg_columns(active_batch.columns(), offset, rg_size, None)?;
                offset += rg_size;
                continue;
            }
            let space = rg_size - self.rg_buf_rows;
            let take = (total - offset).min(space);
            for (col_idx, col) in active_batch.columns().iter().enumerate() {
                if is_l0_fast && self.logical_locality[col_idx] == LocalityGroup::ExternalPayload {
                    self.append_l0_payload_slice(col_idx, col, offset, take)?;
                } else if is_l1_pack
                    && self.logical_locality[col_idx] == LocalityGroup::ExternalPayload
                {
                    self.append_l1_payload_slice(col_idx, col, offset, take)?;
                } else {
                    let piece = if offset == 0 && take == total {
                        col.clone()
                    } else {
                        col.slice(offset, take)?
                    };
                    self.rg_buf_cols[col_idx].push(piece);
                }
            }
            self.rg_buf_rows += take;
            offset += take;

            if self.rg_buf_rows == rg_size {
                self.flush_row_group()?;
            }
        }

        Ok(())
    }

    fn append_l0_payload_slice(
        &mut self,
        cidx: usize,
        col: &Column,
        offset: usize,
        take: usize,
    ) -> Result<(), StatamError> {
        if take == 0 {
            return Ok(());
        }
        let mb_size = self.options.mini_block_size;

        if let Column::Const { value, .. } = col
            && value.is_null()
        {
            let mut pos = 0usize;
            {
                let partial = self.pending_l0_partial.entry(cidx).or_default();
                if !partial.is_empty() {
                    let fill = (mb_size - partial.len()).min(take);
                    partial.resize(partial.len() + fill, PayloadRefRow::Null);
                    pos += fill;
                    if partial.len() == mb_size {
                        let null_count_usize = partial
                            .iter()
                            .filter(|r| matches!(r, PayloadRefRow::Null))
                            .count();
                        let null_count = u16::try_from(null_count_usize).unwrap_or(u16::MAX);
                        let raw_body = if null_count_usize == mb_size {
                            Vec::new()
                        } else {
                            let mut rb = Vec::new();
                            encode_payload_refs_l0_direct(partial, &mut rb)?;
                            rb
                        };
                        self.pending_l0_mbs.entry(cidx).or_default().push((
                            mb_size as u16,
                            null_count,
                            raw_body,
                        ));
                        partial.clear();
                    }
                }
            }
            while take - pos >= mb_size {
                let mb_u16 = mb_size as u16;
                self.pending_l0_mbs
                    .entry(cidx)
                    .or_default()
                    .push((mb_u16, mb_u16, Vec::new()));
                pos += mb_size;
            }
            if pos < take {
                let partial = self.pending_l0_partial.entry(cidx).or_default();
                partial.resize(partial.len() + (take - pos), PayloadRefRow::Null);
            }
            return Ok(());
        }

        let fc = as_flat_cow(col)?;
        let FlatData::Utf8(u) = fc.data() else {
            return Err(StatamError::SchemaMismatch {
                reason: "ExternalPayload column must be DataType::Utf8",
            });
        };
        let col_nulls = fc.nulls();
        let mut pos = 0usize;

        let push_row_ref = |r: usize,
                            local: &mut LocalL0PayloadBuilder,
                            dst: &mut Vec<PayloadRefRow>|
         -> Result<(), StatamError> {
            if col_nulls.is_some_and(|bm| !bm.get(r)) {
                dst.push(PayloadRefRow::Null);
                return Ok(());
            }
            let bytes = u.get(r).unwrap_or("").as_bytes();
            if bytes.len() <= INLINE_PAYLOAD_THRESHOLD {
                dst.push(PayloadRefRow::Inline(bytes.to_vec()));
            } else {
                let mut refs = Vec::new();
                for slice in bytes.chunks(MAX_FRAME_RAW_BYTES) {
                    let cid = local.add_chunk(slice)?;
                    refs.push((L0_LOCAL_PACK_IDX, cid));
                }
                dst.push(PayloadRefRow::Chunks(refs));
            }
            Ok(())
        };

        let has_partial = self
            .pending_l0_partial
            .get(&cidx)
            .is_some_and(|p| !p.is_empty());
        if has_partial {
            let mut partial = self.pending_l0_partial.remove(&cidx).unwrap_or_default();
            let fill = (mb_size - partial.len()).min(take);
            for r in offset..offset + fill {
                push_row_ref(r, &mut self.pending_l0_local, &mut partial)?;
            }
            pos += fill;
            if partial.len() == mb_size {
                let null_count_usize = partial
                    .iter()
                    .filter(|r| matches!(r, PayloadRefRow::Null))
                    .count();
                let null_count = u16::try_from(null_count_usize).unwrap_or(u16::MAX);
                let raw_body = if null_count_usize == mb_size {
                    Vec::new()
                } else {
                    let mut rb = Vec::new();
                    encode_payload_refs_l0_direct(&partial, &mut rb)?;
                    rb
                };
                self.pending_l0_mbs.entry(cidx).or_default().push((
                    mb_size as u16,
                    null_count,
                    raw_body,
                ));
                partial.clear();
            }
            if !partial.is_empty() {
                self.pending_l0_partial.insert(cidx, partial);
            }
        }

        while take - pos >= mb_size {
            let mut raw_body = Vec::new();
            let null_count = encode_payload_refs_l0_from_utf8(
                u,
                col_nulls,
                offset + pos,
                mb_size,
                INLINE_PAYLOAD_THRESHOLD,
                L0_LOCAL_PACK_IDX,
                |slice| self.pending_l0_local.add_chunk(slice),
                &mut raw_body,
            )?;
            self.pending_l0_mbs.entry(cidx).or_default().push((
                mb_size as u16,
                null_count,
                raw_body,
            ));
            pos += mb_size;
        }

        if pos < take {
            let mut partial = self.pending_l0_partial.remove(&cidx).unwrap_or_default();
            for r in offset + pos..offset + take {
                push_row_ref(r, &mut self.pending_l0_local, &mut partial)?;
            }
            self.pending_l0_partial.insert(cidx, partial);
        }

        Ok(())
    }

    fn append_l1_payload_slice(
        &mut self,
        cidx: usize,
        col: &Column,
        offset: usize,
        take: usize,
    ) -> Result<(), StatamError> {
        if take == 0 {
            return Ok(());
        }
        let mb_size = self.options.mini_block_size;

        if let Column::Const { value, .. } = col
            && value.is_null()
        {
            let mut pos = 0usize;
            {
                let partial = self.pending_l0_partial.entry(cidx).or_default();
                if !partial.is_empty() {
                    let fill = (mb_size - partial.len()).min(take);
                    partial.resize(partial.len() + fill, PayloadRefRow::Null);
                    pos += fill;
                    if partial.len() == mb_size {
                        let null_count_usize = partial
                            .iter()
                            .filter(|r| matches!(r, PayloadRefRow::Null))
                            .count();
                        let null_count = u16::try_from(null_count_usize).unwrap_or(u16::MAX);
                        let raw_body = if null_count_usize == mb_size {
                            Vec::new()
                        } else {
                            let mut rb = Vec::new();
                            encode_payload_refs(partial, &mut rb)?;
                            rb
                        };
                        self.pending_l0_mbs.entry(cidx).or_default().push((
                            mb_size as u16,
                            null_count,
                            raw_body,
                        ));
                        partial.clear();
                    }
                }
            }
            while take - pos >= mb_size {
                let mb_u16 = mb_size as u16;
                self.pending_l0_mbs
                    .entry(cidx)
                    .or_default()
                    .push((mb_u16, mb_u16, Vec::new()));
                pos += mb_size;
            }
            if pos < take {
                let partial = self.pending_l0_partial.entry(cidx).or_default();
                partial.resize(partial.len() + (take - pos), PayloadRefRow::Null);
            }
            return Ok(());
        }

        let fc = as_flat_cow(col)?;
        let mut pos = 0usize;
        let has_partial = self
            .pending_l0_partial
            .get(&cidx)
            .is_some_and(|p| !p.is_empty());
        if has_partial {
            let mut partial = self.pending_l0_partial.remove(&cidx).unwrap_or_default();
            let fill = (mb_size - partial.len()).min(take);
            let mut dummy_l0 = LocalL0PayloadBuilder::new(self.options.zstd_level);
            let rows = self.build_payload_ref_rows(cidx, &fc, offset, fill, &mut dummy_l0)?;
            partial.extend(rows);
            pos += fill;
            if partial.len() == mb_size {
                let null_count_usize = partial
                    .iter()
                    .filter(|r| matches!(r, PayloadRefRow::Null))
                    .count();
                let null_count = u16::try_from(null_count_usize).unwrap_or(u16::MAX);
                let raw_body = if null_count_usize == mb_size {
                    Vec::new()
                } else {
                    let mut rb = Vec::new();
                    encode_payload_refs(&partial, &mut rb)?;
                    rb
                };
                self.pending_l0_mbs.entry(cidx).or_default().push((
                    mb_size as u16,
                    null_count,
                    raw_body,
                ));
            } else {
                self.pending_l0_partial.insert(cidx, partial);
            }
        }

        while take - pos >= mb_size {
            let mut dummy_l0 = LocalL0PayloadBuilder::new(self.options.zstd_level);
            let refs =
                self.build_payload_ref_rows(cidx, &fc, offset + pos, mb_size, &mut dummy_l0)?;
            let null_count_usize = refs
                .iter()
                .filter(|r| matches!(r, PayloadRefRow::Null))
                .count();
            let null_count = u16::try_from(null_count_usize).unwrap_or(u16::MAX);
            let raw_body = if null_count_usize == mb_size {
                Vec::new()
            } else {
                let mut rb = Vec::new();
                encode_payload_refs(&refs, &mut rb)?;
                rb
            };
            self.pending_l0_mbs.entry(cidx).or_default().push((
                mb_size as u16,
                null_count,
                raw_body,
            ));
            pos += mb_size;
        }

        if pos < take {
            let mut dummy_l0 = LocalL0PayloadBuilder::new(self.options.zstd_level);
            let refs =
                self.build_payload_ref_rows(cidx, &fc, offset + pos, take - pos, &mut dummy_l0)?;
            let mut partial = self.pending_l0_partial.remove(&cidx).unwrap_or_default();
            partial.extend(refs);
            self.pending_l0_partial.insert(cidx, partial);
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
        if prechunked.is_empty() {
            return self.write_batch(batch);
        }

        if self.rg_buf_rows > 0 {
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

        let mut rg_dir_bytes = Vec::with_capacity(rg_dir_bytes_len);
        for rg in &self.rg_entries {
            rg_dir_bytes.extend_from_slice(&rg.encode());
        }
        let mut block_dir_bytes = Vec::with_capacity(block_dir_bytes_len);
        for col_blocks in &self.phys_blocks {
            for blk in col_blocks {
                block_dir_bytes.extend_from_slice(&blk.encode());
            }
        }

        // Measure compressed directory baseline (`schema + col_dir + rg_dir + block_dir`)
        // so normal files fit cleanly in <= 128 KiB (`DEFAULT_TAIL_HINT`) while small dicts/blooms
        // stay hot whenever compressed Hot Tail remains <= 120 KiB (`HOT_TAIL_SOFT_BUDGET_BYTES`),
        // and large files still keep primary bloom hot up to 1 MiB (`HOT_TAIL_HARD_BUDGET_BYTES`).
        let base_dirs_len =
            schema_bytes.len() + col_dir_bytes_len + rg_dir_bytes.len() + block_dir_bytes.len();
        let base_compressed_len = if self.options.compress_hot_tail_lz4 {
            let mut base_raw = Vec::with_capacity(base_dirs_len);
            base_raw.extend_from_slice(&schema_bytes);
            for cd in &col_dirs {
                base_raw.extend_from_slice(&cd.encode());
            }
            base_raw.extend_from_slice(&rg_dir_bytes);
            base_raw.extend_from_slice(&block_dir_bytes);
            lz4_flex::block::compress(&base_raw)
                .len()
                .min(base_dirs_len)
        } else {
            base_dirs_len
        };

        // Prebuild narrow trigrams, FileDict encodings, secondary blooms, and primary hot bloom
        // without cloning bloom hash vectors.
        let mut trigram_items: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut trigram_indices: Vec<usize> = self.narrow_trigram_col_set.iter().copied().collect();
        trigram_indices.sort_unstable();
        for cidx in trigram_indices {
            if !self.phys_col_meta[cidx].has_non_dict_data_block {
                continue;
            }
            if let Some(filters) = self.narrow_trigrams.get(&cidx)
                && let Some(sec_bytes) = encode_narrow_trigram_section(filters)?
            {
                trigram_items.push((cidx, sec_bytes));
            }
        }
        let has_any_trigram = !trigram_items.is_empty();

        let mut dict_items: Vec<(usize, Vec<u8>, bool)> = Vec::new();
        for (pidx, state) in self.phys_col_meta.iter().enumerate() {
            if !state.used_dict_encoding {
                continue;
            }
            let Some(dict) = &state.dict else {
                continue;
            };
            let dict_bytes = dict.encode(dict_codec, zstd_level)?;
            dict_items.push((pidx, dict_bytes, false));
        }

        let mut sec_blooms: Vec<(usize, Vec<u8>, bool)> = Vec::new();
        let mut bloom_indices: Vec<usize> = self.bloom_col_set.iter().copied().collect();
        bloom_indices.sort_unstable();
        for cidx in bloom_indices {
            if Some(cidx) == self.hot_bloom_col_idx {
                continue;
            }
            if let Some(bloom_bytes) = self.take_and_build_bloom_for_col(cidx)? {
                sec_blooms.push((cidx, bloom_bytes, false));
            }
        }
        // Keep smaller secondary blooms hot first; largest blooms spill first under tail budget.
        sec_blooms.sort_unstable_by_key(|(cidx, bytes, _)| (bytes.len(), *cidx));

        let max_lazy_overhead = (trigram_items.len() + dict_items.len() + sec_blooms.len() + 1)
            .saturating_mul(LAZY_INDEX_ENTRY_LEN);
        let fixed_overhead = POSTSCRIPT_LEN
            .saturating_add(base_compressed_len)
            .saturating_add(max_lazy_overhead)
            .saturating_add(256);
        let avail_for_primary = HOT_TAIL_HARD_BUDGET_BYTES.saturating_sub(fixed_overhead);

        let mut primary_bloom: Option<(usize, Vec<u8>, bool)> =
            if let Some(hot_idx) = self.hot_bloom_col_idx {
                self.take_and_build_primary_bloom(hot_idx, avail_for_primary)?
                    .map(|(bytes, fits_hot)| (hot_idx, bytes, fits_hot))
            } else {
                None
            };

        // Decide initial Hot Tail placement against the 120 KiB compressed soft budget.
        let mut est_stored = POSTSCRIPT_LEN
            .saturating_add(base_compressed_len)
            .saturating_add(trigram_items.len().saturating_mul(LAZY_INDEX_ENTRY_LEN));
        if let Some((_, ref bytes, in_hot)) = primary_bloom {
            if in_hot {
                est_stored = est_stored.saturating_add(bytes.len());
            } else {
                est_stored = est_stored.saturating_add(LAZY_INDEX_ENTRY_LEN);
            }
        }
        let mut remaining_lazy_entries = dict_items.len() + sec_blooms.len();
        for (_, bytes, in_hot) in &mut dict_items {
            remaining_lazy_entries = remaining_lazy_entries.saturating_sub(1);
            let reserve = remaining_lazy_entries * LAZY_INDEX_ENTRY_LEN;
            if bytes.len() <= HOT_DICT_MAX_BYTES
                && est_stored
                    .saturating_add(bytes.len())
                    .saturating_add(reserve)
                    <= HOT_TAIL_SOFT_BUDGET_BYTES
            {
                *in_hot = true;
                est_stored = est_stored.saturating_add(bytes.len());
            } else {
                *in_hot = false;
                est_stored = est_stored.saturating_add(LAZY_INDEX_ENTRY_LEN);
            }
        }
        for (_, bytes, in_hot) in &mut sec_blooms {
            remaining_lazy_entries = remaining_lazy_entries.saturating_sub(1);
            let reserve = remaining_lazy_entries * LAZY_INDEX_ENTRY_LEN;
            if est_stored
                .saturating_add(bytes.len())
                .saturating_add(reserve)
                <= HOT_TAIL_SOFT_BUDGET_BYTES
            {
                *in_hot = true;
                est_stored = est_stored.saturating_add(bytes.len());
            } else {
                *in_hot = false;
                est_stored = est_stored.saturating_add(LAZY_INDEX_ENTRY_LEN);
            }
        }

        let mut assembled = assemble_hot_tail_and_lazy(
            &col_dirs,
            &schema_bytes,
            &rg_dir_bytes,
            &block_dir_bytes,
            self.cur_data_offset,
            self.options.compress_hot_tail_lz4,
            primary_bloom.as_ref(),
            &dict_items,
            &sec_blooms,
            &trigram_items,
        )?;

        // Spill secondary blooms, then small hot dicts if compressed Hot Tail exceeds 128 KiB
        // (`DEFAULT_TAIL_HINT`), and spill primary bloom only if it exceeds 1 MiB (`HOT_TAIL_HARD_BUDGET_BYTES`).
        while assembled.hot_tail_len as usize > DEFAULT_TAIL_HINT as usize {
            if let Some(item) = sec_blooms.iter_mut().rev().find(|x| x.2) {
                item.2 = false;
            } else if let Some(item) = dict_items.iter_mut().rev().find(|x| x.2) {
                item.2 = false;
            } else if assembled.hot_tail_len as usize > HOT_TAIL_HARD_BUDGET_BYTES
                && let Some((_, _, ref mut in_hot)) = primary_bloom
                && *in_hot
            {
                *in_hot = false;
            } else {
                break;
            }
            assembled = assemble_hot_tail_and_lazy(
                &col_dirs,
                &schema_bytes,
                &rg_dir_bytes,
                &block_dir_bytes,
                self.cur_data_offset,
                self.options.compress_hot_tail_lz4,
                primary_bloom.as_ref(),
                &dict_items,
                &sec_blooms,
                &trigram_items,
            )?;
        }

        let hot_tail_raw_len = assembled.hot_tail_raw_len;
        let hot_tail_len = assembled.hot_tail_len;
        let hot_tail_stored = assembled.hot_tail_stored;
        let is_lz4 = assembled.is_lz4;
        let lazy_data_buf = assembled.lazy_data_buf;
        let cur_file_off = assembled.cur_file_off;
        let schema_off = 0u32;
        let schema_len = assembled.schema_len;
        let col_dir_off = assembled.col_dir_off;
        let rg_dir_off = assembled.rg_dir_off;
        let block_dir_off = assembled.block_dir_off;
        let hot_bloom_off = assembled.hot_bloom_off;
        let hot_bloom_len = assembled.hot_bloom_len;
        let hot_dict_off = assembled.hot_dict_off;
        let hot_dict_len = assembled.hot_dict_len;
        let lazy_dir_off = assembled.lazy_dir_off;
        let lazy_dir_len = assembled.lazy_dir_len;

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

        // Concatenate each logical column's buffered slices for this RG.
        let mut rg_cols = Vec::with_capacity(self.schema.len());
        for (cidx, pieces) in self.rg_buf_cols.iter_mut().enumerate() {
            if pieces.is_empty()
                && (self.options.profile == WriteProfile::L0Fast || self.pack_builder.is_some())
                && self.logical_locality[cidx] == LocalityGroup::ExternalPayload
            {
                rg_cols.push(Column::Flat(FlatColumn::empty(&DataType::Utf8)));
            } else {
                let dt = &self.schema.fields()[cidx].data_type;
                let combined = concat_rg_column(dt, pieces, rg_rows)?;
                pieces.clear();
                rg_cols.push(combined);
            }
        }
        self.rg_buf_rows = 0;

        self.flush_rg_columns(&rg_cols, 0, rg_rows, prechunked)
    }

    fn flush_rg_columns(
        &mut self,
        rg_cols: &[Column],
        base_row_off: usize,
        rg_rows: usize,
        prechunked: Option<PrechunkedRefs<'_>>,
    ) -> Result<(), StatamError> {
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

        // Gate RG-level worker thread spawning on large RGs so tiny/1-row flushes stay 1T.
        let rg_threads = if rg_rows >= MIN_PARALLEL_RG_ROWS {
            self.options.write_threads
        } else {
            1
        };

        // RG0 initialization: promote ShreddedMap keys (L1Compact) & seed FileDicts
        // BEFORE building RG0 blooms so dict-encoded columns hash only unique dict entries.
        let is_rg0 = !self.rg0_initialized;
        if is_rg0 {
            if rg_threads > 1 {
                self.init_rg0_parallel(rg_cols, base_row_off, rg_rows)?;
            } else {
                self.init_rg0(rg_cols, base_row_off, rg_rows)?;
            }
            self.rg0_initialized = true;
        }

        // Build per-RG SBBF bloom filters immediately (never accumulate file-wide Vec<u64>).
        let mut bloom_indices: Vec<usize> = self.bloom_col_set.iter().copied().collect();
        bloom_indices.sort_unstable();
        if rg_threads > 1 && bloom_indices.len() > 1 {
            let profile = self.options.profile;
            let schema_ref = &self.schema;
            let phys_meta_ref = &self.phys_col_meta;
            let results = parallel_map_in_order(bloom_indices, rg_threads, |cidx| {
                let col_name = schema_ref.fields()[cidx].name.as_str();
                let rg0_dict = if is_rg0 {
                    phys_meta_ref[cidx].dict.as_deref()
                } else {
                    None
                };
                let (sbbf, had_any) = build_rg_sbbf(
                    &rg_cols[cidx],
                    base_row_off,
                    rg_rows,
                    profile,
                    col_name,
                    rg0_dict,
                )?;
                Ok((cidx, sbbf, had_any))
            })?;
            for (cidx, sbbf, had_any) in results {
                if had_any {
                    self.bloom_has_values.insert(cidx, true);
                }
                if let Some(rgs) = self.bloom_rgs.get_mut(&cidx) {
                    rgs.push(sbbf);
                }
            }
        } else {
            for cidx in bloom_indices {
                let col_name = self.schema.fields()[cidx].name.as_str();
                let rg0_dict = if is_rg0 {
                    self.phys_col_meta[cidx].dict.as_deref()
                } else {
                    None
                };
                let (sbbf, had_any) = build_rg_sbbf(
                    &rg_cols[cidx],
                    base_row_off,
                    rg_rows,
                    self.options.profile,
                    col_name,
                    rg0_dict,
                )?;
                if had_any {
                    self.bloom_has_values.insert(cidx, true);
                }
                if let Some(rgs) = self.bloom_rgs.get_mut(&cidx) {
                    rgs.push(sbbf);
                }
            }
        }

        let zstd_level = self.options.zstd_level;
        let preferred_codec = match self.options.profile {
            WriteProfile::L0Fast => Codec::Lz4,
            WriteProfile::L1Compact => default_l1_codec(),
        };

        let mut core_buf = Vec::new();
        let mut wide_buf = Vec::new();
        let mut l0_local_builder = std::mem::replace(
            &mut self.pending_l0_local,
            LocalL0PayloadBuilder::new_with_threads(zstd_level, self.options.write_threads),
        );

        let core_offset = self.cur_data_offset;
        let mb_size = self.options.mini_block_size;
        let schema = Arc::clone(&self.schema);
        let fields = schema.fields();

        let (core_len, wide_offset) = if rg_threads > 1 {
            self.encode_rg_columns_parallel(
                rg_cols,
                base_row_off,
                rg_rows,
                first_row_u32,
                mb_size,
                core_offset,
                preferred_codec,
                zstd_level,
                prechunked,
                &mut core_buf,
                &mut wide_buf,
                &mut l0_local_builder,
            )?
        } else {
            // Tier 1A: Small Hot Analytics Core filter/agg columns written FIRST at `core_offset`.
            for (cidx, rg_col) in rg_cols.iter().enumerate() {
                if self.logical_locality[cidx] != LocalityGroup::Core {
                    continue;
                }
                if !HOT_ANALYTICS_TIER1_SMALL_COLS.contains(&fields[cidx].name.as_str()) {
                    continue;
                }
                self.encode_rg_scalar_or_nested_col(
                    cidx,
                    rg_col,
                    base_row_off,
                    rg_rows,
                    first_row_u32,
                    mb_size,
                    core_offset,
                    zstd_level,
                    &mut core_buf,
                )?;
            }

            // Tier 1B: Hot timestamp columns immediately before `metadata` shredded sub-columns.
            for (cidx, rg_col) in rg_cols.iter().enumerate() {
                if self.logical_locality[cidx] != LocalityGroup::Core {
                    continue;
                }
                if !HOT_ANALYTICS_TIER1_TIME_COLS.contains(&fields[cidx].name.as_str()) {
                    continue;
                }
                self.encode_rg_scalar_or_nested_col(
                    cidx,
                    rg_col,
                    base_row_off,
                    rg_rows,
                    first_row_u32,
                    mb_size,
                    core_offset,
                    zstd_level,
                    &mut core_buf,
                )?;
            }

            // Tier 2 (Phase B1): `metadata` shredded sub-columns right inside block #0 (< 1 MiB).
            let mut deferred_wide_residuals: Vec<(usize, Vec<EncodedMiniBlock>, Vec<u16>)> =
                Vec::new();
            self.encode_shredded_maps_group(
                rg_cols,
                base_row_off,
                rg_rows,
                first_row_u32,
                mb_size,
                core_offset,
                zstd_level,
                true,
                &mut core_buf,
                &mut deferred_wide_residuals,
            )?;

            // Tier 3a: `trace_id` immediately after `metadata.*`.
            for (cidx, rg_col) in rg_cols.iter().enumerate() {
                if self.logical_locality[cidx] != LocalityGroup::Core {
                    continue;
                }
                if fields[cidx].name != "trace_id" {
                    continue;
                }
                self.encode_rg_scalar_or_nested_col(
                    cidx,
                    rg_col,
                    base_row_off,
                    rg_rows,
                    first_row_u32,
                    mb_size,
                    core_offset,
                    zstd_level,
                    &mut core_buf,
                )?;
            }

            // Tier 3b: `cost_total` & `total_tokens` after `trace_id` and before `span_id`.
            for (cidx, rg_col) in rg_cols.iter().enumerate() {
                if self.logical_locality[cidx] != LocalityGroup::Core {
                    continue;
                }
                if !HOT_ANALYTICS_POST_TRACE_COLS.contains(&fields[cidx].name.as_str()) {
                    continue;
                }
                self.encode_rg_scalar_or_nested_col(
                    cidx,
                    rg_col,
                    base_row_off,
                    rg_rows,
                    first_row_u32,
                    mb_size,
                    core_offset,
                    zstd_level,
                    &mut core_buf,
                )?;
            }

            // Tier 3c: Remaining ID Core columns (`span_id`, `score_id`, `parent_span_id`, ...).
            for (cidx, rg_col) in rg_cols.iter().enumerate() {
                if self.logical_locality[cidx] != LocalityGroup::Core {
                    continue;
                }
                let name = fields[cidx].name.as_str();
                if name == "trace_id" || !ID_CORE_COLS.contains(&name) {
                    continue;
                }
                self.encode_rg_scalar_or_nested_col(
                    cidx,
                    rg_col,
                    base_row_off,
                    rg_rows,
                    first_row_u32,
                    mb_size,
                    core_offset,
                    zstd_level,
                    &mut core_buf,
                )?;
            }

            // Tier 4: Tier-2 Hot Analytics Core columns (`duration_ns`, `environment`, `provider`, etc.).
            for (cidx, rg_col) in rg_cols.iter().enumerate() {
                if self.logical_locality[cidx] != LocalityGroup::Core {
                    continue;
                }
                if !HOT_ANALYTICS_TIER2_COLS.contains(&fields[cidx].name.as_str()) {
                    continue;
                }
                self.encode_rg_scalar_or_nested_col(
                    cidx,
                    rg_col,
                    base_row_off,
                    rg_rows,
                    first_row_u32,
                    mb_size,
                    core_offset,
                    zstd_level,
                    &mut core_buf,
                )?;
            }

            // Tier 5 (Phase B2): Remaining shredded sub-columns (`attributes.*`, `resource_attributes.*`).
            self.encode_shredded_maps_group(
                rg_cols,
                base_row_off,
                rg_rows,
                first_row_u32,
                mb_size,
                core_offset,
                zstd_level,
                false,
                &mut core_buf,
                &mut deferred_wide_residuals,
            )?;

            // Tier 6: Remaining cold Core columns.
            for (cidx, rg_col) in rg_cols.iter().enumerate() {
                if self.logical_locality[cidx] != LocalityGroup::Core {
                    continue;
                }
                let name = fields[cidx].name.as_str();
                if HOT_ANALYTICS_TIER1_SMALL_COLS.contains(&name)
                    || HOT_ANALYTICS_TIER1_TIME_COLS.contains(&name)
                    || HOT_ANALYTICS_POST_TRACE_COLS.contains(&name)
                    || HOT_ANALYTICS_TIER2_COLS.contains(&name)
                    || ID_CORE_COLS.contains(&name)
                {
                    continue;
                }
                self.encode_rg_scalar_or_nested_col(
                    cidx,
                    rg_col,
                    base_row_off,
                    rg_rows,
                    first_row_u32,
                    mb_size,
                    core_offset,
                    zstd_level,
                    &mut core_buf,
                )?;
            }

            let core_len =
                u32::try_from(core_buf.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "row group core_len overflows u32",
                })?;
            let wide_offset = core_offset.checked_add(u64::from(core_len)).ok_or(
                StatamError::BoundsExceeded {
                    reason: "wide_offset overflow",
                },
            )?;

            // Phase C: Write WideRow blocks (regular WideRow cols, deferred shredded residuals, and PayloadRefs).
            for (logical_col, mb_residuals, mb_row_counts) in deferred_wide_residuals {
                let mut cur_first = first_row_u32;
                for (enc, rc) in mb_residuals.iter().zip(mb_row_counts.iter()) {
                    let entry = enc.write_framed_entry(
                        cur_first,
                        *rc,
                        wide_offset,
                        zstd_level,
                        &mut wide_buf,
                    )?;
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
                            base_row_off,
                            rg_rows,
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
                            base_row_off,
                            rg_rows,
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
            (core_len, wide_offset)
        };

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
        self.rg0_dict_codes.clear();
        self.rg_data_blobs.push(rg_blob);

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_rg_columns_parallel(
        &mut self,
        rg_cols: &[Column],
        base_row_off: usize,
        rg_rows: usize,
        first_row_u32: u32,
        mb_size: usize,
        core_offset: u64,
        preferred_codec: Codec,
        zstd_level: i32,
        prechunked: Option<PrechunkedRefs<'_>>,
        core_buf: &mut Vec<u8>,
        wide_buf: &mut Vec<u8>,
        l0_local_builder: &mut LocalL0PayloadBuilder,
    ) -> Result<(u32, u64), StatamError> {
        let num_logical = self.schema.len();
        let num_shredded = self.shredded_maps.len();
        let mut rg0_codes_map = std::mem::take(&mut self.rg0_dict_codes);

        enum RgEncodeTask {
            ShreddedMap(usize),
            ScalarOrNested {
                cidx: usize,
                precomputed_rg0: Option<(Vec<u32>, Option<cetra_column::Bitmap>)>,
            },
        }

        enum RgEncodeResult {
            ShreddedMap(usize, EncodedShreddedMap),
            ScalarOrNested(usize, EncodedScalarCol),
        }

        // Schedule heaviest tasks (shredded maps + WideRow cols) before small Core cols.
        let mut tasks = Vec::with_capacity(num_shredded + num_logical);
        for s_idx in 0..num_shredded {
            tasks.push(RgEncodeTask::ShreddedMap(s_idx));
        }
        for cidx in 0..num_logical {
            if self.logical_locality[cidx] == LocalityGroup::WideRow
                && !self.logical_to_shredded.contains_key(&cidx)
            {
                tasks.push(RgEncodeTask::ScalarOrNested {
                    cidx,
                    precomputed_rg0: rg0_codes_map.remove(&cidx),
                });
            }
        }
        for cidx in 0..num_logical {
            if self.logical_locality[cidx] == LocalityGroup::Core {
                tasks.push(RgEncodeTask::ScalarOrNested {
                    cidx,
                    precomputed_rg0: rg0_codes_map.remove(&cidx),
                });
            }
        }

        let schema_ref = &self.schema;
        let logical_locality = &self.logical_locality;
        let phys_col_meta = &self.phys_col_meta;
        let narrow_trigram_col_set = &self.narrow_trigram_col_set;
        let shredded_maps = &self.shredded_maps;
        let profile = self.options.profile;

        let encoded =
            parallel_map_in_order(tasks, self.options.write_threads, |task| match task {
                RgEncodeTask::ShreddedMap(s_idx) => {
                    let sm = &shredded_maps[s_idx];
                    let logical_col = sm.logical_col_idx as usize;
                    let is_metadata = schema_ref.fields()[logical_col].name == "metadata";
                    let res = encode_shredded_map_to_local(
                        sm,
                        is_metadata,
                        &rg_cols[logical_col],
                        base_row_off,
                        rg_rows,
                        first_row_u32,
                        mb_size,
                        profile,
                        zstd_level,
                    )?;
                    Ok(RgEncodeResult::ShreddedMap(s_idx, res))
                }
                RgEncodeTask::ScalarOrNested {
                    cidx,
                    precomputed_rg0,
                } => {
                    let dt = &schema_ref.fields()[cidx].data_type;
                    let is_wide_row = logical_locality[cidx] == LocalityGroup::WideRow;
                    let dict = phys_col_meta[cidx].dict.as_deref();
                    let track_trigram = narrow_trigram_col_set.contains(&cidx);
                    let mut local_buf = Vec::new();
                    let mut blocks = Vec::new();
                    let mut local_tris = Vec::new();
                    let (used_dict_encoding, has_non_dict_data_block, null_count_total) =
                        encode_scalar_or_nested_col_into(
                            &rg_cols[cidx],
                            dt,
                            profile,
                            is_wide_row,
                            dict,
                            precomputed_rg0,
                            base_row_off,
                            rg_rows,
                            first_row_u32,
                            mb_size,
                            0,
                            zstd_level,
                            &mut local_buf,
                            &mut blocks,
                            track_trigram.then_some(&mut local_tris),
                        )?;
                    Ok(RgEncodeResult::ScalarOrNested(
                        cidx,
                        EncodedScalarCol {
                            local_buf,
                            blocks,
                            used_dict_encoding,
                            has_non_dict_data_block,
                            null_count_total,
                            narrow_trigrams: track_trigram.then_some(local_tris),
                        },
                    ))
                }
            })?;

        let mut col_results: Vec<Option<EncodedScalarCol>> =
            (0..num_logical).map(|_| None).collect();
        let mut shred_results: Vec<Option<EncodedShreddedMap>> =
            (0..num_shredded).map(|_| None).collect();
        for item in encoded {
            match item {
                RgEncodeResult::ScalarOrNested(cidx, res) => {
                    col_results[cidx] = Some(res);
                }
                RgEncodeResult::ShreddedMap(s_idx, res) => {
                    shred_results[s_idx] = Some(res);
                }
            }
        }

        let commit_core_col = |cidx: usize,
                               col_results: &mut [Option<EncodedScalarCol>],
                               phys_col_meta: &mut [PhysColWriteState],
                               phys_blocks: &mut [Vec<BlockEntry>],
                               narrow_trigrams: &mut HashMap<usize, Vec<Option<Vec<u8>>>>,
                               core_buf: &mut Vec<u8>|
         -> Result<(), StatamError> {
            if let Some(res) = col_results[cidx].take() {
                commit_scalar_col_result(
                    phys_col_meta,
                    phys_blocks,
                    narrow_trigrams,
                    cidx,
                    res,
                    core_offset,
                    core_buf,
                )?;
            }
            Ok(())
        };

        let schema = Arc::clone(&self.schema);
        let fields = schema.fields();

        // Tier 1A
        for (cidx, field) in fields.iter().enumerate() {
            if self.logical_locality[cidx] == LocalityGroup::Core
                && HOT_ANALYTICS_TIER1_SMALL_COLS.contains(&field.name.as_str())
            {
                commit_core_col(
                    cidx,
                    &mut col_results,
                    &mut self.phys_col_meta,
                    &mut self.phys_blocks,
                    &mut self.narrow_trigrams,
                    core_buf,
                )?;
            }
        }
        // Tier 1B
        for (cidx, field) in fields.iter().enumerate() {
            if self.logical_locality[cidx] == LocalityGroup::Core
                && HOT_ANALYTICS_TIER1_TIME_COLS.contains(&field.name.as_str())
            {
                commit_core_col(
                    cidx,
                    &mut col_results,
                    &mut self.phys_col_meta,
                    &mut self.phys_blocks,
                    &mut self.narrow_trigrams,
                    core_buf,
                )?;
            }
        }
        // Tier 2 (Phase B1): `metadata` shredded sub-columns
        for (sm, slot) in self.shredded_maps.iter_mut().zip(shred_results.iter_mut()) {
            if let Some(shred) = slot.as_mut()
                && shred.is_metadata
            {
                commit_shredded_subcols(sm, &mut self.phys_blocks, shred, core_offset, core_buf)?;
            }
        }
        // Tier 3a: `trace_id`
        for (cidx, field) in fields.iter().enumerate() {
            if self.logical_locality[cidx] == LocalityGroup::Core && field.name == "trace_id" {
                commit_core_col(
                    cidx,
                    &mut col_results,
                    &mut self.phys_col_meta,
                    &mut self.phys_blocks,
                    &mut self.narrow_trigrams,
                    core_buf,
                )?;
            }
        }
        // Tier 3b: `cost_total` & `total_tokens`
        for (cidx, field) in fields.iter().enumerate() {
            if self.logical_locality[cidx] == LocalityGroup::Core
                && HOT_ANALYTICS_POST_TRACE_COLS.contains(&field.name.as_str())
            {
                commit_core_col(
                    cidx,
                    &mut col_results,
                    &mut self.phys_col_meta,
                    &mut self.phys_blocks,
                    &mut self.narrow_trigrams,
                    core_buf,
                )?;
            }
        }
        // Tier 3c: Remaining ID Core columns
        for (cidx, field) in fields.iter().enumerate() {
            let name = field.name.as_str();
            if self.logical_locality[cidx] == LocalityGroup::Core
                && name != "trace_id"
                && ID_CORE_COLS.contains(&name)
            {
                commit_core_col(
                    cidx,
                    &mut col_results,
                    &mut self.phys_col_meta,
                    &mut self.phys_blocks,
                    &mut self.narrow_trigrams,
                    core_buf,
                )?;
            }
        }
        // Tier 4: Tier-2 Hot Analytics Core columns
        for (cidx, field) in fields.iter().enumerate() {
            if self.logical_locality[cidx] == LocalityGroup::Core
                && HOT_ANALYTICS_TIER2_COLS.contains(&field.name.as_str())
            {
                commit_core_col(
                    cidx,
                    &mut col_results,
                    &mut self.phys_col_meta,
                    &mut self.phys_blocks,
                    &mut self.narrow_trigrams,
                    core_buf,
                )?;
            }
        }
        // Tier 5 (Phase B2): Remaining shredded sub-columns
        for (sm, slot) in self.shredded_maps.iter_mut().zip(shred_results.iter_mut()) {
            if let Some(shred) = slot.as_mut()
                && !shred.is_metadata
            {
                commit_shredded_subcols(sm, &mut self.phys_blocks, shred, core_offset, core_buf)?;
            }
        }
        // Tier 6: Remaining cold Core columns
        for (cidx, field) in fields.iter().enumerate() {
            if self.logical_locality[cidx] != LocalityGroup::Core {
                continue;
            }
            let name = field.name.as_str();
            if HOT_ANALYTICS_TIER1_SMALL_COLS.contains(&name)
                || HOT_ANALYTICS_TIER1_TIME_COLS.contains(&name)
                || HOT_ANALYTICS_POST_TRACE_COLS.contains(&name)
                || HOT_ANALYTICS_TIER2_COLS.contains(&name)
                || ID_CORE_COLS.contains(&name)
            {
                continue;
            }
            commit_core_col(
                cidx,
                &mut col_results,
                &mut self.phys_col_meta,
                &mut self.phys_blocks,
                &mut self.narrow_trigrams,
                core_buf,
            )?;
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

        // Phase C: Deferred shredded map residuals (B1 then B2), then WideRow & ExternalPayload in cidx order.
        for slot in &mut shred_results {
            if slot.as_ref().is_some_and(|s| s.is_metadata)
                && let Some(shred) = slot.take()
            {
                self.phys_col_meta[shred.logical_col].null_count_total = self.phys_col_meta
                    [shred.logical_col]
                    .null_count_total
                    .saturating_add(shred.res_null_count_total);
                append_col_blocks(
                    wide_buf,
                    wide_offset,
                    &shred.local_wide_buf,
                    shred.residual_blocks,
                    &mut self.phys_blocks[shred.logical_col],
                )?;
            }
        }
        for slot in &mut shred_results {
            if let Some(shred) = slot.take() {
                self.phys_col_meta[shred.logical_col].null_count_total = self.phys_col_meta
                    [shred.logical_col]
                    .null_count_total
                    .saturating_add(shred.res_null_count_total);
                append_col_blocks(
                    wide_buf,
                    wide_offset,
                    &shred.local_wide_buf,
                    shred.residual_blocks,
                    &mut self.phys_blocks[shred.logical_col],
                )?;
            }
        }

        for (cidx, rg_col) in rg_cols.iter().enumerate() {
            match self.logical_locality[cidx] {
                LocalityGroup::Core => {}
                LocalityGroup::WideRow => {
                    if let Some(res) = col_results[cidx].take() {
                        commit_scalar_col_result(
                            &mut self.phys_col_meta,
                            &mut self.phys_blocks,
                            &mut self.narrow_trigrams,
                            cidx,
                            res,
                            wide_offset,
                            wide_buf,
                        )?;
                    }
                }
                LocalityGroup::ExternalPayload => {
                    self.encode_rg_payload_col(
                        cidx,
                        rg_col,
                        base_row_off,
                        rg_rows,
                        first_row_u32,
                        mb_size,
                        wide_offset,
                        preferred_codec,
                        zstd_level,
                        prechunked,
                        wide_buf,
                        l0_local_builder,
                    )?;
                }
            }
        }

        Ok((core_len, wide_offset))
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_shredded_maps_group(
        &mut self,
        rg_cols: &[Column],
        base_row_off: usize,
        rg_rows: usize,
        first_row_u32: u32,
        mb_size: usize,
        core_offset: u64,
        zstd_level: i32,
        want_metadata: bool,
        core_buf: &mut Vec<u8>,
        deferred_wide_residuals: &mut Vec<(usize, Vec<EncodedMiniBlock>, Vec<u16>)>,
    ) -> Result<(), StatamError> {
        let num_shredded = self.shredded_maps.len();
        for s_idx in 0..num_shredded {
            let logical_col = self.shredded_maps[s_idx].logical_col_idx as usize;
            let is_metadata = self.schema.fields()[logical_col].name == "metadata";
            if is_metadata != want_metadata {
                continue;
            }
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
                let abs_off = base_row_off + row_off;
                let sliced_owned;
                let sliced = if abs_off == 0 && mb_len == rg_cols[logical_col].len() {
                    &rg_cols[logical_col]
                } else {
                    sliced_owned = rg_cols[logical_col].slice(abs_off, mb_len)?;
                    &sliced_owned
                };
                let flat = as_flat_cow(sliced)?;
                let shredded = shred_map(&flat, &promoted_keys)?;

                for (k_idx, &sub_pidx) in sub_col_indices.iter().enumerate() {
                    self.shredded_maps[s_idx].keys[k_idx].residual_count =
                        self.shredded_maps[s_idx].keys[k_idx]
                            .residual_count
                            .saturating_add(shredded.residual_counts[k_idx]);

                    let (sub_codec, sub_raw) = encode_shredded_subcol_with_codec(
                        &shredded.sub_cols[k_idx],
                        self.options.profile,
                    )?;
                    let blk_off = core_offset + core_buf.len() as u64;
                    let info = write_framed_block(
                        Encoding::ShreddedMap,
                        &sub_raw,
                        sub_codec,
                        zstd_level,
                        core_buf,
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
        Ok(())
    }

    fn init_rg0(
        &mut self,
        rg_cols: &[Column],
        base_row_off: usize,
        rg_rows: usize,
    ) -> Result<(), StatamError> {
        let schema = Arc::clone(&self.schema);
        let fields = schema.fields();

        // 1. Shredded maps in L1Compact.
        if self.options.profile == WriteProfile::L1Compact {
            for (cidx, field) in fields.iter().enumerate() {
                if field.data_type != DataType::MapUtf8 {
                    continue;
                }
                let promoted = extract_rg0_shredded_keys(&rg_cols[cidx], base_row_off, rg_rows)?;
                if promoted.is_empty() {
                    continue;
                }
                self.register_shredded_map(cidx, promoted)?;
            }
        }

        // 2. Seed file-level dictionaries from RG0 on low-cardinality columns.
        for (cidx, field) in fields.iter().enumerate() {
            if !is_rg0_dict_candidate(field, self.logical_locality[cidx], self.options.profile) {
                continue;
            }
            if let Some((dict, codes, nulls)) = eval_rg0_dict_candidate(
                &rg_cols[cidx],
                field.name.as_str(),
                self.logical_locality[cidx],
                self.options.profile,
                base_row_off,
                rg_rows,
            )? {
                let dict_len = dict.len();
                self.phys_col_meta[cidx].dict = Some(Arc::new(dict));
                if dict_len >= 2 {
                    self.rg0_dict_codes.insert(cidx, (codes, nulls));
                }
            }
        }

        Ok(())
    }

    fn init_rg0_parallel(
        &mut self,
        rg_cols: &[Column],
        base_row_off: usize,
        rg_rows: usize,
    ) -> Result<(), StatamError> {
        enum Rg0InitTask {
            ShreddedMap(usize),
            DictCandidate(usize),
        }

        enum Rg0InitResult {
            ShreddedMap(usize, Vec<String>),
            DictCandidate(usize, Option<Rg0DictResult>),
        }

        let schema = Arc::clone(&self.schema);
        let fields = schema.fields();
        let mut tasks = Vec::new();
        if self.options.profile == WriteProfile::L1Compact {
            for (cidx, field) in fields.iter().enumerate() {
                if field.data_type == DataType::MapUtf8 {
                    tasks.push(Rg0InitTask::ShreddedMap(cidx));
                }
            }
        }
        for (cidx, field) in fields.iter().enumerate() {
            if is_rg0_dict_candidate(field, self.logical_locality[cidx], self.options.profile) {
                tasks.push(Rg0InitTask::DictCandidate(cidx));
            }
        }

        let profile = self.options.profile;
        let logical_locality = &self.logical_locality;
        let results =
            parallel_map_in_order(tasks, self.options.write_threads, |task| match task {
                Rg0InitTask::ShreddedMap(cidx) => {
                    let promoted =
                        extract_rg0_shredded_keys(&rg_cols[cidx], base_row_off, rg_rows)?;
                    Ok(Rg0InitResult::ShreddedMap(cidx, promoted))
                }
                Rg0InitTask::DictCandidate(cidx) => {
                    let res = eval_rg0_dict_candidate(
                        &rg_cols[cidx],
                        fields[cidx].name.as_str(),
                        logical_locality[cidx],
                        profile,
                        base_row_off,
                        rg_rows,
                    )?;
                    Ok(Rg0InitResult::DictCandidate(cidx, res))
                }
            })?;

        for res in results {
            match res {
                Rg0InitResult::ShreddedMap(cidx, promoted) => {
                    if !promoted.is_empty() {
                        self.register_shredded_map(cidx, promoted)?;
                    }
                }
                Rg0InitResult::DictCandidate(cidx, Some((dict, codes, nulls))) => {
                    let dict_len = dict.len();
                    self.phys_col_meta[cidx].dict = Some(Arc::new(dict));
                    if dict_len >= 2 {
                        self.rg0_dict_codes.insert(cidx, (codes, nulls));
                    }
                }
                Rg0InitResult::DictCandidate(_, None) => {}
            }
        }

        Ok(())
    }

    fn register_shredded_map(
        &mut self,
        cidx: usize,
        promoted: Vec<String>,
    ) -> Result<(), StatamError> {
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
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_rg_scalar_or_nested_col(
        &mut self,
        cidx: usize,
        col: &Column,
        base_row_off: usize,
        rg_rows: usize,
        first_row_u32: u32,
        mb_size: usize,
        base_offset: u64,
        zstd_level: i32,
        out_buf: &mut Vec<u8>,
    ) -> Result<(), StatamError> {
        let dt = self.schema.fields()[cidx].data_type.clone();
        let precomputed_rg0 = self.rg0_dict_codes.remove(&cidx);
        let dict_clone = self.phys_col_meta[cidx].dict.as_ref().map(Arc::clone);
        let is_wide_row = self.logical_locality[cidx] == LocalityGroup::WideRow;
        let out_trigrams = self.narrow_trigrams.get_mut(&cidx);
        let (used_dict, has_non_dict, null_total) = encode_scalar_or_nested_col_into(
            col,
            &dt,
            self.options.profile,
            is_wide_row,
            dict_clone.as_deref(),
            precomputed_rg0,
            base_row_off,
            rg_rows,
            first_row_u32,
            mb_size,
            base_offset,
            zstd_level,
            out_buf,
            &mut self.phys_blocks[cidx],
            out_trigrams,
        )?;
        if used_dict {
            self.phys_col_meta[cidx].used_dict_encoding = true;
        }
        if has_non_dict {
            self.phys_col_meta[cidx].has_non_dict_data_block = true;
        }
        self.phys_col_meta[cidx].null_count_total = self.phys_col_meta[cidx]
            .null_count_total
            .saturating_add(null_total);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_rg_payload_col(
        &mut self,
        cidx: usize,
        col: &Column,
        base_row_off: usize,
        rg_rows: usize,
        first_row_u32: u32,
        mb_size: usize,
        base_offset: u64,
        preferred_codec: Codec,
        zstd_level: i32,
        prechunked: Option<PrechunkedRefs<'_>>,
        wide_buf: &mut Vec<u8>,
        l0_local: &mut LocalL0PayloadBuilder,
    ) -> Result<(), StatamError> {
        if prechunked.is_none()
            && (self.pending_l0_mbs.contains_key(&cidx)
                || self.pending_l0_partial.contains_key(&cidx))
        {
            let mut mbs = self.pending_l0_mbs.remove(&cidx).unwrap_or_default();
            if let Some(partial) = self.pending_l0_partial.remove(&cidx)
                && !partial.is_empty()
            {
                let mb_len = partial.len();
                let null_count_usize = partial
                    .iter()
                    .filter(|r| matches!(r, PayloadRefRow::Null))
                    .count();
                let null_count = u16::try_from(null_count_usize).unwrap_or(u16::MAX);
                let raw_body = if null_count_usize == mb_len {
                    Vec::new()
                } else {
                    let mut rb = Vec::new();
                    if self.options.profile == WriteProfile::L0Fast {
                        encode_payload_refs_l0_direct(&partial, &mut rb)?;
                    } else {
                        encode_payload_refs(&partial, &mut rb)?;
                    }
                    rb
                };
                mbs.push((mb_len as u16, null_count, raw_body));
            }

            let mut cur_first = first_row_u32;
            for (mb_len_u16, null_count, raw_body) in mbs {
                self.phys_col_meta[cidx].null_count_total = self.phys_col_meta[cidx]
                    .null_count_total
                    .saturating_add(u32::from(null_count));
                if null_count == mb_len_u16 {
                    self.phys_blocks[cidx].push(BlockEntry {
                        first_row: cur_first,
                        row_count: mb_len_u16,
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
                    let block_codec = if raw_body.len() < 1024 {
                        Codec::Lz4
                    } else {
                        default_l1_codec()
                    };
                    let blk_off = base_offset + wide_buf.len() as u64;
                    let info = write_framed_block(
                        Encoding::PayloadRefs,
                        &raw_body,
                        block_codec,
                        zstd_level,
                        wide_buf,
                    )?;
                    let block_flags = if null_count > 0 {
                        BLOCK_FLAG_HAS_NULLS
                    } else {
                        0
                    };
                    self.phys_blocks[cidx].push(BlockEntry {
                        first_row: cur_first,
                        row_count: mb_len_u16,
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
                cur_first += u32::from(mb_len_u16);
            }
            return Ok(());
        }

        if prechunked.is_none()
            && (self.options.profile == WriteProfile::L0Fast || self.pack_builder.is_none())
        {
            if let Column::Const { value, .. } = col
                && value.is_null()
            {
                let mut row_off = 0usize;
                while row_off < rg_rows {
                    let mb_len = (rg_rows - row_off).min(mb_size);
                    let mb_u16 = mb_len as u16;
                    self.phys_col_meta[cidx].null_count_total = self.phys_col_meta[cidx]
                        .null_count_total
                        .saturating_add(u32::from(mb_u16));
                    self.phys_blocks[cidx].push(BlockEntry {
                        first_row: first_row_u32 + row_off as u32,
                        row_count: mb_u16,
                        null_count: mb_u16,
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
                    row_off += mb_len;
                }
                return Ok(());
            }

            let fc = as_flat_cow(col)?;
            let FlatData::Utf8(u) = fc.data() else {
                return Err(StatamError::SchemaMismatch {
                    reason: "ExternalPayload column must be DataType::Utf8",
                });
            };
            let col_nulls = fc.nulls();
            let mut raw_body = Vec::new();
            let mut row_off = 0usize;
            while row_off < rg_rows {
                let mb_len = (rg_rows - row_off).min(mb_size);
                let mb_first_row = first_row_u32 + row_off as u32;
                raw_body.clear();
                let null_count = encode_payload_refs_l0_from_utf8(
                    u,
                    col_nulls,
                    base_row_off + row_off,
                    mb_len,
                    INLINE_PAYLOAD_THRESHOLD,
                    L0_LOCAL_PACK_IDX,
                    |slice| l0_local.add_chunk(slice),
                    &mut raw_body,
                )?;
                self.phys_col_meta[cidx].null_count_total = self.phys_col_meta[cidx]
                    .null_count_total
                    .saturating_add(u32::from(null_count));
                if usize::from(null_count) == mb_len {
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
                    let block_codec = if self.options.profile == WriteProfile::L0Fast {
                        if raw_body.len() < 1024 {
                            Codec::Lz4
                        } else {
                            default_l1_codec()
                        }
                    } else {
                        preferred_codec
                    };
                    let blk_off = base_offset + wide_buf.len() as u64;
                    let info = write_framed_block(
                        Encoding::PayloadRefs,
                        &raw_body,
                        block_codec,
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
            return Ok(());
        }

        let flat = if prechunked.is_some() {
            None
        } else {
            Some(as_flat_cow(col)?)
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
                self.build_payload_ref_rows(cidx, fc, base_row_off + row_off, mb_len, l0_local)?
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
                if self.options.profile == WriteProfile::L0Fast {
                    encode_payload_refs_l0_direct(&ref_rows, &mut raw_body)?;
                } else {
                    encode_payload_refs(&ref_rows, &mut raw_body)?;
                }
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
        let mut boundary_cids_scratch: Vec<u32> = Vec::new();
        let mut prefix_mru: Vec<PayloadPrefixCacheEntry> = Vec::with_capacity(2);
        let mut prefix_table: Vec<Option<PayloadPrefixCacheEntry>> = Vec::new();
        let mut exact_cache: [Option<PayloadExactCacheEntry>; 64] = std::array::from_fn(|_| None);

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
                    // W4 bypass: skip CDC hashing, dedup map, and trigrams; omit edge bytes for single-chunk payloads.
                    let bypass_chunk_max = MAX_FRAME_RAW_BYTES.min(512 * 1024);
                    let single_chunk = bytes.len() <= bypass_chunk_max;
                    for slice in bytes.chunks(bypass_chunk_max) {
                        let cid = pb.append_raw_bypass_with_edge(slice, !single_chunk)?;
                        refs.push((0u16, cid));
                    }
                } else {
                    let need_sample = !self
                        .bypass_detectors
                        .get(&cidx)
                        .is_some_and(DedupBypassDetector::is_decided);
                    let exact_slot = if !need_sample && bytes.len() >= 16 {
                        let mut h = (bytes.len() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
                        let h0 = u64::from_le_bytes([
                            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
                            bytes[7],
                        ]);
                        let h1 = u64::from_le_bytes([
                            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13],
                            bytes[14], bytes[15],
                        ]);
                        let t0 = bytes.len() - 16;
                        let h2 = u64::from_le_bytes([
                            bytes[t0],
                            bytes[t0 + 1],
                            bytes[t0 + 2],
                            bytes[t0 + 3],
                            bytes[t0 + 4],
                            bytes[t0 + 5],
                            bytes[t0 + 6],
                            bytes[t0 + 7],
                        ]);
                        let t1 = bytes.len() - 8;
                        let h3 = u64::from_le_bytes([
                            bytes[t1],
                            bytes[t1 + 1],
                            bytes[t1 + 2],
                            bytes[t1 + 3],
                            bytes[t1 + 4],
                            bytes[t1 + 5],
                            bytes[t1 + 6],
                            bytes[t1 + 7],
                        ]);
                        h = (h ^ h0).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                        h = (h ^ h1).wrapping_mul(0x94d0_49bb_1331_11eb);
                        h = (h ^ h2).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                        h = (h ^ h3).wrapping_mul(0x94d0_49bb_1331_11eb);
                        h ^= h >> 31;
                        Some((h as usize) & 63)
                    } else {
                        None
                    };

                    if let Some(slot) = exact_slot
                        && let Some((prev_row, prev_refs)) = &exact_cache[slot]
                        && u.get(*prev_row).unwrap_or("").as_bytes() == bytes
                    {
                        out.push(PayloadRefRow::Chunks(prev_refs.clone()));
                        continue;
                    }

                    let mut cache_hit = false;
                    let mut mru_hit = false;
                    let prefix_slot = if !need_sample && bytes.len() >= 128 {
                        let mut h = 0x517c_c1b7_2722_0a95u64;
                        for &chunk in bytes[..128].as_chunks::<8>().0 {
                            let v = u64::from_le_bytes(chunk);
                            h = (h ^ v).wrapping_mul(0x9e37_79b9_7f4a_7c15);
                        }
                        h ^= h >> 31;
                        Some((h as usize) & 4095)
                    } else {
                        None
                    };

                    if !need_sample {
                        let table_hit = if !prefix_table.is_empty() {
                            prefix_slot.and_then(|slot| prefix_table[slot].as_ref())
                        } else {
                            None
                        };
                        for (idx_in_iter, (prev_row_idx, prefix_byte_len, prefix_refs)) in
                            prefix_mru.iter().rev().chain(table_hit).enumerate()
                        {
                            let prefix_len = *prefix_byte_len;
                            if bytes.len() > prefix_len + 1 && bytes[prefix_len] == b',' {
                                let prev_bytes = u.get(*prev_row_idx).unwrap_or("").as_bytes();
                                if prev_bytes.len() >= prefix_len {
                                    let edge = prefix_len.min(8);
                                    if bytes[..edge] == prev_bytes[..edge]
                                        && bytes[prefix_len - edge..prefix_len]
                                            == prev_bytes[prefix_len - edge..prefix_len]
                                        && bytes[..prefix_len] == prev_bytes[..prefix_len]
                                    {
                                        refs.extend_from_slice(prefix_refs);
                                        chunk_json_array_continuation(
                                            &bytes[prefix_len..],
                                            &mut chunk_scratch,
                                        );
                                        for &chunk in &chunk_scratch {
                                            let cid = pb.intern_chunk(chunk)?;
                                            refs.push((0u16, cid));
                                        }
                                        cache_hit = true;
                                        mru_hit = idx_in_iter < prefix_mru.len();
                                        break;
                                    }
                                }
                            }
                        }
                        if !cache_hit
                            && let Some((prefix_len, prefix_refs)) = pb.lookup_prefix(bytes)
                        {
                            refs.extend_from_slice(prefix_refs);
                            chunk_json_array_continuation(&bytes[prefix_len..], &mut chunk_scratch);
                            for &chunk in &chunk_scratch {
                                let cid = pb.intern_chunk(chunk)?;
                                refs.push((0u16, cid));
                            }
                            cache_hit = true;
                        }
                    }

                    if !cache_hit {
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

                    if let Some(&last_chunk) = chunk_scratch.last()
                        && (last_chunk == b"]".as_slice() || last_chunk.starts_with(b","))
                        && refs.len() >= 2
                        && bytes.ends_with(b"]")
                        && bytes.len() > last_chunk.len()
                    {
                        if prefix_mru.len() == 2 {
                            prefix_mru.remove(0);
                        }
                        let prefix_byte_len = bytes.len() - last_chunk.len();
                        let prefix_refs = &refs[..refs.len() - 1];
                        if !mru_hit && !need_sample {
                            pb.insert_prefix(&bytes[..prefix_byte_len], prefix_refs);
                        }
                        let entry = (r, prefix_byte_len, prefix_refs.to_vec());
                        if let Some(slot) = prefix_slot {
                            if prefix_table.is_empty() {
                                prefix_table = vec![None; 4096];
                            }
                            prefix_table[slot] = Some(entry.clone());
                        }
                        prefix_mru.push(entry);
                    }
                    if let Some(slot) = exact_slot {
                        exact_cache[slot] = Some((r, refs.clone()));
                    }
                }
                if refs.len() > 1 {
                    boundary_cids_scratch.clear();
                    boundary_cids_scratch.extend(refs.iter().map(|&(_, cid)| cid));
                    pb.mark_boundary_chunks(&boundary_cids_scratch);
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

    fn take_and_build_bloom_for_col(
        &mut self,
        cidx: usize,
    ) -> Result<Option<Vec<u8>>, StatamError> {
        let Some(rgs) = self.bloom_rgs.remove(&cidx) else {
            return Ok(None);
        };
        let has_any = self.bloom_has_values.remove(&cidx).unwrap_or(false);
        if !has_any || rgs.is_empty() {
            return Ok(None);
        }
        let bytes = BloomIndex::PerRowGroup(rgs).to_bytes();
        if bytes.len() > MAX_BLOOM_BYTES {
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    fn take_and_build_primary_bloom(
        &mut self,
        cidx: usize,
        avail_bytes: usize,
    ) -> Result<Option<(Vec<u8>, bool)>, StatamError> {
        let Some(rgs) = self.bloom_rgs.remove(&cidx) else {
            return Ok(None);
        };
        let has_any = self.bloom_has_values.remove(&cidx).unwrap_or(false);
        if !has_any || rgs.is_empty() {
            return Ok(None);
        }
        let bytes = BloomIndex::PerRowGroup(rgs).to_bytes();
        if bytes.len() > MAX_BLOOM_BYTES {
            return Ok(None);
        }
        let fits_hot = bytes.len() <= avail_bytes;
        Ok(Some((bytes, fits_hot)))
    }
}

#[inline]
fn as_flat_cow(col: &Column) -> Result<Cow<'_, FlatColumn>, StatamError> {
    match col {
        Column::Flat(fc) => Ok(Cow::Borrowed(fc)),
        other => Ok(Cow::Owned(other.materialize(None)?)),
    }
}

#[inline]
fn bloom_fpp_for_ndv(ndv: usize) -> f64 {
    if ndv > 80_000 { 0.04 } else { BLOOM_FPP }
}

fn build_rg_sbbf(
    col: &Column,
    base_row_off: usize,
    rg_rows: usize,
    profile: WriteProfile,
    col_name: &str,
    rg0_dict: Option<&FileDict>,
) -> Result<(Sbbf, bool), StatamError> {
    if rg_rows == 0 || col.is_empty() {
        return Ok((Sbbf::new_for_ndv(1, BLOOM_FPP), false));
    }

    // RG0 dict shortcut: hash only the unique dictionary entries directly.
    if let Some(dict) = rg0_dict {
        let dict_len = dict.len();
        if dict_len == 0 {
            return Ok((Sbbf::new_for_ndv(1, BLOOM_FPP), false));
        }
        let mut sbbf = Sbbf::new_for_ndv(dict_len, bloom_fpp_for_ndv(dict_len));
        let mut had_any = false;
        let dict_vals = dict.values();
        for idx in 0..dict_len {
            if let Some(h) = flat_row_bloom_hash(dict_vals, idx) {
                sbbf.insert_hash(h);
                had_any = true;
            }
        }
        return Ok((sbbf, had_any));
    }

    // Fast path for L0Fast Flat columns and unique ID columns (`span_id`, `score_id`):
    // estimate ndv = non-null count without sorting Vec<u64>.
    if (profile == WriteProfile::L0Fast || matches!(col_name, "span_id" | "score_id"))
        && let Column::Flat(f) = col
    {
        let end = base_row_off + rg_rows;
        let non_nulls = match f.nulls() {
            None => rg_rows,
            Some(bm) if base_row_off == 0 && rg_rows == f.len() => {
                rg_rows.saturating_sub(bm.count_zeros())
            }
            Some(bm) => (base_row_off..end).filter(|&r| bm.get(r)).count(),
        };
        if non_nulls == 0 {
            return Ok((Sbbf::new_for_ndv(1, BLOOM_FPP), false));
        }
        let mut sbbf = Sbbf::new_for_ndv(non_nulls, bloom_fpp_for_ndv(non_nulls));
        let had_any = insert_flat_bloom_range(f, base_row_off, end, &mut sbbf);
        return Ok((sbbf, had_any));
    }

    let mut hashes = Vec::new();
    if base_row_off == 0 && rg_rows == col.len() {
        collect_column_bloom_hashes(col, &mut hashes)?;
    } else {
        let sliced = col.slice(base_row_off, rg_rows)?;
        collect_column_bloom_hashes(&sliced, &mut hashes)?;
    }
    if hashes.is_empty() {
        return Ok((Sbbf::new_for_ndv(1, BLOOM_FPP), false));
    }
    if profile == WriteProfile::L1Compact {
        hashes.sort_unstable();
        hashes.dedup();
    }
    let mut sbbf = Sbbf::new_for_ndv(hashes.len(), bloom_fpp_for_ndv(hashes.len()));
    for h in hashes {
        sbbf.insert_hash(h);
    }
    Ok((sbbf, true))
}

struct EncodedScalarCol {
    local_buf: Vec<u8>,
    blocks: Vec<BlockEntry>,
    used_dict_encoding: bool,
    has_non_dict_data_block: bool,
    null_count_total: u32,
    narrow_trigrams: Option<Vec<Option<Vec<u8>>>>,
}

struct EncodedShreddedMap {
    logical_col: usize,
    is_metadata: bool,
    local_core_buf: Vec<u8>,
    sub_col_indices: Vec<usize>,
    sub_blocks: Vec<Vec<BlockEntry>>,
    residual_counts: Vec<u32>,
    res_null_count_total: u32,
    local_wide_buf: Vec<u8>,
    residual_blocks: Vec<BlockEntry>,
}

fn append_col_blocks(
    dst_buf: &mut Vec<u8>,
    group_base_offset: u64,
    local_buf: &[u8],
    blocks: Vec<BlockEntry>,
    dst_blocks: &mut Vec<BlockEntry>,
) -> Result<(), StatamError> {
    let col_rel_off = dst_buf.len() as u64;
    let abs_base =
        group_base_offset
            .checked_add(col_rel_off)
            .ok_or(StatamError::BoundsExceeded {
                reason: "column block offset overflow",
            })?;
    dst_buf.extend_from_slice(local_buf);
    for mut blk in blocks {
        if !matches!(blk.encoding, Encoding::ConstNull | Encoding::Const) {
            blk.offset = blk
                .offset
                .checked_add(abs_base)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "block offset overflow",
                })?;
        }
        dst_blocks.push(blk);
    }
    Ok(())
}

fn commit_scalar_col_result(
    phys_col_meta: &mut [PhysColWriteState],
    phys_blocks: &mut [Vec<BlockEntry>],
    narrow_trigrams: &mut HashMap<usize, Vec<Option<Vec<u8>>>>,
    cidx: usize,
    res: EncodedScalarCol,
    group_base_offset: u64,
    dst_buf: &mut Vec<u8>,
) -> Result<(), StatamError> {
    if res.used_dict_encoding {
        phys_col_meta[cidx].used_dict_encoding = true;
    }
    if res.has_non_dict_data_block {
        phys_col_meta[cidx].has_non_dict_data_block = true;
    }
    phys_col_meta[cidx].null_count_total = phys_col_meta[cidx]
        .null_count_total
        .saturating_add(res.null_count_total);
    if let Some(tris) = res.narrow_trigrams
        && let Some(list) = narrow_trigrams.get_mut(&cidx)
    {
        list.extend(tris);
    }
    append_col_blocks(
        dst_buf,
        group_base_offset,
        &res.local_buf,
        res.blocks,
        &mut phys_blocks[cidx],
    )
}

fn commit_shredded_subcols(
    shredded_map: &mut ShreddedMapDir,
    phys_blocks: &mut [Vec<BlockEntry>],
    shred: &mut EncodedShreddedMap,
    core_offset: u64,
    core_buf: &mut Vec<u8>,
) -> Result<(), StatamError> {
    let col_rel_off = core_buf.len() as u64;
    let abs_base = core_offset
        .checked_add(col_rel_off)
        .ok_or(StatamError::BoundsExceeded {
            reason: "shredded subcol block offset overflow",
        })?;
    core_buf.extend_from_slice(&shred.local_core_buf);
    drop(std::mem::take(&mut shred.local_core_buf));
    for (k_idx, &sub_pidx) in shred.sub_col_indices.iter().enumerate() {
        shredded_map.keys[k_idx].residual_count = shredded_map.keys[k_idx]
            .residual_count
            .saturating_add(shred.residual_counts[k_idx]);
        for mut blk in std::mem::take(&mut shred.sub_blocks[k_idx]) {
            if !matches!(blk.encoding, Encoding::ConstNull | Encoding::Const) {
                blk.offset =
                    blk.offset
                        .checked_add(abs_base)
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "block offset overflow",
                        })?;
            }
            phys_blocks[sub_pidx].push(blk);
        }
    }
    Ok(())
}

fn extract_rg0_shredded_keys(
    col: &Column,
    base_row_off: usize,
    rg_rows: usize,
) -> Result<Vec<String>, StatamError> {
    let sliced_owned;
    let col_ref = if base_row_off == 0 && rg_rows == col.len() {
        col
    } else {
        sliced_owned = col.slice(base_row_off, rg_rows)?;
        &sliced_owned
    };
    let flat = as_flat_cow(col_ref)?;
    let FlatData::MapUtf8(map) = flat.data() else {
        return Ok(Vec::new());
    };
    Ok(select_shredded_keys(map))
}

fn is_rg0_dict_candidate(
    field: &cetra_column::Field,
    locality: LocalityGroup,
    profile: WriteProfile,
) -> bool {
    if locality == LocalityGroup::ExternalPayload {
        return false;
    }
    if DictValueKind::from_data_type(&field.data_type).is_none() {
        return false;
    }
    if HIGH_CARDINALITY_ID_COLS.contains(&field.name.as_str()) {
        return false;
    }
    if profile == WriteProfile::L0Fast
        && (!KNOWN_LOW_CARD_DICT_COLS.contains(&field.name.as_str())
            || matches!(field.name.as_str(), "session_id" | "user_id"))
    {
        return false;
    }
    true
}

fn eval_rg0_dict_candidate(
    col: &Column,
    field_name: &str,
    locality: LocalityGroup,
    profile: WriteProfile,
    base_row_off: usize,
    rg_rows: usize,
) -> Result<Option<Rg0DictResult>, StatamError> {
    let sliced_owned;
    let col_ref = if base_row_off == 0 && rg_rows == col.len() {
        col
    } else {
        sliced_owned = col.slice(base_row_off, rg_rows)?;
        &sliced_owned
    };
    let flat = as_flat_cow(col_ref)?;
    let is_known_low_card = KNOWN_LOW_CARD_DICT_COLS.contains(&field_name);
    let is_core = locality == LocalityGroup::Core;
    let is_wide_non_known = locality == LocalityGroup::WideRow && !is_known_low_card;
    let max_entries = match profile {
        WriteProfile::L0Fast => 512,
        WriteProfile::L1Compact => {
            if is_known_low_card || is_core {
                16_384
            } else if is_wide_non_known {
                1_024
            } else {
                4_096
            }
        }
    };
    let builder = match profile {
        WriteProfile::L0Fast => DictBuilder::l0_with_max(max_entries),
        WriteProfile::L1Compact => DictBuilder::l1_with_max(max_entries),
    };
    let Some((dict, codes)) = builder.build(&flat)? else {
        return Ok(None);
    };
    if is_wide_non_known && (dict.len() > 1_024 || dict.memory_bytes() > DICT_PASSTHROUGH_MAX_BYTES)
    {
        return Ok(None);
    }
    let non_null_rows = flat
        .len()
        .saturating_sub(flat.nulls().map_or(0, |bm| bm.count_zeros()));
    let ratio_den = if dict.len() > 4_096 { 4 } else { 2 };
    let low_ndv = if is_known_low_card {
        dict.len() <= max_entries
            && (non_null_rows < 16 || dict.len() * ratio_den <= non_null_rows + 4)
    } else {
        dict.len() <= max_entries
            && ((non_null_rows >= 16 && dict.len() * ratio_den <= non_null_rows) || dict.len() <= 8)
    };
    if low_ndv {
        Ok(Some((dict, codes, flat.nulls().cloned())))
    } else {
        Ok(None)
    }
}

#[allow(clippy::too_many_arguments)]
fn encode_scalar_or_nested_col_into(
    col: &Column,
    dt: &DataType,
    profile: WriteProfile,
    is_wide_row: bool,
    dict: Option<&FileDict>,
    precomputed_rg0: Option<(Vec<u32>, Option<cetra_column::Bitmap>)>,
    base_row_off: usize,
    rg_rows: usize,
    first_row_u32: u32,
    mb_size: usize,
    base_offset: u64,
    zstd_level: i32,
    out_buf: &mut Vec<u8>,
    out_blocks: &mut Vec<BlockEntry>,
    mut out_trigrams: Option<&mut Vec<Option<Vec<u8>>>>,
) -> Result<(bool, bool, u32), StatamError> {
    let mut used_dict_encoding = false;
    let mut has_non_dict_data_block = false;
    let mut null_count_total = 0u32;

    let precomputed_rg_n = if precomputed_rg0.is_none()
        && rg_rows >= 256
        && let Some(d) = dict
    {
        let rg_slice_owned;
        let rg_slice = if base_row_off == 0 && rg_rows == col.len() {
            col
        } else {
            rg_slice_owned = col.slice(base_row_off, rg_rows)?;
            &rg_slice_owned
        };
        if let Ok(flat) = as_flat_cow(rg_slice)
            && let Some(codes) = d.try_map_column(&flat)?
        {
            Some((codes, flat.nulls().cloned()))
        } else {
            None
        }
    } else {
        None
    };

    if let Some((rg0_codes, rg0_nulls)) = precomputed_rg0.or(precomputed_rg_n) {
        let mut row_off = 0usize;
        while row_off < rg_rows {
            let mb_len = (rg_rows - row_off).min(mb_size);
            let mb_first_row = first_row_u32 + row_off as u32;
            let mb_nulls_owned;
            let mb_nulls = match &rg0_nulls {
                None => None,
                Some(bm) => {
                    let mut sub_bm = cetra_column::Bitmap::new_ones(mb_len);
                    let mut has_null = false;
                    for i in 0..mb_len {
                        if !bm.get(row_off + i) {
                            let _ = sub_bm.set(i, false);
                            has_null = true;
                        }
                    }
                    if has_null {
                        mb_nulls_owned = sub_bm;
                        Some(&mb_nulls_owned)
                    } else {
                        None
                    }
                }
            };
            let enc = encode_dict_codes_precomputed(
                &rg0_codes[row_off..row_off + mb_len],
                mb_nulls,
                profile,
                0,
            )?;
            if enc.encoding == Encoding::Dict {
                used_dict_encoding = true;
            }
            null_count_total = null_count_total.saturating_add(u32::from(enc.null_count));
            let entry = enc.write_framed_entry(
                mb_first_row,
                mb_len as u16,
                base_offset,
                zstd_level,
                out_buf,
            )?;
            out_blocks.push(entry);
            if let Some(list) = out_trigrams.as_deref_mut() {
                list.push(None);
            }
            row_off += mb_len;
        }
        return Ok((
            used_dict_encoding,
            has_non_dict_data_block,
            null_count_total,
        ));
    }

    // O(1) mini-block fast path for `Column::Const` (all-null or inline <=16B / empty collection).
    if let Column::Const { value, .. } = col {
        if value.is_null() || matches!(dt, DataType::Null) {
            let mut row_off = 0usize;
            while row_off < rg_rows {
                let mb_len = (rg_rows - row_off).min(mb_size);
                let mb_u16 = mb_len as u16;
                null_count_total = null_count_total.saturating_add(mb_len as u32);
                out_blocks.push(BlockEntry {
                    first_row: first_row_u32 + row_off as u32,
                    row_count: mb_u16,
                    null_count: mb_u16,
                    offset: 0,
                    stored_len: 0,
                    raw_len: 0,
                    encoding: Encoding::ConstNull,
                    codec: Codec::None,
                    block_flags: BLOCK_FLAG_ALL_NULL | BLOCK_FLAG_HAS_NULLS,
                    min_inline_len: 0,
                    max_inline_len: 0,
                    reserved: [0; 3],
                    min_inline: [0u8; 16],
                    max_inline: [0u8; 16],
                });
                if let Some(list) = out_trigrams.as_deref_mut() {
                    list.push(None);
                }
                row_off += mb_len;
            }
            return Ok((
                used_dict_encoding,
                has_non_dict_data_block,
                null_count_total,
            ));
        }

        let single = Column::Const {
            value: value.clone(),
            len: 1,
        };
        let template = encode_mini_block_with_locality(&single, dt, profile, dict, is_wide_row)?;
        if template.encoding == Encoding::Const {
            let mut row_off = 0usize;
            while row_off < rg_rows {
                let mb_len = (rg_rows - row_off).min(mb_size);
                out_blocks.push(BlockEntry {
                    first_row: first_row_u32 + row_off as u32,
                    row_count: mb_len as u16,
                    null_count: 0,
                    offset: 0,
                    stored_len: 0,
                    raw_len: 0,
                    encoding: Encoding::Const,
                    codec: Codec::None,
                    block_flags: template.block_flags,
                    min_inline_len: template.min_inline_len,
                    max_inline_len: template.max_inline_len,
                    reserved: [0; 3],
                    min_inline: template.min_inline,
                    max_inline: template.max_inline,
                });
                if let Some(list) = out_trigrams.as_deref_mut() {
                    list.push(None);
                }
                row_off += mb_len;
            }
            return Ok((
                used_dict_encoding,
                has_non_dict_data_block,
                null_count_total,
            ));
        }
    }

    let mut row_off = 0usize;
    while row_off < rg_rows {
        let mb_len = (rg_rows - row_off).min(mb_size);
        let mb_first_row = first_row_u32 + row_off as u32;
        let abs_off = base_row_off + row_off;
        let sliced_owned;
        let sliced = if abs_off == 0 && mb_len == col.len() {
            col
        } else {
            sliced_owned = col.slice(abs_off, mb_len)?;
            &sliced_owned
        };
        let enc = encode_mini_block_with_locality(sliced, dt, profile, dict, is_wide_row)?;

        if enc.encoding == Encoding::Dict {
            used_dict_encoding = true;
        } else if enc.encoding != Encoding::ConstNull && enc.encoding != Encoding::Const {
            has_non_dict_data_block = true;
        }
        null_count_total = null_count_total.saturating_add(u32::from(enc.null_count));

        let min_savings_pct =
            if matches!(enc.encoding, Encoding::ForBitpack | Encoding::BitpackBool)
                || (enc.encoding == Encoding::Plain
                    && matches!(
                        dt,
                        DataType::Bool
                            | DataType::I64
                            | DataType::F64
                            | DataType::TimestampNs { .. }
                            | DataType::Date
                            | DataType::Decimal128 { .. }
                            | DataType::Interval
                            | DataType::FixedBytes8
                            | DataType::FixedBytes16
                    ))
            {
                12
            } else {
                0
            };
        let entry = enc.write_framed_entry_with_min_savings(
            mb_first_row,
            mb_len as u16,
            base_offset,
            zstd_level,
            min_savings_pct,
            out_buf,
        )?;
        out_blocks.push(entry);

        if let Some(list) = out_trigrams.as_deref_mut() {
            let filter = if enc.encoding != Encoding::Dict
                && enc.encoding != Encoding::Const
                && enc.encoding != Encoding::ConstNull
            {
                build_narrow_trigram_for_mb(sliced, entry.stored_len as usize)
            } else {
                None
            };
            list.push(filter);
        }

        row_off += mb_len;
    }

    Ok((
        used_dict_encoding,
        has_non_dict_data_block,
        null_count_total,
    ))
}

#[allow(clippy::too_many_arguments)]
fn encode_shredded_map_to_local(
    sm: &ShreddedMapDir,
    is_metadata: bool,
    col: &Column,
    base_row_off: usize,
    rg_rows: usize,
    first_row_u32: u32,
    mb_size: usize,
    profile: WriteProfile,
    zstd_level: i32,
) -> Result<EncodedShreddedMap, StatamError> {
    let logical_col = sm.logical_col_idx as usize;
    let promoted_keys: Vec<String> = sm.keys.iter().map(|k| k.key.clone()).collect();
    let sub_col_indices: Vec<usize> = sm.keys.iter().map(|k| k.sub_col_idx as usize).collect();

    let mut local_core_buf = Vec::new();
    let mut sub_blocks = vec![Vec::new(); sub_col_indices.len()];
    let mut residual_counts = vec![0u32; sub_col_indices.len()];
    let mut res_null_count_total = 0u32;
    let mut local_wide_buf = Vec::new();
    let mut residual_blocks = Vec::new();

    let mut row_off = 0usize;
    while row_off < rg_rows {
        let mb_len = (rg_rows - row_off).min(mb_size);
        let mb_first_row = first_row_u32 + row_off as u32;
        let abs_off = base_row_off + row_off;
        let sliced_owned;
        let sliced = if abs_off == 0 && mb_len == col.len() {
            col
        } else {
            sliced_owned = col.slice(abs_off, mb_len)?;
            &sliced_owned
        };
        let flat = as_flat_cow(sliced)?;
        let shredded = shred_map(&flat, &promoted_keys)?;

        for (k_idx, _) in sub_col_indices.iter().enumerate() {
            residual_counts[k_idx] =
                residual_counts[k_idx].saturating_add(shredded.residual_counts[k_idx]);
            let (sub_codec, sub_raw) =
                encode_shredded_subcol_with_codec(&shredded.sub_cols[k_idx], profile)?;
            let blk_off = local_core_buf.len() as u64;
            let info = write_framed_block(
                Encoding::ShreddedMap,
                &sub_raw,
                sub_codec,
                zstd_level,
                &mut local_core_buf,
            )?;
            sub_blocks[k_idx].push(BlockEntry {
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

        let res_enc = crate::codec::encode_shredded_residual_block(&shredded.residual, profile)?;
        res_null_count_total = res_null_count_total.saturating_add(u32::from(res_enc.null_count));
        let res_entry = res_enc.write_framed_entry(
            mb_first_row,
            mb_len as u16,
            0,
            zstd_level,
            &mut local_wide_buf,
        )?;
        residual_blocks.push(res_entry);

        row_off += mb_len;
    }

    Ok(EncodedShreddedMap {
        logical_col,
        is_metadata,
        local_core_buf,
        sub_col_indices,
        sub_blocks,
        residual_counts,
        res_null_count_total,
        local_wide_buf,
        residual_blocks,
    })
}

struct LocalL0PayloadBuilder {
    zstd_level: i32,
    write_threads: usize,
    frames: Vec<(u64, u32, u32, Vec<u8>)>, // (rel_frame_off, stored_len, raw_len, framed_bytes)
    chunks: Vec<ChunkLoc>,
    cur_raw: Vec<u8>,
    cur_chunks: Vec<(u32, u32)>, // (offset_in_frame, raw_len)
    pending_frames: Vec<PendingL0Frame>,
    pending_chunk_count: usize,
    pending_frame_raw_bytes: usize,
}

impl LocalL0PayloadBuilder {
    fn new(zstd_level: i32) -> Self {
        Self::new_with_threads(zstd_level, 1)
    }

    fn new_with_threads(zstd_level: i32, write_threads: usize) -> Self {
        Self {
            zstd_level,
            write_threads,
            frames: Vec::new(),
            chunks: Vec::new(),
            cur_raw: Vec::new(),
            cur_chunks: Vec::new(),
            pending_frames: Vec::new(),
            pending_chunk_count: 0,
            pending_frame_raw_bytes: 0,
        }
    }

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
        let cid =
            u32::try_from(self.chunks.len() + self.pending_chunk_count + self.cur_chunks.len())
                .map_err(|_| StatamError::BoundsExceeded {
                    reason: "L0 payload chunk_id overflows u32",
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
        if self.write_threads > 1 {
            let batch_cap = self.write_threads.clamp(1, MAX_WRITE_THREADS);
            if !self.pending_frames.is_empty()
                && (self.pending_frames.len() >= batch_cap
                    || self
                        .pending_frame_raw_bytes
                        .saturating_add(self.cur_raw.len())
                        > MAX_PENDING_RAW_BYTES)
            {
                self.flush_pending()?;
            }
            let raw = std::mem::take(&mut self.cur_raw);
            let frame_chunks = std::mem::take(&mut self.cur_chunks);
            self.pending_chunk_count = self.pending_chunk_count.saturating_add(frame_chunks.len());
            self.pending_frame_raw_bytes = self.pending_frame_raw_bytes.saturating_add(raw.len());
            self.pending_frames.push((raw, frame_chunks));
            if self.pending_frames.len() >= batch_cap
                || self.pending_frame_raw_bytes >= MAX_PENDING_RAW_BYTES
            {
                self.flush_pending()?;
            }
            return Ok(());
        }
        let mut framed = Vec::new();
        let codec = if self.cur_raw.len() < 1024 {
            Codec::Lz4
        } else {
            default_l1_codec()
        };
        let info = write_framed_pack_frame(&self.cur_raw, codec, 0, self.zstd_level, &mut framed)?;
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

    fn flush_pending(&mut self) -> Result<(), StatamError> {
        let batch = std::mem::take(&mut self.pending_frames);
        self.pending_chunk_count = 0;
        self.pending_frame_raw_bytes = 0;
        if batch.is_empty() {
            return Ok(());
        }
        let zstd_level = self.zstd_level;
        let encoded = parallel_map_in_order(batch, self.write_threads, |(raw, frame_chunks)| {
            let mut framed = Vec::new();
            let codec = if raw.len() < 1024 {
                Codec::Lz4
            } else {
                default_l1_codec()
            };
            let info = write_framed_pack_frame(&raw, codec, 0, zstd_level, &mut framed)?;
            drop(raw);
            Ok((info.stored_len, info.raw_len, framed, frame_chunks))
        })?;
        for (stored_len, raw_len, framed, frame_chunks) in encoded {
            let fid =
                u32::try_from(self.frames.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "L0 payload frame_id overflows u32",
                })?;
            for (off_in_frame, chunk_raw_len) in frame_chunks {
                self.chunks.push(ChunkLoc {
                    frame_id: fid,
                    offset_in_frame: off_in_frame,
                    raw_len: chunk_raw_len,
                });
            }
            self.frames.push((0, stored_len, raw_len, framed));
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<u8>, StatamError> {
        if !self.cur_raw.is_empty() {
            self.flush_frame()?;
        }
        if !self.pending_frames.is_empty() {
            self.flush_pending()?;
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
    pieces: &mut Vec<Column>,
    total_rows: usize,
) -> Result<Column, StatamError> {
    if pieces.len() == 1
        && let Some(only) = pieces.pop()
    {
        return Ok(only);
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
        flats.push(as_flat_cow(p)?);
    }
    let flat_refs: Vec<&FlatColumn> = flats.iter().map(Cow::as_ref).collect();
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
                    child_flats.push(as_flat_cow(values)?);
                }
            }
            let child_refs: Vec<&FlatColumn> = child_flats.iter().map(Cow::as_ref).collect();
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
            let mut seen = vec![false; flat_vals.len()];
            for (r, &c) in d.codes().iter().enumerate() {
                if !d.is_null(r)
                    && let Some(slot) = seen.get_mut(c as usize)
                    && !*slot
                {
                    *slot = true;
                    if let Some(h) = flat_row_bloom_hash(flat_vals, c as usize) {
                        out.push(h);
                    }
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
            let mut prev_h: Option<u64> = None;
            for row in 0..n {
                if let Some(h) = flat_row_bloom_hash(f, row)
                    && Some(h) != prev_h
                {
                    out.push(h);
                    prev_h = Some(h);
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

fn insert_flat_bloom_range(flat: &FlatColumn, start: usize, end: usize, sbbf: &mut Sbbf) -> bool {
    if start >= end {
        return false;
    }
    let nulls = flat.nulls();
    match flat.data() {
        FlatData::FixedBytes16(v) => match nulls {
            None => {
                for item in &v[start..end] {
                    sbbf.insert_hash(statam_bloom_hash(item));
                }
                true
            }
            Some(bm) => {
                let mut had = false;
                for (i, item) in v[start..end].iter().enumerate() {
                    if bm.get(start + i) {
                        sbbf.insert_hash(statam_bloom_hash(item));
                        had = true;
                    }
                }
                had
            }
        },
        FlatData::FixedBytes8(v) => match nulls {
            None => {
                for item in &v[start..end] {
                    sbbf.insert_hash(statam_bloom_hash(item));
                }
                true
            }
            Some(bm) => {
                let mut had = false;
                for (i, item) in v[start..end].iter().enumerate() {
                    if bm.get(start + i) {
                        sbbf.insert_hash(statam_bloom_hash(item));
                        had = true;
                    }
                }
                had
            }
        },
        FlatData::Utf8(u) => {
            let offs = u.offsets();
            let bytes = u.bytes();
            match nulls {
                None => {
                    for r in start..end {
                        let s = offs[r] as usize;
                        let e = offs[r + 1] as usize;
                        sbbf.insert_hash(statam_bloom_hash(&bytes[s..e]));
                    }
                    true
                }
                Some(bm) => {
                    let mut had = false;
                    for r in start..end {
                        if bm.get(r) {
                            let s = offs[r] as usize;
                            let e = offs[r + 1] as usize;
                            sbbf.insert_hash(statam_bloom_hash(&bytes[s..e]));
                            had = true;
                        }
                    }
                    had
                }
            }
        }
        FlatData::Binary(b) => {
            let offs = b.offsets();
            let bytes = b.bytes();
            match nulls {
                None => {
                    for r in start..end {
                        let s = offs[r] as usize;
                        let e = offs[r + 1] as usize;
                        sbbf.insert_hash(statam_bloom_hash(&bytes[s..e]));
                    }
                    true
                }
                Some(bm) => {
                    let mut had = false;
                    for r in start..end {
                        if bm.get(r) {
                            let s = offs[r] as usize;
                            let e = offs[r + 1] as usize;
                            sbbf.insert_hash(statam_bloom_hash(&bytes[s..e]));
                            had = true;
                        }
                    }
                    had
                }
            }
        }
        FlatData::I64(v) | FlatData::TimestampNs { values: v, .. } => match nulls {
            None => {
                for &x in &v[start..end] {
                    sbbf.insert_hash(statam_bloom_hash(&x.to_le_bytes()));
                }
                true
            }
            Some(bm) => {
                let mut had = false;
                for (i, &x) in v[start..end].iter().enumerate() {
                    if bm.get(start + i) {
                        sbbf.insert_hash(statam_bloom_hash(&x.to_le_bytes()));
                        had = true;
                    }
                }
                had
            }
        },
        FlatData::Date(v) => match nulls {
            None => {
                for &x in &v[start..end] {
                    sbbf.insert_hash(statam_bloom_hash(&x.to_le_bytes()));
                }
                true
            }
            Some(bm) => {
                let mut had = false;
                for (i, &x) in v[start..end].iter().enumerate() {
                    if bm.get(start + i) {
                        sbbf.insert_hash(statam_bloom_hash(&x.to_le_bytes()));
                        had = true;
                    }
                }
                had
            }
        },
        FlatData::Decimal128 { values, .. } => match nulls {
            None => {
                for &x in &values[start..end] {
                    sbbf.insert_hash(statam_bloom_hash(&x.to_le_bytes()));
                }
                true
            }
            Some(bm) => {
                let mut had = false;
                for (i, &x) in values[start..end].iter().enumerate() {
                    if bm.get(start + i) {
                        sbbf.insert_hash(statam_bloom_hash(&x.to_le_bytes()));
                        had = true;
                    }
                }
                had
            }
        },
        _ => false,
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
    if stored_len < MIN_NARROW_TRIGRAM_STORED_BYTES || col.is_empty() {
        return None;
    }
    let flat = as_flat_cow(col).ok()?;
    let FlatData::Utf8(u) = flat.data() else {
        return None;
    };

    let max_bytes = (stored_len / 10).min(MAX_TRIGRAM_BLOOM_BYTES);
    if max_bytes < MIN_TRIGRAM_BLOOM_BYTES {
        return None;
    }
    let cap_pow2 = 1usize << (usize::BITS as usize - 1 - max_bytes.leading_zeros() as usize);

    let mut bloom = TrigramBloom::new(cap_pow2);
    let mut any_trigram = false;
    let nulls = flat.nulls();
    for r in 0..flat.len() {
        if nulls.is_none_or(|bm| bm.get(r))
            && let Some(s) = u.get(r)
            && s.len() >= 3
        {
            any_trigram = true;
            bloom.insert_bytes(s.as_bytes());
        }
    }
    if !any_trigram {
        return None;
    }

    while bloom.as_bytes().len() > MIN_TRIGRAM_BLOOM_BYTES && bloom.bit_density() < 0.25 {
        bloom = bloom.fold_half();
    }
    if bloom.bit_density() > MAX_NARROW_TRIGRAM_DENSITY {
        return None;
    }
    Some(bloom.as_bytes().to_vec())
}

struct AssembledHotTail {
    hot_tail_raw_len: u32,
    hot_tail_len: u32,
    hot_tail_stored: Vec<u8>,
    is_lz4: bool,
    lazy_data_buf: Vec<u8>,
    cur_file_off: u64,
    schema_len: u32,
    col_dir_off: u32,
    rg_dir_off: u32,
    block_dir_off: u32,
    hot_bloom_off: u32,
    hot_bloom_len: u32,
    hot_dict_off: u32,
    hot_dict_len: u32,
    lazy_dir_off: u32,
    lazy_dir_len: u32,
}

#[allow(clippy::too_many_arguments)]
fn assemble_hot_tail_and_lazy(
    base_col_dirs: &[ColumnDirEntry],
    schema_bytes: &[u8],
    rg_dir_bytes: &[u8],
    block_dir_bytes: &[u8],
    data_end_offset: u64,
    compress_hot_tail_lz4: bool,
    primary_bloom: Option<&(usize, Vec<u8>, bool)>,
    dict_items: &[(usize, Vec<u8>, bool)],
    sec_blooms: &[(usize, Vec<u8>, bool)],
    trigram_items: &[(usize, Vec<u8>)],
) -> Result<AssembledHotTail, StatamError> {
    let mut col_dirs = base_col_dirs.to_vec();
    let mut hot_bloom_buf = Vec::new();
    let mut hot_dict_buf = Vec::new();
    let mut lazy_data_buf = Vec::new();
    let mut lazy_entries = Vec::new();
    let mut cur_file_off = data_end_offset;

    if let Some((hot_idx, bloom_bytes, in_hot)) = primary_bloom {
        if *in_hot {
            let off = hot_bloom_buf.len() as u64;
            let len = bloom_bytes.len() as u32;
            hot_bloom_buf.extend_from_slice(bloom_bytes);
            col_dirs[*hot_idx].col_flags |= COL_FLAG_BLOOM_IN_HOT_TAIL;
            col_dirs[*hot_idx].bloom_offset = off;
            col_dirs[*hot_idx].bloom_len = len;
        } else {
            append_lazy_entry(
                LazyIndexKind::Bloom,
                *hot_idx as u32,
                bloom_bytes,
                &mut cur_file_off,
                &mut lazy_data_buf,
                &mut lazy_entries,
                &mut col_dirs[*hot_idx],
            )?;
        }
    }

    for (pidx, dict_bytes, in_hot) in dict_items {
        if *in_hot {
            let off = hot_dict_buf.len() as u64;
            let len = dict_bytes.len() as u32;
            hot_dict_buf.extend_from_slice(dict_bytes);
            col_dirs[*pidx].col_flags |= COL_FLAG_DICT_IN_HOT_TAIL;
            col_dirs[*pidx].dict_offset = off;
            col_dirs[*pidx].dict_len = len;
        } else {
            append_lazy_entry(
                LazyIndexKind::Dict,
                *pidx as u32,
                dict_bytes,
                &mut cur_file_off,
                &mut lazy_data_buf,
                &mut lazy_entries,
                &mut col_dirs[*pidx],
            )?;
        }
    }

    for (cidx, bloom_bytes, in_hot) in sec_blooms {
        if *in_hot {
            let off = hot_bloom_buf.len() as u64;
            let len = bloom_bytes.len() as u32;
            hot_bloom_buf.extend_from_slice(bloom_bytes);
            col_dirs[*cidx].col_flags |= COL_FLAG_BLOOM_IN_HOT_TAIL;
            col_dirs[*cidx].bloom_offset = off;
            col_dirs[*cidx].bloom_len = len;
        } else {
            append_lazy_entry(
                LazyIndexKind::Bloom,
                *cidx as u32,
                bloom_bytes,
                &mut cur_file_off,
                &mut lazy_data_buf,
                &mut lazy_entries,
                &mut col_dirs[*cidx],
            )?;
        }
    }

    for (cidx, sec_bytes) in trigram_items {
        append_lazy_entry(
            LazyIndexKind::NarrowTrigram,
            *cidx as u32,
            sec_bytes,
            &mut cur_file_off,
            &mut lazy_data_buf,
            &mut lazy_entries,
            &mut col_dirs[*cidx],
        )?;
        col_dirs[*cidx].col_flags |= COL_FLAG_HAS_TRIGRAM_INDEX;
    }

    let exact_raw_len = schema_bytes.len()
        + col_dirs.len() * COLUMN_DIR_ENTRY_LEN
        + rg_dir_bytes.len()
        + block_dir_bytes.len()
        + hot_bloom_buf.len()
        + hot_dict_buf.len()
        + lazy_entries.len() * LAZY_INDEX_ENTRY_LEN;
    if exact_raw_len > MAX_HOT_TAIL_BYTES {
        return Err(StatamError::BoundsExceeded {
            reason: "uncompressed hot tail exceeds MAX_HOT_TAIL_BYTES",
        });
    }

    let mut hot_tail_raw = Vec::with_capacity(exact_raw_len);
    let schema_len =
        u32::try_from(schema_bytes.len()).map_err(|_| StatamError::BoundsExceeded {
            reason: "schema_len overflows u32",
        })?;
    hot_tail_raw.extend_from_slice(schema_bytes);

    let col_dir_off = hot_tail_raw.len() as u32;
    for cd in &col_dirs {
        hot_tail_raw.extend_from_slice(&cd.encode());
    }

    let rg_dir_off = hot_tail_raw.len() as u32;
    hot_tail_raw.extend_from_slice(rg_dir_bytes);

    let block_dir_off = hot_tail_raw.len() as u32;
    hot_tail_raw.extend_from_slice(block_dir_bytes);

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

    let hot_tail_raw_len = hot_tail_raw.len() as u32;
    let (hot_tail_stored, is_lz4) = if compress_hot_tail_lz4 {
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

    Ok(AssembledHotTail {
        hot_tail_raw_len,
        hot_tail_len,
        hot_tail_stored,
        is_lz4,
        lazy_data_buf,
        cur_file_off,
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
    })
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

#[cfg(test)]
mod tests {
    use cetra_column::Field;

    use super::*;
    use crate::io::SliceReader;
    use crate::payload::pack::{ChunkPackReader, PackWriteOptions};
    use crate::reader::{
        InMemoryPackResolver, PackResolver, ScanRequest, StatamReader, SubstringQuery,
    };

    #[test]
    fn l0fast_skips_widerow_dict_probing_and_narrow_trigram_skips_dict() {
        let schema = Schema::try_new_arc(vec![
            Field::new("name", DataType::Utf8, false),
            Field::new("input_preview", DataType::Utf8, false),
            Field::new("output_preview", DataType::Utf8, false),
            Field::new("request_params", DataType::Utf8, false),
            Field::new("status_message", DataType::Utf8, false),
        ])
        .unwrap();

        // Low cardinality on all columns (only 3 distinct values across 64 rows).
        let n = 64usize;
        let vals: Vec<String> = (0..n).map(|i| format!("val_{}", i % 3)).collect();
        let refs: Vec<&str> = vals.iter().map(String::as_str).collect();
        let make_col = || {
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(refs.iter().copied()).unwrap()),
                    None,
                )
                .unwrap(),
            )
        };
        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![make_col(), make_col(), make_col(), make_col(), make_col()],
            n,
        )
        .unwrap();

        // L0Fast: `name` (Core) gets FileDict, WideRow preview/params/status_message MUST NOT.
        let mut opts_l0 = WriteOptions::for_table(TableKind::Spans, WriteProfile::L0Fast);
        opts_l0.bloom_cols.clear();
        opts_l0.hot_bloom_col = None;
        let mut l0_bytes = Vec::new();
        let mut w0 = StatamWriter::new(Arc::clone(&schema), opts_l0, None).unwrap();
        w0.write_batch(&batch).unwrap();
        w0.finish(&mut l0_bytes).unwrap();
        let r0 = StatamReader::open(Arc::new(SliceReader::new(l0_bytes)), None).unwrap();
        assert_ne!(
            r0.meta().col_dir[0].col_flags & COL_FLAG_HAS_FILE_DICT,
            0,
            "Core narrow Utf8 col `name` should get FileDict"
        );
        for wide_idx in 1..=4 {
            assert_eq!(
                r0.meta().col_dir[wide_idx].col_flags & COL_FLAG_HAS_FILE_DICT,
                0,
                "L0Fast must NOT probe FileDict on WideRow col {wide_idx}"
            );
        }

        // L1Compact: `name` is Dict-encoded -> must skip narrow trigram index!
        let mut opts_l1 = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        opts_l1.bloom_cols.clear();
        opts_l1.hot_bloom_col = None;
        let mut l1_bytes = Vec::new();
        let mut w1 = StatamWriter::new(Arc::clone(&schema), opts_l1, None).unwrap();
        w1.write_batch(&batch).unwrap();
        w1.finish(&mut l1_bytes).unwrap();
        let r1 = StatamReader::open(Arc::new(SliceReader::new(l1_bytes)), None).unwrap();
        assert_eq!(
            r1.meta().col_dir[0].col_flags & COL_FLAG_HAS_TRIGRAM_INDEX,
            0,
            "Dict-encoded `name` must not build NarrowTrigram index"
        );
    }

    #[test]
    fn w4_bypass_writes_uncompressed_raw_pack_frames_without_trigrams() {
        let schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("input", DataType::Utf8, true),
        ])
        .unwrap();

        let pack_id = [0x55u8; 16];
        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        opts.payload_cols = vec!["input".to_owned()];
        opts.pack_id = Some(pack_id);

        // 128 rows of high-entropy incompressible 2 KiB ASCII strings -> triggers W4 bypass after 64 chunks.
        let n = 128usize;
        let ids: Vec<i64> = (0..n as i64).collect();
        let mut rng = 0x1234_5678_9abc_def0u64;
        let mut payloads = Vec::with_capacity(n);
        for i in 0..n {
            let mut s = format!("W4_ROW_{i:04}_");
            for _ in 0..2048 {
                rng = rng
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                s.push((b'!' + ((rng >> 56) as u8 % 90)) as char);
            }
            payloads.push(s);
        }
        let p_refs: Vec<&str> = payloads.iter().map(String::as_str).collect();
        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(p_refs).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            n,
        )
        .unwrap();

        let pack_opts = PackWriteOptions {
            #[cfg(miri)]
            codec: Codec::Lz4,
            #[cfg(not(miri))]
            codec: Codec::Zstd,
            zstd_level: 3,
            target_frame_raw_bytes: 16 * 1024,
            enable_trigram_skip: true,
            compress_toc_lz4: true,
            write_threads: 1,
            emit_hash_table: true,
        };
        let mut pb = ChunkPackBuilder::new(pack_id, pack_opts);
        let mut file_bytes = Vec::new();
        let mut w = StatamWriter::new(Arc::clone(&schema), opts, Some(&mut pb)).unwrap();
        w.write_batch(&batch).unwrap();
        w.finish(&mut file_bytes).unwrap();
        let mut pack_bytes = Vec::new();
        pb.finish(&mut pack_bytes).unwrap();

        let pack_reader =
            Arc::new(ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes))).unwrap());
        let bypass_frames: Vec<_> = pack_reader
            .toc()
            .frames
            .iter()
            .filter(|f| f.filter_len == 0)
            .collect();
        assert!(
            !bypass_frames.is_empty(),
            "W4 bypass must omit trigram blooms (filter_len == 0)"
        );

        // Also verify append_raw_bypass falls back to Codec::None on truly incompressible bytes.
        let mut raw_pb = ChunkPackBuilder::new([0x66u8; 16], pack_opts);
        let mut incomp = vec![0u8; 20 * 1024];
        let mut s = 0x9e37_79b9_7f4a_7c15u64;
        for b in &mut incomp {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            *b = (s >> 24) as u8;
        }
        raw_pb.append_raw_bypass(&incomp).unwrap();
        let mut raw_pack_bytes = Vec::new();
        raw_pb.finish(&mut raw_pack_bytes).unwrap();
        let raw_pack_reader =
            ChunkPackReader::open(Arc::new(SliceReader::new(raw_pack_bytes))).unwrap();
        assert_eq!(raw_pack_reader.toc().frames[0].codec, Codec::None);
        assert_eq!(raw_pack_reader.toc().frames[0].filter_len, 0);

        let mut resolver = InMemoryPackResolver::new();
        resolver.insert(pack_id, pack_reader);
        let resolver: Arc<dyn PackResolver> = Arc::new(resolver);
        let reader = StatamReader::open(
            Arc::new(SliceReader::new(file_bytes)),
            Some(Arc::clone(&resolver)),
        )
        .unwrap();
        reader.verify_full().unwrap();
        let out = reader.scan(&ScanRequest::default()).unwrap();
        assert_eq!(out[0].num_rows(), n);
        assert_eq!(
            out[0].column(1).unwrap().get_scalar(120).unwrap(),
            ScalarValue::Utf8(payloads[120].clone())
        );
    }

    #[test]
    fn hot_tail_stays_within_1mib_and_keeps_small_dicts_when_compressed_fits() {
        // Test with small RG/MB sizes producing thousands of blocks + primary/secondary blooms + small dict.
        let schema = Schema::try_new_arc(vec![
            Field::new("trace_id", DataType::FixedBytes16, false),
            Field::new("span_id", DataType::FixedBytes8, false),
            Field::new("status_code", DataType::Utf8, false),
        ])
        .unwrap();

        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L0Fast);
        opts.row_group_size = 64;
        opts.mini_block_size = 32;

        // 8,192 rows -> 128 RGs * 2 MBs * 3 cols = 768 blocks (~49 KiB raw block_dir)
        // + unique trace_id/span_id blooms + small status_code FileDict.
        let n = 8_192usize;
        let tids: Vec<[u8; 16]> = (0..n)
            .map(|i| {
                let mut b = [0u8; 16];
                b[0..8].copy_from_slice(&(i as u64).to_le_bytes());
                b
            })
            .collect();
        let sids: Vec<[u8; 8]> = (0..n).map(|i| (i as u64).to_be_bytes()).collect();
        let statuses: Vec<&str> = (0..n)
            .map(|i| if i % 2 == 0 { "OK" } else { "ERR" })
            .collect();

        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(tids), None).unwrap()),
                Column::Flat(FlatColumn::try_new(FlatData::FixedBytes8(sids), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(statuses).unwrap()),
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
        let stats = w.finish(&mut file_bytes).unwrap();
        assert!(
            stats.hot_tail_bytes <= DEFAULT_TAIL_HINT,
            "hot_tail_bytes {} exceeded 128 KiB DEFAULT_TAIL_HINT",
            stats.hot_tail_bytes
        );

        let reader = StatamReader::open(Arc::new(SliceReader::new(file_bytes)), None).unwrap();
        reader.verify_full().unwrap();
        assert_ne!(
            reader.meta().col_dir[0].col_flags & COL_FLAG_BLOOM_IN_HOT_TAIL,
            0
        );
        assert_ne!(
            reader.meta().col_dir[2].col_flags & COL_FLAG_DICT_IN_HOT_TAIL,
            0,
            "small FileDict must remain in Hot Tail when compressed Hot Tail <= 120 KiB"
        );
    }

    #[test]
    fn write_batch_with_empty_prechunked_after_partial_rg_preserves_external_payloads() {
        use crate::payload::pack::PackWriteOptions;
        use crate::reader::{InMemoryPackResolver, PackResolver};

        let schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("input", DataType::Utf8, true),
        ])
        .unwrap();

        let make_batch = |ids: &[i64], payloads: &[&str]| {
            Batch::try_new_all(
                Arc::clone(&schema),
                vec![
                    Column::Flat(FlatColumn::try_new(FlatData::I64(ids.to_vec()), None).unwrap()),
                    Column::Flat(
                        FlatColumn::try_new(
                            FlatData::Utf8(
                                Utf8Data::try_from_slices(payloads.iter().copied()).unwrap(),
                            ),
                            None,
                        )
                        .unwrap(),
                    ),
                ],
                ids.len(),
            )
            .unwrap()
        };

        let p0 = format!("partial_l0_0_{}", "a".repeat(600));
        let p1 = "inline_1".to_owned();
        let p2 = format!("prechunked_empty_2_{}", "b".repeat(700));
        let p3 = format!("prechunked_empty_3_{}", "c".repeat(800));
        let b1 = make_batch(&[0, 1], &[&p0, &p1]);
        let b2 = make_batch(&[2, 3], &[&p2, &p3]);
        let expected = [p0, p1, p2, p3];

        // 1. L0Fast: partial RG via write_batch + write_batch_with_prechunked_refs with empty map.
        let mut l0_opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L0Fast);
        l0_opts.payload_cols = vec!["input".to_owned()];
        l0_opts.row_group_size = 8;
        l0_opts.mini_block_size = 4;

        let mut l0_bytes = Vec::new();
        let mut w0 = StatamWriter::new(Arc::clone(&schema), l0_opts, None).unwrap();
        w0.write_batch(&b1).unwrap();
        w0.write_batch_with_prechunked_refs(&b2, &HashMap::new(), &HashMap::new())
            .unwrap();
        w0.finish(&mut l0_bytes).unwrap();

        let r0 = StatamReader::open(Arc::new(SliceReader::new(l0_bytes)), None).unwrap();
        r0.verify_full().unwrap();
        let out0 = r0.scan(&ScanRequest::default()).unwrap();
        assert_eq!(out0.len(), 1);
        assert_eq!(out0[0].num_rows(), 4);
        for (idx, exp) in expected.iter().enumerate() {
            assert_eq!(
                out0[0].column(1).unwrap().get_scalar(idx).unwrap(),
                ScalarValue::Utf8(exp.clone())
            );
        }

        // 2. L1Compact with ChunkPackBuilder: partial RG via write_batch + empty prechunked.
        let pack_id = [0x5Au8; 16];
        let mut pb = ChunkPackBuilder::new(
            pack_id,
            PackWriteOptions {
                codec: default_l1_codec(),
                zstd_level: 3,
                target_frame_raw_bytes: 16 * 1024,
                enable_trigram_skip: true,
                compress_toc_lz4: true,
                write_threads: 1,
                emit_hash_table: true,
            },
        );
        let mut l1_opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        l1_opts.payload_cols = vec!["input".to_owned()];
        l1_opts.row_group_size = 8;
        l1_opts.mini_block_size = 4;
        l1_opts.pack_id = Some(pack_id);

        let mut l1_bytes = Vec::new();
        {
            let mut w1 = StatamWriter::new(Arc::clone(&schema), l1_opts, Some(&mut pb)).unwrap();
            w1.write_batch(&b1).unwrap();
            w1.write_batch_with_prechunked_refs(&b2, &HashMap::new(), &HashMap::new())
                .unwrap();
            w1.finish(&mut l1_bytes).unwrap();
        }
        let mut pack_bytes = Vec::new();
        pb.finish(&mut pack_bytes).unwrap();
        let mut resolver = InMemoryPackResolver::new();
        resolver.insert_bytes(pack_id, pack_bytes).unwrap();
        let resolver: Arc<dyn PackResolver> = Arc::new(resolver);

        let r1 = StatamReader::open(Arc::new(SliceReader::new(l1_bytes)), Some(resolver)).unwrap();
        r1.verify_full().unwrap();
        let out1 = r1.scan(&ScanRequest::default()).unwrap();
        assert_eq!(out1.len(), 1);
        assert_eq!(out1[0].num_rows(), 4);
        for (idx, exp) in expected.iter().enumerate() {
            assert_eq!(
                out1[0].column(1).unwrap().get_scalar(idx).unwrap(),
                ScalarValue::Utf8(exp.clone())
            );
        }
    }

    #[test]
    fn wide_row_file_dict_caps_entries_and_memory_bytes() {
        use crate::format::COL_FLAG_HAS_FILE_DICT;

        let schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("input_preview", DataType::Utf8, true),
            Field::new("output_preview", DataType::Utf8, true),
        ])
        .unwrap();

        let n = 2_048usize;
        let ids: Vec<i64> = (0..n as i64).collect();
        // 512 unique ~600 B previews (~307 KiB > 256 KiB DICT_PASSTHROUGH_MAX_BYTES): must NOT build a FileDict.
        let large_previews: Vec<String> = (0..n)
            .map(|i| format!("preview_{:04}_{}", i % 512, "x".repeat(590)))
            .collect();
        // 64 unique ~16 B previews (~1 KiB <= 256 KiB cap): MAY build a FileDict.
        let small_previews: Vec<String> =
            (0..n).map(|i| format!("short_{:02}_ok", i % 64)).collect();

        let large_refs: Vec<&str> = large_previews.iter().map(String::as_str).collect();
        let small_refs: Vec<&str> = small_previews.iter().map(String::as_str).collect();
        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(large_refs).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(small_refs).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            n,
        )
        .unwrap();

        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        opts.row_group_size = n;
        opts.mini_block_size = 512;

        let mut file_bytes = Vec::new();
        let mut w = StatamWriter::new(Arc::clone(&schema), opts, None).unwrap();
        w.write_batch(&batch).unwrap();
        w.finish(&mut file_bytes).unwrap();

        let reader = StatamReader::open(Arc::new(SliceReader::new(file_bytes)), None).unwrap();
        assert_eq!(
            reader.meta().col_dir[1].col_flags & COL_FLAG_HAS_FILE_DICT,
            0,
            "WideRow column > 256 KiB (DICT_PASSTHROUGH_MAX_BYTES) dict memory must not build a FileDict"
        );
        assert_ne!(
            reader.meta().col_dir[2].col_flags & COL_FLAG_HAS_FILE_DICT,
            0,
            "compact WideRow column <= 1024 entries and <= 256 KiB may keep a FileDict"
        );
    }

    fn make_mt_spans_batch(n: usize) -> (Arc<Schema>, Batch) {
        use cetra_column::MapData;

        let schema = Schema::try_new_arc(vec![
            Field::new("status", DataType::Utf8, false),
            Field::new("model", DataType::Utf8, true),
            Field::new("name", DataType::Utf8, false),
            Field::new("start_time", DataType::TimestampNs { utc: true }, false),
            Field::new("metadata", DataType::MapUtf8, true),
            Field::new("trace_id", DataType::FixedBytes16, false),
            Field::new("cost_total", DataType::F64, false),
            Field::new("total_tokens", DataType::I64, false),
            Field::new("span_id", DataType::FixedBytes8, false),
            Field::new("user_id", DataType::Utf8, true),
            Field::new("session_id", DataType::Utf8, true),
            Field::new("duration_ns", DataType::I64, false),
            Field::new("provider", DataType::Utf8, false),
            Field::new("attributes", DataType::MapUtf8, true),
            Field::new("prompt_tokens", DataType::I64, false),
            Field::new("input_preview", DataType::Utf8, true),
            Field::new("all_null_col", DataType::Utf8, true),
            Field::new("const_str_col", DataType::Utf8, false),
            Field::new("input", DataType::Utf8, true),
            Field::new("output", DataType::Utf8, true),
        ])
        .unwrap();

        let statuses: Vec<&str> = (0..n)
            .map(|i| if i % 5 == 0 { "ERR" } else { "OK" })
            .collect();
        let models: Vec<&str> = (0..n).map(|i| ["m-a", "m-b", "m-c"][i % 3]).collect();
        // High-cardinality unique `name` so FileDict is skipped and narrow_trigram_cols activates.
        let names: Vec<String> = (0..n)
            .map(|i| format!("span-op-{i:05}-{}", "x".repeat(24)))
            .collect();
        let start_times: Vec<i64> = (0..n)
            .map(|i| 1_700_000_000_000_000_000 + (i as i64) * 1_000)
            .collect();

        let build_map = |prefix: &str| -> Column {
            let mut offsets = Vec::with_capacity(n + 1);
            offsets.push(0u32);
            let mut keys = Vec::new();
            let mut vals = Vec::new();
            for i in 0..n {
                keys.push(format!("{prefix}.env"));
                vals.push(if i % 2 == 0 {
                    "prod".to_owned()
                } else {
                    "dev".to_owned()
                });
                keys.push(format!("{prefix}.region"));
                vals.push(format!("r{}", i % 4));
                if i % 10 == 0 {
                    keys.push(format!("{prefix}.rare_{i}"));
                    vals.push(format!("v_{i}"));
                }
                offsets.push(keys.len() as u32);
            }
            let kd = Utf8Data::try_from_slices(keys.iter().map(String::as_str)).unwrap();
            let vd = Utf8Data::try_from_slices(vals.iter().map(String::as_str)).unwrap();
            Column::Flat(
                FlatColumn::try_new(
                    FlatData::MapUtf8(Box::new(MapData::try_new(offsets, kd, vd, None).unwrap())),
                    None,
                )
                .unwrap(),
            )
        };

        let trace_ids: Vec<[u8; 16]> = (0..n)
            .map(|i| {
                let mut b = [0u8; 16];
                b[0..8].copy_from_slice(&((i / 4) as u64).to_le_bytes());
                b
            })
            .collect();
        let costs: Vec<f64> = (0..n).map(|i| (i as f64) * 0.001).collect();
        let tokens: Vec<i64> = (0..n).map(|i| 100 + (i as i64 % 500)).collect();
        let span_ids: Vec<[u8; 8]> = (0..n).map(|i| (i as u64).to_be_bytes()).collect();
        let users: Vec<String> = (0..n).map(|i| format!("u_{:02}", i % 16)).collect();
        let sessions: Vec<String> = (0..n).map(|i| format!("s_{:02}", i % 32)).collect();
        let durations: Vec<i64> = (0..n).map(|i| 10_000 + (i as i64 * 7)).collect();
        let providers: Vec<&str> = (0..n).map(|i| ["openai", "anthropic"][i % 2]).collect();
        let prompt_tokens: Vec<i64> = (0..n).map(|i| 50 + (i as i64 % 200)).collect();
        let previews: Vec<String> = (0..n)
            .map(|i| format!("preview-{i:04}-{}", "abc ".repeat(16)))
            .collect();
        let inputs: Vec<String> = (0..n)
            .map(|i| {
                format!(
                    "[{{\"role\":\"user\",\"content\":\"msg-{i:04}-{}\"}}]",
                    "Z".repeat(600)
                )
            })
            .collect();
        let outputs: Vec<String> = (0..n)
            .map(|i| format!("reply-{i:04}-{}", "W".repeat(400)))
            .collect();

        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(statuses).unwrap()),
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
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(names.iter().map(String::as_str)).unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
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
                build_map("m"),
                Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(trace_ids), None).unwrap()),
                Column::Flat(FlatColumn::try_new(FlatData::F64(costs), None).unwrap()),
                Column::Flat(FlatColumn::try_new(FlatData::I64(tokens), None).unwrap()),
                Column::Flat(FlatColumn::try_new(FlatData::FixedBytes8(span_ids), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(users.iter().map(String::as_str)).unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(sessions.iter().map(String::as_str)).unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(FlatColumn::try_new(FlatData::I64(durations), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(providers).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
                build_map("a"),
                Column::Flat(FlatColumn::try_new(FlatData::I64(prompt_tokens), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(previews.iter().map(String::as_str)).unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
                Column::try_new_const(ScalarValue::Null(DataType::Utf8), n).unwrap(),
                Column::try_new_const(ScalarValue::Utf8("constant_tag".to_owned()), n).unwrap(),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(inputs.iter().map(String::as_str)).unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(outputs.iter().map(String::as_str)).unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            n,
        )
        .unwrap();
        (schema, batch)
    }

    #[test]
    fn writer_l1_multithreaded_is_bit_exact_with_single_threaded() {
        let total_n = 1_250usize;
        let (schema, batch) = make_mt_spans_batch(total_n);
        let pack_id = [0xC1u8; 16];

        let run = |threads: usize,
                   src: &Batch,
                   split_at: Option<usize>|
         -> (Vec<u8>, Vec<u8>, WriteStats) {
            let pack_opts = PackWriteOptions {
                write_threads: threads,
                target_frame_raw_bytes: 32 * 1024,
                ..PackWriteOptions::default()
            };
            let mut pb = ChunkPackBuilder::new(pack_id, pack_opts);
            let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
            opts.row_group_size = 1_024;
            opts.mini_block_size = 128;
            opts.pack_id = Some(pack_id);
            opts.write_threads = threads;

            let mut statam_bytes = Vec::new();
            let stats = {
                let mut w = StatamWriter::new(Arc::clone(&schema), opts, Some(&mut pb)).unwrap();
                if let Some(mid) = split_at {
                    w.write_batch(&src.slice(0, mid).unwrap()).unwrap();
                    w.write_batch(&src.slice(mid, src.num_rows() - mid).unwrap())
                        .unwrap();
                } else {
                    w.write_batch(src).unwrap();
                }
                w.finish(&mut statam_bytes).unwrap()
            };
            let mut pack_bytes = Vec::new();
            pb.finish(&mut pack_bytes).unwrap();
            (statam_bytes, pack_bytes, stats)
        };

        let (s_0t, p_0t, st_0t) = run(0, &batch, Some(300));
        let (s_1t, p_1t, st_1t) = run(1, &batch, Some(300));
        let (s_4t, p_4t, st_4t) = run(4, &batch, Some(300));
        assert_eq!(st_1t, st_0t);
        assert_eq!(s_1t, s_0t, ".statam L1 output must be bit-exact 1T vs 0T");
        assert_eq!(p_1t, p_0t, ".stpk output must be bit-exact 1T vs 0T");
        assert_eq!(st_1t, st_4t);
        assert_eq!(s_1t, s_4t, ".statam L1 output must be bit-exact 1T vs 4T");
        assert_eq!(p_1t, p_4t, ".stpk output must be bit-exact 1T vs 4T");

        // 1-row tiny-flush parity check with write_threads = 8 (gated to 1T RG path).
        let one_row = batch.slice(0, 1).unwrap();
        let (one_s1, one_p1, one_st1) = run(1, &one_row, None);
        let (one_s8, one_p8, one_st8) = run(8, &one_row, None);
        assert_eq!(one_st1, one_st8);
        assert_eq!(one_s1, one_s8, "1-row L1 output must be bit-exact 1T vs 8T");
        assert_eq!(one_p1, one_p8, "1-row L1 .stpk must be bit-exact 1T vs 8T");

        let pack_reader =
            Arc::new(ChunkPackReader::open(Arc::new(SliceReader::new(p_4t))).unwrap());
        let mut resolver = InMemoryPackResolver::new();
        resolver.insert(pack_id, pack_reader);
        let reader =
            StatamReader::open(Arc::new(SliceReader::new(s_4t)), Some(Arc::new(resolver))).unwrap();
        reader.verify_full().unwrap();
        assert!(reader.meta().rg_dir.len() >= 2);
        assert!(reader.meta().rg_dir[0].row_count >= MIN_PARALLEL_RG_ROWS as u32);
        assert!(!reader.meta().schema_meta.shredded_maps.is_empty());
        let scanned = reader.scan(&ScanRequest::default()).unwrap();
        let total_rows: usize = scanned.iter().map(Batch::num_rows).sum();
        assert_eq!(total_rows, total_n);
    }

    #[test]
    fn writer_l0_multithreaded_is_bit_exact_with_single_threaded() {
        let schema = Schema::try_new_arc(vec![
            Field::new("trace_id", DataType::FixedBytes16, false),
            Field::new("status", DataType::Utf8, false),
            Field::new("duration_ns", DataType::I64, false),
            Field::new("input_preview", DataType::Utf8, true),
            Field::new("input", DataType::Utf8, true),
        ])
        .unwrap();

        // Build >= 1_024 rows and > 3 MiB of L0 local payloads so RG0 exercises
        // parallel RG encoding and LocalL0PayloadBuilder queues >= 6 frames.
        let n = 1_250usize;
        let tids: Vec<[u8; 16]> = (0..n)
            .map(|i| {
                let mut b = [0u8; 16];
                b[0..8].copy_from_slice(&(i as u64).to_le_bytes());
                b
            })
            .collect();
        let statuses: Vec<&str> = (0..n)
            .map(|i| if i % 3 == 0 { "ERR" } else { "OK" })
            .collect();
        let durs: Vec<i64> = (0..n).map(|i| 1000 + i as i64 * 17).collect();
        let previews: Vec<String> = (0..n).map(|i| format!("prev-{i:04}")).collect();
        let inputs: Vec<String> = (0..n)
            .map(|i| {
                format!(
                    "{{\"idx\":{i:04},\"data\":\"{}\"}}",
                    "payload_chunk_".repeat(200)
                )
            })
            .collect();

        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(tids), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(statuses).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(FlatColumn::try_new(FlatData::I64(durs), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(previews.iter().map(String::as_str)).unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(inputs.iter().map(String::as_str)).unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            n,
        )
        .unwrap();

        let run = |threads: usize, src: &Batch, split_at: Option<usize>| -> (Vec<u8>, WriteStats) {
            let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L0Fast);
            opts.row_group_size = 1_024;
            opts.mini_block_size = 128;
            opts.write_threads = threads;
            let mut out = Vec::new();
            let mut w = StatamWriter::new(Arc::clone(&schema), opts, None).unwrap();
            if let Some(mid) = split_at {
                w.write_batch(&src.slice(0, mid).unwrap()).unwrap();
                w.write_batch(&src.slice(mid, src.num_rows() - mid).unwrap())
                    .unwrap();
            } else {
                w.write_batch(src).unwrap();
            }
            let stats = w.finish(&mut out).unwrap();
            (out, stats)
        };

        let (bytes_0t, stats_0t) = run(0, &batch, Some(300));
        let (bytes_1t, stats_1t) = run(1, &batch, Some(300));
        let (bytes_4t, stats_4t) = run(4, &batch, Some(300));
        assert_eq!(stats_1t, stats_0t);
        assert_eq!(
            bytes_1t, bytes_0t,
            ".statam L0 output must be bit-exact 1T vs 0T"
        );
        assert_eq!(stats_1t, stats_4t);
        assert!(stats_4t.l0_payload_bytes > 0);
        assert_eq!(
            bytes_1t, bytes_4t,
            ".statam L0 output with multi-frame L0 local payloads must be bit-exact 1T vs 4T"
        );

        // 1-row tiny-flush parity check with write_threads = 8 (gated to 1T RG path).
        let one_row = batch.slice(0, 1).unwrap();
        let (one_1t, one_s1) = run(1, &one_row, None);
        let (one_8t, one_s8) = run(8, &one_row, None);
        assert_eq!(one_s1, one_s8);
        assert_eq!(one_1t, one_8t, "1-row L0 output must be bit-exact 1T vs 8T");

        let reader = StatamReader::open(Arc::new(SliceReader::new(bytes_4t)), None).unwrap();
        reader.verify_full().unwrap();
        assert!(reader.meta().rg_dir.len() >= 2);
        assert!(reader.meta().rg_dir[0].row_count >= MIN_PARALLEL_RG_ROWS as u32);
        let scanned = reader.scan(&ScanRequest::default()).unwrap();
        let total_rows: usize = scanned.iter().map(Batch::num_rows).sum();
        assert_eq!(total_rows, n);
        assert_eq!(
            scanned[0].column(4).unwrap().get_scalar(0).unwrap(),
            ScalarValue::Utf8(inputs[0].clone())
        );
    }

    #[test]
    fn writer_single_chunk_only_omits_edge_table_and_preserves_substring_parity() {
        let schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("input", DataType::Utf8, true),
        ])
        .unwrap();

        let n = 64usize;
        let ids: Vec<i64> = (0..n as i64).collect();
        let payloads: Vec<String> = (0..n)
            .map(|i| {
                let marker = if i % 16 == 5 {
                    "NeedleSingleAlpha"
                } else {
                    "OtherSingleText"
                };
                format!(
                    "{{\"row\":{i:04},\"tag\":\"{marker}\",\"pad\":\"{}\"}}",
                    "x".repeat(420)
                )
            })
            .collect();
        let p_refs: Vec<&str> = payloads.iter().map(String::as_str).collect();
        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(p_refs).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            n,
        )
        .unwrap();

        let pack_id = [0x71u8; 16];
        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        assert_eq!(opts.mini_block_size, 8_192);
        opts.pack_id = Some(pack_id);
        opts.payload_cols = vec!["input".to_owned()];

        let mut pb = ChunkPackBuilder::new(pack_id, PackWriteOptions::default());
        let mut statam_bytes = Vec::new();
        {
            let mut w = StatamWriter::new(Arc::clone(&schema), opts, Some(&mut pb)).unwrap();
            w.write_batch(&batch).unwrap();
            w.finish(&mut statam_bytes).unwrap();
        }
        let mut pack_bytes = Vec::new();
        let pack_stats = pb.finish(&mut pack_bytes).unwrap();
        assert_eq!(pack_stats.edge_table_bytes, 0);

        let pack_reader =
            Arc::new(ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes))).unwrap());
        assert_eq!(pack_reader.postscript().edge_table_len, 0);

        let mut resolver = InMemoryPackResolver::new();
        resolver.insert(pack_id, pack_reader);
        let reader = StatamReader::open(
            Arc::new(SliceReader::new(statam_bytes)),
            Some(Arc::new(resolver)),
        )
        .unwrap();
        reader.verify_full().unwrap();

        let like_hits = reader
            .scan(&ScanRequest {
                substrings: vec![SubstringQuery {
                    column: "input".to_owned(),
                    needle: b"NeedleSingleAlpha".to_vec(),
                    case_insensitive: false,
                }],
                ..ScanRequest::default()
            })
            .unwrap();
        let like_rows: usize = like_hits.iter().map(Batch::num_rows).sum();
        assert_eq!(like_rows, 4);

        let ilike_hits = reader
            .scan(&ScanRequest {
                substrings: vec![SubstringQuery {
                    column: "input".to_owned(),
                    needle: b"needlesinglealpha".to_vec(),
                    case_insensitive: true,
                }],
                ..ScanRequest::default()
            })
            .unwrap();
        let ilike_rows: usize = ilike_hits.iter().map(Batch::num_rows).sum();
        assert_eq!(ilike_rows, 4);

        let miss_hits = reader
            .scan(&ScanRequest {
                substrings: vec![SubstringQuery {
                    column: "input".to_owned(),
                    needle: b"NotPresentAnywhere".to_vec(),
                    case_insensitive: false,
                }],
                ..ScanRequest::default()
            })
            .unwrap();
        assert_eq!(miss_hits.iter().map(Batch::num_rows).sum::<usize>(), 0);
    }

    #[test]
    fn writer_mixed_single_and_multi_chunk_zeros_non_boundary_edges_and_matches_seams() {
        let schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("input", DataType::Utf8, true),
        ])
        .unwrap();

        let n = 100usize;
        let ids: Vec<i64> = (0..n as i64).collect();
        let mut payloads = Vec::with_capacity(n);
        for i in 0..96usize {
            payloads.push(format!(
                "{{\"single_id\":{i:04},\"marker\":\"ONLY_SINGLE_{i:04}\",\"pad\":\"{}\"}}",
                "a".repeat(500)
            ));
        }
        // Multi-chunk JSON array rows (including continuation) with a seam-spanning needle:
        // element 0 ends with `SEAM_LEFT`, element 1 starts with `SEAM_RIGHT`, so the seam
        // contains `SEAM_LEFT"},{"role":"assistant","txt":"SEAM_RIGHT`.
        let json_prefix = format!(
            "[{{\"role\":\"user\",\"txt\":\"{}SEAM_LEFT\"}}",
            "u".repeat(300)
        );
        payloads.push(format!(
            "{json_prefix},{{\"role\":\"assistant\",\"txt\":\"SEAM_RIGHT{}\"}}]",
            "v".repeat(300)
        ));
        payloads.push(format!(
            "{json_prefix},{{\"role\":\"assistant\",\"txt\":\"SEAM_RIGHT_CONT{}\"}}]",
            "w".repeat(300)
        ));
        // Also add > MAX_CHUNK (~80 KiB) plain-text multi-chunk rows spanning CDC seams.
        let big_a = format!("{}BIG_CDC_PART_A", "c".repeat(45_000));
        let big_b = format!("BIG_CDC_PART_B{}", "d".repeat(45_000));
        payloads.push(format!("{big_a}{big_b}"));
        payloads.push(format!(
            "{{\"single_tail\":true,\"pad\":\"{}\"}}",
            "z".repeat(450)
        ));

        let p_refs: Vec<&str> = payloads.iter().map(String::as_str).collect();
        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(p_refs).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            n,
        )
        .unwrap();

        let pack_id = [0x72u8; 16];
        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        opts.pack_id = Some(pack_id);
        opts.payload_cols = vec!["input".to_owned()];

        let mut pb = ChunkPackBuilder::new(pack_id, PackWriteOptions::default());
        let mut statam_bytes = Vec::new();
        {
            let mut w = StatamWriter::new(Arc::clone(&schema), opts, Some(&mut pb)).unwrap();
            w.write_batch(&batch).unwrap();
            w.finish(&mut statam_bytes).unwrap();
        }
        let mut pack_bytes = Vec::new();
        let pack_stats = pb.finish(&mut pack_bytes).unwrap();
        assert!(pack_stats.edge_table_bytes > 0);

        let pack_reader =
            Arc::new(ChunkPackReader::open(Arc::new(SliceReader::new(pack_bytes))).unwrap());
        let flat_edges = pack_reader.read_edge_table_flat().unwrap();
        assert_eq!(flat_edges.len(), pack_stats.chunk_count as usize);

        // First 96 chunks belong exclusively to single-chunk rows -> edges must be zeroed.
        let mut zeroed_count = 0usize;
        let mut non_zero_count = 0usize;
        for cid in 0..pack_stats.chunk_count {
            if flat_edges.head(cid).is_empty() && flat_edges.tail(cid).is_empty() {
                zeroed_count += 1;
            } else {
                non_zero_count += 1;
            }
        }
        assert!(
            zeroed_count >= 96,
            "expected >= 96 zeroed non-boundary chunks, got {zeroed_count}"
        );
        assert!(
            non_zero_count >= 4,
            "expected boundary chunks to keep non-empty edges"
        );

        // Compare edge_table_bytes against a pack with full untracked edges for the same chunks.
        let mut baseline_pb = ChunkPackBuilder::new(pack_id, PackWriteOptions::default());
        for cid in 0..pack_stats.chunk_count {
            let chunk = pack_reader.read_chunks(&[cid]).unwrap();
            baseline_pb.intern_chunk(&chunk[&cid]).unwrap();
        }
        let mut baseline_bytes = Vec::new();
        let baseline_stats = baseline_pb.finish(&mut baseline_bytes).unwrap();
        assert!(
            pack_stats.edge_table_bytes < baseline_stats.edge_table_bytes,
            "tracked mixed edge_table_bytes ({}) should be smaller than untracked ({})",
            pack_stats.edge_table_bytes,
            baseline_stats.edge_table_bytes
        );

        let mut resolver = InMemoryPackResolver::new();
        resolver.insert(pack_id, pack_reader);
        let reader = StatamReader::open(
            Arc::new(SliceReader::new(statam_bytes)),
            Some(Arc::new(resolver)),
        )
        .unwrap();
        reader.verify_full().unwrap();

        // Seam-spanning LIKE & ILIKE across JSON array element boundaries must match rows 96 & 97.
        let seam_needle = b"SEAM_LEFT\"},{\"role\":\"assistant\",\"txt\":\"SEAM_RIGHT";
        let seam_like = reader
            .scan(&ScanRequest {
                substrings: vec![SubstringQuery {
                    column: "input".to_owned(),
                    needle: seam_needle.to_vec(),
                    case_insensitive: false,
                }],
                ..ScanRequest::default()
            })
            .unwrap();
        assert_eq!(seam_like.iter().map(Batch::num_rows).sum::<usize>(), 2);

        let seam_ilike_needle = b"seam_left\"},{\"ROLE\":\"ASSISTANT\",\"txt\":\"seam_right";
        let seam_ilike = reader
            .scan(&ScanRequest {
                substrings: vec![SubstringQuery {
                    column: "input".to_owned(),
                    needle: seam_ilike_needle.to_vec(),
                    case_insensitive: true,
                }],
                ..ScanRequest::default()
            })
            .unwrap();
        assert_eq!(seam_ilike.iter().map(Batch::num_rows).sum::<usize>(), 2);

        // Large multi-chunk row (row 98) must match at both interior and across any CDC boundary.
        let big_like = reader
            .scan(&ScanRequest {
                substrings: vec![SubstringQuery {
                    column: "input".to_owned(),
                    needle: b"BIG_CDC_PART_ABIG_CDC_PART_B".to_vec(),
                    case_insensitive: false,
                }],
                ..ScanRequest::default()
            })
            .unwrap();
        assert_eq!(big_like.iter().map(Batch::num_rows).sum::<usize>(), 1);
    }
}
