//! Annotation queues (spec 09 § Annotation queues, spec 04 § Annotation queues, D-AQ-2–D-AQ-11).
//!
//! Reads need project view and keys read them. Queue writes need [`ProjectPerm::ManageQueues`]:
//! member and above through a session, a read-write personal access token or a `write` API key.
//! A read-only personal access token cannot write (the access layer answers 403).

use std::collections::HashSet;
use std::str::FromStr;

use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use cetra_core::ids::TraceId;

use crate::api::annotation_queues::{
    AnnotationQueue, AnnotationQueueListParams, CreateAnnotationQueue, QueueCounts, QueueTarget,
    UpdateAnnotationQueue,
};
use crate::api::common::{FieldError, Page};
use crate::auth::principal::{self, Principal};
use crate::auth::roles::ProjectPerm;
use crate::entitlements::Visibility;
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError, field_error};
use crate::meta::annotation_queues::{
    self as store, Counts, MAX_DEFINITIONS, NewQueue, QueueChanges, QueueCreate, QueueRow,
    QueueUpdate, Target,
};
use crate::routes::read::page_limit;

/// A queue body is a name, two texts and up to 50 ids (D-AQ-7).
const QUEUE_BODY_LIMIT: usize = 64 * 1024;
/// Longest queue name, in characters after trimming.
const MAX_NAME_CHARS: usize = 100;
/// Longest queue description, in characters after trimming.
const MAX_DESCRIPTION_CHARS: usize = 1_000;
/// Longest queue instructions, in characters after trimming.
const MAX_INSTRUCTIONS_CHARS: usize = 10_000;
/// Longest session id, in bytes.
const MAX_SESSION_ID_BYTES: usize = 255;

/// Annotation queue routes (spec 09). The item routes are added in their own phase.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/annotation-queues",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_queues),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/annotation-queues",
            access: Access::Project {
                perm: ProjectPerm::ManageQueues,
                session_only: false,
            },
            documented: true,
            handler: post(create_queue).layer(DefaultBodyLimit::max(QUEUE_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/annotation-queues/{id}",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_queue),
        },
        RouteDef {
            method: Method::PATCH,
            path: "/api/v1/projects/{project}/annotation-queues/{id}",
            access: Access::Project {
                perm: ProjectPerm::ManageQueues,
                session_only: false,
            },
            documented: true,
            handler: patch(update_queue).layer(DefaultBodyLimit::max(QUEUE_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::DELETE,
            path: "/api/v1/projects/{project}/annotation-queues/{id}",
            access: Access::Project {
                perm: ProjectPerm::ManageQueues,
                session_only: false,
            },
            documented: true,
            handler: delete(delete_queue),
        },
    ]
}

#[derive(Debug, Deserialize)]
struct QueuePath {
    id: Uuid,
}

/// The position a cursor names: the `(name, id)` of the previous page's last queue.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueueCursor {
    name: String,
    id: Uuid,
}

/// `GET /annotation-queues`: the project's queues by name then id, with their counts. A trace or
/// session target keeps the queues that hold a visible item of it.
async fn list_queues(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<AnnotationQueueListParams>,
) -> Result<error::Json<Page<AnnotationQueue>>, ApiError> {
    let project = access.project.id;
    let after = params.cursor.as_deref().map(decode_cursor).transpose()?;
    let trace = params
        .target_trace_id
        .as_deref()
        .map(|text| {
            TraceId::from_str(text)
                .map(|id| id.to_hex())
                .map_err(|_| field_error("target_trace_id", "must be 32 hex digits"))
        })
        .transpose()?;
    let session = params.target_session_id.as_deref();
    if session.is_some_and(|text| text.is_empty() || text.len() > MAX_SESSION_ID_BYTES) {
        return Err(field_error("target_session_id", "must be 1 to 255 bytes"));
    }
    let holding = match (trace.as_deref(), session) {
        (Some(_), Some(_)) => {
            return Err(field_error("target_session_id", "give one target"));
        }
        (Some(trace), None) => Some(store::Holding::Trace(trace)),
        (None, Some(session)) => Some(store::Holding::Session(session)),
        (None, None) => None,
    };
    let visible_from = Visibility::window(&state, access.project.org_id).visible_from();
    let (rows, next) = store::list(
        state.meta.reader(),
        project,
        after.as_ref().map(|(name, id)| (name.as_str(), *id)),
        page_limit(params.limit),
        holding,
        visible_from,
    )
    .await?;
    let ids: Vec<Uuid> = rows.iter().map(|row| row.id).collect();
    let counts = store::counts(state.meta.reader(), project, &ids, visible_from).await?;
    let next_cursor = next
        .as_ref()
        .map(|(name, id)| encode_cursor(name, *id))
        .transpose()?;
    Ok(error::Json(Page {
        data: rows
            .iter()
            .map(|row| queue_json(row, counts.get(&row.id)))
            .collect(),
        next_cursor,
    }))
}

