//! Deterministic 64-bit bloom hash and 8-lane SIMD Split-Block Bloom Filter (`Sbbf`).

#![allow(unsafe_code)]

use std::fmt;

use crate::format::{BLOOM_RG_MAGIC, MAX_ROW_GROUPS};
use crate::{BLOOM_MAGIC, MAX_BLOOM_BYTES, StatamError};

/// Parquet Split-Block Bloom Filter 8-lane salt constants.
pub const SBBF_SALT: [u32; 8] = [
    0x47b6_137b,
    0x4497_4d91,
    0x8824_ad5b,
    0xa2b7_289d,
    0x7054_95c7,
    0x2df1_424b,
    0x9efc_4947,
    0x5c6b_fb31,
];

/// Wire header length of a serialized [`Sbbf`] (`BF01` + `num_blocks: u32` + `crc32c: u32`).
pub const SBBF_HEADER_LEN: usize = 12;

/// Deterministic, zero-dependency 64-bit hash over byte slices (`statam_bloom_hash`).
///
/// Uses a WyHash/SplitMix64-style 8-byte LE chunk mixer with tail and length
/// finalization so persisted SBBF filters are 100% stable across releases and
/// platforms.
pub fn statam_bloom_hash(bytes: &[u8]) -> u64 {
    const K0: u64 = 0xa076_1d64_78bd_642f;
    const K1: u64 = 0xe703_7ed1_a0b4_28db;
    const K2: u64 = 0x8ebc_6af0_9c88_c6e3;
    const K3: u64 = 0x5899_65cc_7537_4cc3;

    let len = bytes.len() as u64;
    let mut h = K0 ^ len.wrapping_mul(K1);

    let (chunks, rem) = bytes.as_chunks::<8>();
    for &chunk in chunks {
        let w = u64::from_le_bytes(chunk);
        h ^= w.wrapping_mul(K2);
        h = h.rotate_left(27).wrapping_mul(K3) ^ K1;
    }

    if !rem.is_empty() {
        let mut tail = [0u8; 8];
        tail[..rem.len()].copy_from_slice(rem);
        let w = u64::from_le_bytes(tail);
        h ^= w.wrapping_mul(K2);
        h = h.rotate_left(31).wrapping_mul(K3);
    }

    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

/// Parquet-compatible 256-bit Split-Block Bloom Filter (`8 × u32` per block).
#[derive(Clone, PartialEq, Eq)]
pub struct Sbbf {
    words: Vec<u32>,
    num_blocks: usize,
}

impl fmt::Debug for Sbbf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sbbf")
            .field("num_blocks", &self.num_blocks)
            .field("byte_len", &(self.num_blocks * 32))
            .finish()
    }
}

impl Sbbf {
    /// Allocates an empty SBBF sized for `ndv` distinct values at target `fpp`.
    pub fn new_for_ndv(ndv: usize, fpp: f64) -> Self {
        let ndv_f = ndv.max(1) as f64;
        let clamped_fpp = if fpp.is_finite() {
            fpp.clamp(1e-6, 0.5)
        } else {
            0.01
        };
        // Standard 8-probe split-block sizing with 1.25x Poisson load-variance margin.
        let denom = -(1.0 - clamped_fpp.powf(1.0 / 8.0)).ln();
        let bits = ((8.0 * ndv_f / denom) * 1.25).ceil();
        let max_blocks = (MAX_BLOOM_BYTES - SBBF_HEADER_LEN) / 32;
        let num_blocks = if bits.is_finite() && bits > 0.0 {
            ((bits as usize).div_ceil(256)).clamp(1, max_blocks)
        } else {
            1
        };
        Self {
            words: vec![0u32; num_blocks * 8],
            num_blocks,
        }
    }

    /// Number of 256-bit (32-byte) blocks in this filter.
    #[inline]
    pub fn num_blocks(&self) -> usize {
        self.num_blocks
    }

    /// Serialized byte length including the 12-byte `BF01` header.
    #[inline]
    pub fn encoded_len(&self) -> usize {
        SBBF_HEADER_LEN + self.num_blocks * 32
    }

