//! Typed readers for `cetra_column::Batch` across all four `Column` encodings
//! (`Flat`, `Const`, `Dict`, `Rle`) and `Selection` variants.

use cetra_column::{Batch, Bitmap, Column, DataType, FlatColumn, FlatData, ScalarValue, Selection};

use crate::error::EngineError;
use crate::query_error::RunError;

/// Returns `RunError::Internal` with an opaque engine error.
pub(crate) fn internal(_message: impl Into<String>) -> RunError {
    RunError::Internal(EngineError::internal("reads column decode error"))
}

/// Returns the column named `name` in `batch`, or `RunError::Internal` if absent.
pub(crate) fn require_col<'a>(batch: &'a Batch, name: &str) -> Result<&'a Column, RunError> {
    match batch.column_by_name(name) {
        Some(column) => Ok(column),
        None => Err(missing_column(batch, name)),
    }
}

fn missing_column(batch: &Batch, name: &str) -> RunError {
    let columns: Vec<&str> = batch
        .schema()
        .fields()
        .iter()
        .map(|field| field.name.as_str())
        .collect();
    internal(format!("missing column {name}; columns {columns:?}"))
}

/// Maps logical active row index `active_row` (`0..batch.num_rows()`) to physical row index (`0..batch.raw_len()`).
pub(crate) fn physical_row(batch: &Batch, active_row: usize) -> Result<usize, RunError> {
    let raw_len = batch.raw_len();
    match batch.selection() {
        Selection::All => {
            if active_row < raw_len {
                Ok(active_row)
            } else {
                Err(internal(format!(
                    "row index {active_row} out of bounds for batch len {raw_len}"
                )))
            }
        }
        Selection::Mask(mask) => mask.iter_ones().nth(active_row).ok_or_else(|| {
            internal(format!(
                "active row index {active_row} out of bounds for masked batch"
            ))
        }),
        Selection::Indices(indices) => {
            indices
                .get(active_row)
                .map(|&idx| idx as usize)
                .ok_or_else(|| {
                    internal(format!(
                        "active row index {active_row} out of bounds for indexed batch"
                    ))
                })
        }
    }
}

/// Iterates physical row indices of active rows in `batch`.
pub(crate) fn active_rows(batch: &Batch) -> ActiveRows<'_> {
    match batch.selection() {
        Selection::All => ActiveRows::All(0..batch.raw_len()),
        Selection::Mask(mask) => ActiveRows::Mask(MaskWordIter::new(mask)),
        Selection::Indices(indices) => ActiveRows::Indices(indices.iter()),
    }
}

/// Physical row iterator over active rows in a [`Batch`].
pub(crate) enum ActiveRows<'a> {
    All(std::ops::Range<usize>),
    Mask(MaskWordIter<'a>),
    Indices(std::slice::Iter<'a, u32>),
}

/// Iterates set bit positions of a [`Bitmap`] via its backing `u64` words.
pub(crate) struct MaskWordIter<'a> {
    words: &'a [u64],
    len: usize,
    word_idx: usize,
    bit_base: usize,
    current_word: u64,
}

impl<'a> MaskWordIter<'a> {
    fn new(bm: &'a Bitmap) -> Self {
        let words = bm.words();
        Self {
            words,
            len: bm.len(),
            word_idx: 0,
            bit_base: 0,
            current_word: words.first().copied().unwrap_or(0),
        }
    }
}

impl Iterator for MaskWordIter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        while self.current_word == 0 {
            self.word_idx += 1;
            if self.word_idx >= self.words.len() {
                return None;
            }
            self.current_word = self.words[self.word_idx];
            self.bit_base = self.word_idx * 64;
        }
        let tz = self.current_word.trailing_zeros() as usize;
        let pos = self.bit_base + tz;
        if pos >= self.len {
            self.current_word = 0;
            return None;
        }
        self.current_word &= self.current_word - 1;
        Some(pos)
    }
}

