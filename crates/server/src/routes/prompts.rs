//! Prompt routes (spec 09 § Prompts, D-V-2 to D-V-13). Project readers list and fetch; the
//! `ManagePrompts` routes write. Protected labels (D-V-10) need a project admin or an API key.

use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use cetra_core::ids::ProjectId;
use cetra_storage::PromptVersionMetrics as StoredVersionMetrics;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::analytics::actor::Actor;
use crate::analytics::catalog::Event;
use crate::analytics::record;
use crate::api::common::{FieldError, Page, PageParams, ProblemCode, TimeParam};
use crate::api::prompts::{
    CreatePromptRequest, CreateVersionRequest, Prompt, PromptKind, PromptLabel, PromptLabelEvent,
    PromptListParams, PromptMetrics, PromptMetricsParams, PromptScoreMean, PromptSettings,
    PromptVersion, PromptVersionMetrics, ResolveParams, SetLabelRequest, UpdatePromptRequest,
};
use crate::auth::principal::Principal;
use crate::auth::roles::{ProjectPerm, Role};
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::meta::MetaError;
use crate::meta::prompts::{
    self as store, NewPrompt, NewVersion, PromptFilter, PromptRow, PromptType, VersionRow,
};
use crate::prompts::{
    DEFAULT_LABEL, LATEST, MAX_LABELS_PER_REQUEST, MAX_PROTECTED, check_commit_message,
    check_config, check_description, check_label, check_labels, check_name, check_tags, parse_body,
};

use super::datasets::created_by;
use super::orgs::page_named;
use super::read::{Deadline, open_query};

/// The body of a prompt create, a version create and a PATCH: a template of at most 256 KiB,
/// a config of at most 64 KiB, and the JSON around them.
const PROMPT_BODY_LIMIT: usize = 512 * 1024;

/// A versions page holds at most this many versions: each carries its template and config.
const MAX_VERSION_PAGE: u32 = 50;

const DAY_NS: i64 = 86_400_000_000_000;

/// The metrics window when the request names no `from`: 7 days (D-V-17).
const METRICS_DEFAULT_WINDOW_NS: i64 = 7 * DAY_NS;

/// The longest metrics window: 90 days (D-V-17).
const METRICS_MAX_WINDOW_NS: i64 = 90 * DAY_NS;

/// Prompt routes. Reads need project view; writes need [`ProjectPerm::ManagePrompts`]. Not
/// session-only.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/prompts",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_prompts),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/prompts",
            access: Access::Project {
                perm: ProjectPerm::ManagePrompts,
                session_only: false,
            },
            documented: true,
            handler: post(create_prompt).layer(DefaultBodyLimit::max(PROMPT_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/prompts/{prompt}",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_prompt),
        },
        RouteDef {
            method: Method::PATCH,
            path: "/api/v1/projects/{project}/prompts/{prompt}",
            access: Access::Project {
                perm: ProjectPerm::ManagePrompts,
                session_only: false,
            },
            documented: true,
            handler: patch(update_prompt).layer(DefaultBodyLimit::max(PROMPT_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::DELETE,
            path: "/api/v1/projects/{project}/prompts/{prompt}",
            access: Access::Project {
                perm: ProjectPerm::ManagePrompts,
                session_only: false,
            },
            documented: true,
            handler: delete(delete_prompt),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/prompts/{prompt}/resolve",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(resolve_prompt),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/prompts/{prompt}/versions",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_versions),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/prompts/{prompt}/versions",
            access: Access::Project {
                perm: ProjectPerm::ManagePrompts,
                session_only: false,
            },
            documented: true,
            handler: post(create_version).layer(DefaultBodyLimit::max(PROMPT_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/prompts/{prompt}/versions/{version}",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_version),
        },
        RouteDef {
            method: Method::PUT,
            path: "/api/v1/projects/{project}/prompts/{prompt}/labels/{label}",
            access: Access::Project {
                perm: ProjectPerm::ManagePrompts,
                session_only: false,
            },
            documented: true,
            handler: put(set_label),
        },
        RouteDef {
            method: Method::DELETE,
            path: "/api/v1/projects/{project}/prompts/{prompt}/labels/{label}",
            access: Access::Project {
                perm: ProjectPerm::ManagePrompts,
                session_only: false,
            },
            documented: true,
            handler: delete(remove_label),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/prompts/{prompt}/label-events",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_label_events),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/prompts/{prompt}/metrics",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(prompt_metrics),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/prompt-settings",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_settings),
        },
        RouteDef {
            method: Method::PUT,
            path: "/api/v1/projects/{project}/prompt-settings",
            access: Access::Project {
                perm: ProjectPerm::ManageSettings,
                session_only: false,
            },
            documented: true,
            handler: put(put_settings),
        },
    ]
}

/// The `{prompt}` segment: the prompt's name (D-V-2).
#[derive(Debug, Deserialize)]
pub(crate) struct PromptPath {
    pub(crate) prompt: String,
}

/// The `{prompt}` and `{version}` segments of a version route.
#[derive(Debug, Deserialize)]
pub(crate) struct PromptVersionPath {
    pub(crate) prompt: String,
    pub(crate) version: i64,
}

/// The `{prompt}` and `{label}` segments of a label route.
#[derive(Debug, Deserialize)]
pub(crate) struct PromptLabelPath {
    pub(crate) prompt: String,
    pub(crate) label: String,
}

/// What a write did. A protected label refused (D-V-10) and a missing prompt are answers, not
/// store errors: the transaction returns them and writes nothing.
enum Written<T> {
    Done(T),
    Missing,
    Protected,
}

/// The value of a write, or the 404 or 403 that the outcome stands for.
fn settle<T>(outcome: Written<T>) -> Result<T, ApiError> {
    match outcome {
        Written::Done(value) => Ok(value),
        Written::Missing => Err(ApiError::NotFound),
        Written::Protected => Err(ApiError::Forbidden {
            code: ProblemCode::Forbidden,
        }),
    }
}

fn bad_request(errors: Vec<FieldError>) -> ApiError {
    ApiError::BadRequest {
        detail: None,
        errors,
    }
}

fn field_error(field: &str, message: &str) -> FieldError {
    FieldError {
        field: field.to_owned(),
        message: message.to_owned(),
        position: None,
    }
}

/// A stored version that does not read back as it was written: `MetaError::Corrupt`, a 500.
fn corrupt(column: &'static str) -> ApiError {
    MetaError::Corrupt {
        table: "prompt_versions",
        column,
    }
    .into()
}

fn prompt_type_of(kind: PromptKind) -> PromptType {
    match kind {
        PromptKind::Text => PromptType::Text,
        PromptKind::Chat => PromptType::Chat,
    }
}

fn kind_of(prompt_type: PromptType) -> PromptKind {
    match prompt_type {
        PromptType::Text => PromptKind::Text,
        PromptType::Chat => PromptKind::Chat,
    }
}

/// D-V-10: a project admin, or an API key.
/// A key reaching a `ManagePrompts` route has the `write` scope (`ProjectPerm::ManagePrompts.key_scope()`); public keys hold only `feedback`.
fn may_touch_protected(principal: &Principal, access: &ProjectAccess) -> bool {
    matches!(principal, Principal::ApiKey { .. }) || access.role == Some(Role::Admin)
}

/// True when `labels` holds a protected label and the caller may not move it (D-V-10).
async fn refused_by_protection(
    conn: &mut SqliteConnection,
    project: ProjectId,
    labels: &[String],
    may: bool,
) -> Result<bool, MetaError> {
    if may {
        return Ok(false);
    }
    let protected = store::protected_labels_tx(conn, project).await?;
    Ok(labels.iter().any(|label| protected.contains(label)))
}

/// The prompt named by the path, or 404.
async fn find(state: &AppState, project: ProjectId, name: &str) -> Result<PromptRow, ApiError> {
    store::get_prompt(state.meta.reader(), project, name)
        .await?
        .ok_or(ApiError::NotFound)
}

/// API form of a prompt row.
fn prompt_json(row: &PromptRow) -> Prompt {
    Prompt {
        id: row.id,
        name: row.name.clone(),
        kind: kind_of(row.prompt_type),
        description: row.description.clone(),
        tags: row.tags.clone(),
        latest_version: row.latest_version,
        labels: row
            .labels
            .iter()
            .map(|label| PromptLabel {
                label: label.label.clone(),
                version: label.version,
                updated_at: label.updated_at,
            })
            .collect(),
        created_by: row.created_by.clone(),
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

/// API form of a version. The labels are the version's stored labels, plus `latest` when this
/// is the prompt's latest version, sorted. The stored texts are parsed back and the variables
/// and placeholders re-read from the template.
pub(crate) fn version_json(
    prompt: &PromptRow,
    version: &VersionRow,
) -> Result<PromptVersion, ApiError> {
    let template: Value =
        serde_json::from_str(&version.template).map_err(|_| corrupt("template"))?;
    let config: Value = serde_json::from_str(&version.config).map_err(|_| corrupt("config"))?;
    let body = parse_body(prompt.prompt_type, &template).map_err(|_| corrupt("template"))?;
    let mut labels = version.labels.clone();
    if version.version == prompt.latest_version {
        labels.push(LATEST.to_owned());
        labels.sort();
    }
    Ok(PromptVersion {
        name: prompt.name.clone(),
        kind: kind_of(prompt.prompt_type),
        version: version.version,
        template,
        config,
        labels,
        variables: body.variables(),
        placeholders: body.placeholders(),
        commit_message: version.commit_message.clone(),
        created_by: version.created_by.clone(),
        created_at: version.created_at,
    })
}

/// Checks a template, config, labels and commit message for a version of `prompt_type`
/// (D-V-3 to D-V-8). Every field's error is returned.
fn new_version(
    prompt_type: PromptType,
    template: &Value,
    config: Option<&Value>,
    labels: &[String],
    commit_message: Option<&str>,
    actor: Option<String>,
) -> Result<NewVersion, Vec<FieldError>> {
    let mut errors = Vec::new();
    let body = match parse_body(prompt_type, template) {
        Ok(body) => Some(body),
        Err(found) => {
            errors.extend(found);
            None
        }
    };
    let default_config = json!({});
    let config = match check_config(config.unwrap_or(&default_config)) {
        Ok(text) => Some(text),
        Err(found) => {
            errors.extend(found);
            None
        }
    };
    let labels = match check_labels("labels", labels, MAX_LABELS_PER_REQUEST) {
        Ok(labels) => Some(labels),
        Err(found) => {
            errors.extend(found);
            None
        }
    };
    let commit_message = match commit_message {
        None => None,
        Some(text) => match check_commit_message(text) {
            Ok(text) => text,
            Err(error) => {
                errors.push(error);
                None
            }
        },
    };
    let (Some(body), Some(config), Some(labels)) = (body, config, labels) else {
        return Err(errors);
    };
    if !errors.is_empty() {
        return Err(errors);
    }
    let template = serde_json::to_string(&body.to_json())
        .map_err(|_| vec![field_error("template", "must be valid JSON")])?;
    Ok(NewVersion {
        template,
        config,
        commit_message,
        labels,
        actor,
    })
}

/// `GET …/prompts`: filters by `q`, `label` and `tag`; keyset-paged by name, then id.
async fn list_prompts(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<PromptListParams>,
) -> Result<error::Json<Page<Prompt>>, ApiError> {
    let mut errors = Vec::new();
    let label = match params.label.as_deref() {
        None | Some(LATEST) => None,
        Some(label) => match check_label("label", label) {
            Ok(()) => Some(label.to_owned()),
            Err(error) => {
                errors.push(error);
                None
            }
        },
    };
    if !errors.is_empty() {
        return Err(bad_request(errors));
    }
    let filter = PromptFilter {
        q: params
            .q
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_owned),
        label,
        tag: params.tag,
    };
    let rows = store::list_prompts(state.meta.reader(), access.project.id, &filter).await?;
    let items: Vec<(String, String, Prompt)> = rows
        .iter()
        .map(|row| {
            (
                row.name.to_lowercase(),
                row.id.to_string(),
                prompt_json(row),
            )
        })
        .collect();
    Ok(error::Json(page_named(
        items,
        params.cursor.as_deref(),
        params.limit.get(),
    )?))
}

/// The create's write: the protected check, then the prompt with its first version.
async fn create_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    prompt: NewPrompt,
    version: NewVersion,
    may: bool,
    now: i64,
) -> Result<Written<(PromptRow, VersionRow)>, MetaError> {
    if refused_by_protection(conn, project, &version.labels, may).await? {
        return Ok(Written::Protected);
    }
    let created = store::create_prompt(conn, project, prompt, version, now).await?;
    Ok(Written::Done(created))
}

/// `POST …/prompts`: creates the prompt and its version 1.
async fn create_prompt(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Json(body): error::Json<CreatePromptRequest>,
) -> Result<(StatusCode, error::Json<PromptVersion>), ApiError> {
    let kind = body.kind;
    let prompt_type = prompt_type_of(body.kind);
    let mut errors = Vec::new();
    if let Err(error) = check_name(&body.name) {
        errors.push(error);
    }
    let description = match body.description.as_deref() {
        None => None,
        Some(text) => match check_description(text) {
            Ok(description) => description,
            Err(error) => {
                errors.push(error);
                None
            }
        },
    };
    let tags = match check_tags(&body.tags) {
        Ok(tags) => tags,
        Err(found) => {
            errors.extend(found);
            Vec::new()
        }
    };
    let version = match new_version(
        prompt_type,
        &body.template,
        body.config.as_ref(),
        &body.labels,
        body.commit_message.as_deref(),
        created_by(&principal),
    ) {
        Ok(version) => Some(version),
        Err(found) => {
            errors.extend(found);
            None
        }
    };
    let (Some(version), true) = (version, errors.is_empty()) else {
        return Err(bad_request(errors));
    };
    let project = access.project.id;
    let now = state.clock.now_ns();
    let may = may_touch_protected(&principal, &access);
    let new = NewPrompt {
        id: Uuid::now_v7(),
        name: body.name,
        prompt_type,
        description,
        tags,
    };
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move { create_tx(conn, project, new, version, may, now).await })
        })
        .await?;
    let (prompt, version) = settle(outcome)?;
    let actor = created_by(&principal);
    tracing::info!(
        target: "audit",
        prompt_id = %prompt.id,
        project_id = %project,
        version = version.version,
        actor = %actor.as_deref().unwrap_or("-"),
        "prompt created"
    );
    record(
        &state,
        &Event::PromptCreated { kind },
        &Actor::project(&principal, &access),
    );
    Ok((
        StatusCode::CREATED,
        error::Json(version_json(&prompt, &version)?),
    ))
}

