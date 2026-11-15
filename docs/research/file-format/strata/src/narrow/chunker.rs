//! Chunk boundaries: a chunk closes at `max_rows` rows or once its value bytes reach
//! `max_bytes`. `IPC` chunks measure their memory in blocks of 256 rows instead.

use std::ops::Range;

use arrow::array::{Array, AsArray, ListArray, MapArray};
use arrow::buffer::NullBuffer;
use arrow::datatypes::DataType;

use crate::error::Result;

/// Rows measured at a time for `IPC` chunks.
const IPC_BLOCK_ROWS: usize = 256;

/// The row ranges of `array` that become chunks. They are non-empty, in order, and cover
/// `0..array.len()`. An empty array has no chunks. `has_dict` tells whether the column has a
/// dictionary, which decides whether a List(Utf8) column is stored as `IPC`.
pub fn chunk_ranges(
    array: &dyn Array,
    max_rows: u32,
    max_bytes: usize,
    has_dict: bool,
) -> Result<Vec<Range<usize>>> {
    let max_rows = usize::try_from(max_rows).unwrap_or(usize::MAX);
    if uses_ipc(array.data_type(), has_dict) {
        return ipc_ranges(array, max_rows, max_bytes);
    }
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut rows = 0usize;
    let mut bytes = 0usize;
    for (i, row_bytes) in row_sizes(array, has_dict).into_iter().enumerate() {
        rows += 1;
        bytes = bytes.saturating_add(row_bytes);
        if rows >= max_rows || bytes >= max_bytes {
            ranges.push(start..i + 1);
            start = i + 1;
            rows = 0;
            bytes = 0;
        }
    }
    if start < array.len() {
        ranges.push(start..array.len());
    }
    Ok(ranges)
}

/// Whether a column of type `dt` is stored as `IPC` chunks. Every type without a narrow
/// encoding of its own is, a List(Utf8) column is only when it has no dictionary, and a map is
/// only when it is not `Map<Utf8, Utf8>`.
pub(crate) fn uses_ipc(dt: &DataType, has_dict: bool) -> bool {
    match dt {
        DataType::List(field) if field.data_type() == &DataType::Utf8 => !has_dict,
        DataType::Utf8 | DataType::Binary | DataType::Boolean | DataType::FixedSizeBinary(_) => {
            false
        }
        DataType::Map(..) => !super::is_utf8_map(dt),
        _ => super::numeric_of(dt).is_none(),
    }
}

/// The ranges of an `IPC` column. Each step measures a block of at most 256 rows with
/// `get_slice_memory_size`, so a block never crosses the row limit of its chunk, and a chunk
/// closes after the block that reaches the row or byte limit.
fn ipc_ranges(array: &dyn Array, max_rows: usize, max_bytes: usize) -> Result<Vec<Range<usize>>> {
    let n = array.len();
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut pos = 0;
    let mut bytes = 0usize;
    while pos < n {
        let room = max_rows.saturating_sub(pos - start).max(1);
        let step = IPC_BLOCK_ROWS.min(room).min(n - pos);
        let block = slice_bytes(array.slice(pos, step).as_ref())?;
        bytes = bytes.saturating_add(block);
        pos += step;
        if pos - start >= max_rows || bytes >= max_bytes {
            ranges.push(start..pos);
            start = pos;
            bytes = 0;
        }
    }
    if start < n {
        ranges.push(start..n);
    }
    Ok(ranges)
}

/// The memory of a slice of `array`, as an `IPC` block counts it. A list or map counts its
/// offsets and the child values its slice covers. A struct counts its columns, and its
/// validity bits when it has nulls. Any other type uses arrow's slice measure.
///
/// `get_slice_memory_size` counts a list slice's whole child array, so lists recurse into the
/// covered child range instead.
fn slice_bytes(array: &dyn Array) -> Result<usize> {
    let rows = array.len();
    if rows == 0 {
        return Ok(0);
    }
    let validity = |nulls: Option<&NullBuffer>| nulls.map_or(0, |_| rows.div_ceil(8));
    match array.data_type() {
        DataType::List(_) => {
            let list = array.as_list::<i32>();
            let offsets = list.value_offsets();
            let child = covered_child(list.values().as_ref(), offsets_range_i64(offsets))?;
            Ok((rows + 1) * 4 + validity(list.nulls()) + child)
        }
        DataType::LargeList(_) => {
            let list = array.as_list::<i64>();
            let child = covered_child(
                list.values().as_ref(),
                offsets_range_i64(list.value_offsets()),
            )?;
            Ok((rows + 1) * 8 + validity(list.nulls()) + child)
        }
        DataType::Map(_, _) => {
            let map = array.as_map();
            let child = covered_child(map.entries(), offsets_range_i64(map.value_offsets()))?;
            Ok((rows + 1) * 4 + validity(map.nulls()) + child)
        }
        DataType::Struct(_) => {
            let strukt = array.as_struct();
            let mut total = validity(strukt.nulls());
            for column in strukt.columns() {
                total += slice_bytes(column.as_ref())?;
            }
            Ok(total)
        }
        _ => Ok(array.to_data().get_slice_memory_size()?),
    }
}

