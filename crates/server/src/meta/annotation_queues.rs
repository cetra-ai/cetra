//! `annotation_queues`, `annotation_queue_definitions` and `annotation_queue_items` rows (spec 04
//! § Annotation queues, D-AQ-2–D-AQ-9). SQL text is built only from constants here; every caller
//! value is bound.

use std::collections::{HashMap, HashSet};

use cetra_core::ids::{OrgId, ProjectId};
use sqlx::{AssertSqlSafe, QueryBuilder, Sqlite, SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::{MetaError, unique_or};

/// Most queues per project (D-AQ-8).
pub(crate) const MAX_QUEUES_PER_PROJECT: u64 = 100;
/// Most items per queue (D-AQ-8).
pub(crate) const MAX_ITEMS_PER_QUEUE: u64 = 100_000;
/// Most definitions per queue (D-AQ-7).
pub(crate) const MAX_DEFINITIONS: usize = 50;
/// The reviewer lease, in nanoseconds: 5 minutes (D-AQ-4).
pub(crate) const LEASE_NS: i64 = 300_000_000_000;

const QUEUES: &str = "annotation_queues";
const DEFINITIONS: &str = "annotation_queue_definitions";
const ITEMS: &str = "annotation_queue_items";

/// Columns of a queue, in the order [`QueueRecord`] declares them.
const QUEUE_COLUMNS: &str = "id, project_id, name, description, instructions, target, version, \
     created_by, created_at, updated_at";

/// Columns of an item, in the order [`ItemRecord`] declares them.
const ITEM_COLUMNS: &str = "id, queue_id, project_id, trace_id, span_id, session_id, target_time, \
     status, skip_reason, leased_by_user_id, lease_expires_at, done_by_user_id, done_at, \
     added_by_kind, added_by_id, created_at, updated_at";

/// What a queue's items are (D-AQ-2). Stored as `trace`, `span` or `session`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    Trace,
    Span,
    Session,
}

impl Target {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Span => "span",
            Self::Session => "session",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "trace" => Some(Self::Trace),
            "span" => Some(Self::Span),
            "session" => Some(Self::Session),
            _ => None,
        }
    }
}

/// An item's state (D-AQ-5). Stored as `pending`, `completed` or `skipped`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Pending,
    Completed,
    Skipped,
}

impl Status {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Completed => "completed",
            Self::Skipped => "skipped",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "pending" => Some(Self::Pending),
            "completed" => Some(Self::Completed),
            "skipped" => Some(Self::Skipped),
            _ => None,
        }
    }
}

/// Who added an item. Stored as `user`, `pat` or `key`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AddedByKind {
    User,
    Pat,
    Key,
}

impl AddedByKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Pat => "pat",
            Self::Key => "key",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "user" => Some(Self::User),
            "pat" => Some(Self::Pat),
            "key" => Some(Self::Key),
            _ => None,
        }
    }
}

/// Item counts of one queue, by status.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Counts {
    pub(crate) pending: u64,
    pub(crate) completed: u64,
    pub(crate) skipped: u64,
}

/// One queue with its definitions in position order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueueRow {
    pub(crate) id: Uuid,
    pub(crate) project_id: ProjectId,
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) instructions: Option<String>,
    pub(crate) target: Target,
    /// Score definitions by position.
    pub(crate) definition_ids: Vec<Uuid>,
    /// Incremented by every update; an update names the version it read.
    pub(crate) version: i64,
    /// Creator. Absent once the user is gone.
    pub(crate) created_by: Option<Uuid>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

/// One queue item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ItemRow {
    pub(crate) id: Uuid,
    pub(crate) queue_id: Uuid,
    pub(crate) project_id: ProjectId,
    /// Lowercase hex. Trace and span items.
    pub(crate) trace_id: Option<String>,
    /// Lowercase hex. Span items.
    pub(crate) span_id: Option<String>,
    /// Session items.
    pub(crate) session_id: Option<String>,
    /// Nanoseconds: the trace start, or the session's earliest visible trace start.
    pub(crate) target_time: i64,
    pub(crate) status: Status,
    pub(crate) skip_reason: Option<String>,
    pub(crate) leased_by_user_id: Option<Uuid>,
    pub(crate) lease_expires_at: Option<i64>,
    pub(crate) done_by_user_id: Option<Uuid>,
    pub(crate) done_at: Option<i64>,
    pub(crate) added_by_kind: AddedByKind,
    pub(crate) added_by_id: Uuid,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

/// A queue to insert. `definition_ids` is in position order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewQueue {
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) instructions: Option<String>,
    pub(crate) target: Target,
    pub(crate) definition_ids: Vec<Uuid>,
    pub(crate) created_by: Option<Uuid>,
}

/// What [`update_tx`] changes. `None` leaves a field alone. `Some(None)` clears
/// `description` or `instructions`. `definition_ids` replaces the whole list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct QueueChanges {
    pub(crate) name: Option<String>,
    pub(crate) description: Option<Option<String>>,
    pub(crate) instructions: Option<Option<String>>,
    pub(crate) definition_ids: Option<Vec<Uuid>>,
}

/// What [`insert_queue_tx`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QueueCreate {
    Created(QueueRow),
    /// The org's queues plus this one would pass the plan limit. Carries the limit.
    OverPlanLimit(u64),
    /// These positions of `definition_ids` are not live definitions of the project.
    UnknownDefinitions(Vec<usize>),
}

/// What [`update_tx`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QueueUpdate {
    Updated(QueueRow),
    /// The stored version differs from the expected one. Carries the stored version.
    Stale(i64),
    /// There is no such queue in the project.
    Missing,
    /// These positions of `definition_ids` are not live definitions of the project.
    UnknownDefinitions(Vec<usize>),
}

/// One target to insert. Exactly the fields of the queue's kind are set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NewItem {
    pub(crate) trace_id: Option<String>,
    pub(crate) span_id: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) target_time: i64,
}

/// What [`insert_items_tx`] did. `existing` counts the targets that were already in the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ItemsInsert {
    Done {
        added: u64,
        existing: u64,
    },
    /// The queue would pass [`MAX_ITEMS_PER_QUEUE`]. Nothing was added.
    TooMany,
}

