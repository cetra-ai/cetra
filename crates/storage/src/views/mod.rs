//! User-facing views over the stored snapshots (D91, D101).

mod catalog;

use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use cetra_column::{Batch, Schema};
use cetra_query::exec::{
    BatchSource, CollectSink, ScanSource, ScanSpec, run_pipeline as exec_run_pipeline,
};
use cetra_query::sql::TimeWindow as QueryTimeWindow;

pub use catalog::{
    ColumnInfo, ColumnKind, FunctionInfo, ViewColumns, function_catalog, view_catalog,
};

use crate::EngineContext;
use crate::error::EngineError;
use crate::query_error::RunError;
use crate::sql::SqlEngineContext;

/// What `Storage::query` hands the query layer (D91).
///
/// Holds the admitted memory pool and snapshot sources for one project read.
#[derive(Debug, Clone)]
pub struct ProjectQuery {
    pub(crate) inner: EngineContext,
}

impl ProjectQuery {
    /// Wraps an admitted [`EngineContext`] in [`ProjectQuery`].
    pub(crate) fn from_engine(inner: EngineContext) -> Self {
        Self { inner }
    }

    /// Borrows the underlying [`EngineContext`].
    #[cfg(test)]
    pub(crate) fn engine(&self) -> &EngineContext {
        &self.inner
    }

    /// Returns a copy with `visible_from` overridden for visibility tests.
    #[cfg(test)]
    pub(crate) fn with_visible_from(mut self, visible_from: Option<i64>) -> Self {
        self.inner.visible_from = visible_from;
        self
    }

    /// Collects raw stored `spans` rows across Parquet and memtable sources for this query.
    #[doc(hidden)]
    pub async fn collect_raw_spans(&self) -> Result<Vec<Batch>, RunError> {
        let source = Arc::clone(&self.inner.spans);
        collect_scan_source(&self.inner, source).await
    }

    /// Collects raw stored `traces` rows across Parquet and memtable sources for this query.
    #[cfg(test)]
    pub(crate) async fn collect_raw_traces(&self) -> Result<Vec<Batch>, RunError> {
        let source: Arc<dyn ScanSource> = self.inner.traces.clone();
        collect_scan_source(&self.inner, source).await
    }
}

async fn collect_scan_source(
    engine: &EngineContext,
    source: Arc<dyn ScanSource>,
) -> Result<Vec<Batch>, RunError> {
    let (batches, _) = engine
        .run_with_ctx(Duration::from_secs(30), move |exec_ctx| {
            let spec = ScanSpec::default();
            let streams = source.open_partitions(&spec, exec_ctx)?;
            if streams.is_empty() {
                return Ok(Vec::new());
            }
            let pool = Arc::clone(&exec_ctx.pool);
            let sinks = exec_run_pipeline(
                exec_ctx,
                streams,
                |_| Ok(Vec::new()),
                move |_| Ok(CollectSink::new(&pool)),
            )?;
            let mut out = Vec::new();
            for sink in sinks {
                for batch in sink.into_batches() {
                    if batch.num_rows() > 0 {
                        out.push(batch);
                    }
                }
            }
            Ok(out)
        })
        .await?;
    Ok(batches)
}

/// Inclusive `from`, exclusive `to`, nanoseconds since the Unix epoch, UTC (D101).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimeWindow {
    from: i64,
    to: i64,
}

impl TimeWindow {
    /// Builds a window. `from` must be strictly before `to`.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::InvalidRequest`] when `from >= to`.
    pub fn new(from: i64, to: i64) -> Result<Self, RunError> {
        if from < to {
            Ok(Self { from, to })
        } else {
            Err(RunError::InvalidRequest(
                "from must be before to".to_owned(),
            ))
        }
    }

    /// Inclusive start, nanoseconds.
    pub fn from(&self) -> i64 {
        self.from
    }

    /// Exclusive end, nanoseconds.
    pub fn to(&self) -> i64 {
        self.to
    }
}

/// A view the SQL sandbox and read queries can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ViewName {
    /// Merged traces.
    Traces,
    /// One row per span.
    Spans,
    /// One row per session.
    Sessions,
    /// Live scores.
    Scores,
    /// Server-built experiment rows.
    ExperimentItems,
    /// Server-built dataset rows.
    DatasetItems,
}

