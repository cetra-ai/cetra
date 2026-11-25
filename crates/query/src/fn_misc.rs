//! Math, null-handling, array, list-concat, map, and JSON scalar functions.

use std::cmp::Ordering;
use std::collections::HashSet;

use cetra_column::{
    Bitmap, Column, ColumnError, DataType, FlatColumn, FlatData, Interval, Reservation,
    ScalarValue, Utf8Data,
};
use serde_json::Value as JsonValue;
use serde_json_path::JsonPath;

use crate::error::QueryError;
use crate::eval::{
    ElemRef, ListRef, MapRef, bitmap_mem_bytes, build_flat_from_elem_refs, build_flat_from_sources,
    check_cancel, checked_fixed_bytes, checked_varlen_bytes, compare_decimals, f64_canonical_bits,
    f64_cmp, format_decimal, is_active, list_ref_at, list_ref_elem, list_ref_len, map_ref_at,
    pool_size, read_bool, read_date, read_decimal, read_f64, read_i64, read_interval, read_scalar,
    read_str, read_timestamp, reconcile_output, rollback_to, try_reserve,
};
use crate::fn_time::{format_date_days, format_timestamp_ns};
use crate::ir::{EvalContext, ScalarFn};

const CANCEL_CHECK_MASK: usize = 1023;

/// Concatenates two list columns element-wise (`||` on `List(T)`).
///
/// Follows DataFusion `array_concat` null semantics:
/// `NULL || [1] -> [1]`, `[1] || NULL -> [1]`, `NULL || NULL -> NULL`.
/// Inactive rows under `active` are skipped and filled with nulls.
pub fn eval_list_concat(
    left: &Column,
    right: &Column,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let len = left.len();
    if right.len() != len {
        return Err(ColumnError::InvalidBatch {
            reason: "column length mismatch",
        }
        .into());
    }
    validate_active_len(active, len)?;

    let before = pool_size(&pool);
    let res = eval_list_concat_inner(left, right, len, active, ctx, pool.as_deref_mut(), before);
    finish_with_pool(res, &mut pool, before)
}

/// Evaluates a curated math, null-handling, array, map, or JSON [`ScalarFn`].
///
/// Inactive rows under `active` are skipped without faulting and filled with nulls.
/// Enforces the unified `Reservation` postcondition (`before + col.memory_bytes()` on `Ok`,
/// rolled back to `before` on `Err`).
pub fn eval_misc_fn(
    func: ScalarFn,
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    for arg in args {
        if arg.len() != len {
            return Err(ColumnError::InvalidBatch {
                reason: "column length mismatch",
            }
            .into());
        }
    }
    validate_active_len(active, len)?;

    let before = pool_size(&pool);
    let res = eval_misc_fn_inner(func, args, len, active, ctx, pool.as_deref_mut(), before);
    finish_with_pool(res, &mut pool, before)
}

fn finish_with_pool(
    res: Result<Column, QueryError>,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    match res {
        Ok(col) => {
            reconcile_output(pool, before, col.memory_bytes())?;
            Ok(col)
        }
        Err(err) => {
            rollback_to(pool, before);
            Err(err)
        }
    }
}

fn validate_active_len(active: Option<&Bitmap>, len: usize) -> Result<(), QueryError> {
    if let Some(bm) = active
        && bm.len() != len
    {
        return Err(ColumnError::InvalidBatch {
            reason: "active mask length mismatch",
        }
        .into());
    }
    Ok(())
}

fn checked_bool_bytes(len: usize) -> Result<usize, QueryError> {
    bitmap_mem_bytes(len)
        .checked_mul(2)
        .ok_or(QueryError::TooLarge)
}

fn null_const(dt: DataType, len: usize) -> Column {
    Column::Const {
        value: ScalarValue::Null(dt),
        len,
    }
}

fn all_inactive(active: Option<&Bitmap>) -> bool {
    active.is_some_and(Bitmap::all_zeros)
}

fn eval_misc_fn_inner(
    func: ScalarFn,
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    check_cancel(ctx, &mut pool, before)?;

    match func {
        ScalarFn::Abs => eval_abs(args, len, active, ctx, &mut pool, before),
        ScalarFn::Round => eval_round(args, len, active, ctx, &mut pool, before),
        ScalarFn::Floor | ScalarFn::Ceil => {
            eval_floor_ceil(func, args, len, active, ctx, &mut pool, before)
        }
        ScalarFn::Sqrt | ScalarFn::Ln | ScalarFn::Log10 => {
            eval_unary_f64_math(func, args, len, active, ctx, &mut pool, before)
        }
        ScalarFn::Power => eval_power(args, len, active, ctx, &mut pool, before),
        ScalarFn::IsNan | ScalarFn::IsFinite => {
            eval_float_pred(func, args, len, active, ctx, &mut pool, before)
        }
        ScalarFn::Coalesce => eval_coalesce(args, len, active, ctx, &mut pool, before),
        ScalarFn::Nullif => eval_nullif(args, len, active, ctx, &mut pool, before),
        ScalarFn::Greatest | ScalarFn::Least => {
            eval_extremum(func, args, len, active, ctx, &mut pool, before)
        }
        ScalarFn::ArrayHas => eval_array_has(args, len, active, ctx, &mut pool, before),
        ScalarFn::ArrayHasAll => eval_array_has_all(args, len, active, ctx, &mut pool, before),
        ScalarFn::ArrayLength => eval_array_length(args, len, active, ctx, &mut pool, before),
        ScalarFn::ArrayElement => eval_array_element(args, len, active, ctx, &mut pool, before),
        ScalarFn::ArrayToString => eval_array_to_string(args, len, active, ctx, &mut pool, before),
        ScalarFn::ArrayDistinct => eval_array_distinct(args, len, active, ctx, &mut pool, before),
        ScalarFn::MapKeys => eval_map_extract(true, args, len, active, ctx, &mut pool, before),
        ScalarFn::MapValues => eval_map_extract(false, args, len, active, ctx, &mut pool, before),
        ScalarFn::JsonGet => eval_json_get(false, args, len, active, ctx, &mut pool, before),
        ScalarFn::JsonGetText => eval_json_get(true, args, len, active, ctx, &mut pool, before),
        ScalarFn::JsonExtract => {
            eval_json_extract(false, args, len, active, ctx, &mut pool, before)
        }
        ScalarFn::JsonExtractText => {
            eval_json_extract(true, args, len, active, ctx, &mut pool, before)
        }
        _ => Err(QueryError::failed(format!(
            "unsupported misc scalar function: {}",
            func.name()
        ))),
    }
}

// ---------------------------------------------------------------------------
// List concatenation (`||` on List(T))
// ---------------------------------------------------------------------------

fn resolve_list_concat_item_type(
    left: &DataType,
    right: &DataType,
) -> Result<DataType, QueryError> {
    match (left, right) {
        (DataType::List(lt), DataType::List(rt)) => {
            if lt == rt || matches!(rt.as_ref(), DataType::Null) {
                Ok((**lt).clone())
            } else if matches!(lt.as_ref(), DataType::Null) {
                Ok((**rt).clone())
            } else {
                Err(QueryError::failed(
                    "list concatenation requires matching element types",
                ))
            }
        }
        (DataType::List(lt), DataType::Null) => Ok((**lt).clone()),
        (DataType::Null, DataType::List(rt)) => Ok((**rt).clone()),
        (DataType::Null, DataType::Null) => Ok(DataType::Null),
        _ => Err(QueryError::failed(
            "list concatenation requires list operands",
        )),
    }
}

fn eval_list_concat_inner(
    left: &Column,
    right: &Column,
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    check_cancel(ctx, &mut pool, before)?;
    let ldt = left.data_type();
    let rdt = right.data_type();
    let item_type = resolve_list_concat_item_type(&ldt, &rdt)?;
    let out_dt = DataType::List(Box::new(item_type.clone()));

    if all_inactive(active) || (matches!(ldt, DataType::Null) && matches!(rdt, DataType::Null)) {
        return Ok(null_const(out_dt, len));
    }

    let mut total_elems = 0usize;
    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, &mut pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        let l_len = list_ref_at(left, row).map_or(0, list_ref_len);
        let r_len = list_ref_at(right, row).map_or(0, list_ref_len);
        total_elems = total_elems
            .checked_add(l_len)
            .and_then(|n| n.checked_add(r_len))
            .ok_or(ColumnError::Overflow {
                reason: "list concat element count overflow",
            })?;
    }
    let _ = u32::try_from(total_elems).map_err(|_| ColumnError::Overflow {
        reason: "list concat element count exceeds u32::MAX",
    })?;

    let header_bytes = checked_varlen_bytes(len, 0, true)?;
    let scratch_bytes = checked_fixed_bytes(total_elems, size_of::<ElemRef<'_>>(), false)?;
    try_reserve(
        &mut pool,
        header_bytes
            .checked_add(scratch_bytes)
            .ok_or(QueryError::TooLarge)?,
    )?;

    let mut offsets = Vec::with_capacity(len + 1);
    offsets.push(0u32);
    let mut nulls = Bitmap::new_zeroed(len);
    let mut elem_refs: Vec<ElemRef<'_>> = Vec::with_capacity(total_elems);

    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, &mut pool, before)?;
        }
        if is_active(active, row) {
            let lref = list_ref_at(left, row);
            let rref = list_ref_at(right, row);
            if lref.is_some() || rref.is_some() {
                let _ = nulls.set(row, true);
                if let Some(lr) = lref {
                    push_list_ref_elems(lr, &mut elem_refs);
                }
                if let Some(rr) = rref {
                    push_list_ref_elems(rr, &mut elem_refs);
                }
            }
        }
        offsets.push(elem_refs.len() as u32);
    }

    let values = build_flat_from_elem_refs(&item_type, &elem_refs, &mut pool, ctx, before)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::List {
            item_type,
            offsets,
            values: Box::new(values),
        },
        Some(nulls),
    )?))
}

// ---------------------------------------------------------------------------
// Math functions
// ---------------------------------------------------------------------------

fn expect_arity(func: ScalarFn, args: &[Column], expected: usize) -> Result<(), QueryError> {
    if args.len() != expected {
        return Err(QueryError::failed(format!(
            "{} expects {expected} arguments, got {}",
            func.name(),
            args.len()
        )));
    }
    Ok(())
}

fn eval_abs(
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    expect_arity(ScalarFn::Abs, args, 1)?;
    let arg = &args[0];
    let dt = arg.data_type();
    if all_inactive(active) || matches!(dt, DataType::Null) {
        return Ok(null_const(dt, len));
    }
    match dt {
        DataType::I64 => {
            try_reserve(pool, checked_fixed_bytes(len, 8, true)?)?;
            let mut vals = vec![0i64; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if is_active(active, row)
                    && let Some(v) = read_i64(arg, row)
                {
                    *slot = v
                        .checked_abs()
                        .ok_or_else(|| QueryError::failed("integer abs overflow"))?;
                    let _ = nulls.set(row, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::I64(vals),
                Some(nulls),
            )?))
        }
        DataType::F64 => {
            try_reserve(pool, checked_fixed_bytes(len, 8, true)?)?;
            let mut vals = vec![0.0f64; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if is_active(active, row)
                    && let Some(v) = read_f64(arg, row)
                {
                    *slot = v.abs();
                    let _ = nulls.set(row, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::F64(vals),
                Some(nulls),
            )?))
        }
        DataType::Decimal128 { precision, scale } => {
            try_reserve(pool, checked_fixed_bytes(len, 16, true)?)?;
            let mut vals = vec![0i128; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if is_active(active, row)
                    && let Some(v) = read_decimal(arg, row)
                {
                    *slot = v
                        .checked_abs()
                        .ok_or_else(|| QueryError::failed("decimal abs overflow"))?;
                    let _ = nulls.set(row, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vals,
                    precision,
                    scale,
                },
                Some(nulls),
            )?))
        }
        _ => Err(QueryError::failed("abs requires numeric argument")),
    }
}

fn eval_round(
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    if args.is_empty() || args.len() > 2 {
        return Err(QueryError::failed("round expects 1 or 2 arguments"));
    }
    let val_col = &args[0];
    let dp_col = args.get(1);
    let val_dt = val_col.data_type();
    if let Some(dp) = dp_col
        && !matches!(dp.data_type(), DataType::I64 | DataType::Null)
    {
        return Err(QueryError::failed("round decimal places must be integer"));
    }
    if all_inactive(active)
        || matches!(val_dt, DataType::Null)
        || dp_col.is_some_and(|c| matches!(c.data_type(), DataType::Null))
    {
        return Ok(null_const(val_dt, len));
    }

    match val_dt {
        DataType::F64 => {
            try_reserve(pool, checked_fixed_bytes(len, 8, true)?)?;
            let mut vals = vec![0.0f64; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if !is_active(active, row) {
                    continue;
                }
                let Some(v) = read_f64(val_col, row) else {
                    continue;
                };
                let dp = match dp_col {
                    None => 0i64,
                    Some(dc) => match read_i64(dc, row) {
                        Some(d) => d,
                        None => continue,
                    },
                };
                *slot = round_f64(v, dp);
                let _ = nulls.set(row, true);
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::F64(vals),
                Some(nulls),
            )?))
        }
        DataType::I64 => {
            try_reserve(pool, checked_fixed_bytes(len, 8, true)?)?;
            let mut vals = vec![0i64; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if !is_active(active, row) {
                    continue;
                }
                let Some(v) = read_i64(val_col, row) else {
                    continue;
                };
                let dp = match dp_col {
                    None => 0i64,
                    Some(dc) => match read_i64(dc, row) {
                        Some(d) => d,
                        None => continue,
                    },
                };
                *slot = round_i64(v, dp)?;
                let _ = nulls.set(row, true);
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::I64(vals),
                Some(nulls),
            )?))
        }
        _ => Err(QueryError::failed(
            "round requires float or integer argument",
        )),
    }
}

