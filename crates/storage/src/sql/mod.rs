//! SQL console execution: validate, plan, bind, run and encode.
//!
//! `{name:Type}` placeholders are bound before planning. [`QueryContext::sql_json`]
//! runs the plan under a timeout and a row cap. [`QueryContext::sql_export`] streams
//! the same plan as CSV, NDJSON or Parquet.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use cetra_column::{CancelToken, Schema, ScopedPool};
pub use cetra_query::encode::{
    EXPORT_MAX_CHUNK_BYTES, EXPORT_MAX_ROWS, SQL_MAX_RESULT_BYTES, SQL_MAX_ROWS,
};
use cetra_query::encode::{EXPORT_PIECE_BYTES, encode_csv_header};
use cetra_query::exec::{
    BatchSource, DEFAULT_BATCH_SIZE, ExecContext, ExecMetrics, PartitionStream, ScanSource,
    SpillManager, WorkerPool,
};
use cetra_query::sql::run::prepare_sql_plan_with_limit;
use cetra_query::sql::{
    LogicalPlan, SqlCatalog, SqlSources, TimeWindow, encode_csv_batch_pieces,
    encode_ndjson_batch_pieces, execute_plan_json, sql_check as query_sql_check,
    sql_export_plan_stream,
};
use cetra_query::{EvalContext, QueryError};
use futures::stream::{BoxStream, StreamExt};
use serde_json::{Map, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use crate::engine::admission::AsyncAdmissionPool;
use crate::engine::parquet_export::{
    PARQUET_CHUNK_BYTES, PARQUET_ROW_GROUP_ROWS, ParquetBatchExporter,
};
use crate::engine::scores_source::ScoresSource;
use crate::engine::traces_source::TracesSource;
use crate::error::EngineError;
use crate::query_error::{InvalidQuery, Position, RunError};
use crate::verify::Suspects;
use crate::views::QueryContext;

const EXPORT_CHANNEL_CAPACITY: usize = 4;
const DEFAULT_ADMISSION_WAIT: Duration = Duration::from_secs(5);
const DEFAULT_TARGET_PARTITIONS: usize = 2;

/// One console request.
///
/// `limit` is the JSON row cap. [`QueryContext::sql_export`] ignores it; the
/// server passes [`SQL_MAX_ROWS`] there.
#[derive(Debug)]
pub struct SqlRequest<'a> {
    /// The statement, with `{name:Type}` placeholders.
    pub query: &'a str,
    /// JSON values for the placeholders.
    pub params: &'a Map<String, Value>,
    /// Maximum rows to return. `1..=SQL_MAX_ROWS`.
    pub limit: usize,
    /// How long the query may run. Zero is rejected here; the server checks the
    /// spec 09 range.
    pub timeout: Duration,
}

/// One output column. `data_type` is the display type name (`D-QE-15`).
#[derive(Debug)]
pub struct SqlColumn {
    /// Output name.
    pub name: String,
    /// Display type name.
    pub data_type: String,
}

/// JSON result (D98).
#[derive(Debug)]
pub struct SqlJson {
    /// Output columns, in order.
    pub columns: Vec<SqlColumn>,
    /// One object per row. Keys follow [`Self::columns`].
    pub rows: Vec<Map<String, Value>>,
    /// True when more rows matched than `rows` holds. That is because of `limit`,
    /// or because the next row would pass the byte cap.
    pub truncated: bool,
}

/// Byte format of [`QueryContext::sql_export`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// RFC 4180 CSV. The first chunk is the header line.
    Csv,
    /// One JSON object per line.
    Ndjson,
    /// ZSTD Parquet. Chunks are completed row groups, and the footer is last.
    Parquet,
}

impl QueryContext {
    /// Runs `request.query` and encodes at most `request.limit` rows.
    ///
    /// # Errors
    ///
    /// [`RunError::InvalidRequest`] for a limit outside `1..=SQL_MAX_ROWS` or a
    /// zero timeout. Planning and parameter errors pass through. Execution
    /// errors are classified. The time limit yields [`RunError::Timeout`].
    /// [`RunError::TooLarge`] when the first row alone passes the byte cap, or
    /// when the encoded output passes it anyway.
    pub async fn sql_json(&self, request: SqlRequest<'_>) -> Result<SqlJson, RunError> {
        self.sql_json_within(request, SQL_MAX_RESULT_BYTES).await
    }

    /// Plans `query` with `params` bound, without running it, and returns its output columns.
    pub async fn sql_check(
        &self,
        query: &str,
        params: &Map<String, Value>,
    ) -> Result<Vec<SqlColumn>, RunError> {
        self.sql_engine()?.sql_check(query, params).await
    }

    /// Runs `request.query` with an explicit JSON byte budget `max_bytes`.
    #[doc(hidden)]
    pub async fn sql_json_within(
        &self,
        request: SqlRequest<'_>,
        max_bytes: usize,
    ) -> Result<SqlJson, RunError> {
        self.sql_engine()?.sql_json_within(request, max_bytes).await
    }

