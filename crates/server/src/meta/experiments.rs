//! `experiments` and `experiment_items` rows (spec 04 § Datasets & experiments, D-J-10).

use std::collections::{HashMap, HashSet};
use std::fmt;

use futures::TryStreamExt;
use llmobs_core::ids::{ProjectId, TraceId};
use sqlx::{AssertSqlSafe, QueryBuilder, Sqlite, SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::MetaError;

/// Most ids one query binds in an `IN (…)` list. The callers stay under it; more is a server bug.
const MAX_IDS: usize = 1_000;
/// Results one multi-row `INSERT` writes. Eleven binds per row stay under SQLite's bind limit.
const UPSERT_CHUNK: usize = 50;

// The SQL text of this module is built from the constants here and fixed clauses only; every
// caller value is bound, never spliced in.
/// Columns of an experiment read, in the order [`ExperimentRecord`] declares them.
/// The result count is a subquery, so the SQL view never reads it (J3.6).
const EXPERIMENT_COLUMNS: &str = "e.id, e.project_id, e.dataset_id, e.dataset_name, \
    e.dataset_version, e.name, e.description, e.metadata, e.repetitions, e.item_count, \
    (SELECT count(*) FROM experiment_items i WHERE i.experiment_id = e.id) AS result_count, \
    e.status, e.error, e.created_by, e.created_at, e.finished_at";
/// Columns of a full result, in the order [`ResultRecord`] declares them.
const RESULT_COLUMNS: &str = "dataset_item_id, repetition, revision_id, input, \
    expected_output, trace_id, output, error, latency_ns, created_at";
/// Columns of the light projection, in the order [`KeyRecord`] declares them (D-J-13).
const KEY_COLUMNS: &str = "dataset_item_id, repetition, trace_id, \
    error IS NOT NULL AS has_error, latency_ns";
/// The view's experiment columns: no `result_count` subquery, which would run once per row.
const VIEW_COLUMNS: &str = "e.id, e.name, e.dataset_id, e.dataset_version, i.dataset_item_id, \
    i.repetition, i.trace_id, i.input, i.expected_output, i.output, i.error, i.latency_ns, \
    i.created_at";

/// One stored experiment.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExperimentRow {
    pub(crate) id: Uuid,
    pub(crate) project_id: ProjectId,
    /// `None` once the dataset is deleted (`ON DELETE SET NULL`).
    pub(crate) dataset_id: Option<Uuid>,
    pub(crate) dataset_name: String,
    pub(crate) dataset_version: i64,
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    /// JSON object text.
    pub(crate) metadata: String,
    pub(crate) repetitions: i64,
    pub(crate) item_count: i64,
    /// Rows in `experiment_items` (a subquery count).
    pub(crate) result_count: i64,
    /// `running`, `completed` or `failed`.
    pub(crate) status: String,
    pub(crate) error: Option<String>,
    pub(crate) created_by: Option<String>,
    pub(crate) created_at: i64,
    pub(crate) finished_at: Option<i64>,
}

/// A new experiment, before insert.
#[derive(Debug, Clone)]
pub(crate) struct NewExperiment {
    pub(crate) id: Uuid,
    pub(crate) project_id: ProjectId,
    pub(crate) dataset_id: Uuid,
    pub(crate) dataset_name: String,
    pub(crate) dataset_version: i64,
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) metadata: String,
    pub(crate) repetitions: i64,
    pub(crate) item_count: i64,
    pub(crate) created_by: Option<String>,
    pub(crate) created_at: i64,
}

/// One result to store. `input`, `expected_output` and `revision_id` come from the pinned
/// revision, never from the client (D-J-10). `Debug` is hand-written: it holds item content,
/// outputs and error texts.
#[derive(Clone, PartialEq)]
pub(crate) struct NewResult {
    pub(crate) dataset_item_id: Uuid,
    pub(crate) repetition: i64,
    pub(crate) revision_id: Uuid,
    /// JSON text.
    pub(crate) input: String,
    /// JSON text.
    pub(crate) expected_output: Option<String>,
    pub(crate) trace_id: Option<TraceId>,
    /// JSON text.
    pub(crate) output: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) latency_ns: Option<i64>,
}

/// One stored result row. `Debug` is hand-written (see [`NewResult`]).
#[derive(Clone, PartialEq)]
pub(crate) struct ResultRow {
    pub(crate) dataset_item_id: Uuid,
    pub(crate) repetition: i64,
    pub(crate) revision_id: Uuid,
    pub(crate) input: String,
    pub(crate) expected_output: Option<String>,
    pub(crate) trace_id: Option<TraceId>,
    pub(crate) output: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) latency_ns: Option<i64>,
    pub(crate) created_at: i64,
}

/// The light projection of one result: what the aggregation needs, without the text columns
/// (D-J-13). 20,000 of these stay small.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResultKey {
    pub(crate) dataset_item_id: Uuid,
    pub(crate) repetition: i64,
    pub(crate) trace_id: Option<TraceId>,
    /// `error IS NOT NULL`.
    pub(crate) has_error: bool,
    pub(crate) latency_ns: Option<i64>,
}

/// One row of the `experiment_items` SQL view (J3.6). `Debug` is hand-written.
#[derive(Clone, PartialEq)]
pub(crate) struct ViewRow {
    pub(crate) experiment_id: Uuid,
    pub(crate) experiment_name: String,
    pub(crate) dataset_id: Option<Uuid>,
    pub(crate) dataset_version: i64,
    pub(crate) dataset_item_id: Uuid,
    pub(crate) repetition: i64,
    pub(crate) trace_id: Option<TraceId>,
    pub(crate) input: String,
    pub(crate) expected_output: Option<String>,
    pub(crate) output: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) latency_ns: Option<i64>,
    pub(crate) created_at: i64,
}

/// The pinned revision of one item, for [`NewResult`]. `Debug` is hand-written.
#[derive(Clone, PartialEq)]
pub(crate) struct PinnedItem {
    pub(crate) revision_id: Uuid,
    /// JSON text.
    pub(crate) input: String,
    /// JSON text.
    pub(crate) expected_output: Option<String>,
}

/// What `view_rows` returns.
pub(crate) enum ViewRows {
    Rows(Vec<ViewRow>),
    /// More than `max_rows` rows or more than `max_text_bytes` of text (D-J-15).
    TooLarge,
}

impl ViewRow {
    /// Bytes of the four text columns, the measure `view_rows` caps (D-J-15).
    fn text_bytes(&self) -> usize {
        self.input.len()
            + self.expected_output.as_ref().map_or(0, String::len)
            + self.output.as_ref().map_or(0, String::len)
            + self.error.as_ref().map_or(0, String::len)
    }
}