impl Iterator for ActiveRows<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::All(range) => range.next(),
            Self::Mask(iter) => iter.next(),
            Self::Indices(iter) => iter.next().map(|&idx| idx as usize),
        }
    }
}

fn resolve_flat(col: &Column, phys_row: usize) -> Option<(&FlatColumn, usize)> {
    match col {
        Column::Flat(fc) => {
            (phys_row < fc.len() && !fc.is_null(phys_row)).then_some((fc, phys_row))
        }
        Column::Const { .. } => None,
        Column::Dict(d) => {
            if phys_row >= d.len() || d.is_null(phys_row) {
                return None;
            }
            let code = *d.codes().get(phys_row)? as usize;
            (!d.values().is_null(code)).then_some((d.values(), code))
        }
        Column::Rle(r) => {
            if phys_row >= r.len() {
                return None;
            }
            let run_idx = r
                .run_ends()
                .partition_point(|&end| (end as usize) <= phys_row);
            (!r.values().is_null(run_idx)).then_some((r.values(), run_idx))
        }
    }
}

fn check_bounds(col: &Column, phys_row: usize) -> Result<(), RunError> {
    if phys_row >= col.len() {
        return Err(internal(format!(
            "physical row {phys_row} out of bounds for column len {}",
            col.len()
        )));
    }
    Ok(())
}

/// Reads a required `I64` at logical `active_row` in `batch`.
pub(crate) fn i64_at(batch: &Batch, col: &Column, active_row: usize) -> Result<i64, RunError> {
    let phys = physical_row(batch, active_row)?;
    raw_i64_at(col, phys)
}

/// Reads a required `I64` at physical row `phys_row`.
pub(crate) fn raw_i64_at(col: &Column, phys_row: usize) -> Result<i64, RunError> {
    check_bounds(col, phys_row)?;
    if col.data_type() != DataType::I64 {
        return Err(internal(format!(
            "aggregate count has type {}",
            col.data_type().display_name()
        )));
    }
    if col.is_null(phys_row) {
        return Err(internal("aggregate count is null"));
    }
    if let Column::Const { value, .. } = col {
        return match value {
            ScalarValue::I64(v) => Ok(*v),
            _ => Err(internal("aggregate count scalar mismatch")),
        };
    }
    let Some((fc, flat_row)) = resolve_flat(col, phys_row) else {
        return Err(internal("aggregate count is null"));
    };
    match fc.data() {
        FlatData::I64(values) => Ok(values[flat_row]),
        _ => Err(internal("aggregate count flat storage mismatch")),
    }
}

/// Reads an optional `F64` (or `I64` widened to `f64`) at logical `active_row` in `batch`.
pub(crate) fn f64_at(
    batch: &Batch,
    col: &Column,
    active_row: usize,
) -> Result<Option<f64>, RunError> {
    let phys = physical_row(batch, active_row)?;
    raw_f64_at(col, phys)
}

/// Reads an optional `F64` (or `I64` widened to `f64`) at physical row `phys_row`.
pub(crate) fn raw_f64_at(col: &Column, phys_row: usize) -> Result<Option<f64>, RunError> {
    check_bounds(col, phys_row)?;
    match col.data_type() {
        DataType::Null => return Ok(None),
        DataType::F64 | DataType::I64 => {}
        other => {
            return Err(internal(format!(
                "aggregate value has type {}",
                other.display_name()
            )));
        }
    }
    if col.is_null(phys_row) {
        return Ok(None);
    }
    if let Column::Const { value, .. } = col {
        return match value {
            ScalarValue::F64(v) => Ok(Some(*v)),
            ScalarValue::I64(v) => Ok(Some(*v as f64)),
            ScalarValue::Null(_) => Ok(None),
            _ => Err(internal("aggregate value scalar mismatch")),
        };
    }
    let Some((fc, flat_row)) = resolve_flat(col, phys_row) else {
        return Ok(None);
    };
    match fc.data() {
        FlatData::F64(values) => Ok(Some(values[flat_row])),
        FlatData::I64(values) => Ok(Some(values[flat_row] as f64)),
        _ => Err(internal("aggregate value flat storage mismatch")),
    }
}

