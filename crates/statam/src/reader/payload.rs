//! Local L0 and external `.stpk` payload parsing, chunk resolution, substring scan, and reconstruction.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use cetra_column::{
    Batch, BinaryData, Bitmap, CancelToken, Column, DataType, FlatColumn, FlatData, ScalarValue,
    Schema, Selection, Utf8Data,
};

use crate::StatamError;
use crate::codec::decode_mini_block_take;
use crate::codec::dict::FileDict;
use crate::codec::nested::{
    decode_shredded_residual_block_keys_only, decode_shredded_residual_block_selected,
    decode_shredded_subcol_in_map_only, decode_shredded_subcol_selected,
};
use crate::format::{
    BlockEntry, ChunkLoc, Codec, Encoding, LocalityGroup, MAX_BATCH_DECODED_BYTES,
    MAX_FRAME_RAW_BYTES, MAX_ROW_PAYLOAD_BYTES, PackDecoderDict, PackFrameDecompressor,
    read_framed_pack_frame,
};
use crate::parallel::{
    MAX_PARALLEL_THREADS, in_parallel_scope, parallel_map_in_order_static,
    parallel_map_in_order_static_with_ctx,
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
use super::materialize::{
    DecodedSubCol, build_shredded_map_col_with_meta, decode_uncached_p2_non_payload_col_with_meta,
    mask_from_indices, materialize_cached_p1_col_with_meta,
};
use super::meta::StatamFileMeta;
use super::plan::{ActiveMiniBlock, BlockCacheReader, DecodedPhase1, RgIoPlan, TouchedMbRows};
use super::{MapReadMode, StatamReader, effective_read_threads};

/// Computes `(unweighted_raw_bytes, lpt_work_len)` for a `.stpk` `PackFrame` task.
#[inline]
fn pack_frame_work_bytes(codec: Codec, raw_len: u32) -> (usize, usize) {
    let raw = raw_len as usize;
    match codec {
        Codec::None => (0, 0),
        Codec::Lz4 => (raw, raw.saturating_mul(2)),
        Codec::Zstd => (raw, raw.saturating_mul(4)),
    }
}

/// Maximum projected `ExternalPayload` bytes resolved in a single Phase-2 wave (`128 MiB`).
pub(super) const PHASE2_PAYLOAD_WAVE_TARGET_BYTES: usize = MAX_BATCH_DECODED_BYTES / 2;

/// Maximum number of external `.stpk` packs allowed on the dense hash-free single-wave fast path.
const MAX_FAST_PATH_PACKS: usize = 1024;
/// Maximum chunk or frame ID count allowed for a single pack bitset (`<= 256 KiB` per bitset).
const MAX_BITSET_IDS: usize = 2 * 1024 * 1024;
/// Hard cumulative memory cap across all allocated bitsets (`ext_slots` + `l0_slot`) on the fast path.
const MAX_TOTAL_BITSET_BYTES: usize = 2 * 1024 * 1024;

/// Output of hash-free single-pass payload wave planning.
pub(super) enum PayloadWavePlan {
    SingleWave {
        /// Ascending `pack_idx` (`0..pack_ids.len()`, then `L0_LOCAL_PACK_IDX`),
        /// each with sorted unique `chunk_id`s (exact `BTreeMap<u16, BTreeSet<u32>>` order).
        by_pack: Vec<(u16, Vec<u32>)>,
        /// Total decoded payload bytes per projected payload column (indexed by `proj_payload_cols`),
        /// including both chunked and inline rows. Used by Stage B scheduling.
        col_decoded: Vec<usize>,
    },
    MultiWave(Vec<Vec<(usize, Bitmap)>>),
}

struct PackBitsetSlot {
    chunk_bits: Vec<u64>,
    frame_bits: Vec<u64>,
    unique_cids: Vec<u32>,
}

impl PackBitsetSlot {
    fn try_new(
        num_chunks: usize,
        num_frames: usize,
        total_bitset_bytes: &mut usize,
        max_bitset_ids: usize,
        max_total_bitset_bytes: usize,
    ) -> Option<Self> {
        if num_chunks > max_bitset_ids || num_frames > max_bitset_ids {
            return None;
        }
        let chunk_words = num_chunks.div_ceil(64);
        let frame_words = num_frames.div_ceil(64);
        let slot_bytes = chunk_words
            .checked_add(frame_words)?
            .checked_mul(size_of::<u64>())?;
        let next_total = total_bitset_bytes.checked_add(slot_bytes)?;
        if next_total > max_total_bitset_bytes {
            return None;
        }
        *total_bitset_bytes = next_total;
        Some(Self {
            chunk_bits: vec![0u64; chunk_words],
            frame_bits: vec![0u64; frame_words],
            unique_cids: Vec::new(),
        })
    }

    #[inline]
    fn mark_chunk(&mut self, cid: u32) -> Result<bool, StatamError> {
        let idx = cid as usize;
        let word = self
            .chunk_bits
            .get_mut(idx >> 6)
            .ok_or(StatamError::Corrupt {
                reason: "chunk_id out of bounds",
            })?;
        let bit = 1u64 << (idx & 63);
        if (*word & bit) == 0 {
            *word |= bit;
            self.unique_cids.push(cid);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    #[inline]
    fn mark_frame(&mut self, fid: u32) -> Result<bool, StatamError> {
        let idx = fid as usize;
        let word = self
            .frame_bits
            .get_mut(idx >> 6)
            .ok_or(StatamError::Corrupt {
                reason: "frame_id out of bounds",
            })?;
        let bit = 1u64 << (idx & 63);
        if (*word & bit) == 0 {
            *word |= bit;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

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
#[derive(Clone)]
pub(super) struct ResolvedChunks {
    ext_packs: Arc<Vec<(u16, PackChunkStore)>>,
    l0: Option<Arc<(PackChunkStore, usize)>>,
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
            ext_packs: Arc::new(Vec::new()),
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
                l0 = Some(Arc::new((store, bytes)));
            } else {
                ext_chunk_bytes = ext_chunk_bytes.saturating_add(bytes);
                ext_packs.push((pidx, store));
            }
        }
        Self {
            ext_packs: Arc::new(ext_packs),
            l0,
            ext_chunk_bytes,
        }
    }

    pub(super) fn set_l0(&mut self, entries: Vec<(u32, Bytes)>) {
        if entries.is_empty() {
            self.l0 = None;
        } else {
            self.l0 = Some(Arc::new(PackChunkStore::from_entries(entries)));
        }
    }

    pub(super) fn clear_l0(&mut self) {
        self.l0 = None;
    }

    #[inline]
    pub(super) fn get(&self, pack_idx: u16, cid: u32) -> Option<&Bytes> {
        if pack_idx == L0_LOCAL_PACK_IDX {
            return self.l0.as_deref()?.0.get(cid);
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
            .saturating_add(self.l0.as_deref().map_or(0, |(_, b)| *b))
    }
}

impl StatamReader {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn resolve_and_materialize_phase2_mbs(
        &self,
        rg_idx: usize,
        mut active_mbs: Vec<ActiveMiniBlock>,
        plan: &Arc<RgIoPlan>,
        proj_indices: &[usize],
        proj_schema: &Arc<Schema>,
        map_proj_by_col: &Arc<HashMap<usize, MapReadMode>>,
        l0_local: Option<&Arc<ParsedL0Payload>>,
        pre_resolved: &HashMap<([u8; 16], u32), Bytes>,
        read_block: &BlockCacheReader,
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
                map_proj_by_col,
                &ResolvedChunks::empty(),
                read_block,
                read_threads,
            );
        }
        proj_payload_cols.sort_unstable();
        proj_payload_cols.dedup();

        let threads = read_threads.clamp(1, MAX_PARALLEL_THREADS);
        let l0_ref = l0_local.map(Arc::as_ref);

        // Decode all projected ExternalPayload `PayloadRefs` mini-blocks once (in parallel when worthwhile).
        self.ensure_rg_payload_refs_decoded(
            rg_idx,
            &mut active_mbs,
            &proj_payload_cols,
            read_block,
            read_threads,
        )?;

        // Plan payload waves in a single hash-free pass when all active rows fit within 128 MiB,
        // falling back to row-by-row multi-wave splitting on > 128 MiB tables or bitset cap hits.
        match self.plan_payload_waves(&active_mbs, &proj_payload_cols, l0_ref)? {
            PayloadWavePlan::SingleWave {
                by_pack,
                col_decoded,
            } => {
                if active_mbs.len() == 1
                    && threads > 1
                    && !in_parallel_scope()
                    && !proj_indices.is_empty()
                {
                    let (mb_idx, mask, decoded_p1) =
                        active_mbs.pop().ok_or(StatamError::Corrupt {
                            reason: "missing single active mini-block",
                        })?;
                    let batch = self.resolve_and_materialize_single_mb_fused(
                        rg_idx,
                        mb_idx,
                        mask,
                        decoded_p1,
                        plan,
                        proj_indices,
                        &proj_payload_cols,
                        by_pack,
                        &col_decoded,
                        proj_schema,
                        map_proj_by_col,
                        l0_local.map(Arc::clone),
                        pre_resolved,
                        read_block,
                        threads,
                    )?;
                    return Ok(vec![batch]);
                }
                let resolved = self.resolve_rg_payload_chunks_from_by_pack(
                    by_pack,
                    l0_ref,
                    pre_resolved,
                    read_threads,
                )?;
                self.materialize_phase2_mbs(
                    rg_idx,
                    active_mbs,
                    plan,
                    proj_indices,
                    proj_schema,
                    map_proj_by_col,
                    &resolved,
                    read_block,
                    read_threads,
                )
            }
            PayloadWavePlan::MultiWave(waves) => {
                if waves.is_empty() {
                    return Ok(Vec::new());
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
                            wave_mbs.push((
                                *mb_idx,
                                sub_mask,
                                clone_decoded_p1_for_wave(decoded_p1),
                            ));
                        }
                    }
                    let resolved = self.resolve_rg_payload_chunks(
                        rg_idx,
                        &mut wave_mbs,
                        proj_indices,
                        l0_ref,
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
                        map_proj_by_col,
                        &resolved,
                        read_block,
                        read_threads,
                    )?;
                    out_batches.append(&mut wave_batches);
                }

                Ok(out_batches)
            }
        }
    }

    pub(super) fn ensure_rg_payload_refs_decoded(
        &self,
        rg_idx: usize,
        active_mbs: &mut [ActiveMiniBlock],
        proj_payload_cols: &[usize],
        read_block: &BlockCacheReader,
        read_threads: usize,
    ) -> Result<(), StatamError> {
        // (active_pos, mb_idx, cidx, is_sparse, raw_len)
        let mut tasks: Vec<(usize, usize, usize, bool, usize)> = Vec::new();
        let mut work_bytes = 0usize;
        for (active_pos, (mb_idx, mask, decoded_p1)) in active_mbs.iter_mut().enumerate() {
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
                        let raw_len = blk.raw_len as usize;
                        work_bytes = work_bytes.saturating_add(raw_len);
                        tasks.push((active_pos, *mb_idx, cidx, is_sparse, raw_len));
                    }
                }
            }
        }
        if tasks.is_empty() {
            return Ok(());
        }

        let workers = effective_read_threads(work_bytes, read_threads);
        if tasks.len() >= 2 && workers > 1 && !in_parallel_scope() {
            // LPT scheduling: decode the largest `PayloadRefs` blocks first.
            tasks.sort_unstable_by_key(|a| std::cmp::Reverse(a.4));
            let mut sparse_masks: Vec<Option<Arc<Bitmap>>> = vec![None; active_mbs.len()];
            let static_tasks: Vec<(usize, usize, usize, Option<Arc<Bitmap>>)> = tasks
                .into_iter()
                .map(|(active_pos, mb_idx, cidx, is_sparse, _)| {
                    let mask_opt = if is_sparse {
                        let slot = &mut sparse_masks[active_pos];
                        let arc =
                            slot.get_or_insert_with(|| Arc::new(active_mbs[active_pos].1.clone()));
                        Some(Arc::clone(arc))
                    } else {
                        None
                    };
                    (active_pos, mb_idx, cidx, mask_opt)
                })
                .collect();
            let meta = Arc::clone(&self.meta);
            let block_reader = read_block.clone();
            let decoded = parallel_map_in_order_static(
                static_tasks,
                workers,
                move |(active_pos, mb_idx, cidx, mask_opt)| {
                    let mb_row_count = meta.rg_blocks(0, rg_idx)[mb_idx].row_count as usize;
                    let blk = &meta.rg_blocks(cidx, rg_idx)[mb_idx];
                    let raw = block_reader.read_block(blk)?;
                    let item = if let Some(mask) = mask_opt.as_deref() {
                        let selected = decode_payload_refs_selected(
                            &raw,
                            mb_row_count,
                            mask,
                            MAX_BATCH_DECODED_BYTES,
                        )?;
                        DecodedPhase1::SelectedPayloadRefs(selected)
                    } else {
                        let refs =
                            decode_payload_refs_flat(&raw, mb_row_count, MAX_BATCH_DECODED_BYTES)?;
                        DecodedPhase1::FlatPayloadRefs(refs)
                    };
                    Ok((active_pos, cidx, item))
                },
            )?;
            for (active_pos, cidx, item) in decoded {
                active_mbs[active_pos].2.insert(cidx, item);
            }
        } else {
            for (active_pos, mb_idx, cidx, is_sparse, _) in tasks {
                let mb_row_count = self.meta.rg_blocks(0, rg_idx)[mb_idx].row_count as usize;
                let blk = &self.meta.rg_blocks(cidx, rg_idx)[mb_idx];
                let raw = read_block.read_block(blk)?;
                let item = if is_sparse {
                    let mask = &active_mbs[active_pos].1;
                    let selected = decode_payload_refs_selected(
                        &raw,
                        mb_row_count,
                        mask,
                        MAX_BATCH_DECODED_BYTES,
                    )?;
                    DecodedPhase1::SelectedPayloadRefs(selected)
                } else {
                    let refs =
                        decode_payload_refs_flat(&raw, mb_row_count, MAX_BATCH_DECODED_BYTES)?;
                    DecodedPhase1::FlatPayloadRefs(refs)
                };
                active_mbs[active_pos].2.insert(cidx, item);
            }
        }
        Ok(())
    }

    pub(super) fn plan_payload_waves(
        &self,
        active_mbs: &[ActiveMiniBlock],
        proj_payload_cols: &[usize],
        l0_local: Option<&ParsedL0Payload>,
    ) -> Result<PayloadWavePlan, StatamError> {
        self.plan_payload_waves_with_caps(
            active_mbs,
            proj_payload_cols,
            l0_local,
            MAX_FAST_PATH_PACKS,
            MAX_BITSET_IDS,
            MAX_TOTAL_BITSET_BYTES,
        )
    }

    pub(super) fn plan_payload_waves_with_caps(
        &self,
        active_mbs: &[ActiveMiniBlock],
        proj_payload_cols: &[usize],
        l0_local: Option<&ParsedL0Payload>,
        max_fast_path_packs: usize,
        max_bitset_ids: usize,
        max_total_bitset_bytes: usize,
    ) -> Result<PayloadWavePlan, StatamError> {
        let num_packs = self.meta.schema_meta.pack_ids.len();
        if num_packs > max_fast_path_packs {
            return Ok(PayloadWavePlan::MultiWave(self.split_payload_waves_multi(
                active_mbs,
                proj_payload_cols,
                l0_local,
            )?));
        }

        let num_pcols = proj_payload_cols.len();
        let mut pack_cache: Vec<Option<Arc<ChunkPackReader>>> = vec![None; num_packs];
        let mut ext_slots: Vec<Option<PackBitsetSlot>> = (0..num_packs).map(|_| None).collect();
        let mut l0_slot: Option<PackBitsetSlot> = None;
        let mut total_bitset_bytes = 0usize;
        let mut total_chunk_bytes = 0usize;
        let mut total_frame_bytes = 0usize;
        let mut col_decoded = vec![0usize; num_pcols];
        let mut sel_cursors = vec![0usize; num_pcols];
        let mut has_active_rows = false;
        let mut fallback_multi = false;

        'outer: for (_, mask, decoded_p1) in active_mbs {
            sel_cursors.fill(0);
            for r in mask.iter_ones() {
                has_active_rows = true;
                for (col_pos, &cidx) in proj_payload_cols.iter().enumerate() {
                    let mut col_row_len = 0usize;
                    match decoded_p1.get(&cidx) {
                        Some(DecodedPhase1::FlatPayloadRefs(refs)) => match refs.row_kind(r) {
                            FLAT_REF_NULL => continue,
                            FLAT_REF_INLINE => {
                                let inline_len = refs.inline_bytes(r).len();
                                if inline_len > MAX_ROW_PAYLOAD_BYTES {
                                    return Err(StatamError::BoundsExceeded {
                                        reason: "reconstructed row payload exceeds MAX_ROW_PAYLOAD_BYTES",
                                    });
                                }
                                match col_decoded[col_pos].checked_add(inline_len) {
                                    Some(next) if next <= PHASE2_PAYLOAD_WAVE_TARGET_BYTES => {
                                        col_decoded[col_pos] = next;
                                    }
                                    _ => {
                                        fallback_multi = true;
                                        break 'outer;
                                    }
                                }
                                continue;
                            }
                            FLAT_REF_CHUNKS => {
                                let (ps, cs) = refs.chunk_refs(r);
                                for (&pidx, &cid) in ps.iter().zip(cs.iter()) {
                                    let (clen, fid, flen, slot) = if pidx == L0_LOCAL_PACK_IDX {
                                        let l0 = l0_local.ok_or(StatamError::Corrupt {
                                            reason: "missing L0 local payload region",
                                        })?;
                                        let loc = l0.chunks.get(cid as usize).ok_or(
                                            StatamError::Corrupt {
                                                reason: "L0 chunk_id out of bounds",
                                            },
                                        )?;
                                        let &(_, _, f_raw) = l0
                                            .frames
                                            .get(loc.frame_id as usize)
                                            .ok_or(StatamError::Corrupt {
                                            reason: "L0 frame_id out of bounds",
                                        })?;
                                        col_row_len = col_row_len
                                            .checked_add(loc.raw_len as usize)
                                            .ok_or(StatamError::BoundsExceeded {
                                                reason: "row payload byte size overflow",
                                            })?;
                                        if col_row_len > MAX_ROW_PAYLOAD_BYTES {
                                            return Err(StatamError::BoundsExceeded {
                                                reason: "reconstructed row payload exceeds MAX_ROW_PAYLOAD_BYTES",
                                            });
                                        }
                                        let slot = match l0_slot.as_mut() {
                                            Some(s) => s,
                                            None => {
                                                let Some(new_slot) = PackBitsetSlot::try_new(
                                                    l0.chunks.len(),
                                                    l0.frames.len(),
                                                    &mut total_bitset_bytes,
                                                    max_bitset_ids,
                                                    max_total_bitset_bytes,
                                                ) else {
                                                    fallback_multi = true;
                                                    break 'outer;
                                                };
                                                l0_slot.insert(new_slot)
                                            }
                                        };
                                        (loc.raw_len as usize, loc.frame_id, f_raw as usize, slot)
                                    } else {
                                        let p_usize = pidx as usize;
                                        if p_usize >= num_packs {
                                            return Err(StatamError::Corrupt {
                                                reason: "payload ref pack_idx out of bounds",
                                            });
                                        }
                                        let pack = match pack_cache[p_usize].as_ref() {
                                            Some(p) => p,
                                            None => {
                                                let pack_id =
                                                    self.meta.schema_meta.pack_ids[p_usize];
                                                let resolved = self.resolve_pack(pack_id)?;
                                                pack_cache[p_usize].insert(resolved)
                                            }
                                        };
                                        let toc = pack.toc();
                                        let loc = toc.chunks.get(cid as usize).ok_or(
                                            StatamError::Corrupt {
                                                reason: "chunk_id out of bounds",
                                            },
                                        )?;
                                        let f_entry = toc.frames.get(loc.frame_id as usize).ok_or(
                                            StatamError::Corrupt {
                                                reason: "frame_id out of bounds",
                                            },
                                        )?;
                                        col_row_len = col_row_len
                                            .checked_add(loc.raw_len as usize)
                                            .ok_or(StatamError::BoundsExceeded {
                                                reason: "row payload byte size overflow",
                                            })?;
                                        if col_row_len > MAX_ROW_PAYLOAD_BYTES {
                                            return Err(StatamError::BoundsExceeded {
                                                reason: "reconstructed row payload exceeds MAX_ROW_PAYLOAD_BYTES",
                                            });
                                        }
                                        let slot = match ext_slots[p_usize].as_mut() {
                                            Some(s) => s,
                                            None => {
                                                let Some(new_slot) = PackBitsetSlot::try_new(
                                                    toc.chunks.len(),
                                                    toc.frames.len(),
                                                    &mut total_bitset_bytes,
                                                    max_bitset_ids,
                                                    max_total_bitset_bytes,
                                                ) else {
                                                    fallback_multi = true;
                                                    break 'outer;
                                                };
                                                ext_slots[p_usize].insert(new_slot)
                                            }
                                        };
                                        (
                                            loc.raw_len as usize,
                                            loc.frame_id,
                                            f_entry.raw_len as usize,
                                            slot,
                                        )
                                    };
                                    if slot.mark_chunk(cid)? {
                                        match total_chunk_bytes.checked_add(clen) {
                                            Some(next)
                                                if next <= PHASE2_PAYLOAD_WAVE_TARGET_BYTES =>
                                            {
                                                total_chunk_bytes = next;
                                            }
                                            _ => {
                                                fallback_multi = true;
                                                break 'outer;
                                            }
                                        }
                                    }
                                    if slot.mark_frame(fid)? {
                                        match total_frame_bytes.checked_add(flen) {
                                            Some(next)
                                                if next <= PHASE2_PAYLOAD_WAVE_TARGET_BYTES =>
                                            {
                                                total_frame_bytes = next;
                                            }
                                            _ => {
                                                fallback_multi = true;
                                                break 'outer;
                                            }
                                        }
                                    }
                                }
                            }
                            _ => continue,
                        },
                        Some(DecodedPhase1::SelectedPayloadRefs(selected)) => {
                            let cur = &mut sel_cursors[col_pos];
                            while *cur < selected.len() && selected[*cur].0 < r {
                                *cur += 1;
                            }
                            if *cur < selected.len() && selected[*cur].0 == r {
                                match &selected[*cur].1 {
                                    PayloadRefRow::Null => continue,
                                    PayloadRefRow::Inline(b) => {
                                        let inline_len = b.len();
                                        if inline_len > MAX_ROW_PAYLOAD_BYTES {
                                            return Err(StatamError::BoundsExceeded {
                                                reason: "reconstructed row payload exceeds MAX_ROW_PAYLOAD_BYTES",
                                            });
                                        }
                                        match col_decoded[col_pos].checked_add(inline_len) {
                                            Some(next)
                                                if next <= PHASE2_PAYLOAD_WAVE_TARGET_BYTES =>
                                            {
                                                col_decoded[col_pos] = next;
                                            }
                                            _ => {
                                                fallback_multi = true;
                                                break 'outer;
                                            }
                                        }
                                        continue;
                                    }
                                    PayloadRefRow::Chunks(chunks) => {
                                        for &(pidx, cid) in chunks {
                                            let (clen, fid, flen, slot) = if pidx
                                                == L0_LOCAL_PACK_IDX
                                            {
                                                let l0 = l0_local.ok_or(StatamError::Corrupt {
                                                    reason: "missing L0 local payload region",
                                                })?;
                                                let loc = l0.chunks.get(cid as usize).ok_or(
                                                    StatamError::Corrupt {
                                                        reason: "L0 chunk_id out of bounds",
                                                    },
                                                )?;
                                                let &(_, _, f_raw) = l0
                                                    .frames
                                                    .get(loc.frame_id as usize)
                                                    .ok_or(StatamError::Corrupt {
                                                        reason: "L0 frame_id out of bounds",
                                                    })?;
                                                col_row_len = col_row_len
                                                    .checked_add(loc.raw_len as usize)
                                                    .ok_or(StatamError::BoundsExceeded {
                                                        reason: "row payload byte size overflow",
                                                    })?;
                                                if col_row_len > MAX_ROW_PAYLOAD_BYTES {
                                                    return Err(StatamError::BoundsExceeded {
                                                        reason: "reconstructed row payload exceeds MAX_ROW_PAYLOAD_BYTES",
                                                    });
                                                }
                                                let slot = match l0_slot.as_mut() {
                                                    Some(s) => s,
                                                    None => {
                                                        let Some(new_slot) =
                                                            PackBitsetSlot::try_new(
                                                                l0.chunks.len(),
                                                                l0.frames.len(),
                                                                &mut total_bitset_bytes,
                                                                max_bitset_ids,
                                                                max_total_bitset_bytes,
                                                            )
                                                        else {
                                                            fallback_multi = true;
                                                            break 'outer;
                                                        };
                                                        l0_slot.insert(new_slot)
                                                    }
                                                };
                                                (
                                                    loc.raw_len as usize,
                                                    loc.frame_id,
                                                    f_raw as usize,
                                                    slot,
                                                )
                                            } else {
                                                let p_usize = pidx as usize;
                                                if p_usize >= num_packs {
                                                    return Err(StatamError::Corrupt {
                                                        reason: "payload ref pack_idx out of bounds",
                                                    });
                                                }
                                                let pack = match pack_cache[p_usize].as_ref() {
                                                    Some(p) => p,
                                                    None => {
                                                        let pack_id =
                                                            self.meta.schema_meta.pack_ids[p_usize];
                                                        let resolved =
                                                            self.resolve_pack(pack_id)?;
                                                        pack_cache[p_usize].insert(resolved)
                                                    }
                                                };
                                                let toc = pack.toc();
                                                let loc = toc.chunks.get(cid as usize).ok_or(
                                                    StatamError::Corrupt {
                                                        reason: "chunk_id out of bounds",
                                                    },
                                                )?;
                                                let f_entry = toc
                                                    .frames
                                                    .get(loc.frame_id as usize)
                                                    .ok_or(StatamError::Corrupt {
                                                        reason: "frame_id out of bounds",
                                                    })?;
                                                col_row_len = col_row_len
                                                    .checked_add(loc.raw_len as usize)
                                                    .ok_or(StatamError::BoundsExceeded {
                                                        reason: "row payload byte size overflow",
                                                    })?;
                                                if col_row_len > MAX_ROW_PAYLOAD_BYTES {
                                                    return Err(StatamError::BoundsExceeded {
                                                        reason: "reconstructed row payload exceeds MAX_ROW_PAYLOAD_BYTES",
                                                    });
                                                }
                                                let slot = match ext_slots[p_usize].as_mut() {
                                                    Some(s) => s,
                                                    None => {
                                                        let Some(new_slot) =
                                                            PackBitsetSlot::try_new(
                                                                toc.chunks.len(),
                                                                toc.frames.len(),
                                                                &mut total_bitset_bytes,
                                                                max_bitset_ids,
                                                                max_total_bitset_bytes,
                                                            )
                                                        else {
                                                            fallback_multi = true;
                                                            break 'outer;
                                                        };
                                                        ext_slots[p_usize].insert(new_slot)
                                                    }
                                                };
                                                (
                                                    loc.raw_len as usize,
                                                    loc.frame_id,
                                                    f_entry.raw_len as usize,
                                                    slot,
                                                )
                                            };
                                            if slot.mark_chunk(cid)? {
                                                match total_chunk_bytes.checked_add(clen) {
                                                    Some(next)
                                                        if next
                                                            <= PHASE2_PAYLOAD_WAVE_TARGET_BYTES =>
                                                    {
                                                        total_chunk_bytes = next;
                                                    }
                                                    _ => {
                                                        fallback_multi = true;
                                                        break 'outer;
                                                    }
                                                }
                                            }
                                            if slot.mark_frame(fid)? {
                                                match total_frame_bytes.checked_add(flen) {
                                                    Some(next)
                                                        if next
                                                            <= PHASE2_PAYLOAD_WAVE_TARGET_BYTES =>
                                                    {
                                                        total_frame_bytes = next;
                                                    }
                                                    _ => {
                                                        fallback_multi = true;
                                                        break 'outer;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                continue;
                            }
                        }
                        _ => continue,
                    }

                    match col_decoded[col_pos].checked_add(col_row_len) {
                        Some(next) if next <= PHASE2_PAYLOAD_WAVE_TARGET_BYTES => {
                            col_decoded[col_pos] = next;
                        }
                        _ => {
                            fallback_multi = true;
                            break 'outer;
                        }
                    }
                }
            }
        }

        if fallback_multi {
            return Ok(PayloadWavePlan::MultiWave(self.split_payload_waves_multi(
                active_mbs,
                proj_payload_cols,
                l0_local,
            )?));
        }
        if !has_active_rows {
            return Ok(PayloadWavePlan::MultiWave(Vec::new()));
        }

        let mut by_pack: Vec<(u16, Vec<u32>)> = Vec::new();
        for (p_usize, slot_opt) in ext_slots.into_iter().enumerate() {
            if let Some(mut slot) = slot_opt
                && !slot.unique_cids.is_empty()
            {
                slot.unique_cids.sort_unstable();
                by_pack.push((p_usize as u16, slot.unique_cids));
            }
        }
        if let Some(mut slot) = l0_slot
            && !slot.unique_cids.is_empty()
        {
            slot.unique_cids.sort_unstable();
            by_pack.push((L0_LOCAL_PACK_IDX, slot.unique_cids));
        }

        Ok(PayloadWavePlan::SingleWave {
            by_pack,
            col_decoded,
        })
    }

    #[allow(clippy::type_complexity)]
    pub(super) fn split_payload_waves_multi(
        &self,
        active_mbs: &[ActiveMiniBlock],
        proj_payload_cols: &[usize],
        l0_local: Option<&ParsedL0Payload>,
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
                let &(_, _, f_raw) =
                    l0.frames
                        .get(loc.frame_id as usize)
                        .ok_or(StatamError::Corrupt {
                            reason: "L0 frame_id out of bounds",
                        })?;
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
                let f_raw = toc
                    .frames
                    .get(loc.frame_id as usize)
                    .ok_or(StatamError::Corrupt {
                        reason: "frame_id out of bounds",
                    })?
                    .raw_len as usize;
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
        let mut sel_cursors = vec![0usize; num_pcols];
        let mut row_chunks: Vec<(u16, u32, usize, u32, usize)> = Vec::new();
        let mut row_unique_chunks: HashSet<(u16, u32)> = HashSet::new();
        let mut row_unique_frames: HashSet<(u16, u32)> = HashSet::new();
        let mut seen_new_chunks: HashSet<(u16, u32)> = HashSet::new();
        let mut seen_new_frames: HashSet<(u16, u32)> = HashSet::new();

        for (pos, (_, mask, decoded_p1)) in active_mbs.iter().enumerate() {
            let mb_row_count = mask.len();
            sel_cursors.fill(0);
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
                                if col_row_len > MAX_ROW_PAYLOAD_BYTES {
                                    return Err(StatamError::BoundsExceeded {
                                        reason: "reconstructed row payload exceeds MAX_ROW_PAYLOAD_BYTES",
                                    });
                                }
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
                            let cur = &mut sel_cursors[col_pos];
                            while *cur < selected.len() && selected[*cur].0 < r {
                                *cur += 1;
                            }
                            if *cur < selected.len() && selected[*cur].0 == r {
                                match &selected[*cur].1 {
                                    PayloadRefRow::Null => {}
                                    PayloadRefRow::Inline(b) => {
                                        col_row_len = b.len();
                                        if col_row_len > MAX_ROW_PAYLOAD_BYTES {
                                            return Err(StatamError::BoundsExceeded {
                                                reason: "reconstructed row payload exceeds MAX_ROW_PAYLOAD_BYTES",
                                            });
                                        }
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
    fn resolve_and_materialize_single_mb_fused(
        &self,
        rg_idx: usize,
        mb_idx: usize,
        mask: Bitmap,
        mut decoded_p1: HashMap<usize, DecodedPhase1>,
        plan: &Arc<RgIoPlan>,
        proj_indices: &[usize],
        proj_payload_cols: &[usize],
        by_pack: Vec<(u16, Vec<u32>)>,
        col_decoded: &[usize],
        proj_schema: &Arc<Schema>,
        map_proj_by_col: &Arc<HashMap<usize, MapReadMode>>,
        l0_local: Option<Arc<ParsedL0Payload>>,
        pre_resolved: &HashMap<([u8; 16], u32), Bytes>,
        read_block: &BlockCacheReader,
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
                in_map_only: bool,
            },
            ShreddedResidual {
                cidx: usize,
                keys_only: bool,
            },
            IndepCol {
                cidx: usize,
                dict: Option<Arc<FileDict>>,
            },
        }

        struct Stage1TaskItem {
            work_len: usize,
            task: Stage1Task,
        }

        enum Stage1Out {
            PackChunks(u16, Vec<(u32, Bytes)>),
            L0Chunks(Vec<(u32, Bytes)>),
            ShreddedSub(usize, DecodedSubCol),
            ShreddedResidual(usize, FlatColumn),
            IndepCol(usize, Column),
        }

        struct Stage1Ctx {
            meta: Arc<StatamFileMeta>,
            prepared_packs: Vec<PreparedExternalPack>,
            l0_local: Option<Arc<ParsedL0Payload>>,
            block_reader: BlockCacheReader,
            mask: Arc<Bitmap>,
            plan: Arc<RgIoPlan>,
            map_proj: Arc<HashMap<usize, MapReadMode>>,
            empty_subs: HashMap<usize, DecodedSubCol>,
            rg_idx: usize,
            mb_idx: usize,
            mb_row_count: usize,
        }

        enum StageBTask {
            PayloadCol {
                cidx: usize,
                refs: DecodedPhase1,
                est_bytes: usize,
            },
            ShreddedMap {
                cidx: usize,
                cached_p1: Option<DecodedPhase1>,
                pre_residual: Option<FlatColumn>,
                est_bytes: usize,
            },
        }

        #[allow(clippy::too_many_arguments)]
        fn run_stage_b_task_static(
            meta: &StatamFileMeta,
            rg_idx: usize,
            mb_idx: usize,
            mb_row_count: usize,
            mask: &Bitmap,
            plan: &RgIoPlan,
            map_proj_by_col: &HashMap<usize, MapReadMode>,
            decoded_subs: &HashMap<usize, DecodedSubCol>,
            resolved_chunks: &ResolvedChunks,
            block_reader: &BlockCacheReader,
            task: StageBTask,
        ) -> Result<(usize, Column), StatamError> {
            let fields = meta.schema_meta.schema.fields();
            match task {
                StageBTask::PayloadCol { cidx, refs, .. } => {
                    let dt = &fields[cidx].data_type;
                    let col = match refs {
                        DecodedPhase1::SelectedPayloadRefs(selected) => {
                            reconstruct_payload_column_selected(
                                mb_row_count,
                                &selected,
                                mask,
                                resolved_chunks,
                                dt,
                            )?
                        }
                        DecodedPhase1::FlatPayloadRefs(refs) => {
                            reconstruct_payload_column_from_flat(&refs, mask, resolved_chunks, dt)?
                        }
                        _ => {
                            return Err(StatamError::Corrupt {
                                reason: "missing pre-decoded payload refs in fused Phase 2",
                            });
                        }
                    };
                    Ok((cidx, col))
                }
                StageBTask::ShreddedMap {
                    cidx,
                    cached_p1,
                    pre_residual,
                    ..
                } => {
                    let col = if let Some(cached) = cached_p1 {
                        materialize_cached_p1_col_with_meta(
                            meta,
                            mb_row_count,
                            cidx,
                            &fields[cidx].data_type,
                            mask,
                            cached,
                            plan,
                            mb_idx,
                            map_proj_by_col,
                            decoded_subs,
                        )?
                    } else if let Some(sm) = meta
                        .schema_meta
                        .shredded_maps
                        .iter()
                        .find(|s| s.logical_col_idx as usize == cidx)
                    {
                        let keys_only =
                            matches!(map_proj_by_col.get(&cidx), Some(MapReadMode::KeysOnly));
                        let blk = &meta.rg_blocks(cidx, rg_idx)[mb_idx];
                        if keys_only && blk.encoding == Encoding::ConstNull {
                            return Ok((
                                cidx,
                                Column::try_new_const(
                                    ScalarValue::Null(DataType::MapUtf8),
                                    mb_row_count,
                                )?,
                            ));
                        }
                        let skip_residual = plan.phase2_map_fast_path.contains(&(mb_idx, cidx));
                        let residual_flat = if skip_residual {
                            None
                        } else if let Some(pre) = pre_residual {
                            Some(pre)
                        } else {
                            let raw = if blk.stored_len > 0 {
                                block_reader.read_block(blk)?
                            } else {
                                Cow::Borrowed(&[][..])
                            };
                            Some(if keys_only {
                                decode_shredded_residual_block_keys_only(&raw, blk, mask)?
                            } else {
                                decode_shredded_residual_block_selected(&raw, blk, mask)?
                            })
                        };
                        build_shredded_map_col_with_meta(
                            mb_row_count,
                            cidx,
                            sm,
                            skip_residual,
                            residual_flat.as_ref(),
                            mask,
                            map_proj_by_col,
                            decoded_subs,
                        )?
                    } else {
                        decode_uncached_p2_non_payload_col_with_meta(
                            meta,
                            rg_idx,
                            mb_idx,
                            mb_row_count,
                            cidx,
                            mask,
                            plan,
                            map_proj_by_col,
                            decoded_subs,
                            None,
                            block_reader,
                        )?
                    };
                    Ok((cidx, col))
                }
            }
        }

        let mask = Arc::new(mask);
        let mut prepared_packs: Vec<PreparedExternalPack> = Vec::new();
        let mut pre_resolved_by_pack: Vec<(u16, Vec<(u32, Bytes)>)> = Vec::new();
        let mut pending_ext_fetches: Vec<(u16, Arc<ChunkPackReader>, Vec<u32>)> = Vec::new();
        let mut stage1_tasks: Vec<Stage1TaskItem> = Vec::new();
        let mut stage1_work_bytes = 0usize;
        let mut total_chunk_bytes = 0usize;
        let mut total_frame_bytes = 0usize;

        // 1. Validate cumulative chunk/frame budgets across all packs (L0 + external) in
        //    deterministic ascending `pack_idx` order before issuing any external pack frame I/O.
        for (pidx, cids) in by_pack {
            if cids.is_empty() {
                continue;
            }
            if pidx == L0_LOCAL_PACK_IDX {
                let l0 = l0_local.as_deref().ok_or(StatamError::Corrupt {
                    reason: "missing L0 local payload region",
                })?;
                l0.check_chunks_budget(&cids, &mut total_chunk_bytes, &mut total_frame_bytes)?;
                let work_len = l0.compressed_frame_bytes_for_chunks(&cids);
                stage1_work_bytes = stage1_work_bytes.saturating_add(work_len);
                stage1_tasks.push(Stage1TaskItem {
                    work_len,
                    task: Stage1Task::L0Chunks { cids },
                });
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

        let mut compressed_pack_frames = 0usize;
        for (pidx, pack, missing_cids) in pending_ext_fetches {
            let (frame_groups, frame_bytes_list, dict) =
                pack.prepare_and_fetch_frames(&missing_cids)?;
            let pack_slot = prepared_packs.len();
            for ((fid, fcids), raw) in frame_groups.into_iter().zip(frame_bytes_list) {
                let entry = &pack.toc().frames[fid as usize];
                if entry.codec != Codec::None {
                    compressed_pack_frames += 1;
                }
                let (unweighted_bytes, work_len) =
                    pack_frame_work_bytes(entry.codec, entry.raw_len);
                stage1_work_bytes = stage1_work_bytes.saturating_add(unweighted_bytes);
                stage1_tasks.push(Stage1TaskItem {
                    work_len,
                    task: Stage1Task::PackFrame {
                        pack_slot,
                        fid,
                        raw,
                        cids: fcids,
                    },
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
        let mut decoded_subs: HashMap<usize, DecodedSubCol> = HashMap::new();
        let mut uncached_subs: Vec<(usize, bool)> = Vec::new();
        for &cidx in proj_indices {
            if fields[cidx].data_type == DataType::MapUtf8
                && let Some(sm) = self
                    .meta
                    .schema_meta
                    .shredded_maps
                    .iter()
                    .find(|s| s.logical_col_idx as usize == cidx)
            {
                let mode = map_proj_by_col.get(&cidx).unwrap_or(&MapReadMode::Full);
                match mode {
                    MapReadMode::KeysOnly => {
                        let res_blk = &self.meta.rg_blocks(cidx, rg_idx)[mb_idx];
                        if res_blk.encoding == Encoding::ConstNull {
                            continue;
                        }
                        for ke in &sm.keys {
                            let sub_pidx = ke.sub_col_idx as usize;
                            if decoded_subs.contains_key(&sub_pidx)
                                || uncached_subs.iter().any(|&(p, _)| p == sub_pidx)
                            {
                                continue;
                            }
                            let sblk = &self.meta.rg_blocks(sub_pidx, rg_idx)[mb_idx];
                            if sblk.null_count == 0 {
                                decoded_subs.insert(
                                    sub_pidx,
                                    DecodedSubCol::InMapOnly(Bitmap::new_ones(mb_row_count)),
                                );
                            } else if sblk.null_count == sblk.row_count {
                                decoded_subs.insert(
                                    sub_pidx,
                                    DecodedSubCol::InMapOnly(Bitmap::new_zeroed(mb_row_count)),
                                );
                            } else if let Some(DecodedPhase1::ShreddedSub(sub)) =
                                decoded_p1.remove(&sub_pidx)
                            {
                                decoded_subs.insert(sub_pidx, DecodedSubCol::Full(sub));
                            } else {
                                uncached_subs.push((sub_pidx, true));
                            }
                        }
                    }
                    MapReadMode::Keys(req_keys) => {
                        for ke in &sm.keys {
                            if !req_keys.iter().any(|rk| rk == &ke.key) {
                                continue;
                            }
                            let sub_pidx = ke.sub_col_idx as usize;
                            if decoded_subs.contains_key(&sub_pidx)
                                || uncached_subs.iter().any(|&(p, _)| p == sub_pidx)
                            {
                                continue;
                            }
                            if let Some(DecodedPhase1::ShreddedSub(sub)) =
                                decoded_p1.remove(&sub_pidx)
                            {
                                decoded_subs.insert(sub_pidx, DecodedSubCol::Full(sub));
                            } else {
                                uncached_subs.push((sub_pidx, false));
                            }
                        }
                    }
                    MapReadMode::Full => {
                        for ke in &sm.keys {
                            let sub_pidx = ke.sub_col_idx as usize;
                            if decoded_subs.contains_key(&sub_pidx)
                                || uncached_subs.iter().any(|&(p, _)| p == sub_pidx)
                            {
                                continue;
                            }
                            if let Some(DecodedPhase1::ShreddedSub(sub)) =
                                decoded_p1.remove(&sub_pidx)
                            {
                                decoded_subs.insert(sub_pidx, DecodedSubCol::Full(sub));
                            } else {
                                uncached_subs.push((sub_pidx, false));
                            }
                        }
                    }
                }
            }
        }
        for (sub_pidx, in_map_only) in uncached_subs {
            let work_len = self.meta.rg_blocks(sub_pidx, rg_idx)[mb_idx].raw_len as usize;
            stage1_work_bytes = stage1_work_bytes.saturating_add(work_len);
            stage1_tasks.push(Stage1TaskItem {
                work_len,
                task: Stage1Task::ShreddedSub {
                    sub_pidx,
                    in_map_only,
                },
            });
        }

        // 3. Pre-populate cached FileDicts on main thread and enqueue deduplicated independent
        //    non-payload columns plus non-skipped shredded MapUtf8 residuals (whose merge runs in Stage B).
        let mut seen_indep: HashSet<usize> = HashSet::new();
        let mut seen_shredded_res: HashSet<usize> = HashSet::new();
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
            if is_shredded_map {
                if seen_shredded_res.insert(cidx) {
                    let keys_only =
                        matches!(map_proj_by_col.get(&cidx), Some(MapReadMode::KeysOnly));
                    let skip_residual = plan.phase2_map_fast_path.contains(&(mb_idx, cidx));
                    let blk = &self.meta.rg_blocks(cidx, rg_idx)[mb_idx];
                    if !skip_residual
                        && !(keys_only && blk.encoding == Encoding::ConstNull)
                        && blk.stored_len > 0
                    {
                        let work_len = blk.raw_len as usize;
                        stage1_work_bytes = stage1_work_bytes.saturating_add(work_len);
                        stage1_tasks.push(Stage1TaskItem {
                            work_len,
                            task: Stage1Task::ShreddedResidual { cidx, keys_only },
                        });
                    }
                }
                continue;
            }
            if !seen_indep.insert(cidx) {
                continue;
            }
            let blk = &self.meta.rg_blocks(cidx, rg_idx)[mb_idx];
            let dict = if blk.encoding == Encoding::Dict {
                self.get_cached_dict(cidx)
            } else {
                None
            };
            let work_len = blk.raw_len as usize;
            stage1_work_bytes = stage1_work_bytes.saturating_add(work_len);
            stage1_tasks.push(Stage1TaskItem {
                work_len,
                task: Stage1Task::IndepCol { cidx, dict },
            });
        }

        // 4. LPT-schedule Stage 1 so the largest blocks/frames start first at t=0, using the
        //    persistent WorkerPool when `stage1_tasks.len() >= 2 && stage1_workers > 1`.
        stage1_tasks.sort_unstable_by_key(|a| std::cmp::Reverse(a.work_len));
        let stage1_workers = if mask.count_ones() <= 16 && stage1_work_bytes < 4 * 1024 * 1024 {
            threads.min(compressed_pack_frames.saturating_add(2))
        } else if stage1_tasks.len() >= 16 {
            threads
        } else {
            effective_read_threads(stage1_work_bytes, threads)
        };
        let effective_stage1_workers = if stage1_tasks.len() >= 2 && stage1_workers > 1 {
            stage1_workers
        } else {
            1
        };
        let stage1_ctx = Stage1Ctx {
            meta: Arc::clone(&self.meta),
            prepared_packs,
            l0_local,
            block_reader: read_block.clone(),
            mask: Arc::clone(&mask),
            plan: Arc::clone(plan),
            map_proj: Arc::clone(map_proj_by_col),
            empty_subs: HashMap::new(),
            rg_idx,
            mb_idx,
            mb_row_count,
        };
        let stage1_outputs = parallel_map_in_order_static_with_ctx(
            stage1_ctx,
            stage1_tasks,
            effective_stage1_workers,
            |ctx, worker| {
                worker.run_with_init(
                    || Ok(PackFrameDecompressor::new()),
                    |dict_decomp, item| match item.task {
                        Stage1Task::PackFrame {
                            pack_slot,
                            fid,
                            raw,
                            cids,
                        } => {
                            let prep = &ctx.prepared_packs[pack_slot];
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
                            let l0 = ctx.l0_local.as_deref().ok_or(StatamError::Corrupt {
                                reason: "missing L0 local payload region",
                            })?;
                            let pairs = l0.read_chunks_vec(&cids)?;
                            Ok(Stage1Out::L0Chunks(pairs))
                        }
                        Stage1Task::ShreddedSub {
                            sub_pidx,
                            in_map_only,
                        } => {
                            let blk = &ctx.meta.rg_blocks(sub_pidx, ctx.rg_idx)[ctx.mb_idx];
                            let raw = ctx.block_reader.read_block(blk)?;
                            let sub = if in_map_only {
                                DecodedSubCol::InMapOnly(decode_shredded_subcol_in_map_only(
                                    &raw,
                                    ctx.mb_row_count,
                                )?)
                            } else {
                                DecodedSubCol::Full(decode_shredded_subcol_selected(
                                    &raw,
                                    ctx.mb_row_count,
                                    &ctx.mask,
                                )?)
                            };
                            Ok(Stage1Out::ShreddedSub(sub_pidx, sub))
                        }
                        Stage1Task::ShreddedResidual { cidx, keys_only } => {
                            let blk = &ctx.meta.rg_blocks(cidx, ctx.rg_idx)[ctx.mb_idx];
                            let raw = ctx.block_reader.read_block(blk)?;
                            let res = if keys_only {
                                decode_shredded_residual_block_keys_only(&raw, blk, &ctx.mask)?
                            } else {
                                decode_shredded_residual_block_selected(&raw, blk, &ctx.mask)?
                            };
                            Ok(Stage1Out::ShreddedResidual(cidx, res))
                        }
                        Stage1Task::IndepCol { cidx, dict } => {
                            let col = decode_uncached_p2_non_payload_col_with_meta(
                                &ctx.meta,
                                ctx.rg_idx,
                                ctx.mb_idx,
                                ctx.mb_row_count,
                                cidx,
                                &ctx.mask,
                                &ctx.plan,
                                &ctx.map_proj,
                                &ctx.empty_subs,
                                dict.as_deref(),
                                &ctx.block_reader,
                            )?;
                            Ok(Stage1Out::IndepCol(cidx, col))
                        }
                    },
                )
            },
        )?;

        let mut chunk_groups = pre_resolved_by_pack;
        let mut stage1_cols: HashMap<usize, Column> = HashMap::new();
        let mut decoded_residuals: HashMap<usize, FlatColumn> = HashMap::new();
        for out in stage1_outputs {
            match out {
                Stage1Out::PackChunks(pidx, pairs) => chunk_groups.push((pidx, pairs)),
                Stage1Out::L0Chunks(pairs) => chunk_groups.push((L0_LOCAL_PACK_IDX, pairs)),
                Stage1Out::ShreddedSub(sub_pidx, sub) => {
                    decoded_subs.insert(sub_pidx, sub);
                }
                Stage1Out::ShreddedResidual(cidx, res) => {
                    decoded_residuals.insert(cidx, res);
                }
                Stage1Out::IndepCol(cidx, col) => {
                    stage1_cols.insert(cidx, col);
                }
            }
        }
        let resolved_chunks = ResolvedChunks::from_pack_entries(chunk_groups);
        let decoded_subs = Arc::new(decoded_subs);

        // 5. Smart Parallel Stage B: reconstruct `ExternalPayload` cols and shredded `MapUtf8` cols.
        //    Move deduplicated `DecodedPhase1` payloads into tasks so Stage B dispatches cleanly
        //    onto the persistent `'static` WorkerPool.
        let mut seen_stage_b: HashSet<usize> = HashSet::new();
        let mut small_stage_b: Vec<StageBTask> = Vec::new();
        for &cidx in proj_indices {
            let cd = &self.meta.col_dir[cidx];
            if cd.locality_group == LocalityGroup::ExternalPayload {
                if !seen_stage_b.insert(cidx) {
                    continue;
                }
                let refs = decoded_p1.remove(&cidx).ok_or(StatamError::Corrupt {
                    reason: "missing pre-decoded payload refs in fused Phase 2",
                })?;
                let est_bytes = proj_payload_cols
                    .binary_search(&cidx)
                    .ok()
                    .and_then(|pos| col_decoded.get(pos).copied())
                    .unwrap_or(0);
                small_stage_b.push(StageBTask::PayloadCol {
                    cidx,
                    refs,
                    est_bytes,
                });
            } else if fields[cidx].data_type == DataType::MapUtf8
                && self
                    .meta
                    .schema_meta
                    .shredded_maps
                    .iter()
                    .any(|s| s.logical_col_idx as usize == cidx)
            {
                if !seen_stage_b.insert(cidx) {
                    continue;
                }
                let cached_p1 = decoded_p1.remove(&cidx);
                let pre_residual = decoded_residuals.remove(&cidx);
                let raw_len = self.meta.rg_blocks(cidx, rg_idx)[mb_idx].raw_len as usize;
                let selected_rows = mask.count_ones();
                let est_bytes = if mb_row_count > 0 && selected_rows < mb_row_count {
                    raw_len.saturating_mul(selected_rows).div_ceil(mb_row_count)
                } else {
                    raw_len
                };
                small_stage_b.push(StageBTask::ShreddedMap {
                    cidx,
                    cached_p1,
                    pre_residual,
                    est_bytes,
                });
            }
        }

        let mut stage_b_cols: HashMap<usize, Column> = HashMap::with_capacity(small_stage_b.len());

        let small_stage_b_bytes: usize = small_stage_b
            .iter()
            .map(|t| match t {
                StageBTask::PayloadCol { est_bytes, .. }
                | StageBTask::ShreddedMap { est_bytes, .. } => *est_bytes,
            })
            .fold(0usize, usize::saturating_add);

        if small_stage_b.len() >= 2 && small_stage_b_bytes >= 64 * 1024 {
            small_stage_b.sort_unstable_by(|a, b| {
                let ea = match a {
                    StageBTask::PayloadCol { est_bytes, .. }
                    | StageBTask::ShreddedMap { est_bytes, .. } => *est_bytes,
                };
                let eb = match b {
                    StageBTask::PayloadCol { est_bytes, .. }
                    | StageBTask::ShreddedMap { est_bytes, .. } => *est_bytes,
                };
                eb.cmp(&ea)
            });
            let stage_b_workers = threads.min(small_stage_b.len());
            let meta_b = Arc::clone(&self.meta);
            let mask_b = Arc::clone(&mask);
            let plan_b = Arc::clone(plan);
            let map_proj_b = Arc::clone(map_proj_by_col);
            let decoded_subs_b = Arc::clone(&decoded_subs);
            let resolved_b = resolved_chunks;
            let block_reader_b = read_block.clone();
            let decoded_b =
                parallel_map_in_order_static(small_stage_b, stage_b_workers, move |task| {
                    run_stage_b_task_static(
                        &meta_b,
                        rg_idx,
                        mb_idx,
                        mb_row_count,
                        &mask_b,
                        &plan_b,
                        &map_proj_b,
                        &decoded_subs_b,
                        &resolved_b,
                        &block_reader_b,
                        task,
                    )
                })?;
            for (cidx, col) in decoded_b {
                stage_b_cols.insert(cidx, col);
            }
        } else {
            for task in small_stage_b {
                let (cidx, col) = run_stage_b_task_static(
                    &self.meta,
                    rg_idx,
                    mb_idx,
                    mb_row_count,
                    &mask,
                    plan,
                    map_proj_by_col,
                    &decoded_subs,
                    &resolved_chunks,
                    read_block,
                    task,
                )?;
                stage_b_cols.insert(cidx, col);
            }
        }

        // 6. Assemble projected columns in `proj_indices` order, handling duplicates via `remaining_uses`.
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

            if let Some(col) = stage_b_cols.get(&cidx) {
                if is_last {
                    let owned = stage_b_cols.remove(&cidx).ok_or(StatamError::Corrupt {
                        reason: "missing Stage B column",
                    })?;
                    out_cols.push(owned);
                } else {
                    out_cols.push(col.clone());
                }
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

            let dt = &fields[cidx].data_type;
            if let Some(cached) = decoded_p1.remove(&cidx) {
                let col = materialize_cached_p1_col_with_meta(
                    &self.meta,
                    mb_row_count,
                    cidx,
                    dt,
                    &mask,
                    cached,
                    plan,
                    mb_idx,
                    map_proj_by_col,
                    &decoded_subs,
                )?;
                if !is_last {
                    stage2_cached_cols.insert(cidx, col.clone());
                }
                out_cols.push(col);
                continue;
            }

            let blk = &self.meta.rg_blocks(cidx, rg_idx)[mb_idx];
            let dict = if blk.encoding == Encoding::Dict {
                self.get_cached_dict(cidx)
            } else {
                None
            };
            let col = decode_uncached_p2_non_payload_col_with_meta(
                &self.meta,
                rg_idx,
                mb_idx,
                mb_row_count,
                cidx,
                &mask,
                plan,
                map_proj_by_col,
                &decoded_subs,
                dict.as_deref(),
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
            Selection::Mask(Arc::try_unwrap(mask).unwrap_or_else(|a| (*a).clone()))
        };
        Ok(Batch::try_new(
            Arc::clone(proj_schema),
            out_cols,
            mb_row_count,
            sel,
        )?)
    }

    /// Fused LPT single-RG `take_rows` fast path: collapses external `.stpk` frame decompression,
    /// shredded map sub-column decode, and non-payload `decode_mini_block_take` across all touched
    /// mini-blocks into ONE LPT-sorted Stage-1 dispatch on the persistent `WorkerPool`, then
    /// assembles per-MB batches (sequentially for a single MB or below the byte threshold, or in
    /// parallel across MBs when `mb_jobs.len() >= 2 && step4_bytes >= step4_min_bytes`).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn resolve_and_materialize_take_rg_fused(
        &self,
        rg_idx: usize,
        mb_rows: Vec<TouchedMbRows>,
        active_mbs: Vec<ActiveMiniBlock>,
        plan: &Arc<RgIoPlan>,
        proj_indices: &[usize],
        proj_schema: &Arc<Schema>,
        empty_map_proj: &Arc<HashMap<usize, MapReadMode>>,
        merged_ext_by_pack: BTreeMap<u16, BTreeSet<u32>>,
        read_block: &BlockCacheReader,
        threads: usize,
    ) -> Result<Vec<Batch>, StatamError> {
        struct PreparedExternalPack {
            pack_idx: u16,
            pack: Arc<ChunkPackReader>,
            dict: Option<Arc<PackDecoderDict>>,
        }

        enum TakeStage1Task {
            PackFrame {
                pack_slot: usize,
                fid: u32,
                raw: Bytes,
                cids: Vec<u32>,
            },
            ShreddedSub {
                mb_pos: usize,
                sub_pidx: usize,
                blk: BlockEntry,
                mb_row_count: usize,
                mask: Arc<Bitmap>,
            },
            ShreddedResidual {
                mb_pos: usize,
                cidx: usize,
                blk: BlockEntry,
                mask: Arc<Bitmap>,
            },
            NonPayloadCol {
                mb_pos: usize,
                cidx: usize,
                blk: BlockEntry,
                dt: DataType,
                dict: Option<Arc<FileDict>>,
                local_indices: Arc<Vec<usize>>,
            },
        }

        struct TakeStage1Item {
            work_len: usize,
            task: TakeStage1Task,
        }

        enum TakeStage1Out {
            PackChunks(u16, Vec<(u32, Bytes)>),
            ShreddedSub(usize, usize, DecodedSubCol),
            ShreddedResidual(usize, usize, FlatColumn),
            NonPayloadCol(usize, usize, Column),
        }

        struct TakeStage1Ctx {
            prepared_packs: Vec<PreparedExternalPack>,
            block_reader: BlockCacheReader,
        }

        struct FusedTakeMbJob {
            mb_idx: usize,
            mb_row_count: usize,
            local_indices: Arc<Vec<usize>>,
            u32_indices: Option<Vec<u32>>,
            mask_opt: Option<Arc<Bitmap>>,
            payload_p1: HashMap<usize, DecodedPhase1>,
            mb_cols: HashMap<usize, Column>,
            mb_subs: HashMap<usize, DecodedSubCol>,
            mb_residuals: HashMap<usize, FlatColumn>,
        }

        #[allow(clippy::too_many_arguments)]
        fn assemble_fused_take_mb(
            meta: &StatamFileMeta,
            rg_idx: usize,
            plan: &RgIoPlan,
            proj_indices: &[usize],
            proj_schema: &Arc<Schema>,
            empty_map_proj: &HashMap<usize, MapReadMode>,
            resolved_chunks: &ResolvedChunks,
            block_reader: &BlockCacheReader,
            mut job: FusedTakeMbJob,
        ) -> Result<Batch, StatamError> {
            let fields = meta.schema_meta.schema.fields();
            let mut remaining_uses: HashMap<usize, usize> =
                HashMap::with_capacity(proj_indices.len());
            for &cidx in proj_indices {
                *remaining_uses.entry(cidx).or_insert(0) += 1;
            }

            let mut out_cols = Vec::with_capacity(proj_indices.len());
            let mut reconstructed_cache: HashMap<usize, Column> = HashMap::new();
            for &cidx in proj_indices {
                let is_last = match remaining_uses.get_mut(&cidx) {
                    Some(cnt) => {
                        *cnt = cnt.saturating_sub(1);
                        *cnt == 0
                    }
                    None => true,
                };

                if let Some(col) = job.mb_cols.get(&cidx) {
                    if is_last {
                        let owned = job.mb_cols.remove(&cidx).ok_or(StatamError::Corrupt {
                            reason: "missing fused take non-payload column",
                        })?;
                        out_cols.push(owned);
                    } else {
                        out_cols.push(col.clone());
                    }
                    continue;
                }

                if let Some(col) = reconstructed_cache.get(&cidx) {
                    if is_last {
                        let owned =
                            reconstructed_cache
                                .remove(&cidx)
                                .ok_or(StatamError::Corrupt {
                                    reason: "missing fused take cached column",
                                })?;
                        out_cols.push(owned);
                    } else {
                        out_cols.push(col.clone());
                    }
                    continue;
                }

                let dt = &fields[cidx].data_type;
                let col = if meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload {
                    if job.local_indices.is_empty() {
                        Column::Flat(FlatColumn::empty(dt))
                    } else {
                        match job.payload_p1.remove(&cidx) {
                            Some(DecodedPhase1::SelectedPayloadRefs(selected)) => {
                                reconstruct_payload_column_take_selected(
                                    job.mb_row_count,
                                    &selected,
                                    &job.local_indices,
                                    resolved_chunks,
                                    dt,
                                )?
                            }
                            Some(DecodedPhase1::FlatPayloadRefs(refs)) => {
                                reconstruct_payload_column_take_from_flat(
                                    &refs,
                                    &job.local_indices,
                                    resolved_chunks,
                                    dt,
                                )?
                            }
                            _ => {
                                let blk = &meta.rg_blocks(cidx, rg_idx)[job.mb_idx];
                                if blk.encoding == Encoding::ConstNull {
                                    Column::try_new_const(
                                        ScalarValue::Null(dt.clone()),
                                        job.local_indices.len(),
                                    )?
                                } else {
                                    let raw = block_reader.read_block(blk)?;
                                    let is_sparse_payload =
                                        job.local_indices.len().saturating_mul(8)
                                            < job.mb_row_count;
                                    if is_sparse_payload {
                                        let owned_mask;
                                        let m = match job.mask_opt.as_deref() {
                                            Some(m) => m,
                                            None => {
                                                owned_mask = mask_from_indices(
                                                    job.mb_row_count,
                                                    &job.local_indices,
                                                );
                                                &owned_mask
                                            }
                                        };
                                        let selected = decode_payload_refs_selected(
                                            &raw,
                                            job.mb_row_count,
                                            m,
                                            MAX_BATCH_DECODED_BYTES,
                                        )?;
                                        reconstruct_payload_column_take_selected(
                                            job.mb_row_count,
                                            &selected,
                                            &job.local_indices,
                                            resolved_chunks,
                                            dt,
                                        )?
                                    } else {
                                        let refs = decode_payload_refs_flat(
                                            &raw,
                                            job.mb_row_count,
                                            MAX_BATCH_DECODED_BYTES,
                                        )?;
                                        reconstruct_payload_column_take_from_flat(
                                            &refs,
                                            &job.local_indices,
                                            resolved_chunks,
                                            dt,
                                        )?
                                    }
                                }
                            }
                        }
                    }
                } else if *dt == DataType::MapUtf8
                    && let Some(sm) = meta
                        .schema_meta
                        .shredded_maps
                        .iter()
                        .find(|s| s.logical_col_idx as usize == cidx)
                {
                    let mask = job.mask_opt.as_deref().ok_or(StatamError::Corrupt {
                        reason: "missing take_rows shredded map mask",
                    })?;
                    let u32_idx = job.u32_indices.as_deref().ok_or(StatamError::Corrupt {
                        reason: "missing take_rows u32 indices",
                    })?;
                    let skip_residual = plan.phase2_map_fast_path.contains(&(job.mb_idx, cidx));
                    let residual_flat: Option<Cow<'_, FlatColumn>> = if skip_residual {
                        None
                    } else if let Some(pre) = job.mb_residuals.get(&cidx) {
                        Some(Cow::Borrowed(pre))
                    } else {
                        let blk = &meta.rg_blocks(cidx, rg_idx)[job.mb_idx];
                        let raw = if blk.stored_len > 0 {
                            block_reader.read_block(blk)?
                        } else {
                            Cow::Borrowed(&[][..])
                        };
                        Some(Cow::Owned(decode_shredded_residual_block_selected(
                            &raw, blk, mask,
                        )?))
                    };
                    let full_col = build_shredded_map_col_with_meta(
                        job.mb_row_count,
                        cidx,
                        sm,
                        skip_residual,
                        residual_flat.as_deref(),
                        mask,
                        empty_map_proj,
                        &job.mb_subs,
                    )?;
                    Column::Flat(full_col.take(u32_idx, None)?)
                } else {
                    return Err(StatamError::Corrupt {
                        reason: "missing fused take projected column",
                    });
                };

                if !is_last {
                    reconstructed_cache.insert(cidx, col.clone());
                }
                out_cols.push(col);
            }

            Ok(Batch::try_new(
                Arc::clone(proj_schema),
                out_cols,
                job.local_indices.len(),
                Selection::All,
            )?)
        }

        // Step 1 (Exact two-loop S3-invariant budget check + fetch):
        //   Loop 1: validate `check_chunks_budget` across ALL non-empty packs in ascending
        //           `pack_idx` order BEFORE calling `prepare_and_fetch_frames` on any pack.
        let mut pending_ext_fetches: Vec<(u16, Arc<ChunkPackReader>, Vec<u32>)> =
            Vec::with_capacity(merged_ext_by_pack.len());
        let mut total_chunk_bytes = 0usize;
        let mut total_frame_bytes = 0usize;
        for (pidx, cids_set) in merged_ext_by_pack {
            if cids_set.is_empty() {
                continue;
            }
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
            pending_ext_fetches.push((pidx, pack, cids));
        }

        //   Loop 2: fetch frames in exact ascending `pack_idx` order.
        let mut prepared_packs: Vec<PreparedExternalPack> =
            Vec::with_capacity(pending_ext_fetches.len());
        let mut stage1_tasks: Vec<TakeStage1Item> = Vec::new();
        let mut stage1_work_bytes = 0usize;
        for (pidx, pack, cids) in pending_ext_fetches {
            let (frame_groups, frame_bytes_list, dict) = pack.prepare_and_fetch_frames(&cids)?;
            let pack_slot = prepared_packs.len();
            for ((fid, fcids), raw) in frame_groups.into_iter().zip(frame_bytes_list) {
                let entry = &pack.toc().frames[fid as usize];
                let (unweighted_bytes, work_len) =
                    pack_frame_work_bytes(entry.codec, entry.raw_len);
                stage1_work_bytes = stage1_work_bytes.saturating_add(unweighted_bytes);
                stage1_tasks.push(TakeStage1Item {
                    work_len,
                    task: TakeStage1Task::PackFrame {
                        pack_slot,
                        fid,
                        raw,
                        cids: fcids,
                    },
                });
            }
            prepared_packs.push(PreparedExternalPack {
                pack_idx: pidx,
                pack,
                dict,
            });
        }

        // Step 2: Pre-populate dicts, `local_indices`, `u32_indices`, and masks on main thread.
        let fields = self.meta.schema_meta.schema.fields();
        let pre_dicts = self.preload_proj_dicts(proj_indices);

        let mut shredded_map_cols: HashSet<usize> = HashSet::new();
        let mut shredded_map_cols_order: Vec<usize> = Vec::new();
        let mut needed_sub_pidxs: Vec<usize> = Vec::new();
        let mut non_payload_cols: Vec<usize> = Vec::new();
        let mut seen_non_payload: HashSet<usize> = HashSet::new();
        for &cidx in proj_indices {
            if self.meta.col_dir[cidx].locality_group == LocalityGroup::ExternalPayload {
                continue;
            }
            if fields[cidx].data_type == DataType::MapUtf8
                && let Some(sm) = self
                    .meta
                    .schema_meta
                    .shredded_maps
                    .iter()
                    .find(|s| s.logical_col_idx as usize == cidx)
            {
                if shredded_map_cols.insert(cidx) {
                    shredded_map_cols_order.push(cidx);
                    for ke in &sm.keys {
                        let sub_pidx = ke.sub_col_idx as usize;
                        if !needed_sub_pidxs.contains(&sub_pidx) {
                            needed_sub_pidxs.push(sub_pidx);
                        }
                    }
                }
            } else if seen_non_payload.insert(cidx) {
                non_payload_cols.push(cidx);
            }
        }
        let needs_shredded_map = !shredded_map_cols.is_empty();

        let num_mbs = mb_rows.len();
        let mut mb_jobs: Vec<FusedTakeMbJob> = Vec::with_capacity(num_mbs);
        for (mb_pos, ((mb_idx, local_indices), (_, mask, payload_p1))) in
            mb_rows.into_iter().zip(active_mbs).enumerate()
        {
            let mb_row_count = self.meta.rg_blocks(0, rg_idx)[mb_idx].row_count as usize;
            let local_indices_arc = Arc::new(local_indices);
            let u32_indices = if needs_shredded_map {
                let mut out = Vec::with_capacity(local_indices_arc.len());
                for &r in local_indices_arc.iter() {
                    out.push(u32::try_from(r).map_err(|_| StatamError::BoundsExceeded {
                        reason: "take_rows local index overflows u32",
                    })?);
                }
                Some(out)
            } else {
                None
            };
            let mask_opt = if needs_shredded_map {
                Some(Arc::new(mask))
            } else {
                None
            };

            if let Some(ref mask_arc) = mask_opt {
                for &sub_pidx in &needed_sub_pidxs {
                    let blk = self.meta.rg_blocks(sub_pidx, rg_idx)[mb_idx];
                    let work_len = blk.raw_len as usize;
                    stage1_work_bytes = stage1_work_bytes.saturating_add(work_len);
                    stage1_tasks.push(TakeStage1Item {
                        work_len,
                        task: TakeStage1Task::ShreddedSub {
                            mb_pos,
                            sub_pidx,
                            blk,
                            mb_row_count,
                            mask: Arc::clone(mask_arc),
                        },
                    });
                }
                for &cidx in &shredded_map_cols_order {
                    let skip_residual = plan.phase2_map_fast_path.contains(&(mb_idx, cidx));
                    let blk = self.meta.rg_blocks(cidx, rg_idx)[mb_idx];
                    if !skip_residual && blk.stored_len > 0 {
                        let work_len = blk.raw_len as usize;
                        stage1_work_bytes = stage1_work_bytes.saturating_add(work_len);
                        stage1_tasks.push(TakeStage1Item {
                            work_len,
                            task: TakeStage1Task::ShreddedResidual {
                                mb_pos,
                                cidx,
                                blk,
                                mask: Arc::clone(mask_arc),
                            },
                        });
                    }
                }
            }

            for &cidx in &non_payload_cols {
                let blk = self.meta.rg_blocks(cidx, rg_idx)[mb_idx];
                let dict = if blk.encoding == Encoding::Dict {
                    pre_dicts.get(&cidx).map(Arc::clone)
                } else {
                    None
                };
                let work_len = blk.raw_len as usize;
                stage1_work_bytes = stage1_work_bytes.saturating_add(work_len);
                stage1_tasks.push(TakeStage1Item {
                    work_len,
                    task: TakeStage1Task::NonPayloadCol {
                        mb_pos,
                        cidx,
                        blk,
                        dt: fields[cidx].data_type.clone(),
                        dict,
                        local_indices: Arc::clone(&local_indices_arc),
                    },
                });
            }

            mb_jobs.push(FusedTakeMbJob {
                mb_idx,
                mb_row_count,
                local_indices: local_indices_arc,
                u32_indices,
                mask_opt,
                payload_p1,
                mb_cols: HashMap::with_capacity(non_payload_cols.len()),
                mb_subs: HashMap::with_capacity(needed_sub_pidxs.len()),
                mb_residuals: HashMap::with_capacity(shredded_map_cols_order.len()),
            });
        }

        // Step 3: ONE LPT-sorted Stage-1 dispatch on the persistent `WorkerPool`.
        stage1_tasks.sort_unstable_by_key(|a| std::cmp::Reverse(a.work_len));
        let stage1_workers = if stage1_tasks.len() >= 16 && stage1_work_bytes >= 64 * 1024 {
            threads
        } else {
            effective_read_threads(stage1_work_bytes, threads)
        };
        let effective_stage1_workers = if stage1_tasks.len() >= 2 && stage1_workers > 1 {
            stage1_workers
        } else {
            1
        };
        let stage1_ctx = TakeStage1Ctx {
            prepared_packs,
            block_reader: read_block.clone(),
        };
        let stage1_outputs = parallel_map_in_order_static_with_ctx(
            stage1_ctx,
            stage1_tasks,
            effective_stage1_workers,
            |ctx, worker| {
                worker.run_with_init(
                    || Ok(PackFrameDecompressor::new()),
                    |dict_decomp, item| match item.task {
                        TakeStage1Task::PackFrame {
                            pack_slot,
                            fid,
                            raw,
                            cids,
                        } => {
                            let prep = &ctx.prepared_packs[pack_slot];
                            let pairs = prep.pack.decompress_frame_chunks_as_bytes(
                                fid,
                                &raw,
                                &cids,
                                prep.dict.as_deref(),
                                dict_decomp,
                            )?;
                            Ok(TakeStage1Out::PackChunks(prep.pack_idx, pairs))
                        }
                        TakeStage1Task::ShreddedSub {
                            mb_pos,
                            sub_pidx,
                            blk,
                            mb_row_count,
                            mask,
                        } => {
                            let raw = ctx.block_reader.read_block(&blk)?;
                            let sub = decode_shredded_subcol_selected(&raw, mb_row_count, &mask)?;
                            Ok(TakeStage1Out::ShreddedSub(
                                mb_pos,
                                sub_pidx,
                                DecodedSubCol::Full(sub),
                            ))
                        }
                        TakeStage1Task::ShreddedResidual {
                            mb_pos,
                            cidx,
                            blk,
                            mask,
                        } => {
                            let raw = ctx.block_reader.read_block(&blk)?;
                            let res = decode_shredded_residual_block_selected(&raw, &blk, &mask)?;
                            Ok(TakeStage1Out::ShreddedResidual(mb_pos, cidx, res))
                        }
                        TakeStage1Task::NonPayloadCol {
                            mb_pos,
                            cidx,
                            blk,
                            dt,
                            dict,
                            local_indices,
                        } => {
                            let col = if local_indices.is_empty() {
                                Column::Flat(FlatColumn::empty(&dt))
                            } else {
                                let raw = if blk.stored_len > 0 {
                                    ctx.block_reader.read_block(&blk)?
                                } else {
                                    Cow::Borrowed(&[][..])
                                };
                                decode_mini_block_take(
                                    &raw,
                                    &blk,
                                    &dt,
                                    dict.as_deref(),
                                    &local_indices,
                                )?
                            };
                            Ok(TakeStage1Out::NonPayloadCol(mb_pos, cidx, col))
                        }
                    },
                )
            },
        )?;

        let mut chunk_groups: Vec<(u16, Vec<(u32, Bytes)>)> = Vec::new();
        for out in stage1_outputs {
            match out {
                TakeStage1Out::PackChunks(pidx, pairs) => chunk_groups.push((pidx, pairs)),
                TakeStage1Out::ShreddedSub(mb_pos, sub_pidx, sub) => {
                    mb_jobs[mb_pos].mb_subs.insert(sub_pidx, sub);
                }
                TakeStage1Out::ShreddedResidual(mb_pos, cidx, res) => {
                    mb_jobs[mb_pos].mb_residuals.insert(cidx, res);
                }
                TakeStage1Out::NonPayloadCol(mb_pos, cidx, col) => {
                    mb_jobs[mb_pos].mb_cols.insert(cidx, col);
                }
            }
        }
        let resolved_chunks = ResolvedChunks::from_pack_entries(chunk_groups);

        // Step 4: Per-MB batch assembly (`ExternalPayload` + `ShreddedMap` gather + non-payload).
        let mut step4_bytes = resolved_chunks.total_bytes();
        if needs_shredded_map {
            for job in &mb_jobs {
                for &cidx in &shredded_map_cols {
                    step4_bytes = step4_bytes.saturating_add(
                        self.meta.rg_blocks(cidx, rg_idx)[job.mb_idx].raw_len as usize,
                    );
                }
            }
        }

        let step4_min_bytes = if needs_shredded_map {
            64 * 1024
        } else {
            256 * 1024
        };
        if mb_jobs.len() >= 2 && step4_bytes >= step4_min_bytes {
            let step4_workers = threads.min(mb_jobs.len());
            let meta_arc = Arc::clone(&self.meta);
            let plan_arc = Arc::clone(plan);
            let proj_indices_arc = Arc::new(proj_indices.to_vec());
            let proj_schema_arc = Arc::clone(proj_schema);
            let empty_map_proj_arc = Arc::clone(empty_map_proj);
            let resolved_arc = resolved_chunks;
            let block_reader_arc = read_block.clone();
            parallel_map_in_order_static(mb_jobs, step4_workers, move |job| {
                assemble_fused_take_mb(
                    &meta_arc,
                    rg_idx,
                    &plan_arc,
                    &proj_indices_arc,
                    &proj_schema_arc,
                    &empty_map_proj_arc,
                    &resolved_arc,
                    &block_reader_arc,
                    job,
                )
            })
        } else {
            let mut batches = Vec::with_capacity(mb_jobs.len());
            for job in mb_jobs {
                batches.push(assemble_fused_take_mb(
                    &self.meta,
                    rg_idx,
                    plan,
                    proj_indices,
                    proj_schema,
                    empty_map_proj,
                    &resolved_chunks,
                    read_block,
                    job,
                )?);
            }
            Ok(batches)
        }
    }

    pub(super) fn resolve_rg_payload_chunks_from_by_pack(
        &self,
        by_pack: Vec<(u16, Vec<u32>)>,
        l0_local: Option<&ParsedL0Payload>,
        pre_resolved: &HashMap<([u8; 16], u32), Bytes>,
        read_threads: usize,
    ) -> Result<ResolvedChunks, StatamError> {
        if by_pack.is_empty() {
            return Ok(ResolvedChunks::empty());
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
        for (pidx, cids) in by_pack {
            if cids.is_empty() {
                continue;
            }
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

    #[allow(clippy::too_many_arguments)]
    pub(super) fn resolve_rg_payload_chunks(
        &self,
        rg_idx: usize,
        active_mbs: &mut [(usize, Bitmap, HashMap<usize, DecodedPhase1>)],
        proj_indices: &[usize],
        l0_local: Option<&ParsedL0Payload>,
        pre_resolved: &HashMap<([u8; 16], u32), Bytes>,
        read_block: &BlockCacheReader,
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

        self.ensure_rg_payload_refs_decoded(
            rg_idx,
            active_mbs,
            &proj_payload_cols,
            read_block,
            read_threads,
        )?;

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

        let by_pack_vec: Vec<(u16, Vec<u32>)> = by_pack
            .into_iter()
            .map(|(pidx, cids_set)| (pidx, cids_set.into_iter().collect()))
            .collect();
        self.resolve_rg_payload_chunks_from_by_pack(
            by_pack_vec,
            l0_local,
            pre_resolved,
            read_threads,
        )
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
        l0_local: Option<&ParsedL0Payload>,
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
    pub(super) fn eval_payload_refs_substring_batch(
        &self,
        mb_inputs: &[(&FlatPayloadRefs, &Bitmap)],
        needle: &[u8],
        case_insensitive: bool,
        l0_local: Option<&ParsedL0Payload>,
        read_threads: usize,
        retain: Option<&RetainedPackChunks>,
        cancel: Option<&CancelToken>,
    ) -> Result<Vec<Bitmap>, StatamError> {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Err(StatamError::Cancelled);
        }
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
            if cancel.is_some_and(CancelToken::is_cancelled) {
                return Err(StatamError::Cancelled);
            }
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
                cancel,
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
                    if cancel.is_some_and(CancelToken::is_cancelled) {
                        return Err(StatamError::Cancelled);
                    }
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
                        if cancel.is_some_and(CancelToken::is_cancelled) {
                            return Err(StatamError::Cancelled);
                        }
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

pub(super) fn clone_decoded_p1_for_wave(
    src: &HashMap<usize, DecodedPhase1>,
) -> HashMap<usize, DecodedPhase1> {
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

    let mut data_bytes = Vec::with_capacity(total_decoded);
    for r in 0..row_count {
        if !active_mask.get(r) {
            continue;
        }
        match refs.row_kind(r) {
            FLAT_REF_INLINE => data_bytes.extend_from_slice(refs.inline_bytes(r)),
            FLAT_REF_CHUNKS => {
                let (ps, cs) = refs.chunk_refs(r);
                for (&pidx, &cid) in ps.iter().zip(cs.iter()) {
                    let chunk = resolved_chunks.get(pidx, cid).ok_or(StatamError::Corrupt {
                        reason: "missing resolved payload chunk during column reconstruction",
                    })?;
                    data_bytes.extend_from_slice(chunk);
                }
            }
            _ => {}
        }
    }

    finish_payload_column(dt, offsets, data_bytes, nulls)
}

pub(super) fn reconstruct_payload_column_selected(
    row_count: usize,
    selected: &[(usize, PayloadRefRow)],
    active_mask: &Bitmap,
    resolved_chunks: &ResolvedChunks,
    dt: &DataType,
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

    let mut data_bytes = Vec::with_capacity(total_decoded);
    let mut s_idx = 0usize;
    let mut prev_r: Option<usize> = None;
    while s_idx < selected.len() && selected[s_idx].0 < row_count {
        let (r, ref row) = selected[s_idx];
        s_idx += 1;
        if prev_r == Some(r) || !active_mask.get(r) {
            continue;
        }
        prev_r = Some(r);
        extend_payload_row_into_vec(row, resolved_chunks, &mut data_bytes)?;
    }

    finish_payload_column(dt, offsets, data_bytes, nulls)
}

pub(super) fn reconstruct_payload_column_take_from_flat(
    refs: &FlatPayloadRefs,
    local_indices: &[usize],
    resolved_chunks: &ResolvedChunks,
    dt: &DataType,
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

    let mut data_bytes = Vec::with_capacity(total_decoded);
    for &r in local_indices {
        match refs.row_kind(r) {
            FLAT_REF_INLINE => data_bytes.extend_from_slice(refs.inline_bytes(r)),
            FLAT_REF_CHUNKS => {
                let (ps, cs) = refs.chunk_refs(r);
                for (&pidx, &cid) in ps.iter().zip(cs.iter()) {
                    let chunk = resolved_chunks.get(pidx, cid).ok_or(StatamError::Corrupt {
                        reason: "missing resolved payload chunk during column reconstruction",
                    })?;
                    data_bytes.extend_from_slice(chunk);
                }
            }
            _ => {}
        }
    }

    finish_payload_column(dt, offsets, data_bytes, nulls)
}

pub(super) fn reconstruct_payload_column_take_selected(
    mb_row_count: usize,
    selected: &[(usize, PayloadRefRow)],
    local_indices: &[usize],
    resolved_chunks: &ResolvedChunks,
    dt: &DataType,
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

    let mut data_bytes = Vec::with_capacity(total_decoded);
    let mut s_idx = 0usize;
    let mut p_r = 0usize;
    for &r in local_indices {
        if let Some(row) = lookup_selected(r, &mut s_idx, &mut p_r) {
            extend_payload_row_into_vec(row, resolved_chunks, &mut data_bytes)?;
        }
    }

    finish_payload_column(dt, offsets, data_bytes, nulls)
}

pub(super) struct ParsedL0Payload {
    buf: Bytes,
    frames: Vec<(u64, u32, u32)>,
    chunks: Vec<ChunkLoc>,
}

impl ParsedL0Payload {
    pub(super) fn parse(buf: Bytes) -> Result<Self, StatamError> {
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
        let parsed = ParsedL0Payload::parse(buf)?;
        Ok(Self {
            buf: parsed.buf,
            frames: parsed.frames,
            chunks: parsed.chunks,
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