    /// Inserts a precomputed 64-bit hash into the filter.
    #[inline]
    pub fn insert_hash(&mut self, h: u64) {
        let block_idx = self.block_index(h);
        let key = h as u32;
        let block = &mut self.words[block_idx * 8..(block_idx + 1) * 8];
        #[cfg(all(target_arch = "x86_64", not(miri)))]
        {
            if std::arch::is_x86_feature_detected!("avx2") {
                // SAFETY: `is_x86_feature_detected!("avx2")` verified above and
                // `block.len() == 8`.
                unsafe {
                    block_insert_avx2(block, key);
                }
                return;
            }
        }
        block_insert_scalar(block, key);
    }

    /// Hashes `bytes` with [`statam_bloom_hash`] and inserts it.
    #[inline]
    pub fn insert_bytes(&mut self, bytes: &[u8]) {
        self.insert_hash(statam_bloom_hash(bytes));
    }

    /// Returns `true` if `h` may be present in the filter (`false` guarantees absence).
    #[inline]
    pub fn contains_hash(&self, h: u64) -> bool {
        let block_idx = self.block_index(h);
        let key = h as u32;
        let block = &self.words[block_idx * 8..(block_idx + 1) * 8];
        #[cfg(all(target_arch = "x86_64", not(miri)))]
        {
            if std::arch::is_x86_feature_detected!("avx2") {
                // SAFETY: `is_x86_feature_detected!("avx2")` verified above and
                // `block.len() == 8`.
                return unsafe { block_check_avx2(block, key) };
            }
        }
        block_check_scalar(block, key)
    }

    /// Hashes `bytes` with [`statam_bloom_hash`] and checks membership.
    #[inline]
    pub fn contains_bytes(&self, bytes: &[u8]) -> bool {
        self.contains_hash(statam_bloom_hash(bytes))
    }

    /// Serializes the filter as `BF01 | num_blocks (u32 LE) | crc32c (u32 LE) | words LE`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let payload_len = self.num_blocks * 32;
        let mut out = Vec::with_capacity(SBBF_HEADER_LEN + payload_len);
        out.extend_from_slice(&BLOOM_MAGIC);
        out.extend_from_slice(&(self.num_blocks as u32).to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        for &w in &self.words {
            out.extend_from_slice(&w.to_le_bytes());
        }
        let crc = crc32c::crc32c(&out[SBBF_HEADER_LEN..]);
        out[8..12].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// Decodes and validates a serialized `BF01` split-block bloom filter.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, StatamError> {
        if buf.len() < SBBF_HEADER_LEN {
            return Err(StatamError::Corrupt {
                reason: "truncated bloom filter header",
            });
        }
        if buf.len() > MAX_BLOOM_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "bloom filter exceeds MAX_BLOOM_BYTES",
            });
        }
        if buf[0..4] != BLOOM_MAGIC {
            return Err(StatamError::Corrupt {
                reason: "invalid bloom filter magic",
            });
        }
        let num_blocks = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        if num_blocks == 0 {
            return Err(StatamError::Corrupt {
                reason: "zero-block bloom filter",
            });
        }
        let payload_bytes = num_blocks
            .checked_mul(32)
            .ok_or(StatamError::BoundsExceeded {
                reason: "bloom filter byte length overflow",
            })?;
        let total_len = SBBF_HEADER_LEN
            .checked_add(payload_bytes)
            .filter(|&len| len <= MAX_BLOOM_BYTES)
            .ok_or(StatamError::BoundsExceeded {
                reason: "bloom filter exceeds MAX_BLOOM_BYTES",
            })?;
        if buf.len() != total_len {
            return Err(StatamError::Corrupt {
                reason: "bloom filter byte length mismatch",
            });
        }

        let expected_crc = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let actual_crc = crc32c::crc32c(&buf[SBBF_HEADER_LEN..]);
        if expected_crc != actual_crc {
            return Err(StatamError::ChecksumMismatch {
                context: "bloom filter",
            });
        }

        let word_count = num_blocks * 8;
        let mut words = Vec::with_capacity(word_count);
        for &chunk in buf[SBBF_HEADER_LEN..].as_chunks::<4>().0 {
            words.push(u32::from_le_bytes(chunk));
        }
        Ok(Self { words, num_blocks })
    }

    #[inline]
    fn block_index(&self, h: u64) -> usize {
        let hi = (h >> 32) & 0xFFFF_FFFF;
        ((hi * (self.num_blocks as u64)) >> 32) as usize
    }
}

