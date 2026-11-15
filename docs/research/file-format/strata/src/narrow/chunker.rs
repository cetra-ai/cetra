//! Chunk boundaries: a chunk closes at `max_rows` rows or once its value bytes reach
//! `max_bytes`.

use std::ops::Range;

use arrow::array::{Array, AsArray};
use arrow::datatypes::DataType;

/// The row ranges of `array` that become chunks. They are non-empty, in order, and cover
/// `0..array.len()`. An empty array has no chunks.
pub fn chunk_ranges(array: &dyn Array, max_rows: u32, max_bytes: usize) -> Vec<Range<usize>> {
    let max_rows = usize::try_from(max_rows).unwrap_or(usize::MAX);
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut rows = 0usize;
    let mut bytes = 0usize;
    for (i, row_bytes) in row_sizes(array).into_iter().enumerate() {
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
    ranges
}

/// The bytes each row of `array` counts for in the chunk budget.
///
/// Fixed-width types count their width. A Utf8 row counts its length plus 4. A List(Utf8)
/// row counts, for each element, its length plus 4, and 4 more for the row.
fn row_sizes(array: &dyn Array) -> Vec<usize> {
    let n = array.len();
    match array.data_type() {
        DataType::Utf8 => match array.as_string_opt::<i32>() {
            Some(strings) => (0..n).map(|i| strings.value(i).len() + 4).collect(),
            None => vec![0; n],
        },
        DataType::List(field) if field.data_type() == &DataType::Utf8 => {
            match array.as_list_opt::<i32>() {
                Some(list) => list_row_sizes(list),
                None => vec![0; n],
            }
        }
        dt => vec![value_bytes(dt); n],
    }
}

/// The byte budget of each row of a List(Utf8) array.
fn list_row_sizes(list: &arrow::array::ListArray) -> Vec<usize> {
    let Some(child) = list.values().as_string_opt::<i32>() else {
        return vec![0; list.len()];
    };
    list.value_offsets()
        .windows(2)
        .map(|pair| {
            let (start, end) = match pair {
                [start, end] => (
                    usize::try_from(*start).unwrap_or(0),
                    usize::try_from(*end).unwrap_or(0),
                ),
                _ => (0, 0),
            };
            let elements: usize = (start..end).map(|j| child.value(j).len() + 4).sum();
            elements + 4
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
    use arrow::array::{FixedSizeBinaryArray, Int64Array};

    #[test]
    fn int64_rows_split_at_4096() {
        let array = Int64Array::from((0..10_000i64).collect::<Vec<_>>());
        let lens: Vec<usize> = chunk_ranges(&array, 4096, 32 * 1024)
            .iter()
            .map(|r| r.len())
            .collect();
        assert_eq!(lens, vec![4096, 4096, 1808]);
    }

    #[test]
    fn fixed_size_binary_16_splits_at_2048_rows() {
        let array = FixedSizeBinaryArray::try_new(
            16,
            arrow::buffer::Buffer::from_vec(vec![0u8; 16 * 5000]),
            None,
        )
        .unwrap();
        let ranges = chunk_ranges(&array, 4096, 32 * 1024);
        assert_eq!(ranges.first().map(|r| r.len()), Some(2048));
        assert_eq!(ranges.last().map(|r| r.end), Some(5000));
    }

    #[test]
    fn empty_array_has_no_chunks() {
        let array = Int64Array::from(Vec::<i64>::new());
        assert!(chunk_ranges(&array, 4096, 32 * 1024).is_empty());
    }

    #[test]
    fn oversized_row_still_forms_a_non_empty_chunk() {
        let array = FixedSizeBinaryArray::try_new(
            16,
            arrow::buffer::Buffer::from_vec(vec![0u8; 16 * 3]),
            None,
        )
        .unwrap();
        assert_eq!(chunk_ranges(&array, 4096, 1), vec![0..1, 1..2, 2..3]);
    }

    #[test]
    fn utf8_rows_count_their_length_plus_four() {
        // Each row is 4 + 4 = 8 bytes, so a chunk closes after 5 rows of a 40-byte budget.
        let array = arrow::array::StringArray::from(vec!["abcd"; 10]);
        let lens: Vec<usize> = chunk_ranges(&array, 4096, 40)
            .iter()
            .map(|r| r.len())
            .collect();
        assert_eq!(lens, vec![5, 5]);
    }

    #[test]
    fn list_of_utf8_rows_count_elements_and_row() {
        // A row with two elements of 2 bytes: 2 * (2 + 4) + 4 = 16 bytes.
        let mut builder = arrow::array::ListBuilder::new(arrow::array::StringBuilder::new());
        for _ in 0..4 {
            builder.values().append_value("ab");
            builder.values().append_value("cd");
            builder.append(true);
        }
        let array = builder.finish();
        let lens: Vec<usize> = chunk_ranges(&array, 4096, 32)
            .iter()
            .map(|r| r.len())
            .collect();
        assert_eq!(lens, vec![2, 2]);
    }
}
