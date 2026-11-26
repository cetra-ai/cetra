//! The `dataset_items` and `experiment_items` SQL views (D93, D-J-15).
//!
//! The server builds a view's rows per query, only when the statement names it, and registers
//! it on the query's context. Storage owns the other views; the server owns these two and their
//! catalog in `GET /schema`.

use std::sync::Arc;

use cetra_column::{
    Batch, Bitmap, Column, ColumnError, DataType, Field, FlatColumn, FlatData, Schema, Utf8Data,
};
use cetra_core::ids::{OrgId, ProjectId, SpanId, TraceId};
use cetra_storage::{
    ColumnInfo, ColumnKind, QueryContext, RunError, SqliteView, TimeWindow, ViewColumns, ViewName,
};

use crate::http::AppState;
use crate::http::error::ApiError;
use crate::meta;
use crate::meta::datasets::{ViewItem, ViewItems};
use crate::meta::experiments::{ViewRow, ViewRows};

/// Most rows one view holds (D93).
pub(crate) const MAX_VIEW_ROWS: usize = 1_000_000;
/// Most text one view holds, summed over its JSON and error text columns (D-J-15).
pub(crate) const MAX_VIEW_TEXT_BYTES: usize = 256 * 1024 * 1024;

/// Registers each view that `sql` names (a case-insensitive substring test) on `ctx`. Nothing is
/// read for a view the statement does not name.
///
/// # Errors
///
/// `query_failed` (400) when a view is over its budget. An internal error when building or
/// registering the view fails.
pub(crate) async fn register(
    state: &AppState,
    ctx: &QueryContext,
    org: OrgId,
    project: ProjectId,
    sql: &str,
    window: Option<TimeWindow>,
) -> Result<(), ApiError> {
    register_with(
        state,
        ctx,
        org,
        project,
        sql,
        window,
        MAX_VIEW_ROWS,
        MAX_VIEW_TEXT_BYTES,
    )
    .await
}

/// [`register`] with the budgets as arguments, so tests can make a view too large.
#[allow(clippy::too_many_arguments)]
async fn register_with(
    state: &AppState,
    ctx: &QueryContext,
    org: OrgId,
    project: ProjectId,
    sql: &str,
    window: Option<TimeWindow>,
    max_rows: usize,
    max_text_bytes: usize,
) -> Result<(), ApiError> {
    // A case-insensitive substring test of the SQL text, before parsing. A view name inside a
    // comment or a string literal also builds the view. That is accepted: the view is only read
    // (one extra read), never wrong (D-J-15).
    let lower = sql.to_ascii_lowercase();
    if lower.contains("dataset_items") {
        let items =
            match meta::datasets::view_rows(state.meta.reader(), project, max_rows, max_text_bytes)
                .await?
            {
                ViewItems::Rows(rows) => rows,
                ViewItems::TooLarge => return Err(too_large("dataset_items")),
            };
        let (schema, batches) = dataset_items_batches(&items)?;
        ctx.register_sqlite_view(SqliteView::DatasetItems, schema, batches)
            .map_err(ApiError::Sql)?;
    }
    if lower.contains("experiment_items") {
        let visible = crate::experiments::visible_from(state, org);
        let window = window.map(|window| (window.from(), window.to()));
        let rows = match meta::experiments::view_rows(
            state.meta.reader(),
            project,
            visible,
            window,
            max_rows,
            max_text_bytes,
        )
        .await?
        {
            ViewRows::Rows(rows) => rows,
            ViewRows::TooLarge => return Err(too_large("experiment_items")),
        };
        let (schema, batches) = experiment_items_batches(&rows)?;
        ctx.register_sqlite_view(SqliteView::ExperimentItems, schema, batches)
            .map_err(ApiError::Sql)?;
    }
    Ok(())
}