/// `GET /annotation-queues/{id}`: one queue with its counts. Any other id is 404.
async fn get_queue(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<QueuePath>,
) -> Result<error::Json<AnnotationQueue>, ApiError> {
    let project = access.project.id;
    let row = store::get(state.meta.reader(), project, path.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let visible_from = Visibility::window(&state, access.project.org_id).visible_from();
    let counts = store::counts(state.meta.reader(), project, &[row.id], visible_from).await?;
    Ok(error::Json(queue_json(&row, counts.get(&row.id))))
}

/// `POST /annotation-queues`: creates a queue at version 1 and answers 201. The org's plan may
/// cap the queues (`max_annotation_queues`, D-AQ-10): past it is 403 `plan_limit`.
async fn create_queue(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Json(body): error::Json<CreateAnnotationQueue>,
) -> Result<Response, ApiError> {
    // A key creates a queue with no creator: the column names a person.
    let created_by = principal::session_user(&principal).ok().map(|user| user.id);
    let project = access.project.id;
    let org = access.project.org_id;
    let mut errors = Vec::new();
    let name = checked_name(&body.name, "name", &mut errors);
    let description = body
        .description
        .as_deref()
        .and_then(|text| checked_text(text, "description", MAX_DESCRIPTION_CHARS, &mut errors))
        .flatten();
    let instructions = body
        .instructions
        .as_deref()
        .and_then(|text| checked_text(text, "instructions", MAX_INSTRUCTIONS_CHARS, &mut errors))
        .flatten();
    check_definition_ids(&body.definition_ids, &mut errors);
    refuse_if_any(errors)?;

    let plan_limit = crate::entitlements::of(&state, org)
        .limits
        .max_annotation_queues;
    let target = store_target(body.target);
    let definition_ids = body.definition_ids;
    let now = state.clock.now_ns();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                store::insert_queue_tx(
                    conn,
                    project,
                    org,
                    NewQueue {
                        name: name.unwrap_or_default(),
                        description,
                        instructions,
                        target,
                        definition_ids,
                        created_by,
                    },
                    plan_limit,
                    now,
                )
                .await
            })
        })
        .await?;
    match outcome {
        QueueCreate::Created(row) => {
            tracing::info!(project_id = %project, queue_id = %row.id, "annotation queue created");
            let json = queue_json(&row, None);
            Ok((StatusCode::CREATED, error::Json(json)).into_response())
        }
        QueueCreate::OverPlanLimit(limit) => Err(crate::entitlements::plan_limit(
            limit,
            ("annotation queue", "annotation queues"),
        )),
        QueueCreate::UnknownDefinitions(indexes) => Err(unknown_definitions(&indexes)),
    }
}

