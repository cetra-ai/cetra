//! Lengths blocks: `n` lengths stored as one base and bit-packed offsets from it. `VARBIN`,
//! `LIST_DICT` and `MAP` store their lengths this way instead of cumulative offsets, so the
//! bodies hold small repeated bit patterns that zstd compresses well.

use super::bitpack::{pack, unpack, width_for};
use crate::error::{Result, StrataError};
use crate::format::Cursor;

/// Appends a lengths block for `lens`: `u32 base · u8 width · packed (len − base)`. `lens`
/// has its null slots set to 0 already, so the base is 0 whenever there is a null.
pub(crate) fn encode_lengths(lens: &[u32], out: &mut Vec<u8>) {
    let base = lens.iter().copied().min().unwrap_or(0);
    let relative: Vec<u64> = lens
        .iter()
        .map(|len| u64::from(len.saturating_sub(base)))
        .collect();
    let width = width_for(relative.iter().copied().max().unwrap_or(0));
    out.extend_from_slice(&base.to_le_bytes());
    out.push(width);
    pack(&relative, width, out);
}

/// Reads a lengths block of `n` lengths. The packed bytes are checked against the body before
/// anything is allocated, and every failure is `Corrupt` at `at`.
pub(crate) fn decode_lengths(c: &mut Cursor<'_>, n: usize, at: u64) -> Result<Vec<u32>> {
    let corrupt = || StrataError::Corrupt {
        what: "lengths",
        offset: at,
    };
    let base = c.u32("lengths")?;
    let width = c.u8("lengths")?;
    if width > 32 {
        return Err(corrupt());
    }
    let packed = n
        .checked_mul(usize::from(width))
        .map(|bits| bits.div_ceil(64))
        .and_then(|words| words.checked_mul(8))
        .ok_or_else(corrupt)?;
    let bytes = c.bytes(packed, "lengths")?;
    let relative = unpack(bytes, n, width).map_err(|_| corrupt())?;
    relative
        .into_iter()
        .map(|r| {
            let len = u64::from(base).checked_add(r).ok_or_else(corrupt)?;
            u32::try_from(len).map_err(|_| corrupt())
        })
        .collect()
}

/// The sum of `lens` as a byte or element count. A sum that does not fit in `usize` is
/// `Corrupt { what, offset: at }`.
pub(crate) fn total_of(lens: &[u32], what: &'static str, at: u64) -> Result<usize> {
    lens.iter()
        .try_fold(0usize, |acc, len| {
            usize::try_from(*len)
                .ok()
                .and_then(|len| acc.checked_add(len))
        })
        .ok_or(StrataError::Corrupt { what, offset: at })
}

/// The `i32` offsets of `lens`: `lens.len() + 1` values, starting at 0. An offset that does
/// not fit in `i32` is `Corrupt { what, offset: at }`.
pub(crate) fn offsets_of(lens: &[u32], what: &'static str, at: u64) -> Result<Vec<i32>> {
    let corrupt = || StrataError::Corrupt { what, offset: at };
    let mut offsets = Vec::with_capacity(lens.len() + 1);
    let mut total = 0i32;
    offsets.push(total);
    for len in lens {
        let len = i32::try_from(*len).map_err(|_| corrupt())?;
        total = total.checked_add(len).ok_or_else(corrupt)?;
        offsets.push(total);
    }
    Ok(offsets)
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

    fn round_trip(lens: &[u32]) {
        let mut body = Vec::new();
        encode_lengths(lens, &mut body);
        let mut c = Cursor::new(&body, 0);
        assert_eq!(decode_lengths(&mut c, lens.len(), 0).unwrap(), lens);
        assert_eq!(c.remaining(), 0);
    }

    #[test]
    fn round_trips_empty_equal_mixed_and_extreme_lengths() {
        round_trip(&[]);
        round_trip(&[7, 7, 7, 7]);
        round_trip(&[0, 3, 1, 0, 65_536]);
        round_trip(&[0, u32::MAX]);
    }

    #[test]
    fn an_empty_list_is_five_bytes() {
        let mut body = Vec::new();
        encode_lengths(&[], &mut body);
        assert_eq!(body, vec![0, 0, 0, 0, 0]);
    }

    #[test]
    fn width_above_32_is_corrupt() {
        let mut body = vec![0, 0, 0, 0, 33];
        body.extend_from_slice(&[0u8; 16]);
        let mut c = Cursor::new(&body, 0);
        assert!(matches!(
            decode_lengths(&mut c, 2, 0),
            Err(StrataError::Corrupt {
                what: "lengths",
                ..
            })
        ));
    }

    #[test]
    fn a_short_body_is_corrupt() {
        let mut body = Vec::new();
        encode_lengths(&[1, 2, 3, 4_000], &mut body);
        body.pop();
        let mut c = Cursor::new(&body, 0);
        assert!(matches!(
            decode_lengths(&mut c, 4, 0),
            Err(StrataError::Corrupt { .. })
        ));
    }

    #[test]
    fn offsets_past_i32_are_corrupt() {
        let lens = [u32::MAX, u32::MAX];
        assert!(total_of(&lens, "test", 0).is_ok());
        let offsets = offsets_of(&lens, "test", 0);
        assert!(matches!(
            offsets,
            Err(StrataError::Corrupt { what: "test", .. })
        ));
    }

    proptest! {
        #[test]
        fn random_bytes_never_panic(
            n in 0usize..200,
            body in proptest::collection::vec(any::<u8>(), 0..512),
        ) {
            let mut c = Cursor::new(&body, 0);
            let _ = decode_lengths(&mut c, n, 0);
        }
    }
}
