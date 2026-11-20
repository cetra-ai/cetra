//! `issue_sources`, `issues` and `issue_events` rows (spec 04 § Issues, D-IC-1, D-IC-3,
//! D-IC-10). `kind`, `state` and the event kind are stored as text; a stored value outside
//! its list is corrupt.
#![cfg_attr(not(test), allow(dead_code))] // M1.6 onward read and write these rows

use std::collections::HashMap;

use cetra_core::ids::{OrgId, ProjectId};
use sqlx::{QueryBuilder, Sqlite, SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::{MetaError, unique_or};

const SOURCES: &str = "issue_sources";
const ISSUES: &str = "issues";
const EVENTS: &str = "issue_events";

/// Shown when a source name is taken in its project.
const SOURCE_CONFLICT: &str = "an issue source with this name already exists";
/// Shown when an error source already has an issue with the same fingerprint.
const FINGERPRINT_CONFLICT: &str = "an issue with this fingerprint already exists";

/// The kind of an issue source. M3 adds kinds; the Rust enum is the only gate (D-IC-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKind {
    /// Error spans of completed traces, fingerprinted (D-IC-2).
    Error,
}

impl SourceKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
        }
    }

    fn from_db(value: &str) -> Option<Self> {
        match value {
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

/// The state of an issue (D-IC-10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IssueState {
    Open,
    Resolved,
    Ignored,
    /// Folded into `merged_into`; its occurrences count on the target.
    Merged,
}

impl IssueState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Resolved => "resolved",
            Self::Ignored => "ignored",
            Self::Merged => "merged",
        }
    }

    fn from_db(value: &str) -> Option<Self> {
        match value {
            "open" => Some(Self::Open),
            "resolved" => Some(Self::Resolved),
            "ignored" => Some(Self::Ignored),
            "merged" => Some(Self::Merged),
            _ => None,
        }
    }
}

/// The kind of an issue event (D-IC-10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventKind {
    Created,
    StateChanged,
    Renamed,
    Merged,
    Regressed,
}

impl EventKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::StateChanged => "state_changed",
            Self::Renamed => "renamed",
            Self::Merged => "merged",
            Self::Regressed => "regressed",
        }
    }

    fn from_db(value: &str) -> Option<Self> {
        match value {
            "created" => Some(Self::Created),
            "state_changed" => Some(Self::StateChanged),
            "renamed" => Some(Self::Renamed),
            "merged" => Some(Self::Merged),
            "regressed" => Some(Self::Regressed),
            _ => None,
        }
    }
}

/// One stored issue source.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SourceRow {
    pub(crate) id: Uuid,
    pub(crate) project_id: ProjectId,
    /// Unique in the project.
    pub(crate) name: String,
    pub(crate) kind: SourceKind,
    /// Filter DSL expression over spans, when set.
    pub(crate) filter: Option<String>,
    pub(crate) enabled: bool,
    /// Why the sweeper skips the source, when it does.
    pub(crate) paused_reason: Option<String>,
    /// When the pause started, nanoseconds.
    pub(crate) paused_at: Option<i64>,
    pub(crate) created_by: Option<Uuid>,
    /// Nanoseconds.
    pub(crate) created_at: i64,
    /// Nanoseconds.
    pub(crate) updated_at: i64,
}

/// One stored issue. `first_seen` and `last_seen` are trace start times, nanoseconds.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IssueRow {
    pub(crate) id: Uuid,
    pub(crate) project_id: ProjectId,
    pub(crate) source_id: Uuid,
    /// Hex fingerprint of the error group. `None` for issues without one.
    pub(crate) fingerprint: Option<String>,
    pub(crate) title: String,
    pub(crate) description: Option<String>,
    pub(crate) state: IssueState,
    /// Set when a resolved issue recurs (D-IC-10).
    pub(crate) regressed: bool,
    /// The issue this one was merged into.
    pub(crate) merged_into: Option<Uuid>,
    pub(crate) first_seen: i64,
    pub(crate) last_seen: i64,
    pub(crate) resolved_at: Option<i64>,
    pub(crate) resolved_by: Option<Uuid>,
    /// Nanoseconds.
    pub(crate) created_at: i64,
    /// Nanoseconds.
    pub(crate) updated_at: i64,
}

/// One stored issue event. `actor_user_id` `None` means the system.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EventRow {
    pub(crate) id: Uuid,
    pub(crate) issue_id: Uuid,
    pub(crate) kind: EventKind,
    pub(crate) actor_user_id: Option<Uuid>,
    pub(crate) from_value: Option<String>,
    pub(crate) to_value: Option<String>,
    /// Nanoseconds.
    pub(crate) at: i64,
}

/// An enabled error source of an active project in an active org, with what the sweeper needs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SweepSource {
    pub(crate) id: Uuid,
    pub(crate) project_id: ProjectId,
    /// The project's org (D-P-44).
    pub(crate) org_id: OrgId,
    pub(crate) filter: Option<String>,
    /// The project's `settings.trace_completion_timeout_s`, as stored (not range-checked here).
    pub(crate) completion_timeout_s: Option<i64>,
}

/// Which issues [`list_issues`] returns, and in what order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IssueSort {
    /// `last_seen` descending, then id descending.
    LastSeen,
    /// `first_seen` descending, then id descending.
    FirstSeen,
}

/// The filters of an issue list. Empty `states` and `None` fields leave a filter out.
pub(crate) struct IssueListQuery<'a> {
    /// Keep issues in these states. Empty keeps every state.
    pub(crate) states: &'a [IssueState],
    /// Keep the issues of this source.
    pub(crate) source: Option<Uuid>,
    /// Keep issues whose title contains this text, case-insensitively, with `%`, `_` and `\`
    /// matching themselves.
    pub(crate) q: Option<&'a str>,
    /// When set, only these ids. An empty slice matches nothing. The caller bounds it (at most 100).
    pub(crate) ids: Option<&'a [Uuid]>,
    pub(crate) sort: IssueSort,
    /// The sort value and id of the last row of the previous page.
    pub(crate) cursor: Option<(i64, Uuid)>,
    /// Rows per page. The read returns up to `limit + 1` rows so the caller sees whether
    /// a next page exists.
    pub(crate) limit: u32,
}

