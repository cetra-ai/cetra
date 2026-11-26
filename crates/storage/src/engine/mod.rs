//! Native query execution engine storage adapters (`AsyncAdmissionPool`,
//! `MetadataCache`, zero-Arrow `ParquetSource`, `MemtableSource`, `TracesSource`,
//! and `ScoresSource`).

pub(crate) mod admission;
pub(crate) mod mem_source;
pub(crate) mod metadata_cache;
pub(crate) mod parquet_export;
pub(crate) mod parquet_source;
pub(crate) mod scores_source;
pub(crate) mod traces_source;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use cetra_column::{Batch, CancelToken, Column, ColumnError, DataType, Reservation, Schema};
use cetra_query::exec::sort_window::ExternalSort;
use cetra_query::exec::{
    ControlFlow, ExecContext, MAX_MERGE_WAY, PartitionStream, PushSink, SortKey, SpillAppender,
    SpillFile,
};
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
    streams: std::collections::VecDeque<Box<dyn PartitionStream>>,
}

impl PartitionStream for ChainedPartitionStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_ref().map_or(0, |r| r.size());
        while let Some(front) = self.streams.front_mut() {
            if cancel.is_cancelled() {
                rollback_res(res, before);
                return Err(QueryError::Cancelled);
            }
            match front.next_batch(cancel, res.as_deref_mut()) {
                Ok(Some(batch)) => return Ok(Some(batch)),
                Ok(None) => {
                    self.streams.pop_front();
                }
                Err(err) => {
                    rollback_res(res, before);
                    return Err(err);
                }
            }
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
    let mut grouped: Vec<std::collections::VecDeque<Box<dyn PartitionStream>>> = (0..cap)
        .map(|_| std::collections::VecDeque::new())
        .collect();
    for (idx, stream) in streams.into_iter().enumerate() {
        grouped[idx % cap].push_back(stream);
    }
    grouped
        .into_iter()
        .filter(|g| !g.is_empty())
        .map(|group| {
            Box::new(ChainedPartitionStream { streams: group }) as Box<dyn PartitionStream>
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

pub(super) fn read_str_at(col: &Column, row: usize) -> Option<&str> {
    use cetra_column::{FlatData, ScalarValue};
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

pub(super) fn read_fixed16_at(col: &Column, row: usize) -> Option<[u8; 16]> {
    use cetra_column::{FlatData, ScalarValue};
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

pub(super) fn read_bool_at(col: &Column, row: usize) -> Option<bool> {
    use cetra_column::{FlatData, ScalarValue};
    match col {
        Column::Flat(f) => {
            if f.is_null(row) {
                return None;
            }
            match f.data() {
                FlatData::Bool(bm) => Some(bm.get(row)),
                _ => None,
            }
        }
        Column::Const { value, len } => {
            if row >= *len {
                return None;
            }
            match value {
                ScalarValue::Bool(b) => Some(*b),
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
                FlatData::Bool(bm) => Some(bm.get(code)),
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
                FlatData::Bool(bm) => Some(bm.get(phys)),
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

/// Moves `bytes` from `src` into `dst` without double-counting; if `src.size() < bytes`,
/// transfers `src.size()` first and grows `dst` by the remainder (rolling back on error).
pub(super) fn transfer_or_grow(
    src: &mut Reservation,
    dst: &mut Reservation,
    bytes: usize,
) -> Result<(), ColumnError> {
    if bytes == 0 {
        return Ok(());
    }
    let avail = src.size();
    if avail >= bytes {
        src.transfer_to(dst, bytes)
    } else {
        if avail > 0 {
            src.transfer_to(dst, avail)?;
        }
        if let Err(err) = dst.try_grow(bytes - avail) {
            if avail > 0 {
                let _ = dst.transfer_to(src, avail);
            }
            return Err(err);
        }
        Ok(())
    }
}

/// Atomically claims up to `want` rows from a shared query `LIMIT` counter across parallel streams.
pub(super) fn claim_shared_limit(remaining: Option<&Arc<AtomicUsize>>, want: usize) -> usize {
    let Some(counter) = remaining else {
        return want;
    };
    if want == 0 {
        return 0;
    }
    let mut cur = counter.load(Ordering::Acquire);
    loop {
        if cur == 0 {
            return 0;
        }
        let take = cur.min(want);
        match counter.compare_exchange_weak(cur, cur - take, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return take,
            Err(actual) => cur = actual,
        }
    }
}

fn hash_bytes_fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    // Finalize to avoid FNV-1a's weak low-bit avalanche on periodic patterns mod 2^k.
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

fn partition_for_row(col: &Column, phys_row: usize, num_partitions: usize) -> usize {
    match col.data_type() {
        DataType::FixedBytes16 => read_fixed16_at(col, phys_row)
            .map_or(0, |id| (hash_bytes_fnv1a(&id) as usize) % num_partitions),
        DataType::Utf8 => read_str_at(col, phys_row).map_or(0, |s| {
            (hash_bytes_fnv1a(s.as_bytes()) as usize) % num_partitions
        }),
        _ => 0,
    }
}

struct PartitionBucket {
    batches: Vec<Batch>,
    mem_bytes: usize,
    appender: SpillAppender,
}

/// Worker-local hash-partitioning sink holding **one** spillable [`Reservation`] shared across
/// `P` partition buckets and `P` lazy [`SpillAppender`]s (`0` idle FDs, `W` spillable consumers
/// total rather than `W * P`, `R1-M3`).
pub(super) struct HashPartitionedSortSink {
    schema: Arc<Schema>,
    key_col_idx: usize,
    buckets: Vec<PartitionBucket>,
    res: Reservation,
    max_batch_bytes: usize,
    ctx: ExecContext,
}

impl HashPartitionedSortSink {
    pub(super) fn new(
        schema: Arc<Schema>,
        key_col_idx: usize,
        num_partitions: usize,
        ctx: ExecContext,
    ) -> Self {
        let p = num_partitions.max(1);
        let buckets = (0..p)
            .map(|_| PartitionBucket {
                batches: Vec::new(),
                mem_bytes: 0,
                appender: ctx.spill.create_appender(),
            })
            .collect();
        let res = ctx.pool.register("hash_part_sink", true);
        Self {
            schema,
            key_col_idx,
            buckets,
            res,
            max_batch_bytes: 0,
            ctx,
        }
    }

    fn total_mem_bytes(&self) -> usize {
        self.buckets.iter().map(|b| b.mem_bytes).sum()
    }

    fn spill_largest_bucket(&mut self) -> Result<bool, QueryError> {
        let Some((idx, _)) = self
            .buckets
            .iter()
            .enumerate()
            .filter(|(_, b)| !b.batches.is_empty())
            .max_by_key(|(_, b)| b.mem_bytes)
        else {
            return Ok(false);
        };
        let bucket = &mut self.buckets[idx];
        let drained: Vec<Batch> = std::mem::take(&mut bucket.batches);
        let freed_bytes = bucket.mem_bytes;
        bucket.mem_bytes = 0;
        for batch in drained {
            if self.ctx.eval.cancel.is_cancelled() {
                self.res.shrink(self.res.size().min(freed_bytes));
                return Err(QueryError::Cancelled);
            }
            bucket.appender.append_batch(&batch, None)?;
        }
        self.res.shrink(self.res.size().min(freed_bytes));
        Ok(true)
    }

    fn push_bucket_batch(
        &mut self,
        bucket_idx: usize,
        batch: Batch,
        batch_res: &mut Reservation,
    ) -> Result<(), QueryError> {
        let bytes = batch.memory_bytes();
        loop {
            match transfer_or_grow(batch_res, &mut self.res, bytes) {
                Ok(()) => {
                    self.buckets[bucket_idx].batches.push(batch);
                    self.buckets[bucket_idx].mem_bytes =
                        self.buckets[bucket_idx].mem_bytes.saturating_add(bytes);
                    break;
                }
                Err(ColumnError::MemoryLimit { .. }) if self.total_mem_bytes() > 0 => {
                    self.spill_largest_bucket()?;
                }
                Err(ColumnError::MemoryLimit { .. }) => {
                    // Single sub-batch exceeds SpillShare with 0 buffered bytes: spill directly.
                    self.buckets[bucket_idx]
                        .appender
                        .append_batch(&batch, None)?;
                    return Ok(());
                }
                Err(err) => return Err(err.into()),
            }
        }

        let cap = self.ctx.pool.cap();
        while self.total_mem_bytes() > 0
            && (self
                .res
                .size()
                .saturating_add(self.max_batch_bytes.saturating_mul(2))
                > cap
                || self
                    .ctx
                    .pool
                    .reserved()
                    .saturating_add(self.max_batch_bytes.saturating_mul(2))
                    > cap)
        {
            if !self.spill_largest_bucket()? {
                break;
            }
        }
        Ok(())
    }

    /// Combines all worker sinks into up to `P` independent sorted [`PartitionStream`]s,
    /// capping per-partition `max_merge_way = (MAX_MERGE_WAY / P).max(2)` so total open FDs
    /// across `P` concurrent merge streams stay `<= 33`.
    pub(super) fn into_partition_streams(
        mut sinks: Vec<Self>,
        schema: &Arc<Schema>,
        sort_keys: &[SortKey],
        ctx: &ExecContext,
    ) -> Result<Vec<Box<dyn PartitionStream>>, QueryError> {
        if sinks.is_empty() {
            return Ok(Vec::new());
        }
        let num_partitions = sinks[0].buckets.len().max(1);
        let mut combined: Vec<(Vec<Batch>, usize, Vec<SpillFile>)> = (0..num_partitions)
            .map(|_| (Vec::new(), 0usize, Vec::new()))
            .collect();

        let per_part_in_mem_cap = (ctx.pool.cap() / (2 * num_partitions)).max(1);
        let mut part_res: Vec<Reservation> = (0..num_partitions)
            .map(|_| ctx.pool.register("hash_part_buf", false))
            .collect();

        for sink in &mut sinks {
            for (p_idx, bucket) in sink.buckets.iter_mut().enumerate() {
                if let Some(sf) =
                    std::mem::replace(&mut bucket.appender, ctx.spill.create_appender()).finish()
                {
                    combined[p_idx].2.push(sf);
                }
                if bucket.batches.is_empty() {
                    continue;
                }
                let b_bytes = bucket.mem_bytes;
                let mut drained = std::mem::take(&mut bucket.batches);
                bucket.mem_bytes = 0;

                let must_spill = !combined[p_idx].2.is_empty()
                    || combined[p_idx].1.saturating_add(b_bytes) > per_part_in_mem_cap;
                if must_spill {
                    // Spill already-accumulated in-memory batches for this partition too.
                    let mut app = ctx.spill.create_appender();
                    for prev in combined[p_idx].0.drain(..) {
                        app.append_batch(&prev, None)?;
                    }
                    combined[p_idx].1 = 0;
                    part_res[p_idx].free();
                    for b in drained.drain(..) {
                        app.append_batch(&b, None)?;
                    }
                    sink.res.shrink(sink.res.size().min(b_bytes));
                    if let Some(sf) = app.finish() {
                        combined[p_idx].2.push(sf);
                    }
                } else {
                    let move_bytes = sink.res.size().min(b_bytes);
                    if move_bytes > 0 {
                        sink.res.transfer_to(&mut part_res[p_idx], move_bytes)?;
                    }
                    combined[p_idx].0.append(&mut drained);
                    combined[p_idx].1 = combined[p_idx].1.saturating_add(b_bytes);
                }
            }
            sink.res.free();
        }
        drop(sinks);

        let max_merge_way = (MAX_MERGE_WAY / num_partitions).max(2);
        let mut streams: Vec<Box<dyn PartitionStream>> = Vec::with_capacity(num_partitions);
        for ((in_mem, _, spills), mut res) in combined.into_iter().zip(part_res) {
            if in_mem.is_empty() && spills.is_empty() {
                res.free();
                continue;
            }
            streams.push(Box::new(PartitionSortStream {
                schema: Arc::clone(schema),
                keys: sort_keys.to_owned(),
                ctx: ctx.clone(),
                max_merge_way,
                in_mem,
                in_mem_res: res,
                spills,
                sorted_stream: None,
            }));
        }
        Ok(streams)
    }
}

impl PushSink for HashPartitionedSortSink {
    fn consume(
        &mut self,
        batch: Batch,
        batch_res: &mut Reservation,
    ) -> Result<ControlFlow, QueryError> {
        if self.ctx.eval.cancel.is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        if batch.is_empty() {
            return Ok(ControlFlow::Continue);
        }
        if batch.schema().as_ref() != self.schema.as_ref() {
            return Err(ColumnError::InvalidBatch {
                reason: "batch schema does not match HashPartitionedSortSink schema",
            }
            .into());
        }

        self.max_batch_bytes = self.max_batch_bytes.max(batch.memory_bytes());
        let p = self.buckets.len().max(1);
        let key_col = batch
            .column(self.key_col_idx)
            .ok_or(ColumnError::InvalidBatch {
                reason: "partition key column index out of bounds",
            })?;

        let mut per_bucket_rows: Vec<Vec<u32>> = vec![Vec::new(); p];
        let indices = batch.selection().to_indices(batch.raw_len())?;
        for phys_row in indices {
            let b = partition_for_row(key_col, phys_row as usize, p);
            per_bucket_rows[b].push(phys_row);
        }

        let non_empty_buckets = per_bucket_rows.iter().filter(|r| !r.is_empty()).count();
        if non_empty_buckets == 0 {
            return Ok(ControlFlow::Continue);
        }

        if non_empty_buckets == 1
            && let Some(bucket_idx) = per_bucket_rows.iter().position(|r| !r.is_empty())
        {
            let compact = if batch.selection().is_all(batch.raw_len()) {
                batch
            } else {
                batch.compact(None)?
            };
            return self
                .push_bucket_batch(bucket_idx, compact, batch_res)
                .map(|()| ControlFlow::Continue);
        }

        for (bucket_idx, rows) in per_bucket_rows.into_iter().enumerate() {
            if rows.is_empty() {
                continue;
            }
            let mut cols = Vec::with_capacity(batch.columns().len());
            for col in batch.columns() {
                cols.push(Column::Flat(col.take(&rows, None)?));
            }
            let sub = Batch::try_new_all(Arc::clone(&self.schema), cols, rows.len())?;
            self.push_bucket_batch(bucket_idx, sub, batch_res)?;
        }
        Ok(ControlFlow::Continue)
    }
}

struct PartitionSortStream {
    schema: Arc<Schema>,
    keys: Vec<SortKey>,
    ctx: ExecContext,
    max_merge_way: usize,
    in_mem: Vec<Batch>,
    in_mem_res: Reservation,
    spills: Vec<SpillFile>,
    sorted_stream: Option<Box<dyn PartitionStream>>,
}

impl PartitionSortStream {
    fn init_stream(&mut self, cancel: &CancelToken) -> Result<(), QueryError> {
        let mut sort = ExternalSort::new(
            Arc::clone(&self.schema),
            self.keys.clone(),
            None,
            self.ctx.clone(),
        )
        .with_max_merge_way(self.max_merge_way);

        let mut remaining_bytes: usize = self.in_mem.iter().map(Batch::memory_bytes).sum();
        for batch in self.in_mem.drain(..) {
            if cancel.is_cancelled() {
                self.in_mem_res.free();
                return Err(QueryError::Cancelled);
            }
            remaining_bytes = remaining_bytes.saturating_sub(batch.memory_bytes());
            sort.consume(batch, &mut self.in_mem_res)?;
            let excess = self.in_mem_res.size().saturating_sub(remaining_bytes);
            if excess > 0 {
                self.in_mem_res.shrink(excess);
            }
        }
        self.in_mem_res.free();

        let mut read_res = self.ctx.pool.register("hash_part_spill_read", false);
        for spill_file in self.spills.drain(..) {
            let mut reader = spill_file.open_reader(Arc::clone(&self.schema))?;
            loop {
                read_res.free();
                if cancel.is_cancelled() {
                    read_res.free();
                    return Err(QueryError::Cancelled);
                }
                let Some(batch) = reader.next_batch(Some(&mut read_res))? else {
                    break;
                };
                if !batch.is_empty() {
                    sort.consume(batch, &mut read_res)?;
                }
            }
            read_res.free();
        }

        let stream = sort.into_stream()?;
        self.sorted_stream = Some(stream);
        Ok(())
    }
}

impl PartitionStream for PartitionSortStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_ref().map_or(0, |r| r.size());
        if cancel.is_cancelled() {
            self.in_mem_res.free();
            rollback_res(res, before);
            return Err(QueryError::Cancelled);
        }
        if self.sorted_stream.is_none()
            && let Err(err) = self.init_stream(cancel)
        {
            self.in_mem_res.free();
            rollback_res(res, before);
            return Err(err);
        }
        let Some(stream) = self.sorted_stream.as_mut() else {
            rollback_res(res, before);
            return Ok(None);
        };
        stream.next_batch(cancel, res)
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

        let ctx = EvalContext::new(0, CancelToken::new());
        let out = apply_tombstones_and_filters(batch, None, &filters, &ctx, Some(&mut res), before)
            .unwrap();
        assert_eq!(res.size(), before + out.memory_bytes());
    }

    mod integration_tests {
        use std::collections::HashSet;
        use std::ops::Range;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        use bytes::Bytes;
        use cetra_column::{
            Batch, CancelToken, DataType, GlobalPool, Reservation, ScalarValue, Schema,
        };
        use cetra_core::ids::ProjectId;
        use cetra_core::schema::{scores, traces};
        use cetra_core::score::{Score, ScoreTarget, ScoreValue};
        use cetra_core::span::Span;
        use cetra_query::exec::{
            CollectSink, ControlFlow, ExecContext, ExecMetrics, PushSink, ScanSource, ScanSpec,
            SortKey, SpillManager, WorkerPool, run_pipeline,
        };
        use cetra_query::{EvalContext, Expr, QueryError};
        use chrono::NaiveDate;
        use futures::stream::BoxStream;
        use object_store::memory::InMemory;
        use object_store::path::Path as ObjectPath;
        use object_store::{
            CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta,
            ObjectStore, PutMultipartOptions, PutOptions, PutPayload, PutResult,
        };
        use uuid::Uuid;

        use crate::catalog::{FileId, FileMeta, FileTable};
        use crate::deletion::{PendingDeletion, Tombstones};
        use crate::engine::DAY_NS;
        use crate::engine::admission::AsyncAdmissionPool;
        use crate::engine::day_of_ns;
        use crate::engine::metadata_cache::MetadataCache;
        use crate::engine::scores_source::ScoresSource;
        use crate::engine::traces_source::{TraceListMode, TracesSource};
        use crate::file_format::write_file_batch;
        use crate::memtable::Memtable;
        use crate::partials::partials_from_spans_batch;
        use crate::test_support::{score_row, score_rows, span_row, span_rows};
        use crate::verify::{Suspects, VerifyQueue};
        use crate::wal::Table;

        fn project() -> ProjectId {
            "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
        }

        fn day_of(ns: i64) -> NaiveDate {
            day_of_ns(ns).unwrap()
        }

        fn make_ctx(
            cap: usize,
            spill_cap: u64,
            partitions: usize,
        ) -> (ExecContext, tempfile::TempDir) {
            let dir = tempfile::tempdir().unwrap();
            let ctx = ExecContext {
                pool: GlobalPool::standalone(cap),
                eval: EvalContext::new(0, CancelToken::new()),
                spill: Arc::new(SpillManager::with_fixed_cap(
                    dir.path().to_path_buf(),
                    spill_cap,
                )),
                workers: Arc::new(WorkerPool::new(partitions.max(1)).unwrap()),
                batch_size: 128,
                target_partitions: partitions,
                metrics: ExecMetrics::default(),
            };
            (ctx, dir)
        }

        fn run_source(
            source: &dyn ScanSource,
            spec: &ScanSpec,
            ctx: &ExecContext,
        ) -> Result<Vec<Batch>, QueryError> {
            let partitions = source.open_partitions(spec, ctx)?;
            if partitions.is_empty() {
                return Ok(Vec::new());
            }
            let pool = Arc::clone(&ctx.pool);
            let sinks = run_pipeline(
                ctx,
                partitions,
                |_| Ok(Vec::new()),
                move |_| Ok(CollectSink::new(&pool)),
            )?;
            Ok(sinks
                .into_iter()
                .flat_map(CollectSink::into_batches)
                .collect())
        }

        fn write_table_file(
            rt: &tokio::runtime::Runtime,
            store: &Arc<dyn ObjectStore>,
            table: FileTable,
            day: NaiveDate,
            path: &str,
            batch: Batch,
            deletions_applied_seq: u64,
        ) -> Arc<FileMeta> {
            use object_store::ObjectStoreExt;
            let written = write_file_batch(table, 0, &[Arc::new(batch)]).unwrap();
            let size = written.bytes.len() as u64;
            rt.block_on(store.put(&ObjectPath::from(path), PutPayload::from(written.bytes)))
                .unwrap();
            Arc::new(FileMeta {
                id: FileId::new_v7(),
                table,
                project: project(),
                day,
                level: 0,
                path: path.to_owned(),
                size,
                footer_size: written.footer_size.max(8),
                rows: written.rows,
                min_time: written.min_time,
                max_time: written.max_time,
                min_ingest_time: written.min_ingest_time,
                max_ingest_time: written.max_ingest_time,
                fully_merged: false,
                deletions_applied_seq,
            })
        }

        fn col_expr(schema: &Schema, name: &str) -> Expr {
            let idx = schema.index_of(name).unwrap();
            Expr::Column {
                index: idx,
                name: name.to_owned(),
                data_type: schema.fields()[idx].data_type.clone(),
            }
        }

        struct HangingStore {
            inner: Arc<dyn ObjectStore>,
            hang: AtomicBool,
        }

        impl std::fmt::Debug for HangingStore {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("HangingStore")
                    .field("hang", &self.hang.load(Ordering::Relaxed))
                    .finish()
            }
        }

        impl std::fmt::Display for HangingStore {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "HangingStore")
            }
        }

        #[async_trait::async_trait]
        impl ObjectStore for HangingStore {
            async fn put_opts(
                &self,
                location: &ObjectPath,
                payload: PutPayload,
                opts: PutOptions,
            ) -> object_store::Result<PutResult> {
                self.inner.put_opts(location, payload, opts).await
            }

            async fn put_multipart_opts(
                &self,
                location: &ObjectPath,
                opts: PutMultipartOptions,
            ) -> object_store::Result<Box<dyn MultipartUpload>> {
                self.inner.put_multipart_opts(location, opts).await
            }

            async fn get_opts(
                &self,
                location: &ObjectPath,
                options: GetOptions,
            ) -> object_store::Result<GetResult> {
                self.inner.get_opts(location, options).await
            }

            async fn get_ranges(
                &self,
                location: &ObjectPath,
                ranges: &[Range<u64>],
            ) -> object_store::Result<Vec<Bytes>> {
                if self.hang.load(Ordering::Relaxed) {
                    futures::future::pending::<()>().await;
                }
                self.inner.get_ranges(location, ranges).await
            }

            fn delete_stream(
                &self,
                locations: BoxStream<'static, object_store::Result<ObjectPath>>,
            ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
                self.inner.delete_stream(locations)
            }

            fn list(
                &self,
                prefix: Option<&ObjectPath>,
            ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
                self.inner.list(prefix)
            }

            async fn list_with_delimiter(
                &self,
                prefix: Option<&ObjectPath>,
            ) -> object_store::Result<ListResult> {
                self.inner.list_with_delimiter(prefix).await
            }

            async fn copy_opts(
                &self,
                from: &ObjectPath,
                to: &ObjectPath,
                options: CopyOptions,
            ) -> object_store::Result<()> {
                self.inner.copy_opts(from, to, options).await
            }
        }

        #[test]
        fn spill_under_tight_memory_traces_and_scores() {
            struct RowCountSink {
                rows: usize,
            }

            impl PushSink for RowCountSink {
                fn consume(
                    &mut self,
                    batch: Batch,
                    _batch_res: &mut Reservation,
                ) -> Result<ControlFlow, QueryError> {
                    self.rows += batch.num_rows();
                    Ok(ControlFlow::Continue)
                }
            }

            fn count_source_rows(
                source: &dyn ScanSource,
                spec: &ScanSpec,
                ctx: &ExecContext,
            ) -> Result<(usize, u64), QueryError> {
                let partitions = source.open_partitions(spec, ctx)?;
                if partitions.is_empty() {
                    return Ok((0, 0));
                }
                let spilled_after_open = ctx.spill.spilled_bytes();
                let sinks = run_pipeline(
                    ctx,
                    partitions,
                    |_| Ok(Vec::new()),
                    |_| Ok(RowCountSink { rows: 0 }),
                )?;
                Ok((sinks.into_iter().map(|s| s.rows).sum(), spilled_after_open))
            }

            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            let cache = Arc::new(MetadataCache::new(1024 * 1024));
            let suspects = Suspects::new(Arc::new(VerifyQueue::default()));

            let base = 20_000 * DAY_NS;
            let day = day_of(base);

            let mut spans_vec: Vec<Span> = Vec::with_capacity(200);
            let mut scores_vec: Vec<Score> = Vec::with_capacity(200);
            for i in 1..=200_u16 {
                let mut s = span_row(i, 1, base + i64::from(i) * 100, base + 10);
                s.name = format!("trace-name-{:02}-{}", i % 16, "x".repeat(700));
                spans_vec.push(s);

                let mut sc = score_row(
                    u128::from(i),
                    i,
                    base + 100,
                    base + 200,
                    base + 200,
                    false,
                    &format!("score-{:02}-{}", i % 16, "y".repeat(700)),
                );
                sc.reason = Some(format!("reason-{:02}-{}", i % 16, "z".repeat(700)));
                scores_vec.push(sc);
            }

            let f_trace_a = write_table_file(
                &rt,
                &store,
                FileTable::Traces,
                day,
                "tables/traces/spill_a.parquet",
                partials_from_spans_batch(&span_rows(&spans_vec[..100])).unwrap(),
                0,
            );
            let f_trace_b = write_table_file(
                &rt,
                &store,
                FileTable::Traces,
                day,
                "tables/traces/spill_b.parquet",
                partials_from_spans_batch(&span_rows(&spans_vec[100..])).unwrap(),
                0,
            );
            let f_score_a = write_table_file(
                &rt,
                &store,
                FileTable::Scores,
                day,
                "tables/scores/spill_a.parquet",
                score_rows(&scores_vec[..100]),
                0,
            );
            let f_score_b = write_table_file(
                &rt,
                &store,
                FileTable::Scores,
                day,
                "tables/scores/spill_b.parquet",
                score_rows(&scores_vec[100..]),
                0,
            );

            // 1. Tight pool for TracesSource (96 KiB, 16-row batches).
            let (mut tight_trace_ctx, _tdir) = make_ctx(96 * 1024, 16 * 1024 * 1024, 1);
            tight_trace_ctx.batch_size = 16;
            let traces_src = TracesSource::new(
                Arc::clone(&store),
                project(),
                vec![f_trace_a, f_trace_b],
                crate::deletion::ProjectMemSnapshot::empty(),
                0,
                Arc::from([]),
                Arc::clone(&cache),
                suspects.clone(),
                rt.handle().clone(),
            )
            .unwrap();
            let t_schema = traces_src.schema();
            let t_spec = ScanSpec {
                projection: Some(vec![
                    t_schema.index_of(traces::TRACE_ID).unwrap(),
                    t_schema.index_of(traces::NAME).unwrap(),
                ]),
                ..Default::default()
            };
            let (total_traces, trace_spilled_bytes) =
                count_source_rows(&traces_src, &t_spec, &tight_trace_ctx).unwrap();
            assert_eq!(total_traces, 200);
            assert!(
                trace_spilled_bytes > 0,
                "expected TracesSource ExternalSort to spill under tight 96 KiB pool"
            );
            assert_eq!(tight_trace_ctx.spill.spilled_bytes(), 0);
            assert_eq!(tight_trace_ctx.pool.reserved(), 0);

            // 2. Tight pool for ScoresSource (96 KiB, 16-row batches).
            let (mut tight_score_ctx, _sdir) = make_ctx(96 * 1024, 16 * 1024 * 1024, 1);
            tight_score_ctx.batch_size = 16;
            let scores_src = ScoresSource::new(
                Arc::clone(&store),
                project(),
                vec![f_score_a, f_score_b],
                Memtable::new(Table::Scores).snapshot(),
                None,
                Arc::from([]),
                cache,
                suspects,
                rt.handle().clone(),
            )
            .unwrap();
            let s_schema = scores_src.schema();
            let s_spec = ScanSpec {
                projection: Some(vec![
                    s_schema.index_of(scores::SCORE_ID).unwrap(),
                    s_schema.index_of(scores::NAME).unwrap(),
                    s_schema.index_of(scores::REASON).unwrap(),
                ]),
                ..Default::default()
            };
            let (total_scores, score_spilled_bytes) =
                count_source_rows(&scores_src, &s_spec, &tight_score_ctx).unwrap();
            assert_eq!(total_scores, 200);
            assert!(
                score_spilled_bytes > 0,
                "expected ScoresSource ExternalSort to spill under tight 96 KiB pool"
            );
            assert_eq!(tight_score_ctx.spill.spilled_bytes(), 0);
            assert_eq!(tight_score_ctx.pool.reserved(), 0);
        }

        #[test]
        fn cancel_token_aborts_and_drains_synchronously_with_admission() {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            let inner_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            let hanging = Arc::new(HangingStore {
                inner: inner_store,
                hang: AtomicBool::new(false),
            });
            let store: Arc<dyn ObjectStore> = hanging.clone();
            let cache = Arc::new(MetadataCache::new(0));
            let suspects = Suspects::new(Arc::new(VerifyQueue::default()));

            let base = 20_000 * DAY_NS;
            let day = day_of(base);
            let f_trace = write_table_file(
                &rt,
                &store,
                FileTable::Traces,
                day,
                "tables/traces/cancel.parquet",
                partials_from_spans_batch(&span_rows(&[span_row(1, 1, base, base)])).unwrap(),
                0,
            );

            let admission = AsyncAdmissionPool::new(1024 * 1024);
            let held = rt
                .block_on(admission.admit(1024 * 1024, 1024 * 1024, Duration::from_millis(50)))
                .unwrap();
            let busy_err = rt
                .block_on(admission.admit(512 * 1024, 512 * 1024, Duration::from_millis(15)))
                .unwrap_err();
            assert!(matches!(busy_err, QueryError::Busy));
            drop(held);

            let scoped = rt
                .block_on(admission.admit(64 * 1024, 1024 * 1024, Duration::from_millis(50)))
                .unwrap();

            let dir = tempfile::tempdir().unwrap();
            let cancel_ctx = ExecContext {
                pool: Arc::clone(&scoped),
                eval: EvalContext::new(0, CancelToken::new()),
                spill: Arc::new(SpillManager::with_fixed_cap(
                    dir.path().to_path_buf(),
                    16 * 1024 * 1024,
                )),
                workers: Arc::new(WorkerPool::new(2).unwrap()),
                batch_size: 128,
                target_partitions: 2,
                metrics: ExecMetrics::default(),
            };

            let traces_src = TracesSource::new(
                store,
                project(),
                vec![f_trace],
                crate::deletion::ProjectMemSnapshot::empty(),
                0,
                Arc::from([]),
                cache,
                suspects,
                rt.handle().clone(),
            )
            .unwrap();

            hanging.hang.store(true, Ordering::Relaxed);
            let cancel = cancel_ctx.eval.cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(10));
                cancel.cancel();
            });

            let t0 = Instant::now();
            let err = run_source(&traces_src, &ScanSpec::default(), &cancel_ctx).unwrap_err();
            assert!(matches!(err, QueryError::Cancelled));
            assert!(
                t0.elapsed() < Duration::from_secs(2),
                "cancellation took {:?}",
                t0.elapsed()
            );
            assert_eq!(cancel_ctx.spill.spilled_bytes(), 0);
            assert_eq!(cancel_ctx.pool.reserved(), 0);
            drop(cancel_ctx);
            drop(scoped);
            assert_eq!(admission.used(), 0);
        }

        #[test]
        fn traces_and_scores_source_debug_is_privacy_safe() {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            let cache = Arc::new(MetadataCache::new(1024));
            let suspects = Suspects::new(Arc::new(VerifyQueue::default()));
            let base = 20_000 * DAY_NS;

            let mut s = span_row(1, 1, base, base + 100);
            s.name = "secret-span-name".to_owned();
            s.user_id = Some("alice-pii@example.com".to_owned());
            s.session_id = Some("secret-session-xyz".to_owned());
            s.input = Some("top-secret-prompt-content".to_owned());
            s.output = Some("top-secret-completion".to_owned());

            let mut spans_mt = Memtable::new(Table::Spans);
            spans_mt.insert(project(), span_rows(&[s]), 1).unwrap();

            let mut sc = score_row(1, 1, base, base, base, false, "secret-score-name");
            sc.user_id = Some("bob-pii@example.com".to_owned());
            sc.session_id = Some("secret-score-session".to_owned());
            sc.reason = Some("secret-score-reason".to_owned());

            let mut scores_mt = Memtable::new(Table::Scores);
            scores_mt.insert(project(), score_rows(&[sc]), 1).unwrap();

            let deletion = PendingDeletion {
                id: Uuid::now_v7(),
                seq: 1,
                project: project(),
                cutoff: base + 1_000,
                registered_at: base + 1_000,
                trace_ids: HashSet::new(),
                users: ["erased-user-pii".to_owned()].into_iter().collect(),
                sessions: ["erased-session-xyz".to_owned()].into_iter().collect(),
            };
            let tombstones = Tombstones::default().with(deletion);
            let deletions = tombstones.for_project(project());

            let projected_spans = crate::deletion::project_snapshot(
                Table::Spans,
                &spans_mt.snapshot(),
                project(),
                None,
                &deletions,
            )
            .unwrap();
            let traces_src = TracesSource::new(
                Arc::clone(&store),
                project(),
                Vec::new(),
                projected_spans,
                60_000_000_000,
                Arc::clone(&deletions),
                Arc::clone(&cache),
                suspects.clone(),
                rt.handle().clone(),
            )
            .unwrap();
            let widening_src = traces_src
                .clone()
                .with_list_mode(TraceListMode::Widening { fetch: 10 });
            let schema = traces_src.schema();
            let two_phase_src = traces_src.clone().with_list_mode(TraceListMode::TwoPhase {
                sort_keys: vec![SortKey {
                    expr: col_expr(&schema, traces::COST_TOTAL),
                    descending: true,
                    nulls_first: false,
                }],
                limit: 10,
                offset: 0,
            });

            let scores_src = ScoresSource::new(
                store,
                project(),
                Vec::new(),
                scores_mt.snapshot(),
                None,
                deletions,
                cache,
                suspects,
                rt.handle().clone(),
            )
            .unwrap();

            let forbidden = [
                "secret-span-name",
                "alice-pii@example.com",
                "secret-session-xyz",
                "top-secret-prompt-content",
                "top-secret-completion",
                "secret-score-name",
                "bob-pii@example.com",
                "secret-score-session",
                "secret-score-reason",
                "erased-user-pii",
                "erased-session-xyz",
            ];

            for dbg in [
                format!("{traces_src:?}"),
                format!("{widening_src:?}"),
                format!("{two_phase_src:?}"),
            ] {
                assert!(dbg.contains("TracesSource"), "missing struct name: {dbg}");
                assert!(dbg.contains("files: 0"), "missing file count: {dbg}");
                assert!(dbg.contains("mem_batches: 1"), "missing mem count: {dbg}");
                assert!(
                    dbg.contains("deletions: 1"),
                    "missing deletion count: {dbg}"
                );
                for secret in forbidden {
                    assert!(!dbg.contains(secret), "Debug leaked `{secret}`: {dbg}");
                }
            }

            let scores_dbg = format!("{scores_src:?}");
            assert!(
                scores_dbg.contains("ScoresSource"),
                "missing struct name: {scores_dbg}"
            );
            assert!(
                scores_dbg.contains("files: 0"),
                "missing file count: {scores_dbg}"
            );
            assert!(
                scores_dbg.contains("mem_batches: 1"),
                "missing mem count: {scores_dbg}"
            );
            assert!(
                scores_dbg.contains("deletions: 1"),
                "missing deletion count: {scores_dbg}"
            );
            for secret in forbidden {
                assert!(
                    !scores_dbg.contains(secret),
                    "Debug leaked `{secret}`: {scores_dbg}"
                );
            }
        }

        #[test]
        fn scores_source_pushes_score_id_and_trace_id_bloom_filters() {
            use std::collections::HashMap;
            use std::fmt;
            use std::sync::Mutex;

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            let path_reads: Arc<Mutex<HashMap<String, Vec<Range<u64>>>>> =
                Arc::new(Mutex::new(HashMap::new()));

            #[derive(Debug)]
            struct PathCountingStore {
                inner: Arc<dyn ObjectStore>,
                reads: Arc<Mutex<HashMap<String, Vec<Range<u64>>>>>,
            }
            impl fmt::Display for PathCountingStore {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("PathCountingStore")
                }
            }
            #[async_trait::async_trait]
            impl ObjectStore for PathCountingStore {
                async fn put_opts(
                    &self,
                    location: &ObjectPath,
                    payload: PutPayload,
                    opts: PutOptions,
                ) -> object_store::Result<PutResult> {
                    self.inner.put_opts(location, payload, opts).await
                }
                async fn put_multipart_opts(
                    &self,
                    location: &ObjectPath,
                    opts: PutMultipartOptions,
                ) -> object_store::Result<Box<dyn MultipartUpload>> {
                    self.inner.put_multipart_opts(location, opts).await
                }
                async fn get_opts(
                    &self,
                    location: &ObjectPath,
                    options: GetOptions,
                ) -> object_store::Result<GetResult> {
                    self.inner.get_opts(location, options).await
                }
                async fn get_ranges(
                    &self,
                    location: &ObjectPath,
                    ranges: &[Range<u64>],
                ) -> object_store::Result<Vec<Bytes>> {
                    self.reads
                        .lock()
                        .unwrap()
                        .entry(location.to_string())
                        .or_default()
                        .extend(ranges.iter().cloned());
                    self.inner.get_ranges(location, ranges).await
                }
                fn delete_stream(
                    &self,
                    locations: BoxStream<'static, object_store::Result<ObjectPath>>,
                ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
                    self.inner.delete_stream(locations)
                }
                fn list(
                    &self,
                    prefix: Option<&ObjectPath>,
                ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
                    self.inner.list(prefix)
                }
                async fn list_with_delimiter(
                    &self,
                    prefix: Option<&ObjectPath>,
                ) -> object_store::Result<ListResult> {
                    self.inner.list_with_delimiter(prefix).await
                }
                async fn copy_opts(
                    &self,
                    from: &ObjectPath,
                    to: &ObjectPath,
                    options: CopyOptions,
                ) -> object_store::Result<()> {
                    let _ = (from, to, options);
                    unreachable!()
                }
            }

            let counting = Arc::new(PathCountingStore {
                inner: Arc::clone(&inner),
                reads: Arc::clone(&path_reads),
            });
            let store: Arc<dyn ObjectStore> = counting;
            let cache = Arc::new(crate::file_format::FileMetadataCache::new(1024 * 1024));
            let suspects = Suspects::new(Arc::new(VerifyQueue::default()));

            let base = 20_000 * DAY_NS;
            let day = day_of(base);
            // File A: score_id in 1..=50 plus sentinel 10_000 (so min/max brackets 1_000..=1_049!), trace_id = trace(10) and trace(30)
            let mut rows_a = Vec::new();
            for i in 1..=50_u128 {
                rows_a.push(score_row(i, 10, base, base, base, false, "m"));
            }
            rows_a.push(score_row(10_000, 30, base, base, base, false, "m"));
            let mut rows_b = Vec::new();
            for i in 1_000..1_050_u128 {
                rows_b.push(score_row(i, 20, base, base, base, false, "m"));
            }

            let path_a = "tables/scores/bloom_a.parquet";
            let path_b = "tables/scores/bloom_b.parquet";
            let f_a = write_table_file(
                &rt,
                &store,
                FileTable::Scores,
                day,
                path_a,
                score_rows(&rows_a),
                0,
            );
            let f_b = write_table_file(
                &rt,
                &store,
                FileTable::Scores,
                day,
                path_b,
                score_rows(&rows_b),
                0,
            );

            let scores_src = ScoresSource::new(
                Arc::clone(&store),
                project(),
                vec![f_a, f_b],
                Memtable::new(Table::Scores).snapshot(),
                None,
                Arc::from([]),
                cache,
                suspects,
                rt.handle().clone(),
            )
            .unwrap();
            let schema = scores_src.schema();
            let (ctx, _dir) = make_ctx(8 * 1024 * 1024, 16 * 1024 * 1024, 1);

            // Warm metadata cache once.
            let _ = run_source(&scores_src, &ScanSpec::default(), &ctx).unwrap();
            path_reads.lock().unwrap().clear();

            // 1. Query by `score_id = Uuid(1010)`: File A min/max overlaps, so bloom filter MUST prune File A!
            let target_sid = Uuid::from_u128(1_010).to_string();
            let sid_spec = ScanSpec {
                filters: vec![Expr::Binary {
                    left: Box::new(col_expr(&schema, scores::SCORE_ID)),
                    op: cetra_query::BinaryOp::Eq,
                    right: Box::new(Expr::Literal(ScalarValue::Utf8(target_sid.clone()))),
                    data_type: DataType::Bool,
                }],
                ..Default::default()
            };
            let out = run_source(&scores_src, &sid_spec, &ctx).unwrap();
            let total: usize = out.iter().map(Batch::num_rows).sum();
            assert_eq!(total, 1);
            let reads_after_sid = path_reads.lock().unwrap().clone();
            let a_reads = reads_after_sid.get(path_a).cloned().unwrap_or_default();
            let b_reads = reads_after_sid.get(path_b).cloned().unwrap_or_default();
            assert_eq!(
                a_reads.len(),
                1,
                "File A should fetch ONLY its bloom filter (1 range), not data pages: {a_reads:?}"
            );
            assert!(
                b_reads.len() > 1,
                "File B should fetch bloom + data pages: {b_reads:?}"
            );

            // 2. Query by `score_id = Uuid(1010)` via `IdLookupHint`: File A min/max overlaps,
            // so bloom filter MUST prune File A!
            path_reads.lock().unwrap().clear();
            let sid_hint_spec = ScanSpec {
                id_lookups: vec![cetra_query::exec::IdLookupHint::Utf8 {
                    column: scores::SCORE_ID.to_owned(),
                    values: vec![target_sid],
                }],
                ..Default::default()
            };
            let out_sid_hint = run_source(&scores_src, &sid_hint_spec, &ctx).unwrap();
            let total_sid_hint: usize = out_sid_hint.iter().map(Batch::num_rows).sum();
            assert_eq!(total_sid_hint, 50);
            let reads_after_sid_hint = path_reads.lock().unwrap().clone();
            let a_sid_hint_reads = reads_after_sid_hint
                .get(path_a)
                .cloned()
                .unwrap_or_default();
            assert_eq!(
                a_sid_hint_reads.len(),
                1,
                "File A should fetch ONLY its score_id bloom filter (1 range): {a_sid_hint_reads:?}"
            );

            // 3. Query by `trace_id = trace(20)`: `trace_id` is NOT pushed pre-LWW (explicit
            // `score_id` upserts can change `trace_id`), so `ScoresSource` evaluates it post-LWW
            // and returns the 50 matching rows.
            path_reads.lock().unwrap().clear();
            let ScoreTarget::Trace {
                trace_id: target_tid,
            } = score_row(1, 20, base, base, base, false, "m").target
            else {
                panic!("expected Trace target");
            };
            let tid_spec = ScanSpec {
                id_lookups: vec![cetra_query::exec::IdLookupHint::Fixed16 {
                    column: scores::TRACE_ID.to_owned(),
                    values: vec![*target_tid.as_bytes()],
                }],
                filters: vec![Expr::Binary {
                    left: Box::new(col_expr(&schema, scores::TRACE_ID)),
                    op: cetra_query::BinaryOp::Eq,
                    right: Box::new(Expr::Literal(ScalarValue::FixedBytes16(
                        *target_tid.as_bytes(),
                    ))),
                    data_type: DataType::Bool,
                }],
                ..Default::default()
            };
            let out_tid = run_source(&scores_src, &tid_spec, &ctx).unwrap();
            let total_tid: usize = out_tid.iter().map(Batch::num_rows).sum();
            assert_eq!(total_tid, 50);
        }

        #[test]
        fn parallel_traces_and_scores_match_single_partition_and_honor_limit() {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
            let cache = Arc::new(crate::file_format::FileMetadataCache::new(1024 * 1024));
            let suspects = Suspects::new(Arc::new(VerifyQueue::default()));

            let base = 20_000 * DAY_NS;
            let day = day_of(base);
            let mut spans_vec = Vec::new();
            let mut scores_vec = Vec::new();
            for i in 1..=120_u16 {
                let t = base + i64::from(i) * 1_000;
                let mut sp = span_row(i, 1, t, t + 500);
                sp.cost_total = Some(f64::from(i));
                spans_vec.push(sp);
                // Write 2 versions of score `i` across two files to test LWW across parallel partitions!
                let mut sc_old = score_row(u128::from(i), 1, base, t, t, false, "q");
                sc_old.value = ScoreValue::Numeric(1.0);
                scores_vec.push(sc_old);
            }
            let mut scores_vec_new = Vec::new();
            for i in 1..=120_u16 {
                let t = base + i64::from(i) * 1_000;
                let mut sc_new = score_row(
                    u128::from(i),
                    1,
                    base,
                    t + 10_000,
                    t + 10_000,
                    i % 10 == 0,
                    "q",
                );
                sc_new.value = ScoreValue::Numeric(2.0);
                scores_vec_new.push(sc_new);
            }

            let f_trace_a = write_table_file(
                &rt,
                &store,
                FileTable::Traces,
                day,
                "tables/traces/par_a.parquet",
                partials_from_spans_batch(&span_rows(&spans_vec[..60])).unwrap(),
                0,
            );
            let f_trace_b = write_table_file(
                &rt,
                &store,
                FileTable::Traces,
                day,
                "tables/traces/par_b.parquet",
                partials_from_spans_batch(&span_rows(&spans_vec[60..])).unwrap(),
                0,
            );
            let f_score_a = write_table_file(
                &rt,
                &store,
                FileTable::Scores,
                day,
                "tables/scores/par_a.parquet",
                score_rows(&scores_vec),
                0,
            );
            let f_score_b = write_table_file(
                &rt,
                &store,
                FileTable::Scores,
                day,
                "tables/scores/par_b.parquet",
                score_rows(&scores_vec_new),
                0,
            );

            let traces_src = TracesSource::new(
                Arc::clone(&store),
                project(),
                vec![f_trace_a, f_trace_b],
                crate::deletion::ProjectMemSnapshot::empty(),
                0,
                Arc::from([]),
                Arc::clone(&cache),
                suspects.clone(),
                rt.handle().clone(),
            )
            .unwrap();
            let scores_src = ScoresSource::new(
                store,
                project(),
                vec![f_score_a, f_score_b],
                Memtable::new(Table::Scores).snapshot(),
                None,
                Arc::from([]),
                cache,
                suspects,
                rt.handle().clone(),
            )
            .unwrap();

            let (mut ctx_1, _d1) = make_ctx(16 * 1024 * 1024, 16 * 1024 * 1024, 1);
            ctx_1.batch_size = 17;
            let (mut ctx_4, _d4) = make_ctx(16 * 1024 * 1024, 16 * 1024 * 1024, 4);
            ctx_4.batch_size = 17;

            // 1. TracesSource range mode: `target_partitions = 4` returns > 1 stream & matches P=1 row-for-row.
            let t_schema = traces_src.schema();
            let t_spec = ScanSpec {
                projection: Some(vec![
                    t_schema.index_of(traces::TRACE_ID).unwrap(),
                    t_schema.index_of(traces::COST_TOTAL).unwrap(),
                ]),
                ..Default::default()
            };
            let t_streams_4 = traces_src.open_partitions(&t_spec, &ctx_4).unwrap();
            assert!(
                t_streams_4.len() > 1,
                "expected >1 TracesSource stream for target_partitions=4, got {}",
                t_streams_4.len()
            );
            let collect_trace_map =
                |batches: Vec<Batch>| -> std::collections::BTreeMap<[u8; 16], f64> {
                    let mut m = std::collections::BTreeMap::new();
                    for b in batches {
                        let b = b.compact(None).unwrap();
                        let id_c = b.column(0).unwrap();
                        let cost_c = b.column(1).unwrap();
                        for r in 0..b.num_rows() {
                            let id = super::super::read_fixed16_at(id_c, r).unwrap();
                            let ScalarValue::F64(cost) = cost_c.get_scalar(r).unwrap() else {
                                panic!("expected f64 cost");
                            };
                            m.insert(id, cost);
                        }
                    }
                    m
                };
            let t_map_1 = collect_trace_map(run_source(&traces_src, &t_spec, &ctx_1).unwrap());
            let t_map_4 = collect_trace_map(run_source(&traces_src, &t_spec, &ctx_4).unwrap());
            assert_eq!(t_map_1.len(), 120);
            assert_eq!(t_map_1, t_map_4);

            // TracesSource shared limit across parallel streams:
            let t_lim_spec = ScanSpec {
                limit: Some(15),
                ..t_spec.clone()
            };
            let t_lim_rows: usize = run_source(&traces_src, &t_lim_spec, &ctx_4)
                .unwrap()
                .iter()
                .map(Batch::num_rows)
                .sum();
            assert_eq!(t_lim_rows, 15);

            // TracesSource Widening & TwoPhase under P=4:
            let widening = traces_src
                .clone()
                .with_list_mode(TraceListMode::Widening { fetch: 10 });
            let w1 = collect_trace_map(run_source(&widening, &t_spec, &ctx_1).unwrap());
            let w4 = collect_trace_map(run_source(&widening, &t_spec, &ctx_4).unwrap());
            assert_eq!(w1.len(), 10);
            assert_eq!(w1, w4);

            let two_phase = traces_src.with_list_mode(TraceListMode::TwoPhase {
                sort_keys: vec![SortKey {
                    expr: col_expr(&t_schema, traces::COST_TOTAL),
                    descending: true,
                    nulls_first: false,
                }],
                limit: 10,
                offset: 5,
            });
            let tp1 = collect_trace_map(run_source(&two_phase, &t_spec, &ctx_1).unwrap());
            let tp4 = collect_trace_map(run_source(&two_phase, &t_spec, &ctx_4).unwrap());
            assert_eq!(tp1.len(), 10);
            assert_eq!(tp1, tp4);

            // 2. ScoresSource: `target_partitions = 4` returns > 1 stream, matches P=1 row-for-row (108 non-deleted, val=2.0),
            // and enforces shared `spec.limit`.
            let s_schema = scores_src.schema();
            let s_spec = ScanSpec {
                projection: Some(vec![
                    s_schema.index_of(scores::SCORE_ID).unwrap(),
                    s_schema.index_of(scores::VALUE_NUM).unwrap(),
                ]),
                ..Default::default()
            };
            let s_streams_4 = scores_src.open_partitions(&s_spec, &ctx_4).unwrap();
            assert!(
                s_streams_4.len() > 1,
                "expected >1 ScoresSource stream for target_partitions=4, got {}",
                s_streams_4.len()
            );
            let collect_score_map =
                |batches: Vec<Batch>| -> std::collections::BTreeMap<String, f64> {
                    let mut m = std::collections::BTreeMap::new();
                    for b in batches {
                        let b = b.compact(None).unwrap();
                        let id_c = b.column(0).unwrap();
                        let val_c = b.column(1).unwrap();
                        for r in 0..b.num_rows() {
                            let id = super::super::read_str_at(id_c, r).unwrap().to_owned();
                            let ScalarValue::F64(v) = val_c.get_scalar(r).unwrap() else {
                                panic!("expected f64 value_num");
                            };
                            m.insert(id, v);
                        }
                    }
                    m
                };
            let s_map_1 = collect_score_map(run_source(&scores_src, &s_spec, &ctx_1).unwrap());
            let s_map_4 = collect_score_map(run_source(&scores_src, &s_spec, &ctx_4).unwrap());
            assert_eq!(s_map_1.len(), 108);
            assert!(s_map_1.values().all(|&v| v == 2.0));
            assert_eq!(s_map_1, s_map_4);

            let s_lim_spec = ScanSpec {
                limit: Some(13),
                ..s_spec
            };
            let s_lim_rows: usize = run_source(&scores_src, &s_lim_spec, &ctx_4)
                .unwrap()
                .iter()
                .map(Batch::num_rows)
                .sum();
            assert_eq!(s_lim_rows, 13);
        }
    }
}
