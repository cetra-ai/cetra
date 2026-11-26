//! SIMD-accelerated predicate evaluation producing packed `cetra_column::Bitmap` masks.

#![allow(unsafe_code)]

use cetra_column::Bitmap;

/// Comparison operator for SIMD predicate evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FilterOp {
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

#[inline]
fn words_to_bitmap(words: Vec<u64>, len: usize) -> Bitmap {
    Bitmap::from_words(words, len).unwrap_or_else(|_| Bitmap::new_zeroed(len))
}

/// Evaluates `values[i] <op> rhs` for every element in `values`.
pub fn filter_i64(values: &[i64], op: FilterOp, rhs: i64) -> Bitmap {
    if values.is_empty() {
        return Bitmap::new_zeroed(0);
    }
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: `is_x86_feature_detected!("avx2")` verified above.
            return unsafe { filter_i64_avx2(values, op, rhs) };
        }
    }
    filter_i64_scalar(values, op, rhs)
}

/// Evaluates inclusive `lo <= values[i] && values[i] <= hi` for every element in `values`.
pub fn filter_i64_between(values: &[i64], lo: i64, hi: i64) -> Bitmap {
    if values.is_empty() || lo > hi {
        return Bitmap::new_zeroed(values.len());
    }
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: `is_x86_feature_detected!("avx2")` verified above.
            return unsafe { filter_i64_between_avx2(values, lo, hi) };
        }
    }
    filter_i64_between_scalar(values, lo, hi)
}

/// Evaluates `values[i] <op> rhs` for every `u32` element in `values`.
pub fn filter_u32(values: &[u32], op: FilterOp, rhs: u32) -> Bitmap {
    if values.is_empty() {
        return Bitmap::new_zeroed(0);
    }
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: `is_x86_feature_detected!("avx2")` verified above.
            return unsafe { filter_u32_avx2(values, op, rhs) };
        }
    }
    filter_u32_scalar(values, op, rhs)
}

/// Evaluates IEEE-754 `values[i] <op> rhs` for every `f64` element in `values`.
///
/// `NaN` evaluates to `false` for `Eq`, `Lt`, `Le`, `Gt`, `Ge` and `true` for `Ne`,
/// matching Rust scalar IEEE-754 semantics bit-for-bit.
pub fn filter_f64(values: &[f64], op: FilterOp, rhs: f64) -> Bitmap {
    if values.is_empty() {
        return Bitmap::new_zeroed(0);
    }
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: `is_x86_feature_detected!("avx2")` verified above.
            return unsafe { filter_f64_avx2(values, op, rhs) };
        }
    }
    filter_f64_scalar(values, op, rhs)
}

// --- Portable Scalar Implementations ---

#[inline]
fn cmp_i64(v: i64, op: FilterOp, rhs: i64) -> bool {
    match op {
        FilterOp::Eq => v == rhs,
        FilterOp::Ne => v != rhs,
        FilterOp::Lt => v < rhs,
        FilterOp::Le => v <= rhs,
        FilterOp::Gt => v > rhs,
        FilterOp::Ge => v >= rhs,
    }
}

#[inline]
fn cmp_u32(v: u32, op: FilterOp, rhs: u32) -> bool {
    match op {
        FilterOp::Eq => v == rhs,
        FilterOp::Ne => v != rhs,
        FilterOp::Lt => v < rhs,
        FilterOp::Le => v <= rhs,
        FilterOp::Gt => v > rhs,
        FilterOp::Ge => v >= rhs,
    }
}

#[inline]
fn cmp_f64(v: f64, op: FilterOp, rhs: f64) -> bool {
    match op {
        FilterOp::Eq => v == rhs,
        FilterOp::Ne => v != rhs,
        FilterOp::Lt => v < rhs,
        FilterOp::Le => v <= rhs,
        FilterOp::Gt => v > rhs,
        FilterOp::Ge => v >= rhs,
    }
}

fn filter_i64_scalar(values: &[i64], op: FilterOp, rhs: i64) -> Bitmap {
    let mut words = vec![0u64; values.len().div_ceil(64)];
    for (word_idx, chunk) in values.chunks(64).enumerate() {
        let mut word = 0u64;
        for (bit, &v) in chunk.iter().enumerate() {
            if cmp_i64(v, op, rhs) {
                word |= 1u64 << bit;
            }
        }
        words[word_idx] = word;
    }
    words_to_bitmap(words, values.len())
}

