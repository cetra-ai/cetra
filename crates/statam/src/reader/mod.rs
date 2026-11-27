//! Range-based `.statam` reader with 1-GET cold open, SBBF/zone-map pruning,
//! two-phase late materialization, `map_keys` shredded projection, and exact substring search.

mod eval;
mod materialize;
mod meta;
mod payload;
mod plan;
#[cfg(test)]
mod tests;

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use cetra_column::{
    Batch, Bitmap, Column, DataType, FlatColumn, FlatData, ScalarValue, Schema, Selection,
};

use crate::StatamError;
use crate::codec::dict::FileDict;
use crate::codec::nested::{
    ShreddedSubCol, decode_shredded_residual_block, decode_shredded_subcol, unshred_map,
};
use crate::codec::{ZonePred, decode_mini_block};
use crate::format::{
    BLOCK_FRAME_HEADER_LEN, BlockEntry, CHECKSUM_ALG_CRC32C, COL_FLAG_HAS_FILE_DICT,
    COL_FLAG_HAS_TRIGRAM_INDEX, Encoding, FILE_HEADER_LEN, FileHeader, LocalityGroup,
    MAX_BATCH_DECODED_BYTES, MAX_BLOCK_RAW_BYTES, RowGroupEntry, read_framed_block_checked_cow,
};
use crate::io::RangeReader;
use crate::parallel::{MAX_PARALLEL_THREADS, in_parallel_scope, parallel_map_in_order_static};
use crate::payload::pack::ChunkPackReader;
use crate::payload::search::RetainedPackChunks;
use crate::payload::{
    FlatPayloadRefs, PayloadRefRow, decode_payload_refs_flat, decode_payload_refs_selected,
};
use crate::simd::bloom::{BloomIndex, statam_bloom_hash};
use crate::simd::trigram::TrigramBloom;
use crate::writer::{concat_column_refs, estimate_column_bytes};

use eval::{Utf8Matcher, is_always_false_pred, is_supported_id_lookup_type};
#[cfg(test)]
use eval::{eval_utf8_block_substring_raw, eval_utf8_substring};
use materialize::mask_from_indices;
pub use meta::StatamFileMeta;
#[cfg(test)]
use payload::PayloadWavePlan;
pub(crate) use payload::StreamingL0Payload;
use payload::{PHASE2_PAYLOAD_WAVE_TARGET_BYTES, ParsedL0Payload, ResolvedChunks};
use plan::{
    ActiveMiniBlock, BlockCacheReader, DecodedPhase1, RgIoPlan, TouchedMbRows, find_time_column,
};

/// Maximum gap (64 KiB) between blocks coalesced unconditionally into a single `Scan` range read.
const SCAN_COALESCE_MAX_GAP: u64 = 64 * 1024;
/// Maximum gap (192 KiB) between blocks coalesced on remote readers when wasted gap bytes do not
/// exceed useful payload bytes (`new_wasted <= new_useful`).
const REMOTE_SCAN_COALESCE_MAX_GAP: u64 = 192 * 1024;
/// Maximum total span (4 MiB) of a single coalesced `Scan` range read.
const SCAN_COALESCE_MAX_SPAN: u64 = 4 * 1024 * 1024;
/// Maximum cumulative `.statam` byte range volume fetched in a single cross-RG I/O wave (`64 MiB`).
const IO_WAVE_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum cumulative uncompressed Phase-1 block bytes decoded in a single filtered wave (`128 MiB`).
const P1_WAVE_MAX_RAW_BYTES: u64 = 128 * 1024 * 1024;
/// Maximum candidate Phase-2 `.statam` bytes (blocks + L0 payload + uncached non-inline lazy
/// dicts) folded into Wave 1 on remote readers (when `!has_payload_sub`) to eliminate the
/// Phase-2 round-trip (`1.5 MiB`).
const REMOTE_SINGLE_WAVE_P2_MAX_BYTES: u64 = 1536 * 1024;
/// Minimum total estimated decode work bytes required before spawning reader worker threads
/// (48 KiB), avoiding OS thread-spawn overhead on sub-millisecond narrow queries.
const MIN_PARALLEL_WORK_BYTES: usize = 48 * 1024;

/// Computes the bounded worker count for reader parallel scopes based on `work_bytes`.
#[inline]
pub(super) fn effective_read_threads(work_bytes: usize, max_threads: usize) -> usize {
    let cap = max_threads.clamp(1, MAX_PARALLEL_THREADS);
    if cap <= 1 || work_bytes < MIN_PARALLEL_WORK_BYTES {
        return 1;
    }
    cap
}

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
    /// Number of worker threads for parallel `.stpk` frame scanning and Phase-2 column decoding
    /// (`0` or `1` = sequential, clamped to `1..=32`).
    pub read_threads: usize,
}

#[derive(Default)]
pub(super) struct ReaderCaches {
    pub(super) blooms: HashMap<usize, BloomIndex>,
    pub(super) dicts: HashMap<usize, Arc<FileDict>>,
    pub(super) narrow_trigrams: HashMap<usize, Vec<Option<TrigramBloom>>>,
    pub(super) packs: HashMap<[u8; 16], Arc<ChunkPackReader>>,
}

struct P2CandidateFetch {
    ranges: Vec<(u64, u64)>,
    bytes: u64,
    dict_cols: Vec<usize>,
    l0_needed: bool,
}

