//! Run a console statement and return D98's JSON result or an export stream.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use arrow::datatypes::Schema;
use arrow::record_batch::RecordBatch;
use bytes::Bytes;
use datafusion::dataframe::DataFrame;
use datafusion::logical_expr::LogicalPlan;
use datafusion::physical_plan::SendableRecordBatchStream;
use futures::stream::{BoxStream, Stream, StreamExt};
use serde_json::{Map, Value};

use super::encode::{self, Budget, encoded_estimate, json_rows};
use super::prepare_sql;
use crate::query_error::{RunError, classify};
use crate::verify::Suspects;
use crate::views::QueryContext;

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

/// One output column. `data_type` is the Arrow type as DataFusion prints it.
#[derive(Debug)]
pub struct SqlColumn {
    /// Output name.
    pub name: String,
    /// Arrow type display.
    pub data_type: String,
}

/// JSON result (D98).
#[derive(Debug)]
pub struct SqlJson {
    /// Output columns, in order.
    pub columns: Vec<SqlColumn>,
    /// One object per row. Keys follow [`Self::columns`].
    pub rows: Vec<Map<String, Value>>,
    /// True when more than `limit` rows matched. `rows` then holds the first `limit`.
    pub truncated: bool,
}

/// Most rows a JSON console response returns.
pub const SQL_MAX_ROWS: usize = 10_000;

/// Most bytes a JSON console response encodes to. Past it the query is
/// [`RunError::TooLarge`].
pub const SQL_MAX_RESULT_BYTES: usize = 32 * 1024 * 1024;

/// Most bytes one export chunk may hold. A single row whose estimated size is
/// larger is [`RunError::TooLarge`].
pub const EXPORT_MAX_CHUNK_BYTES: usize = 64 * 1024 * 1024;

/// An export slices each batch into pieces of at most this estimated size
/// before it encodes them. A piece is never larger than a chunk.
const EXPORT_PIECE_BYTES: usize = 8 * 1024 * 1024;

const _: () = assert!(EXPORT_PIECE_BYTES <= EXPORT_MAX_CHUNK_BYTES);

/// Most rows an export streams. The stream stops after this many data rows.
pub const EXPORT_MAX_ROWS: usize = 10_000_000;

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
    /// Run `request.query` and encode at most `request.limit` rows.
    ///
    /// The timeout wraps planning and execution. Dropping that future drops the
    /// result stream, which cancels DataFusion's tasks.
    ///
    /// # Errors
    ///
    /// [`RunError::InvalidRequest`] for a limit outside `1..=SQL_MAX_ROWS` or a
    /// zero timeout. Planning and parameter errors pass through. Execution
    /// errors are classified. The time limit yields [`RunError::Timeout`].
    pub async fn sql_json(&self, request: SqlRequest<'_>) -> Result<SqlJson, RunError> {
        self.sql_json_within(request, SQL_MAX_RESULT_BYTES).await
    }

    /// [`Self::sql_json`] with the encoded-size cap as a parameter, so tests can
    /// use a small one.
    async fn sql_json_within(
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
        match tokio::time::timeout(request.timeout, self.execute(request, max_bytes)).await {
            Err(_) => Err(RunError::Timeout),
            Ok(result) => result,
        }
    }

    async fn execute(
        &self,
        request: SqlRequest<'_>,
        max_bytes: usize,
    ) -> Result<SqlJson, RunError> {
        let plan = prepare_sql(self, request.query, request.params).await?;
        let frame = DataFrame::new(self.session().state(), plan)
            .limit(0, Some(request.limit + 1))
            .map_err(classify)?;
        let columns = frame
            .schema()
            .fields()
            .iter()
            .map(|field| SqlColumn {
                name: field.name().to_owned(),
                data_type: field.data_type().to_string(),
            })
            .collect();
        let schema = Schema::new(frame.schema().fields().clone());
        // The plan's limit stops the stream at `limit + 1` rows. The caller's
        // timeout drops this future, which drops the stream and cancels the query.
        let mut stream = frame
            .execute_stream()
            .await
            .map_err(|error| self.fail(error))?;
        let (batches, truncated) = self
            .take_rows(&mut stream, request.limit, max_bytes)
            .await?;
        let budget = Budget {
            max_bytes,
            pool: self.session().runtime_env().memory_pool.clone(),
        };
        let rows = json_rows(&batches, &schema, &budget)?;
        Ok(SqlJson {
            columns,
            rows,
            truncated,
        })
    }

    /// Pull batches until `limit` rows are kept or the stream ends. `truncated`
    /// is set when a row past `limit` arrives.
    ///
    /// # Errors
    ///
    /// [`RunError::TooLarge`] as soon as the estimated size of the kept rows
    /// passes `max_bytes`. Stream errors are classified as usual.
    async fn take_rows(
        &self,
        stream: &mut SendableRecordBatchStream,
        limit: usize,
        max_bytes: usize,
    ) -> Result<(Vec<RecordBatch>, bool), RunError> {
        let mut kept = Vec::new();
        let mut rows = 0;
        let mut bytes = 0usize;
        let mut truncated = false;
        while let Some(item) = stream.next().await {
            let mut batch = item.map_err(|error| self.fail(error))?;
            if batch.num_rows() == 0 {
                continue;
            }
            let room = limit - rows;
            if batch.num_rows() > room {
                truncated = true;
                if room == 0 {
                    break;
                }
                batch = batch.slice(0, room);
            }
            bytes = bytes.saturating_add(encoded_estimate(&batch)?);
            if bytes > max_bytes {
                return Err(RunError::TooLarge);
            }
            rows += batch.num_rows();
            kept.push(batch);
            if truncated {
                break;
            }
        }
        Ok((kept, truncated))
    }

    /// Stream `request.query` as `format`, up to [`EXPORT_MAX_ROWS`] data rows.
    ///
    /// `request.limit` is ignored. Preparation (parsing, validation, parameters,
    /// planning) fails as the outer error, before any byte is produced. The
    /// timeout is measured from this call. When it expires, the stream yields
    /// [`RunError::Timeout`] and ends.
    ///
    /// A failure while reading batches is yielded as `Err(classify(error))` and
    /// the stream ends. Bytes already yielded stay yielded: the server can only
    /// cut the response there, so a Parquet stream that fails mid-way has no footer.
    ///
    /// # Errors
    ///
    /// Parsing, validation, parameter and planning errors. Starting the execution
    /// stream fails the same way. The time limit and later execution errors are
    /// items of the returned stream.
    pub async fn sql_export(
        &self,
        request: SqlRequest<'_>,
        format: ExportFormat,
    ) -> Result<BoxStream<'static, Result<Bytes, RunError>>, RunError> {
        let deadline = tokio::time::Instant::now() + request.timeout;
        let plan = prepare_sql(self, request.query, request.params).await?;
        self.export_stream(
            plan,
            format,
            EXPORT_MAX_ROWS,
            deadline,
            EXPORT_PIECE_BYTES,
            EXPORT_MAX_CHUNK_BYTES,
        )
        .await
    }

    /// Execute `plan` and encode at most `row_cap` data rows.
    ///
    /// `deadline` is the timeout measured by [`Self::sql_export`]. Tests pass a
    /// small `row_cap` and small byte budgets; the public entry point passes
    /// [`EXPORT_MAX_ROWS`], [`EXPORT_PIECE_BYTES`] and [`EXPORT_MAX_CHUNK_BYTES`].
    async fn export_stream(
        &self,
        plan: LogicalPlan,
        format: ExportFormat,
        row_cap: usize,
        deadline: tokio::time::Instant,
        piece_bytes: usize,
        chunk_bytes: usize,
    ) -> Result<BoxStream<'static, Result<Bytes, RunError>>, RunError> {
        if tokio::time::Instant::now() >= deadline {
            return Ok(timeout_stream());
        }
        let frame = DataFrame::new(self.session().state(), plan)
            .limit(0, Some(row_cap))
            .map_err(classify)?;
        let schema = Schema::new(frame.schema().fields().clone());
        let body = match format {
            ExportFormat::Csv => ExportBody::Csv {
                header_pending: true,
            },
            ExportFormat::Ndjson => ExportBody::Ndjson,
            ExportFormat::Parquet => {
                ExportBody::Parquet(Box::new(encode::ParquetEncoder::new(&schema)?))
            }
        };
        let inner = frame
            .execute_stream()
            .await
            .map_err(|error| self.fail(error))?;
        if tokio::time::Instant::now() >= deadline {
            drop(inner);
            return Ok(timeout_stream());
        }
        let budget = Budget {
            max_bytes: chunk_bytes,
            pool: self.session().runtime_env().memory_pool.clone(),
        };
        Ok(ExportStream {
            inner: Some(inner),
            suspects: self.suspects(),
            schema,
            remaining: row_cap,
            deadline,
            sleep: Box::pin(tokio::time::sleep_until(deadline)),
            body,
            budget,
            piece_bytes,
            pending: None,
            buffered: None,
            stashed_error: None,
            done: false,
        }
        .boxed())
    }
}