/// `GET …/prompts/{prompt}`.
async fn get_prompt(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(PromptPath { prompt }): error::Path<PromptPath>,
) -> Result<error::Json<Prompt>, ApiError> {
    let row = find(&state, access.project.id, &prompt).await?;
    Ok(error::Json(prompt_json(&row)))
}

/// `PATCH …/prompts/{prompt}`: the description and the tags. The name, the type and the
/// versions never change.
async fn update_prompt(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(PromptPath { prompt }): error::Path<PromptPath>,
    error::Json(body): error::Json<UpdatePromptRequest>,
) -> Result<error::Json<Prompt>, ApiError> {
    let mut errors = Vec::new();
    let description = match body.description {
        None => None,
        Some(None) => Some(None),
        Some(Some(text)) => match check_description(&text) {
            Ok(description) => Some(description),
            Err(error) => {
                errors.push(error);
                None
            }
        },
    };
    let tags = match body.tags {
        None => None,
        Some(None) => {
            errors.push(field_error("tags", "must be an array"));
            None
        }
        Some(Some(tags)) => match check_tags(&tags) {
            Ok(tags) => Some(tags),
            Err(found) => {
                errors.extend(found);
                None
            }
        },
    };
    if !errors.is_empty() {
        return Err(bad_request(errors));
    }
    let project = access.project.id;
    let now = state.clock.now_ns();
    let name = prompt;
    let row = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                store::update_prompt(conn, project, &name, description, tags, now).await
            })
        })
        .await?
        .ok_or(ApiError::NotFound)?;
    let actor = created_by(&principal);
    tracing::info!(
        target: "audit",
        prompt_id = %row.id,
        project_id = %project,
        actor = %actor.as_deref().unwrap_or("-"),
        "prompt updated"
    );
    Ok(error::Json(prompt_json(&row)))
}

/// The delete's write: the protected check over the prompt's labels, then the delete.
async fn delete_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    name: &str,
    may: bool,
) -> Result<Written<Uuid>, MetaError> {
    let Some(prompt) = store::get_prompt_tx(conn, project, name).await? else {
        return Ok(Written::Missing);
    };
    let labels: Vec<String> = prompt
        .labels
        .iter()
        .map(|label| label.label.clone())
        .collect();
    if refused_by_protection(conn, project, &labels, may).await? {
        return Ok(Written::Protected);
    }
    if !store::delete_prompt(conn, project, prompt.id).await? {
        return Ok(Written::Missing);
    }
    Ok(Written::Done(prompt.id))
}

