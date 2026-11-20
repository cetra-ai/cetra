//! `issue_sources`, `issues` and `issue_events` rows (spec 04 § Issues, D-IC-1, D-IC-3,
//! D-IC-10). `kind`, `state` and the event kind are stored as text; a stored value outside
//! its list is corrupt.

use std::collections::HashMap;

use cetra_core::ids::{OrgId, ProjectId, SpanId, TraceId};
use sqlx::{QueryBuilder, Sqlite, SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::eval_jobs::DAY_NS;
use super::{MetaError, unique_or};

const SOURCES: &str = "issue_sources";
const ISSUES: &str = "issues";
const EVENTS: &str = "issue_events";
const CANDIDATES: &str = "issue_candidates";
const REFITS: &str = "issue_refits";

/// Interruptions a candidate may take before it fails (D-I-12, as for eval jobs).
const MAX_CANDIDATE_INTERRUPTIONS: i64 = 3;

/// Shown when a source name is taken in its project.
const SOURCE_CONFLICT: &str = "an issue source with this name already exists";
/// Shown when an error source already has an issue with the same fingerprint.
const FINGERPRINT_CONFLICT: &str = "an issue with this fingerprint already exists";

/// The kind of an issue source. M3 adds kinds; the Rust enum is the only gate (D-IC-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKind {
    /// Error spans of completed traces, fingerprinted (D-IC-2).
    Error,
    /// Bad scores of the source's score names become candidates (D-IC-1).
    Score,
    /// Feedback scores of the source's feedback sources become candidates (D-IC-1).
    Feedback,
    /// New production traces become candidates (D-IC-1).
    Topic,
}

impl SourceKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Score => "score",
            Self::Feedback => "feedback",
            Self::Topic => "topic",
        }
    }

    /// Whether the kind is an LLM kind (`score`, `feedback` or `topic`, D-IC-1).
    pub(crate) fn is_llm(self) -> bool {
        !matches!(self, Self::Error)
    }

    fn from_db(value: &str) -> Option<Self> {
        match value {
            "error" => Some(Self::Error),
            "score" => Some(Self::Score),
            "feedback" => Some(Self::Feedback),
            "topic" => Some(Self::Topic),
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
    /// Share of the matching traces or scores that become candidates, in `(0, 1]` (LLM kinds).
    pub(crate) sample_rate: Option<f64>,
    /// Most candidates admitted per UTC day (LLM kinds).
    pub(crate) daily_cap: Option<u32>,
    /// Score names read by a `score` source. Empty is stored as NULL.
    pub(crate) score_names: Vec<String>,
    /// `end_user` and/or `human`, read by a `feedback` source. Empty is stored as NULL.
    pub(crate) feedback_sources: Vec<String>,
    /// Connection of the chat model that summarizes and names (LLM kinds).
    pub(crate) chat_connection_id: Option<Uuid>,
    /// Chat model of that connection.
    pub(crate) chat_model: Option<String>,
    /// Connection of the embedding model. Fixed for the source's life (D-IC-5).
    pub(crate) embed_connection_id: Option<Uuid>,
    /// Embedding model of that connection. Fixed for the source's life (D-IC-5).
    pub(crate) embed_model: Option<String>,
    /// Vector dimension of the embedding model, set once (D-IC-5).
    pub(crate) embed_dimension: Option<u32>,
    /// Cosine similarity at which a candidate joins an issue (D-IC-7).
    pub(crate) floor: Option<f64>,
    /// Smallest group a refit turns into a new issue (D-IC-8).
    pub(crate) min_issue_size: Option<u32>,
    /// Spend limit per `budget_period`, in USD. Absent means no budget.
    pub(crate) budget_usd: Option<f64>,
    /// `day` or `month`.
    pub(crate) budget_period: Option<String>,
    /// The provider's error text for a connection pause, shown on the source page.
    pub(crate) paused_detail: Option<String>,
    /// When the next scheduled refit is due, nanoseconds.
    pub(crate) next_refit_at: Option<i64>,
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
    /// Mean of the member vectors, for an LLM source's issue (D-IC-7). `None` for error issues.
    pub(crate) centroid: Option<Vec<f32>>,
    /// Members counted in the centroid, at most 500 (D-IC-7).
    pub(crate) member_count: u64,
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
         created_by, created_at, updated_at, sample_rate, daily_cap, score_names, feedback_sources, \
         chat_connection_id, chat_model, embed_connection_id, embed_model, embed_dimension, floor, \
         min_issue_size, budget_usd, budget_period, paused_detail, next_refit_at \
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
         created_by, created_at, updated_at, sample_rate, daily_cap, score_names, feedback_sources, \
         chat_connection_id, chat_model, embed_connection_id, embed_model, embed_dimension, floor, \
         min_issue_size, budget_usd, budget_period, paused_detail, next_refit_at \
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
         created_by, created_at, updated_at, sample_rate, daily_cap, score_names, feedback_sources, \
         chat_connection_id, chat_model, embed_connection_id, embed_model, embed_dimension, floor, \
         min_issue_size, budget_usd, budget_period, paused_detail, next_refit_at \
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
    let score_names = list_json(&row.score_names, "score_names")?;
    let feedback_sources = list_json(&row.feedback_sources, "feedback_sources")?;
    sqlx::query(
        "INSERT INTO issue_sources (id, project_id, name, kind, filter, enabled, paused_reason, \
         paused_at, created_by, created_at, updated_at, sample_rate, daily_cap, score_names, \
         feedback_sources, chat_connection_id, chat_model, embed_connection_id, embed_model, \
         embed_dimension, floor, min_issue_size, budget_usd, budget_period, paused_detail, \
         next_refit_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
    .bind(row.sample_rate)
    .bind(row.daily_cap.map(i64::from))
    .bind(score_names)
    .bind(feedback_sources)
    .bind(row.chat_connection_id.map(|id| id.to_string()))
    .bind(&row.chat_model)
    .bind(row.embed_connection_id.map(|id| id.to_string()))
    .bind(&row.embed_model)
    .bind(row.embed_dimension.map(i64::from))
    .bind(row.floor)
    .bind(row.min_issue_size.map(i64::from))
    .bind(row.budget_usd)
    .bind(&row.budget_period)
    .bind(&row.paused_detail)
    .bind(row.next_refit_at)
    .execute(&mut *conn)
    .await
    .map_err(|error| unique_or(error, SOURCE_CONFLICT))?;
    Ok(())
}

/// Replaces every editable column of the source in `row` and bumps `updated_at`: the name,
/// filter, enabled flag, pause, and the LLM settings except the embedding connection, model
/// and dimension, which change only through `set_embed_dimension` (D-IC-5). The project, kind
/// and author stay. `false` means the source is missing or belongs to another project.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails, or [`MetaError::Conflict`] for a name
/// already used in the project.
pub(crate) async fn update_source(
    conn: &mut SqliteConnection,
    row: &SourceRow,
) -> Result<bool, MetaError> {
    let score_names = list_json(&row.score_names, "score_names")?;
    let feedback_sources = list_json(&row.feedback_sources, "feedback_sources")?;
    let result = sqlx::query(
        "UPDATE issue_sources SET name = ?, filter = ?, enabled = ?, paused_reason = ?, \
         paused_at = ?, updated_at = ?, sample_rate = ?, daily_cap = ?, score_names = ?, \
         feedback_sources = ?, chat_connection_id = ?, chat_model = ?, floor = ?, \
         min_issue_size = ?, budget_usd = ?, budget_period = ?, paused_detail = ?, \
         next_refit_at = ? WHERE project_id = ? AND id = ?",
    )
    .bind(&row.name)
    .bind(&row.filter)
    .bind(i64::from(row.enabled))
    .bind(&row.paused_reason)
    .bind(row.paused_at)
    .bind(row.updated_at)
    .bind(row.sample_rate)
    .bind(row.daily_cap.map(i64::from))
    .bind(score_names)
    .bind(feedback_sources)
    .bind(row.chat_connection_id.map(|id| id.to_string()))
    .bind(&row.chat_model)
    .bind(row.floor)
    .bind(row.min_issue_size.map(i64::from))
    .bind(row.budget_usd)
    .bind(&row.budget_period)
    .bind(&row.paused_detail)
    .bind(row.next_refit_at)
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
         merged_into, first_seen, last_seen, resolved_at, resolved_by, created_at, updated_at, \
         centroid, member_count FROM issues WHERE source_id = ? AND fingerprint = ?",
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
    let member_count =
        i64::try_from(row.member_count).map_err(|_| corrupt(ISSUES, "member_count"))?;
    sqlx::query(
        "INSERT INTO issues (id, project_id, source_id, fingerprint, title, description, state, \
         regressed, merged_into, first_seen, last_seen, resolved_at, resolved_by, created_at, \
         updated_at, centroid, member_count) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
    .bind(row.centroid.as_deref().map(encode_vector))
    .bind(member_count)
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
         merged_into, first_seen, last_seen, resolved_at, resolved_by, created_at, updated_at, \
         centroid, member_count FROM issues WHERE project_id = ? AND id = ?",
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
         merged_into, first_seen, last_seen, resolved_at, resolved_by, created_at, updated_at, \
         centroid, member_count FROM issues WHERE project_id = ? AND id = ?",
    )
    .bind(project.to_string())
    .bind(id.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    record.map(issue_from).transpose()
}

/// Replaces the mutable columns of the issue in `row`: title, description, state, regressed,
/// merged_into, first_seen, last_seen, resolved_at, resolved_by, centroid, member_count and
/// updated_at. The id, project, source, fingerprint and created_at stay. A missing row changes
/// nothing.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn update_issue(
    conn: &mut SqliteConnection,
    row: &IssueRow,
) -> Result<(), MetaError> {
    let member_count =
        i64::try_from(row.member_count).map_err(|_| corrupt(ISSUES, "member_count"))?;
    sqlx::query(
        "UPDATE issues SET title = ?, description = ?, state = ?, regressed = ?, merged_into = ?, \
         first_seen = ?, last_seen = ?, resolved_at = ?, resolved_by = ?, centroid = ?, \
         member_count = ?, updated_at = ? WHERE project_id = ? AND id = ?",
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
    .bind(row.centroid.as_deref().map(encode_vector))
    .bind(member_count)
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
         merged_into, first_seen, last_seen, resolved_at, resolved_by, created_at, updated_at, \
         centroid, member_count FROM issues WHERE project_id = ",
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

// candidates

/// The state of an issue candidate (D-IC-12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateStatus {
    Queued,
    Running,
    Pending,
    Assigned,
    Outlier,
    Dropped,
    Failed,
    Skipped,
}

impl CandidateStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Pending => "pending",
            Self::Assigned => "assigned",
            Self::Outlier => "outlier",
            Self::Dropped => "dropped",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }

    fn from_db(value: &str) -> Option<Self> {
        match value {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "pending" => Some(Self::Pending),
            "assigned" => Some(Self::Assigned),
            "outlier" => Some(Self::Outlier),
            "dropped" => Some(Self::Dropped),
            "failed" => Some(Self::Failed),
            "skipped" => Some(Self::Skipped),
            _ => None,
        }
    }
}

/// What a candidate is about: a trace, one span of a trace, or a session (D-IC-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateTarget {
    Trace,
    Span,
    Session,
}

impl CandidateTarget {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Span => "span",
            Self::Session => "session",
        }
    }

    fn from_db(value: &str) -> Option<Self> {
        match value {
            "trace" => Some(Self::Trace),
            "span" => Some(Self::Span),
            "session" => Some(Self::Session),
            _ => None,
        }
    }
}

/// One stored issue candidate (D-IC-1, D-IC-12). Times are nanoseconds.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CandidateRow {
    pub(crate) id: Uuid,
    pub(crate) project_id: ProjectId,
    pub(crate) source_id: Uuid,
    pub(crate) target: CandidateTarget,
    pub(crate) trace_id: Option<TraceId>,
    pub(crate) span_id: Option<SpanId>,
    pub(crate) session_id: Option<String>,
    pub(crate) user_id: Option<String>,
    pub(crate) trace_start: Option<i64>,
    pub(crate) score_id: Option<Uuid>,
    /// `<source>:<trace>`, `<source>:<trace>:<span>` or `<source>:session:<session>`.
    pub(crate) dedup_key: String,
    pub(crate) status: CandidateStatus,
    pub(crate) attempts: u32,
    /// Claims so far; the fence of every write made by a worker.
    pub(crate) claims: u32,
    pub(crate) interruptions: u32,
    pub(crate) load_retries: u32,
    pub(crate) not_before: i64,
    pub(crate) error_class: Option<String>,
    pub(crate) issue_id: Option<Uuid>,
    pub(crate) summary: Option<String>,
    pub(crate) summary_prompt_version: Option<u32>,
    /// Embedding of the summary (D-IC-5).
    pub(crate) vector: Option<Vec<f32>>,
    pub(crate) embed_model: Option<String>,
    pub(crate) dimension: Option<u32>,
    pub(crate) call_trace_id: Option<TraceId>,
    /// USD spent on the candidate's calls.
    pub(crate) cost_usd: f64,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

