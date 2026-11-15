//! Shredding of `Map<Utf8, Utf8>` columns: the frequent keys become child columns, and
//! everything else stays in a residual map. [`shred`] and [`unshred`] are inverses up to the
//! key order: `unshred` returns each row's entries sorted bytewise by key.
//!
//! These are pure functions. The writer and the reader call them from M3.2b on.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, MapArray, StringArray, StringBuilder, StructArray};
use arrow::buffer::{NullBuffer, OffsetBuffer, ScalarBuffer};
use arrow::datatypes::{DataType, Field, Fields};

use crate::error::{Result, StrataError};
use crate::format::{MAX_PROMOTED_KEY_LEN, MAX_PROMOTED_KEYS};

/// The result of [`shred`].
pub(crate) struct Shredded {
    /// Promoted keys, sorted bytewise.
    pub keys: Vec<Vec<u8>>,
    /// One `StringArray` per promoted key, with the map's length. A null means the key is
    /// absent, or it stays in the residual of that row.
    pub children: Vec<ArrayRef>,
    /// A `MapArray` with exactly the map's `DataType`: the entries that are not moved to a child.
    pub residual: ArrayRef,
    /// Per promoted key: the rows in which the key was present but stayed in the residual (a
    /// null value, or more than one occurrence).
    pub residual_hits: Vec<u32>,
}

/// The error for a shredded map that is malformed or cannot be built.
fn corrupt() -> StrataError {
    StrataError::Corrupt {
        what: "shredded map",
        offset: 0,
    }
}

/// An offset of the map being built, checked against the `i32` offset type.
fn offset_of(total: usize) -> Result<i32> {
    i32::try_from(total).map_err(|_| corrupt())
}

/// Checks that the bytes one string builder receives fit its `i32` offsets. Arrow's builders
/// panic past `i32::MAX`, so this runs before anything is appended.
fn check_offsets(total: u64, err: impl FnOnce() -> StrataError) -> Result<()> {
    if total > i32::MAX as u64 {
        Err(err())
    } else {
        Ok(())
    }
}

/// The key and value columns of a map's entries. Both are `StringArray`s of one length.
fn entry_columns(entries: &StructArray) -> Result<(&StringArray, &StringArray)> {
    let [keys, values] = entries.columns() else {
        return Err(corrupt());
    };
    let keys = keys
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(corrupt)?;
    let values = values
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(corrupt)?;
    if keys.len() != values.len() {
        return Err(corrupt());
    }
    Ok((keys, values))
}

/// The entry range of each row of `map`, or `None` for a null map. The ranges are absolute
/// indexes into the entries, which hold `entries` items.
fn row_ranges(map: &MapArray, entries: usize) -> Result<Vec<Option<Range<usize>>>> {
    let mut ranges = Vec::with_capacity(map.len());
    for (row, pair) in map.value_offsets().windows(2).enumerate() {
        let [lo, hi] = pair else {
            return Err(corrupt());
        };
        let lo = usize::try_from(*lo).map_err(|_| corrupt())?;
        let hi = usize::try_from(*hi).map_err(|_| corrupt())?;
        if lo > hi || hi > entries {
            return Err(corrupt());
        }
        ranges.push(map.is_valid(row).then_some(lo..hi));
    }
    Ok(ranges)
}

/// How often a key occurs in one row, and the value of its last occurrence. A key that occurs
/// once with a non-null value is moved to its child.
#[derive(Clone, Copy)]
struct Occurrence<'a> {
    count: usize,
    value: Option<&'a str>,
}

/// The keys of the entries in `range`, with their occurrences. A null key is corrupt.
fn occurrences<'a>(
    keys: &'a StringArray,
    values: &'a StringArray,
    range: Range<usize>,
) -> Result<BTreeMap<&'a [u8], Occurrence<'a>>> {
    let mut occ: BTreeMap<&'a [u8], Occurrence<'a>> = BTreeMap::new();
    for i in range {
        if keys.is_null(i) {
            return Err(corrupt());
        }
        let key = keys.value(i).as_bytes();
        let value = values.is_valid(i).then(|| values.value(i));
        let entry = occ.entry(key).or_insert(Occurrence {
            count: 0,
            value: None,
        });
        entry.count = entry.count.saturating_add(1);
        entry.value = value;
    }
    Ok(occ)
}