fn round_f64(x: f64, dp: i64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    if dp == 0 {
        return x.round();
    }
    if dp > 0 {
        // Any |x| >= 2^53 already has zero fractional part in IEEE 754 f64.
        if x.abs() >= 9_007_199_254_740_992.0 || x == 0.0 {
            return x;
        }
        if dp <= 308 {
            let factor = 10f64.powi(dp as i32);
            let scaled = x * factor;
            if scaled.is_finite() {
                return scaled.round() / factor;
            }
            return x;
        }
        if dp <= 508 && x.abs() < 1.0 {
            let f1 = 1e200f64;
            let f2 = 10f64.powi((dp - 200) as i32);
            let scaled = (x * f1) * f2;
            if scaled.is_finite() {
                return (scaled.round() / f2) / f1;
            }
        }
        return x;
    }
    // Negative dp: round to nearest 10^(-dp) half-away-from-zero.
    let abs_dp = dp.unsigned_abs();
    if abs_dp > 308 {
        return if x.is_sign_negative() { -0.0 } else { 0.0 };
    }
    let factor = 10f64.powi(abs_dp as i32);
    (x / factor).round() * factor
}

fn round_i64(v: i64, dp: i64) -> Result<i64, QueryError> {
    if dp >= 0 {
        return Ok(v);
    }
    let abs_dp = dp.unsigned_abs();
    if abs_dp >= 20 {
        return Ok(0);
    }
    let factor = 10i128.pow(abs_dp as u32);
    let iv = i128::from(v);
    let rem = iv % factor;
    let half = factor / 2;
    let rounded = if rem.abs() >= half {
        if iv >= 0 {
            iv - rem + factor
        } else {
            iv - rem - factor
        }
    } else {
        iv - rem
    };
    i64::try_from(rounded).map_err(|_| QueryError::failed("integer round overflow"))
}

fn eval_floor_ceil(
    func: ScalarFn,
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    expect_arity(func, args, 1)?;
    let arg = &args[0];
    let dt = arg.data_type();
    if all_inactive(active) || matches!(dt, DataType::Null) {
        return Ok(null_const(dt, len));
    }
    match dt {
        DataType::F64 => {
            try_reserve(pool, checked_fixed_bytes(len, 8, true)?)?;
            let mut vals = vec![0.0f64; len];
            let mut nulls = Bitmap::new_zeroed(len);
            let is_floor = matches!(func, ScalarFn::Floor);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if is_active(active, row)
                    && let Some(v) = read_f64(arg, row)
                {
                    *slot = if is_floor { v.floor() } else { v.ceil() };
                    let _ = nulls.set(row, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::F64(vals),
                Some(nulls),
            )?))
        }
        DataType::I64 => {
            try_reserve(pool, checked_fixed_bytes(len, 8, true)?)?;
            let mut vals = vec![0i64; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if is_active(active, row)
                    && let Some(v) = read_i64(arg, row)
                {
                    *slot = v;
                    let _ = nulls.set(row, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::I64(vals),
                Some(nulls),
            )?))
        }
        _ => Err(QueryError::failed(format!(
            "{} requires float or integer argument",
            func.name()
        ))),
    }
}

fn read_as_f64(col: &Column, row: usize) -> Result<Option<f64>, QueryError> {
    match col.data_type() {
        DataType::F64 => Ok(read_f64(col, row)),
        DataType::I64 => Ok(read_i64(col, row).map(|v| v as f64)),
        DataType::Null => Ok(None),
        _ => Err(QueryError::failed("expected numeric argument")),
    }
}

fn eval_unary_f64_math(
    func: ScalarFn,
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    expect_arity(func, args, 1)?;
    let arg = &args[0];
    let dt = arg.data_type();
    if !matches!(dt, DataType::F64 | DataType::I64 | DataType::Null) {
        return Err(QueryError::failed(format!(
            "{} requires numeric argument",
            func.name()
        )));
    }
    if all_inactive(active) || matches!(dt, DataType::Null) {
        return Ok(null_const(DataType::F64, len));
    }

    try_reserve(pool, checked_fixed_bytes(len, 8, true)?)?;
    let mut vals = vec![0.0f64; len];
    let mut nulls = Bitmap::new_zeroed(len);
    for (row, slot) in vals.iter_mut().enumerate() {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row)
            && let Some(v) = read_as_f64(arg, row)?
        {
            *slot = match func {
                ScalarFn::Sqrt => v.sqrt(),
                ScalarFn::Ln => v.ln(),
                ScalarFn::Log10 => v.log10(),
                _ => v,
            };
            let _ = nulls.set(row, true);
        }
    }
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::F64(vals),
        Some(nulls),
    )?))
}

fn eval_power(
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    expect_arity(ScalarFn::Power, args, 2)?;
    let base_col = &args[0];
    let exp_col = &args[1];
    if !matches!(
        base_col.data_type(),
        DataType::F64 | DataType::I64 | DataType::Null
    ) || !matches!(
        exp_col.data_type(),
        DataType::F64 | DataType::I64 | DataType::Null
    ) {
        return Err(QueryError::failed("power requires numeric arguments"));
    }
    if all_inactive(active)
        || matches!(base_col.data_type(), DataType::Null)
        || matches!(exp_col.data_type(), DataType::Null)
    {
        return Ok(null_const(DataType::F64, len));
    }

    try_reserve(pool, checked_fixed_bytes(len, 8, true)?)?;
    let mut vals = vec![0.0f64; len];
    let mut nulls = Bitmap::new_zeroed(len);
    for (row, slot) in vals.iter_mut().enumerate() {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row)
            && let (Some(b), Some(e)) = (read_as_f64(base_col, row)?, read_as_f64(exp_col, row)?)
        {
            *slot = b.powf(e);
            let _ = nulls.set(row, true);
        }
    }
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::F64(vals),
        Some(nulls),
    )?))
}

fn eval_float_pred(
    func: ScalarFn,
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    expect_arity(func, args, 1)?;
    let arg = &args[0];
    if !matches!(
        arg.data_type(),
        DataType::F64 | DataType::I64 | DataType::Null
    ) {
        return Err(QueryError::failed(format!(
            "{} requires float argument",
            func.name()
        )));
    }
    if all_inactive(active) || matches!(arg.data_type(), DataType::Null) {
        return Ok(null_const(DataType::Bool, len));
    }

    try_reserve(pool, checked_bool_bytes(len)?)?;
    let mut vals = Bitmap::new_zeroed(len);
    let mut nulls = Bitmap::new_zeroed(len);
    let check_nan = matches!(func, ScalarFn::IsNan);

    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row)
            && let Some(v) = read_as_f64(arg, row)?
        {
            let _ = nulls.set(row, true);
            let res = if check_nan { v.is_nan() } else { v.is_finite() };
            if res {
                let _ = vals.set(row, true);
            }
        }
    }
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Bool(vals),
        Some(nulls),
    )?))
}

// ---------------------------------------------------------------------------
// Null helpers: Coalesce, Nullif, Greatest, Least
// ---------------------------------------------------------------------------

fn unify_common_type(func: ScalarFn, args: &[Column]) -> Result<DataType, QueryError> {
    let mut resolved: Option<DataType> = None;
    for arg in args {
        let dt = arg.data_type();
        if matches!(dt, DataType::Null) {
            continue;
        }
        match &mut resolved {
            None => resolved = Some(dt),
            Some(cur) => {
                if *cur == dt {
                    continue;
                }
                match (cur.clone(), dt) {
                    (DataType::TimestampNs { utc: u1 }, DataType::TimestampNs { utc: u2 }) => {
                        *cur = DataType::TimestampNs { utc: u1 || u2 };
                    }
                    (DataType::List(a), DataType::List(b)) => {
                        if matches!(a.as_ref(), DataType::Null) {
                            *cur = DataType::List(b);
                        } else if !matches!(b.as_ref(), DataType::Null) && a != b {
                            return Err(QueryError::failed(format!(
                                "{} arguments have incompatible list types",
                                func.name()
                            )));
                        }
                    }
                    _ => {
                        return Err(QueryError::failed(format!(
                            "{} arguments must share a common type",
                            func.name()
                        )));
                    }
                }
            }
        }
    }
    Ok(resolved.unwrap_or(DataType::Null))
}

fn eval_coalesce(
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    if args.is_empty() {
        return Err(QueryError::failed("coalesce expects at least 1 argument"));
    }
    let out_dt = unify_common_type(ScalarFn::Coalesce, args)?;
    if all_inactive(active) || matches!(out_dt, DataType::Null) {
        return Ok(null_const(out_dt, len));
    }

    try_reserve(
        pool,
        checked_fixed_bytes(len, size_of::<Option<(&Column, usize)>>(), false)?,
    )?;
    let mut sources: Vec<Option<(&Column, usize)>> = Vec::with_capacity(len);
    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            sources.push(None);
            continue;
        }
        let found = args.iter().find(|c| !c.is_null(row)).map(|c| (c, row));
        sources.push(found);
    }
    build_flat_from_sources(&out_dt, &sources, pool, ctx, before)
}

fn eval_nullif(
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    expect_arity(ScalarFn::Nullif, args, 2)?;
    let a = &args[0];
    let b = &args[1];
    let out_dt = unify_common_type(ScalarFn::Nullif, args)?;
    let a_dt = a.data_type();
    if all_inactive(active) || matches!(a_dt, DataType::Null) {
        return Ok(null_const(
            if matches!(a_dt, DataType::Null) {
                out_dt
            } else {
                a_dt
            },
            len,
        ));
    }

    try_reserve(
        pool,
        checked_fixed_bytes(len, size_of::<Option<(&Column, usize)>>(), false)?,
    )?;
    let mut sources: Vec<Option<(&Column, usize)>> = Vec::with_capacity(len);
    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) || a.is_null(row) {
            sources.push(None);
            continue;
        }
        if !b.is_null(row) && compare_cols_at(a, row, b, row)? == Ordering::Equal {
            sources.push(None);
        } else {
            sources.push(Some((a, row)));
        }
    }
    build_flat_from_sources(&a_dt, &sources, pool, ctx, before)
}

fn eval_extremum(
    func: ScalarFn,
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    if args.is_empty() {
        return Err(QueryError::failed(format!(
            "{} expects at least 1 argument",
            func.name()
        )));
    }
    let out_dt = unify_common_type(func, args)?;
    if all_inactive(active) || matches!(out_dt, DataType::Null) {
        return Ok(null_const(out_dt, len));
    }

    let want_ord = if matches!(func, ScalarFn::Greatest) {
        Ordering::Greater
    } else {
        Ordering::Less
    };

    try_reserve(
        pool,
        checked_fixed_bytes(len, size_of::<Option<(&Column, usize)>>(), false)?,
    )?;
    let mut sources: Vec<Option<(&Column, usize)>> = Vec::with_capacity(len);
    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            sources.push(None);
            continue;
        }
        let mut best: Option<(&Column, usize)> = None;
        for arg in args {
            if arg.is_null(row) {
                continue;
            }
            match best {
                None => best = Some((arg, row)),
                Some((bcol, brow)) => {
                    if compare_cols_at(arg, row, bcol, brow)? == want_ord {
                        best = Some((arg, row));
                    }
                }
            }
        }
        sources.push(best);
    }
    build_flat_from_sources(&out_dt, &sources, pool, ctx, before)
}

// ---------------------------------------------------------------------------
// Array functions
// ---------------------------------------------------------------------------

fn validate_list_arg(dt: &DataType, fn_name: &str) -> Result<Option<DataType>, QueryError> {
    match dt {
        DataType::List(inner) => Ok(Some((**inner).clone())),
        DataType::Null => Ok(None),
        _ => Err(QueryError::failed(format!(
            "{fn_name} requires list argument"
        ))),
    }
}

fn eval_array_has(
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    expect_arity(ScalarFn::ArrayHas, args, 2)?;
    let list_col = &args[0];
    let elem_col = &args[1];
    let item_dt = validate_list_arg(&list_col.data_type(), "array_has")?;
    let elem_dt = elem_col.data_type();
    if let Some(ref it) = item_dt
        && !matches!(it, DataType::Null)
        && !matches!(elem_dt, DataType::Null)
        && !types_comparable(it, &elem_dt)
    {
        return Err(QueryError::failed(
            "array_has element type does not match list element type",
        ));
    }

    if all_inactive(active) || item_dt.is_none() || matches!(elem_dt, DataType::Null) {
        return Ok(null_const(DataType::Bool, len));
    }

    try_reserve(pool, checked_bool_bytes(len)?)?;
    let mut vals = Bitmap::new_zeroed(len);
    let mut nulls = Bitmap::new_zeroed(len);

    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        // DataFusion 55.2 array_has: NULL list OR NULL elem -> NULL.
        let Some(lref) = list_ref_at(list_col, row) else {
            continue;
        };
        if elem_col.is_null(row) {
            continue;
        }
        let target = ElemRef::Col(elem_col, row);
        let found = list_ref_contains_non_null(lref, target, ctx, pool, before)?;
        let _ = nulls.set(row, true);
        if found {
            let _ = vals.set(row, true);
        }
    }

    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Bool(vals),
        Some(nulls),
    )?))
}

