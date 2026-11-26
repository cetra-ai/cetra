//! RFC 4180 CSV header and chunk encoders with OWASP formula-injection quoting.

use std::fmt::Write as _;
use std::ops::Range;

use cetra_column::{Batch, Column, DataType, Reservation, Schema};

use crate::error::QueryError;
use crate::eval::{
    format_decimal, read_bool, read_bytes, read_date, read_decimal, read_f64, read_i64,
    read_interval, read_str, read_timestamp,
};
use crate::fn_time::format_date_days;

use super::size::CappedBuffer;
use super::{
    BufferedSink, for_each_active_row, write_cell_json_to_csv_sink, write_hex_to_string,
    write_interval, write_timestamp_ns_fixed9,
};

const FORMULA_PREFIXES: [char; 6] = ['=', '+', '-', '@', '\t', '\r'];

fn needs_formula_guard(text: &str) -> bool {
    text.starts_with(FORMULA_PREFIXES)
}

fn needs_csv_quotes(text: &str) -> bool {
    text.contains(['"', ',', '\r', '\n'])
}

/// Encodes the RFC 4180 CSV header line (`\r\n`-terminated) with formula guarding on header names.
pub fn encode_csv_header(schema: &Schema) -> Result<Vec<u8>, QueryError> {
    let mut out = Vec::with_capacity(schema.len() * 16 + 2);
    for (idx, field) in schema.fields().iter().enumerate() {
        if idx > 0 {
            out.push(b',');
        }
        push_csv_field_vec(&field.name, true, &mut out);
    }
    out.extend_from_slice(b"\r\n");
    Ok(out)
}

fn push_csv_field_vec(text: &str, guard: bool, out: &mut Vec<u8>) {
    let add_guard = guard && needs_formula_guard(text);
    let quote = needs_csv_quotes(text);
    if quote {
        out.push(b'"');
    }
    if add_guard {
        out.push(b'\'');
    }
    if quote {
        let bytes = text.as_bytes();
        let mut start = 0usize;
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'"' {
                if i > start {
                    out.extend_from_slice(&bytes[start..i]);
                }
                out.extend_from_slice(b"\"\"");
                start = i + 1;
            }
        }
        if start < bytes.len() {
            out.extend_from_slice(&bytes[start..]);
        }
    } else {
        out.extend_from_slice(text.as_bytes());
    }
    if quote {
        out.push(b'"');
    }
}

fn write_csv_field_sink(
    text: &str,
    guard: bool,
    out: &mut BufferedSink<'_, '_>,
) -> Result<(), QueryError> {
    let add_guard = guard && needs_formula_guard(text);
    let quote = needs_csv_quotes(text);
    if quote {
        out.push_byte(b'"')?;
    }
    if add_guard {
        out.push_byte(b'\'')?;
    }
    if quote {
        let bytes = text.as_bytes();
        let mut start = 0usize;
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'"' {
                if i > start {
                    out.push_bytes(&bytes[start..i])?;
                }
                out.push_bytes(b"\"\"")?;
                start = i + 1;
            }
        }
        if start < bytes.len() {
            out.push_bytes(&bytes[start..])?;
        }
    } else {
        out.push_bytes(text.as_bytes())?;
    }
    if quote {
        out.push_byte(b'"')?;
    }
    Ok(())
}

/// Encodes active rows in `row_range` (`0..batch.num_rows()`) as `\r\n`-terminated RFC 4180
/// CSV rows within `max_bytes` and `res`.
pub fn encode_csv_chunk(
    batch: &Batch,
    row_range: Range<usize>,
    max_bytes: usize,
    res: Option<&mut Reservation>,
) -> Result<Vec<u8>, QueryError> {
    if row_range.start > row_range.end || row_range.end > batch.num_rows() {
        return Err(QueryError::internal("csv row range out of bounds"));
    }
    if row_range.is_empty() {
        return Ok(Vec::new());
    }

    let mut sink = CappedBuffer::new(max_bytes, res);
    let mut out = BufferedSink::new(&mut sink);
    let mut cell_scratch = String::with_capacity(64);
    let mut json_num_scratch = Vec::with_capacity(32);

    for_each_active_row(batch, row_range, |phys_row| {
        for (col_idx, col) in batch.columns().iter().enumerate() {
            if col_idx > 0 {
                out.push_byte(b',')?;
            }
            write_csv_cell(
                col,
                phys_row,
                &mut cell_scratch,
                &mut json_num_scratch,
                &mut out,
            )?;
        }
        out.push_bytes(b"\r\n")
    })?;

    out.flush()?;
    Ok(sink.into_bytes())
}

