//! Row Group / Mini-Block pruning, I/O planning, and coalesced range fetching.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use bytes::Bytes;
use cetra_column::{Bitmap, Column, DataType, Schema};

use crate::StatamError;
use crate::codec::dict::FileDict;
use crate::codec::nested::ShreddedSubCol;
use crate::codec::{ZonePred, ZoneValue, zone_map_always_matches, zone_map_may_match};
use crate::format::{
    BLOCK_FLAG_HAS_NAN, BLOCK_FLAG_MIN_MAX_VALID, BLOCK_FRAME_HEADER_LEN, BlockEntry,
    COL_FLAG_HAS_TRIGRAM_INDEX, Codec, Encoding, LocalityGroup, MAX_BLOCK_RAW_BYTES,
    read_framed_block_checked_cow,
};
use crate::io::RangeReader;
use crate::payload::pack::{ChunkPackReader, PackToc};
use crate::payload::search::{
    extract_ilike_trigram_hashes, pack_short_circuits_all, pack_toc_short_circuits_all_if_cached,
};
use crate::payload::{FlatPayloadRefs, PayloadRefRow};
use crate::simd::trigram::extract_trigram_hashes;

use super::eval::id_lookup_zone_pred;
use super::{
    FilterExpr, IdLookup, MapReadMode, Predicate, REMOTE_SCAN_COALESCE_MAX_GAP,
    SCAN_COALESCE_MAX_GAP, SCAN_COALESCE_MAX_SPAN, StatamReader, SubstringQuery, TimeRange,
};

#[derive(Clone)]
pub(super) enum ResolvedFilterExpr {
    Pred { cidx: usize, pred: Predicate },
    And(Vec<ResolvedFilterExpr>),
    Or(Vec<ResolvedFilterExpr>),
    Not(Box<ResolvedFilterExpr>),
}

pub(super) struct RgIoPlan {
    pub(super) phase1_phys_cols: Vec<usize>,
    pub(super) phase2_only_phys_cols: Vec<usize>,
    pub(super) phase1_map_fast_path: HashSet<(usize, usize)>,
    pub(super) phase2_map_fast_path: HashSet<(usize, usize)>,
    pub(super) phase2_skip_subcols: HashSet<(usize, usize)>,
}

impl RgIoPlan {
    pub(super) fn all_phys_cols(&self) -> impl Iterator<Item = &usize> {
        self.phase1_phys_cols
            .iter()
            .chain(self.phase2_only_phys_cols.iter())
    }
}

pub(super) type ActiveMiniBlock = (usize, Bitmap, HashMap<usize, DecodedPhase1>);
pub(super) type TouchedMbRows = (usize, Vec<usize>);
pub(super) type TakeMiniBlock = (
    usize,
    Vec<usize>,
    Option<Bitmap>,
    HashMap<usize, DecodedPhase1>,
);
#[derive(Clone)]
pub(super) enum BlockCacheReader {
    Map {
        primary: Arc<HashMap<(u64, u64), Bytes>>,
        fallback: Option<Arc<HashMap<(u64, u64), Bytes>>>,
        missing_reason: &'static str,
        overflow_reason: &'static str,
    },
    Slice {
        base_offset: u64,
        bytes: Bytes,
    },
}