fn eval_array_has_all(
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    expect_arity(ScalarFn::ArrayHasAll, args, 2)?;
    let list_col = &args[0];
    let sub_col = &args[1];
    let l_item = validate_list_arg(&list_col.data_type(), "array_has_all")?;
    let r_item = validate_list_arg(&sub_col.data_type(), "array_has_all")?;
    if let (Some(lt), Some(rt)) = (&l_item, &r_item)
        && !matches!(lt, DataType::Null)
        && !matches!(rt, DataType::Null)
        && !types_comparable(lt, rt)
    {
        return Err(QueryError::failed(
            "array_has_all list element types do not match",
        ));
    }

    let (Some(lt), Some(rt)) = (l_item, r_item) else {
        return Ok(null_const(DataType::Bool, len));
    };
    if all_inactive(active) {
        return Ok(null_const(DataType::Bool, len));
    }

    try_reserve(pool, checked_bool_bytes(len)?)?;
    let mut vals = Bitmap::new_zeroed(len);
    let mut nulls = Bitmap::new_zeroed(len);

    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        let (Some(lref), Some(sref)) = (list_ref_at(list_col, row), list_ref_at(sub_col, row))
        else {
            continue;
        };
        let _ = nulls.set(row, true);
        if list_ref_has_all(lref, sref, &lt, &rt, ctx, pool, before)? {
            let _ = vals.set(row, true);
        }
    }

    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Bool(vals),
        Some(nulls),
    )?))
}

fn eval_array_length(
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    if args.is_empty() || args.len() > 2 {
        return Err(QueryError::failed("array_length expects 1 or 2 arguments"));
    }
    let list_col = &args[0];
    let dim_col = args.get(1);
    let item_dt = validate_list_arg(&list_col.data_type(), "array_length")?;
    if let Some(dc) = dim_col
        && !matches!(dc.data_type(), DataType::I64 | DataType::Null)
    {
        return Err(QueryError::failed("array_length dimension must be integer"));
    }
    if all_inactive(active)
        || item_dt.is_none()
        || dim_col.is_some_and(|dc| matches!(dc.data_type(), DataType::Null))
    {
        return Ok(null_const(DataType::I64, len));
    }

    try_reserve(pool, checked_fixed_bytes(len, 8, true)?)?;
    let mut vals = vec![0i64; len];
    let mut nulls = Bitmap::new_zeroed(len);

    for (row, slot) in vals.iter_mut().enumerate() {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        let Some(lref) = list_ref_at(list_col, row) else {
            continue;
        };
        if let Some(dc) = dim_col {
            match read_i64(dc, row) {
                Some(1) => {}
                _ => continue,
            }
        }
        *slot = i64::try_from(list_ref_len(lref)).map_err(|_| ColumnError::Overflow {
            reason: "array length exceeds i64::MAX",
        })?;
        let _ = nulls.set(row, true);
    }

    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::I64(vals),
        Some(nulls),
    )?))
}

fn eval_array_element(
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    expect_arity(ScalarFn::ArrayElement, args, 2)?;
    let list_col = &args[0];
    let idx_col = &args[1];
    let item_dt = validate_list_arg(&list_col.data_type(), "array_element")?;
    if !matches!(idx_col.data_type(), DataType::I64 | DataType::Null) {
        return Err(QueryError::failed("array_element index must be integer"));
    }
    let Some(item_dt) = item_dt else {
        return Ok(null_const(DataType::Null, len));
    };
    if all_inactive(active) || matches!(idx_col.data_type(), DataType::Null) {
        return Ok(null_const(item_dt, len));
    }

    try_reserve(
        pool,
        checked_fixed_bytes(len, size_of::<ElemRef<'_>>(), false)?,
    )?;
    let mut elem_refs = Vec::with_capacity(len);
    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            elem_refs.push(ElemRef::Null);
            continue;
        }
        let (Some(lref), Some(idx)) = (list_ref_at(list_col, row), read_i64(idx_col, row)) else {
            elem_refs.push(ElemRef::Null);
            continue;
        };
        let n = list_ref_len(lref);
        let pos = if idx > 0 {
            usize::try_from(idx - 1).ok().filter(|&p| p < n)
        } else if idx < 0 {
            let abs = idx.unsigned_abs();
            usize::try_from(abs).ok().filter(|&a| a <= n).map(|a| n - a)
        } else {
            None
        };
        match pos {
            Some(p) => elem_refs.push(list_ref_elem(lref, p)),
            None => elem_refs.push(ElemRef::Null),
        }
    }

    build_flat_from_elem_refs(&item_dt, &elem_refs, pool, ctx, before)
}

fn eval_array_to_string(
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    if args.len() < 2 || args.len() > 3 {
        return Err(QueryError::failed(
            "array_to_string expects 2 or 3 arguments",
        ));
    }
    let list_col = &args[0];
    let delim_col = &args[1];
    let null_str_col = args.get(2);
    let item_dt = validate_list_arg(&list_col.data_type(), "array_to_string")?;
    if !matches!(delim_col.data_type(), DataType::Utf8 | DataType::Null) {
        return Err(QueryError::failed(
            "array_to_string delimiter must be string",
        ));
    }
    if let Some(nc) = null_str_col
        && !matches!(nc.data_type(), DataType::Utf8 | DataType::Null)
    {
        return Err(QueryError::failed(
            "array_to_string null_str must be string",
        ));
    }

    if len == 0
        || all_inactive(active)
        || item_dt.is_none()
        || matches!(delim_col.data_type(), DataType::Null)
    {
        return Ok(null_const(DataType::Utf8, len));
    }

    // Pre-reserve header + amplification bytes + active leaf payload bytes before streaming.
    let header_bytes = checked_varlen_bytes(len, 0, true)?;
    let max_delim = max_active_str_len(delim_col, len, active, ctx, pool, before)?;
    let max_null_str = match null_str_col {
        Some(nc) => max_active_str_len(nc, len, active, ctx, pool, before)?,
        None => 0,
    };
    let (leaf_count, active_payload_bytes) =
        active_list_leaf_stats(list_col, len, active, ctx, pool, before)?;
    let amp_bytes = max_delim
        .checked_add(max_null_str)
        .and_then(|s| s.checked_mul(leaf_count))
        .ok_or(QueryError::TooLarge)?;
    let mut reserved_data = amp_bytes
        .checked_add(active_payload_bytes)
        .ok_or(QueryError::TooLarge)?;
    try_reserve(
        pool,
        header_bytes
            .checked_add(reserved_data)
            .ok_or(QueryError::TooLarge)?,
    )?;

    let mut offsets = Vec::with_capacity(len + 1);
    offsets.push(0u32);
    let mut bytes = Vec::new();
    let mut nulls = Bitmap::new_zeroed(len);
    let mut buf = String::new();

    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row)
            && let (Some(lref), Some(delim)) =
                (list_ref_at(list_col, row), read_str(delim_col, row))
        {
            let null_str = null_str_col.and_then(|nc| read_str(nc, row));
            buf.clear();
            let mut first = true;
            append_list_to_string(lref, delim, null_str, &mut first, &mut buf)?;
            let next_len = bytes
                .len()
                .checked_add(buf.len())
                .ok_or(ColumnError::Overflow {
                    reason: "array_to_string byte length overflow",
                })?;
            let _ = u32::try_from(next_len).map_err(|_| ColumnError::Overflow {
                reason: "utf8 byte length exceeds u32::MAX",
            })?;
            if next_len > reserved_data {
                try_reserve(pool, next_len - reserved_data)?;
                reserved_data = next_len;
            }
            bytes.extend_from_slice(buf.as_bytes());
            let _ = nulls.set(row, true);
        }
        let end = u32::try_from(bytes.len()).map_err(|_| ColumnError::Overflow {
            reason: "utf8 byte length exceeds u32::MAX",
        })?;
        offsets.push(end);
    }

    let utf8 = Utf8Data::try_new(offsets, bytes)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(utf8),
        Some(nulls),
    )?))
}

fn max_active_str_len(
    col: &Column,
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<usize, QueryError> {
    if let Column::Const { value, .. } = col {
        return Ok(match value {
            ScalarValue::Utf8(s) => s.len(),
            _ => 0,
        });
    }
    let mut max_len = 0usize;
    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row)
            && let Some(s) = read_str(col, row)
        {
            max_len = max_len.max(s.len());
        }
    }
    Ok(max_len)
}

fn active_list_leaf_stats(
    col: &Column,
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<(usize, usize), QueryError> {
    let mut count = 0usize;
    let mut payload_bytes = 0usize;
    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row)
            && let Some(lref) = list_ref_at(col, row)
        {
            let (c, p) = count_list_ref_stats(lref)?;
            count = count.checked_add(c).ok_or(QueryError::TooLarge)?;
            payload_bytes = payload_bytes.checked_add(p).ok_or(QueryError::TooLarge)?;
        }
    }
    Ok((count, payload_bytes))
}

fn count_list_ref_stats(lref: ListRef<'_>) -> Result<(usize, usize), QueryError> {
    match lref {
        ListRef::Const(slice) => {
            let mut count = 0usize;
            let mut bytes = 0usize;
            for sv in slice {
                let (c, b) = count_scalar_stats(sv)?;
                count = count.checked_add(c).ok_or(QueryError::TooLarge)?;
                bytes = bytes.checked_add(b).ok_or(QueryError::TooLarge)?;
            }
            Ok((count, bytes))
        }
        ListRef::Flat { values, start, end } => {
            if matches!(values.data_type(), DataType::List(_)) {
                let mut count = 0usize;
                let mut bytes = 0usize;
                for idx in start..end {
                    let (c, b) = match list_ref_at(values, idx) {
                        Some(inner) => count_list_ref_stats(inner)?,
                        None => (1, 0),
                    };
                    count = count.checked_add(c).ok_or(QueryError::TooLarge)?;
                    bytes = bytes.checked_add(b).ok_or(QueryError::TooLarge)?;
                }
                Ok((count, bytes))
            } else {
                let n = end - start;
                let mut bytes = 0usize;
                match values.data_type() {
                    DataType::Utf8 => {
                        for idx in start..end {
                            if let Some(s) = read_str(values, idx) {
                                bytes = bytes.checked_add(s.len()).ok_or(QueryError::TooLarge)?;
                            }
                        }
                    }
                    DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16 => {
                        for idx in start..end {
                            if let Some(b) = crate::eval::read_bytes(values, idx) {
                                let hex_len = b.len().checked_mul(2).ok_or(QueryError::TooLarge)?;
                                bytes = bytes.checked_add(hex_len).ok_or(QueryError::TooLarge)?;
                            }
                        }
                    }
                    _ => {}
                }
                Ok((n, bytes))
            }
        }
    }
}

fn count_scalar_stats(sv: &ScalarValue) -> Result<(usize, usize), QueryError> {
    match sv {
        ScalarValue::List { values, .. } => {
            let mut count = 0usize;
            let mut bytes = 0usize;
            for v in values {
                let (c, b) = count_scalar_stats(v)?;
                count = count.checked_add(c).ok_or(QueryError::TooLarge)?;
                bytes = bytes.checked_add(b).ok_or(QueryError::TooLarge)?;
            }
            Ok((count, bytes))
        }
        ScalarValue::Utf8(s) => Ok((1, s.len())),
        ScalarValue::Binary(b) => Ok((1, b.len().checked_mul(2).ok_or(QueryError::TooLarge)?)),
        ScalarValue::FixedBytes8(_) => Ok((1, 16)),
        ScalarValue::FixedBytes16(_) => Ok((1, 32)),
        _ => Ok((1, 0)),
    }
}

fn append_list_to_string(
    lref: ListRef<'_>,
    delim: &str,
    null_str: Option<&str>,
    first: &mut bool,
    out: &mut String,
) -> Result<(), QueryError> {
    let n = list_ref_len(lref);
    for i in 0..n {
        let elem = list_ref_elem(lref, i);
        if elem.is_null() {
            if let Some(ns) = null_str {
                if !*first {
                    out.push_str(delim);
                }
                *first = false;
                out.push_str(ns);
            }
            continue;
        }
        if let Some(nested) = elem.as_list_ref() {
            append_list_to_string(nested, delim, null_str, first, out)?;
        } else {
            if !*first {
                out.push_str(delim);
            }
            *first = false;
            append_elem_formatted(elem, out)?;
        }
    }
    Ok(())
}

fn append_elem_formatted(elem: ElemRef<'_>, out: &mut String) -> Result<(), QueryError> {
    use std::fmt::Write;
    match elem {
        ElemRef::Null => {}
        ElemRef::Col(col, row) => match col.data_type() {
            DataType::Utf8 => {
                if let Some(s) = read_str(col, row) {
                    out.push_str(s);
                }
            }
            DataType::Bool => {
                if let Some(b) = read_bool(col, row) {
                    let _ = write!(out, "{b}");
                }
            }
            DataType::I64 => {
                if let Some(v) = read_i64(col, row) {
                    let _ = write!(out, "{v}");
                }
            }
            DataType::F64 => {
                if let Some(v) = read_f64(col, row) {
                    let _ = write!(out, "{v}");
                }
            }
            DataType::Decimal128 { scale, .. } => {
                if let Some(v) = read_decimal(col, row) {
                    out.push_str(&format_decimal(v, scale));
                }
            }
            DataType::TimestampNs { utc } => {
                if let Some(ns) = read_timestamp(col, row) {
                    out.push_str(&format_timestamp_ns(ns, utc));
                }
            }
            DataType::Date => {
                if let Some(d) = read_date(col, row) {
                    out.push_str(&format_date_days(d));
                }
            }
            DataType::Interval => {
                if let Some(iv) = read_interval(col, row) {
                    let _ = write!(out, "{} months {} days {} ns", iv.months, iv.days, iv.nanos);
                }
            }
            DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16 => {
                if let Some(b) = crate::eval::read_bytes(col, row) {
                    out.push_str(&hex::encode(b));
                }
            }
            _ => {
                let sv = read_scalar(col, row);
                append_scalar_formatted(&sv, out)?;
            }
        },
        ElemRef::Scalar(sv) => append_scalar_formatted(sv, out)?,
    }
    Ok(())
}

