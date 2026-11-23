//! Assistant conversation routes (spec 09 § Assistant, D-AS-4, D-AS-10).
//!
//! Every route needs [`ProjectPerm::UseAssistant`] with a session: member and above, no API key,
//! no personal access token. A conversation is private to its author, so another user's id, and
//! an id of another project, answer `404`.

use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::{Method, StatusCode};
use axum::response::sse::Event;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval_at};
use uuid::Uuid;

use cetra_core::ids::{ProjectId, TraceId};

use crate::api::assistant::{
    AssistantConversation, AssistantConversationDetail, AssistantFeedback,
    AssistantFeedbackRequest, AssistantMessage, AssistantMessageStatus, AssistantPart,
    AssistantRole, AssistantSettings, AssistantStatus, AssistantTestRequest, AssistantTurnRequest,
    AssistantUnavailable, PutAssistantSettings, UpdateAssistantConversationRequest,
};
use crate::api::common::{FieldError, Page};
use crate::api::connections::{LlmConnectionTestResult, TestErrorClass};
use crate::assistant::settings::{self as assistant_settings, Resolved};
use crate::assistant::turn::{
    PERMIT_WAIT, TURN_DEADLINE, TURN_MESSAGES_MAX, TurnEvent, TurnSetup, run_turn,
};
use crate::audit;
use crate::audit::catalog::AssistantAction;
use crate::auth::limits::{AssistantRefusal, RetryAfter};
use crate::auth::principal::{self, Principal};
use crate::auth::roles::{OrgPerm, ProjectPerm, Role};
use crate::http::AppState;
use crate::http::access::{Access, OrgAccess, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError, field_error};
use crate::http::restore::refuse_outbound;
use crate::live::StreamTiming;
use crate::meta::MetaError;
use crate::meta::assistant::{
    self as store, AssistantSettingsRow, ConversationRow, Feedback, FeedbackOutcome, MessageRole,
    MessageRow, MessageStatus, NewMessage,
};
use crate::meta::connections;
use crate::meta::orgs;
use crate::routes::streams::{ping_event, send, sse_response};

/// Longest conversation title, in characters after trimming (D-AS-10).
const MAX_TITLE_CHARS: usize = 80;
/// Longest model name, in bytes after trimming (D-AS-24's test takes the same bound).
const MAX_MODEL_BYTES: usize = 256;
/// Largest body of a turn request: the message, the context and their JSON (D-AS-14, D-AS-15).
const TURN_BODY_LIMIT: usize = 64 * 1024;
/// Longest user message, in bytes after trimming (D-AS-15).
const MAX_MESSAGE_BYTES: usize = 16_384;
/// Largest page context, as compact JSON (D-AS-14).
const MAX_CONTEXT_BYTES: usize = 8_192;
/// Longest `page` name of a context, in characters (D-AS-14).
const MAX_PAGE_CHARS: usize = 64;
/// The `409` detail when the org's assistant is off or has no usable connection.
const ASSISTANT_OFF: &str = "the assistant is off in this organization";
/// The `409` detail when the conversation already has a turn in progress.
const TURN_RUNNING: &str = "a turn is already running in this conversation";
/// Events the turn may have queued for its forwarder before the engine waits.
const TURN_BUFFER: usize = 16;

/// Assistant conversation routes (spec 09).
pub(crate) fn routes() -> Vec<RouteDef> {
    let access = Access::Project {
        perm: ProjectPerm::UseAssistant,
        session_only: true,
    };
    vec![
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/assistant/conversations",
            access,
            documented: true,
            handler: get(list_conversations),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/assistant/conversations",
            access,
            documented: true,
            handler: post(create_conversation),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/assistant/conversations/{id}",
            access,
            documented: true,
            handler: get(get_conversation),
        },
        RouteDef {
            method: Method::PUT,
            path: "/api/v1/projects/{project}/assistant/conversations/{id}/messages/{seq}/feedback",
            access,
            documented: true,
            handler: put(put_feedback),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/assistant/conversations/{id}/turns",
            access,
            documented: true,
            handler: post(post_turn).layer(DefaultBodyLimit::max(TURN_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::PATCH,
            path: "/api/v1/projects/{project}/assistant/conversations/{id}",
            access,
            documented: true,
            handler: patch(update_conversation),
        },
        RouteDef {
            method: Method::DELETE,
            path: "/api/v1/projects/{project}/assistant/conversations/{id}",
            access,
            documented: true,
            handler: delete(delete_conversation),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/orgs/{org}/assistant",
            access: Access::Org {
                perm: OrgPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_org_assistant),
        },
        RouteDef {
            method: Method::PUT,
            path: "/api/v1/orgs/{org}/assistant",
            access: Access::Org {
                perm: OrgPerm::ManageConnections,
                session_only: true,
            },
            documented: true,
            handler: put(put_org_assistant),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/orgs/{org}/assistant/test",
            access: Access::Org {
                perm: OrgPerm::ManageConnections,
                session_only: true,
            },
            documented: true,
            handler: post(test_org_assistant),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/assistant",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: true,
            },
            documented: true,
            handler: get(get_project_assistant),
        },
    ]
}

#[derive(Debug, Deserialize)]
struct ConversationPath {
    id: String,
}

#[derive(Debug, Deserialize)]
struct MessagePath {
    id: String,
    seq: String,
}

/// The id of a path. Text that is not a UUID is `404`: the id space is private.
fn parse_id(text: &str) -> Result<Uuid, ApiError> {
    text.parse().map_err(|_| ApiError::NotFound)
}

/// Trimmed, 1 to 80 characters, no control characters. Returns the trimmed title when it is valid.
fn checked_title(text: &str) -> Result<String, ApiError> {
    let title = text.trim();
    let length = title.chars().count();
    if (1..=MAX_TITLE_CHARS).contains(&length) && !title.chars().any(char::is_control) {
        Ok(title.to_owned())
    } else {
        Err(field_error(
            "title",
            "must be 1 to 80 characters without control characters",
        ))
    }
}

impl From<MessageRole> for AssistantRole {
    fn from(role: MessageRole) -> Self {
        match role {
            MessageRole::User => Self::User,
            MessageRole::Assistant => Self::Assistant,
            MessageRole::Tool => Self::Tool,
        }
    }
}

impl From<MessageStatus> for AssistantMessageStatus {
    fn from(status: MessageStatus) -> Self {
        match status {
            MessageStatus::Complete => Self::Complete,
            MessageStatus::Cancelled => Self::Cancelled,
            MessageStatus::Failed => Self::Failed,
        }
    }
}

impl From<Feedback> for AssistantFeedback {
    fn from(feedback: Feedback) -> Self {
        match feedback {
            Feedback::Up => Self::Up,
            Feedback::Down => Self::Down,
        }
    }
}

/// A conversation row as the API returns it. A count that does not fit `u32` is a corrupt row.
fn conversation_json(row: ConversationRow) -> Result<AssistantConversation, ApiError> {
    Ok(AssistantConversation {
        id: row.id,
        title: row.title,
        message_count: count_u32(row.message_count, "message_count")?,
        created_at: row.created_at,
        last_active_at: row.last_active_at,
    })
}

/// A stored message as the API returns it. `parts` and `context` are JSON text, and a value that
/// does not parse is a corrupt row (an internal error, not a `400`).
fn message_json(row: &MessageRow) -> Result<AssistantMessage, ApiError> {
    let parts: Vec<AssistantPart> = serde_json::from_str(&row.parts).map_err(|_| {
        ApiError::from(MetaError::Corrupt {
            table: "assistant_messages",
            column: "parts",
        })
    })?;
    let context = row
        .context
        .as_deref()
        .map(serde_json::from_str::<Value>)
        .transpose()
        .map_err(|_| {
            ApiError::from(MetaError::Corrupt {
                table: "assistant_messages",
                column: "context",
            })
        })?;
    let seq = u32::try_from(row.seq).map_err(|_| {
        ApiError::from(MetaError::Corrupt {
            table: "assistant_messages",
            column: "seq",
        })
    })?;
    Ok(AssistantMessage {
        seq,
        role: row.role.into(),
        parts,
        context,
        status: row.status.into(),
        feedback: row.feedback.map(AssistantFeedback::from),
        turn_trace_id: row.turn_trace_id.clone(),
        created_at: row.created_at,
    })
}

fn count_u32(value: i64, column: &'static str) -> Result<u32, ApiError> {
    u32::try_from(value).map_err(|_| {
        ApiError::from(MetaError::Corrupt {
            table: "assistant_conversations",
            column,
        })
    })
}

/// `GET …/assistant/conversations`: the caller's conversations, newest activity first. Not paged.
async fn list_conversations(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
) -> Result<error::Json<Page<AssistantConversation>>, ApiError> {
    let user = principal::session_user(&principal)?.id;
    let rows = store::list_conversations(state.meta.reader(), access.project.id, user).await?;
    let data = rows
        .into_iter()
        .map(conversation_json)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(error::Json(Page {
        data,
        next_cursor: None,
    }))
}

