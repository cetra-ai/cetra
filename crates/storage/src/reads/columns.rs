//! Typed readers for the single-row aggregate batches that dashboard, alert and facet
//! queries read.

use arrow::array::{Array, AsArray};
use arrow::datatypes::{Float64Type, Int64Type, TimestampNanosecondType, UInt64Type};
use arrow::record_batch::RecordBatch;
use datafusion::error::DataFusionError;
use datafusion::functions_aggregate::expr_fn::approx_percentile_cont;
use datafusion::logical_expr::{Expr, SortExpr, col, lit};

use crate::query_error::RunError;

pub(super) fn percentile(column: &str, quantile: f64) -> Expr {
    percentile_of(col(column), quantile)
}

/// `approx_percentile_cont` of any value expression, such as a `CASE` over a column.
pub(super) fn percentile_of(value: Expr, quantile: f64) -> Expr {
    approx_percentile_cont(SortExpr::new(value, true, true), lit(quantile), None)
}

pub(super) fn require<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a dyn Array, RunError> {
    match batch.column_by_name(name) {
        Some(column) => Ok(column.as_ref()),
        None => Err(missing_column(batch, name)),
    }
}

fn missing_column(batch: &RecordBatch, name: &str) -> RunError {
    let schema = batch.schema();
    let columns: Vec<&str> = schema
        .fields()
        .iter()
        .map(|field| field.name().as_str())
        .collect();
    internal(format!("missing column {name}; columns {columns:?}"))
}

pub(super) fn i64_at(column: &dyn Array, index: usize) -> Result<i64, RunError> {
    if let Some(values) = column.as_primitive_opt::<Int64Type>() {
        if values.is_null(index) {
            return Err(internal("aggregate count is null".to_owned()));
        }
        return Ok(values.value(index));
    }
    if let Some(values) = column.as_primitive_opt::<UInt64Type>() {
        if values.is_null(index) {
            return Err(internal("aggregate count is null".to_owned()));
        }
        return i64::try_from(values.value(index))
            .map_err(|_| internal("aggregate count overflows i64".to_owned()));
    }
    Err(internal(format!(
        "aggregate count has type {}",
        column.data_type()
    )))
}

pub(super) fn f64_at(column: &dyn Array, index: usize) -> Result<Option<f64>, RunError> {
    if let Some(values) = column.as_primitive_opt::<Float64Type>() {
        return Ok((!values.is_null(index)).then(|| values.value(index)));
    }
    if let Some(values) = column.as_primitive_opt::<Int64Type>() {
        return Ok((!values.is_null(index)).then(|| values.value(index) as f64));
    }
    Err(internal(format!(
        "aggregate value has type {}",
        column.data_type()
    )))
}

pub(super) fn internal(message: String) -> RunError {
    RunError::Internal(DataFusionError::Internal(message))
}

pub(super) fn time_at(column: &dyn Array, index: usize) -> Result<i64, RunError> {
    let Some(values) = column.as_primitive_opt::<TimestampNanosecondType>() else {
        return Err(internal(format!("bucket has type {}", column.data_type())));
    };
    if values.is_null(index) {
        return Err(internal("bucket is null".to_owned()));
    }
    Ok(values.value(index))
}

pub(super) fn string_at(column: &dyn Array, index: usize) -> Result<Option<String>, RunError> {
    if column.is_null(index) {
        return Ok(None);
    }
    if let Some(values) = column.as_string_opt::<i32>() {
        return Ok(Some(values.value(index).to_owned()));
    }
    if let Some(values) = column.as_string_opt::<i64>() {
        return Ok(Some(values.value(index).to_owned()));
    }
    if let Some(values) = column.as_string_view_opt() {
        return Ok(Some(values.value(index).to_owned()));
    }
    Err(internal(format!(
        "group key has type {}",
        column.data_type()
    )))
}
