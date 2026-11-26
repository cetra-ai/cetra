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
use std::collections::{BTreeSet, HashMap};
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
    MAX_BLOCK_RAW_BYTES, RowGroupEntry, read_framed_block_checked_cow,
};
use crate::io::RangeReader;
use crate::payload::pack::ChunkPackReader;
use crate::simd::bloom::{BloomIndex, statam_bloom_hash};
use crate::simd::trigram::TrigramBloom;

use eval::{Utf8Matcher, is_always_false_pred, is_supported_id_lookup_type};
#[cfg(test)]
use eval::{eval_utf8_block_substring_raw, eval_utf8_substring};
use materialize::mask_from_indices;
pub use meta::StatamFileMeta;
use payload::ParsedL0Payload;
pub(crate) use payload::StreamingL0Payload;
use plan::{DecodedPhase1, RgIoPlan, TouchedMbRows, fetch_coalesced_blocks, find_time_column};

/// Maximum gap (64 KiB) between blocks coalesced into a single `Scan` range read.
const SCAN_COALESCE_MAX_GAP: u64 = 64 * 1024;
/// Maximum total span (4 MiB) of a single coalesced `Scan` range read.
const SCAN_COALESCE_MAX_SPAN: u64 = 4 * 1024 * 1024;
/// Minimum total uncompressed block bytes required before spawning reader worker threads
/// (48 KiB), avoiding OS thread-spawn overhead on sub-millisecond narrow queries.
/// Tasks touching `LocalityGroup::ExternalPayload` are exempt from this threshold.
const MIN_PARALLEL_WORK_BYTES: usize = 48 * 1024;

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
            if !is_supported_id_lookup_type(&schema.fields()[cidx].data_type) {
                return Err(StatamError::SchemaMismatch {
                    reason: "id_lookup on unsupported column data type",
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

        if early_empty {
            return Ok(Vec::new());
        }

        // Step 1: Precompute lookup hashes once and check file-level SBBF blooms.
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
        if !lookup_hashes.is_empty() && !self.check_blooms(&lookup_hashes)? {
            return Ok(Vec::new());
        }

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

        let has_filters = time_col_idx.is_some()
            || !resolved_lookups.is_empty()
            || !resolved_preds.is_empty()
            || !resolved_subs.is_empty();

        let mut out_batches = Vec::new();
        let mut emitted_rows = 0usize;

        for rg_idx in rg_order {
            if let Some(lim) = effective_limit
                && emitted_rows >= lim
            {
                break;
            }

            if !lookup_hashes.is_empty() {
                let guard = self.caches.lock().unwrap_or_else(|p| p.into_inner());
                let mut rg_may_match = true;
                for (cidx, hashes) in &lookup_hashes {
                    if let Some(bloom) = guard.blooms.get(cidx)
                        && !bloom.rg_may_contain_any_hash(rg_idx, hashes)
                    {
                        rg_may_match = false;
                        break;
                    }
                }
                if !rg_may_match {
                    continue;
                }
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
                    &dict_sub_bitmaps,
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
                    &dict_sub_bitmaps,
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
    /// returned in ascending file order as one [`Batch`] per touched mini-block.
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
            surviving_mbs: Vec<usize>,
            plan: RgIoPlan,
        }

        let empty_map_keys: HashMap<usize, Vec<String>> = HashMap::new();
        let mut dict_cols_set: BTreeSet<usize> = BTreeSet::new();
        let mut touched_with_plans: Vec<TouchedRgPlan> = Vec::with_capacity(touched_rgs.len());
        for (rg_idx, mb_rows) in touched_rgs {
            let surviving_mbs: Vec<usize> = mb_rows.iter().map(|(mb, _)| *mb).collect();
            let plan = self.build_rg_io_plan(
                rg_idx,
                &surviving_mbs,
                &proj_indices,
                &empty_map_keys,
                None,
                TimeRange::ALL,
                &[],
                &[],
                &[],
            );
            for &pidx in plan.all_phys_cols() {
                if (self.meta.col_dir[pidx].col_flags & COL_FLAG_HAS_FILE_DICT) != 0 {
                    let blks = self.meta.rg_blocks(pidx, rg_idx);
                    if surviving_mbs
                        .iter()
                        .any(|&mb| blks[mb].encoding == Encoding::Dict)
                    {
                        dict_cols_set.insert(pidx);
                    }
                }
            }
            touched_with_plans.push(TouchedRgPlan {
                rg_idx,
                mb_rows,
                surviving_mbs,
                plan,
            });
        }
        if !dict_cols_set.is_empty() {
            let dict_cols: Vec<usize> = dict_cols_set.into_iter().collect();
            self.ensure_dicts_loaded(&dict_cols)?;
        }

        let has_payload_proj = proj_indices
            .iter()
            .any(|&cidx| self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload);

        let mut out_batches = Vec::new();
        for TouchedRgPlan {
            rg_idx,
            mb_rows,
            surviving_mbs,
            plan,
        } in touched_with_plans
        {
            let rg = &self.meta.rg_dir[rg_idx];

            let mut ranges = Vec::new();
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
            }

            let block_cache = fetch_coalesced_blocks(self.reader.as_ref(), &ranges)?;
            let l0_parsed = if need_l0_payload {
                block_cache
                    .get(&(rg.l0_payload_offset, l0_end))
                    .map(|b| ParsedL0Payload::parse(b))
                    .transpose()?
            } else {
                None
            };

            let read_block = |blk: &BlockEntry| -> Result<Cow<'_, [u8]>, StatamError> {
                if blk.stored_len == 0 {
                    return Ok(Cow::Borrowed(&[]));
                }
                let end = blk
                    .offset
                    .checked_add(BLOCK_FRAME_HEADER_LEN as u64 + u64::from(blk.stored_len))
                    .ok_or(StatamError::BoundsExceeded {
                        reason: "take_rows block key overflow",
                    })?;
                let key = (blk.offset, end);
                let frame = block_cache.get(&key).ok_or(StatamError::Corrupt {
                    reason: "missing take_rows fetched block",
                })?;
                read_framed_block_checked_cow(frame, blk, MAX_BLOCK_RAW_BYTES)
            };

            let (rg_resolved_chunks, take_mbs) = if has_payload_proj {
                let mut active_mbs: Vec<(usize, Bitmap, HashMap<usize, DecodedPhase1>)> = mb_rows
                    .iter()
                    .map(|(mb_idx, local_indices)| {
                        let mb_row_count =
                            self.meta.rg_blocks(0, rg_idx)[*mb_idx].row_count as usize;
                        let mask = mask_from_indices(mb_row_count, local_indices);
                        (*mb_idx, mask, HashMap::new())
                    })
                    .collect();
                let chunks = self.resolve_rg_payload_chunks(
                    rg_idx,
                    &mut active_mbs,
                    &proj_indices,
                    l0_parsed.as_ref(),
                    &read_block,
                    read_threads,
                )?;
                let mbs = mb_rows
                    .into_iter()
                    .zip(active_mbs)
                    .map(|((mb_idx, local_indices), (_, mask, p1))| {
                        (mb_idx, local_indices, Some(mask), p1)
                    })
                    .collect();
                (chunks, mbs)
            } else {
                let mbs = mb_rows
                    .into_iter()
                    .map(|(mb_idx, local_indices)| (mb_idx, local_indices, None, HashMap::new()))
                    .collect();
                (HashMap::new(), mbs)
            };

            let mut rg_batches = self.materialize_take_mbs(
                rg_idx,
                take_mbs,
                &plan,
                &proj_indices,
                &proj_schema,
                &empty_map_keys,
                &rg_resolved_chunks,
                &read_block,
                read_threads,
            )?;
            out_batches.append(&mut rg_batches);
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
        map_keys_by_col: &HashMap<usize, Vec<String>>,
        time_col_idx: Option<usize>,
        time_range: TimeRange,
        lookups: &[(usize, &IdLookup)],
        preds: &[(usize, &Predicate)],
        subs: &[(usize, &SubstringQuery)],
        dict_sub_bitmaps: &[Option<Bitmap>],
        limit_remaining: Option<usize>,
        trim_tail: bool,
        read_threads: usize,
    ) -> Result<Vec<Batch>, StatamError> {
        let plan = self.build_rg_io_plan(
            rg_idx,
            surviving_mbs,
            proj_indices,
            map_keys_by_col,
            time_col_idx,
            time_range,
            lookups,
            preds,
            subs,
        );

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
        if !dict_cols.is_empty() {
            self.ensure_dicts_loaded(&dict_cols)?;
        }

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

        let block_lookup = |blk: &BlockEntry| -> Result<Cow<'_, [u8]>, StatamError> {
            if blk.stored_len == 0 {
                return Ok(Cow::Borrowed(&[]));
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
            read_framed_block_checked_cow(frame, blk, MAX_BLOCK_RAW_BYTES)
        };

        let mut active_mbs = self.eval_phase1_mbs(
            rg_idx,
            surviving_mbs,
            &plan,
            time_col_idx,
            time_range,
            lookups,
            preds,
            subs,
            dict_sub_bitmaps,
            l0_parsed.as_ref(),
            &block_lookup,
            limit_remaining,
            trim_tail,
            read_threads,
        )?;
        if active_mbs.is_empty() {
            return Ok(Vec::new());
        }

        let rg_resolved_chunks = self.resolve_rg_payload_chunks(
            rg_idx,
            &mut active_mbs,
            proj_indices,
            l0_parsed.as_ref(),
            &block_lookup,
            read_threads,
        )?;

        self.materialize_phase2_mbs(
            rg_idx,
            active_mbs,
            &plan,
            proj_indices,
            proj_schema,
            map_keys_by_col,
            &rg_resolved_chunks,
            &block_lookup,
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
        map_keys_by_col: &HashMap<usize, Vec<String>>,
        time_col_idx: Option<usize>,
        time_range: TimeRange,
        lookups: &[(usize, &IdLookup)],
        preds: &[(usize, &Predicate)],
        subs: &[(usize, &SubstringQuery)],
        dict_sub_bitmaps: &[Option<Bitmap>],
        limit_remaining: Option<usize>,
        trim_tail: bool,
        read_threads: usize,
    ) -> Result<Vec<Batch>, StatamError> {
        let plan = self.build_rg_io_plan(
            rg_idx,
            surviving_mbs,
            proj_indices,
            map_keys_by_col,
            time_col_idx,
            time_range,
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

        let p1_lookup = |blk: &BlockEntry| -> Result<Cow<'_, [u8]>, StatamError> {
            if blk.stored_len == 0 {
                return Ok(Cow::Borrowed(&[]));
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
            read_framed_block_checked_cow(frame, blk, MAX_BLOCK_RAW_BYTES)
        };

        // Evaluate Phase 1 on each surviving mini-block (in parallel across MBs when read_threads > 1).
        let mut active_mbs = self.eval_phase1_mbs(
            rg_idx,
            surviving_mbs,
            &plan,
            time_col_idx,
            time_range,
            lookups,
            preds,
            subs,
            dict_sub_bitmaps,
            l0_from_p1.as_ref(),
            &p1_lookup,
            limit_remaining,
            trim_tail,
            read_threads,
        )?;

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
            |blk: &BlockEntry| -> Result<Cow<'_, [u8]>, StatamError> {
                if blk.stored_len == 0 {
                    return Ok(Cow::Borrowed(&[]));
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
                read_framed_block_checked_cow(frame, blk, MAX_BLOCK_RAW_BYTES)
            };

        // Coalesce all .stpk / L0 chunk reads across ALL active mini-blocks and ALL projected
        // ExternalPayload columns in this RG into ONE scan call per pack!
        let rg_resolved_chunks = self.resolve_rg_payload_chunks(
            rg_idx,
            &mut active_mbs,
            proj_indices,
            l0_ref,
            &p2_lookup,
            read_threads,
        )?;

        self.materialize_phase2_mbs(
            rg_idx,
            active_mbs,
            &plan,
            proj_indices,
            proj_schema,
            map_keys_by_col,
            &rg_resolved_chunks,
            &p2_lookup,
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
