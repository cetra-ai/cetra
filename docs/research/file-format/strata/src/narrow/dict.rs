//! Sorted file-level dictionaries for Utf8 columns, the elements of List(Utf8) columns, and
//! Float64 columns whose values repeat.

use std::collections::{HashMap, HashSet};

use std::sync::{Arc, OnceLock};
use xxhash_rust::xxh3::Xxh3DefaultBuilder;

use arrow::array::{Array, ArrayRef, AsArray, Float64Array, ListArray, StringArray};
use arrow::datatypes::{DataType, Float64Type};

use crate::config::Config;
use crate::error::{Result, StrataError};
use crate::format::{Cursor, MAX_DICT_VALUES, Out};

/// What a dictionary's values are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictKind {
    /// Utf8 strings, for Utf8 and List(Utf8) columns.
    Utf8,
    /// Float64 values as their eight little-endian bytes, for Float64 columns.
    Float64,
}

impl DictKind {
    /// The kind of dictionary a column of type `dt` may have, `None` for a type that has none.
    pub fn of(dt: &DataType) -> Option<Self> {
        match dt {
            DataType::Utf8 => Some(Self::Utf8),
            DataType::List(field) if field.data_type() == &DataType::Utf8 => Some(Self::Utf8),
            DataType::Float64 => Some(Self::Float64),
            _ => None,
        }
    }
}

/// The values of a dictionary.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Values {
    /// Valid UTF-8, checked once when the dictionary is built or decoded.
    Utf8(Vec<String>),
    /// The little-endian bytes of each value, so NaN payloads and -0.0 keep their bits.
    Float64(Vec<[u8; 8]>),
}

/// The distinct values of a column, sorted bytewise. A code is the index of a value.
#[derive(Debug, Clone)]
pub struct Dictionary {
    values: Values,
    /// The values as one Arrow array, built on first use. Decoded chunks share it.
    array: OnceLock<ArrayRef>,
    /// The code of each value, built on first use by `code_of`.
    codes: OnceLock<HashMap<Box<[u8]>, u32, Xxh3DefaultBuilder>>,
}

impl PartialEq for Dictionary {
    fn eq(&self, other: &Self) -> bool {
        self.values == other.values
    }
}

impl Eq for Dictionary {}

impl Dictionary {
    /// A dictionary of `values`, which must be sorted bytewise and distinct.
    fn from_values(values: Values) -> Self {
        Self {
            values,
            array: OnceLock::new(),
            codes: OnceLock::new(),
        }
    }

    /// What the values are.
    pub fn kind(&self) -> DictKind {
        match self.values {
            Values::Utf8(_) => DictKind::Utf8,
            Values::Float64(_) => DictKind::Float64,
        }
    }

    /// The values as one Utf8 or Float64 array, indexed by code. It is built once, and every
    /// chunk decoded as a dictionary array shares it.
    pub(crate) fn values_array(&self) -> ArrayRef {
        Arc::clone(self.array.get_or_init(|| match &self.values {
            Values::Utf8(values) => {
                Arc::new(StringArray::from_iter_values(values.iter())) as ArrayRef
            }
            Values::Float64(values) => Arc::new(Float64Array::from_iter_values(
                values.iter().map(|v| f64::from_le_bytes(*v)),
            )) as ArrayRef,
        }))
    }