/// Range-based reader for `.statam` files.
pub struct StatamReader {
    reader: Arc<dyn RangeReader>,
    meta: Arc<StatamFileMeta>,
    packs: Option<Arc<dyn PackResolver>>,
    retained_tail_start: Option<u64>,
    caches: Mutex<ReaderCaches>,
    pack_init_lock: Mutex<()>,
    #[cfg(test)]
    pack_prefetch_spawns: std::sync::atomic::AtomicUsize,
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
        let (meta, tail_start, tail) = StatamFileMeta::read_from_with_tail(reader.as_ref(), None)?;
        let retain_tail = reader.is_remote()
            && tail_start < meta.hot_tail_start
            && tail.len() <= crate::io::MAX_RETAINED_TAIL_BYTES;
        let reader: Arc<dyn RangeReader> = if retain_tail {
            Arc::new(crate::io::TailCachedReader::new(reader, tail_start, tail)?)
        } else {
            reader
        };
        let mut this = Self::open_with_meta(reader, meta, packs)?;
        this.retained_tail_start = retain_tail.then_some(tail_start);
        Ok(this)
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
            meta: Arc::new(meta),
            packs,
            retained_tail_start: None,
            caches: Mutex::new(caches),
            pack_init_lock: Mutex::new(()),
            #[cfg(test)]
            pack_prefetch_spawns: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    #[cfg(test)]
    pub(super) fn pack_prefetch_spawns(&self) -> usize {
        self.pack_prefetch_spawns
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn has_uncached_packs(&self) -> bool {
        let guard = self.caches.lock().unwrap_or_else(|p| p.into_inner());
        self.meta
            .schema_meta
            .pack_ids
            .iter()
            .any(|id| !guard.packs.contains_key(id))
    }

    fn should_prefetch_packs(&self, eligible_query: bool) -> bool {
        eligible_query
            && self.reader.is_remote()
            && self.packs.is_some()
            && !self.meta.schema_meta.pack_ids.is_empty()
            && self.meta.schema_meta.pack_ids.len() <= 4
            && self.has_uncached_packs()
    }

    /// Speculatively opens uncached `.stpk` packs (`ChunkPackReader::open` ONLY — never
    /// loading Zstd dicts or trigram sections, preserving single-wave dict+frames coalescing).
    /// Errors are ignored so 0-match queries never fail on missing packs.
    fn prefetch_packs(&self) {
        for &pack_id in &self.meta.schema_meta.pack_ids {
            let _ = self.resolve_pack(pack_id);
        }
    }

    fn fetch_blocks_and_lazy_maybe_prefetch_packs(
        &self,
        block_ranges: &[(u64, u64)],
        bloom_cols: &[usize],
        trigram_cols: &[usize],
        dict_cols: &[usize],
        eligible_for_pack_prefetch: bool,
    ) -> Result<HashMap<(u64, u64), Bytes>, StatamError> {
        if !self.should_prefetch_packs(eligible_for_pack_prefetch)
            || (block_ranges.is_empty()
                && bloom_cols.is_empty()
                && trigram_cols.is_empty()
                && dict_cols.is_empty())
        {
            return self.fetch_blocks_and_lazy(block_ranges, bloom_cols, trigram_cols, dict_cols);
        }
        #[cfg(test)]
        self.pack_prefetch_spawns
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::thread::scope(|s| {
            s.spawn(|| self.prefetch_packs());
            self.fetch_blocks_and_lazy(block_ranges, bloom_cols, trigram_cols, dict_cols)
        })
    }

    fn collect_p2_candidate_ranges_and_dicts(
        &self,
        rg_idx: usize,
        mbs: &[usize],
        plan: &RgIoPlan,
        p1_l0_needed: bool,
        l0_end: u64,
        has_payload_proj: bool,
    ) -> Result<P2CandidateFetch, StatamError> {
        let rg = &self.meta.rg_dir[rg_idx];
        let dict_cols: Vec<usize> = plan
            .phase2_only_phys_cols
            .iter()
            .copied()
            .filter(|&pidx| {
                (self.meta.col_dir[pidx].col_flags & COL_FLAG_HAS_FILE_DICT) != 0
                    && mbs
                        .iter()
                        .any(|&mb| self.meta.rg_blocks(pidx, rg_idx)[mb].encoding == Encoding::Dict)
            })
            .collect();

        let mut ranges = Vec::new();
        let mut bytes = 0u64;
        let l0_needed = rg.l0_payload_len > 0 && !p1_l0_needed && has_payload_proj;
        for &mb_idx in mbs {
            for &pidx in &plan.phase2_only_phys_cols {
                if self
                    .meta
                    .schema_meta
                    .schema
                    .fields()
                    .get(pidx)
                    .is_some_and(|f| f.data_type == DataType::MapUtf8)
                    && plan.phase2_map_fast_path.contains(&(mb_idx, pidx))
                {
                    continue;
                }
                let blk = &self.meta.rg_blocks(pidx, rg_idx)[mb_idx];
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
                    ranges.push((blk.offset, end));
                    bytes = bytes.saturating_add(total);
                }
            }
        }
        if l0_needed {
            ranges.push((rg.l0_payload_offset, l0_end));
            bytes = bytes.saturating_add(u64::from(rg.l0_payload_len));
        }
        Ok(P2CandidateFetch {
            ranges,
            bytes,
            dict_cols,
            l0_needed,
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
            let _ = read_framed_block_checked_cow(&frame, blk, MAX_BLOCK_RAW_BYTES)?;
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
                let l0 = ParsedL0Payload::parse(bytes)?;
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

        // Resolve & validate all query columns BEFORE any empty-file or empty-result early exit.
        let time_col_idx = if req.time_range.is_all() {
            None
        } else {
            find_time_column(schema)
        };

        let mut early_empty = req.time_range.is_empty()
            || self.meta.postscript.row_count == 0
            || req.limit == Some(0);

        let mut resolved_lookups = Vec::with_capacity(req.id_lookups.len());
        for lk in &req.id_lookups {
            let cidx = schema
                .index_of(&lk.column)
                .ok_or(StatamError::SchemaMismatch {
                    reason: "id_lookup column not found in schema",
                })?;
            if !is_supported_id_lookup_type(&schema.fields()[cidx].data_type)
                || self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload
            {
                return Err(StatamError::SchemaMismatch {
                    reason: "id_lookup on unsupported column data type or ExternalPayload column",
                });
            }
            if lk.values.is_empty() {
                early_empty = true;
            }
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
            if self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload
                && !matches!(pred.pred, ZonePred::IsNull | ZonePred::IsNotNull)
            {
                return Err(StatamError::SchemaMismatch {
                    reason: "value predicate not supported on ExternalPayload column",
                });
            }
            if is_always_false_pred(&pred.pred) {
                early_empty = true;
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
        let map_keys_by_col = Arc::new(map_keys_by_col);

        if early_empty {
            return Ok(Vec::new());
        }

        // Step 1: Precompute lookup hashes, RG order, and coalescing mode.
        let lookup_hashes: Vec<(usize, Vec<u64>)> = resolved_lookups
            .iter()
            .map(|&(cidx, lk)| {
                let hashes = lk
                    .values
                    .iter()
                    .map(|v| statam_bloom_hash(v))
                    .collect::<Vec<_>>();
                (cidx, hashes)
            })
            .collect();

        // Step 2: Determine RG iteration order & prune via sort key + zone maps.
        let num_rgs = self.meta.rg_dir.len();
        let mut rg_order: Vec<usize> =
            self.candidate_rg_range(time_col_idx, req.time_range, &resolved_preds);
        let can_reverse_early_stop =
            req.sort_desc_hint && req.limit.is_some() && self.is_sort_col0_mb_monotonic();
        let desc_limit = can_reverse_early_stop;
        let effective_limit = if req.sort_desc_hint && !can_reverse_early_stop {
            None
        } else {
            req.limit
        };
        if desc_limit {
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

        let bloom_cols: Vec<usize> = lookup_hashes.iter().map(|&(cidx, _)| cidx).collect();
        let sub_cols_with_trigram: Vec<usize> = resolved_subs
            .iter()
            .filter_map(|&(cidx, _)| {
                ((self.meta.col_dir[cidx].col_flags & COL_FLAG_HAS_TRIGRAM_INDEX) != 0)
                    .then_some(cidx)
            })
            .collect();

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

        // Combine pre-scan blooms + narrow trigrams + Phase-1 dicts into ONE wave when
        // bloom/narrow-trigram predicates exist; otherwise consume `inline_lazy` (0 I/O)
        // and piggyback any remaining lazy Phase-1 dicts onto Wave 1 alongside Phase-1 blocks.
        if !bloom_cols.is_empty() || !sub_cols_with_trigram.is_empty() {
            self.ensure_lazy_indexes_loaded(
                &bloom_cols,
                &sub_cols_with_trigram,
                &preload_dict_cols,
            )?;
            if !lookup_hashes.is_empty() && !self.blooms_may_match(&lookup_hashes) {
                return Ok(Vec::new());
            }
        } else if !preload_dict_cols.is_empty() {
            self.load_inline_lazy_indexes(&[], &[], &preload_dict_cols)?;
        }

        // Precompute matching-code Bitmap for any already-cached dict-encoded substring query.
        let mut dict_sub_bitmaps = self.build_dict_sub_bitmaps(&resolved_subs);

        let has_filters = time_col_idx.is_some()
            || !resolved_lookups.is_empty()
            || !resolved_preds.is_empty()
            || !resolved_subs.is_empty();

        // Cross-RG batched I/O path for unbounded Scan/Auto queries (collapses multi-RG RTTs).
        if effective_limit.is_none() && matches!(mode, CoalesceMode::Scan | CoalesceMode::Auto) {
            let mut surviving_rgs: Vec<(usize, Vec<usize>)> = Vec::new();
            for rg_idx in rg_order {
                if !lookup_hashes.is_empty() && !self.rg_blooms_may_match(rg_idx, &lookup_hashes) {
                    continue;
                }
                let mb_count = self.meta.rg_blocks(0, rg_idx).len();
                if mb_count == 0 {
                    continue;
                }
                let surviving_mbs = self.prune_rg_mini_blocks(
                    rg_idx,
                    mb_count,
                    time_col_idx,
                    req.time_range,
                    &resolved_lookups,
                    &resolved_preds,
                    &resolved_subs,
                    &dict_sub_bitmaps,
                )?;
                if !surviving_mbs.is_empty() {
                    surviving_rgs.push((rg_idx, surviving_mbs));
                }
            }
            if surviving_rgs.is_empty() {
                return Ok(Vec::new());
            }
            return if !has_filters {
                self.scan_rgs_unfiltered_batched(
                    surviving_rgs,
                    &proj_indices,
                    &proj_schema,
                    &map_keys_by_col,
                    req.read_threads,
                )
            } else {
                self.scan_rgs_filtered_batched(
                    surviving_rgs,
                    &proj_indices,
                    &proj_schema,
                    &map_keys_by_col,
                    time_col_idx,
                    req.time_range,
                    &resolved_lookups,
                    &resolved_preds,
                    &resolved_subs,
                    &mut dict_sub_bitmaps,
                    req.read_threads,
                )
            };
        }

        let mut out_batches = Vec::new();
        let mut emitted_rows = 0usize;

        for rg_idx in rg_order {
            if let Some(lim) = effective_limit
                && emitted_rows >= lim
            {
                break;
            }

            if !lookup_hashes.is_empty() && !self.rg_blooms_may_match(rg_idx, &lookup_hashes) {
                continue;
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
            }
            if let Some(lim) = effective_limit
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

            let rg_limit_remaining = effective_limit.map(|lim| lim.saturating_sub(emitted_rows));
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
                    &mut dict_sub_bitmaps,
                    rg_limit_remaining,
                    desc_limit,
                    req.read_threads,
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
                    &mut dict_sub_bitmaps,
                    rg_limit_remaining,
                    desc_limit,
                    req.read_threads,
                )?,
            };

            for batch in rg_batches {
                if batch.is_empty() {
                    continue;
                }
                if let Some(lim) = effective_limit {
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

    /// Materializes the requested global `row_indices` (0-based across the file) with optional
    /// column projection, using coalesced range reads and sparse decoding per touched mini-block.
    ///
    /// Out-of-bounds indices (`>= row_count`) are ignored; valid indices are deduplicated and
    /// returned in ascending file order, coalesced into compact [`Batch`]es bounded by both
    /// `<= 65_536` rows and `<= 64 MiB` estimated bytes per batch (or one 0-column [`Batch`]
    /// when `projection` is `Some(&[])`).
    pub fn take_rows(
        &self,
        row_indices: &[u64],
        projection: Option<&[usize]>,
        read_threads: usize,
    ) -> Result<Vec<Batch>, StatamError> {
        let schema = &self.meta.schema_meta.schema;
        let num_logical = schema.len();

        let proj_indices: Vec<usize> = match projection {
            Some(v) => {
                for &idx in v {
                    if idx >= num_logical {
                        return Err(StatamError::SchemaMismatch {
                            reason: "projection column index out of bounds",
                        });
                    }
                }
                v.to_vec()
            }
            None => (0..num_logical).collect(),
        };
        let proj_schema = Arc::new(schema.project(&proj_indices)?);

        let total_rows = self.meta.postscript.row_count;
        if row_indices.is_empty()
            || total_rows == 0
            || self.meta.col_dir.is_empty()
            || self.meta.rg_dir.is_empty()
        {
            return Ok(Vec::new());
        }

        let mut sorted_rows: Vec<u64> = row_indices
            .iter()
            .copied()
            .filter(|&r| r < total_rows)
            .collect();
        if sorted_rows.is_empty() {
            return Ok(Vec::new());
        }
        sorted_rows.sort_unstable();
        sorted_rows.dedup();

        // Map each sorted global row index -> (rg_idx, mb_idx, local_row) via binary search.
        let mut touched_rgs: Vec<(usize, Vec<TouchedMbRows>)> = Vec::new();
        for row_idx in sorted_rows {
            let rg_idx = self
                .meta
                .rg_dir
                .partition_point(|rg| u64::from(rg.first_row) <= row_idx)
                .saturating_sub(1);
            let mb_blks = self.meta.rg_blocks(0, rg_idx);
            let mb_idx = mb_blks
                .partition_point(|b| u64::from(b.first_row) <= row_idx)
                .saturating_sub(1);
            let mb_blk = mb_blks.get(mb_idx).ok_or(StatamError::Corrupt {
                reason: "mini-block index out of bounds during take_rows",
            })?;
            let local_row = row_idx
                .checked_sub(u64::from(mb_blk.first_row))
                .and_then(|v| usize::try_from(v).ok())
                .ok_or(StatamError::Corrupt {
                    reason: "row index precedes mini-block first_row",
                })?;
            if local_row >= mb_blk.row_count as usize {
                return Err(StatamError::Corrupt {
                    reason: "row index exceeds mini-block row_count",
                });
            }

            let mb_list = if touched_rgs.last().is_some_and(|(r, _)| *r == rg_idx) {
                &mut touched_rgs
                    .last_mut()
                    .ok_or(StatamError::Corrupt {
                        reason: "missing touched row group entry",
                    })?
                    .1
            } else {
                touched_rgs.push((rg_idx, Vec::new()));
                &mut touched_rgs
                    .last_mut()
                    .ok_or(StatamError::Corrupt {
                        reason: "missing touched row group entry",
                    })?
                    .1
            };
            let local_indices = if mb_list.last().is_some_and(|(m, _)| *m == mb_idx) {
                &mut mb_list
                    .last_mut()
                    .ok_or(StatamError::Corrupt {
                        reason: "missing touched mini-block entry",
                    })?
                    .1
            } else {
                mb_list.push((mb_idx, Vec::new()));
                &mut mb_list
                    .last_mut()
                    .ok_or(StatamError::Corrupt {
                        reason: "missing touched mini-block entry",
                    })?
                    .1
            };
            local_indices.push(local_row);
        }

        struct TouchedRgPlan {
            rg_idx: usize,
            mb_rows: Vec<(usize, Vec<usize>)>,
            plan: Arc<RgIoPlan>,
            ranges: Vec<(u64, u64)>,
            rg_bytes: u64,
            need_l0_payload: bool,
            l0_end: u64,
            dict_cols: Vec<usize>,
        }

        let empty_map_keys: Arc<HashMap<usize, Vec<String>>> = Arc::new(HashMap::new());
        let mut touched_with_plans: Vec<TouchedRgPlan> = Vec::with_capacity(touched_rgs.len());
        for (rg_idx, mb_rows) in touched_rgs {
            let surviving_mbs: Vec<usize> = mb_rows.iter().map(|(mb, _)| *mb).collect();
            let plan = Arc::new(self.build_rg_io_plan(
                rg_idx,
                &surviving_mbs,
                &proj_indices,
                &empty_map_keys,
                None,
                TimeRange::ALL,
                &[],
                &[],
                &[],
            ));
            let mut dict_cols = Vec::new();
            for &pidx in plan.all_phys_cols() {
                if (self.meta.col_dir[pidx].col_flags & COL_FLAG_HAS_FILE_DICT) != 0 {
                    let blks = self.meta.rg_blocks(pidx, rg_idx);
                    if surviving_mbs
                        .iter()
                        .any(|&mb| blks[mb].encoding == Encoding::Dict)
                    {
                        dict_cols.push(pidx);
                    }
                }
            }

            let rg = &self.meta.rg_dir[rg_idx];
            let mut ranges = Vec::new();
            let mut rg_bytes = 0u64;
            let mut need_l0_payload = false;
            for &mb_idx in &surviving_mbs {
                for &pidx in plan.all_phys_cols() {
                    let blk = &self.meta.rg_blocks(pidx, rg_idx)[mb_idx];
                    if blk.stored_len > 0 {
                        let total = (BLOCK_FRAME_HEADER_LEN as u64)
                            .checked_add(u64::from(blk.stored_len))
                            .ok_or(StatamError::BoundsExceeded {
                                reason: "take_rows block length overflow",
                            })?;
                        let end =
                            blk.offset
                                .checked_add(total)
                                .ok_or(StatamError::BoundsExceeded {
                                    reason: "take_rows block end overflow",
                                })?;
                        ranges.push((blk.offset, end));
                        rg_bytes = rg_bytes.saturating_add(total);
                    }
                    if self.meta.col_dir[pidx].locality_group == LocalityGroup::ExternalPayload
                        && rg.l0_payload_len > 0
                    {
                        need_l0_payload = true;
                    }
                }
            }
            let l0_end = rg
                .l0_payload_offset
                .checked_add(u64::from(rg.l0_payload_len))
                .ok_or(StatamError::BoundsExceeded {
                    reason: "take_rows L0 payload end overflow",
                })?;
            if need_l0_payload {
                ranges.push((rg.l0_payload_offset, l0_end));
                rg_bytes = rg_bytes.saturating_add(u64::from(rg.l0_payload_len));
            }

            touched_with_plans.push(TouchedRgPlan {
                rg_idx,
                mb_rows,
                plan,
                ranges,
                rg_bytes,
                need_l0_payload,
                l0_end,
                dict_cols,
            });
        }

        let mut proj_payload_cols: Vec<usize> = proj_indices
            .iter()
            .copied()
            .filter(|&cidx| {
                self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload
            })
            .collect();
        proj_payload_cols.sort_unstable();
        proj_payload_cols.dedup();
        let has_payload_proj = !proj_payload_cols.is_empty();

        // Partition touched RGs into <= 64 MiB `.statam` I/O waves.
        let mut io_waves: Vec<Vec<TouchedRgPlan>> = Vec::new();
        let mut cur_wave: Vec<TouchedRgPlan> = Vec::new();
        let mut cur_wave_bytes = 0u64;
        for item in touched_with_plans {
            if !cur_wave.is_empty()
                && cur_wave_bytes.saturating_add(item.rg_bytes) > IO_WAVE_MAX_BYTES
            {
                io_waves.push(std::mem::take(&mut cur_wave));
                cur_wave_bytes = 0;
            }
            cur_wave_bytes = cur_wave_bytes.saturating_add(item.rg_bytes);
            cur_wave.push(item);
        }
        if !cur_wave.is_empty() {
            io_waves.push(cur_wave);
        }

        let mut out_batches = Vec::new();
        for wave in io_waves {
            let total_ranges: usize = wave.iter().map(|w| w.ranges.len()).sum();
            let mut wave_ranges = Vec::with_capacity(total_ranges);
            let mut wave_dict_cols = Vec::new();
            for item in &wave {
                wave_ranges.extend_from_slice(&item.ranges);
                wave_dict_cols.extend_from_slice(&item.dict_cols);
            }

            // Fetch ALL missing lazy dicts + ALL touched mini-blocks + L0 payloads across
            // all RGs in this wave in ONE `reader.read_ranges(...)` call (overlapping `.stpk`
            // open-only prefetch on remote readers when a non-ConstNull ExternalPayload is projected).
            let eligible_for_pack_prefetch = has_payload_proj
                && wave.iter().any(|item| {
                    proj_payload_cols.iter().any(|&cidx| {
                        let blks = self.meta.rg_blocks(cidx, item.rg_idx);
                        item.mb_rows
                            .iter()
                            .any(|(mb_idx, _)| blks[*mb_idx].encoding != Encoding::ConstNull)
                    })
                });
            let block_cache = Arc::new(self.fetch_blocks_and_lazy_maybe_prefetch_packs(
                &wave_ranges,
                &[],
                &[],
                &wave_dict_cols,
                eligible_for_pack_prefetch,
            )?);

            let block_reader = BlockCacheReader::Map {
                primary: Arc::clone(&block_cache),
                fallback: None,
                missing_reason: "missing take_rows fetched block",
                overflow_reason: "take_rows block key overflow",
            };

            if !has_payload_proj {
                let empty_chunks = ResolvedChunks::empty();
                for item in wave {
                    let take_mbs = item
                        .mb_rows
                        .into_iter()
                        .map(|(mb_idx, local_indices)| {
                            (mb_idx, local_indices, None, HashMap::new())
                        })
                        .collect();
                    let mut rg_batches = self.materialize_take_mbs(
                        item.rg_idx,
                        take_mbs,
                        &item.plan,
                        &proj_indices,
                        &proj_schema,
                        &empty_map_keys,
                        &empty_chunks,
                        &block_reader,
                        read_threads,
                    )?;
                    out_batches.append(&mut rg_batches);
                }
                continue;
            }

            // Stage all touched RGs and decode PayloadRefs across the entire wave in ONE parallel
            // scope (0 I/O: all blocks are already in `block_cache`).
            struct StagedTakeRg {
                rg_idx: usize,
                mb_rows: Vec<(usize, Vec<usize>)>,
                plan: Arc<RgIoPlan>,
                need_l0_payload: bool,
                l0_end: u64,
                active_mbs: Vec<ActiveMiniBlock>,
            }

            let mut stage: Vec<StagedTakeRg> = Vec::with_capacity(wave.len());
            // (rg_pos, mb_pos, rg_idx, mb_idx, cidx, is_sparse, raw_len)
            let mut ref_tasks: Vec<(usize, usize, usize, usize, usize, bool, usize)> = Vec::new();
            let mut ref_work_bytes = 0usize;

            for item in wave {
                let rg_pos = stage.len();
                let mut active_mbs: Vec<ActiveMiniBlock> = Vec::with_capacity(item.mb_rows.len());
                for (mb_pos, (mb_idx, local_indices)) in item.mb_rows.iter().enumerate() {
                    let mb_row_count =
                        self.meta.rg_blocks(0, item.rg_idx)[*mb_idx].row_count as usize;
                    let mask = mask_from_indices(mb_row_count, local_indices);
                    let is_sparse =
                        !mask.all_ones() && mask.count_ones().saturating_mul(8) < mb_row_count;
                    let mut decoded_p1: HashMap<usize, DecodedPhase1> =
                        HashMap::with_capacity(proj_payload_cols.len());
                    for &cidx in &proj_payload_cols {
                        let blk = &self.meta.rg_blocks(cidx, item.rg_idx)[*mb_idx];
                        if blk.encoding == Encoding::ConstNull {
                            if is_sparse {
                                let selected =
                                    mask.iter_ones().map(|r| (r, PayloadRefRow::Null)).collect();
                                decoded_p1
                                    .insert(cidx, DecodedPhase1::SelectedPayloadRefs(selected));
                            } else {
                                decoded_p1.insert(
                                    cidx,
                                    DecodedPhase1::FlatPayloadRefs(FlatPayloadRefs::all_null(
                                        mb_row_count,
                                    )),
                                );
                            }
                        } else {
                            let raw_len = blk.raw_len as usize;
                            ref_work_bytes = ref_work_bytes.saturating_add(raw_len);
                            ref_tasks.push((
                                rg_pos,
                                mb_pos,
                                item.rg_idx,
                                *mb_idx,
                                cidx,
                                is_sparse,
                                raw_len,
                            ));
                        }
                    }
                    active_mbs.push((*mb_idx, mask, decoded_p1));
                }
                stage.push(StagedTakeRg {
                    rg_idx: item.rg_idx,
                    mb_rows: item.mb_rows,
                    plan: item.plan,
                    need_l0_payload: item.need_l0_payload,
                    l0_end: item.l0_end,
                    active_mbs,
                });
            }

            if !ref_tasks.is_empty() {
                let ref_workers = effective_read_threads(ref_work_bytes, read_threads);
                if ref_tasks.len() >= 2 && ref_workers > 1 && !in_parallel_scope() {
                    ref_tasks.sort_unstable_by_key(|a| std::cmp::Reverse(a.6));
                    let mut sparse_masks: Vec<Vec<Option<Arc<Bitmap>>>> = stage
                        .iter()
                        .map(|s| vec![None; s.active_mbs.len()])
                        .collect();
                    let static_ref_tasks: Vec<_> = ref_tasks
                        .into_iter()
                        .map(|(rg_pos, mb_pos, rg_idx, mb_idx, cidx, is_sparse, _)| {
                            let mask_opt = if is_sparse {
                                let slot = &mut sparse_masks[rg_pos][mb_pos];
                                let arc = slot.get_or_insert_with(|| {
                                    Arc::new(stage[rg_pos].active_mbs[mb_pos].1.clone())
                                });
                                Some(Arc::clone(arc))
                            } else {
                                None
                            };
                            (rg_pos, mb_pos, rg_idx, mb_idx, cidx, mask_opt)
                        })
                        .collect();
                    let meta = Arc::clone(&self.meta);
                    let br = block_reader.clone();
                    let decoded_refs = parallel_map_in_order_static(
                        static_ref_tasks,
                        ref_workers,
                        move |(rg_pos, mb_pos, rg_idx, mb_idx, cidx, mask_opt)| {
                            let mb_row_count = meta.rg_blocks(0, rg_idx)[mb_idx].row_count as usize;
                            let blk = &meta.rg_blocks(cidx, rg_idx)[mb_idx];
                            let raw = br.read_block(blk)?;
                            let item = if let Some(mask) = mask_opt.as_deref() {
                                let selected = decode_payload_refs_selected(
                                    &raw,
                                    mb_row_count,
                                    mask,
                                    MAX_BATCH_DECODED_BYTES,
                                )?;
                                DecodedPhase1::SelectedPayloadRefs(selected)
                            } else {
                                let refs = decode_payload_refs_flat(
                                    &raw,
                                    mb_row_count,
                                    MAX_BATCH_DECODED_BYTES,
                                )?;
                                DecodedPhase1::FlatPayloadRefs(refs)
                            };
                            Ok((rg_pos, mb_pos, cidx, item))
                        },
                    )?;
                    for (rg_pos, mb_pos, cidx, item) in decoded_refs {
                        stage[rg_pos].active_mbs[mb_pos].2.insert(cidx, item);
                    }
                } else {
                    for (rg_pos, mb_pos, rg_idx, mb_idx, cidx, is_sparse, _) in ref_tasks {
                        let mb_row_count =
                            self.meta.rg_blocks(0, rg_idx)[mb_idx].row_count as usize;
                        let blk = &self.meta.rg_blocks(cidx, rg_idx)[mb_idx];
                        let raw = block_reader.read_block(blk)?;
                        let item = if is_sparse {
                            let mask = &stage[rg_pos].active_mbs[mb_pos].1;
                            let selected = decode_payload_refs_selected(
                                &raw,
                                mb_row_count,
                                mask,
                                MAX_BATCH_DECODED_BYTES,
                            )?;
                            DecodedPhase1::SelectedPayloadRefs(selected)
                        } else {
                            let refs = decode_payload_refs_flat(
                                &raw,
                                mb_row_count,
                                MAX_BATCH_DECODED_BYTES,
                            )?;
                            DecodedPhase1::FlatPayloadRefs(refs)
                        };
                        stage[rg_pos].active_mbs[mb_pos].2.insert(cidx, item);
                    }
                }
            }

            struct PreparedTakeRg {
                rg_idx: usize,
                mb_rows: Vec<(usize, Vec<usize>)>,
                plan: Arc<RgIoPlan>,
                l0_parsed: Option<ParsedL0Payload>,
                active_mbs: Vec<ActiveMiniBlock>,
                ext_by_pack: BTreeMap<u16, BTreeSet<u32>>,
                l0_cids: BTreeSet<u32>,
                l0_chunk_bytes: usize,
                l0_frame_bytes: usize,
                max_mb_col_decoded_bytes: usize,
            }

            let mut prepared: Vec<PreparedTakeRg> = Vec::with_capacity(stage.len());
            let mut merged_ext_by_pack: BTreeMap<u16, BTreeSet<u32>> = BTreeMap::new();
            let mut total_l0_chunk_bytes = 0usize;
            let mut total_l0_frame_bytes = 0usize;
            let mut max_wave_col_decoded_bytes = 0usize;
            for item in stage {
                let rg = &self.meta.rg_dir[item.rg_idx];
                let l0_parsed = if item.need_l0_payload {
                    block_cache
                        .get(&(rg.l0_payload_offset, item.l0_end))
                        .map(|b| ParsedL0Payload::parse(b.clone()))
                        .transpose()?
                } else {
                    None
                };
                let (ext_by_pack, l0_cids) =
                    Self::collect_chunk_refs_for_mbs(&item.active_mbs, &proj_payload_cols);
                for (&pidx, cids) in &ext_by_pack {
                    merged_ext_by_pack.entry(pidx).or_default().extend(cids);
                }
                let (l0_chunk_bytes, l0_frame_bytes) = if l0_cids.is_empty() {
                    (0, 0)
                } else {
                    let l0 = l0_parsed.as_ref().ok_or(StatamError::Corrupt {
                        reason: "missing L0 local payload region",
                    })?;
                    l0.measure_chunks_bytes(&l0_cids)?
                };
                let max_mb_col_decoded_bytes = self.max_mb_col_decoded_bytes(
                    &item.active_mbs,
                    &proj_payload_cols,
                    l0_parsed.as_ref(),
                )?;
                total_l0_chunk_bytes = total_l0_chunk_bytes.saturating_add(l0_chunk_bytes);
                total_l0_frame_bytes = total_l0_frame_bytes.saturating_add(l0_frame_bytes);
                max_wave_col_decoded_bytes =
                    max_wave_col_decoded_bytes.max(max_mb_col_decoded_bytes);
                prepared.push(PreparedTakeRg {
                    rg_idx: item.rg_idx,
                    mb_rows: item.mb_rows,
                    plan: item.plan,
                    l0_parsed,
                    active_mbs: item.active_mbs,
                    ext_by_pack,
                    l0_cids,
                    l0_chunk_bytes,
                    l0_frame_bytes,
                    max_mb_col_decoded_bytes,
                });
            }

            let materialize_prepared_rg = |prep: PreparedTakeRg,
                                           ext_chunks: &mut ResolvedChunks,
                                           ext_chunk_bytes: usize,
                                           ext_frame_bytes: usize|
             -> Result<Vec<Batch>, StatamError> {
                ext_chunks.clear_l0();
                let take_mbs = prep
                    .mb_rows
                    .into_iter()
                    .zip(prep.active_mbs)
                    .map(|((mb_idx, local_indices), (_, mask, p1))| {
                        (mb_idx, local_indices, Some(mask), p1)
                    })
                    .collect();
                if !prep.l0_cids.is_empty() {
                    let l0 = prep.l0_parsed.as_ref().ok_or(StatamError::Corrupt {
                        reason: "missing L0 local payload region",
                    })?;
                    let l0_vec: Vec<u32> = prep.l0_cids.into_iter().collect();
                    let mut combined_chunk_bytes = ext_chunk_bytes;
                    let mut combined_frame_bytes = ext_frame_bytes;
                    l0.check_chunks_budget(
                        &l0_vec,
                        &mut combined_chunk_bytes,
                        &mut combined_frame_bytes,
                    )?;
                    let l0_entries = l0.read_chunks_vec(&l0_vec)?;
                    ext_chunks.set_l0(l0_entries);
                }
                let res = self.materialize_take_mbs(
                    prep.rg_idx,
                    take_mbs,
                    &prep.plan,
                    &proj_indices,
                    &proj_schema,
                    &empty_map_keys,
                    ext_chunks,
                    &block_reader,
                    read_threads,
                );
                ext_chunks.clear_l0();
                res
            };

            // Fast path: all external `.stpk` + L0 local chunks across every RG in this wave fit
            // in <= 128 MiB -> resolve ALL external packs in ONE coalesced wave per pack!
            let (total_ext_chunk_bytes, total_ext_frame_bytes) =
                self.measure_external_pack_bytes(&merged_ext_by_pack)?;
            let total_chunk_bytes = total_ext_chunk_bytes.saturating_add(total_l0_chunk_bytes);
            let total_frame_bytes = total_ext_frame_bytes.saturating_add(total_l0_frame_bytes);
            if total_chunk_bytes <= PHASE2_PAYLOAD_WAVE_TARGET_BYTES
                && total_frame_bytes <= PHASE2_PAYLOAD_WAVE_TARGET_BYTES
                && max_wave_col_decoded_bytes <= PHASE2_PAYLOAD_WAVE_TARGET_BYTES
            {
                let threads = read_threads.clamp(1, MAX_PARALLEL_THREADS);
                if threads > 1
                    && !in_parallel_scope()
                    && prepared.len() == 1
                    && prepared[0].l0_cids.is_empty()
                    && !merged_ext_by_pack.is_empty()
                    && !proj_indices.is_empty()
                {
                    let prep = prepared.pop().ok_or(StatamError::Corrupt {
                        reason: "missing single prepared take RG",
                    })?;
                    let mut rg_batches = self.resolve_and_materialize_take_rg_fused(
                        prep.rg_idx,
                        prep.mb_rows,
                        prep.active_mbs,
                        &prep.plan,
                        &proj_indices,
                        &proj_schema,
                        &empty_map_keys,
                        merged_ext_by_pack,
                        &block_reader,
                        threads,
                    )?;
                    out_batches.append(&mut rg_batches);
                } else {
                    let mut wave_ext_chunks =
                        self.resolve_external_pack_chunks(merged_ext_by_pack, read_threads)?;
                    for prep in prepared {
                        let mut rg_batches = materialize_prepared_rg(
                            prep,
                            &mut wave_ext_chunks,
                            total_ext_chunk_bytes,
                            total_ext_frame_bytes,
                        )?;
                        out_batches.append(&mut rg_batches);
                    }
                }
            } else {
                // Wave-bounded fallback (> 128 MiB of external and/or L0 blobs): partition RGs
                // into <= 128 MiB payload sub-waves, splitting any > 128 MiB single RG via
                // `split_payload_waves_multi`.
                let empty_pre = HashMap::new();
                let mut sub_rgs: Vec<PreparedTakeRg> = Vec::new();
                let mut sub_ext: BTreeMap<u16, BTreeSet<u32>> = BTreeMap::new();
                let mut sub_l0_c = 0usize;
                let mut sub_l0_f = 0usize;

                let flush_sub_rgs = |rgs: &mut Vec<PreparedTakeRg>,
                                     ext: &mut BTreeMap<u16, BTreeSet<u32>>,
                                     l0_c: &mut usize,
                                     l0_f: &mut usize,
                                     out: &mut Vec<Batch>|
                 -> Result<(), StatamError> {
                    if rgs.is_empty() {
                        return Ok(());
                    }
                    let taken_ext = std::mem::take(ext);
                    let (ext_c, ext_f) = self.measure_external_pack_bytes(&taken_ext)?;
                    let mut ext_chunks =
                        self.resolve_external_pack_chunks(taken_ext, read_threads)?;
                    *l0_c = 0;
                    *l0_f = 0;
                    for prep in rgs.drain(..) {
                        let mut rg_batches =
                            materialize_prepared_rg(prep, &mut ext_chunks, ext_c, ext_f)?;
                        out.append(&mut rg_batches);
                    }
                    Ok(())
                };

                for prep in prepared {
                    let (rg_ext_c, rg_ext_f) =
                        self.measure_external_pack_bytes(&prep.ext_by_pack)?;
                    let rg_total_c = rg_ext_c.saturating_add(prep.l0_chunk_bytes);
                    let rg_total_f = rg_ext_f.saturating_add(prep.l0_frame_bytes);
                    if rg_total_c > PHASE2_PAYLOAD_WAVE_TARGET_BYTES
                        || rg_total_f > PHASE2_PAYLOAD_WAVE_TARGET_BYTES
                        || prep.max_mb_col_decoded_bytes > PHASE2_PAYLOAD_WAVE_TARGET_BYTES
                    {
                        flush_sub_rgs(
                            &mut sub_rgs,
                            &mut sub_ext,
                            &mut sub_l0_c,
                            &mut sub_l0_f,
                            &mut out_batches,
                        )?;
                        let pwaves = self.split_payload_waves_multi(
                            &prep.active_mbs,
                            &proj_payload_cols,
                            prep.l0_parsed.as_ref(),
                        )?;
                        let mut slots: Vec<Option<ActiveMiniBlock>> =
                            prep.active_mbs.into_iter().map(Some).collect();
                        let mut last_wave = vec![0usize; slots.len()];
                        for (wi, pw) in pwaves.iter().enumerate() {
                            for &(pos, _) in pw {
                                last_wave[pos] = wi;
                            }
                        }
                        for (wi, pw) in pwaves.into_iter().enumerate() {
                            let mut wave_mbs = Vec::with_capacity(pw.len());
                            for (pos, sub_mask) in pw {
                                if last_wave[pos] == wi {
                                    if let Some((mb_idx, _, p1)) = slots[pos].take() {
                                        wave_mbs.push((mb_idx, sub_mask, p1));
                                    }
                                } else if let Some((mb_idx, _, p1)) = slots[pos].as_ref() {
                                    let mut cloned_p1 = HashMap::with_capacity(p1.len());
                                    for (&k, v) in p1 {
                                        match v {
                                            DecodedPhase1::FlatPayloadRefs(r) => {
                                                cloned_p1.insert(
                                                    k,
                                                    DecodedPhase1::FlatPayloadRefs(r.clone()),
                                                );
                                            }
                                            DecodedPhase1::SelectedPayloadRefs(s) => {
                                                cloned_p1.insert(
                                                    k,
                                                    DecodedPhase1::SelectedPayloadRefs(s.clone()),
                                                );
                                            }
                                            DecodedPhase1::Col(c) => {
                                                cloned_p1.insert(k, DecodedPhase1::Col(c.clone()));
                                            }
                                            DecodedPhase1::ShreddedSub(_) => {}
                                        }
                                    }
                                    wave_mbs.push((*mb_idx, sub_mask, cloned_p1));
                                }
                            }
                            let chunks = self.resolve_rg_payload_chunks(
                                prep.rg_idx,
                                &mut wave_mbs,
                                &proj_indices,
                                prep.l0_parsed.as_ref(),
                                &empty_pre,
                                &block_reader,
                                read_threads,
                            )?;
                            let take_mbs = wave_mbs
                                .into_iter()
                                .map(|(mb_idx, sub_mask, p1)| {
                                    let idxs: Vec<usize> = sub_mask.iter_ones().collect();
                                    (mb_idx, idxs, Some(sub_mask), p1)
                                })
                                .collect();
                            let mut rg_batches = self.materialize_take_mbs(
                                prep.rg_idx,
                                take_mbs,
                                &prep.plan,
                                &proj_indices,
                                &proj_schema,
                                &empty_map_keys,
                                &chunks,
                                &block_reader,
                                read_threads,
                            )?;
                            out_batches.append(&mut rg_batches);
                        }
                        continue;
                    }

                    if !sub_rgs.is_empty() {
                        let mut trial = sub_ext.clone();
                        for (&pidx, cids) in &prep.ext_by_pack {
                            trial.entry(pidx).or_default().extend(cids);
                        }
                        let (tc, tf) = self.measure_external_pack_bytes(&trial)?;
                        let trial_c = tc
                            .saturating_add(sub_l0_c)
                            .saturating_add(prep.l0_chunk_bytes);
                        let trial_f = tf
                            .saturating_add(sub_l0_f)
                            .saturating_add(prep.l0_frame_bytes);
                        if trial_c > PHASE2_PAYLOAD_WAVE_TARGET_BYTES
                            || trial_f > PHASE2_PAYLOAD_WAVE_TARGET_BYTES
                        {
                            flush_sub_rgs(
                                &mut sub_rgs,
                                &mut sub_ext,
                                &mut sub_l0_c,
                                &mut sub_l0_f,
                                &mut out_batches,
                            )?;
                        }
                    }
                    for (&pidx, cids) in &prep.ext_by_pack {
                        sub_ext.entry(pidx).or_default().extend(cids);
                    }
                    sub_l0_c = sub_l0_c.saturating_add(prep.l0_chunk_bytes);
                    sub_l0_f = sub_l0_f.saturating_add(prep.l0_frame_bytes);
                    sub_rgs.push(prep);
                }
                flush_sub_rgs(
                    &mut sub_rgs,
                    &mut sub_ext,
                    &mut sub_l0_c,
                    &mut sub_l0_f,
                    &mut out_batches,
                )?;
            }
        }

        if out_batches.len() > 1 {
            if proj_indices.is_empty() {
                let total_rows: usize = out_batches.iter().map(Batch::num_rows).sum();
                return Ok(vec![Batch::try_new(
                    Arc::clone(&proj_schema),
                    Vec::new(),
                    total_rows,
                    Selection::All,
                )?]);
            }
            out_batches = coalesce_take_batches(&proj_schema, out_batches)?;
        }

        Ok(out_batches)
    }

    fn build_dict_sub_bitmaps(
        &self,
        resolved_subs: &[(usize, &SubstringQuery)],
    ) -> Vec<Option<Bitmap>> {
        if resolved_subs.is_empty() {
            return Vec::new();
        }
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
                        if let Some(b) = crate::codec::utf8_bytes_at(u, i)
                            && matcher.matches(b)
                        {
                            let _ = bm.set(i, true);
                        }
                    }
                }
                Some(bm)
            })
            .collect()
    }

    fn refresh_dict_sub_bitmaps(
        &self,
        resolved_subs: &[(usize, &SubstringQuery)],
        dict_sub_bitmaps: &mut Vec<Option<Bitmap>>,
    ) {
        if resolved_subs.is_empty() {
            return;
        }
        let needs_refresh = resolved_subs.iter().enumerate().any(|(i, &(cidx, sq))| {
            !sq.needle.is_empty()
                && (self.meta.col_dir[cidx].col_flags & COL_FLAG_HAS_FILE_DICT) != 0
                && dict_sub_bitmaps.get(i).is_none_or(Option::is_none)
        });
        if needs_refresh {
            *dict_sub_bitmaps = self.build_dict_sub_bitmaps(resolved_subs);
        }
    }

    fn rg_blooms_may_match(&self, rg_idx: usize, lookup_hashes: &[(usize, Vec<u64>)]) -> bool {
        let guard = self.caches.lock().unwrap_or_else(|p| p.into_inner());
        for (cidx, hashes) in lookup_hashes {
            if let Some(bloom) = guard.blooms.get(cidx)
                && !bloom.rg_may_contain_any_hash(rg_idx, hashes)
            {
                return false;
            }
        }
        true
    }

    fn scan_rgs_unfiltered_batched(
        &self,
        surviving_rgs: Vec<(usize, Vec<usize>)>,
        proj_indices: &[usize],
        proj_schema: &Arc<Schema>,
        map_keys_by_col: &Arc<HashMap<usize, Vec<String>>>,
        read_threads: usize,
    ) -> Result<Vec<Batch>, StatamError> {
        struct UnfilteredRgPlan {
            rg_idx: usize,
            surviving_mbs: Vec<usize>,
            plan: Arc<RgIoPlan>,
            ranges: Vec<(u64, u64)>,
            rg_bytes: u64,
            l0_needed: bool,
            l0_end: u64,
            dict_cols: Vec<usize>,
        }

        let has_payload_proj = proj_indices
            .iter()
            .any(|&cidx| self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload);

        let mut planned = Vec::with_capacity(surviving_rgs.len());
        for (rg_idx, surviving_mbs) in surviving_rgs {
            let rg = &self.meta.rg_dir[rg_idx];
            let plan = Arc::new(self.build_rg_io_plan(
                rg_idx,
                &surviving_mbs,
                proj_indices,
                map_keys_by_col,
                None,
                TimeRange::ALL,
                &[],
                &[],
                &[],
            ));
            let mut ranges = Vec::new();
            let mut rg_bytes = 0u64;
            for &mb_idx in &surviving_mbs {
                for &pidx in plan.all_phys_cols() {
                    if self
                        .meta
                        .schema_meta
                        .schema
                        .fields()
                        .get(pidx)
                        .is_some_and(|f| f.data_type == DataType::MapUtf8)
                        && plan.phase2_map_fast_path.contains(&(mb_idx, pidx))
                    {
                        continue;
                    }
                    let blk = &self.meta.rg_blocks(pidx, rg_idx)[mb_idx];
                    if blk.stored_len > 0 {
                        let total = (BLOCK_FRAME_HEADER_LEN as u64)
                            .checked_add(u64::from(blk.stored_len))
                            .ok_or(StatamError::BoundsExceeded {
                                reason: "Scan block length overflow",
                            })?;
                        let end =
                            blk.offset
                                .checked_add(total)
                                .ok_or(StatamError::BoundsExceeded {
                                    reason: "Scan block end overflow",
                                })?;
                        ranges.push((blk.offset, end));
                        rg_bytes = rg_bytes.saturating_add(total);
                    }
                }
            }
            let l0_needed = has_payload_proj && rg.l0_payload_len > 0;
            let l0_end = rg
                .l0_payload_offset
                .checked_add(u64::from(rg.l0_payload_len))
                .ok_or(StatamError::BoundsExceeded {
                    reason: "Scan L0 payload end overflow",
                })?;
            if l0_needed {
                ranges.push((rg.l0_payload_offset, l0_end));
                rg_bytes = rg_bytes.saturating_add(u64::from(rg.l0_payload_len));
            }
            let dict_cols: Vec<usize> = plan
                .all_phys_cols()
                .copied()
                .filter(|&pidx| {
                    (self.meta.col_dir[pidx].col_flags & COL_FLAG_HAS_FILE_DICT) != 0
                        && surviving_mbs.iter().any(|&mb| {
                            self.meta.rg_blocks(pidx, rg_idx)[mb].encoding == Encoding::Dict
                        })
                })
                .collect();
            planned.push(UnfilteredRgPlan {
                rg_idx,
                surviving_mbs,
                plan,
                ranges,
                rg_bytes,
                l0_needed,
                l0_end,
                dict_cols,
            });
        }

        let mut waves: Vec<Vec<UnfilteredRgPlan>> = Vec::new();
        let mut cur_wave: Vec<UnfilteredRgPlan> = Vec::new();
        let mut cur_bytes = 0u64;
        for item in planned {
            if !cur_wave.is_empty() && cur_bytes.saturating_add(item.rg_bytes) > IO_WAVE_MAX_BYTES {
                waves.push(std::mem::take(&mut cur_wave));
                cur_bytes = 0;
            }
            cur_bytes = cur_bytes.saturating_add(item.rg_bytes);
            cur_wave.push(item);
        }
        if !cur_wave.is_empty() {
            waves.push(cur_wave);
        }

        let empty_pre = HashMap::new();
        let mut out_batches = Vec::new();
        for wave in waves {
            let total_ranges: usize = wave.iter().map(|w| w.ranges.len()).sum();
            let mut wave_ranges = Vec::with_capacity(total_ranges);
            let mut wave_dicts = Vec::new();
            for item in &wave {
                wave_ranges.extend_from_slice(&item.ranges);
                wave_dicts.extend_from_slice(&item.dict_cols);
            }

            let eligible_for_pack_prefetch = has_payload_proj
                && wave.iter().any(|item| {
                    proj_indices.iter().any(|&cidx| {
                        self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload
                            && item.surviving_mbs.iter().any(|&mb| {
                                self.meta.rg_blocks(cidx, item.rg_idx)[mb].encoding
                                    != Encoding::ConstNull
                            })
                    })
                });
            let cache = Arc::new(self.fetch_blocks_and_lazy_maybe_prefetch_packs(
                &wave_ranges,
                &[],
                &[],
                &wave_dicts,
                eligible_for_pack_prefetch,
            )?);
            let block_reader = BlockCacheReader::Map {
                primary: Arc::clone(&cache),
                fallback: None,
                missing_reason: "missing unfiltered Scan fetched block",
                overflow_reason: "Scan block key overflow",
            };

            for item in wave {
                let rg = &self.meta.rg_dir[item.rg_idx];
                let l0_parsed = if item.l0_needed {
                    cache
                        .get(&(rg.l0_payload_offset, item.l0_end))
                        .map(|b| ParsedL0Payload::parse(b.clone()).map(Arc::new))
                        .transpose()?
                } else {
                    None
                };
                let blks0 = self.meta.rg_blocks(0, item.rg_idx);
                let active_mbs = item
                    .surviving_mbs
                    .into_iter()
                    .map(|mb_idx| {
                        (
                            mb_idx,
                            Bitmap::new_ones(blks0[mb_idx].row_count as usize),
                            HashMap::new(),
                        )
                    })
                    .collect();
                let rg_batches = self.resolve_and_materialize_phase2_mbs(
                    item.rg_idx,
                    active_mbs,
                    &item.plan,
                    proj_indices,
                    proj_schema,
                    map_keys_by_col,
                    l0_parsed.as_ref(),
                    &empty_pre,
                    &block_reader,
                    read_threads,
                )?;
                for batch in rg_batches {
                    if !batch.is_empty() {
                        out_batches.push(batch);
                    }
                }
            }
        }
        Ok(out_batches)
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_rgs_filtered_batched(
        &self,
        surviving_rgs: Vec<(usize, Vec<usize>)>,
        proj_indices: &[usize],
        proj_schema: &Arc<Schema>,
        map_keys_by_col: &Arc<HashMap<usize, Vec<String>>>,
        time_col_idx: Option<usize>,
        time_range: TimeRange,
        lookups: &[(usize, &IdLookup)],
        preds: &[(usize, &Predicate)],
        subs: &[(usize, &SubstringQuery)],
        dict_sub_bitmaps: &mut Vec<Option<Bitmap>>,
        read_threads: usize,
    ) -> Result<Vec<Batch>, StatamError> {
        struct FilteredRgP1 {
            rg_idx: usize,
            surviving_mbs: Vec<usize>,
            plan: Arc<RgIoPlan>,
            p1_ranges: Vec<(u64, u64)>,
            p1_bytes: u64,
            p1_raw_bytes: u64,
            p1_l0_needed: bool,
            l0_end: u64,
            p1_dict_cols: Vec<usize>,
        }

        let has_payload_sub = subs.iter().any(|&(cidx, _)| {
            self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload
        });
        let has_payload_proj = proj_indices
            .iter()
            .any(|&cidx| self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload);

        let mut planned = Vec::with_capacity(surviving_rgs.len());
        for (rg_idx, surviving_mbs) in surviving_rgs {
            let rg = &self.meta.rg_dir[rg_idx];
            let plan = Arc::new(self.build_rg_io_plan(
                rg_idx,
                &surviving_mbs,
                proj_indices,
                map_keys_by_col,
                time_col_idx,
                time_range,
                lookups,
                preds,
                subs,
            ));
            let mut p1_ranges = Vec::new();
            let mut p1_bytes = 0u64;
            let mut p1_raw_bytes = 0u64;
            for &mb_idx in &surviving_mbs {
                for &pidx in &plan.phase1_phys_cols {
                    let blk = &self.meta.rg_blocks(pidx, rg_idx)[mb_idx];
                    p1_raw_bytes = p1_raw_bytes.saturating_add(u64::from(blk.raw_len));
                    if blk.stored_len > 0 {
                        let total = (BLOCK_FRAME_HEADER_LEN as u64)
                            .checked_add(u64::from(blk.stored_len))
                            .ok_or(StatamError::BoundsExceeded {
                                reason: "Phase 1 block length overflow",
                            })?;
                        let end =
                            blk.offset
                                .checked_add(total)
                                .ok_or(StatamError::BoundsExceeded {
                                    reason: "Phase 1 block end overflow",
                                })?;
                        p1_ranges.push((blk.offset, end));
                        p1_bytes = p1_bytes.saturating_add(total);
                    }
                }
            }
            let p1_l0_needed = rg.l0_payload_len > 0 && has_payload_sub;
            let l0_end = rg
                .l0_payload_offset
                .checked_add(u64::from(rg.l0_payload_len))
                .ok_or(StatamError::BoundsExceeded {
                    reason: "Phase 1 L0 payload end overflow",
                })?;
            if p1_l0_needed {
                p1_ranges.push((rg.l0_payload_offset, l0_end));
                p1_bytes = p1_bytes.saturating_add(u64::from(rg.l0_payload_len));
            }
            let p1_dict_cols: Vec<usize> = plan
                .phase1_phys_cols
                .iter()
                .copied()
                .filter(|&pidx| {
                    (self.meta.col_dir[pidx].col_flags & COL_FLAG_HAS_FILE_DICT) != 0
                        && surviving_mbs.iter().any(|&mb| {
                            self.meta.rg_blocks(pidx, rg_idx)[mb].encoding == Encoding::Dict
                        })
                })
                .collect();
            planned.push(FilteredRgP1 {
                rg_idx,
                surviving_mbs,
                plan,
                p1_ranges,
                p1_bytes,
                p1_raw_bytes,
                p1_l0_needed,
                l0_end,
                p1_dict_cols,
            });
        }

        // Partition RGs into <= 64 MiB stored / <= 128 MiB decoded-raw Wave-1 batches.
        let mut p1_waves: Vec<Vec<FilteredRgP1>> = Vec::new();
        let mut cur_wave: Vec<FilteredRgP1> = Vec::new();
        let mut cur_bytes = 0u64;
        let mut cur_raw_bytes = 0u64;
        for item in planned {
            if !cur_wave.is_empty()
                && (cur_bytes.saturating_add(item.p1_bytes) > IO_WAVE_MAX_BYTES
                    || cur_raw_bytes.saturating_add(item.p1_raw_bytes) > P1_WAVE_MAX_RAW_BYTES)
            {
                p1_waves.push(std::mem::take(&mut cur_wave));
                cur_bytes = 0;
                cur_raw_bytes = 0;
            }
            cur_bytes = cur_bytes.saturating_add(item.p1_bytes);
            cur_raw_bytes = cur_raw_bytes.saturating_add(item.p1_raw_bytes);
            cur_wave.push(item);
        }
        if !cur_wave.is_empty() {
            p1_waves.push(cur_wave);
        }

        let mut out_batches = Vec::new();
        for p1_wave in p1_waves {
            let total_p1_ranges: usize = p1_wave.iter().map(|w| w.p1_ranges.len()).sum();
            let mut wave_p1_ranges = Vec::with_capacity(total_p1_ranges);
            let mut wave_p1_dicts = Vec::new();
            let mut wave_p1_bytes = 0u64;
            for item in &p1_wave {
                wave_p1_ranges.extend_from_slice(&item.p1_ranges);
                wave_p1_dicts.extend_from_slice(&item.p1_dict_cols);
                wave_p1_bytes = wave_p1_bytes.saturating_add(item.p1_bytes);
            }

            // Remote single-wave folding: when `!has_payload_sub` and candidate Phase-2 bytes
            // (blocks + L0 payload + deduped uncached non-inline lazy dicts) fit within 1.5 MiB
            // and combined wave <= 64 MiB, fold Phase 2 into Wave 1 to eliminate the second S3
            // round-trip.
            let mut p2_folded_into_p1 = false;
            if self.reader.is_remote() && !has_payload_sub {
                let mut cand_p2_ranges = Vec::new();
                let mut cand_p2_block_bytes = 0u64;
                let mut cand_p2_dicts = Vec::new();
                for item in &p1_wave {
                    let cand = self.collect_p2_candidate_ranges_and_dicts(
                        item.rg_idx,
                        &item.surviving_mbs,
                        &item.plan,
                        item.p1_l0_needed,
                        item.l0_end,
                        has_payload_proj,
                    )?;
                    cand_p2_ranges.extend_from_slice(&cand.ranges);
                    cand_p2_block_bytes = cand_p2_block_bytes.saturating_add(cand.bytes);
                    cand_p2_dicts.extend_from_slice(&cand.dict_cols);
                }
                let cand_p2_dict_bytes = self.uncached_non_inline_lazy_dict_bytes(&cand_p2_dicts);
                let p2_candidate_bytes = cand_p2_block_bytes.saturating_add(cand_p2_dict_bytes);
                if p2_candidate_bytes > 0
                    && p2_candidate_bytes <= REMOTE_SINGLE_WAVE_P2_MAX_BYTES
                    && wave_p1_bytes.saturating_add(p2_candidate_bytes) <= IO_WAVE_MAX_BYTES
                {
                    wave_p1_ranges.extend_from_slice(&cand_p2_ranges);
                    wave_p1_dicts.extend_from_slice(&cand_p2_dicts);
                    p2_folded_into_p1 = true;
                }
            }

            let eligible_for_pack_prefetch = has_payload_sub
                && preds.is_empty()
                && time_col_idx.is_none()
                && lookups.is_empty()
                && p1_wave.iter().any(|item| {
                    subs.iter().any(|&(cidx, _)| {
                        self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload
                            && item.surviving_mbs.iter().any(|&mb| {
                                self.meta.rg_blocks(cidx, item.rg_idx)[mb].encoding
                                    != Encoding::ConstNull
                            })
                    })
                });

            // Wave 1: Fetch ALL missing Phase-1 dicts + ALL Phase-1 blocks (plus folded Phase 2 on
            // small remote scans) in ONE wave!
            let p1_cache = Arc::new(self.fetch_blocks_and_lazy_maybe_prefetch_packs(
                &wave_p1_ranges,
                &[],
                &[],
                &wave_p1_dicts,
                eligible_for_pack_prefetch,
            )?);
            self.refresh_dict_sub_bitmaps(subs, dict_sub_bitmaps);

            let p1_reader = BlockCacheReader::Map {
                primary: Arc::clone(&p1_cache),
                fallback: None,
                missing_reason: "missing Phase 1 fetched block",
                overflow_reason: "Phase 1 block key overflow",
            };

            struct ActiveRgP2 {
                rg_idx: usize,
                plan: Arc<RgIoPlan>,
                active_mbs: Vec<(usize, Bitmap, HashMap<usize, DecodedPhase1>)>,
                p1_l0_needed: bool,
                p2_l0_needed: bool,
                l0_end: u64,
                p2_ranges: Vec<(u64, u64)>,
                p2_bytes: u64,
                p2_dict_cols: Vec<usize>,
            }

            // One shared 64 MiB chunk-retention cache across all RGs in this `p1_wave`.
            let wave_retained = RetainedPackChunks::default();
            let mut active_rgs = Vec::new();
            for item in p1_wave {
                let rg = &self.meta.rg_dir[item.rg_idx];
                let l0_from_p1 = if item.p1_l0_needed {
                    p1_cache
                        .get(&(rg.l0_payload_offset, item.l0_end))
                        .map(|b| ParsedL0Payload::parse(b.clone()))
                        .transpose()?
                } else {
                    None
                };
                let active_mbs = self.eval_phase1_mbs(
                    item.rg_idx,
                    &item.surviving_mbs,
                    &item.plan,
                    proj_indices,
                    time_col_idx,
                    time_range,
                    lookups,
                    preds,
                    subs,
                    dict_sub_bitmaps,
                    l0_from_p1.as_ref(),
                    &p1_reader,
                    None,
                    false,
                    read_threads,
                    &wave_retained,
                )?;
                if active_mbs.is_empty() {
                    continue;
                }

                let p2_fetch = if p2_folded_into_p1 {
                    P2CandidateFetch {
                        ranges: Vec::new(),
                        bytes: 0,
                        dict_cols: Vec::new(),
                        l0_needed: rg.l0_payload_len > 0 && !item.p1_l0_needed && has_payload_proj,
                    }
                } else {
                    let active_mb_ids: Vec<usize> =
                        active_mbs.iter().map(|(mb, _, _)| *mb).collect();
                    self.collect_p2_candidate_ranges_and_dicts(
                        item.rg_idx,
                        &active_mb_ids,
                        &item.plan,
                        item.p1_l0_needed,
                        item.l0_end,
                        has_payload_proj,
                    )?
                };

                active_rgs.push(ActiveRgP2 {
                    rg_idx: item.rg_idx,
                    plan: item.plan,
                    active_mbs,
                    p1_l0_needed: item.p1_l0_needed,
                    p2_l0_needed: p2_fetch.l0_needed,
                    l0_end: item.l0_end,
                    p2_ranges: p2_fetch.ranges,
                    p2_bytes: p2_fetch.bytes,
                    p2_dict_cols: p2_fetch.dict_cols,
                });
            }

            if active_rgs.is_empty() {
                continue;
            }
            let wave_retained_map = wave_retained.into_map();

            // Partition active RGs into <= 64 MiB Wave-2 batches.
            let mut p2_waves: Vec<Vec<ActiveRgP2>> = Vec::new();
            let mut cur_p2: Vec<ActiveRgP2> = Vec::new();
            let mut cur_p2_bytes = 0u64;
            for item in active_rgs {
                if !cur_p2.is_empty()
                    && cur_p2_bytes.saturating_add(item.p2_bytes) > IO_WAVE_MAX_BYTES
                {
                    p2_waves.push(std::mem::take(&mut cur_p2));
                    cur_p2_bytes = 0;
                }
                cur_p2_bytes = cur_p2_bytes.saturating_add(item.p2_bytes);
                cur_p2.push(item);
            }
            if !cur_p2.is_empty() {
                p2_waves.push(cur_p2);
            }

            for p2_wave in p2_waves {
                // Wave 2: Skipped completely when Phase 2 was folded into Wave 1!
                let p2_cache = Arc::new(if p2_folded_into_p1 {
                    HashMap::new()
                } else {
                    let total_p2_ranges: usize = p2_wave.iter().map(|w| w.p2_ranges.len()).sum();
                    let mut wave_p2_ranges = Vec::with_capacity(total_p2_ranges);
                    let mut wave_p2_dicts = Vec::new();
                    for item in &p2_wave {
                        wave_p2_ranges.extend_from_slice(&item.p2_ranges);
                        wave_p2_dicts.extend_from_slice(&item.p2_dict_cols);
                    }
                    self.fetch_blocks_and_lazy(&wave_p2_ranges, &[], &[], &wave_p2_dicts)?
                });

                let p2_reader = BlockCacheReader::Map {
                    primary: Arc::clone(&p2_cache),
                    fallback: Some(Arc::clone(&p1_cache)),
                    missing_reason: "missing Phase 2 fetched block",
                    overflow_reason: "Phase 2 block key overflow",
                };

                for item in p2_wave {
                    let rg = &self.meta.rg_dir[item.rg_idx];
                    let l0_parsed = if item.p1_l0_needed {
                        p1_cache
                            .get(&(rg.l0_payload_offset, item.l0_end))
                            .map(|b| ParsedL0Payload::parse(b.clone()).map(Arc::new))
                            .transpose()?
                    } else if item.p2_l0_needed {
                        let l0_key = (rg.l0_payload_offset, item.l0_end);
                        p2_cache
                            .get(&l0_key)
                            .or_else(|| p1_cache.get(&l0_key))
                            .map(|b| ParsedL0Payload::parse(b.clone()).map(Arc::new))
                            .transpose()?
                    } else {
                        None
                    };
                    let rg_batches = self.resolve_and_materialize_phase2_mbs(
                        item.rg_idx,
                        item.active_mbs,
                        &item.plan,
                        proj_indices,
                        proj_schema,
                        map_keys_by_col,
                        l0_parsed.as_ref(),
                        &wave_retained_map,
                        &p2_reader,
                        read_threads,
                    )?;
                    for batch in rg_batches {
                        if !batch.is_empty() {
                            out_batches.push(batch);
                        }
                    }
                }
            }
        }

        Ok(out_batches)
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_rg_point(
        &self,
        rg_idx: usize,
        rg: &RowGroupEntry,
        surviving_mbs: &[usize],
        proj_indices: &[usize],
        proj_schema: &Arc<Schema>,
        map_keys_by_col: &Arc<HashMap<usize, Vec<String>>>,
        time_col_idx: Option<usize>,
        time_range: TimeRange,
        lookups: &[(usize, &IdLookup)],
        preds: &[(usize, &Predicate)],
        subs: &[(usize, &SubstringQuery)],
        dict_sub_bitmaps: &mut Vec<Option<Bitmap>>,
        limit_remaining: Option<usize>,
        trim_tail: bool,
        read_threads: usize,
    ) -> Result<Vec<Batch>, StatamError> {
        let plan = Arc::new(self.build_rg_io_plan(
            rg_idx,
            surviving_mbs,
            proj_indices,
            map_keys_by_col,
            time_col_idx,
            time_range,
            lookups,
            preds,
            subs,
        ));

        // Single tightest contiguous RG byte range fetch covering only needed blocks!
        let mut need_l0_payload = false;
        let mut min_off = u64::MAX;
        let mut max_end = 0u64;
        for &pidx in plan.all_phys_cols() {
            if self.meta.col_dir[pidx].locality_group == LocalityGroup::ExternalPayload
                && rg.l0_payload_len > 0
            {
                need_l0_payload = true;
            }
            let blks = self.meta.rg_blocks(pidx, rg_idx);
            let is_map = self
                .meta
                .schema_meta
                .schema
                .fields()
                .get(pidx)
                .is_some_and(|f| f.data_type == DataType::MapUtf8);
            let in_phase1 = plan.phase1_phys_cols.contains(&pidx);
            for &mb_idx in surviving_mbs {
                if is_map
                    && plan.phase2_map_fast_path.contains(&(mb_idx, pidx))
                    && (!in_phase1 || plan.phase1_map_fast_path.contains(&(mb_idx, pidx)))
                {
                    continue;
                }
                let blk = &blks[mb_idx];
                if blk.stored_len > 0 {
                    let total = (BLOCK_FRAME_HEADER_LEN as u64)
                        .checked_add(u64::from(blk.stored_len))
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "point block length overflow",
                        })?;
                    let end = blk
                        .offset
                        .checked_add(total)
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "point block end overflow",
                        })?;
                    min_off = min_off.min(blk.offset);
                    max_end = max_end.max(end);
                }
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

        if need_l0_payload {
            let l0_end = rg
                .l0_payload_offset
                .checked_add(u64::from(rg.l0_payload_len))
                .ok_or(StatamError::BoundsExceeded {
                    reason: "point RG l0_payload end overflow",
                })?;
            min_off = min_off.min(rg.l0_payload_offset);
            max_end = max_end.max(l0_end);
        }

        let (range_start, range_end) = if max_end > min_off {
            (min_off, max_end)
        } else {
            (0u64, 0u64)
        };
        let expected_range_len =
            range_end
                .checked_sub(range_start)
                .ok_or(StatamError::Corrupt {
                    reason: "invalid Point RG byte range",
                })?;

        let has_non_null_payload_proj = proj_indices.iter().any(|&cidx| {
            self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload
                && surviving_mbs.iter().any(|&mb| {
                    self.meta.rg_blocks(cidx, rg_idx)[mb].encoding != Encoding::ConstNull
                })
        });
        let eligible_for_pack_prefetch = !lookups.is_empty() && has_non_null_payload_proj;

        let rg_slice =
            if range_end > range_start {
                let fetched = self.fetch_blocks_and_lazy_maybe_prefetch_packs(
                    &[(range_start, range_end)],
                    &[],
                    &[],
                    &dict_cols,
                    eligible_for_pack_prefetch,
                )?;
                let bytes = fetched.get(&(range_start, range_end)).cloned().ok_or(
                    StatamError::Corrupt {
                        reason: "missing Point RG range",
                    },
                )?;
                if bytes.len() as u64 != expected_range_len {
                    return Err(StatamError::Corrupt {
                        reason: "short read on Point RG range",
                    });
                }
                bytes
            } else {
                if !dict_cols.is_empty() {
                    self.ensure_dicts_loaded(&dict_cols)?;
                }
                Bytes::new()
            };
        self.refresh_dict_sub_bitmaps(subs, dict_sub_bitmaps);

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
            if rel_start > rel_end || rel_end > rg_slice.len() {
                return Err(StatamError::Corrupt {
                    reason: "L0 payload slice out of Point RG range",
                });
            }
            Some(Arc::new(ParsedL0Payload::parse(
                rg_slice.slice(rel_start..rel_end),
            )?))
        } else {
            None
        };

        let block_reader = BlockCacheReader::Slice {
            base_offset: range_start,
            bytes: rg_slice,
        };

        let retained = RetainedPackChunks::default();
        let active_mbs = self.eval_phase1_mbs(
            rg_idx,
            surviving_mbs,
            &plan,
            proj_indices,
            time_col_idx,
            time_range,
            lookups,
            preds,
            subs,
            dict_sub_bitmaps,
            l0_parsed.as_deref(),
            &block_reader,
            limit_remaining,
            trim_tail,
            read_threads,
            &retained,
        )?;
        if active_mbs.is_empty() {
            return Ok(Vec::new());
        }
        let retained_chunks = retained.into_map();

        self.resolve_and_materialize_phase2_mbs(
            rg_idx,
            active_mbs,
            &plan,
            proj_indices,
            proj_schema,
            map_keys_by_col,
            l0_parsed.as_ref(),
            &retained_chunks,
            &block_reader,
            read_threads,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_rg_late_mat(
        &self,
        rg_idx: usize,
        rg: &RowGroupEntry,
        surviving_mbs: &[usize],
        proj_indices: &[usize],
        proj_schema: &Arc<Schema>,
        map_keys_by_col: &Arc<HashMap<usize, Vec<String>>>,
        time_col_idx: Option<usize>,
        time_range: TimeRange,
        lookups: &[(usize, &IdLookup)],
        preds: &[(usize, &Predicate)],
        subs: &[(usize, &SubstringQuery)],
        dict_sub_bitmaps: &mut Vec<Option<Bitmap>>,
        limit_remaining: Option<usize>,
        trim_tail: bool,
        read_threads: usize,
    ) -> Result<Vec<Batch>, StatamError> {
        let plan = Arc::new(self.build_rg_io_plan(
            rg_idx,
            surviving_mbs,
            proj_indices,
            map_keys_by_col,
            time_col_idx,
            time_range,
            lookups,
            preds,
            subs,
        ));

        // Phase 1: Coalesce and fetch ONLY filter/substring columns (+ any missing Phase-1 lazy dicts).
        let mut p1_ranges = Vec::new();
        let mut p1_bytes = 0u64;
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
                    p1_bytes = p1_bytes.saturating_add(total);
                }
            }
        }

        let has_payload_sub = subs.iter().any(|&(cidx, _)| {
            self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload
        });
        let has_payload_proj = proj_indices
            .iter()
            .any(|&cidx| self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload);

        let p1_l0_needed = rg.l0_payload_len > 0 && has_payload_sub;
        let p1_l0_end = rg
            .l0_payload_offset
            .checked_add(u64::from(rg.l0_payload_len))
            .ok_or(StatamError::BoundsExceeded {
                reason: "Phase 1 L0 payload end overflow",
            })?;
        if p1_l0_needed {
            p1_ranges.push((rg.l0_payload_offset, p1_l0_end));
            p1_bytes = p1_bytes.saturating_add(u64::from(rg.l0_payload_len));
        }

        let mut p1_dict_cols: Vec<usize> = plan
            .phase1_phys_cols
            .iter()
            .copied()
            .filter(|&pidx| {
                (self.meta.col_dir[pidx].col_flags & COL_FLAG_HAS_FILE_DICT) != 0
                    && surviving_mbs
                        .iter()
                        .any(|&mb| self.meta.rg_blocks(pidx, rg_idx)[mb].encoding == Encoding::Dict)
            })
            .collect();

        // Remote single-wave folding: fold small candidate Phase-2 ranges + dicts into Wave 1
        // when `!has_payload_sub`.
        let mut p2_folded_into_p1 = false;
        if self.reader.is_remote() && !has_payload_sub {
            let cand = self.collect_p2_candidate_ranges_and_dicts(
                rg_idx,
                surviving_mbs,
                &plan,
                p1_l0_needed,
                p1_l0_end,
                has_payload_proj,
            )?;
            let cand_p2_dict_bytes = self.uncached_non_inline_lazy_dict_bytes(&cand.dict_cols);
            let p2_candidate_bytes = cand.bytes.saturating_add(cand_p2_dict_bytes);
            if p2_candidate_bytes > 0
                && p2_candidate_bytes <= REMOTE_SINGLE_WAVE_P2_MAX_BYTES
                && p1_bytes.saturating_add(p2_candidate_bytes) <= IO_WAVE_MAX_BYTES
            {
                p1_ranges.extend_from_slice(&cand.ranges);
                p1_dict_cols.extend_from_slice(&cand.dict_cols);
                p2_folded_into_p1 = true;
            }
        }

        let eligible_for_pack_prefetch = has_payload_sub
            && preds.is_empty()
            && time_col_idx.is_none()
            && lookups.is_empty()
            && subs.iter().any(|&(cidx, _)| {
                self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload
                    && surviving_mbs.iter().any(|&mb| {
                        self.meta.rg_blocks(cidx, rg_idx)[mb].encoding != Encoding::ConstNull
                    })
            });

        let p1_cache = Arc::new(self.fetch_blocks_and_lazy_maybe_prefetch_packs(
            &p1_ranges,
            &[],
            &[],
            &p1_dict_cols,
            eligible_for_pack_prefetch,
        )?);
        self.refresh_dict_sub_bitmaps(subs, dict_sub_bitmaps);

        let l0_from_p1 = if p1_l0_needed {
            p1_cache
                .get(&(rg.l0_payload_offset, p1_l0_end))
                .map(|b| ParsedL0Payload::parse(b.clone()).map(Arc::new))
                .transpose()?
        } else {
            None
        };

        let p1_reader = BlockCacheReader::Map {
            primary: Arc::clone(&p1_cache),
            fallback: None,
            missing_reason: "missing Phase 1 fetched block",
            overflow_reason: "Phase 1 block key overflow",
        };

        // Evaluate Phase 1 on each surviving mini-block (in parallel across MBs when read_threads > 1).
        let retained = RetainedPackChunks::default();
        let active_mbs = self.eval_phase1_mbs(
            rg_idx,
            surviving_mbs,
            &plan,
            proj_indices,
            time_col_idx,
            time_range,
            lookups,
            preds,
            subs,
            dict_sub_bitmaps,
            l0_from_p1.as_deref(),
            &p1_reader,
            limit_remaining,
            trim_tail,
            read_threads,
            &retained,
        )?;

        if active_mbs.is_empty() {
            return Ok(Vec::new());
        }
        let retained_chunks = retained.into_map();

        let p2_l0_needed = rg.l0_payload_len > 0 && !p1_l0_needed && has_payload_proj;
        let p2_cache = Arc::new(if p2_folded_into_p1 {
            HashMap::new()
        } else {
            let active_mb_ids: Vec<usize> = active_mbs.iter().map(|(mb, _, _)| *mb).collect();
            let p2_fetch = self.collect_p2_candidate_ranges_and_dicts(
                rg_idx,
                &active_mb_ids,
                &plan,
                p1_l0_needed,
                p1_l0_end,
                has_payload_proj,
            )?;
            self.fetch_blocks_and_lazy(&p2_fetch.ranges, &[], &[], &p2_fetch.dict_cols)?
        });
        let l0_from_p2 = if p2_l0_needed {
            let l0_key = (rg.l0_payload_offset, p1_l0_end);
            p2_cache
                .get(&l0_key)
                .or_else(|| p1_cache.get(&l0_key))
                .map(|b| ParsedL0Payload::parse(b.clone()).map(Arc::new))
                .transpose()?
        } else {
            None
        };
        let l0_ref = l0_from_p1.as_ref().or(l0_from_p2.as_ref());

        let p2_reader = BlockCacheReader::Map {
            primary: Arc::clone(&p2_cache),
            fallback: Some(Arc::clone(&p1_cache)),
            missing_reason: "missing Phase 2 fetched block",
            overflow_reason: "Phase 2 block key overflow",
        };

        // Coalesce all .stpk / L0 chunk reads across active mini-blocks and projected
        // ExternalPayload columns in wave-bounded batches (<= 128 MiB per wave).
        self.resolve_and_materialize_phase2_mbs(
            rg_idx,
            active_mbs,
            &plan,
            proj_indices,
            proj_schema,
            map_keys_by_col,
            l0_ref,
            &retained_chunks,
            &p2_reader,
            read_threads,
        )
    }

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
        Ok(read_framed_block_checked_cow(&frame, blk, MAX_BLOCK_RAW_BYTES)?.into_owned())
    }