impl BlockCacheReader {
    pub(super) fn read_block(&self, blk: &BlockEntry) -> Result<Cow<'_, [u8]>, StatamError> {
        if blk.stored_len == 0 {
            return Ok(Cow::Borrowed(&[]));
        }
        match self {
            Self::Map {
                primary,
                fallback,
                missing_reason,
                overflow_reason,
            } => {
                let end = blk
                    .offset
                    .checked_add(BLOCK_FRAME_HEADER_LEN as u64 + u64::from(blk.stored_len))
                    .ok_or(StatamError::BoundsExceeded {
                        reason: overflow_reason,
                    })?;
                let key = (blk.offset, end);
                let frame = primary
                    .get(&key)
                    .or_else(|| fallback.as_ref().and_then(|fb| fb.get(&key)))
                    .ok_or(StatamError::Corrupt {
                        reason: missing_reason,
                    })?;
                read_framed_block_checked_cow(frame, blk, MAX_BLOCK_RAW_BYTES)
            }
            Self::Slice { base_offset, bytes } => {
                let rel = blk
                    .offset
                    .checked_sub(*base_offset)
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
                let frame = bytes.get(rel..end).ok_or(StatamError::Corrupt {
                    reason: "block slice out of Point RG range",
                })?;
                read_framed_block_checked_cow(frame, blk, MAX_BLOCK_RAW_BYTES)
            }
        }
    }

    pub(super) fn read_block_bytes(&self, blk: &BlockEntry) -> Result<Bytes, StatamError> {
        if blk.stored_len == 0 {
            if blk.raw_len != 0 {
                return Err(StatamError::Corrupt {
                    reason: "zero stored_len with non-zero raw_len",
                });
            }
            return Ok(Bytes::new());
        }
        if blk.codec == Codec::None {
            return match self {
                Self::Map {
                    primary,
                    fallback,
                    missing_reason,
                    overflow_reason,
                } => {
                    let end = blk
                        .offset
                        .checked_add(BLOCK_FRAME_HEADER_LEN as u64 + u64::from(blk.stored_len))
                        .ok_or(StatamError::BoundsExceeded {
                            reason: overflow_reason,
                        })?;
                    let key = (blk.offset, end);
                    let frame = primary
                        .get(&key)
                        .or_else(|| fallback.as_ref().and_then(|fb| fb.get(&key)))
                        .ok_or(StatamError::Corrupt {
                            reason: missing_reason,
                        })?;
                    let _ = read_framed_block_checked_cow(frame, blk, MAX_BLOCK_RAW_BYTES)?;
                    Ok(frame.slice(BLOCK_FRAME_HEADER_LEN..frame.len()))
                }
                Self::Slice { base_offset, bytes } => {
                    let rel = blk
                        .offset
                        .checked_sub(*base_offset)
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
                    let payload_start = rel.checked_add(BLOCK_FRAME_HEADER_LEN).ok_or(
                        StatamError::BoundsExceeded {
                            reason: "block payload start overflow",
                        },
                    )?;
                    let frame = bytes.get(rel..end).ok_or(StatamError::Corrupt {
                        reason: "block slice out of Point RG range",
                    })?;
                    let _ = read_framed_block_checked_cow(frame, blk, MAX_BLOCK_RAW_BYTES)?;
                    Ok(bytes.slice(payload_start..end))
                }
            };
        }
        let cow = self.read_block(blk)?;
        Ok(Bytes::from(cow.into_owned()))
    }
}

pub(super) enum DecodedPhase1 {
    Col(Column),
    ShreddedSub(ShreddedSubCol),
    FlatPayloadRefs(FlatPayloadRefs),
    SelectedPayloadRefs(Vec<(usize, PayloadRefRow)>),
}

impl StatamReader {
    /// Verifies that `sort_cols[0]` (`I64` or `TimestampNs`) is non-decreasing across every
    /// consecutive `(rg_idx, mb_idx)` mini-block in the file, with valid non-null inline bounds
    /// and non-`Dict` encoding. Covers all `num_rgs >= 1` (never skips small RG counts).
    pub(super) fn is_sort_col0_mb_monotonic(&self) -> bool {
        let num_rgs = self.meta.rg_dir.len();
        if num_rgs == 0 {
            return false;
        }
        let Some(&sc0) = self.meta.schema_meta.sort_cols.first() else {
            return false;
        };
        let sc0_idx = sc0 as usize;
        let Some(field) = self.meta.schema_meta.schema.fields().get(sc0_idx) else {
            return false;
        };
        if !matches!(
            field.data_type,
            DataType::I64 | DataType::TimestampNs { .. }
        ) {
            return false;
        }

        let mut prev_bmax: Option<i64> = None;
        for rg_idx in 0..num_rgs {
            let blks = self.meta.rg_blocks(sc0_idx, rg_idx);
            if blks.is_empty() {
                return false;
            }
            for b in blks {
                if b.encoding == Encoding::Dict
                    || b.min_inline_len != 8
                    || b.max_inline_len != 8
                    || b.null_count != 0
                    || (b.block_flags & BLOCK_FLAG_MIN_MAX_VALID) == 0
                {
                    return false;
                }
                let bmin = i64::from_le_bytes([
                    b.min_inline[0],
                    b.min_inline[1],
                    b.min_inline[2],
                    b.min_inline[3],
                    b.min_inline[4],
                    b.min_inline[5],
                    b.min_inline[6],
                    b.min_inline[7],
                ]);
                let bmax = i64::from_le_bytes([
                    b.max_inline[0],
                    b.max_inline[1],
                    b.max_inline[2],
                    b.max_inline[3],
                    b.max_inline[4],
                    b.max_inline[5],
                    b.max_inline[6],
                    b.max_inline[7],
                ]);
                if bmin > bmax {
                    return false;
                }
                if let Some(prev) = prev_bmax
                    && bmin < prev
                {
                    return false;
                }
                prev_bmax = Some(bmax);
            }
        }
        prev_bmax.is_some()
    }

