//! FastLanes-1024 interleaved SIMD bit-packing and `04261537` parallel delta kernels.

#![allow(unsafe_code)]

use crate::{MAX_BLOCK_RAW_BYTES, StatamError};

/// Number of elements in a full FastLanes tile.
pub const FASTLANES_TILE_SIZE: usize = 1024;

/// FastLanes `04261537` 8-lane permutation for parallel `i64` delta prefix-sum.
pub const FASTLANES_ORDER_8: [usize; 8] = [0, 4, 2, 6, 1, 5, 3, 7];

/// Computes the minimum bit width (`0..=32`) required to represent all `u32` values.
#[inline]
pub fn min_bits_u32(values: &[u32]) -> u8 {
    let max_val = values.iter().copied().max().unwrap_or(0);
    if max_val == 0 {
        0
    } else {
        (32 - max_val.leading_zeros()) as u8
    }
}

/// Computes the minimum bit width (`0..=64`) required to represent all `u64` values.
#[inline]
pub fn min_bits_u64(values: &[u64]) -> u8 {
    let max_val = values.iter().copied().max().unwrap_or(0);
    if max_val == 0 {
        0
    } else {
        (64 - max_val.leading_zeros()) as u8
    }
}

/// Returns the exact encoded byte length for `count` elements at `bit_width`.
///
/// Full 1024-value tiles occupy `128 * bit_width` bytes each; any trailing
/// `count % 1024` elements occupy `(rem * bit_width).div_ceil(8)` bytes with
/// zero padding to 1024.
#[inline]
pub fn packed_bytes_len(count: usize, bit_width: u8) -> Result<usize, StatamError> {
    if bit_width == 0 || count == 0 {
        return Ok(0);
    }
    let bw = usize::from(bit_width);
    let full_tiles = count / FASTLANES_TILE_SIZE;
    let rem = count % FASTLANES_TILE_SIZE;
    let tile_bytes = full_tiles
        .checked_mul(128)
        .and_then(|v| v.checked_mul(bw))
        .ok_or(StatamError::BoundsExceeded {
            reason: "bitpack byte length overflow",
        })?;
    let tail_bytes =
        rem.checked_mul(bw)
            .map(|bits| bits.div_ceil(8))
            .ok_or(StatamError::BoundsExceeded {
                reason: "bitpack tail byte length overflow",
            })?;
    tile_bytes
        .checked_add(tail_bytes)
        .ok_or(StatamError::BoundsExceeded {
            reason: "bitpack total byte length overflow",
        })
}

/// Packs `values` at `bit_width` (`0..=32`) using FastLanes-1024 8-lane interleaved
/// tiles plus a compact unpadded scalar tail, appending to `out`.
pub fn pack_u32(values: &[u32], bit_width: u8, out: &mut Vec<u8>) -> Result<(), StatamError> {
    if bit_width > 32 {
        return Err(StatamError::Corrupt {
            reason: "invalid u32 bit_width",
        });
    }
    if bit_width == 0 || values.is_empty() {
        return Ok(());
    }

    let needed = packed_bytes_len(values.len(), bit_width)?;
    let start = out.len();
    out.resize(start + needed, 0);
    let dst = &mut out[start..start + needed];

    let full_tiles = values.len() / FASTLANES_TILE_SIZE;
    let tile_src_len = full_tiles * FASTLANES_TILE_SIZE;
    let tile_dst_len = full_tiles * 128 * usize::from(bit_width);

    if full_tiles > 0 {
        #[cfg(all(target_arch = "x86_64", not(miri)))]
        {
            if std::arch::is_x86_feature_detected!("avx2") {
                // SAFETY: `is_x86_feature_detected!("avx2")` verified above,
                // `tile_src_len == full_tiles * 1024 <= values.len()`, and
                // `tile_dst_len == full_tiles * 128 * bit_width <= dst.len()`.
                unsafe {
                    pack_u32_tiles_avx2(
                        &values[..tile_src_len],
                        bit_width,
                        &mut dst[..tile_dst_len],
                    );
                }
            } else {
                pack_u32_tiles_scalar(&values[..tile_src_len], bit_width, &mut dst[..tile_dst_len]);
            }
        }
        #[cfg(not(all(target_arch = "x86_64", not(miri))))]
        {
            pack_u32_tiles_scalar(&values[..tile_src_len], bit_width, &mut dst[..tile_dst_len]);
        }
    }

    let tail = &values[tile_src_len..];
    if !tail.is_empty() {
        pack_u32_tail_scalar(tail, bit_width, &mut dst[tile_dst_len..]);
    }
    Ok(())
}