impl fmt::Debug for NewResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewResult")
            .field("dataset_item_id", &self.dataset_item_id)
            .field("repetition", &self.repetition)
            .field("trace_id", &self.trace_id)
            .field("input_len", &self.input.len())
            .field(
                "expected_output_len",
                &self.expected_output.as_ref().map(String::len),
            )
            .field("output_len", &self.output.as_ref().map(String::len))
            .field("error_len", &self.error.as_ref().map(String::len))
            .field("latency_ns", &self.latency_ns)
            .finish()
    }
}

impl fmt::Debug for ResultRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResultRow")
            .field("dataset_item_id", &self.dataset_item_id)
            .field("repetition", &self.repetition)
            .field("revision_id", &self.revision_id)
            .field("trace_id", &self.trace_id)
            .field("input_len", &self.input.len())
            .field(
                "expected_output_len",
                &self.expected_output.as_ref().map(String::len),
            )
            .field("output_len", &self.output.as_ref().map(String::len))
            .field("error_len", &self.error.as_ref().map(String::len))
            .field("latency_ns", &self.latency_ns)
            .finish()
    }
}

impl fmt::Debug for ViewRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ViewRow")
            .field("experiment_id", &self.experiment_id)
            .field("dataset_item_id", &self.dataset_item_id)
            .field("repetition", &self.repetition)
            .field("trace_id", &self.trace_id)
            .field("input_len", &self.input.len())
            .field(
                "expected_output_len",
                &self.expected_output.as_ref().map(String::len),
            )
            .field("output_len", &self.output.as_ref().map(String::len))
            .field("error_len", &self.error.as_ref().map(String::len))
            .field("latency_ns", &self.latency_ns)
            .finish()
    }
}

impl fmt::Debug for PinnedItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinnedItem")
            .field("revision_id", &self.revision_id)
            .field("input_len", &self.input.len())
            .field(
                "expected_output_len",
                &self.expected_output.as_ref().map(String::len),
            )
            .finish()
    }
}

/// An `experiments` row as SQLite returns it.
#[derive(sqlx::FromRow)]
struct ExperimentRecord {
    id: String,
    project_id: String,
    dataset_id: Option<String>,
    dataset_name: String,
    dataset_version: i64,
    name: String,
    description: Option<String>,
    metadata: String,
    repetitions: i64,
    item_count: i64,
    result_count: i64,
    status: String,
    error: Option<String>,
    created_by: Option<String>,
    created_at: i64,
    finished_at: Option<i64>,
}

/// An `experiment_items` row as SQLite returns it.
#[derive(sqlx::FromRow)]
struct ResultRecord {
    dataset_item_id: String,
    repetition: i64,
    revision_id: String,
    input: String,
    expected_output: Option<String>,
    trace_id: Option<Vec<u8>>,
    output: Option<String>,
    error: Option<String>,
    latency_ns: Option<i64>,
    created_at: i64,
}

/// The light projection as SQLite returns it. `has_error` is `0` or `1`.
#[derive(sqlx::FromRow)]
struct KeyRecord {
    dataset_item_id: String,
    repetition: i64,
    trace_id: Option<Vec<u8>>,
    has_error: i64,
    latency_ns: Option<i64>,
}

/// One row of the SQL view as SQLite returns it.
#[derive(sqlx::FromRow)]
struct ViewRecord {
    id: String,
    name: String,
    dataset_id: Option<String>,
    dataset_version: i64,
    dataset_item_id: String,
    repetition: i64,
    trace_id: Option<Vec<u8>>,
    input: String,
    expected_output: Option<String>,
    output: Option<String>,
    error: Option<String>,
    latency_ns: Option<i64>,
    created_at: i64,
}

/// The pinned revision of an item as SQLite returns it.
#[derive(sqlx::FromRow)]
struct PinnedRecord {
    item_id: String,
    revision_id: String,
    input: String,
    expected_output: Option<String>,
}