    /// The value bytes of each code, in code order.
    fn value_bytes(&self) -> Box<dyn Iterator<Item = &[u8]> + '_> {
        match &self.values {
            Values::Utf8(values) => Box::new(values.iter().map(String::as_bytes)),
            Values::Float64(values) => Box::new(values.iter().map(<[u8; 8]>::as_slice)),
        }
    }

    /// The code of `v`, or `None` when `v` is not in the dictionary. The first call builds a
    /// hash map of the codes, so encoding a column costs one lookup per value.
    pub fn code_of(&self, v: &[u8]) -> Option<u32> {
        self.codes
            .get_or_init(|| {
                self.value_bytes()
                    .enumerate()
                    .filter_map(|(i, value)| Some((Box::from(value), u32::try_from(i).ok()?)))
                    .collect()
            })
            .get(v)
            .copied()
    }

    /// The value bytes of `code`, or `None` when the code is out of range.
    pub fn value(&self, code: u32) -> Option<&[u8]> {
        match &self.values {
            Values::Utf8(values) => values.get(code as usize).map(String::as_bytes),
            Values::Float64(values) => values.get(code as usize).map(<[u8; 8]>::as_slice),
        }
    }

    /// The value of `code` as a string, or `None` when the code is out of range or the
    /// dictionary is not Utf8.
    pub fn str_value(&self, code: u32) -> Option<&str> {
        match &self.values {
            Values::Utf8(values) => values.get(code as usize).map(String::as_str),
            Values::Float64(_) => None,
        }
    }

    /// The Float64 values, by code, as their little-endian bytes. `None` for a Utf8 dictionary.
    pub(crate) fn floats(&self) -> Option<&[[u8; 8]]> {
        match &self.values {
            Values::Utf8(_) => None,
            Values::Float64(values) => Some(values),
        }
    }

    /// The number of values, so the codes are `0..len()`.
    pub(crate) fn len(&self) -> usize {
        match &self.values {
            Values::Utf8(values) => values.len(),
            Values::Float64(values) => values.len(),
        }
    }

    /// Writes the S3 layout: `u32 n`, `u32 offsets[n+1]` relative to the bytes area, then
    /// the bytes.
    pub fn encode(&self, out: &mut Out) -> Result<()> {
        let n = u32::try_from(self.len()).map_err(|_| StrataError::Unsupported {
            what: "a dictionary of more than 4 billion values".to_string(),
        })?;
        out.u32(n);
        let mut offset = 0u64;
        out.u32(0);
        for value in self.value_bytes() {
            offset += value.len() as u64;
            let stored = u32::try_from(offset).map_err(|_| StrataError::Unsupported {
                what: "a dictionary of more than 4 GiB of values".to_string(),
            })?;
            out.u32(stored);
        }
        for value in self.value_bytes() {
            out.bytes(value);
        }
        Ok(())
    }

    /// Decodes a dictionary of `kind` written by [`Dictionary::encode`], checking the offsets,
    /// the bytewise order, the uniqueness, and the UTF-8 or the eight-byte length of every
    /// value. `offset` is the dictionary's absolute file offset, used in error messages only.
    pub fn decode(bytes: &[u8], offset: u64, kind: DictKind) -> Result<Self> {
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

        let mut previous: Option<&[u8]> = None;
        let mut strings: Vec<String> = Vec::new();
        let mut floats: Vec<[u8; 8]> = Vec::new();
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
            if let Some(prev) = previous {
                if prev == value {
                    return Err(corrupt("dictionary duplicate"));
                }
                if prev > value {
                    return Err(corrupt("dictionary order"));
                }
            }
            previous = Some(value);
            match kind {
                DictKind::Utf8 => {
                    let text =
                        std::str::from_utf8(value).map_err(|_| corrupt("dictionary utf8"))?;
                    strings.push(text.to_string());
                }
                DictKind::Float64 => {
                    let bits: [u8; 8] = value
                        .try_into()
                        .map_err(|_| corrupt("dictionary float width"))?;
                    floats.push(bits);
                }
            }
        }
        Ok(Self::from_values(match kind {
            DictKind::Utf8 => Values::Utf8(strings),
            DictKind::Float64 => Values::Float64(floats),
        }))
    }
}