fn timeout_stream() -> BoxStream<'static, Result<Bytes, RunError>> {
    futures::stream::once(async { Err(RunError::Timeout) }).boxed()
}

/// Which encoder the export stream still has to run.
enum ExportBody {
    Csv { header_pending: bool },
    Ndjson,
    Parquet(Box<encode::ParquetEncoder>),
}

/// Pulls up to `remaining` rows and encodes them until `deadline`.
struct ExportStream {
    inner: Option<SendableRecordBatchStream>,
    /// Reported to when a batch fails with an error that may be corruption.
    suspects: Suspects,
    schema: Schema,
    remaining: usize,
    deadline: tokio::time::Instant,
    sleep: Pin<Box<tokio::time::Sleep>>,
    body: ExportBody,
    /// Limit of one encoded chunk.
    budget: Budget,
    /// Estimated size of one piece, at most `budget.max_bytes`.
    piece_bytes: usize,
    /// Rows of a counted batch that the following polls still have to encode.
    /// Already counted against `remaining`.
    pending: Option<RecordBatch>,
    buffered: Option<RecordBatch>,
    stashed_error: Option<RunError>,
    done: bool,
}

impl Stream for ExportStream {
    type Item = Result<Bytes, RunError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let stream = self.get_mut();
        if stream.done {
            return Poll::Ready(None);
        }
        if stream.deadline_reached(cx) {
            stream.stop();
            return Poll::Ready(Some(Err(RunError::Timeout)));
        }
        if let ExportBody::Csv {
            header_pending: true,
        } = stream.body
        {
            stream.kick(cx);
            if let ExportBody::Csv { header_pending } = &mut stream.body {
                *header_pending = false;
            }
            return Poll::Ready(Some(Ok(encode::csv_header(&stream.schema))));
        }
        if let Some(error) = stream.stashed_error.take() {
            stream.stop();
            return Poll::Ready(Some(Err(error)));
        }
        loop {
            if let Some(batch) = stream.pending.take() {
                match stream.encode_next(&batch) {
                    Err(error) => {
                        stream.stop();
                        return Poll::Ready(Some(Err(error)));
                    }
                    Ok(None) => continue,
                    Ok(Some(bytes)) => return Poll::Ready(Some(Ok(bytes))),
                }
            }
            if stream.buffered.is_none() && (stream.remaining == 0 || stream.inner.is_none()) {
                return stream.finish();
            }
            match stream.next_batch(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return stream.finish(),
                Poll::Ready(Some(Err(error))) => {
                    stream.stop();
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Ready(Some(Ok(batch))) => match stream.encode_next(&batch) {
                    Err(error) => {
                        stream.stop();
                        return Poll::Ready(Some(Err(error)));
                    }
                    Ok(None) => continue,
                    Ok(Some(bytes)) => return Poll::Ready(Some(Ok(bytes))),
                },
            }
        }
    }
}