/// The catalog of both views for `GET /schema`: `experiment_items`, then `dataset_items`.
pub(crate) fn catalog() -> Vec<ViewColumns> {
    vec![
        ViewColumns {
            view: ViewName::ExperimentItems,
            description: "One row per experiment result (item × repetition) of the project's visible experiments.",
            columns: vec![
                column("experiment_id", ColumnKind::String, false, "Experiment id."),
                column(
                    "experiment_name",
                    ColumnKind::String,
                    false,
                    "Experiment name.",
                ),
                column(
                    "dataset_id",
                    ColumnKind::String,
                    true,
                    "Dataset the experiment ran on; null once the dataset is deleted.",
                ),
                column(
                    "dataset_version",
                    ColumnKind::Integer,
                    false,
                    "Dataset version the experiment pinned.",
                ),
                column(
                    "dataset_item_id",
                    ColumnKind::String,
                    false,
                    "Dataset item id, stable across versions.",
                ),
                column(
                    "repetition",
                    ColumnKind::Integer,
                    false,
                    "Repetition number, from 0.",
                ),
                column(
                    "trace_id",
                    ColumnKind::String,
                    true,
                    "Trace of the task run, lowercase hex; null when none was reported.",
                ),
                column(
                    "input",
                    ColumnKind::Json,
                    false,
                    "Item input at the pinned version, JSON text.",
                ),
                column(
                    "expected_output",
                    ColumnKind::Json,
                    true,
                    "Expected output at the pinned version, JSON text; null when absent.",
                ),
                column(
                    "output",
                    ColumnKind::Json,
                    true,
                    "Task output, JSON text; null when the run failed.",
                ),
                column(
                    "error",
                    ColumnKind::String,
                    true,
                    "Error text of the failed run; null when the run did not fail.",
                ),
                column(
                    "latency_ns",
                    ColumnKind::Integer,
                    true,
                    "Task run time in nanoseconds, as the SDK measured it.",
                ),
                column(
                    "created_at",
                    ColumnKind::Timestamp,
                    false,
                    "Time the result was stored.",
                ),
            ],
        },
        ViewColumns {
            view: ViewName::DatasetItems,
            description: "Live items of each dataset at its current version.",
            columns: vec![
                column("dataset_id", ColumnKind::String, false, "Dataset id."),
                column("dataset_name", ColumnKind::String, false, "Dataset name."),
                column(
                    "dataset_version",
                    ColumnKind::Integer,
                    false,
                    "Current version of the dataset, the version these rows belong to.",
                ),
                column(
                    "item_id",
                    ColumnKind::String,
                    false,
                    "Dataset item id, stable across versions.",
                ),
                column("input", ColumnKind::Json, false, "Item input, JSON text."),
                column(
                    "expected_output",
                    ColumnKind::Json,
                    true,
                    "Expected output, JSON text; null when absent.",
                ),
                column(
                    "metadata",
                    ColumnKind::Json,
                    false,
                    "Item metadata, JSON text.",
                ),
                column(
                    "source_trace_id",
                    ColumnKind::String,
                    true,
                    "Trace the item was created from, lowercase hex; null when none.",
                ),
                column(
                    "source_span_id",
                    ColumnKind::String,
                    true,
                    "Span the item was created from, lowercase hex; null when none.",
                ),
                column(
                    "created_at",
                    ColumnKind::Timestamp,
                    false,
                    "Time the item revision was stored.",
                ),
            ],
        },
    ]
}

/// One column of [`catalog`].
fn column(
    name: &'static str,
    kind: ColumnKind,
    nullable: bool,
    description: &'static str,
) -> ColumnInfo {
    ColumnInfo {
        name,
        kind,
        nullable,
        description,
    }
}

/// The `query_failed` answer of a view over its budget.
fn too_large(view: &str) -> ApiError {
    ApiError::Sql(RunError::Failed(format!(
        "{view} is too large for the SQL console"
    )))
}

