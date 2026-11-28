//! Spillable external sort (`ExternalSort`) and window function evaluator (`eval_window_group`).

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::fmt;
use std::mem::size_of;
use std::sync::Arc;

use cetra_column::{
    Batch, Bitmap, CancelToken, Column, ColumnError, DataType, Field, FlatColumn, FlatData,
    Reservation, ScalarValue, Schema, ScopedPool, Selection,
};

use crate::error::{InvalidQuery, QueryError};
use crate::eval::{
    self, ElemRef, check_cancel, eval_expr, pool_size, read_scalar, reconcile_output, rollback_to,
};
use crate::ir::{EvalContext, Expr};

use super::pipeline::{ControlFlow, ExecContext, PushSink, transfer_or_grow};
use super::sort_key::{SortKey, compare_elem_refs, compare_rows, compare_rows_across};
use super::source::PartitionStream;
use super::spill::{MAX_MERGE_WAY, SpillFile, SpillReader, SpillWriter};

const CANCEL_CHECK_MASK: usize = 1023;
const MAX_STREAMING_WINDOW_LAG: usize = 4096;

fn effective_merge_way(pool: &ScopedPool, max_merge_way: usize) -> usize {
    (pool.spill_share() / 4096)
        .saturating_sub(2)
        .clamp(2, max_merge_way.max(2))
}

/// Computes the target spill frame size in bytes so `max_merge_way` active merge batches
/// plus output/scratch batches fit inside a tight `ScopedPool`.
fn target_frame_bytes(pool: &ScopedPool, max_merge_way: usize) -> usize {
    (pool.spill_share() / (max_merge_way + 2)).max(4096)
}

/// Validates and extracts a `Column` for each `SortKey` from `batch`, evaluating only
/// non-`Expr::Column` expressions (tracked in `res`).
fn eval_extra_sort_keys(
    keys: &[SortKey],
    batch: &Batch,
    eval_ctx: &EvalContext,
    res: &mut Reservation,
) -> Result<Vec<Option<Arc<Column>>>, QueryError> {
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        match &key.expr {
            Expr::Column {
                index, data_type, ..
            } => {
                let col = batch.column(*index).ok_or(ColumnError::InvalidBatch {
                    reason: "sort key column index out of bounds",
                })?;
                if col.data_type() != *data_type {
                    return Err(ColumnError::InvalidBatch {
                        reason: "sort key column data type mismatch",
                    }
                    .into());
                }
                out.push(None);
            }
            expr => {
                let col = eval_expr(expr, batch, eval_ctx, Some(res))?;
                out.push(Some(col));
            }
        }
    }
    Ok(out)
}

/// Resolves `&Column` references for `keys` against `batch` and its `extra_keys`.
fn resolve_key_col_refs<'a>(
    keys: &[SortKey],
    batch: &'a Batch,
    extra_keys: &'a [Option<Arc<Column>>],
) -> Result<Vec<&'a Column>, QueryError> {
    let mut refs = Vec::with_capacity(keys.len());
    for (idx, key) in keys.iter().enumerate() {
        if let Some(Some(col)) = extra_keys.get(idx) {
            refs.push(col.as_ref());
        } else if let Expr::Column { index, .. } = &key.expr {
            let col = batch.column(*index).ok_or(ColumnError::InvalidBatch {
                reason: "sort key column index out of bounds",
            })?;
            refs.push(col);
        } else {
            return Err(QueryError::internal("missing evaluated sort key column"));
        }
    }
    Ok(refs)
}

/// Compares row `a` in `cols_a` against row `b` in `cols_b` without cloning `Column` values.
fn compare_key_refs_across(
    cols_a: &[&Column],
    a: usize,
    cols_b: &[&Column],
    b: usize,
    keys: &[SortKey],
) -> Ordering {
    for (idx, key) in keys.iter().enumerate() {
        let (Some(&col_a), Some(&col_b)) = (cols_a.get(idx), cols_b.get(idx)) else {
            continue;
        };
        let ord = compare_rows_across(
            std::slice::from_ref(col_a),
            a,
            std::slice::from_ref(col_b),
            b,
            std::slice::from_ref(key),
        );
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Order-preserving `u64` encoding of `f64` matching `eval::f64_cmp`:
/// `-inf < negative < -0.0 == +0.0 < positive < +inf < NaN == NaN`.
#[inline]
fn f64_to_sortable_u64(v: f64) -> u64 {
    if v.is_nan() {
        return u64::MAX;
    }
    let norm = if v == 0.0 { 0.0f64 } else { v };
    let bits = norm.to_bits();
    if (bits >> 63) != 0 {
        !bits
    } else {
        bits ^ (1u64 << 63)
    }
}

#[derive(Clone, Copy)]
enum FlatKeySpec {
    Fixed16 {
        descending: bool,
        nulls_first: bool,
    },
    /// Covers `DataType::I64` and `DataType::TimestampNs { .. }`.
    I64 {
        descending: bool,
        nulls_first: bool,
    },
    F64 {
        descending: bool,
        nulls_first: bool,
    },
    Utf8 {
        descending: bool,
        nulls_first: bool,
    },
}

impl FlatKeySpec {
    #[inline]
    fn flags(self) -> (bool, bool) {
        match self {
            Self::Fixed16 {
                descending,
                nulls_first,
            }
            | Self::I64 {
                descending,
                nulls_first,
            }
            | Self::F64 {
                descending,
                nulls_first,
            }
            | Self::Utf8 {
                descending,
                nulls_first,
            } => (descending, nulls_first),
        }
    }
}

fn classify_flat_key(key: &SortKey, dt: &DataType) -> Option<FlatKeySpec> {
    match dt {
        DataType::FixedBytes16 => Some(FlatKeySpec::Fixed16 {
            descending: key.descending,
            nulls_first: key.nulls_first,
        }),
        DataType::I64 | DataType::TimestampNs { .. } => Some(FlatKeySpec::I64 {
            descending: key.descending,
            nulls_first: key.nulls_first,
        }),
        DataType::F64 => Some(FlatKeySpec::F64 {
            descending: key.descending,
            nulls_first: key.nulls_first,
        }),
        DataType::Utf8 => Some(FlatKeySpec::Utf8 {
            descending: key.descending,
            nulls_first: key.nulls_first,
        }),
        _ => None,
    }
}

#[inline]
fn cmp_opt_val<T: Ord>(
    a: Option<T>,
    b: Option<T>,
    descending: bool,
    nulls_first: bool,
) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => {
            if nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Some(_), None) => {
            if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Some(va), Some(vb)) => {
            if descending {
                vb.cmp(&va)
            } else {
                va.cmp(&vb)
            }
        }
    }
}

#[inline]
fn extract_fixed16(col: &Column, r: usize) -> Option<[u8; 16]> {
    if let Column::Flat(fc) = col {
        if fc.nulls().is_some_and(|bm| !bm.get(r)) {
            return None;
        }
        return match fc.data() {
            FlatData::FixedBytes16(v) => v.get(r).copied(),
            _ => None,
        };
    }
    eval::read_bytes(col, r).and_then(|b| <[u8; 16]>::try_from(b).ok())
}

#[inline]
fn extract_fixed16_non_null(col: &Column, r: usize) -> Option<[u8; 16]> {
    if let Column::Flat(fc) = col {
        return match fc.data() {
            FlatData::FixedBytes16(v) => v.get(r).copied(),
            _ => None,
        };
    }
    eval::read_bytes(col, r).and_then(|b| <[u8; 16]>::try_from(b).ok())
}

#[inline]
fn extract_i64(col: &Column, r: usize) -> Option<i64> {
    if let Column::Flat(fc) = col {
        if fc.nulls().is_some_and(|bm| !bm.get(r)) {
            return None;
        }
        return match fc.data() {
            FlatData::I64(v) | FlatData::TimestampNs { values: v, .. } => v.get(r).copied(),
            _ => None,
        };
    }
    match col.data_type() {
        DataType::TimestampNs { .. } => eval::read_timestamp(col, r),
        _ => eval::read_i64(col, r),
    }
}

#[inline]
fn extract_i64_non_null(col: &Column, r: usize) -> Option<i64> {
    if let Column::Flat(fc) = col {
        return match fc.data() {
            FlatData::I64(v) | FlatData::TimestampNs { values: v, .. } => v.get(r).copied(),
            _ => None,
        };
    }
    match col.data_type() {
        DataType::TimestampNs { .. } => eval::read_timestamp(col, r),
        _ => eval::read_i64(col, r),
    }
}

#[inline]
fn extract_f64_key(col: &Column, r: usize) -> Option<u64> {
    let v = if let Column::Flat(fc) = col {
        if fc.nulls().is_some_and(|bm| !bm.get(r)) {
            return None;
        }
        match fc.data() {
            FlatData::F64(vals) => vals.get(r).copied(),
            _ => None,
        }
    } else {
        eval::read_f64(col, r)
    }?;
    Some(f64_to_sortable_u64(v))
}

#[inline]
fn extract_f64_key_non_null(col: &Column, r: usize) -> Option<u64> {
    let v = if let Column::Flat(fc) = col {
        match fc.data() {
            FlatData::F64(vals) => vals.get(r).copied(),
            _ => None,
        }
    } else {
        eval::read_f64(col, r)
    }?;
    Some(f64_to_sortable_u64(v))
}

#[inline]
fn extract_utf8(col: &Column, r: usize) -> Option<&[u8]> {
    if let Column::Flat(fc) = col {
        if fc.nulls().is_some_and(|bm| !bm.get(r)) {
            return None;
        }
        return match fc.data() {
            FlatData::Utf8(u) => u.get_bytes(r),
            _ => None,
        };
    }
    eval::read_str_bytes(col, r)
}

#[inline]
fn extract_utf8_non_null(col: &Column, r: usize) -> Option<&[u8]> {
    if let Column::Flat(fc) = col {
        return match fc.data() {
            FlatData::Utf8(u) => u.get_bytes(r),
            _ => None,
        };
    }
    eval::read_str_bytes(col, r)
}

#[inline]
fn permute_refs_from_positions<I: Iterator<Item = u32>>(refs: &mut Vec<(u32, u32)>, positions: I) {
    let orig = std::mem::take(refs);
    refs.reserve(orig.len());
    for pos in positions {
        refs.push(orig[pos as usize]);
    }
}