/// The index of `key` among the promoted keys (which are sorted), if it is promoted.
fn promoted_index(promoted: &[Vec<u8>], key: &[u8]) -> Option<usize> {
    promoted.binary_search_by(|p| p.as_slice().cmp(key)).ok()
}

/// Whether the entry with `key` in a row with occurrences `occ` moves to its child: the key is
/// promoted, and it occurs once with a non-null value.
fn is_moved(occ: &BTreeMap<&[u8], Occurrence<'_>>, key: &[u8], promoted: &[Vec<u8>]) -> bool {
    occ.get(&key)
        .is_some_and(|o| o.count == 1 && o.value.is_some())
        && promoted_index(promoted, key).is_some()
}

/// The promoted keys: keys that occur exactly once with a non-null value in at least 5% of the
/// rows (rounded up, at least one), of at most `MAX_PROMOTED_KEY_LEN` bytes. The top `MAX_PROMOTED_KEYS`
/// by count, then by key bytes, sorted bytewise.
fn promoted_keys(
    keys: &StringArray,
    values: &StringArray,
    rows: &[Option<Range<usize>>],
) -> Result<Vec<Vec<u8>>> {
    let mut counts: BTreeMap<&[u8], usize> = BTreeMap::new();
    for range in rows.iter().flatten() {
        for (key, occ) in occurrences(keys, values, range.clone())? {
            if occ.count == 1 && occ.value.is_some() {
                *counts.entry(key).or_insert(0) += 1;
            }
        }
    }
    let threshold = rows.len().saturating_mul(5).div_ceil(100).max(1);
    let mut ranked: Vec<(&[u8], usize)> = counts
        .into_iter()
        .filter(|(key, count)| *count >= threshold && key.len() <= MAX_PROMOTED_KEY_LEN)
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    ranked.truncate(MAX_PROMOTED_KEYS);
    let mut promoted: Vec<Vec<u8>> = ranked.into_iter().map(|(key, _)| key.to_vec()).collect();
    promoted.sort();
    Ok(promoted)
}

/// The fields of a `Map` type: the entries field, whether the keys are sorted, and the fields
/// of the entries struct.
fn map_parts(map_type: &DataType) -> Result<(&Arc<Field>, bool, &Fields)> {
    let DataType::Map(entries_field, sorted) = map_type else {
        return Err(corrupt());
    };
    let DataType::Struct(fields) = entries_field.data_type() else {
        return Err(corrupt());
    };
    Ok((entries_field, *sorted, fields))
}

/// Builds a `MapArray` of `map_type` from its offsets, its entry keys and values, and its
/// validity.
fn build_map(
    map_type: &DataType,
    offsets: Vec<i32>,
    keys: StringArray,
    values: StringArray,
    nulls: Option<NullBuffer>,
) -> Result<MapArray> {
    let (entries_field, sorted, fields) = map_parts(map_type)?;
    let structs =
        StructArray::try_new(fields.clone(), vec![Arc::new(keys), Arc::new(values)], None)
            .map_err(|_| corrupt())?;
    // Invariant: the offsets start at 0 and never decrease (checked prefix sums), and there is
    // at least one of them, so `OffsetBuffer::new` cannot panic.
    let offsets = OffsetBuffer::new(ScalarBuffer::from(offsets));
    MapArray::try_new(Arc::clone(entries_field), offsets, structs, nulls, sorted)
        .map_err(|_| corrupt())
}

