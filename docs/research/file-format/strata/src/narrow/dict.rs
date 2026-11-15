//! Sorted file-level dictionaries for Utf8 columns and the elements of List(Utf8) columns.

use std::collections::BTreeSet;

use arrow::array::{Array, AsArray, ListArray, StringArray};
use arrow::datatypes::DataType;

use crate::config::Config;
use crate::error::{Result, StrataError};
use crate::format::{Cursor, MAX_DICT_VALUES, Out};

/// The distinct values of a column, sorted bytewise. A code is the index of a value. Every
/// value is valid UTF-8, checked once when the dictionary is built or decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dictionary {
    values: Vec<String>,
}

impl Dictionary {
    /// The code of `v`, or `None` when `v` is not in the dictionary.
    pub fn code_of(&self, v: &[u8]) -> Option<u32> {
        self.values
            .binary_search_by(|x| x.as_bytes().cmp(v))
            .ok()
            .and_then(|i| u32::try_from(i).ok())
    }

    /// The value of `code`, or `None` when the code is out of range.
    pub fn value(&self, code: u32) -> Option<&[u8]> {
        self.values.get(code as usize).map(String::as_bytes)
    }

    /// The value of `code` as a string, or `None` when the code is out of range.
    pub fn str_value(&self, code: u32) -> Option<&str> {
        self.values.get(code as usize).map(String::as_str)
    }

    /// Writes the S3 layout: `u32 n`, `u32 offsets[n+1]` relative to the bytes area, then
    /// the bytes.
    pub fn encode(&self, out: &mut Out) -> Result<()> {
        let n = u32::try_from(self.values.len()).map_err(|_| StrataError::Unsupported {
            what: "a dictionary of more than 4 billion values".to_string(),
        })?;
        out.u32(n);
        let mut offset = 0u64;
        out.u32(0);
        for value in &self.values {
            offset += value.len() as u64;
            let stored = u32::try_from(offset).map_err(|_| StrataError::Unsupported {
                what: "a dictionary of more than 4 GiB of values".to_string(),
            })?;
            out.u32(stored);
        }
        for value in &self.values {
            out.bytes(value.as_bytes());
        }
        Ok(())
    }

    /// Decodes a dictionary written by [`Dictionary::encode`], checking the offsets, the
    /// bytewise order, the uniqueness and the UTF-8 of every value. `offset` is the
    /// dictionary's absolute file offset, used in error messages only.
    pub fn decode(bytes: &[u8], offset: u64) -> Result<Self> {
        let corrupt = |what: &'static str| StrataError::Corrupt { what, offset };
        let mut c = Cursor::new(bytes, offset);
        let n = c.u32("dictionary count")? as usize;
        if n > MAX_DICT_VALUES {
            return Err(corrupt("dictionary count"));
        }
        let mut offsets = Vec::new();
        for _ in 0..=n {
            offsets.push(c.u32("dictionary offsets")? as usize);
        }
        if offsets.first() != Some(&0) {
            return Err(corrupt("dictionary offsets"));
        }
        let area_len = offsets.last().copied().unwrap_or_default();
        let area = c.bytes(area_len, "dictionary bytes")?;
        if c.remaining() != 0 {
            return Err(corrupt("dictionary trailing bytes"));
        }

        let mut values: Vec<String> = Vec::new();
        for pair in offsets.windows(2) {
            let [start, end] = pair else {
                return Err(corrupt("dictionary offsets"));
            };
            if start > end {
                return Err(corrupt("dictionary offsets"));
            }
            let value = area
                .get(*start..*end)
                .ok_or_else(|| corrupt("dictionary offsets"))?;
            let text = std::str::from_utf8(value).map_err(|_| corrupt("dictionary utf8"))?;
            if let Some(prev) = values.last() {
                if prev.as_bytes() == value {
                    return Err(corrupt("dictionary duplicate"));
                }
                if prev.as_bytes() > value {
                    return Err(corrupt("dictionary order"));
                }
            }
            values.push(text.to_string());
        }
        Ok(Self { values })
    }
}

/// The dictionary of the Utf8 values of `array`, or of the elements of a List(Utf8) array,
/// over all its rows. Elements of null lists are left out. `None` for other types, and when
/// the dictionary would exceed the limits in `config`. A Utf8 column with no non-null value
/// gets `None`, so it writes no dictionary; a List(Utf8) column keeps an empty one.
pub fn for_column(array: &dyn Array, config: &Config) -> Option<Dictionary> {
    let max_values = config.dict_max_values;
    let max_bytes = config.dict_max_bytes;
    match array.data_type() {
        DataType::Utf8 => {
            let strings = array.as_string_opt::<i32>()?;
            build(&[strings], max_values, max_bytes).filter(|dict| !dict.values.is_empty())
        }
        DataType::List(field) if field.data_type() == &DataType::Utf8 => {
            let list = array.as_list_opt::<i32>()?;
            let elements = valid_elements(list)?;
            build(&[&elements], max_values, max_bytes)
        }
        _ => None,
    }
}

/// The elements of the valid lists of `list`, in order, as one string array.
fn valid_elements(list: &ListArray) -> Option<StringArray> {
    let child = list.values().as_string_opt::<i32>()?;
    let mut out: Vec<Option<&str>> = Vec::new();
    for (i, pair) in list.value_offsets().windows(2).enumerate() {
        if !list.is_valid(i) {
            continue;
        }
        let [start, end] = pair else {
            return None;
        };
        let start = usize::try_from(*start).ok()?;
        let end = usize::try_from(*end).ok()?;
        for j in start..end {
            out.push(child.is_valid(j).then(|| child.value(j)));
        }
    }
    Some(StringArray::from(out))
}

