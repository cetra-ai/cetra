//! Phase 1 predicate evaluation, raw UTF-8 substring matching, and SIMD filter dispatch.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use cetra_column::{
    Bitmap, CancelToken, Column, DataType, FlatColumn, FlatData, ScalarValue, Utf8Data,
};
use memchr::memmem;

use crate::StatamError;
use crate::codec::dict::{FileDict, decode_dict_codes_body};
use crate::codec::fsst::{FsstBlockView, FsstSubchunkedView};
use crate::codec::nested::{
    ShreddedSubCol, decode_shredded_residual_block, decode_shredded_subcol, decode_single_map_key,
};
use crate::codec::numeric::read_optional_nulls;
use crate::codec::{
    FastUtf8Ref, ZonePred, ZoneValue, decode_mini_block, utf8_bytes_at, zone_map_always_matches,
    zone_map_may_match,
};
use crate::format::{
    BLOCK_FLAG_CONST_EMPTY_COLLECTION, BLOCK_FLAG_FSST_SUBCHUNKED, BLOCK_FLAG_HAS_NAN, BlockEntry,
    COL_FLAG_HAS_FILE_DICT, COL_FLAG_IS_SHREDDED_SUBCOL, Encoding, LocalityGroup,
    MAX_BATCH_DECODED_BYTES, MAX_BLOCK_RAW_BYTES,
};
use crate::parallel::{MAX_PARALLEL_THREADS, parallel_map_in_order_static};
use crate::payload::search::{
    RetainedPackChunks, extract_unicode_ilike_ascii_anchor, pack_short_circuits_all,
    unicode_lowercase_context_free_into_bytes,
};
use crate::payload::{
    FLAT_REF_NULL, FlatPayloadRefs, decode_payload_refs_flat, scan_payload_refs_inline_substring,
};
use crate::simd::bitpack::{packed_bytes_len, unpack_u32};
use crate::simd::filter::{FilterOp, filter_f64, filter_i64, filter_i64_between, filter_u32};
use crate::simd::trigram::{
    ascii_lowercase_fold_ks_into, ascii_lowercase_into, contains_kelvin_or_long_s,
};

use super::meta::StatamFileMeta;
use super::payload::ParsedL0Payload;
use super::plan::{
    ActiveMiniBlock, BlockCacheReader, DecodedPhase1, ResolvedFilterExpr, RgIoPlan,
    find_shredded_subcol_in_meta,
};
use super::{IdLookup, Predicate, StatamReader, SubstringQuery, TimeRange, effective_read_threads};