fn collect_1key_non_null<'a, K: 'a, F>(
    key_refs: &[Vec<&'a Column>],
    refs: &[(u32, u32)],
    extract: F,
) -> Option<Vec<(K, u32)>>
where
    F: Fn(&'a Column, usize) -> Option<K>,
{
    let mut entries = Vec::with_capacity(refs.len());
    for (pos, &(ba, ra)) in refs.iter().enumerate() {
        let col = key_refs.get(ba as usize)?.first().copied()?;
        entries.push((extract(col, ra as usize)?, pos as u32));
    }
    Some(entries)
}

fn collect_1key_nullable<'a, K: 'a, F>(
    key_refs: &[Vec<&'a Column>],
    refs: &[(u32, u32)],
    extract: F,
) -> Option<Vec<(Option<K>, u32)>>
where
    F: Fn(&'a Column, usize) -> Option<K>,
{
    let mut entries = Vec::with_capacity(refs.len());
    for (pos, &(ba, ra)) in refs.iter().enumerate() {
        let col = key_refs.get(ba as usize)?.first().copied()?;
        entries.push((extract(col, ra as usize), pos as u32));
    }
    Some(entries)
}

fn sort_1key_non_null<K: Ord>(
    mut entries: Vec<(K, u32)>,
    descending: bool,
    refs: &mut Vec<(u32, u32)>,
) {
    if descending {
        entries.sort_unstable_by(|(ka, pa), (kb, pb)| kb.cmp(ka).then_with(|| pa.cmp(pb)));
    } else {
        entries.sort_unstable_by(|(ka, pa), (kb, pb)| ka.cmp(kb).then_with(|| pa.cmp(pb)));
    }
    permute_refs_from_positions(refs, entries.into_iter().map(|(_, pos)| pos));
}

fn sort_1key_nullable<K: Ord>(
    mut entries: Vec<(Option<K>, u32)>,
    descending: bool,
    nulls_first: bool,
    refs: &mut Vec<(u32, u32)>,
) {
    entries.sort_unstable_by(|(ka, pa), (kb, pb)| {
        cmp_opt_val(ka.as_ref(), kb.as_ref(), descending, nulls_first).then_with(|| pa.cmp(pb))
    });
    permute_refs_from_positions(refs, entries.into_iter().map(|(_, pos)| pos));
}

fn collect_2key_non_null<'a, K1: 'a, K2: 'a, F1, F2>(
    key_refs: &[Vec<&'a Column>],
    refs: &[(u32, u32)],
    extract1: F1,
    extract2: F2,
) -> Option<Vec<(K1, K2, u32)>>
where
    F1: Fn(&'a Column, usize) -> Option<K1>,
    F2: Fn(&'a Column, usize) -> Option<K2>,
{
    let mut entries = Vec::with_capacity(refs.len());
    for (pos, &(ba, ra)) in refs.iter().enumerate() {
        let cols = key_refs.get(ba as usize)?;
        let c1 = cols.first().copied()?;
        let c2 = cols.get(1).copied()?;
        let r = ra as usize;
        entries.push((extract1(c1, r)?, extract2(c2, r)?, pos as u32));
    }
    Some(entries)
}

type Nullable2KeyVec<K1, K2> = Vec<(Option<K1>, Option<K2>, u32)>;

fn collect_2key_nullable<'a, K1: 'a, K2: 'a, F1, F2>(
    key_refs: &[Vec<&'a Column>],
    refs: &[(u32, u32)],
    extract1: F1,
    extract2: F2,
) -> Option<Nullable2KeyVec<K1, K2>>
where
    F1: Fn(&'a Column, usize) -> Option<K1>,
    F2: Fn(&'a Column, usize) -> Option<K2>,
{
    let mut entries = Vec::with_capacity(refs.len());
    for (pos, &(ba, ra)) in refs.iter().enumerate() {
        let cols = key_refs.get(ba as usize)?;
        let c1 = cols.first().copied()?;
        let c2 = cols.get(1).copied()?;
        let r = ra as usize;
        entries.push((extract1(c1, r), extract2(c2, r), pos as u32));
    }
    Some(entries)
}

fn sort_2key_non_null<K1: Ord, K2: Ord>(
    mut entries: Vec<(K1, K2, u32)>,
    desc1: bool,
    desc2: bool,
    refs: &mut Vec<(u32, u32)>,
) {
    entries.sort_unstable_by(|(a1, a2, pa), (b1, b2, pb)| {
        let o1 = if desc1 { b1.cmp(a1) } else { a1.cmp(b1) };
        if o1 != Ordering::Equal {
            return o1;
        }
        let o2 = if desc2 { b2.cmp(a2) } else { a2.cmp(b2) };
        o2.then_with(|| pa.cmp(pb))
    });
    permute_refs_from_positions(refs, entries.into_iter().map(|(_, _, pos)| pos));
}

fn sort_2key_nullable<K1: Ord, K2: Ord>(
    mut entries: Nullable2KeyVec<K1, K2>,
    k1: (bool, bool),
    k2: (bool, bool),
    refs: &mut Vec<(u32, u32)>,
) {
    let (desc1, nf1) = k1;
    let (desc2, nf2) = k2;
    entries.sort_unstable_by(|(a1, a2, pa), (b1, b2, pb)| {
        let o1 = cmp_opt_val(a1.as_ref(), b1.as_ref(), desc1, nf1);
        if o1 != Ordering::Equal {
            return o1;
        }
        cmp_opt_val(a2.as_ref(), b2.as_ref(), desc2, nf2).then_with(|| pa.cmp(pb))
    });
    permute_refs_from_positions(refs, entries.into_iter().map(|(_, _, pos)| pos));
}

fn try_sort_1key(key_refs: &[Vec<&Column>], key: &SortKey, refs: &mut Vec<(u32, u32)>) -> bool {
    let Some(dt) = key_refs
        .first()
        .and_then(|cols| cols.first())
        .map(|c| c.data_type())
    else {
        return false;
    };
    if !key_refs
        .iter()
        .all(|cols| !cols.is_empty() && cols[0].data_type() == dt)
    {
        return false;
    }
    let Some(spec) = classify_flat_key(key, &dt) else {
        return false;
    };
    let any_nulls = key_refs.iter().any(|cols| cols[0].has_nulls());
    match (spec, any_nulls) {
        (FlatKeySpec::Fixed16 { descending, .. }, false) => {
            if let Some(entries) = collect_1key_non_null(key_refs, refs, extract_fixed16_non_null) {
                sort_1key_non_null(entries, descending, refs);
                return true;
            }
        }
        (
            FlatKeySpec::Fixed16 {
                descending,
                nulls_first,
            },
            true,
        ) => {
            if let Some(entries) = collect_1key_nullable(key_refs, refs, extract_fixed16) {
                sort_1key_nullable(entries, descending, nulls_first, refs);
                return true;
            }
        }
        (FlatKeySpec::I64 { descending, .. }, false) => {
            if let Some(entries) = collect_1key_non_null(key_refs, refs, extract_i64_non_null) {
                sort_1key_non_null(entries, descending, refs);
                return true;
            }
        }
        (
            FlatKeySpec::I64 {
                descending,
                nulls_first,
            },
            true,
        ) => {
            if let Some(entries) = collect_1key_nullable(key_refs, refs, extract_i64) {
                sort_1key_nullable(entries, descending, nulls_first, refs);
                return true;
            }
        }
        (FlatKeySpec::F64 { descending, .. }, false) => {
            if let Some(entries) = collect_1key_non_null(key_refs, refs, extract_f64_key_non_null) {
                sort_1key_non_null(entries, descending, refs);
                return true;
            }
        }
        (
            FlatKeySpec::F64 {
                descending,
                nulls_first,
            },
            true,
        ) => {
            if let Some(entries) = collect_1key_nullable(key_refs, refs, extract_f64_key) {
                sort_1key_nullable(entries, descending, nulls_first, refs);
                return true;
            }
        }
        (FlatKeySpec::Utf8 { descending, .. }, false) => {
            if let Some(entries) = collect_1key_non_null(key_refs, refs, extract_utf8_non_null) {
                sort_1key_non_null(entries, descending, refs);
                return true;
            }
        }
        (
            FlatKeySpec::Utf8 {
                descending,
                nulls_first,
            },
            true,
        ) => {
            if let Some(entries) = collect_1key_nullable(key_refs, refs, extract_utf8) {
                sort_1key_nullable(entries, descending, nulls_first, refs);
                return true;
            }
        }
    }
    false
}

fn try_sort_2key_typed<'a, K1, K2, F1N, F1C, F2N, F2C>(
    key_refs: &[Vec<&'a Column>],
    refs: &mut Vec<(u32, u32)>,
    both_non_null: bool,
    flags: ((bool, bool), (bool, bool)),
    ext1: (F1N, F1C),
    ext2: (F2N, F2C),
) -> bool
where
    K1: Ord + 'a,
    K2: Ord + 'a,
    F1N: Fn(&'a Column, usize) -> Option<K1>,
    F1C: Fn(&'a Column, usize) -> Option<K1>,
    F2N: Fn(&'a Column, usize) -> Option<K2>,
    F2C: Fn(&'a Column, usize) -> Option<K2>,
{
    let ((d1, nf1), (d2, nf2)) = flags;
    if both_non_null {
        if let Some(entries) = collect_2key_non_null(key_refs, refs, ext1.0, ext2.0) {
            sort_2key_non_null(entries, d1, d2, refs);
            return true;
        }
        return false;
    }
    if let Some(entries) = collect_2key_nullable(key_refs, refs, ext1.1, ext2.1) {
        sort_2key_nullable(entries, (d1, nf1), (d2, nf2), refs);
        return true;
    }
    false
}

fn try_sort_2key(key_refs: &[Vec<&Column>], keys: &[SortKey], refs: &mut Vec<(u32, u32)>) -> bool {
    let Some(first) = key_refs.first() else {
        return false;
    };
    if first.len() < 2 {
        return false;
    }
    let dt0 = first[0].data_type();
    let dt1 = first[1].data_type();
    if !key_refs
        .iter()
        .all(|cols| cols.len() >= 2 && cols[0].data_type() == dt0 && cols[1].data_type() == dt1)
    {
        return false;
    }
    let (Some(spec0), Some(spec1)) = (
        classify_flat_key(&keys[0], &dt0),
        classify_flat_key(&keys[1], &dt1),
    ) else {
        return false;
    };
    let both_non_null = !key_refs
        .iter()
        .any(|cols| cols[0].has_nulls() || cols[1].has_nulls());
    let flags = (spec0.flags(), spec1.flags());
    match (spec0, spec1) {
        (FlatKeySpec::Utf8 { .. }, FlatKeySpec::I64 { .. }) => try_sort_2key_typed(
            key_refs,
            refs,
            both_non_null,
            flags,
            (extract_utf8_non_null, extract_utf8),
            (extract_i64_non_null, extract_i64),
        ),
        (FlatKeySpec::I64 { .. }, FlatKeySpec::Fixed16 { .. }) => try_sort_2key_typed(
            key_refs,
            refs,
            both_non_null,
            flags,
            (extract_i64_non_null, extract_i64),
            (extract_fixed16_non_null, extract_fixed16),
        ),
        (FlatKeySpec::Fixed16 { .. }, FlatKeySpec::I64 { .. }) => try_sort_2key_typed(
            key_refs,
            refs,
            both_non_null,
            flags,
            (extract_fixed16_non_null, extract_fixed16),
            (extract_i64_non_null, extract_i64),
        ),
        (FlatKeySpec::I64 { .. }, FlatKeySpec::I64 { .. }) => try_sort_2key_typed(
            key_refs,
            refs,
            both_non_null,
            flags,
            (extract_i64_non_null, extract_i64),
            (extract_i64_non_null, extract_i64),
        ),
        (FlatKeySpec::Utf8 { .. }, FlatKeySpec::Utf8 { .. }) => try_sort_2key_typed(
            key_refs,
            refs,
            both_non_null,
            flags,
            (extract_utf8_non_null, extract_utf8),
            (extract_utf8_non_null, extract_utf8),
        ),
        _ => false,
    }
}

/// Sorts `(batch_idx, row_idx)` references using typed key materialization for common
/// 1-key and 2-key sorts while preserving exact stability across all `Selection` variants
/// and falling back to generic dynamic comparison for other key counts/types.
fn sort_row_refs_specialized(
    key_refs: &[Vec<&Column>],
    keys: &[SortKey],
    refs: &mut Vec<(u32, u32)>,
) {
    if refs.len() <= 1 || keys.is_empty() {
        return;
    }
    if refs.len() <= u32::MAX as usize {
        let handled = match keys.len() {
            1 => try_sort_1key(key_refs, &keys[0], refs),
            2 => try_sort_2key(key_refs, keys, refs),
            _ => false,
        };
        if handled {
            return;
        }
    }
    refs.sort_by(|&(ba, ra), &(bb, rb)| {
        compare_key_refs_across(
            &key_refs[ba as usize],
            ra as usize,
            &key_refs[bb as usize],
            rb as usize,
            keys,
        )
    });
}

/// Gathers `(cursor_or_batch_idx, row_idx)` rows from `batches` into a compacted `Batch`.
fn build_batch_from_refs(
    schema: &Arc<Schema>,
    batches: &[&Batch],
    refs: &[(u32, u32)],
    mut res: Option<&mut Reservation>,
    eval_ctx: &EvalContext,
) -> Result<Batch, QueryError> {
    let len = refs.len();
    if len == 0 {
        return Ok(Batch::empty(Arc::clone(schema)));
    }
    let batch_before = pool_size(&res);
    let mut cols = Vec::with_capacity(schema.len());
    let mut sources: Vec<Option<(&Column, usize)>> = Vec::with_capacity(len);

    for (col_idx, field) in schema.fields().iter().enumerate() {
        sources.clear();
        for &(b_idx, r_idx) in refs {
            let col = batches
                .get(b_idx as usize)
                .and_then(|b| b.column(col_idx))
                .ok_or(ColumnError::InvalidBatch {
                    reason: "column index out of bounds in sort gather",
                })?;
            let row = r_idx as usize;
            if col.is_null(row) {
                sources.push(None);
            } else {
                sources.push(Some((col, row)));
            }
        }
        let col_before = pool_size(&res);
        let built = match eval::build_flat_from_sources(
            &field.data_type,
            &sources,
            &mut res,
            eval_ctx,
            col_before,
        ) {
            Ok(c) => {
                if let Err(err) = reconcile_output(&mut res, col_before, c.memory_bytes()) {
                    rollback_to(&mut res, batch_before);
                    return Err(err);
                }
                c
            }
            Err(err) => {
                rollback_to(&mut res, batch_before);
                return Err(err);
            }
        };
        cols.push(built);
    }

    match Batch::try_new_all(Arc::clone(schema), cols, len) {
        Ok(batch) => {
            if let Err(err) = reconcile_output(&mut res, batch_before, batch.memory_bytes()) {
                rollback_to(&mut res, batch_before);
                return Err(err);
            }
            Ok(batch)
        }
        Err(err) => {
            rollback_to(&mut res, batch_before);
            Err(err.into())
        }
    }
}

/// Writes `batch` to `writer` sliced into frames of at most `frame_bytes` bytes
/// (at least 1 row per frame).
fn write_batch_framed(
    writer: &mut SpillWriter,
    batch: &Batch,
    frame_bytes: usize,
) -> Result<(), QueryError> {
    if batch.is_empty() {
        return Ok(());
    }
    let mut stack = vec![batch.clone()];
    while let Some(cur) = stack.pop() {
        let rows = cur.num_rows();
        if rows == 0 {
            continue;
        }
        if cur.memory_bytes() <= frame_bytes || rows <= 1 {
            writer.write_batch(&cur)?;
        } else {
            let mid = rows / 2;
            let right = cur.slice(mid, rows - mid, None)?;
            let left = cur.slice(0, mid, None)?;
            stack.push(right);
            stack.push(left);
        }
    }
    Ok(())
}

struct BufferedBatch {
    batch: Batch,
    extra_keys: Vec<Option<Arc<Column>>>,
}

/// Spillable external sort sink implementing [`PushSink`] and bounded-FD multi-pass k-way merge.
pub struct ExternalSort {
    schema: Arc<Schema>,
    keys: Vec<SortKey>,
    limit: Option<usize>,
    max_merge_way: usize,
    ctx: ExecContext,
    buffered: Vec<BufferedBatch>,
    buffered_rows: usize,
    max_batch_bytes: usize,
    runs: Vec<SpillFile>,
    res: Option<Reservation>,
}

impl fmt::Debug for ExternalSort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExternalSort")
            .field("columns", &self.schema.len())
            .field("keys", &self.keys.len())
            .field("limit", &self.limit)
            .field("max_merge_way", &self.max_merge_way)
            .field("buffered_rows", &self.buffered_rows)
            .field("max_batch_bytes", &self.max_batch_bytes)
            .field("runs", &self.runs.len())
            .field("reserved_bytes", &self.res_size())
            .finish()
    }
}

impl ExternalSort {
    /// Creates an external sort sink for `schema` sorted by `keys` with optional `limit`.
    pub fn new(
        schema: Arc<Schema>,
        keys: Vec<SortKey>,
        limit: Option<usize>,
        ctx: ExecContext,
    ) -> Self {
        let res = Some(ctx.pool.register("external_sort", true));
        Self {
            schema,
            keys,
            limit,
            max_merge_way: MAX_MERGE_WAY,
            ctx,
            buffered: Vec::new(),
            buffered_rows: 0,
            max_batch_bytes: 0,
            runs: Vec::new(),
            res,
        }
    }

    /// Sets the maximum k-way merge fan-in (clamped to `>= 2`; default [`MAX_MERGE_WAY`]).
    pub fn with_max_merge_way(mut self, max_merge_way: usize) -> Self {
        self.max_merge_way = max_merge_way.max(2);
        self
    }

    /// Number of spilled runs currently held on disk (diagnostics/testing).
    #[cfg(test)]
    pub(crate) fn spilled_run_count(&self) -> usize {
        self.runs.len()
    }

    fn res_mut(&mut self) -> &mut Reservation {
        let pool = &self.ctx.pool;
        self.res
            .get_or_insert_with(|| pool.register("external_sort", true))
    }

    fn res_size(&self) -> usize {
        self.res.as_ref().map_or(0, Reservation::size)
    }

    fn drop_res(&mut self) {
        self.res = None;
    }

    fn try_buffer_batch(
        &mut self,
        batch: &Batch,
        batch_res: &mut Reservation,
    ) -> Result<(), QueryError> {
        let res = self
            .res
            .get_or_insert_with(|| self.ctx.pool.register("external_sort", true));
        let before = res.size();
        let batch_res_before = batch_res.size();
        let rollback = |res: &mut Reservation, batch_res: &mut Reservation| {
            let transferred = batch_res_before.saturating_sub(batch_res.size());
            let target = before.saturating_add(transferred);
            if res.size() > target {
                res.shrink(res.size() - target);
            }
            if transferred > 0 {
                let _ = res.transfer_to(batch_res, transferred);
            }
            if res.size() > before {
                res.shrink(res.size() - before);
            }
        };
        let compacted = if batch.selection().is_all(batch.raw_len()) {
            let mem = batch.memory_bytes();
            if let Err(err) = transfer_or_grow(batch_res, res, mem) {
                rollback(res, batch_res);
                return Err(err.into());
            }
            match batch.clone().into_with_selection(Selection::All) {
                Ok(b) => b,
                Err(err) => {
                    rollback(res, batch_res);
                    return Err(err.into());
                }
            }
        } else {
            match batch.compact(Some(res)) {
                Ok(b) => b,
                Err(err) => {
                    rollback(res, batch_res);
                    return Err(err.into());
                }
            }
        };

        let extra_keys = match eval_extra_sort_keys(&self.keys, &compacted, &self.ctx.eval, res) {
            Ok(ek) => ek,
            Err(err) => {
                rollback(res, batch_res);
                return Err(err);
            }
        };

        let rows = compacted.num_rows();
        self.buffered.push(BufferedBatch {
            batch: compacted,
            extra_keys,
        });
        self.buffered_rows = self.buffered_rows.saturating_add(rows);
        Ok(())
    }

    fn sort_buffered_refs(&self) -> Result<Vec<(u32, u32)>, QueryError> {
        let mut refs = Vec::with_capacity(self.buffered_rows);
        for (b_idx, bb) in self.buffered.iter().enumerate() {
            let b_u32 = u32::try_from(b_idx).map_err(|_| ColumnError::Overflow {
                reason: "buffered batch count exceeds u32::MAX",
            })?;
            for r_idx in 0..bb.batch.num_rows() {
                let r_u32 = u32::try_from(r_idx).map_err(|_| ColumnError::Overflow {
                    reason: "batch row index exceeds u32::MAX",
                })?;
                refs.push((b_u32, r_u32));
            }
        }

        let key_refs: Vec<Vec<&Column>> = self
            .buffered
            .iter()
            .map(|bb| resolve_key_col_refs(&self.keys, &bb.batch, &bb.extra_keys))
            .collect::<Result<Vec<_>, _>>()?;

        sort_row_refs_specialized(&key_refs, &self.keys, &mut refs);

        if let Some(limit) = self.limit {
            refs.truncate(limit);
        }
        Ok(refs)
    }

    fn spill_buffered(&mut self) -> Result<(), QueryError> {
        if self.buffered_rows == 0 {
            self.buffered.clear();
            if let Some(res) = &mut self.res {
                res.free();
            }
            return Ok(());
        }

        let refs = self.sort_buffered_refs()?;
        if refs.is_empty() {
            self.buffered.clear();
            self.buffered_rows = 0;
            if let Some(res) = &mut self.res {
                res.free();
            }
            return Ok(());
        }

        let merge_way = effective_merge_way(&self.ctx.pool, self.max_merge_way);
        let frame_bytes = target_frame_bytes(&self.ctx.pool, merge_way);
        let total_mem: usize = self.buffered.iter().map(|b| b.batch.memory_bytes()).sum();
        let avg_row = (total_mem / self.buffered_rows.max(1)).max(1);
        let rows_per_chunk = (frame_bytes / avg_row)
            .max(1)
            .min(self.ctx.batch_size.max(1));

        let batch_refs: Vec<&Batch> = self.buffered.iter().map(|b| &b.batch).collect();
        let mut writer = self.ctx.spill.create_file()?;

        for chunk in refs.chunks(rows_per_chunk) {
            if self.ctx.eval.cancel.is_cancelled() {
                return Err(QueryError::Cancelled);
            }
            let out_batch =
                build_batch_from_refs(&self.schema, &batch_refs, chunk, None, &self.ctx.eval)?;
            write_batch_framed(&mut writer, &out_batch, frame_bytes)?;
        }

        let spill_file = writer.finish()?;
        self.runs.push(spill_file);
        self.buffered.clear();
        self.buffered_rows = 0;
        self.res_mut().free();
        Ok(())
    }

    fn spill_single_batch(
        &mut self,
        batch: &Batch,
        _batch_res: &mut Reservation,
    ) -> Result<(), QueryError> {
        if let Some(res) = &mut self.res {
            res.free();
        }
        let active_rows = batch.num_rows();
        if active_rows == 0 {
            return Ok(());
        }

        let mut key_res = self.ctx.pool.register("external_sort_single_keys", false);
        let extra_keys = eval_extra_sort_keys(&self.keys, batch, &self.ctx.eval, &mut key_res)?;

        let mut refs: Vec<(u32, u32)> = Vec::with_capacity(active_rows);
        match batch.selection() {
            Selection::All => {
                for r_idx in 0..batch.raw_len() {
                    let r_u32 = u32::try_from(r_idx).map_err(|_| ColumnError::Overflow {
                        reason: "batch row index exceeds u32::MAX",
                    })?;
                    refs.push((0, r_u32));
                }
            }
            Selection::Mask(mask) => {
                for r_idx in mask.iter_ones() {
                    let r_u32 = u32::try_from(r_idx).map_err(|_| ColumnError::Overflow {
                        reason: "batch row index exceeds u32::MAX",
                    })?;
                    refs.push((0, r_u32));
                }
            }
            Selection::Indices(indices) => {
                for &r_u32 in indices {
                    refs.push((0, r_u32));
                }
            }
        }

        {
            let key_refs = resolve_key_col_refs(&self.keys, batch, &extra_keys)?;
            sort_row_refs_specialized(std::slice::from_ref(&key_refs), &self.keys, &mut refs);
        }
        if let Some(limit) = self.limit {
            refs.truncate(limit);
        }
        drop(extra_keys);
        key_res.free();
        drop(key_res);

        if refs.is_empty() {
            return Ok(());
        }

        let merge_way = effective_merge_way(&self.ctx.pool, self.max_merge_way);
        let frame_bytes = target_frame_bytes(&self.ctx.pool, merge_way);
        let avg_row = (batch.memory_bytes() / active_rows.max(1)).max(1);
        let rows_per_chunk = (frame_bytes / avg_row)
            .max(1)
            .min(self.ctx.batch_size.max(1));

        let batch_refs = [batch];
        let mut writer = self.ctx.spill.create_file()?;
        for chunk in refs.chunks(rows_per_chunk) {
            if self.ctx.eval.cancel.is_cancelled() {
                return Err(QueryError::Cancelled);
            }
            let out_batch =
                build_batch_from_refs(&self.schema, &batch_refs, chunk, None, &self.ctx.eval)?;
            write_batch_framed(&mut writer, &out_batch, frame_bytes)?;
        }

        let spill_file = writer.finish()?;
        self.runs.push(spill_file);
        Ok(())
    }

    /// Consumes this sort sink and produces a globally sorted [`PartitionStream`], performing
    /// bounded-FD multi-pass k-way merge when spilled runs exceed `max_merge_way`.
    pub fn into_stream(mut self) -> Result<Box<dyn PartitionStream>, QueryError> {
        if self.limit == Some(0) || (self.runs.is_empty() && self.buffered_rows == 0) {
            self.buffered.clear();
            self.buffered_rows = 0;
            self.runs.clear();
            self.drop_res();
            return Ok(Box::new(EmptyStream));
        }

        if self.runs.is_empty() {
            let refs = self.sort_buffered_refs()?;
            if refs.is_empty() {
                self.buffered.clear();
                self.buffered_rows = 0;
                self.drop_res();
                return Ok(Box::new(EmptyStream));
            }
            let merge_way = effective_merge_way(&self.ctx.pool, self.max_merge_way);
            let frame_bytes = target_frame_bytes(&self.ctx.pool, merge_way);
            let res = self
                .res
                .take()
                .unwrap_or_else(|| self.ctx.pool.register("external_sort", true));
            return Ok(Box::new(InMemorySortedStream {
                schema: self.schema,
                ctx: self.ctx,
                buffered: self.buffered,
                refs,
                cursor: 0,
                frame_bytes,
                res,
                fallback_reader: None,
                fallback_file: None,
            }));
        }

        if self.buffered_rows > 0 {
            self.spill_buffered()?;
        }
        self.drop_res();

        let merge_way = effective_merge_way(&self.ctx.pool, self.max_merge_way);
        merge_spill_passes(
            &mut self.runs,
            &self.schema,
            &self.keys,
            self.limit,
            merge_way,
            &self.ctx,
        )?;

        let frame_bytes = target_frame_bytes(&self.ctx.pool, merge_way);
        let max_output_rows = self.ctx.batch_size.max(1);
        MergeStream::from_spill_files(
            self.runs,
            self.schema,
            &self.keys,
            self.limit,
            frame_bytes,
            max_output_rows,
            self.ctx,
        )
    }

    /// Combines multiple per-partition [`ExternalSort`] sinks into one globally sorted [`PartitionStream`].
    pub fn merge_sinks(
        sinks: Vec<Self>,
        schema: Arc<Schema>,
        keys: Vec<SortKey>,
        limit: Option<usize>,
        ctx: &ExecContext,
    ) -> Result<Box<dyn PartitionStream>, QueryError> {
        for sink in &sinks {
            if sink.schema.as_ref() != schema.as_ref() {
                return Err(ColumnError::InvalidBatch {
                    reason: "ExternalSort sink schema does not match merge_sinks schema",
                }
                .into());
            }
            if sink.keys != keys {
                return Err(QueryError::Invalid(InvalidQuery::new(
                    "ExternalSort sink keys do not match merge_sinks keys",
                    None,
                )));
            }
        }
        if limit == Some(0) || sinks.is_empty() {
            return Ok(Box::new(EmptyStream));
        }

        let max_merge_way = sinks
            .iter()
            .map(|s| s.max_merge_way)
            .min()
            .unwrap_or(MAX_MERGE_WAY)
            .max(2);

        let mut non_empty: Vec<Self> = Vec::with_capacity(sinks.len());
        for mut sink in sinks {
            if sink.runs.is_empty() && sink.buffered_rows == 0 {
                sink.drop_res();
                continue;
            }
            non_empty.push(sink);
        }

        if non_empty.is_empty() {
            return Ok(Box::new(EmptyStream));
        }

        if non_empty.len() == 1 {
            let mut single = non_empty.remove(0);
            single.schema = schema;
            single.keys = keys;
            single.limit = match (single.limit, limit) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) => Some(a),
                (None, b) => b,
            };
            return single.into_stream();
        }

        let merge_way = effective_merge_way(&ctx.pool, max_merge_way);
        let frame_bytes = target_frame_bytes(&ctx.pool, merge_way);
        let any_spilled = non_empty.iter().any(|s| !s.runs.is_empty());
        let total_reserved: usize = non_empty.iter().map(Self::res_size).sum();
        let can_stay_in_memory =
            !any_spilled && non_empty.len() <= merge_way && total_reserved <= ctx.pool.cap() / 2;

        if can_stay_in_memory {
            let mut streams: Vec<Box<dyn PartitionStream>> = Vec::with_capacity(non_empty.len());
            for mut sink in non_empty {
                sink.schema = Arc::clone(&schema);
                sink.keys = keys.clone();
                sink.limit = match (sink.limit, limit) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (Some(a), None) => Some(a),
                    (None, b) => b,
                };
                streams.push(sink.into_stream()?);
            }
            return MergeStream::from_streams(
                streams,
                schema,
                &keys,
                limit,
                frame_bytes,
                ctx.batch_size.max(1),
                ctx.clone(),
            );
        }

        let mut all_runs = Vec::new();
        for mut sink in non_empty {
            sink.schema = Arc::clone(&schema);
            sink.keys = keys.clone();
            if let Some(lim) = limit {
                sink.limit = Some(sink.limit.map_or(lim, |l| l.min(lim)));
            }
            if sink.buffered_rows > 0 {
                sink.spill_buffered()?;
            }
            sink.drop_res();
            all_runs.append(&mut sink.runs);
        }

        let merge_way = effective_merge_way(&ctx.pool, max_merge_way);
        let frame_bytes = target_frame_bytes(&ctx.pool, merge_way);
        merge_spill_passes(&mut all_runs, &schema, &keys, limit, merge_way, ctx)?;

        MergeStream::from_spill_files(
            all_runs,
            schema,
            &keys,
            limit,
            frame_bytes,
            ctx.batch_size.max(1),
            ctx.clone(),
        )
    }
}

impl PushSink for ExternalSort {
    fn consume(
        &mut self,
        batch: Batch,
        batch_res: &mut Reservation,
    ) -> Result<ControlFlow, QueryError> {
        if self.limit == Some(0) {
            return Ok(ControlFlow::Stop);
        }
        if self.ctx.eval.cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        if batch.is_empty() {
            return Ok(ControlFlow::Continue);
        }
        if batch.schema().as_ref() != self.schema.as_ref() {
            return Err(ColumnError::InvalidBatch {
                reason: "batch schema does not match ExternalSort schema",
            }
            .into());
        }

        self.max_batch_bytes = self.max_batch_bytes.max(batch.memory_bytes());
        let buffered = match self.try_buffer_batch(&batch, batch_res) {
            Ok(()) => true,
            Err(QueryError::TooLarge) => {
                if self.buffered_rows > 0 {
                    self.spill_buffered()?;
                    match self.try_buffer_batch(&batch, batch_res) {
                        Ok(()) => true,
                        Err(QueryError::TooLarge)
                            if batch.memory_bytes() <= self.ctx.pool.cap() =>
                        {
                            self.spill_single_batch(&batch, batch_res)?;
                            false
                        }
                        Err(err) => return Err(err),
                    }
                } else if batch.memory_bytes() <= self.ctx.pool.cap() {
                    self.spill_single_batch(&batch, batch_res)?;
                    false
                } else {
                    return Err(QueryError::TooLarge);
                }
            }
            Err(err) => return Err(err),
        };
        if !buffered {
            return Ok(ControlFlow::Continue);
        }
        // Leave enough pool headroom after `consume` returns: an upstream `OpStream`
        // (source pull + operator scratch + output queue) can transiently hold ~3x one batch
        // across unspillable reservations before releasing them.
        let cap = self.ctx.pool.cap();
        if self.buffered_rows > 0
            && (self
                .res_size()
                .saturating_add(self.max_batch_bytes.saturating_mul(3))
                > cap
                || self
                    .ctx
                    .pool
                    .reserved()
                    .saturating_add(self.max_batch_bytes)
                    > cap)
        {
            self.spill_buffered()?;
        }
        Ok(ControlFlow::Continue)
    }

    fn finish(&mut self) -> Result<(), QueryError> {
        if self.buffered_rows == 0 {
            self.drop_res();
            return Ok(());
        }
        if !self.runs.is_empty() {
            self.spill_buffered()?;
            self.drop_res();
            return Ok(());
        }
        if self.ctx.target_partitions > 1 {
            let cap = self.ctx.pool.cap();
            let workers = self.ctx.workers.num_workers().max(1);
            let headroom = self
                .max_batch_bytes
                .saturating_mul(workers.saturating_add(1))
                .min(cap / 2);
            if self.ctx.pool.reserved().saturating_add(headroom) > cap {
                self.spill_buffered()?;
                self.drop_res();
            }
        }
        Ok(())
    }
}

struct EmptyStream;

impl PartitionStream for EmptyStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        _res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        if cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        Ok(None)
    }
}

struct InMemorySortedStream {
    schema: Arc<Schema>,
    ctx: ExecContext,
    buffered: Vec<BufferedBatch>,
    refs: Vec<(u32, u32)>,
    cursor: usize,
    frame_bytes: usize,
    res: Reservation,
    fallback_reader: Option<SpillReader>,
    fallback_file: Option<SpillFile>,
}

impl InMemorySortedStream {
    fn spill_remaining(&mut self, cancel: &CancelToken) -> Result<(), QueryError> {
        let mut writer = self.ctx.spill.create_file()?;
        let total_mem: usize = self.buffered.iter().map(|b| b.batch.memory_bytes()).sum();
        let total_rows: usize = self.buffered.iter().map(|b| b.batch.num_rows()).sum();
        let avg_row = (total_mem / total_rows.max(1)).max(1);
        let rows_per_chunk = (self.frame_bytes / avg_row)
            .max(1)
            .min(self.ctx.batch_size.max(1));
        let batch_refs: Vec<&Batch> = self.buffered.iter().map(|b| &b.batch).collect();
        let eval_ctx = self.ctx.eval.with_cancel(cancel.clone());

        for chunk in self.refs[self.cursor..].chunks(rows_per_chunk) {
            if cancel.is_cancelled() {
                return Err(QueryError::Cancelled);
            }
            let out_batch =
                build_batch_from_refs(&self.schema, &batch_refs, chunk, None, &eval_ctx)?;
            write_batch_framed(&mut writer, &out_batch, self.frame_bytes)?;
        }

        let spill_file = writer.finish()?;
        let reader = spill_file.open_reader(Arc::clone(&self.schema))?;
        self.buffered.clear();
        self.refs.clear();
        self.cursor = 0;
        self.res.free();
        self.fallback_reader = Some(reader);
        self.fallback_file = Some(spill_file);
        Ok(())
    }
}

impl PartitionStream for InMemorySortedStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = pool_size(&res);
        if cancel.is_cancelled() {
            rollback_to(&mut res, before);
            return Err(QueryError::Cancelled);
        }

        if let Some(reader) = &mut self.fallback_reader {
            let out = reader.next_batch(res.as_deref_mut())?;
            if out.is_none() {
                self.fallback_reader = None;
                self.fallback_file = None;
            }
            return Ok(out);
        }

        if self.cursor >= self.refs.len() {
            self.buffered.clear();
            self.refs.clear();
            self.res.free();
            return Ok(None);
        }

        let total_mem: usize = self.buffered.iter().map(|b| b.batch.memory_bytes()).sum();
        let total_rows: usize = self.buffered.iter().map(|b| b.batch.num_rows()).sum();
        let avg_row = (total_mem / total_rows.max(1)).max(1);
        let max_rows = (self.frame_bytes / avg_row)
            .max(1)
            .min(self.ctx.batch_size.max(1));
        let end = (self.cursor + max_rows).min(self.refs.len());
        let is_last = end == self.refs.len();

        let eval_ctx = self.ctx.eval.with_cancel(cancel.clone());

        let chunk_res = {
            let batch_refs: Vec<&Batch> = self.buffered.iter().map(|b| &b.batch).collect();
            let slice_refs = &self.refs[self.cursor..end];
            if is_last {
                // Build unreserved briefly, drop buffered, then reserve exact output bytes.
                let out =
                    build_batch_from_refs(&self.schema, &batch_refs, slice_refs, None, &eval_ctx);
                match out {
                    Ok(batch) => {
                        self.buffered.clear();
                        self.refs.clear();
                        self.cursor = 0;
                        self.res.free();
                        if let Err(err) = reconcile_output(&mut res, before, batch.memory_bytes()) {
                            rollback_to(&mut res, before);
                            return Err(err);
                        }
                        return Ok(Some(batch));
                    }
                    Err(err) => {
                        rollback_to(&mut res, before);
                        return Err(err);
                    }
                }
            }
            build_batch_from_refs(
                &self.schema,
                &batch_refs,
                slice_refs,
                res.as_deref_mut(),
                &eval_ctx,
            )
        };

        match chunk_res {
            Ok(batch) => {
                self.cursor = end;
                Ok(Some(batch))
            }
            Err(QueryError::TooLarge) if !self.buffered.is_empty() => {
                rollback_to(&mut res, before);
                self.spill_remaining(cancel)?;
                let Some(reader) = &mut self.fallback_reader else {
                    return Ok(None);
                };
                let out = reader.next_batch(res.as_deref_mut())?;
                if out.is_none() {
                    self.fallback_reader = None;
                    self.fallback_file = None;
                }
                Ok(out)
            }
            Err(err) => {
                rollback_to(&mut res, before);
                Err(err)
            }
        }
    }
}

enum CursorSource {
    Spill(SpillReader),
    Stream(Box<dyn PartitionStream>),
}

struct RunCursor {
    source: CursorSource,
    keys: Vec<SortKey>,
    eval: EvalContext,
    current_batch: Option<Batch>,
    extra_keys: Vec<Option<Arc<Column>>>,
    row: usize,
    res: Reservation,
}

impl RunCursor {
    fn from_reader(
        reader: SpillReader,
        keys: Vec<SortKey>,
        eval: EvalContext,
        pool: &Arc<ScopedPool>,
    ) -> Self {
        Self {
            source: CursorSource::Spill(reader),
            keys,
            eval,
            current_batch: None,
            extra_keys: Vec::new(),
            row: 0,
            res: pool.register("sort_merge_cursor", false),
        }
    }

    fn from_stream(
        stream: Box<dyn PartitionStream>,
        keys: Vec<SortKey>,
        eval: EvalContext,
        pool: &Arc<ScopedPool>,
    ) -> Self {
        Self {
            source: CursorSource::Stream(stream),
            keys,
            eval,
            current_batch: None,
            extra_keys: Vec::new(),
            row: 0,
            res: pool.register("sort_merge_cursor", false),
        }
    }

    fn advance(&mut self, cancel: &CancelToken) -> Result<bool, QueryError> {
        self.current_batch = None;
        self.extra_keys.clear();
        self.row = 0;
        self.res.free();

        loop {
            if cancel.is_cancelled() {
                return Err(QueryError::Cancelled);
            }
            let next = match &mut self.source {
                CursorSource::Spill(reader) => reader.next_batch(Some(&mut self.res))?,
                CursorSource::Stream(stream) => stream.next_batch(cancel, Some(&mut self.res))?,
            };
            let Some(mut batch) = next else {
                self.res.free();
                return Ok(false);
            };
            if batch.is_empty() {
                self.res.free();
                continue;
            }
            if !batch.selection().is_all(batch.raw_len()) {
                let compacted = batch.compact(Some(&mut self.res))?;
                drop(batch);
                let mem = compacted.memory_bytes();
                let cur = self.res.size();
                if cur > mem {
                    self.res.shrink(cur - mem);
                } else if mem > cur {
                    self.res.try_grow(mem - cur)?;
                }
                batch = compacted;
            }
            let eval_ctx = self.eval.with_cancel(cancel.clone());
            let extra = eval_extra_sort_keys(&self.keys, &batch, &eval_ctx, &mut self.res)?;
            self.current_batch = Some(batch);
            self.extra_keys = extra;
            self.row = 0;
            return Ok(true);
        }
    }

    fn current_batch_ref(&self) -> Option<&Batch> {
        self.current_batch.as_ref()
    }

    fn key_refs(&self) -> Result<Vec<&Column>, QueryError> {
        let Some(batch) = &self.current_batch else {
            return Err(QueryError::internal("cursor has no current batch"));
        };
        resolve_key_col_refs(&self.keys, batch, &self.extra_keys)
    }

    fn remaining_rows_in_batch(&self) -> usize {
        self.current_batch
            .as_ref()
            .map_or(0, |b| b.num_rows().saturating_sub(self.row))
    }

    fn est_row_bytes(&self) -> usize {
        self.current_batch
            .as_ref()
            .map_or(1, |b| (b.memory_bytes() / b.num_rows().max(1)).max(1))
    }
}

/// Compares the current rows of two `RunCursor`s, breaking ties by cursor index for stability.
fn compare_cursors(cursors: &[RunCursor], a_idx: usize, b_idx: usize) -> Ordering {
    let ca = &cursors[a_idx];
    let cb = &cursors[b_idx];
    if let (Ok(ka), Ok(kb)) = (ca.key_refs(), cb.key_refs()) {
        let ord = compare_key_refs_across(&ka, ca.row, &kb, cb.row, &ca.keys);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a_idx.cmp(&b_idx)
}

/// Binary min-heap storing `cursor_idx` ordered by `compare_cursors`.
struct CursorMinHeap {
    items: Vec<usize>,
}

impl CursorMinHeap {
    fn with_capacity(cap: usize) -> Self {
        Self {
            items: Vec::with_capacity(cap),
        }
    }

    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn push(&mut self, cursor_idx: usize, cursors: &[RunCursor]) {
        self.items.push(cursor_idx);
        let mut idx = self.items.len() - 1;
        while idx > 0 {
            let parent = (idx - 1) / 2;
            if compare_cursors(cursors, self.items[idx], self.items[parent]) == Ordering::Less {
                self.items.swap(idx, parent);
                idx = parent;
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<usize> {
        self.items.first().copied()
    }

    fn pop(&mut self, cursors: &[RunCursor]) -> Option<usize> {
        if self.items.is_empty() {
            return None;
        }
        let last = self.items.len() - 1;
        self.items.swap(0, last);
        let top = self.items.pop();
        if !self.items.is_empty() {
            self.sift_down(0, cursors);
        }
        top
    }

    fn replace_top(&mut self, cursor_idx: usize, cursors: &[RunCursor]) {
        self.items[0] = cursor_idx;
        self.sift_down(0, cursors);
    }

    fn sift_down(&mut self, mut idx: usize, cursors: &[RunCursor]) {
        let len = self.items.len();
        loop {
            let left = 2 * idx + 1;
            if left >= len {
                break;
            }
            let right = left + 1;
            let mut smallest = left;
            if right < len
                && compare_cursors(cursors, self.items[right], self.items[left]) == Ordering::Less
            {
                smallest = right;
            }
            if compare_cursors(cursors, self.items[smallest], self.items[idx]) == Ordering::Less {
                self.items.swap(idx, smallest);
                idx = smallest;
            } else {
                break;
            }
        }
    }
}

fn merge_spill_passes(
    runs: &mut Vec<SpillFile>,
    schema: &Arc<Schema>,
    keys: &[SortKey],
    limit: Option<usize>,
    max_merge_way: usize,
    ctx: &ExecContext,
) -> Result<(), QueryError> {
    let mut queue: VecDeque<SpillFile> = runs.drain(..).collect();
    while queue.len() > max_merge_way {
        if ctx.eval.cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        let take_n = max_merge_way.min(queue.len());
        let chunk: Vec<SpillFile> = queue.drain(..take_n).collect();
        let frame_bytes = target_frame_bytes(&ctx.pool, max_merge_way);

        let mut cursors = Vec::with_capacity(chunk.len());
        for sf in &chunk {
            let reader = sf.open_reader(Arc::clone(schema))?;
            cursors.push(RunCursor::from_reader(
                reader,
                keys.to_vec(),
                ctx.eval.clone(),
                &ctx.pool,
            ));
        }

        let mut heap = CursorMinHeap::with_capacity(cursors.len());
        for idx in 0..cursors.len() {
            if cursors[idx].advance(&ctx.eval.cancel)? {
                heap.push(idx, &cursors);
            }
        }

        let mut writer = ctx.spill.create_file()?;
        let mut out_res = ctx.pool.register("sort_merge_pass_out", false);
        let mut written_rows = 0usize;
        let max_rows_pass = limit.unwrap_or(usize::MAX);
        let mut picked: Vec<(u32, u32)> = Vec::new();
        let mut est_bytes = 0usize;

        while !heap.is_empty() && written_rows < max_rows_pass {
            if ctx.eval.cancel.is_cancelled() {
                return Err(QueryError::Cancelled);
            }
            let Some(c_idx) = heap.peek() else {
                break;
            };
            let row = cursors[c_idx].row;
            let row_bytes = cursors[c_idx].est_row_bytes();
            picked.push((c_idx as u32, row as u32));
            est_bytes = est_bytes.saturating_add(row_bytes);
            written_rows += 1;

            cursors[c_idx].row += 1;
            let batch_exhausted = cursors[c_idx].remaining_rows_in_batch() == 0;
            let hit_cap = est_bytes >= frame_bytes
                || picked.len() >= ctx.batch_size.max(1)
                || written_rows >= max_rows_pass;

            if batch_exhausted || hit_cap {
                let mut valid_refs: Vec<&Batch> = Vec::with_capacity(cursors.len());
                let mut remap = vec![u32::MAX; cursors.len()];
                for (i, c) in cursors.iter().enumerate() {
                    if let Some(b) = c.current_batch_ref() {
                        remap[i] = valid_refs.len() as u32;
                        valid_refs.push(b);
                    }
                }
                for (b, _) in &mut picked {
                    *b = remap[*b as usize];
                }
                let batch = build_batch_from_refs(
                    schema,
                    &valid_refs,
                    &picked,
                    Some(&mut out_res),
                    &ctx.eval,
                )?;
                picked.clear();
                est_bytes = 0;
                write_batch_framed(&mut writer, &batch, frame_bytes)?;
                drop(batch);
                out_res.free();
            }

            if batch_exhausted {
                let _ = heap.pop(&cursors);
                if cursors[c_idx].advance(&ctx.eval.cancel)? {
                    heap.push(c_idx, &cursors);
                }
            } else {
                heap.replace_top(c_idx, &cursors);
            }
        }

        drop(out_res);
        drop(cursors);
        // Drop input SpillFiles immediately to close/unlink old files before next pass.
        drop(chunk);

        let merged = writer.finish()?;
        queue.push_back(merged);
    }

    runs.extend(queue);
    Ok(())
}

struct MergeStream {
    schema: Arc<Schema>,
    ctx: ExecContext,
    _spill_files: Vec<SpillFile>,
    cursors: Vec<RunCursor>,
    heap: CursorMinHeap,
    initialized: bool,
    limit: Option<usize>,
    emitted: usize,
    frame_bytes: usize,
    max_output_rows: usize,
    out_res: Reservation,
}

impl MergeStream {
    fn from_spill_files(
        spill_files: Vec<SpillFile>,
        schema: Arc<Schema>,
        keys: &[SortKey],
        limit: Option<usize>,
        frame_bytes: usize,
        max_output_rows: usize,
        ctx: ExecContext,
    ) -> Result<Box<dyn PartitionStream>, QueryError> {
        let mut cursors = Vec::with_capacity(spill_files.len());
        for sf in &spill_files {
            let reader = sf.open_reader(Arc::clone(&schema))?;
            cursors.push(RunCursor::from_reader(
                reader,
                keys.to_vec(),
                ctx.eval.clone(),
                &ctx.pool,
            ));
        }
        let heap = CursorMinHeap::with_capacity(cursors.len());
        let out_res = ctx.pool.register("sort_merge_out", false);
        Ok(Box::new(Self {
            schema,
            ctx,
            _spill_files: spill_files,
            cursors,
            heap,
            initialized: false,
            limit,
            emitted: 0,
            frame_bytes,
            max_output_rows,
            out_res,
        }))
    }

    fn from_streams(
        streams: Vec<Box<dyn PartitionStream>>,
        schema: Arc<Schema>,
        keys: &[SortKey],
        limit: Option<usize>,
        frame_bytes: usize,
        max_output_rows: usize,
        ctx: ExecContext,
    ) -> Result<Box<dyn PartitionStream>, QueryError> {
        let mut cursors = Vec::with_capacity(streams.len());
        for stream in streams {
            cursors.push(RunCursor::from_stream(
                stream,
                keys.to_vec(),
                ctx.eval.clone(),
                &ctx.pool,
            ));
        }
        let heap = CursorMinHeap::with_capacity(cursors.len());
        let out_res = ctx.pool.register("sort_merge_out", false);
        Ok(Box::new(Self {
            schema,
            ctx,
            _spill_files: Vec::new(),
            cursors,
            heap,
            initialized: false,
            limit,
            emitted: 0,
            frame_bytes,
            max_output_rows,
            out_res,
        }))
    }

    fn init_cursors(&mut self, cancel: &CancelToken) -> Result<(), QueryError> {
        if self.initialized {
            return Ok(());
        }
        self.initialized = true;
        for idx in 0..self.cursors.len() {
            if self.cursors[idx].advance(cancel)? {
                self.heap.push(idx, &self.cursors);
            }
        }
        Ok(())
    }

    fn flush_picked(
        schema: &Arc<Schema>,
        cursors: &[RunCursor],
        picked: &mut Vec<(u32, u32)>,
        res: &mut Reservation,
        eval_ctx: &EvalContext,
    ) -> Result<Batch, QueryError> {
        let mut valid_refs: Vec<&Batch> = Vec::with_capacity(cursors.len());
        let mut remap = vec![u32::MAX; cursors.len()];
        for (i, c) in cursors.iter().enumerate() {
            if let Some(b) = c.current_batch_ref() {
                remap[i] = valid_refs.len() as u32;
                valid_refs.push(b);
            }
        }
        for (b, _) in picked.iter_mut() {
            *b = remap[*b as usize];
        }
        let batch = build_batch_from_refs(schema, &valid_refs, picked, Some(res), eval_ctx)?;
        picked.clear();
        Ok(batch)
    }
}

impl PartitionStream for MergeStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = pool_size(&res);
        if cancel.is_cancelled() {
            rollback_to(&mut res, before);
            return Err(QueryError::Cancelled);
        }

        if let Err(err) = self.init_cursors(cancel) {
            rollback_to(&mut res, before);
            return Err(err);
        }

        let remaining_limit = self
            .limit
            .map_or(usize::MAX, |l| l.saturating_sub(self.emitted));
        if remaining_limit == 0 || self.heap.is_empty() {
            self.cursors.clear();
            self._spill_files.clear();
            self.out_res.free();
            return Ok(None);
        }

        let row_cap = self.max_output_rows.min(remaining_limit);
        let eval_ctx = self.ctx.eval.with_cancel(cancel.clone());

        let mut picked: Vec<(u32, u32)> = Vec::new();
        let mut est_picked_bytes = 0usize;
        let mut staged: Vec<Batch> = Vec::new();
        let mut staged_rows = 0usize;
        let mut staged_bytes = 0usize;

        while !self.heap.is_empty() && (staged_rows + picked.len()) < row_cap {
            if cancel.is_cancelled() {
                self.out_res.free();
                rollback_to(&mut res, before);
                return Err(QueryError::Cancelled);
            }
            let Some(c_idx) = self.heap.peek() else {
                break;
            };
            let row = self.cursors[c_idx].row;
            let row_bytes = self.cursors[c_idx].est_row_bytes();
            picked.push((c_idx as u32, row as u32));
            est_picked_bytes = est_picked_bytes.saturating_add(row_bytes);

            self.cursors[c_idx].row += 1;
            let batch_exhausted = self.cursors[c_idx].remaining_rows_in_batch() == 0;
            let total_rows = staged_rows + picked.len();
            let hit_cap =
                (staged_bytes + est_picked_bytes) >= self.frame_bytes || total_rows >= row_cap;

            if batch_exhausted || hit_cap {
                let mini = match Self::flush_picked(
                    &self.schema,
                    &self.cursors,
                    &mut picked,
                    &mut self.out_res,
                    &eval_ctx,
                ) {
                    Ok(b) => b,
                    Err(err) => {
                        self.out_res.free();
                        rollback_to(&mut res, before);
                        return Err(err);
                    }
                };
                est_picked_bytes = 0;
                staged_rows += mini.num_rows();
                staged_bytes += mini.memory_bytes();
                staged.push(mini);
            }

            if batch_exhausted {
                let _ = self.heap.pop(&self.cursors);
                match self.cursors[c_idx].advance(cancel) {
                    Ok(true) => self.heap.push(c_idx, &self.cursors),
                    Ok(false) => {}
                    Err(err) => {
                        drop(staged);
                        self.out_res.free();
                        rollback_to(&mut res, before);
                        return Err(err);
                    }
                }
            } else {
                self.heap.replace_top(c_idx, &self.cursors);
            }

            if staged_bytes >= self.frame_bytes / 2 || staged_rows >= row_cap {
                break;
            }
        }

        if !picked.is_empty() {
            let mini = match Self::flush_picked(
                &self.schema,
                &self.cursors,
                &mut picked,
                &mut self.out_res,
                &eval_ctx,
            ) {
                Ok(b) => b,
                Err(err) => {
                    drop(staged);
                    self.out_res.free();
                    rollback_to(&mut res, before);
                    return Err(err);
                }
            };
            staged_rows += mini.num_rows();
            staged.push(mini);
        }

        if self.heap.is_empty() || self.limit.is_some_and(|l| self.emitted + staged_rows >= l) {
            self.cursors.clear();
            self._spill_files.clear();
        }

        if staged.is_empty() {
            self.out_res.free();
            return Ok(None);
        }

        if staged.len() == 1 {
            let out = staged.remove(0);
            let out_bytes = out.memory_bytes();
            if let Some(cr) = res.as_deref_mut() {
                let xfer = self.out_res.size().min(out_bytes);
                if xfer > 0
                    && let Err(err) = self.out_res.transfer_to(cr, xfer)
                {
                    self.out_res.free();
                    rollback_to(&mut res, before);
                    return Err(err.into());
                }
            }
            self.out_res.free();
            if let Err(err) = reconcile_output(&mut res, before, out_bytes) {
                rollback_to(&mut res, before);
                return Err(err);
            }
            self.emitted += out.num_rows();
            return Ok(Some(out));
        }

        let refs: Vec<&Batch> = staged.iter().collect();
        let out = match Batch::concat(Arc::clone(&self.schema), &refs, res.as_deref_mut()) {
            Ok(b) => b,
            Err(err) => {
                drop(staged);
                self.out_res.free();
                rollback_to(&mut res, before);
                return Err(err.into());
            }
        };
        drop(staged);
        self.out_res.free();
        self.emitted += out.num_rows();
        Ok(Some(out))
    }
}

// ---------------------------------------------------------------------------
// Window operator
// ---------------------------------------------------------------------------

/// Window function evaluated by [`eval_window_group`].
#[derive(Debug, Clone, PartialEq)]
pub enum WindowFunc {
    /// `ROW_NUMBER()` — 1-based sequential row index within each partition.
    RowNumber,
    /// `RANK()` — 1-based rank with gaps for `ORDER BY` peer ties.
    Rank,
    /// `DENSE_RANK()` — 1-based rank without gaps for `ORDER BY` peer ties.
    DenseRank,
    /// `LAG(expr [, offset [, default]])`.
    Lag {
        /// Value expression.
        expr: Expr,
        /// Row offset (`0` = current row, negative flips to lead).
        offset: i64,
        /// Fallback expression evaluated at current row when out of partition bounds.
        default: Option<Expr>,
    },
    /// `LEAD(expr [, offset [, default]])`.
    Lead {
        /// Value expression.
        expr: Expr,
        /// Row offset (`0` = current row, negative flips to lag).
        offset: i64,
        /// Fallback expression evaluated at current row when out of partition bounds.
        default: Option<Expr>,
    },
    /// `FIRST_VALUE(expr)` — value at the first row of the active frame.
    FirstValue(Expr),
    /// `LAST_VALUE(expr)` — value at the last row of the active frame.
    LastValue(Expr),
    /// `SUM(expr)` — sum of non-null values in the active frame.
    Sum(Expr),
    /// `COUNT(*)` (`None`) or `COUNT(expr)` (`Some(expr)`).
    Count(Option<Expr>),
    /// `AVG(expr)` — mean of non-null values in the active frame.
    Avg(Expr),
    /// `MIN(expr)` — minimum non-null value in the active frame.
    Min(Expr),
    /// `MAX(expr)` — maximum non-null value in the active frame.
    Max(Expr),
}

/// Frame boundary for explicit `ROWS BETWEEN start AND end` window frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameBound {
    /// `UNBOUNDED PRECEDING`
    UnboundedPreceding,
    /// `<n> PRECEDING`
    Preceding(u64),
    /// `CURRENT ROW`
    CurrentRow,
    /// `<n> FOLLOWING`
    Following(u64),
    /// `UNBOUNDED FOLLOWING`
    UnboundedFollowing,
}

/// One window output specification inside a window group.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    /// Partition expressions.
    pub partition_by: Vec<Expr>,
    /// Ordering keys within each partition.
    pub order_by: Vec<SortKey>,
    /// Optional explicit `ROWS BETWEEN start AND end` frame; `None` selects the SQL default frame.
    pub frame: Option<(FrameBound, FrameBound)>,
    /// Window function to evaluate.
    pub func: WindowFunc,
    /// Output column name appended to `input_schema`.
    pub output_name: String,
}

fn validate_window_spec(spec: &WindowSpec) -> Result<Field, QueryError> {
    if let Some((start, end)) = spec.frame {
        if start == FrameBound::UnboundedFollowing {
            return Err(QueryError::Invalid(InvalidQuery::new(
                "window frame start cannot be UNBOUNDED FOLLOWING",
                None,
            )));
        }
        if end == FrameBound::UnboundedPreceding {
            return Err(QueryError::Invalid(InvalidQuery::new(
                "window frame end cannot be UNBOUNDED PRECEDING",
                None,
            )));
        }
        if matches!(
            (start, end),
            (FrameBound::CurrentRow, FrameBound::Preceding(_))
                | (
                    FrameBound::Following(_),
                    FrameBound::CurrentRow | FrameBound::Preceding(_)
                )
        ) {
            return Err(QueryError::Invalid(InvalidQuery::new(
                "window frame start cannot follow window frame end",
                None,
            )));
        }
    }

    let (dt, nullable) = match &spec.func {
        WindowFunc::RowNumber | WindowFunc::Rank | WindowFunc::DenseRank | WindowFunc::Count(_) => {
            (DataType::I64, false)
        }
        WindowFunc::Lag { expr, default, .. } | WindowFunc::Lead { expr, default, .. } => {
            let expr_dt = expr.data_type();
            let dt = if let Some(def) = default {
                let def_dt = def.data_type();
                if expr_dt == DataType::Null && def_dt != DataType::Null {
                    def_dt
                } else if def_dt == DataType::Null || expr_dt == def_dt {
                    expr_dt
                } else {
                    return Err(QueryError::Invalid(InvalidQuery::new(
                        "LAG/LEAD default expression type must match value expression",
                        None,
                    )));
                }
            } else {
                expr_dt
            };
            (dt, true)
        }
        WindowFunc::FirstValue(expr) | WindowFunc::LastValue(expr) => (expr.data_type(), true),
        WindowFunc::Sum(expr) => {
            let dt = match expr.data_type() {
                DataType::I64 => DataType::I64,
                DataType::F64 => DataType::F64,
                DataType::Decimal128 { scale, .. } => DataType::Decimal128 {
                    precision: 38,
                    scale,
                },
                DataType::Null => DataType::Null,
                _ => {
                    return Err(QueryError::Invalid(InvalidQuery::new(
                        "SUM requires numeric input",
                        None,
                    )));
                }
            };
            (dt, true)
        }
        WindowFunc::Avg(expr) => {
            let dt = match expr.data_type() {
                DataType::I64 | DataType::F64 => DataType::F64,
                DataType::Decimal128 { precision, scale } => {
                    let out_p = precision.saturating_add(4).min(38);
                    let out_s = scale.saturating_add(4).min(out_p as i8);
                    DataType::Decimal128 {
                        precision: out_p,
                        scale: out_s,
                    }
                }
                DataType::Null => DataType::Null,
                _ => {
                    return Err(QueryError::Invalid(InvalidQuery::new(
                        "AVG requires numeric input",
                        None,
                    )));
                }
            };
            (dt, true)
        }
        WindowFunc::Min(expr) | WindowFunc::Max(expr) => {
            let dt = expr.data_type();
            if matches!(dt, DataType::MapUtf8 | DataType::Events | DataType::Links) {
                return Err(QueryError::Invalid(InvalidQuery::new(
                    "MIN/MAX does not support map, events, or links types",
                    None,
                )));
            }
            (dt, true)
        }
    };

    Ok(Field::new(spec.output_name.clone(), dt, nullable))
}

/// Evaluates a group of [`WindowSpec`]s sharing `(partition_by, order_by)` over `input`,
/// appending one output column per spec to `input_schema`.
pub fn eval_window_group(
    input: Box<dyn PartitionStream>,
    input_schema: Arc<Schema>,
    partition_by: Vec<Expr>,
    order_by: Vec<SortKey>,
    specs: Vec<WindowSpec>,
    ctx: &ExecContext,
) -> Result<Box<dyn PartitionStream>, QueryError> {
    let mut fields = input_schema.fields().to_vec();
    let mut out_types = Vec::with_capacity(specs.len());
    for spec in &specs {
        if spec.partition_by != partition_by || spec.order_by != order_by {
            return Err(QueryError::Invalid(InvalidQuery::new(
                "window spec partition_by/order_by must match window group",
                None,
            )));
        }
        let field = validate_window_spec(spec)?;
        out_types.push(field.data_type.clone());
        fields.push(field);
    }
    let output_schema = Schema::try_new_arc(fields)?;

    if specs.is_empty() {
        return Ok(input);
    }

    let mut sort_keys = Vec::with_capacity(partition_by.len() + order_by.len());
    for p_expr in &partition_by {
        sort_keys.push(SortKey {
            expr: p_expr.clone(),
            descending: false,
            nulls_first: false,
        });
    }
    sort_keys.extend(order_by.iter().cloned());

    let res = ctx.pool.register("window_partition", false);
    let streaming = if specs_are_zero_lookahead_streamable(&specs) {
        Some(StreamingWindowState::new(&specs, &out_types)?)
    } else {
        None
    };
    Ok(Box::new(WindowStream {
        input: Some(input),
        sorted: None,
        input_schema,
        output_schema,
        partition_by,
        order_by,
        sort_keys,
        specs,
        out_types,
        ctx: ctx.clone(),
        lookahead: None,
        queued_out: VecDeque::new(),
        streaming,
        res,
    }))
}

fn frame_is_zero_lookahead(frame: Option<(FrameBound, FrameBound)>) -> bool {
    let Some((start, end)) = frame else {
        return false;
    };
    let end_ok = match end {
        FrameBound::CurrentRow => true,
        FrameBound::Preceding(n) => n <= MAX_STREAMING_WINDOW_LAG as u64,
        FrameBound::UnboundedPreceding
        | FrameBound::Following(_)
        | FrameBound::UnboundedFollowing => false,
    };
    if !end_ok {
        return false;
    }
    match start {
        FrameBound::UnboundedPreceding => true,
        FrameBound::Preceding(m) => m <= MAX_STREAMING_WINDOW_LAG as u64,
        FrameBound::CurrentRow => matches!(end, FrameBound::CurrentRow),
        FrameBound::Following(_) | FrameBound::UnboundedFollowing => false,
    }
}

/// Returns `true` iff every spec in `specs` can stream row-by-row off `ExternalSort`
/// with zero full-partition lookahead.
fn specs_are_zero_lookahead_streamable(specs: &[WindowSpec]) -> bool {
    specs.iter().all(|spec| match &spec.func {
        WindowFunc::RowNumber | WindowFunc::Rank | WindowFunc::DenseRank => true,
        WindowFunc::Lag { offset, .. } => {
            *offset >= 0 && *offset <= MAX_STREAMING_WINDOW_LAG as i64
        }
        WindowFunc::Lead { .. } => false,
        WindowFunc::FirstValue(_)
        | WindowFunc::LastValue(_)
        | WindowFunc::Sum(_)
        | WindowFunc::Count(_)
        | WindowFunc::Avg(_)
        | WindowFunc::Min(_)
        | WindowFunc::Max(_) => frame_is_zero_lookahead(spec.frame),
    })
}

fn normalize_bound(b: FrameBound) -> FrameBound {
    match b {
        FrameBound::Preceding(0) => FrameBound::CurrentRow,
        other => other,
    }
}

fn opt_scalar_bytes(sv: &Option<ScalarValue>) -> usize {
    size_of::<Option<ScalarValue>>() + sv.as_ref().map_or(0, ScalarValue::heap_bytes)
}

#[derive(Clone)]
enum StreamSumAcc {
    Null,
    I64(Option<i64>),
    F64(Option<f64>),
    Decimal128 {
        acc: Option<i128>,
        precision: u8,
        scale: i8,
    },
}

impl StreamSumAcc {
    fn from_types(out_dt: &DataType) -> Result<Self, QueryError> {
        match out_dt {
            DataType::Null => Ok(Self::Null),
            DataType::I64 => Ok(Self::I64(None)),
            DataType::F64 => Ok(Self::F64(None)),
            DataType::Decimal128 { precision, scale } => Ok(Self::Decimal128 {
                acc: None,
                precision: *precision,
                scale: *scale,
            }),
            _ => Err(QueryError::failed("unsupported SUM input type")),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Null => {}
            Self::I64(v) => *v = None,
            Self::F64(v) => *v = None,
            Self::Decimal128 { acc, .. } => *acc = None,
        }
    }

    fn add_scalar(&mut self, val: Option<&ScalarValue>) -> Result<(), QueryError> {
        let Some(sv) = val else {
            return Ok(());
        };
        match (self, sv) {
            (Self::Null, _) => Ok(()),
            (Self::I64(acc), ScalarValue::I64(v)) => {
                *acc = Some(match *acc {
                    Some(cur) => cur
                        .checked_add(*v)
                        .ok_or_else(|| QueryError::failed("window SUM(I64) overflow"))?,
                    None => *v,
                });
                Ok(())
            }
            (Self::F64(acc), ScalarValue::F64(v)) => {
                *acc = Some(acc.map_or(*v, |cur| cur + *v));
                Ok(())
            }
            (Self::Decimal128 { acc, precision, .. }, ScalarValue::Decimal128 { value, .. }) => {
                let next = match *acc {
                    Some(cur) => cur
                        .checked_add(*value)
                        .ok_or_else(|| QueryError::failed("window SUM(Decimal128) overflow"))?,
                    None => *value,
                };
                *acc = Some(check_dec_bounds(next, *precision)?);
                Ok(())
            }
            _ => Err(QueryError::failed("unsupported SUM input type")),
        }
    }

    fn to_scalar(&self) -> Option<ScalarValue> {
        match self {
            Self::Null => None,
            Self::I64(v) => v.map(ScalarValue::I64),
            Self::F64(v) => v.map(ScalarValue::F64),
            Self::Decimal128 {
                acc,
                precision,
                scale,
            } => acc.map(|value| ScalarValue::Decimal128 {
                value,
                precision: *precision,
                scale: *scale,
            }),
        }
    }
}

#[derive(Clone)]
enum StreamAvgAcc {
    Null,
    I64 {
        sum: i128,
        cnt: u64,
    },
    F64 {
        sum: f64,
        cnt: u64,
    },
    Decimal128 {
        sum: i128,
        cnt: u128,
        scale_factor: u128,
        out_p: u8,
        out_s: i8,
    },
}

impl StreamAvgAcc {
    fn from_types(in_dt: &DataType, out_dt: &DataType) -> Result<Self, QueryError> {
        match (in_dt, out_dt) {
            (DataType::Null, _) => Ok(Self::Null),
            (DataType::I64, DataType::F64) => Ok(Self::I64 { sum: 0, cnt: 0 }),
            (DataType::F64, DataType::F64) => Ok(Self::F64 { sum: 0.0, cnt: 0 }),
            (
                DataType::Decimal128 {
                    scale: in_scale, ..
                },
                DataType::Decimal128 {
                    precision: out_p,
                    scale: out_s,
                },
            ) => {
                let scale_diff = (i32::from(*out_s) - i32::from(*in_scale)).max(0) as u32;
                let scale_factor = 10u128
                    .checked_pow(scale_diff)
                    .ok_or_else(|| QueryError::failed("decimal AVG scale overflow"))?;
                Ok(Self::Decimal128 {
                    sum: 0,
                    cnt: 0,
                    scale_factor,
                    out_p: *out_p,
                    out_s: *out_s,
                })
            }
            _ => Err(QueryError::failed("unsupported AVG input type")),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Null => {}
            Self::I64 { sum, cnt } => {
                *sum = 0;
                *cnt = 0;
            }
            Self::F64 { sum, cnt } => {
                *sum = 0.0;
                *cnt = 0;
            }
            Self::Decimal128 { sum, cnt, .. } => {
                *sum = 0;
                *cnt = 0;
            }
        }
    }

    fn add_scalar(&mut self, val: Option<&ScalarValue>) -> Result<(), QueryError> {
        let Some(sv) = val else {
            return Ok(());
        };
        match (self, sv) {
            (Self::Null, _) => Ok(()),
            (Self::I64 { sum, cnt }, ScalarValue::I64(v)) => {
                *sum += i128::from(*v);
                *cnt += 1;
                Ok(())
            }
            (Self::F64 { sum, cnt }, ScalarValue::F64(v)) => {
                *sum += *v;
                *cnt += 1;
                Ok(())
            }
            (Self::Decimal128 { sum, cnt, .. }, ScalarValue::Decimal128 { value, .. }) => {
                *sum = sum
                    .checked_add(*value)
                    .ok_or_else(|| QueryError::failed("window AVG(Decimal128) overflow"))?;
                *cnt += 1;
                Ok(())
            }
            _ => Err(QueryError::failed("unsupported AVG input type")),
        }
    }

    fn to_scalar(&self) -> Result<Option<ScalarValue>, QueryError> {
        match self {
            Self::Null => Ok(None),
            Self::I64 { sum, cnt } => {
                Ok((*cnt > 0).then(|| ScalarValue::F64((*sum as f64) / (*cnt as f64))))
            }
            Self::F64 { sum, cnt } => {
                Ok((*cnt > 0).then(|| ScalarValue::F64(*sum / (*cnt as f64))))
            }
            Self::Decimal128 {
                sum,
                cnt,
                scale_factor,
                out_p,
                out_s,
            } => {
                if *cnt == 0 {
                    Ok(None)
                } else {
                    let v = decimal_avg(*sum, *cnt, *scale_factor, *out_p)?;
                    Ok(Some(ScalarValue::Decimal128 {
                        value: v,
                        precision: *out_p,
                        scale: *out_s,
                    }))
                }
            }
        }
    }
}

enum StreamingSpecState {
    RowNumber,
    Rank {
        current_rank: i64,
    },
    DenseRank {
        current_dense_rank: i64,
    },
    Lag {
        offset: usize,
        ring: VecDeque<Option<ScalarValue>>,
    },
    FrameFirstLast {
        is_last: bool,
        start: FrameBound,
        end: FrameBound,
        first_val: Option<Option<ScalarValue>>,
        ring: VecDeque<Option<ScalarValue>>,
        ring_start: usize,
    },
    FrameCount {
        has_expr: bool,
        start: FrameBound,
        end: FrameBound,
        prefix: i64,
        ring: VecDeque<bool>,
        ring_start: usize,
    },
    FrameSum {
        start: FrameBound,
        end: FrameBound,
        acc: StreamSumAcc,
        ring: VecDeque<Option<ScalarValue>>,
        ring_start: usize,
    },
    FrameAvg {
        start: FrameBound,
        end: FrameBound,
        acc: StreamAvgAcc,
        ring: VecDeque<Option<ScalarValue>>,
        ring_start: usize,
    },
    FrameMinMax {
        is_max: bool,
        start: FrameBound,
        end: FrameBound,
        best: Option<ScalarValue>,
        ring: VecDeque<Option<ScalarValue>>,
        ring_start: usize,
    },
}

impl StreamingSpecState {
    fn from_spec(spec: &WindowSpec, out_dt: &DataType) -> Result<Self, QueryError> {
        let norm_frame = || -> Result<(FrameBound, FrameBound), QueryError> {
            let Some((s, e)) = spec.frame else {
                return Err(QueryError::internal(
                    "missing frame on streaming window spec",
                ));
            };
            Ok((normalize_bound(s), normalize_bound(e)))
        };
        match &spec.func {
            WindowFunc::RowNumber => Ok(Self::RowNumber),
            WindowFunc::Rank => Ok(Self::Rank { current_rank: 1 }),
            WindowFunc::DenseRank => Ok(Self::DenseRank {
                current_dense_rank: 1,
            }),
            WindowFunc::Lag { offset, .. } => Ok(Self::Lag {
                offset: usize::try_from(*offset)
                    .map_err(|_| QueryError::internal("negative offset in streaming LAG"))?,
                ring: VecDeque::new(),
            }),
            WindowFunc::FirstValue(_) => {
                let (start, end) = norm_frame()?;
                Ok(Self::FrameFirstLast {
                    is_last: false,
                    start,
                    end,
                    first_val: None,
                    ring: VecDeque::new(),
                    ring_start: 0,
                })
            }
            WindowFunc::LastValue(_) => {
                let (start, end) = norm_frame()?;
                Ok(Self::FrameFirstLast {
                    is_last: true,
                    start,
                    end,
                    first_val: None,
                    ring: VecDeque::new(),
                    ring_start: 0,
                })
            }
            WindowFunc::Count(expr_opt) => {
                let (start, end) = norm_frame()?;
                Ok(Self::FrameCount {
                    has_expr: expr_opt.is_some(),
                    start,
                    end,
                    prefix: 0,
                    ring: VecDeque::new(),
                    ring_start: 0,
                })
            }
            WindowFunc::Sum(_) => {
                let (start, end) = norm_frame()?;
                Ok(Self::FrameSum {
                    start,
                    end,
                    acc: StreamSumAcc::from_types(out_dt)?,
                    ring: VecDeque::new(),
                    ring_start: 0,
                })
            }
            WindowFunc::Avg(expr) => {
                let (start, end) = norm_frame()?;
                Ok(Self::FrameAvg {
                    start,
                    end,
                    acc: StreamAvgAcc::from_types(&expr.data_type(), out_dt)?,
                    ring: VecDeque::new(),
                    ring_start: 0,
                })
            }
            WindowFunc::Min(_) | WindowFunc::Max(_) => {
                let (start, end) = norm_frame()?;
                Ok(Self::FrameMinMax {
                    is_max: matches!(spec.func, WindowFunc::Max(_)),
                    start,
                    end,
                    best: None,
                    ring: VecDeque::new(),
                    ring_start: 0,
                })
            }
            WindowFunc::Lead { .. } => Err(QueryError::internal(
                "LEAD cannot be evaluated in streaming mode",
            )),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::RowNumber => {}
            Self::Rank { current_rank } => *current_rank = 1,
            Self::DenseRank { current_dense_rank } => *current_dense_rank = 1,
            Self::Lag { ring, .. } => ring.clear(),
            Self::FrameFirstLast {
                first_val,
                ring,
                ring_start,
                ..
            } => {
                *first_val = None;
                ring.clear();
                *ring_start = 0;
            }
            Self::FrameCount {
                prefix,
                ring,
                ring_start,
                ..
            } => {
                *prefix = 0;
                ring.clear();
                *ring_start = 0;
            }
            Self::FrameSum {
                acc,
                ring,
                ring_start,
                ..
            } => {
                acc.reset();
                ring.clear();
                *ring_start = 0;
            }
            Self::FrameAvg {
                acc,
                ring,
                ring_start,
                ..
            } => {
                acc.reset();
                ring.clear();
                *ring_start = 0;
            }
            Self::FrameMinMax {
                best,
                ring,
                ring_start,
                ..
            } => {
                *best = None;
                ring.clear();
                *ring_start = 0;
            }
        }
    }

    fn heap_bytes(&self) -> usize {
        match self {
            Self::RowNumber | Self::Rank { .. } | Self::DenseRank { .. } => 0,
            Self::Lag { ring, .. } => ring.iter().map(opt_scalar_bytes).sum(),
            Self::FrameFirstLast {
                first_val, ring, ..
            } => {
                first_val.as_ref().map_or(0, opt_scalar_bytes)
                    + ring.iter().map(opt_scalar_bytes).sum::<usize>()
            }
            Self::FrameCount { ring, .. } => ring.capacity() * size_of::<bool>(),
            Self::FrameSum { ring, .. } | Self::FrameAvg { ring, .. } => {
                ring.iter().map(opt_scalar_bytes).sum()
            }
            Self::FrameMinMax { best, ring, .. } => {
                best.as_ref().map_or(0, ScalarValue::heap_bytes)
                    + ring.iter().map(opt_scalar_bytes).sum::<usize>()
            }
        }
    }
}

fn stream_frame_bounds(start: FrameBound, end: FrameBound, row: usize) -> Option<(usize, usize)> {
    let r = row as i128;
    let lo = match start {
        FrameBound::UnboundedPreceding => 0,
        FrameBound::Preceding(m) => r - (m as i128),
        FrameBound::CurrentRow => r,
        FrameBound::Following(_) | FrameBound::UnboundedFollowing => return None,
    };
    let hi = match end {
        FrameBound::Preceding(n) => r - (n as i128),
        FrameBound::CurrentRow => r,
        FrameBound::UnboundedPreceding
        | FrameBound::Following(_)
        | FrameBound::UnboundedFollowing => return None,
    };
    let s = lo.max(0);
    if s > hi {
        None
    } else {
        Some((s as usize, (hi as usize) + 1))
    }
}

fn min_max_better(cand: &ScalarValue, cur: &ScalarValue, is_max: bool) -> bool {
    let ord = compare_elem_refs(ElemRef::Scalar(cand), ElemRef::Scalar(cur));
    if is_max {
        ord == Ordering::Greater
    } else {
        ord == Ordering::Less
    }
}

struct StreamingWindowState {
    prev_part_row: Option<Vec<Column>>,
    prev_order_row: Option<Vec<Column>>,
    row_in_partition: usize,
    spec_states: Vec<StreamingSpecState>,
}

impl StreamingWindowState {
    fn new(specs: &[WindowSpec], out_types: &[DataType]) -> Result<Self, QueryError> {
        let mut spec_states = Vec::with_capacity(specs.len());
        for (spec, dt) in specs.iter().zip(out_types) {
            spec_states.push(StreamingSpecState::from_spec(spec, dt)?);
        }
        Ok(Self {
            prev_part_row: None,
            prev_order_row: None,
            row_in_partition: 0,
            spec_states,
        })
    }

    fn reset_partition(&mut self) {
        self.row_in_partition = 0;
        for st in &mut self.spec_states {
            st.reset();
        }
    }

    fn state_bytes(&self) -> usize {
        let part_b: usize = self
            .prev_part_row
            .as_ref()
            .map_or(0, |cols| cols.iter().map(Column::memory_bytes).sum());
        let ord_b: usize = self
            .prev_order_row
            .as_ref()
            .map_or(0, |cols| cols.iter().map(Column::memory_bytes).sum());
        let spec_b: usize = self
            .spec_states
            .iter()
            .map(StreamingSpecState::heap_bytes)
            .sum();
        part_b + ord_b + spec_b
    }
}

struct WindowStream {
    input: Option<Box<dyn PartitionStream>>,
    sorted: Option<Box<dyn PartitionStream>>,
    input_schema: Arc<Schema>,
    output_schema: Arc<Schema>,
    partition_by: Vec<Expr>,
    order_by: Vec<SortKey>,
    sort_keys: Vec<SortKey>,
    specs: Vec<WindowSpec>,
    out_types: Vec<DataType>,
    ctx: ExecContext,
    lookahead: Option<Batch>,
    queued_out: VecDeque<Batch>,
    streaming: Option<StreamingWindowState>,
    res: Reservation,
}

impl WindowStream {
    fn ensure_sorted(&mut self, cancel: &CancelToken) -> Result<(), QueryError> {
        if self.sorted.is_some() {
            return Ok(());
        }
        let Some(mut input) = self.input.take() else {
            return Ok(());
        };
        let mut sort = ExternalSort::new(
            Arc::clone(&self.input_schema),
            self.sort_keys.clone(),
            None,
            self.ctx.clone(),
        );
        let mut scan_res = self.ctx.pool.register("window_scan", false);
        loop {
            if cancel.is_cancelled() {
                scan_res.free();
                return Err(QueryError::Cancelled);
            }
            let next = input.next_batch(cancel, Some(&mut scan_res));
            let batch = match next {
                Ok(Some(b)) => b,
                Ok(None) => {
                    scan_res.free();
                    break;
                }
                Err(err) => {
                    scan_res.free();
                    return Err(err);
                }
            };
            let flow = sort.consume(batch, &mut scan_res);
            scan_res.free();
            if flow? == ControlFlow::Stop {
                break;
            }
        }
        drop(scan_res);
        self.sorted = Some(sort.into_stream()?);
        Ok(())
    }

    fn pull_next_sorted_batch(
        &mut self,
        cancel: &CancelToken,
    ) -> Result<Option<Batch>, QueryError> {
        let Some(mut sorted) = self.sorted.take() else {
            return Ok(None);
        };
        let res = loop {
            if cancel.is_cancelled() {
                break Err(QueryError::Cancelled);
            }
            let before = self.res.size();
            let next = match sorted.next_batch(cancel, Some(&mut self.res)) {
                Ok(n) => n,
                Err(err) => break Err(err),
            };
            let Some(mut batch) = next else {
                break Ok(None);
            };
            if batch.is_empty() {
                let cur = self.res.size();
                if cur > before {
                    self.res.shrink(cur - before);
                }
                continue;
            }
            if !batch.selection().is_all(batch.raw_len()) {
                let compacted = match batch.compact(Some(&mut self.res)) {
                    Ok(c) => c,
                    Err(err) => break Err(err.into()),
                };
                drop(batch);
                batch = compacted;
            }
            break Ok(Some(batch));
        };
        self.sorted = Some(sorted);
        res
    }

    fn current_held_bytes(&self, extra: usize) -> usize {
        let lookahead_bytes = self.lookahead.as_ref().map_or(0, Batch::memory_bytes);
        let queued_bytes: usize = self.queued_out.iter().map(Batch::memory_bytes).sum();
        let stream_bytes = self
            .streaming
            .as_ref()
            .map_or(0, StreamingWindowState::state_bytes);
        lookahead_bytes + queued_bytes + stream_bytes + extra
    }

    fn reconcile_window_res(&mut self) -> Result<(), QueryError> {
        let target = self.current_held_bytes(0);
        self.res.resize(target).map_err(QueryError::from)
    }

    fn next_streaming_batch(
        &mut self,
        cancel: &CancelToken,
        mut caller_res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = pool_size(&caller_res);
        let Some(batch) = self.pull_next_sorted_batch(cancel)? else {
            self.sorted = None;
            if let Some(st) = &mut self.streaming {
                st.prev_part_row = None;
                st.prev_order_row = None;
                st.reset_partition();
            }
            self.res.free();
            return Ok(None);
        };

        let n = batch.num_rows();
        let eval_ctx = self.ctx.eval.with_cancel(cancel.clone());
        let part_sort_keys: Vec<SortKey> = self
            .partition_by
            .iter()
            .map(|e| SortKey {
                expr: e.clone(),
                descending: false,
                nulls_first: false,
            })
            .collect();

        let mut st = self
            .streaming
            .take()
            .ok_or_else(|| QueryError::internal("missing streaming window state"))?;

        let eval_res = (|| -> Result<Batch, QueryError> {
            let mut key_res = self.ctx.pool.register("window_stream_keys", false);
            let part_extra = if part_sort_keys.is_empty() {
                Vec::new()
            } else {
                eval_extra_sort_keys(&part_sort_keys, &batch, &eval_ctx, &mut key_res)?
            };
            let needs_peers = !self.order_by.is_empty()
                && self
                    .specs
                    .iter()
                    .any(|s| matches!(s.func, WindowFunc::Rank | WindowFunc::DenseRank));
            let order_extra = if needs_peers {
                eval_extra_sort_keys(&self.order_by, &batch, &eval_ctx, &mut key_res)?
            } else {
                Vec::new()
            };

            let mut part_reset = vec![false; n];
            let mut part_row = vec![0usize; n];
            let mut is_new_peer = vec![false; n];

            {
                let part_refs = if part_sort_keys.is_empty() {
                    Vec::new()
                } else {
                    resolve_key_col_refs(&part_sort_keys, &batch, &part_extra)?
                };
                let order_refs = if needs_peers {
                    resolve_key_col_refs(&self.order_by, &batch, &order_extra)?
                } else {
                    Vec::new()
                };

                for row in 0..n {
                    let boundary = if row == 0 {
                        match &st.prev_part_row {
                            None => true,
                            Some(prev_cols) if !part_sort_keys.is_empty() => {
                                let prev_refs: Vec<&Column> = prev_cols.iter().collect();
                                compare_key_refs_across(
                                    &prev_refs,
                                    0,
                                    &part_refs,
                                    0,
                                    &part_sort_keys,
                                ) != Ordering::Equal
                            }
                            Some(_) => false,
                        }
                    } else if !part_sort_keys.is_empty() {
                        compare_key_refs_across(
                            &part_refs,
                            row - 1,
                            &part_refs,
                            row,
                            &part_sort_keys,
                        ) != Ordering::Equal
                    } else {
                        false
                    };

                    if boundary {
                        part_reset[row] = true;
                        st.row_in_partition = 0;
                    }
                    part_row[row] = st.row_in_partition;
                    st.row_in_partition += 1;

                    if needs_peers {
                        is_new_peer[row] = if boundary {
                            true
                        } else if row == 0 {
                            match &st.prev_order_row {
                                None => true,
                                Some(prev_cols) => {
                                    let prev_refs: Vec<&Column> = prev_cols.iter().collect();
                                    compare_key_refs_across(
                                        &prev_refs,
                                        0,
                                        &order_refs,
                                        0,
                                        &self.order_by,
                                    ) != Ordering::Equal
                                }
                            }
                        } else {
                            compare_key_refs_across(
                                &order_refs,
                                row - 1,
                                &order_refs,
                                row,
                                &self.order_by,
                            ) != Ordering::Equal
                        };
                    }
                }

                st.prev_part_row = if part_refs.is_empty() {
                    Some(Vec::new())
                } else {
                    let mut saved = Vec::with_capacity(part_refs.len());
                    for col in &part_refs {
                        saved.push(Column::try_new_const(read_scalar(col, n - 1), 1)?);
                    }
                    Some(saved)
                };
                st.prev_order_row = if needs_peers {
                    let mut saved = Vec::with_capacity(order_refs.len());
                    for col in &order_refs {
                        saved.push(Column::try_new_const(read_scalar(col, n - 1), 1)?);
                    }
                    Some(saved)
                } else {
                    None
                };
            }

            drop(part_extra);
            drop(order_extra);
            drop(key_res);

            let mut out_cols: Vec<Column> = Vec::with_capacity(self.specs.len());
            for (spec_idx, spec) in self.specs.iter().enumerate() {
                if cancel.is_cancelled() {
                    return Err(QueryError::Cancelled);
                }
                let out_dt = &self.out_types[spec_idx];
                let col = eval_streaming_spec_col(
                    &mut st.spec_states[spec_idx],
                    spec,
                    out_dt,
                    &batch,
                    &part_reset,
                    &part_row,
                    &is_new_peer,
                    &eval_ctx,
                    &mut self.res,
                )?;
                out_cols.push(col);
            }

            let mut all_cols = Vec::with_capacity(self.output_schema.len());
            all_cols.extend(batch.columns().iter().cloned());
            all_cols.extend(out_cols.into_iter().map(Arc::new));
            drop(batch);

            Ok(Batch::try_new_shared(
                Arc::clone(&self.output_schema),
                all_cols,
                n,
                Selection::All,
            )?)
        })();

        let keep_bytes = st.state_bytes();
        self.streaming = Some(st);

        let out = match eval_res {
            Ok(b) => b,
            Err(err) => {
                let _ = self.res.resize(keep_bytes);
                rollback_to(&mut caller_res, before);
                return Err(err);
            }
        };

        let out_bytes = out.memory_bytes();
        if let Some(cr) = caller_res.as_deref_mut() {
            let excess = self.res.size().saturating_sub(keep_bytes);
            let xfer = excess.min(out_bytes);
            if xfer > 0
                && let Err(err) = self.res.transfer_to(cr, xfer)
            {
                let _ = self.res.resize(keep_bytes);
                rollback_to(&mut caller_res, before);
                return Err(err.into());
            }
        }
        self.res.resize(keep_bytes)?;
        if let Err(err) = reconcile_output(&mut caller_res, before, out_bytes) {
            rollback_to(&mut caller_res, before);
            return Err(err);
        }
        Ok(Some(out))
    }

    fn read_next_partition(&mut self, cancel: &CancelToken) -> Result<Option<Batch>, QueryError> {
        let first = match self.lookahead.take() {
            Some(b) => b,
            None => match self.pull_next_sorted_batch(cancel)? {
                Some(b) => b,
                None => return Ok(None),
            },
        };

        if self.partition_by.is_empty() {
            // Whole input is one partition.
            let mut parts = vec![first];
            while let Some(b) = self.pull_next_sorted_batch(cancel)? {
                parts.push(b);
            }
            return self.concat_partition_batches(parts);
        }

        let part_sort_keys: Vec<SortKey> = self
            .partition_by
            .iter()
            .map(|e| SortKey {
                expr: e.clone(),
                descending: false,
                nulls_first: false,
            })
            .collect();

        let mut first_key_res = self.ctx.pool.register("window_part_keys", false);
        let first_keys = eval_extra_sort_keys(
            &part_sort_keys,
            &first,
            &self.ctx.eval.with_cancel(cancel.clone()),
            &mut first_key_res,
        )?;
        let n_first = first.num_rows();
        let split_at = {
            let first_key_refs = resolve_key_col_refs(&part_sort_keys, &first, &first_keys)?;
            partition_boundary(&first_key_refs, &part_sort_keys, n_first)
        };
        if split_at < n_first {
            drop(first_keys);
            drop(first_key_res);
            let head = first.slice(0, split_at, Some(&mut self.res))?;
            let tail = first.slice(split_at, n_first - split_at, Some(&mut self.res))?;
            drop(first);
            self.lookahead = Some(tail);
            let target = self.current_held_bytes(head.memory_bytes());
            self.res.resize(target)?;
            return Ok(Some(head));
        }

        // Partition spans at least all of `first`; keep pulling until boundary or EOF.
        let mut parts = vec![first];
        loop {
            let next_opt = self.pull_next_sorted_batch(cancel)?;
            let Some(next_b) = next_opt else {
                drop(first_keys);
                drop(first_key_res);
                let total_parts: usize = parts.iter().map(Batch::memory_bytes).sum();
                let target = self.current_held_bytes(total_parts);
                self.res.resize(target)?;
                break;
            };

            let mut next_key_res = self.ctx.pool.register("window_part_next_keys", false);
            let next_keys = eval_extra_sort_keys(
                &part_sort_keys,
                &next_b,
                &self.ctx.eval.with_cancel(cancel.clone()),
                &mut next_key_res,
            )?;

            let diff_partition = {
                let first_key_refs = resolve_key_col_refs(&part_sort_keys, &parts[0], &first_keys)?;
                let next_key_refs = resolve_key_col_refs(&part_sort_keys, &next_b, &next_keys)?;
                compare_key_refs_across(&first_key_refs, 0, &next_key_refs, 0, &part_sort_keys)
                    != Ordering::Equal
            };

            if diff_partition {
                drop(next_keys);
                drop(next_key_res);
                drop(first_keys);
                drop(first_key_res);
                self.lookahead = Some(next_b);
                let total_parts: usize = parts.iter().map(Batch::memory_bytes).sum();
                let target = self.current_held_bytes(total_parts);
                self.res.resize(target)?;
                break;
            }

            let n_next = next_b.num_rows();
            let split = {
                let next_key_refs = resolve_key_col_refs(&part_sort_keys, &next_b, &next_keys)?;
                partition_boundary(&next_key_refs, &part_sort_keys, n_next)
            };
            drop(next_keys);
            drop(next_key_res);

            if split < n_next {
                drop(first_keys);
                drop(first_key_res);
                let head = next_b.slice(0, split, Some(&mut self.res))?;
                let tail = next_b.slice(split, n_next - split, Some(&mut self.res))?;
                drop(next_b);
                parts.push(head);
                self.lookahead = Some(tail);
                let total_parts: usize = parts.iter().map(Batch::memory_bytes).sum();
                let target = self.current_held_bytes(total_parts);
                self.res.resize(target)?;
                break;
            }

            parts.push(next_b);
        }

        self.concat_partition_batches(parts)
    }

    fn concat_partition_batches(
        &mut self,
        mut parts: Vec<Batch>,
    ) -> Result<Option<Batch>, QueryError> {
        if parts.is_empty() {
            return Ok(None);
        }
        if parts.len() == 1 {
            return Ok(parts.pop());
        }
        let refs: Vec<&Batch> = parts.iter().collect();
        let merged = Batch::concat(Arc::clone(&self.input_schema), &refs, Some(&mut self.res))?;
        drop(parts);
        let target = self.current_held_bytes(merged.memory_bytes());
        self.res.resize(target)?;
        Ok(Some(merged))
    }

    fn eval_partition(
        &mut self,
        part_batch: Batch,
        cancel: &CancelToken,
    ) -> Result<(), QueryError> {
        let n = part_batch.num_rows();
        if n == 0 {
            return Ok(());
        }
        let eval_ctx = self.ctx.eval.with_cancel(cancel.clone());
        let base_bytes = self.current_held_bytes(part_batch.memory_bytes());
        self.res.resize(base_bytes)?;

        // Compute peer group bounds once if any spec needs Rank, DenseRank, or default order_by frame.
        let needs_peers = !self.order_by.is_empty()
            && self.specs.iter().any(|s| {
                matches!(s.func, WindowFunc::Rank | WindowFunc::DenseRank) || s.frame.is_none()
            });
        let peers = if needs_peers {
            let order_extra =
                eval_extra_sort_keys(&self.order_by, &part_batch, &eval_ctx, &mut self.res)?;
            let order_refs = resolve_key_col_refs(&self.order_by, &part_batch, &order_extra)?;
            let peer_info = compute_peers(&order_refs, &self.order_by, n);
            drop(order_refs);
            drop(order_extra);
            self.res.resize(base_bytes)?;
            Some(peer_info)
        } else {
            None
        };

        let mut out_cols: Vec<Column> = Vec::with_capacity(self.specs.len());
        let mut out_cols_bytes = 0usize;

        for (spec_idx, spec) in self.specs.iter().enumerate() {
            if cancel.is_cancelled() {
                return Err(QueryError::Cancelled);
            }
            let out_dt = &self.out_types[spec_idx];
            let col = eval_single_window_spec(
                spec,
                out_dt,
                &part_batch,
                self.order_by.is_empty(),
                peers.as_ref(),
                &eval_ctx,
                &mut self.res,
            )?;
            out_cols_bytes = out_cols_bytes.saturating_add(col.memory_bytes());
            out_cols.push(col);
            self.res.resize(base_bytes + out_cols_bytes)?;
        }

        let mut all_cols = Vec::with_capacity(self.output_schema.len());
        all_cols.extend(part_batch.columns().iter().cloned());
        all_cols.extend(out_cols.into_iter().map(Arc::new));
        drop(part_batch);

        let full_out =
            Batch::try_new_shared(Arc::clone(&self.output_schema), all_cols, n, Selection::All)?;
        let chunk_size = self.ctx.batch_size.max(1);
        if n <= chunk_size {
            self.queued_out.push_back(full_out);
        } else {
            let mut slices = Vec::with_capacity(n.div_ceil(chunk_size));
            let mut offset = 0usize;
            while offset < n {
                let len = (n - offset).min(chunk_size);
                let slice = full_out.slice(offset, len, Some(&mut self.res))?;
                slices.push(slice);
                offset += len;
            }
            drop(full_out);
            self.queued_out.extend(slices);
        }
        self.reconcile_window_res()?;
        Ok(())
    }
}

impl PartitionStream for WindowStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = pool_size(&res);
        if cancel.is_cancelled() {
            rollback_to(&mut res, before);
            return Err(QueryError::Cancelled);
        }

        if let Err(err) = self.ensure_sorted(cancel) {
            rollback_to(&mut res, before);
            return Err(err);
        }

        if self.streaming.is_some() {
            return self.next_streaming_batch(cancel, res);
        }

        while self.queued_out.is_empty() {
            let part_opt = match self.read_next_partition(cancel) {
                Ok(p) => p,
                Err(err) => {
                    let _ = self.reconcile_window_res();
                    rollback_to(&mut res, before);
                    return Err(err);
                }
            };
            let Some(part_batch) = part_opt else {
                self.sorted = None;
                self.reconcile_window_res()?;
                return Ok(None);
            };
            if let Err(err) = self.eval_partition(part_batch, cancel) {
                let _ = self.reconcile_window_res();
                rollback_to(&mut res, before);
                return Err(err);
            }
        }

        let Some(batch) = self.queued_out.pop_front() else {
            return Ok(None);
        };
        let batch_bytes = batch.memory_bytes();
        let keep_bytes = self.current_held_bytes(0);
        if let Some(cr) = res.as_deref_mut() {
            let xfer = self.res.size().saturating_sub(keep_bytes).min(batch_bytes);
            if xfer > 0
                && let Err(err) = self.res.transfer_to(cr, xfer)
            {
                let _ = self.reconcile_window_res();
                rollback_to(&mut res, before);
                return Err(err.into());
            }
        }
        if let Err(err) = self.reconcile_window_res() {
            rollback_to(&mut res, before);
            return Err(err);
        }
        if let Err(err) = reconcile_output(&mut res, before, batch_bytes) {
            rollback_to(&mut res, before);
            return Err(err);
        }
        Ok(Some(batch))
    }
}

fn build_opt_scalar_col(
    out_dt: &DataType,
    vals: &[Option<ScalarValue>],
    eval_ctx: &EvalContext,
    res: &mut Reservation,
    before: usize,
) -> Result<Column, QueryError> {
    let refs: Vec<ElemRef<'_>> = vals
        .iter()
        .map(|opt| match opt {
            Some(sv) if !sv.is_null() => ElemRef::Scalar(sv),
            _ => ElemRef::Null,
        })
        .collect();
    eval::build_flat_from_elem_refs(out_dt, &refs, &mut Some(res), eval_ctx, before)
}

fn col_access_owned_bytes(access: &ColAccess<'_>) -> usize {
    match access {
        ColAccess::Borrowed(_) => 0,
        ColAccess::Owned(col) => col.memory_bytes(),
    }
}

fn push_ring_val(
    ring: &mut VecDeque<Option<ScalarValue>>,
    val: Option<ScalarValue>,
    res: &mut Reservation,
) -> Result<(), QueryError> {
    let bytes = opt_scalar_bytes(&val);
    res.try_grow(bytes)?;
    ring.push_back(val);
    Ok(())
}

fn pop_ring_val(
    ring: &mut VecDeque<Option<ScalarValue>>,
    res: &mut Reservation,
) -> Option<Option<ScalarValue>> {
    let popped = ring.pop_front()?;
    let bytes = opt_scalar_bytes(&popped);
    res.shrink(res.size().min(bytes));
    Some(popped)
}

fn clear_ring_vals(ring: &mut VecDeque<Option<ScalarValue>>, res: &mut Reservation) {
    let bytes: usize = ring.iter().map(opt_scalar_bytes).sum();
    ring.clear();
    res.shrink(res.size().min(bytes));
}

fn push_out_val(
    out_vals: &mut Vec<Option<ScalarValue>>,
    val: Option<ScalarValue>,
    res: &mut Reservation,
) -> Result<(), QueryError> {
    let bytes = opt_scalar_bytes(&val);
    res.try_grow(bytes)?;
    out_vals.push(val);
    Ok(())
}

fn update_res_delta(
    res: &mut Reservation,
    old_bytes: usize,
    new_bytes: usize,
) -> Result<(), QueryError> {
    if new_bytes > old_bytes {
        res.try_grow(new_bytes - old_bytes)?;
    } else if old_bytes > new_bytes {
        res.shrink(res.size().min(old_bytes - new_bytes));
    }
    Ok(())
}

fn finish_streaming_opt_col(
    out_dt: &DataType,
    out_vals: Vec<Option<ScalarValue>>,
    eval_ctx: &EvalContext,
    res: &mut Reservation,
    before: usize,
    state_before: usize,
    state_after: usize,
) -> Result<Column, QueryError> {
    let out_vals_bytes: usize = out_vals.iter().map(opt_scalar_bytes).sum();
    let col = build_opt_scalar_col(out_dt, &out_vals, eval_ctx, res, res.size())?;
    drop(out_vals);
    res.shrink(res.size().min(out_vals_bytes));
    let target_before_col = before
        .saturating_sub(state_before)
        .saturating_add(state_after);
    reconcile_output(&mut Some(res), target_before_col, col.memory_bytes())?;
    Ok(col)
}

#[expect(
    clippy::too_many_arguments,
    reason = "streaming window spec evaluator needs batch, partition boundaries, peer flags, and reservation"
)]
fn eval_streaming_spec_col(
    state: &mut StreamingSpecState,
    spec: &WindowSpec,
    out_dt: &DataType,
    batch: &Batch,
    part_reset: &[bool],
    part_row: &[usize],
    is_new_peer: &[bool],
    eval_ctx: &EvalContext,
    res: &mut Reservation,
) -> Result<Column, QueryError> {
    let n = batch.num_rows();
    let before = res.size();
    let state_before = state.heap_bytes();

    match (state, &spec.func) {
        (StreamingSpecState::RowNumber, WindowFunc::RowNumber) => {
            res.try_grow(eval::checked_fixed_bytes(n, 8, false)?)?;
            let vals: Vec<i64> = part_row.iter().map(|&r| (r + 1) as i64).collect();
            let col = Column::Flat(FlatColumn::try_new(FlatData::I64(vals), None)?);
            reconcile_output(&mut Some(res), before, col.memory_bytes())?;
            Ok(col)
        }
        (StreamingSpecState::Rank { current_rank }, WindowFunc::Rank) => {
            res.try_grow(eval::checked_fixed_bytes(n, 8, false)?)?;
            let mut vals = Vec::with_capacity(n);
            for row in 0..n {
                if part_reset[row] {
                    *current_rank = 1;
                } else if is_new_peer[row] {
                    *current_rank = (part_row[row] + 1) as i64;
                }
                vals.push(*current_rank);
            }
            let col = Column::Flat(FlatColumn::try_new(FlatData::I64(vals), None)?);
            reconcile_output(&mut Some(res), before, col.memory_bytes())?;
            Ok(col)
        }
        (StreamingSpecState::DenseRank { current_dense_rank }, WindowFunc::DenseRank) => {
            res.try_grow(eval::checked_fixed_bytes(n, 8, false)?)?;
            let mut vals = Vec::with_capacity(n);
            for row in 0..n {
                if part_reset[row] {
                    *current_dense_rank = 1;
                } else if is_new_peer[row] {
                    *current_dense_rank += 1;
                }
                vals.push(*current_dense_rank);
            }
            let col = Column::Flat(FlatColumn::try_new(FlatData::I64(vals), None)?);
            reconcile_output(&mut Some(res), before, col.memory_bytes())?;
            Ok(col)
        }
        (StreamingSpecState::Lag { offset, ring }, WindowFunc::Lag { expr, default, .. }) => {
            let val_access = eval_or_borrow(expr, batch, eval_ctx, res)?;
            let def_access = match default {
                Some(d) => Some(eval_or_borrow(d, batch, eval_ctx, res)?),
                None => None,
            };
            let val_bytes = col_access_owned_bytes(&val_access);
            let def_bytes = def_access.as_ref().map_or(0, col_access_owned_bytes);
            let val_col = val_access.as_col();
            let def_col = def_access.as_ref().map(ColAccess::as_col);
            let mut out_vals: Vec<Option<ScalarValue>> = Vec::with_capacity(n);
            for row in 0..n {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(eval_ctx, &mut Some(res), before)?;
                }
                if part_reset[row] {
                    clear_ring_vals(ring, res);
                }
                let cur_val = (!val_col.is_null(row)).then(|| read_scalar(val_col, row));
                if *offset == 0 {
                    push_out_val(&mut out_vals, cur_val, res)?;
                } else {
                    let out = if part_row[row] < *offset {
                        def_col.and_then(|dc| (!dc.is_null(row)).then(|| read_scalar(dc, row)))
                    } else {
                        pop_ring_val(ring, res).flatten()
                    };
                    push_ring_val(ring, cur_val, res)?;
                    push_out_val(&mut out_vals, out, res)?;
                }
            }
            drop(def_access);
            drop(val_access);
            res.shrink(res.size().min(val_bytes.saturating_add(def_bytes)));
            let state_after: usize = ring.iter().map(opt_scalar_bytes).sum();
            finish_streaming_opt_col(
                out_dt,
                out_vals,
                eval_ctx,
                res,
                before,
                state_before,
                state_after,
            )
        }
        (
            StreamingSpecState::FrameFirstLast {
                is_last,
                start,
                end,
                first_val,
                ring,
                ring_start,
            },
            WindowFunc::FirstValue(expr) | WindowFunc::LastValue(expr),
        ) => {
            let val_access = eval_or_borrow(expr, batch, eval_ctx, res)?;
            let val_bytes = col_access_owned_bytes(&val_access);
            let val_col = val_access.as_col();
            let mut out_vals: Vec<Option<ScalarValue>> = Vec::with_capacity(n);
            for row in 0..n {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(eval_ctx, &mut Some(res), before)?;
                }
                if part_reset[row] {
                    if let Some(old_fv) = first_val.take() {
                        res.shrink(res.size().min(opt_scalar_bytes(&old_fv)));
                    }
                    clear_ring_vals(ring, res);
                    *ring_start = 0;
                }
                let cur_val = (!val_col.is_null(row)).then(|| read_scalar(val_col, row));
                let p_row = part_row[row];
                let out = match (*start, *end) {
                    (FrameBound::UnboundedPreceding, FrameBound::CurrentRow) => {
                        if p_row == 0 {
                            let old_b = first_val.as_ref().map_or(0, opt_scalar_bytes);
                            let next_fv = Some(cur_val.clone());
                            let new_b = next_fv.as_ref().map_or(0, opt_scalar_bytes);
                            update_res_delta(res, old_b, new_b)?;
                            *first_val = next_fv;
                        }
                        if *is_last {
                            cur_val
                        } else {
                            first_val.as_ref().cloned().flatten()
                        }
                    }
                    (FrameBound::UnboundedPreceding, FrameBound::Preceding(k)) => {
                        if p_row == 0 {
                            let old_b = first_val.as_ref().map_or(0, opt_scalar_bytes);
                            let next_fv = Some(cur_val.clone());
                            let new_b = next_fv.as_ref().map_or(0, opt_scalar_bytes);
                            update_res_delta(res, old_b, new_b)?;
                            *first_val = next_fv;
                        }
                        let delay = k as usize;
                        push_ring_val(ring, cur_val, res)?;
                        if p_row < delay {
                            None
                        } else {
                            let end_val = pop_ring_val(ring, res).flatten();
                            if *is_last {
                                end_val
                            } else {
                                first_val.as_ref().cloned().flatten()
                            }
                        }
                    }
                    (FrameBound::CurrentRow, FrameBound::CurrentRow) => cur_val,
                    (FrameBound::CurrentRow, FrameBound::Preceding(_)) => None,
                    (FrameBound::Preceding(m), _) => {
                        let max_keep = (m as usize) + 1;
                        push_ring_val(ring, cur_val, res)?;
                        if ring.len() > max_keep {
                            pop_ring_val(ring, res);
                            *ring_start += 1;
                        }
                        match stream_frame_bounds(*start, *end, p_row) {
                            None => None,
                            Some((s, e)) => {
                                let idx = if *is_last { e - 1 } else { s };
                                ring[idx - *ring_start].clone()
                            }
                        }
                    }
                    _ => {
                        return Err(QueryError::internal(
                            "non-streamable frame in streaming path",
                        ));
                    }
                };
                push_out_val(&mut out_vals, out, res)?;
            }
            drop(val_access);
            res.shrink(res.size().min(val_bytes));
            let state_after = first_val.as_ref().map_or(0, opt_scalar_bytes)
                + ring.iter().map(opt_scalar_bytes).sum::<usize>();
            finish_streaming_opt_col(
                out_dt,
                out_vals,
                eval_ctx,
                res,
                before,
                state_before,
                state_after,
            )
        }
        (
            StreamingSpecState::FrameCount {
                has_expr,
                start,
                end,
                prefix,
                ring,
                ring_start,
            },
            WindowFunc::Count(expr_opt),
        ) => {
            let val_access = match (has_expr, expr_opt) {
                (true, Some(e)) => Some(eval_or_borrow(e, batch, eval_ctx, res)?),
                _ => None,
            };
            res.try_grow(eval::checked_fixed_bytes(n, 8, false)?)?;
            let mut counts = Vec::with_capacity(n);
            if let Some(va) = &val_access {
                let col = va.as_col();
                for row in 0..n {
                    if (row & CANCEL_CHECK_MASK) == 0 {
                        check_cancel(eval_ctx, &mut Some(res), before)?;
                    }
                    if part_reset[row] {
                        *prefix = 0;
                        ring.clear();
                        *ring_start = 0;
                    }
                    let non_null = !col.is_null(row);
                    let p_row = part_row[row];
                    let c = match (*start, *end) {
                        (FrameBound::UnboundedPreceding, FrameBound::CurrentRow) => {
                            if non_null {
                                *prefix += 1;
                            }
                            *prefix
                        }
                        (FrameBound::UnboundedPreceding, FrameBound::Preceding(k)) => {
                            let delay = k as usize;
                            ring.push_back(non_null);
                            if p_row < delay {
                                0
                            } else {
                                if ring.pop_front() == Some(true) {
                                    *prefix += 1;
                                }
                                *prefix
                            }
                        }
                        (FrameBound::CurrentRow, FrameBound::CurrentRow) => i64::from(non_null),
                        (FrameBound::CurrentRow, FrameBound::Preceding(_)) => 0,
                        (FrameBound::Preceding(m), _) => {
                            let max_keep = (m as usize) + 1;
                            ring.push_back(non_null);
                            if ring.len() > max_keep {
                                ring.pop_front();
                                *ring_start += 1;
                            }
                            match stream_frame_bounds(*start, *end, p_row) {
                                None => 0,
                                Some((s, e)) => ring
                                    .range((s - *ring_start)..(e - *ring_start))
                                    .filter(|&&b| b)
                                    .count() as i64,
                            }
                        }
                        _ => return Err(QueryError::internal("non-streamable frame in COUNT")),
                    };
                    counts.push(c);
                }
            } else {
                for &p_row in part_row {
                    let c = match stream_frame_bounds(*start, *end, p_row) {
                        None => 0,
                        Some((s, e)) => (e - s) as i64,
                    };
                    counts.push(c);
                }
            }
            drop(val_access);
            let col = Column::Flat(FlatColumn::try_new(FlatData::I64(counts), None)?);
            reconcile_output(&mut Some(res), before, col.memory_bytes())?;
            Ok(col)
        }
        (
            StreamingSpecState::FrameSum {
                start,
                end,
                acc,
                ring,
                ring_start,
            },
            WindowFunc::Sum(expr),
        ) => {
            let val_access = eval_or_borrow(expr, batch, eval_ctx, res)?;
            let val_bytes = col_access_owned_bytes(&val_access);
            let col = val_access.as_col();
            let mut out_vals: Vec<Option<ScalarValue>> = Vec::with_capacity(n);
            for row in 0..n {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(eval_ctx, &mut Some(res), before)?;
                }
                if part_reset[row] {
                    acc.reset();
                    clear_ring_vals(ring, res);
                    *ring_start = 0;
                }
                let cur_val = (!col.is_null(row)).then(|| read_scalar(col, row));
                let p_row = part_row[row];
                let out = match (*start, *end) {
                    (FrameBound::UnboundedPreceding, FrameBound::CurrentRow) => {
                        acc.add_scalar(cur_val.as_ref())?;
                        acc.to_scalar()
                    }
                    (FrameBound::UnboundedPreceding, FrameBound::Preceding(k)) => {
                        let delay = k as usize;
                        push_ring_val(ring, cur_val, res)?;
                        if p_row < delay {
                            None
                        } else {
                            let old = pop_ring_val(ring, res).flatten();
                            acc.add_scalar(old.as_ref())?;
                            acc.to_scalar()
                        }
                    }
                    (FrameBound::CurrentRow, FrameBound::CurrentRow) => {
                        acc.reset();
                        acc.add_scalar(cur_val.as_ref())?;
                        acc.to_scalar()
                    }
                    (FrameBound::CurrentRow, FrameBound::Preceding(_)) => None,
                    (FrameBound::Preceding(m), _) => {
                        let max_keep = (m as usize) + 1;
                        push_ring_val(ring, cur_val, res)?;
                        if ring.len() > max_keep {
                            pop_ring_val(ring, res);
                            *ring_start += 1;
                        }
                        match stream_frame_bounds(*start, *end, p_row) {
                            None => None,
                            Some((s, e)) => {
                                acc.reset();
                                for v in ring.range((s - *ring_start)..(e - *ring_start)) {
                                    acc.add_scalar(v.as_ref())?;
                                }
                                acc.to_scalar()
                            }
                        }
                    }
                    _ => return Err(QueryError::internal("non-streamable frame in SUM")),
                };
                push_out_val(&mut out_vals, out, res)?;
            }
            drop(val_access);
            res.shrink(res.size().min(val_bytes));
            let state_after: usize = ring.iter().map(opt_scalar_bytes).sum();
            finish_streaming_opt_col(
                out_dt,
                out_vals,
                eval_ctx,
                res,
                before,
                state_before,
                state_after,
            )
        }
        (
            StreamingSpecState::FrameAvg {
                start,
                end,
                acc,
                ring,
                ring_start,
            },
            WindowFunc::Avg(expr),
        ) => {
            let val_access = eval_or_borrow(expr, batch, eval_ctx, res)?;
            let val_bytes = col_access_owned_bytes(&val_access);
            let col = val_access.as_col();
            let mut out_vals: Vec<Option<ScalarValue>> = Vec::with_capacity(n);
            for row in 0..n {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(eval_ctx, &mut Some(res), before)?;
                }
                if part_reset[row] {
                    acc.reset();
                    clear_ring_vals(ring, res);
                    *ring_start = 0;
                }
                let cur_val = (!col.is_null(row)).then(|| read_scalar(col, row));
                let p_row = part_row[row];
                let out = match (*start, *end) {
                    (FrameBound::UnboundedPreceding, FrameBound::CurrentRow) => {
                        acc.add_scalar(cur_val.as_ref())?;
                        acc.to_scalar()?
                    }
                    (FrameBound::UnboundedPreceding, FrameBound::Preceding(k)) => {
                        let delay = k as usize;
                        push_ring_val(ring, cur_val, res)?;
                        if p_row < delay {
                            None
                        } else {
                            let old = pop_ring_val(ring, res).flatten();
                            acc.add_scalar(old.as_ref())?;
                            acc.to_scalar()?
                        }
                    }
                    (FrameBound::CurrentRow, FrameBound::CurrentRow) => {
                        acc.reset();
                        acc.add_scalar(cur_val.as_ref())?;
                        acc.to_scalar()?
                    }
                    (FrameBound::CurrentRow, FrameBound::Preceding(_)) => None,
                    (FrameBound::Preceding(m), _) => {
                        let max_keep = (m as usize) + 1;
                        push_ring_val(ring, cur_val, res)?;
                        if ring.len() > max_keep {
                            pop_ring_val(ring, res);
                            *ring_start += 1;
                        }
                        match stream_frame_bounds(*start, *end, p_row) {
                            None => None,
                            Some((s, e)) => {
                                acc.reset();
                                for v in ring.range((s - *ring_start)..(e - *ring_start)) {
                                    acc.add_scalar(v.as_ref())?;
                                }
                                acc.to_scalar()?
                            }
                        }
                    }
                    _ => return Err(QueryError::internal("non-streamable frame in AVG")),
                };
                push_out_val(&mut out_vals, out, res)?;
            }
            drop(val_access);
            res.shrink(res.size().min(val_bytes));
            let state_after: usize = ring.iter().map(opt_scalar_bytes).sum();
            finish_streaming_opt_col(
                out_dt,
                out_vals,
                eval_ctx,
                res,
                before,
                state_before,
                state_after,
            )
        }
        (
            StreamingSpecState::FrameMinMax {
                is_max,
                start,
                end,
                best,
                ring,
                ring_start,
            },
            WindowFunc::Min(expr) | WindowFunc::Max(expr),
        ) => {
            let val_access = eval_or_borrow(expr, batch, eval_ctx, res)?;
            let val_bytes = col_access_owned_bytes(&val_access);
            let col = val_access.as_col();
            let mut out_vals: Vec<Option<ScalarValue>> = Vec::with_capacity(n);
            for row in 0..n {
                if (row & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(eval_ctx, &mut Some(res), before)?;
                }
                if part_reset[row] {
                    if let Some(old_best) = best.take() {
                        res.shrink(res.size().min(old_best.heap_bytes()));
                    }
                    clear_ring_vals(ring, res);
                    *ring_start = 0;
                }
                let cur_val = (!col.is_null(row)).then(|| read_scalar(col, row));
                let p_row = part_row[row];
                let out = match (*start, *end) {
                    (FrameBound::UnboundedPreceding, FrameBound::CurrentRow) => {
                        if let Some(cand) = cur_val {
                            let take = best
                                .as_ref()
                                .is_none_or(|cur| min_max_better(&cand, cur, *is_max));
                            if take {
                                let old_b = best.as_ref().map_or(0, ScalarValue::heap_bytes);
                                let new_b = cand.heap_bytes();
                                update_res_delta(res, old_b, new_b)?;
                                *best = Some(cand);
                            }
                        }
                        best.clone()
                    }
                    (FrameBound::UnboundedPreceding, FrameBound::Preceding(k)) => {
                        let delay = k as usize;
                        push_ring_val(ring, cur_val, res)?;
                        if p_row < delay {
                            None
                        } else {
                            if let Some(cand) = pop_ring_val(ring, res).flatten() {
                                let take = best
                                    .as_ref()
                                    .is_none_or(|cur| min_max_better(&cand, cur, *is_max));
                                if take {
                                    let old_b = best.as_ref().map_or(0, ScalarValue::heap_bytes);
                                    let new_b = cand.heap_bytes();
                                    update_res_delta(res, old_b, new_b)?;
                                    *best = Some(cand);
                                }
                            }
                            best.clone()
                        }
                    }
                    (FrameBound::CurrentRow, FrameBound::CurrentRow) => cur_val,
                    (FrameBound::CurrentRow, FrameBound::Preceding(_)) => None,
                    (FrameBound::Preceding(m), _) => {
                        let max_keep = (m as usize) + 1;
                        push_ring_val(ring, cur_val, res)?;
                        if ring.len() > max_keep {
                            pop_ring_val(ring, res);
                            *ring_start += 1;
                        }
                        match stream_frame_bounds(*start, *end, p_row) {
                            None => None,
                            Some((s, e)) => {
                                let mut local_best: Option<&ScalarValue> = None;
                                for cand in
                                    ring.range((s - *ring_start)..(e - *ring_start)).flatten()
                                {
                                    let take = local_best
                                        .is_none_or(|cur| min_max_better(cand, cur, *is_max));
                                    if take {
                                        local_best = Some(cand);
                                    }
                                }
                                local_best.cloned()
                            }
                        }
                    }
                    _ => return Err(QueryError::internal("non-streamable frame in MIN/MAX")),
                };
                push_out_val(&mut out_vals, out, res)?;
            }
            drop(val_access);
            res.shrink(res.size().min(val_bytes));
            let state_after = best.as_ref().map_or(0, ScalarValue::heap_bytes)
                + ring.iter().map(opt_scalar_bytes).sum::<usize>();
            finish_streaming_opt_col(
                out_dt,
                out_vals,
                eval_ctx,
                res,
                before,
                state_before,
                state_after,
            )
        }
        _ => Err(QueryError::internal("streaming window state mismatch")),
    }
}