/// The dictionary of the distinct non-null values of `arrays`. `None` when there are more
/// than `max_values` values, or when the values plus four bytes per offset exceed
/// `max_bytes`.
pub(crate) fn build(
    arrays: &[&StringArray],
    max_values: usize,
    max_bytes: usize,
) -> Option<Dictionary> {
    // The size only grows as values are inserted, so the build stops at the first value that
    // takes it over a limit. The size is checked again at the end, for the empty dictionary.
    let mut distinct: BTreeSet<&str> = BTreeSet::new();
    let mut value_bytes = 0usize;
    for array in arrays {
        for i in 0..array.len() {
            if array.is_valid(i) {
                let value = array.value(i);
                if distinct.insert(value) {
                    if distinct.len() > max_values {
                        return None;
                    }
                    value_bytes = value_bytes.checked_add(value.len())?;
                    if value_bytes.checked_add(4 * (distinct.len() + 1))? > max_bytes {
                        return None;
                    }
                }
            }
        }
    }
    let size = value_bytes.checked_add(4 * (distinct.len() + 1))?;
    if size > max_bytes {
        return None;
    }
    Some(Dictionary {
        values: distinct.into_iter().map(str::to_string).collect(),
    })
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

    /// Bytes in the dictionary layout, without any validation.
    fn raw(n: u32, offsets: &[u32], area: &[u8]) -> Vec<u8> {
        let mut out = Out(Vec::new());
        out.u32(n);
        for o in offsets {
            out.u32(*o);
        }
        out.bytes(area);
        out.0
    }

    fn encoded(values: &[&str]) -> Vec<u8> {
        let array = StringArray::from(values.to_vec());
        let dict = build(&[&array], usize::MAX, usize::MAX).unwrap();
        let mut out = Out(Vec::new());
        dict.encode(&mut out).unwrap();
        out.0
    }

    fn strings(values: &[Option<&str>]) -> StringArray {
        StringArray::from(values.to_vec())
    }

    #[test]
    fn round_trip_keeps_sorted_values_and_codes() {
        let bytes = encoded(&["gpt-4o", "a", "gpt-4o", "", "é"]);
        let dict = Dictionary::decode(&bytes, 0).unwrap();
        assert_eq!(dict.values.len(), 4);
        assert_eq!(dict.value(0), Some(b"".as_slice()));
        assert_eq!(dict.code_of(b"a"), Some(1));
        assert_eq!(dict.code_of("é".as_bytes()), Some(3));
        assert_eq!(dict.code_of(b"missing"), None);
        assert_eq!(dict.value(4), None);
    }

    #[test]
    fn build_ignores_nulls_and_deduplicates() {
        let array = strings(&[Some("b"), None, Some("a"), Some("b")]);
        let dict = build(&[&array], 10, 1000).unwrap();
        assert_eq!(dict.values.len(), 2);
        assert_eq!(dict.code_of(b"a"), Some(0));
    }

    #[test]
    fn build_stops_at_exactly_max_values_plus_one() {
        let values: Vec<String> = (0..11).map(|i| format!("v{i}")).collect();
        let array = StringArray::from(values.clone());
        assert!(build(&[&array], 11, usize::MAX).is_some());
        assert!(build(&[&array], 10, usize::MAX).is_none());
    }

    #[test]
    fn build_limits_bytes_including_four_per_offset() {
        // Two values of 3 bytes: 6 value bytes + 4 * (2 + 1) = 18.
        let array = StringArray::from(vec!["abc", "def"]);
        assert!(build(&[&array], 10, 18).is_some());
        assert!(build(&[&array], 10, 17).is_none());
    }

    #[test]
    fn decode_rejects_unsorted_values() {
        let bytes = raw(2, &[0, 1, 2], b"ba");
        assert!(matches!(
            Dictionary::decode(&bytes, 0),
            Err(StrataError::Corrupt {
                what: "dictionary order",
                ..
            })
        ));
    }

    #[test]
    fn decode_rejects_duplicates() {
        let bytes = raw(2, &[0, 1, 2], b"aa");
        assert!(matches!(
            Dictionary::decode(&bytes, 0),
            Err(StrataError::Corrupt {
                what: "dictionary duplicate",
                ..
            })
        ));
    }

    #[test]
    fn decode_rejects_bad_offsets() {
        // First offset not zero.
        assert!(Dictionary::decode(&raw(1, &[1, 2], b"ab"), 0).is_err());
        // Offsets going down.
        assert!(Dictionary::decode(&raw(2, &[0, 2, 1], b"ab"), 0).is_err());
        // Last offset beyond the bytes area.
        assert!(Dictionary::decode(&raw(1, &[0, 5], b"ab"), 0).is_err());
        // Bytes after the area.
        assert!(Dictionary::decode(&raw(1, &[0, 1], b"ab"), 0).is_err());
    }

    #[test]
    fn decode_rejects_invalid_utf8() {
        let bytes = raw(1, &[0, 1], &[0xFF]);
        assert!(matches!(
            Dictionary::decode(&bytes, 0),
            Err(StrataError::Corrupt {
                what: "dictionary utf8",
                ..
            })
        ));
    }

    #[test]
    fn decode_rejects_a_count_above_the_limit_before_reading_offsets() {
        // Only the count is present: the check comes before any offset is read.
        let bytes = raw(u32::try_from(MAX_DICT_VALUES + 1).unwrap(), &[], b"");
        assert!(matches!(
            Dictionary::decode(&bytes, 0),
            Err(StrataError::Corrupt {
                what: "dictionary count",
                ..
            })
        ));
    }

    proptest! {
        #[test]
        fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = Dictionary::decode(&bytes, 0);
        }
    }
}
