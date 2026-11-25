//! Total-order `SortKey` and row comparison across all `Column` encodings and sortable `DataType`s.

use std::cmp::Ordering;

use cetra_column::{Column, DataType};

use crate::eval::{
    self, ElemRef, compare_decimals, f64_cmp, list_ref_at, list_ref_elem, list_ref_len,
};
use crate::ir::Expr;

/// Sort specification shared by `TopK`, `ExternalSort`, `Window`, `FirstNonNullByOrder`, and
/// `TracesSource`/`ScoresSource`.
#[derive(Debug, Clone, PartialEq)]
pub struct SortKey {
    /// Expression producing the sort key column.
    pub expr: Expr,
    /// True for `DESC`, false for `ASC`.
    pub descending: bool,
    /// Explicit null placement (`true` = `NULLS FIRST`, `false` = `NULLS LAST`).
    pub nulls_first: bool,
}

/// Compares row `a` and row `b` within the same evaluated `key_cols` slice (using `eval::f64_cmp`
/// for `F64` and explicit `nulls_first`, supporting all sortable `DataType`s across all 4 `Column` encodings).
pub fn compare_rows(key_cols: &[Column], keys: &[SortKey], a: usize, b: usize) -> Ordering {
    compare_rows_across(key_cols, a, key_cols, b, keys)
}

