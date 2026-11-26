//! `EngineContext`, base view schemas, projection builders, binary hex-ID rewrites,
//! and timeout-safe pipeline execution over `cetra-query`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use cetra_column::{
    Batch, CancelToken, DataType, Field, Reservation, ScalarValue, Schema, ScopedPool,
};
use cetra_core::ids::ProjectId;
use cetra_core::schema::{scores, scores_schema, spans, spans_schema, traces, traces_schema};
use cetra_query::exec::streaming::{ProjectOp, UnionSource};
#[cfg(test)]
use cetra_query::exec::{CollectSink, PushSink, run_pipeline as exec_run_pipeline};
use cetra_query::exec::{
    ControlFlow, DEFAULT_BATCH_SIZE, ExecContext, ExecMetrics, IdLookupHint, PartitionStream,
    PushOperator, ScanSource, ScanSpec,
};
use cetra_query::{BinaryOp, CastMode, EvalContext, Expr, FilterView, QueryError, ScalarFn};
use chrono::NaiveDate;

use crate::catalog::{FileMeta, FileTable};
use crate::deletion::{self, PendingDeletion};
use crate::engine::mem_source::MemtableSource;
use crate::engine::parquet_source::ParquetSource;
use crate::engine::scores_source::ScoresSource;
use crate::engine::traces_source::TracesSource;
use crate::error::EngineError;
use crate::memtable::MemSnapshot;
use crate::query_error::{InvalidQuery, Position, RunError};
use crate::verify::Suspects;
use crate::wal::Table;
use crate::{QeRuntime, QueryOptions, Storage};

pub(crate) const DURATION_NS: &str = "duration_ns";
pub(crate) const DURATION_MS: &str = "duration_ms";

/// Execution context for read queries over one snapshot of a project.
#[derive(Clone)]
pub(crate) struct EngineContext {
    pub(crate) spans: Arc<dyn ScanSource>,
    pub(crate) traces: Arc<TracesSource>,
    pub(crate) scores: Arc<ScoresSource>,
    pub(crate) suspects: Suspects,
    pub(crate) runtime: Arc<QeRuntime>,
    pub(crate) visible_from: Option<i64>,
    pub(crate) around: Option<i64>,
    pub(crate) force_range_mode: bool,
    pub(crate) pool: Option<Arc<ScopedPool>>,
    last_metrics: Arc<Mutex<Option<ExecMetrics>>>,
}

impl fmt::Debug for EngineContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineContext")
            .field("visible_from", &self.visible_from)
            .field("around", &self.around)
            .field("force_range_mode", &self.force_range_mode)
            .field("runtime", &self.runtime)
            .field("has_pool", &self.pool.is_some())
            .finish_non_exhaustive()
    }
}

