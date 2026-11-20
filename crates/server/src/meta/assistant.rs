//! Assistant conversations and messages (track AS, D-AS-10, D-AS-12).

use std::fmt;

use cetra_core::ids::{OrgId, ProjectId};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::MetaError;

/// Conversations a user keeps per project; creating one more deletes the least recently active (D-AS-10).
pub(crate) const CONVERSATIONS_PER_USER: i64 = 100;
/// Tool messages stubbed per call of [`stub_tool_results`].
pub(crate) const STUB_BATCH: i64 = 500;
/// Messages per conversation (D-AS-10).
pub(crate) const MESSAGES_PER_CONVERSATION: i64 = 200;
/// The `409` detail when a conversation is full.
pub(crate) const TOO_MANY_MESSAGES: &str = "the conversation is full; start a new one";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConversationRow {
    pub(crate) id: Uuid,
    pub(crate) title: String,
    pub(crate) message_count: i64,
    pub(crate) created_at: i64,
    pub(crate) last_active_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageRole {
    User,
    Assistant,
    Tool,
} // stored "user" | "assistant" | "tool"

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageStatus {
    Complete,
    Cancelled,
    Failed,
} // stored "complete" | "cancelled" | "failed"

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Feedback {
    Up,
    Down,
} // stored "up" | "down"

/// A stored message. `parts` and `context` are JSON text, parsed by the route (AS1.3).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct MessageRow {
    pub(crate) seq: i64,
    pub(crate) role: MessageRole,
    pub(crate) parts: String,
    pub(crate) context: Option<String>,
    pub(crate) status: MessageStatus,
    pub(crate) feedback: Option<Feedback>,
    pub(crate) turn_trace_id: Option<String>,
    pub(crate) created_at: i64,
}

/// Prints the shape of a message, never its text: `parts` and `context` hold user text and
/// payload content (AGENTS.md § Privacy).
impl fmt::Debug for MessageRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageRow")
            .field("seq", &self.seq)
            .field("role", &self.role)
            .field("status", &self.status)
            .field("parts_len", &self.parts.len())
            .field("has_context", &self.context.is_some())
            .finish()
    }
}

