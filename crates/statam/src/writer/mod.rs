//! Streaming PAX Row-Group writer for `.statam` files (`L0Fast` & `L1Compact`).

mod concat;
mod l0_payload;
mod rg_encode;
mod tail;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{Seek, SeekFrom, Write};
use std::sync::Arc;

use cetra_column::{Batch, Column, DataType, FlatColumn, FlatData, ScalarValue, Schema};

use crate::StatamError;
use crate::codec::WriteProfile;
use crate::codec::dict::FileDict;
use crate::format::{
    BLOCK_ENTRY_LEN, BLOCK_FRAME_HEADER_LEN, BlockEntry, CHECKSUM_ALG_CRC32C, COL_FLAG_DICT_SORTED,
    COL_FLAG_HAS_FILE_DICT, COL_FLAG_IS_PAYLOAD_REFS, COL_FLAG_IS_SHREDDED_SUBCOL,
    COLUMN_DIR_ENTRY_LEN, Codec, ColumnDirEntry, DEFAULT_TAIL_HINT, FILE_HEADER_LEN,
    FLAG_HAS_PACK_REFS, FLAG_HAS_SHREDDED_MAPS, FLAG_HAS_TRIGRAM_INDEX, FLAG_HOT_TAIL_LZ4,
    FLAG_L1_COMPACT, FORMAT_VERSION, FileHeader, FileSchemaMeta, LAZY_INDEX_ENTRY_LEN,
    LocalityGroup, MAX_BATCH_DECODED_BYTES, MAX_COLUMNS, MAX_FRAME_RAW_BYTES, MAX_MINI_BLOCK_SIZE,
    MAX_PACK_REFS, MAX_ROW_GROUP_SIZE, MAX_ROW_PAYLOAD_BYTES, MAX_TOTAL_BLOCKS, MINI_BLOCK_SIZE,
    POSTSCRIPT_LEN, Postscript, ROW_GROUP_ENTRY_LEN, ROW_GROUP_SIZE, RowGroupEntry, ShreddedMapDir,
    TableKind, encode_schema,
};
use crate::payload::chunker::{DedupBypassDetector, INLINE_PAYLOAD_THRESHOLD};
use crate::payload::pack::ChunkPackBuilder;
use crate::payload::{
    PayloadRefRow, encode_payload_refs, encode_payload_refs_l0_direct,
    encode_payload_refs_l0_from_bytes,
};
use crate::simd::bloom::Sbbf;

use concat::{
    StoredProbeScratch, concat_rg_column, estimate_column_slice_bytes,
    estimate_column_slice_materialized_bytes, estimate_column_slice_stored_bytes,
};
pub(crate) use concat::{concat_column_refs, concat_flat_columns, estimate_column_bytes};
pub use l0_payload::L0_LOCAL_PACK_IDX;
pub(crate) use l0_payload::L0_PAYLOAD_DIR_MAGIC;
use l0_payload::LocalL0PayloadBuilder;
use rg_encode::as_flat_cow;
pub(crate) use tail::NARROW_TRIGRAM_MAGIC;
use tail::{
    HOT_TAIL_HARD_BUDGET_BYTES, assemble_hot_tail_and_lazy, decide_hot_tail_placement,
    encode_narrow_trigram_section,
};

/// Default maximum encoded/stored bytes per PAX Row Group before auto-flushing (64 MiB).
const DEFAULT_MAX_ROW_GROUP_BYTES: usize = 64 * 1024 * 1024;
/// Hard cap on buffered materialized raw non-payload bytes per RG (252 MiB), leaving 4 MiB
/// headroom below `MAX_BATCH_DECODED_BYTES` (256 MiB) for block framing headers even on
/// `Codec::None` fallback.
const MAX_RG_RAW_BUFFER_BYTES: usize = MAX_BATCH_DECODED_BYTES - 4 * 1024 * 1024;
/// Conservative per-column per-mini-block materialized raw byte cap (48 MiB), leaving 16 MiB
/// headroom below `MAX_BLOCK_RAW_BYTES` (64 MiB) for nested headers, validity bitmaps, and
/// dict/FSST decode buffers.
const MAX_COL_MB_RAW_BYTES: usize = 48 * 1024 * 1024;

type PrechunkedRefs<'a> = (
    &'a HashMap<usize, Vec<PayloadRefRow>>,
    Option<&'a mut crate::reader::StreamingL0Payload>,
);
type Rg0DictResult = (FileDict, Vec<u32>, Option<cetra_column::Bitmap>);

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
#[derive(Clone)]
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
    /// Heavy `Utf8` or `Binary` column names stored as `ExternalPayload` (`PayloadRefs`).
    pub payload_cols: Vec<String>,
    /// Narrow `Utf8` column names receiving per-block `TrigramBloom` in `L1Compact` when not dict-encoded.
    pub narrow_trigram_cols: Vec<String>,
    /// Rows per PAX Row Group (default: `131_072`).
    pub row_group_size: usize,
    /// Maximum encoded/stored bytes per PAX Row Group before flushing (default: `64 MiB`,
    /// valid range: `1..=MAX_BATCH_DECODED_BYTES / 2`).
    pub max_row_group_bytes: usize,
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
    /// Optional pluggable [`crate::TaskExecutor`] used for parallel Row Group encoding instead
    /// of Statam's internal fallback worker pool.
    pub executor: Option<Arc<dyn crate::parallel::TaskExecutor>>,
}

