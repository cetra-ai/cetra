//! Native `ScanSource` over an in-memory `MemSnapshot`.

use std::fmt;
use std::sync::Arc;

use cetra_column::{Batch, CancelToken, Reservation, Schema};
use cetra_core::ids::ProjectId;
use cetra_query::exec::{ExecContext, PartitionStream, ScanSource, ScanSpec};
use cetra_query::{EvalContext, Expr, QueryError, covered_predicate};
use chrono::NaiveDate;

use crate::catalog::FileTable;
use crate::deletion::{PendingDeletion, applicable};
use crate::memtable::{MemBatch, MemSnapshot};
use crate::wal::Table;

use super::{
    apply_tombstones_and_filters, day_start_ns, reconcile_res, rollback_res, to_tombstone_specs,
    tombstone_table,
};

/// In-memory [`ScanSource`] reading `MemSnapshot` partitions for one project.
#[derive(Clone)]
pub(crate) struct MemtableSource {
    table: Table,
    project: ProjectId,
    keep_from: Option<NaiveDate>,
    snapshot: Arc<MemSnapshot>,
    deletions: Arc<[Arc<PendingDeletion>]>,
    schema: Arc<Schema>,
}

impl fmt::Debug for MemtableSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemtableSource")
            .field("table", &self.table.name())
            .field("deletions", &self.deletions.len())
            .field("schema_len", &self.schema.len())
            .finish_non_exhaustive()
    }
}

impl MemtableSource {
    /// Creates a [`MemtableSource`] over `snapshot` for `project`, respecting `keep_from`
    /// retention and pending `deletions`.
    pub(crate) fn new(
        table: Table,
        project: ProjectId,
        keep_from: Option<NaiveDate>,
        snapshot: Arc<MemSnapshot>,
        deletions: Arc<[Arc<PendingDeletion>]>,
    ) -> Result<Self, QueryError> {
        let schema = FileTable::from(table).column_schema();
        Ok(Self {
            table,
            project,
            keep_from,
            snapshot,
            deletions,
            schema,
        })
    }

    fn selected_batches(&self, spec: &ScanSpec) -> Vec<MemBatch> {
        let min_ns = spec.time_range.min_ns;
        let max_ns = spec.time_range.max_ns;
        if let (Some(lo), Some(hi)) = (min_ns, max_ns)
            && lo > hi
        {
            return Vec::new();
        }
        let mut selected = Vec::new();
        for (key, batches) in &self.snapshot.partitions {
            if key.project != self.project {
                continue;
            }
            if self.keep_from.is_some_and(|keep| key.day < keep) {
                continue;
            }
            if !day_overlaps(key.day, min_ns, max_ns) {
                continue;
            }
            for batch in batches {
                if batch_overlaps(batch, min_ns, max_ns) {
                    selected.push(batch.clone());
                }
            }
        }
        selected
    }
}

impl ScanSource for MemtableSource {
    fn schema(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }

    fn projected_schema(&self, spec: &ScanSpec) -> Result<Arc<Schema>, QueryError> {
        match &spec.projection {
            Some(indices) => Ok(Arc::new(self.schema.project(indices)?)),
            None => Ok(Arc::clone(&self.schema)),
        }
    }

    fn open_partitions(
        &self,
        spec: &ScanSpec,
        ctx: &ExecContext,
    ) -> Result<Vec<Box<dyn PartitionStream>>, QueryError> {
        let _ = self.projected_schema(spec)?;
        let batches = self.selected_batches(spec);
        if batches.is_empty() {
            return Ok(Vec::new());
        }
        let slots = ctx.target_partitions.max(1).min(batches.len());
        let mut grouped = vec![Vec::new(); slots];
        let mut rows = vec![0usize; slots];
        for batch in batches {
            let mut best = 0;
            for idx in 1..slots {
                if rows[idx] < rows[best] {
                    best = idx;
                }
            }
            rows[best] = rows[best].saturating_add(batch.batch.num_rows());
            grouped[best].push(batch);
        }

        let file_table = FileTable::from(self.table);
        let streams = grouped
            .into_iter()
            .filter(|part| !part.is_empty())
            .map(|batches| {
                Box::new(MemPartitionStream {
                    table: file_table,
                    schema: Arc::clone(&self.schema),
                    batches,
                    cursor: 0,
                    deletions: Arc::clone(&self.deletions),
                    filters: spec.filters.clone(),
                    projection: spec.projection.clone(),
                    now_ns: ctx.eval.now_ns,
                }) as Box<dyn PartitionStream>
            })
            .collect();
        Ok(streams)
    }

    fn explain_summary(&self, spec: &ScanSpec) -> String {
        let project_batches: usize = self
            .snapshot
            .partitions
            .iter()
            .filter(|(key, _)| key.project == self.project)
            .map(|(_, batches)| batches.len())
            .sum();
        format!(
            "MemtableSource(table={}, batches={}, filters={})",
            self.table.name(),
            project_batches,
            spec.filters.len()
        )
    }
}

