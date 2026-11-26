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

pub(super) fn read_bool_at(col: &cetra_column::Column, row: usize) -> Option<bool> {
    use cetra_column::{Column, FlatData, ScalarValue};
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

    mod integration_tests {
        use std::collections::HashSet;
        use std::ops::Range;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        use bytes::Bytes;
        use cetra_column::{Batch, CancelToken, GlobalPool, Schema};
        use cetra_core::ids::ProjectId;
        use cetra_core::schema::{scores, traces};
        use cetra_core::score::Score;
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
                eval: EvalContext {
                    now_ns: 0,
                    cancel: CancelToken::new(),
                },
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
                fn consume(&mut self, batch: Batch) -> Result<ControlFlow, QueryError> {
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
                Memtable::new(Table::Spans).snapshot(),
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
                eval: EvalContext {
                    now_ns: 0,
                    cancel: CancelToken::new(),
                },
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
                Memtable::new(Table::Spans).snapshot(),
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

            let traces_src = TracesSource::new(
                Arc::clone(&store),
                project(),
                Vec::new(),
                spans_mt.snapshot(),
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
    }
}
