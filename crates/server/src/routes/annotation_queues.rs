//! Annotation queues (spec 09 § Annotation queues, spec 04 § Annotation queues, D-AQ-2–D-AQ-11).
//!
//! Reads need project view and keys read them. Queue writes need [`ProjectPerm::ManageQueues`]:
//! member and above through a session, a read-write personal access token or a `write` API key.
//! A read-only personal access token cannot write (the access layer answers 403).

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use cetra_core::ids::{OrgId, ProjectId, SpanId, TraceId};

use crate::analytics::actor::Actor;
use crate::analytics::catalog::Event as CatalogEvent;
use crate::analytics::record;
use crate::api::annotation_queues::{
    AddQueueItems, AddedQueueItems, AnnotationQueue, AnnotationQueueItem,
    AnnotationQueueItemListParams, AnnotationQueueListParams, CreateAnnotationQueue, NextQueueItem,
    QueueCounts, QueueItemAddedBy, QueueItemStatus, QueueItemTarget, QueueMatchingAdded,
    QueueMatchingPreview, QueueMatchingRequest, QueueTarget, SkipQueueItem, SkippedQueueItem,
    UpdateAnnotationQueue,
};
use crate::api::common::{FieldError, Page};
use crate::audit::catalog::AnnotationQueueAction;
use crate::audit::{self, Event, TargetKind};
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
use crate::routes::read::{Deadline, READ_TIMEOUT, check_search, open_query, page_limit};

/// A queue body is a name, two texts and up to 50 ids (D-AQ-7).
const QUEUE_BODY_LIMIT: usize = 64 * 1024;
/// An add body is up to 1 000 targets of a few hundred bytes each (D-AQ-8).
const ITEMS_BODY_LIMIT: usize = 1024 * 1024;
/// A reviewer body is `{}` or a short reason (D-AQ-5).
const REVIEW_BODY_LIMIT: usize = 16 * 1024;
/// A matching body is a filter, a search and two times (D-AQ-8).
const MATCHING_BODY_LIMIT: usize = 64 * 1024;
/// Most targets an add by filter adds, and most a preview counts (D-AQ-8).
pub(crate) const MAX_MATCHING: usize = 10_000;
/// Longest skip reason, in characters after trimming.
const MAX_REASON_CHARS: usize = 1_000;
/// Longest queue name, in characters after trimming.
const MAX_NAME_CHARS: usize = 100;
/// Longest queue description, in characters after trimming.
const MAX_DESCRIPTION_CHARS: usize = 1_000;
/// Longest queue instructions, in characters after trimming.
const MAX_INSTRUCTIONS_CHARS: usize = 10_000;
/// Longest session id, in bytes.
pub(crate) const MAX_SESSION_ID_BYTES: usize = 255;

/// Annotation queue routes (spec 09): the queues and their items.
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
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/annotation-queues/{id}/items",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_items),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/annotation-queues/{id}/items",
            access: Access::Project {
                perm: ProjectPerm::ManageQueues,
                session_only: false,
            },
            documented: true,
            handler: post(add_items).layer(DefaultBodyLimit::max(ITEMS_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/annotation-queues/{id}/items/matching/preview",
            access: Access::Project {
                perm: ProjectPerm::ManageQueues,
                session_only: false,
            },
            documented: true,
            handler: post(preview_matching).layer(DefaultBodyLimit::max(MATCHING_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/annotation-queues/{id}/items/matching",
            access: Access::Project {
                perm: ProjectPerm::ManageQueues,
                session_only: false,
            },
            documented: true,
            handler: post(add_matching).layer(DefaultBodyLimit::max(MATCHING_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_item),
        },
        RouteDef {
            method: Method::DELETE,
            path: "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}",
            access: Access::Project {
                perm: ProjectPerm::ManageQueues,
                session_only: false,
            },
            documented: true,
            handler: delete(remove_item),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/annotation-queues/{id}/next",
            access: Access::Project {
                perm: ProjectPerm::Review,
                session_only: false,
            },
            documented: true,
            handler: post(next_item).layer(DefaultBodyLimit::max(REVIEW_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}/lease",
            access: Access::Project {
                perm: ProjectPerm::Review,
                session_only: false,
            },
            documented: true,
            handler: post(lease_item).layer(DefaultBodyLimit::max(REVIEW_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::DELETE,
            path: "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}/lease",
            access: Access::Project {
                perm: ProjectPerm::Review,
                session_only: false,
            },
            documented: true,
            handler: delete(release_item).layer(DefaultBodyLimit::max(REVIEW_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}/complete",
            access: Access::Project {
                perm: ProjectPerm::Review,
                session_only: false,
            },
            documented: true,
            handler: post(complete_item).layer(DefaultBodyLimit::max(REVIEW_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}/skip",
            access: Access::Project {
                perm: ProjectPerm::Review,
                session_only: false,
            },
            documented: true,
            handler: post(skip_item).layer(DefaultBodyLimit::max(REVIEW_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/annotation-queues/{id}/items/{item_id}/reopen",
            access: Access::Project {
                perm: ProjectPerm::Review,
                session_only: false,
            },
            documented: true,
            handler: post(reopen_item).layer(DefaultBodyLimit::max(REVIEW_BODY_LIMIT)),
        },
    ]
}

#[derive(Debug, Deserialize)]
struct QueuePath {
    id: Uuid,
}

#[derive(Debug, Deserialize)]
struct ItemPath {
    id: Uuid,
    item_id: Uuid,
}

/// The position an item list cursor names: the id of the previous page's last item.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ItemCursor {
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
    ctx: audit::Ctx,
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
                let outcome = store::insert_queue_tx(
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
                .await?;
                if let QueueCreate::Created(row) = &outcome {
                    let event = Event::new(AnnotationQueueAction::Created)
                        .org(org)
                        .project(project)
                        .target(TargetKind::AnnotationQueue, row.id);
                    audit::record(conn, &ctx, event, now).await?;
                }
                Ok(outcome)
            })
        })
        .await?;
    match outcome {
        QueueCreate::Created(row) => {
            let json = queue_json(&row, None);
            record(
                &state,
                &CatalogEvent::AnnotationQueueCreated {
                    target: body.target,
                },
                &Actor::project(&principal, &access),
            );
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
    ctx: audit::Ctx,
    error::Path(path): error::Path<QueuePath>,
    error::Json(body): error::Json<UpdateAnnotationQueue>,
) -> Result<error::Json<AnnotationQueue>, ApiError> {
    let project = access.project.id;
    let org = access.project.org_id;
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
            Box::pin(async move {
                let outcome = store::update_tx(conn, project, id, version, changes, now).await?;
                if let QueueUpdate::Updated(_) = &outcome {
                    let event = Event::new(AnnotationQueueAction::Updated)
                        .org(org)
                        .project(project)
                        .target(TargetKind::AnnotationQueue, id);
                    audit::record(conn, &ctx, event, now).await?;
                }
                Ok(outcome)
            })
        })
        .await?;
    match outcome {
        QueueUpdate::Updated(row) => {
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
    ctx: audit::Ctx,
    error::Path(path): error::Path<QueuePath>,
) -> Result<StatusCode, ApiError> {
    let project = access.project.id;
    let org = access.project.org_id;
    let id = path.id;
    let now = state.clock.now_ns();
    let deleted = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let deleted = store::delete(conn, project, id).await?;
                if deleted {
                    let event = Event::new(AnnotationQueueAction::Deleted)
                        .org(org)
                        .project(project)
                        .target(TargetKind::AnnotationQueue, id);
                    audit::record(conn, &ctx, event, now).await?;
                }
                Ok(deleted)
            })
        })
        .await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

/// `GET /annotation-queues/{id}/items`: the queue's visible items by id, optionally of one
/// status. An absent queue, or one of another project, is 404.
async fn list_items(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<QueuePath>,
    error::Query(params): error::Query<AnnotationQueueItemListParams>,
) -> Result<error::Json<Page<AnnotationQueueItem>>, ApiError> {
    ensure_queue(&state, access.project.id, path.id).await?;
    let after = params
        .cursor
        .as_deref()
        .map(decode_item_cursor)
        .transpose()?;
    let visible_from = Visibility::window(&state, access.project.org_id).visible_from();
    let (rows, next) = store::list_items(
        state.meta.reader(),
        path.id,
        params.status.map(store_status),
        after,
        page_limit(params.limit),
        visible_from,
    )
    .await?;
    let now = state.clock.now_ns();
    let next_cursor = next.map(encode_item_cursor).transpose()?;
    Ok(error::Json(Page {
        data: rows.iter().map(|row| item_json(row, now)).collect(),
        next_cursor,
    }))
}

/// `GET /annotation-queues/{id}/items/{item_id}`: one item of the queue. An absent item, an item
/// of another queue and an item the data window hides are all 404.
async fn get_item(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<ItemPath>,
) -> Result<error::Json<AnnotationQueueItem>, ApiError> {
    ensure_queue(&state, access.project.id, path.id).await?;
    let visible_from = Visibility::window(&state, access.project.org_id).visible_from();
    let row = store::get_item(state.meta.reader(), path.id, path.item_id, visible_from)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(error::Json(item_json(&row, state.clock.now_ns())))
}

/// `POST /annotation-queues/{id}/items`: adds 1 to 1 000 targets by id and answers 201. A target
/// that is unknown, hidden, an evaluation trace, of the wrong kind for the queue, or blocked by a
/// deletion is listed in `skipped` and not added. Adding a target twice is `existing`, not an
/// error. Past 100 000 items the queue refuses with 422 and adds nothing (D-AQ-8).
async fn add_items(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<QueuePath>,
    error::Json(body): error::Json<AddQueueItems>,
) -> Result<(StatusCode, error::Json<AddedQueueItems>), ApiError> {
    let project = access.project.id;
    let queue_id = path.id;
    let target = store::get(state.meta.reader(), project, queue_id)
        .await?
        .ok_or(ApiError::NotFound)?
        .target;
    let added_by = added_by_of(&principal)?;
    if !(1..=cetra_storage::MAX_TARGET_IDS).contains(&body.items.len()) {
        return Err(ApiError::BadRequest {
            detail: None,
            errors: vec![field(
                "items",
                format!("must hold 1 to {} items", cetra_storage::MAX_TARGET_IDS),
            )],
        });
    }

    // Format first: a malformed id is a 400 whatever the queue's kind.
    let mut errors = Vec::new();
    let parsed: Vec<Option<Parsed>> = body
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| parse_target(index, item, &mut errors))
        .collect();
    refuse_if_any(errors)?;
    let parsed: Vec<Parsed> = parsed.into_iter().flatten().collect();

    let mut skipped = Vec::new();
    let mut fitting = Vec::new();
    for item in parsed {
        match shape_error(target, &item) {
            Some(message) => skipped.push(skip(item.index, &message)),
            None => fitting.push(item),
        }
    }

    let answer = add_found_targets(
        &state,
        access.project.org_id,
        project,
        queue_id,
        target,
        added_by,
        fitting,
        skipped,
    )
    .await?;
    Ok((StatusCode::CREATED, error::Json(answer)))
}

/// The core of both adds by id (D-AQ-8): looks `fitting` up in storage over the data window
/// (a trace, a trace and span, or a session, by the queue's `target`), lists what is missing in
/// `skipped`, then guards and inserts the rest in the writer's transaction (`insert_found`: the
/// queue is checked again, the deletion guard is asked per target, a queue past 100 000 items is
/// 422 and nothing is added). `fitting` holds only targets that fit `target`; `skipped` holds the
/// skips the caller already made. Returns the answer with `skipped` sorted by `index`, and logs
/// ids and counts.
#[allow(clippy::too_many_arguments)] // the add's inputs are all distinct
pub(crate) async fn add_found_targets(
    state: &AppState,
    org: OrgId,
    project: ProjectId,
    queue_id: Uuid,
    target: Target,
    added_by: (store::AddedByKind, Uuid),
    fitting: Vec<Parsed>,
    mut skipped: Vec<SkippedQueueItem>,
) -> Result<AddedQueueItems, ApiError> {
    let ctx = open_query(state, org, project, None).await?;
    let deadline = Deadline::start();
    let mut candidates = Vec::new();
    match target {
        Target::Trace => {
            let ids: Vec<TraceId> = fitting
                .iter()
                .filter_map(|item| item.trace)
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            let found: HashMap<TraceId, i64> =
                cetra_storage::trace_targets(&ctx, &ids, deadline.remaining()?)
                    .await?
                    .into_iter()
                    .map(|found| (found.trace_id, found.start_time))
                    .collect();
            for item in &fitting {
                // The shape check gave every trace item a trace id.
                let Some(trace) = item.trace else { continue };
                match found.get(&trace) {
                    Some(start) => candidates.push(Candidate {
                        index: item.index,
                        item: store::NewItem {
                            trace_id: Some(trace.to_hex()),
                            span_id: None,
                            session_id: None,
                            target_time: *start,
                        },
                        guard: vec![trace],
                    }),
                    None => skipped.push(skip(item.index, "the trace was not found")),
                }
            }
        }
        Target::Span => {
            let pairs: Vec<(TraceId, SpanId)> = fitting
                .iter()
                .filter_map(|item| Some((item.trace?, item.span?)))
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            let found: HashMap<(TraceId, SpanId), i64> =
                cetra_storage::span_targets(&ctx, &pairs, deadline.remaining()?)
                    .await?
                    .into_iter()
                    .map(|found| ((found.trace_id, found.span_id), found.trace_start))
                    .collect();
            for item in &fitting {
                let (Some(trace), Some(span)) = (item.trace, item.span) else {
                    continue;
                };
                match found.get(&(trace, span)) {
                    Some(start) => candidates.push(Candidate {
                        index: item.index,
                        item: store::NewItem {
                            trace_id: Some(trace.to_hex()),
                            span_id: Some(span.to_hex()),
                            session_id: None,
                            target_time: *start,
                        },
                        guard: vec![trace],
                    }),
                    None => skipped.push(skip(item.index, "the span was not found")),
                }
            }
        }
        Target::Session => {
            let ids: Vec<String> = fitting
                .iter()
                .filter_map(|item| item.session.clone())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
            let found: HashMap<String, cetra_storage::SessionTarget> =
                cetra_storage::session_targets(&ctx, &refs, deadline.remaining()?)
                    .await?
                    .into_iter()
                    .map(|found| (found.session_id.clone(), found))
                    .collect();
            for item in &fitting {
                let Some(session) = &item.session else {
                    continue;
                };
                match found.get(session) {
                    Some(found) => candidates.push(Candidate {
                        index: item.index,
                        item: store::NewItem {
                            trace_id: None,
                            span_id: None,
                            session_id: Some(session.clone()),
                            target_time: found.start_time,
                        },
                        guard: found.trace_ids.clone(),
                    }),
                    None => skipped.push(skip(item.index, "the session was not found")),
                }
            }
        }
    }

    let indexes: Vec<usize> = candidates.iter().map(|candidate| candidate.index).collect();
    let found: Vec<(store::NewItem, Vec<TraceId>)> = candidates
        .into_iter()
        .map(|candidate| (candidate.item, candidate.guard))
        .collect();
    let inserted = insert_found(state, project, queue_id, added_by, found).await?;
    skipped.extend(inserted.blocked.into_iter().filter_map(|position| {
        indexes
            .get(position)
            .map(|&index| skip(index, "a deletion blocks this target"))
    }));
    skipped.sort_by_key(|item| item.index);
    tracing::info!(
        project_id = %project,
        queue_id = %queue_id,
        added = inserted.added,
        existing = inserted.existing,
        skipped = skipped.len(),
        "annotation queue items added"
    );
    Ok(AddedQueueItems {
        added: inserted.added,
        existing: inserted.existing,
        skipped,
    })
}

/// What [`insert_found`] did: the counts, and the positions in `found` the deletion guard blocked.
struct InsertFound {
    added: u64,
    existing: u64,
    blocked: Vec<usize>,
}

/// Guards and inserts `found`, each row with the traces the deletion guard asks about, in the
/// writer's transaction. The queue is checked again there (404 when it is gone), and the guard is
/// asked for each target (D-M-6), so a deletion that lands meanwhile still blocks the add. A queue
/// past the item cap is 422 and nothing is added (D-AQ-8).
async fn insert_found(
    state: &AppState,
    project: ProjectId,
    queue_id: Uuid,
    added_by: (store::AddedByKind, Uuid),
    found: Vec<(store::NewItem, Vec<TraceId>)>,
) -> Result<InsertFound, ApiError> {
    let now = state.clock.now_ns();
    let deletions = state.clone();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                if store::get_on(conn, project, queue_id).await?.is_none() {
                    return Ok(AddWrite::Gone);
                }
                let mut blocked = Vec::new();
                let mut items = Vec::new();
                for (position, (item, guard)) in found.into_iter().enumerate() {
                    let guarded = guard.iter().any(|trace| {
                        deletions
                            .deletions
                            .guard
                            .blocking(project, *trace, &[], now)
                            .is_some()
                    });
                    if guarded {
                        blocked.push(position);
                    } else {
                        items.push(item);
                    }
                }
                let inserted =
                    store::insert_items_tx(conn, queue_id, project, &items, added_by, now).await?;
                Ok(AddWrite::Written { inserted, blocked })
            })
        })
        .await?;
    let (inserted, blocked) = match outcome {
        AddWrite::Gone => return Err(ApiError::NotFound),
        AddWrite::Written { inserted, blocked } => (inserted, blocked),
    };
    let (added, existing) = items_answer(inserted)?;
    Ok(InsertFound {
        added,
        existing,
        blocked,
    })
}

