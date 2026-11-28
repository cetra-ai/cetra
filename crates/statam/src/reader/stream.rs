//! Stateful mini-block streaming scan iterator with cooperative cancellation and memory reservation.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::Arc;

use bytes::Bytes;
use cetra_column::{Batch, Bitmap, CancelToken, DataType, Reservation, Schema, Selection};

use crate::StatamError;
use crate::format::{
    BLOCK_FRAME_HEADER_LEN, COL_FLAG_HAS_FILE_DICT, COL_FLAG_HAS_TRIGRAM_INDEX, Encoding,
    LocalityGroup,
};
use crate::parallel::{TaskExecutor, enter_executor_scope};
use crate::payload::search::RetainedPackChunks;
use crate::simd::bloom::statam_bloom_hash;

use super::eval::{is_always_false_pred, is_supported_id_lookup_type, trim_bitmap_mask};
use super::payload::{ParsedL0Payload, PayloadWavePlan, ResolvedChunks, clone_decoded_p1_for_wave};
use super::plan::{
    ActiveMiniBlock, BlockCacheReader, DecodedPhase1, ResolvedFilterExpr, RgIoPlan,
    find_time_column,
};
use super::{
    CoalesceMode, IO_WAVE_MAX_BYTES, IdLookup, MapReadMode, P2CandidateFetch, Predicate,
    REMOTE_SINGLE_WAVE_P2_MAX_BYTES, ScanRequest, StatamReader, SubstringQuery, TimeRange,
    truncate_batch_selection, truncate_batch_selection_tail,
};

#[inline]
fn check_cancel(cancel: Option<&CancelToken>) -> Result<(), StatamError> {
    if cancel.is_some_and(CancelToken::is_cancelled) {
        return Err(StatamError::Cancelled);
    }
    Ok(())
}

type RetainedChunkMap = HashMap<([u8; 16], u32), Bytes>;

struct PendingMbWaves {
    mb_idx: usize,
    decoded_p1: HashMap<usize, DecodedPhase1>,
    sub_masks: VecDeque<Bitmap>,
    local_retained: Option<RetainedChunkMap>,
}

enum ActiveRgPhase {
    PointEval {
        surviving_mbs: VecDeque<usize>,
    },
    ReadyMbs {
        active_mbs: VecDeque<ActiveMiniBlock>,
    },
}

struct ActiveRgCursor {
    rg_idx: usize,
    plan: Arc<RgIoPlan>,
    block_reader: BlockCacheReader,
    l0_parsed: Option<Arc<ParsedL0Payload>>,
    retained_chunks: RetainedChunkMap,
    phase: ActiveRgPhase,
    pending_mb_waves: Option<PendingMbWaves>,
}

/// Stateful streaming scan iterator over a [`StatamReader`].
///
/// Decodes and yields one mini-block [`Batch`] at a time (never buffering all batches of a
/// Row Group unreserved), checks [`CancelToken`] cooperatively before I/O waves and mini-block
/// decodes, and enforces exact [`Reservation`] accounting per yielded batch.
pub struct StatamScanIter {
    reader: Arc<StatamReader>,
    proj_indices: Vec<usize>,
    proj_schema: Arc<Schema>,
    proj_payload_cols: Vec<usize>,
    has_payload_proj: bool,
    map_proj_by_col: Arc<HashMap<usize, MapReadMode>>,
    time_col_idx: Option<usize>,
    time_range: TimeRange,
    resolved_lookups: Vec<(usize, IdLookup)>,
    resolved_preds: Vec<(usize, Predicate)>,
    resolved_filter_exprs: Vec<ResolvedFilterExpr>,
    resolved_subs: Vec<Vec<(usize, SubstringQuery)>>,
    lookup_hashes: Vec<(usize, Vec<u64>)>,
    bloom_cols: Vec<usize>,
    sub_cols_with_trigram: Vec<usize>,
    preload_dict_cols: Vec<usize>,
    dict_sub_bitmaps: Vec<Vec<Option<Bitmap>>>,
    has_filters: bool,
    mode: CoalesceMode,
    desc_limit: bool,
    remaining_limit: Option<usize>,
    top_k: Option<super::TopKSpec>,
    read_threads: usize,
    executor: Option<Arc<dyn TaskExecutor>>,
    rg_order: VecDeque<usize>,
    preloaded: bool,
    cur_rg: Option<ActiveRgCursor>,
    done: bool,
}

impl fmt::Debug for StatamScanIter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StatamScanIter")
            .field("remaining_rgs", &self.rg_order.len())
            .field("has_active_rg", &self.cur_rg.is_some())
            .field("remaining_limit", &self.remaining_limit)
            .field("mode", &self.mode)
            .field("done", &self.done)
            .finish()
    }
}

