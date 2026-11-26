//! Async storage-backed Shape B SQL runner ([`SqlEngineContext`]) over M3 primitives
//! (`ScanSource`, `TracesSource`, `ScoresSource`, `AsyncAdmissionPool`, `WorkerPool`,
//! `SpillManager`) and streaming [`ParquetBatchExporter`].

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use cetra_column::{CancelToken, Schema, ScopedPool};
use cetra_query::encode::{
    EXPORT_MAX_CHUNK_BYTES, EXPORT_MAX_ROWS, EXPORT_PIECE_BYTES, SQL_MAX_RESULT_BYTES,
    SQL_MAX_ROWS, SqlJsonResult, encode_csv_header,
};
use cetra_query::exec::{
    BatchSource, DEFAULT_BATCH_SIZE, ExecContext, ExecMetrics, PartitionStream, ScanSource,
    SpillManager, WorkerPool,
};
use cetra_query::sql::{
    LogicalPlan, SqlCatalog, SqlColumn, SqlSources, TimeWindow, encode_csv_batch_pieces,
    encode_ndjson_batch_pieces, execute_plan_json, prepare_sql_plan, sql_check as query_sql_check,
    sql_export_plan_stream,
};
use cetra_query::{EvalContext, QueryError};
use datafusion::error::DataFusionError;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot};

use crate::column_bridge::{arrow_schema_to_column, record_batch_to_column};
use crate::query_error::{InvalidQuery, Position, RunError};
use crate::sql::{ExportFormat, SqlRequest};
use crate::verify::Suspects;

use super::admission::AsyncAdmissionPool;
use super::parquet_export::{PARQUET_CHUNK_BYTES, PARQUET_ROW_GROUP_ROWS, ParquetBatchExporter};
use super::scores_source::ScoresSource;
use super::traces_source::TracesSource;

const EXPORT_CHANNEL_CAPACITY: usize = 4;
const DEFAULT_ADMISSION_WAIT: Duration = Duration::from_secs(5);
const DEFAULT_TARGET_PARTITIONS: usize = 2;

/// Self-contained Shape B SQL execution context over M3 storage primitives.
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
    suspects: Suspects,
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
            .field("target_partitions", &self.target_partitions)
            .field("initial_memory", &self.initial_memory)
            .field("max_memory", &self.max_memory)
            .field("admission_used", &self.admission.used())
            .field("admission_limit", &self.admission.limit())
            .finish_non_exhaustive()
    }
}

