//! Row Group / Mini-Block pruning, I/O planning, and coalesced range fetching.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use bytes::Bytes;
use cetra_column::{Bitmap, Column, DataType, Schema};

use crate::StatamError;
use crate::codec::nested::ShreddedSubCol;
use crate::codec::{ZonePred, ZoneValue, zone_map_always_matches, zone_map_may_match};
use crate::format::{
    BLOCK_FLAG_MIN_MAX_VALID, BLOCK_FRAME_HEADER_LEN, BlockEntry, Encoding, MAX_BLOCK_RAW_BYTES,
    read_framed_block_checked_cow,
};
use crate::io::RangeReader;
use crate::payload::search::extract_ilike_trigram_hashes;
use crate::payload::{FlatPayloadRefs, PayloadRefRow};
use crate::simd::trigram::extract_trigram_hashes;

use super::eval::id_lookup_zone_pred;
use super::{
    IdLookup, Predicate, REMOTE_SCAN_COALESCE_MAX_GAP, SCAN_COALESCE_MAX_GAP,
    SCAN_COALESCE_MAX_SPAN, StatamReader, SubstringQuery, TimeRange,
};

pub(super) struct RgIoPlan {
    pub(super) phase1_phys_cols: Vec<usize>,
    pub(super) phase2_only_phys_cols: Vec<usize>,
    pub(super) phase1_map_fast_path: HashSet<(usize, usize)>,
    pub(super) phase2_map_fast_path: HashSet<(usize, usize)>,
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

    pub(super) fn candidate_rg_range(
        &self,
        time_col_idx: Option<usize>,
        time_range: TimeRange,
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
                            if let Some(Some(bloom)) = tri_vec.get(global_mb_idx) {
                                let hashes = if sq.case_insensitive {
                                    extract_ilike_trigram_hashes(&sq.needle)
                                } else {
                                    let mut h = Vec::new();
                                    extract_trigram_hashes(&sq.needle, false, &mut h);
                                    h
                                };
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
        map_keys_by_col: &HashMap<usize, Vec<String>>,
        time_col_idx: Option<usize>,
        time_range: TimeRange,
        lookups: &[(usize, &IdLookup)],
        preds: &[(usize, &Predicate)],
        subs: &[(usize, &SubstringQuery)],
    ) -> RgIoPlan {
        let fields = self.meta.schema_meta.schema.fields();
        let mut phase1_set = BTreeSet::new();
        let mut phase2_set = BTreeSet::new();
        let mut phase1_map_fast_path = HashSet::new();
        let mut phase1_needs_residual = HashSet::new();
        let mut phase2_map_fast_path = HashSet::new();

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
                    if !zone_map_always_matches(&blks[mb], dt, &pred.pred) {
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
        }
        phase1_map_fast_path.retain(|k| !phase1_needs_residual.contains(k));
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
