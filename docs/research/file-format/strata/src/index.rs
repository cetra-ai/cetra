//! Key filters (S5) and the zone test of sort-key lookup.
//!
//! A key filter answers "may this value occur in the column?" from a binary fuse filter over
//! the `xxh3_64` hashes of the column's values. [`key_bytes`] gives the bytes that are hashed.

use std::ops::Range;

use arrow::array::{Array, AsArray, FixedSizeBinaryArray, StringArray};
use arrow::compute::cast;
use arrow::datatypes::{DataType, Int64Type};
use xorf::{BinaryFuse8, Descriptor, Filter};
use xxhash_rust::xxh3::xxh3_64;

use crate::error::{Result, StrataError};
use crate::format::Cursor;
use crate::narrow::{BlockEntry, flags};

/// The longest value a tag-1 filter stores inline.
const INLINE_MAX: usize = 1024;

const TAG_EMPTY: u8 = 0;
const TAG_ONE: u8 = 1;
const TAG_FUSE: u8 = 2;
const TAG_UNKNOWN: u8 = 3;

/// The bytes each row of `array` contributes to a key filter: `None` for a null row. `None` for
/// the whole column when its type has no key bytes. Integers and timestamps are their value as
/// an 8-byte little-endian `i64`.
pub(crate) fn key_bytes(array: &dyn Array) -> Option<Vec<Option<Vec<u8>>>> {
    match array.data_type() {
        DataType::FixedSizeBinary(_) => {
            let values = array.as_any().downcast_ref::<FixedSizeBinaryArray>()?;
            Some(
                (0..values.len())
                    .map(|i| values.is_valid(i).then(|| values.value(i).to_vec()))
                    .collect(),
            )
        }
        DataType::Utf8 => {
            let values = array.as_any().downcast_ref::<StringArray>()?;
            Some(
                (0..values.len())
                    .map(|i| {
                        values
                            .is_valid(i)
                            .then(|| values.value(i).as_bytes().to_vec())
                    })
                    .collect(),
            )
        }
        DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32
        | DataType::Timestamp(_, _)
        | DataType::Date32
        | DataType::Date64 => {
            let wide = cast(array, &DataType::Int64).ok()?;
            let values = wide.as_primitive_opt::<Int64Type>()?;
            Some(
                (0..values.len())
                    .map(|i| {
                        values
                            .is_valid(i)
                            .then(|| values.value(i).to_le_bytes().to_vec())
                    })
                    .collect(),
            )
        }
        _ => None,
    }
}

/// A key filter of one column: what the body of section 4 or of a key-filter segment holds.
pub(crate) enum KeyFilter {
    /// The column has no non-null value.
    Empty,
    /// One distinct value, stored inline.
    One(Vec<u8>),
    /// A binary fuse filter over the hashes of the values.
    Fuse(BinaryFuse8),
    /// The filter could not be built.
    Unknown,
}

impl KeyFilter {
    /// Builds the filter over `values`: any order, repeats allowed.
    pub(crate) fn build(values: &[Vec<u8>]) -> KeyFilter {
        let mut hashes: Vec<u64> = values.iter().map(|value| xxh3_64(value)).collect();
        hashes.sort_unstable();
        hashes.dedup();
        match (hashes.len(), values.first()) {
            (0, _) => KeyFilter::Empty,
            // With one distinct hash, every value is the same, so the first one is the value.
            (1, Some(value)) if value.len() <= INLINE_MAX => KeyFilter::One(value.clone()),
            _ => fuse(&hashes),
        }
    }