impl fmt::Debug for WriteOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WriteOptions")
            .field("profile", &self.profile)
            .field("table_kind", &self.table_kind)
            .field("sort_cols", &self.sort_cols)
            .field("bloom_cols", &self.bloom_cols)
            .field("hot_bloom_col", &self.hot_bloom_col)
            .field("payload_cols", &self.payload_cols)
            .field("narrow_trigram_cols", &self.narrow_trigram_cols)
            .field("row_group_size", &self.row_group_size)
            .field("max_row_group_bytes", &self.max_row_group_bytes)
            .field("mini_block_size", &self.mini_block_size)
            .field("zstd_level", &self.zstd_level)
            .field("compress_hot_tail_lz4", &self.compress_hot_tail_lz4)
            .field("pack_id", &self.pack_id)
            .field("write_threads", &self.write_threads)
            .field("has_executor", &self.executor.is_some())
            .finish()
    }
}

impl PartialEq for WriteOptions {
    fn eq(&self, other: &Self) -> bool {
        self.profile == other.profile
            && self.table_kind == other.table_kind
            && self.sort_cols == other.sort_cols
            && self.bloom_cols == other.bloom_cols
            && self.hot_bloom_col == other.hot_bloom_col
            && self.payload_cols == other.payload_cols
            && self.narrow_trigram_cols == other.narrow_trigram_cols
            && self.row_group_size == other.row_group_size
            && self.max_row_group_bytes == other.max_row_group_bytes
            && self.mini_block_size == other.mini_block_size
            && self.zstd_level == other.zstd_level
            && self.compress_hot_tail_lz4 == other.compress_hot_tail_lz4
            && self.pack_id == other.pack_id
            && self.write_threads == other.write_threads
    }
}

impl Eq for WriteOptions {}

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
        let max_row_group_bytes = DEFAULT_MAX_ROW_GROUP_BYTES;
        let mini_block_size = MINI_BLOCK_SIZE;
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
            max_row_group_bytes,
            mini_block_size,
            zstd_level,
            compress_hot_tail_lz4: true,
            pack_id: None,
            write_threads: 1,
            executor: None,
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

enum PackBuilderTarget<'a, PW: Write + Seek> {
    Borrowed(&'a mut ChunkPackBuilder<PW>),
    Owned(Box<ChunkPackBuilder<PW>>),
}

impl<PW: Write + Seek> PackBuilderTarget<'_, PW> {
    #[inline]
    pub(super) fn as_mut(&mut self) -> &mut ChunkPackBuilder<PW> {
        match self {
            Self::Borrowed(pb) => pb,
            Self::Owned(pb) => pb,
        }
    }
}

/// Streaming bounded-memory PAX writer producing `.statam` files.
pub struct StatamWriter<'a, W: Write + Seek, PW: Write + Seek = std::io::Cursor<Vec<u8>>> {
    out: W,
    start_offset: u64,
    schema: Arc<Schema>,
    options: WriteOptions,
    pack_builder: Option<PackBuilderTarget<'a, PW>>,
    logical_locality: Vec<LocalityGroup>,
    bloom_col_set: HashSet<usize>,
    hot_bloom_col_idx: Option<usize>,
    narrow_trigram_col_set: HashSet<usize>,
    pack_ids: Vec<[u8; 16]>,
    uses_external_pack: bool,

    // Bounded 1-RG input buffer (`<= row_group_size` rows).
    rg_buf_cols: Vec<Vec<Column>>,
    rg_buf_rows: usize,
    rg_buf_est_non_payload_bytes: usize,
    rg_buf_probed_stored_bytes: usize,
    rg_buf_unprobed_raw_bytes: usize,
    rg_buf_unprobed_pieces: Vec<usize>,
    rg_buf_cur_mb_col_raw: Vec<usize>,
    // Zero-copy L0Fast / local ExternalPayload accumulators for the current partial RG.
    pending_l0_local: LocalL0PayloadBuilder,
    pending_l0_mbs: HashMap<usize, Vec<(u16, u16, Vec<u8>)>>,
    pending_l0_partial: HashMap<usize, Vec<PayloadRefRow>>,

    // RG0-initialized file-level schema/dict state.
    rg0_initialized: bool,
    shredded_maps: Vec<ShreddedMapDir>,
    logical_to_shredded: HashMap<usize, usize>,
    phys_col_meta: Vec<PhysColWriteState>,
    phys_blocks: Vec<Vec<BlockEntry>>,

    // Flushed Row Group directory entries (RG data streams directly into `out`).
    rg_entries: Vec<RowGroupEntry>,
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

impl<W: Write + Seek, PW: Write + Seek> fmt::Debug for StatamWriter<'_, W, PW> {
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

