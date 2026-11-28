//! Wide payload chunking, `.stpk` chunk packs, tail-sharing `PayloadRefs`, and exact substring search.

pub mod chunker;
pub mod pack;
pub mod search;

use std::collections::VecDeque;
use std::fmt;

use cetra_column::Bitmap;

use crate::StatamError;
use crate::format::{
    MAX_BATCH_DECODED_BYTES, MAX_BLOCK_RAW_BYTES, MAX_CHUNKS_PER_ROW, MAX_REF_CHAIN_DEPTH,
    MAX_ROW_PAYLOAD_BYTES, MAX_ROWS_PER_FILE,
};
use crate::simd::bitpack::{zigzag_decode_i64, zigzag_encode_i64};

/// Maximum lookback distance (`1..=32`) allowed by the wire decoder for `PayloadRefs` delta rows.
const MAX_TAIL_SHARE_BACK: usize = 32;
/// Encoder lookback window (`1..=8`) for fast `PayloadRefs` tail-sharing.
const ENCODE_TAIL_SHARE_BACK: usize = 8;
/// Tag for inline payload bytes (`<= 256 B`, or up to `u16::MAX`).
const TAG_INLINE: u8 = 0;
/// Tag for keyframe chunk reference list.
const TAG_KEYFRAME: u8 = 1;
/// Tag for delta tail-sharing chunk reference list.
const TAG_DELTA: u8 = 2;

type RefHistoryEntry = Option<(Vec<(u16, u32)>, u8)>;
type RefHistory = VecDeque<RefHistoryEntry>;

/// One logical row in a `PayloadRefs` column block.
#[derive(Clone, PartialEq, Eq)]
pub enum PayloadRefRow {
    /// SQL NULL payload.
    Null,
    /// Short payload stored inline inside the `.statam` block (`<= 256 B`).
    Inline(Vec<u8>),
    /// Sequence of `(pack_idx, chunk_id)` references into `.stpk` packs (or `0xFFFF` for L0 local).
    Chunks(Vec<(u16, u32)>),
}

impl fmt::Debug for PayloadRefRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("Null"),
            Self::Inline(bytes) => f.debug_struct("Inline").field("len", &bytes.len()).finish(),
            Self::Chunks(chunks) => f
                .debug_struct("Chunks")
                .field("count", &chunks.len())
                .finish(),
        }
    }
}

/// Encodes `rows` using null-bitmap prefix + tail-sharing keyframe/delta compression.
pub fn encode_payload_refs(rows: &[PayloadRefRow], out: &mut Vec<u8>) -> Result<(), StatamError> {
    if rows.len() as u64 > MAX_ROWS_PER_FILE {
        return Err(StatamError::BoundsExceeded {
            reason: "payload refs row count exceeds MAX_ROWS_PER_FILE",
        });
    }

    let has_nulls = rows.iter().any(|r| matches!(r, PayloadRefRow::Null));
    out.push(u8::from(has_nulls));
    if has_nulls {
        let mut validity = Bitmap::new_zeroed(rows.len());
        for (i, row) in rows.iter().enumerate() {
            if !matches!(row, PayloadRefRow::Null) {
                let _ = validity.set(i, true);
            }
        }
        out.extend_from_slice(&validity.to_bytes());
    }

    let mut history: RefHistory = VecDeque::with_capacity(ENCODE_TAIL_SHARE_BACK);
    let mut scratch_delta = Vec::new();

    for row in rows {
        match row {
            PayloadRefRow::Null => {}
            PayloadRefRow::Inline(bytes) => {
                let len = u16::try_from(bytes.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "inline payload length exceeds u16::MAX",
                })?;
                out.push(TAG_INLINE);
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(bytes);
                push_history_capped(&mut history, None, ENCODE_TAIL_SHARE_BACK);
            }
            PayloadRefRow::Chunks(chunks) => {
                if chunks.len() > MAX_CHUNKS_PER_ROW {
                    return Err(StatamError::BoundsExceeded {
                        reason: "row chunks exceed MAX_CHUNKS_PER_ROW",
                    });
                }

                // Search lookback window (1..=8) for longest shared prefix with chain depth < 15.
                let mut best: Option<(usize, usize, u8)> = None;
                let max_back = history.len().min(ENCODE_TAIL_SHARE_BACK);
                let cur_len = chunks.len();
                for back in 1..=max_back {
                    if let Some((prev_chunks, prev_depth)) = &history[history.len() - back] {
                        if (*prev_depth as usize) >= MAX_REF_CHAIN_DEPTH {
                            continue;
                        }
                        let lcp = common_prefix_len(prev_chunks, chunks).min(u16::MAX as usize);
                        let n_new = cur_len - lcp;
                        if lcp > 0
                            && n_new <= u16::MAX as usize
                            && best.is_none_or(|(_, best_lcp, _)| lcp > best_lcp)
                        {
                            best = Some((back, lcp, *prev_depth + 1));
                            if lcp == cur_len.min(prev_chunks.len()) {
                                break;
                            }
                        }
                    }
                }

                let keyframe_bytes = 1usize + 2 + chunks.len() * 6;
                let mut wrote_delta = false;

                if let Some((back, keep_prefix, new_depth)) = best {
                    scratch_delta.clear();
                    let mut prev = if keep_prefix > 0 {
                        chunks[keep_prefix - 1]
                    } else {
                        (0u16, 0u32)
                    };
                    for &(pack_idx, chunk_id) in &chunks[keep_prefix..] {
                        let p_delta = i64::from(pack_idx) - i64::from(prev.0);
                        let c_delta = i64::from(chunk_id) - i64::from(prev.1);
                        encode_varint_u64(zigzag_encode_i64(p_delta), &mut scratch_delta);
                        encode_varint_u64(zigzag_encode_i64(c_delta), &mut scratch_delta);
                        prev = (pack_idx, chunk_id);
                    }
                    let delta_total_bytes = 1 + 1 + 2 + 2 + scratch_delta.len();
                    if delta_total_bytes < keyframe_bytes {
                        out.push(TAG_DELTA);
                        out.push(back as u8);
                        out.extend_from_slice(&(keep_prefix as u16).to_le_bytes());
                        out.extend_from_slice(&((chunks.len() - keep_prefix) as u16).to_le_bytes());
                        out.extend_from_slice(&scratch_delta);
                        push_history_capped(
                            &mut history,
                            Some((chunks.clone(), new_depth)),
                            ENCODE_TAIL_SHARE_BACK,
                        );
                        wrote_delta = true;
                    }
                }

                if !wrote_delta {
                    let n_chunks =
                        u16::try_from(chunks.len()).map_err(|_| StatamError::BoundsExceeded {
                            reason: "keyframe chunk count exceeds u16::MAX",
                        })?;
                    out.push(TAG_KEYFRAME);
                    out.extend_from_slice(&n_chunks.to_le_bytes());
                    for &(pack_idx, chunk_id) in chunks {
                        out.extend_from_slice(&pack_idx.to_le_bytes());
                        out.extend_from_slice(&chunk_id.to_le_bytes());
                    }
                    push_history_capped(
                        &mut history,
                        Some((chunks.clone(), 0)),
                        ENCODE_TAIL_SHARE_BACK,
                    );
                }
            }
        }
    }

    Ok(())
}

/// Fast `L0Fast` encoder emitting `TAG_INLINE` / `TAG_KEYFRAME` directly without tail-sharing.
pub fn encode_payload_refs_l0_direct(
    rows: &[PayloadRefRow],
    out: &mut Vec<u8>,
) -> Result<(), StatamError> {
    if rows.len() as u64 > MAX_ROWS_PER_FILE {
        return Err(StatamError::BoundsExceeded {
            reason: "payload refs row count exceeds MAX_ROWS_PER_FILE",
        });
    }

    let has_nulls = rows.iter().any(|r| matches!(r, PayloadRefRow::Null));
    out.push(u8::from(has_nulls));
    if has_nulls {
        let mut validity = Bitmap::new_zeroed(rows.len());
        for (i, row) in rows.iter().enumerate() {
            if !matches!(row, PayloadRefRow::Null) {
                let _ = validity.set(i, true);
            }
        }
        out.extend_from_slice(&validity.to_bytes());
    }

    for row in rows {
        match row {
            PayloadRefRow::Null => {}
            PayloadRefRow::Inline(bytes) => {
                let len = u16::try_from(bytes.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "inline payload length exceeds u16::MAX",
                })?;
                out.push(TAG_INLINE);
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(bytes);
            }
            PayloadRefRow::Chunks(chunks) => {
                if chunks.len() > MAX_CHUNKS_PER_ROW {
                    return Err(StatamError::BoundsExceeded {
                        reason: "row chunks exceed MAX_CHUNKS_PER_ROW",
                    });
                }
                let n_chunks =
                    u16::try_from(chunks.len()).map_err(|_| StatamError::BoundsExceeded {
                        reason: "keyframe chunk count exceeds u16::MAX",
                    })?;
                out.push(TAG_KEYFRAME);
                out.extend_from_slice(&n_chunks.to_le_bytes());
                for &(pack_idx, chunk_id) in chunks {
                    out.extend_from_slice(&pack_idx.to_le_bytes());
                    out.extend_from_slice(&chunk_id.to_le_bytes());
                }
            }
        }
    }

    Ok(())
}

/// Zero-copy `L0Fast` streaming encoder directly from `(offsets, all_bytes)` (`Utf8Data` or `BinaryData`) into `out`.
///
/// Returns the mini-block's `null_count`. When all `mb_len` rows are null, `out` is left empty.
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_payload_refs_l0_from_bytes<F>(
    offsets: &[u32],
    all_bytes: &[u8],
    col_nulls: Option<&Bitmap>,
    row_off: usize,
    mb_len: usize,
    inline_threshold: usize,
    l0_pack_idx: u16,
    mut append_row_chunks: F,
    out: &mut Vec<u8>,
) -> Result<u16, StatamError>
where
    F: FnMut(&[u8], &mut Vec<u32>) -> Result<(), StatamError>,
{
    if mb_len as u64 > MAX_ROWS_PER_FILE {
        return Err(StatamError::BoundsExceeded {
            reason: "payload refs row count exceeds MAX_ROWS_PER_FILE",
        });
    }

    let mut null_count = 0usize;
    if let Some(bm) = col_nulls {
        for r in row_off..row_off + mb_len {
            if !bm.get(r) {
                null_count += 1;
            }
        }
    }
    let null_count_u16 = u16::try_from(null_count).unwrap_or(u16::MAX);
    if null_count == mb_len {
        return Ok(null_count_u16);
    }

    let has_nulls = null_count > 0;
    out.push(u8::from(has_nulls));
    if let Some(bm) = col_nulls
        && has_nulls
    {
        let mut validity = Bitmap::new_zeroed(mb_len);
        for i in 0..mb_len {
            if bm.get(row_off + i) {
                let _ = validity.set(i, true);
            }
        }
        out.extend_from_slice(&validity.to_bytes());
    }

    let mut row_cids = Vec::new();
    for r in row_off..row_off + mb_len {
        if col_nulls.is_some_and(|bm| !bm.get(r)) {
            continue;
        }
        let s = offsets[r] as usize;
        let e = offsets[r + 1] as usize;
        let bytes = &all_bytes[s..e];
        if bytes.len() > MAX_ROW_PAYLOAD_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "row payload exceeds MAX_ROW_PAYLOAD_BYTES",
            });
        }
        if bytes.len() <= inline_threshold {
            let len = u16::try_from(bytes.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "inline payload length exceeds u16::MAX",
            })?;
            out.push(TAG_INLINE);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(bytes);
        } else {
            row_cids.clear();
            append_row_chunks(bytes, &mut row_cids)?;
            if row_cids.len() > MAX_CHUNKS_PER_ROW {
                return Err(StatamError::BoundsExceeded {
                    reason: "row chunks exceed MAX_CHUNKS_PER_ROW",
                });
            }
            let n_chunks =
                u16::try_from(row_cids.len()).map_err(|_| StatamError::BoundsExceeded {
                    reason: "keyframe chunk count exceeds u16::MAX",
                })?;
            out.push(TAG_KEYFRAME);
            out.extend_from_slice(&n_chunks.to_le_bytes());
            for &cid in &row_cids {
                out.extend_from_slice(&l0_pack_idx.to_le_bytes());
                out.extend_from_slice(&cid.to_le_bytes());
            }
        }
    }

    Ok(null_count_u16)
}