    /// Streams `request.query` in `format` up to [`EXPORT_MAX_ROWS`] data rows.
    pub async fn sql_export(
        &self,
        request: SqlRequest<'_>,
        format: ExportFormat,
    ) -> Result<BoxStream<'static, Result<Bytes, RunError>>, RunError> {
        self.sql_engine()?.sql_export(request, format).await
    }
}

/// Self-contained Shape B SQL execution context over storage sources and resource pools.
///
/// Manual [`fmt::Debug`] omits row payloads and `extra_tables` contents (`AGENTS.md` § Privacy).
#[derive(Clone)]
pub(crate) struct SqlEngineContext {
    spans: Arc<dyn ScanSource>,
    traces: Arc<TracesSource>,
    scores: Arc<ScoresSource>,
    admission: Arc<AsyncAdmissionPool>,
    workers: Arc<WorkerPool>,
    spill: Arc<SpillManager>,
    visible_from: Option<i64>,
    window: Option<TimeWindow>,
    extra_tables: HashMap<String, Arc<BatchSource>>,
    pool: Option<Arc<ScopedPool>>,
    suspects: Option<Suspects>,
    coordinator_slots: Arc<Semaphore>,
    target_partitions: usize,
    initial_memory: usize,
    max_memory: usize,
    admission_wait: Duration,
    batch_size: usize,
}

impl fmt::Debug for SqlEngineContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut extra_names: Vec<&str> = self.extra_tables.keys().map(String::as_str).collect();
        extra_names.sort_unstable();
        f.debug_struct("SqlEngineContext")
            .field("visible_from", &self.visible_from)
            .field("window", &self.window)
            .field("extra_tables", &extra_names)
            .field("has_pool", &self.pool.is_some())
            .field(
                "coordinator_slots_available",
                &self.coordinator_slots.available_permits(),
            )
            .field("target_partitions", &self.target_partitions)
            .field("initial_memory", &self.initial_memory)
            .field("max_memory", &self.max_memory)
            .field("admission_used", &self.admission.used())
            .field("admission_limit", &self.admission.limit())
            .finish_non_exhaustive()
    }
}

