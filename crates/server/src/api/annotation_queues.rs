//! Annotation queues: their definitions and the bodies of the queue and item routes.
//!
//! Spec 04 § Annotation queues, spec 09 § Annotation queues, D-AQ-2–D-AQ-11.

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::common::{Limit, TimeParam};

/// What a queue's items are (D-AQ-2). Fixed at creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueueTarget {
    /// Traces.
    Trace,
    /// Spans.
    Span,
    /// Sessions.
    Session,
}

/// Where an item is in its review (D-AQ-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueueItemStatus {
    /// Not reviewed yet.
    Pending,
    /// Reviewed.
    Completed,
    /// Passed over.
    Skipped,
}

/// Who added an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueueItemAddedBy {
    /// A session user.
    User,
    /// A personal access token.
    Pat,
    /// An API key.
    Key,
}

/// Item counts of a queue, by status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct QueueCounts {
    /// Items not reviewed yet.
    pub pending: u64,
    /// Items reviewed.
    pub completed: u64,
    /// Items passed over.
    pub skipped: u64,
}

/// An annotation queue with its counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AnnotationQueue {
    /// Queue id.
    pub id: Uuid,
    /// Display name, unique in the project.
    pub name: String,
    /// Free text, 0 to 1 000 characters. Omitted when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Markdown for the reviewers, 0 to 10 000 characters. Omitted when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// What the items are.
    pub target: QueueTarget,
    /// The score definitions a review fills in, in order (1 to 50).
    pub definition_ids: Vec<Uuid>,
    /// Incremented by every change. A `PATCH` names the version it read.
    pub version: i64,
    /// Creator. Omitted for an API key's queue and when the user is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Uuid>,
    /// Creation time, nanoseconds since the Unix epoch. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
    /// Last update, nanoseconds since the Unix epoch. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: i64,
    /// Item counts by status, over the items the data window shows.
    pub counts: QueueCounts,
}

/// Body of `POST /api/v1/projects/{project}/annotation-queues`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateAnnotationQueue {
    /// Display name, 1 to 100 characters after trimming.
    pub name: String,
    /// Free text, 0 to 1 000 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Markdown for the reviewers, 0 to 10 000 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// What the items are. Fixed for the queue's life.
    pub target: QueueTarget,
    /// Score definitions of the project, 1 to 50, all distinct, in review order.
    pub definition_ids: Vec<Uuid>,
}

/// Body of `PATCH /api/v1/projects/{project}/annotation-queues/{id}`. Omitted fields are
/// unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateAnnotationQueue {
    /// The version the caller read. Must be the stored one, else `409`.
    pub version: i64,
    /// Display name, 1 to 100 characters after trimming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Free text, 0 to 1 000 characters. `""` clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Markdown for the reviewers, 0 to 10 000 characters. `""` clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// The whole list of score definitions, 1 to 50, all distinct, in review order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition_ids: Option<Vec<Uuid>>,
}

/// Query parameters of `GET /api/v1/projects/{project}/annotation-queues`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct AnnotationQueueListParams {
    /// Maximum number of items to return (1–1000, default 50).
    #[serde(default)]
    #[param(minimum = 1, maximum = 1000, default = 50)]
    pub limit: Limit,
    /// Opaque cursor from a previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Only queues that hold a visible trace or span item of this trace (32 hex digits).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_trace_id: Option<String>,
    /// Only queues that hold a visible item of this session. Not with `target_trace_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_session_id: Option<String>,
}

