//! SIGMOD-2024 ALP (Adaptive Lossless floating-Point) bit-exact `f64` codec.
//!
//! Encodes `f64` values by picking `(exp, fac)` in `0..=18` that maximizes
//! exact decimal-scaled integer round-trips, bitpacking the scaled `i64`s via
//! FastLanes `ForBitpack`, and patching non-matching values (`NaN` with exact
//! payload bits, `±inf`, `-0.0`, subnormals, high-precision floats) via a
//! sparse `(pos: u16, raw_bits: u64)` exception list.

use std::cmp::Ordering;

use cetra_column::Bitmap;

use crate::simd::bitpack::{min_bits_u64, pack_u64, unpack_u64, unpack_u64_at};
use crate::{MAX_BLOCK_RAW_BYTES, StatamError};

/// Maximum `exp` / `fac` index in ALP (`0..=18`).
pub const ALP_MAX_EXP: u8 = 18;

/// Bit flag in the stored `exp` header byte indicating exact division by `10^exp`
/// (`(scaled * 10^fac) / 10^exp`) instead of multiplication by `10^-exp`.
const ALP_DIV_MODE_FLAG: u8 = 0x80;

/// Maximum exact integer magnitude representable cleanly in ALP (`2^52`).
const ALP_MAX_SAFE_INT: f64 = (1i64 << 52) as f64;

/// Maximum sample size used to select `(exp, fac)`.
const ALP_SAMPLE_LIMIT: usize = 32;

/// Powers of 10 (`10^0 ..= 10^18`).
const POW10: [f64; 19] = [
    1.0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18,
];

/// Negative powers of 10 (`10^-0 ..= 10^-18`).
const FRAC10: [f64; 19] = [
    1.0, 1e-1, 1e-2, 1e-3, 1e-4, 1e-5, 1e-6, 1e-7, 1e-8, 1e-9, 1e-10, 1e-11, 1e-12, 1e-13, 1e-14,
    1e-15, 1e-16, 1e-17, 1e-18,
];

#[inline]
fn try_scale_exact(v: f64, exp: u8, fac: u8, div_mode: bool) -> Option<i64> {
    if !v.is_finite() {
        return None;
    }
    let prod = v * POW10[usize::from(exp)] * FRAC10[usize::from(fac)];
    if !prod.is_finite() {
        return None;
    }
    let rounded = prod.round();
    if !(-ALP_MAX_SAFE_INT..=ALP_MAX_SAFE_INT).contains(&rounded) {
        return None;
    }
    let scaled = rounded as i64;
    let recon = unscale(scaled, exp, fac, div_mode);
    if recon.to_bits() == v.to_bits() {
        Some(scaled)
    } else {
        None
    }
}

#[inline]
fn unscale(scaled: i64, exp: u8, fac: u8, div_mode: bool) -> f64 {
    if div_mode {
        ((scaled as f64) * POW10[usize::from(fac)]) / POW10[usize::from(exp)]
    } else {
        (scaled as f64) * POW10[usize::from(fac)] * FRAC10[usize::from(exp)]
    }
}

fn pick_best_params(values: &[f64], nulls: Option<&Bitmap>) -> (u8, u8, bool) {
    let mut samples = [0.0f64; ALP_SAMPLE_LIMIT];
    let mut sample_count = 0usize;

    let step = (values.len() / ALP_SAMPLE_LIMIT).max(1);
    let mut idx = 0usize;
    while idx < values.len() && sample_count < ALP_SAMPLE_LIMIT {
        if nulls.is_none_or(|bm| bm.get(idx)) && values[idx].is_finite() {
            samples[sample_count] = values[idx];
            sample_count += 1;
        }
        idx += step;
    }
    if sample_count == 0 {
        for (i, &v) in values.iter().enumerate() {
            if nulls.is_none_or(|bm| bm.get(i)) && v.is_finite() {
                samples[sample_count] = v;
                sample_count += 1;
                if sample_count == ALP_SAMPLE_LIMIT {
                    break;
                }
            }
        }
    }
    if sample_count == 0 {
        return (0, 0, false);
    }

    let sample_slice = &samples[..sample_count];
    let mut best_hits = 0usize;
    let mut best_exp = 0u8;
    let mut best_fac = 0u8;
    let mut best_div = false;

    // Fast path: test common decimal scaling `(exp, fac = 0)` for `exp = 0..=10` first,
    // preferring multiplication mode when equal and falling back to exact `/ POW10[exp]`.
    for exp in 0..=10u8 {
        let mut hits = 0usize;
        for &v in sample_slice {
            if try_scale_exact(v, exp, 0, false).is_some() {
                hits += 1;
            }
        }
        if hits > best_hits || (hits == best_hits && hits > 0 && exp < best_exp) {
            best_hits = hits;
            best_exp = exp;
            best_fac = 0;
            best_div = false;
        }
        if best_hits == sample_count {
            return (best_exp, 0, false);
        }
    }

    for exp in 1..=10u8 {
        let mut hits = 0usize;
        for &v in sample_slice {
            if try_scale_exact(v, exp, 0, true).is_some() {
                hits += 1;
            }
        }
        if hits > best_hits {
            best_hits = hits;
            best_exp = exp;
            best_fac = 0;
            best_div = true;
        }
        if best_hits == sample_count {
            return (best_exp, 0, true);
        }
    }

    for exp in 0..=ALP_MAX_EXP {
        for fac in 0..=exp {
            if fac == 0 && exp <= 10 {
                continue;
            }
            let mut hits = 0usize;
            for &v in sample_slice {
                if try_scale_exact(v, exp, fac, false).is_some() {
                    hits += 1;
                }
            }
            if hits > best_hits || (hits == best_hits && hits > 0 && exp < best_exp) {
                best_hits = hits;
                best_exp = exp;
                best_fac = fac;
                best_div = false;
            }
            if best_hits == sample_count {
                // Found a parameter pair with 100% sample match; prefer smaller exp.
                return (best_exp, best_fac, false);
            }
        }
    }

    (best_exp, best_fac, best_div)
}

