//! Phase 2 late materialization and `take_rows` mini-block decode helpers.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use cetra_column::{Batch, Bitmap, Column, DataType, FlatColumn, ScalarValue, Schema, Selection};

use crate::StatamError;
use crate::codec::dict::FileDict;
use crate::codec::nested::{
    ShreddedSubCol, decode_shredded_residual_block_selected, decode_shredded_subcol_selected,
    decode_single_map_key, unshred_map_selected,
};
use crate::codec::{decode_mini_block_selected, decode_mini_block_take};
use crate::format::{COL_FLAG_HAS_FILE_DICT, Encoding, LocalityGroup, MAX_BATCH_DECODED_BYTES};
use crate::parallel::{MAX_PARALLEL_THREADS, parallel_map_in_order_static};
use crate::payload::{decode_payload_refs_flat, decode_payload_refs_selected};

use super::eval::filter_residual_to_keys;
use super::meta::StatamFileMeta;
use super::payload::{
    ResolvedChunks, reconstruct_payload_column_from_flat, reconstruct_payload_column_selected,
    reconstruct_payload_column_take_from_flat, reconstruct_payload_column_take_selected,
};
use super::plan::{BlockCacheReader, DecodedPhase1, RgIoPlan, TakeMiniBlock};
use super::{StatamReader, effective_read_threads};

#[derive(Clone, Copy)]
enum DictSource<'a> {
    Reader(&'a StatamReader),
    Pre(&'a HashMap<usize, Arc<FileDict>>),
}

impl DictSource<'_> {
    #[inline]
    fn get(&self, cidx: usize) -> Option<Arc<FileDict>> {
        match self {
            Self::Reader(r) => r.get_cached_dict(cidx),
            Self::Pre(map) => map.get(&cidx).map(Arc::clone),
        }
    }
}