/// The dictionary of the Utf8 values of `array`, of the elements of a List(Utf8) array, or of
/// the values of a Float64 array, over all its rows. Elements of null lists are left out.
/// `None` for other types, and when the dictionary would exceed the limits in `config`. A Utf8
/// column with no non-null value gets `None`, so it writes no dictionary; a List(Utf8) column
/// keeps an empty one. A Float64 column gets one only when its values repeat
/// (`float_dictionary`).
pub fn for_column(array: &dyn Array, config: &Config) -> Option<Dictionary> {
    let max_values = config.dict_max_values;
    let max_bytes = config.dict_max_bytes;
    match array.data_type() {
        DataType::Utf8 => {
            let strings = array.as_string_opt::<i32>()?;
            build(&[strings], max_values, max_bytes).filter(|dict| dict.len() != 0)
        }
        DataType::List(field) if field.data_type() == &DataType::Utf8 => {
            let list = array.as_list_opt::<i32>()?;
            let elements = valid_elements(list)?;
            build(&[&elements], max_values, max_bytes)
        }
        DataType::Float64 => float_dictionary(array.as_primitive_opt::<Float64Type>()?, config),
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
    let mut distinct: HashSet<&str, Xxh3DefaultBuilder> = HashSet::default();
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
    let mut values: Vec<String> = distinct.into_iter().map(str::to_string).collect();
    values.sort_unstable();
    Some(Dictionary::from_values(Values::Utf8(values)))
}

/// A Float64 value is stored as a code only when the column's valid values average at least
/// this many rows per distinct value. Codes then take at most about a quarter of the eight
/// bytes a plain value takes before zstd, enough to beat what zstd does with plain values.
pub(crate) const FLOAT_DICT_MIN_REPEAT: usize = 8;

/// The dictionary of the distinct valid values of a Float64 column, compared by their bits.
/// `None` when the values repeat less than `FLOAT_DICT_MIN_REPEAT` times on average, when
/// there are more than `dict_max_values` of them, or when they and their offsets exceed
/// `dict_max_bytes`.
fn float_dictionary(array: &Float64Array, config: &Config) -> Option<Dictionary> {
    let valid = array.len() - array.null_count();
    // Stopping at this many distinct values keeps the build cheap for unique values.
    let limit = (valid / FLOAT_DICT_MIN_REPEAT).min(config.dict_max_values);
    let mut distinct: HashSet<u64, Xxh3DefaultBuilder> = HashSet::default();
    for value in array.iter().flatten() {
        if distinct.insert(value.to_bits()) && distinct.len() > limit {
            return None;
        }
    }
    let size = distinct.len().checked_mul(12)?.checked_add(4)?;
    if distinct.is_empty() || size > config.dict_max_bytes {
        return None;
    }
    let mut values: Vec<[u8; 8]> = distinct.into_iter().map(u64::to_le_bytes).collect();
    values.sort_unstable();
    Some(Dictionary::from_values(Values::Float64(values)))
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

    fn floats(values: &[Option<f64>]) -> Float64Array {
        Float64Array::from(values.to_vec())
    }

    #[test]
    fn a_float_column_gets_a_dictionary_only_when_its_values_repeat_eight_times() {
        let config = Config::default();
        // 80 valid values: 10 distinct ones repeat 8 times, 11 do not.
        let ten = floats(&(0..80).map(|i| Some(f64::from(i % 10))).collect::<Vec<_>>());
        let dict = for_column(&ten, &config).unwrap();
        assert_eq!(dict.kind(), DictKind::Float64);
        assert_eq!(dict.len(), 10);
        let eleven = floats(&(0..80).map(|i| Some(f64::from(i % 11))).collect::<Vec<_>>());
        assert!(for_column(&eleven, &config).is_none());
        // Nulls do not count as values, and an all-null column has none.
        let mut sparse: Vec<Option<f64>> = (0..80).map(|i| Some(f64::from(i % 10))).collect();
        sparse.extend(vec![None; 80]);
        assert!(for_column(&floats(&sparse), &config).is_some());
        assert!(for_column(&floats(&[None; 16]), &config).is_none());
        // More values than `dict_max_values` gives none.
        let few = Config {
            dict_max_values: 9,
            ..Config::default()
        };
        assert!(for_column(&ten, &few).is_none());
    }

    #[test]
    fn a_float_dictionary_keeps_bits_and_round_trips() {
        // -0.0, 0.0 and NaN are three values, told apart by their bits.
        let values: Vec<Option<f64>> = (0..64)
            .map(|i| Some([-0.0, 0.0, f64::NAN, 1.5][i % 4]))
            .collect();
        let dict = for_column(&floats(&values), &Config::default()).unwrap();
        assert_eq!(dict.len(), 4);
        assert!(dict.code_of(&(-0.0f64).to_le_bytes()).is_some());
        assert_ne!(
            dict.code_of(&(-0.0f64).to_le_bytes()),
            dict.code_of(&0.0f64.to_le_bytes())
        );
        assert_eq!(dict.str_value(0), None);
        let mut out = Out(Vec::new());
        dict.encode(&mut out).unwrap();
        let decoded = Dictionary::decode(&out.0, 0, DictKind::Float64).unwrap();
        assert_eq!(decoded, dict);
        assert_eq!(decoded.floats().unwrap().len(), 4);
    }

    #[test]
    fn a_float_dictionary_rejects_a_value_that_is_not_eight_bytes() {
        // A 3-byte value, then an 8-byte one, in bytewise order.
        let bytes = raw(2, &[0, 3, 11], &[0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1]);
        assert!(matches!(
            Dictionary::decode(&bytes, 0, DictKind::Float64),
            Err(StrataError::Corrupt {
                what: "dictionary float width",
                ..
            })
        ));
    }

    #[test]
    fn dictionary_kinds_by_type() {
        let list = DataType::List(Arc::new(arrow::datatypes::Field::new(
            "item",
            DataType::Utf8,
            true,
        )));
        assert_eq!(DictKind::of(&DataType::Utf8), Some(DictKind::Utf8));
        assert_eq!(DictKind::of(&list), Some(DictKind::Utf8));
        assert_eq!(DictKind::of(&DataType::Float64), Some(DictKind::Float64));
        assert_eq!(DictKind::of(&DataType::Float32), None);
        assert_eq!(DictKind::of(&DataType::Int64), None);
    }

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
        let dict = Dictionary::decode(&bytes, 0, DictKind::Utf8).unwrap();
        assert_eq!(dict.len(), 4);
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
        assert_eq!(dict.len(), 2);
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
            Dictionary::decode(&bytes, 0, DictKind::Utf8),
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
            Dictionary::decode(&bytes, 0, DictKind::Utf8),
            Err(StrataError::Corrupt {
                what: "dictionary duplicate",
                ..
            })
        ));
    }

    #[test]
    fn decode_rejects_bad_offsets() {
        // First offset not zero.
        assert!(Dictionary::decode(&raw(1, &[1, 2], b"ab"), 0, DictKind::Utf8).is_err());
        // Offsets going down.
        assert!(Dictionary::decode(&raw(2, &[0, 2, 1], b"ab"), 0, DictKind::Utf8).is_err());
        // Last offset beyond the bytes area.
        assert!(Dictionary::decode(&raw(1, &[0, 5], b"ab"), 0, DictKind::Utf8).is_err());
        // Bytes after the area.
        assert!(Dictionary::decode(&raw(1, &[0, 1], b"ab"), 0, DictKind::Utf8).is_err());
    }

    #[test]
    fn decode_rejects_invalid_utf8() {
        let bytes = raw(1, &[0, 1], &[0xFF]);
        assert!(matches!(
            Dictionary::decode(&bytes, 0, DictKind::Utf8),
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
            Dictionary::decode(&bytes, 0, DictKind::Utf8),
            Err(StrataError::Corrupt {
                what: "dictionary count",
                ..
            })
        ));
    }

    proptest! {
        #[test]
        fn random_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = Dictionary::decode(&bytes, 0, DictKind::Utf8);
        }
    }
}