/// How a worker leaves a running candidate (A5). The spend is passed to
/// [`finish_candidate`] separately.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CandidateFinish {
    /// The summary is stored; the candidate stays `running`.
    Summarized {
        summary: String,
        prompt_version: u32,
        call_trace_id: TraceId,
    },
    /// The embedding is stored; the candidate waits for a refit.
    Pending {
        vector: Vec<f32>,
        embed_model: String,
        dimension: u32,
    },
    /// The embedding is stored and the candidate joined `issue_id`.
    Assigned {
        issue_id: Uuid,
        vector: Vec<f32>,
        embed_model: String,
        dimension: u32,
    },
    /// The candidate is not kept.
    Dropped,
    /// A failed attempt goes back to `queued`.
    Retry {
        not_before: i64,
    },
    /// A failed trace load goes back to `queued` and counts a load retry.
    LoadRetry {
        not_before: i64,
    },
    /// Back to `queued` with no counter change (saturated connection, source missing or paused).
    Requeue {
        not_before: i64,
    },
    /// Back to `queued`, with the attempt given back (a pause after `start_candidate_attempt`).
    PauseRequeue {
        not_before: i64,
    },
    Failed {
        class: &'static str,
    },
    Skipped {
        class: &'static str,
    },
}

/// Inserts a candidate unless its `dedup_key` already exists. `true` when inserted.
///
/// # Errors
///
/// Returns [`MetaError`] when the insert fails, for example when the source does not exist.
pub(crate) async fn insert_candidate(
    conn: &mut SqliteConnection,
    row: &CandidateRow,
) -> Result<bool, MetaError> {
    let result = sqlx::query(
        "INSERT INTO issue_candidates (id, project_id, source_id, target, trace_id, span_id, \
         session_id, user_id, trace_start, score_id, dedup_key, status, attempts, claims, \
         interruptions, load_retries, not_before, error_class, issue_id, summary, \
         summary_prompt_version, vector, embed_model, dimension, call_trace_id, cost_usd, \
         created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(dedup_key) DO NOTHING",
    )
    .bind(row.id.to_string())
    .bind(row.project_id.to_string())
    .bind(row.source_id.to_string())
    .bind(row.target.as_str())
    .bind(row.trace_id.map(|id| id.as_bytes().to_vec()))
    .bind(row.span_id.map(|id| id.as_bytes().to_vec()))
    .bind(&row.session_id)
    .bind(&row.user_id)
    .bind(row.trace_start)
    .bind(row.score_id.map(|id| id.to_string()))
    .bind(&row.dedup_key)
    .bind(row.status.as_str())
    .bind(i64::from(row.attempts))
    .bind(i64::from(row.claims))
    .bind(i64::from(row.interruptions))
    .bind(i64::from(row.load_retries))
    .bind(row.not_before)
    .bind(&row.error_class)
    .bind(row.issue_id.map(|id| id.to_string()))
    .bind(&row.summary)
    .bind(row.summary_prompt_version.map(i64::from))
    .bind(row.vector.as_deref().map(encode_vector))
    .bind(&row.embed_model)
    .bind(row.dimension.map(i64::from))
    .bind(row.call_trace_id.map(|id| id.as_bytes().to_vec()))
    .bind(row.cost_usd)
    .bind(row.created_at)
    .bind(row.updated_at)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Adds one to the source's candidate count for `day` (UTC day start, nanoseconds) when that
/// count is below `cap`. `false` at the cap, when `cap` is 0, and when the source does not exist.
///
/// # Errors
///
/// Returns [`MetaError`] when the upsert fails.
pub(crate) async fn admit_daily(
    conn: &mut SqliteConnection,
    source: Uuid,
    day: i64,
    cap: u32,
) -> Result<bool, MetaError> {
    let result = sqlx::query(
        "INSERT INTO issue_source_days (source_id, day, candidates, spent_usd) \
         SELECT ?1, ?2, 1, 0 WHERE ?3 > 0 AND EXISTS (SELECT 1 FROM issue_sources WHERE id = ?1) \
         ON CONFLICT(source_id, day) DO UPDATE SET candidates = candidates + 1 \
         WHERE candidates < ?3",
    )
    .bind(source.to_string())
    .bind(day)
    .bind(i64::from(cap))
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Adds `usd` to the source's spend on `day` (UTC day start, nanoseconds). A non-finite or
/// negative amount adds 0. A source that no longer exists inserts nothing, so a finish that
/// lands after a deletion does not fail (as for rule spend).
///
/// # Errors
///
/// Returns [`MetaError`] when the upsert fails.
pub(crate) async fn add_spend(
    conn: &mut SqliteConnection,
    source: Uuid,
    day: i64,
    usd: f64,
) -> Result<(), MetaError> {
    let usd = if usd.is_finite() && usd > 0.0 {
        usd
    } else {
        0.0
    };
    sqlx::query(
        "INSERT INTO issue_source_days (source_id, day, candidates, spent_usd) \
         SELECT ?, ?, 0, ? WHERE EXISTS (SELECT 1 FROM issue_sources WHERE id = ?) \
         ON CONFLICT(source_id, day) DO UPDATE SET spent_usd = spent_usd + excluded.spent_usd",
    )
    .bind(source.to_string())
    .bind(day)
    .bind(usd)
    .bind(source.to_string())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The source's spend on the days `from_day <= day < to_day`, in USD.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails.
pub(crate) async fn spent_between(
    pool: &SqlitePool,
    source: Uuid,
    from_day: i64,
    to_day: i64,
) -> Result<f64, MetaError> {
    let total: f64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(spent_usd), 0.0) FROM issue_source_days \
         WHERE source_id = ? AND day >= ? AND day < ?",
    )
    .bind(source.to_string())
    .bind(from_day)
    .bind(to_day)
    .fetch_one(pool)
    .await?;
    Ok(total)
}

/// Queued candidates ready at `now` (`not_before <= now`), oldest `not_before` first, of enabled
/// unpaused sources of active projects in active orgs. A source whose chat or embedding
/// connection is in `saturated` is skipped. At most `limit` rows.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn ready_candidates(
    pool: &SqlitePool,
    now: i64,
    saturated: &[Uuid],
    limit: u32,
) -> Result<Vec<CandidateRow>, MetaError> {
    let saturated: Vec<String> = saturated.iter().map(Uuid::to_string).collect();
    // A list of strings always serializes; the fallback is the empty list.
    let saturated = serde_json::to_string(&saturated).unwrap_or_else(|_| "[]".to_owned());
    let records: Vec<CandidateRecord> = sqlx::query_as(
        "SELECT c.* FROM issue_candidates c \
         JOIN issue_sources s ON s.id = c.source_id \
         JOIN projects p ON p.id = s.project_id \
         JOIN orgs o ON o.id = p.org_id \
         WHERE c.status = 'queued' AND c.not_before <= ?1 \
         AND s.enabled = 1 AND s.paused_reason IS NULL \
         AND p.state = 'active' AND o.state = 'active' \
         AND (s.chat_connection_id IS NULL \
              OR s.chat_connection_id NOT IN (SELECT value FROM json_each(?2))) \
         AND (s.embed_connection_id IS NULL \
              OR s.embed_connection_id NOT IN (SELECT value FROM json_each(?2))) \
         ORDER BY c.not_before, c.id LIMIT ?3",
    )
    .bind(now)
    .bind(saturated)
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await?;
    records.into_iter().map(candidate_from).collect()
}

/// Claims a queued candidate: `queued → running`, `claims += 1`. Returns the new claim count,
/// or `None` when the candidate is not queued.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn claim_candidate(
    conn: &mut SqliteConnection,
    id: Uuid,
    now: i64,
) -> Result<Option<u32>, MetaError> {
    let claims: Option<i64> = sqlx::query_scalar(
        "UPDATE issue_candidates SET status = 'running', claims = claims + 1, updated_at = ? \
         WHERE id = ? AND status = 'queued' RETURNING claims",
    )
    .bind(now)
    .bind(id.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    claims
        .map(|value| u32_from(value, CANDIDATES, "claims"))
        .transpose()
}

/// Counts an attempt before a call: `attempts += 1`, for the running claim `claims`. Returns the
/// new attempt count, or `None` when the claim is stale.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn start_candidate_attempt(
    conn: &mut SqliteConnection,
    id: Uuid,
    claims: u32,
    now: i64,
) -> Result<Option<u32>, MetaError> {
    let attempts: Option<i64> = sqlx::query_scalar(
        "UPDATE issue_candidates SET attempts = attempts + 1, updated_at = ? \
         WHERE id = ? AND status = 'running' AND claims = ? RETURNING attempts",
    )
    .bind(now)
    .bind(id.to_string())
    .bind(i64::from(claims))
    .fetch_optional(&mut *conn)
    .await?;
    attempts
        .map(|value| u32_from(value, CANDIDATES, "attempts"))
        .transpose()
}

/// Leaves a running candidate as `finish` says, fenced by `status = 'running' AND claims`.
/// Adds `spent_usd` (a non-finite or negative amount adds 0) to its cost in every case. `false`
/// means the fence failed and nothing changed on the candidate.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn finish_candidate(
    conn: &mut SqliteConnection,
    id: Uuid,
    claims: u32,
    finish: CandidateFinish,
    spent_usd: f64,
    now: i64,
) -> Result<bool, MetaError> {
    let spent = if spent_usd.is_finite() && spent_usd > 0.0 {
        spent_usd
    } else {
        0.0
    };
    let id = id.to_string();
    let claims = i64::from(claims);
    let result =
        match finish {
            CandidateFinish::Summarized {
                summary,
                prompt_version,
                call_trace_id,
            } => {
                sqlx::query(
                    "UPDATE issue_candidates SET summary = ?, summary_prompt_version = ?, \
                 call_trace_id = ?, error_class = NULL, cost_usd = cost_usd + ?, updated_at = ? \
                 WHERE id = ? AND status = 'running' AND claims = ?",
                )
                .bind(summary)
                .bind(i64::from(prompt_version))
                .bind(call_trace_id.as_bytes().to_vec())
                .bind(spent)
                .bind(now)
                .bind(id)
                .bind(claims)
                .execute(&mut *conn)
                .await?
            }
            CandidateFinish::Pending {
                vector,
                embed_model,
                dimension,
            } => {
                sqlx::query(
                    "UPDATE issue_candidates SET status = 'pending', vector = ?, embed_model = ?, \
                 dimension = ?, error_class = NULL, cost_usd = cost_usd + ?, updated_at = ? \
                 WHERE id = ? AND status = 'running' AND claims = ?",
                )
                .bind(encode_vector(&vector))
                .bind(embed_model)
                .bind(i64::from(dimension))
                .bind(spent)
                .bind(now)
                .bind(id)
                .bind(claims)
                .execute(&mut *conn)
                .await?
            }
            CandidateFinish::Assigned {
                issue_id,
                vector,
                embed_model,
                dimension,
            } => {
                sqlx::query(
                    "UPDATE issue_candidates SET status = 'assigned', issue_id = ?, vector = ?, \
                 embed_model = ?, dimension = ?, error_class = NULL, cost_usd = cost_usd + ?, \
                 updated_at = ? WHERE id = ? AND status = 'running' AND claims = ?",
                )
                .bind(issue_id.to_string())
                .bind(encode_vector(&vector))
                .bind(embed_model)
                .bind(i64::from(dimension))
                .bind(spent)
                .bind(now)
                .bind(id)
                .bind(claims)
                .execute(&mut *conn)
                .await?
            }
            CandidateFinish::Dropped => {
                sqlx::query(
                    "UPDATE issue_candidates SET status = 'dropped', cost_usd = cost_usd + ?, \
                 updated_at = ? WHERE id = ? AND status = 'running' AND claims = ?",
                )
                .bind(spent)
                .bind(now)
                .bind(id)
                .bind(claims)
                .execute(&mut *conn)
                .await?
            }
            CandidateFinish::Retry { not_before } | CandidateFinish::Requeue { not_before } => {
                sqlx::query(
                    "UPDATE issue_candidates SET status = 'queued', not_before = ?, \
                 cost_usd = cost_usd + ?, updated_at = ? \
                 WHERE id = ? AND status = 'running' AND claims = ?",
                )
                .bind(not_before)
                .bind(spent)
                .bind(now)
                .bind(id)
                .bind(claims)
                .execute(&mut *conn)
                .await?
            }
            CandidateFinish::LoadRetry { not_before } => sqlx::query(
                "UPDATE issue_candidates SET status = 'queued', load_retries = load_retries + 1, \
                 not_before = ?, cost_usd = cost_usd + ?, updated_at = ? \
                 WHERE id = ? AND status = 'running' AND claims = ?",
            )
            .bind(not_before)
            .bind(spent)
            .bind(now)
            .bind(id)
            .bind(claims)
            .execute(&mut *conn)
            .await?,
            CandidateFinish::PauseRequeue { not_before } => {
                sqlx::query(
                    "UPDATE issue_candidates SET status = 'queued', not_before = ?, \
                 attempts = MAX(attempts - 1, 0), cost_usd = cost_usd + ?, updated_at = ? \
                 WHERE id = ? AND status = 'running' AND claims = ?",
                )
                .bind(not_before)
                .bind(spent)
                .bind(now)
                .bind(id)
                .bind(claims)
                .execute(&mut *conn)
                .await?
            }
            CandidateFinish::Failed { class } => {
                sqlx::query(
                    "UPDATE issue_candidates SET status = 'failed', error_class = ?, \
                 cost_usd = cost_usd + ?, updated_at = ? \
                 WHERE id = ? AND status = 'running' AND claims = ?",
                )
                .bind(class)
                .bind(spent)
                .bind(now)
                .bind(id)
                .bind(claims)
                .execute(&mut *conn)
                .await?
            }
            CandidateFinish::Skipped { class } => {
                sqlx::query(
                    "UPDATE issue_candidates SET status = 'skipped', error_class = ?, \
                 cost_usd = cost_usd + ?, updated_at = ? \
                 WHERE id = ? AND status = 'running' AND claims = ?",
                )
                .bind(class)
                .bind(spent)
                .bind(now)
                .bind(id)
                .bind(claims)
                .execute(&mut *conn)
                .await?
            }
        };
    Ok(result.rows_affected() > 0)
}