/// Decodes `row_count` [`PayloadRefRow`] entries from `src`, enforcing chain-depth,
/// per-row chunk count, and `max_batch_bytes` bounds BEFORE allocating.
pub fn decode_payload_refs(
    src: &[u8],
    row_count: usize,
    max_batch_bytes: usize,
) -> Result<Vec<PayloadRefRow>, StatamError> {
    if row_count as u64 > MAX_ROWS_PER_FILE || row_count > MAX_BLOCK_RAW_BYTES / 32 {
        return Err(StatamError::BoundsExceeded {
            reason: "payload refs row_count exceeds safe block limit",
        });
    }
    let effective_max = max_batch_bytes.min(MAX_BATCH_DECODED_BYTES);
    let row_vec_bytes =
        row_count
            .checked_mul(size_of::<PayloadRefRow>())
            .ok_or(StatamError::BoundsExceeded {
                reason: "payload refs row vector size overflow",
            })?;
    if row_vec_bytes > effective_max {
        return Err(StatamError::BoundsExceeded {
            reason: "payload refs row vector exceeds max_batch_bytes",
        });
    }

    let mut cur = RefCursor::new(src);
    let has_nulls = cur.read_u8()?;
    let validity = match has_nulls {
        0 => None,
        1 => {
            let bm_bytes = row_count.div_ceil(8);
            if bm_bytes > cur.remaining() {
                return Err(StatamError::Corrupt {
                    reason: "truncated payload refs null bitmap",
                });
            }
            let bm_slice = cur.read_exact(bm_bytes)?;
            Some(Bitmap::from_bytes(bm_slice, row_count)?)
        }
        _ => {
            return Err(StatamError::Corrupt {
                reason: "invalid payload refs has_nulls flag",
            });
        }
    };

    let non_null_count = match &validity {
        Some(bm) => bm.count_ones(),
        None => row_count,
    };
    if non_null_count > cur.remaining() {
        return Err(StatamError::Corrupt {
            reason: "truncated payload refs row stream",
        });
    }

    let mut out = Vec::with_capacity(row_count);
    let mut history: RefHistory = VecDeque::with_capacity(MAX_TAIL_SHARE_BACK);
    let mut total_decoded_bytes: usize = 0;

    for row_idx in 0..row_count {
        if validity.as_ref().is_some_and(|bm| !bm.get(row_idx)) {
            out.push(PayloadRefRow::Null);
            continue;
        }

        let tag = cur.read_u8()?;
        match tag {
            TAG_INLINE => {
                let len = cur.read_u16()? as usize;
                if len > cur.remaining() {
                    return Err(StatamError::Corrupt {
                        reason: "truncated inline payload bytes",
                    });
                }
                total_decoded_bytes = add_budget(total_decoded_bytes, len, effective_max)?;
                let bytes = cur.read_exact(len)?.to_vec();
                push_history(&mut history, None);
                out.push(PayloadRefRow::Inline(bytes));
            }
            TAG_KEYFRAME => {
                let n_chunks = cur.read_u16()? as usize;
                if n_chunks > MAX_CHUNKS_PER_ROW {
                    return Err(StatamError::BoundsExceeded {
                        reason: "keyframe chunk count exceeds MAX_CHUNKS_PER_ROW",
                    });
                }
                let wire_bytes = n_chunks.checked_mul(6).ok_or(StatamError::BoundsExceeded {
                    reason: "keyframe wire byte size overflow",
                })?;
                if wire_bytes > cur.remaining() {
                    return Err(StatamError::Corrupt {
                        reason: "truncated keyframe chunk refs",
                    });
                }
                let mem_bytes = n_chunks.checked_mul(size_of::<(u16, u32)>()).ok_or(
                    StatamError::BoundsExceeded {
                        reason: "keyframe memory size overflow",
                    },
                )?;
                total_decoded_bytes = add_budget(total_decoded_bytes, mem_bytes, effective_max)?;

                let mut refs = Vec::with_capacity(n_chunks);
                let mut first_pack: Option<u16> = None;
                for _ in 0..n_chunks {
                    let pack_idx = cur.read_u16()?;
                    let chunk_id = cur.read_u32()?;
                    if let Some(fp) = first_pack {
                        if pack_idx != fp {
                            return Err(StatamError::Corrupt {
                                reason: "mixed pack indices within a single payload row",
                            });
                        }
                    } else {
                        first_pack = Some(pack_idx);
                    }
                    refs.push((pack_idx, chunk_id));
                }
                push_history(&mut history, Some((refs.clone(), 0)));
                out.push(PayloadRefRow::Chunks(refs));
            }
            TAG_DELTA => {
                let back = cur.read_u8()? as usize;
                if back == 0 || back > MAX_TAIL_SHARE_BACK {
                    return Err(StatamError::Corrupt {
                        reason: "payload ref delta back offset outside 1..=32",
                    });
                }
                if back > history.len() {
                    return Err(StatamError::Corrupt {
                        reason: "payload ref delta back offset precedes block start",
                    });
                }
                let (parent_chunks, parent_depth) = match &history[history.len() - back] {
                    Some(pair) => pair,
                    None => {
                        return Err(StatamError::Corrupt {
                            reason: "payload ref delta references inline row",
                        });
                    }
                };
                if (*parent_depth as usize) >= MAX_REF_CHAIN_DEPTH {
                    return Err(StatamError::BoundsExceeded {
                        reason: "payload ref chain depth exceeds MAX_REF_CHAIN_DEPTH",
                    });
                }

                let keep_prefix = cur.read_u16()? as usize;
                if keep_prefix > parent_chunks.len() {
                    return Err(StatamError::Corrupt {
                        reason: "payload ref keep_prefix exceeds parent chunk count",
                    });
                }
                let n_new = cur.read_u16()? as usize;
                let total_chunks =
                    keep_prefix
                        .checked_add(n_new)
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "payload ref total chunk count overflow",
                        })?;
                if total_chunks > MAX_CHUNKS_PER_ROW {
                    return Err(StatamError::BoundsExceeded {
                        reason: "row chunks exceed MAX_CHUNKS_PER_ROW",
                    });
                }
                let min_wire = n_new.checked_mul(2).ok_or(StatamError::BoundsExceeded {
                    reason: "payload ref delta wire size overflow",
                })?;
                if min_wire > cur.remaining() {
                    return Err(StatamError::Corrupt {
                        reason: "truncated payload ref delta entries",
                    });
                }
                let mem_bytes = total_chunks.checked_mul(size_of::<(u16, u32)>()).ok_or(
                    StatamError::BoundsExceeded {
                        reason: "payload ref delta memory size overflow",
                    },
                )?;
                total_decoded_bytes = add_budget(total_decoded_bytes, mem_bytes, effective_max)?;

                let mut refs = Vec::with_capacity(total_chunks);
                refs.extend_from_slice(&parent_chunks[..keep_prefix]);

                let mut prev = if keep_prefix > 0 {
                    parent_chunks[keep_prefix - 1]
                } else {
                    (0u16, 0u32)
                };
                let mut row_pack = (keep_prefix > 0).then_some(prev.0);
                for _ in 0..n_new {
                    let p_delta = zigzag_decode_i64(cur.read_varint_u64()?);
                    let c_delta = zigzag_decode_i64(cur.read_varint_u64()?);
                    let next_pack = i64::from(prev.0)
                        .checked_add(p_delta)
                        .and_then(|v| u16::try_from(v).ok())
                        .ok_or(StatamError::Corrupt {
                            reason: "payload ref pack_idx delta out of u16 range",
                        })?;
                    if let Some(rp) = row_pack {
                        if next_pack != rp {
                            return Err(StatamError::Corrupt {
                                reason: "mixed pack indices within a single payload row",
                            });
                        }
                    } else {
                        row_pack = Some(next_pack);
                    }
                    let next_chunk = i64::from(prev.1)
                        .checked_add(c_delta)
                        .and_then(|v| u32::try_from(v).ok())
                        .ok_or(StatamError::Corrupt {
                            reason: "payload ref chunk_id delta out of u32 range",
                        })?;
                    prev = (next_pack, next_chunk);
                    refs.push(prev);
                }

                let new_depth = *parent_depth + 1;
                push_history(&mut history, Some((refs.clone(), new_depth)));
                out.push(PayloadRefRow::Chunks(refs));
            }
            _ => {
                return Err(StatamError::Corrupt {
                    reason: "unknown payload ref row tag",
                });
            }
        }
    }

    if !cur.is_eof() {
        return Err(StatamError::Corrupt {
            reason: "trailing bytes after payload refs block",
        });
    }

    Ok(out)
}

/// Row kind tag in [`FlatPayloadRefs`] representing SQL NULL.
pub const FLAT_REF_NULL: u8 = 0;
/// Row kind tag in [`FlatPayloadRefs`] representing an inline payload slice.
pub const FLAT_REF_INLINE: u8 = 1;
/// Row kind tag in [`FlatPayloadRefs`] representing `.stpk`/L0 chunk references.
pub const FLAT_REF_CHUNKS: u8 = 2;

/// Zero-per-row-allocation CSR representation of a decoded `PayloadRefs` mini-block.
#[derive(Clone, PartialEq, Eq)]
pub struct FlatPayloadRefs {
    row_kinds: Vec<u8>,
    chunk_offsets: Vec<u32>,
    inline_offsets: Vec<u32>,
    row_pack_idxs: Vec<u16>,
    chunk_ids: Vec<u32>,
    inline_data: Vec<u8>,
}

impl fmt::Debug for FlatPayloadRefs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlatPayloadRefs")
            .field("row_count", &self.row_kinds.len())
            .field("total_chunks", &self.chunk_ids.len())
            .field("inline_bytes", &self.inline_data.len())
            .finish()
    }
}

impl FlatPayloadRefs {
    /// Constructs an all-NULL `FlatPayloadRefs` of `row_count` rows.
    pub fn all_null(row_count: usize) -> Self {
        Self {
            row_kinds: vec![FLAT_REF_NULL; row_count],
            chunk_offsets: vec![0u32; row_count + 1],
            inline_offsets: vec![0u32; row_count + 1],
            row_pack_idxs: Vec::new(),
            chunk_ids: Vec::new(),
            inline_data: Vec::new(),
        }
    }

    /// Number of logical rows.
    #[inline]
    pub fn len(&self) -> usize {
        self.row_kinds.len()
    }

    /// True when `len() == 0`.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.row_kinds.is_empty()
    }

    /// Row kind tag (`FLAT_REF_NULL`, `FLAT_REF_INLINE`, or `FLAT_REF_CHUNKS`).
    #[inline]
    pub fn row_kind(&self, row: usize) -> u8 {
        self.row_kinds[row]
    }

    /// Inline payload slice for `row` (empty if `row` is Null or Chunks).
    #[inline]
    pub fn inline_bytes(&self, row: usize) -> &[u8] {
        let start = self.inline_offsets[row] as usize;
        let end = self.inline_offsets[row + 1] as usize;
        &self.inline_data[start..end]
    }

    /// `(pack_idxs, chunk_ids)` slices for `row` (empty slices if `row` is Null or Inline).
    #[inline]
    pub fn chunk_refs(&self, row: usize) -> (&[u16], &[u32]) {
        let start = self.chunk_offsets[row] as usize;
        let end = self.chunk_offsets[row + 1] as usize;
        (&self.row_pack_idxs[start..end], &self.chunk_ids[start..end])
    }

    /// Converts to `Vec<PayloadRefRow>` (for test parity verification).
    pub fn to_rows(&self) -> Vec<PayloadRefRow> {
        let mut out = Vec::with_capacity(self.row_kinds.len());
        for r in 0..self.row_kinds.len() {
            match self.row_kinds[r] {
                FLAT_REF_NULL => out.push(PayloadRefRow::Null),
                FLAT_REF_INLINE => out.push(PayloadRefRow::Inline(self.inline_bytes(r).to_vec())),
                FLAT_REF_CHUNKS => {
                    let (ps, cs) = self.chunk_refs(r);
                    let refs = ps.iter().copied().zip(cs.iter().copied()).collect();
                    out.push(PayloadRefRow::Chunks(refs));
                }
                _ => out.push(PayloadRefRow::Null),
            }
        }
        out
    }
}

struct FlatRefHistory {
    entries: [Option<(u32, u32, u8)>; MAX_TAIL_SHARE_BACK],
    len: usize,
    write_idx: usize,
}

impl FlatRefHistory {
    fn new() -> Self {
        Self {
            entries: [None; MAX_TAIL_SHARE_BACK],
            len: 0,
            write_idx: 0,
        }
    }

    #[inline]
    fn push(&mut self, entry: Option<(u32, u32, u8)>) {
        self.entries[self.write_idx] = entry;
        self.write_idx = (self.write_idx + 1) % MAX_TAIL_SHARE_BACK;
        if self.len < MAX_TAIL_SHARE_BACK {
            self.len += 1;
        }
    }

    #[inline]
    fn get_back(&self, back: usize) -> Option<(u32, u32, u8)> {
        let idx = (self.write_idx + MAX_TAIL_SHARE_BACK - back) % MAX_TAIL_SHARE_BACK;
        self.entries[idx]
    }
}

