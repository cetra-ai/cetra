//! Vectorized expression evaluation over `cetra_column::Batch`.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashSet;

use cetra_column::{
    Batch, BinaryData, Bitmap, Column, ColumnError, DataType, EventItem, EventsData, FlatColumn,
    FlatData, Interval, LinkItem, LinksData, MapData, Reservation, ScalarValue, Selection,
    Utf8Data,
};
use chrono::NaiveDate;

use crate::error::QueryError;
use crate::ir::{BinaryOp, CastMode, EvalContext, Expr, ScalarFn, UnaryOp};

const CANCEL_CHECK_MASK: usize = 1023;
const NS_PER_DAY: i64 = 86_400_000_000_000;
const UNIX_EPOCH_CE_DAYS: i32 = 719_163;

/// Evaluates `expr` over `batch`, always returning a `Column` of length `batch.raw_len()`.
///
/// Inactive rows under `batch.selection()` are masked out (never fault) and filled with
/// canonical nulls. On `Ok(col)`, `pool` is reconciled to `before + col.memory_bytes()`;
/// on `Err(_)`, `pool` is rolled back to `before`.
pub fn eval_expr(
    expr: &Expr,
    batch: &Batch,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = pool_size(&pool);
    let res = eval_expr_root(expr, batch, ctx, pool.as_deref_mut());
    match res {
        Ok(col) => {
            reconcile_output(&mut pool, before, col.memory_bytes())?;
            Ok(col)
        }
        Err(err) => {
            rollback_to(&mut pool, before);
            Err(err)
        }
    }
}

/// Evaluates boolean/null predicate `expr` over `batch`, returning a `Bitmap` of length `batch.raw_len()`.
///
/// Bit `1` means the row is active under `batch.selection()` AND `expr` evaluated to `Some(true)`.
/// Rejects non-boolean/non-null expressions with `QueryError::Failed`.
pub fn eval_filter(
    expr: &Expr,
    batch: &Batch,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Bitmap, QueryError> {
    if !matches!(expr.data_type(), DataType::Bool | DataType::Null) {
        return Err(QueryError::failed(
            "filter predicate must evaluate to boolean",
        ));
    }
    let before = pool_size(&pool);
    check_cancel(ctx, &mut pool, before)?;

    let raw_len = batch.raw_len();
    let active = match selection_active_bitmap(batch, &mut pool) {
        Ok(bm) => bm,
        Err(err) => {
            rollback_to(&mut pool, before);
            return Err(err);
        }
    };

    let col = match eval_expr_masked(expr, batch, active.as_deref(), ctx, pool.as_deref_mut()) {
        Ok(c) => c,
        Err(err) => {
            rollback_to(&mut pool, before);
            return Err(err);
        }
    };

    let bitmap_bytes = bitmap_mem_bytes(raw_len);
    if let Err(err) = try_reserve(&mut pool, bitmap_bytes) {
        rollback_to(&mut pool, before);
        return Err(err);
    }

    let mut out = Bitmap::new_zeroed(raw_len);
    for row in 0..raw_len {
        if (row & CANCEL_CHECK_MASK) == 0
            && let Err(err) = check_cancel(ctx, &mut pool, before)
        {
            return Err(err);
        }
        if is_active(active.as_deref(), row) && read_bool(&col, row) == Some(true) {
            let _ = out.set(row, true);
        }
    }

    drop(col);
    drop(active);
    reconcile_output(&mut pool, before, out.memory_bytes())?;
    Ok(out)
}

fn eval_expr_root(
    expr: &Expr,
    batch: &Batch,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = pool_size(&pool);
    check_cancel(ctx, &mut pool, before)?;
    let active = selection_active_bitmap(batch, &mut pool)?;
    eval_expr_masked(expr, batch, active.as_deref(), ctx, pool)
}

fn selection_active_bitmap<'a>(
    batch: &'a Batch,
    pool: &mut Option<&mut Reservation>,
) -> Result<Option<Cow<'a, Bitmap>>, QueryError> {
    let raw_len = batch.raw_len();
    batch.selection().validate(raw_len)?;
    match batch.selection() {
        Selection::All => Ok(None),
        Selection::Mask(bm) => Ok(Some(Cow::Borrowed(bm))),
        Selection::Indices(idx) => {
            try_reserve(pool, bitmap_mem_bytes(raw_len))?;
            let mut bm = Bitmap::new_zeroed(raw_len);
            for &i in idx {
                let _ = bm.set(i as usize, true);
            }
            Ok(Some(Cow::Owned(bm)))
        }
    }
}

fn eval_expr_masked(
    expr: &Expr,
    batch: &Batch,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = pool_size(&pool);
    check_cancel(ctx, &mut pool, before)?;

    let raw_len = batch.raw_len();
    if active.is_some_and(Bitmap::all_zeros) {
        let out = Column::Const {
            value: ScalarValue::Null(expr.data_type()),
            len: raw_len,
        };
        reconcile_output(&mut pool, before, out.memory_bytes())?;
        return Ok(out);
    }

    let res = match expr {
        Expr::Column {
            index, data_type, ..
        } => {
            let col = batch.column(*index).ok_or(ColumnError::InvalidBatch {
                reason: "column index out of bounds",
            })?;
            if col.data_type() != *data_type {
                return Err(ColumnError::InvalidBatch {
                    reason: "column data type mismatch",
                }
                .into());
            }
            Ok(col.clone())
        }
        Expr::Literal(val) => {
            let col = Column::try_new_const(val.clone(), raw_len)?;
            Ok(col)
        }
        Expr::Unary {
            op,
            expr: inner,
            data_type,
        } => eval_unary(
            *op,
            inner,
            data_type,
            batch,
            active,
            ctx,
            pool.as_deref_mut(),
        ),
        Expr::Binary {
            left,
            op,
            right,
            data_type,
        } => eval_binary(
            left,
            *op,
            right,
            data_type,
            batch,
            active,
            ctx,
            pool.as_deref_mut(),
        ),
        Expr::Cast {
            expr: inner,
            target,
            mode,
        } => eval_cast(
            inner,
            target,
            *mode,
            batch,
            active,
            ctx,
            pool.as_deref_mut(),
        ),
        Expr::Between {
            expr: inner,
            low,
            high,
            negated,
        } => eval_between(
            inner,
            low,
            high,
            *negated,
            batch,
            active,
            ctx,
            pool.as_deref_mut(),
        ),
        Expr::InList {
            expr: inner,
            list,
            negated,
            ..
        } => eval_in_list(
            inner,
            list,
            *negated,
            batch,
            active,
            ctx,
            pool.as_deref_mut(),
        ),
        Expr::Case {
            operand,
            when_then,
            else_expr,
            data_type,
        } => eval_case(
            operand.as_deref(),
            when_then,
            else_expr.as_deref(),
            data_type,
            batch,
            active,
            ctx,
            pool.as_deref_mut(),
        ),
        Expr::MapIndex { map, key } => {
            eval_map_index(map, key, batch, active, ctx, pool.as_deref_mut())
        }
        Expr::MakeArray { elems, item_type } => {
            eval_make_array(elems, item_type, batch, active, ctx, pool.as_deref_mut())
        }
        Expr::Like {
            expr: inner,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => {
            let val_col = eval_expr_masked(inner, batch, active, ctx, pool.as_deref_mut())?;
            let pat_col = eval_expr_masked(pattern, batch, active, ctx, pool.as_deref_mut())?;
            crate::fn_string::eval_like(
                &val_col,
                &pat_col,
                *escape,
                *case_insensitive,
                *negated,
                active,
                ctx,
                pool.as_deref_mut(),
            )
        }
        Expr::Call { func, args, .. } => {
            eval_call(*func, args, batch, active, ctx, pool.as_deref_mut())
        }
    };

    match res {
        Ok(col) => {
            reconcile_output(&mut pool, before, col.memory_bytes())?;
            Ok(col)
        }
        Err(err) => {
            rollback_to(&mut pool, before);
            Err(err)
        }
    }
}

// ---------------------------------------------------------------------------
// Shared pub(crate) reservation, masking, cancel, float, and reader helpers
// ---------------------------------------------------------------------------

pub(crate) fn pool_size(pool: &Option<&mut Reservation>) -> usize {
    pool.as_deref().map_or(0, Reservation::size)
}

pub(crate) fn try_reserve(
    pool: &mut Option<&mut Reservation>,
    bytes: usize,
) -> Result<(), QueryError> {
    if let Some(res) = pool.as_deref_mut() {
        res.try_grow(bytes)?;
    }
    Ok(())
}

pub(crate) fn rollback_to(pool: &mut Option<&mut Reservation>, before: usize) {
    if let Some(res) = pool.as_deref_mut() {
        let cur = res.size();
        if cur > before {
            res.shrink(cur - before);
        }
    }
}

pub(crate) fn reconcile_output(
    pool: &mut Option<&mut Reservation>,
    before: usize,
    actual_bytes: usize,
) -> Result<(), QueryError> {
    let Some(res) = pool.as_deref_mut() else {
        return Ok(());
    };
    let Some(target) = before.checked_add(actual_bytes) else {
        let cur = res.size();
        if cur > before {
            res.shrink(cur - before);
        }
        return Err(QueryError::TooLarge);
    };
    let cur = res.size();
    if target > cur {
        if let Err(err) = res.try_grow(target - cur) {
            let cur_after = res.size();
            if cur_after > before {
                res.shrink(cur_after - before);
            }
            return Err(err.into());
        }
    } else if target < cur {
        res.shrink(cur - target);
    }
    Ok(())
}

pub(crate) fn is_active(active: Option<&Bitmap>, row: usize) -> bool {
    active.is_none_or(|bm| bm.get(row))
}

pub(crate) fn active_count(active: Option<&Bitmap>, len: usize) -> usize {
    active.map_or(len, Bitmap::count_ones)
}

pub(crate) fn check_cancel(
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<(), QueryError> {
    if ctx.cancel.is_cancelled() {
        rollback_to(pool, before);
        return Err(QueryError::Cancelled);
    }
    Ok(())
}

pub(crate) fn f64_canonical_bits(v: f64) -> u64 {
    if v.is_nan() {
        f64::NAN.to_bits()
    } else if v == 0.0 {
        0.0f64.to_bits()
    } else {
        v.to_bits()
    }
}

pub(crate) fn f64_cmp(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            let ca = if a == 0.0 { 0.0 } else { a };
            let cb = if b == 0.0 { 0.0 } else { b };
            ca.total_cmp(&cb)
        }
    }
}

/// Zero-copy borrowed slice of a list element at one row.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ListRef<'a> {
    Flat {
        values: &'a Column,
        start: usize,
        end: usize,
    },
    Const(&'a [ScalarValue]),
}

/// Zero-copy borrowed view of a map at one row.
#[derive(Debug, Clone, Copy)]
pub(crate) enum MapRef<'a> {
    Flat {
        data: &'a MapData,
        start: usize,
        end: usize,
    },
    Const(&'a [(String, Option<String>)]),
}

fn resolve_flat(col: &Column, row: usize) -> Option<(&FlatColumn, usize)> {
    match col {
        Column::Flat(fc) => (row < fc.len() && !fc.is_null(row)).then_some((fc, row)),
        Column::Const { .. } => None,
        Column::Dict(d) => {
            if row >= d.len() || d.nulls().is_some_and(|bm| !bm.get(row)) {
                return None;
            }
            let code = d.codes()[row] as usize;
            (!d.values().is_null(code)).then_some((d.values(), code))
        }
        Column::Rle(r) => {
            if row >= r.len() {
                return None;
            }
            let run_idx = r.run_ends().partition_point(|&end| (end as usize) <= row);
            (!r.values().is_null(run_idx)).then_some((r.values(), run_idx))
        }
    }
}

pub(crate) fn read_bool(col: &Column, row: usize) -> Option<bool> {
    if let Column::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|v| match v {
            ScalarValue::Bool(b) => Some(*b),
            _ => None,
        });
    }
    let (fc, flat_row) = resolve_flat(col, row)?;
    match fc.data() {
        FlatData::Bool(bm) => Some(bm.get(flat_row)),
        _ => None,
    }
}

pub(crate) fn read_i64(col: &Column, row: usize) -> Option<i64> {
    if let Column::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|v| match v {
            ScalarValue::I64(n) => Some(*n),
            _ => None,
        });
    }
    let (fc, flat_row) = resolve_flat(col, row)?;
    match fc.data() {
        FlatData::I64(v) => Some(v[flat_row]),
        _ => None,
    }
}

pub(crate) fn read_f64(col: &Column, row: usize) -> Option<f64> {
    if let Column::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|v| match v {
            ScalarValue::F64(x) => Some(*x),
            _ => None,
        });
    }
    let (fc, flat_row) = resolve_flat(col, row)?;
    match fc.data() {
        FlatData::F64(v) => Some(v[flat_row]),
        _ => None,
    }
}

pub(crate) fn read_decimal(col: &Column, row: usize) -> Option<i128> {
    if let Column::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|v| match v {
            ScalarValue::Decimal128 { value, .. } => Some(*value),
            _ => None,
        });
    }
    let (fc, flat_row) = resolve_flat(col, row)?;
    match fc.data() {
        FlatData::Decimal128 { values, .. } => Some(values[flat_row]),
        _ => None,
    }
}

pub(crate) fn read_timestamp(col: &Column, row: usize) -> Option<i64> {
    if let Column::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|v| match v {
            ScalarValue::TimestampNs { value, .. } => Some(*value),
            _ => None,
        });
    }
    let (fc, flat_row) = resolve_flat(col, row)?;
    match fc.data() {
        FlatData::TimestampNs { values, .. } => Some(values[flat_row]),
        _ => None,
    }
}

pub(crate) fn read_date(col: &Column, row: usize) -> Option<i32> {
    if let Column::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|v| match v {
            ScalarValue::Date(d) => Some(*d),
            _ => None,
        });
    }
    let (fc, flat_row) = resolve_flat(col, row)?;
    match fc.data() {
        FlatData::Date(v) => Some(v[flat_row]),
        _ => None,
    }
}

pub(crate) fn read_interval(col: &Column, row: usize) -> Option<Interval> {
    if let Column::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|v| match v {
            ScalarValue::Interval(iv) => Some(*iv),
            _ => None,
        });
    }
    let (fc, flat_row) = resolve_flat(col, row)?;
    match fc.data() {
        FlatData::Interval(v) => Some(v[flat_row]),
        _ => None,
    }
}

pub(crate) fn read_str(col: &Column, row: usize) -> Option<&str> {
    if let Column::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|v| match v {
            ScalarValue::Utf8(s) => Some(s.as_str()),
            _ => None,
        });
    }
    let (fc, flat_row) = resolve_flat(col, row)?;
    match fc.data() {
        FlatData::Utf8(u) => u.get(flat_row),
        _ => None,
    }
}

pub(crate) fn read_bytes(col: &Column, row: usize) -> Option<&[u8]> {
    if let Column::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|v| match v {
            ScalarValue::Binary(b) => Some(b.as_slice()),
            ScalarValue::FixedBytes8(b) => Some(b.as_slice()),
            ScalarValue::FixedBytes16(b) => Some(b.as_slice()),
            _ => None,
        });
    }
    let (fc, flat_row) = resolve_flat(col, row)?;
    match fc.data() {
        FlatData::Binary(b) => b.get(flat_row),
        FlatData::FixedBytes8(v) => Some(v[flat_row].as_slice()),
        FlatData::FixedBytes16(v) => Some(v[flat_row].as_slice()),
        _ => None,
    }
}

pub(crate) fn read_scalar(col: &Column, row: usize) -> ScalarValue {
    col.get_scalar(row)
        .unwrap_or_else(|_| ScalarValue::Null(col.data_type()))
}

pub(crate) fn list_ref_at(col: &Column, row: usize) -> Option<ListRef<'_>> {
    if let Column::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|v| match v {
            ScalarValue::List { values, .. } => Some(ListRef::Const(values.as_slice())),
            _ => None,
        });
    }
    let (fc, flat_row) = resolve_flat(col, row)?;
    match fc.data() {
        FlatData::List {
            offsets, values, ..
        } => {
            let start = offsets[flat_row] as usize;
            let end = offsets[flat_row + 1] as usize;
            Some(ListRef::Flat { values, start, end })
        }
        _ => None,
    }
}

