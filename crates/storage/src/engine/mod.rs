//! Native query execution engine storage adapters (`AsyncAdmissionPool`,
//! `MetadataCache`, zero-Arrow `ParquetSource`, `MemtableSource`, `TracesSource`,
//! and `ScoresSource`).

pub(crate) mod admission;
#[cfg(test)]
mod diff_tests;
pub(crate) mod mem_source;
pub(crate) mod metadata_cache;
pub(crate) mod parquet_export;
pub(crate) mod parquet_source;
pub(crate) mod scores_source;
pub(crate) mod traces_source;

use std::sync::Arc;

use cetra_column::{Batch, CancelToken, Reservation};
use cetra_query::exec::PartitionStream;
use cetra_query::{
    EvalContext, Expr, QueryError, TombstoneSpec, TombstoneTable, eval_filter, filter_uncovered,
};
use chrono::{Datelike, NaiveDate};

use crate::catalog::FileTable;
use crate::deletion::PendingDeletion;

/// Nanoseconds in a UTC day.
pub(super) const DAY_NS: i64 = 86_400_000_000_000;

/// `NaiveDate::num_days_from_ce` of 1970-01-01.
const UNIX_EPOCH_DAYS_FROM_CE: i64 = 719_163;

pub(super) fn day_start_ns(day: NaiveDate) -> Option<i64> {
    let days = i64::from(day.num_days_from_ce()) - UNIX_EPOCH_DAYS_FROM_CE;
    days.checked_mul(DAY_NS)
}

pub(super) fn day_of_ns(ns: i64) -> Option<NaiveDate> {
    let days = ns.div_euclid(DAY_NS);
    let ce = days.checked_add(UNIX_EPOCH_DAYS_FROM_CE)?;
    NaiveDate::from_num_days_from_ce_opt(i32::try_from(ce).ok()?)
}

pub(super) fn tombstone_table(table: FileTable) -> TombstoneTable {
    match table {
        FileTable::Spans => TombstoneTable::Spans,
        FileTable::Scores => TombstoneTable::Scores,
        FileTable::Traces => TombstoneTable::Traces,
    }
}

pub(super) fn to_tombstone_specs(deletions: &[Arc<PendingDeletion>]) -> Vec<TombstoneSpec> {
    deletions
        .iter()
        .map(|d| TombstoneSpec {
            cutoff: d.cutoff,
            trace_ids: d.trace_ids.iter().copied().collect(),
            users: d.users.iter().cloned().collect(),
            sessions: d.sessions.iter().cloned().collect(),
        })
        .collect()
}

pub(super) fn rollback_res(res: Option<&mut Reservation>, before: usize) {
    if let Some(r) = res {
        let cur = r.size();
        if cur > before {
            r.shrink(cur - before);
        }
    }
}

pub(super) fn reconcile_res(
    res: Option<&mut Reservation>,
    before: usize,
    target_bytes: usize,
) -> Result<(), QueryError> {
    let Some(r) = res else {
        return Ok(());
    };
    let Some(target) = before.checked_add(target_bytes) else {
        rollback_res(Some(r), before);
        return Err(QueryError::TooLarge);
    };
    let cur = r.size();
    if target > cur {
        if let Err(err) = r.try_grow(target - cur) {
            rollback_res(Some(r), before);
            return Err(err.into());
        }
    } else if target < cur {
        r.shrink(cur - target);
    }
    Ok(())
}

/// Evaluates an optional tombstone predicate and conjunctive filters on `batch`,
/// updating `batch`'s `Selection` without copying column buffers.
///
/// On error, rolls back `res` to `before`.
pub(super) fn apply_tombstones_and_filters(
    mut batch: Batch,
    tombstone_pred: Option<&Expr>,
    filters: &[Expr],
    eval_ctx: &EvalContext,
    mut res: Option<&mut Reservation>,
    before: usize,
) -> Result<Batch, QueryError> {
    if let Some(pred) = tombstone_pred {
        batch = match filter_uncovered(&batch, pred, eval_ctx, res.as_deref_mut()) {
            Ok(b) => b,
            Err(err) => {
                rollback_res(res, before);
                return Err(err);
            }
        };
        if batch.is_empty() {
            return Ok(batch);
        }
    }

    for filter in filters {
        let mask = match eval_filter(filter, &batch, eval_ctx, res.as_deref_mut()) {
            Ok(m) => m,
            Err(err) => {
                rollback_res(res, before);
                return Err(err);
            }
        };
        batch = match batch.into_filter_mask(&mask) {
            Ok(b) => b,
            Err(err) => {
                rollback_res(res, before);
                return Err(err.into());
            }
        };
        drop(mask);
        reconcile_res(res.as_deref_mut(), before, batch.memory_bytes())?;
        if batch.is_empty() {
            break;
        }
    }

    Ok(batch)
}

