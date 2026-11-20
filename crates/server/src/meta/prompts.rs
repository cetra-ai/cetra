//! The prompt registry's rows (spec 04 § Prompts, D-V-7, D-V-8, D-V-10, D-V-11).

use std::collections::HashMap;
use std::fmt;

use cetra_core::ids::ProjectId;
use serde::Serialize;
use sqlx::{AssertSqlSafe, SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::{MetaError, unique_or};

/// A project holds at most this many prompts (D-V-2).
pub(crate) const PROJECT_CAP: i64 = 5_000;
/// A prompt has at most this many versions (D-V-2).
pub(crate) const VERSION_CAP: i64 = 10_000;
/// A prompt carries at most this many labels (D-V-8).
pub(crate) const LABEL_CAP: i64 = 50;
/// The protected labels of a project without a `prompt_settings` row (D-V-10).
pub(crate) const DEFAULT_PROTECTED: &[&str] = &["production"];
/// Conflict reasons.
pub(crate) const DUPLICATE_NAME: &str = "a prompt with this name already exists";
pub(crate) const TOO_MANY_PROMPTS: &str = "a project has at most 5000 prompts";
pub(crate) const TOO_MANY_VERSIONS: &str = "a prompt has at most 10000 versions";
pub(crate) const TOO_MANY_LABELS: &str = "a prompt has at most 50 labels";
/// The prompt was deleted and created again with another type between the request's read and
/// its write.
pub(crate) const PROMPT_CHANGED: &str = "the prompt changed; retry";

/// `prompts.type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptType {
    Text,
    Chat,
}

impl PromptType {
    /// The stored and API spelling: `text` or `chat`.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Chat => "chat",
        }
    }

    /// Parses the stored spelling. Anything else is `None`.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "text" => Some(Self::Text),
            "chat" => Some(Self::Chat),
            _ => None,
        }
    }
}

/// One label on a prompt.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub(crate) struct LabelRow {
    pub(crate) label: String,
    pub(crate) version: i64,
    pub(crate) updated_by: Option<String>,
    pub(crate) updated_at: i64,
}

/// One stored prompt with its labels (ordered by label).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PromptRow {
    pub(crate) id: Uuid,
    pub(crate) project_id: ProjectId,
    pub(crate) name: String,
    pub(crate) prompt_type: PromptType,
    pub(crate) description: Option<String>,
    pub(crate) tags: Vec<String>,
    pub(crate) latest_version: i64,
    pub(crate) labels: Vec<LabelRow>,
    pub(crate) created_by: Option<String>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

/// One version. `template` and `config` are the stored JSON texts. `Debug` prints sizes only.
#[derive(Clone, PartialEq)]
pub(crate) struct VersionRow {
    pub(crate) prompt_id: Uuid,
    pub(crate) version: i64,
    pub(crate) template: String,
    pub(crate) config: String,
    pub(crate) commit_message: Option<String>,
    /// Labels pointing at this version, ordered.
    pub(crate) labels: Vec<String>,
    pub(crate) created_by: Option<String>,
    pub(crate) created_at: i64,
}

impl fmt::Debug for VersionRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VersionRow")
            .field("prompt_id", &self.prompt_id)
            .field("version", &self.version)
            .field("template_bytes", &self.template.len())
            .field("config_bytes", &self.config.len())
            .field("label_count", &self.labels.len())
            .finish_non_exhaustive()
    }
}

/// A prompt to insert.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NewPrompt {
    pub(crate) id: Uuid,
    pub(crate) name: String,
    pub(crate) prompt_type: PromptType,
    pub(crate) description: Option<String>,
    pub(crate) tags: Vec<String>,
}

/// A version to append, and the labels to point at it. `Debug` prints sizes only.
#[derive(Clone, PartialEq)]
pub(crate) struct NewVersion {
    pub(crate) template: String,
    pub(crate) config: String,
    pub(crate) commit_message: Option<String>,
    pub(crate) labels: Vec<String>,
    pub(crate) actor: Option<String>,
}

impl fmt::Debug for NewVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewVersion")
            .field("template_bytes", &self.template.len())
            .field("config_bytes", &self.config.len())
            .field("label_count", &self.labels.len())
            .finish_non_exhaustive()
    }
}

/// One label history entry.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub(crate) struct LabelEventRow {
    pub(crate) id: i64,
    pub(crate) label: String,
    pub(crate) from_version: Option<i64>,
    pub(crate) to_version: Option<i64>,
    pub(crate) actor: Option<String>,
    pub(crate) at: i64,
}

/// What [`set_label`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LabelChange {
    /// The label was new or moved; `from` is its previous version.
    Moved { from: Option<i64> },
    /// The label already pointed at that version: nothing written.
    Unchanged,
}

/// List filters. `q` matches a case-insensitive substring of the name (LIKE, escaped as
/// `meta::datasets` does with `escape_like`); `label` keeps prompts carrying that label;
/// `tag` keeps prompts whose tags contain it exactly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PromptFilter {
    pub(crate) q: Option<String>,
    pub(crate) label: Option<String>,
    pub(crate) tag: Option<String>,
}

#[derive(sqlx::FromRow)]
struct PromptRecord {
    id: String,
    project_id: String,
    name: String,
    prompt_type: String,
    description: Option<String>,
    tags: String,
    latest_version: i64,
    created_by: Option<String>,
    created_at: i64,
    updated_at: i64,
}

#[derive(sqlx::FromRow)]
struct PromptLabelRecord {
    prompt_id: String,
    label: String,
    version: i64,
    updated_by: Option<String>,
    updated_at: i64,
}