/// Encodes an `f64` block body (without leading row null bitmap) using ALP.
///
/// Returns `Ok(None)` if `values` is empty, `values.len() > 65_536`, exception rate
/// exceeds 25%, or the encoded body is not `< 75%` of raw `values.len() * 8`.
pub fn encode_alp_body(
    values: &[f64],
    nulls: Option<&Bitmap>,
) -> Result<Option<Vec<u8>>, StatamError> {
    if values.is_empty() || values.len() > (u16::MAX as usize) + 1 {
        return Ok(None);
    }

    let valid_count = match nulls {
        Some(bm) => bm.count_ones(),
        None => values.len(),
    };
    if valid_count == 0 {
        return Ok(None);
    }

    let (exp, fac, div_mode) = pick_best_params(values, nulls);

    let mut scaled_vals = vec![0i64; values.len()];
    let mut exc_pos: Vec<u16> = Vec::new();
    let mut exc_bits: Vec<u64> = Vec::new();
    let mut first_exact: Option<i64> = None;

    // 25% exception budget over non-null values; size check below rejects unbeneficial blocks.
    let max_exceptions = valid_count / 4;

    for (i, &v) in values.iter().enumerate() {
        if nulls.is_some_and(|bm| !bm.get(i)) {
            continue;
        }
        if let Some(s) = try_scale_exact(v, exp, fac, div_mode) {
            scaled_vals[i] = s;
            if first_exact.is_none() {
                first_exact = Some(s);
            }
        } else {
            if exc_pos.len() >= max_exceptions {
                return Ok(None);
            }
            let pos = u16::try_from(i).map_err(|_| StatamError::BoundsExceeded {
                reason: "alp exception position exceeds u16::MAX",
            })?;
            exc_pos.push(pos);
            exc_bits.push(v.to_bits());
        }
    }

    let filler = first_exact.unwrap_or(0);
    if !exc_pos.is_empty() {
        for &p in &exc_pos {
            scaled_vals[usize::from(p)] = filler;
        }
    }
    if let Some(bm) = nulls {
        for (i, slot) in scaled_vals.iter_mut().enumerate() {
            if !bm.get(i) {
                *slot = filler;
            }
        }
    }

    let min_scaled = scaled_vals.iter().copied().min().unwrap_or(0);
    let mut deltas = Vec::with_capacity(scaled_vals.len());
    for &s in &scaled_vals {
        deltas.push(s.wrapping_sub(min_scaled) as u64);
    }
    let bit_width = min_bits_u64(&deltas);

    let exc_count = u16::try_from(exc_pos.len()).map_err(|_| StatamError::BoundsExceeded {
        reason: "alp exception count exceeds u16::MAX",
    })?;

    // Wire layout:
    //   exp: u8 | fac: u8 | exc_count: u16 | base: i64 | bit_width: u8 |
    //   packed_deltas | [u16; exc_count] | [u64; exc_count]
    let mut out = Vec::with_capacity(13 + exc_pos.len() * 10);
    let exp_byte = if div_mode {
        exp | ALP_DIV_MODE_FLAG
    } else {
        exp
    };
    out.push(exp_byte);
    out.push(fac);
    out.extend_from_slice(&exc_count.to_le_bytes());
    out.extend_from_slice(&min_scaled.to_le_bytes());
    out.push(bit_width);
    pack_u64(&deltas, bit_width, &mut out)?;
    for &pos in &exc_pos {
        out.extend_from_slice(&pos.to_le_bytes());
    }
    for &bits in &exc_bits {
        out.extend_from_slice(&bits.to_le_bytes());
    }

    if out.len().saturating_mul(4) >= values.len().saturating_mul(8 * 3) {
        return Ok(None);
    }

    Ok(Some(out))
}