    pub(super) fn resolve_filter_expr(
        &self,
        expr: &FilterExpr,
    ) -> Result<ResolvedFilterExpr, StatamError> {
        let schema = &self.meta.schema_meta.schema;
        match expr {
            FilterExpr::Pred(pred) => {
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
                Ok(ResolvedFilterExpr::Pred {
                    cidx,
                    pred: pred.clone(),
                })
            }
            FilterExpr::And(children) => {
                let mut out = Vec::with_capacity(children.len());
                for c in children {
                    out.push(self.resolve_filter_expr(c)?);
                }
                Ok(ResolvedFilterExpr::And(out))
            }
            FilterExpr::Or(children) => {
                let mut out = Vec::with_capacity(children.len());
                for c in children {
                    out.push(self.resolve_filter_expr(c)?);
                }
                Ok(ResolvedFilterExpr::Or(out))
            }
            FilterExpr::Not(inner) => Ok(ResolvedFilterExpr::Not(Box::new(
                self.resolve_filter_expr(inner)?,
            ))),
        }
    }

    pub(super) fn collect_filter_expr_dict_cols(
        &self,
        expr: &ResolvedFilterExpr,
        out: &mut Vec<usize>,
    ) {
        match expr {
            ResolvedFilterExpr::Pred { cidx, pred } => {
                out.push(*cidx);
                if let Some(key) = &pred.map_key
                    && let Some((sub_pidx, _)) = self.find_shredded_subcol(*cidx, key)
                {
                    out.push(sub_pidx);
                }
            }
            ResolvedFilterExpr::And(cs) | ResolvedFilterExpr::Or(cs) => {
                for c in cs {
                    self.collect_filter_expr_dict_cols(c, out);
                }
            }
            ResolvedFilterExpr::Not(inner) => self.collect_filter_expr_dict_cols(inner, out),
        }
    }

    pub(super) fn candidate_rg_range(
        &self,
        time_col_idx: Option<usize>,
        time_range: TimeRange,
        preds: &[(usize, &Predicate)],
        filter_exprs: &[ResolvedFilterExpr],
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
        let mut tighten = |cidx: usize, p: &Predicate| {
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
        };
        for &(cidx, p) in preds {
            tighten(cidx, p);
        }
        fn tighten_expr(expr: &ResolvedFilterExpr, tighten: &mut impl FnMut(usize, &Predicate)) {
            match expr {
                ResolvedFilterExpr::Pred { cidx, pred } => tighten(*cidx, pred),
                ResolvedFilterExpr::And(cs) => {
                    for c in cs {
                        tighten_expr(c, tighten);
                    }
                }
                ResolvedFilterExpr::Or(_) | ResolvedFilterExpr::Not(_) => {}
            }
        }
        for expr in filter_exprs {
            tighten_expr(expr, &mut tighten);
        }
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
    pub(super) fn prune_rg_mini_blocks(
        &self,
        rg_idx: usize,
        mb_count: usize,
        time_col_idx: Option<usize>,
        time_range: TimeRange,
        lookups: &[(usize, &IdLookup)],
        preds: &[(usize, &Predicate)],
        filter_exprs: &[ResolvedFilterExpr],
        subs: &[Vec<(usize, SubstringQuery)>],
        dict_sub_bitmaps: &[Vec<Option<Bitmap>>],
    ) -> Result<Vec<usize>, StatamError> {
        let fields = self.meta.schema_meta.schema.fields();
        let rg_has_no_l0_payload = self.meta.rg_dir[rg_idx].l0_payload_len == 0;
        let pack_ids = &self.meta.schema_meta.pack_ids;

        // Stage 1 (under `self.caches.lock()`): narrow pruning (`time_range`, `lookups`, `preds`,
        // `filter_exprs`) so zone-map-pruned RGs never touch `.stpk` packs.
        let caches = self.caches.lock().unwrap_or_else(|p| p.into_inner());
        let mut candidates = Vec::with_capacity(mb_count);

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
                for expr in filter_exprs {
                    if !filter_expr_may_be_true(&self.meta, rg_idx, mb_idx, expr, &caches.dicts) {
                        keep = false;
                        break;
                    }
                }
            }

            if keep {
                candidates.push(mb_idx);
            }
        }

        if candidates.is_empty() || subs.is_empty() {
            return Ok(candidates);
        }

