//! The reads that become `expected.json` (R8.6, plan § dump.rs): the public reads of `Storage`
//! and `reads`, with the request values `model_tests/observe.rs` uses, on an explicit window,
//! every page followed. A dump runs on a copy of a directory only.

use std::fs;
use std::future::Future;
use std::path::Path;
use std::time::Duration;

use arrow::array::RecordBatch;
use llmobs_core::ids::ProjectId;
use llmobs_core::schema::{scores, spans, traces};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqliteRow};
use sqlx::{Connection, Row, SqliteConnection};

use super::{
    CACHE_LOCATION, CACHE_NAME, CACHE_SIZE, DELETION_TRACE, DELETION_USER, GENERATOR, MEDIA, P1,
    P2, P3, ns_at, window,
};
use crate::format::FORMAT_VERSION;
use crate::{
    Bucket, BucketWidth, DeletionStatus, ListScope, MAX_PAGE, MetricsRequest, QueryContext,
    QueryOptions, ScoreListRequest, ScoreTargets, SessionListRequest, SessionSort, SortOrder,
    SpanListRequest, SpanSort, Storage, TimeWindow, TraceListRequest, TraceSort, UserListRequest,
    UserSort, latest_scores, metrics, score_list, session_list, span_list, trace_detail,
    trace_list, user_list,
};

/// Time allowed for each read request.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The window of a read, as an explicit `TimeWindow` (never the clock).
fn time_window() -> TimeWindow {
    let (from, to) = window();
    TimeWindow::new(from, to).expect("the golden window has from before to")
}

/// The scope of every list read: the golden window, evaluation and experiment rows kept.
fn scope() -> ListScope {
    ListScope {
        window: Some(time_window()),
        include_eval: true,
        include_experiments: true,
    }
}

/// A query context on `project`, as the server's read handlers build one.
async fn context(storage: &Storage, project: ProjectId) -> QueryContext {
    let query = storage
        .query(project, QueryOptions::new(None, None))
        .await
        .unwrap_or_else(|error| panic!("query a golden project: {error}"));
    QueryContext::new(query, Some(time_window())).unwrap_or_else(|error| panic!("context: {error}"))
}

/// The rows of `batches`, projected to `columns` by name, as JSON objects in order.
fn rows(batches: &[RecordBatch], columns: &[&str]) -> Vec<Value> {
    let mut out = Vec::new();
    for batch in batches {
        if batch.num_rows() == 0 {
            continue;
        }
        let indexes: Vec<usize> = columns
            .iter()
            .map(|name| {
                batch
                    .schema()
                    .index_of(name)
                    .unwrap_or_else(|_| panic!("the read result has a {name} column"))
            })
            .collect();
        let projected = batch
            .project(&indexes)
            .expect("project the columns of a read result");
        let mut writer = arrow::json::ArrayWriter::new(Vec::new());
        writer
            .write(&projected)
            .expect("write a read result as JSON");
        writer.finish().expect("finish the JSON of a read result");
        let Value::Array(values) =
            serde_json::from_slice(&writer.into_inner()).expect("the JSON of a read result")
        else {
            panic!("the JSON writer returns an array");
        };
        out.extend(values);
    }
    out
}

/// The `trace_id` values of JSON rows, in order.
fn ids_of(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .map(|row| {
            row["trace_id"]
                .as_str()
                .expect("a trace_id text")
                .to_owned()
        })
        .collect()
}

/// Sorts JSON rows by their text: for reads that do not order their rows.
fn sorted(mut rows: Vec<Value>) -> Vec<Value> {
    rows.sort_by_key(Value::to_string);
    rows
}