pub(crate) fn map_ref_at(col: &Column, row: usize) -> Option<MapRef<'_>> {
    if let Column::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|v| match v {
            ScalarValue::MapUtf8(entries) => Some(MapRef::Const(entries.as_slice())),
            _ => None,
        });
    }
    let (fc, flat_row) = resolve_flat(col, row)?;
    match fc.data() {
        FlatData::MapUtf8(data) => {
            let start = data.offsets()[flat_row] as usize;
            let end = data.offsets()[flat_row + 1] as usize;
            Some(MapRef::Flat { data, start, end })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Core expression implementations
// ---------------------------------------------------------------------------

fn eval_call(
    func: ScalarFn,
    args: &[Expr],
    batch: &Batch,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let mut arg_cols = Vec::with_capacity(args.len());
    for arg in args {
        arg_cols.push(eval_expr_masked(
            arg,
            batch,
            active,
            ctx,
            pool.as_deref_mut(),
        )?);
    }
    let len = batch.raw_len();
    match func {
        ScalarFn::Lower
        | ScalarFn::Upper
        | ScalarFn::Length
        | ScalarFn::Substr
        | ScalarFn::Trim
        | ScalarFn::Ltrim
        | ScalarFn::Rtrim
        | ScalarFn::Left
        | ScalarFn::Right
        | ScalarFn::SplitPart
        | ScalarFn::Concat
        | ScalarFn::ConcatWs
        | ScalarFn::StartsWith
        | ScalarFn::EndsWith
        | ScalarFn::Contains
        | ScalarFn::Strpos
        | ScalarFn::RegexpLike
        | ScalarFn::RegexpExtract
        | ScalarFn::Encode => {
            crate::fn_string::eval_string_fn(func, &arg_cols, len, active, ctx, pool)
        }
        ScalarFn::Now
        | ScalarFn::DateTrunc
        | ScalarFn::DateBin
        | ScalarFn::Extract
        | ScalarFn::ToTimestamp => {
            crate::fn_time::eval_time_fn(func, &arg_cols, len, active, ctx, pool)
        }
        ScalarFn::Abs
        | ScalarFn::Round
        | ScalarFn::Floor
        | ScalarFn::Ceil
        | ScalarFn::Sqrt
        | ScalarFn::Ln
        | ScalarFn::Log10
        | ScalarFn::Power
        | ScalarFn::Coalesce
        | ScalarFn::Nullif
        | ScalarFn::Greatest
        | ScalarFn::Least
        | ScalarFn::IsNan
        | ScalarFn::IsFinite
        | ScalarFn::ArrayHas
        | ScalarFn::ArrayHasAll
        | ScalarFn::ArrayLength
        | ScalarFn::ArrayElement
        | ScalarFn::ArrayToString
        | ScalarFn::ArrayDistinct
        | ScalarFn::MapKeys
        | ScalarFn::MapValues
        | ScalarFn::JsonGet
        | ScalarFn::JsonGetText
        | ScalarFn::JsonExtract
        | ScalarFn::JsonExtractText => {
            crate::fn_misc::eval_misc_fn(func, &arg_cols, len, active, ctx, pool)
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal timestamp-interval helper preserves operands, target type, mask, ctx, pool"
)]
fn eval_timestamp_interval(
    op: BinaryOp,
    lcol: &Column,
    rcol: &Column,
    data_type: &DataType,
    raw_len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = pool_size(pool);
    let utc = match data_type {
        DataType::TimestampNs { utc } => *utc,
        _ => match (lcol.data_type(), rcol.data_type()) {
            (DataType::TimestampNs { utc }, _) | (_, DataType::TimestampNs { utc }) => utc,
            _ => true,
        },
    };
    if matches!(lcol.data_type(), DataType::Null) || matches!(rcol.data_type(), DataType::Null) {
        return Ok(Column::Const {
            value: ScalarValue::Null(DataType::TimestampNs { utc }),
            len: raw_len,
        });
    }
    let (ts_col, iv_col, subtract) = match (lcol.data_type(), rcol.data_type(), op) {
        (DataType::TimestampNs { .. }, DataType::Interval, BinaryOp::Add) => (lcol, rcol, false),
        (DataType::TimestampNs { .. }, DataType::Interval, BinaryOp::Sub) => (lcol, rcol, true),
        (DataType::Interval, DataType::TimestampNs { .. }, BinaryOp::Add) => (rcol, lcol, false),
        _ => {
            return Err(QueryError::failed(
                "unsupported timestamp and interval arithmetic",
            ));
        }
    };

    try_reserve(pool, raw_len * 8 + bitmap_mem_bytes(raw_len))?;
    let mut vals = vec![0i64; raw_len];
    let mut nulls = Bitmap::new_zeroed(raw_len);
    for (row, slot) in vals.iter_mut().enumerate() {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        if let (Some(ts), Some(iv)) = (read_timestamp(ts_col, row), read_interval(iv_col, row)) {
            *slot = crate::fn_time::add_interval(ts, iv, subtract)?;
            let _ = nulls.set(row, true);
        }
    }
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::TimestampNs { values: vals, utc },
        Some(nulls),
    )?))
}

fn bitmap_mem_bytes(len: usize) -> usize {
    len.div_ceil(64) * 8
}

fn build_bool_column(
    raw_len: usize,
    values: Bitmap,
    nulls: Option<Bitmap>,
) -> Result<Column, QueryError> {
    let fc = FlatColumn::try_new(FlatData::Bool(values), nulls)?;
    debug_assert_eq!(fc.len(), raw_len);
    Ok(Column::Flat(fc))
}

fn eval_unary(
    op: UnaryOp,
    inner: &Expr,
    data_type: &DataType,
    batch: &Batch,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = pool_size(&pool);
    let raw_len = batch.raw_len();
    let child = eval_expr_masked(inner, batch, active, ctx, pool.as_deref_mut())?;

    match op {
        UnaryOp::Not => {
            if matches!(child.data_type(), DataType::Null) {
                return Ok(Column::Const {
                    value: ScalarValue::Null(data_type.clone()),
                    len: raw_len,
                });
            }
            if active.is_none()
                && let Column::Const { value, len } = &child
            {
                let out_val = match value {
                    ScalarValue::Bool(b) => ScalarValue::Bool(!b),
                    ScalarValue::Null(dt) => ScalarValue::Null(dt.clone()),
                    _ => return Err(QueryError::failed("NOT requires boolean operand")),
                };
                return Ok(Column::Const {
                    value: out_val,
                    len: *len,
                });
            }
            let bm_bytes = bitmap_mem_bytes(raw_len);
            try_reserve(&mut pool, bm_bytes * 2)?;
            let mut vals = Bitmap::new_zeroed(raw_len);
            let mut nulls = Bitmap::new_zeroed(raw_len);
            for row in 0..raw_len {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, &mut pool, before)?;
                }
                if is_active(active, row)
                    && let Some(b) = read_bool(&child, row)
                {
                    let _ = nulls.set(row, true);
                    if !b {
                        let _ = vals.set(row, true);
                    }
                }
            }
            build_bool_column(raw_len, vals, Some(nulls))
        }
        UnaryOp::IsNull
        | UnaryOp::IsNotNull
        | UnaryOp::IsTrue
        | UnaryOp::IsFalse
        | UnaryOp::IsNotTrue
        | UnaryOp::IsNotFalse => {
            let bm_bytes = bitmap_mem_bytes(raw_len);
            let need_nulls = active.is_some();
            try_reserve(&mut pool, bm_bytes * if need_nulls { 2 } else { 1 })?;
            let mut vals = Bitmap::new_zeroed(raw_len);
            let mut nulls = need_nulls.then(|| Bitmap::new_zeroed(raw_len));
            for row in 0..raw_len {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, &mut pool, before)?;
                }
                if !is_active(active, row) {
                    continue;
                }
                if let Some(n) = nulls.as_mut() {
                    let _ = n.set(row, true);
                }
                let result = match op {
                    UnaryOp::IsNull => child.is_null(row),
                    UnaryOp::IsNotNull => !child.is_null(row),
                    UnaryOp::IsTrue => read_bool(&child, row) == Some(true),
                    UnaryOp::IsFalse => read_bool(&child, row) == Some(false),
                    UnaryOp::IsNotTrue => read_bool(&child, row) != Some(true),
                    UnaryOp::IsNotFalse => read_bool(&child, row) != Some(false),
                    UnaryOp::Not | UnaryOp::Neg => false,
                };
                if result {
                    let _ = vals.set(row, true);
                }
            }
            build_bool_column(raw_len, vals, nulls)
        }
        UnaryOp::Neg => eval_neg(&child, data_type, raw_len, active, ctx, &mut pool, before),
    }
}

fn eval_neg(
    child: &Column,
    data_type: &DataType,
    raw_len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    match child.data_type() {
        DataType::Null => Ok(Column::Const {
            value: ScalarValue::Null(data_type.clone()),
            len: raw_len,
        }),
        DataType::I64 => {
            try_reserve(pool, raw_len * 8 + bitmap_mem_bytes(raw_len))?;
            let mut vals = vec![0i64; raw_len];
            let mut nulls = Bitmap::new_zeroed(raw_len);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if is_active(active, row)
                    && let Some(v) = read_i64(child, row)
                {
                    *slot = v
                        .checked_neg()
                        .ok_or_else(|| QueryError::failed("integer negation overflow"))?;
                    let _ = nulls.set(row, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::I64(vals),
                Some(nulls),
            )?))
        }
        DataType::F64 => {
            try_reserve(pool, raw_len * 8 + bitmap_mem_bytes(raw_len))?;
            let mut vals = vec![0.0f64; raw_len];
            let mut nulls = Bitmap::new_zeroed(raw_len);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if is_active(active, row)
                    && let Some(v) = read_f64(child, row)
                {
                    *slot = -v;
                    let _ = nulls.set(row, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::F64(vals),
                Some(nulls),
            )?))
        }
        DataType::Decimal128 { precision, scale } => {
            try_reserve(pool, raw_len * 16 + bitmap_mem_bytes(raw_len))?;
            let mut vals = vec![0i128; raw_len];
            let mut nulls = Bitmap::new_zeroed(raw_len);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if is_active(active, row)
                    && let Some(v) = read_decimal(child, row)
                {
                    let neg = v
                        .checked_neg()
                        .ok_or_else(|| QueryError::failed("decimal negation overflow"))?;
                    *slot = check_decimal_bounds(neg, precision)?;
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
        DataType::Interval => {
            try_reserve(
                pool,
                raw_len * size_of::<Interval>() + bitmap_mem_bytes(raw_len),
            )?;
            let mut vals = vec![Interval::default(); raw_len];
            let mut nulls = Bitmap::new_zeroed(raw_len);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if is_active(active, row)
                    && let Some(iv) = read_interval(child, row)
                {
                    let months = iv
                        .months
                        .checked_neg()
                        .ok_or_else(|| QueryError::failed("interval negation overflow"))?;
                    let days = iv
                        .days
                        .checked_neg()
                        .ok_or_else(|| QueryError::failed("interval negation overflow"))?;
                    let nanos = iv
                        .nanos
                        .checked_neg()
                        .ok_or_else(|| QueryError::failed("interval negation overflow"))?;
                    *slot = Interval {
                        months,
                        days,
                        nanos,
                    };
                    let _ = nulls.set(row, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::Interval(vals),
                Some(nulls),
            )?))
        }
        _ => Err(QueryError::failed("unsupported operand for unary negation")),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal dispatch preserves explicit batch, mask, context, and reservation"
)]
fn eval_binary(
    left: &Expr,
    op: BinaryOp,
    right: &Expr,
    data_type: &DataType,
    batch: &Batch,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    match op {
        BinaryOp::Concat => {
            let lcol = eval_expr_masked(left, batch, active, ctx, pool.as_deref_mut())?;
            let rcol = eval_expr_masked(right, batch, active, ctx, pool.as_deref_mut())?;
            if matches!(data_type, DataType::List(_))
                || matches!(lcol.data_type(), DataType::List(_))
                || matches!(rcol.data_type(), DataType::List(_))
            {
                crate::fn_misc::eval_list_concat(&lcol, &rcol, active, ctx, pool)
            } else {
                crate::fn_string::eval_string_concat(&lcol, &rcol, active, ctx, pool)
            }
        }
        BinaryOp::ArrayContains => {
            let lcol = eval_expr_masked(left, batch, active, ctx, pool.as_deref_mut())?;
            let rcol = eval_expr_masked(right, batch, active, ctx, pool.as_deref_mut())?;
            crate::fn_misc::eval_misc_fn(
                ScalarFn::ArrayHasAll,
                &[lcol, rcol],
                batch.raw_len(),
                active,
                ctx,
                pool,
            )
        }
        BinaryOp::And => eval_and(left, right, batch, active, ctx, pool),
        BinaryOp::Or => eval_or(left, right, batch, active, ctx, pool),
        BinaryOp::Eq
        | BinaryOp::NotEq
        | BinaryOp::Lt
        | BinaryOp::LtEq
        | BinaryOp::Gt
        | BinaryOp::GtEq => {
            let lcol = eval_expr_masked(left, batch, active, ctx, pool.as_deref_mut())?;
            let rcol = eval_expr_masked(right, batch, active, ctx, pool.as_deref_mut())?;
            eval_cmp(op, &lcol, &rcol, batch.raw_len(), active, ctx, &mut pool)
        }
        BinaryOp::IsDistinctFrom | BinaryOp::IsNotDistinctFrom => {
            let lcol = eval_expr_masked(left, batch, active, ctx, pool.as_deref_mut())?;
            let rcol = eval_expr_masked(right, batch, active, ctx, pool.as_deref_mut())?;
            eval_distinct(op, &lcol, &rcol, batch.raw_len(), active, ctx, &mut pool)
        }
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
            let ldt = left.data_type();
            let rdt = right.data_type();
            if matches!(
                (&ldt, &rdt),
                (DataType::TimestampNs { .. }, DataType::Interval)
                    | (DataType::Interval, DataType::TimestampNs { .. })
            ) {
                let lcol = eval_expr_masked(left, batch, active, ctx, pool.as_deref_mut())?;
                let rcol = eval_expr_masked(right, batch, active, ctx, pool.as_deref_mut())?;
                return eval_timestamp_interval(
                    op,
                    &lcol,
                    &rcol,
                    data_type,
                    batch.raw_len(),
                    active,
                    ctx,
                    &mut pool,
                );
            }
            let lcol = eval_expr_masked(left, batch, active, ctx, pool.as_deref_mut())?;
            let rcol = eval_expr_masked(right, batch, active, ctx, pool.as_deref_mut())?;
            eval_arith(
                op,
                &lcol,
                &rcol,
                data_type,
                batch.raw_len(),
                active,
                ctx,
                &mut pool,
            )
        }
    }
}

fn eval_and(
    left: &Expr,
    right: &Expr,
    batch: &Batch,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = pool_size(&pool);
    let raw_len = batch.raw_len();
    let lcol = eval_expr_masked(left, batch, active, ctx, pool.as_deref_mut())?;

    let bm_bytes = bitmap_mem_bytes(raw_len);
    try_reserve(&mut pool, bm_bytes)?;
    let mut right_mask = Bitmap::new_zeroed(raw_len);
    for row in 0..raw_len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, &mut pool, before)?;
        }
        if is_active(active, row) && read_bool(&lcol, row) != Some(false) {
            let _ = right_mask.set(row, true);
        }
    }

    if right_mask.all_zeros() {
        try_reserve(&mut pool, bm_bytes * 2)?;
        let vals = Bitmap::new_zeroed(raw_len);
        let nulls = active.cloned().unwrap_or_else(|| Bitmap::new_ones(raw_len));
        return build_bool_column(raw_len, vals, Some(nulls));
    }

    let rcol = eval_expr_masked(right, batch, Some(&right_mask), ctx, pool.as_deref_mut())?;
    try_reserve(&mut pool, bm_bytes * 2)?;
    let mut vals = Bitmap::new_zeroed(raw_len);
    let mut nulls = Bitmap::new_zeroed(raw_len);
    for row in 0..raw_len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, &mut pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        let l = read_bool(&lcol, row);
        let r = if right_mask.get(row) {
            read_bool(&rcol, row)
        } else {
            None
        };
        match (l, r) {
            (Some(false), _) | (_, Some(false)) => {
                let _ = nulls.set(row, true);
            }
            (Some(true), Some(true)) => {
                let _ = nulls.set(row, true);
                let _ = vals.set(row, true);
            }
            _ => {}
        }
    }
    build_bool_column(raw_len, vals, Some(nulls))
}

fn eval_or(
    left: &Expr,
    right: &Expr,
    batch: &Batch,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = pool_size(&pool);
    let raw_len = batch.raw_len();
    let lcol = eval_expr_masked(left, batch, active, ctx, pool.as_deref_mut())?;

    let bm_bytes = bitmap_mem_bytes(raw_len);
    try_reserve(&mut pool, bm_bytes)?;
    let mut right_mask = Bitmap::new_zeroed(raw_len);
    for row in 0..raw_len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, &mut pool, before)?;
        }
        if is_active(active, row) && read_bool(&lcol, row) != Some(true) {
            let _ = right_mask.set(row, true);
        }
    }

    if right_mask.all_zeros() {
        try_reserve(&mut pool, bm_bytes * 2)?;
        let vals = active.cloned().unwrap_or_else(|| Bitmap::new_ones(raw_len));
        let nulls = active.cloned().unwrap_or_else(|| Bitmap::new_ones(raw_len));
        return build_bool_column(raw_len, vals, Some(nulls));
    }

    let rcol = eval_expr_masked(right, batch, Some(&right_mask), ctx, pool.as_deref_mut())?;
    try_reserve(&mut pool, bm_bytes * 2)?;
    let mut vals = Bitmap::new_zeroed(raw_len);
    let mut nulls = Bitmap::new_zeroed(raw_len);
    for row in 0..raw_len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, &mut pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        let l = read_bool(&lcol, row);
        let r = if right_mask.get(row) {
            read_bool(&rcol, row)
        } else {
            None
        };
        match (l, r) {
            (Some(true), _) | (_, Some(true)) => {
                let _ = nulls.set(row, true);
                let _ = vals.set(row, true);
            }
            (Some(false), Some(false)) => {
                let _ = nulls.set(row, true);
            }
            _ => {}
        }
    }
    build_bool_column(raw_len, vals, Some(nulls))
}

fn eval_cmp(
    op: BinaryOp,
    lcol: &Column,
    rcol: &Column,
    raw_len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = pool_size(pool);
    if matches!(lcol.data_type(), DataType::Null) || matches!(rcol.data_type(), DataType::Null) {
        return Ok(Column::Const {
            value: ScalarValue::Null(DataType::Bool),
            len: raw_len,
        });
    }
    let bm_bytes = bitmap_mem_bytes(raw_len);
    try_reserve(pool, bm_bytes * 2)?;
    let mut vals = Bitmap::new_zeroed(raw_len);
    let mut nulls = Bitmap::new_zeroed(raw_len);

    for row in 0..raw_len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) || lcol.is_null(row) || rcol.is_null(row) {
            continue;
        }
        let ord = compare_row(lcol, rcol, row)?;
        let _ = nulls.set(row, true);
        if matches_cmp_op(op, ord) {
            let _ = vals.set(row, true);
        }
    }
    build_bool_column(raw_len, vals, Some(nulls))
}

fn eval_distinct(
    op: BinaryOp,
    lcol: &Column,
    rcol: &Column,
    raw_len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = pool_size(pool);
    let bm_bytes = bitmap_mem_bytes(raw_len);
    let need_nulls = active.is_some();
    try_reserve(pool, bm_bytes * if need_nulls { 2 } else { 1 })?;
    let mut vals = Bitmap::new_zeroed(raw_len);
    let mut nulls = need_nulls.then(|| Bitmap::new_zeroed(raw_len));

    let want_distinct = matches!(op, BinaryOp::IsDistinctFrom);
    for row in 0..raw_len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        if let Some(n) = nulls.as_mut() {
            let _ = n.set(row, true);
        }
        let is_distinct = match (lcol.is_null(row), rcol.is_null(row)) {
            (true, true) => false,
            (true, false) | (false, true) => true,
            (false, false) => compare_row(lcol, rcol, row)? != Ordering::Equal,
        };
        if is_distinct == want_distinct {
            let _ = vals.set(row, true);
        }
    }
    build_bool_column(raw_len, vals, nulls)
}