/// The targets a queue's list filter selects over a window: at most `cap` of them, with the
/// target time and the traces the deletion guard asks about, and whether more matched.
struct Matching {
    found: Vec<(store::NewItem, Vec<TraceId>)>,
    capped: bool,
}

/// `POST /annotation-queues/{id}/items/matching/preview`: counts what the add by filter would add.
/// Answers 200.
async fn preview_matching(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<QueuePath>,
    error::Json(body): error::Json<QueueMatchingRequest>,
) -> Result<error::Json<QueueMatchingPreview>, ApiError> {
    let matching = matching_targets(
        &state,
        access.project.org_id,
        access.project.id,
        path.id,
        &body,
        MAX_MATCHING,
    )
    .await?;
    Ok(error::Json(QueueMatchingPreview {
        matched: u64::try_from(matching.found.len()).unwrap_or(u64::MAX),
        capped: matching.capped,
    }))
}

/// `POST /annotation-queues/{id}/items/matching`: adds what the queue kind's list filter matches
/// over the window, at most 10 000 (D-AQ-8). Evaluation traces are never added. Answers 201.
async fn add_matching(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<QueuePath>,
    error::Json(body): error::Json<QueueMatchingRequest>,
) -> Result<(StatusCode, error::Json<QueueMatchingAdded>), ApiError> {
    let project = access.project.id;
    let queue_id = path.id;
    let matching = matching_targets(
        &state,
        access.project.org_id,
        project,
        queue_id,
        &body,
        MAX_MATCHING,
    )
    .await?;
    let added_by = added_by_of(&principal)?;
    let capped = matching.capped;
    let inserted = insert_found(&state, project, queue_id, added_by, matching.found).await?;
    let blocked = u64::try_from(inserted.blocked.len()).unwrap_or(u64::MAX);
    tracing::info!(
        project_id = %project,
        queue_id = %queue_id,
        added = inserted.added,
        existing = inserted.existing,
        blocked,
        capped,
        "annotation queue items added by filter"
    );
    Ok((
        StatusCode::CREATED,
        error::Json(QueueMatchingAdded {
            added: inserted.added,
            existing: inserted.existing,
            blocked,
            capped,
        }),
    ))
}