/// Which queues [`list`] keeps: those holding a visible item with this trace id (trace and span
/// items) or this session id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Holding<'a> {
    Trace(&'a str),
    Session(&'a str),
}

/// A `annotation_queues` row, as SQLite returns it.
#[derive(Debug, sqlx::FromRow)]
struct QueueRecord {
    id: String,
    project_id: String,
    name: String,
    description: Option<String>,
    instructions: Option<String>,
    target: String,
    version: i64,
    created_by: Option<String>,
    created_at: i64,
    updated_at: i64,
}

/// A `annotation_queue_items` row, as SQLite returns it.
#[derive(Debug, sqlx::FromRow)]
struct ItemRecord {
    id: String,
    queue_id: String,
    project_id: String,
    trace_id: Option<String>,
    span_id: Option<String>,
    session_id: Option<String>,
    target_time: i64,
    status: String,
    skip_reason: Option<String>,
    leased_by_user_id: Option<String>,
    lease_expires_at: Option<i64>,
    done_by_user_id: Option<String>,
    done_at: Option<i64>,
    added_by_kind: String,
    added_by_id: String,
    created_at: i64,
    updated_at: i64,
}

fn corrupt(table: &'static str, column: &'static str) -> MetaError {
    MetaError::Corrupt { table, column }
}

fn uuid_of(text: &str, table: &'static str, column: &'static str) -> Result<Uuid, MetaError> {
    Uuid::parse_str(text).map_err(|_| corrupt(table, column))
}

fn optional_uuid(
    text: Option<&str>,
    table: &'static str,
    column: &'static str,
) -> Result<Option<Uuid>, MetaError> {
    text.map(|text| uuid_of(text, table, column)).transpose()
}

fn queue_from(record: QueueRecord, definition_ids: Vec<Uuid>) -> Result<QueueRow, MetaError> {
    let target = Target::parse(&record.target).ok_or_else(|| corrupt(QUEUES, "target"))?;
    Ok(QueueRow {
        id: uuid_of(&record.id, QUEUES, "id")?,
        project_id: ProjectId::from_uuid(uuid_of(&record.project_id, QUEUES, "project_id")?),
        name: record.name,
        description: record.description,
        instructions: record.instructions,
        target,
        definition_ids,
        version: record.version,
        created_by: optional_uuid(record.created_by.as_deref(), QUEUES, "created_by")?,
        created_at: record.created_at,
        updated_at: record.updated_at,
    })
}

fn item_from(record: ItemRecord) -> Result<ItemRow, MetaError> {
    let status = Status::parse(&record.status).ok_or_else(|| corrupt(ITEMS, "status"))?;
    let added_by_kind =
        AddedByKind::parse(&record.added_by_kind).ok_or_else(|| corrupt(ITEMS, "added_by_kind"))?;
    Ok(ItemRow {
        id: uuid_of(&record.id, ITEMS, "id")?,
        queue_id: uuid_of(&record.queue_id, ITEMS, "queue_id")?,
        project_id: ProjectId::from_uuid(uuid_of(&record.project_id, ITEMS, "project_id")?),
        trace_id: record.trace_id,
        span_id: record.span_id,
        session_id: record.session_id,
        target_time: record.target_time,
        status,
        skip_reason: record.skip_reason,
        leased_by_user_id: optional_uuid(
            record.leased_by_user_id.as_deref(),
            ITEMS,
            "leased_by_user_id",
        )?,
        lease_expires_at: record.lease_expires_at,
        done_by_user_id: optional_uuid(
            record.done_by_user_id.as_deref(),
            ITEMS,
            "done_by_user_id",
        )?,
        done_at: record.done_at,
        added_by_kind,
        added_by_id: uuid_of(&record.added_by_id, ITEMS, "added_by_id")?,
        created_at: record.created_at,
        updated_at: record.updated_at,
    })
}

/// Pushes `AND target_time >= ?` when `visible_from` is set.
fn push_visible(builder: &mut QueryBuilder<Sqlite>, visible_from: Option<i64>) {
    if let Some(from) = visible_from {
        builder.push(" AND target_time >= ").push_bind(from);
    }
}

/// Pushes the ids as a bound list and closes it. The caller has written `IN (`.
fn push_uuids(builder: &mut QueryBuilder<Sqlite>, ids: &[Uuid]) {
    let mut list = builder.separated(", ");
    for id in ids {
        list.push_bind(id.to_string());
    }
    list.push_unseparated(")");
}

/// The indexes of `ids` that are not live (non-archived) definitions of `project`.
async fn live_definitions_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    ids: &[Uuid],
) -> Result<Vec<usize>, MetaError> {
    let mut missing = Vec::new();
    for (index, id) in ids.iter().enumerate() {
        let live: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM score_definitions \
             WHERE project_id = ? AND id = ? AND archived_at IS NULL",
        )
        .bind(project.to_string())
        .bind(id.to_string())
        .fetch_optional(&mut *conn)
        .await?;
        if live.is_none() {
            missing.push(index);
        }
    }
    Ok(missing)
}

/// Writes the definition rows of `queue` with positions 0..n.
async fn insert_definition_rows(
    conn: &mut SqliteConnection,
    queue: Uuid,
    definition_ids: &[Uuid],
) -> Result<(), MetaError> {
    for (position, definition) in definition_ids.iter().enumerate() {
        sqlx::query(
            "INSERT INTO annotation_queue_definitions (queue_id, definition_id, position) \
             VALUES (?, ?, ?)",
        )
        .bind(queue.to_string())
        .bind(definition.to_string())
        .bind(position as i64)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// The queue count of `org`: every queue of every project in it.
pub(crate) async fn count_org_queues_tx(
    conn: &mut SqliteConnection,
    org: OrgId,
) -> Result<u64, MetaError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM annotation_queues \
         WHERE project_id IN (SELECT id FROM projects WHERE org_id = ?)",
    )
    .bind(org.to_string())
    .fetch_one(&mut *conn)
    .await?;
    // A count that does not fit is treated as at the limit.
    Ok(u64::try_from(count).unwrap_or(u64::MAX))
}