fn filter_i64_between_scalar(values: &[i64], lo: i64, hi: i64) -> Bitmap {
    let mut words = vec![0u64; values.len().div_ceil(64)];
    if lo > hi {
        return words_to_bitmap(words, values.len());
    }
    for (word_idx, chunk) in values.chunks(64).enumerate() {
        let mut word = 0u64;
        for (bit, &v) in chunk.iter().enumerate() {
            if v >= lo && v <= hi {
                word |= 1u64 << bit;
            }
        }
        words[word_idx] = word;
    }
    words_to_bitmap(words, values.len())
}

fn filter_u32_scalar(values: &[u32], op: FilterOp, rhs: u32) -> Bitmap {
    let mut words = vec![0u64; values.len().div_ceil(64)];
    for (word_idx, chunk) in values.chunks(64).enumerate() {
        let mut word = 0u64;
        for (bit, &v) in chunk.iter().enumerate() {
            if cmp_u32(v, op, rhs) {
                word |= 1u64 << bit;
            }
        }
        words[word_idx] = word;
    }
    words_to_bitmap(words, values.len())
}

fn filter_f64_scalar(values: &[f64], op: FilterOp, rhs: f64) -> Bitmap {
    let mut words = vec![0u64; values.len().div_ceil(64)];
    for (word_idx, chunk) in values.chunks(64).enumerate() {
        let mut word = 0u64;
        for (bit, &v) in chunk.iter().enumerate() {
            if cmp_f64(v, op, rhs) {
                word |= 1u64 << bit;
            }
        }
        words[word_idx] = word;
    }
    words_to_bitmap(words, values.len())
}