/// Sets the target of a running candidate after its trace loads (A4), fenced by
/// `status = 'running' AND claims`. The trace and its start are set; a user or session already on
/// the candidate is kept. `false` means the fence failed.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
#[allow(clippy::too_many_arguments)] // the candidate, its claim, the resolved trace and its owners, and the time
pub(crate) async fn set_candidate_target(
    conn: &mut SqliteConnection,
    id: Uuid,
    claims: u32,
    trace_id: TraceId,
    trace_start: i64,
    user_id: Option<&str>,
    session_id: Option<&str>,
    now: i64,
) -> Result<bool, MetaError> {
    let result = sqlx::query(
        "UPDATE issue_candidates SET trace_id = ?, trace_start = ?, \
         user_id = COALESCE(user_id, ?), session_id = COALESCE(session_id, ?), updated_at = ? \
         WHERE id = ? AND status = 'running' AND claims = ?",
    )
    .bind(trace_id.as_bytes().to_vec())
    .bind(trace_start)
    .bind(user_id)
    .bind(session_id)
    .bind(now)
    .bind(id.to_string())
    .bind(i64::from(claims))
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// The interruption rule for candidates (D-I-12), for startup and the janitor: a `running`
/// candidate not updated since `older_than` fails with class `interrupted` when it has been
/// interrupted [`MAX_CANDIDATE_INTERRUPTIONS`] times, and goes back to `queued` with one more
/// interruption otherwise. Returns how many rows changed.
///
/// # Errors
///
/// Returns [`MetaError`] when an update fails.
pub(crate) async fn interrupt_candidates(
    conn: &mut SqliteConnection,
    older_than: i64,
    now: i64,
) -> Result<u64, MetaError> {
    let failed = sqlx::query(
        "UPDATE issue_candidates SET status = 'failed', error_class = 'interrupted', \
         updated_at = ?1 \
         WHERE status = 'running' AND updated_at < ?2 AND interruptions >= ?3",
    )
    .bind(now)
    .bind(older_than)
    .bind(MAX_CANDIDATE_INTERRUPTIONS)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let requeued = sqlx::query(
        "UPDATE issue_candidates SET status = 'queued', not_before = ?1, \
         interruptions = interruptions + 1, updated_at = ?1 \
         WHERE status = 'running' AND updated_at < ?2",
    )
    .bind(now)
    .bind(older_than)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(failed + requeued)
}

/// The centroids of a source's issues that are not merged: `(id, state, centroid,
/// member_count)`, ordered by id. Issues without a centroid are left out.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn source_centroids(
    conn: &mut SqliteConnection,
    source: Uuid,
) -> Result<Vec<(Uuid, IssueState, Vec<f32>, u64)>, MetaError> {
    let records: Vec<(String, String, Vec<u8>, i64)> = sqlx::query_as(
        "SELECT id, state, centroid, member_count FROM issues \
         WHERE source_id = ? AND state <> 'merged' AND centroid IS NOT NULL ORDER BY id",
    )
    .bind(source.to_string())
    .fetch_all(&mut *conn)
    .await?;
    records
        .into_iter()
        .map(|(id, state, centroid, member_count)| {
            Ok((
                parse_uuid(&id, ISSUES, "id")?,
                IssueState::from_db(&state).ok_or_else(|| corrupt(ISSUES, "state"))?,
                decode_vector(&centroid, ISSUES, "centroid")?,
                u64::try_from(member_count).map_err(|_| corrupt(ISSUES, "member_count"))?,
            ))
        })
        .collect()
}

/// The pending candidates of `source` created at or after `since`, newest first (ties by id
/// descending), at most `limit`.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn pending_candidates(
    pool: &SqlitePool,
    source: Uuid,
    since: i64,
    limit: u32,
) -> Result<Vec<CandidateRow>, MetaError> {
    let records: Vec<CandidateRecord> = sqlx::query_as(
        "SELECT * FROM issue_candidates \
         WHERE source_id = ? AND status = 'pending' AND created_at >= ? \
         ORDER BY created_at DESC, id DESC LIMIT ?",
    )
    .bind(source.to_string())
    .bind(since)
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await?;
    records.into_iter().map(candidate_from).collect()
}

/// Marks each of `ids` that is still `pending` as joined to `issue`. Returns the ids that were
/// assigned, in the order given: a candidate deleted or assigned since it was read is skipped.
///
/// # Errors
///
/// Returns [`MetaError`] when an update fails.
pub(crate) async fn set_candidates_assigned(
    conn: &mut SqliteConnection,
    ids: &[Uuid],
    issue: Uuid,
    now: i64,
) -> Result<Vec<Uuid>, MetaError> {
    let mut assigned = Vec::with_capacity(ids.len());
    for id in ids {
        let result = sqlx::query(
            "UPDATE issue_candidates SET status = 'assigned', issue_id = ?, updated_at = ? \
             WHERE id = ? AND status = 'pending'",
        )
        .bind(issue.to_string())
        .bind(now)
        .bind(id.to_string())
        .execute(&mut *conn)
        .await?;
        if result.rows_affected() > 0 {
            assigned.push(*id);
        }
    }
    Ok(assigned)
}

/// Pending candidates of `source` created before `before` become outliers. Returns how many.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn mark_outliers(
    conn: &mut SqliteConnection,
    source: Uuid,
    before: i64,
    now: i64,
) -> Result<u64, MetaError> {
    let result = sqlx::query(
        "UPDATE issue_candidates SET status = 'outlier', updated_at = ? \
         WHERE source_id = ? AND status = 'pending' AND created_at < ?",
    )
    .bind(now)
    .bind(source.to_string())
    .bind(before)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected())
}

/// Sets the source's embedding dimension when it is NULL (D-IC-5), and returns the stored
/// dimension: the one just set, or the one set before.
///
/// # Errors
///
/// Returns [`MetaError`] when the source does not exist or the query fails.
pub(crate) async fn set_embed_dimension(
    conn: &mut SqliteConnection,
    source: Uuid,
    dimension: u32,
) -> Result<u32, MetaError> {
    sqlx::query(
        "UPDATE issue_sources SET embed_dimension = ? WHERE id = ? AND embed_dimension IS NULL",
    )
    .bind(i64::from(dimension))
    .bind(source.to_string())
    .execute(&mut *conn)
    .await?;
    let stored: Option<i64> =
        sqlx::query_scalar("SELECT embed_dimension FROM issue_sources WHERE id = ?")
            .bind(source.to_string())
            .fetch_one(&mut *conn)
            .await?;
    let stored = stored.ok_or_else(|| corrupt(SOURCES, "embed_dimension"))?;
    u32_from(stored, SOURCES, "embed_dimension")
}

// spend and refits

/// One stored refit of an LLM source (D-IC-8). Times are nanoseconds.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RefitRow {
    pub(crate) id: Uuid,
    pub(crate) source_id: Uuid,
    /// `schedule` or `manual`.
    pub(crate) trigger: &'static str,
    pub(crate) started_at: i64,
    pub(crate) finished_at: Option<i64>,
    pub(crate) pending: u32,
    pub(crate) clusters: u32,
    pub(crate) created: u32,
    pub(crate) joined: u32,
    pub(crate) outliers: u32,
    pub(crate) cost_usd: f64,
    pub(crate) error_class: Option<String>,
}

/// Inserts a refit row.
///
/// # Errors
///
/// Returns [`MetaError`] when the insert fails.
pub(crate) async fn insert_refit(
    conn: &mut SqliteConnection,
    row: &RefitRow,
) -> Result<(), MetaError> {
    sqlx::query(
        "INSERT INTO issue_refits (id, source_id, trigger, started_at, finished_at, pending, \
         clusters, created, joined, outliers, cost_usd, error_class) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(row.id.to_string())
    .bind(row.source_id.to_string())
    .bind(row.trigger)
    .bind(row.started_at)
    .bind(row.finished_at)
    .bind(i64::from(row.pending))
    .bind(i64::from(row.clusters))
    .bind(i64::from(row.created))
    .bind(i64::from(row.joined))
    .bind(i64::from(row.outliers))
    .bind(row.cost_usd)
    .bind(&row.error_class)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Writes the counters, cost, finish time and error class of the refit in `row`.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn finish_refit(
    conn: &mut SqliteConnection,
    row: &RefitRow,
) -> Result<(), MetaError> {
    sqlx::query(
        "UPDATE issue_refits SET finished_at = ?, pending = ?, clusters = ?, created = ?, \
         joined = ?, outliers = ?, cost_usd = ?, error_class = ? WHERE id = ?",
    )
    .bind(row.finished_at)
    .bind(i64::from(row.pending))
    .bind(i64::from(row.clusters))
    .bind(i64::from(row.created))
    .bind(i64::from(row.joined))
    .bind(i64::from(row.outliers))
    .bind(row.cost_usd)
    .bind(&row.error_class)
    .bind(row.id.to_string())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The refits of `source`, newest first (ties by id descending), at most `limit`.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn list_refits(
    pool: &SqlitePool,
    source: Uuid,
    limit: u32,
) -> Result<Vec<RefitRow>, MetaError> {
    let records: Vec<RefitRecord> = sqlx::query_as(
        "SELECT id, source_id, trigger, started_at, finished_at, pending, clusters, created, \
         joined, outliers, cost_usd, error_class FROM issue_refits WHERE source_id = ? \
         ORDER BY started_at DESC, id DESC LIMIT ?",
    )
    .bind(source.to_string())
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await?;
    records.into_iter().map(refit_from).collect()
}

/// When the source's latest refit started (nanoseconds), or `None` when it has none.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails.
pub(crate) async fn last_refit_started(
    pool: &SqlitePool,
    source: Uuid,
) -> Result<Option<i64>, MetaError> {
    let started: Option<i64> =
        sqlx::query_scalar("SELECT MAX(started_at) FROM issue_refits WHERE source_id = ?")
            .bind(source.to_string())
            .fetch_one(pool)
            .await?;
    Ok(started)
}

/// Enabled, unpaused LLM sources of active projects in active orgs whose `next_refit_at` is at
/// or before `now`, ordered by `next_refit_at`, then id.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn due_refits(pool: &SqlitePool, now: i64) -> Result<Vec<SourceRow>, MetaError> {
    let records: Vec<SourceRecord> = sqlx::query_as(
        "SELECT s.* FROM issue_sources s \
         JOIN projects p ON p.id = s.project_id \
         JOIN orgs o ON o.id = p.org_id \
         WHERE s.enabled = 1 AND s.paused_reason IS NULL \
         AND s.kind IN ('score', 'feedback', 'topic') \
         AND p.state = 'active' AND o.state = 'active' \
         AND s.next_refit_at <= ? ORDER BY s.next_refit_at, s.id",
    )
    .bind(now)
    .fetch_all(pool)
    .await?;
    records.into_iter().map(source_from).collect()
}

/// Sets the source's next scheduled refit (nanoseconds).
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn set_next_refit(
    conn: &mut SqliteConnection,
    source: Uuid,
    at: i64,
) -> Result<(), MetaError> {
    sqlx::query("UPDATE issue_sources SET next_refit_at = ? WHERE id = ?")
        .bind(at)
        .bind(source.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Closes the refits that never finished (startup, D-I-12): `finished_at = now` and error class
/// `interrupted`. Returns how many.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn close_open_refits(
    conn: &mut SqliteConnection,
    now: i64,
) -> Result<u64, MetaError> {
    let result = sqlx::query(
        "UPDATE issue_refits SET finished_at = ?, error_class = 'interrupted' \
         WHERE finished_at IS NULL",
    )
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected())
}

/// Enabled, unpaused LLM sources (`score`, `feedback`, `topic`) of active projects in active
/// orgs, with the project's org and its completion timeout, for the triggers.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn llm_sources(
    pool: &SqlitePool,
) -> Result<Vec<(SourceRow, OrgId, Option<i64>)>, MetaError> {
    let records: Vec<LlmSourceRecord> = sqlx::query_as(
        "SELECT s.*, p.org_id, \
         CAST(CASE WHEN json_valid(p.settings) AND json_type(p.settings) = 'object' \
              THEN json_extract(p.settings, '$.trace_completion_timeout_s') END AS INTEGER) \
           AS completion_timeout_s \
         FROM issue_sources s \
         JOIN projects p ON p.id = s.project_id \
         JOIN orgs o ON o.id = p.org_id \
         WHERE s.enabled = 1 AND s.paused_reason IS NULL \
         AND s.kind IN ('score', 'feedback', 'topic') \
         AND p.state = 'active' AND o.state = 'active' \
         ORDER BY s.project_id, s.id",
    )
    .fetch_all(pool)
    .await?;
    records
        .into_iter()
        .map(|record| {
            Ok((
                source_from(record.source)?,
                OrgId::from_uuid(parse_uuid(&record.org_id, SOURCES, "org_id")?),
                record.completion_timeout_s,
            ))
        })
        .collect()
}

/// Pauses an enabled, unpaused source with `reason` (`connection`, `budget` or `errors`) and
/// `detail` as its `paused_detail`. A source that is already paused keeps its first pause.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn pause_source(
    conn: &mut SqliteConnection,
    id: Uuid,
    reason: &str,
    detail: Option<&str>,
    now: i64,
) -> Result<(), MetaError> {
    sqlx::query(
        "UPDATE issue_sources SET paused_reason = ?1, paused_at = ?2, paused_detail = ?3, \
         updated_at = ?2 WHERE id = ?4 AND enabled = 1 AND paused_reason IS NULL",
    )
    .bind(reason)
    .bind(now)
    .bind(detail)
    .bind(id.to_string())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Pauses the enabled, unpaused sources whose chat or embedding connection is `connection`, with
/// reason `connection` and `detail`. Returns their ids.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn pause_sources_of_connection(
    conn: &mut SqliteConnection,
    connection: Uuid,
    detail: Option<&str>,
    now: i64,
) -> Result<Vec<Uuid>, MetaError> {
    let ids: Vec<String> = sqlx::query_scalar(
        "UPDATE issue_sources SET paused_reason = 'connection', paused_at = ?1, paused_detail = ?2, \
         updated_at = ?1 WHERE enabled = 1 AND paused_reason IS NULL \
         AND (chat_connection_id = ?3 OR embed_connection_id = ?3) RETURNING id",
    )
    .bind(now)
    .bind(detail)
    .bind(connection.to_string())
    .fetch_all(&mut *conn)
    .await?;
    source_ids(ids)
}