/// Binary searches for the first row index in `0..len` whose partition key differs from row `0`.
fn partition_boundary(key_refs: &[&Column], keys: &[SortKey], len: usize) -> usize {
    let mut lo = 1usize;
    let mut hi = len;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if compare_key_refs_across(key_refs, 0, key_refs, mid, keys) == Ordering::Equal {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

struct PeerInfo {
    /// Start row index of the peer group containing row `i`.
    peer_start: Vec<usize>,
    /// Exclusive end row index of the peer group containing row `i`.
    peer_end_excl: Vec<usize>,
    /// 0-based dense peer group index for row `i`.
    dense_group: Vec<usize>,
}

fn compute_peers(order_refs: &[&Column], order_by: &[SortKey], n: usize) -> PeerInfo {
    let mut peer_start = vec![0usize; n];
    let mut peer_end_excl = vec![n; n];
    let mut dense_group = vec![0usize; n];
    if n == 0 {
        return PeerInfo {
            peer_start,
            peer_end_excl,
            dense_group,
        };
    }

    let mut cur_start = 0usize;
    let mut cur_group = 0usize;
    for i in 1..n {
        let same =
            compare_key_refs_across(order_refs, i - 1, order_refs, i, order_by) == Ordering::Equal;
        if !same {
            for slot in &mut peer_end_excl[cur_start..i] {
                *slot = i;
            }
            cur_start = i;
            cur_group += 1;
        }
        peer_start[i] = cur_start;
        dense_group[i] = cur_group;
    }
    for slot in &mut peer_end_excl[cur_start..n] {
        *slot = n;
    }

    PeerInfo {
        peer_start,
        peer_end_excl,
        dense_group,
    }
}

enum ColAccess<'a> {
    Borrowed(&'a Column),
    Owned(Arc<Column>),
}

impl ColAccess<'_> {
    fn as_col(&self) -> &Column {
        match self {
            Self::Borrowed(c) => c,
            Self::Owned(c) => c,
        }
    }
}

fn eval_or_borrow<'a>(
    expr: &Expr,
    batch: &'a Batch,
    eval_ctx: &EvalContext,
    res: &mut Reservation,
) -> Result<ColAccess<'a>, QueryError> {
    if let Expr::Column {
        index, data_type, ..
    } = expr
    {
        let col = batch.column(*index).ok_or(ColumnError::InvalidBatch {
            reason: "window expression column index out of bounds",
        })?;
        if col.data_type() != *data_type {
            return Err(ColumnError::InvalidBatch {
                reason: "window expression column data type mismatch",
            }
            .into());
        }
        Ok(ColAccess::Borrowed(col))
    } else {
        let col = eval_expr(expr, batch, eval_ctx, Some(res))?;
        Ok(ColAccess::Owned(col))
    }
}