impl MessageRole {
    fn parse(text: &str) -> Result<Self, MetaError> {
        match text {
            "user" => Ok(Self::User),
            "assistant" => Ok(Self::Assistant),
            "tool" => Ok(Self::Tool),
            _ => Err(corrupt_message("role")),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

impl MessageStatus {
    fn parse(text: &str) -> Result<Self, MetaError> {
        match text {
            "complete" => Ok(Self::Complete),
            "cancelled" => Ok(Self::Cancelled),
            "failed" => Ok(Self::Failed),
            _ => Err(corrupt_message("status")),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

impl Feedback {
    fn parse(text: &str) -> Result<Self, MetaError> {
        match text {
            "up" => Ok(Self::Up),
            "down" => Ok(Self::Down),
            _ => Err(corrupt_message("feedback")),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

/// A message to store. `parts` and `context` are JSON text.
pub(crate) struct NewMessage {
    pub(crate) role: MessageRole,
    pub(crate) parts: String,
    pub(crate) context: Option<String>,
    pub(crate) status: MessageStatus,
    pub(crate) turn_trace_id: Option<String>,
    pub(crate) stub_after: Option<i64>,
}

/// Prints the shape of a message, never its text (AGENTS.md § Privacy).
impl fmt::Debug for NewMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewMessage")
            .field("role", &self.role)
            .field("status", &self.status)
            .field("parts_len", &self.parts.len())
            .field("has_context", &self.context.is_some())
            .field("has_trace", &self.turn_trace_id.is_some())
            .field("stub_after", &self.stub_after)
            .finish()
    }
}

/// What [`set_feedback`] found and did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FeedbackOutcome {
    Set,
    NotFound,
    NotAnAssistantMessage,
}

/// A `assistant_conversations` row as SQLite returns it.
#[derive(Debug, sqlx::FromRow)]
struct ConversationRecord {
    id: String,
    title: String,
    message_count: i64,
    created_at: i64,
    last_active_at: i64,
}

/// A `assistant_messages` row as SQLite returns it.
#[derive(Debug, sqlx::FromRow)]
struct MessageRecord {
    seq: i64,
    role: String,
    parts: String,
    context: Option<String>,
    status: String,
    feedback: Option<String>,
    turn_trace_id: Option<String>,
    created_at: i64,
}

fn corrupt_conversation(column: &'static str) -> MetaError {
    MetaError::Corrupt {
        table: "assistant_conversations",
        column,
    }
}

fn corrupt_message(column: &'static str) -> MetaError {
    MetaError::Corrupt {
        table: "assistant_messages",
        column,
    }
}

fn conversation_from(record: ConversationRecord) -> Result<ConversationRow, MetaError> {
    Ok(ConversationRow {
        id: Uuid::parse_str(&record.id).map_err(|_| corrupt_conversation("id"))?,
        title: record.title,
        message_count: record.message_count,
        created_at: record.created_at,
        last_active_at: record.last_active_at,
    })
}

fn message_from(record: MessageRecord) -> Result<MessageRow, MetaError> {
    Ok(MessageRow {
        seq: record.seq,
        role: MessageRole::parse(&record.role)?,
        parts: record.parts,
        context: record.context,
        status: MessageStatus::parse(&record.status)?,
        feedback: record
            .feedback
            .as_deref()
            .map(Feedback::parse)
            .transpose()?,
        turn_trace_id: record.turn_trace_id,
        created_at: record.created_at,
    })
}

/// The conversation `id` of `project` and `user`, read on `conn`. Another user's or another
/// project's conversation is `None`.
async fn fetch_conversation(
    conn: &mut SqliteConnection,
    project: ProjectId,
    user: Uuid,
    id: Uuid,
) -> Result<Option<ConversationRow>, MetaError> {
    let record: Option<ConversationRecord> = sqlx::query_as(
        "SELECT id, title, message_count, created_at, last_active_at \
         FROM assistant_conversations WHERE id = ? AND project_id = ? AND user_id = ?",
    )
    .bind(id.to_string())
    .bind(project.to_string())
    .bind(user.to_string())
    .fetch_optional(&mut *conn)
    .await?;
    record.map(conversation_from).transpose()
}

/// Inserts a conversation for `user` in `project` with an empty title, first evicting the
/// least recently active conversations of that user and project until fewer than
/// [`CONVERSATIONS_PER_USER`] remain (D-AS-10).
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails or the new row cannot be read back.
pub(crate) async fn insert_conversation(
    conn: &mut SqliteConnection,
    id: Uuid,
    project: ProjectId,
    user: Uuid,
    now: i64,
) -> Result<ConversationRow, MetaError> {
    let project_text = project.to_string();
    let user_text = user.to_string();
    let mut count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM assistant_conversations WHERE project_id = ? AND user_id = ?",
    )
    .bind(&project_text)
    .bind(&user_text)
    .fetch_one(&mut *conn)
    .await?;
    while count >= CONVERSATIONS_PER_USER {
        let result = sqlx::query(
            "DELETE FROM assistant_conversations WHERE id = (\
               SELECT id FROM assistant_conversations WHERE project_id = ? AND user_id = ? \
               ORDER BY last_active_at ASC, id ASC LIMIT 1)",
        )
        .bind(&project_text)
        .bind(&user_text)
        .execute(&mut *conn)
        .await?;
        if result.rows_affected() == 0 {
            // The count said there are rows, so this is unreachable inside one transaction.
            break;
        }
        count -= 1;
    }
    sqlx::query(
        "INSERT INTO assistant_conversations \
         (id, project_id, user_id, title, message_count, created_at, last_active_at) \
         VALUES (?, ?, ?, '', 0, ?, ?)",
    )
    .bind(id.to_string())
    .bind(&project_text)
    .bind(&user_text)
    .bind(now)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    fetch_conversation(conn, project, user, id)
        .await?
        .ok_or_else(|| corrupt_conversation("id"))
}

/// The conversations of `user` in `project`, most recently active first, ties by id descending.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn list_conversations(
    pool: &SqlitePool,
    project: ProjectId,
    user: Uuid,
) -> Result<Vec<ConversationRow>, MetaError> {
    let records: Vec<ConversationRecord> = sqlx::query_as(
        "SELECT id, title, message_count, created_at, last_active_at \
         FROM assistant_conversations WHERE project_id = ? AND user_id = ? \
         ORDER BY last_active_at DESC, id DESC",
    )
    .bind(project.to_string())
    .bind(user.to_string())
    .fetch_all(pool)
    .await?;
    records.into_iter().map(conversation_from).collect()
}

/// The conversation `id` of `project` and `user`. Another user's or another project's id is `None`.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn get_conversation(
    pool: &SqlitePool,
    project: ProjectId,
    user: Uuid,
    id: Uuid,
) -> Result<Option<ConversationRow>, MetaError> {
    let mut conn = pool.acquire().await?;
    fetch_conversation(&mut conn, project, user, id).await
}

/// Sets the title of the conversation `id` of `project` and `user`. The caller has checked the
/// title. Does not change `last_active_at`. `None` when no conversation of the user matches.
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails or a stored value does not decode.
pub(crate) async fn rename_conversation(
    conn: &mut SqliteConnection,
    project: ProjectId,
    user: Uuid,
    id: Uuid,
    title: &str,
) -> Result<Option<ConversationRow>, MetaError> {
    let result = sqlx::query(
        "UPDATE assistant_conversations SET title = ? \
         WHERE id = ? AND project_id = ? AND user_id = ?",
    )
    .bind(title)
    .bind(id.to_string())
    .bind(project.to_string())
    .bind(user.to_string())
    .execute(&mut *conn)
    .await?;
    if result.rows_affected() == 0 {
        return Ok(None);
    }
    fetch_conversation(conn, project, user, id).await
}

/// Deletes the conversation `id` of `project` and `user`. Its messages go by cascade. Returns
/// whether a row was deleted.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the delete fails.
pub(crate) async fn delete_conversation(
    conn: &mut SqliteConnection,
    project: ProjectId,
    user: Uuid,
    id: Uuid,
) -> Result<bool, MetaError> {
    let result = sqlx::query(
        "DELETE FROM assistant_conversations WHERE id = ? AND project_id = ? AND user_id = ?",
    )
    .bind(id.to_string())
    .bind(project.to_string())
    .bind(user.to_string())
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// The messages of `conversation` in sequence order. Callers check ownership with
/// [`get_conversation`] first.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
async fn list_messages(
    pool: &SqlitePool,
    conversation: Uuid,
) -> Result<Vec<MessageRow>, MetaError> {
    let records: Vec<MessageRecord> = sqlx::query_as(
        "SELECT seq, role, parts, context, status, feedback, turn_trace_id, created_at \
         FROM assistant_messages WHERE conversation_id = ? ORDER BY seq ASC",
    )
    .bind(conversation.to_string())
    .fetch_all(pool)
    .await?;
    records.into_iter().map(message_from).collect()
}

/// The conversation `id` of `project` and `user` with its messages in sequence order. `None`
/// when it is not theirs.
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails or a stored value does not decode.
pub(crate) async fn get_conversation_with_messages(
    pool: &SqlitePool,
    project: ProjectId,
    user: Uuid,
    id: Uuid,
) -> Result<Option<(ConversationRow, Vec<MessageRow>)>, MetaError> {
    let Some(row) = get_conversation(pool, project, user, id).await? else {
        return Ok(None);
    };
    let messages = list_messages(pool, id).await?;
    Ok(Some((row, messages)))
}

/// Refuses with [`MetaError::Conflict`] and [`TOO_MANY_MESSAGES`] unless `conversation` has room
/// for `needed` more messages (D-AS-25). Call it inside the write that appends them, so the count
/// is read in the same transaction as the append.
///
/// # Errors
///
/// Returns [`MetaError::Conflict`] when the messages would not fit, [`MetaError::Corrupt`] when the
/// conversation has no count, or [`MetaError::Db`] when the query fails.
pub(crate) async fn ensure_room(
    conn: &mut SqliteConnection,
    conversation: Uuid,
    needed: i64,
) -> Result<(), MetaError> {
    let count: Option<i64> =
        sqlx::query_scalar("SELECT message_count FROM assistant_conversations WHERE id = ?")
            .bind(conversation.to_string())
            .fetch_optional(&mut *conn)
            .await?;
    let count = count.ok_or_else(|| corrupt_conversation("message_count"))?;
    if count + needed > MESSAGES_PER_CONVERSATION {
        return Err(MetaError::Conflict(TOO_MANY_MESSAGES));
    }
    Ok(())
}

/// Appends a message to `conversation` and returns its `seq`. Read the count, refuse at
/// [`MESSAGES_PER_CONVERSATION`] (a conflict), then insert the message and set the count and
/// `last_active_at = now`.
///
/// # Errors
///
/// Returns [`MetaError::Conflict`] with [`TOO_MANY_MESSAGES`] when the conversation is full,
/// [`MetaError::Corrupt`] when the conversation has no count, or [`MetaError::Db`] when a
/// query fails.
pub(crate) async fn append_message(
    conn: &mut SqliteConnection,
    conversation: Uuid,
    message: NewMessage,
    now: i64,
) -> Result<i64, MetaError> {
    let count: Option<i64> =
        sqlx::query_scalar("SELECT message_count FROM assistant_conversations WHERE id = ?")
            .bind(conversation.to_string())
            .fetch_optional(&mut *conn)
            .await?;
    let count = count.ok_or_else(|| corrupt_conversation("message_count"))?;
    if count >= MESSAGES_PER_CONVERSATION {
        return Err(MetaError::Conflict(TOO_MANY_MESSAGES));
    }
    let seq = count + 1;
    sqlx::query(
        "INSERT INTO assistant_messages \
         (conversation_id, seq, role, parts, context, status, turn_trace_id, stub_after, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(conversation.to_string())
    .bind(seq)
    .bind(message.role.as_str())
    .bind(&message.parts)
    .bind(message.context.as_deref())
    .bind(message.status.as_str())
    .bind(message.turn_trace_id.as_deref())
    .bind(message.stub_after)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE assistant_conversations SET message_count = ?, last_active_at = ? WHERE id = ?",
    )
    .bind(seq)
    .bind(now)
    .bind(conversation.to_string())
    .execute(&mut *conn)
    .await?;
    Ok(seq)
}

/// Sets the title of `conversation` when it has none (`title = ''`). Otherwise does nothing.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the update fails.
pub(crate) async fn set_title_if_empty(
    conn: &mut SqliteConnection,
    conversation: Uuid,
    title: &str,
) -> Result<(), MetaError> {
    sqlx::query("UPDATE assistant_conversations SET title = ? WHERE id = ? AND title = ''")
        .bind(title)
        .bind(conversation.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Sets or clears (`None`) the feedback of the message `seq` of `conversation`. The conversation
/// must be `user`'s in `project`; the message must be an assistant message (D-AS-10).
///
/// # Errors
///
/// Returns [`MetaError`] when a query fails or a stored value does not decode.
pub(crate) async fn set_feedback(
    conn: &mut SqliteConnection,
    project: ProjectId,
    user: Uuid,
    conversation: Uuid,
    seq: i64,
    feedback: Option<Feedback>,
) -> Result<FeedbackOutcome, MetaError> {
    if fetch_conversation(conn, project, user, conversation)
        .await?
        .is_none()
    {
        return Ok(FeedbackOutcome::NotFound);
    }
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM assistant_messages WHERE conversation_id = ? AND seq = ?",
    )
    .bind(conversation.to_string())
    .bind(seq)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(role) = role else {
        return Ok(FeedbackOutcome::NotFound);
    };
    if role != MessageRole::Assistant.as_str() {
        return Ok(FeedbackOutcome::NotAnAssistantMessage);
    }
    sqlx::query("UPDATE assistant_messages SET feedback = ? WHERE conversation_id = ? AND seq = ?")
        .bind(feedback.map(Feedback::as_str))
        .bind(conversation.to_string())
        .bind(seq)
        .execute(&mut *conn)
        .await?;
    Ok(FeedbackOutcome::Set)
}

/// Deletes the conversations whose `last_active_at` is before `idle_before` (nanoseconds).
/// Returns how many were deleted.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when the delete fails.
pub(crate) async fn expire_conversations(
    conn: &mut SqliteConnection,
    idle_before: i64,
) -> Result<u64, MetaError> {
    let result = sqlx::query("DELETE FROM assistant_conversations WHERE last_active_at < ?")
        .bind(idle_before)
        .execute(&mut *conn)
        .await?;
    Ok(result.rows_affected())
}

/// Replaces the tool results of up to [`STUB_BATCH`] tool messages whose `stub_after` is at or
/// before `now` with stubs, and clears their `stub_after`. Returns how many messages were
/// stubbed. A message whose `parts` is not a JSON array gets `[]` instead, so one bad row
/// cannot stall the job.
///
/// # Errors
///
/// Returns [`MetaError::Db`] when a query fails.
pub(crate) async fn stub_tool_results(
    conn: &mut SqliteConnection,
    now: i64,
) -> Result<u64, MetaError> {
    let due: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT conversation_id, seq, parts FROM assistant_messages \
         WHERE stub_after IS NOT NULL AND stub_after <= ? ORDER BY stub_after LIMIT ?",
    )
    .bind(now)
    .bind(STUB_BATCH)
    .fetch_all(&mut *conn)
    .await?;
    let mut stubbed = 0;
    for (conversation_id, seq, parts) in due {
        let parts = stubbed_parts(&conversation_id, seq, &parts);
        let result = sqlx::query(
            "UPDATE assistant_messages SET parts = ?, stub_after = NULL \
             WHERE conversation_id = ? AND seq = ?",
        )
        .bind(parts)
        .bind(&conversation_id)
        .bind(seq)
        .execute(&mut *conn)
        .await?;
        stubbed += result.rows_affected();
    }
    Ok(stubbed)
}

/// `parts` with every `tool_result` part's `result` set to `null` and `stubbed` to `true`.
/// Other keys and parts stay as they are. Unparseable `parts` become `[]`.
fn stubbed_parts(conversation_id: &str, seq: i64, parts: &str) -> String {
    let mut values: Vec<serde_json::Value> = match serde_json::from_str(parts) {
        Ok(values) => values,
        Err(_) => {
            tracing::warn!(
                conversation_id = %conversation_id,
                seq,
                "assistant message parts did not parse; cleared"
            );
            Vec::new()
        }
    };
    for value in &mut values {
        let Some(object) = value.as_object_mut() else {
            continue;
        };
        if object.get("type").and_then(serde_json::Value::as_str) == Some("tool_result") {
            object.insert("result".to_owned(), serde_json::Value::Null);
            object.insert("stubbed".to_owned(), serde_json::Value::Bool(true));
        }
    }
    // A vector of JSON values always serializes; `[]` is the fallback that keeps the job going.
    serde_json::to_string(&values).unwrap_or_else(|_| "[]".to_owned())
}

/// An org's assistant setting (D-AS-3). An org with no row has the setting off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AssistantSettingsRow {
    pub(crate) enabled: bool,
    pub(crate) connection_id: Option<Uuid>,
    pub(crate) model: Option<String>,
    pub(crate) updated_by: Option<Uuid>,
    pub(crate) updated_at: i64,
}

/// A `org_assistant_settings` row as SQLite returns it.
#[derive(Debug, sqlx::FromRow)]
struct SettingsRecord {
    enabled: i64,
    connection_id: Option<String>,
    model: Option<String>,
    updated_by: Option<String>,
    updated_at: i64,
}

fn corrupt_settings(column: &'static str) -> MetaError {
    MetaError::Corrupt {
        table: "org_assistant_settings",
        column,
    }
}

fn settings_from(record: SettingsRecord) -> Result<AssistantSettingsRow, MetaError> {
    let optional_uuid = |text: Option<String>, column: &'static str| {
        text.map(|text| Uuid::parse_str(&text).map_err(|_| corrupt_settings(column)))
            .transpose()
    };
    Ok(AssistantSettingsRow {
        enabled: match record.enabled {
            0 => false,
            1 => true,
            _ => return Err(corrupt_settings("enabled")),
        },
        connection_id: optional_uuid(record.connection_id, "connection_id")?,
        model: record.model,
        updated_by: optional_uuid(record.updated_by, "updated_by")?,
        updated_at: record.updated_at,
    })
}

/// The setting of `org`, or `None` when the org never saved one.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails or a stored value does not decode.
pub(crate) async fn get_settings(
    pool: &SqlitePool,
    org: OrgId,
) -> Result<Option<AssistantSettingsRow>, MetaError> {
    let record: Option<SettingsRecord> = sqlx::query_as(
        "SELECT enabled, connection_id, model, updated_by, updated_at \
         FROM org_assistant_settings WHERE org_id = ?",
    )
    .bind(org.to_string())
    .fetch_optional(pool)
    .await?;
    record.map(settings_from).transpose()
}

/// Stores the setting of `org`, replacing every column of its row.
///
/// # Errors
///
/// Returns [`MetaError`] when the query fails.
pub(crate) async fn put_settings(
    conn: &mut SqliteConnection,
    org: OrgId,
    row: &AssistantSettingsRow,
) -> Result<(), MetaError> {
    sqlx::query(
        "INSERT INTO org_assistant_settings \
         (org_id, enabled, connection_id, model, updated_by, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(org_id) DO UPDATE SET \
           enabled = excluded.enabled, \
           connection_id = excluded.connection_id, \
           model = excluded.model, \
           updated_by = excluded.updated_by, \
           updated_at = excluded.updated_at",
    )
    .bind(org.to_string())
    .bind(i64::from(row.enabled))
    .bind(row.connection_id.map(|id| id.to_string()))
    .bind(row.model.as_deref())
    .bind(row.updated_by.map(|id| id.to_string()))
    .bind(row.updated_at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn insert_message_raw(
    conn: &mut SqliteConnection,
    conversation: Uuid,
    seq: i64,
    role: &str,
    parts: &str,
    context: Option<&str>,
    stub_after: Option<i64>,
    now: i64,
) -> Result<(), MetaError> {
    sqlx::query(
        "INSERT INTO assistant_messages \
         (conversation_id, seq, role, parts, context, stub_after, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(conversation.to_string())
    .bind(seq)
    .bind(role)
    .bind(parts)
    .bind(context)
    .bind(stub_after)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{MetaDb, orgs, projects, test_db, users};
    use cetra_core::ids::OrgId;

    /// One org, two projects (`support-bot`, `other-bot`), and two users: `owner` and `other`.
    struct World {
        project: ProjectId,
        second: ProjectId,
        owner: Uuid,
        other: Uuid,
        org: String,
    }

    fn new_user(email: &str) -> users::NewUser {
        users::NewUser {
            email: email.to_owned(),
            name: None,
            password_hash: None,
            email_verified_at: None,
            email_verified_via: None,
            is_instance_admin: false,
        }
    }

    async fn world(db: &MetaDb) -> World {
        db.write(|conn| Box::pin(async move { world_on(conn).await }))
            .await
            .unwrap()
    }

    async fn world_on(conn: &mut SqliteConnection) -> Result<World, MetaError> {
        let owner = users::insert_user(conn, new_user("owner@example.com"), 1)
            .await?
            .id;
        let other = users::insert_user(conn, new_user("other@example.com"), 1)
            .await?
            .id;
        let org = orgs::insert_org(conn, OrgId::new_v7(), "Acme", "acme", 1).await?;
        let project = projects::insert_project(conn, org.id, "Support", "support-bot", None, 1)
            .await?
            .id;
        let second = projects::insert_project(conn, org.id, "Other", "other-bot", None, 1)
            .await?
            .id;
        Ok(World {
            project,
            second,
            owner,
            other,
            org: org.id.to_string(),
        })
    }

    async fn create(db: &MetaDb, project: ProjectId, user: Uuid, now: i64) -> ConversationRow {
        db.write(move |conn| {
            Box::pin(
                async move { insert_conversation(conn, Uuid::now_v7(), project, user, now).await },
            )
        })
        .await
        .unwrap()
    }

    async fn set_activity_on(
        conn: &mut SqliteConnection,
        id: Uuid,
        at: i64,
    ) -> Result<(), MetaError> {
        sqlx::query("UPDATE assistant_conversations SET last_active_at = ? WHERE id = ?")
            .bind(at)
            .bind(id.to_string())
            .execute(&mut *conn)
            .await?;
        Ok(())
    }

    async fn set_activity(db: &MetaDb, id: Uuid, at: i64) {
        db.write(move |conn| Box::pin(async move { set_activity_on(conn, id, at).await }))
            .await
            .unwrap();
    }

    /// Raw SQL with no binds, for status and feedback changes the store does not make yet.
    async fn raw_on(conn: &mut SqliteConnection, sql: &'static str) -> Result<(), MetaError> {
        sqlx::query(sql).execute(&mut *conn).await?;
        Ok(())
    }

    async fn list_ids(db: &MetaDb, project: ProjectId, user: Uuid) -> Vec<Uuid> {
        list_conversations(db.reader(), project, user)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect()
    }

    async fn count_conversations(db: &MetaDb, project: ProjectId, user: Uuid) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM assistant_conversations WHERE project_id = ? AND user_id = ?",
        )
        .bind(project.to_string())
        .bind(user.to_string())
        .fetch_one(db.reader())
        .await
        .unwrap()
    }

    async fn count_messages(db: &MetaDb, conversation: Uuid) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM assistant_messages WHERE conversation_id = ?")
            .bind(conversation.to_string())
            .fetch_one(db.reader())
            .await
            .unwrap()
    }

    /// The raw `parts` text and `stub_after` of one message.
    async fn raw_message(db: &MetaDb, conversation: Uuid, seq: i64) -> (String, Option<i64>) {
        sqlx::query_as(
            "SELECT parts, stub_after FROM assistant_messages \
             WHERE conversation_id = ? AND seq = ?",
        )
        .bind(conversation.to_string())
        .bind(seq)
        .fetch_one(db.reader())
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn insert_then_get_round_trips() {
        let (_dir, db) = test_db().await;
        let w = world(&db).await;
        let created = create(&db, w.project, w.owner, 10).await;
        assert_eq!(created.title, "");
        assert_eq!(created.message_count, 0);
        assert_eq!(created.created_at, 10);
        assert_eq!(created.last_active_at, 10);
        let fetched = get_conversation(db.reader(), w.project, w.owner, created.id)
            .await
            .unwrap();
        assert_eq!(fetched, Some(created));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_is_scoped_to_owner_and_project() {
        let (_dir, db) = test_db().await;
        let w = world(&db).await;
        let created = create(&db, w.project, w.owner, 10).await;
        let by_other = get_conversation(db.reader(), w.project, w.other, created.id)
            .await
            .unwrap();
        assert_eq!(by_other, None);
        let other_project = get_conversation(db.reader(), w.second, w.owner, created.id)
            .await
            .unwrap();
        assert_eq!(other_project, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_is_newest_activity_first() {
        let (_dir, db) = test_db().await;
        let w = world(&db).await;
        let a = create(&db, w.project, w.owner, 10).await;
        let b = create(&db, w.project, w.owner, 20).await;
        let c = create(&db, w.project, w.owner, 30).await;
        set_activity(&db, a.id, 40).await;
        let listed = list_ids(&db, w.project, w.owner).await;
        assert_eq!(listed, vec![a.id, c.id, b.id]);

        // Equal activity: id descending.
        set_activity(&db, b.id, 60).await;
        set_activity(&db, c.id, 60).await;
        let listed = list_ids(&db, w.project, w.owner).await;
        assert_eq!(listed[2], a.id);
        assert!(listed[0] > listed[1]);
        assert!([b.id, c.id].contains(&listed[0]) && [b.id, c.id].contains(&listed[1]));

        // Another user's rows are absent.
        create(&db, w.project, w.other, 100).await;
        let listed = list_conversations(db.reader(), w.project, w.owner)
            .await
            .unwrap();
        assert_eq!(listed.len(), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_101st_conversation_evicts_the_least_recently_active() {
        let (_dir, db) = test_db().await;
        let w = world(&db).await;
        let foreign = create(&db, w.project, w.other, 5).await;
        let first = create(&db, w.project, w.owner, 1).await;
        let second = create(&db, w.project, w.owner, 2).await;
        for now in 3..=100 {
            create(&db, w.project, w.owner, now).await;
        }
        assert_eq!(count_conversations(&db, w.project, w.owner).await, 100);
        set_activity(&db, first.id, 1_000).await;
        create(&db, w.project, w.owner, 1_001).await;

        assert_eq!(count_conversations(&db, w.project, w.owner).await, 100);
        assert_eq!(
            get_conversation(db.reader(), w.project, w.owner, second.id)
                .await
                .unwrap(),
            None
        );
        assert!(
            get_conversation(db.reader(), w.project, w.owner, first.id)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(count_conversations(&db, w.project, w.other).await, 1);
        assert!(
            get_conversation(db.reader(), w.project, w.other, foreign.id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rename_changes_only_the_title() {
        let (_dir, db) = test_db().await;
        let w = world(&db).await;
        let created = create(&db, w.project, w.owner, 10).await;
        let renamed = db
            .write(move |conn| {
                Box::pin(async move {
                    rename_conversation(conn, w.project, w.owner, created.id, "Q3 errors").await
                })
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(renamed.title, "Q3 errors");
        assert_eq!(renamed.last_active_at, 10);
        assert_eq!(renamed.created_at, 10);

        let by_other = db
            .write(move |conn| {
                Box::pin(async move {
                    rename_conversation(conn, w.project, w.other, created.id, "Hacked").await
                })
            })
            .await
            .unwrap();
        assert_eq!(by_other, None);
        let stored = get_conversation(db.reader(), w.project, w.owner, created.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.title, "Q3 errors");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_cascades_messages() {
        let (_dir, db) = test_db().await;
        let w = world(&db).await;
        let created = create(&db, w.project, w.owner, 10).await;
        db.write(move |conn| {
            Box::pin(async move {
                insert_message_raw(conn, created.id, 1, "user", "[]", None, None, 10).await?;
                insert_message_raw(conn, created.id, 2, "assistant", "[]", None, None, 10).await
            })
        })
        .await
        .unwrap();
        assert_eq!(count_messages(&db, created.id).await, 2);

        let by_other = db
            .write(move |conn| {
                Box::pin(
                    async move { delete_conversation(conn, w.project, w.other, created.id).await },
                )
            })
            .await
            .unwrap();
        assert!(!by_other);
        let deleted = db
            .write(move |conn| {
                Box::pin(
                    async move { delete_conversation(conn, w.project, w.owner, created.id).await },
                )
            })
            .await
            .unwrap();
        assert!(deleted);
        assert_eq!(count_messages(&db, created.id).await, 0);
        assert!(
            list_messages(db.reader(), created.id)
                .await
                .unwrap()
                .is_empty()
        );
        let again = db
            .write(move |conn| {
                Box::pin(
                    async move { delete_conversation(conn, w.project, w.owner, created.id).await },
                )
            })
            .await
            .unwrap();
        assert!(!again);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_messages_is_in_seq_order() {
        let (_dir, db) = test_db().await;
        let w = world(&db).await;
        let created = create(&db, w.project, w.owner, 10).await;
        db.write(move |conn| {
            Box::pin(async move {
                insert_message_raw(
                    conn,
                    created.id,
                    2,
                    "user",
                    "[]",
                    Some(r#"{"page":"traces"}"#),
                    None,
                    10,
                )
                .await?;
                insert_message_raw(
                    conn,
                    created.id,
                    1,
                    "user",
                    r#"[{"type":"text","content":"a"}]"#,
                    None,
                    None,
                    10,
                )
                .await?;
                insert_message_raw(conn, created.id, 3, "assistant", "[]", None, None, 10).await
            })
        })
        .await
        .unwrap();
        let messages = list_messages(db.reader(), created.id).await.unwrap();
        let seqs: Vec<i64> = messages.iter().map(|m| m.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        let contexts: Vec<Option<&str>> = messages.iter().map(|m| m.context.as_deref()).collect();
        assert_eq!(contexts, vec![None, Some(r#"{"page":"traces"}"#), None]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn messages_round_trip_every_enum() {
        let (_dir, db) = test_db().await;
        let w = world(&db).await;
        let created = create(&db, w.project, w.owner, 10).await;
        db.write(move |conn| {
            Box::pin(async move {
                insert_message_raw(conn, created.id, 1, "user", "[]", None, None, 10).await?;
                insert_message_raw(conn, created.id, 2, "assistant", "[]", None, None, 10).await?;
                insert_message_raw(conn, created.id, 3, "tool", "[]", None, None, 10).await?;
                raw_on(
                    conn,
                    "UPDATE assistant_messages SET status = 'cancelled', feedback = 'up' WHERE seq = 2",
                )
                .await?;
                raw_on(
                    conn,
                    "UPDATE assistant_messages SET status = 'failed', feedback = 'down' WHERE seq = 3",
                )
                .await
            })
        })
        .await
        .unwrap();
        let messages = list_messages(db.reader(), created.id).await.unwrap();
        let roles: Vec<MessageRole> = messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![MessageRole::User, MessageRole::Assistant, MessageRole::Tool]
        );
        let statuses: Vec<MessageStatus> = messages.iter().map(|m| m.status).collect();
        assert_eq!(
            statuses,
            vec![
                MessageStatus::Complete,
                MessageStatus::Cancelled,
                MessageStatus::Failed
            ]
        );
        let feedback: Vec<Option<Feedback>> = messages.iter().map(|m| m.feedback).collect();
        assert_eq!(
            feedback,
            vec![None, Some(Feedback::Up), Some(Feedback::Down)]
        );
    }

    #[test]
    fn a_bad_enum_value_is_corrupt() {
        assert!(matches!(
            MessageRole::parse("x"),
            Err(MetaError::Corrupt {
                table: "assistant_messages",
                column: "role"
            })
        ));
        assert!(matches!(
            MessageStatus::parse("x"),
            Err(MetaError::Corrupt {
                table: "assistant_messages",
                column: "status"
            })
        ));
        assert!(matches!(
            Feedback::parse("x"),
            Err(MetaError::Corrupt {
                table: "assistant_messages",
                column: "feedback"
            })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn expire_deletes_only_idle_conversations() {
        let (_dir, db) = test_db().await;
        let w = world(&db).await;
        let a = create(&db, w.project, w.owner, 10).await;
        let b = create(&db, w.project, w.owner, 20).await;
        let c = create(&db, w.project, w.owner, 30).await;
        db.write(move |conn| {
            Box::pin(async move {
                insert_message_raw(conn, c.id, 1, "user", "[]", None, None, 30).await
            })
        })
        .await
        .unwrap();
        let expired = db
            .write(|conn| Box::pin(async move { expire_conversations(conn, 25).await }))
            .await
            .unwrap();
        assert_eq!(expired, 2);
        assert_eq!(
            get_conversation(db.reader(), w.project, w.owner, a.id)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            get_conversation(db.reader(), w.project, w.owner, b.id)
                .await
                .unwrap(),
            None
        );
        assert!(
            get_conversation(db.reader(), w.project, w.owner, c.id)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(count_messages(&db, c.id).await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stub_rewrites_only_due_tool_results() {
        let (_dir, db) = test_db().await;
        let w = world(&db).await;
        let created = create(&db, w.project, w.owner, 10).await;
        let first_parts = r#"[{"type":"tool_result","id":"c1","name":"query_sql","ok":true,"result":{"rows":[1]},"size_bytes":12,"rows":1,"stubbed":false},{"type":"text","content":"x"}]"#;
        let second_parts = r#"[{"type":"tool_result","id":"c2","name":"query_sql","ok":true,"result":{"rows":[2]},"size_bytes":12,"rows":1,"stubbed":false}]"#;
        db.write(move |conn| {
            Box::pin(async move {
                insert_message_raw(
                    conn,
                    created.id,
                    1,
                    "tool",
                    first_parts,
                    None,
                    Some(100),
                    10,
                )
                .await?;
                insert_message_raw(
                    conn,
                    created.id,
                    2,
                    "tool",
                    second_parts,
                    None,
                    Some(200),
                    10,
                )
                .await?;
                insert_message_raw(
                    conn,
                    created.id,
                    3,
                    "user",
                    r#"[{"type":"text","content":"hi"}]"#,
                    None,
                    None,
                    10,
                )
                .await
            })
        })
        .await
        .unwrap();

        let stubbed = db
            .write(|conn| Box::pin(async move { stub_tool_results(conn, 150).await }))
            .await
            .unwrap();
        assert_eq!(stubbed, 1);
        let (parts, stub_after) = raw_message(&db, created.id, 1).await;
        let parts: serde_json::Value = serde_json::from_str(&parts).unwrap();
        assert_eq!(
            parts,
            serde_json::json!([
                {"type":"tool_result","id":"c1","name":"query_sql","ok":true,"result":null,"size_bytes":12,"rows":1,"stubbed":true},
                {"type":"text","content":"x"}
            ])
        );
        assert_eq!(stub_after, None);
        assert_eq!(
            raw_message(&db, created.id, 2).await,
            (second_parts.to_owned(), Some(200))
        );
        assert_eq!(
            raw_message(&db, created.id, 3).await,
            (r#"[{"type":"text","content":"hi"}]"#.to_owned(), None)
        );

        let again = db
            .write(|conn| Box::pin(async move { stub_tool_results(conn, 150).await }))
            .await
            .unwrap();
        assert_eq!(again, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stub_clears_unparseable_parts() {
        let (_dir, db) = test_db().await;
        let w = world(&db).await;
        let created = create(&db, w.project, w.owner, 10).await;
        db.write(move |conn| {
            Box::pin(async move {
                insert_message_raw(conn, created.id, 1, "tool", "not json", None, Some(10), 10)
                    .await
            })
        })
        .await
        .unwrap();
        let stubbed = db
            .write(|conn| Box::pin(async move { stub_tool_results(conn, 10).await }))
            .await
            .unwrap();
        assert_eq!(stubbed, 1);
        assert_eq!(
            raw_message(&db, created.id, 1).await,
            ("[]".to_owned(), None)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stub_takes_at_most_a_batch() {
        let (_dir, db) = test_db().await;
        let w = world(&db).await;
        let created = create(&db, w.project, w.owner, 10).await;
        db.write(move |conn| {
            Box::pin(async move {
                for seq in 1..=501_i64 {
                    insert_message_raw(conn, created.id, seq, "tool", "[]", None, Some(10), 10)
                        .await?;
                }
                Ok(())
            })
        })
        .await
        .unwrap();
        let first = db
            .write(|conn| Box::pin(async move { stub_tool_results(conn, 10).await }))
            .await
            .unwrap();
        assert_eq!(first, 500);
        let second = db
            .write(|conn| Box::pin(async move { stub_tool_results(conn, 10).await }))
            .await
            .unwrap();
        assert_eq!(second, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cascades_from_project_org_and_user() {
        let (_dir, db) = test_db().await;
        let w = world(&db).await;

        // The project.
        let in_project = create(&db, w.project, w.owner, 10).await;
        db.write(move |conn| {
            Box::pin(async move {
                insert_message_raw(conn, in_project.id, 1, "user", "[]", None, None, 10).await?;
                sqlx::query("DELETE FROM projects WHERE id = ?")
                    .bind(w.project.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        assert_eq!(count_conversations(&db, w.project, w.owner).await, 0);
        assert_eq!(count_messages(&db, in_project.id).await, 0);

        // The user.
        let by_user = create(&db, w.second, w.other, 10).await;
        db.write(move |conn| {
            Box::pin(async move {
                insert_message_raw(conn, by_user.id, 1, "user", "[]", None, None, 10).await?;
                sqlx::query("DELETE FROM users WHERE id = ?")
                    .bind(w.other.to_string())
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        assert_eq!(count_conversations(&db, w.second, w.other).await, 0);
        assert_eq!(count_messages(&db, by_user.id).await, 0);

        // The org, through its projects.
        let by_org = create(&db, w.second, w.owner, 10).await;
        let org = w.org.clone();
        db.write(move |conn| {
            Box::pin(async move {
                insert_message_raw(conn, by_org.id, 1, "user", "[]", None, None, 10).await?;
                sqlx::query("DELETE FROM orgs WHERE id = ?")
                    .bind(org)
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        assert_eq!(count_conversations(&db, w.second, w.owner).await, 0);
        assert_eq!(count_messages(&db, by_org.id).await, 0);
    }

    #[test]
    fn message_debug_hides_content() {
        let row = MessageRow {
            seq: 1,
            role: MessageRole::User,
            parts: r#"[{"type":"text","content":"secret-text"}]"#.to_owned(),
            context: Some(r#"{"note":"secret-text"}"#.to_owned()),
            status: MessageStatus::Complete,
            feedback: None,
            turn_trace_id: None,
            created_at: 1,
        };
        let text = format!("{row:?}");
        assert!(!text.contains("secret-text"));
        assert!(text.contains("parts_len"));
        assert!(text.contains("has_context: true"));
    }

    #[test]
    fn new_message_debug_hides_content() {
        let message = NewMessage {
            role: MessageRole::Assistant,
            parts: r#"[{"type":"text","content":"secret-text"}]"#.to_owned(),
            context: Some(r#"{"note":"secret-text"}"#.to_owned()),
            status: MessageStatus::Cancelled,
            turn_trace_id: None,
            stub_after: None,
        };
        let text = format!("{message:?}");
        assert!(!text.contains("secret-text"));
        assert!(text.contains("parts_len"));
        assert!(text.contains("has_context: true"));
    }

    fn plain(role: MessageRole) -> NewMessage {
        NewMessage {
            role,
            parts: "[]".to_owned(),
            context: None,
            status: MessageStatus::Complete,
            turn_trace_id: None,
            stub_after: None,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn append_numbers_and_caps() {
        let (_dir, db) = test_db().await;
        let w = world(&db).await;
        let created = create(&db, w.project, w.owner, 10).await;
        let id = created.id;
        let seqs = db
            .write(move |conn| {
                Box::pin(async move {
                    let mut seqs = Vec::new();
                    for _ in 0..MESSAGES_PER_CONVERSATION {
                        seqs.push(append_message(conn, id, plain(MessageRole::User), 20).await?);
                    }
                    Ok(seqs)
                })
            })
            .await
            .unwrap();
        assert_eq!(seqs, (1..=MESSAGES_PER_CONVERSATION).collect::<Vec<i64>>());

        let full = db
            .write(move |conn| {
                Box::pin(
                    async move { append_message(conn, id, plain(MessageRole::User), 30).await },
                )
            })
            .await;
        assert!(matches!(full, Err(MetaError::Conflict(TOO_MANY_MESSAGES))));
        assert_eq!(count_messages(&db, id).await, MESSAGES_PER_CONVERSATION);
        let stored = get_conversation(db.reader(), w.project, w.owner, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.message_count, MESSAGES_PER_CONVERSATION);
        assert_eq!(stored.last_active_at, 20);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn title_is_set_once() {
        let (_dir, db) = test_db().await;
        let w = world(&db).await;
        let created = create(&db, w.project, w.owner, 10).await;
        let id = created.id;
        db.write(move |conn| {
            Box::pin(async move { set_title_if_empty(conn, id, "First question").await })
        })
        .await
        .unwrap();
        db.write(move |conn| Box::pin(async move { set_title_if_empty(conn, id, "Second").await }))
            .await
            .unwrap();
        let stored = get_conversation(db.reader(), w.project, w.owner, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.title, "First question");
    }

    async fn feedback_as(
        db: &MetaDb,
        project: ProjectId,
        user: Uuid,
        conversation: Uuid,
        seq: i64,
        feedback: Option<Feedback>,
    ) -> FeedbackOutcome {
        db.write(move |conn| {
            Box::pin(
                async move { set_feedback(conn, project, user, conversation, seq, feedback).await },
            )
        })
        .await
        .unwrap()
    }

    async fn feedback_of(db: &MetaDb, conversation: Uuid, seq: i64) -> Option<Feedback> {
        list_messages(db.reader(), conversation)
            .await
            .unwrap()
            .into_iter()
            .find(|message| message.seq == seq)
            .unwrap()
            .feedback
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn feedback_outcomes() {
        let (_dir, db) = test_db().await;
        let w = world(&db).await;
        let created = create(&db, w.project, w.owner, 10).await;
        let id = created.id;
        db.write(move |conn| {
            Box::pin(async move {
                insert_message_raw(conn, id, 1, "user", "[]", None, None, 10).await?;
                insert_message_raw(conn, id, 2, "assistant", "[]", None, None, 10).await?;
                insert_message_raw(conn, id, 3, "tool", "[]", None, None, 10).await
            })
        })
        .await
        .unwrap();

        let set = feedback_as(&db, w.project, w.owner, id, 2, Some(Feedback::Up)).await;
        assert_eq!(set, FeedbackOutcome::Set);
        assert_eq!(feedback_of(&db, id, 2).await, Some(Feedback::Up));

        let cleared = feedback_as(&db, w.project, w.owner, id, 2, None).await;
        assert_eq!(cleared, FeedbackOutcome::Set);
        assert_eq!(feedback_of(&db, id, 2).await, None);

        let by_other = feedback_as(&db, w.project, w.other, id, 2, Some(Feedback::Down)).await;
        assert_eq!(by_other, FeedbackOutcome::NotFound);
        assert_eq!(feedback_of(&db, id, 2).await, None);

        let other_project = feedback_as(&db, w.second, w.owner, id, 2, Some(Feedback::Down)).await;
        assert_eq!(other_project, FeedbackOutcome::NotFound);

        let on_tool = feedback_as(&db, w.project, w.owner, id, 3, Some(Feedback::Up)).await;
        assert_eq!(on_tool, FeedbackOutcome::NotAnAssistantMessage);
        assert_eq!(feedback_of(&db, id, 3).await, None);

        let missing = feedback_as(&db, w.project, w.owner, id, 9, Some(Feedback::Up)).await;
        assert_eq!(missing, FeedbackOutcome::NotFound);
    }
}
