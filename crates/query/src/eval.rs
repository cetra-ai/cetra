//! Vectorized expression evaluation over `cetra_column::Batch`.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::Arc;

use cetra_column::{
    Batch, BinaryData, Bitmap, Column, ColumnError, DataType, EventItem, EventsData, FlatColumn,
    FlatData, Interval, LinkItem, LinksData, MAX_NESTING_DEPTH, MapData, Reservation, ScalarValue,
    Selection, Utf8Data,
};

use crate::error::QueryError;
use crate::ir::{BinaryOp, CastMode, EvalContext, Expr, ScalarFn, UnaryOp};

const CANCEL_CHECK_MASK: usize = 1023;
const NS_PER_DAY: i64 = 86_400_000_000_000;

/// Evaluates `expr` over `batch`, always returning an `Arc<Column>` of length `batch.raw_len()`.
///
/// Active rows under `batch.selection()` are exact, and inactive rows never fault. Callers
/// must respect `batch.selection()`: zero-copy fast paths (such as `Expr::Column`,
/// `Expr::Literal`, and identity `CAST`) may leave existing or constant values in
/// inactive slots rather than canonical nulls.
///
/// `Reservation` postcondition: on `Ok(col)`, if `col` is `Arc::ptr_eq` to a column in `batch`
/// (zero-copy `Expr::Column` or identity `CAST`), `pool` is restored to `before` (0 newly
/// allocated bytes); otherwise `pool` is reconciled to `before + col.memory_bytes()`. On `Err(_)`,
/// `pool` is rolled back to `before`.
pub fn eval_expr(
    expr: &Expr,
    batch: &Batch,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Arc<Column>, QueryError> {
    let before = pool_size(&pool);
    let res = eval_expr_root(expr, batch, ctx, pool.as_deref_mut());
    match res {
        Ok(col) => {
            if batch.columns().iter().any(|c| Arc::ptr_eq(c, &col)) {
                rollback_to(&mut pool, before);
            } else {
                reconcile_output(&mut pool, before, col.memory_bytes())?;
            }
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
) -> Result<Arc<Column>, QueryError> {
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

/// Evaluates `expr` over `batch` restricted to rows where `active` is `true` (or all rows if `None`).
pub(crate) fn eval_expr_masked(
    expr: &Expr,
    batch: &Batch,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Arc<Column>, QueryError> {
    let before = pool_size(&pool);
    check_cancel(ctx, &mut pool, before)?;

    let raw_len = batch.raw_len();
    if active.is_some_and(Bitmap::all_zeros) {
        let out = Arc::new(Column::Const {
            value: ScalarValue::Null(expr.data_type()),
            len: raw_len,
        });
        reconcile_output(&mut pool, before, out.memory_bytes())?;
        return Ok(out);
    }

    let res: Result<Arc<Column>, QueryError> = match expr {
        Expr::Column {
            index, data_type, ..
        } => match batch.column_arc(*index) {
            None => Err(ColumnError::InvalidBatch {
                reason: "column index out of bounds",
            }
            .into()),
            Some(col) if col.data_type() != *data_type => Err(ColumnError::InvalidBatch {
                reason: "column data type mismatch",
            }
            .into()),
            Some(col) => Ok(Arc::clone(col)),
        },
        Expr::Literal(val) => eval_literal_const(val, raw_len, pool.as_deref_mut()).map(Arc::new),
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
        )
        .map(Arc::new),
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
        )
        .map(Arc::new),
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
        )
        .map(Arc::new),
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
        )
        .map(Arc::new),
        Expr::MapIndex { map, key } => {
            eval_map_index(map, key, batch, active, ctx, pool.as_deref_mut()).map(Arc::new)
        }
        Expr::MakeArray { elems, item_type } => {
            eval_make_array(elems, item_type, batch, active, ctx, pool.as_deref_mut()).map(Arc::new)
        }
        Expr::Like {
            expr: inner,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => eval_like_expr(
            inner,
            pattern,
            *escape,
            *case_insensitive,
            *negated,
            batch,
            active,
            ctx,
            pool.as_deref_mut(),
        )
        .map(Arc::new),
        Expr::Call {
            func,
            args,
            data_type,
        } => eval_call(
            *func,
            args,
            data_type,
            batch,
            active,
            ctx,
            pool.as_deref_mut(),
        )
        .map(Arc::new),
    };

    match res {
        Ok(col) => {
            if batch.columns().iter().any(|c| Arc::ptr_eq(c, &col)) {
                rollback_to(&mut pool, before);
            } else {
                reconcile_output(&mut pool, before, col.memory_bytes())?;
            }
            Ok(col)
        }
        Err(err) => {
            rollback_to(&mut pool, before);
            Err(err)
        }
    }
}

fn eval_literal_const(
    val: &ScalarValue,
    raw_len: usize,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    try_reserve(&mut pool, val.heap_bytes())?;
    Ok(Column::try_new_const(val.clone(), raw_len)?)
}

#[expect(
    clippy::too_many_arguments,
    reason = "LIKE expression helper forwards AST fields plus batch/mask/ctx/pool"
)]
fn eval_like_expr(
    inner: &Expr,
    pattern: &Expr,
    escape: Option<char>,
    case_insensitive: bool,
    negated: bool,
    batch: &Batch,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let val_col = eval_expr_masked(inner, batch, active, ctx, pool.as_deref_mut())?;
    let pat_col = eval_expr_masked(pattern, batch, active, ctx, pool.as_deref_mut())?;
    crate::fn_string::eval_like(
        &val_col,
        &pat_col,
        escape,
        case_insensitive,
        negated,
        active,
        ctx,
        pool,
    )
}

// ---------------------------------------------------------------------------
// Shared pub(crate) reservation, masking, cancel, float, and reader helpers
// ---------------------------------------------------------------------------

pub(crate) fn pool_size(pool: &Option<&mut Reservation>) -> usize {
    pool.as_deref().map_or(0, Reservation::size)
}

pub(crate) fn checked_fixed_bytes(
    len: usize,
    elem_size: usize,
    with_nulls: bool,
) -> Result<usize, QueryError> {
    let data = len.checked_mul(elem_size).ok_or(QueryError::TooLarge)?;
    if with_nulls {
        data.checked_add(bitmap_mem_bytes(len))
            .ok_or(QueryError::TooLarge)
    } else {
        Ok(data)
    }
}

pub(crate) fn checked_varlen_bytes(
    len: usize,
    total_bytes: usize,
    with_nulls: bool,
) -> Result<usize, QueryError> {
    let offsets = len
        .checked_add(1)
        .and_then(|n| n.checked_mul(4))
        .ok_or(QueryError::TooLarge)?;
    let total = offsets
        .checked_add(total_bytes)
        .ok_or(QueryError::TooLarge)?;
    if with_nulls {
        total
            .checked_add(bitmap_mem_bytes(len))
            .ok_or(QueryError::TooLarge)
    } else {
        Ok(total)
    }
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

/// Zero-copy borrowed view of an events list at one row.
#[derive(Debug, Clone, Copy)]
pub(crate) enum EventsRef<'a> {
    Flat {
        data: &'a EventsData,
        start: usize,
        end: usize,
    },
    Const(&'a [EventItem]),
}

/// Zero-copy borrowed view of a links list at one row.
#[derive(Debug, Clone, Copy)]
pub(crate) enum LinksRef<'a> {
    Flat {
        data: &'a LinksData,
        start: usize,
        end: usize,
    },
    Const(&'a [LinkItem]),
}

pub(crate) fn events_ref_at(col: &Column, row: usize) -> Option<EventsRef<'_>> {
    if let Column::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|v| match v {
            ScalarValue::Events(evs) => Some(EventsRef::Const(evs.as_slice())),
            _ => None,
        });
    }
    let (fc, flat_row) = resolve_flat(col, row)?;
    match fc.data() {
        FlatData::Events(data) => {
            let start = data.offsets()[flat_row] as usize;
            let end = data.offsets()[flat_row + 1] as usize;
            Some(EventsRef::Flat { data, start, end })
        }
        _ => None,
    }
}

pub(crate) fn links_ref_at(col: &Column, row: usize) -> Option<LinksRef<'_>> {
    if let Column::Const { value, len } = col {
        return (row < *len).then_some(value).and_then(|v| match v {
            ScalarValue::Links(lks) => Some(LinksRef::Const(lks.as_slice())),
            _ => None,
        });
    }
    let (fc, flat_row) = resolve_flat(col, row)?;
    match fc.data() {
        FlatData::Links(data) => {
            let start = data.offsets()[flat_row] as usize;
            let end = data.offsets()[flat_row + 1] as usize;
            Some(LinksRef::Flat { data, start, end })
        }
        _ => None,
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ElemRef<'a> {
    Null,
    Col(&'a Column, usize),
    Scalar(&'a ScalarValue),
}

impl<'a> ElemRef<'a> {
    pub(crate) fn is_null(self) -> bool {
        match self {
            Self::Null => true,
            Self::Col(c, r) => c.is_null(r),
            Self::Scalar(sv) => sv.is_null(),
        }
    }

    pub(crate) fn read_bool(self) -> Option<bool> {
        match self {
            Self::Null => None,
            Self::Col(c, r) => read_bool(c, r),
            Self::Scalar(ScalarValue::Bool(b)) => Some(*b),
            Self::Scalar(_) => None,
        }
    }

    pub(crate) fn read_i64(self) -> Option<i64> {
        match self {
            Self::Null => None,
            Self::Col(c, r) => read_i64(c, r),
            Self::Scalar(ScalarValue::I64(v)) => Some(*v),
            Self::Scalar(_) => None,
        }
    }

    pub(crate) fn read_f64(self) -> Option<f64> {
        match self {
            Self::Null => None,
            Self::Col(c, r) => read_f64(c, r),
            Self::Scalar(ScalarValue::F64(v)) => Some(*v),
            Self::Scalar(_) => None,
        }
    }

    pub(crate) fn read_decimal(self) -> Option<(i128, i8)> {
        match self {
            Self::Null => None,
            Self::Col(c, r) => {
                let v = read_decimal(c, r)?;
                let DataType::Decimal128 { scale, .. } = c.data_type() else {
                    return None;
                };
                Some((v, scale))
            }
            Self::Scalar(ScalarValue::Decimal128 { value, scale, .. }) => Some((*value, *scale)),
            Self::Scalar(_) => None,
        }
    }

    pub(crate) fn read_timestamp(self) -> Option<i64> {
        match self {
            Self::Null => None,
            Self::Col(c, r) => read_timestamp(c, r),
            Self::Scalar(ScalarValue::TimestampNs { value, .. }) => Some(*value),
            Self::Scalar(_) => None,
        }
    }

    pub(crate) fn read_date(self) -> Option<i32> {
        match self {
            Self::Null => None,
            Self::Col(c, r) => read_date(c, r),
            Self::Scalar(ScalarValue::Date(d)) => Some(*d),
            Self::Scalar(_) => None,
        }
    }

    pub(crate) fn read_interval(self) -> Option<Interval> {
        match self {
            Self::Null => None,
            Self::Col(c, r) => read_interval(c, r),
            Self::Scalar(ScalarValue::Interval(iv)) => Some(*iv),
            Self::Scalar(_) => None,
        }
    }

    pub(crate) fn read_str(self) -> Option<&'a str> {
        match self {
            Self::Null => None,
            Self::Col(c, r) => read_str(c, r),
            Self::Scalar(ScalarValue::Utf8(s)) => Some(s.as_str()),
            Self::Scalar(_) => None,
        }
    }

    pub(crate) fn read_bytes(self) -> Option<&'a [u8]> {
        match self {
            Self::Null => None,
            Self::Col(c, r) => read_bytes(c, r),
            Self::Scalar(ScalarValue::Binary(b)) => Some(b.as_slice()),
            Self::Scalar(ScalarValue::FixedBytes8(b)) => Some(b.as_slice()),
            Self::Scalar(ScalarValue::FixedBytes16(b)) => Some(b.as_slice()),
            Self::Scalar(_) => None,
        }
    }

    pub(crate) fn as_list_ref(self) -> Option<ListRef<'a>> {
        match self {
            Self::Null => None,
            Self::Col(c, r) => list_ref_at(c, r),
            Self::Scalar(ScalarValue::List { values, .. }) => {
                Some(ListRef::Const(values.as_slice()))
            }
            Self::Scalar(_) => None,
        }
    }

    pub(crate) fn as_map_ref(self) -> Option<MapRef<'a>> {
        match self {
            Self::Null => None,
            Self::Col(c, r) => map_ref_at(c, r),
            Self::Scalar(ScalarValue::MapUtf8(entries)) => Some(MapRef::Const(entries.as_slice())),
            Self::Scalar(_) => None,
        }
    }

    pub(crate) fn as_events_ref(self) -> Option<EventsRef<'a>> {
        match self {
            Self::Null => None,
            Self::Col(c, r) => events_ref_at(c, r),
            Self::Scalar(ScalarValue::Events(evs)) => Some(EventsRef::Const(evs.as_slice())),
            Self::Scalar(_) => None,
        }
    }

    pub(crate) fn as_links_ref(self) -> Option<LinksRef<'a>> {
        match self {
            Self::Null => None,
            Self::Col(c, r) => links_ref_at(c, r),
            Self::Scalar(ScalarValue::Links(lks)) => Some(LinksRef::Const(lks.as_slice())),
            Self::Scalar(_) => None,
        }
    }

    pub(crate) fn to_scalar(self) -> ScalarValue {
        match self {
            Self::Null => ScalarValue::Null(DataType::Null),
            Self::Col(c, r) => read_scalar(c, r),
            Self::Scalar(sv) => sv.clone(),
        }
    }

    pub(crate) fn data_type(self) -> DataType {
        match self {
            Self::Null => DataType::Null,
            Self::Col(c, _) => c.data_type(),
            Self::Scalar(sv) => sv.data_type(),
        }
    }
}

pub(crate) fn list_ref_len(lref: ListRef<'_>) -> usize {
    match lref {
        ListRef::Flat { start, end, .. } => end - start,
        ListRef::Const(s) => s.len(),
    }
}