fn compute_row_frame_bounds(
    frame: Option<(FrameBound, FrameBound)>,
    order_empty: bool,
    peers: Option<&PeerInfo>,
    row: usize,
    n: usize,
) -> (usize, usize) {
    match frame {
        None => {
            if order_empty {
                (0, n)
            } else if let Some(p) = peers {
                (0, p.peer_end_excl[row])
            } else {
                (0, row + 1)
            }
        }
        Some((start_b, end_b)) => {
            let n_i128 = n as i128;
            let r_i128 = row as i128;
            let lo = match start_b {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::Preceding(k) => r_i128 - (k as i128),
                FrameBound::CurrentRow => r_i128,
                FrameBound::Following(k) => r_i128 + (k as i128),
                FrameBound::UnboundedFollowing => n_i128 - 1,
            };
            let hi = match end_b {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::Preceding(k) => r_i128 - (k as i128),
                FrameBound::CurrentRow => r_i128,
                FrameBound::Following(k) => r_i128 + (k as i128),
                FrameBound::UnboundedFollowing => n_i128 - 1,
            };
            let s = lo.max(0);
            let e = hi.min(n_i128 - 1);
            if s > e {
                (0, 0)
            } else {
                (s as usize, (e as usize) + 1)
            }
        }
    }
}