pub(super) struct ChainedPartitionStream {
    streams: Vec<Box<dyn PartitionStream>>,
    cursor: usize,
}

impl PartitionStream for ChainedPartitionStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_ref().map_or(0, |r| r.size());
        while self.cursor < self.streams.len() {
            if cancel.is_cancelled() {
                rollback_res(res, before);
                return Err(QueryError::Cancelled);
            }
            if let Some(batch) = self.streams[self.cursor].next_batch(cancel, res.as_deref_mut())? {
                return Ok(Some(batch));
            }
            self.cursor += 1;
        }
        rollback_res(res, before);
        Ok(None)
    }
}

pub(super) fn cap_partitions(
    streams: Vec<Box<dyn PartitionStream>>,
    max_parts: usize,
) -> Vec<Box<dyn PartitionStream>> {
    let cap = max_parts.max(1);
    if streams.len() <= cap {
        return streams;
    }
    let mut grouped: Vec<Vec<Box<dyn PartitionStream>>> = (0..cap).map(|_| Vec::new()).collect();
    for (idx, stream) in streams.into_iter().enumerate() {
        grouped[idx % cap].push(stream);
    }
    grouped
        .into_iter()
        .filter(|g| !g.is_empty())
        .map(|group| {
            Box::new(ChainedPartitionStream {
                streams: group,
                cursor: 0,
            }) as Box<dyn PartitionStream>
        })
        .collect()
}

/// Inclusive nanosecond bounds extracted from filters (`query/mod.rs` parity).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct TimeBounds {
    pub min: Option<i64>,
    pub max: Option<i64>,
}

impl TimeBounds {
    pub(super) fn open() -> Self {
        Self {
            min: None,
            max: None,
        }
    }

    pub(super) fn contradicts(&self) -> bool {
        matches!((self.min, self.max), (Some(lo), Some(hi)) if lo > hi)
    }

    fn raise_min(&mut self, value: i64) {
        self.min = Some(self.min.map_or(value, |cur| cur.max(value)));
    }

    fn lower_max(&mut self, value: i64) {
        self.max = Some(self.max.map_or(value, |cur| cur.min(value)));
    }

    fn force_empty(&mut self) {
        self.min = Some(0);
        self.max = Some(-1);
    }

    pub(super) fn union_with(&self, other: &Self) -> Self {
        Self {
            min: self.min.zip(other.min).map(|(a, b)| a.min(b)),
            max: self.max.zip(other.max).map(|(a, b)| a.max(b)),
        }
    }

    pub(super) fn intersect_with(&self, other: &Self) -> Self {
        Self {
            min: match (self.min, other.min) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            },
            max: match (self.max, other.max) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            },
        }
    }
}

pub(super) fn extract_time_bounds(filters: &[Expr], column: &str) -> TimeBounds {
    let mut bounds = TimeBounds::open();
    for filter in filters {
        walk_time_bounds(&mut bounds, filter, column);
    }
    bounds
}

