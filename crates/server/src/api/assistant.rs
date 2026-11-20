//! Assistant conversations: their list, their detail and the rename body (spec 09, spec 04
//! § Assistant, D-AS-10). A conversation is private to its author.

use std::fmt;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// A conversation in the list (D-AS-10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AssistantConversation {
    /// Conversation id.
    pub id: Uuid,
    /// Empty until the first message or a rename; the UI shows "New chat".
    pub title: String,
    /// Messages stored in the conversation.
    pub message_count: u32,
    /// Creation time, nanoseconds since the Unix epoch. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
    /// Last message, nanoseconds since the Unix epoch. JSON is RFC 3339. A rename does not change it.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub last_active_at: i64,
}

/// A conversation with its messages, oldest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AssistantConversationDetail {
    /// Conversation id.
    pub id: Uuid,
    /// Empty until the first message or a rename; the UI shows "New chat".
    pub title: String,
    /// Messages stored in the conversation.
    pub message_count: u32,
    /// Creation time, nanoseconds since the Unix epoch. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
    /// Last message, nanoseconds since the Unix epoch. JSON is RFC 3339. A rename does not change it.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub last_active_at: i64,
    /// The messages, oldest first.
    pub messages: Vec<AssistantMessage>,
}

/// Who wrote a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AssistantRole {
    /// The signed-in user.
    User,
    /// The assistant.
    Assistant,
    /// A tool's result.
    Tool,
}

/// How a message ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AssistantMessageStatus {
    /// Finished.
    Complete,
    /// Stopped by the user.
    Cancelled,
    /// Ended with an error.
    Failed,
}

/// The user's rating of an assistant message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AssistantFeedback {
    /// Good answer.
    Up,
    /// Bad answer.
    Down,
}

/// The kind of a draft the assistant proposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AssistantDraftKind {
    /// A trace filter.
    Filter,
    /// A SQL query.
    Sql,
    /// A dashboard.
    Dashboard,
    /// An evaluator.
    Evaluator,
    /// An online rule.
    OnlineRule,
}

/// One part of a message (spec 04 § Assistant).
#[derive(Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantPart {
    /// Plain text.
    Text {
        /// The text.
        content: String,
    },
    /// A tool the assistant called.
    ToolCall {
        /// The call's id, unique within the message.
        id: String,
        /// The tool's name.
        name: String,
        /// The arguments the assistant passed.
        #[schema(value_type = Object)]
        arguments: serde_json::Value,
    },
    /// The answer of a tool call.
    ToolResult {
        /// The id of the call this answers.
        id: String,
        /// The tool's name.
        name: String,
        /// Whether the tool succeeded.
        ok: bool,
        /// The tool's answer; omitted once stubbed (D-AS-12).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Option<Object>)]
        result: Option<serde_json::Value>,
        /// Size of the answer in bytes.
        size_bytes: u64,
        /// Rows in the answer, when it has rows.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rows: Option<u64>,
        /// Whether the answer was replaced by a stub (D-AS-12).
        stubbed: bool,
    },
    /// A draft the assistant proposes, for the user to apply.
    Draft {
        /// The draft's id, unique within the message.
        id: String,
        /// What the draft is.
        kind: AssistantDraftKind,
        /// The draft's content.
        #[schema(value_type = Object)]
        draft: serde_json::Value,
    },
}

/// One message of a conversation.
#[derive(Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AssistantMessage {
    /// Position in the conversation, from 1.
    pub seq: u32,
    /// Who wrote it.
    pub role: AssistantRole,
    /// The message's parts, in order.
    pub parts: Vec<AssistantPart>,
    /// The page context sent with a user message (D-AS-14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub context: Option<serde_json::Value>,
    /// How the message ended.
    pub status: AssistantMessageStatus,
    /// The user's rating, when given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<AssistantFeedback>,
    /// The turn's trace, 32 lowercase hex characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_trace_id: Option<String>,
    /// Creation time, nanoseconds since the Unix epoch. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
}

/// `PATCH …/assistant/conversations/{id}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateAssistantConversationRequest {
    /// 1–80 characters after trimming.
    pub title: String,
}

/// `PUT …/assistant/conversations/{id}/messages/{seq}/feedback`: the rating of an assistant message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AssistantFeedbackRequest {
    /// `up` or `down`. `null` clears the rating.
    pub feedback: Option<AssistantFeedback>,
}

/// `POST …/assistant/conversations/{id}/turns`: one user message and its page context.
#[derive(Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AssistantTurnRequest {
    /// 1–16,384 bytes after trimming (D-AS-15).
    pub message: String,
    /// The page context (D-AS-14): a JSON object with a string `page` (1–64 characters), at most
    /// 8 KiB as compact JSON.
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    pub context: Option<serde_json::Value>,
}

/// The org's assistant setting (D-AS-3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AssistantSettings {
    /// Whether the assistant is on for the org.
    pub enabled: bool,
    /// The org's LLM connection the assistant calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<Uuid>,
    /// The model the assistant calls on that connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// When the setting was last saved. Absent while the org never saved one.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub updated_at: Option<i64>,
}

