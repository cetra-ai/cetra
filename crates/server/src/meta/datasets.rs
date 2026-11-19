//! `datasets`, `dataset_versions` and `dataset_items` rows (spec 04 § Datasets & experiments, D-J-2, D-J-3, D-J-4).

use std::collections::{HashMap, HashSet};
use std::fmt;

use cetra_core::ids::{ProjectId, SpanId, TraceId};
use futures::TryStreamExt;
use serde_json::{Map, Value};
use sqlx::{AssertSqlSafe, SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::{MetaError, unique_or};
use crate::datasets::json_bytes;

/// A project holds at most this many datasets (D-J-4).
pub(crate) const PROJECT_CAP: i64 = 1_000;
/// The latest version of a dataset holds at most this many live items (D-J-4).
pub(crate) const LIVE_ITEM_CAP: i64 = 50_000;
/// Conflict reasons.
pub(crate) const DUPLICATE_NAME: &str = "a dataset with this name already exists";
pub(crate) const TOO_MANY_DATASETS: &str = "a project has at most 1000 datasets";
pub(crate) const TOO_MANY_ITEMS: &str = "a dataset holds at most 50000 items";

/// One stored dataset, with its counts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DatasetRow {
    /// UUIDv7.
    pub(crate) id: Uuid,
    /// Owning project.
    pub(crate) project_id: ProjectId,
    /// Unique in the project; never parses as a UUID (D-J-4).
    pub(crate) name: String,
    /// Free text.
    pub(crate) description: Option<String>,
    /// JSON Schema text for `input`, when set.
    pub(crate) input_schema: Option<String>,
    /// JSON Schema text for `expected_output`, when set.
    pub(crate) expected_schema: Option<String>,
    /// Latest version; 0 for a new dataset.
    pub(crate) current_version: i64,
    /// Live items at `current_version`.
    pub(crate) item_count: i64,
    /// Experiments whose `dataset_id` is this dataset.
    pub(crate) experiment_count: i64,
    /// Creation time, nanoseconds.
    pub(crate) created_at: i64,
    /// Last change, nanoseconds.
    pub(crate) updated_at: i64,
}

/// The editable columns of a dataset. Insert takes them; update replaces them all.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DatasetFields {
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) input_schema: Option<String>,
    pub(crate) expected_schema: Option<String>,
}

/// How a route names a dataset: `{dataset}` is an id when it parses as a UUID, else a name (D-J-4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DatasetKey {
    Id(Uuid),
    Name(String),
}

impl DatasetKey {
    /// `Id` when `text` parses as a UUID, else `Name(text)` unchanged.
    pub(crate) fn parse(text: &str) -> Self {
        text.parse::<Uuid>()
            .map_or_else(|_| Self::Name(text.to_owned()), Self::Id)
    }
}

/// One `dataset_versions` row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VersionRow {
    pub(crate) version: i64,
    pub(crate) description: String,
    /// A user id, or `key:<key id>`.
    pub(crate) created_by: Option<String>,
    pub(crate) created_at: i64,
    pub(crate) counts: VersionCounts,
}

/// The item changes of one version (D-J-24): an item whose revision the version starts and
/// that had none closed by it is added; one with a revision closed by it and another started by
/// it is edited; one with only a closed revision is deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct VersionCounts {
    pub(crate) added: i64,
    pub(crate) edited: i64,
    pub(crate) deleted: i64,
}

impl VersionCounts {
    /// The counts of the version `change` writes, as [`list_versions`] counts it afterwards.
    pub(crate) fn of_change(change: &VersionChange) -> Self {
        let inserted: HashSet<Uuid> = change.insert.iter().map(|rev| rev.item_id).collect();
        let closed: HashSet<Uuid> = change.close.iter().copied().collect();
        let edited = closed.intersection(&inserted).count();
        Self {
            added: i64::try_from(inserted.len() - edited).unwrap_or(i64::MAX),
            edited: i64::try_from(edited).unwrap_or(i64::MAX),
            deleted: i64::try_from(closed.difference(&inserted).count()).unwrap_or(i64::MAX),
        }
    }
}

/// One item that changed in a version, with the revisions on either side of it (D-J-24).
/// Both are absent only for an item the version does not touch, which listing never returns.
#[derive(Clone, PartialEq)]
pub(crate) struct ItemChangeRow {
    pub(crate) item_id: Uuid,
    /// The revision the version closed.
    pub(crate) before: Option<ItemRow>,
    /// The revision the version started.
    pub(crate) after: Option<ItemRow>,
}

impl fmt::Debug for ItemChangeRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ItemChangeRow")
            .field("item_id", &self.item_id)
            .field("before", &self.before)
            .field("after", &self.after)
            .finish()
    }
}

/// One item revision. `Debug` is hand-written (below): it prints ids, versions and sizes, never content.
#[derive(Clone, PartialEq)]
pub(crate) struct ItemRow {
    pub(crate) revision_id: Uuid,
    /// Stable across revisions.
    pub(crate) item_id: Uuid,
    pub(crate) dataset_id: Uuid,
    pub(crate) input: Value,
    pub(crate) expected_output: Option<Value>,
    pub(crate) metadata: Map<String, Value>,
    pub(crate) source_trace_id: Option<TraceId>,
    pub(crate) source_span_id: Option<SpanId>,
    pub(crate) valid_from_version: i64,
    pub(crate) valid_to_version: Option<i64>,
    pub(crate) created_at: i64,
}

/// A revision to insert. Its `valid_from_version` is the new version. `Debug` is hand-written.
#[derive(Clone, PartialEq)]
pub(crate) struct NewRevision {
    pub(crate) revision_id: Uuid,
    pub(crate) item_id: Uuid,
    pub(crate) input: Value,
    pub(crate) expected_output: Option<Value>,
    pub(crate) metadata: Map<String, Value>,
    pub(crate) source_trace_id: Option<TraceId>,
    pub(crate) source_span_id: Option<SpanId>,
}

/// One mutation of a dataset: the items whose live revision closes, the revisions to insert.
/// An edit closes the item and inserts its new revision; a delete only closes; an add only inserts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VersionChange {
    pub(crate) close: Vec<Uuid>,
    pub(crate) insert: Vec<NewRevision>,
    pub(crate) description: String,
    pub(crate) created_by: Option<String>,
}

impl fmt::Debug for ItemRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ItemRow")
            .field("revision_id", &self.revision_id)
            .field("item_id", &self.item_id)
            .field("dataset_id", &self.dataset_id)
            .field("valid_from_version", &self.valid_from_version)
            .field("valid_to_version", &self.valid_to_version)
            .field(
                "bytes",
                &json_bytes(&self.input, self.expected_output.as_ref(), &self.metadata),
            )
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for NewRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewRevision")
            .field("revision_id", &self.revision_id)
            .field("item_id", &self.item_id)
            .field(
                "bytes",
                &json_bytes(&self.input, self.expected_output.as_ref(), &self.metadata),
            )
            .finish_non_exhaustive()
    }
}

