//! Local L0 and external `.stpk` payload parsing, chunk resolution, substring scan, and reconstruction.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use cetra_column::{
    Batch, BinaryData, Bitmap, Column, DataType, FlatColumn, FlatData, ScalarValue, Schema,
    Utf8Data,
};

use crate::StatamError;
use crate::format::{
    ChunkLoc, Encoding, LocalityGroup, MAX_BATCH_DECODED_BYTES, MAX_FRAME_RAW_BYTES,
    MAX_ROW_PAYLOAD_BYTES, read_framed_pack_frame,
};
use crate::parallel::MAX_PARALLEL_THREADS;
use crate::payload::pack::ChunkPackReader;
use crate::payload::search::{
    RetainedPackChunks, pack_short_circuits_all, search_pack_rows_flat_multi_masked_with_threads,
};
use crate::payload::{
    FLAT_REF_CHUNKS, FLAT_REF_INLINE, FLAT_REF_NULL, FlatPayloadRefs, PayloadRefRow,
    decode_payload_refs_flat, decode_payload_refs_selected,
};
use crate::writer::{L0_LOCAL_PACK_IDX, L0_PAYLOAD_DIR_MAGIC};

use super::StatamReader;
use super::eval::Utf8Matcher;
use super::plan::{ActiveMiniBlock, BlockReader, DecodedPhase1, RgIoPlan};

/// Maximum projected `ExternalPayload` bytes resolved in a single Phase-2 wave (`128 MiB`).
pub(super) const PHASE2_PAYLOAD_WAVE_TARGET_BYTES: usize = MAX_BATCH_DECODED_BYTES / 2;