/// `POST …/assistant/conversations`: a new conversation with an empty title. Answers `201`.
async fn create_conversation(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
) -> Result<Response, ApiError> {
    let user = principal::session_user(&principal)?.id;
    let project: ProjectId = access.project.id;
    let id = Uuid::now_v7();
    let now = state.clock.now_ns();
    let row = state
        .meta
        .write(move |conn| {
            Box::pin(async move { store::insert_conversation(conn, id, project, user, now).await })
        })
        .await?;
    tracing::info!(conversation_id = %id, project_id = %project, user_id = %user, "assistant conversation created");
    Ok((StatusCode::CREATED, error::Json(conversation_json(row)?)).into_response())
}

/// `GET …/assistant/conversations/{id}`: the conversation with its messages, oldest first.
async fn get_conversation(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<ConversationPath>,
) -> Result<error::Json<AssistantConversationDetail>, ApiError> {
    let user = principal::session_user(&principal)?.id;
    let project = access.project.id;
    let id = parse_id(&path.id)?;
    let (row, messages) =
        store::get_conversation_with_messages(state.meta.reader(), project, user, id)
            .await?
            .ok_or(ApiError::NotFound)?;
    let messages = messages
        .iter()
        .map(message_json)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(error::Json(AssistantConversationDetail {
        id: row.id,
        title: row.title,
        message_count: count_u32(row.message_count, "message_count")?,
        created_at: row.created_at,
        last_active_at: row.last_active_at,
        messages,
    }))
}

fn stored_feedback(feedback: AssistantFeedback) -> Feedback {
    match feedback {
        AssistantFeedback::Up => Feedback::Up,
        AssistantFeedback::Down => Feedback::Down,
    }
}

/// `PUT …/assistant/conversations/{id}/messages/{seq}/feedback`: rates an assistant message, or
/// clears the rating with `null`. Answers `204`. Another user's conversation and a missing message
/// are `404`; a user or tool message is `400`.
async fn put_feedback(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<MessagePath>,
    error::Json(body): error::Json<AssistantFeedbackRequest>,
) -> Result<StatusCode, ApiError> {
    let user = principal::session_user(&principal)?.id;
    let project = access.project.id;
    let id = parse_id(&path.id)?;
    let seq = path
        .seq
        .parse::<i64>()
        .ok()
        .filter(|seq| *seq >= 1)
        .ok_or(ApiError::NotFound)?;
    let feedback = body.feedback.map(stored_feedback);
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(
                async move { store::set_feedback(conn, project, user, id, seq, feedback).await },
            )
        })
        .await?;
    match outcome {
        FeedbackOutcome::Set => {
            tracing::info!(conversation_id = %id, project_id = %project, user_id = %user, seq, feedback = ?body.feedback, "assistant feedback saved");
            Ok(StatusCode::NO_CONTENT)
        }
        FeedbackOutcome::NotFound => Err(ApiError::NotFound),
        FeedbackOutcome::NotAnAssistantMessage => {
            Err(field_error("seq", "only assistant messages take feedback"))
        }
    }
}

/// `PATCH …/assistant/conversations/{id}`: sets the title. Does not move the conversation in the
/// list.
async fn update_conversation(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<ConversationPath>,
    error::Json(body): error::Json<UpdateAssistantConversationRequest>,
) -> Result<error::Json<AssistantConversation>, ApiError> {
    let user = principal::session_user(&principal)?.id;
    let project = access.project.id;
    let id = parse_id(&path.id)?;
    let title = checked_title(&body.title)?;
    let row = state
        .meta
        .write(move |conn| {
            Box::pin(
                async move { store::rename_conversation(conn, project, user, id, &title).await },
            )
        })
        .await?
        .ok_or(ApiError::NotFound)?;
    tracing::info!(conversation_id = %id, project_id = %project, user_id = %user, "assistant conversation renamed");
    Ok(error::Json(conversation_json(row)?))
}

/// `DELETE …/assistant/conversations/{id}`: deletes the conversation and its messages.
async fn delete_conversation(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<ConversationPath>,
) -> Result<StatusCode, ApiError> {
    let user = principal::session_user(&principal)?.id;
    let project = access.project.id;
    let id = parse_id(&path.id)?;
    let deleted = state
        .meta
        .write(move |conn| {
            Box::pin(async move { store::delete_conversation(conn, project, user, id).await })
        })
        .await?;
    if deleted {
        tracing::info!(conversation_id = %id, project_id = %project, user_id = %user, "assistant conversation deleted");
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

/// The user message trimmed, when it is 1 to 16,384 bytes (D-AS-15).
fn checked_message(text: &str) -> Result<String, ApiError> {
    let message = text.trim();
    if (1..=MAX_MESSAGE_BYTES).contains(&message.len()) {
        Ok(message.to_owned())
    } else {
        Err(field_error("message", "must be 1 to 16384 bytes"))
    }
}

/// The page context as compact JSON (D-AS-14): an object, at most 8 KiB, with a string `page` of
/// 1 to 64 characters. `None` when the request has no context.
fn checked_context(context: Option<&Value>) -> Result<Option<String>, ApiError> {
    let Some(context) = context else {
        return Ok(None);
    };
    let Some(object) = context.as_object() else {
        return Err(field_error("context", "must be a JSON object"));
    };
    let compact = serde_json::to_string(context).map_err(internal)?;
    if compact.len() > MAX_CONTEXT_BYTES {
        return Err(field_error("context", "must be at most 8 KiB"));
    }
    let page_ok = object
        .get("page")
        .and_then(Value::as_str)
        .is_some_and(|page| (1..=MAX_PAGE_CHARS).contains(&page.chars().count()));
    if !page_ok {
        return Err(field_error(
            "context.page",
            "must be a string of 1 to 64 characters",
        ));
    }
    Ok(Some(compact))
}

/// The conversation title of a turn's first message: control characters become spaces, and the
/// text is cut to 80 characters (D-AS-10).
fn turn_title(message: &str) -> String {
    message
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_TITLE_CHARS)
        .collect()
}

/// A trace id of 16 random bytes (as the playground's runs make them).
fn new_trace_id() -> Result<TraceId, ApiError> {
    let bytes = crate::auth::secret::random_bytes(16).map_err(internal)?;
    let bytes = <[u8; 16]>::try_from(bytes.as_slice()).map_err(internal)?;
    TraceId::from_bytes(bytes).map_err(internal)
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> ApiError {
    ApiError::Internal(Box::new(error))
}

/// `POST …/assistant/conversations/{id}/turns`: stores the user message, then streams the turn's
/// events as SSE (D-AS-9). Every refusal before the stream is a problem answer. Aborting the
/// request cancels the turn.
async fn post_turn(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<ConversationPath>,
    error::Json(body): error::Json<AssistantTurnRequest>,
) -> Result<Response, ApiError> {
    refuse_outbound(&state, "assistant turn")?;
    let user = principal::session_user(&principal)?.clone();
    let project = access.project.clone();
    let id = parse_id(&path.id)?;
    let message = checked_message(&body.message)?;
    let context_json = checked_context(body.context.as_ref())?;
    let reader = state.meta.reader();
    let conversation = store::get_conversation(reader, project.id, user.id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let org = orgs::org_by_id(reader, project.org_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if conversation.message_count + TURN_MESSAGES_MAX > store::MESSAGES_PER_CONVERSATION {
        return Err(ApiError::Conflict {
            detail: store::TOO_MANY_MESSAGES,
        });
    }
    let (connection_row, model) = match assistant_settings::resolve(&state, project.org_id).await? {
        Resolved::Usable { connection, model } => (*connection, model),
        Resolved::Disabled | Resolved::NoConnection => {
            return Err(ApiError::Conflict {
                detail: ASSISTANT_OFF,
            });
        }
    };
    let now = state.clock.now_ns();
    let guard = state
        .limits
        .assistant
        .admit(id, user.id, project.org_id, now)
        .map_err(|refusal| match refusal {
            AssistantRefusal::Busy => ApiError::Conflict {
                detail: TURN_RUNNING,
            },
            AssistantRefusal::Limited(RetryAfter(wait)) => ApiError::RateLimited {
                retry_after_s: wait.as_secs().max(1),
            },
        })?;
    let connection = state
        .connections
        .get(&state.secrets, &connection_row)
        .map_err(internal)?;
    let trace_id = new_trace_id()?;
    let user_message = NewMessage {
        role: MessageRole::User,
        parts: serde_json::to_string(&[AssistantPart::Text {
            content: message.clone(),
        }])
        .map_err(internal)?,
        context: context_json,
        status: MessageStatus::Complete,
        turn_trace_id: Some(trace_id.to_hex()),
        stub_after: None,
    };
    let title = turn_title(&message);
    let user_seq = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                // The room is checked here, not only above: another turn may have appended since.
                store::ensure_room(conn, id, TURN_MESSAGES_MAX).await?;
                let seq = store::append_message(conn, id, user_message, now).await?;
                store::set_title_if_empty(conn, id, &title).await?;
                Ok::<i64, MetaError>(seq)
            })
        })
        .await?;
    let (project_id, user_id) = (project.id, user.id);
    let setup = TurnSetup {
        project,
        org_slug: org.slug,
        user,
        conversation: id,
        user_seq,
        context: body.context,
        connection_row,
        connection,
        model,
        trace_id,
        permit_wait: PERMIT_WAIT,
        turn_deadline: TURN_DEADLINE,
    };
    let timing = state.live.timing;
    let (turn_tx, turn_rx) = mpsc::channel::<TurnEvent>(TURN_BUFFER);
    let (sse_tx, sse_rx) = mpsc::channel::<Event>(TURN_BUFFER);
    tokio::spawn(run_turn(state.clone(), setup, turn_tx, guard));
    tokio::spawn(forward_turn(turn_rx, sse_tx, timing));
    tracing::info!(conversation_id = %id, project_id = %project_id, user_id = %user_id, turn_seq = user_seq, "assistant turn started");
    Ok(sse_response(sse_rx))
}

