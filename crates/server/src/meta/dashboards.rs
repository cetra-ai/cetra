//! `dashboards` rows (spec 04 § Dashboards, D-C-16).
//!
//! The SQL text of this module is built only from the constants here; every caller value is
//! bound.

use cetra_core::ids::ProjectId;
use sqlx::{AssertSqlSafe, SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::MetaError;
use crate::api::dashboards::{Dashboard, DashboardSummary, Definition};

/// Most dashboards per project (spec 04 § Dashboards).
pub(crate) const MAX_DASHBOARDS_PER_PROJECT: i64 = 100;

/// Display name of a project's stored Overview.
pub(crate) const OVERVIEW_NAME: &str = "Overview";

/// Columns of a dashboard without its definition, in the order [`InfoRecord`] declares them.
const INFO_COLUMNS: &str =
    "id, name, description, version, created_by, updated_by, created_at, updated_at";

/// A dashboard without its definition.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DashboardInfo {
    /// UUIDv7.
    pub(crate) id: Uuid,
    /// Display name.
    pub(crate) name: String,
    /// Free text.
    pub(crate) description: Option<String>,
    /// Incremented by every change.
    pub(crate) version: i64,
    /// Creator. Absent once the user is gone.
    pub(crate) created_by: Option<Uuid>,
    /// Last editor. Absent once the user is gone.
    pub(crate) updated_by: Option<Uuid>,
    /// Creation time, nanoseconds.
    pub(crate) created_at: i64,
    /// Last update, nanoseconds.
    pub(crate) updated_at: i64,
}

/// A dashboard with its definition.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DashboardRow {
    /// Everything but the definition.
    pub(crate) info: DashboardInfo,
    /// The widgets.
    pub(crate) definition: Definition,
}

/// One page of [`list`].
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DashboardPage {
    /// Rows in name, then id order.
    pub(crate) rows: Vec<DashboardInfo>,
    /// `(name, id)` of the last row when more rows follow, else `None`.
    pub(crate) next: Option<(String, Uuid)>,
}

/// Columns of a new dashboard. `name` and `description` are already checked.
#[derive(Debug, Clone)]
pub(crate) struct NewDashboard {
    /// Project the dashboard belongs to.
    pub(crate) project_id: ProjectId,
    /// Display name.
    pub(crate) name: String,
    /// Free text.
    pub(crate) description: Option<String>,
    /// The widgets. Already validated.
    pub(crate) definition: Definition,
    /// Creator.
    pub(crate) user: Uuid,
}

/// What [`update`] changes. `None` leaves the column alone.
#[derive(Debug, Clone, Default)]
pub(crate) struct DashboardChanges {
    /// New display name.
    pub(crate) name: Option<String>,
    /// New free text: `Some(None)` clears it (NULL), `Some(Some(text))` sets it.
    pub(crate) description: Option<Option<String>>,
    /// New widgets. Already validated.
    pub(crate) definition: Option<Definition>,
}

/// What [`update`] did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum UpdateOutcome {
    /// The row changed; this is the row after the change.
    Updated(DashboardRow),
    /// The stored version differs from the expected one. Carries the stored version.
    Stale(i64),
    /// There is no such dashboard in the project.
    Missing,
}

/// What [`save_overview`] did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum OverviewSave {
    /// Stored. `created` is true when this save made the project's copy.
    Saved { row: DashboardRow, created: bool },
    /// The expected version is not the stored one (0 when nothing is stored). Carries the stored version.
    Stale(i64),
}

/// A `dashboards` row without its definition, as SQLite returns it.
#[derive(Debug, sqlx::FromRow)]
struct InfoRecord {
    id: String,
    name: String,
    description: Option<String>,
    version: i64,
    created_by: Option<String>,
    updated_by: Option<String>,
    created_at: i64,
    updated_at: i64,
}

/// A `dashboards` row with its definition.
#[derive(Debug, sqlx::FromRow)]
struct FullRecord {
    #[sqlx(flatten)]
    info: InfoRecord,
    definition: String,
}

fn corrupt(column: &'static str) -> MetaError {
    MetaError::Corrupt {
        table: "dashboards",
        column,
    }
}