fn matches_cmp_op(op: BinaryOp, ord: Ordering) -> bool {
    match op {
        BinaryOp::Eq => ord == Ordering::Equal,
        BinaryOp::NotEq => ord != Ordering::Equal,
        BinaryOp::Lt => ord == Ordering::Less,
        BinaryOp::LtEq => ord != Ordering::Greater,
        BinaryOp::Gt => ord == Ordering::Greater,
        BinaryOp::GtEq => ord != Ordering::Less,
        _ => false,
    }
}

fn compare_row(lcol: &Column, rcol: &Column, row: usize) -> Result<Ordering, QueryError> {
    match (lcol.data_type(), rcol.data_type()) {
        (DataType::Bool, DataType::Bool) => {
            let a = read_bool(lcol, row);
            let b = read_bool(rcol, row);
            Ok(a.cmp(&b))
        }
        (DataType::I64, DataType::I64) => Ok(read_i64(lcol, row).cmp(&read_i64(rcol, row))),
        (DataType::F64, DataType::F64) => match (read_f64(lcol, row), read_f64(rcol, row)) {
            (Some(a), Some(b)) => Ok(f64_cmp(a, b)),
            _ => Err(QueryError::failed("invalid f64 comparison")),
        },
        (DataType::Decimal128 { scale: s1, .. }, DataType::Decimal128 { scale: s2, .. }) => {
            match (read_decimal(lcol, row), read_decimal(rcol, row)) {
                (Some(a), Some(b)) => compare_decimals(a, s1, b, s2),
                _ => Err(QueryError::failed("invalid decimal comparison")),
            }
        }
        (DataType::TimestampNs { .. }, DataType::TimestampNs { .. }) => {
            Ok(read_timestamp(lcol, row).cmp(&read_timestamp(rcol, row)))
        }
        (DataType::Date, DataType::Date) => Ok(read_date(lcol, row).cmp(&read_date(rcol, row))),
        (DataType::Interval, DataType::Interval) => {
            match (read_interval(lcol, row), read_interval(rcol, row)) {
                (Some(a), Some(b)) => {
                    Ok((a.months, a.days, a.nanos).cmp(&(b.months, b.days, b.nanos)))
                }
                _ => Err(QueryError::failed("invalid interval comparison")),
            }
        }
        (DataType::Utf8, DataType::Utf8) => Ok(read_str(lcol, row).cmp(&read_str(rcol, row))),
        (
            DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16,
            DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16,
        ) => Ok(read_bytes(lcol, row).cmp(&read_bytes(rcol, row))),
        (DataType::List(_), DataType::List(_)) => {
            match (list_ref_at(lcol, row), list_ref_at(rcol, row)) {
                (Some(la), Some(lb)) => compare_list_refs(la, lb),
                _ => Err(QueryError::failed("invalid list comparison")),
            }
        }
        _ => Err(QueryError::failed("incompatible types in comparison")),
    }
}

fn compare_decimals(a: i128, s1: i8, b: i128, s2: i8) -> Result<Ordering, QueryError> {
    if s1 == s2 {
        return Ok(a.cmp(&b));
    }
    if s1 < s2 {
        let diff = (s2 as i32 - s1 as i32) as u32;
        if let Some(factor) = 10i128.checked_pow(diff)
            && let Some(sa) = a.checked_mul(factor)
        {
            return Ok(sa.cmp(&b));
        }
        let (hi_a, lo_a) = u256_mul(a.unsigned_abs(), 10u128.pow(diff));
        Ok(cmp_signed_u256_i128(a < 0, hi_a, lo_a, b))
    } else {
        let diff = (s1 as i32 - s2 as i32) as u32;
        if let Some(factor) = 10i128.checked_pow(diff)
            && let Some(sb) = b.checked_mul(factor)
        {
            return Ok(a.cmp(&sb));
        }
        let (hi_b, lo_b) = u256_mul(b.unsigned_abs(), 10u128.pow(diff));
        Ok(cmp_signed_u256_i128(b < 0, hi_b, lo_b, a).reverse())
    }
}

fn cmp_signed_u256_i128(neg_u: bool, hi: u128, lo: u128, other: i128) -> Ordering {
    let neg_o = other < 0;
    if neg_u != neg_o {
        return if neg_u {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    let mag_ord = if hi > 0 {
        Ordering::Greater
    } else {
        lo.cmp(&other.unsigned_abs())
    };
    if neg_u { mag_ord.reverse() } else { mag_ord }
}

fn compare_list_refs(la: ListRef<'_>, lb: ListRef<'_>) -> Result<Ordering, QueryError> {
    let len_a = match la {
        ListRef::Flat { start, end, .. } => end - start,
        ListRef::Const(s) => s.len(),
    };
    let len_b = match lb {
        ListRef::Flat { start, end, .. } => end - start,
        ListRef::Const(s) => s.len(),
    };
    for i in 0..len_a.min(len_b) {
        let sa = match la {
            ListRef::Flat { values, start, .. } => read_scalar(values, start + i),
            ListRef::Const(s) => s[i].clone(),
        };
        let sb = match lb {
            ListRef::Flat { values, start, .. } => read_scalar(values, start + i),
            ListRef::Const(s) => s[i].clone(),
        };
        let ord = compare_scalars(&sa, &sb)?;
        if ord != Ordering::Equal {
            return Ok(ord);
        }
    }
    Ok(len_a.cmp(&len_b))
}

fn compare_scalars(a: &ScalarValue, b: &ScalarValue) -> Result<Ordering, QueryError> {
    match (a.is_null(), b.is_null()) {
        (true, true) => return Ok(Ordering::Equal),
        (true, false) => return Ok(Ordering::Less),
        (false, true) => return Ok(Ordering::Greater),
        (false, false) => {}
    }
    match (a, b) {
        (ScalarValue::Bool(x), ScalarValue::Bool(y)) => Ok(x.cmp(y)),
        (ScalarValue::I64(x), ScalarValue::I64(y)) => Ok(x.cmp(y)),
        (ScalarValue::F64(x), ScalarValue::F64(y)) => Ok(f64_cmp(*x, *y)),
        (
            ScalarValue::Decimal128 {
                value: x,
                scale: sx,
                ..
            },
            ScalarValue::Decimal128 {
                value: y,
                scale: sy,
                ..
            },
        ) => compare_decimals(*x, *sx, *y, *sy),
        (ScalarValue::TimestampNs { value: x, .. }, ScalarValue::TimestampNs { value: y, .. }) => {
            Ok(x.cmp(y))
        }
        (ScalarValue::Date(x), ScalarValue::Date(y)) => Ok(x.cmp(y)),
        (ScalarValue::Interval(x), ScalarValue::Interval(y)) => {
            Ok((x.months, x.days, x.nanos).cmp(&(y.months, y.days, y.nanos)))
        }
        (ScalarValue::Utf8(x), ScalarValue::Utf8(y)) => Ok(x.cmp(y)),
        (ScalarValue::Binary(x), ScalarValue::Binary(y)) => Ok(x.cmp(y)),
        (ScalarValue::FixedBytes8(x), ScalarValue::FixedBytes8(y)) => Ok(x.cmp(y)),
        (ScalarValue::FixedBytes16(x), ScalarValue::FixedBytes16(y)) => Ok(x.cmp(y)),
        (ScalarValue::List { values: xs, .. }, ScalarValue::List { values: ys, .. }) => {
            for (x, y) in xs.iter().zip(ys.iter()) {
                let ord = compare_scalars(x, y)?;
                if ord != Ordering::Equal {
                    return Ok(ord);
                }
            }
            Ok(xs.len().cmp(&ys.len()))
        }
        _ => Err(QueryError::failed("incompatible scalar comparison")),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal arithmetic helper avoids extra context struct allocation"
)]
fn eval_arith(
    op: BinaryOp,
    lcol: &Column,
    rcol: &Column,
    data_type: &DataType,
    raw_len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = pool_size(pool);
    if matches!(lcol.data_type(), DataType::Null) || matches!(rcol.data_type(), DataType::Null) {
        return Ok(Column::Const {
            value: ScalarValue::Null(data_type.clone()),
            len: raw_len,
        });
    }

    match data_type {
        DataType::I64 => {
            try_reserve(pool, raw_len * 8 + bitmap_mem_bytes(raw_len))?;
            let mut vals = vec![0i64; raw_len];
            let mut nulls = Bitmap::new_zeroed(raw_len);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if !is_active(active, row) {
                    continue;
                }
                if let (Some(a), Some(b)) = (read_i64(lcol, row), read_i64(rcol, row)) {
                    *slot = match op {
                        BinaryOp::Add => a
                            .checked_add(b)
                            .ok_or_else(|| QueryError::failed("integer addition overflow"))?,
                        BinaryOp::Sub => a
                            .checked_sub(b)
                            .ok_or_else(|| QueryError::failed("integer subtraction overflow"))?,
                        BinaryOp::Mul => a
                            .checked_mul(b)
                            .ok_or_else(|| QueryError::failed("integer multiplication overflow"))?,
                        BinaryOp::Div => {
                            if b == 0 {
                                return Err(QueryError::failed("division by zero"));
                            }
                            a.checked_div(b)
                                .ok_or_else(|| QueryError::failed("integer division overflow"))?
                        }
                        BinaryOp::Mod => {
                            if b == 0 {
                                return Err(QueryError::failed("division by zero"));
                            }
                            a.checked_rem(b)
                                .ok_or_else(|| QueryError::failed("integer remainder overflow"))?
                        }
                        _ => return Err(QueryError::failed("unsupported integer operator")),
                    };
                    let _ = nulls.set(row, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::I64(vals),
                Some(nulls),
            )?))
        }
        DataType::F64 => {
            try_reserve(pool, raw_len * 8 + bitmap_mem_bytes(raw_len))?;
            let mut vals = vec![0.0f64; raw_len];
            let mut nulls = Bitmap::new_zeroed(raw_len);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if !is_active(active, row) {
                    continue;
                }
                if let (Some(a), Some(b)) = (read_f64(lcol, row), read_f64(rcol, row)) {
                    *slot = match op {
                        BinaryOp::Add => a + b,
                        BinaryOp::Sub => a - b,
                        BinaryOp::Mul => a * b,
                        BinaryOp::Div => {
                            if b == 0.0 {
                                return Err(QueryError::failed("division by zero"));
                            }
                            a / b
                        }
                        BinaryOp::Mod => {
                            if b == 0.0 {
                                return Err(QueryError::failed("division by zero"));
                            }
                            a % b
                        }
                        _ => return Err(QueryError::failed("unsupported float operator")),
                    };
                    let _ = nulls.set(row, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::F64(vals),
                Some(nulls),
            )?))
        }
        DataType::Decimal128 { precision, scale } => {
            try_reserve(pool, raw_len * 16 + bitmap_mem_bytes(raw_len))?;
            let mut vals = vec![0i128; raw_len];
            let mut nulls = Bitmap::new_zeroed(raw_len);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if !is_active(active, row) {
                    continue;
                }
                if let (Some(a), Some(b)) = (read_decimal(lcol, row), read_decimal(rcol, row)) {
                    let res = match op {
                        BinaryOp::Add => a
                            .checked_add(b)
                            .ok_or_else(|| QueryError::failed("decimal addition overflow"))?,
                        BinaryOp::Sub => a
                            .checked_sub(b)
                            .ok_or_else(|| QueryError::failed("decimal subtraction overflow"))?,
                        BinaryOp::Mul => decimal_mul(a, b, *scale)?,
                        BinaryOp::Div => decimal_div(a, b, *scale)?,
                        BinaryOp::Mod => {
                            if b == 0 {
                                return Err(QueryError::failed("division by zero"));
                            }
                            a.checked_rem(b)
                                .ok_or_else(|| QueryError::failed("decimal remainder overflow"))?
                        }
                        _ => return Err(QueryError::failed("unsupported decimal operator")),
                    };
                    *slot = check_decimal_bounds(res, *precision)?;
                    let _ = nulls.set(row, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vals,
                    precision: *precision,
                    scale: *scale,
                },
                Some(nulls),
            )?))
        }
        _ => Err(QueryError::failed("unsupported arithmetic types")),
    }
}

fn check_decimal_bounds(val: i128, precision: u8) -> Result<i128, QueryError> {
    if precision == 0 || precision > 38 {
        return Err(QueryError::failed("decimal precision out of range"));
    }
    let limit = 10u128.pow(u32::from(precision));
    if val.unsigned_abs() >= limit {
        return Err(QueryError::failed("decimal overflow"));
    }
    Ok(val)
}

fn u256_mul(a: u128, b: u128) -> (u128, u128) {
    let a_lo = a & 0xffff_ffff_ffff_ffff;
    let a_hi = a >> 64;
    let b_lo = b & 0xffff_ffff_ffff_ffff;
    let b_hi = b >> 64;

    let p0 = a_lo * b_lo;
    let p1 = a_lo * b_hi;
    let p2 = a_hi * b_lo;
    let p3 = a_hi * b_hi;

    let mid1 = (p0 >> 64) + (p1 & 0xffff_ffff_ffff_ffff) + (p2 & 0xffff_ffff_ffff_ffff);
    let lo = (p0 & 0xffff_ffff_ffff_ffff) | (mid1 << 64);
    let hi = p3 + (p1 >> 64) + (p2 >> 64) + (mid1 >> 64);
    (hi, lo)
}

fn u256_div_rem(hi: u128, lo: u128, d: u128) -> Option<(u128, u128)> {
    if d == 0 || hi >= d {
        return None;
    }
    let mut q = 0u128;
    let mut r = hi;
    for shift in (0..128).rev() {
        let carry = r >> 127;
        r = (r << 1) | ((lo >> shift) & 1);
        if carry != 0 || r >= d {
            r = r.wrapping_sub(d);
            q |= 1u128 << shift;
        }
    }
    Some((q, r))
}

fn signed_from_u128(neg: bool, mag: u128) -> Result<i128, QueryError> {
    if mag == 0 {
        return Ok(0);
    }
    let abs = i128::try_from(mag).map_err(|_| QueryError::failed("decimal overflow"))?;
    Ok(if neg { -abs } else { abs })
}

fn decimal_mul(a: i128, b: i128, scale: i8) -> Result<i128, QueryError> {
    let neg = (a < 0) ^ (b < 0);
    let (hi, lo) = u256_mul(a.unsigned_abs(), b.unsigned_abs());
    let mag = if scale >= 0 {
        let denom = 10u128
            .checked_pow(scale as u32)
            .ok_or_else(|| QueryError::failed("decimal scale overflow"))?;
        let (mut q, r) =
            u256_div_rem(hi, lo, denom).ok_or_else(|| QueryError::failed("decimal overflow"))?;
        if r >= denom.div_ceil(2) {
            q = q
                .checked_add(1)
                .ok_or_else(|| QueryError::failed("decimal overflow"))?;
        }
        q
    } else {
        if hi != 0 {
            return Err(QueryError::failed("decimal overflow"));
        }
        let factor = 10u128
            .checked_pow((-scale) as u32)
            .ok_or_else(|| QueryError::failed("decimal scale overflow"))?;
        lo.checked_mul(factor)
            .ok_or_else(|| QueryError::failed("decimal overflow"))?
    };
    signed_from_u128(neg, mag)
}

fn decimal_div(a: i128, b: i128, scale: i8) -> Result<i128, QueryError> {
    if b == 0 {
        return Err(QueryError::failed("division by zero"));
    }
    let neg = (a < 0) ^ (b < 0);
    let b_abs = b.unsigned_abs();
    let mag = if scale >= 0 {
        let factor = 10u128
            .checked_pow(scale as u32)
            .ok_or_else(|| QueryError::failed("decimal scale overflow"))?;
        let (hi, lo) = u256_mul(a.unsigned_abs(), factor);
        let (q, _) = u256_div_rem(hi, lo, b_abs)
            .ok_or_else(|| QueryError::failed("decimal division overflow"))?;
        q
    } else {
        let factor = 10u128
            .checked_pow((-scale) as u32)
            .ok_or_else(|| QueryError::failed("decimal scale overflow"))?;
        let (hi, lo) = u256_mul(b_abs, factor);
        if hi != 0 { 0 } else { a.unsigned_abs() / lo }
    };
    signed_from_u128(neg, mag)
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal BETWEEN evaluator preserves explicit bounds and mask"
)]
fn eval_between(
    inner: &Expr,
    low: &Expr,
    high: &Expr,
    negated: bool,
    batch: &Batch,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = pool_size(&pool);
    let raw_len = batch.raw_len();
    let val = eval_expr_masked(inner, batch, active, ctx, pool.as_deref_mut())?;
    let lo = eval_expr_masked(low, batch, active, ctx, pool.as_deref_mut())?;
    let hi = eval_expr_masked(high, batch, active, ctx, pool.as_deref_mut())?;

    let bm_bytes = bitmap_mem_bytes(raw_len);
    try_reserve(&mut pool, bm_bytes * 2)?;
    let mut vals = Bitmap::new_zeroed(raw_len);
    let mut nulls = Bitmap::new_zeroed(raw_len);

    for row in 0..raw_len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, &mut pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        let ge = if val.is_null(row) || lo.is_null(row) {
            None
        } else {
            Some(compare_row(&val, &lo, row)? != Ordering::Less)
        };
        let le = if val.is_null(row) || hi.is_null(row) {
            None
        } else {
            Some(compare_row(&val, &hi, row)? != Ordering::Greater)
        };
        let between = match (ge, le) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        };
        if let Some(b) = between {
            let _ = nulls.set(row, true);
            let out = if negated { !b } else { b };
            if out {
                let _ = vals.set(row, true);
            }
        }
    }
    build_bool_column(raw_len, vals, Some(nulls))
}

enum LiteralSet {
    Bool(HashSet<bool>),
    I64(HashSet<i64>),
    F64(HashSet<u64>),
    Decimal128(HashSet<i128>),
    Utf8(HashSet<String>),
    Bytes(HashSet<Vec<u8>>),
    TimestampNs(HashSet<i64>),
    Date(HashSet<i32>),
}

