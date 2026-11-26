//! SIMD ASCII lowercasing, deterministic trigram hashing, and adaptive `TrigramBloom`.

#![allow(unsafe_code)]

use std::fmt;

use crate::StatamError;

/// Minimum allowed byte length for a [`TrigramBloom`] bitset.
pub const MIN_TRIGRAM_BLOOM_BYTES: usize = 256;
/// Maximum allowed byte length for a [`TrigramBloom`] bitset.
pub const MAX_TRIGRAM_BLOOM_BYTES: usize = 4096;

/// Writes ASCII-lowercased `src` into `dst` (clearing `dst` first).
///
/// Only ASCII uppercase bytes (`'A'..='Z'`) are mapped to `'a'..='z'`; all
/// other bytes including non-ASCII UTF-8 bytes (`>= 0x80`) are preserved verbatim.
pub fn ascii_lowercase_into(src: &[u8], dst: &mut Vec<u8>) {
    dst.clear();
    if src.is_empty() {
        return;
    }
    dst.resize(src.len(), 0);

    #[cfg(all(target_arch = "x86_64", not(miri)))]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: `is_x86_feature_detected!("avx2")` verified above and
            // `dst.len() == src.len()`.
            unsafe {
                ascii_lowercase_avx2(src, dst);
            }
            return;
        }
    }
    ascii_lowercase_scalar(src, dst);
}

/// Computes a deterministic 32-bit hash for a 3-byte trigram.
#[inline]
pub fn trigram_hash(b0: u8, b1: u8, b2: u8) -> u32 {
    let mut x = u32::from(b0) | (u32::from(b1) << 8) | (u32::from(b2) << 16);
    x ^= x >> 16;
    x = x.wrapping_mul(0x85eb_ca6b);
    x ^= x >> 13;
    x = x.wrapping_mul(0xc2b2_ae35);
    x ^ (x >> 16)
}

/// Extracts ASCII-lowercased trigram hashes from `bytes`, appending to `out`.
///
/// When `ascii_only` is `true` (used for `ILIKE` query pruning), only trigrams
/// whose three bytes are all `< 128` are emitted so multi-byte UTF-8 case folding
/// never produces false negatives.
pub fn extract_trigram_hashes(bytes: &[u8], ascii_only: bool, out: &mut Vec<u32>) {
    if bytes.len() < 3 {
        return;
    }
    let mut lowered = Vec::new();
    ascii_lowercase_into(bytes, &mut lowered);
    out.reserve(lowered.len() - 2);
    for w in lowered.windows(3) {
        let (b0, b1, b2) = (w[0], w[1], w[2]);
        if !ascii_only || (b0 < 128 && b1 < 128 && b2 < 128) {
            out.push(trigram_hash(b0, b1, b2));
        }
    }
}

/// Adaptive power-of-two byte-sized trigram bloom filter (`256..=4096` bytes).
#[derive(Clone, PartialEq, Eq)]
pub struct TrigramBloom {
    bits: Vec<u8>,
}

impl fmt::Debug for TrigramBloom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrigramBloom")
            .field("byte_len", &self.bits.len())
            .finish()
    }
}

impl TrigramBloom {
    /// Creates an empty `TrigramBloom` clamped and rounded to a power of two in `256..=4096` bytes.
    pub fn new(byte_len: usize) -> Self {
        let clamped = byte_len
            .clamp(MIN_TRIGRAM_BLOOM_BYTES, MAX_TRIGRAM_BLOOM_BYTES)
            .next_power_of_two()
            .min(MAX_TRIGRAM_BLOOM_BYTES);
        Self {
            bits: vec![0u8; clamped],
        }
    }