        let needs_pack_short_circuit = rg_has_no_l0_payload
            && !pack_ids.is_empty()
            && self.packs.is_some()
            && subs.iter().any(|clause| {
                clause.iter().any(|&(cidx, ref sq)| {
                    if sq.needle.len() < 3 {
                        return false;
                    }
                    let cd = &self.meta.col_dir[cidx];
                    if cd.locality_group != LocalityGroup::ExternalPayload
                        || (cd.col_flags & COL_FLAG_HAS_TRIGRAM_INDEX) == 0
                    {
                        return false;
                    }
                    let blks = self.meta.rg_blocks(cidx, rg_idx);
                    candidates.iter().any(|&mb_idx| {
                        let blk = &blks[mb_idx];
                        blk.encoding == Encoding::PayloadRefs && blk.null_count < blk.row_count
                    })
                })
            });

        if !needs_pack_short_circuit {
            let empty_map: HashMap<(&[u8], bool), bool> = HashMap::new();
            return Ok(self.prune_candidates_by_subs(
                rg_idx,
                &candidates,
                subs,
                dict_sub_bitmaps,
                &empty_map,
                &caches,
            ));
        }

        drop(caches);

        // Stage 2 (only when needed): only open uncached `.stpk` packs eagerly before Wave 1 on
        // local readers when no narrow predicates/lookups/filter_exprs could eliminate all rows in
        // Phase 1 without opening packs (preserving remote `.stpk` prefetch overlap and
        // narrow-filter MissingPack isolation). Already-cached packs are always consulted with zero
        // extra I/O.
        enum CachedOrOpenPacks {
            Open(Vec<Arc<ChunkPackReader>>),
            Toc(Vec<Arc<PackToc>>),
        }

        let allow_open_uncached_packs = !self.reader.is_remote()
            && lookups.is_empty()
            && preds.is_empty()
            && filter_exprs.is_empty();
        let mut pack_short_circuit_by_query: HashMap<(&[u8], bool), bool> = HashMap::new();
        let mut resolved_packs: Option<Option<CachedOrOpenPacks>> = None;
        for clause in subs {
            for &(cidx, ref sq) in clause {
                if sq.needle.len() < 3 {
                    continue;
                }
                let cd = &self.meta.col_dir[cidx];
                if cd.locality_group != LocalityGroup::ExternalPayload
                    || (cd.col_flags & COL_FLAG_HAS_TRIGRAM_INDEX) == 0
                {
                    continue;
                }
                let blks = self.meta.rg_blocks(cidx, rg_idx);
                if !candidates.iter().any(|&mb_idx| {
                    let blk = &blks[mb_idx];
                    blk.encoding == Encoding::PayloadRefs && blk.null_count < blk.row_count
                }) {
                    continue;
                }
                let key = (sq.needle.as_slice(), sq.case_insensitive);
                if pack_short_circuit_by_query.contains_key(&key) {
                    continue;
                }
                let packs_opt = match resolved_packs {
                    Some(ref opt) => opt.as_ref(),
                    None => {
                        let loaded_opt = if allow_open_uncached_packs {
                            let mut loaded = Vec::with_capacity(pack_ids.len());
                            let mut missing = false;
                            for &pack_id in pack_ids {
                                match self.resolve_pack(pack_id) {
                                    Ok(pack) => loaded.push(pack),
                                    Err(StatamError::MissingPack { .. }) => {
                                        missing = true;
                                        break;
                                    }
                                    Err(e) => return Err(e),
                                }
                            }
                            (!missing).then_some(CachedOrOpenPacks::Open(loaded))
                        } else {
                            let mut toc_slots: Vec<Option<Arc<PackToc>>> = {
                                let guard = self.caches.lock().unwrap_or_else(|p| p.into_inner());
                                pack_ids
                                    .iter()
                                    .map(|pack_id| {
                                        guard.packs.get(pack_id).map(|p| Arc::clone(p.toc_arc()))
                                    })
                                    .collect()
                            };
                            let mut loaded = Vec::with_capacity(pack_ids.len());
                            let mut all_cached = true;
                            for (&pack_id, slot) in pack_ids.iter().zip(toc_slots.drain(..)) {
                                let toc = slot.or_else(|| {
                                    self.packs.as_ref().and_then(|r| r.cached_toc(pack_id))
                                });
                                if let Some(t) = toc {
                                    loaded.push(t);
                                } else {
                                    all_cached = false;
                                    break;
                                }
                            }
                            all_cached.then_some(CachedOrOpenPacks::Toc(loaded))
                        };
                        resolved_packs.insert(loaded_opt).as_ref()
                    }
                };
                let all_short_circuit = match packs_opt {
                    Some(CachedOrOpenPacks::Open(packs)) => {
                        let mut ok = true;
                        for pack in packs {
                            if !pack_short_circuits_all(pack, &sq.needle, sq.case_insensitive)? {
                                ok = false;
                                break;
                            }
                        }
                        ok
                    }
                    Some(CachedOrOpenPacks::Toc(tocs)) => tocs.iter().all(|toc| {
                        pack_toc_short_circuits_all_if_cached(toc, &sq.needle, sq.case_insensitive)
                            == Some(true)
                    }),
                    None => false,
                };
                pack_short_circuit_by_query.insert(key, all_short_circuit);
            }
        }