/// Decodes `row_count` payload reference entries from `raw` into flat CSR arrays with
/// **zero per-row heap allocations**, enforcing the exact same bounds and delta-chain
/// invariants as [`decode_payload_refs`].
pub fn decode_payload_refs_flat(
    raw: &[u8],
    row_count: usize,
    max_decoded_bytes: usize,
) -> Result<FlatPayloadRefs, StatamError> {
    if row_count as u64 > MAX_ROWS_PER_FILE || row_count > MAX_BLOCK_RAW_BYTES / 32 {
        return Err(StatamError::BoundsExceeded {
            reason: "payload refs row_count exceeds safe block limit",
        });
    }
    let effective_max = max_decoded_bytes.min(MAX_BATCH_DECODED_BYTES);
    let row_vec_bytes =
        row_count
            .checked_mul(size_of::<PayloadRefRow>())
            .ok_or(StatamError::BoundsExceeded {
                reason: "payload refs row vector size overflow",
            })?;
    if row_vec_bytes > effective_max {
        return Err(StatamError::BoundsExceeded {
            reason: "payload refs row vector exceeds max_batch_bytes",
        });
    }

    let mut cur = RefCursor::new(raw);
    let has_nulls = cur.read_u8()?;
    let validity = match has_nulls {
        0 => None,
        1 => {
            let bm_bytes = row_count.div_ceil(8);
            if bm_bytes > cur.remaining() {
                return Err(StatamError::Corrupt {
                    reason: "truncated payload refs null bitmap",
                });
            }
            let bm_slice = cur.read_exact(bm_bytes)?;
            Some(Bitmap::from_bytes(bm_slice, row_count)?)
        }
        _ => {
            return Err(StatamError::Corrupt {
                reason: "invalid payload refs has_nulls flag",
            });
        }
    };

    let non_null_count = match &validity {
        Some(bm) => bm.count_ones(),
        None => row_count,
    };
    if non_null_count > cur.remaining() {
        return Err(StatamError::Corrupt {
            reason: "truncated payload refs row stream",
        });
    }

    let mut row_kinds = Vec::with_capacity(row_count);
    let mut chunk_offsets = Vec::with_capacity(row_count + 1);
    chunk_offsets.push(0u32);
    let mut inline_offsets = Vec::with_capacity(row_count + 1);
    inline_offsets.push(0u32);
    let mut row_pack_idxs: Vec<u16> = Vec::new();
    let mut chunk_ids: Vec<u32> = Vec::new();
    let mut inline_data: Vec<u8> = Vec::new();

    let mut history = FlatRefHistory::new();
    let mut total_decoded_bytes: usize = 0;

    for row_idx in 0..row_count {
        if validity.as_ref().is_some_and(|bm| !bm.get(row_idx)) {
            row_kinds.push(FLAT_REF_NULL);
            chunk_offsets.push(chunk_ids.len() as u32);
            inline_offsets.push(inline_data.len() as u32);
            continue;
        }

        let tag = cur.read_u8()?;
        match tag {
            TAG_INLINE => {
                let len = cur.read_u16()? as usize;
                if len > cur.remaining() {
                    return Err(StatamError::Corrupt {
                        reason: "truncated inline payload bytes",
                    });
                }
                total_decoded_bytes = add_budget(total_decoded_bytes, len, effective_max)?;
                let bytes = cur.read_exact(len)?;
                inline_data.extend_from_slice(bytes);
                let inline_end =
                    u32::try_from(inline_data.len()).map_err(|_| StatamError::BoundsExceeded {
                        reason: "flat inline_data exceeds u32::MAX",
                    })?;
                history.push(None);
                row_kinds.push(FLAT_REF_INLINE);
                chunk_offsets.push(chunk_ids.len() as u32);
                inline_offsets.push(inline_end);
            }
            TAG_KEYFRAME => {
                let n_chunks = cur.read_u16()? as usize;
                if n_chunks > MAX_CHUNKS_PER_ROW {
                    return Err(StatamError::BoundsExceeded {
                        reason: "keyframe chunk count exceeds MAX_CHUNKS_PER_ROW",
                    });
                }
                let wire_bytes = n_chunks.checked_mul(6).ok_or(StatamError::BoundsExceeded {
                    reason: "keyframe wire byte size overflow",
                })?;
                if wire_bytes > cur.remaining() {
                    return Err(StatamError::Corrupt {
                        reason: "truncated keyframe chunk refs",
                    });
                }
                let mem_bytes = n_chunks.checked_mul(size_of::<(u16, u32)>()).ok_or(
                    StatamError::BoundsExceeded {
                        reason: "keyframe memory size overflow",
                    },
                )?;
                total_decoded_bytes = add_budget(total_decoded_bytes, mem_bytes, effective_max)?;

                let start_idx =
                    u32::try_from(chunk_ids.len()).map_err(|_| StatamError::BoundsExceeded {
                        reason: "flat chunk_ids exceeds u32::MAX",
                    })?;
                row_pack_idxs.reserve(n_chunks);
                chunk_ids.reserve(n_chunks);
                let mut first_pack: Option<u16> = None;
                for _ in 0..n_chunks {
                    let pack_idx = cur.read_u16()?;
                    let chunk_id = cur.read_u32()?;
                    if let Some(fp) = first_pack {
                        if pack_idx != fp {
                            return Err(StatamError::Corrupt {
                                reason: "mixed pack indices within a single payload row",
                            });
                        }
                    } else {
                        first_pack = Some(pack_idx);
                    }
                    row_pack_idxs.push(pack_idx);
                    chunk_ids.push(chunk_id);
                }
                let end_idx =
                    u32::try_from(chunk_ids.len()).map_err(|_| StatamError::BoundsExceeded {
                        reason: "flat chunk_ids exceeds u32::MAX",
                    })?;
                history.push(Some((start_idx, n_chunks as u32, 0)));
                row_kinds.push(FLAT_REF_CHUNKS);
                chunk_offsets.push(end_idx);
                inline_offsets.push(inline_data.len() as u32);
            }
            TAG_DELTA => {
                let back = cur.read_u8()? as usize;
                if back == 0 || back > MAX_TAIL_SHARE_BACK {
                    return Err(StatamError::Corrupt {
                        reason: "payload ref delta back offset outside 1..=32",
                    });
                }
                if back > history.len {
                    return Err(StatamError::Corrupt {
                        reason: "payload ref delta back offset precedes block start",
                    });
                }
                let Some((parent_start, parent_len, parent_depth)) = history.get_back(back) else {
                    return Err(StatamError::Corrupt {
                        reason: "payload ref delta references inline row",
                    });
                };
                if (parent_depth as usize) >= MAX_REF_CHAIN_DEPTH {
                    return Err(StatamError::BoundsExceeded {
                        reason: "payload ref chain depth exceeds MAX_REF_CHAIN_DEPTH",
                    });
                }

                let keep_prefix = cur.read_u16()? as usize;
                if keep_prefix > parent_len as usize {
                    return Err(StatamError::Corrupt {
                        reason: "payload ref keep_prefix exceeds parent chunk count",
                    });
                }
                let n_new = cur.read_u16()? as usize;
                let total_chunks =
                    keep_prefix
                        .checked_add(n_new)
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "payload ref total chunk count overflow",
                        })?;
                if total_chunks > MAX_CHUNKS_PER_ROW {
                    return Err(StatamError::BoundsExceeded {
                        reason: "row chunks exceed MAX_CHUNKS_PER_ROW",
                    });
                }
                let min_wire = n_new.checked_mul(2).ok_or(StatamError::BoundsExceeded {
                    reason: "payload ref delta wire size overflow",
                })?;
                if min_wire > cur.remaining() {
                    return Err(StatamError::Corrupt {
                        reason: "truncated payload ref delta entries",
                    });
                }
                let mem_bytes = total_chunks.checked_mul(size_of::<(u16, u32)>()).ok_or(
                    StatamError::BoundsExceeded {
                        reason: "payload ref delta memory size overflow",
                    },
                )?;
                total_decoded_bytes = add_budget(total_decoded_bytes, mem_bytes, effective_max)?;

                let cur_start =
                    u32::try_from(chunk_ids.len()).map_err(|_| StatamError::BoundsExceeded {
                        reason: "flat chunk_ids exceeds u32::MAX",
                    })?;
                row_pack_idxs.reserve(total_chunks);
                chunk_ids.reserve(total_chunks);

                let p_start = parent_start as usize;
                let p_end = p_start + keep_prefix;
                row_pack_idxs.extend_from_within(p_start..p_end);
                chunk_ids.extend_from_within(p_start..p_end);

                let mut prev = if keep_prefix > 0 {
                    (row_pack_idxs[p_end - 1], chunk_ids[p_end - 1])
                } else {
                    (0u16, 0u32)
                };
                let mut row_pack = (keep_prefix > 0).then_some(prev.0);
                for _ in 0..n_new {
                    let p_delta = zigzag_decode_i64(cur.read_varint_u64()?);
                    let c_delta = zigzag_decode_i64(cur.read_varint_u64()?);
                    let next_pack = i64::from(prev.0)
                        .checked_add(p_delta)
                        .and_then(|v| u16::try_from(v).ok())
                        .ok_or(StatamError::Corrupt {
                            reason: "payload ref pack_idx delta out of u16 range",
                        })?;
                    if let Some(rp) = row_pack {
                        if next_pack != rp {
                            return Err(StatamError::Corrupt {
                                reason: "mixed pack indices within a single payload row",
                            });
                        }
                    } else {
                        row_pack = Some(next_pack);
                    }
                    let next_chunk = i64::from(prev.1)
                        .checked_add(c_delta)
                        .and_then(|v| u32::try_from(v).ok())
                        .ok_or(StatamError::Corrupt {
                            reason: "payload ref chunk_id delta out of u32 range",
                        })?;
                    prev = (next_pack, next_chunk);
                    row_pack_idxs.push(prev.0);
                    chunk_ids.push(prev.1);
                }

                let end_idx =
                    u32::try_from(chunk_ids.len()).map_err(|_| StatamError::BoundsExceeded {
                        reason: "flat chunk_ids exceeds u32::MAX",
                    })?;
                let new_depth = parent_depth + 1;
                history.push(Some((cur_start, total_chunks as u32, new_depth)));
                row_kinds.push(FLAT_REF_CHUNKS);
                chunk_offsets.push(end_idx);
                inline_offsets.push(inline_data.len() as u32);
            }
            _ => {
                return Err(StatamError::Corrupt {
                    reason: "unknown payload ref row tag",
                });
            }
        }
    }

    if !cur.is_eof() {
        return Err(StatamError::Corrupt {
            reason: "trailing bytes after payload refs block",
        });
    }

    Ok(FlatPayloadRefs {
        row_kinds,
        chunk_offsets,
        inline_offsets,
        row_pack_idxs,
        chunk_ids,
        inline_data,
    })
}

/// Sparsely decodes only `mask`-active `(row_idx, PayloadRefRow)` entries from `src`,
/// skipping inactive rows outside the `MAX_REF_CHAIN_DEPTH * MAX_TAIL_SHARE_BACK` (`480`)
/// non-null transitive history window with zero per-row heap allocation.
pub(crate) fn decode_payload_refs_selected(
    src: &[u8],
    row_count: usize,
    mask: &Bitmap,
    max_batch_bytes: usize,
) -> Result<Vec<(usize, PayloadRefRow)>, StatamError> {
    let mut used_fallback = false;
    decode_payload_refs_selected_inner(src, row_count, mask, max_batch_bytes, &mut used_fallback)
}

#[cfg(test)]
fn decode_payload_refs_selected_with_fallback_flag(
    src: &[u8],
    row_count: usize,
    mask: &Bitmap,
    max_batch_bytes: usize,
) -> Result<(Vec<(usize, PayloadRefRow)>, bool), StatamError> {
    let mut used_fallback = false;
    let out = decode_payload_refs_selected_inner(
        src,
        row_count,
        mask,
        max_batch_bytes,
        &mut used_fallback,
    )?;
    Ok((out, used_fallback))
}

/// Validates and skips `n_new` delta varint pairs for a non-active `TAG_DELTA` row outside the
/// materialized history window (or whose parent is `HistorySlot::Summary`).
///
/// When initial `prev` is known (`keep_prefix == 0` or `parent_chunks_opt.is_some()`), exact
/// cumulative `u16`/`u32` bounds are validated identically to `decode_payload_refs`. When
/// `keep_prefix > 0` and the parent is `Summary`, exact `prev` is not materialized for skipped
/// rows outside `HISTORY_WINDOW`, so unconditional delta bounds (`|p_delta| <= u16::MAX` and
/// `|c_delta| <= u32::MAX`) are validated while active rows validate exact cumulative bounds.
fn skip_payload_delta_entries(
    cur: &mut RefCursor<'_>,
    keep_prefix: usize,
    n_new: usize,
    parent_chunks_opt: Option<&[(u16, u32)]>,
) -> Result<(), StatamError> {
    let exact_prev = if keep_prefix == 0 {
        Some((0u16, 0u32))
    } else {
        parent_chunks_opt.map(|p| p[keep_prefix - 1])
    };
    if let Some(mut prev) = exact_prev {
        let mut row_pack = (keep_prefix > 0).then_some(prev.0);
        for _ in 0..n_new {
            let p_delta = zigzag_decode_i64(cur.read_varint_u64()?);
            let c_delta = zigzag_decode_i64(cur.read_varint_u64()?);
            let next_pack = i64::from(prev.0)
                .checked_add(p_delta)
                .and_then(|v| u16::try_from(v).ok())
                .ok_or(StatamError::Corrupt {
                    reason: "payload ref pack_idx delta out of u16 range",
                })?;
            if let Some(rp) = row_pack {
                if next_pack != rp {
                    return Err(StatamError::Corrupt {
                        reason: "mixed pack indices within a single payload row",
                    });
                }
            } else {
                row_pack = Some(next_pack);
            }
            let next_chunk = i64::from(prev.1)
                .checked_add(c_delta)
                .and_then(|v| u32::try_from(v).ok())
                .ok_or(StatamError::Corrupt {
                    reason: "payload ref chunk_id delta out of u32 range",
                })?;
            prev = (next_pack, next_chunk);
        }
    } else {
        // `keep_prefix > 0` with `HistorySlot::Summary`: every new entry must keep the parent's
        // uniform pack_idx, so `p_delta` must be `0`.
        for _ in 0..n_new {
            let p_delta = zigzag_decode_i64(cur.read_varint_u64()?);
            let c_delta = zigzag_decode_i64(cur.read_varint_u64()?);
            if p_delta != 0 {
                return Err(StatamError::Corrupt {
                    reason: "mixed pack indices within a single payload row",
                });
            }
            if c_delta.unsigned_abs() > u64::from(u32::MAX) {
                return Err(StatamError::Corrupt {
                    reason: "payload ref chunk_id delta out of u32 range",
                });
            }
        }
    }
    Ok(())
}

