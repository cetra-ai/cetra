//! Local L0 and external `.stpk` payload parsing, chunk resolution, substring scan, and reconstruction.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use bytes::Bytes;
use cetra_column::{Bitmap, Column, DataType, FlatColumn, FlatData, ScalarValue, Utf8Data};

use crate::StatamError;
use crate::format::{
    ChunkLoc, Encoding, LocalityGroup, MAX_BATCH_DECODED_BYTES, MAX_FRAME_RAW_BYTES,
    MAX_ROW_PAYLOAD_BYTES, read_framed_pack_frame,
};
use crate::parallel::MAX_PARALLEL_THREADS;
use crate::payload::pack::ChunkPackReader;
use crate::payload::search::{pack_short_circuits_all, search_pack_rows_flat_masked_with_threads};
use crate::payload::{
    FLAT_REF_CHUNKS, FLAT_REF_INLINE, FLAT_REF_NULL, FlatPayloadRefs, PayloadRefRow,
    decode_payload_refs_flat, decode_payload_refs_selected,
};
use crate::writer::{L0_LOCAL_PACK_IDX, L0_PAYLOAD_DIR_MAGIC};

use super::StatamReader;
use super::eval::Utf8Matcher;
use super::plan::{BlockReader, DecodedPhase1};

impl StatamReader {
    pub(super) fn resolve_rg_payload_chunks<'a>(
        &self,
        rg_idx: usize,
        active_mbs: &mut [(usize, Bitmap, HashMap<usize, DecodedPhase1>)],
        proj_indices: &[usize],
        l0_local: Option<&ParsedL0Payload<'_>>,
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

        let mut by_pack: HashMap<u16, BTreeSet<u32>> = HashMap::new();
        for (mb_idx, mask, decoded_p1) in active_mbs.iter_mut() {
            let mb_row_count = self.meta.rg_blocks(0, rg_idx)[*mb_idx].row_count as usize;
            let is_sparse = !mask.all_ones() && mask.count_ones().saturating_mul(8) < mb_row_count;
            for &cidx in &proj_payload_cols {
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
                        for (_, row) in selected {
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
                work_items.push(PackWork::External(pidx, pack, cids));
            }
        }

        let threads = read_threads.clamp(1, MAX_PARALLEL_THREADS);
        let mut resolved_chunks: HashMap<(u16, u32), Bytes> = HashMap::new();
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

    pub(super) fn eval_payload_refs_substring(
        &self,
        refs: &FlatPayloadRefs,
        active_mask: &Bitmap,
        needle: &[u8],
        case_insensitive: bool,
        l0_local: Option<&ParsedL0Payload<'_>>,
        read_threads: usize,
    ) -> Result<Bitmap, StatamError> {
        let mut results = Bitmap::new_zeroed(refs.len());
        if active_mask.all_zeros() {
            return Ok(results);
        }
        if needle.is_empty() {
            for i in active_mask.iter_ones() {
                if refs.row_kind(i) != FLAT_REF_NULL {
                    let _ = results.set(i, true);
                }
            }
            return Ok(results);
        }

        // Handle active inline rows + track external pack_idxs + L0 local chunks on active rows only.
        let mut matcher = Utf8Matcher::new(needle, case_insensitive);
        let mut external_pack_idxs = BTreeSet::new();
        let mut has_l0_chunks = false;
        let mut active_chunk_rows = Vec::new();

        for i in active_mask.iter_ones() {
            match refs.row_kind(i) {
                FLAT_REF_NULL => {}
                FLAT_REF_INLINE => {
                    if matcher.matches(refs.inline_bytes(i)) {
                        let _ = results.set(i, true);
                    }
                }
                FLAT_REF_CHUNKS => {
                    active_chunk_rows.push(i);
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
                for &r in &active_chunk_rows {
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
                continue;
            }

            let mut pack_mask = Bitmap::new_zeroed(refs.len());
            let mut any_pack_row = false;
            for &r in &active_chunk_rows {
                if results.get(r) {
                    continue;
                }
                let (ps, _) = refs.chunk_refs(r);
                if ps.contains(&pack_idx) {
                    let _ = pack_mask.set(r, true);
                    any_pack_row = true;
                }
            }
            if !any_pack_row {
                continue;
            }
            let bm = search_pack_rows_flat_masked_with_threads(
                &pack,
                refs,
                &pack_mask,
                pack_idx,
                needle,
                case_insensitive,
                read_threads,
            )?;
            for bit in bm.iter_ones() {
                let _ = results.set(bit, true);
            }
        }

        if has_l0_chunks {
            let l0 = l0_local.ok_or(StatamError::Corrupt {
                reason: "missing L0 local payload region for chunked L0 row",
            })?;
            let mut single_by_frame: BTreeMap<u32, Vec<(u32, usize)>> = BTreeMap::new();
            let mut multi_chunk_rows: Vec<usize> = Vec::new();

            for &r in &active_chunk_rows {
                if results.get(r) {
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
                            let _ = results.set(cid_rows[idx].1, true);
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
                        let _ = results.set(r, true);
                    }
                }
            }
        }

        Ok(results)
    }
}

fn append_payload_row_to_utf8(
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
                    reason: "utf8 payload column exceeds u32::MAX",
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
                    reason: "utf8 payload column exceeds u32::MAX",
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
) -> Result<Column, StatamError> {
    let row_count = refs.len();
    let all_active_null = active_mask
        .iter_ones()
        .all(|r| refs.row_kind(r) == FLAT_REF_NULL);
    if all_active_null {
        return Ok(Column::try_new_const(
            ScalarValue::Null(DataType::Utf8),
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
                        reason: "utf8 payload column exceeds u32::MAX",
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
                        reason: "utf8 payload column exceeds u32::MAX",
                    }
                })?;
                offsets.push(next_off);
            }
            _ => {
                offsets.push(data_bytes.len() as u32);
            }
        }
    }

    let utf8 = Utf8Data::try_new(offsets, data_bytes)?;
    let nulls_opt = if nulls.all_ones() { None } else { Some(nulls) };
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(utf8),
        nulls_opt,
    )?))
}

pub(super) fn reconstruct_payload_column_selected(
    row_count: usize,
    selected: &[(usize, PayloadRefRow)],
    active_mask: &Bitmap,
    resolved_chunks: &HashMap<(u16, u32), Bytes>,
) -> Result<Column, StatamError> {
    let all_active_null = selected
        .iter()
        .all(|(r, row)| !active_mask.get(*r) || matches!(row, PayloadRefRow::Null));
    if all_active_null {
        return Ok(Column::try_new_const(
            ScalarValue::Null(DataType::Utf8),
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
            append_payload_row_to_utf8(
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

    let utf8 = Utf8Data::try_new(offsets, data_bytes)?;
    let nulls_opt = if nulls.all_ones() { None } else { Some(nulls) };
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(utf8),
        nulls_opt,
    )?))
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

    fn check_chunks_budget(
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