impl ExportStream {
    fn deadline_reached(&mut self, cx: &mut Context<'_>) -> bool {
        tokio::time::Instant::now() >= self.deadline
            || matches!(self.sleep.as_mut().poll(cx), Poll::Ready(()))
    }

    /// Poll the execution stream once so a CSV header does not leave it idle.
    fn kick(&mut self, cx: &mut Context<'_>) {
        if self.buffered.is_some() || self.stashed_error.is_some() {
            return;
        }
        match self.poll_inner(cx) {
            Poll::Ready(Some(Ok(batch))) => self.buffered = Some(batch),
            Poll::Ready(Some(Err(error))) => self.stashed_error = Some(error),
            Poll::Ready(None) | Poll::Pending => {}
        }
    }

    fn next_batch(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<RecordBatch, RunError>>> {
        if let Some(batch) = self.buffered.take() {
            return Poll::Ready(Some(Ok(self.cap(batch))));
        }
        match self.poll_inner(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(error))),
            Poll::Ready(Some(Ok(batch))) => Poll::Ready(Some(Ok(self.cap(batch)))),
        }
    }

    fn poll_inner(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<RecordBatch, RunError>>> {
        let Some(inner) = self.inner.as_mut() else {
            return Poll::Ready(None);
        };
        match inner.poll_next_unpin(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                self.inner.take();
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(error))) => {
                self.suspects.report(&error);
                Poll::Ready(Some(Err(classify(error))))
            }
            Poll::Ready(Some(Ok(batch))) => Poll::Ready(Some(Ok(batch))),
        }
    }

    fn cap(&mut self, batch: RecordBatch) -> RecordBatch {
        let rows = batch.num_rows().min(self.remaining);
        self.remaining -= rows;
        if self.remaining == 0 {
            self.inner.take();
        }
        if rows == batch.num_rows() {
            batch
        } else {
            batch.slice(0, rows)
        }
    }

    /// Encode the first piece of `batch` and keep the rest in `pending`.
    fn encode_next(&mut self, batch: &RecordBatch) -> Result<Option<Bytes>, RunError> {
        let (piece, rest) = split_piece(batch, self.piece_bytes, self.budget.max_bytes)?;
        self.pending = rest;
        let bytes = self.encode(&piece)?;
        if bytes
            .as_ref()
            .is_some_and(|bytes| bytes.len() > self.budget.max_bytes)
        {
            return Err(RunError::TooLarge);
        }
        Ok(bytes)
    }

    fn encode(&mut self, batch: &RecordBatch) -> Result<Option<Bytes>, RunError> {
        match &mut self.body {
            ExportBody::Csv { .. } => encode::csv_rows(batch, &self.schema, &self.budget),
            ExportBody::Ndjson => encode::ndjson_rows(batch, &self.schema, &self.budget),
            ExportBody::Parquet(encoder) => encoder.push(batch),
        }
    }

    fn finish(&mut self) -> Poll<Option<Result<Bytes, RunError>>> {
        self.inner.take();
        self.done = true;
        let ExportBody::Parquet(encoder) = std::mem::replace(&mut self.body, ExportBody::Ndjson)
        else {
            return Poll::Ready(None);
        };
        match (*encoder).finish() {
            Ok(bytes) if bytes.is_empty() => Poll::Ready(None),
            Ok(bytes) => Poll::Ready(Some(Ok(bytes))),
            Err(error) => Poll::Ready(Some(Err(error))),
        }
    }

    fn stop(&mut self) {
        self.inner.take();
        self.pending = None;
        self.done = true;
    }
}