/// The answers of the public reads over the golden scenario, as JSON. Called on a COPY of the
/// directory only (see `expected_of` and `check_dir`).
pub(super) async fn dump(storage: &Storage) -> Value {
    let mut projects = Map::new();
    for project in [P1, P2] {
        let id: ProjectId = project.parse().expect("a golden project id");
        projects.insert(project.to_owned(), project_dump(storage, id).await);
    }

    // The dropped project: no trace is listed.
    let dropped_id: ProjectId = P3.parse().expect("a golden project id");
    let dropped_context = context(storage, dropped_id).await;
    let dropped_list = trace_list(
        &dropped_context,
        TraceListRequest {
            filter: None,
            search: None,
            scope: scope(),
            strict_exclusions: false,
            sort: TraceSort::StartTime,
            order: SortOrder::Desc,
            limit: MAX_PAGE,
            cursor: None,
            timeout: TIMEOUT,
        },
    )
    .await
    .unwrap_or_else(|error| panic!("trace_list of the dropped project: {error}"));
    let mut dropped = Map::new();
    dropped.insert(
        P3.to_owned(),
        json!({ "trace_list": rows(&dropped_list.batches, TRACE_COLUMNS) }),
    );

    let mut deletions = Map::new();
    for id in [DELETION_TRACE, DELETION_USER] {
        let status = match storage
            .deletion_status(id)
            .await
            .unwrap_or_else(|error| panic!("deletion_status of a golden deletion: {error}"))
        {
            DeletionStatus::Pending => "pending",
            DeletionStatus::Applied { .. } => "applied",
        };
        deletions.insert(id.to_string(), Value::from(status));
    }

    let sha = Sha256::digest(MEDIA);
    let media = storage
        .get_media(
            P1.parse().expect("a golden project id"),
            &sha.into(),
            None,
            None,
        )
        .await
        .unwrap_or_else(|error| panic!("get_media of the golden blob: {error}"));

    json!({
        "format_version": FORMAT_VERSION,
        "workspace_version": env!("CARGO_PKG_VERSION"),
        "generator": GENERATOR,
        "windows": { "from": window().0, "to": window().1 },
        "write_probe": { "project": P1, "day": "2026-02-11", "start_time": ns_at("2026-02-11", 3600) },
        "projects": projects,
        "dropped": dropped,
        "deletions": deletions,
        "media": {
            "project": P1,
            "sha256": hex::encode(sha),
            "bytes_len": media.bytes.len(),
        },
        "cache": { "name": CACHE_NAME, "location": CACHE_LOCATION, "size": CACHE_SIZE },
    })
}

/// The columns of a `traces` row the dump keeps.
const TRACE_COLUMNS: &[&str] = &[
    traces::TRACE_ID,
    traces::NAME,
    traces::START_TIME,
    traces::END_TIME,
    traces::SPAN_COUNT,
    traces::ERROR_COUNT,
    traces::TOTAL_TOKENS,
    traces::COST_TOTAL,
    traces::SESSION_ID,
    traces::USER_ID,
];

/// The columns of a `spans` row the dump keeps in lists.
const SPAN_COLUMNS: &[&str] = &[
    spans::TRACE_ID,
    spans::SPAN_ID,
    spans::START_TIME,
    spans::NAME,
    spans::SPAN_KIND,
    spans::STATUS,
    spans::TOTAL_TOKENS,
    spans::COST_TOTAL,
    spans::SESSION_ID,
    spans::USER_ID,
];

/// The columns of a span of a trace detail.
const DETAIL_COLUMNS: &[&str] = &[
    spans::SPAN_ID,
    spans::PARENT_SPAN_ID,
    spans::NAME,
    spans::SPAN_KIND,
    spans::STATUS,
    spans::MODEL,
    spans::TOTAL_TOKENS,
];

/// The columns of a `scores` row the dump keeps.
const SCORE_COLUMNS: &[&str] = &[
    scores::SCORE_ID,
    scores::NAME,
    scores::TARGET,
    scores::TRACE_ID,
    scores::SPAN_ID,
    scores::SESSION_ID,
    scores::VALUE_NUM,
    scores::VALUE_STR,
    scores::REASON,
    scores::CREATED_AT,
];

/// Everything the dump records about one project.
async fn project_dump(storage: &Storage, project: ProjectId) -> Value {
    let traces = all_traces(storage, project).await;
    let trace_ids = ids_of(&traces);

    let mut details = Map::new();
    let mut sorted_ids = trace_ids.clone();
    sorted_ids.sort();
    for id in &sorted_ids {
        let ctx = context(storage, project).await;
        let detail = trace_detail(&ctx, id, TIMEOUT)
            .await
            .unwrap_or_else(|error| panic!("trace_detail of a golden trace: {error}"))
            .expect("a listed trace has a detail");
        details.insert(
            id.clone(),
            json!({
                "truncated": detail.truncated,
                "spans": rows(&detail.spans, DETAIL_COLUMNS),
            }),
        );
    }

    let span_list = all_spans(storage, project).await;
    let scores = all_scores(storage, project).await;

    let ctx = context(storage, project).await;
    let trace_refs: Vec<&str> = sorted_ids.iter().map(String::as_str).collect();
    let latest = latest_scores(&ctx, ScoreTargets::Traces(&trace_refs), TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("latest_scores of golden traces: {error}"));
    let latest_rows = sorted(rows(
        &latest,
        &["target_key", "name", "score_id", "value_num"],
    ));

    let sessions = all_sessions(storage, project).await;
    let users = all_users(storage, project).await;

    let ctx = context(storage, project).await;
    let kpis = metrics(
        &ctx,
        MetricsRequest {
            window: time_window(),
            bucket: Bucket::Fixed(BucketWidth::D1),
            filter: None,
            group_by: None,
            include_eval: false,
            include_experiments: true,
            timeout: TIMEOUT,
        },
    )
    .await
    .unwrap_or_else(|error| panic!("metrics of a golden project: {error}"))
    .kpis
    .iter()
    .map(|kpi| json!({ "name": format!("{:?}", kpi.name), "value": kpi.value, "previous": kpi.previous }))
    .collect::<Vec<Value>>();

    json!({
        "traces": traces,
        "trace_details": details,
        "spans": span_list,
        "scores": scores,
        "latest_scores": latest_rows,
        "sessions": sessions,
        "users": users,
        "kpis": kpis,
    })
}