// --- x86_64 AVX2 Implementations ---

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn eval_i64_vec_mask(
    v: std::arch::x86_64::__m256i,
    vrhs: std::arch::x86_64::__m256i,
    op: FilterOp,
) -> u64 {
    use std::arch::x86_64::*;

    let (cmp, invert) = match op {
        FilterOp::Eq => (_mm256_cmpeq_epi64(v, vrhs), false),
        FilterOp::Ne => (_mm256_cmpeq_epi64(v, vrhs), true),
        FilterOp::Gt => (_mm256_cmpgt_epi64(v, vrhs), false),
        FilterOp::Le => (_mm256_cmpgt_epi64(v, vrhs), true),
        FilterOp::Lt => (_mm256_cmpgt_epi64(vrhs, v), false),
        FilterOp::Ge => (_mm256_cmpgt_epi64(vrhs, v), true),
    };
    let mask = _mm256_movemask_pd(_mm256_castsi256_pd(cmp)) as u64;
    if invert { (!mask) & 0xF } else { mask }
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn filter_i64_avx2(values: &[i64], op: FilterOp, rhs: i64) -> Bitmap {
    use std::arch::x86_64::*;

    let vrhs = _mm256_set1_epi64x(rhs);
    let mut words = vec![0u64; values.len().div_ceil(64)];

    for (word_idx, chunk) in values.chunks(64).enumerate() {
        let mut word = 0u64;
        let full_quads = chunk.len() / 4;
        for q in 0..full_quads {
            // SAFETY: `q < chunk.len() / 4`, so `q * 4 + 4 <= chunk.len()`;
            // `_mm256_loadu_si256` accepts any alignment.
            let v = unsafe { _mm256_loadu_si256(chunk.as_ptr().add(q * 4) as *const __m256i) };
            // SAFETY: AVX2 target feature is active for this function.
            let m = unsafe { eval_i64_vec_mask(v, vrhs, op) };
            word |= m << (q * 4);
        }
        let rem_start = full_quads * 4;
        for (idx, &v) in chunk[rem_start..].iter().enumerate() {
            if cmp_i64(v, op, rhs) {
                word |= 1u64 << (rem_start + idx);
            }
        }
        words[word_idx] = word;
    }
    words_to_bitmap(words, values.len())
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn filter_i64_between_avx2(values: &[i64], lo: i64, hi: i64) -> Bitmap {
    use std::arch::x86_64::*;

    let vlo = _mm256_set1_epi64x(lo);
    let vhi = _mm256_set1_epi64x(hi);
    let mut words = vec![0u64; values.len().div_ceil(64)];

    for (word_idx, chunk) in values.chunks(64).enumerate() {
        let mut word = 0u64;
        let full_quads = chunk.len() / 4;
        for q in 0..full_quads {
            // SAFETY: `q * 4 + 4 <= chunk.len()`; unaligned load is valid.
            let v = unsafe { _mm256_loadu_si256(chunk.as_ptr().add(q * 4) as *const __m256i) };
            let out_of_range =
                _mm256_or_si256(_mm256_cmpgt_epi64(vlo, v), _mm256_cmpgt_epi64(v, vhi));
            let outside_mask = _mm256_movemask_pd(_mm256_castsi256_pd(out_of_range)) as u64;
            let inside_mask = (!outside_mask) & 0xF;
            word |= inside_mask << (q * 4);
        }
        let rem_start = full_quads * 4;
        for (idx, &v) in chunk[rem_start..].iter().enumerate() {
            if v >= lo && v <= hi {
                word |= 1u64 << (rem_start + idx);
            }
        }
        words[word_idx] = word;
    }
    words_to_bitmap(words, values.len())
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn filter_u32_avx2(values: &[u32], op: FilterOp, rhs: u32) -> Bitmap {
    use std::arch::x86_64::*;

    let bias = _mm256_set1_epi32(i32::MIN);
    let vrhs_raw = _mm256_set1_epi32(rhs as i32);
    let vrhs_biased = _mm256_xor_si256(vrhs_raw, bias);
    let mut words = vec![0u64; values.len().div_ceil(64)];

    for (word_idx, chunk) in values.chunks(64).enumerate() {
        let mut word = 0u64;
        let full_octs = chunk.len() / 8;
        for o in 0..full_octs {
            // SAFETY: `o * 8 + 8 <= chunk.len()`; unaligned load is valid.
            let v_raw = unsafe { _mm256_loadu_si256(chunk.as_ptr().add(o * 8) as *const __m256i) };
            let (cmp, invert) = match op {
                FilterOp::Eq => (_mm256_cmpeq_epi32(v_raw, vrhs_raw), false),
                FilterOp::Ne => (_mm256_cmpeq_epi32(v_raw, vrhs_raw), true),
                FilterOp::Gt => {
                    let v_biased = _mm256_xor_si256(v_raw, bias);
                    (_mm256_cmpgt_epi32(v_biased, vrhs_biased), false)
                }
                FilterOp::Le => {
                    let v_biased = _mm256_xor_si256(v_raw, bias);
                    (_mm256_cmpgt_epi32(v_biased, vrhs_biased), true)
                }
                FilterOp::Lt => {
                    let v_biased = _mm256_xor_si256(v_raw, bias);
                    (_mm256_cmpgt_epi32(vrhs_biased, v_biased), false)
                }
                FilterOp::Ge => {
                    let v_biased = _mm256_xor_si256(v_raw, bias);
                    (_mm256_cmpgt_epi32(vrhs_biased, v_biased), true)
                }
            };
            let mask = _mm256_movemask_ps(_mm256_castsi256_ps(cmp)) as u64;
            let m = if invert { (!mask) & 0xFF } else { mask };
            word |= m << (o * 8);
        }
        let rem_start = full_octs * 8;
        for (idx, &v) in chunk[rem_start..].iter().enumerate() {
            if cmp_u32(v, op, rhs) {
                word |= 1u64 << (rem_start + idx);
            }
        }
        words[word_idx] = word;
    }
    words_to_bitmap(words, values.len())
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn filter_f64_avx2(values: &[f64], op: FilterOp, rhs: f64) -> Bitmap {
    use std::arch::x86_64::*;

    let vrhs = _mm256_set1_pd(rhs);
    let mut words = vec![0u64; values.len().div_ceil(64)];

    for (word_idx, chunk) in values.chunks(64).enumerate() {
        let mut word = 0u64;
        let full_quads = chunk.len() / 4;
        for q in 0..full_quads {
            // SAFETY: `q * 4 + 4 <= chunk.len()`; `_mm256_loadu_pd` allows arbitrary alignment.
            let v = unsafe { _mm256_loadu_pd(chunk.as_ptr().add(q * 4)) };
            let cmp = match op {
                FilterOp::Eq => _mm256_cmp_pd::<_CMP_EQ_OQ>(v, vrhs),
                FilterOp::Ne => _mm256_cmp_pd::<_CMP_NEQ_UQ>(v, vrhs),
                FilterOp::Lt => _mm256_cmp_pd::<_CMP_LT_OQ>(v, vrhs),
                FilterOp::Le => _mm256_cmp_pd::<_CMP_LE_OQ>(v, vrhs),
                FilterOp::Gt => _mm256_cmp_pd::<_CMP_GT_OQ>(v, vrhs),
                FilterOp::Ge => _mm256_cmp_pd::<_CMP_GE_OQ>(v, vrhs),
            };
            let m = _mm256_movemask_pd(cmp) as u64;
            word |= m << (q * 4);
        }
        let rem_start = full_quads * 4;
        for (idx, &v) in chunk[rem_start..].iter().enumerate() {
            if cmp_f64(v, op, rhs) {
                word |= 1u64 << (rem_start + idx);
            }
        }
        words[word_idx] = word;
    }
    words_to_bitmap(words, values.len())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const ALL_OPS: &[FilterOp] = &[
        FilterOp::Eq,
        FilterOp::Ne,
        FilterOp::Lt,
        FilterOp::Le,
        FilterOp::Gt,
        FilterOp::Ge,
    ];

    #[test]
    fn filter_i64_u32_f64_simd_matches_scalar_including_nan_and_inf() {
        let i64_vals = [
            i64::MIN,
            i64::MIN + 1,
            -42,
            0,
            1,
            42,
            i64::MAX - 1,
            i64::MAX,
        ];
        let mut long_i64 = Vec::new();
        for rep in 0..20 {
            for &v in &i64_vals {
                long_i64.push(v.wrapping_add(rep));
            }
        }
        long_i64.push(77);
        for &op in ALL_OPS {
            for &rhs in &[i64::MIN, -42, 0, 42, 77, i64::MAX] {
                assert_eq!(
                    filter_i64(&long_i64, op, rhs),
                    filter_i64_scalar(&long_i64, op, rhs),
                    "i64 mismatch op={op:?} rhs={rhs}"
                );
            }
        }
        for (lo, hi) in [(i64::MIN, i64::MAX), (-42, 42), (42, -42), (0, 0)] {
            assert_eq!(
                filter_i64_between(&long_i64, lo, hi),
                filter_i64_between_scalar(&long_i64, lo, hi),
                "i64 between mismatch lo={lo} hi={hi}"
            );
        }

        let u32_vals = [
            0u32,
            1,
            127,
            128,
            0x7FFF_FFFF,
            0x8000_0000,
            u32::MAX - 1,
            u32::MAX,
        ];
        let mut long_u32 = Vec::new();
        for rep in 0..18u32 {
            for &v in &u32_vals {
                long_u32.push(v.wrapping_add(rep));
            }
        }
        long_u32.extend_from_slice(&[0, u32::MAX, 0x8000_0000]);
        for &op in ALL_OPS {
            for &rhs in &[0u32, 128, 0x7FFF_FFFF, 0x8000_0000, u32::MAX] {
                assert_eq!(
                    filter_u32(&long_u32, op, rhs),
                    filter_u32_scalar(&long_u32, op, rhs),
                    "u32 mismatch op={op:?} rhs={rhs}"
                );
            }
        }

        let snan = f64::from_bits(0x7FF0_0000_0000_0001);
        let qnan = f64::NAN;
        let neg_nan = f64::from_bits(0xFFF8_0000_0000_0042);
        let f64_vals = [
            f64::NEG_INFINITY,
            -1e100,
            -1.0,
            -f64::MIN_POSITIVE / 2.0,
            -0.0,
            0.0,
            f64::MIN_POSITIVE / 2.0,
            1.0,
            1e100,
            f64::INFINITY,
            qnan,
            snan,
            neg_nan,
        ];
        let mut long_f64 = Vec::new();
        for _ in 0..11 {
            long_f64.extend_from_slice(&f64_vals);
        }
        for &op in ALL_OPS {
            for &rhs in &[-0.0, 0.0, 1.0, f64::NEG_INFINITY, f64::INFINITY, qnan, snan] {
                assert_eq!(
                    filter_f64(&long_f64, op, rhs),
                    filter_f64_scalar(&long_f64, op, rhs),
                    "f64 mismatch op={op:?} rhs={rhs:?}"
                );
            }
        }
    }

    proptest! {
        #[test]
        fn proptest_filter_i64_parity(
            vals in prop::collection::vec(any::<i64>(), 0..=300),
            rhs in any::<i64>(),
            lo in any::<i64>(),
            hi in any::<i64>(),
        ) {
            for &op in ALL_OPS {
                prop_assert_eq!(filter_i64(&vals, op, rhs), filter_i64_scalar(&vals, op, rhs));
            }
            prop_assert_eq!(
                filter_i64_between(&vals, lo, hi),
                filter_i64_between_scalar(&vals, lo, hi)
            );
        }

        #[test]
        fn proptest_filter_u32_parity(
            vals in prop::collection::vec(any::<u32>(), 0..=300),
            rhs in any::<u32>(),
        ) {
            for &op in ALL_OPS {
                prop_assert_eq!(filter_u32(&vals, op, rhs), filter_u32_scalar(&vals, op, rhs));
            }
        }

        #[test]
        fn proptest_filter_f64_parity(
            bits in prop::collection::vec(any::<u64>(), 0..=300),
            rhs_bits in any::<u64>(),
        ) {
            let vals: Vec<f64> = bits.into_iter().map(f64::from_bits).collect();
            let rhs = f64::from_bits(rhs_bits);
            for &op in ALL_OPS {
                prop_assert_eq!(filter_f64(&vals, op, rhs), filter_f64_scalar(&vals, op, rhs));
            }
        }
    }
}