        let caches = self.caches.lock().unwrap_or_else(|p| p.into_inner());
        Ok(self.prune_candidates_by_subs(
            rg_idx,
            &candidates,
            subs,
            dict_sub_bitmaps,
            &pack_short_circuit_by_query,
            &caches,
        ))
    }

    fn prune_candidates_by_subs(
        &self,
        rg_idx: usize,
        candidates: &[usize],
        subs: &[Vec<(usize, SubstringQuery)>],
        dict_sub_bitmaps: &[Vec<Option<Bitmap>>],
        pack_short_circuit_by_query: &HashMap<(&[u8], bool), bool>,
        caches: &super::ReaderCaches,
    ) -> Vec<usize> {
        let rg_has_no_l0_payload = self.meta.rg_dir[rg_idx].l0_payload_len == 0;
        let pack_ids = &self.meta.schema_meta.pack_ids;
        let mut surviving = Vec::with_capacity(candidates.len());

        for &mb_idx in candidates {
            let mut keep = true;
            for (cl_idx, clause) in subs.iter().enumerate() {
                // Sound CNF OR pruning: a mini-block may ONLY be pruned if EVERY disjunct
                // in this clause is pruned or provably empty/ConstNull.
                let mut any_disjunct_may_match = false;
                for (d_idx, (cidx, sq)) in clause.iter().enumerate() {
                    let cidx = *cidx;
                    let blk = &self.meta.rg_blocks(cidx, rg_idx)[mb_idx];
                    if blk.null_count == blk.row_count {
                        continue;
                    }
                    let mut disjunct_ok = true;
                    if !sq.needle.is_empty() {
                        let dict_bm = dict_sub_bitmaps
                            .get(cl_idx)
                            .and_then(|v| v.get(d_idx))
                            .and_then(Option::as_ref);
                        if blk.encoding == Encoding::Dict
                            && let Some(code_bm) = dict_bm
                        {
                            if code_bm.all_zeros() {
                                disjunct_ok = false;
                            } else if (blk.block_flags & BLOCK_FLAG_MIN_MAX_VALID) != 0
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
                                    disjunct_ok = false;
                                }
                            }
                        } else if let Some(tri_vec) = caches.narrow_trigrams.get(&cidx) {
                            let allow_trigram_prune = blk.encoding != Encoding::PayloadRefs
                                || (rg_has_no_l0_payload
                                    && !pack_ids.is_empty()
                                    && pack_short_circuit_by_query
                                        .get(&(sq.needle.as_slice(), sq.case_insensitive))
                                        .copied()
                                        .unwrap_or(false));
                            if allow_trigram_prune {
                                let global_mb_idx = self.global_mb_idx(cidx, rg_idx, mb_idx);
                                if let Some(Some(bloom)) = tri_vec.get(global_mb_idx) {
                                    let hashes = if sq.case_insensitive {
                                        extract_ilike_trigram_hashes(&sq.needle)
                                    } else {
                                        let mut h = Vec::new();
                                        extract_trigram_hashes(&sq.needle, false, &mut h);
                                        h
                                    };
                                    if !hashes.is_empty() && !bloom.may_contain_hashes(&hashes) {
                                        disjunct_ok = false;
                                    }
                                }
                            }
                        }
                    }
                    if disjunct_ok {
                        any_disjunct_may_match = true;
                        break;
                    }
                }
                if !any_disjunct_may_match {
                    keep = false;
                    break;
                }
            }
            if keep {
                surviving.push(mb_idx);
            }
        }
        surviving
    }

    pub(super) fn global_mb_idx(&self, pidx: usize, rg_idx: usize, mb_in_rg: usize) -> usize {
        let cd = &self.meta.col_dir[pidx];
        let rg = &self.meta.rg_dir[rg_idx];
        let col_blocks = &self.meta.block_dir
            [cd.first_block_idx as usize..(cd.first_block_idx + cd.num_blocks) as usize];
        let lo = col_blocks.partition_point(|b| b.first_row < rg.first_row);
        lo + mb_in_rg
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn build_rg_io_plan(
        &self,
        rg_idx: usize,
        surviving_mbs: &[usize],
        proj_indices: &[usize],
        map_proj_by_col: &HashMap<usize, MapReadMode>,
        time_col_idx: Option<usize>,
        time_range: TimeRange,
        lookups: &[(usize, &IdLookup)],
        preds: &[(usize, &Predicate)],
        filter_exprs: &[ResolvedFilterExpr],
        subs: &[Vec<(usize, SubstringQuery)>],
    ) -> RgIoPlan {
        let fields = self.meta.schema_meta.schema.fields();
        let mut phase1_set = BTreeSet::new();
        let mut phase2_set = BTreeSet::new();
        let mut phase1_map_fast_path = HashSet::new();
        let mut phase1_needs_residual = HashSet::new();
        let mut phase2_map_fast_path = HashSet::new();
        let mut phase2_skip_subcols = HashSet::new();

        if let Some(tc) = time_col_idx {
            let time_pred = ZonePred::Between(
                ZoneValue::TimestampNs(time_range.min_ns),
                ZoneValue::TimestampNs(time_range.max_ns),
            );
            let blks = self.meta.rg_blocks(tc, rg_idx);
            let dt = &fields[tc].data_type;
            if surviving_mbs
                .iter()
                .any(|&mb| !zone_map_always_matches(&blks[mb], dt, &time_pred))
            {
                phase1_set.insert(tc);
            }
        }
        for &(cidx, _) in lookups {
            phase1_set.insert(cidx);
        }
        let mut register_pred_p1 =
            |cidx: usize, pred: &Predicate, allow_always_match_skip: bool| {
                if let Some(key) = &pred.map_key {
                    if let Some((sub_pidx, res_count)) = self.find_shredded_subcol(cidx, key) {
                        phase1_set.insert(sub_pidx);
                        if res_count == 0 {
                            let mut any_p1_needs_res = false;
                            for &mb in surviving_mbs {
                                if self.meta.rg_blocks(cidx, rg_idx)[mb].null_count == 0 {
                                    phase1_map_fast_path.insert((mb, cidx));
                                } else {
                                    phase1_needs_residual.insert((mb, cidx));
                                    any_p1_needs_res = true;
                                }
                            }
                            if any_p1_needs_res {
                                phase1_set.insert(cidx);
                            }
                        } else {
                            for &mb in surviving_mbs {
                                phase1_needs_residual.insert((mb, cidx));
                            }
                            phase1_set.insert(cidx);
                        }
                    } else {
                        for &mb in surviving_mbs {
                            phase1_needs_residual.insert((mb, cidx));
                        }
                        phase1_set.insert(cidx);
                    }
                } else {
                    let blks = self.meta.rg_blocks(cidx, rg_idx);
                    let dt = &fields[cidx].data_type;
                    let mut any_needed = false;
                    for &mb in surviving_mbs {
                        if !allow_always_match_skip
                            || !zone_map_always_matches(&blks[mb], dt, &pred.pred)
                        {
                            any_needed = true;
                            if *dt == DataType::MapUtf8 {
                                phase1_needs_residual.insert((mb, cidx));
                            }
                        }
                    }
                    if any_needed {
                        phase1_set.insert(cidx);
                    }
                }
            };
        for &(cidx, pred) in preds {
            register_pred_p1(cidx, pred, true);
        }
        fn walk_filter_expr_p1(
            expr: &ResolvedFilterExpr,
            register: &mut impl FnMut(usize, &Predicate, bool),
        ) {
            match expr {
                ResolvedFilterExpr::Pred { cidx, pred } => register(*cidx, pred, false),
                ResolvedFilterExpr::And(cs) | ResolvedFilterExpr::Or(cs) => {
                    for c in cs {
                        walk_filter_expr_p1(c, register);
                    }
                }
                ResolvedFilterExpr::Not(inner) => walk_filter_expr_p1(inner, register),
            }
        }
        for expr in filter_exprs {
            walk_filter_expr_p1(expr, &mut register_pred_p1);
        }
        phase1_map_fast_path.retain(|k| !phase1_needs_residual.contains(k));
        for clause in subs {
            for (cidx, _) in clause {
                phase1_set.insert(*cidx);
            }
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
                match map_proj_by_col.get(&cidx) {
                    Some(MapReadMode::Keys(req_keys)) => {
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
                    }
                    Some(MapReadMode::KeysOnly) => {
                        // Always schedule residual (stored_len == 0 for Const/ConstNull so 0 I/O
                        // when empty/all-null; carries rare keys, duplicate spills, and row_nulls).
                        phase2_set.insert(cidx);
                        for ke in &sm.keys {
                            let sub_pidx = ke.sub_col_idx as usize;
                            let mut any_mb_needs_sub = false;
                            for &mb in surviving_mbs {
                                let res_blk = &self.meta.rg_blocks(cidx, rg_idx)[mb];
                                let sblk = &self.meta.rg_blocks(sub_pidx, rg_idx)[mb];
                                // 0-byte skip: all-null map row OR key present on every row
                                // (null_count == 0) OR key absent on every row (null_count == row_count).
                                if res_blk.encoding == Encoding::ConstNull
                                    || sblk.null_count == 0
                                    || sblk.null_count == sblk.row_count
                                {
                                    phase2_skip_subcols.insert((mb, sub_pidx));
                                } else {
                                    any_mb_needs_sub = true;
                                }
                            }
                            if any_mb_needs_sub {
                                phase2_set.insert(sub_pidx);
                            }
                        }
                    }
                    None | Some(MapReadMode::Full) => {
                        phase2_set.insert(cidx);
                        for ke in &sm.keys {
                            phase2_set.insert(ke.sub_col_idx as usize);
                        }
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
            phase2_skip_subcols,
        }
    }

    pub(super) fn find_shredded_subcol(
        &self,
        logical_col: usize,
        key: &str,
    ) -> Option<(usize, u32)> {
        find_shredded_subcol_in_meta(&self.meta, logical_col, key)
    }
}

pub(super) fn find_shredded_subcol_in_meta(
    meta: &super::StatamFileMeta,
    logical_col: usize,
    key: &str,
) -> Option<(usize, u32)> {
    let sm = meta
        .schema_meta
        .shredded_maps
        .iter()
        .find(|s| s.logical_col_idx as usize == logical_col)?;
    let ke = sm.keys.iter().find(|k| k.key == key)?;
    Some((ke.sub_col_idx as usize, ke.residual_count))
}

fn invert_zone_pred_for_3vl(pred: &ZonePred) -> Option<ZonePred> {
    match pred {
        ZonePred::Eq(v) => Some(ZonePred::Ne(v.clone())),
        ZonePred::Ne(v) => Some(ZonePred::Eq(v.clone())),
        ZonePred::Lt(v) => Some(ZonePred::Ge(v.clone())),
        ZonePred::Le(v) => Some(ZonePred::Gt(v.clone())),
        ZonePred::Gt(v) => Some(ZonePred::Le(v.clone())),
        ZonePred::Ge(v) => Some(ZonePred::Lt(v.clone())),
        ZonePred::IsNull => Some(ZonePred::IsNotNull),
        ZonePred::IsNotNull => Some(ZonePred::IsNull),
        ZonePred::Between(_, _) | ZonePred::In(_) | ZonePred::Prefix(_) => None,
    }
}

fn zone_pred_has_nan(pred: &ZonePred) -> bool {
    match pred {
        ZonePred::Eq(ZoneValue::F64(v))
        | ZonePred::Ne(ZoneValue::F64(v))
        | ZonePred::Lt(ZoneValue::F64(v))
        | ZonePred::Le(ZoneValue::F64(v))
        | ZonePred::Gt(ZoneValue::F64(v))
        | ZonePred::Ge(ZoneValue::F64(v)) => v.is_nan(),
        _ => false,
    }
}

/// Sound 3VL zone-map check: returns `false` ONLY if no row in `(rg_idx, mb_idx)` can
/// evaluate `expr` to SQL `TRUE`.
pub(super) fn filter_expr_may_be_true(
    meta: &super::StatamFileMeta,
    rg_idx: usize,
    mb_idx: usize,
    expr: &ResolvedFilterExpr,
    dicts: &HashMap<usize, Arc<FileDict>>,
) -> bool {
    match expr {
        ResolvedFilterExpr::Pred { cidx, pred } => {
            if pred.map_key.is_some() {
                return true;
            }
            let blk = &meta.rg_blocks(*cidx, rg_idx)[mb_idx];
            let dt = &meta.schema_meta.schema.fields()[*cidx].data_type;
            zone_map_may_match(
                blk,
                dt,
                meta.col_dir[*cidx].col_flags,
                &pred.pred,
                dicts.get(cidx).map(Arc::as_ref),
            )
        }
        ResolvedFilterExpr::And(cs) => cs
            .iter()
            .all(|c| filter_expr_may_be_true(meta, rg_idx, mb_idx, c, dicts)),
        ResolvedFilterExpr::Or(cs) => cs
            .iter()
            .any(|c| filter_expr_may_be_true(meta, rg_idx, mb_idx, c, dicts)),
        ResolvedFilterExpr::Not(inner) => {
            filter_expr_may_be_false(meta, rg_idx, mb_idx, inner, dicts)
        }
    }
}

/// Sound 3VL zone-map check: returns `false` ONLY if no row in `(rg_idx, mb_idx)` can
/// evaluate `expr` to SQL `FALSE` (so `NOT expr` can never evaluate to SQL `TRUE`).
fn filter_expr_may_be_false(
    meta: &super::StatamFileMeta,
    rg_idx: usize,
    mb_idx: usize,
    expr: &ResolvedFilterExpr,
    dicts: &HashMap<usize, Arc<FileDict>>,
) -> bool {
    match expr {
        ResolvedFilterExpr::Pred { cidx, pred } => {
            if pred.map_key.is_some() {
                return true;
            }
            let Some(inv) = invert_zone_pred_for_3vl(&pred.pred) else {
                return true;
            };
            let blk = &meta.rg_blocks(*cidx, rg_idx)[mb_idx];
            let dt = &meta.schema_meta.schema.fields()[*cidx].data_type;
            if matches!(dt, DataType::F64)
                && ((blk.block_flags & BLOCK_FLAG_HAS_NAN) != 0 || zone_pred_has_nan(&pred.pred))
            {
                return true;
            }
            zone_map_may_match(
                blk,
                dt,
                meta.col_dir[*cidx].col_flags,
                &inv,
                dicts.get(cidx).map(Arc::as_ref),
            )
        }
        ResolvedFilterExpr::And(cs) => cs
            .iter()
            .any(|c| filter_expr_may_be_false(meta, rg_idx, mb_idx, c, dicts)),
        ResolvedFilterExpr::Or(cs) => cs
            .iter()
            .all(|c| filter_expr_may_be_false(meta, rg_idx, mb_idx, c, dicts)),
        ResolvedFilterExpr::Not(inner) => {
            filter_expr_may_be_true(meta, rg_idx, mb_idx, inner, dicts)
        }
    }
}

pub(super) fn fetch_coalesced_blocks(
    reader: &dyn RangeReader,
    raw_ranges: &[(u64, u64)],
    retained_tail_start: Option<u64>,
) -> Result<HashMap<(u64, u64), Bytes>, StatamError> {
    if raw_ranges.is_empty() {
        return Ok(HashMap::new());
    }
    let mut sorted: Vec<(u64, u64)> = raw_ranges.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let is_remote = reader.is_remote();
    let mut coalesced: Vec<Range<u64>> = Vec::new();
    let mut groups: Vec<Vec<(u64, u64)>> = Vec::new();
    let mut span_useful: u64 = 0;
    for &(s, e) in &sorted {
        if s > e {
            return Err(StatamError::Corrupt {
                reason: "invalid block range with start > end",
            });
        }
        if let Some(last_r) = coalesced.last_mut() {
            let crosses_tail = retained_tail_start.is_some_and(|ts| last_r.start < ts && s >= ts);
            let gap = s.saturating_sub(last_r.end);
            let merged_end = last_r.end.max(e);
            let new_span_len = merged_end.saturating_sub(last_r.start);
            let added_useful = e.saturating_sub(s.max(last_r.end));
            let new_useful = span_useful.saturating_add(added_useful);
            let new_wasted = new_span_len.saturating_sub(new_useful);
            let within_span = e <= last_r.end || new_span_len <= SCAN_COALESCE_MAX_SPAN;
            let within_gap = gap <= SCAN_COALESCE_MAX_GAP
                || (is_remote
                    && gap <= REMOTE_SCAN_COALESCE_MAX_GAP
                    && added_useful > 0
                    && new_wasted <= new_useful);
            if !crosses_tail && within_span && within_gap {
                last_r.end = merged_end;
                span_useful = new_useful;
                if let Some(g) = groups.last_mut() {
                    g.push((s, e));
                }
                continue;
            }
        }
        coalesced.push(s..e);
        groups.push(vec![(s, e)]);
        span_useful = e - s;
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

pub(super) fn find_time_column(schema: &Schema) -> Option<usize> {
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
