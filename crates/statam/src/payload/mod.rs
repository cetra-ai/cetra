//! Wide payload chunking, `.stpk` chunk packs, tail-sharing `PayloadRefs`, and exact substring search.

pub mod chunker;
pub mod pack;
pub mod search;

use std::collections::VecDeque;
use std::fmt;

use cetra_column::{Bitmap, Utf8Data};

use crate::StatamError;
use crate::format::{
    MAX_BATCH_DECODED_BYTES, MAX_BLOCK_RAW_BYTES, MAX_CHUNKS_PER_ROW, MAX_FRAME_RAW_BYTES,
    MAX_REF_CHAIN_DEPTH, MAX_ROWS_PER_FILE,
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

/// Zero-copy `L0Fast` streaming encoder directly from a `Utf8Data` slice into `out`.
///
/// Returns the mini-block's `null_count`. When all `mb_len` rows are null, `out` is left empty.
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_payload_refs_l0_from_utf8<F>(
    u: &Utf8Data,
    col_nulls: Option<&Bitmap>,
    row_off: usize,
    mb_len: usize,
    inline_threshold: usize,
    l0_pack_idx: u16,
    mut add_chunk: F,
    out: &mut Vec<u8>,
) -> Result<u16, StatamError>
where
    F: FnMut(&[u8]) -> Result<u32, StatamError>,
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

    let offsets = u.offsets();
    let all_bytes = u.bytes();
    for r in row_off..row_off + mb_len {
        if col_nulls.is_some_and(|bm| !bm.get(r)) {
            continue;
        }
        let s = offsets[r] as usize;
        let e = offsets[r + 1] as usize;
        let bytes = &all_bytes[s..e];
        if bytes.len() <= inline_threshold {
            let len = u16::try_from(bytes.len()).map_err(|_| StatamError::BoundsExceeded {
                reason: "inline payload length exceeds u16::MAX",
            })?;
            out.push(TAG_INLINE);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(bytes);
        } else {
            let n_chunks_usize = bytes.len().div_ceil(MAX_FRAME_RAW_BYTES);
            if n_chunks_usize > MAX_CHUNKS_PER_ROW {
                return Err(StatamError::BoundsExceeded {
                    reason: "row chunks exceed MAX_CHUNKS_PER_ROW",
                });
            }
            let n_chunks =
                u16::try_from(n_chunks_usize).map_err(|_| StatamError::BoundsExceeded {
                    reason: "keyframe chunk count exceeds u16::MAX",
                })?;
            out.push(TAG_KEYFRAME);
            out.extend_from_slice(&n_chunks.to_le_bytes());
            for slice in bytes.chunks(MAX_FRAME_RAW_BYTES) {
                let cid = add_chunk(slice)?;
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
                for _ in 0..n_chunks {
                    let pack_idx = cur.read_u16()?;
                    let chunk_id = cur.read_u32()?;
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
                for _ in 0..n_new {
                    let p_delta = zigzag_decode_i64(cur.read_varint_u64()?);
                    let c_delta = zigzag_decode_i64(cur.read_varint_u64()?);
                    let next_pack = i64::from(prev.0)
                        .checked_add(p_delta)
                        .and_then(|v| u16::try_from(v).ok())
                        .ok_or(StatamError::Corrupt {
                            reason: "payload ref pack_idx delta out of u16 range",
                        })?;
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
        let u = Utf8Data::try_from_slices(["tiny", "", long.as_str(), "also short"]).unwrap();
        let nulls = Bitmap::from_bools(&[true, false, true, true]);
        let mut streamed = Vec::new();
        let mut next_cid = 0u32;
        let nc = encode_payload_refs_l0_from_utf8(
            &u,
            Some(&nulls),
            0,
            4,
            256,
            0xFFFF,
            |_slice| {
                let cid = next_cid;
                next_cid += 1;
                Ok(cid)
            },
            &mut streamed,
        )
        .unwrap();
        assert_eq!(nc, 1);
        assert_eq!(streamed, direct);
    }
}