fn write_csv_cell(
    col: &Column,
    phys_row: usize,
    scratch: &mut String,
    json_num_scratch: &mut Vec<u8>,
    out: &mut BufferedSink<'_, '_>,
) -> Result<(), QueryError> {
    if col.is_null(phys_row) {
        return Ok(());
    }
    match col.data_type() {
        DataType::Null => Ok(()),
        DataType::Bool => {
            let v = read_bool(col, phys_row)
                .ok_or_else(|| QueryError::internal("missing bool cell"))?;
            out.push_bytes(if v { b"true" } else { b"false" })
        }
        DataType::I64 => {
            let v =
                read_i64(col, phys_row).ok_or_else(|| QueryError::internal("missing i64 cell"))?;
            scratch.clear();
            let _ = write!(scratch, "{v}");
            out.push_bytes(scratch.as_bytes())
        }
        DataType::F64 => {
            let v =
                read_f64(col, phys_row).ok_or_else(|| QueryError::internal("missing f64 cell"))?;
            if !v.is_finite() {
                return Ok(());
            }
            json_num_scratch.clear();
            serde_json::to_writer(&mut *json_num_scratch, &v)
                .map_err(|_| QueryError::internal("failed to encode f64"))?;
            out.push_bytes(json_num_scratch)
        }
        DataType::Decimal128 { scale, .. } => {
            let v = read_decimal(col, phys_row)
                .ok_or_else(|| QueryError::internal("missing decimal cell"))?;
            scratch.clear();
            scratch.push_str(&format_decimal(v, scale));
            out.push_bytes(scratch.as_bytes())
        }
        DataType::TimestampNs { utc } => {
            let v = read_timestamp(col, phys_row)
                .ok_or_else(|| QueryError::internal("missing timestamp cell"))?;
            scratch.clear();
            write_timestamp_ns_fixed9(v, utc, scratch)
                .map_err(|_| QueryError::internal("failed to format timestamp"))?;
            write_csv_field_sink(scratch, true, out)
        }
        DataType::Date => {
            let v = read_date(col, phys_row)
                .ok_or_else(|| QueryError::internal("missing date cell"))?;
            scratch.clear();
            scratch.push_str(&format_date_days(v));
            write_csv_field_sink(scratch, true, out)
        }
        DataType::Interval => {
            let iv = read_interval(col, phys_row)
                .ok_or_else(|| QueryError::internal("missing interval cell"))?;
            scratch.clear();
            write_interval(iv, scratch)
                .map_err(|_| QueryError::internal("failed to format interval"))?;
            write_csv_field_sink(scratch, true, out)
        }
        DataType::Utf8 => {
            let s =
                read_str(col, phys_row).ok_or_else(|| QueryError::internal("missing utf8 cell"))?;
            write_csv_field_sink(s, true, out)
        }
        DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16 => {
            let b = read_bytes(col, phys_row)
                .ok_or_else(|| QueryError::internal("missing binary cell"))?;
            scratch.clear();
            write_hex_to_string(b, scratch);
            write_csv_field_sink(scratch, true, out)
        }
        DataType::List(_) | DataType::MapUtf8 | DataType::Events | DataType::Links => {
            write_cell_json_to_csv_sink(col, phys_row, out, |text, sink| {
                write_csv_field_sink(text, false, sink)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use cetra_column::{
        Bitmap, Column, DataType, Field, FlatColumn, FlatData, GlobalPool, Interval, MapData,
        ScalarValue, Schema, Selection, Utf8Data,
    };

    use super::*;

    #[test]
    fn csv_header_applies_formula_guard_and_rfc4180() {
        let schema = Schema::try_new(vec![
            Field::new("plain", DataType::I64, false),
            Field::new("=cmd", DataType::Utf8, false),
            Field::new("a,b\"c", DataType::Utf8, false),
            Field::new("\rhead", DataType::Utf8, false),
        ])
        .unwrap();
        let header = String::from_utf8(encode_csv_header(&schema).unwrap()).unwrap();
        assert_eq!(header, "plain,'=cmd,\"a,b\"\"c\",\"'\rhead\"\r\n");
    }

    #[test]
    fn csv_chunk_guards_strings_and_temporal_only_not_numbers_or_json() {
        let schema = Schema::try_new_arc(vec![
            Field::new("s", DataType::Utf8, true),
            Field::new("i", DataType::I64, true),
            Field::new("f", DataType::F64, true),
            Field::new(
                "d",
                DataType::Decimal128 {
                    precision: 10,
                    scale: 2,
                },
                true,
            ),
            Field::new("b", DataType::Bool, true),
            Field::new("n", DataType::Null, true),
            Field::new("dt", DataType::Date, true),
            Field::new("iv", DataType::Interval, true),
            Field::new("mp", DataType::MapUtf8, true),
        ])
        .unwrap();

        let batch = Batch::try_new(
            schema,
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices([
                                "=SUM(A1)",
                                "+1",
                                "-1",
                                "@at",
                                "\ttab",
                                "\rcr",
                                "ok, \"q\"",
                            ])
                            .unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Const {
                    value: ScalarValue::I64(-42),
                    len: 7,
                },
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::F64(vec![-1.5, f64::NAN, f64::INFINITY, 2.0, 0.0, 1.0, 3.0]),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Const {
                    value: ScalarValue::Decimal128 {
                        value: -123,
                        precision: 10,
                        scale: 2,
                    },
                    len: 7,
                },
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Bool(Bitmap::from_bools(&[
                            true, false, true, false, true, false, true,
                        ])),
                        Some(Bitmap::from_bools(&[
                            true, false, true, true, true, true, true,
                        ])),
                    )
                    .unwrap(),
                ),
                Column::Flat(FlatColumn::try_new(FlatData::Null(7), None).unwrap()),
                Column::Const {
                    value: ScalarValue::Date(-719_529), // -0001-12-31
                    len: 7,
                },
                Column::Const {
                    value: ScalarValue::Interval(Interval {
                        months: -1,
                        days: 0,
                        nanos: 0,
                    }),
                    len: 7,
                },
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::MapUtf8(Box::new(
                            MapData::try_new(
                                vec![0, 2, 2, 2, 2, 2, 2, 2],
                                Utf8Data::try_from_slices(["a", "b"]).unwrap(),
                                Utf8Data::try_from_slices(["1", ""]).unwrap(),
                                Some(Bitmap::from_bools(&[true, false])),
                            )
                            .unwrap(),
                        )),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            7,
            Selection::Mask(Bitmap::from_bools(&[
                true, true, false, false, false, true, true,
            ])),
        )
        .unwrap();

        let bytes = encode_csv_chunk(&batch, 0..batch.num_rows(), usize::MAX, None).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let expected = concat!(
            "'=SUM(A1),-42,-1.5,-1.23,true,,'-0001-12-31,'-1 mons,\"{\"\"a\"\":\"\"1\"\",\"\"b\"\":null}\"\r\n",
            "'+1,-42,,-1.23,,,'-0001-12-31,'-1 mons,{}\r\n",
            "\"'\rcr\",-42,1.0,-1.23,false,,'-0001-12-31,'-1 mons,{}\r\n",
            "\"ok, \"\"q\"\"\",-42,3.0,-1.23,true,,'-0001-12-31,'-1 mons,{}\r\n",
        );
        assert_eq!(text, expected);
    }

    #[test]
    fn csv_chunk_enforces_budget_and_rolls_back_reservation() {
        let schema = Schema::try_new_arc(vec![Field::new(
            "list",
            DataType::List(Box::new(DataType::Utf8)),
            false,
        )])
        .unwrap();
        let big = "a".repeat(500);
        let elems = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices([big.as_str()]).unwrap()),
                None,
            )
            .unwrap(),
        );
        let col = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets: vec![0, 1],
                    values: Box::new(elems),
                },
                None,
            )
            .unwrap(),
        );
        let batch = Batch::try_new_all(schema, vec![col], 1).unwrap();
        let scope = GlobalPool::standalone(200);
        let mut res = scope.register("csv", false);
        let err = encode_csv_chunk(&batch, 0..1, 1024, Some(&mut res)).unwrap_err();
        assert!(matches!(err, QueryError::TooLarge));
        assert_eq!(res.size(), 0);

        // Peak simultaneous reservation holds temp JSON (104B for 100-byte elem) + CSV output (108B):
        // pool cap 150B must reject even though each piece alone is < 150B; cap 240B succeeds and
        // shrinks back to final CSV length.
        let med = "a".repeat(100);
        let med_elems = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices([med.as_str()]).unwrap()),
                None,
            )
            .unwrap(),
        );
        let med_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets: vec![0, 1],
                    values: Box::new(med_elems),
                },
                None,
            )
            .unwrap(),
        );
        let med_schema = Schema::try_new_arc(vec![Field::new(
            "list",
            DataType::List(Box::new(DataType::Utf8)),
            false,
        )])
        .unwrap();
        let med_batch = Batch::try_new_all(med_schema, vec![med_col], 1).unwrap();

        let tight = GlobalPool::standalone(150);
        let mut tight_res = tight.register("csv_tight", false);
        let err_peak = encode_csv_chunk(&med_batch, 0..1, 1024, Some(&mut tight_res)).unwrap_err();
        assert!(matches!(err_peak, QueryError::TooLarge));
        assert_eq!(tight_res.size(), 0);

        let roomy = GlobalPool::standalone(240);
        let mut roomy_res = roomy.register("csv_roomy", false);
        let ok_bytes = encode_csv_chunk(&med_batch, 0..1, 1024, Some(&mut roomy_res)).unwrap();
        assert_eq!(roomy_res.size(), ok_bytes.len());
    }
}