/// Inserts a `running` experiment.
pub(crate) async fn insert_tx(
    conn: &mut SqliteConnection,
    new: &NewExperiment,
) -> Result<(), MetaError> {
    sqlx::query(
        "INSERT INTO experiments (id, project_id, dataset_id, dataset_name, dataset_version, name, \
         description, metadata, repetitions, item_count, status, error, created_by, created_at, \
         finished_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'running', NULL, ?, ?, NULL)",
    )
    .bind(new.id.to_string())
    .bind(new.project_id.to_string())
    .bind(new.dataset_id.to_string())
    .bind(new.dataset_name.as_str())
    .bind(new.dataset_version)
    .bind(new.name.as_str())
    .bind(new.description.as_deref())
    .bind(new.metadata.as_str())
    .bind(new.repetitions)
    .bind(new.item_count)
    .bind(new.created_by.as_deref())
    .bind(new.created_at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The experiment `id` of `project`, visible at `visible_from` (`created_at >= visible_from`
/// when set). `None` when absent or hidden.
pub(crate) async fn get(
    pool: &SqlitePool,
    project: ProjectId,
    id: Uuid,
    visible_from: Option<i64>,
) -> Result<Option<ExperimentRow>, MetaError> {
    let mut builder = get_query(project, id, visible_from);
    let record: Option<ExperimentRecord> = builder.build_query_as().fetch_optional(pool).await?;
    record.map(experiment_from).transpose()
}

/// The same read inside a write transaction.
pub(crate) async fn get_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
    visible_from: Option<i64>,
) -> Result<Option<ExperimentRow>, MetaError> {
    let mut builder = get_query(project, id, visible_from);
    let record: Option<ExperimentRecord> =
        builder.build_query_as().fetch_optional(&mut *conn).await?;
    record.map(experiment_from).transpose()
}

/// One page of `project`'s visible experiments, newest first (`ORDER BY e.id DESC`; ids are
/// UUIDv7, so this is creation order). `dataset` keeps one dataset's experiments. `cursor` is the
/// last id of the previous page (`e.id < ?`). Returns the rows and the next cursor (the last
/// row's id when one more row exists).
pub(crate) async fn list(
    pool: &SqlitePool,
    project: ProjectId,
    dataset: Option<Uuid>,
    visible_from: Option<i64>,
    limit: u32,
    cursor: Option<Uuid>,
) -> Result<(Vec<ExperimentRow>, Option<Uuid>), MetaError> {
    let mut builder = QueryBuilder::<Sqlite>::new(format!(
        "SELECT {EXPERIMENT_COLUMNS} FROM experiments e WHERE e.project_id = "
    ));
    builder.push_bind(project.to_string());
    if let Some(dataset) = dataset {
        builder
            .push(" AND e.dataset_id = ")
            .push_bind(dataset.to_string());
    }
    push_visible(&mut builder, visible_from);
    if let Some(cursor) = cursor {
        builder.push(" AND e.id < ").push_bind(cursor.to_string());
    }
    builder
        .push(" ORDER BY e.id DESC LIMIT ")
        .push_bind(i64::from(limit) + 1);

    let records: Vec<ExperimentRecord> = builder.build_query_as().fetch_all(pool).await?;
    let mut rows = records
        .into_iter()
        .map(experiment_from)
        .collect::<Result<Vec<_>, _>>()?;
    let page = limit as usize;
    let next = if rows.len() > page {
        rows.truncate(page);
        rows.last().map(|row| row.id)
    } else {
        None
    };
    Ok((rows, next))
}

/// The visible experiments of `project` among `ids`, in no order.
pub(crate) async fn get_many(
    pool: &SqlitePool,
    project: ProjectId,
    ids: &[Uuid],
    visible_from: Option<i64>,
) -> Result<Vec<ExperimentRow>, MetaError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    check_ids(ids)?;
    let mut builder = QueryBuilder::<Sqlite>::new(format!(
        "SELECT {EXPERIMENT_COLUMNS} FROM experiments e WHERE e.project_id = "
    ));
    builder.push_bind(project.to_string());
    push_visible(&mut builder, visible_from);
    builder.push(" AND e.id IN (");
    push_ids(&mut builder, ids);
    let records: Vec<ExperimentRecord> = builder.build_query_as().fetch_all(pool).await?;
    records.into_iter().map(experiment_from).collect()
}

/// The ids among `ids` that are experiments of `project` (any status, hidden or not), for the
/// score write check (D-J-12).
pub(crate) async fn existing_ids(
    pool: &SqlitePool,
    project: ProjectId,
    ids: &[Uuid],
) -> Result<HashSet<Uuid>, MetaError> {
    if ids.is_empty() {
        return Ok(HashSet::new());
    }
    check_ids(ids)?;
    let mut builder = QueryBuilder::<Sqlite>::new("SELECT id FROM experiments WHERE project_id = ");
    builder.push_bind(project.to_string()).push(" AND id IN (");
    push_ids(&mut builder, ids);
    let texts: Vec<String> = builder.build_query_scalar().fetch_all(pool).await?;
    texts
        .iter()
        .map(|text| parse_uuid(text, "experiments", "id"))
        .collect()
}

/// Live items of `dataset` at `version` (D-J-3's validity ranges), inside a write transaction
/// (the create route counts and inserts in one transaction, D-J-10).
pub(crate) async fn count_version_items_tx(
    conn: &mut SqliteConnection,
    dataset: Uuid,
    version: i64,
) -> Result<i64, MetaError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM dataset_items WHERE dataset_id = ?1 AND valid_from_version <= ?2 \
         AND (valid_to_version IS NULL OR valid_to_version > ?2)",
    )
    .bind(dataset.to_string())
    .bind(version)
    .fetch_one(&mut *conn)
    .await?;
    Ok(count)
}

/// The pinned revisions of `items` in `dataset` at `version`, keyed by item id. An item with no
/// live revision at `version` is absent.
pub(crate) async fn pinned_items_tx(
    conn: &mut SqliteConnection,
    dataset: Uuid,
    version: i64,
    items: &[Uuid],
) -> Result<HashMap<Uuid, PinnedItem>, MetaError> {
    if items.is_empty() {
        return Ok(HashMap::new());
    }
    check_ids(items)?;
    let mut builder = QueryBuilder::<Sqlite>::new(
        "SELECT item_id, revision_id, input, expected_output FROM dataset_items \
         WHERE dataset_id = ",
    );
    builder
        .push_bind(dataset.to_string())
        .push(" AND valid_from_version <= ")
        .push_bind(version)
        .push(" AND (valid_to_version IS NULL OR valid_to_version > ")
        .push_bind(version)
        .push(") AND item_id IN (");
    push_ids(&mut builder, items);
    let records: Vec<PinnedRecord> = builder.build_query_as().fetch_all(&mut *conn).await?;
    records
        .into_iter()
        .map(|record| {
            let item = parse_uuid(&record.item_id, "dataset_items", "item_id")?;
            let pinned = PinnedItem {
                revision_id: parse_uuid(&record.revision_id, "dataset_items", "revision_id")?,
                input: record.input,
                expected_output: record.expected_output,
            };
            Ok((item, pinned))
        })
        .collect()
}

/// Inserts or replaces results of `experiment` (one row per item and repetition, D-J-10).
/// `now` is each replaced or new row's `created_at`. Returns the rows written.
pub(crate) async fn upsert_results_tx(
    conn: &mut SqliteConnection,
    experiment: Uuid,
    results: &[NewResult],
    now: i64,
) -> Result<u64, MetaError> {
    let mut written = 0;
    for chunk in results.chunks(UPSERT_CHUNK) {
        let mut builder = QueryBuilder::<Sqlite>::new(
            "INSERT INTO experiment_items (experiment_id, dataset_item_id, repetition, revision_id, \
             input, expected_output, trace_id, output, error, latency_ns, created_at) ",
        );
        builder.push_values(chunk, |mut row, result| {
            row.push_bind(experiment.to_string())
                .push_bind(result.dataset_item_id.to_string())
                .push_bind(result.repetition)
                .push_bind(result.revision_id.to_string())
                .push_bind(result.input.clone())
                .push_bind(result.expected_output.clone())
                .push_bind(result.trace_id.as_ref().map(trace_bytes))
                .push_bind(result.output.clone())
                .push_bind(result.error.clone())
                .push_bind(result.latency_ns)
                .push_bind(now);
        });
        builder.push(
            " ON CONFLICT (experiment_id, dataset_item_id, repetition) DO UPDATE SET \
             revision_id = excluded.revision_id, input = excluded.input, \
             expected_output = excluded.expected_output, trace_id = excluded.trace_id, \
             output = excluded.output, error = excluded.error, latency_ns = excluded.latency_ns, \
             created_at = excluded.created_at",
        );
        let done = builder.build().execute(&mut *conn).await?;
        written += done.rows_affected();
    }
    Ok(written)
}