/// The dataset SELECT used by every dataset read.
const DATASET_SELECT: &str = "SELECT d.id, d.project_id, d.name, d.description, d.input_schema, d.expected_schema, \
     d.current_version, d.created_at, d.updated_at, \
     (SELECT COUNT(*) FROM dataset_items i \
       WHERE i.dataset_id = d.id AND i.valid_to_version IS NULL) AS item_count, \
     (SELECT COUNT(*) FROM experiments e WHERE e.dataset_id = d.id) AS experiment_count \
     FROM datasets d";

/// The item columns, in the order `ItemRecord` reads them.
const ITEM_COLUMNS: &str = "revision_id, item_id, dataset_id, input, expected_output, metadata, \
     source_trace_id, source_span_id, valid_from_version, valid_to_version, created_at";

/// The visibility predicate of version `?1` (D-J-3).
const IN_VERSION: &str =
    "valid_from_version <= ?1 AND (valid_to_version IS NULL OR valid_to_version > ?1)";

#[derive(sqlx::FromRow)]
struct DatasetRecord {
    id: String,
    project_id: String,
    name: String,
    description: Option<String>,
    input_schema: Option<String>,
    expected_schema: Option<String>,
    current_version: i64,
    created_at: i64,
    updated_at: i64,
    item_count: i64,
    experiment_count: i64,
}

#[derive(sqlx::FromRow)]
struct VersionRecord {
    version: i64,
    description: String,
    created_by: Option<String>,
    created_at: i64,
}

#[derive(sqlx::FromRow)]
struct CountRecord {
    version: i64,
    added: i64,
    edited: i64,
    deleted: i64,
}

#[derive(sqlx::FromRow)]
struct ItemRecord {
    revision_id: String,
    item_id: String,
    dataset_id: String,
    input: String,
    expected_output: Option<String>,
    metadata: String,
    source_trace_id: Option<Vec<u8>>,
    source_span_id: Option<Vec<u8>>,
    valid_from_version: i64,
    valid_to_version: Option<i64>,
    created_at: i64,
}

/// Every dataset of `project` whose name contains `needle` (case-insensitive, literally: `%`,
/// `_` and `\` in it match themselves), ordered by `lower(name)`, then `id`.
pub(crate) async fn list_datasets(
    pool: &SqlitePool,
    project: ProjectId,
    needle: Option<&str>,
) -> Result<Vec<DatasetRow>, MetaError> {
    // Only constants are spliced; every value is bound.
    let sql = format!(
        "{DATASET_SELECT} WHERE d.project_id = ?1 \
         AND (?2 IS NULL OR lower(d.name) LIKE '%' || lower(?2) || '%' ESCAPE '\\') \
         ORDER BY lower(d.name), d.id"
    );
    let needle = needle.map(escape_like);
    let records: Vec<DatasetRecord> = sqlx::query_as(AssertSqlSafe(sql))
        .bind(project.to_string())
        .bind(needle)
        .fetch_all(pool)
        .await?;
    records.into_iter().map(dataset_from).collect()
}

/// `text` with the LIKE wildcards and the escape character itself escaped (`ESCAPE '\'`).
fn escape_like(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// The dataset of `project` named by `key`.
pub(crate) async fn get_dataset(
    pool: &SqlitePool,
    project: ProjectId,
    key: &DatasetKey,
) -> Result<Option<DatasetRow>, MetaError> {
    let record: Option<DatasetRecord> = sqlx::query_as(AssertSqlSafe(dataset_where(key)))
        .bind(project.to_string())
        .bind(key_text(key))
        .fetch_optional(pool)
        .await?;
    record.map(dataset_from).transpose()
}

/// The same read inside a write transaction.
pub(crate) async fn get_dataset_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    key: &DatasetKey,
) -> Result<Option<DatasetRow>, MetaError> {
    let record: Option<DatasetRecord> = sqlx::query_as(AssertSqlSafe(dataset_where(key)))
        .bind(project.to_string())
        .bind(key_text(key))
        .fetch_optional(&mut *conn)
        .await?;
    record.map(dataset_from).transpose()
}

/// Inserts a dataset at version 0. Past [`PROJECT_CAP`] is `Conflict(TOO_MANY_DATASETS)`;
/// a duplicate name is `Conflict(DUPLICATE_NAME)`.
pub(crate) async fn insert_dataset(
    conn: &mut SqliteConnection,
    id: Uuid,
    project: ProjectId,
    fields: DatasetFields,
    now: i64,
) -> Result<DatasetRow, MetaError> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM datasets WHERE project_id = ?")
        .bind(project.to_string())
        .fetch_one(&mut *conn)
        .await?;
    if count >= PROJECT_CAP {
        return Err(MetaError::Conflict(TOO_MANY_DATASETS));
    }
    sqlx::query(
        "INSERT INTO datasets (id, project_id, name, description, input_schema, expected_schema, \
         current_version, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, 0, ?, ?)",
    )
    .bind(id.to_string())
    .bind(project.to_string())
    .bind(fields.name)
    .bind(fields.description)
    .bind(fields.input_schema)
    .bind(fields.expected_schema)
    .bind(now)
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(|error| unique_or(error, DUPLICATE_NAME))?;
    get_dataset_tx(conn, project, &DatasetKey::Id(id))
        .await?
        .ok_or(corrupt("datasets", "id"))
}

/// Replaces the editable columns and `updated_at`. `None` when no such dataset in `project`.
/// A duplicate name is `Conflict(DUPLICATE_NAME)`.
pub(crate) async fn update_dataset(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
    fields: DatasetFields,
    now: i64,
) -> Result<Option<DatasetRow>, MetaError> {
    let result = sqlx::query(
        "UPDATE datasets SET name = ?, description = ?, input_schema = ?, expected_schema = ?, \
         updated_at = ? WHERE project_id = ? AND id = ?",
    )
    .bind(fields.name)
    .bind(fields.description)
    .bind(fields.input_schema)
    .bind(fields.expected_schema)
    .bind(now)
    .bind(project.to_string())
    .bind(id.to_string())
    .execute(&mut *conn)
    .await
    .map_err(|error| unique_or(error, DUPLICATE_NAME))?;
    if result.rows_affected() == 0 {
        return Ok(None);
    }
    get_dataset_tx(conn, project, &DatasetKey::Id(id))
        .await?
        .ok_or(corrupt("datasets", "id"))
        .map(Some)
}