impl StatamReader {
    pub(super) fn preload_proj_dicts(
        &self,
        proj_indices: &[usize],
    ) -> Arc<HashMap<usize, Arc<FileDict>>> {
        let caches = self.caches.lock().unwrap_or_else(|p| p.into_inner());
        let mut out = HashMap::new();
        for &cidx in proj_indices {
            if self.meta.col_dir[cidx].locality_group != LocalityGroup::ExternalPayload
                && (self.meta.col_dir[cidx].col_flags & COL_FLAG_HAS_FILE_DICT) != 0
                && !out.contains_key(&cidx)
                && let Some(dict) = caches.dicts.get(&cidx)
            {
                out.insert(cidx, Arc::clone(dict));
            }
        }
        Arc::new(out)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn materialize_phase2_mbs(
        &self,
        rg_idx: usize,
        active_mbs: Vec<(usize, Bitmap, HashMap<usize, DecodedPhase1>)>,
        plan: &Arc<RgIoPlan>,
        proj_indices: &[usize],
        proj_schema: &Arc<Schema>,
        map_keys_by_col: &Arc<HashMap<usize, Vec<String>>>,
        resolved_chunks: &ResolvedChunks,
        read_block: &BlockCacheReader,
        read_threads: usize,
    ) -> Result<Vec<Batch>, StatamError> {
        let threads = read_threads.clamp(1, MAX_PARALLEL_THREADS);
        let mut work_bytes = resolved_chunks.total_bytes() / 4;
        for (mb_idx, _, decoded_p1) in &active_mbs {
            for &pidx in &plan.phase2_only_phys_cols {
                work_bytes = work_bytes
                    .saturating_add(self.meta.rg_blocks(pidx, rg_idx)[*mb_idx].raw_len as usize);
            }
            for &cidx in proj_indices {
                if !decoded_p1.contains_key(&cidx) && !plan.phase2_only_phys_cols.contains(&cidx) {
                    work_bytes = work_bytes.saturating_add(
                        self.meta.rg_blocks(cidx, rg_idx)[*mb_idx].raw_len as usize,
                    );
                }
            }
        }
        let mb_workers = effective_read_threads(work_bytes, threads);
        if mb_workers > 1 && active_mbs.len() >= 2 {
            let inner_threads = if active_mbs.len() >= mb_workers {
                1
            } else {
                (mb_workers / active_mbs.len()).max(1)
            };
            let meta = Arc::clone(&self.meta);
            let plan_arc = Arc::clone(plan);
            let proj_indices_arc = Arc::new(proj_indices.to_vec());
            let proj_schema_arc = Arc::clone(proj_schema);
            let map_keys_arc = Arc::clone(map_keys_by_col);
            let resolved_arc = resolved_chunks.clone();
            let pre_dicts = self.preload_proj_dicts(proj_indices);
            let block_reader = read_block.clone();
            return parallel_map_in_order_static(
                active_mbs,
                mb_workers,
                move |(mb_idx, mask, decoded_p1)| {
                    materialize_phase2_mb_with_meta(
                        &meta,
                        rg_idx,
                        mb_idx,
                        &mask,
                        decoded_p1,
                        &plan_arc,
                        &proj_indices_arc,
                        &proj_schema_arc,
                        &map_keys_arc,
                        &resolved_arc,
                        DictSource::Pre(&pre_dicts),
                        &block_reader,
                        inner_threads,
                    )
                },
            );
        }

        let mut batches = Vec::with_capacity(active_mbs.len());
        for (mb_idx, mask, decoded_p1) in active_mbs {
            let batch = materialize_phase2_mb_with_meta(
                &self.meta,
                rg_idx,
                mb_idx,
                &mask,
                decoded_p1,
                plan,
                proj_indices,
                proj_schema,
                map_keys_by_col,
                resolved_chunks,
                DictSource::Reader(self),
                read_block,
                read_threads,
            )?;
            batches.push(batch);
        }
        Ok(batches)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn materialize_take_mbs(
        &self,
        rg_idx: usize,
        take_mbs: Vec<TakeMiniBlock>,
        plan: &Arc<RgIoPlan>,
        proj_indices: &[usize],
        proj_schema: &Arc<Schema>,
        empty_map_keys: &Arc<HashMap<usize, Vec<String>>>,
        resolved_chunks: &ResolvedChunks,
        read_block: &BlockCacheReader,
        read_threads: usize,
    ) -> Result<Vec<Batch>, StatamError> {
        let threads = read_threads.clamp(1, MAX_PARALLEL_THREADS);
        let mut work_bytes = resolved_chunks.total_bytes() / 4;
        for (mb_idx, _, _, _) in &take_mbs {
            for &pidx in plan.all_phys_cols() {
                work_bytes = work_bytes
                    .saturating_add(self.meta.rg_blocks(pidx, rg_idx)[*mb_idx].raw_len as usize);
            }
        }
        let mb_workers = if proj_indices.len() >= 16 {
            threads
        } else {
            effective_read_threads(work_bytes, threads)
        };
        if mb_workers > 1 && take_mbs.len() >= 2 {
            let inner_threads = if take_mbs.len() >= mb_workers {
                1
            } else {
                (mb_workers / take_mbs.len()).max(1)
            };
            let meta = Arc::clone(&self.meta);
            let plan_arc = Arc::clone(plan);
            let proj_indices_arc = Arc::new(proj_indices.to_vec());
            let proj_schema_arc = Arc::clone(proj_schema);
            let map_keys_arc = Arc::clone(empty_map_keys);
            let resolved_arc = resolved_chunks.clone();
            let pre_dicts = self.preload_proj_dicts(proj_indices);
            let block_reader = read_block.clone();
            return parallel_map_in_order_static(
                take_mbs,
                mb_workers,
                move |(mb_idx, local_indices, mask_opt, payload_p1)| {
                    materialize_take_mb_with_meta(
                        &meta,
                        rg_idx,
                        mb_idx,
                        &local_indices,
                        mask_opt,
                        payload_p1,
                        &plan_arc,
                        &proj_indices_arc,
                        &proj_schema_arc,
                        &map_keys_arc,
                        &resolved_arc,
                        DictSource::Pre(&pre_dicts),
                        &block_reader,
                        inner_threads,
                    )
                },
            );
        }

        let mut batches = Vec::with_capacity(take_mbs.len());
        for (mb_idx, local_indices, mask_opt, payload_p1) in take_mbs {
            let batch = materialize_take_mb_with_meta(
                &self.meta,
                rg_idx,
                mb_idx,
                &local_indices,
                mask_opt,
                payload_p1,
                plan,
                proj_indices,
                proj_schema,
                empty_map_keys,
                resolved_chunks,
                DictSource::Reader(self),
                read_block,
                read_threads,
            )?;
            batches.push(batch);
        }
        Ok(batches)
    }
}

#[allow(clippy::too_many_arguments)]
fn materialize_phase2_mb_with_meta(
    meta: &Arc<StatamFileMeta>,
    rg_idx: usize,
    mb_idx: usize,
    mask: &Bitmap,
    mut decoded_p1: HashMap<usize, DecodedPhase1>,
    plan: &Arc<RgIoPlan>,
    proj_indices: &[usize],
    proj_schema: &Arc<Schema>,
    map_keys_by_col: &Arc<HashMap<usize, Vec<String>>>,
    resolved_chunks: &ResolvedChunks,
    dict_source: DictSource<'_>,
    read_block: &BlockCacheReader,
    read_threads: usize,
) -> Result<Batch, StatamError> {
    let fields = meta.schema_meta.schema.fields();
    let mb_row_count = meta.rg_blocks(0, rg_idx)[mb_idx].row_count as usize;
    let threads = read_threads.clamp(1, MAX_PARALLEL_THREADS);

    if proj_indices.is_empty() {
        let sel = if mask.all_ones() {
            Selection::All
        } else {
            Selection::Mask(mask.clone())
        };
        return Ok(Batch::try_new(
            Arc::clone(proj_schema),
            Vec::new(),
            mb_row_count,
            sel,
        )?);
    }

    // Decode any needed shredded subcols for projected MapUtf8 columns (moving from decoded_p1).
    let mut decoded_subs: HashMap<usize, ShreddedSubCol> = HashMap::new();
    let mut uncached_subs: Vec<usize> = Vec::new();
    for &cidx in proj_indices {
        if fields[cidx].data_type == DataType::MapUtf8
            && let Some(sm) = meta
                .schema_meta
                .shredded_maps
                .iter()
                .find(|s| s.logical_col_idx as usize == cidx)
        {
            let keys_needed: Vec<&str> = match map_keys_by_col.get(&cidx) {
                Some(req_keys) => req_keys.iter().map(String::as_str).collect(),
                None => sm.keys.iter().map(|k| k.key.as_str()).collect(),
            };
            for ke in &sm.keys {
                if !keys_needed.contains(&ke.key.as_str()) {
                    continue;
                }
                let sub_pidx = ke.sub_col_idx as usize;
                if decoded_subs.contains_key(&sub_pidx) || uncached_subs.contains(&sub_pidx) {
                    continue;
                }
                if let Some(DecodedPhase1::ShreddedSub(sub)) = decoded_p1.remove(&sub_pidx) {
                    decoded_subs.insert(sub_pidx, sub);
                } else {
                    uncached_subs.push(sub_pidx);
                }
            }
        }
    }

    if !uncached_subs.is_empty() {
        let sub_work_bytes: usize = uncached_subs
            .iter()
            .map(|&sub_pidx| meta.rg_blocks(sub_pidx, rg_idx)[mb_idx].raw_len as usize)
            .fold(0usize, usize::saturating_add);
        let sub_workers = if uncached_subs.len() >= 8 {
            threads
        } else {
            effective_read_threads(sub_work_bytes, threads)
        };
        if sub_workers > 1 && uncached_subs.len() >= 4 {
            let meta_arc = Arc::clone(meta);
            let block_reader = read_block.clone();
            let mask_arc = Arc::new(mask.clone());
            let decoded_pairs =
                parallel_map_in_order_static(uncached_subs, sub_workers, move |sub_pidx| {
                    let blk = &meta_arc.rg_blocks(sub_pidx, rg_idx)[mb_idx];
                    let raw = block_reader.read_block(blk)?;
                    let sub = decode_shredded_subcol_selected(&raw, mb_row_count, &mask_arc)?;
                    Ok((sub_pidx, sub))
                })?;
            for (sub_pidx, sub) in decoded_pairs {
                decoded_subs.insert(sub_pidx, sub);
            }
        } else {
            for sub_pidx in uncached_subs {
                let blk = &meta.rg_blocks(sub_pidx, rg_idx)[mb_idx];
                let raw = read_block.read_block(blk)?;
                let sub = decode_shredded_subcol_selected(&raw, mb_row_count, mask)?;
                decoded_subs.insert(sub_pidx, sub);
            }
        }
    }

    let is_sparse_payload = !mask.all_ones() && mask.count_ones().saturating_mul(8) < mb_row_count;

    let decode_payload_col = |cidx: usize,
                              cached: Option<DecodedPhase1>|
     -> Result<Column, StatamError> {
        let dt = &fields[cidx].data_type;
        match cached {
            Some(DecodedPhase1::SelectedPayloadRefs(selected)) => {
                reconstruct_payload_column_selected(
                    mb_row_count,
                    &selected,
                    mask,
                    resolved_chunks,
                    dt,
                    threads,
                )
            }
            Some(DecodedPhase1::FlatPayloadRefs(refs)) => {
                reconstruct_payload_column_from_flat(&refs, mask, resolved_chunks, dt, threads)
            }
            _ => {
                let blk = &meta.rg_blocks(cidx, rg_idx)[mb_idx];
                if blk.encoding == Encoding::ConstNull {
                    return Ok(Column::try_new_const(
                        ScalarValue::Null(dt.clone()),
                        mb_row_count,
                    )?);
                }
                let raw = read_block.read_block(blk)?;
                if is_sparse_payload {
                    let selected = decode_payload_refs_selected(
                        &raw,
                        mb_row_count,
                        mask,
                        MAX_BATCH_DECODED_BYTES,
                    )?;
                    reconstruct_payload_column_selected(
                        mb_row_count,
                        &selected,
                        mask,
                        resolved_chunks,
                        dt,
                        threads,
                    )
                } else {
                    let refs =
                        decode_payload_refs_flat(&raw, mb_row_count, MAX_BATCH_DECODED_BYTES)?;
                    reconstruct_payload_column_from_flat(&refs, mask, resolved_chunks, dt, threads)
                }
            }
        }
    };

    let out_cols = if threads <= 1 {
        let mut out_cols = Vec::with_capacity(proj_indices.len());
        for &cidx in proj_indices {
            let dt = &fields[cidx].data_type;
            let cd = &meta.col_dir[cidx];
            if cd.locality_group == LocalityGroup::ExternalPayload {
                let cached = decoded_p1.remove(&cidx);
                out_cols.push(decode_payload_col(cidx, cached)?);
                continue;
            }
            if let Some(cached) = decoded_p1.remove(&cidx) {
                out_cols.push(materialize_cached_p1_col_with_meta(
                    meta,
                    mb_row_count,
                    cidx,
                    dt,
                    mask,
                    cached,
                    plan,
                    mb_idx,
                    map_keys_by_col,
                    &decoded_subs,
                )?);
                continue;
            }
            let blk = &meta.rg_blocks(cidx, rg_idx)[mb_idx];
            let dict = if blk.encoding == Encoding::Dict {
                dict_source.get(cidx)
            } else {
                None
            };
            out_cols.push(decode_uncached_p2_non_payload_col_with_meta(
                meta,
                rg_idx,
                mb_idx,
                mb_row_count,
                cidx,
                mask,
                plan,
                map_keys_by_col,
                &decoded_subs,
                dict.as_deref(),
                read_block,
            )?);
        }
        out_cols
    } else {
        // Pre-populate cached FileDicts on main thread so parallel workers never touch `caches` Mutex.
        let mut pre_dicts: HashMap<usize, Arc<FileDict>> = HashMap::new();
        for &cidx in proj_indices {
            let cd = &meta.col_dir[cidx];
            if cd.locality_group != LocalityGroup::ExternalPayload && !pre_dicts.contains_key(&cidx)
            {
                let blk = &meta.rg_blocks(cidx, rg_idx)[mb_idx];
                if blk.encoding == Encoding::Dict
                    && let Some(dict) = dict_source.get(cidx)
                {
                    pre_dicts.insert(cidx, dict);
                }
            }
        }

        let mut slots: Vec<Option<Column>> = vec![None; proj_indices.len()];
        let mut parallel_tasks: Vec<(usize, usize, Option<Arc<FileDict>>)> = Vec::new();
        for (pos, &cidx) in proj_indices.iter().enumerate() {
            let dt = &fields[cidx].data_type;
            let cd = &meta.col_dir[cidx];
            if cd.locality_group == LocalityGroup::ExternalPayload {
                let cached = decoded_p1.remove(&cidx);
                slots[pos] = Some(decode_payload_col(cidx, cached)?);
            } else if let Some(cached) = decoded_p1.remove(&cidx) {
                slots[pos] = Some(materialize_cached_p1_col_with_meta(
                    meta,
                    mb_row_count,
                    cidx,
                    dt,
                    mask,
                    cached,
                    plan,
                    mb_idx,
                    map_keys_by_col,
                    &decoded_subs,
                )?);
            } else {
                parallel_tasks.push((pos, cidx, pre_dicts.get(&cidx).map(Arc::clone)));
            }
        }

        let task_work_bytes: usize = parallel_tasks
            .iter()
            .map(|&(_, cidx, _)| meta.rg_blocks(cidx, rg_idx)[mb_idx].raw_len as usize)
            .fold(0usize, usize::saturating_add);
        let task_workers = if parallel_tasks.len() >= 16 {
            threads
        } else {
            effective_read_threads(task_work_bytes, threads)
        };
        if parallel_tasks.len() >= 4 && task_workers > 1 {
            let meta_arc = Arc::clone(meta);
            let plan_arc = Arc::clone(plan);
            let map_keys_arc = Arc::clone(map_keys_by_col);
            let decoded_subs_arc = Arc::new(decoded_subs);
            let mask_arc = Arc::new(mask.clone());
            let block_reader = read_block.clone();
            let decoded_cols = parallel_map_in_order_static(
                parallel_tasks,
                task_workers,
                move |(pos, cidx, dict)| {
                    let col = decode_uncached_p2_non_payload_col_with_meta(
                        &meta_arc,
                        rg_idx,
                        mb_idx,
                        mb_row_count,
                        cidx,
                        &mask_arc,
                        &plan_arc,
                        &map_keys_arc,
                        &decoded_subs_arc,
                        dict.as_deref(),
                        &block_reader,
                    )?;
                    Ok((pos, col))
                },
            )?;
            for (pos, col) in decoded_cols {
                slots[pos] = Some(col);
            }
        } else {
            for (pos, cidx, dict) in parallel_tasks {
                slots[pos] = Some(decode_uncached_p2_non_payload_col_with_meta(
                    meta,
                    rg_idx,
                    mb_idx,
                    mb_row_count,
                    cidx,
                    mask,
                    plan,
                    map_keys_by_col,
                    &decoded_subs,
                    dict.as_deref(),
                    read_block,
                )?);
            }
        }

        slots
            .into_iter()
            .map(|s| {
                s.ok_or(StatamError::Corrupt {
                    reason: "missing materialized column",
                })
            })
            .collect::<Result<Vec<_>, _>>()?
    };

    let sel = if mask.all_ones() {
        Selection::All
    } else {
        Selection::Mask(mask.clone())
    };
    Ok(Batch::try_new(
        Arc::clone(proj_schema),
        out_cols,
        mb_row_count,
        sel,
    )?)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn materialize_cached_p1_col_with_meta(
    meta: &StatamFileMeta,
    mb_row_count: usize,
    cidx: usize,
    dt: &DataType,
    mask: &Bitmap,
    cached: DecodedPhase1,
    plan: &RgIoPlan,
    mb_idx: usize,
    map_keys_by_col: &HashMap<usize, Vec<String>>,
    decoded_subs: &HashMap<usize, ShreddedSubCol>,
) -> Result<Column, StatamError> {
    let DecodedPhase1::Col(col) = cached else {
        return Err(StatamError::Corrupt {
            reason: "unexpected Phase 1 cache variant for Phase 2 column",
        });
    };
    if *dt == DataType::MapUtf8
        && let Some(sm) = meta
            .schema_meta
            .shredded_maps
            .iter()
            .find(|s| s.logical_col_idx as usize == cidx)
    {
        let skip_residual = plan.phase2_map_fast_path.contains(&(mb_idx, cidx));
        let residual_flat = if skip_residual {
            None
        } else {
            Some(col.materialize(None)?)
        };
        return build_shredded_map_col_with_meta(
            mb_row_count,
            cidx,
            sm,
            skip_residual,
            residual_flat.as_ref(),
            mask,
            map_keys_by_col,
            decoded_subs,
        );
    }
    if *dt == DataType::MapUtf8
        && let Some(req_keys) = map_keys_by_col.get(&cidx)
    {
        let flat = col.materialize(None)?;
        return Ok(Column::Flat(filter_residual_to_keys(&flat, req_keys)?));
    }
    Ok(col)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn decode_uncached_p2_non_payload_col_with_meta(
    meta: &StatamFileMeta,
    rg_idx: usize,
    mb_idx: usize,
    mb_row_count: usize,
    cidx: usize,
    mask: &Bitmap,
    plan: &RgIoPlan,
    map_keys_by_col: &HashMap<usize, Vec<String>>,
    decoded_subs: &HashMap<usize, ShreddedSubCol>,
    dict: Option<&FileDict>,
    read_block: &BlockCacheReader,
) -> Result<Column, StatamError> {
    let dt = &meta.schema_meta.schema.fields()[cidx].data_type;
    if *dt == DataType::MapUtf8
        && let Some(sm) = meta
            .schema_meta
            .shredded_maps
            .iter()
            .find(|s| s.logical_col_idx as usize == cidx)
    {
        let skip_residual = plan.phase2_map_fast_path.contains(&(mb_idx, cidx));
        let residual_flat = if skip_residual {
            None
        } else {
            let blk = &meta.rg_blocks(cidx, rg_idx)[mb_idx];
            let raw = if blk.stored_len > 0 {
                read_block.read_block(blk)?
            } else {
                Cow::Borrowed(&[][..])
            };
            Some(decode_shredded_residual_block_selected(&raw, blk, mask)?)
        };
        return build_shredded_map_col_with_meta(
            mb_row_count,
            cidx,
            sm,
            skip_residual,
            residual_flat.as_ref(),
            mask,
            map_keys_by_col,
            decoded_subs,
        );
    }

    let blk = &meta.rg_blocks(cidx, rg_idx)[mb_idx];
    let raw = if blk.stored_len > 0 {
        read_block.read_block(blk)?
    } else {
        Cow::Borrowed(&[][..])
    };
    let col = decode_mini_block_selected(&raw, blk, dt, dict, mask)?;
    if *dt == DataType::MapUtf8
        && let Some(req_keys) = map_keys_by_col.get(&cidx)
    {
        let flat = col.materialize(None)?;
        return Ok(Column::Flat(filter_residual_to_keys(&flat, req_keys)?));
    }
    Ok(col)
}

#[allow(clippy::too_many_arguments)]
fn decode_non_payload_take_with_meta(
    meta: &StatamFileMeta,
    rg_idx: usize,
    mb_idx: usize,
    mb_row_count: usize,
    cidx: usize,
    dict: Option<&FileDict>,
    local_indices: &[usize],
    mask_ref: Option<&Bitmap>,
    u32_indices: Option<&[u32]>,
    plan: &RgIoPlan,
    empty_map_keys: &HashMap<usize, Vec<String>>,
    decoded_subs: &HashMap<usize, ShreddedSubCol>,
    read_block: &BlockCacheReader,
) -> Result<Column, StatamError> {
    let dt = &meta.schema_meta.schema.fields()[cidx].data_type;
    if *dt == DataType::MapUtf8
        && let Some(sm) = meta
            .schema_meta
            .shredded_maps
            .iter()
            .find(|s| s.logical_col_idx as usize == cidx)
    {
        let mask = mask_ref.ok_or(StatamError::Corrupt {
            reason: "missing take_rows shredded map mask",
        })?;
        let u32_idx = u32_indices.ok_or(StatamError::Corrupt {
            reason: "missing take_rows u32 indices",
        })?;
        let skip_residual = plan.phase2_map_fast_path.contains(&(mb_idx, cidx));
        let residual_flat = if skip_residual {
            None
        } else {
            let blk = &meta.rg_blocks(cidx, rg_idx)[mb_idx];
            let raw = if blk.stored_len > 0 {
                read_block.read_block(blk)?
            } else {
                Cow::Borrowed(&[][..])
            };
            Some(decode_shredded_residual_block_selected(&raw, blk, mask)?)
        };
        let full_col = build_shredded_map_col_with_meta(
            mb_row_count,
            cidx,
            sm,
            skip_residual,
            residual_flat.as_ref(),
            mask,
            empty_map_keys,
            decoded_subs,
        )?;
        return Ok(Column::Flat(full_col.take(u32_idx, None)?));
    }
    let blk = &meta.rg_blocks(cidx, rg_idx)[mb_idx];
    let raw = if blk.stored_len > 0 {
        read_block.read_block(blk)?
    } else {
        Cow::Borrowed(&[][..])
    };
    decode_mini_block_take(&raw, blk, dt, dict, local_indices)
}

#[allow(clippy::too_many_arguments)]
fn materialize_take_mb_with_meta(
    meta: &Arc<StatamFileMeta>,
    rg_idx: usize,
    mb_idx: usize,
    local_indices: &[usize],
    mut mask_opt: Option<Bitmap>,
    mut payload_p1: HashMap<usize, DecodedPhase1>,
    plan: &Arc<RgIoPlan>,
    proj_indices: &[usize],
    proj_schema: &Arc<Schema>,
    empty_map_keys: &Arc<HashMap<usize, Vec<String>>>,
    resolved_chunks: &ResolvedChunks,
    dict_source: DictSource<'_>,
    read_block: &BlockCacheReader,
    read_threads: usize,
) -> Result<Batch, StatamError> {
    if proj_indices.is_empty() {
        return Ok(Batch::try_new(
            Arc::clone(proj_schema),
            Vec::new(),
            local_indices.len(),
            Selection::All,
        )?);
    }

    let fields = meta.schema_meta.schema.fields();
    let mb_row_count = meta.rg_blocks(0, rg_idx)[mb_idx].row_count as usize;
    let threads = read_threads.clamp(1, MAX_PARALLEL_THREADS);

    let mut needs_shredded_map = false;
    let mut uncached_subs: Vec<usize> = Vec::new();
    for &cidx in proj_indices {
        if meta.col_dir[cidx].locality_group != LocalityGroup::ExternalPayload
            && fields[cidx].data_type == DataType::MapUtf8
            && let Some(sm) = meta
                .schema_meta
                .shredded_maps
                .iter()
                .find(|s| s.logical_col_idx as usize == cidx)
        {
            needs_shredded_map = true;
            for ke in &sm.keys {
                let sub_pidx = ke.sub_col_idx as usize;
                if !uncached_subs.contains(&sub_pidx) {
                    uncached_subs.push(sub_pidx);
                }
            }
        }
    }

    let mask_ref: Option<&Bitmap> = if needs_shredded_map {
        Some(&*mask_opt.get_or_insert_with(|| mask_from_indices(mb_row_count, local_indices)))
    } else {
        mask_opt.as_ref()
    };
    let u32_indices = if needs_shredded_map {
        let mut out = Vec::with_capacity(local_indices.len());
        for &r in local_indices {
            out.push(u32::try_from(r).map_err(|_| StatamError::BoundsExceeded {
                reason: "take_rows local index overflows u32",
            })?);
        }
        Some(out)
    } else {
        None
    };

    let mut decoded_subs: HashMap<usize, ShreddedSubCol> = HashMap::new();
    if let Some(mask) = mask_ref
        && !uncached_subs.is_empty()
    {
        let sub_work_bytes: usize = uncached_subs
            .iter()
            .map(|&sub_pidx| meta.rg_blocks(sub_pidx, rg_idx)[mb_idx].raw_len as usize)
            .fold(0usize, usize::saturating_add);
        let sub_workers = if uncached_subs.len() >= 8 {
            threads
        } else {
            effective_read_threads(sub_work_bytes, threads)
        };
        if sub_workers > 1 && uncached_subs.len() >= 4 {
            let meta_arc = Arc::clone(meta);
            let block_reader = read_block.clone();
            let mask_arc = Arc::new(mask.clone());
            let decoded_pairs =
                parallel_map_in_order_static(uncached_subs, sub_workers, move |sub_pidx| {
                    let blk = &meta_arc.rg_blocks(sub_pidx, rg_idx)[mb_idx];
                    let raw = block_reader.read_block(blk)?;
                    let sub = decode_shredded_subcol_selected(&raw, mb_row_count, &mask_arc)?;
                    Ok((sub_pidx, sub))
                })?;
            for (sub_pidx, sub) in decoded_pairs {
                decoded_subs.insert(sub_pidx, sub);
            }
        } else {
            for sub_pidx in uncached_subs {
                let blk = &meta.rg_blocks(sub_pidx, rg_idx)[mb_idx];
                let raw = read_block.read_block(blk)?;
                let sub = decode_shredded_subcol_selected(&raw, mb_row_count, mask)?;
                decoded_subs.insert(sub_pidx, sub);
            }
        }
    }

    let decode_payload_take_col = |cidx: usize,
                                   cached: Option<DecodedPhase1>,
                                   mask: Option<&Bitmap>|
     -> Result<Column, StatamError> {
        let dt = &fields[cidx].data_type;
        if local_indices.is_empty() {
            return Ok(Column::Flat(FlatColumn::empty(dt)));
        }
        match cached {
            Some(DecodedPhase1::SelectedPayloadRefs(selected)) => {
                reconstruct_payload_column_take_selected(
                    mb_row_count,
                    &selected,
                    local_indices,
                    resolved_chunks,
                    dt,
                    threads,
                )
            }
            Some(DecodedPhase1::FlatPayloadRefs(refs)) => {
                reconstruct_payload_column_take_from_flat(
                    &refs,
                    local_indices,
                    resolved_chunks,
                    dt,
                    threads,
                )
            }
            _ => {
                let blk = &meta.rg_blocks(cidx, rg_idx)[mb_idx];
                if blk.encoding == Encoding::ConstNull {
                    return Ok(Column::try_new_const(
                        ScalarValue::Null(dt.clone()),
                        local_indices.len(),
                    )?);
                }
                let raw = read_block.read_block(blk)?;
                let is_sparse_payload = local_indices.len().saturating_mul(8) < mb_row_count;
                if is_sparse_payload {
                    let owned_mask;
                    let m = match mask {
                        Some(m) => m,
                        None => {
                            owned_mask = mask_from_indices(mb_row_count, local_indices);
                            &owned_mask
                        }
                    };
                    let selected = decode_payload_refs_selected(
                        &raw,
                        mb_row_count,
                        m,
                        MAX_BATCH_DECODED_BYTES,
                    )?;
                    reconstruct_payload_column_take_selected(
                        mb_row_count,
                        &selected,
                        local_indices,
                        resolved_chunks,
                        dt,
                        threads,
                    )
                } else {
                    let refs =
                        decode_payload_refs_flat(&raw, mb_row_count, MAX_BATCH_DECODED_BYTES)?;
                    reconstruct_payload_column_take_from_flat(
                        &refs,
                        local_indices,
                        resolved_chunks,
                        dt,
                        threads,
                    )
                }
            }
        }
    };

    let out_cols = if threads <= 1 {
        let mut out_cols = Vec::with_capacity(proj_indices.len());
        for &cidx in proj_indices {
            let cd = &meta.col_dir[cidx];
            if cd.locality_group == LocalityGroup::ExternalPayload {
                let cached = payload_p1.remove(&cidx);
                out_cols.push(decode_payload_take_col(cidx, cached, mask_ref)?);
            } else {
                let blk = &meta.rg_blocks(cidx, rg_idx)[mb_idx];
                let dict = if blk.encoding == Encoding::Dict {
                    dict_source.get(cidx)
                } else {
                    None
                };
                out_cols.push(decode_non_payload_take_with_meta(
                    meta,
                    rg_idx,
                    mb_idx,
                    mb_row_count,
                    cidx,
                    dict.as_deref(),
                    local_indices,
                    mask_ref,
                    u32_indices.as_deref(),
                    plan,
                    empty_map_keys,
                    &decoded_subs,
                    read_block,
                )?);
            }
        }
        out_cols
    } else {
        let mut pre_dicts: HashMap<usize, Arc<FileDict>> = HashMap::new();
        for &cidx in proj_indices {
            let cd = &meta.col_dir[cidx];
            if cd.locality_group != LocalityGroup::ExternalPayload && !pre_dicts.contains_key(&cidx)
            {
                let blk = &meta.rg_blocks(cidx, rg_idx)[mb_idx];
                if blk.encoding == Encoding::Dict
                    && let Some(dict) = dict_source.get(cidx)
                {
                    pre_dicts.insert(cidx, dict);
                }
            }
        }

        let mut slots: Vec<Option<Column>> = vec![None; proj_indices.len()];
        let mut parallel_tasks: Vec<(usize, usize, Option<Arc<FileDict>>)> = Vec::new();
        for (pos, &cidx) in proj_indices.iter().enumerate() {
            let cd = &meta.col_dir[cidx];
            if cd.locality_group == LocalityGroup::ExternalPayload {
                let cached = payload_p1.remove(&cidx);
                slots[pos] = Some(decode_payload_take_col(cidx, cached, mask_ref)?);
            } else {
                parallel_tasks.push((pos, cidx, pre_dicts.get(&cidx).map(Arc::clone)));
            }
        }

        let task_work_bytes: usize = parallel_tasks
            .iter()
            .map(|&(_, cidx, _)| meta.rg_blocks(cidx, rg_idx)[mb_idx].raw_len as usize)
            .fold(0usize, usize::saturating_add);
        let task_workers = if parallel_tasks.len() >= 16 {
            threads
        } else {
            effective_read_threads(task_work_bytes, threads)
        };
        if parallel_tasks.len() >= 4 && task_workers > 1 {
            let meta_arc = Arc::clone(meta);
            let plan_arc = Arc::clone(plan);
            let map_keys_arc = Arc::clone(empty_map_keys);
            let decoded_subs_arc = Arc::new(decoded_subs);
            let local_indices_arc = Arc::new(local_indices.to_vec());
            let mask_arc_opt = mask_ref.map(|m| Arc::new(m.clone()));
            let u32_arc_opt = u32_indices.map(Arc::new);
            let block_reader = read_block.clone();
            let decoded_cols = parallel_map_in_order_static(
                parallel_tasks,
                task_workers,
                move |(pos, cidx, dict)| {
                    let col = decode_non_payload_take_with_meta(
                        &meta_arc,
                        rg_idx,
                        mb_idx,
                        mb_row_count,
                        cidx,
                        dict.as_deref(),
                        &local_indices_arc,
                        mask_arc_opt.as_deref(),
                        u32_arc_opt.as_deref().map(Vec::as_slice),
                        &plan_arc,
                        &map_keys_arc,
                        &decoded_subs_arc,
                        &block_reader,
                    )?;
                    Ok((pos, col))
                },
            )?;
            for (pos, col) in decoded_cols {
                slots[pos] = Some(col);
            }
        } else {
            for (pos, cidx, dict) in parallel_tasks {
                slots[pos] = Some(decode_non_payload_take_with_meta(
                    meta,
                    rg_idx,
                    mb_idx,
                    mb_row_count,
                    cidx,
                    dict.as_deref(),
                    local_indices,
                    mask_ref,
                    u32_indices.as_deref(),
                    plan,
                    empty_map_keys,
                    &decoded_subs,
                    read_block,
                )?);
            }
        }

        slots
            .into_iter()
            .map(|s| {
                s.ok_or(StatamError::Corrupt {
                    reason: "missing materialized take column",
                })
            })
            .collect::<Result<Vec<_>, _>>()?
    };

    Ok(Batch::try_new(
        Arc::clone(proj_schema),
        out_cols,
        local_indices.len(),
        Selection::All,
    )?)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_shredded_map_col_with_meta(
    mb_row_count: usize,
    cidx: usize,
    sm: &crate::format::ShreddedMapDir,
    skip_residual: bool,
    residual_flat: Option<&FlatColumn>,
    mask: &Bitmap,
    map_keys_by_col: &HashMap<usize, Vec<String>>,
    decoded_subs: &HashMap<usize, ShreddedSubCol>,
) -> Result<Column, StatamError> {
    let flat_map = if let Some(req_keys) = map_keys_by_col.get(&cidx) {
        if req_keys.len() == 1
            && let Some(ke) = sm.keys.iter().find(|k| k.key == req_keys[0])
        {
            let sub = &decoded_subs[&(ke.sub_col_idx as usize)];
            decode_single_map_key(
                mb_row_count,
                residual_flat.and_then(FlatColumn::nulls),
                &ke.key,
                sub,
                if skip_residual { 0 } else { ke.residual_count },
                residual_flat,
            )?
        } else {
            let mut subpairs: Vec<(&str, &ShreddedSubCol)> = Vec::new();
            for ke in &sm.keys {
                if req_keys.iter().any(|rk| rk == &ke.key)
                    && let Some(sub) = decoded_subs.get(&(ke.sub_col_idx as usize))
                {
                    subpairs.push((ke.key.as_str(), sub));
                }
            }
            let pruned_residual = if skip_residual {
                None
            } else {
                residual_flat
                    .map(|res| filter_residual_to_keys(res, req_keys))
                    .transpose()?
            };
            unshred_map_selected(
                mb_row_count,
                pruned_residual.as_ref().and_then(FlatColumn::nulls),
                pruned_residual.as_ref(),
                &subpairs,
                mask,
            )?
        }
    } else {
        let mut subpairs: Vec<(&str, &ShreddedSubCol)> = Vec::with_capacity(sm.keys.len());
        for ke in &sm.keys {
            if let Some(sub) = decoded_subs.get(&(ke.sub_col_idx as usize)) {
                subpairs.push((ke.key.as_str(), sub));
            }
        }
        unshred_map_selected(
            mb_row_count,
            residual_flat.and_then(FlatColumn::nulls),
            residual_flat,
            &subpairs,
            mask,
        )?
    };
    Ok(Column::Flat(flat_map))
}

pub(super) fn mask_from_indices(row_count: usize, indices: &[usize]) -> Bitmap {
    let mut mask = Bitmap::new_zeroed(row_count);
    for &r in indices {
        let _ = mask.set(r, true);
    }
    mask
}