/// Reads a required `TimestampNs` at logical `active_row` in `batch`.
#[cfg(test)]
pub(crate) fn time_at(batch: &Batch, col: &Column, active_row: usize) -> Result<i64, RunError> {
    let phys = physical_row(batch, active_row)?;
    raw_time_at(col, phys)
}

/// Reads a required `TimestampNs` at physical row `phys_row`.
pub(crate) fn raw_time_at(col: &Column, phys_row: usize) -> Result<i64, RunError> {
    check_bounds(col, phys_row)?;
    if !matches!(col.data_type(), DataType::TimestampNs { .. }) {
        return Err(internal(format!(
            "bucket has type {}",
            col.data_type().display_name()
        )));
    }
    if col.is_null(phys_row) {
        return Err(internal("bucket is null"));
    }
    if let Column::Const { value, .. } = col {
        return match value {
            ScalarValue::TimestampNs { value, .. } => Ok(*value),
            _ => Err(internal("timestamp scalar mismatch")),
        };
    }
    let Some((fc, flat_row)) = resolve_flat(col, phys_row) else {
        return Err(internal("bucket is null"));
    };
    match fc.data() {
        FlatData::TimestampNs { values, .. } => Ok(values[flat_row]),
        _ => Err(internal("timestamp flat storage mismatch")),
    }
}

/// Reads an optional `Utf8` string at logical `active_row` in `batch`.
pub(crate) fn string_at(
    batch: &Batch,
    col: &Column,
    active_row: usize,
) -> Result<Option<String>, RunError> {
    let phys = physical_row(batch, active_row)?;
    raw_string_at(col, phys)
}

/// Reads an optional `Utf8` string at physical row `phys_row`.
pub(crate) fn raw_string_at(col: &Column, phys_row: usize) -> Result<Option<String>, RunError> {
    check_bounds(col, phys_row)?;
    match col.data_type() {
        DataType::Null => return Ok(None),
        DataType::Utf8 => {}
        other => {
            return Err(internal(format!(
                "group key has type {}",
                other.display_name()
            )));
        }
    }
    if col.is_null(phys_row) {
        return Ok(None);
    }
    if let Column::Const { value, .. } = col {
        return match value {
            ScalarValue::Utf8(s) => Ok(Some(s.clone())),
            ScalarValue::Null(_) => Ok(None),
            _ => Err(internal("string scalar mismatch")),
        };
    }
    let Some((fc, flat_row)) = resolve_flat(col, phys_row) else {
        return Ok(None);
    };
    match fc.data() {
        FlatData::Utf8(utf8) => utf8
            .get(flat_row)
            .map(|s| Some(s.to_owned()))
            .ok_or_else(|| internal("utf8 offset out of bounds")),
        _ => Err(internal("utf8 flat storage mismatch")),
    }
}

/// Reads an optional `Bool` at logical `active_row` in `batch`.
#[cfg(test)]
pub(crate) fn bool_at(
    batch: &Batch,
    col: &Column,
    active_row: usize,
) -> Result<Option<bool>, RunError> {
    let phys = physical_row(batch, active_row)?;
    raw_bool_at(col, phys)
}