/// Unpacks `count` `u32` values at `bit_width` (`0..=32`) from `src`, appending to `out`.
pub fn unpack_u32(
    src: &[u8],
    count: usize,
    bit_width: u8,
    out: &mut Vec<u32>,
) -> Result<(), StatamError> {
    if bit_width > 32 {
        return Err(StatamError::Corrupt {
            reason: "invalid u32 bit_width",
        });
    }
    if count
        .checked_mul(size_of::<u32>())
        .is_none_or(|bytes| bytes > MAX_BLOCK_RAW_BYTES)
    {
        return Err(StatamError::BoundsExceeded {
            reason: "unpack_u32 count exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    let needed = packed_bytes_len(count, bit_width)?;
    if src.len() < needed {
        return Err(StatamError::Corrupt {
            reason: "truncated u32 bitpack input",
        });
    }
    if count == 0 {
        return Ok(());
    }

    let start = out.len();
    out.resize(start + count, 0);
    if bit_width == 0 {
        return Ok(());
    }

    let dst = &mut out[start..start + count];
    let full_tiles = count / FASTLANES_TILE_SIZE;
    let tile_dst_len = full_tiles * FASTLANES_TILE_SIZE;
    let tile_src_len = full_tiles * 128 * usize::from(bit_width);

    if full_tiles > 0 {
        #[cfg(all(target_arch = "x86_64", not(miri)))]
        {
            if std::arch::is_x86_feature_detected!("avx2") {
                // SAFETY: `is_x86_feature_detected!("avx2")` verified above,
                // `tile_src_len == full_tiles * 128 * bit_width <= src.len()`, and
                // `tile_dst_len == full_tiles * 1024 <= dst.len()`.
                unsafe {
                    unpack_u32_tiles_avx2(
                        &src[..tile_src_len],
                        bit_width,
                        &mut dst[..tile_dst_len],
                    );
                }
            } else {
                unpack_u32_tiles_scalar(&src[..tile_src_len], bit_width, &mut dst[..tile_dst_len]);
            }
        }
        #[cfg(not(all(target_arch = "x86_64", not(miri))))]
        {
            unpack_u32_tiles_scalar(&src[..tile_src_len], bit_width, &mut dst[..tile_dst_len]);
        }
    }

    let tail_count = count - tile_dst_len;
    if tail_count > 0 {
        unpack_u32_tail_scalar(
            &src[tile_src_len..needed],
            bit_width,
            &mut dst[tile_dst_len..],
        );
    }
    Ok(())
}

/// Packs `values` at `bit_width` (`0..=64`) using FastLanes-1024 4-lane interleaved
/// tiles plus a compact unpadded scalar tail, appending to `out`.
pub fn pack_u64(values: &[u64], bit_width: u8, out: &mut Vec<u8>) -> Result<(), StatamError> {
    if bit_width > 64 {
        return Err(StatamError::Corrupt {
            reason: "invalid u64 bit_width",
        });
    }
    if bit_width == 0 || values.is_empty() {
        return Ok(());
    }

    let needed = packed_bytes_len(values.len(), bit_width)?;
    let start = out.len();
    out.resize(start + needed, 0);
    let dst = &mut out[start..start + needed];

    let full_tiles = values.len() / FASTLANES_TILE_SIZE;
    let tile_src_len = full_tiles * FASTLANES_TILE_SIZE;
    let tile_dst_len = full_tiles * 128 * usize::from(bit_width);

    if full_tiles > 0 {
        #[cfg(all(target_arch = "x86_64", not(miri)))]
        {
            if std::arch::is_x86_feature_detected!("avx2") {
                // SAFETY: `is_x86_feature_detected!("avx2")` verified above,
                // `tile_src_len == full_tiles * 1024 <= values.len()`, and
                // `tile_dst_len == full_tiles * 128 * bit_width <= dst.len()`.
                unsafe {
                    pack_u64_tiles_avx2(
                        &values[..tile_src_len],
                        bit_width,
                        &mut dst[..tile_dst_len],
                    );
                }
            } else {
                pack_u64_tiles_scalar(&values[..tile_src_len], bit_width, &mut dst[..tile_dst_len]);
            }
        }
        #[cfg(not(all(target_arch = "x86_64", not(miri))))]
        {
            pack_u64_tiles_scalar(&values[..tile_src_len], bit_width, &mut dst[..tile_dst_len]);
        }
    }

    let tail = &values[tile_src_len..];
    if !tail.is_empty() {
        pack_u64_tail_scalar(tail, bit_width, &mut dst[tile_dst_len..]);
    }
    Ok(())
}

/// Unpacks `count` `u64` values at `bit_width` (`0..=64`) from `src`, appending to `out`.
pub fn unpack_u64(
    src: &[u8],
    count: usize,
    bit_width: u8,
    out: &mut Vec<u64>,
) -> Result<(), StatamError> {
    if bit_width > 64 {
        return Err(StatamError::Corrupt {
            reason: "invalid u64 bit_width",
        });
    }
    if count
        .checked_mul(size_of::<u64>())
        .is_none_or(|bytes| bytes > MAX_BLOCK_RAW_BYTES)
    {
        return Err(StatamError::BoundsExceeded {
            reason: "unpack_u64 count exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    let needed = packed_bytes_len(count, bit_width)?;
    if src.len() < needed {
        return Err(StatamError::Corrupt {
            reason: "truncated u64 bitpack input",
        });
    }
    if count == 0 {
        return Ok(());
    }

    let start = out.len();
    out.resize(start + count, 0);
    if bit_width == 0 {
        return Ok(());
    }

    let dst = &mut out[start..start + count];
    let full_tiles = count / FASTLANES_TILE_SIZE;
    let tile_dst_len = full_tiles * FASTLANES_TILE_SIZE;
    let tile_src_len = full_tiles * 128 * usize::from(bit_width);

    if full_tiles > 0 {
        #[cfg(all(target_arch = "x86_64", not(miri)))]
        {
            if std::arch::is_x86_feature_detected!("avx2") {
                // SAFETY: `is_x86_feature_detected!("avx2")` verified above,
                // `tile_src_len == full_tiles * 128 * bit_width <= src.len()`, and
                // `tile_dst_len == full_tiles * 1024 <= dst.len()`.
                unsafe {
                    unpack_u64_tiles_avx2(
                        &src[..tile_src_len],
                        bit_width,
                        &mut dst[..tile_dst_len],
                    );
                }
            } else {
                unpack_u64_tiles_scalar(&src[..tile_src_len], bit_width, &mut dst[..tile_dst_len]);
            }
        }
        #[cfg(not(all(target_arch = "x86_64", not(miri))))]
        {
            unpack_u64_tiles_scalar(&src[..tile_src_len], bit_width, &mut dst[..tile_dst_len]);
        }
    }

    let tail_count = count - tile_dst_len;
    if tail_count > 0 {
        unpack_u64_tail_scalar(
            &src[tile_src_len..needed],
            bit_width,
            &mut dst[tile_dst_len..],
        );
    }
    Ok(())
}

/// ZigZag-encodes a signed `i64` into an unsigned `u64`.
#[inline]
pub fn zigzag_encode_i64(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

/// ZigZag-decodes an unsigned `u64` back to a signed `i64`.
#[inline]
pub fn zigzag_decode_i64(v: u64) -> i64 {
    ((v >> 1) as i64) ^ (-((v & 1) as i64))
}

/// Returns the expected number of base values for `count` delta-encoded `i64`s.
#[inline]
pub fn delta_bases_len(count: usize) -> usize {
    let full_tiles = count / FASTLANES_TILE_SIZE;
    let rem = count % FASTLANES_TILE_SIZE;
    full_tiles * 8 + usize::from(rem > 0)
}

/// Delta-encodes `values` using FastLanes `04261537` 8-lane independent bases per
/// 1024-value tile plus a sequential scalar tail, appending to `bases_out` and `deltas_out`.
pub fn delta_encode_i64(values: &[i64], bases_out: &mut Vec<i64>, deltas_out: &mut Vec<i64>) {
    if values.is_empty() {
        return;
    }

    let full_tiles = values.len() / FASTLANES_TILE_SIZE;
    let rem = values.len() % FASTLANES_TILE_SIZE;
    let needed_bases = full_tiles * 8 + usize::from(rem > 0);
    let base_start = bases_out.len();
    let delta_start = deltas_out.len();
    bases_out.resize(base_start + needed_bases, 0);
    deltas_out.resize(delta_start + values.len(), 0);

    let tile_val_len = full_tiles * FASTLANES_TILE_SIZE;
    let tile_base_len = full_tiles * 8;

    if full_tiles > 0 {
        #[cfg(all(target_arch = "x86_64", not(miri)))]
        {
            if std::arch::is_x86_feature_detected!("avx2") {
                // SAFETY: `is_x86_feature_detected!("avx2")` verified above and
                // output slices are pre-sized to `full_tiles * 8` and `full_tiles * 1024`.
                unsafe {
                    delta_encode_i64_tiles_avx2(
                        &values[..tile_val_len],
                        &mut bases_out[base_start..base_start + tile_base_len],
                        &mut deltas_out[delta_start..delta_start + tile_val_len],
                    );
                }
            } else {
                delta_encode_i64_tiles_scalar(
                    &values[..tile_val_len],
                    &mut bases_out[base_start..base_start + tile_base_len],
                    &mut deltas_out[delta_start..delta_start + tile_val_len],
                );
            }
        }
        #[cfg(not(all(target_arch = "x86_64", not(miri))))]
        {
            delta_encode_i64_tiles_scalar(
                &values[..tile_val_len],
                &mut bases_out[base_start..base_start + tile_base_len],
                &mut deltas_out[delta_start..delta_start + tile_val_len],
            );
        }
    }

    if rem > 0 {
        let tail = &values[tile_val_len..];
        bases_out[base_start + tile_base_len] = tail[0];
        let tail_deltas = &mut deltas_out[delta_start + tile_val_len..];
        tail_deltas[0] = 0;
        for i in 1..rem {
            tail_deltas[i] = tail[i].wrapping_sub(tail[i - 1]);
        }
    }
}

/// Delta-decodes `count` `i64` values from `bases` and `deltas`, appending to `out`.
pub fn delta_decode_i64(
    bases: &[i64],
    deltas: &[i64],
    count: usize,
    out: &mut Vec<i64>,
) -> Result<(), StatamError> {
    if count
        .checked_mul(size_of::<i64>())
        .is_none_or(|bytes| bytes > MAX_BLOCK_RAW_BYTES)
    {
        return Err(StatamError::BoundsExceeded {
            reason: "delta_decode_i64 count exceeds MAX_BLOCK_RAW_BYTES",
        });
    }
    let expected_bases = delta_bases_len(count);
    if bases.len() < expected_bases || deltas.len() < count {
        return Err(StatamError::Corrupt {
            reason: "truncated delta_i64 input",
        });
    }
    if count == 0 {
        return Ok(());
    }

    let start = out.len();
    out.resize(start + count, 0);
    let dst = &mut out[start..start + count];

    let full_tiles = count / FASTLANES_TILE_SIZE;
    let rem = count % FASTLANES_TILE_SIZE;
    let tile_val_len = full_tiles * FASTLANES_TILE_SIZE;
    let tile_base_len = full_tiles * 8;

    if full_tiles > 0 {
        #[cfg(all(target_arch = "x86_64", not(miri)))]
        {
            if std::arch::is_x86_feature_detected!("avx2") {
                // SAFETY: `is_x86_feature_detected!("avx2")` verified above and
                // input/output slices are exact multiples of `full_tiles`.
                unsafe {
                    delta_decode_i64_tiles_avx2(
                        &bases[..tile_base_len],
                        &deltas[..tile_val_len],
                        &mut dst[..tile_val_len],
                    );
                }
            } else {
                delta_decode_i64_tiles_scalar(
                    &bases[..tile_base_len],
                    &deltas[..tile_val_len],
                    &mut dst[..tile_val_len],
                );
            }
        }
        #[cfg(not(all(target_arch = "x86_64", not(miri))))]
        {
            delta_decode_i64_tiles_scalar(
                &bases[..tile_base_len],
                &deltas[..tile_val_len],
                &mut dst[..tile_val_len],
            );
        }
    }

    if rem > 0 {
        let mut acc = bases[tile_base_len];
        let tail_deltas = &deltas[tile_val_len..count];
        let tail_dst = &mut dst[tile_val_len..];
        for i in 0..rem {
            acc = acc.wrapping_add(tail_deltas[i]);
            tail_dst[i] = acc;
        }
    }
    Ok(())
}

// --- Portable Scalar Implementations ---

fn pack_u32_tiles_scalar(values: &[u32], bit_width: u8, dst: &mut [u8]) {
    let bw = u32::from(bit_width);
    let mask = if bit_width == 32 {
        u32::MAX
    } else {
        (1u32 << bw) - 1
    };
    let tile_out_bytes = 128 * usize::from(bit_width);

    for (tile_in, tile_out) in values
        .as_chunks::<FASTLANES_TILE_SIZE>()
        .0
        .iter()
        .zip(dst.chunks_exact_mut(tile_out_bytes))
    {
        let mut acc = [0u32; 8];
        let mut bit_pos: u32 = 0;
        let mut word_idx: usize = 0;

        for k in 0..128 {
            let base = k * 8;
            let mut vals = [0u32; 8];
            for lane in 0..8 {
                vals[lane] = tile_in[base + lane] & mask;
                acc[lane] |= vals[lane] << bit_pos;
            }
            let next_bit_pos = bit_pos + bw;
            if next_bit_pos >= 32 {
                let out_off = word_idx * 32;
                for lane in 0..8 {
                    let bytes = acc[lane].to_le_bytes();
                    tile_out[out_off + lane * 4..out_off + lane * 4 + 4].copy_from_slice(&bytes);
                }
                word_idx += 1;
                let rem_bits = next_bit_pos - 32;
                if rem_bits > 0 {
                    let rshift = 32 - bit_pos;
                    for lane in 0..8 {
                        acc[lane] = vals[lane] >> rshift;
                    }
                } else {
                    acc = [0u32; 8];
                }
                bit_pos = rem_bits;
            } else {
                bit_pos = next_bit_pos;
            }
        }
    }
}

fn unpack_u32_tiles_scalar(src: &[u8], bit_width: u8, dst: &mut [u32]) {
    let bw = u32::from(bit_width);
    let mask = if bit_width == 32 {
        u32::MAX
    } else {
        (1u32 << bw) - 1
    };
    let tile_in_bytes = 128 * usize::from(bit_width);
    let total_words = 4 * usize::from(bit_width);

    for (tile_in, tile_out) in src
        .chunks_exact(tile_in_bytes)
        .zip(dst.as_chunks_mut::<FASTLANES_TILE_SIZE>().0.iter_mut())
    {
        let mut cur = load_8xu32_le(tile_in, 0);
        let mut word_idx: usize = 0;
        let mut bit_pos: u32 = 0;

        for k in 0..128 {
            let mut vals = [0u32; 8];
            for lane in 0..8 {
                vals[lane] = (cur[lane] >> bit_pos) & mask;
            }
            let next_bit_pos = bit_pos + bw;
            if next_bit_pos >= 32 {
                let rem_bits = next_bit_pos - 32;
                word_idx += 1;
                if word_idx < total_words {
                    let next = load_8xu32_le(tile_in, word_idx * 32);
                    if rem_bits > 0 {
                        let lshift = 32 - bit_pos;
                        for lane in 0..8 {
                            vals[lane] |= (next[lane] << lshift) & mask;
                        }
                    }
                    cur = next;
                }
                bit_pos = rem_bits;
            } else {
                bit_pos = next_bit_pos;
            }
            let out_base = k * 8;
            tile_out[out_base..out_base + 8].copy_from_slice(&vals);
        }
    }
}

#[inline]
fn load_8xu32_le(buf: &[u8], offset: usize) -> [u32; 8] {
    let mut out = [0u32; 8];
    for (lane, slot) in out.iter_mut().enumerate() {
        let p = offset + lane * 4;
        *slot = u32::from_le_bytes([buf[p], buf[p + 1], buf[p + 2], buf[p + 3]]);
    }
    out
}

fn pack_u32_tail_scalar(tail: &[u32], bit_width: u8, dst: &mut [u8]) {
    let bw = u32::from(bit_width);
    let mask = if bit_width == 32 {
        u64::from(u32::MAX)
    } else {
        (1u64 << bw) - 1
    };
    let mut acc: u64 = 0;
    let mut bits_in_acc: u32 = 0;
    let mut pos: usize = 0;

    for &v in tail {
        acc |= (u64::from(v) & mask) << bits_in_acc;
        bits_in_acc += bw;
        while bits_in_acc >= 8 {
            dst[pos] = (acc & 0xFF) as u8;
            pos += 1;
            acc >>= 8;
            bits_in_acc -= 8;
        }
    }
    if bits_in_acc > 0 {
        dst[pos] = (acc & 0xFF) as u8;
    }
}

fn unpack_u32_tail_scalar(src: &[u8], bit_width: u8, dst: &mut [u32]) {
    let bw = u32::from(bit_width);
    let mask = if bit_width == 32 {
        u64::from(u32::MAX)
    } else {
        (1u64 << bw) - 1
    };
    let mut acc: u64 = 0;
    let mut bits_in_acc: u32 = 0;
    let mut byte_idx: usize = 0;

    for slot in dst.iter_mut() {
        while bits_in_acc < bw {
            acc |= u64::from(src[byte_idx]) << bits_in_acc;
            byte_idx += 1;
            bits_in_acc += 8;
        }
        *slot = (acc & mask) as u32;
        acc >>= bw;
        bits_in_acc -= bw;
    }
}

fn pack_u64_tiles_scalar(values: &[u64], bit_width: u8, dst: &mut [u8]) {
    let bw = u32::from(bit_width);
    let mask = if bit_width == 64 {
        u64::MAX
    } else {
        (1u64 << bw) - 1
    };
    let tile_out_bytes = 128 * usize::from(bit_width);

    for (tile_in, tile_out) in values
        .as_chunks::<FASTLANES_TILE_SIZE>()
        .0
        .iter()
        .zip(dst.chunks_exact_mut(tile_out_bytes))
    {
        let mut acc = [0u64; 4];
        let mut bit_pos: u32 = 0;
        let mut word_idx: usize = 0;

        for k in 0..256 {
            let base = k * 4;
            let mut vals = [0u64; 4];
            for lane in 0..4 {
                vals[lane] = tile_in[base + lane] & mask;
                acc[lane] |= vals[lane] << bit_pos;
            }
            let next_bit_pos = bit_pos + bw;
            if next_bit_pos >= 64 {
                let out_off = word_idx * 32;
                for lane in 0..4 {
                    let bytes = acc[lane].to_le_bytes();
                    tile_out[out_off + lane * 8..out_off + lane * 8 + 8].copy_from_slice(&bytes);
                }
                word_idx += 1;
                let rem_bits = next_bit_pos - 64;
                if rem_bits > 0 {
                    let rshift = 64 - bit_pos;
                    for lane in 0..4 {
                        acc[lane] = vals[lane] >> rshift;
                    }
                } else {
                    acc = [0u64; 4];
                }
                bit_pos = rem_bits;
            } else {
                bit_pos = next_bit_pos;
            }
        }
    }
}

fn unpack_u64_tiles_scalar(src: &[u8], bit_width: u8, dst: &mut [u64]) {
    let bw = u32::from(bit_width);
    let mask = if bit_width == 64 {
        u64::MAX
    } else {
        (1u64 << bw) - 1
    };
    let tile_in_bytes = 128 * usize::from(bit_width);
    let total_words = 4 * usize::from(bit_width);

    for (tile_in, tile_out) in src
        .chunks_exact(tile_in_bytes)
        .zip(dst.as_chunks_mut::<FASTLANES_TILE_SIZE>().0.iter_mut())
    {
        let mut cur = load_4xu64_le(tile_in, 0);
        let mut word_idx: usize = 0;
        let mut bit_pos: u32 = 0;

        for k in 0..256 {
            let mut vals = [0u64; 4];
            for lane in 0..4 {
                vals[lane] = (cur[lane] >> bit_pos) & mask;
            }
            let next_bit_pos = bit_pos + bw;
            if next_bit_pos >= 64 {
                let rem_bits = next_bit_pos - 64;
                word_idx += 1;
                if word_idx < total_words {
                    let next = load_4xu64_le(tile_in, word_idx * 32);
                    if rem_bits > 0 {
                        let lshift = 64 - bit_pos;
                        for lane in 0..4 {
                            vals[lane] |= (next[lane] << lshift) & mask;
                        }
                    }
                    cur = next;
                }
                bit_pos = rem_bits;
            } else {
                bit_pos = next_bit_pos;
            }
            let out_base = k * 4;
            tile_out[out_base..out_base + 4].copy_from_slice(&vals);
        }
    }
}

#[inline]
fn load_4xu64_le(buf: &[u8], offset: usize) -> [u64; 4] {
    let mut out = [0u64; 4];
    for (lane, slot) in out.iter_mut().enumerate() {
        let p = offset + lane * 8;
        *slot = u64::from_le_bytes([
            buf[p],
            buf[p + 1],
            buf[p + 2],
            buf[p + 3],
            buf[p + 4],
            buf[p + 5],
            buf[p + 6],
            buf[p + 7],
        ]);
    }
    out
}

fn pack_u64_tail_scalar(tail: &[u64], bit_width: u8, dst: &mut [u8]) {
    let bw = u32::from(bit_width);
    let mask = if bit_width == 64 {
        u128::from(u64::MAX)
    } else {
        (1u128 << bw) - 1
    };
    let mut acc: u128 = 0;
    let mut bits_in_acc: u32 = 0;
    let mut pos: usize = 0;

    for &v in tail {
        acc |= (u128::from(v) & mask) << bits_in_acc;
        bits_in_acc += bw;
        while bits_in_acc >= 8 {
            dst[pos] = (acc & 0xFF) as u8;
            pos += 1;
            acc >>= 8;
            bits_in_acc -= 8;
        }
    }
    if bits_in_acc > 0 {
        dst[pos] = (acc & 0xFF) as u8;
    }
}

fn unpack_u64_tail_scalar(src: &[u8], bit_width: u8, dst: &mut [u64]) {
    let bw = u32::from(bit_width);
    let mask = if bit_width == 64 {
        u128::from(u64::MAX)
    } else {
        (1u128 << bw) - 1
    };
    let mut acc: u128 = 0;
    let mut bits_in_acc: u32 = 0;
    let mut byte_idx: usize = 0;

    for slot in dst.iter_mut() {
        while bits_in_acc < bw {
            acc |= u128::from(src[byte_idx]) << bits_in_acc;
            byte_idx += 1;
            bits_in_acc += 8;
        }
        *slot = (acc & mask) as u64;
        acc >>= bw;
        bits_in_acc -= bw;
    }
}

fn delta_encode_i64_tiles_scalar(values: &[i64], bases_out: &mut [i64], deltas_out: &mut [i64]) {
    for ((tile_in, tile_bases), tile_deltas) in values
        .as_chunks::<FASTLANES_TILE_SIZE>()
        .0
        .iter()
        .zip(bases_out.as_chunks_mut::<8>().0.iter_mut())
        .zip(
            deltas_out
                .as_chunks_mut::<FASTLANES_TILE_SIZE>()
                .0
                .iter_mut(),
        )
    {
        for lane in 0..8 {
            let seg = FASTLANES_ORDER_8[lane];
            tile_bases[lane] = tile_in[seg * 128];
            tile_deltas[lane] = 0;
        }
        for r in 1..128 {
            for lane in 0..8 {
                let seg = FASTLANES_ORDER_8[lane];
                let idx = seg * 128 + r;
                tile_deltas[r * 8 + lane] = tile_in[idx].wrapping_sub(tile_in[idx - 1]);
            }
        }
    }
}

fn delta_decode_i64_tiles_scalar(bases: &[i64], deltas: &[i64], dst: &mut [i64]) {
    for ((tile_bases, tile_deltas), tile_out) in bases
        .as_chunks::<8>()
        .0
        .iter()
        .zip(deltas.as_chunks::<FASTLANES_TILE_SIZE>().0.iter())
        .zip(dst.as_chunks_mut::<FASTLANES_TILE_SIZE>().0.iter_mut())
    {
        let mut acc = *tile_bases;
        for r in 0..128 {
            for lane in 0..8 {
                acc[lane] = acc[lane].wrapping_add(tile_deltas[r * 8 + lane]);
                let seg = FASTLANES_ORDER_8[lane];
                tile_out[seg * 128 + r] = acc[lane];
            }
        }
    }
}

// --- x86_64 AVX2 Implementations ---

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn pack_u32_tiles_avx2(values: &[u32], bit_width: u8, dst: &mut [u8]) {
    use std::arch::x86_64::*;

    let bw = u32::from(bit_width);
    let mask_scalar = if bit_width == 32 {
        u32::MAX
    } else {
        (1u32 << bw) - 1
    };
    let mask = _mm256_set1_epi32(mask_scalar as i32);
    let tile_out_bytes = 128 * usize::from(bit_width);

    for (tile_in, tile_out) in values
        .as_chunks::<FASTLANES_TILE_SIZE>()
        .0
        .iter()
        .zip(dst.chunks_exact_mut(tile_out_bytes))
    {
        let mut acc = _mm256_setzero_si256();
        let mut bit_pos: u32 = 0;
        let mut word_idx: usize = 0;

        for k in 0..128 {
            // SAFETY: `tile_in` has length 1024 and `k * 8 + 8 <= 1024`;
            // `_mm256_loadu_si256` allows arbitrary alignment.
            let raw = unsafe { _mm256_loadu_si256(tile_in.as_ptr().add(k * 8) as *const __m256i) };
            let v = _mm256_and_si256(raw, mask);
            let lshift = _mm256_set1_epi32(bit_pos as i32);
            acc = _mm256_or_si256(acc, _mm256_sllv_epi32(v, lshift));

            let next_bit_pos = bit_pos + bw;
            if next_bit_pos >= 32 {
                // SAFETY: `word_idx < 4 * bit_width`, so `word_idx * 32 + 32 <= 128 * bit_width == tile_out.len()`;
                // `_mm256_storeu_si256` allows arbitrary alignment.
                unsafe {
                    _mm256_storeu_si256(
                        tile_out.as_mut_ptr().add(word_idx * 32) as *mut __m256i,
                        acc,
                    );
                }
                word_idx += 1;
                let rem_bits = next_bit_pos - 32;
                if rem_bits > 0 {
                    let rshift = _mm256_set1_epi32((32 - bit_pos) as i32);
                    acc = _mm256_srlv_epi32(v, rshift);
                } else {
                    acc = _mm256_setzero_si256();
                }
                bit_pos = rem_bits;
            } else {
                bit_pos = next_bit_pos;
            }
        }
    }
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn unpack_u32_tiles_avx2(src: &[u8], bit_width: u8, dst: &mut [u32]) {
    use std::arch::x86_64::*;

    let bw = u32::from(bit_width);
    let mask_scalar = if bit_width == 32 {
        u32::MAX
    } else {
        (1u32 << bw) - 1
    };
    let mask = _mm256_set1_epi32(mask_scalar as i32);
    let tile_in_bytes = 128 * usize::from(bit_width);
    let total_words = 4 * usize::from(bit_width);

    for (tile_in, tile_out) in src
        .chunks_exact(tile_in_bytes)
        .zip(dst.as_chunks_mut::<FASTLANES_TILE_SIZE>().0.iter_mut())
    {
        // SAFETY: `bit_width >= 1`, so `tile_in.len() >= 128 >= 32`; unaligned load is valid.
        let mut cur = unsafe { _mm256_loadu_si256(tile_in.as_ptr() as *const __m256i) };
        let mut word_idx: usize = 0;
        let mut bit_pos: u32 = 0;

        for k in 0..128 {
            let rshift = _mm256_set1_epi32(bit_pos as i32);
            let mut vals = _mm256_and_si256(_mm256_srlv_epi32(cur, rshift), mask);

            let next_bit_pos = bit_pos + bw;
            if next_bit_pos >= 32 {
                let rem_bits = next_bit_pos - 32;
                word_idx += 1;
                if word_idx < total_words {
                    // SAFETY: `word_idx < total_words == 4 * bit_width`, so `word_idx * 32 + 32 <= tile_in.len()`.
                    let next = unsafe {
                        _mm256_loadu_si256(tile_in.as_ptr().add(word_idx * 32) as *const __m256i)
                    };
                    if rem_bits > 0 {
                        let lshift = _mm256_set1_epi32((32 - bit_pos) as i32);
                        let hi = _mm256_and_si256(_mm256_sllv_epi32(next, lshift), mask);
                        vals = _mm256_or_si256(vals, hi);
                    }
                    cur = next;
                }
                bit_pos = rem_bits;
            } else {
                bit_pos = next_bit_pos;
            }
            // SAFETY: `tile_out` has length 1024 and `k * 8 + 8 <= 1024`; unaligned store is valid.
            unsafe {
                _mm256_storeu_si256(tile_out.as_mut_ptr().add(k * 8) as *mut __m256i, vals);
            }
        }
    }
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn pack_u64_tiles_avx2(values: &[u64], bit_width: u8, dst: &mut [u8]) {
    use std::arch::x86_64::*;

    let bw = u32::from(bit_width);
    let mask_scalar = if bit_width == 64 {
        u64::MAX
    } else {
        (1u64 << bw) - 1
    };
    let mask = _mm256_set1_epi64x(mask_scalar as i64);
    let tile_out_bytes = 128 * usize::from(bit_width);

    for (tile_in, tile_out) in values
        .as_chunks::<FASTLANES_TILE_SIZE>()
        .0
        .iter()
        .zip(dst.chunks_exact_mut(tile_out_bytes))
    {
        let mut acc = _mm256_setzero_si256();
        let mut bit_pos: u32 = 0;
        let mut word_idx: usize = 0;

        for k in 0..256 {
            // SAFETY: `tile_in` has length 1024 and `k * 4 + 4 <= 1024`; unaligned load is valid.
            let raw = unsafe { _mm256_loadu_si256(tile_in.as_ptr().add(k * 4) as *const __m256i) };
            let v = _mm256_and_si256(raw, mask);
            let lshift = _mm256_set1_epi64x(i64::from(bit_pos));
            acc = _mm256_or_si256(acc, _mm256_sllv_epi64(v, lshift));

            let next_bit_pos = bit_pos + bw;
            if next_bit_pos >= 64 {
                // SAFETY: `word_idx < 4 * bit_width`, so `word_idx * 32 + 32 <= 128 * bit_width == tile_out.len()`.
                unsafe {
                    _mm256_storeu_si256(
                        tile_out.as_mut_ptr().add(word_idx * 32) as *mut __m256i,
                        acc,
                    );
                }
                word_idx += 1;
                let rem_bits = next_bit_pos - 64;
                if rem_bits > 0 {
                    let rshift = _mm256_set1_epi64x(i64::from(64 - bit_pos));
                    acc = _mm256_srlv_epi64(v, rshift);
                } else {
                    acc = _mm256_setzero_si256();
                }
                bit_pos = rem_bits;
            } else {
                bit_pos = next_bit_pos;
            }
        }
    }
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn unpack_u64_tiles_avx2(src: &[u8], bit_width: u8, dst: &mut [u64]) {
    use std::arch::x86_64::*;

    let bw = u32::from(bit_width);
    let mask_scalar = if bit_width == 64 {
        u64::MAX
    } else {
        (1u64 << bw) - 1
    };
    let mask = _mm256_set1_epi64x(mask_scalar as i64);
    let tile_in_bytes = 128 * usize::from(bit_width);
    let total_words = 4 * usize::from(bit_width);

    for (tile_in, tile_out) in src
        .chunks_exact(tile_in_bytes)
        .zip(dst.as_chunks_mut::<FASTLANES_TILE_SIZE>().0.iter_mut())
    {
        // SAFETY: `bit_width >= 1`, so `tile_in.len() >= 128 >= 32`; unaligned load is valid.
        let mut cur = unsafe { _mm256_loadu_si256(tile_in.as_ptr() as *const __m256i) };
        let mut word_idx: usize = 0;
        let mut bit_pos: u32 = 0;

        for k in 0..256 {
            let rshift = _mm256_set1_epi64x(i64::from(bit_pos));
            let mut vals = _mm256_and_si256(_mm256_srlv_epi64(cur, rshift), mask);

            let next_bit_pos = bit_pos + bw;
            if next_bit_pos >= 64 {
                let rem_bits = next_bit_pos - 64;
                word_idx += 1;
                if word_idx < total_words {
                    // SAFETY: `word_idx < total_words == 4 * bit_width`, so `word_idx * 32 + 32 <= tile_in.len()`.
                    let next = unsafe {
                        _mm256_loadu_si256(tile_in.as_ptr().add(word_idx * 32) as *const __m256i)
                    };
                    if rem_bits > 0 {
                        let lshift = _mm256_set1_epi64x(i64::from(64 - bit_pos));
                        let hi = _mm256_and_si256(_mm256_sllv_epi64(next, lshift), mask);
                        vals = _mm256_or_si256(vals, hi);
                    }
                    cur = next;
                }
                bit_pos = rem_bits;
            } else {
                bit_pos = next_bit_pos;
            }
            // SAFETY: `tile_out` has length 1024 and `k * 4 + 4 <= 1024`; unaligned store is valid.
            unsafe {
                _mm256_storeu_si256(tile_out.as_mut_ptr().add(k * 4) as *mut __m256i, vals);
            }
        }
    }
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn delta_encode_i64_tiles_avx2(
    values: &[i64],
    bases_out: &mut [i64],
    deltas_out: &mut [i64],
) {
    use std::arch::x86_64::*;

    for ((tile_in, tile_bases), tile_deltas) in values
        .as_chunks::<FASTLANES_TILE_SIZE>()
        .0
        .iter()
        .zip(bases_out.as_chunks_mut::<8>().0.iter_mut())
        .zip(
            deltas_out
                .as_chunks_mut::<FASTLANES_TILE_SIZE>()
                .0
                .iter_mut(),
        )
    {
        for lane in 0..8 {
            let seg = FASTLANES_ORDER_8[lane];
            tile_bases[lane] = tile_in[seg * 128];
            tile_deltas[lane] = 0;
        }
        let mut prev0 =
            _mm256_set_epi64x(tile_bases[3], tile_bases[2], tile_bases[1], tile_bases[0]);
        let mut prev1 =
            _mm256_set_epi64x(tile_bases[7], tile_bases[6], tile_bases[5], tile_bases[4]);
        for r in 1..128 {
            let cur0 = _mm256_set_epi64x(
                tile_in[FASTLANES_ORDER_8[3] * 128 + r],
                tile_in[FASTLANES_ORDER_8[2] * 128 + r],
                tile_in[FASTLANES_ORDER_8[1] * 128 + r],
                tile_in[FASTLANES_ORDER_8[0] * 128 + r],
            );
            let cur1 = _mm256_set_epi64x(
                tile_in[FASTLANES_ORDER_8[7] * 128 + r],
                tile_in[FASTLANES_ORDER_8[6] * 128 + r],
                tile_in[FASTLANES_ORDER_8[5] * 128 + r],
                tile_in[FASTLANES_ORDER_8[4] * 128 + r],
            );
            let d0 = _mm256_sub_epi64(cur0, prev0);
            let d1 = _mm256_sub_epi64(cur1, prev1);
            // SAFETY: `r < 128` so `r * 8 + 8 <= 1024 == tile_deltas.len()`; unaligned store is valid.
            unsafe {
                let dst_ptr = tile_deltas.as_mut_ptr().add(r * 8);
                _mm256_storeu_si256(dst_ptr as *mut __m256i, d0);
                _mm256_storeu_si256(dst_ptr.add(4) as *mut __m256i, d1);
            }
            prev0 = cur0;
            prev1 = cur1;
        }
    }
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "avx2")]
unsafe fn delta_decode_i64_tiles_avx2(bases: &[i64], deltas: &[i64], dst: &mut [i64]) {
    use std::arch::x86_64::*;

    for ((tile_bases, tile_deltas), tile_out) in bases
        .as_chunks::<8>()
        .0
        .iter()
        .zip(deltas.as_chunks::<FASTLANES_TILE_SIZE>().0.iter())
        .zip(dst.as_chunks_mut::<FASTLANES_TILE_SIZE>().0.iter_mut())
    {
        // SAFETY: `tile_bases.len() == 8`; unaligned loads of 4 `i64`s each are in bounds.
        let (mut acc0, mut acc1) = unsafe {
            (
                _mm256_loadu_si256(tile_bases.as_ptr() as *const __m256i),
                _mm256_loadu_si256(tile_bases.as_ptr().add(4) as *const __m256i),
            )
        };
        let mut lanes = [0i64; 8];

        for r in 0..128 {
            // SAFETY: `r < 128` so `r * 8 + 8 <= 1024 == tile_deltas.len()`; unaligned loads are valid.
            let (d0, d1) = unsafe {
                let ptr = tile_deltas.as_ptr().add(r * 8);
                (
                    _mm256_loadu_si256(ptr as *const __m256i),
                    _mm256_loadu_si256(ptr.add(4) as *const __m256i),
                )
            };
            acc0 = _mm256_add_epi64(acc0, d0);
            acc1 = _mm256_add_epi64(acc1, d1);
            // SAFETY: `lanes` is an 8-element `[i64; 8]` array on the stack.
            unsafe {
                _mm256_storeu_si256(lanes.as_mut_ptr() as *mut __m256i, acc0);
                _mm256_storeu_si256(lanes.as_mut_ptr().add(4) as *mut __m256i, acc1);
            }
            for lane in 0..8 {
                let seg = FASTLANES_ORDER_8[lane];
                tile_out[seg * 128 + r] = lanes[lane];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const TEST_LENGTHS: &[usize] = &[
        0, 1, 7, 15, 16, 31, 32, 63, 64, 1023, 1024, 1025, 2048, 4096,
    ];

    fn make_u32_values(len: usize, bit_width: u8) -> Vec<u32> {
        let mask = if bit_width == 0 {
            0
        } else if bit_width == 32 {
            u32::MAX
        } else {
            (1u32 << bit_width) - 1
        };
        (0..len)
            .map(|i| {
                let mixed = (i as u32)
                    .wrapping_mul(2_654_435_761)
                    .wrapping_add((i >> 3) as u32 ^ 0xA5A5_5A5A);
                mixed & mask
            })
            .collect()
    }

    fn make_u64_values(len: usize, bit_width: u8) -> Vec<u64> {
        let mask = if bit_width == 0 {
            0
        } else if bit_width == 64 {
            u64::MAX
        } else {
            (1u64 << bit_width) - 1
        };
        (0..len)
            .map(|i| {
                let mixed = (i as u64)
                    .wrapping_mul(11_400_714_819_323_198_485)
                    .wrapping_add((i >> 2) as u64 ^ 0x5A5A_A5A5_F0F0_0F0F);
                mixed & mask
            })
            .collect()
    }

    #[test]
    fn bitpack_u32_u64_simd_matches_scalar_all_widths_and_lengths() {
        for bw in 0..=32u8 {
            for &len in TEST_LENGTHS {
                let values = make_u32_values(len, bw);
                let mut packed_simd = Vec::new();
                pack_u32(&values, bw, &mut packed_simd).unwrap();

                let expected_len = packed_bytes_len(len, bw).unwrap();
                assert_eq!(packed_simd.len(), expected_len, "u32 bw={bw} len={len}");

                // Compare full tiles directly against portable scalar implementation.
                let full_tiles = len / FASTLANES_TILE_SIZE;
                let rem = len % FASTLANES_TILE_SIZE;
                if full_tiles > 0 && bw > 0 {
                    let tile_bytes = full_tiles * 128 * usize::from(bw);
                    let mut scalar_tiles = vec![0u8; tile_bytes];
                    pack_u32_tiles_scalar(
                        &values[..full_tiles * FASTLANES_TILE_SIZE],
                        bw,
                        &mut scalar_tiles,
                    );
                    assert_eq!(
                        &packed_simd[..tile_bytes],
                        &scalar_tiles[..],
                        "u32 pack SIMD != scalar at bw={bw} len={len}"
                    );
                }

                let mut unpacked_simd = Vec::new();
                unpack_u32(&packed_simd, len, bw, &mut unpacked_simd).unwrap();
                assert_eq!(unpacked_simd, values, "u32 roundtrip bw={bw} len={len}");

                if full_tiles > 0 && bw > 0 {
                    let tile_bytes = full_tiles * 128 * usize::from(bw);
                    let mut scalar_unpacked = vec![0u32; full_tiles * FASTLANES_TILE_SIZE];
                    unpack_u32_tiles_scalar(&packed_simd[..tile_bytes], bw, &mut scalar_unpacked);
                    assert_eq!(
                        &scalar_unpacked[..],
                        &values[..full_tiles * FASTLANES_TILE_SIZE],
                        "u32 unpack scalar mismatch bw={bw} len={len}"
                    );
                }
                if rem > 0 {
                    assert_eq!(
                        packed_simd.len(),
                        full_tiles * 128 * usize::from(bw) + (rem * usize::from(bw)).div_ceil(8)
                    );
                }
            }
        }

        for bw in 0..=64u8 {
            for &len in TEST_LENGTHS {
                let values = make_u64_values(len, bw);
                let mut packed_simd = Vec::new();
                pack_u64(&values, bw, &mut packed_simd).unwrap();

                let expected_len = packed_bytes_len(len, bw).unwrap();
                assert_eq!(packed_simd.len(), expected_len, "u64 bw={bw} len={len}");

                let full_tiles = len / FASTLANES_TILE_SIZE;
                if full_tiles > 0 && bw > 0 {
                    let tile_bytes = full_tiles * 128 * usize::from(bw);
                    let mut scalar_tiles = vec![0u8; tile_bytes];
                    pack_u64_tiles_scalar(
                        &values[..full_tiles * FASTLANES_TILE_SIZE],
                        bw,
                        &mut scalar_tiles,
                    );
                    assert_eq!(
                        &packed_simd[..tile_bytes],
                        &scalar_tiles[..],
                        "u64 pack SIMD != scalar at bw={bw} len={len}"
                    );
                }

                let mut unpacked_simd = Vec::new();
                unpack_u64(&packed_simd, len, bw, &mut unpacked_simd).unwrap();
                assert_eq!(unpacked_simd, values, "u64 roundtrip bw={bw} len={len}");

                if full_tiles > 0 && bw > 0 {
                    let tile_bytes = full_tiles * 128 * usize::from(bw);
                    let mut scalar_unpacked = vec![0u64; full_tiles * FASTLANES_TILE_SIZE];
                    unpack_u64_tiles_scalar(&packed_simd[..tile_bytes], bw, &mut scalar_unpacked);
                    assert_eq!(
                        &scalar_unpacked[..],
                        &values[..full_tiles * FASTLANES_TILE_SIZE],
                        "u64 unpack scalar mismatch bw={bw} len={len}"
                    );
                }
            }
        }
    }

    #[test]
    fn delta_i64_simd_matches_scalar_monotonic_and_zigzag() {
        for &len in TEST_LENGTHS {
            // Monotonic timestamps (non-negative deltas).
            let mono: Vec<i64> = (0..len)
                .map(|i| 1_700_000_000_000_000_000i64 + (i as i64) * 1_234 + ((i % 7) as i64))
                .collect();
            let mut bases = Vec::new();
            let mut deltas = Vec::new();
            delta_encode_i64(&mono, &mut bases, &mut deltas);

            let full_tiles = len / FASTLANES_TILE_SIZE;
            if full_tiles > 0 {
                let mut scalar_bases = vec![0i64; full_tiles * 8];
                let mut scalar_deltas = vec![0i64; full_tiles * FASTLANES_TILE_SIZE];
                delta_encode_i64_tiles_scalar(
                    &mono[..full_tiles * FASTLANES_TILE_SIZE],
                    &mut scalar_bases,
                    &mut scalar_deltas,
                );
                assert_eq!(&bases[..full_tiles * 8], &scalar_bases[..]);
                assert_eq!(
                    &deltas[..full_tiles * FASTLANES_TILE_SIZE],
                    &scalar_deltas[..]
                );
            }

            let mut decoded = Vec::new();
            delta_decode_i64(&bases, &deltas, len, &mut decoded).unwrap();
            assert_eq!(decoded, mono);

            // Alternating positive/negative values including i64::MIN / i64::MAX jumps -> zigzag + pack_u64.
            let oscillating: Vec<i64> = (0..len)
                .map(|i| match i % 5 {
                    0 => i64::MIN + (i as i64),
                    1 => i64::MAX - (i as i64),
                    2 => -(i as i64 * 997),
                    3 => i as i64 * 4099,
                    _ => 0,
                })
                .collect();
            bases.clear();
            deltas.clear();
            delta_encode_i64(&oscillating, &mut bases, &mut deltas);

            let zz: Vec<u64> = deltas.iter().copied().map(zigzag_encode_i64).collect();
            let bw = min_bits_u64(&zz);
            let mut packed = Vec::new();
            pack_u64(&zz, bw, &mut packed).unwrap();

            let mut unpacked_zz = Vec::new();
            unpack_u64(&packed, len, bw, &mut unpacked_zz).unwrap();
            let unpacked_deltas: Vec<i64> =
                unpacked_zz.into_iter().map(zigzag_decode_i64).collect();
            assert_eq!(unpacked_deltas, deltas);

            decoded.clear();
            delta_decode_i64(&bases, &unpacked_deltas, len, &mut decoded).unwrap();
            assert_eq!(decoded, oscillating);

            if full_tiles > 0 {
                let mut scalar_decoded = vec![0i64; full_tiles * FASTLANES_TILE_SIZE];
                delta_decode_i64_tiles_scalar(
                    &bases[..full_tiles * 8],
                    &deltas[..full_tiles * FASTLANES_TILE_SIZE],
                    &mut scalar_decoded,
                );
                assert_eq!(
                    &scalar_decoded[..],
                    &oscillating[..full_tiles * FASTLANES_TILE_SIZE]
                );
            }
        }
    }

    #[test]
    fn bitpack_rejects_truncated_input_and_invalid_bit_width() {
        let mut out32 = Vec::new();
        let mut dec32 = Vec::new();
        assert!(matches!(
            pack_u32(&[1, 2, 3], 33, &mut out32),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            unpack_u32(&[0; 16], 3, 33, &mut dec32),
            Err(StatamError::Corrupt { .. })
        ));

        let mut out64 = Vec::new();
        let mut dec64 = Vec::new();
        assert!(matches!(
            pack_u64(&[1, 2, 3], 65, &mut out64),
            Err(StatamError::Corrupt { .. })
        ));
        assert!(matches!(
            unpack_u64(&[0; 16], 3, 65, &mut dec64),
            Err(StatamError::Corrupt { .. })
        ));

        // Truncated full tile and truncated tail.
        let vals = make_u32_values(1025, 7);
        let mut packed = Vec::new();
        pack_u32(&vals, 7, &mut packed).unwrap();
        assert!(matches!(
            unpack_u32(&packed[..packed.len() - 1], 1025, 7, &mut dec32),
            Err(StatamError::Corrupt { .. })
        ));

        let vals64 = make_u64_values(1025, 13);
        let mut packed64 = Vec::new();
        pack_u64(&vals64, 13, &mut packed64).unwrap();
        assert!(matches!(
            unpack_u64(&packed64[..packed64.len() - 1], 1025, 13, &mut dec64),
            Err(StatamError::Corrupt { .. })
        ));

        // Oversized count at bit_width == 0 must return BoundsExceeded without OOM.
        assert!(matches!(
            unpack_u32(&[], usize::MAX, 0, &mut dec32),
            Err(StatamError::BoundsExceeded { .. })
        ));
        assert!(matches!(
            unpack_u64(&[], usize::MAX, 0, &mut dec64),
            Err(StatamError::BoundsExceeded { .. })
        ));
        assert!(matches!(
            delta_decode_i64(&[], &[], usize::MAX, &mut Vec::new()),
            Err(StatamError::BoundsExceeded { .. })
        ));
        assert!(matches!(
            delta_decode_i64(&[0], &[0], 2, &mut Vec::new()),
            Err(StatamError::Corrupt { .. })
        ));
    }

    proptest! {
        #[test]
        fn proptest_bitpack_u32_and_u64_parity(
            bw32 in 0u8..=32,
            bw64 in 0u8..=64,
            raw in prop::collection::vec(any::<u64>(), 0..=2100),
        ) {
            let mask32 = if bw32 == 0 { 0 } else if bw32 == 32 { u32::MAX } else { (1u32 << bw32) - 1 };
            let v32: Vec<u32> = raw.iter().map(|&x| (x as u32) & mask32).collect();
            let mut p32 = Vec::new();
            pack_u32(&v32, bw32, &mut p32).unwrap();
            let mut u32_out = Vec::new();
            unpack_u32(&p32, v32.len(), bw32, &mut u32_out).unwrap();
            prop_assert_eq!(u32_out, v32);

            let mask64 = if bw64 == 0 { 0 } else if bw64 == 64 { u64::MAX } else { (1u64 << bw64) - 1 };
            let v64: Vec<u64> = raw.iter().map(|&x| x & mask64).collect();
            let mut p64 = Vec::new();
            pack_u64(&v64, bw64, &mut p64).unwrap();
            let mut u64_out = Vec::new();
            unpack_u64(&p64, v64.len(), bw64, &mut u64_out).unwrap();
            prop_assert_eq!(u64_out, v64);
        }

        #[test]
        fn proptest_delta_i64_parity(vals in prop::collection::vec(any::<i64>(), 0..=2100)) {
            let mut bases = Vec::new();
            let mut deltas = Vec::new();
            delta_encode_i64(&vals, &mut bases, &mut deltas);
            let mut out = Vec::new();
            delta_decode_i64(&bases, &deltas, vals.len(), &mut out).unwrap();
            prop_assert_eq!(out, vals);
        }
    }
}
