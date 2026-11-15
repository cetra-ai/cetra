//! Trigram hashes and the bitmaps of the frame filters and the rollup (S4 § Trigram sets,
//! § Frame filter).

use std::collections::HashSet;

use super::FILTER_MIN_BITS;

/// Largest multiple of 64 that fits in a `u32`: the cap of a filter's bit count.
const MAX_FILTER_BITS: u32 = u32::MAX - u32::MAX % 64;

/// Hash of the trigram `(a, b, c)`.
pub(crate) fn tri_hash(a: u8, b: u8, c: u8) -> u32 {
    let x = u64::from(a) | (u64::from(b) << 8) | (u64::from(c) << 16);
    (x.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as u32
}

/// Adds the hash of every trigram of `bytes` to `out` (nothing when `bytes.len() < 3`).
pub(crate) fn add_trigrams(bytes: &[u8], out: &mut HashSet<u32>) {
    for window in bytes.windows(3) {
        if let &[a, b, c] = window {
            out.insert(tri_hash(a, b, c));
        }
    }
}

/// The bit index of hash `h` in a filter of `bits` bits.
pub(crate) fn filter_bit(h: u32, bits: u32) -> u32 {
    ((u64::from(h) * u64::from(bits)) >> 32) as u32
}

/// Number of bits of a frame filter for `d` distinct hashes:
/// `max(FILTER_MIN_BITS, round_up_64(2 × d))`, saturating at the largest multiple of 64 that
/// fits in a `u32`.
pub(crate) fn filter_bits(d: usize) -> u32 {
    let wanted = d.saturating_mul(2).div_ceil(64).saturating_mul(64);
    u32::try_from(wanted)
        .unwrap_or(MAX_FILTER_BITS)
        .max(FILTER_MIN_BITS)
}

/// Sets bit `i` of `bitmap` (bit `i % 8` of byte `i / 8`, least significant bit first). An
/// index past the end does nothing.
pub(crate) fn set_bit(bitmap: &mut [u8], i: u32) {
    if let Some(byte) = bitmap.get_mut(i as usize / 8) {
        *byte |= 1 << (i % 8);
    }
}

/// Whether bit `i` of `bitmap` is set. An index past the end is not set.
pub(crate) fn get_bit(bitmap: &[u8], i: u32) -> bool {
    bitmap
        .get(i as usize / 8)
        .is_some_and(|byte| byte & (1 << (i % 8)) != 0)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn tri_hash_of_known_trigrams() {
        assert_eq!(tri_hash(0, 0, 0), 0);
        assert_eq!(tri_hash(1, 0, 0), (0x9E37_79B9_7F4A_7C15u64 >> 32) as u32);
    }

    #[test]
    fn filter_bits_rounds_and_clamps() {
        assert_eq!(filter_bits(0), 1024);
        assert_eq!(filter_bits(512), 1024);
        assert_eq!(filter_bits(513), 1088);
        assert_eq!(filter_bits(10_000), 20_032);
    }

    #[test]
    fn filter_bits_saturates_at_the_largest_multiple_of_64() {
        let bits = filter_bits(usize::MAX);
        assert_eq!(bits, MAX_FILTER_BITS);
        assert_eq!(bits % 64, 0);
    }

    #[test]
    fn short_inputs_add_no_trigrams() {
        let mut set = HashSet::new();
        add_trigrams(b"ab", &mut set);
        assert!(set.is_empty());
        add_trigrams(b"abcd", &mut set);
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn bits_round_trip_through_a_bitmap() {
        let mut bitmap = vec![0u8; 16];
        set_bit(&mut bitmap, 0);
        set_bit(&mut bitmap, 9);
        set_bit(&mut bitmap, 127);
        assert!(get_bit(&bitmap, 0));
        assert!(get_bit(&bitmap, 9));
        assert!(get_bit(&bitmap, 127));
        assert!(!get_bit(&bitmap, 1));
        assert!(!get_bit(&bitmap, 128));
        assert_eq!(bitmap[0], 1);
        assert_eq!(bitmap[1], 2);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn filter_bit_is_in_range(h in any::<u32>(), k in 16u32..=(1 << 18)) {
            let bits = k * 64;
            prop_assert!(filter_bit(h, bits) < bits);
        }
    }
}