struct CancelOnDrop(CancelToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl EngineContext {
    /// Forces range-mode execution on `trace_list` (disabling widening-window and two-phase list optimizations).
    pub(crate) fn set_force_range_mode(&mut self, force: bool) {
        self.force_range_mode = force;
    }

    /// Returns the [`ExecMetrics`] recorded by the most recent pipeline executed on this context.
    pub(crate) fn last_metrics(&self) -> Option<ExecMetrics> {
        self.last_metrics
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Returns the pre-admitted [`ScopedPool`], if this context was admitted via [`Storage::query`].
    pub(crate) fn pool(&self) -> Option<&Arc<ScopedPool>> {
        self.pool.as_ref()
    }

    /// Admits a query against `storage`'s [`QeRuntime`] admission pool up front (surfacing
    /// [`crate::error::QueryError::Busy`] on saturation) and then builds an [`EngineContext`]
    /// over `storage`'s newest published snapshot for `project`.
    pub(crate) async fn admit_from_storage(
        storage: &Storage,
        project: ProjectId,
        options: &QueryOptions,
    ) -> Result<Self, crate::error::QueryError> {
        let pool = storage
            .qe_runtime
            .admission
            .admit(
                storage.qe_runtime.query_memory.initial,
                storage.qe_runtime.query_memory.cap,
                storage.qe_runtime.query_memory.wait,
            )
            .await
            .map_err(|err| match err {
                QueryError::Busy => crate::error::QueryError::Busy,
                other => crate::error::QueryError::Internal(EngineError::from_query(&other)),
            })?;
        Self::build_from_storage(storage, project, options, Some(pool))
            .map_err(crate::error::QueryError::Internal)
    }

    /// Builds an [`EngineContext`] over `storage`'s current published snapshot for `project`.
    #[cfg(test)]
    pub(crate) fn from_storage(
        storage: &Storage,
        project: ProjectId,
        options: &QueryOptions,
    ) -> Result<Self, RunError> {
        Self::build_from_storage(storage, project, options, None).map_err(RunError::Internal)
    }

    fn build_from_storage(
        storage: &Storage,
        project: ProjectId,
        options: &QueryOptions,
        pool: Option<Arc<ScopedPool>>,
    ) -> Result<Self, EngineError> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| EngineError::internal("EngineContext requires an active Tokio runtime"))?;
        let state = storage.published.load();
        Self::build_from_state(
            &state,
            storage.objects.store(),
            project,
            options,
            Arc::clone(&storage.qe_runtime),
            storage.lookback,
            &storage.verify,
            handle,
            pool,
        )
    }

    /// Builds an [`EngineContext`] over an explicit [`ReadState`] and `ObjectStore`.
    #[cfg(any(test, fuzzing))]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_state(
        state: &crate::read_state::ReadState,
        store: Arc<dyn object_store::ObjectStore>,
        project: ProjectId,
        options: &QueryOptions,
        runtime: Arc<QeRuntime>,
        lookback: i64,
        verify: &Arc<crate::verify::VerifyQueue>,
        handle: tokio::runtime::Handle,
    ) -> Result<Self, EngineError> {
        Self::build_from_state(
            state, store, project, options, runtime, lookback, verify, handle, None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build_from_state(
        state: &crate::read_state::ReadState,
        store: Arc<dyn object_store::ObjectStore>,
        project: ProjectId,
        options: &QueryOptions,
        runtime: Arc<QeRuntime>,
        lookback: i64,
        verify: &Arc<crate::verify::VerifyQueue>,
        handle: tokio::runtime::Handle,
        pool: Option<Arc<ScopedPool>>,
    ) -> Result<Self, EngineError> {
        let suspects = Suspects::new(Arc::clone(verify));
        let spans_raw = spans_schema();

        if state.projects.is_dropped(project) {
            let empty_mem = Arc::new(MemSnapshot::default());
            let no_deletions: Arc<[Arc<PendingDeletion>]> = Arc::from(Vec::new());
            let spans: Arc<dyn ScanSource> = Arc::new(
                UnionSource::new(spans_raw, Vec::new()).map_err(|e| EngineError::from_query(&e))?,
            );
            let traces = Arc::new(
                TracesSource::new(
                    Arc::clone(&store),
                    project,
                    Vec::new(),
                    Arc::clone(&empty_mem),
                    lookback,
                    Arc::clone(&no_deletions),
                    Arc::clone(&runtime.metadata_cache),
                    suspects.clone(),
                    handle.clone(),
                )
                .map_err(|e| EngineError::from_query(&e))?,
            );
            let scores = Arc::new(
                ScoresSource::new(
                    store,
                    project,
                    Vec::new(),
                    empty_mem,
                    None,
                    no_deletions,
                    Arc::clone(&runtime.metadata_cache),
                    suspects.clone(),
                    handle,
                )
                .map_err(|e| EngineError::from_query(&e))?,
            );
            return Ok(Self {
                spans,
                traces,
                scores,
                suspects,
                runtime,
                visible_from: options.visible_from(),
                around: options.around(),
                force_range_mode: false,
                pool,
                last_metrics: Arc::new(Mutex::new(None)),
            });
        }

        let deletions = state.tombstones.for_project(project);
        let keep_from = state.projects.keep_from(project);
        let has_late = |snapshot: &MemSnapshot| {
            keep_from.is_some_and(|keep| {
                snapshot
                    .partitions
                    .keys()
                    .any(|key| key.project == project && key.day < keep)
            })
        };
        let (spans_snapshot, scores_snapshot) =
            if deletions.is_empty() && !has_late(&state.spans) && !has_late(&state.scores) {
                (Arc::clone(&state.spans), Arc::clone(&state.scores))
            } else {
                (
                    deletion::project_snapshot(
                        Table::Spans,
                        &state.spans,
                        project,
                        keep_from,
                        &deletions,
                    )
                    .map_err(|e| query_err_to_engine(&e))?,
                    deletion::project_snapshot(
                        Table::Scores,
                        &state.scores,
                        project,
                        keep_from,
                        &deletions,
                    )
                    .map_err(|e| query_err_to_engine(&e))?,
                )
            };

        let all_days = NaiveDate::MIN..=NaiveDate::MAX;
        let spans_files: Vec<Arc<FileMeta>> = state
            .catalog
            .files(FileTable::Spans, project, all_days.clone())
            .cloned()
            .collect();
        let traces_files: Vec<Arc<FileMeta>> = state
            .catalog
            .files(FileTable::Traces, project, all_days.clone())
            .cloned()
            .collect();
        let scores_files: Vec<Arc<FileMeta>> = state
            .catalog
            .files(FileTable::Scores, project, all_days)
            .cloned()
            .collect();

        let spans_parquet = ParquetSource::new(
            Arc::clone(&store),
            FileTable::Spans,
            Arc::clone(&spans_raw),
            spans_files,
            Arc::clone(&deletions),
            Arc::clone(&runtime.metadata_cache),
            suspects.clone(),
            handle.clone(),
        );
        let spans_mem = MemtableSource::new(
            Table::Spans,
            project,
            keep_from,
            Arc::clone(&spans_snapshot),
            Arc::clone(&deletions),
        )
        .map_err(|e| EngineError::from_query(&e))?;
        let spans: Arc<dyn ScanSource> = Arc::new(
            UnionSource::new(
                spans_raw,
                vec![Arc::new(spans_parquet), Arc::new(spans_mem)],
            )
            .map_err(|e| EngineError::from_query(&e))?,
        );

        let traces = Arc::new(
            TracesSource::new(
                Arc::clone(&store),
                project,
                traces_files,
                spans_snapshot,
                lookback,
                Arc::clone(&deletions),
                Arc::clone(&runtime.metadata_cache),
                suspects.clone(),
                handle.clone(),
            )
            .map_err(|e| EngineError::from_query(&e))?,
        );

        let scores = Arc::new(
            ScoresSource::new(
                store,
                project,
                scores_files,
                scores_snapshot,
                keep_from,
                deletions,
                Arc::clone(&runtime.metadata_cache),
                suspects.clone(),
                handle,
            )
            .map_err(|e| EngineError::from_query(&e))?,
        );

        Ok(Self {
            spans,
            traces,
            scores,
            suspects,
            runtime,
            visible_from: options.visible_from(),
            around: options.around(),
            force_range_mode: false,
            pool,
            last_metrics: Arc::new(Mutex::new(None)),
        })
    }

    /// Executes arbitrary synchronous query work on `spawn_blocking` with admission,
    /// `CancelToken`, `Drop` cancellation guard, and `timeout` worker drain.
    pub(crate) async fn run_with_ctx<T, F>(
        &self,
        timeout: Duration,
        work: F,
    ) -> Result<(T, ExecMetrics), RunError>
    where
        F: FnOnce(&ExecContext) -> Result<T, QueryError> + Send + 'static,
        T: Send + 'static,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        let workers = self.runtime.workers().map_err(query_err_to_run)?;
        let pool = match &self.pool {
            Some(p) => Arc::clone(p),
            None => {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return Err(RunError::Timeout);
                }
                let admission_wait = self.runtime.query_memory.wait;
                let wait = admission_wait.min(remaining);
                match tokio::time::timeout(
                    remaining,
                    self.runtime.admission.admit(
                        self.runtime.query_memory.initial,
                        self.runtime.query_memory.cap,
                        wait,
                    ),
                )
                .await
                {
                    Ok(Ok(p)) => p,
                    Ok(Err(QueryError::Busy))
                        if remaining <= admission_wait
                            && tokio::time::Instant::now() >= deadline =>
                    {
                        return Err(RunError::Timeout);
                    }
                    Ok(Err(err)) => return Err(query_err_to_run(err)),
                    Err(_) => return Err(RunError::Timeout),
                }
            }
        };

        let cancel = CancelToken::new();
        let cancel_guard = CancelOnDrop(cancel.clone());
        let metrics = ExecMetrics::new();
        let exec_ctx = ExecContext {
            pool,
            eval: EvalContext {
                now_ns: crate::catalog::now_ns(),
                cancel: cancel.clone(),
            },
            spill: Arc::clone(&self.runtime.spill),
            workers,
            batch_size: DEFAULT_BATCH_SIZE,
            target_partitions: self.runtime.target_partitions,
            metrics: metrics.clone(),
        };

        let mut handle = tokio::task::spawn_blocking(move || work(&exec_ctx));
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let res = match tokio::time::timeout(remaining, &mut handle).await {
            Ok(Ok(Ok(value))) => {
                drop(cancel_guard);
                Ok((value, metrics.clone()))
            }
            Ok(Ok(Err(err))) => {
                drop(cancel_guard);
                let run_err = query_err_to_run(err);
                if let RunError::Internal(engine) = &run_err {
                    self.suspects.report(engine);
                }
                Err(run_err)
            }
            Ok(Err(_join_err)) => {
                drop(cancel_guard);
                Err(RunError::Internal(EngineError::internal(
                    "query execution worker panicked",
                )))
            }
            Err(_elapsed) => {
                cancel.cancel();
                let _ = handle.await;
                drop(cancel_guard);
                Err(RunError::Timeout)
            }
        };
        *self
            .last_metrics
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(metrics);
        res
    }

    /// Runs a scan + push pipeline and combines the resulting sinks synchronously inside the
    /// bounded worker execution scope.
    #[cfg(test)]
    pub(crate) async fn run_pipeline<S, FChain, FSink, R, FCombine>(
        &self,
        source: Arc<dyn ScanSource>,
        spec: ScanSpec,
        make_chain: FChain,
        make_sink: FSink,
        combine: FCombine,
        timeout: Duration,
    ) -> Result<(R, ExecMetrics), RunError>
    where
        S: PushSink + Send + 'static,
        R: Send + 'static,
        FChain: Fn(usize, &ExecContext) -> Result<Vec<Box<dyn PushOperator>>, QueryError>
            + Send
            + Sync
            + 'static,
        FSink: Fn(usize, &ExecContext) -> Result<S, QueryError> + Send + Sync + 'static,
        FCombine: FnOnce(Vec<S>, &ExecContext) -> Result<R, QueryError> + Send + 'static,
    {
        self.run_with_ctx(timeout, move |ctx| {
            let partitions = source.open_partitions(&spec, ctx)?;
            let sinks = if partitions.is_empty() {
                Vec::new()
            } else {
                let ctx_chain = ctx.clone();
                let ctx_sink = ctx.clone();
                exec_run_pipeline(
                    ctx,
                    partitions,
                    move |idx| make_chain(idx, &ctx_chain),
                    move |idx| make_sink(idx, &ctx_sink),
                )?
            };
            combine(sinks, ctx)
        })
        .await
    }

    /// Scans `source` with `spec` and collects all output batches.
    #[cfg(test)]
    pub(crate) async fn collect_batches(
        &self,
        source: Arc<dyn ScanSource>,
        spec: ScanSpec,
        timeout: Duration,
    ) -> Result<(Vec<Batch>, ExecMetrics), RunError> {
        self.run_pipeline(
            source,
            spec,
            |_, _| Ok(Vec::new()),
            |_, ctx| Ok(CollectSink::new(&ctx.pool)),
            |sinks, _| {
                Ok(sinks
                    .into_iter()
                    .flat_map(CollectSink::into_batches)
                    .collect())
            },
            timeout,
        )
        .await
    }
}

fn query_err_to_engine(err: &crate::error::QueryError) -> EngineError {
    match err {
        crate::error::QueryError::Internal(engine) => *engine,
        crate::error::QueryError::Busy => EngineError::from_query(&QueryError::Busy),
    }
}

