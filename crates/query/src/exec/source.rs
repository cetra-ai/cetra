//! Format-agnostic table/file scan boundary (`ScanSource`, `ScanSpec`, `PartitionStream`)
//! and in-memory [`BatchSource`].

use std::fmt;
use std::sync::Arc;

use cetra_column::{Batch, CancelToken, ColumnError, Reservation, Schema};

use crate::error::QueryError;
use crate::eval::eval_filter;
use crate::ir::{EvalContext, Expr};

use super::pipeline::ExecContext;

/// Inclusive UTC-nanosecond range hint extracted for file/row-group pruning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimeRangeHint {
    /// Inclusive lower bound (`>= min_ns`).
    pub min_ns: Option<i64>,
    /// Inclusive upper bound (`<= max_ns`).
    pub max_ns: Option<i64>,
    /// Optional `QueryOptions::around` anchor hint (±1 day pruning for spans/traces lookups, D154).
    pub around_ns: Option<i64>,
}

/// Binary or string ID equality / `IN (...)` lookup hint for bloom-filter and sort-key pruning.
/// Manual `fmt::Debug` prints column + value count only (never user/session IDs, `AGENTS.md` § Privacy).
#[derive(Clone, PartialEq, Eq)]
pub enum IdLookupHint {
    /// 16-byte binary IDs (e.g. `trace_id`).
    Fixed16 {
        /// Column name.
        column: String,
        /// 16-byte IDs to match.
        values: Vec<[u8; 16]>,
    },
    /// 8-byte binary IDs (e.g. `span_id`).
    Fixed8 {
        /// Column name.
        column: String,
        /// 8-byte IDs to match.
        values: Vec<[u8; 8]>,
    },
    /// String IDs (e.g. `session_id`, `user_id`, `score_id`).
    Utf8 {
        /// Column name.
        column: String,
        /// String IDs to match (redacted in `Debug`).
        values: Vec<String>,
    },
}

impl fmt::Debug for IdLookupHint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fixed16 { column, values } => f
                .debug_struct("Fixed16")
                .field("column", column)
                .field("count", &values.len())
                .finish(),
            Self::Fixed8 { column, values } => f
                .debug_struct("Fixed8")
                .field("column", column)
                .field("count", &values.len())
                .finish(),
            Self::Utf8 { column, values } => f
                .debug_struct("Utf8")
                .field("column", column)
                .field("count", &values.len())
                .finish(),
        }
    }
}

/// Substring search hint (`col LIKE/ILIKE '%needle%'`).
/// Manual `fmt::Debug` prints column + needle byte length + case flag only (never needle text!).
#[derive(Clone, PartialEq, Eq)]
pub struct SubstringHint {
    /// Column name.
    pub column: String,
    /// Unescaped literal needle.
    pub needle: String,
    /// True for `ILIKE`.
    pub case_insensitive: bool,
}

impl fmt::Debug for SubstringHint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubstringHint")
            .field("column", &self.column)
            .field("needle_len", &self.needle.len())
            .field("case_insensitive", &self.case_insensitive)
            .finish()
    }
}

/// Requested output sort hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortHint {
    /// Column name.
    pub column: String,
    /// True for `DESC`.
    pub descending: bool,
}

/// Format-agnostic scan specification passed to every [`ScanSource`] (`D-QE-3`).
#[derive(Debug, Clone, Default)]
pub struct ScanSpec {
    /// Projected column indices in the source's full schema (`None` = all columns; `Some(vec![])` = 0-column row-count batch).
    pub projection: Option<Vec<usize>>,
    /// Referenced map keys `(column_name, key)` for shredded readers (Strata).
    pub map_keys: Vec<(String, String)>,
    /// Time range hint on the table's primary time column (`start_time` or `created_at`).
    pub time_range: TimeRangeHint,
    /// ID lookup hints (`trace_id`, `span_id`, `session_id`, `user_id`, `score_id`).
    pub id_lookups: Vec<IdLookupHint>,
    /// Conjunctive predicates bound against the source's **full** `schema()`; every `ScanSource` MUST evaluate these exactly before projecting.
    pub filters: Vec<Expr>,
    /// Substring search hints.
    pub substrings: Vec<SubstringHint>,
    /// Optional requested ordering hints.
    pub sort_hints: Vec<SortHint>,
    /// Optional row limit hint (sources may still emit more; upstream `Limit` enforces exactness).
    pub limit: Option<usize>,
}

/// One partition stream pulled synchronously by a CPU worker inside a push pipeline.
pub trait PartitionStream: Send {
    /// Returns the next `Batch` (or `Ok(None)` when exhausted), checking `cancel`.
    /// `Reservation` postcondition: `res.size() == before + batch.memory_bytes()` on `Ok(Some)`,
    /// and restored to `before` on `Ok(None)` or `Err`.
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError>;
}