fn eval_single_window_spec(
    spec: &WindowSpec,
    out_dt: &DataType,
    batch: &Batch,
    order_empty: bool,
    peers: Option<&PeerInfo>,
    eval_ctx: &EvalContext,
    res: &mut Reservation,
) -> Result<Column, QueryError> {
    let n = batch.num_rows();
    let before = res.size();

    match &spec.func {
        WindowFunc::RowNumber => {
            res.try_grow(eval::checked_fixed_bytes(n, 8, false)?)?;
            let vals: Vec<i64> = (1..=n as i64).collect();
            let col = Column::Flat(FlatColumn::try_new(FlatData::I64(vals), None)?);
            reconcile_output(&mut Some(res), before, col.memory_bytes())?;
            Ok(col)
        }
        WindowFunc::Rank => {
            res.try_grow(eval::checked_fixed_bytes(n, 8, false)?)?;
            let vals: Vec<i64> = if let Some(p) = peers {
                (0..n).map(|i| (p.peer_start[i] + 1) as i64).collect()
            } else {
                vec![1i64; n]
            };
            let col = Column::Flat(FlatColumn::try_new(FlatData::I64(vals), None)?);
            reconcile_output(&mut Some(res), before, col.memory_bytes())?;
            Ok(col)
        }
        WindowFunc::DenseRank => {
            res.try_grow(eval::checked_fixed_bytes(n, 8, false)?)?;
            let vals: Vec<i64> = if let Some(p) = peers {
                (0..n).map(|i| (p.dense_group[i] + 1) as i64).collect()
            } else {
                vec![1i64; n]
            };
            let col = Column::Flat(FlatColumn::try_new(FlatData::I64(vals), None)?);
            reconcile_output(&mut Some(res), before, col.memory_bytes())?;
            Ok(col)
        }
        WindowFunc::Lag {
            expr,
            offset,
            default,
        } => eval_lag_lead(
            expr,
            -(*offset as i128),
            default.as_ref(),
            out_dt,
            batch,
            eval_ctx,
            res,
        ),
        WindowFunc::Lead {
            expr,
            offset,
            default,
        } => eval_lag_lead(
            expr,
            *offset as i128,
            default.as_ref(),
            out_dt,
            batch,
            eval_ctx,
            res,
        ),
        WindowFunc::FirstValue(expr) => {
            let val_access = eval_or_borrow(expr, batch, eval_ctx, res)?;
            let val_col = val_access.as_col();
            let mut refs = Vec::with_capacity(n);
            for i in 0..n {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(eval_ctx, &mut Some(res), before)?;
                }
                let (s, e) = compute_row_frame_bounds(spec.frame, order_empty, peers, i, n);
                if s < e && !val_col.is_null(s) {
                    refs.push(ElemRef::Col(val_col, s));
                } else {
                    refs.push(ElemRef::Null);
                }
            }
            let build_before = res.size();
            let col = eval::build_flat_from_elem_refs(
                out_dt,
                &refs,
                &mut Some(res),
                eval_ctx,
                build_before,
            )?;
            drop(val_access);
            reconcile_output(&mut Some(res), before, col.memory_bytes())?;
            Ok(col)
        }
        WindowFunc::LastValue(expr) => {
            let val_access = eval_or_borrow(expr, batch, eval_ctx, res)?;
            let val_col = val_access.as_col();
            let mut refs = Vec::with_capacity(n);
            for i in 0..n {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(eval_ctx, &mut Some(res), before)?;
                }
                let (s, e) = compute_row_frame_bounds(spec.frame, order_empty, peers, i, n);
                if s < e && !val_col.is_null(e - 1) {
                    refs.push(ElemRef::Col(val_col, e - 1));
                } else {
                    refs.push(ElemRef::Null);
                }
            }
            let build_before = res.size();
            let col = eval::build_flat_from_elem_refs(
                out_dt,
                &refs,
                &mut Some(res),
                eval_ctx,
                build_before,
            )?;
            drop(val_access);
            reconcile_output(&mut Some(res), before, col.memory_bytes())?;
            Ok(col)
        }
        WindowFunc::Count(expr_opt) => eval_window_count(
            expr_opt.as_ref(),
            spec.frame,
            order_empty,
            peers,
            batch,
            eval_ctx,
            res,
        ),
        WindowFunc::Sum(expr) => eval_window_sum(
            expr,
            out_dt,
            spec.frame,
            order_empty,
            peers,
            batch,
            eval_ctx,
            res,
        ),
        WindowFunc::Avg(expr) => eval_window_avg(
            expr,
            out_dt,
            spec.frame,
            order_empty,
            peers,
            batch,
            eval_ctx,
            res,
        ),
        WindowFunc::Min(expr) => eval_window_min_max(
            expr,
            out_dt,
            spec.frame,
            order_empty,
            peers,
            batch,
            eval_ctx,
            res,
            false,
        ),
        WindowFunc::Max(expr) => eval_window_min_max(
            expr,
            out_dt,
            spec.frame,
            order_empty,
            peers,
            batch,
            eval_ctx,
            res,
            true,
        ),
    }
}

fn eval_lag_lead(
    expr: &Expr,
    signed_delta: i128,
    default: Option<&Expr>,
    out_dt: &DataType,
    batch: &Batch,
    eval_ctx: &EvalContext,
    res: &mut Reservation,
) -> Result<Column, QueryError> {
    let n = batch.num_rows();
    let before = res.size();
    let val_access = eval_or_borrow(expr, batch, eval_ctx, res)?;
    let def_access = match default {
        Some(d) => Some(eval_or_borrow(d, batch, eval_ctx, res)?),
        None => None,
    };

    let val_col = val_access.as_col();
    let def_col = def_access.as_ref().map(ColAccess::as_col);
    let mut refs = Vec::with_capacity(n);

    for i in 0..n {
        if (i & CANCEL_CHECK_MASK) == 0 {
            check_cancel(eval_ctx, &mut Some(res), before)?;
        }
        let target = (i as i128) + signed_delta;
        if target >= 0 && target < (n as i128) {
            let t = target as usize;
            if val_col.is_null(t) {
                refs.push(ElemRef::Null);
            } else {
                refs.push(ElemRef::Col(val_col, t));
            }
        } else if let Some(dc) = def_col {
            if dc.is_null(i) {
                refs.push(ElemRef::Null);
            } else {
                refs.push(ElemRef::Col(dc, i));
            }
        } else {
            refs.push(ElemRef::Null);
        }
    }

    let build_before = res.size();
    let col =
        eval::build_flat_from_elem_refs(out_dt, &refs, &mut Some(res), eval_ctx, build_before)?;
    drop(def_access);
    drop(val_access);
    reconcile_output(&mut Some(res), before, col.memory_bytes())?;
    Ok(col)
}

fn is_unbounded_prefix_frame(frame: Option<(FrameBound, FrameBound)>) -> bool {
    matches!(frame, None | Some((FrameBound::UnboundedPreceding, _)))
}

fn eval_window_count(
    expr_opt: Option<&Expr>,
    frame: Option<(FrameBound, FrameBound)>,
    order_empty: bool,
    peers: Option<&PeerInfo>,
    batch: &Batch,
    eval_ctx: &EvalContext,
    res: &mut Reservation,
) -> Result<Column, QueryError> {
    let n = batch.num_rows();
    let before = res.size();
    let val_access = match expr_opt {
        Some(e) => Some(eval_or_borrow(e, batch, eval_ctx, res)?),
        None => None,
    };
    res.try_grow(eval::checked_fixed_bytes(n, 8, false)?)?;
    let mut counts = vec![0i64; n];

    if let Some(va) = &val_access {
        let col = va.as_col();
        if is_unbounded_prefix_frame(frame) {
            let mut prefix = 0i64;
            let mut pos = 0usize;
            for (i, slot) in counts.iter_mut().enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(eval_ctx, &mut Some(res), before)?;
                }
                let (_, e) = compute_row_frame_bounds(frame, order_empty, peers, i, n);
                while pos < e {
                    if (pos & CANCEL_CHECK_MASK) == 0 {
                        check_cancel(eval_ctx, &mut Some(res), before)?;
                    }
                    if !col.is_null(pos) {
                        prefix += 1;
                    }
                    pos += 1;
                }
                *slot = prefix;
            }
        } else {
            for (i, slot) in counts.iter_mut().enumerate() {
                if (i & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(eval_ctx, &mut Some(res), before)?;
                }
                let (s, e) = compute_row_frame_bounds(frame, order_empty, peers, i, n);
                let mut c = 0i64;
                for r in s..e {
                    if (r & CANCEL_CHECK_MASK) == 0 {
                        check_cancel(eval_ctx, &mut Some(res), before)?;
                    }
                    if !col.is_null(r) {
                        c += 1;
                    }
                }
                *slot = c;
            }
        }
    } else {
        for (i, slot) in counts.iter_mut().enumerate() {
            if (i & CANCEL_CHECK_MASK) == 0 {
                check_cancel(eval_ctx, &mut Some(res), before)?;
            }
            let (s, e) = compute_row_frame_bounds(frame, order_empty, peers, i, n);
            *slot = (e - s) as i64;
        }
    }

    drop(val_access);
    let col = Column::Flat(FlatColumn::try_new(FlatData::I64(counts), None)?);
    reconcile_output(&mut Some(res), before, col.memory_bytes())?;
    Ok(col)
}

#[expect(
    clippy::too_many_arguments,
    reason = "window sum shares frame/peer/reservation parameters"
)]
fn eval_window_sum(
    expr: &Expr,
    out_dt: &DataType,
    frame: Option<(FrameBound, FrameBound)>,
    order_empty: bool,
    peers: Option<&PeerInfo>,
    batch: &Batch,
    eval_ctx: &EvalContext,
    res: &mut Reservation,
) -> Result<Column, QueryError> {
    let n = batch.num_rows();
    let before = res.size();
    let val_access = eval_or_borrow(expr, batch, eval_ctx, res)?;
    let col = val_access.as_col();

    let out = match out_dt {
        DataType::Null => Column::Const {
            value: ScalarValue::Null(DataType::Null),
            len: n,
        },
        DataType::I64 => {
            res.try_grow(eval::checked_fixed_bytes(n, 8, true)?)?;
            let mut vals = vec![0i64; n];
            let mut nulls = Bitmap::new_zeroed(n);
            if is_unbounded_prefix_frame(frame) {
                let mut acc: Option<i64> = None;
                let mut pos = 0usize;
                for (i, slot) in vals.iter_mut().enumerate() {
                    if (i & CANCEL_CHECK_MASK) == 0 {
                        check_cancel(eval_ctx, &mut Some(res), before)?;
                    }
                    let (_, e) = compute_row_frame_bounds(frame, order_empty, peers, i, n);
                    while pos < e {
                        if (pos & CANCEL_CHECK_MASK) == 0 {
                            check_cancel(eval_ctx, &mut Some(res), before)?;
                        }
                        if let Some(v) = eval::read_i64(col, pos) {
                            acc = Some(match acc {
                                Some(cur) => cur.checked_add(v).ok_or_else(|| {
                                    QueryError::failed("window SUM(I64) overflow")
                                })?,
                                None => v,
                            });
                        }
                        pos += 1;
                    }
                    if let Some(s) = acc {
                        *slot = s;
                        let _ = nulls.set(i, true);
                    }
                }
            } else {
                for (i, slot) in vals.iter_mut().enumerate() {
                    if (i & CANCEL_CHECK_MASK) == 0 {
                        check_cancel(eval_ctx, &mut Some(res), before)?;
                    }
                    let (s, e) = compute_row_frame_bounds(frame, order_empty, peers, i, n);
                    let mut acc: Option<i64> = None;
                    for r in s..e {
                        if (r & CANCEL_CHECK_MASK) == 0 {
                            check_cancel(eval_ctx, &mut Some(res), before)?;
                        }
                        if let Some(v) = eval::read_i64(col, r) {
                            acc = Some(match acc {
                                Some(cur) => cur.checked_add(v).ok_or_else(|| {
                                    QueryError::failed("window SUM(I64) overflow")
                                })?,
                                None => v,
                            });
                        }
                    }
                    if let Some(sum) = acc {
                        *slot = sum;
                        let _ = nulls.set(i, true);
                    }
                }
            }
            Column::Flat(FlatColumn::try_new(FlatData::I64(vals), Some(nulls))?)
        }
        DataType::F64 => {
            res.try_grow(eval::checked_fixed_bytes(n, 8, true)?)?;
            let mut vals = vec![0.0f64; n];
            let mut nulls = Bitmap::new_zeroed(n);
            if is_unbounded_prefix_frame(frame) {
                let mut acc: Option<f64> = None;
                let mut pos = 0usize;
                for (i, slot) in vals.iter_mut().enumerate() {
                    if (i & CANCEL_CHECK_MASK) == 0 {
                        check_cancel(eval_ctx, &mut Some(res), before)?;
                    }
                    let (_, e) = compute_row_frame_bounds(frame, order_empty, peers, i, n);
                    while pos < e {
                        if (pos & CANCEL_CHECK_MASK) == 0 {
                            check_cancel(eval_ctx, &mut Some(res), before)?;
                        }
                        if let Some(v) = eval::read_f64(col, pos) {
                            acc = Some(acc.map_or(v, |cur| cur + v));
                        }
                        pos += 1;
                    }
                    if let Some(s) = acc {
                        *slot = s;
                        let _ = nulls.set(i, true);
                    }
                }
            } else {
                for (i, slot) in vals.iter_mut().enumerate() {
                    if (i & CANCEL_CHECK_MASK) == 0 {
                        check_cancel(eval_ctx, &mut Some(res), before)?;
                    }
                    let (s, e) = compute_row_frame_bounds(frame, order_empty, peers, i, n);
                    let mut acc: Option<f64> = None;
                    for r in s..e {
                        if (r & CANCEL_CHECK_MASK) == 0 {
                            check_cancel(eval_ctx, &mut Some(res), before)?;
                        }
                        if let Some(v) = eval::read_f64(col, r) {
                            acc = Some(acc.map_or(v, |cur| cur + v));
                        }
                    }
                    if let Some(sum) = acc {
                        *slot = sum;
                        let _ = nulls.set(i, true);
                    }
                }
            }
            Column::Flat(FlatColumn::try_new(FlatData::F64(vals), Some(nulls))?)
        }
        DataType::Decimal128 { precision, scale } => {
            res.try_grow(eval::checked_fixed_bytes(n, 16, true)?)?;
            let mut vals = vec![0i128; n];
            let mut nulls = Bitmap::new_zeroed(n);
            if is_unbounded_prefix_frame(frame) {
                let mut acc: Option<i128> = None;
                let mut pos = 0usize;
                for (i, slot) in vals.iter_mut().enumerate() {
                    if (i & CANCEL_CHECK_MASK) == 0 {
                        check_cancel(eval_ctx, &mut Some(res), before)?;
                    }
                    let (_, e) = compute_row_frame_bounds(frame, order_empty, peers, i, n);
                    while pos < e {
                        if (pos & CANCEL_CHECK_MASK) == 0 {
                            check_cancel(eval_ctx, &mut Some(res), before)?;
                        }
                        if let Some(v) = eval::read_decimal(col, pos) {
                            acc = Some(match acc {
                                Some(cur) => cur.checked_add(v).ok_or_else(|| {
                                    QueryError::failed("window SUM(Decimal128) overflow")
                                })?,
                                None => v,
                            });
                        }
                        pos += 1;
                    }
                    if let Some(s) = acc {
                        *slot = check_dec_bounds(s, *precision)?;
                        let _ = nulls.set(i, true);
                    }
                }
            } else {
                for (i, slot) in vals.iter_mut().enumerate() {
                    if (i & CANCEL_CHECK_MASK) == 0 {
                        check_cancel(eval_ctx, &mut Some(res), before)?;
                    }
                    let (s, e) = compute_row_frame_bounds(frame, order_empty, peers, i, n);
                    let mut acc: Option<i128> = None;
                    for r in s..e {
                        if (r & CANCEL_CHECK_MASK) == 0 {
                            check_cancel(eval_ctx, &mut Some(res), before)?;
                        }
                        if let Some(v) = eval::read_decimal(col, r) {
                            acc = Some(match acc {
                                Some(cur) => cur.checked_add(v).ok_or_else(|| {
                                    QueryError::failed("window SUM(Decimal128) overflow")
                                })?,
                                None => v,
                            });
                        }
                    }
                    if let Some(sum) = acc {
                        *slot = check_dec_bounds(sum, *precision)?;
                        let _ = nulls.set(i, true);
                    }
                }
            }
            Column::Flat(FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vals,
                    precision: *precision,
                    scale: *scale,
                },
                Some(nulls),
            )?)
        }
        _ => return Err(QueryError::failed("unsupported SUM input type")),
    };

    drop(val_access);
    reconcile_output(&mut Some(res), before, out.memory_bytes())?;
    Ok(out)
}

