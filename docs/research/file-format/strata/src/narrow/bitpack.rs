//! Bit packing of `u64` values at a fixed width, for `FOR_BITPACK`.
//!
//! Value `i` occupies bits `[i·width, (i+1)·width)` of a little-endian bit stream, which
//! is stored as `ceil(n·width / 64)` little-endian `u64` words.

use crate::error::{Result, StrataError};

/// The width needed to store `max_delta`: the number of significant bits.
pub fn width_for(max_delta: u64) -> u8 {
    (64 - max_delta.leading_zeros()) as u8
}

/// The low `width` bits set.
fn mask(width: u8) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    }
}

/// Appends `values`, each packed into `width` bits. Width 0 writes nothing.
pub fn pack(values: &[u64], width: u8, out: &mut Vec<u8>) {
    if width == 0 {
        return;
    }
    let w = usize::from(width);
    let words = values.len().saturating_mul(w).div_ceil(64);
    let mut buf = vec![0u64; words];
    let m = mask(width);
    for (i, value) in values.iter().enumerate() {
        let v = value & m;
        let bit = i * w;
        let word = bit / 64;
        let shift = (bit % 64) as u32;
        if let Some(slot) = buf.get_mut(word) {
            *slot |= v << shift;
        }
        if shift + u32::from(width) > 64
            && let Some(slot) = buf.get_mut(word + 1)
        {
            *slot |= v >> (64 - shift);
        }
    }
    for word in buf {
        out.extend_from_slice(&word.to_le_bytes());
    }
}

/// Checks that `bytes` is exactly the packed length of `n` values of `width` bits.
fn check_packed(bytes: &[u8], n: usize, width: u8) -> Result<()> {
    let corrupt = || StrataError::Corrupt {
        what: "bit-packed values",
        offset: 0,
    };
    if width > 64 {
        return Err(corrupt());
    }
    let expected = n
        .checked_mul(usize::from(width))
        .ok_or_else(corrupt)?
        .div_ceil(64)
        .checked_mul(8)
        .ok_or_else(corrupt)?;
    if bytes.len() != expected {
        return Err(corrupt());
    }
    Ok(())
}

/// The 8 bytes of `bytes` from `at`, little-endian, zero-padded past the end.
fn word_at(bytes: &[u8], at: usize) -> u64 {
    if let Some(word) = bytes.get(at..).and_then(<[u8]>::first_chunk::<8>) {
        return u64::from_le_bytes(*word);
    }
    let mut padded = [0u8; 8];
    let rest = bytes.get(at..).unwrap_or_default();
    if let Some(head) = padded.get_mut(..rest.len()) {
        head.copy_from_slice(rest);
    }
    u64::from_le_bytes(padded)
}

/// Calls `f` with each of the `n` values of `width` bits in `bytes`, in order. `bytes` must
/// already be checked. Values of at most 56 bits are read with one unaligned load each.
fn for_each_value(bytes: &[u8], n: usize, width: u8, mut f: impl FnMut(u64)) {
    let w = usize::from(width);
    let m = mask(width);
    if width == 0 {
        (0..n).for_each(|_| f(0));
    } else if width <= 56 {
        for i in 0..n {
            let bit = i * w;
            f((word_at(bytes, bit >> 3) >> (bit & 7)) & m);
        }
    } else {
        for i in 0..n {
            let bit = i * w;
            let word = (bit / 64) * 8;
            let shift = (bit % 64) as u32;
            let mut v = word_at(bytes, word) >> shift;
            if shift + u32::from(width) > 64 {
                v |= word_at(bytes, word + 8) << (64 - shift);
            }
            f(v & m);
        }
    }
}

