//! The error boundary around Parquet and Arrow decoding (spec 03 §10): a
//! decoder panic becomes a decode error, so the read fails, its files become
//! suspects, and verification can quarantine them.

use std::fmt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use datafusion::common::Statistics;
use datafusion::common::config::ConfigOptions;
use datafusion::common::tree_node::TreeNodeRecursion;
use datafusion::datasource::physical_plan::FileScanConfig;
use datafusion::error::{DataFusionError, Result as DfResult};
use datafusion::execution::{SendableRecordBatchStream, TaskContext};
use datafusion::physical_expr::{PhysicalExpr, PhysicalSortExpr};
use datafusion::physical_plan::execution_plan::CardinalityEffect;
use datafusion::physical_plan::filter_pushdown::{
    ChildPushdownResult, FilterDescription, FilterPushdownPhase, FilterPushdownPropagation,
};
use datafusion::physical_plan::metrics::MetricsSet;
use datafusion::physical_plan::projection::ProjectionExec;
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    ChildStats, DisplayAs, DisplayFormatType, ExecutionPlan, PlanProperties,
    SortOrderPushdownResult, StatisticsArgs,
};
use datafusion_datasource::source::DataSourceExec;
use futures::{FutureExt, Stream};
use parquet::errors::ParquetError;

/// The error a caught decoder panic becomes. `verify::may_be_corrupt` treats
/// it as a decode error.
pub(crate) fn decoder_panicked() -> ParquetError {
    ParquetError::General("decoder panicked".to_owned())
}

/// Runs `f`, turning a panic into `Err(decoder_panicked())`.
pub(crate) fn guard_sync<T>(
    f: impl FnOnce() -> Result<T, ParquetError>,
) -> Result<T, ParquetError> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| Err(decoder_panicked()))
}

/// Polls `future`, turning a panic into `Err(decoder_panicked())`.
pub(crate) async fn guard<T>(
    future: impl Future<Output = Result<T, ParquetError>>,
) -> Result<T, ParquetError> {
    AssertUnwindSafe(future)
        .catch_unwind()
        .await
        .unwrap_or_else(|_| Err(decoder_panicked()))
}

/// A stream whose panics become one `on_panic()` item, after which it ends.
pub(crate) struct GuardedStream<S: Stream> {
    inner: Option<Pin<Box<S>>>,
    on_panic: fn() -> S::Item,
}

impl<S: Stream> GuardedStream<S> {
    /// Wraps `stream`. `on_panic` builds the item a panic becomes.
    pub(crate) fn new(stream: S, on_panic: fn() -> S::Item) -> Self {
        Self {
            inner: Some(Box::pin(stream)),
            on_panic,
        }
    }
}

impl<S: Stream> Stream for GuardedStream<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<S::Item>> {
        // The reader is taken out while it runs: after a panic it is dropped, never polled again.
        let Some(mut inner) = self.inner.take() else {
            return Poll::Ready(None);
        };
        match catch_unwind(AssertUnwindSafe(|| inner.as_mut().poll_next(cx))) {
            Ok(poll) => {
                self.inner = Some(inner);
                poll
            }
            Err(_) => {
                tracing::error!("a Parquet decoder panicked; the read fails as a decode error");
                Poll::Ready(Some((self.on_panic)()))
            }
        }
    }
}

/// The item a panicking Parquet scan yields at the DataFusion boundary.
fn scan_panicked() -> DfResult<RecordBatch> {
    Err(DataFusionError::ParquetError(Box::new(decoder_panicked())))
}

/// Builds a Parquet scan under a [`GuardExec`]. Every Parquet scan of stored
/// files is built with this (spec 03 §10).
pub(crate) fn guarded_scan(config: FileScanConfig) -> Arc<dyn ExecutionPlan> {
    wrap(DataSourceExec::from_data_source(config))
}

fn wrap(input: Arc<dyn ExecutionPlan>) -> Arc<dyn ExecutionPlan> {
    Arc::new(GuardExec::new(input))
}

/// A transparent wrapper over a Parquet scan. Its streams' panics become a
/// `DataFusionError::ParquetError(decoder_panicked())`.
///
/// It delegates everything the optimizer and the executor ask of its input, so
/// it changes no plan except for its own line. Its `downcast_delegate` makes
/// `is` and `downcast_ref` see the scan.
#[derive(Debug)]
pub(crate) struct GuardExec {
    input: Arc<dyn ExecutionPlan>,
    properties: Arc<PlanProperties>,
}