impl StatamReader {
    /// Opens a stateful mini-block streaming scan iterator for `req`.
    ///
    /// Validates projection and filter columns eagerly; defers lazy index and block I/O to
    /// [`StatamScanIter::next_batch`] so cooperative cancellation is respected before any I/O.
    pub fn scan_iter(self: &Arc<Self>, req: ScanRequest) -> Result<StatamScanIter, StatamError> {
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

        let time_col_idx = if req.time_range.is_all() {
            None
        } else {
            find_time_column(schema)
        };

        let mut early_empty = req.time_range.is_empty()
            || self.meta.postscript.row_count == 0
            || req.limit == Some(0);
        if let Some(spec) = req.top_k {
            self.validate_top_k_spec(spec)?;
            if spec.limit == 0 {
                early_empty = true;
            }
        }

        let mut resolved_lookups = Vec::with_capacity(req.id_lookups.len());
        for lk in req.id_lookups {
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
        for pred in req.predicates {
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
                && !matches!(
                    pred.pred,
                    crate::codec::ZonePred::IsNull | crate::codec::ZonePred::IsNotNull
                )
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

        let mut resolved_filter_exprs = Vec::with_capacity(req.filter_exprs.len());
        for expr in &req.filter_exprs {
            resolved_filter_exprs.push(self.resolve_filter_expr(expr)?);
        }

        let mut resolved_subs: Vec<Vec<(usize, SubstringQuery)>> =
            Vec::with_capacity(req.substrings.len());
        for clause in req.substrings {
            if clause.is_empty() {
                early_empty = true;
            }
            let mut resolved_clause = Vec::with_capacity(clause.len());
            for sq in clause {
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
                resolved_clause.push((cidx, sq));
            }
            resolved_subs.push(resolved_clause);
        }

        let mut map_proj_by_col: HashMap<usize, MapReadMode> = HashMap::new();
        for (col_name, mode) in req.map_projections {
            let cidx = schema
                .index_of(&col_name)
                .ok_or(StatamError::SchemaMismatch {
                    reason: "map_projections column not found in schema",
                })?;
            if schema.fields()[cidx].data_type != DataType::MapUtf8 {
                return Err(StatamError::SchemaMismatch {
                    reason: "map_projections specified for non-MapUtf8 column",
                });
            }
            match mode {
                MapReadMode::Full => {}
                MapReadMode::KeysOnly => {
                    map_proj_by_col.insert(cidx, MapReadMode::KeysOnly);
                }
                MapReadMode::Keys(mut keys) => {
                    keys.sort_unstable();
                    keys.dedup();
                    map_proj_by_col.insert(cidx, MapReadMode::Keys(keys));
                }
            }
        }
        let map_proj_by_col = Arc::new(map_proj_by_col);

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

        let lookup_hashes: Vec<(usize, Vec<u64>)> = resolved_lookups
            .iter()
            .map(|(cidx, lk)| {
                let hashes = lk.values.iter().map(|v| statam_bloom_hash(v)).collect();
                (*cidx, hashes)
            })
            .collect();

        let pred_refs: Vec<(usize, &Predicate)> =
            resolved_preds.iter().map(|(c, p)| (*c, p)).collect();
        let mut rg_vec = self.candidate_rg_range(
            time_col_idx,
            req.time_range,
            &pred_refs,
            &resolved_filter_exprs,
        );
        if let Some(allowed_rgs) = req.rg_indices {
            let allowed: HashSet<usize> = allowed_rgs.into_iter().collect();
            rg_vec.retain(|rg_idx| allowed.contains(rg_idx));
        }
        let can_reverse_early_stop =
            req.sort_desc_hint && req.limit.is_some() && self.is_sort_col0_mb_monotonic();
        let desc_limit = can_reverse_early_stop;
        let remaining_limit =
            if req.top_k.is_none() && req.sort_desc_hint && !can_reverse_early_stop {
                None
            } else {
                req.limit
            };
        if desc_limit {
            rg_vec.reverse();
        }
        let done = early_empty || rg_vec.is_empty();

        let mode = match req.coalesce_mode {
            CoalesceMode::Auto => {
                if !resolved_lookups.is_empty() {
                    CoalesceMode::Point
                } else {
                    CoalesceMode::Scan
                }
            }
            other => other,
        };

        let bloom_cols: Vec<usize> = lookup_hashes.iter().map(|&(cidx, _)| cidx).collect();
        let mut sub_cols_with_trigram: Vec<usize> = resolved_subs
            .iter()
            .flat_map(|cl| cl.iter())
            .filter_map(|(cidx, _)| {
                ((self.meta.col_dir[*cidx].col_flags & COL_FLAG_HAS_TRIGRAM_INDEX) != 0)
                    .then_some(*cidx)
            })
            .collect();
        sub_cols_with_trigram.sort_unstable();
        sub_cols_with_trigram.dedup();

        let mut preload_dict_cols = Vec::new();
        if let Some(tc) = time_col_idx {
            preload_dict_cols.push(tc);
        }
        for (cidx, _) in &resolved_lookups {
            preload_dict_cols.push(*cidx);
        }
        for (cidx, pred) in &resolved_preds {
            preload_dict_cols.push(*cidx);
            if let Some(key) = &pred.map_key
                && let Some((sub_pidx, _)) = self.find_shredded_subcol(*cidx, key)
            {
                preload_dict_cols.push(sub_pidx);
            }
        }
        for expr in &resolved_filter_exprs {
            self.collect_filter_expr_dict_cols(expr, &mut preload_dict_cols);
        }
        for clause in &resolved_subs {
            for (cidx, _) in clause {
                preload_dict_cols.push(*cidx);
            }
        }

        let has_filters = time_col_idx.is_some()
            || !resolved_lookups.is_empty()
            || !resolved_preds.is_empty()
            || !resolved_filter_exprs.is_empty()
            || !resolved_subs.is_empty();

        let executor = req.executor.or_else(|| self.executor.clone());

        Ok(StatamScanIter {
            reader: Arc::clone(self),
            proj_indices,
            proj_schema,
            proj_payload_cols,
            has_payload_proj,
            map_proj_by_col,
            time_col_idx,
            time_range: req.time_range,
            resolved_lookups,
            resolved_preds,
            resolved_filter_exprs,
            resolved_subs,
            lookup_hashes,
            bloom_cols,
            sub_cols_with_trigram,
            preload_dict_cols,
            dict_sub_bitmaps: Vec::new(),
            has_filters,
            mode,
            desc_limit,
            remaining_limit,
            top_k: req.top_k,
            read_threads: req.read_threads,
            executor,
            rg_order: rg_vec.into(),
            preloaded: false,
            cur_rg: None,
            done,
        })
    }
}

impl StatamScanIter {
    /// Decodes and returns the next mini-block [`Batch`], or `Ok(None)` when exhausted.
    ///
    /// Enforces cooperative cancellation via `cancel` (`Err(StatamError::Cancelled)`) and
    /// exact `Reservation` accounting: on `Ok(Some(batch))`, `res.size() == before + batch.memory_bytes()`;
    /// on `Ok(None)` or `Err(_)`, `res.size() == before`.
    pub fn next_batch(
        &mut self,
        cancel: Option<&CancelToken>,
        res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, StatamError> {
        Ok(self.next_batch_inner(cancel, res, false)?.map(|(b, _)| b))
    }

    /// Decodes and returns the next mini-block [`Batch`] paired with exact file-global 0-based
    /// `u64` row indices matching every active row of `batch` (computed before `Selection` compaction).
    ///
    /// Enforces the same cooperative cancellation and exact `Reservation` postconditions as
    /// [`Self::next_batch`].
    pub fn next_batch_with_row_ids(
        &mut self,
        cancel: Option<&CancelToken>,
        res: Option<&mut Reservation>,
    ) -> Result<Option<(Batch, Vec<u64>)>, StatamError> {
        self.next_batch_inner(cancel, res, true)
    }

    fn next_batch_inner(
        &mut self,
        cancel: Option<&CancelToken>,
        mut res: Option<&mut Reservation>,
        want_row_ids: bool,
    ) -> Result<Option<(Batch, Vec<u64>)>, StatamError> {
        let _exec_guard = enter_executor_scope(self.executor.as_ref());
        let res_before = res.as_ref().map(|r| r.size());
        check_cancel(cancel)?;
        if self.done || self.remaining_limit == Some(0) {
            self.cur_rg = None;
            self.done = true;
            return Ok(None);
        }

        if !self.preloaded {
            if !self.bloom_cols.is_empty() || !self.sub_cols_with_trigram.is_empty() {
                self.reader.ensure_lazy_indexes_loaded(
                    &self.bloom_cols,
                    &self.sub_cols_with_trigram,
                    &self.preload_dict_cols,
                )?;
                if !self.lookup_hashes.is_empty()
                    && !self.reader.blooms_may_match(&self.lookup_hashes)
                {
                    self.done = true;
                    return Ok(None);
                }
            } else if !self.preload_dict_cols.is_empty() {
                self.reader
                    .load_inline_lazy_indexes(&[], &[], &self.preload_dict_cols)?;
            }
            self.dict_sub_bitmaps = self.reader.build_dict_sub_bitmaps(&self.resolved_subs);
            self.preloaded = true;
        }

        if let Some(spec) = self.top_k {
            let effective_k = self
                .remaining_limit
                .map_or(spec.limit, |l| l.min(spec.limit));
            let rg_slice: Vec<usize> = self.rg_order.iter().copied().collect();
            let lookup_refs: Vec<(usize, &IdLookup)> =
                self.resolved_lookups.iter().map(|(c, l)| (*c, l)).collect();
            let pred_refs: Vec<(usize, &Predicate)> =
                self.resolved_preds.iter().map(|(c, p)| (*c, p)).collect();
            let outcome = self.reader.scan_top_k_with_limits(
                spec,
                effective_k,
                &rg_slice,
                &self.proj_indices,
                &self.proj_schema,
                &self.map_proj_by_col,
                self.time_col_idx,
                self.time_range,
                &lookup_refs,
                &self.lookup_hashes,
                &pred_refs,
                &self.resolved_filter_exprs,
                &self.resolved_subs,
                &mut self.dict_sub_bitmaps,
                self.read_threads,
                cancel,
                res.as_deref_mut(),
            );
            self.done = true;
            self.rg_order.clear();
            let (batches, row_ids) = match outcome {
                Ok(pair) => pair,
                Err(e) => {
                    if let (Some(r), Some(before)) = (res, res_before)
                        && r.size() > before
                    {
                        r.shrink(r.size() - before);
                    }
                    return Err(e);
                }
            };
            return Ok(batches.into_iter().next().map(|b| {
                let ids = if want_row_ids { row_ids } else { Vec::new() };
                (b, ids)
            }));
        }

        loop {
            check_cancel(cancel)?;
            if self.remaining_limit == Some(0) {
                self.cur_rg = None;
                self.done = true;
                return Ok(None);
            }

            if self.cur_rg.is_some() {
                match self.next_mb_from_active_rg(cancel)? {
                    Some((batch, first_row)) => {
                        if batch.is_empty() {
                            continue;
                        }
                        let batch = if let Some(rem) = self.remaining_limit {
                            if batch.num_rows() > rem {
                                if self.desc_limit {
                                    truncate_batch_selection_tail(&batch, rem)?
                                } else {
                                    truncate_batch_selection(&batch, rem)?
                                }
                            } else {
                                batch
                            }
                        } else {
                            batch
                        };

                        let mem = batch.memory_bytes();
                        if let Some(r) = res.as_deref_mut()
                            && r.try_grow(mem).is_err()
                        {
                            if let Some(before) = res_before
                                && r.size() > before
                            {
                                r.shrink(r.size() - before);
                            }
                            return Err(StatamError::MemoryExceeded);
                        }

                        let row_ids = if want_row_ids {
                            match batch.selection() {
                                Selection::All => {
                                    (0..batch.raw_len()).map(|r| first_row + r as u64).collect()
                                }
                                Selection::Mask(bm) => {
                                    bm.iter_ones().map(|r| first_row + r as u64).collect()
                                }
                                Selection::Indices(idxs) => {
                                    idxs.iter().map(|&r| first_row + u64::from(r)).collect()
                                }
                            }
                        } else {
                            Vec::new()
                        };

                        if let Some(rem) = &mut self.remaining_limit {
                            *rem = rem.saturating_sub(batch.num_rows());
                            if *rem == 0 {
                                self.cur_rg = None;
                                self.done = true;
                            }
                        }
                        return Ok(Some((batch, row_ids)));
                    }
                    None => {
                        self.cur_rg = None;
                    }
                }
            } else if !self.open_next_rg(cancel)? {
                self.done = true;
                return Ok(None);
            }
        }
    }

    fn open_next_rg(&mut self, cancel: Option<&CancelToken>) -> Result<bool, StatamError> {
        while let Some(rg_idx) = self.rg_order.pop_front() {
            check_cancel(cancel)?;
            if !self.lookup_hashes.is_empty()
                && !self.reader.rg_blooms_may_match(rg_idx, &self.lookup_hashes)
            {
                continue;
            }
            let mb_count = self.reader.meta.rg_blocks(0, rg_idx).len();
            if mb_count == 0 {
                continue;
            }

            let (surviving_mbs, plan) = {
                let lookup_refs: Vec<(usize, &IdLookup)> = self
                    .resolved_lookups
                    .iter()
                    .map(|(c, lk)| (*c, lk))
                    .collect();
                let pred_refs: Vec<(usize, &Predicate)> =
                    self.resolved_preds.iter().map(|(c, p)| (*c, p)).collect();

                let mut surviving_mbs = self.reader.prune_rg_mini_blocks(
                    rg_idx,
                    mb_count,
                    self.time_col_idx,
                    self.time_range,
                    &lookup_refs,
                    &pred_refs,
                    &self.resolved_filter_exprs,
                    &self.resolved_subs,
                    &self.dict_sub_bitmaps,
                )?;
                if surviving_mbs.is_empty() {
                    continue;
                }
                let allow_pre_wave1_pack_open = self.resolved_preds.is_empty()
                    && self.resolved_filter_exprs.is_empty()
                    && self.time_col_idx.is_none()
                    && self.resolved_lookups.is_empty();
                if self.reader.rg_pruned_by_external_packs(
                    rg_idx,
                    &surviving_mbs,
                    &self.resolved_subs,
                    allow_pre_wave1_pack_open,
                )? {
                    continue;
                }

                if self.desc_limit {
                    surviving_mbs.reverse();
                }
                if let Some(rem) = self.remaining_limit
                    && !self.has_filters
                {
                    let mut covered = 0usize;
                    let mut keep_cnt = 0usize;
                    let blks0 = self.reader.meta.rg_blocks(0, rg_idx);
                    for &mb in &surviving_mbs {
                        keep_cnt += 1;
                        covered = covered.saturating_add(blks0[mb].row_count as usize);
                        if covered >= rem {
                            break;
                        }
                    }
                    surviving_mbs.truncate(keep_cnt);
                }

                let plan = Arc::new(self.reader.build_rg_io_plan(
                    rg_idx,
                    &surviving_mbs,
                    &self.proj_indices,
                    &self.map_proj_by_col,
                    self.time_col_idx,
                    self.time_range,
                    &lookup_refs,
                    &pred_refs,
                    &self.resolved_filter_exprs,
                    &self.resolved_subs,
                ));
                (surviving_mbs, plan)
            };

            if self.mode == CoalesceMode::Point {
                let cursor = self.open_rg_point(rg_idx, surviving_mbs, plan, cancel)?;
                self.cur_rg = Some(cursor);
                return Ok(true);
            }
            if !self.has_filters {
                let cursor = self.open_rg_unfiltered(rg_idx, surviving_mbs, plan, cancel)?;
                self.cur_rg = Some(cursor);
                return Ok(true);
            }
            if let Some(cursor) =
                self.open_rg_filtered_scan(rg_idx, &surviving_mbs, plan, cancel)?
            {
                self.cur_rg = Some(cursor);
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn open_rg_point(
        &mut self,
        rg_idx: usize,
        surviving_mbs: Vec<usize>,
        plan: Arc<RgIoPlan>,
        cancel: Option<&CancelToken>,
    ) -> Result<ActiveRgCursor, StatamError> {
        check_cancel(cancel)?;
        let rg = &self.reader.meta.rg_dir[rg_idx];

        let mut need_l0_payload = false;
        let mut min_off = u64::MAX;
        let mut max_end = 0u64;
        for &pidx in plan.all_phys_cols() {
            if self.reader.meta.col_dir[pidx].locality_group == LocalityGroup::ExternalPayload
                && rg.l0_payload_len > 0
            {
                need_l0_payload = true;
            }
            let blks = self.reader.meta.rg_blocks(pidx, rg_idx);
            let is_map = self
                .reader
                .meta
                .schema_meta
                .schema
                .fields()
                .get(pidx)
                .is_some_and(|f| f.data_type == DataType::MapUtf8);
            let in_phase1 = plan.phase1_phys_cols.contains(&pidx);
            for &mb_idx in &surviving_mbs {
                if !in_phase1 && plan.phase2_skip_subcols.contains(&(mb_idx, pidx)) {
                    continue;
                }
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

        let dict_cols: Vec<usize> = plan
            .all_phys_cols()
            .copied()
            .filter(|&pidx| {
                (self.reader.meta.col_dir[pidx].col_flags & COL_FLAG_HAS_FILE_DICT) != 0
                    && surviving_mbs.iter().any(|&mb| {
                        self.reader.meta.rg_blocks(pidx, rg_idx)[mb].encoding == Encoding::Dict
                    })
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

        let has_non_null_payload_proj = self.proj_indices.iter().any(|&cidx| {
            self.reader.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload
                && surviving_mbs.iter().any(|&mb| {
                    self.reader.meta.rg_blocks(cidx, rg_idx)[mb].encoding != Encoding::ConstNull
                })
        });
        let eligible_for_pack_prefetch =
            !self.resolved_lookups.is_empty() && has_non_null_payload_proj;

        check_cancel(cancel)?;
        let rg_slice =
            if range_end > range_start {
                let fetched = self.reader.fetch_blocks_and_lazy_maybe_prefetch_packs(
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
                    self.reader.ensure_dicts_loaded(&dict_cols)?;
                }
                Bytes::new()
            };

        self.reader
            .refresh_dict_sub_bitmaps(&self.resolved_subs, &mut self.dict_sub_bitmaps);

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

        Ok(ActiveRgCursor {
            rg_idx,
            plan,
            block_reader,
            l0_parsed,
            retained_chunks: HashMap::new(),
            phase: ActiveRgPhase::PointEval {
                surviving_mbs: surviving_mbs.into(),
            },
            pending_mb_waves: None,
        })
    }

    fn open_rg_unfiltered(
        &mut self,
        rg_idx: usize,
        surviving_mbs: Vec<usize>,
        plan: Arc<RgIoPlan>,
        cancel: Option<&CancelToken>,
    ) -> Result<ActiveRgCursor, StatamError> {
        check_cancel(cancel)?;
        let rg = &self.reader.meta.rg_dir[rg_idx];

        let mut ranges = Vec::new();
        for &mb_idx in &surviving_mbs {
            for &pidx in plan.all_phys_cols() {
                if plan.phase2_skip_subcols.contains(&(mb_idx, pidx)) {
                    continue;
                }
                if self
                    .reader
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
                let blk = &self.reader.meta.rg_blocks(pidx, rg_idx)[mb_idx];
                if blk.stored_len > 0 {
                    let total = (BLOCK_FRAME_HEADER_LEN as u64)
                        .checked_add(u64::from(blk.stored_len))
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "Scan block length overflow",
                        })?;
                    let end = blk
                        .offset
                        .checked_add(total)
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "Scan block end overflow",
                        })?;
                    ranges.push((blk.offset, end));
                }
            }
        }
        let l0_needed = self.has_payload_proj && rg.l0_payload_len > 0;
        let l0_end = rg
            .l0_payload_offset
            .checked_add(u64::from(rg.l0_payload_len))
            .ok_or(StatamError::BoundsExceeded {
                reason: "Scan L0 payload end overflow",
            })?;
        if l0_needed {
            ranges.push((rg.l0_payload_offset, l0_end));
        }

        let dict_cols: Vec<usize> = plan
            .all_phys_cols()
            .copied()
            .filter(|&pidx| {
                (self.reader.meta.col_dir[pidx].col_flags & COL_FLAG_HAS_FILE_DICT) != 0
                    && surviving_mbs.iter().any(|&mb| {
                        self.reader.meta.rg_blocks(pidx, rg_idx)[mb].encoding == Encoding::Dict
                    })
            })
            .collect();

        let eligible_for_pack_prefetch = self.has_payload_proj
            && self.proj_payload_cols.iter().any(|&cidx| {
                surviving_mbs.iter().any(|&mb| {
                    self.reader.meta.rg_blocks(cidx, rg_idx)[mb].encoding != Encoding::ConstNull
                })
            });

        check_cancel(cancel)?;
        let cache = Arc::new(self.reader.fetch_blocks_and_lazy_maybe_prefetch_packs(
            &ranges,
            &[],
            &[],
            &dict_cols,
            eligible_for_pack_prefetch,
        )?);

        let l0_parsed = if l0_needed {
            cache
                .get(&(rg.l0_payload_offset, l0_end))
                .map(|b| ParsedL0Payload::parse(b.clone()).map(Arc::new))
                .transpose()?
        } else {
            None
        };

        let block_reader = BlockCacheReader::Map {
            primary: cache,
            fallback: None,
            missing_reason: "missing unfiltered Scan fetched block",
            overflow_reason: "Scan block key overflow",
        };

        let blks0 = self.reader.meta.rg_blocks(0, rg_idx);
        let mut active_mbs: VecDeque<ActiveMiniBlock> =
            VecDeque::with_capacity(surviving_mbs.len());
        let mut active_rows = 0usize;
        for mb_idx in surviving_mbs {
            let mut mask = Bitmap::new_ones(blks0[mb_idx].row_count as usize);
            let hits = mask.len();
            if hits == 0 {
                continue;
            }
            if let Some(lim) = self.remaining_limit {
                let rem = lim.saturating_sub(active_rows);
                if rem == 0 {
                    break;
                }
                if hits > rem {
                    trim_bitmap_mask(&mut mask, rem, self.desc_limit);
                    active_mbs.push_back((mb_idx, mask, HashMap::new()));
                    break;
                }
                active_rows = active_rows.saturating_add(hits);
                active_mbs.push_back((mb_idx, mask, HashMap::new()));
                if active_rows >= lim {
                    break;
                }
            } else {
                active_rows = active_rows.saturating_add(hits);
                active_mbs.push_back((mb_idx, mask, HashMap::new()));
            }
        }

        Ok(ActiveRgCursor {
            rg_idx,
            plan,
            block_reader,
            l0_parsed,
            retained_chunks: HashMap::new(),
            phase: ActiveRgPhase::ReadyMbs { active_mbs },
            pending_mb_waves: None,
        })
    }

    fn open_rg_filtered_scan(
        &mut self,
        rg_idx: usize,
        surviving_mbs: &[usize],
        plan: Arc<RgIoPlan>,
        cancel: Option<&CancelToken>,
    ) -> Result<Option<ActiveRgCursor>, StatamError> {
        check_cancel(cancel)?;
        let rg = &self.reader.meta.rg_dir[rg_idx];

        let mut p1_ranges = Vec::new();
        let mut p1_bytes = 0u64;
        for &mb_idx in surviving_mbs {
            for &pidx in &plan.phase1_phys_cols {
                let blk = &self.reader.meta.rg_blocks(pidx, rg_idx)[mb_idx];
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

        let has_payload_sub = self.resolved_subs.iter().any(|cl| {
            cl.iter().any(|(cidx, _)| {
                self.reader.meta.col_dir[*cidx].locality_group == LocalityGroup::ExternalPayload
            })
        });

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
                (self.reader.meta.col_dir[pidx].col_flags & COL_FLAG_HAS_FILE_DICT) != 0
                    && surviving_mbs.iter().any(|&mb| {
                        self.reader.meta.rg_blocks(pidx, rg_idx)[mb].encoding == Encoding::Dict
                    })
            })
            .collect();

        let mut p2_folded_into_p1 = false;
        if self.reader.reader.is_remote() && !has_payload_sub {
            let cand = self.reader.collect_p2_candidate_ranges_and_dicts(
                rg_idx,
                surviving_mbs,
                &plan,
                p1_l0_needed,
                p1_l0_end,
                self.has_payload_proj,
            )?;
            let cand_p2_dict_bytes = self
                .reader
                .uncached_non_inline_lazy_dict_bytes(&cand.dict_cols);
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
            && self.resolved_preds.is_empty()
            && self.resolved_filter_exprs.is_empty()
            && self.time_col_idx.is_none()
            && self.resolved_lookups.is_empty()
            && self.resolved_subs.iter().any(|cl| {
                cl.iter().any(|(cidx, _)| {
                    self.reader.meta.col_dir[*cidx].locality_group == LocalityGroup::ExternalPayload
                        && surviving_mbs.iter().any(|&mb| {
                            self.reader.meta.rg_blocks(*cidx, rg_idx)[mb].encoding
                                != Encoding::ConstNull
                        })
                })
            });

        // Wave 1: Phase-1 blocks + dicts.
        check_cancel(cancel)?;
        let p1_cache = Arc::new(self.reader.fetch_blocks_and_lazy_maybe_prefetch_packs(
            &p1_ranges,
            &[],
            &[],
            &p1_dict_cols,
            eligible_for_pack_prefetch,
        )?);

        self.reader
            .refresh_dict_sub_bitmaps(&self.resolved_subs, &mut self.dict_sub_bitmaps);

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

        let lookup_refs: Vec<(usize, &IdLookup)> = self
            .resolved_lookups
            .iter()
            .map(|(c, lk)| (*c, lk))
            .collect();
        let pred_refs: Vec<(usize, &Predicate)> =
            self.resolved_preds.iter().map(|(c, p)| (*c, p)).collect();

        // Hoist `.stpk` frame dedup to RG level when there is NO `LIMIT`; preserve MB-by-MB
        // early exit when `remaining_limit` is set.
        let retained = RetainedPackChunks::default();
        let active_mbs: VecDeque<ActiveMiniBlock> =
            if self.remaining_limit.is_none() && has_payload_sub && surviving_mbs.len() >= 2 {
                let threads = self
                    .read_threads
                    .clamp(1, crate::parallel::MAX_PARALLEL_THREADS);
                self.reader
                    .eval_phase1_mbs_batched_payload(
                        rg_idx,
                        surviving_mbs,
                        &plan,
                        &self.proj_indices,
                        self.time_col_idx,
                        self.time_range,
                        &lookup_refs,
                        &pred_refs,
                        &self.resolved_filter_exprs,
                        &self.resolved_subs,
                        &self.dict_sub_bitmaps,
                        l0_from_p1.as_deref(),
                        &p1_reader,
                        threads,
                        &retained,
                        cancel,
                    )?
                    .into()
            } else {
                let mut active_mbs: VecDeque<ActiveMiniBlock> = VecDeque::new();
                let mut active_rows = 0usize;
                for &mb_idx in surviving_mbs {
                    check_cancel(cancel)?;
                    let (mut mask, decoded_p1) = self.reader.eval_phase1_mb(
                        rg_idx,
                        mb_idx,
                        &plan,
                        &self.proj_indices,
                        self.time_col_idx,
                        self.time_range,
                        &lookup_refs,
                        &pred_refs,
                        &self.resolved_filter_exprs,
                        &self.resolved_subs,
                        &self.dict_sub_bitmaps,
                        l0_from_p1.as_deref(),
                        &p1_reader,
                        self.read_threads,
                        &retained,
                        None,
                        cancel,
                    )?;
                    let hits = mask.count_ones();
                    if hits > 0 {
                        if let Some(lim) = self.remaining_limit {
                            let rem = lim.saturating_sub(active_rows);
                            if rem == 0 {
                                break;
                            }
                            if hits > rem {
                                trim_bitmap_mask(&mut mask, rem, self.desc_limit);
                                active_mbs.push_back((mb_idx, mask, decoded_p1));
                                break;
                            }
                            active_rows = active_rows.saturating_add(hits);
                            active_mbs.push_back((mb_idx, mask, decoded_p1));
                            if active_rows >= lim {
                                break;
                            }
                        } else {
                            active_rows = active_rows.saturating_add(hits);
                            active_mbs.push_back((mb_idx, mask, decoded_p1));
                        }
                    }
                }
                active_mbs
            };

        if active_mbs.is_empty() {
            return Ok(None);
        }
        let retained_chunks = retained.into_map();

        // Wave 2: Phase-2 blocks + dicts for surviving mini-blocks only.
        check_cancel(cancel)?;
        let p2_l0_needed = rg.l0_payload_len > 0 && !p1_l0_needed && self.has_payload_proj;
        let p2_cache = Arc::new(if p2_folded_into_p1 {
            HashMap::new()
        } else {
            let active_mb_ids: Vec<usize> = active_mbs.iter().map(|(mb, _, _)| *mb).collect();
            let P2CandidateFetch {
                ranges, dict_cols, ..
            } = self.reader.collect_p2_candidate_ranges_and_dicts(
                rg_idx,
                &active_mb_ids,
                &plan,
                p1_l0_needed,
                p1_l0_end,
                self.has_payload_proj,
            )?;
            self.reader
                .fetch_blocks_and_lazy(&ranges, &[], &[], &dict_cols)?
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
        let l0_parsed = l0_from_p1.or(l0_from_p2);

        let block_reader = BlockCacheReader::Map {
            primary: p2_cache,
            fallback: Some(p1_cache),
            missing_reason: "missing Phase 2 fetched block",
            overflow_reason: "Phase 2 block key overflow",
        };

        Ok(Some(ActiveRgCursor {
            rg_idx,
            plan,
            block_reader,
            l0_parsed,
            retained_chunks,
            phase: ActiveRgPhase::ReadyMbs { active_mbs },
            pending_mb_waves: None,
        }))
    }

    fn next_mb_from_active_rg(
        &mut self,
        cancel: Option<&CancelToken>,
    ) -> Result<Option<(Batch, u64)>, StatamError> {
        let lookup_refs: Vec<(usize, &IdLookup)> = self
            .resolved_lookups
            .iter()
            .map(|(c, lk)| (*c, lk))
            .collect();
        let pred_refs: Vec<(usize, &Predicate)> =
            self.resolved_preds.iter().map(|(c, p)| (*c, p)).collect();

        loop {
            check_cancel(cancel)?;
            let Some(cur) = self.cur_rg.as_mut() else {
                return Ok(None);
            };

            // 1. Drain any remaining payload sub-wave for the current single mini-block.
            if let Some(mut pw) = cur.pending_mb_waves.take()
                && let Some(sub_mask) = pw.sub_masks.pop_front()
            {
                let mb_idx = pw.mb_idx;
                let first_row =
                    u64::from(self.reader.meta.rg_blocks(0, cur.rg_idx)[mb_idx].first_row);
                let (decoded_p1, local_retained) = if pw.sub_masks.is_empty() {
                    (pw.decoded_p1, pw.local_retained)
                } else {
                    let cloned = clone_decoded_p1_for_wave(&pw.decoded_p1);
                    let ret_clone = pw.local_retained.clone();
                    cur.pending_mb_waves = Some(pw);
                    (cloned, ret_clone)
                };
                let pre_resolved = local_retained.as_ref().unwrap_or(&cur.retained_chunks);
                let mut wave_mbs = vec![(mb_idx, sub_mask, decoded_p1)];
                let resolved = self.reader.resolve_rg_payload_chunks(
                    cur.rg_idx,
                    &mut wave_mbs,
                    &self.proj_indices,
                    cur.l0_parsed.as_deref(),
                    pre_resolved,
                    &cur.block_reader,
                    self.read_threads,
                )?;
                check_cancel(cancel)?;
                let mut batches = self.reader.materialize_phase2_mbs(
                    cur.rg_idx,
                    wave_mbs,
                    &cur.plan,
                    &self.proj_indices,
                    &self.proj_schema,
                    &self.map_proj_by_col,
                    &resolved,
                    &cur.block_reader,
                    self.read_threads,
                )?;
                if let Some(batch) = batches.pop() {
                    return Ok(Some((batch, first_row)));
                }
                continue;
            }

            // 2. Pull the next single mini-block from the active RG.
            let next_mb: Option<(ActiveMiniBlock, Option<RetainedChunkMap>)> = match &mut cur.phase
            {
                ActiveRgPhase::PointEval { surviving_mbs } => {
                    let mut found = None;
                    while let Some(mb_idx) = surviving_mbs.pop_front() {
                        check_cancel(cancel)?;
                        let retained = RetainedPackChunks::default();
                        let (mut mask, decoded_p1) = self.reader.eval_phase1_mb(
                            cur.rg_idx,
                            mb_idx,
                            &cur.plan,
                            &self.proj_indices,
                            self.time_col_idx,
                            self.time_range,
                            &lookup_refs,
                            &pred_refs,
                            &self.resolved_filter_exprs,
                            &self.resolved_subs,
                            &self.dict_sub_bitmaps,
                            cur.l0_parsed.as_deref(),
                            &cur.block_reader,
                            self.read_threads,
                            &retained,
                            None,
                            cancel,
                        )?;
                        let hits = mask.count_ones();
                        if hits == 0 {
                            continue;
                        }
                        if let Some(rem) = self.remaining_limit
                            && hits > rem
                        {
                            trim_bitmap_mask(&mut mask, rem, self.desc_limit);
                        }
                        found = Some(((mb_idx, mask, decoded_p1), Some(retained.into_map())));
                        break;
                    }
                    found
                }
                ActiveRgPhase::ReadyMbs { active_mbs } => {
                    active_mbs.pop_front().map(|mb| (mb, None))
                }
            };

            let Some((mb, local_retained)) = next_mb else {
                return Ok(None);
            };
            let mb_idx = mb.0;
            let first_row = u64::from(self.reader.meta.rg_blocks(0, cur.rg_idx)[mb_idx].first_row);

            check_cancel(cancel)?;
            if !self.has_payload_proj {
                let mut batches = self.reader.materialize_phase2_mbs(
                    cur.rg_idx,
                    vec![mb],
                    &cur.plan,
                    &self.proj_indices,
                    &self.proj_schema,
                    &self.map_proj_by_col,
                    &ResolvedChunks::empty(),
                    &cur.block_reader,
                    self.read_threads,
                )?;
                if let Some(batch) = batches.pop() {
                    return Ok(Some((batch, first_row)));
                }
                continue;
            }

            let mut single = vec![mb];
            self.reader.ensure_rg_payload_refs_decoded(
                cur.rg_idx,
                &mut single,
                &self.proj_payload_cols,
                &cur.block_reader,
                self.read_threads,
            )?;
            check_cancel(cancel)?;

            match self.reader.plan_payload_waves(
                &single,
                &self.proj_payload_cols,
                cur.l0_parsed.as_deref(),
            )? {
                PayloadWavePlan::SingleWave { by_pack, .. } => {
                    let pre_resolved = local_retained.as_ref().unwrap_or(&cur.retained_chunks);
                    let resolved = self.reader.resolve_rg_payload_chunks_from_by_pack(
                        by_pack,
                        cur.l0_parsed.as_deref(),
                        pre_resolved,
                        self.read_threads,
                    )?;
                    check_cancel(cancel)?;
                    let mut batches = self.reader.materialize_phase2_mbs(
                        cur.rg_idx,
                        single,
                        &cur.plan,
                        &self.proj_indices,
                        &self.proj_schema,
                        &self.map_proj_by_col,
                        &resolved,
                        &cur.block_reader,
                        self.read_threads,
                    )?;
                    if let Some(batch) = batches.pop() {
                        return Ok(Some((batch, first_row)));
                    }
                }
                PayloadWavePlan::MultiWave(waves) => {
                    let sub_masks: VecDeque<Bitmap> = waves
                        .into_iter()
                        .flat_map(|w| w.into_iter().map(|(_, m)| m))
                        .collect();
                    if sub_masks.is_empty() {
                        continue;
                    }
                    let Some((mb_idx, _, decoded_p1)) = single.pop() else {
                        continue;
                    };
                    cur.pending_mb_waves = Some(PendingMbWaves {
                        mb_idx,
                        decoded_p1,
                        sub_masks,
                        local_retained,
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Arc;

    use cetra_column::{
        Batch, CancelToken, Column, DataType, Field, FlatColumn, FlatData, GlobalPool, MapData,
        ScalarValue, Schema, Utf8Data,
    };

    use crate::StatamError;
    use crate::codec::{WriteProfile, ZonePred, ZoneValue};
    use crate::format::{DEFAULT_TAIL_HINT, TableKind};
    use crate::io::{SimulatedS3Config, SimulatedS3Reader, SliceReader};
    use crate::payload::pack::{ChunkPackBuilder, ChunkPackReader, PackToc, PackWriteOptions};
    use crate::reader::{
        CoalesceMode, IdLookup, InMemoryPackResolver, Predicate, ScanRequest, StatamFileMeta,
        StatamReader, SubstringQuery, TimeRange,
    };
    use crate::writer::{StatamWriter, WriteOptions};

    fn test_schema() -> Arc<Schema> {
        Schema::try_new_arc(vec![
            Field::new("start_time", DataType::TimestampNs { utc: true }, false),
            Field::new("trace_id", DataType::FixedBytes16, false),
            Field::new("status", DataType::Utf8, false),
            Field::new("name", DataType::Utf8, false),
            Field::new("metadata", DataType::MapUtf8, true),
            Field::new("input", DataType::Utf8, true),
        ])
        .unwrap()
    }

    type FixtureBytes = (Vec<u8>, Option<([u8; 16], Vec<u8>)>);

    fn build_multi_rg_fixture(profile: WriteProfile) -> FixtureBytes {
        let schema = test_schema();
        let n = 300usize;
        let mut ts = Vec::with_capacity(n);
        let mut tids: Vec<[u8; 16]> = Vec::with_capacity(n);
        let mut statuses = Vec::with_capacity(n);
        let mut names = Vec::with_capacity(n);
        let mut map_offsets = vec![0u32];
        let mut map_keys = Vec::new();
        let mut map_vals = Vec::new();
        let mut inputs = Vec::with_capacity(n);

        for i in 0..n {
            ts.push(10_000 + i as i64 * 100);
            let mut tid = [0u8; 16];
            tid[0..8].copy_from_slice(&(i as u64 + 1).to_le_bytes());
            tids.push(tid);
            statuses.push(if i % 3 == 0 { "ERROR" } else { "OK" });
            names.push(format!("span-op-{}", i % 10));
            map_keys.push("env");
            map_vals.push(if i % 2 == 0 { "prod" } else { "staging" });
            map_keys.push("tier");
            map_vals.push("gold");
            map_offsets.push(map_keys.len() as u32);
            if i % 15 == 0 {
                inputs.push(format!("NEEDLE_HIT_{i}_payload_{}", "x".repeat(4_000)));
            } else {
                inputs.push(format!("normal_row_{i}_payload_{}", "y".repeat(3_000)));
            }
        }

        let map_data = MapData::try_new(
            map_offsets,
            Utf8Data::try_from_slices(map_keys).unwrap(),
            Utf8Data::try_from_slices(map_vals).unwrap(),
            None,
        )
        .unwrap();

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
                Column::Flat(FlatColumn::try_new(FlatData::FixedBytes16(tids), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(statuses).unwrap()),
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
                    FlatColumn::try_new(FlatData::MapUtf8(Box::new(map_data)), None).unwrap(),
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

        let mut opts = WriteOptions::for_table(TableKind::Spans, profile);
        opts.sort_cols = vec![0];
        opts.row_group_size = 96;
        opts.mini_block_size = 32;

        if profile == WriteProfile::L1Compact {
            let pack_id = [0x42u8; 16];
            opts.pack_id = Some(pack_id);
            let pb = ChunkPackBuilder::new_streaming(
                pack_id,
                PackWriteOptions {
                    target_frame_raw_bytes: 32 * 1024,
                    ..PackWriteOptions::default()
                },
                Cursor::new(Vec::new()),
            )
            .unwrap();
            let mut writer =
                StatamWriter::new_with_owned_pack(Cursor::new(Vec::new()), schema, opts, Some(pb))
                    .unwrap();
            writer.write_batch(&batch).unwrap();
            let (statam_cursor, _stats, owned_pack) = writer.finish_into_inner().unwrap();
            let (pack_cursor, _pstats) = owned_pack.unwrap().finish_into_inner().unwrap();
            (
                statam_cursor.into_inner(),
                Some((pack_id, pack_cursor.into_inner())),
            )
        } else {
            let mut out = Vec::new();
            let mut writer = StatamWriter::new(Cursor::new(&mut out), schema, opts, None).unwrap();
            writer.write_batch(&batch).unwrap();
            writer.finish().unwrap();
            (out, None)
        }
    }

    fn flatten_rows(batches: &[Batch]) -> Vec<Vec<ScalarValue>> {
        let mut rows = Vec::new();
        for b in batches {
            let c = b.compact(None).unwrap();
            for r in 0..c.num_rows() {
                let mut vals = Vec::with_capacity(c.columns().len());
                for col in c.columns() {
                    vals.push(col.get_scalar(r).unwrap());
                }
                rows.push(vals);
            }
        }
        rows
    }

    #[test]
    fn scan_iter_matches_scan_exact_parity_across_modes() {
        for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
            let (statam_bytes, pack_opt) = build_multi_rg_fixture(profile);
            let mut resolver = InMemoryPackResolver::new();
            if let Some((pid, pbytes)) = pack_opt {
                resolver.insert_bytes(pid, pbytes).unwrap();
            }
            let reader = Arc::new(
                StatamReader::open(
                    Arc::new(SliceReader::new(statam_bytes)),
                    Some(Arc::new(resolver)),
                )
                .unwrap(),
            );

            let mut target_tid = [0u8; 16];
            target_tid[0..8].copy_from_slice(&42u64.to_le_bytes());

            let requests = vec![
                // 1. Unfiltered full scan
                ScanRequest::default(),
                // 2. 0-column count scan
                ScanRequest {
                    projection: Some(vec![]),
                    ..ScanRequest::default()
                },
                // 3. Narrow predicate + time range + map_keys shredded projection
                ScanRequest {
                    projection: Some(vec![0, 2, 4]),
                    time_range: TimeRange::new(15_000, 35_000),
                    predicates: vec![Predicate::new(
                        "status",
                        ZonePred::Eq(ZoneValue::Bytes(b"ERROR".to_vec())),
                    )],
                    ..ScanRequest::default()
                }
                .with_map_keys("metadata", vec!["env".to_owned()]),
                // 4. Point lookup via SBBF bloom
                ScanRequest {
                    id_lookups: vec![IdLookup::eq("trace_id", target_tid)],
                    coalesce_mode: CoalesceMode::Point,
                    ..ScanRequest::default()
                },
                // 5. Payload substring search
                ScanRequest {
                    projection: Some(vec![0, 5]),
                    substrings: vec![vec![SubstringQuery::like("input", b"NEEDLE_HIT_")]],
                    ..ScanRequest::default()
                },
                // 6. Narrow substring search
                ScanRequest {
                    projection: Some(vec![0, 3]),
                    substrings: vec![vec![SubstringQuery::like("name", b"op-7")]],
                    ..ScanRequest::default()
                },
                // 7. Reverse early-stop sort_desc_hint + limit
                ScanRequest {
                    projection: Some(vec![0, 2]),
                    sort_desc_hint: true,
                    limit: Some(50),
                    ..ScanRequest::default()
                },
                // 8. Forward limit with filter
                ScanRequest {
                    projection: Some(vec![0, 2, 5]),
                    predicates: vec![Predicate::new(
                        "status",
                        ZonePred::Eq(ZoneValue::Bytes(b"ERROR".to_vec())),
                    )],
                    limit: Some(25),
                    ..ScanRequest::default()
                },
            ];

            for (idx, req) in requests.into_iter().enumerate() {
                let expected = reader.scan(&req).unwrap();
                let mut iter = reader.scan_iter(req).unwrap();
                let mut actual = Vec::new();
                while let Some(b) = iter.next_batch(None, None).unwrap() {
                    actual.push(b);
                }
                assert!(iter.next_batch(None, None).unwrap().is_none());
                assert_eq!(
                    flatten_rows(&actual),
                    flatten_rows(&expected),
                    "parity mismatch for profile={profile:?} req_idx={idx}"
                );
                let exp_rows: usize = expected.iter().map(Batch::num_rows).sum();
                let act_rows: usize = actual.iter().map(Batch::num_rows).sum();
                assert_eq!(act_rows, exp_rows);
            }
        }
    }

    #[test]
    fn scan_iter_cancel_token_and_reservation_accounting_and_oom_rollback() {
        let (statam_bytes, pack_opt) = build_multi_rg_fixture(WriteProfile::L1Compact);
        let mut resolver = InMemoryPackResolver::new();
        if let Some((pid, pbytes)) = pack_opt {
            resolver.insert_bytes(pid, pbytes).unwrap();
        }
        let s3 = Arc::new(SimulatedS3Reader::new(
            Arc::new(SliceReader::new(statam_bytes)),
            SimulatedS3Config::default(),
        ));
        let reader = Arc::new(
            StatamReader::open(
                Arc::clone(&s3) as Arc<dyn crate::RangeReader>,
                Some(Arc::new(resolver)),
            )
            .unwrap(),
        );

        // 1. Pre-cancelled token returns Cancelled with ZERO extra GETs.
        let gets_before = s3.counters().gets;
        let pre_cancel = CancelToken::new();
        pre_cancel.cancel();
        let mut iter = reader.scan_iter(ScanRequest::default()).unwrap();
        let err = iter.next_batch(Some(&pre_cancel), None).unwrap_err();
        assert!(matches!(err, StatamError::Cancelled));
        assert!(!err.is_corrupt());
        assert_eq!(s3.counters().gets, gets_before);

        // 2. Reservation exact postcondition on every yielded batch + Ok(None).
        let scoped = GlobalPool::standalone(64 * 1024 * 1024);
        let mut res = scoped.register("scan_test", false);
        let mut iter = reader
            .scan_iter(ScanRequest {
                projection: Some(vec![0, 2, 5]),
                ..ScanRequest::default()
            })
            .unwrap();
        let mut batch_count = 0usize;
        let mut total_rows = 0usize;
        let cancel = CancelToken::new();
        loop {
            let before = res.size();
            match iter.next_batch(Some(&cancel), Some(&mut res)).unwrap() {
                Some(b) => {
                    let expected_after = before + b.memory_bytes();
                    assert_eq!(res.size(), expected_after);
                    batch_count += 1;
                    total_rows += b.num_rows();
                    // Simulate downstream operator releasing batch memory.
                    res.shrink(b.memory_bytes());
                    assert_eq!(res.size(), before);
                    // Cancel mid-scan after 2 batches to test mid-stream cancellation.
                    if batch_count == 2 {
                        cancel.cancel();
                        let mid_before = res.size();
                        let err = iter.next_batch(Some(&cancel), Some(&mut res)).unwrap_err();
                        assert!(matches!(err, StatamError::Cancelled));
                        assert_eq!(res.size(), mid_before);
                        break;
                    }
                }
                None => {
                    assert_eq!(res.size(), before);
                    break;
                }
            }
        }
        assert_eq!(batch_count, 2);
        assert!(total_rows > 0 && total_rows < 300);

        // 3. OOM rollback: tiny pool rejects try_grow -> MemoryExceeded and res.size() unchanged.
        let tiny_scoped = GlobalPool::standalone(128);
        let mut tiny_res = tiny_scoped.register("tiny_scan", false);
        tiny_res.try_grow(32).unwrap();
        let before_oom = tiny_res.size();
        let mut iter = reader.scan_iter(ScanRequest::default()).unwrap();
        let err = iter.next_batch(None, Some(&mut tiny_res)).unwrap_err();
        assert!(matches!(err, StatamError::MemoryExceeded));
        assert!(!err.is_corrupt());
        assert_eq!(tiny_res.size(), before_oom);
    }

    #[test]
    fn external_metadata_cache_hooks_zero_get_open_and_hints() {
        let (statam_bytes, pack_opt) = build_multi_rg_fixture(WriteProfile::L1Compact);
        let (pack_id, pack_bytes) = pack_opt.unwrap();

        // 1. StatamFileMeta::read_from_with_hint on local vs remote reader.
        let local_rr = Arc::new(SliceReader::new(statam_bytes.clone()));
        let meta_local = StatamFileMeta::read_from_with_hint(local_rr.as_ref(), Some(512)).unwrap();
        assert!(meta_local.memory_bytes() > 0);
        assert_eq!(meta_local.memory_bytes(), meta_local.memory_bytes());

        let s3_statam = Arc::new(SimulatedS3Reader::new(
            Arc::new(SliceReader::new(statam_bytes.clone())),
            SimulatedS3Config::default(),
        ));
        let meta_remote =
            Arc::new(StatamFileMeta::read_from_with_hint(s3_statam.as_ref(), Some(512)).unwrap());
        // On remote, hint expands upward to DEFAULT_TAIL_HINT so small files open in 1 GET.
        assert_eq!(s3_statam.counters().gets, 1);
        assert!(
            s3_statam.counters().bytes_requested
                <= (DEFAULT_TAIL_HINT as u64).min(statam_bytes.len() as u64)
        );

        // 2. PackToc::read_from_with_hint + ChunkPackReader::open_with_toc (zero GETs).
        let s3_pack = Arc::new(SimulatedS3Reader::new(
            Arc::new(SliceReader::new(pack_bytes.clone())),
            SimulatedS3Config::default(),
        ));
        let (toc, _retained) = PackToc::read_from_with_hint(s3_pack.as_ref(), Some(256)).unwrap();
        assert_eq!(s3_pack.counters().gets, 1);
        assert!(toc.memory_bytes() > 0);
        assert_eq!(toc.postscript.pack_id, pack_id);

        let pack_gets_before = s3_pack.counters().gets;
        let cached_pack = Arc::new(
            ChunkPackReader::open_with_toc(
                Arc::clone(&s3_pack) as Arc<dyn crate::RangeReader>,
                Arc::clone(&toc),
            )
            .unwrap(),
        );
        assert_eq!(s3_pack.counters().gets, pack_gets_before);

        // 3. StatamReader::open_with_shared_meta opens with zero GETs and scans identically.
        let mut resolver = InMemoryPackResolver::new();
        resolver.insert(pack_id, cached_pack);
        let statam_gets_before = s3_statam.counters().gets;
        let cached_reader = StatamReader::open_with_shared_meta(
            Arc::clone(&s3_statam) as Arc<dyn crate::RangeReader>,
            Arc::clone(&meta_remote),
            Some(Arc::new(resolver)),
        )
        .unwrap();
        assert_eq!(s3_statam.counters().gets, statam_gets_before);

        let fresh_reader = StatamReader::open(
            Arc::new(SliceReader::new(statam_bytes)),
            Some(Arc::new({
                let mut r = InMemoryPackResolver::new();
                r.insert_bytes(pack_id, pack_bytes).unwrap();
                r
            })),
        )
        .unwrap();

        let expected = fresh_reader.scan(&ScanRequest::default()).unwrap();
        let actual = cached_reader.scan(&ScanRequest::default()).unwrap();
        assert_eq!(flatten_rows(&actual), flatten_rows(&expected));
    }

    #[test]
    fn m35_next_batch_with_row_ids_exact_parity_and_take_rows_roundtrip() {
        for profile in [WriteProfile::L0Fast, WriteProfile::L1Compact] {
            let (statam_bytes, pack_opt) = build_multi_rg_fixture(profile);
            let mut resolver = InMemoryPackResolver::new();
            if let Some((pid, pbytes)) = pack_opt {
                resolver.insert_bytes(pid, pbytes).unwrap();
            }
            let reader = Arc::new(
                StatamReader::open(
                    Arc::new(SliceReader::new(statam_bytes)),
                    Some(Arc::new(resolver)),
                )
                .unwrap(),
            );

            let requests: Vec<(bool, ScanRequest)> = vec![
                (false, ScanRequest::default()),
                (
                    false,
                    ScanRequest {
                        projection: Some(vec![0, 2, 5]),
                        predicates: vec![Predicate::new(
                            "status",
                            ZonePred::Eq(ZoneValue::Bytes(b"ERROR".to_vec())),
                        )],
                        ..ScanRequest::default()
                    },
                ),
                (
                    false,
                    ScanRequest {
                        projection: Some(vec![0, 2, 3]),
                        predicates: vec![Predicate::new(
                            "status",
                            ZonePred::Eq(ZoneValue::Bytes(b"ERROR".to_vec())),
                        )],
                        limit: Some(25),
                        ..ScanRequest::default()
                    },
                ),
                (
                    true,
                    ScanRequest {
                        projection: Some(vec![0, 2]),
                        sort_desc_hint: true,
                        limit: Some(50),
                        ..ScanRequest::default()
                    },
                ),
                (
                    false,
                    ScanRequest {
                        projection: Some(vec![0, 3, 5]),
                        rg_indices: Some(vec![1, 2]),
                        ..ScanRequest::default()
                    },
                ),
            ];

            let scoped = GlobalPool::standalone(64 * 1024 * 1024);
            let mut res = scoped.register("row_ids_test", false);

            for (idx, (is_desc, req)) in requests.into_iter().enumerate() {
                let proj = req.projection.clone();
                let mut iter = reader.scan_iter(req).unwrap();
                let mut batches = Vec::new();
                let mut all_ids: Vec<u64> = Vec::new();
                while let Some((b, ids)) =
                    iter.next_batch_with_row_ids(None, Some(&mut res)).unwrap()
                {
                    assert_eq!(
                        ids.len(),
                        b.num_rows(),
                        "row_ids len mismatch profile={profile:?} idx={idx}"
                    );
                    res.shrink(b.memory_bytes());
                    assert_eq!(res.size(), 0);
                    all_ids.extend_from_slice(&ids);
                    batches.push(b);
                }
                assert!(
                    iter.next_batch_with_row_ids(None, Some(&mut res))
                        .unwrap()
                        .is_none()
                );
                assert_eq!(res.size(), 0);
                assert!(!all_ids.is_empty());

                let taken = reader.take_rows(&all_ids, proj.as_deref(), 1).unwrap();
                let mut got_rows = flatten_rows(&batches);
                let mut taken_rows = flatten_rows(&taken);
                if is_desc {
                    // Reverse-order scan yields MBs in descending order while take_rows returns
                    // ascending global file order; sort both by start_time (col 0) to verify exact
                    // row-set identity.
                    got_rows.sort_by_key(|r| match &r[0] {
                        ScalarValue::TimestampNs { value, .. } => *value,
                        _ => unreachable!(),
                    });
                    taken_rows.sort_by_key(|r| match &r[0] {
                        ScalarValue::TimestampNs { value, .. } => *value,
                        _ => unreachable!(),
                    });
                }
                assert_eq!(
                    got_rows, taken_rows,
                    "take_rows(row_ids) mismatch profile={profile:?} idx={idx}"
                );
            }
        }
    }

    #[test]
    fn m35_scan_iter_rg_wide_stpk_frame_dedup_limit_and_cancel() {
        // 1 RG with 6 mini-blocks (192 rows total), all sharing a small number of `.stpk` frames.
        let schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("input", DataType::Utf8, true),
        ])
        .unwrap();
        let n = 192usize;
        let ids: Vec<i64> = (0..n as i64).collect();
        let inputs: Vec<String> = (0..n)
            .map(|i| {
                if i % 32 == 0 {
                    format!("COMMON_TOKEN_{i}_HIT_{}", "a".repeat(3_500))
                } else {
                    format!("COMMON_TOKEN_{i}_MISS_{}", "b".repeat(3_500))
                }
            })
            .collect();
        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
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

        let pack_id = [0x99u8; 16];
        let mut opts = WriteOptions::for_table(TableKind::Spans, WriteProfile::L1Compact);
        opts.sort_cols = vec![0];
        opts.row_group_size = n;
        opts.mini_block_size = 32; // 6 mini-blocks in 1 RG
        opts.pack_id = Some(pack_id);
        let pb = ChunkPackBuilder::new_streaming(
            pack_id,
            PackWriteOptions {
                target_frame_raw_bytes: 128 * 1024,
                ..PackWriteOptions::default()
            },
            Cursor::new(Vec::new()),
        )
        .unwrap();
        let mut writer =
            StatamWriter::new_with_owned_pack(Cursor::new(Vec::new()), schema, opts, Some(pb))
                .unwrap();
        writer.write_batch(&batch).unwrap();
        let (statam_cursor, _, owned_pack) = writer.finish_into_inner().unwrap();
        let (pack_cursor, _) = owned_pack.unwrap().finish_into_inner().unwrap();
        let statam_bytes = statam_cursor.into_inner();
        let pack_bytes = pack_cursor.into_inner();

        let pack_s3 = Arc::new(SimulatedS3Reader::new(
            Arc::new(SliceReader::new(pack_bytes)),
            SimulatedS3Config::default(),
        ));
        let pack_reader = Arc::new(
            ChunkPackReader::open(Arc::clone(&pack_s3) as Arc<dyn crate::RangeReader>).unwrap(),
        );
        let mut resolver = InMemoryPackResolver::new();
        resolver.insert(pack_id, pack_reader);
        let reader = Arc::new(
            StatamReader::open(
                Arc::new(SliceReader::new(statam_bytes)),
                Some(Arc::new(resolver)),
            )
            .unwrap(),
        );

        // 1. Unbounded `scan_iter` hoists `.stpk` frame dedup to RG level: total pack bytes read
        //    is strictly bounded (not 6x re-reads of the same frame across 6 MBs).
        pack_s3.reset_counters();
        let req_all = ScanRequest {
            projection: Some(vec![0]),
            substrings: vec![vec![SubstringQuery::like("input", b"_HIT_")]],
            ..ScanRequest::default()
        };
        let mut iter = reader.scan_iter(req_all.clone()).unwrap();
        let mut got_all = Vec::new();
        while let Some(b) = iter.next_batch(None, None).unwrap() {
            got_all.push(b);
        }
        assert_eq!(got_all.iter().map(Batch::num_rows).sum::<usize>(), 6);
        let unbounded_gets = pack_s3.counters().gets;
        assert!(
            unbounded_gets <= 3,
            "RG-wide .stpk frame dedup should coalesce across all 6 MBs (got {unbounded_gets} GETs)"
        );

        // 2. With `LIMIT 1`, MB-by-MB early exit stops after MB 0 without scanning later frames.
        pack_s3.reset_counters();
        let req_lim = ScanRequest {
            projection: Some(vec![0]),
            substrings: vec![vec![SubstringQuery::like("input", b"_HIT_")]],
            limit: Some(1),
            ..ScanRequest::default()
        };
        let mut iter_lim = reader.scan_iter(req_lim).unwrap();
        let mut got_lim = Vec::new();
        while let Some(b) = iter_lim.next_batch(None, None).unwrap() {
            got_lim.push(b);
        }
        assert_eq!(got_lim.iter().map(Batch::num_rows).sum::<usize>(), 1);
        assert!(
            pack_s3.counters().bytes_requested <= crate::RangeReader::file_len(pack_s3.as_ref()),
            "LIMIT 1 must exit early without redundant reads"
        );

        // 3. Pre-cancelled CancelToken aborts before any pack frame search GETs.
        pack_s3.reset_counters();
        let tok = CancelToken::new();
        tok.cancel();
        let mut iter_cancel = reader.scan_iter(req_all).unwrap();
        let err = iter_cancel.next_batch(Some(&tok), None).unwrap_err();
        assert!(matches!(err, StatamError::Cancelled));
        assert_eq!(pack_s3.counters().gets, 0);
    }
}