/// Clears the `connection` pauses of the sources whose chat or embedding connection is
/// `connection`. Returns their ids.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn resume_connection_pauses(
    conn: &mut SqliteConnection,
    connection: Uuid,
) -> Result<Vec<Uuid>, MetaError> {
    let ids: Vec<String> = sqlx::query_scalar(
        "UPDATE issue_sources SET paused_reason = NULL, paused_at = NULL, paused_detail = NULL \
         WHERE paused_reason = 'connection' AND (chat_connection_id = ?1 OR embed_connection_id = ?1) \
         RETURNING id",
    )
    .bind(connection.to_string())
    .fetch_all(&mut *conn)
    .await?;
    source_ids(ids)
}

/// Clears the `budget` pauses whose period has turned (as `online_rules::clear_budget_pauses`):
/// a `day` source paused before `day_start`, a `month` source paused before `month_start`, and any
/// source whose budget was removed. Returns how many sources were resumed.
///
/// # Errors
///
/// Returns [`MetaError`] when the update fails.
pub(crate) async fn clear_budget_pauses(
    conn: &mut SqliteConnection,
    day_start: i64,
    month_start: i64,
    now: i64,
) -> Result<u64, MetaError> {
    let result = sqlx::query(
        "UPDATE issue_sources SET paused_reason = NULL, paused_at = NULL, paused_detail = NULL, \
         updated_at = ?1 WHERE paused_reason = 'budget' AND \
         ((budget_period = 'day' AND paused_at < ?2) OR \
          (budget_period = 'month' AND paused_at < ?3) OR budget_usd IS NULL)",
    )
    .bind(now)
    .bind(day_start)
    .bind(month_start)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected())
}

/// Deletes the candidates of a project whose traces a deletion removed. Returns how many.
///
/// # Errors
///
/// Returns [`MetaError`] when the delete fails.
pub(crate) async fn delete_candidates_of_deletion(
    conn: &mut SqliteConnection,
    project: ProjectId,
    deletion: Uuid,
) -> Result<u64, MetaError> {
    let result = sqlx::query(
        "DELETE FROM issue_candidates WHERE project_id = ?1 AND trace_id IN \
         (SELECT trace_id FROM deletion_traces WHERE deletion_id = ?2)",
    )
    .bind(project.to_string())
    .bind(deletion.to_string())
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected())
}

/// A queued candidate expires after this long (A7).
const QUEUED_NS: i64 = DAY_NS;
/// A pending candidate loses its vector after this long (D-IC-6).
const PENDING_VECTOR_NS: i64 = 7 * DAY_NS;
/// An assigned candidate loses its vector, and every candidate is deleted, after this long
/// (D-IC-6).
const CANDIDATE_NS: i64 = 30 * DAY_NS;
/// The per-day counters of a source are deleted after this long (D-IC-6).
const DAY_ROW_NS: i64 = 62 * DAY_NS;
/// Refits kept per source (D-IC-6).
const REFITS_KEPT: i64 = 50;

/// Prunes the candidate data (D-IC-6, A7):
/// 1. a `queued` candidate created more than a day ago is `skipped` with `expired`;
/// 2. the vector of a `pending` candidate older than 7 days, and of an `assigned` one older than
///    30 days, is cleared;
/// 3. candidates older than 30 days are deleted;
/// 4. `issue_source_days` rows older than 62 days are deleted;
/// 5. each source keeps its newest 50 refits.
///
/// Returns how many candidates were expired or deleted.
///
/// # Errors
///
/// Returns [`MetaError`] when a statement fails.
pub(crate) async fn prune_candidates(
    conn: &mut SqliteConnection,
    now: i64,
) -> Result<u64, MetaError> {
    let expired = sqlx::query(
        "UPDATE issue_candidates SET status = 'skipped', error_class = 'expired', updated_at = ?1 \
         WHERE status = 'queued' AND created_at < ?2",
    )
    .bind(now)
    .bind(now.saturating_sub(QUEUED_NS))
    .execute(&mut *conn)
    .await?
    .rows_affected();
    sqlx::query(
        "UPDATE issue_candidates SET vector = NULL \
         WHERE vector IS NOT NULL AND ((status = 'pending' AND created_at < ?1) \
         OR (status = 'assigned' AND created_at < ?2))",
    )
    .bind(now.saturating_sub(PENDING_VECTOR_NS))
    .bind(now.saturating_sub(CANDIDATE_NS))
    .execute(&mut *conn)
    .await?;
    let deleted = sqlx::query("DELETE FROM issue_candidates WHERE created_at < ?")
        .bind(now.saturating_sub(CANDIDATE_NS))
        .execute(&mut *conn)
        .await?
        .rows_affected();
    sqlx::query("DELETE FROM issue_source_days WHERE day < ?")
        .bind(now.saturating_sub(DAY_ROW_NS))
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "DELETE FROM issue_refits WHERE id IN (\
         SELECT id FROM (SELECT id, ROW_NUMBER() OVER (PARTITION BY source_id \
         ORDER BY started_at DESC, id DESC) AS rn FROM issue_refits) WHERE rn > ?)",
    )
    .bind(REFITS_KEPT)
    .execute(&mut *conn)
    .await?;
    Ok(expired.saturating_add(deleted))
}

/// Deletes the candidates of the org's projects whose trace starts before `visible_from`
/// (the org's data window). Returns how many.
///
/// # Errors
///
/// Returns [`MetaError`] when the delete fails.
pub(crate) async fn prune_to_window(
    conn: &mut SqliteConnection,
    org: OrgId,
    visible_from: i64,
) -> Result<u64, MetaError> {
    let result = sqlx::query(
        "DELETE FROM issue_candidates WHERE trace_start < ?1 AND project_id IN \
         (SELECT id FROM projects WHERE org_id = ?2)",
    )
    .bind(visible_from)
    .bind(org.to_string())
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected())
}