/// Forwards the engine's events to the client as SSE, with a ping every `timing.ping` (D-AS-9).
/// Stops when the client goes away or stops reading within `timing.send`. Dropping `turn_rx` then
/// cancels the turn.
async fn forward_turn(
    mut turn_rx: mpsc::Receiver<TurnEvent>,
    sse_tx: mpsc::Sender<Event>,
    timing: StreamTiming,
) {
    let mut ping = interval_at(Instant::now() + timing.ping, timing.ping);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            () = sse_tx.closed() => break,
            event = turn_rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                let sse = Event::default()
                    .event(event.name())
                    .data(event.data().to_string());
                if !send(&sse_tx, sse, timing.send).await {
                    break;
                }
            }
            _ = ping.tick() => {
                if !send(&sse_tx, ping_event(), timing.send).await {
                    break;
                }
            }
        }
    }
}

/// The model name trimmed, when it is 1 to 256 bytes.
fn checked_model(text: &str) -> Option<String> {
    let model = text.trim();
    (!model.is_empty() && model.len() <= MAX_MODEL_BYTES).then(|| model.to_owned())
}

fn model_error() -> FieldError {
    FieldError {
        field: "model".to_owned(),
        message: "must be 1 to 256 bytes".to_owned(),
        position: None,
    }
}

fn required_error(field: &str) -> FieldError {
    FieldError {
        field: field.to_owned(),
        message: "required when enabled".to_owned(),
        position: None,
    }
}

/// The per-user limit of connection tests (`state.limits.connection_tests`), shared with the
/// assistant's test (D-AS-24).
fn check_test_limit(state: &AppState, user: Uuid) -> Result<(), ApiError> {
    state
        .limits
        .connection_tests
        .check(&user, state.clock.now_ns())
        .map_err(|retry| ApiError::RateLimited {
            retry_after_s: retry.0.as_secs().max(1),
        })
}

/// The stored setting as the API returns it. No row is the setting off, never saved.
fn settings_json(row: Option<AssistantSettingsRow>) -> AssistantSettings {
    match row {
        None => AssistantSettings {
            enabled: false,
            connection_id: None,
            model: None,
            updated_at: None,
        },
        Some(row) => AssistantSettings {
            enabled: row.enabled,
            connection_id: row.connection_id,
            model: row.model,
            updated_at: Some(row.updated_at),
        },
    }
}

fn class_name(class: TestErrorClass) -> &'static str {
    match class {
        TestErrorClass::Retryable => "retryable",
        TestErrorClass::Pause => "pause",
        TestErrorClass::Permanent => "permanent",
    }
}

/// A failed test call, as `400`: the class and the provider's text, with the field `model`.
fn test_failed(result: &LlmConnectionTestResult) -> ApiError {
    let detail = match &result.error {
        Some(error) => format!(
            "the test call failed: {}: {}",
            class_name(error.class),
            error.message
        ),
        None => "the test call failed".to_owned(),
    };
    ApiError::BadRequest {
        detail: Some(detail),
        errors: vec![FieldError {
            field: "model".to_owned(),
            message: "the model must support tool calling".to_owned(),
            position: None,
        }],
    }
}

fn unusable(reason: AssistantUnavailable) -> AssistantStatus {
    AssistantStatus {
        usable: false,
        reason: Some(reason),
        connection_name: None,
        adapter: None,
        model: None,
    }
}

/// `GET /orgs/{org}/assistant`: the setting, or `enabled: false` when the org never saved one.
async fn get_org_assistant(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
) -> Result<error::Json<AssistantSettings>, ApiError> {
    let row = store::get_settings(state.meta.reader(), access.org.id).await?;
    Ok(error::Json(settings_json(row)))
}

/// `PUT /orgs/{org}/assistant`: saves the whole setting. Enabling runs the tool-calling test first
/// (D-AS-3, D-AS-24); a failed test stores nothing. Answers the saved setting.
async fn put_org_assistant(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    ctx: audit::Ctx,
    error::Json(body): error::Json<PutAssistantSettings>,
) -> Result<error::Json<AssistantSettings>, ApiError> {
    let user = principal::session_user(&principal)?.id;
    let org = access.org.id;
    let mut errors = Vec::new();
    let model = match body.model.as_deref() {
        None => None,
        Some(text) => {
            let model = checked_model(text);
            if model.is_none() {
                errors.push(model_error());
            }
            model
        }
    };
    if body.enabled {
        if body.connection_id.is_none() {
            errors.push(required_error("connection_id"));
        }
        if body.model.is_none() {
            errors.push(required_error("model"));
        }
    }
    if !errors.is_empty() {
        return Err(ApiError::BadRequest {
            detail: None,
            errors,
        });
    }
    let connection = match body.connection_id {
        Some(id) => Some(
            connections::get_connection(state.meta.reader(), org, id)
                .await?
                .ok_or_else(|| {
                    field_error("connection_id", "connection not found in this organization")
                })?,
        ),
        None => None,
    };
    if let (true, Some(connection), Some(model)) =
        (body.enabled, connection.as_ref(), model.as_deref())
    {
        refuse_outbound(&state, "assistant test")?;
        check_test_limit(&state, user)?;
        let result = assistant_settings::test_call(&state, connection, model).await?;
        if !result.ok {
            return Err(test_failed(&result));
        }
    }
    let now = state.clock.now_ns();
    let saved = AssistantSettingsRow {
        enabled: body.enabled,
        connection_id: body.connection_id,
        model,
        updated_by: Some(user),
        updated_at: now,
    };
    let to_store = saved.clone();
    let event = audit::Event::new(AssistantAction::SettingChanged {
        enabled: saved.enabled,
        connection_id: saved.connection_id,
    })
    .org(org);
    state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                store::put_settings(conn, org, &to_store).await?;
                audit::record(conn, &ctx, event, now).await
            })
        })
        .await?;
    Ok(error::Json(settings_json(Some(saved))))
}

/// `POST /orgs/{org}/assistant/test`: runs the tool-calling test on a connection without saving.
async fn test_org_assistant(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<OrgAccess>,
    error::Json(body): error::Json<AssistantTestRequest>,
) -> Result<error::Json<LlmConnectionTestResult>, ApiError> {
    refuse_outbound(&state, "assistant test")?;
    let user = principal::session_user(&principal)?.id;
    check_test_limit(&state, user)?;
    let connection =
        connections::get_connection(state.meta.reader(), access.org.id, body.connection_id)
            .await?
            .ok_or(ApiError::NotFound)?;
    let model = checked_model(&body.model).ok_or_else(|| ApiError::BadRequest {
        detail: None,
        errors: vec![model_error()],
    })?;
    let result = assistant_settings::test_call(&state, &connection, &model).await?;
    Ok(error::Json(result))
}

