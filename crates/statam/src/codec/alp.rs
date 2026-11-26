//! SIGMOD-2024 ALP (Adaptive Lossless floating-Point) bit-exact `f64` codec.
//!
//! Encodes `f64` values by picking `(exp, fac)` in `0..=18` that maximizes
//! exact decimal-scaled integer round-trips, bitpacking the scaled `i64`s via
//! FastLanes `ForBitpack`, and patching non-matching values (`NaN` with exact
//! payload bits, `±inf`, `-0.0`, subnormals, high-precision floats) via a
//! sparse `(pos: u16, raw_bits: u64)` exception list.

use cetra_column::Bitmap;

use crate::simd::bitpack::{min_bits_u64, pack_u64, unpack_u64};
use crate::{MAX_BLOCK_RAW_BYTES, StatamError};

/// Maximum `exp` / `fac` index in ALP (`0..=18`).
pub const ALP_MAX_EXP: u8 = 18;

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
fn try_scale_exact(v: f64, exp: u8, fac: u8) -> Option<i64> {
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
    let recon = (scaled as f64) * POW10[usize::from(fac)] * FRAC10[usize::from(exp)];
    if recon.to_bits() == v.to_bits() {
        Some(scaled)
    } else {
        None
    }
}

#[inline]
fn unscale(scaled: i64, exp: u8, fac: u8) -> f64 {
    (scaled as f64) * POW10[usize::from(fac)] * FRAC10[usize::from(exp)]
}

fn pick_best_params(values: &[f64], nulls: Option<&Bitmap>) -> (u8, u8) {
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
        return (0, 0);
    }

    let sample_slice = &samples[..sample_count];
    let mut best_hits = 0usize;
    let mut best_exp = 0u8;
    let mut best_fac = 0u8;

    // Fast path: test common decimal scaling `(exp, fac = 0)` for `exp = 0..=10` first.
    for exp in 0..=10u8 {
        let mut hits = 0usize;
        for &v in sample_slice {
            if try_scale_exact(v, exp, 0).is_some() {
                hits += 1;
            }
        }
        if hits > best_hits || (hits == best_hits && hits > 0 && exp < best_exp) {
            best_hits = hits;
            best_exp = exp;
            best_fac = 0;
        }
        if best_hits == sample_count {
            return (best_exp, 0);
        }
    }

    for exp in 0..=ALP_MAX_EXP {
        for fac in 0..=exp {
            if fac == 0 && exp <= 10 {
                continue;
            }
            let mut hits = 0usize;
            for &v in sample_slice {
                if try_scale_exact(v, exp, fac).is_some() {
                    hits += 1;
                }
            }
            if hits > best_hits || (hits == best_hits && hits > 0 && exp < best_exp) {
                best_hits = hits;
                best_exp = exp;
                best_fac = fac;
            }
            if best_hits == sample_count {
                // Found a parameter pair with 100% sample match; prefer smaller exp.
                return (best_exp, best_fac);
            }
        }
    }

    (best_exp, best_fac)
}

/// Encodes an `f64` block body (without leading row null bitmap) using ALP.
///
/// Returns `Ok(None)` if `values` is empty, `values.len() > 65_536`, exception rate
/// exceeds 25%, or the encoded body is not strictly smaller than raw `values.len() * 8`.
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

    let (exp, fac) = pick_best_params(values, nulls);

    let mut scaled_vals = vec![0i64; values.len()];
    let mut exc_pos: Vec<u16> = Vec::new();
    let mut exc_bits: Vec<u64> = Vec::new();
    let mut first_exact: Option<i64> = None;

    // 25% exception budget over non-null values.
    let max_exceptions = valid_count / 4;

    for (i, &v) in values.iter().enumerate() {
        if nulls.is_some_and(|bm| !bm.get(i)) {
            continue;
        }
        if let Some(s) = try_scale_exact(v, exp, fac) {
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
    out.push(exp);
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

    if out.len() >= values.len() * 8 {
        return Ok(None);
    }

    Ok(Some(out))
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
    if count == 0 {
        if !src.is_empty() {
            return Err(StatamError::Corrupt {
                reason: "trailing bytes in empty alp block",
            });
        }
        return Ok(Vec::new());
    }
    if src.len() < 13 {
        return Err(StatamError::Corrupt {
            reason: "truncated alp block header",
        });
    }

    let exp = src[0];
    let fac = src[1];
    if exp > ALP_MAX_EXP || fac > exp {
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

    let mut deltas = Vec::with_capacity(count);
    let packed_slice = &src[13..];
    let packed_len = crate::simd::bitpack::packed_bytes_len(count, bit_width)?;
    if packed_slice.len() < packed_len {
        return Err(StatamError::Corrupt {
            reason: "truncated alp packed deltas",
        });
    }
    unpack_u64(&packed_slice[..packed_len], count, bit_width, &mut deltas)?;

    let exc_bytes_needed = exc_count
        .checked_mul(10)
        .ok_or(StatamError::BoundsExceeded {
            reason: "alp exception table size overflow",
        })?;
    let exc_slice = &packed_slice[packed_len..];
    if exc_slice.len() != exc_bytes_needed {
        return Err(StatamError::Corrupt {
            reason: "alp exception table length mismatch",
        });
    }

    let mut out = Vec::with_capacity(count);
    for &d in &deltas {
        let scaled = base.wrapping_add(d as i64);
        out.push(unscale(scaled, exp, fac));
    }

    let pos_section = &exc_slice[..exc_count * 2];
    let bits_section = &exc_slice[exc_count * 2..];
    for i in 0..exc_count {
        let pos = u16::from_le_bytes([pos_section[i * 2], pos_section[i * 2 + 1]]) as usize;
        if pos >= count {
            return Err(StatamError::Corrupt {
                reason: "alp exception position out of bounds",
            });
        }
        let b_off = i * 8;
        let bits = u64::from_le_bytes([
            bits_section[b_off],
            bits_section[b_off + 1],
            bits_section[b_off + 2],
            bits_section[b_off + 3],
            bits_section[b_off + 4],
            bits_section[b_off + 5],
            bits_section[b_off + 6],
            bits_section[b_off + 7],
        ]);
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