/// Maps a [`QueryError`] into a [`RunError`].
pub(crate) fn query_err_to_run(err: QueryError) -> RunError {
    match err {
        QueryError::TooLarge => RunError::TooLarge,
        QueryError::Cancelled => RunError::Timeout,
        QueryError::Invalid(inv) => RunError::Invalid(InvalidQuery {
            message: inv.message,
            position: inv.position.map(|p| Position {
                line: p.line,
                column: p.column,
            }),
        }),
        QueryError::InvalidRequest(msg) => RunError::InvalidRequest(msg),
        QueryError::Failed(msg) => RunError::Failed(msg),
        err @ (QueryError::Column(_) | QueryError::Internal(_) | QueryError::Busy) => {
            RunError::Internal(EngineError::from_query(&err))
        }
    }
}

/// Maps a [`cetra_query::InvalidQuery`] into a [`RunError`].
pub(crate) fn invalid_err_to_run(err: cetra_query::InvalidQuery) -> RunError {
    RunError::Invalid(InvalidQuery {
        message: err.message,
        position: err.position.map(|p| Position {
            line: p.line,
            column: p.column,
        }),
    })
}

/// Maps a [`RunError`] back to [`QueryError`] inside synchronous pipeline callbacks.
pub(crate) fn run_err_to_query(err: RunError) -> QueryError {
    match err {
        RunError::InvalidRequest(msg) => QueryError::InvalidRequest(msg),
        RunError::Invalid(inv) => QueryError::Invalid(cetra_query::InvalidQuery {
            message: inv.message,
            position: inv.position.map(|p| cetra_query::Position {
                line: p.line,
                column: p.column,
            }),
        }),
        RunError::Failed(msg) => QueryError::Failed(msg),
        RunError::Timeout => QueryError::Cancelled,
        RunError::TooLarge => QueryError::TooLarge,
        RunError::Internal(engine_err) => QueryError::internal(engine_err.kind_label()),
    }
}

/// Raw `spans` `cetra_column::Schema` (56 stored columns).
pub(crate) fn spans_raw_schema() -> Result<Arc<Schema>, RunError> {
    Ok(spans_schema())
}

/// Raw `traces` `cetra_column::Schema` (30 stored columns).
pub(crate) fn traces_raw_schema() -> Result<Arc<Schema>, RunError> {
    Ok(traces_schema())
}

/// Raw `scores` `cetra_column::Schema` (24 stored columns).
pub(crate) fn scores_raw_schema() -> Result<Arc<Schema>, RunError> {
    Ok(scores_schema())
}

/// View `spans` `cetra_column::Schema` (56 columns: skips `project_id`, hex IDs, adds `duration_ms`).
pub(crate) fn spans_view_schema() -> Result<Arc<Schema>, RunError> {
    static SCHEMA: OnceLock<Arc<Schema>> = OnceLock::new();
    if let Some(s) = SCHEMA.get() {
        return Ok(Arc::clone(s));
    }
    let raw = spans_raw_schema()?;
    let hex_cols = [spans::TRACE_ID, spans::SPAN_ID, spans::PARENT_SPAN_ID];
    let mut fields = Vec::with_capacity(56);
    for field in raw.fields() {
        let name = field.name.as_str();
        if name == spans::PROJECT_ID {
            continue;
        }
        let dt = if hex_cols.contains(&name) {
            DataType::Utf8
        } else {
            field.data_type.clone()
        };
        fields.push(Field::new(name, dt, field.nullable));
        if name == DURATION_NS {
            fields.push(Field::new(DURATION_MS, DataType::F64, field.nullable));
        }
    }
    let built = Schema::try_new_arc(fields)
        .map_err(|e| RunError::Internal(EngineError::from_column(&e)))?;
    let _ = SCHEMA.set(Arc::clone(&built));
    Ok(built)
}

/// View `traces` `cetra_column::Schema` (29 columns: skips `project_id`, `last_ingest_time`,
/// `partial_count`, hex IDs, adds `duration_ns` and `duration_ms`).
pub(crate) fn traces_view_schema() -> Result<Arc<Schema>, RunError> {
    static SCHEMA: OnceLock<Arc<Schema>> = OnceLock::new();
    if let Some(s) = SCHEMA.get() {
        return Ok(Arc::clone(s));
    }
    let raw = traces_raw_schema()?;
    let skip = [
        traces::PROJECT_ID,
        traces::LAST_INGEST_TIME,
        traces::PARTIAL_COUNT,
    ];
    let hex_cols = [traces::TRACE_ID, traces::ROOT_SPAN_ID];
    let mut fields = Vec::with_capacity(29);
    for field in raw.fields() {
        let name = field.name.as_str();
        if skip.contains(&name) {
            continue;
        }
        let dt = if hex_cols.contains(&name) {
            DataType::Utf8
        } else {
            field.data_type.clone()
        };
        fields.push(Field::new(name, dt, field.nullable));
        if name == traces::END_TIME {
            fields.push(Field::new(DURATION_NS, DataType::I64, false));
            fields.push(Field::new(DURATION_MS, DataType::F64, false));
        }
    }
    let built = Schema::try_new_arc(fields)
        .map_err(|e| RunError::Internal(EngineError::from_column(&e)))?;
    let _ = SCHEMA.set(Arc::clone(&built));
    Ok(built)
}

/// View `scores` `cetra_column::Schema` (23 columns: skips `project_id`, hex `trace_id`/`span_id`).
pub(crate) fn scores_view_schema() -> Result<Arc<Schema>, RunError> {
    static SCHEMA: OnceLock<Arc<Schema>> = OnceLock::new();
    if let Some(s) = SCHEMA.get() {
        return Ok(Arc::clone(s));
    }
    let raw = scores_raw_schema()?;
    let hex_cols = [scores::TRACE_ID, scores::SPAN_ID];
    let mut fields = Vec::with_capacity(23);
    for field in raw.fields() {
        let name = field.name.as_str();
        if name == scores::PROJECT_ID {
            continue;
        }
        let dt = if hex_cols.contains(&name) {
            DataType::Utf8
        } else {
            field.data_type.clone()
        };
        fields.push(Field::new(name, dt, field.nullable));
    }
    let built = Schema::try_new_arc(fields)
        .map_err(|e| RunError::Internal(EngineError::from_column(&e)))?;
    let _ = SCHEMA.set(Arc::clone(&built));
    Ok(built)
}

/// View `sessions` `cetra_column::Schema` (11 columns).
pub(crate) fn sessions_view_schema() -> Result<Arc<Schema>, RunError> {
    static SCHEMA: OnceLock<Arc<Schema>> = OnceLock::new();
    if let Some(s) = SCHEMA.get() {
        return Ok(Arc::clone(s));
    }
    let ts = DataType::timestamp_utc();
    let built = Schema::try_new_arc(vec![
        Field::new(traces::SESSION_ID, DataType::Utf8, false),
        Field::new(traces::START_TIME, ts.clone(), false),
        Field::new(traces::END_TIME, ts, false),
        Field::new(DURATION_NS, DataType::I64, false),
        Field::new(DURATION_MS, DataType::F64, false),
        Field::new("trace_count", DataType::I64, false),
        Field::new(traces::ERROR_COUNT, DataType::I64, false),
        Field::new(traces::TOTAL_TOKENS, DataType::I64, true),
        Field::new(traces::COST_TOTAL, DataType::F64, true),
        Field::new(traces::USER_ID, DataType::Utf8, true),
        Field::new(
            traces::TAGS,
            DataType::List(Box::new(DataType::Utf8)),
            false,
        ),
    ])
    .map_err(|e| RunError::Internal(EngineError::from_column(&e)))?;
    let _ = SCHEMA.set(Arc::clone(&built));
    Ok(built)
}

/// Returns the view schema for `view`.
pub(crate) fn view_schema(view: FilterView) -> Result<Arc<Schema>, RunError> {
    match view {
        FilterView::Traces => traces_view_schema(),
        FilterView::Spans => spans_view_schema(),
        FilterView::Sessions => sessions_view_schema(),
        FilterView::Scores => scores_view_schema(),
    }
}

fn raw_col_expr(schema: &Schema, name: &str) -> Result<Expr, QueryError> {
    let idx = schema
        .index_of(name)
        .ok_or_else(|| QueryError::internal(format!("missing column {name} in schema")))?;
    Ok(Expr::Column {
        index: idx,
        name: name.to_owned(),
        data_type: schema.fields()[idx].data_type.clone(),
    })
}

fn encode_hex_expr(inner: Expr) -> Expr {
    if inner.data_type() == DataType::Utf8 {
        return inner;
    }
    Expr::Call {
        func: ScalarFn::Encode,
        args: vec![inner, Expr::Literal(ScalarValue::Utf8("hex".to_owned()))],
        data_type: DataType::Utf8,
    }
}