/// The bytes the builders of [`shred`] will receive: one total per promoted key (its child), then
/// the residual's key bytes and value bytes. Makes the same moving decisions as the build loop.
fn shred_bytes(
    keys: &StringArray,
    values: &StringArray,
    rows: &[Option<Range<usize>>],
    promoted: &[Vec<u8>],
) -> Result<(Vec<u64>, u64, u64)> {
    let mut children = vec![0u64; promoted.len()];
    let mut residual_keys = 0u64;
    let mut residual_values = 0u64;
    for range in rows.iter().flatten() {
        let occ = occurrences(keys, values, range.clone())?;
        for i in range.clone() {
            let key = keys.value(i).as_bytes();
            let value_len = if values.is_valid(i) {
                values.value(i).len() as u64
            } else {
                0
            };
            match promoted_index(promoted, key) {
                Some(p) if is_moved(&occ, key, promoted) => {
                    if let Some(total) = children.get_mut(p) {
                        *total = total.saturating_add(value_len);
                    }
                }
                _ => {
                    residual_keys = residual_keys.saturating_add(key.len() as u64);
                    residual_values = residual_values.saturating_add(value_len);
                }
            }
        }
    }
    Ok((children, residual_keys, residual_values))
}

/// Splits `map` into promoted-key children and a residual map.
///
/// The residual has the map's `DataType`. A null map gives a null child and residual row; a
/// non-null row keeps every entry that is not moved, in its original order.
pub(crate) fn shred(map: &MapArray) -> Result<Shredded> {
    let (keys, values) = entry_columns(map.entries())?;
    let rows = row_ranges(map, keys.len())?;
    let promoted = promoted_keys(keys, values, &rows)?;

    let too_big = || StrataError::Unsupported {
        what: "map column larger than 2 GiB".to_string(),
    };
    let (child_bytes, residual_key_bytes, residual_value_bytes) =
        shred_bytes(keys, values, &rows, &promoted)?;
    for total in child_bytes {
        check_offsets(total, too_big)?;
    }
    check_offsets(residual_key_bytes, too_big)?;
    check_offsets(residual_value_bytes, too_big)?;

    let mut children: Vec<StringBuilder> = promoted.iter().map(|_| StringBuilder::new()).collect();
    let mut residual_hits = vec![0u32; promoted.len()];
    let mut res_keys = StringBuilder::new();
    let mut res_values = StringBuilder::new();
    let mut res_offsets: Vec<i32> = Vec::with_capacity(rows.len() + 1);
    res_offsets.push(0);
    let mut res_valid = Vec::with_capacity(rows.len());
    let mut res_total: usize = 0;

    for range in &rows {
        let Some(range) = range else {
            for child in &mut children {
                child.append_null();
            }
            res_valid.push(false);
            res_offsets.push(offset_of(res_total)?);
            continue;
        };
        let occ = occurrences(keys, values, range.clone())?;

        let mut child_values: Vec<Option<&str>> = vec![None; promoted.len()];
        for (key, o) in &occ {
            let Some(p) = promoted_index(&promoted, key) else {
                continue;
            };
            match o.value {
                Some(v) if o.count == 1 => {
                    if let Some(slot) = child_values.get_mut(p) {
                        *slot = Some(v);
                    }
                }
                _ => {
                    if let Some(hits) = residual_hits.get_mut(p) {
                        *hits = hits.saturating_add(1);
                    }
                }
            }
        }
        for (child, value) in children.iter_mut().zip(child_values.iter().copied()) {
            match value {
                Some(v) => child.append_value(v),
                None => child.append_null(),
            }
        }

        let mut count = 0usize;
        for i in range.clone() {
            let key = keys.value(i);
            if is_moved(&occ, key.as_bytes(), &promoted) {
                continue;
            }
            res_keys.append_value(key);
            if values.is_valid(i) {
                res_values.append_value(values.value(i));
            } else {
                res_values.append_null();
            }
            count += 1;
        }
        res_total = res_total.checked_add(count).ok_or_else(corrupt)?;
        res_offsets.push(offset_of(res_total)?);
        res_valid.push(true);
    }

    let residual_nulls = res_valid
        .contains(&false)
        .then(|| NullBuffer::from(res_valid));
    let residual = build_map(
        map.data_type(),
        res_offsets,
        res_keys.finish(),
        res_values.finish(),
        residual_nulls,
    )?;
    let children = children
        .into_iter()
        .map(|mut child| Arc::new(child.finish()) as ArrayRef)
        .collect();
    Ok(Shredded {
        keys: promoted,
        children,
        residual: Arc::new(residual),
        residual_hits,
    })
}