/// `GET /projects/{project}/assistant`: whether the caller can chat, and why not. Answers `200`
/// in every case: the reason is in the body.
async fn get_project_assistant(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
) -> Result<error::Json<AssistantStatus>, ApiError> {
    if !matches!(access.role, Some(role) if role >= Role::Member) {
        return Ok(error::Json(unusable(AssistantUnavailable::Forbidden)));
    }
    if state.config.restore_mode {
        return Ok(error::Json(unusable(AssistantUnavailable::RestoreMode)));
    }
    let status = match assistant_settings::resolve(&state, access.project.org_id).await? {
        Resolved::Disabled => unusable(AssistantUnavailable::Disabled),
        Resolved::NoConnection => unusable(AssistantUnavailable::NoConnection),
        Resolved::Usable { connection, model } => AssistantStatus {
            usable: true,
            reason: None,
            connection_name: Some(connection.name),
            adapter: Some(connection.adapter),
            model: Some(model),
        },
    };
    Ok(error::Json(status))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::api::assistant::AssistantDraftKind;
    use crate::api::common::ProblemCode;
    use crate::auth::roles::OrgRole;
    use crate::meta::assistant::insert_message_raw;
    use crate::meta::projects::insert_project;
    use crate::test_support::{Seeded, TestApp, capture_logs};

    /// A trace id of 32 lowercase hex characters, for the messages that have one.
    const TRACE: &str = "0af7651916cd43dd8448eb211c80319c";
    /// One day in nanoseconds.
    const DAY_NS: i64 = 86_400_000_000_000;

    fn base(project: ProjectId) -> String {
        format!("/api/v1/projects/{project}/assistant/conversations")
    }

    fn path(project: ProjectId, id: impl std::fmt::Display) -> String {
        format!("{}/{id}", base(project))
    }

    async fn send(
        app: &TestApp,
        method: Method,
        path: &str,
        cookie: &str,
        body: Option<Value>,
    ) -> crate::test_support::TestResponse {
        let request = app.request(method, path).header("cookie", cookie);
        match body {
            Some(body) => request.json(&body).send().await,
            None => request.send().await,
        }
    }

    async fn create_as(app: &TestApp, project: ProjectId, cookie: &str) -> AssistantConversation {
        let response = send(app, Method::POST, &base(project), cookie, None).await;
        assert_eq!(response.status, StatusCode::CREATED);
        response.json()
    }

    async fn rename_as(
        app: &TestApp,
        project: ProjectId,
        id: Uuid,
        cookie: &str,
        title: &str,
    ) -> crate::test_support::TestResponse {
        send(
            app,
            Method::PATCH,
            &path(project, id),
            cookie,
            Some(json!({"title": title})),
        )
        .await
    }

    /// Three messages written with the raw helper: a user message with context; an assistant
    /// message with a tool call and a draft, rated up and with a trace; and a tool message with
    /// a result.
    async fn seed_three_messages(app: &TestApp, conversation: Uuid) {
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_message_raw(
                        conn,
                        conversation,
                        1,
                        "user",
                        r#"[{"type":"text","content":"Hello"}]"#,
                        Some(r#"{"page":"traces"}"#),
                        None,
                        now,
                    )
                    .await?;
                    insert_message_raw(
                        conn,
                        conversation,
                        2,
                        "assistant",
                        r#"[{"type":"tool_call","id":"call_1","name":"query_sql","arguments":{"query":"SELECT 1"}},{"type":"draft","id":"draft_1","kind":"sql","draft":{"query":"SELECT 1"}}]"#,
                        None,
                        None,
                        now,
                    )
                    .await?;
                    insert_message_raw(
                        conn,
                        conversation,
                        3,
                        "tool",
                        r#"[{"type":"tool_result","id":"call_1","name":"query_sql","ok":true,"result":{"rows":[{"one":1}]},"size_bytes":20,"rows":1,"stubbed":false}]"#,
                        None,
                        Some(now + DAY_NS),
                        now,
                    )
                    .await?;
                    sqlx::query(
                        "UPDATE assistant_messages SET feedback = 'up', turn_trace_id = ? \
                         WHERE conversation_id = ? AND seq = 2",
                    )
                    .bind(TRACE)
                    .bind(conversation.to_string())
                    .execute(&mut *conn)
                    .await
                    .map_err(MetaError::Db)?;
                    sqlx::query(
                        "UPDATE assistant_conversations SET message_count = 3 WHERE id = ?",
                    )
                    .bind(conversation.to_string())
                    .execute(&mut *conn)
                    .await
                    .map_err(MetaError::Db)?;
                    Ok(())
                })
            })
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn member_creates_lists_reads_renames_and_deletes() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let member = app
            .add_member(&seeded, "bob@example.com", OrgRole::Member)
            .await;
        let project = seeded.project.id;

        let created = create_as(&app, project, &member.cookie).await;
        assert_eq!(created.title, "");
        assert_eq!(created.message_count, 0);

        let listed = send(&app, Method::GET, &base(project), &member.cookie, None).await;
        assert_eq!(listed.status, StatusCode::OK);
        let page: Page<AssistantConversation> = listed.json();
        assert_eq!(page.next_cursor, None);
        assert_eq!(page.data.len(), 1);
        assert_eq!(page.data[0].id, created.id);

        let renamed = rename_as(&app, project, created.id, &member.cookie, "Checkout").await;
        assert_eq!(renamed.status, StatusCode::OK);
        let renamed: AssistantConversation = renamed.json();
        assert_eq!(renamed.title, "Checkout");

        let read = send(
            &app,
            Method::GET,
            &path(project, created.id),
            &member.cookie,
            None,
        )
        .await;
        assert_eq!(read.status, StatusCode::OK);
        let detail: AssistantConversationDetail = read.json();
        assert_eq!(detail.title, "Checkout");
        assert!(detail.messages.is_empty());

        let deleted = send(
            &app,
            Method::DELETE,
            &path(project, created.id),
            &member.cookie,
            None,
        )
        .await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);

        let listed = send(&app, Method::GET, &base(project), &member.cookie, None).await;
        let page: Page<AssistantConversation> = listed.json();
        assert!(page.data.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn viewer_is_forbidden() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let project = seeded.project.id;
        let own = create_as(&app, project, &seeded.cookie).await;

        let list = send(&app, Method::GET, &base(project), &viewer.cookie, None).await;
        assert_eq!(list.status, StatusCode::FORBIDDEN);
        let create = send(&app, Method::POST, &base(project), &viewer.cookie, None).await;
        assert_eq!(create.status, StatusCode::FORBIDDEN);
        let read = send(
            &app,
            Method::GET,
            &path(project, own.id),
            &viewer.cookie,
            None,
        )
        .await;
        assert_eq!(read.status, StatusCode::FORBIDDEN);
        let rename = rename_as(&app, project, own.id, &viewer.cookie, "x").await;
        assert_eq!(rename.status, StatusCode::FORBIDDEN);
        let delete = send(
            &app,
            Method::DELETE,
            &path(project, own.id),
            &viewer.cookie,
            None,
        )
        .await;
        assert_eq!(delete.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pat_and_key_are_forbidden() {
        let app = TestApp::new().await;
        let seeded: Seeded = app.seed().await;
        let project = seeded.project.id;
        let token = app.personal_token(seeded.user_id, true).await;
        let bearer_token = format!("Bearer {token}");
        let bearer_key = format!("Bearer {}", seeded.read_key);
        for header in [&bearer_token, &bearer_key] {
            let list = app
                .request(Method::GET, &base(project))
                .header("authorization", header)
                .send()
                .await;
            assert_eq!(list.status, StatusCode::FORBIDDEN);
            let create = app
                .request(Method::POST, &base(project))
                .header("authorization", header)
                .send()
                .await;
            assert_eq!(create.status, StatusCode::FORBIDDEN);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn another_users_conversation_is_not_found() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let author = app
            .add_member(&seeded, "bob@example.com", OrgRole::Member)
            .await;
        let other = app
            .add_member(&seeded, "carol@example.com", OrgRole::Member)
            .await;
        let project = seeded.project.id;
        let conversation = create_as(&app, project, &author.cookie).await;

        // Another member, and the org owner, cannot see the conversation.
        for cookie in [&other.cookie, &seeded.cookie] {
            let read = send(
                &app,
                Method::GET,
                &path(project, conversation.id),
                cookie,
                None,
            )
            .await;
            assert_eq!(read.status, StatusCode::NOT_FOUND);
            let rename = rename_as(&app, project, conversation.id, cookie, "stolen").await;
            assert_eq!(rename.status, StatusCode::NOT_FOUND);
            let delete = send(
                &app,
                Method::DELETE,
                &path(project, conversation.id),
                cookie,
                None,
            )
            .await;
            assert_eq!(delete.status, StatusCode::NOT_FOUND);
            let listed = send(&app, Method::GET, &base(project), cookie, None).await;
            let page: Page<AssistantConversation> = listed.json();
            assert!(page.data.is_empty());
        }

        // The author still has it, untouched.
        let listed = send(&app, Method::GET, &base(project), &author.cookie, None).await;
        let page: Page<AssistantConversation> = listed.json();
        assert_eq!(page.data.len(), 1);
        assert_eq!(page.data[0].title, "");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn other_project_is_not_found() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let conversation = create_as(&app, project, &seeded.cookie).await;
        let org = seeded.org_id;
        let now = app.state.clock.now_ns();
        let other = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_project(conn, org, "Other bot", "other-bot", None, now).await
                })
            })
            .await
            .unwrap();

        let read = send(
            &app,
            Method::GET,
            &path(other.id, conversation.id),
            &seeded.cookie,
            None,
        )
        .await;
        assert_eq!(read.status, StatusCode::NOT_FOUND);
        let rename = rename_as(&app, other.id, conversation.id, &seeded.cookie, "x").await;
        assert_eq!(rename.status, StatusCode::NOT_FOUND);
        let delete = send(
            &app,
            Method::DELETE,
            &path(other.id, conversation.id),
            &seeded.cookie,
            None,
        )
        .await;
        assert_eq!(delete.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bad_id_is_not_found() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let read = send(
            &app,
            Method::GET,
            &path(seeded.project.id, "not-a-uuid"),
            &seeded.cookie,
            None,
        )
        .await;
        assert_eq!(read.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rename_validates_the_title() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let conversation = create_as(&app, project, &seeded.cookie).await;

        for title in [String::new(), "   ".to_owned(), "a".repeat(81)] {
            let response = rename_as(&app, project, conversation.id, &seeded.cookie, &title).await;
            assert_eq!(response.status, StatusCode::BAD_REQUEST, "{title:?}");
            let problem = response.problem();
            assert_eq!(problem.errors.len(), 1);
            assert_eq!(problem.errors[0].field, "title");
        }

        let longest = rename_as(
            &app,
            project,
            conversation.id,
            &seeded.cookie,
            &"a".repeat(80),
        )
        .await;
        assert_eq!(longest.status, StatusCode::OK);

        let padded = rename_as(
            &app,
            project,
            conversation.id,
            &seeded.cookie,
            "  Checkout errors  ",
        )
        .await;
        assert_eq!(padded.status, StatusCode::OK);
        let renamed: AssistantConversation = padded.json();
        assert_eq!(renamed.title, "Checkout errors");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rename_refuses_control_characters() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let conversation = create_as(&app, project, &seeded.cookie).await;

        for title in ["a\nb", "a\u{7}b"] {
            let response = rename_as(&app, project, conversation.id, &seeded.cookie, title).await;
            assert_eq!(response.status, StatusCode::BAD_REQUEST, "{title:?}");
            let problem = response.problem();
            assert_eq!(problem.errors.len(), 1);
            assert_eq!(problem.errors[0].field, "title");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rename_rejects_unknown_fields() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let conversation = create_as(&app, project, &seeded.cookie).await;
        let response = send(
            &app,
            Method::PATCH,
            &path(project, conversation.id),
            &seeded.cookie,
            Some(json!({"title": "x", "extra": 1})),
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }

    async fn detail_of(
        app: &TestApp,
        project: ProjectId,
        id: Uuid,
        cookie: &str,
    ) -> AssistantConversationDetail {
        let response = send(app, Method::GET, &path(project, id), cookie, None).await;
        assert_eq!(response.status, StatusCode::OK);
        response.json()
    }

    fn feedback_path(project: ProjectId, id: Uuid, seq: impl std::fmt::Display) -> String {
        format!("{}/messages/{seq}/feedback", path(project, id))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn feedback_is_set_and_cleared() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let conversation = create_as(&app, project, &seeded.cookie).await;
        seed_three_messages(&app, conversation.id).await;
        let target = feedback_path(project, conversation.id, 2);

        let down = send(
            &app,
            Method::PUT,
            &target,
            &seeded.cookie,
            Some(json!({"feedback": "down"})),
        )
        .await;
        assert_eq!(down.status, StatusCode::NO_CONTENT);
        let detail = detail_of(&app, project, conversation.id, &seeded.cookie).await;
        assert_eq!(detail.messages[1].feedback, Some(AssistantFeedback::Down));

        let cleared = send(
            &app,
            Method::PUT,
            &target,
            &seeded.cookie,
            Some(json!({"feedback": null})),
        )
        .await;
        assert_eq!(cleared.status, StatusCode::NO_CONTENT);
        let detail = detail_of(&app, project, conversation.id, &seeded.cookie).await;
        assert_eq!(detail.messages[1].feedback, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn feedback_is_404_for_another_user() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let member = app
            .add_member(&seeded, "bob@example.com", OrgRole::Member)
            .await;
        let project = seeded.project.id;
        let conversation = create_as(&app, project, &seeded.cookie).await;
        seed_three_messages(&app, conversation.id).await;

        let response = send(
            &app,
            Method::PUT,
            &feedback_path(project, conversation.id, 2),
            &member.cookie,
            Some(json!({"feedback": "down"})),
        )
        .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND);
        let detail = detail_of(&app, project, conversation.id, &seeded.cookie).await;
        assert_eq!(detail.messages[1].feedback, Some(AssistantFeedback::Up));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn feedback_on_a_user_message_is_400() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let conversation = create_as(&app, project, &seeded.cookie).await;
        seed_three_messages(&app, conversation.id).await;

        let response = send(
            &app,
            Method::PUT,
            &feedback_path(project, conversation.id, 1),
            &seeded.cookie,
            Some(json!({"feedback": "up"})),
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn feedback_on_a_missing_message_is_404() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let conversation = create_as(&app, project, &seeded.cookie).await;
        seed_three_messages(&app, conversation.id).await;

        for seq in ["9", "0", "x"] {
            let response = send(
                &app,
                Method::PUT,
                &feedback_path(project, conversation.id, seq),
                &seeded.cookie,
                Some(json!({"feedback": "up"})),
            )
            .await;
            assert_eq!(response.status, StatusCode::NOT_FOUND, "seq {seq}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn detail_returns_messages_in_order() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let conversation = create_as(&app, project, &seeded.cookie).await;
        seed_three_messages(&app, conversation.id).await;

        let response = send(
            &app,
            Method::GET,
            &path(project, conversation.id),
            &seeded.cookie,
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        let detail: AssistantConversationDetail = response.json();
        assert_eq!(detail.message_count, 3);
        let seqs: Vec<u32> = detail.messages.iter().map(|message| message.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3]);

        let first = &detail.messages[0];
        assert_eq!(first.role, AssistantRole::User);
        assert_eq!(first.status, AssistantMessageStatus::Complete);
        assert_eq!(first.context, Some(json!({"page": "traces"})));
        assert_eq!(
            first.parts,
            vec![AssistantPart::Text {
                content: "Hello".to_owned()
            }]
        );
        assert_eq!(first.feedback, None);
        assert_eq!(first.turn_trace_id, None);

        let second = &detail.messages[1];
        assert_eq!(second.role, AssistantRole::Assistant);
        assert_eq!(second.context, None);
        assert_eq!(second.feedback, Some(AssistantFeedback::Up));
        assert_eq!(second.turn_trace_id.as_deref(), Some(TRACE));
        assert_eq!(
            second.parts,
            vec![
                AssistantPart::ToolCall {
                    id: "call_1".to_owned(),
                    name: "query_sql".to_owned(),
                    arguments: json!({"query": "SELECT 1"}),
                },
                AssistantPart::Draft {
                    id: "draft_1".to_owned(),
                    kind: AssistantDraftKind::Sql,
                    draft: json!({"query": "SELECT 1"}),
                },
            ]
        );

        let third = &detail.messages[2];
        assert_eq!(third.role, AssistantRole::Tool);
        assert_eq!(third.context, None);
        assert_eq!(
            third.parts,
            vec![AssistantPart::ToolResult {
                id: "call_1".to_owned(),
                name: "query_sql".to_owned(),
                ok: true,
                result: Some(json!({"rows": [{"one": 1}]})),
                size_bytes: 20,
                rows: Some(1),
                stubbed: false,
            }]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn corrupt_parts_are_an_internal_error() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let conversation = create_as(&app, project, &seeded.cookie).await;
        seed_three_messages(&app, conversation.id).await;
        let id = conversation.id.to_string();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query(
                        "UPDATE assistant_messages SET parts = 'x' \
                         WHERE conversation_id = ? AND seq = 1",
                    )
                    .bind(id)
                    .execute(&mut *conn)
                    .await
                    .map_err(MetaError::Db)?;
                    Ok(())
                })
            })
            .await
            .unwrap();

        let response = send(
            &app,
            Method::GET,
            &path(project, conversation.id),
            &seeded.cookie,
            None,
        )
        .await;
        assert_eq!(response.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(response.problem().code, ProblemCode::Internal);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn audit_logs_hold_no_title() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let (conversation_id, logs) = capture_logs(async {
            let created = create_as(&app, project, &seeded.cookie).await;
            let renamed =
                rename_as(&app, project, created.id, &seeded.cookie, "secret title").await;
            assert_eq!(renamed.status, StatusCode::OK);
            created.id
        })
        .await;
        assert!(logs.contains("assistant conversation created"), "{logs}");
        assert!(logs.contains("assistant conversation renamed"), "{logs}");
        assert!(logs.contains(&conversation_id.to_string()), "{logs}");
        assert!(!logs.contains("secret title"), "{logs}");
    }
}

#[cfg(test)]
mod settings_tests {
    use std::sync::Arc;

    use axum::http::{Method, StatusCode};
    use cetra_capture::{Mock, Turn};
    use serde_json::{Value, json};
    use uuid::Uuid;

    use crate::auth::roles::OrgRole;
    use crate::http::restore::tests::assert_refused;
    use crate::test_support::{TestApp, TestResponse, capture_logs, second_org};

    fn text(response: &TestResponse) -> String {
        String::from_utf8_lossy(&response.body).into_owned()
    }

    /// A chat turn whose answer is `text`, from the OpenAI-style mock.
    fn text_turn(text: &str) -> Turn {
        serde_json::from_value(json!({
            "api": "openai.chat",
            "model": "gpt-test",
            "message": {"text": text},
            "finish": "stop",
            "usage": {"input": 5, "output": 2},
            "ttft_ms": 0,
            "chunk_delay_ms": 0,
        }))
        .expect("a text turn")
    }

    /// A turn that calls the tool `ping` with `ok` true.
    fn ping_turn() -> Turn {
        serde_json::from_value(json!({
            "api": "openai.chat",
            "model": "gpt-test",
            "message": {"tool_calls": [{"id": "call_1", "name": "ping", "arguments": {"ok": true}}]},
            "finish": "tool_calls",
            "usage": {"input": 5, "output": 2},
            "ttft_ms": 0,
            "chunk_delay_ms": 0,
        }))
        .expect("a ping turn")
    }

    /// Serves `turns` on loopback as an OpenAI-style provider. Returns the mock and the base URL.
    async fn provider(turns: Vec<Turn>) -> (Arc<Mock>, String) {
        let mock = Mock::new(turns);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("address");
        let router = mock.router();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (mock, format!("http://{addr}/v1"))
    }

    /// An app that may call the loopback mock.
    async fn mock_app() -> TestApp {
        TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
        })
        .await
    }

    /// Creates an `openai` connection in `org` and returns its id. Without `base`, the provider's
    /// default address is used, which these tests never call.
    async fn connection(app: &TestApp, cookie: &str, org: &str, base: Option<&str>) -> Uuid {
        let mut body = json!({
            "name": format!("Mocked {}", Uuid::now_v7()),
            "adapter": "openai",
            "api_key": "sk-test-key-0123456789",
            "default_model": "gpt-test",
        });
        if let Some(base) = base {
            body["base_url"] = json!(base);
        }
        let response = app
            .request(Method::POST, &format!("/api/v1/orgs/{org}/llm-connections"))
            .header("cookie", cookie)
            .json(&body)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::CREATED, "{}", text(&response));
        let value: Value = response.json();
        value["id"].as_str().expect("id").parse().expect("uuid")
    }

    async fn connection_status(app: &TestApp, cookie: &str, id: Uuid) -> String {
        let response = app
            .request(
                Method::GET,
                &format!("/api/v1/orgs/acme/llm-connections/{id}"),
            )
            .header("cookie", cookie)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", text(&response));
        let value: Value = response.json();
        value["status"].as_str().expect("status").to_owned()
    }

    async fn put_settings(app: &TestApp, cookie: &str, body: &Value) -> TestResponse {
        app.request(Method::PUT, "/api/v1/orgs/acme/assistant")
            .header("cookie", cookie)
            .json(body)
            .send()
            .await
    }

    async fn read_settings(app: &TestApp, cookie: &str) -> Value {
        let response = app
            .request(Method::GET, "/api/v1/orgs/acme/assistant")
            .header("cookie", cookie)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", text(&response));
        response.json()
    }

    async fn read_status(app: &TestApp, cookie: &str, project: &str) -> Value {
        let response = app
            .request(
                Method::GET,
                &format!("/api/v1/projects/{project}/assistant"),
            )
            .header("cookie", cookie)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", text(&response));
        response.json()
    }

    #[tokio::test]
    async fn settings_default_to_off() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let member = app
            .add_member(&seeded, "member@example.com", OrgRole::Member)
            .await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        for cookie in [&seeded.cookie, &member.cookie, &viewer.cookie] {
            assert_eq!(read_settings(&app, cookie).await, json!({"enabled": false}));
        }
    }

    #[tokio::test]
    async fn assistant_setting_change_is_recorded() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let response = put_settings(&app, &seeded.cookie, &json!({"enabled": false})).await;
        assert_eq!(response.status, StatusCode::OK, "{}", text(&response));
        let rows: Vec<(Option<String>, String)> = sqlx::query_as(
            "SELECT org_id, details FROM audit_events WHERE action = 'assistant.setting_changed'",
        )
        .fetch_all(app.state.meta.reader())
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, Some(seeded.org_id.to_string()));
        let details: Value = serde_json::from_str(&rows[0].1).unwrap();
        assert_eq!(details, json!({"enabled": false, "connection_id": null}));
    }

    #[tokio::test]
    async fn only_admins_write() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let member = app
            .add_member(&seeded, "member@example.com", OrgRole::Member)
            .await;
        let response = put_settings(&app, &member.cookie, &json!({"enabled": false})).await;
        assert_eq!(
            response.status,
            StatusCode::FORBIDDEN,
            "{}",
            text(&response)
        );
    }

    #[tokio::test]
    async fn enabling_needs_connection_and_model() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let neither = put_settings(&app, &seeded.cookie, &json!({"enabled": true})).await;
        assert_eq!(
            neither.status,
            StatusCode::BAD_REQUEST,
            "{}",
            text(&neither)
        );
        assert!(text(&neither).contains("connection_id"));
        assert!(text(&neither).contains("model"));
        let id = connection(&app, &seeded.cookie, "acme", None).await;
        let no_model = put_settings(
            &app,
            &seeded.cookie,
            &json!({"enabled": true, "connection_id": id}),
        )
        .await;
        assert_eq!(
            no_model.status,
            StatusCode::BAD_REQUEST,
            "{}",
            text(&no_model)
        );
        assert!(text(&no_model).contains("model"));
        let no_connection = put_settings(
            &app,
            &seeded.cookie,
            &json!({"enabled": true, "model": "gpt-test"}),
        )
        .await;
        assert_eq!(
            no_connection.status,
            StatusCode::BAD_REQUEST,
            "{}",
            text(&no_connection)
        );
        assert!(text(&no_connection).contains("connection_id"));
        assert_eq!(
            read_settings(&app, &seeded.cookie).await,
            json!({"enabled": false})
        );
    }

    #[tokio::test]
    async fn enabling_runs_the_test() {
        let (_mock, base) = provider(vec![text_turn("No tool here."), ping_turn()]).await;
        let app = mock_app().await;
        let seeded = app.seed().await;
        let id = connection(&app, &seeded.cookie, "acme", Some(&base)).await;
        let body = json!({"enabled": true, "connection_id": id, "model": "gpt-test"});

        // The model answers in text: the test fails, nothing is stored, the connection stays ok.
        let failed = put_settings(&app, &seeded.cookie, &body).await;
        assert_eq!(failed.status, StatusCode::BAD_REQUEST, "{}", text(&failed));
        assert!(text(&failed).contains("the model must support tool calling"));
        assert_eq!(
            read_settings(&app, &seeded.cookie).await,
            json!({"enabled": false})
        );
        assert_eq!(connection_status(&app, &seeded.cookie, id).await, "ok");

        // The model calls `ping`: the setting is stored.
        let saved = put_settings(&app, &seeded.cookie, &body).await;
        assert_eq!(saved.status, StatusCode::OK, "{}", text(&saved));
        let stored = read_settings(&app, &seeded.cookie).await;
        assert_eq!(stored["enabled"], true);
        assert_eq!(stored["connection_id"], json!(id));
        assert_eq!(stored["model"], "gpt-test");
        assert!(stored["updated_at"].is_string());
        assert_eq!(connection_status(&app, &seeded.cookie, id).await, "ok");
    }

    #[tokio::test]
    async fn disabling_needs_no_test() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let response = put_settings(&app, &seeded.cookie, &json!({"enabled": false})).await;
        assert_eq!(response.status, StatusCode::OK, "{}", text(&response));
        let stored = read_settings(&app, &seeded.cookie).await;
        assert_eq!(stored["enabled"], false);
        assert!(stored["updated_at"].is_string());
    }

    #[tokio::test]
    async fn connection_of_another_org_is_400() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (_other, owner) = second_org(&app, "other", "owner@example.com").await;
        let foreign = connection(&app, &owner.cookie, "other", None).await;
        let response = put_settings(
            &app,
            &seeded.cookie,
            &json!({"enabled": false, "connection_id": foreign}),
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "{}",
            text(&response)
        );
        assert!(text(&response).contains("connection not found in this organization"));
    }

    #[tokio::test]
    async fn deleting_the_connection_turns_it_off() {
        let (_mock, base) = provider(vec![ping_turn()]).await;
        let app = mock_app().await;
        let seeded = app.seed().await;
        let id = connection(&app, &seeded.cookie, "acme", Some(&base)).await;
        let saved = put_settings(
            &app,
            &seeded.cookie,
            &json!({"enabled": true, "connection_id": id, "model": "gpt-test"}),
        )
        .await;
        assert_eq!(saved.status, StatusCode::OK, "{}", text(&saved));
        let deleted = app
            .request(
                Method::DELETE,
                &format!("/api/v1/orgs/acme/llm-connections/{id}"),
            )
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", text(&deleted));
        let status = read_status(&app, &seeded.cookie, &seeded.project.id.to_string()).await;
        assert_eq!(status, json!({"usable": false, "reason": "no_connection"}));
    }

    #[tokio::test]
    async fn status_reasons() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let member = app
            .add_member(&seeded, "member@example.com", OrgRole::Member)
            .await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let project = seeded.project.id.to_string();
        let as_viewer = read_status(&app, &viewer.cookie, &project).await;
        assert_eq!(as_viewer, json!({"usable": false, "reason": "forbidden"}));
        let as_member = read_status(&app, &member.cookie, &project).await;
        assert_eq!(as_member, json!({"usable": false, "reason": "disabled"}));
    }

    #[tokio::test]
    async fn usable_status_names_the_connection() {
        let (_mock, base) = provider(vec![ping_turn()]).await;
        let app = mock_app().await;
        let seeded = app.seed().await;
        let member = app
            .add_member(&seeded, "member@example.com", OrgRole::Member)
            .await;
        let id = connection(&app, &seeded.cookie, "acme", Some(&base)).await;
        let saved = put_settings(
            &app,
            &seeded.cookie,
            &json!({"enabled": true, "connection_id": id, "model": "gpt-test"}),
        )
        .await;
        assert_eq!(saved.status, StatusCode::OK, "{}", text(&saved));
        let status = read_status(&app, &member.cookie, &seeded.project.id.to_string()).await;
        assert_eq!(status["usable"], true);
        assert_eq!(
            status["connection_name"]
                .as_str()
                .map(|name| name.starts_with("Mocked")),
            Some(true)
        );
        assert_eq!(status["adapter"], "openai");
        assert_eq!(status["model"], "gpt-test");
        assert!(status.get("reason").is_none());
    }

    #[tokio::test]
    async fn restore_mode_is_its_own_reason() {
        let app = TestApp::with_config(|config| {
            config.restore_mode = true;
        })
        .await;
        let seeded = app.seed().await;
        let status = read_status(&app, &seeded.cookie, &seeded.project.id.to_string()).await;
        assert_eq!(status, json!({"usable": false, "reason": "restore_mode"}));
    }

    #[tokio::test]
    async fn test_route_reports_a_failed_call() {
        let (_mock, base) = provider(vec![text_turn("I cannot call tools.")]).await;
        let app = mock_app().await;
        let seeded = app.seed().await;
        let id = connection(&app, &seeded.cookie, "acme", Some(&base)).await;
        let response = app
            .request(Method::POST, "/api/v1/orgs/acme/assistant/test")
            .header("cookie", &seeded.cookie)
            .json(&json!({"connection_id": id, "model": "gpt-test"}))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", text(&response));
        let value: Value = response.json();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["class"], "permanent");
        assert_eq!(connection_status(&app, &seeded.cookie, id).await, "ok");
    }

    #[tokio::test]
    async fn restore_mode_refuses_the_test() {
        let app = TestApp::with_config(|config| {
            config.restore_mode = true;
        })
        .await;
        let seeded = app.seed().await;
        let response = app
            .request(Method::POST, "/api/v1/orgs/acme/assistant/test")
            .header("cookie", &seeded.cookie)
            .json(&json!({"connection_id": Uuid::now_v7(), "model": "gpt-test"}))
            .send()
            .await;
        assert_refused(&response, "assistant test");
    }

    #[tokio::test]
    async fn audit_log_has_no_model_answer() {
        const ANSWER: &str = "ANSWER-MARKER-7f3a9c";
        let (_mock, base) = provider(vec![text_turn(ANSWER)]).await;
        let app = mock_app().await;
        let seeded = app.seed().await;
        let id = connection(&app, &seeded.cookie, "acme", Some(&base)).await;
        let body = json!({"enabled": true, "connection_id": id, "model": "gpt-test"});
        let (response, logs) = capture_logs(put_settings(&app, &seeded.cookie, &body)).await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "{}",
            text(&response)
        );
        assert!(!text(&response).contains(ANSWER));
        assert!(!logs.contains(ANSWER));
    }
}