/// Deletes the dataset; its versions and items go by cascade, its experiments keep their rows
/// with `dataset_id` NULL. False when no such dataset in `project`.
pub(crate) async fn delete_dataset(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
) -> Result<bool, MetaError> {
    let result = sqlx::query("DELETE FROM datasets WHERE project_id = ? AND id = ?")
        .bind(project.to_string())
        .bind(id.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Writes one version (design § Data model, "Writing a version") and returns its number.
/// Past [`LIVE_ITEM_CAP`] live items afterwards is `Conflict(TOO_MANY_ITEMS)`; the caller's
/// transaction then rolls back.
pub(crate) async fn write_version(
    conn: &mut SqliteConnection,
    dataset: Uuid,
    change: VersionChange,
    now: i64,
) -> Result<i64, MetaError> {
    let dataset_text = dataset.to_string();
    let current: Option<i64> =
        sqlx::query_scalar("SELECT current_version FROM datasets WHERE id = ?")
            .bind(&dataset_text)
            .fetch_optional(&mut *conn)
            .await?;
    let version = current.ok_or(corrupt("datasets", "id"))? + 1;

    for item in &change.close {
        let closed = sqlx::query(
            "UPDATE dataset_items SET valid_to_version = ? \
             WHERE dataset_id = ? AND item_id = ? AND valid_to_version IS NULL",
        )
        .bind(version)
        .bind(&dataset_text)
        .bind(item.to_string())
        .execute(&mut *conn)
        .await?;
        if closed.rows_affected() == 0 {
            return Err(corrupt("dataset_items", "item_id"));
        }
    }

    for revision in &change.insert {
        let input = serde_json::to_string(&revision.input)
            .map_err(|_| corrupt("dataset_items", "input"))?;
        let expected = revision
            .expected_output
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| corrupt("dataset_items", "expected_output"))?;
        let metadata = serde_json::to_string(&revision.metadata)
            .map_err(|_| corrupt("dataset_items", "metadata"))?;
        sqlx::query(
            "INSERT INTO dataset_items (revision_id, item_id, dataset_id, input, expected_output, \
             metadata, source_trace_id, source_span_id, valid_from_version, valid_to_version, \
             created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?)",
        )
        .bind(revision.revision_id.to_string())
        .bind(revision.item_id.to_string())
        .bind(&dataset_text)
        .bind(input)
        .bind(expected)
        .bind(metadata)
        .bind(revision.source_trace_id.as_ref().map(trace_bytes))
        .bind(revision.source_span_id.as_ref().map(span_bytes))
        .bind(version)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    }

    let live: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM dataset_items WHERE dataset_id = ? AND valid_to_version IS NULL",
    )
    .bind(&dataset_text)
    .fetch_one(&mut *conn)
    .await?;
    if live > LIVE_ITEM_CAP {
        return Err(MetaError::Conflict(TOO_MANY_ITEMS));
    }

    sqlx::query(
        "INSERT INTO dataset_versions (dataset_id, version, description, created_by, created_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&dataset_text)
    .bind(version)
    .bind(change.description)
    .bind(change.created_by)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE datasets SET current_version = ?, updated_at = ? WHERE id = ?")
        .bind(version)
        .bind(now)
        .bind(&dataset_text)
        .execute(&mut *conn)
        .await?;
    Ok(version)
}

/// Versions of `dataset`, newest first; `before` is the cursor (versions strictly lower).
/// Returns at most `limit + 1` rows; the caller uses the extra row to know there is a next page.
/// Each row carries its item counts (D-J-24), computed for the rows' range in one query.
pub(crate) async fn list_versions(
    pool: &SqlitePool,
    dataset: Uuid,
    before: Option<i64>,
    limit: u32,
) -> Result<Vec<VersionRow>, MetaError> {
    let records: Vec<VersionRecord> = sqlx::query_as(
        "SELECT version, description, created_by, created_at FROM dataset_versions \
         WHERE dataset_id = ?1 AND (?2 IS NULL OR version < ?2) ORDER BY version DESC LIMIT ?3",
    )
    .bind(dataset.to_string())
    .bind(before)
    .bind(i64::from(limit) + 1)
    .fetch_all(pool)
    .await?;
    let (Some(newest), Some(oldest)) = (
        records.first().map(|record| record.version),
        records.last().map(|record| record.version),
    ) else {
        return Ok(Vec::new());
    };
    let counts = version_counts(pool, dataset, oldest, newest).await?;
    Ok(records
        .into_iter()
        .map(|record| {
            let counts = counts.get(&record.version).copied().unwrap_or_default();
            VersionRow {
                counts,
                ..version_from(record)
            }
        })
        .collect())
}

/// The item counts of the versions `low..=high` of `dataset`: per version, the items whose
/// revision it starts and had no closed revision (added), had one (edited), and the items it
/// only closes (deleted). Versions with no change are absent.
async fn version_counts(
    pool: &SqlitePool,
    dataset: Uuid,
    low: i64,
    high: i64,
) -> Result<HashMap<i64, VersionCounts>, MetaError> {
    let records: Vec<CountRecord> = sqlx::query_as(
        "WITH events AS ( \
           SELECT valid_from_version AS version, item_id, 1 AS opened, 0 AS closed \
             FROM dataset_items WHERE dataset_id = ?1 AND valid_from_version BETWEEN ?2 AND ?3 \
           UNION ALL \
           SELECT valid_to_version, item_id, 0, 1 \
             FROM dataset_items WHERE dataset_id = ?1 AND valid_to_version BETWEEN ?2 AND ?3 \
         ), per_item AS ( \
           SELECT version, item_id, MAX(opened) AS opened, MAX(closed) AS closed \
             FROM events GROUP BY version, item_id \
         ) \
         SELECT version, \
           COALESCE(SUM(opened = 1 AND closed = 0), 0) AS added, \
           COALESCE(SUM(opened = 1 AND closed = 1), 0) AS edited, \
           COALESCE(SUM(opened = 0 AND closed = 1), 0) AS deleted \
         FROM per_item GROUP BY version",
    )
    .bind(dataset.to_string())
    .bind(low)
    .bind(high)
    .fetch_all(pool)
    .await?;
    Ok(records
        .into_iter()
        .map(|record| {
            (
                record.version,
                VersionCounts {
                    added: record.added,
                    edited: record.edited,
                    deleted: record.deleted,
                },
            )
        })
        .collect())
}

/// The revisions that `version` starts or closes, paired per item and ordered by `item_id`,
/// with `item_id > after` when set. Returns at most `limit + 1` items; the caller uses the extra
/// one to know there is a next page (D-J-24).
pub(crate) async fn list_changes(
    pool: &SqlitePool,
    dataset: Uuid,
    version: i64,
    after: Option<Uuid>,
    limit: u32,
) -> Result<Vec<ItemChangeRow>, MetaError> {
    // Only constants are spliced; every value is bound. The page is chosen by item, so that both
    // revisions of an edited item are read together.
    let sql = format!(
        "SELECT {ITEM_COLUMNS} FROM dataset_items WHERE dataset_id = ?1 \
         AND (valid_from_version = ?2 OR valid_to_version = ?2) \
         AND item_id IN ( \
           SELECT item_id FROM dataset_items WHERE dataset_id = ?1 \
           AND (valid_from_version = ?2 OR valid_to_version = ?2) \
           AND (?3 IS NULL OR item_id > ?3) ORDER BY item_id LIMIT ?4) \
         ORDER BY item_id, valid_from_version"
    );
    let records: Vec<ItemRecord> = sqlx::query_as(AssertSqlSafe(sql))
        .bind(dataset.to_string())
        .bind(version)
        .bind(after.map(|id| id.to_string()))
        .bind(i64::from(limit) + 1)
        .fetch_all(pool)
        .await?;
    let mut changes: Vec<ItemChangeRow> = Vec::new();
    for record in records {
        let row = item_from(record)?;
        let item_id = row.item_id;
        let opened = row.valid_from_version == version;
        match changes.last_mut() {
            Some(last) if last.item_id == item_id => {
                if opened {
                    last.after = Some(row);
                } else {
                    last.before = Some(row);
                }
            }
            _ => changes.push(if opened {
                ItemChangeRow {
                    item_id,
                    before: None,
                    after: Some(row),
                }
            } else {
                ItemChangeRow {
                    item_id,
                    before: Some(row),
                    after: None,
                }
            }),
        }
    }
    Ok(changes)
}

/// The revisions in `version`, ordered by `item_id`, with `item_id > after` when set. With
/// `needle`, only the revisions whose JSON text (input, expected output, metadata) contains it,
/// case-insensitively and literally (D-J-22). Returns at most `limit + 1` rows.
pub(crate) async fn list_items(
    pool: &SqlitePool,
    dataset: Uuid,
    version: i64,
    after: Option<Uuid>,
    limit: u32,
    needle: Option<&str>,
) -> Result<Vec<ItemRow>, MetaError> {
    // Only constants are spliced; every value is bound.
    let sql = format!(
        "SELECT {ITEM_COLUMNS} FROM dataset_items WHERE dataset_id = ?2 AND {IN_VERSION} \
         AND (?3 IS NULL OR item_id > ?3) \
         AND (?5 IS NULL OR lower(input || ' ' || COALESCE(expected_output, '') || ' ' || metadata) \
           LIKE '%' || lower(?5) || '%' ESCAPE '\\') \
         ORDER BY item_id LIMIT ?4"
    );
    let needle = needle.map(escape_like);
    let records: Vec<ItemRecord> = sqlx::query_as(AssertSqlSafe(sql))
        .bind(version)
        .bind(dataset.to_string())
        .bind(after.map(|id| id.to_string()))
        .bind(i64::from(limit) + 1)
        .bind(needle)
        .fetch_all(pool)
        .await?;
    records.into_iter().map(item_from).collect()
}

/// Bytes of the live revisions of `dataset`, summed in SQLite so that no item is read to know
/// it (D-J-2). The same measure as [`json_bytes`] of each item, for stored JSON.
pub(crate) async fn live_json_bytes_tx(
    conn: &mut SqliteConnection,
    dataset: Uuid,
) -> Result<i64, MetaError> {
    let total: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(length(CAST(input AS BLOB)) \
         + length(CAST(COALESCE(expected_output, '') AS BLOB)) \
         + length(CAST(metadata AS BLOB))), 0) \
         FROM dataset_items WHERE dataset_id = ?1 AND valid_to_version IS NULL",
    )
    .bind(dataset.to_string())
    .fetch_one(&mut *conn)
    .await?;
    Ok(total)
}