/// `PATCH /annotation-queues/{id}`: changes the fields given. `version` must be the stored one
/// (409 with the stored version otherwise). A `""` description or instructions clears it.
async fn update_queue(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<QueuePath>,
    error::Json(body): error::Json<UpdateAnnotationQueue>,
) -> Result<error::Json<AnnotationQueue>, ApiError> {
    let project = access.project.id;
    let mut errors = Vec::new();
    let name = body
        .name
        .as_deref()
        .and_then(|name| checked_name(name, "name", &mut errors));
    // Omitted: unchanged. `""`: cleared (stored as none). Other text: replaced.
    let description = body.description.as_deref().map(|text| {
        checked_text(text, "description", MAX_DESCRIPTION_CHARS, &mut errors).flatten()
    });
    let instructions = body.instructions.as_deref().map(|text| {
        checked_text(text, "instructions", MAX_INSTRUCTIONS_CHARS, &mut errors).flatten()
    });
    if let Some(ids) = &body.definition_ids {
        check_definition_ids(ids, &mut errors);
    }
    refuse_if_any(errors)?;

    let changes = QueueChanges {
        name,
        description,
        instructions,
        definition_ids: body.definition_ids,
    };
    let version = body.version;
    let id = path.id;
    let now = state.clock.now_ns();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(
                async move { store::update_tx(conn, project, id, version, changes, now).await },
            )
        })
        .await?;
    match outcome {
        QueueUpdate::Updated(row) => {
            tracing::info!(project_id = %project, queue_id = %id, "annotation queue updated");
            let visible_from = Visibility::window(&state, access.project.org_id).visible_from();
            let counts =
                store::counts(state.meta.reader(), project, &[row.id], visible_from).await?;
            Ok(error::Json(queue_json(&row, counts.get(&row.id))))
        }
        QueueUpdate::Stale(current) => Err(ApiError::StaleVersion {
            what: "annotation queue",
            current,
        }),
        QueueUpdate::Missing => Err(ApiError::NotFound),
        QueueUpdate::UnknownDefinitions(indexes) => Err(unknown_definitions(&indexes)),
    }
}

/// `DELETE /annotation-queues/{id}`: deletes the queue and its items. The score definitions and
/// the scores stay. Answers 204, or 404.
async fn delete_queue(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<QueuePath>,
) -> Result<StatusCode, ApiError> {
    let project = access.project.id;
    let id = path.id;
    let deleted = state
        .meta
        .write(move |conn| Box::pin(async move { store::delete(conn, project, id).await }))
        .await?;
    if deleted {
        tracing::info!(project_id = %project, queue_id = %id, "annotation queue deleted");
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

/// The API form of a stored queue. A missing count is zero.
fn queue_json(row: &QueueRow, counts: Option<&Counts>) -> AnnotationQueue {
    let counts = counts.cloned().unwrap_or_default();
    AnnotationQueue {
        id: row.id,
        name: row.name.clone(),
        description: row.description.clone(),
        instructions: row.instructions.clone(),
        target: api_target(row.target),
        definition_ids: row.definition_ids.clone(),
        version: row.version,
        created_by: row.created_by,
        created_at: row.created_at,
        updated_at: row.updated_at,
        counts: QueueCounts {
            pending: counts.pending,
            completed: counts.completed,
            skipped: counts.skipped,
        },
    }
}

fn api_target(target: Target) -> QueueTarget {
    match target {
        Target::Trace => QueueTarget::Trace,
        Target::Span => QueueTarget::Span,
        Target::Session => QueueTarget::Session,
    }
}

fn store_target(target: QueueTarget) -> Target {
    match target {
        QueueTarget::Trace => Target::Trace,
        QueueTarget::Span => Target::Span,
        QueueTarget::Session => Target::Session,
    }
}

/// The 400 for definition positions that are not live definitions of the project.
fn unknown_definitions(indexes: &[usize]) -> ApiError {
    ApiError::BadRequest {
        detail: None,
        errors: indexes
            .iter()
            .map(|index| {
                field(
                    format!("definition_ids[{index}]"),
                    "unknown or archived score definition",
                )
            })
            .collect(),
    }
}

/// The position a cursor names. Text that does not decode is `400` on `cursor`.
fn encode_cursor(name: &str, id: Uuid) -> Result<String, ApiError> {
    let bytes = serde_json::to_vec(&QueueCursor {
        name: name.to_owned(),
        id,
    })
    .map_err(|error| ApiError::Internal(Box::new(error)))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The position of a cursor. Text that does not decode is `400` on `cursor`.
fn decode_cursor(text: &str) -> Result<(String, Uuid), ApiError> {
    let invalid = || field_error("cursor", "invalid cursor");
    let bytes = URL_SAFE_NO_PAD.decode(text).map_err(|_| invalid())?;
    let cursor: QueueCursor = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    Ok((cursor.name, cursor.id))
}

/// Answers every collected error at once as `400 bad_request`.
fn refuse_if_any(errors: Vec<FieldError>) -> Result<(), ApiError> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ApiError::BadRequest {
            detail: None,
            errors,
        })
    }
}