/// Whether any of `ids` is still `pending`. A refit checks this inside its create write, so no
/// issue is written for members that a deletion or an earlier assignment removed.
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails.
pub(crate) async fn any_pending(
    conn: &mut SqliteConnection,
    ids: &[Uuid],
) -> Result<bool, MetaError> {
    for id in ids {
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM issue_candidates WHERE id = ? AND status = 'pending'",
        )
        .bind(id.to_string())
        .fetch_one(&mut *conn)
        .await?;
        if pending > 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Parses the ids a `RETURNING id` update gave back.
fn source_ids(ids: Vec<String>) -> Result<Vec<Uuid>, MetaError> {
    ids.iter().map(|id| parse_uuid(id, SOURCES, "id")).collect()
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
    sample_rate: Option<f64>,
    daily_cap: Option<i64>,
    score_names: Option<String>,
    feedback_sources: Option<String>,
    chat_connection_id: Option<String>,
    chat_model: Option<String>,
    embed_connection_id: Option<String>,
    embed_model: Option<String>,
    embed_dimension: Option<i64>,
    floor: Option<f64>,
    min_issue_size: Option<i64>,
    budget_usd: Option<f64>,
    budget_period: Option<String>,
    paused_detail: Option<String>,
    next_refit_at: Option<i64>,
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
        sample_rate: record.sample_rate,
        daily_cap: optional_u32(record.daily_cap, SOURCES, "daily_cap")?,
        score_names: list_from(record.score_names, "score_names")?,
        feedback_sources: list_from(record.feedback_sources, "feedback_sources")?,
        chat_connection_id: optional_uuid(
            record.chat_connection_id,
            SOURCES,
            "chat_connection_id",
        )?,
        chat_model: record.chat_model,
        embed_connection_id: optional_uuid(
            record.embed_connection_id,
            SOURCES,
            "embed_connection_id",
        )?,
        embed_model: record.embed_model,
        embed_dimension: optional_u32(record.embed_dimension, SOURCES, "embed_dimension")?,
        floor: record.floor,
        min_issue_size: optional_u32(record.min_issue_size, SOURCES, "min_issue_size")?,
        budget_usd: record.budget_usd,
        budget_period: record.budget_period,
        paused_detail: record.paused_detail,
        next_refit_at: record.next_refit_at,
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
    centroid: Option<Vec<u8>>,
    member_count: i64,
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
        centroid: record
            .centroid
            .as_deref()
            .map(|bytes| decode_vector(bytes, ISSUES, "centroid"))
            .transpose()?,
        member_count: u64::try_from(record.member_count)
            .map_err(|_| corrupt(ISSUES, "member_count"))?,
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

#[derive(sqlx::FromRow)]
struct CandidateRecord {
    id: String,
    project_id: String,
    source_id: String,
    target: String,
    trace_id: Option<Vec<u8>>,
    span_id: Option<Vec<u8>>,
    session_id: Option<String>,
    user_id: Option<String>,
    trace_start: Option<i64>,
    score_id: Option<String>,
    dedup_key: String,
    status: String,
    attempts: i64,
    claims: i64,
    interruptions: i64,
    load_retries: i64,
    not_before: i64,
    error_class: Option<String>,
    issue_id: Option<String>,
    summary: Option<String>,
    summary_prompt_version: Option<i64>,
    vector: Option<Vec<u8>>,
    embed_model: Option<String>,
    dimension: Option<i64>,
    call_trace_id: Option<Vec<u8>>,
    cost_usd: f64,
    created_at: i64,
    updated_at: i64,
}

fn candidate_from(record: CandidateRecord) -> Result<CandidateRow, MetaError> {
    Ok(CandidateRow {
        id: parse_uuid(&record.id, CANDIDATES, "id")?,
        project_id: ProjectId::from_uuid(parse_uuid(&record.project_id, CANDIDATES, "project_id")?),
        source_id: parse_uuid(&record.source_id, CANDIDATES, "source_id")?,
        target: CandidateTarget::from_db(&record.target)
            .ok_or_else(|| corrupt(CANDIDATES, "target"))?,
        trace_id: record
            .trace_id
            .as_deref()
            .map(|bytes| TraceId::from_slice(bytes).map_err(|_| corrupt(CANDIDATES, "trace_id")))
            .transpose()?,
        span_id: record
            .span_id
            .as_deref()
            .map(|bytes| SpanId::from_slice(bytes).map_err(|_| corrupt(CANDIDATES, "span_id")))
            .transpose()?,
        session_id: record.session_id,
        user_id: record.user_id,
        trace_start: record.trace_start,
        score_id: optional_uuid(record.score_id, CANDIDATES, "score_id")?,
        dedup_key: record.dedup_key,
        status: CandidateStatus::from_db(&record.status)
            .ok_or_else(|| corrupt(CANDIDATES, "status"))?,
        attempts: u32_from(record.attempts, CANDIDATES, "attempts")?,
        claims: u32_from(record.claims, CANDIDATES, "claims")?,
        interruptions: u32_from(record.interruptions, CANDIDATES, "interruptions")?,
        load_retries: u32_from(record.load_retries, CANDIDATES, "load_retries")?,
        not_before: record.not_before,
        error_class: record.error_class,
        issue_id: optional_uuid(record.issue_id, CANDIDATES, "issue_id")?,
        summary: record.summary,
        summary_prompt_version: optional_u32(
            record.summary_prompt_version,
            CANDIDATES,
            "summary_prompt_version",
        )?,
        vector: record
            .vector
            .as_deref()
            .map(|bytes| decode_vector(bytes, CANDIDATES, "vector"))
            .transpose()?,
        embed_model: record.embed_model,
        dimension: optional_u32(record.dimension, CANDIDATES, "dimension")?,
        call_trace_id: record
            .call_trace_id
            .as_deref()
            .map(|bytes| {
                TraceId::from_slice(bytes).map_err(|_| corrupt(CANDIDATES, "call_trace_id"))
            })
            .transpose()?,
        cost_usd: record.cost_usd,
        created_at: record.created_at,
        updated_at: record.updated_at,
    })
}

#[derive(sqlx::FromRow)]
struct RefitRecord {
    id: String,
    source_id: String,
    trigger: String,
    started_at: i64,
    finished_at: Option<i64>,
    pending: i64,
    clusters: i64,
    created: i64,
    joined: i64,
    outliers: i64,
    cost_usd: f64,
    error_class: Option<String>,
}

fn refit_from(record: RefitRecord) -> Result<RefitRow, MetaError> {
    let trigger = match record.trigger.as_str() {
        "schedule" => "schedule",
        "manual" => "manual",
        _ => return Err(corrupt(REFITS, "trigger")),
    };
    Ok(RefitRow {
        id: parse_uuid(&record.id, REFITS, "id")?,
        source_id: parse_uuid(&record.source_id, REFITS, "source_id")?,
        trigger,
        started_at: record.started_at,
        finished_at: record.finished_at,
        pending: u32_from(record.pending, REFITS, "pending")?,
        clusters: u32_from(record.clusters, REFITS, "clusters")?,
        created: u32_from(record.created, REFITS, "created")?,
        joined: u32_from(record.joined, REFITS, "joined")?,
        outliers: u32_from(record.outliers, REFITS, "outliers")?,
        cost_usd: record.cost_usd,
        error_class: record.error_class,
    })
}

/// An LLM source with the org of its project and the project's completion timeout.
#[derive(sqlx::FromRow)]
struct LlmSourceRecord {
    org_id: String,
    completion_timeout_s: Option<i64>,
    #[sqlx(flatten)]
    source: SourceRecord,
}

fn u32_from(value: i64, table: &'static str, column: &'static str) -> Result<u32, MetaError> {
    u32::try_from(value).map_err(|_| corrupt(table, column))
}

/// A vector as f32 little-endian bytes: the BLOB form of a centroid or a candidate vector.
pub(crate) fn encode_vector(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// The vector [`encode_vector`] wrote, read from `table.column` for the error. A length that is
/// not a multiple of four is corrupt.
///
/// # Errors
///
/// Returns [`MetaError::Corrupt`] naming `table` and `column` when the length is not a multiple
/// of four.
pub(crate) fn decode_vector(
    bytes: &[u8],
    table: &'static str,
    column: &'static str,
) -> Result<Vec<f32>, MetaError> {
    if !bytes.len().is_multiple_of(4) {
        return Err(corrupt(table, column));
    }
    bytes
        .chunks_exact(4)
        .map(|chunk| -> Result<f32, MetaError> {
            let array: [u8; 4] = chunk.try_into().map_err(|_| corrupt(table, column))?;
            Ok(f32::from_le_bytes(array))
        })
        .collect()
}

/// A list as a JSON array of strings, or NULL when it is empty.
fn list_json(values: &[String], column: &'static str) -> Result<Option<String>, MetaError> {
    if values.is_empty() {
        return Ok(None);
    }
    serde_json::to_string(values)
        .map(Some)
        .map_err(|_| corrupt(SOURCES, column))
}

/// The list [`list_json`] wrote. NULL reads as an empty list.
fn list_from(value: Option<String>, column: &'static str) -> Result<Vec<String>, MetaError> {
    match value {
        None => Ok(Vec::new()),
        Some(text) => serde_json::from_str(&text).map_err(|_| corrupt(SOURCES, column)),
    }
}

fn optional_u32(
    value: Option<i64>,
    table: &'static str,
    column: &'static str,
) -> Result<Option<u32>, MetaError> {
    value
        .map(|number| u32::try_from(number).map_err(|_| corrupt(table, column)))
        .transpose()
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
    use cetra_core::ids::{OrgId, ProjectId, SpanId, TraceId};
    use uuid::Uuid;

    use super::{
        CandidateFinish, CandidateRecord, CandidateRow, CandidateStatus, CandidateTarget,
        EventKind, EventRow, IssueListQuery, IssueRow, IssueSort, IssueState, RefitRow, SourceKind,
        SourceRow, add_spend, admit_daily, candidate_from, claim_candidate, close_open_refits,
        count_enabled_sources, decode_vector, delete_source, due_refits, encode_vector,
        finish_candidate, finish_refit, get_issue, get_issue_tx, get_source, insert_candidate,
        insert_event, insert_issue, insert_refit, insert_source, interrupt_candidates,
        issue_by_fingerprint, last_refit_started, list_events, list_issues, list_refits,
        list_sources, llm_sources, mark_outliers, merged_ids, open_counts, pending_candidates,
        ready_candidates, retarget_merged, set_candidate_target, set_candidates_assigned,
        set_embed_dimension, set_next_refit, source_centroids, spent_between,
        start_candidate_attempt, sweep_sources, update_issue, update_source,
    };
    use super::{
        any_pending, clear_budget_pauses, delete_candidates_of_deletion, pause_source,
        pause_sources_of_connection, prune_candidates, prune_to_window, resume_connection_pauses,
    };
    use crate::meta::deletions::{
        ActorKind, DeletionKind, DeletionState, NewDeletion, RequestDetails, insert_deletion,
        insert_traces,
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
            sample_rate: None,
            daily_cap: None,
            score_names: Vec::new(),
            feedback_sources: Vec::new(),
            chat_connection_id: None,
            chat_model: None,
            embed_connection_id: None,
            embed_model: None,
            embed_dimension: None,
            floor: None,
            min_issue_size: None,
            budget_usd: None,
            budget_period: None,
            paused_detail: None,
            next_refit_at: None,
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
            centroid: None,
            member_count: 0,
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

    /// Inserts an LLM connection of `org` with `id`, so a source can point at it.
    async fn insert_connection(
        conn: &mut sqlx::SqliteConnection,
        org: OrgId,
        id: Uuid,
    ) -> Result<(), MetaError> {
        sqlx::query(
            "INSERT INTO llm_connections (id, org_id, name, adapter, secret_enc, \
             api_key_hint, created_at, updated_at) \
             VALUES (?, ?, ?, 'openai', ?, 'hint', ?, ?)",
        )
        .bind(id.to_string())
        .bind(org.to_string())
        .bind(format!("connection {id}"))
        .bind(Vec::<u8>::new())
        .bind(NOW)
        .bind(NOW)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    async fn connection(db: &MetaDb, org: OrgId, id: Uuid) {
        db.write(move |conn| Box::pin(async move { insert_connection(conn, org, id).await }))
            .await
            .unwrap();
    }

    #[test]
    fn the_llm_kinds_are_named_and_only_they_are_llm() {
        for (kind, name, llm) in [
            (SourceKind::Error, "error", false),
            (SourceKind::Score, "score", true),
            (SourceKind::Feedback, "feedback", true),
            (SourceKind::Topic, "topic", true),
        ] {
            assert_eq!(kind.as_str(), name);
            assert_eq!(kind.is_llm(), llm);
            assert_eq!(SourceKind::from_db(name), Some(kind));
        }
        assert_eq!(SourceKind::from_db("unknown"), None);
    }

    #[tokio::test]
    async fn an_llm_source_round_trips_and_empty_lists_are_stored_as_null() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, projects) = seed(&db, &["support"]).await;
        let project = projects[0];
        let chat = Uuid::from_u128(11);
        let embed = Uuid::from_u128(12);
        connection(&db, org, chat).await;
        connection(&db, org, embed).await;
        let mut row = source(project, "scores", true);
        row.kind = SourceKind::Score;
        row.sample_rate = Some(0.5);
        row.daily_cap = Some(40);
        row.score_names = vec!["quality".to_owned(), "helpful".to_owned()];
        row.chat_connection_id = Some(chat);
        row.chat_model = Some("chat-model".to_owned());
        row.embed_connection_id = Some(embed);
        row.embed_model = Some("embed-model".to_owned());
        row.embed_dimension = Some(512);
        row.floor = Some(0.8);
        row.min_issue_size = Some(5);
        row.budget_usd = Some(12.5);
        row.budget_period = Some("month".to_owned());
        row.paused_reason = Some("connection".to_owned());
        row.paused_at = Some(NOW);
        row.paused_detail = Some("rate limited".to_owned());
        row.next_refit_at = Some(NOW + 1);
        put_source(&db, row.clone()).await;
        assert_eq!(
            get_source(db.reader(), project, row.id).await.unwrap(),
            Some(row.clone())
        );

        let plain = source(project, "plain", true);
        put_source(&db, plain.clone()).await;
        let stored: (Option<String>, Option<String>) =
            sqlx::query_as("SELECT score_names, feedback_sources FROM issue_sources WHERE id = ?")
                .bind(plain.id.to_string())
                .fetch_one(db.reader())
                .await
                .unwrap();
        assert_eq!(stored, (None, None));
        let read = get_source(db.reader(), project, plain.id)
            .await
            .unwrap()
            .unwrap();
        assert!(read.score_names.is_empty());
        assert!(read.feedback_sources.is_empty());
    }

    #[tokio::test]
    async fn update_source_changes_the_llm_settings_but_not_the_embedding() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, projects) = seed(&db, &["support"]).await;
        let project = projects[0];
        let chat = Uuid::from_u128(11);
        let embed = Uuid::from_u128(12);
        connection(&db, org, chat).await;
        connection(&db, org, embed).await;
        let mut row = source(project, "feedback", true);
        row.kind = SourceKind::Feedback;
        row.feedback_sources = vec!["end_user".to_owned()];
        row.chat_connection_id = Some(chat);
        row.chat_model = Some("chat-model".to_owned());
        row.embed_connection_id = Some(embed);
        row.embed_model = Some("embed-model".to_owned());
        row.embed_dimension = Some(512);
        put_source(&db, row.clone()).await;

        let mut changed = row.clone();
        changed.chat_model = Some("other-chat".to_owned());
        changed.feedback_sources = vec!["human".to_owned()];
        changed.daily_cap = Some(7);
        changed.embed_model = Some("other-embed".to_owned());
        changed.embed_dimension = Some(256);
        changed.embed_connection_id = None;
        changed.next_refit_at = Some(NOW + 5);
        changed.updated_at = 2_000;
        let writer = changed.clone();
        let updated = db
            .write(move |conn| Box::pin(async move { update_source(conn, &writer).await }))
            .await
            .unwrap();
        assert!(updated);

        let stored = get_source(db.reader(), project, row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.chat_model.as_deref(), Some("other-chat"));
        assert_eq!(stored.feedback_sources, vec!["human".to_owned()]);
        assert_eq!(stored.daily_cap, Some(7));
        assert_eq!(stored.next_refit_at, Some(NOW + 5));
        assert_eq!(stored.updated_at, 2_000);
        // The embedding fields change only through set_embed_dimension (D-IC-5).
        assert_eq!(stored.embed_connection_id, Some(embed));
        assert_eq!(stored.embed_model.as_deref(), Some("embed-model"));
        assert_eq!(stored.embed_dimension, Some(512));
    }

    #[tokio::test]
    async fn an_issue_centroid_and_member_count_round_trip() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let project = projects[0];
        let src = source(project, "scores", true);
        put_source(&db, src.clone()).await;
        let mut row = issue(1, src.id, project, "group", IssueState::Open, 10);
        row.centroid = Some(vec![0.5, -1.25, 3.0]);
        row.member_count = 7;
        put_issue(&db, row.clone()).await;
        assert_eq!(
            get_issue(db.reader(), project, row.id).await.unwrap(),
            Some(row.clone())
        );

        let mut changed = row.clone();
        changed.centroid = Some(vec![1.0, 2.0]);
        changed.member_count = 9;
        changed.updated_at = 2_000;
        let writer = changed.clone();
        db.write(move |conn| Box::pin(async move { update_issue(conn, &writer).await }))
            .await
            .unwrap();
        assert_eq!(
            get_issue(db.reader(), project, row.id).await.unwrap(),
            Some(changed)
        );
    }

    #[test]
    fn a_vector_round_trips_through_its_bytes_and_a_bad_length_is_corrupt() {
        let vector = vec![1.0_f32, -0.5, f32::MIN_POSITIVE, 0.0];
        let bytes = encode_vector(&vector);
        assert_eq!(bytes.len(), 16);
        assert_eq!(&bytes[..4], &1.0_f32.to_le_bytes()[..]);
        assert_eq!(decode_vector(&bytes, "issues", "centroid").unwrap(), vector);
        assert_eq!(
            decode_vector(&encode_vector(&[]), "issues", "centroid").unwrap(),
            Vec::<f32>::new()
        );
        let error = decode_vector(&bytes[..15], "issue_candidates", "vector").unwrap_err();
        assert!(
            matches!(
                error,
                MetaError::Corrupt {
                    table: "issue_candidates",
                    column: "vector"
                }
            ),
            "{error:?}"
        );
    }

    const DAY: i64 = 86_400_000_000_000;

    fn trace(byte: u8) -> TraceId {
        TraceId::from_slice(&[byte; 16]).unwrap()
    }

    /// A queued trace candidate of `source`, created at `created_at`.
    fn candidate(
        id: u128,
        source: &SourceRow,
        dedup: &str,
        status: CandidateStatus,
        created_at: i64,
    ) -> CandidateRow {
        CandidateRow {
            id: Uuid::from_u128(id),
            project_id: source.project_id,
            source_id: source.id,
            target: CandidateTarget::Trace,
            trace_id: Some(trace(7)),
            span_id: None,
            session_id: None,
            user_id: None,
            trace_start: Some(created_at),
            score_id: None,
            dedup_key: dedup.to_owned(),
            status,
            attempts: 0,
            claims: 0,
            interruptions: 0,
            load_retries: 0,
            not_before: created_at,
            error_class: None,
            issue_id: None,
            summary: None,
            summary_prompt_version: None,
            vector: None,
            embed_model: None,
            dimension: None,
            call_trace_id: None,
            cost_usd: 0.0,
            created_at,
            updated_at: created_at,
        }
    }

    async fn put_candidate(db: &MetaDb, row: CandidateRow) -> bool {
        db.write(move |conn| Box::pin(async move { insert_candidate(conn, &row).await }))
            .await
            .unwrap()
    }

    async fn read_candidate(db: &MetaDb, id: Uuid) -> CandidateRow {
        let record: CandidateRecord = sqlx::query_as("SELECT * FROM issue_candidates WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(db.reader())
            .await
            .unwrap();
        candidate_from(record).unwrap()
    }

    fn scored_source(project: ProjectId, name: &str, kind: SourceKind) -> SourceRow {
        let mut row = source(project, name, true);
        row.kind = kind;
        row
    }

    async fn admit(db: &MetaDb, source: Uuid, day: i64, cap: u32) -> bool {
        db.write(move |conn| Box::pin(async move { admit_daily(conn, source, day, cap).await }))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_candidate_round_trips_and_a_duplicate_dedup_key_is_ignored() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let project = projects[0];
        let src = scored_source(project, "scores", SourceKind::Score);
        put_source(&db, src.clone()).await;
        let mut row = candidate(1, &src, "k1", CandidateStatus::Pending, NOW);
        row.target = CandidateTarget::Span;
        row.span_id = Some(SpanId::from_slice(&[9; 8]).unwrap());
        row.session_id = Some("session-1".to_owned());
        row.user_id = Some("user-1".to_owned());
        row.score_id = Some(Uuid::from_u128(77));
        row.attempts = 2;
        row.claims = 3;
        row.interruptions = 1;
        row.load_retries = 1;
        row.not_before = NOW + 5;
        row.error_class = Some("retryable".to_owned());
        row.summary = Some("the summary".to_owned());
        row.summary_prompt_version = Some(4);
        row.vector = Some(vec![0.5, -1.0]);
        row.embed_model = Some("embed-model".to_owned());
        row.dimension = Some(2);
        row.call_trace_id = Some(trace(5));
        row.cost_usd = 0.25;
        row.updated_at = NOW + 1;
        assert!(put_candidate(&db, row.clone()).await);

        let listed = pending_candidates(db.reader(), src.id, 0, 10)
            .await
            .unwrap();
        assert_eq!(listed, vec![row.clone()]);

        let mut duplicate = candidate(2, &src, "k1", CandidateStatus::Queued, NOW);
        duplicate.updated_at = NOW;
        assert!(!put_candidate(&db, duplicate).await);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM issue_candidates").await, 1);
    }

    #[tokio::test]
    async fn admit_daily_stops_at_the_cap_and_starts_again_the_next_day() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let src = scored_source(projects[0], "scores", SourceKind::Score);
        put_source(&db, src.clone()).await;
        let day = 5 * DAY;

        assert!(admit(&db, src.id, day, 2).await);
        assert!(admit(&db, src.id, day, 2).await);
        assert!(!admit(&db, src.id, day, 2).await);
        assert!(!admit(&db, src.id, day, 2).await);
        assert!(admit(&db, src.id, day + DAY, 2).await);
        // A cap of 0 admits nothing, and a source that does not exist admits nothing.
        assert!(!admit(&db, src.id, day + 2 * DAY, 0).await);
        assert!(!admit(&db, Uuid::from_u128(999), day, 5).await);
    }

    #[tokio::test]
    async fn add_spend_accumulates_per_day_and_sums_a_range() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let src = scored_source(projects[0], "scores", SourceKind::Score);
        put_source(&db, src.clone()).await;
        let day = 5 * DAY;
        let id = src.id;
        db.write(move |conn| {
            Box::pin(async move {
                add_spend(conn, id, day, 0.25).await?;
                add_spend(conn, id, day, 0.5).await?;
                add_spend(conn, id, day + DAY, 1.0).await?;
                // A non-finite amount adds nothing.
                add_spend(conn, id, day + DAY, f64::NAN).await?;
                // A source that does not exist inserts nothing.
                add_spend(conn, Uuid::from_u128(999), day, 4.0).await
            })
        })
        .await
        .unwrap();

        assert_eq!(
            spent_between(db.reader(), src.id, day, day + DAY)
                .await
                .unwrap(),
            0.75
        );
        assert_eq!(
            spent_between(db.reader(), src.id, day, day + 2 * DAY)
                .await
                .unwrap(),
            1.75
        );
        assert_eq!(
            spent_between(db.reader(), Uuid::from_u128(999), day, day + DAY)
                .await
                .unwrap(),
            0.0
        );
    }

    #[tokio::test]
    async fn claim_start_and_finish_are_fenced_by_the_claim() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let src = scored_source(projects[0], "scores", SourceKind::Score);
        put_source(&db, src.clone()).await;
        let row = candidate(1, &src, "k1", CandidateStatus::Queued, NOW);
        put_candidate(&db, row.clone()).await;
        let id = row.id;

        let claimed = db
            .write(move |conn| Box::pin(async move { claim_candidate(conn, id, NOW).await }))
            .await
            .unwrap();
        assert_eq!(claimed, Some(1));
        let again = db
            .write(move |conn| Box::pin(async move { claim_candidate(conn, id, NOW).await }))
            .await
            .unwrap();
        assert_eq!(again, None);

        let attempts = db
            .write(move |conn| {
                Box::pin(async move { start_candidate_attempt(conn, id, 1, NOW).await })
            })
            .await
            .unwrap();
        assert_eq!(attempts, Some(1));
        let stale_attempt = db
            .write(move |conn| {
                Box::pin(async move { start_candidate_attempt(conn, id, 0, NOW).await })
            })
            .await
            .unwrap();
        assert_eq!(stale_attempt, None);

        let stale = db
            .write(move |conn| {
                Box::pin(async move {
                    finish_candidate(
                        conn,
                        id,
                        0,
                        CandidateFinish::Retry {
                            not_before: NOW + 10,
                        },
                        0.5,
                        NOW,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        assert!(!stale);
        let untouched = read_candidate(&db, id).await;
        assert_eq!(untouched.status, CandidateStatus::Running);
        assert_eq!(untouched.cost_usd, 0.0);

        let retried = db
            .write(move |conn| {
                Box::pin(async move {
                    finish_candidate(
                        conn,
                        id,
                        1,
                        CandidateFinish::Retry {
                            not_before: NOW + 10,
                        },
                        0.5,
                        NOW + 1,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        assert!(retried);
        let back = read_candidate(&db, id).await;
        assert_eq!(back.status, CandidateStatus::Queued);
        assert_eq!(back.not_before, NOW + 10);
        assert_eq!(back.attempts, 1);
        assert_eq!(back.cost_usd, 0.5);
        assert_eq!(back.updated_at, NOW + 1);
    }

    #[tokio::test]
    async fn finish_candidate_writes_each_variant() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let project = projects[0];
        let src = scored_source(project, "scores", SourceKind::Score);
        put_source(&db, src.clone()).await;
        put_issue(
            &db,
            issue(50, src.id, project, "group", IssueState::Open, NOW),
        )
        .await;
        // Each candidate is running with claim 1; `attempts` is 2 so a pause can give one back.
        for n in 1..=9_u128 {
            let mut row = candidate(n, &src, &format!("k{n}"), CandidateStatus::Running, NOW);
            row.claims = 1;
            row.attempts = 2;
            put_candidate(&db, row).await;
        }
        let summarized = Uuid::from_u128(1);
        let pending = Uuid::from_u128(2);
        let assigned = Uuid::from_u128(3);
        let dropped = Uuid::from_u128(4);
        let load_retry = Uuid::from_u128(5);
        let pause = Uuid::from_u128(6);
        let failed = Uuid::from_u128(7);
        let skipped = Uuid::from_u128(8);
        let requeued = Uuid::from_u128(9);

        let writes: Vec<(Uuid, CandidateFinish)> = vec![
            (
                summarized,
                CandidateFinish::Summarized {
                    summary: "a summary".to_owned(),
                    prompt_version: 3,
                    call_trace_id: trace(8),
                },
            ),
            (
                pending,
                CandidateFinish::Pending {
                    vector: vec![1.0, 2.0],
                    embed_model: "embed".to_owned(),
                    dimension: 2,
                },
            ),
            (
                assigned,
                CandidateFinish::Assigned {
                    issue_id: Uuid::from_u128(50),
                    vector: vec![3.0, 4.0],
                    embed_model: "embed".to_owned(),
                    dimension: 2,
                },
            ),
            (dropped, CandidateFinish::Dropped),
            (
                load_retry,
                CandidateFinish::LoadRetry {
                    not_before: NOW + 2,
                },
            ),
            (
                pause,
                CandidateFinish::PauseRequeue {
                    not_before: NOW + 3,
                },
            ),
            (
                failed,
                CandidateFinish::Failed {
                    class: "trace_not_found",
                },
            ),
            (skipped, CandidateFinish::Skipped { class: "budget" }),
            (
                requeued,
                CandidateFinish::Requeue {
                    not_before: NOW + 4,
                },
            ),
        ];
        for (id, finish) in writes {
            let done = db
                .write(move |conn| {
                    Box::pin(
                        async move { finish_candidate(conn, id, 1, finish, 0.1, NOW + 1).await },
                    )
                })
                .await
                .unwrap();
            assert!(done);
        }

        let summarized = read_candidate(&db, summarized).await;
        assert_eq!(summarized.status, CandidateStatus::Running);
        assert_eq!(summarized.summary.as_deref(), Some("a summary"));
        assert_eq!(summarized.summary_prompt_version, Some(3));
        assert_eq!(summarized.call_trace_id, Some(trace(8)));
        assert!((summarized.cost_usd - 0.1).abs() < 1e-12);

        let pending = read_candidate(&db, pending).await;
        assert_eq!(pending.status, CandidateStatus::Pending);
        assert_eq!(pending.vector, Some(vec![1.0, 2.0]));
        assert_eq!(pending.embed_model.as_deref(), Some("embed"));
        assert_eq!(pending.dimension, Some(2));

        let assigned = read_candidate(&db, assigned).await;
        assert_eq!(assigned.status, CandidateStatus::Assigned);
        assert_eq!(assigned.issue_id, Some(Uuid::from_u128(50)));
        assert_eq!(assigned.vector, Some(vec![3.0, 4.0]));

        assert_eq!(
            read_candidate(&db, dropped).await.status,
            CandidateStatus::Dropped
        );

        let load_retry = read_candidate(&db, load_retry).await;
        assert_eq!(load_retry.status, CandidateStatus::Queued);
        assert_eq!(load_retry.load_retries, 1);
        assert_eq!(load_retry.not_before, NOW + 2);

        let pause = read_candidate(&db, pause).await;
        assert_eq!(pause.status, CandidateStatus::Queued);
        assert_eq!(pause.attempts, 1);
        assert_eq!(pause.not_before, NOW + 3);

        let failed = read_candidate(&db, failed).await;
        assert_eq!(failed.status, CandidateStatus::Failed);
        assert_eq!(failed.error_class.as_deref(), Some("trace_not_found"));

        let skipped = read_candidate(&db, skipped).await;
        assert_eq!(skipped.status, CandidateStatus::Skipped);
        assert_eq!(skipped.error_class.as_deref(), Some("budget"));

        let requeued = read_candidate(&db, requeued).await;
        assert_eq!(requeued.status, CandidateStatus::Queued);
        assert_eq!(requeued.attempts, 2);
        assert_eq!(requeued.not_before, NOW + 4);
        assert_eq!(requeued.load_retries, 0);
    }

    #[tokio::test]
    async fn interrupt_candidates_requeues_then_fails_at_the_third_interruption() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let src = scored_source(projects[0], "scores", SourceKind::Score);
        put_source(&db, src.clone()).await;
        let mut once = candidate(1, &src, "k1", CandidateStatus::Running, NOW);
        once.claims = 1;
        let mut last = candidate(2, &src, "k2", CandidateStatus::Running, NOW);
        last.claims = 1;
        last.interruptions = 3;
        let mut fresh = candidate(3, &src, "k3", CandidateStatus::Running, NOW + 100);
        fresh.claims = 1;
        put_candidate(&db, once).await;
        put_candidate(&db, last).await;
        put_candidate(&db, fresh).await;

        // Rows not updated since `NOW + 50` are interrupted; `fresh` is left alone.
        let changed = db
            .write(move |conn| {
                Box::pin(async move { interrupt_candidates(conn, NOW + 50, NOW + 60).await })
            })
            .await
            .unwrap();
        assert_eq!(changed, 2);

        let requeued = read_candidate(&db, Uuid::from_u128(1)).await;
        assert_eq!(requeued.status, CandidateStatus::Queued);
        assert_eq!(requeued.interruptions, 1);
        assert_eq!(requeued.not_before, NOW + 60);
        let failed = read_candidate(&db, Uuid::from_u128(2)).await;
        assert_eq!(failed.status, CandidateStatus::Failed);
        assert_eq!(failed.error_class.as_deref(), Some("interrupted"));
        let untouched = read_candidate(&db, Uuid::from_u128(3)).await;
        assert_eq!(untouched.status, CandidateStatus::Running);
        assert_eq!(untouched.interruptions, 0);
    }

    #[tokio::test]
    async fn ready_candidates_skips_paused_saturated_and_future_candidates() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, projects) = seed(&db, &["support"]).await;
        let project = projects[0];
        let (chat, embed, saturated) = (
            Uuid::from_u128(11),
            Uuid::from_u128(12),
            Uuid::from_u128(13),
        );
        for id in [chat, embed, saturated] {
            connection(&db, org, id).await;
        }
        let mut ok = scored_source(project, "ok", SourceKind::Score);
        ok.chat_connection_id = Some(chat);
        ok.embed_connection_id = Some(embed);
        let mut paused = scored_source(project, "paused", SourceKind::Score);
        paused.paused_reason = Some("connection".to_owned());
        let mut chat_saturated = scored_source(project, "chat", SourceKind::Score);
        chat_saturated.chat_connection_id = Some(saturated);
        let mut embed_saturated = scored_source(project, "embed", SourceKind::Score);
        embed_saturated.embed_connection_id = Some(saturated);
        // A source without connections has NULL in both columns, which is not saturated.
        let bare = scored_source(project, "bare", SourceKind::Score);
        for row in [&ok, &paused, &chat_saturated, &embed_saturated, &bare] {
            put_source(&db, row.clone()).await;
        }

        let wanted = candidate(1, &ok, "k1", CandidateStatus::Queued, NOW);
        let mut future = candidate(2, &ok, "k2", CandidateStatus::Queued, NOW);
        future.not_before = NOW + 1_000;
        let blocked_paused = candidate(3, &paused, "k3", CandidateStatus::Queued, NOW);
        let blocked_chat = candidate(4, &chat_saturated, "k4", CandidateStatus::Queued, NOW);
        let blocked_embed = candidate(5, &embed_saturated, "k5", CandidateStatus::Queued, NOW);
        let mut bare_row = candidate(6, &bare, "k6", CandidateStatus::Queued, NOW);
        bare_row.not_before = NOW - 1;
        for row in [
            wanted,
            future,
            blocked_paused,
            blocked_chat,
            blocked_embed,
            bare_row,
        ] {
            put_candidate(&db, row).await;
        }

        let ids =
            |rows: &[CandidateRow]| rows.iter().map(|row| row.id.as_u128()).collect::<Vec<_>>();
        let ready = ready_candidates(db.reader(), NOW, &[saturated], 10)
            .await
            .unwrap();
        // Oldest `not_before` first.
        assert_eq!(ids(&ready), vec![6, 1]);
        let embed_busy = ready_candidates(db.reader(), NOW, &[embed], 10)
            .await
            .unwrap();
        assert_eq!(ids(&embed_busy), vec![6]);
        let limited = ready_candidates(db.reader(), NOW, &[], 1).await.unwrap();
        assert_eq!(ids(&limited), vec![6]);
    }

    #[tokio::test]
    async fn source_centroids_leaves_out_merged_issues_and_issues_without_a_centroid() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let project = projects[0];
        let src = scored_source(project, "scores", SourceKind::Score);
        put_source(&db, src.clone()).await;
        let mut open = issue(1, src.id, project, "open", IssueState::Open, NOW);
        open.centroid = Some(vec![1.0, 2.0]);
        open.member_count = 3;
        let mut merged = issue(2, src.id, project, "merged", IssueState::Merged, NOW);
        merged.merged_into = Some(Uuid::from_u128(1));
        merged.centroid = Some(vec![9.0, 9.0]);
        let mut resolved = issue(3, src.id, project, "resolved", IssueState::Resolved, NOW);
        resolved.centroid = Some(vec![3.0, 4.0]);
        resolved.member_count = 5;
        let no_centroid = issue(4, src.id, project, "none", IssueState::Open, NOW);
        for row in [open, merged, resolved, no_centroid] {
            put_issue(&db, row).await;
        }

        let centroids = db
            .write(move |conn| Box::pin(async move { source_centroids(conn, src.id).await }))
            .await
            .unwrap();
        assert_eq!(
            centroids,
            vec![
                (Uuid::from_u128(1), IssueState::Open, vec![1.0, 2.0], 3),
                (Uuid::from_u128(3), IssueState::Resolved, vec![3.0, 4.0], 5),
            ]
        );
    }

    #[tokio::test]
    async fn pending_candidates_is_newest_first_and_mark_outliers_takes_the_old_ones() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let src = scored_source(projects[0], "scores", SourceKind::Score);
        put_source(&db, src.clone()).await;
        let old = candidate(1, &src, "k1", CandidateStatus::Pending, NOW - 100);
        let middle = candidate(2, &src, "k2", CandidateStatus::Pending, NOW - 50);
        let newest = candidate(3, &src, "k3", CandidateStatus::Pending, NOW);
        let queued = candidate(4, &src, "k4", CandidateStatus::Queued, NOW - 10);
        for row in [old, middle, newest, queued] {
            put_candidate(&db, row).await;
        }

        let ids =
            |rows: &[CandidateRow]| rows.iter().map(|row| row.id.as_u128()).collect::<Vec<_>>();
        let all = pending_candidates(db.reader(), src.id, NOW - 100, 10)
            .await
            .unwrap();
        assert_eq!(ids(&all), vec![3, 2, 1]);
        let recent = pending_candidates(db.reader(), src.id, NOW - 60, 10)
            .await
            .unwrap();
        assert_eq!(ids(&recent), vec![3, 2]);
        let first = pending_candidates(db.reader(), src.id, NOW - 100, 1)
            .await
            .unwrap();
        assert_eq!(ids(&first), vec![3]);

        let marked = db
            .write(move |conn| {
                Box::pin(async move { mark_outliers(conn, src.id, NOW - 10, NOW + 1).await })
            })
            .await
            .unwrap();
        assert_eq!(marked, 2);
        assert_eq!(
            read_candidate(&db, Uuid::from_u128(1)).await.status,
            CandidateStatus::Outlier
        );
        assert_eq!(
            read_candidate(&db, Uuid::from_u128(2)).await.status,
            CandidateStatus::Outlier
        );
        assert_eq!(
            read_candidate(&db, Uuid::from_u128(3)).await.status,
            CandidateStatus::Pending
        );
        assert_eq!(
            read_candidate(&db, Uuid::from_u128(4)).await.status,
            CandidateStatus::Queued
        );
    }

    #[tokio::test]
    async fn set_candidates_assigned_returns_only_the_pending_rows_it_assigned() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let project = projects[0];
        let src = scored_source(project, "scores", SourceKind::Score);
        put_source(&db, src.clone()).await;
        put_issue(
            &db,
            issue(50, src.id, project, "group", IssueState::Open, NOW),
        )
        .await;
        put_candidate(&db, candidate(1, &src, "k1", CandidateStatus::Pending, NOW)).await;
        put_candidate(&db, candidate(2, &src, "k2", CandidateStatus::Queued, NOW)).await;
        put_candidate(&db, candidate(3, &src, "k3", CandidateStatus::Pending, NOW)).await;
        let issue_id = Uuid::from_u128(50);
        let ids = [Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)];

        let assigned = db
            .write(move |conn| {
                Box::pin(
                    async move { set_candidates_assigned(conn, &ids, issue_id, NOW + 1).await },
                )
            })
            .await
            .unwrap();
        assert_eq!(assigned, vec![Uuid::from_u128(1), Uuid::from_u128(3)]);
        let first = read_candidate(&db, Uuid::from_u128(1)).await;
        assert_eq!(first.status, CandidateStatus::Assigned);
        assert_eq!(first.issue_id, Some(issue_id));
        assert_eq!(
            read_candidate(&db, Uuid::from_u128(2)).await.status,
            CandidateStatus::Queued
        );

        // The fence: an assigned candidate is not assigned again.
        let again = db
            .write(move |conn| {
                Box::pin(async move {
                    set_candidates_assigned(conn, &[Uuid::from_u128(1)], issue_id, NOW + 2).await
                })
            })
            .await
            .unwrap();
        assert!(again.is_empty());
    }

    #[tokio::test]
    async fn set_candidate_target_sets_the_trace_and_keeps_an_owner_already_set() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let src = scored_source(projects[0], "scores", SourceKind::Score);
        put_source(&db, src.clone()).await;
        let mut row = candidate(1, &src, "session:s1", CandidateStatus::Running, NOW);
        row.target = CandidateTarget::Session;
        row.trace_id = None;
        row.trace_start = None;
        row.session_id = Some("s1".to_owned());
        row.claims = 1;
        let id = row.id;
        put_candidate(&db, row).await;

        let stale = db
            .write(move |conn| {
                Box::pin(async move {
                    set_candidate_target(
                        conn,
                        id,
                        0,
                        trace(3),
                        NOW + 3,
                        Some("user-2"),
                        Some("s2"),
                        NOW + 4,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        assert!(!stale);

        let set = db
            .write(move |conn| {
                Box::pin(async move {
                    set_candidate_target(
                        conn,
                        id,
                        1,
                        trace(3),
                        NOW + 3,
                        Some("user-2"),
                        Some("s2"),
                        NOW + 4,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        assert!(set);
        let stored = read_candidate(&db, id).await;
        assert_eq!(stored.trace_id, Some(trace(3)));
        assert_eq!(stored.trace_start, Some(NOW + 3));
        assert_eq!(stored.user_id.as_deref(), Some("user-2"));
        // The session was set before, so it is kept.
        assert_eq!(stored.session_id.as_deref(), Some("s1"));
    }

    #[tokio::test]
    async fn set_embed_dimension_sets_once_and_then_returns_the_stored_value() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let src = scored_source(projects[0], "scores", SourceKind::Score);
        put_source(&db, src.clone()).await;
        let id = src.id;

        let first = db
            .write(move |conn| Box::pin(async move { set_embed_dimension(conn, id, 512).await }))
            .await
            .unwrap();
        assert_eq!(first, 512);
        let second = db
            .write(move |conn| Box::pin(async move { set_embed_dimension(conn, id, 256).await }))
            .await
            .unwrap();
        assert_eq!(second, 512);
        let stored = get_source(db.reader(), projects[0], id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.embed_dimension, Some(512));

        let missing = db
            .write(move |conn| {
                Box::pin(async move { set_embed_dimension(conn, Uuid::from_u128(999), 8).await })
            })
            .await;
        assert!(missing.is_err());
    }

    fn refit(id: u128, source: Uuid, trigger: &'static str, started_at: i64) -> RefitRow {
        RefitRow {
            id: Uuid::from_u128(id),
            source_id: source,
            trigger,
            started_at,
            finished_at: None,
            pending: 0,
            clusters: 0,
            created: 0,
            joined: 0,
            outliers: 0,
            cost_usd: 0.0,
            error_class: None,
        }
    }

    #[tokio::test]
    async fn refits_round_trip_and_list_newest_first() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let src = scored_source(projects[0], "scores", SourceKind::Score);
        put_source(&db, src.clone()).await;
        assert_eq!(last_refit_started(db.reader(), src.id).await.unwrap(), None);

        let mut first = refit(1, src.id, "schedule", NOW);
        first.finished_at = Some(NOW + 1);
        first.pending = 4;
        first.clusters = 1;
        first.created = 1;
        first.joined = 2;
        first.outliers = 1;
        first.cost_usd = 0.5;
        let mut second = refit(2, src.id, "manual", NOW + 10);
        second.error_class = Some("busy".to_owned());
        for row in [first.clone(), second.clone()] {
            db.write(move |conn| Box::pin(async move { insert_refit(conn, &row).await }))
                .await
                .unwrap();
        }
        assert_eq!(
            list_refits(db.reader(), src.id, 10).await.unwrap(),
            vec![second.clone(), first.clone()]
        );
        assert_eq!(
            list_refits(db.reader(), src.id, 1).await.unwrap(),
            vec![second.clone()]
        );
        assert_eq!(
            last_refit_started(db.reader(), src.id).await.unwrap(),
            Some(NOW + 10)
        );

        let mut finished = second.clone();
        finished.finished_at = Some(NOW + 20);
        finished.error_class = None;
        finished.created = 3;
        finished.cost_usd = 1.0;
        db.write(move |conn| Box::pin(async move { finish_refit(conn, &finished).await }))
            .await
            .unwrap();
        let listed = list_refits(db.reader(), src.id, 10).await.unwrap();
        assert_eq!(listed[0].finished_at, Some(NOW + 20));
        assert_eq!(listed[0].error_class, None);
        assert_eq!(listed[0].created, 3);
        assert_eq!(listed[0].cost_usd, 1.0);
        assert_eq!(listed[1], first);
    }

    #[tokio::test]
    async fn close_open_refits_closes_only_the_refits_that_never_finished() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_, projects) = seed(&db, &["support"]).await;
        let src = scored_source(projects[0], "scores", SourceKind::Score);
        put_source(&db, src.clone()).await;
        let mut done = refit(1, src.id, "schedule", NOW);
        done.finished_at = Some(NOW + 1);
        let open = refit(2, src.id, "manual", NOW + 2);
        for row in [done.clone(), open.clone()] {
            db.write(move |conn| Box::pin(async move { insert_refit(conn, &row).await }))
                .await
                .unwrap();
        }

        let closed = db
            .write(move |conn| Box::pin(async move { close_open_refits(conn, NOW + 9).await }))
            .await
            .unwrap();
        assert_eq!(closed, 1);
        let listed = list_refits(db.reader(), src.id, 10).await.unwrap();
        assert_eq!(listed[0].finished_at, Some(NOW + 9));
        assert_eq!(listed[0].error_class.as_deref(), Some("interrupted"));
        assert_eq!(listed[1], done);
    }

    #[tokio::test]
    async fn due_refits_and_llm_sources_leave_out_the_wrong_kinds_pauses_and_inactive_projects() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, projects) = seed(&db, &["a", "b"]).await;
        let project = projects[0];
        let mut due = scored_source(project, "due", SourceKind::Topic);
        due.next_refit_at = Some(NOW);
        let mut later = scored_source(project, "later", SourceKind::Feedback);
        later.next_refit_at = Some(NOW + 1);
        let mut paused = scored_source(project, "paused", SourceKind::Score);
        paused.next_refit_at = Some(NOW);
        paused.paused_reason = Some("connection".to_owned());
        let mut error = scored_source(project, "errors", SourceKind::Error);
        error.next_refit_at = Some(NOW);
        let mut disabled = scored_source(project, "disabled", SourceKind::Score);
        disabled.next_refit_at = Some(NOW);
        disabled.enabled = false;
        let mut inactive = scored_source(projects[1], "inactive", SourceKind::Score);
        inactive.next_refit_at = Some(NOW);
        for row in [&due, &later, &paused, &error, &disabled, &inactive] {
            put_source(&db, row.clone()).await;
        }
        let deleting = projects[1];
        db.write(move |conn| {
            Box::pin(async move { mark_project_deleting(conn, deleting, NOW).await })
        })
        .await
        .unwrap();

        let due_now = due_refits(db.reader(), NOW).await.unwrap();
        assert_eq!(due_now, vec![due.clone()]);

        // A source moved to `now` by `set_next_refit` is due.
        let later_id = later.id;
        db.write(move |conn| Box::pin(async move { set_next_refit(conn, later_id, NOW).await }))
            .await
            .unwrap();
        let due_after = due_refits(db.reader(), NOW).await.unwrap();
        assert_eq!(due_after.len(), 2);

        let swept = llm_sources(db.reader()).await.unwrap();
        let mut ids: Vec<Uuid> = swept.iter().map(|(row, _, _)| row.id).collect();
        ids.sort();
        let mut expected = vec![due.id, later.id];
        expected.sort();
        assert_eq!(ids, expected);
        for (_, org_id, timeout) in &swept {
            assert_eq!(*org_id, org);
            assert_eq!(*timeout, None);
        }
    }

    /// The status text of candidate `id`.
    async fn status_text(db: &MetaDb, id: Uuid) -> String {
        sqlx::query_scalar("SELECT status FROM issue_candidates WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(db.reader())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_connection_pause_keeps_the_first_pause_and_resumes_with_its_connection() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, projects) = seed(&db, &["a"]).await;
        let project = projects[0];
        let chat = Uuid::from_u128(70);
        let embed = Uuid::from_u128(71);
        connection(&db, org, chat).await;
        connection(&db, org, embed).await;
        let mut on_chat = scored_source(project, "chat", SourceKind::Score);
        on_chat.chat_connection_id = Some(chat);
        let mut on_embed = scored_source(project, "embed", SourceKind::Score);
        on_embed.embed_connection_id = Some(embed);
        let other = scored_source(project, "other", SourceKind::Score);
        for row in [&on_chat, &on_embed, &other] {
            put_source(&db, row.clone()).await;
        }
        let (chat_source, chat_pause, embed_pause) = (on_chat.id, chat, embed);

        let paused = db
            .write(move |conn| {
                Box::pin(async move {
                    pause_sources_of_connection(conn, chat_pause, Some("bad key"), NOW).await
                })
            })
            .await
            .unwrap();
        assert_eq!(paused, vec![chat_source]);
        // A budget pause does not replace the connection pause.
        db.write(move |conn| {
            Box::pin(async move {
                pause_source(conn, chat_source, "budget", Some("budget reached"), NOW + 1).await
            })
        })
        .await
        .unwrap();
        let row = get_source(db.reader(), project, chat_source)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.paused_reason.as_deref(), Some("connection"));
        assert_eq!(row.paused_detail.as_deref(), Some("bad key"));

        let paused = db
            .write(move |conn| {
                Box::pin(async move {
                    pause_sources_of_connection(conn, embed_pause, None, NOW + 2).await
                })
            })
            .await
            .unwrap();
        assert_eq!(paused, vec![on_embed.id]);

        let resumed = db
            .write(move |conn| {
                Box::pin(async move { resume_connection_pauses(conn, chat_pause).await })
            })
            .await
            .unwrap();
        assert_eq!(resumed, vec![chat_source]);
        let resumed_row = get_source(db.reader(), project, chat_source)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resumed_row.paused_reason, None);
        assert_eq!(resumed_row.paused_detail, None);
        let still = get_source(db.reader(), project, on_embed.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still.paused_reason.as_deref(), Some("connection"));
        let untouched = get_source(db.reader(), project, other.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(untouched.paused_reason, None);
    }

    #[tokio::test]
    async fn a_budget_pause_clears_when_its_period_turns() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_org, projects) = seed(&db, &["a"]).await;
        let project = projects[0];
        let mut daily = scored_source(project, "daily", SourceKind::Score);
        daily.budget_usd = Some(1.0);
        daily.budget_period = Some("day".to_owned());
        let mut monthly = scored_source(project, "monthly", SourceKind::Score);
        monthly.budget_usd = Some(1.0);
        monthly.budget_period = Some("month".to_owned());
        for row in [&daily, &monthly] {
            put_source(&db, row.clone()).await;
        }
        for id in [daily.id, monthly.id] {
            db.write(move |conn| {
                Box::pin(async move {
                    pause_source(conn, id, "budget", Some("budget reached"), NOW).await
                })
            })
            .await
            .unwrap();
        }

        // The day has turned; the month has not.
        let cleared = db
            .write(move |conn| {
                Box::pin(async move { clear_budget_pauses(conn, NOW + 1, 0, NOW + 2).await })
            })
            .await
            .unwrap();
        assert_eq!(cleared, 1);
        let day = get_source(db.reader(), project, daily.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(day.paused_reason, None);
        let month = get_source(db.reader(), project, monthly.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(month.paused_reason.as_deref(), Some("budget"));
    }

    #[tokio::test]
    async fn prune_clears_old_vectors_expires_queued_candidates_and_keeps_fifty_refits() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_org, projects) = seed(&db, &["a"]).await;
        let source = scored_source(projects[0], "scores", SourceKind::Score);
        put_source(&db, source.clone()).await;
        let now = 100 * DAY;
        let with_vector = |mut row: CandidateRow| {
            row.vector = Some(vec![1.0, 0.0]);
            row
        };
        let old_pending = with_vector(candidate(
            1,
            &source,
            "old-pending",
            CandidateStatus::Pending,
            now - 8 * DAY,
        ));
        let new_pending = with_vector(candidate(
            2,
            &source,
            "new-pending",
            CandidateStatus::Pending,
            now - DAY,
        ));
        let old_queued = candidate(
            3,
            &source,
            "old-queued",
            CandidateStatus::Queued,
            now - 2 * DAY,
        );
        let new_queued = candidate(
            4,
            &source,
            "new-queued",
            CandidateStatus::Queued,
            now - DAY / 24,
        );
        let ancient = candidate(
            5,
            &source,
            "ancient",
            CandidateStatus::Pending,
            now - 40 * DAY,
        );
        for row in [
            old_pending,
            new_pending,
            old_queued,
            new_queued,
            ancient.clone(),
        ] {
            assert!(put_candidate(&db, row).await);
        }
        for i in 0..52_u64 {
            let row = refit(
                1_000 + u128::from(i),
                source.id,
                "schedule",
                i64::try_from(i).unwrap(),
            );
            db.write(move |conn| Box::pin(async move { insert_refit(conn, &row).await }))
                .await
                .unwrap();
        }

        // One queued candidate expires and the ancient one is deleted.
        let changed = db
            .write(move |conn| Box::pin(async move { prune_candidates(conn, now).await }))
            .await
            .unwrap();
        assert_eq!(changed, 2);
        assert!(
            read_candidate(&db, Uuid::from_u128(1))
                .await
                .vector
                .is_none()
        );
        assert_eq!(status_text(&db, Uuid::from_u128(1)).await, "pending");
        assert!(
            read_candidate(&db, Uuid::from_u128(2))
                .await
                .vector
                .is_some()
        );
        assert_eq!(status_text(&db, Uuid::from_u128(3)).await, "skipped");
        assert_eq!(
            read_candidate(&db, Uuid::from_u128(3))
                .await
                .error_class
                .as_deref(),
            Some("expired")
        );
        assert_eq!(status_text(&db, Uuid::from_u128(4)).await, "queued");
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM issue_candidates WHERE id = ?")
            .bind(ancient.id.to_string())
            .fetch_one(db.reader())
            .await
            .unwrap();
        assert_eq!(left, 0);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM issue_refits").await, 50);
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM issue_refits WHERE started_at < 2"
            )
            .await,
            0
        );
    }

    #[tokio::test]
    async fn prune_to_window_deletes_the_candidates_before_the_data_window() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, projects) = seed(&db, &["a"]).await;
        let source = scored_source(projects[0], "scores", SourceKind::Score);
        put_source(&db, source.clone()).await;
        let before = candidate(1, &source, "before", CandidateStatus::Pending, 10);
        let inside = candidate(2, &source, "inside", CandidateStatus::Pending, 500);
        for row in [before, inside] {
            assert!(put_candidate(&db, row).await);
        }

        let removed = db
            .write(move |conn| Box::pin(async move { prune_to_window(conn, org, 100).await }))
            .await
            .unwrap();
        assert_eq!(removed, 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM issue_candidates").await, 1);
    }

    #[tokio::test]
    async fn any_pending_sees_only_pending_candidates() {
        let (_dir, db) = crate::meta::test_db().await;
        let (_org, projects) = seed(&db, &["a"]).await;
        let source = scored_source(projects[0], "scores", SourceKind::Score);
        put_source(&db, source.clone()).await;
        let pending = candidate(1, &source, "pending", CandidateStatus::Pending, NOW);
        let (pending_id, gone) = (pending.id, Uuid::from_u128(9));
        assert!(put_candidate(&db, pending).await);

        let found = db
            .write(move |conn| {
                Box::pin(async move { any_pending(conn, &[gone, pending_id]).await })
            })
            .await
            .unwrap();
        assert!(found);
        let missing = db
            .write(move |conn| Box::pin(async move { any_pending(conn, &[gone]).await }))
            .await
            .unwrap();
        assert!(!missing);
    }

    #[tokio::test]
    async fn a_deletion_deletes_the_candidates_of_its_traces_in_its_project() {
        let (_dir, db) = crate::meta::test_db().await;
        let (org, projects) = seed(&db, &["a", "b"]).await;
        let (project_a, project_b) = (projects[0], projects[1]);
        let source_a = scored_source(project_a, "scores", SourceKind::Score);
        let source_b = scored_source(project_b, "scores", SourceKind::Score);
        put_source(&db, source_a.clone()).await;
        put_source(&db, source_b.clone()).await;
        // Candidates 1 and 2 are on the deleted traces; 3 is on another trace, and 4 is the
        // deleted trace 1 of the other project.
        let on = |id: u128, source: &SourceRow, byte: u8| CandidateRow {
            trace_id: Some(trace(byte)),
            ..candidate(
                id,
                source,
                &format!("dedup-{id}"),
                CandidateStatus::Pending,
                NOW,
            )
        };
        for row in [
            on(1, &source_a, 1),
            on(2, &source_a, 2),
            on(3, &source_a, 3),
            on(4, &source_b, 1),
        ] {
            assert!(put_candidate(&db, row).await);
        }

        let deletion = Uuid::from_u128(77);
        db.write(move |conn| {
            Box::pin(async move {
                insert_deletion(
                    conn,
                    &NewDeletion {
                        id: deletion,
                        org_id: org,
                        project_id: Some(project_a),
                        kind: DeletionKind::TraceIds,
                        user_id: None,
                        request: RequestDetails::default(),
                        actor_kind: ActorKind::User,
                        actor_id: Uuid::from_u128(1000),
                        state: DeletionState::Resolving,
                        created_at: NOW,
                    },
                )
                .await?;
                insert_traces(conn, deletion, &[trace(1), trace(2)]).await
            })
        })
        .await
        .unwrap();

        let removed = db
            .write(move |conn| {
                Box::pin(
                    async move { delete_candidates_of_deletion(conn, project_a, deletion).await },
                )
            })
            .await
            .unwrap();
        assert_eq!(removed, 2);
        for (id, expected) in [(1, 0), (2, 0), (3, 1), (4, 1)] {
            let found: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM issue_candidates WHERE id = ?")
                    .bind(Uuid::from_u128(id).to_string())
                    .fetch_one(db.reader())
                    .await
                    .unwrap();
            assert_eq!(found, expected, "candidate {id}");
        }
    }
}