/// The page of live revisions (`valid_to_version IS NULL`), inside a write transaction.
pub(crate) async fn live_items_tx(
    conn: &mut SqliteConnection,
    dataset: Uuid,
    after: Option<Uuid>,
    limit: u32,
) -> Result<Vec<ItemRow>, MetaError> {
    let records: Vec<ItemRecord> = sqlx::query_as(
        "SELECT revision_id, item_id, dataset_id, input, expected_output, metadata, \
         source_trace_id, source_span_id, valid_from_version, valid_to_version, created_at \
         FROM dataset_items WHERE dataset_id = ?1 AND valid_to_version IS NULL \
         AND (?2 IS NULL OR item_id > ?2) ORDER BY item_id LIMIT ?3",
    )
    .bind(dataset.to_string())
    .bind(after.map(|id| id.to_string()))
    .bind(i64::from(limit) + 1)
    .fetch_all(&mut *conn)
    .await?;
    records.into_iter().map(item_from).collect()
}

/// The page of revisions in `version`, ordered by `item_id`, inside a write transaction (D-J-24).
/// Returns at most `limit + 1` rows.
pub(crate) async fn version_items_tx(
    conn: &mut SqliteConnection,
    dataset: Uuid,
    version: i64,
    after: Option<Uuid>,
    limit: u32,
) -> Result<Vec<ItemRow>, MetaError> {
    // Only constants are spliced; every value is bound.
    let sql = format!(
        "SELECT {ITEM_COLUMNS} FROM dataset_items WHERE dataset_id = ?2 AND {IN_VERSION} \
         AND (?3 IS NULL OR item_id > ?3) ORDER BY item_id LIMIT ?4"
    );
    let records: Vec<ItemRecord> = sqlx::query_as(AssertSqlSafe(sql))
        .bind(version)
        .bind(dataset.to_string())
        .bind(after.map(|id| id.to_string()))
        .bind(i64::from(limit) + 1)
        .fetch_all(&mut *conn)
        .await?;
    records.into_iter().map(item_from).collect()
}

/// The revision of `item` in `version`.
pub(crate) async fn get_item(
    pool: &SqlitePool,
    dataset: Uuid,
    item: Uuid,
    version: i64,
) -> Result<Option<ItemRow>, MetaError> {
    // Only constants are spliced; every value is bound.
    let sql = format!(
        "SELECT {ITEM_COLUMNS} FROM dataset_items WHERE dataset_id = ?2 AND item_id = ?3 \
         AND {IN_VERSION}"
    );
    let record: Option<ItemRecord> = sqlx::query_as(AssertSqlSafe(sql))
        .bind(version)
        .bind(dataset.to_string())
        .bind(item.to_string())
        .fetch_optional(pool)
        .await?;
    record.map(item_from).transpose()
}

/// The live revision of `item`, inside a write transaction.
pub(crate) async fn get_live_item_tx(
    conn: &mut SqliteConnection,
    dataset: Uuid,
    item: Uuid,
) -> Result<Option<ItemRow>, MetaError> {
    let record: Option<ItemRecord> = sqlx::query_as(
        "SELECT revision_id, item_id, dataset_id, input, expected_output, metadata, \
         source_trace_id, source_span_id, valid_from_version, valid_to_version, created_at \
         FROM dataset_items WHERE dataset_id = ?1 AND item_id = ?2 AND valid_to_version IS NULL",
    )
    .bind(dataset.to_string())
    .bind(item.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    record.map(item_from).transpose()
}

/// One live item of a dataset at its `current_version`, for the `dataset_items` SQL view
/// (D-J-15). The JSON fields are the stored text. `Debug` is hand-written: ids and sizes only.
#[derive(Clone, PartialEq)]
pub(crate) struct ViewItem {
    pub(crate) dataset_id: Uuid,
    pub(crate) dataset_name: String,
    pub(crate) dataset_version: i64,
    pub(crate) item_id: Uuid,
    /// JSON text.
    pub(crate) input: String,
    /// JSON text, `None` when absent.
    pub(crate) expected_output: Option<String>,
    /// JSON text.
    pub(crate) metadata: String,
    pub(crate) source_trace_id: Option<TraceId>,
    pub(crate) source_span_id: Option<SpanId>,
    pub(crate) created_at: i64,
}

impl ViewItem {
    /// The summed byte lengths of the three JSON texts.
    fn text_bytes(&self) -> usize {
        self.input.len()
            + self.expected_output.as_ref().map_or(0, String::len)
            + self.metadata.len()
    }
}

impl fmt::Debug for ViewItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ViewItem")
            .field("dataset_id", &self.dataset_id)
            .field("dataset_version", &self.dataset_version)
            .field("item_id", &self.item_id)
            .field("bytes", &self.text_bytes())
            .finish_non_exhaustive()
    }
}