impl SqlEngineContext {
    /// Creates a [`SqlEngineContext`] over storage sources and resource pools.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        spans: Arc<dyn ScanSource>,
        traces: Arc<TracesSource>,
        scores: Arc<ScoresSource>,
        admission: Arc<AsyncAdmissionPool>,
        workers: Arc<WorkerPool>,
        spill: Arc<SpillManager>,
        visible_from: Option<i64>,
        window: Option<TimeWindow>,
        extra_tables: HashMap<String, Arc<BatchSource>>,
    ) -> Self {
        let max_memory = admission.limit();
        let target_partitions = DEFAULT_TARGET_PARTITIONS.min(workers.num_workers().max(1));
        let max_coordinators = workers.num_workers().saturating_mul(4).max(32);
        Self {
            spans,
            traces,
            scores,
            admission,
            workers,
            spill,
            visible_from,
            window,
            extra_tables,
            pool: None,
            suspects: None,
            coordinator_slots: Arc::new(Semaphore::new(max_coordinators)),
            target_partitions,
            initial_memory: 0,
            max_memory,
            admission_wait: DEFAULT_ADMISSION_WAIT,
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }

    /// Reuses an already-admitted [`ScopedPool`] (for example from [`crate::ProjectQuery`]).
    pub(crate) fn with_pool(mut self, pool: Option<Arc<ScopedPool>>) -> Self {
        self.pool = pool;
        self
    }

    /// Attaches a [`Suspects`] tracker so corrupt Parquet failures trigger verification.
    pub(crate) fn with_suspects(mut self, suspects: Suspects) -> Self {
        self.suspects = Some(suspects);
        self
    }

    /// Shares the process-wide coordinator slot semaphore from [`crate::QeRuntime`].
    pub(crate) fn with_coordinator_slots(mut self, slots: Arc<Semaphore>) -> Self {
        self.coordinator_slots = slots;
        self
    }

    /// Sets admission memory limits `(initial, cap, wait)` for SQL queries without a pre-admitted pool.
    #[cfg(test)]
    pub(crate) fn with_memory_limits(
        mut self,
        initial_memory: usize,
        max_memory: usize,
        admission_wait: Duration,
    ) -> Self {
        self.initial_memory = initial_memory;
        self.max_memory = max_memory;
        self.admission_wait = admission_wait;
        self
    }

    /// Sets `target_partitions` for pipeline scans and execution.
    pub(crate) fn with_target_partitions(mut self, target_partitions: usize) -> Self {
        self.target_partitions = target_partitions.max(1);
        self
    }

    /// Registers an in-memory [`BatchSource`] view (e.g. `dataset_items`, `experiment_items`).
    #[cfg(test)]
    pub(crate) fn register_extra_table(
        &mut self,
        name: impl Into<String>,
        source: Arc<BatchSource>,
    ) -> Result<(), RunError> {
        let key = name.into().to_ascii_lowercase();
        if matches!(key.as_str(), "spans" | "traces" | "scores" | "sessions")
            || self.extra_tables.contains_key(&key)
        {
            return Err(RunError::InvalidRequest(format!(
                "{key} is already registered"
            )));
        }
        self.extra_tables.insert(key, source);
        Ok(())
    }

    fn build_catalog_and_sources(&self) -> Result<(SqlCatalog, SqlSources), RunError> {
        let mut catalog = SqlCatalog::new(self.window, self.visible_from);
        let mut sources = SqlSources::new();
        sources.register("spans", Arc::clone(&self.spans));
        sources.register("traces", Arc::clone(&self.traces) as Arc<dyn ScanSource>);
        sources.register("scores", Arc::clone(&self.scores) as Arc<dyn ScanSource>);

        for (name, batch_src) in &self.extra_tables {
            catalog
                .register_batch_source(name, Arc::clone(batch_src))
                .map_err(map_query_error)?;
            sources.register(name, Arc::clone(batch_src) as Arc<dyn ScanSource>);
        }
        Ok((catalog, sources))
    }

    /// Validates, binds `{name:Type}` parameters, and plans `query` without executing it.
    ///
    /// Completes synchronously (dropping any `sqlparser` `Rc` state before any `.await`).
    pub(crate) async fn sql_check(
        &self,
        query: &str,
        params: &Map<String, Value>,
    ) -> Result<Vec<SqlColumn>, RunError> {
        let (catalog, _) = self.build_catalog_and_sources()?;
        let cols = query_sql_check(&catalog, query, params).map_err(map_query_error)?;
        Ok(cols
            .into_iter()
            .map(|c| SqlColumn {
                data_type: c.type_name().to_owned(),
                name: c.name,
            })
            .collect())
    }

    /// Executes `request.query` and encodes at most `request.limit` rows within 32 MiB JSON.
    #[cfg(test)]
    pub(crate) async fn sql_json(&self, request: SqlRequest<'_>) -> Result<SqlJson, RunError> {
        self.sql_json_within(request, SQL_MAX_RESULT_BYTES).await
    }

    /// Executes `request.query` with an explicit JSON byte budget `max_bytes`.
    pub(crate) async fn sql_json_within(
        &self,
        request: SqlRequest<'_>,
        max_bytes: usize,
    ) -> Result<SqlJson, RunError> {
        if !(1..=SQL_MAX_ROWS).contains(&request.limit) {
            return Err(RunError::InvalidRequest(
                "limit must be between 1 and 10000".to_owned(),
            ));
        }
        if request.timeout.is_zero() {
            return Err(RunError::InvalidRequest(
                "timeout must be greater than zero".to_owned(),
            ));
        }

        let deadline = tokio::time::Instant::now() + request.timeout;
        // Synchronous parse + validate + param bind + optimize BEFORE any `.await`.
        let limit = request.limit;
        let (catalog, sources) = self.build_catalog_and_sources()?;
        let plan =
            prepare_sql_plan_with_limit(&catalog, request.query, request.params, Some(limit + 1))
                .map_err(map_query_error)?;

        let scoped_pool = self.resolve_pool_until(deadline).await?;
        let permit = self.acquire_coordinator_until(deadline).await?;

        let cancel = CancelToken::new();
        let _drop_guard = CancelOnDrop::new(cancel.clone());
        let exec_ctx = self.make_exec_ctx(scoped_pool, cancel.clone());

        let (done_tx, mut done_rx) = oneshot::channel();
        let join = std::thread::Builder::new()
            .name("cetra-sql-json".to_owned())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let _permit = permit;
                let res = execute_plan_json(&plan, &sources, &exec_ctx, limit, max_bytes);
                drop(exec_ctx);
                drop(plan);
                drop(sources);
                let _ = done_tx.send(res);
            })
            .map_err(|_| RunError::Internal(EngineError::internal("spawn sql worker thread")))?;

        match tokio::time::timeout_at(deadline, &mut done_rx).await {
            Ok(Ok(result)) => result
                .map(map_json_result)
                .map_err(|err| map_query_error_and_report(err, self.suspects.as_ref())),
            Ok(Err(_closed)) => {
                cancel.cancel();
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = join.join();
                })
                .await;
                Err(RunError::Internal(EngineError::internal(
                    "sql execution thread panicked",
                )))
            }
            Err(_) => {
                cancel.cancel();
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = join.join();
                })
                .await;
                Err(RunError::Timeout)
            }
        }
    }

    /// Streams `request.query` as RFC 4180 CSV up to [`EXPORT_MAX_ROWS`] data rows.
    #[cfg(test)]
    pub(crate) async fn sql_export_csv(
        &self,
        request: SqlRequest<'_>,
    ) -> Result<BoxStream<'static, Result<Bytes, RunError>>, RunError> {
        self.sql_export(request, ExportFormat::Csv).await
    }

    /// Streams `request.query` in `format` up to [`EXPORT_MAX_ROWS`] data rows.
    pub(crate) async fn sql_export(
        &self,
        request: SqlRequest<'_>,
        format: ExportFormat,
    ) -> Result<BoxStream<'static, Result<Bytes, RunError>>, RunError> {
        self.sql_export_with_limits(
            request,
            format,
            EXPORT_MAX_ROWS,
            EXPORT_PIECE_BYTES,
            EXPORT_MAX_CHUNK_BYTES,
            PARQUET_ROW_GROUP_ROWS,
            PARQUET_CHUNK_BYTES,
        )
        .await
    }

    /// Streams `request.query` with explicit row/piece/chunk/row-group budgets.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn sql_export_with_limits(
        &self,
        request: SqlRequest<'_>,
        format: ExportFormat,
        row_cap: usize,
        piece_bytes: usize,
        chunk_bytes: usize,
        parquet_rows: usize,
        parquet_bytes: usize,
    ) -> Result<BoxStream<'static, Result<Bytes, RunError>>, RunError> {
        let deadline = tokio::time::Instant::now() + request.timeout;
        // Synchronous parse + validate + param bind + optimize BEFORE any `.await`.
        let (catalog, sources) = self.build_catalog_and_sources()?;
        let plan =
            prepare_sql_plan_with_limit(&catalog, request.query, request.params, Some(row_cap))
                .map_err(map_query_error)?;

        if tokio::time::Instant::now() >= deadline {
            return Ok(timeout_stream());
        }

        let scoped_pool = match self.resolve_pool_until(deadline).await {
            Ok(pool) => pool,
            Err(RunError::Timeout) => return Ok(timeout_stream()),
            Err(err) => return Err(err),
        };
        let permit = match self.acquire_coordinator_until(deadline).await {
            Ok(permit) => permit,
            Err(RunError::Timeout) => return Ok(timeout_stream()),
            Err(err) => return Err(err),
        };

        let cancel = CancelToken::new();
        let mut init_guard = CancelOnDrop::new(cancel.clone());
        let stream_pool = Arc::clone(&scoped_pool);
        let exec_ctx = self.make_exec_ctx(scoped_pool, cancel.clone());
        let (tx, rx) = mpsc::channel::<Result<Bytes, RunError>>(EXPORT_CHANNEL_CAPACITY);
        let (init_tx, init_rx) = oneshot::channel::<Result<(), RunError>>();

        let cfg = ExportJobConfig {
            plan,
            sources,
            exec_ctx,
            suspects: self.suspects.clone(),
            format,
            row_cap,
            piece_bytes,
            chunk_bytes,
            parquet_rows,
            parquet_bytes,
        };
        let (done_tx, done_rx) = oneshot::channel::<()>();
        let join = std::thread::Builder::new()
            .name("cetra-sql-export".to_owned())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let _permit = permit;
                run_export_producer(&cfg, init_tx, &tx);
                drop(cfg);
                let _ = done_tx.send(());
            })
            .map_err(|_| RunError::Internal(EngineError::internal("spawn sql export thread")))?;

        match tokio::time::timeout_at(deadline, init_rx).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(err))) => {
                cancel.cancel();
                let _ = done_rx.await;
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = join.join();
                })
                .await;
                drop(stream_pool);
                return Err(err);
            }
            Ok(Err(_closed)) => {
                cancel.cancel();
                let _ = done_rx.await;
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = join.join();
                })
                .await;
                drop(stream_pool);
                return Err(RunError::Internal(EngineError::internal(
                    "export initialization channel closed",
                )));
            }
            Err(_) => {
                cancel.cancel();
                let _ = done_rx.await;
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = join.join();
                })
                .await;
                drop(stream_pool);
                return Ok(timeout_stream());
            }
        }

        let state = ExportStreamState {
            rx,
            cancel,
            done_rx: Some(done_rx),
            join: Some(join),
            _pool: Some(stream_pool),
            deadline,
            done: false,
        };
        init_guard.disarm();

        Ok(futures::stream::unfold(state, |mut state| async move {
            if state.done {
                return None;
            }
            if tokio::time::Instant::now() >= state.deadline {
                state.drain_on_abort().await;
                return Some((Err(RunError::Timeout), state));
            }

            enum Step {
                Timeout,
                Item(Option<Result<Bytes, RunError>>),
            }

            let step = tokio::select! {
                biased;
                () = tokio::time::sleep_until(state.deadline) => Step::Timeout,
                item = state.rx.recv() => Step::Item(item),
            };

            match step {
                Step::Timeout => {
                    state.drain_on_abort().await;
                    Some((Err(RunError::Timeout), state))
                }
                Step::Item(Some(Ok(bytes))) => Some((Ok(bytes), state)),
                Step::Item(Some(Err(err))) => {
                    state.drain_on_abort().await;
                    Some((Err(err), state))
                }
                Step::Item(None) => {
                    state.done = true;
                    state.drain_on_abort().await;
                    None
                }
            }
        })
        .boxed())
    }

    async fn resolve_pool_until(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<Arc<ScopedPool>, RunError> {
        if let Some(pool) = &self.pool {
            return Ok(Arc::clone(pool));
        }
        self.admit_until(deadline).await
    }

    async fn admit_until(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<Arc<ScopedPool>, RunError> {
        let now = tokio::time::Instant::now();
        if deadline <= now {
            return Err(RunError::Timeout);
        }
        match tokio::time::timeout_at(
            deadline,
            self.admission
                .admit(self.initial_memory, self.max_memory, self.admission_wait),
        )
        .await
        {
            Ok(Ok(pool)) => Ok(pool),
            Ok(Err(err)) => Err(map_query_error(err)),
            Err(_) => Err(RunError::Timeout),
        }
    }

    async fn acquire_coordinator_until(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<OwnedSemaphorePermit, RunError> {
        if tokio::time::Instant::now() >= deadline {
            return Err(RunError::Timeout);
        }
        match tokio::time::timeout_at(
            deadline,
            Arc::clone(&self.coordinator_slots).acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_closed)) => Err(RunError::Internal(EngineError::internal(
                "sql coordinator semaphore closed",
            ))),
            Err(_) => Err(RunError::Timeout),
        }
    }

    fn make_exec_ctx(&self, pool: Arc<ScopedPool>, cancel: CancelToken) -> ExecContext {
        ExecContext {
            pool,
            eval: EvalContext::new(current_time_ns(), cancel),
            spill: Arc::clone(&self.spill),
            workers: Arc::clone(&self.workers),
            batch_size: self.batch_size,
            target_partitions: self.target_partitions,
            metrics: ExecMetrics::default(),
        }
    }
}