/// Sets `completed` (no `error`) or `failed` (with it) and `finished_at = now` when the
/// experiment is `running`. `Ok(false)` when it is not running.
pub(crate) async fn finish_tx(
    conn: &mut SqliteConnection,
    id: Uuid,
    error: Option<&str>,
    now: i64,
) -> Result<bool, MetaError> {
    let status = if error.is_some() {
        "failed"
    } else {
        "completed"
    };
    let result = sqlx::query(
        "UPDATE experiments SET status = ?, error = ?, finished_at = ? \
         WHERE id = ? AND status = 'running'",
    )
    .bind(status)
    .bind(error)
    .bind(now)
    .bind(id.to_string())
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Deletes the experiment `id` of `project` and its results (FK cascade). False when absent.
pub(crate) async fn delete_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
) -> Result<bool, MetaError> {
    let result = sqlx::query("DELETE FROM experiments WHERE id = ? AND project_id = ?")
        .bind(id.to_string())
        .bind(project.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Rows removed by [`delete_copies_of_tx`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cascade {
    /// Experiment results removed: about a trace in the scope, or whose snapshot is a deleted
    /// dataset item revision.
    pub(crate) experiment_items: u64,
    /// Dataset item revisions removed, of every version.
    pub(crate) dataset_item_revisions: u64,
}

/// D-J-9: deletes, in `project`, the experiment results whose trace is in the deletion's scope,
/// the results whose snapshot came from a dataset item copied from a trace in the scope, and
/// those dataset item revisions (all versions). Reads the scope from `deletion_traces`, inside
/// the caller's write transaction, after `insert_traces` stored it.
pub(crate) async fn delete_copies_of_tx(
    conn: &mut SqliteConnection,
    deletion: Uuid,
    project: ProjectId,
) -> Result<Cascade, MetaError> {
    let project = project.to_string();
    let deletion = deletion.to_string();
    // The results first: their snapshot's revision is one of the items deleted by the third.
    let by_revision = sqlx::query(
        "DELETE FROM experiment_items \
         WHERE experiment_id IN (SELECT id FROM experiments WHERE project_id = ?) \
           AND revision_id IN ( \
             SELECT i.revision_id FROM dataset_items i JOIN datasets d ON d.id = i.dataset_id \
             WHERE d.project_id = ? \
               AND i.source_trace_id IN (SELECT trace_id FROM deletion_traces WHERE deletion_id = ?))",
    )
    .bind(&project)
    .bind(&project)
    .bind(&deletion)
    .execute(&mut *conn)
    .await?;
    let by_trace = sqlx::query(
        "DELETE FROM experiment_items \
         WHERE experiment_id IN (SELECT id FROM experiments WHERE project_id = ?) \
           AND trace_id IN (SELECT trace_id FROM deletion_traces WHERE deletion_id = ?)",
    )
    .bind(&project)
    .bind(&deletion)
    .execute(&mut *conn)
    .await?;
    let revisions = sqlx::query(
        "DELETE FROM dataset_items \
         WHERE dataset_id IN (SELECT id FROM datasets WHERE project_id = ?) \
           AND source_trace_id IN (SELECT trace_id FROM deletion_traces WHERE deletion_id = ?)",
    )
    .bind(&project)
    .bind(&deletion)
    .execute(&mut *conn)
    .await?;
    Ok(Cascade {
        experiment_items: by_revision.rows_affected() + by_trace.rows_affected(),
        dataset_item_revisions: revisions.rows_affected(),
    })
}

/// The light projection of every result of `experiment`, ordered by `dataset_item_id`, then
/// `repetition`. At most `MAX_EXPERIMENT_ROWS` rows exist (the create check bounds them). No
/// text column is read (D-J-13).
pub(crate) async fn result_keys(
    pool: &SqlitePool,
    experiment: Uuid,
) -> Result<Vec<ResultKey>, MetaError> {
    let sql = format!(
        "SELECT {KEY_COLUMNS} FROM experiment_items WHERE experiment_id = ? \
         ORDER BY dataset_item_id, repetition"
    );
    let records: Vec<KeyRecord> = sqlx::query_as(AssertSqlSafe(sql))
        .bind(experiment.to_string())
        .fetch_all(pool)
        .await?;
    records.into_iter().map(key_from).collect()
}

/// One page of full results of `experiment` in `(dataset_item_id, repetition)` order, strictly
/// after `after` when set; `errors_only` keeps rows with an error. At most `limit` rows; the
/// caller asks for one more than its page to know whether a next page exists.
pub(crate) async fn results_page(
    pool: &SqlitePool,
    experiment: Uuid,
    after: Option<(Uuid, i64)>,
    errors_only: bool,
    limit: i64,
) -> Result<Vec<ResultRow>, MetaError> {
    let mut builder = QueryBuilder::<Sqlite>::new(format!(
        "SELECT {RESULT_COLUMNS} FROM experiment_items WHERE experiment_id = "
    ));
    builder.push_bind(experiment.to_string());
    if let Some((item, repetition)) = after {
        builder
            .push(" AND (dataset_item_id, repetition) > (")
            .push_bind(item.to_string())
            .push(", ")
            .push_bind(repetition)
            .push(")");
    }
    if errors_only {
        builder.push(" AND error IS NOT NULL");
    }
    builder
        .push(" ORDER BY dataset_item_id, repetition LIMIT ")
        .push_bind(limit);
    let records: Vec<ResultRecord> = builder.build_query_as().fetch_all(pool).await?;
    records.into_iter().map(result_from).collect()
}

/// Every full result (all repetitions) of `experiment` for the given items, ordered by
/// `dataset_item_id`, then `repetition`. At most 1,000 items (a compare page).
pub(crate) async fn item_rows(
    pool: &SqlitePool,
    experiment: Uuid,
    items: &[Uuid],
) -> Result<Vec<ResultRow>, MetaError> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    check_ids(items)?;
    let mut builder = QueryBuilder::<Sqlite>::new(format!(
        "SELECT {RESULT_COLUMNS} FROM experiment_items WHERE experiment_id = "
    ));
    builder.push_bind(experiment.to_string());
    builder.push(" AND dataset_item_id IN (");
    push_ids(&mut builder, items);
    builder.push(" ORDER BY dataset_item_id, repetition");
    let records: Vec<ResultRecord> = builder.build_query_as().fetch_all(pool).await?;
    records.into_iter().map(result_from).collect()
}

/// Every result row of the visible experiments of `project`, for the `experiment_items` SQL
/// view (J3.6). `window` keeps rows with `created_at ∈ [from, to)`. Rows are streamed; reading
/// stops with `TooLarge` as soon as the rows exceed `max_rows` or the summed byte lengths of
/// their text exceed `max_text_bytes`.
pub(crate) async fn view_rows(
    pool: &SqlitePool,
    project: ProjectId,
    visible_from: Option<i64>,
    window: Option<(i64, i64)>,
    max_rows: usize,
    max_text_bytes: usize,
) -> Result<ViewRows, MetaError> {
    let mut builder = QueryBuilder::<Sqlite>::new(format!(
        "SELECT {VIEW_COLUMNS} FROM experiment_items i \
         JOIN experiments e ON e.id = i.experiment_id WHERE e.project_id = "
    ));
    builder.push_bind(project.to_string());
    push_visible(&mut builder, visible_from);
    if let Some((from, to)) = window {
        builder
            .push(" AND i.created_at >= ")
            .push_bind(from)
            .push(" AND i.created_at < ")
            .push_bind(to);
    }
    builder.push(" ORDER BY e.id, i.dataset_item_id, i.repetition");

    let mut records = builder.build_query_as::<ViewRecord>().fetch(pool);
    let mut rows = Vec::new();
    let mut text_bytes = 0_usize;
    while let Some(record) = records.try_next().await? {
        let row = view_from(record)?;
        if rows.len() >= max_rows {
            return Ok(ViewRows::TooLarge);
        }
        text_bytes = text_bytes.saturating_add(row.text_bytes());
        if text_bytes > max_text_bytes {
            return Ok(ViewRows::TooLarge);
        }
        rows.push(row);
    }
    Ok(ViewRows::Rows(rows))
}

/// The experiment `id` of `project` as a builder, with the visibility clause of `visible_from`.
fn get_query(project: ProjectId, id: Uuid, visible_from: Option<i64>) -> QueryBuilder<Sqlite> {
    let mut builder = QueryBuilder::<Sqlite>::new(format!(
        "SELECT {EXPERIMENT_COLUMNS} FROM experiments e WHERE e.project_id = "
    ));
    builder
        .push_bind(project.to_string())
        .push(" AND e.id = ")
        .push_bind(id.to_string());
    push_visible(&mut builder, visible_from);
    builder
}

/// Appends the visibility clause of an experiment read (D-J-14). Callers alias `experiments e`.
fn push_visible(builder: &mut QueryBuilder<Sqlite>, visible_from: Option<i64>) {
    if let Some(from) = visible_from {
        builder.push(" AND e.created_at >= ").push_bind(from);
    }
}

/// Appends the bound ids of an `IN (` the caller opened, and the closing `)`.
fn push_ids(builder: &mut QueryBuilder<Sqlite>, ids: &[Uuid]) {
    let mut list = builder.separated(", ");
    for id in ids {
        list.push_bind(id.to_string());
    }
    list.push_unseparated(")");
}

/// Refuses an `IN` list above [`MAX_IDS`]. The callers check their own limits first, so this is
/// a server bug and reads as a stored-value failure, never as a client conflict.
fn check_ids(ids: &[Uuid]) -> Result<(), MetaError> {
    if ids.len() > MAX_IDS {
        return Err(corrupt("experiments", "ids"));
    }
    Ok(())
}

fn trace_bytes(trace: &TraceId) -> Vec<u8> {
    trace.as_bytes().to_vec()
}

fn trace_from(bytes: Option<Vec<u8>>) -> Result<Option<TraceId>, MetaError> {
    bytes
        .map(|bytes| {
            TraceId::from_slice(&bytes).map_err(|_| corrupt("experiment_items", "trace_id"))
        })
        .transpose()
}

fn experiment_from(record: ExperimentRecord) -> Result<ExperimentRow, MetaError> {
    Ok(ExperimentRow {
        id: parse_uuid(&record.id, "experiments", "id")?,
        project_id: ProjectId::from_uuid(parse_uuid(
            &record.project_id,
            "experiments",
            "project_id",
        )?),
        dataset_id: record
            .dataset_id
            .as_deref()
            .map(|text| parse_uuid(text, "experiments", "dataset_id"))
            .transpose()?,
        dataset_name: record.dataset_name,
        dataset_version: record.dataset_version,
        name: record.name,
        description: record.description,
        metadata: record.metadata,
        repetitions: record.repetitions,
        item_count: record.item_count,
        result_count: record.result_count,
        status: record.status,
        error: record.error,
        created_by: record.created_by,
        created_at: record.created_at,
        finished_at: record.finished_at,
    })
}

fn result_from(record: ResultRecord) -> Result<ResultRow, MetaError> {
    Ok(ResultRow {
        dataset_item_id: parse_uuid(
            &record.dataset_item_id,
            "experiment_items",
            "dataset_item_id",
        )?,
        repetition: record.repetition,
        revision_id: parse_uuid(&record.revision_id, "experiment_items", "revision_id")?,
        input: record.input,
        expected_output: record.expected_output,
        trace_id: trace_from(record.trace_id)?,
        output: record.output,
        error: record.error,
        latency_ns: record.latency_ns,
        created_at: record.created_at,
    })
}

fn key_from(record: KeyRecord) -> Result<ResultKey, MetaError> {
    Ok(ResultKey {
        dataset_item_id: parse_uuid(
            &record.dataset_item_id,
            "experiment_items",
            "dataset_item_id",
        )?,
        repetition: record.repetition,
        trace_id: trace_from(record.trace_id)?,
        has_error: record.has_error != 0,
        latency_ns: record.latency_ns,
    })
}

fn view_from(record: ViewRecord) -> Result<ViewRow, MetaError> {
    Ok(ViewRow {
        experiment_id: parse_uuid(&record.id, "experiments", "id")?,
        experiment_name: record.name,
        dataset_id: record
            .dataset_id
            .as_deref()
            .map(|text| parse_uuid(text, "experiments", "dataset_id"))
            .transpose()?,
        dataset_version: record.dataset_version,
        dataset_item_id: parse_uuid(
            &record.dataset_item_id,
            "experiment_items",
            "dataset_item_id",
        )?,
        repetition: record.repetition,
        trace_id: trace_from(record.trace_id)?,
        input: record.input,
        expected_output: record.expected_output,
        output: record.output,
        error: record.error,
        latency_ns: record.latency_ns,
        created_at: record.created_at,
    })
}

fn corrupt(table: &'static str, column: &'static str) -> MetaError {
    MetaError::Corrupt { table, column }
}

fn parse_uuid(value: &str, table: &'static str, column: &'static str) -> Result<Uuid, MetaError> {
    value.parse().map_err(|_| corrupt(table, column))
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value, json};

    use super::*;
    use crate::meta::datasets::{
        DatasetFields, NewRevision, VersionChange, insert_dataset, list_items, write_version,
    };
    use crate::meta::orgs::insert_org;
    use crate::meta::projects::insert_project;
    use crate::meta::{MetaDb, MetaError};
    use llmobs_core::ids::OrgId;

    const NOW: i64 = 1_000;

    /// An org, a project `support` and a dataset `faq` with three items at version 1.
    async fn seed(db: &MetaDb) -> (OrgId, ProjectId, Uuid) {
        db.write(|conn| {
            Box::pin(async move {
                let org = insert_org(conn, OrgId::new_v7(), "Acme", "acme", NOW).await?;
                let project = insert_project(conn, org.id, "Support", "support", None, NOW)
                    .await?
                    .id;
                let dataset = insert_dataset(
                    conn,
                    Uuid::now_v7(),
                    project,
                    DatasetFields {
                        name: "faq".to_owned(),
                        description: None,
                        input_schema: None,
                        expected_schema: None,
                    },
                    NOW,
                )
                .await?
                .id;
                let inserts = (0..3).map(|i| revision(Uuid::now_v7(), json!(i))).collect();
                write_version(
                    conn,
                    dataset,
                    VersionChange {
                        close: vec![],
                        insert: inserts,
                        description: "add 3 items".to_owned(),
                        created_by: None,
                    },
                    NOW,
                )
                .await?;
                Ok((org.id, project, dataset))
            })
        })
        .await
        .unwrap()
    }

    fn revision(item: Uuid, input: Value) -> NewRevision {
        NewRevision {
            revision_id: Uuid::now_v7(),
            item_id: item,
            input,
            expected_output: None,
            metadata: Map::new(),
            source_trace_id: None,
            source_span_id: None,
        }
    }

    /// A second project in `org`, for isolation checks.
    async fn second_project(db: &MetaDb, org: OrgId) -> ProjectId {
        db.write(move |conn| {
            Box::pin(async move {
                insert_project(conn, org, "Other", "other", None, NOW)
                    .await
                    .map(|row| row.id)
            })
        })
        .await
        .unwrap()
    }

    /// The live items of `version` of `dataset`, by `item_id`.
    async fn items(
        db: &MetaDb,
        dataset: Uuid,
        version: i64,
    ) -> Vec<crate::meta::datasets::ItemRow> {
        list_items(db.reader(), dataset, version, None, 100)
            .await
            .unwrap()
    }

    fn new_experiment(
        id: Uuid,
        project: ProjectId,
        dataset: Uuid,
        name: &str,
        created_at: i64,
    ) -> NewExperiment {
        NewExperiment {
            id,
            project_id: project,
            dataset_id: dataset,
            dataset_name: "faq".to_owned(),
            dataset_version: 1,
            name: name.to_owned(),
            description: None,
            metadata: r#"{"model":"m"}"#.to_owned(),
            repetitions: 2,
            item_count: 3,
            created_by: Some("user-1".to_owned()),
            created_at,
        }
    }

    /// `insert_tx` in its own `db.write`; returns the experiment id.
    async fn create(db: &MetaDb, new: NewExperiment) -> Uuid {
        let id = new.id;
        db.write(move |conn| Box::pin(async move { insert_tx(conn, &new).await }))
            .await
            .unwrap();
        id
    }

    async fn upsert(db: &MetaDb, experiment: Uuid, results: Vec<NewResult>, now: i64) {
        db.write(move |conn| {
            Box::pin(async move {
                upsert_results_tx(conn, experiment, &results, now)
                    .await
                    .map(|_| ())
            })
        })
        .await
        .unwrap();
    }

    fn result(item: Uuid, repetition: i64, output: Option<&str>, error: Option<&str>) -> NewResult {
        NewResult {
            dataset_item_id: item,
            repetition,
            revision_id: Uuid::now_v7(),
            input: "\"in\"".to_owned(),
            expected_output: Some("\"exp\"".to_owned()),
            trace_id: None,
            output: output.map(str::to_owned),
            error: error.map(str::to_owned),
            latency_ns: Some(7),
        }
    }

    async fn stored_result_count(db: &MetaDb, experiment: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM experiment_items WHERE experiment_id = ?")
            .bind(experiment.to_string())
            .fetch_one(db.reader())
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn insert_then_get_round_trips() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_org, project, dataset) = seed(&db).await;
        let id = Uuid::now_v7();
        create(&db, new_experiment(id, project, dataset, "baseline", NOW)).await;

        let row = get(db.reader(), project, id, None).await.unwrap().unwrap();
        assert_eq!(row.id, id);
        assert_eq!(row.project_id, project);
        assert_eq!(row.dataset_id, Some(dataset));
        assert_eq!(row.dataset_name, "faq");
        assert_eq!(row.dataset_version, 1);
        assert_eq!(row.name, "baseline");
        assert_eq!(row.description, None);
        assert_eq!(row.metadata, r#"{"model":"m"}"#);
        assert_eq!(row.repetitions, 2);
        assert_eq!(row.item_count, 3);
        assert_eq!(row.result_count, 0);
        assert_eq!(row.status, "running");
        assert_eq!(row.error, None);
        assert_eq!(row.created_by.as_deref(), Some("user-1"));
        assert_eq!(row.created_at, NOW);
        assert_eq!(row.finished_at, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_hides_before_visible_from() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_org, project, dataset) = seed(&db).await;
        let id = create(
            &db,
            new_experiment(Uuid::now_v7(), project, dataset, "old", NOW),
        )
        .await;

        assert!(
            get(db.reader(), project, id, Some(NOW + 1))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            get(db.reader(), project, id, Some(NOW))
                .await
                .unwrap()
                .is_some()
        );
        assert!(get(db.reader(), project, id, None).await.unwrap().is_some());
        let in_transaction = db
            .write(move |conn| {
                Box::pin(async move { get_tx(conn, project, id, Some(NOW + 1)).await })
            })
            .await
            .unwrap();
        assert!(in_transaction.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_is_scoped_to_the_project() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, project, dataset) = seed(&db).await;
        let other = second_project(&db, org).await;
        let id = create(
            &db,
            new_experiment(Uuid::now_v7(), project, dataset, "mine", NOW),
        )
        .await;

        assert!(get(db.reader(), other, id, None).await.unwrap().is_none());
        assert!(
            get_many(db.reader(), other, &[id], None)
                .await
                .unwrap()
                .is_empty()
        );
        let here = get_many(db.reader(), project, &[id], None).await.unwrap();
        assert_eq!(here.len(), 1);
        assert_eq!(here[0].id, id);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_pages_newest_first() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_org, project, dataset) = seed(&db).await;
        // UUIDv7 ids minted in one millisecond are not ordered, so sort them first.
        let mut ids = [Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7()];
        ids.sort();
        for id in ids {
            create(&db, new_experiment(id, project, dataset, "e", NOW)).await;
        }

        let (first, cursor) = list(db.reader(), project, Some(dataset), None, 2, None)
            .await
            .unwrap();
        let first_ids: Vec<Uuid> = first.iter().map(|row| row.id).collect();
        assert_eq!(first_ids, [ids[2], ids[1]]);
        assert_eq!(cursor, Some(ids[1]));

        let (second, next) = list(db.reader(), project, Some(dataset), None, 2, cursor)
            .await
            .unwrap();
        let second_ids: Vec<Uuid> = second.iter().map(|row| row.id).collect();
        assert_eq!(second_ids, [ids[0]]);
        assert_eq!(next, None);

        let (other, none) = list(db.reader(), project, Some(Uuid::now_v7()), None, 10, None)
            .await
            .unwrap();
        assert!(other.is_empty());
        assert_eq!(none, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pinned_items_follow_the_version() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_org, _project, dataset) = seed(&db).await;
        let v1 = items(&db, dataset, 1).await;
        let (item1, item2) = (v1[0].item_id, v1[1].item_id);
        let v1_revision = v1[0].revision_id;
        let unknown = Uuid::now_v7();

        // Edit item 1 into version 2.
        let edited = Uuid::now_v7();
        db.write(move |conn| {
            Box::pin(async move {
                write_version(
                    conn,
                    dataset,
                    VersionChange {
                        close: vec![item1],
                        insert: vec![NewRevision {
                            revision_id: edited,
                            item_id: item1,
                            input: json!("edited"),
                            expected_output: None,
                            metadata: Map::new(),
                            source_trace_id: None,
                            source_span_id: None,
                        }],
                        description: "edit".to_owned(),
                        created_by: None,
                    },
                    NOW,
                )
                .await
            })
        })
        .await
        .unwrap();

        let ids = vec![item1, unknown];
        let at_v1 = db
            .write(move |conn| {
                Box::pin(async move { pinned_items_tx(conn, dataset, 1, &ids).await })
            })
            .await
            .unwrap();
        assert_eq!(at_v1.get(&item1).map(|p| p.revision_id), Some(v1_revision));
        assert!(!at_v1.contains_key(&unknown));

        let ids = vec![item1];
        let at_v2 = db
            .write(move |conn| {
                Box::pin(async move { pinned_items_tx(conn, dataset, 2, &ids).await })
            })
            .await
            .unwrap();
        let pinned = at_v2.get(&item1).unwrap();
        assert_eq!(pinned.revision_id, edited);
        assert_eq!(pinned.input, "\"edited\"");
        assert_eq!(pinned.expected_output, None);

        // Counts follow the validity ranges: 3 live at version 2, then item 2 is deleted.
        db.write(move |conn| {
            Box::pin(async move {
                write_version(
                    conn,
                    dataset,
                    VersionChange {
                        close: vec![item2],
                        insert: vec![],
                        description: "delete".to_owned(),
                        created_by: None,
                    },
                    NOW,
                )
                .await
            })
        })
        .await
        .unwrap();
        let counts = db
            .write(move |conn| {
                Box::pin(async move {
                    let mut counts = Vec::new();
                    for version in [1, 2, 3] {
                        counts.push(count_version_items_tx(conn, dataset, version).await?);
                    }
                    Ok(counts)
                })
            })
            .await
            .unwrap();
        assert_eq!(counts, [3, 3, 2]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn results_upsert_replaces_one_row_per_repetition() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_org, project, dataset) = seed(&db).await;
        let item = items(&db, dataset, 1).await[0].item_id;
        let id = create(
            &db,
            new_experiment(Uuid::now_v7(), project, dataset, "e", NOW),
        )
        .await;
        let trace = TraceId::from_slice(&[0xa1; 16]).unwrap();

        upsert(
            &db,
            id,
            vec![
                result(item, 0, Some("\"old\""), None),
                result(item, 1, None, None),
            ],
            NOW,
        )
        .await;
        let mut replacement = result(item, 0, Some("\"new\""), None);
        replacement.trace_id = Some(trace);
        upsert(&db, id, vec![replacement], NOW + 1).await;

        let rows = results_page(db.reader(), id, None, false, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].repetition, 0);
        assert_eq!(rows[0].output.as_deref(), Some("\"new\""));
        assert_eq!(rows[0].trace_id, Some(trace));
        assert_eq!(rows[0].created_at, NOW + 1);
        assert_eq!(rows[1].repetition, 1);

        let row = get(db.reader(), project, id, None).await.unwrap().unwrap();
        assert_eq!(row.result_count, 2);
        let keys = result_keys(db.reader(), id).await.unwrap();
        assert_eq!(keys.len(), 2);
        assert!(keys.iter().all(|key| !key.has_error));
        assert_eq!(keys[0].trace_id, Some(trace));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn finish_only_once() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_org, project, dataset) = seed(&db).await;
        let first = create(
            &db,
            new_experiment(Uuid::now_v7(), project, dataset, "a", NOW),
        )
        .await;
        let second = create(
            &db,
            new_experiment(Uuid::now_v7(), project, dataset, "b", NOW),
        )
        .await;

        let done = db
            .write(move |conn| Box::pin(async move { finish_tx(conn, first, None, NOW + 5).await }))
            .await
            .unwrap();
        assert!(done);
        let row = get(db.reader(), project, first, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "completed");
        assert_eq!(row.error, None);
        assert_eq!(row.finished_at, Some(NOW + 5));

        let again = db
            .write(move |conn| {
                Box::pin(async move { finish_tx(conn, first, Some("late"), NOW + 9).await })
            })
            .await
            .unwrap();
        assert!(!again);
        let unchanged = get(db.reader(), project, first, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(unchanged.status, "completed");
        assert_eq!(unchanged.error, None);
        assert_eq!(unchanged.finished_at, Some(NOW + 5));

        let failed = db
            .write(move |conn| {
                Box::pin(async move { finish_tx(conn, second, Some("boom"), NOW + 1).await })
            })
            .await
            .unwrap();
        assert!(failed);
        let row = get(db.reader(), project, second, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "failed");
        assert_eq!(row.error.as_deref(), Some("boom"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_cascades_results() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_org, project, dataset) = seed(&db).await;
        let item = items(&db, dataset, 1).await[0].item_id;
        let id = create(
            &db,
            new_experiment(Uuid::now_v7(), project, dataset, "e", NOW),
        )
        .await;
        upsert(&db, id, vec![result(item, 0, Some("\"x\""), None)], NOW).await;
        assert_eq!(stored_result_count(&db, id).await, 1);

        let deleted = db
            .write(move |conn| Box::pin(async move { delete_tx(conn, project, id).await }))
            .await
            .unwrap();
        assert!(deleted);
        assert_eq!(stored_result_count(&db, id).await, 0);
        assert!(get(db.reader(), project, id, None).await.unwrap().is_none());
        let again = db
            .write(move |conn| Box::pin(async move { delete_tx(conn, project, id).await }))
            .await
            .unwrap();
        assert!(!again);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deleting_the_dataset_keeps_the_experiment() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_org, project, dataset) = seed(&db).await;
        let item = items(&db, dataset, 1).await[0].item_id;
        let id = create(
            &db,
            new_experiment(Uuid::now_v7(), project, dataset, "e", NOW),
        )
        .await;
        upsert(&db, id, vec![result(item, 0, Some("\"x\""), None)], NOW).await;

        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query("DELETE FROM datasets WHERE id = ?")
                    .bind(dataset.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();

        let row = get(db.reader(), project, id, None).await.unwrap().unwrap();
        assert_eq!(row.dataset_id, None);
        assert_eq!(row.dataset_name, "faq");
        assert_eq!(row.result_count, 1);
        assert_eq!(result_keys(db.reader(), id).await.unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn existing_ids_ignores_other_projects() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, project, dataset) = seed(&db).await;
        let other = second_project(&db, org).await;
        let a1 = create(
            &db,
            new_experiment(Uuid::now_v7(), project, dataset, "a1", NOW),
        )
        .await;
        let a2 = create(
            &db,
            new_experiment(Uuid::now_v7(), project, dataset, "a2", NOW),
        )
        .await;
        let b1 = create(
            &db,
            new_experiment(Uuid::now_v7(), other, dataset, "b1", NOW),
        )
        .await;

        let found = existing_ids(db.reader(), project, &[a1, a2, b1, Uuid::now_v7()])
            .await
            .unwrap();
        assert_eq!(found, HashSet::from([a1, a2]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn view_rows_bounds() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_org, project, dataset) = seed(&db).await;
        let item = items(&db, dataset, 1).await[0].item_id;
        // Experiment A is created at 10 and B at 20; their results are stamped the same.
        let a = create(
            &db,
            new_experiment(Uuid::now_v7(), project, dataset, "A", 10),
        )
        .await;
        let b = create(
            &db,
            new_experiment(Uuid::now_v7(), project, dataset, "B", 20),
        )
        .await;
        // The text of A is 7 bytes, the text of B 8 bytes: 15 in all.
        let a_result = NewResult {
            dataset_item_id: item,
            repetition: 0,
            revision_id: Uuid::now_v7(),
            input: "\"aa\"".to_owned(),
            expected_output: None,
            trace_id: None,
            output: Some("\"x\"".to_owned()),
            error: None,
            latency_ns: None,
        };
        let b_result = NewResult {
            input: "\"bb\"".to_owned(),
            output: Some("\"yy\"".to_owned()),
            ..a_result.clone()
        };
        upsert(&db, a, vec![a_result], 10).await;
        upsert(&db, b, vec![b_result], 20).await;

        let window = view_rows(db.reader(), project, None, Some((15, 30)), 10, 100)
            .await
            .unwrap();
        let ViewRows::Rows(rows) = window else {
            panic!("the window keeps B's row");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].experiment_id, b);
        assert_eq!(rows[0].experiment_name, "B");
        assert_eq!(rows[0].dataset_id, Some(dataset));
        assert_eq!(rows[0].dataset_version, 1);
        assert_eq!(rows[0].created_at, 20);
        assert_eq!(rows[0].input, "\"bb\"");

        let visible = view_rows(db.reader(), project, Some(15), None, 10, 100)
            .await
            .unwrap();
        let ViewRows::Rows(rows) = visible else {
            panic!("the visible window keeps B");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].experiment_id, b);

        let too_many = view_rows(db.reader(), project, None, None, 1, 100)
            .await
            .unwrap();
        assert!(matches!(too_many, ViewRows::TooLarge));

        let one_less = view_rows(db.reader(), project, None, None, 10, 14)
            .await
            .unwrap();
        assert!(matches!(one_less, ViewRows::TooLarge));
        let exact = view_rows(db.reader(), project, None, None, 10, 15)
            .await
            .unwrap();
        assert!(matches!(exact, ViewRows::Rows(ref rows) if rows.len() == 2));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn results_page_keyset_and_errors_only() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_org, project, dataset) = seed(&db).await;
        // The live items, sorted by id: item1 < item2 < item3.
        let [item1, item2, item3] = {
            let ids: Vec<Uuid> = items(&db, dataset, 1)
                .await
                .iter()
                .map(|row| row.item_id)
                .collect();
            [ids[0], ids[1], ids[2]]
        };
        let id = create(
            &db,
            new_experiment(Uuid::now_v7(), project, dataset, "e", NOW),
        )
        .await;
        let mut results = Vec::new();
        for item in [item1, item2, item3] {
            for repetition in 0..2 {
                let error = (item == item2 && repetition == 1).then_some("failed here");
                results.push(result(item, repetition, Some("\"o\""), error));
            }
        }
        upsert(&db, id, results, NOW).await;

        let after = results_page(db.reader(), id, Some((item1, 1)), false, 2)
            .await
            .unwrap();
        let positions: Vec<(Uuid, i64)> = after
            .iter()
            .map(|row| (row.dataset_item_id, row.repetition))
            .collect();
        assert_eq!(positions, [(item2, 0), (item2, 1)]);

        let errors = results_page(db.reader(), id, None, true, 10).await.unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(
            (errors[0].dataset_item_id, errors[0].repetition),
            (item2, 1)
        );
        assert_eq!(errors[0].error.as_deref(), Some("failed here"));

        let third = item_rows(db.reader(), id, &[item3]).await.unwrap();
        assert_eq!(third.len(), 2);
        assert!(third.iter().all(|row| row.dataset_item_id == item3));

        let keys = result_keys(db.reader(), id).await.unwrap();
        assert_eq!(keys.len(), 6);
        let failing: Vec<(Uuid, i64)> = keys
            .iter()
            .filter(|key| key.has_error)
            .map(|key| (key.dataset_item_id, key.repetition))
            .collect();
        assert_eq!(failing, [(item2, 1)]);
        assert_eq!(keys[0].latency_ns, Some(7));
    }

    #[test]
    fn content_is_not_in_debug() {
        let row = ResultRow {
            dataset_item_id: Uuid::now_v7(),
            repetition: 0,
            revision_id: Uuid::now_v7(),
            input: "\"secret-input-1\"".to_owned(),
            expected_output: None,
            trace_id: None,
            output: Some("\"secret-output-1\"".to_owned()),
            error: None,
            latency_ns: Some(5),
            created_at: NOW,
        };
        let printed = format!("{row:?}");
        assert!(!printed.contains("secret-input-1"), "{printed}");
        assert!(!printed.contains("secret-output-1"), "{printed}");
        assert!(printed.contains("latency_ns"), "{printed}");
    }

    #[test]
    fn check_ids_refuses_a_list_above_the_limit() {
        let ids: Vec<Uuid> = (0..=MAX_IDS).map(|_| Uuid::now_v7()).collect();
        assert!(matches!(
            check_ids(&ids),
            Err(MetaError::Corrupt {
                table: "experiments",
                column: "ids"
            })
        ));
        assert!(check_ids(&ids[..MAX_IDS]).is_ok());
    }
}