impl ViewName {
    /// Unqualified table name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Traces => "traces",
            Self::Spans => "spans",
            Self::Sessions => "sessions",
            Self::Scores => "scores",
            Self::ExperimentItems => "experiment_items",
            Self::DatasetItems => "dataset_items",
        }
    }
}

/// A server-owned table the SQL console can name (D93).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SqliteView {
    /// Experiment item rows.
    ExperimentItems,
    /// Dataset item rows.
    DatasetItems,
}

impl SqliteView {
    /// Returns the matching [`ViewName`].
    fn view_name(self) -> ViewName {
        match self {
            Self::ExperimentItems => ViewName::ExperimentItems,
            Self::DatasetItems => ViewName::DatasetItems,
        }
    }

    /// Unqualified table name.
    pub fn as_str(self) -> &'static str {
        self.view_name().as_str()
    }
}

/// Sandboxed query context over a snapshot of project data.
///
/// Shares the [`ProjectQuery`]'s admitted memory pool across both typed read
/// endpoints (`crate::reads`) and sandboxed SQL queries (`sql_json` / `sql_export`).
pub struct QueryContext {
    engine: EngineContext,
    window: Option<TimeWindow>,
    extra_tables: RwLock<HashMap<SqliteView, Arc<BatchSource>>>,
}

impl fmt::Debug for QueryContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let extra_count = self
            .extra_tables
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .len();
        formatter
            .debug_struct("QueryContext")
            .field("window", &self.window)
            .field("extra_tables", &extra_count)
            .field("engine", &self.engine)
            .finish_non_exhaustive()
    }
}

impl QueryContext {
    /// Wraps `source` with an optional SQL console scan `window`.
    ///
    /// `window`, when set, restricts every SQL base table scan (`spans`, `traces`,
    /// `scores`, and derived `sessions`) to `[from, to)`. `source`'s `visible_from`,
    /// when set, hides older rows across both SQL and read queries (D-P-21).
    pub fn new(source: ProjectQuery, window: Option<TimeWindow>) -> Result<Self, RunError> {
        Ok(Self {
            engine: source.inner,
            window,
            extra_tables: RwLock::new(HashMap::new()),
        })
    }

    /// Wraps an [`EngineContext`] directly for internal read tests.
    #[cfg(test)]
    pub(crate) fn from_engine(engine: EngineContext) -> Self {
        Self {
            engine,
            window: None,
            extra_tables: RwLock::new(HashMap::new()),
        }
    }

    /// Borrows the underlying execution context used by read endpoints.
    pub(crate) fn engine(&self) -> &EngineContext {
        &self.engine
    }

    /// Registers a server-built SQLite-backed view table under its view name.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::InvalidRequest`] when `view` is already registered, or
    /// [`RunError::Internal`] when a batch fails schema validation.
    pub fn register_sqlite_view(
        &self,
        view: SqliteView,
        schema: Arc<Schema>,
        batches: Vec<Batch>,
    ) -> Result<(), RunError> {
        let source = Arc::new(
            BatchSource::new(schema, batches)
                .map_err(|e| RunError::Internal(EngineError::from_query(&e)))?,
        );
        let mut guard = self
            .extra_tables
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if guard.contains_key(&view) {
            return Err(RunError::InvalidRequest(format!(
                "{} is already registered",
                view.as_str()
            )));
        }
        guard.insert(view, source);
        Ok(())
    }

    /// Builds a [`SqlEngineContext`] sharing this query's admitted memory pool and extra views.
    pub(crate) fn sql_engine(&self) -> Result<SqlEngineContext, RunError> {
        let sql_window = self
            .window
            .map(|w| {
                QueryTimeWindow::new(w.from(), w.to())
                    .map_err(|e| RunError::Internal(EngineError::from_query(&e)))
            })
            .transpose()?;
        let extras = {
            let guard = self
                .extra_tables
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            guard
                .iter()
                .map(|(view, src)| (view.as_str().to_owned(), Arc::clone(src)))
                .collect::<HashMap<_, _>>()
        };
        let workers = self
            .engine
            .runtime
            .workers()
            .map_err(|e| RunError::Internal(EngineError::from_query(&e)))?;
        Ok(SqlEngineContext::new(
            Arc::clone(&self.engine.spans),
            Arc::clone(&self.engine.traces),
            Arc::clone(&self.engine.scores),
            Arc::clone(&self.engine.runtime.admission),
            workers,
            Arc::clone(&self.engine.runtime.spill),
            self.engine.visible_from,
            sql_window,
            extras,
        )
        .with_pool(self.engine.pool().cloned())
        .with_suspects(self.engine.suspects.clone())
        .with_target_partitions(self.engine.runtime.target_partitions))
    }