/// `DELETE …/prompts/{prompt}`: removes the prompt with its versions, labels and history.
async fn delete_prompt(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(PromptPath { prompt }): error::Path<PromptPath>,
) -> Result<StatusCode, ApiError> {
    let project = access.project.id;
    let may = may_touch_protected(&principal, &access);
    let name = prompt;
    let outcome = state
        .meta
        .write(move |conn| Box::pin(async move { delete_tx(conn, project, &name, may).await }))
        .await?;
    let id = settle(outcome)?;
    let actor = created_by(&principal);
    tracing::info!(
        target: "audit",
        prompt_id = %id,
        project_id = %project,
        actor = %actor.as_deref().unwrap_or("-"),
        "prompt deleted"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `GET …/prompts/{prompt}/resolve` (D-V-9, D-V-12): the version a label, a number or `latest`
/// points at. The answer carries a strong ETag; a matching `If-None-Match` gets `304`.
async fn resolve_prompt(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(PromptPath { prompt }): error::Path<PromptPath>,
    error::Query(params): error::Query<ResolveParams>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if params.label.is_some() && params.version.is_some() {
        return Err(bad_request(vec![field_error(
            "version",
            "give label or version, not both",
        )]));
    }
    let label = params.label.as_deref();
    if let Some(label) = label
        && label != LATEST
    {
        check_label("label", label).map_err(|error| bad_request(vec![error]))?;
    }
    let row = find(&state, access.project.id, &prompt).await?;
    let reader = state.meta.reader();
    let found = match (params.version, label) {
        (Some(version), _) => store::get_version(reader, row.id, version).await?,
        (None, Some(LATEST)) => store::get_version(reader, row.id, row.latest_version).await?,
        (None, label) => {
            store::resolve_label(reader, row.id, label.unwrap_or(DEFAULT_LABEL)).await?
        }
    };
    let version = found.ok_or(ApiError::NotFound)?;
    let body = version_json(&row, &version)?;
    let bytes = serde_json::to_vec(&body).map_err(|error| ApiError::Internal(Box::new(error)))?;
    let etag = format!("\"{}\"", hex::encode(&Sha256::digest(&bytes)[..16]));
    let matches = headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|tag| tag.trim().trim_start_matches("W/"))
        .any(|tag| tag == etag || tag == "*");
    if matches {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag),
                (header::CACHE_CONTROL, "no-cache".to_owned()),
            ],
        )
            .into_response());
    }
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/json".to_owned()),
            (header::ETAG, etag),
            (header::CACHE_CONTROL, "no-cache".to_owned()),
        ],
        bytes,
    )
        .into_response())
}

/// A page cursor: a positive decimal number (a version or a label event id), or none.
fn positive_cursor(cursor: Option<&str>) -> Result<Option<i64>, ApiError> {
    match cursor {
        None => Ok(None),
        Some(text) => match text.parse::<i64>() {
            Ok(value) if value > 0 => Ok(Some(value)),
            _ => Err(bad_request(vec![field_error("cursor", "invalid cursor")])),
        },
    }
}

/// `GET …/prompts/{prompt}/versions`: newest first; the cursor is the last version of a page.
async fn list_versions(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(PromptPath { prompt }): error::Path<PromptPath>,
    error::Query(params): error::Query<PageParams>,
) -> Result<error::Json<Page<PromptVersion>>, ApiError> {
    let row = find(&state, access.project.id, &prompt).await?;
    let before = positive_cursor(params.cursor.as_deref())?;
    let limit = params.limit.get().min(MAX_VERSION_PAGE);
    let keep = usize::try_from(limit).unwrap_or(usize::MAX);
    let mut rows = store::list_versions(state.meta.reader(), row.id, before, limit).await?;
    let more = rows.len() > keep;
    rows.truncate(keep);
    let next_cursor = if more {
        rows.last().map(|version| version.version.to_string())
    } else {
        None
    };
    let data = rows
        .iter()
        .map(|version| version_json(&row, version))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(error::Json(Page { data, next_cursor }))
}

/// The version create's write: the prompt re-read, the type check against `validated_for` (the
/// type the template was checked for), the protected check, then the new version and its labels.
async fn add_version_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    name: &str,
    validated_for: PromptType,
    version: NewVersion,
    may: bool,
    now: i64,
) -> Result<Written<(PromptRow, VersionRow)>, MetaError> {
    let Some(prompt) = store::get_prompt_tx(conn, project, name).await? else {
        return Ok(Written::Missing);
    };
    if prompt.prompt_type != validated_for {
        return Err(MetaError::Conflict(store::PROMPT_CHANGED));
    }
    if refused_by_protection(conn, project, &version.labels, may).await? {
        return Ok(Written::Protected);
    }
    let added = store::add_version(conn, prompt.id, version, now).await?;
    let prompt = store::get_prompt_tx(conn, project, name)
        .await?
        .ok_or(MetaError::Corrupt {
            table: "prompts",
            column: "id",
        })?;
    Ok(Written::Done((prompt, added)))
}

/// `POST …/prompts/{prompt}/versions` (D-V-7): appends the next version.
async fn create_version(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(PromptPath { prompt }): error::Path<PromptPath>,
    error::Json(body): error::Json<CreateVersionRequest>,
) -> Result<(StatusCode, error::Json<PromptVersion>), ApiError> {
    let existing = find(&state, access.project.id, &prompt).await?;
    let validated_for = existing.prompt_type;
    let version = new_version(
        validated_for,
        &body.template,
        body.config.as_ref(),
        &body.labels,
        body.commit_message.as_deref(),
        created_by(&principal),
    )
    .map_err(bad_request)?;
    let project = access.project.id;
    let now = state.clock.now_ns();
    let may = may_touch_protected(&principal, &access);
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                add_version_tx(conn, project, &prompt, validated_for, version, may, now).await
            })
        })
        .await?;
    let (prompt, version) = settle(outcome)?;
    let actor = created_by(&principal);
    tracing::info!(
        target: "audit",
        prompt_id = %prompt.id,
        project_id = %project,
        version = version.version,
        actor = %actor.as_deref().unwrap_or("-"),
        "prompt version created"
    );
    record(
        &state,
        &Event::PromptVersionCreated,
        &Actor::project(&principal, &access),
    );
    Ok((
        StatusCode::CREATED,
        error::Json(version_json(&prompt, &version)?),
    ))
}

/// `GET …/prompts/{prompt}/versions/{version}`.
async fn get_version(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(PromptVersionPath { prompt, version }): error::Path<PromptVersionPath>,
) -> Result<error::Json<PromptVersion>, ApiError> {
    let row = find(&state, access.project.id, &prompt).await?;
    let found = store::get_version(state.meta.reader(), row.id, version)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(error::Json(version_json(&row, &found)?))
}

/// The label move's write (D-V-8, D-V-10): the prompt, the protected check for the label, then
/// the move. A version that does not exist is `Missing`.
async fn set_label_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    name: &str,
    target: (&str, i64),
    actor: Option<&str>,
    may: bool,
    now: i64,
) -> Result<Written<PromptRow>, MetaError> {
    let (label, version) = target;
    let Some(prompt) = store::get_prompt_tx(conn, project, name).await? else {
        return Ok(Written::Missing);
    };
    if refused_by_protection(conn, project, &[label.to_owned()], may).await? {
        return Ok(Written::Protected);
    }
    if store::set_label(conn, prompt.id, label, version, actor, now)
        .await?
        .is_none()
    {
        return Ok(Written::Missing);
    }
    let prompt = store::get_prompt_tx(conn, project, name)
        .await?
        .ok_or(MetaError::Corrupt {
            table: "prompts",
            column: "id",
        })?;
    Ok(Written::Done(prompt))
}

/// `PUT …/prompts/{prompt}/labels/{label}` (D-V-8): points the label at a version, creating the
/// label when it is new. Answers the prompt.
async fn set_label(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(PromptLabelPath { prompt, label }): error::Path<PromptLabelPath>,
    error::Json(body): error::Json<SetLabelRequest>,
) -> Result<error::Json<Prompt>, ApiError> {
    check_label("label", &label).map_err(|error| bad_request(vec![error]))?;
    let version = body.version;
    if version < 1 {
        return Err(bad_request(vec![field_error(
            "version",
            "must be a positive version",
        )]));
    }
    let project = access.project.id;
    let now = state.clock.now_ns();
    let may = may_touch_protected(&principal, &access);
    let actor = created_by(&principal);
    let name = prompt;
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                set_label_tx(
                    conn,
                    project,
                    &name,
                    (&label, version),
                    actor.as_deref(),
                    may,
                    now,
                )
                .await
            })
        })
        .await?;
    let prompt = settle(outcome)?;
    let actor = created_by(&principal);
    tracing::info!(
        target: "audit",
        prompt_id = %prompt.id,
        project_id = %project,
        version,
        actor = %actor.as_deref().unwrap_or("-"),
        "prompt label set"
    );
    Ok(error::Json(prompt_json(&prompt)))
}

/// The label removal's write (D-V-10): the prompt, the protected check, then the removal. A
/// label that is not set is `Missing`.
async fn remove_label_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    name: &str,
    label: &str,
    actor: Option<&str>,
    may: bool,
    now: i64,
) -> Result<Written<Uuid>, MetaError> {
    let Some(prompt) = store::get_prompt_tx(conn, project, name).await? else {
        return Ok(Written::Missing);
    };
    if refused_by_protection(conn, project, &[label.to_owned()], may).await? {
        return Ok(Written::Protected);
    }
    if !store::remove_label(conn, prompt.id, label, actor, now).await? {
        return Ok(Written::Missing);
    }
    Ok(Written::Done(prompt.id))
}