fn decode_payload_refs_selected_inner(
    src: &[u8],
    row_count: usize,
    mask: &Bitmap,
    max_batch_bytes: usize,
    used_fallback: &mut bool,
) -> Result<Vec<(usize, PayloadRefRow)>, StatamError> {
    if mask.len() != row_count {
        return Err(StatamError::Corrupt {
            reason: "selection mask length mismatch",
        });
    }
    if row_count as u64 > MAX_ROWS_PER_FILE || row_count > MAX_BLOCK_RAW_BYTES / 32 {
        return Err(StatamError::BoundsExceeded {
            reason: "payload refs row_count exceeds safe block limit",
        });
    }

    let selected_count = mask.count_ones();
    if selected_count > 0 && (mask.all_ones() || selected_count.saturating_mul(8) >= row_count) {
        let full = decode_payload_refs(src, row_count, max_batch_bytes)?;
        return Ok(full
            .into_iter()
            .enumerate()
            .filter(|(r, _)| mask.get(*r))
            .collect());
    }

    let effective_max = max_batch_bytes.min(MAX_BATCH_DECODED_BYTES);
    let selected_vec_bytes = selected_count
        .checked_mul(size_of::<(usize, PayloadRefRow)>())
        .ok_or(StatamError::BoundsExceeded {
            reason: "payload refs selected vector size overflow",
        })?;
    if selected_vec_bytes > effective_max {
        return Err(StatamError::BoundsExceeded {
            reason: "payload refs selected vector exceeds max_batch_bytes",
        });
    }

    let mut cur = RefCursor::new(src);
    let has_nulls = cur.read_u8()?;
    let validity = match has_nulls {
        0 => None,
        1 => {
            let bm_bytes = row_count.div_ceil(8);
            if bm_bytes > cur.remaining() {
                return Err(StatamError::Corrupt {
                    reason: "truncated payload refs null bitmap",
                });
            }
            let bm_slice = cur.read_exact(bm_bytes)?;
            Some(Bitmap::from_bytes(bm_slice, row_count)?)
        }
        _ => {
            return Err(StatamError::Corrupt {
                reason: "invalid payload refs has_nulls flag",
            });
        }
    };

    let non_null_count = match &validity {
        Some(bm) => bm.count_ones(),
        None => row_count,
    };
    if non_null_count > cur.remaining() {
        return Err(StatamError::Corrupt {
            reason: "truncated payload refs row stream",
        });
    }

    enum HistorySlot {
        Inline,
        Summary { len: usize, depth: u8 },
        Full { chunks: Vec<(u16, u32)>, depth: u8 },
    }

    const HISTORY_WINDOW: usize = MAX_REF_CHAIN_DEPTH * MAX_TAIL_SHARE_BACK;

    let active_rows: Vec<usize> = if selected_count == 0 {
        Vec::new()
    } else {
        let mut rows = Vec::with_capacity(selected_count);
        rows.extend(mask.iter_ones());
        rows
    };

    // Map active non-null rows to their 0-based non-null ordinals (`nn_idx`) in O(row_count / 64).
    let active_nn: Vec<usize> = match &validity {
        None => active_rows.clone(),
        Some(bm) => {
            let words = bm.words();
            let mut nn_list = Vec::with_capacity(active_rows.len());
            let mut cur_word_idx = 0usize;
            let mut prefix_nn = 0usize;
            for &r in &active_rows {
                let w_idx = r / 64;
                while cur_word_idx < w_idx {
                    prefix_nn += words[cur_word_idx].count_ones() as usize;
                    cur_word_idx += 1;
                }
                let bit = r % 64;
                let word = words[w_idx];
                if ((word >> bit) & 1) != 0 {
                    let low_mask = (1u64 << bit) - 1;
                    let nn = prefix_nn + (word & low_mask).count_ones() as usize;
                    nn_list.push(nn);
                }
            }
            nn_list
        }
    };

    let mut next_active_idx = 0usize;
    let mut next_active_nn = 0usize;
    let mut nn_idx = 0usize;

    let mut out = Vec::with_capacity(active_rows.len());
    let mut history: VecDeque<HistorySlot> = VecDeque::with_capacity(MAX_TAIL_SHARE_BACK);
    let mut total_decoded_bytes: usize = 0;

    let push_slot = |history: &mut VecDeque<HistorySlot>, slot: HistorySlot| {
        if history.len() == MAX_TAIL_SHARE_BACK {
            history.pop_front();
        }
        history.push_back(slot);
    };

    for row_idx in 0..row_count {
        let is_active = if active_rows.get(next_active_idx).copied() == Some(row_idx) {
            next_active_idx += 1;
            true
        } else {
            false
        };
        if validity.as_ref().is_some_and(|bm| !bm.get(row_idx)) {
            if is_active {
                out.push((row_idx, PayloadRefRow::Null));
            }
            continue;
        }

        if is_active && active_nn.get(next_active_nn).copied() == Some(nn_idx) {
            next_active_nn += 1;
        }
        let need_full = active_nn
            .get(next_active_nn)
            .is_some_and(|&a_nn| a_nn <= nn_idx.saturating_add(HISTORY_WINDOW));

        let tag = cur.read_u8()?;
        match tag {
            TAG_INLINE => {
                let len = cur.read_u16()? as usize;
                if len > cur.remaining() {
                    return Err(StatamError::Corrupt {
                        reason: "truncated inline payload bytes",
                    });
                }
                let bytes = cur.read_exact(len)?;
                if is_active {
                    total_decoded_bytes = add_budget(total_decoded_bytes, len, effective_max)?;
                    out.push((row_idx, PayloadRefRow::Inline(bytes.to_vec())));
                }
                push_slot(&mut history, HistorySlot::Inline);
            }
            TAG_KEYFRAME => {
                let n_chunks = cur.read_u16()? as usize;
                if n_chunks > MAX_CHUNKS_PER_ROW {
                    return Err(StatamError::BoundsExceeded {
                        reason: "keyframe chunk count exceeds MAX_CHUNKS_PER_ROW",
                    });
                }
                let wire_bytes = n_chunks.checked_mul(6).ok_or(StatamError::BoundsExceeded {
                    reason: "keyframe wire byte size overflow",
                })?;
                if wire_bytes > cur.remaining() {
                    return Err(StatamError::Corrupt {
                        reason: "truncated keyframe chunk refs",
                    });
                }
                if !is_active && !need_full {
                    let raw_pairs = cur.read_exact(wire_bytes)?;
                    if n_chunks > 1 {
                        let first_pack = u16::from_le_bytes([raw_pairs[0], raw_pairs[1]]);
                        for pair in raw_pairs.as_chunks::<6>().0.iter().skip(1) {
                            let pack_idx = u16::from_le_bytes([pair[0], pair[1]]);
                            if pack_idx != first_pack {
                                return Err(StatamError::Corrupt {
                                    reason: "mixed pack indices within a single payload row",
                                });
                            }
                        }
                    }
                    push_slot(
                        &mut history,
                        HistorySlot::Summary {
                            len: n_chunks,
                            depth: 0,
                        },
                    );
                } else {
                    let mem_bytes = n_chunks.checked_mul(size_of::<(u16, u32)>()).ok_or(
                        StatamError::BoundsExceeded {
                            reason: "keyframe memory size overflow",
                        },
                    )?;
                    total_decoded_bytes =
                        add_budget(total_decoded_bytes, mem_bytes, effective_max)?;
                    let mut refs = Vec::with_capacity(n_chunks);
                    let mut first_pack: Option<u16> = None;
                    for _ in 0..n_chunks {
                        let pack_idx = cur.read_u16()?;
                        let chunk_id = cur.read_u32()?;
                        if let Some(fp) = first_pack {
                            if pack_idx != fp {
                                return Err(StatamError::Corrupt {
                                    reason: "mixed pack indices within a single payload row",
                                });
                            }
                        } else {
                            first_pack = Some(pack_idx);
                        }
                        refs.push((pack_idx, chunk_id));
                    }
                    if is_active && need_full {
                        out.push((row_idx, PayloadRefRow::Chunks(refs.clone())));
                        push_slot(
                            &mut history,
                            HistorySlot::Full {
                                chunks: refs,
                                depth: 0,
                            },
                        );
                    } else if is_active {
                        out.push((row_idx, PayloadRefRow::Chunks(refs)));
                        push_slot(
                            &mut history,
                            HistorySlot::Summary {
                                len: n_chunks,
                                depth: 0,
                            },
                        );
                    } else {
                        push_slot(
                            &mut history,
                            HistorySlot::Full {
                                chunks: refs,
                                depth: 0,
                            },
                        );
                    }
                }
            }
            TAG_DELTA => {
                let back = cur.read_u8()? as usize;
                if back == 0 || back > MAX_TAIL_SHARE_BACK {
                    return Err(StatamError::Corrupt {
                        reason: "payload ref delta back offset outside 1..=32",
                    });
                }
                if back > history.len() {
                    return Err(StatamError::Corrupt {
                        reason: "payload ref delta back offset precedes block start",
                    });
                }
                let (parent_len, parent_depth, parent_chunks_opt) =
                    match &history[history.len() - back] {
                        HistorySlot::Inline => {
                            return Err(StatamError::Corrupt {
                                reason: "payload ref delta references inline row",
                            });
                        }
                        HistorySlot::Summary { len, depth } => (*len, *depth, None),
                        HistorySlot::Full { chunks, depth } => {
                            (chunks.len(), *depth, Some(chunks.as_slice()))
                        }
                    };
                if (parent_depth as usize) >= MAX_REF_CHAIN_DEPTH {
                    return Err(StatamError::BoundsExceeded {
                        reason: "payload ref chain depth exceeds MAX_REF_CHAIN_DEPTH",
                    });
                }

                let keep_prefix = cur.read_u16()? as usize;
                if keep_prefix > parent_len {
                    return Err(StatamError::Corrupt {
                        reason: "payload ref keep_prefix exceeds parent chunk count",
                    });
                }
                let n_new = cur.read_u16()? as usize;
                let total_chunks =
                    keep_prefix
                        .checked_add(n_new)
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "payload ref total chunk count overflow",
                        })?;
                if total_chunks > MAX_CHUNKS_PER_ROW {
                    return Err(StatamError::BoundsExceeded {
                        reason: "row chunks exceed MAX_CHUNKS_PER_ROW",
                    });
                }
                let min_wire = n_new.checked_mul(2).ok_or(StatamError::BoundsExceeded {
                    reason: "payload ref delta wire size overflow",
                })?;
                if min_wire > cur.remaining() {
                    return Err(StatamError::Corrupt {
                        reason: "truncated payload ref delta entries",
                    });
                }

                let new_depth = parent_depth + 1;
                if !is_active && (!need_full || parent_chunks_opt.is_none()) {
                    skip_payload_delta_entries(&mut cur, keep_prefix, n_new, parent_chunks_opt)?;
                    push_slot(
                        &mut history,
                        HistorySlot::Summary {
                            len: total_chunks,
                            depth: new_depth,
                        },
                    );
                } else {
                    let Some(parent_chunks) = parent_chunks_opt else {
                        *used_fallback = true;
                        let full = decode_payload_refs(src, row_count, max_batch_bytes)?;
                        return Ok(full
                            .into_iter()
                            .enumerate()
                            .filter(|(r, _)| mask.get(*r))
                            .collect());
                    };
                    let mem_bytes = total_chunks.checked_mul(size_of::<(u16, u32)>()).ok_or(
                        StatamError::BoundsExceeded {
                            reason: "payload ref delta memory size overflow",
                        },
                    )?;
                    total_decoded_bytes =
                        add_budget(total_decoded_bytes, mem_bytes, effective_max)?;

                    let mut refs = Vec::with_capacity(total_chunks);
                    refs.extend_from_slice(&parent_chunks[..keep_prefix]);

                    let mut prev = if keep_prefix > 0 {
                        parent_chunks[keep_prefix - 1]
                    } else {
                        (0u16, 0u32)
                    };
                    let mut row_pack = (keep_prefix > 0).then_some(prev.0);
                    for _ in 0..n_new {
                        let p_delta = zigzag_decode_i64(cur.read_varint_u64()?);
                        let c_delta = zigzag_decode_i64(cur.read_varint_u64()?);
                        let next_pack = i64::from(prev.0)
                            .checked_add(p_delta)
                            .and_then(|v| u16::try_from(v).ok())
                            .ok_or(StatamError::Corrupt {
                                reason: "payload ref pack_idx delta out of u16 range",
                            })?;
                        if let Some(rp) = row_pack {
                            if next_pack != rp {
                                return Err(StatamError::Corrupt {
                                    reason: "mixed pack indices within a single payload row",
                                });
                            }
                        } else {
                            row_pack = Some(next_pack);
                        }
                        let next_chunk = i64::from(prev.1)
                            .checked_add(c_delta)
                            .and_then(|v| u32::try_from(v).ok())
                            .ok_or(StatamError::Corrupt {
                                reason: "payload ref chunk_id delta out of u32 range",
                            })?;
                        prev = (next_pack, next_chunk);
                        refs.push(prev);
                    }

                    if is_active && need_full {
                        out.push((row_idx, PayloadRefRow::Chunks(refs.clone())));
                        push_slot(
                            &mut history,
                            HistorySlot::Full {
                                chunks: refs,
                                depth: new_depth,
                            },
                        );
                    } else if is_active {
                        out.push((row_idx, PayloadRefRow::Chunks(refs)));
                        push_slot(
                            &mut history,
                            HistorySlot::Summary {
                                len: total_chunks,
                                depth: new_depth,
                            },
                        );
                    } else {
                        push_slot(
                            &mut history,
                            HistorySlot::Full {
                                chunks: refs,
                                depth: new_depth,
                            },
                        );
                    }
                }
            }
            _ => {
                return Err(StatamError::Corrupt {
                    reason: "unknown payload ref row tag",
                });
            }
        }
        nn_idx += 1;
    }

    if !cur.is_eof() {
        return Err(StatamError::Corrupt {
            reason: "trailing bytes after payload refs block",
        });
    }

    Ok(out)
}