/// Every `trace_list` row over the window, in list order, every page followed.
async fn all_traces(storage: &Storage, project: ProjectId) -> Vec<Value> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let ctx = context(storage, project).await;
        let page = trace_list(
            &ctx,
            TraceListRequest {
                filter: None,
                search: None,
                scope: scope(),
                strict_exclusions: false,
                sort: TraceSort::StartTime,
                order: SortOrder::Desc,
                limit: MAX_PAGE,
                cursor: cursor.as_deref(),
                timeout: TIMEOUT,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("trace_list of a golden project: {error}"));
        out.extend(rows(&page.batches, TRACE_COLUMNS));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return out,
        }
    }
}

/// Every `span_list` row over the window, in list order, every page followed.
async fn all_spans(storage: &Storage, project: ProjectId) -> Vec<Value> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let ctx = context(storage, project).await;
        let page = span_list(
            &ctx,
            SpanListRequest {
                filter: None,
                search: None,
                scope: scope(),
                strict_exclusions: false,
                sort: SpanSort::StartTime,
                order: SortOrder::Asc,
                limit: MAX_PAGE,
                cursor: cursor.as_deref(),
                timeout: TIMEOUT,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("span_list of a golden project: {error}"));
        out.extend(rows(&page.batches, SPAN_COLUMNS));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return out,
        }
    }
}

/// Every live `score_list` row over the window, every page followed, sorted by text.
async fn all_scores(storage: &Storage, project: ProjectId) -> Vec<Value> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let ctx = context(storage, project).await;
        let page = score_list(
            &ctx,
            ScoreListRequest {
                target: None,
                trace_id: None,
                span_id: None,
                session_id: None,
                name: None,
                source: None,
                evaluator_id: None,
                rule_id: None,
                window: Some(time_window()),
                limit: MAX_PAGE,
                cursor: cursor.as_deref(),
                timeout: TIMEOUT,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("score_list of a golden project: {error}"));
        out.extend(rows(&page.batches, SCORE_COLUMNS));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return sorted(out),
        }
    }
}

/// Every `session_list` row over the window, sorted by text.
async fn all_sessions(storage: &Storage, project: ProjectId) -> Vec<Value> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let ctx = context(storage, project).await;
        let page = session_list(
            &ctx,
            SessionListRequest {
                filter: None,
                scope: scope(),
                sort: SessionSort::StartTime,
                order: SortOrder::Desc,
                limit: MAX_PAGE,
                cursor: cursor.as_deref(),
                timeout: TIMEOUT,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("session_list of a golden project: {error}"));
        out.extend(rows(
            &page.batches,
            &[
                traces::SESSION_ID,
                "trace_count",
                traces::ERROR_COUNT,
                traces::TOTAL_TOKENS,
                traces::COST_TOTAL,
            ],
        ));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return sorted(out),
        }
    }
}

/// Every `user_list` row over the window, sorted by text.
async fn all_users(storage: &Storage, project: ProjectId) -> Vec<Value> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let ctx = context(storage, project).await;
        let page = user_list(
            &ctx,
            UserListRequest {
                filter: None,
                scope: scope(),
                sort: UserSort::TraceCount,
                order: SortOrder::Desc,
                limit: MAX_PAGE,
                cursor: cursor.as_deref(),
                timeout: TIMEOUT,
            },
        )
        .await
        .unwrap_or_else(|error| panic!("user_list of a golden project: {error}"));
        out.extend(rows(&page.batches, &[traces::USER_ID, "trace_count"]));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return sorted(out),
        }
    }
}

/// Whether `trace` (hex) has a detail on `project` now: a read of one trace by id.
pub(super) async fn has_detail(storage: &Storage, project: ProjectId, trace: &str) -> bool {
    let ctx = context(storage, project).await;
    trace_detail(&ctx, trace, TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("trace_detail of a golden trace: {error}"))
        .is_some()
}

