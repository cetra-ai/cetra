//! Assistant conversation routes (spec 09 § Assistant, D-AS-4, D-AS-10).
//!
//! Every route needs [`ProjectPerm::UseAssistant`] with a session: member and above, no API key,
//! no personal access token. A conversation is private to its author, so another user's id, and
//! an id of another project, answer `404`.

use axum::extract::{Extension, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use serde::Deserialize;
use uuid::Uuid;

use cetra_core::ids::ProjectId;

use crate::api::assistant::{
    AssistantConversation, AssistantConversationDetail, AssistantFeedback, AssistantMessage,
    AssistantMessageStatus, AssistantPart, AssistantRole, UpdateAssistantConversationRequest,
};
use crate::api::common::Page;
use crate::auth::principal::{self, Principal};
use crate::auth::roles::ProjectPerm;
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError, field_error};
use crate::meta::MetaError;
use crate::meta::assistant::{
    self as store, ConversationRow, Feedback, MessageRole, MessageRow, MessageStatus,
};

/// Longest conversation title, in characters after trimming (D-AS-10).
const MAX_TITLE_CHARS: usize = 80;

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
    ]
}

#[derive(Debug, Deserialize)]
struct ConversationPath {
    id: String,
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
        .map(serde_json::from_str::<serde_json::Value>)
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
    tracing::info!(target: "audit", conversation_id = %id, project_id = %project, user_id = %user, "assistant conversation created");
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
    let row = store::get_conversation(state.meta.reader(), project, user, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let messages = store::list_messages(state.meta.reader(), id).await?;
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
    tracing::info!(target: "audit", conversation_id = %id, project_id = %project, user_id = %user, "assistant conversation renamed");
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
        tracing::info!(target: "audit", conversation_id = %id, project_id = %project, user_id = %user, "assistant conversation deleted");
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
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