/// The `dataset_items` schema and batches.
fn dataset_items_batches(items: &[ViewItem]) -> Result<(Arc<Schema>, Vec<Batch>), ApiError> {
    let schema = Schema::try_new_arc(vec![
        Field::new("dataset_id", DataType::Utf8, false),
        Field::new("dataset_name", DataType::Utf8, false),
        Field::new("dataset_version", DataType::I64, false),
        Field::new("item_id", DataType::Utf8, false),
        Field::new("input", DataType::Utf8, false),
        Field::new("expected_output", DataType::Utf8, true),
        Field::new("metadata", DataType::Utf8, false),
        Field::new("source_trace_id", DataType::Utf8, true),
        Field::new("source_span_id", DataType::Utf8, true),
        Field::new("created_at", DataType::timestamp_utc(), false),
    ])
    .map_err(column_err)?;
    if items.is_empty() {
        return Ok((schema, Vec::new()));
    }
    let columns = vec![
        owned_strings(items, |i| Some(i.dataset_id.to_string()))?,
        borrowed_strings(items, |i| Some(i.dataset_name.as_str()))?,
        i64_col(items.iter().map(|i| i.dataset_version).collect())?,
        owned_strings(items, |i| Some(i.item_id.to_string()))?,
        borrowed_strings(items, |i| Some(i.input.as_str()))?,
        borrowed_strings(items, |i| i.expected_output.as_deref())?,
        borrowed_strings(items, |i| Some(i.metadata.as_str()))?,
        owned_strings(items, |i| {
            i.source_trace_id.map(|trace: TraceId| trace.to_string())
        })?,
        owned_strings(items, |i| {
            i.source_span_id.map(|span: SpanId| span.to_string())
        })?,
        ts_col(items.iter().map(|i| i.created_at).collect())?,
    ];
    build_batches(schema, columns, items.len())
}

/// The `experiment_items` schema and batches.
fn experiment_items_batches(rows: &[ViewRow]) -> Result<(Arc<Schema>, Vec<Batch>), ApiError> {
    let schema = Schema::try_new_arc(vec![
        Field::new("experiment_id", DataType::Utf8, false),
        Field::new("experiment_name", DataType::Utf8, false),
        Field::new("dataset_id", DataType::Utf8, true),
        Field::new("dataset_version", DataType::I64, false),
        Field::new("dataset_item_id", DataType::Utf8, false),
        Field::new("repetition", DataType::I64, false),
        Field::new("trace_id", DataType::Utf8, true),
        Field::new("input", DataType::Utf8, false),
        Field::new("expected_output", DataType::Utf8, true),
        Field::new("output", DataType::Utf8, true),
        Field::new("error", DataType::Utf8, true),
        Field::new("latency_ns", DataType::I64, true),
        Field::new("created_at", DataType::timestamp_utc(), false),
    ])
    .map_err(column_err)?;
    if rows.is_empty() {
        return Ok((schema, Vec::new()));
    }
    let columns = vec![
        owned_strings(rows, |r| Some(r.experiment_id.to_string()))?,
        borrowed_strings(rows, |r| Some(r.experiment_name.as_str()))?,
        owned_strings(rows, |r| r.dataset_id.map(|id| id.to_string()))?,
        i64_col(rows.iter().map(|r| r.dataset_version).collect())?,
        owned_strings(rows, |r| Some(r.dataset_item_id.to_string()))?,
        i64_col(rows.iter().map(|r| r.repetition).collect())?,
        owned_strings(rows, |r| r.trace_id.map(|trace: TraceId| trace.to_string()))?,
        borrowed_strings(rows, |r| Some(r.input.as_str()))?,
        borrowed_strings(rows, |r| r.expected_output.as_deref())?,
        borrowed_strings(rows, |r| r.output.as_deref())?,
        borrowed_strings(rows, |r| r.error.as_deref())?,
        opt_i64_col(rows.iter().map(|r| r.latency_ns))?,
        ts_col(rows.iter().map(|r| r.created_at).collect())?,
    ];
    build_batches(schema, columns, rows.len())
}

fn column_err(error: ColumnError) -> ApiError {
    ApiError::Internal(Box::new(error))
}

fn owned_strings<T>(rows: &[T], value: impl Fn(&T) -> Option<String>) -> Result<Column, ApiError> {
    let mut nulls = Bitmap::new_ones(rows.len());
    let mut any_null = false;
    let mut owned = Vec::with_capacity(rows.len());
    for (idx, row) in rows.iter().enumerate() {
        if let Some(text) = value(row) {
            owned.push(text);
        } else {
            let _ = nulls.set(idx, false);
            any_null = true;
            owned.push(String::new());
        }
    }
    let data = Utf8Data::try_from_slices(owned.iter().map(String::as_str)).map_err(column_err)?;
    let col =
        FlatColumn::try_new(FlatData::Utf8(data), any_null.then_some(nulls)).map_err(column_err)?;
    Ok(Column::Flat(col))
}