struct MemPartitionStream {
    table: FileTable,
    schema: Arc<Schema>,
    batches: Vec<MemBatch>,
    cursor: usize,
    deletions: Arc<[Arc<PendingDeletion>]>,
    filters: Vec<Expr>,
    projection: Option<Vec<usize>>,
    now_ns: i64,
}

impl PartitionStream for MemPartitionStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        mut res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_ref().map_or(0, |r| r.size());
        let eval_ctx = EvalContext {
            now_ns: self.now_ns,
            cancel: cancel.clone(),
        };

        while self.cursor < self.batches.len() {
            if cancel.is_cancelled() {
                rollback_res(res, before);
                return Err(QueryError::Cancelled);
            }
            let mem_batch = &self.batches[self.cursor];
            self.cursor += 1;
            if mem_batch.batch.num_rows() == 0 {
                continue;
            }

            let batch = Batch::clone(&mem_batch.batch);
            reconcile_res(res.as_deref_mut(), before, batch.memory_bytes())?;

            let covering = applicable(&self.deletions, None, mem_batch.min_ingest_time);
            let tombstone_pred = if covering.is_empty() {
                None
            } else {
                let specs = to_tombstone_specs(&covering);
                match covered_predicate(tombstone_table(self.table), &self.schema, &specs) {
                    Ok(p) => p,
                    Err(err) => {
                        rollback_res(res, before);
                        return Err(err);
                    }
                }
            };

            let mut filtered = apply_tombstones_and_filters(
                batch,
                tombstone_pred.as_ref(),
                &self.filters,
                &eval_ctx,
                res.as_deref_mut(),
                before,
            )?;
            if filtered.is_empty() {
                rollback_res(res.as_deref_mut(), before);
                continue;
            }

            if let Some(indices) = &self.projection {
                filtered = match filtered.into_project(indices) {
                    Ok(b) => b,
                    Err(err) => {
                        rollback_res(res, before);
                        return Err(err.into());
                    }
                };
            }

            reconcile_res(res, before, filtered.memory_bytes())?;
            return Ok(Some(filtered));
        }

        rollback_res(res, before);
        Ok(None)
    }
}

/// Exact day overlap matching `query::day_overlaps`.
fn day_overlaps(day: NaiveDate, min_ns: Option<i64>, max_ns: Option<i64>) -> bool {
    let Some(start) = day_start_ns(day) else {
        return true;
    };
    let Some(end) = day.succ_opt().and_then(day_start_ns) else {
        return true;
    };
    if let Some(min) = min_ns
        && end <= min
    {
        return false;
    }
    if let Some(max) = max_ns
        && start > max
    {
        return false;
    }
    true
}