fn uuid_of(text: &str, column: &'static str) -> Result<Uuid, MetaError> {
    Uuid::parse_str(text).map_err(|_| corrupt(column))
}

fn info_from(record: InfoRecord) -> Result<DashboardInfo, MetaError> {
    Ok(DashboardInfo {
        id: uuid_of(&record.id, "id")?,
        name: record.name,
        description: record.description,
        version: record.version,
        created_by: record
            .created_by
            .as_deref()
            .map(|text| uuid_of(text, "created_by"))
            .transpose()?,
        updated_by: record
            .updated_by
            .as_deref()
            .map(|text| uuid_of(text, "updated_by"))
            .transpose()?,
        created_at: record.created_at,
        updated_at: record.updated_at,
    })
}

fn row_from(record: FullRecord) -> Result<DashboardRow, MetaError> {
    let definition = serde_json::from_str::<Definition>(&record.definition)
        .map_err(|_| corrupt("definition"))?;
    Ok(DashboardRow {
        info: info_from(record.info)?,
        definition,
    })
}

fn definition_text(definition: &Definition) -> Result<String, MetaError> {
    serde_json::to_string(definition).map_err(|_| corrupt("definition"))
}

/// Dashboards of `project`, ordered by name then id. `after` is the `(name, id)` of the
/// previous page's last dashboard: rows after it in that order follow. A dashboard that has
/// been deleted since does not matter, since the position is the key itself. `limit` is
/// 1–1000; the route checks it.
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails or a stored value does not decode.
pub(crate) async fn list(
    pool: &SqlitePool,
    project: ProjectId,
    limit: usize,
    after: Option<(&str, Uuid)>,
) -> Result<DashboardPage, MetaError> {
    let fetch = limit
        .checked_add(1)
        .and_then(|n| i64::try_from(n).ok())
        .ok_or(MetaError::Conflict("page size is too large"))?;
    let project_text = project.to_string();
    let after_id = after.map(|(_, id)| id.to_string());

    let mut sql =
        format!("SELECT {INFO_COLUMNS} FROM dashboards WHERE project_id = ? AND kind = 'custom'");
    if after.is_some() {
        sql.push_str(" AND (name > ? OR (name = ? AND id > ?))");
    }
    sql.push_str(" ORDER BY name, id LIMIT ?");
    let mut query = sqlx::query_as::<_, InfoRecord>(AssertSqlSafe(sql)).bind(&project_text);
    if let (Some((name, _)), Some(id)) = (after, &after_id) {
        query = query.bind(name).bind(name).bind(id.as_str());
    }
    let records = query.bind(fetch).fetch_all(pool).await?;

    let mut rows = records
        .into_iter()
        .map(info_from)
        .collect::<Result<Vec<_>, _>>()?;
    let next = if rows.len() > limit {
        rows.truncate(limit);
        rows.last().map(|row| (row.name.clone(), row.id))
    } else {
        None
    };
    Ok(DashboardPage { rows, next })
}

/// The dashboard `id` of `project`, with its definition.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn get(
    pool: &SqlitePool,
    project: ProjectId,
    id: Uuid,
) -> Result<Option<DashboardRow>, MetaError> {
    let sql = format!(
        "SELECT {INFO_COLUMNS}, definition FROM dashboards \
         WHERE project_id = ? AND id = ? AND kind = 'custom'"
    );
    let record: Option<FullRecord> = sqlx::query_as(AssertSqlSafe(sql))
        .bind(project.to_string())
        .bind(id.to_string())
        .fetch_optional(pool)
        .await?;
    record.map(row_from).transpose()
}

/// Dashboards in `project`, read on the writer so a check and a write see the same rows.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails.
pub(crate) async fn count(
    conn: &mut SqliteConnection,
    project: ProjectId,
) -> Result<i64, MetaError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM dashboards WHERE project_id = ? AND kind = 'custom'",
    )
    .bind(project.to_string())
    .fetch_one(&mut *conn)
    .await?;
    Ok(count)
}