fn append_scalar_formatted(sv: &ScalarValue, out: &mut String) -> Result<(), QueryError> {
    use std::fmt::Write;
    match sv {
        ScalarValue::Null(_) => {}
        ScalarValue::Utf8(s) => out.push_str(s),
        ScalarValue::Bool(b) => {
            let _ = write!(out, "{b}");
        }
        ScalarValue::I64(v) => {
            let _ = write!(out, "{v}");
        }
        ScalarValue::F64(v) => {
            let _ = write!(out, "{v}");
        }
        ScalarValue::Decimal128 { value, scale, .. } => {
            out.push_str(&format_decimal(*value, *scale));
        }
        ScalarValue::TimestampNs { value, utc } => {
            out.push_str(&format_timestamp_ns(*value, *utc));
        }
        ScalarValue::Date(d) => out.push_str(&format_date_days(*d)),
        ScalarValue::Interval(iv) => {
            let _ = write!(out, "{} months {} days {} ns", iv.months, iv.days, iv.nanos);
        }
        ScalarValue::Binary(b) => out.push_str(&hex::encode(b)),
        ScalarValue::FixedBytes8(b) => out.push_str(&hex::encode(b)),
        ScalarValue::FixedBytes16(b) => out.push_str(&hex::encode(b)),
        ScalarValue::MapUtf8(entries) => {
            out.push('{');
            for (i, (k, v)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(k);
                out.push(':');
                match v {
                    Some(val) => out.push_str(val),
                    None => out.push_str("null"),
                }
            }
            out.push('}');
        }
        ScalarValue::List { .. } | ScalarValue::Events(_) | ScalarValue::Links(_) => {
            return Err(QueryError::failed(
                "unsupported element type in array_to_string",
            ));
        }
    }
    Ok(())
}

fn eval_array_distinct(
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    expect_arity(ScalarFn::ArrayDistinct, args, 1)?;
    let list_col = &args[0];
    let dt = list_col.data_type();
    let Some(item_dt) = validate_list_arg(&dt, "array_distinct")? else {
        return Ok(null_const(DataType::Null, len));
    };
    if all_inactive(active) {
        return Ok(null_const(dt, len));
    }

    let mut total_bound = 0usize;
    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row)
            && let Some(lref) = list_ref_at(list_col, row)
        {
            total_bound =
                total_bound
                    .checked_add(list_ref_len(lref))
                    .ok_or(ColumnError::Overflow {
                        reason: "array_distinct element count overflow",
                    })?;
        }
    }
    let header_bytes = checked_varlen_bytes(len, 0, true)?;
    let scratch_bytes = checked_fixed_bytes(total_bound, size_of::<ElemRef<'_>>(), false)?;
    try_reserve(
        pool,
        header_bytes
            .checked_add(scratch_bytes)
            .ok_or(QueryError::TooLarge)?,
    )?;

    let mut offsets = Vec::with_capacity(len + 1);
    offsets.push(0u32);
    let mut nulls = Bitmap::new_zeroed(len);
    let mut elem_refs: Vec<ElemRef<'_>> = Vec::with_capacity(total_bound);

    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row)
            && let Some(lref) = list_ref_at(list_col, row)
        {
            let _ = nulls.set(row, true);
            collect_distinct_elems(lref, &item_dt, &mut elem_refs, ctx, pool, before)?;
        }
        let end = u32::try_from(elem_refs.len()).map_err(|_| ColumnError::Overflow {
            reason: "array_distinct element count exceeds u32::MAX",
        })?;
        offsets.push(end);
    }

    let values = build_flat_from_elem_refs(&item_dt, &elem_refs, pool, ctx, before)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::List {
            item_type: item_dt,
            offsets,
            values: Box::new(values),
        },
        Some(nulls),
    )?))
}

#[derive(Hash, PartialEq, Eq)]
enum DistinctKey<'a> {
    Null,
    Bool(bool),
    I64(i64),
    F64(u64),
    Decimal(i128),
    Interval(Interval),
    Str(&'a str),
    Bytes(&'a [u8]),
}

fn elem_distinct_key<'a>(elem: ElemRef<'a>, dt: &DataType) -> Option<DistinctKey<'a>> {
    if elem.is_null() {
        return Some(DistinctKey::Null);
    }
    match dt {
        DataType::Null => Some(DistinctKey::Null),
        DataType::Bool => elem.read_bool().map(DistinctKey::Bool),
        DataType::I64 => elem.read_i64().map(DistinctKey::I64),
        DataType::F64 => elem
            .read_f64()
            .map(|v| DistinctKey::F64(f64_canonical_bits(v))),
        DataType::Decimal128 { .. } => elem.read_decimal().map(|(v, _)| DistinctKey::Decimal(v)),
        DataType::TimestampNs { .. } => elem.read_timestamp().map(DistinctKey::I64),
        DataType::Date => elem.read_date().map(|d| DistinctKey::I64(i64::from(d))),
        DataType::Interval => elem.read_interval().map(DistinctKey::Interval),
        DataType::Utf8 => elem.read_str().map(DistinctKey::Str),
        DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16 => {
            elem.read_bytes().map(DistinctKey::Bytes)
        }
        _ => None,
    }
}

fn is_hashable_scalar(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Null
            | DataType::Bool
            | DataType::I64
            | DataType::F64
            | DataType::Decimal128 { .. }
            | DataType::TimestampNs { .. }
            | DataType::Date
            | DataType::Interval
            | DataType::Utf8
            | DataType::Binary
            | DataType::FixedBytes8
            | DataType::FixedBytes16
    )
}

fn is_hashable_pair(a: &DataType, b: &DataType) -> bool {
    if !is_hashable_scalar(a) || !is_hashable_scalar(b) {
        return false;
    }
    if let (DataType::Decimal128 { scale: sa, .. }, DataType::Decimal128 { scale: sb, .. }) = (a, b)
    {
        return sa == sb;
    }
    true
}

fn collect_distinct_elems<'a>(
    lref: ListRef<'a>,
    item_dt: &DataType,
    out: &mut Vec<ElemRef<'a>>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<(), QueryError> {
    let n = list_ref_len(lref);
    if n == 0 {
        return Ok(());
    }
    // Use O(n) HashSet for all hashable scalar element types.
    let first = list_ref_elem(lref, 0);
    let can_hash = is_hashable_scalar(item_dt) && elem_distinct_key(first, item_dt).is_some();

    if can_hash {
        let before_set = pool_size(pool);
        let set_bytes = checked_fixed_bytes(n, size_of::<DistinctKey<'a>>() + 8, false)?;
        try_reserve(pool, set_bytes)?;
        let mut seen: HashSet<DistinctKey<'a>> = HashSet::with_capacity(n);
        for i in 0..n {
            if (i & CANCEL_CHECK_MASK) == 0 {
                check_cancel(ctx, pool, before)?;
            }
            let elem = list_ref_elem(lref, i);
            if let Some(key) = elem_distinct_key(elem, item_dt)
                && seen.insert(key)
            {
                out.push(elem);
            }
        }
        drop(seen);
        rollback_to(pool, before_set);
        return Ok(());
    }

    let start_out = out.len();
    let mut iters = 0usize;
    for i in 0..n {
        let elem = list_ref_elem(lref, i);
        let mut dup = false;
        for existing in &out[start_out..] {
            if (iters & CANCEL_CHECK_MASK) == 0 {
                check_cancel(ctx, pool, before)?;
            }
            iters = iters.wrapping_add(1);
            if elems_equal(elem, *existing)? {
                dup = true;
                break;
            }
        }
        if !dup {
            if (iters & CANCEL_CHECK_MASK) == 0 {
                check_cancel(ctx, pool, before)?;
            }
            iters = iters.wrapping_add(1);
            out.push(elem);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Map functions: MapKeys, MapValues
// ---------------------------------------------------------------------------

fn eval_map_extract(
    keys_only: bool,
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    let fn_sym = if keys_only {
        ScalarFn::MapKeys
    } else {
        ScalarFn::MapValues
    };
    expect_arity(fn_sym, args, 1)?;
    let map_col = &args[0];
    let dt = map_col.data_type();
    if !matches!(dt, DataType::MapUtf8 | DataType::Null) {
        return Err(QueryError::failed(format!(
            "{} requires map argument",
            fn_sym.name()
        )));
    }
    let out_dt = DataType::List(Box::new(DataType::Utf8));
    if all_inactive(active) || matches!(dt, DataType::Null) {
        return Ok(null_const(out_dt, len));
    }

    let mut total_entries = 0usize;
    let mut total_bytes = 0usize;
    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        let Some(mref) = map_ref_at(map_col, row) else {
            continue;
        };
        match mref {
            MapRef::Const(entries) => {
                total_entries =
                    total_entries
                        .checked_add(entries.len())
                        .ok_or(ColumnError::Overflow {
                            reason: "map entry count overflow",
                        })?;
                for (k, v) in entries {
                    let s_len = if keys_only {
                        k.len()
                    } else {
                        v.as_ref().map_or(0, String::len)
                    };
                    total_bytes = total_bytes
                        .checked_add(s_len)
                        .ok_or(ColumnError::Overflow {
                            reason: "utf8 byte length overflow",
                        })?;
                }
            }
            MapRef::Flat { data, start, end } => {
                total_entries =
                    total_entries
                        .checked_add(end - start)
                        .ok_or(ColumnError::Overflow {
                            reason: "map entry count overflow",
                        })?;
                for idx in start..end {
                    let s_len = if keys_only {
                        data.keys().get(idx).map_or(0, str::len)
                    } else if data.value_nulls().is_some_and(|bm| !bm.get(idx)) {
                        0
                    } else {
                        data.values().get(idx).map_or(0, str::len)
                    };
                    total_bytes = total_bytes
                        .checked_add(s_len)
                        .ok_or(ColumnError::Overflow {
                            reason: "utf8 byte length overflow",
                        })?;
                }
            }
        }
    }

    let _ = u32::try_from(total_entries).map_err(|_| ColumnError::Overflow {
        reason: "map list length exceeds u32::MAX",
    })?;
    let _ = u32::try_from(total_bytes).map_err(|_| ColumnError::Overflow {
        reason: "utf8 byte length exceeds u32::MAX",
    })?;

    let est = checked_varlen_bytes(len, 0, true)?
        .checked_add(checked_varlen_bytes(
            total_entries,
            total_bytes,
            !keys_only,
        )?)
        .ok_or(QueryError::TooLarge)?;
    try_reserve(pool, est)?;

    let mut list_offsets = Vec::with_capacity(len + 1);
    list_offsets.push(0u32);
    let mut list_nulls = Bitmap::new_zeroed(len);
    let mut child_offsets = Vec::with_capacity(total_entries + 1);
    child_offsets.push(0u32);
    let mut child_bytes = Vec::with_capacity(total_bytes);
    let mut child_nulls = (!keys_only).then(|| Bitmap::new_zeroed(total_entries));
    let mut entry_pos = 0usize;

    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row)
            && let Some(mref) = map_ref_at(map_col, row)
        {
            let _ = list_nulls.set(row, true);
            match mref {
                MapRef::Const(entries) => {
                    for (k, v) in entries {
                        let opt = if keys_only {
                            Some(k.as_str())
                        } else {
                            v.as_deref()
                        };
                        if let Some(s) = opt {
                            child_bytes.extend_from_slice(s.as_bytes());
                            if let Some(cn) = child_nulls.as_mut() {
                                let _ = cn.set(entry_pos, true);
                            }
                        }
                        child_offsets.push(child_bytes.len() as u32);
                        entry_pos += 1;
                    }
                }
                MapRef::Flat { data, start, end } => {
                    for idx in start..end {
                        let opt = if keys_only {
                            data.keys().get(idx)
                        } else if data.value_nulls().is_some_and(|bm| !bm.get(idx)) {
                            None
                        } else {
                            data.values().get(idx)
                        };
                        if let Some(s) = opt {
                            child_bytes.extend_from_slice(s.as_bytes());
                            if let Some(cn) = child_nulls.as_mut() {
                                let _ = cn.set(entry_pos, true);
                            }
                        }
                        child_offsets.push(child_bytes.len() as u32);
                        entry_pos += 1;
                    }
                }
            }
        }
        list_offsets.push(entry_pos as u32);
    }

    let utf8 = Utf8Data::try_new(child_offsets, child_bytes)?;
    let child_col = Column::Flat(FlatColumn::try_new(FlatData::Utf8(utf8), child_nulls)?);
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::List {
            item_type: DataType::Utf8,
            offsets: list_offsets,
            values: Box::new(child_col),
        },
        Some(list_nulls),
    )?))
}

