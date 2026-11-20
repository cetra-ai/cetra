//! Assistant conversations: their list, their detail and the rename body (spec 09, spec 04
//! § Assistant, D-AS-10). A conversation is private to its author.

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
    /// Last message or change, nanoseconds since the Unix epoch. JSON is RFC 3339.
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
    /// Last message or change, nanoseconds since the Unix epoch. JSON is RFC 3339.
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
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