#[inline]
fn read_u16_le(slice: &[u8], idx: usize) -> u16 {
    let off = idx * 2;
    u16::from_le_bytes([slice[off], slice[off + 1]])
}

#[inline]
fn read_u64_le(slice: &[u8], idx: usize) -> u64 {
    let off = idx * 8;
    u64::from_le_bytes([
        slice[off],
        slice[off + 1],
        slice[off + 2],
        slice[off + 3],
        slice[off + 4],
        slice[off + 5],
        slice[off + 6],
        slice[off + 7],
    ])
}

struct ParsedAlpHeader<'a> {
    div_mode: bool,
    exp: u8,
    fac: u8,
    exc_count: usize,
    base: i64,
    bit_width: u8,
    packed: &'a [u8],
    pos_section: &'a [u8],
    bits_section: &'a [u8],
    exc_strictly_sorted: bool,
}

impl ParsedAlpHeader<'_> {
    #[inline]
    fn lookup_exception(&self, row: usize) -> Option<f64> {
        if self.exc_count == 0 {
            return None;
        }
        let Ok(target) = u16::try_from(row) else {
            return None;
        };
        if self.exc_strictly_sorted {
            let mut lo = 0usize;
            let mut hi = self.exc_count;
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let p = read_u16_le(self.pos_section, mid);
                match p.cmp(&target) {
                    Ordering::Less => lo = mid + 1,
                    Ordering::Greater => hi = mid,
                    Ordering::Equal => {
                        return Some(f64::from_bits(read_u64_le(self.bits_section, mid)));
                    }
                }
            }
            None
        } else {
            // Crafted/fuzzed unsorted or duplicate exc_pos: match `decode_alp_body` last-write-wins.
            for i in (0..self.exc_count).rev() {
                if read_u16_le(self.pos_section, i) == target {
                    return Some(f64::from_bits(read_u64_le(self.bits_section, i)));
                }
            }
            None
        }
    }
}

fn parse_alp_header(src: &[u8], count: usize) -> Result<Option<ParsedAlpHeader<'_>>, StatamError> {
    if count
        .checked_mul(size_of::<f64>())
        .is_none_or(|bytes| bytes > MAX_BLOCK_RAW_BYTES)
    {
        return Err(StatamError::BoundsExceeded {
            reason: "alp decode count exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    if count == 0 {
        if !src.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty alp block",
            });
        }
        return Ok(None);
    }
    if src.len() < 13 {
        return Err(StatamError::Corrupt {
            reason: "truncated alp block header",
        });
    }

    let div_mode = (src[0] & ALP_DIV_MODE_FLAG) != 0;
    let exp = src[0] & !ALP_DIV_MODE_FLAG;
    let fac = src[1];
    if exp > ALP_MAX_EXP || fac > exp || (div_mode && exp == 0) {
        return Err(StatamError::Corrupt {
            reason: "invalid alp exp/fac parameters",
        });
    }
    let exc_count = u16::from_le_bytes([src[2], src[3]]) as usize;
    if exc_count > count {
        return Err(StatamError::Corrupt {
            reason: "alp exception count exceeds row count",
        });
    }
    let base = i64::from_le_bytes([
        src[4], src[5], src[6], src[7], src[8], src[9], src[10], src[11],
    ]);
    let bit_width = src[12];
    if bit_width > 64 {
        return Err(StatamError::Corrupt {
            reason: "invalid alp bit_width",
        });
    }

    let packed_slice = &src[13..];
    let packed_len = crate::simd::bitpack::packed_bytes_len(count, bit_width)?;
    if packed_slice.len() < packed_len {
        return Err(StatamError::Corrupt {
            reason: "truncated alp packed deltas",
        });
    }
    let exc_bytes_needed = exc_count
        .checked_mul(10)
        .ok_or(StatamError::BoundsExceeded {
            reason: "alp exception table size overflow",
        })?;
    let expected_total = 13usize
        .checked_add(packed_len)
        .and_then(|n| n.checked_add(exc_bytes_needed))
        .ok_or(StatamError::BoundsExceeded {
            reason: "alp block size overflow",
        })?;
    if src.len() != expected_total {
        return Err(StatamError::Corrupt {
            reason: "alp exception table length mismatch",
        });
    }

    let exc_slice = &packed_slice[packed_len..];
    let pos_section = &exc_slice[..exc_count * 2];
    let bits_section = &exc_slice[exc_count * 2..];

    let mut exc_strictly_sorted = true;
    let mut prev_pos: Option<u16> = None;
    for i in 0..exc_count {
        let pos = read_u16_le(pos_section, i);
        if usize::from(pos) >= count {
            return Err(StatamError::Corrupt {
                reason: "alp exception position out of bounds",
            });
        }
        if let Some(prev) = prev_pos
            && pos <= prev
        {
            exc_strictly_sorted = false;
        }
        prev_pos = Some(pos);
    }

    Ok(Some(ParsedAlpHeader {
        div_mode,
        exp,
        fac,
        exc_count,
        base,
        bit_width,
        packed: &packed_slice[..packed_len],
        pos_section,
        bits_section,
        exc_strictly_sorted,
    }))
}