fn walk_time_bounds(bounds: &mut TimeBounds, expr: &Expr, column: &str) {
    use cetra_query::{BinaryOp, UnaryOp};

    match expr {
        Expr::Binary {
            left,
            op: BinaryOp::And,
            right,
            ..
        } => {
            walk_time_bounds(bounds, left, column);
            walk_time_bounds(bounds, right, column);
        }
        Expr::Binary {
            left,
            op: BinaryOp::Or,
            ..
        } => {
            let _ = left;
            let mut disjuncts = Vec::new();
            collect_or_disjuncts(expr, &mut disjuncts);
            let mut union: Option<TimeBounds> = None;
            for disjunct in disjuncts {
                if matches!(
                    disjunct,
                    Expr::Unary {
                        op: UnaryOp::IsNull,
                        expr: inner,
                        ..
                    } if matches!(inner.as_ref(), Expr::Column { name, .. } if name == column)
                ) {
                    continue;
                }
                let mut separate = TimeBounds::open();
                walk_time_bounds(&mut separate, disjunct, column);
                union = Some(match union {
                    None => separate,
                    Some(acc) => acc.union_with(&separate),
                });
            }
            if let Some(u) = union {
                *bounds = bounds.intersect_with(&u);
            }
        }
        Expr::Binary {
            left, op, right, ..
        } => match (left.as_ref(), right.as_ref()) {
            (Expr::Column { name, .. }, other) if name == column => {
                if let Some(ns) = expr_timestamp_ns(other) {
                    apply_time_cmp(bounds, *op, ns, true);
                }
            }
            (other, Expr::Column { name, .. }) if name == column => {
                if let Some(ns) = expr_timestamp_ns(other) {
                    apply_time_cmp(bounds, *op, ns, false);
                }
            }
            _ => {}
        },
        Expr::Between {
            expr,
            low,
            high,
            negated: false,
        } => {
            if matches!(expr.as_ref(), Expr::Column { name, .. } if name == column) {
                if let Some(lo) = expr_timestamp_ns(low) {
                    bounds.raise_min(lo);
                }
                if let Some(hi) = expr_timestamp_ns(high) {
                    bounds.lower_max(hi);
                }
            }
        }
        _ => {}
    }
}

fn collect_or_disjuncts<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    use cetra_query::BinaryOp;
    match expr {
        Expr::Binary {
            left,
            op: BinaryOp::Or,
            right,
            ..
        } => {
            collect_or_disjuncts(left, out);
            collect_or_disjuncts(right, out);
        }
        other => out.push(other),
    }
}

fn apply_time_cmp(bounds: &mut TimeBounds, op: cetra_query::BinaryOp, lit: i64, col_left: bool) {
    use cetra_query::BinaryOp;
    let op = if col_left {
        op
    } else {
        match op {
            BinaryOp::Eq => BinaryOp::Eq,
            BinaryOp::Gt => BinaryOp::Lt,
            BinaryOp::GtEq => BinaryOp::LtEq,
            BinaryOp::Lt => BinaryOp::Gt,
            BinaryOp::LtEq => BinaryOp::GtEq,
            _ => return,
        }
    };
    match op {
        BinaryOp::Eq => {
            bounds.raise_min(lit);
            bounds.lower_max(lit);
        }
        BinaryOp::Gt => match lit.checked_add(1) {
            Some(next) => bounds.raise_min(next),
            None => bounds.force_empty(),
        },
        BinaryOp::GtEq => bounds.raise_min(lit),
        BinaryOp::Lt => match lit.checked_sub(1) {
            Some(prev) => bounds.lower_max(prev),
            None => bounds.force_empty(),
        },
        BinaryOp::LtEq => bounds.lower_max(lit),
        _ => {}
    }
}

fn expr_timestamp_ns(expr: &Expr) -> Option<i64> {
    use cetra_column::ScalarValue;
    match expr {
        Expr::Literal(ScalarValue::TimestampNs { value, .. }) => Some(*value),
        Expr::Literal(ScalarValue::Utf8(s)) => cetra_core::time_serde::parse_rfc3339(s).ok(),
        _ => None,
    }
}

pub(super) fn read_str_at(col: &cetra_column::Column, row: usize) -> Option<&str> {
    use cetra_column::{Column, FlatData, ScalarValue};
    match col {
        Column::Flat(f) => {
            if f.is_null(row) {
                return None;
            }
            match f.data() {
                FlatData::Utf8(u) => u.get(row),
                _ => None,
            }
        }
        Column::Const { value, len } => {
            if row >= *len {
                return None;
            }
            match value {
                ScalarValue::Utf8(s) => Some(s.as_str()),
                _ => None,
            }
        }
        Column::Dict(d) => {
            if d.is_null(row) {
                return None;
            }
            let code = *d.codes().get(row)? as usize;
            if d.values().is_null(code) {
                return None;
            }
            match d.values().data() {
                FlatData::Utf8(u) => u.get(code),
                _ => None,
            }
        }
        Column::Rle(r) => {
            if row >= r.len() {
                return None;
            }
            let phys = r.run_ends().partition_point(|&end| (end as usize) <= row);
            if r.values().is_null(phys) {
                return None;
            }
            match r.values().data() {
                FlatData::Utf8(u) => u.get(phys),
                _ => None,
            }
        }
    }
}