/// Writes `f` of each of the `n` values of `width` bits in `bytes` into `out`, in order.
/// `bytes` must be exactly the packed length, as for [`unpack`]. Up to 56 bits a
/// value is one unaligned load; the loop extends `out` from an exact-size iterator, so it
/// checks the capacity once, not per value.
pub fn extend_packed<T>(
    out: &mut Vec<T>,
    bytes: &[u8],
    n: usize,
    width: u8,
    mut f: impl FnMut(u64) -> T,
) -> Result<()> {
    check_packed(bytes, n, width)?;
    if width == 0 || width > 56 {
        out.reserve(n);
        for_each_value(bytes, n, width, |v| out.push(f(v)));
        return Ok(());
    }
    if width <= 32 {
        extend_narrow(out, bytes, n, width, f);
        return Ok(());
    }
    let w = usize::from(width);
    let m = mask(width);
    out.extend((0..n).map(|i| {
        let bit = i * w;
        f((word_at(bytes, bit >> 3) >> (bit & 7)) & m)
    }));
    Ok(())
}

/// [`extend_packed`] for a width of 1 to 32 bits, through the kernel of that width.
fn extend_narrow<T>(out: &mut Vec<T>, bytes: &[u8], n: usize, width: u8, f: impl FnMut(u64) -> T) {
    macro_rules! kernels {
        ($($w:literal)*) => {
            match width {
                $($w => extend_width::<T, $w>(out, bytes, n, f),)*
                _ => {}
            }
        };
    }
    kernels!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32);
}

/// Unpacks `n` values of `W` bits (1 to 32) into `out`. Eight values take exactly `W` bytes, so
/// each group of eight is copied into a fixed buffer and its values read at constant offsets;
/// the last values, fewer than eight, are read one by one.
fn extend_width<T, const W: usize>(
    out: &mut Vec<T>,
    bytes: &[u8],
    n: usize,
    mut f: impl FnMut(u64) -> T,
) {
    let m = mask(W as u8);
    let groups = n / 8;
    out.reserve(n);
    for group in bytes.as_chunks::<W>().0.iter().take(groups) {
        // `W` bytes plus room for the last 8-byte load of the group.
        let mut buf = [0u8; 40];
        if let Some(head) = buf.get_mut(..W) {
            head.copy_from_slice(group);
        }
        let values: [u64; 8] = std::array::from_fn(|j| {
            let bit = j * W;
            let word = buf
                .get(bit / 8..)
                .and_then(<[u8]>::first_chunk::<8>)
                .map_or(0, |w| u64::from_le_bytes(*w));
            (word >> (bit % 8)) & m
        });
        out.extend(values.map(&mut f));
    }
    for i in groups * 8..n {
        let bit = i * W;
        out.push(f((word_at(bytes, bit >> 3) >> (bit & 7)) & m));
    }
}

/// Writes `f` of the values at positions `at` (each below `n`) of the `n` values of `width`
/// bits in `bytes` into `out`, in the order of `at`. `bytes` must be exactly the packed length;
/// a wrong length, a width above 64 or a position past `n` is `Corrupt`, and `out` is then
/// left as it was.
pub fn pick_packed<T>(
    out: &mut Vec<T>,
    bytes: &[u8],
    n: usize,
    width: u8,
    at: &[u32],
    mut f: impl FnMut(u64) -> T,
) -> Result<()> {
    check_packed(bytes, n, width)?;
    if at.iter().any(|&i| i as usize >= n) {
        return Err(StrataError::Corrupt {
            what: "bit-packed position",
            offset: 0,
        });
    }
    let w = usize::from(width);
    let m = mask(width);
    out.reserve(at.len());
    for &i in at {
        let bit = i as usize * w;
        let v = if width == 0 {
            0
        } else if width <= 56 {
            (word_at(bytes, bit >> 3) >> (bit & 7)) & m
        } else {
            let word = (bit / 64) * 8;
            let shift = (bit % 64) as u32;
            let mut v = word_at(bytes, word) >> shift;
            if shift + u32::from(width) > 64 {
                v |= word_at(bytes, word + 8) << (64 - shift);
            }
            v & m
        };
        out.push(f(v));
    }
    Ok(())
}

/// Reads `n` values of `width` bits from `bytes`. `bytes` must be exactly the packed
/// length; a wrong length, or a width above 64, is `Corrupt`.
pub fn unpack(bytes: &[u8], n: usize, width: u8) -> Result<Vec<u64>> {
    check_packed(bytes, n, width)?;
    let mut out = Vec::with_capacity(n);
    for_each_value(bytes, n, width, |v| out.push(v));
    Ok(out)
}