// sources

/// Every issue source of `project`, ordered by name then id.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn list_sources(
    pool: &SqlitePool,
    project: ProjectId,
) -> Result<Vec<SourceRow>, MetaError> {
    let records: Vec<SourceRecord> = sqlx::query_as(
        "SELECT id, project_id, name, kind, filter, enabled, paused_reason, paused_at, \
         created_by, created_at, updated_at \
         FROM issue_sources WHERE project_id = ? ORDER BY name, id",
    )
    .bind(project.to_string())
    .fetch_all(pool)
    .await?;
    records.into_iter().map(source_from).collect()
}

/// The source `id` in `project`.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn get_source(
    pool: &SqlitePool,
    project: ProjectId,
    id: Uuid,
) -> Result<Option<SourceRow>, MetaError> {
    let record: Option<SourceRecord> = sqlx::query_as(
        "SELECT id, project_id, name, kind, filter, enabled, paused_reason, paused_at, \
         created_by, created_at, updated_at \
         FROM issue_sources WHERE project_id = ? AND id = ?",
    )
    .bind(project.to_string())
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    record.map(source_from).transpose()
}

/// The same read on the writer, inside a transaction.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn get_source_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
) -> Result<Option<SourceRow>, MetaError> {
    let record: Option<SourceRecord> = sqlx::query_as(
        "SELECT id, project_id, name, kind, filter, enabled, paused_reason, paused_at, \
         created_by, created_at, updated_at \
         FROM issue_sources WHERE project_id = ? AND id = ?",
    )
    .bind(project.to_string())
    .bind(id.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    record.map(source_from).transpose()
}

/// Inserts a source. A name already used in its project is [`MetaError::Conflict`].
///
/// # Errors
///
/// Returns [`MetaError`] when the insert fails.
pub(crate) async fn insert_source(
    conn: &mut SqliteConnection,
    row: &SourceRow,
) -> Result<(), MetaError> {
    sqlx::query(
        "INSERT INTO issue_sources (id, project_id, name, kind, filter, enabled, paused_reason, \
         paused_at, created_by, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(row.id.to_string())
    .bind(row.project_id.to_string())
    .bind(&row.name)
    .bind(row.kind.as_str())
    .bind(&row.filter)
    .bind(i64::from(row.enabled))
    .bind(&row.paused_reason)
    .bind(row.paused_at)
    .bind(row.created_by.map(|user| user.to_string()))
    .bind(row.created_at)
    .bind(row.updated_at)
    .execute(&mut *conn)
    .await
    .map_err(|error| unique_or(error, SOURCE_CONFLICT))?;
    Ok(())
}

/// Replaces the name, filter, enabled flag and pause of the source in `row`, and bumps
/// `updated_at`. The project, kind and author stay. `false` means the source is missing or
/// belongs to another project.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails, or [`MetaError::Conflict`] for a name
/// already used in the project.
pub(crate) async fn update_source(
    conn: &mut SqliteConnection,
    row: &SourceRow,
) -> Result<bool, MetaError> {
    let result = sqlx::query(
        "UPDATE issue_sources SET name = ?, filter = ?, enabled = ?, paused_reason = ?, \
         paused_at = ?, updated_at = ? WHERE project_id = ? AND id = ?",
    )
    .bind(&row.name)
    .bind(&row.filter)
    .bind(i64::from(row.enabled))
    .bind(&row.paused_reason)
    .bind(row.paused_at)
    .bind(row.updated_at)
    .bind(row.project_id.to_string())
    .bind(row.id.to_string())
    .execute(&mut *conn)
    .await
    .map_err(|error| unique_or(error, SOURCE_CONFLICT))?;
    Ok(result.rows_affected() > 0)
}

/// Deletes the source. Its issues and their events go with it (D-IC-3). `false` means the
/// source is missing or belongs to another project.
///
/// # Errors
///
/// Returns [`MetaError`] when the delete fails.
pub(crate) async fn delete_source(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
) -> Result<bool, MetaError> {
    let result = sqlx::query("DELETE FROM issue_sources WHERE project_id = ? AND id = ?")
        .bind(project.to_string())
        .bind(id.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Enabled sources of active projects of an active `org`, paused ones included (D-IC-13).
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails.
pub(crate) async fn count_enabled_sources(
    conn: &mut SqliteConnection,
    org: OrgId,
) -> Result<u64, MetaError> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM issue_sources s \
         JOIN projects p ON p.id = s.project_id \
         JOIN orgs o ON o.id = p.org_id \
         WHERE p.org_id = ? AND p.state = 'active' AND o.state = 'active' AND s.enabled = 1",
    )
    .bind(org.to_string())
    .fetch_one(&mut *conn)
    .await?;
    u64::try_from(count).map_err(|_| corrupt(SOURCES, "enabled"))
}

/// Enabled, unpaused error sources of active projects of active orgs, ordered by
/// `project_id, id`.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored id does not parse.
pub(crate) async fn sweep_sources(pool: &SqlitePool) -> Result<Vec<SweepSource>, MetaError> {
    /// `id, project_id, org_id, filter, completion_timeout_s`.
    type SweepRecord = (String, String, String, Option<String>, Option<i64>);
    let records: Vec<SweepRecord> = sqlx::query_as(
        "SELECT s.id, s.project_id, p.org_id, s.filter, \
         CAST(CASE WHEN json_valid(p.settings) AND json_type(p.settings) = 'object' \
              THEN json_extract(p.settings, '$.trace_completion_timeout_s') END AS INTEGER) \
           AS completion_timeout_s \
         FROM issue_sources s \
         JOIN projects p ON p.id = s.project_id \
         JOIN orgs o ON o.id = p.org_id \
         WHERE s.enabled = 1 AND s.paused_reason IS NULL AND s.kind = 'error' \
         AND p.state = 'active' AND o.state = 'active' \
         ORDER BY s.project_id, s.id",
    )
    .fetch_all(pool)
    .await?;
    records
        .into_iter()
        .map(|(id, project_id, org_id, filter, completion_timeout_s)| {
            Ok(SweepSource {
                id: parse_uuid(&id, SOURCES, "id")?,
                project_id: ProjectId::from_uuid(parse_uuid(&project_id, SOURCES, "project_id")?),
                org_id: OrgId::from_uuid(parse_uuid(&org_id, SOURCES, "org_id")?),
                filter,
                completion_timeout_s,
            })
        })
        .collect()
}

// issues

/// The issue of `source` with `fingerprint`, when there is one.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn issue_by_fingerprint(
    conn: &mut SqliteConnection,
    source: Uuid,
    fingerprint: &str,
) -> Result<Option<IssueRow>, MetaError> {
    let record: Option<IssueRecord> = sqlx::query_as(
        "SELECT id, project_id, source_id, fingerprint, title, description, state, regressed, \
         merged_into, first_seen, last_seen, resolved_at, resolved_by, created_at, updated_at \
         FROM issues WHERE source_id = ? AND fingerprint = ?",
    )
    .bind(source.to_string())
    .bind(fingerprint)
    .fetch_optional(&mut *conn)
    .await?;
    record.map(issue_from).transpose()
}

/// Inserts an issue. A second issue with the same source and fingerprint is
/// [`MetaError::Conflict`].
///
/// # Errors
///
/// Returns [`MetaError`] when the insert fails.
pub(crate) async fn insert_issue(
    conn: &mut SqliteConnection,
    row: &IssueRow,
) -> Result<(), MetaError> {
    sqlx::query(
        "INSERT INTO issues (id, project_id, source_id, fingerprint, title, description, state, \
         regressed, merged_into, first_seen, last_seen, resolved_at, resolved_by, created_at, \
         updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(row.id.to_string())
    .bind(row.project_id.to_string())
    .bind(row.source_id.to_string())
    .bind(&row.fingerprint)
    .bind(&row.title)
    .bind(&row.description)
    .bind(row.state.as_str())
    .bind(i64::from(row.regressed))
    .bind(row.merged_into.map(|id| id.to_string()))
    .bind(row.first_seen)
    .bind(row.last_seen)
    .bind(row.resolved_at)
    .bind(row.resolved_by.map(|user| user.to_string()))
    .bind(row.created_at)
    .bind(row.updated_at)
    .execute(&mut *conn)
    .await
    .map_err(|error| unique_or(error, FINGERPRINT_CONFLICT))?;
    Ok(())
}

/// The issue `id` in `project`.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn get_issue(
    pool: &SqlitePool,
    project: ProjectId,
    id: Uuid,
) -> Result<Option<IssueRow>, MetaError> {
    let record: Option<IssueRecord> = sqlx::query_as(
        "SELECT id, project_id, source_id, fingerprint, title, description, state, regressed, \
         merged_into, first_seen, last_seen, resolved_at, resolved_by, created_at, updated_at \
         FROM issues WHERE project_id = ? AND id = ?",
    )
    .bind(project.to_string())
    .bind(id.to_string())
    .fetch_optional(pool)
    .await?;
    record.map(issue_from).transpose()
}

/// The same read on the writer, inside a transaction.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn get_issue_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
) -> Result<Option<IssueRow>, MetaError> {
    let record: Option<IssueRecord> = sqlx::query_as(
        "SELECT id, project_id, source_id, fingerprint, title, description, state, regressed, \
         merged_into, first_seen, last_seen, resolved_at, resolved_by, created_at, updated_at \
         FROM issues WHERE project_id = ? AND id = ?",
    )
    .bind(project.to_string())
    .bind(id.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    record.map(issue_from).transpose()
}

/// Replaces the mutable columns of the issue in `row`: title, description, state, regressed,
/// merged_into, first_seen, last_seen, resolved_at, resolved_by and updated_at. The id, project,
/// source, fingerprint and created_at stay. A missing row changes nothing.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn update_issue(
    conn: &mut SqliteConnection,
    row: &IssueRow,
) -> Result<(), MetaError> {
    sqlx::query(
        "UPDATE issues SET title = ?, description = ?, state = ?, regressed = ?, merged_into = ?, \
         first_seen = ?, last_seen = ?, resolved_at = ?, resolved_by = ?, updated_at = ? \
         WHERE project_id = ? AND id = ?",
    )
    .bind(&row.title)
    .bind(&row.description)
    .bind(row.state.as_str())
    .bind(i64::from(row.regressed))
    .bind(row.merged_into.map(|id| id.to_string()))
    .bind(row.first_seen)
    .bind(row.last_seen)
    .bind(row.resolved_at)
    .bind(row.resolved_by.map(|user| user.to_string()))
    .bind(row.updated_at)
    .bind(row.project_id.to_string())
    .bind(row.id.to_string())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The ids of the issues of `project` merged into `into`, ordered by id.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored id does not parse.
pub(crate) async fn merged_ids(
    pool: &SqlitePool,
    project: ProjectId,
    into: Uuid,
) -> Result<Vec<Uuid>, MetaError> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM issues WHERE project_id = ? AND merged_into = ? ORDER BY id",
    )
    .bind(project.to_string())
    .bind(into.to_string())
    .fetch_all(pool)
    .await?;
    ids.into_iter()
        .map(|id| parse_uuid(&id, ISSUES, "id"))
        .collect()
}