    /// Forces range-mode execution on `trace_list` (disabling list-mode optimizations).
    #[doc(hidden)]
    pub fn set_force_range_mode(&mut self, force: bool) {
        self.engine.set_force_range_mode(force);
    }

    /// Formats the operator metrics from the most recent read query executed on this context.
    #[doc(hidden)]
    pub fn format_last_metrics(&self) -> Option<String> {
        let metrics = self.engine.last_metrics()?;
        let profiles = metrics.snapshot();
        if profiles.is_empty() {
            return None;
        }
        let mut out = String::new();
        for p in profiles {
            let _ = writeln!(
                out,
                "  {}: batches={} rows={} elapsed_ns={}",
                p.name, p.batches_out, p.rows_out, p.elapsed_ns
            );
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::time::Duration;

    use cetra_column::Schema;
    use cetra_core::time_serde::parse_rfc3339;
    use cetra_query::QueryError;
    use cetra_query::exec::{ExecContext, PartitionStream, ScanSource, ScanSpec};
    use serde_json::{Map, Value};

    use super::*;
    use crate::sql::{SQL_MAX_ROWS, SqlRequest};
    use crate::test_support::{dataset_scores, dataset_spans, example_query, live_scores};

    const TIMEOUT: Duration = Duration::from_secs(30);

    #[derive(Debug)]
    struct RecordingScanSource {
        inner: Arc<dyn ScanSource>,
        specs: Mutex<Vec<ScanSpec>>,
    }

    impl RecordingScanSource {
        fn new(inner: Arc<dyn ScanSource>) -> Self {
            Self {
                inner,
                specs: Mutex::new(Vec::new()),
            }
        }

        fn specs(&self) -> Vec<ScanSpec> {
            self.specs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        fn clear(&self) {
            self.specs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clear();
        }
    }

    impl ScanSource for RecordingScanSource {
        fn schema(&self) -> Arc<Schema> {
            self.inner.schema()
        }

        fn projected_schema(&self, spec: &ScanSpec) -> Result<Arc<Schema>, QueryError> {
            self.inner.projected_schema(spec)
        }

        fn open_partitions(
            &self,
            spec: &ScanSpec,
            ctx: &ExecContext,
        ) -> Result<Vec<Box<dyn PartitionStream>>, QueryError> {
            self.specs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(spec.clone());
            self.inner.open_partitions(spec, ctx)
        }

        fn explain_summary(&self, spec: &ScanSpec) -> String {
            self.inner.explain_summary(spec)
        }
    }

    fn context() -> QueryContext {
        QueryContext::new(example_query(), None).expect("example context")
    }

    async fn sql_rows(ctx: &QueryContext, query: &str) -> Vec<Map<String, Value>> {
        ctx.sql_json(SqlRequest {
            query,
            params: &Map::new(),
            limit: SQL_MAX_ROWS,
            timeout: TIMEOUT,
        })
        .await
        .expect(query)
        .rows
    }

    async fn field_names(ctx: &QueryContext, table: &str) -> Vec<String> {
        let query = format!("SELECT * FROM {table}");
        ctx.sql_check(&query, &Map::new())
            .await
            .expect(table)
            .into_iter()
            .map(|c| c.name)
            .collect()
    }

    fn same_columns(actual: &[String], expected: &[&str]) {
        let expected: Vec<String> = expected.iter().map(|name| (*name).to_owned()).collect();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn traces_table_agrees_with_the_fixture() {
        let ctx = context();
        let rows = sql_rows(&ctx, "SELECT * FROM traces").await;
        let mut actual = BTreeMap::new();
        for row in rows {
            let id = row["trace_id"].as_str().expect("trace_id").to_owned();
            actual.insert(id, Value::Object(row));
        }

        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../fixtures/api/responses/traces-include-eval.json"
        ))
        .expect("fixture json");
        let expected = fixture["data"].as_array().expect("data");
        assert_eq!(actual.len(), expected.len());
        for trace in expected {
            let id = trace["trace_id"].as_str().expect("id");
            let row = actual.get(id).unwrap_or_else(|| panic!("missing {id}"));
            for field in ["start_time", "end_time"] {
                let got_ns = parse_rfc3339(row[field].as_str().expect("ts")).expect("ns");
                let exp_ns = parse_rfc3339(trace[field].as_str().expect("ts")).expect("ns");
                assert_eq!(got_ns, exp_ns, "{id} {field}");
            }
            for field in [
                "name",
                "status",
                "span_count",
                "error_count",
                "tags",
                "models",
            ] {
                assert_eq!(&row[field], &trace[field], "{id} {field}");
            }
            match (&row["total_tokens"], &trace["total_tokens"]) {
                (Value::Null, Value::Null) => {}
                (Value::Number(left), Value::Number(right)) => {
                    assert_eq!(left.as_i64(), right.as_i64(), "{id} total_tokens");
                }
                (left, right) => panic!("{id} total_tokens {left} vs {right}"),
            }
            match (&row["cost_total"], &trace["cost_total"]) {
                (Value::Null, Value::Null) => {}
                (Value::Number(left), Value::Number(right)) => {
                    let left = left.as_f64().expect("left cost");
                    let right = right.as_f64().expect("right cost");
                    assert!((left - right).abs() < 1e-12, "{id} cost {left} vs {right}");
                }
                (left, right) => panic!("{id} cost_total {left} vs {right}"),
            }
        }
    }

    #[tokio::test]
    async fn view_columns_follow_the_spec() {
        let ctx = context();
        let spans = field_names(&ctx, "spans").await;
        same_columns(
            &spans,
            &[
                "trace_id",
                "span_id",
                "parent_span_id",
                "name",
                "span_kind",
                "start_time",
                "end_time",
                "duration_ns",
                "duration_ms",
                "status",
                "status_message",
                "service_name",
                "session_id",
                "user_id",
                "environment",
                "release",
                "tags",
                "metadata",
                "provider",
                "request_model",
                "response_model",
                "model",
                "prompt_name",
                "prompt_version",
                "operation",
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
                "reasoning_tokens",
                "total_tokens",
                "cost_input",
                "cost_output",
                "cost_total",
                "cost_source",
                "ttft_ns",
                "request_params",
                "input",
                "input_format",
                "output",
                "output_format",
                "input_preview",
                "output_preview",
                "system_instructions",
                "tool_definitions",
                "attributes",
                "resource_attributes",
                "scope_name",
                "scope_version",
                "events",
                "links",
                "media_refs",
                "truncated",
                "conventions",
                "ingest_time",
                "schema_version",
            ],
        );
        let traces = field_names(&ctx, "traces").await;
        same_columns(
            &traces,
            &[
                "trace_id",
                "start_time",
                "end_time",
                "duration_ns",
                "duration_ms",
                "name",
                "root_span_id",
                "has_root",
                "status",
                "session_id",
                "user_id",
                "environment",
                "release",
                "tags",
                "metadata",
                "input_preview",
                "output_preview",
                "span_count",
                "error_count",
                "llm_count",
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
                "reasoning_tokens",
                "total_tokens",
                "cost_total",
                "models",
                "prompts",
            ],
        );
        let sessions = field_names(&ctx, "sessions").await;
        same_columns(
            &sessions,
            &[
                "session_id",
                "start_time",
                "end_time",
                "duration_ns",
                "duration_ms",
                "trace_count",
                "error_count",
                "total_tokens",
                "cost_total",
                "user_id",
                "tags",
            ],
        );
        let scores = field_names(&ctx, "scores").await;
        same_columns(
            &scores,
            &[
                "score_id",
                "target",
                "trace_id",
                "span_id",
                "session_id",
                "user_id",
                "name",
                "data_type",
                "value_num",
                "value_str",
                "reason",
                "source",
                "evaluator_id",
                "rule_id",
                "experiment_id",
                "dataset_item_id",
                "queue_id",
                "author_user_id",
                "metadata",
                "created_at",
                "updated_at",
                "deleted",
                "ingest_time",
            ],
        );
    }

    #[tokio::test]
    async fn trace_one_ids_and_duration() {
        let ctx = context();
        let rows = sql_rows(
            &ctx,
            "SELECT trace_id, root_span_id, duration_ns, duration_ms FROM traces WHERE name = 'support-agent'",
        )
        .await;
        let row = rows
            .iter()
            .find(|r| r["trace_id"] == "a1000000000000000000000000000001")
            .expect("trace 1 is named support-agent");
        assert_eq!(row["root_span_id"], "b100000000000011");
        let duration_ns = row["duration_ns"].as_i64().expect("duration_ns");
        assert_eq!(duration_ns, 4_200_000_000);
        let duration_ms = row["duration_ms"].as_f64().expect("duration_ms");
        assert!((duration_ms - (duration_ns as f64 / 1e6)).abs() < 1e-9);
    }

    #[tokio::test]
    async fn span_ids_are_lowercase_hex() {
        let ctx = context();
        let rows = sql_rows(&ctx, "SELECT span_id, parent_span_id FROM spans").await;
        let mut saw_null_parent = false;
        for row in rows {
            let span_id = row["span_id"].as_str().expect("span_id");
            assert_eq!(span_id.len(), 16, "{span_id}");
            assert!(
                span_id
                    .chars()
                    .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase())
            );
            match row["parent_span_id"].as_str() {
                None => saw_null_parent = true,
                Some(parent) => {
                    assert_eq!(parent.len(), 16, "{parent}");
                    assert!(
                        parent
                            .chars()
                            .all(|ch| ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase())
                    );
                }
            }
        }
        assert!(saw_null_parent);
    }

    #[tokio::test]
    async fn score_count_matches_live_rows() {
        let ctx = context();
        let live = live_scores(&dataset_scores()).len() as i64;
        let rows = sql_rows(&ctx, "SELECT count(*) AS n FROM scores").await;
        assert_eq!(rows[0]["n"].as_i64(), Some(live));
    }

    #[tokio::test]
    async fn time_window_filters_every_scan() {
        let from = parse_rfc3339("2026-01-01T10:05:00Z").expect("from");
        let to = parse_rfc3339("2026-01-01T10:20:00Z").expect("to");
        let window = TimeWindow::new(from, to).expect("window");
        let ctx = QueryContext::new(example_query(), Some(window)).expect("windowed");

        let traces = sql_rows(&ctx, "SELECT trace_id, start_time FROM traces").await;
        let mut ids = Vec::new();
        for row in &traces {
            let start = parse_rfc3339(row["start_time"].as_str().expect("start")).expect("ns");
            assert!((from..to).contains(&start), "{start}");
            ids.push(row["trace_id"].as_str().expect("id").to_owned());
        }
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../fixtures/api/responses/traces-include-eval.json"
        ))
        .expect("fixture");
        let mut expected: Vec<_> = fixture["data"]
            .as_array()
            .expect("data")
            .iter()
            .filter(|trace| {
                let start =
                    parse_rfc3339(trace["start_time"].as_str().expect("start")).expect("ns");
                (from..to).contains(&start)
            })
            .map(|trace| trace["trace_id"].as_str().expect("id").to_owned())
            .collect();
        ids.sort();
        expected.sort();
        assert_eq!(ids, expected);
        assert!(
            expected
                .iter()
                .all(|id| id != "a1000000000000000000000000000004")
        );