impl LiteralSet {
    fn from_literals(lhs_type: &DataType, list: &[Expr]) -> Option<(Self, bool)> {
        let mut has_null = false;
        let mut scalars = Vec::with_capacity(list.len());
        for item in list {
            let Expr::Literal(sv) = item else {
                return None;
            };
            if sv.is_null() {
                has_null = true;
            } else {
                scalars.push(sv);
            }
        }

        let set = match lhs_type {
            DataType::Bool => {
                let mut s = HashSet::with_capacity(scalars.len());
                for sv in scalars {
                    let ScalarValue::Bool(v) = sv else {
                        return None;
                    };
                    s.insert(*v);
                }
                Self::Bool(s)
            }
            DataType::I64 => {
                let mut s = HashSet::with_capacity(scalars.len());
                for sv in scalars {
                    let ScalarValue::I64(v) = sv else {
                        return None;
                    };
                    s.insert(*v);
                }
                Self::I64(s)
            }
            DataType::F64 => {
                let mut s = HashSet::with_capacity(scalars.len());
                for sv in scalars {
                    let ScalarValue::F64(v) = sv else {
                        return None;
                    };
                    s.insert(f64_canonical_bits(*v));
                }
                Self::F64(s)
            }
            DataType::Decimal128 { scale, .. } => {
                let mut s = HashSet::with_capacity(scalars.len());
                for sv in scalars {
                    let ScalarValue::Decimal128 {
                        value, scale: sv_s, ..
                    } = sv
                    else {
                        return None;
                    };
                    if sv_s != scale {
                        return None;
                    }
                    s.insert(*value);
                }
                Self::Decimal128(s)
            }
            DataType::Utf8 => {
                let mut s = HashSet::with_capacity(scalars.len());
                for sv in scalars {
                    let ScalarValue::Utf8(v) = sv else {
                        return None;
                    };
                    s.insert(v.clone());
                }
                Self::Utf8(s)
            }
            DataType::Binary | DataType::FixedBytes8 | DataType::FixedBytes16 => {
                let mut s = HashSet::with_capacity(scalars.len());
                for sv in scalars {
                    match sv {
                        ScalarValue::Binary(b) => {
                            s.insert(b.clone());
                        }
                        ScalarValue::FixedBytes8(b) => {
                            s.insert(b.to_vec());
                        }
                        ScalarValue::FixedBytes16(b) => {
                            s.insert(b.to_vec());
                        }
                        _ => return None,
                    }
                }
                Self::Bytes(s)
            }
            DataType::TimestampNs { .. } => {
                let mut s = HashSet::with_capacity(scalars.len());
                for sv in scalars {
                    let ScalarValue::TimestampNs { value, .. } = sv else {
                        return None;
                    };
                    s.insert(*value);
                }
                Self::TimestampNs(s)
            }
            DataType::Date => {
                let mut s = HashSet::with_capacity(scalars.len());
                for sv in scalars {
                    let ScalarValue::Date(d) = sv else {
                        return None;
                    };
                    s.insert(*d);
                }
                Self::Date(s)
            }
            _ => return None,
        };
        Some((set, has_null))
    }

    fn contains(&self, col: &Column, row: usize) -> Option<bool> {
        match self {
            Self::Bool(s) => read_bool(col, row).map(|v| s.contains(&v)),
            Self::I64(s) => read_i64(col, row).map(|v| s.contains(&v)),
            Self::F64(s) => read_f64(col, row).map(|v| s.contains(&f64_canonical_bits(v))),
            Self::Decimal128(s) => read_decimal(col, row).map(|v| s.contains(&v)),
            Self::Utf8(s) => read_str(col, row).map(|v| s.contains(v)),
            Self::Bytes(s) => read_bytes(col, row).map(|v| s.contains(v)),
            Self::TimestampNs(s) => read_timestamp(col, row).map(|v| s.contains(&v)),
            Self::Date(s) => read_date(col, row).map(|v| s.contains(&v)),
        }
    }
}

fn eval_in_list(
    inner: &Expr,
    list: &[Expr],
    negated: bool,
    batch: &Batch,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = pool_size(&pool);
    let raw_len = batch.raw_len();

    if list.is_empty() {
        if active.is_none() {
            return Ok(Column::Const {
                value: ScalarValue::Bool(negated),
                len: raw_len,
            });
        }
        let bm_bytes = bitmap_mem_bytes(raw_len);
        try_reserve(&mut pool, bm_bytes * 2)?;
        let mut vals = Bitmap::new_zeroed(raw_len);
        let mut nulls = Bitmap::new_zeroed(raw_len);
        for row in 0..raw_len {
            if is_active(active, row) {
                let _ = nulls.set(row, true);
                if negated {
                    let _ = vals.set(row, true);
                }
            }
        }
        return build_bool_column(raw_len, vals, Some(nulls));
    }

    let lhs = eval_expr_masked(inner, batch, active, ctx, pool.as_deref_mut())?;
    let lhs_dt = lhs.data_type();
    if matches!(lhs_dt, DataType::Null) {
        return Ok(Column::Const {
            value: ScalarValue::Null(DataType::Bool),
            len: raw_len,
        });
    }

    if let Some((set, has_null)) = LiteralSet::from_literals(&lhs_dt, list) {
        let bm_bytes = bitmap_mem_bytes(raw_len);
        try_reserve(&mut pool, bm_bytes * 2)?;
        let mut vals = Bitmap::new_zeroed(raw_len);
        let mut nulls = Bitmap::new_zeroed(raw_len);
        for row in 0..raw_len {
            if (row & CANCEL_CHECK_MASK) == 0 {
                check_cancel(ctx, &mut pool, before)?;
            }
            if !is_active(active, row) {
                continue;
            }
            match set.contains(&lhs, row) {
                None => {}
                Some(true) => {
                    let _ = nulls.set(row, true);
                    if !negated {
                        let _ = vals.set(row, true);
                    }
                }
                Some(false) if !has_null => {
                    let _ = nulls.set(row, true);
                    if negated {
                        let _ = vals.set(row, true);
                    }
                }
                Some(false) => {}
            }
        }
        return build_bool_column(raw_len, vals, Some(nulls));
    }

    let mut item_cols = Vec::with_capacity(list.len());
    for item in list {
        item_cols.push(eval_expr_masked(
            item,
            batch,
            active,
            ctx,
            pool.as_deref_mut(),
        )?);
    }

    let bm_bytes = bitmap_mem_bytes(raw_len);
    try_reserve(&mut pool, bm_bytes * 2)?;
    let mut vals = Bitmap::new_zeroed(raw_len);
    let mut nulls = Bitmap::new_zeroed(raw_len);

    for row in 0..raw_len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, &mut pool, before)?;
        }
        if !is_active(active, row) || lhs.is_null(row) {
            continue;
        }
        let mut any_match = false;
        let mut any_null = false;
        for ic in &item_cols {
            if ic.is_null(row) {
                any_null = true;
            } else if compare_row(&lhs, ic, row)? == Ordering::Equal {
                any_match = true;
                break;
            }
        }
        if any_match {
            let _ = nulls.set(row, true);
            if !negated {
                let _ = vals.set(row, true);
            }
        } else if !any_null {
            let _ = nulls.set(row, true);
            if negated {
                let _ = vals.set(row, true);
            }
        }
    }
    build_bool_column(raw_len, vals, Some(nulls))
}

#[expect(
    clippy::too_many_arguments,
    reason = "CASE evaluator tracks operand, branches, else, output type, mask, ctx, pool"
)]
fn eval_case(
    operand: Option<&Expr>,
    when_then: &[(Expr, Expr)],
    else_expr: Option<&Expr>,
    data_type: &DataType,
    batch: &Batch,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = pool_size(&pool);
    let raw_len = batch.raw_len();

    let op_col = match operand {
        Some(op_ex) => Some(eval_expr_masked(
            op_ex,
            batch,
            active,
            ctx,
            pool.as_deref_mut(),
        )?),
        None => None,
    };

    try_reserve(&mut pool, bitmap_mem_bytes(raw_len) * 2)?;
    let mut remaining = active.cloned().unwrap_or_else(|| Bitmap::new_ones(raw_len));
    let mut branch_cols: Vec<Column> = Vec::with_capacity(when_then.len() + 1);
    let mut row_source: Vec<Option<(usize, usize)>> = vec![None; raw_len];

    for (when_ex, then_ex) in when_then {
        if remaining.all_zeros() {
            break;
        }
        check_cancel(ctx, &mut pool, before)?;
        let when_col =
            eval_expr_masked(when_ex, batch, Some(&remaining), ctx, pool.as_deref_mut())?;
        let mut matched = Bitmap::new_zeroed(raw_len);
        for row in remaining.iter_ones() {
            if (row & CANCEL_CHECK_MASK) == 0 {
                check_cancel(ctx, &mut pool, before)?;
            }
            let is_match = match &op_col {
                None => read_bool(&when_col, row) == Some(true),
                Some(op_c) => {
                    !op_c.is_null(row)
                        && !when_col.is_null(row)
                        && compare_row(op_c, &when_col, row)? == Ordering::Equal
                }
            };
            if is_match {
                let _ = matched.set(row, true);
            }
        }
        drop(when_col);

        if !matched.all_zeros() {
            let then_col =
                eval_expr_masked(then_ex, batch, Some(&matched), ctx, pool.as_deref_mut())?;
            let col_idx = branch_cols.len();
            branch_cols.push(then_col);
            for row in matched.iter_ones() {
                row_source[row] = Some((col_idx, row));
                let _ = remaining.set(row, false);
            }
        }
    }

    if !remaining.all_zeros()
        && let Some(el_ex) = else_expr
    {
        let else_col = eval_expr_masked(el_ex, batch, Some(&remaining), ctx, pool.as_deref_mut())?;
        let col_idx = branch_cols.len();
        branch_cols.push(else_col);
        for row in remaining.iter_ones() {
            row_source[row] = Some((col_idx, row));
        }
    }

    drop(op_col);
    drop(remaining);

    let sources: Vec<Option<(&Column, usize)>> = row_source
        .iter()
        .map(|entry| entry.map(|(ci, r)| (&branch_cols[ci], r)))
        .collect();
    build_flat_from_sources(data_type, &sources, &mut pool, ctx, before)
}

fn eval_map_index(
    map_expr: &Expr,
    key_expr: &Expr,
    batch: &Batch,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = pool_size(&pool);
    let raw_len = batch.raw_len();
    let map_col = eval_expr_masked(map_expr, batch, active, ctx, pool.as_deref_mut())?;
    let key_col = eval_expr_masked(key_expr, batch, active, ctx, pool.as_deref_mut())?;

    let mut total_bytes = 0usize;
    for row in 0..raw_len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, &mut pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        if let Some(v) = lookup_map_value(&map_col, &key_col, row) {
            total_bytes = total_bytes
                .checked_add(v.len())
                .ok_or(ColumnError::Overflow {
                    reason: "map index utf8 byte overflow",
                })?;
        }
    }
    let _ = u32::try_from(total_bytes).map_err(|_| ColumnError::Overflow {
        reason: "map index utf8 byte length exceeds u32::MAX",
    })?;
    let est = (raw_len + 1) * 4 + total_bytes + bitmap_mem_bytes(raw_len);
    try_reserve(&mut pool, est)?;

    let mut offsets = Vec::with_capacity(raw_len + 1);
    offsets.push(0u32);
    let mut bytes = Vec::with_capacity(total_bytes);
    let mut nulls = Bitmap::new_zeroed(raw_len);

    for row in 0..raw_len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, &mut pool, before)?;
        }
        if is_active(active, row)
            && let Some(val) = lookup_map_value(&map_col, &key_col, row)
        {
            bytes.extend_from_slice(val.as_bytes());
            let _ = nulls.set(row, true);
        }
        offsets.push(bytes.len() as u32);
    }

    let utf8 = Utf8Data::try_new(offsets, bytes)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(utf8),
        Some(nulls),
    )?))
}

fn lookup_map_value<'a>(map_col: &'a Column, key_col: &Column, row: usize) -> Option<&'a str> {
    let key = read_str(key_col, row)?;
    let map_ref = map_ref_at(map_col, row)?;
    match map_ref {
        MapRef::Const(entries) => entries
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.as_deref()),
        MapRef::Flat { data, start, end } => {
            for idx in start..end {
                if data.keys().get(idx) == Some(key) {
                    if data.value_nulls().is_some_and(|bm| !bm.get(idx)) {
                        return None;
                    }
                    return data.values().get(idx);
                }
            }
            None
        }
    }
}

fn eval_make_array(
    elems: &[Expr],
    item_type: &DataType,
    batch: &Batch,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = pool_size(&pool);
    let raw_len = batch.raw_len();
    let mut elem_cols = Vec::with_capacity(elems.len());
    for elem in elems {
        elem_cols.push(eval_expr_masked(
            elem,
            batch,
            active,
            ctx,
            pool.as_deref_mut(),
        )?);
    }

    let n_active = active_count(active, raw_len);
    let total_elems = n_active
        .checked_mul(elems.len())
        .ok_or(ColumnError::Overflow {
            reason: "array element count overflow",
        })?;
    let _ = u32::try_from(total_elems).map_err(|_| ColumnError::Overflow {
        reason: "array element count exceeds u32::MAX",
    })?;

    let mut child_sources: Vec<Option<(&Column, usize)>> = Vec::with_capacity(total_elems);
    try_reserve(&mut pool, (raw_len + 1) * 4 + bitmap_mem_bytes(raw_len))?;
    let mut offsets = Vec::with_capacity(raw_len + 1);
    offsets.push(0u32);
    let mut nulls = active.map(|_| Bitmap::new_zeroed(raw_len));

    for row in 0..raw_len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, &mut pool, before)?;
        }
        if is_active(active, row) {
            if let Some(n) = nulls.as_mut() {
                let _ = n.set(row, true);
            }
            for ec in &elem_cols {
                if ec.is_null(row) {
                    child_sources.push(None);
                } else {
                    child_sources.push(Some((ec, row)));
                }
            }
        }
        offsets.push(child_sources.len() as u32);
    }

    let values = build_flat_from_sources(item_type, &child_sources, &mut pool, ctx, before)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::List {
            item_type: item_type.clone(),
            offsets,
            values: Box::new(values),
        },
        nulls,
    )?))
}

fn build_flat_from_sources(
    data_type: &DataType,
    sources: &[Option<(&Column, usize)>],
    pool: &mut Option<&mut Reservation>,
    ctx: &EvalContext,
    before: usize,
) -> Result<Column, QueryError> {
    let len = sources.len();
    let nulls_bytes = bitmap_mem_bytes(len);

    match data_type {
        DataType::Null => Ok(Column::Flat(FlatColumn::try_new(
            FlatData::Null(len),
            None,
        )?)),
        DataType::Bool => {
            try_reserve(pool, nulls_bytes * 2)?;
            let mut vals = Bitmap::new_zeroed(len);
            let mut nulls = Bitmap::new_zeroed(len);
            for (i, src) in sources.iter().enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some((col, row)) = src
                    && let Some(b) = read_bool(col, *row)
                {
                    let _ = nulls.set(i, true);
                    if b {
                        let _ = vals.set(i, true);
                    }
                }
            }
            build_bool_column(len, vals, Some(nulls))
        }
        DataType::I64 => {
            try_reserve(pool, len * 8 + nulls_bytes)?;
            let mut vals = vec![0i64; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (i, (src, slot)) in sources.iter().zip(vals.iter_mut()).enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some((col, row)) = src
                    && let Some(v) = read_i64(col, *row)
                {
                    *slot = v;
                    let _ = nulls.set(i, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::I64(vals),
                Some(nulls),
            )?))
        }
        DataType::F64 => {
            try_reserve(pool, len * 8 + nulls_bytes)?;
            let mut vals = vec![0.0f64; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (i, (src, slot)) in sources.iter().zip(vals.iter_mut()).enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some((col, row)) = src
                    && let Some(v) = read_f64(col, *row)
                {
                    *slot = v;
                    let _ = nulls.set(i, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::F64(vals),
                Some(nulls),
            )?))
        }
        DataType::Decimal128 { precision, scale } => {
            try_reserve(pool, len * 16 + nulls_bytes)?;
            let mut vals = vec![0i128; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (i, (src, slot)) in sources.iter().zip(vals.iter_mut()).enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some((col, row)) = src
                    && let Some(v) = read_decimal(col, *row)
                {
                    *slot = v;
                    let _ = nulls.set(i, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vals,
                    precision: *precision,
                    scale: *scale,
                },
                Some(nulls),
            )?))
        }
        DataType::TimestampNs { utc } => {
            try_reserve(pool, len * 8 + nulls_bytes)?;
            let mut vals = vec![0i64; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (i, (src, slot)) in sources.iter().zip(vals.iter_mut()).enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some((col, row)) = src
                    && let Some(v) = read_timestamp(col, *row)
                {
                    *slot = v;
                    let _ = nulls.set(i, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: vals,
                    utc: *utc,
                },
                Some(nulls),
            )?))
        }
        DataType::Date => {
            try_reserve(pool, len * 4 + nulls_bytes)?;
            let mut vals = vec![0i32; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (i, (src, slot)) in sources.iter().zip(vals.iter_mut()).enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some((col, row)) = src
                    && let Some(v) = read_date(col, *row)
                {
                    *slot = v;
                    let _ = nulls.set(i, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::Date(vals),
                Some(nulls),
            )?))
        }
        DataType::Interval => {
            try_reserve(pool, len * size_of::<Interval>() + nulls_bytes)?;
            let mut vals = vec![Interval::default(); len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (i, (src, slot)) in sources.iter().zip(vals.iter_mut()).enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some((col, row)) = src
                    && let Some(v) = read_interval(col, *row)
                {
                    *slot = v;
                    let _ = nulls.set(i, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::Interval(vals),
                Some(nulls),
            )?))
        }
        DataType::Utf8 => {
            let mut total_bytes = 0usize;
            for src in sources {
                if let Some((col, row)) = src
                    && let Some(s) = read_str(col, *row)
                {
                    total_bytes =
                        total_bytes
                            .checked_add(s.len())
                            .ok_or(ColumnError::Overflow {
                                reason: "utf8 byte length overflow",
                            })?;
                }
            }
            let _ = u32::try_from(total_bytes).map_err(|_| ColumnError::Overflow {
                reason: "utf8 byte length exceeds u32::MAX",
            })?;
            try_reserve(pool, (len + 1) * 4 + total_bytes + nulls_bytes)?;
            let mut offsets = Vec::with_capacity(len + 1);
            offsets.push(0u32);
            let mut bytes = Vec::with_capacity(total_bytes);
            let mut nulls = Bitmap::new_zeroed(len);
            for (i, src) in sources.iter().enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some((col, row)) = src
                    && let Some(s) = read_str(col, *row)
                {
                    bytes.extend_from_slice(s.as_bytes());
                    let _ = nulls.set(i, true);
                }
                offsets.push(bytes.len() as u32);
            }
            let utf8 = Utf8Data::try_new(offsets, bytes)?;
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::Utf8(utf8),
                Some(nulls),
            )?))
        }
        DataType::Binary => {
            let mut total_bytes = 0usize;
            for src in sources {
                if let Some((col, row)) = src
                    && let Some(b) = read_bytes(col, *row)
                {
                    total_bytes =
                        total_bytes
                            .checked_add(b.len())
                            .ok_or(ColumnError::Overflow {
                                reason: "binary byte length overflow",
                            })?;
                }
            }
            let _ = u32::try_from(total_bytes).map_err(|_| ColumnError::Overflow {
                reason: "binary byte length exceeds u32::MAX",
            })?;
            try_reserve(pool, (len + 1) * 4 + total_bytes + nulls_bytes)?;
            let mut offsets = Vec::with_capacity(len + 1);
            offsets.push(0u32);
            let mut bytes = Vec::with_capacity(total_bytes);
            let mut nulls = Bitmap::new_zeroed(len);
            for (i, src) in sources.iter().enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some((col, row)) = src
                    && let Some(b) = read_bytes(col, *row)
                {
                    bytes.extend_from_slice(b);
                    let _ = nulls.set(i, true);
                }
                offsets.push(bytes.len() as u32);
            }
            let bin = BinaryData::try_new(offsets, bytes)?;
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::Binary(bin),
                Some(nulls),
            )?))
        }
        DataType::FixedBytes8 => {
            try_reserve(pool, len * 8 + nulls_bytes)?;
            let mut vals = vec![[0u8; 8]; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (i, (src, slot)) in sources.iter().zip(vals.iter_mut()).enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some((col, row)) = src
                    && let Some(b) = read_bytes(col, *row)
                {
                    let arr: [u8; 8] = b.try_into().map_err(|_| ColumnError::InvalidBatch {
                        reason: "expected 8-byte binary value",
                    })?;
                    *slot = arr;
                    let _ = nulls.set(i, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::FixedBytes8(vals),
                Some(nulls),
            )?))
        }
        DataType::FixedBytes16 => {
            try_reserve(pool, len * 16 + nulls_bytes)?;
            let mut vals = vec![[0u8; 16]; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (i, (src, slot)) in sources.iter().zip(vals.iter_mut()).enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some((col, row)) = src
                    && let Some(b) = read_bytes(col, *row)
                {
                    let arr: [u8; 16] = b.try_into().map_err(|_| ColumnError::InvalidBatch {
                        reason: "expected 16-byte binary value",
                    })?;
                    *slot = arr;
                    let _ = nulls.set(i, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::FixedBytes16(vals),
                Some(nulls),
            )?))
        }
        DataType::List(inner_dt) => build_list_from_sources(inner_dt, sources, pool, ctx, before),
        DataType::MapUtf8 => build_map_from_sources(sources, pool, ctx, before),
        DataType::Events => build_events_from_sources(sources, pool, ctx, before),
        DataType::Links => build_links_from_sources(sources, pool, ctx, before),
    }
}