    pub(crate) fn open_l0_payload_for_rg(
        &self,
        rg_idx: usize,
    ) -> Result<Option<StreamingL0Payload>, StatamError> {
        let rg = &self.meta.rg_dir[rg_idx];
        if rg.l0_payload_len == 0 {
            return Ok(None);
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
        Ok(Some(StreamingL0Payload::from_bytes(bytes)?))
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

        let read_from_rg = |blk: &BlockEntry| -> Result<Cow<'_, [u8]>, StatamError> {
            if blk.stored_len == 0 {
                return Ok(Cow::Borrowed(&[]));
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
            read_framed_block_checked_cow(frame, blk, MAX_BLOCK_RAW_BYTES)
        };

        let mut cols = Vec::with_capacity(fields.len());
        for (cidx, field) in fields.iter().enumerate() {
            let cd = &self.meta.col_dir[cidx];
            if cd.locality_group == LocalityGroup::ExternalPayload {
                let placeholder = match field.data_type {
                    DataType::Binary => ScalarValue::Binary(Vec::new()),
                    _ => ScalarValue::Utf8(String::new()),
                };
                cols.push(Column::try_new_const(placeholder, rg_rows)?);
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

/// Maximum rows coalesced into a single output [`Batch`] by [`StatamReader::take_rows`].
const TAKE_COALESCE_MAX_ROWS: usize = 65_536;
/// Maximum estimated decoded bytes coalesced into a single output [`Batch`] by
/// [`StatamReader::take_rows`] (`64 MiB`, keeping large-blob takes well below
/// `MAX_BATCH_DECODED_BYTES` and `u32::MAX` offsets).
const TAKE_COALESCE_MAX_BYTES: usize = 64 * 1024 * 1024;

fn estimate_batch_bytes(batch: &Batch) -> usize {
    batch
        .columns()
        .iter()
        .map(estimate_column_bytes)
        .fold(0usize, usize::saturating_add)
}

fn flush_take_run(
    proj_schema: &Arc<Schema>,
    run: &mut Vec<Batch>,
    run_rows: usize,
    out: &mut Vec<Batch>,
) -> Result<(), StatamError> {
    if run.is_empty() {
        return Ok(());
    }
    if run.len() == 1
        && let Some(only) = run.pop()
    {
        out.push(only);
        return Ok(());
    }
    let fields = proj_schema.fields();
    let mut merged_cols = Vec::with_capacity(fields.len());
    let mut col_refs: Vec<&Column> = Vec::with_capacity(run.len());
    for (c, field) in fields.iter().enumerate() {
        col_refs.clear();
        for b in run.iter() {
            let col = b.column(c).ok_or(StatamError::Corrupt {
                reason: "missing column during take_rows coalescing",
            })?;
            col_refs.push(col);
        }
        merged_cols.push(concat_column_refs(&field.data_type, &col_refs, run_rows)?);
    }
    run.clear();
    out.push(Batch::try_new(
        Arc::clone(proj_schema),
        merged_cols,
        run_rows,
        Selection::All,
    )?);
    Ok(())
}

fn coalesce_take_batches(
    proj_schema: &Arc<Schema>,
    batches: Vec<Batch>,
) -> Result<Vec<Batch>, StatamError> {
    let mut out = Vec::new();
    let mut cur_run: Vec<Batch> = Vec::new();
    let mut cur_rows = 0usize;
    let mut cur_bytes = 0usize;

    for batch in batches {
        let b_rows = batch.num_rows();
        let b_bytes = estimate_batch_bytes(&batch);
        let exceeds_rows = cur_rows
            .checked_add(b_rows)
            .is_none_or(|r| r > TAKE_COALESCE_MAX_ROWS);
        let exceeds_bytes = cur_bytes
            .checked_add(b_bytes)
            .is_none_or(|b| b > TAKE_COALESCE_MAX_BYTES);
        if !cur_run.is_empty() && (exceeds_rows || exceeds_bytes) {
            flush_take_run(proj_schema, &mut cur_run, cur_rows, &mut out)?;
            cur_rows = 0;
            cur_bytes = 0;
        }
        cur_rows = cur_rows.saturating_add(b_rows);
        cur_bytes = cur_bytes.saturating_add(b_bytes);
        cur_run.push(batch);
    }
    flush_take_run(proj_schema, &mut cur_run, cur_rows, &mut out)?;
    Ok(out)
}