// ---------------------------------------------------------------------------
// JSON functions: JsonGet (->), JsonGetText (->>), JsonExtract, JsonExtractText
// ---------------------------------------------------------------------------

struct JsonBudgetWriter<'a, 'b> {
    bytes: &'a mut Vec<u8>,
    pool: &'a mut Option<&'b mut Reservation>,
    err: Option<QueryError>,
}

impl std::io::Write for JsonBudgetWriter<'_, '_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let Some(next_len) = self.bytes.len().checked_add(buf.len()) else {
            self.err = Some(QueryError::TooLarge);
            return Err(std::io::Error::other("json output byte length overflow"));
        };
        if u32::try_from(next_len).is_err() {
            self.err = Some(QueryError::TooLarge);
            return Err(std::io::Error::other("json output exceeds u32::MAX"));
        }
        if let Err(err) = try_reserve(self.pool, buf.len()) {
            self.err = Some(err);
            return Err(std::io::Error::other("json output reservation exceeded"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn write_json_slice(
    slice: &[u8],
    bytes: &mut Vec<u8>,
    pool: &mut Option<&mut Reservation>,
) -> Result<(), QueryError> {
    let next_len = bytes
        .len()
        .checked_add(slice.len())
        .ok_or(ColumnError::Overflow {
            reason: "json output byte length overflow",
        })?;
    let _ = u32::try_from(next_len).map_err(|_| ColumnError::Overflow {
        reason: "json output byte length exceeds u32::MAX",
    })?;
    try_reserve(pool, slice.len())?;
    bytes.extend_from_slice(slice);
    Ok(())
}

fn write_json_result(
    val: &JsonValue,
    as_text: bool,
    bytes: &mut Vec<u8>,
    pool: &mut Option<&mut Reservation>,
) -> Result<bool, QueryError> {
    if as_text {
        match val {
            JsonValue::Null => Ok(false),
            JsonValue::String(s) => {
                write_json_slice(s.as_bytes(), bytes, pool)?;
                Ok(true)
            }
            other => {
                let mut writer = JsonBudgetWriter {
                    bytes,
                    pool,
                    err: None,
                };
                if serde_json::to_writer(&mut writer, other).is_err() {
                    return Err(writer
                        .err
                        .take()
                        .unwrap_or_else(|| QueryError::failed("json serialization failed")));
                }
                Ok(true)
            }
        }
    } else {
        let mut writer = JsonBudgetWriter {
            bytes,
            pool,
            err: None,
        };
        if serde_json::to_writer(&mut writer, val).is_err() {
            return Err(writer
                .err
                .take()
                .unwrap_or_else(|| QueryError::failed("json serialization failed")));
        }
        Ok(true)
    }
}

fn eval_json_get(
    as_text: bool,
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    let fn_sym = if as_text {
        ScalarFn::JsonGetText
    } else {
        ScalarFn::JsonGet
    };
    expect_arity(fn_sym, args, 2)?;
    let json_col = &args[0];
    let key_col = &args[1];
    if !matches!(json_col.data_type(), DataType::Utf8 | DataType::Null) {
        return Err(QueryError::failed(format!(
            "{} requires string JSON input",
            fn_sym.name()
        )));
    }
    let key_dt = key_col.data_type();
    if !matches!(key_dt, DataType::Utf8 | DataType::I64 | DataType::Null) {
        return Err(QueryError::failed(format!(
            "{} key must be string or integer",
            fn_sym.name()
        )));
    }
    if len == 0
        || all_inactive(active)
        || matches!(json_col.data_type(), DataType::Null)
        || matches!(key_dt, DataType::Null)
    {
        return Ok(null_const(DataType::Utf8, len));
    }

    try_reserve(pool, checked_varlen_bytes(len, 0, true)?)?;
    let const_parsed: Option<Option<JsonValue>> = match json_col {
        Column::Const { value, .. } => match value {
            ScalarValue::Utf8(s) => {
                try_reserve(pool, s.len())?;
                Some(serde_json::from_str::<JsonValue>(s).ok())
            }
            _ => Some(None),
        },
        _ => None,
    };

    let mut offsets = Vec::with_capacity(len + 1);
    offsets.push(0u32);
    let mut bytes = Vec::new();
    let mut nulls = Bitmap::new_zeroed(len);

    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row) && !key_col.is_null(row) {
            let mut dom_reserved = 0usize;
            let mut parsed_owned: Option<JsonValue> = None;
            let val_ref: Option<&JsonValue> = match &const_parsed {
                Some(opt) => opt.as_ref(),
                None => match read_str(json_col, row) {
                    Some(s) => {
                        dom_reserved = s.len();
                        try_reserve(pool, dom_reserved)?;
                        parsed_owned = serde_json::from_str::<JsonValue>(s).ok();
                        parsed_owned.as_ref()
                    }
                    None => None,
                },
            };

            if let Some(root) = val_ref {
                let target: Option<&JsonValue> = match key_dt {
                    DataType::Utf8 => read_str(key_col, row).and_then(|k| match root {
                        JsonValue::Object(map) => map.get(k),
                        _ => None,
                    }),
                    DataType::I64 => read_i64(key_col, row).and_then(|idx| {
                        if idx < 0 {
                            return None;
                        }
                        let i = usize::try_from(idx).ok()?;
                        match root {
                            JsonValue::Array(arr) => arr.get(i),
                            _ => None,
                        }
                    }),
                    _ => None,
                };
                if let Some(v) = target
                    && write_json_result(v, as_text, &mut bytes, pool)?
                {
                    let _ = nulls.set(row, true);
                }
            }

            drop(parsed_owned);
            if dom_reserved > 0
                && let Some(res) = pool.as_deref_mut()
            {
                res.shrink(dom_reserved);
            }
        }
        let end = u32::try_from(bytes.len()).map_err(|_| ColumnError::Overflow {
            reason: "utf8 byte length exceeds u32::MAX",
        })?;
        offsets.push(end);
    }

    let utf8 = Utf8Data::try_new(offsets, bytes)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(utf8),
        Some(nulls),
    )?))
}

fn eval_json_extract(
    as_text: bool,
    args: &[Column],
    len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    let fn_sym = if as_text {
        ScalarFn::JsonExtractText
    } else {
        ScalarFn::JsonExtract
    };
    expect_arity(fn_sym, args, 2)?;
    let json_col = &args[0];
    let path_col = &args[1];
    if !matches!(json_col.data_type(), DataType::Utf8 | DataType::Null) {
        return Err(QueryError::failed(format!(
            "{} requires string JSON input",
            fn_sym.name()
        )));
    }
    if !matches!(path_col.data_type(), DataType::Utf8 | DataType::Null) {
        return Err(QueryError::failed(format!(
            "{} requires string JSONPath",
            fn_sym.name()
        )));
    }
    if len == 0
        || all_inactive(active)
        || matches!(json_col.data_type(), DataType::Null)
        || matches!(path_col.data_type(), DataType::Null)
    {
        return Ok(null_const(DataType::Utf8, len));
    }

    try_reserve(pool, checked_varlen_bytes(len, 0, true)?)?;
    let path_is_const = matches!(path_col, Column::Const { .. });
    let mut cached_const_path: Option<JsonPath> = None;
    let mut last_dyn_path: Option<(&str, JsonPath)> = None;

    let const_parsed: Option<Option<JsonValue>> = match json_col {
        Column::Const { value, .. } => match value {
            ScalarValue::Utf8(s) => {
                try_reserve(pool, s.len())?;
                Some(serde_json::from_str::<JsonValue>(s).ok())
            }
            _ => Some(None),
        },
        _ => None,
    };

    let mut offsets = Vec::with_capacity(len + 1);
    offsets.push(0u32);
    let mut bytes = Vec::new();
    let mut nulls = Bitmap::new_zeroed(len);

    for row in 0..len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row)
            && let Some(path_str) = read_str(path_col, row)
        {
            let compiled_path: &JsonPath = if path_is_const {
                if let Some(ref p) = cached_const_path {
                    p
                } else {
                    let parsed = JsonPath::parse(path_str)
                        .map_err(|_| QueryError::failed("invalid JSONPath expression"))?;
                    cached_const_path = Some(parsed);
                    match cached_const_path.as_ref() {
                        Some(p) => p,
                        None => {
                            return Err(QueryError::failed("invalid JSONPath expression"));
                        }
                    }
                }
            } else {
                let hit = matches!(last_dyn_path.as_ref(), Some((prev, _)) if *prev == path_str);
                if !hit {
                    let parsed = JsonPath::parse(path_str)
                        .map_err(|_| QueryError::failed("invalid JSONPath expression"))?;
                    last_dyn_path = Some((path_str, parsed));
                }
                match last_dyn_path.as_ref() {
                    Some((_, p)) => p,
                    None => {
                        return Err(QueryError::failed("invalid JSONPath expression"));
                    }
                }
            };

            let mut dom_reserved = 0usize;
            let mut parsed_owned: Option<JsonValue> = None;
            let val_ref: Option<&JsonValue> = match &const_parsed {
                Some(opt) => opt.as_ref(),
                None => match read_str(json_col, row) {
                    Some(s) => {
                        dom_reserved = s.len();
                        try_reserve(pool, dom_reserved)?;
                        parsed_owned = serde_json::from_str::<JsonValue>(s).ok();
                        parsed_owned.as_ref()
                    }
                    None => None,
                },
            };

            if let Some(root) = val_ref {
                let nodes = compiled_path.query(root).all();
                match nodes.as_slice() {
                    [] => {}
                    [one] => {
                        if write_json_result(one, as_text, &mut bytes, pool)? {
                            let _ = nulls.set(row, true);
                        }
                    }
                    many => {
                        let mut writer = JsonBudgetWriter {
                            bytes: &mut bytes,
                            pool,
                            err: None,
                        };
                        if serde_json::to_writer(&mut writer, many).is_err() {
                            return Err(writer.err.take().unwrap_or_else(|| {
                                QueryError::failed("json serialization failed")
                            }));
                        }
                        let _ = nulls.set(row, true);
                    }
                }
            }

            drop(parsed_owned);
            if dom_reserved > 0
                && let Some(res) = pool.as_deref_mut()
            {
                res.shrink(dom_reserved);
            }
        }
        let end = u32::try_from(bytes.len()).map_err(|_| ColumnError::Overflow {
            reason: "utf8 byte length exceeds u32::MAX",
        })?;
        offsets.push(end);
    }

    let utf8 = Utf8Data::try_new(offsets, bytes)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(utf8),
        Some(nulls),
    )?))
}

// ---------------------------------------------------------------------------
// Shared encoding-agnostic list/column comparison helpers
// ---------------------------------------------------------------------------

fn push_list_ref_elems<'a>(lref: ListRef<'a>, out: &mut Vec<ElemRef<'a>>) {
    let n = list_ref_len(lref);
    for i in 0..n {
        out.push(list_ref_elem(lref, i));
    }
}

fn types_comparable(a: &DataType, b: &DataType) -> bool {
    match (a, b) {
        (DataType::TimestampNs { .. }, DataType::TimestampNs { .. }) => true,
        (DataType::Decimal128 { .. }, DataType::Decimal128 { .. }) => true,
        (
            DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16,
            DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16,
        ) => true,
        (DataType::List(la), DataType::List(lb)) => {
            matches!(la.as_ref(), DataType::Null)
                || matches!(lb.as_ref(), DataType::Null)
                || types_comparable(la, lb)
        }
        _ => a == b,
    }
}