fn build_list_from_sources(
    inner_dt: &DataType,
    sources: &[Option<(&Column, usize)>],
    pool: &mut Option<&mut Reservation>,
    ctx: &EvalContext,
    before: usize,
) -> Result<Column, QueryError> {
    let len = sources.len();
    let mut offsets = Vec::with_capacity(len + 1);
    offsets.push(0u32);
    let mut child_scalars = Vec::new();
    let mut nulls = Bitmap::new_zeroed(len);

    for (i, src) in sources.iter().enumerate() {
        if (i & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if let Some((col, row)) = src {
            let sv = read_scalar(col, *row);
            if let ScalarValue::List { values, .. } = sv {
                let _ = nulls.set(i, true);
                child_scalars.extend(values);
            }
        }
        let end = u32::try_from(child_scalars.len()).map_err(|_| ColumnError::Overflow {
            reason: "list length exceeds u32::MAX",
        })?;
        offsets.push(end);
    }

    let est: usize = (len + 1) * 4
        + bitmap_mem_bytes(len)
        + child_scalars
            .iter()
            .map(ScalarValue::heap_bytes)
            .sum::<usize>();
    try_reserve(pool, est)?;

    let child_cols: Vec<Column> = child_scalars
        .into_iter()
        .map(|s| Column::try_new_const(s, 1))
        .collect::<Result<_, _>>()?;
    let child_sources: Vec<Option<(&Column, usize)>> = child_cols
        .iter()
        .map(|c| (!c.is_null(0)).then_some((c, 0)))
        .collect();
    let values = build_flat_from_sources(inner_dt, &child_sources, pool, ctx, before)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::List {
            item_type: inner_dt.clone(),
            offsets,
            values: Box::new(values),
        },
        Some(nulls),
    )?))
}

fn build_map_from_sources(
    sources: &[Option<(&Column, usize)>],
    pool: &mut Option<&mut Reservation>,
    ctx: &EvalContext,
    before: usize,
) -> Result<Column, QueryError> {
    let len = sources.len();
    let mut offsets = Vec::with_capacity(len + 1);
    offsets.push(0u32);
    let mut keys: Vec<String> = Vec::new();
    let mut vals: Vec<String> = Vec::new();
    let mut val_valid: Vec<bool> = Vec::new();
    let mut nulls = Bitmap::new_zeroed(len);

    for (i, src) in sources.iter().enumerate() {
        if (i & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if let Some((col, row)) = src
            && let ScalarValue::MapUtf8(entries) = read_scalar(col, *row)
        {
            let _ = nulls.set(i, true);
            for (k, mv) in entries {
                keys.push(k);
                val_valid.push(mv.is_some());
                vals.push(mv.unwrap_or_default());
            }
        }
        let end = u32::try_from(keys.len()).map_err(|_| ColumnError::Overflow {
            reason: "map entry count exceeds u32::MAX",
        })?;
        offsets.push(end);
    }
    let key_bytes: usize = keys.iter().map(String::len).sum();
    let val_bytes: usize = vals.iter().map(String::len).sum();
    try_reserve(
        pool,
        (len + 1) * 4 + (keys.len() + 1) * 8 + key_bytes + val_bytes + bitmap_mem_bytes(len),
    )?;
    let keys_data = Utf8Data::try_from_slices(keys.iter().map(String::as_str))?;
    let vals_data = Utf8Data::try_from_slices(vals.iter().map(String::as_str))?;
    let val_nulls = Bitmap::from_bools(&val_valid);
    let map = MapData::try_new(offsets, keys_data, vals_data, Some(val_nulls))?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::MapUtf8(Box::new(map)),
        Some(nulls),
    )?))
}

fn build_events_from_sources(
    sources: &[Option<(&Column, usize)>],
    pool: &mut Option<&mut Reservation>,
    ctx: &EvalContext,
    before: usize,
) -> Result<Column, QueryError> {
    let len = sources.len();
    let mut offsets = Vec::with_capacity(len + 1);
    offsets.push(0u32);
    let mut items: Vec<EventItem> = Vec::new();
    let mut nulls = Bitmap::new_zeroed(len);
    for (i, src) in sources.iter().enumerate() {
        if (i & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if let Some((col, row)) = src
            && let ScalarValue::Events(evs) = read_scalar(col, *row)
        {
            let _ = nulls.set(i, true);
            items.extend(evs);
        }
        let end = u32::try_from(items.len()).map_err(|_| ColumnError::Overflow {
            reason: "events count exceeds u32::MAX",
        })?;
        offsets.push(end);
    }
    let est: usize = (len + 1) * 4
        + items.len() * 16
        + items
            .iter()
            .map(|e| e.name.len() + e.attributes.as_ref().map_or(0, String::len))
            .sum::<usize>();
    try_reserve(pool, est)?;
    let times: Vec<i64> = items.iter().map(|e| e.time).collect();
    let names = Utf8Data::try_from_slices(items.iter().map(|e| e.name.as_str()))?;
    let attrs =
        Utf8Data::try_from_slices(items.iter().map(|e| e.attributes.as_deref().unwrap_or("")))?;
    let attr_valid: Vec<bool> = items.iter().map(|e| e.attributes.is_some()).collect();
    let attr_nulls = Bitmap::from_bools(&attr_valid);
    let ev_data = EventsData::try_new(offsets, times, names, attrs, Some(attr_nulls))?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Events(Box::new(ev_data)),
        Some(nulls),
    )?))
}

fn build_links_from_sources(
    sources: &[Option<(&Column, usize)>],
    pool: &mut Option<&mut Reservation>,
    ctx: &EvalContext,
    before: usize,
) -> Result<Column, QueryError> {
    let len = sources.len();
    let mut offsets = Vec::with_capacity(len + 1);
    offsets.push(0u32);
    let mut items: Vec<LinkItem> = Vec::new();
    let mut nulls = Bitmap::new_zeroed(len);
    for (i, src) in sources.iter().enumerate() {
        if (i & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if let Some((col, row)) = src
            && let ScalarValue::Links(lks) = read_scalar(col, *row)
        {
            let _ = nulls.set(i, true);
            items.extend(lks);
        }
        let end = u32::try_from(items.len()).map_err(|_| ColumnError::Overflow {
            reason: "links count exceeds u32::MAX",
        })?;
        offsets.push(end);
    }
    let est: usize = (len + 1) * 4
        + items.len() * 32
        + items
            .iter()
            .map(|l| l.attributes.as_ref().map_or(0, String::len))
            .sum::<usize>();
    try_reserve(pool, est)?;
    let trace_ids: Vec<[u8; 16]> = items.iter().map(|l| l.trace_id).collect();
    let span_ids: Vec<[u8; 8]> = items.iter().map(|l| l.span_id).collect();
    let attrs =
        Utf8Data::try_from_slices(items.iter().map(|l| l.attributes.as_deref().unwrap_or("")))?;
    let attr_valid: Vec<bool> = items.iter().map(|l| l.attributes.is_some()).collect();
    let attr_nulls = Bitmap::from_bools(&attr_valid);
    let lk_data = LinksData::try_new(offsets, trace_ids, span_ids, attrs, Some(attr_nulls))?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Links(Box::new(lk_data)),
        Some(nulls),
    )?))
}

// ---------------------------------------------------------------------------
// Cast & TryCast
// ---------------------------------------------------------------------------