pub(crate) fn list_ref_elem(lref: ListRef<'_>, idx: usize) -> ElemRef<'_> {
    match lref {
        ListRef::Flat { values, start, .. } => {
            let pos = start + idx;
            if values.is_null(pos) {
                ElemRef::Null
            } else {
                ElemRef::Col(values, pos)
            }
        }
        ListRef::Const(slice) => {
            let sv = &slice[idx];
            if sv.is_null() {
                ElemRef::Null
            } else {
                ElemRef::Scalar(sv)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Core expression implementations
// ---------------------------------------------------------------------------

fn coalesce_types_compatible(expected: &DataType, actual: &DataType) -> bool {
    if expected == actual || matches!(actual, DataType::Null) {
        return true;
    }
    match (expected, actual) {
        (DataType::TimestampNs { .. }, DataType::TimestampNs { .. }) => true,
        (DataType::List(a), DataType::List(b)) => coalesce_types_compatible(a, b),
        _ => false,
    }
}

fn eval_coalesce_lazy(
    args: &[Expr],
    data_type: &DataType,
    batch: &Batch,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    if args.is_empty() {
        return Err(QueryError::failed("coalesce expects at least 1 argument"));
    }
    data_type.validate(MAX_NESTING_DEPTH)?;
    let before = pool_size(&pool);
    let raw_len = batch.raw_len();

    if matches!(data_type, DataType::Null) || active.is_some_and(Bitmap::all_zeros) {
        return Ok(Column::Const {
            value: ScalarValue::Null(data_type.clone()),
            len: raw_len,
        });
    }

    let bm_bytes = bitmap_mem_bytes(raw_len);
    let row_source_bytes = checked_fixed_bytes(raw_len, size_of::<Option<usize>>(), false)?;
    try_reserve(
        &mut pool,
        bm_bytes
            .checked_add(row_source_bytes)
            .ok_or(QueryError::TooLarge)?,
    )?;

    let mut remaining = active.cloned().unwrap_or_else(|| Bitmap::new_ones(raw_len));
    let mut arg_cols: Vec<Arc<Column>> = Vec::with_capacity(args.len());
    let mut row_source: Vec<Option<usize>> = vec![None; raw_len];

    for arg in args {
        if remaining.all_zeros() {
            break;
        }
        check_cancel(ctx, &mut pool, before)?;
        let before_arg = pool_size(&pool);
        let col = eval_expr_masked(arg, batch, Some(&remaining), ctx, pool.as_deref_mut())?;
        if !coalesce_types_compatible(data_type, &col.data_type()) {
            drop(col);
            rollback_to(&mut pool, before_arg);
            return Err(QueryError::failed(
                "coalesce arguments must share a common type",
            ));
        }

        let col_idx = arg_cols.len();
        let mut any_won = false;
        for (row, slot) in row_source.iter_mut().enumerate() {
            if (row & CANCEL_CHECK_MASK) == 0 {
                check_cancel(ctx, &mut pool, before)?;
            }
            if remaining.get(row) && !col.is_null(row) {
                *slot = Some(col_idx);
                let _ = remaining.set(row, false);
                any_won = true;
            }
        }
        if any_won {
            arg_cols.push(col);
        } else {
            drop(col);
            rollback_to(&mut pool, before_arg);
        }
    }

    drop(remaining);
    if let Some(res) = pool.as_deref_mut() {
        res.shrink(bm_bytes.min(res.size().saturating_sub(before)));
    }

    build_flat_from_fn(
        data_type,
        raw_len,
        |row| match row_source[row] {
            Some(ci) if !arg_cols[ci].is_null(row) => ElemRef::Col(&arg_cols[ci], row),
            _ => ElemRef::Null,
        },
        &mut pool,
        ctx,
        before,
    )
}

fn eval_call(
    func: ScalarFn,
    args: &[Expr],
    data_type: &DataType,
    batch: &Batch,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    if matches!(func, ScalarFn::Coalesce) {
        return eval_coalesce_lazy(args, data_type, batch, active, ctx, pool);
    }

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
    let refs: Vec<&Column> = arg_cols.iter().map(Arc::as_ref).collect();
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
            crate::fn_string::eval_string_fn_refs(func, &refs, len, active, ctx, pool)
        }
        ScalarFn::Now
        | ScalarFn::DateTrunc
        | ScalarFn::DateBin
        | ScalarFn::Extract
        | ScalarFn::ToTimestamp => {
            crate::fn_time::eval_time_fn_refs(func, &refs, len, active, ctx, pool)
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
            crate::fn_misc::eval_misc_fn_refs(func, &refs, len, active, ctx, pool)
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
            _ => false,
        },
    };
    if matches!(lcol.data_type(), DataType::Null) || matches!(rcol.data_type(), DataType::Null) {
        return Ok(Column::Const {
            value: ScalarValue::Null(DataType::TimestampNs { utc }),
            len: raw_len,
        });
    }
    enum BaseKind {
        TsLeft,
        TsRight,
        DateLeft,
        DateRight,
    }
    let (base_kind, iv_col, subtract) = match (lcol.data_type(), rcol.data_type(), op) {
        (DataType::TimestampNs { .. }, DataType::Interval, BinaryOp::Add) => {
            (BaseKind::TsLeft, rcol, false)
        }
        (DataType::TimestampNs { .. }, DataType::Interval, BinaryOp::Sub) => {
            (BaseKind::TsLeft, rcol, true)
        }
        (DataType::Interval, DataType::TimestampNs { .. }, BinaryOp::Add) => {
            (BaseKind::TsRight, lcol, false)
        }
        (DataType::Date, DataType::Interval, BinaryOp::Add) => (BaseKind::DateLeft, rcol, false),
        (DataType::Date, DataType::Interval, BinaryOp::Sub) => (BaseKind::DateLeft, rcol, true),
        (DataType::Interval, DataType::Date, BinaryOp::Add) => (BaseKind::DateRight, lcol, false),
        _ => {
            return Err(QueryError::failed(
                "unsupported timestamp/date and interval arithmetic",
            ));
        }
    };

    try_reserve(pool, checked_fixed_bytes(raw_len, 8, true)?)?;
    let mut vals = vec![0i64; raw_len];
    let mut nulls = Bitmap::new_zeroed(raw_len);
    for (row, slot) in vals.iter_mut().enumerate() {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if !is_active(active, row) {
            continue;
        }
        let base_ns = match base_kind {
            BaseKind::TsLeft => read_timestamp(lcol, row),
            BaseKind::TsRight => read_timestamp(rcol, row),
            BaseKind::DateLeft => match read_date(lcol, row) {
                Some(d) => Some(
                    i64::from(d)
                        .checked_mul(NS_PER_DAY)
                        .ok_or_else(|| QueryError::failed("date to timestamp overflow"))?,
                ),
                None => None,
            },
            BaseKind::DateRight => match read_date(rcol, row) {
                Some(d) => Some(
                    i64::from(d)
                        .checked_mul(NS_PER_DAY)
                        .ok_or_else(|| QueryError::failed("date to timestamp overflow"))?,
                ),
                None => None,
            },
        };
        if let (Some(ts), Some(iv)) = (base_ns, read_interval(iv_col, row)) {
            *slot = crate::fn_time::add_interval(ts, iv, subtract)?;
            let _ = nulls.set(row, true);
        }
    }
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::TimestampNs { values: vals, utc },
        Some(nulls),
    )?))
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal temporal arithmetic helper preserves operands, target type, mask, ctx, pool"
)]
fn eval_temporal_arith(
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
    match (lcol.data_type(), rcol.data_type(), op) {
        (DataType::TimestampNs { .. }, DataType::TimestampNs { .. }, BinaryOp::Sub) => {
            try_reserve(
                pool,
                checked_fixed_bytes(raw_len, size_of::<Interval>(), true)?,
            )?;
            let mut vals = vec![Interval::default(); raw_len];
            let mut nulls = Bitmap::new_zeroed(raw_len);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if !is_active(active, row) {
                    continue;
                }
                if let (Some(a), Some(b)) = (read_timestamp(lcol, row), read_timestamp(rcol, row)) {
                    let nanos = a
                        .checked_sub(b)
                        .ok_or_else(|| QueryError::failed("timestamp subtraction overflow"))?;
                    *slot = Interval {
                        months: 0,
                        days: 0,
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
        (DataType::Date, DataType::Date, BinaryOp::Sub) => {
            try_reserve(pool, checked_fixed_bytes(raw_len, 8, true)?)?;
            let mut vals = vec![0i64; raw_len];
            let mut nulls = Bitmap::new_zeroed(raw_len);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if !is_active(active, row) {
                    continue;
                }
                if let (Some(a), Some(b)) = (read_date(lcol, row), read_date(rcol, row)) {
                    *slot = i64::from(a) - i64::from(b);
                    let _ = nulls.set(row, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::I64(vals),
                Some(nulls),
            )?))
        }
        (DataType::Interval, DataType::Interval, BinaryOp::Add | BinaryOp::Sub) => {
            try_reserve(
                pool,
                checked_fixed_bytes(raw_len, size_of::<Interval>(), true)?,
            )?;
            let mut vals = vec![Interval::default(); raw_len];
            let mut nulls = Bitmap::new_zeroed(raw_len);
            let is_add = matches!(op, BinaryOp::Add);
            for (row, slot) in vals.iter_mut().enumerate() {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if !is_active(active, row) {
                    continue;
                }
                if let (Some(a), Some(b)) = (read_interval(lcol, row), read_interval(rcol, row)) {
                    let (months, days, nanos) = if is_add {
                        (
                            a.months.checked_add(b.months),
                            a.days.checked_add(b.days),
                            a.nanos.checked_add(b.nanos),
                        )
                    } else {
                        (
                            a.months.checked_sub(b.months),
                            a.days.checked_sub(b.days),
                            a.nanos.checked_sub(b.nanos),
                        )
                    };
                    *slot = Interval {
                        months: months
                            .ok_or_else(|| QueryError::failed("interval arithmetic overflow"))?,
                        days: days
                            .ok_or_else(|| QueryError::failed("interval arithmetic overflow"))?,
                        nanos: nanos
                            .ok_or_else(|| QueryError::failed("interval arithmetic overflow"))?,
                    };
                    let _ = nulls.set(row, true);
                }
            }
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::Interval(vals),
                Some(nulls),
            )?))
        }
        _ => Err(QueryError::failed("unsupported temporal arithmetic")),
    }
}

pub(crate) fn bitmap_mem_bytes(len: usize) -> usize {
    len.div_ceil(64) * 8
}