/// Validates an ALP block header, packed length, and exception table without allocating.
pub(crate) fn validate_alp_body(src: &[u8], row_count: usize) -> Result<(), StatamError> {
    let _ = parse_alp_header(src, row_count)?;
    Ok(())
}

fn decode_alp_from_header(
    hdr: &ParsedAlpHeader<'_>,
    count: usize,
    nulls: Option<&Bitmap>,
) -> Result<Vec<f64>, StatamError> {
    let mut deltas = Vec::with_capacity(count);
    unpack_u64(hdr.packed, count, hdr.bit_width, &mut deltas)?;

    let mut out = Vec::with_capacity(count);
    for &d in &deltas {
        let scaled = hdr.base.wrapping_add(d as i64);
        out.push(unscale(scaled, hdr.exp, hdr.fac, hdr.div_mode));
    }

    for i in 0..hdr.exc_count {
        let pos = usize::from(read_u16_le(hdr.pos_section, i));
        let bits = read_u64_le(hdr.bits_section, i);
        out[pos] = f64::from_bits(bits);
    }

    if let Some(bm) = nulls {
        for (i, slot) in out.iter_mut().enumerate() {
            if !bm.get(i) {
                *slot = 0.0;
            }
        }
    }

    Ok(out)
}

/// Decodes an ALP-encoded `f64` block body into `count` values.
///
/// Null rows (where `nulls` bit is `0`) are set to canonical `0.0`.
pub fn decode_alp_body(
    src: &[u8],
    count: usize,
    nulls: Option<&Bitmap>,
) -> Result<Vec<f64>, StatamError> {
    if count
        .checked_mul(size_of::<f64>())
        .is_none_or(|bytes| bytes > MAX_BLOCK_RAW_BYTES)
    {
        return Err(StatamError::BoundsExceeded {
            reason: "alp decode count exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    if nulls.is_some_and(|bm| bm.len() != count) {
        return Err(StatamError::Corrupt {
            reason: "alp null bitmap length mismatch",
        });
    }
    let Some(hdr) = parse_alp_header(src, count)? else {
        return Ok(Vec::new());
    };
    decode_alp_from_header(&hdr, count, nulls)
}

/// Sparsely decodes only `indices` of an ALP-encoded `f64` block body in O(k log(exc_count)) time.
///
/// Null rows (where `nulls` bit is `0`) are set to canonical `0.0`.
pub fn decode_alp_take(
    src: &[u8],
    row_count: usize,
    nulls: Option<&Bitmap>,
    indices: &[usize],
) -> Result<Vec<f64>, StatamError> {
    if row_count
        .checked_mul(size_of::<f64>())
        .is_none_or(|bytes| bytes > MAX_BLOCK_RAW_BYTES)
    {
        return Err(StatamError::BoundsExceeded {
            reason: "alp decode count exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    if indices
        .len()
        .checked_mul(size_of::<f64>())
        .is_none_or(|bytes| bytes > MAX_BLOCK_RAW_BYTES)
    {
        return Err(StatamError::BoundsExceeded {
            reason: "alp take indices exceed MAX_BLOCK_RAW_BYTES",
        });
    }
    if nulls.is_some_and(|bm| bm.len() != row_count) {
        return Err(StatamError::Corrupt {
            reason: "alp null bitmap length mismatch",
        });
    }
    for &row in indices {
        if row >= row_count {
            return Err(StatamError::Corrupt {
                reason: "alp take index out of bounds",
            });
        }
    }
    let Some(hdr) = parse_alp_header(src, row_count)? else {
        return Ok(Vec::new());
    };
    if indices.is_empty() {
        return Ok(Vec::new());
    }

    let dense_or_unsorted_heavy = indices.len().saturating_mul(16) >= row_count
        || (!hdr.exc_strictly_sorted && indices.len().saturating_mul(hdr.exc_count) > row_count);
    if dense_or_unsorted_heavy {
        let all = decode_alp_from_header(&hdr, row_count, nulls)?;
        let mut out = Vec::with_capacity(indices.len());
        for &row in indices {
            out.push(all[row]);
        }
        return Ok(out);
    }

    let mut out = Vec::with_capacity(indices.len());
    for &row in indices {
        if nulls.is_some_and(|bm| !bm.get(row)) {
            out.push(0.0);
        } else if let Some(exc_val) = hdr.lookup_exception(row) {
            out.push(exc_val);
        } else {
            let d = unpack_u64_at(hdr.packed, row_count, hdr.bit_width, row)?;
            let scaled = hdr.base.wrapping_add(d as i64);
            out.push(unscale(scaled, hdr.exp, hdr.fac, hdr.div_mode));
        }
    }
    Ok(out)
}

/// Sparsely decodes only `mask`-selected non-null rows of an ALP-encoded `f64` block body.
///
/// Unselected and null rows are set to canonical `0.0`.
pub fn decode_alp_selected(
    src: &[u8],
    row_count: usize,
    nulls: Option<&Bitmap>,
    mask: &Bitmap,
) -> Result<Vec<f64>, StatamError> {
    if mask.len() != row_count {
        return Err(StatamError::Corrupt {
            reason: "selection mask length mismatch",
        });
    }
    if row_count
        .checked_mul(size_of::<f64>())
        .is_none_or(|bytes| bytes > MAX_BLOCK_RAW_BYTES)
    {
        return Err(StatamError::BoundsExceeded {
            reason: "alp decode count exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    if nulls.is_some_and(|bm| bm.len() != row_count) {
        return Err(StatamError::Corrupt {
            reason: "alp null bitmap length mismatch",
        });
    }
    let Some(hdr) = parse_alp_header(src, row_count)? else {
        return Ok(Vec::new());
    };

    let selected_count = mask.count_ones();
    if selected_count == 0 {
        return Ok(vec![0.0f64; row_count]);
    }

    let dense_or_unsorted_heavy = selected_count.saturating_mul(16) >= row_count
        || (!hdr.exc_strictly_sorted && selected_count.saturating_mul(hdr.exc_count) > row_count);
    if dense_or_unsorted_heavy {
        let mut all = decode_alp_from_header(&hdr, row_count, nulls)?;
        if !mask.all_ones() {
            for (row, slot) in all.iter_mut().enumerate() {
                if !mask.get(row) {
                    *slot = 0.0;
                }
            }
        }
        return Ok(all);
    }

    let mut out = vec![0.0f64; row_count];
    for row in mask.iter_ones() {
        if row >= row_count {
            break;
        }
        if nulls.is_some_and(|bm| !bm.get(row)) {
            continue;
        }
        out[row] = if let Some(exc_val) = hdr.lookup_exception(row) {
            exc_val
        } else {
            let d = unpack_u64_at(hdr.packed, row_count, hdr.bit_width, row)?;
            let scaled = hdr.base.wrapping_add(d as i64);
            unscale(scaled, hdr.exp, hdr.fac, hdr.div_mode)
        };
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_alp_rejects_mismatched_length_and_nulls_before_allocating() {
        // 13-byte header claiming 8M rows (64 MiB) with bit_width=1 (needs 1 MiB packed)
        // must fail immediately on truncated payload without allocating.
        let mut hdr = vec![0u8; 13];
        hdr[12] = 1; // bit_width = 1
        assert!(matches!(
            decode_alp_body(&hdr, MAX_BLOCK_RAW_BYTES / 8, None),
            Err(StatamError::Corrupt {
                reason: "truncated alp packed deltas",
            })
        ));

        // Trailing bytes after valid 0-bit-width block must be rejected before allocating.
        let mut with_trailing = vec![0u8; 14];
        with_trailing[12] = 0; // bit_width = 0 -> packed_len = 0, exc_count = 0 -> exact len 13
        assert!(matches!(
            decode_alp_body(&with_trailing, MAX_BLOCK_RAW_BYTES / 8, None),
            Err(StatamError::Corrupt {
                reason: "alp exception table length mismatch",
            })
        ));

        // Mismatched null bitmap length must return Corrupt instead of silently zeroing trailing rows.
        let vals: Vec<f64> = (0..32).map(|i| (i as f64) * 0.25).collect();
        let body = encode_alp_body(&vals, None).unwrap().unwrap();
        let short_nulls = Bitmap::new_ones(16);
        assert!(matches!(
            decode_alp_body(&body, vals.len(), Some(&short_nulls)),
            Err(StatamError::Corrupt {
                reason: "alp null bitmap length mismatch",
            })
        ));
        let long_nulls = Bitmap::new_ones(64);
        assert!(matches!(
            decode_alp_body(&body, vals.len(), Some(&long_nulls)),
            Err(StatamError::Corrupt {
                reason: "alp null bitmap length mismatch",
            })
        ));
    }

    fn assert_alp_sparse_parity(body: &[u8], row_count: usize, nulls: Option<&Bitmap>) {
        let full = decode_alp_body(body, row_count, nulls).unwrap();
        assert_eq!(full.len(), row_count);

        let all_null_indices: Vec<usize> = (0..row_count)
            .filter(|&r| nulls.is_some_and(|bm| !bm.get(r)))
            .take(5)
            .collect();
        let unsorted_dup_indices: Vec<usize> = [
            row_count - 1,
            0,
            3,
            1024.min(row_count - 1),
            3,
            1,
            2,
            0,
            1023.min(row_count - 1),
            11.min(row_count - 1),
        ]
        .into_iter()
        .collect();
        let dense_indices: Vec<usize> = (0..row_count).step_by(3).collect();

        for indices in [
            &[][..],
            &[0][..],
            &all_null_indices[..],
            &unsorted_dup_indices[..],
            &dense_indices[..],
        ] {
            let taken = decode_alp_take(body, row_count, nulls, indices).unwrap();
            assert_eq!(taken.len(), indices.len());
            for (pos, &row) in indices.iter().enumerate() {
                assert_eq!(
                    taken[pos].to_bits(),
                    full[row].to_bits(),
                    "decode_alp_take mismatch at out_pos={pos}, row={row}"
                );
            }
        }

        let empty_mask = Bitmap::new_zeroed(row_count);
        let mut sparse_mask = Bitmap::new_zeroed(row_count);
        for &r in &unsorted_dup_indices {
            let _ = sparse_mask.set(r, true);
        }
        let mut all_null_mask = Bitmap::new_zeroed(row_count);
        for &r in &all_null_indices {
            let _ = all_null_mask.set(r, true);
        }
        let mut dense_mask = Bitmap::new_zeroed(row_count);
        for &r in &dense_indices {
            let _ = dense_mask.set(r, true);
        }
        let all_ones_mask = Bitmap::new_ones(row_count);

        for mask in [
            &empty_mask,
            &all_null_mask,
            &sparse_mask,
            &dense_mask,
            &all_ones_mask,
        ] {
            let sel = decode_alp_selected(body, row_count, nulls, mask).unwrap();
            assert_eq!(sel.len(), row_count);
            for r in 0..row_count {
                let expected_bits = if mask.get(r) {
                    full[r].to_bits()
                } else {
                    0.0f64.to_bits()
                };
                assert_eq!(
                    sel[r].to_bits(),
                    expected_bits,
                    "decode_alp_selected mismatch at row={r}"
                );
            }
        }
    }

    #[test]
    fn decode_alp_take_and_selected_bit_exact_parity() {
        let n = 1300usize;
        let nulls = Bitmap::from_bools(&(0..n).map(|i| i % 9 != 0).collect::<Vec<_>>());

        // 1. Decimal scaling + all IEEE-754 edge exceptions across FastLanes tile & tail.
        let mut vals: Vec<f64> = (0..n).map(|i| ((i % 97) as f64) * 0.000_025).collect();
        vals[0] = -0.0;
        vals[1] = 0.0;
        vals[2] = f64::from_bits(0x7ff8_0000_0000_0001); // quiet NaN payload
        vals[3] = f64::from_bits(0x7ff0_0000_0000_0042); // signaling NaN payload
        vals[4] = f64::from_bits(0xfff8_1234_5678_9abc); // negative NaN payload
        vals[5] = f64::INFINITY;
        vals[6] = f64::NEG_INFINITY;
        vals[7] = f64::MIN_POSITIVE;
        vals[8] = 1e-310; // subnormal
        vals[9] = f64::from_bits(1); // smallest positive subnormal
        vals[10] = f64::from_bits(0x8000_0000_0000_0001); // smallest negative subnormal
        vals[11] = std::f64::consts::PI;
        vals[1023] = -0.0;
        vals[1024] = f64::from_bits(0x7ff8_cafe_babe_0001);
        vals[n - 1] = f64::NEG_INFINITY;

        for opt_nulls in [None, Some(&nulls)] {
            let body = encode_alp_body(&vals, opt_nulls).unwrap().unwrap();
            assert_alp_sparse_parity(&body, n, opt_nulls);
        }

        // 2. ALP_DIV_MODE_FLAG (`div_mode = true`) with exact `/ 100.0` decimals + exceptions.
        let mut div_vals: Vec<f64> = (0..n).map(|i| (((i % 97) + 1) as f64) / 100.0).collect();
        div_vals[2] = -0.0;
        div_vals[1025] = std::f64::consts::E;
        let div_body = encode_alp_body(&div_vals, None).unwrap().unwrap();
        assert_ne!(
            div_body[0] & ALP_DIV_MODE_FLAG,
            0,
            "expected ALP div_mode to be selected"
        );
        assert_alp_sparse_parity(&div_body, n, None);
        assert_alp_sparse_parity(&div_body, n, Some(&nulls));

        // 3. Zero-exception block (every value has 2 decimal digits so all samples pick exp=2).
        let clean_vals: Vec<f64> = (0..n).map(|i| ((i % 128) as f64) * 0.5 + 0.25).collect();
        let clean_body = encode_alp_body(&clean_vals, None).unwrap().unwrap();
        assert_eq!(u16::from_le_bytes([clean_body[2], clean_body[3]]), 0);
        assert_alp_sparse_parity(&clean_body, n, None);
        assert_alp_sparse_parity(&clean_body, n, Some(&nulls));

        // 4. Crafted block with unsorted & duplicate exception positions (last-write-wins)
        //    and an exception on a null row (must still yield +0.0 when masked by nulls).
        let crafted_n = 64usize;
        let mut crafted = Vec::new();
        crafted.push(0); // exp = 0, div_mode = false
        crafted.push(0); // fac = 0
        crafted.extend_from_slice(&4u16.to_le_bytes()); // exc_count = 4
        crafted.extend_from_slice(&7i64.to_le_bytes()); // base = 7
        crafted.push(0); // bit_width = 0 -> packed_len = 0
        // Unsorted + duplicate positions: [10, 5, 10, 0]
        for pos in [10u16, 5, 10, 0] {
            crafted.extend_from_slice(&pos.to_le_bytes());
        }
        for bits in [
            1.5f64.to_bits(),
            (-0.0f64).to_bits(),
            0x7ff8_0000_0000_0099u64, // overwrites pos 10
            f64::INFINITY.to_bits(),  // pos 0 (will be null when nulls[0] == false)
        ] {
            crafted.extend_from_slice(&bits.to_le_bytes());
        }
        let crafted_nulls =
            Bitmap::from_bools(&(0..crafted_n).map(|i| i % 4 != 0).collect::<Vec<_>>());
        assert_alp_sparse_parity(&crafted, crafted_n, None);
        assert_alp_sparse_parity(&crafted, crafted_n, Some(&crafted_nulls));
    }

    #[test]
    fn decode_alp_take_and_selected_reject_corrupt_inputs() {
        let vals: Vec<f64> = (0..64).map(|i| (i as f64) * 0.5 + 0.25).collect();
        let mut body = encode_alp_body(&vals, None).unwrap().unwrap();
        let empty_mask = Bitmap::new_zeroed(vals.len());
        let mut one_mask = Bitmap::new_zeroed(vals.len());
        let _ = one_mask.set(0, true);

        // Trailing bytes on empty block.
        assert!(decode_alp_take(&[0], 0, None, &[]).is_err());
        assert!(decode_alp_selected(&[0], 0, None, &Bitmap::new_zeroed(0)).is_err());

        // Truncated header.
        assert!(decode_alp_take(&body[..12], vals.len(), None, &[0]).is_err());
        assert!(decode_alp_selected(&body[..12], vals.len(), None, &empty_mask).is_err());

        // Invalid exp/fac/div_mode.
        for (exp_byte, fac) in [(19u8, 0u8), (3, 4), (ALP_DIV_MODE_FLAG, 0)] {
            let mut bad = body.clone();
            bad[0] = exp_byte;
            bad[1] = fac;
            assert!(decode_alp_take(&bad, vals.len(), None, &[0]).is_err());
            assert!(decode_alp_selected(&bad, vals.len(), None, &empty_mask).is_err());
        }

        // exc_count > row_count.
        {
            let mut bad = body.clone();
            bad[2..4].copy_from_slice(&(65u16).to_le_bytes());
            assert!(decode_alp_take(&bad, vals.len(), None, &[0]).is_err());
            assert!(decode_alp_selected(&bad, vals.len(), None, &empty_mask).is_err());
        }

        // bit_width > 64.
        {
            let mut bad = body.clone();
            bad[12] = 65;
            assert!(decode_alp_take(&bad, vals.len(), None, &[0]).is_err());
            assert!(decode_alp_selected(&bad, vals.len(), None, &empty_mask).is_err());
        }

        // Truncated payload & trailing bytes.
        assert!(decode_alp_take(&body[..body.len() - 1], vals.len(), None, &[0]).is_err());
        assert!(
            decode_alp_selected(&body[..body.len() - 1], vals.len(), None, &empty_mask).is_err()
        );
        body.push(0);
        assert!(decode_alp_take(&body, vals.len(), None, &[0]).is_err());
        assert!(decode_alp_selected(&body, vals.len(), None, &empty_mask).is_err());
        body.pop();

        // Out-of-bounds exc_pos >= row_count.
        let mut bad_exc = vec![0u8; 13];
        bad_exc[2..4].copy_from_slice(&1u16.to_le_bytes());
        bad_exc.extend_from_slice(&64u16.to_le_bytes()); // pos == count -> OOB
        bad_exc.extend_from_slice(&0u64.to_le_bytes());
        assert!(matches!(
            decode_alp_take(&bad_exc, 64, None, &[0]),
            Err(StatamError::Corrupt {
                reason: "alp exception position out of bounds",
            })
        ));
        assert!(matches!(
            decode_alp_selected(&bad_exc, 64, None, &empty_mask),
            Err(StatamError::Corrupt {
                reason: "alp exception position out of bounds",
            })
        ));

        // Out-of-bounds take index and mismatched nulls/mask lengths.
        assert!(decode_alp_take(&body, vals.len(), None, &[vals.len()]).is_err());
        let short_nulls = Bitmap::new_ones(32);
        assert!(decode_alp_take(&body, vals.len(), Some(&short_nulls), &[0]).is_err());
        assert!(decode_alp_selected(&body, vals.len(), Some(&short_nulls), &one_mask).is_err());
        assert!(decode_alp_selected(&body, vals.len(), None, &short_nulls).is_err());
    }

    #[test]
    fn decode_alp_take_and_selected_row_count_over_u16_max() {
        // m4: verify `decode_alp_take` and `decode_alp_selected` handle `row_count > 65_536`
        // (including exceptions at `< 65_536` and regular values at `>= 65_536`, plus nulls).
        let row_count = 70_000usize;
        let mut nulls = Bitmap::new_ones(row_count);
        for i in [10usize, 65_536, 69_999] {
            let _ = nulls.set(i, false);
        }

        // Hand-craft a valid ALP block with `row_count = 70_000 > 65_536`:
        // exp = 2, fac = 0, div_mode = false -> value = (base + delta) / 100.0
        // plus two exceptions at positions < u16::MAX (7 -> NaN, 42_000 -> -0.0).
        let deltas: Vec<u64> = (0..row_count).map(|i| (i % 1000) as u64).collect();
        let exc_pos: [u16; 2] = [7, 42_000];
        let exc_bits: [u64; 2] = [f64::NAN.to_bits(), (-0.0f64).to_bits()];
        let mut encoded = Vec::new();
        encoded.push(2u8); // exp = 2, div_mode = false
        encoded.push(0u8); // fac = 0
        encoded.extend_from_slice(&(exc_pos.len() as u16).to_le_bytes());
        encoded.extend_from_slice(&0i64.to_le_bytes()); // base = 0
        encoded.push(10u8); // bit_width = 10 (fits 0..1000)
        pack_u64(&deltas, 10, &mut encoded).unwrap();
        for &pos in &exc_pos {
            encoded.extend_from_slice(&pos.to_le_bytes());
        }
        for &bits in &exc_bits {
            encoded.extend_from_slice(&bits.to_le_bytes());
        }

        let full = decode_alp_body(&encoded, row_count, Some(&nulls)).unwrap();
        assert_eq!(full.len(), row_count);
        assert!(full[7].is_nan());
        assert_eq!(full[42_000].to_bits(), (-0.0f64).to_bits());
        assert_eq!(full[10].to_bits(), 0.0f64.to_bits());
        assert_eq!(full[65_536].to_bits(), 0.0f64.to_bits());
        assert_eq!(full[65_537], ((65_537 % 1000) as f64) / 100.0);

        // Sparse take crossing u16::MAX boundary.
        let sparse_indices = [
            0usize, 7, 10, 42_000, 65_535, 65_536, 65_537, 69_998, 69_999,
        ];
        let got_sparse =
            decode_alp_take(&encoded, row_count, Some(&nulls), &sparse_indices).unwrap();
        for (&idx, &got) in sparse_indices.iter().zip(got_sparse.iter()) {
            assert_eq!(
                got.to_bits(),
                full[idx].to_bits(),
                "sparse take mismatch at row {idx}"
            );
        }

        // Dense take (> row_count / 16) crossing u16::MAX boundary.
        let dense_indices: Vec<usize> = (0..row_count).step_by(8).collect();
        let got_dense = decode_alp_take(&encoded, row_count, Some(&nulls), &dense_indices).unwrap();
        for (&idx, &got) in dense_indices.iter().zip(got_dense.iter()) {
            assert_eq!(
                got.to_bits(),
                full[idx].to_bits(),
                "dense take mismatch at row {idx}"
            );
        }

        // Sparse `decode_alp_selected` crossing u16::MAX boundary.
        let mut sparse_mask = Bitmap::new_zeroed(row_count);
        for &idx in &sparse_indices {
            let _ = sparse_mask.set(idx, true);
        }
        let got_sel = decode_alp_selected(&encoded, row_count, Some(&nulls), &sparse_mask).unwrap();
        for i in 0..row_count {
            let expected = if sparse_mask.get(i) { full[i] } else { 0.0 };
            assert_eq!(
                got_sel[i].to_bits(),
                expected.to_bits(),
                "sparse selected mismatch at row {i}"
            );
        }
    }
}