impl SqlEngineContext {
    /// Creates a [`SqlEngineContext`] over M3 storage sources and resource pools.
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
        suspects: Suspects,
    ) -> Self {
        let max_memory = admission.limit();
        let target_partitions = DEFAULT_TARGET_PARTITIONS.min(workers.num_workers().max(1));
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
            suspects,
            target_partitions,
            initial_memory: 0,
            max_memory,
            admission_wait: DEFAULT_ADMISSION_WAIT,
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }

    /// Sets admission memory limits `(initial, cap, wait)` for each SQL query.
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

    /// Sets target rows per batch for execution operators.
    pub(crate) fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size.max(1);
        self
    }

    /// Returns the corrupt-file [`Suspects`] tracker attached to this context.
    #[allow(dead_code)]
    pub(crate) fn suspects(&self) -> &Suspects {
        &self.suspects
    }

    /// Returns the admission pool attached to this context.
    #[cfg(test)]
    pub(crate) fn admission(&self) -> &AsyncAdmissionPool {
        &self.admission
    }

    /// Registers an in-memory [`BatchSource`] view (e.g. `dataset_items`, `experiment_items`).
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

    /// Converts Arrow `RecordBatch`es into a [`BatchSource`] and registers it under `name`.
    #[allow(dead_code)]
    pub(crate) fn register_extra_arrow_table(
        &mut self,
        name: impl Into<String>,
        arrow_schema: &arrow::datatypes::Schema,
        batches: &[arrow::record_batch::RecordBatch],
    ) -> Result<(), RunError> {
        let col_schema = Arc::new(
            arrow_schema_to_column(arrow_schema)
                .map_err(|e| map_query_error(QueryError::from(e)))?,
        );
        let mut col_batches = Vec::with_capacity(batches.len());
        for rb in batches {
            let b = record_batch_to_column(rb).map_err(|e| map_query_error(QueryError::from(e)))?;
            col_batches.push(b);
        }
        let source = Arc::new(BatchSource::new(col_schema, col_batches).map_err(map_query_error)?);
        self.register_extra_table(name, source)
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
        query_sql_check(&catalog, query, params).map_err(map_query_error)
    }

    /// Executes `request.query` and encodes at most `request.limit` rows within 32 MiB JSON.
    pub(crate) async fn sql_json(
        &self,
        request: SqlRequest<'_>,
    ) -> Result<SqlJsonResult, RunError> {
        self.sql_json_within(request, SQL_MAX_RESULT_BYTES).await
    }

    /// Executes `request.query` with an explicit JSON byte budget `max_bytes`.
    pub(crate) async fn sql_json_within(
        &self,
        request: SqlRequest<'_>,
        max_bytes: usize,
    ) -> Result<SqlJsonResult, RunError> {
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

        // Synchronous parse + validate + param bind + optimize BEFORE any `.await`.
        let (catalog, sources) = self.build_catalog_and_sources()?;
        let plan =
            prepare_sql_plan(&catalog, request.query, request.params).map_err(map_query_error)?;
        let limit = request.limit;

        let deadline = tokio::time::Instant::now() + request.timeout;
        let scoped_pool = self.admit_until(deadline).await?;

        let cancel = CancelToken::new();
        let _drop_guard = CancelOnDrop(cancel.clone());
        let exec_ctx = self.make_exec_ctx(scoped_pool, cancel.clone());

        let (done_tx, mut done_rx) = oneshot::channel();
        let join = std::thread::Builder::new()
            .name("cetra-sql-json".to_owned())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let res = execute_plan_json(&plan, &sources, &exec_ctx, limit, max_bytes);
                let _ = done_tx.send(res);
            })
            .map_err(|e| {
                RunError::Internal(DataFusionError::Internal(format!(
                    "failed to spawn sql worker thread: {e}"
                )))
            })?;

        match tokio::time::timeout_at(deadline, &mut done_rx).await {
            Ok(Ok(result)) => {
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = join.join();
                })
                .await;
                result.map_err(map_query_error)
            }
            Ok(Err(_closed)) => {
                cancel.cancel();
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = join.join();
                })
                .await;
                Err(RunError::Internal(DataFusionError::Internal(
                    "sql execution thread panicked".to_owned(),
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
    pub(crate) async fn sql_export_csv(
        &self,
        request: SqlRequest<'_>,
    ) -> Result<BoxStream<'static, Result<Bytes, RunError>>, RunError> {
        self.sql_export(request, ExportFormat::Csv).await
    }

    /// Streams `request.query` as NDJSON up to [`EXPORT_MAX_ROWS`] data rows.
    pub(crate) async fn sql_export_ndjson(
        &self,
        request: SqlRequest<'_>,
    ) -> Result<BoxStream<'static, Result<Bytes, RunError>>, RunError> {
        self.sql_export(request, ExportFormat::Ndjson).await
    }

    /// Streams `request.query` as ZSTD Parquet via [`ParquetBatchExporter`].
    pub(crate) async fn sql_export_parquet(
        &self,
        request: SqlRequest<'_>,
    ) -> Result<BoxStream<'static, Result<Bytes, RunError>>, RunError> {
        self.sql_export(request, ExportFormat::Parquet).await
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
        // Synchronous parse + validate + param bind + optimize BEFORE any `.await`.
        let (catalog, sources) = self.build_catalog_and_sources()?;
        let plan =
            prepare_sql_plan(&catalog, request.query, request.params).map_err(map_query_error)?;

        let deadline = tokio::time::Instant::now() + request.timeout;
        if tokio::time::Instant::now() >= deadline {
            return Ok(timeout_stream());
        }

        let scoped_pool =
            match tokio::time::timeout_at(deadline, self.admit_for_export(deadline)).await {
                Ok(Ok(pool)) => pool,
                Ok(Err(err)) => return Err(err),
                Err(_) => return Ok(timeout_stream()),
            };

        let cancel = CancelToken::new();
        let exec_ctx = self.make_exec_ctx(scoped_pool, cancel.clone());
        let (tx, rx) = mpsc::channel::<Result<Bytes, RunError>>(EXPORT_CHANNEL_CAPACITY);
        let (init_tx, init_rx) = oneshot::channel::<Result<(), RunError>>();

        let cfg = ExportJobConfig {
            plan,
            sources,
            exec_ctx,
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
                run_export_producer(&cfg, init_tx, &tx);
                let _ = done_tx.send(());
            })
            .map_err(|e| {
                RunError::Internal(DataFusionError::Internal(format!(
                    "failed to spawn sql export thread: {e}"
                )))
            })?;

        match tokio::time::timeout_at(deadline, init_rx).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(err))) => {
                cancel.cancel();
                let _ = done_rx.await;
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = join.join();
                })
                .await;
                return Err(err);
            }
            Ok(Err(_closed)) => {
                cancel.cancel();
                let _ = done_rx.await;
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = join.join();
                })
                .await;
                return Err(RunError::Internal(DataFusionError::Internal(
                    "export initialization channel closed".to_owned(),
                )));
            }
            Err(_) => {
                cancel.cancel();
                let _ = done_rx.await;
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = join.join();
                })
                .await;
                return Ok(timeout_stream());
            }
        }

        let state = ExportStreamState {
            rx,
            cancel,
            done_rx: Some(done_rx),
            join: Some(join),
            deadline,
            done: false,
        };

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

    async fn admit_until(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<Arc<ScopedPool>, RunError> {
        let now = tokio::time::Instant::now();
        let remaining = deadline.saturating_duration_since(now);
        if remaining.is_zero() {
            return Err(RunError::Timeout);
        }
        let wait = self.admission_wait.min(remaining);
        match tokio::time::timeout_at(
            deadline,
            self.admission
                .admit(self.initial_memory, self.max_memory, wait),
        )
        .await
        {
            Ok(Ok(pool)) => Ok(pool),
            Ok(Err(err)) => Err(map_query_error(err)),
            Err(_) => Err(RunError::Timeout),
        }
    }

    async fn admit_for_export(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<Arc<ScopedPool>, RunError> {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let wait = self.admission_wait.min(remaining);
        self.admission
            .admit(self.initial_memory, self.max_memory, wait)
            .await
            .map_err(map_query_error)
    }

    fn make_exec_ctx(&self, pool: Arc<ScopedPool>, cancel: CancelToken) -> ExecContext {
        ExecContext {
            pool,
            eval: EvalContext {
                now_ns: current_time_ns(),
                cancel,
            },
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
    }
}

impl Drop for ExportStreamState {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.rx.close();
    }
}

struct CancelOnDrop(CancelToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
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
            let _ = init_tx.send(Err(map_query_error(err)));
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
            Ok(exp) => Some(exp),
            Err(err) => {
                drop(stream);
                let _ = init_tx.send(Err(map_query_error(err)));
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
                let _ = tx.blocking_send(Err(map_query_error(err)));
                return;
            }
        }
    }

    let res = drive_export_loop(cfg, &schema, stream.as_mut(), parquet_exporter.take(), tx);
    drop(stream);
    if let Err(err) = res {
        let _ = tx.blocking_send(Err(map_query_error(err)));
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
                let mut closed = false;
                let r = encode_csv_batch_pieces(
                    &batch,
                    cfg.piece_bytes,
                    cfg.chunk_bytes,
                    &cfg.exec_ctx.pool,
                    |chunk| {
                        if tx.blocking_send(Ok(Bytes::from(chunk))).is_err() {
                            closed = true;
                        }
                        Ok(())
                    },
                );
                drop(batch);
                pull_res.free();
                r.map(|()| closed)
            }
            ExportFormat::Ndjson => {
                let mut closed = false;
                let r = encode_ndjson_batch_pieces(
                    &batch,
                    cfg.piece_bytes,
                    cfg.chunk_bytes,
                    &cfg.exec_ctx.pool,
                    |chunk| {
                        if tx.blocking_send(Ok(Bytes::from(chunk))).is_err() {
                            closed = true;
                        }
                        Ok(())
                    },
                );
                drop(batch);
                pull_res.free();
                r.map(|()| closed)
            }
            ExportFormat::Parquet => {
                let Some(exp) = parquet_exporter.as_mut() else {
                    drop(batch);
                    pull_res.free();
                    return Err(QueryError::internal("missing parquet exporter"));
                };
                // Transfer batch memory ownership into ParquetBatchExporter, then release pull_res.
                let pushed = exp.push_batch(batch);
                pull_res.free();
                match pushed {
                    Ok(chunks) => {
                        let mut closed = false;
                        for chunk in chunks {
                            if !chunk.is_empty()
                                && tx.blocking_send(Ok(Bytes::from(chunk))).is_err()
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
        let tail = exp.finish()?;
        if !tail.is_empty() {
            let _ = tx.blocking_send(Ok(Bytes::from(tail)));
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
        QueryError::Failed(msg) => RunError::Failed(msg),
        QueryError::Column(col_err) => {
            RunError::Internal(DataFusionError::External(Box::new(col_err)))
        }
        QueryError::Internal(msg) => RunError::Internal(DataFusionError::External(msg.into())),
        QueryError::Busy => {
            RunError::Internal(DataFusionError::External("query admission busy".into()))
        }
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
    use crate::memtable::Memtable;
    use crate::verify::VerifyQueue;
    use crate::wal::Table;

    fn empty_engine_ctx(pool_bytes: usize) -> (SqlEngineContext, tempfile::TempDir) {
        let rt = tokio::runtime::Handle::current();
        let store = Arc::new(InMemory::new());
        let project: ProjectId = "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap();
        let cache = Arc::new(super::super::metadata_cache::MetadataCache::new(0));
        let suspects = Suspects::new(Arc::new(VerifyQueue::default()));
        let spans_mt = Memtable::new(Table::Spans).snapshot();
        let scores_mt = Memtable::new(Table::Scores).snapshot();

        let spans = Arc::new(
            super::super::mem_source::MemtableSource::new(
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
                spans_mt,
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
                suspects.clone(),
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
            suspects,
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
}