pub(super) fn read_fixed16_at(col: &cetra_column::Column, row: usize) -> Option<[u8; 16]> {
    use cetra_column::{Column, FlatData, ScalarValue};
    match col {
        Column::Flat(f) => {
            if f.is_null(row) {
                return None;
            }
            match f.data() {
                FlatData::FixedBytes16(v) => v.get(row).copied(),
                _ => None,
            }
        }
        Column::Const { value, len } => {
            if row >= *len {
                return None;
            }
            match value {
                ScalarValue::FixedBytes16(b) => Some(*b),
                _ => None,
            }
        }
        Column::Dict(d) => {
            if d.is_null(row) {
                return None;
            }
            let code = *d.codes().get(row)? as usize;
            if d.values().is_null(code) {
                return None;
            }
            match d.values().data() {
                FlatData::FixedBytes16(v) => v.get(code).copied(),
                _ => None,
            }
        }
        Column::Rle(r) => {
            if row >= r.len() {
                return None;
            }
            let phys = r.run_ends().partition_point(|&end| (end as usize) <= row);
            if r.values().is_null(phys) {
                return None;
            }
            match r.values().data() {
                FlatData::FixedBytes16(v) => v.get(phys).copied(),
                _ => None,
            }
        }
    }
}

pub(super) struct BufferedBatchStream {
    batches: std::collections::VecDeque<Batch>,
    res: Reservation,
}

impl BufferedBatchStream {
    pub(super) fn new(batches: Vec<Batch>, res: Reservation) -> Self {
        Self {
            batches: batches.into(),
            res,
        }
    }
}

impl PartitionStream for BufferedBatchStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_ref().map_or(0, |r| r.size());
        if cancel.is_cancelled() {
            rollback_res(res, before);
            return Err(QueryError::Cancelled);
        }
        let Some(batch) = self.batches.pop_front() else {
            self.res.free();
            rollback_res(res, before);
            return Ok(None);
        };
        let bytes = batch.memory_bytes();
        self.res.shrink(self.res.size().min(bytes));
        reconcile_res(res, before, bytes)?;
        Ok(Some(batch))
    }
}

#[cfg(test)]
mod tests {
    use cetra_column::{
        Column, DataType, Field, FlatColumn, FlatData, GlobalPool, ScalarValue, Schema, Selection,
    };
    use cetra_query::BinaryOp;

    use super::*;

    #[test]
    fn multiple_filters_do_not_inflate_reservation() {
        let schema = Schema::try_new_arc(vec![Field::new("x", DataType::I64, false)]).unwrap();
        let col = Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1, 2, 3, 4]), None).unwrap());
        let batch = Batch::try_new(Arc::clone(&schema), vec![col], 4, Selection::All).unwrap();
        let col_expr = Expr::Column {
            index: 0,
            name: "x".to_owned(),
            data_type: DataType::I64,
        };
        let filters: Vec<Expr> = (0..5)
            .map(|_| Expr::Binary {
                left: Box::new(col_expr.clone()),
                op: BinaryOp::Gt,
                right: Box::new(Expr::Literal(ScalarValue::I64(1))),
                data_type: DataType::Bool,
            })
            .collect();

        let pool = GlobalPool::standalone(1024 * 1024);
        let mut res = pool.register("test_filters", false);
        res.try_grow(64).unwrap();
        let before = res.size();
        reconcile_res(Some(&mut res), before, batch.memory_bytes()).unwrap();

        let ctx = EvalContext {
            now_ns: 0,
            cancel: CancelToken::new(),
        };
        let out = apply_tombstones_and_filters(batch, None, &filters, &ctx, Some(&mut res), before)
            .unwrap();
        assert_eq!(res.size(), before + out.memory_bytes());
    }
}