        let spans = dataset_spans();
        let span_count = spans
            .iter()
            .filter(|span| (from..to).contains(&span.start_time))
            .count() as i64;
        let counted = sql_rows(&ctx, "SELECT count(*) AS n FROM spans").await;
        assert_eq!(counted[0]["n"].as_i64(), Some(span_count));

        let scores = live_scores(&dataset_scores());
        let score_count = scores
            .iter()
            .filter(|score| (from..to).contains(&score.created_at))
            .count() as i64;
        let counted = sql_rows(&ctx, "SELECT count(*) AS n FROM scores").await;
        assert_eq!(counted[0]["n"].as_i64(), Some(score_count));

        // Verify scan-spec time-range bounds reach the underlying source on self-joins, subqueries, and CTEs.
        let base = example_query();
        let recorded_spans = Arc::new(RecordingScanSource::new(Arc::clone(&base.inner.spans)));
        let mut recorded_engine = base.inner.clone();
        recorded_engine.spans = Arc::clone(&recorded_spans) as Arc<dyn ScanSource>;
        let recorded = QueryContext::new(ProjectQuery::from_engine(recorded_engine), Some(window))
            .expect("recorded");

        let assert_bounds = |specs: &[ScanSpec], min_scans: usize| {
            let matching = specs
                .iter()
                .filter(|s| {
                    s.time_range.min_ns == Some(from) && s.time_range.max_ns == Some(to - 1)
                })
                .count();
            assert!(
                matching >= min_scans,
                "expected at least {min_scans} windowed scans, got {specs:?}"
            );
        };