/// Rebuilds the map that [`shred`] split: each row's residual entries, then every promoted key
/// whose child is non-null, stable-sorted by key bytes.
///
/// Corrupt when the lengths differ, when `children` and `keys` differ in number, when a child is
/// not a `StringArray`, when a key is not UTF-8, when `residual` does not have `map_type`, or
/// when a child holds a value in a row whose residual is null.
pub(crate) fn unshred(
    map_type: &DataType,
    keys: &[Vec<u8>],
    children: &[ArrayRef],
    residual: &ArrayRef,
) -> Result<ArrayRef> {
    map_parts(map_type)?;
    if residual.data_type() != map_type {
        return Err(corrupt());
    }
    let residual = residual
        .as_any()
        .downcast_ref::<MapArray>()
        .ok_or_else(corrupt)?;
    if children.len() != keys.len() {
        return Err(corrupt());
    }
    let n = residual.len();
    let mut names: Vec<&str> = Vec::with_capacity(keys.len());
    let mut columns: Vec<&StringArray> = Vec::with_capacity(keys.len());
    for (key, child) in keys.iter().zip(children) {
        names.push(std::str::from_utf8(key).map_err(|_| corrupt())?);
        let column = child
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or_else(corrupt)?;
        if column.len() != n {
            return Err(corrupt());
        }
        columns.push(column);
    }
    let (res_keys, res_values) = entry_columns(residual.entries())?;
    let ranges = row_ranges(residual, res_keys.len())?;

    // The key builder receives the residual keys and the promoted names; the value builder the
    // residual values and the child values. Checked before anything is appended.
    let mut key_bytes = 0u64;
    let mut value_bytes = 0u64;
    for (r, range) in ranges.iter().enumerate() {
        let Some(range) = range else {
            continue;
        };
        for i in range.clone() {
            key_bytes = key_bytes.saturating_add(res_keys.value(i).len() as u64);
            if res_values.is_valid(i) {
                value_bytes = value_bytes.saturating_add(res_values.value(i).len() as u64);
            }
        }
        for (name, column) in names.iter().zip(&columns) {
            if column.is_valid(r) {
                key_bytes = key_bytes.saturating_add(name.len() as u64);
                value_bytes = value_bytes.saturating_add(column.value(r).len() as u64);
            }
        }
    }
    check_offsets(key_bytes, corrupt)?;
    check_offsets(value_bytes, corrupt)?;

    let mut out_keys = StringBuilder::new();
    let mut out_values = StringBuilder::new();
    let mut offsets: Vec<i32> = Vec::with_capacity(n + 1);
    offsets.push(0);
    let mut valid = Vec::with_capacity(n);
    let mut total: usize = 0;
    for (r, range) in ranges.iter().enumerate() {
        let present: Vec<Option<&str>> = columns
            .iter()
            .copied()
            .map(|c| c.is_valid(r).then(|| c.value(r)))
            .collect();
        let Some(range) = range else {
            if present.iter().any(|v| v.is_some()) {
                return Err(corrupt());
            }
            valid.push(false);
            offsets.push(offset_of(total)?);
            continue;
        };

        let mut row: Vec<(&str, Option<&str>)> = Vec::new();
        for i in range.clone() {
            if res_keys.is_null(i) {
                return Err(corrupt());
            }
            let value = res_values.is_valid(i).then(|| res_values.value(i));
            row.push((res_keys.value(i), value));
        }
        for (name, value) in names.iter().copied().zip(present) {
            if let Some(v) = value {
                row.push((name, Some(v)));
            }
        }
        row.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));

        total = total.checked_add(row.len()).ok_or_else(corrupt)?;
        for (key, value) in row {
            out_keys.append_value(key);
            match value {
                Some(v) => out_values.append_value(v),
                None => out_values.append_null(),
            }
        }
        offsets.push(offset_of(total)?);
        valid.push(true);
    }

    let nulls = valid.contains(&false).then(|| NullBuffer::from(valid));
    let map = build_map(
        map_type,
        offsets,
        out_keys.finish(),
        out_values.finish(),
        nulls,
    )?;
    Ok(Arc::new(map))
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
    use arrow::array::{Int32Array, MapBuilder};
    use proptest::prelude::*;

    #[test]
    fn offsets_fit_i32_up_to_i32_max_and_not_past_it() {
        let limit = i32::MAX as u64;
        assert!(check_offsets(limit, corrupt).is_ok());
        assert!(matches!(
            check_offsets(limit + 1, corrupt),
            Err(StrataError::Corrupt { .. })
        ));
    }

    /// One map row: `None` is a null map, otherwise its entries in order.
    type Row = Option<Vec<(String, Option<String>)>>;

    /// A `Map<Utf8, Utf8>` array with the layout of the spans schema.
    fn map_of(rows: &[Row]) -> MapArray {
        let mut builder = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new());
        for row in rows {
            match row {
                Some(entries) => {
                    for (key, value) in entries {
                        builder.keys().append_value(key);
                        builder.values().append_option(value.as_deref());
                    }
                    builder.append(true).unwrap();
                }
                None => builder.append(false).unwrap(),
            }
        }
        builder.finish()
    }

    /// The rows of a map array, as the entries of each row.
    fn rows_of(array: &dyn Array) -> Vec<Row> {
        let map = array.as_any().downcast_ref::<MapArray>().unwrap();
        let (keys, values) = entry_columns(map.entries()).unwrap();
        row_ranges(map, keys.len())
            .unwrap()
            .into_iter()
            .map(|range| {
                range.map(|range| {
                    range
                        .map(|i| {
                            let value = values.is_valid(i).then(|| values.value(i).to_string());
                            (keys.value(i).to_string(), value)
                        })
                        .collect()
                })
            })
            .collect()
    }

    /// The rows with each row's entries stable-sorted by key bytes.
    fn sorted(rows: &[Row]) -> Vec<Row> {
        rows.iter()
            .map(|row| {
                row.as_ref().map(|entries| {
                    let mut entries = entries.clone();
                    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
                    entries
                })
            })
            .collect()
    }

    /// A row of `(key, value)` pairs.
    fn row(entries: &[(&str, Option<&str>)]) -> Row {
        Some(
            entries
                .iter()
                .map(|(k, v)| (k.to_string(), v.map(str::to_string)))
                .collect(),
        )
    }

    /// The keys of the test vocabulary: multi-byte and NUL-containing keys included.
    const VOCAB: [&str; 15] = [
        "a",
        "b",
        "c",
        "d",
        "e",
        "f",
        "g",
        "h",
        "i",
        "j",
        "k",
        "é",
        "日本",
        "x\u{0}y",
        "key-with-a-longer-name",
    ];

    fn rows_strategy() -> impl Strategy<Value = Vec<Row>> {
        let entry = (
            proptest::sample::select(VOCAB.to_vec()),
            proptest::option::of(".{0,6}"),
        )
            .prop_map(|(key, value)| (key.to_string(), value));
        proptest::collection::vec(
            proptest::option::of(proptest::collection::vec(entry, 0..12)),
            0..300,
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn unshred_of_shred_is_the_sorted_map(rows in rows_strategy()) {
            let map = map_of(&rows);
            let shredded = shred(&map).unwrap();
            let back = unshred(
                map.data_type(),
                &shredded.keys,
                &shredded.children,
                &shredded.residual,
            )
            .unwrap();
            prop_assert_eq!(rows_of(back.as_ref()), sorted(&rows));
        }
    }

    #[test]
    fn a_key_in_five_of_100_rows_is_promoted_and_one_in_four_is_not() {
        let mut rows = Vec::new();
        for i in 0..100 {
            let mut entries = vec![("pad", Some("x"))];
            if i < 5 {
                entries.push(("hit", Some("h")));
            }
            if (5..9).contains(&i) {
                entries.push(("miss", Some("m")));
            }
            rows.push(row(&entries));
        }
        let shredded = shred(&map_of(&rows)).unwrap();
        assert_eq!(shredded.keys, vec![b"hit".to_vec(), b"pad".to_vec()]);
    }

    #[test]
    fn with_70_equal_keys_the_bytewise_smallest_64_are_promoted() {
        let names: Vec<String> = (0..70).map(|i| format!("k{i:02}")).collect();
        let rows: Vec<Row> = (0..10)
            .map(|_| {
                Some(
                    names
                        .iter()
                        .map(|n| (n.clone(), Some("v".to_string())))
                        .collect(),
                )
            })
            .collect();
        let shredded = shred(&map_of(&rows)).unwrap();
        let expected: Vec<Vec<u8>> = (0..64).map(|i| format!("k{i:02}").into_bytes()).collect();
        assert_eq!(shredded.keys, expected);
        assert_eq!(shredded.children.len(), 64);
    }

    #[test]
    fn a_key_of_1025_bytes_is_not_promoted() {
        let long = "x".repeat(1025);
        let rows: Vec<Row> = (0..10)
            .map(|_| row(&[(long.as_str(), Some("v"))]))
            .collect();
        let shredded = shred(&map_of(&rows)).unwrap();
        assert!(shredded.keys.is_empty());
        assert_eq!(rows_of(shredded.residual.as_ref()), rows);
    }

    #[test]
    fn a_null_promoted_value_stays_in_the_residual() {
        let mut rows: Vec<Row> = (0..10).map(|_| row(&[("k", Some("v"))])).collect();
        rows[0] = row(&[("k", None)]);
        let shredded = shred(&map_of(&rows)).unwrap();
        assert_eq!(shredded.keys, vec![b"k".to_vec()]);
        assert_eq!(shredded.residual_hits, vec![1]);
        let child = shredded.children[0]
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert!(child.is_null(0));
        assert_eq!(child.value(1), "v");
        assert_eq!(rows_of(shredded.residual.as_ref())[0], row(&[("k", None)]));
        assert_eq!(rows_of(shredded.residual.as_ref())[1], Some(Vec::new()));
    }

    #[test]
    fn a_key_twice_in_a_row_stays_in_the_residual_and_counts_once() {
        let mut rows: Vec<Row> = (0..10).map(|_| row(&[("k", Some("v"))])).collect();
        rows[0] = row(&[("k", Some("a")), ("k", Some("b"))]);
        let shredded = shred(&map_of(&rows)).unwrap();
        assert_eq!(shredded.residual_hits, vec![1]);
        let child = shredded.children[0]
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert!(child.is_null(0));
        let residual = rows_of(shredded.residual.as_ref());
        assert_eq!(residual[0], row(&[("k", Some("a")), ("k", Some("b"))]));
        assert_eq!(residual[1], Some(Vec::new()));
    }

    #[test]
    fn identical_three_key_rows_leave_empty_residual_rows() {
        let rows: Vec<Row> = (0..20)
            .map(|_| row(&[("a", Some("1")), ("b", Some("2")), ("c", Some("3"))]))
            .collect();
        let map = map_of(&rows);
        let shredded = shred(&map).unwrap();
        assert_eq!(shredded.keys.len(), 3);
        let empty: Vec<Row> = vec![Some(Vec::new()); 20];
        assert_eq!(rows_of(shredded.residual.as_ref()), empty);
        let back = unshred(
            map.data_type(),
            &shredded.keys,
            &shredded.children,
            &shredded.residual,
        )
        .unwrap();
        assert_eq!(rows_of(back.as_ref()), sorted(&rows));
    }

    #[test]
    fn without_a_frequent_key_nothing_is_promoted() {
        let rows: Vec<Row> = (0..100)
            .map(|i| {
                let key = format!("u{i}");
                row(&[(key.as_str(), Some("v"))])
            })
            .collect();
        let map = map_of(&rows);
        let shredded = shred(&map).unwrap();
        assert!(shredded.keys.is_empty());
        assert!(shredded.residual_hits.is_empty());
        let back = unshred(
            map.data_type(),
            &shredded.keys,
            &shredded.children,
            &shredded.residual,
        )
        .unwrap();
        assert_eq!(rows_of(back.as_ref()), rows);
    }

    #[test]
    fn a_zero_row_map_round_trips() {
        let map = map_of(&[]);
        let shredded = shred(&map).unwrap();
        assert!(shredded.keys.is_empty());
        assert_eq!(shredded.residual.len(), 0);
        let back = unshred(
            map.data_type(),
            &shredded.keys,
            &shredded.children,
            &shredded.residual,
        )
        .unwrap();
        assert_eq!(back.len(), 0);
    }

    /// A residual of `rows` empty maps, with its map type.
    fn empty_maps(rows: usize) -> (DataType, ArrayRef) {
        let empty: Vec<Row> = vec![Some(Vec::new()); rows];
        let map = map_of(&empty);
        (map.data_type().clone(), Arc::new(map))
    }

    #[test]
    fn unshred_rejects_a_child_of_another_length() {
        let (dt, residual) = empty_maps(3);
        let child: ArrayRef = Arc::new(StringArray::from(vec!["v", "w"]));
        let result = unshred(&dt, &[b"k".to_vec()], &[child], &residual);
        assert!(matches!(
            result,
            Err(StrataError::Corrupt {
                what: "shredded map",
                ..
            })
        ));
    }

    #[test]
    fn unshred_rejects_a_child_count_that_differs_from_the_keys() {
        let (dt, residual) = empty_maps(3);
        let result = unshred(&dt, &[b"k".to_vec()], &[], &residual);
        assert!(matches!(
            result,
            Err(StrataError::Corrupt {
                what: "shredded map",
                ..
            })
        ));
    }

    #[test]
    fn unshred_rejects_a_child_that_is_not_a_string_array() {
        let (dt, residual) = empty_maps(3);
        let child: ArrayRef = Arc::new(Int32Array::from(vec![1, 2, 3]));
        let result = unshred(&dt, &[b"k".to_vec()], &[child], &residual);
        assert!(matches!(
            result,
            Err(StrataError::Corrupt {
                what: "shredded map",
                ..
            })
        ));
    }

    #[test]
    fn unshred_rejects_a_child_value_in_a_null_residual_row() {
        let null_row: Vec<Row> = vec![None];
        let map = map_of(&null_row);
        let residual: ArrayRef = Arc::new(map.clone());
        let child: ArrayRef = Arc::new(StringArray::from(vec![Some("v")]));
        let result = unshred(map.data_type(), &[b"k".to_vec()], &[child], &residual);
        assert!(matches!(
            result,
            Err(StrataError::Corrupt {
                what: "shredded map",
                ..
            })
        ));
    }

    #[test]
    fn unshred_rejects_a_key_that_is_not_utf8() {
        let (dt, residual) = empty_maps(1);
        let child: ArrayRef = Arc::new(StringArray::from(vec![Some("v")]));
        let result = unshred(&dt, &[vec![0xff]], &[child], &residual);
        assert!(matches!(
            result,
            Err(StrataError::Corrupt {
                what: "shredded map",
                ..
            })
        ));
    }

    #[test]
    fn unshred_rejects_a_residual_of_another_type() {
        let (_, residual) = empty_maps(3);
        let other: ArrayRef = Arc::new(Int32Array::from(vec![1, 2, 3]));
        let result = unshred(residual.data_type(), &[], &[], &other);
        assert!(matches!(
            result,
            Err(StrataError::Corrupt {
                what: "shredded map",
                ..
            })
        ));
    }
}