impl GuardExec {
    fn new(input: Arc<dyn ExecutionPlan>) -> Self {
        let properties = Arc::clone(input.properties());
        Self { input, properties }
    }

    /// The single child, or an error when the children do not match.
    fn one_child(children: Vec<Arc<dyn ExecutionPlan>>) -> DfResult<Arc<dyn ExecutionPlan>> {
        <[Arc<dyn ExecutionPlan>; 1]>::try_from(children)
            .map(|[child]| child)
            .map_err(|_| DataFusionError::Internal("GuardExec takes exactly one child".to_owned()))
    }
}

impl DisplayAs for GuardExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "GuardExec")
    }
}

impl ExecutionPlan for GuardExec {
    fn name(&self) -> &str {
        "GuardExec"
    }

    fn schema(&self) -> SchemaRef {
        self.input.schema()
    }

    fn properties(&self) -> &Arc<PlanProperties> {
        &self.properties
    }

    fn downcast_delegate(&self) -> Option<&dyn ExecutionPlan> {
        Some(self.input.as_ref())
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![&self.input]
    }

    fn maintains_input_order(&self) -> Vec<bool> {
        vec![true; self.children().len()]
    }

    // A leaf for repartitioning: no RepartitionExec goes between the guard and its scan.
    fn benefits_from_input_partitioning(&self) -> Vec<bool> {
        vec![false]
    }