/// `PUT /orgs/{org}/assistant`: the whole setting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PutAssistantSettings {
    /// Turns the assistant on or off. Turning it on runs the tool-calling test (D-AS-24).
    pub enabled: bool,
    /// Required when `enabled` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<Uuid>,
    /// Required when `enabled` is true. 1 to 256 bytes after trimming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// `POST /orgs/{org}/assistant/test`: runs the tool-calling test without saving anything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AssistantTestRequest {
    /// The org's connection to test.
    pub connection_id: Uuid,
    /// The model to test. 1 to 256 bytes after trimming.
    pub model: String,
}

/// Why the caller cannot use the assistant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AssistantUnavailable {
    /// The caller is a viewer, or an implicit demo visitor.
    Forbidden,
    /// The server runs in restore mode (D-AS-16).
    RestoreMode,
    /// An org admin has not turned the assistant on.
    Disabled,
    /// The assistant is on, but its connection is gone.
    NoConnection,
}

/// `GET /projects/{project}/assistant`: whether the caller can chat, and why not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AssistantStatus {
    /// Whether the caller can chat now.
    pub usable: bool,
    /// Why the caller cannot chat. Omitted when `usable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<AssistantUnavailable>,
    /// The connection's name, when `usable`, for the panel's notice (D-AS-18).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_name: Option<String>,
    /// The connection's adapter, when `usable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    /// The model, when `usable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// The size of a JSON value as compact JSON, in bytes. Zero when it cannot be written.
fn json_len(value: &serde_json::Value) -> usize {
    serde_json::to_string(value).map_or(0, |text| text.len())
}

/// Prints the size of the message and whether the context is sent, never the text or the context
/// (AGENTS.md § Privacy).
impl fmt::Debug for AssistantTurnRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AssistantTurnRequest")
            .field("message_bytes", &self.message.len())
            .field("has_context", &self.context.is_some())
            .field("context_bytes", &self.context.as_ref().map_or(0, json_len))
            .finish()
    }
}

/// Prints the variant, the ids, names and sizes, never the text, the arguments, the result or the
/// draft (AGENTS.md § Privacy).
impl fmt::Debug for AssistantPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text { content } => f
                .debug_struct("Text")
                .field("bytes", &content.len())
                .finish(),
            Self::ToolCall {
                id,
                name,
                arguments,
            } => f
                .debug_struct("ToolCall")
                .field("id", id)
                .field("name", name)
                .field("arguments_bytes", &json_len(arguments))
                .finish(),
            Self::ToolResult {
                id,
                name,
                ok,
                result,
                size_bytes,
                rows,
                stubbed,
            } => f
                .debug_struct("ToolResult")
                .field("id", id)
                .field("name", name)
                .field("ok", ok)
                .field("has_result", &result.is_some())
                .field("size_bytes", size_bytes)
                .field("rows", rows)
                .field("stubbed", stubbed)
                .finish(),
            Self::Draft { id, kind, draft } => f
                .debug_struct("Draft")
                .field("id", id)
                .field("kind", kind)
                .field("draft_bytes", &json_len(draft))
                .finish(),
        }
    }
}

/// Prints the position, the role, the status and the number of parts, never the parts or the
/// context (AGENTS.md § Privacy).
impl fmt::Debug for AssistantMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AssistantMessage")
            .field("seq", &self.seq)
            .field("role", &self.role)
            .field("status", &self.status)
            .field("parts_len", &self.parts.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const CANARY: &str = "canary-payload-7f3a";

    #[test]
    fn debug_holds_no_payload() {
        let request = AssistantTurnRequest {
            message: CANARY.to_owned(),
            context: Some(json!({"page": "traces", "filter": CANARY})),
        };
        let parts = vec![
            AssistantPart::Text {
                content: CANARY.to_owned(),
            },
            AssistantPart::ToolCall {
                id: "call_1".to_owned(),
                name: "query_sql".to_owned(),
                arguments: json!({"query": CANARY}),
            },
            AssistantPart::ToolResult {
                id: "call_1".to_owned(),
                name: "query_sql".to_owned(),
                ok: true,
                result: Some(json!({"rows": [CANARY]})),
                size_bytes: 21,
                rows: Some(1),
                stubbed: false,
            },
            AssistantPart::Draft {
                id: "draft_call_1".to_owned(),
                kind: AssistantDraftKind::Sql,
                draft: json!({"sql": CANARY}),
            },
        ];
        let message = AssistantMessage {
            seq: 2,
            role: AssistantRole::Assistant,
            parts: parts.clone(),
            context: Some(json!(CANARY)),
            status: AssistantMessageStatus::Complete,
            feedback: None,
            turn_trace_id: None,
            created_at: 1,
        };

        let mut texts = vec![format!("{request:?}"), format!("{message:?}")];
        texts.extend(parts.iter().map(|part| format!("{part:?}")));
        for text in &texts {
            assert!(!text.contains(CANARY), "{text}");
        }
        assert!(format!("{request:?}").contains(&format!("message_bytes: {}", CANARY.len())));
        assert!(format!("{message:?}").contains("parts_len: 4"));
    }
}
