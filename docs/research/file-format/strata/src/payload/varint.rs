//! Unsigned LEB128 varints, and the zigzag mapping of signed deltas to unsigned values.

use crate::error::{Result, StrataError};
use crate::format::Cursor;

/// Appends `v` as an unsigned LEB128 varint, least significant group first.
pub(crate) fn put_uvarint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let low = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(low);
            return;
        }
        out.push(low | 0x80);
    }
}

/// Reads an unsigned LEB128 varint. At most 10 bytes are read; the 10th byte must end the
/// value and hold only bit 63. Non-canonical encodings are accepted.
pub(crate) fn get_uvarint(c: &mut Cursor<'_>, what: &'static str) -> Result<u64> {
    let mut value = 0u64;
    for shift in 0..10u32 {
        let byte = c.u8(what)?;
        let bits = u64::from(byte & 0x7f);
        if shift == 9 {
            if byte & 0x80 != 0 || bits > 1 {
                return Err(StrataError::Corrupt {
                    what,
                    offset: c.position(),
                });
            }
            return Ok(value | (bits << 63));
        }
        value |= bits << (7 * shift);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(StrataError::Corrupt {
        what,
        offset: c.position(),
    })
}

/// Maps a signed value to an unsigned one, so that small magnitudes stay small.
pub(crate) fn zigzag(d: i64) -> u64 {
    ((d << 1) ^ (d >> 63)) as u64
}

/// The inverse of [`zigzag`].
pub(crate) fn unzigzag(z: u64) -> i64 {
    ((z >> 1) as i64) ^ -((z & 1) as i64)
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

    fn round_trip(v: u64) -> u64 {
        let mut out = Vec::new();
        put_uvarint(&mut out, v);
        let mut c = Cursor::new(&out, 0);
        let got = get_uvarint(&mut c, "test").unwrap();
        assert_eq!(c.remaining(), 0);
        got
    }

    fn is_corrupt(bytes: &[u8]) -> bool {
        let mut c = Cursor::new(bytes, 0);
        matches!(
            get_uvarint(&mut c, "test"),
            Err(StrataError::Corrupt { .. })
        )
    }

    #[test]
    fn fixed_values_round_trip() {
        for v in [0, 1, 127, 128, u64::from(u32::MAX), u64::MAX] {
            assert_eq!(round_trip(v), v);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1000))]

        #[test]
        fn random_values_round_trip(v in any::<u64>()) {
            prop_assert_eq!(round_trip(v), v);
        }
    }

    #[test]
    fn zigzag_of_edge_values() {
        assert_eq!(zigzag(i64::MIN), u64::MAX);
        assert_eq!(zigzag(-1), 1);
        assert_eq!(zigzag(0), 0);
        assert_eq!(zigzag(1), 2);
        assert_eq!(zigzag(i64::MAX), u64::MAX - 1);
        for d in [i64::MIN, -1, 0, 1, i64::MAX] {
            assert_eq!(unzigzag(zigzag(d)), d);
        }
    }

    #[test]
    fn eleven_byte_encodings_are_corrupt() {
        let mut bytes = vec![0x80u8; 10];
        bytes.push(0x00);
        assert!(is_corrupt(&bytes));
    }

    #[test]
    fn tenth_byte_above_one_is_corrupt() {
        let mut bytes = vec![0x80u8; 9];
        bytes.push(0x02);
        assert!(is_corrupt(&bytes));
        // A tenth byte of 1 is bit 63, which is valid.
        let mut max = vec![0xffu8; 9];
        max.push(0x01);
        assert_eq!(round_trip(u64::MAX), u64::MAX);
        let mut c = Cursor::new(&max, 0);
        assert_eq!(get_uvarint(&mut c, "test").unwrap(), u64::MAX);
    }

    #[test]
    fn truncated_varint_is_corrupt() {
        assert!(is_corrupt(&[0x80]));
        assert!(is_corrupt(&[]));
    }

    #[test]
    fn non_canonical_encoding_is_accepted() {
        let mut c = Cursor::new(&[0x80, 0x00], 0);
        assert_eq!(get_uvarint(&mut c, "test").unwrap(), 0);
        assert_eq!(c.remaining(), 0);
    }
}