        let self_join = sql_rows(
            &recorded,
            "SELECT count(*) AS n FROM spans a JOIN spans b ON a.span_id = b.span_id",
        )
        .await;
        assert_eq!(self_join[0]["n"].as_i64(), Some(span_count));
        assert_bounds(&recorded_spans.specs(), 2);

        recorded_spans.clear();
        let sub = sql_rows(
            &recorded,
            "SELECT count(*) AS n FROM traces WHERE trace_id IN (SELECT trace_id FROM spans)",
        )
        .await;
        assert_eq!(sub[0]["n"].as_i64(), Some(expected.len() as i64));
        assert_bounds(&recorded_spans.specs(), 1);

        recorded_spans.clear();
        let cte = sql_rows(
            &recorded,
            "WITH x AS (SELECT * FROM spans) SELECT count(*) AS n FROM x",
        )
        .await;
        assert_eq!(cte[0]["n"].as_i64(), Some(span_count));
        assert_bounds(&recorded_spans.specs(), 1);

        assert!(matches!(
            TimeWindow::new(5, 5),
            Err(RunError::InvalidRequest(_))
        ));
        assert!(matches!(
            TimeWindow::new(6, 5),
            Err(RunError::InvalidRequest(_))
        ));
    }

    /// The merged `start_time` of each trace of the dataset, ascending.
    fn trace_starts() -> Vec<i64> {
        let mut starts = BTreeMap::new();
        for span in dataset_spans() {
            starts
                .entry(span.trace_id)
                .and_modify(|start: &mut i64| *start = (*start).min(span.start_time))
                .or_insert(span.start_time);
        }
        let mut starts: Vec<i64> = starts.into_values().collect();
        starts.sort_unstable();
        starts
    }

    #[tokio::test]
    async fn visible_from_hides_older_spans_traces_and_scores() {
        let bound = trace_starts()[2];
        let spans = dataset_spans();
        let live = live_scores(&dataset_scores());
        let query = example_query().with_visible_from(Some(bound));
        let ctx = QueryContext::new(query, None).expect("bounded context");

        let visible_spans = spans.iter().filter(|s| s.start_time >= bound).count() as i64;
        let visible_traces = trace_starts().iter().filter(|s| **s >= bound).count() as i64;
        let visible_scores = live.iter().filter(|s| s.created_at >= bound).count() as i64;
        assert!(
            visible_spans < spans.len() as i64,
            "the bound must hide something"
        );
        let count = |rows: Vec<Map<String, Value>>| rows[0]["n"].as_i64();
        assert_eq!(
            count(sql_rows(&ctx, "SELECT count(*) AS n FROM spans").await),
            Some(visible_spans)
        );
        assert_eq!(
            count(sql_rows(&ctx, "SELECT count(*) AS n FROM traces").await),
            Some(visible_traces)
        );
        assert_eq!(
            count(sql_rows(&ctx, "SELECT count(*) AS n FROM scores").await),
            Some(visible_scores)
        );
        let sessions = sql_rows(&ctx, "SELECT count(*) AS n FROM sessions").await;
        let unbounded = QueryContext::new(example_query(), None).expect("context");
        let all_sessions = sql_rows(&unbounded, "SELECT count(*) AS n FROM sessions").await;
        assert!(count(sessions).expect("n") <= count(all_sessions).expect("n"));
    }

    #[tokio::test]
    async fn visible_from_and_the_window_intersect() {
        let bound = trace_starts()[2];
        let hour = 3_600_000_000_000_i64;
        let window = TimeWindow::new(bound - 24 * hour, bound + hour).expect("window");
        let query = example_query().with_visible_from(Some(bound));
        let ctx = QueryContext::new(query, Some(window)).expect("bounded context");

        let expected = dataset_spans()
            .iter()
            .filter(|s| (bound..bound + hour).contains(&s.start_time))
            .count();
        let rows = sql_rows(&ctx, "SELECT start_time FROM spans").await;
        for row in &rows {
            let start = parse_rfc3339(row["start_time"].as_str().expect("start")).expect("ns");
            assert!((bound..bound + hour).contains(&start), "{start}");
        }
        assert_eq!(rows.len(), expected);
    }

    #[tokio::test]
    async fn no_visible_from_changes_nothing() {
        let ctx = QueryContext::new(example_query(), None).expect("context");
        let rows = sql_rows(&ctx, "SELECT count(*) AS n FROM spans").await;
        assert_eq!(rows[0]["n"].as_i64(), Some(dataset_spans().len() as i64));
    }
}