/// Compares row `a` in `cols_a` against row `b` in `cols_b` (used by k-way merge and `TopKSink::merge`).
pub fn compare_rows_across(
    cols_a: &[Column],
    a: usize,
    cols_b: &[Column],
    b: usize,
    keys: &[SortKey],
) -> Ordering {
    for (idx, key) in keys.iter().enumerate() {
        let (Some(col_a), Some(col_b)) = (cols_a.get(idx), cols_b.get(idx)) else {
            continue;
        };
        let null_a = col_a.is_null(a);
        let null_b = col_b.is_null(b);
        match (null_a, null_b) {
            (true, true) => continue,
            (true, false) => {
                return if key.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            (false, true) => {
                return if key.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
            }
            (false, false) => {}
        }

        let raw_ord = compare_non_null_cols(col_a, a, col_b, b);
        let ord = if key.descending {
            raw_ord.reverse()
        } else {
            raw_ord
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

fn compare_non_null_cols(col_a: &Column, a: usize, col_b: &Column, b: usize) -> Ordering {
    match (col_a.data_type(), col_b.data_type()) {
        (DataType::Null, DataType::Null) => Ordering::Equal,
        (DataType::Bool, DataType::Bool) => {
            eval::read_bool(col_a, a).cmp(&eval::read_bool(col_b, b))
        }
        (DataType::I64, DataType::I64) => eval::read_i64(col_a, a).cmp(&eval::read_i64(col_b, b)),
        (DataType::F64, DataType::F64) => {
            match (eval::read_f64(col_a, a), eval::read_f64(col_b, b)) {
                (Some(x), Some(y)) => f64_cmp(x, y),
                _ => Ordering::Equal,
            }
        }
        (DataType::Decimal128 { scale: s1, .. }, DataType::Decimal128 { scale: s2, .. }) => {
            match (eval::read_decimal(col_a, a), eval::read_decimal(col_b, b)) {
                (Some(x), Some(y)) => compare_decimals(x, s1, y, s2).unwrap_or(Ordering::Equal),
                _ => Ordering::Equal,
            }
        }
        (DataType::TimestampNs { .. }, DataType::TimestampNs { .. }) => {
            eval::read_timestamp(col_a, a).cmp(&eval::read_timestamp(col_b, b))
        }
        (DataType::Date, DataType::Date) => {
            eval::read_date(col_a, a).cmp(&eval::read_date(col_b, b))
        }
        (DataType::Interval, DataType::Interval) => {
            match (eval::read_interval(col_a, a), eval::read_interval(col_b, b)) {
                (Some(x), Some(y)) => (x.months, x.days, x.nanos).cmp(&(y.months, y.days, y.nanos)),
                _ => Ordering::Equal,
            }
        }
        (DataType::Utf8, DataType::Utf8) => eval::read_str(col_a, a).cmp(&eval::read_str(col_b, b)),
        (
            DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16,
            DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16,
        ) => eval::read_bytes(col_a, a).cmp(&eval::read_bytes(col_b, b)),
        (DataType::List(_), DataType::List(_)) => {
            compare_elem_refs(ElemRef::Col(col_a, a), ElemRef::Col(col_b, b))
        }
        _ => Ordering::Equal,
    }
}

fn compare_elem_refs(ea: ElemRef<'_>, eb: ElemRef<'_>) -> Ordering {
    match (ea.is_null(), eb.is_null()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        (false, false) => {}
    }

    match (ea.data_type(), eb.data_type()) {
        (DataType::Null, DataType::Null) => Ordering::Equal,
        (DataType::Bool, DataType::Bool) => ea.read_bool().cmp(&eb.read_bool()),
        (DataType::I64, DataType::I64) => ea.read_i64().cmp(&eb.read_i64()),
        (DataType::F64, DataType::F64) => match (ea.read_f64(), eb.read_f64()) {
            (Some(x), Some(y)) => f64_cmp(x, y),
            _ => Ordering::Equal,
        },
        (DataType::Decimal128 { .. }, DataType::Decimal128 { .. }) => {
            match (ea.read_decimal(), eb.read_decimal()) {
                (Some((x, s1)), Some((y, s2))) => {
                    compare_decimals(x, s1, y, s2).unwrap_or(Ordering::Equal)
                }
                _ => Ordering::Equal,
            }
        }
        (DataType::TimestampNs { .. }, DataType::TimestampNs { .. }) => {
            ea.read_timestamp().cmp(&eb.read_timestamp())
        }
        (DataType::Date, DataType::Date) => ea.read_date().cmp(&eb.read_date()),
        (DataType::Interval, DataType::Interval) => {
            match (ea.read_interval(), eb.read_interval()) {
                (Some(x), Some(y)) => (x.months, x.days, x.nanos).cmp(&(y.months, y.days, y.nanos)),
                _ => Ordering::Equal,
            }
        }
        (DataType::Utf8, DataType::Utf8) => ea.read_str().cmp(&eb.read_str()),
        (
            DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16,
            DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16,
        ) => ea.read_bytes().cmp(&eb.read_bytes()),
        (DataType::List(_), DataType::List(_)) => {
            let (Some(la), Some(lb)) = (
                match ea {
                    ElemRef::Col(c, r) => list_ref_at(c, r),
                    _ => ea.as_list_ref(),
                },
                match eb {
                    ElemRef::Col(c, r) => list_ref_at(c, r),
                    _ => eb.as_list_ref(),
                },
            ) else {
                return Ordering::Equal;
            };
            let len_a = list_ref_len(la);
            let len_b = list_ref_len(lb);
            for i in 0..len_a.min(len_b) {
                let ord = compare_elem_refs(list_ref_elem(la, i), list_ref_elem(lb, i));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            len_a.cmp(&len_b)
        }
        _ => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cetra_column::{
        BinaryData, Bitmap, DictColumn, FlatColumn, FlatData, Interval, RleColumn, ScalarValue,
        Utf8Data,
    };

    fn key(descending: bool, nulls_first: bool) -> SortKey {
        SortKey {
            expr: Expr::Literal(ScalarValue::Null(DataType::Null)),
            descending,
            nulls_first,
        }
    }

    #[test]
    fn compare_rows_across_all_types_and_nulls_first_last() {
        // Test nulls_first = true vs false with both ASC and DESC
        let c_nulls = Column::Flat(
            FlatColumn::try_new(
                FlatData::I64(vec![10, 0, 20]),
                Some(Bitmap::from_bools(&[true, false, true])),
            )
            .unwrap(),
        );
        // NULLS FIRST + ASC: null (1) < 10 (0) < 20 (2)
        assert_eq!(
            compare_rows(std::slice::from_ref(&c_nulls), &[key(false, true)], 1, 0),
            Ordering::Less
        );
        // NULLS LAST + ASC: null (1) > 20 (2)
        assert_eq!(
            compare_rows(std::slice::from_ref(&c_nulls), &[key(false, false)], 1, 2),
            Ordering::Greater
        );
        // NULLS FIRST + DESC: null (1) < 20 (2) < 10 (0)
        assert_eq!(
            compare_rows(std::slice::from_ref(&c_nulls), &[key(true, true)], 1, 2),
            Ordering::Less
        );
        assert_eq!(
            compare_rows(std::slice::from_ref(&c_nulls), &[key(true, true)], 2, 0),
            Ordering::Less
        );
        // NULLS LAST + DESC: 20 (2) < 10 (0) < null (1)
        assert_eq!(
            compare_rows(std::slice::from_ref(&c_nulls), &[key(true, false)], 1, 0),
            Ordering::Greater
        );

        // F64 total order: -inf < -0.0 == +0.0 < +inf < NaN == NaN
        let f_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::F64(vec![
                    f64::NEG_INFINITY,
                    -0.0,
                    0.0,
                    f64::INFINITY,
                    f64::NAN,
                    f64::NAN,
                ]),
                None,
            )
            .unwrap(),
        );
        let k_asc = [key(false, false)];
        assert_eq!(
            compare_rows(std::slice::from_ref(&f_col), &k_asc, 0, 1),
            Ordering::Less
        );
        assert_eq!(
            compare_rows(std::slice::from_ref(&f_col), &k_asc, 1, 2),
            Ordering::Equal
        );
        assert_eq!(
            compare_rows(std::slice::from_ref(&f_col), &k_asc, 2, 3),
            Ordering::Less
        );
        assert_eq!(
            compare_rows(std::slice::from_ref(&f_col), &k_asc, 3, 4),
            Ordering::Less
        );
        assert_eq!(
            compare_rows(std::slice::from_ref(&f_col), &k_asc, 4, 5),
            Ordering::Equal
        );

        // Cross-encoding comparison (Flat vs Const vs Dict vs Rle) for Utf8
        let flat_s = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(["apple", "banana"]).unwrap()),
                None,
            )
            .unwrap(),
        );
        let const_s = Column::try_new_const(ScalarValue::Utf8("banana".to_owned()), 2).unwrap();
        let dict_s = Column::Dict(
            DictColumn::try_new(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(["cherry", "apple"]).unwrap()),
                    None,
                )
                .unwrap(),
                vec![1, 0],
                None,
            )
            .unwrap(),
        );
        let rle_s = Column::Rle(
            RleColumn::try_new(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(["banana", "date"]).unwrap()),
                    None,
                )
                .unwrap(),
                vec![1, 2],
            )
            .unwrap(),
        );
        assert_eq!(
            compare_rows_across(
                std::slice::from_ref(&flat_s),
                0,
                std::slice::from_ref(&dict_s),
                0,
                &k_asc
            ),
            Ordering::Equal
        );
        assert_eq!(
            compare_rows_across(&[flat_s], 1, &[const_s], 0, &k_asc),
            Ordering::Equal
        );
        assert_eq!(
            compare_rows_across(&[dict_s], 1, &[rle_s], 1, &k_asc),
            Ordering::Less
        );

        // Decimal128 across different scales: 1.50 (150, scale 2) == 1.500 (1500, scale 3)
        let d_a = Column::Flat(
            FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vec![150, 200],
                    precision: 10,
                    scale: 2,
                },
                None,
            )
            .unwrap(),
        );
        let d_b = Column::Flat(
            FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vec![1500, 1999],
                    precision: 10,
                    scale: 3,
                },
                None,
            )
            .unwrap(),
        );
        assert_eq!(
            compare_rows_across(
                std::slice::from_ref(&d_a),
                0,
                std::slice::from_ref(&d_b),
                0,
                &k_asc
            ),
            Ordering::Equal
        );
        assert_eq!(
            compare_rows_across(&[d_a], 1, &[d_b], 1, &k_asc),
            Ordering::Greater
        );

        // All remaining scalar types: Bool, TimestampNs, Date, Interval, Binary, FixedBytes8, FixedBytes16, Null
        let bool_col = Column::Flat(
            FlatColumn::try_new(FlatData::Bool(Bitmap::from_bools(&[false, true])), None).unwrap(),
        );
        assert_eq!(compare_rows(&[bool_col], &k_asc, 0, 1), Ordering::Less);

        let ts_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: vec![100, 200],
                    utc: true,
                },
                None,
            )
            .unwrap(),
        );
        assert_eq!(compare_rows(&[ts_col], &k_asc, 0, 1), Ordering::Less);

        let date_col =
            Column::Flat(FlatColumn::try_new(FlatData::Date(vec![-5, 10]), None).unwrap());
        assert_eq!(compare_rows(&[date_col], &k_asc, 0, 1), Ordering::Less);

        let iv_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Interval(vec![
                    Interval {
                        months: 1,
                        days: 2,
                        nanos: 3,
                    },
                    Interval {
                        months: 1,
                        days: 2,
                        nanos: 4,
                    },
                ]),
                None,
            )
            .unwrap(),
        );
        assert_eq!(compare_rows(&[iv_col], &k_asc, 0, 1), Ordering::Less);

        let bin_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Binary(BinaryData::try_from_slices([b"aa".as_slice(), b"ab"]).unwrap()),
                None,
            )
            .unwrap(),
        );
        assert_eq!(compare_rows(&[bin_col], &k_asc, 0, 1), Ordering::Less);

        let f8_col = Column::Flat(
            FlatColumn::try_new(FlatData::FixedBytes8(vec![[0u8; 8], [1u8; 8]]), None).unwrap(),
        );
        assert_eq!(compare_rows(&[f8_col], &k_asc, 0, 1), Ordering::Less);

        let f16_col = Column::Flat(
            FlatColumn::try_new(FlatData::FixedBytes16(vec![[0u8; 16], [1u8; 16]]), None).unwrap(),
        );
        assert_eq!(compare_rows(&[f16_col], &k_asc, 0, 1), Ordering::Less);

        let null_col = Column::Flat(FlatColumn::try_new(FlatData::Null(2), None).unwrap());
        assert_eq!(compare_rows(&[null_col], &k_asc, 0, 1), Ordering::Equal);

        // Multi-key tie-breaker
        let k0 = Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1, 1]), None).unwrap());
        let k1 = Column::Flat(FlatColumn::try_new(FlatData::I64(vec![10, 20]), None).unwrap());
        assert_eq!(
            compare_rows(&[k0, k1], &[key(false, false), key(true, false)], 0, 1),
            Ordering::Greater
        );
    }
}