impl StatamReader {
    fn preload_p1_dicts(&self, plan: &RgIoPlan) -> Arc<HashMap<usize, Arc<FileDict>>> {
        let caches = self.caches.lock().unwrap_or_else(|p| p.into_inner());
        let mut out = HashMap::new();
        for &pidx in &plan.phase1_phys_cols {
            if (self.meta.col_dir[pidx].col_flags & COL_FLAG_HAS_FILE_DICT) != 0
                && let Some(dict) = caches.dicts.get(&pidx)
            {
                out.insert(pidx, Arc::clone(dict));
            }
        }
        Arc::new(out)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn eval_phase1_mbs(
        &self,
        rg_idx: usize,
        surviving_mbs: &[usize],
        plan: &Arc<RgIoPlan>,
        proj_indices: &[usize],
        time_col_idx: Option<usize>,
        time_range: TimeRange,
        lookups: &[(usize, &IdLookup)],
        preds: &[(usize, &Predicate)],
        filter_exprs: &[ResolvedFilterExpr],
        subs: &[Vec<(usize, SubstringQuery)>],
        dict_sub_bitmaps: &[Vec<Option<Bitmap>>],
        l0_local: Option<&ParsedL0Payload>,
        read_block: &BlockCacheReader,
        limit_remaining: Option<usize>,
        trim_tail: bool,
        read_threads: usize,
        retained: &RetainedPackChunks,
        cancel: Option<&CancelToken>,
    ) -> Result<Vec<ActiveMiniBlock>, StatamError> {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Err(StatamError::Cancelled);
        }
        if limit_remaining == Some(0) {
            return Ok(Vec::new());
        }
        let threads = read_threads.clamp(1, MAX_PARALLEL_THREADS);
        let has_payload_sub = subs.iter().any(|clause| {
            clause.iter().any(|&(cidx, _)| {
                self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload
            })
        });
        if has_payload_sub && surviving_mbs.len() >= 2 && limit_remaining.is_none() {
            return self.eval_phase1_mbs_batched_payload(
                rg_idx,
                surviving_mbs,
                plan,
                proj_indices,
                time_col_idx,
                time_range,
                lookups,
                preds,
                filter_exprs,
                subs,
                dict_sub_bitmaps,
                l0_local,
                read_block,
                threads,
                retained,
                cancel,
            );
        }
        let mb_workers = if has_payload_sub {
            threads
        } else {
            let mut work_bytes = 0usize;
            for &mb_idx in surviving_mbs {
                for &pidx in &plan.phase1_phys_cols {
                    work_bytes = work_bytes
                        .saturating_add(self.meta.rg_blocks(pidx, rg_idx)[mb_idx].raw_len as usize);
                }
            }
            effective_read_threads(work_bytes, threads)
        };
        if !has_payload_sub
            && mb_workers > 1
            && surviving_mbs.len() >= 2
            && limit_remaining.is_none()
        {
            let fields = self.meta.schema_meta.schema.fields();
            let preload_always_matched_col = if !self.reader.is_remote()
                && preds.len() == 1
                && preds[0].1.map_key.is_none()
                && filter_exprs.is_empty()
                && lookups.is_empty()
                && subs.is_empty()
                && time_col_idx.is_none()
            {
                let cidx = preds[0].0;
                if plan.phase1_phys_cols.contains(&cidx)
                    && proj_indices.contains(&cidx)
                    && self.meta.col_dir[cidx].locality_group != LocalityGroup::ExternalPayload
                    && fields[cidx].data_type != DataType::MapUtf8
                {
                    Some(cidx)
                } else {
                    None
                }
            } else {
                None
            };
            let meta = Arc::clone(&self.meta);
            let plan_arc = Arc::clone(plan);
            let block_reader = read_block.clone();
            let p1_dicts = self.preload_p1_dicts(plan);
            let owned_lookups: Arc<Vec<(usize, IdLookup)>> =
                Arc::new(lookups.iter().map(|&(c, lk)| (c, (*lk).clone())).collect());
            let owned_preds: Arc<Vec<(usize, Predicate)>> =
                Arc::new(preds.iter().map(|&(c, p)| (c, (*p).clone())).collect());
            let owned_filter_exprs: Arc<Vec<ResolvedFilterExpr>> = Arc::new(filter_exprs.to_vec());
            let owned_subs: Arc<Vec<Vec<(usize, SubstringQuery)>>> = Arc::new(subs.to_vec());
            let owned_dict_sub_bitmaps: Arc<Vec<Vec<Option<Bitmap>>>> =
                Arc::new(dict_sub_bitmaps.to_vec());
            let per_mb =
                parallel_map_in_order_static(surviving_mbs.to_vec(), mb_workers, move |mb_idx| {
                    let (mask, decoded_p1) = eval_phase1_mb_narrow_static(
                        &meta,
                        rg_idx,
                        mb_idx,
                        &plan_arc,
                        time_col_idx,
                        time_range,
                        &owned_lookups,
                        &owned_preds,
                        &owned_filter_exprs,
                        &owned_subs,
                        &owned_dict_sub_bitmaps,
                        &p1_dicts,
                        &block_reader,
                        preload_always_matched_col,
                    )?;
                    Ok((mb_idx, mask, decoded_p1))
                })?;
            let mut active = Vec::new();
            for (mb_idx, mask, decoded_p1) in per_mb {
                if mask.count_ones() > 0 {
                    active.push((mb_idx, mask, decoded_p1));
                }
            }
            return Ok(active);
        }

        let mut active_mbs = Vec::new();
        let mut active_rows = 0usize;
        for &mb_idx in surviving_mbs {
            if cancel.is_some_and(CancelToken::is_cancelled) {
                return Err(StatamError::Cancelled);
            }
            let (mut mask, decoded_p1) = self.eval_phase1_mb(
                rg_idx,
                mb_idx,
                plan,
                proj_indices,
                time_col_idx,
                time_range,
                lookups,
                preds,
                filter_exprs,
                subs,
                dict_sub_bitmaps,
                l0_local,
                read_block,
                read_threads,
                retained,
                None,
                cancel,
            )?;
            let hits = mask.count_ones();
            if hits > 0 {
                if let Some(lim) = limit_remaining {
                    let rem = lim.saturating_sub(active_rows);
                    if rem == 0 {
                        break;
                    }
                    if hits > rem {
                        trim_bitmap_mask(&mut mask, rem, trim_tail);
                        active_mbs.push((mb_idx, mask, decoded_p1));
                        break;
                    }
                    active_rows = active_rows.saturating_add(hits);
                    active_mbs.push((mb_idx, mask, decoded_p1));
                    if active_rows >= lim {
                        break;
                    }
                } else {
                    active_rows = active_rows.saturating_add(hits);
                    active_mbs.push((mb_idx, mask, decoded_p1));
                }
            }
        }
        Ok(active_mbs)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn eval_phase1_mbs_batched_payload(
        &self,
        rg_idx: usize,
        surviving_mbs: &[usize],
        plan: &Arc<RgIoPlan>,
        proj_indices: &[usize],
        time_col_idx: Option<usize>,
        time_range: TimeRange,
        lookups: &[(usize, &IdLookup)],
        preds: &[(usize, &Predicate)],
        filter_exprs: &[ResolvedFilterExpr],
        subs: &[Vec<(usize, SubstringQuery)>],
        dict_sub_bitmaps: &[Vec<Option<Bitmap>>],
        l0_local: Option<&ParsedL0Payload>,
        read_block: &BlockCacheReader,
        threads: usize,
        retained: &RetainedPackChunks,
        cancel: Option<&CancelToken>,
    ) -> Result<Vec<ActiveMiniBlock>, StatamError> {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Err(StatamError::Cancelled);
        }
        // Stage 1: Evaluate narrow predicates, 3VL filter_exprs, and pure-narrow CNF substring
        // clauses across surviving MBs so masks are as selective as possible before touching `.stpk`.
        let mut narrow_work_bytes = 0usize;
        for &mb_idx in surviving_mbs {
            for &pidx in &plan.phase1_phys_cols {
                if self.meta.col_dir[pidx].locality_group != LocalityGroup::ExternalPayload {
                    narrow_work_bytes = narrow_work_bytes
                        .saturating_add(self.meta.rg_blocks(pidx, rg_idx)[mb_idx].raw_len as usize);
                }
            }
        }
        let narrow_workers = effective_read_threads(narrow_work_bytes, threads);
        let mut active: Vec<ActiveMiniBlock> = if narrow_workers > 1 && surviving_mbs.len() >= 2 {
            let meta = Arc::clone(&self.meta);
            let plan_arc = Arc::clone(plan);
            let block_reader = read_block.clone();
            let p1_dicts = self.preload_p1_dicts(plan);
            let owned_lookups: Arc<Vec<(usize, IdLookup)>> =
                Arc::new(lookups.iter().map(|&(c, lk)| (c, (*lk).clone())).collect());
            let owned_preds: Arc<Vec<(usize, Predicate)>> =
                Arc::new(preds.iter().map(|&(c, p)| (c, (*p).clone())).collect());
            let owned_filter_exprs: Arc<Vec<ResolvedFilterExpr>> = Arc::new(filter_exprs.to_vec());
            let owned_subs: Arc<Vec<Vec<(usize, SubstringQuery)>>> = Arc::new(subs.to_vec());
            let owned_dict_sub_bitmaps: Arc<Vec<Vec<Option<Bitmap>>>> =
                Arc::new(dict_sub_bitmaps.to_vec());
            let per_mb = parallel_map_in_order_static(
                surviving_mbs.to_vec(),
                narrow_workers,
                move |mb_idx| {
                    let (mask, decoded_p1) = eval_phase1_mb_narrow_static(
                        &meta,
                        rg_idx,
                        mb_idx,
                        &plan_arc,
                        time_col_idx,
                        time_range,
                        &owned_lookups,
                        &owned_preds,
                        &owned_filter_exprs,
                        &owned_subs,
                        &owned_dict_sub_bitmaps,
                        &p1_dicts,
                        &block_reader,
                        None,
                    )?;
                    Ok((mb_idx, mask, decoded_p1))
                },
            )?;
            per_mb
                .into_iter()
                .filter(|(_, mask, _)| mask.count_ones() > 0)
                .collect()
        } else {
            let mut out = Vec::with_capacity(surviving_mbs.len());
            for &mb_idx in surviving_mbs {
                if cancel.is_some_and(CancelToken::is_cancelled) {
                    return Err(StatamError::Cancelled);
                }
                let (mask, decoded_p1) = self.eval_phase1_mb_narrow(
                    rg_idx,
                    mb_idx,
                    plan,
                    time_col_idx,
                    time_range,
                    lookups,
                    preds,
                    filter_exprs,
                    subs,
                    dict_sub_bitmaps,
                    read_block,
                    None,
                )?;
                if mask.count_ones() > 0 {
                    out.push((mb_idx, mask, decoded_p1));
                }
            }
            out
        };

        if active.is_empty() {
            return Ok(active);
        }

        // Stage 2: Evaluate each mixed/payload CNF clause ONCE across all surviving MBs so `.stpk`
        // frame reads are deduplicated into a single wave per pack, evaluating narrow disjuncts
        // first to shrink per-MB remaining masks before touching ExternalPayload columns.
        for (ci, clause) in subs.iter().enumerate() {
            let clause_has_payload = clause.iter().any(|(cidx, _)| {
                self.meta.col_dir[*cidx].locality_group == LocalityGroup::ExternalPayload
            });
            if !clause_has_payload {
                continue;
            }
            if clause.is_empty() {
                return Ok(Vec::new());
            }

            let mut clause_matched: Vec<Bitmap> = active
                .iter()
                .map(|(_, mask, _)| Bitmap::new_zeroed(mask.len()))
                .collect();
            let mut remaining: Vec<Bitmap> =
                active.iter().map(|(_, mask, _)| mask.clone()).collect();

            // 2a. Narrow disjuncts first across all active MBs.
            for (di, (cidx, sq)) in clause.iter().enumerate() {
                if self.meta.col_dir[*cidx].locality_group == LocalityGroup::ExternalPayload {
                    continue;
                }
                let dict_bm = dict_sub_bitmaps
                    .get(ci)
                    .and_then(|v| v.get(di))
                    .and_then(Option::as_ref);
                for (pos, (mb_idx, _, decoded_p1)) in active.iter_mut().enumerate() {
                    if remaining[pos].all_zeros() {
                        continue;
                    }
                    let mb_row_count = self.meta.rg_blocks(0, rg_idx)[*mb_idx].row_count as usize;
                    let hits = eval_narrow_sub_disjunct(
                        &self.meta,
                        rg_idx,
                        *mb_idx,
                        mb_row_count,
                        *cidx,
                        sq,
                        dict_bm,
                        &remaining[pos],
                        &|c| self.get_cached_dict(c),
                        read_block,
                        decoded_p1,
                    )?;
                    if !hits.all_zeros() {
                        clause_matched[pos] = clause_matched[pos].or(&hits)?;
                        remaining[pos] = remaining[pos].and(&hits.not())?;
                    }
                }
            }

            // 2b. ExternalPayload disjuncts ONLY on rows still unmatched in `remaining`.
            for (di, (cidx, sq)) in clause.iter().enumerate() {
                if self.meta.col_dir[*cidx].locality_group != LocalityGroup::ExternalPayload {
                    continue;
                }
                if cancel.is_some_and(CancelToken::is_cancelled) {
                    return Err(StatamError::Cancelled);
                }
                if remaining.iter().all(Bitmap::all_zeros) {
                    break;
                }

                let mut pack_chunk_counts = Vec::new();
                let mut pack_has_edges = Vec::new();
                let mut inline_fast_path_ok = false;

                if l0_local.is_none() && !self.meta.schema_meta.pack_ids.is_empty() {
                    let mut all_short_circuit = true;
                    pack_chunk_counts.reserve(self.meta.schema_meta.pack_ids.len());
                    pack_has_edges.reserve(self.meta.schema_meta.pack_ids.len());
                    for &pack_id in &self.meta.schema_meta.pack_ids {
                        let pack = self.resolve_pack(pack_id)?;
                        if !pack_short_circuits_all(&pack, &sq.needle, sq.case_insensitive)? {
                            all_short_circuit = false;
                            break;
                        }
                        pack_chunk_counts.push(pack.toc().chunks.len());
                        pack_has_edges.push(pack.postscript().edge_table_len > 0);
                    }
                    if all_short_circuit {
                        let mut needed_by_other = false;
                        'other: for (oci, oclause) in subs.iter().enumerate() {
                            for (odi, (oc, osq)) in oclause.iter().enumerate() {
                                if (oci != ci || odi != di) && *oc == *cidx {
                                    for &pack_id in &self.meta.schema_meta.pack_ids {
                                        let pack = self.resolve_pack(pack_id)?;
                                        if !pack_short_circuits_all(
                                            &pack,
                                            &osq.needle,
                                            osq.case_insensitive,
                                        )? {
                                            needed_by_other = true;
                                            break 'other;
                                        }
                                    }
                                }
                            }
                        }
                        inline_fast_path_ok = !needed_by_other;
                    }
                }

                let mut batch_positions: Vec<usize> = Vec::new();
                for (pos, (mb_idx, _, decoded_p1)) in active.iter_mut().enumerate() {
                    if remaining[pos].all_zeros() {
                        continue;
                    }
                    let mb_row_count = self.meta.rg_blocks(0, rg_idx)[*mb_idx].row_count as usize;
                    if !decoded_p1.contains_key(cidx) {
                        let blk = &self.meta.rg_blocks(*cidx, rg_idx)[*mb_idx];
                        if blk.encoding == Encoding::ConstNull {
                            // Disjunct produces 0 hits for this MB (do NOT zero other OR branches).
                            continue;
                        }
                        if inline_fast_path_ok {
                            let raw = read_block.read_block(blk)?;
                            let mut matcher = Utf8Matcher::new(&sq.needle, sq.case_insensitive);
                            let bm = scan_payload_refs_inline_substring(
                                &raw,
                                mb_row_count,
                                &remaining[pos],
                                &pack_chunk_counts,
                                &pack_has_edges,
                                |bytes| matcher.matches(bytes),
                            )?;
                            let hits = bm.and(&remaining[pos])?;
                            if !hits.all_zeros() {
                                clause_matched[pos] = clause_matched[pos].or(&hits)?;
                                remaining[pos] = remaining[pos].and(&hits.not())?;
                            }
                            continue;
                        }
                        self.ensure_p1_col(
                            *cidx,
                            rg_idx,
                            *mb_idx,
                            mb_row_count,
                            read_block,
                            decoded_p1,
                        )?;
                    }
                    batch_positions.push(pos);
                }

                if !batch_positions.is_empty() {
                    let mut mb_inputs: Vec<(&FlatPayloadRefs, &Bitmap)> =
                        Vec::with_capacity(batch_positions.len());
                    for &pos in &batch_positions {
                        let (_, _, decoded_p1) = &active[pos];
                        let DecodedPhase1::FlatPayloadRefs(refs) = &decoded_p1[cidx] else {
                            return Err(StatamError::Corrupt {
                                reason: "unexpected Phase 1 variant for ExternalPayload substring query",
                            });
                        };
                        mb_inputs.push((refs, &remaining[pos]));
                    }
                    let batch_bms = self.eval_payload_refs_substring_batch(
                        &mb_inputs,
                        &sq.needle,
                        sq.case_insensitive,
                        l0_local,
                        threads,
                        if proj_indices.contains(cidx) {
                            Some(retained)
                        } else {
                            None
                        },
                        cancel,
                    )?;
                    for (pos, bm) in batch_positions.into_iter().zip(batch_bms) {
                        let hits = bm.and(&remaining[pos])?;
                        if !hits.all_zeros() {
                            clause_matched[pos] = clause_matched[pos].or(&hits)?;
                            remaining[pos] = remaining[pos].and(&hits.not())?;
                        }
                    }
                }
            }

            for (pos, (_, mask, _)) in active.iter_mut().enumerate() {
                *mask = mask.and(&clause_matched[pos])?;
            }
            active.retain(|(_, mask, _)| mask.count_ones() > 0);
            if active.is_empty() {
                break;
            }
        }