fn checked_bool_bytes(len: usize, with_nulls: bool) -> Result<usize, QueryError> {
    let bm = bitmap_mem_bytes(len);
    if with_nulls {
        bm.checked_mul(2).ok_or(QueryError::TooLarge)
    } else {
        Ok(bm)
    }
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
            if !matches!(child.data_type(), DataType::Bool | DataType::Null) {
                return Err(QueryError::failed("NOT requires boolean operand"));
            }
            if matches!(child.data_type(), DataType::Null) {
                return Ok(Column::Const {
                    value: ScalarValue::Null(data_type.clone()),
                    len: raw_len,
                });
            }
            if active.is_none()
                && let Column::Const { value, len } = &*child
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
            try_reserve(&mut pool, checked_bool_bytes(raw_len, true)?)?;
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
            if matches!(
                op,
                UnaryOp::IsTrue | UnaryOp::IsFalse | UnaryOp::IsNotTrue | UnaryOp::IsNotFalse
            ) && !matches!(child.data_type(), DataType::Bool | DataType::Null)
            {
                return Err(QueryError::failed(
                    "boolean truthiness test requires boolean operand",
                ));
            }
            let need_nulls = active.is_some();
            try_reserve(&mut pool, checked_bool_bytes(raw_len, need_nulls)?)?;
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
    data_type.validate(MAX_NESTING_DEPTH)?;
    match child.data_type() {
        DataType::Null => Ok(Column::Const {
            value: ScalarValue::Null(data_type.clone()),
            len: raw_len,
        }),
        DataType::I64 => {
            try_reserve(pool, checked_fixed_bytes(raw_len, 8, true)?)?;
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
            try_reserve(pool, checked_fixed_bytes(raw_len, 8, true)?)?;
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
            try_reserve(pool, checked_fixed_bytes(raw_len, 16, true)?)?;
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
                checked_fixed_bytes(raw_len, size_of::<Interval>(), true)?,
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

fn flatten_bool_chain<'a>(left: &'a Expr, right: &'a Expr, target_op: BinaryOp) -> Vec<&'a Expr> {
    let mut terms = Vec::new();
    let mut stack = vec![right, left];
    while let Some(node) = stack.pop() {
        if let Expr::Binary {
            left: l,
            op,
            right: r,
            ..
        } = node
            && *op == target_op
        {
            stack.push(r);
            stack.push(l);
        } else {
            terms.push(node);
        }
    }
    terms
}

fn eval_bool_chain(
    op: BinaryOp,
    left: &Expr,
    right: &Expr,
    batch: &Batch,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    mut pool: Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    let is_and = matches!(op, BinaryOp::And);
    let short_val = !is_and;
    let err_msg = if is_and {
        "AND requires boolean operands"
    } else {
        "OR requires boolean operands"
    };

    let before = pool_size(&pool);
    let raw_len = batch.raw_len();
    let terms = flatten_bool_chain(left, right, op);

    let bm_bytes = bitmap_mem_bytes(raw_len);
    let fixed_bytes = bm_bytes.checked_mul(3).ok_or(QueryError::TooLarge)?;
    try_reserve(&mut pool, fixed_bytes)?;
    let step_before = pool_size(&pool);

    let mut vals = Bitmap::new_zeroed(raw_len);
    let mut nulls = Bitmap::new_zeroed(raw_len);
    let mut remaining = Bitmap::new_zeroed(raw_len);

    for (idx, term) in terms.iter().enumerate() {
        check_cancel(ctx, &mut pool, before)?;
        let step_active = if idx == 0 { active } else { Some(&remaining) };
        let col = eval_expr_masked(term, batch, step_active, ctx, pool.as_deref_mut())?;
        if !matches!(col.data_type(), DataType::Bool | DataType::Null) {
            return Err(QueryError::failed(err_msg));
        }

        if idx == 0 {
            for row in 0..raw_len {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, &mut pool, before)?;
                }
                if !is_active(active, row) {
                    continue;
                }
                match read_bool(&col, row) {
                    Some(v) if v == short_val => {
                        let _ = nulls.set(row, true);
                        let _ = vals.set(row, short_val);
                    }
                    Some(v) => {
                        let _ = nulls.set(row, true);
                        let _ = vals.set(row, v);
                        let _ = remaining.set(row, true);
                    }
                    None => {
                        let _ = remaining.set(row, true);
                    }
                }
            }
        } else {
            for row in 0..raw_len {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, &mut pool, before)?;
                }
                if !remaining.get(row) {
                    continue;
                }
                match read_bool(&col, row) {
                    Some(v) if v == short_val => {
                        let _ = nulls.set(row, true);
                        let _ = vals.set(row, short_val);
                        let _ = remaining.set(row, false);
                    }
                    Some(_) => {}
                    None => {
                        let _ = nulls.set(row, false);
                        let _ = vals.set(row, false);
                    }
                }
            }
        }

        drop(col);
        rollback_to(&mut pool, step_before);

        if remaining.all_zeros() {
            break;
        }
    }

    drop(remaining);
    build_bool_column(raw_len, vals, Some(nulls))
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal dispatch preserves explicit batch, mask, context, and reservation"
)]
fn eval_binary_step(
    op: BinaryOp,
    lcol: &Column,
    rcol: &Column,
    data_type: &DataType,
    raw_len: usize,
    active: Option<&Bitmap>,
    ctx: &EvalContext,
    pool: &mut Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    match op {
        BinaryOp::Concat => {
            if matches!(data_type, DataType::List(_))
                || matches!(lcol.data_type(), DataType::List(_))
                || matches!(rcol.data_type(), DataType::List(_))
            {
                crate::fn_misc::eval_list_concat(lcol, rcol, active, ctx, pool.as_deref_mut())
            } else {
                crate::fn_string::eval_string_concat(lcol, rcol, active, ctx, pool.as_deref_mut())
            }
        }
        BinaryOp::ArrayContains => crate::fn_misc::eval_misc_fn_refs(
            ScalarFn::ArrayHasAll,
            &[lcol, rcol],
            raw_len,
            active,
            ctx,
            pool.as_deref_mut(),
        ),
        BinaryOp::Eq
        | BinaryOp::NotEq
        | BinaryOp::Lt
        | BinaryOp::LtEq
        | BinaryOp::Gt
        | BinaryOp::GtEq => eval_cmp(op, lcol, rcol, raw_len, active, ctx, pool),
        BinaryOp::IsDistinctFrom | BinaryOp::IsNotDistinctFrom => {
            eval_distinct(op, lcol, rcol, raw_len, active, ctx, pool)
        }
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
            let ldt = lcol.data_type();
            let rdt = rcol.data_type();
            if matches!(
                (&ldt, &rdt),
                (
                    DataType::TimestampNs { .. } | DataType::Date,
                    DataType::Interval
                ) | (
                    DataType::Interval,
                    DataType::TimestampNs { .. } | DataType::Date
                )
            ) {
                return eval_timestamp_interval(
                    op, lcol, rcol, data_type, raw_len, active, ctx, pool,
                );
            }
            if matches!(
                (&ldt, &rdt, op),
                (
                    DataType::TimestampNs { .. },
                    DataType::TimestampNs { .. },
                    BinaryOp::Sub
                ) | (DataType::Date, DataType::Date, BinaryOp::Sub)
                    | (
                        DataType::Interval,
                        DataType::Interval,
                        BinaryOp::Add | BinaryOp::Sub
                    )
            ) {
                return eval_temporal_arith(op, lcol, rcol, data_type, raw_len, active, ctx, pool);
            }
            eval_arith(op, lcol, rcol, data_type, raw_len, active, ctx, pool)
        }
        BinaryOp::And | BinaryOp::Or => Err(QueryError::internal(
            "boolean chain should be dispatched to eval_bool_chain",
        )),
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
) -> Result<Arc<Column>, QueryError> {
    if matches!(op, BinaryOp::And | BinaryOp::Or) {
        return eval_bool_chain(op, left, right, batch, active, ctx, pool).map(Arc::new);
    }

    let mut steps: Vec<(BinaryOp, &Expr, &DataType)> = vec![(op, right, data_type)];
    let mut base = left;
    while let Expr::Binary {
        left: l,
        op: inner_op,
        right: r,
        data_type: inner_dt,
    } = base
    {
        if matches!(inner_op, BinaryOp::And | BinaryOp::Or) {
            break;
        }
        steps.push((*inner_op, r, inner_dt));
        base = l;
    }

    let before = pool_size(&pool);
    let raw_len = batch.raw_len();
    let mut acc = eval_expr_masked(base, batch, active, ctx, pool.as_deref_mut())?;
    for (step_op, step_right, step_dt) in steps.into_iter().rev() {
        check_cancel(ctx, &mut pool, before)?;
        let rcol = eval_expr_masked(step_right, batch, active, ctx, pool.as_deref_mut())?;
        let next = eval_binary_step(
            step_op, &acc, &rcol, step_dt, raw_len, active, ctx, &mut pool,
        )?;
        let next_bytes = next.memory_bytes();
        drop(acc);
        drop(rcol);
        reconcile_output(&mut pool, before, next_bytes)?;
        acc = Arc::new(next);
    }
    Ok(acc)
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
    try_reserve(pool, checked_bool_bytes(raw_len, true)?)?;
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
    let need_nulls = active.is_some();
    try_reserve(pool, checked_bool_bytes(raw_len, need_nulls)?)?;
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
        (DataType::Decimal128 { scale, .. }, DataType::I64) => {
            match (read_decimal(lcol, row), read_i64(rcol, row)) {
                (Some(a), Some(b)) => compare_decimals(a, scale, i128::from(b), 0),
                _ => Err(QueryError::failed("invalid decimal comparison")),
            }
        }
        (DataType::I64, DataType::Decimal128 { scale, .. }) => {
            match (read_i64(lcol, row), read_decimal(rcol, row)) {
                (Some(a), Some(b)) => compare_decimals(i128::from(a), 0, b, scale),
                _ => Err(QueryError::failed("invalid decimal comparison")),
            }
        }
        (DataType::TimestampNs { .. }, DataType::TimestampNs { .. }) => {
            Ok(read_timestamp(lcol, row).cmp(&read_timestamp(rcol, row)))
        }
        (DataType::Date, DataType::Date) => Ok(read_date(lcol, row).cmp(&read_date(rcol, row))),
        (DataType::Interval, DataType::Interval) => {
            match (read_interval(lcol, row), read_interval(rcol, row)) {
                (Some(a), Some(b)) => Ok(a.canonical_key().cmp(&b.canonical_key())),
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

fn compare_scaled_decimal(a: i128, b: i128, diff: u32) -> Ordering {
    if diff > 38 {
        return if a == 0 {
            0i128.cmp(&b)
        } else if a < 0 {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    if let Some(factor) = 10i128.checked_pow(diff)
        && let Some(sa) = a.checked_mul(factor)
    {
        return sa.cmp(&b);
    }
    let (hi_a, lo_a) = u256_mul(a.unsigned_abs(), 10u128.pow(diff));
    cmp_signed_u256_i128(a < 0, hi_a, lo_a, b)
}

pub(crate) fn compare_decimals(a: i128, s1: i8, b: i128, s2: i8) -> Result<Ordering, QueryError> {
    if s1 == s2 {
        return Ok(a.cmp(&b));
    }
    if s1 < s2 {
        let diff = (i32::from(s2) - i32::from(s1)) as u32;
        Ok(compare_scaled_decimal(a, b, diff))
    } else {
        let diff = (i32::from(s1) - i32::from(s2)) as u32;
        Ok(compare_scaled_decimal(b, a, diff).reverse())
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
        (
            ScalarValue::Decimal128 {
                value: x,
                scale: sx,
                ..
            },
            ScalarValue::I64(y),
        ) => compare_decimals(*x, *sx, i128::from(*y), 0),
        (
            ScalarValue::I64(x),
            ScalarValue::Decimal128 {
                value: y,
                scale: sy,
                ..
            },
        ) => compare_decimals(i128::from(*x), 0, *y, *sy),
        (ScalarValue::TimestampNs { value: x, .. }, ScalarValue::TimestampNs { value: y, .. }) => {
            Ok(x.cmp(y))
        }
        (ScalarValue::Date(x), ScalarValue::Date(y)) => Ok(x.cmp(y)),
        (ScalarValue::Interval(x), ScalarValue::Interval(y)) => {
            Ok(x.canonical_key().cmp(&y.canonical_key()))
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
    data_type.validate(MAX_NESTING_DEPTH)?;
    if matches!(lcol.data_type(), DataType::Null) || matches!(rcol.data_type(), DataType::Null) {
        return Ok(Column::Const {
            value: ScalarValue::Null(data_type.clone()),
            len: raw_len,
        });
    }

    match data_type {
        DataType::I64 => {
            try_reserve(pool, checked_fixed_bytes(raw_len, 8, true)?)?;
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
            try_reserve(pool, checked_fixed_bytes(raw_len, 8, true)?)?;
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
            let ldt = lcol.data_type();
            let rdt = rcol.data_type();
            let (DataType::Decimal128 { scale: ls, .. }, DataType::Decimal128 { scale: rs, .. }) =
                (&ldt, &rdt)
            else {
                return Err(QueryError::failed(
                    "decimal arithmetic requires decimal operands",
                ));
            };
            ldt.validate(MAX_NESTING_DEPTH)?;
            rdt.validate(MAX_NESTING_DEPTH)?;
            if matches!(op, BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mod)
                && (*ls != *scale || *rs != *scale)
            {
                return Err(QueryError::failed("decimal operand scale mismatch"));
            }
            try_reserve(pool, checked_fixed_bytes(raw_len, 16, true)?)?;
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
                        BinaryOp::Mul => decimal_mul_scaled(a, b, *ls, *rs, *scale)?,
                        BinaryOp::Div => decimal_div_scaled(a, b, *ls, *rs, *scale)?,
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

fn decimal_mul_scaled(a: i128, b: i128, s1: i8, s2: i8, out_scale: i8) -> Result<i128, QueryError> {
    if s1.unsigned_abs() > 38 || s2.unsigned_abs() > 38 || out_scale.unsigned_abs() > 38 {
        return Err(QueryError::failed("decimal scale out of range"));
    }
    if a == 0 || b == 0 {
        return Ok(0);
    }
    let neg = (a < 0) ^ (b < 0);
    let (hi, lo) = u256_mul(a.unsigned_abs(), b.unsigned_abs());
    let shift = i32::from(out_scale) - i32::from(s1) - i32::from(s2);
    let mag = if shift == 0 {
        if hi != 0 {
            return Err(QueryError::failed("decimal overflow"));
        }
        lo
    } else if shift > 0 {
        let up = shift as u32;
        if hi != 0 || up > 38 {
            return Err(QueryError::failed("decimal overflow"));
        }
        let factor = 10u128.pow(up);
        lo.checked_mul(factor)
            .ok_or_else(|| QueryError::failed("decimal overflow"))?
    } else {
        let down = (-shift) as u32;
        if down <= 38 {
            let denom = 10u128.pow(down);
            let (mut q, r) = u256_div_rem(hi, lo, denom)
                .ok_or_else(|| QueryError::failed("decimal overflow"))?;
            if r >= denom / 2 {
                q = q
                    .checked_add(1)
                    .ok_or_else(|| QueryError::failed("decimal overflow"))?;
            }
            q
        } else if down <= 76 {
            let (q1, _) = u256_div_rem(hi, lo, 10u128.pow(38))
                .ok_or_else(|| QueryError::failed("decimal overflow"))?;
            let d2 = 10u128.pow(down - 38);
            let mut q2 = q1 / d2;
            let r2 = q1 % d2;
            if r2 >= d2 / 2 {
                q2 = q2
                    .checked_add(1)
                    .ok_or_else(|| QueryError::failed("decimal overflow"))?;
            }
            q2
        } else {
            return Ok(0);
        }
    };
    signed_from_u128(neg, mag)
}

fn decimal_div_scaled(a: i128, b: i128, s1: i8, s2: i8, out_scale: i8) -> Result<i128, QueryError> {
    if s1.unsigned_abs() > 38 || s2.unsigned_abs() > 38 || out_scale.unsigned_abs() > 38 {
        return Err(QueryError::failed("decimal scale out of range"));
    }
    if b == 0 {
        return Err(QueryError::failed("division by zero"));
    }
    if a == 0 {
        return Ok(0);
    }
    let neg = (a < 0) ^ (b < 0);
    let a_abs = a.unsigned_abs();
    let b_abs = b.unsigned_abs();
    let shift = i32::from(out_scale) + i32::from(s2) - i32::from(s1);
    let mag = if shift >= 0 {
        let mut q = a_abs / b_abs;
        let mut rem = a_abs % b_abs;
        let mut remaining = shift as u32;
        while remaining > 0 {
            let chunk = remaining.min(38);
            let factor = 10u128.pow(chunk);
            q = q
                .checked_mul(factor)
                .ok_or_else(|| QueryError::failed("decimal division overflow"))?;
            let (hi, lo) = u256_mul(rem, factor);
            let (dq, new_rem) = u256_div_rem(hi, lo, b_abs)
                .ok_or_else(|| QueryError::failed("decimal division overflow"))?;
            q = q
                .checked_add(dq)
                .ok_or_else(|| QueryError::failed("decimal division overflow"))?;
            rem = new_rem;
            remaining -= chunk;
        }
        q
    } else {
        let down = (-shift) as u32;
        if down > 38 {
            return Ok(0);
        }
        let factor = 10u128.pow(down);
        let (hi, lo) = u256_mul(b_abs, factor);
        if hi != 0 { 0 } else { a_abs / lo }
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

    try_reserve(&mut pool, checked_bool_bytes(raw_len, true)?)?;
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

pub(crate) enum LiteralSet {
    Bool(HashSet<bool>),
    I64(HashSet<i64>),
    F64(HashSet<u64>),
    Decimal128(HashSet<i128>),
    Utf8(HashSet<String>),
    Bytes(HashSet<Vec<u8>>),
    TimestampNs(HashSet<i64>),
    Date(HashSet<i32>),
    Interval(HashSet<i128>),
}

impl LiteralSet {
    pub(crate) fn estimated_bytes(&self) -> usize {
        match self {
            Self::Bool(s) => s.len().saturating_mul(16),
            Self::I64(s) | Self::TimestampNs(s) => s.len().saturating_mul(16),
            Self::F64(s) => s.len().saturating_mul(16),
            Self::Decimal128(s) | Self::Interval(s) => s.len().saturating_mul(24),
            Self::Date(s) => s.len().saturating_mul(12),
            Self::Utf8(s) => s.iter().map(|v| size_of::<String>() + v.len() + 8).sum(),
            Self::Bytes(s) => s.iter().map(|v| size_of::<Vec<u8>>() + v.len() + 8).sum(),
        }
    }

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
            DataType::Interval => {
                let mut s = HashSet::with_capacity(scalars.len());
                for sv in scalars {
                    let ScalarValue::Interval(iv) = sv else {
                        return None;
                    };
                    s.insert(iv.canonical_key());
                }
                Self::Interval(s)
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
            Self::Interval(s) => read_interval(col, row).map(|iv| s.contains(&iv.canonical_key())),
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
        try_reserve(&mut pool, checked_bool_bytes(raw_len, true)?)?;
        let mut vals = Bitmap::new_zeroed(raw_len);
        let mut nulls = Bitmap::new_zeroed(raw_len);
        for row in 0..raw_len {
            if (row & CANCEL_CHECK_MASK) == 0 {
                check_cancel(ctx, &mut pool, before)?;
            }
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

    if let Some(cached) = ctx
        .cache
        .get_or_insert_in_list(&lhs_dt, list, || LiteralSet::from_literals(&lhs_dt, list))
    {
        let (set, has_null) = cached.as_ref();
        try_reserve(&mut pool, checked_bool_bytes(raw_len, true)?)?;
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
                Some(false) if !*has_null => {
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

    try_reserve(&mut pool, checked_bool_bytes(raw_len, true)?)?;
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
    let op_bytes = pool_size(&pool).saturating_sub(before);

    let bm_bytes = checked_bool_bytes(raw_len, true)?;
    let row_source_bytes =
        checked_fixed_bytes(raw_len, size_of::<Option<(usize, usize)>>(), false)?;
    try_reserve(
        &mut pool,
        bm_bytes
            .checked_add(row_source_bytes)
            .ok_or(QueryError::TooLarge)?,
    )?;
    let mut remaining = active.cloned().unwrap_or_else(|| Bitmap::new_ones(raw_len));
    let mut branch_cols: Vec<Arc<Column>> = Vec::with_capacity(when_then.len() + 1);
    let mut row_source: Vec<Option<(usize, usize)>> = vec![None; raw_len];

    for (when_ex, then_ex) in when_then {
        if remaining.all_zeros() {
            break;
        }
        check_cancel(ctx, &mut pool, before)?;
        let before_when = pool_size(&pool);
        let when_col =
            eval_expr_masked(when_ex, batch, Some(&remaining), ctx, pool.as_deref_mut())?;
        let mut matched = Bitmap::new_zeroed(raw_len);
        for (iter_idx, row) in remaining.iter_ones().enumerate() {
            if (iter_idx & CANCEL_CHECK_MASK) == 0 {
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
        rollback_to(&mut pool, before_when);

        if !matched.all_zeros() {
            let then_col =
                eval_expr_masked(then_ex, batch, Some(&matched), ctx, pool.as_deref_mut())?;
            let col_idx = branch_cols.len();
            branch_cols.push(then_col);
            for (iter_idx, row) in matched.iter_ones().enumerate() {
                if (iter_idx & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, &mut pool, before)?;
                }
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
        for (iter_idx, row) in remaining.iter_ones().enumerate() {
            if (iter_idx & CANCEL_CHECK_MASK) == 0 {
                check_cancel(ctx, &mut pool, before)?;
            }
            row_source[row] = Some((col_idx, row));
        }
    }

    drop(op_col);
    drop(remaining);
    let temp_bytes = op_bytes.saturating_add(bm_bytes);
    if let Some(res) = pool.as_deref_mut() {
        res.shrink(temp_bytes.min(res.size().saturating_sub(before)));
    }

    build_flat_from_fn(
        data_type,
        raw_len,
        |row| match row_source[row] {
            Some((ci, r)) if !branch_cols[ci].is_null(r) => ElemRef::Col(&branch_cols[ci], r),
            _ => ElemRef::Null,
        },
        &mut pool,
        ctx,
        before,
    )
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
    let est = checked_varlen_bytes(raw_len, total_bytes, true)?;
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

    let header_bytes = checked_varlen_bytes(raw_len, 0, active.is_some())?;
    let scratch_bytes = checked_fixed_bytes(total_elems, size_of::<ElemRef<'_>>(), false)?;
    try_reserve(
        &mut pool,
        header_bytes
            .checked_add(scratch_bytes)
            .ok_or(QueryError::TooLarge)?,
    )?;

    let mut child_sources: Vec<ElemRef<'_>> = Vec::with_capacity(total_elems);
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
                    child_sources.push(ElemRef::Null);
                } else {
                    child_sources.push(ElemRef::Col(ec, row));
                }
            }
        }
        offsets.push(child_sources.len() as u32);
    }

    let values = build_flat_from_elem_refs(item_type, &child_sources, &mut pool, ctx, before)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::List {
            item_type: item_type.clone(),
            offsets,
            values: Box::new(values),
        },
        nulls,
    )?))
}

pub(crate) fn build_flat_from_sources(
    data_type: &DataType,
    sources: &[Option<(&Column, usize)>],
    pool: &mut Option<&mut Reservation>,
    ctx: &EvalContext,
    before: usize,
) -> Result<Column, QueryError> {
    build_flat_from_fn(
        data_type,
        sources.len(),
        |i| match sources[i] {
            Some((col, row)) if !col.is_null(row) => ElemRef::Col(col, row),
            _ => ElemRef::Null,
        },
        pool,
        ctx,
        before,
    )
}

pub(crate) fn build_flat_from_elem_refs(
    data_type: &DataType,
    sources: &[ElemRef<'_>],
    pool: &mut Option<&mut Reservation>,
    ctx: &EvalContext,
    before: usize,
) -> Result<Column, QueryError> {
    build_flat_from_fn(data_type, sources.len(), |i| sources[i], pool, ctx, before)
}

fn build_flat_from_fn<'a>(
    data_type: &DataType,
    len: usize,
    at: impl Fn(usize) -> ElemRef<'a>,
    pool: &mut Option<&mut Reservation>,
    ctx: &EvalContext,
    before: usize,
) -> Result<Column, QueryError> {
    match data_type {
        DataType::Null => Ok(Column::Flat(FlatColumn::try_new(
            FlatData::Null(len),
            None,
        )?)),
        DataType::Bool => {
            try_reserve(pool, checked_bool_bytes(len, true)?)?;
            let mut vals = Bitmap::new_zeroed(len);
            let mut nulls = Bitmap::new_zeroed(len);
            for i in 0..len {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some(b) = at(i).read_bool() {
                    let _ = nulls.set(i, true);
                    if b {
                        let _ = vals.set(i, true);
                    }
                }
            }
            build_bool_column(len, vals, Some(nulls))
        }
        DataType::I64 => {
            try_reserve(pool, checked_fixed_bytes(len, 8, true)?)?;
            let mut vals = vec![0i64; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (i, slot) in vals.iter_mut().enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some(v) = at(i).read_i64() {
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
            try_reserve(pool, checked_fixed_bytes(len, 8, true)?)?;
            let mut vals = vec![0.0f64; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (i, slot) in vals.iter_mut().enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some(v) = at(i).read_f64() {
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
            try_reserve(pool, checked_fixed_bytes(len, 16, true)?)?;
            let mut vals = vec![0i128; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (i, slot) in vals.iter_mut().enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some((v, _)) = at(i).read_decimal() {
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
            try_reserve(pool, checked_fixed_bytes(len, 8, true)?)?;
            let mut vals = vec![0i64; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (i, slot) in vals.iter_mut().enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some(v) = at(i).read_timestamp() {
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
            try_reserve(pool, checked_fixed_bytes(len, 4, true)?)?;
            let mut vals = vec![0i32; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (i, slot) in vals.iter_mut().enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some(v) = at(i).read_date() {
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
            try_reserve(pool, checked_fixed_bytes(len, size_of::<Interval>(), true)?)?;
            let mut vals = vec![Interval::default(); len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (i, slot) in vals.iter_mut().enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some(v) = at(i).read_interval() {
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
            for i in 0..len {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some(s) = at(i).read_str() {
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
            try_reserve(pool, checked_varlen_bytes(len, total_bytes, true)?)?;
            let mut offsets = Vec::with_capacity(len + 1);
            offsets.push(0u32);
            let mut bytes = Vec::with_capacity(total_bytes);
            let mut nulls = Bitmap::new_zeroed(len);
            for i in 0..len {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some(s) = at(i).read_str() {
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
            for i in 0..len {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some(b) = at(i).read_bytes() {
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
            try_reserve(pool, checked_varlen_bytes(len, total_bytes, true)?)?;
            let mut offsets = Vec::with_capacity(len + 1);
            offsets.push(0u32);
            let mut bytes = Vec::with_capacity(total_bytes);
            let mut nulls = Bitmap::new_zeroed(len);
            for i in 0..len {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some(b) = at(i).read_bytes() {
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
            try_reserve(pool, checked_fixed_bytes(len, 8, true)?)?;
            let mut vals = vec![[0u8; 8]; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (i, slot) in vals.iter_mut().enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some(b) = at(i).read_bytes() {
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
            try_reserve(pool, checked_fixed_bytes(len, 16, true)?)?;
            let mut vals = vec![[0u8; 16]; len];
            let mut nulls = Bitmap::new_zeroed(len);
            for (i, slot) in vals.iter_mut().enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                if let Some(b) = at(i).read_bytes() {
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
        DataType::List(inner_dt) => build_list_from_refs(inner_dt, len, at, pool, ctx, before),
        DataType::MapUtf8 => build_map_from_refs(len, at, pool, ctx, before),
        DataType::Events => build_events_from_refs(len, at, pool, ctx, before),
        DataType::Links => build_links_from_refs(len, at, pool, ctx, before),
    }
}

fn build_list_from_refs<'a>(
    inner_dt: &DataType,
    len: usize,
    at: impl Fn(usize) -> ElemRef<'a>,
    pool: &mut Option<&mut Reservation>,
    ctx: &EvalContext,
    before: usize,
) -> Result<Column, QueryError> {
    let mut total_elems = 0usize;
    for i in 0..len {
        if (i & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if let Some(lref) = at(i).as_list_ref() {
            total_elems =
                total_elems
                    .checked_add(list_ref_len(lref))
                    .ok_or(ColumnError::Overflow {
                        reason: "list length exceeds usize",
                    })?;
        }
    }
    let _ = u32::try_from(total_elems).map_err(|_| ColumnError::Overflow {
        reason: "list length exceeds u32::MAX",
    })?;

    let header_bytes = checked_varlen_bytes(len, 0, true)?;
    let scratch_bytes = checked_fixed_bytes(total_elems, size_of::<ElemRef<'_>>(), false)?;
    try_reserve(
        pool,
        header_bytes
            .checked_add(scratch_bytes)
            .ok_or(QueryError::TooLarge)?,
    )?;

    let mut offsets = Vec::with_capacity(len + 1);
    offsets.push(0u32);
    let mut nulls = Bitmap::new_zeroed(len);
    let mut child_refs: Vec<ElemRef<'_>> = Vec::with_capacity(total_elems);

    for i in 0..len {
        if (i & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if let Some(lref) = at(i).as_list_ref() {
            let _ = nulls.set(i, true);
            let n = list_ref_len(lref);
            for j in 0..n {
                if (j & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, pool, before)?;
                }
                child_refs.push(list_ref_elem(lref, j));
            }
        }
        offsets.push(child_refs.len() as u32);
    }

    let values = build_flat_from_elem_refs(inner_dt, &child_refs, pool, ctx, before)?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::List {
            item_type: inner_dt.clone(),
            offsets,
            values: Box::new(values),
        },
        Some(nulls),
    )?))
}

fn build_map_from_refs<'a>(
    len: usize,
    at: impl Fn(usize) -> ElemRef<'a>,
    pool: &mut Option<&mut Reservation>,
    ctx: &EvalContext,
    before: usize,
) -> Result<Column, QueryError> {
    let mut total_entries = 0usize;
    let mut key_bytes_len = 0usize;
    let mut val_bytes_len = 0usize;
    for i in 0..len {
        if (i & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if let Some(mref) = at(i).as_map_ref() {
            match mref {
                MapRef::Const(entries) => {
                    total_entries =
                        total_entries
                            .checked_add(entries.len())
                            .ok_or(ColumnError::Overflow {
                                reason: "map entry count overflow",
                            })?;
                    for (k, v) in entries {
                        key_bytes_len =
                            key_bytes_len
                                .checked_add(k.len())
                                .ok_or(ColumnError::Overflow {
                                    reason: "map key byte length overflow",
                                })?;
                        if let Some(vs) = v {
                            val_bytes_len = val_bytes_len.checked_add(vs.len()).ok_or(
                                ColumnError::Overflow {
                                    reason: "map value byte length overflow",
                                },
                            )?;
                        }
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
                        if let Some(k) = data.keys().get(idx) {
                            key_bytes_len = key_bytes_len.checked_add(k.len()).ok_or(
                                ColumnError::Overflow {
                                    reason: "map key byte length overflow",
                                },
                            )?;
                        }
                        if !data.value_nulls().is_some_and(|bm| !bm.get(idx))
                            && let Some(v) = data.values().get(idx)
                        {
                            val_bytes_len = val_bytes_len.checked_add(v.len()).ok_or(
                                ColumnError::Overflow {
                                    reason: "map value byte length overflow",
                                },
                            )?;
                        }
                    }
                }
            }
        }
    }
    let _ = u32::try_from(total_entries).map_err(|_| ColumnError::Overflow {
        reason: "map entry count exceeds u32::MAX",
    })?;
    let _ = u32::try_from(key_bytes_len).map_err(|_| ColumnError::Overflow {
        reason: "map key byte length exceeds u32::MAX",
    })?;
    let _ = u32::try_from(val_bytes_len).map_err(|_| ColumnError::Overflow {
        reason: "map value byte length exceeds u32::MAX",
    })?;

    let est = checked_varlen_bytes(len, 0, true)?
        .checked_add(checked_varlen_bytes(total_entries, key_bytes_len, false)?)
        .and_then(|s| s.checked_add(checked_varlen_bytes(total_entries, val_bytes_len, true).ok()?))
        .ok_or(QueryError::TooLarge)?;
    try_reserve(pool, est)?;

    let mut offsets = Vec::with_capacity(len + 1);
    offsets.push(0u32);
    let mut nulls = Bitmap::new_zeroed(len);
    let mut key_offsets = Vec::with_capacity(total_entries + 1);
    key_offsets.push(0u32);
    let mut key_bytes = Vec::with_capacity(key_bytes_len);
    let mut val_offsets = Vec::with_capacity(total_entries + 1);
    val_offsets.push(0u32);
    let mut val_bytes = Vec::with_capacity(val_bytes_len);
    let mut val_nulls = Bitmap::new_zeroed(total_entries);
    let mut entry_pos = 0usize;

    for i in 0..len {
        if (i & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if let Some(mref) = at(i).as_map_ref() {
            let _ = nulls.set(i, true);
            match mref {
                MapRef::Const(entries) => {
                    for (k, v) in entries {
                        key_bytes.extend_from_slice(k.as_bytes());
                        key_offsets.push(key_bytes.len() as u32);
                        if let Some(vs) = v {
                            val_bytes.extend_from_slice(vs.as_bytes());
                            let _ = val_nulls.set(entry_pos, true);
                        }
                        val_offsets.push(val_bytes.len() as u32);
                        entry_pos += 1;
                    }
                }
                MapRef::Flat { data, start, end } => {
                    for idx in start..end {
                        if let Some(k) = data.keys().get(idx) {
                            key_bytes.extend_from_slice(k.as_bytes());
                        }
                        key_offsets.push(key_bytes.len() as u32);
                        if !data.value_nulls().is_some_and(|bm| !bm.get(idx))
                            && let Some(vs) = data.values().get(idx)
                        {
                            val_bytes.extend_from_slice(vs.as_bytes());
                            let _ = val_nulls.set(entry_pos, true);
                        }
                        val_offsets.push(val_bytes.len() as u32);
                        entry_pos += 1;
                    }
                }
            }
        }
        offsets.push(entry_pos as u32);
    }

    let keys_data = Utf8Data::try_new(key_offsets, key_bytes)?;
    let vals_data = Utf8Data::try_new(val_offsets, val_bytes)?;
    let map = MapData::try_new(offsets, keys_data, vals_data, Some(val_nulls))?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::MapUtf8(Box::new(map)),
        Some(nulls),
    )?))
}

fn build_events_from_refs<'a>(
    len: usize,
    at: impl Fn(usize) -> ElemRef<'a>,
    pool: &mut Option<&mut Reservation>,
    ctx: &EvalContext,
    before: usize,
) -> Result<Column, QueryError> {
    let mut total_events = 0usize;
    let mut name_bytes_len = 0usize;
    let mut attr_bytes_len = 0usize;
    for i in 0..len {
        if (i & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if let Some(eref) = at(i).as_events_ref() {
            match eref {
                EventsRef::Const(evs) => {
                    total_events =
                        total_events
                            .checked_add(evs.len())
                            .ok_or(ColumnError::Overflow {
                                reason: "events count overflow",
                            })?;
                    for ev in evs {
                        name_bytes_len = name_bytes_len.checked_add(ev.name.len()).ok_or(
                            ColumnError::Overflow {
                                reason: "events name byte length overflow",
                            },
                        )?;
                        if let Some(a) = &ev.attributes {
                            attr_bytes_len = attr_bytes_len.checked_add(a.len()).ok_or(
                                ColumnError::Overflow {
                                    reason: "events attributes byte length overflow",
                                },
                            )?;
                        }
                    }
                }
                EventsRef::Flat { data, start, end } => {
                    total_events =
                        total_events
                            .checked_add(end - start)
                            .ok_or(ColumnError::Overflow {
                                reason: "events count overflow",
                            })?;
                    for idx in start..end {
                        if let Some(n) = data.names().get(idx) {
                            name_bytes_len = name_bytes_len.checked_add(n.len()).ok_or(
                                ColumnError::Overflow {
                                    reason: "events name byte length overflow",
                                },
                            )?;
                        }
                        if !data.attribute_nulls().is_some_and(|bm| !bm.get(idx))
                            && let Some(a) = data.attributes().get(idx)
                        {
                            attr_bytes_len = attr_bytes_len.checked_add(a.len()).ok_or(
                                ColumnError::Overflow {
                                    reason: "events attributes byte length overflow",
                                },
                            )?;
                        }
                    }
                }
            }
        }
    }
    let _ = u32::try_from(total_events).map_err(|_| ColumnError::Overflow {
        reason: "events count exceeds u32::MAX",
    })?;
    let _ = u32::try_from(name_bytes_len).map_err(|_| ColumnError::Overflow {
        reason: "events name byte length exceeds u32::MAX",
    })?;
    let _ = u32::try_from(attr_bytes_len).map_err(|_| ColumnError::Overflow {
        reason: "events attributes byte length exceeds u32::MAX",
    })?;

    let est = checked_varlen_bytes(len, 0, true)?
        .checked_add(checked_fixed_bytes(total_events, 8, false)?)
        .and_then(|s| {
            s.checked_add(checked_varlen_bytes(total_events, name_bytes_len, false).ok()?)
        })
        .and_then(|s| s.checked_add(checked_varlen_bytes(total_events, attr_bytes_len, true).ok()?))
        .ok_or(QueryError::TooLarge)?;
    try_reserve(pool, est)?;

    let mut offsets = Vec::with_capacity(len + 1);
    offsets.push(0u32);
    let mut nulls = Bitmap::new_zeroed(len);
    let mut times = Vec::with_capacity(total_events);
    let mut name_offsets = Vec::with_capacity(total_events + 1);
    name_offsets.push(0u32);
    let mut name_bytes = Vec::with_capacity(name_bytes_len);
    let mut attr_offsets = Vec::with_capacity(total_events + 1);
    attr_offsets.push(0u32);
    let mut attr_bytes = Vec::with_capacity(attr_bytes_len);
    let mut attr_nulls = Bitmap::new_zeroed(total_events);
    let mut ev_pos = 0usize;

    for i in 0..len {
        if (i & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if let Some(eref) = at(i).as_events_ref() {
            let _ = nulls.set(i, true);
            match eref {
                EventsRef::Const(evs) => {
                    for ev in evs {
                        times.push(ev.time);
                        name_bytes.extend_from_slice(ev.name.as_bytes());
                        name_offsets.push(name_bytes.len() as u32);
                        if let Some(a) = &ev.attributes {
                            attr_bytes.extend_from_slice(a.as_bytes());
                            let _ = attr_nulls.set(ev_pos, true);
                        }
                        attr_offsets.push(attr_bytes.len() as u32);
                        ev_pos += 1;
                    }
                }
                EventsRef::Flat { data, start, end } => {
                    for idx in start..end {
                        times.push(data.times()[idx]);
                        if let Some(n) = data.names().get(idx) {
                            name_bytes.extend_from_slice(n.as_bytes());
                        }
                        name_offsets.push(name_bytes.len() as u32);
                        if !data.attribute_nulls().is_some_and(|bm| !bm.get(idx))
                            && let Some(a) = data.attributes().get(idx)
                        {
                            attr_bytes.extend_from_slice(a.as_bytes());
                            let _ = attr_nulls.set(ev_pos, true);
                        }
                        attr_offsets.push(attr_bytes.len() as u32);
                        ev_pos += 1;
                    }
                }
            }
        }
        offsets.push(ev_pos as u32);
    }

    let names = Utf8Data::try_new(name_offsets, name_bytes)?;
    let attrs = Utf8Data::try_new(attr_offsets, attr_bytes)?;
    let ev_data = EventsData::try_new(offsets, times, names, attrs, Some(attr_nulls))?;
    Ok(Column::Flat(FlatColumn::try_new(
        FlatData::Events(Box::new(ev_data)),
        Some(nulls),
    )?))
}

fn build_links_from_refs<'a>(
    len: usize,
    at: impl Fn(usize) -> ElemRef<'a>,
    pool: &mut Option<&mut Reservation>,
    ctx: &EvalContext,
    before: usize,
) -> Result<Column, QueryError> {
    let mut total_links = 0usize;
    let mut attr_bytes_len = 0usize;
    for i in 0..len {
        if (i & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if let Some(lref) = at(i).as_links_ref() {
            match lref {
                LinksRef::Const(lks) => {
                    total_links =
                        total_links
                            .checked_add(lks.len())
                            .ok_or(ColumnError::Overflow {
                                reason: "links count overflow",
                            })?;
                    for lk in lks {
                        if let Some(a) = &lk.attributes {
                            attr_bytes_len = attr_bytes_len.checked_add(a.len()).ok_or(
                                ColumnError::Overflow {
                                    reason: "links attributes byte length overflow",
                                },
                            )?;
                        }
                    }
                }
                LinksRef::Flat { data, start, end } => {
                    total_links =
                        total_links
                            .checked_add(end - start)
                            .ok_or(ColumnError::Overflow {
                                reason: "links count overflow",
                            })?;
                    for idx in start..end {
                        if !data.attribute_nulls().is_some_and(|bm| !bm.get(idx))
                            && let Some(a) = data.attributes().get(idx)
                        {
                            attr_bytes_len = attr_bytes_len.checked_add(a.len()).ok_or(
                                ColumnError::Overflow {
                                    reason: "links attributes byte length overflow",
                                },
                            )?;
                        }
                    }
                }
            }
        }
    }
    let _ = u32::try_from(total_links).map_err(|_| ColumnError::Overflow {
        reason: "links count exceeds u32::MAX",
    })?;
    let _ = u32::try_from(attr_bytes_len).map_err(|_| ColumnError::Overflow {
        reason: "links attributes byte length exceeds u32::MAX",
    })?;

    let est = checked_varlen_bytes(len, 0, true)?
        .checked_add(checked_fixed_bytes(total_links, 24, false)?)
        .and_then(|s| s.checked_add(checked_varlen_bytes(total_links, attr_bytes_len, true).ok()?))
        .ok_or(QueryError::TooLarge)?;
    try_reserve(pool, est)?;

    let mut offsets = Vec::with_capacity(len + 1);
    offsets.push(0u32);
    let mut nulls = Bitmap::new_zeroed(len);
    let mut trace_ids = Vec::with_capacity(total_links);
    let mut span_ids = Vec::with_capacity(total_links);
    let mut attr_offsets = Vec::with_capacity(total_links + 1);
    attr_offsets.push(0u32);
    let mut attr_bytes = Vec::with_capacity(attr_bytes_len);
    let mut attr_nulls = Bitmap::new_zeroed(total_links);
    let mut lk_pos = 0usize;

    for i in 0..len {
        if (i & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if let Some(lref) = at(i).as_links_ref() {
            let _ = nulls.set(i, true);
            match lref {
                LinksRef::Const(lks) => {
                    for lk in lks {
                        trace_ids.push(lk.trace_id);
                        span_ids.push(lk.span_id);
                        if let Some(a) = &lk.attributes {
                            attr_bytes.extend_from_slice(a.as_bytes());
                            let _ = attr_nulls.set(lk_pos, true);
                        }
                        attr_offsets.push(attr_bytes.len() as u32);
                        lk_pos += 1;
                    }
                }
                LinksRef::Flat { data, start, end } => {
                    for idx in start..end {
                        trace_ids.push(data.trace_ids()[idx]);
                        span_ids.push(data.span_ids()[idx]);
                        if !data.attribute_nulls().is_some_and(|bm| !bm.get(idx))
                            && let Some(a) = data.attributes().get(idx)
                        {
                            attr_bytes.extend_from_slice(a.as_bytes());
                            let _ = attr_nulls.set(lk_pos, true);
                        }
                        attr_offsets.push(attr_bytes.len() as u32);
                        lk_pos += 1;
                    }
                }
            }
        }
        offsets.push(lk_pos as u32);
    }

    let attrs = Utf8Data::try_new(attr_offsets, attr_bytes)?;
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
) -> Result<Arc<Column>, QueryError> {
    let before = pool_size(&pool);
    target.validate(MAX_NESTING_DEPTH)?;
    let raw_len = batch.raw_len();
    let src_dt = inner.data_type();

    let src = eval_expr_masked(inner, batch, active, ctx, pool.as_deref_mut())?;

    if src_dt == *target {
        return Ok(src);
    }

    if let (DataType::TimestampNs { .. }, DataType::TimestampNs { utc }) = (&src_dt, target) {
        return retag_timestamp_utc(&src, *utc, &mut pool).map(Arc::new);
    }

    if matches!(src_dt, DataType::Null) || matches!(target, DataType::Null) {
        return Ok(Arc::new(Column::Const {
            value: ScalarValue::Null(target.clone()),
            len: raw_len,
        }));
    }

    let out = match (&src_dt, target) {
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
            try_reserve(&mut pool, checked_bool_bytes(raw_len, true)?)?;
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
            try_reserve(&mut pool, checked_bool_bytes(raw_len, true)?)?;
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
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, &mut pool, before)?;
                }
                if is_active(active, row)
                    && let Some(s) = read_str(&src, row)
                {
                    total_bytes = total_bytes
                        .checked_add(s.len())
                        .ok_or(QueryError::TooLarge)?;
                }
            }
            let _ = u32::try_from(total_bytes).map_err(|_| QueryError::TooLarge)?;
            try_reserve(&mut pool, checked_varlen_bytes(raw_len, total_bytes, true)?)?;
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
                let end = u32::try_from(bytes.len()).map_err(|_| QueryError::TooLarge)?;
                offsets.push(end);
            }
            let bin = BinaryData::try_new(offsets, bytes)?;
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::Binary(bin),
                Some(nulls),
            )?))
        }
        (DataType::Utf8, DataType::FixedBytes8) => {
            try_reserve(&mut pool, checked_fixed_bytes(raw_len, 8, true)?)?;
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
            try_reserve(&mut pool, checked_fixed_bytes(raw_len, 16, true)?)?;
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
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(ctx, &mut pool, before)?;
                }
                if is_active(active, row)
                    && let Some(b) = read_bytes(&src, row)
                {
                    total_bytes = total_bytes
                        .checked_add(b.len())
                        .ok_or(QueryError::TooLarge)?;
                }
            }
            let _ = u32::try_from(total_bytes).map_err(|_| QueryError::TooLarge)?;
            try_reserve(&mut pool, checked_varlen_bytes(raw_len, total_bytes, true)?)?;
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
                let end = u32::try_from(bytes.len()).map_err(|_| QueryError::TooLarge)?;
                offsets.push(end);
            }
            let bin = BinaryData::try_new(offsets, bytes)?;
            Ok(Column::Flat(FlatColumn::try_new(
                FlatData::Binary(bin),
                Some(nulls),
            )?))
        }
        (DataType::Binary, DataType::FixedBytes8) => {
            try_reserve(&mut pool, checked_fixed_bytes(raw_len, 8, true)?)?;
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
            try_reserve(&mut pool, checked_fixed_bytes(raw_len, 16, true)?)?;
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
            try_reserve(&mut pool, checked_varlen_bytes(raw_len, 0, true)?)?;
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
    };
    out.map(Arc::new)
}

fn retag_timestamp_utc(
    src: &Column,
    utc: bool,
    pool: &mut Option<&mut Reservation>,
) -> Result<Column, QueryError> {
    try_reserve(pool, src.memory_bytes())?;
    retag_timestamp_utc_inner(src, utc)
}

fn retag_timestamp_utc_inner(src: &Column, utc: bool) -> Result<Column, QueryError> {
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
                ScalarValue::TimestampNs { value, .. } => {
                    ScalarValue::TimestampNs { value: *value, utc }
                }
                ScalarValue::Null(_) => ScalarValue::Null(DataType::TimestampNs { utc }),
                _ => return Err(QueryError::failed("expected timestamp scalar")),
            };
            Ok(Column::Const {
                value: new_val,
                len: *len,
            })
        }
        Column::Dict(d) => {
            let Column::Flat(new_vals) =
                retag_timestamp_utc_inner(&Column::Flat(d.values().clone()), utc)?
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
                retag_timestamp_utc_inner(&Column::Flat(r.values().clone()), utc)?
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
    use std::fmt::Write;

    try_reserve(pool, checked_varlen_bytes(raw_len, 0, true)?)?;
    let mut offsets = Vec::with_capacity(raw_len + 1);
    offsets.push(0u32);
    let mut bytes = Vec::new();
    let mut nulls = Bitmap::new_zeroed(raw_len);
    let mut scratch = String::new();

    for row in 0..raw_len {
        if (row & CANCEL_CHECK_MASK) == 0 {
            check_cancel(ctx, pool, before)?;
        }
        if is_active(active, row) && !src.is_null(row) {
            let row_slice: Option<&[u8]> = match src_dt {
                DataType::Bool => read_bool(src, row).map(|b| {
                    scratch.clear();
                    let _ = write!(scratch, "{b}");
                    scratch.as_bytes()
                }),
                DataType::I64 => read_i64(src, row).map(|v| {
                    scratch.clear();
                    let _ = write!(scratch, "{v}");
                    scratch.as_bytes()
                }),
                DataType::F64 => read_f64(src, row).map(|v| {
                    scratch.clear();
                    let _ = write!(scratch, "{v}");
                    scratch.as_bytes()
                }),
                DataType::Decimal128 { scale, .. } => read_decimal(src, row).map(|v| {
                    scratch = format_decimal(v, *scale);
                    scratch.as_bytes()
                }),
                DataType::Binary => match read_bytes(src, row) {
                    Some(b) => match std::str::from_utf8(b) {
                        Ok(s) => Some(s.as_bytes()),
                        Err(_) => match mode {
                            CastMode::Strict => {
                                return Err(QueryError::failed("invalid utf-8 in binary cast"));
                            }
                            CastMode::Try => None,
                        },
                    },
                    None => None,
                },
                DataType::FixedBytes8 | DataType::FixedBytes16 => read_bytes(src, row).map(|b| {
                    scratch = hex::encode(b);
                    scratch.as_bytes()
                }),
                DataType::TimestampNs { utc } => read_timestamp(src, row).map(|ns| {
                    scratch = crate::fn_time::format_timestamp_ns(ns, *utc);
                    scratch.as_bytes()
                }),
                DataType::Date => read_date(src, row).map(|days| {
                    scratch = crate::fn_time::format_date_days(days);
                    scratch.as_bytes()
                }),
                _ => return Err(QueryError::failed("unsupported cast to utf8")),
            };
            if let Some(slice) = row_slice {
                let next_len =
                    bytes
                        .len()
                        .checked_add(slice.len())
                        .ok_or(ColumnError::Overflow {
                            reason: "utf8 cast byte length overflow",
                        })?;
                let _ = u32::try_from(next_len).map_err(|_| ColumnError::Overflow {
                    reason: "utf8 cast byte length exceeds u32::MAX",
                })?;
                try_reserve(pool, slice.len())?;
                bytes.extend_from_slice(slice);
                let _ = nulls.set(row, true);
            }
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
    try_reserve(pool, checked_fixed_bytes(raw_len, size_of::<T>(), true)?)?;
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
            .checked_pow(u32::from(scale.unsigned_abs()))
            .ok_or_else(|| QueryError::failed("decimal scale overflow"))?;
        i128::from(v) / denom
    };
    check_decimal_bounds(scaled, precision)
}

struct StackFmtBuf {
    buf: [u8; 128],
    len: usize,
}

impl std::fmt::Write for StackFmtBuf {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let bytes = s.as_bytes();
        let end = self.len.checked_add(bytes.len()).ok_or(std::fmt::Error)?;
        if end > self.buf.len() {
            return Err(std::fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }
}

pub(crate) fn f64_to_decimal(v: f64, precision: u8, scale: i8) -> Result<i128, QueryError> {
    if !v.is_finite() {
        return Err(QueryError::failed(
            "cannot cast non-finite float to decimal",
        ));
    }
    if precision == 0 || precision > 38 {
        return Err(QueryError::failed("decimal precision out of range"));
    }
    if v == 0.0 {
        return Ok(0);
    }
    let max_exp = i32::from(precision) - i32::from(scale) + 1;
    if v.abs() >= 10f64.powi(max_exp) {
        return Err(QueryError::failed("float out of range for decimal"));
    }
    let min_exp = -i32::from(scale) - 2;
    if v.abs() < 10f64.powi(min_exp) {
        return Ok(0);
    }
    let mut buf = StackFmtBuf {
        buf: [0u8; 128],
        len: 0,
    };
    if std::fmt::write(&mut buf, format_args!("{v}")).is_ok()
        && let Ok(s) = std::str::from_utf8(&buf.buf[..buf.len])
    {
        return parse_decimal_str(s, precision, scale);
    }
    let s = format!("{v}");
    parse_decimal_str(&s, precision, scale)
}

fn decimal_to_i64(v: i128, scale: i8) -> Option<i64> {
    let unscaled = if scale >= 0 {
        let denom = 10i128.checked_pow(scale as u32)?;
        v / denom
    } else {
        let factor = 10i128.checked_pow(u32::from(scale.unsigned_abs()))?;
        v.checked_mul(factor)?
    };
    i64::try_from(unscaled).ok()
}

fn rescale_decimal(v: i128, s1: i8, p2: u8, s2: i8) -> Result<i128, QueryError> {
    if v == 0 {
        return check_decimal_bounds(0, p2);
    }
    let res = if s2 >= s1 {
        let diff = (s2 as i32 - s1 as i32) as u32;
        let factor = 10i128
            .checked_pow(diff)
            .ok_or_else(|| QueryError::failed("decimal scale overflow"))?;
        v.checked_mul(factor)
            .ok_or_else(|| QueryError::failed("decimal overflow"))?
    } else {
        let diff = (s1 as i32 - s2 as i32) as u32;
        if diff > 38 {
            return check_decimal_bounds(0, p2);
        }
        let denom = 10u128.pow(diff);
        let neg = v < 0;
        let abs = v.unsigned_abs();
        let mut q = abs / denom;
        let r = abs % denom;
        if r >= denom / 2 {
            q = q
                .checked_add(1)
                .ok_or_else(|| QueryError::failed("decimal overflow"))?;
        }
        signed_from_u128(neg, q)?
    };
    check_decimal_bounds(res, p2)
}

fn parse_decimal_str(text: &str, precision: u8, scale: i8) -> Result<i128, QueryError> {
    if !(1..=38).contains(&precision) {
        return Err(QueryError::failed("decimal precision out of range"));
    }
    let max_scale = precision as i8;
    if !(-max_scale..=max_scale).contains(&scale) {
        return Err(QueryError::failed("decimal scale out of range"));
    }

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
    let (mantissa, exp_opt) = match rest.find(['e', 'E']) {
        Some(idx) => (&rest[..idx], Some(&rest[idx + 1..])),
        None => (rest, None),
    };
    let exp: i128 = match exp_opt {
        None => 0,
        Some(exp_str) => {
            let (exp_neg, exp_digits) = if let Some(r) = exp_str.strip_prefix('-') {
                (true, r)
            } else if let Some(r) = exp_str.strip_prefix('+') {
                (false, r)
            } else {
                (false, exp_str)
            };
            if exp_digits.is_empty() || !exp_digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(QueryError::failed("invalid decimal string"));
            }
            let mut acc: i128 = 0;
            for b in exp_digits.bytes() {
                acc = acc
                    .saturating_mul(10)
                    .saturating_add(i128::from(b - b'0'))
                    .min(1_000_000_000);
            }
            if exp_neg { -acc } else { acc }
        }
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
    if (int_part.is_empty() && frac_part.is_empty())
        || !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(QueryError::failed("invalid decimal string"));
    }

    let mut sig = [0u8; 39];
    let mut sig_len = 0usize;
    let first_pos: i128;

    let int_sig = int_part.trim_start_matches('0');
    if !int_sig.is_empty() {
        let int_pos = i128::try_from(int_sig.len())
            .unwrap_or(1_000_000_000)
            .saturating_sub(1);
        first_pos = int_pos.saturating_add(exp);
        for &b in int_sig.as_bytes().iter().chain(frac_part.as_bytes()) {
            if sig_len < sig.len() {
                sig[sig_len] = b;
                sig_len += 1;
            } else {
                break;
            }
        }
    } else {
        let Some(first_nz) = frac_part.bytes().position(|b| b != b'0') else {
            return Ok(0);
        };
        let frac_pos = -(i128::try_from(first_nz)
            .unwrap_or(1_000_000_000)
            .saturating_add(1));
        first_pos = frac_pos.saturating_add(exp);
        for &b in &frac_part.as_bytes()[first_nz..] {
            if sig_len < sig.len() {
                sig[sig_len] = b;
                sig_len += 1;
            } else {
                break;
            }
        }
    }

    let keep = first_pos
        .saturating_add(i128::from(scale))
        .saturating_add(1);
    if keep > i128::from(precision) {
        return Err(QueryError::failed("decimal overflow"));
    }
    let mag: u128 = if keep < 0 {
        0
    } else if keep == 0 {
        u128::from(sig[0] >= b'5')
    } else {
        let keep_usize = keep as usize;
        let copied = keep_usize.min(sig_len);
        let mut val = 0u128;
        for &b in &sig[..copied] {
            val = val * 10 + u128::from(b - b'0');
        }
        for _ in copied..keep_usize {
            val *= 10;
        }
        if keep_usize < sig_len && sig[keep_usize] >= b'5' {
            val = val
                .checked_add(1)
                .ok_or_else(|| QueryError::failed("decimal overflow"))?;
        }
        val
    };

    let signed = signed_from_u128(neg, mag)?;
    check_decimal_bounds(signed, precision)
}

pub(crate) fn format_decimal(value: i128, scale: i8) -> String {
    if scale == 0 {
        return value.to_string();
    }
    if scale < 0 {
        if value == 0 {
            return "0".to_owned();
        }
        let mut s = value.to_string();
        for _ in 0..usize::from(scale.unsigned_abs()) {
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
        EvalContext::new(1_700_000_000_000_000_000, CancelToken::new())
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
        let cancelled_ctx = EvalContext::new(0, CancelToken::new());
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

    #[test]
    fn decimal_extreme_scales_and_parse_validation() {
        // Blocker 1: |s1 - s2| > 38 without 10u128.pow overflow panic.
        assert_eq!(compare_decimals(0, -10, 0, 38).unwrap(), Ordering::Equal);
        assert_eq!(compare_decimals(0, -10, 1, 38).unwrap(), Ordering::Less);
        assert_eq!(compare_decimals(0, -10, -1, 38).unwrap(), Ordering::Greater);
        assert_eq!(
            compare_decimals(1, -10, 999, 38).unwrap(),
            Ordering::Greater
        );
        assert_eq!(compare_decimals(-1, -10, -999, 38).unwrap(), Ordering::Less);
        assert_eq!(compare_decimals(0, 38, 0, -10).unwrap(), Ordering::Equal);
        assert_eq!(compare_decimals(5, 38, 0, -10).unwrap(), Ordering::Greater);
        assert_eq!(compare_decimals(999, 38, 1, -10).unwrap(), Ordering::Less);
        assert_eq!(
            compare_decimals(-999, 38, -1, -10).unwrap(),
            Ordering::Greater
        );

        // Blocker 2: scale == i8::MIN must never panic on `-scale`.
        assert_eq!(format_decimal(0, i8::MIN), "0");
        assert!(format_decimal(5, i8::MIN).starts_with('5'));
        assert_eq!(format_decimal(5, i8::MIN).len(), 1 + 128);
        assert!(i64_to_decimal(5, 10, i8::MIN).is_err());
        assert_eq!(decimal_to_i64(5, i8::MIN), None);
        assert!(decimal_mul_scaled(1, 2, i8::MIN, 0, 0).is_err());
        assert!(decimal_div_scaled(1, 2, i8::MIN, 0, 0).is_err());
        assert!(parse_decimal_str("1", 10, i8::MIN).is_err());

        // Minor 5: parse_decimal_str trims trailing fractional zeros and handles >38 frac digits.
        assert_eq!(
            parse_decimal_str("1.5000000000000000000000000000000000000000", 10, 2).unwrap(),
            150
        );
        assert_eq!(
            parse_decimal_str("1.236000000000000000000000000000000000000001", 10, 2).unwrap(),
            124
        );
        assert_eq!(
            parse_decimal_str("0.0000000000000000000000000000000000000005", 38, 38).unwrap(),
            0
        );

        // Blocker 2: DataType::validate in eval_cast / eval_arith / eval_neg.
        let schema = Schema::try_new_arc(vec![Field::new("x", DataType::I64, false)]).unwrap();
        let batch = Batch::try_new_all(
            schema,
            vec![Column::Flat(
                FlatColumn::try_new(FlatData::I64(vec![1]), None).unwrap(),
            )],
            1,
        )
        .unwrap();
        let bad_cast = Expr::Cast {
            expr: Box::new(Expr::Column {
                index: 0,
                name: "x".into(),
                data_type: DataType::I64,
            }),
            target: DataType::Decimal128 {
                precision: 5,
                scale: i8::MIN,
            },
            mode: CastMode::Strict,
        };
        assert!(matches!(
            eval_expr(&bad_cast, &batch, &ctx(), None).unwrap_err(),
            QueryError::Column(ColumnError::InvalidBatch { .. })
        ));

        let bad_arith = Expr::Binary {
            left: Box::new(Expr::Literal(ScalarValue::Null(DataType::Null))),
            op: BinaryOp::Add,
            right: Box::new(Expr::Literal(ScalarValue::Null(DataType::Null))),
            data_type: DataType::Decimal128 {
                precision: 0,
                scale: 0,
            },
        };
        assert!(matches!(
            eval_expr(&bad_arith, &batch, &ctx(), None).unwrap_err(),
            QueryError::Column(ColumnError::InvalidBatch { .. })
        ));
    }

    #[test]
    fn reservation_guards_nested_builders_case_and_casts() {
        // Major 2: checked_fixed_bytes / checked_varlen_bytes overflow -> TooLarge
        assert!(matches!(
            checked_fixed_bytes(usize::MAX, 8, true),
            Err(QueryError::TooLarge)
        ));
        assert!(matches!(
            checked_varlen_bytes(usize::MAX, 0, true),
            Err(QueryError::TooLarge)
        ));

        // Blocker 3a: Expr::Column and retag_timestamp_utc reserve before cloning.
        let n = 128usize;
        let schema = Schema::try_new_arc(vec![
            Field::new("x", DataType::I64, false),
            Field::new("ts", DataType::timestamp_utc(), false),
            Field::new("s", DataType::Utf8, true),
            Field::new("fb", DataType::FixedBytes8, true),
        ])
        .unwrap();
        let x_col = Column::Flat(FlatColumn::try_new(FlatData::I64(vec![42; n]), None).unwrap());
        let ts_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: vec![1_700_000_000_000_000_000; n],
                    utc: true,
                },
                None,
            )
            .unwrap(),
        );
        let s_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(Utf8Data::try_from_slices(vec!["hello_world_payload"; n]).unwrap()),
                None,
            )
            .unwrap(),
        );
        let fb_col = Column::Flat(
            FlatColumn::try_new(FlatData::FixedBytes8(vec![[1u8; 8]; n]), None).unwrap(),
        );
        let col_bytes = x_col.memory_bytes();
        let ts_bytes = ts_col.memory_bytes();
        let batch =
            Batch::try_new_all(Arc::clone(&schema), vec![x_col, ts_col, s_col, fb_col], n).unwrap();

        let tiny_pool = GlobalPool::standalone(col_bytes / 2);
        let mut tiny_res = tiny_pool.register("col_zero_copy", false);
        let col_expr = Expr::Column {
            index: 0,
            name: "x".into(),
            data_type: DataType::I64,
        };
        let zero_copy_col = eval_expr(&col_expr, &batch, &ctx(), Some(&mut tiny_res)).unwrap();
        assert!(Arc::ptr_eq(&zero_copy_col, batch.column_arc(0).unwrap()));
        assert_eq!(tiny_res.size(), 0);

        // Allocating binary expression under tight pool still fails with TooLarge and rolls back.
        let alloc_expr = Expr::Binary {
            left: Box::new(col_expr),
            op: BinaryOp::Add,
            right: Box::new(Expr::Literal(ScalarValue::I64(1))),
            data_type: DataType::I64,
        };
        assert!(matches!(
            eval_expr(&alloc_expr, &batch, &ctx(), Some(&mut tiny_res)).unwrap_err(),
            QueryError::TooLarge
        ));
        assert_eq!(tiny_res.size(), 0);

        // retag_timestamp_utc now sees zero-copy src, so peak is 1 * ts_bytes.
        let retag_pool = GlobalPool::standalone(ts_bytes / 2);
        let mut retag_res = retag_pool.register("retag_oom", false);
        let retag_expr = Expr::Cast {
            expr: Box::new(Expr::Column {
                index: 1,
                name: "ts".into(),
                data_type: DataType::timestamp_utc(),
            }),
            target: DataType::TimestampNs { utc: false },
            mode: CastMode::Strict,
        };
        assert!(matches!(
            eval_expr(&retag_expr, &batch, &ctx(), Some(&mut retag_res)).unwrap_err(),
            QueryError::TooLarge
        ));
        assert_eq!(retag_res.size(), 0);

        let retag_fit_pool = GlobalPool::standalone(ts_bytes + ts_bytes / 2);
        let mut retag_fit_res = retag_fit_pool.register("retag_fit", false);
        let retagged = eval_expr(&retag_expr, &batch, &ctx(), Some(&mut retag_fit_res)).unwrap();
        assert_eq!(retag_fit_res.size(), retagged.memory_bytes());

        // Blocker 3d & Blocker 4: cast_to_utf8, Utf8 -> Binary, FixedBytes8 -> Binary OOM rollback.
        let cast_pool = GlobalPool::standalone(256);
        let mut cast_res = cast_pool.register("cast_oom", false);
        cast_res.try_grow(16).unwrap();
        for cast_expr in [
            Expr::Cast {
                expr: Box::new(Expr::Column {
                    index: 2,
                    name: "s".into(),
                    data_type: DataType::Utf8,
                }),
                target: DataType::Binary,
                mode: CastMode::Strict,
            },
            Expr::Cast {
                expr: Box::new(Expr::Column {
                    index: 3,
                    name: "fb".into(),
                    data_type: DataType::FixedBytes8,
                }),
                target: DataType::Binary,
                mode: CastMode::Strict,
            },
            Expr::Cast {
                expr: Box::new(Expr::Column {
                    index: 0,
                    name: "x".into(),
                    data_type: DataType::I64,
                }),
                target: DataType::Utf8,
                mode: CastMode::Strict,
            },
        ] {
            assert!(matches!(
                eval_expr(&cast_expr, &batch, &ctx(), Some(&mut cast_res)).unwrap_err(),
                QueryError::TooLarge
            ));
            assert_eq!(cast_res.size(), 16);
        }

        // Major 6: CancelToken aborts empty InList, Utf8->Binary, FixedBytes8->Binary, cast_to_utf8.
        let cancelled = EvalContext::new(0, CancelToken::new());
        cancelled.cancel.cancel();
        let masked = batch
            .with_selection(Selection::Mask(Bitmap::new_ones(n)))
            .unwrap();
        for cancel_expr in [
            Expr::InList {
                expr: Box::new(Expr::Column {
                    index: 0,
                    name: "x".into(),
                    data_type: DataType::I64,
                }),
                list: vec![],
                negated: false,
                redacted: false,
            },
            Expr::Cast {
                expr: Box::new(Expr::Column {
                    index: 2,
                    name: "s".into(),
                    data_type: DataType::Utf8,
                }),
                target: DataType::Binary,
                mode: CastMode::Strict,
            },
            Expr::Cast {
                expr: Box::new(Expr::Column {
                    index: 3,
                    name: "fb".into(),
                    data_type: DataType::FixedBytes8,
                }),
                target: DataType::Binary,
                mode: CastMode::Strict,
            },
            Expr::Cast {
                expr: Box::new(Expr::Column {
                    index: 0,
                    name: "x".into(),
                    data_type: DataType::I64,
                }),
                target: DataType::Utf8,
                mode: CastMode::Strict,
            },
        ] {
            assert!(matches!(
                eval_expr(&cancel_expr, &masked, &cancelled, None).unwrap_err(),
                QueryError::Cancelled
            ));
        }

        // Major 1: eval_case must shrink when_col & op_col reservations across many branches.
        let mut when_then = Vec::new();
        for b in 0..12i64 {
            when_then.push((
                Expr::Literal(ScalarValue::I64(b)),
                Expr::Literal(ScalarValue::I64(b * 10)),
            ));
        }
        let many_case = Expr::Case {
            operand: Some(Box::new(Expr::Column {
                index: 0,
                name: "x".into(),
                data_type: DataType::I64,
            })),
            when_then,
            else_expr: Some(Box::new(Expr::Literal(ScalarValue::I64(-1)))),
            data_type: DataType::I64,
        };
        // Batch has x = 42, so all 12 WHEN branches run and miss, then ELSE runs.
        // Without rollback per branch, 12 when_cols + op_col + row_source would leak.
        let case_pool = GlobalPool::standalone(6 * 1024);
        let mut case_res = case_pool.register("case_tight", false);
        let case_out = eval_expr(&many_case, &batch, &ctx(), Some(&mut case_res)).unwrap();
        assert_eq!(case_res.size(), case_out.memory_bytes());
        assert_eq!(read_i64(&case_out, 0), Some(-1));

        // Blocker 3b: eval_make_array reserves child_sources before allocating.
        let make_arr = Expr::MakeArray {
            elems: vec![
                Expr::Column {
                    index: 0,
                    name: "x".into(),
                    data_type: DataType::I64,
                };
                16
            ],
            item_type: DataType::I64,
        };
        let arr_oom_pool = GlobalPool::standalone(col_bytes + 512);
        let mut arr_oom_res = arr_oom_pool.register("arr_oom", false);
        assert!(matches!(
            eval_expr(&make_arr, &batch, &ctx(), Some(&mut arr_oom_res)).unwrap_err(),
            QueryError::TooLarge
        ));
        assert_eq!(arr_oom_res.size(), 0);

        // Blocker 3c: CASE over List, MapUtf8, Events, Links with zero-copy pass-1 reservation.
        let complex_schema = Schema::try_new_arc(vec![
            Field::new("flag", DataType::Bool, false),
            Field::new("l", DataType::List(Box::new(DataType::Utf8)), true),
            Field::new("m", DataType::MapUtf8, true),
            Field::new("e", DataType::Events, true),
            Field::new("k", DataType::Links, true),
        ])
        .unwrap();
        let flag_col = Column::Flat(
            FlatColumn::try_new(FlatData::Bool(Bitmap::from_bools(&[true, false])), None).unwrap(),
        );
        let list_col = Column::try_new_const(
            ScalarValue::List {
                item_type: DataType::Utf8,
                values: vec![ScalarValue::Utf8("a".into())],
            },
            2,
        )
        .unwrap();
        let map_col = Column::try_new_const(
            ScalarValue::MapUtf8(vec![("k".into(), Some("v".into()))]),
            2,
        )
        .unwrap();
        let ev_col = Column::try_new_const(
            ScalarValue::Events(vec![EventItem {
                time: 123,
                name: "ev".into(),
                attributes: Some("{}".into()),
            }]),
            2,
        )
        .unwrap();
        let lk_col = Column::try_new_const(
            ScalarValue::Links(vec![LinkItem {
                trace_id: [1; 16],
                span_id: [2; 8],
                attributes: None,
            }]),
            2,
        )
        .unwrap();
        let complex_batch = Batch::try_new_all(
            complex_schema,
            vec![flag_col, list_col, map_col, ev_col, lk_col],
            2,
        )
        .unwrap();
        let c_pool = GlobalPool::standalone(64 * 1024);
        let mut c_res = c_pool.register("complex_case", false);
        for (idx, dt) in [
            (1, DataType::List(Box::new(DataType::Utf8))),
            (2, DataType::MapUtf8),
            (3, DataType::Events),
            (4, DataType::Links),
        ] {
            let ex = Expr::Case {
                operand: None,
                when_then: vec![(
                    Expr::Column {
                        index: 0,
                        name: "flag".into(),
                        data_type: DataType::Bool,
                    },
                    Expr::Column {
                        index: idx,
                        name: "c".into(),
                        data_type: dt.clone(),
                    },
                )],
                else_expr: None,
                data_type: dt,
            };
            let before_c = c_res.size();
            let out = eval_expr(&ex, &complex_batch, &ctx(), Some(&mut c_res)).unwrap();
            assert_eq!(c_res.size(), before_c + out.memory_bytes());
            assert!(!out.is_null(0));
            assert!(out.is_null(1));
        }

        // Minor 7: boolean ops reject non-bool non-null columns.
        for bad_bool in [
            Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(Expr::Column {
                    index: 0,
                    name: "x".into(),
                    data_type: DataType::I64,
                }),
                data_type: DataType::Bool,
            },
            Expr::Unary {
                op: UnaryOp::IsTrue,
                expr: Box::new(Expr::Column {
                    index: 0,
                    name: "x".into(),
                    data_type: DataType::I64,
                }),
                data_type: DataType::Bool,
            },
            Expr::Binary {
                left: Box::new(Expr::Column {
                    index: 0,
                    name: "x".into(),
                    data_type: DataType::I64,
                }),
                op: BinaryOp::And,
                right: Box::new(Expr::Literal(ScalarValue::Bool(true))),
                data_type: DataType::Bool,
            },
            Expr::Binary {
                left: Box::new(Expr::Literal(ScalarValue::Bool(false))),
                op: BinaryOp::Or,
                right: Box::new(Expr::Column {
                    index: 0,
                    name: "x".into(),
                    data_type: DataType::I64,
                }),
                data_type: DataType::Bool,
            },
        ] {
            assert!(matches!(
                eval_expr(&bad_bool, &batch, &ctx(), None).unwrap_err(),
                QueryError::Failed(_)
            ));
        }
    }

    #[test]
    fn zero_copy_column_eval_returns_ptr_eq_and_reserves_zero_bytes() {
        let schema = Schema::try_new_arc(vec![
            Field::new("a", DataType::I64, false),
            Field::new("b", DataType::Utf8, false),
        ])
        .unwrap();
        let batch = Batch::try_new_all(
            schema,
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1, 2, 3]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(["alpha", "beta", "gamma"]).unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            3,
        )
        .unwrap();

        let expr = Expr::Column {
            index: 1,
            name: "b".to_owned(),
            data_type: DataType::Utf8,
        };
        let pool = GlobalPool::standalone(64);
        let mut res = pool.register("zero_copy", false);
        res.try_grow(32).unwrap();
        let before = res.size();

        let out = eval_expr(&expr, &batch, &ctx(), Some(&mut res)).unwrap();
        assert!(Arc::ptr_eq(&out, batch.column_arc(1).unwrap()));
        assert_eq!(res.size(), before);

        let mask = Bitmap::from_bools(&[true, false, true]);
        let masked_out =
            eval_expr_masked(&expr, &batch, Some(&mask), &ctx(), Some(&mut res)).unwrap();
        assert!(Arc::ptr_eq(&masked_out, batch.column_arc(1).unwrap()));
        assert_eq!(res.size(), before);
    }

    #[test]
    fn coalesce_short_circuits_lazily_per_row() {
        let schema = Schema::try_new_arc(vec![
            Field::new("a", DataType::I64, true),
            Field::new("b", DataType::I64, false),
        ])
        .unwrap();
        let batch = Batch::try_new_all(
            schema,
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::I64(vec![10, 0, 30]),
                        Some(Bitmap::from_bools(&[true, false, true])),
                    )
                    .unwrap(),
                ),
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![0, 5, 0]), None).unwrap()),
            ],
            3,
        )
        .unwrap();

        let div_by_b = Expr::Binary {
            left: Box::new(Expr::Literal(ScalarValue::I64(100))),
            op: BinaryOp::Div,
            right: Box::new(Expr::Column {
                index: 1,
                name: "b".to_owned(),
                data_type: DataType::I64,
            }),
            data_type: DataType::I64,
        };
        let expr = Expr::Call {
            func: ScalarFn::Coalesce,
            args: vec![
                Expr::Column {
                    index: 0,
                    name: "a".to_owned(),
                    data_type: DataType::I64,
                },
                div_by_b,
            ],
            data_type: DataType::I64,
        };
        let out = eval_expr(&expr, &batch, &ctx(), None).unwrap();
        assert_eq!(out.get_scalar(0).unwrap(), ScalarValue::I64(10));
        assert_eq!(out.get_scalar(1).unwrap(), ScalarValue::I64(20));
        assert_eq!(out.get_scalar(2).unwrap(), ScalarValue::I64(30));

        // When first argument is completely non-null, remaining arguments (even 1/0) never evaluate.
        let all_non_null = Expr::Call {
            func: ScalarFn::Coalesce,
            args: vec![
                Expr::Literal(ScalarValue::I64(7)),
                Expr::Binary {
                    left: Box::new(Expr::Literal(ScalarValue::I64(1))),
                    op: BinaryOp::Div,
                    right: Box::new(Expr::Literal(ScalarValue::I64(0))),
                    data_type: DataType::I64,
                },
            ],
            data_type: DataType::I64,
        };
        let fast = eval_expr(&all_non_null, &batch, &ctx(), None).unwrap();
        assert_eq!(fast.get_scalar(0).unwrap(), ScalarValue::I64(7));
        assert_eq!(fast.get_scalar(1).unwrap(), ScalarValue::I64(7));
        assert_eq!(fast.get_scalar(2).unwrap(), ScalarValue::I64(7));
    }

    #[test]
    fn f64_to_decimal_accuracy_negative_scale_and_bounds() {
        assert_eq!(f64_to_decimal(0.1, 10, 2).unwrap(), 10);
        assert_eq!(f64_to_decimal(0.125, 10, 2).unwrap(), 13);
        assert_eq!(f64_to_decimal(-0.125, 10, 2).unwrap(), -13);
        assert_eq!(f64_to_decimal(0.0, 10, 2).unwrap(), 0);
        assert_eq!(f64_to_decimal(-0.0, 10, 2).unwrap(), 0);
        assert_eq!(f64_to_decimal(1e-10, 10, 2).unwrap(), 0);
        assert_eq!(f64_to_decimal(1234.0, 4, -2).unwrap(), 12);
        assert_eq!(f64_to_decimal(1250.0, 4, -2).unwrap(), 13);
        assert!(matches!(
            f64_to_decimal(1e20, 10, 2),
            Err(QueryError::Failed(_))
        ));
        assert!(matches!(
            f64_to_decimal(f64::NAN, 10, 2),
            Err(QueryError::Failed(_))
        ));
        assert!(matches!(
            f64_to_decimal(f64::INFINITY, 10, 2),
            Err(QueryError::Failed(_))
        ));
    }

    #[test]
    fn f64_to_decimal_stack_fmt_bit_exact_parity() {
        let cases: &[(f64, u8, i8)] = &[
            (0.0, 38, 18),
            (-0.0, 38, 18),
            (f64::MIN_POSITIVE, 38, 38),
            (f64::MIN_POSITIVE / 2.0, 38, 38),
            (5e-324, 38, 38),
            (-5e-324, 38, 38),
            (1e-38, 38, 38),
            (-1e-38, 38, 38),
            (1e-18, 38, 18),
            (0.00012345678901234567, 38, 18),
            (-123456789.01234567, 38, 18),
            (-1e76, 38, -38),
            (9.999999999999999e75, 38, -38),
            (1e76, 38, -38),
            (12345.6789, 10, 4),
            (-12345.6789, 10, 4),
            (1e37, 38, 0),
            (-1e37, 38, 0),
        ];

        for &(v, p, s) in cases {
            let expected = {
                if !v.is_finite() {
                    Err(QueryError::failed(
                        "cannot cast non-finite float to decimal",
                    ))
                } else if p == 0 || p > 38 {
                    Err(QueryError::failed("decimal precision out of range"))
                } else if v == 0.0 {
                    Ok(0)
                } else {
                    let max_exp = i32::from(p) - i32::from(s) + 1;
                    if v.abs() >= 10f64.powi(max_exp) {
                        Err(QueryError::failed("float out of range for decimal"))
                    } else if v.abs() < 10f64.powi(-i32::from(s) - 2) {
                        Ok(0)
                    } else {
                        let txt = format!("{v}");
                        parse_decimal_str(&txt, p, s)
                    }
                }
            };
            let actual = f64_to_decimal(v, p, s);
            match (actual, expected) {
                (Ok(a), Ok(e)) => assert_eq!(a, e, "mismatch for v={v}, p={p}, s={s}"),
                (Err(QueryError::Failed(_)), Err(QueryError::Failed(_))) => {}
                (a, e) => panic!("result kind mismatch for v={v}, p={p}, s={s}: {a:?} vs {e:?}"),
            }
        }

        // Verify StackFmtBuf returns Err on overflow without panicking.
        let mut small = StackFmtBuf {
            buf: [0u8; 128],
            len: 0,
        };
        assert!(std::fmt::Write::write_str(&mut small, &"x".repeat(128)).is_ok());
        assert!(std::fmt::Write::write_str(&mut small, "y").is_err());
    }

    #[test]
    fn decimal128_and_i64_un_narrowed_comparison_and_canonical_interval() {
        let schema = Schema::try_new_arc(vec![
            Field::new(
                "d",
                DataType::Decimal128 {
                    precision: 30,
                    scale: 10,
                },
                false,
            ),
            Field::new("i", DataType::I64, false),
            Field::new("iv", DataType::Interval, false),
        ])
        .unwrap();
        let batch = Batch::try_new_all(
            schema,
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Decimal128 {
                            precision: 30,
                            scale: 10,
                            values: vec![1_000_000_000_000, 1_000_000_000_001],
                        },
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![100, 100]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Interval(vec![
                            Interval {
                                months: 1,
                                days: 0,
                                nanos: 0,
                            },
                            Interval {
                                months: 0,
                                days: 30,
                                nanos: 0,
                            },
                        ]),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            2,
        )
        .unwrap();

        let eq_expr = Expr::Binary {
            left: Box::new(Expr::Column {
                index: 0,
                name: "d".to_owned(),
                data_type: DataType::Decimal128 {
                    precision: 30,
                    scale: 10,
                },
            }),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Column {
                index: 1,
                name: "i".to_owned(),
                data_type: DataType::I64,
            }),
            data_type: DataType::Bool,
        };
        let eq_out = eval_expr(&eq_expr, &batch, &ctx(), None).unwrap();
        assert_eq!(eq_out.get_scalar(0).unwrap(), ScalarValue::Bool(true));
        assert_eq!(eq_out.get_scalar(1).unwrap(), ScalarValue::Bool(false));

        let gt_expr = Expr::Binary {
            left: Box::new(Expr::Column {
                index: 0,
                name: "d".to_owned(),
                data_type: DataType::Decimal128 {
                    precision: 30,
                    scale: 10,
                },
            }),
            op: BinaryOp::Gt,
            right: Box::new(Expr::Column {
                index: 1,
                name: "i".to_owned(),
                data_type: DataType::I64,
            }),
            data_type: DataType::Bool,
        };
        let gt_out = eval_expr(&gt_expr, &batch, &ctx(), None).unwrap();
        assert_eq!(gt_out.get_scalar(0).unwrap(), ScalarValue::Bool(false));
        assert_eq!(gt_out.get_scalar(1).unwrap(), ScalarValue::Bool(true));

        let iv_in = Expr::InList {
            expr: Box::new(Expr::Column {
                index: 2,
                name: "iv".to_owned(),
                data_type: DataType::Interval,
            }),
            list: vec![Expr::Literal(ScalarValue::Interval(Interval {
                months: 0,
                days: 30,
                nanos: 0,
            }))],
            negated: false,
            redacted: false,
        };
        let iv_out = eval_expr(&iv_in, &batch, &ctx(), None).unwrap();
        assert_eq!(iv_out.get_scalar(0).unwrap(), ScalarValue::Bool(true));
        assert_eq!(iv_out.get_scalar(1).unwrap(), ScalarValue::Bool(true));
    }

    #[test]
    fn temporal_arithmetic_date_interval_and_negation() {
        let schema = Schema::try_new_arc(vec![
            Field::new("ts", DataType::timestamp_utc(), false),
            Field::new("d1", DataType::Date, false),
            Field::new("d2", DataType::Date, false),
            Field::new("iv", DataType::Interval, false),
        ])
        .unwrap();
        let batch = Batch::try_new_all(
            schema,
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::TimestampNs {
                            values: vec![2000, 500],
                            utc: true,
                        },
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(FlatColumn::try_new(FlatData::Date(vec![10, 0]), None).unwrap()),
                Column::Flat(FlatColumn::try_new(FlatData::Date(vec![7, 3]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Interval(vec![
                            Interval {
                                months: 0,
                                days: 2,
                                nanos: 500,
                            },
                            Interval {
                                months: 0,
                                days: 1,
                                nanos: 100,
                            },
                        ]),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            2,
        )
        .unwrap();

        let ts_col = Expr::Column {
            index: 0,
            name: "ts".to_owned(),
            data_type: DataType::timestamp_utc(),
        };
        let d1_col = Expr::Column {
            index: 1,
            name: "d1".to_owned(),
            data_type: DataType::Date,
        };
        let d2_col = Expr::Column {
            index: 2,
            name: "d2".to_owned(),
            data_type: DataType::Date,
        };
        let iv_col = Expr::Column {
            index: 3,
            name: "iv".to_owned(),
            data_type: DataType::Interval,
        };

        let ts_diff = Expr::Binary {
            left: Box::new(ts_col),
            op: BinaryOp::Sub,
            right: Box::new(Expr::Literal(ScalarValue::TimestampNs {
                value: 500,
                utc: true,
            })),
            data_type: DataType::Interval,
        };
        let ts_diff_out = eval_expr(&ts_diff, &batch, &ctx(), None).unwrap();
        assert_eq!(
            ts_diff_out.get_scalar(0).unwrap(),
            ScalarValue::Interval(Interval {
                months: 0,
                days: 0,
                nanos: 1500,
            })
        );

        let date_diff = Expr::Binary {
            left: Box::new(d1_col.clone()),
            op: BinaryOp::Sub,
            right: Box::new(d2_col),
            data_type: DataType::I64,
        };
        let date_diff_out = eval_expr(&date_diff, &batch, &ctx(), None).unwrap();
        assert_eq!(date_diff_out.get_scalar(0).unwrap(), ScalarValue::I64(3));
        assert_eq!(date_diff_out.get_scalar(1).unwrap(), ScalarValue::I64(-3));

        let date_plus_iv = Expr::Binary {
            left: Box::new(d1_col),
            op: BinaryOp::Add,
            right: Box::new(iv_col.clone()),
            data_type: DataType::TimestampNs { utc: false },
        };
        let dpi_out = eval_expr(&date_plus_iv, &batch, &ctx(), None).unwrap();
        assert_eq!(
            dpi_out.get_scalar(0).unwrap(),
            ScalarValue::TimestampNs {
                value: 12 * 86_400_000_000_000 + 500,
                utc: false,
            }
        );

        let neg_iv = Expr::Unary {
            op: UnaryOp::Neg,
            expr: Box::new(iv_col),
            data_type: DataType::Interval,
        };
        let neg_out = eval_expr(&neg_iv, &batch, &ctx(), None).unwrap();
        assert_eq!(
            neg_out.get_scalar(0).unwrap(),
            ScalarValue::Interval(Interval {
                months: 0,
                days: -2,
                nanos: -500,
            })
        );
    }

    #[test]
    fn eval_cache_shares_via_with_cancel_caps_entries_and_is_redacted() {
        let schema = Schema::try_new_arc(vec![
            Field::new("s", DataType::Utf8, false),
            Field::new("i", DataType::I64, false),
        ])
        .unwrap();
        let batch = Batch::try_new_all(
            schema,
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(
                            Utf8Data::try_from_slices(["secret_needle", "other"]).unwrap(),
                        ),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![10, 99]), None).unwrap()),
            ],
            2,
        )
        .unwrap();

        let shared_ctx = EvalContext::new(0, CancelToken::new());
        let child_ctx = shared_ctx.with_cancel(CancelToken::new());

        let s_col = Expr::Column {
            index: 0,
            name: "s".to_owned(),
            data_type: DataType::Utf8,
        };
        let i_col = Expr::Column {
            index: 1,
            name: "i".to_owned(),
            data_type: DataType::I64,
        };

        // Populate LIKE (literal substring, no `_` wildcard -> no compiled Regex) and IN list
        // (>= 4 items) via child_ctx, verify shared_ctx sees them and enforces the 256-entry cap.
        for idx in 0..270usize {
            let pat = format!("%secret-needle-{idx}%");
            let like_expr = Expr::Like {
                expr: Box::new(s_col.clone()),
                pattern: Box::new(Expr::Literal(ScalarValue::Utf8(pat))),
                escape: Some('\\'),
                case_insensitive: false,
                negated: false,
            };
            let _ = eval_expr(&like_expr, &batch, &child_ctx, None).unwrap();
        }
        let (like_cnt, _, _, _, total_bytes) = shared_ctx.cache.stats();
        assert_eq!(like_cnt, 256);
        assert!(total_bytes > 0);
        assert!(total_bytes <= 4 * 1024 * 1024);
        let dbg = format!("{:?}", shared_ctx.cache);
        assert!(
            !dbg.contains("secret-needle"),
            "redacted Debug leaked: {dbg}"
        );

        let in_a = Expr::InList {
            expr: Box::new(i_col.clone()),
            list: vec![
                Expr::Literal(ScalarValue::I64(10)),
                Expr::Literal(ScalarValue::I64(20)),
                Expr::Literal(ScalarValue::I64(30)),
                Expr::Literal(ScalarValue::I64(40)),
            ],
            negated: false,
            redacted: false,
        };
        let in_b = Expr::InList {
            expr: Box::new(i_col),
            list: vec![
                Expr::Literal(ScalarValue::I64(1)),
                Expr::Literal(ScalarValue::I64(2)),
                Expr::Literal(ScalarValue::I64(3)),
                Expr::Literal(ScalarValue::I64(4)),
            ],
            negated: false,
            redacted: false,
        };
        let out_a = eval_expr(&in_a, &batch, &child_ctx, None).unwrap();
        let out_b = eval_expr(&in_b, &batch, &child_ctx, None).unwrap();
        assert_eq!(out_a.get_scalar(0).unwrap(), ScalarValue::Bool(true));
        assert_eq!(out_b.get_scalar(0).unwrap(), ScalarValue::Bool(false));
        let (_, _, _, in_list_cnt, _) = shared_ctx.cache.stats();
        assert_eq!(in_list_cnt, 2);
    }

    #[test]
    fn eval_expr_masked_like_pattern_err_rolls_back_reservation() {
        let schema = Schema::try_new_arc(vec![Field::new("s", DataType::Utf8, false)]).unwrap();
        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(["hello", "world"]).unwrap()),
                    None,
                )
                .unwrap(),
            )],
            2,
        )
        .unwrap();

        let like_expr = Expr::Like {
            expr: Box::new(Expr::Call {
                func: ScalarFn::Upper,
                args: vec![Expr::Column {
                    index: 0,
                    name: "s".to_owned(),
                    data_type: DataType::Utf8,
                }],
                data_type: DataType::Utf8,
            }),
            pattern: Box::new(Expr::Cast {
                expr: Box::new(Expr::Binary {
                    left: Box::new(Expr::Literal(ScalarValue::I64(1))),
                    op: BinaryOp::Div,
                    right: Box::new(Expr::Literal(ScalarValue::I64(0))),
                    data_type: DataType::I64,
                }),
                target: DataType::Utf8,
                mode: CastMode::Strict,
            }),
            escape: Some('\\'),
            case_insensitive: false,
            negated: false,
        };

        let pool = GlobalPool::standalone(1024 * 1024);
        let mut res = pool.register("like_rollback", false);
        res.try_grow(128).unwrap();
        let before = res.size();
        let err = eval_expr_masked(
            &like_expr,
            &batch,
            None,
            &EvalContext::new(0, CancelToken::new()),
            Some(&mut res),
        )
        .unwrap_err();
        assert!(matches!(err, QueryError::Failed(_)));
        assert_eq!(res.size(), before);
    }

    #[test]
    fn decimal_mul_div_scaled_and_arith_scale_validation() {
        // Scale validation upfront.
        assert!(decimal_mul_scaled(0, 1, 39, 0, 0).is_err());
        assert!(decimal_mul_scaled(0, 1, 0, -39, 0).is_err());
        assert!(decimal_mul_scaled(0, 1, 0, 0, 39).is_err());
        assert!(decimal_div_scaled(0, 1, 39, 0, 0).is_err());
        assert!(decimal_div_scaled(1, 0, 0, 0, 0).is_err());

        // Zero early return before shift > 38 overflow check.
        assert_eq!(decimal_mul_scaled(0, 123, -30, -30, 30).unwrap(), 0);
        assert_eq!(decimal_mul_scaled(123, 0, -30, -30, 30).unwrap(), 0);
        assert_eq!(decimal_div_scaled(0, 123, -30, 30, 30).unwrap(), 0);

        // shift == 0 must not round up!
        assert_eq!(decimal_mul_scaled(3, 4, 2, 3, 5).unwrap(), 12);
        assert_eq!(decimal_mul_scaled(-3, 4, 2, 3, 5).unwrap(), -12);

        // shift > 0 multiplies by 10^shift; shift > 38 overflows for non-zero.
        assert_eq!(decimal_mul_scaled(3, 4, 1, 1, 4).unwrap(), 1200);
        assert!(decimal_mul_scaled(1, 1, -20, -20, 0).is_err());

        // shift < 0: down in 1..=38 (half-away-from-zero).
        assert_eq!(decimal_mul_scaled(15, 10, 2, 2, 2).unwrap(), 2); // 0.15 * 0.10 = 0.015 -> 0.02
        assert_eq!(decimal_mul_scaled(-15, 10, 2, 2, 2).unwrap(), -2);
        assert_eq!(decimal_mul_scaled(14, 10, 2, 2, 2).unwrap(), 1); // 0.14 * 0.10 = 0.014 -> 0.01

        // shift < 0: down in 39..=76 (two-step exact rounding).
        let ten20 = 10_i128.pow(20);
        // (5 * 10^20) * (10^20) = 5 * 10^40; shift = 0 - 20 - 20 = -40 (down = 40) -> 5
        assert_eq!(decimal_mul_scaled(5 * ten20, ten20, 20, 20, 0).unwrap(), 5);
        // (5 * 10^19) * (10^20) = 5 * 10^39; down = 40 -> 0.5 rounds to 1
        assert_eq!(
            decimal_mul_scaled(5 * 10_i128.pow(19), ten20, 20, 20, 0).unwrap(),
            1
        );
        // (4 * 10^19) * (10^20) = 4 * 10^39; down = 40 -> 0.4 rounds to 0
        assert_eq!(
            decimal_mul_scaled(4 * 10_i128.pow(19), ten20, 20, 20, 0).unwrap(),
            0
        );
        // down > 76 always returns 0.
        assert_eq!(
            decimal_mul_scaled(10_i128.pow(37), 10_i128.pow(37), 38, 38, -2).unwrap(),
            0
        );

        // decimal_div_scaled: truncation toward zero, multi-chunk shift > 38, negative shift.
        assert_eq!(decimal_div_scaled(10, 3, 0, 0, 2).unwrap(), 333);
        assert_eq!(decimal_div_scaled(-10, 3, 0, 0, 2).unwrap(), -333);
        // shift = 12 + 0 - (-38) = 50 (> 38): 1 * 10^38 / 10^37 = 10, at out_scale=12 -> 10 * 10^12
        assert_eq!(
            decimal_div_scaled(1, 10_i128.pow(37), -38, 0, 12).unwrap(),
            10_i128.pow(13)
        );
        // shift < 0, down <= 38 and down > 38.
        assert_eq!(decimal_div_scaled(1234, 10, 4, 0, 2).unwrap(), 1);
        assert_eq!(
            decimal_div_scaled(10_i128.pow(37), 1, 38, 0, -2).unwrap(),
            0
        );

        // eval_arith rejects mismatched decimal scales on Add/Sub/Mod and non-decimal operands.
        let schema = Schema::try_new_arc(vec![Field::new("x", DataType::I64, false)]).unwrap();
        let batch = Batch::try_new_all(
            schema,
            vec![Column::Flat(
                FlatColumn::try_new(FlatData::I64(vec![1]), None).unwrap(),
            )],
            1,
        )
        .unwrap();
        let d_a = Expr::Literal(ScalarValue::Decimal128 {
            value: 100,
            precision: 10,
            scale: 2,
        });
        let d_b = Expr::Literal(ScalarValue::Decimal128 {
            value: 1000,
            precision: 10,
            scale: 3,
        });
        for op in [BinaryOp::Add, BinaryOp::Sub, BinaryOp::Mod] {
            let mismatch = Expr::Binary {
                left: Box::new(d_a.clone()),
                op,
                right: Box::new(d_b.clone()),
                data_type: DataType::Decimal128 {
                    precision: 10,
                    scale: 2,
                },
            };
            assert!(matches!(
                eval_expr(&mismatch, &batch, &ctx(), None),
                Err(QueryError::Failed(_))
            ));
        }
        let non_dec = Expr::Binary {
            left: Box::new(Expr::Literal(ScalarValue::I64(1))),
            op: BinaryOp::Add,
            right: Box::new(d_a),
            data_type: DataType::Decimal128 {
                precision: 10,
                scale: 2,
            },
        };
        assert!(matches!(
            eval_expr(&non_dec, &batch, &ctx(), None),
            Err(QueryError::Failed(_))
        ));
    }

    #[test]
    fn rescale_and_parse_decimal_scientific_and_edge_cases() {
        // rescale_decimal: v == 0 with huge diff succeeds, diff == 38 rounds, diff > 38 -> 0.
        assert_eq!(rescale_decimal(0, -20, 38, 20).unwrap(), 0);
        assert_eq!(rescale_decimal(6 * 10_i128.pow(37), 38, 10, 0).unwrap(), 1);
        assert_eq!(
            rescale_decimal(-6 * 10_i128.pow(37), 38, 10, 0).unwrap(),
            -1
        );
        assert_eq!(rescale_decimal(4 * 10_i128.pow(37), 38, 10, 0).unwrap(), 0);
        assert_eq!(rescale_decimal(9 * 10_i128.pow(37), 38, 10, -1).unwrap(), 0);

        // parse_decimal_str: invalid precision/scale rejected at top even for "0".
        assert!(parse_decimal_str("0", 0, 0).is_err());
        assert!(parse_decimal_str("0", 39, 0).is_err());
        assert!(parse_decimal_str("0", 10, 11).is_err());
        assert!(parse_decimal_str("0", 10, -11).is_err());

        // Scientific notation & valid dot forms.
        assert_eq!(parse_decimal_str(".5", 10, 2).unwrap(), 50);
        assert_eq!(parse_decimal_str("5.", 10, 2).unwrap(), 500);
        assert_eq!(parse_decimal_str("1.e2", 10, 2).unwrap(), 10_000);
        assert_eq!(parse_decimal_str(".5e2", 10, 2).unwrap(), 5_000);
        assert_eq!(parse_decimal_str("1.5e-2", 10, 4).unwrap(), 150);
        assert_eq!(parse_decimal_str("-1.5E+2", 10, 1).unwrap(), -1_500);
        assert_eq!(parse_decimal_str("5e-3", 10, 2).unwrap(), 1); // keep == 0, sig[0] == '5'
        assert_eq!(parse_decimal_str("4e-3", 10, 2).unwrap(), 0); // keep == 0, sig[0] == '4'
        assert_eq!(
            parse_decimal_str("1e-99999999999999999999", 10, 2).unwrap(),
            0
        );
        assert_eq!(
            parse_decimal_str("0e99999999999999999999", 10, 2).unwrap(),
            0
        );
        assert!(parse_decimal_str("1e99999999999999999999", 38, 0).is_err());
        assert_eq!(parse_decimal_str("50", 10, -2).unwrap(), 1);
        assert_eq!(parse_decimal_str("49", 10, -2).unwrap(), 0);
        assert_eq!(parse_decimal_str("1250", 10, -2).unwrap(), 13);

        // Invalid syntax forms rejected.
        for bad in [".", ".e2", "e2", "1e", "1e+", "1e-", "1e1e2", "1.2.3"] {
            assert!(
                parse_decimal_str(bad, 10, 2).is_err(),
                "expected `{bad}` to fail"
            );
        }
    }

    #[test]
    fn iterative_bool_chain_3vl_short_circuit_and_deep_binary_spine() {
        let schema = Schema::try_new_arc(vec![
            Field::new("a", DataType::Bool, true),
            Field::new("b", DataType::Bool, true),
            Field::new("x", DataType::I64, false),
        ])
        .unwrap();
        let batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Bool(Bitmap::from_bools(&[true, false, false, true, false])),
                        Some(Bitmap::from_bools(&[false, false, true, true, true])),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Bool(Bitmap::from_bools(&[false, true, true, false, false])),
                        Some(Bitmap::from_bools(&[true, true, false, true, true])),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(FlatData::I64(vec![0, 1, 0, 0, 1]), None).unwrap(),
                ),
            ],
            5,
        )
        .unwrap();

        let col_a = Expr::Column {
            index: 0,
            name: "a".into(),
            data_type: DataType::Bool,
        };
        let col_b = Expr::Column {
            index: 1,
            name: "b".into(),
            data_type: DataType::Bool,
        };
        // Row 0: NULL AND false AND (1/0 = 1) -> false (overrides NULL AND short-circuits 1/0!)
        // Row 1: NULL AND true AND (1/1 = 1) -> NULL
        // Row 2: false AND NULL AND (1/0 = 1) -> false (short-circuits at term 0)
        // Row 3: true AND false AND (1/0 = 1) -> false (short-circuits at term 1)
        // Row 4: false AND false AND (1/1 = 1) -> false
        let div_eq = Expr::Binary {
            left: Box::new(Expr::Binary {
                left: Box::new(Expr::Literal(ScalarValue::I64(1))),
                op: BinaryOp::Div,
                right: Box::new(Expr::Column {
                    index: 2,
                    name: "x".into(),
                    data_type: DataType::I64,
                }),
                data_type: DataType::I64,
            }),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::I64(1))),
            data_type: DataType::Bool,
        };
        let and_chain = Expr::Binary {
            left: Box::new(Expr::Binary {
                left: Box::new(col_a.clone()),
                op: BinaryOp::And,
                right: Box::new(col_b.clone()),
                data_type: DataType::Bool,
            }),
            op: BinaryOp::And,
            right: Box::new(div_eq),
            data_type: DataType::Bool,
        };
        let and_out = eval_expr(&and_chain, &batch, &ctx(), None).unwrap();
        let and_vals: Vec<Option<bool>> = (0..5).map(|r| read_bool(&and_out, r)).collect();
        assert_eq!(
            and_vals,
            vec![Some(false), None, Some(false), Some(false), Some(false)]
        );

        // OR chain: NULL OR true -> true (overrides earlier NULL)
        let or_chain = Expr::Binary {
            left: Box::new(col_a),
            op: BinaryOp::Or,
            right: Box::new(col_b),
            data_type: DataType::Bool,
        };
        let or_out = eval_expr(&or_chain, &batch, &ctx(), None).unwrap();
        let or_vals: Vec<Option<bool>> = (0..5).map(|r| read_bool(&or_out, r)).collect();
        assert_eq!(
            or_vals,
            vec![None, Some(true), None, Some(true), Some(false)]
        );

        // Deep 2000-op left-spine Add under tight pool does not overflow stack or accumulate pool.
        let mut deep_add = Expr::Column {
            index: 2,
            name: "x".into(),
            data_type: DataType::I64,
        };
        for _ in 0..2_000 {
            deep_add = Expr::Binary {
                left: Box::new(deep_add),
                op: BinaryOp::Add,
                right: Box::new(Expr::Literal(ScalarValue::I64(1))),
                data_type: DataType::I64,
            };
        }
        let tight_pool = GlobalPool::standalone(4096);
        let mut res = tight_pool.register("deep_spine", false);
        let sum_col = eval_expr(&deep_add, &batch, &ctx(), Some(&mut res)).unwrap();
        assert_eq!(read_i64(&sum_col, 0), Some(2_000));
        assert_eq!(read_i64(&sum_col, 1), Some(2_001));
        assert_eq!(res.size(), sum_col.memory_bytes());
        drop(sum_col);
        res.free();

        // Deep 2000-op OR chain under tight pool does not overflow stack or accumulate pool.
        let mut deep_or = Expr::Literal(ScalarValue::Bool(false));
        for _ in 0..1_999 {
            deep_or = Expr::Binary {
                left: Box::new(deep_or),
                op: BinaryOp::Or,
                right: Box::new(Expr::Literal(ScalarValue::Bool(false))),
                data_type: DataType::Bool,
            };
        }
        deep_or = Expr::Binary {
            left: Box::new(deep_or),
            op: BinaryOp::Or,
            right: Box::new(Expr::Literal(ScalarValue::Bool(true))),
            data_type: DataType::Bool,
        };
        let or_deep_col = eval_expr(&deep_or, &batch, &ctx(), Some(&mut res)).unwrap();
        assert_eq!(read_bool(&or_deep_col, 0), Some(true));
        assert_eq!(res.size(), or_deep_col.memory_bytes());
    }
}