#[derive(sqlx::FromRow)]
struct VersionRecord {
    prompt_id: String,
    version: i64,
    template: String,
    config: String,
    commit_message: Option<String>,
    created_by: Option<String>,
    created_at: i64,
}

/// The columns of a prompt, with `type` renamed so it cannot clash with the Rust keyword.
const PROMPT_COLUMNS: &str = "p.id, p.project_id, p.name, p.type AS prompt_type, p.description, \
     p.tags, p.latest_version, p.created_by, p.created_at, p.updated_at";

const VERSION_SELECT: &str = "SELECT prompt_id, version, template, config, commit_message, \
     created_by, created_at FROM prompt_versions WHERE prompt_id = ? AND version = ?";

/// Every prompt of `project` matching `filter`, ordered by `lower(name)`, then `id`.
pub(crate) async fn list_prompts(
    pool: &SqlitePool,
    project: ProjectId,
    filter: &PromptFilter,
) -> Result<Vec<PromptRow>, MetaError> {
    // Only constants are spliced; every value is bound.
    let sql = format!(
        "SELECT {PROMPT_COLUMNS} FROM prompts p WHERE p.project_id = ?1 \
         AND (?2 IS NULL OR lower(p.name) LIKE '%' || lower(?2) || '%' ESCAPE '\\') \
         AND (?3 IS NULL OR EXISTS (SELECT 1 FROM prompt_labels l \
              WHERE l.prompt_id = p.id AND l.label = ?3)) \
         AND (?4 IS NULL OR EXISTS (SELECT 1 FROM json_each(p.tags) t WHERE t.value = ?4)) \
         ORDER BY lower(p.name), p.id"
    );
    let records: Vec<PromptRecord> = sqlx::query_as(AssertSqlSafe(sql))
        .bind(project.to_string())
        .bind(filter.q.as_deref().map(escape_like))
        .bind(filter.label.as_deref())
        .bind(filter.tag.as_deref())
        .fetch_all(pool)
        .await?;
    let labels: Vec<PromptLabelRecord> = sqlx::query_as(
        "SELECT prompt_id, label, version, updated_by, updated_at FROM prompt_labels \
         WHERE prompt_id IN (SELECT id FROM prompts WHERE project_id = ?) \
         ORDER BY prompt_id, label",
    )
    .bind(project.to_string())
    .fetch_all(pool)
    .await?;
    let mut by_prompt: HashMap<String, Vec<LabelRow>> = HashMap::new();
    for label in labels {
        by_prompt
            .entry(label.prompt_id)
            .or_default()
            .push(LabelRow {
                label: label.label,
                version: label.version,
                updated_by: label.updated_by,
                updated_at: label.updated_at,
            });
    }
    records
        .into_iter()
        .map(|record| {
            let labels = by_prompt.remove(&record.id).unwrap_or_default();
            prompt_from(record, labels)
        })
        .collect()
}

/// The prompt of `project` named `name`.
pub(crate) async fn get_prompt(
    pool: &SqlitePool,
    project: ProjectId,
    name: &str,
) -> Result<Option<PromptRow>, MetaError> {
    let mut conn = pool.acquire().await?;
    get_prompt_tx(&mut conn, project, name).await
}

/// The same read inside a write transaction.
pub(crate) async fn get_prompt_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    name: &str,
) -> Result<Option<PromptRow>, MetaError> {
    // Only constants are spliced; every value is bound.
    let sql =
        format!("SELECT {PROMPT_COLUMNS} FROM prompts p WHERE p.project_id = ?1 AND p.name = ?2");
    let record: Option<PromptRecord> = sqlx::query_as(AssertSqlSafe(sql))
        .bind(project.to_string())
        .bind(name)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(record) = record else {
        return Ok(None);
    };
    let labels = labels_of_prompt(conn, &record.id).await?;
    prompt_from(record, labels).map(Some)
}

async fn labels_of_prompt(
    conn: &mut SqliteConnection,
    prompt_id: &str,
) -> Result<Vec<LabelRow>, MetaError> {
    let labels: Vec<LabelRow> = sqlx::query_as(
        "SELECT label, version, updated_by, updated_at FROM prompt_labels \
         WHERE prompt_id = ? ORDER BY label",
    )
    .bind(prompt_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(labels)
}

/// Inserts `prompt` with `first` as version 1 and its labels. Past [`PROJECT_CAP`] is
/// `Conflict(TOO_MANY_PROMPTS)`; a duplicate name is `Conflict(DUPLICATE_NAME)`.
pub(crate) async fn create_prompt(
    conn: &mut SqliteConnection,
    project: ProjectId,
    prompt: NewPrompt,
    first: NewVersion,
    now: i64,
) -> Result<(PromptRow, VersionRow), MetaError> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM prompts WHERE project_id = ?")
        .bind(project.to_string())
        .fetch_one(&mut *conn)
        .await?;
    if count >= PROJECT_CAP {
        return Err(MetaError::Conflict(TOO_MANY_PROMPTS));
    }
    sqlx::query(
        "INSERT INTO prompts (id, project_id, name, type, description, tags, latest_version, \
         created_by, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, 0, ?, ?, ?)",
    )
    .bind(prompt.id.to_string())
    .bind(project.to_string())
    .bind(prompt.name.as_str())
    .bind(prompt.prompt_type.as_str())
    .bind(prompt.description.as_deref())
    .bind(json_text(&prompt.tags, "prompts", "tags")?)
    .bind(first.actor.as_deref())
    .bind(now)
    .bind(now)
    .execute(&mut *conn)
    .await
    .map_err(|error| unique_or(error, DUPLICATE_NAME))?;
    let version = add_version(conn, prompt.id, first, now).await?;
    let stored = get_prompt_tx(conn, project, &prompt.name)
        .await?
        .ok_or(corrupt("prompts", "id"))?;
    Ok((stored, version))
}