fn eval_cast(
    inner: &Expr,
    target: &DataType,
    mode: CastMode,
    batch: &Batch,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let before = pool_size(&pool);
    let raw_len = batch.raw_len();
    let src_dt = inner.data_type();

    let src = eval_expr_masked(inner, batch, active, ctx, pool.as_deref_mut())?;

    if src_dt == *target {
        return Ok(src);
    }

    if let (DataType::TimestampNs { .. }, DataType::TimestampNs { utc }) = (&src_dt, target) {
        return retag_timestamp_utc(src, *utc);
    }

    if matches!(src_dt, DataType::Null) || matches!(target, DataType::Null) {
        return Ok(Column::Const {
            value: ScalarValue::Null(target.clone()),
            len: raw_len,
        });
    }

    match (&src_dt, target) {
        (DataType::I64, DataType::F64) => {
            cast_fixed(raw_len, active, ctx, &mut pool, before, |row| {
                Ok(read_i64(&src, row).map(|v| v as f64))
            })
            .map(|(vals, nulls)| {
                FlatColumn::try_new(FlatData::F64(vals), Some(nulls)).map(Column::Flat)
            })?
            .map_err(Into::into)
        }
        (DataType::F64, DataType::I64) => {
            cast_fixed(raw_len, active, ctx, &mut pool, before, |row| {
                let Some(v) = read_f64(&src, row) else {
                    return Ok(None);
                };
                match f64_to_i64(v) {
                    Some(i) => Ok(Some(i)),
                    None => cast_err_or_null(
                        mode,
                        "cannot cast non-finite or out-of-range float to integer",
                    ),
                }
            })
            .map(|(vals, nulls)| {
                FlatColumn::try_new(FlatData::I64(vals), Some(nulls)).map(Column::Flat)
            })?
            .map_err(Into::into)
        }
        (DataType::I64, DataType::Bool) => {
            try_reserve(&mut pool, bitmap_mem_bytes(raw_len) * 2)?;
            let mut vals = Bitmap::new_zeroed(raw_len);
            let mut nulls = Bitmap::new_zeroed(raw_len);
            for row in 0..raw_len {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, &mut pool, before)?;
                }
                if is_active(active, row)
                    && let Some(v) = read_i64(&src, row)
                {
                    let _ = nulls.set(row, true);
                    if v != 0 {
                        let _ = vals.set(row, true);
                    }
                }
            }
            build_bool_column(raw_len, vals, Some(nulls))
        }
        (DataType::Bool, DataType::I64) => {
            cast_fixed(raw_len, active, ctx, &mut pool, before, |row| {
                Ok(read_bool(&src, row).map(i64::from))
            })
            .map(|(vals, nulls)| {
                FlatColumn::try_new(FlatData::I64(vals), Some(nulls)).map(Column::Flat)
            })?
            .map_err(Into::into)
        }
        (DataType::I64, DataType::Decimal128 { precision, scale }) => {
            let p = *precision;
            let s = *scale;
            cast_fixed128(raw_len, active, ctx, &mut pool, before, |row| {
                let Some(v) = read_i64(&src, row) else {
                    return Ok(None);
                };
                match i64_to_decimal(v, p, s) {
                    Ok(d) => Ok(Some(d)),
                    Err(e) => match mode {
                        CastMode::Strict => Err(e),
                        CastMode::Try => Ok(None),
                    },
                }
            })
            .map(|(vals, nulls)| {
                FlatColumn::try_new(
                    FlatData::Decimal128 {
                        values: vals,
                        precision: p,
                        scale: s,
                    },
                    Some(nulls),
                )
                .map(Column::Flat)
            })?
            .map_err(Into::into)
        }
        (DataType::F64, DataType::Decimal128 { precision, scale }) => {
            let p = *precision;
            let s = *scale;
            cast_fixed128(raw_len, active, ctx, &mut pool, before, |row| {
                let Some(v) = read_f64(&src, row) else {
                    return Ok(None);
                };
                match f64_to_decimal(v, p, s) {
                    Ok(d) => Ok(Some(d)),
                    Err(e) => match mode {
                        CastMode::Strict => Err(e),
                        CastMode::Try => Ok(None),
                    },
                }
            })
            .map(|(vals, nulls)| {
                FlatColumn::try_new(
                    FlatData::Decimal128 {
                        values: vals,
                        precision: p,
                        scale: s,
                    },
                    Some(nulls),
                )
                .map(Column::Flat)
            })?
            .map_err(Into::into)
        }
        (DataType::Decimal128 { scale, .. }, DataType::I64) => {
            let s = *scale;
            cast_fixed(raw_len, active, ctx, &mut pool, before, |row| {
                let Some(v) = read_decimal(&src, row) else {
                    return Ok(None);
                };
                match decimal_to_i64(v, s) {
                    Some(i) => Ok(Some(i)),
                    None => cast_err_or_null(mode, "decimal out of integer range"),
                }
            })
            .map(|(vals, nulls)| {
                FlatColumn::try_new(FlatData::I64(vals), Some(nulls)).map(Column::Flat)
            })?
            .map_err(Into::into)
        }
        (DataType::Decimal128 { scale, .. }, DataType::F64) => {
            let s = *scale;
            cast_fixed(raw_len, active, ctx, &mut pool, before, |row| {
                Ok(read_decimal(&src, row).map(|v| (v as f64) / 10f64.powi(i32::from(s))))
            })
            .map(|(vals, nulls)| {
                FlatColumn::try_new(FlatData::F64(vals), Some(nulls)).map(Column::Flat)
            })?
            .map_err(Into::into)
        }
        (
            DataType::Decimal128 { scale: s1, .. },
            DataType::Decimal128 {
                precision: p2,
                scale: s2,
            },
        ) => {
            let s1 = *s1;
            let p2 = *p2;
            let s2 = *s2;
            cast_fixed128(raw_len, active, ctx, &mut pool, before, |row| {
                let Some(v) = read_decimal(&src, row) else {
                    return Ok(None);
                };
                match rescale_decimal(v, s1, p2, s2) {
                    Ok(d) => Ok(Some(d)),
                    Err(e) => match mode {
                        CastMode::Strict => Err(e),
                        CastMode::Try => Ok(None),
                    },
                }
            })
            .map(|(vals, nulls)| {
                FlatColumn::try_new(
                    FlatData::Decimal128 {
                        values: vals,
                        precision: p2,
                        scale: s2,
                    },
                    Some(nulls),
                )
                .map(Column::Flat)
            })?
            .map_err(Into::into)
        }
        (DataType::Utf8, DataType::Bool) => {
            try_reserve(&mut pool, bitmap_mem_bytes(raw_len) * 2)?;
            let mut vals = Bitmap::new_zeroed(raw_len);
            let mut nulls = Bitmap::new_zeroed(raw_len);
            for row in 0..raw_len {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, &mut pool, before)?;
                }
                if is_active(active, row)
                    && let Some(s) = read_str(&src, row)
                {
                    match parse_bool_str(s) {
                        Some(b) => {
                            let _ = nulls.set(row, true);
                            if b {
                                let _ = vals.set(row, true);
                            }
                        }
                        None => {
                            if mode == CastMode::Strict {
                                return Err(QueryError::failed("cannot cast string to boolean"));
                            }
                        }
                    }
                }
            }
            build_bool_column(raw_len, vals, Some(nulls))
        }
        (DataType::Utf8, DataType::I64) => {
            cast_fixed(raw_len, active, ctx, &mut pool, before, |row| {
                let Some(s) = read_str(&src, row) else {
                    return Ok(None);
                };
                match s.trim_ascii().parse::<i64>() {
                    Ok(v) => Ok(Some(v)),
                    Err(_) => cast_err_or_null(mode, "cannot cast string to integer"),
                }
            })
            .map(|(vals, nulls)| {
                FlatColumn::try_new(FlatData::I64(vals), Some(nulls)).map(Column::Flat)
            })?
            .map_err(Into::into)
        }
        (DataType::Utf8, DataType::F64) => {
            cast_fixed(raw_len, active, ctx, &mut pool, before, |row| {
                let Some(s) = read_str(&src, row) else {
                    return Ok(None);
                };
                match s.trim_ascii().parse::<f64>() {
                    Ok(v) => Ok(Some(v)),
                    Err(_) => cast_err_or_null(mode, "cannot cast string to float"),
                }
            })
            .map(|(vals, nulls)| {
                FlatColumn::try_new(FlatData::F64(vals), Some(nulls)).map(Column::Flat)
            })?
            .map_err(Into::into)
        }
        (DataType::Utf8, DataType::Decimal128 { precision, scale }) => {
            let p = *precision;
            let s = *scale;
            cast_fixed128(raw_len, active, ctx, &mut pool, before, |row| {
                let Some(text) = read_str(&src, row) else {
                    return Ok(None);
                };
                match parse_decimal_str(text, p, s) {
                    Ok(d) => Ok(Some(d)),
                    Err(e) => match mode {
                        CastMode::Strict => Err(e),
                        CastMode::Try => Ok(None),
                    },
                }
            })
            .map(|(vals, nulls)| {
                FlatColumn::try_new(
                    FlatData::Decimal128 {
                        values: vals,
                        precision: p,
                        scale: s,
                    },
                    Some(nulls),
                )
                .map(Column::Flat)
            })?
            .map_err(Into::into)
        }
        (DataType::I64, DataType::TimestampNs { utc }) => {
            let u = *utc;
            cast_fixed(raw_len, active, ctx, &mut pool, before, |row| {
                Ok(read_i64(&src, row))
            })
            .map(|(vals, nulls)| {
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: vals,
                        utc: u,
                    },
                    Some(nulls),
                )
                .map(Column::Flat)
            })?
            .map_err(Into::into)
        }
        (DataType::TimestampNs { .. }, DataType::I64) => {
            cast_fixed(raw_len, active, ctx, &mut pool, before, |row| {
                Ok(read_timestamp(&src, row))
            })
            .map(|(vals, nulls)| {
                FlatColumn::try_new(FlatData::I64(vals), Some(nulls)).map(Column::Flat)
            })?
            .map_err(Into::into)
        }
        (DataType::Date, DataType::TimestampNs { utc }) => {
            let u = *utc;
            cast_fixed(raw_len, active, ctx, &mut pool, before, |row| {
                let Some(days) = read_date(&src, row) else {
                    return Ok(None);
                };
                match i64::from(days).checked_mul(NS_PER_DAY) {
                    Some(ns) => Ok(Some(ns)),
                    None => cast_err_or_null(mode, "date to timestamp overflow"),
                }
            })
            .map(|(vals, nulls)| {
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: vals,
                        utc: u,
                    },
                    Some(nulls),
                )
                .map(Column::Flat)
            })?
            .map_err(Into::into)
        }
        (DataType::TimestampNs { .. }, DataType::Date) => {
            cast_fixed(raw_len, active, ctx, &mut pool, before, |row| {
                let Some(ns) = read_timestamp(&src, row) else {
                    return Ok(None);
                };
                let days = ns.div_euclid(NS_PER_DAY);
                match i32::try_from(days) {
                    Ok(d) => Ok(Some(d)),
                    Err(_) => cast_err_or_null(mode, "timestamp to date overflow"),
                }
            })
            .map(|(vals, nulls)| {
                FlatColumn::try_new(FlatData::Date(vals), Some(nulls)).map(Column::Flat)
            })?
            .map_err(Into::into)
        }
        (DataType::Utf8, DataType::Binary) => {
            let mut total_bytes = 0usize;
            for row in 0..raw_len {
                if is_active(active, row)
                    && let Some(s) = read_str(&src, row)
                {
                    total_bytes += s.len();
                }
            }
            try_reserve(
                &mut pool,
                (raw_len + 1) * 4 + total_bytes + bitmap_mem_bytes(raw_len),
            )?;
            let mut offsets = Vec::with_capacity(raw_len + 1);
            offsets.push(0u32);
            let mut bytes = Vec::with_capacity(total_bytes);
            let mut nulls = Bitmap::new_zeroed(raw_len);
            for row in 0..raw_len {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, &mut pool, before)?;
                }
                if is_active(active, row)
                    && let Some(s) = read_str(&src, row)
                {
                    bytes.extend_from_slice(s.as_bytes());
                    let _ = nulls.set(row, true);
                }
                offsets.push(bytes.len() as u32);
            }
            let bin = BinaryData::try_new(offsets, bytes)?;
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::Binary(bin),
                Some(nulls),
            )?))
        }
        (DataType::Utf8, DataType::FixedBytes8) => {
            try_reserve(&mut pool, raw_len * 8 + bitmap_mem_bytes(raw_len))?;
            let mut vals = vec![[0u8; 8]; raw_len];
            let mut nulls = Bitmap::new_zeroed(raw_len);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, &mut pool, before)?;
                }
                if is_active(active, row)
                    && let Some(s) = read_str(&src, row)
                {
                    match decode_fixed_hex::<8>(s) {
                        Some(arr) => {
                            *slot = arr;
                            let _ = nulls.set(row, true);
                        }
                        None => {
                            if mode == CastMode::Strict {
                                return Err(QueryError::failed("invalid 8-byte hex string"));
                            }
                        }
                    }
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::FixedBytes8(vals),
                Some(nulls),
            )?))
        }
        (DataType::Utf8, DataType::FixedBytes16) => {
            try_reserve(&mut pool, raw_len * 16 + bitmap_mem_bytes(raw_len))?;
            let mut vals = vec![[0u8; 16]; raw_len];
            let mut nulls = Bitmap::new_zeroed(raw_len);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, &mut pool, before)?;
                }
                if is_active(active, row)
                    && let Some(s) = read_str(&src, row)
                {
                    match decode_fixed_hex::<16>(s) {
                        Some(arr) => {
                            *slot = arr;
                            let _ = nulls.set(row, true);
                        }
                        None => {
                            if mode == CastMode::Strict {
                                return Err(QueryError::failed("invalid 16-byte hex string"));
                            }
                        }
                    }
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::FixedBytes16(vals),
                Some(nulls),
            )?))
        }
        (DataType::FixedBytes8 | DataType::FixedBytes16, DataType::Binary) => {
            let mut total_bytes = 0usize;
            for row in 0..raw_len {
                if is_active(active, row)
                    && let Some(b) = read_bytes(&src, row)
                {
                    total_bytes += b.len();
                }
            }
            try_reserve(
                &mut pool,
                (raw_len + 1) * 4 + total_bytes + bitmap_mem_bytes(raw_len),
            )?;
            let mut offsets = Vec::with_capacity(raw_len + 1);
            offsets.push(0u32);
            let mut bytes = Vec::with_capacity(total_bytes);
            let mut nulls = Bitmap::new_zeroed(raw_len);
            for row in 0..raw_len {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, &mut pool, before)?;
                }
                if is_active(active, row)
                    && let Some(b) = read_bytes(&src, row)
                {
                    bytes.extend_from_slice(b);
                    let _ = nulls.set(row, true);
                }
                offsets.push(bytes.len() as u32);
            }
            let bin = BinaryData::try_new(offsets, bytes)?;
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::Binary(bin),
                Some(nulls),
            )?))
        }
        (DataType::Binary, DataType::FixedBytes8) => {
            try_reserve(&mut pool, raw_len * 8 + bitmap_mem_bytes(raw_len))?;
            let mut vals = vec![[0u8; 8]; raw_len];
            let mut nulls = Bitmap::new_zeroed(raw_len);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, &mut pool, before)?;
                }
                if is_active(active, row)
                    && let Some(b) = read_bytes(&src, row)
                {
                    match <[u8; 8]>::try_from(b) {
                        Ok(arr) => {
                            *slot = arr;
                            let _ = nulls.set(row, true);
                        }
                        Err(_) => {
                            if mode == CastMode::Strict {
                                return Err(QueryError::failed("binary length is not 8 bytes"));
                            }
                        }
                    }
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::FixedBytes8(vals),
                Some(nulls),
            )?))
        }
        (DataType::Binary, DataType::FixedBytes16) => {
            try_reserve(&mut pool, raw_len * 16 + bitmap_mem_bytes(raw_len))?;
            let mut vals = vec![[0u8; 16]; raw_len];
            let mut nulls = Bitmap::new_zeroed(raw_len);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, &mut pool, before)?;
                }
                if is_active(active, row)
                    && let Some(b) = read_bytes(&src, row)
                {
                    match <[u8; 16]>::try_from(b) {
                        Ok(arr) => {
                            *slot = arr;
                            let _ = nulls.set(row, true);
                        }
                        Err(_) => {
                            if mode == CastMode::Strict {
                                return Err(QueryError::failed("binary length is not 16 bytes"));
                            }
                        }
                    }
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::FixedBytes16(vals),
                Some(nulls),
            )?))
        }
        (DataType::List(src_inner), DataType::List(tgt_inner))
            if matches!(src_inner.as_ref(), DataType::Null) =>
        {
            let mut offsets = Vec::with_capacity(raw_len + 1);
            offsets.push(0u32);
            let mut total_items = 0usize;
            let mut nulls = Bitmap::new_zeroed(raw_len);
            for row in 0..raw_len {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, &mut pool, before)?;
                }
                if is_active(active, row)
                    && let Some(lref) = list_ref_at(&src, row)
                {
                    let n = match lref {
                        ListRef::Flat { start, end, .. } => end - start,
                        ListRef::Const(s) => s.len(),
                    };
                    total_items = total_items.checked_add(n).ok_or(ColumnError::Overflow {
                        reason: "list cast element count overflow",
                    })?;
                    let _ = nulls.set(row, true);
                }
                let end_u32 = u32::try_from(total_items).map_err(|_| ColumnError::Overflow {
                    reason: "list cast element count exceeds u32::MAX",
                })?;
                offsets.push(end_u32);
            }
            try_reserve(&mut pool, (raw_len + 1) * 4 + bitmap_mem_bytes(raw_len))?;
            let values = Column::Const {
                value: ScalarValue::Null((**tgt_inner).clone()),
                len: total_items,
            };
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::List {
                    item_type: (**tgt_inner).clone(),
                    offsets,
                    values: Box::new(values),
                },
                Some(nulls),
            )?))
        }
        (DataType::Utf8, DataType::TimestampNs { utc }) => {
            let u = *utc;
            cast_fixed(raw_len, active, ctx, &mut pool, before, |row| {
                let Some(s) = read_str(&src, row) else {
                    return Ok(None);
                };
                match crate::fn_time::parse_timestamp_ns(s, u) {
                    Ok(ns) => Ok(Some(ns)),
                    Err(e) => match mode {
                        CastMode::Strict => Err(e),
                        CastMode::Try => Ok(None),
                    },
                }
            })
            .map(|(vals, nulls)| {
                FlatColumn::try_new(
                    FlatData::TimestampNs {
                        values: vals,
                        utc: u,
                    },
                    Some(nulls),
                )
                .map(Column::Flat)
            })?
            .map_err(Into::into)
        }
        (DataType::Utf8, DataType::Date) => {
            cast_fixed(raw_len, active, ctx, &mut pool, before, |row| {
                let Some(s) = read_str(&src, row) else {
                    return Ok(None);
                };
                match crate::fn_time::parse_date_days(s) {
                    Ok(d) => Ok(Some(d)),
                    Err(e) => match mode {
                        CastMode::Strict => Err(e),
                        CastMode::Try => Ok(None),
                    },
                }
            })
            .map(|(vals, nulls)| {
                FlatColumn::try_new(FlatData::Date(vals), Some(nulls)).map(Column::Flat)
            })?
            .map_err(Into::into)
        }
        (_, DataType::Utf8) => {
            cast_to_utf8(&src, &src_dt, mode, raw_len, active, ctx, &mut pool, before)
        }
        _ => Err(QueryError::failed("unsupported cast")),
    }
}

fn retag_timestamp_utc(src: Column, utc: bool) -> Result<Column, QueryError> {
    match src {
        Column::Flat(fc) => {
            let nulls = fc.nulls().cloned();
            let FlatData::TimestampNs { values, .. } = fc.data() else {
                return Err(QueryError::failed("expected timestamp column"));
            };
            let new_fc = FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: values.clone(),
                    utc,
                },
                nulls,
            )?;
            Ok(Column::Flat(new_fc))
        }
        Column::Const { value, len } => {
            let new_val = match value {
                ScalarValue::TimestampNs { value, .. } => ScalarValue::TimestampNs { value, utc },
                ScalarValue::Null(_) => ScalarValue::Null(DataType::TimestampNs { utc }),
                _ => return Err(QueryError::failed("expected timestamp scalar")),
            };
            Ok(Column::Const {
                value: new_val,
                len,
            })
        }
        Column::Dict(d) => {
            let Column::Flat(new_vals) =
                retag_timestamp_utc(Column::Flat(d.values().clone()), utc)?
            else {
                return Err(QueryError::failed("expected flat dict values"));
            };
            Ok(Column::Dict(cetra_column::DictColumn::try_new(
                new_vals,
                d.codes().to_vec(),
                d.nulls().cloned(),
            )?))
        }
        Column::Rle(r) => {
            let Column::Flat(new_vals) =
                retag_timestamp_utc(Column::Flat(r.values().clone()), utc)?
            else {
                return Err(QueryError::failed("expected flat rle values"));
            };
            Ok(Column::Rle(cetra_column::RleColumn::try_new(
                new_vals,
                r.run_ends().to_vec(),
            )?))
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "scalar-to-utf8 cast helper avoids extra wrapper struct"
)]
fn cast_to_utf8(
    src: &Column,
    src_dt: &DataType,
    mode: CastMode,
    raw_len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
) -> Result<Column, QueryError> {
    let mut rendered: Vec<Option<String>> = Vec::with_capacity(raw_len);
    let mut total_bytes = 0usize;
    for row in 0..raw_len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) || src.is_null(row) {
            rendered.push(None);
            continue;
        }
        let s = match src_dt {
            DataType::Bool => read_bool(src, row).map(|b| b.to_string()),
            DataType::I64 => read_i64(src, row).map(|v| v.to_string()),
            DataType::F64 => read_f64(src, row).map(|v| v.to_string()),
            DataType::Decimal128 { scale, .. } => {
                read_decimal(src, row).map(|v| format_decimal(v, *scale))
            }
            DataType::Binary => {
                let Some(b) = read_bytes(src, row) else {
                    rendered.push(None);
                    continue;
                };
                match std::str::from_utf8(b) {
                    Ok(s) => Some(s.to_owned()),
                    Err(_) => match mode {
                        CastMode::Strict => {
                            return Err(QueryError::failed("invalid utf-8 in binary cast"));
                        }
                        CastMode::Try => None,
                    },
                }
            }
            DataType::FixedBytes8 | DataType::FixedBytes16 => read_bytes(src, row).map(hex::encode),
            DataType::TimestampNs { utc } => {
                read_timestamp(src, row).map(|ns| crate::fn_time::format_timestamp_ns(ns, *utc))
            }
            DataType::Date => {
                let Some(days) = read_date(src, row) else {
                    rendered.push(None);
                    continue;
                };
                match days
                    .checked_add(UNIX_EPOCH_CE_DAYS)
                    .and_then(NaiveDate::from_num_days_from_ce_opt)
                {
                    Some(d) => Some(d.format("%Y-%m-%d").to_string()),
                    None => match mode {
                        CastMode::Strict => {
                            return Err(QueryError::failed("date out of range for string cast"));
                        }
                        CastMode::Try => None,
                    },
                }
            }
            _ => return Err(QueryError::failed("unsupported cast to utf8")),
        };
        if let Some(ref st) = s {
            total_bytes = total_bytes
                .checked_add(st.len())
                .ok_or(ColumnError::Overflow {
                    reason: "utf8 cast byte length overflow",
                })?;
        }
        rendered.push(s);
    }

    let _ = u32::try_from(total_bytes).map_err(|_| ColumnError::Overflow {
        reason: "utf8 cast byte length exceeds u32::MAX",
    })?;
    try_reserve(
        pool,
        (raw_len + 1) * 4 + total_bytes + bitmap_mem_bytes(raw_len),
    )?;
    let mut offsets = Vec::with_capacity(raw_len + 1);
    offsets.push(0u32);
    let mut bytes = Vec::with_capacity(total_bytes);
    let mut nulls = Bitmap::new_zeroed(raw_len);

    for (row, opt) in rendered.iter().enumerate() {
        if let Some(s) = opt {
            bytes.extend_from_slice(s.as_bytes());
            let _ = nulls.set(row, true);
        }
        offsets.push(bytes.len() as u32);
    }
    let utf8 = Utf8Data::try_new(offsets, bytes)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Utf8(utf8),
        Some(nulls),
    )?))
}

fn cast_fixed<T: Default + Copy>(
    raw_len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
    mut f: impl FnMut(usize) -> Result<Option<T>, QueryError>,
) -> Result<(Vec<T>, Bitmap), QueryError> {
    try_reserve(pool, raw_len * size_of::<T>() + bitmap_mem_bytes(raw_len))?;
    let mut vals = vec![T::default(); raw_len];
    let mut nulls = Bitmap::new_zeroed(raw_len);
    for (row, slot) in vals.iter_mut().enumerate() {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row)
            && let Some(v) = f(row)?
        {
            *slot = v;
            let _ = nulls.set(row, true);
        }
    }
    Ok((vals, nulls))
}

fn cast_fixed128(
    raw_len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
    before: usize,
    f: impl FnMut(usize) -> Result<Option<i128>, QueryError>,
) -> Result<(Vec<i128>, Bitmap), QueryError> {
    cast_fixed(raw_len, active, ctx, pool, before, f)
}

fn cast_err_or_null<T>(mode: CastMode, msg: &str) -> Result<Option<T>, QueryError> {
    match mode {
        CastMode::Strict => Err(QueryError::failed(msg)),
        CastMode::Try => Ok(None),
    }
}

fn f64_to_i64(v: f64) -> Option<i64> {
    if !v.is_finite() {
        return None;
    }
    let t = v.trunc();
    if t < (i64::MIN as f64) || t >= 9223372036854775808.0 {
        return None;
    }
    Some(t as i64)
}

fn parse_bool_str(s: &str) -> Option<bool> {
    match s.trim_ascii().to_ascii_lowercase().as_str() {
        "true" | "t" | "yes" | "y" | "1" | "on" => Some(true),
        "false" | "f" | "no" | "n" | "0" | "off" => Some(false),
        _ => None,
    }
}

fn decode_fixed_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    hex::decode_to_slice(s, &mut out).ok()?;
    Some(out)
}