/// Reads an optional `Bool` at physical row `phys_row`.
pub(crate) fn raw_bool_at(col: &Column, phys_row: usize) -> Result<Option<bool>, RunError> {
    check_bounds(col, phys_row)?;
    match col.data_type() {
        DataType::Null => return Ok(None),
        DataType::Bool => {}
        other => {
            return Err(internal(format!(
                "boolean column has type {}",
                other.display_name()
            )));
        }
    }
    if col.is_null(phys_row) {
        return Ok(None);
    }
    if let Column::Const { value, .. } = col {
        return match value {
            ScalarValue::Bool(b) => Ok(Some(*b)),
            ScalarValue::Null(_) => Ok(None),
            _ => Err(internal("bool scalar mismatch")),
        };
    }
    let Some((fc, flat_row)) = resolve_flat(col, phys_row) else {
        return Ok(None);
    };
    match fc.data() {
        FlatData::Bool(bm) => Ok(Some(bm.get(flat_row))),
        _ => Err(internal("bool flat storage mismatch")),
    }
}

/// Reads an optional `Decimal128` (or `F64`) cost value converted to finite `f64`
/// at logical `active_row` in `batch`. Returns `RunError::Failed` on non-finite overflow.
pub(crate) fn decimal_to_f64_at(
    batch: &Batch,
    col: &Column,
    active_row: usize,
) -> Result<Option<f64>, RunError> {
    let phys = physical_row(batch, active_row)?;
    raw_decimal_to_f64_at(col, phys)
}

/// Reads an optional `Decimal128` (or `F64`) cost value converted to finite `f64`
/// at physical row `phys_row`. Returns `RunError::Failed` on non-finite overflow.
pub(crate) fn raw_decimal_to_f64_at(
    col: &Column,
    phys_row: usize,
) -> Result<Option<f64>, RunError> {
    check_bounds(col, phys_row)?;
    match col.data_type() {
        DataType::Null => return Ok(None),
        DataType::Decimal128 { .. } | DataType::F64 => {}
        other => {
            return Err(internal(format!(
                "cost column has type {}",
                other.display_name()
            )));
        }
    }
    if col.is_null(phys_row) {
        return Ok(None);
    }
    if let Column::Const { value, .. } = col {
        return match value {
            ScalarValue::Decimal128 { value, scale, .. } => {
                convert_decimal(*value, *scale).map(Some)
            }
            ScalarValue::F64(v) => check_finite(*v).map(Some),
            ScalarValue::Null(_) => Ok(None),
            _ => Err(internal("cost scalar mismatch")),
        };
    }
    let Some((fc, flat_row)) = resolve_flat(col, phys_row) else {
        return Ok(None);
    };
    match fc.data() {
        FlatData::Decimal128 { values, scale, .. } => {
            convert_decimal(values[flat_row], *scale).map(Some)
        }
        FlatData::F64(values) => check_finite(values[flat_row]).map(Some),
        _ => Err(internal("cost flat storage mismatch")),
    }
}

fn convert_decimal(value: i128, scale: i8) -> Result<f64, RunError> {
    let out = if scale >= 0 {
        (value as f64) / 10f64.powi(i32::from(scale))
    } else {
        (value as f64) * 10f64.powi(-i32::from(scale))
    };
    check_finite(out)
}