impl<'a, W: Write + Seek> StatamWriter<'a, W, std::io::Cursor<Vec<u8>>> {
    /// Creates a new streaming [`StatamWriter`] writing into `out` for `schema` and `options`.
    ///
    /// Records the initial stream offset and writes a 32-byte header placeholder that
    /// [`Self::finish`] patches in place once all Row Groups and tail sections are written.
    pub fn new(
        out: W,
        schema: Arc<Schema>,
        options: WriteOptions,
        pack_builder: Option<&'a mut ChunkPackBuilder>,
    ) -> Result<Self, StatamError> {
        Self::new_inner(
            out,
            schema,
            options,
            pack_builder.map(PackBuilderTarget::Borrowed),
        )
    }
}

impl<W: Write + Seek, PW: Write + Seek> StatamWriter<'static, W, PW> {
    /// Creates a `'static` [`StatamWriter`] that owns its optional streaming [`ChunkPackBuilder`].
    ///
    /// When `options.pack_id` is `None` and `pack` is `Some(ref p)`, automatically sets
    /// `options.pack_id = Some(p.pack_id())` so callers never fall back to `[0u8; 16]`.
    pub fn new_with_owned_pack(
        out: W,
        schema: Arc<Schema>,
        mut options: WriteOptions,
        pack: Option<ChunkPackBuilder<PW>>,
    ) -> Result<Self, StatamError> {
        if let Some(ref p) = pack {
            match options.pack_id {
                None => options.pack_id = Some(p.pack_id()),
                Some(id) if id != p.pack_id() => {
                    return Err(StatamError::SchemaMismatch {
                        reason: "WriteOptions::pack_id does not match owned ChunkPackBuilder::pack_id",
                    });
                }
                Some(_) => {}
            }
        }
        Self::new_inner(
            out,
            schema,
            options,
            pack.map(|p| PackBuilderTarget::Owned(Box::new(p))),
        )
    }
}