/// What [`view_rows`] returns.
pub(crate) enum ViewItems {
    Rows(Vec<ViewItem>),
    /// More than `max_rows` rows or more than `max_text_bytes` of text (D-J-15).
    TooLarge,
}

#[derive(sqlx::FromRow)]
struct ViewRecord {
    dataset_id: String,
    dataset_name: String,
    current_version: i64,
    item_id: String,
    input: String,
    expected_output: Option<String>,
    metadata: String,
    source_trace_id: Option<Vec<u8>>,
    source_span_id: Option<Vec<u8>>,
    created_at: i64,
}

/// The live items of every dataset of `project` at its current version, ordered by dataset id
/// then item id, for the `dataset_items` SQL view (D-J-15). Rows are streamed; reading stops
/// with `TooLarge` as soon as the rows exceed `max_rows` or the summed text exceeds
/// `max_text_bytes`.
pub(crate) async fn view_rows(
    pool: &SqlitePool,
    project: ProjectId,
    max_rows: usize,
    max_text_bytes: usize,
) -> Result<ViewItems, MetaError> {
    let mut records = sqlx::query_as::<_, ViewRecord>(
        "SELECT d.id AS dataset_id, d.name AS dataset_name, d.current_version, i.item_id, \
         i.input, i.expected_output, \
         i.metadata, i.source_trace_id, i.source_span_id, i.created_at \
         FROM dataset_items i JOIN datasets d ON d.id = i.dataset_id \
         WHERE d.project_id = ?1 \
         AND i.valid_from_version <= d.current_version \
         AND (i.valid_to_version IS NULL OR i.valid_to_version > d.current_version) \
         ORDER BY d.id, i.item_id",
    )
    .bind(project.to_string())
    .fetch(pool);
    let mut rows = Vec::new();
    let mut text_bytes = 0_usize;
    while let Some(record) = records.try_next().await? {
        let row = view_item_from(record)?;
        if rows.len() >= max_rows {
            return Ok(ViewItems::TooLarge);
        }
        text_bytes = text_bytes.saturating_add(row.text_bytes());
        if text_bytes > max_text_bytes {
            return Ok(ViewItems::TooLarge);
        }
        rows.push(row);
    }
    Ok(ViewItems::Rows(rows))
}

fn view_item_from(record: ViewRecord) -> Result<ViewItem, MetaError> {
    Ok(ViewItem {
        dataset_id: parse_uuid(&record.dataset_id, "datasets", "id")?,
        dataset_name: record.dataset_name,
        dataset_version: record.current_version,
        item_id: parse_uuid(&record.item_id, "dataset_items", "item_id")?,
        input: record.input,
        expected_output: record.expected_output,
        metadata: record.metadata,
        source_trace_id: record
            .source_trace_id
            .map(|bytes| {
                TraceId::from_slice(&bytes).map_err(|_| corrupt("dataset_items", "source_trace_id"))
            })
            .transpose()?,
        source_span_id: record
            .source_span_id
            .map(|bytes| {
                SpanId::from_slice(&bytes).map_err(|_| corrupt("dataset_items", "source_span_id"))
            })
            .transpose()?,
        created_at: record.created_at,
    })
}

/// The `WHERE` form of the dataset read for `key`. Only constants are spliced.
fn dataset_where(key: &DatasetKey) -> String {
    let column = match key {
        DatasetKey::Id(_) => "id",
        DatasetKey::Name(_) => "name",
    };
    format!("{DATASET_SELECT} WHERE d.project_id = ? AND d.{column} = ?")
}

/// The value bound to `dataset_where`: the id text or the name.
fn key_text(key: &DatasetKey) -> String {
    match key {
        DatasetKey::Id(id) => id.to_string(),
        DatasetKey::Name(name) => name.clone(),
    }
}

fn dataset_from(record: DatasetRecord) -> Result<DatasetRow, MetaError> {
    Ok(DatasetRow {
        id: parse_uuid(&record.id, "datasets", "id")?,
        project_id: ProjectId::from_uuid(parse_uuid(&record.project_id, "datasets", "project_id")?),
        name: record.name,
        description: record.description,
        input_schema: record.input_schema,
        expected_schema: record.expected_schema,
        current_version: record.current_version,
        item_count: record.item_count,
        experiment_count: record.experiment_count,
        created_at: record.created_at,
        updated_at: record.updated_at,
    })
}

fn version_from(record: VersionRecord) -> VersionRow {
    VersionRow {
        version: record.version,
        description: record.description,
        created_by: record.created_by,
        created_at: record.created_at,
        counts: VersionCounts::default(),
    }
}

fn item_from(record: ItemRecord) -> Result<ItemRow, MetaError> {
    Ok(ItemRow {
        revision_id: parse_uuid(&record.revision_id, "dataset_items", "revision_id")?,
        item_id: parse_uuid(&record.item_id, "dataset_items", "item_id")?,
        dataset_id: parse_uuid(&record.dataset_id, "dataset_items", "dataset_id")?,
        input: serde_json::from_str(&record.input)
            .map_err(|_| corrupt("dataset_items", "input"))?,
        expected_output: record
            .expected_output
            .map(|text| {
                serde_json::from_str(&text).map_err(|_| corrupt("dataset_items", "expected_output"))
            })
            .transpose()?,
        metadata: serde_json::from_str::<Map<String, Value>>(&record.metadata)
            .map_err(|_| corrupt("dataset_items", "metadata"))?,
        source_trace_id: record
            .source_trace_id
            .map(|bytes| {
                TraceId::from_slice(&bytes).map_err(|_| corrupt("dataset_items", "source_trace_id"))
            })
            .transpose()?,
        source_span_id: record
            .source_span_id
            .map(|bytes| {
                SpanId::from_slice(&bytes).map_err(|_| corrupt("dataset_items", "source_span_id"))
            })
            .transpose()?,
        valid_from_version: record.valid_from_version,
        valid_to_version: record.valid_to_version,
        created_at: record.created_at,
    })
}

fn corrupt(table: &'static str, column: &'static str) -> MetaError {
    MetaError::Corrupt { table, column }
}

fn parse_uuid(value: &str, table: &'static str, column: &'static str) -> Result<Uuid, MetaError> {
    value.parse().map_err(|_| corrupt(table, column))
}

fn trace_bytes(trace: &TraceId) -> Vec<u8> {
    trace.as_bytes().to_vec()
}

