//! Local L0 and external `.stpk` payload parsing, chunk resolution, substring scan, and reconstruction.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use cetra_column::{
    Batch, BinaryData, Bitmap, Column, DataType, FlatColumn, FlatData, ScalarValue, Schema,
    Selection, Utf8Data,
};

use crate::StatamError;
use crate::codec::dict::FileDict;
use crate::codec::nested::{ShreddedSubCol, decode_shredded_subcol_selected};
use crate::format::{
    ChunkLoc, Codec, Encoding, LocalityGroup, MAX_BATCH_DECODED_BYTES, MAX_FRAME_RAW_BYTES,
    MAX_ROW_PAYLOAD_BYTES, PackDecoderDict, PackFrameDecompressor, read_framed_pack_frame,
};
use crate::parallel::{
    MAX_PARALLEL_THREADS, in_parallel_scope, parallel_map_in_order, parallel_map_in_order_with_init,
};
use crate::payload::pack::ChunkPackReader;
use crate::payload::search::{
    RetainedPackChunks, pack_short_circuits_all, search_pack_rows_flat_multi_masked_with_threads,
};
use crate::payload::{
    FLAT_REF_CHUNKS, FLAT_REF_INLINE, FLAT_REF_NULL, FlatPayloadRefs, PayloadRefRow,
    decode_payload_refs_flat, decode_payload_refs_selected,
};
use crate::writer::{L0_LOCAL_PACK_IDX, L0_PAYLOAD_DIR_MAGIC};

use super::eval::Utf8Matcher;
use super::plan::{ActiveMiniBlock, BlockReader, DecodedPhase1, RgIoPlan};
use super::{StatamReader, effective_read_threads};

/// Maximum projected `ExternalPayload` bytes resolved in a single Phase-2 wave (`128 MiB`).
pub(super) const PHASE2_PAYLOAD_WAVE_TARGET_BYTES: usize = MAX_BATCH_DECODED_BYTES / 2;

/// Minimum reconstructed payload column bytes required before parallelizing byte copy (`1 MiB`).
const PARALLEL_RECONSTRUCT_MIN_BYTES: usize = 1024 * 1024;

enum PackChunkStore {
    Dense {
        min_cid: u32,
        slots: Vec<Option<Bytes>>,
    },
    Sparse(Vec<(u32, Bytes)>),
}

impl std::fmt::Debug for PackChunkStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dense { min_cid, slots } => f
                .debug_struct("PackChunkStore::Dense")
                .field("min_cid", min_cid)
                .field("slot_count", &slots.len())
                .finish(),
            Self::Sparse(entries) => f
                .debug_struct("PackChunkStore::Sparse")
                .field("entry_count", &entries.len())
                .finish(),
        }
    }
}

impl PackChunkStore {
    fn from_entries(mut entries: Vec<(u32, Bytes)>) -> (Self, usize) {
        if entries.is_empty() {
            return (Self::Sparse(Vec::new()), 0);
        }
        entries.sort_unstable_by_key(|&(cid, _)| cid);
        entries.dedup_by_key(|(cid, _)| *cid);

        let total_bytes = entries
            .iter()
            .map(|(_, b)| b.len())
            .fold(0usize, usize::saturating_add);

        let min_cid = entries[0].0;
        let max_cid = entries[entries.len() - 1].0;
        let span_u64 = u64::from(max_cid - min_cid) + 1;
        let max_dense = (entries.len() as u64)
            .saturating_mul(4)
            .clamp(64, 1_048_576);
        if span_u64 <= max_dense
            && let Ok(span) = usize::try_from(span_u64)
        {
            let mut slots: Vec<Option<Bytes>> = vec![None; span];
            for (cid, bytes) in entries {
                let idx = (cid - min_cid) as usize;
                slots[idx] = Some(bytes);
            }
            (Self::Dense { min_cid, slots }, total_bytes)
        } else {
            (Self::Sparse(entries), total_bytes)
        }
    }

    #[inline]
    fn get(&self, cid: u32) -> Option<&Bytes> {
        match self {
            Self::Dense { min_cid, slots } => {
                let idx = usize::try_from(cid.checked_sub(*min_cid)?).ok()?;
                slots.get(idx)?.as_ref()
            }
            Self::Sparse(entries) => {
                let idx = entries.binary_search_by_key(&cid, |&(id, _)| id).ok()?;
                Some(&entries[idx].1)
            }
        }
    }
}

/// Hash-free, overflow-safe resolved chunk lookup table with dedicated per-RG L0 storage.
pub(super) struct ResolvedChunks {
    ext_packs: Vec<(u16, PackChunkStore)>,
    l0: Option<(PackChunkStore, usize)>,
    ext_chunk_bytes: usize,
}

impl std::fmt::Debug for ResolvedChunks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedChunks")
            .field("ext_pack_count", &self.ext_packs.len())
            .field("has_l0", &self.l0.is_some())
            .field("total_bytes", &self.total_bytes())
            .finish()
    }
}

impl ResolvedChunks {
    pub(super) fn empty() -> Self {
        Self {
            ext_packs: Vec::new(),
            l0: None,
            ext_chunk_bytes: 0,
        }
    }

    pub(super) fn from_pack_entries(entries_by_pack: Vec<(u16, Vec<(u32, Bytes)>)>) -> Self {
        let mut grouped: BTreeMap<u16, Vec<(u32, Bytes)>> = BTreeMap::new();
        for (pidx, mut entries) in entries_by_pack {
            if entries.is_empty() {
                continue;
            }
            match grouped.entry(pidx) {
                std::collections::btree_map::Entry::Vacant(v) => {
                    v.insert(entries);
                }
                std::collections::btree_map::Entry::Occupied(mut o) => {
                    o.get_mut().append(&mut entries);
                }
            }
        }

        let mut ext_packs = Vec::with_capacity(grouped.len());
        let mut l0 = None;
        let mut ext_chunk_bytes = 0usize;
        for (pidx, entries) in grouped {
            let (store, bytes) = PackChunkStore::from_entries(entries);
            if pidx == L0_LOCAL_PACK_IDX {
                l0 = Some((store, bytes));
            } else {
                ext_chunk_bytes = ext_chunk_bytes.saturating_add(bytes);
                ext_packs.push((pidx, store));
            }
        }
        Self {
            ext_packs,
            l0,
            ext_chunk_bytes,
        }
    }

