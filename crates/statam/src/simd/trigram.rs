//! SIMD ASCII lowercasing, deterministic trigram hashing, and adaptive `TrigramBloom`.

#![allow(unsafe_code)]

use std::fmt;

use crate::StatamError;

/// Minimum allowed byte length for a [`TrigramBloom`] bitset.
pub const MIN_TRIGRAM_BLOOM_BYTES: usize = 256;
/// Maximum allowed byte length for a [`TrigramBloom`] bitset.
pub const MAX_TRIGRAM_BLOOM_BYTES: usize = 16_384;

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
    #[cfg(all(
        target_arch = "aarch64",
        target_feature = "neon",
        target_endian = "little",
        not(miri)
    ))]
    {
        // SAFETY: `cfg(target_feature = "neon")` guarantees NEON at compile time.
        unsafe {
            ascii_lowercase_neon(src, dst);
        }
        return;
    }
    #[allow(unreachable_code)]
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

/// Adaptive power-of-two byte-sized trigram bloom filter (`256..=16_384` bytes).
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
    /// Creates an empty `TrigramBloom` clamped and rounded to a power of two in `256..=16_384` bytes.
    pub fn new(byte_len: usize) -> Self {
        let clamped = byte_len
            .clamp(MIN_TRIGRAM_BLOOM_BYTES, MAX_TRIGRAM_BLOOM_BYTES)
            .next_power_of_two()
            .min(MAX_TRIGRAM_BLOOM_BYTES);
        Self {
            bits: vec![0u8; clamped],
        }
    }

    /// Wraps serialized filter bytes after validating power-of-two size in `256..=16_384`.
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
        if bytes.len() < 3 || !self.bits.len().is_power_of_two() {
            return;
        }
        #[cfg(all(target_arch = "x86_64", not(miri)))]
        {
            if bytes.len() >= 16 && std::arch::is_x86_feature_detected!("avx2") {
                // SAFETY: `is_x86_feature_detected!("avx2")` verified above and
                // `self.bits.len()` is a non-zero power of two in `256..=16_384`.
                unsafe {
                    self.insert_bytes_avx2(bytes);
                }
                return;
            }
        }
        #[cfg(all(
            target_arch = "aarch64",
            target_feature = "neon",
            target_endian = "little",
            not(miri)
        ))]
        {
            if bytes.len() >= 16 {
                // SAFETY: `cfg(target_feature = "neon")` guarantees NEON at compile time.
                unsafe {
                    self.insert_bytes_neon(bytes);
                }
                return;
            }
        }
        self.insert_bytes_scalar(bytes);
    }

    #[inline]
    fn insert_bytes_scalar(&mut self, bytes: &[u8]) {
        if bytes.len() < 3 || !self.bits.len().is_power_of_two() {
            return;
        }
        let byte_mask = self.bits.len() - 1;
        let bit_mask = (self.bits.len() * 8) - 1;
        let bits = &mut self.bits;
        let mut i = 0usize;
        let end = bytes.len().saturating_sub(2);
        while i < end {
            let h = trigram_hash(
                bytes[i].to_ascii_lowercase(),
                bytes[i + 1].to_ascii_lowercase(),
                bytes[i + 2].to_ascii_lowercase(),
            );
            let p1 = (h as usize) & bit_mask;
            let p2 = (h.rotate_right(16) as usize) & bit_mask;
            bits[(p1 >> 3) & byte_mask] |= 1u8 << (p1 & 7);
            bits[(p2 >> 3) & byte_mask] |= 1u8 << (p2 & 7);
            i += 1;
        }
    }

    #[cfg(all(target_arch = "x86_64", not(miri)))]
    #[target_feature(enable = "avx2")]
    unsafe fn insert_bytes_avx2(&mut self, bytes: &[u8]) {
        use std::arch::x86_64::*;

        #[inline(always)]
        unsafe fn set_two_lanes(ptr: *mut u8, byte_mask: usize, word: u64) {
            let idx0 = (word as usize) & byte_mask;
            let mask0 = (word >> 16) as u8;
            let idx1 = ((word >> 32) as usize) & byte_mask;
            let mask1 = (word >> 48) as u8;
            // SAFETY: caller ensures `byte_mask == bits.len() - 1` and `bits.len()` is a power of two
            // (`<= MAX_TRIGRAM_BLOOM_BYTES == 16_384`), so `idx0 < bits.len()` and `idx1 < bits.len()`.
            unsafe {
                *ptr.add(idx0) |= mask0;
                *ptr.add(idx1) |= mask1;
            }
        }

        // Packed `(idx | mask << 16)` layout requires `idx < 65_536`.
        const { assert!(MAX_TRIGRAM_BLOOM_BYTES <= 65_536) };
        if !self.bits.len().is_power_of_two() || self.bits.len() > MAX_TRIGRAM_BLOOM_BYTES {
            self.insert_bytes_scalar(bytes);
            return;
        }
        let byte_mask = self.bits.len() - 1;
        let bit_mask = (self.bits.len() * 8) - 1;
        let bits = &mut self.bits;
        let ptr = bits.as_mut_ptr();

        let offset_256 = _mm256_set1_epi8(0x3Fi8);
        let upper_256 = _mm256_set1_epi8(-102i8);
        let bit20_256 = _mm256_set1_epi8(0x20i8);
        let offset_128 = _mm_set1_epi8(0x3Fi8);
        let upper_128 = _mm_set1_epi8(-102i8);
        let bit20_128 = _mm_set1_epi8(0x20i8);
        let shuf_mask = _mm256_setr_epi8(
            0, 1, 2, -1, 1, 2, 3, -1, 2, 3, 4, -1, 3, 4, 5, -1, 4, 5, 6, -1, 5, 6, 7, -1, 6, 7, 8,
            -1, 7, 8, 9, -1,
        );
        let c1 = _mm256_set1_epi32(0x85eb_ca6bu32 as i32);
        let c2 = _mm256_set1_epi32(0xc2b2_ae35u32 as i32);
        let mask_v = _mm256_set1_epi32(bit_mask as i32);
        let ones_v = _mm256_set1_epi32(1);
        let seven_v = _mm256_set1_epi32(7);

        let mut i = 0usize;
        // Wide 32-byte path: lowercase 32 bytes once and hash 24 trigrams (3 × 8 lanes) interleaved.
        while i + 32 <= bytes.len() {
            // SAFETY: `i + 32 <= bytes.len()`, so unaligned 32-byte load is within bounds.
            let v = unsafe { _mm256_loadu_si256(bytes.as_ptr().add(i) as *const __m256i) };
            let shifted = _mm256_add_epi8(v, offset_256);
            let is_upper = _mm256_cmpgt_epi8(upper_256, shifted);
            let lowered = _mm256_or_si256(v, _mm256_and_si256(is_upper, bit20_256));

            let lo_128 = _mm256_castsi256_si128(lowered);
            let hi_128 = _mm256_extracti128_si256::<1>(lowered);
            let mid_128 = _mm_alignr_epi8::<8>(hi_128, lo_128);

            let mut x0 = _mm256_shuffle_epi8(_mm256_broadcastsi128_si256(lo_128), shuf_mask);
            let mut x1 = _mm256_shuffle_epi8(_mm256_broadcastsi128_si256(mid_128), shuf_mask);
            let mut x2 = _mm256_shuffle_epi8(_mm256_broadcastsi128_si256(hi_128), shuf_mask);

            x0 = _mm256_xor_si256(x0, _mm256_srli_epi32(x0, 16));
            x1 = _mm256_xor_si256(x1, _mm256_srli_epi32(x1, 16));
            x2 = _mm256_xor_si256(x2, _mm256_srli_epi32(x2, 16));
            x0 = _mm256_mullo_epi32(x0, c1);
            x1 = _mm256_mullo_epi32(x1, c1);
            x2 = _mm256_mullo_epi32(x2, c1);
            x0 = _mm256_xor_si256(x0, _mm256_srli_epi32(x0, 13));
            x1 = _mm256_xor_si256(x1, _mm256_srli_epi32(x1, 13));
            x2 = _mm256_xor_si256(x2, _mm256_srli_epi32(x2, 13));
            x0 = _mm256_mullo_epi32(x0, c2);
            x1 = _mm256_mullo_epi32(x1, c2);
            x2 = _mm256_mullo_epi32(x2, c2);
            x0 = _mm256_xor_si256(x0, _mm256_srli_epi32(x0, 16));
            x1 = _mm256_xor_si256(x1, _mm256_srli_epi32(x1, 16));
            x2 = _mm256_xor_si256(x2, _mm256_srli_epi32(x2, 16));

            for x in [x0, x1, x2] {
                let p1_vec = _mm256_and_si256(x, mask_v);
                let rot16 = _mm256_or_si256(_mm256_srli_epi32(x, 16), _mm256_slli_epi32(x, 16));
                let p2_vec = _mm256_and_si256(rot16, mask_v);
                let idx1_vec = _mm256_srli_epi32(p1_vec, 3);
                let mask1_vec = _mm256_sllv_epi32(ones_v, _mm256_and_si256(p1_vec, seven_v));
                let idx2_vec = _mm256_srli_epi32(p2_vec, 3);
                let mask2_vec = _mm256_sllv_epi32(ones_v, _mm256_and_si256(p2_vec, seven_v));

                let v1 = _mm256_or_si256(idx1_vec, _mm256_slli_epi32(mask1_vec, 16));
                let v2 = _mm256_or_si256(idx2_vec, _mm256_slli_epi32(mask2_vec, 16));

                // SAFETY: `ptr` points to `bits` of length `byte_mask + 1` (power of two, `<= 16_384`).
                unsafe {
                    set_two_lanes(ptr, byte_mask, _mm256_extract_epi64::<0>(v1) as u64);
                    set_two_lanes(ptr, byte_mask, _mm256_extract_epi64::<1>(v1) as u64);
                    set_two_lanes(ptr, byte_mask, _mm256_extract_epi64::<2>(v1) as u64);
                    set_two_lanes(ptr, byte_mask, _mm256_extract_epi64::<3>(v1) as u64);
                    set_two_lanes(ptr, byte_mask, _mm256_extract_epi64::<0>(v2) as u64);
                    set_two_lanes(ptr, byte_mask, _mm256_extract_epi64::<1>(v2) as u64);
                    set_two_lanes(ptr, byte_mask, _mm256_extract_epi64::<2>(v2) as u64);
                    set_two_lanes(ptr, byte_mask, _mm256_extract_epi64::<3>(v2) as u64);
                }
            }
            i += 24;
        }

        while i + 16 <= bytes.len() {
            // SAFETY: `i + 16 <= bytes.len()`, so unaligned 16-byte load is within bounds.
            let v = unsafe { _mm_loadu_si128(bytes.as_ptr().add(i) as *const __m128i) };
            let shifted = _mm_add_epi8(v, offset_128);
            let is_upper = _mm_cmpgt_epi8(upper_128, shifted);
            let lowered = _mm_or_si128(v, _mm_and_si128(is_upper, bit20_128));
            let broad = _mm256_broadcastsi128_si256(lowered);
            let mut x = _mm256_shuffle_epi8(broad, shuf_mask);

            x = _mm256_xor_si256(x, _mm256_srli_epi32(x, 16));
            x = _mm256_mullo_epi32(x, c1);
            x = _mm256_xor_si256(x, _mm256_srli_epi32(x, 13));
            x = _mm256_mullo_epi32(x, c2);
            x = _mm256_xor_si256(x, _mm256_srli_epi32(x, 16));

            let p1_vec = _mm256_and_si256(x, mask_v);
            let rot16 = _mm256_or_si256(_mm256_srli_epi32(x, 16), _mm256_slli_epi32(x, 16));
            let p2_vec = _mm256_and_si256(rot16, mask_v);
            let idx1_vec = _mm256_srli_epi32(p1_vec, 3);
            let mask1_vec = _mm256_sllv_epi32(ones_v, _mm256_and_si256(p1_vec, seven_v));
            let idx2_vec = _mm256_srli_epi32(p2_vec, 3);
            let mask2_vec = _mm256_sllv_epi32(ones_v, _mm256_and_si256(p2_vec, seven_v));

            let v1 = _mm256_or_si256(idx1_vec, _mm256_slli_epi32(mask1_vec, 16));
            let v2 = _mm256_or_si256(idx2_vec, _mm256_slli_epi32(mask2_vec, 16));

            // SAFETY: `ptr` points to `bits` of length `byte_mask + 1` (power of two, `<= 16_384`).
            unsafe {
                set_two_lanes(ptr, byte_mask, _mm256_extract_epi64::<0>(v1) as u64);
                set_two_lanes(ptr, byte_mask, _mm256_extract_epi64::<1>(v1) as u64);
                set_two_lanes(ptr, byte_mask, _mm256_extract_epi64::<2>(v1) as u64);
                set_two_lanes(ptr, byte_mask, _mm256_extract_epi64::<3>(v1) as u64);
                set_two_lanes(ptr, byte_mask, _mm256_extract_epi64::<0>(v2) as u64);
                set_two_lanes(ptr, byte_mask, _mm256_extract_epi64::<1>(v2) as u64);
                set_two_lanes(ptr, byte_mask, _mm256_extract_epi64::<2>(v2) as u64);
                set_two_lanes(ptr, byte_mask, _mm256_extract_epi64::<3>(v2) as u64);
            }
            i += 8;
        }

        let end = bytes.len().saturating_sub(2);
        while i < end {
            let h = trigram_hash(
                bytes[i].to_ascii_lowercase(),
                bytes[i + 1].to_ascii_lowercase(),
                bytes[i + 2].to_ascii_lowercase(),
            );
            let p1 = (h as usize) & bit_mask;
            let p2 = (h.rotate_right(16) as usize) & bit_mask;
            bits[(p1 >> 3) & byte_mask] |= 1u8 << (p1 & 7);
            bits[(p2 >> 3) & byte_mask] |= 1u8 << (p2 & 7);
            i += 1;
        }
    }

    #[cfg(all(
        target_arch = "aarch64",
        target_feature = "neon",
        target_endian = "little",
        not(miri)
    ))]
    #[target_feature(enable = "neon")]
    fn insert_bytes_neon(&mut self, bytes: &[u8]) {
        use std::arch::aarch64::*;

        #[inline(always)]
        unsafe fn set_two_lanes(ptr: *mut u8, byte_mask: usize, word: u64) {
            let idx0 = (word as usize) & byte_mask;
            let mask0 = (word >> 16) as u8;
            let idx1 = ((word >> 32) as usize) & byte_mask;
            let mask1 = (word >> 48) as u8;
            // SAFETY: caller ensures `byte_mask == bits.len() - 1` and `bits.len()` is a power of two
            // (`<= MAX_TRIGRAM_BLOOM_BYTES == 16_384`), so `idx0 < bits.len()` and `idx1 < bits.len()`.
            unsafe {
                *ptr.add(idx0) |= mask0;
                *ptr.add(idx1) |= mask1;
            }
        }

        const SHUF0: [u8; 16] = [0, 1, 2, 255, 1, 2, 3, 255, 2, 3, 4, 255, 3, 4, 5, 255];
        const SHUF1: [u8; 16] = [4, 5, 6, 255, 5, 6, 7, 255, 6, 7, 8, 255, 7, 8, 9, 255];
        const SHUF2: [u8; 16] = [
            8, 9, 10, 255, 9, 10, 11, 255, 10, 11, 12, 255, 11, 12, 13, 255,
        ];

        // Packed `(idx | mask << 16)` layout requires `idx < 65_536`.
        const { assert!(MAX_TRIGRAM_BLOOM_BYTES <= 65_536) };
        if !self.bits.len().is_power_of_two() || self.bits.len() > MAX_TRIGRAM_BLOOM_BYTES {
            self.insert_bytes_scalar(bytes);
            return;
        }
        let byte_mask = self.bits.len() - 1;
        let bit_mask = (self.bits.len() * 8) - 1;
        let bits = &mut self.bits;
        let ptr = bits.as_mut_ptr();

        let cap_a = vdupq_n_u8(b'A');
        let alpha_len = vdupq_n_u8(26);
        let bit20 = vdupq_n_u8(0x20);
        // SAFETY: `SHUF0`, `SHUF1`, and `SHUF2` are `[u8; 16]`.
        let (shuf0, shuf1, shuf2) = unsafe {
            (
                vld1q_u8(SHUF0.as_ptr()),
                vld1q_u8(SHUF1.as_ptr()),
                vld1q_u8(SHUF2.as_ptr()),
            )
        };
        let c1 = vdupq_n_u32(0x85eb_ca6b);
        let c2 = vdupq_n_u32(0xc2b2_ae35);
        let mask_v = vdupq_n_u32(bit_mask as u32);
        let ones_v = vdupq_n_u32(1);
        let seven_v = vdupq_n_u32(7);

        let mut i = 0usize;
        while i + 16 <= bytes.len() {
            // SAFETY: `i + 16 <= bytes.len()`, so unaligned 16-byte load is within bounds.
            let v = unsafe { vld1q_u8(bytes.as_ptr().add(i)) };
            let is_upper = vcltq_u8(vsubq_u8(v, cap_a), alpha_len);
            let lowered = vorrq_u8(v, vandq_u8(is_upper, bit20));

            let mut x0 = vreinterpretq_u32_u8(vqtbl1q_u8(lowered, shuf0));
            let mut x1 = vreinterpretq_u32_u8(vqtbl1q_u8(lowered, shuf1));
            let mut x2 = vreinterpretq_u32_u8(vqtbl1q_u8(lowered, shuf2));

            x0 = veorq_u32(x0, vshrq_n_u32::<16>(x0));
            x1 = veorq_u32(x1, vshrq_n_u32::<16>(x1));
            x2 = veorq_u32(x2, vshrq_n_u32::<16>(x2));
            x0 = vmulq_u32(x0, c1);
            x1 = vmulq_u32(x1, c1);
            x2 = vmulq_u32(x2, c1);
            x0 = veorq_u32(x0, vshrq_n_u32::<13>(x0));
            x1 = veorq_u32(x1, vshrq_n_u32::<13>(x1));
            x2 = veorq_u32(x2, vshrq_n_u32::<13>(x2));
            x0 = vmulq_u32(x0, c2);
            x1 = vmulq_u32(x1, c2);
            x2 = vmulq_u32(x2, c2);
            x0 = veorq_u32(x0, vshrq_n_u32::<16>(x0));
            x1 = veorq_u32(x1, vshrq_n_u32::<16>(x1));
            x2 = veorq_u32(x2, vshrq_n_u32::<16>(x2));

            let p1_0 = vandq_u32(x0, mask_v);
            let p1_1 = vandq_u32(x1, mask_v);
            let p1_2 = vandq_u32(x2, mask_v);
            let rot16_0 = vreinterpretq_u32_u16(vrev32q_u16(vreinterpretq_u16_u32(x0)));
            let rot16_1 = vreinterpretq_u32_u16(vrev32q_u16(vreinterpretq_u16_u32(x1)));
            let rot16_2 = vreinterpretq_u32_u16(vrev32q_u16(vreinterpretq_u16_u32(x2)));
            let p2_0 = vandq_u32(rot16_0, mask_v);
            let p2_1 = vandq_u32(rot16_1, mask_v);
            let p2_2 = vandq_u32(rot16_2, mask_v);

            let idx1_0 = vshrq_n_u32::<3>(p1_0);
            let mask1_0 = vshlq_u32(ones_v, vreinterpretq_s32_u32(vandq_u32(p1_0, seven_v)));
            let idx1_1 = vshrq_n_u32::<3>(p1_1);
            let mask1_1 = vshlq_u32(ones_v, vreinterpretq_s32_u32(vandq_u32(p1_1, seven_v)));
            let idx1_2 = vshrq_n_u32::<3>(p1_2);
            let mask1_2 = vshlq_u32(ones_v, vreinterpretq_s32_u32(vandq_u32(p1_2, seven_v)));

            let idx2_0 = vshrq_n_u32::<3>(p2_0);
            let mask2_0 = vshlq_u32(ones_v, vreinterpretq_s32_u32(vandq_u32(p2_0, seven_v)));
            let idx2_1 = vshrq_n_u32::<3>(p2_1);
            let mask2_1 = vshlq_u32(ones_v, vreinterpretq_s32_u32(vandq_u32(p2_1, seven_v)));
            let idx2_2 = vshrq_n_u32::<3>(p2_2);
            let mask2_2 = vshlq_u32(ones_v, vreinterpretq_s32_u32(vandq_u32(p2_2, seven_v)));

            let v1_0 = vreinterpretq_u64_u32(vorrq_u32(idx1_0, vshlq_n_u32::<16>(mask1_0)));
            let v1_1 = vreinterpretq_u64_u32(vorrq_u32(idx1_1, vshlq_n_u32::<16>(mask1_1)));
            let v1_2 = vreinterpretq_u64_u32(vorrq_u32(idx1_2, vshlq_n_u32::<16>(mask1_2)));
            let v2_0 = vreinterpretq_u64_u32(vorrq_u32(idx2_0, vshlq_n_u32::<16>(mask2_0)));
            let v2_1 = vreinterpretq_u64_u32(vorrq_u32(idx2_1, vshlq_n_u32::<16>(mask2_1)));
            let v2_2 = vreinterpretq_u64_u32(vorrq_u32(idx2_2, vshlq_n_u32::<16>(mask2_2)));

            // SAFETY: `ptr` points to `bits` of length `byte_mask + 1` (power of two, `<= 16_384`).
            unsafe {
                set_two_lanes(ptr, byte_mask, vgetq_lane_u64::<0>(v1_0));
                set_two_lanes(ptr, byte_mask, vgetq_lane_u64::<1>(v1_0));
                set_two_lanes(ptr, byte_mask, vgetq_lane_u64::<0>(v1_1));
                set_two_lanes(ptr, byte_mask, vgetq_lane_u64::<1>(v1_1));
                set_two_lanes(ptr, byte_mask, vgetq_lane_u64::<0>(v1_2));
                set_two_lanes(ptr, byte_mask, vgetq_lane_u64::<1>(v1_2));
                set_two_lanes(ptr, byte_mask, vgetq_lane_u64::<0>(v2_0));
                set_two_lanes(ptr, byte_mask, vgetq_lane_u64::<1>(v2_0));
                set_two_lanes(ptr, byte_mask, vgetq_lane_u64::<0>(v2_1));
                set_two_lanes(ptr, byte_mask, vgetq_lane_u64::<1>(v2_1));
                set_two_lanes(ptr, byte_mask, vgetq_lane_u64::<0>(v2_2));
                set_two_lanes(ptr, byte_mask, vgetq_lane_u64::<1>(v2_2));
            }
            i += 12;
        }

        let end = bytes.len().saturating_sub(2);
        while i < end {
            let h = trigram_hash(
                bytes[i].to_ascii_lowercase(),
                bytes[i + 1].to_ascii_lowercase(),
                bytes[i + 2].to_ascii_lowercase(),
            );
            let p1 = (h as usize) & bit_mask;
            let p2 = (h.rotate_right(16) as usize) & bit_mask;
            bits[(p1 >> 3) & byte_mask] |= 1u8 << (p1 & 7);
            bits[(p2 >> 3) & byte_mask] |= 1u8 << (p2 & 7);
            i += 1;
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

    /// Folds the filter in half by OR-ing the upper half into the lower half.
    ///
    /// Valid when `self.bits.len() > MIN_TRIGRAM_BLOOM_BYTES`; the result is bit-identical
    /// to inserting the same trigrams directly into a half-sized [`TrigramBloom`].
    pub fn fold_half(&self) -> Self {
        if self.bits.len() <= MIN_TRIGRAM_BLOOM_BYTES {
            return self.clone();
        }
        let half = self.bits.len() / 2;
        let mut out = Vec::with_capacity(half);
        for i in 0..half {
            out.push(self.bits[i] | self.bits[i + half]);
        }
        Self { bits: out }
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
    let len = src.len().min(dst.len());
    // Map 'A'..='Z' (0x41..=0x5A) to -128..=-103 via wrapping +0x3F so signed
    // `cmpgt(-102, x)` selects strictly 'A'..='Z' and leaves non-ASCII untouched.
    let offset = _mm256_set1_epi8(0x3Fi8);
    let upper_bound = _mm256_set1_epi8(-102i8);
    let bit20 = _mm256_set1_epi8(0x20i8);

    let full_chunks = len / 32;
    for i in 0..full_chunks {
        let byte_off = i * 32;
        // SAFETY: `i * 32 + 32 <= len <= min(src.len(), dst.len())`; unaligned load/store are valid.
        unsafe {
            let v = _mm256_loadu_si256(src.as_ptr().add(byte_off) as *const __m256i);
            let shifted = _mm256_add_epi8(v, offset);
            let is_upper = _mm256_cmpgt_epi8(upper_bound, shifted);
            let lowered = _mm256_or_si256(v, _mm256_and_si256(is_upper, bit20));
            _mm256_storeu_si256(dst.as_mut_ptr().add(byte_off) as *mut __m256i, lowered);
        }
    }

    let rem_start = full_chunks * 32;
    if rem_start < len {
        ascii_lowercase_scalar(&src[rem_start..len], &mut dst[rem_start..len]);
    }
}

#[cfg(all(
    target_arch = "aarch64",
    target_feature = "neon",
    target_endian = "little",
    not(miri)
))]
#[target_feature(enable = "neon")]
fn ascii_lowercase_neon(src: &[u8], dst: &mut [u8]) {
    use std::arch::aarch64::*;

    debug_assert_eq!(src.len(), dst.len());
    let len = src.len().min(dst.len());
    let cap_a = vdupq_n_u8(b'A');
    let alpha_len = vdupq_n_u8(26);
    let bit20 = vdupq_n_u8(0x20);

    let full_chunks = len / 16;
    for i in 0..full_chunks {
        let byte_off = i * 16;
        // SAFETY: `i * 16 + 16 <= len <= min(src.len(), dst.len())`; byte load allows any alignment.
        let v = unsafe { vld1q_u8(src.as_ptr().add(byte_off)) };
        let is_upper = vcltq_u8(vsubq_u8(v, cap_a), alpha_len);
        let lowered = vorrq_u8(v, vandq_u8(is_upper, bit20));
        // SAFETY: `i * 16 + 16 <= len <= dst.len()`; byte store allows any alignment.
        unsafe {
            vst1q_u8(dst.as_mut_ptr().add(byte_off), lowered);
        }
    }

    let rem_start = full_chunks * 16;
    if rem_start < len {
        ascii_lowercase_scalar(&src[rem_start..len], &mut dst[rem_start..len]);
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
            TrigramBloom::from_bytes(vec![0u8; 32_768]),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(TrigramBloom::from_bytes(vec![0u8; 16_384]).is_ok());
    }

    #[test]
    fn trigram_bloom_fold_half_matches_direct_half_sized_insert() {
        let sample =
            b"{\"role\":\"assistant\",\"content\":\"Tool call trace: Bearer TOKEN_ABC_12345\"}";
        let mut big = TrigramBloom::new(16_384);
        big.insert_bytes(sample);

        let folded_8192 = big.fold_half();
        let mut direct_8192 = TrigramBloom::new(8_192);
        direct_8192.insert_bytes(sample);
        assert_eq!(folded_8192.as_bytes(), direct_8192.as_bytes());

        let folded_4096 = folded_8192.fold_half();
        let mut direct_4096 = TrigramBloom::new(4_096);
        direct_4096.insert_bytes(sample);
        assert_eq!(folded_4096.as_bytes(), direct_4096.as_bytes());

        let folded_2048 = folded_4096.fold_half();
        let mut direct_2048 = TrigramBloom::new(2048);
        direct_2048.insert_bytes(sample);
        assert_eq!(folded_2048.as_bytes(), direct_2048.as_bytes());

        let folded_1024 = folded_2048.fold_half();
        let mut direct_1024 = TrigramBloom::new(1024);
        direct_1024.insert_bytes(sample);
        assert_eq!(folded_1024.as_bytes(), direct_1024.as_bytes());

        let folded_256 = folded_1024.fold_half().fold_half();
        let mut direct_256 = TrigramBloom::new(256);
        direct_256.insert_bytes(sample);
        assert_eq!(folded_256.as_bytes(), direct_256.as_bytes());
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

        #[test]
        fn proptest_trigram_bloom_simd_scalar_parity(
            bytes in prop::collection::vec(any::<u8>(), 0..=512),
            cap_shift in 0usize..=6,
        ) {
            let cap = 256usize << cap_shift;
            let mut simd = TrigramBloom::new(cap);
            let mut scalar = TrigramBloom::new(cap);
            simd.insert_bytes(&bytes);
            scalar.insert_bytes_scalar(&bytes);
            prop_assert_eq!(simd.as_bytes(), scalar.as_bytes());
        }
    }
}