/// Appends version `latest_version + 1` of `prompt_id` and points its labels at it. Past
/// [`VERSION_CAP`] is `Conflict(TOO_MANY_VERSIONS)`.
pub(crate) async fn add_version(
    conn: &mut SqliteConnection,
    prompt_id: Uuid,
    new: NewVersion,
    now: i64,
) -> Result<VersionRow, MetaError> {
    let latest: Option<i64> = sqlx::query_scalar("SELECT latest_version FROM prompts WHERE id = ?")
        .bind(prompt_id.to_string())
        .fetch_optional(&mut *conn)
        .await?;
    let version = latest.ok_or(corrupt("prompts", "id"))? + 1;
    if version > VERSION_CAP {
        return Err(MetaError::Conflict(TOO_MANY_VERSIONS));
    }
    sqlx::query(
        "INSERT INTO prompt_versions (prompt_id, version, template, config, commit_message, \
         created_by, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(prompt_id.to_string())
    .bind(version)
    .bind(new.template.as_str())
    .bind(new.config.as_str())
    .bind(new.commit_message.as_deref())
    .bind(new.actor.as_deref())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE prompts SET latest_version = ?, updated_at = ? WHERE id = ?")
        .bind(version)
        .bind(now)
        .bind(prompt_id.to_string())
        .execute(&mut *conn)
        .await?;
    for label in &new.labels {
        let change = set_label(conn, prompt_id, label, version, new.actor.as_deref(), now).await?;
        // The version was inserted in this transaction, so the move cannot find it missing.
        if change.is_none() {
            return Err(corrupt("prompt_versions", "version"));
        }
    }
    get_version_tx(conn, prompt_id, version)
        .await?
        .ok_or(corrupt("prompt_versions", "version"))
}

/// Replaces `description` when `Some` (the inner `None` clears it) and `tags` when `Some`,
/// and sets `updated_at`. `None` when no such prompt in `project`.
pub(crate) async fn update_prompt(
    conn: &mut SqliteConnection,
    project: ProjectId,
    name: &str,
    description: Option<Option<String>>,
    tags: Option<Vec<String>>,
    now: i64,
) -> Result<Option<PromptRow>, MetaError> {
    let description_set = description.is_some();
    let description = description.flatten();
    let tags_set = tags.is_some();
    let tags = tags
        .map(|tags| json_text(&tags, "prompts", "tags"))
        .transpose()?;
    let result = sqlx::query(
        "UPDATE prompts SET updated_at = ?, \
         description = CASE WHEN ? THEN ? ELSE description END, \
         tags = CASE WHEN ? THEN ? ELSE tags END \
         WHERE project_id = ? AND name = ?",
    )
    .bind(now)
    .bind(description_set)
    .bind(description)
    .bind(tags_set)
    .bind(tags)
    .bind(project.to_string())
    .bind(name)
    .execute(&mut *conn)
    .await?;
    if result.rows_affected() == 0 {
        return Ok(None);
    }
    get_prompt_tx(conn, project, name).await
}

/// Deletes the prompt and, by cascade, its versions, labels and history. False when absent.
pub(crate) async fn delete_prompt(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
) -> Result<bool, MetaError> {
    let result = sqlx::query("DELETE FROM prompts WHERE project_id = ? AND id = ?")
        .bind(project.to_string())
        .bind(id.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// One version.
pub(crate) async fn get_version(
    pool: &SqlitePool,
    prompt_id: Uuid,
    version: i64,
) -> Result<Option<VersionRow>, MetaError> {
    let mut conn = pool.acquire().await?;
    get_version_tx(&mut conn, prompt_id, version).await
}

async fn get_version_tx(
    conn: &mut SqliteConnection,
    prompt_id: Uuid,
    version: i64,
) -> Result<Option<VersionRow>, MetaError> {
    let record: Option<VersionRecord> = sqlx::query_as(VERSION_SELECT)
        .bind(prompt_id.to_string())
        .bind(version)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(record) = record else {
        return Ok(None);
    };
    let labels: Vec<String> = sqlx::query_scalar(
        "SELECT label FROM prompt_labels WHERE prompt_id = ? AND version = ? ORDER BY label",
    )
    .bind(prompt_id.to_string())
    .bind(version)
    .fetch_all(&mut *conn)
    .await?;
    version_from(record, labels).map(Some)
}

/// The version a stored label points at (`latest` is not stored: callers use `get_version`).
pub(crate) async fn resolve_label(
    pool: &SqlitePool,
    prompt_id: Uuid,
    label: &str,
) -> Result<Option<VersionRow>, MetaError> {
    let mut conn = pool.acquire().await?;
    let version: Option<i64> =
        sqlx::query_scalar("SELECT version FROM prompt_labels WHERE prompt_id = ? AND label = ?")
            .bind(prompt_id.to_string())
            .bind(label)
            .fetch_optional(&mut *conn)
            .await?;
    match version {
        Some(version) => get_version_tx(&mut conn, prompt_id, version).await,
        None => Ok(None),
    }
}

/// Versions newest first, strictly below `before` when set; at most `limit + 1` rows.
pub(crate) async fn list_versions(
    pool: &SqlitePool,
    prompt_id: Uuid,
    before: Option<i64>,
    limit: u32,
) -> Result<Vec<VersionRow>, MetaError> {
    let records: Vec<VersionRecord> = sqlx::query_as(
        "SELECT prompt_id, version, template, config, commit_message, created_by, created_at \
         FROM prompt_versions WHERE prompt_id = ?1 AND (?2 IS NULL OR version < ?2) \
         ORDER BY version DESC LIMIT ?3",
    )
    .bind(prompt_id.to_string())
    .bind(before)
    .bind(i64::from(limit) + 1)
    .fetch_all(pool)
    .await?;
    // One query for the prompt's labels, attached in memory (at most LABEL_CAP rows).
    let labels: Vec<(i64, String)> = sqlx::query_as(
        "SELECT version, label FROM prompt_labels WHERE prompt_id = ? ORDER BY label",
    )
    .bind(prompt_id.to_string())
    .fetch_all(pool)
    .await?;
    let mut by_version: HashMap<i64, Vec<String>> = HashMap::new();
    for (version, label) in labels {
        by_version.entry(version).or_default().push(label);
    }
    records
        .into_iter()
        .map(|record| {
            let labels = by_version.remove(&record.version).unwrap_or_default();
            version_from(record, labels)
        })
        .collect()
}

/// Points `label` at `version` and records the move. A version that does not exist is
/// `Ok(None)`. A new label past [`LABEL_CAP`] is `Conflict(TOO_MANY_LABELS)`.
pub(crate) async fn set_label(
    conn: &mut SqliteConnection,
    prompt_id: Uuid,
    label: &str,
    version: i64,
    actor: Option<&str>,
    now: i64,
) -> Result<Option<LabelChange>, MetaError> {
    let exists: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM prompt_versions WHERE prompt_id = ? AND version = ?")
            .bind(prompt_id.to_string())
            .bind(version)
            .fetch_optional(&mut *conn)
            .await?;
    if exists.is_none() {
        return Ok(None);
    }
    let from = current_label_version(conn, prompt_id, label).await?;
    if from == Some(version) {
        return Ok(Some(LabelChange::Unchanged));
    }
    if from.is_none() {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM prompt_labels WHERE prompt_id = ?")
                .bind(prompt_id.to_string())
                .fetch_one(&mut *conn)
                .await?;
        if count >= LABEL_CAP {
            return Err(MetaError::Conflict(TOO_MANY_LABELS));
        }
    }
    sqlx::query(
        "INSERT INTO prompt_labels (prompt_id, label, version, updated_by, updated_at) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(prompt_id, label) DO UPDATE SET version = excluded.version, \
         updated_by = excluded.updated_by, updated_at = excluded.updated_at",
    )
    .bind(prompt_id.to_string())
    .bind(label)
    .bind(version)
    .bind(actor)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    write_label_event(conn, prompt_id, label, from, Some(version), actor, now).await?;
    touch_prompt(conn, prompt_id, now).await?;
    Ok(Some(LabelChange::Moved { from }))
}

/// Removes `label` and records it. False when the label is not set.
pub(crate) async fn remove_label(
    conn: &mut SqliteConnection,
    prompt_id: Uuid,
    label: &str,
    actor: Option<&str>,
    now: i64,
) -> Result<bool, MetaError> {
    let Some(from) = current_label_version(conn, prompt_id, label).await? else {
        return Ok(false);
    };
    sqlx::query("DELETE FROM prompt_labels WHERE prompt_id = ? AND label = ?")
        .bind(prompt_id.to_string())
        .bind(label)
        .execute(&mut *conn)
        .await?;
    write_label_event(conn, prompt_id, label, Some(from), None, actor, now).await?;
    touch_prompt(conn, prompt_id, now).await?;
    Ok(true)
}

/// History newest first, with `id < before` when set; at most `limit + 1` rows.
pub(crate) async fn list_label_events(
    pool: &SqlitePool,
    prompt_id: Uuid,
    before: Option<i64>,
    limit: u32,
) -> Result<Vec<LabelEventRow>, MetaError> {
    let events: Vec<LabelEventRow> = sqlx::query_as(
        "SELECT id, label, from_version, to_version, actor, at FROM prompt_label_events \
         WHERE prompt_id = ?1 AND (?2 IS NULL OR id < ?2) ORDER BY id DESC LIMIT ?3",
    )
    .bind(prompt_id.to_string())
    .bind(before)
    .bind(i64::from(limit) + 1)
    .fetch_all(pool)
    .await?;
    Ok(events)
}

/// The project's protected labels: its `prompt_settings` row, else [`DEFAULT_PROTECTED`].
pub(crate) async fn protected_labels(
    pool: &SqlitePool,
    project: ProjectId,
) -> Result<Vec<String>, MetaError> {
    let mut conn = pool.acquire().await?;
    protected_labels_tx(&mut conn, project).await
}

/// The same inside a write transaction.
pub(crate) async fn protected_labels_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
) -> Result<Vec<String>, MetaError> {
    let text: Option<String> =
        sqlx::query_scalar("SELECT protected_labels FROM prompt_settings WHERE project_id = ?")
            .bind(project.to_string())
            .fetch_optional(&mut *conn)
            .await?;
    match text {
        None => Ok(DEFAULT_PROTECTED
            .iter()
            .map(|label| (*label).to_owned())
            .collect()),
        Some(text) => {
            serde_json::from_str(&text).map_err(|_| corrupt("prompt_settings", "protected_labels"))
        }
    }
}

/// Upserts the project's protected labels.
pub(crate) async fn set_protected_labels(
    conn: &mut SqliteConnection,
    project: ProjectId,
    labels: &[String],
    actor: Option<&str>,
    now: i64,
) -> Result<(), MetaError> {
    sqlx::query(
        "INSERT INTO prompt_settings (project_id, protected_labels, updated_by, updated_at) \
         VALUES (?, ?, ?, ?) \
         ON CONFLICT(project_id) DO UPDATE SET protected_labels = excluded.protected_labels, \
         updated_by = excluded.updated_by, updated_at = excluded.updated_at",
    )
    .bind(project.to_string())
    .bind(json_text(labels, "prompt_settings", "protected_labels")?)
    .bind(actor)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The version a label points at now, `None` when the label is not set.
async fn current_label_version(
    conn: &mut SqliteConnection,
    prompt_id: Uuid,
    label: &str,
) -> Result<Option<i64>, MetaError> {
    let version =
        sqlx::query_scalar("SELECT version FROM prompt_labels WHERE prompt_id = ? AND label = ?")
            .bind(prompt_id.to_string())
            .bind(label)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(version)
}

async fn write_label_event(
    conn: &mut SqliteConnection,
    prompt_id: Uuid,
    label: &str,
    from: Option<i64>,
    to: Option<i64>,
    actor: Option<&str>,
    now: i64,
) -> Result<(), MetaError> {
    sqlx::query(
        "INSERT INTO prompt_label_events (prompt_id, label, from_version, to_version, actor, at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(prompt_id.to_string())
    .bind(label)
    .bind(from)
    .bind(to)
    .bind(actor)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn touch_prompt(
    conn: &mut SqliteConnection,
    prompt_id: Uuid,
    now: i64,
) -> Result<(), MetaError> {
    sqlx::query("UPDATE prompts SET updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(prompt_id.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

fn prompt_from(record: PromptRecord, labels: Vec<LabelRow>) -> Result<PromptRow, MetaError> {
    Ok(PromptRow {
        id: parse_uuid(&record.id, "prompts", "id")?,
        project_id: record
            .project_id
            .parse()
            .map_err(|_| corrupt("prompts", "project_id"))?,
        name: record.name,
        prompt_type: PromptType::parse(&record.prompt_type).ok_or(corrupt("prompts", "type"))?,
        description: record.description,
        tags: serde_json::from_str(&record.tags).map_err(|_| corrupt("prompts", "tags"))?,
        latest_version: record.latest_version,
        labels,
        created_by: record.created_by,
        created_at: record.created_at,
        updated_at: record.updated_at,
    })
}

fn version_from(record: VersionRecord, labels: Vec<String>) -> Result<VersionRow, MetaError> {
    Ok(VersionRow {
        prompt_id: parse_uuid(&record.prompt_id, "prompt_versions", "prompt_id")?,
        version: record.version,
        template: record.template,
        config: record.config,
        commit_message: record.commit_message,
        labels,
        created_by: record.created_by,
        created_at: record.created_at,
    })
}

/// `value` as JSON text. A failure is `Corrupt` on the given column.
fn json_text<T: Serialize + ?Sized>(
    value: &T,
    table: &'static str,
    column: &'static str,
) -> Result<String, MetaError> {
    serde_json::to_string(value).map_err(|_| corrupt(table, column))
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

fn corrupt(table: &'static str, column: &'static str) -> MetaError {
    MetaError::Corrupt { table, column }
}

fn parse_uuid(value: &str, table: &'static str, column: &'static str) -> Result<Uuid, MetaError> {
    value.parse().map_err(|_| corrupt(table, column))
}

#[cfg(test)]
mod tests {
    use cetra_core::ids::{OrgId, ProjectId};
    use uuid::Uuid;

    use super::{
        DEFAULT_PROTECTED, DUPLICATE_NAME, LABEL_CAP, LabelChange, NewPrompt, NewVersion,
        PROJECT_CAP, PromptFilter, PromptRow, PromptType, TOO_MANY_LABELS, TOO_MANY_PROMPTS,
        TOO_MANY_VERSIONS, VersionRow, add_version, create_prompt, delete_prompt, get_prompt,
        get_prompt_tx, get_version, list_label_events, list_prompts, list_versions,
        protected_labels, remove_label, resolve_label, set_label, set_protected_labels,
        update_prompt,
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

    fn new_prompt(name: &str, prompt_type: PromptType) -> NewPrompt {
        NewPrompt {
            id: Uuid::now_v7(),
            name: name.to_owned(),
            prompt_type,
            description: None,
            tags: Vec::new(),
        }
    }

    fn version(template: &str, labels: &[&str]) -> NewVersion {
        NewVersion {
            template: template.to_owned(),
            config: "{}".to_owned(),
            commit_message: None,
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
            actor: Some("user-1".to_owned()),
        }
    }

    async fn create(
        db: &MetaDb,
        project: ProjectId,
        prompt: NewPrompt,
        first: NewVersion,
    ) -> Result<(PromptRow, VersionRow), MetaError> {
        db.write(move |conn| {
            Box::pin(async move { create_prompt(conn, project, prompt, first, NOW).await })
        })
        .await
    }

    async fn add(db: &MetaDb, id: Uuid, new: NewVersion) -> Result<VersionRow, MetaError> {
        db.write(move |conn| Box::pin(async move { add_version(conn, id, new, NOW).await }))
            .await
    }

    async fn set(
        db: &MetaDb,
        id: Uuid,
        label: &str,
        version: i64,
    ) -> Result<Option<LabelChange>, MetaError> {
        let label = label.to_owned();
        db.write(move |conn| {
            Box::pin(async move { set_label(conn, id, &label, version, Some("user-2"), NOW).await })
        })
        .await
    }

    async fn remove(db: &MetaDb, id: Uuid, label: &str) -> Result<bool, MetaError> {
        let label = label.to_owned();
        db.write(move |conn| {
            Box::pin(async move { remove_label(conn, id, &label, Some("user-2"), NOW).await })
        })
        .await
    }

    async fn event_count(db: &MetaDb, id: Uuid) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM prompt_label_events WHERE prompt_id = ?")
            .bind(id.to_string())
            .fetch_one(db.reader())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn create_get_list_and_delete() {
        let (_dir, db) = crate::meta::test_db().await;
        let org = org(&db).await;
        let project = project(&db, org).await;
        let (beta, _) = create(
            &db,
            project,
            new_prompt("beta", PromptType::Text),
            version("hello {{ name }}", &["production"]),
        )
        .await
        .unwrap();
        create(
            &db,
            project,
            new_prompt("Alpha", PromptType::Chat),
            version("[]", &[]),
        )
        .await
        .unwrap();
        create(
            &db,
            project,
            new_prompt("gamma", PromptType::Text),
            version("g", &[]),
        )
        .await
        .unwrap();

        let names: Vec<String> = list_prompts(db.reader(), project, &PromptFilter::default())
            .await
            .unwrap()
            .into_iter()
            .map(|prompt| prompt.name)
            .collect();
        assert_eq!(names, ["Alpha", "beta", "gamma"]);

        let stored = get_prompt(db.reader(), project, "beta")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.latest_version, 1);
        assert_eq!(stored.prompt_type, PromptType::Text);
        assert_eq!(stored, beta);
        let first = get_version(db.reader(), beta.id, 1).await.unwrap().unwrap();
        assert_eq!(first.template, "hello {{ name }}");
        assert_eq!(event_count(&db, beta.id).await, 1);

        let id = beta.id;
        let deleted = db
            .write(move |conn| Box::pin(async move { delete_prompt(conn, project, id).await }))
            .await
            .unwrap();
        assert!(deleted);
        let deleted_again = db
            .write(move |conn| Box::pin(async move { delete_prompt(conn, project, id).await }))
            .await
            .unwrap();
        assert!(!deleted_again);
        assert!(
            get_prompt(db.reader(), project, "beta")
                .await
                .unwrap()
                .is_none()
        );
        assert!(get_version(db.reader(), id, 1).await.unwrap().is_none());
        assert_eq!(event_count(&db, id).await, 0);
    }

    #[tokio::test]
    async fn duplicate_name_is_a_conflict() {
        let (_dir, db) = crate::meta::test_db().await;
        let org = org(&db).await;
        let project = project(&db, org).await;
        create(
            &db,
            project,
            new_prompt("beta", PromptType::Text),
            version("a", &[]),
        )
        .await
        .unwrap();

        let error = create(
            &db,
            project,
            new_prompt("beta", PromptType::Chat),
            version("[]", &[]),
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), DUPLICATE_NAME);

        let other = db
            .write(move |conn| {
                Box::pin(async move {
                    insert_project(conn, org, "Other", "other", None, NOW)
                        .await
                        .map(|row| row.id)
                })
            })
            .await
            .unwrap();
        create(
            &db,
            other,
            new_prompt("beta", PromptType::Text),
            version("b", &[]),
        )
        .await
        .unwrap();

        let rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM prompts WHERE project_id = ? AND name = 'beta'",
        )
        .bind(project.to_string())
        .fetch_one(db.reader())
        .await
        .unwrap();
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn versions_count_up_and_labels_move() {
        let (_dir, db) = crate::meta::test_db().await;
        let org = org(&db).await;
        let project = project(&db, org).await;
        let (prompt, _) = create(
            &db,
            project,
            new_prompt("p", PromptType::Text),
            version("one", &["production"]),
        )
        .await
        .unwrap();
        let id = prompt.id;

        let second = add(&db, id, version("two", &["staging"])).await.unwrap();
        assert_eq!(second.version, 2);
        assert_eq!(second.labels, ["staging"]);
        let third = add(&db, id, version("three", &["production"]))
            .await
            .unwrap();
        assert_eq!(third.version, 3);

        let production = resolve_label(db.reader(), id, "production")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(production.version, 3);
        let staging = resolve_label(db.reader(), id, "staging")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(staging.version, 2);
        let first = get_version(db.reader(), id, 1).await.unwrap().unwrap();
        assert!(first.labels.is_empty());

        let stored = get_prompt(db.reader(), project, "p")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.latest_version, 3);
        let labels: Vec<(String, i64)> = stored
            .labels
            .iter()
            .map(|label| (label.label.clone(), label.version))
            .collect();
        assert_eq!(
            labels,
            [("production".to_owned(), 3), ("staging".to_owned(), 2)]
        );

        let events = list_label_events(db.reader(), id, None, 10).await.unwrap();
        let moves: Vec<(String, Option<i64>, Option<i64>)> = events
            .iter()
            .map(|event| (event.label.clone(), event.from_version, event.to_version))
            .collect();
        assert_eq!(
            moves,
            [
                ("production".to_owned(), Some(1), Some(3)),
                ("staging".to_owned(), None, Some(2)),
                ("production".to_owned(), None, Some(1)),
            ]
        );
    }

    #[tokio::test]
    async fn set_label_cases() {
        let (_dir, db) = crate::meta::test_db().await;
        let org = org(&db).await;
        let project = project(&db, org).await;
        let (prompt, _) = create(
            &db,
            project,
            new_prompt("p", PromptType::Text),
            version("one", &[]),
        )
        .await
        .unwrap();
        let id = prompt.id;
        add(&db, id, version("two", &[])).await.unwrap();

        assert_eq!(set(&db, id, "x", 9).await.unwrap(), None);
        assert_eq!(
            set(&db, id, "production", 1).await.unwrap(),
            Some(LabelChange::Moved { from: None })
        );
        let before = event_count(&db, id).await;
        assert_eq!(
            set(&db, id, "production", 1).await.unwrap(),
            Some(LabelChange::Unchanged)
        );
        assert_eq!(event_count(&db, id).await, before);
        assert_eq!(
            set(&db, id, "production", 2).await.unwrap(),
            Some(LabelChange::Moved { from: Some(1) })
        );
    }

    #[tokio::test]
    async fn remove_label_cases() {
        let (_dir, db) = crate::meta::test_db().await;
        let org = org(&db).await;
        let project = project(&db, org).await;
        let (prompt, _) = create(
            &db,
            project,
            new_prompt("p", PromptType::Text),
            version("one", &["staging"]),
        )
        .await
        .unwrap();
        let id = prompt.id;

        assert!(remove(&db, id, "staging").await.unwrap());
        let last = list_label_events(db.reader(), id, None, 1)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(last.label, "staging");
        assert_eq!(last.to_version, None);
        assert_eq!(last.from_version, Some(1));
        assert!(!remove(&db, id, "staging").await.unwrap());
    }

    #[tokio::test]
    async fn label_cap() {
        let (_dir, db) = crate::meta::test_db().await;
        let org = org(&db).await;
        let project = project(&db, org).await;
        let (prompt, _) = create(
            &db,
            project,
            new_prompt("p", PromptType::Text),
            version("one", &[]),
        )
        .await
        .unwrap();
        let id = prompt.id;
        add(&db, id, version("two", &[])).await.unwrap();

        db.write(move |conn| {
            Box::pin(async move {
                for index in 0..LABEL_CAP {
                    let label = format!("l{index:02}");
                    set_label(conn, id, &label, 1, None, NOW).await?;
                }
                Ok::<(), MetaError>(())
            })
        })
        .await
        .unwrap();

        let error = set(&db, id, "extra", 1).await.unwrap_err();
        assert_eq!(error.to_string(), TOO_MANY_LABELS);
        assert_eq!(
            set(&db, id, "l00", 2).await.unwrap(),
            Some(LabelChange::Moved { from: Some(1) })
        );
    }

    #[tokio::test]
    async fn project_cap() {
        let (_dir, db) = crate::meta::test_db().await;
        let org = org(&db).await;
        let project = project(&db, org).await;

        db.write(move |conn| {
            Box::pin(async move {
                for index in 0..PROJECT_CAP {
                    let name = format!("p{index:05}");
                    create_prompt(
                        conn,
                        project,
                        new_prompt(&name, PromptType::Text),
                        version("t", &[]),
                        NOW,
                    )
                    .await?;
                }
                Ok::<(), MetaError>(())
            })
        })
        .await
        .unwrap();

        let error = create(
            &db,
            project,
            new_prompt("one-more", PromptType::Text),
            version("t", &[]),
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), TOO_MANY_PROMPTS);
    }

    #[tokio::test]
    async fn version_cap() {
        let (_dir, db) = crate::meta::test_db().await;
        let org = org(&db).await;
        let project = project(&db, org).await;
        let (prompt, _) = create(
            &db,
            project,
            new_prompt("p", PromptType::Text),
            version("one", &[]),
        )
        .await
        .unwrap();
        let id = prompt.id;
        db.write(move |conn| {
            Box::pin(async move {
                sqlx::query("UPDATE prompts SET latest_version = 10000 WHERE id = ?")
                    .bind(id.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok::<(), MetaError>(())
            })
        })
        .await
        .unwrap();

        let error = add(&db, id, version("too many", &[])).await.unwrap_err();
        assert_eq!(error.to_string(), TOO_MANY_VERSIONS);
    }

    #[tokio::test]
    async fn update_prompt_tri_state() {
        let (_dir, db) = crate::meta::test_db().await;
        let org = org(&db).await;
        let project = project(&db, org).await;
        create(
            &db,
            project,
            new_prompt("p", PromptType::Text),
            version("one", &[]),
        )
        .await
        .unwrap();

        async fn update(
            db: &MetaDb,
            project: ProjectId,
            name: &str,
            description: Option<Option<String>>,
            tags: Option<Vec<String>>,
        ) -> Option<PromptRow> {
            let name = name.to_owned();
            db.write(move |conn| {
                Box::pin(async move {
                    update_prompt(conn, project, &name, description, tags, NOW).await
                })
            })
            .await
            .unwrap()
        }

        let after_set = update(&db, project, "p", Some(Some("d".to_owned())), None)
            .await
            .unwrap();
        assert_eq!(after_set.description.as_deref(), Some("d"));
        let kept = update(&db, project, "p", None, Some(vec!["a".to_owned()]))
            .await
            .unwrap();
        assert_eq!(kept.description.as_deref(), Some("d"));
        assert_eq!(kept.tags, ["a"]);
        let cleared = update(&db, project, "p", Some(None), Some(vec!["b".to_owned()]))
            .await
            .unwrap();
        assert_eq!(cleared.description, None);
        assert_eq!(cleared.tags, ["b"]);
        assert!(
            update(&db, project, "missing", Some(None), None)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn filters() {
        let (_dir, db) = crate::meta::test_db().await;
        let org = org(&db).await;
        let project = project(&db, org).await;
        let mut support_a = new_prompt("support-a", PromptType::Text);
        support_a.tags = vec!["x".to_owned()];
        create(&db, project, support_a, version("a", &["production"]))
            .await
            .unwrap();
        create(
            &db,
            project,
            new_prompt("support-b", PromptType::Text),
            version("b", &[]),
        )
        .await
        .unwrap();
        create(
            &db,
            project,
            new_prompt("other", PromptType::Text),
            version("c", &[]),
        )
        .await
        .unwrap();

        async fn names(db: &MetaDb, project: ProjectId, filter: PromptFilter) -> Vec<String> {
            list_prompts(db.reader(), project, &filter)
                .await
                .unwrap()
                .into_iter()
                .map(|prompt| prompt.name)
                .collect()
        }

        let by_q = PromptFilter {
            q: Some("SUPPORT".to_owned()),
            ..PromptFilter::default()
        };
        assert_eq!(names(&db, project, by_q).await, ["support-a", "support-b"]);
        let by_wildcard = PromptFilter {
            q: Some("%".to_owned()),
            ..PromptFilter::default()
        };
        assert!(names(&db, project, by_wildcard).await.is_empty());
        let by_label = PromptFilter {
            label: Some("production".to_owned()),
            ..PromptFilter::default()
        };
        assert_eq!(names(&db, project, by_label).await, ["support-a"]);
        let by_tag = PromptFilter {
            tag: Some("x".to_owned()),
            ..PromptFilter::default()
        };
        assert_eq!(names(&db, project, by_tag).await, ["support-a"]);
    }

    #[tokio::test]
    async fn paging() {
        let (_dir, db) = crate::meta::test_db().await;
        let org = org(&db).await;
        let project = project(&db, org).await;
        let (prompt, _) = create(
            &db,
            project,
            new_prompt("p", PromptType::Text),
            version("one", &["production"]),
        )
        .await
        .unwrap();
        let id = prompt.id;
        add(&db, id, version("two", &["staging"])).await.unwrap();
        add(&db, id, version("three", &["production"]))
            .await
            .unwrap();
        add(&db, id, version("four", &[])).await.unwrap();
        add(&db, id, version("five", &[])).await.unwrap();
        set(&db, id, "staging", 4).await.unwrap();
        set(&db, id, "staging", 5).await.unwrap();

        let newest: Vec<i64> = list_versions(db.reader(), id, None, 2)
            .await
            .unwrap()
            .iter()
            .map(|row| row.version)
            .collect();
        assert_eq!(newest, [5, 4, 3]);
        let older: Vec<i64> = list_versions(db.reader(), id, Some(3), 10)
            .await
            .unwrap()
            .iter()
            .map(|row| row.version)
            .collect();
        assert_eq!(older, [2, 1]);

        let first_page = list_label_events(db.reader(), id, None, 2).await.unwrap();
        assert_eq!(first_page.len(), 3);
        let cursor = first_page[1].id;
        let second_page = list_label_events(db.reader(), id, Some(cursor), 10)
            .await
            .unwrap();
        assert_eq!(second_page.len(), 3);
        assert!(second_page.iter().all(|event| event.id < cursor));
        assert_eq!(event_count(&db, id).await, 5);
    }

    #[tokio::test]
    async fn protected_labels_default_and_set() {
        let (_dir, db) = crate::meta::test_db().await;
        let org = org(&db).await;
        let project = project(&db, org).await;
        let defaults: Vec<String> = DEFAULT_PROTECTED.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(
            protected_labels(db.reader(), project).await.unwrap(),
            defaults
        );

        db.write(move |conn| {
            Box::pin(async move {
                let labels = vec!["prod".to_owned(), "live".to_owned()];
                set_protected_labels(conn, project, &labels, Some("user-1"), NOW).await
            })
        })
        .await
        .unwrap();
        assert_eq!(
            protected_labels(db.reader(), project).await.unwrap(),
            ["prod", "live"]
        );

        db.write(move |conn| {
            Box::pin(async move { set_protected_labels(conn, project, &[], None, NOW).await })
        })
        .await
        .unwrap();
        assert!(
            protected_labels(db.reader(), project)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn debug_prints_no_content() {
        let row = VersionRow {
            prompt_id: Uuid::now_v7(),
            version: 1,
            template: "canary-debug".to_owned(),
            config: "{}".to_owned(),
            commit_message: None,
            labels: Vec::new(),
            created_by: None,
            created_at: NOW,
        };
        let new = version("canary-debug", &[]);
        assert!(!format!("{row:?}").contains("canary-debug"));
        assert!(!format!("{new:?}").contains("canary-debug"));
    }

    #[tokio::test]
    async fn get_prompt_tx_matches_the_pool_read() {
        let (_dir, db) = crate::meta::test_db().await;
        let org = org(&db).await;
        let project = project(&db, org).await;
        create(
            &db,
            project,
            new_prompt("p", PromptType::Chat),
            version("[]", &["production"]),
        )
        .await
        .unwrap();
        let from_tx = db
            .write(move |conn| Box::pin(async move { get_prompt_tx(conn, project, "p").await }))
            .await
            .unwrap();
        assert_eq!(
            from_tx,
            get_prompt(db.reader(), project, "p").await.unwrap()
        );
    }
}