/// Inserts a queue at version 1 with its definitions.
///
/// Order: the project's queue cap, the org's plan limit (`plan_limit` = `None` is unlimited),
/// then the definitions. A name that exists in the project is
/// `MetaError::Conflict("an annotation queue with this name exists")`.
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails, or with `Conflict` for the project cap.
pub(crate) async fn insert_queue_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    org: OrgId,
    queue: NewQueue,
    plan_limit: Option<u64>,
    now: i64,
) -> Result<QueueCreate, MetaError> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM annotation_queues WHERE project_id = ?")
            .bind(project.to_string())
            .fetch_one(&mut *conn)
            .await?;
    let count = u64::try_from(count).unwrap_or(u64::MAX);
    if count >= MAX_QUEUES_PER_PROJECT {
        return Err(MetaError::Conflict("too many annotation queues"));
    }
    if let Some(limit) = plan_limit {
        let org_count = count_org_queues_tx(&mut *conn, org).await?;
        if crate::entitlements::over_limit(Some(limit), org_count, 1) {
            return Ok(QueueCreate::OverPlanLimit(limit));
        }
    }
    let missing = live_definitions_tx(&mut *conn, project, &queue.definition_ids).await?;
    if !missing.is_empty() {
        return Ok(QueueCreate::UnknownDefinitions(missing));
    }
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO annotation_queues \
         (id, project_id, name, description, instructions, target, version, created_by, \
          created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, 1, ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(project.to_string())
    .bind(queue.name.as_str())
    .bind(queue.description.as_deref())
    .bind(queue.instructions.as_deref())
    .bind(queue.target.as_str())
    .bind(queue.created_by.map(|user| user.to_string()))
    .bind(now)
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(|error| unique_or(error, "an annotation queue with this name exists"))?;
    insert_definition_rows(&mut *conn, id, &queue.definition_ids).await?;
    Ok(QueueCreate::Created(QueueRow {
        id,
        project_id: project,
        name: queue.name,
        description: queue.description,
        instructions: queue.instructions,
        target: queue.target,
        definition_ids: queue.definition_ids,
        version: 1,
        created_by: queue.created_by,
        created_at: now,
        updated_at: now,
    }))
}

/// The definition ids of `queue`, by position.
async fn definition_ids_on(
    conn: &mut SqliteConnection,
    queue: Uuid,
) -> Result<Vec<Uuid>, MetaError> {
    let texts: Vec<String> = sqlx::query_scalar(
        "SELECT definition_id FROM annotation_queue_definitions \
         WHERE queue_id = ? ORDER BY position",
    )
    .bind(queue.to_string())
    .fetch_all(&mut *conn)
    .await?;
    texts
        .iter()
        .map(|text| uuid_of(text, DEFINITIONS, "definition_id"))
        .collect()
}

/// The definition ids of every queue of `project`, by queue, in position order.
async fn definitions_by_queue(
    pool: &SqlitePool,
    project: ProjectId,
) -> Result<HashMap<Uuid, Vec<Uuid>>, MetaError> {
    let records: Vec<(String, String)> = sqlx::query_as(
        "SELECT d.queue_id, d.definition_id FROM annotation_queue_definitions d \
         JOIN annotation_queues q ON q.id = d.queue_id \
         WHERE q.project_id = ? ORDER BY d.queue_id, d.position",
    )
    .bind(project.to_string())
    .fetch_all(pool)
    .await?;
    let mut by_queue: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for (queue, definition) in records {
        by_queue
            .entry(uuid_of(&queue, DEFINITIONS, "queue_id")?)
            .or_default()
            .push(uuid_of(&definition, DEFINITIONS, "definition_id")?);
    }
    Ok(by_queue)
}

/// The queue `id` of `project`, with its definitions.
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails or a stored value does not decode.
pub(crate) async fn get(
    pool: &SqlitePool,
    project: ProjectId,
    id: Uuid,
) -> Result<Option<QueueRow>, MetaError> {
    let mut conn = pool.acquire().await?;
    get_on(&mut conn, project, id).await
}

/// [`get`] on the writer, inside a transaction.
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails or a stored value does not decode.
pub(crate) async fn get_on(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
) -> Result<Option<QueueRow>, MetaError> {
    let sql =
        format!("SELECT {QUEUE_COLUMNS} FROM annotation_queues WHERE project_id = ? AND id = ?");
    let record: Option<QueueRecord> = sqlx::query_as(AssertSqlSafe(sql))
        .bind(project.to_string())
        .bind(id.to_string())
        .fetch_optional(&mut *conn)
        .await?;
    match record {
        Some(record) => {
            let definitions = definition_ids_on(&mut *conn, id).await?;
            queue_from(record, definitions).map(Some)
        }
        None => Ok(None),
    }
}

/// Queues of `project`, ordered by name then id. `after` is the `(name, id)` of the previous
/// page's last queue. `limit` is checked by the route. `holding` keeps only the queues that hold
/// a visible item with that trace or session id. `visible_from` hides items whose `target_time`
/// is earlier, in that filter only.
///
/// Returns the page and the `(name, id)` of its last queue when more follow.
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails or a stored value does not decode.
pub(crate) async fn list(
    pool: &SqlitePool,
    project: ProjectId,
    after: Option<(&str, Uuid)>,
    limit: usize,
    holding: Option<Holding<'_>>,
    visible_from: Option<i64>,
) -> Result<(Vec<QueueRow>, Option<(String, Uuid)>), MetaError> {
    let fetch = limit
        .checked_add(1)
        .and_then(|n| i64::try_from(n).ok())
        .ok_or(MetaError::Conflict("page size is too large"))?;
    let mut builder = QueryBuilder::<Sqlite>::new(format!(
        "SELECT {QUEUE_COLUMNS} FROM annotation_queues WHERE project_id = "
    ));
    builder.push_bind(project.to_string());
    if let Some((name, id)) = after {
        builder
            .push(" AND (name > ")
            .push_bind(name.to_owned())
            .push(" OR (name = ")
            .push_bind(name.to_owned())
            .push(" AND id > ")
            .push_bind(id.to_string())
            .push("))");
    }
    match holding {
        Some(Holding::Trace(trace)) => {
            builder
                .push(" AND id IN (SELECT queue_id FROM annotation_queue_items WHERE project_id = ")
                .push_bind(project.to_string())
                .push(" AND trace_id = ")
                .push_bind(trace.to_owned());
            push_visible(&mut builder, visible_from);
            builder.push(")");
        }
        Some(Holding::Session(session)) => {
            builder
                .push(" AND id IN (SELECT queue_id FROM annotation_queue_items WHERE project_id = ")
                .push_bind(project.to_string())
                .push(" AND session_id = ")
                .push_bind(session.to_owned());
            push_visible(&mut builder, visible_from);
            builder.push(")");
        }
        None => {}
    }
    builder.push(" ORDER BY name, id LIMIT ").push_bind(fetch);
    let records: Vec<QueueRecord> = builder.build_query_as().fetch_all(pool).await?;
    let mut definitions = definitions_by_queue(pool, project).await?;

    let mut rows = Vec::with_capacity(records.len());
    for record in records {
        let id = uuid_of(&record.id, QUEUES, "id")?;
        let queue_definitions = definitions.remove(&id).unwrap_or_default();
        rows.push(queue_from(record, queue_definitions)?);
    }
    let next = if rows.len() > limit {
        rows.truncate(limit);
        rows.last().map(|row| (row.name.clone(), row.id))
    } else {
        None
    };
    Ok((rows, next))
}