/// The first and last offset of a list slice, widened to i64.
fn offsets_range_i64<O: Into<i64> + Copy>(offsets: &[O]) -> (i64, i64) {
    let first = offsets.first().copied().map_or(0, Into::into);
    let last = offsets.last().copied().map_or(0, Into::into);
    (first, last)
}

/// The memory of the child values covered by a list slice from `first` to `last`.
fn covered_child(child: &dyn Array, (first, last): (i64, i64)) -> Result<usize> {
    let start = usize::try_from(first).unwrap_or(0);
    let end = usize::try_from(last).unwrap_or(0);
    if end <= start {
        return Ok(0);
    }
    slice_bytes(child.slice(start, end - start).as_ref())
}

/// The bytes each row of `array` counts for in the chunk budget. `has_dict` tells whether the
/// column has a dictionary, which decides what a Utf8 row and a List(Utf8) row cost.
///
/// Fixed-width types count their width. A Utf8 row with a dictionary counts its 4-byte code;
/// without one it counts its length plus 4. A Binary row counts its length plus 4. A List(Utf8)
/// row with a dictionary counts 4 bytes per element and 4 more for the row. A `Map<Utf8, Utf8>`
/// row counts, per entry, its key length, its value length (0 for a null value) and 8, and 4
/// more for the row.
fn row_sizes(array: &dyn Array, has_dict: bool) -> Vec<usize> {
    let n = array.len();
    match array.data_type() {
        DataType::Utf8 if has_dict => vec![4; n],
        DataType::Utf8 => match array.as_string_opt::<i32>() {
            Some(strings) => (0..n).map(|i| strings.value(i).len() + 4).collect(),
            None => vec![0; n],
        },
        DataType::Binary => match array.as_binary_opt::<i32>() {
            Some(binaries) => (0..n).map(|i| binaries.value(i).len() + 4).collect(),
            None => vec![0; n],
        },
        DataType::List(field) if field.data_type() == &DataType::Utf8 => {
            match array.as_list_opt::<i32>() {
                Some(list) => list_row_sizes(list),
                None => vec![0; n],
            }
        }
        dt if super::is_utf8_map(dt) => match array.as_map_opt() {
            Some(map) => map_row_sizes(map),
            None => vec![0; n],
        },
        dt => vec![value_bytes(dt); n],
    }
}

/// The byte budget of each row of a List(Utf8) array with a dictionary: 4 bytes per element
/// and 4 for the row. The elements are counted from the offsets, so no child is read.
fn list_row_sizes(list: &ListArray) -> Vec<usize> {
    list.value_offsets()
        .windows(2)
        .map(|pair| {
            let elements = match pair {
                [start, end] => usize::try_from(end.saturating_sub(*start)).unwrap_or(0),
                _ => 0,
            };
            elements * 4 + 4
        })
        .collect()
}

/// The byte budget of each row of a `Map<Utf8, Utf8>` array, as its encoding stores it.
fn map_row_sizes(map: &MapArray) -> Vec<usize> {
    let entries = map.entries();
    let (Some(keys), Some(values)) = (
        entries.column(0).as_string_opt::<i32>(),
        entries.column(1).as_string_opt::<i32>(),
    ) else {
        return vec![0; map.len()];
    };
    map.value_offsets()
        .windows(2)
        .map(|pair| {
            let (start, end) = match pair {
                [start, end] => (
                    usize::try_from(*start).unwrap_or(0),
                    usize::try_from(*end).unwrap_or(0),
                ),
                _ => (0, 0),
            };
            let entry_bytes: usize = (start..end)
                .map(|j| {
                    let value = if values.is_valid(j) {
                        values.value(j).len()
                    } else {
                        0
                    };
                    keys.value(j).len() + value + 8
                })
                .sum();
            entry_bytes + 4
        })
        .collect()
}