    /// Appends the body: the tag, then the fields of that tag (S5).
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        match self {
            KeyFilter::Empty => out.push(TAG_EMPTY),
            KeyFilter::One(value) => {
                out.push(TAG_ONE);
                out.extend_from_slice(&(value.len() as u32).to_le_bytes());
                out.extend_from_slice(value);
            }
            KeyFilter::Fuse(filter) => {
                let d = &filter.descriptor;
                out.push(TAG_FUSE);
                out.extend_from_slice(&d.seed.to_le_bytes());
                out.extend_from_slice(&d.segment_length.to_le_bytes());
                out.extend_from_slice(&d.segment_length_mask.to_le_bytes());
                out.extend_from_slice(&d.segment_count_length.to_le_bytes());
                // `build` keeps the fingerprint count within u32.
                out.extend_from_slice(&(filter.fingerprints.len() as u32).to_le_bytes());
                out.extend_from_slice(&filter.fingerprints);
            }
            KeyFilter::Unknown => out.push(TAG_UNKNOWN),
        }
    }

    /// Decodes a body that starts at file offset `offset`. Every check of S5 is made before
    /// anything is allocated, so that `may_contain` cannot index out of bounds.
    pub(crate) fn decode(bytes: &[u8], offset: u64) -> Result<KeyFilter> {
        let corrupt = || StrataError::Corrupt {
            what: "key filter",
            offset,
        };
        let mut c = Cursor::new(bytes, offset);
        let tag = c.u8("key filter")?;
        let filter = match tag {
            TAG_EMPTY => KeyFilter::Empty,
            TAG_UNKNOWN => KeyFilter::Unknown,
            TAG_ONE => {
                let len = usize::try_from(c.u32("key filter")?).map_err(|_| corrupt())?;
                if len > INLINE_MAX {
                    return Err(corrupt());
                }
                KeyFilter::One(c.bytes(len, "key filter")?.to_vec())
            }
            TAG_FUSE => {
                let seed = c.u64("key filter")?;
                let segment_length = c.u32("key filter")?;
                let segment_length_mask = c.u32("key filter")?;
                let segment_count_length = c.u32("key filter")?;
                let n = c.u32("key filter")?;
                // The geometry keeps `contains` in bounds: its indexes are below
                // `scl`, `scl + sl` and `scl + 2·sl`, and `scl ≥ sl` excludes `scl = 0`.
                let geometry_ok = segment_length.is_power_of_two()
                    && segment_length_mask == segment_length - 1
                    && segment_count_length >= segment_length
                    && segment_count_length % segment_length == 0
                    && u64::from(n)
                        == u64::from(segment_count_length) + 2 * u64::from(segment_length);
                if !geometry_ok {
                    return Err(corrupt());
                }
                let fingerprints = c.bytes(n as usize, "key filter")?;
                KeyFilter::Fuse(BinaryFuse8 {
                    descriptor: Descriptor {
                        seed,
                        segment_length,
                        segment_length_mask,
                        segment_count_length,
                    },
                    fingerprints: fingerprints.into(),
                })
            }
            _ => return Err(corrupt()),
        };
        if c.remaining() != 0 {
            return Err(corrupt());
        }
        Ok(filter)
    }

    /// Whether `value` may be in the column. `Some(false)` is a proof of absence; `None` means
    /// the filter cannot say.
    pub(crate) fn may_contain(&self, value: &[u8]) -> Option<bool> {
        match self {
            KeyFilter::Empty => Some(false),
            KeyFilter::One(one) => Some(one.as_slice() == value),
            KeyFilter::Fuse(filter) => Some(filter.contains(&xxh3_64(value))),
            KeyFilter::Unknown => None,
        }
    }
}

/// Builds a fuse filter over distinct `hashes`, or `Unknown` when construction fails.
fn fuse(hashes: &[u64]) -> KeyFilter {
    match BinaryFuse8::try_from(hashes) {
        Ok(filter) if filter.fingerprints.len() <= u32::MAX as usize => KeyFilter::Fuse(filter),
        _ => KeyFilter::Unknown,
    }
}