fn i64_to_decimal(v: i64, precision: u8, scale: i8) -> Result<i128, QueryError> {
    let scaled = if scale >= 0 {
        let factor = 10i128
            .checked_pow(scale as u32)
            .ok_or_else(|| QueryError::failed("decimal scale overflow"))?;
        i128::from(v)
            .checked_mul(factor)
            .ok_or_else(|| QueryError::failed("decimal overflow"))?
    } else {
        let denom = 10i128
            .checked_pow((-scale) as u32)
            .ok_or_else(|| QueryError::failed("decimal scale overflow"))?;
        i128::from(v) / denom
    };
    check_decimal_bounds(scaled, precision)
}

fn f64_to_decimal(v: f64, precision: u8, scale: i8) -> Result<i128, QueryError> {
    if !v.is_finite() {
        return Err(QueryError::failed(
            "cannot cast non-finite float to decimal",
        ));
    }
    let scaled = (v * 10f64.powi(i32::from(scale))).round();
    if !scaled.is_finite() || scaled.abs() >= 10f64.powi(i32::from(precision)) {
        return Err(QueryError::failed("float out of range for decimal"));
    }
    if scaled <= (i128::MIN as f64) || scaled >= (i128::MAX as f64) {
        return Err(QueryError::failed("decimal overflow"));
    }
    check_decimal_bounds(scaled as i128, precision)
}

fn decimal_to_i64(v: i128, scale: i8) -> Option<i64> {
    let unscaled = if scale >= 0 {
        let denom = 10i128.checked_pow(scale as u32)?;
        v / denom
    } else {
        let factor = 10i128.checked_pow((-scale) as u32)?;
        v.checked_mul(factor)?
    };
    i64::try_from(unscaled).ok()
}

fn rescale_decimal(v: i128, s1: i8, p2: u8, s2: i8) -> Result<i128, QueryError> {
    let res = if s2 >= s1 {
        let diff = (s2 as i32 - s1 as i32) as u32;
        let factor = 10i128
            .checked_pow(diff)
            .ok_or_else(|| QueryError::failed("decimal scale overflow"))?;
        v.checked_mul(factor)
            .ok_or_else(|| QueryError::failed("decimal overflow"))?
    } else {
        let diff = (s1 as i32 - s2 as i32) as u32;
        let denom = 10u128
            .checked_pow(diff)
            .ok_or_else(|| QueryError::failed("decimal scale overflow"))?;
        let neg = v < 0;
        let abs = v.unsigned_abs();
        let mut q = abs / denom;
        let r = abs % denom;
        if r >= denom.div_ceil(2) {
            q = q
                .checked_add(1)
                .ok_or_else(|| QueryError::failed("decimal overflow"))?;
        }
        signed_from_u128(neg, q)?
    };
    check_decimal_bounds(res, p2)
}

fn parse_decimal_str(text: &str, precision: u8, scale: i8) -> Result<i128, QueryError> {
    let trimmed = text.trim_ascii();
    if trimmed.is_empty() {
        return Err(QueryError::failed("empty string cannot cast to decimal"));
    }
    let (neg, rest) = if let Some(r) = trimmed.strip_prefix('-') {
        (true, r)
    } else if let Some(r) = trimmed.strip_prefix('+') {
        (false, r)
    } else {
        (false, trimmed)
    };
    let (int_part, frac_part) = match rest.split_once('.') {
        Some((i, f)) => (i, f),
        None => (rest, ""),
    };
    if (int_part.is_empty() && frac_part.is_empty())
        || !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(QueryError::failed("invalid decimal string"));
    }
    let mut digits = String::with_capacity(int_part.len() + frac_part.len());
    digits.push_str(int_part);
    digits.push_str(frac_part);
    let raw_mag = digits
        .parse::<u128>()
        .map_err(|_| QueryError::failed("decimal string overflow"))?;
    let frac_len = i32::try_from(frac_part.len())
        .map_err(|_| QueryError::failed("decimal string overflow"))?;
    let diff = i32::from(scale) - frac_len;
    let mag = if diff >= 0 {
        let factor = 10u128
            .checked_pow(diff as u32)
            .ok_or_else(|| QueryError::failed("decimal overflow"))?;
        raw_mag
            .checked_mul(factor)
            .ok_or_else(|| QueryError::failed("decimal overflow"))?
    } else {
        let denom = 10u128
            .checked_pow((-diff) as u32)
            .ok_or_else(|| QueryError::failed("decimal scale overflow"))?;
        let mut q = raw_mag / denom;
        let r = raw_mag % denom;
        if r >= denom.div_ceil(2) {
            q = q
                .checked_add(1)
                .ok_or_else(|| QueryError::failed("decimal overflow"))?;
        }
        q
    };
    let signed = signed_from_u128(neg, mag)?;
    check_decimal_bounds(signed, precision)
}