struct ExportJobConfig {
    plan: LogicalPlan,
    sources: SqlSources,
    exec_ctx: ExecContext,
    suspects: Option<Suspects>,
    format: ExportFormat,
    row_cap: usize,
    piece_bytes: usize,
    chunk_bytes: usize,
    parquet_rows: usize,
    parquet_bytes: usize,
}

struct ExportStreamState {
    rx: mpsc::Receiver<Result<Bytes, RunError>>,
    cancel: CancelToken,
    done_rx: Option<oneshot::Receiver<()>>,
    join: Option<std::thread::JoinHandle<()>>,
    _pool: Option<Arc<ScopedPool>>,
    deadline: tokio::time::Instant,
    done: bool,
}

impl ExportStreamState {
    async fn drain_on_abort(&mut self) {
        self.done = true;
        self.cancel.cancel();
        // Close the receiver first so a producer blocked in `blocking_send` wakes immediately.
        self.rx.close();
        if let Some(done_rx) = self.done_rx.take() {
            let _ = done_rx.await;
        }
        if let Some(j) = self.join.take() {
            let _ = tokio::task::spawn_blocking(move || {
                let _ = j.join();
            })
            .await;
        }
        let _ = self._pool.take();
    }
}

impl Drop for ExportStreamState {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.rx.close();
    }
}

