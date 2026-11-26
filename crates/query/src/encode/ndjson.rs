//! Line-delimited JSON (NDJSON) chunk encoder.

use std::ops::Range;

use cetra_column::{Batch, Reservation};

use crate::error::QueryError;

use super::size::CappedBuffer;
use super::{BufferedSink, encode_json_keys, for_each_active_row, write_json_row};

/// Encodes active rows in `row_range` (`0..batch.num_rows()`) as compact JSON objects,
/// one per line terminated by `\n`, with explicit nulls and no formula guarding.
pub fn encode_ndjson_chunk(
    batch: &Batch,
    row_range: Range<usize>,
    max_bytes: usize,
    res: Option<&mut Reservation>,
) -> Result<Vec<u8>, QueryError> {
    if row_range.start > row_range.end || row_range.end > batch.num_rows() {
        return Err(QueryError::internal("ndjson row range out of bounds"));
    }
    if row_range.is_empty() {
        return Ok(Vec::new());
    }

    let encoded_keys = encode_json_keys(batch.schema())?;
    let mut sink = CappedBuffer::new(max_bytes, res);
    let mut out = BufferedSink::new(&mut sink);

    for_each_active_row(batch, row_range, |phys_row| {
        write_json_row(batch, phys_row, &encoded_keys, &mut out)?;
        out.push_byte(b'\n')
    })?;

    out.flush()?;
    Ok(sink.into_bytes())
}

#[cfg(test)]
mod tests {
    use cetra_column::{
        Bitmap, Column, DataType, Field, FlatColumn, FlatData, GlobalPool, Schema, Selection,
        Utf8Data,
    };

    use super::*;

    #[test]
    fn ndjson_writes_compact_lines_no_formula_guard_and_honors_range() {
        let schema = Schema::try_new_arc(vec![
            Field::new("s", DataType::Utf8, true),
            Field::new("f", DataType::F64, true),
        ])
        .unwrap();
        let batch = Batch::try_new(
            schema,
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(["=1+1", "skip", "two", "three"]).unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::F64(vec![f64::NEG_INFINITY, 0.0, 2.5, 0.0]),
                        Some(Bitmap::from_bools(&[true, true, true, false])),
                    )
                    .unwrap(),
                ),
            ],
            4,
            Selection::Indices(vec![0, 2, 3]),
        )
        .unwrap();

        let bytes = encode_ndjson_chunk(&batch, 0..2, usize::MAX, None).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(
            text,
            "{\"s\":\"=1+1\",\"f\":null}\n{\"s\":\"two\",\"f\":2.5}\n"
        );

        let empty = encode_ndjson_chunk(&batch, 1..1, usize::MAX, None).unwrap();
        assert!(empty.is_empty());

        let scope = GlobalPool::standalone(16);
        let mut res = scope.register("ndjson", false);
        let err = encode_ndjson_chunk(&batch, 0..3, 1024, Some(&mut res)).unwrap_err();
        assert!(matches!(err, QueryError::TooLarge));
        assert_eq!(res.size(), 0);
    }
}