enum ScanHistoryEntry<'a> {
    Inline,
    Keyframe {
        raw_pairs: &'a [u8],
        n_chunks: usize,
    },
    Delta {
        chunks: Vec<(u16, u32)>,
        depth: u8,
    },
    DeltaAtMaxDepth,
}

#[inline]
fn validate_pack_chunk(
    pack_idx: u16,
    chunk_id: u32,
    pack_chunk_counts: &[usize],
) -> Result<(), StatamError> {
    let max_chunks =
        pack_chunk_counts
            .get(pack_idx as usize)
            .copied()
            .ok_or(StatamError::Corrupt {
                reason: "payload ref pack_idx out of bounds",
            })?;
    if (chunk_id as usize) >= max_chunks {
        return Err(StatamError::Corrupt {
            reason: "payload ref chunk_id out of pack bounds",
        });
    }
    Ok(())
}

#[inline]
fn record_pack_ref_in_row(
    pack_idx: u16,
    pack_has_edges: &[bool],
    pack_counts: &mut [u32],
    touched_packs: &mut Vec<u16>,
) -> Result<(), StatamError> {
    let pidx = pack_idx as usize;
    if !pack_has_edges[pidx] {
        if pack_counts[pidx] == 0 {
            touched_packs.push(pack_idx);
        }
        pack_counts[pidx] += 1;
        if pack_counts[pidx] > 1 {
            return Err(StatamError::Corrupt {
                reason: "multi-chunk row references pack without edge table",
            });
        }
    }
    Ok(())
}

#[inline]
fn push_scan_history<'a>(
    history: &mut VecDeque<ScanHistoryEntry<'a>>,
    entry: ScanHistoryEntry<'a>,
    recycle_vecs: &mut Vec<Vec<(u16, u32)>>,
) {
    if history.len() == MAX_TAIL_SHARE_BACK
        && let Some(ScanHistoryEntry::Delta { mut chunks, .. }) = history.pop_front()
    {
        chunks.clear();
        recycle_vecs.push(chunks);
    }
    history.push_back(entry);
}

#[inline]
fn read_raw_pair(raw_pairs: &[u8], idx: usize) -> (u16, u32) {
    let off = idx * 6;
    let p = u16::from_le_bytes([raw_pairs[off], raw_pairs[off + 1]]);
    let c = u32::from_le_bytes([
        raw_pairs[off + 2],
        raw_pairs[off + 3],
        raw_pairs[off + 4],
        raw_pairs[off + 5],
    ]);
    (p, c)
}

/// Scans a `PayloadRefs` wire block directly when all external packs short-circuit,
/// validating all wire bounds and chunk references without allocating `PayloadRefRow` vectors.
pub(crate) fn scan_payload_refs_inline_substring(
    src: &[u8],
    row_count: usize,
    active_mask: &Bitmap,
    pack_chunk_counts: &[usize],
    pack_has_edges: &[bool],
    mut match_inline: impl FnMut(&[u8]) -> bool,
) -> Result<Bitmap, StatamError> {
    if active_mask.len() != row_count {
        return Err(StatamError::Corrupt {
            reason: "active mask length mismatch in payload refs scan",
        });
    }
    if pack_has_edges.len() != pack_chunk_counts.len() {
        return Err(StatamError::Corrupt {
            reason: "pack metadata length mismatch in payload refs scan",
        });
    }
    if row_count as u64 > MAX_ROWS_PER_FILE || row_count > MAX_BLOCK_RAW_BYTES / 32 {
        return Err(StatamError::BoundsExceeded {
            reason: "payload refs row_count exceeds safe block limit",
        });
    }

    let mut cur = RefCursor::new(src);
    let has_nulls = cur.read_u8()?;
    let validity = match has_nulls {
        0 => None,
        1 => {
            let bm_bytes = row_count.div_ceil(8);
            if bm_bytes > cur.remaining() {
                return Err(StatamError::Corrupt {
                    reason: "truncated payload refs null bitmap",
                });
            }
            let bm_slice = cur.read_exact(bm_bytes)?;
            Some(Bitmap::from_bytes(bm_slice, row_count)?)
        }
        _ => {
            return Err(StatamError::Corrupt {
                reason: "invalid payload refs has_nulls flag",
            });
        }
    };

    let non_null_count = match &validity {
        Some(bm) => bm.count_ones(),
        None => row_count,
    };
    if non_null_count > cur.remaining() {
        return Err(StatamError::Corrupt {
            reason: "truncated payload refs row stream",
        });
    }

    let mut results = Bitmap::new_zeroed(row_count);
    let mut history: VecDeque<ScanHistoryEntry<'_>> = VecDeque::with_capacity(MAX_TAIL_SHARE_BACK);
    let mut recycle_vecs: Vec<Vec<(u16, u32)>> = Vec::new();
    let mut total_decoded_bytes: usize = 0;
    let any_no_edges = pack_has_edges.iter().any(|&has| !has);
    let mut pack_counts = if any_no_edges {
        vec![0u32; pack_has_edges.len()]
    } else {
        Vec::new()
    };
    let mut touched_packs: Vec<u16> = Vec::new();

    for row_idx in 0..row_count {
        if validity.as_ref().is_some_and(|bm| !bm.get(row_idx)) {
            continue;
        }

        let tag = cur.read_u8()?;
        match tag {
            TAG_INLINE => {
                let len = cur.read_u16()? as usize;
                if len > cur.remaining() {
                    return Err(StatamError::Corrupt {
                        reason: "truncated inline payload bytes",
                    });
                }
                total_decoded_bytes =
                    add_budget(total_decoded_bytes, len, MAX_BATCH_DECODED_BYTES)?;
                let bytes = cur.read_exact(len)?;
                if active_mask.get(row_idx) && match_inline(bytes) {
                    let _ = results.set(row_idx, true);
                }
                push_scan_history(&mut history, ScanHistoryEntry::Inline, &mut recycle_vecs);
            }
            TAG_KEYFRAME => {
                let n_chunks = cur.read_u16()? as usize;
                if n_chunks > MAX_CHUNKS_PER_ROW {
                    return Err(StatamError::BoundsExceeded {
                        reason: "keyframe chunk count exceeds MAX_CHUNKS_PER_ROW",
                    });
                }
                let wire_bytes = n_chunks.checked_mul(6).ok_or(StatamError::BoundsExceeded {
                    reason: "keyframe wire byte size overflow",
                })?;
                if wire_bytes > cur.remaining() {
                    return Err(StatamError::Corrupt {
                        reason: "truncated keyframe chunk refs",
                    });
                }
                let mem_bytes = n_chunks.checked_mul(size_of::<(u16, u32)>()).ok_or(
                    StatamError::BoundsExceeded {
                        reason: "keyframe memory size overflow",
                    },
                )?;
                total_decoded_bytes =
                    add_budget(total_decoded_bytes, mem_bytes, MAX_BATCH_DECODED_BYTES)?;
                let raw_pairs = cur.read_exact(wire_bytes)?;
                let check_edges = any_no_edges && n_chunks > 1;
                let mut first_pack: Option<u16> = None;
                for pair in raw_pairs.as_chunks::<6>().0 {
                    let pack_idx = u16::from_le_bytes([pair[0], pair[1]]);
                    let chunk_id = u32::from_le_bytes([pair[2], pair[3], pair[4], pair[5]]);
                    if let Some(fp) = first_pack {
                        if pack_idx != fp {
                            return Err(StatamError::Corrupt {
                                reason: "mixed pack indices within a single payload row",
                            });
                        }
                    } else {
                        first_pack = Some(pack_idx);
                    }
                    validate_pack_chunk(pack_idx, chunk_id, pack_chunk_counts)?;
                    if check_edges {
                        record_pack_ref_in_row(
                            pack_idx,
                            pack_has_edges,
                            &mut pack_counts,
                            &mut touched_packs,
                        )?;
                    }
                }
                if check_edges {
                    for p in touched_packs.drain(..) {
                        pack_counts[p as usize] = 0;
                    }
                }
                push_scan_history(
                    &mut history,
                    ScanHistoryEntry::Keyframe {
                        raw_pairs,
                        n_chunks,
                    },
                    &mut recycle_vecs,
                );
            }
            TAG_DELTA => {
                let back = cur.read_u8()? as usize;
                if back == 0 || back > MAX_TAIL_SHARE_BACK {
                    return Err(StatamError::Corrupt {
                        reason: "payload ref delta back offset outside 1..=32",
                    });
                }
                if back > history.len() {
                    return Err(StatamError::Corrupt {
                        reason: "payload ref delta back offset precedes block start",
                    });
                }

                let (parent_len, parent_depth) = match &history[history.len() - back] {
                    ScanHistoryEntry::Inline => {
                        return Err(StatamError::Corrupt {
                            reason: "payload ref delta references inline row",
                        });
                    }
                    ScanHistoryEntry::Keyframe { n_chunks, .. } => (*n_chunks, 0u8),
                    ScanHistoryEntry::Delta { chunks, depth } => (chunks.len(), *depth),
                    ScanHistoryEntry::DeltaAtMaxDepth => {
                        return Err(StatamError::BoundsExceeded {
                            reason: "payload ref chain depth exceeds MAX_REF_CHAIN_DEPTH",
                        });
                    }
                };
                if (parent_depth as usize) >= MAX_REF_CHAIN_DEPTH {
                    return Err(StatamError::BoundsExceeded {
                        reason: "payload ref chain depth exceeds MAX_REF_CHAIN_DEPTH",
                    });
                }

                let keep_prefix = cur.read_u16()? as usize;
                if keep_prefix > parent_len {
                    return Err(StatamError::Corrupt {
                        reason: "payload ref keep_prefix exceeds parent chunk count",
                    });
                }
                let n_new = cur.read_u16()? as usize;
                let total_chunks =
                    keep_prefix
                        .checked_add(n_new)
                        .ok_or(StatamError::BoundsExceeded {
                            reason: "payload ref total chunk count overflow",
                        })?;
                if total_chunks > MAX_CHUNKS_PER_ROW {
                    return Err(StatamError::BoundsExceeded {
                        reason: "row chunks exceed MAX_CHUNKS_PER_ROW",
                    });
                }
                let min_wire = n_new.checked_mul(2).ok_or(StatamError::BoundsExceeded {
                    reason: "payload ref delta wire size overflow",
                })?;
                if min_wire > cur.remaining() {
                    return Err(StatamError::Corrupt {
                        reason: "truncated payload ref delta entries",
                    });
                }
                let mem_bytes = total_chunks.checked_mul(size_of::<(u16, u32)>()).ok_or(
                    StatamError::BoundsExceeded {
                        reason: "payload ref delta memory size overflow",
                    },
                )?;
                total_decoded_bytes =
                    add_budget(total_decoded_bytes, mem_bytes, MAX_BATCH_DECODED_BYTES)?;

                let new_depth = parent_depth + 1;
                let mut prev = if keep_prefix > 0 {
                    match &history[history.len() - back] {
                        ScanHistoryEntry::Keyframe { raw_pairs, .. } => {
                            read_raw_pair(raw_pairs, keep_prefix - 1)
                        }
                        ScanHistoryEntry::Delta { chunks, .. } => chunks[keep_prefix - 1],
                        ScanHistoryEntry::Inline | ScanHistoryEntry::DeltaAtMaxDepth => {
                            unreachable!()
                        }
                    }
                } else {
                    (0u16, 0u32)
                };

                if (new_depth as usize) >= MAX_REF_CHAIN_DEPTH {
                    let check_edges = any_no_edges && total_chunks > 1;
                    if check_edges && keep_prefix > 0 {
                        match &history[history.len() - back] {
                            ScanHistoryEntry::Keyframe { raw_pairs, .. } => {
                                for idx in 0..keep_prefix {
                                    let (p, _) = read_raw_pair(raw_pairs, idx);
                                    record_pack_ref_in_row(
                                        p,
                                        pack_has_edges,
                                        &mut pack_counts,
                                        &mut touched_packs,
                                    )?;
                                }
                            }
                            ScanHistoryEntry::Delta { chunks, .. } => {
                                for &(p, _) in &chunks[..keep_prefix] {
                                    record_pack_ref_in_row(
                                        p,
                                        pack_has_edges,
                                        &mut pack_counts,
                                        &mut touched_packs,
                                    )?;
                                }
                            }
                            ScanHistoryEntry::Inline | ScanHistoryEntry::DeltaAtMaxDepth => {
                                unreachable!()
                            }
                        }
                    }
                    let mut row_pack = (keep_prefix > 0).then_some(prev.0);
                    for _ in 0..n_new {
                        let p_delta = zigzag_decode_i64(cur.read_varint_u64()?);
                        let c_delta = zigzag_decode_i64(cur.read_varint_u64()?);
                        let next_pack = i64::from(prev.0)
                            .checked_add(p_delta)
                            .and_then(|v| u16::try_from(v).ok())
                            .ok_or(StatamError::Corrupt {
                                reason: "payload ref pack_idx delta out of u16 range",
                            })?;
                        if let Some(rp) = row_pack {
                            if next_pack != rp {
                                return Err(StatamError::Corrupt {
                                    reason: "mixed pack indices within a single payload row",
                                });
                            }
                        } else {
                            row_pack = Some(next_pack);
                        }
                        let next_chunk = i64::from(prev.1)
                            .checked_add(c_delta)
                            .and_then(|v| u32::try_from(v).ok())
                            .ok_or(StatamError::Corrupt {
                                reason: "payload ref chunk_id delta out of u32 range",
                            })?;
                        validate_pack_chunk(next_pack, next_chunk, pack_chunk_counts)?;
                        if check_edges {
                            record_pack_ref_in_row(
                                next_pack,
                                pack_has_edges,
                                &mut pack_counts,
                                &mut touched_packs,
                            )?;
                        }
                        prev = (next_pack, next_chunk);
                    }
                    if check_edges {
                        for p in touched_packs.drain(..) {
                            pack_counts[p as usize] = 0;
                        }
                    }
                    push_scan_history(
                        &mut history,
                        ScanHistoryEntry::DeltaAtMaxDepth,
                        &mut recycle_vecs,
                    );
                } else {
                    let mut refs = recycle_vecs.pop().unwrap_or_default();
                    refs.clear();
                    refs.reserve(total_chunks);
                    match &history[history.len() - back] {
                        ScanHistoryEntry::Keyframe { raw_pairs, .. } => {
                            for idx in 0..keep_prefix {
                                refs.push(read_raw_pair(raw_pairs, idx));
                            }
                        }
                        ScanHistoryEntry::Delta { chunks, .. } => {
                            refs.extend_from_slice(&chunks[..keep_prefix]);
                        }
                        ScanHistoryEntry::Inline | ScanHistoryEntry::DeltaAtMaxDepth => {
                            unreachable!()
                        }
                    }
                    let mut row_pack = (keep_prefix > 0).then_some(prev.0);
                    for _ in 0..n_new {
                        let p_delta = zigzag_decode_i64(cur.read_varint_u64()?);
                        let c_delta = zigzag_decode_i64(cur.read_varint_u64()?);
                        let next_pack = i64::from(prev.0)
                            .checked_add(p_delta)
                            .and_then(|v| u16::try_from(v).ok())
                            .ok_or(StatamError::Corrupt {
                                reason: "payload ref pack_idx delta out of u16 range",
                            })?;
                        if let Some(rp) = row_pack {
                            if next_pack != rp {
                                return Err(StatamError::Corrupt {
                                    reason: "mixed pack indices within a single payload row",
                                });
                            }
                        } else {
                            row_pack = Some(next_pack);
                        }
                        let next_chunk = i64::from(prev.1)
                            .checked_add(c_delta)
                            .and_then(|v| u32::try_from(v).ok())
                            .ok_or(StatamError::Corrupt {
                                reason: "payload ref chunk_id delta out of u32 range",
                            })?;
                        validate_pack_chunk(next_pack, next_chunk, pack_chunk_counts)?;
                        prev = (next_pack, next_chunk);
                        refs.push(prev);
                    }
                    if any_no_edges && refs.len() > 1 {
                        for &(p, _) in &refs {
                            record_pack_ref_in_row(
                                p,
                                pack_has_edges,
                                &mut pack_counts,
                                &mut touched_packs,
                            )?;
                        }
                        for p in touched_packs.drain(..) {
                            pack_counts[p as usize] = 0;
                        }
                    }
                    push_scan_history(
                        &mut history,
                        ScanHistoryEntry::Delta {
                            chunks: refs,
                            depth: new_depth,
                        },
                        &mut recycle_vecs,
                    );
                }
            }
            _ => {
                return Err(StatamError::Corrupt {
                    reason: "unknown payload ref row tag",
                });
            }
        }
    }

    if !cur.is_eof() {
        return Err(StatamError::Corrupt {
            reason: "trailing bytes after payload refs block",
        });
    }

    Ok(results)
}