    fn apply_expressions(
        &self,
        _f: &mut dyn FnMut(&Arc<dyn PhysicalExpr>) -> DfResult<TreeNodeRecursion>,
    ) -> DfResult<TreeNodeRecursion> {
        // The wrapper owns no expressions, as DataFusion's CooperativeExec.
        Ok(TreeNodeRecursion::Continue)
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> DfResult<Arc<dyn ExecutionPlan>> {
        Ok(wrap(Self::one_child(children)?))
    }

    fn with_new_children_and_same_properties(
        self: Arc<Self>,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> DfResult<Arc<dyn ExecutionPlan>> {
        Ok(Arc::new(Self {
            input: Self::one_child(children)?,
            properties: Arc::clone(&self.properties),
        }))
    }

    fn repartitioned(
        &self,
        target_partitions: usize,
        config: &ConfigOptions,
    ) -> DfResult<Option<Arc<dyn ExecutionPlan>>> {
        Ok(self
            .input
            .repartitioned(target_partitions, config)?
            .map(wrap))
    }

    fn execute(
        &self,
        partition: usize,
        context: Arc<TaskContext>,
    ) -> DfResult<SendableRecordBatchStream> {
        let schema = self.input.schema();
        let stream = match catch_unwind(AssertUnwindSafe(|| self.input.execute(partition, context)))
        {
            Ok(stream) => stream?,
            Err(_) => {
                tracing::error!("a Parquet decoder panicked; the read fails as a decode error");
                return Err(DataFusionError::ParquetError(Box::new(decoder_panicked())));
            }
        };
        Ok(Box::pin(RecordBatchStreamAdapter::new(
            schema,
            GuardedStream::new(stream, scan_panicked),
        )))
    }

    fn metrics(&self) -> Option<MetricsSet> {
        self.input.metrics()
    }

    fn statistics_from_inputs(
        &self,
        input_stats: &[Arc<Statistics>],
        _args: &StatisticsArgs,
    ) -> DfResult<Arc<Statistics>> {
        input_stats.first().map(Arc::clone).ok_or_else(|| {
            DataFusionError::Internal("GuardExec has no input statistics".to_owned())
        })
    }

    fn child_stats_requests(&self, partition: Option<usize>) -> Vec<ChildStats> {
        vec![ChildStats::At(partition)]
    }

    fn supports_limit_pushdown(&self) -> bool {
        true
    }

    fn fetch(&self) -> Option<usize> {
        self.input.fetch()
    }

    fn with_fetch(&self, limit: Option<usize>) -> Option<Arc<dyn ExecutionPlan>> {
        self.input.with_fetch(limit).map(wrap)
    }

    fn cardinality_effect(&self) -> CardinalityEffect {
        self.input.cardinality_effect()
    }

    fn try_swapping_with_projection(
        &self,
        projection: &ProjectionExec,
    ) -> DfResult<Option<Arc<dyn ExecutionPlan>>> {
        Ok(self
            .input
            .try_swapping_with_projection(projection)?
            .map(wrap))
    }

    fn gather_filters_for_pushdown(
        &self,
        _phase: FilterPushdownPhase,
        parent_filters: Vec<Arc<dyn PhysicalExpr>>,
        _config: &ConfigOptions,
    ) -> DfResult<FilterDescription> {
        FilterDescription::from_children(parent_filters, &self.children())
    }

    fn handle_child_pushdown_result(
        &self,
        _phase: FilterPushdownPhase,
        child_pushdown_result: ChildPushdownResult,
        _config: &ConfigOptions,
    ) -> DfResult<FilterPushdownPropagation<Arc<dyn ExecutionPlan>>> {
        Ok(FilterPushdownPropagation::if_all(child_pushdown_result))
    }

    fn with_new_state(
        &self,
        state: Arc<dyn std::any::Any + Send + Sync>,
    ) -> Option<Arc<dyn ExecutionPlan>> {
        self.input.with_new_state(state).map(wrap)
    }

    fn try_pushdown_sort(
        &self,
        order: &[PhysicalSortExpr],
    ) -> DfResult<SortOrderPushdownResult<Arc<dyn ExecutionPlan>>> {
        Ok(match self.input.try_pushdown_sort(order)? {
            SortOrderPushdownResult::Exact { inner } => {
                SortOrderPushdownResult::Exact { inner: wrap(inner) }
            }
            SortOrderPushdownResult::Inexact { inner } => {
                SortOrderPushdownResult::Inexact { inner: wrap(inner) }
            }
            SortOrderPushdownResult::Unsupported => SortOrderPushdownResult::Unsupported,
        })
    }

    fn with_preserve_order(&self, preserve_order: bool) -> Option<Arc<dyn ExecutionPlan>> {
        self.input.with_preserve_order(preserve_order).map(wrap)
    }
}

#[cfg(test)]
mod tests {
    use std::fmt;
    use std::sync::Arc;

    use async_trait::async_trait;
    use datafusion::physical_plan::displayable;
    use futures::stream::BoxStream;
    use llmobs_core::schema::{scores, spans};
    use object_store::memory::InMemory;
    use object_store::path::Path;
    use object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
        ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
    };
    use uuid::Uuid;

    use super::*;
    use crate::anchor::Anchors;
    use crate::catalog::{FileId, FileMeta, FileTable};
    use crate::compaction::slice::time_values;
    use crate::error::{AnchorError, CompactionError};
    use crate::fuzz_entry::tests::parquet_bytes;
    use crate::fuzz_entry::{parquet_meta, parquet_query, runtime};
    use crate::verify::{Verdict, is_store_error, may_be_corrupt, verify_file};
    use crate::{ListScope, SortOrder, SpanListRequest, SpanSort, span_list};

    const DAY_NS: i64 = 86_400 * 1_000_000_000;

    /// A store whose range reads panic, as a decoder panic does inside a scan's poll.
    /// `head` and full reads pass through, so the panic happens at the footer read.
    #[derive(Debug, Default)]
    struct PanicStore {
        inner: InMemory,
    }

    impl fmt::Display for PanicStore {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "PanicStore")
        }
    }

    #[async_trait]
    impl ObjectStore for PanicStore {
        async fn put_opts(
            &self,
            location: &Path,
            payload: PutPayload,
            opts: PutOptions,
        ) -> object_store::Result<PutResult> {
            self.inner.put_opts(location, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            location: &Path,
            opts: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }

        async fn get_opts(
            &self,
            location: &Path,
            options: GetOptions,
        ) -> object_store::Result<GetResult> {
            if options.range.is_some() {
                panic!("test store panics on read");
            }
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<Path>>,
        ) -> BoxStream<'static, object_store::Result<Path>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&Path>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&Path>,
        ) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &Path,
            to: &Path,
            options: CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    /// Puts one written file of `table` into `store` at its fixed path, and returns its catalog row.
    fn stored(table: FileTable, store: &Arc<dyn ObjectStore>) -> FileMeta {
        let written = parquet_bytes(table);
        let meta = parquet_meta(table, written.bytes.len() as u64, written.footer_size);
        runtime().block_on(async {
            store
                .put(
                    &Path::from(meta.path.as_str()),
                    PutPayload::from(written.bytes.clone()),
                )
                .await
                .expect("store the test file")
        });
        meta
    }

    /// Every span of the seed file, with the default exclusions off.
    fn all_spans() -> SpanListRequest<'static> {
        SpanListRequest {
            filter: None,
            search: None,
            scope: ListScope {
                window: None,
                include_eval: true,
                include_experiments: true,
            },
            sort: SpanSort::StartTime,
            order: SortOrder::Desc,
            limit: 100,
            cursor: None,
            timeout: std::time::Duration::from_secs(30),
        }
    }

    #[test]
    fn guard_sync_turns_a_panic_into_an_error() {
        let result: Result<(), ParquetError> =
            guard_sync(|| -> Result<(), ParquetError> { panic!("boom") });
        assert!(matches!(result, Err(ref error) if error.to_string().contains("decoder panicked")));
    }

    #[test]
    fn a_panicking_scan_fails_the_query_and_suspects_its_file() {
        let store: Arc<dyn ObjectStore> = Arc::new(PanicStore::default());
        let meta = stored(FileTable::Spans, &store);
        let (ctx, verify) = parquet_query(store, &meta);
        let result = runtime().block_on(span_list(&ctx, all_spans()));
        assert!(result.is_err(), "a panicking read must fail the query");
        assert_eq!(verify.take(10), vec![FileId(Uuid::from_u128(1))]);
    }

    #[test]
    fn the_runtime_is_usable_after_a_caught_panic() {
        let store: Arc<dyn ObjectStore> = Arc::new(PanicStore::default());
        let meta = stored(FileTable::Spans, &store);
        let (panicking, _) = parquet_query(store, &meta);
        let _ = runtime().block_on(span_list(&panicking, all_spans()));

        let plain: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let meta = stored(FileTable::Spans, &plain);
        let (ctx, _) = parquet_query(plain, &meta);
        let page = runtime()
            .block_on(span_list(&ctx, all_spans()))
            .expect("a plain read works after the panic");
        let rows: usize = page.batches.iter().map(|batch| batch.num_rows()).sum();
        assert_eq!(rows, crate::test_support::dataset_spans().len());
    }

    #[test]
    fn verify_reports_a_panicking_read_as_corrupt() {
        let store: Arc<dyn ObjectStore> = Arc::new(PanicStore::default());
        let meta = stored(FileTable::Spans, &store);
        let verdict = runtime().block_on(verify_file(&store, &meta));
        assert_eq!(
            verdict,
            Verdict::Corrupt {
                reason: "decode failed"
            }
        );
    }

    #[test]
    fn anchor_rebuild_skips_a_panicking_file() {
        let store: Arc<dyn ObjectStore> = Arc::new(PanicStore::default());
        let meta = stored(FileTable::Traces, &store);
        let mut anchors = Anchors::new(DAY_NS);
        let result = runtime().block_on(anchors.rebuild_file(&meta, store, 0));
        assert!(
            matches!(result, Err(AnchorError::Parquet(ref error)) if !is_store_error(error)),
            "expected a decode error, got {result:?}"
        );
    }

    #[test]
    fn time_values_reports_a_panic_as_a_corrupt_read() {
        let store: Arc<dyn ObjectStore> = Arc::new(PanicStore::default());
        let meta = stored(FileTable::Spans, &store);
        let result = runtime().block_on(time_values(
            Arc::clone(&store),
            &[Arc::new(meta)],
            spans::START_TIME,
        ));
        match result {
            Err(CompactionError::Read { source }) => assert!(may_be_corrupt(&source)),
            other => panic!("expected a read error, got {other:?}"),
        }
        // The scores column reads through the same guard.
        let meta = stored(FileTable::Scores, &store);
        let result = runtime().block_on(time_values(
            Arc::clone(&store),
            &[Arc::new(meta)],
            scores::CREATED_AT,
        ));
        assert!(matches!(result, Err(CompactionError::Read { .. })));
    }

    /// The optimized plan of the fixed spans read, as `GuardExec` leaves it (R3.2's plan,
    /// stripped of indentation). R3.3 must not change any other line.
    const EXPECTED: &[&str] = &[
        "ProjectionExec: expr=[span_id@0 as span_id]",
        "SortPreservingMergeExec: [start_time@1 DESC], fetch=10",
        "SortExec: TopK(fetch=10), expr=[start_time@1 DESC], preserve_partitioning=[true]",
        "ProjectionExec: expr=[encode(span_id@0, hex) as span_id, start_time@1 as start_time]",
        "RepartitionExec: partitioning=RoundRobinBatch(2), input_partitions=1",
        "DataSourceExec: file_groups={1 group: [[tables/spans/fuzz.parquet]]}, projection=[span_id, start_time, status], file_type=parquet, predicate=status@9 = error AND status@9 = error AND DynamicFilter [ empty ], sort_order_for_reorder=[start_time@1 DESC], reverse_row_groups=true, dynamic_rg_pruning=eligible, pruning_predicate=status_null_count@2 != row_count@3 AND status_min@0 <= error AND error <= status_max@1 AND status_null_count@2 != row_count@3 AND status_min@0 <= error AND error <= status_max@1, required_guarantees=[status in (error)]",
    ];

    #[test]
    fn guard_changes_no_other_plan_node() {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let meta = stored(FileTable::Spans, &store);
        let (ctx, _) = parquet_query(store, &meta);
        let plan = runtime().block_on(async {
            ctx.session()
                .sql("SELECT span_id FROM spans WHERE status = 'error' ORDER BY start_time DESC LIMIT 10")
                .await
                .expect("the query parses")
                .create_physical_plan()
                .await
                .expect("the query plans")
        });
        let text = displayable(plan.as_ref()).indent(false).to_string();
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("GuardExec"))
            .collect();
        assert_eq!(lines, EXPECTED);
    }

    /// The optimized plan of `sql` over the seed spans file, indentation stripped and `GuardExec`
    /// lines removed.
    fn plan_text(sql: &str) -> Vec<String> {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let meta = stored(FileTable::Spans, &store);
        let (ctx, _) = parquet_query(store, &meta);
        let plan = runtime().block_on(async {
            ctx.session()
                .sql(sql)
                .await
                .expect("the query parses")
                .create_physical_plan()
                .await
                .expect("the query plans")
        });
        displayable(plan.as_ref())
            .indent(false)
            .to_string()
            .lines()
            .map(|line| line.trim().to_owned())
            .filter(|line| !line.starts_with("GuardExec"))
            .collect()
    }

    /// Baselines captured with the scan unwrapped; the guard must not change them.
    #[test]
    fn guard_changes_no_other_plan_in_more_shapes() {
        let limit = [
            "ProjectionExec: expr=[encode(span_id@0, hex) as span_id]",
            "RepartitionExec: partitioning=RoundRobinBatch(2), input_partitions=1",
            "DataSourceExec: file_groups={1 group: [[tables/spans/fuzz.parquet]]}, projection=[span_id], limit=10, file_type=parquet",
        ];
        assert_eq!(
            plan_text("SELECT span_id FROM spans LIMIT 10"),
            limit,
            "LIMIT without a sort"
        );
        let filter = [
            "ProjectionExec: expr=[encode(span_id@0, hex) as span_id]",
            "RepartitionExec: partitioning=RoundRobinBatch(2), input_partitions=1",
            "DataSourceExec: file_groups={1 group: [[tables/spans/fuzz.parquet]]}, projection=[span_id, status], file_type=parquet, predicate=status@9 = error AND status@9 = error, pruning_predicate=status_null_count@2 != row_count@3 AND status_min@0 <= error AND error <= status_max@1 AND status_null_count@2 != row_count@3 AND status_min@0 <= error AND error <= status_max@1, required_guarantees=[status in (error)]",
        ];
        assert_eq!(
            plan_text("SELECT span_id FROM spans WHERE status = 'error'"),
            filter,
            "a filter"
        );
        let group = [
            "ProjectionExec: expr=[status@0 as status, count(Int64(1))@1 as n]",
            "AggregateExec: mode=FinalPartitioned, gby=[status@0 as status], aggr=[count(Int64(1))]",
            "RepartitionExec: partitioning=Hash([status@0], 2), input_partitions=2",
            "AggregateExec: mode=Partial, gby=[status@0 as status], aggr=[count(Int64(1))]",
            "RepartitionExec: partitioning=RoundRobinBatch(2), input_partitions=1",
            "DataSourceExec: file_groups={1 group: [[tables/spans/fuzz.parquet]]}, projection=[status], file_type=parquet",
        ];
        assert_eq!(
            plan_text("SELECT status, count(*) AS n FROM spans GROUP BY status"),
            group,
            "a group-by"
        );
    }
}