impl<'a, W: Write + Seek, PW: Write + Seek> StatamWriter<'a, W, PW> {
    fn new_inner(
        mut out: W,
        schema: Arc<Schema>,
        options: WriteOptions,
        mut pack_builder: Option<PackBuilderTarget<'a, PW>>,
    ) -> Result<Self, StatamError> {
        let fields = schema.fields();
        if fields.is_empty() {
            return Err(StatamError::SchemaMismatch {
                reason: "schema must contain at least one field",
            });
        }
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
        if options.max_row_group_bytes == 0
            || options.max_row_group_bytes > MAX_BATCH_DECODED_BYTES / 2
        {
            return Err(StatamError::BoundsExceeded {
                reason: "max_row_group_bytes must be in 1..=MAX_BATCH_DECODED_BYTES / 2",
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
        let mut seen_sort = HashSet::with_capacity(options.sort_cols.len());
        for &sc in &options.sort_cols {
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

        for &sc in &options.sort_cols {
            if logical_locality[sc as usize] == LocalityGroup::ExternalPayload {
                return Err(StatamError::SchemaMismatch {
                    reason: "sort column cannot be an ExternalPayload column",
                });
            }
        }

        let mut bloom_col_set = HashSet::new();
        let mut bloom_rgs = HashMap::new();
        let mut bloom_has_values = HashMap::new();
        for name in &options.bloom_cols {
            if let Some(idx) = schema.index_of(name) {
                if logical_locality[idx] == LocalityGroup::ExternalPayload {
                    return Err(StatamError::SchemaMismatch {
                        reason: "bloom column cannot be an ExternalPayload column",
                    });
                }
                bloom_col_set.insert(idx);
                bloom_rgs.insert(idx, Vec::new());
                bloom_has_values.insert(idx, false);
            }
        }
        let hot_bloom_col_idx = match options
            .hot_bloom_col
            .as_deref()
            .and_then(|name| schema.index_of(name))
        {
            Some(idx) if logical_locality[idx] == LocalityGroup::ExternalPayload => {
                return Err(StatamError::SchemaMismatch {
                    reason: "hot_bloom_col cannot be an ExternalPayload column",
                });
            }
            other => other,
        };
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

        if let Some(pb) = pack_builder.as_mut() {
            pb.as_mut().enable_boundary_tracking();
        }
        let pack_ids = match (&pack_builder, options.pack_id) {
            (Some(_), Some(id)) => vec![id],
            _ => Vec::new(),
        };

        let start_offset = out.stream_position()?;
        out.write_all(&[0u8; FILE_HEADER_LEN])?;

        let num_logical = fields.len();
        let pending_l0_local =
            LocalL0PayloadBuilder::new_with_threads(options.zstd_level, options.write_threads);
        Ok(Self {
            out,
            start_offset,
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
            rg_buf_est_non_payload_bytes: 0,
            rg_buf_probed_stored_bytes: 0,
            rg_buf_unprobed_raw_bytes: 0,
            rg_buf_unprobed_pieces: Vec::new(),
            rg_buf_cur_mb_col_raw: vec![0; num_logical],
            pending_l0_local,
            pending_l0_mbs: HashMap::new(),
            pending_l0_partial: HashMap::new(),
            rg0_initialized: false,
            shredded_maps: Vec::new(),
            logical_to_shredded: HashMap::new(),
            phys_col_meta,
            phys_blocks: vec![Vec::new(); num_logical],
            rg_entries: Vec::new(),
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
        let _exec_guard = crate::parallel::enter_executor_scope(self.options.executor.as_ref());
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
        let max_rg_bytes = self.options.max_row_group_bytes;
        let half_cap = (max_rg_bytes / 2).max(1);
        let max_rg_raw_bytes = MAX_RG_RAW_BUFFER_BYTES;
        let half_raw_cap = max_rg_raw_bytes / 2;
        let is_l0_fast = self.options.profile == WriteProfile::L0Fast;
        let is_local_payload = is_l0_fast || self.pack_builder.is_none();
        let has_local_payload_cols = is_local_payload
            && self
                .logical_locality
                .contains(&LocalityGroup::ExternalPayload);
        let is_l1_pack = !is_l0_fast && self.pack_builder.is_some();
        let first_non_payload_col = self
            .logical_locality
            .iter()
            .position(|&lg| lg != LocalityGroup::ExternalPayload);
        let mut offset = 0usize;
        while offset < total {
            if self.rg_buf_rows == 0 && total - offset >= rg_size {
                let local_payload_est = if has_local_payload_cols {
                    self.estimate_slice_local_payload_raw_bytes(
                        active_batch.columns(),
                        offset,
                        rg_size,
                    )
                } else {
                    0
                };
                if local_payload_est <= max_rg_bytes {
                    let np_mat_raw = self.estimate_slice_non_payload_materialized_bytes(
                        active_batch.columns(),
                        offset,
                        rg_size,
                    );
                    if np_mat_raw <= max_rg_raw_bytes
                        && self.slice_fits_mb_raw_cap(
                            active_batch.columns(),
                            offset,
                            rg_size,
                            false,
                        )
                    {
                        let np_budget = max_rg_bytes.saturating_sub(local_payload_est);
                        let (_, np_stored, _) = self.estimate_slice_non_payload_stored(
                            active_batch.columns(),
                            offset,
                            rg_size,
                            np_budget,
                        );
                        if np_stored.saturating_add(local_payload_est) <= max_rg_bytes {
                            self.flush_rg_columns(active_batch.columns(), offset, rg_size, None)?;
                            offset += rg_size;
                            continue;
                        }
                    }
                }
            }
            let space = rg_size - self.rg_buf_rows;
            let mut take = (total - offset).min(space);
            if has_local_payload_cols {
                while take > 1 {
                    let payload_raw = self.estimate_slice_local_payload_raw_bytes(
                        active_batch.columns(),
                        offset,
                        take,
                    );
                    if payload_raw <= half_cap {
                        break;
                    }
                    take /= 2;
                }
            }
            let raw_limit = if self.rg_buf_rows == 0 {
                max_rg_raw_bytes
            } else {
                half_raw_cap
            };
            let stored_limit = if self.rg_buf_rows == 0 && !has_local_payload_cols {
                max_rg_bytes
            } else {
                half_cap
            };
            let (slice_np_mat_raw, mut slice_np_enc_raw, mut slice_np_stored, mut slice_was_probed) = loop {
                let np_mat_raw = self.estimate_slice_non_payload_materialized_bytes(
                    active_batch.columns(),
                    offset,
                    take,
                );
                let mb_ok = self.slice_fits_mb_raw_cap(active_batch.columns(), offset, take, false);
                let (np_enc_raw, np_stored, np_probed) =
                    if (np_mat_raw <= raw_limit && mb_ok) || take <= 1 {
                        self.estimate_slice_non_payload_stored(
                            active_batch.columns(),
                            offset,
                            take,
                            stored_limit,
                        )
                    } else {
                        (0, usize::MAX, false)
                    };
                if take <= 1 || (np_mat_raw <= raw_limit && mb_ok && np_stored <= stored_limit) {
                    break (np_mat_raw, np_enc_raw, np_stored, np_probed);
                }
                take /= 2;
            };

            if self.rg_buf_rows > 0 {
                if self
                    .rg_buf_est_non_payload_bytes
                    .saturating_add(slice_np_mat_raw)
                    > max_rg_raw_bytes
                    || !self.slice_fits_mb_raw_cap(active_batch.columns(), offset, take, true)
                {
                    self.flush_row_group()?;
                    continue;
                }
                let mut cur_est = self.measure_current_rg_stored_bytes(max_rg_bytes)?;
                let next_single_row_payload = if has_local_payload_cols && take == 1 {
                    self.estimate_slice_local_payload_raw_bytes(active_batch.columns(), offset, 1)
                } else {
                    0
                };
                if cur_est
                    .saturating_add(slice_np_stored)
                    .saturating_add(next_single_row_payload)
                    > max_rg_bytes
                {
                    if !self.rg_buf_unprobed_pieces.is_empty() {
                        self.reprobe_rg_buf_non_payload_stored_bytes();
                        cur_est = self.measure_current_rg_stored_bytes(max_rg_bytes)?;
                    }
                    if !slice_was_probed
                        && cur_est
                            .saturating_add(slice_np_stored)
                            .saturating_add(next_single_row_payload)
                            > max_rg_bytes
                    {
                        let (enc_raw, stored, probed) = self.estimate_slice_non_payload_stored(
                            active_batch.columns(),
                            offset,
                            take,
                            0,
                        );
                        slice_np_enc_raw = enc_raw;
                        slice_np_stored = stored;
                        slice_was_probed = probed;
                    }
                    if cur_est
                        .saturating_add(slice_np_stored)
                        .saturating_add(next_single_row_payload)
                        > max_rg_bytes
                    {
                        self.flush_row_group()?;
                        continue;
                    }
                }
            }

            let piece_idx = first_non_payload_col.map(|c| self.rg_buf_cols[c].len());
            let old_rows = self.rg_buf_rows;
            let new_rows = old_rows + take;
            let mb_size = self.options.mini_block_size;
            let same_mb = old_rows / mb_size == new_rows / mb_size;
            let rem_in_mb = new_rows % mb_size;

            for (col_idx, col) in active_batch.columns().iter().enumerate() {
                if is_local_payload
                    && self.logical_locality[col_idx] == LocalityGroup::ExternalPayload
                {
                    self.append_l0_payload_slice(col_idx, col, offset, take)?;
                } else if is_l1_pack
                    && self.logical_locality[col_idx] == LocalityGroup::ExternalPayload
                {
                    self.append_l1_payload_slice(col_idx, col, offset, take)?;
                } else {
                    let piece = if offset == 0 && take == total {
                        (**col).clone()
                    } else {
                        col.slice(offset, take, None)?
                    };
                    let piece_mat_bytes = estimate_column_bytes(&piece);
                    self.rg_buf_est_non_payload_bytes = self
                        .rg_buf_est_non_payload_bytes
                        .saturating_add(piece_mat_bytes);
                    if same_mb {
                        self.rg_buf_cur_mb_col_raw[col_idx] =
                            self.rg_buf_cur_mb_col_raw[col_idx].saturating_add(piece_mat_bytes);
                    } else if rem_in_mb == 0 {
                        self.rg_buf_cur_mb_col_raw[col_idx] = 0;
                    } else {
                        self.rg_buf_cur_mb_col_raw[col_idx] =
                            estimate_column_slice_materialized_bytes(
                                col,
                                offset + take - rem_in_mb,
                                rem_in_mb,
                            );
                    }
                    self.rg_buf_cols[col_idx].push(piece);
                }
            }
            if let Some(pidx) = piece_idx {
                if slice_was_probed {
                    self.rg_buf_probed_stored_bytes = self
                        .rg_buf_probed_stored_bytes
                        .saturating_add(slice_np_stored);
                } else if slice_np_enc_raw > 0 {
                    self.rg_buf_unprobed_raw_bytes = self
                        .rg_buf_unprobed_raw_bytes
                        .saturating_add(slice_np_enc_raw);
                    self.rg_buf_unprobed_pieces.push(pidx);
                }
            }
            self.rg_buf_rows = new_rows;
            offset += take;

            if self.rg_buf_rows == rg_size
                || self.rg_buf_est_non_payload_bytes >= max_rg_raw_bytes
                || self.measure_current_rg_stored_bytes(max_rg_bytes)? >= max_rg_bytes
            {
                self.flush_row_group()?;
            }
        }

        Ok(())
    }

    fn pending_l0_encoded_bytes(&self) -> usize {
        let mbs_bytes: usize = self
            .pending_l0_mbs
            .values()
            .flat_map(|v| v.iter())
            .map(|(_, _, body)| body.len().saturating_add(BLOCK_FRAME_HEADER_LEN))
            .sum();
        let partial_bytes: usize = self
            .pending_l0_partial
            .values()
            .flat_map(|v| v.iter())
            .map(|row| match row {
                PayloadRefRow::Null => 1,
                PayloadRefRow::Inline(b) => b.len().saturating_add(4),
                PayloadRefRow::Chunks(c) => c.len().saturating_mul(6).saturating_add(4),
            })
            .sum();
        mbs_bytes.saturating_add(partial_bytes)
    }

    fn current_rg_non_payload_stored_est(&self) -> usize {
        self.rg_buf_probed_stored_bytes
            .saturating_add(self.rg_buf_unprobed_raw_bytes)
    }

    fn reprobe_rg_buf_non_payload_stored_bytes(&mut self) {
        if self.rg_buf_unprobed_pieces.is_empty() {
            self.rg_buf_unprobed_raw_bytes = 0;
            return;
        }
        let mut scratch = StoredProbeScratch::default();
        let unprobed = std::mem::take(&mut self.rg_buf_unprobed_pieces);
        let mut added_stored = 0usize;
        for piece_idx in unprobed {
            for (cidx, col_pieces) in self.rg_buf_cols.iter().enumerate() {
                if self.logical_locality[cidx] == LocalityGroup::ExternalPayload {
                    continue;
                }
                if let Some(piece) = col_pieces.get(piece_idx) {
                    added_stored = added_stored.saturating_add(estimate_column_slice_stored_bytes(
                        piece,
                        0,
                        piece.len(),
                        &mut scratch,
                    ));
                }
            }
        }
        self.rg_buf_probed_stored_bytes =
            self.rg_buf_probed_stored_bytes.saturating_add(added_stored);
        self.rg_buf_unprobed_raw_bytes = 0;
    }

    fn measure_current_rg_stored_bytes(
        &mut self,
        max_rg_bytes: usize,
    ) -> Result<usize, StatamError> {
        let mut base = self
            .current_rg_non_payload_stored_est()
            .saturating_add(self.pending_l0_encoded_bytes());
        let mut est = base.saturating_add(self.pending_l0_local.estimated_stored_bytes());
        if est >= max_rg_bytes && !self.rg_buf_unprobed_pieces.is_empty() {
            self.reprobe_rg_buf_non_payload_stored_bytes();
            base = self
                .current_rg_non_payload_stored_est()
                .saturating_add(self.pending_l0_encoded_bytes());
            est = base.saturating_add(self.pending_l0_local.estimated_stored_bytes());
        }
        if est >= max_rg_bytes && !self.pending_l0_local.pending_frames.is_empty() {
            self.pending_l0_local.flush_pending()?;
            est = base.saturating_add(self.pending_l0_local.estimated_stored_bytes());
        }
        if est >= max_rg_bytes && !self.pending_l0_local.cur_chunks.is_empty() {
            self.pending_l0_local.flush_frame()?;
            if !self.pending_l0_local.pending_frames.is_empty() {
                self.pending_l0_local.flush_pending()?;
            }
            est = base.saturating_add(self.pending_l0_local.estimated_stored_bytes());
        }
        Ok(est)
    }

    fn estimate_slice_local_payload_raw_bytes(
        &self,
        cols: &[Arc<Column>],
        offset: usize,
        len: usize,
    ) -> usize {
        if len == 0 {
            return 0;
        }
        let mut sum = 0usize;
        for (cidx, col) in cols.iter().enumerate() {
            if self.logical_locality[cidx] != LocalityGroup::ExternalPayload {
                continue;
            }
            let col_bytes = match col.as_ref() {
                Column::Flat(f) => match f.data() {
                    FlatData::Utf8(u) => {
                        let offs = u.offsets();
                        let start = offs.get(offset).copied().unwrap_or(0) as usize;
                        let end = offs.get(offset + len).copied().unwrap_or(0) as usize;
                        end.saturating_sub(start)
                    }
                    FlatData::Binary(b) => {
                        let offs = b.offsets();
                        let start = offs.get(offset).copied().unwrap_or(0) as usize;
                        let end = offs.get(offset + len).copied().unwrap_or(0) as usize;
                        end.saturating_sub(start)
                    }
                    _ => 0,
                },
                Column::Const { value, .. } => match value {
                    ScalarValue::Utf8(s) => s.len().saturating_mul(len),
                    ScalarValue::Binary(b) => b.len().saturating_mul(len),
                    _ => 0,
                },
                Column::Dict(d) => match d.values().data() {
                    FlatData::Utf8(u) => {
                        let codes = d.codes();
                        let end = (offset + len).min(codes.len());
                        let mut acc = 0usize;
                        for &code in &codes[offset.min(end)..end] {
                            acc = acc.saturating_add(crate::codec::utf8_len_at(u, code as usize));
                        }
                        acc
                    }
                    FlatData::Binary(b) => {
                        let codes = d.codes();
                        let end = (offset + len).min(codes.len());
                        let mut acc = 0usize;
                        for &code in &codes[offset.min(end)..end] {
                            if let Some(s) = b.get(code as usize) {
                                acc = acc.saturating_add(s.len());
                            }
                        }
                        acc
                    }
                    _ => 0,
                },
                Column::Rle(r) => match r.values().data() {
                    FlatData::Utf8(u) => {
                        let mut acc = 0usize;
                        let mut run_start = 0usize;
                        let slice_end = offset.saturating_add(len);
                        for (run_idx, &run_end_u32) in r.run_ends().iter().enumerate() {
                            let run_end = run_end_u32 as usize;
                            let lo = run_start.max(offset);
                            let hi = run_end.min(slice_end);
                            if hi > lo {
                                acc = acc.saturating_add(
                                    crate::codec::utf8_len_at(u, run_idx).saturating_mul(hi - lo),
                                );
                            }
                            run_start = run_end;
                            if run_start >= slice_end {
                                break;
                            }
                        }
                        acc
                    }
                    FlatData::Binary(b) => {
                        let mut acc = 0usize;
                        let mut run_start = 0usize;
                        let slice_end = offset.saturating_add(len);
                        for (run_idx, &run_end_u32) in r.run_ends().iter().enumerate() {
                            let run_end = run_end_u32 as usize;
                            let lo = run_start.max(offset);
                            let hi = run_end.min(slice_end);
                            if hi > lo
                                && let Some(s) = b.get(run_idx)
                            {
                                acc = acc.saturating_add(s.len().saturating_mul(hi - lo));
                            }
                            run_start = run_end;
                            if run_start >= slice_end {
                                break;
                            }
                        }
                        acc
                    }
                    _ => 0,
                },
            };
            sum = sum.saturating_add(col_bytes);
        }
        sum
    }

    fn estimate_slice_non_payload_bytes(
        &self,
        cols: &[Arc<Column>],
        offset: usize,
        len: usize,
    ) -> usize {
        if len == 0 {
            return 0;
        }
        let mut sum = 0usize;
        for (cidx, col) in cols.iter().enumerate() {
            if self.logical_locality[cidx] == LocalityGroup::ExternalPayload {
                continue;
            }
            sum = sum.saturating_add(estimate_column_slice_bytes(col, offset, len));
        }
        sum
    }

    fn estimate_slice_non_payload_materialized_bytes(
        &self,
        cols: &[Arc<Column>],
        offset: usize,
        len: usize,
    ) -> usize {
        if len == 0 {
            return 0;
        }
        let mut sum = 0usize;
        for (cidx, col) in cols.iter().enumerate() {
            if self.logical_locality[cidx] == LocalityGroup::ExternalPayload {
                continue;
            }
            sum = sum.saturating_add(estimate_column_slice_materialized_bytes(col, offset, len));
        }
        sum
    }

    fn slice_fits_mb_raw_cap(
        &self,
        cols: &[Arc<Column>],
        offset: usize,
        len: usize,
        aligned_to_rg_buf: bool,
    ) -> bool {
        if len == 0 {
            return true;
        }
        let mb_size = self.options.mini_block_size;
        let open_mod = if aligned_to_rg_buf {
            self.rg_buf_rows % mb_size
        } else {
            0
        };
        for (cidx, col) in cols.iter().enumerate() {
            if self.logical_locality[cidx] == LocalityGroup::ExternalPayload {
                continue;
            }
            let col_mat = estimate_column_slice_materialized_bytes(col, offset, len);
            let open_raw = if open_mod != 0 {
                self.rg_buf_cur_mb_col_raw[cidx]
            } else {
                0
            };
            if open_raw.saturating_add(col_mat) <= MAX_COL_MB_RAW_BYTES {
                continue;
            }
            let mut pos = 0usize;
            if open_mod != 0 {
                let chunk = (mb_size - open_mod).min(len);
                if open_raw
                    .saturating_add(estimate_column_slice_materialized_bytes(col, offset, chunk))
                    > MAX_COL_MB_RAW_BYTES
                {
                    return false;
                }
                pos += chunk;
            }
            while pos < len {
                let chunk = (len - pos).min(mb_size);
                if estimate_column_slice_materialized_bytes(col, offset + pos, chunk)
                    > MAX_COL_MB_RAW_BYTES
                {
                    return false;
                }
                pos += chunk;
            }
        }
        true
    }

    fn estimate_slice_non_payload_stored(
        &self,
        cols: &[Arc<Column>],
        offset: usize,
        len: usize,
        probe_threshold: usize,
    ) -> (usize, usize, bool) {
        let enc_raw = self.estimate_slice_non_payload_bytes(cols, offset, len);
        if len == 0 || enc_raw <= probe_threshold {
            return (enc_raw, enc_raw, false);
        }
        let mut scratch = StoredProbeScratch::default();
        let mut stored = 0usize;
        for (cidx, col) in cols.iter().enumerate() {
            if self.logical_locality[cidx] == LocalityGroup::ExternalPayload {
                continue;
            }
            stored = stored.saturating_add(estimate_column_slice_stored_bytes(
                col,
                offset,
                len,
                &mut scratch,
            ));
        }
        (enc_raw, stored.min(enc_raw), true)
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
        let (offsets, all_bytes, is_binary): (&[u32], &[u8], bool) = match fc.data() {
            FlatData::Utf8(u) => (u.offsets(), u.bytes(), false),
            FlatData::Binary(b) => (b.offsets(), b.bytes(), true),
            _ => {
                return Err(StatamError::SchemaMismatch {
                    reason: "ExternalPayload column must be DataType::Utf8 or DataType::Binary",
                });
            }
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
            let s = offsets[r] as usize;
            let e = offsets[r + 1] as usize;
            let bytes = &all_bytes[s..e];
            if bytes.len() > MAX_ROW_PAYLOAD_BYTES {
                return Err(StatamError::BoundsExceeded {
                    reason: "row payload exceeds MAX_ROW_PAYLOAD_BYTES",
                });
            }
            if bytes.len() <= INLINE_PAYLOAD_THRESHOLD {
                dst.push(PayloadRefRow::Inline(bytes.to_vec()));
            } else if is_binary {
                let cids = local.add_blob(bytes)?;
                let refs = cids
                    .into_iter()
                    .map(|cid| (L0_LOCAL_PACK_IDX, cid))
                    .collect();
                dst.push(PayloadRefRow::Chunks(refs));
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
            let null_count = encode_payload_refs_l0_from_bytes(
                offsets,
                all_bytes,
                col_nulls,
                offset + pos,
                mb_size,
                INLINE_PAYLOAD_THRESHOLD,
                L0_LOCAL_PACK_IDX,
                |bytes, cids| {
                    if is_binary {
                        self.pending_l0_local.add_blob_into(bytes, cids)
                    } else {
                        for slice in bytes.chunks(MAX_FRAME_RAW_BYTES) {
                            cids.push(self.pending_l0_local.add_chunk(slice)?);
                        }
                        Ok(())
                    }
                },
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
        mut l0_local_source: Option<&mut crate::reader::StreamingL0Payload>,
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
                    (**col).clone()
                } else {
                    col.slice(offset, take, None)?
                };
                self.rg_buf_cols[col_idx].push(piece);
            }
            self.rg_buf_rows += take;
            let sliced_refs: HashMap<usize, Vec<PayloadRefRow>> = prechunked
                .iter()
                .map(|(&cidx, rows)| (cidx, rows[offset..offset + take].to_vec()))
                .collect();
            offset += take;
            self.flush_row_group_with_refs(Some((&sliced_refs, l0_local_source.as_deref_mut())))?;
        }
        Ok(())
    }

    /// Finishes the `.statam` file (flushing any partial Row Group, writing Lazy Indexes,
    /// Hot Tail, and 112-byte Postscript, then patching the 32-byte header at `start_offset`
    /// and seeking to EOF).
    pub fn finish(self) -> Result<WriteStats, StatamError> {
        self.finish_into_inner().map(|(_, stats, _)| stats)
    }

    /// Finishes the `.statam` file and returns `(out, stats, owned_pack_builder)`.
    ///
    /// Returns `Some(pack_builder)` when constructed via [`Self::new_with_owned_pack`] with
    /// an owned [`ChunkPackBuilder`], or `None` otherwise.
    #[allow(clippy::type_complexity)]
    pub fn finish_into_inner(
        mut self,
    ) -> Result<(W, WriteStats, Option<ChunkPackBuilder<PW>>), StatamError> {
        let _exec_guard = crate::parallel::enter_executor_scope(self.options.executor.as_ref());
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

        // Decide initial Hot Tail placement against both the 120 KiB compressed soft budget
        // and the hard `MAX_HOT_TAIL_BYTES` uncompressed budget (`est_raw_len + POSTSCRIPT_LEN`).
        decide_hot_tail_placement(
            base_compressed_len,
            base_dirs_len,
            trigram_items.len(),
            &mut primary_bloom,
            &mut dict_items,
            &mut sec_blooms,
        );

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

        if !lazy_data_buf.is_empty() {
            self.out.write_all(&lazy_data_buf)?;
        }
        self.out.write_all(&hot_tail_stored)?;
        self.out.write_all(&postscript.encode())?;

        let total_bytes = cur_file_off.saturating_add(u64::from(hot_tail_len));
        let eof_pos =
            self.start_offset
                .checked_add(total_bytes)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "output stream EOF offset overflow",
                })?;
        self.out.seek(SeekFrom::Start(self.start_offset))?;
        self.out.write_all(&header.encode())?;
        self.out.seek(SeekFrom::Start(eof_pos))?;
        self.out.flush()?;

        let shredded_subcols = (self.phys_col_meta.len() - self.schema.len()) as u16;

        let owned_pack = match self.pack_builder.take() {
            Some(PackBuilderTarget::Owned(pb)) => Some(*pb),
            Some(PackBuilderTarget::Borrowed(_)) | None => None,
        };

        Ok((
            self.out,
            WriteStats {
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
            },
            owned_pack,
        ))
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
            let dt = &self.schema.fields()[cidx].data_type;
            if pieces.is_empty() && self.logical_locality[cidx] == LocalityGroup::ExternalPayload {
                rg_cols.push(Arc::new(Column::Flat(FlatColumn::empty(dt))));
            } else {
                let combined = concat_rg_column(dt, pieces, rg_rows)?;
                pieces.clear();
                rg_cols.push(Arc::new(combined));
            }
        }
        self.rg_buf_rows = 0;
        self.rg_buf_est_non_payload_bytes = 0;
        self.rg_buf_probed_stored_bytes = 0;
        self.rg_buf_unprobed_raw_bytes = 0;
        self.rg_buf_unprobed_pieces.clear();
        self.rg_buf_cur_mb_col_raw.fill(0);

        self.flush_rg_columns(&rg_cols, 0, rg_rows, prechunked)
    }
}

fn classify_locality_group(
    field: &cetra_column::Field,
    payload_names: &HashSet<&str>,
) -> LocalityGroup {
    if matches!(field.data_type, DataType::Utf8 | DataType::Binary)
        && payload_names.contains(field.name.as_str())
    {
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

#[cfg(not(miri))]
fn default_l1_codec() -> Codec {
    Codec::Zstd
}

#[cfg(miri)]
fn default_l1_codec() -> Codec {
    Codec::Lz4
}