fn borrowed_strings<T>(rows: &[T], value: impl Fn(&T) -> Option<&str>) -> Result<Column, ApiError> {
    let mut nulls = Bitmap::new_ones(rows.len());
    let mut any_null = false;
    let mut slices = Vec::with_capacity(rows.len());
    for (idx, row) in rows.iter().enumerate() {
        if let Some(text) = value(row) {
            slices.push(text);
        } else {
            let _ = nulls.set(idx, false);
            any_null = true;
            slices.push("");
        }
    }
    let data = Utf8Data::try_from_slices(slices).map_err(column_err)?;
    let col =
        FlatColumn::try_new(FlatData::Utf8(data), any_null.then_some(nulls)).map_err(column_err)?;
    Ok(Column::Flat(col))
}

fn i64_col(values: Vec<i64>) -> Result<Column, ApiError> {
    let col = FlatColumn::try_new(FlatData::I64(values), None).map_err(column_err)?;
    Ok(Column::Flat(col))
}

fn opt_i64_col(iter: impl ExactSizeIterator<Item = Option<i64>>) -> Result<Column, ApiError> {
    let len = iter.len();
    let mut values = Vec::with_capacity(len);
    let mut nulls = Bitmap::new_ones(len);
    let mut any_null = false;
    for (idx, item) in iter.enumerate() {
        match item {
            Some(v) => values.push(v),
            None => {
                values.push(0);
                let _ = nulls.set(idx, false);
                any_null = true;
            }
        }
    }
    let col = FlatColumn::try_new(FlatData::I64(values), any_null.then_some(nulls))
        .map_err(column_err)?;
    Ok(Column::Flat(col))
}

fn ts_col(values: Vec<i64>) -> Result<Column, ApiError> {
    let col = FlatColumn::try_new(FlatData::TimestampNs { values, utc: true }, None)
        .map_err(column_err)?;
    Ok(Column::Flat(col))
}