/// Format-agnostic table/file scan boundary (`ParquetSource`, future `StrataSource`,
/// `MemtableSource`, `TracesSource`, `ScoresSource`, and in-memory `BatchSource`).
pub trait ScanSource: Send + Sync + fmt::Debug {
    /// Full schema of this source (before `ScanSpec::projection`).
    fn schema(&self) -> Arc<Schema>;
    /// Output schema after applying `spec.projection`.
    fn projected_schema(&self, spec: &ScanSpec) -> Result<Arc<Schema>, QueryError>;
    /// Opens up to `ctx.target_partitions` synchronous partition streams for `spec`.
    /// Every emitted batch MUST have passed all `spec.filters` and match `projected_schema(spec)`.
    fn open_partitions(
        &self,
        spec: &ScanSpec,
        ctx: &ExecContext,
    ) -> Result<Vec<Box<dyn PartitionStream>>, QueryError>;
    /// Human-readable plan marker (e.g. `"mode=list"` / `"mode=range"` / `"mode=lookup"`).
    fn explain_summary(&self, _spec: &ScanSpec) -> String;
}

/// In-memory [`ScanSource`] backed by pre-built `cetra_column::Batch`es (evaluates `spec.filters`
/// exactly and supports `projection = Some(vec![])`).
#[derive(Debug, Clone)]
pub struct BatchSource {
    schema: Arc<Schema>,
    batches: Arc<Vec<Batch>>,
}

impl BatchSource {
    /// Builds an in-memory [`BatchSource`], verifying every batch matches `schema`.
    pub fn new(schema: Arc<Schema>, batches: Vec<Batch>) -> Result<Self, QueryError> {
        for batch in &batches {
            if batch.schema().as_ref() != schema.as_ref() {
                return Err(ColumnError::InvalidBatch {
                    reason: "batch schema does not match BatchSource schema",
                }
                .into());
            }
        }
        Ok(Self {
            schema,
            batches: Arc::new(batches),
        })
    }

    /// Creates an empty [`BatchSource`] with `schema`.
    pub fn empty(schema: Arc<Schema>) -> Self {
        Self {
            schema,
            batches: Arc::new(Vec::new()),
        }
    }
}

impl ScanSource for BatchSource {
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
        if self.batches.is_empty() {
            return Ok(Vec::new());
        }
        let num_parts = ctx.target_partitions.max(1).min(self.batches.len());
        let chunk_size = self.batches.len().div_ceil(num_parts);
        let mut streams: Vec<Box<dyn PartitionStream>> = Vec::with_capacity(num_parts);
        let mut start = 0usize;
        while start < self.batches.len() {
            let end = (start + chunk_size).min(self.batches.len());
            streams.push(Box::new(BatchPartitionStream {
                batches: Arc::clone(&self.batches),
                cursor: start,
                end,
                filters: spec.filters.clone(),
                projection: spec.projection.clone(),
                now_ns: ctx.eval.now_ns,
            }));
            start = end;
        }
        Ok(streams)
    }

    fn explain_summary(&self, spec: &ScanSpec) -> String {
        format!(
            "BatchSource(batches={}, filters={})",
            self.batches.len(),
            spec.filters.len()
        )
    }
}

struct BatchPartitionStream {
    batches: Arc<Vec<Batch>>,
    cursor: usize,
    end: usize,
    filters: Vec<Expr>,
    projection: Option<Vec<usize>>,
    now_ns: i64,
}

impl PartitionStream for BatchPartitionStream {
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

        while self.cursor < self.end {
            if cancel.is_cancelled() {
                rollback_res(res, before);
                return Err(QueryError::Cancelled);
            }
            let Some(src_batch) = self.batches.get(self.cursor) else {
                break;
            };
            self.cursor += 1;
            if src_batch.is_empty() {
                continue;
            }
            let mut batch = src_batch.clone();
            for filter in &self.filters {
                let mask = match eval_filter(filter, &batch, &eval_ctx, res.as_deref_mut()) {
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
                rollback_res(res.as_deref_mut(), before);
                if batch.is_empty() {
                    break;
                }
            }
            if batch.is_empty() {
                continue;
            }
            if let Some(indices) = &self.projection {
                batch = match batch.into_project(indices) {
                    Ok(b) => b,
                    Err(err) => {
                        rollback_res(res, before);
                        return Err(err.into());
                    }
                };
            }
            reconcile_res(res, before, batch.memory_bytes())?;
            return Ok(Some(batch));
        }

        rollback_res(res, before);
        Ok(None)
    }
}