/// The value bytes one row of a fixed-width type `dt` counts for in the chunk budget.
fn value_bytes(dt: &DataType) -> usize {
    match dt {
        DataType::Boolean => 1,
        DataType::FixedSizeBinary(w) => usize::try_from(*w).unwrap_or(0),
        _ => dt.primitive_width().unwrap_or(0),
    }
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
    use arrow::array::{
        BinaryArray, FixedSizeBinaryArray, Int64Array, ListArray, StringArray, StructArray,
        TimestampNanosecondArray,
    };
    use arrow::buffer::OffsetBuffer;
    use arrow::datatypes::{Field, Fields, TimeUnit};
    use std::sync::Arc;

    fn lens(ranges: &[Range<usize>]) -> Vec<usize> {
        ranges.iter().map(Range::len).collect()
    }

    #[test]
    fn int64_rows_split_at_4096() {
        let array = Int64Array::from((0..10_000i64).collect::<Vec<_>>());
        let ranges = chunk_ranges(&array, 4096, 32 * 1024, false).unwrap();
        assert_eq!(lens(&ranges), vec![4096, 4096, 1808]);
    }

    #[test]
    fn fixed_size_binary_16_splits_at_2048_rows() {
        let array = FixedSizeBinaryArray::try_new(
            16,
            arrow::buffer::Buffer::from_vec(vec![0u8; 16 * 5000]),
            None,
        )
        .unwrap();
        let ranges = chunk_ranges(&array, 4096, 32 * 1024, false).unwrap();
        assert_eq!(ranges.first().map(|r| r.len()), Some(2048));
        assert_eq!(ranges.last().map(|r| r.end), Some(5000));
    }

    #[test]
    fn empty_array_has_no_chunks() {
        let array = Int64Array::from(Vec::<i64>::new());
        assert!(
            chunk_ranges(&array, 4096, 32 * 1024, false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn oversized_row_still_forms_a_non_empty_chunk() {
        let array = FixedSizeBinaryArray::try_new(
            16,
            arrow::buffer::Buffer::from_vec(vec![0u8; 16 * 3]),
            None,
        )
        .unwrap();
        assert_eq!(
            chunk_ranges(&array, 4096, 1, false).unwrap(),
            vec![0..1, 1..2, 2..3]
        );
    }

    #[test]
    fn utf8_rows_count_their_length_plus_four() {
        // Each row is 4 + 4 = 8 bytes, so a chunk closes after 5 rows of a 40-byte budget.
        let array = StringArray::from(vec!["abcd"; 10]);
        let ranges = chunk_ranges(&array, 4096, 40, false).unwrap();
        assert_eq!(lens(&ranges), vec![5, 5]);
    }

    #[test]
    fn binary_rows_count_their_length_plus_four() {
        let array = BinaryArray::from(vec![&[1u8, 2, 3, 4][..]; 10]);
        let ranges = chunk_ranges(&array, 4096, 40, false).unwrap();
        assert_eq!(lens(&ranges), vec![5, 5]);
    }

    #[test]
    fn list_of_utf8_rows_count_elements_and_row() {
        // With a dictionary a row with two elements costs 4 + 4 * 2 = 12 bytes. Rows of 12, 24
        // and 36 bytes: the third row reaches the 32-byte budget, so the chunks are [3, 1] rows.
        let mut builder = arrow::array::ListBuilder::new(arrow::array::StringBuilder::new());
        for _ in 0..4 {
            builder.values().append_value("ab");
            builder.values().append_value("cd");
            builder.append(true);
        }
        let array = builder.finish();
        let ranges = chunk_ranges(&array, 4096, 32, true).unwrap();
        assert_eq!(lens(&ranges), vec![3, 1]);
    }

    #[test]
    fn utf8_with_a_dictionary_costs_four_bytes_per_row() {
        let array = StringArray::from(vec!["abc"; 20_000]);
        let ranges = chunk_ranges(&array, 65_535, 32 * 1024, true).unwrap();
        assert_eq!(lens(&ranges), vec![8192, 8192, 3616]);
        let ranges = chunk_ranges(&array, 4096, 32 * 1024, true).unwrap();
        assert_eq!(lens(&ranges), vec![4096, 4096, 4096, 4096, 3616]);
    }

    #[test]
    fn list_of_utf8_with_a_dictionary_costs_four_per_element_and_row() {
        // One element per row costs 4 + 4 = 8 bytes, so 4,096 rows reach 32 KiB.
        let mut builder = arrow::array::ListBuilder::new(arrow::array::StringBuilder::new());
        for _ in 0..10_000 {
            builder.values().append_value("v");
            builder.append(true);
        }
        let array = builder.finish();
        let ranges = chunk_ranges(&array, 65_535, 32 * 1024, true).unwrap();
        assert_eq!(lens(&ranges), vec![4096, 4096, 1808]);
    }

    #[test]
    fn map_rows_cost_their_entries_and_row() {
        // One entry ("k", "v") costs 1 + 1 + 8 = 10 bytes and a row adds 4, so a row costs 14.
        // 100 rows reach the 1,400-byte budget exactly.
        let mut builder = arrow::array::MapBuilder::new(
            None,
            arrow::array::StringBuilder::new(),
            arrow::array::StringBuilder::new(),
        );
        for _ in 0..250 {
            builder.keys().append_value("k");
            builder.values().append_value("v");
            builder.append(true).unwrap();
        }
        let array = builder.finish();
        let ranges = chunk_ranges(&array, 65_535, 1_400, false).unwrap();
        assert_eq!(lens(&ranges), vec![100, 100, 50]);
    }

    #[test]
    fn one_100_kib_value_is_a_one_row_chunk() {
        let big = "x".repeat(100 * 1024);
        let array = StringArray::from(vec![big.as_str(), "a", "b"]);
        let ranges = chunk_ranges(&array, 4096, 32 * 1024, false).unwrap();
        assert_eq!(ranges, vec![0..1, 1..3]);
        assert!(ranges.iter().all(|r| !r.is_empty()));
    }

    /// `rows` events of one element each, with attributes of `attribute_len` bytes.
    fn events(rows: usize, attribute_len: usize) -> ListArray {
        let fields = Fields::from(vec![
            Field::new(
                "time",
                DataType::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())),
                false,
            ),
            Field::new("name", DataType::Utf8, false),
            Field::new("attributes", DataType::Utf8, true),
        ]);
        let times = TimestampNanosecondArray::from((0..rows as i64).collect::<Vec<_>>())
            .with_timezone("UTC");
        let names = StringArray::from_iter_values((0..rows).map(|_| "event"));
        let attributes =
            StringArray::from_iter_values((0..rows).map(|_| "a".repeat(attribute_len)));
        let structs = StructArray::try_new(
            fields.clone(),
            vec![Arc::new(times), Arc::new(names), Arc::new(attributes)],
            None,
        )
        .unwrap();
        ListArray::try_new(
            Arc::new(Field::new("item", DataType::Struct(fields), false)),
            OffsetBuffer::from_lengths(vec![1; rows]),
            Arc::new(structs),
            None,
        )
        .unwrap()
    }

    #[test]
    fn events_list_chunks_measure_blocks_of_rows() {
        let array = events(100_000, 40);
        let ranges = chunk_ranges(&array, 4096, 32 * 1024, false).unwrap();
        assert!(ranges.len() > 1, "expected several chunks");
        assert_eq!(ranges.first().map(|r| r.start), Some(0));
        assert_eq!(ranges.last().map(|r| r.end), Some(100_000));
        // Every chunk but the last closes on the byte budget, after more than one block.
        for range in &ranges[..ranges.len() - 1] {
            assert!(range.len() > 256, "chunk of {} rows", range.len());
        }
    }

    #[test]
    fn ipc_chunks_never_exceed_the_row_limit() {
        let array = events(1_000, 0);
        let ranges = chunk_ranges(&array, 300, usize::MAX, false).unwrap();
        assert!(ranges.iter().all(|r| r.len() <= 300));
        assert_eq!(ranges.last().map(|r| r.end), Some(1_000));
    }

    #[test]
    fn empty_ipc_array_has_no_chunks() {
        let array = events(0, 0);
        assert!(
            chunk_ranges(&array, 4096, 32 * 1024, false)
                .unwrap()
                .is_empty()
        );
    }
}