struct CancelOnDrop(Option<CancelToken>);

impl CancelOnDrop {
    fn new(cancel: CancelToken) -> Self {
        Self(Some(cancel))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(cancel) = &self.0 {
            cancel.cancel();
        }
    }
}

fn map_json_result(result: cetra_query::encode::SqlJsonResult) -> SqlJson {
    SqlJson {
        columns: result
            .columns
            .into_iter()
            .map(|c| SqlColumn {
                data_type: c.type_name().to_owned(),
                name: c.name,
            })
            .collect(),
        rows: result.rows,
        truncated: result.truncated,
    }
}

fn run_export_producer(
    cfg: &ExportJobConfig,
    init_tx: oneshot::Sender<Result<(), RunError>>,
    tx: &mpsc::Sender<Result<Bytes, RunError>>,
) {
    let compiled = match sql_export_plan_stream(&cfg.plan, &cfg.sources, &cfg.exec_ctx, cfg.row_cap)
    {
        Ok(pair) => pair,
        Err(err) => {
            let _ = init_tx.send(Err(map_query_error_and_report(err, cfg.suspects.as_ref())));
            return;
        }
    };
    let (schema, mut stream) = compiled;

    let mut parquet_exporter = if cfg.format == ExportFormat::Parquet {
        match ParquetBatchExporter::with_row_group_limits(
            Arc::clone(&schema),
            &cfg.exec_ctx.pool,
            cfg.parquet_rows,
            cfg.parquet_bytes,
        ) {
            Ok(exp) => Some(exp.with_chunk_bytes(cfg.chunk_bytes)),
            Err(err) => {
                drop(stream);
                let _ = init_tx.send(Err(map_query_error_and_report(err, cfg.suspects.as_ref())));
                return;
            }
        }
    } else {
        None
    };

    if init_tx.send(Ok(())).is_err() {
        drop(parquet_exporter);
        drop(stream);
        return;
    }

    if cfg.format == ExportFormat::Csv {
        match encode_csv_header(&schema) {
            Ok(header) => {
                if tx.blocking_send(Ok(Bytes::from(header))).is_err() {
                    drop(stream);
                    return;
                }
            }
            Err(err) => {
                drop(stream);
                let _ =
                    tx.blocking_send(Err(map_query_error_and_report(err, cfg.suspects.as_ref())));
                return;
            }
        }
    }

    let res = drive_export_loop(cfg, &schema, stream.as_mut(), parquet_exporter.take(), tx);
    drop(stream);
    if let Err(err) = res {
        let _ = tx.blocking_send(Err(map_query_error_and_report(err, cfg.suspects.as_ref())));
    }
}