/// File-level (`BF01`) or per-Row-Group (`BF02`) SBBF bloom index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BloomIndex {
    /// Legacy single file-wide SBBF (`BF01`).
    File(Sbbf),
    /// Per-Row-Group SBBF array (`BF02`).
    PerRowGroup(Vec<Sbbf>),
}

impl BloomIndex {
    /// Returns `true` if any Row Group (or the file filter) may contain at least one hash in `hashes`.
    #[inline]
    pub fn contains_any_hash(&self, hashes: &[u64]) -> bool {
        if hashes.is_empty() {
            return false;
        }
        match self {
            Self::File(sbbf) => hashes.iter().any(|&h| sbbf.contains_hash(h)),
            Self::PerRowGroup(rgs) => rgs
                .iter()
                .any(|sbbf| hashes.iter().any(|&h| sbbf.contains_hash(h))),
        }
    }

    /// Returns `true` if Row Group `rg_idx` may contain at least one hash in `hashes`.
    #[inline]
    pub fn rg_may_contain_any_hash(&self, rg_idx: usize, hashes: &[u64]) -> bool {
        if hashes.is_empty() {
            return false;
        }
        match self {
            Self::File(sbbf) => hashes.iter().any(|&h| sbbf.contains_hash(h)),
            Self::PerRowGroup(rgs) => rgs
                .get(rg_idx)
                .is_none_or(|sbbf| hashes.iter().any(|&h| sbbf.contains_hash(h))),
        }
    }