    pub(super) fn set_l0(&mut self, entries: Vec<(u32, Bytes)>) {
        if entries.is_empty() {
            self.l0 = None;
        } else {
            self.l0 = Some(PackChunkStore::from_entries(entries));
        }
    }

    pub(super) fn clear_l0(&mut self) {
        self.l0 = None;
    }

    #[inline]
    pub(super) fn get(&self, pack_idx: u16, cid: u32) -> Option<&Bytes> {
        if pack_idx == L0_LOCAL_PACK_IDX {
            return self.l0.as_ref()?.0.get(cid);
        }
        match self.ext_packs.len() {
            0 => None,
            1 => {
                let (pidx, store) = &self.ext_packs[0];
                if *pidx == pack_idx {
                    store.get(cid)
                } else {
                    None
                }
            }
            2..=4 => self
                .ext_packs
                .iter()
                .find(|(pidx, _)| *pidx == pack_idx)
                .and_then(|(_, store)| store.get(cid)),
            _ => {
                let idx = self
                    .ext_packs
                    .binary_search_by_key(&pack_idx, |&(pidx, _)| pidx)
                    .ok()?;
                self.ext_packs[idx].1.get(cid)
            }
        }
    }

    #[inline]
    pub(super) fn total_bytes(&self) -> usize {
        self.ext_chunk_bytes
            .saturating_add(self.l0.as_ref().map_or(0, |(_, b)| *b))
    }
}

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
                &ResolvedChunks::empty(),
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
        let threads = read_threads.clamp(1, MAX_PARALLEL_THREADS);
        if waves.len() <= 1 {
            if active_mbs.len() == 1
                && threads > 1
                && !in_parallel_scope()
                && !proj_indices.is_empty()
            {
                let (mb_idx, mask, decoded_p1) = active_mbs.pop().ok_or(StatamError::Corrupt {
                    reason: "missing single active mini-block",
                })?;
                let batch = self.resolve_and_materialize_single_mb_fused(
                    rg_idx,
                    mb_idx,
                    &mask,
                    decoded_p1,
                    plan,
                    proj_indices,
                    &proj_payload_cols,
                    proj_schema,
                    map_keys_by_col,
                    l0_local,
                    pre_resolved,
                    read_block,
                    threads,
                )?;
                return Ok(vec![batch]);
            }
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
    fn resolve_and_materialize_single_mb_fused<'a>(
        &self,
        rg_idx: usize,
        mb_idx: usize,
        mask: &Bitmap,
        mut decoded_p1: HashMap<usize, DecodedPhase1>,
        plan: &RgIoPlan,
        proj_indices: &[usize],
        proj_payload_cols: &[usize],
        proj_schema: &Arc<Schema>,
        map_keys_by_col: &HashMap<usize, Vec<String>>,
        l0_local: Option<&ParsedL0Payload<'_>>,
        pre_resolved: &HashMap<([u8; 16], u32), Bytes>,
        read_block: &BlockReader<'a>,
        threads: usize,
    ) -> Result<Batch, StatamError> {
        struct PreparedExternalPack {
            pack_idx: u16,
            pack: Arc<ChunkPackReader>,
            dict: Option<Arc<PackDecoderDict>>,
        }

        enum Stage1Task {
            PackFrame {
                pack_slot: usize,
                fid: u32,
                raw: Bytes,
                cids: Vec<u32>,
            },
            L0Chunks {
                cids: Vec<u32>,
            },
            ShreddedSub {
                sub_pidx: usize,
            },
            IndepCol {
                cidx: usize,
                dict: Option<Arc<FileDict>>,
            },
        }

        enum Stage1Out {
            PackChunks(u16, Vec<(u32, Bytes)>),
            L0Chunks(Vec<(u32, Bytes)>),
            ShreddedSub(usize, ShreddedSubCol),
            IndepCol(usize, Column),
        }

        // 1. Collect all referenced payload chunks deterministically by pack_idx.
        let mut by_pack: BTreeMap<u16, BTreeSet<u32>> = BTreeMap::new();
        for &cidx in proj_payload_cols {
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

        let mut prepared_packs: Vec<PreparedExternalPack> = Vec::new();
        let mut pre_resolved_by_pack: Vec<(u16, Vec<(u32, Bytes)>)> = Vec::new();
        let mut pending_ext_fetches: Vec<(u16, Arc<ChunkPackReader>, Vec<u32>)> = Vec::new();
        let mut stage1_tasks: Vec<Stage1Task> = Vec::new();
        let mut stage1_work_bytes = 0usize;
        let mut total_chunk_bytes = 0usize;
        let mut total_frame_bytes = 0usize;

        // First validate cumulative chunk/frame budgets across all packs (L0 + external) before
        // issuing any external pack frame I/O.
        for (pidx, cids_set) in by_pack {
            let cids: Vec<u32> = cids_set.into_iter().collect();
            if pidx == L0_LOCAL_PACK_IDX {
                let l0 = l0_local.ok_or(StatamError::Corrupt {
                    reason: "missing L0 local payload region",
                })?;
                l0.check_chunks_budget(&cids, &mut total_chunk_bytes, &mut total_frame_bytes)?;
                stage1_work_bytes =
                    stage1_work_bytes.saturating_add(l0.compressed_frame_bytes_for_chunks(&cids));
                stage1_tasks.push(Stage1Task::L0Chunks { cids });
            } else {
                let pack_id = *self.meta.schema_meta.pack_ids.get(pidx as usize).ok_or(
                    StatamError::Corrupt {
                        reason: "payload ref pack_idx out of bounds",
                    },
                )?;
                let pack = self.resolve_pack(pack_id)?;
                pack.check_chunks_budget(&cids, &mut total_chunk_bytes, &mut total_frame_bytes)?;

                let missing_cids = if pre_resolved.is_empty() {
                    cids
                } else {
                    let mut hits = Vec::new();
                    let mut missing = Vec::new();
                    for cid in cids {
                        if let Some(cached) = pre_resolved.get(&(pack_id, cid)) {
                            hits.push((cid, cached.clone()));
                        } else {
                            missing.push(cid);
                        }
                    }
                    if !hits.is_empty() {
                        pre_resolved_by_pack.push((pidx, hits));
                    }
                    missing
                };

                if !missing_cids.is_empty() {
                    pending_ext_fetches.push((pidx, pack, missing_cids));
                }
            }
        }

        for (pidx, pack, missing_cids) in pending_ext_fetches {
            let (frame_groups, frame_bytes_list, dict) =
                pack.prepare_and_fetch_frames(&missing_cids)?;
            let pack_slot = prepared_packs.len();
            for ((fid, fcids), raw) in frame_groups.into_iter().zip(frame_bytes_list) {
                let entry = &pack.toc().frames[fid as usize];
                if entry.codec != Codec::None {
                    stage1_work_bytes = stage1_work_bytes.saturating_add(entry.raw_len as usize);
                }
                stage1_tasks.push(Stage1Task::PackFrame {
                    pack_slot,
                    fid,
                    raw,
                    cids: fcids,
                });
            }
            prepared_packs.push(PreparedExternalPack {
                pack_idx: pidx,
                pack,
                dict,
            });
        }

        // 2. Extract P1-cached shredded sub-columns first, then enqueue uncached sub-columns.
        let fields = self.meta.schema_meta.schema.fields();
        let mb_row_count = self.meta.rg_blocks(0, rg_idx)[mb_idx].row_count as usize;
        let mut decoded_subs: HashMap<usize, ShreddedSubCol> = HashMap::new();
        let mut uncached_subs: Vec<usize> = Vec::new();
        for &cidx in proj_indices {
            if fields[cidx].data_type == DataType::MapUtf8
                && let Some(sm) = self
                    .meta
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
        for sub_pidx in uncached_subs {
            stage1_work_bytes = stage1_work_bytes
                .saturating_add(self.meta.rg_blocks(sub_pidx, rg_idx)[mb_idx].raw_len as usize);
            stage1_tasks.push(Stage1Task::ShreddedSub { sub_pidx });
        }

        // 3. Pre-populate cached FileDicts on main thread and enqueue deduplicated independent
        //    non-payload columns (including unshredded MapUtf8; shredded MapUtf8 waits for Stage 2).
        let mut seen_indep: HashSet<usize> = HashSet::new();
        for &cidx in proj_indices {
            let cd = &self.meta.col_dir[cidx];
            if cd.locality_group == LocalityGroup::ExternalPayload || decoded_p1.contains_key(&cidx)
            {
                continue;
            }
            let is_shredded_map = fields[cidx].data_type == DataType::MapUtf8
                && self
                    .meta
                    .schema_meta
                    .shredded_maps
                    .iter()
                    .any(|s| s.logical_col_idx as usize == cidx);
            if is_shredded_map || !seen_indep.insert(cidx) {
                continue;
            }
            let blk = &self.meta.rg_blocks(cidx, rg_idx)[mb_idx];
            let dict = if blk.encoding == Encoding::Dict {
                self.get_cached_dict(cidx)
            } else {
                None
            };
            stage1_work_bytes = stage1_work_bytes.saturating_add(blk.raw_len as usize);
            stage1_tasks.push(Stage1Task::IndepCol { cidx, dict });
        }

        // 4. Execute Stage 1 in ONE bounded scope with a single reusable PackFrameDecompressor per worker.
        stage1_work_bytes = stage1_work_bytes.saturating_add(total_chunk_bytes / 4);
        let stage1_workers = if stage1_tasks.len() >= 16 {
            threads
        } else {
            effective_read_threads(stage1_work_bytes, threads)
        };
        let empty_subs: HashMap<usize, ShreddedSubCol> = HashMap::new();
        let stage1_outputs = parallel_map_in_order_with_init(
            stage1_tasks,
            stage1_workers,
            || Ok(PackFrameDecompressor::new()),
            |dict_decomp, task| match task {
                Stage1Task::PackFrame {
                    pack_slot,
                    fid,
                    raw,
                    cids,
                } => {
                    let prep = &prepared_packs[pack_slot];
                    let pairs = prep.pack.decompress_frame_chunks_as_bytes(
                        fid,
                        &raw,
                        &cids,
                        prep.dict.as_deref(),
                        dict_decomp,
                    )?;
                    Ok(Stage1Out::PackChunks(prep.pack_idx, pairs))
                }
                Stage1Task::L0Chunks { cids } => {
                    let l0 = l0_local.ok_or(StatamError::Corrupt {
                        reason: "missing L0 local payload region",
                    })?;
                    let pairs = l0.read_chunks_vec(&cids)?;
                    Ok(Stage1Out::L0Chunks(pairs))
                }
                Stage1Task::ShreddedSub { sub_pidx } => {
                    let blk = &self.meta.rg_blocks(sub_pidx, rg_idx)[mb_idx];
                    let raw = read_block(blk)?;
                    let sub = decode_shredded_subcol_selected(&raw, mb_row_count, mask)?;
                    Ok(Stage1Out::ShreddedSub(sub_pidx, sub))
                }
                Stage1Task::IndepCol { cidx, dict } => {
                    let col = self.decode_uncached_p2_non_payload_col(
                        rg_idx,
                        mb_idx,
                        mb_row_count,
                        cidx,
                        mask,
                        plan,
                        map_keys_by_col,
                        &empty_subs,
                        dict.as_deref(),
                        read_block,
                    )?;
                    Ok(Stage1Out::IndepCol(cidx, col))
                }
            },
        )?;

        let mut chunk_groups = pre_resolved_by_pack;
        let mut stage1_cols: HashMap<usize, Column> = HashMap::new();
        for out in stage1_outputs {
            match out {
                Stage1Out::PackChunks(pidx, pairs) => chunk_groups.push((pidx, pairs)),
                Stage1Out::L0Chunks(pairs) => chunk_groups.push((L0_LOCAL_PACK_IDX, pairs)),
                Stage1Out::ShreddedSub(sub_pidx, sub) => {
                    decoded_subs.insert(sub_pidx, sub);
                }
                Stage1Out::IndepCol(cidx, col) => {
                    stage1_cols.insert(cidx, col);
                }
            }
        }
        let resolved_chunks = ResolvedChunks::from_pack_entries(chunk_groups);

        // 5. Stage 2: Assemble projected columns in order (including shredded maps and parallel
        //    payload reconstruction using full `threads`, handling duplicate `proj_indices`).
        let mut remaining_uses: HashMap<usize, usize> = HashMap::with_capacity(proj_indices.len());
        for &cidx in proj_indices {
            *remaining_uses.entry(cidx).or_insert(0) += 1;
        }

        let mut out_cols = Vec::with_capacity(proj_indices.len());
        let mut stage2_cached_cols: HashMap<usize, Column> = HashMap::new();
        for &cidx in proj_indices {
            let is_last = match remaining_uses.get_mut(&cidx) {
                Some(cnt) => {
                    *cnt = cnt.saturating_sub(1);
                    *cnt == 0
                }
                None => true,
            };
            let dt = &fields[cidx].data_type;
            let cd = &self.meta.col_dir[cidx];
            if cd.locality_group == LocalityGroup::ExternalPayload {
                if let Some(col) = stage2_cached_cols.get(&cidx) {
                    if is_last {
                        let owned =
                            stage2_cached_cols
                                .remove(&cidx)
                                .ok_or(StatamError::Corrupt {
                                    reason: "missing cached payload column",
                                })?;
                        out_cols.push(owned);
                    } else {
                        out_cols.push(col.clone());
                    }
                    continue;
                }
                let col = match decoded_p1.get(&cidx) {
                    Some(DecodedPhase1::SelectedPayloadRefs(selected)) => {
                        reconstruct_payload_column_selected(
                            mb_row_count,
                            selected,
                            mask,
                            &resolved_chunks,
                            dt,
                            threads,
                        )?
                    }
                    Some(DecodedPhase1::FlatPayloadRefs(refs)) => {
                        reconstruct_payload_column_from_flat(
                            refs,
                            mask,
                            &resolved_chunks,
                            dt,
                            threads,
                        )?
                    }
                    _ => {
                        return Err(StatamError::Corrupt {
                            reason: "missing pre-decoded payload refs in fused Phase 2",
                        });
                    }
                };
                if !is_last {
                    stage2_cached_cols.insert(cidx, col.clone());
                }
                out_cols.push(col);
                continue;
            }

            if let Some(col) = stage1_cols.get(&cidx) {
                if is_last {
                    let owned = stage1_cols.remove(&cidx).ok_or(StatamError::Corrupt {
                        reason: "missing Stage 1 column",
                    })?;
                    out_cols.push(owned);
                } else {
                    out_cols.push(col.clone());
                }
                continue;
            }

            if let Some(col) = stage2_cached_cols.get(&cidx) {
                if is_last {
                    let owned = stage2_cached_cols
                        .remove(&cidx)
                        .ok_or(StatamError::Corrupt {
                            reason: "missing Stage 2 cached column",
                        })?;
                    out_cols.push(owned);
                } else {
                    out_cols.push(col.clone());
                }
                continue;
            }

            if let Some(cached) = decoded_p1.remove(&cidx) {
                let col = self.materialize_cached_p1_col(
                    mb_row_count,
                    cidx,
                    dt,
                    mask,
                    cached,
                    plan,
                    mb_idx,
                    map_keys_by_col,
                    &decoded_subs,
                )?;
                if !is_last {
                    stage2_cached_cols.insert(cidx, col.clone());
                }
                out_cols.push(col);
                continue;
            }

            let col = self.decode_uncached_p2_non_payload_col(
                rg_idx,
                mb_idx,
                mb_row_count,
                cidx,
                mask,
                plan,
                map_keys_by_col,
                &decoded_subs,
                None,
                read_block,
            )?;
            if !is_last {
                stage2_cached_cols.insert(cidx, col.clone());
            }
            out_cols.push(col);
        }

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
    pub(super) fn resolve_rg_payload_chunks<'a>(
        &self,
        rg_idx: usize,
        active_mbs: &mut [(usize, Bitmap, HashMap<usize, DecodedPhase1>)],
        proj_indices: &[usize],
        l0_local: Option<&ParsedL0Payload<'_>>,
        pre_resolved: &HashMap<([u8; 16], u32), Bytes>,
        read_block: &BlockReader<'a>,
        read_threads: usize,
    ) -> Result<ResolvedChunks, StatamError> {
        let mut proj_payload_cols: Vec<usize> = proj_indices
            .iter()
            .copied()
            .filter(|&cidx| {
                self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload
            })
            .collect();
        if proj_payload_cols.is_empty() {
            return Ok(ResolvedChunks::empty());
        }
        proj_payload_cols.sort_unstable();
        proj_payload_cols.dedup();

        self.ensure_rg_payload_refs_decoded(rg_idx, active_mbs, &proj_payload_cols, read_block)?;

        let mut by_pack: BTreeMap<u16, BTreeSet<u32>> = BTreeMap::new();
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
        let mut entries_by_pack: Vec<(u16, Vec<(u32, Bytes)>)> = Vec::new();
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
                    let mut hits = Vec::new();
                    let mut missing_cids = Vec::new();
                    for cid in cids {
                        if let Some(cached) = pre_resolved.get(&(pack_id, cid)) {
                            hits.push((cid, cached.clone()));
                        } else {
                            missing_cids.push(cid);
                        }
                    }
                    if !hits.is_empty() {
                        entries_by_pack.push((pidx, hits));
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
                    let pairs = l0.read_chunks_vec(&cids)?;
                    entries_by_pack.push((L0_LOCAL_PACK_IDX, pairs));
                }
                PackWork::External(pidx, pack, cids) => {
                    let pairs = pack.scan_chunks_as_bytes_parallel(&cids, threads)?;
                    entries_by_pack.push((pidx, pairs));
                }
            }
        }
        Ok(ResolvedChunks::from_pack_entries(entries_by_pack))
    }

    /// Collects all unique `(pack_idx, chunk_id)` references for `active_mbs` across
    /// `proj_payload_cols`, separated into external `.stpk` chunks and local L0 chunks.
    pub(super) fn collect_chunk_refs_for_mbs(
        active_mbs: &[ActiveMiniBlock],
        proj_payload_cols: &[usize],
    ) -> (BTreeMap<u16, BTreeSet<u32>>, BTreeSet<u32>) {
        let mut ext_by_pack: BTreeMap<u16, BTreeSet<u32>> = BTreeMap::new();
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
        ext_by_pack: &BTreeMap<u16, BTreeSet<u32>>,
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
        ext_by_pack: BTreeMap<u16, BTreeSet<u32>>,
        read_threads: usize,
    ) -> Result<ResolvedChunks, StatamError> {
        if ext_by_pack.is_empty() {
            return Ok(ResolvedChunks::empty());
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
        let mut entries_by_pack = Vec::with_capacity(work_items.len());
        for (pidx, pack, cids) in work_items {
            let pairs = pack.scan_chunks_as_bytes_parallel(&cids, threads)?;
            entries_by_pack.push((pidx, pairs));
        }
        Ok(ResolvedChunks::from_pack_entries(entries_by_pack))
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

fn build_reconstruct_shards<'a>(
    row_count: usize,
    offsets: &[u32],
    data_bytes: &'a mut [u8],
    read_threads: usize,
) -> Vec<(usize, usize, &'a mut [u8])> {
    let target_shards = read_threads.clamp(1, MAX_PARALLEL_THREADS).min(row_count);
    let total_bytes = data_bytes.len();
    let target_bytes = total_bytes.div_ceil(target_shards).max(1);

    let mut shards = Vec::with_capacity(target_shards);
    let mut rem = data_bytes;
    let mut r_lo = 0usize;
    let mut shard_idx = 0usize;

    while r_lo < row_count {
        let base_off = offsets[r_lo] as usize;
        let r_hi = if shard_idx + 1 >= target_shards {
            row_count
        } else {
            let want_off = base_off.saturating_add(target_bytes);
            let mut hi = r_lo + 1;
            while hi < row_count && (offsets[hi] as usize) < want_off {
                hi += 1;
            }
            hi
        };
        let shard_len = (offsets[r_hi] as usize) - base_off;
        let (dst, rest) = rem.split_at_mut(shard_len);
        rem = rest;
        if shard_len > 0 {
            shards.push((r_lo, r_hi, dst));
        }
        r_lo = r_hi;
        shard_idx += 1;
    }
    debug_assert!(rem.is_empty());
    shards
}

fn measure_payload_row(
    row: &PayloadRefRow,
    row_idx: usize,
    nulls: &mut Bitmap,
    offsets: &mut Vec<u32>,
    total_decoded: &mut usize,
    active_non_null_rows: &mut usize,
    resolved_chunks: &ResolvedChunks,
) -> Result<(), StatamError> {
    match row {
        PayloadRefRow::Null => {
            offsets.push(*total_decoded as u32);
        }
        PayloadRefRow::Inline(bytes) => {
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
            let next_off =
                u32::try_from(*total_decoded).map_err(|_| StatamError::BoundsExceeded {
                    reason: "payload column exceeds u32::MAX",
                })?;
            let _ = nulls.set(row_idx, true);
            *active_non_null_rows += 1;
            offsets.push(next_off);
        }
        PayloadRefRow::Chunks(chunks) => {
            let mut row_len = 0usize;
            for &(pidx, cid) in chunks {
                let chunk = resolved_chunks.get(pidx, cid).ok_or(StatamError::Corrupt {
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
            }
            let next_off =
                u32::try_from(*total_decoded).map_err(|_| StatamError::BoundsExceeded {
                    reason: "payload column exceeds u32::MAX",
                })?;
            let _ = nulls.set(row_idx, true);
            *active_non_null_rows += 1;
            offsets.push(next_off);
        }
    }
    Ok(())
}

fn copy_payload_row_into_slice(
    row: &PayloadRefRow,
    resolved_chunks: &ResolvedChunks,
    dst: &mut [u8],
    pos: &mut usize,
) -> Result<(), StatamError> {
    match row {
        PayloadRefRow::Null => {}
        PayloadRefRow::Inline(bytes) => {
            let end = *pos + bytes.len();
            dst[*pos..end].copy_from_slice(bytes);
            *pos = end;
        }
        PayloadRefRow::Chunks(chunks) => {
            for &(pidx, cid) in chunks {
                let chunk = resolved_chunks.get(pidx, cid).ok_or(StatamError::Corrupt {
                    reason: "missing resolved payload chunk",
                })?;
                let end = *pos + chunk.len();
                dst[*pos..end].copy_from_slice(chunk);
                *pos = end;
            }
        }
    }
    Ok(())
}

fn extend_payload_row_into_vec(
    row: &PayloadRefRow,
    resolved_chunks: &ResolvedChunks,
    out: &mut Vec<u8>,
) -> Result<(), StatamError> {
    match row {
        PayloadRefRow::Null => {}
        PayloadRefRow::Inline(bytes) => {
            out.extend_from_slice(bytes);
        }
        PayloadRefRow::Chunks(chunks) => {
            for &(pidx, cid) in chunks {
                let chunk = resolved_chunks.get(pidx, cid).ok_or(StatamError::Corrupt {
                    reason: "missing resolved payload chunk",
                })?;
                out.extend_from_slice(chunk);
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn measure_flat_row(
    refs: &FlatPayloadRefs,
    src_row: usize,
    dst_row: usize,
    nulls: &mut Bitmap,
    offsets: &mut Vec<u32>,
    total_decoded: &mut usize,
    active_non_null_rows: &mut usize,
    resolved_chunks: &ResolvedChunks,
) -> Result<(), StatamError> {
    match refs.row_kind(src_row) {
        FLAT_REF_NULL => {
            offsets.push(*total_decoded as u32);
        }
        FLAT_REF_INLINE => {
            let len = refs.inline_bytes(src_row).len();
            *total_decoded = total_decoded
                .checked_add(len)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "decoded payload column overflows usize",
                })?;
            if *total_decoded > MAX_BATCH_DECODED_BYTES {
                return Err(StatamError::BoundsExceeded {
                    reason: "decoded payload column exceeds MAX_BATCH_DECODED_BYTES",
                });
            }
            let next_off =
                u32::try_from(*total_decoded).map_err(|_| StatamError::BoundsExceeded {
                    reason: "payload column exceeds u32::MAX",
                })?;
            let _ = nulls.set(dst_row, true);
            *active_non_null_rows += 1;
            offsets.push(next_off);
        }
        FLAT_REF_CHUNKS => {
            let mut row_len = 0usize;
            let (ps, cs) = refs.chunk_refs(src_row);
            for (&pidx, &cid) in ps.iter().zip(cs.iter()) {
                let chunk = resolved_chunks.get(pidx, cid).ok_or(StatamError::Corrupt {
                    reason: "missing resolved payload chunk during column reconstruction",
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
            }
            let next_off =
                u32::try_from(*total_decoded).map_err(|_| StatamError::BoundsExceeded {
                    reason: "payload column exceeds u32::MAX",
                })?;
            let _ = nulls.set(dst_row, true);
            *active_non_null_rows += 1;
            offsets.push(next_off);
        }
        _ => {
            offsets.push(*total_decoded as u32);
        }
    }
    Ok(())
}

pub(super) fn reconstruct_payload_column_from_flat(
    refs: &FlatPayloadRefs,
    active_mask: &Bitmap,
    resolved_chunks: &ResolvedChunks,
    dt: &DataType,
    read_threads: usize,
) -> Result<Column, StatamError> {
    let row_count = refs.len();
    let mut offsets = Vec::with_capacity(row_count + 1);
    offsets.push(0u32);
    let mut nulls = Bitmap::new_zeroed(row_count);
    let mut total_decoded = 0usize;
    let mut active_non_null_rows = 0usize;

    for r in 0..row_count {
        if !active_mask.get(r) {
            offsets.push(total_decoded as u32);
            continue;
        }
        measure_flat_row(
            refs,
            r,
            r,
            &mut nulls,
            &mut offsets,
            &mut total_decoded,
            &mut active_non_null_rows,
            resolved_chunks,
        )?;
    }

    if active_non_null_rows == 0 {
        return Ok(Column::try_new_const(
            ScalarValue::Null(dt.clone()),
            row_count,
        )?);
    }

    let threads = read_threads.clamp(1, MAX_PARALLEL_THREADS);
    let data_bytes = if threads > 1
        && total_decoded >= PARALLEL_RECONSTRUCT_MIN_BYTES
        && active_non_null_rows >= 4
        && !in_parallel_scope()
    {
        let mut buf = vec![0u8; total_decoded];
        let shards = build_reconstruct_shards(row_count, &offsets, &mut buf, threads);
        let _ = parallel_map_in_order(shards, threads, |(r_lo, r_hi, dst)| {
            let mut pos = 0usize;
            for r in r_lo..r_hi {
                if !active_mask.get(r) {
                    continue;
                }
                match refs.row_kind(r) {
                    FLAT_REF_INLINE => {
                        let bytes = refs.inline_bytes(r);
                        let end = pos + bytes.len();
                        dst[pos..end].copy_from_slice(bytes);
                        pos = end;
                    }
                    FLAT_REF_CHUNKS => {
                        let (ps, cs) = refs.chunk_refs(r);
                        for (&pidx, &cid) in ps.iter().zip(cs.iter()) {
                            let chunk =
                                resolved_chunks.get(pidx, cid).ok_or(StatamError::Corrupt {
                                    reason: "missing resolved payload chunk during column reconstruction",
                                })?;
                            let end = pos + chunk.len();
                            dst[pos..end].copy_from_slice(chunk);
                            pos = end;
                        }
                    }
                    _ => {}
                }
            }
            if pos != dst.len() {
                return Err(StatamError::Corrupt {
                    reason: "payload reconstruction shard length mismatch",
                });
            }
            Ok(())
        })?;
        buf
    } else {
        let mut buf = Vec::with_capacity(total_decoded);
        for r in 0..row_count {
            if !active_mask.get(r) {
                continue;
            }
            match refs.row_kind(r) {
                FLAT_REF_INLINE => buf.extend_from_slice(refs.inline_bytes(r)),
                FLAT_REF_CHUNKS => {
                    let (ps, cs) = refs.chunk_refs(r);
                    for (&pidx, &cid) in ps.iter().zip(cs.iter()) {
                        let chunk = resolved_chunks.get(pidx, cid).ok_or(StatamError::Corrupt {
                            reason: "missing resolved payload chunk during column reconstruction",
                        })?;
                        buf.extend_from_slice(chunk);
                    }
                }
                _ => {}
            }
        }
        buf
    };

    finish_payload_column(dt, offsets, data_bytes, nulls)
}

pub(super) fn reconstruct_payload_column_selected(
    row_count: usize,
    selected: &[(usize, PayloadRefRow)],
    active_mask: &Bitmap,
    resolved_chunks: &ResolvedChunks,
    dt: &DataType,
    read_threads: usize,
) -> Result<Column, StatamError> {
    let mut offsets = Vec::with_capacity(row_count + 1);
    offsets.push(0u32);
    let mut nulls = Bitmap::new_zeroed(row_count);
    let mut total_decoded = 0usize;
    let mut active_non_null_rows = 0usize;
    let mut sel_idx = 0usize;

    for r in 0..row_count {
        while sel_idx < selected.len() && selected[sel_idx].0 < r {
            sel_idx += 1;
        }
        if sel_idx < selected.len() && selected[sel_idx].0 == r {
            let row = &selected[sel_idx].1;
            sel_idx += 1;
            if !active_mask.get(r) {
                offsets.push(total_decoded as u32);
                continue;
            }
            measure_payload_row(
                row,
                r,
                &mut nulls,
                &mut offsets,
                &mut total_decoded,
                &mut active_non_null_rows,
                resolved_chunks,
            )?;
        } else {
            offsets.push(total_decoded as u32);
        }
    }

    if active_non_null_rows == 0 {
        return Ok(Column::try_new_const(
            ScalarValue::Null(dt.clone()),
            row_count,
        )?);
    }

    let threads = read_threads.clamp(1, MAX_PARALLEL_THREADS);
    let data_bytes = if threads > 1
        && total_decoded >= PARALLEL_RECONSTRUCT_MIN_BYTES
        && active_non_null_rows >= 4
        && !in_parallel_scope()
    {
        let mut buf = vec![0u8; total_decoded];
        let shards = build_reconstruct_shards(row_count, &offsets, &mut buf, threads);
        let _ = parallel_map_in_order(shards, threads, |(r_lo, r_hi, dst)| {
            let mut s_idx = selected.partition_point(|&(r, _)| r < r_lo);
            let mut pos = 0usize;
            let mut prev_r: Option<usize> = None;
            while s_idx < selected.len() && selected[s_idx].0 < r_hi {
                let (r, ref row) = selected[s_idx];
                s_idx += 1;
                if prev_r == Some(r) || !active_mask.get(r) {
                    continue;
                }
                prev_r = Some(r);
                copy_payload_row_into_slice(row, resolved_chunks, dst, &mut pos)?;
            }
            if pos != dst.len() {
                return Err(StatamError::Corrupt {
                    reason: "payload reconstruction shard length mismatch",
                });
            }
            Ok(())
        })?;
        buf
    } else {
        let mut buf = Vec::with_capacity(total_decoded);
        let mut s_idx = 0usize;
        let mut prev_r: Option<usize> = None;
        while s_idx < selected.len() && selected[s_idx].0 < row_count {
            let (r, ref row) = selected[s_idx];
            s_idx += 1;
            if prev_r == Some(r) || !active_mask.get(r) {
                continue;
            }
            prev_r = Some(r);
            extend_payload_row_into_vec(row, resolved_chunks, &mut buf)?;
        }
        buf
    };

    finish_payload_column(dt, offsets, data_bytes, nulls)
}

pub(super) fn reconstruct_payload_column_take_from_flat(
    refs: &FlatPayloadRefs,
    local_indices: &[usize],
    resolved_chunks: &ResolvedChunks,
    dt: &DataType,
    read_threads: usize,
) -> Result<Column, StatamError> {
    if local_indices.is_empty() {
        return Ok(Column::Flat(FlatColumn::empty(dt)));
    }
    let mb_row_count = refs.len();
    for &r in local_indices {
        if r >= mb_row_count {
            return Err(StatamError::Corrupt {
                reason: "take_rows local index out of bounds",
            });
        }
    }

    let out_len = local_indices.len();
    let mut offsets = Vec::with_capacity(out_len + 1);
    offsets.push(0u32);
    let mut nulls = Bitmap::new_zeroed(out_len);
    let mut total_decoded = 0usize;
    let mut active_non_null_rows = 0usize;

    for (out_idx, &r) in local_indices.iter().enumerate() {
        measure_flat_row(
            refs,
            r,
            out_idx,
            &mut nulls,
            &mut offsets,
            &mut total_decoded,
            &mut active_non_null_rows,
            resolved_chunks,
        )?;
    }

    if active_non_null_rows == 0 {
        return Ok(Column::try_new_const(
            ScalarValue::Null(dt.clone()),
            out_len,
        )?);
    }

    let threads = read_threads.clamp(1, MAX_PARALLEL_THREADS);
    let data_bytes = if threads > 1
        && total_decoded >= PARALLEL_RECONSTRUCT_MIN_BYTES
        && active_non_null_rows >= 4
        && !in_parallel_scope()
    {
        let mut buf = vec![0u8; total_decoded];
        let shards = build_reconstruct_shards(out_len, &offsets, &mut buf, threads);
        let _ = parallel_map_in_order(shards, threads, |(r_lo, r_hi, dst)| {
            let mut pos = 0usize;
            for &r in &local_indices[r_lo..r_hi] {
                match refs.row_kind(r) {
                    FLAT_REF_INLINE => {
                        let bytes = refs.inline_bytes(r);
                        let end = pos + bytes.len();
                        dst[pos..end].copy_from_slice(bytes);
                        pos = end;
                    }
                    FLAT_REF_CHUNKS => {
                        let (ps, cs) = refs.chunk_refs(r);
                        for (&pidx, &cid) in ps.iter().zip(cs.iter()) {
                            let chunk =
                                resolved_chunks.get(pidx, cid).ok_or(StatamError::Corrupt {
                                    reason: "missing resolved payload chunk during column reconstruction",
                                })?;
                            let end = pos + chunk.len();
                            dst[pos..end].copy_from_slice(chunk);
                            pos = end;
                        }
                    }
                    _ => {}
                }
            }
            if pos != dst.len() {
                return Err(StatamError::Corrupt {
                    reason: "payload reconstruction shard length mismatch",
                });
            }
            Ok(())
        })?;
        buf
    } else {
        let mut buf = Vec::with_capacity(total_decoded);
        for &r in local_indices {
            match refs.row_kind(r) {
                FLAT_REF_INLINE => buf.extend_from_slice(refs.inline_bytes(r)),
                FLAT_REF_CHUNKS => {
                    let (ps, cs) = refs.chunk_refs(r);
                    for (&pidx, &cid) in ps.iter().zip(cs.iter()) {
                        let chunk = resolved_chunks.get(pidx, cid).ok_or(StatamError::Corrupt {
                            reason: "missing resolved payload chunk during column reconstruction",
                        })?;
                        buf.extend_from_slice(chunk);
                    }
                }
                _ => {}
            }
        }
        buf
    };

    finish_payload_column(dt, offsets, data_bytes, nulls)
}

pub(super) fn reconstruct_payload_column_take_selected(
    mb_row_count: usize,
    selected: &[(usize, PayloadRefRow)],
    local_indices: &[usize],
    resolved_chunks: &ResolvedChunks,
    dt: &DataType,
    read_threads: usize,
) -> Result<Column, StatamError> {
    if local_indices.is_empty() {
        return Ok(Column::Flat(FlatColumn::empty(dt)));
    }
    for &r in local_indices {
        if r >= mb_row_count {
            return Err(StatamError::Corrupt {
                reason: "take_rows local index out of bounds",
            });
        }
    }

    let lookup_selected =
        |r: usize, sel_idx: &mut usize, prev_r: &mut usize| -> Option<&PayloadRefRow> {
            if r < *prev_r {
                *sel_idx = selected.partition_point(|&(sr, _)| sr < r);
            } else {
                while *sel_idx < selected.len() && selected[*sel_idx].0 < r {
                    *sel_idx += 1;
                }
            }
            *prev_r = r;
            if *sel_idx < selected.len() && selected[*sel_idx].0 == r {
                Some(&selected[*sel_idx].1)
            } else {
                None
            }
        };

    let out_len = local_indices.len();
    let mut offsets = Vec::with_capacity(out_len + 1);
    offsets.push(0u32);
    let mut nulls = Bitmap::new_zeroed(out_len);
    let mut total_decoded = 0usize;
    let mut active_non_null_rows = 0usize;
    let mut sel_idx = 0usize;
    let mut prev_r = 0usize;

    for (out_idx, &r) in local_indices.iter().enumerate() {
        if let Some(row) = lookup_selected(r, &mut sel_idx, &mut prev_r) {
            measure_payload_row(
                row,
                out_idx,
                &mut nulls,
                &mut offsets,
                &mut total_decoded,
                &mut active_non_null_rows,
                resolved_chunks,
            )?;
        } else {
            offsets.push(total_decoded as u32);
        }
    }

    if active_non_null_rows == 0 {
        return Ok(Column::try_new_const(
            ScalarValue::Null(dt.clone()),
            out_len,
        )?);
    }

    let threads = read_threads.clamp(1, MAX_PARALLEL_THREADS);
    let data_bytes = if threads > 1
        && total_decoded >= PARALLEL_RECONSTRUCT_MIN_BYTES
        && active_non_null_rows >= 4
        && !in_parallel_scope()
    {
        let mut buf = vec![0u8; total_decoded];
        let shards = build_reconstruct_shards(out_len, &offsets, &mut buf, threads);
        let _ = parallel_map_in_order(shards, threads, |(r_lo, r_hi, dst)| {
            let mut s_idx = 0usize;
            let mut p_r = usize::MAX;
            let mut pos = 0usize;
            for &r in &local_indices[r_lo..r_hi] {
                if let Some(row) = lookup_selected(r, &mut s_idx, &mut p_r) {
                    copy_payload_row_into_slice(row, resolved_chunks, dst, &mut pos)?;
                }
            }
            if pos != dst.len() {
                return Err(StatamError::Corrupt {
                    reason: "payload reconstruction shard length mismatch",
                });
            }
            Ok(())
        })?;
        buf
    } else {
        let mut buf = Vec::with_capacity(total_decoded);
        let mut s_idx = 0usize;
        let mut p_r = 0usize;
        for &r in local_indices {
            if let Some(row) = lookup_selected(r, &mut s_idx, &mut p_r) {
                extend_payload_row_into_vec(row, resolved_chunks, &mut buf)?;
            }
        }
        buf
    };

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

    pub(super) fn compressed_frame_bytes_for_chunks(&self, cids: &[u32]) -> usize {
        let mut seen_frames = BTreeSet::new();
        let mut total = 0usize;
        for &cid in cids {
            if let Some(loc) = self.chunks.get(cid as usize)
                && seen_frames.insert(loc.frame_id)
                && let Some(&(rel_off, _, raw_len)) = self.frames.get(loc.frame_id as usize)
                && (rel_off as usize)
                    .checked_add(2)
                    .and_then(|idx| self.buf.get(idx))
                    .copied()
                    != Some(Codec::None as u8)
            {
                total = total.saturating_add(raw_len as usize);
            }
        }
        total
    }

    pub(super) fn read_chunks_vec(&self, cids: &[u32]) -> Result<Vec<(u32, Bytes)>, StatamError> {
        if cids.is_empty() {
            return Ok(Vec::new());
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

        let mut out = Vec::with_capacity(unique_cids.len());
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
            out.push((cid, frame_buf.slice(s..e)));
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