/// One target of a queue: a trace, a span or a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AnnotationQueueItem {
    /// Item id.
    pub id: Uuid,
    /// The queue the item is in.
    pub queue_id: Uuid,
    /// Trace id, 32 lowercase hex digits. Trace and span items. Omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Span id, 16 lowercase hex digits. Span items. Omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    /// Session id. Session items. Omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The trace's start, or the session's earliest visible trace start. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub target_time: i64,
    /// Where the item is in its review.
    pub status: QueueItemStatus,
    /// Why it was skipped. Omitted unless skipped with a reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    /// The reviewer who holds the lease. Set only while the lease is live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leased_by_user_id: Option<Uuid>,
    /// When the lease ends. Set only while the lease is live. JSON is RFC 3339.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub lease_expires_at: Option<i64>,
    /// Who completed or skipped the item. Omitted while pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done_by_user_id: Option<Uuid>,
    /// When the item was completed or skipped. Omitted while pending. JSON is RFC 3339.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub done_at: Option<i64>,
    /// Who added the item.
    pub added_by: QueueItemAddedBy,
    /// The id of the user, personal token or key that added the item.
    pub added_by_id: Uuid,
    /// When the item was added. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: i64,
}

/// Query parameters of `GET /api/v1/projects/{project}/annotation-queues/{id}/items`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct AnnotationQueueItemListParams {
    /// Maximum number of items to return (1–1000, default 50).
    #[serde(default)]
    #[param(minimum = 1, maximum = 1000, default = 50)]
    pub limit: Limit,
    /// Opaque cursor from a previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Only items with this status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<QueueItemStatus>,
}

/// One target to add to a queue. Exactly the fields of the queue's target are set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueItemTarget {
    /// Trace id, 32 lowercase hex digits. Trace and span queues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Span id, 16 lowercase hex digits. Span queues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    /// Session id. Session queues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Body of `POST /api/v1/projects/{project}/annotation-queues/{id}/items`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AddQueueItems {
    /// 1 to 1 000 targets.
    pub items: Vec<QueueItemTarget>,
}

/// A target that was not added, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SkippedQueueItem {
    /// Position of the target in the request.
    pub index: u64,
    /// Why it was not added. Never the rejected value.
    pub message: String,
}

/// The answer of an add. A target already in the queue counts as `existing`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AddedQueueItems {
    /// Targets added.
    pub added: u64,
    /// Targets that were already in the queue.
    pub existing: u64,
    /// Targets that were not added: unknown, hidden, evaluation or guarded.
    pub skipped: Vec<SkippedQueueItem>,
}

/// Body of `POST /api/v1/projects/{project}/annotation-queues/{id}/items/matching/preview` and
/// `POST .../items/matching`: the queue kind's list filter over a window. `search` is not taken
/// by a session queue.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueMatchingRequest {
    /// Filter text over the queue kind's list: `traces`, `spans` or `sessions`.
    #[serde(default)]
    pub filter: Option<String>,
    /// Full-text search over the traces or spans. Refused on a session queue.
    #[serde(default)]
    pub search: Option<String>,
    /// Start of the window: the targets' own start, `[from, to)`.
    pub from: TimeParam,
    /// End of the window, exclusive.
    pub to: TimeParam,
    /// Keep the experiment traces, as the list does. Evaluation traces are never matched.
    #[serde(default)]
    pub include_experiments: bool,
}

/// How many targets a filter would add. `matched` is at most 10 000; `capped` says more matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct QueueMatchingPreview {
    /// Targets the filter matches, at most 10 000.
    pub matched: u64,
    /// More than 10 000 targets matched: only the first 10 000 by id are added.
    pub capped: bool,
}

/// The answer of an add by filter. A target already in the queue counts as `existing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct QueueMatchingAdded {
    /// Targets added.
    pub added: u64,
    /// Targets that were already in the queue.
    pub existing: u64,
    /// Matched targets not added because a deletion blocks them.
    pub blocked: u64,
    /// More than 10 000 targets matched: only the first 10 000 by id were considered.
    pub capped: bool,
}

/// Body of `POST /api/v1/projects/{project}/annotation-queues/{id}/next`. `{}` is enough.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NextQueueItem {
    /// The item to move on from: the next free item after it by id, wrapping around. Need not
    /// exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Uuid>,
}

/// Body of `POST .../items/{item_id}/skip`. `{}` is enough.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SkipQueueItem {
    /// Why the item is skipped. Free text, 0 to 1 000 characters after trimming; blank is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