/// Points every issue merged into `from` at `to` (a merge chain stays one level deep, D-IC-9).
/// Returns how many issues moved.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn retarget_merged(
    conn: &mut SqliteConnection,
    from: Uuid,
    to: Uuid,
) -> Result<u64, MetaError> {
    let result = sqlx::query("UPDATE issues SET merged_into = ? WHERE merged_into = ?")
        .bind(to.to_string())
        .bind(from.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(result.rows_affected())
}

/// `(id, title, state)` of the issues of `project` in `ids`. Missing ids are left out.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails, or `MetaError::Corrupt` for a stored value that
/// does not decode.
pub(crate) async fn titles(
    pool: &SqlitePool,
    project: ProjectId,
    ids: &[Uuid],
) -> Result<Vec<(Uuid, String, IssueState)>, MetaError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut builder =
        QueryBuilder::<Sqlite>::new("SELECT id, title, state FROM issues WHERE project_id = ");
    builder.push_bind(project.to_string());
    builder.push(" AND id IN (");
    let mut separated = builder.separated(", ");
    for id in ids {
        separated.push_bind(id.to_string());
    }
    separated.push_unseparated(")");
    let records: Vec<(String, String, String)> = builder.build_query_as().fetch_all(pool).await?;
    records
        .into_iter()
        .map(|(id, title, state)| {
            Ok((
                parse_uuid(&id, ISSUES, "id")?,
                title,
                IssueState::from_db(&state).ok_or_else(|| corrupt(ISSUES, "state"))?,
            ))
        })
        .collect()
}