/// Item counts of each queue in `queues` in `project`. A queue with no items is absent from the
/// map; callers use [`Counts::default`]. `visible_from` hides items as in [`list_items`].
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn counts(
    pool: &SqlitePool,
    project: ProjectId,
    queues: &[Uuid],
    visible_from: Option<i64>,
) -> Result<HashMap<Uuid, Counts>, MetaError> {
    if queues.is_empty() {
        return Ok(HashMap::new());
    }
    let mut builder = QueryBuilder::<Sqlite>::new(
        "SELECT queue_id, status, COUNT(*) FROM annotation_queue_items WHERE project_id = ",
    );
    builder
        .push_bind(project.to_string())
        .push(" AND queue_id IN (");
    push_uuids(&mut builder, queues);
    push_visible(&mut builder, visible_from);
    builder.push(" GROUP BY queue_id, status");
    let records: Vec<(String, String, i64)> = builder.build_query_as().fetch_all(pool).await?;

    let mut by_queue: HashMap<Uuid, Counts> = HashMap::new();
    for (queue, status, count) in records {
        let queue = uuid_of(&queue, ITEMS, "queue_id")?;
        let status = Status::parse(&status).ok_or_else(|| corrupt(ITEMS, "status"))?;
        let count = u64::try_from(count).unwrap_or(0);
        let entry = by_queue.entry(queue).or_default();
        match status {
            Status::Pending => entry.pending += count,
            Status::Completed => entry.completed += count,
            Status::Skipped => entry.skipped += count,
        }
    }
    Ok(by_queue)
}

/// Applies `changes` to the queue `id` of `project` when its stored version is `version`. The
/// compare-and-set is one `UPDATE`: the version rises by one, and `updated_at` is set. A new
/// `definition_ids` list replaces the old rows. Checked as [`insert_queue_tx`] checks them.
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails, or a stored value does not decode.
pub(crate) async fn update_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
    version: i64,
    changes: QueueChanges,
    now: i64,
) -> Result<QueueUpdate, MetaError> {
    if let Some(definition_ids) = &changes.definition_ids {
        let missing = live_definitions_tx(&mut *conn, project, definition_ids).await?;
        if !missing.is_empty() {
            return Ok(QueueUpdate::UnknownDefinitions(missing));
        }
    }
    let result = sqlx::query(
        "UPDATE annotation_queues SET \
         name = COALESCE(?, name), \
         description = CASE WHEN ? THEN ? ELSE description END, \
         instructions = CASE WHEN ? THEN ? ELSE instructions END, \
         version = version + 1, updated_at = ? \
         WHERE project_id = ? AND id = ? AND version = ?",
    )
    .bind(changes.name.as_deref())
    .bind(changes.description.is_some())
    .bind(changes.description.clone().flatten())
    .bind(changes.instructions.is_some())
    .bind(changes.instructions.clone().flatten())
    .bind(now)
    .bind(project.to_string())
    .bind(id.to_string())
    .bind(version)
    .execute(&mut *conn)
    .await
    .map_err(|error| unique_or(error, "an annotation queue with this name exists"))?;
    if result.rows_affected() == 1 {
        if let Some(definition_ids) = &changes.definition_ids {
            sqlx::query("DELETE FROM annotation_queue_definitions WHERE queue_id = ?")
                .bind(id.to_string())
                .execute(&mut *conn)
                .await?;
            insert_definition_rows(&mut *conn, id, definition_ids).await?;
        }
        let row = get_on(&mut *conn, project, id).await?;
        return Ok(row.map_or(QueueUpdate::Missing, QueueUpdate::Updated));
    }
    let stored: Option<i64> =
        sqlx::query_scalar("SELECT version FROM annotation_queues WHERE project_id = ? AND id = ?")
            .bind(project.to_string())
            .bind(id.to_string())
            .fetch_optional(&mut *conn)
            .await?;
    Ok(stored.map_or(QueueUpdate::Missing, QueueUpdate::Stale))
}