#[expect(
    clippy::too_many_arguments,
    reason = "window avg shares frame/peer/reservation parameters"
)]
fn eval_window_avg(
    expr: &Expr,
    out_dt: &DataType,
    frame: Option<(FrameBound, FrameBound)>,
    order_empty: bool,
    peers: Option<&PeerInfo>,
    batch: &Batch,
    eval_ctx: &EvalContext,
    res: &mut Reservation,
) -> Result<Column, QueryError> {
    let n = batch.num_rows();
    let before = res.size();
    let val_access = eval_or_borrow(expr, batch, eval_ctx, res)?;
    let col = val_access.as_col();

    let out = match (expr.data_type(), out_dt) {
        (DataType::Null, _) => Column::Const {
            value: ScalarValue::Null(DataType::Null),
            len: n,
        },
        (DataType::I64, DataType::F64) => {
            res.try_grow(eval::checked_fixed_bytes(n, 8, true)?)?;
            let mut vals = vec![0.0f64; n];
            let mut nulls = Bitmap::new_zeroed(n);
            if is_unbounded_prefix_frame(frame) {
                let mut sum: i128 = 0;
                let mut cnt: u64 = 0;
                let mut pos = 0usize;
                for (i, slot) in vals.iter_mut().enumerate() {
                    if (i & CANCEL_CHECK_MASK) == 0 {
                        check_cancel(eval_ctx, &mut Some(res), before)?;
                    }
                    let (_, e) = compute_row_frame_bounds(frame, order_empty, peers, i, n);
                    while pos < e {
                        if (pos & CANCEL_CHECK_MASK) == 0 {
                            check_cancel(eval_ctx, &mut Some(res), before)?;
                        }
                        if let Some(v) = eval::read_i64(col, pos) {
                            sum += i128::from(v);
                            cnt += 1;
                        }
                        pos += 1;
                    }
                    if cnt > 0 {
                        *slot = (sum as f64) / (cnt as f64);
                        let _ = nulls.set(i, true);
                    }
                }
            } else {
                for (i, slot) in vals.iter_mut().enumerate() {
                    if (i & CANCEL_CHECK_MASK) == 0 {
                        check_cancel(eval_ctx, &mut Some(res), before)?;
                    }
                    let (s, e) = compute_row_frame_bounds(frame, order_empty, peers, i, n);
                    let mut sum: i128 = 0;
                    let mut cnt: u64 = 0;
                    for r in s..e {
                        if (r & CANCEL_CHECK_MASK) == 0 {
                            check_cancel(eval_ctx, &mut Some(res), before)?;
                        }
                        if let Some(v) = eval::read_i64(col, r) {
                            sum += i128::from(v);
                            cnt += 1;
                        }
                    }
                    if cnt > 0 {
                        *slot = (sum as f64) / (cnt as f64);
                        let _ = nulls.set(i, true);
                    }
                }
            }
            Column::Flat(FlatColumn::try_new(FlatData::F64(vals), Some(nulls))?)
        }
        (DataType::F64, DataType::F64) => {
            res.try_grow(eval::checked_fixed_bytes(n, 8, true)?)?;
            let mut vals = vec![0.0f64; n];
            let mut nulls = Bitmap::new_zeroed(n);
            if is_unbounded_prefix_frame(frame) {
                let mut sum: f64 = 0.0;
                let mut cnt: u64 = 0;
                let mut pos = 0usize;
                for (i, slot) in vals.iter_mut().enumerate() {
                    if (i & CANCEL_CHECK_MASK) == 0 {
                        check_cancel(eval_ctx, &mut Some(res), before)?;
                    }
                    let (_, e) = compute_row_frame_bounds(frame, order_empty, peers, i, n);
                    while pos < e {
                        if (pos & CANCEL_CHECK_MASK) == 0 {
                            check_cancel(eval_ctx, &mut Some(res), before)?;
                        }
                        if let Some(v) = eval::read_f64(col, pos) {
                            sum += v;
                            cnt += 1;
                        }
                        pos += 1;
                    }
                    if cnt > 0 {
                        *slot = sum / (cnt as f64);
                        let _ = nulls.set(i, true);
                    }
                }
            } else {
                for (i, slot) in vals.iter_mut().enumerate() {
                    if (i & CANCEL_CHECK_MASK) == 0 {
                        check_cancel(eval_ctx, &mut Some(res), before)?;
                    }
                    let (s, e) = compute_row_frame_bounds(frame, order_empty, peers, i, n);
                    let mut sum: f64 = 0.0;
                    let mut cnt: u64 = 0;
                    for r in s..e {
                        if (r & CANCEL_CHECK_MASK) == 0 {
                            check_cancel(eval_ctx, &mut Some(res), before)?;
                        }
                        if let Some(v) = eval::read_f64(col, r) {
                            sum += v;
                            cnt += 1;
                        }
                    }
                    if cnt > 0 {
                        *slot = sum / (cnt as f64);
                        let _ = nulls.set(i, true);
                    }
                }
            }
            Column::Flat(FlatColumn::try_new(FlatData::F64(vals), Some(nulls))?)
        }
        (
            DataType::Decimal128 {
                scale: in_scale, ..
            },
            DataType::Decimal128 {
                precision: out_p,
                scale: out_s,
            },
        ) => {
            let scale_diff = (i32::from(*out_s) - i32::from(in_scale)).max(0) as u32;
            let scale_factor = 10u128
                .checked_pow(scale_diff)
                .ok_or_else(|| QueryError::failed("decimal AVG scale overflow"))?;
            res.try_grow(eval::checked_fixed_bytes(n, 16, true)?)?;
            let mut vals = vec![0i128; n];
            let mut nulls = Bitmap::new_zeroed(n);

            if is_unbounded_prefix_frame(frame) {
                let mut sum: i128 = 0;
                let mut cnt: u128 = 0;
                let mut pos = 0usize;
                for (i, slot) in vals.iter_mut().enumerate() {
                    if (i & CANCEL_CHECK_MASK) == 0 {
                        check_cancel(eval_ctx, &mut Some(res), before)?;
                    }
                    let (_, e) = compute_row_frame_bounds(frame, order_empty, peers, i, n);
                    while pos < e {
                        if (pos & CANCEL_CHECK_MASK) == 0 {
                            check_cancel(eval_ctx, &mut Some(res), before)?;
                        }
                        if let Some(v) = eval::read_decimal(col, pos) {
                            sum = sum.checked_add(v).ok_or_else(|| {
                                QueryError::failed("window AVG(Decimal128) overflow")
                            })?;
                            cnt += 1;
                        }
                        pos += 1;
                    }
                    if cnt > 0 {
                        *slot = decimal_avg(sum, cnt, scale_factor, *out_p)?;
                        let _ = nulls.set(i, true);
                    }
                }
            } else {
                for (i, slot) in vals.iter_mut().enumerate() {
                    if (i & CANCEL_CHECK_MASK) == 0 {
                        check_cancel(eval_ctx, &mut Some(res), before)?;
                    }
                    let (s, e) = compute_row_frame_bounds(frame, order_empty, peers, i, n);
                    let mut sum: i128 = 0;
                    let mut cnt: u128 = 0;
                    for r in s..e {
                        if (r & CANCEL_CHECK_MASK) == 0 {
                            check_cancel(eval_ctx, &mut Some(res), before)?;
                        }
                        if let Some(v) = eval::read_decimal(col, r) {
                            sum = sum.checked_add(v).ok_or_else(|| {
                                QueryError::failed("window AVG(Decimal128) overflow")
                            })?;
                            cnt += 1;
                        }
                    }
                    if cnt > 0 {
                        *slot = decimal_avg(sum, cnt, scale_factor, *out_p)?;
                        let _ = nulls.set(i, true);
                    }
                }
            }
            Column::Flat(FlatColumn::try_new(
                FlatData::Decimal128 {
                    values: vals,
                    precision: *out_p,
                    scale: *out_s,
                },
                Some(nulls),
            )?)
        }
        _ => return Err(QueryError::failed("unsupported AVG input type")),
    };

    drop(val_access);
    reconcile_output(&mut Some(res), before, out.memory_bytes())?;
    Ok(out)
}

#[expect(
    clippy::too_many_arguments,
    reason = "window min/max shares frame/peer/reservation parameters"
)]
fn eval_window_min_max(
    expr: &Expr,
    out_dt: &DataType,
    frame: Option<(FrameBound, FrameBound)>,
    order_empty: bool,
    peers: Option<&PeerInfo>,
    batch: &Batch,
    eval_ctx: &EvalContext,
    res: &mut Reservation,
    is_max: bool,
) -> Result<Column, QueryError> {
    let n = batch.num_rows();
    let before = res.size();
    let val_access = eval_or_borrow(expr, batch, eval_ctx, res)?;
    let col = val_access.as_col();
    let key = [SortKey {
        expr: expr.clone(),
        descending: false,
        nulls_first: false,
    }];
    let col_slice = std::slice::from_ref(col);
    let mut best_rows: Vec<Option<usize>> = vec![None; n];

    let better = |cand: usize, cur: usize| -> bool {
        let ord = compare_rows(col_slice, &key, cand, cur);
        if is_max {
            ord == Ordering::Greater
        } else {
            ord == Ordering::Less
        }
    };

    if is_unbounded_prefix_frame(frame) {
        let mut best: Option<usize> = None;
        let mut pos = 0usize;
        for (i, slot) in best_rows.iter_mut().enumerate() {
            if (i & CANCEL_CHECK_MASK) == 0 {
                check_cancel(eval_ctx, &mut Some(res), before)?;
            }
            let (_, e) = compute_row_frame_bounds(frame, order_empty, peers, i, n);
            while pos < e {
                if (pos & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(eval_ctx, &mut Some(res), before)?;
                }
                if !col.is_null(pos) {
                    best = Some(match best {
                        Some(cur) if !better(pos, cur) => cur,
                        _ => pos,
                    });
                }
                pos += 1;
            }
            *slot = best;
        }
    } else {
        for (i, slot) in best_rows.iter_mut().enumerate() {
            if (i & CANCEL_CHECK_MASK) == 0 {
                check_cancel(eval_ctx, &mut Some(res), before)?;
            }
            let (s, e) = compute_row_frame_bounds(frame, order_empty, peers, i, n);
            let mut best: Option<usize> = None;
            for r in s..e {
                if (r & CANCEL_CHECK_MASK) == 0 {
                    check_cancel(eval_ctx, &mut Some(res), before)?;
                }
                if !col.is_null(r) {
                    best = Some(match best {
                        Some(cur) if !better(r, cur) => cur,
                        _ => r,
                    });
                }
            }
            *slot = best;
        }
    }

    let refs: Vec<ElemRef<'_>> = best_rows
        .into_iter()
        .map(|opt| match opt {
            Some(r) => ElemRef::Col(col, r),
            None => ElemRef::Null,
        })
        .collect();
    let build_before = res.size();
    let out =
        eval::build_flat_from_elem_refs(out_dt, &refs, &mut Some(res), eval_ctx, build_before)?;
    drop(val_access);
    reconcile_output(&mut Some(res), before, out.memory_bytes())?;
    Ok(out)
}

fn check_dec_bounds(val: i128, precision: u8) -> Result<i128, QueryError> {
    if precision == 0 || precision > 38 {
        return Err(QueryError::failed("decimal precision out of range"));
    }
    let limit = 10u128.pow(u32::from(precision));
    if val.unsigned_abs() >= limit {
        return Err(QueryError::failed("decimal overflow"));
    }
    Ok(val)
}