/// Open issues of `project`, per source. Sources without open issues are absent.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored id does not parse.
pub(crate) async fn open_counts(
    pool: &SqlitePool,
    project: ProjectId,
) -> Result<HashMap<Uuid, u64>, MetaError> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT source_id, COUNT(*) FROM issues \
         WHERE project_id = ? AND state = 'open' GROUP BY source_id",
    )
    .bind(project.to_string())
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|(source, count)| {
            // COUNT(*) is never negative, so the conversion cannot fail.
            Ok((
                parse_uuid(&source, ISSUES, "source_id")?,
                u64::try_from(count).unwrap_or_default(),
            ))
        })
        .collect()
}

/// Issues of `project` that match `query`, in the order `query.sort` gives, ties by id
/// descending. Returns up to `query.limit + 1` rows.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn list_issues(
    pool: &SqlitePool,
    project: ProjectId,
    query: &IssueListQuery<'_>,
) -> Result<Vec<IssueRow>, MetaError> {
    let column = match query.sort {
        IssueSort::LastSeen => "last_seen",
        IssueSort::FirstSeen => "first_seen",
    };
    let mut builder = QueryBuilder::<Sqlite>::new(
        "SELECT id, project_id, source_id, fingerprint, title, description, state, regressed, \
         merged_into, first_seen, last_seen, resolved_at, resolved_by, created_at, updated_at \
         FROM issues WHERE project_id = ",
    );
    builder.push_bind(project.to_string());
    if !query.states.is_empty() {
        builder.push(" AND state IN (");
        let mut separated = builder.separated(", ");
        for state in query.states {
            separated.push_bind(state.as_str());
        }
        separated.push_unseparated(")");
    }
    if let Some(source) = query.source {
        builder
            .push(" AND source_id = ")
            .push_bind(source.to_string());
    }
    if let Some(q) = query.q {
        builder
            .push(" AND lower(title) LIKE '%' || lower(")
            .push_bind(escape_like(q))
            .push(") || '%' ESCAPE '\\'");
    }
    if let Some(ids) = query.ids {
        if ids.is_empty() {
            builder.push(" AND 0 = 1");
        } else {
            builder.push(" AND id IN (");
            let mut separated = builder.separated(", ");
            for id in ids {
                separated.push_bind(id.to_string());
            }
            separated.push_unseparated(")");
        }
    }
    if let Some((key, id)) = query.cursor {
        builder
            .push(" AND (")
            .push(column)
            .push(", id) < (")
            .push_bind(key)
            .push(", ")
            .push_bind(id.to_string())
            .push(")");
    }
    builder
        .push(" ORDER BY ")
        .push(column)
        .push(" DESC, id DESC LIMIT ")
        .push_bind(i64::from(query.limit) + 1);

    let records: Vec<IssueRecord> = builder.build_query_as().fetch_all(pool).await?;
    records.into_iter().map(issue_from).collect()
}

// events