/// The matching targets of queue `queue_id` for `body`, at most `cap` of them. Shared by the
/// preview and the add. Order: 404 when the queue is absent, a session queue refuses a search,
/// the search length is checked, then the scan runs under the read timeout over the data window.
async fn matching_targets(
    state: &AppState,
    org: OrgId,
    project: ProjectId,
    queue_id: Uuid,
    body: &QueueMatchingRequest,
    cap: usize,
) -> Result<Matching, ApiError> {
    let target = store::get(state.meta.reader(), project, queue_id)
        .await?
        .ok_or(ApiError::NotFound)?
        .target;
    let filter = non_blank(body.filter.as_deref());
    let search = non_blank(body.search.as_deref());
    if search.is_some() && target == Target::Session {
        return Err(field_error("search", "sessions have no search"));
    }
    check_search(search)?;
    let window = cetra_storage::TimeWindow::new(body.from.ns(), body.to.ns())?;
    let ctx = open_query(state, org, project, None).await?;
    let include_experiments = body.include_experiments;
    let matching = match target {
        Target::Trace => {
            let deadline = Deadline::start();
            let scanned = cetra_storage::filter_trace_ids(
                &ctx,
                cetra_storage::FilterDeletionRequest {
                    filter,
                    search,
                    window,
                    include_eval: false,
                    include_experiments,
                    limit: cap.saturating_add(1),
                    timeout: deadline.remaining()?,
                },
            )
            .await?;
            let capped = scanned.len() > cap;
            let ids: Vec<TraceId> = scanned
                .into_iter()
                .take(cap)
                .map(|found| found.trace_id)
                .collect();
            // D-AQ-8: a filter that names `environment` lifts the scan's evaluation-trace
            // exclusion, so the traces are looked up again: `trace_targets` leaves them out.
            let mut found: Vec<(store::NewItem, Vec<TraceId>)> = Vec::new();
            for chunk in ids.chunks(cetra_storage::MAX_TARGET_IDS) {
                let traces =
                    cetra_storage::trace_targets(&ctx, chunk, deadline.remaining()?).await?;
                found.extend(traces.into_iter().map(|trace| {
                    (
                        store::NewItem {
                            trace_id: Some(trace.trace_id.to_hex()),
                            span_id: None,
                            session_id: None,
                            target_time: trace.start_time,
                        },
                        vec![trace.trace_id],
                    )
                }));
            }
            Matching { found, capped }
        }
        Target::Span => {
            let spans = cetra_storage::filter_span_targets(
                &ctx,
                cetra_storage::SpanTargetFilter {
                    filter,
                    search,
                    window,
                    include_experiments,
                    limit: cap,
                    timeout: READ_TIMEOUT,
                },
            )
            .await?;
            let capped = spans.capped;
            let found = spans
                .targets
                .into_iter()
                .map(|found| {
                    (
                        store::NewItem {
                            trace_id: Some(found.trace_id.to_hex()),
                            span_id: Some(found.span_id.to_hex()),
                            session_id: None,
                            target_time: found.trace_start,
                        },
                        vec![found.trace_id],
                    )
                })
                .collect();
            Matching { found, capped }
        }
        Target::Session => {
            let sessions = cetra_storage::filter_session_targets(
                &ctx,
                cetra_storage::SessionTargetFilter {
                    filter,
                    window,
                    include_experiments,
                    limit: cap,
                    timeout: READ_TIMEOUT,
                },
            )
            .await?;
            let capped = sessions.capped;
            let found = sessions
                .targets
                .into_iter()
                .map(|found| {
                    (
                        store::NewItem {
                            trace_id: None,
                            span_id: None,
                            session_id: Some(found.session_id),
                            target_time: found.start_time,
                        },
                        found.trace_ids,
                    )
                })
                .collect();
            Matching { found, capped }
        }
    };
    Ok(matching)
}

/// Blank filter or search text means "no filter" or "no search".
fn non_blank(text: Option<&str>) -> Option<&str> {
    text.map(str::trim).filter(|text| !text.is_empty())
}

/// `DELETE /annotation-queues/{id}/items/{item_id}`: removes one item. Answers 204, or 404.
async fn remove_item(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<ItemPath>,
) -> Result<StatusCode, ApiError> {
    let project = access.project.id;
    let queue_id = path.id;
    let item_id = path.item_id;
    let deleted = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                if store::get_on(conn, project, queue_id).await?.is_none() {
                    return Ok(false);
                }
                store::delete_item(conn, queue_id, item_id).await
            })
        })
        .await?;
    if deleted {
        tracing::info!(
            project_id = %project,
            queue_id = %queue_id,
            item_id = %item_id,
            "annotation queue item removed"
        );
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

/// Fails with 404 unless `id` is a queue of the project.
async fn ensure_queue(state: &AppState, project: ProjectId, id: Uuid) -> Result<(), ApiError> {
    let found = store::existing_ids(state.meta.reader(), project, &[id]).await?;
    if found.contains(&id) {
        Ok(())
    } else {
        Err(ApiError::NotFound)
    }
}

/// Who adds an item (D-AQ-11): the session user, the personal token or the API key. The
/// control plane never reaches a project route.
pub(crate) fn added_by_of(principal: &Principal) -> Result<(store::AddedByKind, Uuid), ApiError> {
    match principal {
        Principal::Session { user, .. } => Ok((store::AddedByKind::User, user.id)),
        Principal::Pat { token_id, .. } => Ok((store::AddedByKind::Pat, *token_id)),
        Principal::ApiKey { key_id, .. } => Ok((store::AddedByKind::Key, *key_id)),
        Principal::ControlPlane { .. } | Principal::Scim { .. } => Err(ApiError::NotFound),
    }
}

/// One target of an add after the format checks. `index` is its position in the request
/// (for the issue route: in the occurrences read, D-IC-29).
pub(crate) struct Parsed {
    pub(crate) index: usize,
    pub(crate) trace: Option<TraceId>,
    pub(crate) span: Option<SpanId>,
    pub(crate) session: Option<String>,
}

/// A target that may be added: its row, and the traces the deletion guard asks about.
struct Candidate {
    index: usize,
    item: store::NewItem,
    guard: Vec<TraceId>,
}

/// What the writer's transaction of an add did.
enum AddWrite {
    /// The queue is gone.
    Gone,
    /// The queue's rows, and the request positions the deletion guard blocked.
    Written {
        inserted: store::ItemsInsert,
        blocked: Vec<usize>,
    },
}

/// Checks one target's ids. A malformed id is recorded in `errors` and gives `None`.
fn parse_target(
    index: usize,
    item: &QueueItemTarget,
    errors: &mut Vec<FieldError>,
) -> Option<Parsed> {
    let trace = hex_field::<TraceId>(
        item.trace_id.as_deref(),
        &format!("items[{index}].trace_id"),
        "invalid trace id",
        errors,
    );
    let span = hex_field::<SpanId>(
        item.span_id.as_deref(),
        &format!("items[{index}].span_id"),
        "invalid span id",
        errors,
    );
    let session = match item.session_id.as_deref() {
        None => Some(None),
        Some(text) if text.is_empty() || text.len() > MAX_SESSION_ID_BYTES => {
            errors.push(field(
                format!("items[{index}].session_id"),
                "must be 1 to 255 bytes",
            ));
            None
        }
        Some(text) => Some(Some(text.to_owned())),
    };
    Some(Parsed {
        index,
        trace: trace?,
        span: span?,
        session: session?,
    })
}

/// An optional id: `Some(None)` when absent, `Some(Some(id))` when it parses, and `None` with
/// an error at `path` when it does not.
fn hex_field<T: FromStr>(
    text: Option<&str>,
    path: &str,
    message: &str,
    errors: &mut Vec<FieldError>,
) -> Option<Option<T>> {
    match text {
        None => Some(None),
        Some(text) => match text.parse::<T>() {
            Ok(id) => Some(Some(id)),
            Err(_) => {
                errors.push(field(path, message));
                None
            }
        },
    }
}

/// The reason a target does not fit the queue's kind, as the skip message names it: `None` when
/// it fits. A trace queue takes a trace id; a span queue a trace and a span id; a session queue
/// a session id. Nothing else.
fn shape_error(target: Target, item: &Parsed) -> Option<String> {
    let (takes, fits) = match target {
        Target::Trace => (
            "trace_id",
            item.trace.is_some() && item.span.is_none() && item.session.is_none(),
        ),
        Target::Span => (
            "trace_id and span_id",
            item.trace.is_some() && item.span.is_some() && item.session.is_none(),
        ),
        Target::Session => (
            "session_id",
            item.session.is_some() && item.trace.is_none() && item.span.is_none(),
        ),
    };
    (!fits).then(|| format!("a {} queue takes {takes}", target.as_str()))
}

/// The answer of an add's insert: `(added, existing)`. A queue past the item cap is 422 and
/// nothing was added (D-AQ-8).
fn items_answer(inserted: store::ItemsInsert) -> Result<(u64, u64), ApiError> {
    match inserted {
        store::ItemsInsert::Done { added, existing } => Ok((added, existing)),
        store::ItemsInsert::TooMany => Err(ApiError::TooManyItems),
    }
}

pub(crate) fn skip(index: usize, message: &str) -> SkippedQueueItem {
    SkippedQueueItem {
        index: u64::try_from(index).unwrap_or(u64::MAX),
        message: message.to_owned(),
    }
}

fn store_status(status: QueueItemStatus) -> store::Status {
    match status {
        QueueItemStatus::Pending => store::Status::Pending,
        QueueItemStatus::Completed => store::Status::Completed,
        QueueItemStatus::Skipped => store::Status::Skipped,
    }
}

fn api_status(status: store::Status) -> QueueItemStatus {
    match status {
        store::Status::Pending => QueueItemStatus::Pending,
        store::Status::Completed => QueueItemStatus::Completed,
        store::Status::Skipped => QueueItemStatus::Skipped,
    }
}

fn api_added_by(kind: store::AddedByKind) -> QueueItemAddedBy {
    match kind {
        store::AddedByKind::User => QueueItemAddedBy::User,
        store::AddedByKind::Pat => QueueItemAddedBy::Pat,
        store::AddedByKind::Key => QueueItemAddedBy::Key,
    }
}

/// The API form of a stored item. The lease fields show only while the lease is live (D-AQ-4).
fn item_json(row: &store::ItemRow, now: i64) -> AnnotationQueueItem {
    let live = row.lease_expires_at.filter(|expires| *expires > now);
    AnnotationQueueItem {
        id: row.id,
        queue_id: row.queue_id,
        trace_id: row.trace_id.clone(),
        span_id: row.span_id.clone(),
        session_id: row.session_id.clone(),
        target_time: row.target_time,
        status: api_status(row.status),
        skip_reason: row.skip_reason.clone(),
        leased_by_user_id: row.leased_by_user_id.filter(|_| live.is_some()),
        lease_expires_at: live,
        done_by_user_id: row.done_by_user_id,
        done_at: row.done_at,
        added_by: api_added_by(row.added_by_kind),
        added_by_id: row.added_by_id,
        created_at: row.created_at,
    }
}

/// The position an item cursor names. Text that does not decode is `400` on `cursor`.
fn encode_item_cursor(id: Uuid) -> Result<String, ApiError> {
    let bytes = serde_json::to_vec(&ItemCursor { id })
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn decode_item_cursor(text: &str) -> Result<Uuid, ApiError> {
    let invalid = || field_error("cursor", "invalid cursor");
    let bytes = URL_SAFE_NO_PAD.decode(text).map_err(|_| invalid())?;
    let cursor: ItemCursor = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    Ok(cursor.id)
}

/// `POST /annotation-queues/{id}/next`: leases the first free pending item after `after`, wrapping
/// to the first when none follows, and releases the caller's other leases in the queue. Answers
/// 200 with the item, or 204 with no body when nothing is free (D-AQ-4).
async fn next_item(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<QueuePath>,
    error::Json(body): error::Json<NextQueueItem>,
) -> Result<Response, ApiError> {
    let user = reviewer(&principal)?;
    let project = access.project.id;
    let queue_id = path.id;
    ensure_queue(&state, project, queue_id).await?;
    let visible_from = Visibility::window(&state, access.project.org_id).visible_from();
    let now = state.clock.now_ns();
    let after = body.after;
    let claimed = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                store::claim_next_tx(conn, queue_id, user, after, visible_from, now).await
            })
        })
        .await?;
    match claimed {
        Some(row) => Ok((StatusCode::OK, error::Json(item_json(&row, now))).into_response()),
        None => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

/// `POST /annotation-queues/{id}/items/{item_id}/lease`: takes or renews the item's lease for the
/// caller. Another reviewer's live lease is 409 with the holder.
async fn lease_item(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<ItemPath>,
) -> Result<error::Json<AnnotationQueueItem>, ApiError> {
    let user = reviewer(&principal)?;
    let project = access.project.id;
    let (queue_id, item_id) = (path.id, path.item_id);
    ensure_queue(&state, project, queue_id).await?;
    let visible_from = Visibility::window(&state, access.project.org_id).visible_from();
    let now = state.clock.now_ns();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                store::lease_tx(conn, queue_id, item_id, user, visible_from, now).await
            })
        })
        .await?;
    review_answer(outcome, now).map(error::Json)
}