fn decimal_avg(
    sum: i128,
    count: u128,
    scale_factor: u128,
    out_precision: u8,
) -> Result<i128, QueryError> {
    let neg = sum < 0;
    let abs_sum = sum.unsigned_abs();
    let a_lo = abs_sum & 0xffff_ffff_ffff_ffff;
    let a_hi = abs_sum >> 64;
    let b_lo = scale_factor & 0xffff_ffff_ffff_ffff;
    let b_hi = scale_factor >> 64;

    let p0 = a_lo * b_lo;
    let p1 = a_lo * b_hi;
    let p2 = a_hi * b_lo;
    let p3 = a_hi * b_hi;

    let mid1 = (p0 >> 64) + (p1 & 0xffff_ffff_ffff_ffff) + (p2 & 0xffff_ffff_ffff_ffff);
    let lo = (p0 & 0xffff_ffff_ffff_ffff) | (mid1 << 64);
    let hi = p3 + (p1 >> 64) + (p2 >> 64) + (mid1 >> 64);

    if hi >= count {
        return Err(QueryError::failed("decimal AVG overflow"));
    }
    let mut q = 0u128;
    let mut r = hi;
    for shift in (0..128).rev() {
        let carry = r >> 127;
        r = (r << 1) | ((lo >> shift) & 1);
        if carry != 0 || r >= count {
            r = r.wrapping_sub(count);
            q |= 1u128 << shift;
        }
    }
    let signed_mag = i128::try_from(q).map_err(|_| QueryError::failed("decimal AVG overflow"))?;
    let val = if neg && signed_mag != 0 {
        -signed_mag
    } else {
        signed_mag
    };
    check_dec_bounds(val, out_precision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{
        BatchSource, ExecMetrics, MAX_MERGE_WAY, ScanSource, ScanSpec, SpillManager, WorkerPool,
        run_pipeline,
    };
    use cetra_column::{GlobalPool, ScalarValue, Utf8Data};

    fn test_ctx(dir: &std::path::Path, pool_cap: usize, spill_cap: u64) -> ExecContext {
        ExecContext {
            pool: GlobalPool::standalone(pool_cap),
            eval: EvalContext::new(0, CancelToken::new()),
            spill: Arc::new(SpillManager::with_fixed_cap(dir.to_path_buf(), spill_cap)),
            workers: Arc::new(WorkerPool::new(2).unwrap()),
            batch_size: 8192,
            target_partitions: 4,
            metrics: ExecMetrics::new(),
        }
    }

    fn col_expr(index: usize, name: &str, data_type: DataType) -> Expr {
        Expr::Column {
            index,
            name: name.to_owned(),
            data_type,
        }
    }

    fn drain_stream(
        stream: &mut dyn PartitionStream,
        ctx: &ExecContext,
    ) -> Result<Vec<Batch>, QueryError> {
        let mut out = Vec::new();
        let mut res = ctx.pool.register("drain", false);
        loop {
            let before = res.size();
            match stream.next_batch(&ctx.eval.cancel, Some(&mut res))? {
                Some(b) => {
                    assert_eq!(res.size(), before + b.memory_bytes());
                    res.shrink(b.memory_bytes());
                    out.push(b);
                }
                None => {
                    assert_eq!(res.size(), before);
                    break;
                }
            }
        }
        Ok(out)
    }

    #[test]
    fn external_sort_spills_under_tight_memory_and_bounds_fds() {
        let dir = tempfile::tempdir().unwrap();
        // Tight 64 KiB pool with 10 MiB disk spill cap.
        let ctx = test_ctx(dir.path(), 64 * 1024, 10 * 1024 * 1024);
        let schema = Schema::try_new_arc(vec![
            Field::new("k", DataType::I64, true),
            Field::new("payload", DataType::Utf8, false),
        ])
        .unwrap();

        let keys = vec![SortKey {
            expr: col_expr(0, "k", DataType::I64),
            descending: false,
            nulls_first: false,
        }];

        let mut sort = ExternalSort::new(Arc::clone(&schema), keys.clone(), None, ctx.clone())
            .with_max_merge_way(4);

        // Feed 160 batches (~19.5 KiB each = ~3.1 MiB total) -> forces > 40 runs in 64 KiB pool.
        let pad = "x".repeat(580);
        let mut expected_rows: Vec<(Option<i64>, String)> = Vec::new();
        let mut dummy_res = ctx.pool.register("test_in", false);
        for batch_id in 0..160i64 {
            let mut ks = Vec::with_capacity(32);
            let mut k_valid = Vec::with_capacity(32);
            let mut payloads = Vec::with_capacity(32);
            for j in 0..32i64 {
                let raw = ((batch_id * 37 + j * 13) % 1000) - 500;
                let is_null = (batch_id + j) % 19 == 0;
                ks.push(raw);
                k_valid.push(!is_null);
                let p = format!("p-{batch_id:03}-{j:02}-{pad}");
                expected_rows.push((if is_null { None } else { Some(raw) }, p.clone()));
                payloads.push(p);
            }
            let c0 = Column::Flat(
                FlatColumn::try_new(FlatData::I64(ks), Some(Bitmap::from_bools(&k_valid))).unwrap(),
            );
            let p_refs: Vec<&str> = payloads.iter().map(String::as_str).collect();
            let c1 = Column::Flat(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(p_refs).unwrap()),
                    None,
                )
                .unwrap(),
            );
            let batch = Batch::try_new_all(Arc::clone(&schema), vec![c0, c1], 32).unwrap();
            assert_eq!(
                sort.consume(batch, &mut dummy_res).unwrap(),
                ControlFlow::Continue
            );
        }

        assert!(
            sort.spilled_run_count() >= 40,
            "expected >= 40 spilled runs for multi-pass merge, got {}",
            sort.spilled_run_count()
        );

        let mut stream = sort.into_stream().unwrap();
        let out_batches = drain_stream(stream.as_mut(), &ctx).unwrap();
        drop(stream);

        assert_eq!(ctx.spill.spilled_bytes(), 0);
        assert_eq!(ctx.pool.reserved(), 0);

        let mut actual_rows: Vec<(Option<i64>, String)> = Vec::new();
        for b in &out_batches {
            for r in 0..b.num_rows() {
                let k = eval::read_i64(b.column(0).unwrap(), r);
                let p = eval::read_str(b.column(1).unwrap(), r).unwrap().to_owned();
                actual_rows.push((k, p));
            }
        }

        // Verify exact sorted order (ASC NULLS LAST) and multiset preservation.
        expected_rows.sort_by(|(ka, pa), (kb, pb)| match (ka, kb) {
            (None, None) => pa.cmp(pb),
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(a), Some(b)) => a.cmp(b).then_with(|| pa.cmp(pb)),
        });
        assert_eq!(actual_rows.len(), expected_rows.len());
        // Group-by key check (since sort preserves ties per run/batch order, verify key order + multiset)
        let actual_keys: Vec<Option<i64>> = actual_rows.iter().map(|(k, _)| *k).collect();
        let expected_keys: Vec<Option<i64>> = expected_rows.iter().map(|(k, _)| *k).collect();
        assert_eq!(actual_keys, expected_keys);

        let mut actual_sorted = actual_rows;
        actual_sorted.sort_by(|(ka, pa), (kb, pb)| ka.cmp(kb).then_with(|| pa.cmp(pb)));
        let mut expected_sorted = expected_rows;
        expected_sorted.sort_by(|(ka, pa), (kb, pb)| ka.cmp(kb).then_with(|| pa.cmp(pb)));
        assert_eq!(actual_sorted, expected_sorted);

        // Verify immediate QueryError::TooLarge when a single batch exceeds the pool at 0 buffered rows.
        let mut tiny_sort = ExternalSort::new(Arc::clone(&schema), keys, None, ctx.clone());
        let big_payload = "x".repeat(70 * 1024);
        let big_batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices([big_payload.as_str()]).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            1,
        )
        .unwrap();
        let err = tiny_sort.consume(big_batch, &mut dummy_res).unwrap_err();
        drop(dummy_res);
        assert!(matches!(err, QueryError::TooLarge));
        drop(tiny_sort);
        assert_eq!(ctx.spill.spilled_bytes(), 0);
        assert_eq!(ctx.pool.reserved(), 0);
    }

    #[test]
    fn merge_sinks_combines_parallel_partitions() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path(), 4 * 1024 * 1024, 16 * 1024 * 1024);
        let schema = Schema::try_new_arc(vec![
            Field::new("score", DataType::F64, true),
            Field::new("id", DataType::I64, false),
        ])
        .unwrap();

        let keys = vec![
            SortKey {
                expr: col_expr(0, "score", DataType::F64),
                descending: true,
                nulls_first: false,
            },
            SortKey {
                expr: col_expr(1, "id", DataType::I64),
                descending: false,
                nulls_first: false,
            },
        ];

        // 4 input batches distributed across parallel partitions.
        let batches = vec![
            Batch::try_new_all(
                Arc::clone(&schema),
                vec![
                    Column::Flat(
                        FlatColumn::try_new(
                            FlatData::F64(vec![1.5, f64::NAN, -0.0, 0.0]),
                            Some(Bitmap::from_bools(&[true, true, true, false])),
                        )
                        .unwrap(),
                    ),
                    Column::Flat(
                        FlatColumn::try_new(FlatData::I64(vec![10, 11, 12, 13]), None).unwrap(),
                    ),
                ],
                4,
            )
            .unwrap(),
            Batch::try_new_all(
                Arc::clone(&schema),
                vec![
                    Column::Flat(
                        FlatColumn::try_new(FlatData::F64(vec![0.0, 10.0, 1.5]), None).unwrap(),
                    ),
                    Column::Flat(FlatColumn::try_new(FlatData::I64(vec![5, 6, 7]), None).unwrap()),
                ],
                3,
            )
            .unwrap(),
            Batch::try_new_all(
                Arc::clone(&schema),
                vec![
                    Column::Flat(
                        FlatColumn::try_new(
                            FlatData::F64(vec![f64::INFINITY, 0.0]),
                            Some(Bitmap::from_bools(&[true, false])),
                        )
                        .unwrap(),
                    ),
                    Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1, 2]), None).unwrap()),
                ],
                2,
            )
            .unwrap(),
            Batch::try_new_all(
                Arc::clone(&schema),
                vec![
                    Column::Flat(
                        FlatColumn::try_new(FlatData::F64(vec![f64::NEG_INFINITY, 10.0]), None)
                            .unwrap(),
                    ),
                    Column::Flat(FlatColumn::try_new(FlatData::I64(vec![3, 4]), None).unwrap()),
                ],
                2,
            )
            .unwrap(),
        ];

        let source = BatchSource::new(Arc::clone(&schema), batches).unwrap();
        let partitions = source.open_partitions(&ScanSpec::default(), &ctx).unwrap();
        assert_eq!(partitions.len(), 4);

        let schema_c = Arc::clone(&schema);
        let keys_c = keys.clone();
        let ctx_c = ctx.clone();
        let sinks = run_pipeline(
            &ctx,
            partitions,
            |_| Ok(Vec::new()),
            move |_| {
                Ok(ExternalSort::new(
                    Arc::clone(&schema_c),
                    keys_c.clone(),
                    None,
                    ctx_c.clone(),
                ))
            },
        )
        .unwrap();

        let mut merged =
            ExternalSort::merge_sinks(sinks, Arc::clone(&schema), keys, Some(9), &ctx).unwrap();
        let out = drain_stream(merged.as_mut(), &ctx).unwrap();
        drop(merged);

        assert_eq!(ctx.spill.spilled_bytes(), 0);
        assert_eq!(ctx.pool.reserved(), 0);

        let got_ids: Vec<i64> = out
            .iter()
            .flat_map(|b| {
                (0..b.num_rows()).map(|r| eval::read_i64(b.column(1).unwrap(), r).unwrap())
            })
            .collect();
        // Total order DESC NULLS LAST:
        // NaN (11), +inf (1), 10.0 (4, 6), 1.5 (7, 10), 0.0/-0.0 (5, 12), -inf (3), [truncated: NULL (2, 13)]
        assert_eq!(got_ids, vec![11, 1, 4, 6, 7, 10, 5, 12, 3]);
    }

    #[test]
    fn window_functions_row_number_rank_lag_lead_first_last_and_frames() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path(), 8 * 1024 * 1024, 8 * 1024 * 1024);
        let schema = Schema::try_new_arc(vec![
            Field::new("grp", DataType::Utf8, false),
            Field::new("ord", DataType::I64, false),
            Field::new("val", DataType::I64, true),
            Field::new("cost", DataType::cost_decimal(), true),
        ])
        .unwrap();

        let partition_by = vec![col_expr(0, "grp", DataType::Utf8)];
        let order_by = vec![SortKey {
            expr: col_expr(1, "ord", DataType::I64),
            descending: false,
            nulls_first: false,
        }];

        let specs = vec![
            WindowSpec {
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: None,
                func: WindowFunc::RowNumber,
                output_name: "rn".to_owned(),
            },
            WindowSpec {
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: None,
                func: WindowFunc::Rank,
                output_name: "rk".to_owned(),
            },
            WindowSpec {
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: None,
                func: WindowFunc::DenseRank,
                output_name: "drk".to_owned(),
            },
            WindowSpec {
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: None,
                func: WindowFunc::LastValue(col_expr(2, "val", DataType::I64)),
                output_name: "last_default".to_owned(),
            },
            WindowSpec {
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: None,
                func: WindowFunc::FirstValue(col_expr(2, "val", DataType::I64)),
                output_name: "first_default".to_owned(),
            },
            WindowSpec {
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: None,
                func: WindowFunc::Lag {
                    expr: col_expr(2, "val", DataType::I64),
                    offset: 1,
                    default: Some(Expr::Literal(ScalarValue::I64(-1))),
                },
                output_name: "lag1".to_owned(),
            },
            WindowSpec {
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: None,
                func: WindowFunc::Lag {
                    expr: col_expr(2, "val", DataType::I64),
                    offset: 0,
                    default: Some(Expr::Literal(ScalarValue::I64(-99))),
                },
                output_name: "lag0".to_owned(),
            },
            WindowSpec {
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: None,
                func: WindowFunc::Lag {
                    expr: col_expr(2, "val", DataType::I64),
                    offset: -1,
                    default: Some(Expr::Literal(ScalarValue::I64(-2))),
                },
                output_name: "lag_neg1".to_owned(),
            },
            WindowSpec {
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: None,
                func: WindowFunc::Lead {
                    expr: col_expr(2, "val", DataType::I64),
                    offset: -1,
                    default: None,
                },
                output_name: "lead_neg1".to_owned(),
            },
            WindowSpec {
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: Some((FrameBound::Preceding(1), FrameBound::Following(1))),
                func: WindowFunc::Sum(col_expr(2, "val", DataType::I64)),
                output_name: "sum_win".to_owned(),
            },
            WindowSpec {
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: Some((FrameBound::Preceding(1), FrameBound::Following(1))),
                func: WindowFunc::Count(Some(col_expr(2, "val", DataType::I64))),
                output_name: "cnt_val".to_owned(),
            },
            WindowSpec {
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: Some((FrameBound::Preceding(1), FrameBound::Following(1))),
                func: WindowFunc::Avg(col_expr(2, "val", DataType::I64)),
                output_name: "avg_win".to_owned(),
            },
            WindowSpec {
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: Some((FrameBound::Preceding(1), FrameBound::Following(1))),
                func: WindowFunc::Min(col_expr(2, "val", DataType::I64)),
                output_name: "min_win".to_owned(),
            },
            WindowSpec {
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: Some((FrameBound::Preceding(1), FrameBound::Following(1))),
                func: WindowFunc::Max(col_expr(2, "val", DataType::I64)),
                output_name: "max_win".to_owned(),
            },
            WindowSpec {
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: None,
                func: WindowFunc::Sum(col_expr(3, "cost", DataType::cost_decimal())),
                output_name: "sum_cost".to_owned(),
            },
            WindowSpec {
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: None,
                func: WindowFunc::Avg(col_expr(3, "cost", DataType::cost_decimal())),
                output_name: "avg_cost".to_owned(),
            },
            WindowSpec {
                partition_by: partition_by.clone(),
                order_by: order_by.clone(),
                frame: Some((FrameBound::Preceding(2), FrameBound::Preceding(1))),
                func: WindowFunc::Sum(col_expr(2, "val", DataType::I64)),
                output_name: "prev2_sum".to_owned(),
            },
        ];

        // 1. 0-row input -> 0-row output
        let empty_src = BatchSource::empty(Arc::clone(&schema));
        let empty_stream = Box::new(EmptyStream);
        let mut win_empty = eval_window_group(
            empty_stream,
            Arc::clone(&schema),
            partition_by.clone(),
            order_by.clone(),
            specs.clone(),
            &ctx,
        )
        .unwrap();
        let empty_out = drain_stream(win_empty.as_mut(), &ctx).unwrap();
        assert!(empty_out.is_empty());
        let _ = empty_src;

        // 2. Non-empty multi-batch input with peer ties in grp="a" (ord: 1, 1, 2) and grp="b" (ord: 5, 6)
        // Split across 2 batches so partition "a" crosses a batch boundary.
        let b1 = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(["b", "a", "a"]).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![6, 1, 2]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::I64(vec![300, 10, 0]),
                        Some(Bitmap::from_bools(&[true, true, false])),
                    )
                    .unwrap(),
                ),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Decimal128 {
                            values: vec![3_000, 100, 200],
                            precision: 38,
                            scale: 18,
                        },
                        None,
                    )
                    .unwrap(),
                ),
            ],
            3,
        )
        .unwrap();
        let b2 = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(["a", "b"]).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1, 5]), None).unwrap()),
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![20, 200]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Decimal128 {
                            values: vec![150, 2_000],
                            precision: 38,
                            scale: 18,
                        },
                        None,
                    )
                    .unwrap(),
                ),
            ],
            2,
        )
        .unwrap();

        let src = BatchSource::new(Arc::clone(&schema), vec![b1, b2]).unwrap();
        let mut parts = src
            .open_partitions(
                &ScanSpec::default(),
                &ExecContext {
                    target_partitions: 1,
                    ..ctx.clone()
                },
            )
            .unwrap();
        assert_eq!(parts.len(), 1);
        let mut win_stream = eval_window_group(
            parts.remove(0),
            Arc::clone(&schema),
            partition_by,
            order_by,
            specs,
            &ctx,
        )
        .unwrap();
        let out_batches = drain_stream(win_stream.as_mut(), &ctx).unwrap();
        drop(win_stream);

        assert_eq!(ctx.spill.spilled_bytes(), 0);
        assert_eq!(ctx.pool.reserved(), 0);

        let merged = if out_batches.len() == 1 {
            out_batches.into_iter().next().unwrap()
        } else {
            let out_schema = Arc::clone(out_batches[0].schema());
            let refs: Vec<&Batch> = out_batches.iter().collect();
            Batch::concat(out_schema, &refs, None).unwrap()
        };
        assert_eq!(merged.num_rows(), 5);
        assert_eq!(merged.columns().len(), 4 + 17);

        // Expected sorted rows:
        // 0: ("a", ord=1, val=10)
        // 1: ("a", ord=1, val=20)
        // 2: ("a", ord=2, val=NULL)
        // 3: ("b", ord=5, val=200)
        // 4: ("b", ord=6, val=300)
        let col = |name: &str| merged.column_by_name(name).unwrap();
        let grps: Vec<&str> = (0..5)
            .map(|r| eval::read_str(col("grp"), r).unwrap())
            .collect();
        let ords: Vec<i64> = (0..5)
            .map(|r| eval::read_i64(col("ord"), r).unwrap())
            .collect();
        assert_eq!(grps, vec!["a", "a", "a", "b", "b"]);
        assert_eq!(ords, vec![1, 1, 2, 5, 6]);

        let rns: Vec<i64> = (0..5)
            .map(|r| eval::read_i64(col("rn"), r).unwrap())
            .collect();
        let rks: Vec<i64> = (0..5)
            .map(|r| eval::read_i64(col("rk"), r).unwrap())
            .collect();
        let drks: Vec<i64> = (0..5)
            .map(|r| eval::read_i64(col("drk"), r).unwrap())
            .collect();
        assert_eq!(rns, vec![1, 2, 3, 1, 2]);
        assert_eq!(rks, vec![1, 1, 3, 1, 2]);
        assert_eq!(drks, vec![1, 1, 2, 1, 2]);

        // Default frame with ORDER BY extends to end of peer group:
        // For ("a", ord=1) peers (rows 0, 1): last_default = 20 (last peer)!
        // For ("a", ord=2) (row 2): last_default = NULL (val is NULL)!
        let last_def: Vec<Option<i64>> = (0..5)
            .map(|r| eval::read_i64(col("last_default"), r))
            .collect();
        let first_def: Vec<Option<i64>> = (0..5)
            .map(|r| eval::read_i64(col("first_default"), r))
            .collect();
        assert_eq!(
            last_def,
            vec![Some(20), Some(20), None, Some(200), Some(300)]
        );
        assert_eq!(
            first_def,
            vec![Some(10), Some(10), Some(10), Some(200), Some(200)]
        );

        // LAG(val, 1, -1), LAG(val, 0, -99), LAG(val, -1, -2), LEAD(val, -1)
        let lag1: Vec<Option<i64>> = (0..5).map(|r| eval::read_i64(col("lag1"), r)).collect();
        let lag0: Vec<Option<i64>> = (0..5).map(|r| eval::read_i64(col("lag0"), r)).collect();
        let lag_neg1: Vec<Option<i64>> =
            (0..5).map(|r| eval::read_i64(col("lag_neg1"), r)).collect();
        let lead_neg1: Vec<Option<i64>> = (0..5)
            .map(|r| eval::read_i64(col("lead_neg1"), r))
            .collect();
        assert_eq!(
            lag1,
            vec![Some(-1), Some(10), Some(20), Some(-1), Some(200)]
        );
        assert_eq!(lag0, vec![Some(10), Some(20), None, Some(200), Some(300)]);
        assert_eq!(
            lag_neg1,
            vec![Some(20), None, Some(-2), Some(300), Some(-2)]
        );
        assert_eq!(lead_neg1, vec![None, Some(10), Some(20), None, Some(200)]);

        // ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING
        let sum_w: Vec<Option<i64>> = (0..5).map(|r| eval::read_i64(col("sum_win"), r)).collect();
        let cnt_w: Vec<i64> = (0..5)
            .map(|r| eval::read_i64(col("cnt_val"), r).unwrap())
            .collect();
        let avg_w: Vec<Option<f64>> = (0..5).map(|r| eval::read_f64(col("avg_win"), r)).collect();
        let min_w: Vec<Option<i64>> = (0..5).map(|r| eval::read_i64(col("min_win"), r)).collect();
        let max_w: Vec<Option<i64>> = (0..5).map(|r| eval::read_i64(col("max_win"), r)).collect();
        assert_eq!(
            sum_w,
            vec![Some(30), Some(30), Some(20), Some(500), Some(500)]
        );
        assert_eq!(cnt_w, vec![2, 2, 1, 2, 2]);
        assert_eq!(
            avg_w,
            vec![Some(15.0), Some(15.0), Some(20.0), Some(250.0), Some(250.0)]
        );
        assert_eq!(
            min_w,
            vec![Some(10), Some(10), Some(20), Some(200), Some(200)]
        );
        assert_eq!(
            max_w,
            vec![Some(20), Some(20), Some(20), Some(300), Some(300)]
        );

        // Running Decimal128 sum & avg over peer group:
        // ("a", ord=1) peers sum = 100 + 150 = 250 (avg = 125 * 10^4); ("a", ord=2) = 450 (avg = 150 * 10^4);
        // ("b", 5) = 2000 (avg = 2000 * 10^4); ("b", 6) = 5000 (avg = 2500 * 10^4)
        let sum_cost: Vec<Option<i128>> = (0..5)
            .map(|r| eval::read_decimal(col("sum_cost"), r))
            .collect();
        assert_eq!(
            sum_cost,
            vec![Some(250), Some(250), Some(450), Some(2_000), Some(5_000)]
        );
        let avg_cost: Vec<Option<i128>> = (0..5)
            .map(|r| eval::read_decimal(col("avg_cost"), r))
            .collect();
        assert_eq!(
            avg_cost,
            vec![
                Some(125 * 10_000),
                Some(125 * 10_000),
                Some(150 * 10_000),
                Some(2_000 * 10_000),
                Some(2_500 * 10_000),
            ]
        );
        // Empty frame on first row of each partition: ROWS BETWEEN 2 PRECEDING AND 1 PRECEDING
        let prev2: Vec<Option<i64>> = (0..5)
            .map(|r| eval::read_i64(col("prev2_sum"), r))
            .collect();
        assert_eq!(prev2, vec![None, Some(10), Some(30), None, Some(200)]);

        // 3. Whole-partition default frame when ORDER BY is empty
        let mut parts2 = src
            .open_partitions(
                &ScanSpec::default(),
                &ExecContext {
                    target_partitions: 1,
                    ..ctx.clone()
                },
            )
            .unwrap();
        let no_order_specs = vec![
            WindowSpec {
                partition_by: vec![col_expr(0, "grp", DataType::Utf8)],
                order_by: vec![],
                frame: None,
                func: WindowFunc::Sum(col_expr(2, "val", DataType::I64)),
                output_name: "whole_sum".to_owned(),
            },
            WindowSpec {
                partition_by: vec![col_expr(0, "grp", DataType::Utf8)],
                order_by: vec![],
                frame: None,
                func: WindowFunc::Count(None),
                output_name: "whole_cnt".to_owned(),
            },
        ];
        let mut win_no_ord = eval_window_group(
            parts2.remove(0),
            Arc::clone(&schema),
            vec![col_expr(0, "grp", DataType::Utf8)],
            vec![],
            no_order_specs,
            &ctx,
        )
        .unwrap();
        let no_ord_out = drain_stream(win_no_ord.as_mut(), &ctx).unwrap();
        drop(win_no_ord);
        let no_ord_merged = {
            let s = Arc::clone(no_ord_out[0].schema());
            let r: Vec<&Batch> = no_ord_out.iter().collect();
            Batch::concat(s, &r, None).unwrap()
        };
        let wsum: Vec<Option<i64>> = (0..5)
            .map(|r| eval::read_i64(no_ord_merged.column_by_name("whole_sum").unwrap(), r))
            .collect();
        let wcnt: Vec<i64> = (0..5)
            .map(|r| eval::read_i64(no_ord_merged.column_by_name("whole_cnt").unwrap(), r).unwrap())
            .collect();
        assert_eq!(
            wsum,
            vec![Some(30), Some(30), Some(30), Some(500), Some(500)]
        );
        assert_eq!(wcnt, vec![3, 3, 3, 2, 2]);
        assert_eq!(MAX_MERGE_WAY, 32);
        assert_eq!(ctx.pool.reserved(), 0);
    }

    #[test]
    fn contract_validation_for_window_frames_group_and_merge_sinks() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path(), 1024 * 1024, 1024 * 1024);
        let schema = Schema::try_new_arc(vec![
            Field::new("grp", DataType::Utf8, false),
            Field::new("ord", DataType::I64, false),
        ])
        .unwrap();
        let other_schema = Schema::try_new_arc(vec![
            Field::new("grp", DataType::Utf8, false),
            Field::new("other", DataType::I64, false),
        ])
        .unwrap();

        let p_by = vec![col_expr(0, "grp", DataType::Utf8)];
        let o_by = vec![SortKey {
            expr: col_expr(1, "ord", DataType::I64),
            descending: false,
            nulls_first: false,
        }];

        // 1. Invalid frame bounds in validate_window_spec
        for bad_frame in [
            (FrameBound::CurrentRow, FrameBound::Preceding(1)),
            (FrameBound::Following(1), FrameBound::CurrentRow),
            (FrameBound::Following(2), FrameBound::Preceding(1)),
        ] {
            let err = eval_window_group(
                Box::new(EmptyStream),
                Arc::clone(&schema),
                p_by.clone(),
                o_by.clone(),
                vec![WindowSpec {
                    partition_by: p_by.clone(),
                    order_by: o_by.clone(),
                    frame: Some(bad_frame),
                    func: WindowFunc::Count(None),
                    output_name: "c".to_owned(),
                }],
                &ctx,
            )
            .err()
            .expect("invalid frame bound must fail");
            assert!(matches!(err, QueryError::Invalid(_)), "got {err:?}");
        }

        // 2. Mismatched partition_by / order_by in eval_window_group
        let err_p = eval_window_group(
            Box::new(EmptyStream),
            Arc::clone(&schema),
            p_by.clone(),
            o_by.clone(),
            vec![WindowSpec {
                partition_by: vec![],
                order_by: o_by.clone(),
                frame: None,
                func: WindowFunc::RowNumber,
                output_name: "rn".to_owned(),
            }],
            &ctx,
        )
        .err()
        .expect("mismatched partition_by must fail");
        assert!(matches!(err_p, QueryError::Invalid(_)), "got {err_p:?}");

        let err_o = eval_window_group(
            Box::new(EmptyStream),
            Arc::clone(&schema),
            p_by.clone(),
            o_by.clone(),
            vec![WindowSpec {
                partition_by: p_by,
                order_by: vec![],
                frame: None,
                func: WindowFunc::RowNumber,
                output_name: "rn".to_owned(),
            }],
            &ctx,
        )
        .err()
        .expect("mismatched order_by must fail");
        assert!(matches!(err_o, QueryError::Invalid(_)), "got {err_o:?}");

        // 3. Mismatched schema / keys in ExternalSort::merge_sinks
        let sink_bad_schema =
            ExternalSort::new(Arc::clone(&other_schema), o_by.clone(), None, ctx.clone());
        let err_s = ExternalSort::merge_sinks(
            vec![sink_bad_schema],
            Arc::clone(&schema),
            o_by.clone(),
            None,
            &ctx,
        )
        .err()
        .expect("mismatched sink schema must fail");
        assert!(matches!(err_s, QueryError::Column(_)), "got {err_s:?}");

        let sink_bad_keys = ExternalSort::new(Arc::clone(&schema), vec![], None, ctx.clone());
        let err_k =
            ExternalSort::merge_sinks(vec![sink_bad_keys], Arc::clone(&schema), o_by, None, &ctx)
                .err()
                .expect("mismatched sink keys must fail");
        assert!(matches!(err_k, QueryError::Invalid(_)), "got {err_k:?}");
    }

    #[test]
    fn external_sort_and_window_proactively_spill_with_wide_batches_and_scan_res() {
        let dir = tempfile::tempdir().unwrap();
        // 32 MiB pool cap with ~4.7 MiB batches (~42 MiB total) so unspilled accumulation
        // without proactive headroom would exhaust `cap` on `scan_res` at `spilled == 0`.
        let ctx = test_ctx(dir.path(), 32 * 1024 * 1024, 256 * 1024 * 1024);
        let schema = Schema::try_new_arc(vec![
            Field::new("part", DataType::I64, false),
            Field::new("id", DataType::I64, false),
            Field::new("payload", DataType::Utf8, false),
        ])
        .unwrap();

        let rows_per_batch = 4600_usize;
        let num_batches = 9_usize;
        let payload = "p".repeat(1024);
        let mut batches = Vec::with_capacity(num_batches);
        for b in 0..num_batches {
            let parts = vec![i64::try_from(b).unwrap(); rows_per_batch];
            let ids: Vec<i64> = (0..rows_per_batch)
                .map(|r| i64::try_from(rows_per_batch - r).unwrap())
                .collect();
            let payloads = vec![payload.as_str(); rows_per_batch];
            batches.push(
                Batch::try_new_all(
                    Arc::clone(&schema),
                    vec![
                        Column::Flat(FlatColumn::try_new(FlatData::I64(parts), None).unwrap()),
                        Column::Flat(FlatColumn::try_new(FlatData::I64(ids), None).unwrap()),
                        Column::Flat(
                            FlatColumn::try_new(
                                FlatData::Utf8(Utf8Data::try_from_slices(payloads).unwrap()),
                                None,
                            )
                            .unwrap(),
                        ),
                    ],
                    rows_per_batch,
                )
                .unwrap(),
            );
        }

        // 1. Direct ExternalSort with a concurrent unspillable `scan_res` held during `consume`.
        let mut single_ctx = ctx;
        single_ctx.target_partitions = 1;
        let keys = vec![
            SortKey {
                expr: col_expr(0, "part", DataType::I64),
                descending: false,
                nulls_first: false,
            },
            SortKey {
                expr: col_expr(2, "payload", DataType::Utf8),
                descending: false,
                nulls_first: false,
            },
            SortKey {
                expr: col_expr(1, "id", DataType::I64),
                descending: false,
                nulls_first: false,
            },
        ];
        let src = BatchSource::new(Arc::clone(&schema), batches.clone()).unwrap();
        let mut scan_stream = src
            .open_partitions(&ScanSpec::default(), &single_ctx)
            .unwrap()
            .remove(0);
        let mut sort = ExternalSort::new(Arc::clone(&schema), keys, None, single_ctx.clone());
        let mut scan_res = single_ctx.pool.register("scan_res", false);
        while let Some(batch) = scan_stream
            .next_batch(&single_ctx.eval.cancel, Some(&mut scan_res))
            .unwrap()
        {
            assert_eq!(
                sort.consume(batch, &mut scan_res).unwrap(),
                ControlFlow::Continue
            );
            scan_res.free();
        }
        drop(scan_res);
        drop(scan_stream);
        assert!(
            sort.spilled_run_count() > 0,
            "expected proactive spill before scan_res exceeded 32 MiB cap"
        );
        let mut sorted = sort.into_stream().unwrap();
        let sorted_out = drain_stream(sorted.as_mut(), &single_ctx).unwrap();
        drop(sorted);
        assert_eq!(
            sorted_out.iter().map(Batch::num_rows).sum::<usize>(),
            num_batches * rows_per_batch
        );
        assert_eq!(single_ctx.spill.spilled_bytes(), 0);
        assert_eq!(single_ctx.pool.reserved(), 0);

        // 2. End-to-end WindowStream (`PARTITION BY part ORDER BY payload, id`) over BatchSource.
        let win_src = BatchSource::new(Arc::clone(&schema), batches).unwrap();
        let win_in = win_src
            .open_partitions(&ScanSpec::default(), &single_ctx)
            .unwrap()
            .remove(0);
        let p_by = vec![col_expr(0, "part", DataType::I64)];
        let o_by = vec![
            SortKey {
                expr: col_expr(2, "payload", DataType::Utf8),
                descending: false,
                nulls_first: false,
            },
            SortKey {
                expr: col_expr(1, "id", DataType::I64),
                descending: false,
                nulls_first: false,
            },
        ];
        let mut win = eval_window_group(
            win_in,
            Arc::clone(&schema),
            p_by.clone(),
            o_by.clone(),
            vec![WindowSpec {
                partition_by: p_by,
                order_by: o_by,
                frame: None,
                func: WindowFunc::RowNumber,
                output_name: "rn".to_owned(),
            }],
            &single_ctx,
        )
        .unwrap();
        let win_out = drain_stream(win.as_mut(), &single_ctx).unwrap();
        drop(win);
        assert_eq!(
            win_out.iter().map(Batch::num_rows).sum::<usize>(),
            num_batches * rows_per_batch
        );
        assert_eq!(single_ctx.spill.spilled_bytes(), 0);
        assert_eq!(single_ctx.pool.reserved(), 0);
    }

    #[test]
    fn external_sort_default_max_merge_way_32_succeeds_in_64kib_pool() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path(), 64 * 1024, 10 * 1024 * 1024);
        let schema = Schema::try_new_arc(vec![
            Field::new("k", DataType::I64, false),
            Field::new("v", DataType::I64, false),
        ])
        .unwrap();
        let keys = vec![SortKey {
            expr: col_expr(0, "k", DataType::I64),
            descending: false,
            nulls_first: false,
        }];

        // Use default max_merge_way = 32!
        let mut sort = ExternalSort::new(Arc::clone(&schema), keys, None, ctx.clone());
        let mut dummy_res = ctx.pool.register("in", false);
        // Feed 700 batches of 128 rows (~2 KiB each = ~1.4 MiB total -> >20 runs in 64 KiB pool).
        let mut total_rows = 0usize;
        for batch_id in 0..700i64 {
            let ks: Vec<i64> = (0..128i64)
                .map(|j| (batch_id * 31 + j * 17) % 10_000)
                .collect();
            let vs: Vec<i64> = (0..128i64).map(|j| batch_id * 128 + j).collect();
            total_rows += 128;
            let b = Batch::try_new_all(
                Arc::clone(&schema),
                vec![
                    Column::Flat(FlatColumn::try_new(FlatData::I64(ks), None).unwrap()),
                    Column::Flat(FlatColumn::try_new(FlatData::I64(vs), None).unwrap()),
                ],
                128,
            )
            .unwrap();
            assert_eq!(
                sort.consume(b, &mut dummy_res).unwrap(),
                ControlFlow::Continue
            );
        }
        drop(dummy_res);
        assert!(sort.spilled_run_count() > 20);

        let mut stream = sort.into_stream().unwrap();
        let out = drain_stream(stream.as_mut(), &ctx).unwrap();
        drop(stream);
        assert_eq!(ctx.spill.spilled_bytes(), 0);
        assert_eq!(ctx.pool.reserved(), 0);

        let mut prev = i64::MIN;
        let mut got_rows = 0usize;
        for b in &out {
            for r in 0..b.num_rows() {
                let k = eval::read_i64(b.column(0).unwrap(), r).unwrap();
                assert!(k >= prev, "out of order: {k} < {prev}");
                prev = k;
                got_rows += 1;
            }
        }
        assert_eq!(got_rows, total_rows);
    }

    #[test]
    fn streaming_window_50k_rows_succeeds_in_64kib_pool() {
        let dir = tempfile::tempdir().unwrap();
        // Tight 64 KiB pool across 50,000 unpartitioned rows!
        let mut ctx = test_ctx(dir.path(), 64 * 1024, 64 * 1024 * 1024);
        ctx.batch_size = 256;
        ctx.target_partitions = 1;

        let schema = Schema::try_new_arc(vec![
            Field::new("k", DataType::I64, false),
            Field::new("val", DataType::I64, false),
        ])
        .unwrap();

        let total_rows = 50_000usize;
        let chunk = 250usize;
        let mut batches = Vec::with_capacity(total_rows / chunk);
        for b_idx in 0..(total_rows / chunk) {
            let mut ks = Vec::with_capacity(chunk);
            let mut vs = Vec::with_capacity(chunk);
            for j in 0..chunk {
                let row_num = (b_idx * chunk + j) as i64;
                // Deterministic pseudo-shuffled keys with peer ties (div by 2)
                let rev = (total_rows as i64 - 1) - row_num;
                ks.push(rev / 2);
                vs.push((rev / 2) + 1);
            }
            batches.push(
                Batch::try_new_all(
                    Arc::clone(&schema),
                    vec![
                        Column::Flat(FlatColumn::try_new(FlatData::I64(ks), None).unwrap()),
                        Column::Flat(FlatColumn::try_new(FlatData::I64(vs), None).unwrap()),
                    ],
                    chunk,
                )
                .unwrap(),
            );
        }

        let order_by = vec![SortKey {
            expr: col_expr(0, "k", DataType::I64),
            descending: false,
            nulls_first: false,
        }];
        let specs = vec![
            WindowSpec {
                partition_by: vec![],
                order_by: order_by.clone(),
                frame: None,
                func: WindowFunc::RowNumber,
                output_name: "rn".to_owned(),
            },
            WindowSpec {
                partition_by: vec![],
                order_by: order_by.clone(),
                frame: None,
                func: WindowFunc::Rank,
                output_name: "rk".to_owned(),
            },
            WindowSpec {
                partition_by: vec![],
                order_by: order_by.clone(),
                frame: None,
                func: WindowFunc::DenseRank,
                output_name: "drk".to_owned(),
            },
            WindowSpec {
                partition_by: vec![],
                order_by: order_by.clone(),
                frame: None,
                func: WindowFunc::Lag {
                    expr: col_expr(0, "k", DataType::I64),
                    offset: 1,
                    default: Some(Expr::Literal(ScalarValue::I64(-1))),
                },
                output_name: "lag1".to_owned(),
            },
            WindowSpec {
                partition_by: vec![],
                order_by: order_by.clone(),
                frame: Some((FrameBound::UnboundedPreceding, FrameBound::CurrentRow)),
                func: WindowFunc::Sum(col_expr(1, "val", DataType::I64)),
                output_name: "run_sum".to_owned(),
            },
        ];
        assert!(specs_are_zero_lookahead_streamable(&specs));

        let src = BatchSource::new(Arc::clone(&schema), batches).unwrap();
        let input = src
            .open_partitions(&ScanSpec::default(), &ctx)
            .unwrap()
            .remove(0);
        let mut win = eval_window_group(
            input,
            Arc::clone(&schema),
            vec![],
            order_by.clone(),
            specs,
            &ctx,
        )
        .unwrap();
        let out = drain_stream(win.as_mut(), &ctx).unwrap();
        drop(win);
        assert_eq!(ctx.spill.spilled_bytes(), 0);
        assert_eq!(ctx.pool.reserved(), 0);

        let mut seen = 0i64;
        let mut running_sum = 0i64;
        for b in &out {
            for r in 0..b.num_rows() {
                let k = eval::read_i64(b.column(0).unwrap(), r).unwrap();
                let val = eval::read_i64(b.column(1).unwrap(), r).unwrap();
                let rn = eval::read_i64(b.column(2).unwrap(), r).unwrap();
                let rk = eval::read_i64(b.column(3).unwrap(), r).unwrap();
                let drk = eval::read_i64(b.column(4).unwrap(), r).unwrap();
                let lag1 = eval::read_i64(b.column(5).unwrap(), r).unwrap();
                let rsum = eval::read_i64(b.column(6).unwrap(), r).unwrap();

                let expected_k = seen / 2;
                assert_eq!(k, expected_k);
                assert_eq!(rn, seen + 1);
                assert_eq!(rk, (seen & !1) + 1);
                assert_eq!(drk, (seen / 2) + 1);
                let expected_lag = if seen == 0 { -1 } else { (seen - 1) / 2 };
                assert_eq!(lag1, expected_lag);
                running_sum += val;
                assert_eq!(rsum, running_sum);
                seen += 1;
            }
        }
        assert_eq!(seen, total_rows as i64);

        // Verify non-streamable specs return false and order_by.is_empty() Rank/DenseRank emit 1
        assert!(!specs_are_zero_lookahead_streamable(&[WindowSpec {
            partition_by: vec![],
            order_by: vec![],
            frame: None,
            func: WindowFunc::Sum(col_expr(0, "k", DataType::I64)),
            output_name: "s".to_owned(),
        }]));
        assert!(!specs_are_zero_lookahead_streamable(&[WindowSpec {
            partition_by: vec![],
            order_by: vec![],
            frame: None,
            func: WindowFunc::Lead {
                expr: col_expr(0, "k", DataType::I64),
                offset: 1,
                default: None,
            },
            output_name: "ld".to_owned(),
        }]));
        assert!(specs_are_zero_lookahead_streamable(&[WindowSpec {
            partition_by: vec![],
            order_by: vec![],
            frame: None,
            func: WindowFunc::Rank,
            output_name: "rk".to_owned(),
        }]));

        // H3a-B3: streaming empty frame ROWS BETWEEN 0 PRECEDING AND 2 PRECEDING.
        let empty_frame = Some((FrameBound::Preceding(0), FrameBound::Preceding(2)));
        let empty_specs = vec![
            WindowSpec {
                partition_by: vec![],
                order_by: order_by.clone(),
                frame: empty_frame,
                func: WindowFunc::Count(None),
                output_name: "cnt_star".to_owned(),
            },
            WindowSpec {
                partition_by: vec![],
                order_by: order_by.clone(),
                frame: empty_frame,
                func: WindowFunc::Count(Some(col_expr(1, "val", DataType::I64))),
                output_name: "cnt_expr".to_owned(),
            },
            WindowSpec {
                partition_by: vec![],
                order_by: order_by.clone(),
                frame: empty_frame,
                func: WindowFunc::Sum(col_expr(1, "val", DataType::I64)),
                output_name: "sum".to_owned(),
            },
            WindowSpec {
                partition_by: vec![],
                order_by: order_by.clone(),
                frame: empty_frame,
                func: WindowFunc::Avg(col_expr(1, "val", DataType::I64)),
                output_name: "avg".to_owned(),
            },
            WindowSpec {
                partition_by: vec![],
                order_by: order_by.clone(),
                frame: empty_frame,
                func: WindowFunc::Min(col_expr(1, "val", DataType::I64)),
                output_name: "min".to_owned(),
            },
            WindowSpec {
                partition_by: vec![],
                order_by: order_by.clone(),
                frame: empty_frame,
                func: WindowFunc::Max(col_expr(1, "val", DataType::I64)),
                output_name: "max".to_owned(),
            },
            WindowSpec {
                partition_by: vec![],
                order_by: order_by.clone(),
                frame: empty_frame,
                func: WindowFunc::FirstValue(col_expr(1, "val", DataType::I64)),
                output_name: "fv".to_owned(),
            },
            WindowSpec {
                partition_by: vec![],
                order_by: order_by.clone(),
                frame: empty_frame,
                func: WindowFunc::LastValue(col_expr(1, "val", DataType::I64)),
                output_name: "lv".to_owned(),
            },
        ];
        assert!(specs_are_zero_lookahead_streamable(&empty_specs));
        let empty_batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1, 2, 3]), None).unwrap()),
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![10, 20, 30]), None).unwrap()),
            ],
            3,
        )
        .unwrap();
        let empty_src = BatchSource::new(Arc::clone(&schema), vec![empty_batch]).unwrap();
        let empty_in = empty_src
            .open_partitions(&ScanSpec::default(), &ctx)
            .unwrap()
            .remove(0);
        let mut empty_win = eval_window_group(
            empty_in,
            Arc::clone(&schema),
            vec![],
            order_by,
            empty_specs,
            &ctx,
        )
        .unwrap();
        let empty_out = drain_stream(empty_win.as_mut(), &ctx).unwrap();
        drop(empty_win);
        assert_eq!(ctx.pool.reserved(), 0);
        assert_eq!(empty_out.len(), 1);
        for r in 0..3 {
            assert_eq!(
                empty_out[0].column(2).unwrap().get_scalar(r).unwrap(),
                ScalarValue::I64(0)
            );
            assert_eq!(
                empty_out[0].column(3).unwrap().get_scalar(r).unwrap(),
                ScalarValue::I64(0)
            );
            for c in 4..=9 {
                assert!(
                    empty_out[0].column(c).unwrap().is_null(r),
                    "col {c} row {r} not null"
                );
            }
        }

        // H3a-M4: wide Const(Utf8) LAG(..., 4096) hits TooLarge cleanly under tight pool.
        let tight_dir = tempfile::tempdir().unwrap();
        let tight_ctx = test_ctx(tight_dir.path(), 512 * 1024, 1024 * 1024);
        let wide_schema = Schema::try_new_arc(vec![
            Field::new("k", DataType::I64, false),
            Field::new("s", DataType::Utf8, false),
        ])
        .unwrap();
        let wide_len = 512usize;
        let wide_batch = Batch::try_new_all(
            Arc::clone(&wide_schema),
            vec![
                Column::Flat(
                    FlatColumn::try_new(FlatData::I64((0..wide_len as i64).collect()), None)
                        .unwrap(),
                ),
                Column::try_new_const(ScalarValue::Utf8("x".repeat(4096)), wide_len).unwrap(),
            ],
            wide_len,
        )
        .unwrap();
        let wide_src = BatchSource::new(Arc::clone(&wide_schema), vec![wide_batch]).unwrap();
        let wide_in = wide_src
            .open_partitions(&ScanSpec::default(), &tight_ctx)
            .unwrap()
            .remove(0);
        let wide_order = vec![SortKey {
            expr: col_expr(0, "k", DataType::I64),
            descending: false,
            nulls_first: false,
        }];
        let wide_specs = vec![WindowSpec {
            partition_by: vec![],
            order_by: wide_order.clone(),
            frame: None,
            func: WindowFunc::Lag {
                expr: col_expr(1, "s", DataType::Utf8),
                offset: 4096,
                default: None,
            },
            output_name: "lag_wide".to_owned(),
        }];
        let mut wide_win = eval_window_group(
            wide_in,
            Arc::clone(&wide_schema),
            vec![],
            wide_order,
            wide_specs,
            &tight_ctx,
        )
        .unwrap();
        let err = drain_stream(wide_win.as_mut(), &tight_ctx).unwrap_err();
        drop(wide_win);
        assert!(matches!(err, QueryError::TooLarge));
        assert_eq!(tight_ctx.pool.reserved(), 0);
    }

    #[test]
    fn external_sort_spill_single_batch_respects_selection_and_finish_releases_res() {
        use crate::ir::BinaryOp;

        let dir = tempfile::tempdir().unwrap();
        let schema = Schema::try_new_arc(vec![
            Field::new("k", DataType::I64, false),
            Field::new("payload", DataType::Utf8, false),
        ])
        .unwrap();

        // Build a batch with Selection::Mask where only even rows are active.
        let n = 64usize;
        let k_vals: Vec<i64> = (0..n as i64).rev().collect();
        let pads: Vec<String> = (0..n).map(|i| format!("row-{i:0>200}")).collect();
        let pad_refs: Vec<&str> = pads.iter().map(String::as_str).collect();
        let mask_bools: Vec<bool> = (0..n).map(|i| i % 2 == 0).collect();
        let filtered_batch = Batch::try_new(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(k_vals), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(pad_refs).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            n,
            Selection::Mask(Bitmap::from_bools(&mask_bools)),
        )
        .unwrap();

        let batch_bytes = filtered_batch.memory_bytes();
        // Pool cap allows holding `batch_res` + small expression key column, but NOT compacting
        // `filtered_batch` into a second reservation (forces `spill_single_batch`).
        let cap = batch_bytes + 4096;
        let ctx = test_ctx(dir.path(), cap, 4 * 1024 * 1024);

        // Expression sort key: `k + 1` ASC
        let keys = vec![SortKey {
            expr: Expr::Binary {
                left: Box::new(col_expr(0, "k", DataType::I64)),
                op: BinaryOp::Add,
                right: Box::new(Expr::Literal(ScalarValue::I64(1))),
                data_type: DataType::I64,
            },
            descending: false,
            nulls_first: false,
        }];

        let mut sink = ExternalSort::new(Arc::clone(&schema), keys.clone(), None, ctx.clone());
        let mut scan_res = ctx.pool.register("scan", false);
        scan_res.try_grow(batch_bytes).unwrap();
        assert_eq!(
            sink.consume(filtered_batch, &mut scan_res).unwrap(),
            ControlFlow::Continue
        );
        scan_res.free();
        drop(scan_res);
        assert_eq!(sink.spilled_run_count(), 1);

        // `finish()` drops `res` for spilled / empty sinks so sibling partitions & merge_sinks
        // observe full `spill_share()`.
        sink.finish().unwrap();
        assert!(sink.res.is_none());
        assert_eq!(ctx.pool.reserved(), 0);

        let mut empty_sink =
            ExternalSort::new(Arc::clone(&schema), keys.clone(), None, ctx.clone());
        empty_sink.finish().unwrap();
        assert!(empty_sink.res.is_none());

        let mut merged = ExternalSort::merge_sinks(
            vec![empty_sink, sink],
            Arc::clone(&schema),
            keys,
            None,
            &ctx,
        )
        .unwrap();
        let out = drain_stream(merged.as_mut(), &ctx).unwrap();
        drop(merged);
        assert_eq!(ctx.pool.reserved(), 0);

        let got_keys: Vec<i64> = out
            .iter()
            .flat_map(|b| {
                let c = b.column(0).unwrap();
                (0..b.num_rows()).map(move |r| match c.get_scalar(r).unwrap() {
                    ScalarValue::I64(v) => v,
                    other => panic!("unexpected scalar: {other:?}"),
                })
            })
            .collect();
        // Active rows had indices 0, 2, 4, ..., 62 -> k values 63, 61, ..., 1 -> sorted ASC: 1, 3, ..., 63
        let expected: Vec<i64> = (1..64).step_by(2).collect();
        assert_eq!(got_keys, expected);
    }

    #[test]
    fn window_multi_batch_partition_expr_key_and_inner_loop_cancel() {
        use crate::ir::BinaryOp;

        let dir = tempfile::tempdir().unwrap();
        let mut ctx = test_ctx(dir.path(), 512 * 1024, 4 * 1024 * 1024);
        ctx.batch_size = 16;
        ctx.target_partitions = 1;

        let schema = Schema::try_new_arc(vec![
            Field::new("p", DataType::I64, false),
            Field::new("v", DataType::I64, false),
        ])
        .unwrap();

        // 4 batches of 16 rows in the same partition (p=1), plus 1 batch of p=2.
        let mut batches = Vec::new();
        for chunk in 0..4 {
            let p = vec![1i64; 16];
            let v: Vec<i64> = (chunk * 16..(chunk + 1) * 16).map(|x| x as i64).collect();
            batches.push(
                Batch::try_new_all(
                    Arc::clone(&schema),
                    vec![
                        Column::Flat(FlatColumn::try_new(FlatData::I64(p), None).unwrap()),
                        Column::Flat(FlatColumn::try_new(FlatData::I64(v), None).unwrap()),
                    ],
                    16,
                )
                .unwrap(),
            );
        }
        batches.push(
            Batch::try_new_all(
                Arc::clone(&schema),
                vec![
                    Column::Flat(FlatColumn::try_new(FlatData::I64(vec![2; 8]), None).unwrap()),
                    Column::Flat(
                        FlatColumn::try_new(FlatData::I64((0..8).collect()), None).unwrap(),
                    ),
                ],
                8,
            )
            .unwrap(),
        );

        let src = BatchSource::new(Arc::clone(&schema), batches).unwrap();
        let input = src
            .open_partitions(&ScanSpec::default(), &ctx)
            .unwrap()
            .remove(0);

        // Expression PARTITION BY (`p + 0`) + non-streamable frame (UNBOUNDED FOLLOWING)
        let part_expr = Expr::Binary {
            left: Box::new(col_expr(0, "p", DataType::I64)),
            op: BinaryOp::Add,
            right: Box::new(Expr::Literal(ScalarValue::I64(0))),
            data_type: DataType::I64,
        };
        let order_by = vec![SortKey {
            expr: col_expr(1, "v", DataType::I64),
            descending: false,
            nulls_first: false,
        }];
        let specs = vec![WindowSpec {
            partition_by: vec![part_expr.clone()],
            order_by: order_by.clone(),
            frame: Some((FrameBound::CurrentRow, FrameBound::UnboundedFollowing)),
            func: WindowFunc::Sum(col_expr(1, "v", DataType::I64)),
            output_name: "suffix_sum".to_owned(),
        }];

        let mut win = eval_window_group(
            input,
            Arc::clone(&schema),
            vec![part_expr],
            order_by,
            specs,
            &ctx,
        )
        .unwrap();
        let out = drain_stream(win.as_mut(), &ctx).unwrap();
        drop(win);
        assert_eq!(ctx.pool.reserved(), 0);
        let total_rows: usize = out.iter().map(Batch::num_rows).sum();
        assert_eq!(total_rows, 72);
        // First row of partition p=1 has suffix_sum = 0+1+...+63 = 2016
        assert_eq!(
            out[0].column(2).unwrap().get_scalar(0).unwrap(),
            ScalarValue::I64(63 * 64 / 2)
        );

        // Cancelled token is observed immediately inside non-prefix and prefix window loops
        let cancelled = CancelToken::new();
        cancelled.cancel();
        let cancel_eval = EvalContext::new(0, cancelled);
        let big_n = 2048usize;
        let big_batch = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1; big_n]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(FlatData::I64((0..big_n as i64).collect()), None).unwrap(),
                ),
            ],
            big_n,
        )
        .unwrap();
        let mut res = ctx.pool.register("cancel_win", false);
        let err = eval_window_sum(
            &col_expr(1, "v", DataType::I64),
            &DataType::I64,
            Some((FrameBound::CurrentRow, FrameBound::UnboundedFollowing)),
            false,
            None,
            &big_batch,
            &cancel_eval,
            &mut res,
        )
        .unwrap_err();
        assert!(matches!(err, QueryError::Cancelled));
        assert_eq!(res.size(), 0);
    }

    #[test]
    fn f64_to_sortable_u64_matches_f64_cmp_all_edge_cases() {
        let vals = [
            f64::NEG_INFINITY,
            -1e300,
            -1.0,
            -0.0,
            0.0,
            1e-300,
            1.0,
            f64::INFINITY,
            f64::NAN,
            -f64::NAN,
            f64::from_bits(0x7ff8_0000_0000_0001),
        ];
        assert_eq!(f64_to_sortable_u64(-0.0), f64_to_sortable_u64(0.0));
        assert_eq!(f64_to_sortable_u64(f64::NAN), u64::MAX);
        assert_eq!(f64_to_sortable_u64(-f64::NAN), u64::MAX);
        assert_eq!(
            f64_to_sortable_u64(f64::from_bits(0x7ff8_0000_0000_0001)),
            u64::MAX
        );

        for (i, &a) in vals.iter().enumerate() {
            for (j, &b) in vals.iter().enumerate() {
                let got = f64_to_sortable_u64(a).cmp(&f64_to_sortable_u64(b));
                let expected = eval::f64_cmp(a, b);
                assert_eq!(
                    got, expected,
                    "mismatch for pair ({i}: {a:?}, {j}: {b:?}): got {got:?}, expected {expected:?}"
                );
            }
        }
    }

    fn assert_specialized_matches_generic(
        key_refs: &[Vec<&Column>],
        keys: &[SortKey],
        refs: &[(u32, u32)],
    ) {
        let mut expected = refs.to_vec();
        expected.sort_by(|&(ba, ra), &(bb, rb)| {
            compare_key_refs_across(
                &key_refs[ba as usize],
                ra as usize,
                &key_refs[bb as usize],
                rb as usize,
                keys,
            )
        });
        let mut actual = refs.to_vec();
        sort_row_refs_specialized(key_refs, keys, &mut actual);
        assert_eq!(actual, expected);
    }

    fn all_refs_for(batches: &[Vec<&Column>]) -> Vec<(u32, u32)> {
        let mut refs = Vec::new();
        for (b_idx, cols) in batches.iter().enumerate() {
            let n = cols[0].len();
            for r_idx in 0..n {
                refs.push((b_idx as u32, r_idx as u32));
            }
        }
        refs
    }

    #[test]
    fn specialized_sort_matches_generic_across_1key_and_2key_types_nulls_and_ties() {
        use cetra_column::{DictColumn, RleColumn};

        let flag_combos = [(false, false), (false, true), (true, false), (true, true)];

        // --- 1-key: FixedBytes16 (non-null Flat, nullable Flat with non-zero payload under NULLs, mixed encodings) ---
        let id_a = [0x01u8; 16];
        let id_b = [0x02u8; 16];
        let id_c = [0xffu8; 16];
        let id_zero = [0x00u8; 16];
        let f16_flat_nn = Column::Flat(
            FlatColumn::try_new(
                FlatData::FixedBytes16(vec![id_c, id_a, id_b, id_a, id_zero]),
                None,
            )
            .unwrap(),
        );
        // Underlying buffer at null rows (1, 3) is non-zero (`id_zero`, `id_c`) to catch missing null checks.
        let f16_flat_null = Column::Flat(
            FlatColumn::try_new(
                FlatData::FixedBytes16(vec![id_b, id_zero, id_a, id_c, id_b]),
                Some(Bitmap::from_bools(&[true, false, true, false, true])),
            )
            .unwrap(),
        );
        let f16_dict = Column::Dict(
            DictColumn::try_new(
                FlatColumn::try_new(FlatData::FixedBytes16(vec![id_c, id_a]), None).unwrap(),
                vec![0, 1, 0],
                Some(Bitmap::from_bools(&[true, false, true])),
            )
            .unwrap(),
        );
        let f16_const_null =
            Column::try_new_const(ScalarValue::Null(DataType::FixedBytes16), 2).unwrap();

        // --- 1-key: I64 ---
        let i64_flat_nn = Column::Flat(
            FlatColumn::try_new(FlatData::I64(vec![42, -5, 0, 42, i64::MIN, i64::MAX]), None)
                .unwrap(),
        );
        // Underlying values at null rows (0, 2) are `0` and `-100`.
        let i64_flat_null = Column::Flat(
            FlatColumn::try_new(
                FlatData::I64(vec![0, 10, -100, 10, 5]),
                Some(Bitmap::from_bools(&[false, true, false, true, true])),
            )
            .unwrap(),
        );
        let i64_rle = Column::Rle(
            RleColumn::try_new(
                FlatColumn::try_new(
                    FlatData::I64(vec![0, 10]),
                    Some(Bitmap::from_bools(&[false, true])),
                )
                .unwrap(),
                vec![2, 4],
            )
            .unwrap(),
        );
        let i64_const = Column::try_new_const(ScalarValue::I64(10), 3).unwrap();

        // --- 1-key: TimestampNs ---
        let ts_flat_nn = Column::Flat(
            FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: vec![300, 100, 200, 100],
                    utc: true,
                },
                None,
            )
            .unwrap(),
        );
        let ts_flat_null = Column::Flat(
            FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: vec![0, 200, 999, 100],
                    utc: true,
                },
                Some(Bitmap::from_bools(&[false, true, false, true])),
            )
            .unwrap(),
        );
        let ts_const = Column::try_new_const(
            ScalarValue::TimestampNs {
                value: 200,
                utc: true,
            },
            2,
        )
        .unwrap();

        // --- 1-key: F64 ---
        let f64_flat_nn = Column::Flat(
            FlatColumn::try_new(
                FlatData::F64(vec![f64::NAN, 1.0, -0.0, 0.0, f64::NEG_INFINITY, -f64::NAN]),
                None,
            )
            .unwrap(),
        );
        let f64_flat_null = Column::Flat(
            FlatColumn::try_new(
                FlatData::F64(vec![0.0, 2.5, -1.0, f64::NAN, 0.0]),
                Some(Bitmap::from_bools(&[false, true, true, false, true])),
            )
            .unwrap(),
        );
        let f64_dict = Column::Dict(
            DictColumn::try_new(
                FlatColumn::try_new(FlatData::F64(vec![-0.0, f64::INFINITY]), None).unwrap(),
                vec![1, 0, 1],
                Some(Bitmap::from_bools(&[true, true, false])),
            )
            .unwrap(),
        );

        // --- 1-key: Utf8 ---
        // Include empty string `""` vs NULL (`""` stored in buffer under null bitmap!).
        let utf8_flat_nn = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(["banana", "", "apple", "banana", "🦀"]).unwrap(),
                ),
                None,
            )
            .unwrap(),
        );
        let utf8_flat_null = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(["", "banana", "aaa", "", "apple"]).unwrap(),
                ),
                Some(Bitmap::from_bools(&[false, true, false, true, true])),
            )
            .unwrap(),
        );
        let utf8_dict = Column::Dict(
            DictColumn::try_new(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(["apple", "cherry"]).unwrap()),
                    None,
                )
                .unwrap(),
                vec![1, 0, 1],
                Some(Bitmap::from_bools(&[true, false, true])),
            )
            .unwrap(),
        );
        let utf8_rle = Column::Rle(
            RleColumn::try_new(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(["", "banana"]).unwrap()),
                    Some(Bitmap::from_bools(&[false, true])),
                )
                .unwrap(),
                vec![1, 3],
            )
            .unwrap(),
        );
        let utf8_const = Column::try_new_const(ScalarValue::Utf8("apple".to_owned()), 2).unwrap();

        let one_key_cases: Vec<(DataType, Vec<Vec<&Column>>)> = vec![
            (DataType::FixedBytes16, vec![vec![&f16_flat_nn]]),
            (
                DataType::FixedBytes16,
                vec![vec![&f16_flat_null], vec![&f16_dict], vec![&f16_const_null]],
            ),
            (DataType::I64, vec![vec![&i64_flat_nn]]),
            (
                DataType::I64,
                vec![vec![&i64_flat_null], vec![&i64_rle], vec![&i64_const]],
            ),
            (DataType::TimestampNs { utc: true }, vec![vec![&ts_flat_nn]]),
            (
                DataType::TimestampNs { utc: true },
                vec![vec![&ts_flat_null], vec![&ts_const]],
            ),
            (DataType::F64, vec![vec![&f64_flat_nn]]),
            (DataType::F64, vec![vec![&f64_flat_null], vec![&f64_dict]]),
            (DataType::Utf8, vec![vec![&utf8_flat_nn]]),
            (
                DataType::Utf8,
                vec![
                    vec![&utf8_flat_null],
                    vec![&utf8_dict],
                    vec![&utf8_rle],
                    vec![&utf8_const],
                ],
            ),
        ];

        for (dt, batches) in &one_key_cases {
            let refs = all_refs_for(batches);
            for &(descending, nulls_first) in &flag_combos {
                let keys = vec![SortKey {
                    expr: col_expr(0, "k0", dt.clone()),
                    descending,
                    nulls_first,
                }];
                assert_specialized_matches_generic(batches, &keys, &refs);
            }
        }

        // --- 2-key pairs: (Utf8, I64), (I64, FixedBytes16), (FixedBytes16, I64), (I64, I64), (Utf8, Utf8) ---
        // Each batch has length 5 (non-null and nullable variants + mixed encodings).
        let u5_nn = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(["gpt-4", "claude", "gpt-4", "", "claude"]).unwrap(),
                ),
                None,
            )
            .unwrap(),
        );
        let u5_null = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    Utf8Data::try_from_slices(["", "gpt-4", "aaa", "gpt-4", ""]).unwrap(),
                ),
                Some(Bitmap::from_bools(&[false, true, false, true, true])),
            )
            .unwrap(),
        );
        let u5_dict = Column::Dict(
            DictColumn::try_new(
                FlatColumn::try_new(
                    FlatData::Utf8(Utf8Data::try_from_slices(["gpt-4", "claude"]).unwrap()),
                    None,
                )
                .unwrap(),
                vec![0, 1, 0, 1, 0],
                Some(Bitmap::from_bools(&[true, false, true, true, true])),
            )
            .unwrap(),
        );

        let i5_nn = Column::Flat(
            FlatColumn::try_new(FlatData::I64(vec![10, 20, 10, 30, 20]), None).unwrap(),
        );
        let i5_null = Column::Flat(
            FlatColumn::try_new(
                FlatData::I64(vec![0, 20, -1, 20, 10]),
                Some(Bitmap::from_bools(&[false, true, false, true, true])),
            )
            .unwrap(),
        );
        let ts5_nn = Column::Flat(
            FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: vec![100, 100, 200, 100, 200],
                    utc: true,
                },
                None,
            )
            .unwrap(),
        );
        let ts5_null = Column::Flat(
            FlatColumn::try_new(
                FlatData::TimestampNs {
                    values: vec![999, 100, 0, 100, 200],
                    utc: true,
                },
                Some(Bitmap::from_bools(&[false, true, false, true, true])),
            )
            .unwrap(),
        );
        let f5_nn = Column::Flat(
            FlatColumn::try_new(
                FlatData::FixedBytes16(vec![id_a, id_b, id_a, id_c, id_b]),
                None,
            )
            .unwrap(),
        );
        let f5_null = Column::Flat(
            FlatColumn::try_new(
                FlatData::FixedBytes16(vec![id_zero, id_a, id_c, id_a, id_b]),
                Some(Bitmap::from_bools(&[false, true, false, true, true])),
            )
            .unwrap(),
        );

        let two_key_cases: Vec<(DataType, DataType, Vec<Vec<&Column>>)> = vec![
            // (Utf8, I64) non-null and nullable + mixed
            (DataType::Utf8, DataType::I64, vec![vec![&u5_nn, &i5_nn]]),
            (
                DataType::Utf8,
                DataType::I64,
                vec![vec![&u5_null, &i5_null], vec![&u5_dict, &i5_nn]],
            ),
            // (Utf8, TimestampNs)
            (
                DataType::Utf8,
                DataType::TimestampNs { utc: true },
                vec![vec![&u5_null, &ts5_null], vec![&u5_nn, &ts5_nn]],
            ),
            // (I64, FixedBytes16) non-null and nullable
            (
                DataType::I64,
                DataType::FixedBytes16,
                vec![vec![&i5_nn, &f5_nn]],
            ),
            (
                DataType::I64,
                DataType::FixedBytes16,
                vec![vec![&i5_null, &f5_null], vec![&i5_nn, &f5_null]],
            ),
            // (FixedBytes16, TimestampNs)
            (
                DataType::FixedBytes16,
                DataType::TimestampNs { utc: true },
                vec![vec![&f5_nn, &ts5_nn]],
            ),
            (
                DataType::FixedBytes16,
                DataType::TimestampNs { utc: true },
                vec![vec![&f5_null, &ts5_null], vec![&f5_nn, &ts5_null]],
            ),
            // (I64, I64)
            (DataType::I64, DataType::I64, vec![vec![&i5_nn, &i5_nn]]),
            (
                DataType::I64,
                DataType::I64,
                vec![vec![&i5_null, &i5_nn], vec![&i5_nn, &i5_null]],
            ),
            // (Utf8, Utf8)
            (DataType::Utf8, DataType::Utf8, vec![vec![&u5_nn, &u5_nn]]),
            (
                DataType::Utf8,
                DataType::Utf8,
                vec![vec![&u5_null, &u5_dict], vec![&u5_nn, &u5_null]],
            ),
        ];

        for (dt0, dt1, batches) in &two_key_cases {
            let refs = all_refs_for(batches);
            for &(d0, nf0) in &flag_combos {
                for &(d1, nf1) in &flag_combos {
                    let keys = vec![
                        SortKey {
                            expr: col_expr(0, "k0", dt0.clone()),
                            descending: d0,
                            nulls_first: nf0,
                        },
                        SortKey {
                            expr: col_expr(1, "k1", dt1.clone()),
                            descending: d1,
                            nulls_first: nf1,
                        },
                    ];
                    assert_specialized_matches_generic(batches, &keys, &refs);
                }
            }
        }

        // Empty / single-element / empty-keys / 3-key fallback / unsupported type fallback.
        let single_ref = vec![(0u32, 0u32)];
        let one_key = vec![SortKey {
            expr: col_expr(0, "k0", DataType::I64),
            descending: false,
            nulls_first: false,
        }];
        assert_specialized_matches_generic(&[vec![&i5_nn]], &one_key, &single_ref);
        assert_specialized_matches_generic(&[vec![&i5_nn]], &[], &all_refs_for(&[vec![&i5_nn]]));

        let three_keys = vec![
            SortKey {
                expr: col_expr(0, "k0", DataType::Utf8),
                descending: false,
                nulls_first: false,
            },
            SortKey {
                expr: col_expr(1, "k1", DataType::I64),
                descending: true,
                nulls_first: true,
            },
            SortKey {
                expr: col_expr(2, "k2", DataType::FixedBytes16),
                descending: false,
                nulls_first: false,
            },
        ];
        assert_specialized_matches_generic(
            &[vec![&u5_null, &i5_null, &f5_null]],
            &three_keys,
            &all_refs_for(&[vec![&u5_null, &i5_null, &f5_null]]),
        );
    }

    #[test]
    fn specialized_sort_preserves_stability_on_unsorted_selection_indices() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path(), 64 * 1024 * 1024, 64 * 1024 * 1024);
        let schema = Schema::try_new_arc(vec![
            Field::new("k", DataType::I64, true),
            Field::new("row_id", DataType::I64, false),
        ])
        .unwrap();

        // Physical rows 0..10; Selection::Indices visits [5, 2, 8, 1, 4].
        // Rows 5, 2, 8 share k = 10; rows 1, 4 share k = 20.
        // Physical row_id equals physical index * 10 so any physical-order leak is visible.
        let k_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::I64(vec![99, 20, 10, 99, 20, 10, 99, 99, 10, 99]),
                None,
            )
            .unwrap(),
        );
        let id_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::I64(vec![0, 10, 20, 30, 40, 50, 60, 70, 80, 90]),
                None,
            )
            .unwrap(),
        );
        let sel = Selection::Indices(vec![5, 2, 8, 1, 4]);
        let batch = Batch::try_new(Arc::clone(&schema), vec![k_col, id_col], 10, sel).unwrap();

        let keys = vec![SortKey {
            expr: col_expr(0, "k", DataType::I64),
            descending: false,
            nulls_first: false,
        }];

        // 1. Direct sort_row_refs_specialized check with unsorted refs.
        let key_refs = resolve_key_col_refs(&keys, &batch, &[None]).unwrap();
        let mut refs = vec![(0, 5), (0, 2), (0, 8), (0, 1), (0, 4)];
        sort_row_refs_specialized(std::slice::from_ref(&key_refs), &keys, &mut refs);
        assert_eq!(refs, vec![(0, 5), (0, 2), (0, 8), (0, 1), (0, 4)]);

        // 2. spill_single_batch end-to-end check on unsorted Selection::Indices.
        let mut sort = ExternalSort::new(Arc::clone(&schema), keys.clone(), None, ctx.clone());
        let mut dummy_res = ctx.pool.register("spill_sel_in", false);
        sort.spill_single_batch(&batch, &mut dummy_res).unwrap();
        assert_eq!(sort.spilled_run_count(), 1);
        let mut stream = sort.into_stream().unwrap();
        let out = drain_stream(stream.as_mut(), &ctx).unwrap();
        drop(stream);
        assert_eq!(ctx.pool.reserved(), 0);

        let mut got_ids = Vec::new();
        for b in &out {
            let col = b.column(1).unwrap();
            for r in 0..b.num_rows() {
                got_ids.push(eval::read_i64(col, r).unwrap());
            }
        }
        // Must preserve Selection::Indices order [5, 2, 8] for k=10, then [1, 4] for k=20.
        assert_eq!(got_ids, vec![50, 20, 80, 10, 40]);

        // 3. Also verify nullable DESC NULLS FIRST on spill_single_batch with unsorted Selection::Indices.
        let k_null_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::I64(vec![0, 20, 0, 0, 20, 0, 0, 0, 10, 0]),
                Some(Bitmap::from_bools(&[
                    true, true, false, true, true, false, true, true, true, true,
                ])),
            )
            .unwrap(),
        );
        let id_col2 = Column::Flat(
            FlatColumn::try_new(
                FlatData::I64(vec![0, 10, 20, 30, 40, 50, 60, 70, 80, 90]),
                None,
            )
            .unwrap(),
        );
        let batch2 = Batch::try_new(
            Arc::clone(&schema),
            vec![k_null_col, id_col2],
            10,
            Selection::Indices(vec![5, 2, 8, 1, 4]),
        )
        .unwrap();
        let keys_desc_nf = vec![SortKey {
            expr: col_expr(0, "k", DataType::I64),
            descending: true,
            nulls_first: true,
        }];
        let mut sort2 = ExternalSort::new(Arc::clone(&schema), keys_desc_nf, None, ctx.clone());
        sort2.spill_single_batch(&batch2, &mut dummy_res).unwrap();
        let mut stream2 = sort2.into_stream().unwrap();
        let out2 = drain_stream(stream2.as_mut(), &ctx).unwrap();
        drop(stream2);
        assert_eq!(ctx.pool.reserved(), 0);

        let mut got_ids2 = Vec::new();
        for b in &out2 {
            let col = b.column(1).unwrap();
            for r in 0..b.num_rows() {
                got_ids2.push(eval::read_i64(col, r).unwrap());
            }
        }
        // NULLs first in original Selection order [50, 20], then k=20 [10, 40], then k=10 [80].
        assert_eq!(got_ids2, vec![50, 20, 10, 40, 80]);
    }
}