fn format_decimal(value: i128, scale: i8) -> String {
    if scale == 0 {
        return value.to_string();
    }
    if scale < 0 {
        if value == 0 {
            return "0".to_owned();
        }
        let mut s = value.to_string();
        for _ in 0..(-scale) {
            s.push('0');
        }
        return s;
    }
    let neg = value < 0;
    let digits = value.unsigned_abs().to_string();
    let sc = scale as usize;
    let mut out = String::with_capacity(digits.len() + sc + 3);
    if neg {
        out.push('-');
    }
    if digits.len() <= sc {
        out.push_str("0.");
        for _ in 0..(sc - digits.len()) {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        let split = digits.len() - sc;
        out.push_str(&digits[..split]);
        out.push('.');
        out.push_str(&digits[split..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cetra_column::{CancelToken, DictColumn, Field, GlobalPool, RleColumn, Schema};

    use super::*;

    fn ctx() -> EvalContext {
        EvalContext {
            now_ns: 1_700_000_000_000_000_000,
            cancel: CancelToken::new(),
        }
    }

    fn bool_vec(col: &Column) -> Vec<Option<bool>> {
        (0..col.len())
            .map(|r| {
                if col.is_null(r) {
                    None
                } else {
                    read_bool(col, r)
                }
            })
            .collect()
    }

    #[test]
    fn three_valued_boolean_and_null_safe_distinct() {
        let schema = Schema::try_new_arc(vec![
            Field::new("a", DataType::Bool, true),
            Field::new("b", DataType::Bool, true),
            Field::new("x", DataType::I64, true),
            Field::new("y", DataType::I64, true),
        ])
        .unwrap();

        // Truth table pairs: (T,T), (T,F), (T,N), (F,T), (F,F), (F,N), (N,T), (N,F), (N,N)
        let a_vals =
            Bitmap::from_bools(&[true, true, true, false, false, false, false, false, false]);
        let a_nulls =
            Bitmap::from_bools(&[true, true, true, true, true, true, false, false, false]);
        let b_vals =
            Bitmap::from_bools(&[true, false, false, true, false, false, true, false, false]);
        let b_nulls =
            Bitmap::from_bools(&[true, true, false, true, true, false, true, true, false]);

        let col_a =
            Column::Flat(FlatColumn::try_new(FlatData::Bool(a_vals), Some(a_nulls)).unwrap());
        let col_b =
            Column::Flat(FlatColumn::try_new(FlatData::Bool(b_vals), Some(b_nulls)).unwrap());
        let col_x = Column::Flat(
            FlatColumn::try_new(
                FlatData::I64(vec![1, 1, 0, 2, 0, 5, 5, 5, 5]),
                Some(Bitmap::from_bools(&[
                    true, true, false, true, false, true, true, true, true,
                ])),
            )
            .unwrap(),
        );
        let col_y = Column::Flat(
            FlatColumn::try_new(
                FlatData::I64(vec![1, 2, 0, 0, 0, 5, 5, 5, 5]),
                Some(Bitmap::from_bools(&[
                    true, true, true, false, false, true, true, true, true,
                ])),
            )
            .unwrap(),
        );
        let batch = Batch::try_new_all(schema, vec![col_a, col_b, col_x, col_y], 9).unwrap();

        let ea = Box::new(Expr::Column {
            index: 0,
            name: "a".into(),
            data_type: DataType::Bool,
        });
        let eb = Box::new(Expr::Column {
            index: 1,
            name: "b".into(),
            data_type: DataType::Bool,
        });
        let ex = Box::new(Expr::Column {
            index: 2,
            name: "x".into(),
            data_type: DataType::I64,
        });
        let ey = Box::new(Expr::Column {
            index: 3,
            name: "y".into(),
            data_type: DataType::I64,
        });

        let and_res = eval_expr(
            &Expr::Binary {
                left: ea.clone(),
                op: BinaryOp::And,
                right: eb.clone(),
                data_type: DataType::Bool,
            },
            &batch,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(
            bool_vec(&and_res),
            vec![
                Some(true),
                Some(false),
                None,
                Some(false),
                Some(false),
                Some(false),
                None,
                Some(false),
                None,
            ]
        );

        let or_res = eval_expr(
            &Expr::Binary {
                left: ea.clone(),
                op: BinaryOp::Or,
                right: eb,
                data_type: DataType::Bool,
            },
            &batch,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(
            bool_vec(&or_res),
            vec![
                Some(true),
                Some(true),
                Some(true),
                Some(true),
                Some(false),
                None,
                Some(true),
                None,
                None,
            ]
        );

        let not_res = eval_expr(
            &Expr::Unary {
                op: UnaryOp::Not,
                expr: ea.clone(),
                data_type: DataType::Bool,
            },
            &batch,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(
            bool_vec(&not_res),
            vec![
                Some(false),
                Some(false),
                Some(false),
                Some(true),
                Some(true),
                Some(true),
                None,
                None,
                None,
            ]
        );

        // IS TRUE / IS FALSE / IS NOT TRUE / IS NOT FALSE on a (T,T,T,F,F,F,N,N,N)
        for (op, expected_t, expected_f, expected_n) in [
            (UnaryOp::IsTrue, true, false, false),
            (UnaryOp::IsFalse, false, true, false),
            (UnaryOp::IsNotTrue, false, true, true),
            (UnaryOp::IsNotFalse, true, false, true),
        ] {
            let r = eval_expr(
                &Expr::Unary {
                    op,
                    expr: ea.clone(),
                    data_type: DataType::Bool,
                },
                &batch,
                &ctx(),
                None,
            )
            .unwrap();
            assert_eq!(
                bool_vec(&r),
                vec![
                    Some(expected_t),
                    Some(expected_t),
                    Some(expected_t),
                    Some(expected_f),
                    Some(expected_f),
                    Some(expected_f),
                    Some(expected_n),
                    Some(expected_n),
                    Some(expected_n),
                ]
            );
        }

        // IS NULL / IS NOT NULL / IS DISTINCT FROM / IS NOT DISTINCT FROM on x vs y
        // Rows 0..5: (1,1), (1,2), (N,0), (2,N), (N,N)
        let is_null_x = eval_expr(
            &Expr::Unary {
                op: UnaryOp::IsNull,
                expr: ex.clone(),
                data_type: DataType::Bool,
            },
            &batch,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(
            bool_vec(&is_null_x)[..5],
            [
                Some(false),
                Some(false),
                Some(true),
                Some(false),
                Some(true)
            ]
        );

        let is_not_null_x = eval_expr(
            &Expr::Unary {
                op: UnaryOp::IsNotNull,
                expr: ex.clone(),
                data_type: DataType::Bool,
            },
            &batch,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(
            bool_vec(&is_not_null_x)[..5],
            [Some(true), Some(true), Some(false), Some(true), Some(false)]
        );

        let distinct = eval_expr(
            &Expr::Binary {
                left: ex.clone(),
                op: BinaryOp::IsDistinctFrom,
                right: ey.clone(),
                data_type: DataType::Bool,
            },
            &batch,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(
            bool_vec(&distinct)[..5],
            [Some(false), Some(true), Some(true), Some(true), Some(false)]
        );

        let not_distinct = eval_expr(
            &Expr::Binary {
                left: ex.clone(),
                op: BinaryOp::IsNotDistinctFrom,
                right: ey,
                data_type: DataType::Bool,
            },
            &batch,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(
            bool_vec(&not_distinct)[..5],
            [
                Some(true),
                Some(false),
                Some(false),
                Some(false),
                Some(true)
            ]
        );

        // IN / NOT IN with NULL and empty IN ()
        let in_with_null = eval_expr(
            &Expr::InList {
                expr: ex.clone(),
                list: vec![
                    Expr::Literal(ScalarValue::I64(1)),
                    Expr::Literal(ScalarValue::Null(DataType::I64)),
                ],
                negated: false,
                redacted: false,
            },
            &batch,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(
            bool_vec(&in_with_null)[..4],
            [Some(true), Some(true), None, None]
        );

        let not_in_with_null = eval_expr(
            &Expr::InList {
                expr: ex.clone(),
                list: vec![
                    Expr::Literal(ScalarValue::I64(1)),
                    Expr::Literal(ScalarValue::Null(DataType::I64)),
                ],
                negated: true,
                redacted: false,
            },
            &batch,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(
            bool_vec(&not_in_with_null)[..4],
            [Some(false), Some(false), None, None]
        );

        let empty_in = eval_expr(
            &Expr::InList {
                expr: ex.clone(),
                list: vec![],
                negated: false,
                redacted: false,
            },
            &batch,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(bool_vec(&empty_in)[..5], [Some(false); 5]);

        let empty_not_in = eval_expr(
            &Expr::InList {
                expr: ex,
                list: vec![],
                negated: true,
                redacted: false,
            },
            &batch,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(bool_vec(&empty_not_in)[..5], [Some(true); 5]);

        // 5 BETWEEN 10 AND NULL == false; 5 BETWEEN 1 AND NULL == NULL
        let five_between = eval_expr(
            &Expr::Between {
                expr: Box::new(Expr::Literal(ScalarValue::I64(5))),
                low: Box::new(Expr::Literal(ScalarValue::I64(10))),
                high: Box::new(Expr::Literal(ScalarValue::Null(DataType::I64))),
                negated: false,
            },
            &batch,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(bool_vec(&five_between)[0], Some(false));

        let five_between_null = eval_expr(
            &Expr::Between {
                expr: Box::new(Expr::Literal(ScalarValue::I64(5))),
                low: Box::new(Expr::Literal(ScalarValue::I64(1))),
                high: Box::new(Expr::Literal(ScalarValue::Null(DataType::I64))),
                negated: false,
            },
            &batch,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(bool_vec(&five_between_null)[0], None);
    }

    #[test]
    fn arithmetic_overflow_div_zero_f64_total_order_and_casts() {
        let schema = Schema::try_new_arc(vec![Field::new("i", DataType::I64, false)]).unwrap();
        let batch = Batch::try_new_all(
            schema,
            vec![Column::Flat(
                FlatColumn::try_new(FlatData::I64(vec![1]), None).unwrap(),
            )],
            1,
        )
        .unwrap();

        let err_cases = [
            // I64 div/mod zero
            Expr::Binary {
                left: Box::new(Expr::Literal(ScalarValue::I64(10))),
                op: BinaryOp::Div,
                right: Box::new(Expr::Literal(ScalarValue::I64(0))),
                data_type: DataType::I64,
            },
            Expr::Binary {
                left: Box::new(Expr::Literal(ScalarValue::I64(10))),
                op: BinaryOp::Mod,
                right: Box::new(Expr::Literal(ScalarValue::I64(0))),
                data_type: DataType::I64,
            },
            // F64 div/mod zero (+0.0 and -0.0)
            Expr::Binary {
                left: Box::new(Expr::Literal(ScalarValue::F64(10.0))),
                op: BinaryOp::Div,
                right: Box::new(Expr::Literal(ScalarValue::F64(-0.0))),
                data_type: DataType::F64,
            },
            Expr::Binary {
                left: Box::new(Expr::Literal(ScalarValue::F64(10.0))),
                op: BinaryOp::Mod,
                right: Box::new(Expr::Literal(ScalarValue::F64(0.0))),
                data_type: DataType::F64,
            },
            // I64 overflows
            Expr::Binary {
                left: Box::new(Expr::Literal(ScalarValue::I64(i64::MAX))),
                op: BinaryOp::Add,
                right: Box::new(Expr::Literal(ScalarValue::I64(1))),
                data_type: DataType::I64,
            },
            Expr::Binary {
                left: Box::new(Expr::Literal(ScalarValue::I64(i64::MIN))),
                op: BinaryOp::Div,
                right: Box::new(Expr::Literal(ScalarValue::I64(-1))),
                data_type: DataType::I64,
            },
            Expr::Unary {
                op: UnaryOp::Neg,
                expr: Box::new(Expr::Literal(ScalarValue::I64(i64::MIN))),
                data_type: DataType::I64,
            },
            // Strict casts that fail
            Expr::Cast {
                expr: Box::new(Expr::Literal(ScalarValue::Utf8("abc".into()))),
                target: DataType::F64,
                mode: CastMode::Strict,
            },
            Expr::Cast {
                expr: Box::new(Expr::Literal(ScalarValue::F64(f64::NAN))),
                target: DataType::I64,
                mode: CastMode::Strict,
            },
            Expr::Cast {
                expr: Box::new(Expr::Literal(ScalarValue::F64(9223372036854775808.0))),
                target: DataType::I64,
                mode: CastMode::Strict,
            },
            Expr::Cast {
                expr: Box::new(Expr::Literal(ScalarValue::Date(i32::MAX))),
                target: DataType::timestamp_utc(),
                mode: CastMode::Strict,
            },
        ];

        for expr in &err_cases {
            let err = eval_expr(expr, &batch, &ctx(), None).unwrap_err();
            assert!(
                matches!(err, QueryError::Failed(_)),
                "expected Failed, got {err:?} for {expr}"
            );
        }

        // TRY_CAST('abc' AS DOUBLE) -> NULL, TRY_CAST(NaN AS BIGINT) -> NULL
        let try_double = eval_expr(
            &Expr::Cast {
                expr: Box::new(Expr::Literal(ScalarValue::Utf8("abc".into()))),
                target: DataType::F64,
                mode: CastMode::Try,
            },
            &batch,
            &ctx(),
            None,
        )
        .unwrap();
        assert!(try_double.is_null(0));

        let try_i64 = eval_expr(
            &Expr::Cast {
                expr: Box::new(Expr::Literal(ScalarValue::F64(f64::NAN))),
                target: DataType::I64,
                mode: CastMode::Try,
            },
            &batch,
            &ctx(),
            None,
        )
        .unwrap();
        assert!(try_i64.is_null(0));

        // F64 total order: -0.0 == +0.0, NaN == NaN, NaN > +inf
        assert_eq!(f64_cmp(-0.0, 0.0), Ordering::Equal);
        assert_eq!(f64_canonical_bits(-0.0), f64_canonical_bits(0.0));
        assert_eq!(f64_cmp(f64::NAN, -f64::NAN), Ordering::Equal);
        assert_eq!(f64_canonical_bits(f64::NAN), f64_canonical_bits(-f64::NAN));
        assert_eq!(f64_cmp(f64::NAN, f64::INFINITY), Ordering::Greater);

        let eq_nan = eval_expr(
            &Expr::Binary {
                left: Box::new(Expr::Literal(ScalarValue::F64(f64::NAN))),
                op: BinaryOp::Eq,
                right: Box::new(Expr::Literal(ScalarValue::F64(f64::NAN))),
                data_type: DataType::Bool,
            },
            &batch,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(read_bool(&eq_nan, 0), Some(true));

        let eq_zero = eval_expr(
            &Expr::Binary {
                left: Box::new(Expr::Literal(ScalarValue::F64(-0.0))),
                op: BinaryOp::Eq,
                right: Box::new(Expr::Literal(ScalarValue::F64(0.0))),
                data_type: DataType::Bool,
            },
            &batch,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(read_bool(&eq_zero, 0), Some(true));

        let nan_gt_inf = eval_expr(
            &Expr::Binary {
                left: Box::new(Expr::Literal(ScalarValue::F64(f64::NAN))),
                op: BinaryOp::Gt,
                right: Box::new(Expr::Literal(ScalarValue::F64(f64::INFINITY))),
                data_type: DataType::Bool,
            },
            &batch,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(read_bool(&nan_gt_inf, 0), Some(true));

        // Decimal(38, 18) mul without spurious intermediate overflow
        let dec_dt = DataType::cost_decimal();
        let dec_mul_res = eval_expr(
            &Expr::Binary {
                left: Box::new(Expr::Literal(ScalarValue::Decimal128 {
                    value: 100_000_000_000_000_000_000,
                    precision: 38,
                    scale: 18,
                })),
                op: BinaryOp::Mul,
                right: Box::new(Expr::Literal(ScalarValue::Decimal128 {
                    value: 100_000_000_000_000_000_000,
                    precision: 38,
                    scale: 18,
                })),
                data_type: dec_dt,
            },
            &batch,
            &ctx(),
            None,
        )
        .unwrap();
        assert_eq!(
            read_decimal(&dec_mul_res, 0),
            Some(10_000_000_000_000_000_000_000)
        );
    }

    #[test]
    fn case_short_circuit_selection_masking_map_index_and_cancellation() {
        // 1. CASE WHEN x != 0 THEN 10 / x ELSE 0 END with x = 0 row
        let schema = Schema::try_new_arc(vec![Field::new("x", DataType::I64, false)]).unwrap();
        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![Column::Flat(
                FlatColumn::try_new(FlatData::I64(vec![2, 0, 5]), None).unwrap(),
            )],
            3,
        )
        .unwrap();
        let ex = Box::new(Expr::Column {
            index: 0,
            name: "x".into(),
            data_type: DataType::I64,
        });
        let case_expr = Expr::Case {
            operand: None,
            when_then: vec![(
                Expr::Binary {
                    left: ex.clone(),
                    op: BinaryOp::NotEq,
                    right: Box::new(Expr::Literal(ScalarValue::I64(0))),
                    data_type: DataType::Bool,
                },
                Expr::Binary {
                    left: Box::new(Expr::Literal(ScalarValue::I64(10))),
                    op: BinaryOp::Div,
                    right: ex.clone(),
                    data_type: DataType::I64,
                },
            )],
            else_expr: Some(Box::new(Expr::Literal(ScalarValue::I64(0)))),
            data_type: DataType::I64,
        };
        let case_res = eval_expr(&case_expr, &batch, &ctx(), None).unwrap();
        assert_eq!(
            (0..3).map(|r| read_i64(&case_res, r)).collect::<Vec<_>>(),
            vec![Some(5), Some(0), Some(2)]
        );

        // 2. Selection::Mask and Selection::Indices mask out x = 0 so 10 / x never faults
        let div_expr = Expr::Binary {
            left: Box::new(Expr::Literal(ScalarValue::I64(10))),
            op: BinaryOp::Div,
            right: ex,
            data_type: DataType::I64,
        };
        let masked = batch
            .with_selection(Selection::Mask(Bitmap::from_bools(&[true, false, true])))
            .unwrap();
        let masked_res = eval_expr(&div_expr, &masked, &ctx(), None).unwrap();
        assert_eq!(masked_res.len(), 3);
        assert_eq!(read_i64(&masked_res, 0), Some(5));
        assert!(masked_res.is_null(1));
        assert_eq!(read_i64(&masked_res, 2), Some(2));

        let indexed = batch
            .with_selection(Selection::Indices(vec![0, 2]))
            .unwrap();
        let idx_res = eval_expr(&div_expr, &indexed, &ctx(), None).unwrap();
        assert_eq!(idx_res.len(), 3);
        assert_eq!(read_i64(&idx_res, 0), Some(5));
        assert!(idx_res.is_null(1));
        assert_eq!(read_i64(&idx_res, 2), Some(2));

        // 3. MapIndex across Flat, Const, Dict, Rle (present, missing, null value, duplicate keys)
        let map_data = MapData::try_new(
            vec![0, 3, 4],
            Utf8Data::try_from_slices(["a", "a", "null_v", "b"]).unwrap(),
            Utf8Data::try_from_slices(["first", "second", "", "val_b"]).unwrap(),
            Some(Bitmap::from_bools(&[true, true, false, true])),
        )
        .unwrap();
        let flat_maps = FlatColumn::try_new(FlatData::MapUtf8(Box::new(map_data)), None).unwrap();
        let const_map = Column::try_new_const(
            ScalarValue::MapUtf8(vec![
                ("a".into(), Some("c_first".into())),
                ("a".into(), Some("c_second".into())),
                ("null_v".into(), None),
            ]),
            2,
        )
        .unwrap();
        let dict_map =
            Column::Dict(DictColumn::try_new(flat_maps.clone(), vec![1, 0], None).unwrap());
        let rle_map = Column::Rle(RleColumn::try_new(flat_maps.clone(), vec![1, 2]).unwrap());
        let keys = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(["a", "null_v"]).unwrap()),
                None,
            )
            .unwrap(),
        );
        let missing_keys = Column::try_new_const(ScalarValue::Utf8("missing".into()), 2).unwrap();

        let map_schema = Schema::try_new_arc(vec![
            Field::new("m_flat", DataType::MapUtf8, true),
            Field::new("m_const", DataType::MapUtf8, true),
            Field::new("m_dict", DataType::MapUtf8, true),
            Field::new("m_rle", DataType::MapUtf8, true),
            Field::new("k", DataType::Utf8, true),
            Field::new("k_miss", DataType::Utf8, true),
        ])
        .unwrap();
        let map_batch = Batch::try_new_all(
            map_schema,
            vec![
                Column::Flat(flat_maps),
                const_map,
                dict_map,
                rle_map,
                keys,
                missing_keys,
            ],
            2,
        )
        .unwrap();

        for (col_idx, expected_row0, expected_row1) in [
            (0usize, Some("first"), None),
            (1, Some("c_first"), None),
            (2, None, None), // dict row 0 -> map 1 ("b"), key "a" -> missing; row 1 -> map 0, key "null_v" -> null
            (3, Some("first"), None),
        ] {
            let expr = Expr::MapIndex {
                map: Box::new(Expr::Column {
                    index: col_idx,
                    name: "m".into(),
                    data_type: DataType::MapUtf8,
                }),
                key: Box::new(Expr::Column {
                    index: 4,
                    name: "k".into(),
                    data_type: DataType::Utf8,
                }),
            };
            let out = eval_expr(&expr, &map_batch, &ctx(), None).unwrap();
            assert_eq!(read_str(&out, 0), expected_row0, "col {col_idx} row 0");
            assert_eq!(read_str(&out, 1), expected_row1, "col {col_idx} row 1");
        }

        let miss_out = eval_expr(
            &Expr::MapIndex {
                map: Box::new(Expr::Column {
                    index: 0,
                    name: "m_flat".into(),
                    data_type: DataType::MapUtf8,
                }),
                key: Box::new(Expr::Column {
                    index: 5,
                    name: "k_miss".into(),
                    data_type: DataType::Utf8,
                }),
            },
            &map_batch,
            &ctx(),
            None,
        )
        .unwrap();
        assert!(miss_out.is_null(0));
        assert!(miss_out.is_null(1));

        // 4. CancelToken abort
        let cancelled_ctx = EvalContext {
            now_ns: 0,
            cancel: CancelToken::new(),
        };
        cancelled_ctx.cancel.cancel();
        let cancel_err = eval_expr(&case_expr, &batch, &cancelled_ctx, None).unwrap_err();
        assert!(matches!(cancel_err, QueryError::Cancelled));

        // 5. Reservation OOM rollback and Ok postcondition
        let scope = GlobalPool::standalone(1024);
        let mut res = scope.register("eval", false);
        res.try_grow(40).unwrap();
        let before = res.size();
        let ok_col = eval_expr(&case_expr, &batch, &ctx(), Some(&mut res)).unwrap();
        assert_eq!(res.size(), before + ok_col.memory_bytes());

        // Force OOM on a larger batch
        let big_schema = Schema::try_new_arc(vec![Field::new("x", DataType::I64, false)]).unwrap();
        let big_batch = Batch::try_new_all(
            big_schema,
            vec![Column::Flat(
                FlatColumn::try_new(FlatData::I64(vec![1; 512]), None).unwrap(),
            )],
            512,
        )
        .unwrap();
        let big_expr = Expr::Binary {
            left: Box::new(Expr::Column {
                index: 0,
                name: "x".into(),
                data_type: DataType::I64,
            }),
            op: BinaryOp::Add,
            right: Box::new(Expr::Literal(ScalarValue::I64(1))),
            data_type: DataType::I64,
        };
        let err_oom = eval_expr(&big_expr, &big_batch, &ctx(), Some(&mut res)).unwrap_err();
        assert!(matches!(err_oom, QueryError::TooLarge));
        assert_eq!(res.size(), before + ok_col.memory_bytes());

        // eval_filter rejects non-bool/non-null and checks reservation
        let non_bool = Expr::Column {
            index: 0,
            name: "x".into(),
            data_type: DataType::I64,
        };
        assert!(matches!(
            eval_filter(&non_bool, &batch, &ctx(), None).unwrap_err(),
            QueryError::Failed(_)
        ));
        let pred = Expr::Binary {
            left: Box::new(Expr::Column {
                index: 0,
                name: "x".into(),
                data_type: DataType::I64,
            }),
            op: BinaryOp::Gt,
            right: Box::new(Expr::Literal(ScalarValue::I64(0))),
            data_type: DataType::Bool,
        };
        let filter_bm = eval_filter(&pred, &batch, &ctx(), Some(&mut res)).unwrap();
        assert_eq!(filter_bm, Bitmap::from_bools(&[true, false, true]));
        assert_eq!(
            res.size(),
            before + ok_col.memory_bytes() + filter_bm.memory_bytes()
        );
    }

    #[test]
    fn in_list_redacted_display_and_debug_hide_values() {
        let expr = Expr::InList {
            expr: Box::new(Expr::Column {
                index: 0,
                name: "user_id".into(),
                data_type: DataType::Utf8,
            }),
            list: vec![
                Expr::Literal(ScalarValue::Utf8("erased_alice_secret".into())),
                Expr::Literal(ScalarValue::Utf8("erased_bob_secret".into())),
            ],
            negated: false,
            redacted: true,
        };
        let plan = expr.fmt_plan();
        let disp = format!("{expr}");
        let dbg = format!("{expr:?}");
        assert_eq!(plan, "user_id IN (<2 redacted>)");
        assert_eq!(disp, "user_id IN (<2 redacted>)");
        assert!(dbg.contains("<2 redacted>"), "debug was: {dbg}");
        for text in [&plan, &disp, &dbg] {
            assert!(!text.contains("erased_alice_secret"));
            assert!(!text.contains("erased_bob_secret"));
        }
    }

    #[test]
    fn wired_dispatch_and_reservation_postconditions() {
        use crate::bind::bind_expr;
        use crate::text::{SQL_LIMITS, TextKind, expect_end, parse_failure, parser, tokenize};
        use cetra_column::Interval;

        let schema = Schema::try_new_arc(vec![
            Field::new("s", DataType::Utf8, true),
            Field::new("ts", DataType::timestamp_utc(), true),
            Field::new("tags", DataType::List(Box::new(DataType::Utf8)), true),
        ])
        .unwrap();

        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices([
                                "2026-01-01T10:00:00Z",
                                "bad-timestamp",
                                "{\"k\": [10, \"hi\"]}",
                            ])
                            .unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: vec![1_767_261_600_000_000_000, i64::MAX, 0],
                            utc: true,
                        },
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::List {
                            item_type: DataType::Utf8,
                            offsets: vec![0, 1, 1, 2],
                            values: Box::new(Column::Flat(
                                FlatColumn::try_new(
                                    FlatData::Utf8(
                                        Utf8Data::try_from_slices(["alpha", "beta"]).unwrap(),
                                    ),
                                    None,
                                )
                                .unwrap(),
                            )),
                        },
                        None,
                    )
                    .unwrap(),
                ),
            ],
            3,
        )
        .unwrap();

        // Mask out row 1 (which has "bad-timestamp" and i64::MAX ts) to prove active-row masking
        let masked = batch
            .with_selection(Selection::Mask(Bitmap::from_bools(&[true, false, true])))
            .unwrap();

        let parse_bind = |sql: &str| {
            let toks = tokenize(sql, &SQL_LIMITS, TextKind::Sql).unwrap();
            let mut p = parser(toks);
            let ast = p
                .parse_expr()
                .map_err(|e| parse_failure(e, &p, sql))
                .unwrap();
            expect_end(&mut p, sql).unwrap();
            bind_expr(&ast, &schema).unwrap()
        };

        let scope = GlobalPool::standalone(1024 * 1024);
        let mut pool = scope.register("wired", false);
        pool.try_grow(64).unwrap();
        let before = pool.size();

        // Utf8 -> TimestampNs cast + TimestampNs -> Utf8 cast + TimestampNs + Interval
        let ts_cast = parse_bind("CAST(s AS TIMESTAMP)");
        // Masked out rows 1 and 2? Row 2 is JSON, so mask only row 0 for strict ts cast!
        let only_row0 = batch
            .with_selection(Selection::Mask(Bitmap::from_bools(&[true, false, false])))
            .unwrap();
        let ts_col = eval_expr(&ts_cast, &only_row0, &ctx(), Some(&mut pool)).unwrap();
        assert_eq!(pool.size(), before + ts_col.memory_bytes());
        assert_eq!(read_timestamp(&ts_col, 0), Some(1_767_261_600_000_000_000));
        assert!(ts_col.is_null(1));

        let ts_to_str = parse_bind("CAST(ts AS VARCHAR)");
        let str_col = eval_expr(&ts_to_str, &only_row0, &ctx(), Some(&mut pool)).unwrap();
        assert_eq!(
            pool.size(),
            before + ts_col.memory_bytes() + str_col.memory_bytes()
        );
        assert_eq!(read_str(&str_col, 0), Some("2026-01-01T10:00:00Z"));

        let date_cast = parse_bind("CAST('2026-01-02' AS DATE)");
        let date_col = eval_expr(&date_cast, &only_row0, &ctx(), None).unwrap();
        assert!(read_date(&date_col, 0).is_some());

        // Timestamp + Interval & Interval + Timestamp & Timestamp - Interval (masked out i64::MAX row 1!)
        let ts_add = parse_bind("ts + INTERVAL '1 hour'");
        let ts_add_col = eval_expr(&ts_add, &masked, &ctx(), None).unwrap();
        assert_eq!(
            read_timestamp(&ts_add_col, 0),
            Some(1_767_261_600_000_000_000 + 3_600_000_000_000)
        );
        assert!(ts_add_col.is_null(1));
        assert_eq!(read_timestamp(&ts_add_col, 2), Some(3_600_000_000_000));

        let iv_add = parse_bind("INTERVAL '2 hours' + ts");
        let iv_add_col = eval_expr(&iv_add, &masked, &ctx(), None).unwrap();
        assert_eq!(read_timestamp(&iv_add_col, 2), Some(7_200_000_000_000));

        let ts_sub = parse_bind("ts - INTERVAL '1 hour'");
        let ts_sub_col = eval_expr(&ts_sub, &masked, &ctx(), None).unwrap();
        assert_eq!(read_timestamp(&ts_sub_col, 2), Some(-3_600_000_000_000));

        // LIKE, string ||, list ||, @>, JSON -> / ->> / json_extract
        let like_ex = parse_bind("s LIKE '2026%'");
        let like_col = eval_expr(&like_ex, &masked, &ctx(), None).unwrap();
        assert_eq!(read_bool(&like_col, 0), Some(true));

        let str_cat = parse_bind("s || '_ok'");
        let str_cat_col = eval_expr(&str_cat, &only_row0, &ctx(), None).unwrap();
        assert_eq!(read_str(&str_cat_col, 0), Some("2026-01-01T10:00:00Z_ok"));

        let list_cat = parse_bind("tags || ['gamma']");
        let list_cat_col = eval_expr(&list_cat, &masked, &ctx(), None).unwrap();
        assert!(list_ref_at(&list_cat_col, 0).is_some());

        let contains = parse_bind("tags @> ['beta']");
        let contains_col = eval_expr(&contains, &masked, &ctx(), None).unwrap();
        assert_eq!(read_bool(&contains_col, 0), Some(false));
        assert_eq!(read_bool(&contains_col, 2), Some(true));

        let only_row2 = batch
            .with_selection(Selection::Mask(Bitmap::from_bools(&[false, false, true])))
            .unwrap();
        let json_ex = parse_bind("s -> 'k' ->> 1");
        let json_col = eval_expr(&json_ex, &only_row2, &ctx(), None).unwrap();
        assert_eq!(read_str(&json_col, 2), Some("hi"));
        let _ = Interval::default();
    }
}
