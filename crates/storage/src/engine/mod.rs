//! Native query execution engine storage adapters (`AsyncAdmissionPool`,
//! `MetadataCache`, zero-Arrow `ParquetSource`, and `MemtableSource`).

pub(crate) mod admission;
pub(crate) mod mem_source;
pub(crate) mod metadata_cache;
pub(crate) mod parquet_source;

use std::sync::Arc;

use cetra_column::{Batch, Reservation};
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
            cancel: cetra_column::CancelToken::new(),
        };
        let out = apply_tombstones_and_filters(batch, None, &filters, &ctx, Some(&mut res), before)
            .unwrap();
        assert_eq!(res.size(), before + out.memory_bytes());
    }
}