fn span_bytes(span: &SpanId) -> Vec<u8> {
    span.as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use cetra_core::ids::{OrgId, ProjectId, SpanId, TraceId};
    use serde_json::{Map, Value, json};
    use uuid::Uuid;

    use super::{
        DUPLICATE_NAME, DatasetFields, DatasetKey, ItemRow, LIVE_ITEM_CAP, NewRevision,
        PROJECT_CAP, TOO_MANY_DATASETS, TOO_MANY_ITEMS, VersionChange, delete_dataset, get_dataset,
        get_item, get_live_item_tx, insert_dataset, list_datasets, list_items, list_versions,
        live_items_tx, update_dataset, write_version,
    };
    use crate::meta::orgs::insert_org;
    use crate::meta::projects::insert_project;
    use crate::meta::{MetaDb, MetaError};

    const NOW: i64 = 1_000;

    async fn org(db: &MetaDb) -> OrgId {
        db.write(|conn| {
            Box::pin(async move { insert_org(conn, OrgId::new_v7(), "Acme", "acme", NOW).await })
        })
        .await
        .unwrap()
        .id
    }

    async fn project(db: &MetaDb, org: OrgId) -> ProjectId {
        db.write(move |conn| {
            Box::pin(async move {
                insert_project(conn, org, "Support", "support", None, NOW)
                    .await
                    .map(|row| row.id)
            })
        })
        .await
        .unwrap()
    }

    fn fields(name: &str) -> DatasetFields {
        DatasetFields {
            name: name.to_owned(),
            description: None,
            input_schema: None,
            expected_schema: None,
        }
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

    fn change(close: Vec<Uuid>, insert: Vec<NewRevision>, description: &str) -> VersionChange {
        VersionChange {
            close,
            insert,
            description: description.to_owned(),
            created_by: Some("user-1".to_owned()),
        }
    }

    async fn create(db: &MetaDb, project: ProjectId, name: &str) -> Uuid {
        let name = name.to_owned();
        db.write(move |conn| {
            Box::pin(async move {
                insert_dataset(conn, Uuid::now_v7(), project, fields(&name), NOW)
                    .await
                    .map(|row| row.id)
            })
        })
        .await
        .unwrap()
    }

    async fn write(db: &MetaDb, dataset: Uuid, change: VersionChange) -> Result<i64, MetaError> {
        db.write(move |conn| {
            Box::pin(async move { write_version(conn, dataset, change, NOW).await })
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn insert_get_list_and_delete() {
        let (_dir, db) = crate::meta::test_db().await;
        let project = project(&db, org(&db).await).await;
        let faq = create(&db, project, "faq").await;
        create(&db, project, "Zeta").await;
        create(&db, project, "alpha").await;

        let names: Vec<String> = list_datasets(db.reader(), project, None)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.name)
            .collect();
        assert_eq!(names, ["alpha", "faq", "Zeta"]);

        assert_eq!(DatasetKey::parse(&faq.to_string()), DatasetKey::Id(faq));
        assert_eq!(DatasetKey::parse("faq"), DatasetKey::Name("faq".to_owned()));
        for key in [DatasetKey::Id(faq), DatasetKey::Name("faq".to_owned())] {
            let row = get_dataset(db.reader(), project, &key)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.id, faq);
            assert_eq!(row.current_version, 0);
            assert_eq!(row.item_count, 0);
            assert_eq!(row.experiment_count, 0);
        }

        let deleted = db
            .write(move |conn| Box::pin(async move { delete_dataset(conn, project, faq).await }))
            .await
            .unwrap();
        assert!(deleted);
        let deleted_again = db
            .write(move |conn| Box::pin(async move { delete_dataset(conn, project, faq).await }))
            .await
            .unwrap();
        assert!(!deleted_again);
        assert!(
            get_dataset(db.reader(), project, &DatasetKey::Id(faq))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn duplicate_name_is_a_conflict() {
        let (_dir, db) = crate::meta::test_db().await;
        let org = org(&db).await;
        let first = project(&db, org).await;
        let second = db
            .write(move |conn| {
                Box::pin(async move {
                    insert_project(conn, org, "Other", "other", None, NOW)
                        .await
                        .map(|row| row.id)
                })
            })
            .await
            .unwrap();
        create(&db, first, "faq").await;
        create(&db, second, "faq").await;

        let duplicate = db
            .write(move |conn| {
                Box::pin(async move {
                    insert_dataset(conn, Uuid::now_v7(), first, fields("faq"), NOW).await
                })
            })
            .await;
        assert!(matches!(
            duplicate,
            Err(MetaError::Conflict(DUPLICATE_NAME))
        ));

        let alpha = create(&db, first, "alpha").await;
        let renamed = db
            .write(move |conn| {
                Box::pin(
                    async move { update_dataset(conn, first, alpha, fields("faq"), NOW).await },
                )
            })
            .await;
        assert!(matches!(renamed, Err(MetaError::Conflict(DUPLICATE_NAME))));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn project_cap() {
        let (_dir, db) = crate::meta::test_db().await;
        let project = project(&db, org(&db).await).await;
        db.write(move |conn| {
            Box::pin(async move {
                for i in 0..PROJECT_CAP {
                    insert_dataset(
                        conn,
                        Uuid::now_v7(),
                        project,
                        fields(&format!("d{i:04}")),
                        NOW,
                    )
                    .await?;
                }
                Ok(())
            })
        })
        .await
        .unwrap();

        let extra = db
            .write(move |conn| {
                Box::pin(async move {
                    insert_dataset(conn, Uuid::now_v7(), project, fields("one-more"), NOW).await
                })
            })
            .await;
        assert!(matches!(extra, Err(MetaError::Conflict(TOO_MANY_DATASETS))));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn versions_follow_the_validity_ranges() {
        let (_dir, db) = crate::meta::test_db().await;
        let project = project(&db, org(&db).await).await;
        let dataset = create(&db, project, "faq").await;
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        let a_v1 = revision(a, json!("a1"));
        let a_v1_id = a_v1.revision_id;
        let a_v2 = revision(a, json!("a2"));
        let a_v2_id = a_v2.revision_id;

        assert_eq!(
            write(
                &db,
                dataset,
                change(vec![], vec![a_v1, revision(b, json!("b1"))], "add 2 items")
            )
            .await
            .unwrap(),
            1
        );
        assert_eq!(
            write(
                &db,
                dataset,
                change(vec![a], vec![a_v2], &format!("edit item {a}"))
            )
            .await
            .unwrap(),
            2
        );
        assert_eq!(
            write(
                &db,
                dataset,
                change(vec![b], vec![], &format!("delete item {b}"))
            )
            .await
            .unwrap(),
            3
        );

        let inputs = |rows: &[ItemRow]| -> Vec<(Uuid, Value)> {
            rows.iter()
                .map(|row| (row.item_id, row.input.clone()))
                .collect()
        };
        let v1 = list_items(db.reader(), dataset, 1, None, 10, None)
            .await
            .unwrap();
        assert_eq!(inputs(&v1), [(a, json!("a1")), (b, json!("b1"))]);
        let v2 = list_items(db.reader(), dataset, 2, None, 10, None)
            .await
            .unwrap();
        assert_eq!(inputs(&v2), [(a, json!("a2")), (b, json!("b1"))]);
        let v3 = list_items(db.reader(), dataset, 3, None, 10, None)
            .await
            .unwrap();
        assert_eq!(inputs(&v3), [(a, json!("a2"))]);
        assert!(
            list_items(db.reader(), dataset, 0, None, 10, None)
                .await
                .unwrap()
                .is_empty()
        );

        let old = get_item(db.reader(), dataset, a, 1).await.unwrap().unwrap();
        assert_eq!(old.revision_id, a_v1_id);
        let new = get_item(db.reader(), dataset, a, 2).await.unwrap().unwrap();
        assert_eq!(new.revision_id, a_v2_id);
        assert!(
            get_item(db.reader(), dataset, b, 3)
                .await
                .unwrap()
                .is_none()
        );

        let row = get_dataset(db.reader(), project, &DatasetKey::Id(dataset))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.current_version, 3);
        assert_eq!(row.item_count, 1);

        let versions = list_versions(db.reader(), dataset, None, 10).await.unwrap();
        let numbers: Vec<i64> = versions.iter().map(|row| row.version).collect();
        assert_eq!(numbers, [3, 2, 1]);
        assert_eq!(versions[0].description, format!("delete item {b}"));
        assert_eq!(versions[2].description, "add 2 items");
        assert_eq!(versions[2].created_by.as_deref(), Some("user-1"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_matches_the_json_text_literally() {
        let (_dir, db) = crate::meta::test_db().await;
        let project = project(&db, org(&db).await).await;
        let dataset = create(&db, project, "faq").await;
        let [order, expected, meta, percent, underscore, letter_x] =
            [(); 6].map(|()| Uuid::now_v7());
        let mut with_expected = revision(expected, json!("question"));
        with_expected.expected_output = Some(json!("pay_42"));
        let mut with_meta = revision(meta, json!("plain"));
        with_meta.metadata = Map::from_iter([("tier".to_owned(), json!("gold"))]);
        let inserts = vec![
            revision(order, json!("Refund the ORDER")),
            with_expected,
            with_meta,
            revision(percent, json!("100% sure")),
            revision(underscore, json!("a_b")),
            revision(letter_x, json!("axb")),
        ];
        write(&db, dataset, change(vec![], inserts, "add 6 items"))
            .await
            .unwrap();

        let ids =
            |rows: Vec<ItemRow>| -> Vec<Uuid> { rows.iter().map(|row| row.item_id).collect() };
        let search = |needle: &'static str| {
            let db = &db;
            async move {
                ids(list_items(db.reader(), dataset, 1, None, 100, Some(needle))
                    .await
                    .unwrap())
            }
        };
        // Case-insensitive, in the input.
        assert_eq!(search("order").await, [order]);
        // In the expected output.
        assert_eq!(search("PAY_42").await, [expected]);
        // In the metadata, by value and by key.
        assert_eq!(search("gold").await, [meta]);
        assert_eq!(search("tier").await, [meta]);
        // LIKE wildcards are literal: `%` matches only the percent sign, `_` only an underscore.
        assert_eq!(search("%").await, [percent]);
        assert_eq!(search("a_b").await, [underscore]);
        assert_eq!(search("x").await, [letter_x]);
        // An absent search lists everything.
        assert_eq!(
            list_items(db.reader(), dataset, 1, None, 100, None)
                .await
                .unwrap()
                .len(),
            6
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_pages_by_item_id() {
        let (_dir, db) = crate::meta::test_db().await;
        let project = project(&db, org(&db).await).await;
        let dataset = create(&db, project, "faq").await;
        let mut matching: Vec<Uuid> = (0..3).map(|_| Uuid::now_v7()).collect();
        matching.sort();
        let mut inserts: Vec<NewRevision> = matching
            .iter()
            .map(|item| revision(*item, json!("needle here")))
            .collect();
        inserts.push(revision(Uuid::now_v7(), json!("other")));
        write(&db, dataset, change(vec![], inserts, "add 4 items"))
            .await
            .unwrap();

        let first = list_items(db.reader(), dataset, 1, None, 1, Some("needle"))
            .await
            .unwrap();
        assert_eq!(first.len(), 2);
        let next = list_items(
            db.reader(),
            dataset,
            1,
            Some(matching[0]),
            1,
            Some("needle"),
        )
        .await
        .unwrap();
        assert_eq!(next.first().map(|row| row.item_id), Some(matching[1]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn paging() {
        let (_dir, db) = crate::meta::test_db().await;
        let project = project(&db, org(&db).await).await;
        let dataset = create(&db, project, "faq").await;
        let mut items: Vec<Uuid> = (0..5).map(|_| Uuid::now_v7()).collect();
        items.sort();
        let inserts = items.iter().map(|item| revision(*item, json!(0))).collect();
        write(&db, dataset, change(vec![], inserts, "add 5 items"))
            .await
            .unwrap();
        write(&db, dataset, change(vec![], vec![], "noop"))
            .await
            .unwrap();
        write(&db, dataset, change(vec![], vec![], "noop"))
            .await
            .unwrap();

        let first = list_items(db.reader(), dataset, 1, None, 2, None)
            .await
            .unwrap();
        assert_eq!(first.len(), 3);
        let after = first[1].item_id;
        let next = list_items(db.reader(), dataset, 1, Some(after), 2, None)
            .await
            .unwrap();
        assert_eq!(next.first().map(|row| row.item_id), Some(items[2]));

        let versions = list_versions(db.reader(), dataset, Some(3), 10)
            .await
            .unwrap();
        let numbers: Vec<i64> = versions.iter().map(|row| row.version).collect();
        assert_eq!(numbers, [2, 1]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn live_item_cap() {
        let (_dir, db) = crate::meta::test_db().await;
        let project = project(&db, org(&db).await).await;
        let dataset = create(&db, project, "faq").await;
        let items: Vec<Uuid> = (0..LIVE_ITEM_CAP).map(|_| Uuid::now_v7()).collect();
        let inserts = items
            .iter()
            .enumerate()
            .map(|(i, item)| revision(*item, json!(i)))
            .collect();
        write(&db, dataset, change(vec![], inserts, "add 50000 items"))
            .await
            .unwrap();

        let extra = write(
            &db,
            dataset,
            change(vec![], vec![revision(Uuid::now_v7(), json!(0))], "add"),
        )
        .await;
        assert!(matches!(extra, Err(MetaError::Conflict(TOO_MANY_ITEMS))));
        let row = get_dataset(db.reader(), project, &DatasetKey::Id(dataset))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.current_version, 1);

        // The count is taken after the close and the insert of one change.
        let replacement = revision(items[0], json!("replaced"));
        assert_eq!(
            write(
                &db,
                dataset,
                change(vec![items[0]], vec![replacement], "edit")
            )
            .await
            .unwrap(),
            2
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn source_ids_round_trip() {
        let (_dir, db) = crate::meta::test_db().await;
        let project = project(&db, org(&db).await).await;
        let dataset = create(&db, project, "faq").await;
        let item = Uuid::now_v7();
        let mut row = revision(item, json!("x"));
        row.source_trace_id =
            Some(TraceId::from_slice(&hex("a1000000000000000000000000000001")).unwrap());
        row.source_span_id = Some(SpanId::from_slice(&hex("b100000000000011")).unwrap());
        let expected = row.clone();
        write(&db, dataset, change(vec![], vec![row], "add from trace"))
            .await
            .unwrap();

        let read = get_item(db.reader(), dataset, item, 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read.source_trace_id, expected.source_trace_id);
        assert_eq!(read.source_span_id, expected.source_span_id);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_live_revision_per_item() {
        let (_dir, db) = crate::meta::test_db().await;
        let project = project(&db, org(&db).await).await;
        let dataset = create(&db, project, "faq").await;
        let item = Uuid::now_v7();
        write(
            &db,
            dataset,
            change(vec![], vec![revision(item, json!(1))], "add"),
        )
        .await
        .unwrap();

        let duplicate = revision(item, json!(2));
        let duplicate_id = duplicate.revision_id.to_string();
        let direct = db
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query(
                        "INSERT INTO dataset_items (revision_id, item_id, dataset_id, input, metadata, \
                         valid_from_version, valid_to_version, created_at) \
                         VALUES (?, ?, ?, '2', '{}', 1, NULL, ?)",
                    )
                    .bind(duplicate_id)
                    .bind(item.to_string())
                    .bind(dataset.to_string())
                    .bind(NOW)
                    .execute(&mut *conn)
                    .await
                    .map_err(MetaError::from)
                })
            })
            .await;
        let violation = direct.is_err_and(|error| {
            matches!(&error, MetaError::Db(db) if db.as_database_error().is_some_and(|d| d.is_unique_violation()))
        });
        assert!(violation);

        let twice = write(
            &db,
            dataset,
            change(
                vec![],
                vec![revision(item, json!(3)), revision(item, json!(4))],
                "twice",
            ),
        )
        .await;
        assert!(twice.is_err());
        let row = get_dataset(db.reader(), project, &DatasetKey::Id(dataset))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.current_version, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn live_reads_inside_a_transaction() {
        let (_dir, db) = crate::meta::test_db().await;
        let project = project(&db, org(&db).await).await;
        let dataset = create(&db, project, "faq").await;
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        write(
            &db,
            dataset,
            change(
                vec![],
                vec![revision(a, json!("a")), revision(b, json!("b"))],
                "add 2",
            ),
        )
        .await
        .unwrap();
        write(
            &db,
            dataset,
            change(vec![a], vec![revision(a, json!("a2"))], "edit"),
        )
        .await
        .unwrap();

        let (page, live_a) = db
            .write(move |conn| {
                Box::pin(async move {
                    let page = live_items_tx(conn, dataset, None, 10).await?;
                    let live_a = get_live_item_tx(conn, dataset, a).await?;
                    Ok((page, live_a))
                })
            })
            .await
            .unwrap();
        let ids: Vec<Uuid> = page.iter().map(|row| row.item_id).collect();
        assert_eq!(ids.len(), 2);
        assert_eq!(live_a.map(|row| row.input), Some(json!("a2")));
    }

    #[test]
    fn debug_prints_no_content() {
        let mut row = revision(Uuid::now_v7(), json!("canary-debug"));
        row.source_span_id = Some(SpanId::from_slice(&hex("b100000000000011")).unwrap());
        let item = ItemRow {
            revision_id: row.revision_id,
            item_id: row.item_id,
            dataset_id: Uuid::now_v7(),
            input: row.input.clone(),
            expected_output: None,
            metadata: Map::new(),
            source_trace_id: None,
            source_span_id: row.source_span_id,
            valid_from_version: 1,
            valid_to_version: None,
            created_at: NOW,
        };
        let printed = format!("{item:?} {row:?}");
        assert!(!printed.contains("canary-debug"), "{printed}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn view_rows_are_the_current_versions() {
        use super::{ViewItems, view_rows};
        let (_dir, db) = crate::meta::test_db().await;
        let org_id = org(&db).await;
        let project = project(&db, org_id).await;
        let faq = create(&db, project, "faq").await;
        let other = create(&db, project, "other").await;
        let (a, b, c) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        write(
            &db,
            faq,
            change(
                vec![],
                vec![revision(a, json!("a1")), revision(b, json!("b1"))],
                "add 2 items",
            ),
        )
        .await
        .unwrap();
        write(
            &db,
            faq,
            change(vec![a], vec![revision(a, json!("a2"))], "edit a"),
        )
        .await
        .unwrap();
        write(
            &db,
            other,
            change(vec![], vec![revision(c, json!("c1"))], "add c"),
        )
        .await
        .unwrap();
        write(&db, other, change(vec![c], vec![], "delete c"))
            .await
            .unwrap();
        // Another project's items are not in the view.
        let elsewhere = db
            .write(move |conn| {
                Box::pin(async move {
                    insert_project(conn, org_id, "Billing", "billing", None, NOW)
                        .await
                        .map(|row| row.id)
                })
            })
            .await
            .unwrap();
        let dataset = create(&db, elsewhere, "elsewhere").await;
        write(
            &db,
            dataset,
            change(vec![], vec![revision(Uuid::now_v7(), json!("x"))], "add"),
        )
        .await
        .unwrap();

        let ViewItems::Rows(rows) = view_rows(db.reader(), project, 100, 10_000).await.unwrap()
        else {
            panic!("the view is within its budget");
        };
        let mut found: Vec<(Uuid, i64, String)> = rows
            .iter()
            .map(|row| (row.item_id, row.dataset_version, row.input.clone()))
            .collect();
        found.sort();
        let mut expected = vec![(a, 2, "\"a2\"".to_owned()), (b, 2, "\"b1\"".to_owned())];
        expected.sort();
        assert_eq!(found, expected);
        assert!(rows.iter().all(|row| row.dataset_name == "faq"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn view_rows_stop_at_the_budget() {
        use super::{ViewItems, view_rows};
        let (_dir, db) = crate::meta::test_db().await;
        let project = project(&db, org(&db).await).await;
        let dataset = create(&db, project, "faq").await;
        // Each item is `"x"` (3 bytes) and `{}` (2 bytes): 5 bytes, 15 in all.
        let items = (0..3)
            .map(|_| revision(Uuid::now_v7(), json!("x")))
            .collect();
        write(&db, dataset, change(vec![], items, "add 3"))
            .await
            .unwrap();

        let too_many_rows = view_rows(db.reader(), project, 2, 10_000).await.unwrap();
        assert!(matches!(too_many_rows, ViewItems::TooLarge));
        let too_much_text = view_rows(db.reader(), project, 100, 14).await.unwrap();
        assert!(matches!(too_much_text, ViewItems::TooLarge));
        let exact = view_rows(db.reader(), project, 3, 15).await.unwrap();
        let ViewItems::Rows(rows) = exact else {
            panic!("three rows of 15 bytes fit the budget");
        };
        assert_eq!(rows.len(), 3);
    }

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }
}