/// Whether the zone of `entry` may hold `key`. An entry without a zone may hold any key. The
/// zone is compared bytewise over the first `key.len()` bytes.
pub(crate) fn zone_may_hold(entry: &BlockEntry, key: &[u8]) -> bool {
    if entry.flags & flags::HAS_ZONE == 0 {
        return true;
    }
    match (entry.min.get(..key.len()), entry.max.get(..key.len())) {
        (Some(low), Some(high)) => low <= key && key <= high,
        _ => false,
    }
}

/// Merges ascending rows into row ranges, with adjacent rows in one range. Each row must be
/// below `u32::MAX`.
pub(crate) fn merge_rows(rows: &[u32]) -> Vec<Range<u32>> {
    let mut ranges: Vec<Range<u32>> = Vec::new();
    for &row in rows {
        match ranges.last_mut() {
            Some(last) if last.end == row => last.end = row + 1,
            _ => ranges.push(row..row + 1),
        }
    }
    ranges
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
    use arrow::array::{Int32Array, TimestampNanosecondArray, UInt64Array};
    use arrow::buffer::Buffer;
    use proptest::prelude::*;
    use xxhash_rust::xxh3::xxh3_128;

    /// The 16-byte test value `i`.
    fn value_of(i: u64) -> Vec<u8> {
        xxh3_128(&i.to_le_bytes()).to_le_bytes().to_vec()
    }

    fn distinct(n: u64) -> Vec<Vec<u8>> {
        (0..n).map(value_of).collect()
    }

    fn tag(filter: &KeyFilter) -> u8 {
        let mut body = Vec::new();
        filter.encode(&mut body);
        body[0]
    }

    fn is_corrupt<T>(result: &Result<T>) -> bool {
        matches!(
            result,
            Err(StrataError::Corrupt {
                what: "key filter",
                ..
            })
        )
    }

    /// A tag-2 body with the given descriptor fields and `fingerprints` filler bytes.
    fn fuse_body(sl: u32, mask: u32, scl: u32, n: u32, fingerprints: usize) -> Vec<u8> {
        let mut body = vec![TAG_FUSE];
        body.extend_from_slice(&7u64.to_le_bytes());
        body.extend_from_slice(&sl.to_le_bytes());
        body.extend_from_slice(&mask.to_le_bytes());
        body.extend_from_slice(&scl.to_le_bytes());
        body.extend_from_slice(&n.to_le_bytes());
        body.extend(vec![0xA5u8; fingerprints]);
        body
    }

    #[test]
    fn tags_follow_the_number_of_distinct_values() {
        assert_eq!(tag(&KeyFilter::build(&[])), 0);
        assert_eq!(tag(&KeyFilter::build(&[b"short".to_vec()])), 1);
        assert_eq!(tag(&KeyFilter::build(&[vec![7u8; 2048]])), 2);
        assert_eq!(tag(&KeyFilter::build(&distinct(10_000))), 2);
    }

    #[test]
    fn filters_round_trip_and_contain_their_values() {
        let cases = [
            Vec::new(),
            vec![b"one".to_vec()],
            vec![vec![1u8; 2048]],
            distinct(1_000),
        ];
        for values in cases {
            let filter = KeyFilter::build(&values);
            let mut body = Vec::new();
            filter.encode(&mut body);
            let decoded = KeyFilter::decode(&body, 0).unwrap();
            let mut again = Vec::new();
            decoded.encode(&mut again);
            assert_eq!(again, body);
            for value in &values {
                assert_eq!(decoded.may_contain(value), Some(true));
            }
        }
    }

    #[test]
    fn empty_and_unknown_answer_without_values() {
        assert_eq!(KeyFilter::Empty.may_contain(b"x"), Some(false));
        assert_eq!(KeyFilter::Unknown.may_contain(b"x"), None);
        let one = KeyFilter::build(&[b"x".to_vec()]);
        assert_eq!(one.may_contain(b"x"), Some(true));
        assert_eq!(one.may_contain(b"y"), Some(false));
    }

    #[test]
    fn false_positive_rate_is_low() {
        let filter = KeyFilter::build(&distinct(10_000));
        let false_positives = (10_000..20_000)
            .filter(|&i| filter.may_contain(&value_of(i)) == Some(true))
            .count();
        assert!(false_positives <= 200, "{false_positives} false positives");
    }

    #[test]
    fn corrupt_bodies_are_corrupt_errors() {
        let mut one_too_long = vec![TAG_ONE];
        one_too_long.extend_from_slice(&1025u32.to_le_bytes());
        one_too_long.extend(vec![0u8; 1025]);
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("unknown tag", vec![9]),
            ("tag 0 with a trailing byte", vec![TAG_EMPTY, 0]),
            ("tag 1 with len 1025", one_too_long),
            ("segment length 3", fuse_body(3, 2, 6, 12, 12)),
            ("a wrong mask", fuse_body(4, 2, 8, 16, 16)),
            ("scl 0, sl 1, mask 0, n 2", fuse_body(1, 0, 0, 2, 2)),
            ("scl not a multiple of sl", fuse_body(4, 3, 6, 14, 14)),
            ("n not scl + 2 sl", fuse_body(4, 3, 8, 15, 15)),
            ("n past the end of the input", fuse_body(4, 3, 8, 16, 8)),
        ];
        for (name, body) in cases {
            let result = KeyFilter::decode(&body, 0);
            assert!(is_corrupt(&result), "{name} must be a Corrupt error");
        }
    }

    proptest! {
        #[test]
        fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
            if let Ok(filter) = KeyFilter::decode(&bytes, 0) {
                let _ = filter.may_contain(b"x");
            }
        }

        #[test]
        fn well_formed_geometry_never_panics(
            k in 0u32..10,
            m in 1u32..64,
            seed in any::<u64>(),
            probe in any::<u64>(),
        ) {
            let sl = 1u32 << k;
            let scl = sl * m;
            let n = scl + 2 * sl;
            let mut body = vec![TAG_FUSE];
            body.extend_from_slice(&seed.to_le_bytes());
            body.extend_from_slice(&sl.to_le_bytes());
            body.extend_from_slice(&(sl - 1).to_le_bytes());
            body.extend_from_slice(&scl.to_le_bytes());
            body.extend_from_slice(&n.to_le_bytes());
            let mut x = seed | 1;
            for _ in 0..n {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                body.push(x as u8);
            }
            let filter = KeyFilter::decode(&body, 0).unwrap();
            let mut y = probe | 1;
            for _ in 0..1_000 {
                y ^= y << 13;
                y ^= y >> 7;
                y ^= y << 17;
                let _ = filter.may_contain(&y.to_le_bytes());
            }
        }
    }

    #[test]
    fn key_bytes_of_the_supported_types() {
        let ints = Int32Array::from(vec![Some(5), None]);
        assert_eq!(
            key_bytes(&ints),
            Some(vec![Some(5i64.to_le_bytes().to_vec()), None])
        );
        let times = TimestampNanosecondArray::from(vec![Some(-3i64)]);
        assert_eq!(
            key_bytes(&times),
            Some(vec![Some((-3i64).to_le_bytes().to_vec())])
        );
        let strings = StringArray::from(vec![Some("é"), None]);
        assert_eq!(
            key_bytes(&strings),
            Some(vec![Some("é".as_bytes().to_vec()), None])
        );
        let ids =
            FixedSizeBinaryArray::try_new(2, Buffer::from_vec(vec![1u8, 2, 3, 4]), None).unwrap();
        assert_eq!(
            key_bytes(&ids),
            Some(vec![Some(vec![1, 2]), Some(vec![3, 4])])
        );
        assert_eq!(key_bytes(&UInt64Array::from(vec![1u64])), None);
    }

    #[test]
    fn merge_rows_joins_adjacent_rows() {
        assert_eq!(merge_rows(&[]), Vec::<Range<u32>>::new());
        assert_eq!(merge_rows(&[1, 2, 3, 7, 9, 10]), vec![1..4, 7..8, 9..11]);
    }
}