fn list_ref_contains_non_null(
    lref: ListRef<'_>,
    target: ElemRef<'_>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<bool, QueryError> {
    let n = list_ref_len(lref);
    for i in 0..n {
        if (i & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        let elem = list_ref_elem(lref, i);
        if !elem.is_null() && elems_equal(elem, target)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn list_ref_has_all(
    list: ListRef<'_>,
    sub: ListRef<'_>,
    list_item_dt: &DataType,
    sub_item_dt: &DataType,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<bool, QueryError> {
    let sub_len = list_ref_len(sub);
    if sub_len == 0 {
        return Ok(true);
    }
    let list_len = list_ref_len(list);
    if list_len.saturating_mul(sub_len) > 64 && is_hashable_pair(list_item_dt, sub_item_dt) {
        let before_set = pool_size(pool);
        let set_bytes = checked_fixed_bytes(list_len, size_of::<DistinctKey<'_>>() + 8, false)?;
        try_reserve(pool, set_bytes)?;
        let mut set: HashSet<DistinctKey<'_>> = HashSet::with_capacity(list_len);
        for j in 0..list_len {
            if (j & CANCEL_CHECK_MASK) == 0 {
                check_cancel(ctx, pool, before)?;
            }
            if let Some(key) = elem_distinct_key(list_ref_elem(list, j), list_item_dt) {
                set.insert(key);
            }
        }
        let mut all_present = true;
        for i in 0..sub_len {
            if (i & CANCEL_CHECK_MASK) == 0 {
                check_cancel(ctx, pool, before)?;
            }
            let Some(key) = elem_distinct_key(list_ref_elem(sub, i), sub_item_dt) else {
                all_present = false;
                break;
            };
            if !set.contains(&key) {
                all_present = false;
                break;
            }
        }
        drop(set);
        rollback_to(pool, before_set);
        return Ok(all_present);
    }

    let mut iters = 0usize;
    for i in 0..sub_len {
        let target = list_ref_elem(sub, i);
        let mut found = false;
        for j in 0..list_len {
            if (iters & CANCEL_CHECK_MASK) == 0 {
                check_cancel(ctx, pool, before)?;
            }
            iters = iters.wrapping_add(1);
            if elems_equal(list_ref_elem(list, j), target)? {
                found = true;
                break;
            }
        }
        if !found {
            return Ok(false);
        }
    }
    Ok(true)
}

fn elems_equal(a: ElemRef<'_>, b: ElemRef<'_>) -> Result<bool, QueryError> {
    match (a.is_null(), b.is_null()) {
        (true, true) => return Ok(true),
        (true, false) | (false, true) => return Ok(false),
        (false, false) => {}
    }
    match (a.data_type(), b.data_type()) {
        (DataType::Bool, DataType::Bool) => Ok(a.read_bool() == b.read_bool()),
        (DataType::I64, DataType::I64) => Ok(a.read_i64() == b.read_i64()),
        (DataType::F64, DataType::F64) => match (a.read_f64(), b.read_f64()) {
            (Some(x), Some(y)) => Ok(f64_cmp(x, y) == Ordering::Equal),
            _ => Ok(false),
        },
        (DataType::Decimal128 { .. }, DataType::Decimal128 { .. }) => {
            match (a.read_decimal(), b.read_decimal()) {
                (Some((x, sx)), Some((y, sy))) => {
                    Ok(compare_decimals(x, sx, y, sy)? == Ordering::Equal)
                }
                _ => Ok(false),
            }
        }
        (DataType::TimestampNs { .. }, DataType::TimestampNs { .. }) => {
            Ok(a.read_timestamp() == b.read_timestamp())
        }
        (DataType::Date, DataType::Date) => Ok(a.read_date() == b.read_date()),
        (DataType::Interval, DataType::Interval) => Ok(a.read_interval() == b.read_interval()),
        (DataType::Utf8, DataType::Utf8) => Ok(a.read_str() == b.read_str()),
        (
            DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16,
            DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16,
        ) => Ok(a.read_bytes() == b.read_bytes()),
        (DataType::List(_), DataType::List(_)) => match (a.as_list_ref(), b.as_list_ref()) {
            (Some(la), Some(lb)) => {
                let len_a = list_ref_len(la);
                let len_b = list_ref_len(lb);
                if len_a != len_b {
                    return Ok(false);
                }
                for i in 0..len_a {
                    if !elems_equal(list_ref_elem(la, i), list_ref_elem(lb, i))? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Ok(false),
        },
        (DataType::MapUtf8, DataType::MapUtf8)
        | (DataType::Events, DataType::Events)
        | (DataType::Links, DataType::Links) => Ok(a.to_scalar() == b.to_scalar()),
        _ => Err(QueryError::failed("incompatible element comparison")),
    }
}

fn compare_cols_at(
    lcol: &Column,
    lrow: usize,
    rcol: &Column,
    rrow: usize,
) -> Result<Ordering, QueryError> {
    compare_elems(ElemRef::Col(lcol, lrow), ElemRef::Col(rcol, rrow))
}

fn compare_elems(a: ElemRef<'_>, b: ElemRef<'_>) -> Result<Ordering, QueryError> {
    match (a.is_null(), b.is_null()) {
        (true, true) => return Ok(Ordering::Equal),
        (true, false) => return Ok(Ordering::Less),
        (false, true) => return Ok(Ordering::Greater),
        (false, false) => {}
    }
    match (a.data_type(), b.data_type()) {
        (DataType::Bool, DataType::Bool) => Ok(a.read_bool().cmp(&b.read_bool())),
        (DataType::I64, DataType::I64) => Ok(a.read_i64().cmp(&b.read_i64())),
        (DataType::F64, DataType::F64) => match (a.read_f64(), b.read_f64()) {
            (Some(x), Some(y)) => Ok(f64_cmp(x, y)),
            _ => Err(QueryError::failed("invalid f64 comparison")),
        },
        (DataType::Decimal128 { .. }, DataType::Decimal128 { .. }) => {
            match (a.read_decimal(), b.read_decimal()) {
                (Some((x, sx)), Some((y, sy))) => compare_decimals(x, sx, y, sy),
                _ => Err(QueryError::failed("invalid decimal comparison")),
            }
        }
        (DataType::TimestampNs { .. }, DataType::TimestampNs { .. }) => {
            Ok(a.read_timestamp().cmp(&b.read_timestamp()))
        }
        (DataType::Date, DataType::Date) => Ok(a.read_date().cmp(&b.read_date())),
        (DataType::Interval, DataType::Interval) => match (a.read_interval(), b.read_interval()) {
            (Some(x), Some(y)) => Ok((x.months, x.days, x.nanos).cmp(&(y.months, y.days, y.nanos))),
            _ => Err(QueryError::failed("invalid interval comparison")),
        },
        (DataType::Utf8, DataType::Utf8) => Ok(a.read_str().cmp(&b.read_str())),
        (
            DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16,
            DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16,
        ) => Ok(a.read_bytes().cmp(&b.read_bytes())),
        (DataType::List(_), DataType::List(_)) => match (a.as_list_ref(), b.as_list_ref()) {
            (Some(la), Some(lb)) => {
                let len_a = list_ref_len(la);
                let len_b = list_ref_len(lb);
                for i in 0..len_a.min(len_b) {
                    let ord = compare_elems(list_ref_elem(la, i), list_ref_elem(lb, i))?;
                    if ord != Ordering::Equal {
                        return Ok(ord);
                    }
                }
                Ok(len_a.cmp(&len_b))
            }
            _ => Err(QueryError::failed("invalid list comparison")),
        },
        _ => Err(QueryError::failed("incompatible types in comparison")),
    }
}

#[cfg(test)]
mod tests {
    use cetra_column::{CancelToken, DictColumn, GlobalPool, MapData, RleColumn};

    use super::*;

    fn ctx() -> EvalContext {
        EvalContext {
            now_ns: 1_700_000_000_000_000_000,
            cancel: CancelToken::new(),
        }
    }

    fn i64_col(vals: &[Option<i64>]) -> Column {
        let data: Vec<i64> = vals.iter().map(|v| v.unwrap_or(0)).collect();
        let nulls = Bitmap::from_bools(&vals.iter().map(Option::is_some).collect::<Vec<_>>());
        Column::Flat(FlatColumn::try_new(FlatData::I64(data), Some(nulls)).unwrap())
    }

    fn f64_col(vals: &[Option<f64>]) -> Column {
        let data: Vec<f64> = vals.iter().map(|v| v.unwrap_or(0.0)).collect();
        let nulls = Bitmap::from_bools(&vals.iter().map(Option::is_some).collect::<Vec<_>>());
        Column::Flat(FlatColumn::try_new(FlatData::F64(data), Some(nulls)).unwrap())
    }

    fn utf8_col(vals: &[Option<&str>]) -> Column {
        let data = Utf8Data::try_from_slices(vals.iter().map(|v| v.unwrap_or(""))).unwrap();
        let nulls = Bitmap::from_bools(&vals.iter().map(Option::is_some).collect::<Vec<_>>());
        Column::Flat(FlatColumn::try_new(FlatData::Utf8(data), Some(nulls)).unwrap())
    }

    fn list_i64_col(rows: &[Option<&[Option<i64>]>]) -> Column {
        let mut offsets = vec![0u32];
        let mut child_vals = Vec::new();
        let mut row_valid = Vec::with_capacity(rows.len());
        for r in rows {
            match r {
                None => row_valid.push(false),
                Some(elems) => {
                    row_valid.push(true);
                    child_vals.extend_from_slice(elems);
                }
            }
            offsets.push(child_vals.len() as u32);
        }
        let values = i64_col(&child_vals);
        let nulls = Bitmap::from_bools(&row_valid);
        Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::I64,
                    offsets,
                    values: Box::new(values),
                },
                Some(nulls),
            )
            .unwrap(),
        )
    }

    fn collect_bools(col: &Column) -> Vec<Option<bool>> {
        (0..col.len())
            .map(|r| (!col.is_null(r)).then(|| read_bool(col, r)).flatten())
            .collect()
    }

    fn collect_i64s(col: &Column) -> Vec<Option<i64>> {
        (0..col.len())
            .map(|r| (!col.is_null(r)).then(|| read_i64(col, r)).flatten())
            .collect()
    }

    fn collect_f64s(col: &Column) -> Vec<Option<f64>> {
        (0..col.len())
            .map(|r| (!col.is_null(r)).then(|| read_f64(col, r)).flatten())
            .collect()
    }

    fn collect_strs(col: &Column) -> Vec<Option<String>> {
        (0..col.len())
            .map(|r| {
                (!col.is_null(r))
                    .then(|| read_str(col, r).map(str::to_owned))
                    .flatten()
            })
            .collect()
    }

    fn collect_scalars(col: &Column) -> Vec<ScalarValue> {
        (0..col.len()).map(|r| col.get_scalar(r).unwrap()).collect()
    }

    #[test]
    fn math_and_null_helpers() {
        let c = ctx();
        let pool = GlobalPool::standalone(1 << 20);
        let mut res = pool.register("test", false);

        // abs(i64::MIN) -> Failed, with reservation rollback
        let min_col = i64_col(&[Some(i64::MIN)]);
        let before = res.size();
        let err = eval_misc_fn(
            ScalarFn::Abs,
            std::slice::from_ref(&min_col),
            1,
            None,
            &c,
            Some(&mut res),
        )
        .unwrap_err();
        assert!(matches!(err, QueryError::Failed(_)));
        assert_eq!(res.size(), before);

        // abs(i64::MIN) on inactive row does not fault
        let mask = Bitmap::from_bools(&[false]);
        let masked_out = eval_misc_fn(
            ScalarFn::Abs,
            &[min_col],
            1,
            Some(&mask),
            &c,
            Some(&mut res),
        )
        .unwrap();
        assert_eq!(collect_i64s(&masked_out), vec![None]);
        assert_eq!(res.size(), before + masked_out.memory_bytes());
        res.shrink(res.size());

        // round(12.3456, 2), round(1234.0, -2), half-away-from-zero, NaN/+inf
        let xs = f64_col(&[
            Some(12.3456),
            Some(1234.0),
            Some(2.5),
            Some(-2.5),
            Some(f64::NAN),
            Some(f64::INFINITY),
            None,
        ]);
        let dps = i64_col(&[
            Some(2),
            Some(-2),
            Some(0),
            Some(0),
            Some(2),
            Some(-2),
            Some(2),
        ]);
        let rounded =
            eval_misc_fn(ScalarFn::Round, &[xs, dps], 7, None, &c, Some(&mut res)).unwrap();
        let rv = collect_f64s(&rounded);
        assert_eq!(rv[0], Some(12.35));
        assert_eq!(rv[1], Some(1200.0));
        assert_eq!(rv[2], Some(3.0));
        assert_eq!(rv[3], Some(-3.0));
        assert!(rv[4].unwrap().is_nan());
        assert_eq!(rv[5], Some(f64::INFINITY));
        assert_eq!(rv[6], None);

        // round on I64
        let ri = eval_misc_fn(
            ScalarFn::Round,
            &[
                i64_col(&[Some(1250), Some(-1250)]),
                i64_col(&[Some(-2), Some(-2)]),
            ],
            2,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(collect_i64s(&ri), vec![Some(1300), Some(-1300)]);

        // sqrt(-1.0) == NaN, ln(0.0) == -inf, log10(100.0) == 2.0, power
        let sq = eval_misc_fn(
            ScalarFn::Sqrt,
            &[f64_col(&[Some(-1.0), Some(9.0), None])],
            3,
            None,
            &c,
            None,
        )
        .unwrap();
        let sqv = collect_f64s(&sq);
        assert!(sqv[0].unwrap().is_nan());
        assert_eq!(sqv[1], Some(3.0));
        assert_eq!(sqv[2], None);

        let ln = eval_misc_fn(
            ScalarFn::Ln,
            &[f64_col(&[Some(0.0), Some(-1.0), Some(1.0)])],
            3,
            None,
            &c,
            None,
        )
        .unwrap();
        let lnv = collect_f64s(&ln);
        assert_eq!(lnv[0], Some(f64::NEG_INFINITY));
        assert!(lnv[1].unwrap().is_nan());
        assert_eq!(lnv[2], Some(0.0));

        let l10 = eval_misc_fn(
            ScalarFn::Log10,
            &[f64_col(&[Some(100.0), Some(0.0)])],
            2,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(collect_f64s(&l10), vec![Some(2.0), Some(f64::NEG_INFINITY)]);

        let pw = eval_misc_fn(
            ScalarFn::Power,
            &[
                f64_col(&[Some(2.0), Some(1.0)]),
                f64_col(&[Some(10.0), None]),
            ],
            2,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(collect_f64s(&pw), vec![Some(1024.0), None]);

        // floor / ceil
        let fl = eval_misc_fn(
            ScalarFn::Floor,
            &[f64_col(&[Some(2.9), Some(-2.1), None])],
            3,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(collect_f64s(&fl), vec![Some(2.0), Some(-3.0), None]);

        let ce = eval_misc_fn(
            ScalarFn::Ceil,
            &[f64_col(&[Some(2.1), Some(-2.9), None])],
            3,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(collect_f64s(&ce), vec![Some(3.0), Some(-2.0), None]);

        // isnan / isfinite returning NULL on NULL
        let f_vals = f64_col(&[Some(f64::NAN), Some(f64::INFINITY), Some(1.5), None]);
        let is_nan = eval_misc_fn(
            ScalarFn::IsNan,
            std::slice::from_ref(&f_vals),
            4,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(
            collect_bools(&is_nan),
            vec![Some(true), Some(false), Some(false), None]
        );
        let is_fin = eval_misc_fn(ScalarFn::IsFinite, &[f_vals], 4, None, &c, None).unwrap();
        assert_eq!(
            collect_bools(&is_fin),
            vec![Some(false), Some(false), Some(true), None]
        );

        // coalesce
        let coal = eval_misc_fn(
            ScalarFn::Coalesce,
            &[
                i64_col(&[None, Some(10), None]),
                i64_col(&[Some(20), Some(30), None]),
                i64_col(&[Some(40), None, None]),
            ],
            3,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(collect_i64s(&coal), vec![Some(20), Some(10), None]);

        // nullif with F64 f64_cmp (-0.0 == +0.0, NaN == NaN)
        let nif = eval_misc_fn(
            ScalarFn::Nullif,
            &[
                f64_col(&[Some(-0.0), Some(f64::NAN), Some(1.0), None, Some(5.0)]),
                f64_col(&[Some(0.0), Some(f64::NAN), Some(2.0), Some(1.0), None]),
            ],
            5,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(
            collect_f64s(&nif),
            vec![None, None, Some(1.0), None, Some(5.0)]
        );

        // greatest / least with nulls and NaNs (NaN > +inf)
        let ga = f64_col(&[Some(1.0), Some(f64::INFINITY), None, None]);
        let gb = f64_col(&[Some(5.0), Some(f64::NAN), Some(2.0), None]);
        let gr = eval_misc_fn(
            ScalarFn::Greatest,
            &[ga.clone(), gb.clone()],
            4,
            None,
            &c,
            None,
        )
        .unwrap();
        let grv = collect_f64s(&gr);
        assert_eq!(grv[0], Some(5.0));
        assert!(grv[1].unwrap().is_nan());
        assert_eq!(grv[2], Some(2.0));
        assert_eq!(grv[3], None);

        let le = eval_misc_fn(ScalarFn::Least, &[ga, gb], 4, None, &c, None).unwrap();
        assert_eq!(
            collect_f64s(&le),
            vec![Some(1.0), Some(f64::INFINITY), Some(2.0), None]
        );
    }

    #[test]
    fn array_list_concat_and_map_functions() {
        let c = ctx();
        let pool = GlobalPool::standalone(4096);
        let mut res = pool.register("test", false);

        // eval_list_concat: NULL || [1], [1] || [2], NULL || NULL
        let l1 = list_i64_col(&[None, Some(&[Some(1)]), None]);
        let l2 = list_i64_col(&[Some(&[Some(10)]), Some(&[Some(2), None]), None]);
        let before = res.size();
        let cat = eval_list_concat(&l1, &l2, None, &c, Some(&mut res)).unwrap();
        assert_eq!(res.size(), before + cat.memory_bytes());
        assert_eq!(
            collect_scalars(&cat),
            vec![
                ScalarValue::List {
                    item_type: DataType::I64,
                    values: vec![ScalarValue::I64(10)],
                },
                ScalarValue::List {
                    item_type: DataType::I64,
                    values: vec![
                        ScalarValue::I64(1),
                        ScalarValue::I64(2),
                        ScalarValue::Null(DataType::I64),
                    ],
                },
                ScalarValue::Null(DataType::List(Box::new(DataType::I64))),
            ]
        );
        res.shrink(res.size());

        // array_has(list, NULL) == NULL, array_has([1, NULL], 2) == false, array_has([1, NULL], 1) == true
        let lists = list_i64_col(&[
            Some(&[Some(1), None]),
            Some(&[Some(1), None]),
            Some(&[Some(1), Some(2)]),
            None,
        ]);
        let elems = i64_col(&[None, Some(2), Some(2), Some(1)]);
        let has = eval_misc_fn(
            ScalarFn::ArrayHas,
            &[lists.clone(), elems],
            4,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(
            collect_bools(&has),
            vec![None, Some(false), Some(true), None]
        );

        // array_has_all: empty sub -> true, NULL elem in sub requires NULL in list, NULL list/sub -> NULL
        let subs = list_i64_col(&[
            Some(&[]),
            Some(&[None, Some(1)]),
            Some(&[None, Some(1)]),
            Some(&[Some(1)]),
        ]);
        let has_all =
            eval_misc_fn(ScalarFn::ArrayHasAll, &[lists, subs], 4, None, &c, None).unwrap();
        assert_eq!(
            collect_bools(&has_all),
            vec![Some(true), Some(true), Some(false), None]
        );

        // array_length([]) == 0, NULL -> NULL, dim == 1 vs dim == 2
        let len_lists = list_i64_col(&[Some(&[]), Some(&[Some(1), Some(2)]), None]);
        let lens = eval_misc_fn(
            ScalarFn::ArrayLength,
            std::slice::from_ref(&len_lists),
            3,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(collect_i64s(&lens), vec![Some(0), Some(2), None]);
        let lens_dim = eval_misc_fn(
            ScalarFn::ArrayLength,
            &[len_lists, i64_col(&[Some(1), Some(2), Some(1)])],
            3,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(collect_i64s(&lens_dim), vec![Some(0), None, None]);

        // array_element: 1-based, -1 last, 0/OOB/NULL -> NULL
        let el_list = Column::try_new_const(
            ScalarValue::List {
                item_type: DataType::I64,
                values: vec![
                    ScalarValue::I64(10),
                    ScalarValue::I64(20),
                    ScalarValue::I64(30),
                ],
            },
            5,
        )
        .unwrap();
        let idxs = i64_col(&[Some(1), Some(-1), Some(0), Some(4), Some(-4)]);
        let el_out =
            eval_misc_fn(ScalarFn::ArrayElement, &[el_list, idxs], 5, None, &c, None).unwrap();
        assert_eq!(
            collect_i64s(&el_out),
            vec![Some(10), Some(30), None, None, None]
        );

        // array_to_string: skip NULLs vs null_str, non-Utf8 elems, tight Reservation -> TooLarge
        let ats_list = list_i64_col(&[Some(&[Some(1), None, Some(2)]), Some(&[]), None]);
        let delim = Column::try_new_const(ScalarValue::Utf8(",".into()), 3).unwrap();
        let ats1 = eval_misc_fn(
            ScalarFn::ArrayToString,
            &[ats_list.clone(), delim.clone()],
            3,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(
            collect_strs(&ats1),
            vec![Some("1,2".into()), Some("".into()), None]
        );
        let null_str = Column::try_new_const(ScalarValue::Utf8("NA".into()), 3).unwrap();
        let ats2 = eval_misc_fn(
            ScalarFn::ArrayToString,
            &[ats_list.clone(), delim, null_str],
            3,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(
            collect_strs(&ats2),
            vec![Some("1,NA,2".into()), Some("".into()), None]
        );

        // Tight reservation amplification check on array_to_string
        let big_delim = Column::try_new_const(ScalarValue::Utf8("x".repeat(3000)), 3).unwrap();
        let before = res.size();
        let err = eval_misc_fn(
            ScalarFn::ArrayToString,
            &[ats_list, big_delim],
            3,
            None,
            &c,
            Some(&mut res),
        )
        .unwrap_err();
        assert!(matches!(err, QueryError::TooLarge));
        assert_eq!(res.size(), before);

        // array_distinct: preserves first-seen order and deduplicates NULLs
        let dup_list = list_i64_col(&[
            Some(&[Some(1), None, Some(2), None, Some(1)]),
            Some(&[None, None]),
            None,
        ]);
        let distinct =
            eval_misc_fn(ScalarFn::ArrayDistinct, &[dup_list], 3, None, &c, None).unwrap();
        assert_eq!(
            collect_scalars(&distinct),
            vec![
                ScalarValue::List {
                    item_type: DataType::I64,
                    values: vec![
                        ScalarValue::I64(1),
                        ScalarValue::Null(DataType::I64),
                        ScalarValue::I64(2),
                    ],
                },
                ScalarValue::List {
                    item_type: DataType::I64,
                    values: vec![ScalarValue::Null(DataType::I64)],
                },
                ScalarValue::Null(DataType::List(Box::new(DataType::I64))),
            ]
        );

        // map_keys / map_values with null map values and Dict & Rle encodings
        let map_data = MapData::try_new(
            vec![0, 2, 2],
            Utf8Data::try_from_slices(["a", "b"]).unwrap(),
            Utf8Data::try_from_slices(["x", ""]).unwrap(),
            Some(Bitmap::from_bools(&[true, false])),
        )
        .unwrap();
        let flat_maps = FlatColumn::try_new(
            FlatData::MapUtf8(Box::new(map_data)),
            Some(Bitmap::from_bools(&[true, false])),
        )
        .unwrap();
        let dict_maps =
            Column::Dict(DictColumn::try_new(flat_maps.clone(), vec![0, 1, 0], None).unwrap());
        let rle_maps = Column::Rle(RleColumn::try_new(flat_maps, vec![2, 3]).unwrap());

        let mkeys = eval_misc_fn(ScalarFn::MapKeys, &[dict_maps], 3, None, &c, None).unwrap();
        assert_eq!(
            collect_scalars(&mkeys),
            vec![
                ScalarValue::List {
                    item_type: DataType::Utf8,
                    values: vec![ScalarValue::Utf8("a".into()), ScalarValue::Utf8("b".into())],
                },
                ScalarValue::Null(DataType::List(Box::new(DataType::Utf8))),
                ScalarValue::List {
                    item_type: DataType::Utf8,
                    values: vec![ScalarValue::Utf8("a".into()), ScalarValue::Utf8("b".into())],
                },
            ]
        );

        let mvals = eval_misc_fn(ScalarFn::MapValues, &[rle_maps], 3, None, &c, None).unwrap();
        assert_eq!(
            collect_scalars(&mvals),
            vec![
                ScalarValue::List {
                    item_type: DataType::Utf8,
                    values: vec![
                        ScalarValue::Utf8("x".into()),
                        ScalarValue::Null(DataType::Utf8),
                    ],
                },
                ScalarValue::List {
                    item_type: DataType::Utf8,
                    values: vec![
                        ScalarValue::Utf8("x".into()),
                        ScalarValue::Null(DataType::Utf8),
                    ],
                },
                ScalarValue::Null(DataType::List(Box::new(DataType::Utf8))),
            ]
        );
    }

    #[test]
    fn json_arrow_and_jsonpath_extraction() {
        let c = ctx();
        let pool = GlobalPool::standalone(1 << 20);
        let mut res = pool.register("test", false);

        // -> and ->> on object keys (including JSON null vs SQL NULL) and array indices
        let docs = utf8_col(&[
            Some(r#"{"name":"alice","nil":null,"num":42}"#),
            Some(r#"{"name":"bob","nil":null}"#),
            Some("not json"),
            None,
        ]);
        let key_name = Column::try_new_const(ScalarValue::Utf8("name".into()), 4).unwrap();
        let key_nil = Column::try_new_const(ScalarValue::Utf8("nil".into()), 4).unwrap();

        let get_name = eval_misc_fn(
            ScalarFn::JsonGet,
            &[docs.clone(), key_name.clone()],
            4,
            None,
            &c,
            Some(&mut res),
        )
        .unwrap();
        assert_eq!(
            collect_strs(&get_name),
            vec![Some("\"alice\"".into()), Some("\"bob\"".into()), None, None]
        );

        let text_name = eval_misc_fn(
            ScalarFn::JsonGetText,
            &[docs.clone(), key_name],
            4,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(
            collect_strs(&text_name),
            vec![Some("alice".into()), Some("bob".into()), None, None]
        );

        let get_nil = eval_misc_fn(
            ScalarFn::JsonGet,
            &[docs.clone(), key_nil.clone()],
            4,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(
            collect_strs(&get_nil),
            vec![Some("null".into()), Some("null".into()), None, None]
        );

        let text_nil =
            eval_misc_fn(ScalarFn::JsonGetText, &[docs, key_nil], 4, None, &c, None).unwrap();
        assert_eq!(collect_strs(&text_nil), vec![None, None, None, None]);

        // Array indexing with -> / ->>: negative index -> NULL
        let arr_docs =
            Column::try_new_const(ScalarValue::Utf8(r#"["zero",null,123]"#.into()), 4).unwrap();
        let idx_col = i64_col(&[Some(0), Some(1), Some(-1), Some(5)]);
        let arr_get = eval_misc_fn(
            ScalarFn::JsonGet,
            &[arr_docs.clone(), idx_col.clone()],
            4,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(
            collect_strs(&arr_get),
            vec![Some("\"zero\"".into()), Some("null".into()), None, None]
        );
        let arr_text = eval_misc_fn(
            ScalarFn::JsonGetText,
            &[arr_docs, idx_col],
            4,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(
            collect_strs(&arr_text),
            vec![Some("zero".into()), None, None, None]
        );

        // json_extract and json_extract_text on nested objects/arrays, single vs multiple vs none
        let nested = utf8_col(&[
            Some(r#"{"items":[{"id":"a"},{"id":"b"}],"meta":{"n":null}}"#),
            Some(r#"{"items":[]}"#),
            Some("bad{"),
        ]);
        let path_multi =
            Column::try_new_const(ScalarValue::Utf8("$.items[*].id".into()), 3).unwrap();
        let ex_multi = eval_misc_fn(
            ScalarFn::JsonExtract,
            &[nested.clone(), path_multi],
            3,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(
            collect_strs(&ex_multi),
            vec![Some(r#"["a","b"]"#.into()), None, None]
        );

        let path_single =
            Column::try_new_const(ScalarValue::Utf8("$.items[0].id".into()), 3).unwrap();
        let ex_single_json = eval_misc_fn(
            ScalarFn::JsonExtract,
            &[nested.clone(), path_single.clone()],
            3,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(
            collect_strs(&ex_single_json),
            vec![Some("\"a\"".into()), None, None]
        );
        let ex_single_text = eval_misc_fn(
            ScalarFn::JsonExtractText,
            &[nested.clone(), path_single],
            3,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(
            collect_strs(&ex_single_text),
            vec![Some("a".into()), None, None]
        );

        // Invalid JSONPath -> Failed, unless row is inactive
        let mixed_paths = utf8_col(&[Some("$[invalid"), Some("$.items[0].id"), Some("$.items[0]")]);
        let err = eval_misc_fn(
            ScalarFn::JsonExtract,
            &[nested.clone(), mixed_paths.clone()],
            3,
            None,
            &c,
            None,
        )
        .unwrap_err();
        assert!(matches!(err, QueryError::Failed(_)));

        let mask = Bitmap::from_bools(&[false, true, true]);
        let masked = eval_misc_fn(
            ScalarFn::JsonExtractText,
            &[nested, mixed_paths],
            3,
            Some(&mask),
            &c,
            None,
        )
        .unwrap();
        assert_eq!(collect_strs(&masked), vec![None, None, None]);

        // JSON null via json_extract vs json_extract_text
        let doc_with_null =
            Column::try_new_const(ScalarValue::Utf8(r#"{"meta":{"n":null}}"#.into()), 1).unwrap();
        let path_null = Column::try_new_const(ScalarValue::Utf8("$.meta.n".into()), 1).unwrap();
        let ex_json_null = eval_misc_fn(
            ScalarFn::JsonExtract,
            &[doc_with_null.clone(), path_null.clone()],
            1,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(collect_strs(&ex_json_null), vec![Some("null".into())]);
        let ex_text_null = eval_misc_fn(
            ScalarFn::JsonExtractText,
            &[doc_with_null, path_null],
            1,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(collect_strs(&ex_text_null), vec![None]);
    }

    #[test]
    fn nested_encoded_lists_f64_distinct_and_cancel() {
        let c = ctx();

        // List whose child `values` column is Dict-encoded Utf8
        let dict_vals = Column::Dict(
            DictColumn::try_new(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(["alpha", "beta"]).unwrap()),
                    None,
                )
                .unwrap(),
                vec![0, 1, 0, 1],
                Some(Bitmap::from_bools(&[true, true, false, true])),
            )
            .unwrap(),
        );
        let dict_list = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    item_type: DataType::Utf8,
                    offsets: vec![0, 2, 4],
                    values: Box::new(dict_vals),
                },
                None,
            )
            .unwrap(),
        );

        let delim = Column::try_new_const(ScalarValue::Utf8("|".into()), 2).unwrap();
        let ats = eval_misc_fn(
            ScalarFn::ArrayToString,
            &[dict_list.clone(), delim],
            2,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(
            collect_strs(&ats),
            vec![Some("alpha|beta".into()), Some("beta".into())]
        );

        // Rle list column concatenated with Const list column under active mask
        let rle_list = Column::Rle(
            RleColumn::try_new(
                match dict_list {
                    Column::Flat(fc) => fc,
                    _ => unreachable!(),
                },
                vec![1, 2],
            )
            .unwrap(),
        );
        let const_list = Column::try_new_const(
            ScalarValue::List {
                item_type: DataType::Utf8,
                values: vec![ScalarValue::Utf8("gamma".into())],
            },
            2,
        )
        .unwrap();
        let mask = Bitmap::from_bools(&[false, true]);
        let cat = eval_list_concat(&rle_list, &const_list, Some(&mask), &c, None).unwrap();
        assert_eq!(
            collect_scalars(&cat),
            vec![
                ScalarValue::Null(DataType::List(Box::new(DataType::Utf8))),
                ScalarValue::List {
                    item_type: DataType::Utf8,
                    values: vec![
                        ScalarValue::Null(DataType::Utf8),
                        ScalarValue::Utf8("beta".into()),
                        ScalarValue::Utf8("gamma".into()),
                    ],
                },
            ]
        );

        // ArrayDistinct with F64: -0.0 == +0.0 and NaN == NaN
        let f64_list = Column::try_new_const(
            ScalarValue::List {
                item_type: DataType::F64,
                values: vec![
                    ScalarValue::F64(-0.0),
                    ScalarValue::F64(f64::NAN),
                    ScalarValue::F64(0.0),
                    ScalarValue::F64(f64::NAN),
                    ScalarValue::Null(DataType::F64),
                    ScalarValue::Null(DataType::F64),
                ],
            },
            1,
        )
        .unwrap();
        let distinct_f =
            eval_misc_fn(ScalarFn::ArrayDistinct, &[f64_list], 1, None, &c, None).unwrap();
        let ScalarValue::List { values, .. } = distinct_f.get_scalar(0).unwrap() else {
            panic!("expected list");
        };
        assert_eq!(values.len(), 3);
        assert_eq!(values[0], ScalarValue::F64(-0.0));
        assert!(matches!(values[1], ScalarValue::F64(x) if x.is_nan()));
        assert_eq!(values[2], ScalarValue::Null(DataType::F64));

        // Nested list array_to_string
        let nested_list = Column::try_new_const(
            ScalarValue::List {
                item_type: DataType::List(Box::new(DataType::I64)),
                values: vec![
                    ScalarValue::List {
                        item_type: DataType::I64,
                        values: vec![ScalarValue::I64(1), ScalarValue::I64(2)],
                    },
                    ScalarValue::List {
                        item_type: DataType::I64,
                        values: vec![ScalarValue::I64(3)],
                    },
                ],
            },
            1,
        )
        .unwrap();
        let comma = Column::try_new_const(ScalarValue::Utf8(",".into()), 1).unwrap();
        let nested_ats = eval_misc_fn(
            ScalarFn::ArrayToString,
            &[nested_list, comma],
            1,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(collect_strs(&nested_ats), vec![Some("1,2,3".into())]);

        // Cancellation returns Cancelled and rolls back Reservation
        let cancelled = EvalContext {
            now_ns: 0,
            cancel: CancelToken::new(),
        };
        cancelled.cancel.cancel();
        let pool = GlobalPool::standalone(1 << 20);
        let mut res = pool.register("cancel", false);
        let before = res.size();
        let err = eval_misc_fn(
            ScalarFn::Abs,
            &[i64_col(&[Some(-1)])],
            1,
            None,
            &cancelled,
            Some(&mut res),
        )
        .unwrap_err();
        assert!(matches!(err, QueryError::Cancelled));
        assert_eq!(res.size(), before);
    }

    #[test]
    fn json_recursive_oom_array_hash_and_reservation_guards() {
        let c = ctx();

        // Blocker 3e & Minor 6: $..* on nested JSON stops immediately on tight pool with TooLarge,
        // and len == 0 skips const_parsed, and dynamic path_col uses 1-entry cache.
        let nested_doc = r#"{"a":{"b":{"c":{"d":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}}}}"#;
        let json_col = Column::try_new_const(ScalarValue::Utf8(nested_doc.into()), 4).unwrap();
        let path_col = Column::try_new_const(ScalarValue::Utf8("$..*".into()), 4).unwrap();
        let tight_pool = GlobalPool::standalone(128);
        let mut tight_res = tight_pool.register("json_oom", false);
        tight_res.try_grow(16).unwrap();
        let err = eval_misc_fn(
            ScalarFn::JsonExtract,
            &[json_col, path_col],
            4,
            None,
            &c,
            Some(&mut tight_res),
        )
        .unwrap_err();
        assert!(matches!(err, QueryError::TooLarge));
        assert_eq!(tight_res.size(), 16);

        let empty_json = Column::try_new_const(ScalarValue::Utf8("not-json".into()), 0).unwrap();
        let empty_path = Column::try_new_const(ScalarValue::Utf8("$.a".into()), 0).unwrap();
        let empty_out = eval_misc_fn(
            ScalarFn::JsonExtract,
            &[empty_json, empty_path],
            0,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(empty_out.len(), 0);

        let dyn_json = utf8_col(&[Some(r#"{"a":1,"b":2}"#), Some(r#"{"a":3,"b":4}"#)]);
        let dyn_paths = utf8_col(&[Some("$.a"), Some("$.a")]);
        let dyn_out = eval_misc_fn(
            ScalarFn::JsonExtract,
            &[dyn_json, dyn_paths],
            2,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(
            collect_strs(&dyn_out),
            vec![Some("1".into()), Some("3".into())]
        );

        // Major 5: array_has_all HashSet fast path (>64 product) + Decimal scale mismatch fallback + cancel + OOM.
        let big_vals: Vec<ScalarValue> = (0..100).map(ScalarValue::I64).collect();
        let sub_vals: Vec<ScalarValue> = (50..60).map(ScalarValue::I64).collect();
        let big_list = Column::try_new_const(
            ScalarValue::List {
                item_type: DataType::I64,
                values: big_vals,
            },
            1,
        )
        .unwrap();
        let sub_list = Column::try_new_const(
            ScalarValue::List {
                item_type: DataType::I64,
                values: sub_vals,
            },
            1,
        )
        .unwrap();
        let has_all = eval_misc_fn(
            ScalarFn::ArrayHasAll,
            &[big_list.clone(), sub_list.clone()],
            1,
            None,
            &c,
            None,
        )
        .unwrap();
        assert_eq!(read_bool(&has_all, 0), Some(true));

        let hs_pool = GlobalPool::standalone(256);
        let mut hs_res = hs_pool.register("has_all_oom", false);
        assert!(matches!(
            eval_misc_fn(
                ScalarFn::ArrayHasAll,
                &[big_list.clone(), sub_list.clone()],
                1,
                None,
                &c,
                Some(&mut hs_res),
            )
            .unwrap_err(),
            QueryError::TooLarge
        ));
        assert_eq!(hs_res.size(), 0);

        // Different Decimal scales: [1.00 (scale 2)] @> [1 (scale 0)] -> true
        let dec_l = Column::try_new_const(
            ScalarValue::List {
                item_type: DataType::Decimal128 {
                    precision: 10,
                    scale: 2,
                },
                values: vec![ScalarValue::Decimal128 {
                    value: 100,
                    precision: 10,
                    scale: 2,
                }],
            },
            1,
        )
        .unwrap();
        let dec_r = Column::try_new_const(
            ScalarValue::List {
                item_type: DataType::Decimal128 {
                    precision: 10,
                    scale: 0,
                },
                values: vec![ScalarValue::Decimal128 {
                    value: 1,
                    precision: 10,
                    scale: 0,
                }],
            },
            1,
        )
        .unwrap();
        let dec_has =
            eval_misc_fn(ScalarFn::ArrayHasAll, &[dec_l, dec_r], 1, None, &c, None).unwrap();
        assert_eq!(read_bool(&dec_has, 0), Some(true));

        // Blocker 3b & Major 5: list_concat / array_distinct OOM rollback and cancel.
        let concat_pool = GlobalPool::standalone(256);
        let mut concat_res = concat_pool.register("concat_oom", false);
        assert!(matches!(
            eval_list_concat(&big_list, &big_list, None, &c, Some(&mut concat_res)).unwrap_err(),
            QueryError::TooLarge
        ));
        assert_eq!(concat_res.size(), 0);

        let dist_pool = GlobalPool::standalone(256);
        let mut dist_res = dist_pool.register("distinct_oom", false);
        assert!(matches!(
            eval_misc_fn(
                ScalarFn::ArrayDistinct,
                std::slice::from_ref(&big_list),
                1,
                None,
                &c,
                Some(&mut dist_res),
            )
            .unwrap_err(),
            QueryError::TooLarge
        ));
        assert_eq!(dist_res.size(), 0);

        // CancelToken inside list_ref_has_all and collect_distinct_elems
        let cancelled = EvalContext {
            now_ns: 0,
            cancel: CancelToken::new(),
        };
        cancelled.cancel.cancel();
        assert!(matches!(
            eval_misc_fn(
                ScalarFn::ArrayHasAll,
                &[big_list.clone(), sub_list],
                1,
                None,
                &cancelled,
                None,
            )
            .unwrap_err(),
            QueryError::Cancelled
        ));
        assert!(matches!(
            eval_misc_fn(
                ScalarFn::ArrayDistinct,
                &[big_list],
                1,
                None,
                &cancelled,
                None,
            )
            .unwrap_err(),
            QueryError::Cancelled
        ));

        // Blocker 3e: array_to_string active_payload_bytes OOM rollback
        let payload_list = Column::try_new_const(
            ScalarValue::List {
                item_type: DataType::Utf8,
                values: vec![ScalarValue::Utf8("z".repeat(500))],
            },
            2,
        )
        .unwrap();
        let comma = Column::try_new_const(ScalarValue::Utf8(",".into()), 2).unwrap();
        let ats_pool = GlobalPool::standalone(256);
        let mut ats_res = ats_pool.register("ats_payload_oom", false);
        assert!(matches!(
            eval_misc_fn(
                ScalarFn::ArrayToString,
                &[payload_list, comma],
                2,
                None,
                &c,
                Some(&mut ats_res),
            )
            .unwrap_err(),
            QueryError::TooLarge
        ));
        assert_eq!(ats_res.size(), 0);
    }
}