fn field(path: impl Into<String>, message: impl Into<String>) -> FieldError {
    FieldError {
        field: path.into(),
        message: message.into(),
        position: None,
    }
}

/// Trimmed, 1 to 100 characters. Returns the trimmed name when it is valid.
fn checked_name(name: &str, path: &str, errors: &mut Vec<FieldError>) -> Option<String> {
    let trimmed = name.trim();
    let length = trimmed.chars().count();
    if (1..=MAX_NAME_CHARS).contains(&length) {
        Some(trimmed.to_owned())
    } else {
        errors.push(field(path, "must be 1 to 100 characters"));
        None
    }
}

/// Trimmed text of at most `max` characters. Blank text is `Some(None)`, which the callers store
/// as none. Too long is an error and `None`.
fn checked_text(
    text: &str,
    path: &str,
    max: usize,
    errors: &mut Vec<FieldError>,
) -> Option<Option<String>> {
    let trimmed = text.trim();
    if trimmed.chars().count() > max {
        errors.push(field(path, format!("must be at most {max} characters")));
        return None;
    }
    Some((!trimmed.is_empty()).then(|| trimmed.to_owned()))
}

/// 1 to 50 distinct definition ids.
fn check_definition_ids(ids: &[Uuid], errors: &mut Vec<FieldError>) {
    if !(1..=MAX_DEFINITIONS).contains(&ids.len()) {
        errors.push(field(
            "definition_ids",
            "must have 1 to 50 score definitions",
        ));
    }
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(*id) {
            errors.push(field("definition_ids", "duplicate definition"));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::api::common::ProblemCode;
    use crate::api::score_definitions::ScoreDirection;
    use crate::auth::roles::OrgRole;
    use crate::meta::score_definitions::{DefinitionFields, insert_definition};
    use crate::test_support::{Seeded, TestApp, TestResponse, set_limits};
    use cetra_core::enums::ScoreDataType;
    use cetra_core::ids::ProjectId;

    fn base(seeded: &Seeded) -> String {
        format!("/api/v1/projects/{}/annotation-queues", seeded.project.id)
    }

    async fn send(
        app: &TestApp,
        method: Method,
        path: &str,
        cookie: &str,
        body: Option<Value>,
    ) -> TestResponse {
        let request = app.request(method, path).header("cookie", cookie);
        match body {
            Some(body) => request.json(&body).send().await,
            None => request.send().await,
        }
    }

    /// Inserts a numeric score definition in `project` and returns its id.
    async fn define(app: &TestApp, project: ProjectId, name: &str, archived: bool) -> Uuid {
        let id = Uuid::now_v7();
        let now = app.state.clock.now_ns();
        let fields = DefinitionFields {
            name: name.to_owned(),
            categories: Vec::new(),
            min_value: Some(0.0),
            max_value: Some(1.0),
            description: None,
            direction: ScoreDirection::None,
            archived_at: archived.then_some(now),
        };
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_definition(conn, id, project, ScoreDataType::Numeric, fields, now).await
                })
            })
            .await
            .unwrap();
        id
    }

    async fn create(
        app: &TestApp,
        seeded: &Seeded,
        name: &str,
        definition: Uuid,
    ) -> AnnotationQueue {
        let response = send(
            app,
            Method::POST,
            &base(seeded),
            &seeded.cookie,
            Some(json!({"name": name, "target": "trace", "definition_ids": [definition]})),
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED);
        response.json()
    }

    /// Asserts a `400` whose errors name `field`.
    fn assert_refused_at(response: &TestResponse, field: &str) {
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        let body: Value = response.json();
        let errors = body["errors"].as_array().expect("field errors");
        assert!(
            errors.iter().any(|error| error["field"] == field),
            "{field} in {body}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_list_get_update_delete() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;

        let created = create(&app, &seeded, "Review", definition).await;
        assert_eq!(created.version, 1);
        assert_eq!(created.created_by, Some(seeded.user_id));
        assert_eq!(created.definition_ids, vec![definition]);
        assert_eq!(
            created.counts,
            QueueCounts {
                pending: 0,
                completed: 0,
                skipped: 0
            }
        );

        let page = send(&app, Method::GET, &base(&seeded), &seeded.cookie, None).await;
        assert_eq!(page.status, StatusCode::OK);
        let page: Page<AnnotationQueue> = page.json();
        assert_eq!(page.data, vec![created.clone()]);
        assert!(page.next_cursor.is_none());

        let path = format!("{}/{}", base(&seeded), created.id);
        let fetched = send(&app, Method::GET, &path, &seeded.cookie, None).await;
        assert_eq!(fetched.status, StatusCode::OK);
        assert_eq!(fetched.json::<AnnotationQueue>(), created);

        let updated = send(
            &app,
            Method::PATCH,
            &path,
            &seeded.cookie,
            Some(json!({"version": 1, "name": " Review two ", "description": "Second pass"})),
        )
        .await;
        assert_eq!(updated.status, StatusCode::OK);
        let updated: AnnotationQueue = updated.json();
        assert_eq!(updated.version, 2);
        assert_eq!(updated.name, "Review two");
        assert_eq!(updated.description.as_deref(), Some("Second pass"));

        let cleared = send(
            &app,
            Method::PATCH,
            &path,
            &seeded.cookie,
            Some(json!({"version": 2, "description": ""})),
        )
        .await;
        assert_eq!(cleared.status, StatusCode::OK);
        let cleared: AnnotationQueue = cleared.json();
        assert_eq!(cleared.description, None);
        assert_eq!(cleared.version, 3);

        let deleted = send(&app, Method::DELETE, &path, &seeded.cookie, None).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        let gone = send(&app, Method::GET, &path, &seeded.cookie, None).await;
        assert_eq!(gone.status, StatusCode::NOT_FOUND);
        let again = send(&app, Method::DELETE, &path, &seeded.cookie, None).await;
        assert_eq!(again.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stale_version_is_409_with_the_queue_wording() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let created = create(&app, &seeded, "Review", definition).await;
        let path = format!("{}/{}", base(&seeded), created.id);
        let bumped = send(
            &app,
            Method::PATCH,
            &path,
            &seeded.cookie,
            Some(json!({"version": 1, "name": "Second"})),
        )
        .await;
        assert_eq!(bumped.status, StatusCode::OK);

        let stale = send(
            &app,
            Method::PATCH,
            &path,
            &seeded.cookie,
            Some(json!({"version": 1, "name": "Third"})),
        )
        .await;
        assert_eq!(stale.status, StatusCode::CONFLICT);
        let body: Value = stale.json();
        assert_eq!(body["code"], "conflict");
        assert_eq!(
            body["detail"],
            "the annotation queue changed; current version is 2"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_validates_fields() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let live = define(&app, project, "relevance", false).await;
        let archived = define(&app, project, "old", true).await;
        let (other, _key) = app.add_project(&seeded, "other", None).await;
        let elsewhere = define(&app, other.id, "elsewhere", false).await;
        let unknown = Uuid::now_v7();
        let many: Vec<Uuid> = (0..51).map(|_| Uuid::now_v7()).collect();

        let cases = [
            (
                json!({"name": "   ", "target": "trace", "definition_ids": [live]}),
                "name",
            ),
            (
                json!({"name": "a".repeat(101), "target": "trace", "definition_ids": [live]}),
                "name",
            ),
            (
                json!({"name": "Review", "target": "trace", "definition_ids": []}),
                "definition_ids",
            ),
            (
                json!({"name": "Review", "target": "trace", "definition_ids": many}),
                "definition_ids",
            ),
            (
                json!({"name": "Review", "target": "trace", "definition_ids": [live, live]}),
                "definition_ids",
            ),
            (
                json!({"name": "Review", "target": "trace", "definition_ids": [archived]}),
                "definition_ids[0]",
            ),
            (
                json!({"name": "Review", "target": "trace", "definition_ids": [unknown]}),
                "definition_ids[0]",
            ),
            (
                json!({"name": "Review", "target": "trace", "definition_ids": [elsewhere]}),
                "definition_ids[0]",
            ),
            (
                json!({"name": "Review", "target": "trace", "definition_ids": [live],
                       "description": "d".repeat(1_001)}),
                "description",
            ),
            (
                json!({"name": "Review", "target": "trace", "definition_ids": [live],
                       "instructions": "i".repeat(10_001)}),
                "instructions",
            ),
        ];
        for (body, field) in cases {
            let response = send(
                &app,
                Method::POST,
                &base(&seeded),
                &seeded.cookie,
                Some(body),
            )
            .await;
            assert_refused_at(&response, field);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn duplicate_name_is_409() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        create(&app, &seeded, "Review", definition).await;
        let again = send(
            &app,
            Method::POST,
            &base(&seeded),
            &seeded.cookie,
            Some(json!({"name": "Review", "target": "trace", "definition_ids": [definition]})),
        )
        .await;
        assert_eq!(again.status, StatusCode::CONFLICT);
        let body: Value = again.json();
        assert_eq!(body["detail"], "an annotation queue with this name exists");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_takes_one_target() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let path = format!(
            "{}?target_trace_id={}&target_session_id=s1",
            base(&seeded),
            "a".repeat(32)
        );
        let response = send(&app, Method::GET, &path, &seeded.cookie, None).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        let body: Value = response.json();
        assert_eq!(body["errors"][0]["field"], "target_session_id");
        assert_eq!(body["errors"][0]["message"], "give one target");

        let bad = format!("{}?target_trace_id=xyz", base(&seeded));
        let response = send(&app, Method::GET, &bad, &seeded.cookie, None).await;
        assert_refused_at(&response, "target_trace_id");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn free_plan_allows_one_queue() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2026-01-01T00:00:00Z",
            &json!({"max_annotation_queues": 1}),
        );
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        create(&app, &seeded, "First", definition).await;

        let refused = send(
            &app,
            Method::POST,
            &base(&seeded),
            &seeded.cookie,
            Some(json!({"name": "Second", "target": "trace", "definition_ids": [definition]})),
        )
        .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        let body: Value = refused.json();
        assert_eq!(body["code"], "plan_limit");
        assert_eq!(
            ProblemCode::PlanLimit.status(),
            403,
            "the plan limit is a 403"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn queue_permissions() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let body = json!({"name": "Review", "target": "trace", "definition_ids": [definition]});
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let member = app
            .add_member(&seeded, "member@example.com", OrgRole::Member)
            .await;

        let read = send(&app, Method::GET, &base(&seeded), &viewer.cookie, None).await;
        assert_eq!(read.status, StatusCode::OK);
        let denied = send(
            &app,
            Method::POST,
            &base(&seeded),
            &viewer.cookie,
            Some(body.clone()),
        )
        .await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN);

        let read_key = app
            .request(Method::POST, &base(&seeded))
            .header("authorization", &format!("Bearer {}", seeded.read_key))
            .json(&body)
            .send()
            .await;
        assert_eq!(read_key.status, StatusCode::FORBIDDEN);

        let write_key = app.write_key(&seeded).await;
        let by_key = app
            .request(Method::POST, &base(&seeded))
            .header("authorization", &format!("Bearer {write_key}"))
            .json(&json!({"name": "By key", "target": "trace", "definition_ids": [definition]}))
            .send()
            .await;
        assert_eq!(by_key.status, StatusCode::CREATED);
        let by_key: AnnotationQueue = by_key.json();
        assert_eq!(by_key.created_by, None);

        let read_pat = app.personal_token(member.id, false).await;
        let pat_write = app
            .request(Method::POST, &base(&seeded))
            .header("authorization", &format!("Bearer {read_pat}"))
            .json(
                &json!({"name": "By read pat", "target": "trace", "definition_ids": [definition]}),
            )
            .send()
            .await;
        assert_eq!(pat_write.status, StatusCode::FORBIDDEN);

        let write_pat = app.personal_token(member.id, true).await;
        let written = app
            .request(Method::POST, &base(&seeded))
            .header("authorization", &format!("Bearer {write_pat}"))
            .json(&json!({"name": "By pat", "target": "trace", "definition_ids": [definition]}))
            .send()
            .await;
        assert_eq!(written.status, StatusCode::CREATED);

        let member_write = send(
            &app,
            Method::POST,
            &base(&seeded),
            &member.cookie,
            Some(json!({"name": "By member", "target": "trace", "definition_ids": [definition]})),
        )
        .await;
        assert_eq!(member_write.status, StatusCode::CREATED);
    }
}