fn build_batches(
    schema: Arc<Schema>,
    columns: Vec<Column>,
    len: usize,
) -> Result<(Arc<Schema>, Vec<Batch>), ApiError> {
    let batch = Batch::try_new_all(Arc::clone(&schema), columns, len).map_err(column_err)?;
    Ok((schema, vec![batch]))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::http::{Method, StatusCode};
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;
    use crate::routes::read::open_query_windowed;
    use crate::test_support::{Seeded, TestApp, TestResponse, set_limits};

    fn base(seeded: &Seeded) -> String {
        format!("/api/v1/projects/{}", seeded.project.id)
    }

    /// An `Authorization` value of a key with read and write on the seeded project. Keys do not
    /// expire with the manual clock, unlike the session cookie, so tests that advance it use one.
    async fn auth(app: &TestApp, seeded: &Seeded) -> String {
        format!("Bearer {}", app.write_key(seeded).await)
    }

    async fn post(app: &TestApp, auth: &str, path: &str, body: &Value) -> TestResponse {
        app.request(Method::POST, path)
            .header("authorization", auth)
            .json(body)
            .send()
            .await
    }

    /// Creates dataset `faq` with one item per input, and returns the item ids.
    async fn faq(app: &TestApp, auth: &str, seeded: &Seeded, inputs: &[Value]) -> Vec<Uuid> {
        let base = base(seeded);
        let created = post(
            app,
            auth,
            &format!("{base}/datasets"),
            &json!({"name": "faq"}),
        )
        .await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&created.body)
        );
        let items: Vec<Value> = inputs.iter().map(|input| json!({"input": input})).collect();
        let added = post(
            app,
            auth,
            &format!("{base}/datasets/faq/items"),
            &json!({"items": items}),
        )
        .await;
        assert_eq!(
            added.status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&added.body)
        );
        let body: Value = added.json();
        serde_json::from_value(body["item_ids"].clone()).unwrap()
    }

    /// Creates experiment `name` on `faq`, and returns its id.
    async fn experiment(app: &TestApp, auth: &str, seeded: &Seeded, name: &str) -> Uuid {
        experiment_with(app, auth, seeded, name, 1).await
    }

    /// Creates experiment `name` on `faq` with `repetitions`, and returns its id.
    async fn experiment_with(
        app: &TestApp,
        auth: &str,
        seeded: &Seeded,
        name: &str,
        repetitions: i64,
    ) -> Uuid {
        let response = post(
            app,
            auth,
            &format!("{}/experiments", base(seeded)),
            &json!({"name": name, "dataset": "faq", "repetitions": repetitions}),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let body: Value = response.json();
        body["experiment"]["id"].as_str().unwrap().parse().unwrap()
    }

    /// Stores `results` (`dataset_item_id`, `repetition`, `output`) on `experiment`.
    async fn results(app: &TestApp, auth: &str, seeded: &Seeded, experiment: Uuid, results: Value) {
        let response = post(
            app,
            auth,
            &format!("{}/experiments/{experiment}/items", base(seeded)),
            &json!({"results": results}),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
    }

    /// Runs `body` in the SQL console of the seeded project.
    async fn sql(app: &TestApp, auth: &str, seeded: &Seeded, body: Value) -> (StatusCode, Value) {
        let response = post(app, auth, &format!("{}/sql", base(seeded)), &body).await;
        (response.status, response.json())
    }

    /// The rows of a SQL answer.
    fn rows(body: &Value) -> Vec<Value> {
        body["rows"].as_array().unwrap().clone()
    }

    /// RFC 3339 text of `ns`, for a window bound.
    fn rfc3339(ns: i64) -> String {
        chrono::DateTime::from_timestamp_nanos(ns)
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dataset_items_lists_current_items() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let auth = auth(&app, &seeded).await;
        let ids = faq(&app, &auth, &seeded, &[json!("a1"), json!("b1")]).await;
        let edited = app
            .request(
                Method::PATCH,
                &format!("{}/datasets/faq/items/{}", base(&seeded), ids[0]),
            )
            .header("authorization", &auth)
            .json(&json!({"input": "a2"}))
            .send()
            .await;
        assert_eq!(edited.status, StatusCode::OK);

        let (status, body) = sql(
            &app,
            &auth,
            &seeded,
            json!({"query": "SELECT item_id, input FROM dataset_items ORDER BY item_id"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let rows = rows(&body);
        assert_eq!(rows.len(), 2, "{body}");
        let input_of = |id: Uuid| {
            rows.iter()
                .find(|row| row["item_id"] == json!(id.to_string()))
                .map(|row| row["input"].clone())
        };
        // The edited item shows its current revision only; the stored text is JSON.
        assert_eq!(input_of(ids[0]), Some(json!("\"a2\"")));
        assert_eq!(input_of(ids[1]), Some(json!("\"b1\"")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn experiment_items_lists_results() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let auth = auth(&app, &seeded).await;
        let ids = faq(&app, &auth, &seeded, &[json!("q0"), json!("q1")]).await;
        let baseline = experiment(&app, &auth, &seeded, "baseline").await;
        results(
            &app,
            &auth,
            &seeded,
            baseline,
            json!([
                {"dataset_item_id": ids[0], "repetition": 0, "output": "a"},
                {"dataset_item_id": ids[1], "repetition": 0, "output": "b"},
            ]),
        )
        .await;

        let (status, body) = sql(
            &app,
            &auth,
            &seeded,
            json!({"query": "SELECT experiment_name, repetition FROM experiment_items ORDER BY dataset_item_id"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            rows(&body),
            vec![
                json!({"experiment_name": "baseline", "repetition": 0}),
                json!({"experiment_name": "baseline", "repetition": 0}),
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn window_applies_to_experiment_items_only() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let auth = auth(&app, &seeded).await;
        let ids = faq(&app, &auth, &seeded, &[json!("q0"), json!("q1")]).await;
        let run = experiment_with(&app, &auth, &seeded, "run", 2).await;
        results(
            &app,
            &auth,
            &seeded,
            run,
            json!([{"dataset_item_id": ids[0], "repetition": 0, "output": "a"}]),
        )
        .await;
        app.state.clock.advance(Duration::from_secs(3600));
        let later = app.state.clock.now_ns();
        results(
            &app,
            &auth,
            &seeded,
            run,
            json!([{"dataset_item_id": ids[0], "repetition": 1, "output": "b"}]),
        )
        .await;

        let window = json!({
            "from": rfc3339(later - 60_000_000_000),
            "to": rfc3339(later + 60_000_000_000),
        });
        let mut experiment_query = window.clone();
        experiment_query["query"] = json!("SELECT repetition FROM experiment_items");
        let (status, body) = sql(&app, &auth, &seeded, experiment_query).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(rows(&body), vec![json!({"repetition": 1})]);

        // The window does not apply to dataset_items: both items stay.
        let mut dataset_query = window;
        dataset_query["query"] = json!("SELECT item_id FROM dataset_items");
        let (status, body) = sql(&app, &auth, &seeded, dataset_query).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(rows(&body).len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hidden_experiments_are_left_out() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let auth = auth(&app, &seeded).await;
        let ids = faq(&app, &auth, &seeded, &[json!("q0")]).await;
        let old = experiment(&app, &auth, &seeded, "old").await;
        results(
            &app,
            &auth,
            &seeded,
            old,
            json!([{"dataset_item_id": ids[0], "repetition": 0, "output": "a"}]),
        )
        .await;
        set_limits(
            &app,
            seeded.org_id,
            "2026-01-01T00:00:00Z",
            &json!({ "data_access_days": 30 }),
        );
        app.state.clock.advance(Duration::from_secs(31 * 24 * 3600));
        let new = experiment(&app, &auth, &seeded, "new").await;
        results(
            &app,
            &auth,
            &seeded,
            new,
            json!([{"dataset_item_id": ids[0], "repetition": 0, "output": "b"}]),
        )
        .await;

        let (status, body) = sql(
            &app,
            &auth,
            &seeded,
            json!({"query": "SELECT experiment_name FROM experiment_items"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(rows(&body), vec![json!({"experiment_name": "new"})]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn views_are_built_only_when_named() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let auth = auth(&app, &seeded).await;
        faq(&app, &auth, &seeded, &[json!("q0")]).await;
        let ctx = open_query_windowed(&app.state, seeded.org_id, seeded.project.id, None)
            .await
            .unwrap();
        // A statement that names no view reads no view rows, so a zero budget is no error.
        register_with(
            &app.state,
            &ctx,
            seeded.org_id,
            seeded.project.id,
            "SELECT 1 AS one",
            None,
            0,
            0,
        )
        .await
        .unwrap();
        // Naming the view reads it, and the same budget refuses it.
        let refused = register_with(
            &app.state,
            &ctx,
            seeded.org_id,
            seeded.project.id,
            "SELECT item_id FROM dataset_items",
            None,
            0,
            0,
        )
        .await;
        assert!(matches!(
            refused,
            Err(ApiError::Sql(RunError::Failed(ref message))) if message.contains("dataset_items")
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn too_large_views_are_query_failed() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let auth = auth(&app, &seeded).await;
        let ids = faq(&app, &auth, &seeded, &[json!("q0"), json!("q1")]).await;
        let run = experiment(&app, &auth, &seeded, "run").await;
        results(
            &app,
            &auth,
            &seeded,
            run,
            json!([{"dataset_item_id": ids[0], "repetition": 0, "output": "a"}]),
        )
        .await;
        let ctx = open_query_windowed(&app.state, seeded.org_id, seeded.project.id, None)
            .await
            .unwrap();
        let items = register_with(
            &app.state,
            &ctx,
            seeded.org_id,
            seeded.project.id,
            "SELECT * FROM dataset_items",
            None,
            1,
            MAX_VIEW_TEXT_BYTES,
        )
        .await;
        assert!(matches!(
            items,
            Err(ApiError::Sql(RunError::Failed(ref message))) if message == "dataset_items is too large for the SQL console"
        ));
        let text = register_with(
            &app.state,
            &ctx,
            seeded.org_id,
            seeded.project.id,
            "SELECT * FROM experiment_items",
            None,
            MAX_VIEW_ROWS,
            1,
        )
        .await;
        assert!(matches!(
            text,
            Err(ApiError::Sql(RunError::Failed(ref message))) if message == "experiment_items is too large for the SQL console"
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn schema_lists_the_server_views() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let response = app
            .request(Method::GET, &format!("{}/schema", base(&seeded)))
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK);
        let body: Value = response.json();
        let names: Vec<&str> = body["views"]
            .as_array()
            .unwrap()
            .iter()
            .map(|view| view["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "traces",
                "spans",
                "sessions",
                "scores",
                "experiment_items",
                "dataset_items"
            ]
        );
    }
}