impl StatamReader {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn resolve_and_materialize_phase2_mbs<'a>(
        &self,
        rg_idx: usize,
        mut active_mbs: Vec<ActiveMiniBlock>,
        plan: &RgIoPlan,
        proj_indices: &[usize],
        proj_schema: &Arc<Schema>,
        map_keys_by_col: &HashMap<usize, Vec<String>>,
        l0_local: Option<&ParsedL0Payload<'_>>,
        pre_resolved: &HashMap<([u8; 16], u32), Bytes>,
        read_block: &BlockReader<'a>,
        read_threads: usize,
    ) -> Result<Vec<Batch>, StatamError> {
        let mut proj_payload_cols: Vec<usize> = proj_indices
            .iter()
            .copied()
            .filter(|&cidx| {
                self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload
            })
            .collect();
        if proj_payload_cols.is_empty() {
            return self.materialize_phase2_mbs(
                rg_idx,
                active_mbs,
                plan,
                proj_indices,
                proj_schema,
                map_keys_by_col,
                &HashMap::new(),
                read_block,
                read_threads,
            );
        }
        proj_payload_cols.sort_unstable();
        proj_payload_cols.dedup();

        // Decode all projected ExternalPayload `PayloadRefs` mini-blocks once.
        self.ensure_rg_payload_refs_decoded(
            rg_idx,
            &mut active_mbs,
            &proj_payload_cols,
            read_block,
        )?;

        // Partition active rows across `active_mbs` into sequential waves of <= 128 MiB so
        // 1-RG large-blob tables (e.g. `blobs_2k` 368 MiB) scan safely within `MAX_BATCH_DECODED_BYTES`
        // while still rejecting any single row exceeding `MAX_ROW_PAYLOAD_BYTES` or `MAX_BATCH_DECODED_BYTES`.
        let waves = self.partition_payload_waves(&active_mbs, &proj_payload_cols, l0_local)?;
        if waves.len() <= 1 {
            let resolved = self.resolve_rg_payload_chunks(
                rg_idx,
                &mut active_mbs,
                proj_indices,
                l0_local,
                pre_resolved,
                read_block,
                read_threads,
            )?;
            return self.materialize_phase2_mbs(
                rg_idx,
                active_mbs,
                plan,
                proj_indices,
                proj_schema,
                map_keys_by_col,
                &resolved,
                read_block,
                read_threads,
            );
        }

        let mut last_wave_for_pos = vec![0usize; active_mbs.len()];
        for (wave_idx, wave) in waves.iter().enumerate() {
            for &(pos, _) in wave {
                last_wave_for_pos[pos] = wave_idx;
            }
        }
        let mut active_slots: Vec<Option<ActiveMiniBlock>> =
            active_mbs.into_iter().map(Some).collect();
        let mut out_batches = Vec::with_capacity(waves.len());

        for (wave_idx, wave) in waves.into_iter().enumerate() {
            let mut wave_mbs = Vec::with_capacity(wave.len());
            for (pos, sub_mask) in wave {
                if last_wave_for_pos[pos] == wave_idx {
                    if let Some((mb_idx, _, decoded_p1)) = active_slots[pos].take() {
                        wave_mbs.push((mb_idx, sub_mask, decoded_p1));
                    }
                } else if let Some((mb_idx, _, decoded_p1)) = active_slots[pos].as_ref() {
                    wave_mbs.push((*mb_idx, sub_mask, clone_decoded_p1_for_wave(decoded_p1)));
                }
            }
            let resolved = self.resolve_rg_payload_chunks(
                rg_idx,
                &mut wave_mbs,
                proj_indices,
                l0_local,
                pre_resolved,
                read_block,
                read_threads,
            )?;
            let mut wave_batches = self.materialize_phase2_mbs(
                rg_idx,
                wave_mbs,
                plan,
                proj_indices,
                proj_schema,
                map_keys_by_col,
                &resolved,
                read_block,
                read_threads,
            )?;
            out_batches.append(&mut wave_batches);
        }

        Ok(out_batches)
    }

    pub(super) fn ensure_rg_payload_refs_decoded<'a>(
        &self,
        rg_idx: usize,
        active_mbs: &mut [ActiveMiniBlock],
        proj_payload_cols: &[usize],
        read_block: &BlockReader<'a>,
    ) -> Result<(), StatamError> {
        for (mb_idx, mask, decoded_p1) in active_mbs.iter_mut() {
            let mb_row_count = self.meta.rg_blocks(0, rg_idx)[*mb_idx].row_count as usize;
            let is_sparse = !mask.all_ones() && mask.count_ones().saturating_mul(8) < mb_row_count;
            for &cidx in proj_payload_cols {
                if let std::collections::hash_map::Entry::Vacant(e) = decoded_p1.entry(cidx) {
                    let blk = &self.meta.rg_blocks(cidx, rg_idx)[*mb_idx];
                    if blk.encoding == Encoding::ConstNull {
                        if is_sparse {
                            let selected =
                                mask.iter_ones().map(|r| (r, PayloadRefRow::Null)).collect();
                            e.insert(DecodedPhase1::SelectedPayloadRefs(selected));
                        } else {
                            e.insert(DecodedPhase1::FlatPayloadRefs(FlatPayloadRefs::all_null(
                                mb_row_count,
                            )));
                        }
                    } else {
                        let raw = read_block(blk)?;
                        if is_sparse {
                            let selected = decode_payload_refs_selected(
                                &raw,
                                mb_row_count,
                                mask,
                                MAX_BATCH_DECODED_BYTES,
                            )?;
                            e.insert(DecodedPhase1::SelectedPayloadRefs(selected));
                        } else {
                            let refs = decode_payload_refs_flat(
                                &raw,
                                mb_row_count,
                                MAX_BATCH_DECODED_BYTES,
                            )?;
                            e.insert(DecodedPhase1::FlatPayloadRefs(refs));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    pub(super) fn partition_payload_waves(
        &self,
        active_mbs: &[ActiveMiniBlock],
        proj_payload_cols: &[usize],
        l0_local: Option<&ParsedL0Payload<'_>>,
    ) -> Result<Vec<Vec<(usize, Bitmap)>>, StatamError> {
        let mut pack_cache: HashMap<u16, Arc<ChunkPackReader>> = HashMap::new();
        let mut resolve_chunk_and_frame = |pidx: u16,
                                           cid: u32|
         -> Result<(usize, u32, usize), StatamError> {
            if pidx == L0_LOCAL_PACK_IDX {
                let l0 = l0_local.ok_or(StatamError::Corrupt {
                    reason: "missing L0 local payload region",
                })?;
                let loc = l0.chunks.get(cid as usize).ok_or(StatamError::Corrupt {
                    reason: "L0 chunk_id out of bounds",
                })?;
                let (_, _, f_raw) = l0.frames[loc.frame_id as usize];
                Ok((loc.raw_len as usize, loc.frame_id, f_raw as usize))
            } else {
                let pack = match pack_cache.entry(pidx) {
                    std::collections::hash_map::Entry::Occupied(o) => Arc::clone(o.get()),
                    std::collections::hash_map::Entry::Vacant(v) => {
                        let pack_id = *self.meta.schema_meta.pack_ids.get(pidx as usize).ok_or(
                            StatamError::Corrupt {
                                reason: "payload ref pack_idx out of bounds",
                            },
                        )?;
                        let resolved = self.resolve_pack(pack_id)?;
                        v.insert(Arc::clone(&resolved));
                        resolved
                    }
                };
                let toc = pack.toc();
                let loc = toc.chunks.get(cid as usize).ok_or(StatamError::Corrupt {
                    reason: "chunk_id out of bounds",
                })?;
                let f_raw = toc.frames[loc.frame_id as usize].raw_len as usize;
                Ok((loc.raw_len as usize, loc.frame_id, f_raw))
            }
        };

        let num_pcols = proj_payload_cols.len();
        let mut waves: Vec<Vec<(usize, Bitmap)>> = Vec::new();
        let mut cur_wave: Vec<(usize, Bitmap)> = Vec::new();
        let mut wave_seen_chunks: HashSet<(u16, u32)> = HashSet::new();
        let mut wave_seen_frames: HashSet<(u16, u32)> = HashSet::new();
        let mut wave_chunk_bytes = 0usize;
        let mut wave_frame_bytes = 0usize;
        let mut wave_col_decoded = vec![0usize; num_pcols];

        let mut row_col_bytes = vec![0usize; num_pcols];
        let mut row_chunks: Vec<(u16, u32, usize, u32, usize)> = Vec::new();
        let mut row_unique_chunks: HashSet<(u16, u32)> = HashSet::new();
        let mut row_unique_frames: HashSet<(u16, u32)> = HashSet::new();
        let mut seen_new_chunks: HashSet<(u16, u32)> = HashSet::new();
        let mut seen_new_frames: HashSet<(u16, u32)> = HashSet::new();

        for (pos, (_, mask, decoded_p1)) in active_mbs.iter().enumerate() {
            let mb_row_count = mask.len();
            for r in mask.iter_ones() {
                row_col_bytes.fill(0);
                row_chunks.clear();
                row_unique_chunks.clear();
                row_unique_frames.clear();
                let mut row_unique_chunk_bytes = 0usize;
                let mut row_unique_frame_bytes = 0usize;

                for (col_pos, &cidx) in proj_payload_cols.iter().enumerate() {
                    let mut col_row_len = 0usize;
                    match decoded_p1.get(&cidx) {
                        Some(DecodedPhase1::FlatPayloadRefs(refs)) => match refs.row_kind(r) {
                            FLAT_REF_INLINE => {
                                col_row_len = refs.inline_bytes(r).len();
                            }
                            FLAT_REF_CHUNKS => {
                                let (ps, cs) = refs.chunk_refs(r);
                                for (&pidx, &cid) in ps.iter().zip(cs.iter()) {
                                    let (clen, fid, flen) = resolve_chunk_and_frame(pidx, cid)?;
                                    col_row_len = col_row_len.checked_add(clen).ok_or(
                                        StatamError::BoundsExceeded {
                                            reason: "row payload byte size overflow",
                                        },
                                    )?;
                                    if col_row_len > MAX_ROW_PAYLOAD_BYTES {
                                        return Err(StatamError::BoundsExceeded {
                                            reason: "reconstructed row payload exceeds MAX_ROW_PAYLOAD_BYTES",
                                        });
                                    }
                                    if row_unique_chunks.insert((pidx, cid)) {
                                        row_unique_chunk_bytes = row_unique_chunk_bytes
                                            .checked_add(clen)
                                            .ok_or(StatamError::BoundsExceeded {
                                                reason: "row chunk decoded bytes overflow",
                                            })?;
                                        if row_unique_chunk_bytes > MAX_BATCH_DECODED_BYTES {
                                            return Err(StatamError::BoundsExceeded {
                                                reason: "single row chunk bytes exceed MAX_BATCH_DECODED_BYTES",
                                            });
                                        }
                                    }
                                    if row_unique_frames.insert((pidx, fid)) {
                                        row_unique_frame_bytes = row_unique_frame_bytes
                                            .checked_add(flen)
                                            .ok_or(StatamError::BoundsExceeded {
                                                reason: "row frame decoded bytes overflow",
                                            })?;
                                        if row_unique_frame_bytes > MAX_BATCH_DECODED_BYTES {
                                            return Err(StatamError::BoundsExceeded {
                                                reason: "single row frame bytes exceed MAX_BATCH_DECODED_BYTES",
                                            });
                                        }
                                    }
                                    row_chunks.push((pidx, cid, clen, fid, flen));
                                }
                            }
                            _ => {}
                        },
                        Some(DecodedPhase1::SelectedPayloadRefs(selected)) => {
                            if let Ok(idx) = selected.binary_search_by_key(&r, |&(row, _)| row) {
                                match &selected[idx].1 {
                                    PayloadRefRow::Null => {}
                                    PayloadRefRow::Inline(b) => {
                                        col_row_len = b.len();
                                    }
                                    PayloadRefRow::Chunks(chunks) => {
                                        for &(pidx, cid) in chunks {
                                            let (clen, fid, flen) =
                                                resolve_chunk_and_frame(pidx, cid)?;
                                            col_row_len = col_row_len.checked_add(clen).ok_or(
                                                StatamError::BoundsExceeded {
                                                    reason: "row payload byte size overflow",
                                                },
                                            )?;
                                            if col_row_len > MAX_ROW_PAYLOAD_BYTES {
                                                return Err(StatamError::BoundsExceeded {
                                                    reason: "reconstructed row payload exceeds MAX_ROW_PAYLOAD_BYTES",
                                                });
                                            }
                                            if row_unique_chunks.insert((pidx, cid)) {
                                                row_unique_chunk_bytes = row_unique_chunk_bytes
                                                    .checked_add(clen)
                                                    .ok_or(StatamError::BoundsExceeded {
                                                        reason: "row chunk decoded bytes overflow",
                                                    })?;
                                                if row_unique_chunk_bytes > MAX_BATCH_DECODED_BYTES
                                                {
                                                    return Err(StatamError::BoundsExceeded {
                                                        reason: "single row chunk bytes exceed MAX_BATCH_DECODED_BYTES",
                                                    });
                                                }
                                            }
                                            if row_unique_frames.insert((pidx, fid)) {
                                                row_unique_frame_bytes = row_unique_frame_bytes
                                                    .checked_add(flen)
                                                    .ok_or(StatamError::BoundsExceeded {
                                                        reason: "row frame decoded bytes overflow",
                                                    })?;
                                                if row_unique_frame_bytes > MAX_BATCH_DECODED_BYTES
                                                {
                                                    return Err(StatamError::BoundsExceeded {
                                                        reason: "single row frame bytes exceed MAX_BATCH_DECODED_BYTES",
                                                    });
                                                }
                                            }
                                            row_chunks.push((pidx, cid, clen, fid, flen));
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    row_col_bytes[col_pos] = col_row_len;
                }

                // Compute incremental bytes if added to `cur_wave`.
                let mut delta_chunk_bytes = 0usize;
                let mut delta_frame_bytes = 0usize;
                if !cur_wave.is_empty() {
                    seen_new_chunks.clear();
                    seen_new_frames.clear();
                    for &(pidx, cid, clen, fid, flen) in &row_chunks {
                        if !wave_seen_chunks.contains(&(pidx, cid))
                            && seen_new_chunks.insert((pidx, cid))
                        {
                            delta_chunk_bytes = delta_chunk_bytes.saturating_add(clen);
                        }
                        if !wave_seen_frames.contains(&(pidx, fid))
                            && seen_new_frames.insert((pidx, fid))
                        {
                            delta_frame_bytes = delta_frame_bytes.saturating_add(flen);
                        }
                    }
                    let exceeds_wave =
                        wave_chunk_bytes.saturating_add(delta_chunk_bytes)
                            > PHASE2_PAYLOAD_WAVE_TARGET_BYTES
                            || wave_frame_bytes.saturating_add(delta_frame_bytes)
                                > PHASE2_PAYLOAD_WAVE_TARGET_BYTES
                            || wave_col_decoded.iter().zip(row_col_bytes.iter()).any(
                                |(&w, &rb)| w.saturating_add(rb) > PHASE2_PAYLOAD_WAVE_TARGET_BYTES,
                            );
                    if exceeds_wave {
                        waves.push(std::mem::take(&mut cur_wave));
                        wave_seen_chunks.clear();
                        wave_seen_frames.clear();
                        wave_chunk_bytes = 0;
                        wave_frame_bytes = 0;
                        wave_col_decoded.fill(0);
                    }
                }

                for &(pidx, cid, clen, fid, flen) in &row_chunks {
                    if wave_seen_chunks.insert((pidx, cid)) {
                        wave_chunk_bytes = wave_chunk_bytes.saturating_add(clen);
                    }
                    if wave_seen_frames.insert((pidx, fid)) {
                        wave_frame_bytes = wave_frame_bytes.saturating_add(flen);
                    }
                }
                for (w, &rb) in wave_col_decoded.iter_mut().zip(row_col_bytes.iter()) {
                    *w = w.saturating_add(rb);
                }

                if cur_wave.last().is_some_and(|(p, _)| *p == pos) {
                    if let Some((_, sub_mask)) = cur_wave.last_mut() {
                        let _ = sub_mask.set(r, true);
                    }
                } else {
                    let mut sub_mask = Bitmap::new_zeroed(mb_row_count);
                    let _ = sub_mask.set(r, true);
                    cur_wave.push((pos, sub_mask));
                }
            }
        }
        if !cur_wave.is_empty() {
            waves.push(cur_wave);
        }
        Ok(waves)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn resolve_rg_payload_chunks<'a>(
        &self,
        rg_idx: usize,
        active_mbs: &mut [(usize, Bitmap, HashMap<usize, DecodedPhase1>)],
        proj_indices: &[usize],
        l0_local: Option<&ParsedL0Payload<'_>>,
        pre_resolved: &HashMap<([u8; 16], u32), Bytes>,
        read_block: &BlockReader<'a>,
        read_threads: usize,
    ) -> Result<HashMap<(u16, u32), Bytes>, StatamError> {
        let mut proj_payload_cols: Vec<usize> = proj_indices
            .iter()
            .copied()
            .filter(|&cidx| {
                self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload
            })
            .collect();
        if proj_payload_cols.is_empty() {
            return Ok(HashMap::new());
        }
        proj_payload_cols.sort_unstable();
        proj_payload_cols.dedup();

        self.ensure_rg_payload_refs_decoded(rg_idx, active_mbs, &proj_payload_cols, read_block)?;

        let mut by_pack: HashMap<u16, BTreeSet<u32>> = HashMap::new();
        for (_, mask, decoded_p1) in active_mbs.iter() {
            for &cidx in &proj_payload_cols {
                match decoded_p1.get(&cidx) {
                    Some(DecodedPhase1::FlatPayloadRefs(refs)) => {
                        for r in mask.iter_ones() {
                            if refs.row_kind(r) == FLAT_REF_CHUNKS {
                                let (ps, cs) = refs.chunk_refs(r);
                                for (&pidx, &cid) in ps.iter().zip(cs.iter()) {
                                    by_pack.entry(pidx).or_default().insert(cid);
                                }
                            }
                        }
                    }
                    Some(DecodedPhase1::SelectedPayloadRefs(selected)) => {
                        for (r, row) in selected {
                            if !mask.get(*r) {
                                continue;
                            }
                            if let PayloadRefRow::Chunks(chunks) = row {
                                for &(pidx, cid) in chunks {
                                    by_pack.entry(pidx).or_default().insert(cid);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // Pre-validate cumulative unique chunk bytes AND unique frame bytes across ALL packs
        // (L0 local + all external packs) before fetching or decompressing any frame.
        enum PackWork {
            L0(Vec<u32>),
            External(u16, Arc<ChunkPackReader>, Vec<u32>),
        }
        let mut work_items = Vec::with_capacity(by_pack.len());
        let mut total_chunk_bytes = 0usize;
        let mut total_frame_bytes = 0usize;
        let mut resolved_chunks: HashMap<(u16, u32), Bytes> = HashMap::new();
        for (pidx, cids_set) in by_pack {
            let cids: Vec<u32> = cids_set.into_iter().collect();
            if pidx == L0_LOCAL_PACK_IDX {
                let l0 = l0_local.ok_or(StatamError::Corrupt {
                    reason: "missing L0 local payload region",
                })?;
                l0.check_chunks_budget(&cids, &mut total_chunk_bytes, &mut total_frame_bytes)?;
                work_items.push(PackWork::L0(cids));
            } else {
                let pack_id = *self.meta.schema_meta.pack_ids.get(pidx as usize).ok_or(
                    StatamError::Corrupt {
                        reason: "payload ref pack_idx out of bounds",
                    },
                )?;
                let pack = self.resolve_pack(pack_id)?;
                pack.check_chunks_budget(&cids, &mut total_chunk_bytes, &mut total_frame_bytes)?;
                if pre_resolved.is_empty() {
                    work_items.push(PackWork::External(pidx, pack, cids));
                } else {
                    let mut missing_cids = Vec::new();
                    for cid in cids {
                        if let Some(cached) = pre_resolved.get(&(pack_id, cid)) {
                            resolved_chunks.insert((pidx, cid), cached.clone());
                        } else {
                            missing_cids.push(cid);
                        }
                    }
                    if !missing_cids.is_empty() {
                        work_items.push(PackWork::External(pidx, pack, missing_cids));
                    }
                }
            }
        }

        let threads = read_threads.clamp(1, MAX_PARALLEL_THREADS);
        for item in work_items {
            match item {
                PackWork::L0(cids) => {
                    let l0 = l0_local.ok_or(StatamError::Corrupt {
                        reason: "missing L0 local payload region",
                    })?;
                    let map = l0.read_chunks(&cids)?;
                    for (cid, bytes) in map {
                        resolved_chunks.insert((L0_LOCAL_PACK_IDX, cid), bytes);
                    }
                }
                PackWork::External(pidx, pack, cids) => {
                    if threads > 1 {
                        let pairs = pack.scan_chunks_in_frames_parallel(
                            &cids,
                            threads,
                            || Ok(()),
                            |(), cid, bytes| Some((cid, Bytes::copy_from_slice(bytes))),
                        )?;
                        for (cid, bytes) in pairs {
                            resolved_chunks.insert((pidx, cid), bytes);
                        }
                    } else {
                        pack.scan_chunks_in_frames(&cids, |cid, bytes| {
                            resolved_chunks.insert((pidx, cid), Bytes::copy_from_slice(bytes));
                        })?;
                    }
                }
            }
        }
        Ok(resolved_chunks)
    }

    /// Collects all unique `(pack_idx, chunk_id)` references for `active_mbs` across
    /// `proj_payload_cols`, separated into external `.stpk` chunks and local L0 chunks.
    pub(super) fn collect_chunk_refs_for_mbs(
        active_mbs: &[ActiveMiniBlock],
        proj_payload_cols: &[usize],
    ) -> (HashMap<u16, BTreeSet<u32>>, BTreeSet<u32>) {
        let mut ext_by_pack: HashMap<u16, BTreeSet<u32>> = HashMap::new();
        let mut l0_cids: BTreeSet<u32> = BTreeSet::new();
        for (_, mask, decoded_p1) in active_mbs {
            for &cidx in proj_payload_cols {
                match decoded_p1.get(&cidx) {
                    Some(DecodedPhase1::FlatPayloadRefs(refs)) => {
                        for r in mask.iter_ones() {
                            if refs.row_kind(r) == FLAT_REF_CHUNKS {
                                let (ps, cs) = refs.chunk_refs(r);
                                for (&pidx, &cid) in ps.iter().zip(cs.iter()) {
                                    if pidx == L0_LOCAL_PACK_IDX {
                                        l0_cids.insert(cid);
                                    } else {
                                        ext_by_pack.entry(pidx).or_default().insert(cid);
                                    }
                                }
                            }
                        }
                    }
                    Some(DecodedPhase1::SelectedPayloadRefs(selected)) => {
                        for (r, row) in selected {
                            if !mask.get(*r) {
                                continue;
                            }
                            if let PayloadRefRow::Chunks(chunks) = row {
                                for &(pidx, cid) in chunks {
                                    if pidx == L0_LOCAL_PACK_IDX {
                                        l0_cids.insert(cid);
                                    } else {
                                        ext_by_pack.entry(pidx).or_default().insert(cid);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        (ext_by_pack, l0_cids)
    }

    /// Computes cumulative unique `(chunk_bytes, frame_bytes)` for `ext_by_pack`, validating
    /// every pack index and chunk ID against pack TOC bounds.
    pub(super) fn measure_external_pack_bytes(
        &self,
        ext_by_pack: &HashMap<u16, BTreeSet<u32>>,
    ) -> Result<(usize, usize), StatamError> {
        let mut total_chunk_bytes = 0usize;
        let mut total_frame_bytes = 0usize;
        for (&pidx, cids_set) in ext_by_pack {
            let pack_id =
                *self
                    .meta
                    .schema_meta
                    .pack_ids
                    .get(pidx as usize)
                    .ok_or(StatamError::Corrupt {
                        reason: "payload ref pack_idx out of bounds",
                    })?;
            let pack = self.resolve_pack(pack_id)?;
            let toc = pack.toc();
            let mut seen_frames: BTreeSet<u32> = BTreeSet::new();
            for &cid in cids_set {
                let loc = toc.chunks.get(cid as usize).ok_or(StatamError::Corrupt {
                    reason: "chunk_id out of bounds",
                })?;
                total_chunk_bytes = total_chunk_bytes.saturating_add(loc.raw_len as usize);
                if seen_frames.insert(loc.frame_id) {
                    total_frame_bytes = total_frame_bytes
                        .saturating_add(toc.frames[loc.frame_id as usize].raw_len as usize);
                }
            }
        }
        Ok((total_chunk_bytes, total_frame_bytes))
    }

    /// Computes the maximum reconstructed logical byte size across all `(mini_block, payload_col)`
    /// pairs in `active_mbs` (summing inline + referenced chunk `raw_len` for active rows).
    /// Used by `take_rows` to detect CDC-deduplicated RGs whose unique physical chunk bytes fit
    /// in <= 128 MiB but whose logical reconstructed column exceeds `PHASE2_PAYLOAD_WAVE_TARGET_BYTES`.
    pub(super) fn max_mb_col_decoded_bytes(
        &self,
        active_mbs: &[ActiveMiniBlock],
        proj_payload_cols: &[usize],
        l0_local: Option<&ParsedL0Payload<'_>>,
    ) -> Result<usize, StatamError> {
        let mut pack_cache: HashMap<u16, Arc<ChunkPackReader>> = HashMap::new();
        let mut chunk_raw_len = |pidx: u16, cid: u32| -> Result<usize, StatamError> {
            if pidx == L0_LOCAL_PACK_IDX {
                let l0 = l0_local.ok_or(StatamError::Corrupt {
                    reason: "missing L0 local payload region",
                })?;
                let loc = l0.chunks.get(cid as usize).ok_or(StatamError::Corrupt {
                    reason: "L0 chunk_id out of bounds",
                })?;
                Ok(loc.raw_len as usize)
            } else {
                let pack = match pack_cache.entry(pidx) {
                    std::collections::hash_map::Entry::Occupied(o) => Arc::clone(o.get()),
                    std::collections::hash_map::Entry::Vacant(v) => {
                        let pack_id = *self.meta.schema_meta.pack_ids.get(pidx as usize).ok_or(
                            StatamError::Corrupt {
                                reason: "payload ref pack_idx out of bounds",
                            },
                        )?;
                        let resolved = self.resolve_pack(pack_id)?;
                        v.insert(Arc::clone(&resolved));
                        resolved
                    }
                };
                let loc = pack
                    .toc()
                    .chunks
                    .get(cid as usize)
                    .ok_or(StatamError::Corrupt {
                        reason: "chunk_id out of bounds",
                    })?;
                Ok(loc.raw_len as usize)
            }
        };

        let mut max_col_bytes = 0usize;
        for (_, mask, decoded_p1) in active_mbs {
            for &cidx in proj_payload_cols {
                let mut col_bytes = 0usize;
                match decoded_p1.get(&cidx) {
                    Some(DecodedPhase1::FlatPayloadRefs(refs)) => {
                        for r in mask.iter_ones() {
                            match refs.row_kind(r) {
                                FLAT_REF_INLINE => {
                                    col_bytes =
                                        col_bytes.saturating_add(refs.inline_bytes(r).len());
                                }
                                FLAT_REF_CHUNKS => {
                                    let (ps, cs) = refs.chunk_refs(r);
                                    for (&pidx, &cid) in ps.iter().zip(cs.iter()) {
                                        col_bytes =
                                            col_bytes.saturating_add(chunk_raw_len(pidx, cid)?);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(DecodedPhase1::SelectedPayloadRefs(selected)) => {
                        for (r, row) in selected {
                            if !mask.get(*r) {
                                continue;
                            }
                            match row {
                                PayloadRefRow::Null => {}
                                PayloadRefRow::Inline(b) => {
                                    col_bytes = col_bytes.saturating_add(b.len());
                                }
                                PayloadRefRow::Chunks(chunks) => {
                                    for &(pidx, cid) in chunks {
                                        col_bytes =
                                            col_bytes.saturating_add(chunk_raw_len(pidx, cid)?);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
                if col_bytes > max_col_bytes {
                    max_col_bytes = col_bytes;
                }
            }
        }
        Ok(max_col_bytes)
    }

    /// Resolves all external `.stpk` chunks in `ext_by_pack` in **ONE** coalesced read wave per
    /// pack (preserving `read_threads` parallelism and enforcing `MAX_BATCH_DECODED_BYTES`).
    pub(super) fn resolve_external_pack_chunks(
        &self,
        ext_by_pack: HashMap<u16, BTreeSet<u32>>,
        read_threads: usize,
    ) -> Result<HashMap<(u16, u32), Bytes>, StatamError> {
        if ext_by_pack.is_empty() {
            return Ok(HashMap::new());
        }
        let mut work_items: Vec<(u16, Arc<ChunkPackReader>, Vec<u32>)> =
            Vec::with_capacity(ext_by_pack.len());
        let mut total_chunk_bytes = 0usize;
        let mut total_frame_bytes = 0usize;
        for (pidx, cids_set) in ext_by_pack {
            let cids: Vec<u32> = cids_set.into_iter().collect();
            let pack_id =
                *self
                    .meta
                    .schema_meta
                    .pack_ids
                    .get(pidx as usize)
                    .ok_or(StatamError::Corrupt {
                        reason: "payload ref pack_idx out of bounds",
                    })?;
            let pack = self.resolve_pack(pack_id)?;
            pack.check_chunks_budget(&cids, &mut total_chunk_bytes, &mut total_frame_bytes)?;
            work_items.push((pidx, pack, cids));
        }
        let threads = read_threads.clamp(1, MAX_PARALLEL_THREADS);
        let mut resolved_chunks: HashMap<(u16, u32), Bytes> = HashMap::new();
        for (pidx, pack, cids) in work_items {
            if threads > 1 {
                let pairs = pack.scan_chunks_in_frames_parallel(
                    &cids,
                    threads,
                    || Ok(()),
                    |(), cid, bytes| Some((cid, Bytes::copy_from_slice(bytes))),
                )?;
                for (cid, bytes) in pairs {
                    resolved_chunks.insert((pidx, cid), bytes);
                }
            } else {
                pack.scan_chunks_in_frames(&cids, |cid, bytes| {
                    resolved_chunks.insert((pidx, cid), Bytes::copy_from_slice(bytes));
                })?;
            }
        }
        Ok(resolved_chunks)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn eval_payload_refs_substring(
        &self,
        refs: &FlatPayloadRefs,
        active_mask: &Bitmap,
        needle: &[u8],
        case_insensitive: bool,
        l0_local: Option<&ParsedL0Payload<'_>>,
        read_threads: usize,
        retain: Option<&RetainedPackChunks>,
    ) -> Result<Bitmap, StatamError> {
        let mut out = self.eval_payload_refs_substring_batch(
            &[(refs, active_mask)],
            needle,
            case_insensitive,
            l0_local,
            read_threads,
            retain,
        )?;
        out.pop().ok_or(StatamError::Corrupt {
            reason: "missing single-mb payload substring result",
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn eval_payload_refs_substring_batch(
        &self,
        mb_inputs: &[(&FlatPayloadRefs, &Bitmap)],
        needle: &[u8],
        case_insensitive: bool,
        l0_local: Option<&ParsedL0Payload<'_>>,
        read_threads: usize,
        retain: Option<&RetainedPackChunks>,
    ) -> Result<Vec<Bitmap>, StatamError> {
        let mut results: Vec<Bitmap> = mb_inputs
            .iter()
            .map(|(refs, _)| Bitmap::new_zeroed(refs.len()))
            .collect();
        if mb_inputs.is_empty() || mb_inputs.iter().all(|(_, mask)| mask.all_zeros()) {
            return Ok(results);
        }
        if needle.is_empty() {
            for (m, &(refs, active_mask)) in mb_inputs.iter().enumerate() {
                for i in active_mask.iter_ones() {
                    if i < refs.len() && refs.row_kind(i) != FLAT_REF_NULL {
                        let _ = results[m].set(i, true);
                    }
                }
            }
            return Ok(results);
        }

        // Handle active inline rows + track external pack_idxs + L0 local chunks on active rows only.
        let mut matcher = Utf8Matcher::new(needle, case_insensitive);
        let mut external_pack_idxs = BTreeSet::new();
        let mut has_l0_chunks = false;
        let mut active_chunk_rows: Vec<Vec<usize>> = vec![Vec::new(); mb_inputs.len()];

        for (m, &(refs, active_mask)) in mb_inputs.iter().enumerate() {
            for i in active_mask.iter_ones() {
                if i >= refs.len() {
                    break;
                }
                match refs.row_kind(i) {
                    FLAT_REF_NULL => {}
                    FLAT_REF_INLINE => {
                        if matcher.matches(refs.inline_bytes(i)) {
                            let _ = results[m].set(i, true);
                        }
                    }
                    FLAT_REF_CHUNKS => {
                        active_chunk_rows[m].push(i);
                        let (ps, _) = refs.chunk_refs(i);
                        for &pidx in ps {
                            if pidx == L0_LOCAL_PACK_IDX {
                                has_l0_chunks = true;
                            } else {
                                external_pack_idxs.insert(pidx);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        let all_mb_refs: Vec<&FlatPayloadRefs> = mb_inputs.iter().map(|&(refs, _)| refs).collect();
        for pack_idx in external_pack_idxs {
            let pack_id = *self
                .meta
                .schema_meta
                .pack_ids
                .get(pack_idx as usize)
                .ok_or(StatamError::Corrupt {
                    reason: "payload ref pack_idx out of bounds",
                })?;
            let pack = self.resolve_pack(pack_id)?;
            if pack_short_circuits_all(&pack, needle, case_insensitive)? {
                let pack_chunk_count = pack.toc().chunks.len();
                let no_edges = pack.postscript().edge_table_len == 0;
                for (m, &(refs, _)) in mb_inputs.iter().enumerate() {
                    for &r in &active_chunk_rows[m] {
                        let (ps, cs) = refs.chunk_refs(r);
                        let mut pack_refs = 0usize;
                        for (&p, &cid) in ps.iter().zip(cs.iter()) {
                            if p == pack_idx {
                                if (cid as usize) >= pack_chunk_count {
                                    return Err(StatamError::Corrupt {
                                        reason: "payload ref chunk_id out of pack bounds",
                                    });
                                }
                                pack_refs += 1;
                                if no_edges && pack_refs > 1 {
                                    return Err(StatamError::Corrupt {
                                        reason: "multi-chunk row references pack without edge table",
                                    });
                                }
                            }
                        }
                    }
                }
                continue;
            }

            let mut pack_masks: Vec<Bitmap> = Vec::with_capacity(mb_inputs.len());
            let mut any_pack_row = false;
            for (m, &(refs, _)) in mb_inputs.iter().enumerate() {
                let mut pack_mask = Bitmap::new_zeroed(refs.len());
                for &r in &active_chunk_rows[m] {
                    if results[m].get(r) {
                        continue;
                    }
                    let (ps, _) = refs.chunk_refs(r);
                    if ps.contains(&pack_idx) {
                        let _ = pack_mask.set(r, true);
                        any_pack_row = true;
                    }
                }
                pack_masks.push(pack_mask);
            }
            if !any_pack_row {
                continue;
            }

            let pack_mask_refs: Vec<&Bitmap> = pack_masks.iter().collect();
            let bms = search_pack_rows_flat_multi_masked_with_threads(
                &pack,
                &all_mb_refs,
                &pack_mask_refs,
                pack_idx,
                needle,
                case_insensitive,
                read_threads,
                retain.map(|r| (pack_id, r)),
            )?;
            for (m, bm) in bms.into_iter().enumerate() {
                for bit in bm.iter_ones() {
                    let _ = results[m].set(bit, true);
                }
            }
        }

        if has_l0_chunks {
            let l0 = l0_local.ok_or(StatamError::Corrupt {
                reason: "missing L0 local payload region for chunked L0 row",
            })?;
            for (m, &(refs, _)) in mb_inputs.iter().enumerate() {
                if active_chunk_rows[m].is_empty() {
                    continue;
                }
                let mut single_by_frame: BTreeMap<u32, Vec<(u32, usize)>> = BTreeMap::new();
                let mut multi_chunk_rows: Vec<usize> = Vec::new();

                for &r in &active_chunk_rows[m] {
                    if results[m].get(r) {
                        continue;
                    }
                    let (ps, cs) = refs.chunk_refs(r);
                    if ps.first().copied() != Some(L0_LOCAL_PACK_IDX) {
                        continue;
                    }
                    for &cid in cs {
                        if (cid as usize) >= l0.chunks.len() {
                            return Err(StatamError::Corrupt {
                                reason: "L0 chunk_id out of bounds",
                            });
                        }
                    }
                    if cs.len() == 1 {
                        let cid = cs[0];
                        let fid = l0.chunks[cid as usize].frame_id;
                        single_by_frame.entry(fid).or_default().push((cid, r));
                    } else if cs.len() > 1 {
                        multi_chunk_rows.push(r);
                    }
                }

                // Stream single-chunk L0 rows one frame at a time (<= 1 frame in RAM),
                // deduplicating repeated chunk IDs per frame to prevent aliased-chunk DoS.
                for (fid, mut cid_rows) in single_by_frame {
                    let decoded = l0.decompress_frame(fid)?;
                    cid_rows.sort_unstable_by_key(|&(cid, _)| cid);
                    let mut idx = 0usize;
                    while idx < cid_rows.len() {
                        let cid = cid_rows[idx].0;
                        let loc = l0.chunks[cid as usize];
                        let s = loc.offset_in_frame as usize;
                        let e = s + loc.raw_len as usize;
                        let slice = decoded.get(s..e).ok_or(StatamError::Corrupt {
                            reason: "L0 chunk slice out of frame bounds",
                        })?;
                        let hit = matcher.matches(slice);
                        while idx < cid_rows.len() && cid_rows[idx].0 == cid {
                            if hit {
                                let _ = results[m].set(cid_rows[idx].1, true);
                            }
                            idx += 1;
                        }
                    }
                }

                // Reconstruct rare multi-chunk L0 rows row-by-row with a 1-frame cache.
                if !multi_chunk_rows.is_empty() {
                    let mut cached_frame: Option<(u32, Vec<u8>)> = None;
                    let mut buf = Vec::new();
                    for r in multi_chunk_rows {
                        let (_, cs) = refs.chunk_refs(r);
                        buf.clear();
                        for &cid in cs {
                            let loc = l0.chunks[cid as usize];
                            let next_len = buf.len().checked_add(loc.raw_len as usize).ok_or(
                                StatamError::BoundsExceeded {
                                    reason: "L0 payload exceeds MAX_ROW_PAYLOAD_BYTES",
                                },
                            )?;
                            if next_len > MAX_ROW_PAYLOAD_BYTES {
                                return Err(StatamError::BoundsExceeded {
                                    reason: "L0 payload exceeds MAX_ROW_PAYLOAD_BYTES",
                                });
                            }
                            if cached_frame
                                .as_ref()
                                .is_none_or(|(cfid, _)| *cfid != loc.frame_id)
                            {
                                let decoded = l0.decompress_frame(loc.frame_id)?;
                                cached_frame = Some((loc.frame_id, decoded));
                            }
                            if let Some((_, ref frame_buf)) = cached_frame {
                                let s = loc.offset_in_frame as usize;
                                let e = s + loc.raw_len as usize;
                                let slice = frame_buf.get(s..e).ok_or(StatamError::Corrupt {
                                    reason: "L0 chunk slice out of frame bounds",
                                })?;
                                buf.extend_from_slice(slice);
                            }
                        }
                        if matcher.matches(&buf) {
                            let _ = results[m].set(r, true);
                        }
                    }
                }
            }
        }

        Ok(results)
    }
}

fn clone_decoded_p1_for_wave(src: &HashMap<usize, DecodedPhase1>) -> HashMap<usize, DecodedPhase1> {
    let mut out = HashMap::with_capacity(src.len());
    for (&k, v) in src {
        match v {
            DecodedPhase1::Col(c) => {
                out.insert(k, DecodedPhase1::Col(c.clone()));
            }
            DecodedPhase1::FlatPayloadRefs(r) => {
                out.insert(k, DecodedPhase1::FlatPayloadRefs(r.clone()));
            }
            DecodedPhase1::SelectedPayloadRefs(s) => {
                out.insert(k, DecodedPhase1::SelectedPayloadRefs(s.clone()));
            }
            DecodedPhase1::ShreddedSub(_) => {}
        }
    }
    out
}

fn finish_payload_column(
    dt: &DataType,
    offsets: Vec<u32>,
    data_bytes: Vec<u8>,
    nulls: Bitmap,
) -> Result<Column, StatamError> {
    let flat_data = match dt {
        DataType::Binary => FlatData::Binary(BinaryData::try_new(offsets, data_bytes)?),
        DataType::Utf8 => FlatData::Utf8(Utf8Data::try_new(offsets, data_bytes)?),
        _ => {
            return Err(StatamError::SchemaMismatch {
                reason: "ExternalPayload column must be DataType::Utf8 or DataType::Binary",
            });
        }
    };
    let nulls_opt = if nulls.all_ones() { None } else { Some(nulls) };
    Ok(Column::Flat(FlatColumn::try_new(flat_data, nulls_opt)?))
}

fn append_payload_row_to_bytes(
    row: &PayloadRefRow,
    row_idx: usize,
    nulls: &mut Bitmap,
    offsets: &mut Vec<u32>,
    data_bytes: &mut Vec<u8>,
    total_decoded: &mut usize,
    resolved_chunks: &HashMap<(u16, u32), Bytes>,
) -> Result<(), StatamError> {
    match row {
        PayloadRefRow::Null => {
            offsets.push(data_bytes.len() as u32);
        }
        PayloadRefRow::Inline(bytes) => {
            let _ = nulls.set(row_idx, true);
            *total_decoded =
                total_decoded
                    .checked_add(bytes.len())
                    .ok_or(StatamError::BoundsExceeded {
                        reason: "decoded payload column overflows usize",
                    })?;
            if *total_decoded > MAX_BATCH_DECODED_BYTES {
                return Err(StatamError::BoundsExceeded {
                    reason: "decoded payload column exceeds MAX_BATCH_DECODED_BYTES",
                });
            }
            let next_off = u32::try_from(data_bytes.len() + bytes.len()).map_err(|_| {
                StatamError::BoundsExceeded {
                    reason: "payload column exceeds u32::MAX",
                }
            })?;
            data_bytes.extend_from_slice(bytes);
            offsets.push(next_off);
        }
        PayloadRefRow::Chunks(chunks) => {
            let _ = nulls.set(row_idx, true);
            let mut row_len = 0usize;
            let start_pos = data_bytes.len();
            for &(pidx, cid) in chunks {
                let chunk = resolved_chunks
                    .get(&(pidx, cid))
                    .ok_or(StatamError::Corrupt {
                        reason: "missing resolved payload chunk",
                    })?;
                row_len = row_len
                    .checked_add(chunk.len())
                    .ok_or(StatamError::BoundsExceeded {
                        reason: "row payload byte size overflow",
                    })?;
                if row_len > MAX_ROW_PAYLOAD_BYTES {
                    return Err(StatamError::BoundsExceeded {
                        reason: "reconstructed row payload exceeds MAX_ROW_PAYLOAD_BYTES",
                    });
                }
                *total_decoded =
                    total_decoded
                        .checked_add(chunk.len())
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "decoded payload column overflows usize",
                        })?;
                if *total_decoded > MAX_BATCH_DECODED_BYTES {
                    return Err(StatamError::BoundsExceeded {
                        reason: "decoded payload column exceeds MAX_BATCH_DECODED_BYTES",
                    });
                }
                data_bytes.extend_from_slice(chunk);
            }
            let next_off =
                u32::try_from(start_pos + row_len).map_err(|_| StatamError::BoundsExceeded {
                    reason: "payload column exceeds u32::MAX",
                })?;
            offsets.push(next_off);
        }
    }
    Ok(())
}

pub(super) fn reconstruct_payload_column_from_flat(
    refs: &FlatPayloadRefs,
    active_mask: &Bitmap,
    resolved_chunks: &HashMap<(u16, u32), Bytes>,
    dt: &DataType,
) -> Result<Column, StatamError> {
    let row_count = refs.len();
    let all_active_null = active_mask
        .iter_ones()
        .all(|r| refs.row_kind(r) == FLAT_REF_NULL);
    if all_active_null {
        return Ok(Column::try_new_const(
            ScalarValue::Null(dt.clone()),
            row_count,
        )?);
    }

    let mut offsets = Vec::with_capacity(row_count + 1);
    offsets.push(0u32);
    let mut data_bytes = Vec::new();
    let mut nulls = Bitmap::new_zeroed(row_count);
    let mut total_decoded = 0usize;

    for r in 0..row_count {
        if !active_mask.get(r) {
            offsets.push(data_bytes.len() as u32);
            continue;
        }
        match refs.row_kind(r) {
            FLAT_REF_NULL => {
                offsets.push(data_bytes.len() as u32);
            }
            FLAT_REF_INLINE => {
                let bytes = refs.inline_bytes(r);
                total_decoded =
                    total_decoded
                        .checked_add(bytes.len())
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "decoded payload column overflows usize",
                        })?;
                if total_decoded > MAX_BATCH_DECODED_BYTES {
                    return Err(StatamError::BoundsExceeded {
                        reason: "decoded payload column exceeds MAX_BATCH_DECODED_BYTES",
                    });
                }
                let _ = nulls.set(r, true);
                data_bytes.extend_from_slice(bytes);
                let next_off =
                    u32::try_from(data_bytes.len()).map_err(|_| StatamError::BoundsExceeded {
                        reason: "payload column exceeds u32::MAX",
                    })?;
                offsets.push(next_off);
            }
            FLAT_REF_CHUNKS => {
                let _ = nulls.set(r, true);
                let start_pos = data_bytes.len();
                let mut row_len = 0usize;
                let (ps, cs) = refs.chunk_refs(r);
                for (&pidx, &cid) in ps.iter().zip(cs.iter()) {
                    let chunk = resolved_chunks
                        .get(&(pidx, cid))
                        .ok_or(StatamError::Corrupt {
                            reason: "missing resolved payload chunk during column reconstruction",
                        })?;
                    row_len =
                        row_len
                            .checked_add(chunk.len())
                            .ok_or(StatamError::BoundsExceeded {
                                reason: "row payload byte size overflow",
                            })?;
                    if row_len > MAX_ROW_PAYLOAD_BYTES {
                        return Err(StatamError::BoundsExceeded {
                            reason: "reconstructed row payload exceeds MAX_ROW_PAYLOAD_BYTES",
                        });
                    }
                    total_decoded = total_decoded.checked_add(chunk.len()).ok_or(
                        StatamError::BoundsExceeded {
                            reason: "decoded payload column overflows usize",
                        },
                    )?;
                    if total_decoded > MAX_BATCH_DECODED_BYTES {
                        return Err(StatamError::BoundsExceeded {
                            reason: "decoded payload column exceeds MAX_BATCH_DECODED_BYTES",
                        });
                    }
                    data_bytes.extend_from_slice(chunk);
                }
                let next_off = u32::try_from(start_pos + row_len).map_err(|_| {
                    StatamError::BoundsExceeded {
                        reason: "payload column exceeds u32::MAX",
                    }
                })?;
                offsets.push(next_off);
            }
            _ => {
                offsets.push(data_bytes.len() as u32);
            }
        }
    }

    finish_payload_column(dt, offsets, data_bytes, nulls)
}

pub(super) fn reconstruct_payload_column_selected(
    row_count: usize,
    selected: &[(usize, PayloadRefRow)],
    active_mask: &Bitmap,
    resolved_chunks: &HashMap<(u16, u32), Bytes>,
    dt: &DataType,
) -> Result<Column, StatamError> {
    let all_active_null = selected
        .iter()
        .all(|(r, row)| !active_mask.get(*r) || matches!(row, PayloadRefRow::Null));
    if all_active_null {
        return Ok(Column::try_new_const(
            ScalarValue::Null(dt.clone()),
            row_count,
        )?);
    }

    let mut offsets = Vec::with_capacity(row_count + 1);
    offsets.push(0u32);
    let mut data_bytes = Vec::new();
    let mut nulls = Bitmap::new_zeroed(row_count);
    let mut total_decoded = 0usize;
    let mut sel_idx = 0usize;

    for r in 0..row_count {
        if sel_idx < selected.len() && selected[sel_idx].0 == r {
            let row = &selected[sel_idx].1;
            sel_idx += 1;
            if !active_mask.get(r) {
                offsets.push(data_bytes.len() as u32);
                continue;
            }
            append_payload_row_to_bytes(
                row,
                r,
                &mut nulls,
                &mut offsets,
                &mut data_bytes,
                &mut total_decoded,
                resolved_chunks,
            )?;
        } else {
            offsets.push(data_bytes.len() as u32);
        }
    }

    finish_payload_column(dt, offsets, data_bytes, nulls)
}

pub(super) struct ParsedL0Payload<'a> {
    buf: &'a [u8],
    frames: Vec<(u64, u32, u32)>,
    chunks: Vec<ChunkLoc>,
}

impl<'a> ParsedL0Payload<'a> {
    pub(super) fn parse(buf: &'a [u8]) -> Result<Self, StatamError> {
        if buf.len() < 12 || buf[0..4] != L0_PAYLOAD_DIR_MAGIC {
            return Err(StatamError::Corrupt {
                reason: "invalid L0 payload directory header",
            });
        }
        let frame_count = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        let chunk_count = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]) as usize;
        let frames_bytes = frame_count
            .checked_mul(16)
            .ok_or(StatamError::BoundsExceeded {
                reason: "L0 payload frame table overflow",
            })?;
        let chunks_bytes = chunk_count
            .checked_mul(12)
            .ok_or(StatamError::BoundsExceeded {
                reason: "L0 payload chunk table overflow",
            })?;
        let dir_bytes = 12usize
            .checked_add(frames_bytes)
            .and_then(|v| v.checked_add(chunks_bytes))
            .ok_or(StatamError::BoundsExceeded {
                reason: "L0 payload directory size overflow",
            })?;
        if dir_bytes > buf.len() {
            return Err(StatamError::Corrupt {
                reason: "truncated L0 payload directory",
            });
        }

        let mut frames = Vec::with_capacity(frame_count);
        let mut pos = 12usize;
        for _ in 0..frame_count {
            let rel_off = u64::from_le_bytes(buf[pos..pos + 8].try_into().unwrap_or([0; 8]));
            let stored_len =
                u32::from_le_bytes([buf[pos + 8], buf[pos + 9], buf[pos + 10], buf[pos + 11]]);
            let raw_len =
                u32::from_le_bytes([buf[pos + 12], buf[pos + 13], buf[pos + 14], buf[pos + 15]]);
            let total = (crate::format::PACK_FRAME_HEADER_LEN as u64)
                .checked_add(u64::from(stored_len))
                .ok_or(StatamError::BoundsExceeded {
                    reason: "L0 frame length overflow",
                })?;
            let end = rel_off
                .checked_add(total)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "L0 frame end overflow",
                })?;
            if rel_off < dir_bytes as u64 || end > buf.len() as u64 {
                return Err(StatamError::Corrupt {
                    reason: "L0 frame out of region bounds",
                });
            }
            frames.push((rel_off, stored_len, raw_len));
            pos += 16;
        }

        let mut chunks = Vec::with_capacity(chunk_count);
        for _ in 0..chunk_count {
            let loc = ChunkLoc::decode(&buf[pos..pos + 12])?;
            let Some(&(_, _, f_raw)) = frames.get(loc.frame_id as usize) else {
                return Err(StatamError::Corrupt {
                    reason: "L0 chunk frame_id out of bounds",
                });
            };
            let end = loc.offset_in_frame.checked_add(loc.raw_len).ok_or(
                StatamError::BoundsExceeded {
                    reason: "L0 chunk end overflow",
                },
            )?;
            if end > f_raw {
                return Err(StatamError::Corrupt {
                    reason: "L0 chunk exceeds frame raw_len",
                });
            }
            chunks.push(loc);
            pos += 12;
        }

        Ok(Self {
            buf,
            frames,
            chunks,
        })
    }

    pub(super) fn verify_all_frames(&self) -> Result<(), StatamError> {
        for &(rel_off, stored_len, raw_len) in &self.frames {
            let start = rel_off as usize;
            let end = start + crate::format::PACK_FRAME_HEADER_LEN + stored_len as usize;
            let (_, _, decoded) =
                read_framed_pack_frame(&self.buf[start..end], MAX_FRAME_RAW_BYTES)?;
            if decoded.len() != raw_len as usize {
                return Err(StatamError::Corrupt {
                    reason: "L0 frame raw_len mismatch",
                });
            }
        }
        Ok(())
    }

    fn decompress_frame(&self, fid: u32) -> Result<Vec<u8>, StatamError> {
        let (rel_off, stored_len, raw_len) =
            *self.frames.get(fid as usize).ok_or(StatamError::Corrupt {
                reason: "L0 frame_id out of bounds",
            })?;
        let start = rel_off as usize;
        let end = start + crate::format::PACK_FRAME_HEADER_LEN + stored_len as usize;
        let (_, _, decoded) = read_framed_pack_frame(&self.buf[start..end], MAX_FRAME_RAW_BYTES)?;
        if decoded.len() != raw_len as usize {
            return Err(StatamError::Corrupt {
                reason: "L0 frame raw_len mismatch",
            });
        }
        Ok(decoded)
    }

    pub(super) fn measure_chunks_bytes(
        &self,
        cids: &BTreeSet<u32>,
    ) -> Result<(usize, usize), StatamError> {
        let mut total_chunk_bytes = 0usize;
        let mut total_frame_bytes = 0usize;
        let mut seen_frames = BTreeSet::new();
        for &cid in cids {
            let loc = self.chunks.get(cid as usize).ok_or(StatamError::Corrupt {
                reason: "L0 chunk_id out of bounds",
            })?;
            total_chunk_bytes = total_chunk_bytes.saturating_add(loc.raw_len as usize);
            if seen_frames.insert(loc.frame_id) {
                let (_, _, raw_len) = self.frames[loc.frame_id as usize];
                total_frame_bytes = total_frame_bytes.saturating_add(raw_len as usize);
            }
        }
        Ok((total_chunk_bytes, total_frame_bytes))
    }

    pub(super) fn check_chunks_budget(
        &self,
        cids: &[u32],
        total_chunk_bytes: &mut usize,
        total_frame_bytes: &mut usize,
    ) -> Result<(), StatamError> {
        let mut seen_chunks = BTreeSet::new();
        let mut seen_frames = BTreeSet::new();
        for &cid in cids {
            let loc = self.chunks.get(cid as usize).ok_or(StatamError::Corrupt {
                reason: "L0 chunk_id out of bounds",
            })?;
            if seen_chunks.insert(cid) {
                *total_chunk_bytes = total_chunk_bytes.checked_add(loc.raw_len as usize).ok_or(
                    StatamError::BoundsExceeded {
                        reason: "L0 chunk decoded bytes overflow",
                    },
                )?;
                if *total_chunk_bytes > MAX_BATCH_DECODED_BYTES {
                    return Err(StatamError::BoundsExceeded {
                        reason: "requested L0 chunk bytes exceed MAX_BATCH_DECODED_BYTES",
                    });
                }
            }
            if seen_frames.insert(loc.frame_id) {
                let (_, _, raw_len) = self.frames[loc.frame_id as usize];
                *total_frame_bytes = total_frame_bytes.checked_add(raw_len as usize).ok_or(
                    StatamError::BoundsExceeded {
                        reason: "L0 frame decoded bytes overflow",
                    },
                )?;
                if *total_frame_bytes > MAX_BATCH_DECODED_BYTES {
                    return Err(StatamError::BoundsExceeded {
                        reason: "requested L0 frame bytes exceed MAX_BATCH_DECODED_BYTES",
                    });
                }
            }
        }
        Ok(())
    }

    pub(super) fn read_chunks(&self, cids: &[u32]) -> Result<HashMap<u32, Bytes>, StatamError> {
        if cids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut unique_cids = BTreeSet::new();
        let mut needed_frames = BTreeSet::new();
        let mut total_chunk_bytes = 0usize;
        let mut total_frame_bytes = 0usize;
        for &cid in cids {
            let loc = self.chunks.get(cid as usize).ok_or(StatamError::Corrupt {
                reason: "L0 chunk_id out of bounds",
            })?;
            if unique_cids.insert(cid) {
                total_chunk_bytes = total_chunk_bytes.checked_add(loc.raw_len as usize).ok_or(
                    StatamError::BoundsExceeded {
                        reason: "L0 chunk decoded bytes overflow",
                    },
                )?;
                if total_chunk_bytes > MAX_BATCH_DECODED_BYTES {
                    return Err(StatamError::BoundsExceeded {
                        reason: "requested L0 chunk bytes exceed MAX_BATCH_DECODED_BYTES",
                    });
                }
            }
            if needed_frames.insert(loc.frame_id) {
                let (_, _, raw_len) = self.frames[loc.frame_id as usize];
                total_frame_bytes = total_frame_bytes.checked_add(raw_len as usize).ok_or(
                    StatamError::BoundsExceeded {
                        reason: "L0 frame decoded bytes overflow",
                    },
                )?;
                if total_frame_bytes > MAX_BATCH_DECODED_BYTES {
                    return Err(StatamError::BoundsExceeded {
                        reason: "requested L0 frame bytes exceed MAX_BATCH_DECODED_BYTES",
                    });
                }
            }
        }

        let mut decoded_frames: HashMap<u32, Bytes> = HashMap::with_capacity(needed_frames.len());
        for fid in needed_frames {
            let decoded = self.decompress_frame(fid)?;
            decoded_frames.insert(fid, Bytes::from(decoded));
        }

        let mut out = HashMap::with_capacity(unique_cids.len());
        for cid in unique_cids {
            let loc = self.chunks[cid as usize];
            let frame_buf = &decoded_frames[&loc.frame_id];
            let s = loc.offset_in_frame as usize;
            let e = s + loc.raw_len as usize;
            if e > frame_buf.len() {
                return Err(StatamError::Corrupt {
                    reason: "L0 chunk slice out of frame bounds",
                });
            }
            out.insert(cid, frame_buf.slice(s..e));
        }
        Ok(out)
    }
}

/// On-demand streaming reader over an RG's local L0 payload region holding at most one
/// decompressed frame in memory (`<= MAX_FRAME_RAW_BYTES`), used by GDPR rewrites.
pub(crate) struct StreamingL0Payload {
    buf: Bytes,
    frames: Vec<(u64, u32, u32)>,
    chunks: Vec<ChunkLoc>,
    cached_frame: Option<(u32, Vec<u8>)>,
}

impl StreamingL0Payload {
    pub(super) fn from_bytes(buf: Bytes) -> Result<Self, StatamError> {
        let (frames, chunks) = {
            let parsed = ParsedL0Payload::parse(&buf)?;
            (parsed.frames, parsed.chunks)
        };
        Ok(Self {
            buf,
            frames,
            chunks,
            cached_frame: None,
        })
    }

    pub(crate) fn read_chunk(&mut self, cid: u32) -> Result<&[u8], StatamError> {
        let loc = *self.chunks.get(cid as usize).ok_or(StatamError::Corrupt {
            reason: "L0 chunk_id out of bounds",
        })?;
        if self
            .cached_frame
            .as_ref()
            .is_none_or(|(fid, _)| *fid != loc.frame_id)
        {
            let (rel_off, stored_len, raw_len) =
                *self
                    .frames
                    .get(loc.frame_id as usize)
                    .ok_or(StatamError::Corrupt {
                        reason: "L0 frame_id out of bounds",
                    })?;
            let start = rel_off as usize;
            let end = start + crate::format::PACK_FRAME_HEADER_LEN + stored_len as usize;
            let (_, _, decoded) =
                read_framed_pack_frame(&self.buf[start..end], MAX_FRAME_RAW_BYTES)?;
            if decoded.len() != raw_len as usize {
                return Err(StatamError::Corrupt {
                    reason: "L0 frame raw_len mismatch",
                });
            }
            self.cached_frame = Some((loc.frame_id, decoded));
        }
        let s = loc.offset_in_frame as usize;
        let e = s + loc.raw_len as usize;
        let Some((_, ref frame_buf)) = self.cached_frame else {
            return Err(StatamError::Corrupt {
                reason: "missing cached L0 frame",
            });
        };
        frame_buf.get(s..e).ok_or(StatamError::Corrupt {
            reason: "L0 chunk slice out of frame bounds",
        })
    }
}