fn check_finite(value: f64) -> Result<f64, RunError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(RunError::Failed("cost value is not finite".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use cetra_column::{
        Bitmap, Column, DictColumn, Field, FlatColumn, FlatData, RleColumn, ScalarValue, Schema,
        Selection, Utf8Data,
    };

    use super::*;

    fn test_batch(col: Column, name: &str, nullable: bool, selection: Selection) -> Batch {
        let len = col.len();
        let schema =
            Schema::try_new_arc(vec![Field::new(name, col.data_type(), nullable)]).expect("schema");
        Batch::try_new(schema, vec![col], len, selection).expect("batch")
    }

    #[test]
    fn reads_across_all_four_encodings_and_selection() {
        let mut bm = Bitmap::new_zeroed(4);
        let _ = bm.set(1, true);
        let _ = bm.set(3, true);
        let sel = Selection::Mask(bm);

        // Flat I64
        let flat = Column::Flat(
            FlatColumn::try_new(FlatData::I64(vec![10, 20, 30, 40]), None).expect("flat"),
        );
        let batch = test_batch(flat, "n", false, sel.clone());
        let col = require_col(&batch, "n").expect("col");
        assert_eq!(i64_at(&batch, col, 0).expect("row 0"), 20);
        assert_eq!(i64_at(&batch, col, 1).expect("row 1"), 40);

        // Const Decimal128(38, 18)
        let const_dec = Column::try_new_const(
            ScalarValue::Decimal128 {
                value: 250_000_000_000_000_000,
                precision: 38,
                scale: 18,
            },
            4,
        )
        .expect("const");
        let batch = test_batch(const_dec, "cost", true, sel.clone());
        let col = require_col(&batch, "cost").expect("col");
        let val = decimal_to_f64_at(&batch, col, 0)
            .expect("dec")
            .expect("some");
        assert!((val - 0.25).abs() < 1e-12);

        // Dict Utf8
        let dict_vals = FlatColumn::try_new(
            FlatData::Utf8(Utf8Data::try_from_slices(["a", "b"]).expect("utf8")),
            None,
        )
        .expect("vals");
        let mut dict_nulls = Bitmap::new_ones(4);
        let _ = dict_nulls.set(3, false);
        let dict = Column::Dict(
            DictColumn::try_new(dict_vals, vec![0, 1, 0, 0], Some(dict_nulls)).expect("dict"),
        );
        let batch = test_batch(dict, "s", true, Selection::Indices(vec![3, 1]));
        let col = require_col(&batch, "s").expect("col");
        assert_eq!(string_at(&batch, col, 0).expect("row 0"), None);
        assert_eq!(
            string_at(&batch, col, 1).expect("row 1").as_deref(),
            Some("b")
        );

        // Rle Bool
        let rle_vals = FlatColumn::try_new(
            FlatData::Bool({
                let mut b = Bitmap::new_zeroed(2);
                let _ = b.set(0, true);
                b
            }),
            None,
        )
        .expect("rle vals");
        let rle = Column::Rle(RleColumn::try_new(rle_vals, vec![2, 4]).expect("rle"));
        let batch = test_batch(rle, "b", false, sel);
        let col = require_col(&batch, "b").expect("col");
        assert_eq!(bool_at(&batch, col, 0).expect("row 0"), Some(true));
        assert_eq!(bool_at(&batch, col, 1).expect("row 1"), Some(false));

        // Rle TimestampNs
        let ts_vals = FlatColumn::try_new(
            FlatData::TimestampNs {
                values: vec![1_000, 2_000],
                utc: true,
            },
            None,
        )
        .expect("ts vals");
        let ts_rle = Column::Rle(RleColumn::try_new(ts_vals, vec![1, 4]).expect("ts rle"));
        let batch = test_batch(ts_rle, "ts", false, Selection::Indices(vec![0, 2]));
        let col = require_col(&batch, "ts").expect("col");
        assert_eq!(time_at(&batch, col, 0).expect("row 0"), 1_000);
        assert_eq!(time_at(&batch, col, 1).expect("row 1"), 2_000);
        assert_eq!(
            active_rows(&batch).collect::<Vec<_>>(),
            vec![0usize, 2usize]
        );
    }

    #[test]
    fn missing_column_and_null_count_error() {
        let flat = Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1]), None).expect("flat"));
        let batch = test_batch(flat, "x", false, Selection::All);
        assert!(matches!(
            require_col(&batch, "missing"),
            Err(RunError::Internal(_))
        ));

        let null_flat = Column::Flat(
            FlatColumn::try_new(FlatData::I64(vec![0]), Some(Bitmap::new_zeroed(1))).expect("flat"),
        );
        let batch = test_batch(null_flat, "cnt", true, Selection::All);
        let col = require_col(&batch, "cnt").expect("col");
        assert!(matches!(i64_at(&batch, col, 0), Err(RunError::Internal(_))));
    }
}