pub(crate) fn duration_ns_from_bounds_expr(input_schema: &Schema) -> Result<Expr, QueryError> {
    let end_col = raw_col_expr(input_schema, traces::END_TIME)?;
    let start_col = raw_col_expr(input_schema, traces::START_TIME)?;
    Ok(Expr::Binary {
        left: Box::new(Expr::Cast {
            expr: Box::new(end_col),
            target: DataType::I64,
            mode: CastMode::Strict,
        }),
        op: BinaryOp::Sub,
        right: Box::new(Expr::Cast {
            expr: Box::new(start_col),
            target: DataType::I64,
            mode: CastMode::Strict,
        }),
        data_type: DataType::I64,
    })
}

pub(crate) fn duration_ms_from_ns_expr(duration_ns: Expr) -> Expr {
    Expr::Binary {
        left: Box::new(Expr::Cast {
            expr: Box::new(duration_ns),
            target: DataType::F64,
            mode: CastMode::Strict,
        }),
        op: BinaryOp::Div,
        right: Box::new(Expr::Literal(ScalarValue::F64(1_000_000.0))),
        data_type: DataType::F64,
    }
}

/// Builds a [`ProjectOp`] projecting all 56 `spans` view columns from `input_schema` (raw or view).
pub(crate) fn spans_view_projection(
    input_schema: &Schema,
    ctx: ExecContext,
) -> Result<ProjectOp, QueryError> {
    let out_schema = spans_view_schema().map_err(run_err_to_query)?;
    let names: Vec<&str> = out_schema
        .fields()
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    project_view_columns(input_schema, Arc::clone(&out_schema), &names, false, ctx)
}

/// Builds a [`ProjectOp`] projecting all 29 `traces` view columns from `input_schema` (raw or view).
pub(crate) fn traces_view_projection(
    input_schema: &Schema,
    ctx: ExecContext,
) -> Result<ProjectOp, QueryError> {
    let out_schema = traces_view_schema().map_err(run_err_to_query)?;
    let names: Vec<&str> = out_schema
        .fields()
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    project_view_columns(input_schema, Arc::clone(&out_schema), &names, true, ctx)
}

/// Builds a [`ProjectOp`] projecting all 23 `scores` view columns from `input_schema` (raw or view).
pub(crate) fn scores_view_projection(
    input_schema: &Schema,
    ctx: ExecContext,
) -> Result<ProjectOp, QueryError> {
    let out_schema = scores_view_schema().map_err(run_err_to_query)?;
    let names: Vec<&str> = out_schema
        .fields()
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    project_view_columns(input_schema, Arc::clone(&out_schema), &names, false, ctx)
}

/// Builds a [`ProjectOp`] projecting all 29 `traces` view columns plus a binary
/// `FixedBytes16` copy of `trace_id` under `raw_trace_id_col`.
pub(crate) fn traces_view_projection_with_raw_trace_id(
    input_schema: &Schema,
    raw_trace_id_col: &str,
    ctx: ExecContext,
) -> Result<(ProjectOp, Arc<Schema>), QueryError> {
    let view_schema = traces_view_schema().map_err(run_err_to_query)?;
    let names: Vec<&str> = view_schema
        .fields()
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    let mut exprs = view_column_exprs(input_schema, &names, true)?;
    exprs.push(raw_col_expr(input_schema, traces::TRACE_ID)?);
    let mut fields = view_schema.fields().to_vec();
    fields.push(Field::new(raw_trace_id_col, DataType::FixedBytes16, false));
    let out_schema = Schema::try_new_arc(fields)?;
    let op = ProjectOp::with_schema(exprs, Arc::clone(&out_schema), ctx)?;
    Ok((op, out_schema))
}

/// Builds a [`ProjectOp`] projecting named columns from `input_schema`, converting binary IDs to
/// lowercase hex and computing derived `duration_ns` / `duration_ms` when absent from `input_schema`.
pub(crate) fn project_view_columns(
    input_schema: &Schema,
    out_schema: Arc<Schema>,
    col_names: &[&str],
    compute_trace_duration: bool,
    ctx: ExecContext,
) -> Result<ProjectOp, QueryError> {
    let exprs = view_column_exprs(input_schema, col_names, compute_trace_duration)?;
    ProjectOp::with_schema(exprs, out_schema, ctx)
}

fn view_column_exprs(
    input_schema: &Schema,
    col_names: &[&str],
    compute_trace_duration: bool,
) -> Result<Vec<Expr>, QueryError> {
    let mut exprs = Vec::with_capacity(col_names.len());
    for &name in col_names {
        let expr = if name == DURATION_NS {
            if input_schema.index_of(DURATION_NS).is_some() {
                raw_col_expr(input_schema, DURATION_NS)?
            } else {
                duration_ns_from_bounds_expr(input_schema)?
            }
        } else if name == DURATION_MS {
            if input_schema.index_of(DURATION_MS).is_some() {
                raw_col_expr(input_schema, DURATION_MS)?
            } else {
                let ns_expr =
                    if !compute_trace_duration && input_schema.index_of(DURATION_NS).is_some() {
                        raw_col_expr(input_schema, DURATION_NS)?
                    } else {
                        duration_ns_from_bounds_expr(input_schema)?
                    };
                duration_ms_from_ns_expr(ns_expr)
            }
        } else {
            let col = raw_col_expr(input_schema, name)?;
            if matches!(
                col.data_type(),
                DataType::FixedBytes8 | DataType::FixedBytes16
            ) {
                encode_hex_expr(col)
            } else {
                col
            }
        };
        exprs.push(expr);
    }
    Ok(exprs)
}

/// Adapts a [`PartitionStream`] followed by a chain of [`PushOperator`]s back into a
/// [`PartitionStream`] while upholding the `Reservation` postcondition.
pub(crate) struct OperatorStream {
    input: Box<dyn PartitionStream>,
    ops: Vec<Box<dyn PushOperator>>,
    input_res: Reservation,
    buf_res: Reservation,
    pending: VecDeque<Batch>,
    finished_ops: bool,
}

impl OperatorStream {
    pub(crate) fn new(
        input: Box<dyn PartitionStream>,
        ops: Vec<Box<dyn PushOperator>>,
        ctx: &ExecContext,
    ) -> Self {
        Self {
            input,
            ops,
            input_res: ctx.pool.register("op_stream_in", false),
            buf_res: ctx.pool.register("op_stream_buf", false),
            pending: VecDeque::new(),
            finished_ops: false,
        }
    }

    fn push_pending(&mut self, batch: Batch) -> Result<(), QueryError> {
        let bytes = batch.memory_bytes();
        self.buf_res.try_grow(bytes)?;
        self.pending.push_back(batch);
        Ok(())
    }

    fn push_through_ops(
        ops: &mut [Box<dyn PushOperator>],
        batch: Batch,
        out: &mut Vec<Batch>,
    ) -> Result<ControlFlow, QueryError> {
        let Some((op, rest)) = ops.split_first_mut() else {
            out.push(batch);
            return Ok(ControlFlow::Continue);
        };
        let mut downstream = ControlFlow::Continue;
        let flow = op.push(batch, &mut |b| {
            let f = Self::push_through_ops(rest, b, out)?;
            if f == ControlFlow::Stop {
                downstream = ControlFlow::Stop;
            }
            Ok(f)
        })?;
        if flow == ControlFlow::Stop || downstream == ControlFlow::Stop {
            Ok(ControlFlow::Stop)
        } else {
            Ok(ControlFlow::Continue)
        }
    }

    fn finish_ops(
        ops: &mut [Box<dyn PushOperator>],
        out: &mut Vec<Batch>,
    ) -> Result<(), QueryError> {
        for i in 0..ops.len() {
            let (head, tail) = ops.split_at_mut(i + 1);
            let op = &mut head[i];
            let mut downstream = ControlFlow::Continue;
            op.finish(&mut |b| {
                let f = Self::push_through_ops(tail, b, out)?;
                if f == ControlFlow::Stop {
                    downstream = ControlFlow::Stop;
                }
                Ok(f)
            })?;
            if downstream == ControlFlow::Stop {
                break;
            }
        }
        Ok(())
    }
}