fn common_prefix_len(a: &[(u16, u32)], b: &[(u16, u32)]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

fn push_history_capped(history: &mut RefHistory, entry: RefHistoryEntry, cap: usize) {
    if history.len() == cap {
        history.pop_front();
    }
    history.push_back(entry);
}

fn push_history(history: &mut RefHistory, entry: RefHistoryEntry) {
    push_history_capped(history, entry, MAX_TAIL_SHARE_BACK);
}

fn add_budget(current: usize, delta: usize, max_bytes: usize) -> Result<usize, StatamError> {
    let next = current
        .checked_add(delta)
        .ok_or(StatamError::BoundsExceeded {
            reason: "payload refs decoded byte count overflow",
        })?;
    if next > max_bytes {
        return Err(StatamError::BoundsExceeded {
            reason: "payload refs decoded bytes exceed max_batch_bytes",
        });
    }
    Ok(next)
}

fn encode_varint_u64(mut val: u64, out: &mut Vec<u8>) {
    while val >= 0x80 {
        out.push(((val as u8) & 0x7F) | 0x80);
        val >>= 7;
    }
    out.push(val as u8);
}

struct RefCursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> RefCursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    fn is_eof(&self) -> bool {
        self.pos == self.buf.len()
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], StatamError> {
        let end = self.pos.checked_add(len).ok_or(StatamError::Corrupt {
            reason: "payload ref cursor overflow",
        })?;
        if end > self.buf.len() {
            return Err(StatamError::Corrupt {
                reason: "unexpected end of payload ref buffer",
            });
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn read_u8(&mut self) -> Result<u8, StatamError> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u16(&mut self) -> Result<u16, StatamError> {
        let s = self.read_exact(2)?;
        Ok(u16::from_le_bytes([s[0], s[1]]))
    }

    fn read_u32(&mut self) -> Result<u32, StatamError> {
        let s = self.read_exact(4)?;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }

    fn read_varint_u64(&mut self) -> Result<u64, StatamError> {
        let mut res: u64 = 0;
        let mut shift: u32 = 0;
        for _ in 0..10 {
            let b = self.read_u8()?;
            let low = u64::from(b & 0x7F);
            if shift == 63 && low > 1 {
                return Err(StatamError::Corrupt {
                    reason: "varint overflow in payload ref delta",
                });
            }
            res |= low << shift;
            if b & 0x80 == 0 {
                return Ok(res);
            }
            shift += 7;
        }
        Err(StatamError::Corrupt {
            reason: "overlong varint in payload ref delta",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_refs_tail_sharing_collapses_multiturn_and_bounds_depth_15() {
        // 40 turns growing monotonically by 1 chunk each -> tests both lookback and depth 15 reset!
        let mut rows = Vec::new();
        for turn in 1..=40u32 {
            if turn % 11 == 0 {
                rows.push(PayloadRefRow::Null);
            } else if turn == 1 {
                rows.push(PayloadRefRow::Inline(b"short".to_vec()));
            }
            let refs: Vec<(u16, u32)> = (0..turn * 2).map(|c| (0u16, c)).collect();
            rows.push(PayloadRefRow::Chunks(refs));
        }

        let raw_ref_bytes: usize = rows
            .iter()
            .map(|r| match r {
                PayloadRefRow::Null => 0,
                PayloadRefRow::Inline(b) => 3 + b.len(),
                PayloadRefRow::Chunks(c) => 3 + c.len() * 6,
            })
            .sum();

        let mut encoded = Vec::new();
        encode_payload_refs(&rows, &mut encoded).unwrap();
        // Tail sharing should achieve huge compression over naive keyframe encoding.
        assert!(
            encoded.len() * 4 < raw_ref_bytes,
            "expected >4x ref compression, got {} vs {raw_ref_bytes}",
            encoded.len()
        );

        let decoded = decode_payload_refs(&encoded, rows.len(), MAX_BATCH_DECODED_BYTES).unwrap();
        assert_eq!(decoded, rows);

        // Specifically verify depth never exceeded 15 even over a 40-row chain.
        // Build a pure 25-row chain where each row appends 1 chunk to the previous row.
        let mut chain_rows = Vec::new();
        for len in 10..=34u32 {
            let refs: Vec<(u16, u32)> = (0..len).map(|c| (0u16, c)).collect();
            chain_rows.push(PayloadRefRow::Chunks(refs));
        }
        let mut chain_enc = Vec::new();
        encode_payload_refs(&chain_rows, &mut chain_enc).unwrap();
        let chain_dec =
            decode_payload_refs(&chain_enc, chain_rows.len(), MAX_BATCH_DECODED_BYTES).unwrap();
        assert_eq!(chain_dec, chain_rows);
    }

    #[test]
    fn payload_refs_rejects_amplification_bomb_and_cycle_depth() {
        // 1. Amplification bomb: 1 keyframe of 1,000 chunks + 50 delta rows cloning all 1,000 chunks.
        // Encoded size is ~6 KB, decoded heap ref footprint is 51 * 1,000 * 8 = 408 KB.
        let mut bomb = Vec::new();
        bomb.push(0); // has_nulls = 0
        bomb.push(TAG_KEYFRAME);
        bomb.extend_from_slice(&1000u16.to_le_bytes());
        for i in 0..1000u32 {
            bomb.extend_from_slice(&0u16.to_le_bytes());
            bomb.extend_from_slice(&i.to_le_bytes());
        }
        for _ in 0..50 {
            bomb.push(TAG_DELTA);
            bomb.push(1); // back = 1 (actually stays depth <= 15 if alternating back, wait: let's point back=1 up to 15, or back=1..15 to row 0!)
            bomb.extend_from_slice(&1000u16.to_le_bytes());
            bomb.extend_from_slice(&0u16.to_le_bytes());
        }
        // Wait: using back=1 50 times would hit depth 15 first; point every delta to row 0 (back = i)!
        let mut bomb_depth1 = Vec::new();
        bomb_depth1.push(0); // has_nulls = 0
        bomb_depth1.push(TAG_KEYFRAME);
        bomb_depth1.extend_from_slice(&1000u16.to_le_bytes());
        for i in 0..1000u32 {
            bomb_depth1.extend_from_slice(&0u16.to_le_bytes());
            bomb_depth1.extend_from_slice(&i.to_le_bytes());
        }
        for i in 1..=30u8 {
            bomb_depth1.push(TAG_DELTA);
            bomb_depth1.push(i); // always points directly to row 0 (depth 1!)
            bomb_depth1.extend_from_slice(&1000u16.to_le_bytes());
            bomb_depth1.extend_from_slice(&0u16.to_le_bytes());
        }
        let err = decode_payload_refs(&bomb_depth1, 31, 64 * 1024).unwrap_err();
        assert!(matches!(err, StatamError::BoundsExceeded { .. }));

        // 2. Chain depth > 15 (1 keyframe + 16 consecutive back=1 deltas -> depth 16).
        let mut deep = Vec::new();
        deep.push(0); // has_nulls = 0
        deep.push(TAG_KEYFRAME);
        deep.extend_from_slice(&2u16.to_le_bytes());
        deep.extend_from_slice(&0u16.to_le_bytes());
        deep.extend_from_slice(&0u32.to_le_bytes());
        deep.extend_from_slice(&0u16.to_le_bytes());
        deep.extend_from_slice(&1u32.to_le_bytes());
        for _ in 0..=MAX_REF_CHAIN_DEPTH {
            deep.push(TAG_DELTA);
            deep.push(1);
            deep.extend_from_slice(&2u16.to_le_bytes());
            deep.extend_from_slice(&0u16.to_le_bytes());
        }
        let err = decode_payload_refs(&deep, MAX_REF_CHAIN_DEPTH + 2, MAX_BATCH_DECODED_BYTES)
            .unwrap_err();
        assert!(matches!(err, StatamError::BoundsExceeded { .. }));

        // 3. Self-cycle (back == 0) rejected cleanly.
        let mut cycle = Vec::new();
        cycle.push(0);
        cycle.push(TAG_DELTA);
        cycle.push(0); // back = 0 (self-cycle!)
        cycle.extend_from_slice(&0u16.to_le_bytes());
        cycle.extend_from_slice(&0u16.to_le_bytes());
        assert!(matches!(
            decode_payload_refs(&cycle, 1, MAX_BATCH_DECODED_BYTES),
            Err(StatamError::Corrupt { .. })
        ));

        // 4. keep_prefix + n_new > MAX_CHUNKS_PER_ROW rejected before allocating.
        let mut over_chunks = Vec::new();
        over_chunks.push(0);
        over_chunks.push(TAG_KEYFRAME);
        over_chunks.extend_from_slice(&40_000u16.to_le_bytes());
        for i in 0..40_000u32 {
            over_chunks.extend_from_slice(&0u16.to_le_bytes());
            over_chunks.extend_from_slice(&i.to_le_bytes());
        }
        over_chunks.push(TAG_DELTA);
        over_chunks.push(1);
        over_chunks.extend_from_slice(&40_000u16.to_le_bytes());
        over_chunks.extend_from_slice(&30_000u16.to_le_bytes()); // 70,000 > 65,536
        assert!(matches!(
            decode_payload_refs(&over_chunks, 2, MAX_BATCH_DECODED_BYTES),
            Err(StatamError::BoundsExceeded { .. })
        ));

        // 5. Huge row_count in tiny buffer rejected before Vec::with_capacity.
        assert!(matches!(
            decode_payload_refs(&[0, TAG_INLINE, 0, 0], 1_000_000, MAX_BATCH_DECODED_BYTES),
            Err(StatamError::Corrupt { .. })
        ));

        // 6. All-null bitmap bomb with huge row_count or exceeding effective_max rejected before alloc.
        let zero_bm = vec![0u8; 1 + 10_000usize.div_ceil(8)];
        let mut all_null_bomb = vec![1u8];
        all_null_bomb.extend_from_slice(&zero_bm[1..]);
        assert!(matches!(
            decode_payload_refs(&all_null_bomb, 10_000, 64 * 1024),
            Err(StatamError::BoundsExceeded { .. })
        ));
        assert!(matches!(
            decode_payload_refs(
                &[1, 0, 0, 0],
                MAX_BLOCK_RAW_BYTES / 32 + 1,
                MAX_BATCH_DECODED_BYTES
            ),
            Err(StatamError::BoundsExceeded { .. })
        ));
    }

    #[test]
    fn payload_refs_l0_direct_and_utf8_streaming_roundtrip() {
        let rows = vec![
            PayloadRefRow::Inline(b"tiny".to_vec()),
            PayloadRefRow::Null,
            PayloadRefRow::Chunks(vec![(0xFFFF, 0)]),
            PayloadRefRow::Inline(b"also short".to_vec()),
        ];
        let mut direct = Vec::new();
        encode_payload_refs_l0_direct(&rows, &mut direct).unwrap();
        let decoded = decode_payload_refs(&direct, rows.len(), MAX_BATCH_DECODED_BYTES).unwrap();
        assert_eq!(decoded, rows);

        let long = "Z".repeat(300);
        let u = cetra_column::Utf8Data::try_from_slices(["tiny", "", long.as_str(), "also short"])
            .unwrap();
        let nulls = Bitmap::from_bools(&[true, false, true, true]);
        let mut streamed = Vec::new();
        let mut next_cid = 0u32;
        let nc = encode_payload_refs_l0_from_bytes(
            u.offsets(),
            u.bytes(),
            Some(&nulls),
            0,
            4,
            256,
            0xFFFF,
            |_slice, cids| {
                let cid = next_cid;
                next_cid += 1;
                cids.push(cid);
                Ok(())
            },
            &mut streamed,
        )
        .unwrap();
        assert_eq!(nc, 1);
        assert_eq!(streamed, direct);
    }

    #[test]
    fn decode_payload_refs_selected_matches_full_across_patterns() {
        // Build >650 rows mixing Null, Inline, Keyframe, and 15-hop TAG_DELTA chains (with
        // maximal back=32 hops constructed directly on wire too!).
        let mut rows = Vec::new();
        for i in 0..700u32 {
            if i % 17 == 0 {
                rows.push(PayloadRefRow::Null);
            } else if i % 7 == 0 {
                rows.push(PayloadRefRow::Inline(format!("inline-{i}").into_bytes()));
            } else {
                let base = (i / 25) * 25;
                let len = ((i - base) + 2) as usize;
                let refs: Vec<(u16, u32)> =
                    (0..len).map(|k| (0u16, base * 10 + k as u32)).collect();
                rows.push(PayloadRefRow::Chunks(refs));
            }
        }

        let mut encoded = Vec::new();
        encode_payload_refs(&rows, &mut encoded).unwrap();
        let full = decode_payload_refs(&encoded, rows.len(), MAX_BATCH_DECODED_BYTES).unwrap();
        assert_eq!(full, rows);

        // 1. Empty mask
        let empty = Bitmap::new_zeroed(rows.len());
        assert!(
            decode_payload_refs_selected(&encoded, rows.len(), &empty, MAX_BATCH_DECODED_BYTES)
                .unwrap()
                .is_empty()
        );

        // 2. Single row (including null, inline, and deep delta row > 500) with ZERO fallback!
        for target in [0usize, 7, 17, 512, 699] {
            let mut m = Bitmap::new_zeroed(rows.len());
            let _ = m.set(target, true);
            let (got, used_fallback) = decode_payload_refs_selected_with_fallback_flag(
                &encoded,
                rows.len(),
                &m,
                MAX_BATCH_DECODED_BYTES,
            )
            .unwrap();
            assert!(!used_fallback, "unexpected fallback at target={target}");
            assert_eq!(got, vec![(target, full[target].clone())]);
        }

        // 2b. > 480 consecutive nulls between TAG_KEYFRAME and active TAG_DELTA(back = 1),
        //     verifying non-null ordinal windowing takes the sparse path with ZERO fallback!
        let mut null_gap_rows = Vec::with_capacity(550);
        null_gap_rows.push(PayloadRefRow::Chunks(vec![(0, 10), (0, 11), (0, 12)]));
        for _ in 0..520 {
            null_gap_rows.push(PayloadRefRow::Null);
        }
        null_gap_rows.push(PayloadRefRow::Chunks(vec![
            (0, 10),
            (0, 11),
            (0, 12),
            (0, 13),
        ]));
        let mut null_gap_enc = Vec::new();
        encode_payload_refs(&null_gap_rows, &mut null_gap_enc).unwrap();
        let mut null_gap_mask = Bitmap::new_zeroed(null_gap_rows.len());
        let target_after_nulls = 521usize;
        let _ = null_gap_mask.set(target_after_nulls, true);
        let (null_gap_got, null_gap_fb) = decode_payload_refs_selected_with_fallback_flag(
            &null_gap_enc,
            null_gap_rows.len(),
            &null_gap_mask,
            MAX_BATCH_DECODED_BYTES,
        )
        .unwrap();
        assert!(!null_gap_fb);
        assert_eq!(
            null_gap_got,
            vec![(
                target_after_nulls,
                null_gap_rows[target_after_nulls].clone()
            )]
        );

        // 3. Clustered & scattered masks
        let mut clustered = Bitmap::new_zeroed(rows.len());
        for r in 300..310 {
            let _ = clustered.set(r, true);
        }
        let got_clustered =
            decode_payload_refs_selected(&encoded, rows.len(), &clustered, MAX_BATCH_DECODED_BYTES)
                .unwrap();
        let exp_clustered: Vec<(usize, PayloadRefRow)> =
            (300..310).map(|r| (r, full[r].clone())).collect();
        assert_eq!(got_clustered, exp_clustered);

        let mut scattered = Bitmap::new_zeroed(rows.len());
        for r in (5..rows.len()).step_by(97) {
            let _ = scattered.set(r, true);
        }
        let got_scattered =
            decode_payload_refs_selected(&encoded, rows.len(), &scattered, MAX_BATCH_DECODED_BYTES)
                .unwrap();
        let exp_scattered: Vec<(usize, PayloadRefRow)> = scattered
            .iter_ones()
            .map(|r| (r, full[r].clone()))
            .collect();
        assert_eq!(got_scattered, exp_scattered);

        // 4. Explicit 15-hop TAG_DELTA chain with back=32 hops spanning 520+ rows!
        //    Row 0 = Keyframe, rows 1..520: every 32nd row deltas from 32 non-null rows back,
        //    reaching depth=15 at row 480 and depth=15 again after a reset at row 512.
        let total = 560usize;
        let mut wire = Vec::new();
        wire.push(0); // has_nulls = 0
        wire.push(TAG_KEYFRAME);
        wire.extend_from_slice(&2u16.to_le_bytes());
        wire.extend_from_slice(&0u16.to_le_bytes());
        wire.extend_from_slice(&10u32.to_le_bytes());
        wire.extend_from_slice(&0u16.to_le_bytes());
        wire.extend_from_slice(&11u32.to_le_bytes());
        for r in 1..total {
            if r <= 15 * 32 && r % 32 == 0 {
                // back=32 chain: depth = r / 32 (1..=15)
                wire.push(TAG_DELTA);
                wire.push(32);
                let keep = 2 + (r / 32 - 1);
                wire.extend_from_slice(&(keep as u16).to_le_bytes());
                wire.extend_from_slice(&1u16.to_le_bytes());
                encode_varint_u64(zigzag_encode_i64(0), &mut wire);
                encode_varint_u64(zigzag_encode_i64(1), &mut wire);
            } else {
                wire.push(TAG_KEYFRAME);
                wire.extend_from_slice(&2u16.to_le_bytes());
                wire.extend_from_slice(&0u16.to_le_bytes());
                wire.extend_from_slice(&(1000 + r as u32).to_le_bytes());
                wire.extend_from_slice(&0u16.to_le_bytes());
                wire.extend_from_slice(&(2000 + r as u32).to_le_bytes());
            }
        }
        let chain_full = decode_payload_refs(&wire, total, MAX_BATCH_DECODED_BYTES).unwrap();
        let mut chain_mask = Bitmap::new_zeroed(total);
        let _ = chain_mask.set(15 * 32, true); // exact 480-row boundary!
        let _ = chain_mask.set(550, true);
        let (chain_sel, chain_fb) = decode_payload_refs_selected_with_fallback_flag(
            &wire,
            total,
            &chain_mask,
            MAX_BATCH_DECODED_BYTES,
        )
        .unwrap();
        assert!(!chain_fb);
        assert_eq!(
            chain_sel,
            vec![
                (15 * 32, chain_full[15 * 32].clone()),
                (550, chain_full[550].clone()),
            ]
        );

        // 5. L0Fast direct keyframes
        let l0_rows = vec![
            PayloadRefRow::Inline(b"a".to_vec()),
            PayloadRefRow::Null,
            PayloadRefRow::Chunks(vec![(0xFFFF, 0), (0xFFFF, 1)]),
            PayloadRefRow::Inline(b"b".to_vec()),
            PayloadRefRow::Chunks(vec![(0xFFFF, 2)]),
            PayloadRefRow::Null,
            PayloadRefRow::Inline(b"c".to_vec()),
            PayloadRefRow::Inline(b"d".to_vec()),
            PayloadRefRow::Inline(b"e".to_vec()),
            PayloadRefRow::Inline(b"f".to_vec()),
        ];
        let mut l0_enc = Vec::new();
        encode_payload_refs_l0_direct(&l0_rows, &mut l0_enc).unwrap();
        let mut l0_mask = Bitmap::new_zeroed(l0_rows.len());
        let _ = l0_mask.set(2, true);
        assert_eq!(
            decode_payload_refs_selected(&l0_enc, l0_rows.len(), &l0_mask, MAX_BATCH_DECODED_BYTES)
                .unwrap(),
            vec![(2, l0_rows[2].clone())]
        );

        // 6. Corrupt / truncated inputs rejected even on empty/sparse masks
        let mut truncated = encoded.clone();
        truncated.pop();
        assert!(matches!(
            decode_payload_refs_selected(&truncated, rows.len(), &empty, MAX_BATCH_DECODED_BYTES),
            Err(StatamError::Corrupt { .. })
        ));
        let mut trailing = encoded.clone();
        trailing.push(0xAA);
        assert!(matches!(
            decode_payload_refs_selected(&trailing, rows.len(), &empty, MAX_BATCH_DECODED_BYTES),
            Err(StatamError::Corrupt { .. })
        ));
        let wrong_mask = Bitmap::new_zeroed(rows.len() + 1);
        assert!(matches!(
            decode_payload_refs_selected(
                &encoded,
                rows.len(),
                &wrong_mask,
                MAX_BATCH_DECODED_BYTES
            ),
            Err(StatamError::Corrupt { .. })
        ));

        // 7. Skipped TAG_DELTA rows with out-of-range pack_idx/chunk_id deltas are rejected
        //    even on an empty mask (both keep_prefix == 0 exact and keep_prefix > 0 unconditional).
        let mut bad_skip_kp0 = Vec::new();
        bad_skip_kp0.push(0); // has_nulls = 0
        bad_skip_kp0.push(TAG_KEYFRAME);
        bad_skip_kp0.extend_from_slice(&1u16.to_le_bytes());
        bad_skip_kp0.extend_from_slice(&0u16.to_le_bytes());
        bad_skip_kp0.extend_from_slice(&0u32.to_le_bytes());
        bad_skip_kp0.push(TAG_DELTA);
        bad_skip_kp0.push(1);
        bad_skip_kp0.extend_from_slice(&0u16.to_le_bytes()); // keep_prefix = 0
        bad_skip_kp0.extend_from_slice(&1u16.to_le_bytes());
        encode_varint_u64(zigzag_encode_i64(-1), &mut bad_skip_kp0); // negative pack_idx from 0!
        encode_varint_u64(zigzag_encode_i64(0), &mut bad_skip_kp0);
        let empty_2 = Bitmap::new_zeroed(2);
        assert!(matches!(
            decode_payload_refs_selected(&bad_skip_kp0, 2, &empty_2, MAX_BATCH_DECODED_BYTES),
            Err(StatamError::Corrupt { .. })
        ));

        let mut bad_skip_kp1 = Vec::new();
        bad_skip_kp1.push(0); // has_nulls = 0
        bad_skip_kp1.push(TAG_KEYFRAME);
        bad_skip_kp1.extend_from_slice(&1u16.to_le_bytes());
        bad_skip_kp1.extend_from_slice(&0u16.to_le_bytes());
        bad_skip_kp1.extend_from_slice(&0u32.to_le_bytes());
        bad_skip_kp1.push(TAG_DELTA);
        bad_skip_kp1.push(1);
        bad_skip_kp1.extend_from_slice(&1u16.to_le_bytes()); // keep_prefix = 1
        bad_skip_kp1.extend_from_slice(&1u16.to_le_bytes());
        encode_varint_u64(
            zigzag_encode_i64(i64::from(u16::MAX) + 1),
            &mut bad_skip_kp1,
        );
        encode_varint_u64(zigzag_encode_i64(0), &mut bad_skip_kp1);
        assert!(matches!(
            decode_payload_refs_selected(&bad_skip_kp1, 2, &empty_2, MAX_BATCH_DECODED_BYTES),
            Err(StatamError::Corrupt { .. })
        ));
    }

    #[test]
    fn decode_payload_refs_flat_matches_decode_payload_refs_bit_for_bit() {
        let mut rows = Vec::with_capacity(600);
        for i in 0..600usize {
            match i % 7 {
                0 => rows.push(PayloadRefRow::Null),
                1 => rows.push(PayloadRefRow::Inline(
                    format!("inline-row-{i}").into_bytes(),
                )),
                _ => {
                    let base = (i / 10) as u32;
                    let pidx = if i % 3 == 0 { 1u16 } else { 0u16 };
                    let mut chunks = vec![(pidx, base), (pidx, base + 1), (pidx, base + 2)];
                    if i % 5 == 0 {
                        chunks.push((pidx, i as u32));
                    }
                    rows.push(PayloadRefRow::Chunks(chunks));
                }
            }
        }
        let mut encoded = Vec::new();
        encode_payload_refs(&rows, &mut encoded).unwrap();
        let expected = decode_payload_refs(&encoded, rows.len(), MAX_BATCH_DECODED_BYTES).unwrap();
        let flat = decode_payload_refs_flat(&encoded, rows.len(), MAX_BATCH_DECODED_BYTES).unwrap();
        assert_eq!(flat.len(), expected.len());
        assert_eq!(flat.to_rows(), expected);

        // Also test explicit 15-hop TAG_DELTA chain with ring-buffer wraparound (>32 rows) and L0 direct.
        let mut chain_rows = Vec::new();
        let mut cur = vec![(0u16, 10u32), (0, 11)];
        for step in 0..80u32 {
            if step % 15 == 0 {
                cur = vec![(0, 100 + step), (0, 101 + step)];
            } else {
                cur.push((0, 200 + step));
            }
            if step % 9 == 4 {
                chain_rows.push(PayloadRefRow::Null);
            } else if step % 9 == 8 {
                chain_rows.push(PayloadRefRow::Inline(b"tiny".to_vec()));
            } else {
                chain_rows.push(PayloadRefRow::Chunks(cur.clone()));
            }
        }
        let mut chain_enc = Vec::new();
        encode_payload_refs(&chain_rows, &mut chain_enc).unwrap();
        let chain_expected =
            decode_payload_refs(&chain_enc, chain_rows.len(), MAX_BATCH_DECODED_BYTES).unwrap();
        let chain_flat =
            decode_payload_refs_flat(&chain_enc, chain_rows.len(), MAX_BATCH_DECODED_BYTES)
                .unwrap();
        assert_eq!(chain_flat.to_rows(), chain_expected);

        let mut l0_enc = Vec::new();
        encode_payload_refs_l0_direct(&rows, &mut l0_enc).unwrap();
        let l0_expected =
            decode_payload_refs(&l0_enc, rows.len(), MAX_BATCH_DECODED_BYTES).unwrap();
        let l0_flat =
            decode_payload_refs_flat(&l0_enc, rows.len(), MAX_BATCH_DECODED_BYTES).unwrap();
        assert_eq!(l0_flat.to_rows(), l0_expected);

        // Bounds & corruption parity.
        let truncated = &encoded[..encoded.len() - 1];
        assert!(decode_payload_refs_flat(truncated, rows.len(), MAX_BATCH_DECODED_BYTES).is_err());
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(decode_payload_refs_flat(&trailing, rows.len(), MAX_BATCH_DECODED_BYTES).is_err());
    }

    #[test]
    fn scan_payload_refs_inline_rejects_multi_chunk_row_without_edge_table() {
        // Keyframe 2-chunk row referencing pack 0 with pack_has_edges[0] == false.
        let kf_rows = vec![PayloadRefRow::Chunks(vec![(0, 0), (0, 1)])];
        let mut kf_enc = Vec::new();
        encode_payload_refs(&kf_rows, &mut kf_enc).unwrap();
        let mask1 = Bitmap::new_ones(1);
        let err = scan_payload_refs_inline_substring(&kf_enc, 1, &mask1, &[4], &[false], |_| false)
            .unwrap_err();
        assert!(matches!(
            err,
            StatamError::Corrupt {
                reason: "multi-chunk row references pack without edge table",
            }
        ));

        // Delta row extending a 1-chunk parent to 2 chunks in pack 0 where !pack_has_edges[0].
        let delta_rows = vec![
            PayloadRefRow::Chunks(vec![(0, 0)]),
            PayloadRefRow::Chunks(vec![(0, 0), (0, 1)]),
        ];
        let mut delta_enc = Vec::new();
        encode_payload_refs(&delta_rows, &mut delta_enc).unwrap();
        let mask2 = Bitmap::new_ones(2);
        let err_delta =
            scan_payload_refs_inline_substring(&delta_enc, 2, &mask2, &[4], &[false], |_| false)
                .unwrap_err();
        assert!(matches!(
            err_delta,
            StatamError::Corrupt {
                reason: "multi-chunk row references pack without edge table",
            }
        ));

        // Single-chunk rows succeed when !pack_has_edges[0].
        let ok_rows = vec![
            PayloadRefRow::Chunks(vec![(0, 0)]),
            PayloadRefRow::Inline(b"hello needle".to_vec()),
        ];
        let mut ok_enc = Vec::new();
        encode_payload_refs(&ok_rows, &mut ok_enc).unwrap();
        let bm = scan_payload_refs_inline_substring(&ok_enc, 2, &mask2, &[4], &[false], |b| {
            b.windows(6).any(|w| w == b"needle")
        })
        .unwrap();
        assert_eq!(bm.iter_ones().collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn m7_m19_rejects_mixed_pack_indices_across_all_decoders_and_bad_mask_len() {
        // 1. Keyframe mixing (0, 1) + (1, 2) and (0xFFFF, 0) + (0, 1).
        for pair in [
            ((0u16, 1u32), (1u16, 2u32)),
            ((0xFFFFu16, 0u32), (0u16, 1u32)),
        ] {
            let mut enc = vec![0u8]; // has_nulls = 0
            enc.push(TAG_KEYFRAME);
            enc.extend_from_slice(&2u16.to_le_bytes());
            enc.extend_from_slice(&pair.0.0.to_le_bytes());
            enc.extend_from_slice(&pair.0.1.to_le_bytes());
            enc.extend_from_slice(&pair.1.0.to_le_bytes());
            enc.extend_from_slice(&pair.1.1.to_le_bytes());

            let all_active = Bitmap::new_ones(1);
            let none_active = Bitmap::new_zeroed(1);
            assert!(matches!(
                decode_payload_refs(&enc, 1, MAX_BATCH_DECODED_BYTES),
                Err(StatamError::Corrupt { .. })
            ));
            assert!(matches!(
                decode_payload_refs_flat(&enc, 1, MAX_BATCH_DECODED_BYTES),
                Err(StatamError::Corrupt { .. })
            ));
            assert!(matches!(
                decode_payload_refs_selected(&enc, 1, &all_active, MAX_BATCH_DECODED_BYTES),
                Err(StatamError::Corrupt { .. })
            ));
            assert!(matches!(
                decode_payload_refs_selected(&enc, 1, &none_active, MAX_BATCH_DECODED_BYTES),
                Err(StatamError::Corrupt { .. })
            ));
            assert!(matches!(
                scan_payload_refs_inline_substring(
                    &enc,
                    1,
                    &all_active,
                    &[10, 10],
                    &[true, true],
                    |_| false,
                ),
                Err(StatamError::Corrupt { .. })
            ));
        }

        // 2. Delta with keep_prefix = 1 changing pack_idx (p_delta = +1), both active and skipped.
        let mut enc_delta_kp1 = vec![0u8];
        enc_delta_kp1.push(TAG_KEYFRAME);
        enc_delta_kp1.extend_from_slice(&1u16.to_le_bytes());
        enc_delta_kp1.extend_from_slice(&0u16.to_le_bytes());
        enc_delta_kp1.extend_from_slice(&5u32.to_le_bytes());
        enc_delta_kp1.push(TAG_DELTA);
        enc_delta_kp1.push(1); // back = 1
        enc_delta_kp1.extend_from_slice(&1u16.to_le_bytes()); // keep_prefix = 1
        enc_delta_kp1.extend_from_slice(&1u16.to_le_bytes()); // n_new = 1
        encode_varint_u64(zigzag_encode_i64(1), &mut enc_delta_kp1); // p_delta = +1 -> pack 1
        encode_varint_u64(zigzag_encode_i64(1), &mut enc_delta_kp1);

        let all2 = Bitmap::new_ones(2);
        let none2 = Bitmap::new_zeroed(2);
        assert!(matches!(
            decode_payload_refs(&enc_delta_kp1, 2, MAX_BATCH_DECODED_BYTES),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            decode_payload_refs_flat(&enc_delta_kp1, 2, MAX_BATCH_DECODED_BYTES),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            decode_payload_refs_selected(&enc_delta_kp1, 2, &all2, MAX_BATCH_DECODED_BYTES),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            decode_payload_refs_selected(&enc_delta_kp1, 2, &none2, MAX_BATCH_DECODED_BYTES),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            scan_payload_refs_inline_substring(
                &enc_delta_kp1,
                2,
                &all2,
                &[10, 10],
                &[true, true],
                |_| false,
            ),
            Err(StatamError::Corrupt { .. })
        ));

        // 3. Delta with keep_prefix = 0, n_new = 2 where second entry changes pack_idx.
        let mut enc_delta_kp0 = vec![0u8];
        enc_delta_kp0.push(TAG_KEYFRAME);
        enc_delta_kp0.extend_from_slice(&1u16.to_le_bytes());
        enc_delta_kp0.extend_from_slice(&0u16.to_le_bytes());
        enc_delta_kp0.extend_from_slice(&5u32.to_le_bytes());
        enc_delta_kp0.push(TAG_DELTA);
        enc_delta_kp0.push(1);
        enc_delta_kp0.extend_from_slice(&0u16.to_le_bytes()); // keep_prefix = 0
        enc_delta_kp0.extend_from_slice(&2u16.to_le_bytes()); // n_new = 2
        encode_varint_u64(zigzag_encode_i64(0), &mut enc_delta_kp0); // pack 0
        encode_varint_u64(zigzag_encode_i64(1), &mut enc_delta_kp0);
        encode_varint_u64(zigzag_encode_i64(1), &mut enc_delta_kp0); // pack 1 (mixed!)
        encode_varint_u64(zigzag_encode_i64(1), &mut enc_delta_kp0);

        assert!(matches!(
            decode_payload_refs(&enc_delta_kp0, 2, MAX_BATCH_DECODED_BYTES),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            decode_payload_refs_flat(&enc_delta_kp0, 2, MAX_BATCH_DECODED_BYTES),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            decode_payload_refs_selected(&enc_delta_kp0, 2, &none2, MAX_BATCH_DECODED_BYTES),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            scan_payload_refs_inline_substring(
                &enc_delta_kp0,
                2,
                &all2,
                &[10, 10],
                &[true, true],
                |_| false,
            ),
            Err(StatamError::Corrupt { .. })
        ));

        // 4. m19: mismatched active_mask.len() returns Corrupt.
        let mut valid = Vec::new();
        encode_payload_refs(&[PayloadRefRow::Inline(b"ok".to_vec())], &mut valid).unwrap();
        let wrong_mask = Bitmap::new_ones(2);
        assert!(matches!(
            scan_payload_refs_inline_substring(&valid, 1, &wrong_mask, &[], &[], |_| false),
            Err(StatamError::Corrupt { .. })
        ));
    }
}