        Ok(active)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn eval_phase1_mb(
        &self,
        rg_idx: usize,
        mb_idx: usize,
        plan: &RgIoPlan,
        proj_indices: &[usize],
        time_col_idx: Option<usize>,
        time_range: TimeRange,
        lookups: &[(usize, &IdLookup)],
        preds: &[(usize, &Predicate)],
        filter_exprs: &[ResolvedFilterExpr],
        subs: &[Vec<(usize, SubstringQuery)>],
        dict_sub_bitmaps: &[Vec<Option<Bitmap>>],
        l0_local: Option<&ParsedL0Payload>,
        read_block: &BlockCacheReader,
        read_threads: usize,
        retained: &RetainedPackChunks,
        preload_always_matched_col: Option<usize>,
        cancel: Option<&CancelToken>,
    ) -> Result<(Bitmap, HashMap<usize, DecodedPhase1>), StatamError> {
        let (mut mask, mut decoded_p1) = self.eval_phase1_mb_narrow(
            rg_idx,
            mb_idx,
            plan,
            time_col_idx,
            time_range,
            lookups,
            preds,
            filter_exprs,
            subs,
            dict_sub_bitmaps,
            read_block,
            preload_always_matched_col,
        )?;
        if mask.all_zeros() {
            return Ok((mask, decoded_p1));
        }

        let mb_row_count = self.meta.rg_blocks(0, rg_idx)[mb_idx].row_count as usize;
        for (ci, clause) in subs.iter().enumerate() {
            let clause_has_payload = clause.iter().any(|(cidx, _)| {
                self.meta.col_dir[*cidx].locality_group == LocalityGroup::ExternalPayload
            });
            if !clause_has_payload {
                continue;
            }
            if clause.is_empty() {
                return Ok((Bitmap::new_zeroed(mb_row_count), decoded_p1));
            }

            let mut clause_matched = Bitmap::new_zeroed(mb_row_count);
            let mut remaining = mask.clone();

            // Narrow disjuncts first so `remaining` is minimized before touching `.stpk` / L0.
            for (di, (cidx, sq)) in clause.iter().enumerate() {
                if self.meta.col_dir[*cidx].locality_group == LocalityGroup::ExternalPayload {
                    continue;
                }
                if remaining.all_zeros() {
                    break;
                }
                let dict_bm = dict_sub_bitmaps
                    .get(ci)
                    .and_then(|v| v.get(di))
                    .and_then(Option::as_ref);
                let hits = eval_narrow_sub_disjunct(
                    &self.meta,
                    rg_idx,
                    mb_idx,
                    mb_row_count,
                    *cidx,
                    sq,
                    dict_bm,
                    &remaining,
                    &|c| self.get_cached_dict(c),
                    read_block,
                    &mut decoded_p1,
                )?;
                if !hits.all_zeros() {
                    clause_matched = clause_matched.or(&hits)?;
                    remaining = remaining.and(&hits.not())?;
                }
            }

            // ExternalPayload disjuncts ONLY on rows still in `remaining`.
            for (di, (cidx, sq)) in clause.iter().enumerate() {
                if self.meta.col_dir[*cidx].locality_group != LocalityGroup::ExternalPayload {
                    continue;
                }
                if remaining.all_zeros() {
                    break;
                }
                if cancel.is_some_and(CancelToken::is_cancelled) {
                    return Err(StatamError::Cancelled);
                }
                if !decoded_p1.contains_key(cidx) {
                    let blk = &self.meta.rg_blocks(*cidx, rg_idx)[mb_idx];
                    if blk.encoding == Encoding::ConstNull {
                        continue;
                    }
                    if l0_local.is_none() && !self.meta.schema_meta.pack_ids.is_empty() {
                        let mut all_short_circuit = true;
                        let mut pack_chunk_counts =
                            Vec::with_capacity(self.meta.schema_meta.pack_ids.len());
                        let mut pack_has_edges =
                            Vec::with_capacity(self.meta.schema_meta.pack_ids.len());
                        for &pack_id in &self.meta.schema_meta.pack_ids {
                            let pack = self.resolve_pack(pack_id)?;
                            if !pack_short_circuits_all(&pack, &sq.needle, sq.case_insensitive)? {
                                all_short_circuit = false;
                                break;
                            }
                            pack_chunk_counts.push(pack.toc().chunks.len());
                            pack_has_edges.push(pack.postscript().edge_table_len > 0);
                        }
                        if all_short_circuit {
                            let mut needed_by_other = false;
                            'other: for (oci, oclause) in subs.iter().enumerate() {
                                for (odi, (oc, osq)) in oclause.iter().enumerate() {
                                    if (oci != ci || odi != di) && *oc == *cidx {
                                        for &pack_id in &self.meta.schema_meta.pack_ids {
                                            let pack = self.resolve_pack(pack_id)?;
                                            if !pack_short_circuits_all(
                                                &pack,
                                                &osq.needle,
                                                osq.case_insensitive,
                                            )? {
                                                needed_by_other = true;
                                                break 'other;
                                            }
                                        }
                                    }
                                }
                            }
                            if !needed_by_other {
                                let raw = read_block.read_block(blk)?;
                                let mut matcher = Utf8Matcher::new(&sq.needle, sq.case_insensitive);
                                let bm = scan_payload_refs_inline_substring(
                                    &raw,
                                    mb_row_count,
                                    &remaining,
                                    &pack_chunk_counts,
                                    &pack_has_edges,
                                    |bytes| matcher.matches(bytes),
                                )?;
                                let hits = bm.and(&remaining)?;
                                if !hits.all_zeros() {
                                    clause_matched = clause_matched.or(&hits)?;
                                    remaining = remaining.and(&hits.not())?;
                                }
                                continue;
                            }
                        }
                    }
                }
                self.ensure_p1_col(
                    *cidx,
                    rg_idx,
                    mb_idx,
                    mb_row_count,
                    read_block,
                    &mut decoded_p1,
                )?;
                let item = &decoded_p1[cidx];
                let bm = match item {
                    DecodedPhase1::FlatPayloadRefs(refs) => self.eval_payload_refs_substring(
                        refs,
                        &remaining,
                        &sq.needle,
                        sq.case_insensitive,
                        l0_local,
                        read_threads,
                        if proj_indices.contains(cidx) {
                            Some(retained)
                        } else {
                            None
                        },
                        cancel,
                    )?,
                    _ => {
                        return Err(StatamError::Corrupt {
                            reason: "unexpected Phase 1 variant for substring query",
                        });
                    }
                };
                let hits = bm.and(&remaining)?;
                if !hits.all_zeros() {
                    clause_matched = clause_matched.or(&hits)?;
                    remaining = remaining.and(&hits.not())?;
                }
            }

            mask = mask.and(&clause_matched)?;
            if mask.all_zeros() {
                return Ok((mask, decoded_p1));
            }
        }

        Ok((mask, decoded_p1))
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_phase1_mb_narrow(
        &self,
        rg_idx: usize,
        mb_idx: usize,
        plan: &RgIoPlan,
        time_col_idx: Option<usize>,
        time_range: TimeRange,
        lookups: &[(usize, &IdLookup)],
        preds: &[(usize, &Predicate)],
        filter_exprs: &[ResolvedFilterExpr],
        subs: &[Vec<(usize, SubstringQuery)>],
        dict_sub_bitmaps: &[Vec<Option<Bitmap>>],
        read_block: &BlockCacheReader,
        preload_always_matched_col: Option<usize>,
    ) -> Result<(Bitmap, HashMap<usize, DecodedPhase1>), StatamError> {
        eval_phase1_mb_narrow_impl(
            &self.meta,
            rg_idx,
            mb_idx,
            plan,
            time_col_idx,
            time_range,
            lookups,
            preds,
            filter_exprs,
            subs,
            dict_sub_bitmaps,
            &|cidx| self.get_cached_dict(cidx),
            read_block,
            preload_always_matched_col,
        )
    }