impl PartitionStream for OperatorStream {
    fn next_batch(
        &mut self,
        cancel: &CancelToken,
        res: Option<&mut Reservation>,
    ) -> Result<Option<Batch>, QueryError> {
        let before = res.as_ref().map_or(0, |r| r.size());
        loop {
            if cancel.is_cancelled() {
                self.buf_res.free();
                self.input_res.free();
                rollback_res(res, before);
                return Err(QueryError::Cancelled);
            }
            if let Some(batch) = self.pending.pop_front() {
                let bytes = batch.memory_bytes();
                self.buf_res.shrink(self.buf_res.size().min(bytes));
                reconcile_res(res, before, bytes)?;
                return Ok(Some(batch));
            }
            if self.finished_ops {
                self.buf_res.free();
                self.input_res.free();
                rollback_res(res, before);
                return Ok(None);
            }
            match self.input.next_batch(cancel, Some(&mut self.input_res)) {
                Ok(Some(batch)) => {
                    let mut emitted = Vec::new();
                    let flow = Self::push_through_ops(&mut self.ops, batch, &mut emitted);
                    self.input_res.free();
                    let flow = match flow {
                        Ok(f) => f,
                        Err(err) => {
                            self.buf_res.free();
                            rollback_res(res, before);
                            return Err(err);
                        }
                    };
                    for b in emitted {
                        if let Err(err) = self.push_pending(b) {
                            self.buf_res.free();
                            rollback_res(res, before);
                            return Err(err);
                        }
                    }
                    if flow == ControlFlow::Stop {
                        self.finished_ops = true;
                    }
                }
                Ok(None) => {
                    self.input_res.free();
                    let mut emitted = Vec::new();
                    if let Err(err) = Self::finish_ops(&mut self.ops, &mut emitted) {
                        self.buf_res.free();
                        rollback_res(res, before);
                        return Err(err);
                    }
                    for b in emitted {
                        if let Err(err) = self.push_pending(b) {
                            self.buf_res.free();
                            rollback_res(res, before);
                            return Err(err);
                        }
                    }
                    self.finished_ops = true;
                }
                Err(err) => {
                    self.buf_res.free();
                    self.input_res.free();
                    rollback_res(res, before);
                    return Err(err);
                }
            }
        }
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

/// Remaps an expression bound against a view schema to `raw_schema`, converting hex-encoded
/// binary ID comparisons (`encode(col, 'hex') = '...'`, `!=`, `IN`, `NOT IN`) into native
/// `FixedBytes16` / `FixedBytes8` comparisons so `ParquetSource` and `TracesSource` can prune.
pub(crate) fn rewrite_view_expr_for_raw(
    expr: &Expr,
    raw_schema: &Schema,
    hex_cols: &[&str],
    compute_trace_duration: bool,
) -> Result<Expr, QueryError> {
    let remapped = remap_to_raw(expr, raw_schema, hex_cols, compute_trace_duration)?;
    Ok(rewrite_hex_comparisons(remapped))
}

/// Remaps `Expr::Column` nodes in `expr` to column indices in `target_schema` by column name.
pub(crate) fn remap_expr_by_name(expr: &Expr, target_schema: &Schema) -> Result<Expr, QueryError> {
    match expr {
        Expr::Column { name, .. } => raw_col_expr(target_schema, name),
        Expr::Literal(sv) => Ok(Expr::Literal(sv.clone())),
        Expr::Unary {
            op,
            expr,
            data_type,
        } => Ok(Expr::Unary {
            op: *op,
            expr: Box::new(remap_expr_by_name(expr, target_schema)?),
            data_type: data_type.clone(),
        }),
        Expr::Binary {
            left,
            op,
            right,
            data_type,
        } => Ok(Expr::Binary {
            left: Box::new(remap_expr_by_name(left, target_schema)?),
            op: *op,
            right: Box::new(remap_expr_by_name(right, target_schema)?),
            data_type: data_type.clone(),
        }),
        Expr::Cast { expr, target, mode } => Ok(Expr::Cast {
            expr: Box::new(remap_expr_by_name(expr, target_schema)?),
            target: target.clone(),
            mode: *mode,
        }),
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Ok(Expr::Between {
            expr: Box::new(remap_expr_by_name(expr, target_schema)?),
            low: Box::new(remap_expr_by_name(low, target_schema)?),
            high: Box::new(remap_expr_by_name(high, target_schema)?),
            negated: *negated,
        }),
        Expr::InList {
            expr,
            list,
            negated,
            redacted,
        } => Ok(Expr::InList {
            expr: Box::new(remap_expr_by_name(expr, target_schema)?),
            list: list
                .iter()
                .map(|e| remap_expr_by_name(e, target_schema))
                .collect::<Result<Vec<_>, _>>()?,
            negated: *negated,
            redacted: *redacted,
        }),
        Expr::Like {
            expr,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => Ok(Expr::Like {
            expr: Box::new(remap_expr_by_name(expr, target_schema)?),
            pattern: Box::new(remap_expr_by_name(pattern, target_schema)?),
            escape: *escape,
            case_insensitive: *case_insensitive,
            negated: *negated,
        }),
        Expr::Case {
            operand,
            when_then,
            else_expr,
            data_type,
        } => Ok(Expr::Case {
            operand: operand
                .as_deref()
                .map(|e| remap_expr_by_name(e, target_schema))
                .transpose()?
                .map(Box::new),
            when_then: when_then
                .iter()
                .map(|(w, t)| {
                    Ok((
                        remap_expr_by_name(w, target_schema)?,
                        remap_expr_by_name(t, target_schema)?,
                    ))
                })
                .collect::<Result<Vec<_>, QueryError>>()?,
            else_expr: else_expr
                .as_deref()
                .map(|e| remap_expr_by_name(e, target_schema))
                .transpose()?
                .map(Box::new),
            data_type: data_type.clone(),
        }),
        Expr::MapIndex { map, key } => Ok(Expr::MapIndex {
            map: Box::new(remap_expr_by_name(map, target_schema)?),
            key: Box::new(remap_expr_by_name(key, target_schema)?),
        }),
        Expr::MakeArray { elems, item_type } => Ok(Expr::MakeArray {
            elems: elems
                .iter()
                .map(|e| remap_expr_by_name(e, target_schema))
                .collect::<Result<Vec<_>, _>>()?,
            item_type: item_type.clone(),
        }),
        Expr::Call {
            func,
            args,
            data_type,
        } => Ok(Expr::Call {
            func: *func,
            args: args
                .iter()
                .map(|e| remap_expr_by_name(e, target_schema))
                .collect::<Result<Vec<_>, _>>()?,
            data_type: data_type.clone(),
        }),
    }
}

fn remap_to_raw(
    expr: &Expr,
    raw_schema: &Schema,
    hex_cols: &[&str],
    compute_trace_duration: bool,
) -> Result<Expr, QueryError> {
    match expr {
        Expr::Column { name, .. } => {
            if hex_cols.contains(&name.as_str()) {
                Ok(encode_hex_expr(raw_col_expr(raw_schema, name)?))
            } else if compute_trace_duration && name == DURATION_NS {
                duration_ns_from_bounds_expr(raw_schema)
            } else if name == DURATION_MS {
                let ns_expr =
                    if !compute_trace_duration && raw_schema.index_of(DURATION_NS).is_some() {
                        raw_col_expr(raw_schema, DURATION_NS)?
                    } else {
                        duration_ns_from_bounds_expr(raw_schema)?
                    };
                Ok(duration_ms_from_ns_expr(ns_expr))
            } else {
                raw_col_expr(raw_schema, name)
            }
        }
        Expr::Literal(sv) => Ok(Expr::Literal(sv.clone())),
        Expr::Unary {
            op,
            expr,
            data_type,
        } => Ok(Expr::Unary {
            op: *op,
            expr: Box::new(remap_to_raw(
                expr,
                raw_schema,
                hex_cols,
                compute_trace_duration,
            )?),
            data_type: data_type.clone(),
        }),
        Expr::Binary {
            left,
            op,
            right,
            data_type,
        } => Ok(Expr::Binary {
            left: Box::new(remap_to_raw(
                left,
                raw_schema,
                hex_cols,
                compute_trace_duration,
            )?),
            op: *op,
            right: Box::new(remap_to_raw(
                right,
                raw_schema,
                hex_cols,
                compute_trace_duration,
            )?),
            data_type: data_type.clone(),
        }),
        Expr::Cast { expr, target, mode } => Ok(Expr::Cast {
            expr: Box::new(remap_to_raw(
                expr,
                raw_schema,
                hex_cols,
                compute_trace_duration,
            )?),
            target: target.clone(),
            mode: *mode,
        }),
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Ok(Expr::Between {
            expr: Box::new(remap_to_raw(
                expr,
                raw_schema,
                hex_cols,
                compute_trace_duration,
            )?),
            low: Box::new(remap_to_raw(
                low,
                raw_schema,
                hex_cols,
                compute_trace_duration,
            )?),
            high: Box::new(remap_to_raw(
                high,
                raw_schema,
                hex_cols,
                compute_trace_duration,
            )?),
            negated: *negated,
        }),
        Expr::InList {
            expr,
            list,
            negated,
            redacted,
        } => Ok(Expr::InList {
            expr: Box::new(remap_to_raw(
                expr,
                raw_schema,
                hex_cols,
                compute_trace_duration,
            )?),
            list: list
                .iter()
                .map(|e| remap_to_raw(e, raw_schema, hex_cols, compute_trace_duration))
                .collect::<Result<Vec<_>, _>>()?,
            negated: *negated,
            redacted: *redacted,
        }),
        Expr::Like {
            expr,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => Ok(Expr::Like {
            expr: Box::new(remap_to_raw(
                expr,
                raw_schema,
                hex_cols,
                compute_trace_duration,
            )?),
            pattern: Box::new(remap_to_raw(
                pattern,
                raw_schema,
                hex_cols,
                compute_trace_duration,
            )?),
            escape: *escape,
            case_insensitive: *case_insensitive,
            negated: *negated,
        }),
        Expr::Case {
            operand,
            when_then,
            else_expr,
            data_type,
        } => Ok(Expr::Case {
            operand: operand
                .as_deref()
                .map(|e| remap_to_raw(e, raw_schema, hex_cols, compute_trace_duration))
                .transpose()?
                .map(Box::new),
            when_then: when_then
                .iter()
                .map(|(w, t)| {
                    Ok((
                        remap_to_raw(w, raw_schema, hex_cols, compute_trace_duration)?,
                        remap_to_raw(t, raw_schema, hex_cols, compute_trace_duration)?,
                    ))
                })
                .collect::<Result<Vec<_>, QueryError>>()?,
            else_expr: else_expr
                .as_deref()
                .map(|e| remap_to_raw(e, raw_schema, hex_cols, compute_trace_duration))
                .transpose()?
                .map(Box::new),
            data_type: data_type.clone(),
        }),
        Expr::MapIndex { map, key } => Ok(Expr::MapIndex {
            map: Box::new(remap_to_raw(
                map,
                raw_schema,
                hex_cols,
                compute_trace_duration,
            )?),
            key: Box::new(remap_to_raw(
                key,
                raw_schema,
                hex_cols,
                compute_trace_duration,
            )?),
        }),
        Expr::MakeArray { elems, item_type } => Ok(Expr::MakeArray {
            elems: elems
                .iter()
                .map(|e| remap_to_raw(e, raw_schema, hex_cols, compute_trace_duration))
                .collect::<Result<Vec<_>, _>>()?,
            item_type: item_type.clone(),
        }),
        Expr::Call {
            func,
            args,
            data_type,
        } => Ok(Expr::Call {
            func: *func,
            args: args
                .iter()
                .map(|e| remap_to_raw(e, raw_schema, hex_cols, compute_trace_duration))
                .collect::<Result<Vec<_>, _>>()?,
            data_type: data_type.clone(),
        }),
    }
}

fn rewrite_hex_comparisons(expr: Expr) -> Expr {
    match expr {
        Expr::Unary {
            op,
            expr: inner,
            data_type,
        } => Expr::Unary {
            op,
            expr: Box::new(rewrite_hex_comparisons(*inner)),
            data_type,
        },
        Expr::Binary {
            left,
            op,
            right,
            data_type,
        } => {
            let left = rewrite_hex_comparisons(*left);
            let right = rewrite_hex_comparisons(*right);
            if matches!(op, BinaryOp::Eq | BinaryOp::NotEq)
                && let Some(rewritten) = try_rewrite_binary_hex(&left, op, &right)
            {
                return rewritten;
            }
            Expr::Binary {
                left: Box::new(left),
                op,
                right: Box::new(right),
                data_type,
            }
        }
        Expr::InList {
            expr: inner,
            list,
            negated,
            redacted,
        } => {
            let inner = rewrite_hex_comparisons(*inner);
            let list: Vec<Expr> = list.into_iter().map(rewrite_hex_comparisons).collect();
            if let Some(rewritten) = try_rewrite_in_list_hex(&inner, &list, negated, redacted) {
                return rewritten;
            }
            Expr::InList {
                expr: Box::new(inner),
                list,
                negated,
                redacted,
            }
        }
        Expr::Between {
            expr,
            low,
            high,
            negated,
        } => Expr::Between {
            expr: Box::new(rewrite_hex_comparisons(*expr)),
            low: Box::new(rewrite_hex_comparisons(*low)),
            high: Box::new(rewrite_hex_comparisons(*high)),
            negated,
        },
        Expr::Cast { expr, target, mode } => Expr::Cast {
            expr: Box::new(rewrite_hex_comparisons(*expr)),
            target,
            mode,
        },
        Expr::Like {
            expr,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => Expr::Like {
            expr: Box::new(rewrite_hex_comparisons(*expr)),
            pattern: Box::new(rewrite_hex_comparisons(*pattern)),
            escape,
            case_insensitive,
            negated,
        },
        Expr::Case {
            operand,
            when_then,
            else_expr,
            data_type,
        } => Expr::Case {
            operand: operand.map(|e| Box::new(rewrite_hex_comparisons(*e))),
            when_then: when_then
                .into_iter()
                .map(|(w, t)| (rewrite_hex_comparisons(w), rewrite_hex_comparisons(t)))
                .collect(),
            else_expr: else_expr.map(|e| Box::new(rewrite_hex_comparisons(*e))),
            data_type,
        },
        Expr::MapIndex { map, key } => Expr::MapIndex {
            map: Box::new(rewrite_hex_comparisons(*map)),
            key: Box::new(rewrite_hex_comparisons(*key)),
        },
        Expr::MakeArray { elems, item_type } => Expr::MakeArray {
            elems: elems.into_iter().map(rewrite_hex_comparisons).collect(),
            item_type,
        },
        Expr::Call {
            func,
            args,
            data_type,
        } => Expr::Call {
            func,
            args: args.into_iter().map(rewrite_hex_comparisons).collect(),
            data_type,
        },
        other => other,
    }
}

fn encoded_fixed_col(expr: &Expr) -> Option<(&Expr, usize)> {
    let Expr::Call {
        func: ScalarFn::Encode,
        args,
        ..
    } = expr
    else {
        return None;
    };
    let [inner, fmt_arg] = args.as_slice() else {
        return None;
    };
    let Expr::Literal(ScalarValue::Utf8(fmt)) = fmt_arg else {
        return None;
    };
    if !fmt.eq_ignore_ascii_case("hex") {
        return None;
    }
    let width = match inner.data_type() {
        DataType::FixedBytes16 => 16,
        DataType::FixedBytes8 => 8,
        _ => return None,
    };
    Some((inner, width))
}

fn hex_literal_expr(expr: &Expr, width: usize) -> Option<Expr> {
    let Expr::Literal(ScalarValue::Utf8(text)) = expr else {
        return None;
    };
    if text.len() != width * 2 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let decoded = hex::decode(text).ok()?;
    let sv = match width {
        16 => ScalarValue::FixedBytes16(decoded.try_into().ok()?),
        8 => ScalarValue::FixedBytes8(decoded.try_into().ok()?),
        _ => return None,
    };
    Some(Expr::Literal(sv))
}

fn try_rewrite_binary_hex(left: &Expr, op: BinaryOp, right: &Expr) -> Option<Expr> {
    match (encoded_fixed_col(left), encoded_fixed_col(right)) {
        (Some((inner, width)), None) => Some(Expr::Binary {
            left: Box::new(inner.clone()),
            op,
            right: Box::new(hex_literal_expr(right, width)?),
            data_type: DataType::Bool,
        }),
        (None, Some((inner, width))) => Some(Expr::Binary {
            left: Box::new(hex_literal_expr(left, width)?),
            op,
            right: Box::new(inner.clone()),
            data_type: DataType::Bool,
        }),
        (Some((l_inner, lw)), Some((r_inner, rw))) if lw == rw => Some(Expr::Binary {
            left: Box::new(l_inner.clone()),
            op,
            right: Box::new(r_inner.clone()),
            data_type: DataType::Bool,
        }),
        _ => None,
    }
}

fn try_rewrite_in_list_hex(
    expr: &Expr,
    list: &[Expr],
    negated: bool,
    redacted: bool,
) -> Option<Expr> {
    let (inner, width) = encoded_fixed_col(expr)?;
    if list.is_empty() {
        return None;
    }
    let mut literals = Vec::with_capacity(list.len());
    for item in list {
        literals.push(hex_literal_expr(item, width)?);
    }
    Some(Expr::InList {
        expr: Box::new(inner.clone()),
        list: literals,
        negated,
        redacted,
    })
}

/// Inspects top-level `AND` conjuncts of `spec.filters` for `FixedBytes16` / `FixedBytes8`
/// equality and `IN (...)` predicates and populates `spec.id_lookups` for bloom/sort-key pruning.
pub(crate) fn populate_id_lookups(spec: &mut ScanSpec) {
    let mut fixed16: BTreeMap<String, BTreeSet<[u8; 16]>> = BTreeMap::new();
    let mut fixed8: BTreeMap<String, BTreeSet<[u8; 8]>> = BTreeMap::new();

    for hint in &spec.id_lookups {
        match hint {
            IdLookupHint::Fixed16 { column, values } => {
                let set: BTreeSet<[u8; 16]> = values.iter().copied().collect();
                intersect_set(&mut fixed16, column, set);
            }
            IdLookupHint::Fixed8 { column, values } => {
                let set: BTreeSet<[u8; 8]> = values.iter().copied().collect();
                intersect_set(&mut fixed8, column, set);
            }
            IdLookupHint::Utf8 { .. } => {}
        }
    }

    for filter in &spec.filters {
        collect_id_conjuncts(filter, &mut fixed16, &mut fixed8);
    }

    spec.id_lookups
        .retain(|h| matches!(h, IdLookupHint::Utf8 { .. }));
    for (column, values) in fixed16 {
        spec.id_lookups.push(IdLookupHint::Fixed16 {
            column,
            values: values.into_iter().collect(),
        });
    }
    for (column, values) in fixed8 {
        spec.id_lookups.push(IdLookupHint::Fixed8 {
            column,
            values: values.into_iter().collect(),
        });
    }
}

fn intersect_set<T: Ord>(map: &mut BTreeMap<String, BTreeSet<T>>, column: &str, set: BTreeSet<T>) {
    match map.get_mut(column) {
        Some(existing) => {
            existing.retain(|v| set.contains(v));
        }
        None => {
            map.insert(column.to_owned(), set);
        }
    }
}

fn collect_id_conjuncts(
    expr: &Expr,
    fixed16: &mut BTreeMap<String, BTreeSet<[u8; 16]>>,
    fixed8: &mut BTreeMap<String, BTreeSet<[u8; 8]>>,
) {
    match expr {
        Expr::Binary {
            left,
            op: BinaryOp::And,
            right,
            ..
        } => {
            collect_id_conjuncts(left, fixed16, fixed8);
            collect_id_conjuncts(right, fixed16, fixed8);
        }
        Expr::Binary {
            left,
            op: BinaryOp::Eq,
            right,
            ..
        } => match (left.as_ref(), right.as_ref()) {
            (Expr::Column { name, .. }, Expr::Literal(ScalarValue::FixedBytes16(b)))
            | (Expr::Literal(ScalarValue::FixedBytes16(b)), Expr::Column { name, .. }) => {
                intersect_set(fixed16, name, BTreeSet::from([*b]));
            }
            (Expr::Column { name, .. }, Expr::Literal(ScalarValue::FixedBytes8(b)))
            | (Expr::Literal(ScalarValue::FixedBytes8(b)), Expr::Column { name, .. }) => {
                intersect_set(fixed8, name, BTreeSet::from([*b]));
            }
            _ => {}
        },
        Expr::InList {
            expr: inner,
            list,
            negated: false,
            ..
        } => {
            let Expr::Column {
                name, data_type, ..
            } = inner.as_ref()
            else {
                return;
            };
            match data_type {
                DataType::FixedBytes16 => {
                    let mut set = BTreeSet::new();
                    for item in list {
                        let Expr::Literal(ScalarValue::FixedBytes16(b)) = item else {
                            return;
                        };
                        set.insert(*b);
                    }
                    intersect_set(fixed16, name, set);
                }
                DataType::FixedBytes8 => {
                    let mut set = BTreeSet::new();
                    for item in list {
                        let Expr::Literal(ScalarValue::FixedBytes8(b)) = item else {
                            return;
                        };
                        set.insert(*b);
                    }
                    intersect_set(fixed8, name, set);
                }
                _ => {}
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use bytesize::ByteSize;
    use cetra_column::{Batch, GlobalPool, Reservation};
    use cetra_query::compile_filter;
    use cetra_query::exec::{PartitionStream, ScanSpec};

    use super::*;
    use crate::config::{
        FlushConfig, MemtableConfig, QueryConfig, S3Config, StorageConfig, WalConfig,
    };

    fn test_storage_config(dir: &std::path::Path) -> StorageConfig {
        StorageConfig {
            delete_objects: true,
            data_dir: dir.to_path_buf(),
            min_free_bytes: ByteSize::b(0),
            object_store: None,
            s3: S3Config::default(),
            wal: WalConfig::default(),
            flush: FlushConfig::default(),
            memtable: MemtableConfig::default(),
            query: QueryConfig {
                target_partitions: 2,
                ..QueryConfig::default()
            },
            anchor: Default::default(),
            gc: Default::default(),
            compaction: Default::default(),
            deletion: Default::default(),
        }
    }

    #[test]
    fn view_schemas_match_expected_column_counts() {
        assert_eq!(spans_view_schema().expect("spans").len(), 56);
        assert_eq!(traces_view_schema().expect("traces").len(), 29);
        assert_eq!(scores_view_schema().expect("scores").len(), 23);
        assert_eq!(sessions_view_schema().expect("sessions").len(), 11);
    }

    #[test]
    fn binary_hex_rewrite_populates_id_lookups() {
        let trace_hex = "a1000000000000000000000000000001";
        let span_hex = "b100000000000001";
        let spans_view = spans_view_schema().expect("spans view");
        let compiled = compile_filter(
            FilterView::Spans,
            &spans_view,
            &format!("trace_id = '{trace_hex}' AND span_id IN ('{span_hex}')"),
        )
        .expect("compile");
        let raw = spans_raw_schema().expect("spans raw");
        let rewritten = rewrite_view_expr_for_raw(
            &compiled.expr,
            &raw,
            &[spans::TRACE_ID, spans::SPAN_ID, spans::PARENT_SPAN_ID],
            false,
        )
        .expect("rewrite");

        let mut spec = ScanSpec {
            filters: vec![rewritten],
            ..ScanSpec::default()
        };
        populate_id_lookups(&mut spec);
        assert_eq!(spec.id_lookups.len(), 2);
        let expected_trace: [u8; 16] = hex::decode(trace_hex).expect("hex").try_into().expect("16");
        let expected_span: [u8; 8] = hex::decode(span_hex).expect("hex").try_into().expect("8");
        assert!(spec.id_lookups.contains(&IdLookupHint::Fixed16 {
            column: spans::TRACE_ID.to_owned(),
            values: vec![expected_trace],
        }));
        assert!(spec.id_lookups.contains(&IdLookupHint::Fixed8 {
            column: spans::SPAN_ID.to_owned(),
            values: vec![expected_span],
        }));
    }

    #[test]
    fn query_error_mapping_matches_contract() {
        assert!(matches!(
            query_err_to_run(QueryError::TooLarge),
            RunError::TooLarge
        ));
        assert!(matches!(
            query_err_to_run(QueryError::Cancelled),
            RunError::Timeout
        ));
        assert!(matches!(
            query_err_to_run(QueryError::InvalidRequest("bad".to_owned())),
            RunError::InvalidRequest(ref m) if m == "bad"
        ));
        assert!(matches!(
            query_err_to_run(QueryError::Failed("fail".to_owned())),
            RunError::Failed(ref m) if m == "fail"
        ));
        assert!(matches!(
            query_err_to_run(QueryError::Busy),
            RunError::Internal(_)
        ));
        assert!(matches!(
            query_err_to_run(QueryError::Internal("boom".to_owned())),
            RunError::Internal(_)
        ));
        match query_err_to_run(QueryError::Invalid(cetra_query::InvalidQuery {
            message: "bad syntax".to_owned(),
            position: Some(cetra_query::Position { line: 1, column: 4 }),
        })) {
            RunError::Invalid(inv) => {
                assert_eq!(inv.message, "bad syntax");
                assert_eq!(inv.position, Some(Position { line: 1, column: 4 }));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[derive(Debug)]
    struct SlowBlockingSource {
        schema: Arc<Schema>,
        started: Arc<AtomicBool>,
        finished: Arc<AtomicBool>,
    }

    impl ScanSource for SlowBlockingSource {
        fn schema(&self) -> Arc<Schema> {
            Arc::clone(&self.schema)
        }
        fn projected_schema(&self, _spec: &ScanSpec) -> Result<Arc<Schema>, QueryError> {
            Ok(Arc::clone(&self.schema))
        }
        fn open_partitions(
            &self,
            _spec: &ScanSpec,
            _ctx: &ExecContext,
        ) -> Result<Vec<Box<dyn PartitionStream>>, QueryError> {
            Ok(vec![Box::new(SlowStream {
                started: Arc::clone(&self.started),
                finished: Arc::clone(&self.finished),
            })])
        }
        fn explain_summary(&self, _spec: &ScanSpec) -> String {
            "SlowBlockingSource".to_owned()
        }
    }

    struct SlowStream {
        started: Arc<AtomicBool>,
        finished: Arc<AtomicBool>,
    }

    impl PartitionStream for SlowStream {
        fn next_batch(
            &mut self,
            cancel: &CancelToken,
            res: Option<&mut Reservation>,
        ) -> Result<Option<Batch>, QueryError> {
            self.started.store(true, Ordering::Release);
            if let Some(r) = res {
                r.try_grow(1024)?;
            }
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            // Hold reservation briefly after cancel is seen to verify `timeout` waits for full drain.
            std::thread::sleep(Duration::from_millis(25));
            self.finished.store(true, Ordering::Release);
            Err(QueryError::Cancelled)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_cancels_and_drains_worker_pool_and_scoped_pool_before_returning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(test_storage_config(dir.path()))
            .await
            .expect("open storage");

        let pool = storage
            .qe_runtime
            .admission
            .admit(1024, 1024 * 1024, Duration::from_secs(1))
            .await
            .expect("admit");
        let mut ctx = EngineContext::from_storage(
            &storage,
            "018f6b4e-7c1a-7c1a-8c1a-000000000001"
                .parse()
                .expect("project"),
            &QueryOptions::new(None, None),
        )
        .expect("ctx");
        let started = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        ctx.spans = Arc::new(SlowBlockingSource {
            schema: Schema::try_new_arc(vec![Field::new("x", DataType::I64, false)]).expect("s"),
            started: Arc::clone(&started),
            finished: Arc::clone(&finished),
        });
        ctx.pool = Some(Arc::clone(&pool));

        let err = ctx
            .collect_batches(
                Arc::clone(&ctx.spans),
                ScanSpec::default(),
                Duration::from_millis(30),
            )
            .await
            .expect_err("must timeout");
        assert!(matches!(err, RunError::Timeout));
        assert!(started.load(Ordering::Acquire));
        assert!(
            finished.load(Ordering::Acquire),
            "timeout must drain WorkerPool::run_scoped before returning"
        );
        assert_eq!(
            pool.reserved(),
            0,
            "ScopedPool reservations must be drained"
        );
        drop(ctx);
        drop(pool);
        assert_eq!(storage.qe_runtime.admission.used(), 0);
        let _ = storage.shutdown().await;
    }

    #[tokio::test]
    async fn qe_runtime_debug_is_privacy_safe_and_workers_lazy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Storage::open(test_storage_config(dir.path()))
            .await
            .expect("open storage");
        let dbg_before = format!("{:?}", storage.qe_runtime);
        assert!(dbg_before.contains("workers_initialized: false"));
        assert!(!dbg_before.contains(&dir.path().display().to_string()));

        let w1 = storage.qe_runtime.workers().expect("workers");
        let w2 = storage.qe_runtime.workers().expect("workers");
        assert!(Arc::ptr_eq(&w1, &w2));
        let dbg_after = format!("{:?}", storage.qe_runtime);
        assert!(dbg_after.contains("workers_initialized: true"));

        let ctx = EngineContext::from_storage(
            &storage,
            "018f6b4e-7c1a-7c1a-8c1a-000000000001"
                .parse()
                .expect("project"),
            &QueryOptions::new(Some(12345), Some(999)),
        )
        .expect("ctx");
        let ctx_dbg = format!("{ctx:?}");
        assert!(ctx_dbg.contains("EngineContext"));
        assert!(ctx_dbg.contains("visible_from: Some(999)"));
        assert!(ctx_dbg.contains("around: Some(12345)"));
        assert_eq!(ctx.around, Some(12345));
        assert_eq!(ctx.visible_from, Some(999));

        // Standalone GlobalPool smoke check to verify Pool usage.
        let _ = GlobalPool::standalone(1024);
        let _ = storage.shutdown().await;
    }

    #[tokio::test]
    async fn admit_from_storage_surfaces_busy_and_fallback_distinguishes_timeout_vs_busy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cfg = test_storage_config(dir.path());
        cfg.query.admission_wait = Duration::from_millis(40);
        let storage = Storage::open(cfg).await.expect("open storage");
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001"
            .parse()
            .expect("project");

        // Build a saturated context with a tiny pool and short admission wait.
        let cap = 2 * 1024 * 1024;
        let admission = Arc::new(crate::engine::admission::AsyncAdmissionPool::new(cap));
        let held = admission
            .admit(cap, cap, Duration::from_secs(1))
            .await
            .expect("hold entire pool");

        let mut base =
            EngineContext::from_storage(&storage, project, &QueryOptions::new(None, None))
                .expect("ctx");
        base.runtime = Arc::new(QeRuntime {
            admission: Arc::clone(&admission),
            workers: Default::default(),
            workers_init: Default::default(),
            metadata_cache: Arc::clone(&base.runtime.metadata_cache),
            spill: Arc::clone(&base.runtime.spill),
            target_partitions: 1,
            query_memory: crate::QueryMemory {
                initial: cap,
                cap,
                wait: Duration::from_millis(40),
            },
        });

        // 1. Caller timeout <= admission_wait -> RunError::Timeout (not Busy).
        let err_timeout = base
            .run_with_ctx(Duration::from_millis(15), |_| Ok(()))
            .await
            .expect_err("caller timeout shorter than admission_wait");
        assert!(
            matches!(err_timeout, RunError::Timeout),
            "expected Timeout, got {err_timeout:?}"
        );

        // 2. Caller timeout > admission_wait -> Busy (not Timeout).
        let err_busy = base
            .run_with_ctx(Duration::from_millis(200), |_| Ok(()))
            .await
            .expect_err("admission_wait expired before caller timeout");
        assert!(
            matches!(err_busy, RunError::Internal(_)),
            "expected Internal(Busy), got {err_busy:?}"
        );

        // 3. `admit_from_storage` when pool is saturated returns `QueryError::Busy` (HTTP 503).
        let storage_pool_limit = storage.qe_runtime.admission.limit();
        let storage_held = storage
            .qe_runtime
            .admission
            .admit(
                storage_pool_limit,
                storage_pool_limit,
                Duration::from_secs(1),
            )
            .await
            .expect("saturate storage pool");
        let err =
            EngineContext::admit_from_storage(&storage, project, &QueryOptions::new(None, None))
                .await
                .expect_err("saturated storage pool must fail with Busy");
        assert!(
            matches!(err, crate::error::QueryError::Busy),
            "expected crate::error::QueryError::Busy, got {err:?}"
        );
        drop(held);
        drop(storage_held);

        // Verify admitted context succeeds when pool has room.
        let admitted = storage
            .query(project, QueryOptions::new(None, None))
            .await
            .expect("admit succeeds");
        assert!(admitted.inner.pool.is_some());
        drop(admitted);

        let _ = storage.shutdown().await;
    }
}