/// `DELETE …/prompts/{prompt}/labels/{label}` (D-V-8): removes the label; the versions stay.
async fn remove_label(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(PromptLabelPath { prompt, label }): error::Path<PromptLabelPath>,
) -> Result<StatusCode, ApiError> {
    check_label("label", &label).map_err(|error| bad_request(vec![error]))?;
    let project = access.project.id;
    let now = state.clock.now_ns();
    let may = may_touch_protected(&principal, &access);
    let actor = created_by(&principal);
    let name = prompt;
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                remove_label_tx(conn, project, &name, &label, actor.as_deref(), may, now).await
            })
        })
        .await?;
    let id = settle(outcome)?;
    let actor = created_by(&principal);
    tracing::info!(
        target: "audit",
        prompt_id = %id,
        project_id = %project,
        actor = %actor.as_deref().unwrap_or("-"),
        "prompt label removed"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// `GET …/prompts/{prompt}/label-events` (D-V-8): the label history, newest first; the cursor is
/// the id of the last event of a page.
async fn list_label_events(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(PromptPath { prompt }): error::Path<PromptPath>,
    error::Query(params): error::Query<PageParams>,
) -> Result<error::Json<Page<PromptLabelEvent>>, ApiError> {
    let row = find(&state, access.project.id, &prompt).await?;
    let before = positive_cursor(params.cursor.as_deref())?;
    let limit = params.limit.get();
    let keep = usize::try_from(limit).unwrap_or(usize::MAX);
    let mut rows = store::list_label_events(state.meta.reader(), row.id, before, limit).await?;
    let more = rows.len() > keep;
    rows.truncate(keep);
    let next_cursor = if more {
        rows.last().map(|event| event.id.to_string())
    } else {
        None
    };
    let data = rows
        .into_iter()
        .map(|event| PromptLabelEvent {
            label: event.label,
            from_version: event.from_version,
            to_version: event.to_version,
            actor: event.actor,
            at: event.at,
        })
        .collect();
    Ok(error::Json(Page { data, next_cursor }))
}

/// `GET /prompt-settings` (D-V-10): the project's protected labels, `["production"]` until set.
async fn get_settings(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
) -> Result<error::Json<PromptSettings>, ApiError> {
    let protected_labels = store::protected_labels(state.meta.reader(), access.project.id).await?;
    Ok(error::Json(PromptSettings { protected_labels }))
}

/// `PUT /prompt-settings` (D-V-10): replaces the protected labels. Project admins only, since
/// the route needs `ManageSettings`.
async fn put_settings(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Json(body): error::Json<PromptSettings>,
) -> Result<error::Json<PromptSettings>, ApiError> {
    let labels = check_labels("protected_labels", &body.protected_labels, MAX_PROTECTED)
        .map_err(bad_request)?;
    let project = access.project.id;
    let now = state.clock.now_ns();
    let actor = created_by(&principal);
    let stored = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                store::set_protected_labels(conn, project, &labels, actor.as_deref(), now).await?;
                store::protected_labels_tx(conn, project).await
            })
        })
        .await?;
    let actor = created_by(&principal);
    tracing::info!(
        target: "audit",
        project_id = %project,
        labels = stored.len(),
        actor = %actor.as_deref().unwrap_or("-"),
        "prompt settings updated"
    );
    Ok(error::Json(PromptSettings {
        protected_labels: stored,
    }))
}

/// `versions`: comma-separated version numbers, each at least 1, distinct, at most
/// [`cetra_storage::MAX_PROMPT_VERSIONS`] of them. Absent: no filter (D-V-17).
fn parse_versions(text: Option<&str>) -> Result<Vec<i64>, FieldError> {
    let Some(text) = text else {
        return Ok(Vec::new());
    };
    let mut versions: Vec<i64> = Vec::new();
    for part in text.split(',') {
        if part.trim().is_empty() {
            return Err(field_error("versions", "must name 1 to 50 versions"));
        }
        match part.trim().parse::<i64>() {
            Ok(version) if version >= 1 => {
                if !versions.contains(&version) {
                    versions.push(version);
                }
            }
            _ => {
                return Err(field_error(
                    "versions",
                    "must be comma-separated positive versions",
                ));
            }
        }
    }
    if versions.len() > cetra_storage::MAX_PROMPT_VERSIONS {
        return Err(field_error("versions", "must name 1 to 50 versions"));
    }
    Ok(versions)
}

/// Nanoseconds as milliseconds.
fn nanos_to_millis(nanos: i64) -> f64 {
    nanos as f64 / 1e6
}

fn version_metrics(stored: StoredVersionMetrics) -> PromptVersionMetrics {
    PromptVersionMetrics {
        version: stored.version,
        span_count: stored.span_count,
        trace_count: stored.trace_count,
        error_count: stored.error_count,
        cost_total: stored.cost_total,
        cost_avg: stored.cost_avg,
        latency_p50_ms: stored.latency_p50_ns.map(nanos_to_millis),
        latency_p95_ms: stored.latency_p95_ns.map(nanos_to_millis),
        input_tokens_avg: stored.input_tokens_avg,
        output_tokens_avg: stored.output_tokens_avg,
        scores: stored
            .scores
            .into_iter()
            .map(|score| PromptScoreMean {
                name: score.name,
                mean: score.mean,
                count: score.count,
            })
            .collect(),
    }
}