/// Deletes the queue `id` of `project`. Its definition rows and items go by cascade. Returns
/// whether a row was deleted.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the delete fails.
pub(crate) async fn delete(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
) -> Result<bool, MetaError> {
    let result = sqlx::query("DELETE FROM annotation_queues WHERE project_id = ? AND id = ?")
        .bind(project.to_string())
        .bind(id.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// The ids among `ids` that are queues of `project`.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn existing_ids(
    pool: &SqlitePool,
    project: ProjectId,
    ids: &[Uuid],
) -> Result<HashSet<Uuid>, MetaError> {
    if ids.is_empty() {
        return Ok(HashSet::new());
    }
    let mut builder =
        QueryBuilder::<Sqlite>::new("SELECT id FROM annotation_queues WHERE project_id = ");
    builder.push_bind(project.to_string()).push(" AND id IN (");
    push_uuids(&mut builder, ids);
    let records: Vec<(String,)> = builder.build_query_as().fetch_all(pool).await?;
    records
        .into_iter()
        .map(|(text,)| uuid_of(&text, QUEUES, "id"))
        .collect()
}

/// Adds `items` to `queue`. A target already in the queue is not added again (the unique index
/// on the target). The item cap is [`MAX_ITEMS_PER_QUEUE`]; see [`insert_items_tx_capped`].
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails.
pub(crate) async fn insert_items_tx(
    conn: &mut SqliteConnection,
    queue: Uuid,
    project: ProjectId,
    items: &[NewItem],
    added_by: (AddedByKind, Uuid),
    now: i64,
) -> Result<ItemsInsert, MetaError> {
    insert_items_tx_capped(
        conn,
        queue,
        project,
        items,
        added_by,
        now,
        MAX_ITEMS_PER_QUEUE,
    )
    .await
}

/// [`insert_items_tx`] with the item cap as an argument. The rows are inserted inside a
/// `SAVEPOINT`. Above `cap` items in the queue, the savepoint is rolled back and nothing is
/// added. Targets that already existed never count against the cap.
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails. The savepoint is rolled back first.
pub(crate) async fn insert_items_tx_capped(
    conn: &mut SqliteConnection,
    queue: Uuid,
    project: ProjectId,
    items: &[NewItem],
    added_by: (AddedByKind, Uuid),
    now: i64,
    cap: u64,
) -> Result<ItemsInsert, MetaError> {
    sqlx::query("SAVEPOINT add_items")
        .execute(&mut *conn)
        .await?;
    match add_rows_and_count(&mut *conn, queue, project, items, added_by, now).await {
        Ok((_, total)) if total > cap => {
            release_add_items(&mut *conn, true).await?;
            Ok(ItemsInsert::TooMany)
        }
        Ok((added, _)) => {
            release_add_items(&mut *conn, false).await?;
            Ok(ItemsInsert::Done {
                added,
                existing: (items.len() as u64).saturating_sub(added),
            })
        }
        Err(error) => {
            // The caller's transaction rolls back on this error. Undoing the savepoint here
            // keeps the connection clean if the caller goes on.
            let _ = release_add_items(&mut *conn, true).await;
            Err(error)
        }
    }
}

/// Inserts each item and returns `(rows added, items in the queue after the inserts)`.
async fn add_rows_and_count(
    conn: &mut SqliteConnection,
    queue: Uuid,
    project: ProjectId,
    items: &[NewItem],
    added_by: (AddedByKind, Uuid),
    now: i64,
) -> Result<(u64, u64), MetaError> {
    let mut added = 0;
    for item in items {
        let result = sqlx::query(
            "INSERT INTO annotation_queue_items \
             (id, queue_id, project_id, trace_id, span_id, session_id, target_time, status, \
              added_by_kind, added_by_id, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?, ?) ON CONFLICT DO NOTHING",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(queue.to_string())
        .bind(project.to_string())
        .bind(item.trace_id.as_deref())
        .bind(item.span_id.as_deref())
        .bind(item.session_id.as_deref())
        .bind(item.target_time)
        .bind(added_by.0.as_str())
        .bind(added_by.1.to_string())
        .bind(now)
        .bind(now)
        .execute(&mut *conn)
        .await?;
        added += result.rows_affected();
    }
    let total: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM annotation_queue_items WHERE queue_id = ?")
            .bind(queue.to_string())
            .fetch_one(&mut *conn)
            .await?;
    Ok((added, u64::try_from(total).unwrap_or(u64::MAX)))
}

/// Ends the `add_items` savepoint: rolled back to and released when `rollback`, else released.
async fn release_add_items(conn: &mut SqliteConnection, rollback: bool) -> Result<(), MetaError> {
    if rollback {
        sqlx::query("ROLLBACK TO add_items")
            .execute(&mut *conn)
            .await?;
    }
    sqlx::query("RELEASE add_items").execute(&mut *conn).await?;
    Ok(())
}

/// Items of `queue`, ordered by id. `after` is the previous page's last id. `status` keeps one
/// state. `visible_from` hides items whose `target_time` is earlier. Returns the page and the
/// id of its last item when more follow.
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails or a stored value does not decode.
pub(crate) async fn list_items(
    pool: &SqlitePool,
    queue: Uuid,
    status: Option<Status>,
    after: Option<Uuid>,
    limit: usize,
    visible_from: Option<i64>,
) -> Result<(Vec<ItemRow>, Option<Uuid>), MetaError> {
    let fetch = limit
        .checked_add(1)
        .and_then(|n| i64::try_from(n).ok())
        .ok_or(MetaError::Conflict("page size is too large"))?;
    let mut builder = QueryBuilder::<Sqlite>::new(format!(
        "SELECT {ITEM_COLUMNS} FROM annotation_queue_items WHERE queue_id = "
    ));
    builder.push_bind(queue.to_string());
    if let Some(status) = status {
        builder.push(" AND status = ").push_bind(status.as_str());
    }
    if let Some(after) = after {
        builder.push(" AND id > ").push_bind(after.to_string());
    }
    push_visible(&mut builder, visible_from);
    builder.push(" ORDER BY id LIMIT ").push_bind(fetch);
    let records: Vec<ItemRecord> = builder.build_query_as().fetch_all(pool).await?;

    let mut rows = records
        .into_iter()
        .map(item_from)
        .collect::<Result<Vec<_>, _>>()?;
    let next = if rows.len() > limit {
        rows.truncate(limit);
        rows.last().map(|row| row.id)
    } else {
        None
    };
    Ok((rows, next))
}

/// The item `item` of `queue`, on the writer. `visible_from` as in [`list_items`].
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn get_item_on(
    conn: &mut SqliteConnection,
    queue: Uuid,
    item: Uuid,
    visible_from: Option<i64>,
) -> Result<Option<ItemRow>, MetaError> {
    let mut builder = QueryBuilder::<Sqlite>::new(format!(
        "SELECT {ITEM_COLUMNS} FROM annotation_queue_items WHERE queue_id = "
    ));
    builder
        .push_bind(queue.to_string())
        .push(" AND id = ")
        .push_bind(item.to_string());
    push_visible(&mut builder, visible_from);
    let record: Option<ItemRecord> = builder.build_query_as().fetch_optional(&mut *conn).await?;
    record.map(item_from).transpose()
}

/// Deletes the item `item` of `queue`. Returns whether a row was deleted.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the delete fails.
pub(crate) async fn delete_item(
    conn: &mut SqliteConnection,
    queue: Uuid,
    item: Uuid,
) -> Result<bool, MetaError> {
    let result = sqlx::query("DELETE FROM annotation_queue_items WHERE queue_id = ? AND id = ?")
        .bind(queue.to_string())
        .bind(item.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::score_definitions::ScoreDirection;
    use crate::meta::MetaDb;
    use crate::meta::orgs::insert_org;
    use crate::meta::projects::insert_project;
    use crate::meta::score_definitions::{DefinitionFields, insert_definition};
    use cetra_core::enums::ScoreDataType;

    const NOW: i64 = 1_000;
    const TRACE: &str = "0af7651916cd43dd8448eb211c80319c";
    const OTHER_TRACE: &str = "1bf7651916cd43dd8448eb211c80319d";
    const SPAN: &str = "b7ad6b7169203331";

    /// An org and a project `support`.
    async fn seed(db: &MetaDb) -> (OrgId, ProjectId) {
        db.write(|conn| {
            Box::pin(async move {
                let org = insert_org(conn, OrgId::new_v7(), "Acme", "acme", NOW).await?;
                let project = insert_project(conn, org.id, "Support", "support", None, NOW)
                    .await?
                    .id;
                Ok((org.id, project))
            })
        })
        .await
        .unwrap()
    }

    /// A second project in `org`, named `name`.
    async fn project_in(db: &MetaDb, org: OrgId, name: &'static str) -> ProjectId {
        db.write(move |conn| {
            Box::pin(async move {
                insert_project(conn, org, name, name, None, NOW)
                    .await
                    .map(|row| row.id)
            })
        })
        .await
        .unwrap()
    }

    /// A live (or archived) numeric definition of `project`.
    async fn definition(
        db: &MetaDb,
        project: ProjectId,
        name: &str,
        archived_at: Option<i64>,
    ) -> Uuid {
        let name = name.to_owned();
        db.write(move |conn| {
            Box::pin(async move {
                insert_definition(
                    conn,
                    Uuid::now_v7(),
                    project,
                    ScoreDataType::Numeric,
                    DefinitionFields {
                        name,
                        categories: Vec::new(),
                        min_value: Some(0.0),
                        max_value: Some(1.0),
                        description: None,
                        direction: ScoreDirection::None,
                        archived_at,
                    },
                    NOW,
                )
                .await
                .map(|row| row.id)
            })
        })
        .await
        .unwrap()
    }

    fn new_queue(name: &str, target: Target, definition_ids: Vec<Uuid>) -> NewQueue {
        NewQueue {
            name: name.to_owned(),
            description: None,
            instructions: None,
            target,
            definition_ids,
            created_by: None,
        }
    }

    async fn create(
        db: &MetaDb,
        org: OrgId,
        project: ProjectId,
        queue: NewQueue,
        plan_limit: Option<u64>,
    ) -> QueueCreate {
        db.write(move |conn| {
            Box::pin(
                async move { insert_queue_tx(conn, project, org, queue, plan_limit, NOW).await },
            )
        })
        .await
        .unwrap()
    }

    async fn created(db: &MetaDb, org: OrgId, project: ProjectId, queue: NewQueue) -> QueueRow {
        match create(db, org, project, queue, None).await {
            QueueCreate::Created(row) => row,
            other => panic!("expected a created queue, got {other:?}"),
        }
    }

    async fn update(
        db: &MetaDb,
        project: ProjectId,
        id: Uuid,
        version: i64,
        changes: QueueChanges,
    ) -> QueueUpdate {
        db.write(move |conn| {
            Box::pin(async move { update_tx(conn, project, id, version, changes, NOW + 1).await })
        })
        .await
        .unwrap()
    }

    async fn add(db: &MetaDb, queue: Uuid, project: ProjectId, items: Vec<NewItem>) -> ItemsInsert {
        db.write(move |conn| {
            Box::pin(async move {
                insert_items_tx(
                    conn,
                    queue,
                    project,
                    &items,
                    (AddedByKind::User, Uuid::now_v7()),
                    NOW,
                )
                .await
            })
        })
        .await
        .unwrap()
    }

    fn trace_item(trace: &str, target_time: i64) -> NewItem {
        NewItem {
            trace_id: Some(trace.to_owned()),
            span_id: None,
            session_id: None,
            target_time,
        }
    }

    fn span_item(trace: &str, span: &str) -> NewItem {
        NewItem {
            trace_id: Some(trace.to_owned()),
            span_id: Some(span.to_owned()),
            session_id: None,
            target_time: NOW,
        }
    }

    async fn count_rows(db: &MetaDb, table: &str) -> i64 {
        sqlx::query_scalar(AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
            .fetch_one(db.reader())
            .await
            .unwrap()
    }

    #[test]
    fn constants_match_the_decisions() {
        assert_eq!(MAX_QUEUES_PER_PROJECT, 100);
        assert_eq!(MAX_ITEMS_PER_QUEUE, 100_000);
        assert_eq!(MAX_DEFINITIONS, 50);
        assert_eq!(LEASE_NS, 5 * 60 * 1_000_000_000);
    }

    #[test]
    fn enum_text_round_trips() {
        for target in [Target::Trace, Target::Span, Target::Session] {
            assert_eq!(Target::parse(target.as_str()), Some(target));
        }
        for status in [Status::Pending, Status::Completed, Status::Skipped] {
            assert_eq!(Status::parse(status.as_str()), Some(status));
        }
        for kind in [AddedByKind::User, AddedByKind::Pat, AddedByKind::Key] {
            assert_eq!(AddedByKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(Target::parse("Trace"), None);
        assert_eq!(Status::parse("done"), None);
    }

    #[tokio::test]
    async fn create_get_list_and_delete() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, project) = seed(&db).await;
        let tone = definition(&db, project, "tone", None).await;
        let accuracy = definition(&db, project, "accuracy", None).await;
        let b = created(
            &db,
            org,
            project,
            new_queue("b-queue", Target::Trace, vec![tone, accuracy]),
        )
        .await;
        let a = created(
            &db,
            org,
            project,
            new_queue("a-queue", Target::Span, vec![accuracy]),
        )
        .await;
        assert_eq!(b.definition_ids, vec![tone, accuracy]);
        assert_eq!(b.version, 1);
        assert_eq!(b.project_id, project);
        assert_eq!(b.target, Target::Trace);

        assert_eq!(
            get(db.reader(), project, b.id).await.unwrap(),
            Some(b.clone())
        );
        assert_eq!(
            get(db.reader(), project, Uuid::now_v7()).await.unwrap(),
            None
        );

        let (rows, next) = list(db.reader(), project, None, 1, None, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "a-queue");
        let (name, id) = next.unwrap();
        assert_eq!((name.as_str(), id), ("a-queue", a.id));
        let (rows, next) = list(db.reader(), project, Some((&name, id)), 10, None, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "b-queue");
        assert!(next.is_none());

        add(&db, b.id, project, vec![trace_item(TRACE, NOW)]).await;
        let deleted = db
            .write(move |conn| Box::pin(async move { delete(conn, project, b.id).await }))
            .await
            .unwrap();
        assert!(deleted);
        assert_eq!(get(db.reader(), project, b.id).await.unwrap(), None);
        assert_eq!(count_rows(&db, "annotation_queue_items").await, 0);
        // The other queue keeps its one definition row.
        assert_eq!(count_rows(&db, "annotation_queue_definitions").await, 1);
        assert_eq!(count_rows(&db, "annotation_queues").await, 1);
    }

    #[tokio::test]
    async fn name_is_unique_per_project() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, project) = seed(&db).await;
        created(
            &db,
            org,
            project,
            new_queue("review", Target::Trace, vec![]),
        )
        .await;
        let second = db
            .write(move |conn| {
                Box::pin(async move {
                    insert_queue_tx(
                        conn,
                        project,
                        org,
                        new_queue("review", Target::Trace, vec![]),
                        None,
                        NOW,
                    )
                    .await
                })
            })
            .await;
        assert!(matches!(
            second,
            Err(MetaError::Conflict(
                "an annotation queue with this name exists"
            ))
        ));
    }

    #[tokio::test]
    async fn project_cap_is_100() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, project) = seed(&db).await;
        for i in 0..MAX_QUEUES_PER_PROJECT {
            created(
                &db,
                org,
                project,
                new_queue(&format!("q-{i:03}"), Target::Trace, vec![]),
            )
            .await;
        }
        let over = db
            .write(move |conn| {
                Box::pin(async move {
                    insert_queue_tx(
                        conn,
                        project,
                        org,
                        new_queue("one-too-many", Target::Trace, vec![]),
                        None,
                        NOW,
                    )
                    .await
                })
            })
            .await;
        assert!(matches!(
            over,
            Err(MetaError::Conflict("too many annotation queues"))
        ));
    }

    #[tokio::test]
    async fn plan_limit_counts_the_org() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, project) = seed(&db).await;
        let other = project_in(&db, org, "other").await;
        assert!(matches!(
            create(
                &db,
                org,
                project,
                new_queue("first", Target::Trace, vec![]),
                Some(1)
            )
            .await,
            QueueCreate::Created(_)
        ));
        assert_eq!(
            create(
                &db,
                org,
                other,
                new_queue("second", Target::Trace, vec![]),
                Some(1)
            )
            .await,
            QueueCreate::OverPlanLimit(1)
        );
        assert!(matches!(
            create(
                &db,
                org,
                other,
                new_queue("second", Target::Trace, vec![]),
                None
            )
            .await,
            QueueCreate::Created(_)
        ));
    }

    #[tokio::test]
    async fn unknown_or_archived_definitions_are_reported() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, project) = seed(&db).await;
        let other = project_in(&db, org, "other").await;
        let live = definition(&db, project, "live", None).await;
        let archived = definition(&db, project, "old", Some(NOW)).await;
        let foreign = definition(&db, other, "foreign", None).await;
        assert_eq!(
            create(
                &db,
                org,
                project,
                new_queue("q", Target::Trace, vec![live, archived, foreign]),
                None,
            )
            .await,
            QueueCreate::UnknownDefinitions(vec![1, 2])
        );
    }

    #[tokio::test]
    async fn update_checks_the_version() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, project) = seed(&db).await;
        let d1 = definition(&db, project, "tone", None).await;
        let d2 = definition(&db, project, "accuracy", None).await;
        let queue = created(
            &db,
            org,
            project,
            new_queue("review", Target::Trace, vec![d1, d2]),
        )
        .await;

        let changes = QueueChanges {
            description: Some(Some("check tone".to_owned())),
            definition_ids: Some(vec![d2, d1]),
            ..QueueChanges::default()
        };
        let QueueUpdate::Updated(row) = update(&db, project, queue.id, 1, changes.clone()).await
        else {
            panic!("expected the update to apply");
        };
        assert_eq!(row.version, 2);
        assert_eq!(row.definition_ids, vec![d2, d1]);
        assert_eq!(row.description.as_deref(), Some("check tone"));
        assert_eq!(row.updated_at, NOW + 1);

        assert_eq!(
            update(&db, project, queue.id, 1, changes).await,
            QueueUpdate::Stale(2)
        );
        assert_eq!(
            update(&db, project, Uuid::now_v7(), 1, QueueChanges::default()).await,
            QueueUpdate::Missing
        );

        let cleared = QueueChanges {
            description: Some(None),
            ..QueueChanges::default()
        };
        let QueueUpdate::Updated(row) = update(&db, project, queue.id, 2, cleared).await else {
            panic!("expected the clear to apply");
        };
        assert_eq!(row.version, 3);
        assert_eq!(row.description, None);
        assert_eq!(row.definition_ids, vec![d2, d1]);
    }

    #[tokio::test]
    async fn items_are_unique_per_target() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, project) = seed(&db).await;
        let q = created(
            &db,
            org,
            project,
            new_queue("traces", Target::Trace, vec![]),
        )
        .await;
        let q2 = created(
            &db,
            org,
            project,
            new_queue("traces-2", Target::Trace, vec![]),
        )
        .await;

        assert_eq!(
            add(
                &db,
                q.id,
                project,
                vec![trace_item(TRACE, NOW), trace_item(TRACE, NOW)],
            )
            .await,
            ItemsInsert::Done {
                added: 1,
                existing: 1,
            }
        );
        assert_eq!(
            add(&db, q.id, project, vec![trace_item(TRACE, NOW)]).await,
            ItemsInsert::Done {
                added: 0,
                existing: 1,
            }
        );
        // A span item of the same trace is a different target.
        assert_eq!(
            add(&db, q.id, project, vec![span_item(TRACE, SPAN)]).await,
            ItemsInsert::Done {
                added: 1,
                existing: 0,
            }
        );
        assert_eq!(
            add(&db, q.id, project, vec![span_item(TRACE, SPAN)]).await,
            ItemsInsert::Done {
                added: 0,
                existing: 1,
            }
        );
        // The same trace can be in another queue.
        assert_eq!(
            add(&db, q2.id, project, vec![trace_item(TRACE, NOW)]).await,
            ItemsInsert::Done {
                added: 1,
                existing: 0,
            }
        );
    }

    #[tokio::test]
    async fn item_cap_rolls_back() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, project) = seed(&db).await;
        let q = created(
            &db,
            org,
            project,
            new_queue("traces", Target::Trace, vec![]),
        )
        .await;
        let queue = q.id;
        let first = db
            .write(move |conn| {
                Box::pin(async move {
                    insert_items_tx_capped(
                        conn,
                        queue,
                        project,
                        &[trace_item(TRACE, NOW), trace_item(OTHER_TRACE, NOW)],
                        (AddedByKind::Key, Uuid::now_v7()),
                        NOW,
                        2,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        assert_eq!(
            first,
            ItemsInsert::Done {
                added: 2,
                existing: 0,
            }
        );

        // A new target past the cap adds nothing, and the savepoint leaves no trace.
        let over = db
            .write(move |conn| {
                Box::pin(async move {
                    insert_items_tx_capped(
                        conn,
                        queue,
                        project,
                        &[trace_item(TRACE, NOW), span_item(OTHER_TRACE, SPAN)],
                        (AddedByKind::Key, Uuid::now_v7()),
                        NOW,
                        2,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        assert_eq!(over, ItemsInsert::TooMany);
        assert_eq!(count_rows(&db, "annotation_queue_items").await, 2);
        assert_eq!(
            add(&db, queue, project, vec![trace_item(TRACE, NOW)]).await,
            ItemsInsert::Done {
                added: 0,
                existing: 1,
            }
        );
    }

    #[tokio::test]
    async fn items_are_listed_counted_and_filtered() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, project) = seed(&db).await;
        let q = created(
            &db,
            org,
            project,
            new_queue("traces", Target::Trace, vec![]),
        )
        .await;
        add(
            &db,
            q.id,
            project,
            vec![
                trace_item(TRACE, 10),
                trace_item(OTHER_TRACE, 20),
                NewItem {
                    trace_id: None,
                    span_id: None,
                    session_id: Some("s-1".to_owned()),
                    target_time: 30,
                },
            ],
        )
        .await;
        let counted = counts(db.reader(), project, &[q.id], None).await.unwrap();
        assert_eq!(
            counted.get(&q.id),
            Some(&Counts {
                pending: 3,
                completed: 0,
                skipped: 0,
            })
        );
        assert!(
            counts(db.reader(), project, &[], None)
                .await
                .unwrap()
                .is_empty()
        );

        let (page, next) = list_items(db.reader(), q.id, None, None, 2, None)
            .await
            .unwrap();
        assert_eq!(page.len(), 2);
        let first = &page[0];
        assert_eq!(first.trace_id.as_deref(), Some(TRACE));
        assert_eq!(first.status, Status::Pending);
        assert_eq!(first.skip_reason, None);
        assert_eq!(first.leased_by_user_id, None);
        assert_eq!(first.lease_expires_at, None);
        assert_eq!(first.done_by_user_id, None);
        assert_eq!(first.done_at, None);
        assert_eq!(first.added_by_kind, AddedByKind::User);
        assert_eq!(first.project_id, project);
        let next = next.unwrap();
        assert_eq!(next, page[1].id);
        let (rest, after_rest) = list_items(db.reader(), q.id, None, Some(next), 2, None)
            .await
            .unwrap();
        assert_eq!(rest.len(), 1);
        assert!(after_rest.is_none());

        // The data window hides items started before it.
        let (windowed, _) = list_items(db.reader(), q.id, None, None, 10, Some(15))
            .await
            .unwrap();
        assert_eq!(windowed.len(), 2);
        let item = page[0].id;
        let visible = db
            .write(move |conn| Box::pin(async move { get_item_on(conn, q.id, item, None).await }))
            .await
            .unwrap();
        assert!(visible.is_some());
        let hidden = db
            .write(move |conn| {
                Box::pin(async move { get_item_on(conn, q.id, item, Some(15)).await })
            })
            .await
            .unwrap();
        assert!(hidden.is_none());

        let (skipped, _) = list_items(db.reader(), q.id, Some(Status::Skipped), None, 10, None)
            .await
            .unwrap();
        assert!(skipped.is_empty());

        let deleted = db
            .write(move |conn| Box::pin(async move { delete_item(conn, q.id, item).await }))
            .await
            .unwrap();
        assert!(deleted);
        assert_eq!(count_rows(&db, "annotation_queue_items").await, 2);
    }

    #[tokio::test]
    async fn list_holding_keeps_queues_with_a_visible_item() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, project) = seed(&db).await;
        let traces = created(
            &db,
            org,
            project,
            new_queue("traces", Target::Trace, vec![]),
        )
        .await;
        let sessions = created(
            &db,
            org,
            project,
            new_queue("sessions", Target::Session, vec![]),
        )
        .await;
        let empty = created(&db, org, project, new_queue("empty", Target::Trace, vec![])).await;
        add(&db, traces.id, project, vec![trace_item(TRACE, 100)]).await;
        add(
            &db,
            sessions.id,
            project,
            vec![NewItem {
                trace_id: None,
                span_id: None,
                session_id: Some("s-1".to_owned()),
                target_time: 100,
            }],
        )
        .await;

        let (by_trace, _) = list(
            db.reader(),
            project,
            None,
            10,
            Some(Holding::Trace(TRACE)),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            by_trace.iter().map(|q| q.id).collect::<Vec<_>>(),
            vec![traces.id]
        );
        let (hidden, _) = list(
            db.reader(),
            project,
            None,
            10,
            Some(Holding::Trace(TRACE)),
            Some(200),
        )
        .await
        .unwrap();
        assert!(hidden.is_empty());
        let (by_session, _) = list(
            db.reader(),
            project,
            None,
            10,
            Some(Holding::Session("s-1")),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            by_session.iter().map(|q| q.id).collect::<Vec<_>>(),
            vec![sessions.id]
        );
        let (all, _) = list(db.reader(), project, None, 10, None, None)
            .await
            .unwrap();
        assert_eq!(all.len(), 3);
        assert!(all.iter().any(|q| q.id == empty.id));
    }

    #[tokio::test]
    async fn existing_ids_ignores_other_projects() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, project) = seed(&db).await;
        let other = project_in(&db, org, "other").await;
        let mine = created(&db, org, project, new_queue("mine", Target::Trace, vec![])).await;
        let theirs = created(&db, org, other, new_queue("theirs", Target::Trace, vec![])).await;
        let found = existing_ids(db.reader(), project, &[mine.id, theirs.id, Uuid::now_v7()])
            .await
            .unwrap();
        assert_eq!(found, HashSet::from([mine.id]));
        assert!(
            existing_ids(db.reader(), project, &[])
                .await
                .unwrap()
                .is_empty()
        );
    }
}