#[cfg(test)]
mod turn_tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use axum::http::{Method, StatusCode};
    use cetra_capture::{Mock, Turn};
    use serde_json::{Value, json};
    use uuid::Uuid;

    use crate::auth::roles::OrgRole;
    use crate::http::restore::tests::assert_refused;
    use crate::meta::MetaError;
    use crate::meta::assistant::insert_message_raw;
    use crate::test_support::{Seeded, TestApp, TestResponse, TestStream};

    /// How long a test waits for the next event.
    const WAIT: Duration = Duration::from_secs(10);

    fn text(response: &TestResponse) -> String {
        String::from_utf8_lossy(&response.body).into_owned()
    }

    fn real_now() -> i64 {
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos(),
        )
        .expect("fits i64")
    }

    /// An app whose clock is the real time and that may call the loopback mock.
    async fn mock_app() -> TestApp {
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
        })
        .await;
        app.state.clock.set(real_now());
        app
    }

    /// A chat turn whose answer is `text`, from the OpenAI-style mock.
    fn text_turn(text: &str) -> Turn {
        serde_json::from_value(json!({
            "api": "openai.chat",
            "model": "gpt-test",
            "message": {"text": text},
            "finish": "stop",
            "usage": {"input": 5, "output": 2},
            "ttft_ms": 0,
            "chunk_delay_ms": 0,
        }))
        .expect("a text turn")
    }

    /// A turn that calls the tool `ping` with `ok` true: the org setting's test (D-AS-24).
    fn ping_turn() -> Turn {
        serde_json::from_value(json!({
            "api": "openai.chat",
            "model": "gpt-test",
            "message": {"tool_calls": [{"id": "call_1", "name": "ping", "arguments": {"ok": true}}]},
            "finish": "tool_calls",
            "usage": {"input": 5, "output": 2},
            "ttft_ms": 0,
            "chunk_delay_ms": 0,
        }))
        .expect("a ping turn")
    }

    /// A turn that waits five seconds before it answers, so the turn stays open.
    fn slow_turn() -> Turn {
        serde_json::from_value(json!({
            "api": "openai.chat",
            "model": "gpt-test",
            "message": {"text": "Late."},
            "finish": "stop",
            "usage": {"input": 1, "output": 1},
            "ttft_ms": 5000,
            "chunk_delay_ms": 0,
        }))
        .expect("a slow turn")
    }

    /// Serves `turns` on loopback as an OpenAI-style provider. Returns the mock and the base URL.
    async fn provider(turns: Vec<Turn>) -> (Arc<Mock>, String) {
        let mock = Mock::new(turns);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("address");
        let router = mock.router();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (mock, format!("http://{addr}/v1"))
    }

    /// Creates an `openai` connection in the seeded org `acme`, served at `base`.
    async fn connection(app: &TestApp, seeded: &Seeded, base: &str) -> Uuid {
        let response = app
            .request(Method::POST, "/api/v1/orgs/acme/llm-connections")
            .header("cookie", &seeded.cookie)
            .json(&json!({
                "name": format!("Mocked {}", Uuid::now_v7()),
                "adapter": "openai",
                "api_key": "sk-test-key-0123456789",
                "base_url": base,
                "default_model": "gpt-test",
            }))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::CREATED, "{}", text(&response));
        let value: Value = response.json();
        value["id"].as_str().expect("id").parse().expect("uuid")
    }

    /// Turns the assistant on with `connection` and `gpt-test`. The setting's test takes one
    /// `ping` turn from the mock.
    async fn enable(app: &TestApp, seeded: &Seeded, connection: Uuid) {
        let response = app
            .request(Method::PUT, "/api/v1/orgs/acme/assistant")
            .header("cookie", &seeded.cookie)
            .json(&json!({"enabled": true, "connection_id": connection, "model": "gpt-test"}))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", text(&response));
    }

    fn conversations_path(seeded: &Seeded) -> String {
        format!(
            "/api/v1/projects/{}/assistant/conversations",
            seeded.project.id
        )
    }

    fn turns_path(seeded: &Seeded, conversation: Uuid) -> String {
        format!("{}/{conversation}/turns", conversations_path(seeded))
    }

    async fn create_conversation(app: &TestApp, seeded: &Seeded) -> Uuid {
        let response = app
            .request(Method::POST, &conversations_path(seeded))
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::CREATED, "{}", text(&response));
        let value: Value = response.json();
        value["id"].as_str().expect("id").parse().expect("uuid")
    }

    /// Posts a turn as `cookie` and answers the stream (its status says whether it was admitted).
    async fn post_turn(
        app: &TestApp,
        seeded: &Seeded,
        cookie: &str,
        conversation: Uuid,
        body: &Value,
    ) -> TestStream {
        app.request(Method::POST, &turns_path(seeded, conversation))
            .header("cookie", cookie)
            .json(body)
            .send_stream()
            .await
    }

    /// Reads a turn's events to its end: the `(event, data)` pairs.
    async fn read_turn(stream: &mut TestStream) -> Vec<(String, Value)> {
        let mut events = Vec::new();
        while let Some(event) = stream.next_non_ping(WAIT).await {
            let data = serde_json::from_str(&event.data).unwrap_or(Value::Null);
            events.push((event.event, data));
        }
        events
    }

    fn event_names(events: &[(String, Value)]) -> Vec<&str> {
        events.iter().map(|event| event.0.as_str()).collect()
    }

    async fn detail(app: &TestApp, seeded: &Seeded, conversation: Uuid) -> Value {
        let response = app
            .request(
                Method::GET,
                &format!("{}/{conversation}", conversations_path(seeded)),
            )
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", text(&response));
        response.json()
    }

    /// Stores `count` user messages in `conversation` and sets its count to `count`: the raw
    /// insert does not set the count.
    async fn fill(app: &TestApp, conversation: Uuid, count: i64) {
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    for seq in 1..=count {
                        insert_message_raw(
                            conn,
                            conversation,
                            seq,
                            "user",
                            r#"[{"type":"text","content":"Filler"}]"#,
                            None,
                            None,
                            now,
                        )
                        .await?;
                    }
                    sqlx::query(
                        "UPDATE assistant_conversations SET message_count = ? WHERE id = ?",
                    )
                    .bind(count)
                    .bind(conversation.to_string())
                    .execute(&mut *conn)
                    .await
                    .map_err(MetaError::Db)?;
                    Ok(())
                })
            })
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn streams_a_turn() {
        let (_mock, base) = provider(vec![
            ping_turn(),
            text_turn("Checkout errors rose at noon."),
        ])
        .await;
        let app = mock_app().await;
        let seeded = app.seed().await;
        let connection = connection(&app, &seeded, &base).await;
        enable(&app, &seeded, connection).await;
        let conversation = create_conversation(&app, &seeded).await;
        let message = "Why did checkout errors spike yesterday?";
        let mut stream = post_turn(
            &app,
            &seeded,
            &seeded.cookie,
            conversation,
            &json!({"message": message}),
        )
        .await;
        assert_eq!(stream.status, StatusCode::OK);
        let events = read_turn(&mut stream).await;
        let names = event_names(&events);
        assert_eq!(names.first(), Some(&"start"), "{names:?}");
        assert!(names.contains(&"text"), "{names:?}");
        assert_eq!(names.last(), Some(&"done"), "{names:?}");
        let done = &events.last().expect("a done event").1;
        assert_eq!(done["steps"], 1);

        let stored = detail(&app, &seeded, conversation).await;
        assert_eq!(stored["title"], message);
        let messages = stored["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["status"], "complete");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_validation() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let conversation = create_conversation(&app, &seeded).await;
        let cases = [
            (json!({"message": "   "}), "message"),
            (json!({"message": "a".repeat(16_385)}), "message"),
            (json!({"message": "hi", "context": "traces"}), "context"),
            (
                json!({"message": "hi", "context": {"page": "traces", "pad": "x".repeat(9_000)}}),
                "context",
            ),
            (
                json!({"message": "hi", "context": {"filter": "x"}}),
                "context.page",
            ),
        ];
        for (body, field) in cases {
            let response = app
                .request(Method::POST, &turns_path(&seeded, conversation))
                .header("cookie", &seeded.cookie)
                .json(&body)
                .send()
                .await;
            assert_eq!(
                response.status,
                StatusCode::BAD_REQUEST,
                "{}",
                text(&response)
            );
            let value: Value = response.json();
            assert!(
                value
                    .to_string()
                    .contains(&format!("\"field\":\"{field}\"")),
                "{field}: {value}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn assistant_off_is_409() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let conversation = create_conversation(&app, &seeded).await;
        let response = app
            .request(Method::POST, &turns_path(&seeded, conversation))
            .header("cookie", &seeded.cookie)
            .json(&json!({"message": "hi"}))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::CONFLICT, "{}", text(&response));
        assert!(text(&response).contains("the assistant is off"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_turn_per_conversation() {
        let (_mock, base) =
            provider(vec![ping_turn(), slow_turn(), text_turn("Another chat.")]).await;
        let app = mock_app().await;
        let seeded = app.seed().await;
        let connection = connection(&app, &seeded, &base).await;
        enable(&app, &seeded, connection).await;
        let busy = create_conversation(&app, &seeded).await;
        let other = create_conversation(&app, &seeded).await;

        let mut first = post_turn(
            &app,
            &seeded,
            &seeded.cookie,
            busy,
            &json!({"message": "one"}),
        )
        .await;
        assert_eq!(first.status, StatusCode::OK);
        let start = first.next_non_ping(WAIT).await.expect("the start event");
        assert_eq!(start.event, "start");

        let second = post_turn(
            &app,
            &seeded,
            &seeded.cookie,
            busy,
            &json!({"message": "two"}),
        )
        .await;
        assert_eq!(second.status, StatusCode::CONFLICT);

        let mut third = post_turn(
            &app,
            &seeded,
            &seeded.cookie,
            other,
            &json!({"message": "three"}),
        )
        .await;
        assert_eq!(third.status, StatusCode::OK);
        let events = read_turn(&mut third).await;
        assert_eq!(event_names(&events).last(), Some(&"done"));
        drop(first);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn viewer_pat_and_key_are_refused() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let conversation = create_conversation(&app, &seeded).await;
        let body = json!({"message": "hi"});
        let token = app.personal_token(seeded.user_id, true).await;
        let bearer_token = format!("Bearer {token}");
        let bearer_key = format!("Bearer {}", seeded.read_key);

        let viewer_stream = post_turn(&app, &seeded, &viewer.cookie, conversation, &body).await;
        assert_eq!(viewer_stream.status, StatusCode::FORBIDDEN);
        for header in [&bearer_token, &bearer_key] {
            let response = app
                .request(Method::POST, &turns_path(&seeded, conversation))
                .header("authorization", header)
                .json(&body)
                .send()
                .await;
            assert_eq!(
                response.status,
                StatusCode::FORBIDDEN,
                "{}",
                text(&response)
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn another_users_conversation_is_404() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let member = app
            .add_member(&seeded, "member@example.com", OrgRole::Member)
            .await;
        let conversation = create_conversation(&app, &seeded).await;
        let stream = post_turn(
            &app,
            &seeded,
            &member.cookie,
            conversation,
            &json!({"message": "hi"}),
        )
        .await;
        assert_eq!(stream.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn full_conversation_is_409() {
        let (_mock, base) = provider(vec![ping_turn(), text_turn("Room left.")]).await;
        let app = mock_app().await;
        let seeded = app.seed().await;
        let connection = connection(&app, &seeded, &base).await;
        enable(&app, &seeded, connection).await;

        let full = create_conversation(&app, &seeded).await;
        fill(&app, full, 167).await;
        let refused = app
            .request(Method::POST, &turns_path(&seeded, full))
            .header("cookie", &seeded.cookie)
            .json(&json!({"message": "hi"}))
            .send()
            .await;
        assert_eq!(refused.status, StatusCode::CONFLICT, "{}", text(&refused));
        assert!(text(&refused).contains("the conversation is full"));

        let room = create_conversation(&app, &seeded).await;
        fill(&app, room, 166).await;
        let mut stream = post_turn(
            &app,
            &seeded,
            &seeded.cookie,
            room,
            &json!({"message": "hi"}),
        )
        .await;
        assert_eq!(stream.status, StatusCode::OK);
        let events = read_turn(&mut stream).await;
        assert_eq!(event_names(&events).last(), Some(&"done"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restore_mode_refuses() {
        let app = TestApp::with_config(|config| config.restore_mode = true).await;
        let seeded = app.seed().await;
        let conversation = create_conversation_in_restore(&app, &seeded).await;
        let response = app
            .request(Method::POST, &turns_path(&seeded, conversation))
            .header("cookie", &seeded.cookie)
            .json(&json!({"message": "hi"}))
            .send()
            .await;
        assert_refused(&response, "assistant turn");
    }

    /// A conversation stored with `insert_conversation`, so the restore-mode test does not depend
    /// on the create route's behaviour in restore mode.
    async fn create_conversation_in_restore(app: &TestApp, seeded: &Seeded) -> Uuid {
        let id = Uuid::now_v7();
        let now = app.state.clock.now_ns();
        let project = seeded.project.id;
        let user = seeded.user_id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    crate::meta::assistant::insert_conversation(conn, id, project, user, now)
                        .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        id
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn abort_cancels_the_turn() {
        let (_mock, base) =
            provider(vec![ping_turn(), slow_turn(), text_turn("Back again.")]).await;
        let app = mock_app().await;
        let seeded = app.seed().await;
        let connection = connection(&app, &seeded, &base).await;
        enable(&app, &seeded, connection).await;
        let conversation = create_conversation(&app, &seeded).await;

        let mut stream = post_turn(
            &app,
            &seeded,
            &seeded.cookie,
            conversation,
            &json!({"message": "slow"}),
        )
        .await;
        let start = stream.next_non_ping(WAIT).await.expect("the start event");
        assert_eq!(start.event, "start");
        // The client leaves while the provider is still waiting.
        drop(stream);

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let stored = detail(&app, &seeded, conversation).await;
            let cancelled = stored["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["role"] == "assistant" && message["status"] == "cancelled"
                })
            });
            if cancelled {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the cancelled turn stored no message"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // The turn's slot is released once the engine returns, so a new turn is admitted soon.
        let mut again = loop {
            let stream = post_turn(
                &app,
                &seeded,
                &seeded.cookie,
                conversation,
                &json!({"message": "again"}),
            )
            .await;
            if stream.status == StatusCode::OK {
                break stream;
            }
            assert_eq!(stream.status, StatusCode::CONFLICT);
            assert!(
                Instant::now() < deadline + Duration::from_secs(2),
                "the slot was not freed"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let events = read_turn(&mut again).await;
        assert_eq!(event_names(&events).last(), Some(&"done"));
    }
}
