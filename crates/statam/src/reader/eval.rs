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
use crate::codec::numeric::{PlainSubchunkedView, read_optional_nulls};
use crate::codec::{
    FastUtf8Ref, ZonePred, ZoneValue, decode_mini_block, utf8_bytes_at, zone_map_always_matches,
    zone_map_may_match,
};
use crate::format::{
    BLOCK_FLAG_CONST_EMPTY_COLLECTION, BLOCK_FLAG_FSST_SUBCHUNKED, BLOCK_FLAG_HAS_NAN,
    BLOCK_FLAG_PLAIN_SUBCHUNKED, BlockEntry, COL_FLAG_HAS_FILE_DICT, COL_FLAG_IS_SHREDDED_SUBCOL,
    Encoding, LocalityGroup, MAX_BATCH_DECODED_BYTES, MAX_BLOCK_RAW_BYTES,
};
use crate::parallel::{MAX_PARALLEL_THREADS, parallel_map_in_order_static};
use crate::payload::search::{
    RetainedPackChunks, extract_ilike_trigram_hashes, extract_unicode_ilike_ascii_anchor,
    pack_short_circuits_all, unicode_lowercase_context_free_into_bytes,
};
use crate::payload::{
    FLAT_REF_NULL, FlatPayloadRefs, decode_payload_refs_flat, scan_payload_refs_inline_substring,
};
use crate::simd::bitpack::{packed_bytes_len, unpack_u32};
use crate::simd::filter::{FilterOp, filter_f64, filter_i64, filter_i64_between, filter_u32};
use crate::simd::trigram::{
    ascii_lowercase_fold_ks_into, ascii_lowercase_into, contains_kelvin_or_long_s,
    extract_trigram_hashes,
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

            // 2b. ExternalPayload disjuncts grouped by `(needle, case_insensitive)` ONLY on rows
            // still unmatched in `remaining`, so all grouped columns share one `.stpk` pass.
            let payload_groups = group_payload_or_disjuncts(&self.meta, clause);
            for group in payload_groups {
                if cancel.is_some_and(CancelToken::is_cancelled) {
                    return Err(StatamError::Cancelled);
                }
                if remaining.iter().all(Bitmap::all_zeros) {
                    break;
                }

                let needs_fast_path_meta = group.col_indices.iter().any(|&cidx| {
                    let rg_blocks = self.meta.rg_blocks(cidx, rg_idx);
                    active
                        .iter()
                        .enumerate()
                        .any(|(pos, (mb_idx, _, decoded_p1))| {
                            !remaining[pos].all_zeros()
                                && !decoded_p1.contains_key(&cidx)
                                && rg_blocks[*mb_idx].encoding != Encoding::ConstNull
                        })
                });
                let fast_meta = if needs_fast_path_meta {
                    self.compute_inline_fast_path_meta(subs, ci, &group, l0_local)?
                } else {
                    InlineFastPathMeta::default()
                };

                let mut inline_tasks: Vec<(usize, usize, usize, usize, BlockEntry, Bitmap)> =
                    Vec::new();
                let mut batch_pairs: Vec<(usize, usize)> = Vec::new();
                let mut missing: Vec<(usize, usize, usize, BlockEntry)> = Vec::new();

                for &cidx in &group.col_indices {
                    let col_inline_ok =
                        fast_meta.col_inline_ok.get(&cidx).copied().unwrap_or(false);
                    let rg_blocks = self.meta.rg_blocks(cidx, rg_idx);
                    for (pos, (mb_idx, _, decoded_p1)) in active.iter().enumerate() {
                        if remaining[pos].all_zeros() {
                            continue;
                        }
                        let blk = &rg_blocks[*mb_idx];
                        if blk.encoding == Encoding::ConstNull {
                            // Disjunct produces 0 hits for this MB (do NOT zero other OR branches).
                            continue;
                        }
                        let mb_row_count =
                            self.meta.rg_blocks(0, rg_idx)[*mb_idx].row_count as usize;
                        if !decoded_p1.contains_key(&cidx) && col_inline_ok {
                            inline_tasks.push((
                                pos,
                                cidx,
                                *mb_idx,
                                mb_row_count,
                                *blk,
                                remaining[pos].clone(),
                            ));
                        } else {
                            if !decoded_p1.contains_key(&cidx) {
                                missing.push((pos, cidx, mb_row_count, *blk));
                            }
                            batch_pairs.push((pos, cidx));
                        }
                    }
                }

                if !inline_tasks.is_empty() {
                    let inline_results = self.eval_inline_fast_path_blocks(
                        rg_idx,
                        group.needle,
                        group.case_insensitive,
                        &fast_meta.pack_chunk_counts,
                        &fast_meta.pack_has_edges,
                        read_block,
                        threads,
                        inline_tasks,
                        cancel,
                    )?;
                    for (pos, bm) in inline_results {
                        let hits = bm.and(&remaining[pos])?;
                        if !hits.all_zeros() {
                            clause_matched[pos] = clause_matched[pos].or(&hits)?;
                            remaining[pos] = remaining[pos].and(&hits.not())?;
                        }
                    }
                }

                if !batch_pairs.is_empty() {
                    missing.retain(|&(pos, _, _, _)| !remaining[pos].all_zeros());
                    if !missing.is_empty() {
                        if threads > 1 && missing.len() >= 2 {
                            let block_reader = read_block.clone();
                            let decoded_missing = parallel_map_in_order_static(
                                missing,
                                threads,
                                move |(pos, cidx, mb_row_count, blk)| {
                                    let raw = block_reader.read_block(&blk)?;
                                    let refs = decode_payload_refs_flat(
                                        &raw,
                                        mb_row_count,
                                        MAX_BATCH_DECODED_BYTES,
                                    )?;
                                    Ok((pos, cidx, refs))
                                },
                            )?;
                            for (pos, cidx, refs) in decoded_missing {
                                active[pos]
                                    .2
                                    .insert(cidx, DecodedPhase1::FlatPayloadRefs(refs));
                            }
                        } else {
                            for (pos, cidx, mb_row_count, _) in missing {
                                if cancel.is_some_and(CancelToken::is_cancelled) {
                                    return Err(StatamError::Cancelled);
                                }
                                let mb_idx = active[pos].0;
                                self.ensure_p1_col(
                                    cidx,
                                    rg_idx,
                                    mb_idx,
                                    mb_row_count,
                                    read_block,
                                    &mut active[pos].2,
                                )?;
                            }
                        }
                    }

                    let mut entries: Vec<usize> = Vec::with_capacity(batch_pairs.len());
                    let mut mb_inputs: Vec<(&FlatPayloadRefs, &Bitmap)> =
                        Vec::with_capacity(batch_pairs.len());
                    for (pos, cidx) in batch_pairs {
                        if remaining[pos].all_zeros() {
                            continue;
                        }
                        let (_, _, decoded_p1) = &active[pos];
                        let DecodedPhase1::FlatPayloadRefs(refs) = &decoded_p1[&cidx] else {
                            return Err(StatamError::Corrupt {
                                reason: "unexpected Phase 1 variant for ExternalPayload substring query",
                            });
                        };
                        entries.push(pos);
                        mb_inputs.push((refs, &remaining[pos]));
                    }
                    if !mb_inputs.is_empty() {
                        let any_projected = group
                            .col_indices
                            .iter()
                            .any(|cidx| proj_indices.contains(cidx));
                        let batch_bms = self.eval_payload_refs_substring_batch(
                            &mb_inputs,
                            group.needle,
                            group.case_insensitive,
                            l0_local,
                            threads,
                            any_projected.then_some(retained),
                            cancel,
                        )?;
                        for (pos, bm) in entries.into_iter().zip(batch_bms) {
                            let hits = bm.and(&remaining[pos])?;
                            if !hits.all_zeros() {
                                clause_matched[pos] = clause_matched[pos].or(&hits)?;
                                remaining[pos] = remaining[pos].and(&hits.not())?;
                            }
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

            // ExternalPayload disjuncts grouped by `(needle, case_insensitive)` ONLY on rows
            // still in `remaining`.
            let payload_groups = group_payload_or_disjuncts(&self.meta, clause);
            for group in payload_groups {
                if remaining.all_zeros() {
                    break;
                }
                if cancel.is_some_and(CancelToken::is_cancelled) {
                    return Err(StatamError::Cancelled);
                }

                let needs_fast_path_meta = !remaining.all_zeros()
                    && group.col_indices.iter().any(|&cidx| {
                        let blk = &self.meta.rg_blocks(cidx, rg_idx)[mb_idx];
                        !decoded_p1.contains_key(&cidx) && blk.encoding != Encoding::ConstNull
                    });
                let fast_meta = if needs_fast_path_meta {
                    self.compute_inline_fast_path_meta(subs, ci, &group, l0_local)?
                } else {
                    InlineFastPathMeta::default()
                };

                let mut inline_tasks: Vec<(usize, usize, usize, usize, BlockEntry, Bitmap)> =
                    Vec::new();
                let mut batch_cols: Vec<usize> = Vec::new();
                let mut missing: Vec<(usize, BlockEntry)> = Vec::new();

                for &cidx in &group.col_indices {
                    let blk = &self.meta.rg_blocks(cidx, rg_idx)[mb_idx];
                    if blk.encoding == Encoding::ConstNull {
                        continue;
                    }
                    let col_inline_ok =
                        fast_meta.col_inline_ok.get(&cidx).copied().unwrap_or(false);
                    if !decoded_p1.contains_key(&cidx) && col_inline_ok {
                        inline_tasks.push((0, cidx, mb_idx, mb_row_count, *blk, remaining.clone()));
                    } else {
                        if !decoded_p1.contains_key(&cidx) {
                            missing.push((cidx, *blk));
                        }
                        batch_cols.push(cidx);
                    }
                }

                if !inline_tasks.is_empty() {
                    let inline_results = self.eval_inline_fast_path_blocks(
                        rg_idx,
                        group.needle,
                        group.case_insensitive,
                        &fast_meta.pack_chunk_counts,
                        &fast_meta.pack_has_edges,
                        read_block,
                        read_threads,
                        inline_tasks,
                        cancel,
                    )?;
                    for (_, bm) in inline_results {
                        let hits = bm.and(&remaining)?;
                        if !hits.all_zeros() {
                            clause_matched = clause_matched.or(&hits)?;
                            remaining = remaining.and(&hits.not())?;
                        }
                    }
                }

                if !remaining.all_zeros() && !batch_cols.is_empty() {
                    if !missing.is_empty() {
                        if read_threads > 1 && missing.len() >= 2 {
                            let block_reader = read_block.clone();
                            let decoded_missing = parallel_map_in_order_static(
                                missing,
                                read_threads,
                                move |(cidx, blk)| {
                                    let raw = block_reader.read_block(&blk)?;
                                    let refs = decode_payload_refs_flat(
                                        &raw,
                                        mb_row_count,
                                        MAX_BATCH_DECODED_BYTES,
                                    )?;
                                    Ok((cidx, refs))
                                },
                            )?;
                            for (cidx, refs) in decoded_missing {
                                decoded_p1.insert(cidx, DecodedPhase1::FlatPayloadRefs(refs));
                            }
                        } else {
                            for (cidx, _) in missing {
                                if cancel.is_some_and(CancelToken::is_cancelled) {
                                    return Err(StatamError::Cancelled);
                                }
                                self.ensure_p1_col(
                                    cidx,
                                    rg_idx,
                                    mb_idx,
                                    mb_row_count,
                                    read_block,
                                    &mut decoded_p1,
                                )?;
                            }
                        }
                    }

                    let mut mb_inputs: Vec<(&FlatPayloadRefs, &Bitmap)> =
                        Vec::with_capacity(batch_cols.len());
                    for &cidx in &batch_cols {
                        let DecodedPhase1::FlatPayloadRefs(refs) = &decoded_p1[&cidx] else {
                            return Err(StatamError::Corrupt {
                                reason: "unexpected Phase 1 variant for substring query",
                            });
                        };
                        mb_inputs.push((refs, &remaining));
                    }
                    let any_projected = group
                        .col_indices
                        .iter()
                        .any(|cidx| proj_indices.contains(cidx));
                    let batch_bms = self.eval_payload_refs_substring_batch(
                        &mb_inputs,
                        group.needle,
                        group.case_insensitive,
                        l0_local,
                        read_threads,
                        any_projected.then_some(retained),
                        cancel,
                    )?;
                    for bm in batch_bms {
                        let hits = bm.and(&remaining)?;
                        if !hits.all_zeros() {
                            clause_matched = clause_matched.or(&hits)?;
                            remaining = remaining.and(&hits.not())?;
                        }
                    }
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

    fn compute_inline_fast_path_meta(
        &self,
        subs: &[Vec<(usize, SubstringQuery)>],
        ci: usize,
        group: &PayloadOrGroup<'_>,
        l0_local: Option<&ParsedL0Payload>,
    ) -> Result<InlineFastPathMeta, StatamError> {
        let mut pack_chunk_counts = Vec::new();
        let mut pack_has_edges = Vec::new();
        let mut resolved_packs = Vec::new();
        let mut all_short_circuit = false;

        if l0_local.is_none() && !self.meta.schema_meta.pack_ids.is_empty() {
            all_short_circuit = true;
            pack_chunk_counts.reserve(self.meta.schema_meta.pack_ids.len());
            pack_has_edges.reserve(self.meta.schema_meta.pack_ids.len());
            resolved_packs.reserve(self.meta.schema_meta.pack_ids.len());
            for &pack_id in &self.meta.schema_meta.pack_ids {
                let pack = match self.resolve_pack(pack_id) {
                    Ok(p) => p,
                    Err(StatamError::MissingPack { .. }) => {
                        all_short_circuit = false;
                        break;
                    }
                    Err(e) => return Err(e),
                };
                if !pack_short_circuits_all(&pack, group.needle, group.case_insensitive)? {
                    all_short_circuit = false;
                    break;
                }
                pack_chunk_counts.push(pack.toc().chunks.len());
                pack_has_edges.push(pack.postscript().edge_table_len > 0);
                resolved_packs.push(pack);
            }
        }

        let mut col_inline_ok = HashMap::with_capacity(group.col_indices.len());
        if all_short_circuit {
            for &cidx in &group.col_indices {
                let mut needed_by_other = false;
                'other: for (oci, oclause) in subs.iter().enumerate() {
                    for (oc, osq) in oclause {
                        if (oci != ci
                            || (osq.needle.as_slice(), osq.case_insensitive)
                                != (group.needle, group.case_insensitive))
                            && *oc == cidx
                        {
                            for pack in &resolved_packs {
                                if !pack_short_circuits_all(
                                    pack,
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
                col_inline_ok.insert(cidx, !needed_by_other);
            }
        }

        Ok(InlineFastPathMeta {
            col_inline_ok,
            pack_chunk_counts,
            pack_has_edges,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_inline_fast_path_blocks(
        &self,
        rg_idx: usize,
        needle: &[u8],
        case_insensitive: bool,
        pack_chunk_counts: &[usize],
        pack_has_edges: &[bool],
        read_block: &BlockCacheReader,
        threads: usize,
        tasks: Vec<(usize, usize, usize, usize, BlockEntry, Bitmap)>,
        cancel: Option<&CancelToken>,
    ) -> Result<Vec<(usize, Bitmap)>, StatamError> {
        if tasks.is_empty() {
            return Ok(Vec::new());
        }
        let tri_hashes = needle_trigram_hashes(needle, case_insensitive);
        let surviving: Vec<(usize, usize, BlockEntry, Bitmap)> = if !tri_hashes.is_empty() {
            let caches = self.caches.lock().unwrap_or_else(|p| p.into_inner());
            tasks
                .into_iter()
                .filter_map(|(pos, cidx, mb_idx, mb_row_count, blk, mask)| {
                    let pruned = caches
                        .narrow_trigrams
                        .get(&cidx)
                        .and_then(|tri_vec| {
                            let global_mb_idx = self.global_mb_idx(cidx, rg_idx, mb_idx);
                            tri_vec.get(global_mb_idx).and_then(Option::as_ref)
                        })
                        .is_some_and(|bloom| !bloom.may_contain_hashes(&tri_hashes));
                    (!pruned).then_some((pos, mb_row_count, blk, mask))
                })
                .collect()
        } else {
            tasks
                .into_iter()
                .map(|(pos, _, _, mb_row_count, blk, mask)| (pos, mb_row_count, blk, mask))
                .collect()
        };

        if surviving.is_empty() {
            return Ok(Vec::new());
        }

        if threads > 1 && surviving.len() >= 2 {
            let needle_arc: Arc<[u8]> = Arc::from(needle);
            let counts_arc = Arc::new(pack_chunk_counts.to_vec());
            let edges_arc = Arc::new(pack_has_edges.to_vec());
            let block_reader = read_block.clone();
            let cancel_opt = cancel.cloned();
            parallel_map_in_order_static(
                surviving,
                threads,
                move |(pos, mb_row_count, blk, mask)| {
                    if cancel_opt.as_ref().is_some_and(CancelToken::is_cancelled) {
                        return Err(StatamError::Cancelled);
                    }
                    let mut matcher = Utf8Matcher::new(&needle_arc, case_insensitive);
                    let raw = block_reader.read_block(&blk)?;
                    let bm = scan_payload_refs_inline_substring(
                        &raw,
                        mb_row_count,
                        &mask,
                        &counts_arc,
                        &edges_arc,
                        |bytes| matcher.matches(bytes),
                    )?;
                    Ok((pos, bm))
                },
            )
        } else {
            let mut matcher = Utf8Matcher::new(needle, case_insensitive);
            let mut out = Vec::with_capacity(surviving.len());
            for (pos, mb_row_count, blk, mask) in surviving {
                if cancel.is_some_and(CancelToken::is_cancelled) {
                    return Err(StatamError::Cancelled);
                }
                let raw = read_block.read_block(&blk)?;
                let bm = scan_payload_refs_inline_substring(
                    &raw,
                    mb_row_count,
                    &mask,
                    pack_chunk_counts,
                    pack_has_edges,
                    |bytes| matcher.matches(bytes),
                )?;
                out.push((pos, bm));
            }
            Ok(out)
        }
    }
}

struct PayloadOrGroup<'a> {
    needle: &'a [u8],
    case_insensitive: bool,
    col_indices: Vec<usize>,
}

#[derive(Default)]
struct InlineFastPathMeta {
    col_inline_ok: HashMap<usize, bool>,
    pack_chunk_counts: Vec<usize>,
    pack_has_edges: Vec<bool>,
}

fn group_payload_or_disjuncts<'a>(
    meta: &StatamFileMeta,
    clause: &'a [(usize, SubstringQuery)],
) -> Vec<PayloadOrGroup<'a>> {
    let mut groups: Vec<PayloadOrGroup<'a>> = Vec::new();
    for (cidx, sq) in clause {
        if meta.col_dir[*cidx].locality_group != LocalityGroup::ExternalPayload {
            continue;
        }
        if let Some(g) = groups
            .iter_mut()
            .find(|g| g.needle == sq.needle.as_slice() && g.case_insensitive == sq.case_insensitive)
        {
            if !g.col_indices.contains(cidx) {
                g.col_indices.push(*cidx);
            }
        } else {
            groups.push(PayloadOrGroup {
                needle: sq.needle.as_slice(),
                case_insensitive: sq.case_insensitive,
                col_indices: vec![*cidx],
            });
        }
    }
    groups
}

fn needle_trigram_hashes(needle: &[u8], case_insensitive: bool) -> Vec<u32> {
    if case_insensitive {
        extract_ilike_trigram_hashes(needle)
    } else {
        let mut hashes = Vec::new();
        extract_trigram_hashes(needle, false, &mut hashes);
        hashes
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
            if (blk.block_flags & BLOCK_FLAG_PLAIN_SUBCHUNKED) != 0 {
                let view = PlainSubchunkedView::parse(body, mb_row_count)?;
                if needle.is_empty() {
                    return Ok(nulls.unwrap_or_else(|| Bitmap::new_ones(mb_row_count)));
                }
                let mut matcher = Utf8Matcher::new(needle, case_insensitive);
                return view
                    .eval_substring(active_mask, nulls.as_ref(), |slice| matcher.matches(slice));
            }
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

    fn collect_i64_ids(batches: &[cetra_column::Batch]) -> Vec<i64> {
        let mut out = Vec::new();
        for b in batches {
            let col = b.columns()[0].materialize(None).unwrap();
            let cetra_column::FlatData::I64(vals) = col.data() else {
                panic!("expected I64 id column");
            };
            match b.selection() {
                cetra_column::Selection::All => out.extend_from_slice(vals),
                cetra_column::Selection::Mask(bm) => {
                    for r in bm.iter_ones() {
                        out.push(vals[r]);
                    }
                }
                cetra_column::Selection::Indices(idxs) => {
                    for &r in idxs {
                        out.push(vals[r as usize]);
                    }
                }
            }
        }
        out
    }

    fn collect_utf8_col(batches: &[cetra_column::Batch], col_idx: usize) -> Vec<Option<String>> {
        let mut out = Vec::new();
        for b in batches {
            let col = b.columns()[col_idx].materialize(None).unwrap();
            let cetra_column::FlatData::Utf8(u) = col.data() else {
                panic!("expected Utf8 column");
            };
            let push_row = |r: usize, dst: &mut Vec<Option<String>>| {
                if col.is_null(r) {
                    dst.push(None);
                } else {
                    dst.push(
                        crate::codec::utf8_bytes_at(u, r)
                            .map(|bytes| String::from_utf8(bytes.to_vec()).unwrap()),
                    );
                }
            };
            match b.selection() {
                cetra_column::Selection::All => {
                    for r in 0..b.num_rows() {
                        push_row(r, &mut out);
                    }
                }
                cetra_column::Selection::Mask(bm) => {
                    for r in bm.iter_ones() {
                        push_row(r, &mut out);
                    }
                }
                cetra_column::Selection::Indices(idxs) => {
                    for &r in idxs {
                        push_row(r as usize, &mut out);
                    }
                }
            }
        }
        out
    }

    fn opt_utf8_col(vals: &[Option<String>]) -> cetra_column::Column {
        let mut bm = cetra_column::Bitmap::new_zeroed(vals.len());
        let refs: Vec<&str> = vals
            .iter()
            .enumerate()
            .map(|(i, v)| match v {
                Some(s) => {
                    let _ = bm.set(i, true);
                    s.as_str()
                }
                None => "",
            })
            .collect();
        cetra_column::Column::Flat(
            cetra_column::FlatColumn::try_new(
                cetra_column::FlatData::Utf8(
                    cetra_column::Utf8Data::try_from_slices(refs).unwrap(),
                ),
                Some(bm),
            )
            .unwrap(),
        )
    }

    fn make_opt_test_pack_opts() -> crate::payload::pack::PackWriteOptions {
        crate::payload::pack::PackWriteOptions {
            #[cfg(miri)]
            codec: crate::format::Codec::Lz4,
            #[cfg(not(miri))]
            codec: crate::format::Codec::Zstd,
            zstd_level: 1,
            target_frame_raw_bytes: 16 * 1024,
            enable_trigram_skip: true,
            compress_toc_lz4: true,
            write_threads: 1,
            ..crate::payload::pack::PackWriteOptions::default()
        }
    }

    struct CountingPackResolver {
        inner: crate::reader::InMemoryPackResolver,
        gets: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        bytes_map: std::collections::HashMap<[u8; 16], bytes::Bytes>,
    }

    impl crate::reader::PackResolver for CountingPackResolver {
        fn resolve(
            &self,
            pack_id: [u8; 16],
        ) -> Result<std::sync::Arc<crate::payload::pack::ChunkPackReader>, crate::StatamError>
        {
            struct CountingReader {
                data: bytes::Bytes,
                gets: std::sync::Arc<std::sync::atomic::AtomicUsize>,
            }
            impl crate::io::RangeReader for CountingReader {
                fn file_len(&self) -> u64 {
                    self.data.len() as u64
                }
                fn is_remote(&self) -> bool {
                    true
                }
                fn read_range(
                    &self,
                    range: std::ops::Range<u64>,
                ) -> Result<bytes::Bytes, crate::StatamError> {
                    self.gets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok(self.data.slice(range.start as usize..range.end as usize))
                }
                fn read_ranges(
                    &self,
                    ranges: &[std::ops::Range<u64>],
                ) -> Result<Vec<bytes::Bytes>, crate::StatamError> {
                    self.gets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok(ranges
                        .iter()
                        .map(|r| self.data.slice(r.start as usize..r.end as usize))
                        .collect())
                }
            }
            if let Some(bytes) = self.bytes_map.get(&pack_id) {
                let rr = std::sync::Arc::new(CountingReader {
                    data: bytes.clone(),
                    gets: std::sync::Arc::clone(&self.gets),
                });
                Ok(std::sync::Arc::new(
                    crate::payload::pack::ChunkPackReader::open(rr)?,
                ))
            } else {
                self.inner.resolve(pack_id)
            }
        }
    }

    #[test]
    fn opt4_multicol_payload_or_dedup_parity() {
        use std::io::Cursor;
        use std::sync::Arc;

        let schema = cetra_column::Schema::try_new_arc(vec![
            cetra_column::Field::new("id", cetra_column::DataType::I64, false),
            cetra_column::Field::new("name", cetra_column::DataType::Utf8, false),
            cetra_column::Field::new("input", cetra_column::DataType::Utf8, true),
            cetra_column::Field::new("output", cetra_column::DataType::Utf8, true),
            cetra_column::Field::new("system_instructions", cetra_column::DataType::Utf8, true),
        ])
        .unwrap();

        // Build a large string > MAX_CHUNK (8192) and extract a 12-byte slice straddling a chunk cut.
        let mut seam_buf = String::with_capacity(11_000);
        for i in 0..11_000 {
            seam_buf.push((b'a' + ((i * 7 + (i >> 5)) % 26) as u8) as char);
        }
        let mut seam_cuts = Vec::new();
        crate::payload::chunker::chunk_payload(seam_buf.as_bytes(), &mut seam_cuts);
        assert!(seam_cuts.len() >= 2, "expected multi-chunk payload");
        let seam_off = seam_cuts[0].len();
        let seam_needle = seam_buf[seam_off - 6..seam_off + 6].to_owned();

        let n = 64usize;
        let ids: Vec<i64> = (0..n as i64).collect();
        let names: Vec<String> = (0..n).map(|i| format!("span_name_{i:03}")).collect();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut inputs: Vec<Option<String>> = Vec::with_capacity(n);
        let mut outputs: Vec<Option<String>> = Vec::with_capacity(n);
        let mut sys: Vec<Option<String>> = Vec::with_capacity(n);

        for r in 0..n {
            // MB 2 (rows 32..48): output is all-null (ConstNull).
            let inp = match r {
                2 => Some("inline hit OR_DEDUP_TOK in input".to_owned()),
                19 => Some(format!(
                    "{} OR_DEDUP_TOK {}",
                    "x".repeat(400),
                    "y".repeat(300)
                )),
                25 => Some(seam_buf.clone()),
                35 => Some(format!(
                    "{} SECONDARY_PACK_TOK {}",
                    "p".repeat(350),
                    "q".repeat(350)
                )),
                _ => Some(format!("plain_input_{r:03}_{}", "z".repeat(300))),
            };
            let out = if (32..48).contains(&r) {
                None
            } else {
                match r {
                    5 => Some("inline hit or_dedup_tok in output".to_owned()),
                    30 => Some(format!(
                        "{} OR_DEDUP_TOK {}",
                        "m".repeat(420),
                        "n".repeat(300)
                    )),
                    _ => Some(format!("plain_output_{r:03}_{}", "w".repeat(300))),
                }
            };
            let sy = match r {
                12 => Some("inline Or_DeDuP_ToK in sys".to_owned()),
                50 => Some(format!(
                    "{} OR_DEDUP_TOK {}",
                    "k".repeat(400),
                    "j".repeat(300)
                )),
                51 => Some(seam_buf.clone()),
                _ => Some(format!("plain_sys_{r:03}_{}", "v".repeat(300))),
            };
            inputs.push(inp);
            outputs.push(out);
            sys.push(sy);
        }

        let batch = cetra_column::Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                cetra_column::Column::Flat(
                    cetra_column::FlatColumn::try_new(cetra_column::FlatData::I64(ids), None)
                        .unwrap(),
                ),
                cetra_column::Column::Flat(
                    cetra_column::FlatColumn::try_new(
                        cetra_column::FlatData::Utf8(
                            cetra_column::Utf8Data::try_from_slices(name_refs).unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
                opt_utf8_col(&inputs),
                opt_utf8_col(&outputs),
                opt_utf8_col(&sys),
            ],
            n,
        )
        .unwrap();

        let pack_id = [0xA4u8; 16];
        let mut pb =
            crate::payload::pack::ChunkPackBuilder::new(pack_id, make_opt_test_pack_opts());
        let mut l1_opts = crate::writer::WriteOptions::for_table(
            crate::format::TableKind::Spans,
            crate::WriteProfile::L1Compact,
        );
        l1_opts.mini_block_size = 16;
        l1_opts.pack_id = Some(pack_id);
        let mut l1_bytes = Vec::new();
        let mut w1 = crate::writer::StatamWriter::new(
            Cursor::new(&mut l1_bytes),
            Arc::clone(&schema),
            l1_opts,
            Some(&mut pb),
        )
        .unwrap();
        w1.write_batch(&batch).unwrap();
        w1.finish().unwrap();
        let mut pack_bytes = Vec::new();
        pb.finish(&mut pack_bytes).unwrap();

        let mut base_resolver = crate::reader::InMemoryPackResolver::new();
        base_resolver
            .insert_bytes(pack_id, pack_bytes.clone())
            .unwrap();
        let mut bytes_map = std::collections::HashMap::new();
        bytes_map.insert(pack_id, bytes::Bytes::from(pack_bytes));
        let pack_gets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counting_resolver = Arc::new(CountingPackResolver {
            inner: base_resolver,
            gets: Arc::clone(&pack_gets),
            bytes_map,
        });

        let r_l1 = Arc::new(
            crate::reader::StatamReader::open(
                Arc::new(crate::io::SliceReader::new(l1_bytes.clone())),
                Some(Arc::clone(&counting_resolver) as Arc<dyn crate::reader::PackResolver>),
            )
            .unwrap(),
        );

        // 1. Multi-column OR with duplicate `input` disjunct + projection on `input` only (RetainedPackChunks).
        let or_clause = vec![
            crate::reader::SubstringQuery::ilike("input", "or_dedup_tok"),
            crate::reader::SubstringQuery::ilike("output", "or_dedup_tok"),
            crate::reader::SubstringQuery::ilike("input", "or_dedup_tok"),
            crate::reader::SubstringQuery::ilike("system_instructions", "or_dedup_tok"),
        ];
        pack_gets.store(0, std::sync::atomic::Ordering::Relaxed);
        for (limit, read_threads) in [(None, 1usize), (None, 2usize), (Some(10usize), 2usize)] {
            let req = crate::reader::ScanRequest {
                projection: Some(vec![0, 2]),
                substrings: vec![or_clause.clone()],
                limit,
                read_threads,
                ..crate::reader::ScanRequest::default()
            };
            let res = r_l1.scan(&req).unwrap();
            let got_ids = collect_i64_ids(&res);
            assert_eq!(
                got_ids,
                vec![2, 5, 12, 19, 30, 50],
                "limit={limit:?} read_threads={read_threads}"
            );
            assert!(pack_gets.load(std::sync::atomic::Ordering::Relaxed) > 0);
            let got_inputs = collect_utf8_col(&res, 1);
            for (idx, id) in got_ids.iter().enumerate() {
                assert_eq!(got_inputs[idx], inputs[*id as usize]);
            }
            // Streaming iterator parity (`eval_phase1_mb` path).
            if limit.is_none() {
                let mut iter = r_l1.scan_iter(req).unwrap();
                let mut iter_batches = Vec::new();
                while let Some(b) = iter.next_batch(None, None).unwrap() {
                    iter_batches.push(b);
                }
                assert_eq!(collect_i64_ids(&iter_batches), vec![2, 5, 12, 19, 30, 50]);
            }
        }

        // Verify Phase-1 `.stpk` dedup: 3-column OR scan issues <= 1-column `input` scan `.stpk` reads
        // on fresh readers (so ChunkPackReader frame caches start cold), using `projection: Some(vec![0])`
        // so Phase-2 `input` chunk materialization for rows matched by `output`/`system_instructions`
        // does not skew the Phase-1 dedup comparison.
        let r_or = crate::reader::StatamReader::open(
            Arc::new(crate::io::SliceReader::new(l1_bytes.clone())),
            Some(Arc::clone(&counting_resolver) as Arc<dyn crate::reader::PackResolver>),
        )
        .unwrap();
        pack_gets.store(0, std::sync::atomic::Ordering::Relaxed);
        let dedup_or_res = r_or
            .scan(&crate::reader::ScanRequest {
                projection: Some(vec![0]),
                substrings: vec![or_clause.clone()],
                read_threads: 1,
                ..crate::reader::ScanRequest::default()
            })
            .unwrap();
        assert_eq!(collect_i64_ids(&dedup_or_res), vec![2, 5, 12, 19, 30, 50]);
        let or_gets = pack_gets.load(std::sync::atomic::Ordering::Relaxed);

        let r_single = crate::reader::StatamReader::open(
            Arc::new(crate::io::SliceReader::new(l1_bytes)),
            Some(Arc::clone(&counting_resolver) as Arc<dyn crate::reader::PackResolver>),
        )
        .unwrap();
        pack_gets.store(0, std::sync::atomic::Ordering::Relaxed);
        let single_res = r_single
            .scan(&crate::reader::ScanRequest {
                projection: Some(vec![0]),
                substrings: vec![vec![crate::reader::SubstringQuery::ilike(
                    "input",
                    "or_dedup_tok",
                )]],
                read_threads: 1,
                ..crate::reader::ScanRequest::default()
            })
            .unwrap();
        assert_eq!(collect_i64_ids(&single_res), vec![2, 19]);
        let single_gets = pack_gets.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            or_gets > 0 && or_gets <= single_gets,
            "expected 3-col OR scan ({or_gets} .stpk gets) <= 1-col scan ({single_gets} .stpk gets)"
        );

        // 2. Boundary match across `.stpk` frame/chunk seam in `input` OR `system_instructions`.
        let seam_res = r_l1
            .scan(&crate::reader::ScanRequest {
                projection: Some(vec![0]),
                substrings: vec![vec![
                    crate::reader::SubstringQuery::like("input", seam_needle.as_str()),
                    crate::reader::SubstringQuery::like(
                        "system_instructions",
                        seam_needle.as_str(),
                    ),
                ]],
                read_threads: 2,
                ..crate::reader::ScanRequest::default()
            })
            .unwrap();
        assert_eq!(collect_i64_ids(&seam_res), vec![25, 51]);

        // 3. `decoded_p1` reuse when same column appears in same clause with two different needles
        //    (one inline-friendly or different needle, one requiring `.stpk`).
        for limit in [None, Some(10usize)] {
            let reuse_res = r_l1
                .scan(&crate::reader::ScanRequest {
                    projection: Some(vec![0]),
                    substrings: vec![vec![
                        crate::reader::SubstringQuery::ilike("input", "or_dedup_tok"),
                        crate::reader::SubstringQuery::ilike("output", "or_dedup_tok"),
                        crate::reader::SubstringQuery::like("input", "SECONDARY_PACK_TOK"),
                    ]],
                    limit,
                    read_threads: 2,
                    ..crate::reader::ScanRequest::default()
                })
                .unwrap();
            assert_eq!(collect_i64_ids(&reuse_res), vec![2, 5, 19, 30, 35]);
        }

        // 4. L0Fast (`L0Local`) multi-column OR dedup parity.
        let mut l0_opts = crate::writer::WriteOptions::for_table(
            crate::format::TableKind::Spans,
            crate::WriteProfile::L0Fast,
        );
        l0_opts.mini_block_size = 16;
        let mut l0_bytes = Vec::new();
        let mut w0 = crate::writer::StatamWriter::new(
            Cursor::new(&mut l0_bytes),
            Arc::clone(&schema),
            l0_opts,
            None,
        )
        .unwrap();
        w0.write_batch(&batch).unwrap();
        w0.finish().unwrap();
        let r_l0 = crate::reader::StatamReader::open(
            Arc::new(crate::io::SliceReader::new(l0_bytes)),
            None,
        )
        .unwrap();
        for limit in [None, Some(10usize)] {
            let l0_res = r_l0
                .scan(&crate::reader::ScanRequest {
                    projection: Some(vec![0, 2]),
                    substrings: vec![or_clause.clone()],
                    limit,
                    read_threads: 2,
                    ..crate::reader::ScanRequest::default()
                })
                .unwrap();
            assert_eq!(collect_i64_ids(&l0_res), vec![2, 5, 12, 19, 30, 50]);
        }
    }

    #[test]
    fn opt6_inline_narrow_trigram_soundness_and_zero_io() {
        use std::io::Cursor;
        use std::sync::Arc;

        let schema = cetra_column::Schema::try_new_arc(vec![
            cetra_column::Field::new("id", cetra_column::DataType::I64, false),
            cetra_column::Field::new("name", cetra_column::DataType::Utf8, false),
            cetra_column::Field::new("input", cetra_column::DataType::Utf8, true),
            cetra_column::Field::new("output", cetra_column::DataType::Utf8, true),
            cetra_column::Field::new("system_instructions", cetra_column::DataType::Utf8, true),
        ])
        .unwrap();

        // 4 mini-blocks of 160 rows each (640 rows total, ensuring raw_body_len >= 512 B for every
        // non-ConstNull PayloadRefs MB including MB 2 which has 1/3 nulls + external chunk refs).
        let mb_size = 160usize;
        let mb1_hit_in = mb_size + 1;
        let mb1_hit_name = mb_size + 6;
        let mb1_hit_out = mb_size + 16;
        let mb2_short = 2 * mb_size + 2;
        let mb2_gamma = 2 * mb_size + 3;
        let n = 4 * mb_size;
        let ids: Vec<i64> = (0..n as i64).collect();
        let mut names = Vec::with_capacity(n);
        let mut inputs: Vec<Option<String>> = Vec::with_capacity(n);
        let mut outputs: Vec<Option<String>> = Vec::with_capacity(n);
        let mut sys: Vec<Option<String>> = Vec::with_capacity(n);

        for r in 0..n {
            let mb = r / mb_size;
            names.push(if r == mb1_hit_name {
                "ONLY_IN_NAME_DELTA".to_owned()
            } else {
                format!("span_unique_{r:04}")
            });
            let inp = match mb {
                // MB 0: 100% external .stpk chunks -> zero inline rows -> all-zeros 256 B bloom.
                0 => {
                    if r == 10 {
                        Some(format!(
                            "{} CHUNKED_NEEDLE_ALPHA {}",
                            "a".repeat(350),
                            "b".repeat(350)
                        ))
                    } else {
                        Some(format!("{} filler_mb0_{r:03}", "c".repeat(350)))
                    }
                }
                // MB 1: mixed inline (<256 B) + external .stpk chunks.
                1 => {
                    if r == mb1_hit_in {
                        Some("inline_hit_beta".to_owned())
                    } else if r % 2 == 0 {
                        Some(format!("inline_mb1_row_{r:03}"))
                    } else {
                        Some(format!("{} ext_mb1_{r:03}", "d".repeat(350)))
                    }
                }
                // MB 2: only <3 B inline ("ab"), nulls, and .stpk chunks -> all-zeros 256 B bloom.
                2 => {
                    if r == mb2_short {
                        Some("ab".to_owned())
                    } else if r == mb2_gamma {
                        Some(format!(
                            "{} CHUNKED_NEEDLE_GAMMA {}",
                            "e".repeat(350),
                            "f".repeat(350)
                        ))
                    } else if r % 3 == 0 {
                        None
                    } else {
                        Some(format!("{} ext_mb2_{r:03}", "g".repeat(350)))
                    }
                }
                // MB 3: ConstNull.
                _ => None,
            };
            let out = if mb == 3 {
                None
            } else if r == mb1_hit_out {
                Some("inline_hit_beta_in_output".to_owned())
            } else {
                Some(format!("{} out_filler_{r:03}", "h".repeat(350)))
            };
            let sy = if mb == 3 {
                None
            } else {
                Some(format!("{} sys_filler_{r:03}", "k".repeat(350)))
            };
            inputs.push(inp);
            outputs.push(out);
            sys.push(sy);
        }
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();

        let batch = cetra_column::Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                cetra_column::Column::Flat(
                    cetra_column::FlatColumn::try_new(cetra_column::FlatData::I64(ids), None)
                        .unwrap(),
                ),
                cetra_column::Column::Flat(
                    cetra_column::FlatColumn::try_new(
                        cetra_column::FlatData::Utf8(
                            cetra_column::Utf8Data::try_from_slices(name_refs).unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
                opt_utf8_col(&inputs),
                opt_utf8_col(&outputs),
                opt_utf8_col(&sys),
            ],
            n,
        )
        .unwrap();

        let pack_id = [0xB6u8; 16];
        let mut pb =
            crate::payload::pack::ChunkPackBuilder::new(pack_id, make_opt_test_pack_opts());
        let mut opts = crate::writer::WriteOptions::for_table(
            crate::format::TableKind::Spans,
            crate::WriteProfile::L1Compact,
        );
        opts.mini_block_size = mb_size;
        opts.pack_id = Some(pack_id);
        let mut statam_bytes = Vec::new();
        let mut w = crate::writer::StatamWriter::new(
            Cursor::new(&mut statam_bytes),
            Arc::clone(&schema),
            opts,
            Some(&mut pb),
        )
        .unwrap();
        w.write_batch(&batch).unwrap();
        w.finish().unwrap();
        let mut pack_bytes = Vec::new();
        pb.finish(&mut pack_bytes).unwrap();

        let mut resolver = crate::reader::InMemoryPackResolver::new();
        resolver.insert_bytes(pack_id, pack_bytes).unwrap();
        let resolver = Arc::new(resolver);

        let no_cache_cfg = crate::io::SimulatedS3Config {
            cache_capacity_blocks: 0,
            ..crate::io::SimulatedS3Config::default()
        };
        let s3_statam = Arc::new(
            crate::io::SimulatedS3Reader::from_bytes(statam_bytes.clone(), no_cache_cfg)
                .with_remote_hint(true),
        );
        let reader = Arc::new(
            crate::reader::StatamReader::open(
                s3_statam.clone(),
                Some(Arc::clone(&resolver) as Arc<dyn crate::reader::PackResolver>),
            )
            .unwrap(),
        );

        assert_ne!(
            reader.meta().postscript.flags & crate::format::FLAG_HAS_TRIGRAM_INDEX,
            0
        );
        // Payload columns (input=2, output=3, system_instructions=4) must have
        // COL_FLAG_HAS_TRIGRAM_INDEX; col 1 (`name`) is Dict-encoded so uses dict_sub_bitmaps.
        for col_idx in [2usize, 3, 4] {
            assert_ne!(
                reader.meta().col_dir[col_idx].col_flags
                    & crate::format::COL_FLAG_HAS_TRIGRAM_INDEX,
                0,
                "column {col_idx} must have COL_FLAG_HAS_TRIGRAM_INDEX"
            );
        }

        // Warm lazy trigram indexes by running one miss first, then reset counters to measure Wave-1 I/O!
        let miss_req = crate::reader::ScanRequest {
            projection: Some(vec![0]),
            substrings: vec![vec![
                crate::reader::SubstringQuery::ilike("input", "nonexistent_needle_9x7z"),
                crate::reader::SubstringQuery::ilike("output", "nonexistent_needle_9x7z"),
                crate::reader::SubstringQuery::ilike(
                    "system_instructions",
                    "nonexistent_needle_9x7z",
                ),
            ]],
            read_threads: 2,
            ..crate::reader::ScanRequest::default()
        };
        assert!(reader.scan(&miss_req).unwrap().is_empty());

        s3_statam.reset_counters();
        let miss_res = reader.scan(&miss_req).unwrap();
        assert!(miss_res.is_empty());
        assert_eq!(
            s3_statam.counters().gets,
            0,
            "Wave-1 pruning must issue ZERO .statam PayloadRefs block reads on miss"
        );
        let mut iter = reader.scan_iter(miss_req.clone()).unwrap();
        assert!(iter.next_batch(None, None).unwrap().is_none());
        assert_eq!(
            s3_statam.counters().gets,
            0,
            "scan_iter Wave-1 pruning must issue ZERO .statam reads on miss"
        );

        // Cold local-reader (`is_remote() == false`) first-query assertion:
        // with warm lazy trigrams and a cold `.stpk` pack, Wave-1 pruning issues 0 `.statam`
        // `PayloadRefs` block GETs on the very first miss!
        let local_s3 = Arc::new(
            crate::io::SimulatedS3Reader::from_bytes(statam_bytes, no_cache_cfg)
                .with_remote_hint(false),
        );
        let local_reader = Arc::new(
            crate::reader::StatamReader::open(
                local_s3.clone(),
                Some(Arc::clone(&resolver) as Arc<dyn crate::reader::PackResolver>),
            )
            .unwrap(),
        );
        assert!(!local_reader.reader.is_remote());
        local_reader
            .ensure_lazy_indexes_loaded(&[], &[2, 3, 4], &[])
            .unwrap();
        local_s3.reset_counters();
        assert!(local_reader.scan(&miss_req).unwrap().is_empty());
        assert_eq!(
            local_s3.counters().gets,
            0,
            "cold local-reader first miss must issue ZERO .statam PayloadRefs block GETs"
        );

        // Chunked .stpk hits when inline bloom is all-zeros (MB 0 and MB 2) MUST NOT false-negative!
        for (needle, expected) in [
            ("CHUNKED_NEEDLE_ALPHA", vec![10i64]),
            ("CHUNKED_NEEDLE_GAMMA", vec![mb2_gamma as i64]),
        ] {
            let res = reader
                .scan(&crate::reader::ScanRequest {
                    projection: Some(vec![0]),
                    substrings: vec![vec![crate::reader::SubstringQuery::like("input", needle)]],
                    read_threads: 2,
                    ..crate::reader::ScanRequest::default()
                })
                .unwrap();
            assert_eq!(collect_i64_ids(&res), expected, "needle={needle}");
        }

        // Inline hits when `.stpk` packs short-circuit MUST match (sequential & parallel threads=2)!
        for threads in [1usize, 2usize] {
            let res_inline = reader
                .scan(&crate::reader::ScanRequest {
                    projection: Some(vec![0]),
                    substrings: vec![vec![
                        crate::reader::SubstringQuery::ilike("input", "INLINE_HIT_BETA"),
                        crate::reader::SubstringQuery::ilike("output", "INLINE_HIT_BETA"),
                        crate::reader::SubstringQuery::ilike(
                            "system_instructions",
                            "INLINE_HIT_BETA",
                        ),
                    ]],
                    read_threads: threads,
                    ..crate::reader::ScanRequest::default()
                })
                .unwrap();
            assert_eq!(
                collect_i64_ids(&res_inline),
                vec![mb1_hit_in as i64, mb1_hit_out as i64]
            );
        }

        // Short <3 B needle MUST NOT be pruned by the all-zeros 256 B bloom!
        let short_res = reader
            .scan(&crate::reader::ScanRequest {
                projection: Some(vec![0]),
                substrings: vec![vec![crate::reader::SubstringQuery::like("input", "ab")]],
                ..crate::reader::ScanRequest::default()
            })
            .unwrap();
        assert_eq!(collect_i64_ids(&short_res), vec![mb2_short as i64]);

        // Mixed OR (`name` matches MB 1, `input` inline trigram rejects inside `eval_inline_fast_path_blocks`).
        let mixed_res = reader
            .scan(&crate::reader::ScanRequest {
                projection: Some(vec![0]),
                substrings: vec![vec![
                    crate::reader::SubstringQuery::ilike("name", "ONLY_IN_NAME_DELTA"),
                    crate::reader::SubstringQuery::ilike("input", "ONLY_IN_NAME_DELTA"),
                ]],
                read_threads: 2,
                ..crate::reader::ScanRequest::default()
            })
            .unwrap();
        assert_eq!(collect_i64_ids(&mixed_res), vec![mb1_hit_name as i64]);
    }

    #[test]
    fn opt6_gdpr_rewrite_l1_no_pack_and_old_file_fallback() {
        use std::io::Cursor;
        use std::sync::Arc;

        let schema = cetra_column::Schema::try_new_arc(vec![
            cetra_column::Field::new("id", cetra_column::DataType::I64, false),
            cetra_column::Field::new("name", cetra_column::DataType::Utf8, false),
            cetra_column::Field::new("input", cetra_column::DataType::Utf8, true),
            cetra_column::Field::new("output", cetra_column::DataType::Utf8, true),
        ])
        .unwrap();

        let mb_size = 64usize;
        let n = 128usize;
        let ids: Vec<i64> = (0..n as i64).collect();
        let names: Vec<String> = (0..n).map(|i| format!("gdpr_name_{i:03}")).collect();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut inputs = Vec::with_capacity(n);
        let mut outputs = Vec::with_capacity(n);
        for r in 0..n {
            inputs.push(if r == 5 {
                Some("GDPR_ERASE_INLINE_SECRET".to_owned())
            } else if r == 6 {
                Some("SURVIVING_INLINE_TOKEN".to_owned())
            } else if r == 70 {
                Some(format!(
                    "{} SURVIVING_EXT_TOKEN {}",
                    "x".repeat(350),
                    "y".repeat(350)
                ))
            } else {
                Some(format!("{} normal_input_{r:03}", "z".repeat(350)))
            });
            outputs.push(Some(format!("inline_out_{r:03}")));
        }
        let batch = cetra_column::Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                cetra_column::Column::Flat(
                    cetra_column::FlatColumn::try_new(cetra_column::FlatData::I64(ids), None)
                        .unwrap(),
                ),
                cetra_column::Column::Flat(
                    cetra_column::FlatColumn::try_new(
                        cetra_column::FlatData::Utf8(
                            cetra_column::Utf8Data::try_from_slices(name_refs).unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
                opt_utf8_col(&inputs),
                opt_utf8_col(&outputs),
            ],
            n,
        )
        .unwrap();

        // 1. L1Compact with `pack_builder = None` (Branch 2 Site 5) writes and reads cleanly.
        let mut no_pack_opts = crate::writer::WriteOptions::for_table(
            crate::format::TableKind::Spans,
            crate::WriteProfile::L1Compact,
        );
        no_pack_opts.mini_block_size = mb_size;
        let mut no_pack_bytes = Vec::new();
        let mut w_np = crate::writer::StatamWriter::new(
            Cursor::new(&mut no_pack_bytes),
            Arc::clone(&schema),
            no_pack_opts,
            None,
        )
        .unwrap();
        w_np.write_batch(&batch).unwrap();
        w_np.finish().unwrap();
        let r_np = crate::reader::StatamReader::open(
            Arc::new(crate::io::SliceReader::new(no_pack_bytes)),
            None,
        )
        .unwrap();
        let np_hits = r_np
            .scan(&crate::reader::ScanRequest {
                projection: Some(vec![0]),
                substrings: vec![vec![crate::reader::SubstringQuery::like(
                    "input",
                    "SURVIVING_INLINE_TOKEN",
                )]],
                ..crate::reader::ScanRequest::default()
            })
            .unwrap();
        assert_eq!(collect_i64_ids(&np_hits), vec![6]);

        // 2. Old-file fallback (no COL_FLAG_HAS_TRIGRAM_INDEX on payload cols).
        let old_pack_id = [0xC1u8; 16];
        let mut pb_old =
            crate::payload::pack::ChunkPackBuilder::new(old_pack_id, make_opt_test_pack_opts());
        let mut old_opts = crate::writer::WriteOptions::for_table(
            crate::format::TableKind::Spans,
            crate::WriteProfile::L1Compact,
        );
        old_opts.mini_block_size = mb_size;
        old_opts.pack_id = Some(old_pack_id);
        old_opts.narrow_trigram_cols.clear();
        let mut old_statam = Vec::new();
        let mut w_old = crate::writer::StatamWriter::new(
            Cursor::new(&mut old_statam),
            Arc::clone(&schema),
            old_opts,
            Some(&mut pb_old),
        )
        .unwrap();
        w_old.write_batch(&batch).unwrap();
        w_old.finish().unwrap();
        let mut old_pack_bytes = Vec::new();
        pb_old.finish(&mut old_pack_bytes).unwrap();
        let mut old_res = crate::reader::InMemoryPackResolver::new();
        old_res
            .insert_bytes(old_pack_id, old_pack_bytes.clone())
            .unwrap();
        let r_old = crate::reader::StatamReader::open(
            Arc::new(crate::io::SliceReader::new(old_statam)),
            Some(Arc::new(old_res)),
        )
        .unwrap();
        assert_eq!(
            r_old.meta().col_dir[2].col_flags & crate::format::COL_FLAG_HAS_TRIGRAM_INDEX,
            0
        );
        let old_hit = r_old
            .scan(&crate::reader::ScanRequest {
                projection: Some(vec![0]),
                substrings: vec![vec![crate::reader::SubstringQuery::like(
                    "input",
                    "SURVIVING_INLINE_TOKEN",
                )]],
                ..crate::reader::ScanRequest::default()
            })
            .unwrap();
        assert_eq!(collect_i64_ids(&old_hit), vec![6]);

        // 3. GDPR rewrite parity on Opt-6 L1Compact file.
        let mut pb_src =
            crate::payload::pack::ChunkPackBuilder::new(old_pack_id, make_opt_test_pack_opts());
        let mut src_opts = crate::writer::WriteOptions::for_table(
            crate::format::TableKind::Spans,
            crate::WriteProfile::L1Compact,
        );
        src_opts.mini_block_size = mb_size;
        src_opts.pack_id = Some(old_pack_id);
        let mut src_statam = Vec::new();
        let mut w_src = crate::writer::StatamWriter::new(
            Cursor::new(&mut src_statam),
            Arc::clone(&schema),
            src_opts,
            Some(&mut pb_src),
        )
        .unwrap();
        w_src.write_batch(&batch).unwrap();
        w_src.finish().unwrap();
        let mut src_pack_bytes = Vec::new();
        pb_src.finish(&mut src_pack_bytes).unwrap();

        let new_pack_id = [0xC2u8; 16];
        let new_statam_cur = Cursor::new(Vec::new());
        let new_pack_cur = Cursor::new(Vec::new());
        let (out_s, out_p, _gdpr_stats) = crate::gdpr::gdpr_rewrite_single_file_streaming(
            Arc::new(crate::io::SliceReader::new(src_statam)),
            Some((
                old_pack_id,
                Arc::new(crate::io::SliceReader::new(src_pack_bytes)),
            )),
            new_pack_id,
            new_statam_cur,
            Some(new_pack_cur),
            None,
            &mut |b| {
                let col = b.columns()[0].materialize(None)?;
                let cetra_column::FlatData::I64(vals) = col.data() else {
                    unreachable!();
                };
                let mut erase = cetra_column::Bitmap::new_zeroed(vals.len());
                for (i, &id) in vals.iter().enumerate() {
                    if id == 5 {
                        let _ = erase.set(i, true);
                    }
                }
                Ok(erase)
            },
            &mut |_| Ok(()),
        )
        .unwrap();
        let rewritten_statam = out_s.into_inner();
        let rewritten_pack = out_p.unwrap().into_inner();

        let mut rw_res = crate::reader::InMemoryPackResolver::new();
        rw_res.insert_bytes(new_pack_id, rewritten_pack).unwrap();
        let s3_rw = Arc::new(
            crate::io::SimulatedS3Reader::from_bytes(
                rewritten_statam,
                crate::io::SimulatedS3Config {
                    cache_capacity_blocks: 0,
                    ..crate::io::SimulatedS3Config::default()
                },
            )
            .with_remote_hint(true),
        );
        let r_rw =
            crate::reader::StatamReader::open(s3_rw.clone(), Some(Arc::new(rw_res))).unwrap();
        assert_ne!(
            r_rw.meta().col_dir[2].col_flags & crate::format::COL_FLAG_HAS_TRIGRAM_INDEX,
            0
        );

        let erased_req = crate::reader::ScanRequest {
            projection: Some(vec![0]),
            substrings: vec![vec![crate::reader::SubstringQuery::like(
                "input",
                "GDPR_ERASE_INLINE_SECRET",
            )]],
            ..crate::reader::ScanRequest::default()
        };
        assert!(r_rw.scan(&erased_req).unwrap().is_empty());
        s3_rw.reset_counters();
        assert!(r_rw.scan(&erased_req).unwrap().is_empty());
        assert_eq!(
            s3_rw.counters().gets,
            0,
            "erased inline secret must NOT linger in rebuilt trigram bloom"
        );

        let surviving_inline = r_rw
            .scan(&crate::reader::ScanRequest {
                projection: Some(vec![0]),
                substrings: vec![vec![crate::reader::SubstringQuery::like(
                    "input",
                    "SURVIVING_INLINE_TOKEN",
                )]],
                ..crate::reader::ScanRequest::default()
            })
            .unwrap();
        assert_eq!(collect_i64_ids(&surviving_inline), vec![6]);

        let surviving_ext = r_rw
            .scan(&crate::reader::ScanRequest {
                projection: Some(vec![0]),
                substrings: vec![vec![crate::reader::SubstringQuery::like(
                    "input",
                    "SURVIVING_EXT_TOKEN",
                )]],
                ..crate::reader::ScanRequest::default()
            })
            .unwrap();
        assert_eq!(collect_i64_ids(&surviving_ext), vec![70]);
    }

    #[test]
    fn opt6_inline_fast_path_const_null_and_inline_missing_pack_isolation() {
        use std::io::Cursor;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use crate::StatamError;
        use crate::codec::{ZonePred, ZoneValue};
        use crate::format::Encoding;
        use cetra_column::{Bitmap, CancelToken};

        struct MissingPackCountingResolver {
            calls: Arc<AtomicUsize>,
        }
        impl crate::reader::PackResolver for MissingPackCountingResolver {
            fn resolve(
                &self,
                pack_id: [u8; 16],
            ) -> Result<Arc<crate::payload::pack::ChunkPackReader>, StatamError> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                Err(StatamError::MissingPack { pack_id })
            }
        }

        let schema = cetra_column::Schema::try_new_arc(vec![
            cetra_column::Field::new("id", cetra_column::DataType::I64, false),
            cetra_column::Field::new("input", cetra_column::DataType::Utf8, true),
            cetra_column::Field::new("output", cetra_column::DataType::Utf8, true),
        ])
        .unwrap();

        // 2 RGs of 4 rows each (2 MBs of 2 rows each):
        //  - RG 0 (ids 0..4): `input` has external `.stpk` chunks, `output` is inline.
        //  - RG 1 (ids 4..8): `input` is all-null (`ConstNull`), `output` is inline (`"match_me"` at id=5).
        let n = 8usize;
        let ids: Vec<i64> = (0..n as i64).collect();
        let inputs: Vec<Option<String>> = (0..n)
            .map(|r| {
                if r < 4 {
                    Some(format!("{} rg0_chunk_{r}", "x".repeat(350)))
                } else {
                    None
                }
            })
            .collect();
        let outputs: Vec<Option<String>> = (0..n)
            .map(|r| {
                if r == 5 {
                    Some("match_me".to_owned())
                } else {
                    Some(format!("inline_out_{r}"))
                }
            })
            .collect();

        let batch = cetra_column::Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                cetra_column::Column::Flat(
                    cetra_column::FlatColumn::try_new(cetra_column::FlatData::I64(ids), None)
                        .unwrap(),
                ),
                opt_utf8_col(&inputs),
                opt_utf8_col(&outputs),
            ],
            n,
        )
        .unwrap();

        let pack_id = [0xD7u8; 16];
        let mut pb =
            crate::payload::pack::ChunkPackBuilder::new(pack_id, make_opt_test_pack_opts());
        let mut opts = crate::writer::WriteOptions::for_table(
            crate::format::TableKind::Spans,
            crate::WriteProfile::L1Compact,
        );
        opts.row_group_size = 4;
        opts.mini_block_size = 2;
        opts.pack_id = Some(pack_id);
        let mut statam_bytes = Vec::new();
        let mut w = crate::writer::StatamWriter::new(
            Cursor::new(&mut statam_bytes),
            Arc::clone(&schema),
            opts,
            Some(&mut pb),
        )
        .unwrap();
        w.write_batch(&batch).unwrap();
        w.finish().unwrap();
        let mut pack_bytes = Vec::new();
        pb.finish(&mut pack_bytes).unwrap();

        let resolve_calls = Arc::new(AtomicUsize::new(0));
        let reader = Arc::new(
            crate::reader::StatamReader::open(
                Arc::new(crate::io::SliceReader::new(statam_bytes)),
                Some(Arc::new(MissingPackCountingResolver {
                    calls: Arc::clone(&resolve_calls),
                })),
            )
            .unwrap(),
        );
        assert_eq!(reader.meta().rg_dir.len(), 2);
        assert_eq!(reader.meta().schema_meta.pack_ids, vec![pack_id]);
        for blk in reader.meta().rg_blocks(1, 1) {
            assert_eq!(blk.encoding, Encoding::ConstNull);
        }
        for blk in reader.meta().rg_blocks(2, 1) {
            assert_eq!(blk.encoding, Encoding::PayloadRefs);
        }

        let req_or = crate::reader::ScanRequest {
            projection: Some(vec![0]),
            predicates: vec![crate::reader::Predicate::new(
                "id",
                ZonePred::Eq(ZoneValue::I64(5)),
            )],
            substrings: vec![vec![
                crate::reader::SubstringQuery::like("input", "aaa"),
                crate::reader::SubstringQuery::like("output", "match_me"),
            ]],
            limit: Some(10),
            ..crate::reader::ScanRequest::default()
        };
        let res_scan = reader.scan(&req_or).unwrap();
        assert_eq!(collect_i64_ids(&res_scan), vec![5]);

        let mut iter = reader
            .scan_iter(crate::reader::ScanRequest {
                limit: None,
                ..req_or
            })
            .unwrap();
        let mut iter_batches = Vec::new();
        while let Some(b) = iter.next_batch(None, None).unwrap() {
            iter_batches.push(b);
        }
        assert_eq!(collect_i64_ids(&iter_batches), vec![5]);

        // Also cover `eval_phase1_mbs_batched_payload` (2 surviving MBs in RG 1, `limit: None`).
        let res_batched = reader
            .scan(&crate::reader::ScanRequest {
                projection: Some(vec![0]),
                predicates: vec![crate::reader::Predicate::new(
                    "id",
                    ZonePred::Ge(ZoneValue::I64(4)),
                )],
                substrings: vec![vec![
                    crate::reader::SubstringQuery::like("input", "aaa"),
                    crate::reader::SubstringQuery::like("output", "match_me"),
                ]],
                read_threads: 2,
                ..crate::reader::ScanRequest::default()
            })
            .unwrap();
        assert_eq!(collect_i64_ids(&res_batched), vec![5]);

        // Pure `ConstNull` payload column must never invoke `resolve_pack`.
        resolve_calls.store(0, Ordering::Relaxed);
        let res_null_only = reader
            .scan(&crate::reader::ScanRequest {
                projection: Some(vec![0]),
                predicates: vec![crate::reader::Predicate::new(
                    "id",
                    ZonePred::Eq(ZoneValue::I64(5)),
                )],
                substrings: vec![vec![crate::reader::SubstringQuery::like("input", "aaa")]],
                limit: Some(10),
                ..crate::reader::ScanRequest::default()
            })
            .unwrap();
        assert!(res_null_only.is_empty());
        assert_eq!(
            resolve_calls.load(Ordering::Relaxed),
            0,
            "ConstNull payload column must not call resolve_pack"
        );

        // Active RG 0 external-chunk row must still surface `StatamError::MissingPack`.
        let err = reader
            .scan(&crate::reader::ScanRequest {
                projection: Some(vec![0]),
                predicates: vec![crate::reader::Predicate::new(
                    "id",
                    ZonePred::Eq(ZoneValue::I64(0)),
                )],
                substrings: vec![vec![crate::reader::SubstringQuery::like(
                    "input",
                    "rg0_chunk",
                )]],
                ..crate::reader::ScanRequest::default()
            })
            .unwrap_err();
        assert!(matches!(err, StatamError::MissingPack { pack_id: pid } if pid == pack_id));

        // Cooperative cancellation in `eval_inline_fast_path_blocks` (sequential & parallel).
        let cancel = CancelToken::new();
        cancel.cancel();
        let rg1_out_blks = reader.meta().rg_blocks(2, 1);
        let dummy_reader = super::BlockCacheReader::Slice {
            base_offset: 0,
            bytes: bytes::Bytes::new(),
        };
        for threads in [1usize, 2usize] {
            let tasks = vec![
                (0, 2, 0, 2, rg1_out_blks[0], Bitmap::new_ones(2)),
                (1, 2, 1, 2, rg1_out_blks[1], Bitmap::new_ones(2)),
            ];
            let err = reader
                .eval_inline_fast_path_blocks(
                    1,
                    b"match_me",
                    false,
                    &[0],
                    &[false],
                    &dummy_reader,
                    threads,
                    tasks,
                    Some(&cancel),
                )
                .unwrap_err();
            assert!(
                matches!(err, StatamError::Cancelled),
                "threads={threads} expected Cancelled, got {err:?}"
            );
        }
    }
}