/// `DELETE /annotation-queues/{id}/items/{item_id}/lease`: releases the caller's lease. Answers
/// 204 even when the caller holds none (D-AQ-4).
async fn release_item(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<ItemPath>,
) -> Result<StatusCode, ApiError> {
    let user = reviewer(&principal)?;
    let project = access.project.id;
    let (queue_id, item_id) = (path.id, path.item_id);
    ensure_queue(&state, project, queue_id).await?;
    state
        .meta
        .write(move |conn| {
            Box::pin(async move { store::release_tx(conn, queue_id, item_id, user).await })
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /annotation-queues/{id}/items/{item_id}/complete`: completes a pending item (D-AQ-5).
async fn complete_item(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<ItemPath>,
) -> Result<error::Json<AnnotationQueueItem>, ApiError> {
    let item = finish(&state, &principal, &access, path, store::Finish::Complete).await?;
    record(
        &state,
        &CatalogEvent::AnnotationQueueItemCompleted,
        &Actor::project(&principal, &access),
    );
    Ok(item)
}

/// `POST /annotation-queues/{id}/items/{item_id}/skip`: skips a pending item, with an optional
/// reason (D-AQ-5).
async fn skip_item(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<ItemPath>,
    error::Json(body): error::Json<SkipQueueItem>,
) -> Result<error::Json<AnnotationQueueItem>, ApiError> {
    let mut errors = Vec::new();
    let reason = body
        .reason
        .as_deref()
        .and_then(|text| checked_text(text, "reason", MAX_REASON_CHARS, &mut errors))
        .flatten();
    refuse_if_any(errors)?;
    finish(
        &state,
        &principal,
        &access,
        path,
        store::Finish::Skip(reason),
    )
    .await
}

/// `POST /annotation-queues/{id}/items/{item_id}/reopen`: sets a completed or skipped item back to
/// pending (D-AQ-5).
async fn reopen_item(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<ItemPath>,
) -> Result<error::Json<AnnotationQueueItem>, ApiError> {
    reviewer(&principal)?;
    let project = access.project.id;
    let (queue_id, item_id) = (path.id, path.item_id);
    ensure_queue(&state, project, queue_id).await?;
    let visible_from = Visibility::window(&state, access.project.org_id).visible_from();
    let now = state.clock.now_ns();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(
                async move { store::reopen_tx(conn, queue_id, item_id, visible_from, now).await },
            )
        })
        .await?;
    let answer = review_answer(outcome, now)?;
    tracing::info!(project_id = %project, queue_id = %queue_id, item_id = %item_id, "annotation queue item reopened");
    Ok(error::Json(answer))
}

/// Completes or skips a pending item as the caller. Shared by complete and skip.
async fn finish(
    state: &AppState,
    principal: &Principal,
    access: &ProjectAccess,
    path: ItemPath,
    finish: store::Finish,
) -> Result<error::Json<AnnotationQueueItem>, ApiError> {
    let user = reviewer(principal)?;
    let project = access.project.id;
    let (queue_id, item_id) = (path.id, path.item_id);
    ensure_queue(state, project, queue_id).await?;
    let visible_from = Visibility::window(state, access.project.org_id).visible_from();
    let now = state.clock.now_ns();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                store::finish_tx(conn, queue_id, item_id, user, finish, visible_from, now).await
            })
        })
        .await?;
    let answer = review_answer(outcome, now)?;
    let event = match answer.status {
        QueueItemStatus::Skipped => "skipped",
        _ => "completed",
    };
    tracing::info!(
        project_id = %project,
        queue_id = %queue_id,
        item_id = %item_id,
        "annotation queue item {}",
        event
    );
    Ok(error::Json(answer))
}

/// The reviewing user (D-AQ-11). Only sessions and personal tokens reach a review route.
fn reviewer(principal: &Principal) -> Result<Uuid, ApiError> {
    Ok(principal::session_user(principal)?.id)
}

/// The answer of a review: the item, or the refusal the store named (D-AQ-4, D-AQ-5).
fn review_answer(outcome: store::ReviewOutcome, now: i64) -> Result<AnnotationQueueItem, ApiError> {
    match outcome {
        store::ReviewOutcome::Done(row) => Ok(item_json(&row, now)),
        store::ReviewOutcome::Missing => Err(ApiError::NotFound),
        store::ReviewOutcome::NotPending => Err(ApiError::Conflict {
            detail: "the item is not pending",
        }),
        store::ReviewOutcome::Pending => Err(ApiError::Conflict {
            detail: "the item is pending",
        }),
        store::ReviewOutcome::HeldBy(holder) => Err(ApiError::ConflictWith {
            detail: "another reviewer holds this item",
            errors: vec![field("leased_by_user_id", holder.to_string())],
        }),
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
    use std::time::Duration;

    use super::*;
    use crate::api::common::ProblemCode;
    use crate::api::score_definitions::ScoreDirection;
    use crate::auth::roles::OrgRole;
    use crate::meta::score_definitions::{DefinitionFields, insert_definition};
    use crate::test_support::{Seeded, TestApp, TestResponse, dataset_span, set_limits};
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
        assert!(
            app.state.analytics.counters.get("annotation_queue.created") >= 1,
            "annotation_queue.created not counted"
        );
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

    /// A trace id with `n` in its low digits.
    fn trace_id(n: u128) -> TraceId {
        format!("{:032x}", n + 0x1000)
            .parse::<TraceId>()
            .expect("trace id hex")
    }

    /// Creates a queue of `target` (`trace`, `span` or `session`) with `definition`.
    async fn create_of(
        app: &TestApp,
        seeded: &Seeded,
        name: &str,
        target: &str,
        definition: Uuid,
    ) -> AnnotationQueue {
        let response = send(
            app,
            Method::POST,
            &base(seeded),
            &seeded.cookie,
            Some(json!({"name": name, "target": target, "definition_ids": [definition]})),
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED);
        response.json()
    }

    /// Commits one span `n` of `trace`, starting at `start`, in `session` and in the
    /// evaluation environment when `eval`.
    async fn commit_span(
        app: &TestApp,
        seeded: &Seeded,
        trace: TraceId,
        n: u64,
        start: i64,
        session: Option<&str>,
        eval: bool,
    ) {
        let mut span = dataset_span(0);
        span.trace_id = trace;
        span.span_id = format!("d4{n:014x}")
            .parse::<SpanId>()
            .expect("span id hex");
        span.parent_span_id = None;
        span.start_time = start;
        span.end_time = start + 1_000_000_000;
        span.ingest_time = app.state.clock.now_ns();
        span.session_id = session.map(str::to_owned);
        if eval {
            span.environment = Some("cetra-eval".to_owned());
        }
        app.commit_spans(&seeded.project, vec![span]).await;
    }

    /// Adds `body` to the queue `queue` as the owner.
    #[tokio::test(flavor = "multi_thread")]
    async fn queue_create_is_recorded_items_are_not() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let queue = create(&app, &seeded, "Review", definition).await;
        let now = app.state.clock.now_ns();
        app.commit_trace_at(&seeded.project, trace_id(1), 0, now)
            .await;
        let added = add(
            &app,
            &seeded,
            queue.id,
            json!({"items": [{"trace_id": trace_id(1).to_hex()}]}),
        )
        .await;
        assert_eq!(added.status, StatusCode::CREATED);

        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT action, target_id FROM audit_events WHERE action LIKE 'annotation_queue.%' \
             ORDER BY seq",
        )
        .fetch_all(app.state.meta.reader())
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![(
                "annotation_queue.created".to_owned(),
                Some(queue.id.to_string())
            )]
        );
    }

    async fn add(app: &TestApp, seeded: &Seeded, queue: Uuid, body: Value) -> TestResponse {
        let path = format!("{}/{queue}/items", base(seeded));
        send(app, Method::POST, &path, &seeded.cookie, Some(body)).await
    }

    /// The items of the queue `queue`, as the owner lists them.
    async fn items_of(app: &TestApp, seeded: &Seeded, queue: Uuid) -> Vec<AnnotationQueueItem> {
        let path = format!("{}/{queue}/items", base(seeded));
        let response = send(app, Method::GET, &path, &seeded.cookie, None).await;
        assert_eq!(response.status, StatusCode::OK);
        let page: Page<AnnotationQueueItem> = response.json();
        page.data
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_items_by_id() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let queue = create(&app, &seeded, "Review", definition).await;
        let now = app.state.clock.now_ns();
        app.commit_trace_at(&seeded.project, trace_id(1), 0, now)
            .await;
        app.commit_trace_at(&seeded.project, trace_id(2), 0, now)
            .await;
        let body = json!({"items": [
            {"trace_id": trace_id(1).to_hex()},
            {"trace_id": trace_id(2).to_hex()},
            {"trace_id": trace_id(9).to_hex()},
        ]});

        let first = add(&app, &seeded, queue.id, body.clone()).await;
        assert_eq!(first.status, StatusCode::CREATED);
        let answer: AddedQueueItems = first.json();
        assert_eq!(answer.added, 2);
        assert_eq!(answer.existing, 0);
        assert_eq!(
            answer.skipped,
            vec![SkippedQueueItem {
                index: 2,
                message: "the trace was not found".to_owned(),
            }]
        );

        let second = add(&app, &seeded, queue.id, body).await;
        assert_eq!(second.status, StatusCode::CREATED);
        let again: AddedQueueItems = second.json();
        assert_eq!(again.added, 0);
        assert_eq!(again.existing, 2);
        assert_eq!(again.skipped.len(), 1);

        let items = items_of(&app, &seeded, queue.id).await;
        assert_eq!(items.len(), 2);
        assert!(
            items
                .iter()
                .all(|item| item.status == QueueItemStatus::Pending)
        );
        assert!(items.iter().all(|item| item.target_time == now));
        assert!(items.iter().all(|item| item.span_id.is_none()));
        assert!(
            items
                .iter()
                .all(|item| item.added_by == QueueItemAddedBy::User)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_items_checks_the_shape() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let queue = create(&app, &seeded, "Review", definition).await;
        let now = app.state.clock.now_ns();
        app.commit_trace_at(&seeded.project, trace_id(1), 0, now)
            .await;

        let wrong = add(
            &app,
            &seeded,
            queue.id,
            json!({"items": [
                {"trace_id": trace_id(1).to_hex(), "span_id": "0000000000000007"},
                {"session_id": "s1"},
            ]}),
        )
        .await;
        assert_eq!(wrong.status, StatusCode::CREATED);
        let answer: AddedQueueItems = wrong.json();
        assert_eq!(answer.added, 0);
        assert_eq!(
            answer.skipped,
            vec![
                SkippedQueueItem {
                    index: 0,
                    message: "a trace queue takes trace_id".to_owned(),
                },
                SkippedQueueItem {
                    index: 1,
                    message: "a trace queue takes trace_id".to_owned(),
                },
            ]
        );

        let bad = add(
            &app,
            &seeded,
            queue.id,
            json!({"items": [{"trace_id": "xyz"}]}),
        )
        .await;
        assert_refused_at(&bad, "items[0].trace_id");

        let empty = add(&app, &seeded, queue.id, json!({"items": []})).await;
        assert_refused_at(&empty, "items");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_span_and_session_items() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let now = app.state.clock.now_ns();
        let spans = create_of(&app, &seeded, "Spans", "span", definition).await;
        let sessions = create_of(&app, &seeded, "Sessions", "session", definition).await;
        commit_span(&app, &seeded, trace_id(1), 1, now, Some("s-1"), false).await;
        commit_span(
            &app,
            &seeded,
            trace_id(2),
            2,
            now + 5_000_000_000,
            Some("s-1"),
            false,
        )
        .await;
        commit_span(
            &app,
            &seeded,
            trace_id(3),
            3,
            now + 1_000_000_000,
            Some("s-1"),
            false,
        )
        .await;

        let span_added = add(
            &app,
            &seeded,
            spans.id,
            json!({"items": [
                {"trace_id": trace_id(1).to_hex(), "span_id": "d400000000000001"},
                {"trace_id": trace_id(1).to_hex(), "span_id": "d400000000000002"},
            ]}),
        )
        .await;
        assert_eq!(span_added.status, StatusCode::CREATED);
        let answer: AddedQueueItems = span_added.json();
        assert_eq!(answer.added, 1);
        assert_eq!(
            answer.skipped,
            vec![SkippedQueueItem {
                index: 1,
                message: "the span was not found".to_owned(),
            }]
        );
        let span_items = items_of(&app, &seeded, spans.id).await;
        assert_eq!(span_items.len(), 1);
        assert_eq!(span_items[0].span_id.as_deref(), Some("d400000000000001"));
        assert_eq!(span_items[0].target_time, now);

        let session_added = add(
            &app,
            &seeded,
            sessions.id,
            json!({"items": [{"session_id": "s-1"}, {"session_id": "missing"}]}),
        )
        .await;
        assert_eq!(session_added.status, StatusCode::CREATED);
        let answer: AddedQueueItems = session_added.json();
        assert_eq!(answer.added, 1);
        assert_eq!(answer.skipped.len(), 1);
        assert_eq!(answer.skipped[0].index, 1);
        assert_eq!(answer.skipped[0].message, "the session was not found");
        let session_items = items_of(&app, &seeded, sessions.id).await;
        assert_eq!(session_items.len(), 1);
        // The session's earliest visible trace starts first.
        assert_eq!(session_items[0].target_time, now);
        assert_eq!(session_items[0].session_id.as_deref(), Some("s-1"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_items_skips_evaluation_traces() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let queue = create(&app, &seeded, "Review", definition).await;
        let now = app.state.clock.now_ns();
        commit_span(&app, &seeded, trace_id(5), 5, now, None, true).await;

        let response = add(
            &app,
            &seeded,
            queue.id,
            json!({"items": [{"trace_id": trace_id(5).to_hex()}]}),
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED);
        let answer: AddedQueueItems = response.json();
        assert_eq!(answer.added, 0);
        assert_eq!(
            answer.skipped,
            vec![SkippedQueueItem {
                index: 0,
                message: "the trace was not found".to_owned(),
            }]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_items_respects_the_guard() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let traces = create(&app, &seeded, "Traces", definition).await;
        let sessions = create_of(&app, &seeded, "Sessions", "session", definition).await;
        let now = app.state.clock.now_ns();
        app.commit_trace_at(&seeded.project, trace_id(2), 0, now)
            .await;
        commit_span(&app, &seeded, trace_id(1), 1, now, Some("s-guard"), false).await;
        app.state.deletions.guard.add_traces(
            seeded.project.id,
            &[trace_id(1)],
            Uuid::now_v7(),
            app.state.clock.now_ns(),
        );

        let response = add(
            &app,
            &seeded,
            traces.id,
            json!({"items": [
                {"trace_id": trace_id(1).to_hex()},
                {"trace_id": trace_id(2).to_hex()},
            ]}),
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED);
        let answer: AddedQueueItems = response.json();
        assert_eq!(answer.added, 1);
        assert_eq!(
            answer.skipped,
            vec![SkippedQueueItem {
                index: 0,
                message: "a deletion blocks this target".to_owned(),
            }]
        );

        // A session that holds the deleted trace is blocked too.
        let session = add(
            &app,
            &seeded,
            sessions.id,
            json!({"items": [{"session_id": "s-guard"}]}),
        )
        .await;
        let answer: AddedQueueItems = session.json();
        assert_eq!(answer.added, 0);
        assert_eq!(answer.skipped[0].message, "a deletion blocks this target");
    }

    #[test]
    fn a_full_queue_is_422() {
        assert!(matches!(
            items_answer(store::ItemsInsert::TooMany),
            Err(ApiError::TooManyItems)
        ));
        match items_answer(store::ItemsInsert::Done {
            added: 2,
            existing: 1,
        }) {
            Ok(counts) => assert_eq!(counts, (2, 1)),
            Err(_) => panic!("a fitting add is not an error"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn items_list_pages_and_filters_by_status() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let queue = create(&app, &seeded, "Review", definition).await;
        let now = app.state.clock.now_ns();
        for n in 1..=3 {
            app.commit_trace_at(&seeded.project, trace_id(n), 0, now)
                .await;
        }
        let added = add(
            &app,
            &seeded,
            queue.id,
            json!({"items": [
                {"trace_id": trace_id(1).to_hex()},
                {"trace_id": trace_id(2).to_hex()},
                {"trace_id": trace_id(3).to_hex()},
            ]}),
        )
        .await;
        let answer: AddedQueueItems = added.json();
        assert_eq!(answer.added, 3);

        let path = format!("{}/{}/items", base(&seeded), queue.id);
        let first = send(
            &app,
            Method::GET,
            &format!("{path}?limit=2"),
            &seeded.cookie,
            None,
        )
        .await;
        assert_eq!(first.status, StatusCode::OK);
        let page: Page<AnnotationQueueItem> = first.json();
        assert_eq!(page.data.len(), 2);
        let cursor = page.next_cursor.expect("a second page");
        let second = send(
            &app,
            Method::GET,
            &format!("{path}?limit=2&cursor={cursor}"),
            &seeded.cookie,
            None,
        )
        .await;
        let page: Page<AnnotationQueueItem> = second.json();
        assert_eq!(page.data.len(), 1);
        assert!(page.next_cursor.is_none());

        // Complete the first item through the store (the reviewer routes come in Q5).
        let done = page_first_id(&app, &seeded, queue.id).await;
        let finish_now = app.state.clock.now_ns();
        let user = seeded.user_id;
        let queue_id = queue.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    store::finish_tx(
                        conn,
                        queue_id,
                        done,
                        user,
                        store::Finish::Complete,
                        None,
                        finish_now,
                    )
                    .await
                })
            })
            .await
            .unwrap();

        let completed = send(
            &app,
            Method::GET,
            &format!("{path}?status=completed"),
            &seeded.cookie,
            None,
        )
        .await;
        let page: Page<AnnotationQueueItem> = completed.json();
        assert_eq!(page.data.len(), 1);
        assert_eq!(page.data[0].id, done);
        assert_eq!(page.data[0].status, QueueItemStatus::Completed);
        assert_eq!(page.data[0].done_by_user_id, Some(seeded.user_id));

        let pending = send(
            &app,
            Method::GET,
            &format!("{path}?status=pending"),
            &seeded.cookie,
            None,
        )
        .await;
        let page: Page<AnnotationQueueItem> = pending.json();
        assert_eq!(page.data.len(), 2);
    }

    /// The id of the first item of `queue` by id.
    async fn page_first_id(app: &TestApp, seeded: &Seeded, queue: Uuid) -> Uuid {
        items_of(app, seeded, queue).await[0].id
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_item_by_id() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let queue = create(&app, &seeded, "Review", definition).await;
        let other = create(&app, &seeded, "Other", definition).await;
        let now = app.state.clock.now_ns();
        app.commit_trace_at(&seeded.project, trace_id(1), 0, now)
            .await;
        add(
            &app,
            &seeded,
            queue.id,
            json!({"items": [{"trace_id": trace_id(1).to_hex()}]}),
        )
        .await;
        let item = items_of(&app, &seeded, queue.id).await.remove(0);

        let path = format!("{}/{}/items/{}", base(&seeded), queue.id, item.id);
        let found = send(&app, Method::GET, &path, &seeded.cookie, None).await;
        assert_eq!(found.status, StatusCode::OK);
        let found: AnnotationQueueItem = found.json();
        assert_eq!(found, item);
        assert_eq!(found.trace_id, Some(trace_id(1).to_hex()));
        assert_eq!(found.added_by_id, seeded.user_id);

        let unknown = format!("{}/{}/items/{}", base(&seeded), queue.id, Uuid::now_v7());
        let response = send(&app, Method::GET, &unknown, &seeded.cookie, None).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND);

        let wrong_queue = format!("{}/{}/items/{}", base(&seeded), other.id, item.id);
        let response = send(&app, Method::GET, &wrong_queue, &seeded.cookie, None).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_by_target_trace() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let first = create(&app, &seeded, "First", definition).await;
        let second = create(&app, &seeded, "Second", definition).await;
        let now = app.state.clock.now_ns();
        app.commit_trace_at(&seeded.project, trace_id(1), 0, now)
            .await;
        app.commit_trace_at(&seeded.project, trace_id(2), 0, now)
            .await;
        add(
            &app,
            &seeded,
            first.id,
            json!({"items": [{"trace_id": trace_id(1).to_hex()}]}),
        )
        .await;
        add(
            &app,
            &seeded,
            second.id,
            json!({"items": [{"trace_id": trace_id(2).to_hex()}]}),
        )
        .await;

        let path = format!("{}?target_trace_id={}", base(&seeded), trace_id(1).to_hex());
        let response = send(&app, Method::GET, &path, &seeded.cookie, None).await;
        assert_eq!(response.status, StatusCode::OK);
        let page: Page<AnnotationQueue> = response.json();
        assert_eq!(page.data.len(), 1);
        assert_eq!(page.data[0].id, first.id);
        assert_eq!(page.data[0].counts.pending, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn item_permissions() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let queue = create(&app, &seeded, "Review", definition).await;
        let now = app.state.clock.now_ns();
        for n in 1..=4 {
            app.commit_trace_at(&seeded.project, trace_id(n), 0, now)
                .await;
        }
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let member = app
            .add_member(&seeded, "member@example.com", OrgRole::Member)
            .await;
        let items_path = format!("{}/{}/items", base(&seeded), queue.id);
        let body = |trace: u128| json!({"items": [{"trace_id": trace_id(trace).to_hex()}]});

        let listed = send(&app, Method::GET, &items_path, &viewer.cookie, None).await;
        assert_eq!(listed.status, StatusCode::OK);
        let denied = send(
            &app,
            Method::POST,
            &items_path,
            &viewer.cookie,
            Some(body(1)),
        )
        .await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN);

        let write_key = app.write_key(&seeded).await;
        let by_key = app
            .request(Method::POST, &items_path)
            .header("authorization", &format!("Bearer {write_key}"))
            .json(&body(2))
            .send()
            .await;
        assert_eq!(by_key.status, StatusCode::CREATED);

        let write_pat = app.personal_token(member.id, true).await;
        let by_pat = app
            .request(Method::POST, &items_path)
            .header("authorization", &format!("Bearer {write_pat}"))
            .json(&body(3))
            .send()
            .await;
        assert_eq!(by_pat.status, StatusCode::CREATED);

        let by_user = add(&app, &seeded, queue.id, body(4)).await;
        assert_eq!(by_user.status, StatusCode::CREATED);

        let items = items_of(&app, &seeded, queue.id).await;
        let added_by = |trace: u128| {
            items
                .iter()
                .find(|item| item.trace_id == Some(trace_id(trace).to_hex()))
                .map(|item| item.added_by)
        };
        assert_eq!(added_by(2), Some(QueueItemAddedBy::Key));
        assert_eq!(added_by(3), Some(QueueItemAddedBy::Pat));
        assert_eq!(added_by(4), Some(QueueItemAddedBy::User));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn items_hidden_by_the_window() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2026-01-01T00:00:00Z",
            &json!({"data_access_days": 30}),
        );
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let queue = create(&app, &seeded, "Review", definition).await;
        let now = app.state.clock.now_ns();
        let day = 86_400_000_000_000_i64;
        let old = store::NewItem {
            trace_id: Some(trace_id(7).to_hex()),
            span_id: None,
            session_id: None,
            target_time: now - 60 * day,
        };
        let queue_id = queue.id;
        let project = seeded.project.id;
        let user = seeded.user_id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    store::insert_items_tx(
                        conn,
                        queue_id,
                        project,
                        &[old],
                        (store::AddedByKind::User, user),
                        now,
                    )
                    .await
                })
            })
            .await
            .unwrap();

        assert!(items_of(&app, &seeded, queue.id).await.is_empty());
        let path = format!("{}/{}", base(&seeded), queue.id);
        let response = send(&app, Method::GET, &path, &seeded.cookie, None).await;
        let fetched: AnnotationQueue = response.json();
        assert_eq!(fetched.counts.pending, 0);
    }

    /// A trace queue of `n` traces (ids 1 to n), all added by the owner.
    async fn queue_of(app: &TestApp, seeded: &Seeded, name: &str, n: u128) -> AnnotationQueue {
        let definition = define(app, seeded.project.id, "relevance", false).await;
        let queue = create(app, seeded, name, definition).await;
        let now = app.state.clock.now_ns();
        let mut targets = Vec::new();
        for trace in 1..=n {
            app.commit_trace_at(&seeded.project, trace_id(trace), 0, now)
                .await;
            targets.push(json!({"trace_id": trace_id(trace).to_hex()}));
        }
        let added = add(app, seeded, queue.id, json!({"items": targets})).await;
        assert_eq!(added.status, StatusCode::CREATED);
        queue
    }

    /// `POST .../next` as `cookie`, with `body`.
    async fn claim(
        app: &TestApp,
        seeded: &Seeded,
        queue: Uuid,
        cookie: &str,
        body: Value,
    ) -> TestResponse {
        let path = format!("{}/{queue}/next", base(seeded));
        send(app, Method::POST, &path, cookie, Some(body)).await
    }

    /// `POST .../items/{item}/{action}` as `cookie`, with an optional body.
    async fn act(
        app: &TestApp,
        seeded: &Seeded,
        queue: Uuid,
        item: Uuid,
        cookie: &str,
        action: &str,
        body: Option<Value>,
    ) -> TestResponse {
        let path = format!("{}/{queue}/items/{item}/{action}", base(seeded));
        send(app, Method::POST, &path, cookie, body).await
    }

    /// A claimed item from a `200` answer.
    #[allow(clippy::needless_pass_by_value)]
    fn claimed(response: TestResponse) -> AnnotationQueueItem {
        assert_eq!(response.status, StatusCode::OK);
        response.json()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_reviewers_never_get_the_same_item() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let queue = queue_of(&app, &seeded, "Review", 3).await;
        let bob = app
            .add_member(&seeded, "bob@example.com", OrgRole::Member)
            .await;
        let carol = app
            .add_member(&seeded, "carol@example.com", OrgRole::Member)
            .await;
        let dan = app
            .add_member(&seeded, "dan@example.com", OrgRole::Member)
            .await;

        let first = claimed(claim(&app, &seeded, queue.id, &seeded.cookie, json!({})).await);
        let second = claimed(
            claim(
                &app,
                &seeded,
                queue.id,
                &seeded.cookie,
                json!({"after": first.id}),
            )
            .await,
        );
        // Ada moved on, and her first lease is gone.
        assert_ne!(second.id, first.id);
        let items = items_of(&app, &seeded, queue.id).await;
        let released = items
            .iter()
            .find(|item| item.id == first.id)
            .expect("first item");
        assert_eq!(released.leased_by_user_id, None);

        let bobs = claimed(claim(&app, &seeded, queue.id, &bob.cookie, json!({})).await);
        assert_ne!(bobs.id, second.id, "Ada holds the second item");
        let carols = claimed(claim(&app, &seeded, queue.id, &carol.cookie, json!({})).await);
        assert_ne!(carols.id, second.id);
        assert_ne!(carols.id, bobs.id);

        // Every item is leased by someone else now.
        let none = claim(&app, &seeded, queue.id, &dan.cookie, json!({})).await;
        assert_eq!(none.status, StatusCode::NO_CONTENT);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lease_expires_after_five_minutes() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let queue = queue_of(&app, &seeded, "Review", 1).await;
        let bob = app
            .add_member(&seeded, "bob@example.com", OrgRole::Member)
            .await;
        let item = claimed(claim(&app, &seeded, queue.id, &bob.cookie, json!({})).await);

        let held = act(
            &app,
            &seeded,
            queue.id,
            item.id,
            &seeded.cookie,
            "lease",
            None,
        )
        .await;
        assert_eq!(held.status, StatusCode::CONFLICT);

        app.state.clock.advance(Duration::from_secs(301));
        let taken = act(
            &app,
            &seeded,
            queue.id,
            item.id,
            &seeded.cookie,
            "lease",
            None,
        )
        .await;
        assert_eq!(taken.status, StatusCode::OK);
        let taken: AnnotationQueueItem = taken.json();
        assert_eq!(taken.leased_by_user_id, Some(seeded.user_id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn renew_and_release() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let queue = queue_of(&app, &seeded, "Review", 1).await;
        let bob = app
            .add_member(&seeded, "bob@example.com", OrgRole::Member)
            .await;
        let first = claimed(claim(&app, &seeded, queue.id, &seeded.cookie, json!({})).await);
        let first_expiry = first.lease_expires_at.expect("a live lease");

        app.state.clock.advance(Duration::from_secs(60));
        let renewed = act(
            &app,
            &seeded,
            queue.id,
            first.id,
            &seeded.cookie,
            "lease",
            None,
        )
        .await;
        assert_eq!(renewed.status, StatusCode::OK);
        let renewed: AnnotationQueueItem = renewed.json();
        assert!(renewed.lease_expires_at.expect("renewed") > first_expiry);

        let refused = act(
            &app,
            &seeded,
            queue.id,
            first.id,
            &bob.cookie,
            "lease",
            None,
        )
        .await;
        assert_eq!(refused.status, StatusCode::CONFLICT);
        let body: Value = refused.json();
        assert_eq!(body["errors"][0]["field"], "leased_by_user_id");
        assert_eq!(body["errors"][0]["message"], seeded.user_id.to_string());

        let path = format!("{}/{}/items/{}/lease", base(&seeded), queue.id, first.id);
        let released = send(&app, Method::DELETE, &path, &seeded.cookie, None).await;
        assert_eq!(released.status, StatusCode::NO_CONTENT);

        let taken = act(
            &app,
            &seeded,
            queue.id,
            first.id,
            &bob.cookie,
            "lease",
            None,
        )
        .await;
        assert_eq!(taken.status, StatusCode::OK);
        let taken: AnnotationQueueItem = taken.json();
        assert_eq!(taken.leased_by_user_id, Some(bob.id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn complete_skip_reopen() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let queue = queue_of(&app, &seeded, "Review", 1).await;
        let item = claimed(claim(&app, &seeded, queue.id, &seeded.cookie, json!({})).await);

        let done = act(
            &app,
            &seeded,
            queue.id,
            item.id,
            &seeded.cookie,
            "complete",
            None,
        )
        .await;
        assert_eq!(done.status, StatusCode::OK);
        let done: AnnotationQueueItem = done.json();
        assert_eq!(done.status, QueueItemStatus::Completed);
        assert_eq!(done.done_by_user_id, Some(seeded.user_id));
        assert_eq!(done.leased_by_user_id, None);

        let again = act(
            &app,
            &seeded,
            queue.id,
            item.id,
            &seeded.cookie,
            "complete",
            None,
        )
        .await;
        assert_eq!(again.status, StatusCode::CONFLICT);
        let body: Value = again.json();
        assert_eq!(body["detail"], "the item is not pending");

        let reopened = act(
            &app,
            &seeded,
            queue.id,
            item.id,
            &seeded.cookie,
            "reopen",
            None,
        )
        .await;
        assert_eq!(reopened.status, StatusCode::OK);
        let reopened: AnnotationQueueItem = reopened.json();
        assert_eq!(reopened.status, QueueItemStatus::Pending);

        let pending = act(
            &app,
            &seeded,
            queue.id,
            item.id,
            &seeded.cookie,
            "reopen",
            None,
        )
        .await;
        assert_eq!(pending.status, StatusCode::CONFLICT);
        let body: Value = pending.json();
        assert_eq!(body["detail"], "the item is pending");

        let skipped = act(
            &app,
            &seeded,
            queue.id,
            item.id,
            &seeded.cookie,
            "skip",
            Some(json!({"reason": "  off topic "})),
        )
        .await;
        assert_eq!(skipped.status, StatusCode::OK);
        let skipped: AnnotationQueueItem = skipped.json();
        assert_eq!(skipped.status, QueueItemStatus::Skipped);
        assert_eq!(skipped.skip_reason.as_deref(), Some("off topic"));
        assert!(
            app.state
                .analytics
                .counters
                .get("annotation_queue.item_completed")
                >= 1,
            "annotation_queue.item_completed not counted"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn complete_without_lease_is_allowed_when_free() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let queue = queue_of(&app, &seeded, "Review", 2).await;
        let bob = app
            .add_member(&seeded, "bob@example.com", OrgRole::Member)
            .await;
        let held = claimed(claim(&app, &seeded, queue.id, &bob.cookie, json!({})).await);

        let refused = act(
            &app,
            &seeded,
            queue.id,
            held.id,
            &seeded.cookie,
            "complete",
            None,
        )
        .await;
        assert_eq!(refused.status, StatusCode::CONFLICT);
        let body: Value = refused.json();
        assert_eq!(body["code"], "conflict");
        assert_eq!(body["errors"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["errors"][0]["field"], "leased_by_user_id");
        assert_eq!(body["errors"][0]["message"], bob.id.to_string());

        let free = items_of(&app, &seeded, queue.id)
            .await
            .into_iter()
            .find(|item| item.id != held.id)
            .expect("the free item");
        let done = act(
            &app,
            &seeded,
            queue.id,
            free.id,
            &seeded.cookie,
            "complete",
            None,
        )
        .await;
        assert_eq!(done.status, StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bodies() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let queue = queue_of(&app, &seeded, "Review", 2).await;
        let strange = claim(
            &app,
            &seeded,
            queue.id,
            &seeded.cookie,
            json!({"unknown": 1}),
        )
        .await;
        assert_eq!(strange.status, StatusCode::BAD_REQUEST);

        let claimed_item = claimed(claim(&app, &seeded, queue.id, &seeded.cookie, json!({})).await);
        // Complete ignores any body, so none is fine.
        let done = act(
            &app,
            &seeded,
            queue.id,
            claimed_item.id,
            &seeded.cookie,
            "complete",
            None,
        )
        .await;
        assert_eq!(done.status, StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keys_cannot_review() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let queue = queue_of(&app, &seeded, "Review", 1).await;
        let path = format!("{}/{}/next", base(&seeded), queue.id);

        let write_key = app.write_key(&seeded).await;
        let by_key = app
            .request(Method::POST, &path)
            .header("authorization", &format!("Bearer {write_key}"))
            .json(&json!({}))
            .send()
            .await;
        assert_eq!(by_key.status, StatusCode::FORBIDDEN);

        let member = app
            .add_member(&seeded, "member@example.com", OrgRole::Member)
            .await;
        let read_pat = app.personal_token(member.id, false).await;
        let by_read_pat = app
            .request(Method::POST, &path)
            .header("authorization", &format!("Bearer {read_pat}"))
            .json(&json!({}))
            .send()
            .await;
        assert_eq!(by_read_pat.status, StatusCode::FORBIDDEN);

        let write_pat = app.personal_token(member.id, true).await;
        let by_write_pat = app
            .request(Method::POST, &path)
            .header("authorization", &format!("Bearer {write_pat}"))
            .json(&json!({}))
            .send()
            .await;
        assert_eq!(by_write_pat.status, StatusCode::OK);
        let item: AnnotationQueueItem = by_write_pat.json();
        assert_eq!(item.leased_by_user_id, Some(member.id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn viewer_cannot_review() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let queue = queue_of(&app, &seeded, "Review", 1).await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let response = claim(&app, &seeded, queue.id, &viewer.cookie, json!({})).await;
        assert_eq!(response.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hidden_items_are_404() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        // Wide enough to see the item, which is 60 days old, while it is added.
        set_limits(
            &app,
            seeded.org_id,
            "2026-01-01T00:00:00Z",
            &json!({"data_access_days": 3650}),
        );
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let queue = create(&app, &seeded, "Review", definition).await;
        let now = app.state.clock.now_ns();
        let old = store::NewItem {
            trace_id: Some(trace_id(7).to_hex()),
            span_id: None,
            session_id: None,
            target_time: now - 60 * 86_400_000_000_000_i64,
        };
        let (queue_id, project, user) = (queue.id, seeded.project.id, seeded.user_id);
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    store::insert_items_tx(
                        conn,
                        queue_id,
                        project,
                        &[old],
                        (store::AddedByKind::User, user),
                        now,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        let item = items_of(&app, &seeded, queue.id).await.remove(0);

        // Narrow the window: the item is now hidden.
        set_limits(
            &app,
            seeded.org_id,
            "2026-01-01T00:00:01Z",
            &json!({"data_access_days": 30}),
        );
        let done = act(
            &app,
            &seeded,
            queue.id,
            item.id,
            &seeded.cookie,
            "complete",
            None,
        )
        .await;
        assert_eq!(done.status, StatusCode::NOT_FOUND);
        let none = claim(&app, &seeded, queue.id, &seeded.cookie, json!({})).await;
        assert_eq!(none.status, StatusCode::NO_CONTENT);
    }

    /// One hour in epoch milliseconds, and one day.
    const HOUR_MS: i64 = 3_600_000;
    const DAY_MS: i64 = 86_400_000;

    /// The window `[now - 1 h, now + 1 h)` of a matching body, as epoch milliseconds.
    fn window(now: i64) -> Value {
        let ms = now / 1_000_000;
        json!({
            "from": (ms - HOUR_MS).to_string(),
            "to": (ms + HOUR_MS).to_string(),
        })
    }

    /// Calls the matching preview (`/preview`) or the add (`""`) of `queue` as `cookie`.
    async fn matching(
        app: &TestApp,
        seeded: &Seeded,
        queue: Uuid,
        action: &str,
        body: Value,
    ) -> TestResponse {
        let path = format!("{}/{queue}/items/matching{action}", base(seeded));
        send(app, Method::POST, &path, &seeded.cookie, Some(body)).await
    }

    /// Commits one span of `trace` named `name`, a root span starting at `start`.
    async fn commit_named(app: &TestApp, seeded: &Seeded, trace: TraceId, name: &str, start: i64) {
        let mut span = dataset_span(0);
        span.trace_id = trace;
        span.span_id = "e400000000000001".parse::<SpanId>().expect("span id hex");
        span.parent_span_id = None;
        span.name = name.to_owned();
        span.start_time = start;
        span.end_time = start + 1_000_000_000;
        span.ingest_time = app.state.clock.now_ns();
        app.commit_spans(&seeded.project, vec![span]).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn preview_and_add_matching_traces() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let queue = create(&app, &seeded, "Review", definition).await;
        let now = app.state.clock.now_ns();
        for n in 1..=3 {
            app.commit_trace_at(&seeded.project, trace_id(n), 0, now)
                .await;
        }
        app.commit_trace_at(
            &seeded.project,
            trace_id(4),
            0,
            now - 2 * DAY_MS * 1_000_000,
        )
        .await;

        let preview = matching(&app, &seeded, queue.id, "/preview", window(now)).await;
        assert_eq!(preview.status, StatusCode::OK);
        let counted: QueueMatchingPreview = preview.json();
        assert_eq!(
            counted,
            QueueMatchingPreview {
                matched: 3,
                capped: false,
            }
        );

        let added = matching(&app, &seeded, queue.id, "", window(now)).await;
        assert_eq!(added.status, StatusCode::CREATED);
        let answer: QueueMatchingAdded = added.json();
        assert_eq!(
            answer,
            QueueMatchingAdded {
                added: 3,
                existing: 0,
                blocked: 0,
                capped: false,
            }
        );

        let again = matching(&app, &seeded, queue.id, "", window(now)).await;
        let answer: QueueMatchingAdded = again.json();
        assert_eq!(answer.added, 0);
        assert_eq!(answer.existing, 3);
        assert_eq!(items_of(&app, &seeded, queue.id).await.len(), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn matching_respects_the_filter_and_search() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let queue = create(&app, &seeded, "Review", definition).await;
        let now = app.state.clock.now_ns();
        commit_named(&app, &seeded, trace_id(1), "needle", now).await;
        app.commit_trace_at(&seeded.project, trace_id(2), 0, now)
            .await;

        let mut body = window(now);
        body["filter"] = json!("name = 'needle'");
        let preview = matching(&app, &seeded, queue.id, "/preview", body.clone()).await;
        assert_eq!(preview.status, StatusCode::OK);
        let counted: QueueMatchingPreview = preview.json();
        assert_eq!(counted.matched, 1);

        let added = matching(&app, &seeded, queue.id, "", body).await;
        assert_eq!(added.status, StatusCode::CREATED);
        let items = items_of(&app, &seeded, queue.id).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].trace_id, Some(trace_id(1).to_hex()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn matching_spans_and_sessions() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let spans = create_of(&app, &seeded, "Spans", "span", definition).await;
        let sessions = create_of(&app, &seeded, "Sessions", "session", definition).await;
        let now = app.state.clock.now_ns();
        commit_span(&app, &seeded, trace_id(1), 1, now, Some("s-1"), false).await;
        commit_span(
            &app,
            &seeded,
            trace_id(2),
            2,
            now + 1_000_000_000,
            Some("s-1"),
            false,
        )
        .await;
        commit_span(&app, &seeded, trace_id(3), 3, now, None, false).await;

        let span_added = matching(&app, &seeded, spans.id, "", window(now)).await;
        assert_eq!(span_added.status, StatusCode::CREATED);
        let answer: QueueMatchingAdded = span_added.json();
        assert_eq!(answer.added, 3);
        let mut span_items = items_of(&app, &seeded, spans.id).await;
        span_items.sort_by_key(|item| item.trace_id.clone());
        assert_eq!(span_items.len(), 3);
        assert_eq!(span_items[1].target_time, now + 1_000_000_000);
        assert!(span_items.iter().all(|item| item.span_id.is_some()));

        // Session `s-1` is one target, with the start of its earliest trace.
        let session_added = matching(&app, &seeded, sessions.id, "", window(now)).await;
        let answer: QueueMatchingAdded = session_added.json();
        assert_eq!(answer.added, 1);
        let session_items = items_of(&app, &seeded, sessions.id).await;
        assert_eq!(session_items.len(), 1);
        assert_eq!(session_items[0].session_id.as_deref(), Some("s-1"));
        assert_eq!(session_items[0].target_time, now);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_queue_refuses_search() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let sessions = create_of(&app, &seeded, "Sessions", "session", definition).await;
        let now = app.state.clock.now_ns();
        let mut body = window(now);
        body["search"] = json!("refund");

        let preview = matching(&app, &seeded, sessions.id, "/preview", body.clone()).await;
        assert_refused_at(&preview, "search");
        let added = matching(&app, &seeded, sessions.id, "", body).await;
        assert_refused_at(&added, "search");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn matching_is_capped() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let traces = create(&app, &seeded, "Traces", definition).await;
        let spans = create_of(&app, &seeded, "Spans", "span", definition).await;
        let sessions = create_of(&app, &seeded, "Sessions", "session", definition).await;
        let now = app.state.clock.now_ns();
        for n in 1..=3_u64 {
            let session = format!("c-{n}");
            let trace = trace_id(u128::from(n));
            commit_span(&app, &seeded, trace, n, now, Some(&session), false).await;
        }
        let body: QueueMatchingRequest =
            serde_json::from_value(window(now)).expect("a matching body");

        for queue in [traces.id, spans.id, sessions.id] {
            let Ok(matching) = matching_targets(
                &app.state,
                seeded.org_id,
                seeded.project.id,
                queue,
                &body,
                2,
            )
            .await
            else {
                panic!("the matching targets of a queue");
            };
            assert_eq!(matching.found.len(), 2, "queue {queue}");
            assert!(matching.capped, "queue {queue}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn matching_skips_guarded_targets() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let queue = create(&app, &seeded, "Review", definition).await;
        let now = app.state.clock.now_ns();
        app.commit_trace_at(&seeded.project, trace_id(1), 0, now)
            .await;
        app.commit_trace_at(&seeded.project, trace_id(2), 0, now)
            .await;
        app.state.deletions.guard.add_traces(
            seeded.project.id,
            &[trace_id(1)],
            Uuid::now_v7(),
            app.state.clock.now_ns(),
        );

        let added = matching(&app, &seeded, queue.id, "", window(now)).await;
        assert_eq!(added.status, StatusCode::CREATED);
        let answer: QueueMatchingAdded = added.json();
        assert_eq!(
            answer,
            QueueMatchingAdded {
                added: 1,
                existing: 0,
                blocked: 1,
                capped: false,
            }
        );
        let items = items_of(&app, &seeded, queue.id).await;
        assert_eq!(items[0].trace_id, Some(trace_id(2).to_hex()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn matching_never_adds_evaluation_traces() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let queue = create(&app, &seeded, "Review", definition).await;
        let now = app.state.clock.now_ns();
        commit_span(&app, &seeded, trace_id(5), 5, now, None, true).await;
        app.commit_trace_at(&seeded.project, trace_id(6), 0, now)
            .await;

        let preview = matching(&app, &seeded, queue.id, "/preview", window(now)).await;
        let counted: QueueMatchingPreview = preview.json();
        assert_eq!(counted.matched, 1);
        let added = matching(&app, &seeded, queue.id, "", window(now)).await;
        let answer: QueueMatchingAdded = added.json();
        assert_eq!(answer.added, 1);
        let items = items_of(&app, &seeded, queue.id).await;
        assert_eq!(items[0].trace_id, Some(trace_id(6).to_hex()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn matching_never_adds_evaluation_traces_even_with_an_environment_filter() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let traces = create(&app, &seeded, "Traces", definition).await;
        let spans = create_of(&app, &seeded, "Spans", "span", definition).await;
        let now = app.state.clock.now_ns();
        commit_span(&app, &seeded, trace_id(5), 5, now, None, true).await;
        app.commit_trace_at(&seeded.project, trace_id(6), 0, now)
            .await;

        // The filter names `environment`, which lifts the scan's evaluation exclusion.
        let mut body = window(now);
        body["filter"] = json!("environment = 'cetra-eval'");
        for queue in [traces.id, spans.id] {
            let added = matching(&app, &seeded, queue, "", body.clone()).await;
            assert_eq!(added.status, StatusCode::CREATED, "queue {queue}");
            let answer: QueueMatchingAdded = added.json();
            assert_eq!(answer.added, 0, "queue {queue}");
        }
        assert!(items_of(&app, &seeded, traces.id).await.is_empty());
        assert!(items_of(&app, &seeded, spans.id).await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn matching_respects_the_data_window() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2026-01-01T00:00:00Z",
            &json!({"data_access_days": 30}),
        );
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let queue = create(&app, &seeded, "Review", definition).await;
        let now = app.state.clock.now_ns();
        app.commit_trace_at(&seeded.project, trace_id(1), 0, now)
            .await;
        app.commit_trace_at(
            &seeded.project,
            trace_id(2),
            0,
            now - 60 * DAY_MS * 1_000_000,
        )
        .await;

        // The window reaches back 90 days; the data window shows 30.
        let mut body = window(now);
        body["from"] = json!((now / 1_000_000 - 90 * DAY_MS).to_string());
        let preview = matching(&app, &seeded, queue.id, "/preview", body.clone()).await;
        let counted: QueueMatchingPreview = preview.json();
        assert_eq!(counted.matched, 1);
        let added = matching(&app, &seeded, queue.id, "", body).await;
        let answer: QueueMatchingAdded = added.json();
        assert_eq!(answer.added, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn matching_permissions() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let queue = create(&app, &seeded, "Review", definition).await;
        let now = app.state.clock.now_ns();
        app.commit_trace_at(&seeded.project, trace_id(1), 0, now)
            .await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;

        let denied = send(
            &app,
            Method::POST,
            &format!("{}/{}/items/matching/preview", base(&seeded), queue.id),
            &viewer.cookie,
            Some(window(now)),
        )
        .await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN);
        let denied = send(
            &app,
            Method::POST,
            &format!("{}/{}/items/matching", base(&seeded), queue.id),
            &viewer.cookie,
            Some(window(now)),
        )
        .await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN);

        let read_key = app
            .request(
                Method::POST,
                &format!("{}/{}/items/matching", base(&seeded), queue.id),
            )
            .header("authorization", &format!("Bearer {}", seeded.read_key))
            .json(&window(now))
            .send()
            .await;
        assert_eq!(read_key.status, StatusCode::FORBIDDEN);

        let write_key = app.write_key(&seeded).await;
        let added = app
            .request(
                Method::POST,
                &format!("{}/{}/items/matching", base(&seeded), queue.id),
            )
            .header("authorization", &format!("Bearer {write_key}"))
            .json(&window(now))
            .send()
            .await;
        assert_eq!(added.status, StatusCode::CREATED);
        let answer: QueueMatchingAdded = added.json();
        assert_eq!(answer.added, 1);
        let items = items_of(&app, &seeded, queue.id).await;
        assert_eq!(items[0].added_by, QueueItemAddedBy::Key);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bad_matching_bodies_are_400() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let definition = define(&app, seeded.project.id, "relevance", false).await;
        let queue = create(&app, &seeded, "Review", definition).await;
        let now = app.state.clock.now_ns();

        let mut bad_filter = window(now);
        bad_filter["filter"] = json!("name =");
        let response = matching(&app, &seeded, queue.id, "/preview", bad_filter).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        let problem: Value = response.json();
        assert_eq!(problem["code"], "invalid_filter");

        let mut empty_window = window(now);
        let from = empty_window["from"].clone();
        empty_window["to"] = from;
        let response = matching(&app, &seeded, queue.id, "", empty_window).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);

        let missing = matching(&app, &seeded, Uuid::now_v7(), "/preview", window(now)).await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
    }
}