/// Inserts an event.
///
/// # Errors
///
/// Returns [`MetaError`] when the insert fails.
pub(crate) async fn insert_event(
    conn: &mut SqliteConnection,
    row: &EventRow,
) -> Result<(), MetaError> {
    sqlx::query(
        "INSERT INTO issue_events (id, issue_id, kind, actor_user_id, from_value, to_value, at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(row.id.to_string())
    .bind(row.issue_id.to_string())
    .bind(row.kind.as_str())
    .bind(row.actor_user_id.map(|user| user.to_string()))
    .bind(&row.from_value)
    .bind(&row.to_value)
    .bind(row.at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The events of `issue`, newest first, ties by id descending. At most `limit` rows.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn list_events(
    pool: &SqlitePool,
    issue: Uuid,
    limit: u32,
) -> Result<Vec<EventRow>, MetaError> {
    let records: Vec<EventRecord> = sqlx::query_as(
        "SELECT id, issue_id, kind, actor_user_id, from_value, to_value, at FROM issue_events \
         WHERE issue_id = ? ORDER BY at DESC, id DESC LIMIT ?",
    )
    .bind(issue.to_string())
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await?;
    records.into_iter().map(event_from).collect()
}

// rows

#[derive(sqlx::FromRow)]
struct SourceRecord {
    id: String,
    project_id: String,
    name: String,
    kind: String,
    filter: Option<String>,
    enabled: i64,
    paused_reason: Option<String>,
    paused_at: Option<i64>,
    created_by: Option<String>,
    created_at: i64,
    updated_at: i64,
}

fn source_from(record: SourceRecord) -> Result<SourceRow, MetaError> {
    Ok(SourceRow {
        id: parse_uuid(&record.id, SOURCES, "id")?,
        project_id: ProjectId::from_uuid(parse_uuid(&record.project_id, SOURCES, "project_id")?),
        name: record.name,
        kind: SourceKind::from_db(&record.kind).ok_or_else(|| corrupt(SOURCES, "kind"))?,
        filter: record.filter,
        enabled: record.enabled != 0,
        paused_reason: record.paused_reason,
        paused_at: record.paused_at,
        created_by: optional_uuid(record.created_by, SOURCES, "created_by")?,
        created_at: record.created_at,
        updated_at: record.updated_at,
    })
}

#[derive(sqlx::FromRow)]
struct IssueRecord {
    id: String,
    project_id: String,
    source_id: String,
    fingerprint: Option<String>,
    title: String,
    description: Option<String>,
    state: String,
    regressed: i64,
    merged_into: Option<String>,
    first_seen: i64,
    last_seen: i64,
    resolved_at: Option<i64>,
    resolved_by: Option<String>,
    created_at: i64,
    updated_at: i64,
}

fn issue_from(record: IssueRecord) -> Result<IssueRow, MetaError> {
    Ok(IssueRow {
        id: parse_uuid(&record.id, ISSUES, "id")?,
        project_id: ProjectId::from_uuid(parse_uuid(&record.project_id, ISSUES, "project_id")?),
        source_id: parse_uuid(&record.source_id, ISSUES, "source_id")?,
        fingerprint: record.fingerprint,
        title: record.title,
        description: record.description,
        state: IssueState::from_db(&record.state).ok_or_else(|| corrupt(ISSUES, "state"))?,
        regressed: record.regressed != 0,
        merged_into: optional_uuid(record.merged_into, ISSUES, "merged_into")?,
        first_seen: record.first_seen,
        last_seen: record.last_seen,
        resolved_at: record.resolved_at,
        resolved_by: optional_uuid(record.resolved_by, ISSUES, "resolved_by")?,
        created_at: record.created_at,
        updated_at: record.updated_at,
    })
}

#[derive(sqlx::FromRow)]
struct EventRecord {
    id: String,
    issue_id: String,
    kind: String,
    actor_user_id: Option<String>,
    from_value: Option<String>,
    to_value: Option<String>,
    at: i64,
}

fn event_from(record: EventRecord) -> Result<EventRow, MetaError> {
    Ok(EventRow {
        id: parse_uuid(&record.id, EVENTS, "id")?,
        issue_id: parse_uuid(&record.issue_id, EVENTS, "issue_id")?,
        kind: EventKind::from_db(&record.kind).ok_or_else(|| corrupt(EVENTS, "kind"))?,
        actor_user_id: optional_uuid(record.actor_user_id, EVENTS, "actor_user_id")?,
        from_value: record.from_value,
        to_value: record.to_value,
        at: record.at,
    })
}

/// `text` with the LIKE wildcards and the escape character escaped (`ESCAPE '\'`).
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

fn corrupt(table: &'static str, column: &'static str) -> MetaError {
    MetaError::Corrupt { table, column }
}

fn parse_uuid(value: &str, table: &'static str, column: &'static str) -> Result<Uuid, MetaError> {
    value.parse().map_err(|_| corrupt(table, column))
}

fn optional_uuid(
    value: Option<String>,
    table: &'static str,
    column: &'static str,
) -> Result<Option<Uuid>, MetaError> {
    value
        .map(|text| parse_uuid(&text, table, column))
        .transpose()
}

#[cfg(test)]
mod tests {
    use cetra_core::ids::{OrgId, ProjectId};
    use uuid::Uuid;

    use super::{
        EventKind, EventRow, IssueListQuery, IssueRow, IssueSort, IssueState, SourceKind,
        SourceRow, count_enabled_sources, delete_source, get_issue, get_issue_tx, get_source,
        insert_event, insert_issue, insert_source, issue_by_fingerprint, list_events, list_issues,
        list_sources, merged_ids, open_counts, retarget_merged, sweep_sources, titles,
        update_issue, update_source,
    };
    use crate::meta::orgs::insert_org;
    use crate::meta::projects::{delete_project, insert_project, mark_project_deleting};
    use crate::meta::{MetaDb, MetaError};

    const NOW: i64 = 1_000;

    /// An org with the given project slugs; returns the org and the project ids in order.
    async fn seed(db: &MetaDb, slugs: &'static [&'static str]) -> (OrgId, Vec<ProjectId>) {
        db.write(move |conn| {
            Box::pin(async move {
                let org = OrgId::new_v7();
                insert_org(conn, org, "Acme", "acme", NOW).await?;
                let mut projects = Vec::new();
                for slug in slugs {
                    let row = insert_project(conn, org, slug, slug, None, NOW).await?;
                    projects.push(row.id);
                }
                Ok((org, projects))
            })
        })
        .await
        .unwrap()
    }

    fn source(project: ProjectId, name: &str, enabled: bool) -> SourceRow {
        SourceRow {
            id: Uuid::now_v7(),
            project_id: project,
            name: name.to_owned(),
            kind: SourceKind::Error,
            filter: None,
            enabled,
            paused_reason: None,
            paused_at: None,
            created_by: None,
            created_at: NOW,
            updated_at: NOW,
        }
    }

    fn issue(
        id: u128,
        source: Uuid,
        project: ProjectId,
        title: &str,
        state: IssueState,
        last_seen: i64,
    ) -> IssueRow {
        IssueRow {
            id: Uuid::from_u128(id),
            project_id: project,
            source_id: source,
            fingerprint: None,
            title: title.to_owned(),
            description: None,
            state,
            regressed: false,
            merged_into: None,
            first_seen: last_seen,
            last_seen,
            resolved_at: None,
            resolved_by: None,
            created_at: NOW,
            updated_at: NOW,
        }
    }

    async fn put_source(db: &MetaDb, row: SourceRow) {
        db.write(move |conn| Box::pin(async move { insert_source(conn, &row).await }))
            .await
            .unwrap();
    }

    async fn put_issue(db: &MetaDb, row: IssueRow) {
        db.write(move |conn| Box::pin(async move { insert_issue(conn, &row).await }))
            .await
            .unwrap();
    }

    async fn list(db: &MetaDb, project: ProjectId, query: IssueListQuery<'_>) -> Vec<IssueRow> {
        list_issues(db.reader(), project, &query).await.unwrap()
    }

    async fn count(db: &MetaDb, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql)
            .fetch_one(db.reader())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_source_round_trips_and_a_duplicate_name_is_a_conflict() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let project = projects[0];
        let mut row = source(project, "errors", true);
        row.filter = Some("service.name = 'api'".to_owned());
        row.paused_reason = Some("errors".to_owned());
        row.paused_at = Some(NOW);
        put_source(&db, row.clone()).await;

        assert_eq!(
            get_source(db.reader(), project, row.id).await.unwrap(),
            Some(row.clone())
        );
        assert_eq!(
            list_sources(db.reader(), project).await.unwrap(),
            vec![row.clone()]
        );
        assert_eq!(
            get_source(db.reader(), ProjectId::new_v7(), row.id)
                .await
                .unwrap(),
            None
        );

        let duplicate = source(project, "errors", true);
        let error = db
            .write(move |conn| Box::pin(async move { insert_source(conn, &duplicate).await }))
            .await
            .unwrap_err();
        assert!(matches!(error, MetaError::Conflict(_)), "{error:?}");
    }

    #[tokio::test]
    async fn update_source_replaces_the_editable_columns() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let project = projects[0];
        let row = source(project, "errors", true);
        put_source(&db, row.clone()).await;

        let mut changed = row.clone();
        changed.name = "api errors".to_owned();
        changed.enabled = false;
        changed.updated_at = 2_000;
        let updated = db
            .write(move |conn| Box::pin(async move { update_source(conn, &changed).await }))
            .await
            .unwrap();
        assert!(updated);
        let stored = get_source(db.reader(), project, row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.name, "api errors");
        assert!(!stored.enabled);
        assert_eq!(stored.updated_at, 2_000);
        assert_eq!(stored.created_at, NOW);
    }

    #[tokio::test]
    async fn count_enabled_sources_counts_enabled_sources_of_active_projects_only() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, projects) = seed(&db, &["a", "b", "c"]).await;
        put_source(&db, source(projects[0], "one", true)).await;
        put_source(&db, source(projects[0], "two", false)).await;
        put_source(&db, source(projects[1], "three", true)).await;
        put_source(&db, source(projects[2], "four", true)).await;
        let deleting = projects[2];
        db.write(move |conn| {
            Box::pin(async move { mark_project_deleting(conn, deleting, NOW).await })
        })
        .await
        .unwrap();

        let total = db
            .write(move |conn| Box::pin(async move { count_enabled_sources(conn, org).await }))
            .await
            .unwrap();
        assert_eq!(total, 2);
    }

    #[tokio::test]
    async fn sweep_sources_leaves_out_disabled_paused_and_inactive_sources() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["a", "b"]).await;
        let wanted = source(projects[0], "wanted", true);
        put_source(&db, wanted.clone()).await;
        put_source(&db, source(projects[0], "disabled", false)).await;
        let mut paused = source(projects[0], "paused", true);
        paused.paused_reason = Some("errors".to_owned());
        put_source(&db, paused).await;
        put_source(&db, source(projects[1], "inactive", true)).await;
        let inactive = projects[1];
        db.write(move |conn| {
            Box::pin(async move { mark_project_deleting(conn, inactive, NOW).await })
        })
        .await
        .unwrap();

        let swept = sweep_sources(db.reader()).await.unwrap();
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].id, wanted.id);
        assert_eq!(swept[0].project_id, projects[0]);
        assert_eq!(swept[0].completion_timeout_s, None);
    }

    #[tokio::test]
    async fn issue_by_fingerprint_finds_by_source_and_the_index_refuses_a_second_one() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let project = projects[0];
        let first_source = source(project, "errors", true);
        let second_source = source(project, "other", true);
        put_source(&db, first_source.clone()).await;
        put_source(&db, second_source.clone()).await;
        let mut row = issue(1, first_source.id, project, "boom", IssueState::Open, 10);
        row.fingerprint = Some("f1".to_owned());
        put_issue(&db, row.clone()).await;

        let source_id = first_source.id;
        let found = db
            .write(move |conn| {
                Box::pin(async move { issue_by_fingerprint(conn, source_id, "f1").await })
            })
            .await
            .unwrap();
        assert_eq!(found, Some(row.clone()));
        let other_source = second_source.id;
        let missing = db
            .write(move |conn| {
                Box::pin(async move { issue_by_fingerprint(conn, other_source, "f1").await })
            })
            .await
            .unwrap();
        assert_eq!(missing, None);

        let mut duplicate = issue(
            2,
            first_source.id,
            project,
            "boom again",
            IssueState::Open,
            20,
        );
        duplicate.fingerprint = Some("f1".to_owned());
        let error = db
            .write(move |conn| Box::pin(async move { insert_issue(conn, &duplicate).await }))
            .await
            .unwrap_err();
        assert!(matches!(error, MetaError::Conflict(_)), "{error:?}");
    }

    #[tokio::test]
    async fn get_and_update_issue_round_trip() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let project = projects[0];
        let src = source(project, "errors", true);
        put_source(&db, src.clone()).await;
        let row = issue(1, src.id, project, "boom", IssueState::Open, 10);
        put_issue(&db, row.clone()).await;

        let mut changed = row.clone();
        changed.title = "renamed".to_owned();
        changed.state = IssueState::Resolved;
        changed.regressed = true;
        changed.resolved_at = Some(30);
        changed.updated_at = 40;
        let writer = changed.clone();
        db.write(move |conn| Box::pin(async move { update_issue(conn, &writer).await }))
            .await
            .unwrap();
        let stored = get_issue(db.reader(), project, row.id).await.unwrap();
        assert_eq!(stored, Some(changed.clone()));
        assert_eq!(
            get_issue(db.reader(), ProjectId::new_v7(), row.id)
                .await
                .unwrap(),
            None
        );

        let found = titles(db.reader(), project, &[row.id]).await.unwrap();
        assert_eq!(
            found,
            vec![(row.id, "renamed".to_owned(), IssueState::Resolved)]
        );

        let id = row.id;
        let in_transaction = db
            .write(move |conn| Box::pin(async move { get_issue_tx(conn, project, id).await }))
            .await
            .unwrap();
        assert_eq!(in_transaction, Some(changed));
    }

    #[tokio::test]
    async fn list_issues_sorts_by_first_seen() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let project = projects[0];
        let src = source(project, "errors", true);
        put_source(&db, src.clone()).await;
        // Last seen runs the other way, so the sort key decides the order.
        let mut early = issue(1, src.id, project, "early", IssueState::Open, 500);
        early.first_seen = 10;
        let mut late = issue(2, src.id, project, "late", IssueState::Open, 20);
        late.first_seen = 90;
        put_issue(&db, early).await;
        put_issue(&db, late).await;

        let query = IssueListQuery {
            states: &[],
            source: None,
            q: None,
            ids: None,
            sort: IssueSort::FirstSeen,
            cursor: None,
            limit: 10,
        };
        let rows = list(&db, project, query).await;
        let order: Vec<u128> = rows.iter().map(|row| row.id.as_u128()).collect();
        assert_eq!(order, vec![2, 1]);
    }

    #[tokio::test]
    async fn list_issues_filters_sorts_and_pages() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let project = projects[0];
        let s1 = source(project, "one", true);
        let s2 = source(project, "two", true);
        put_source(&db, s1.clone()).await;
        put_source(&db, s2.clone()).await;
        put_issue(
            &db,
            issue(
                1,
                s1.id,
                project,
                "Timeout 50% of calls",
                IssueState::Open,
                100,
            ),
        )
        .await;
        put_issue(
            &db,
            issue(
                2,
                s1.id,
                project,
                "Parse error 50x",
                IssueState::Resolved,
                300,
            ),
        )
        .await;
        put_issue(
            &db,
            issue(3, s2.id, project, "Timeout", IssueState::Open, 200),
        )
        .await;
        put_issue(
            &db,
            issue(4, s1.id, project, "other 50%", IssueState::Open, 200),
        )
        .await;
        let mut merged = issue(5, s1.id, project, "merged", IssueState::Merged, 50);
        merged.merged_into = Some(Uuid::from_u128(1));
        put_issue(&db, merged).await;

        let ids = |rows: &[IssueRow]| rows.iter().map(|row| row.id.as_u128()).collect::<Vec<_>>();
        fn base(states: &[IssueState]) -> IssueListQuery<'_> {
            IssueListQuery {
                states,
                source: None,
                q: None,
                ids: None,
                sort: IssueSort::LastSeen,
                cursor: None,
                limit: 10,
            }
        }

        let open = list(&db, project, base(&[IssueState::Open])).await;
        assert_eq!(ids(&open), vec![4, 3, 1]);
        let resolved = list(&db, project, base(&[IssueState::Resolved])).await;
        assert_eq!(ids(&resolved), vec![2]);
        // Equal last_seen (issues 3 and 4 at 200) sorts by id descending.
        let all = list(&db, project, base(&[])).await;
        assert_eq!(ids(&all), vec![2, 4, 3, 1, 5]);

        let mut by_source = base(&[]);
        by_source.source = Some(s2.id);
        assert_eq!(ids(&list(&db, project, by_source).await), vec![3]);

        let picked = [Uuid::from_u128(1), Uuid::from_u128(3)];
        let mut by_ids = base(&[]);
        by_ids.ids = Some(&picked);
        assert_eq!(ids(&list(&db, project, by_ids).await), vec![3, 1]);
        let none: [Uuid; 0] = [];
        let mut no_ids = base(&[]);
        no_ids.ids = Some(&none);
        assert!(list(&db, project, no_ids).await.is_empty());

        let mut literal = base(&[]);
        literal.q = Some("50%");
        assert_eq!(ids(&list(&db, project, literal).await), vec![4, 1]);
        let mut underscore = base(&[]);
        underscore.q = Some("_");
        assert!(list(&db, project, underscore).await.is_empty());

        let mut first_page = base(&[]);
        first_page.limit = 2;
        let page = list(&db, project, first_page).await;
        // limit + 1 rows come back; the caller keeps the first two.
        assert_eq!(ids(&page), vec![2, 4, 3]);
        let last = &page[1];
        let mut second_page = base(&[]);
        second_page.limit = 2;
        second_page.cursor = Some((last.last_seen, last.id));
        assert_eq!(ids(&list(&db, project, second_page).await), vec![3, 1, 5]);
    }

    #[tokio::test]
    async fn open_counts_counts_open_issues_per_source() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let project = projects[0];
        let s1 = source(project, "one", true);
        let s2 = source(project, "two", true);
        put_source(&db, s1.clone()).await;
        put_source(&db, s2.clone()).await;
        put_issue(&db, issue(1, s1.id, project, "a", IssueState::Open, 1)).await;
        put_issue(&db, issue(2, s1.id, project, "b", IssueState::Resolved, 1)).await;
        put_issue(&db, issue(3, s2.id, project, "c", IssueState::Open, 1)).await;
        put_issue(&db, issue(4, s1.id, project, "d", IssueState::Open, 1)).await;
        put_issue(&db, issue(5, s1.id, project, "e", IssueState::Ignored, 1)).await;

        let counts = open_counts(db.reader(), project).await.unwrap();
        assert_eq!(counts.len(), 2);
        assert_eq!(counts.get(&s1.id), Some(&2));
        assert_eq!(counts.get(&s2.id), Some(&1));
    }

    #[tokio::test]
    async fn retarget_merged_moves_a_chain_to_the_final_target() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let project = projects[0];
        let src = source(project, "errors", true);
        put_source(&db, src.clone()).await;
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let c = Uuid::from_u128(3);
        put_issue(&db, issue(3, src.id, project, "c", IssueState::Open, 1)).await;
        let mut row_b = issue(2, src.id, project, "b", IssueState::Merged, 1);
        row_b.merged_into = Some(c);
        put_issue(&db, row_b).await;
        let mut row_a = issue(1, src.id, project, "a", IssueState::Merged, 1);
        row_a.merged_into = Some(b);
        put_issue(&db, row_a).await;

        let moved = db
            .write(move |conn| Box::pin(async move { retarget_merged(conn, b, c).await }))
            .await
            .unwrap();
        assert_eq!(moved, 1);
        let a_row = get_issue(db.reader(), project, a).await.unwrap().unwrap();
        assert_eq!(a_row.merged_into, Some(c));
        let mut into_c = merged_ids(db.reader(), project, c).await.unwrap();
        into_c.sort();
        assert_eq!(into_c, vec![a, b]);
    }

    #[tokio::test]
    async fn titles_returns_the_named_issues_of_the_project() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let project = projects[0];
        let src = source(project, "errors", true);
        put_source(&db, src.clone()).await;
        put_issue(&db, issue(1, src.id, project, "first", IssueState::Open, 1)).await;
        put_issue(
            &db,
            issue(2, src.id, project, "second", IssueState::Ignored, 1),
        )
        .await;

        let found = titles(
            db.reader(),
            project,
            &[Uuid::from_u128(2), Uuid::from_u128(9)],
        )
        .await
        .unwrap();
        assert_eq!(
            found,
            vec![(Uuid::from_u128(2), "second".to_owned(), IssueState::Ignored)]
        );
        assert!(titles(db.reader(), project, &[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_events_is_newest_first_and_limited() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let project = projects[0];
        let src = source(project, "errors", true);
        put_source(&db, src.clone()).await;
        let issue_id = Uuid::from_u128(1);
        put_issue(&db, issue(1, src.id, project, "a", IssueState::Open, 1)).await;
        db.write(move |conn| {
            Box::pin(async move {
                for (id, at) in [(10_u128, 1_i64), (11, 3), (12, 2)] {
                    insert_event(
                        conn,
                        &EventRow {
                            id: Uuid::from_u128(id),
                            issue_id,
                            kind: EventKind::StateChanged,
                            actor_user_id: None,
                            from_value: Some("open".to_owned()),
                            to_value: Some("resolved".to_owned()),
                            at,
                        },
                    )
                    .await?;
                }
                Ok(())
            })
        })
        .await
        .unwrap();

        let all = list_events(db.reader(), issue_id, 10).await.unwrap();
        assert_eq!(
            all.iter().map(|event| event.at).collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
        assert_eq!(all[0].kind, EventKind::StateChanged);
        let limited = list_events(db.reader(), issue_id, 2).await.unwrap();
        assert_eq!(limited.len(), 2);
    }

    #[tokio::test]
    async fn deleting_a_project_cascades_to_sources_issues_and_events() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let project = projects[0];
        let src = source(project, "errors", true);
        put_source(&db, src.clone()).await;
        put_issue(&db, issue(1, src.id, project, "a", IssueState::Open, 1)).await;
        db.write(move |conn| {
            Box::pin(async move {
                insert_event(
                    conn,
                    &EventRow {
                        id: Uuid::from_u128(10),
                        issue_id: Uuid::from_u128(1),
                        kind: EventKind::Created,
                        actor_user_id: None,
                        from_value: None,
                        to_value: None,
                        at: 1,
                    },
                )
                .await
            })
        })
        .await
        .unwrap();

        db.write(move |conn| Box::pin(async move { delete_project(conn, project).await }))
            .await
            .unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) FROM issue_sources").await, 0);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM issues").await, 0);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM issue_events").await, 0);
    }

    #[tokio::test]
    async fn deleting_a_source_cascades_to_its_issues_and_events() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let project = projects[0];
        let gone = source(project, "gone", true);
        let kept = source(project, "kept", true);
        put_source(&db, gone.clone()).await;
        put_source(&db, kept.clone()).await;
        put_issue(&db, issue(1, gone.id, project, "a", IssueState::Open, 1)).await;
        put_issue(&db, issue(2, kept.id, project, "b", IssueState::Open, 1)).await;
        db.write(move |conn| {
            Box::pin(async move {
                insert_event(
                    conn,
                    &EventRow {
                        id: Uuid::from_u128(10),
                        issue_id: Uuid::from_u128(1),
                        kind: EventKind::Created,
                        actor_user_id: None,
                        from_value: None,
                        to_value: None,
                        at: 1,
                    },
                )
                .await
            })
        })
        .await
        .unwrap();

        let removed = db
            .write(move |conn| Box::pin(async move { delete_source(conn, project, gone.id).await }))
            .await
            .unwrap();
        assert!(removed);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM issues").await, 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM issue_events").await, 0);
        assert_eq!(
            list_sources(db.reader(), project).await.unwrap(),
            vec![kept]
        );
    }
}