    fn ensure_p1_col(
        &self,
        pidx: usize,
        rg_idx: usize,
        mb_idx: usize,
        mb_row_count: usize,
        read_block: &BlockCacheReader,
        decoded_p1: &mut HashMap<usize, DecodedPhase1>,
    ) -> Result<(), StatamError> {
        ensure_p1_col_with_meta(
            &self.meta,
            pidx,
            rg_idx,
            mb_idx,
            mb_row_count,
            &|cidx| self.get_cached_dict(cidx),
            read_block,
            decoded_p1,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn eval_phase1_mb_narrow_static(
    meta: &StatamFileMeta,
    rg_idx: usize,
    mb_idx: usize,
    plan: &RgIoPlan,
    time_col_idx: Option<usize>,
    time_range: TimeRange,
    lookups: &[(usize, IdLookup)],
    preds: &[(usize, Predicate)],
    filter_exprs: &[ResolvedFilterExpr],
    subs: &[Vec<(usize, SubstringQuery)>],
    dict_sub_bitmaps: &[Vec<Option<Bitmap>>],
    p1_dicts: &HashMap<usize, Arc<FileDict>>,
    read_block: &BlockCacheReader,
    preload_always_matched_col: Option<usize>,
) -> Result<(Bitmap, HashMap<usize, DecodedPhase1>), StatamError> {
    let lookup_refs: Vec<(usize, &IdLookup)> = lookups.iter().map(|(c, lk)| (*c, lk)).collect();
    let pred_refs: Vec<(usize, &Predicate)> = preds.iter().map(|(c, p)| (*c, p)).collect();
    eval_phase1_mb_narrow_impl(
        meta,
        rg_idx,
        mb_idx,
        plan,
        time_col_idx,
        time_range,
        &lookup_refs,
        &pred_refs,
        filter_exprs,
        subs,
        dict_sub_bitmaps,
        &|cidx| p1_dicts.get(&cidx).map(Arc::clone),
        read_block,
        preload_always_matched_col,
    )
}

#[allow(clippy::too_many_arguments)]
fn eval_narrow_sub_disjunct(
    meta: &StatamFileMeta,
    rg_idx: usize,
    mb_idx: usize,
    mb_row_count: usize,
    cidx: usize,
    sq: &SubstringQuery,
    dict_bm: Option<&Bitmap>,
    active_mask: &Bitmap,
    get_dict: &dyn Fn(usize) -> Option<Arc<FileDict>>,
    read_block: &BlockCacheReader,
    decoded_p1: &mut HashMap<usize, DecodedPhase1>,
) -> Result<Bitmap, StatamError> {
    if active_mask.all_zeros() {
        return Ok(Bitmap::new_zeroed(mb_row_count));
    }
    if !decoded_p1.contains_key(&cidx) {
        let blk = &meta.rg_blocks(cidx, rg_idx)[mb_idx];
        if blk.encoding == Encoding::ConstNull {
            return Ok(Bitmap::new_zeroed(mb_row_count));
        }
        if matches!(
            blk.encoding,
            Encoding::Const | Encoding::Rle | Encoding::Plain | Encoding::Fsst | Encoding::Dict
        ) {
            let dict = if blk.encoding == Encoding::Dict {
                get_dict(cidx)
            } else {
                None
            };
            if blk.encoding != Encoding::Dict
                || (dict.is_some() && (dict_bm.is_some() || sq.needle.is_empty()))
            {
                let raw = if blk.encoding == Encoding::Const || blk.stored_len == 0 {
                    Cow::Borrowed(&[][..])
                } else {
                    read_block.read_block(blk)?
                };
                let bm = eval_utf8_block_substring_raw(
                    &raw,
                    blk,
                    mb_row_count,
                    active_mask,
                    &sq.needle,
                    sq.case_insensitive,
                    dict_bm,
                    dict.as_deref(),
                )?;
                return Ok(bm.and(active_mask)?);
            }
        }
    }
    ensure_p1_col_with_meta(
        meta,
        cidx,
        rg_idx,
        mb_idx,
        mb_row_count,
        get_dict,
        read_block,
        decoded_p1,
    )?;
    let item = &decoded_p1[&cidx];
    let bm = match item {
        DecodedPhase1::Col(col) => {
            eval_utf8_substring(col, &sq.needle, sq.case_insensitive, dict_bm)?
        }
        DecodedPhase1::FlatPayloadRefs(_)
        | DecodedPhase1::SelectedPayloadRefs(_)
        | DecodedPhase1::ShreddedSub(_) => {
            return Err(StatamError::Corrupt {
                reason: "unexpected Phase 1 variant for substring query",
            });
        }
    };
    Ok(bm.and(active_mask)?)
}

#[allow(clippy::too_many_arguments)]
fn eval_phase1_mb_narrow_impl(
    meta: &StatamFileMeta,
    rg_idx: usize,
    mb_idx: usize,
    plan: &RgIoPlan,
    time_col_idx: Option<usize>,
    time_range: TimeRange,
    lookups: &[(usize, &IdLookup)],
    preds: &[(usize, &Predicate)],
    filter_exprs: &[ResolvedFilterExpr],
    subs: &[Vec<(usize, SubstringQuery)>],
    dict_sub_bitmaps: &[Vec<Option<Bitmap>>],
    get_dict: &dyn Fn(usize) -> Option<Arc<FileDict>>,
    read_block: &BlockCacheReader,
    preload_always_matched_col: Option<usize>,
) -> Result<(Bitmap, HashMap<usize, DecodedPhase1>), StatamError> {
    let fields = meta.schema_meta.schema.fields();
    let mb_row_count = meta.rg_blocks(0, rg_idx)[mb_idx].row_count as usize;
    let mut mask = Bitmap::new_ones(mb_row_count);
    let mut decoded_p1: HashMap<usize, DecodedPhase1> = HashMap::new();

    if let Some(tc) = time_col_idx {
        let blk = &meta.rg_blocks(tc, rg_idx)[mb_idx];
        let time_pred = ZonePred::Between(
            ZoneValue::TimestampNs(time_range.min_ns),
            ZoneValue::TimestampNs(time_range.max_ns),
        );
        if !zone_map_always_matches(blk, &fields[tc].data_type, &time_pred) {
            ensure_p1_col_with_meta(
                meta,
                tc,
                rg_idx,
                mb_idx,
                mb_row_count,
                get_dict,
                read_block,
                &mut decoded_p1,
            )?;
            let DecodedPhase1::Col(col) = &decoded_p1[&tc] else {
                return Err(StatamError::Corrupt {
                    reason: "unexpected Phase 1 decoded type for time column",
                });
            };
            let bm =
                eval_column_pred(col, &time_pred, (blk.block_flags & BLOCK_FLAG_HAS_NAN) != 0)?;
            mask = mask.and(&bm)?;
            if mask.all_zeros() {
                return Ok((mask, decoded_p1));
            }
        }
    }

    for &(cidx, lk) in lookups {
        ensure_p1_col_with_meta(
            meta,
            cidx,
            rg_idx,
            mb_idx,
            mb_row_count,
            get_dict,
            read_block,
            &mut decoded_p1,
        )?;
        let DecodedPhase1::Col(col) = &decoded_p1[&cidx] else {
            return Err(match &decoded_p1[&cidx] {
                DecodedPhase1::FlatPayloadRefs(_) | DecodedPhase1::SelectedPayloadRefs(_) => {
                    StatamError::SchemaMismatch {
                        reason: "id_lookup not supported on ExternalPayload column",
                    }
                }
                _ => StatamError::Corrupt {
                    reason: "unexpected Phase 1 decoded type for id_lookup",
                },
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
            let bm = if let Some((sub_pidx, res_count)) =
                find_shredded_subcol_in_meta(meta, cidx, key)
            {
                let fast_no_res =
                    res_count == 0 && plan.phase1_map_fast_path.contains(&(mb_idx, cidx));
                if fast_no_res {
                    ensure_p1_col_with_meta(
                        meta,
                        sub_pidx,
                        rg_idx,
                        mb_idx,
                        mb_row_count,
                        get_dict,
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
                    ensure_p1_col_with_meta(
                        meta,
                        sub_pidx,
                        rg_idx,
                        mb_idx,
                        mb_row_count,
                        get_dict,
                        read_block,
                        &mut decoded_p1,
                    )?;
                    ensure_p1_col_with_meta(
                        meta,
                        cidx,
                        rg_idx,
                        mb_idx,
                        mb_row_count,
                        get_dict,
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
                ensure_p1_col_with_meta(
                    meta,
                    cidx,
                    rg_idx,
                    mb_idx,
                    mb_row_count,
                    get_dict,
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
            let blk = &meta.rg_blocks(cidx, rg_idx)[mb_idx];
            if !zone_map_always_matches(blk, &fields[cidx].data_type, &pred.pred) {
                ensure_p1_col_with_meta(
                    meta,
                    cidx,
                    rg_idx,
                    mb_idx,
                    mb_row_count,
                    get_dict,
                    read_block,
                    &mut decoded_p1,
                )?;
                let bm = match &decoded_p1[&cidx] {
                    DecodedPhase1::Col(col) => eval_column_pred(
                        col,
                        &pred.pred,
                        (blk.block_flags & BLOCK_FLAG_HAS_NAN) != 0,
                    )?,
                    DecodedPhase1::FlatPayloadRefs(refs) => {
                        eval_payload_refs_pred(refs, &pred.pred)?
                    }
                    _ => {
                        return Err(StatamError::Corrupt {
                            reason: "expected scalar/nested Column or FlatPayloadRefs in Phase 1",
                        });
                    }
                };
                mask = mask.and(&bm)?;
                if mask.all_zeros() {
                    return Ok((mask, decoded_p1));
                }
            } else if preload_always_matched_col == Some(cidx) {
                ensure_p1_col_with_meta(
                    meta,
                    cidx,
                    rg_idx,
                    mb_idx,
                    mb_row_count,
                    get_dict,
                    read_block,
                    &mut decoded_p1,
                )?;
            }
        }
    }

    for expr in filter_exprs {
        let (is_true, _) = eval_filter_expr_3vl(
            meta,
            rg_idx,
            mb_idx,
            mb_row_count,
            plan,
            expr,
            get_dict,
            read_block,
            &mut decoded_p1,
        )?;
        mask = mask.and(&is_true)?;
        if mask.all_zeros() {
            return Ok((mask, decoded_p1));
        }
    }

    // Evaluate pure-narrow CNF substring clauses BEFORE ExternalPayload clauses so
    // `mask` is as narrow as possible before touching `.stpk` or L0 payload chunks.
    for (ci, clause) in subs.iter().enumerate() {
        let clause_has_payload = clause
            .iter()
            .any(|(cidx, _)| meta.col_dir[*cidx].locality_group == LocalityGroup::ExternalPayload);
        if clause_has_payload {
            continue;
        }
        if clause.is_empty() {
            return Ok((Bitmap::new_zeroed(mb_row_count), decoded_p1));
        }
        let mut clause_matched = Bitmap::new_zeroed(mb_row_count);
        let mut remaining = mask.clone();
        for (di, (cidx, sq)) in clause.iter().enumerate() {
            if remaining.all_zeros() {
                break;
            }
            let dict_bm = dict_sub_bitmaps
                .get(ci)
                .and_then(|v| v.get(di))
                .and_then(Option::as_ref);
            let hits = eval_narrow_sub_disjunct(
                meta,
                rg_idx,
                mb_idx,
                mb_row_count,
                *cidx,
                sq,
                dict_bm,
                &remaining,
                get_dict,
                read_block,
                &mut decoded_p1,
            )?;
            if !hits.all_zeros() {
                clause_matched = clause_matched.or(&hits)?;
                remaining = remaining.and(&hits.not())?;
            }
        }
        mask = mask.and(&clause_matched)?;
        if mask.all_zeros() {
            return Ok((mask, decoded_p1));
        }
    }

    Ok((mask, decoded_p1))
}

/// Evaluates a [`ResolvedFilterExpr`] in exact SQL 3-valued logic, returning
/// `(is_true, is_valid)` where `is_true` has 1s for TRUE rows and `is_valid` has 1s
/// for known (TRUE or FALSE) rows (`is_true <= is_valid`).
#[allow(clippy::too_many_arguments)]
fn eval_filter_expr_3vl(
    meta: &StatamFileMeta,
    rg_idx: usize,
    mb_idx: usize,
    mb_row_count: usize,
    plan: &RgIoPlan,
    expr: &ResolvedFilterExpr,
    get_dict: &dyn Fn(usize) -> Option<Arc<FileDict>>,
    read_block: &BlockCacheReader,
    decoded_p1: &mut HashMap<usize, DecodedPhase1>,
) -> Result<(Bitmap, Bitmap), StatamError> {
    let fields = meta.schema_meta.schema.fields();
    match expr {
        ResolvedFilterExpr::Pred { cidx, pred } => {
            if let Some(key) = &pred.map_key {
                if let Some((sub_pidx, res_count)) = find_shredded_subcol_in_meta(meta, *cidx, key)
                {
                    let fast_no_res =
                        res_count == 0 && plan.phase1_map_fast_path.contains(&(mb_idx, *cidx));
                    if fast_no_res {
                        ensure_p1_col_with_meta(
                            meta,
                            sub_pidx,
                            rg_idx,
                            mb_idx,
                            mb_row_count,
                            get_dict,
                            read_block,
                            decoded_p1,
                        )?;
                        let DecodedPhase1::ShreddedSub(sub) = &decoded_p1[&sub_pidx] else {
                            return Err(StatamError::Corrupt {
                                reason: "expected ShreddedSub in Phase 1",
                            });
                        };
                        eval_shredded_subcol_pred_3vl(sub, &pred.pred)
                    } else {
                        ensure_p1_col_with_meta(
                            meta,
                            sub_pidx,
                            rg_idx,
                            mb_idx,
                            mb_row_count,
                            get_dict,
                            read_block,
                            decoded_p1,
                        )?;
                        ensure_p1_col_with_meta(
                            meta,
                            *cidx,
                            rg_idx,
                            mb_idx,
                            mb_row_count,
                            get_dict,
                            read_block,
                            decoded_p1,
                        )?;
                        let DecodedPhase1::ShreddedSub(sub) = &decoded_p1[&sub_pidx] else {
                            return Err(StatamError::Corrupt {
                                reason: "expected ShreddedSub in Phase 1",
                            });
                        };
                        let DecodedPhase1::Col(res_col) = &decoded_p1[cidx] else {
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
                        eval_map_key_pred_3vl(&single_map, key, &pred.pred)
                    }
                } else {
                    ensure_p1_col_with_meta(
                        meta,
                        *cidx,
                        rg_idx,
                        mb_idx,
                        mb_row_count,
                        get_dict,
                        read_block,
                        decoded_p1,
                    )?;
                    let DecodedPhase1::Col(col) = &decoded_p1[cidx] else {
                        return Err(StatamError::Corrupt {
                            reason: "expected MapUtf8 in Phase 1",
                        });
                    };
                    let flat = col.materialize(None)?;
                    eval_map_key_pred_3vl(&flat, key, &pred.pred)
                }
            } else {
                let blk = &meta.rg_blocks(*cidx, rg_idx)[mb_idx];
                let dt = &fields[*cidx].data_type;
                if zone_map_always_matches(blk, dt, &pred.pred) {
                    return Ok((
                        Bitmap::new_ones(mb_row_count),
                        Bitmap::new_ones(mb_row_count),
                    ));
                }
                let dict = if blk.encoding == Encoding::Dict {
                    get_dict(*cidx)
                } else {
                    None
                };
                if (blk.null_count == 0
                    || matches!(pred.pred, ZonePred::IsNull | ZonePred::IsNotNull))
                    && !zone_map_may_match(
                        blk,
                        dt,
                        meta.col_dir[*cidx].col_flags,
                        &pred.pred,
                        dict.as_deref(),
                    )
                {
                    return Ok((
                        Bitmap::new_zeroed(mb_row_count),
                        Bitmap::new_ones(mb_row_count),
                    ));
                }
                ensure_p1_col_with_meta(
                    meta,
                    *cidx,
                    rg_idx,
                    mb_idx,
                    mb_row_count,
                    get_dict,
                    read_block,
                    decoded_p1,
                )?;
                match &decoded_p1[cidx] {
                    DecodedPhase1::Col(col) => eval_column_pred_3vl(
                        col,
                        &pred.pred,
                        (blk.block_flags & BLOCK_FLAG_HAS_NAN) != 0,
                    ),
                    DecodedPhase1::FlatPayloadRefs(refs) => {
                        eval_payload_refs_pred_3vl(refs, &pred.pred)
                    }
                    _ => Err(StatamError::Corrupt {
                        reason: "expected scalar/nested Column or FlatPayloadRefs in Phase 1",
                    }),
                }
            }
        }
        ResolvedFilterExpr::Not(inner) => {
            let (t, v) = eval_filter_expr_3vl(
                meta,
                rg_idx,
                mb_idx,
                mb_row_count,
                plan,
                inner,
                get_dict,
                read_block,
                decoded_p1,
            )?;
            Ok((t.not().and(&v)?, v))
        }
        ResolvedFilterExpr::And(children) => {
            let mut acc_t = Bitmap::new_ones(mb_row_count);
            let mut acc_v = Bitmap::new_ones(mb_row_count);
            for child in children {
                let (ct, cv) = eval_filter_expr_3vl(
                    meta,
                    rg_idx,
                    mb_idx,
                    mb_row_count,
                    plan,
                    child,
                    get_dict,
                    read_block,
                    decoded_p1,
                )?;
                let next_t = acc_t.and(&ct)?;
                let known_both = acc_v.and(&cv)?;
                let known_false_acc = acc_t.not().and(&acc_v)?;
                let known_false_c = ct.not().and(&cv)?;
                let next_v = known_both.or(&known_false_acc)?.or(&known_false_c)?;
                acc_t = next_t;
                acc_v = next_v;
                // Early exit only when EVERY row is already known FALSE (`acc_t == 0 && acc_v == all_ones`).
                if acc_t.all_zeros() && acc_v.all_ones() {
                    break;
                }
            }
            Ok((acc_t, acc_v))
        }
        ResolvedFilterExpr::Or(children) => {
            let mut acc_t = Bitmap::new_zeroed(mb_row_count);
            let mut acc_v = Bitmap::new_ones(mb_row_count);
            for child in children {
                let (ct, cv) = eval_filter_expr_3vl(
                    meta,
                    rg_idx,
                    mb_idx,
                    mb_row_count,
                    plan,
                    child,
                    get_dict,
                    read_block,
                    decoded_p1,
                )?;
                let next_t = acc_t.or(&ct)?;
                let known_both = acc_v.and(&cv)?;
                let next_v = known_both.or(&acc_t)?.or(&ct)?;
                acc_t = next_t;
                acc_v = next_v;
                // Early exit when EVERY row is already known TRUE (`acc_t == all_ones`).
                if acc_t.all_ones() {
                    break;
                }
            }
            Ok((acc_t, acc_v))
        }
    }
}

fn eval_payload_refs_pred_3vl(
    refs: &FlatPayloadRefs,
    pred: &ZonePred,
) -> Result<(Bitmap, Bitmap), StatamError> {
    let is_true = eval_payload_refs_pred(refs, pred)?;
    let is_valid = Bitmap::new_ones(refs.len());
    Ok((is_true, is_valid))
}

fn eval_column_pred_3vl(
    col: &Column,
    pred: &ZonePred,
    has_nan: bool,
) -> Result<(Bitmap, Bitmap), StatamError> {
    let n = col.len();
    match pred {
        ZonePred::IsNull | ZonePred::IsNotNull => {
            let is_true = eval_column_pred(col, pred, has_nan)?;
            Ok((is_true, Bitmap::new_ones(n)))
        }
        ZonePred::In(vs) if vs.is_empty() => Ok((Bitmap::new_zeroed(n), Bitmap::new_ones(n))),
        _ => {
            let is_true = eval_column_pred(col, pred, has_nan)?;
            let is_valid = eval_column_pred(col, &ZonePred::IsNotNull, false)?;
            Ok((is_true, is_valid))
        }
    }
}

fn eval_shredded_subcol_pred_3vl(
    sub: &ShreddedSubCol,
    pred: &ZonePred,
) -> Result<(Bitmap, Bitmap), StatamError> {
    let n = sub.in_map.len();
    match pred {
        ZonePred::IsNull | ZonePred::IsNotNull => {
            let is_true = eval_shredded_subcol_pred(sub, pred)?;
            Ok((is_true, Bitmap::new_ones(n)))
        }
        ZonePred::In(vs) if vs.is_empty() => Ok((Bitmap::new_zeroed(n), Bitmap::new_ones(n))),
        _ => {
            let is_true = eval_shredded_subcol_pred(sub, pred)?;
            let is_valid = sub.in_map.and(&sub.value_null)?;
            Ok((is_true, is_valid))
        }
    }
}

fn eval_map_key_pred_3vl(
    map_col: &FlatColumn,
    key: &str,
    pred: &ZonePred,
) -> Result<(Bitmap, Bitmap), StatamError> {
    let FlatData::MapUtf8(map) = map_col.data() else {
        return Err(StatamError::SchemaMismatch {
            reason: "eval_map_key_pred_3vl expects MapUtf8",
        });
    };
    let n = map_col.len();
    if matches!(pred, ZonePred::In(vs) if vs.is_empty()) {
        return Ok((Bitmap::new_zeroed(n), Bitmap::new_ones(n)));
    }
    let mut is_true = Bitmap::new_zeroed(n);
    let mut is_valid = if matches!(pred, ZonePred::IsNull | ZonePred::IsNotNull) {
        Bitmap::new_ones(n)
    } else {
        Bitmap::new_zeroed(n)
    };
    let offsets = map.offsets();
    let keys = map.keys();
    let vals = map.values();
    let val_nulls = map.value_nulls();
    let row_nulls = map_col.nulls();
    let key_bytes = key.as_bytes();

    for r in 0..n {
        if row_nulls.is_some_and(|bm| !bm.get(r)) {
            if matches!(pred, ZonePred::IsNull) {
                let _ = is_true.set(r, true);
            }
            continue;
        }
        let s = offsets[r] as usize;
        let e = offsets[r + 1] as usize;
        let mut found: Option<Option<&[u8]>> = None;
        for idx in s..e {
            if utf8_bytes_at(keys, idx) == Some(key_bytes) {
                if val_nulls.is_some_and(|bm| !bm.get(idx)) {
                    found = Some(None);
                } else {
                    found = Some(utf8_bytes_at(vals, idx));
                }
                break;
            }
        }
        match (found, pred) {
            (None | Some(None), ZonePred::IsNull) => {
                let _ = is_true.set(r, true);
            }
            (Some(Some(_)), ZonePred::IsNotNull) => {
                let _ = is_true.set(r, true);
            }
            (Some(Some(b)), _) => {
                let _ = is_valid.set(r, true);
                if eval_bytes_val(b, pred) {
                    let _ = is_true.set(r, true);
                }
            }
            _ => {}
        }
    }
    Ok((is_true, is_valid))
}

#[allow(clippy::too_many_arguments)]
fn ensure_p1_col_with_meta(
    meta: &StatamFileMeta,
    pidx: usize,
    rg_idx: usize,
    mb_idx: usize,
    mb_row_count: usize,
    get_dict: &dyn Fn(usize) -> Option<Arc<FileDict>>,
    read_block: &BlockCacheReader,
    decoded_p1: &mut HashMap<usize, DecodedPhase1>,
) -> Result<(), StatamError> {
    if decoded_p1.contains_key(&pidx) {
        return Ok(());
    }
    let fields = meta.schema_meta.schema.fields();
    let blk = &meta.rg_blocks(pidx, rg_idx)[mb_idx];
    let cd = &meta.col_dir[pidx];
    let item = if (cd.col_flags & COL_FLAG_IS_SHREDDED_SUBCOL) != 0 {
        let raw = read_block.read_block(blk)?;
        let sub = decode_shredded_subcol(&raw, mb_row_count)?;
        DecodedPhase1::ShreddedSub(sub)
    } else if cd.locality_group == LocalityGroup::ExternalPayload {
        if blk.encoding == Encoding::ConstNull {
            DecodedPhase1::FlatPayloadRefs(FlatPayloadRefs::all_null(mb_row_count))
        } else {
            let raw = read_block.read_block(blk)?;
            let refs = decode_payload_refs_flat(&raw, mb_row_count, MAX_BATCH_DECODED_BYTES)?;
            DecodedPhase1::FlatPayloadRefs(refs)
        }
    } else if meta
        .schema_meta
        .shredded_maps
        .iter()
        .any(|s| s.logical_col_idx as usize == pidx)
    {
        let raw = if blk.stored_len > 0 {
            read_block.read_block(blk)?
        } else {
            Cow::Borrowed(&[][..])
        };
        let flat = decode_shredded_residual_block(&raw, blk)?;
        DecodedPhase1::Col(Column::Flat(flat))
    } else {
        let raw = if blk.stored_len > 0 {
            read_block.read_block(blk)?
        } else {
            Cow::Borrowed(&[][..])
        };
        let dict = if blk.encoding == Encoding::Dict {
            get_dict(pidx)
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

pub(super) fn is_always_false_pred(pred: &ZonePred) -> bool {
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

pub(super) fn is_supported_id_lookup_type(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Utf8
            | DataType::Binary
            | DataType::FixedBytes8
            | DataType::FixedBytes16
            | DataType::I64
            | DataType::TimestampNs { .. }
            | DataType::Date
            | DataType::Decimal128 { .. }
    )
}

pub(super) fn id_lookup_zone_pred(dt: &DataType, values: &[Vec<u8>]) -> Option<ZonePred> {
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
            DataType::Date if b.len() == 4 => {
                Some(ZoneValue::Date(i32::from_le_bytes(b.try_into().ok()?)))
            }
            DataType::Decimal128 { .. } if b.len() == 16 => Some(ZoneValue::Decimal128(
                i128::from_le_bytes(b.try_into().ok()?),
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

fn eval_payload_refs_pred(refs: &FlatPayloadRefs, pred: &ZonePred) -> Result<Bitmap, StatamError> {
    let n = refs.len();
    match pred {
        ZonePred::IsNull => {
            let mut bm = Bitmap::new_zeroed(n);
            for r in 0..n {
                if refs.row_kind(r) == FLAT_REF_NULL {
                    let _ = bm.set(r, true);
                }
            }
            Ok(bm)
        }
        ZonePred::IsNotNull => {
            let mut bm = Bitmap::new_zeroed(n);
            for r in 0..n {
                if refs.row_kind(r) != FLAT_REF_NULL {
                    let _ = bm.set(r, true);
                }
            }
            Ok(bm)
        }
        _ => Err(StatamError::SchemaMismatch {
            reason: "value predicate not supported on ExternalPayload column",
        }),
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
        Column::Dict(d) => eval_dict_pred(d, pred, has_nan),
        Column::Flat(f) => eval_flat_pred(f, pred, has_nan),
    }
}

pub(super) fn eval_dict_pred(
    d: &cetra_column::DictColumn,
    pred: &ZonePred,
    has_nan: bool,
) -> Result<Bitmap, StatamError> {
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
            let val_mask = eval_flat_pred(d.values(), pred, has_nan)?;
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
        FlatData::Utf8(u) => eval_bytes_rows(n, |r| utf8_bytes_at(u, r), pred),
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
    let key_bytes = key.as_bytes();

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
            if utf8_bytes_at(keys, idx) == Some(key_bytes) {
                if val_nulls.is_some_and(|bm| !bm.get(idx)) {
                    found = Some(None);
                } else {
                    found = Some(utf8_bytes_at(vals, idx));
                }
                break;
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

pub(super) fn filter_residual_to_keys(
    map_col: &FlatColumn,
    keys_keep: &[String],
) -> Result<FlatColumn, StatamError> {
    let FlatData::MapUtf8(map) = map_col.data() else {
        return Err(StatamError::SchemaMismatch {
            reason: "filter_residual_to_keys expects MapUtf8",
        });
    };
    if map.entry_count() == 0 {
        return Ok(map_col.clone());
    }
    let n = map_col.len();
    let mut out_offsets = Vec::with_capacity(n + 1);
    out_offsets.push(0u32);
    let mut out_keys = Vec::new();
    let mut out_vals = Vec::new();
    let mut out_val_valid = Vec::new();

    let offsets = map.offsets();
    let mkeys = FastUtf8Ref::new(map.keys())?;
    let mvals = FastUtf8Ref::new(map.values())?;
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

/// Evaluates a `Utf8` mini-block substring filter directly over raw encoded block bytes.
///
/// Bits outside `active_mask` are unspecified; caller must intersect with `active_mask`.
#[allow(clippy::too_many_arguments)]
pub(super) fn eval_utf8_block_substring_raw(
    raw: &[u8],
    blk: &BlockEntry,
    mb_row_count: usize,
    active_mask: &Bitmap,
    needle: &[u8],
    case_insensitive: bool,
    dict_sub_bitmap: Option<&Bitmap>,
    file_dict: Option<&FileDict>,
) -> Result<Bitmap, StatamError> {
    if active_mask.len() != mb_row_count {
        return Err(StatamError::Corrupt {
            reason: "active_mask length mismatch in raw substring eval",
        });
    }
    if mb_row_count == 0 {
        if !matches!(blk.encoding, Encoding::ConstNull | Encoding::Const) && !raw.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty narrow block",
            });
        }
        return Ok(Bitmap::new_zeroed(0));
    }
    if active_mask.all_zeros() {
        return Ok(Bitmap::new_zeroed(mb_row_count));
    }

    match blk.encoding {
        Encoding::ConstNull => Ok(Bitmap::new_zeroed(mb_row_count)),
        Encoding::Const => {
            if blk.null_count != 0 {
                return Err(StatamError::Corrupt {
                    reason: "Const block cannot have non-zero null_count",
                });
            }
            if (blk.block_flags & BLOCK_FLAG_CONST_EMPTY_COLLECTION) != 0 {
                return Err(StatamError::Corrupt {
                    reason: "CONST_EMPTY_COLLECTION set on non-collection DataType",
                });
            }
            let inline_len = usize::from(blk.min_inline_len);
            if inline_len > 16 {
                return Err(StatamError::Corrupt {
                    reason: "invalid Const Utf8 inline length",
                });
            }
            let bytes = &blk.min_inline[..inline_len];
            let s = std::str::from_utf8(bytes).map_err(|_| StatamError::Corrupt {
                reason: "invalid UTF-8 in Const Utf8 inline",
            })?;
            let hit = needle.is_empty()
                || Utf8Matcher::new(needle, case_insensitive).matches(s.as_bytes());
            Ok(if hit {
                Bitmap::new_ones(mb_row_count)
            } else {
                Bitmap::new_zeroed(mb_row_count)
            })
        }
        Encoding::Plain => {
            let (nulls, body) = read_optional_nulls(raw, mb_row_count, blk.block_flags)?;
            let (off_raw, data) = parse_plain_varlen_slices(body, mb_row_count)?;
            if needle.is_empty() {
                return Ok(nulls.unwrap_or_else(|| Bitmap::new_ones(mb_row_count)));
            }
            let mut matcher = Utf8Matcher::new(needle, case_insensitive);
            let mut out = Bitmap::new_zeroed(mb_row_count);
            for row in active_mask.iter_ones() {
                if row >= mb_row_count {
                    break;
                }
                if nulls.as_ref().is_none_or(|bm| bm.get(row)) {
                    let start = read_u32_le_at(off_raw, row);
                    let end = read_u32_le_at(off_raw, row + 1);
                    if matcher.matches(&data[start..end]) {
                        let _ = out.set(row, true);
                    }
                }
            }
            Ok(out)
        }
        Encoding::Fsst => {
            let (nulls, body) = read_optional_nulls(raw, mb_row_count, blk.block_flags)?;
            if (blk.block_flags & BLOCK_FLAG_FSST_SUBCHUNKED) != 0 {
                let view = FsstSubchunkedView::parse(body, mb_row_count)?;
                if needle.is_empty() {
                    return Ok(nulls.unwrap_or_else(|| Bitmap::new_ones(mb_row_count)));
                }
                let mut matcher = Utf8Matcher::new(needle, case_insensitive);
                return view
                    .eval_substring(active_mask, nulls.as_ref(), |slice| matcher.matches(slice));
            }
            let view = FsstBlockView::parse(body, mb_row_count)?;
            if needle.is_empty() {
                return Ok(nulls.unwrap_or_else(|| Bitmap::new_ones(mb_row_count)));
            }
            let mut matcher = Utf8Matcher::new(needle, case_insensitive);
            let mut scratch = Vec::new();
            let mut out = Bitmap::new_zeroed(mb_row_count);
            for row in active_mask.iter_ones() {
                if row >= mb_row_count {
                    break;
                }
                if nulls.as_ref().is_none_or(|bm| bm.get(row)) {
                    let slice = view.decode_row_into(row, &mut scratch)?;
                    if matcher.matches(slice) {
                        let _ = out.set(row, true);
                    }
                }
            }
            Ok(out)
        }
        Encoding::Rle => {
            if raw.len() < 4 {
                return Err(StatamError::Corrupt {
                    reason: "truncated Rle num_runs",
                });
            }
            let num_runs = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
            if num_runs == 0 || num_runs > mb_row_count {
                return Err(StatamError::Corrupt {
                    reason: "invalid Rle num_runs",
                });
            }
            if num_runs
                .checked_mul(4)
                .is_none_or(|b| b > MAX_BLOCK_RAW_BYTES)
            {
                return Err(StatamError::BoundsExceeded {
                    reason: "Rle num_runs exceeds MAX_BLOCK_RAW_BYTES",
                });
            }
            let (run_nulls, after_nulls) =
                read_optional_nulls(&raw[4..], num_runs, blk.block_flags)?;
            let Some(&re_bw) = after_nulls.first() else {
                return Err(StatamError::Corrupt {
                    reason: "truncated Rle run_ends bit_width",
                });
            };
            if re_bw > 32 {
                return Err(StatamError::Corrupt {
                    reason: "invalid Rle run_ends bit_width",
                });
            }
            let re_bytes = packed_bytes_len(num_runs, re_bw)?;
            if after_nulls.len() < 1 + re_bytes + 1 {
                return Err(StatamError::Corrupt {
                    reason: "truncated Rle body",
                });
            }
            let mut run_ends = Vec::with_capacity(num_runs);
            unpack_u32(
                &after_nulls[1..1 + re_bytes],
                num_runs,
                re_bw,
                &mut run_ends,
            )?;
            let mut prev = 0u32;
            for &end in &run_ends {
                if end <= prev || (end as usize) > mb_row_count {
                    return Err(StatamError::Corrupt {
                        reason: "invalid Rle run_ends monotonicity",
                    });
                }
                prev = end;
            }
            if prev as usize != mb_row_count {
                return Err(StatamError::Corrupt {
                    reason: "Rle run_ends do not sum to row_count",
                });
            }

            let inner_enc = Encoding::from_u8(after_nulls[1 + re_bytes])?;
            let inner_body = &after_nulls[1 + re_bytes + 1..];
            let mut out_words = vec![0u64; mb_row_count.div_ceil(64)];
            let active_words = active_mask.words();
            let mut matcher = Utf8Matcher::new(needle, case_insensitive);

            match inner_enc {
                Encoding::Plain => {
                    let (off_raw, data) = parse_plain_varlen_slices(inner_body, num_runs)?;
                    let mut run_start = 0usize;
                    for (run_idx, &end_u32) in run_ends.iter().enumerate() {
                        let run_end = end_u32 as usize;
                        if run_nulls.as_ref().is_none_or(|bm| bm.get(run_idx))
                            && range_has_any_one(active_words, run_start, run_end)
                        {
                            let start = read_u32_le_at(off_raw, run_idx);
                            let end = read_u32_le_at(off_raw, run_idx + 1);
                            if needle.is_empty() || matcher.matches(&data[start..end]) {
                                set_bit_range(&mut out_words, run_start, run_end);
                            }
                        }
                        run_start = run_end;
                    }
                }
                Encoding::Fsst => {
                    let view = FsstBlockView::parse(inner_body, num_runs)?;
                    let mut scratch = Vec::new();
                    let mut run_start = 0usize;
                    for (run_idx, &end_u32) in run_ends.iter().enumerate() {
                        let run_end = end_u32 as usize;
                        if run_nulls.as_ref().is_none_or(|bm| bm.get(run_idx))
                            && range_has_any_one(active_words, run_start, run_end)
                        {
                            let hit = if needle.is_empty() {
                                true
                            } else {
                                let slice = view.decode_row_into(run_idx, &mut scratch)?;
                                matcher.matches(slice)
                            };
                            if hit {
                                set_bit_range(&mut out_words, run_start, run_end);
                            }
                        }
                        run_start = run_end;
                    }
                }
                _ => {
                    return Err(StatamError::Corrupt {
                        reason: "incompatible Rle inner encoding for Utf8",
                    });
                }
            }
            Ok(Bitmap::from_words(out_words, mb_row_count)?)
        }
        Encoding::Dict => {
            let Some(dict) = file_dict else {
                return Err(StatamError::Corrupt {
                    reason: "Dict mini-block requires FileDict",
                });
            };
            if dict.values().data_type() != DataType::Utf8 {
                return Err(StatamError::SchemaMismatch {
                    reason: "FileDict DataType mismatch",
                });
            }
            let (nulls, body) = read_optional_nulls(raw, mb_row_count, blk.block_flags)?;
            let codes = decode_dict_codes_body(body, mb_row_count, dict.len(), nulls.as_ref())?;
            if needle.is_empty() {
                return Ok(nulls.unwrap_or_else(|| Bitmap::new_ones(mb_row_count)));
            }
            let Some(code_bm) = dict_sub_bitmap else {
                return Err(StatamError::Corrupt {
                    reason: "missing dict_sub_bitmap for Dict substring",
                });
            };
            if code_bm.len() != dict.len() {
                return Err(StatamError::Corrupt {
                    reason: "dict_sub_bitmap length mismatch",
                });
            }
            if code_bm.all_zeros() {
                return Ok(Bitmap::new_zeroed(mb_row_count));
            }
            if code_bm.all_ones() {
                return Ok(nulls.unwrap_or_else(|| Bitmap::new_ones(mb_row_count)));
            }
            let mut out = Bitmap::new_zeroed(mb_row_count);
            for row in active_mask.iter_ones() {
                if row >= mb_row_count {
                    break;
                }
                if nulls.as_ref().is_none_or(|bm| bm.get(row)) && code_bm.get(codes[row] as usize) {
                    let _ = out.set(row, true);
                }
            }
            Ok(out)
        }
        _ => Err(StatamError::Corrupt {
            reason: "unsupported encoding for raw Utf8 substring eval",
        }),
    }
}

#[inline]
fn read_u32_le_at(raw: &[u8], idx: usize) -> usize {
    let off = idx * 4;
    u32::from_le_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]) as usize
}

fn parse_plain_varlen_slices(body: &[u8], row_count: usize) -> Result<(&[u8], &[u8]), StatamError> {
    let offsets_count = row_count
        .checked_add(1)
        .ok_or(StatamError::BoundsExceeded {
            reason: "plain varlen offsets count overflow",
        })?;
    let offsets_bytes = offsets_count
        .checked_mul(4)
        .ok_or(StatamError::BoundsExceeded {
            reason: "plain varlen offsets size overflow",
        })?;
    if offsets_bytes > MAX_BLOCK_RAW_BYTES {
        return Err(StatamError::BoundsExceeded {
            reason: "plain varlen offsets exceed MAX_BLOCK_RAW_BYTES",
        });
    }
    if body.len() < offsets_bytes {
        return Err(StatamError::Corrupt {
            reason: "truncated plain varlen offsets",
        });
    }
    let data_len = body.len() - offsets_bytes;
    if data_len > MAX_BLOCK_RAW_BYTES {
        return Err(StatamError::BoundsExceeded {
            reason: "plain varlen data exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    let off_raw = &body[..offsets_bytes];
    let mut prev = 0usize;
    for (idx, &chunk) in off_raw.as_chunks::<4>().0.iter().enumerate() {
        let v = u32::from_le_bytes(chunk) as usize;
        if (idx == 0 && v != 0) || v < prev || v > data_len {
            return Err(StatamError::Corrupt {
                reason: "invalid plain varlen offsets",
            });
        }
        prev = v;
    }
    if prev != data_len {
        return Err(StatamError::Corrupt {
            reason: "plain varlen final offset mismatch",
        });
    }
    Ok((off_raw, &body[offsets_bytes..]))
}

#[inline]
fn range_has_any_one(words: &[u64], start: usize, end: usize) -> bool {
    if start >= end {
        return false;
    }
    let first_w = start >> 6;
    let last_w = (end - 1) >> 6;
    let start_mask = !0u64 << (start & 63);
    let end_rem = end & 63;
    let end_mask = if end_rem == 0 {
        !0u64
    } else {
        (1u64 << end_rem) - 1
    };
    if first_w == last_w {
        return (words[first_w] & start_mask & end_mask) != 0;
    }
    if (words[first_w] & start_mask) != 0 {
        return true;
    }
    for &w in &words[first_w + 1..last_w] {
        if w != 0 {
            return true;
        }
    }
    (words[last_w] & end_mask) != 0
}

#[inline]
fn set_bit_range(words: &mut [u64], start: usize, end: usize) {
    if start >= end {
        return;
    }
    let first_w = start >> 6;
    let last_w = (end - 1) >> 6;
    let start_mask = !0u64 << (start & 63);
    let end_rem = end & 63;
    let end_mask = if end_rem == 0 {
        !0u64
    } else {
        (1u64 << end_rem) - 1
    };
    if first_w == last_w {
        words[first_w] |= start_mask & end_mask;
        return;
    }
    words[first_w] |= start_mask;
    for w in &mut words[first_w + 1..last_w] {
        *w = !0u64;
    }
    words[last_w] |= end_mask;
}

pub(super) fn eval_utf8_substring(
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
                        if let Some(b) = utf8_bytes_at(u, i)
                            && matcher.matches(b)
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
                        && let Some(b) = utf8_bytes_at(u, i)
                        && matcher.matches(b)
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
                        && let Some(b) = utf8_bytes_at(u, r)
                        && matcher.matches(b)
                    {
                        let _ = out.set(r, true);
                    }
                }
            }
            Ok(out)
        }
    }
}

pub(super) fn trim_bitmap_mask(mask: &mut Bitmap, keep: usize, trim_tail: bool) {
    let mut trimmed = Bitmap::new_zeroed(mask.len());
    if trim_tail {
        let skip = mask.count_ones().saturating_sub(keep);
        for idx in mask.iter_ones().skip(skip) {
            let _ = trimmed.set(idx, true);
        }
    } else {
        for idx in mask.iter_ones().take(keep) {
            let _ = trimmed.set(idx, true);
        }
    }
    *mask = trimmed;
}

pub(super) enum Utf8Matcher {
    Exact(memmem::Finder<'static>),
    AsciiIlike {
        finder: memmem::Finder<'static>,
        has_ks: bool,
        scratch: Vec<u8>,
    },
    UnicodeIlike {
        finder: memmem::Finder<'static>,
        ascii_anchor: Option<Box<memmem::Finder<'static>>>,
        needle_has_non_ascii: bool,
        needle_raw_lower: Vec<u8>,
        scratch: Vec<u8>,
        uni_scratch: Vec<u8>,
    },
}

impl Utf8Matcher {
    pub(super) fn new(needle: &[u8], case_insensitive: bool) -> Self {
        if !case_insensitive {
            return Self::Exact(memmem::Finder::new(needle).into_owned());
        }
        if needle.is_ascii() {
            let mut lowered = Vec::new();
            ascii_lowercase_into(needle, &mut lowered);
            let has_ks = lowered.iter().any(|&b| b == b'k' || b == b's');
            return Self::AsciiIlike {
                finder: memmem::Finder::new(&lowered).into_owned(),
                has_ks,
                scratch: Vec::new(),
            };
        }
        let mut ascii_lowered = Vec::new();
        ascii_lowercase_into(needle, &mut ascii_lowered);
        match std::str::from_utf8(needle) {
            Ok(s) => {
                let mut uni_lower = Vec::new();
                unicode_lowercase_context_free_into_bytes(s, &mut uni_lower);
                let needle_has_non_ascii = !uni_lower.is_ascii();
                let ascii_anchor = extract_unicode_ilike_ascii_anchor(&uni_lower, &ascii_lowered)
                    .map(|anchor| Box::new(memmem::Finder::new(&anchor).into_owned()));
                Self::UnicodeIlike {
                    finder: memmem::Finder::new(&uni_lower).into_owned(),
                    ascii_anchor,
                    needle_has_non_ascii,
                    needle_raw_lower: ascii_lowered,
                    scratch: Vec::new(),
                    uni_scratch: Vec::new(),
                }
            }
            Err(_) => Self::AsciiIlike {
                finder: memmem::Finder::new(&ascii_lowered).into_owned(),
                has_ks: false,
                scratch: Vec::new(),
            },
        }
    }

    pub(super) fn matches(&mut self, haystack: &[u8]) -> bool {
        match self {
            Self::Exact(finder) => finder.find(haystack).is_some(),
            Self::AsciiIlike {
                finder,
                has_ks,
                scratch,
            } => {
                if *has_ks && contains_kelvin_or_long_s(haystack) {
                    ascii_lowercase_fold_ks_into(haystack, scratch);
                } else {
                    ascii_lowercase_into(haystack, scratch);
                }
                finder.find(scratch).is_some()
            }
            Self::UnicodeIlike {
                finder,
                ascii_anchor,
                needle_has_non_ascii,
                needle_raw_lower,
                scratch,
                uni_scratch,
            } => {
                if *needle_has_non_ascii && haystack.is_ascii() {
                    return false;
                }
                if let Some(anchor) = ascii_anchor {
                    ascii_lowercase_into(haystack, scratch);
                    if anchor.find(scratch).is_none() {
                        return false;
                    }
                }
                if let Ok(s) = std::str::from_utf8(haystack) {
                    unicode_lowercase_context_free_into_bytes(s, uni_scratch);
                    finder.find(uni_scratch).is_some()
                } else {
                    let lossy = String::from_utf8_lossy(haystack);
                    unicode_lowercase_context_free_into_bytes(&lossy, uni_scratch);
                    if finder.find(uni_scratch).is_some() {
                        return true;
                    }
                    if ascii_anchor.is_none() {
                        ascii_lowercase_into(haystack, scratch);
                    }
                    memmem::find(scratch, needle_raw_lower).is_some()
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Utf8Matcher;

    #[test]
    fn pass14a_utf8_matcher_unicode_ilike_special_foldings_and_ascii_fast_reject() {
        let mut kelvin = Utf8Matcher::new("\u{212A}".as_bytes(), true);
        assert!(kelvin.matches(b"k"));
        assert!(kelvin.matches(b"K"));
        assert!(kelvin.matches("300 \u{212A}".as_bytes()));
        assert!(!kelvin.matches(b"hello world"));

        // Long s U+017F stays U+017F under `char::to_lowercase()` (non-ASCII), matching "ſong"
        // and rejecting ASCII "s"/"S" consistently with `unicode_lowercase_context_free_into`.
        let mut long_s = Utf8Matcher::new("\u{017F}".as_bytes(), true);
        assert!(long_s.matches("\u{017F}ong".as_bytes()));
        assert!(!long_s.matches(b"s"));
        assert!(!long_s.matches(b"S"));
        assert!(!long_s.matches(b"hello world"));

        // Turkish dotted I (U+0130 -> "i\u{0307}") matches decomposed lowercase and uppercase, rejects ASCII.
        let mut dotted_i = Utf8Matcher::new("\u{0130}".as_bytes(), true);
        assert!(dotted_i.matches("i\u{0307}stanbul".as_bytes()));
        assert!(dotted_i.matches("\u{0130}STANBUL".as_bytes()));
        assert!(!dotted_i.matches(b"istanbul"));

        let mut sigma = Utf8Matcher::new("ς".as_bytes(), true);
        assert!(sigma.matches("ΟΔΥΣΣΕΥΣ".as_bytes()));
        assert!(!sigma.matches(b"pure ascii text without greek"));

        let mut alien = Utf8Matcher::new("Alienación".as_bytes(), true);
        assert!(!alien.matches(b"Alienacion without accent"));
        assert!(!alien.matches("PURE ASCII ".repeat(20).as_bytes()));
        assert!(alien.matches("la ALIENACIÓN moderna".as_bytes()));
        assert!(alien.matches("alienación".as_bytes()));
    }

    #[test]
    fn pass15a_utf8_matcher_ascii_anchor_prefilter_and_invalid_utf8_invariants() {
        let mut alien = Utf8Matcher::new("Alienación".as_bytes(), true);
        assert!(alien.matches("ALIENACIÓN".as_bytes()));
        assert!(!alien.matches("日本語テキスト".as_bytes()));

        let mut cafe = Utf8Matcher::new("café".as_bytes(), true);
        assert!(cafe.matches("CAFÉ".as_bytes()));
        assert!(cafe.matches("un café noir".as_bytes()));
        assert!(!cafe.matches("日本語テキスト".as_bytes()));

        let mut istanbul = Utf8Matcher::new("İSTANBUL".as_bytes(), true);
        assert!(istanbul.matches("i\u{0307}stanbul".as_bytes()));
        assert!(istanbul.matches("İSTANBUL".as_bytes()));
        assert!(!istanbul.matches(b"istanbul"));

        let mut istanbul_decomp = Utf8Matcher::new("i\u{0307}stanbul".as_bytes(), true);
        assert!(istanbul_decomp.matches("İSTANBUL".as_bytes()));

        let mut kelvin = Utf8Matcher::new("Kelvin".as_bytes(), true);
        assert!(kelvin.matches(b"kelvin"));
        assert!(kelvin.matches(b"KELVIN"));
        assert!(kelvin.matches("KELVIN".as_bytes()));
        assert!(!kelvin.matches(b"celsius"));

        let mut sigma = Utf8Matcher::new("ς".as_bytes(), true);
        assert!(sigma.matches("ΟΔΥΣΣΕΥΣ".as_bytes()));
        assert!(!sigma.matches("한국어".as_bytes()));

        let mut trailing_i = Utf8Matcher::new("abci\u{0307}".as_bytes(), true);
        assert!(trailing_i.matches("xxABC\u{0130}yy".as_bytes()));

        // Invalid UTF-8 slices must have zero false negatives on both lossy & raw-ASCII fallbacks.
        let mut invalid_alien = vec![0xFF, 0xFE];
        invalid_alien.extend_from_slice("ALIENACIÓN".as_bytes());
        invalid_alien.push(0xC3);
        assert!(std::str::from_utf8(&invalid_alien).is_err());
        assert!(alien.matches(&invalid_alien));

        let mut invalid_cafe = vec![0x80];
        invalid_cafe.extend_from_slice("CAFÉ".as_bytes());
        invalid_cafe.push(0xFF);
        assert!(cafe.matches(&invalid_cafe));
    }

    #[test]
    fn pass16c_utf8_matcher_invalid_utf8_scratch_reuse_does_not_poison() {
        // UnicodeIlike WITH ascii_anchor: valid match -> invalid non-match hitting anchor ->
        // valid non-match -> valid match.
        let mut alien = Utf8Matcher::new("Alienación".as_bytes(), true);
        assert!(alien.matches("prefix ALIENACIÓN suffix".as_bytes()));
        let invalid_non_match = b"\xFF\xFEalienac\x80\xC3no-accent";
        assert!(!alien.matches(invalid_non_match));
        assert!(!alien.matches("alienacion without accent".as_bytes()));
        assert!(alien.matches("un ALIENACIÓN nuevo".as_bytes()));

        // UnicodeIlike WITHOUT ascii_anchor: valid match -> invalid non-match ->
        // invalid match -> valid non-match -> valid match.
        let mut sigma = Utf8Matcher::new("ς".as_bytes(), true);
        assert!(sigma.matches("ΟΔΥΣΣΕΥΣ".as_bytes()));
        let invalid_no_sigma = b"\xFF\xFEhello\x80world\xC3";
        assert!(!sigma.matches(invalid_no_sigma));
        let mut invalid_with_sigma = vec![0xFF, 0xFE];
        invalid_with_sigma.extend_from_slice("ΟΔΥΣΣΕΥΣ".as_bytes());
        invalid_with_sigma.push(0x80);
        assert!(sigma.matches(&invalid_with_sigma));
        assert!(!sigma.matches("αβγδε".as_bytes()));
        assert!(sigma.matches("οδυσσευς".as_bytes()));

        // AsciiIlike: valid match -> invalid UTF-8 non-match -> invalid UTF-8 match -> valid non-match.
        let mut ascii = Utf8Matcher::new(b"Needle", true);
        assert!(ascii.matches(b"haystack NEEDLE here"));
        assert!(!ascii.matches(b"\xFF\xFEnot_here\x80"));
        assert!(ascii.matches(b"\xFF\xFEhas_NeEdLe\x80"));
        assert!(!ascii.matches(b"clean ascii without match"));
        assert!(ascii.matches(b"needle"));
    }
}
