//! Chunk boundaries: a chunk closes at `max_rows` rows or once its value bytes reach
//! `max_bytes`.

use std::ops::Range;

use arrow::array::Array;
use arrow::datatypes::DataType;

/// The row ranges of `array` that become chunks. They are non-empty, in order, and cover
/// `0..array.len()`. An empty array has no chunks.
pub fn chunk_ranges(array: &dyn Array, max_rows: u32, max_bytes: usize) -> Vec<Range<usize>> {
    let row_bytes = value_bytes(array.data_type());
    let max_rows = usize::try_from(max_rows).unwrap_or(usize::MAX);
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut rows = 0usize;
    let mut bytes = 0usize;
    for i in 0..array.len() {
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

/// The value bytes one row of `dt` counts for in the chunk budget.
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
}