/// Inserts a dashboard at version 1. When `project` already has [`MAX_DASHBOARDS_PER_PROJECT`]
/// dashboards, returns `MetaError::Conflict("too many dashboards")`.
///
/// # Errors
///
/// Returns [`MetaError`] when the count or the insert fails.
pub(crate) async fn insert(
    conn: &mut SqliteConnection,
    dashboard: NewDashboard,
    now: i64,
) -> Result<DashboardRow, MetaError> {
    if count(conn, dashboard.project_id).await? >= MAX_DASHBOARDS_PER_PROJECT {
        return Err(MetaError::Conflict("too many dashboards"));
    }
    let id = Uuid::now_v7();
    let text = definition_text(&dashboard.definition)?;
    sqlx::query(
        "INSERT INTO dashboards \
         (id, project_id, name, description, definition, version, created_by, updated_by, \
          created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, 1, ?, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(dashboard.project_id.to_string())
    .bind(dashboard.name.as_str())
    .bind(dashboard.description.as_deref())
    .bind(text)
    .bind(dashboard.user.to_string())
    .bind(dashboard.user.to_string())
    .bind(now)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(DashboardRow {
        info: DashboardInfo {
            id,
            name: dashboard.name,
            description: dashboard.description,
            version: 1,
            created_by: Some(dashboard.user),
            updated_by: Some(dashboard.user),
            created_at: now,
            updated_at: now,
        },
        definition: dashboard.definition,
    })
}

/// Applies `changes` to the dashboard `id` of `project` when its stored version is
/// `expected_version`. The compare-and-set is one `UPDATE`: the version rises by one, and
/// `updated_at` and `updated_by` are set. Otherwise the stored version comes back, or
/// [`UpdateOutcome::Missing`] when there is no such dashboard.
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails or a stored value does not decode.
pub(crate) async fn update(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
    expected_version: i64,
    changes: DashboardChanges,
    user: Uuid,
    now: i64,
) -> Result<UpdateOutcome, MetaError> {
    let definition = changes
        .definition
        .as_ref()
        .map(definition_text)
        .transpose()?;
    let result = sqlx::query(
        "UPDATE dashboards SET \
         name = COALESCE(?, name), \
         description = CASE WHEN ? THEN ? ELSE description END, \
         definition = COALESCE(?, definition), \
         version = version + 1, updated_at = ?, updated_by = ? \
         WHERE project_id = ? AND id = ? AND version = ? AND kind = 'custom'",
    )
    .bind(changes.name.as_deref())
    .bind(changes.description.is_some())
    .bind(changes.description.clone().flatten())
    .bind(definition)
    .bind(now)
    .bind(user.to_string())
    .bind(project.to_string())
    .bind(id.to_string())
    .bind(expected_version)
    .execute(&mut *conn)
    .await?;
    if result.rows_affected() == 1 {
        let row = get_on(conn, project, id).await?;
        return Ok(row.map_or(UpdateOutcome::Missing, UpdateOutcome::Updated));
    }
    let stored: Option<i64> = sqlx::query_scalar(
        "SELECT version FROM dashboards WHERE project_id = ? AND id = ? AND kind = 'custom'",
    )
    .bind(project.to_string())
    .bind(id.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    Ok(stored.map_or(UpdateOutcome::Missing, UpdateOutcome::Stale))
}

/// [`get`] on the writer.
async fn get_on(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
) -> Result<Option<DashboardRow>, MetaError> {
    let sql = format!(
        "SELECT {INFO_COLUMNS}, definition FROM dashboards \
         WHERE project_id = ? AND id = ? AND kind = 'custom'"
    );
    let record: Option<FullRecord> = sqlx::query_as(AssertSqlSafe(sql))
        .bind(project.to_string())
        .bind(id.to_string())
        .fetch_optional(&mut *conn)
        .await?;
    record.map(row_from).transpose()
}

/// Deletes the dashboard `id` of `project`. Returns whether a row was deleted.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the delete fails.
pub(crate) async fn delete(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
) -> Result<bool, MetaError> {
    let result =
        sqlx::query("DELETE FROM dashboards WHERE project_id = ? AND id = ? AND kind = 'custom'")
            .bind(project.to_string())
            .bind(id.to_string())
            .execute(&mut *conn)
            .await?;
    Ok(result.rows_affected() > 0)
}

/// The stored Overview of the project behind `conn`: the row of kind `overview`.
async fn overview_on(
    conn: &mut SqliteConnection,
    project: ProjectId,
) -> Result<Option<DashboardRow>, MetaError> {
    let sql = format!(
        "SELECT {INFO_COLUMNS}, definition FROM dashboards \
         WHERE project_id = ? AND kind = 'overview'"
    );
    let record: Option<FullRecord> = sqlx::query_as(AssertSqlSafe(sql))
        .bind(project.to_string())
        .fetch_optional(&mut *conn)
        .await?;
    record.map(row_from).transpose()
}

/// The project's stored Overview, if a member saved one. Reads like [`get`].
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn get_overview(
    pool: &SqlitePool,
    project: ProjectId,
) -> Result<Option<DashboardRow>, MetaError> {
    let mut conn = pool.acquire().await?;
    overview_on(&mut conn, project).await
}

/// Creates (`expected` `None`, nothing stored) or updates (`expected` is the stored version) the
/// project's Overview. Writes like [`update`]: a stale or missing expectation comes back as
/// [`OverviewSave::Stale`] with the stored version, 0 when nothing is stored.
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails or a stored value does not decode.
pub(crate) async fn save_overview(
    conn: &mut SqliteConnection,
    project: ProjectId,
    expected: Option<i64>,
    definition: &Definition,
    user: Uuid,
    now: i64,
) -> Result<OverviewSave, MetaError> {
    let stored: Option<i64> = sqlx::query_scalar(
        "SELECT version FROM dashboards WHERE project_id = ? AND kind = 'overview'",
    )
    .bind(project.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    let text = definition_text(definition)?;
    match (stored, expected) {
        (None, None) => {
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO dashboards \
                 (id, project_id, name, description, definition, version, kind, created_by, \
                  updated_by, created_at, updated_at) \
                 VALUES (?, ?, ?, NULL, ?, 1, 'overview', ?, ?, ?, ?)",
            )
            .bind(id.to_string())
            .bind(project.to_string())
            .bind(OVERVIEW_NAME)
            .bind(text)
            .bind(user.to_string())
            .bind(user.to_string())
            .bind(now)
            .bind(now)
            .execute(&mut *conn)
            .await?;
            let row = overview_on(conn, project)
                .await?
                .ok_or_else(|| corrupt("id"))?;
            Ok(OverviewSave::Saved { row, created: true })
        }
        (Some(version), Some(expected)) if version == expected => {
            sqlx::query(
                "UPDATE dashboards SET definition = ?, version = version + 1, updated_at = ?, \
                 updated_by = ? WHERE project_id = ? AND kind = 'overview'",
            )
            .bind(text)
            .bind(now)
            .bind(user.to_string())
            .bind(project.to_string())
            .execute(&mut *conn)
            .await?;
            let row = overview_on(conn, project)
                .await?
                .ok_or_else(|| corrupt("id"))?;
            Ok(OverviewSave::Saved {
                row,
                created: false,
            })
        }
        (None, Some(_)) => Ok(OverviewSave::Stale(0)),
        (Some(version), _) => Ok(OverviewSave::Stale(version)),
    }
}

/// Deletes the project's Overview, so the preset shows again. Returns whether a row was deleted.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the delete fails.
pub(crate) async fn reset_overview(
    conn: &mut SqliteConnection,
    project: ProjectId,
) -> Result<bool, MetaError> {
    let result = sqlx::query("DELETE FROM dashboards WHERE project_id = ? AND kind = 'overview'")
        .bind(project.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(result.rows_affected() > 0)
}

impl From<DashboardInfo> for DashboardSummary {
    fn from(info: DashboardInfo) -> Self {
        Self {
            id: info.id,
            name: info.name,
            description: info.description,
            version: info.version,
            created_by: info.created_by,
            updated_by: info.updated_by,
            created_at: info.created_at,
            updated_at: info.updated_at,
        }
    }
}

impl From<DashboardRow> for Dashboard {
    fn from(row: DashboardRow) -> Self {
        let info = row.info;
        Self {
            id: info.id,
            name: info.name,
            description: info.description,
            definition: row.definition,
            version: info.version,
            created_by: info.created_by,
            updated_by: info.updated_by,
            created_at: info.created_at,
            updated_at: info.updated_at,
        }
    }
}