fn drive_export_loop(
    cfg: &ExportJobConfig,
    _schema: &Schema,
    stream: &mut dyn PartitionStream,
    mut parquet_exporter: Option<ParquetBatchExporter>,
    tx: &mpsc::Sender<Result<Bytes, RunError>>,
) -> Result<(), QueryError> {
    let mut pull_res = cfg.exec_ctx.pool.register("sql_export_pull", false);
    let cancel = &cfg.exec_ctx.eval.cancel;

    loop {
        if cancel.is_cancelled() {
            pull_res.free();
            drop(parquet_exporter);
            return Err(QueryError::Cancelled);
        }
        let next = stream.next_batch(cancel, Some(&mut pull_res));
        let Some(batch) = (match next {
            Ok(b) => b,
            Err(err) => {
                pull_res.free();
                drop(parquet_exporter);
                return Err(err);
            }
        }) else {
            pull_res.free();
            break;
        };

        if batch.is_empty() {
            pull_res.free();
            continue;
        }

        let send_res: Result<bool, QueryError> = match cfg.format {
            ExportFormat::Csv => {
                let r = encode_csv_batch_pieces(
                    &batch,
                    cfg.piece_bytes,
                    cfg.chunk_bytes,
                    &cfg.exec_ctx.pool,
                    |chunk| {
                        if cancel.is_cancelled()
                            || tx.blocking_send(Ok(Bytes::from(chunk))).is_err()
                        {
                            return Err(QueryError::Cancelled);
                        }
                        Ok(())
                    },
                );
                drop(batch);
                pull_res.free();
                r.map(|()| false)
            }
            ExportFormat::Ndjson => {
                let r = encode_ndjson_batch_pieces(
                    &batch,
                    cfg.piece_bytes,
                    cfg.chunk_bytes,
                    &cfg.exec_ctx.pool,
                    |chunk| {
                        if cancel.is_cancelled()
                            || tx.blocking_send(Ok(Bytes::from(chunk))).is_err()
                        {
                            return Err(QueryError::Cancelled);
                        }
                        Ok(())
                    },
                );
                drop(batch);
                pull_res.free();
                r.map(|()| false)
            }
            ExportFormat::Parquet => {
                let Some(exp) = parquet_exporter.as_mut() else {
                    drop(batch);
                    pull_res.free();
                    return Err(QueryError::internal("missing parquet exporter"));
                };
                // Transfer batch memory ownership into ParquetBatchExporter, then release pull_res.
                let pushed = exp.push_batch(batch, Some(&mut pull_res));
                pull_res.free();
                match pushed {
                    Ok(chunks) => {
                        let mut closed = false;
                        for chunk in chunks {
                            if !chunk.is_empty()
                                && (cancel.is_cancelled() || tx.blocking_send(Ok(chunk)).is_err())
                            {
                                closed = true;
                                break;
                            }
                        }
                        Ok(closed)
                    }
                    Err(err) => Err(err),
                }
            }
        };

        match send_res {
            Ok(true) => {
                drop(parquet_exporter);
                return Ok(());
            }
            Ok(false) => {}
            Err(err) => {
                drop(parquet_exporter);
                return Err(err);
            }
        }
    }

    if let Some(exp) = parquet_exporter {
        for chunk in exp.finish()? {
            if !chunk.is_empty() && (cancel.is_cancelled() || tx.blocking_send(Ok(chunk)).is_err())
            {
                break;
            }
        }
    }
    Ok(())
}

fn timeout_stream() -> BoxStream<'static, Result<Bytes, RunError>> {
    futures::stream::once(async { Err(RunError::Timeout) }).boxed()
}

fn current_time_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

/// Maps [`QueryError`] from `cetra-query` into storage's [`RunError`] cleanly.
fn map_query_error_and_report(err: QueryError, suspects: Option<&Suspects>) -> RunError {
    let run_err = map_query_error(err);
    if let (Some(suspects), RunError::Internal(engine)) = (suspects, &run_err) {
        suspects.report(engine);
    }
    run_err
}