/// `GET …/prompts/{prompt}/metrics` (D-V-17): per version, the counts, costs, latencies, token
/// averages and score means of the prompt's linked `llm` spans in the window.
async fn prompt_metrics(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(PromptPath { prompt }): error::Path<PromptPath>,
    error::Query(params): error::Query<PromptMetricsParams>,
) -> Result<error::Json<PromptMetrics>, ApiError> {
    let row = find(&state, access.project.id, &prompt).await?;
    let to = params
        .to
        .map_or_else(|| state.clock.now_ns(), TimeParam::ns);
    let from = params.from.map_or_else(
        || to.saturating_sub(METRICS_DEFAULT_WINDOW_NS),
        TimeParam::ns,
    );
    let mut errors = Vec::new();
    if from >= to {
        errors.push(field_error("from", "must be before to"));
    } else if to.saturating_sub(from) > METRICS_MAX_WINDOW_NS {
        errors.push(field_error("from", "the window is at most 90 days"));
    }
    let versions = match parse_versions(params.versions.as_deref()) {
        Ok(versions) => versions,
        Err(error) => {
            errors.push(error);
            Vec::new()
        }
    };
    if !errors.is_empty() {
        return Err(bad_request(errors));
    }
    let deadline = Deadline::start();
    let ctx = open_query(&state, access.project.org_id, access.project.id, None).await?;
    let stored = cetra_storage::prompt_metrics(
        &ctx,
        cetra_storage::PromptMetricsRequest {
            prompt: &row.name,
            window: cetra_storage::TimeWindow::new(from, to)?,
            versions: &versions,
            timeout: deadline.remaining()?,
        },
    )
    .await?;
    Ok(error::Json(PromptMetrics {
        from,
        to,
        versions: stored.into_iter().map(version_metrics).collect(),
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::auth::roles::OrgRole;
    use crate::test_support::{Seeded, TestApp, TestResponse, capture_logs};

    fn base(seeded: &Seeded) -> String {
        format!("/api/v1/projects/{}/prompts", seeded.project.id)
    }

    /// One request. `auth` is a header name and value: `cookie` or `authorization`.
    async fn call(
        app: &TestApp,
        method: Method,
        path: &str,
        auth: (&str, &str),
        body: Option<Value>,
    ) -> TestResponse {
        let request = app.request(method, path).header(auth.0, auth.1);
        match body {
            Some(body) => request.json(&body).send().await,
            None => request.send().await,
        }
    }

    fn bearer(key: &str) -> String {
        format!("Bearer {key}")
    }

    fn support_reply_body(labels: &[&str]) -> Value {
        json!({
            "name": "support-reply",
            "type": "chat",
            "template": [
                {"role": "system", "content": "You are a support agent for {{ product }}."},
                {"type": "placeholder", "name": "history"},
                {"role": "user", "content": "{{ question }}"}
            ],
            "config": {"model": "gpt-4o", "params": {"temperature": 0.3, "max_tokens": 512}},
            "labels": labels,
            "commit_message": "First version"
        })
    }

    fn text_body(name: &str, labels: &[&str]) -> Value {
        json!({
            "name": name,
            "type": "text",
            "template": "Hello {{ name }}",
            "labels": labels
        })
    }

    fn friendlier_version(labels: &[&str]) -> Value {
        json!({
            "template": [
                {"role": "system", "content": "You are a friendly support agent for {{ product }}. Answer in {{ language }}."},
                {"type": "placeholder", "name": "history"},
                {"role": "user", "content": "{{ question }}"}
            ],
            "config": {"model": "gpt-4o", "params": {"temperature": 0.3, "max_tokens": 512}},
            "labels": labels,
            "commit_message": "Friendlier tone"
        })
    }

    fn field_names(response: &TestResponse) -> Vec<String> {
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let body: Value = response.json();
        body["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|error| error["field"].as_str().unwrap().to_owned())
            .collect()
    }

    /// The number of prompts on the page at `path`.
    async fn page_len(app: &TestApp, auth: (&str, &str), path: &str) -> usize {
        let page: Page<Prompt> = call(app, Method::GET, path, auth, None).await.json();
        page.data.len()
    }

    /// The version number `path` (a resolve) answers with.
    async fn resolved_number(app: &TestApp, auth: (&str, &str), path: &str) -> i64 {
        let response = call(app, Method::GET, path, auth, None).await;
        assert_eq!(response.status, StatusCode::OK, "{path}");
        response.json::<PromptVersion>().version
    }

    /// A resolve with `If-None-Match: tag`.
    async fn revalidate(app: &TestApp, cookie: &str, target: &str, tag: &str) -> TestResponse {
        app.request(Method::GET, target)
            .header("cookie", cookie)
            .header("if-none-match", tag)
            .send()
            .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_get_and_list() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let auth = ("cookie", seeded.cookie.as_str());
        let created = call(
            &app,
            Method::POST,
            &base(&seeded),
            auth,
            Some(support_reply_body(&["production"])),
        )
        .await;
        assert_eq!(
            created.status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&created.body)
        );
        let version: PromptVersion = created.json();
        assert_eq!(version.version, 1);
        assert_eq!(version.labels, vec!["latest", "production"]);
        assert_eq!(version.variables, vec!["product", "question"]);
        assert_eq!(version.placeholders, vec!["history"]);

        let path = format!("{}/support-reply", base(&seeded));
        let prompt: Prompt = call(&app, Method::GET, &path, auth, None).await.json();
        assert_eq!(prompt.latest_version, 1);
        let labels: Vec<(&str, i64)> = prompt
            .labels
            .iter()
            .map(|label| (label.label.as_str(), label.version))
            .collect();
        assert_eq!(labels, vec![("production", 1)]);

        let list = base(&seeded);
        assert_eq!(page_len(&app, auth, &list).await, 1);
        assert_eq!(page_len(&app, auth, &format!("{list}?q=SUPPORT")).await, 1);
        assert_eq!(
            page_len(&app, auth, &format!("{list}?label=staging")).await,
            0
        );
        assert_eq!(
            page_len(&app, auth, &format!("{list}?label=latest")).await,
            1
        );
        assert!(
            app.state.analytics.counters.get("prompt.created") >= 1,
            "prompt.created not counted"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn versions_and_resolve() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let auth = ("cookie", seeded.cookie.as_str());
        call(
            &app,
            Method::POST,
            &base(&seeded),
            auth,
            Some(support_reply_body(&["production"])),
        )
        .await;
        let path = format!("{}/support-reply", base(&seeded));
        let second = call(
            &app,
            Method::POST,
            &format!("{path}/versions"),
            auth,
            Some(friendlier_version(&["staging"])),
        )
        .await;
        assert_eq!(second.status, StatusCode::CREATED);
        let second: PromptVersion = second.json();
        assert_eq!(second.version, 2);

        let resolve = |query: &str| format!("{path}/resolve{query}");
        assert_eq!(resolved_number(&app, auth, &resolve("")).await, 1);
        assert_eq!(
            resolved_number(&app, auth, &resolve("?label=staging")).await,
            2
        );
        assert_eq!(
            resolved_number(&app, auth, &resolve("?label=latest")).await,
            2
        );
        assert_eq!(resolved_number(&app, auth, &resolve("?version=1")).await, 1);

        for (query, status) in [
            ("?version=9", StatusCode::NOT_FOUND),
            ("?label=nope", StatusCode::NOT_FOUND),
        ] {
            let response = call(&app, Method::GET, &resolve(query), auth, None).await;
            assert_eq!(response.status, status, "{query}");
        }
        let both = call(
            &app,
            Method::GET,
            &resolve("?label=staging&version=1"),
            auth,
            None,
        )
        .await;
        assert_eq!(field_names(&both), vec!["version"]);
        let bad_label = call(&app, Method::GET, &resolve("?label=BAD"), auth, None).await;
        assert_eq!(field_names(&bad_label), vec!["label"]);

        let versions: Page<PromptVersion> =
            call(&app, Method::GET, &format!("{path}/versions"), auth, None)
                .await
                .json();
        let numbers: Vec<i64> = versions
            .data
            .iter()
            .map(|version| version.version)
            .collect();
        assert_eq!(numbers, vec![2, 1]);

        let first_page: Page<PromptVersion> = call(
            &app,
            Method::GET,
            &format!("{path}/versions?limit=1"),
            auth,
            None,
        )
        .await
        .json();
        assert_eq!(first_page.data.len(), 1);
        assert_eq!(first_page.data[0].version, 2);
        let cursor = first_page.next_cursor.expect("a next page");
        let rest: Page<PromptVersion> = call(
            &app,
            Method::GET,
            &format!("{path}/versions?limit=1&cursor={cursor}"),
            auth,
            None,
        )
        .await
        .json();
        assert_eq!(rest.data.len(), 1);
        assert_eq!(rest.data[0].version, 1);
        assert_eq!(rest.next_cursor, None);

        let one = call(&app, Method::GET, &format!("{path}/versions/2"), auth, None).await;
        assert_eq!(one.json::<PromptVersion>().version, 2);
        assert!(
            app.state.analytics.counters.get("prompt.version_created") >= 1,
            "prompt.version_created not counted"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resolve_etag() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let auth = ("cookie", seeded.cookie.as_str());
        call(
            &app,
            Method::POST,
            &base(&seeded),
            auth,
            Some(support_reply_body(&["production"])),
        )
        .await;
        let target = format!("{}/support-reply/resolve", base(&seeded));
        let first = call(&app, Method::GET, &target, auth, None).await;
        assert_eq!(first.status, StatusCode::OK);
        assert_eq!(first.header("cache-control"), Some("no-cache"));
        let etag = first.header("etag").expect("an etag").to_owned();
        assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");

        let cookie = seeded.cookie.as_str();
        let not_modified = revalidate(&app, cookie, &target, &etag).await;
        assert_eq!(not_modified.status, StatusCode::NOT_MODIFIED);
        assert!(not_modified.body.is_empty());
        let listed = revalidate(&app, cookie, &target, &format!("\"other\", {etag}")).await;
        assert_eq!(listed.status, StatusCode::NOT_MODIFIED);
        let other = revalidate(&app, cookie, &target, "\"other\"").await;
        assert_eq!(other.status, StatusCode::OK);

        let moved = call(
            &app,
            Method::POST,
            &format!("{}/support-reply/versions", base(&seeded)),
            auth,
            Some(friendlier_version(&["production"])),
        )
        .await;
        assert_eq!(moved.status, StatusCode::CREATED);
        let stale = revalidate(&app, cookie, &target, &etag).await;
        assert_eq!(stale.status, StatusCode::OK);
        assert_ne!(stale.header("etag"), Some(etag.as_str()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn validation_errors() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let auth = ("cookie", seeded.cookie.as_str());
        let bad = call(
            &app,
            Method::POST,
            &base(&seeded),
            auth,
            Some(json!({
                "name": "a b",
                "type": "text",
                "template": "{% if x %}",
                "config": {"params": {"temperature": 5}},
                "labels": ["latest"]
            })),
        )
        .await;
        let fields = field_names(&bad);
        for expected in ["name", "template", "config.params.temperature", "labels[0]"] {
            assert!(fields.iter().any(|field| field == expected), "{expected}");
        }
        let body: Value = bad.json();
        let template = body["errors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|error| error["field"] == "template")
            .unwrap();
        assert!(template["position"].is_object(), "{template}");

        let wrong_shape = call(
            &app,
            Method::POST,
            &base(&seeded),
            auth,
            Some(json!({"name": "shape", "type": "text", "template": []})),
        )
        .await;
        let fields = field_names(&wrong_shape);
        assert!(!fields.is_empty() && fields.iter().all(|field| field == "template"));

        let created = call(
            &app,
            Method::POST,
            &base(&seeded),
            auth,
            Some(text_body("summarize", &[])),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED);
        let duplicate = call(
            &app,
            Method::POST,
            &base(&seeded),
            auth,
            Some(text_body("summarize", &[])),
        )
        .await;
        assert_eq!(duplicate.status, StatusCode::CONFLICT);

        let mismatch = call(
            &app,
            Method::POST,
            &format!("{}/summarize/versions", base(&seeded)),
            auth,
            Some(json!({"template": [{"role": "user", "content": "hi"}]})),
        )
        .await;
        let fields = field_names(&mismatch);
        assert!(!fields.is_empty() && fields.iter().all(|field| field == "template"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn permissions() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let member = app
            .add_member(&seeded, "member@example.com", OrgRole::Member)
            .await;
        let viewer_cookie = viewer.cookie.clone();
        let member_cookie = member.cookie.clone();
        let viewer_auth = ("cookie", viewer_cookie.as_str());
        let member_auth = ("cookie", member_cookie.as_str());
        let read_key = bearer(&seeded.read_key);
        let ingest_key = bearer(&seeded.ingest_key);
        let write_key = bearer(&app.write_key(&seeded).await);
        let read_only_pat = bearer(&app.personal_token(seeded.user_id, false).await);
        let read_write_pat = bearer(&app.personal_token(member.id, true).await);
        let list = base(&seeded);
        let created = call(
            &app,
            Method::POST,
            &list,
            ("cookie", seeded.cookie.as_str()),
            Some(text_body("res", &[])),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED);
        let resolve = format!("{}/res/resolve?label=latest", base(&seeded));

        let viewer_list = call(&app, Method::GET, &list, viewer_auth, None).await;
        assert_eq!(viewer_list.status, StatusCode::OK);
        let viewer_post = call(
            &app,
            Method::POST,
            &list,
            viewer_auth,
            Some(text_body("v", &[])),
        )
        .await;
        assert_eq!(viewer_post.status, StatusCode::FORBIDDEN);

        let member_post = call(
            &app,
            Method::POST,
            &list,
            member_auth,
            Some(text_body("m", &[])),
        )
        .await;
        assert_eq!(member_post.status, StatusCode::CREATED);

        let read_get = call(
            &app,
            Method::GET,
            &resolve,
            ("authorization", read_key.as_str()),
            None,
        )
        .await;
        assert_eq!(read_get.status, StatusCode::OK);
        let read_post = call(
            &app,
            Method::POST,
            &list,
            ("authorization", read_key.as_str()),
            Some(text_body("r", &[])),
        )
        .await;
        assert_eq!(read_post.status, StatusCode::FORBIDDEN);
        let ingest_get = call(
            &app,
            Method::GET,
            &resolve,
            ("authorization", ingest_key.as_str()),
            None,
        )
        .await;
        assert_eq!(ingest_get.status, StatusCode::FORBIDDEN);

        let key_post = call(
            &app,
            Method::POST,
            &list,
            ("authorization", write_key.as_str()),
            Some(text_body("by-key", &[])),
        )
        .await;
        assert_eq!(key_post.status, StatusCode::CREATED);
        let pat_read = call(
            &app,
            Method::POST,
            &list,
            ("authorization", read_only_pat.as_str()),
            Some(text_body("pat-read", &[])),
        )
        .await;
        assert_eq!(pat_read.status, StatusCode::FORBIDDEN);
        let pat_write = call(
            &app,
            Method::POST,
            &list,
            ("authorization", read_write_pat.as_str()),
            Some(text_body("pat-write", &[])),
        )
        .await;
        assert_eq!(pat_write.status, StatusCode::CREATED);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn protected_labels() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let member = app
            .add_member(&seeded, "member@example.com", OrgRole::Member)
            .await;
        let member_cookie = member.cookie.clone();
        let member_auth = ("cookie", member_cookie.as_str());
        let admin_auth = ("cookie", seeded.cookie.as_str());
        let write_key = bearer(&app.write_key(&seeded).await);
        let list = base(&seeded);
        let pl = format!("{list}/pl");

        let refused = call(
            &app,
            Method::POST,
            &list,
            member_auth,
            Some(support_reply_body(&["production"])),
        )
        .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        let missing = call(
            &app,
            Method::GET,
            &format!("{list}/support-reply"),
            admin_auth,
            None,
        )
        .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);

        let created = call(
            &app,
            Method::POST,
            &list,
            member_auth,
            Some(text_body("pl", &[])),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED);
        let version_body = |labels: &[&str]| json!({"template": "Hi {{ name }}", "labels": labels});
        let protected_version = call(
            &app,
            Method::POST,
            &format!("{pl}/versions"),
            member_auth,
            Some(version_body(&["production"])),
        )
        .await;
        assert_eq!(protected_version.status, StatusCode::FORBIDDEN);
        let prompt: Prompt = call(&app, Method::GET, &pl, admin_auth, None).await.json();
        assert_eq!(prompt.latest_version, 1);

        let staging = call(
            &app,
            Method::POST,
            &format!("{pl}/versions"),
            member_auth,
            Some(version_body(&["staging"])),
        )
        .await;
        assert_eq!(staging.status, StatusCode::CREATED);
        let by_key = call(
            &app,
            Method::POST,
            &format!("{pl}/versions"),
            ("authorization", write_key.as_str()),
            Some(version_body(&["production"])),
        )
        .await;
        assert_eq!(by_key.status, StatusCode::CREATED);
        let member_delete = call(&app, Method::DELETE, &pl, member_auth, None).await;
        assert_eq!(member_delete.status, StatusCode::FORBIDDEN);

        let small = call(
            &app,
            Method::POST,
            &list,
            member_auth,
            Some(text_body("s", &["staging"])),
        )
        .await;
        assert_eq!(small.status, StatusCode::CREATED);
        let small_delete = call(
            &app,
            Method::DELETE,
            &format!("{list}/s"),
            member_auth,
            None,
        )
        .await;
        assert_eq!(small_delete.status, StatusCode::NO_CONTENT);

        let admin_version = call(
            &app,
            Method::POST,
            &format!("{pl}/versions"),
            admin_auth,
            Some(version_body(&["production"])),
        )
        .await;
        assert_eq!(admin_version.status, StatusCode::CREATED);

        let key_delete = call(
            &app,
            Method::DELETE,
            &pl,
            ("authorization", write_key.as_str()),
            None,
        )
        .await;
        assert_eq!(key_delete.status, StatusCode::NO_CONTENT);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patch_refuses_null_tags() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let auth = ("cookie", seeded.cookie.as_str());
        let list = base(&seeded);
        let created = call(
            &app,
            Method::POST,
            &list,
            auth,
            Some(text_body("tagged", &[])),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED);
        let path = format!("{list}/tagged");

        let refused = call(
            &app,
            Method::PATCH,
            &path,
            auth,
            Some(json!({"tags": null})),
        )
        .await;
        assert_eq!(field_names(&refused), vec!["tags"]);
        let body: Value = refused.json();
        assert_eq!(body["errors"][0]["message"], "must be an array");
        let prompt: Prompt = call(&app, Method::GET, &path, auth, None).await.json();
        assert!(prompt.tags.is_empty());
    }

    /// A version write checks the type that the template was validated for against the prompt
    /// re-read in the transaction. A prompt deleted and created again under the same name can
    /// have another type; that write is a 409 (`PROMPT_CHANGED`) and writes nothing.
    #[tokio::test]
    async fn version_write_refuses_a_changed_type() {
        let (_dir, db) = crate::meta::test_db().await;
        let project = db
            .write(move |conn| {
                Box::pin(async move {
                    let org = crate::meta::orgs::insert_org(
                        conn,
                        cetra_core::ids::OrgId::new_v7(),
                        "Acme",
                        "acme",
                        1,
                    )
                    .await?;
                    let project = crate::meta::projects::insert_project(
                        conn, org.id, "Support", "support", None, 1,
                    )
                    .await?;
                    Ok::<_, MetaError>(project.id)
                })
            })
            .await
            .unwrap();
        let new_text_version = || NewVersion {
            template: "\"hello\"".to_owned(),
            config: "{}".to_owned(),
            commit_message: None,
            labels: Vec::new(),
            actor: None,
        };
        let id = db
            .write(move |conn| {
                Box::pin(async move {
                    let prompt = NewPrompt {
                        id: Uuid::now_v7(),
                        name: "p".to_owned(),
                        prompt_type: PromptType::Text,
                        description: None,
                        tags: Vec::new(),
                    };
                    let (row, _) =
                        store::create_prompt(conn, project, prompt, new_text_version(), 1).await?;
                    Ok::<_, MetaError>(row.id)
                })
            })
            .await
            .unwrap();

        let changed = db
            .write(move |conn| {
                Box::pin(async move {
                    add_version_tx(
                        conn,
                        project,
                        "p",
                        PromptType::Chat,
                        new_text_version(),
                        true,
                        2,
                    )
                    .await
                })
            })
            .await;
        assert!(matches!(
            changed,
            Err(MetaError::Conflict(detail)) if detail == store::PROMPT_CHANGED
        ));
        let stored = store::get_version(db.reader(), id, 2).await.unwrap();
        assert!(stored.is_none(), "a refused write adds no version");

        let same_type = db
            .write(move |conn| {
                Box::pin(async move {
                    add_version_tx(
                        conn,
                        project,
                        "p",
                        PromptType::Text,
                        new_text_version(),
                        true,
                        3,
                    )
                    .await
                })
            })
            .await;
        assert!(matches!(same_type, Ok(Written::Done(_))));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patch_and_delete() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let auth = ("cookie", seeded.cookie.as_str());
        let list = base(&seeded);
        let path = format!("{list}/pa");
        call(&app, Method::POST, &list, auth, Some(text_body("pa", &[]))).await;

        let set = call(
            &app,
            Method::PATCH,
            &path,
            auth,
            Some(json!({"description": "d"})),
        )
        .await;
        assert_eq!(set.json::<Prompt>().description.as_deref(), Some("d"));
        let cleared = call(
            &app,
            Method::PATCH,
            &path,
            auth,
            Some(json!({"description": null})),
        )
        .await;
        assert_eq!(cleared.json::<Prompt>().description, None);
        let tagged = call(
            &app,
            Method::PATCH,
            &path,
            auth,
            Some(json!({"tags": [" a ", "a", "b"]})),
        )
        .await;
        assert_eq!(tagged.json::<Prompt>().tags, vec!["a", "b"]);

        let deleted = call(&app, Method::DELETE, &path, auth, None).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
        let gone = call(&app, Method::GET, &path, auth, None).await;
        assert_eq!(gone.status, StatusCode::NOT_FOUND);
        let again = call(&app, Method::DELETE, &path, auth, None).await;
        assert_eq!(again.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn other_project_is_404() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let auth = ("cookie", seeded.cookie.as_str());
        call(
            &app,
            Method::POST,
            &base(&seeded),
            auth,
            Some(text_body("op", &[])),
        )
        .await;
        let (other, _key) = app.add_project(&seeded, "other", None).await;
        let other_path = format!("/api/v1/projects/{}/prompts/op", other.id);
        let through_other = call(&app, Method::GET, &other_path, auth, None).await;
        assert_eq!(through_other.status, StatusCode::NOT_FOUND);

        let write_key = bearer(&app.write_key(&seeded).await);
        let foreign = call(
            &app,
            Method::GET,
            &format!("/api/v1/projects/{}/prompts/op", other.id),
            ("authorization", write_key.as_str()),
            None,
        )
        .await;
        assert_eq!(foreign.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn audit_log_has_no_payload() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let write_key = bearer(&app.write_key(&seeded).await);
        let body = json!({
            "name": "canary-name-x",
            "type": "text",
            "template": "canary-template-y",
            "tags": ["canary-tag-z"],
            "commit_message": "canary-commit-w"
        });
        let (response, logs) = capture_logs(async {
            call(
                &app,
                Method::POST,
                &base(&seeded),
                ("authorization", write_key.as_str()),
                Some(body),
            )
            .await
        })
        .await;
        assert_eq!(response.status, StatusCode::CREATED);
        assert!(logs.contains("prompt created"), "{logs}");
        assert!(logs.contains("actor=key:"), "{logs}");
        for canary in [
            "canary-name-x",
            "canary-template-y",
            "canary-tag-z",
            "canary-commit-w",
        ] {
            assert!(!logs.contains(canary), "{canary} in the logs");
        }
    }

    /// The label `label` points at in a prompt's answer, if set.
    fn label_version(prompt: &Prompt, label: &str) -> Option<i64> {
        prompt
            .labels
            .iter()
            .find(|item| item.label == label)
            .map(|item| item.version)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn labels_move_and_history() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let auth = ("cookie", seeded.cookie.as_str());
        let list = base(&seeded);
        let lh = format!("{list}/lh");
        let created = call(
            &app,
            Method::POST,
            &list,
            auth,
            Some(text_body("lh", &["production"])),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED);
        let version_two = call(
            &app,
            Method::POST,
            &format!("{lh}/versions"),
            auth,
            Some(json!({"template": "Hi {{ name }}, again", "labels": []})),
        )
        .await;
        assert_eq!(version_two.status, StatusCode::CREATED);

        let staging = call(
            &app,
            Method::PUT,
            &format!("{lh}/labels/staging"),
            auth,
            Some(json!({"version": 2})),
        )
        .await;
        assert_eq!(staging.status, StatusCode::OK);
        let answer: Prompt = staging.json();
        assert_eq!(label_version(&answer, "staging"), Some(2));
        let production = call(
            &app,
            Method::PUT,
            &format!("{lh}/labels/production"),
            auth,
            Some(json!({"version": 2})),
        )
        .await;
        assert_eq!(production.status, StatusCode::OK);
        assert_eq!(label_version(&production.json(), "production"), Some(2));

        let removed = call(
            &app,
            Method::DELETE,
            &format!("{lh}/labels/staging"),
            auth,
            None,
        )
        .await;
        assert_eq!(removed.status, StatusCode::NO_CONTENT);
        let again = call(
            &app,
            Method::DELETE,
            &format!("{lh}/labels/staging"),
            auth,
            None,
        )
        .await;
        assert_eq!(again.status, StatusCode::NOT_FOUND);
        let missing_version = call(
            &app,
            Method::PUT,
            &format!("{lh}/labels/production"),
            auth,
            Some(json!({"version": 9})),
        )
        .await;
        assert_eq!(missing_version.status, StatusCode::NOT_FOUND);
        let computed = call(
            &app,
            Method::PUT,
            &format!("{lh}/labels/latest"),
            auth,
            Some(json!({"version": 2})),
        )
        .await;
        assert_eq!(field_names(&computed), vec!["label".to_owned()]);
        let zero = call(
            &app,
            Method::PUT,
            &format!("{lh}/labels/staging"),
            auth,
            Some(json!({"version": 0})),
        )
        .await;
        assert_eq!(field_names(&zero), vec!["version".to_owned()]);

        // Newest first: the removal, the move of production, the new staging, the first production.
        let events_path = format!("{lh}/label-events");
        let first: Page<PromptLabelEvent> = call(&app, Method::GET, &events_path, auth, None)
            .await
            .json();
        let shape: Vec<(&str, Option<i64>, Option<i64>)> = first
            .data
            .iter()
            .map(|event| (event.label.as_str(), event.from_version, event.to_version))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("staging", Some(2), None),
                ("production", Some(1), Some(2)),
                ("staging", None, Some(2)),
                ("production", None, Some(1)),
            ]
        );
        assert_eq!(first.next_cursor, None);

        let page_one: Page<PromptLabelEvent> = call(
            &app,
            Method::GET,
            &format!("{events_path}?limit=2"),
            auth,
            None,
        )
        .await
        .json();
        assert_eq!(page_one.data.len(), 2);
        let cursor = page_one.next_cursor.expect("a second page");
        let page_two: Page<PromptLabelEvent> = call(
            &app,
            Method::GET,
            &format!("{events_path}?limit=2&cursor={cursor}"),
            auth,
            None,
        )
        .await
        .json();
        assert_eq!(page_two.data.len(), 2);
        assert_eq!(page_two.data[1].label, "production");
        assert_eq!(page_two.next_cursor, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn label_protection() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let member = app
            .add_member(&seeded, "member@example.com", OrgRole::Member)
            .await;
        let member_auth = ("cookie", member.cookie.as_str());
        let admin_auth = ("cookie", seeded.cookie.as_str());
        let write_key = bearer(&app.write_key(&seeded).await);
        let list = base(&seeded);
        let lp = format!("{list}/lp");
        let created = call(
            &app,
            Method::POST,
            &list,
            admin_auth,
            Some(text_body("lp", &["production"])),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED);

        let refused = call(
            &app,
            Method::PUT,
            &format!("{lp}/labels/production"),
            member_auth,
            Some(json!({"version": 1})),
        )
        .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        let refused_delete = call(
            &app,
            Method::DELETE,
            &format!("{lp}/labels/production"),
            member_auth,
            None,
        )
        .await;
        assert_eq!(refused_delete.status, StatusCode::FORBIDDEN);

        let staging = call(
            &app,
            Method::PUT,
            &format!("{lp}/labels/staging"),
            member_auth,
            Some(json!({"version": 1})),
        )
        .await;
        assert_eq!(staging.status, StatusCode::OK);
        let by_key = call(
            &app,
            Method::PUT,
            &format!("{lp}/labels/production"),
            ("authorization", write_key.as_str()),
            Some(json!({"version": 1})),
        )
        .await;
        assert_eq!(by_key.status, StatusCode::OK);
        let removed = call(
            &app,
            Method::DELETE,
            &format!("{lp}/labels/staging"),
            member_auth,
            None,
        )
        .await;
        assert_eq!(removed.status, StatusCode::NO_CONTENT);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn settings() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let member = app
            .add_member(&seeded, "member@example.com", OrgRole::Member)
            .await;
        let member_auth = ("cookie", member.cookie.as_str());
        let admin_auth = ("cookie", seeded.cookie.as_str());
        let write_key = bearer(&app.write_key(&seeded).await);
        let settings = format!("/api/v1/projects/{}/prompt-settings", seeded.project.id);
        let list = base(&seeded);

        let default: PromptSettings = call(&app, Method::GET, &settings, admin_auth, None)
            .await
            .json();
        assert_eq!(default.protected_labels, vec!["production".to_owned()]);

        let member_put = call(
            &app,
            Method::PUT,
            &settings,
            member_auth,
            Some(json!({"protected_labels": ["prod"]})),
        )
        .await;
        assert_eq!(member_put.status, StatusCode::FORBIDDEN);
        let key_put = call(
            &app,
            Method::PUT,
            &settings,
            ("authorization", write_key.as_str()),
            Some(json!({"protected_labels": ["prod"]})),
        )
        .await;
        assert_eq!(key_put.status, StatusCode::FORBIDDEN);

        let stored = call(
            &app,
            Method::PUT,
            &settings,
            admin_auth,
            Some(json!({"protected_labels": ["prod"]})),
        )
        .await;
        assert_eq!(stored.status, StatusCode::OK);
        let answer: PromptSettings = stored.json();
        assert_eq!(answer.protected_labels, vec!["prod".to_owned()]);

        let st = format!("{list}/st");
        let created = call(
            &app,
            Method::POST,
            &list,
            member_auth,
            Some(text_body("st", &[])),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED);
        let production = call(
            &app,
            Method::PUT,
            &format!("{st}/labels/production"),
            member_auth,
            Some(json!({"version": 1})),
        )
        .await;
        assert_eq!(production.status, StatusCode::OK);
        let prod = call(
            &app,
            Method::PUT,
            &format!("{st}/labels/prod"),
            member_auth,
            Some(json!({"version": 1})),
        )
        .await;
        assert_eq!(prod.status, StatusCode::FORBIDDEN);

        let computed = call(
            &app,
            Method::PUT,
            &settings,
            admin_auth,
            Some(json!({"protected_labels": ["latest"]})),
        )
        .await;
        assert_eq!(
            field_names(&computed),
            vec!["protected_labels[0]".to_owned()]
        );
        let many: Vec<String> = (0..21).map(|index| format!("l{index:02}")).collect();
        let too_many = call(
            &app,
            Method::PUT,
            &settings,
            admin_auth,
            Some(json!({"protected_labels": many})),
        )
        .await;
        assert_eq!(field_names(&too_many), vec!["protected_labels".to_owned()]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn label_cap_conflict() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let auth = ("cookie", seeded.cookie.as_str());
        let list = base(&seeded);
        let cap = format!("{list}/cap");
        let created = call(&app, Method::POST, &list, auth, Some(text_body("cap", &[]))).await;
        assert_eq!(created.status, StatusCode::CREATED);
        for index in 0..50 {
            let response = call(
                &app,
                Method::PUT,
                &format!("{cap}/labels/l{index:02}"),
                auth,
                Some(json!({"version": 1})),
            )
            .await;
            assert_eq!(response.status, StatusCode::OK, "label l{index:02}");
        }
        let fifty_first = call(
            &app,
            Method::PUT,
            &format!("{cap}/labels/l50"),
            auth,
            Some(json!({"version": 1})),
        )
        .await;
        assert_eq!(fifty_first.status, StatusCode::CONFLICT);
    }

    const PROMPT_TRACE: &str = "a5000000000000000000000000000001";
    const WINDOW: &str = "from=2026-01-01T00:00:00Z&to=2026-01-02T00:00:00Z";

    /// Two `llm` spans of one trace, linked to versions 1 and 2 of `support-reply`, committed
    /// through the ingest path. Version 2's span is an error.
    async fn committed_prompt_spans(app: &TestApp, seeded: &Seeded) {
        let span = |span_id: &str, version: i64, status: &str| -> cetra_core::span::Span {
            serde_json::from_value(json!({
                "project_id": seeded.project.id.to_string(),
                "trace_id": PROMPT_TRACE,
                "span_id": span_id,
                "name": "generate",
                "span_kind": "llm",
                "start_time": "2026-01-01T00:10:00.000000000Z",
                "end_time": "2026-01-01T00:10:01.000000000Z",
                "status": status,
                "ingest_time": "2026-01-01T00:10:01.000000000Z",
                "schema_version": 2,
                "prompt_name": "support-reply",
                "prompt_version": version
            }))
            .unwrap()
        };
        app.commit_spans(
            &seeded.project,
            vec![
                span("b100000000000101", 1, "ok"),
                span("b100000000000102", 2, "error"),
            ],
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn metrics_route() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let auth = ("cookie", seeded.cookie.as_str());
        let created = call(
            &app,
            Method::POST,
            &base(&seeded),
            auth,
            Some(support_reply_body(&[])),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED);
        committed_prompt_spans(&app, &seeded).await;
        let metrics = format!("{}/support-reply/metrics", base(&seeded));

        let response = call(
            &app,
            Method::GET,
            &format!("{metrics}?{WINDOW}"),
            auth,
            None,
        )
        .await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let body: PromptMetrics = response.json();
        let rows: Vec<(i64, i64, i64, i64)> = body
            .versions
            .iter()
            .map(|version| {
                (
                    version.version,
                    version.span_count,
                    version.trace_count,
                    version.error_count,
                )
            })
            .collect();
        assert_eq!(rows, vec![(2, 1, 1, 1), (1, 1, 1, 0)]);

        let one = call(
            &app,
            Method::GET,
            &format!("{metrics}?{WINDOW}&versions=1"),
            auth,
            None,
        )
        .await;
        let one: PromptMetrics = one.json();
        let versions: Vec<i64> = one.versions.iter().map(|version| version.version).collect();
        assert_eq!(versions, vec![1]);

        for (query, field) in [
            ("versions=0", "versions"),
            ("versions=1,x", "versions"),
            ("from=2026-01-02T00:00:00Z&to=2026-01-01T00:00:00Z", "from"),
            ("from=2025-01-01T00:00:00Z&to=2026-01-02T00:00:00Z", "from"),
        ] {
            let response = call(&app, Method::GET, &format!("{metrics}?{query}"), auth, None).await;
            assert_eq!(field_names(&response), [field], "{query}");
        }
        for query in ["versions=", "versions=1,", "versions=,2"] {
            let response = call(
                &app,
                Method::GET,
                &format!("{metrics}?{WINDOW}&{query}"),
                auth,
                None,
            )
            .await;
            assert_eq!(field_names(&response), ["versions"], "{query}");
            let body: Value = response.json();
            assert_eq!(
                body["errors"][0]["message"], "must name 1 to 50 versions",
                "{query}"
            );
        }
        // The window as epoch milliseconds (2026-01-01 to 2026-01-02 UTC) gives the same rows.
        let by_millis = call(
            &app,
            Method::GET,
            &format!("{metrics}?from=1767225600000&to=1767312000000"),
            auth,
            None,
        )
        .await;
        assert_eq!(by_millis.status, StatusCode::OK);
        let by_millis: PromptMetrics = by_millis.json();
        assert_eq!(by_millis.versions.len(), 2);
        let versions_51 = (1..=51)
            .map(|version| version.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let response = call(
            &app,
            Method::GET,
            &format!("{metrics}?{WINDOW}&versions={versions_51}"),
            auth,
            None,
        )
        .await;
        assert_eq!(field_names(&response), ["versions"]);
        let body: Value = response.json();
        assert_eq!(body["errors"][0]["message"], "must name 1 to 50 versions");

        let missing = call(
            &app,
            Method::GET,
            &format!("{}/no-such-prompt/metrics?{WINDOW}", base(&seeded)),
            auth,
            None,
        )
        .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);

        let read_key = bearer(&seeded.read_key);
        let read = call(
            &app,
            Method::GET,
            &format!("{metrics}?{WINDOW}"),
            ("authorization", read_key.as_str()),
            None,
        )
        .await;
        assert_eq!(read.status, StatusCode::OK);
        let ingest_key = bearer(&seeded.ingest_key);
        let ingest = call(
            &app,
            Method::GET,
            &format!("{metrics}?{WINDOW}"),
            ("authorization", ingest_key.as_str()),
            None,
        )
        .await;
        assert_eq!(ingest.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn trace_summary_lists_prompts() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let auth = ("cookie", seeded.cookie.as_str());
        committed_prompt_spans(&app, &seeded).await;

        let traces = format!("/api/v1/projects/{}/traces?{WINDOW}", seeded.project.id);
        let page: Value = call(&app, Method::GET, &traces, auth, None).await.json();
        let mut prompts: Vec<&str> = page["data"][0]["prompts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|prompt| prompt.as_str().unwrap())
            .collect();
        prompts.sort();
        assert_eq!(prompts, ["support-reply@1", "support-reply@2"]);

        let spans = format!("/api/v1/projects/{}/spans?{WINDOW}", seeded.project.id);
        let listed: Value = call(&app, Method::GET, &spans, auth, None).await.json();
        let mut linked: Vec<(&str, i64)> = listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["prompt_name"].as_str().unwrap(),
                    row["prompt_version"].as_i64().unwrap(),
                )
            })
            .collect();
        linked.sort();
        assert_eq!(linked, [("support-reply", 1), ("support-reply", 2)]);

        let detail = format!(
            "/api/v1/projects/{}/traces/{PROMPT_TRACE}/spans/b100000000000102",
            seeded.project.id
        );
        let detail: Value = call(&app, Method::GET, &detail, auth, None).await.json();
        assert_eq!(detail["prompt_name"], "support-reply");
        assert_eq!(detail["prompt_version"], 2);
    }
}