    /// Wraps serialized filter bytes after validating power-of-two size in `256..=4096`.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, StatamError> {
        let len = bytes.len();
        if !(MIN_TRIGRAM_BLOOM_BYTES..=MAX_TRIGRAM_BLOOM_BYTES).contains(&len)
            || !len.is_power_of_two()
        {
            return Err(StatamError::Corrupt {
                reason: "invalid trigram bloom length",
            });
        }
        Ok(Self { bits: bytes })
    }

    /// Returns the raw bitset bytes.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bits
    }

    /// Inserts precomputed 32-bit trigram hashes into the filter.
    #[inline]
    pub fn insert_hashes(&mut self, hashes: &[u32]) {
        let bit_mask = (self.bits.len() * 8) - 1;
        for &h in hashes {
            self.set_hash_bits(h, bit_mask);
        }
    }

    /// ASCII-lowercases `bytes` and inserts every 3-byte trigram into the filter.
    pub fn insert_bytes(&mut self, bytes: &[u8]) {
        if bytes.len() < 3 {
            return;
        }
        let mut lowered = Vec::new();
        ascii_lowercase_into(bytes, &mut lowered);
        let bit_mask = (self.bits.len() * 8) - 1;
        for w in lowered.windows(3) {
            let h = trigram_hash(w[0], w[1], w[2]);
            self.set_hash_bits(h, bit_mask);
        }
    }

    #[inline]
    fn set_hash_bits(&mut self, h: u32, bit_mask: usize) {
        let p1 = (h as usize) & bit_mask;
        let p2 = (h.rotate_right(16) as usize) & bit_mask;
        self.bits[p1 >> 3] |= 1u8 << (p1 & 7);
        self.bits[p2 >> 3] |= 1u8 << (p2 & 7);
    }

    /// Returns `true` if all `hashes` may be present (`false` guarantees at least one is absent).
    #[inline]
    pub fn may_contain_hashes(&self, hashes: &[u32]) -> bool {
        let bit_mask = (self.bits.len() * 8) - 1;
        for &h in hashes {
            let p1 = (h as usize) & bit_mask;
            if (self.bits[p1 >> 3] & (1u8 << (p1 & 7))) == 0 {
                return false;
            }
            let p2 = (h.rotate_right(16) as usize) & bit_mask;
            if (self.bits[p2 >> 3] & (1u8 << (p2 & 7))) == 0 {
                return false;
            }
        }
        true
    }

    /// Fraction of set bits in `0.0..=1.0`.
    pub fn bit_density(&self) -> f64 {
        let ones: usize = self.bits.iter().map(|b| b.count_ones() as usize).sum();
        (ones as f64) / ((self.bits.len() * 8) as f64)
    }
}