pub(crate) fn map_query_error(err: QueryError) -> RunError {
    match err {
        QueryError::InvalidRequest(msg) => RunError::InvalidRequest(msg),
        QueryError::Invalid(inv) => RunError::Invalid(InvalidQuery {
            message: inv.message,
            position: inv.position.map(|p| Position {
                line: p.line,
                column: p.column,
            }),
        }),
        QueryError::TooLarge => RunError::TooLarge,
        QueryError::Cancelled => RunError::Timeout,
        QueryError::Busy => RunError::Busy,
        QueryError::Failed(msg) => RunError::Failed(msg),
        err @ (QueryError::Column(_)
        | QueryError::CorruptData(_)
        | QueryError::StorageIo(_)
        | QueryError::Internal(_)) => RunError::Internal(EngineError::from_query(&err)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use cetra_column::{Batch, Column, DataType, Field, FlatColumn, FlatData, Schema};
    use cetra_core::ids::ProjectId;
    use object_store::memory::InMemory;

    use super::*;
    use crate::deletion::ProjectMemSnapshot;
    use crate::file_format::FileMetadataCache;
    use crate::memtable::Memtable;
    use crate::verify::VerifyQueue;
    use crate::wal::Table;

    fn empty_engine_ctx(pool_bytes: usize) -> (SqlEngineContext, tempfile::TempDir) {
        let rt = tokio::runtime::Handle::current();
        let store = Arc::new(InMemory::new());
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap();
        let cache = Arc::new(FileMetadataCache::new(0));
        let suspects = Suspects::new(Arc::new(VerifyQueue::default()));
        let spans_mt = Memtable::new(Table::Spans).snapshot();
        let scores_mt = Memtable::new(Table::Scores).snapshot();

        let spans = Arc::new(
            crate::engine::mem_source::MemtableSource::new(
                Table::Spans,
                project,
                None,
                Arc::clone(&spans_mt),
                Arc::from([]),
            )
            .unwrap(),
        );
        let traces = Arc::new(
            TracesSource::new(
                Arc::clone(&store) as _,
                project,
                Vec::new(),
                ProjectMemSnapshot::empty(),
                0,
                Arc::from([]),
                Arc::clone(&cache),
                suspects.clone(),
                rt.clone(),
            )
            .unwrap(),
        );
        let scores = Arc::new(
            ScoresSource::new(
                store,
                project,
                Vec::new(),
                scores_mt,
                None,
                Arc::from([]),
                cache,
                suspects,
                rt,
            )
            .unwrap(),
        );
        let dir = tempfile::tempdir().unwrap();
        let admission = Arc::new(AsyncAdmissionPool::new(pool_bytes));
        let workers = Arc::new(WorkerPool::new(2).unwrap());
        let spill = Arc::new(SpillManager::with_fixed_cap(
            dir.path().to_path_buf(),
            16 * 1024 * 1024,
        ));
        let ctx = SqlEngineContext::new(
            spans,
            traces,
            scores,
            admission,
            workers,
            spill,
            None,
            None,
            HashMap::new(),
        );
        (ctx, dir)
    }

    fn assert_send<T: Send>(_: &T) {}

    #[tokio::test]
    async fn futures_are_send_and_debug_is_privacy_safe() {
        let (mut ctx, _dir) = empty_engine_ctx(8 * 1024 * 1024);
        let schema = Schema::try_new_arc(vec![Field::new("s", DataType::Utf8, false)]).unwrap();
        let col = Column::Flat(
            FlatColumn::try_new(
                FlatData::Utf8(
                    cetra_column::Utf8Data::try_from_slices(["top-secret-row-payload"]).unwrap(),
                ),
                None,
            )
            .unwrap(),
        );
        let batch = Batch::try_new_all(Arc::clone(&schema), vec![col], 1).unwrap();
        ctx.register_extra_table(
            "dataset_items",
            Arc::new(BatchSource::new(schema, vec![batch]).unwrap()),
        )
        .unwrap();

        let dbg = format!("{ctx:?}");
        assert!(dbg.contains("SqlEngineContext"));
        assert!(dbg.contains("dataset_items"));
        assert!(!dbg.contains("top-secret-row-payload"), "leaked in {dbg}");

        let params = Map::new();
        assert_send(&ctx.sql_check("SELECT 1", &params));
        let req = SqlRequest {
            query: "SELECT 1",
            params: &params,
            limit: 10,
            timeout: Duration::from_secs(1),
        };
        assert_send(&ctx.sql_json(req));
        let req_exp = SqlRequest {
            query: "SELECT 1",
            params: &params,
            limit: 10,
            timeout: Duration::from_secs(1),
        };
        assert_send(&ctx.sql_export_csv(req_exp));
    }

    #[tokio::test]
    async fn timeout_drains_worker_and_releases_all_memory() {
        let (mut ctx, _dir) = empty_engine_ctx(64 * 1024 * 1024);
        let schema = Schema::try_new_arc(vec![Field::new("v", DataType::I64, false)]).unwrap();
        let values: Vec<i64> = (0..20_000).collect();
        let col = Column::Flat(FlatColumn::try_new(FlatData::I64(values), None).unwrap());
        let batch = Batch::try_new_all(Arc::clone(&schema), vec![col], 20_000).unwrap();
        ctx.register_extra_table(
            "experiment_items",
            Arc::new(BatchSource::new(schema, vec![batch]).unwrap()),
        )
        .unwrap();

        let params = Map::new();
        let err = ctx
            .sql_json(SqlRequest {
                query: "SELECT sum(a.v + b.v) FROM experiment_items a CROSS JOIN experiment_items b",
                params: &params,
                limit: 10,
                timeout: Duration::from_millis(50),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, RunError::Timeout), "{err:?}");
        assert_eq!(ctx.admission.used(), 0);
        assert_eq!(ctx.spill.spilled_bytes(), 0);
    }

    #[tokio::test]
    async fn export_timeout_and_early_drop_release_all_memory() {
        let (mut ctx, _dir) = empty_engine_ctx(64 * 1024 * 1024);
        let schema = Schema::try_new_arc(vec![Field::new("v", DataType::I64, false)]).unwrap();
        let values: Vec<i64> = (0..20_000).collect();
        let col = Column::Flat(FlatColumn::try_new(FlatData::I64(values), None).unwrap());
        let batch = Batch::try_new_all(Arc::clone(&schema), vec![col], 20_000).unwrap();
        ctx.register_extra_table(
            "experiment_items",
            Arc::new(BatchSource::new(schema, vec![batch]).unwrap()),
        )
        .unwrap();

        let params = Map::new();
        // 1. Timeout drains producer and frees pool
        let mut stream = ctx
            .sql_export_csv(SqlRequest {
                query: "SELECT sum(a.v + b.v) AS total FROM experiment_items a CROSS JOIN experiment_items b",
                params: &params,
                limit: SQL_MAX_ROWS,
                timeout: Duration::from_millis(50),
            })
            .await
            .unwrap();
        let mut got_timeout = false;
        while let Some(item) = stream.next().await {
            if matches!(item, Err(RunError::Timeout)) {
                got_timeout = true;
                break;
            }
        }
        assert!(got_timeout);
        assert!(stream.next().await.is_none());
        drop(stream);
        assert_eq!(ctx.admission.used(), 0);

        // 2. Early stream drop frees pool once producer exits
        let mut stream2 = ctx
            .sql_export_csv(SqlRequest {
                query: "SELECT a.v AS a_v, b.v AS b_v FROM experiment_items a CROSS JOIN experiment_items b",
                params: &params,
                limit: SQL_MAX_ROWS,
                timeout: Duration::from_secs(30),
            })
            .await
            .unwrap();
        let first = stream2.next().await.unwrap().unwrap();
        assert!(!first.is_empty());
        drop(stream2);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while ctx.admission.used() > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(ctx.admission.used(), 0);
    }

    #[tokio::test]
    async fn admission_deadline_returns_timeout_and_busy_is_opaque() {
        let (ctx, _dir) = empty_engine_ctx(1024);
        let ctx = ctx.with_memory_limits(1024, 1024, Duration::from_secs(5));
        let held = ctx
            .admission
            .admit(1024, 1024, Duration::from_millis(100))
            .await
            .unwrap();

        let params = Map::new();
        let err = ctx
            .sql_json(SqlRequest {
                query: "SELECT 1",
                params: &params,
                limit: 10,
                timeout: Duration::from_millis(30),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, RunError::Timeout), "{err:?}");

        let mut exp_stream = ctx
            .sql_export_csv(SqlRequest {
                query: "SELECT 1",
                params: &params,
                limit: 10,
                timeout: Duration::from_millis(30),
            })
            .await
            .unwrap();
        let first = exp_stream.next().await.unwrap().unwrap_err();
        assert!(matches!(first, RunError::Timeout), "{first:?}");
        drop(held);

        let mapped = map_query_error(QueryError::Busy);
        assert!(matches!(mapped, RunError::Busy), "{mapped:?}");
        assert!(EngineError::from_query(&QueryError::Busy).is_busy());
    }

    #[tokio::test]
    async fn sql_json_happy_path_releases_memory_and_export_drop_during_init_drains() {
        let (ctx, _dir) = empty_engine_ctx(64 * 1024 * 1024);
        let ctx = ctx.with_memory_limits(1024 * 1024, 64 * 1024 * 1024, Duration::from_secs(5));
        let schema = Schema::try_new_arc(vec![Field::new("v", DataType::I64, false)]).unwrap();
        let values: Vec<i64> = (0..10_000).collect();
        let col = Column::Flat(FlatColumn::try_new(FlatData::I64(values), None).unwrap());
        let batch = Batch::try_new_all(Arc::clone(&schema), vec![col], 10_000).unwrap();
        let mut ctx = ctx;
        ctx.register_extra_table(
            "experiment_items",
            Arc::new(BatchSource::new(schema, vec![batch]).unwrap()),
        )
        .unwrap();

        let params = Map::new();
        let res = ctx
            .sql_json(SqlRequest {
                query: "SELECT v FROM experiment_items ORDER BY v DESC",
                params: &params,
                limit: 5,
                timeout: Duration::from_secs(5),
            })
            .await
            .unwrap();
        assert_eq!(res.rows.len(), 5);
        assert!(res.truncated);
        assert_eq!(ctx.admission.used(), 0);

        // Dropping an export future before init completes cancels and releases admission.
        let fut = ctx.sql_export_csv(SqlRequest {
            query: "SELECT sum(a.v + b.v) AS total FROM experiment_items a CROSS JOIN experiment_items b",
            params: &params,
            limit: SQL_MAX_ROWS,
            timeout: Duration::from_secs(30),
        });
        let _ = tokio::time::timeout(Duration::from_millis(5), fut).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while ctx.admission.used() > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(ctx.admission.used(), 0);
    }
}