fn rollback_res(res: Option<&mut Reservation>, before: usize) {
    if let Some(r) = res {
        let cur = r.size();
        if cur > before {
            r.shrink(cur - before);
        }
    }
}

fn reconcile_res(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{ExecMetrics, SpillManager, WorkerPool};
    use crate::ir::BinaryOp;
    use cetra_column::{
        Column, DataType, Field, FlatColumn, FlatData, GlobalPool, ScalarValue, Utf8Data,
    };

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
    fn batch_source_filters_empty_projection_and_privacy_debug() {
        let spec_privacy = ScanSpec {
            id_lookups: vec![
                IdLookupHint::Utf8 {
                    column: "user_id".to_owned(),
                    values: vec!["alice-secret-user".to_owned()],
                },
                IdLookupHint::Fixed16 {
                    column: "trace_id".to_owned(),
                    values: vec![[0xAB; 16]],
                },
                IdLookupHint::Fixed8 {
                    column: "span_id".to_owned(),
                    values: vec![[0xCD; 8]],
                },
            ],
            substrings: vec![SubstringHint {
                column: "input".to_owned(),
                needle: "secret-prompt-needle".to_owned(),
                case_insensitive: true,
            }],
            ..ScanSpec::default()
        };
        let dbg = format!("{spec_privacy:?}");
        assert!(!dbg.contains("alice-secret-user"), "leaked id in {dbg}");
        assert!(
            !dbg.contains("secret-prompt-needle"),
            "leaked needle in {dbg}"
        );
        assert!(dbg.contains("count: 1"));
        assert!(dbg.contains("needle_len: 20"));

        let schema = Schema::try_new_arc(vec![
            Field::new("id", DataType::I64, false),
            Field::new("tag", DataType::Utf8, false),
        ])
        .unwrap();
        let b1 = Batch::try_new_all(
            Arc::clone(&schema),
            vec![
                Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1, 2, 3, 4]), None).unwrap()),
                Column::Flat(
                    FlatColumn::try_new(
                        FlatData::Utf8(Utf8Data::try_from_slices(["a", "b", "a", "b"]).unwrap()),
                        None,
                    )
                    .unwrap(),
                ),
            ],
            4,
        )
        .unwrap();
        let source = BatchSource::new(Arc::clone(&schema), vec![b1]).unwrap();

        // Filter on column 0 (`id > 2`), then empty projection `Some(vec![])` (SELECT COUNT(*))
        let filter_gt2 = Expr::Binary {
            left: Box::new(Expr::Column {
                index: 0,
                name: "id".to_owned(),
                data_type: DataType::I64,
            }),
            op: BinaryOp::Gt,
            right: Box::new(Expr::Literal(ScalarValue::I64(2))),
            data_type: DataType::Bool,
        };
        let spec = ScanSpec {
            projection: Some(vec![]),
            filters: vec![filter_gt2.clone()],
            ..ScanSpec::default()
        };
        assert_eq!(source.projected_schema(&spec).unwrap().len(), 0);

        let ctx = test_ctx(1024 * 1024);
        let mut streams = source.open_partitions(&spec, &ctx).unwrap();
        assert_eq!(streams.len(), 1);
        let mut res = ctx.pool.register("scan", false);
        res.try_grow(64).unwrap();
        let before = res.size();

        let batch = streams[0]
            .next_batch(&ctx.eval.cancel, Some(&mut res))
            .unwrap()
            .unwrap();
        assert_eq!(res.size(), before + batch.memory_bytes());
        assert_eq!(batch.columns().len(), 0);
        assert_eq!(batch.num_rows(), 2);
        res.shrink(batch.memory_bytes());
        drop(batch);

        assert!(
            streams[0]
                .next_batch(&ctx.eval.cancel, Some(&mut res))
                .unwrap()
                .is_none()
        );
        assert_eq!(res.size(), before);

        // Reservation error restores `res` to `before`
        let tight_ctx = test_ctx(1024 * 1024);
        let spec_all = ScanSpec {
            projection: None,
            filters: vec![filter_gt2],
            ..ScanSpec::default()
        };
        let mut tight_streams = source.open_partitions(&spec_all, &tight_ctx).unwrap();
        let tiny_pool = GlobalPool::standalone(32);
        let mut tiny_res = tiny_pool.register("tiny", false);
        tiny_res.try_grow(16).unwrap();
        let err = tight_streams[0]
            .next_batch(&tight_ctx.eval.cancel, Some(&mut tiny_res))
            .unwrap_err();
        assert!(matches!(err, QueryError::TooLarge));
        assert_eq!(tiny_res.size(), 16);
    }
}