/// The KPIs of `expected` plus the contribution of the probe trace row `probe`: one trace, its
/// cost and its tokens. The probe has no error and lasts as long as every golden trace (3 ms), so
/// ErrorRate and LatencyP95 do not change.
pub(super) fn kpis_with_probe(expected: &Value, probe: &Value) -> Value {
    let cost = probe["cost_total"].as_f64().expect("the probe's cost");
    let tokens = probe["total_tokens"].as_f64().expect("the probe's tokens");
    let mut kpis = expected.clone();
    for kpi in kpis.as_array_mut().expect("the KPIs are a list") {
        let value = kpi["value"].as_f64().expect("a golden KPI has a value");
        let added = match kpi["name"].as_str().expect("a KPI name") {
            "Traces" => 1.0,
            "CostTotal" => cost,
            "TotalTokens" => tokens,
            _ => continue,
        };
        kpi["value"] = Value::from(value + added);
    }
    kpis
}

/// A current-thread runtime for the catalog reads, which are not async in this module.
pub(super) fn block_on<T>(future: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime for a catalog read")
        .block_on(future)
}

/// One cell of a catalog row: an integer, text or null.
fn cell(row: &SqliteRow, index: usize) -> Value {
    if let Ok(value) = row.try_get::<Option<i64>, _>(index) {
        return value.map_or(Value::Null, Value::from);
    }
    if let Ok(value) = row.try_get::<Option<String>, _>(index) {
        return value.map_or(Value::Null, Value::from);
    }
    panic!("a catalog column of an unexpected type")
}

/// The rows of `sql` on the catalog file `file`, each row an array of its cells. The file is
/// opened on its own: callers pass a copy, never a live directory.
pub(super) fn catalog_rows(file: &Path, sql: &'static str) -> Vec<Value> {
    let file = file.to_path_buf();
    block_on(async move {
        let options = SqliteConnectOptions::new().filename(file);
        let mut connection = SqliteConnection::connect_with(&options)
            .await
            .expect("open a copy of catalog.db");
        let found = sqlx::query(sql)
            .fetch_all(&mut connection)
            .await
            .expect("query a copy of catalog.db");
        // Closing a copy's connection fails only if it is already gone, which changes nothing read here.
        let _ = connection.close().await;
        found
            .iter()
            .map(|row| Value::Array((0..row.len()).map(|index| cell(row, index)).collect()))
            .collect()
    })
}

/// The single integer `sql` returns on the copy of the catalog `file`.
pub(super) fn catalog_count(file: &Path, sql: &'static str) -> i64 {
    let temp = tempfile::tempdir().expect("a temp dir for a catalog copy");
    let copy = temp.path().join("catalog.db");
    fs::copy(file, &copy).expect("copy catalog.db");
    let rows = catalog_rows(&copy, sql);
    rows.first()
        .and_then(|row| row[0].as_i64())
        .expect("the query returns one integer")
}

/// The catalog of the directory `dir`: its data files by table, level and state, the retention
/// watermarks, the projects and media rows, and the number of applied migrations. Read from a
/// copy of `catalog.db` before any `Storage` opens the directory. Not the flushed sequence, file
/// ids or timestamps: storage assigns those.
pub(super) fn catalog_summary(dir: &Path) -> Value {
    let temp = tempfile::tempdir().expect("a temp dir for a catalog copy");
    let copy = temp.path().join("catalog.db");
    fs::copy(dir.join("catalog.db"), &copy).expect("copy catalog.db");
    let migrations = catalog_rows(&copy, "SELECT COUNT(*) FROM _sqlx_migrations");
    json!({
        "data_files": catalog_rows(
            &copy,
            "SELECT table_name, level, state, COUNT(*) FROM data_files \
             GROUP BY table_name, level, state ORDER BY table_name, level, state",
        ),
        "retention_watermarks": catalog_rows(
            &copy,
            "SELECT project_id, keep_from FROM retention_watermarks ORDER BY project_id",
        ),
        "projects": catalog_rows(&copy, "SELECT project_id, state FROM projects ORDER BY project_id"),
        "media": catalog_rows(
            &copy,
            "SELECT project_id, sha256, mime_type, size_bytes FROM media ORDER BY project_id, sha256",
        ),
        "applied_migrations": migrations[0][0].clone(),
    })
}

/// The `expected.json` of the directory `dir`: the dump of a copy, plus its catalog summary
/// (taken from the copy before it opens). `dir` is not changed.
pub(super) fn expected_of(dir: &Path) -> Value {
    let temp = tempfile::tempdir().expect("a temp dir for the copy");
    let root = temp.path().join("golden");
    crate::test_support::copy_dir(dir, &root);
    let catalog = catalog_summary(&root);
    let config = super::build::golden_config(&root);
    let mut expected =
        crate::test_support::clean_life(&config, async |storage| dump(storage).await);
    expected
        .as_object_mut()
        .expect("the dump is an object")
        .insert("catalog".to_owned(), catalog);
    expected
}