/// Split the first piece off `batch`: the longest prefix whose estimate is at
/// most `piece_bytes`, and at least one row. Returns the piece and the rest.
///
/// # Errors
///
/// [`RunError::TooLarge`] when the piece's estimate passes `max_bytes`, which
/// happens only when one row alone is that large.
fn split_piece(
    batch: &RecordBatch,
    piece_bytes: usize,
    max_bytes: usize,
) -> Result<(RecordBatch, Option<RecordBatch>), RunError> {
    let rows = batch.num_rows();
    let mut keep = rows;
    if encoded_estimate(batch)? > piece_bytes {
        // The estimate grows with the prefix length, so bisect for the longest
        // prefix that fits. One row is the fallback when even that is too big.
        let (mut low, mut high) = (1, rows);
        while low < high {
            let mid = (low + high).div_ceil(2);
            if encoded_estimate(&batch.slice(0, mid))? <= piece_bytes {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        keep = low;
    }
    let piece = batch.slice(0, keep);
    if encoded_estimate(&piece)? > max_bytes {
        return Err(RunError::TooLarge);
    }
    let rest = (keep < rows).then(|| batch.slice(keep, rows - keep));
    Ok((piece, rest))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::query_error::RunError;
    use crate::test_support::{dataset_spans, example_query};
    use crate::views::{QueryContext, SqliteView, TimeWindow};
    use datafusion::arrow::array::{Int64Array, StringArray};
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    use datafusion::arrow::record_batch::RecordBatch;
    use datafusion::common::Diagnostic;
    use datafusion::datasource::TableProvider;
    use datafusion::datasource::memory::MemTable;
    use datafusion::error::DataFusionError;
    use datafusion::execution::memory_pool::UnboundedMemoryPool;
    use datafusion::execution::runtime_env::{RuntimeEnv, RuntimeEnvBuilder};
    use llmobs_core::time_serde::parse_rfc3339;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use serde_json::Value;

    fn context() -> QueryContext {
        QueryContext::new(example_query(), None).expect("example project")
    }

    fn empty_params() -> Map<String, Value> {
        Map::new()
    }

    fn request<'a>(query: &'a str, params: &'a Map<String, Value>, limit: usize) -> SqlRequest<'a> {
        SqlRequest {
            query,
            params,
            limit,
            timeout: Duration::from_secs(30),
        }
    }

    async fn run(query: &str, limit: usize) -> SqlJson {
        context()
            .sql_json(request(query, &empty_params(), limit))
            .await
            .unwrap_or_else(|error| panic!("{query}: {error}"))
    }

    fn numbers_match(left: &Value, right: &Value) -> bool {
        match (left, right) {
            (Value::Null, Value::Null) => true,
            (Value::Number(left), Value::Number(right)) => match (left.as_i64(), right.as_i64()) {
                (Some(left), Some(right)) => left == right,
                _ => {
                    let left = left.as_f64().expect("left");
                    let right = right.as_f64().expect("right");
                    (left - right).abs() < 1e-12
                }
            },
            _ => left == right,
        }
    }

    #[tokio::test]
    async fn json_rows_match_the_trace_fixture() {
        let result = run(
            "SELECT trace_id, name, start_time, tags, metadata, cost_total \
             FROM traces ORDER BY start_time DESC LIMIT 3",
            10,
        )
        .await;
        assert!(!result.truncated);
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../fixtures/api/responses/traces.json"
        ))
        .expect("fixture");
        let expected = fixture["data"].as_array().expect("data");
        assert_eq!(result.rows.len(), 3);
        let fields = [
            "trace_id",
            "name",
            "start_time",
            "tags",
            "metadata",
            "cost_total",
        ];
        for (row, trace) in result.rows.iter().zip(expected) {
            let keys: Vec<_> = row.keys().cloned().collect();
            assert_eq!(keys, fields);
            for field in fields {
                let actual = &row[field];
                let expected = &trace[field];
                assert!(
                    numbers_match(actual, expected),
                    "{field}: {actual} vs {expected}"
                );
            }
            let start = row["start_time"].as_str().expect("start");
            assert!(start.ends_with('Z'), "{start}");
            assert!(start.contains('.'), "{start}");
            assert_eq!(
                start.split_once('.').expect("fraction").1.len(),
                10,
                "{start}"
            );
        }
        let types: Vec<_> = result
            .columns
            .iter()
            .map(|column| format!("{} {}", column.name, column.data_type))
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            [
                "trace_id Utf8",
                "name Utf8",
                "start_time Timestamp(ns, \"UTC\")",
                "tags List(non-null Utf8)",
                "metadata Map(\"entries\": non-null Struct(\"keys\": non-null Utf8, \"values\": Utf8), unsorted)",
                "cost_total Float64",
            ]
        );
    }

    #[tokio::test]
    async fn the_row_cap_sets_truncated() {
        let query = "SELECT trace_id FROM traces ORDER BY start_time LIMIT 5";
        let two = run(query, 2).await;
        assert_eq!(two.rows.len(), 2);
        assert!(two.truncated);
        let five = run(query, 5).await;
        assert_eq!(five.rows.len(), 5);
        assert!(!five.truncated);
    }

    #[tokio::test]
    async fn a_limit_outside_the_range_is_rejected() {
        for limit in [0, 10_001] {
            match context()
                .sql_json(request("SELECT 1", &empty_params(), limit))
                .await
            {
                Err(RunError::InvalidRequest(message)) => {
                    assert_eq!(message, "limit must be between 1 and 10000");
                }
                other => panic!("limit {limit}: {other:?}"),
            }
        }
        let params = empty_params();
        let mut zero = request("SELECT 1", &params, 1);
        zero.timeout = Duration::ZERO;
        match context().sql_json(zero).await {
            Err(RunError::InvalidRequest(message)) => {
                assert_eq!(message, "timeout must be greater than zero");
            }
            other => panic!("zero timeout: {other:?}"),
        }
    }

    #[tokio::test]
    async fn special_values_follow_the_json_rules() {
        let result = run(
            "SELECT CAST('NaN' AS DOUBLE) AS x, X'00ff' AS b, CAST(1 AS TIMESTAMP) AS naive",
            10,
        )
        .await;
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0]["x"], Value::Null);
        assert_eq!(result.rows[0]["b"], Value::String("00ff".to_owned()));
        let naive = result.rows[0]["naive"].as_str().expect("naive");
        assert!(!naive.contains('Z'), "{naive}");
        assert!(naive.contains('T'), "{naive}");
    }

    #[tokio::test]
    async fn division_by_zero_is_a_failed_query() {
        let error = context()
            .sql_json(request("SELECT 1 / 0", &empty_params(), 10))
            .await
            .expect_err("divide");
        match error {
            RunError::Failed(message) => {
                assert!(message.to_ascii_lowercase().contains("zero"), "{message}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_oversized_regex_is_a_failed_query() {
        let error = context()
            .sql_json(request(
                "SELECT regexp_like(name, '(((a{100}){100}){100})') FROM spans",
                &empty_params(),
                10,
            ))
            .await
            .expect_err("regex");
        match error {
            RunError::Failed(message) => assert!(message.contains("CompiledTooBig"), "{message}"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    fn integers(rows: usize) -> Arc<dyn TableProvider> {
        let values: Vec<i64> = (0..rows as i64).collect();
        let schema = Arc::new(Schema::new(vec![Field::new("v", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![Arc::new(Int64Array::from(values))],
        )
        .expect("batch");
        Arc::new(MemTable::try_new(schema, vec![vec![batch]]).expect("table"))
    }

    fn runtime(bytes: usize) -> Arc<RuntimeEnv> {
        Arc::new(
            RuntimeEnvBuilder::new()
                .with_memory_limit(bytes, 1.0)
                .build()
                .expect("runtime"),
        )
    }

    fn context_with(runtime: Arc<RuntimeEnv>) -> QueryContext {
        let mut source = example_query();
        source.runtime = runtime;
        QueryContext::new(source, None).expect("context")
    }

    #[tokio::test]
    async fn a_sort_past_the_memory_limit_is_too_large() {
        let ctx = context_with(runtime(1 << 20));
        ctx.register_sqlite_view(SqliteView::ExperimentItems, integers(200_000))
            .expect("register");
        let error = ctx
            .sql_json(request(
                "SELECT v FROM experiment_items ORDER BY v",
                &empty_params(),
                10,
            ))
            .await
            .expect_err("memory");
        assert!(matches!(error, RunError::TooLarge), "{error:?}");
    }

    #[tokio::test]
    async fn a_cross_join_hits_the_timeout() {
        let ctx = context();
        ctx.register_sqlite_view(SqliteView::ExperimentItems, integers(20_000))
            .expect("register");
        let started = Instant::now();
        let error = ctx
            .sql_json(SqlRequest {
                // count(*) of this join is the product of the table sizes, so it
                // never runs. The sum has to read every pair.
                query: "SELECT sum(a.v + b.v) FROM experiment_items a, experiment_items b",
                params: &empty_params(),
                limit: 10,
                timeout: Duration::from_millis(50),
            })
            .await
            .expect_err("timeout");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "returned in {:?}",
            started.elapsed()
        );
        assert!(matches!(error, RunError::Timeout), "{error:?}");
    }

    #[tokio::test]
    async fn cancelling_a_query_releases_its_memory() {
        let runtime = runtime(1 << 30);
        let ctx = context_with(Arc::clone(&runtime));
        ctx.register_sqlite_view(SqliteView::ExperimentItems, integers(20_000))
            .expect("register");
        let error = ctx
            .sql_json(SqlRequest {
                // `a.v, b.v` is rejected: output names are unique (Q10). The row cap
                // turns a plain ORDER BY into a top-k, which finishes immediately,
                // so the window has to read the whole join.
                query: "SELECT a_v, sum(b_v) OVER () FROM ( \
                        SELECT a.v AS a_v, b.v AS b_v \
                        FROM experiment_items a, experiment_items b)",
                params: &empty_params(),
                limit: 10,
                timeout: Duration::from_millis(50),
            })
            .await
            .expect_err("cancel");
        assert!(matches!(error, RunError::Timeout), "{error:?}");
        let started = Instant::now();
        loop {
            if runtime.memory_pool.reserved() == 0 {
                break;
            }
            if started.elapsed() > Duration::from_secs(10) {
                panic!("reserved {}", runtime.memory_pool.reserved());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn wrappers_are_classified_by_the_root() {
        let planned = DataFusionError::Plan("bad plan".into())
            .with_diagnostic(Diagnostic::new_error("planned badly", None));
        match classify(planned) {
            RunError::Invalid(error) => assert_eq!(error.message, "planned badly"),
            other => panic!("diagnostic: {other:?}"),
        }
        let exhausted = DataFusionError::ResourcesExhausted("full".into()).context("sorting");
        assert!(matches!(classify(exhausted), RunError::TooLarge));
        let shared =
            DataFusionError::Shared(Arc::new(DataFusionError::ResourcesExhausted("full".into())));
        assert!(matches!(classify(shared), RunError::TooLarge));
    }

    #[tokio::test]
    async fn a_statement_that_is_not_a_query_stays_invalid() {
        let error = context()
            .sql_json(request("INSERT INTO spans VALUES (1)", &empty_params(), 10))
            .await
            .expect_err("insert");
        match error {
            RunError::Invalid(error) => {
                assert_eq!(error.message, "only SELECT queries are allowed");
                assert_eq!(error.position.map(|position| position.column), Some(1));
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_missing_parameter_is_an_invalid_request() {
        let error = context()
            .sql_json(request("SELECT {n:String} AS s", &empty_params(), 10))
            .await
            .expect_err("missing");
        match error {
            RunError::InvalidRequest(message) => {
                assert_eq!(message, "missing value for parameter n");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_time_window_limits_the_span_count() {
        let from = parse_rfc3339("2026-01-01T10:05:00Z").expect("from");
        let to = parse_rfc3339("2026-01-01T10:20:00Z").expect("to");
        let window = TimeWindow::new(from, to).expect("window");
        let ctx = QueryContext::new(example_query(), Some(window)).expect("windowed");
        let result = ctx
            .sql_json(request("SELECT count(*) FROM spans", &empty_params(), 10))
            .await
            .expect("count");
        let expected = dataset_spans()
            .iter()
            .filter(|span| (from..to).contains(&span.start_time))
            .count();
        let count = result.rows[0]
            .values()
            .next()
            .expect("column")
            .as_i64()
            .expect("count");
        assert_eq!(count, expected as i64);
        assert!(count > 0);
        assert!(count < dataset_spans().len() as i64);
    }

    async fn export_bytes(
        ctx: &QueryContext,
        query: &str,
        format: ExportFormat,
        row_cap: usize,
    ) -> Vec<u8> {
        let plan = prepare_sql(ctx, query, &empty_params())
            .await
            .unwrap_or_else(|error| panic!("{query}: {error}"));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut stream = ctx
            .export_stream(
                plan,
                format,
                row_cap,
                deadline,
                EXPORT_PIECE_BYTES,
                EXPORT_MAX_CHUNK_BYTES,
            )
            .await
            .unwrap_or_else(|error| panic!("{query}: {error}"));
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.unwrap_or_else(|error| panic!("{query}: {error}"));
            body.extend_from_slice(&chunk);
        }
        body
    }

    fn csv_lines(bytes: &[u8]) -> Vec<String> {
        let text = String::from_utf8(bytes.to_vec()).expect("csv utf-8");
        assert!(text.ends_with("\r\n"), "{text:?}");
        text.split("\r\n")
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()
    }

    fn ndjson_rows(bytes: &[u8]) -> Vec<Map<String, Value>> {
        let text = String::from_utf8(bytes.to_vec()).expect("ndjson utf-8");
        text.lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                serde_json::from_str(line).unwrap_or_else(|error| panic!("{line}: {error}"))
            })
            .collect()
    }

    fn parquet_batches(bytes: &[u8]) -> Vec<RecordBatch> {
        let reader = ParquetRecordBatchReaderBuilder::try_new(Bytes::copy_from_slice(bytes))
            .expect("parquet")
            .build()
            .expect("reader");
        reader.map(|batch| batch.expect("batch")).collect()
    }

    fn data_rows(batches: &[RecordBatch]) -> Vec<Map<String, Value>> {
        if batches.is_empty() {
            return Vec::new();
        }
        let schema = batches[0].schema();
        json_rows(batches, schema.as_ref(), &unbounded()).expect("json")
    }

    fn unbounded() -> Budget {
        Budget {
            max_bytes: usize::MAX,
            pool: Arc::new(UnboundedMemoryPool::default()),
        }
    }

    #[tokio::test]
    async fn csv_quotes_special_names_and_writes_tags_as_json() {
        let ctx = context();
        let bytes = export_bytes(
            &ctx,
            "SELECT trace_id, name, tags FROM traces ORDER BY start_time",
            ExportFormat::Csv,
            EXPORT_MAX_ROWS,
        )
        .await;
        let lines = csv_lines(&bytes);
        assert_eq!(lines.len(), 7, "{lines:?}");
        assert_eq!(lines[0], "trace_id,name,tags");
        assert!(
            lines[1..].iter().any(|line| line.contains('[')),
            "{lines:?}"
        );

        let schema = Arc::new(Schema::new(vec![Field::new("name", DataType::Utf8, true)]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![Arc::new(StringArray::from(vec![
                Some("plain"),
                Some("say \"hi\", there"),
            ]))],
        )
        .expect("batch");
        let table = MemTable::try_new(schema, vec![vec![batch]]).expect("table");
        let ctx = context();
        ctx.register_sqlite_view(SqliteView::ExperimentItems, Arc::new(table))
            .expect("register");
        let bytes = export_bytes(
            &ctx,
            "SELECT name FROM experiment_items",
            ExportFormat::Csv,
            EXPORT_MAX_ROWS,
        )
        .await;
        assert_eq!(
            csv_lines(&bytes),
            [
                "name".to_owned(),
                "plain".to_owned(),
                "\"say \"\"hi\"\", there\"".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn csv_guards_formula_cells_and_names_but_not_numbers() {
        let values = [
            "=1+1", "+x", "-x", "@x", "\tx", "\rx", "=1,2", "a=b", "plain",
        ];
        let ctx = context();
        let cells: Vec<Option<&str>> = values.iter().map(|value| Some(*value)).collect();
        ctx.register_sqlite_view(SqliteView::ExperimentItems, string_table(&cells))
            .expect("register");
        let query = "SELECT s AS \"=cmd\", -1 AS n FROM experiment_items";

        let bytes = export_bytes(&ctx, query, ExportFormat::Csv, EXPORT_MAX_ROWS).await;
        let lines = csv_lines(&bytes);
        let expected = [
            "'=1+1,-1",
            "'+x,-1",
            "'-x,-1",
            "'@x,-1",
            "'\tx,-1",
            "\"'\rx\",-1",
            "\"'=1,2\",-1",
            "a=b,-1",
            "plain,-1",
        ];
        assert_eq!(lines[0], "'=cmd,n");
        assert_eq!(lines.len(), expected.len() + 1, "{lines:?}");
        for (line, want) in lines[1..].iter().zip(expected) {
            assert_eq!(line, want);
        }

        // NDJSON is exact: the text is unchanged and -1 is still a number.
        let bytes = export_bytes(&ctx, query, ExportFormat::Ndjson, EXPORT_MAX_ROWS).await;
        let rows = ndjson_rows(&bytes);
        assert_eq!(rows.len(), values.len());
        for (row, value) in rows.iter().zip(values) {
            assert_eq!(row["=cmd"], Value::String(value.to_owned()));
            assert_eq!(row["n"], Value::from(-1));
        }
    }

    #[tokio::test]
    async fn ndjson_matches_sql_json_rows() {
        let query = "SELECT trace_id, name, tags FROM traces ORDER BY start_time";
        let ctx = context();
        let json = ctx
            .sql_json(request(query, &empty_params(), 10))
            .await
            .expect("json");
        let bytes = export_bytes(&ctx, query, ExportFormat::Ndjson, EXPORT_MAX_ROWS).await;
        assert_eq!(ndjson_rows(&bytes), json.rows);
    }

    #[tokio::test]
    async fn parquet_rows_match_the_spans_view() {
        let ctx = context();
        let bytes = export_bytes(
            &ctx,
            "SELECT * FROM spans",
            ExportFormat::Parquet,
            EXPORT_MAX_ROWS,
        )
        .await;
        let expected = crate::test_support::rows(&ctx, "SELECT * FROM spans").await;
        assert_eq!(data_rows(&parquet_batches(&bytes)), data_rows(&expected));
    }

    #[tokio::test]
    async fn the_row_cap_limits_each_format() {
        let ctx = context();
        let query = "SELECT trace_id FROM traces ORDER BY start_time";
        for format in [
            ExportFormat::Csv,
            ExportFormat::Ndjson,
            ExportFormat::Parquet,
        ] {
            let bytes = export_bytes(&ctx, query, format, 3).await;
            let rows = match format {
                ExportFormat::Csv => csv_lines(&bytes).len() - 1,
                ExportFormat::Ndjson => ndjson_rows(&bytes).len(),
                ExportFormat::Parquet => parquet_batches(&bytes)
                    .iter()
                    .map(RecordBatch::num_rows)
                    .sum(),
            };
            assert_eq!(rows, 3, "{format:?}");
        }
    }

    #[tokio::test]
    async fn invalid_sql_is_an_outer_error() {
        let exported = context()
            .sql_export(
                request("INSERT INTO spans VALUES (1)", &empty_params(), 10),
                ExportFormat::Csv,
            )
            .await;
        match exported {
            Err(RunError::Invalid(_)) => {}
            Err(error) => panic!("{error:?}"),
            Ok(_) => panic!("invalid SQL returned a stream"),
        }
    }

    #[tokio::test]
    async fn division_by_zero_ends_the_stream() {
        let mut stream = context()
            .sql_export(
                request(
                    "SELECT 1 / x FROM (VALUES (1), (0)) t(x)",
                    &empty_params(),
                    10,
                ),
                ExportFormat::Ndjson,
            )
            .await
            .expect("stream");
        let mut failed = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(_) => {}
                Err(RunError::Failed(message)) => {
                    failed = Some(message);
                    break;
                }
                Err(other) => panic!("{other:?}"),
            }
        }
        let message = failed.expect("failed");
        assert!(message.to_ascii_lowercase().contains("zero"), "{message}");
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn a_slow_export_hits_the_timeout() {
        let ctx = context();
        ctx.register_sqlite_view(SqliteView::ExperimentItems, integers(20_000))
            .expect("register");
        let started = Instant::now();
        let mut stream = ctx
            .sql_export(
                SqlRequest {
                    query: "SELECT sum(a.v + b.v) FROM experiment_items a, experiment_items b",
                    params: &empty_params(),
                    limit: SQL_MAX_ROWS,
                    timeout: Duration::from_millis(50),
                },
                ExportFormat::Csv,
            )
            .await
            .expect("stream");
        let mut timed_out = false;
        while let Some(item) = stream.next().await {
            match item {
                Err(RunError::Timeout) => {
                    timed_out = true;
                    break;
                }
                Ok(_) => {}
                Err(other) => panic!("{other:?}"),
            }
        }
        assert!(timed_out);
        assert!(stream.next().await.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "returned in {:?}",
            started.elapsed()
        );
    }

    /// One Int64 column `v` holding `values`, as a registered view.
    fn int_table(values: Vec<i64>) -> Arc<dyn TableProvider> {
        let schema = Arc::new(Schema::new(vec![Field::new("v", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![Arc::new(Int64Array::from(values))],
        )
        .expect("batch");
        Arc::new(MemTable::try_new(schema, vec![vec![batch]]).expect("table"))
    }

    /// One Utf8 column `s`, as a registered view.
    fn string_table(values: &[Option<&str>]) -> Arc<dyn TableProvider> {
        let schema = Arc::new(Schema::new(vec![Field::new("s", DataType::Utf8, true)]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![Arc::new(StringArray::from(values.to_vec()))],
        )
        .expect("batch");
        Arc::new(MemTable::try_new(schema, vec![vec![batch]]).expect("table"))
    }

    #[tokio::test]
    async fn many_aliases_of_one_column_are_too_large() {
        let ctx = context();
        ctx.register_sqlite_view(SqliteView::ExperimentItems, integers(10_000))
            .expect("register");
        // 4,000 aliases of an 8-byte column are 256 MiB for one 8,192-row batch,
        // and the statement stays under 64 KiB. The pool sees none of it.
        let aliases: Vec<String> = (1..=4_000).map(|i| format!("v AS c{i}")).collect();
        let query = format!("SELECT {} FROM experiment_items", aliases.join(", "));
        let error = ctx
            .sql_json(request(&query, &empty_params(), SQL_MAX_ROWS))
            .await
            .expect_err("aliases");
        assert!(matches!(error, RunError::TooLarge), "{error:?}");
    }

    #[tokio::test]
    async fn a_cross_join_of_wide_rows_is_too_large() {
        let ctx = context();
        let wide = "x".repeat(4_096);
        let values = vec![Some(wide.as_str()); 100];
        ctx.register_sqlite_view(SqliteView::ExperimentItems, string_table(&values))
            .expect("register");
        // 10,000 pairs of 4 KiB strings are 40 MiB, past the 32 MiB cap.
        let error = ctx
            .sql_json(request(
                "SELECT a.s AS a_s, b.s AS b_s \
                 FROM experiment_items a CROSS JOIN experiment_items b",
                &empty_params(),
                SQL_MAX_ROWS,
            ))
            .await
            .expect_err("cross join");
        assert!(matches!(error, RunError::TooLarge), "{error:?}");
    }

    #[tokio::test]
    async fn a_10000_row_query_succeeds_under_the_real_caps() {
        let ctx = context();
        ctx.register_sqlite_view(SqliteView::ExperimentItems, integers(10_000))
            .expect("register");
        let result = ctx
            .sql_json(request(
                "SELECT v FROM experiment_items ORDER BY v",
                &empty_params(),
                SQL_MAX_ROWS,
            ))
            .await
            .expect("10,000 rows");
        assert_eq!(result.rows.len(), 10_000);
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn json_past_the_budget_is_refused_even_when_the_estimate_fits() {
        let ctx = context();
        // The estimate counts 8 bytes a value. These values are about 21 bytes
        // of JSON each, so only the writer sees the overflow.
        let values: Vec<i64> = (0..1_000).map(|i| i * 1_000_000_000_000).collect();
        ctx.register_sqlite_view(SqliteView::ExperimentItems, int_table(values))
            .expect("register");
        let error = ctx
            .sql_json_within(
                request("SELECT v FROM experiment_items", &empty_params(), 1_000),
                10_000,
            )
            .await
            .expect_err("writer");
        assert!(matches!(error, RunError::TooLarge), "{error:?}");
    }

    #[tokio::test]
    async fn the_encoding_reservation_counts_against_the_pool() {
        let ctx = context_with(runtime(4 * 1024));
        ctx.register_sqlite_view(SqliteView::ExperimentItems, integers(1_000))
            .expect("register");
        let error = ctx
            .sql_json(request(
                "SELECT v FROM experiment_items",
                &empty_params(),
                SQL_MAX_ROWS,
            ))
            .await
            .expect_err("pool");
        assert!(matches!(error, RunError::TooLarge), "{error:?}");
    }

    #[tokio::test]
    async fn the_encoding_reservation_is_released_after_a_json_answer() {
        let runtime = runtime(1 << 30);
        let ctx = context_with(Arc::clone(&runtime));
        ctx.register_sqlite_view(SqliteView::ExperimentItems, integers(1_000))
            .expect("register");
        let result = ctx
            .sql_json(request(
                "SELECT v FROM experiment_items",
                &empty_params(),
                SQL_MAX_ROWS,
            ))
            .await
            .expect("json");
        assert_eq!(result.rows.len(), 1_000);
        assert_eq!(runtime.memory_pool.reserved(), 0);
    }

    /// Every item the export of `query` yields, in order.
    async fn export_items(
        ctx: &QueryContext,
        query: &str,
        format: ExportFormat,
        piece_bytes: usize,
        chunk_bytes: usize,
    ) -> Vec<Result<Bytes, RunError>> {
        let plan = prepare_sql(ctx, query, &empty_params())
            .await
            .unwrap_or_else(|error| panic!("{query}: {error}"));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let stream = ctx
            .export_stream(
                plan,
                format,
                EXPORT_MAX_ROWS,
                deadline,
                piece_bytes,
                chunk_bytes,
            )
            .await
            .unwrap_or_else(|error| panic!("{query}: {error}"));
        stream.collect().await
    }

    #[tokio::test]
    async fn every_export_chunk_stays_within_the_budget() {
        let ctx = context();
        ctx.register_sqlite_view(SqliteView::ExperimentItems, integers(1_000))
            .expect("register");
        let budget = 16 * 1024;
        for format in [
            ExportFormat::Csv,
            ExportFormat::Ndjson,
            ExportFormat::Parquet,
        ] {
            let chunks: Vec<Bytes> =
                export_items(&ctx, "SELECT v FROM experiment_items", format, 256, budget)
                    .await
                    .into_iter()
                    .map(|item| item.unwrap_or_else(|error| panic!("{format:?}: {error}")))
                    .collect();
            assert!(
                chunks.iter().all(|chunk| chunk.len() <= budget),
                "{format:?}"
            );
            if format != ExportFormat::Parquet {
                // A 256-byte piece holds about 32 integers, so the batch is split.
                assert!(chunks.len() > 10, "{format:?}: {} chunks", chunks.len());
            }
            let body: Vec<u8> = chunks.concat();
            let rows = match format {
                ExportFormat::Csv => csv_lines(&body).len() - 1,
                ExportFormat::Ndjson => ndjson_rows(&body).len(),
                ExportFormat::Parquet => parquet_batches(&body)
                    .iter()
                    .map(RecordBatch::num_rows)
                    .sum(),
            };
            assert_eq!(rows, 1_000, "{format:?}");
        }
    }

    #[tokio::test]
    async fn one_oversized_row_in_an_export_is_a_too_large_item() {
        let ctx = context();
        let big = "y".repeat(4_096);
        ctx.register_sqlite_view(
            SqliteView::ExperimentItems,
            string_table(&[Some("small"), Some(big.as_str()), Some("small")]),
        )
        .expect("register");
        // The budget is 1 KiB, so the 4 KiB row alone is too large. The row
        // before it is written first.
        let items = export_items(
            &ctx,
            "SELECT s FROM experiment_items",
            ExportFormat::Ndjson,
            512,
            1024,
        )
        .await;
        let mut items = items.into_iter();
        assert!(matches!(items.next(), Some(Ok(_))));
        assert!(
            matches!(items.next(), Some(Err(RunError::TooLarge))),
            "the oversized row must end the stream"
        );
        assert!(items.next().is_none());
    }

    #[tokio::test]
    async fn dropping_an_export_releases_its_memory() {
        let runtime = runtime(1 << 30);
        let ctx = context_with(Arc::clone(&runtime));
        ctx.register_sqlite_view(SqliteView::ExperimentItems, integers(20_000))
            .expect("register");
        let mut stream = ctx
            .sql_export(
                SqlRequest {
                    // Output names are unique (Q10). The export cap is 10M rows,
                    // so this join is still running after the first chunk.
                    query: "SELECT a.v AS a_v, b.v AS b_v \
                            FROM experiment_items a, experiment_items b",
                    params: &empty_params(),
                    limit: SQL_MAX_ROWS,
                    timeout: Duration::from_secs(30),
                },
                ExportFormat::Csv,
            )
            .await
            .expect("stream");
        let first = stream.next().await.expect("chunk").expect("bytes");
        assert!(!first.is_empty());
        drop(stream);
        let started = Instant::now();
        loop {
            if runtime.memory_pool.reserved() == 0 {
                break;
            }
            if started.elapsed() > Duration::from_secs(10) {
                panic!("reserved {}", runtime.memory_pool.reserved());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