fn batch_overlaps(batch: &MemBatch, min_ns: Option<i64>, max_ns: Option<i64>) -> bool {
    if let Some(min) = min_ns
        && batch.max_time < min
    {
        return false;
    }
    if let Some(max) = max_ns
        && batch.min_time > max
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use cetra_column::{DataType, GlobalPool, ScalarValue};
    use cetra_core::schema::spans;
    use cetra_query::BinaryOp;
    use cetra_query::exec::{ExecMetrics, SpillManager, TimeRangeHint, WorkerPool};
    use uuid::Uuid;

    use super::*;
    use crate::column_bridge::record_batch_to_column;
    use crate::engine::DAY_NS;
    use crate::memtable::Memtable;
    use crate::test_support::{span_row, span_rows};

    fn project(n: u8) -> ProjectId {
        format!("018f6b4e-7c1a-7c1a-8c1a-0000000000{n:02x}")
            .parse()
            .unwrap()
    }

    fn test_ctx(cap: usize) -> ExecContext {
        let dir = tempfile::tempdir().unwrap();
        ExecContext {
            pool: GlobalPool::standalone(cap),
            eval: EvalContext {
                now_ns: 0,
                cancel: CancelToken::new(),
            },
            spill: Arc::new(SpillManager::with_fixed_cap(dir.keep(), 1024 * 1024)),
            workers: Arc::new(WorkerPool::new(2).unwrap()),
            batch_size: 8192,
            target_partitions: 2,
            metrics: ExecMetrics::default(),
        }
    }

    #[test]
    fn mem_source_filters_retention_tombstones_and_empty_projection() {
        let p1 = project(1);
        let p2 = project(2);
        let mut mt = Memtable::new(Table::Spans);

        let mut s_old = span_row(1, 0, 1_000, 500);
        s_old.project_id = p1;
        let mut s_keep = span_row(2, 0, 2 * DAY_NS + 100, 1_000);
        s_keep.project_id = p1;
        s_keep.user_id = Some("alice".to_owned());
        let mut s_erased = span_row(3, 0, 2 * DAY_NS + 200, 900);
        s_erased.project_id = p1;
        s_erased.user_id = Some("bob".to_owned());
        let mut s_other_proj = span_row(4, 0, 2 * DAY_NS + 300, 1_000);
        s_other_proj.project_id = p2;

        mt.insert(
            p1,
            record_batch_to_column(&span_rows(&[s_old, s_keep, s_erased])).unwrap(),
            1,
        )
        .unwrap();
        mt.insert(
            p2,
            record_batch_to_column(&span_rows(&[s_other_proj])).unwrap(),
            2,
        )
        .unwrap();
        let snapshot = mt.snapshot();

        let keep_from = NaiveDate::from_ymd_opt(1970, 1, 2).unwrap();
        let deletion = Arc::new(PendingDeletion {
            id: Uuid::from_u128(1),
            seq: 1,
            project: p1,
            cutoff: 950,
            registered_at: 1000,
            trace_ids: HashSet::new(),
            users: ["bob".to_owned()].into_iter().collect(),
            sessions: HashSet::new(),
        });

        let source = MemtableSource::new(
            Table::Spans,
            p1,
            Some(keep_from),
            snapshot,
            Arc::from([deletion]),
        )
        .unwrap();

        let ctx = test_ctx(4 * 1024 * 1024);
        let user_col_idx = source.schema().index_of(spans::USER_ID).unwrap();
        let filter_alice = Expr::Binary {
            left: Box::new(Expr::Column {
                index: user_col_idx,
                name: spans::USER_ID.to_owned(),
                data_type: DataType::Utf8,
            }),
            op: BinaryOp::Eq,
            right: Box::new(Expr::Literal(ScalarValue::Utf8("alice".to_owned()))),
            data_type: DataType::Bool,
        };
        let spec = ScanSpec {
            projection: Some(vec![]),
            time_range: TimeRangeHint {
                min_ns: Some(DAY_NS),
                max_ns: Some(3 * DAY_NS),
                around_ns: None,
            },
            filters: vec![filter_alice],
            ..ScanSpec::default()
        };

        let mut streams = source.open_partitions(&spec, &ctx).unwrap();
        assert_eq!(streams.len(), 1);
        let mut res = ctx.pool.register("mem_scan", false);
        res.try_grow(32).unwrap();
        let before = res.size();

        let batch = streams[0]
            .next_batch(&ctx.eval.cancel, Some(&mut res))
            .unwrap()
            .unwrap();
        assert_eq!(res.size(), before + batch.memory_bytes());
        assert_eq!(batch.columns().len(), 0);
        assert_eq!(batch.num_rows(), 1);
        res.shrink(batch.memory_bytes());
        drop(batch);

        assert!(
            streams[0]
                .next_batch(&ctx.eval.cancel, Some(&mut res))
                .unwrap()
                .is_none()
        );
        assert_eq!(res.size(), before);
    }

    #[test]
    fn mem_source_debug_is_privacy_safe_and_explain_reports_project_batches() {
        let p1 = project(1);
        let p2 = project(2);
        let mut mt = Memtable::new(Table::Spans);

        let mut s1 = span_row(1, 0, DAY_NS + 100, 1_000);
        s1.project_id = p1;
        s1.user_id = Some("secret-alice@example.com".to_owned());
        s1.session_id = Some("secret-session-123".to_owned());
        let mut s2 = span_row(2, 0, DAY_NS + 200, 1_000);
        s2.project_id = p1;
        let mut s3 = span_row(3, 0, DAY_NS + 300, 1_000);
        s3.project_id = p2;
        s3.user_id = Some("secret-other-user".to_owned());

        mt.insert(p1, record_batch_to_column(&span_rows(&[s1])).unwrap(), 1)
            .unwrap();
        mt.insert(p1, record_batch_to_column(&span_rows(&[s2])).unwrap(), 2)
            .unwrap();
        mt.insert(p2, record_batch_to_column(&span_rows(&[s3])).unwrap(), 3)
            .unwrap();
        let snapshot = mt.snapshot();

        let deletion = Arc::new(PendingDeletion {
            id: Uuid::from_u128(1),
            seq: 1,
            project: p1,
            cutoff: 950,
            registered_at: 1000,
            trace_ids: HashSet::new(),
            users: ["secret-erased-user".to_owned()].into_iter().collect(),
            sessions: ["secret-erased-session".to_owned()].into_iter().collect(),
        });

        let source =
            MemtableSource::new(Table::Spans, p1, None, snapshot, Arc::from([deletion])).unwrap();

        let dbg = format!("{source:?}");
        assert!(dbg.contains("MemtableSource"));
        assert!(dbg.contains("spans"));
        assert!(dbg.contains("deletions: 1"));
        assert!(!dbg.contains("secret-alice"));
        assert!(!dbg.contains("secret-session-123"));
        assert!(!dbg.contains("secret-other-user"));
        assert!(!dbg.contains("secret-erased-user"));
        assert!(!dbg.contains("secret-erased-session"));

        let summary = source.explain_summary(&ScanSpec::default());
        assert_eq!(summary, "MemtableSource(table=spans, batches=2, filters=0)");
    }
}
