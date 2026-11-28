//! Audited SIMD kernels with CPU dispatch and safe scalar fallbacks.
//!
//! Every kernel dispatches to AVX2 on `x86_64` (when `is_x86_feature_detected!("avx2")`
//! holds) or little-endian NEON on `aarch64` (when `target_feature = "neon"` is enabled)
//! under `#[cfg(not(miri))]`, and falls back to a 100% safe portable scalar implementation
//! otherwise.

pub mod bitpack;
pub mod bloom;
pub mod filter;
pub mod trigram;

pub use bitpack::{
    FASTLANES_ORDER_8, FASTLANES_TILE_SIZE, delta_decode_i64, delta_encode_i64, min_bits_u32,
    min_bits_u64, pack_u32, pack_u64, packed_bytes_len, unpack_u32, unpack_u64, zigzag_decode_i64,
    zigzag_encode_i64,
};
pub use bloom::{SBBF_SALT, Sbbf, statam_bloom_hash};
pub use filter::{FilterOp, filter_f64, filter_i64, filter_i64_between, filter_u32};
pub use trigram::{TrigramBloom, ascii_lowercase_into, extract_trigram_hashes, trigram_hash};