    /// Serializes as `BF01` (`File`) or `BF02` (`PerRowGroup`).
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::File(sbbf) => sbbf.to_bytes(),
            Self::PerRowGroup(rgs) => {
                let num_rgs = rgs.len();
                let header_and_lens = SBBF_HEADER_LEN + num_rgs * 4;
                let payload_sum: usize = rgs.iter().map(Sbbf::encoded_len).sum();
                let mut out = Vec::with_capacity(header_and_lens + payload_sum);
                out.extend_from_slice(&BLOOM_RG_MAGIC);
                out.extend_from_slice(&(num_rgs as u32).to_le_bytes());
                out.extend_from_slice(&[0u8; 4]);
                for rg in rgs {
                    out.extend_from_slice(&(rg.encoded_len() as u32).to_le_bytes());
                }
                for rg in rgs {
                    out.extend_from_slice(&rg.to_bytes());
                }
                let crc = crc32c::crc32c(&out[SBBF_HEADER_LEN..]);
                out[8..12].copy_from_slice(&crc.to_le_bytes());
                out
            }
        }
    }

    /// Decodes either a `BF01` file-wide bloom or a `BF02` per-RG bloom index.
    pub fn from_bytes(src: &[u8]) -> Result<Self, StatamError> {
        if src.len() < SBBF_HEADER_LEN {
            return Err(StatamError::Corrupt {
                reason: "truncated bloom index header",
            });
        }
        if src.len() > MAX_BLOOM_BYTES {
            return Err(StatamError::BoundsExceeded {
                reason: "bloom index exceeds MAX_BLOOM_BYTES",
            });
        }

        if src[0..4] == BLOOM_MAGIC {
            return Ok(Self::File(Sbbf::from_bytes(src)?));
        }
        if src[0..4] != BLOOM_RG_MAGIC {
            return Err(StatamError::Corrupt {
                reason: "invalid bloom index magic",
            });
        }

        let num_rgs = u32::from_le_bytes([src[4], src[5], src[6], src[7]]) as usize;
        if num_rgs == 0 {
            return Err(StatamError::Corrupt {
                reason: "zero-row-group bloom index",
            });
        }
        if num_rgs > MAX_ROW_GROUPS {
            return Err(StatamError::BoundsExceeded {
                reason: "per-rg bloom num_rgs exceeds MAX_ROW_GROUPS",
            });
        }

        let lens_bytes = num_rgs.checked_mul(4).ok_or(StatamError::BoundsExceeded {
            reason: "per-rg bloom length table overflow",
        })?;
        let data_start =
            SBBF_HEADER_LEN
                .checked_add(lens_bytes)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "per-rg bloom header size overflow",
                })?;
        if data_start > src.len() {
            return Err(StatamError::Corrupt {
                reason: "truncated per-rg bloom length table",
            });
        }

        let expected_crc = u32::from_le_bytes([src[8], src[9], src[10], src[11]]);
        let actual_crc = crc32c::crc32c(&src[SBBF_HEADER_LEN..]);
        if expected_crc != actual_crc {
            return Err(StatamError::ChecksumMismatch {
                context: "per-rg bloom index",
            });
        }

        let mut sum_len: usize = 0;
        for &chunk in src[SBBF_HEADER_LEN..data_start].as_chunks::<4>().0 {
            let entry_len = u32::from_le_bytes(chunk) as usize;
            if entry_len < SBBF_HEADER_LEN {
                return Err(StatamError::Corrupt {
                    reason: "per-rg bloom entry shorter than BF01 header",
                });
            }
            sum_len = sum_len
                .checked_add(entry_len)
                .ok_or(StatamError::BoundsExceeded {
                    reason: "per-rg bloom total length overflow",
                })?;
        }
        if sum_len > MAX_BLOOM_BYTES || data_start.checked_add(sum_len) != Some(src.len()) {
            return Err(StatamError::Corrupt {
                reason: "per-rg bloom total length mismatch",
            });
        }

        let mut rgs = Vec::with_capacity(num_rgs);
        let mut pos = data_start;
        for &chunk in src[SBBF_HEADER_LEN..data_start].as_chunks::<4>().0 {
            let entry_len = u32::from_le_bytes(chunk) as usize;
            let end = pos + entry_len;
            rgs.push(Sbbf::from_bytes(&src[pos..end])?);
            pos = end;
        }
        Ok(Self::PerRowGroup(rgs))
    }
}

#[inline]
fn block_insert_scalar(block: &mut [u32], key: u32) {
    for (slot, &salt) in block.iter_mut().zip(&SBBF_SALT) {
        let bit = key.wrapping_mul(salt) >> 27;
        *slot |= 1u32 << bit;
    }
}