/// Like [`unpack`], for values of at most 32 bits, as `u32`. A width above 32 is `Corrupt`.
pub fn unpack_u32(bytes: &[u8], n: usize, width: u8) -> Result<Vec<u32>> {
    check_packed(bytes, n, width)?;
    if width > 32 {
        return Err(StrataError::Corrupt {
            what: "bit-packed values",
            offset: 0,
        });
    }
    let mut out = Vec::with_capacity(n);
    // The width check makes every value fit in 32 bits.
    for_each_value(bytes, n, width, |v| out.push(v as u32));
    Ok(out)
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

    proptest! {
        #[test]
        fn pack_then_unpack_round_trips(
            width in 0u8..=64,
            values in proptest::collection::vec(any::<u64>(), 0..5_000),
        ) {
            let m = mask(width);
            let masked: Vec<u64> = values.iter().map(|v| v & m).collect();
            let mut bytes = Vec::new();
            pack(&masked, width, &mut bytes);
            prop_assert_eq!(unpack(&bytes, masked.len(), width).unwrap(), masked.clone());
            let mut extended = vec![7u64];
            extend_packed(&mut extended, &bytes, masked.len(), width, |v| v).unwrap();
            prop_assert_eq!(extended[0], 7);
            prop_assert_eq!(&extended[1..], &masked[..]);
            if width <= 32 {
                let small: Vec<u32> = masked.iter().map(|&v| v as u32).collect();
                prop_assert_eq!(unpack_u32(&bytes, masked.len(), width).unwrap(), small);
            } else {
                prop_assert!(unpack_u32(&bytes, masked.len(), width).is_err());
            }
        }
    }

    #[test]
    fn every_width_extends_like_it_unpacks() {
        for width in 0u8..=64 {
            for n in [0usize, 1, 7, 8, 9, 63, 64, 65, 2048, 2051] {
                let m = mask(width);
                let values: Vec<u64> = (0..n as u64)
                    .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17) & m)
                    .collect();
                let mut bytes = Vec::new();
                pack(&values, width, &mut bytes);
                let mut out = Vec::new();
                extend_packed(&mut out, &bytes, n, width, |v| v).unwrap();
                assert_eq!(out, values, "width {width}, {n} values");
            }
        }
    }

    #[test]
    fn picked_values_match_the_unpacked_ones() {
        for width in 0u8..=64 {
            let n = 300usize;
            let m = mask(width);
            let values: Vec<u64> = (0..n as u64)
                .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29) & m)
                .collect();
            let mut bytes = Vec::new();
            pack(&values, width, &mut bytes);
            let at = [299u32, 0, 7, 8, 63, 64, 65, 150, 150];
            let mut out = Vec::new();
            pick_packed(&mut out, &bytes, n, width, &at, |v| v).unwrap();
            let want: Vec<u64> = at.iter().map(|&i| values[i as usize]).collect();
            assert_eq!(out, want, "width {width}");
            // A position past the values is an error, and writes nothing.
            let mut out = Vec::new();
            assert!(pick_packed(&mut out, &bytes, n, width, &[1, 300], |v| v).is_err());
            assert!(out.is_empty());
        }
    }

    #[test]
    fn wrong_length_or_width_is_an_error() {
        let mut bytes = Vec::new();
        pack(&[1, 2, 3], 5, &mut bytes);
        assert!(unpack(&bytes, 3, 5).is_ok());
        assert!(unpack(&bytes[..bytes.len() - 1], 3, 5).is_err());
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(unpack(&longer, 3, 5).is_err());
        assert!(unpack(&bytes, 3, 65).is_err());
        let mut out: Vec<u64> = Vec::new();
        assert!(extend_packed(&mut out, &bytes[..bytes.len() - 1], 3, 5, |v| v).is_err());
        assert!(extend_packed(&mut out, &bytes, 3, 65, |v| v).is_err());
        assert!(out.is_empty());
    }
}