fn ascii_lowercase_scalar(src: &[u8], dst: &mut [u8]) {
    for (out, &b) in dst.iter_mut().zip(src) {
        *out = b.to_ascii_lowercase();
    }
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn ascii_lowercase_avx2(src: &[u8], dst: &mut [u8]) {
    use std::arch::x86_64::*;

    debug_assert_eq!(src.len(), dst.len());
    // Map 'A'..='Z' (0x41..=0x5A) to -128..=-103 via wrapping +0x3F so signed
    // `cmpgt(-102, x)` selects strictly 'A'..='Z' and leaves non-ASCII untouched.
    let offset = _mm256_set1_epi8(0x3Fi8);
    let upper_bound = _mm256_set1_epi8(-102i8);
    let bit20 = _mm256_set1_epi8(0x20i8);

    let full_chunks = src.len() / 32;
    for i in 0..full_chunks {
        let byte_off = i * 32;
        // SAFETY: `i * 32 + 32 <= src.len() == dst.len()`; unaligned load/store are valid.
        unsafe {
            let v = _mm256_loadu_si256(src.as_ptr().add(byte_off) as *const __m256i);
            let shifted = _mm256_add_epi8(v, offset);
            let is_upper = _mm256_cmpgt_epi8(upper_bound, shifted);
            let lowered = _mm256_or_si256(v, _mm256_and_si256(is_upper, bit20));
            _mm256_storeu_si256(dst.as_mut_ptr().add(byte_off) as *mut __m256i, lowered);
        }
    }

    let rem_start = full_chunks * 32;
    if rem_start < src.len() {
        ascii_lowercase_scalar(&src[rem_start..], &mut dst[rem_start..]);
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn trigram_ascii_lowercase_simd_matches_scalar_preserves_utf8() {
        // Test every single byte value 0..=255 multiple times across 32-byte boundaries.
        let mut all_bytes: Vec<u8> = (0..=255u8).collect();
        all_bytes
            .extend_from_slice("Hello Café ÜBER Ñoño 🚀 JSON: {\"KEY\": \"VALUE\"}!".as_bytes());

        let mut simd_out = Vec::new();
        ascii_lowercase_into(&all_bytes, &mut simd_out);

        let mut scalar_out = vec![0u8; all_bytes.len()];
        ascii_lowercase_scalar(&all_bytes, &mut scalar_out);
        assert_eq!(simd_out, scalar_out);

        // Verify UTF-8 non-ASCII bytes are untouched (e.g. 'Ü' = [0xC3, 0x9C] stays uppercase UTF-8).
        let text = "ÜBER_Café_ABC";
        ascii_lowercase_into(text.as_bytes(), &mut simd_out);
        assert_eq!(simd_out, "Über_café_abc".as_bytes());

        // Verify ascii_only filtering skips non-ASCII trigrams for ILIKE.
        let mut ilike_hashes = Vec::new();
        extract_trigram_hashes("Üabc".as_bytes(), true, &mut ilike_hashes);
        // 'Ü' is 2 bytes (0xC3, 0x9C), so only "abc" has all bytes < 128.
        assert_eq!(ilike_hashes.len(), 1);
        assert_eq!(ilike_hashes[0], trigram_hash(b'a', b'b', b'c'));

        // TrigramBloom roundtrip and query check.
        let mut tb = TrigramBloom::new(256);
        tb.insert_bytes(b"Authorization: Bearer SECRET_TOKEN_123");
        assert!(tb.bit_density() > 0.0 && tb.bit_density() < 0.5);

        let mut tb_from_hashes = TrigramBloom::new(256);
        let mut all_tris = Vec::new();
        extract_trigram_hashes(
            b"Authorization: Bearer SECRET_TOKEN_123",
            false,
            &mut all_tris,
        );
        tb_from_hashes.insert_hashes(&all_tris);
        assert_eq!(tb_from_hashes, tb);

        let mut needle_hashes = Vec::new();
        extract_trigram_hashes(b"bearer secret", true, &mut needle_hashes);
        assert!(tb.may_contain_hashes(&needle_hashes));

        let mut missing_hashes = Vec::new();
        extract_trigram_hashes(b"nonexistent_xyz", true, &mut missing_hashes);
        assert!(!tb.may_contain_hashes(&missing_hashes));

        let round = TrigramBloom::from_bytes(tb.as_bytes().to_vec()).unwrap();
        assert_eq!(round, tb);
        assert!(matches!(
            TrigramBloom::from_bytes(vec![0u8; 300]),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            TrigramBloom::from_bytes(vec![0u8; 128]),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            TrigramBloom::from_bytes(vec![0u8; 8192]),
            Err(StatamError::Corrupt { .. })
        ));
    }

    proptest! {
        #[test]
        fn proptest_ascii_lowercase_parity(bytes in prop::collection::vec(any::<u8>(), 0..=512)) {
            let mut simd_out = Vec::new();
            ascii_lowercase_into(&bytes, &mut simd_out);
            let mut scalar_out = vec![0u8; bytes.len()];
            ascii_lowercase_scalar(&bytes, &mut scalar_out);
            prop_assert_eq!(simd_out, scalar_out);
        }

        #[test]
        fn proptest_trigram_bloom_zero_false_negatives(
            haystack in prop::collection::vec(any::<u8>(), 3..=256),
            start in 0usize..=250,
            len in 3usize..=32,
        ) {
            if start < haystack.len() {
                let end = (start + len).min(haystack.len());
                let needle = &haystack[start..end];
                if needle.len() >= 3 {
                    let mut tb = TrigramBloom::new(512);
                    tb.insert_bytes(&haystack);
                    let mut hashes = Vec::new();
                    extract_trigram_hashes(needle, true, &mut hashes);
                    prop_assert!(tb.may_contain_hashes(&hashes));
                }
            }
        }
    }
}