#[inline]
fn block_check_scalar(block: &[u32], key: u32) -> bool {
    for (&word, &salt) in block.iter().zip(&SBBF_SALT) {
        let bit = key.wrapping_mul(salt) >> 27;
        if (word & (1u32 << bit)) == 0 {
            return false;
        }
    }
    true
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn sbbf_mask_avx2(key: u32) -> std::arch::x86_64::__m256i {
    use std::arch::x86_64::*;

    // SAFETY: `SBBF_SALT` is a `[u32; 8]` (32 bytes); unaligned load is valid.
    let salt = unsafe { _mm256_loadu_si256(SBBF_SALT.as_ptr() as *const __m256i) };
    let k = _mm256_set1_epi32(key as i32);
    let shifts = _mm256_srli_epi32::<27>(_mm256_mullo_epi32(k, salt));
    _mm256_sllv_epi32(_mm256_set1_epi32(1), shifts)
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn block_insert_avx2(block: &mut [u32], key: u32) {
    use std::arch::x86_64::*;

    debug_assert_eq!(block.len(), 8);
    // SAFETY: AVX2 is active; `block.len() == 8` (32 bytes); unaligned load/store are valid.
    unsafe {
        let mask = sbbf_mask_avx2(key);
        let cur = _mm256_loadu_si256(block.as_ptr() as *const __m256i);
        let updated = _mm256_or_si256(cur, mask);
        _mm256_storeu_si256(block.as_mut_ptr() as *mut __m256i, updated);
    }
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn block_check_avx2(block: &[u32], key: u32) -> bool {
    use std::arch::x86_64::*;

    debug_assert_eq!(block.len(), 8);
    // SAFETY: AVX2 is active; `block.len() == 8` (32 bytes); unaligned load is valid.
    unsafe {
        let mask = sbbf_mask_avx2(key);
        let cur = _mm256_loadu_si256(block.as_ptr() as *const __m256i);
        _mm256_testc_si256(cur, mask) != 0
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn sbbf_simd_matches_scalar_zero_false_negatives_and_fpp_bound() {
        let ndv = 5_000usize;
        let mut simd_bf = Sbbf::new_for_ndv(ndv, 0.01);
        let mut scalar_bf = Sbbf::new_for_ndv(ndv, 0.01);

        for i in 0..ndv {
            let key = format!("trace-{i:08x}");
            let h = statam_bloom_hash(key.as_bytes());
            simd_bf.insert_hash(h);

            let b = scalar_bf.block_index(h);
            block_insert_scalar(&mut scalar_bf.words[b * 8..(b + 1) * 8], h as u32);
        }

        assert_eq!(simd_bf, scalar_bf, "SBBF insert SIMD != scalar");

        for i in 0..ndv {
            let key = format!("trace-{i:08x}");
            assert!(simd_bf.contains_bytes(key.as_bytes()));
            let h = statam_bloom_hash(key.as_bytes());
            let b = scalar_bf.block_index(h);
            assert!(block_check_scalar(
                &scalar_bf.words[b * 8..(b + 1) * 8],
                h as u32
            ));
        }

        let probes = 20_000usize;
        let mut fp = 0usize;
        for i in 0..probes {
            let probe = format!("absent-{i:08x}");
            let h = statam_bloom_hash(probe.as_bytes());
            let simd_hit = simd_bf.contains_hash(h);
            let b = scalar_bf.block_index(h);
            let scalar_hit = block_check_scalar(&scalar_bf.words[b * 8..(b + 1) * 8], h as u32);
            assert_eq!(simd_hit, scalar_hit);
            if simd_hit {
                fp += 1;
            }
        }
        let observed_fpp = (fp as f64) / (probes as f64);
        assert!(
            observed_fpp <= 0.015,
            "observed FPP {observed_fpp:.4} exceeded target bound"
        );

        // Wire roundtrip & corruption checks.
        let encoded = simd_bf.to_bytes();
        let decoded = Sbbf::from_bytes(&encoded).unwrap();
        assert_eq!(decoded, simd_bf);

        let mut bad_crc = encoded.clone();
        *bad_crc.last_mut().unwrap() ^= 1;
        assert!(matches!(
            Sbbf::from_bytes(&bad_crc),
            Err(StatamError::ChecksumMismatch { .. })
        ));

        let mut bad_magic = encoded;
        bad_magic[0] = b'X';
        assert!(matches!(
            Sbbf::from_bytes(&bad_magic),
            Err(StatamError::Corrupt { .. })
        ));

        let mut zero_blocks = [0u8; 12];
        zero_blocks[0..4].copy_from_slice(&BLOOM_MAGIC);
        assert!(matches!(
            Sbbf::from_bytes(&zero_blocks),
            Err(StatamError::Corrupt { .. })
        ));

        let mut oversized = [0u8; 12];
        oversized[0..4].copy_from_slice(&BLOOM_MAGIC);
        let too_many_blocks = (MAX_BLOOM_BYTES / 32) as u32;
        oversized[4..8].copy_from_slice(&too_many_blocks.to_le_bytes());
        assert!(matches!(
            Sbbf::from_bytes(&oversized),
            Err(StatamError::BoundsExceeded { .. })
        ));
        assert!(Sbbf::new_for_ndv(usize::MAX, 1e-6).encoded_len() <= MAX_BLOOM_BYTES);
    }

    #[test]
    fn bloom_index_bf01_and_bf02_roundtrip_and_bounds_checks() {
        let mut bf0 = Sbbf::new_for_ndv(64, 0.01);
        let mut bf1 = Sbbf::new_for_ndv(64, 0.01);
        let h_rg0 = statam_bloom_hash(b"trace-rg0");
        let h_rg1 = statam_bloom_hash(b"trace-rg1");
        let h_absent = statam_bloom_hash(b"trace-none");
        bf0.insert_hash(h_rg0);
        bf1.insert_hash(h_rg1);

        // BF01 compatibility through BloomIndex
        let b1 = bf0.to_bytes();
        let idx1 = BloomIndex::from_bytes(&b1).unwrap();
        assert!(matches!(idx1, BloomIndex::File(_)));
        assert!(idx1.contains_any_hash(&[h_rg0]));
        assert!(idx1.rg_may_contain_any_hash(0, &[h_rg0]));

        // BF02 roundtrip and per-RG pruning
        let idx2 = BloomIndex::PerRowGroup(vec![bf0, bf1]);
        let b2 = idx2.to_bytes();
        assert_eq!(&b2[0..4], &BLOOM_RG_MAGIC);
        let dec2 = BloomIndex::from_bytes(&b2).unwrap();
        assert_eq!(dec2, idx2);
        assert!(dec2.contains_any_hash(&[h_rg0]));
        assert!(dec2.contains_any_hash(&[h_rg1]));
        assert!(!dec2.contains_any_hash(&[h_absent]));
        assert!(dec2.rg_may_contain_any_hash(0, &[h_rg0]));
        assert!(!dec2.rg_may_contain_any_hash(1, &[h_rg0]));
        assert!(dec2.rg_may_contain_any_hash(1, &[h_rg1]));
        assert!(!dec2.rg_may_contain_any_hash(0, &[h_rg1]));

        // Corruption & bounds checks
        let mut bad_crc = b2;
        *bad_crc.last_mut().unwrap() ^= 1;
        assert!(matches!(
            BloomIndex::from_bytes(&bad_crc),
            Err(StatamError::ChecksumMismatch { .. })
        ));

        let mut zero_rgs = [0u8; 12];
        zero_rgs[0..4].copy_from_slice(&BLOOM_RG_MAGIC);
        assert!(matches!(
            BloomIndex::from_bytes(&zero_rgs),
            Err(StatamError::Corrupt { .. })
        ));

        let mut huge_rgs = [0u8; 12];
        huge_rgs[0..4].copy_from_slice(&BLOOM_RG_MAGIC);
        huge_rgs[4..8].copy_from_slice(&((MAX_ROW_GROUPS + 1) as u32).to_le_bytes());
        assert!(matches!(
            BloomIndex::from_bytes(&huge_rgs),
            Err(StatamError::BoundsExceeded { .. })
        ));
    }

    proptest! {
        #[test]
        fn proptest_sbbf_block_parity(
            init_words in prop::collection::vec(any::<u32>(), 8..=8),
            keys in prop::collection::vec(any::<u64>(), 1..=32),
            probes in prop::collection::vec(any::<u64>(), 1..=32),
        ) {
            let mut bf_simd = Sbbf {
                words: init_words.clone(),
                num_blocks: 1,
            };
            let mut bf_scalar = Sbbf {
                words: init_words,
                num_blocks: 1,
            };
            for &h in &keys {
                bf_simd.insert_hash(h);
                block_insert_scalar(&mut bf_scalar.words, h as u32);
            }
            prop_assert_eq!(&bf_simd.words, &bf_scalar.words);
            for &h in &probes {
                let s1 = bf_simd.contains_hash(h);
                let s2 = block_check_scalar(&bf_scalar.words, h as u32);
                prop_assert_eq!(s1, s2);
            }
        }
    }
}
