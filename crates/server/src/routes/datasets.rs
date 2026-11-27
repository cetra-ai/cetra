//! Dataset routes (spec 09 § Datasets & experiments, D-J-2 to D-J-5). Project readers list and
//! read; `ManageDatasets` writes (member and above, a `write` key, a read-write token).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::io;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::multipart::{MultipartError, MultipartRejection};
use axum::extract::{DefaultBodyLimit, Extension, Multipart, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use cetra_core::ids::OrgId;
use cetra_core::ids::{ProjectId, SpanId, TraceId};
use cetra_eval::{TraceData, VariableMapping, resolve};
use cetra_storage::RunError;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::{SqliteConnection, SqlitePool};
use tokio::sync::{Semaphore, SemaphorePermit};
use uuid::Uuid;

use crate::api::common::{FieldError, Page};
use crate::api::datasets::{
    AddItemsRequest, AddItemsResponse, CreateDatasetRequest, Dataset, DatasetItem,
    DatasetItemListParams, DatasetItemParams, DatasetListParams, DatasetTrend, DatasetVersion,
    DatasetVersionListParams, DeleteItemsRequest, EditItemsRequest, EditItemsResponse,
    ExperimentScoreMean, ExperimentTrend, ExperimentTrendParams, ExperimentTrends, ExportFormat,
    ExportParams, FromTraceRequest, FromTracesRequest, FromTracesResult, ImportFormat,
    ImportMapping, ImportParams, ImportPreview, ImportResult, ImportRowError, ItemChange,
    ItemChangeKind, ItemChangeListParams, NewDatasetItem, PreviewParams, RestoreRequest,
    SkippedTrace, TraceRef, UpdateDatasetRequest, UpdateItemRequest,
};
use crate::api::experiments::CLOCK_SKEW_NS;
use crate::audit::catalog::DatasetAction;
use crate::audit::{self, Event, TargetKind};
use crate::auth::principal::Principal;
use crate::auth::roles::ProjectPerm;
use crate::datasets::import::{self, FileError};
use crate::datasets::{
    ItemValues, MAX_BATCH_ITEMS, MAX_SCHEMA_CHECK_BYTES, MAX_SCHEMA_FAILURES, Schemas,
    check_description, check_item, check_name, check_schema, json_bytes,
};
use crate::entitlements::Visibility;
use crate::eval::execute;
use crate::eval::trace_load::{self, LoadError, LoadedTrace};
use crate::experiments::visible_from;
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::meta::MetaError;
use crate::meta::datasets as store;
use crate::meta::datasets::{
    DatasetFields, DatasetKey, DatasetRow, ItemRow, NewRevision, VersionChange, VersionCounts,
};
use crate::meta::experiments as experiment_store;

use super::orgs::page_named;
use super::read::{Deadline, open_query};

/// Bodies of dataset create and update: a name, a description and two schemas of ≤ 64 KiB.
const DATASET_BODY_LIMIT: usize = 256 * 1024;

/// Items read per page by a restore (D-J-24).
const RESTORE_PAGE: u32 = 1_000;

/// A batch add: up to 1 000 items of at most 1 MiB, within 16 MiB (D-J-4).
const ITEMS_BODY_LIMIT: usize = 16 * 1024 * 1024;

/// One item edit: an item of at most 1 MiB plus JSON overhead.
const ITEM_BODY_LIMIT: usize = 2 * 1024 * 1024;

/// Page size of the live-item read when a new schema is checked.
const SCHEMA_PAGE: u32 = 100;

/// Field errors listed by a refused batch add (D-J-2). The first ones, in request order.
const MAX_LISTED_ERRORS: usize = 100;

/// The preview body: the head of the file (D-J-6).
const PREVIEW_BODY_LIMIT: usize = import::MAX_PREVIEW_BYTES;

/// The import body: the file plus the multipart framing and the mapping part.
const IMPORT_BODY_LIMIT: usize = import::MAX_FILE_BYTES + 1024 * 1024;

/// The largest mapping part.
const MAPPING_BYTES: usize = 1024 * 1024;

/// Imports parsed at once in this process; each holds its file in memory.
const IMPORTS_AT_ONCE: usize = 2;
static IMPORTS: Semaphore = Semaphore::const_new(IMPORTS_AT_ONCE);

/// The whole multipart body must arrive within this time, so a slow sender cannot hold an
/// import permit (D-J-6).
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// Dataset routes. Reads need project view; writes need [`ProjectPerm::ManageDatasets`].
/// Not session-only.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/datasets",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_datasets),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/datasets",
            access: Access::Project {
                perm: ProjectPerm::ManageDatasets,
                session_only: false,
            },
            documented: true,
            handler: post(create_dataset).layer(DefaultBodyLimit::max(DATASET_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/dataset-trends",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(experiment_trends),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/datasets/{dataset}",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_dataset),
        },
        RouteDef {
            method: Method::PATCH,
            path: "/api/v1/projects/{project}/datasets/{dataset}",
            access: Access::Project {
                perm: ProjectPerm::ManageDatasets,
                session_only: false,
            },
            documented: true,
            handler: patch(update_dataset).layer(DefaultBodyLimit::max(DATASET_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::DELETE,
            path: "/api/v1/projects/{project}/datasets/{dataset}",
            access: Access::Project {
                perm: ProjectPerm::ManageDatasets,
                session_only: false,
            },
            documented: true,
            handler: delete(delete_dataset),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/datasets/{dataset}/versions",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_versions),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/datasets/{dataset}/versions/{version}/changes",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_changes),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/datasets/{dataset}/items",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_items),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/datasets/{dataset}/items",
            access: Access::Project {
                perm: ProjectPerm::ManageDatasets,
                session_only: false,
            },
            documented: true,
            handler: post(add_items).layer(DefaultBodyLimit::max(ITEMS_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::PATCH,
            path: "/api/v1/projects/{project}/datasets/{dataset}/items",
            access: Access::Project {
                perm: ProjectPerm::ManageDatasets,
                session_only: false,
            },
            documented: true,
            handler: patch(edit_items).layer(DefaultBodyLimit::max(ITEMS_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/datasets/{dataset}/items/from-traces",
            access: Access::Project {
                perm: ProjectPerm::ManageDatasets,
                session_only: false,
            },
            documented: true,
            handler: post(add_from_traces).layer(DefaultBodyLimit::max(DATASET_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/datasets/{dataset}/restore",
            access: Access::Project {
                perm: ProjectPerm::ManageDatasets,
                session_only: false,
            },
            documented: true,
            handler: post(restore_version).layer(DefaultBodyLimit::max(DATASET_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/datasets/{dataset}/items/delete",
            access: Access::Project {
                perm: ProjectPerm::ManageDatasets,
                session_only: false,
            },
            documented: true,
            handler: post(delete_items).layer(DefaultBodyLimit::max(DATASET_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/datasets/{dataset}/items/{item_id}",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_item),
        },
        RouteDef {
            method: Method::PATCH,
            path: "/api/v1/projects/{project}/datasets/{dataset}/items/{item_id}",
            access: Access::Project {
                perm: ProjectPerm::ManageDatasets,
                session_only: false,
            },
            documented: true,
            handler: patch(update_item).layer(DefaultBodyLimit::max(ITEM_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::DELETE,
            path: "/api/v1/projects/{project}/datasets/{dataset}/items/{item_id}",
            access: Access::Project {
                perm: ProjectPerm::ManageDatasets,
                session_only: false,
            },
            documented: true,
            handler: delete(delete_item),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/datasets/{dataset}/import/preview",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: post(preview_import).layer(DefaultBodyLimit::max(PREVIEW_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/datasets/{dataset}/import",
            access: Access::Project {
                perm: ProjectPerm::ManageDatasets,
                session_only: false,
            },
            documented: true,
            handler: post(import_items).layer(DefaultBodyLimit::max(IMPORT_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/datasets/{dataset}/export",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(export_items),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/datasets/{dataset}/items/from-trace",
            access: Access::Project {
                perm: ProjectPerm::ManageDatasets,
                session_only: false,
            },
            documented: true,
            handler: post(add_from_trace),
        },
    ]
}

/// The `{dataset}` segment: an id or a name (D-J-4).
#[derive(Debug, Deserialize)]
pub(crate) struct DatasetPath {
    pub(crate) dataset: String,
}

/// The `{dataset}` and `{version}` segments of a version route.
#[derive(Debug, Deserialize)]
pub(crate) struct VersionPath {
    pub(crate) dataset: String,
    pub(crate) version: i64,
}

/// The `{dataset}` and `{item_id}` segments of an item route.
#[derive(Debug, Deserialize)]
pub(crate) struct ItemPath {
    dataset: String,
    item_id: Uuid,
}

/// Who made a change: a user id, `key:<key id>`, or `None` (the control plane never reaches
/// these routes).
pub(crate) fn created_by(principal: &Principal) -> Option<String> {
    match principal {
        Principal::Session { user, .. } | Principal::Pat { user, .. } => Some(user.id.to_string()),
        Principal::ApiKey { key_id, .. } => Some(format!("key:{key_id}")),
        Principal::ControlPlane { .. } | Principal::Scim { .. } => None,
    }
}

/// The dataset named by the path (`DatasetKey::parse(raw)`, read on the reader pool), or 404.
pub(crate) async fn find(
    state: &AppState,
    project: ProjectId,
    raw: &str,
) -> Result<DatasetRow, ApiError> {
    store::get_dataset(state.meta.reader(), project, &DatasetKey::parse(raw))
        .await?
        .ok_or(ApiError::NotFound)
}

/// API form of a stored dataset. A stored schema that does not parse is `MetaError::Corrupt`.
pub(crate) fn dataset_json(row: &DatasetRow) -> Result<Dataset, ApiError> {
    Ok(Dataset {
        id: row.id,
        name: row.name.clone(),
        description: row.description.clone(),
        input_schema: stored_schema(row.input_schema.as_deref(), "input_schema")?,
        expected_schema: stored_schema(row.expected_schema.as_deref(), "expected_schema")?,
        current_version: row.current_version,
        item_count: row.item_count,
        experiment_count: row.experiment_count,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

fn stored_schema(text: Option<&str>, column: &'static str) -> Result<Option<Value>, ApiError> {
    text.map(serde_json::from_str::<Value>)
        .transpose()
        .map_err(|_| {
            ApiError::from(MetaError::Corrupt {
                table: "datasets",
                column,
            })
        })
}

fn bad_request(errors: Vec<FieldError>) -> ApiError {
    ApiError::BadRequest {
        detail: None,
        errors,
    }
}

/// `Some` of the value when it checked; otherwise the error is kept and `None` returned.
fn collect<T>(result: Result<T, FieldError>, errors: &mut Vec<FieldError>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            errors.push(error);
            None
        }
    }
}

/// Every field error of a create body; the fields that check are returned as a [`DatasetFields`].
fn create_fields(body: &CreateDatasetRequest) -> Result<DatasetFields, Vec<FieldError>> {
    let mut errors = Vec::new();
    let name = collect(check_name(&body.name), &mut errors);
    let description = match &body.description {
        Some(text) => collect(check_description(text), &mut errors).flatten(),
        None => None,
    };
    let input_schema = match &body.input_schema {
        Some(schema) => collect(check_schema("input_schema", schema), &mut errors),
        None => None,
    };
    let expected_schema = match &body.expected_schema {
        Some(schema) => collect(check_schema("expected_schema", schema), &mut errors),
        None => None,
    };
    match name {
        Some(name) if errors.is_empty() => Ok(DatasetFields {
            name,
            description,
            input_schema,
            expected_schema,
        }),
        _ => Err(errors),
    }
}

/// The present fields of an update, checked. `None` keeps the stored value; `Some(None)`
/// clears it.
struct CheckedUpdate {
    name: Option<String>,
    description: Option<Option<String>>,
    input_schema: Option<Option<String>>,
    expected_schema: Option<Option<String>>,
}

fn check_schema_field(
    field: &str,
    value: &Option<Option<Value>>,
    errors: &mut Vec<FieldError>,
) -> Option<Option<String>> {
    match value {
        None => None,
        Some(None) => Some(None),
        Some(Some(schema)) => collect(check_schema(field, schema), errors).map(Some),
    }
}

fn check_update(body: &UpdateDatasetRequest) -> Result<CheckedUpdate, Vec<FieldError>> {
    let mut errors = Vec::new();
    let name = match &body.name {
        Some(name) => collect(check_name(name), &mut errors),
        None => None,
    };
    let description = match &body.description {
        None => None,
        Some(None) => Some(None),
        Some(Some(text)) => collect(check_description(text), &mut errors),
    };
    let input_schema = check_schema_field("input_schema", &body.input_schema, &mut errors);
    let expected_schema = check_schema_field("expected_schema", &body.expected_schema, &mut errors);
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(CheckedUpdate {
        name,
        description,
        input_schema,
        expected_schema,
    })
}

/// The stored row with every checked field replaced.
fn merged(row: &DatasetRow, checked: CheckedUpdate) -> DatasetFields {
    DatasetFields {
        name: checked.name.unwrap_or_else(|| row.name.clone()),
        description: checked
            .description
            .unwrap_or_else(|| row.description.clone()),
        input_schema: checked
            .input_schema
            .unwrap_or_else(|| row.input_schema.clone()),
        expected_schema: checked
            .expected_schema
            .unwrap_or_else(|| row.expected_schema.clone()),
    }
}

/// What a write transaction answers when it writes nothing.
enum WriteOutcome {
    NotFound,
    Invalid(Vec<FieldError>),
}

async fn list_datasets(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<DatasetListParams>,
) -> Result<error::Json<Page<Dataset>>, ApiError> {
    let needle = params.q.as_deref().map(str::trim).filter(|q| !q.is_empty());
    let rows = store::list_datasets(state.meta.reader(), access.project.id, needle).await?;
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        let lower = row.name.to_lowercase();
        items.push((lower, row.id.to_string(), dataset_json(&row)?));
    }
    Ok(error::Json(page_named(
        items,
        params.cursor.as_deref(),
        params.limit.get(),
    )?))
}

async fn create_dataset(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    ctx: audit::Ctx,
    error::Json(body): error::Json<CreateDatasetRequest>,
) -> Result<(StatusCode, error::Json<Dataset>), ApiError> {
    let fields = create_fields(&body).map_err(bad_request)?;
    let id = Uuid::now_v7();
    let project = access.project.id;
    let org = access.project.org_id;
    let now = state.clock.now_ns();
    let row = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let row = store::insert_dataset(conn, id, project, fields, now).await?;
                let event = Event::new(DatasetAction::Created)
                    .org(org)
                    .project(project)
                    .target(TargetKind::Dataset, id);
                audit::record(conn, &ctx, event, now).await?;
                Ok(row)
            })
        })
        .await?;
    Ok((StatusCode::CREATED, error::Json(dataset_json(&row)?)))
}

async fn get_dataset(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(DatasetPath { dataset }): error::Path<DatasetPath>,
) -> Result<error::Json<Dataset>, ApiError> {
    let row = find(&state, access.project.id, &dataset).await?;
    Ok(error::Json(dataset_json(&row)?))
}

async fn update_dataset(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    ctx: audit::Ctx,
    error::Path(DatasetPath { dataset }): error::Path<DatasetPath>,
    error::Json(body): error::Json<UpdateDatasetRequest>,
) -> Result<error::Json<Dataset>, ApiError> {
    let changed_fields: Vec<&'static str> = [
        ("name", body.name.is_some()),
        ("description", body.description.is_some()),
        ("input_schema", body.input_schema.is_some()),
        ("expected_schema", body.expected_schema.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, given)| given.then_some(name))
    .collect();
    let checked = check_update(&body).map_err(bad_request)?;
    let project = access.project.id;
    let org = access.project.org_id;
    let key = DatasetKey::parse(&dataset);
    let now = state.clock.now_ns();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let written = update_tx(conn, project, key, checked, now).await?;
                if let Ok(row) = &written {
                    let event = Event::new(DatasetAction::Updated {
                        fields: changed_fields,
                    })
                    .org(org)
                    .project(project)
                    .target(TargetKind::Dataset, row.id);
                    audit::record(conn, &ctx, event, now).await?;
                }
                Ok(written)
            })
        })
        .await?;
    let row = match outcome {
        Ok(row) => row,
        Err(WriteOutcome::NotFound) => return Err(ApiError::NotFound),
        Err(WriteOutcome::Invalid(errors)) => return Err(bad_request(errors)),
    };
    Ok(error::Json(dataset_json(&row)?))
}

/// The update's write transaction. A schema that changes is checked against every live item
/// first (D-J-2), inside the transaction, so nothing is written when an item fails.
async fn update_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    key: DatasetKey,
    checked: CheckedUpdate,
    now: i64,
) -> Result<Result<DatasetRow, WriteOutcome>, MetaError> {
    let Some(row) = store::get_dataset_tx(conn, project, &key).await? else {
        return Ok(Err(WriteOutcome::NotFound));
    };
    let fields = merged(&row, checked);
    let input_changed = fields.input_schema != row.input_schema;
    let expected_changed = fields.expected_schema != row.expected_schema;
    let needs_check = (input_changed && fields.input_schema.is_some())
        || (expected_changed && fields.expected_schema.is_some());
    if needs_check {
        let field = if input_changed {
            "input_schema"
        } else {
            "expected_schema"
        };
        let schemas = Schemas::compile(
            fields.input_schema.as_deref(),
            fields.expected_schema.as_deref(),
        )
        .map_err(|column| MetaError::Corrupt {
            table: "datasets",
            column,
        })?;
        if let Some(errors) =
            check_live_items(conn, row.id, field, &schemas, MAX_SCHEMA_CHECK_BYTES).await?
        {
            return Ok(Err(WriteOutcome::Invalid(errors)));
        }
    }
    Ok(
        match store::update_dataset(conn, project, row.id, fields, now).await? {
            Some(row) => Ok(row),
            None => Err(WriteOutcome::NotFound),
        },
    )
}

/// The one error of a schema change whose live items hold more than [`MAX_SCHEMA_CHECK_BYTES`].
fn too_large_for_schema(field: &str) -> FieldError {
    field_error(
        field,
        "the dataset is too large to validate against a new schema",
    )
}

/// Checks the live items of the dataset against `schemas`. `Some(errors)` lists the first
/// [`MAX_SCHEMA_FAILURES`] failures, or the one "too large" error when the live items hold more
/// than `max_bytes` of JSON. The size is summed in SQL before any item is read, so an oversized
/// dataset is refused without parsing it (D-J-2). `field` names the schema that changed.
async fn check_live_items(
    conn: &mut SqliteConnection,
    dataset: Uuid,
    field: &str,
    schemas: &Schemas,
    max_bytes: usize,
) -> Result<Option<Vec<FieldError>>, MetaError> {
    let total = store::live_json_bytes_tx(conn, dataset).await?;
    if total > i64::try_from(max_bytes).unwrap_or(i64::MAX) {
        return Ok(Some(vec![too_large_for_schema(field)]));
    }
    let mut errors = Vec::new();
    let mut bytes = 0usize;
    let mut after = None;
    loop {
        let mut page = store::live_items_tx(conn, dataset, after, SCHEMA_PAGE).await?;
        let more = page.len() > SCHEMA_PAGE as usize;
        page.truncate(SCHEMA_PAGE as usize);
        for item in &page {
            bytes += json_bytes(&item.input, item.expected_output.as_ref(), &item.metadata);
            if bytes > max_bytes {
                return Ok(Some(vec![too_large_for_schema(field)]));
            }
            let values = ItemValues {
                input: item.input.clone(),
                expected_output: item.expected_output.clone(),
                metadata: item.metadata.clone(),
            };
            errors.extend(check_item(
                &format!("items[{}]", item.item_id),
                &values,
                schemas,
            ));
            if errors.len() >= MAX_SCHEMA_FAILURES {
                errors.truncate(MAX_SCHEMA_FAILURES);
                return Ok(Some(errors));
            }
        }
        let Some(last) = page.last() else {
            break;
        };
        if !more {
            break;
        }
        after = Some(last.item_id);
    }
    Ok(if errors.is_empty() {
        None
    } else {
        Some(errors)
    })
}

async fn delete_dataset(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    ctx: audit::Ctx,
    error::Path(DatasetPath { dataset }): error::Path<DatasetPath>,
) -> Result<StatusCode, ApiError> {
    let project = access.project.id;
    let org = access.project.org_id;
    let id = find(&state, project, &dataset).await?.id;
    let now = state.clock.now_ns();
    let deleted = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let deleted = store::delete_dataset(conn, project, id).await?;
                if deleted {
                    let event = Event::new(DatasetAction::Deleted)
                        .org(org)
                        .project(project)
                        .target(TargetKind::Dataset, id);
                    audit::record(conn, &ctx, event, now).await?;
                }
                Ok(deleted)
            })
        })
        .await?;
    if !deleted {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

fn field_error(field: &str, message: impl Into<String>) -> FieldError {
    FieldError {
        field: field.to_owned(),
        message: message.into(),
        position: None,
    }
}

fn invalid_cursor() -> ApiError {
    bad_request(vec![field_error("cursor", "invalid cursor")])
}

/// `?version=`: the default is the current version. 0 is the empty version, before the first
/// write (D-J-3). Any other value must be between 0 and the current version.
fn version_of(row: &DatasetRow, requested: Option<i64>) -> Result<i64, ApiError> {
    match requested {
        None => Ok(row.current_version),
        Some(version) if (0..=row.current_version).contains(&version) => Ok(version),
        Some(_) => Err(bad_request(vec![field_error(
            "version",
            format!("must be between 0 and {}", row.current_version),
        )])),
    }
}

/// API form of one stored item revision.
pub(crate) fn item_json(row: &ItemRow) -> DatasetItem {
    DatasetItem {
        id: row.item_id,
        revision_id: row.revision_id,
        version: row.valid_from_version,
        input: row.input.clone(),
        expected_output: row.expected_output.clone(),
        metadata: row.metadata.clone(),
        source_trace_id: row.source_trace_id,
        source_span_id: row.source_span_id,
        created_at: row.created_at,
    }
}

fn add_description(count: usize) -> String {
    if count == 1 {
        "add 1 item".to_owned()
    } else {
        format!("add {count} items")
    }
}

/// The schemas of a stored dataset. Stored text that no longer compiles is `MetaError::Corrupt`.
fn compiled_schemas(row: &DatasetRow) -> Result<Schemas, MetaError> {
    Schemas::compile(row.input_schema.as_deref(), row.expected_schema.as_deref()).map_err(
        |column| MetaError::Corrupt {
            table: "datasets",
            column,
        },
    )
}

async fn list_versions(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(DatasetPath { dataset }): error::Path<DatasetPath>,
    error::Query(params): error::Query<DatasetVersionListParams>,
) -> Result<error::Json<Page<DatasetVersion>>, ApiError> {
    let row = find(&state, access.project.id, &dataset).await?;
    let before = match params.cursor.as_deref() {
        None => None,
        Some(cursor) => Some(cursor.parse::<i64>().map_err(|_| invalid_cursor())?),
    };
    let limit = params.limit.get();
    let mut versions = store::list_versions(state.meta.reader(), row.id, before, limit).await?;
    let more = versions.len() > limit as usize;
    versions.truncate(limit as usize);
    let next_cursor = if more {
        versions.last().map(|last| last.version.to_string())
    } else {
        None
    };
    let data = versions
        .into_iter()
        .map(|version| DatasetVersion {
            version: version.version,
            description: version.description,
            created_by: version.created_by,
            created_at: version.created_at,
            added: version.counts.added,
            edited: version.counts.edited,
            deleted: version.counts.deleted,
        })
        .collect();
    Ok(error::Json(Page { data, next_cursor }))
}

/// `GET …/versions/{version}/changes` (D-J-24): the items the version changed, paged by item id.
/// A version outside 1 to the current one is 404.
async fn list_changes(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(VersionPath { dataset, version }): error::Path<VersionPath>,
    error::Query(params): error::Query<ItemChangeListParams>,
) -> Result<error::Json<Page<ItemChange>>, ApiError> {
    let row = find(&state, access.project.id, &dataset).await?;
    if !(1..=row.current_version).contains(&version) {
        return Err(ApiError::NotFound);
    }
    let after = match params.cursor.as_deref() {
        None => None,
        Some(cursor) => Some(cursor.parse::<Uuid>().map_err(|_| invalid_cursor())?),
    };
    let limit = params.limit.get();
    let mut changes =
        store::list_changes(state.meta.reader(), row.id, version, after, limit).await?;
    let more = changes.len() > limit as usize;
    changes.truncate(limit as usize);
    let next_cursor = if more {
        changes.last().map(|last| last.item_id.to_string())
    } else {
        None
    };
    let data = changes
        .iter()
        .map(|change| ItemChange {
            item_id: change.item_id,
            change: match (&change.before, &change.after) {
                (None, _) => ItemChangeKind::Added,
                (Some(_), None) => ItemChangeKind::Deleted,
                (Some(_), Some(_)) => ItemChangeKind::Edited,
            },
            before: change.before.as_ref().map(item_json),
            after: change.after.as_ref().map(item_json),
        })
        .collect();
    Ok(error::Json(Page { data, next_cursor }))
}

/// The trimmed search text of an item list: 1 to 200 characters (D-J-22).
fn check_search(q: &str) -> Result<String, ApiError> {
    let trimmed = q.trim();
    let length = trimmed.chars().count();
    if (1..=200).contains(&length) {
        Ok(trimmed.to_owned())
    } else {
        Err(bad_request(vec![field_error(
            "q",
            "must be 1 to 200 characters",
        )]))
    }
}

async fn list_items(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(DatasetPath { dataset }): error::Path<DatasetPath>,
    error::Query(params): error::Query<DatasetItemListParams>,
) -> Result<error::Json<Page<DatasetItem>>, ApiError> {
    let row = find(&state, access.project.id, &dataset).await?;
    let version = version_of(&row, params.version)?;
    let after = match params.cursor.as_deref() {
        None => None,
        Some(cursor) => Some(cursor.parse::<Uuid>().map_err(|_| invalid_cursor())?),
    };
    let needle = match params.q.as_deref() {
        None => None,
        Some(q) => Some(check_search(q)?),
    };
    let limit = params.limit.get();
    let mut items = store::list_items(
        state.meta.reader(),
        row.id,
        version,
        after,
        limit,
        needle.as_deref(),
    )
    .await?;
    let more = items.len() > limit as usize;
    items.truncate(limit as usize);
    let next_cursor = if more {
        items.last().map(|last| last.item_id.to_string())
    } else {
        None
    };
    Ok(error::Json(Page {
        data: items.iter().map(item_json).collect(),
        next_cursor,
    }))
}

async fn get_item(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(ItemPath { dataset, item_id }): error::Path<ItemPath>,
    error::Query(params): error::Query<DatasetItemParams>,
) -> Result<error::Json<DatasetItem>, ApiError> {
    let row = find(&state, access.project.id, &dataset).await?;
    let version = version_of(&row, params.version)?;
    let item = store::get_item(state.meta.reader(), row.id, item_id, version)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(error::Json(item_json(&item)))
}

/// Rows read per page of an export (J2.3). Each SQLite read stays short.
const EXPORT_PAGE: u32 = 1_000;

/// The header record of a CSV export (D-J-7).
const CSV_HEADER: &[u8] = b"id,input,expected_output,metadata,source_trace_id,source_span_id\r\n";

/// One JSONL line of an export (D-J-7). The source ids are omitted when none.
#[derive(Serialize)]
struct ExportLine<'a> {
    id: Uuid,
    input: &'a Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_output: Option<&'a Value>,
    metadata: &'a Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_trace_id: Option<TraceId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_span_id: Option<SpanId>,
}

/// Where an export's stream is: the item id to read after, whether the CSV header is still due,
/// and whether the last page was read.
struct ExportCursor {
    after: Option<Uuid>,
    header_due: bool,
    done: bool,
}

/// `GET …/datasets/{dataset}/export?version=&format=` (D-J-7): the live items of a version,
/// streamed in pages of [`EXPORT_PAGE`] ordered by item id. Versions are immutable, so the stream
/// cannot change under the reader.
async fn export_items(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    ctx: audit::Ctx,
    error::Path(DatasetPath { dataset }): error::Path<DatasetPath>,
    error::Query(params): error::Query<ExportParams>,
) -> Result<Response, ApiError> {
    let row = find(&state, access.project.id, &dataset).await?;
    let version = version_of(&row, params.version)?;
    let format = params.format.unwrap_or(ExportFormat::Jsonl);
    let (content_type, extension) = match format {
        ExportFormat::Jsonl => ("application/x-ndjson", "jsonl"),
        ExportFormat::Csv => ("text/csv; charset=utf-8", "csv"),
    };
    let disposition = format!("attachment; filename=\"{}-v{version}.{extension}\"", row.id);
    let disposition =
        HeaderValue::from_str(&disposition).map_err(|error| ApiError::Internal(Box::new(error)))?;
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CONTENT_DISPOSITION, disposition);
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));

    // The export is recorded before any item is read (D-EA-6): no record, no stream.
    let event = Event::new(DatasetAction::ExportStarted {
        version: u32::try_from(version).unwrap_or(u32::MAX),
        format: extension,
    })
    .org(access.project.org_id)
    .project(access.project.id)
    .target(TargetKind::Dataset, row.id);
    audit::record_alone(&state.meta, &ctx, event, state.clock.now_ns()).await?;

    let pool = state.meta.reader().clone();
    let dataset = row.id;
    let project = access.project.id;
    let cursor = ExportCursor {
        after: None,
        header_due: format == ExportFormat::Csv,
        done: false,
    };
    let pages = futures::stream::try_unfold(cursor, move |cursor| {
        export_page(pool.clone(), dataset, project, version, format, cursor)
    });
    Ok((StatusCode::OK, headers, Body::from_stream(pages)).into_response())
}

/// One page of an export as its chunk of bytes, with the cursor for the next one. `None` ends the
/// stream: after the last page, or for a JSONL export of an empty version.
async fn export_page(
    pool: SqlitePool,
    dataset: Uuid,
    project: ProjectId,
    version: i64,
    format: ExportFormat,
    cursor: ExportCursor,
) -> Result<Option<(Bytes, ExportCursor)>, io::Error> {
    if cursor.done {
        return Ok(None);
    }
    let fail = |error: &dyn fmt::Display| {
        tracing::warn!(
            dataset_id = %dataset,
            project_id = %project,
            version,
            error = %error,
            "dataset export failed"
        );
        io::Error::other("dataset export failed")
    };
    let mut rows =
        match store::list_items(&pool, dataset, version, cursor.after, EXPORT_PAGE, None).await {
            Ok(rows) => rows,
            Err(error) => return Err(fail(&error)),
        };
    // The store returns one more row than asked when there is a next page.
    let more = rows.len() > EXPORT_PAGE as usize;
    rows.truncate(EXPORT_PAGE as usize);
    let bytes = match encode_rows(&rows, format, cursor.header_due) {
        Ok(bytes) => bytes,
        Err(error) => return Err(fail(&error)),
    };
    if bytes.is_empty() {
        return Ok(None);
    }
    let next = ExportCursor {
        after: rows.last().map(|row| row.item_id),
        header_due: false,
        done: !more,
    };
    Ok(Some((Bytes::from(bytes), next)))
}

/// The bytes of one page of rows: JSONL lines, or CSV records preceded by the header when
/// `with_header` is set.
fn encode_rows(rows: &[ItemRow], format: ExportFormat, with_header: bool) -> io::Result<Vec<u8>> {
    match format {
        ExportFormat::Jsonl => {
            let mut bytes = Vec::new();
            for row in rows {
                let line = ExportLine {
                    id: row.item_id,
                    input: &row.input,
                    expected_output: row.expected_output.as_ref(),
                    metadata: &row.metadata,
                    source_trace_id: row.source_trace_id,
                    source_span_id: row.source_span_id,
                };
                serde_json::to_writer(&mut bytes, &line).map_err(io::Error::other)?;
                bytes.push(b'\n');
            }
            Ok(bytes)
        }
        ExportFormat::Csv => {
            let mut bytes = if with_header {
                CSV_HEADER.to_vec()
            } else {
                Vec::new()
            };
            let mut writer = csv::WriterBuilder::new()
                .terminator(csv::Terminator::CRLF)
                .from_writer(Vec::new());
            for row in rows {
                writer
                    .write_record([
                        row.item_id.to_string(),
                        compact_json(&row.input)?,
                        row.expected_output
                            .as_ref()
                            .map(compact_json)
                            .transpose()?
                            .unwrap_or_default(),
                        compact_json(&row.metadata)?,
                        row.source_trace_id
                            .map(|id| id.to_string())
                            .unwrap_or_default(),
                        row.source_span_id
                            .map(|id| id.to_string())
                            .unwrap_or_default(),
                    ])
                    .map_err(io::Error::other)?;
            }
            bytes.extend(
                writer
                    .into_inner()
                    .map_err(csv::IntoInnerError::into_error)?,
            );
            Ok(bytes)
        }
    }
}

/// A JSON value as compact text, for a CSV cell.
fn compact_json(value: &impl Serialize) -> io::Result<String> {
    serde_json::to_string(value).map_err(io::Error::other)
}

async fn add_items(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(DatasetPath { dataset }): error::Path<DatasetPath>,
    error::Json(body): error::Json<AddItemsRequest>,
) -> Result<(StatusCode, error::Json<AddItemsResponse>), ApiError> {
    if !(1..=MAX_BATCH_ITEMS).contains(&body.items.len()) {
        return Err(bad_request(vec![field_error(
            "items",
            "must hold 1 to 1000 items",
        )]));
    }
    let project = access.project.id;
    let key = DatasetKey::parse(&dataset);
    let now = state.clock.now_ns();
    let actor = created_by(&principal);
    let created_by = actor.clone();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(
                async move { add_items_tx(conn, project, key, body.items, created_by, now).await },
            )
        })
        .await?;
    let (dataset_id, response) = match outcome {
        Ok(added) => added,
        Err(WriteOutcome::NotFound) => return Err(ApiError::NotFound),
        Err(WriteOutcome::Invalid(errors)) => return Err(bad_request(errors)),
    };
    tracing::info!(
        dataset_id = %dataset_id,
        project_id = %project,
        actor = %actor.as_deref().unwrap_or("-"),
        count = response.item_ids.len(),
        version = response.version,
        "dataset items added"
    );
    Ok((StatusCode::CREATED, error::Json(response)))
}

/// The batch add's write transaction. Every item is checked before anything is written; one
/// failing item writes nothing (D-J-2).
async fn add_items_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    key: DatasetKey,
    items: Vec<NewDatasetItem>,
    created_by: Option<String>,
    now: i64,
) -> Result<Result<(Uuid, AddItemsResponse), WriteOutcome>, MetaError> {
    let Some(row) = store::get_dataset_tx(conn, project, &key).await? else {
        return Ok(Err(WriteOutcome::NotFound));
    };
    let schemas = compiled_schemas(&row)?;
    let mut errors = Vec::new();
    let mut values = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        let item_values = ItemValues {
            input: item.input,
            expected_output: item.expected_output,
            metadata: item.metadata.unwrap_or_default(),
        };
        errors.extend(check_item(
            &format!("items[{index}]"),
            &item_values,
            &schemas,
        ));
        values.push(item_values);
    }
    if !errors.is_empty() {
        errors.truncate(MAX_LISTED_ERRORS);
        return Ok(Err(WriteOutcome::Invalid(errors)));
    }
    let mut item_ids = Vec::with_capacity(values.len());
    let mut insert = Vec::with_capacity(values.len());
    for item in values {
        let item_id = Uuid::now_v7();
        item_ids.push(item_id);
        insert.push(NewRevision {
            revision_id: Uuid::now_v7(),
            item_id,
            input: item.input,
            expected_output: item.expected_output,
            metadata: item.metadata,
            source_trace_id: None,
            source_span_id: None,
        });
    }
    let change = VersionChange {
        close: Vec::new(),
        description: add_description(insert.len()),
        insert,
        created_by,
    };
    let version = store::write_version(conn, row.id, change, now).await?;
    Ok(Ok((row.id, AddItemsResponse { version, item_ids })))
}

/// An item edit after the `null` check: `None` keeps the field (D-J-3). `expected_output`
/// keeps its `null`, which clears it.
struct ItemPatch {
    input: Option<Value>,
    expected_output: Option<Option<Value>>,
    metadata: Option<Map<String, Value>>,
}

/// `input` and `metadata` must not be null (`expected_output` may be, and clears it).
fn check_item_patch(body: UpdateItemRequest) -> Result<ItemPatch, Vec<FieldError>> {
    let mut errors = Vec::new();
    let input = match body.input {
        Some(None) => {
            errors.push(field_error("input", "must not be null"));
            None
        }
        other => other.flatten(),
    };
    let metadata = match body.metadata {
        Some(None) => {
            errors.push(field_error("metadata", "must not be null"));
            None
        }
        other => other.flatten(),
    };
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(ItemPatch {
        input,
        expected_output: body.expected_output,
        metadata,
    })
}

async fn update_item(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(ItemPath { dataset, item_id }): error::Path<ItemPath>,
    error::Json(body): error::Json<UpdateItemRequest>,
) -> Result<error::Json<DatasetItem>, ApiError> {
    let patch = check_item_patch(body).map_err(bad_request)?;
    let project = access.project.id;
    let key = DatasetKey::parse(&dataset);
    let now = state.clock.now_ns();
    let actor = created_by(&principal);
    let created_by = actor.clone();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                update_item_tx(conn, project, key, item_id, patch, created_by, now).await
            })
        })
        .await?;
    match outcome {
        Ok((item, version)) => {
            if let Some(version) = version {
                tracing::info!(
                    item_id = %item.item_id,
                    dataset_id = %item.dataset_id,
                    project_id = %project,
                    actor = %actor.as_deref().unwrap_or("-"),
                    version,
                    "dataset item edited"
                );
            }
            Ok(error::Json(item_json(&item)))
        }
        Err(WriteOutcome::NotFound) => Err(ApiError::NotFound),
        Err(WriteOutcome::Invalid(errors)) => Err(bad_request(errors)),
    }
}

/// The values an edit makes of a live revision: the fields the patch sets replace the live ones.
fn edited_values(live: &ItemRow, patch: ItemPatch) -> ItemValues {
    ItemValues {
        input: patch.input.unwrap_or_else(|| live.input.clone()),
        expected_output: match patch.expected_output {
            None => live.expected_output.clone(),
            Some(value) => value,
        },
        metadata: patch.metadata.unwrap_or_else(|| live.metadata.clone()),
    }
}

/// Whether `values` are the live revision's own (an edit that changes nothing writes no version).
fn is_unchanged(live: &ItemRow, values: &ItemValues) -> bool {
    values.input == live.input
        && values.expected_output == live.expected_output
        && values.metadata == live.metadata
}

/// The item edit's write transaction. A body that changes nothing writes no version and answers
/// the live revision with `None` (D-J-3). Otherwise the live revision closes and its replacement
/// is inserted in one new version, which is answered with the revision and the version; the
/// source ids of the item are kept (D-J-9).
async fn update_item_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    key: DatasetKey,
    item: Uuid,
    patch: ItemPatch,
    created_by: Option<String>,
    now: i64,
) -> Result<Result<(ItemRow, Option<i64>), WriteOutcome>, MetaError> {
    let Some(row) = store::get_dataset_tx(conn, project, &key).await? else {
        return Ok(Err(WriteOutcome::NotFound));
    };
    let Some(live) = store::get_live_item_tx(conn, row.id, item).await? else {
        return Ok(Err(WriteOutcome::NotFound));
    };
    let values = edited_values(&live, patch);
    if is_unchanged(&live, &values) {
        return Ok(Ok((live, None)));
    }
    let schemas = compiled_schemas(&row)?;
    let errors = check_item("", &values, &schemas);
    if !errors.is_empty() {
        return Ok(Err(WriteOutcome::Invalid(errors)));
    }
    let change = VersionChange {
        close: vec![item],
        insert: vec![NewRevision {
            revision_id: Uuid::now_v7(),
            item_id: item,
            input: values.input,
            expected_output: values.expected_output,
            metadata: values.metadata,
            source_trace_id: live.source_trace_id,
            source_span_id: live.source_span_id,
        }],
        description: format!("edit item {item}"),
        created_by,
    };
    let version = store::write_version(conn, row.id, change, now).await?;
    let updated = store::get_live_item_tx(conn, row.id, item)
        .await?
        .ok_or(MetaError::Corrupt {
            table: "dataset_items",
            column: "item_id",
        })?;
    Ok(Ok((updated, Some(version))))
}

async fn delete_item(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(ItemPath { dataset, item_id }): error::Path<ItemPath>,
) -> Result<error::Json<DatasetVersion>, ApiError> {
    let project = access.project.id;
    let key = DatasetKey::parse(&dataset);
    let now = state.clock.now_ns();
    let actor = created_by(&principal);
    let created_by = actor.clone();
    let deleted = state
        .meta
        .write(move |conn| {
            Box::pin(
                async move { delete_item_tx(conn, project, key, item_id, created_by, now).await },
            )
        })
        .await?;
    let Some((dataset_id, version)) = deleted else {
        return Err(ApiError::NotFound);
    };
    tracing::info!(
        item_id = %item_id,
        dataset_id = %dataset_id,
        project_id = %project,
        actor = %actor.as_deref().unwrap_or("-"),
        version = version.version,
        "dataset item deleted"
    );
    Ok(error::Json(version))
}

/// The item delete's write transaction: the live revision closes in one new version that has
/// no revision of its own. `None` when there is no such dataset or no live such item.
async fn delete_item_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    key: DatasetKey,
    item: Uuid,
    created_by: Option<String>,
    now: i64,
) -> Result<Option<(Uuid, DatasetVersion)>, MetaError> {
    let Some(row) = store::get_dataset_tx(conn, project, &key).await? else {
        return Ok(None);
    };
    if store::get_live_item_tx(conn, row.id, item).await?.is_none() {
        return Ok(None);
    }
    let description = format!("delete item {item}");
    let change = VersionChange {
        close: vec![item],
        insert: Vec::new(),
        description: description.clone(),
        created_by: created_by.clone(),
    };
    let counts = VersionCounts::of_change(&change);
    let version = store::write_version(conn, row.id, change, now).await?;
    Ok(Some((
        row.id,
        DatasetVersion {
            version,
            description,
            created_by,
            created_at: now,
            added: counts.added,
            edited: counts.edited,
            deleted: counts.deleted,
        },
    )))
}

/// A batch must hold 1 to [`MAX_BATCH_ITEMS`] entries: a 400 on `field` (`noun` names them)
/// when it does not.
fn check_batch_len(len: usize, field: &str, noun: &str) -> Result<(), ApiError> {
    if (1..=MAX_BATCH_ITEMS).contains(&len) {
        Ok(())
    } else {
        Err(bad_request(vec![field_error(
            field,
            format!("must hold 1 to {MAX_BATCH_ITEMS} {noun}"),
        )]))
    }
}

/// One 400 per id that repeats an earlier id of the batch (D-J-23). Each entry pairs the field
/// that names the id with the id.
fn repeated_ids(ids: &[(String, Uuid)]) -> Vec<FieldError> {
    let mut seen = HashSet::new();
    ids.iter()
        .filter(|(_, id)| !seen.insert(*id))
        .map(|(field, _)| field_error(field, "appears more than once"))
        .collect()
}

/// `error` with its field moved under `prefix` (`items[3]` makes `input` into `items[3].input`).
fn prefixed(prefix: &str, error: FieldError) -> FieldError {
    FieldError {
        field: format!("{prefix}.{}", error.field),
        ..error
    }
}

/// `PATCH …/items` (D-J-23): edits of up to 1 000 live items in one new version. Every item is
/// checked before anything is written; an item that changes nothing is left out.
async fn edit_items(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(DatasetPath { dataset }): error::Path<DatasetPath>,
    error::Json(body): error::Json<EditItemsRequest>,
) -> Result<error::Json<EditItemsResponse>, ApiError> {
    check_batch_len(body.items.len(), "items", "items")?;
    let ids: Vec<(String, Uuid)> = body
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| (format!("items[{index}].id"), item.id))
        .collect();
    let mut errors = repeated_ids(&ids);
    let mut patches = Vec::new();
    for (index, item) in body.items.into_iter().enumerate() {
        let id = item.id;
        let request = UpdateItemRequest {
            input: item.input,
            expected_output: item.expected_output,
            metadata: item.metadata,
        };
        match check_item_patch(request) {
            Ok(patch) => patches.push((id, patch)),
            Err(found) => errors.extend(
                found
                    .into_iter()
                    .map(|error| prefixed(&format!("items[{index}]"), error)),
            ),
        }
    }
    if !errors.is_empty() {
        errors.truncate(MAX_LISTED_ERRORS);
        return Err(bad_request(errors));
    }
    let project = access.project.id;
    let key = DatasetKey::parse(&dataset);
    let now = state.clock.now_ns();
    let actor = created_by(&principal);
    let created_by = actor.clone();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(
                async move { edit_items_tx(conn, project, key, patches, created_by, now).await },
            )
        })
        .await?;
    let (dataset_id, response, changed) = match outcome {
        Ok(edited) => edited,
        Err(WriteOutcome::NotFound) => return Err(ApiError::NotFound),
        Err(WriteOutcome::Invalid(errors)) => return Err(bad_request(errors)),
    };
    if let Some(version) = &response.version {
        tracing::info!(
            dataset_id = %dataset_id,
            project_id = %project,
            actor = %actor.as_deref().unwrap_or("-"),
            count = changed,
            version = version.version,
            "dataset items edited"
        );
    }
    Ok(error::Json(response))
}

/// The batch edit's write transaction. `patches` holds every request item in request order, each
/// parsed, so the index of an item here is its index in the request. Answers the dataset's id,
/// the response and the number of changed items.
async fn edit_items_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    key: DatasetKey,
    patches: Vec<(Uuid, ItemPatch)>,
    created_by: Option<String>,
    now: i64,
) -> Result<Result<(Uuid, EditItemsResponse, usize), WriteOutcome>, MetaError> {
    let Some(row) = store::get_dataset_tx(conn, project, &key).await? else {
        return Ok(Err(WriteOutcome::NotFound));
    };
    let schemas = compiled_schemas(&row)?;
    let ids: Vec<Uuid> = patches.iter().map(|(id, _)| *id).collect();
    let mut errors = Vec::new();
    let mut close = Vec::new();
    let mut insert = Vec::new();
    for (index, (item, patch)) in patches.into_iter().enumerate() {
        let Some(live) = store::get_live_item_tx(conn, row.id, item).await? else {
            errors.push(field_error(&format!("items[{index}].id"), "no such item"));
            continue;
        };
        let values = edited_values(&live, patch);
        if is_unchanged(&live, &values) {
            continue;
        }
        errors.extend(check_item(&format!("items[{index}]"), &values, &schemas));
        close.push(item);
        insert.push(NewRevision {
            revision_id: Uuid::now_v7(),
            item_id: item,
            input: values.input,
            expected_output: values.expected_output,
            metadata: values.metadata,
            source_trace_id: live.source_trace_id,
            source_span_id: live.source_span_id,
        });
    }
    if !errors.is_empty() {
        errors.truncate(MAX_LISTED_ERRORS);
        return Ok(Err(WriteOutcome::Invalid(errors)));
    }
    let changed = close.len();
    let version = if changed == 0 {
        None
    } else {
        let description = match close.as_slice() {
            [item] => format!("edit item {item}"),
            _ => format!("edit {changed} items"),
        };
        let change = VersionChange {
            close,
            insert,
            description: description.clone(),
            created_by: created_by.clone(),
        };
        let counts = VersionCounts::of_change(&change);
        let number = store::write_version(conn, row.id, change, now).await?;
        Some(DatasetVersion {
            version: number,
            description,
            created_by,
            created_at: now,
            added: counts.added,
            edited: counts.edited,
            deleted: counts.deleted,
        })
    };
    let mut items = Vec::with_capacity(ids.len());
    for item in ids {
        let live =
            store::get_live_item_tx(conn, row.id, item)
                .await?
                .ok_or(MetaError::Corrupt {
                    table: "dataset_items",
                    column: "item_id",
                })?;
        items.push(item_json(&live));
    }
    Ok(Ok((row.id, EditItemsResponse { version, items }, changed)))
}

/// `POST …/items/delete` (D-J-23): closes up to 1 000 live items in one new version.
async fn delete_items(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(DatasetPath { dataset }): error::Path<DatasetPath>,
    error::Json(body): error::Json<DeleteItemsRequest>,
) -> Result<error::Json<DatasetVersion>, ApiError> {
    check_batch_len(body.ids.len(), "ids", "ids")?;
    let ids: Vec<(String, Uuid)> = body
        .ids
        .iter()
        .enumerate()
        .map(|(index, id)| (format!("ids[{index}]"), *id))
        .collect();
    let errors = repeated_ids(&ids);
    if !errors.is_empty() {
        return Err(bad_request(errors));
    }
    let project = access.project.id;
    let key = DatasetKey::parse(&dataset);
    let now = state.clock.now_ns();
    let actor = created_by(&principal);
    let created_by = actor.clone();
    let ids = body.ids;
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move { delete_items_tx(conn, project, key, ids, created_by, now).await })
        })
        .await?;
    let (dataset_id, version, count) = match outcome {
        Ok(deleted) => deleted,
        Err(WriteOutcome::NotFound) => return Err(ApiError::NotFound),
        Err(WriteOutcome::Invalid(errors)) => return Err(bad_request(errors)),
    };
    tracing::info!(
        dataset_id = %dataset_id,
        project_id = %project,
        actor = %actor.as_deref().unwrap_or("-"),
        count,
        version = version.version,
        "dataset items deleted"
    );
    Ok(error::Json(version))
}

/// The batch delete's write transaction: every id must be a live item, or nothing is written.
async fn delete_items_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    key: DatasetKey,
    ids: Vec<Uuid>,
    created_by: Option<String>,
    now: i64,
) -> Result<Result<(Uuid, DatasetVersion, usize), WriteOutcome>, MetaError> {
    let Some(row) = store::get_dataset_tx(conn, project, &key).await? else {
        return Ok(Err(WriteOutcome::NotFound));
    };
    let mut errors = Vec::new();
    for (index, id) in ids.iter().enumerate() {
        if store::get_live_item_tx(conn, row.id, *id).await?.is_none() {
            errors.push(field_error(&format!("ids[{index}]"), "no such item"));
        }
    }
    if !errors.is_empty() {
        errors.truncate(MAX_LISTED_ERRORS);
        return Ok(Err(WriteOutcome::Invalid(errors)));
    }
    let count = ids.len();
    let description = match ids.as_slice() {
        [item] => format!("delete item {item}"),
        _ => format!("delete {count} items"),
    };
    let change = VersionChange {
        close: ids,
        insert: Vec::new(),
        description: description.clone(),
        created_by: created_by.clone(),
    };
    let counts = VersionCounts::of_change(&change);
    let version = store::write_version(conn, row.id, change, now).await?;
    Ok(Ok((
        row.id,
        DatasetVersion {
            version,
            description,
            created_by,
            created_at: now,
            added: counts.added,
            edited: counts.edited,
            deleted: counts.deleted,
        },
        count,
    )))
}

/// `POST …/restore` (D-J-24): makes the live items those of an earlier version, as one new
/// version. Live items the version lacks, or holds a different revision of, are closed; items the
/// version holds and the live set lacks, or holds differently, get a new revision copying its
/// content and source ids under the same item id.
async fn restore_version(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(DatasetPath { dataset }): error::Path<DatasetPath>,
    error::Json(body): error::Json<RestoreRequest>,
) -> Result<error::Json<DatasetVersion>, ApiError> {
    let project = access.project.id;
    let key = DatasetKey::parse(&dataset);
    let now = state.clock.now_ns();
    let actor = created_by(&principal);
    let created_by = actor.clone();
    let target = body.version;
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move { restore_tx(conn, project, key, target, created_by, now).await })
        })
        .await?;
    let (dataset_id, version, changed) = match outcome {
        Ok(restored) => restored,
        Err(WriteOutcome::NotFound) => return Err(ApiError::NotFound),
        Err(WriteOutcome::Invalid(errors)) => return Err(bad_request(errors)),
    };
    tracing::info!(
        dataset_id = %dataset_id,
        project_id = %project,
        actor = %actor.as_deref().unwrap_or("-"),
        count = changed,
        version = version.version,
        restored_version = target,
        "dataset restored"
    );
    Ok(error::Json(version))
}

/// One page of the items of `version` (`None`: the live items), inside a write transaction.
async fn page_items_tx(
    conn: &mut SqliteConnection,
    dataset: Uuid,
    version: Option<i64>,
    after: Option<Uuid>,
) -> Result<Vec<ItemRow>, MetaError> {
    match version {
        Some(version) => store::version_items_tx(conn, dataset, version, after, RESTORE_PAGE).await,
        None => store::live_items_tx(conn, dataset, after, RESTORE_PAGE).await,
    }
}

/// Every item of `version` (`None`: every live item), ordered by item id, read in pages.
async fn all_items_tx(
    conn: &mut SqliteConnection,
    dataset: Uuid,
    version: Option<i64>,
) -> Result<Vec<ItemRow>, MetaError> {
    let mut all = Vec::new();
    let mut after = None;
    loop {
        let mut page = page_items_tx(conn, dataset, version, after).await?;
        let more = page.len() > RESTORE_PAGE as usize;
        page.truncate(RESTORE_PAGE as usize);
        let Some(last) = page.last().map(|row| row.item_id) else {
            break;
        };
        all.extend(page);
        if !more {
            break;
        }
        after = Some(last);
    }
    Ok(all)
}

/// The restore's write transaction (D-J-24). Answers the dataset id, the new version and the
/// number of items it changed. A target outside the earlier versions, or one that changes
/// nothing, is a 400 on `version` and writes nothing; a restored item that breaks the current
/// schemas is a 400 on its `items[<id>]` field.
async fn restore_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    key: DatasetKey,
    target: i64,
    created_by: Option<String>,
    now: i64,
) -> Result<Result<(Uuid, DatasetVersion, usize), WriteOutcome>, MetaError> {
    let Some(row) = store::get_dataset_tx(conn, project, &key).await? else {
        return Ok(Err(WriteOutcome::NotFound));
    };
    if !(0..row.current_version).contains(&target) {
        return Ok(Err(WriteOutcome::Invalid(vec![field_error(
            "version",
            format!(
                "must be an earlier version, 0 to {}",
                row.current_version - 1
            ),
        )])));
    }
    let schemas = compiled_schemas(&row)?;
    let live = all_items_tx(conn, row.id, None).await?;
    let wanted = all_items_tx(conn, row.id, Some(target)).await?;
    let live_revisions: HashMap<Uuid, Uuid> = live
        .iter()
        .map(|item| (item.item_id, item.revision_id))
        .collect();
    let wanted_revisions: HashMap<Uuid, Uuid> = wanted
        .iter()
        .map(|item| (item.item_id, item.revision_id))
        .collect();
    let close: Vec<Uuid> = live
        .iter()
        .filter(|item| wanted_revisions.get(&item.item_id) != Some(&item.revision_id))
        .map(|item| item.item_id)
        .collect();
    let mut errors = Vec::new();
    let mut insert = Vec::new();
    for item in &wanted {
        if live_revisions.get(&item.item_id) == Some(&item.revision_id) {
            continue;
        }
        let values = ItemValues {
            input: item.input.clone(),
            expected_output: item.expected_output.clone(),
            metadata: item.metadata.clone(),
        };
        errors.extend(check_item(
            &format!("items[{}]", item.item_id),
            &values,
            &schemas,
        ));
        insert.push(NewRevision {
            revision_id: Uuid::now_v7(),
            item_id: item.item_id,
            input: values.input,
            expected_output: values.expected_output,
            metadata: values.metadata,
            source_trace_id: item.source_trace_id,
            source_span_id: item.source_span_id,
        });
    }
    if !errors.is_empty() {
        errors.truncate(MAX_LISTED_ERRORS);
        return Ok(Err(WriteOutcome::Invalid(errors)));
    }
    if close.is_empty() && insert.is_empty() {
        return Ok(Err(WriteOutcome::Invalid(vec![field_error(
            "version",
            "the latest version already holds these items",
        )])));
    }
    let description = format!("restore version {target}");
    let change = VersionChange {
        close,
        insert,
        description: description.clone(),
        created_by: created_by.clone(),
    };
    let counts = VersionCounts::of_change(&change);
    let number = store::write_version(conn, row.id, change, now).await?;
    let changed = usize::try_from(counts.added + counts.edited + counts.deleted).unwrap_or(0);
    Ok(Ok((
        row.id,
        DatasetVersion {
            version: number,
            description,
            created_by,
            created_at: now,
            added: counts.added,
            edited: counts.edited,
            deleted: counts.deleted,
        },
        changed,
    )))
}

/// The head of an import file, checked against the dataset's schemas and not written (D-J-6).
async fn preview_import(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(DatasetPath { dataset }): error::Path<DatasetPath>,
    error::Query(params): error::Query<PreviewParams>,
    body: Body,
) -> Result<error::Json<ImportPreview>, ApiError> {
    let project = access.project.id;
    // Reading the dataset proves the caller may see it; the preview uses no schema.
    let row = find(&state, project, &dataset).await?;
    // Read here, not by an extractor, so the body is under the upload deadline (D-J-6).
    let bytes = upload_deadline(
        axum::body::to_bytes(body, PREVIEW_BODY_LIMIT),
        UPLOAD_TIMEOUT,
    )
    .await?
    .map_err(preview_body_failed)?;
    tracing::debug!(
        dataset_id = %row.id,
        project_id = %project,
        bytes = bytes.len(),
        format = ?params.format,
        "dataset import preview"
    );
    let format = params.format;
    let partial = params.partial;
    let head = tokio::task::spawn_blocking(move || import::preview(&bytes, format, partial))
        .await
        .map_err(task_failed)?
        .map_err(file_error)?;
    Ok(error::Json(ImportPreview {
        format,
        columns: head.columns,
        rows: head.rows,
        errors: head.errors,
        truncated: head.truncated,
    }))
}

/// Imports a CSV or JSONL file as one new version (D-J-6). Every row is checked first; a file
/// with no valid row writes nothing.
async fn import_items(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(DatasetPath { dataset }): error::Path<DatasetPath>,
    error::Query(params): error::Query<ImportParams>,
    multipart: Result<Multipart, MultipartRejection>,
) -> Result<(StatusCode, error::Json<ImportResult>), ApiError> {
    let multipart = multipart.map_err(|_| ApiError::UnsupportedMediaType {
        detail: "the import is multipart/form-data with a file part",
    })?;
    let project = access.project.id;
    let used = find(&state, project, &dataset).await?;
    // Taken before any of the file is read: the permits bound the memory of the imports.
    let _permit = import_permit(&IMPORTS)?;
    let parts = upload_deadline(read_parts(multipart), UPLOAD_TIMEOUT).await??;
    let format = params.format;
    let mapping = match (format, parts.mapping) {
        (ImportFormat::Csv, Some(mapping)) => Some(mapping),
        (ImportFormat::Csv, None) => {
            return Err(bad_request(vec![field_error(
                "mapping",
                "required for CSV",
            )]));
        }
        (ImportFormat::Jsonl, Some(_)) => {
            return Err(bad_request(vec![field_error(
                "mapping",
                "JSONL imports take no mapping",
            )]));
        }
        (ImportFormat::Jsonl, None) => None,
    };
    let file_name = parts.file_name;
    let file = parts.file;
    let checked_against = used.clone();
    let checked =
        tokio::task::spawn_blocking(move || check_file(&checked_against, mapping.as_ref(), &file))
            .await
            .map_err(task_failed)??
            .map_err(file_error)?;
    let Checked { items, errors } = checked;
    let imported = i64::try_from(items.len()).unwrap_or(i64::MAX);
    let error_count = i64::try_from(errors.len()).unwrap_or(i64::MAX);
    if items.is_empty() {
        let listed = errors
            .iter()
            .take(import::MAX_LISTED_ERRORS)
            .map(|error| field_error(&format!("rows[{}]", error.row), &error.message))
            .collect();
        return Err(ApiError::BadRequest {
            detail: Some("no row could be imported".to_owned()),
            errors: listed,
        });
    }
    let actor = created_by(&principal);
    let insert = items
        .into_iter()
        .map(|item| NewRevision {
            revision_id: Uuid::now_v7(),
            item_id: Uuid::now_v7(),
            input: item.input,
            expected_output: item.expected_output,
            metadata: item.metadata,
            source_trace_id: None,
            source_span_id: None,
        })
        .collect();
    let change = VersionChange {
        close: Vec::new(),
        insert,
        description: format!("import {file_name} ({imported} items)"),
        created_by: actor.clone(),
    };
    let version = write_checked(&state, project, &used, change, None).await?;
    tracing::info!(
        dataset_id = %used.id,
        project_id = %project,
        actor = %actor.as_deref().unwrap_or("-"),
        version,
        imported,
        rejected = error_count,
        format = ?format,
        "dataset items imported"
    );
    let listed = errors.into_iter().take(import::MAX_LISTED_ERRORS).collect();
    Ok((
        StatusCode::CREATED,
        error::Json(ImportResult {
            version,
            imported,
            error_count,
            errors: listed,
        }),
    ))
}

/// The rows of a file that passed their checks, and every rejected row.
struct Checked {
    items: Vec<ItemValues>,
    errors: Vec<ImportRowError>,
}

/// Parses `bytes` (CSV with `mapping`, or JSONL) and checks every row against `used`'s schemas.
/// Runs on a blocking thread. The outer `Err` is a stored schema that no longer compiles.
fn check_file(
    used: &DatasetRow,
    mapping: Option<&ImportMapping>,
    bytes: &[u8],
) -> Result<Result<Checked, FileError>, MetaError> {
    let schemas = Schemas::of(used)?;
    let parsed = match mapping {
        Some(mapping) => import::parse_csv(bytes, mapping),
        None => import::parse_jsonl(bytes),
    };
    let parsed = match parsed {
        Ok(parsed) => parsed,
        Err(error) => return Ok(Err(error)),
    };
    let mut errors = parsed.errors;
    let mut items = Vec::with_capacity(parsed.items.len());
    for (row, item) in parsed.items {
        match check_item("", &item, &schemas).first() {
            None => items.push(item),
            Some(error) => errors.push(ImportRowError {
                row,
                message: format!("{}: {}", error.field, error.message),
            }),
        }
    }
    errors.sort_by_key(|error| error.row);
    Ok(Ok(Checked { items, errors }))
}

/// What the write transaction of [`write_checked`] found.
enum CheckedWrite {
    Written(i64),
    Gone,
    SchemaChanged,
    Blocked,
}

/// Writes `change` as one version of the dataset `used`, after re-reading the dataset in the
/// write transaction: gone is 404; a changed `input_schema` or `expected_schema` is a 409 and
/// nothing is written (D-J-2: the rows were checked against `used`). With `guard = Some((trace,
/// users))` (add-from-trace) the transaction first asks the deletion guard whether a deletion
/// blocks a copy of `trace` and answers 404 when one does, nothing written (D-J-9). The import
/// passes `None`.
async fn write_checked(
    state: &AppState,
    project: ProjectId,
    used: &DatasetRow,
    change: VersionChange,
    guard: Option<(TraceId, Vec<(String, i64)>)>,
) -> Result<i64, ApiError> {
    let key = DatasetKey::Id(used.id);
    let input_schema = used.input_schema.clone();
    let expected_schema = used.expected_schema.clone();
    let now = state.clock.now_ns();
    let deletions = state.clone();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                if let Some((trace, users)) = &guard {
                    let users = execute::borrowed_users(users);
                    if deletions
                        .deletions
                        .guard
                        .blocking(project, *trace, &users, now)
                        .is_some()
                    {
                        return Ok(CheckedWrite::Blocked);
                    }
                }
                let Some(row) = store::get_dataset_tx(conn, project, &key).await? else {
                    return Ok(CheckedWrite::Gone);
                };
                if row.input_schema != input_schema || row.expected_schema != expected_schema {
                    return Ok(CheckedWrite::SchemaChanged);
                }
                let version = store::write_version(conn, row.id, change, now).await?;
                Ok(CheckedWrite::Written(version))
            })
        })
        .await?;
    match outcome {
        CheckedWrite::Written(version) => Ok(version),
        CheckedWrite::Gone | CheckedWrite::Blocked => Err(ApiError::NotFound),
        CheckedWrite::SchemaChanged => Err(ApiError::Conflict {
            detail: "the dataset's schema changed; retry",
        }),
    }
}

/// A 400 on one id field of a `from-trace` request.
fn bad_id(field: &str, message: &str) -> ApiError {
    bad_request(vec![field_error(field, message)])
}

/// The answer of the add-from-trace route for a failed trace load (D-J-8, D-I-23).
fn trace_load_failed(error: LoadError) -> ApiError {
    match error {
        LoadError::NotFound | LoadError::NoTargetSpan => ApiError::NotFound,
        LoadError::EvaluationTrace => bad_request(vec![field_error(
            "trace_id",
            "an evaluation trace cannot be added to a dataset",
        )]),
        LoadError::TooLarge => bad_request(vec![field_error(
            "trace_id",
            "the trace is too large to load",
        )]),
        LoadError::Busy | LoadError::Saturated => ApiError::Unavailable {
            detail: "storage is busy; retry in a few seconds",
        },
        LoadError::Failed(error) => ApiError::Internal(Box::new(error)),
        LoadError::Convert(error) => ApiError::Internal(Box::new(error)),
    }
}

/// The item a trace copy makes (D-J-8): the source span (`span`, or the trace root), its input
/// and output as `span.input?` and `span.output?` decode them, and the request's replacements.
/// A truncated trace is refused, since its root may be a span that was skipped. Pure: no store.
fn item_from(
    loaded: &LoadedTrace,
    span: Option<SpanId>,
    request: &FromTraceRequest,
) -> Result<(SpanId, ItemValues), ApiError> {
    if loaded.truncated {
        return Err(bad_request(vec![field_error(
            "trace_id",
            "the trace is too large to copy",
        )]));
    }
    let source = match span {
        Some(span) => span,
        None => TraceData::new(&loaded.spans, None)
            .root_span_id()
            .ok_or(ApiError::NotFound)?,
    };
    let mapping = VariableMapping::from_json(&json!({
        "input": "span.input?",
        "output": "span.output?",
    }))
    .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let variables = resolve(
        &mapping,
        Some(&TraceData::new(&loaded.spans, Some(source))),
        None,
    )
    .map_err(|error| ApiError::Internal(Box::new(error)))?;
    let decoded = |name: &str| -> Value {
        variables
            .iter()
            .find(|(key, _)| *key == name)
            .map_or(Value::Null, |(_, value)| value.clone())
    };
    let input = match &request.input {
        Some(input) => input.clone(),
        None => decoded("input"),
    };
    if input.is_null() {
        return Err(bad_request(vec![field_error(
            "input",
            "the span has no input",
        )]));
    }
    let expected_output = match &request.expected_output {
        Some(explicit) => explicit.clone(),
        None => Some(decoded("output")).filter(|value| !value.is_null()),
    };
    let metadata = request.metadata.clone().unwrap_or_default();
    Ok((
        source,
        ItemValues {
            input,
            expected_output,
            metadata,
        },
    ))
}

/// `POST …/items/from-trace` (D-J-8, D-J-9): one item copied from a trace or one of its spans,
/// as a new version. A deletion that blocks the trace answers 404 and writes nothing.
async fn add_from_trace(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(DatasetPath { dataset }): error::Path<DatasetPath>,
    error::Json(body): error::Json<FromTraceRequest>,
) -> Result<(StatusCode, error::Json<DatasetItem>), ApiError> {
    let trace_id = body
        .trace_id
        .parse::<TraceId>()
        .map_err(|_| bad_id("trace_id", "must be 32 hex digits"))?;
    let span_id = match body.span_id.as_deref() {
        Some(text) => Some(
            text.parse::<SpanId>()
                .map_err(|_| bad_id("span_id", "must be 16 hex digits"))?,
        ),
        None => None,
    };
    let project = access.project.id;
    let used = find(&state, project, &dataset).await?;
    let loaded = trace_load::load_trace(
        &state,
        project,
        Visibility::window(&state, access.project.org_id),
        trace_id,
        span_id,
        None,
    )
    .await
    .map_err(trace_load_failed)?;
    let (source, values) = item_from(&loaded, span_id, &body)?;
    let errors = check_item("", &values, &Schemas::of(&used)?);
    if !errors.is_empty() {
        return Err(bad_request(errors));
    }
    let item_id = Uuid::now_v7();
    let actor = created_by(&principal);
    let users = execute::users_of(&loaded.spans);
    let change = VersionChange {
        close: Vec::new(),
        insert: vec![NewRevision {
            revision_id: Uuid::now_v7(),
            item_id,
            input: values.input,
            expected_output: values.expected_output,
            metadata: values.metadata,
            source_trace_id: Some(trace_id),
            source_span_id: Some(source),
        }],
        description: format!("add from trace {}", trace_id.to_hex()),
        created_by: actor.clone(),
    };
    let version = write_checked(&state, project, &used, change, Some((trace_id, users))).await?;
    tracing::info!(
        dataset_id = %used.id,
        project_id = %project,
        actor = %actor.as_deref().unwrap_or("-"),
        version,
        item_id = %item_id,
        "dataset item added from a trace"
    );
    let row = store::get_item(state.meta.reader(), used.id, item_id, version)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok((StatusCode::CREATED, error::Json(item_json(&row))))
}

/// The most datasets one trends request names (D-J-26).
const MAX_TREND_DATASETS: usize = 100;
/// Experiments per dataset in the trends answer (D-J-26).
const TRENDS_PER_DATASET: u32 = 10;

/// The dataset ids of a trends request: 1 to 100 distinct UUIDs, comma-separated.
fn parse_trend_ids(text: &str) -> Result<Vec<Uuid>, ApiError> {
    let ids = text
        .split(',')
        .map(|part| part.trim().parse::<Uuid>())
        .collect::<Result<Vec<Uuid>, _>>()
        .map_err(|_| bad_request(vec![field_error("ids", "must be comma-separated UUIDs")]))?;
    if !(1..=MAX_TREND_DATASETS).contains(&ids.len()) {
        return Err(bad_request(vec![field_error(
            "ids",
            format!("must hold 1 to {MAX_TREND_DATASETS} ids"),
        )]));
    }
    let mut seen = HashSet::new();
    if !ids.iter().all(|id| seen.insert(*id)) {
        return Err(bad_request(vec![field_error("ids", "must be distinct")]));
    }
    Ok(ids)
}

/// The score means of `experiments` (D-J-26), keyed by experiment id text, from one storage
/// read. `None` when storage is busy or the read times out: the trends then go without scores.
async fn trend_scores(
    state: &AppState,
    org: OrgId,
    project: ProjectId,
    experiments: &[experiment_store::TrendRow],
) -> Result<Option<HashMap<String, Vec<ExperimentScoreMean>>>, ApiError> {
    let Some(oldest) = experiments.iter().map(|row| row.created_at).min() else {
        return Ok(Some(HashMap::new()));
    };
    let ids: Vec<String> = experiments.iter().map(|row| row.id.to_string()).collect();
    let since = oldest.saturating_sub(CLOCK_SKEW_NS);
    let read = async {
        let ctx = open_query(state, org, project, None).await?;
        cetra_storage::experiment_score_means(&ctx, &ids, since, Deadline::start().remaining()?)
            .await
            .map_err(ApiError::from)
    }
    .await;
    let means = match read {
        Ok(means) => means,
        Err(ApiError::Unavailable { .. } | ApiError::Query(RunError::Timeout)) => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut grouped: HashMap<String, Vec<ExperimentScoreMean>> = HashMap::new();
    for mean in means {
        grouped
            .entry(mean.experiment_id)
            .or_default()
            .push(ExperimentScoreMean {
                name: mean.name,
                data_type: mean.data_type,
                mean: mean.mean,
                count: mean.count,
            });
    }
    for scores in grouped.values_mut() {
        scores.sort_by(|left, right| {
            (&left.name, &left.data_type).cmp(&(&right.name, &right.data_type))
        });
    }
    Ok(Some(grouped))
}

/// `GET …/dataset-trends` (D-J-26): for each dataset of `ids` in the project, its latest visible
/// experiments with their result and error counts, and their score means. Its own path, so a
/// dataset named `experiment-trends` is still reached by `…/datasets/{dataset}`.
async fn experiment_trends(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<ExperimentTrendParams>,
) -> Result<error::Json<ExperimentTrends>, ApiError> {
    let requested = parse_trend_ids(&params.ids)?;
    let project = access.project.id;
    let org = access.project.org_id;
    let known: HashSet<Uuid> = store::list_datasets(state.meta.reader(), project, None)
        .await?
        .into_iter()
        .map(|row| row.id)
        .collect();
    let datasets: Vec<Uuid> = requested
        .into_iter()
        .filter(|id| known.contains(id))
        .collect();
    let rows = experiment_store::trends(
        state.meta.reader(),
        project,
        &datasets,
        visible_from(&state, org),
        TRENDS_PER_DATASET,
    )
    .await?;
    let scores = trend_scores(&state, org, project, &rows).await?;
    let data = datasets
        .iter()
        .map(|dataset| DatasetTrend {
            dataset_id: *dataset,
            experiments: rows
                .iter()
                .filter(|row| row.dataset_id == *dataset)
                .map(|row| ExperimentTrend {
                    id: row.id,
                    name: row.name.clone(),
                    status: row.status.clone(),
                    created_at: row.created_at,
                    result_count: row.result_count,
                    error_count: row.error_count,
                    scores: scores.as_ref().map(|grouped| {
                        grouped
                            .get(&row.id.to_string())
                            .cloned()
                            .unwrap_or_default()
                    }),
                })
                .collect(),
        })
        .collect();
    Ok(error::Json(ExperimentTrends {
        data,
        scores_unavailable: scores.is_none(),
    }))
}

/// The most traces one bulk add copies (D-J-25).
const MAX_TRACE_BATCH: usize = 100;

/// Why a trace of a bulk add was not copied, in words for the request's `skipped` list.
fn load_refusal(error: &LoadError) -> &'static str {
    match error {
        LoadError::EvaluationTrace => "an evaluation trace cannot be added to a dataset",
        LoadError::TooLarge => "the trace is too large to load",
        _ => "the trace or span was not found",
    }
}

/// Why `item_from` refused a trace of a bulk add, or the error itself when it is not a refusal of
/// the trace (the caller answers that error for the whole request).
fn copy_refusal(error: ApiError) -> Result<String, ApiError> {
    match error {
        ApiError::BadRequest { errors, .. } => Ok(errors.first().map_or_else(
            || "the trace cannot be copied".to_owned(),
            |error| format!("{}: {}", error.field, error.message),
        )),
        ApiError::NotFound => Ok("the trace or span was not found".to_owned()),
        other => Err(other),
    }
}

/// A trace of a bulk add that passed its checks, ready to be written.
struct CopyPlan {
    index: usize,
    trace_id: TraceId,
    source: SpanId,
    values: ItemValues,
    users: Vec<(String, i64)>,
}

/// The write transaction of a bulk add: what it found and wrote.
enum CopiesWrite {
    Gone,
    SchemaChanged,
    /// No copy was left after the deletion guard; the listed indices were blocked.
    Nothing {
        blocked: Vec<usize>,
    },
    Written {
        version: DatasetVersion,
        items: Vec<Uuid>,
        blocked: Vec<usize>,
    },
}

/// `POST …/items/from-traces` (D-J-25): copies up to 100 traces as `from-trace` does, in one new
/// version. A trace that cannot be copied is listed in `skipped`; storage busy is a 503 for the
/// whole call, and no copyable trace is a 400 with the reasons as `items[i]` field errors.
async fn add_from_traces(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(DatasetPath { dataset }): error::Path<DatasetPath>,
    error::Json(body): error::Json<FromTracesRequest>,
) -> Result<(StatusCode, error::Json<FromTracesResult>), ApiError> {
    let result = add_trace_refs(
        &state,
        &principal,
        &access,
        &dataset,
        &body.items,
        &Map::new(),
    )
    .await?;
    Ok((StatusCode::CREATED, error::Json(result)))
}

/// The core of both bulk adds (D-J-25, D-IC-22): copies the traces of `items` into `dataset` as one
/// new version, giving every item `metadata`. Writes the audit line.
pub(crate) async fn add_trace_refs(
    state: &AppState,
    principal: &Principal,
    access: &ProjectAccess,
    dataset: &str,
    items: &[TraceRef],
    metadata: &Map<String, Value>,
) -> Result<FromTracesResult, ApiError> {
    if !(1..=MAX_TRACE_BATCH).contains(&items.len()) {
        return Err(bad_request(vec![field_error(
            "items",
            format!("must hold 1 to {MAX_TRACE_BATCH} items"),
        )]));
    }
    let mut refs = Vec::new();
    let mut errors = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let trace = item.trace_id.parse::<TraceId>();
        let span = match item.span_id.as_deref() {
            Some(text) => text.parse::<SpanId>().map(Some),
            None => Ok(None),
        };
        match (trace, span) {
            (Ok(trace), Ok(span)) => refs.push((index, trace, span)),
            (Err(_), _) => errors.push(field_error(
                &format!("items[{index}].trace_id"),
                "must be 32 hex digits",
            )),
            (Ok(_), Err(_)) => errors.push(field_error(
                &format!("items[{index}].span_id"),
                "must be 16 hex digits",
            )),
        }
    }
    if !errors.is_empty() {
        return Err(bad_request(errors));
    }

    let project = access.project.id;
    let used = find(state, project, dataset).await?;
    let schemas = Schemas::of(&used)?;
    let mut copies: Vec<CopyPlan> = Vec::new();
    let mut skipped: Vec<(usize, String)> = Vec::new();
    for (index, trace_id, span_id) in refs {
        let text = &items[index];
        let loaded = match trace_load::load_trace(
            state,
            project,
            Visibility::window(state, access.project.org_id),
            trace_id,
            span_id,
            None,
        )
        .await
        {
            Ok(loaded) => loaded,
            Err(
                error @ (LoadError::NotFound
                | LoadError::NoTargetSpan
                | LoadError::EvaluationTrace
                | LoadError::TooLarge),
            ) => {
                skipped.push((index, load_refusal(&error).to_owned()));
                continue;
            }
            Err(error) => return Err(trace_load_failed(error)),
        };
        // The same request shape `from-trace` reads, without overrides.
        let request = FromTraceRequest {
            trace_id: text.trace_id.clone(),
            span_id: text.span_id.clone(),
            input: None,
            expected_output: None,
            metadata: (!metadata.is_empty()).then(|| metadata.clone()),
        };
        let (source, values) = match item_from(&loaded, span_id, &request) {
            Ok(copied) => copied,
            Err(error) => {
                skipped.push((index, copy_refusal(error)?));
                continue;
            }
        };
        if let Some(first) = check_item("", &values, &schemas).first() {
            skipped.push((index, format!("{}: {}", first.field, first.message)));
            continue;
        }
        let users = execute::users_of(&loaded.spans);
        copies.push(CopyPlan {
            index,
            trace_id,
            source,
            values,
            users,
        });
    }
    if copies.is_empty() {
        return Err(refusals(&skipped));
    }

    let actor = created_by(principal);
    let created_by = actor.clone();
    let now = state.clock.now_ns();
    let key = DatasetKey::Id(used.id);
    let input_schema = used.input_schema.clone();
    let expected_schema = used.expected_schema.clone();
    let deletions = state.clone();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let Some(row) = store::get_dataset_tx(conn, project, &key).await? else {
                    return Ok(CopiesWrite::Gone);
                };
                if row.input_schema != input_schema || row.expected_schema != expected_schema {
                    return Ok(CopiesWrite::SchemaChanged);
                }
                // The deletion guard is asked for every trace inside the transaction (D-J-9).
                let mut blocked = Vec::new();
                let mut inserts = Vec::new();
                let mut traces = Vec::new();
                let mut items = Vec::new();
                for copy in copies {
                    let users = execute::borrowed_users(&copy.users);
                    if deletions
                        .deletions
                        .guard
                        .blocking(project, copy.trace_id, &users, now)
                        .is_some()
                    {
                        blocked.push(copy.index);
                        continue;
                    }
                    let item_id = Uuid::now_v7();
                    items.push(item_id);
                    traces.push(copy.trace_id);
                    inserts.push(NewRevision {
                        revision_id: Uuid::now_v7(),
                        item_id,
                        input: copy.values.input,
                        expected_output: copy.values.expected_output,
                        metadata: copy.values.metadata,
                        source_trace_id: Some(copy.trace_id),
                        source_span_id: Some(copy.source),
                    });
                }
                if inserts.is_empty() {
                    return Ok(CopiesWrite::Nothing { blocked });
                }
                // One written trace gets the single-trace form, as `from-trace` writes it (D-J-25).
                let description = match traces.as_slice() {
                    [trace] => format!("add from trace {}", trace.to_hex()),
                    _ => format!("add from {} traces", traces.len()),
                };
                let change = VersionChange {
                    close: Vec::new(),
                    insert: inserts,
                    description: description.clone(),
                    created_by: created_by.clone(),
                };
                let counts = VersionCounts::of_change(&change);
                let version = store::write_version(conn, row.id, change, now).await?;
                Ok(CopiesWrite::Written {
                    version: DatasetVersion {
                        version,
                        description,
                        created_by,
                        created_at: now,
                        added: counts.added,
                        edited: counts.edited,
                        deleted: counts.deleted,
                    },
                    items,
                    blocked,
                })
            })
        })
        .await?;
    let (version, item_ids, blocked) = match outcome {
        CopiesWrite::Gone => return Err(ApiError::NotFound),
        CopiesWrite::SchemaChanged => {
            return Err(ApiError::Conflict {
                detail: "the dataset's schema changed; retry",
            });
        }
        CopiesWrite::Nothing { blocked } => {
            skipped.extend(
                blocked
                    .into_iter()
                    .map(|index| (index, "a deletion blocks this trace".to_owned())),
            );
            skipped.sort_by_key(|(index, _)| *index);
            return Err(refusals(&skipped));
        }
        CopiesWrite::Written {
            version,
            items,
            blocked,
        } => (version, items, blocked),
    };
    skipped.extend(
        blocked
            .into_iter()
            .map(|index| (index, "a deletion blocks this trace".to_owned())),
    );
    skipped.sort_by_key(|(index, _)| *index);
    let mut copied = Vec::with_capacity(item_ids.len());
    for item_id in &item_ids {
        let row = store::get_item(state.meta.reader(), used.id, *item_id, version.version)
            .await?
            .ok_or(ApiError::NotFound)?;
        copied.push(item_json(&row));
    }
    tracing::info!(
        dataset_id = %used.id,
        project_id = %project,
        actor = %actor.as_deref().unwrap_or("-"),
        version = version.version,
        count = copied.len(),
        skipped = skipped.len(),
        "dataset items added from traces"
    );
    let skipped = skipped
        .into_iter()
        .map(|(index, message)| SkippedTrace {
            index: i64::try_from(index).unwrap_or(i64::MAX),
            trace_id: items[index].trace_id.clone(),
            message,
        })
        .collect();
    Ok(FromTracesResult {
        version,
        items: copied,
        skipped,
    })
}

/// The 400 of a bulk add that copied no trace: each refusal as an `items[i]` field error.
fn refusals(skipped: &[(usize, String)]) -> ApiError {
    let errors = skipped
        .iter()
        .map(|(index, message)| field_error(&format!("items[{index}]"), message))
        .take(MAX_LISTED_ERRORS)
        .collect();
    bad_request(errors)
}

/// One permit of [`IMPORTS`], or 503 when every one is taken.
fn import_permit(imports: &Semaphore) -> Result<SemaphorePermit<'_>, ApiError> {
    imports.try_acquire().map_err(|_| ApiError::Unavailable {
        detail: "another import is running; retry in a few seconds",
    })
}

/// The parts of an import upload. `Debug` prints sizes only.
struct Parts {
    file_name: String,
    file: Vec<u8>,
    mapping: Option<ImportMapping>,
}

impl fmt::Debug for Parts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Parts")
            .field("file_bytes", &self.file.len())
            .field("mapping", &self.mapping.is_some())
            .finish()
    }
}

/// Reads the `file` part (at most 100 MiB) and the optional `mapping` part (at most 1 MiB).
async fn read_parts(mut multipart: Multipart) -> Result<Parts, ApiError> {
    let mut file: Option<(String, Vec<u8>)> = None;
    let mut mapping: Option<ImportMapping> = None;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|error| multipart_failed(&error))?
    {
        let name = field.name().unwrap_or_default().to_owned();
        match name.as_str() {
            "file" => {
                if file.is_some() {
                    return Err(bad_request(vec![field_error(&name, "part repeated")]));
                }
                let file_name = clean_file_name(field.file_name());
                let mut bytes = Vec::new();
                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|error| multipart_failed(&error))?
                {
                    if bytes.len() + chunk.len() > import::MAX_FILE_BYTES {
                        return Err(too_large_file());
                    }
                    bytes.extend_from_slice(&chunk);
                }
                file = Some((file_name, bytes));
            }
            "mapping" => {
                if mapping.is_some() {
                    return Err(bad_request(vec![field_error(&name, "part repeated")]));
                }
                let mut bytes = Vec::new();
                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|error| multipart_failed(&error))?
                {
                    if bytes.len() + chunk.len() > MAPPING_BYTES {
                        return Err(bad_request(vec![field_error(
                            "mapping",
                            "the mapping is larger than 1 MiB",
                        )]));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                mapping = Some(serde_json::from_slice(&bytes).map_err(|_| {
                    bad_request(vec![field_error("mapping", "invalid mapping JSON")])
                })?);
            }
            _ => return Err(bad_request(vec![field_error(&name, "unknown part")])),
        }
    }
    let Some((file_name, file)) = file else {
        return Err(bad_request(vec![field_error("file", "required")]));
    };
    Ok(Parts {
        file_name,
        file,
        mapping,
    })
}

/// The file name as stored in the version description: control, zero-width, bidi and line or
/// paragraph separator characters removed, cut to 200 characters; `upload` when absent or empty
/// after that.
fn clean_file_name(name: Option<&str>) -> String {
    let cleaned: String = name
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control() && !is_format_control(*c))
        .take(200)
        .collect();
    if cleaned.is_empty() {
        "upload".to_owned()
    } else {
        cleaned
    }
}

/// `fut`, or 400 `the upload took too long` after `limit`.
async fn upload_deadline<T>(fut: impl Future<Output = T>, limit: Duration) -> Result<T, ApiError> {
    tokio::time::timeout(limit, fut)
        .await
        .map_err(|_| ApiError::BadRequest {
            detail: Some("the upload took too long".to_owned()),
            errors: Vec::new(),
        })
}

/// Zero-width spaces and marks (U+200B–U+200F), line and paragraph separators and bidi
/// embedding and isolate controls (U+2028–U+202E, U+2066–U+2069).
fn is_format_control(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}' | '\u{2028}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

/// A preview body that cannot be read: 413 past 1 MiB, 400 otherwise.
fn preview_body_failed(error: axum::Error) -> ApiError {
    if error.into_inner().is::<http_body_util::LengthLimitError>() {
        ApiError::PayloadTooLarge {
            detail: "a preview takes at most 1 MiB",
        }
    } else {
        bad_request_text("the body cannot be read")
    }
}

fn multipart_failed(error: &MultipartError) -> ApiError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        too_large_file()
    } else {
        bad_request_text("the multipart body cannot be read")
    }
}

fn too_large_file() -> ApiError {
    ApiError::PayloadTooLarge {
        detail: "the file is larger than 100 MiB",
    }
}

/// A file that cannot be read or checked as a whole (a 400).
fn file_error(error: FileError) -> ApiError {
    let (detail, errors) = match error {
        FileError::Mapping(errors) => ("invalid mapping".to_owned(), errors),
        FileError::Header(message) => (message, Vec::new()),
        FileError::TooManyRows => ("the file has more than 50000 rows".to_owned(), Vec::new()),
    };
    ApiError::BadRequest {
        detail: Some(detail),
        errors,
    }
}

/// A blocking task that did not finish (it panicked or was cancelled) is a server failure.
fn task_failed(error: tokio::task::JoinError) -> ApiError {
    ApiError::Internal(Box::new(error))
}

fn bad_request_text(text: &str) -> ApiError {
    ApiError::BadRequest {
        detail: Some(text.to_owned()),
        errors: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::*;
    use crate::auth::roles::OrgRole;
    use crate::test_support::{
        Seeded, TestApp, TestResponse, capture_logs, dataset_span, set_limits,
    };

    const FAQ_SCHEMA: &str =
        r#"{"type":"object","required":["question"],"properties":{"question":{"type":"string"}}}"#;

    fn base(seeded: &Seeded) -> String {
        format!("/api/v1/projects/{}/datasets", seeded.project.id)
    }

    async fn create(app: &TestApp, seeded: &Seeded, body: Value) -> TestResponse {
        app.request(Method::POST, &base(seeded))
            .header("cookie", &seeded.cookie)
            .json(&body)
            .send()
            .await
    }

    async fn create_ok(app: &TestApp, seeded: &Seeded, name: &str) -> Value {
        let response = create(app, seeded, json!({ "name": name })).await;
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        response.json()
    }

    /// `path` is a dataset segment (`faq`, an id), a query (`?limit=2`) or empty (the list).
    async fn get(app: &TestApp, seeded: &Seeded, path: &str) -> TestResponse {
        let separator = if path.is_empty() || path.starts_with('?') {
            ""
        } else {
            "/"
        };
        app.request(Method::GET, &format!("{}{separator}{path}", base(seeded)))
            .header("cookie", &seeded.cookie)
            .send()
            .await
    }

    async fn patch(app: &TestApp, seeded: &Seeded, dataset: &str, body: Value) -> TestResponse {
        app.request(Method::PATCH, &format!("{}/{dataset}", base(seeded)))
            .header("cookie", &seeded.cookie)
            .json(&body)
            .send()
            .await
    }

    /// The `(field, message)` pairs of a 400 answer.
    fn field_errors(response: &TestResponse) -> Vec<(String, String)> {
        assert_eq!(
            response.status,
            400,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let body: Value = response.json();
        body["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|error| {
                (
                    error["field"].as_str().unwrap().to_owned(),
                    error["message"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    /// Adds `inputs` as live items of `dataset` in one version; returns their item ids.
    async fn add_items(app: &TestApp, dataset: &str, inputs: Vec<Value>) -> Vec<Uuid> {
        let dataset: Uuid = dataset.parse().unwrap();
        let now = app.state.clock.now_ns();
        let revisions: Vec<NewRevision> = inputs
            .into_iter()
            .map(|input| NewRevision {
                revision_id: Uuid::now_v7(),
                item_id: Uuid::now_v7(),
                input,
                expected_output: None,
                metadata: Map::new(),
                source_trace_id: None,
                source_span_id: None,
            })
            .collect();
        let ids = revisions.iter().map(|revision| revision.item_id).collect();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let change = VersionChange {
                        close: Vec::new(),
                        insert: revisions,
                        description: "add items".to_owned(),
                        created_by: None,
                    };
                    store::write_version(conn, dataset, change, now).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        ids
    }

    /// Closes `item` in a new version of `dataset`, with no revision.
    async fn close_item(app: &TestApp, dataset: &str, item: Uuid) {
        let dataset: Uuid = dataset.parse().unwrap();
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let change = VersionChange {
                        close: vec![item],
                        insert: Vec::new(),
                        description: format!("delete item {item}"),
                        created_by: None,
                    };
                    store::write_version(conn, dataset, change, now).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_get_list_delete() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let response = create(
            &app,
            &seeded,
            json!({ "name": "faq", "input_schema": { "type": "object" } }),
        )
        .await;
        assert_eq!(response.status, 201);
        let created: Value = response.json();
        assert_eq!(created["current_version"], 0);
        assert_eq!(created["item_count"], 0);
        assert_eq!(created["input_schema"], json!({ "type": "object" }));
        let id = created["id"].as_str().unwrap().to_owned();

        let by_id: Value = get(&app, &seeded, &id).await.json();
        let by_name: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(by_id, created);
        assert_eq!(by_name, created);

        let list: Value = get(&app, &seeded, "").await.json();
        assert_eq!(list["data"].as_array().unwrap().len(), 1);

        let deleted = app
            .request(Method::DELETE, &format!("{}/{id}", base(&seeded)))
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        assert_eq!(deleted.status, 204);
        assert_eq!(get(&app, &seeded, &id).await.status, 404);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_validates() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let uuid_name = create(
            &app,
            &seeded,
            json!({ "name": "01900000-0000-7000-8000-000000000001" }),
        )
        .await;
        assert_eq!(
            field_errors(&uuid_name),
            vec![("name".to_owned(), "must not be a UUID".to_owned())]
        );

        let bad_schema = create(
            &app,
            &seeded,
            json!({ "name": "x", "input_schema": { "type": 5 } }),
        )
        .await;
        let errors = field_errors(&bad_schema);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, "input_schema");

        let unknown = create(&app, &seeded, json!({ "name": "x", "extra": 1 })).await;
        assert_eq!(unknown.status, 400);

        create_ok(&app, &seeded, "faq").await;
        let duplicate = create(&app, &seeded, json!({ "name": "faq" })).await;
        assert_eq!(duplicate.status, 409);
        let problem: Value = duplicate.json();
        assert_eq!(problem["detail"], "a dataset with this name already exists");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_filters_and_pages() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        for name in ["Beta", "alpha", "gamma"] {
            create_ok(&app, &seeded, name).await;
        }
        let names = |body: &Value| -> Vec<String> {
            body["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["name"].as_str().unwrap().to_owned())
                .collect()
        };

        let filtered: Value = get(&app, &seeded, "?q=ET").await.json();
        assert_eq!(names(&filtered), vec!["Beta"]);

        let blank: Value = get(&app, &seeded, "?q=%20").await.json();
        assert_eq!(names(&blank), vec!["alpha", "Beta", "gamma"]);

        let first: Value = get(&app, &seeded, "?limit=2").await.json();
        assert_eq!(names(&first), vec!["alpha", "Beta"]);
        let cursor = first["next_cursor"].as_str().unwrap().to_owned();

        let second: Value = get(&app, &seeded, &format!("?limit=2&cursor={cursor}"))
            .await
            .json();
        assert_eq!(names(&second), vec!["gamma"]);
        assert!(second["next_cursor"].is_null());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn patch_fields_and_clears() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;

        let set: Value = patch(&app, &seeded, "faq", json!({ "description": "d" }))
            .await
            .json();
        assert_eq!(set["description"], "d");

        let cleared: Value = patch(&app, &seeded, "faq", json!({ "description": null }))
            .await
            .json();
        assert!(cleared.get("description").is_none());

        let renamed = patch(&app, &seeded, "faq", json!({ "name": "renamed" })).await;
        assert_eq!(renamed.status, 200);
        assert_eq!(get(&app, &seeded, "renamed").await.status, 200);

        let untouched = patch(&app, &seeded, "renamed", json!({})).await;
        assert_eq!(untouched.status, 200);
        let body: Value = untouched.json();
        assert_eq!(body["name"], "renamed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn schema_change_checks_live_items() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let dataset = create_ok(&app, &seeded, "faq").await;
        let id = dataset["id"].as_str().unwrap().to_owned();
        let items = add_items(
            &app,
            &id,
            vec![json!({ "question": "a" }), json!({ "q": "b" })],
        )
        .await;
        let bad = items[1];
        let schema: Value = serde_json::from_str(FAQ_SCHEMA).unwrap();

        let refused = patch(&app, &seeded, &id, json!({ "input_schema": schema })).await;
        assert_eq!(
            field_errors(&refused),
            vec![(format!("items[{bad}].input"), "/: required".to_owned())]
        );
        let unchanged: Value = get(&app, &seeded, &id).await.json();
        assert!(unchanged.get("input_schema").is_none());

        close_item(&app, &id, bad).await;
        let accepted = patch(&app, &seeded, &id, json!({ "input_schema": schema })).await;
        assert_eq!(
            accepted.status,
            200,
            "{}",
            String::from_utf8_lossy(&accepted.body)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn schema_check_is_capped() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let dataset = create_ok(&app, &seeded, "big").await;
        let id = dataset["id"].as_str().unwrap().to_owned();
        // Each item is just under 1 MiB, so 70 of them are over the 64 MiB check cap.
        let text = "a".repeat(1024 * 1024 - 100);
        add_items(
            &app,
            &id,
            (0..70).map(|_| json!({ "question": text })).collect(),
        )
        .await;

        let schema = json!({ "type": "object", "required": ["question"] });
        let refused = patch(&app, &seeded, &id, json!({ "input_schema": schema })).await;
        assert_eq!(
            field_errors(&refused),
            vec![(
                "input_schema".to_owned(),
                "the dataset is too large to validate against a new schema".to_owned()
            )]
        );
        let unchanged: Value = get(&app, &seeded, &id).await.json();
        assert!(unchanged.get("input_schema").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn roles() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let viewer = app
            .add_member(&seeded, "v@example.com", OrgRole::Viewer)
            .await;
        let read = app
            .request(Method::GET, &base(&seeded))
            .header("cookie", &viewer.cookie)
            .send()
            .await;
        assert_eq!(read.status, 200);
        let write = app
            .request(Method::POST, &base(&seeded))
            .header("cookie", &viewer.cookie)
            .json(&json!({ "name": "nope" }))
            .send()
            .await;
        assert_eq!(write.status, 403);
        assert_eq!(
            create(&app, &seeded, json!({ "name": "yes" })).await.status,
            201
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keys() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let read_header = format!("Bearer {}", seeded.read_key);
        let read = app
            .request(Method::GET, &base(&seeded))
            .header("authorization", &read_header)
            .send()
            .await;
        assert_eq!(read.status, 200);
        let write = app
            .request(Method::POST, &base(&seeded))
            .header("authorization", &read_header)
            .json(&json!({ "name": "nope" }))
            .send()
            .await;
        assert_eq!(write.status, 403);

        let write_key = app.write_key(&seeded).await;
        let write = app
            .request(Method::POST, &base(&seeded))
            .header("authorization", &format!("Bearer {write_key}"))
            .json(&json!({ "name": "yes" }))
            .send()
            .await;
        assert_eq!(write.status, 201);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn personal_tokens() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let read_write = app.personal_token(seeded.user_id, true).await;
        let created = app
            .request(Method::POST, &base(&seeded))
            .header("authorization", &format!("Bearer {read_write}"))
            .json(&json!({ "name": "pat" }))
            .send()
            .await;
        assert_eq!(created.status, 201);

        let read_only = app.personal_token(seeded.user_id, false).await;
        let read = app
            .request(Method::GET, &base(&seeded))
            .header("authorization", &format!("Bearer {read_only}"))
            .send()
            .await;
        assert_eq!(read.status, 200);
        let write = app
            .request(Method::POST, &base(&seeded))
            .header("authorization", &format!("Bearer {read_only}"))
            .json(&json!({ "name": "nope" }))
            .send()
            .await;
        assert_eq!(write.status, 403);
    }

    #[tokio::test]
    async fn audit_log_names_the_key() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let write_key = app.write_key(&seeded).await;
        let ((status, id), logs) = capture_logs(async {
            let response = app
                .request(Method::POST, &base(&seeded))
                .header("authorization", &format!("Bearer {write_key}"))
                .json(&json!({ "name": "canary-dataset-name" }))
                .send()
                .await;
            let id = response.json::<Value>()["id"].as_str().unwrap().to_owned();
            (response.status, id)
        })
        .await;
        assert_eq!(status, 201);
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT action, actor_kind, target_id, details FROM audit_events \
             WHERE action = 'dataset.created'",
        )
        .fetch_all(app.state.meta.reader())
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        let (_, actor_kind, target_id, details) = &rows[0];
        assert_eq!(actor_kind, "api_key");
        assert_eq!(target_id, &id);
        assert_eq!(details, "{}");
        assert!(!logs.contains("canary-dataset-name"), "{logs}");
        assert!(!details.contains("canary-dataset-name"), "{details}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dataset_export_is_recorded_before_streaming() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let response = get(&app, &seeded, "faq/export").await;
        assert_eq!(response.status, 200);

        // The record exists once the response is back, so before any item was streamed.
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT actor_kind, target_kind, details FROM audit_events \
             WHERE action = 'dataset.export_started'",
        )
        .fetch_all(app.state.meta.reader())
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        let (actor_kind, target_kind, details) = &rows[0];
        assert_eq!(actor_kind, "user");
        assert_eq!(target_kind, "dataset");
        let details: Value = serde_json::from_str(details).unwrap();
        assert!(details["version"].is_u64(), "{details}");
        assert_eq!(details["format"], "jsonl");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_audit_write_refuses_the_dataset_export() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, "faq").await;
        let dataset = created["id"].as_str().unwrap().to_owned();
        add_items(&app, &dataset, vec![json!({ "text": "canary-row" })]).await;
        // The record cannot be written, so the export must not start.
        app.state
            .meta
            .write(|conn| {
                Box::pin(async move {
                    sqlx::query("DROP TABLE audit_events")
                        .execute(&mut *conn)
                        .await
                        .map_err(MetaError::from)?;
                    Ok::<_, MetaError>(())
                })
            })
            .await
            .unwrap();

        let response = get(&app, &seeded, "faq/export").await;
        assert_eq!(response.status, 500);
        let body = String::from_utf8_lossy(&response.body);
        assert!(!body.contains("canary-row"), "{body}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn other_projects_datasets_are_404() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (other, _key) = app.add_project(&seeded, "playground", None).await;
        let other_id = other.id;
        let now = app.state.clock.now_ns();
        let fields = DatasetFields {
            name: "elsewhere".to_owned(),
            description: None,
            input_schema: None,
            expected_schema: None,
        };
        let id = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let id = Uuid::now_v7();
                    store::insert_dataset(conn, id, other_id, fields, now).await?;
                    Ok(id)
                })
            })
            .await
            .unwrap();
        assert_eq!(get(&app, &seeded, &id.to_string()).await.status, 404);
    }

    /// `POST …/faq/items` (or another `dataset` segment) with `body`.
    async fn post_items(
        app: &TestApp,
        seeded: &Seeded,
        dataset: &str,
        body: Value,
    ) -> TestResponse {
        app.request(Method::POST, &format!("{}/{dataset}/items", base(seeded)))
            .header("cookie", &seeded.cookie)
            .json(&body)
            .send()
            .await
    }

    /// `PATCH …/{dataset}/items/{item}` with `body`.
    async fn patch_item(
        app: &TestApp,
        seeded: &Seeded,
        dataset: &str,
        item: &str,
        body: Value,
    ) -> TestResponse {
        app.request(
            Method::PATCH,
            &format!("{}/{dataset}/items/{item}", base(seeded)),
        )
        .header("cookie", &seeded.cookie)
        .json(&body)
        .send()
        .await
    }

    /// The item ids of a `201` batch answer, as strings.
    fn item_ids(response: &TestResponse) -> Vec<String> {
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let body: Value = response.json();
        body["item_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn export_jsonl_of_a_past_version() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let added = post_items(
            &app,
            &seeded,
            "faq",
            json!({ "items": [{ "input": { "q": "a" } }, { "input": { "q": "b" } }] }),
        )
        .await;
        let ids = item_ids(&added);
        let edited = patch_item(
            &app,
            &seeded,
            "faq",
            &ids[0],
            json!({ "input": { "q": "a2" } }),
        )
        .await;
        assert_eq!(edited.status, 200);
        let third = post_items(
            &app,
            &seeded,
            "faq",
            json!({ "items": [{ "input": { "q": "c" } }] }),
        )
        .await;
        assert_eq!(item_ids(&third).len(), 1);

        let response = get(&app, &seeded, "faq/export?version=1").await;
        assert_eq!(response.status, 200);
        assert_eq!(
            response.header("content-type"),
            Some("application/x-ndjson")
        );
        assert_eq!(response.header("cache-control"), Some("no-store"));
        let disposition = response.header("content-disposition").unwrap();
        assert!(disposition.ends_with("-v1.jsonl\""), "{disposition}");
        let body = String::from_utf8(response.body.to_vec()).unwrap();
        let lines: Vec<Value> = body
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        let first = lines
            .iter()
            .find(|line| line["id"] == ids[0].as_str())
            .unwrap();
        assert_eq!(first["input"], json!({ "q": "a" }));
        assert!(first.get("expected_output").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn export_csv() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        post_items(
            &app,
            &seeded,
            "faq",
            json!({ "items": [
                { "input": { "question": "a" }, "expected_output": "x", "metadata": { "t": "x" } },
                { "input": { "question": "b" } }
            ] }),
        )
        .await;

        let response = get(&app, &seeded, "faq/export?format=csv").await;
        assert_eq!(response.status, 200);
        assert_eq!(
            response.header("content-type"),
            Some("text/csv; charset=utf-8")
        );
        assert!(response.body.starts_with(CSV_HEADER));
        // Every line ends with CRLF, the header's terminator: no bare LF.
        let lines = response.body.iter().filter(|byte| **byte == b'\n').count();
        let crlf = response
            .body
            .windows(2)
            .filter(|pair| pair[0] == b'\r' && pair[1] == b'\n')
            .count();
        assert_eq!((lines, crlf), (3, 3));
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .from_reader(response.body.as_ref());
        let records: Vec<csv::StringRecord> = reader.records().collect::<Result<_, _>>().unwrap();
        assert_eq!(records.len(), 3);
        let header: Vec<&str> = records[0].iter().collect();
        assert_eq!(
            header,
            [
                "id",
                "input",
                "expected_output",
                "metadata",
                "source_trace_id",
                "source_span_id"
            ]
        );
        let with_output = records[1..]
            .iter()
            .find(|record| &record[2] == "\"x\"")
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&with_output[1]).unwrap(),
            json!({ "question": "a" })
        );
        assert_eq!(
            serde_json::from_str::<Value>(&with_output[3]).unwrap(),
            json!({ "t": "x" })
        );
        assert_eq!(records[1..].iter().filter(|r| r[2].is_empty()).count(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn export_pages() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, "faq").await;
        let dataset = created["id"].as_str().unwrap().to_owned();
        add_items(
            &app,
            &dataset,
            (0..2_500).map(|n| json!({ "n": n })).collect(),
        )
        .await;

        let response = get(&app, &seeded, "faq/export?format=jsonl").await;
        assert_eq!(response.status, 200);
        let body = String::from_utf8(response.body.to_vec()).unwrap();
        let ids: Vec<String> = body
            .lines()
            .map(|line| {
                let line: Value = serde_json::from_str(line).unwrap();
                line["id"].as_str().unwrap().to_owned()
            })
            .collect();
        assert_eq!(ids.len(), 2_500);
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn export_empty_dataset() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let response = get(&app, &seeded, "faq/export?format=csv").await;
        assert_eq!(response.status, 200);
        assert_eq!(response.body.as_ref(), CSV_HEADER);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_trims_the_needle() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        create_ok(&app, &seeded, "other").await;
        let trailing: Value = get(&app, &seeded, "?q=faq%20").await.json();
        assert_eq!(trailing["data"][0]["name"], "faq");
        assert_eq!(trailing["data"].as_array().unwrap().len(), 1);
        let padded: Value = get(&app, &seeded, "?q=%20%20FAQ%20").await.json();
        assert_eq!(padded["data"].as_array().unwrap().len(), 1);
        let blank: Value = get(&app, &seeded, "?q=%20%20").await.json();
        assert_eq!(blank["data"].as_array().unwrap().len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn export_rejects_unknown_parameters() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let response = get(&app, &seeded, "faq/export?format=csv&colour=red").await;
        assert_eq!(response.status, 400);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn export_bad_version() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let response = get(&app, &seeded, "faq/export?version=99").await;
        assert_eq!(response.status, 400);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn export_with_a_read_key() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let response = app
            .request(Method::GET, &format!("{}/faq/export", base(&seeded)))
            .header("authorization", &format!("Bearer {}", seeded.read_key))
            .send()
            .await;
        assert_eq!(response.status, 200);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn export_source_ids() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, "faq").await;
        let dataset: Uuid = created["id"].as_str().unwrap().parse().unwrap();
        let now = app.state.clock.now_ns();
        let trace: TraceId = "a1000000000000000000000000000001".parse().unwrap();
        let span: SpanId = "b100000000000011".parse().unwrap();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let change = VersionChange {
                        close: Vec::new(),
                        insert: vec![NewRevision {
                            revision_id: Uuid::now_v7(),
                            item_id: Uuid::now_v7(),
                            input: json!("x"),
                            expected_output: None,
                            metadata: Map::new(),
                            source_trace_id: Some(trace),
                            source_span_id: Some(span),
                        }],
                        description: "add from trace".to_owned(),
                        created_by: None,
                    };
                    store::write_version(conn, dataset, change, now).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();

        let response = get(&app, &seeded, "faq/export").await;
        assert_eq!(response.status, 200);
        let line: Value =
            serde_json::from_slice(response.body.split(|b| *b == b'\n').next().unwrap()).unwrap();
        assert_eq!(line["source_trace_id"], "a1000000000000000000000000000001");
        assert_eq!(line["source_span_id"], "b100000000000011");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_list_and_version() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let schema: Value = serde_json::from_str(FAQ_SCHEMA).unwrap();
        let created = create(
            &app,
            &seeded,
            json!({ "name": "faq", "input_schema": schema }),
        )
        .await;
        assert_eq!(created.status, 201);

        let added = post_items(
            &app,
            &seeded,
            "faq",
            json!({ "items": [
                { "input": { "question": "a" } },
                { "input": { "question": "b" }, "expected_output": "x", "metadata": { "topic": "t" } }
            ] }),
        )
        .await;
        let ids = item_ids(&added);
        let body: Value = added.json();
        assert_eq!(body["version"], 1);
        assert_eq!(ids.len(), 2);

        let items: Value = get(&app, &seeded, "faq/items").await.json();
        let data = items["data"].as_array().unwrap();
        assert_eq!(data.len(), 2);
        assert!(data.iter().all(|row| row["version"] == 1));
        let mut listed: Vec<&str> = data.iter().map(|row| row["id"].as_str().unwrap()).collect();
        let mut expected: Vec<&str> = ids.iter().map(String::as_str).collect();
        listed.sort_unstable();
        expected.sort_unstable();
        assert_eq!(listed, expected);

        let versions: Value = get(&app, &seeded, "faq/versions").await.json();
        let versions = versions["data"].as_array().unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0]["description"], "add 2 items");
        assert_eq!(versions[0]["created_by"], seeded.user_id.to_string());

        let dataset: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(dataset["current_version"], 1);
        assert_eq!(dataset["item_count"], 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn add_validates_every_item() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let schema: Value = serde_json::from_str(FAQ_SCHEMA).unwrap();
        create(
            &app,
            &seeded,
            json!({ "name": "faq", "input_schema": schema }),
        )
        .await;

        let refused = post_items(
            &app,
            &seeded,
            "faq",
            json!({ "items": [ { "input": { "question": "ok" } }, { "input": { "q": 1 } } ] }),
        )
        .await;
        assert_eq!(
            field_errors(&refused),
            vec![("items[1].input".to_owned(), "/: required".to_owned())]
        );
        let dataset: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(dataset["current_version"], 0);

        let empty = post_items(&app, &seeded, "faq", json!({ "items": [] })).await;
        assert_eq!(
            field_errors(&empty),
            vec![("items".to_owned(), "must hold 1 to 1000 items".to_owned())]
        );

        let many: Vec<Value> = (0..1_001)
            .map(|_| json!({ "input": { "question": "a" } }))
            .collect();
        let too_many = post_items(&app, &seeded, "faq", json!({ "items": many })).await;
        assert_eq!(
            field_errors(&too_many),
            vec![("items".to_owned(), "must hold 1 to 1000 items".to_owned())]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn edit_creates_a_version_and_keeps_history() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let schema: Value = serde_json::from_str(FAQ_SCHEMA).unwrap();
        create(
            &app,
            &seeded,
            json!({ "name": "faq", "input_schema": schema }),
        )
        .await;
        let added = post_items(
            &app,
            &seeded,
            "faq",
            json!({ "items": [ { "input": { "question": "a" } } ] }),
        )
        .await;
        let id = item_ids(&added).remove(0);
        let item_path = format!("faq/items/{id}");
        let first: Value = get(&app, &seeded, &item_path).await.json();

        let edited = patch_item(
            &app,
            &seeded,
            "faq",
            &id,
            json!({ "expected_output": { "a": 1 } }),
        )
        .await;
        assert_eq!(edited.status, 200);
        let edited: Value = edited.json();
        assert_eq!(edited["id"], first["id"]);
        assert_eq!(edited["version"], 2);
        assert_ne!(edited["revision_id"], first["revision_id"]);
        assert_eq!(edited["expected_output"], json!({ "a": 1 }));

        let old: Value = get(&app, &seeded, &format!("{item_path}?version=1"))
            .await
            .json();
        assert_eq!(old["version"], 1);
        assert!(old.get("expected_output").is_none());

        // The same body again changes nothing: no version.
        let same = patch_item(
            &app,
            &seeded,
            "faq",
            &id,
            json!({ "expected_output": { "a": 1 } }),
        )
        .await;
        assert_eq!(same.status, 200);
        let same: Value = same.json();
        assert_eq!(same["revision_id"], edited["revision_id"]);
        let dataset: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(dataset["current_version"], 2);

        let cleared = patch_item(
            &app,
            &seeded,
            "faq",
            &id,
            json!({ "expected_output": null }),
        )
        .await;
        assert_eq!(cleared.status, 200);
        let cleared: Value = cleared.json();
        assert_eq!(cleared["version"], 3);
        assert!(cleared.get("expected_output").is_none());

        let refused = patch_item(&app, &seeded, "faq", &id, json!({ "input": { "q": 1 } })).await;
        assert_eq!(
            field_errors(&refused),
            vec![("input".to_owned(), "/: required".to_owned())]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_item_keeps_older_versions() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let id = item_ids(
            &post_items(
                &app,
                &seeded,
                "faq",
                json!({ "items": [ { "input": "a" } ] }),
            )
            .await,
        )
        .remove(0);
        let item_path = format!("faq/items/{id}");

        let deleted = app
            .request(Method::DELETE, &format!("{}/{item_path}", base(&seeded)))
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        assert_eq!(deleted.status, 200);
        let deleted: Value = deleted.json();
        assert_eq!(deleted["version"], 2);
        assert_eq!(deleted["description"], format!("delete item {id}"));

        assert_eq!(get(&app, &seeded, &item_path).await.status, 404);
        assert_eq!(
            get(&app, &seeded, &format!("{item_path}?version=1"))
                .await
                .status,
            200
        );
        let listed: Value = get(&app, &seeded, "faq/items").await.json();
        assert!(listed["data"].as_array().unwrap().is_empty());
        let dataset: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(dataset["item_count"], 0);

        let again = app
            .request(Method::DELETE, &format!("{}/{item_path}", base(&seeded)))
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        assert_eq!(again.status, 404);
        assert_eq!(
            patch_item(&app, &seeded, "faq", &id, json!({ "input": "b" }))
                .await
                .status,
            404
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn item_search_filters_the_listed_items() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        post_items(
            &app,
            &seeded,
            "faq",
            json!({ "items": [
                { "input": "Where is my ORDER" },
                { "input": "unrelated", "expected_output": "order shipped" },
                { "input": "plain", "metadata": { "channel": "email" } },
            ] }),
        )
        .await;

        let found: Value = get(&app, &seeded, "faq/items?q=%20order%20").await.json();
        assert_eq!(found["data"].as_array().unwrap().len(), 2);
        let by_key: Value = get(&app, &seeded, "faq/items?q=channel").await.json();
        assert_eq!(by_key["data"].as_array().unwrap().len(), 1);
        let none: Value = get(&app, &seeded, "faq/items?q=absent").await.json();
        assert!(none["data"].as_array().unwrap().is_empty());
        let all: Value = get(&app, &seeded, "faq/items").await.json();
        assert_eq!(all["data"].as_array().unwrap().len(), 3);

        let blank = format!("faq/items?q={}", "%20".repeat(3));
        assert_eq!(
            field_errors(&get(&app, &seeded, &blank).await),
            vec![("q".to_owned(), "must be 1 to 200 characters".to_owned())]
        );
        let long = format!("faq/items?q={}", "x".repeat(201));
        assert_eq!(
            field_errors(&get(&app, &seeded, &long).await),
            vec![("q".to_owned(), "must be 1 to 200 characters".to_owned())]
        );
        let longest = format!("faq/items?q={}", "x".repeat(200));
        assert_eq!(get(&app, &seeded, &longest).await.status, 200);
    }

    /// `PATCH …/{dataset}/items` (the batch edit) with `body`.
    async fn batch_edit(
        app: &TestApp,
        seeded: &Seeded,
        dataset: &str,
        body: Value,
    ) -> TestResponse {
        app.request(Method::PATCH, &format!("{}/{dataset}/items", base(seeded)))
            .header("cookie", &seeded.cookie)
            .json(&body)
            .send()
            .await
    }

    /// `POST …/{dataset}/items/delete` (the batch delete) with `body`.
    async fn batch_delete(
        app: &TestApp,
        seeded: &Seeded,
        dataset: &str,
        body: Value,
    ) -> TestResponse {
        app.request(
            Method::POST,
            &format!("{}/{dataset}/items/delete", base(seeded)),
        )
        .header("cookie", &seeded.cookie)
        .json(&body)
        .send()
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn batch_edit_writes_one_version_for_the_changed_items() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let ids = item_ids(
            &post_items(
                &app,
                &seeded,
                "faq",
                json!({ "items": [ { "input": "a" }, { "input": "b" }, { "input": "c" } ] }),
            )
            .await,
        );

        // Two of three change; the middle one is sent unchanged and is left out.
        let edited = batch_edit(
            &app,
            &seeded,
            "faq",
            json!({ "items": [
                { "id": ids[0], "input": "a2" },
                { "id": ids[1], "input": "b" },
                { "id": ids[2], "expected_output": "c-out" },
            ] }),
        )
        .await;
        assert_eq!(
            edited.status,
            200,
            "{}",
            String::from_utf8_lossy(&edited.body)
        );
        let edited: Value = edited.json();
        assert_eq!(edited["version"]["version"], 2);
        assert_eq!(edited["version"]["description"], "edit 2 items");
        let answered = edited["items"].as_array().unwrap();
        assert_eq!(
            answered
                .iter()
                .map(|item| item["id"].clone())
                .collect::<Vec<_>>(),
            ids.iter().map(|id| json!(id)).collect::<Vec<_>>()
        );
        assert_eq!(answered[0]["input"], "a2");
        assert_eq!(answered[0]["version"], 2);
        assert_eq!(
            answered[1]["version"], 1,
            "the unchanged item keeps its revision"
        );
        assert_eq!(answered[2]["expected_output"], "c-out");

        // Nothing changes: no version, the items as they are.
        let same = batch_edit(
            &app,
            &seeded,
            "faq",
            json!({ "items": [ { "id": ids[0], "input": "a2" } ] }),
        )
        .await;
        assert_eq!(same.status, 200);
        let same: Value = same.json();
        assert!(same["version"].is_null());
        assert_eq!(same["items"][0]["version"], 2);
        let dataset: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(dataset["current_version"], 2);

        // Exactly one changed item: the single-item description.
        let single = batch_edit(
            &app,
            &seeded,
            "faq",
            json!({ "items": [ { "id": ids[1], "input": "b2" } ] }),
        )
        .await;
        assert_eq!(single.status, 200);
        let single: Value = single.json();
        assert_eq!(single["version"]["version"], 3);
        assert_eq!(
            single["version"]["description"],
            format!("edit item {}", ids[1])
        );
        let before: Value = get(&app, &seeded, "faq/items?version=1").await.json();
        assert_eq!(before["data"][0]["input"], "a");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn batch_edit_refuses_a_bad_batch_and_writes_nothing() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let ids = item_ids(
            &post_items(
                &app,
                &seeded,
                "faq",
                json!({ "items": [ { "input": "a" } ] }),
            )
            .await,
        );
        let unknown = Uuid::now_v7().to_string();

        let missing = batch_edit(
            &app,
            &seeded,
            "faq",
            json!({ "items": [ { "id": ids[0], "input": "x" }, { "id": unknown, "input": "y" } ] }),
        )
        .await;
        assert_eq!(
            field_errors(&missing),
            vec![("items[1].id".to_owned(), "no such item".to_owned())]
        );

        let repeated = batch_edit(
            &app,
            &seeded,
            "faq",
            json!({ "items": [ { "id": ids[0], "input": "x" }, { "id": ids[0], "input": "y" } ] }),
        )
        .await;
        assert_eq!(
            field_errors(&repeated),
            vec![(
                "items[1].id".to_owned(),
                "appears more than once".to_owned()
            )]
        );

        let null_input = batch_edit(
            &app,
            &seeded,
            "faq",
            json!({ "items": [ { "id": ids[0], "input": null } ] }),
        )
        .await;
        assert_eq!(
            field_errors(&null_input),
            vec![("items[0].input".to_owned(), "must not be null".to_owned())]
        );

        let empty = batch_edit(&app, &seeded, "faq", json!({ "items": [] })).await;
        assert_eq!(
            field_errors(&empty),
            vec![("items".to_owned(), "must hold 1 to 1000 items".to_owned())]
        );

        let dataset: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(dataset["current_version"], 1);
        let kept: Value = get(&app, &seeded, &format!("faq/items/{}", ids[0]))
            .await
            .json();
        assert_eq!(kept["input"], "a");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn batch_edit_checks_each_item_against_the_schema() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let dataset = create(
            &app,
            &seeded,
            json!({ "name": "faq", "input_schema": serde_json::from_str::<Value>(FAQ_SCHEMA).unwrap() }),
        )
        .await;
        assert_eq!(dataset.status, 201);
        let ids = item_ids(
            &post_items(
                &app,
                &seeded,
                "faq",
                json!({ "items": [ { "input": { "question": "a" } } ] }),
            )
            .await,
        );

        let refused = batch_edit(
            &app,
            &seeded,
            "faq",
            json!({ "items": [ { "id": ids[0], "input": { "q": "b" } } ] }),
        )
        .await;
        let errors = field_errors(&refused);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].0, "items[0].input");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn batch_delete_closes_the_items_in_one_version() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let ids = item_ids(
            &post_items(
                &app,
                &seeded,
                "faq",
                json!({ "items": [ { "input": 1 }, { "input": 2 }, { "input": 3 } ] }),
            )
            .await,
        );

        let unknown = Uuid::now_v7().to_string();
        assert_eq!(
            field_errors(
                &batch_delete(&app, &seeded, "faq", json!({ "ids": [ids[0], unknown] })).await
            ),
            vec![("ids[1]".to_owned(), "no such item".to_owned())]
        );
        assert_eq!(
            field_errors(
                &batch_delete(&app, &seeded, "faq", json!({ "ids": [ids[0], ids[0]] })).await
            ),
            vec![("ids[1]".to_owned(), "appears more than once".to_owned())]
        );
        assert_eq!(
            field_errors(&batch_delete(&app, &seeded, "faq", json!({ "ids": [] })).await),
            vec![("ids".to_owned(), "must hold 1 to 1000 ids".to_owned())]
        );

        let deleted = batch_delete(&app, &seeded, "faq", json!({ "ids": [ids[0], ids[2]] })).await;
        assert_eq!(
            deleted.status,
            200,
            "{}",
            String::from_utf8_lossy(&deleted.body)
        );
        let deleted: Value = deleted.json();
        assert_eq!(deleted["version"], 2);
        assert_eq!(deleted["description"], "delete 2 items");

        let listed: Value = get(&app, &seeded, "faq/items").await.json();
        assert_eq!(listed["data"].as_array().unwrap().len(), 1);
        assert_eq!(listed["data"][0]["id"], json!(ids[1]));
        let before: Value = get(&app, &seeded, "faq/items?version=1").await.json();
        assert_eq!(before["data"].as_array().unwrap().len(), 3);

        // Deleted ids are no longer live.
        assert_eq!(
            field_errors(&batch_delete(&app, &seeded, "faq", json!({ "ids": [ids[0]] })).await),
            vec![("ids[0]".to_owned(), "no such item".to_owned())]
        );

        // Exactly one id: the single-item description.
        let one = batch_delete(&app, &seeded, "faq", json!({ "ids": [ids[1]] })).await;
        assert_eq!(one.status, 200);
        let one: Value = one.json();
        assert_eq!(one["version"], 3);
        assert_eq!(one["description"], format!("delete item {}", ids[1]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn versions_count_their_changes_and_list_them() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let ids = item_ids(
            &post_items(
                &app,
                &seeded,
                "faq",
                json!({ "items": [ { "input": "a" }, { "input": "b" }, { "input": "c" } ] }),
            )
            .await,
        );
        let (a, b, c) = (ids[0].clone(), ids[1].clone(), ids[2].clone());

        // v2: one item edited, through a batch of one.
        let edited = batch_edit(
            &app,
            &seeded,
            "faq",
            json!({ "items": [ { "id": a, "input": "a2" } ] }),
        )
        .await;
        assert_eq!(edited.status, 200);
        let edited: Value = edited.json();
        assert_eq!(edited["version"]["edited"], 1);
        assert_eq!(edited["version"]["added"], 0);
        assert_eq!(edited["version"]["deleted"], 0);

        // v3: one item deleted through the single-item route.
        let deleted = app
            .request(Method::DELETE, &format!("{}/faq/items/{b}", base(&seeded)))
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        assert_eq!(deleted.status, 200);
        let deleted: Value = deleted.json();
        assert_eq!(deleted["version"], 3);
        assert_eq!(deleted["deleted"], 1);
        assert_eq!(deleted["edited"], 0);

        // v4: two items edited in one batch.
        let edited = batch_edit(
            &app,
            &seeded,
            "faq",
            json!({ "items": [
                { "id": a, "input": "a3" },
                { "id": c, "expected_output": "c-out" },
            ] }),
        )
        .await;
        let edited: Value = edited.json();
        assert_eq!(edited["version"]["edited"], 2);

        // v5: one item deleted in a batch.
        let deleted = batch_delete(&app, &seeded, "faq", json!({ "ids": [c] })).await;
        assert_eq!(deleted.status, 200);
        let deleted: Value = deleted.json();
        assert_eq!(deleted["deleted"], 1);

        // Newest first, every version with its own counts.
        let versions: Value = get(&app, &seeded, "faq/versions").await.json();
        let counts: Vec<(i64, i64, i64, i64)> = versions["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|version| {
                (
                    version["version"].as_i64().unwrap(),
                    version["added"].as_i64().unwrap(),
                    version["edited"].as_i64().unwrap(),
                    version["deleted"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            counts,
            [
                (5, 0, 0, 1),
                (4, 0, 2, 0),
                (3, 0, 0, 1),
                (2, 0, 1, 0),
                (1, 3, 0, 0)
            ]
        );

        // v1 added all three: after only.
        let first: Value = get(&app, &seeded, "faq/versions/1/changes").await.json();
        let rows = first["data"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| row["change"] == "added"
            && row["before"].is_null()
            && !row["after"].is_null()));
        assert!(first["next_cursor"].is_null());

        // v3 deleted b: before only.
        let third: Value = get(&app, &seeded, "faq/versions/3/changes").await.json();
        let rows = third["data"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["item_id"], json!(b));
        assert_eq!(rows[0]["change"], "deleted");
        assert_eq!(rows[0]["before"]["input"], "b");
        assert!(rows[0]["after"].is_null());

        // v4 edited a and c: both sides, ordered by item id, each revision its own.
        let fourth: Value = get(&app, &seeded, "faq/versions/4/changes").await.json();
        let rows = fourth["data"].as_array().unwrap();
        let mut expected = [a.clone(), c.clone()];
        expected.sort();
        assert_eq!(
            rows.iter()
                .map(|row| row["item_id"].clone())
                .collect::<Vec<_>>(),
            expected.iter().map(|id| json!(id)).collect::<Vec<_>>()
        );
        let row_of = |id: &str| rows.iter().find(|row| row["item_id"] == json!(id)).unwrap();
        assert_eq!(row_of(&a)["change"], "edited");
        assert_eq!(row_of(&a)["before"]["input"], "a2");
        assert_eq!(row_of(&a)["after"]["input"], "a3");
        assert_eq!(row_of(&a)["after"]["version"], 4);
        assert_eq!(row_of(&c)["before"]["input"], "c");
        assert_eq!(row_of(&c)["after"]["expected_output"], "c-out");

        // v5 deleted c.
        let fifth: Value = get(&app, &seeded, "faq/versions/5/changes").await.json();
        assert_eq!(fifth["data"][0]["item_id"], json!(c));
        assert_eq!(fifth["data"][0]["change"], "deleted");

        // Paging by item id: two of the three added items, then the last one.
        let page: Value = get(&app, &seeded, "faq/versions/1/changes?limit=2")
            .await
            .json();
        assert_eq!(page["data"].as_array().unwrap().len(), 2);
        let cursor = page["next_cursor"].as_str().unwrap().to_owned();
        let rest: Value = get(
            &app,
            &seeded,
            &format!("faq/versions/1/changes?limit=2&cursor={cursor}"),
        )
        .await
        .json();
        assert_eq!(rest["data"].as_array().unwrap().len(), 1);
        assert!(rest["next_cursor"].is_null());

        // Versions outside 1 to the current one are 404; the limit is 1 to 100.
        assert_eq!(
            get(&app, &seeded, "faq/versions/0/changes").await.status,
            404
        );
        assert_eq!(
            get(&app, &seeded, "faq/versions/6/changes").await.status,
            404
        );
        assert_eq!(
            get(&app, &seeded, "faq/versions/1/changes?limit=101")
                .await
                .status,
            400
        );
        assert_eq!(
            get(&app, &seeded, "faq/versions/1/changes?cursor=nope")
                .await
                .status,
            400
        );
    }

    /// `POST …/{dataset}/restore` with `body`.
    async fn restore(app: &TestApp, seeded: &Seeded, dataset: &str, body: Value) -> TestResponse {
        app.request(Method::POST, &format!("{}/{dataset}/restore", base(seeded)))
            .header("cookie", &seeded.cookie)
            .json(&body)
            .send()
            .await
    }

    /// The live items of `dataset` as (id, input) pairs.
    async fn live_inputs(app: &TestApp, seeded: &Seeded, dataset: &str) -> Vec<(String, Value)> {
        let listed: Value = get(app, seeded, &format!("{dataset}/items")).await.json();
        listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                (
                    item["id"].as_str().unwrap().to_owned(),
                    item["input"].clone(),
                )
            })
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restore_takes_an_earlier_version_again() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let ids = item_ids(
            &post_items(
                &app,
                &seeded,
                "faq",
                json!({ "items": [ { "input": "a" }, { "input": "b" }, { "input": "c" } ] }),
            )
            .await,
        );
        let (a, b, c) = (ids[0].clone(), ids[1].clone(), ids[2].clone());
        // v2 edits a, v3 deletes b, v4 edits c.
        batch_edit(
            &app,
            &seeded,
            "faq",
            json!({ "items": [ { "id": a, "input": "a2" } ] }),
        )
        .await;
        batch_delete(&app, &seeded, "faq", json!({ "ids": [b] })).await;
        batch_edit(
            &app,
            &seeded,
            "faq",
            json!({ "items": [ { "id": c, "input": "c2" } ] }),
        )
        .await;

        // v5: the items of v1 again. a and c are closed and re-inserted, b is re-added.
        let restored = restore(&app, &seeded, "faq", json!({ "version": 1 })).await;
        assert_eq!(
            restored.status,
            200,
            "{}",
            String::from_utf8_lossy(&restored.body)
        );
        let restored: Value = restored.json();
        assert_eq!(restored["version"], 5);
        assert_eq!(restored["description"], "restore version 1");
        assert_eq!(restored["added"], 1);
        assert_eq!(restored["edited"], 2);
        assert_eq!(restored["deleted"], 0);

        let mut live = live_inputs(&app, &seeded, "faq").await;
        live.sort_by(|left, right| left.0.cmp(&right.0));
        let mut expected = vec![
            (a.clone(), json!("a")),
            (b.clone(), json!("b")),
            (c.clone(), json!("c")),
        ];
        expected.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(live, expected);
        let dataset: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(dataset["current_version"], 5);
        assert_eq!(dataset["item_count"], 3);

        // The history is kept: v2 still has a2.
        let v2: Value = get(&app, &seeded, "faq/items?version=2").await.json();
        assert!(
            v2["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["input"] == "a2")
        );

        // Restoring to 0 closes every live item in one version.
        let emptied = restore(&app, &seeded, "faq", json!({ "version": 0 })).await;
        assert_eq!(emptied.status, 200);
        let emptied: Value = emptied.json();
        assert_eq!(emptied["version"], 6);
        assert_eq!(emptied["deleted"], 3);
        assert_eq!(emptied["added"], 0);
        assert!(live_inputs(&app, &seeded, "faq").await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restore_refuses_a_bad_target_and_writes_nothing() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        post_items(&app, &seeded, "faq", json!({ "items": [ { "input": 1 } ] })).await;

        // The current version and the versions past it are not earlier versions.
        for target in [1, 2, -1] {
            assert_eq!(
                field_errors(&restore(&app, &seeded, "faq", json!({ "version": target })).await),
                vec![(
                    "version".to_owned(),
                    "must be an earlier version, 0 to 0".to_owned()
                )],
                "target {target}"
            );
        }
        let dataset: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(dataset["current_version"], 1);

        // Reading key is refused.
        let read = app
            .request(Method::POST, &format!("{}/faq/restore", base(&seeded)))
            .header("authorization", &format!("Bearer {}", seeded.read_key))
            .json(&json!({ "version": 0 }))
            .send()
            .await;
        assert_eq!(read.status, 403);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restore_to_the_live_items_is_refused_and_changes_outside_are_404() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        post_items(
            &app,
            &seeded,
            "faq",
            json!({ "items": [ { "input": "a" } ] }),
        )
        .await;
        let ids = item_ids(
            &post_items(
                &app,
                &seeded,
                "faq",
                json!({ "items": [ { "input": "b" } ] }),
            )
            .await,
        );
        // v2 added b, v3 deleted b: the live items are v1's items again.
        batch_delete(&app, &seeded, "faq", json!({ "ids": [ids[0].clone()] })).await;

        let refused = restore(&app, &seeded, "faq", json!({ "version": 1 })).await;
        assert_eq!(refused.status, 400);
        assert_eq!(
            field_errors(&refused),
            vec![(
                "version".to_owned(),
                "the latest version already holds these items".to_owned()
            )]
        );
        let dataset: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(dataset["current_version"], 3, "a refusal writes nothing");

        for version in [0, 4] {
            let response = get(&app, &seeded, &format!("faq/versions/{version}/changes")).await;
            assert_eq!(response.status, 404, "version {version}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restore_checks_the_current_schema() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let ids = item_ids(
            &post_items(&app, &seeded, "faq", json!({ "items": [ { "input": 5 } ] })).await,
        );
        batch_edit(
            &app,
            &seeded,
            "faq",
            json!({ "items": [ { "id": ids[0], "input": { "question": "a" } } ] }),
        )
        .await;
        // The live item is an object, so the schema is accepted; v1 holds a number.
        let schema: Value = serde_json::from_str(FAQ_SCHEMA).unwrap();
        let set = patch(&app, &seeded, "faq", json!({ "input_schema": schema })).await;
        assert_eq!(set.status, 200);

        let refused = restore(&app, &seeded, "faq", json!({ "version": 1 })).await;
        let errors = field_errors(&refused);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].0, format!("items[{}].input", ids[0]));
        let dataset: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(dataset["current_version"], 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn batch_routes_need_manage_datasets() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let ids = item_ids(
            &post_items(&app, &seeded, "faq", json!({ "items": [ { "input": 1 } ] })).await,
        );
        let read_header = format!("Bearer {}", seeded.read_key);
        let edit = app
            .request(Method::PATCH, &format!("{}/faq/items", base(&seeded)))
            .header("authorization", &read_header)
            .json(&json!({ "items": [ { "id": ids[0], "input": 2 } ] }))
            .send()
            .await;
        assert_eq!(edit.status, 403);
        let delete = app
            .request(Method::POST, &format!("{}/faq/items/delete", base(&seeded)))
            .header("authorization", &read_header)
            .json(&json!({ "ids": [ids[0]] }))
            .send()
            .await;
        assert_eq!(delete.status, 403);
        let dataset: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(dataset["current_version"], 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn version_bounds_and_cursors() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        post_items(
            &app,
            &seeded,
            "faq",
            json!({ "items": [ { "input": 1 }, { "input": 2 }, { "input": 3 } ] }),
        )
        .await;

        assert_eq!(
            field_errors(&get(&app, &seeded, "faq/items?version=5").await),
            vec![("version".to_owned(), "must be between 0 and 1".to_owned())]
        );
        assert_eq!(
            field_errors(&get(&app, &seeded, "faq/items?version=-1").await),
            vec![("version".to_owned(), "must be between 0 and 1".to_owned())]
        );
        assert_eq!(
            field_errors(&get(&app, &seeded, "faq/items?cursor=nope").await),
            vec![("cursor".to_owned(), "invalid cursor".to_owned())]
        );
        // Version 0 is the empty version, not the current one.
        let empty: Value = get(&app, &seeded, "faq/items?version=0").await.json();
        assert!(empty["data"].as_array().unwrap().is_empty());
        assert!(empty["next_cursor"].is_null());

        let first: Value = get(&app, &seeded, "faq/items?limit=2").await.json();
        assert_eq!(first["data"].as_array().unwrap().len(), 2);
        let cursor = first["next_cursor"].as_str().unwrap().to_owned();
        let second: Value = get(&app, &seeded, &format!("faq/items?limit=2&cursor={cursor}"))
            .await
            .json();
        assert_eq!(second["data"].as_array().unwrap().len(), 1);
        assert!(second["next_cursor"].is_null());

        post_items(&app, &seeded, "faq", json!({ "items": [ { "input": 4 } ] })).await;
        let newest: Value = get(&app, &seeded, "faq/versions?limit=1").await.json();
        assert_eq!(newest["data"][0]["version"], 2);
        assert_eq!(newest["next_cursor"], "2");
        let older: Value = get(&app, &seeded, "faq/versions?limit=1&cursor=2")
            .await
            .json();
        assert_eq!(older["data"][0]["version"], 1);
        assert!(older["next_cursor"].is_null());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn live_item_cap() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let dataset = create_ok(&app, &seeded, "faq").await;
        let dataset_id: Uuid = dataset["id"].as_str().unwrap().parse().unwrap();
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let insert: Vec<NewRevision> = (0..store::LIVE_ITEM_CAP)
                        .map(|i| NewRevision {
                            revision_id: Uuid::now_v7(),
                            item_id: Uuid::now_v7(),
                            input: json!(i),
                            expected_output: None,
                            metadata: Map::new(),
                            source_trace_id: None,
                            source_span_id: None,
                        })
                        .collect();
                    let change = VersionChange {
                        close: Vec::new(),
                        insert,
                        description: "fill".to_owned(),
                        created_by: None,
                    };
                    store::write_version(conn, dataset_id, change, now).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();

        let refused = post_items(
            &app,
            &seeded,
            "faq",
            json!({ "items": [ { "input": "one more" } ] }),
        )
        .await;
        assert_eq!(refused.status, 409);
        let problem: Value = refused.json();
        assert_eq!(problem["detail"], "a dataset holds at most 50000 items");
        let after: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(after["current_version"], 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn source_ids_survive_an_edit() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let dataset = create_ok(&app, &seeded, "faq").await;
        let dataset_id: Uuid = dataset["id"].as_str().unwrap().parse().unwrap();
        let item = Uuid::now_v7();
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let change = VersionChange {
                        close: Vec::new(),
                        insert: vec![NewRevision {
                            revision_id: Uuid::now_v7(),
                            item_id: item,
                            input: json!({ "question": "x" }),
                            expected_output: None,
                            metadata: Map::new(),
                            source_trace_id: Some(
                                "a1000000000000000000000000000001".parse().unwrap(),
                            ),
                            source_span_id: Some("b100000000000011".parse().unwrap()),
                        }],
                        description: "copy".to_owned(),
                        created_by: None,
                    };
                    store::write_version(conn, dataset_id, change, now).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();

        let edited = patch_item(
            &app,
            &seeded,
            "faq",
            &item.to_string(),
            json!({ "metadata": { "topic": "t" } }),
        )
        .await;
        assert_eq!(edited.status, 200);
        let edited: Value = edited.json();
        assert_eq!(
            edited["source_trace_id"],
            "a1000000000000000000000000000001"
        );
        assert_eq!(edited["source_span_id"], "b100000000000011");
        assert_eq!(edited["metadata"], json!({ "topic": "t" }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn size_limits() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;

        let oversized = post_items(
            &app,
            &seeded,
            "faq",
            json!({ "items": [ { "input": "x".repeat(1_048_577) } ] }),
        )
        .await;
        assert_eq!(
            field_errors(&oversized),
            vec![("items[0]".to_owned(), "is larger than 1 MiB".to_owned())]
        );

        // Twenty items of about 870 KiB each: under the per-item cap, over the 16 MiB body cap.
        let text = "a".repeat(870 * 1024);
        let body =
            json!({ "items": (0..20).map(|_| json!({ "input": text })).collect::<Vec<_>>() });
        assert_eq!(post_items(&app, &seeded, "faq", body).await.status, 413);

        let id = item_ids(
            &post_items(
                &app,
                &seeded,
                "faq",
                json!({ "items": [ { "input": "a" } ] }),
            )
            .await,
        )
        .remove(0);
        let large = patch_item(
            &app,
            &seeded,
            "faq",
            &id,
            json!({ "input": "x".repeat(3 * 1024 * 1024) }),
        )
        .await;
        assert_eq!(large.status, 413);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn personal_tokens_write_items() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let read_write = app.personal_token(seeded.user_id, true).await;
        let added = app
            .request(Method::POST, &format!("{}/faq/items", base(&seeded)))
            .header("authorization", &format!("Bearer {read_write}"))
            .json(&json!({ "items": [ { "input": "a" } ] }))
            .send()
            .await;
        assert_eq!(added.status, 201);
        let versions: Value = get(&app, &seeded, "faq/versions").await.json();
        assert_eq!(
            versions["data"][0]["created_by"],
            seeded.user_id.to_string()
        );

        let read_only = app.personal_token(seeded.user_id, false).await;
        let refused = app
            .request(Method::POST, &format!("{}/faq/items", base(&seeded)))
            .header("authorization", &format!("Bearer {read_only}"))
            .json(&json!({ "items": [ { "input": "b" } ] }))
            .send()
            .await;
        assert_eq!(refused.status, 403);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keys_write_items() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let write_key = app.write_key(&seeded).await;
        let added = app
            .request(Method::POST, &format!("{}/faq/items", base(&seeded)))
            .header("authorization", &format!("Bearer {write_key}"))
            .json(&json!({ "items": [ { "input": "a" } ] }))
            .send()
            .await;
        assert_eq!(added.status, 201);
        let versions: Value = get(&app, &seeded, "faq/versions").await.json();
        let created_by = versions["data"][0]["created_by"].as_str().unwrap();
        assert!(
            created_by
                .strip_prefix("key:")
                .is_some_and(|id| id.parse::<Uuid>().is_ok()),
            "{created_by}"
        );

        let read_header = format!("Bearer {}", seeded.read_key);
        let read = app
            .request(Method::GET, &format!("{}/faq/items", base(&seeded)))
            .header("authorization", &read_header)
            .send()
            .await;
        assert_eq!(read.status, 200);
        let write = app
            .request(Method::POST, &format!("{}/faq/items", base(&seeded)))
            .header("authorization", &read_header)
            .json(&json!({ "items": [ { "input": "b" } ] }))
            .send()
            .await;
        assert_eq!(write.status, 403);
    }

    #[tokio::test]
    async fn item_logs_hold_no_content() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let (status, logs) = capture_logs(async {
            post_items(
                &app,
                &seeded,
                "faq",
                json!({ "items": [ { "input": { "question": "canary-j1-input" } } ] }),
            )
            .await
            .status
        })
        .await;
        assert_eq!(status, 201);
        assert!(!logs.contains("canary-j1-input"), "{logs}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn schema_check_sums_in_sql_before_reading() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let dataset = create_ok(&app, &seeded, "sql-first").await;
        let id: Uuid = dataset["id"].as_str().unwrap().parse().unwrap();
        // Both items fail the schema, so only the size bound can make the check answer "too large".
        add_items(
            &app,
            &id.to_string(),
            vec![json!({ "q": "a" }), json!({ "q": "b" })],
        )
        .await;

        let schemas = Schemas::compile(Some(FAQ_SCHEMA), None).unwrap();
        let capped = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(
                    async move { check_live_items(conn, id, "input_schema", &schemas, 16).await },
                )
            })
            .await
            .unwrap()
            .expect("the bound is exceeded");
        assert_eq!(capped.len(), 1);
        assert_eq!(
            capped[0].message,
            "the dataset is too large to validate against a new schema"
        );

        let schemas = Schemas::compile(Some(FAQ_SCHEMA), None).unwrap();
        let failures = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    check_live_items(conn, id, "input_schema", &schemas, MAX_SCHEMA_CHECK_BYTES)
                        .await
                })
            })
            .await
            .unwrap()
            .expect("the items fail");
        assert_eq!(failures.len(), 2);
        assert!(failures[0].field.starts_with("items["), "{failures:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn item_patch_refuses_null_input_and_metadata() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let added: Value = post_items(
            &app,
            &seeded,
            "faq",
            json!({ "items": [ { "input": "kept", "expected_output": "old", "metadata": { "k": 1 } } ] }),
        )
        .await
        .json();
        let item = added["item_ids"][0].as_str().unwrap().to_owned();

        assert_eq!(
            field_errors(&patch_item(&app, &seeded, "faq", &item, json!({ "input": null })).await),
            vec![("input".to_owned(), "must not be null".to_owned())]
        );
        assert_eq!(
            field_errors(
                &patch_item(&app, &seeded, "faq", &item, json!({ "metadata": null })).await
            ),
            vec![("metadata".to_owned(), "must not be null".to_owned())]
        );
        let unchanged: Value = get(&app, &seeded, &format!("faq/items/{item}"))
            .await
            .json();
        assert_eq!(unchanged["input"], "kept");
        assert_eq!(unchanged["version"], 1);

        // `expected_output: null` still clears the field.
        let cleared = patch_item(
            &app,
            &seeded,
            "faq",
            &item,
            json!({ "expected_output": null }),
        )
        .await;
        assert_eq!(cleared.status, 200);
        assert_eq!(cleared.json::<Value>()["version"], 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failing_batch_lists_at_most_100_errors() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let items: Vec<Value> = (0..1_000).map(|_| json!({ "input": null })).collect();
        let errors =
            field_errors(&post_items(&app, &seeded, "faq", json!({ "items": items })).await);
        assert_eq!(errors.len(), 100);
        assert_eq!(
            errors[0],
            ("items[0].input".to_owned(), "must not be null".to_owned())
        );
        assert_eq!(
            errors[99],
            ("items[99].input".to_owned(), "must not be null".to_owned())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_filter_matches_wildcards_literally() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        for name in ["100% done", "100 done", "a_b", "axb"] {
            create_ok(&app, &seeded, name).await;
        }
        let names = |body: &Value| -> Vec<String> {
            body["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["name"].as_str().unwrap().to_owned())
                .collect()
        };
        let percent: Value = get(&app, &seeded, "?q=%25").await.json();
        assert_eq!(names(&percent), vec!["100% done"]);
        let underscore: Value = get(&app, &seeded, "?q=_").await.json();
        assert_eq!(names(&underscore), vec!["a_b"]);
    }

    #[tokio::test]
    async fn item_edits_and_deletes_are_audited() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let dataset = create_ok(&app, &seeded, "faq").await;
        let dataset_id = dataset["id"].as_str().unwrap().to_owned();
        let added: Value = post_items(
            &app,
            &seeded,
            "faq",
            json!({ "items": [ { "input": "canary-audit-old" } ] }),
        )
        .await
        .json();
        let item = added["item_ids"][0].as_str().unwrap().to_owned();

        let ((edited, deleted), logs) = capture_logs(async {
            let edited = patch_item(
                &app,
                &seeded,
                "faq",
                &item,
                json!({ "input": "canary-audit-new" }),
            )
            .await;
            let deleted = app
                .request(
                    Method::DELETE,
                    &format!("{}/faq/items/{item}", base(&seeded)),
                )
                .header("cookie", &seeded.cookie)
                .send()
                .await;
            (edited.status, deleted.status)
        })
        .await;
        assert_eq!((edited, deleted), (StatusCode::OK, StatusCode::OK));
        assert!(logs.contains("dataset item edited"), "{logs}");
        assert!(logs.contains("dataset item deleted"), "{logs}");
        assert!(logs.contains(&item), "{logs}");
        assert!(logs.contains(&dataset_id), "{logs}");
        assert!(logs.contains("version=2"), "{logs}");
        assert!(logs.contains("version=3"), "{logs}");
        assert!(!logs.contains("canary-audit"), "{logs}");
    }

    const CSV_MAPPING: &str =
        r#"{"columns":[{"column":"q","target":"input"},{"column":"a","target":"expected"}]}"#;

    /// A multipart body with a `file` part and an optional `mapping` part.
    fn multipart(file_name: &str, file: &[u8], mapping: Option<&str>) -> (String, Vec<u8>) {
        const BOUNDARY: &str = "cetra-test-boundary";
        let mut body = Vec::new();
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(file);
        body.extend_from_slice(b"\r\n");
        if let Some(mapping) = mapping {
            body.extend_from_slice(
                format!(
                    "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"mapping\"\r\nContent-Type: application/json\r\n\r\n{mapping}\r\n"
                )
                .as_bytes(),
            );
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        (format!("multipart/form-data; boundary={BOUNDARY}"), body)
    }

    /// The import tests share [`IMPORTS`], which has only [`IMPORTS_AT_ONCE`] permits, so they run
    /// one at a time under this lock: more concurrent imports would see a 503, and the permit
    /// tests can hold every permit without a 503 in another test.
    static IMPORT_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// `POST …/import?format=` with a `file` part named `file_name`.
    async fn import(
        app: &TestApp,
        seeded: &Seeded,
        dataset: &str,
        format: &str,
        file_name: &str,
        file: &[u8],
        mapping: Option<&str>,
    ) -> TestResponse {
        let _gate = IMPORT_GATE.lock().await;
        let (content_type, body) = multipart(file_name, file, mapping);
        app.request(
            Method::POST,
            &format!("{}/{dataset}/import?format={format}", base(seeded)),
        )
        .header("cookie", &seeded.cookie)
        .header("content-type", &content_type)
        .body(body)
        .send()
        .await
    }

    /// `POST …/import/preview?query` with `body` as the raw file.
    async fn preview(
        app: &TestApp,
        seeded: &Seeded,
        dataset: &str,
        query: &str,
        body: &[u8],
    ) -> TestResponse {
        app.request(
            Method::POST,
            &format!("{}/{dataset}/import/preview?{query}", base(seeded)),
        )
        .header("cookie", &seeded.cookie)
        .body(body.to_vec())
        .send()
        .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn preview_shows_the_head() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let response = preview(&app, &seeded, "faq", "format=csv", b"q,a\nhi,x\nho,y\n").await;
        assert_eq!(response.status, 200);
        let body: Value = response.json();
        assert_eq!(body["columns"], json!(["q", "a"]));
        assert_eq!(body["rows"].as_array().unwrap().len(), 2);
        assert_eq!(body["truncated"], false);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn preview_partial_drops_the_cut_record() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let response = preview(
            &app,
            &seeded,
            "faq",
            "format=csv&partial=true",
            b"q,a\nhi,x\nho",
        )
        .await;
        assert_eq!(response.status, 200);
        let body: Value = response.json();
        assert_eq!(body["rows"].as_array().unwrap().len(), 1);
        assert_eq!(body["truncated"], true);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn preview_bad_header_is_a_400() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let response = preview(&app, &seeded, "faq", "format=csv", b"q,q\n").await;
        assert_eq!(response.status, 400);
        assert_eq!(
            response.problem().detail.as_deref(),
            Some("column 2 repeats the name of column 1")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn preview_over_one_mib_is_a_413() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let big = vec![b'a'; 1024 * 1024 + 1];
        let response = preview(&app, &seeded, "faq", "format=csv", &big).await;
        assert_eq!(response.status, 413);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn preview_with_a_read_key_is_refused() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let response = app
            .request(
                Method::POST,
                &format!("{}/faq/import/preview?format=csv", base(&seeded)),
            )
            .header("authorization", &format!("Bearer {}", seeded.read_key))
            .body(b"q,a\nhi,x\n".to_vec())
            .send()
            .await;
        assert_eq!(response.status, 403);
    }

    /// `(input, expected_output, metadata)` of every live item, as sorted JSON texts.
    fn item_texts(list: &Value) -> Vec<String> {
        let mut texts: Vec<String> = list["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                json!({
                    "input": item["input"],
                    "expected_output": item["expected_output"],
                    "metadata": item["metadata"],
                })
                .to_string()
            })
            .collect();
        texts.sort();
        texts
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exported_jsonl_imports_back_unchanged() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        post_items(
            &app,
            &seeded,
            "faq",
            json!({ "items": [
                { "input": { "q": "a" }, "expected_output": "x", "metadata": { "t": "one" } },
                { "input": ["b", 2] },
                { "input": "c", "expected_output": { "n": [1] } }
            ] }),
        )
        .await;
        let export = get(&app, &seeded, "faq/export").await;
        assert_eq!(export.status, 200);

        create_ok(&app, &seeded, "copy").await;
        let imported = import(
            &app,
            &seeded,
            "copy",
            "jsonl",
            "export.jsonl",
            &export.body,
            None,
        )
        .await;
        assert_eq!(
            imported.status,
            201,
            "{}",
            String::from_utf8_lossy(&imported.body)
        );
        let body: Value = imported.json();
        assert_eq!(body["imported"], 3);
        assert_eq!(body["error_count"], 0);

        let original = get(&app, &seeded, "faq/items").await.json();
        let copied = get(&app, &seeded, "copy/items").await.json();
        assert_eq!(item_texts(&copied), item_texts(&original));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_refuses_a_value_nested_past_100_levels() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        // 120 levels parse (serde_json stops at 127); `input.x` adds one more.
        let deep = format!("{}1{}", "[".repeat(120), "]".repeat(120));
        let file = format!("x\n{deep}\nhi\n");
        let mapping = r#"{"columns":[{"column":"x","target":"input.x"}]}"#;
        let response = import(
            &app,
            &seeded,
            "faq",
            "csv",
            "data.csv",
            file.as_bytes(),
            Some(mapping),
        )
        .await;
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let body: Value = response.json();
        assert_eq!(body["imported"], 1);
        assert_eq!(
            body["errors"],
            json!([{ "row": 1, "message": "input: is nested too deeply" }])
        );
        let items = get(&app, &seeded, "faq/items").await;
        assert_eq!(items.status, 200);
        let items: Value = items.json();
        assert_eq!(items["data"].as_array().unwrap().len(), 1);
        assert_eq!(items["data"][0]["input"], json!({ "x": "hi" }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_csv_makes_one_version() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let response = import(
            &app,
            &seeded,
            "faq",
            "csv",
            "data.csv",
            b"q,a\nWhat?,Yes\n,No\nHow?,Ok\n",
            Some(CSV_MAPPING),
        )
        .await;
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let body: Value = response.json();
        assert_eq!(body["version"], 1);
        assert_eq!(body["imported"], 2);
        assert_eq!(body["error_count"], 1);
        assert_eq!(
            body["errors"],
            json!([{ "row": 2, "message": "the row has no input" }])
        );

        let items: Value = get(&app, &seeded, "faq/items").await.json();
        assert_eq!(items["data"].as_array().unwrap().len(), 2);
        let versions: Value = get(&app, &seeded, "faq/versions").await.json();
        assert_eq!(
            versions["data"][0]["description"],
            "import data.csv (2 items)"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_jsonl_makes_one_version() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let file = b"{\"input\":\"a\"}\n{\"input\":\"b\"}\n{\"input\":1,\"x\":2}\n";
        let response = import(&app, &seeded, "faq", "jsonl", "data.jsonl", file, None).await;
        assert_eq!(response.status, 201);
        let body: Value = response.json();
        assert_eq!(body["imported"], 2);
        assert_eq!(
            body["errors"],
            json!([{ "row": 3, "message": "unknown key x" }])
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_checks_the_schema() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let schema: Value = serde_json::from_str(FAQ_SCHEMA).unwrap();
        create(
            &app,
            &seeded,
            json!({ "name": "faq", "input_schema": schema }),
        )
        .await;
        let file = b"{\"input\":{\"question\":\"a\"}}\n{\"input\":{}}\n";
        let response = import(&app, &seeded, "faq", "jsonl", "data.jsonl", file, None).await;
        assert_eq!(response.status, 201);
        let body: Value = response.json();
        assert_eq!(body["imported"], 1);
        assert_eq!(
            body["errors"],
            json!([{ "row": 2, "message": "input: /: required" }])
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_schema_change_during_an_import_is_a_conflict() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let used = find(&app.state, seeded.project.id, "faq").await.unwrap();
        let schema: Value = serde_json::from_str(FAQ_SCHEMA).unwrap();
        let patched = patch(&app, &seeded, "faq", json!({ "input_schema": schema })).await;
        assert_eq!(patched.status, 200);

        let change = VersionChange {
            close: Vec::new(),
            insert: vec![NewRevision {
                revision_id: Uuid::now_v7(),
                item_id: Uuid::now_v7(),
                input: json!({ "question": "a" }),
                expected_output: None,
                metadata: Map::new(),
                source_trace_id: None,
                source_span_id: None,
            }],
            description: "import".to_owned(),
            created_by: None,
        };
        let outcome = write_checked(&app.state, seeded.project.id, &used, change, None).await;
        assert!(matches!(
            outcome,
            Err(ApiError::Conflict { detail }) if detail == "the dataset's schema changed; retry"
        ));
        let dataset: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(dataset["current_version"], 0);
    }

    #[test]
    fn clean_file_name_rules() {
        assert_eq!(clean_file_name(None), "upload");
        assert_eq!(clean_file_name(Some("")), "upload");
        assert_eq!(clean_file_name(Some("\u{1}")), "upload");
        assert_eq!(clean_file_name(Some("a\u{7}b.csv")), "ab.csv");
        assert_eq!(clean_file_name(Some(&"a".repeat(250))), "a".repeat(200));
        // Zero-width, bidi and line or paragraph separator characters go too.
        let hidden = "a\u{200B}b\u{200F}c\u{2028}d\u{202E}e\u{2066}f\u{2069}g.csv";
        assert_eq!(clean_file_name(Some(hidden)), "abcdefg.csv");
        // Characters next to the ranges stay.
        assert_eq!(
            clean_file_name(Some("\u{200A}\u{2010}\u{202F}\u{2070}é.csv")),
            "\u{200A}\u{2010}\u{202F}\u{2070}é.csv"
        );
        assert_eq!(clean_file_name(Some("\u{200B}\u{2066}")), "upload");
    }

    #[tokio::test]
    async fn upload_deadline_stops_a_slow_body() {
        let slow = upload_deadline(std::future::pending::<()>(), Duration::from_millis(10)).await;
        let error = slow.expect_err("times out");
        assert!(matches!(
            error,
            ApiError::BadRequest { detail: Some(ref detail), .. }
                if detail == "the upload took too long"
        ));
        assert_eq!(
            upload_deadline(std::future::ready(1), Duration::from_millis(10))
                .await
                .expect("ready"),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_without_valid_rows_writes_nothing() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let response = import(
            &app,
            &seeded,
            "faq",
            "jsonl",
            "data.jsonl",
            b"{\"expected_output\":1}\n",
            None,
        )
        .await;
        assert_eq!(response.status, 400);
        let problem = response.problem();
        assert_eq!(problem.detail.as_deref(), Some("no row could be imported"));
        assert_eq!(problem.errors[0].field, "rows[1]");
        let dataset: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(dataset["current_version"], 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_lists_at_most_100_errors() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let mut file = String::from("{\"input\":1}\n");
        for _ in 0..150 {
            file.push_str("[1]\n");
        }
        let response = import(
            &app,
            &seeded,
            "faq",
            "jsonl",
            "data.jsonl",
            file.as_bytes(),
            None,
        )
        .await;
        assert_eq!(response.status, 201);
        let body: Value = response.json();
        assert_eq!(body["error_count"], 150);
        assert_eq!(body["errors"].as_array().unwrap().len(), 100);
        assert_eq!(body["errors"][0]["row"], 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_refuses_more_than_50000_rows() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let file = "{\"input\":1}\n".repeat(50_001);
        let response = import(
            &app,
            &seeded,
            "faq",
            "jsonl",
            "data.jsonl",
            file.as_bytes(),
            None,
        )
        .await;
        assert_eq!(response.status, 400);
        assert_eq!(
            response.problem().detail.as_deref(),
            Some("the file has more than 50000 rows")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_respects_the_live_item_cap() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let dataset = create_ok(&app, &seeded, "faq").await;
        let dataset_id = dataset["id"].as_str().unwrap();
        let inputs = (0..store::LIVE_ITEM_CAP - 1).map(|i| json!(i)).collect();
        add_items(&app, dataset_id, inputs).await;
        let file = b"{\"input\":1}\n{\"input\":2}\n";
        let response = import(&app, &seeded, "faq", "jsonl", "data.jsonl", file, None).await;
        assert_eq!(response.status, 409);
        assert_eq!(
            response.problem().detail.as_deref(),
            Some("a dataset holds at most 50000 items")
        );
        let stored: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(stored["current_version"], 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_over_100_mib_is_a_413() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let file = vec![b'a'; import::MAX_FILE_BYTES + 1];
        let response = import(
            &app,
            &seeded,
            "faq",
            "csv",
            "data.csv",
            &file,
            Some(CSV_MAPPING),
        )
        .await;
        assert_eq!(response.status, 413);
        assert_eq!(
            response.problem().detail.as_deref(),
            Some("the file is larger than 100 MiB")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_mapping_errors_name_the_entry() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let mapping = r#"{"columns":[{"column":"nope","target":"input"}]}"#;
        let response = import(
            &app,
            &seeded,
            "faq",
            "csv",
            "data.csv",
            b"q,a\nhi,x\n",
            Some(mapping),
        )
        .await;
        let errors = field_errors(&response);
        assert_eq!(errors[0].0, "mapping.columns[0].column");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_csv_needs_a_mapping_and_jsonl_refuses_one() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let no_mapping = import(
            &app,
            &seeded,
            "faq",
            "csv",
            "data.csv",
            b"q,a\nhi,x\n",
            None,
        )
        .await;
        assert_eq!(
            field_errors(&no_mapping),
            vec![("mapping".to_owned(), "required for CSV".to_owned())]
        );
        let with_mapping = import(
            &app,
            &seeded,
            "faq",
            "jsonl",
            "data.jsonl",
            b"{\"input\":1}\n",
            Some(CSV_MAPPING),
        )
        .await;
        assert_eq!(
            field_errors(&with_mapping),
            vec![(
                "mapping".to_owned(),
                "JSONL imports take no mapping".to_owned()
            )]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_needs_multipart_and_manage_datasets() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let json_body = app
            .request(
                Method::POST,
                &format!("{}/faq/import?format=jsonl", base(&seeded)),
            )
            .header("cookie", &seeded.cookie)
            .json(&json!({}))
            .send()
            .await;
        assert_eq!(json_body.status, 415);

        let (content_type, body) = multipart("data.jsonl", b"{\"input\":1}\n", None);
        let read_only = app
            .request(
                Method::POST,
                &format!("{}/faq/import?format=jsonl", base(&seeded)),
            )
            .header("authorization", &format!("Bearer {}", seeded.read_key))
            .header("content-type", &content_type)
            .body(body)
            .send()
            .await;
        assert_eq!(read_only.status, 403);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_permit_is_refused_when_busy() {
        let _gate = IMPORT_GATE.lock().await;
        assert_eq!(IMPORTS.available_permits(), IMPORTS_AT_ONCE);
        let first = import_permit(&IMPORTS).expect("first permit");
        let second = import_permit(&IMPORTS).expect("second permit");
        assert!(matches!(
            import_permit(&IMPORTS),
            Err(ApiError::Unavailable { .. })
        ));
        drop(first);
        let third = import_permit(&IMPORTS).expect("a freed permit");
        drop((second, third));
        assert_eq!(IMPORTS.available_permits(), IMPORTS_AT_ONCE);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_is_503_while_both_permits_are_held() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let _gate = IMPORT_GATE.lock().await;
        let held = (
            import_permit(&IMPORTS).expect("first permit"),
            import_permit(&IMPORTS).expect("second permit"),
        );
        let (content_type, body) = multipart("data.jsonl", b"{\"input\":1}\n", None);
        let response = app
            .request(
                Method::POST,
                &format!("{}/faq/import?format=jsonl", base(&seeded)),
            )
            .header("cookie", &seeded.cookie)
            .header("content-type", &content_type)
            .body(body)
            .send()
            .await;
        drop(held);
        assert_eq!(response.status, 503);
        let stored: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(stored["current_version"], 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_of_a_non_multipart_body_is_415() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let response = app
            .request(
                Method::POST,
                &format!("{}/faq/import?format=jsonl", base(&seeded)),
            )
            .header("cookie", &seeded.cookie)
            .header("content-type", "text/plain")
            .body(b"{\"input\":1}\n".to_vec())
            .send()
            .await;
        assert_eq!(response.status, 415);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn import_logs_hold_no_content() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let (status, logs) = capture_logs(async {
            import(
                &app,
                &seeded,
                "faq",
                "csv",
                "canary-file-2.csv",
                b"q,a\ncanary-import-2,yes\n",
                Some(CSV_MAPPING),
            )
            .await
            .status
        })
        .await;
        assert_eq!(status, 201);
        assert!(!logs.contains("canary-import-2"), "{logs}");
        assert!(!logs.contains("canary-file-2"), "{logs}");
    }

    /// The example trace of `fixtures/api/dataset`: root `b100000000000011`.
    const T1: &str = "a1000000000000000000000000000001";
    const DAY_NS: i64 = 86_400_000_000_000;

    fn from_trace_path(seeded: &Seeded) -> String {
        format!("{}/faq/items/from-trace", base(seeded))
    }

    async fn from_trace(app: &TestApp, seeded: &Seeded, body: Value) -> TestResponse {
        app.request(Method::POST, &from_trace_path(seeded))
            .header("cookie", &seeded.cookie)
            .json(&body)
            .send()
            .await
    }

    /// The example traces in the project, and the dataset `faq` with no items.
    async fn faq_with_traces(app: &TestApp, seeded: &Seeded) {
        app.load_example_dataset(seeded.project.id).await;
        create_ok(app, seeded, "faq").await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn from_trace_copies_the_root() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        faq_with_traces(&app, &seeded).await;

        let response = from_trace(&app, &seeded, json!({ "trace_id": T1 })).await;
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let item: Value = response.json();
        assert_eq!(item["input"], "My order 1234 hasn't arrived.");
        assert_eq!(
            item["expected_output"],
            "Your order 1234 shipped and arrives tomorrow."
        );
        assert_eq!(item["source_trace_id"], T1);
        assert_eq!(item["source_span_id"], "b100000000000011");

        let dataset: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(dataset["current_version"], 1);
        let versions: Value = get(&app, &seeded, "faq/versions").await.json();
        assert_eq!(
            versions["data"][0]["description"],
            "add from trace a1000000000000000000000000000001"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn from_span_decodes_json() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        faq_with_traces(&app, &seeded).await;

        let response = from_trace(
            &app,
            &seeded,
            json!({ "trace_id": T1, "span_id": "b100000000000013" }),
        )
        .await;
        assert_eq!(response.status, 201);
        let item: Value = response.json();
        assert_eq!(item["input"], json!({ "order_id": "1234" }));
        assert_eq!(
            item["expected_output"],
            json!({ "status": "shipped", "eta": "2026-01-02" })
        );
        assert_eq!(item["source_span_id"], "b100000000000013");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn edited_values_replace_the_span() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        faq_with_traces(&app, &seeded).await;

        let response = from_trace(
            &app,
            &seeded,
            json!({
                "trace_id": T1,
                "input": { "q": "x" },
                "expected_output": null,
                "metadata": { "k": 1 }
            }),
        )
        .await;
        assert_eq!(response.status, 201);
        let item: Value = response.json();
        assert_eq!(item["input"], json!({ "q": "x" }));
        assert!(item.get("expected_output").is_none(), "{item}");
        assert_eq!(item["metadata"], json!({ "k": 1 }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn uppercase_ids_are_accepted() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        faq_with_traces(&app, &seeded).await;

        let response = from_trace(
            &app,
            &seeded,
            json!({ "trace_id": T1.to_uppercase(), "span_id": "B100000000000011" }),
        )
        .await;
        assert_eq!(response.status, 201);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_trace_and_span_are_not_found() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        faq_with_traces(&app, &seeded).await;

        let trace = from_trace(
            &app,
            &seeded,
            json!({ "trace_id": "a10000000000000000000000000000ff" }),
        )
        .await;
        assert_eq!(trace.status, 404);
        let span = from_trace(
            &app,
            &seeded,
            json!({ "trace_id": T1, "span_id": "b1000000000000ff" }),
        )
        .await;
        assert_eq!(span.status, 404);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn evaluation_trace_is_refused() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        faq_with_traces(&app, &seeded).await;

        let response = from_trace(
            &app,
            &seeded,
            json!({ "trace_id": "a1000000000000000000000000000005" }),
        )
        .await;
        assert_eq!(response.status, 400);
        let errors = field_errors(&response);
        assert_eq!(errors[0].0, "trace_id");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bad_ids_are_refused() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        faq_with_traces(&app, &seeded).await;

        let trace = from_trace(&app, &seeded, json!({ "trace_id": "xyz" })).await;
        assert_eq!(field_errors(&trace)[0].0, "trace_id");
        let span = from_trace(&app, &seeded, json!({ "trace_id": T1, "span_id": "xyz" })).await;
        assert_eq!(field_errors(&span)[0].0, "span_id");

        // The static `from-trace` segment is not an item id: a GET there is not an item read.
        let get_path = app
            .request(Method::GET, &from_trace_path(&seeded))
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        assert!(
            matches!(get_path.status.as_u16(), 404 | 405),
            "GET from-trace answered {}",
            get_path.status
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn schema_applies_to_the_copied_item() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.load_example_dataset(seeded.project.id).await;
        let created = create(
            &app,
            &seeded,
            json!({ "name": "faq", "input_schema": { "type": "object" } }),
        )
        .await;
        assert_eq!(created.status, 201);

        // The root's input is a string, which the object schema refuses.
        let response = from_trace(&app, &seeded, json!({ "trace_id": T1 })).await;
        assert_eq!(response.status, 400);
        assert_eq!(field_errors(&response)[0].0, "input");
        let dataset: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(dataset["current_version"], 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hidden_trace_is_not_found() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        faq_with_traces(&app, &seeded).await;
        // Two days after the example traces, a one-day window no longer reaches them.
        set_limits(
            &app,
            seeded.org_id,
            "2026-01-01T00:00:00Z",
            &json!({ "data_access_days": 1 }),
        );
        app.state.clock.set(app.state.clock.now_ns() + 2 * DAY_NS);

        let response = from_trace(&app, &seeded, json!({ "trace_id": T1 })).await;
        assert_eq!(response.status, 404);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn blocked_trace_is_refused() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        faq_with_traces(&app, &seeded).await;
        let trace: TraceId = T1.parse().unwrap();
        app.state.deletions.guard.add_traces(
            seeded.project.id,
            &[trace],
            Uuid::now_v7(),
            app.state.clock.now_ns(),
        );

        let response = from_trace(&app, &seeded, json!({ "trace_id": T1 })).await;
        assert_eq!(response.status, 404);
        let dataset: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(dataset["current_version"], 0);
    }

    #[test]
    fn a_truncated_trace_is_refused() {
        let loaded = LoadedTrace {
            spans: vec![dataset_span(0)],
            truncated: true,
        };
        let result = item_from(&loaded, None, &from_trace_body(T1));
        assert!(matches!(
            result,
            Err(ApiError::BadRequest { ref detail, ref errors })
                if detail.is_none()
                    && errors.len() == 1
                    && errors[0].field == "trace_id"
                    && errors[0].message == "the trace is too large to copy"
        ));
    }

    /// A request body with only the trace id.
    fn from_trace_body(trace: &str) -> FromTraceRequest {
        FromTraceRequest {
            trace_id: trace.to_owned(),
            span_id: None,
            input: None,
            expected_output: None,
            metadata: None,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn from_trace_rechecks_the_schema() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        faq_with_traces(&app, &seeded).await;
        let used = find(&app.state, seeded.project.id, "faq").await.unwrap();
        let schema: Value = serde_json::from_str(FAQ_SCHEMA).unwrap();
        let patched = patch(&app, &seeded, "faq", json!({ "input_schema": schema })).await;
        assert_eq!(patched.status, 200);

        let change = VersionChange {
            close: Vec::new(),
            insert: vec![NewRevision {
                revision_id: Uuid::now_v7(),
                item_id: Uuid::now_v7(),
                input: json!("x"),
                expected_output: None,
                metadata: Map::new(),
                source_trace_id: Some(T1.parse().unwrap()),
                source_span_id: None,
            }],
            description: "add from trace".to_owned(),
            created_by: None,
        };
        let trace: TraceId = T1.parse().unwrap();
        let outcome = write_checked(
            &app.state,
            seeded.project.id,
            &used,
            change,
            Some((trace, Vec::new())),
        )
        .await;
        assert!(matches!(
            outcome,
            Err(ApiError::Conflict { detail }) if detail == "the dataset's schema changed; retry"
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn read_key_is_refused() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        faq_with_traces(&app, &seeded).await;
        let read_header = format!("Bearer {}", seeded.read_key);
        let response = app
            .request(Method::POST, &from_trace_path(&seeded))
            .header("authorization", &read_header)
            .json(&json!({ "trace_id": T1 }))
            .send()
            .await;
        assert_eq!(response.status, 403);
    }

    /// A second example trace, committed as a copy of the first one's root (its input is the same).
    const T9: &str = "a1000000000000000000000000000009";

    fn from_traces_path(seeded: &Seeded) -> String {
        format!("{}/faq/items/from-traces", base(seeded))
    }

    async fn from_traces(app: &TestApp, seeded: &Seeded, body: Value) -> TestResponse {
        app.request(Method::POST, &from_traces_path(seeded))
            .header("cookie", &seeded.cookie)
            .json(&body)
            .send()
            .await
    }

    /// `faq` over the example traces, plus `T9`.
    async fn faq_with_two_traces(app: &TestApp, seeded: &Seeded) {
        faq_with_traces(app, seeded).await;
        let now = app.state.clock.now_ns();
        let trace: TraceId = T9.parse().unwrap();
        app.commit_trace_at(&seeded.project, trace, 1, now).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bulk_add_copies_what_it_can_and_lists_the_rest() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        faq_with_two_traces(&app, &seeded).await;

        let response = from_traces(
            &app,
            &seeded,
            json!({ "items": [
                { "trace_id": T1 },
                { "trace_id": "a10000000000000000000000000000ff" },
                { "trace_id": "a1000000000000000000000000000005" },
                { "trace_id": T9 },
            ] }),
        )
        .await;
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let result: Value = response.json();
        assert_eq!(result["version"]["version"], 1);
        assert_eq!(result["version"]["description"], "add from 2 traces");
        assert_eq!(result["version"]["added"], 2);
        let items = result["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["source_trace_id"], T1);
        assert_eq!(items[1]["source_trace_id"], T9);
        let skipped = result["skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), 2);
        assert_eq!(skipped[0]["index"], 1);
        assert_eq!(skipped[0]["trace_id"], "a10000000000000000000000000000ff");
        assert_eq!(skipped[0]["message"], "the trace or span was not found");
        assert_eq!(skipped[1]["index"], 2);
        assert_eq!(
            skipped[1]["message"],
            "an evaluation trace cannot be added to a dataset"
        );

        let dataset: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(dataset["current_version"], 1);
        assert_eq!(dataset["item_count"], 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bulk_add_with_nothing_to_copy_is_a_400_and_writes_nothing() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        faq_with_traces(&app, &seeded).await;

        let response = from_traces(
            &app,
            &seeded,
            json!({ "items": [
                { "trace_id": "a10000000000000000000000000000ff" },
                { "trace_id": "a1000000000000000000000000000005" },
            ] }),
        )
        .await;
        assert_eq!(response.status, 400);
        let errors = field_errors(&response);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert_eq!(errors[0].0, "items[0]");
        assert_eq!(errors[1].0, "items[1]");
        let dataset: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(dataset["current_version"], 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bulk_add_refuses_a_bad_batch() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        faq_with_traces(&app, &seeded).await;

        let empty = from_traces(&app, &seeded, json!({ "items": [] })).await;
        assert_eq!(
            field_errors(&empty),
            vec![("items".to_owned(), "must hold 1 to 100 items".to_owned())]
        );
        let many: Vec<Value> = (0..101).map(|_| json!({ "trace_id": T1 })).collect();
        let too_many = from_traces(&app, &seeded, json!({ "items": many })).await;
        assert_eq!(
            field_errors(&too_many),
            vec![("items".to_owned(), "must hold 1 to 100 items".to_owned())]
        );
        let bad_trace = from_traces(
            &app,
            &seeded,
            json!({ "items": [ { "trace_id": T1 }, { "trace_id": "xyz" } ] }),
        )
        .await;
        assert_eq!(field_errors(&bad_trace)[0].0, "items[1].trace_id");
        let bad_span = from_traces(
            &app,
            &seeded,
            json!({ "items": [ { "trace_id": T1, "span_id": "xyz" } ] }),
        )
        .await;
        assert_eq!(field_errors(&bad_span)[0].0, "items[0].span_id");
        let unknown_field = from_traces(
            &app,
            &seeded,
            json!({ "items": [ { "trace_id": T1, "input": 1 } ] }),
        )
        .await;
        assert_eq!(unknown_field.status, 400);

        let dataset: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(dataset["current_version"], 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bulk_add_asks_the_deletion_guard_for_each_trace() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        faq_with_two_traces(&app, &seeded).await;
        let trace: TraceId = T1.parse().unwrap();
        app.state.deletions.guard.add_traces(
            seeded.project.id,
            &[trace],
            Uuid::now_v7(),
            app.state.clock.now_ns(),
        );

        let response = from_traces(
            &app,
            &seeded,
            json!({ "items": [ { "trace_id": T1 }, { "trace_id": T9 } ] }),
        )
        .await;
        assert_eq!(response.status, 201);
        let result: Value = response.json();
        assert_eq!(
            result["version"]["description"],
            format!("add from trace {T9}")
        );
        assert_eq!(result["items"].as_array().unwrap().len(), 1);
        assert_eq!(result["items"][0]["source_trace_id"], T9);
        let skipped = result["skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0]["index"], 0);
        assert_eq!(skipped[0]["message"], "a deletion blocks this trace");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bulk_add_checks_the_schema_of_each_copy() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.load_example_dataset(seeded.project.id).await;
        let created = create(
            &app,
            &seeded,
            json!({ "name": "faq", "input_schema": { "type": "object" } }),
        )
        .await;
        assert_eq!(created.status, 201);

        // The roots' inputs are strings, which the object schema refuses.
        let response = from_traces(
            &app,
            &seeded,
            json!({ "items": [ { "trace_id": T1 }, { "trace_id": T1 } ] }),
        )
        .await;
        assert_eq!(response.status, 400);
        let errors = field_errors(&response);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert_eq!(errors[0].0, "items[0]");
        assert_eq!(errors[1].0, "items[1]");
        let dataset: Value = get(&app, &seeded, "faq").await.json();
        assert_eq!(dataset["current_version"], 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bulk_add_needs_manage_datasets() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        faq_with_traces(&app, &seeded).await;
        let response = app
            .request(Method::POST, &from_traces_path(&seeded))
            .header("authorization", &format!("Bearer {}", seeded.read_key))
            .json(&json!({ "items": [ { "trace_id": T1 } ] }))
            .send()
            .await;
        assert_eq!(response.status, 403);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_j_route_is_unchanged() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        faq_with_traces(&app, &seeded).await;

        let response = from_traces(&app, &seeded, json!({ "items": [ { "trace_id": T1 } ] })).await;

        assert_eq!(response.status, 201);
        let result: Value = response.json();
        assert_eq!(result["items"][0]["source_trace_id"], T1);
        assert_eq!(result["items"][0]["metadata"], json!({}));
    }

    /// `GET …/dataset-trends?ids=…`.
    async fn get_trends(app: &TestApp, seeded: &Seeded, ids: &str) -> TestResponse {
        app.request(
            Method::GET,
            &format!(
                "/api/v1/projects/{}/dataset-trends?ids={ids}",
                seeded.project.id
            ),
        )
        .header("cookie", &seeded.cookie)
        .send()
        .await
    }

    /// Starts an experiment of `dataset` (a name) and returns its id.
    async fn start_experiment(app: &TestApp, seeded: &Seeded, name: &str, dataset: &str) -> Uuid {
        let response = app
            .request(
                Method::POST,
                &format!("/api/v1/projects/{}/experiments", seeded.project.id),
            )
            .header("cookie", &seeded.cookie)
            .json(&json!({ "name": name, "dataset": dataset }))
            .send()
            .await;
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let created: Value = response.json();
        created["experiment"]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }

    /// Commits one score of `experiment` on `trace` through storage, as an experiment writes it.
    async fn commit_experiment_score(
        app: &TestApp,
        seeded: &Seeded,
        experiment: Uuid,
        trace: TraceId,
        name: &str,
        value: cetra_core::score::ScoreValue,
    ) {
        let now = app.state.clock.now_ns();
        let score = cetra_core::score::Score {
            project_id: seeded.project.id,
            score_id: Uuid::now_v7(),
            target: cetra_core::score::ScoreTarget::Trace { trace_id: trace },
            session_id: None,
            user_id: None,
            name: name.to_owned(),
            value,
            reason: None,
            source: cetra_core::enums::ScoreSource::Api,
            evaluator_id: None,
            rule_id: None,
            experiment_id: Some(experiment),
            dataset_item_id: None,
            queue_id: None,
            author_user_id: None,
            metadata: std::collections::BTreeMap::new(),
            created_at: now,
            updated_at: now,
            deleted: false,
            ingest_time: now,
        };
        let batch = cetra_core::batch::scores_to_batch(&[score]).unwrap();
        app.state
            .storage
            .append_scores(seeded.project.id, batch)
            .await
            .unwrap();
    }

    /// The id of the dataset `name`.
    async fn dataset_id(app: &TestApp, seeded: &Seeded, name: &str) -> String {
        let dataset: Value = get(app, seeded, name).await.json();
        dataset["id"].as_str().unwrap().to_owned()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn trends_list_the_latest_experiments_with_score_means() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        create_ok(&app, &seeded, "other").await;
        post_items(
            &app,
            &seeded,
            "other",
            json!({ "items": [ { "input": "o" } ] }),
        )
        .await;
        let items = item_ids(
            &post_items(
                &app,
                &seeded,
                "faq",
                json!({ "items": [ { "input": "a" }, { "input": "b" } ] }),
            )
            .await,
        );
        // Eleven experiments: the oldest falls out of the latest ten.
        let mut experiments = Vec::new();
        for index in 0..11 {
            experiments.push(start_experiment(&app, &seeded, &format!("e{index}"), "faq").await);
        }
        let newest = experiments[10];
        let results = app
            .request(
                Method::POST,
                &format!(
                    "/api/v1/projects/{}/experiments/{newest}/items",
                    seeded.project.id
                ),
            )
            .header("cookie", &seeded.cookie)
            .json(&json!({ "results": [
                { "dataset_item_id": items[0], "repetition": 0, "output": "x" },
                { "dataset_item_id": items[1], "repetition": 0, "output": "y", "error": "boom" },
            ] }))
            .send()
            .await;
        assert_eq!(
            results.status,
            200,
            "{}",
            String::from_utf8_lossy(&results.body)
        );
        let trace: TraceId = T1.parse().unwrap();
        for value in [0.5, 1.0] {
            commit_experiment_score(
                &app,
                &seeded,
                newest,
                trace,
                "accuracy",
                cetra_core::score::ScoreValue::Numeric(value),
            )
            .await;
        }
        commit_experiment_score(
            &app,
            &seeded,
            newest,
            trace,
            "passed",
            cetra_core::score::ScoreValue::Boolean(true),
        )
        .await;
        let other = start_experiment(&app, &seeded, "o1", "other").await;

        let faq_id = dataset_id(&app, &seeded, "faq").await;
        let other_id = dataset_id(&app, &seeded, "other").await;
        let response = get_trends(
            &app,
            &seeded,
            &format!("{faq_id},{other_id},{}", Uuid::now_v7()),
        )
        .await;
        assert_eq!(
            response.status,
            200,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let trends: Value = response.json();
        assert_eq!(trends["scores_unavailable"], false);
        let data = trends["data"].as_array().unwrap();
        assert_eq!(data.len(), 2, "an unknown id is left out");
        assert_eq!(data[0]["dataset_id"], json!(faq_id));
        assert_eq!(data[1]["dataset_id"], json!(other_id));

        let faq = data[0]["experiments"].as_array().unwrap();
        assert_eq!(faq.len(), 10);
        assert_eq!(faq[0]["id"], json!(newest));
        assert_eq!(faq[0]["name"], "e10");
        assert_eq!(faq[0]["result_count"], 2);
        assert_eq!(faq[0]["error_count"], 1);
        let scores = faq[0]["scores"].as_array().unwrap();
        assert_eq!(scores.len(), 2, "{scores:?}");
        assert_eq!(scores[0]["name"], "accuracy");
        assert_eq!(scores[0]["data_type"], "numeric");
        assert_eq!(scores[0]["count"], 2);
        assert!((scores[0]["mean"].as_f64().unwrap() - 0.75).abs() < 1e-9);
        assert_eq!(scores[1]["name"], "passed");
        assert_eq!(scores[1]["data_type"], "boolean");
        assert_eq!(scores[1]["mean"], 1.0);
        assert_eq!(faq[9]["name"], "e1");

        let others = data[1]["experiments"].as_array().unwrap();
        assert_eq!(others.len(), 1);
        assert_eq!(others[0]["id"], json!(other));
        assert!(others[0]["scores"].as_array().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn trends_leave_out_hidden_experiments() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        post_items(
            &app,
            &seeded,
            "faq",
            json!({ "items": [ { "input": "a" } ] }),
        )
        .await;
        start_experiment(&app, &seeded, "old", "faq").await;
        // Two days on, a one-day window no longer reaches the first experiment.
        set_limits(
            &app,
            seeded.org_id,
            "2026-01-01T00:00:00Z",
            &json!({ "data_access_days": 1 }),
        );
        app.state.clock.set(app.state.clock.now_ns() + 2 * DAY_NS);
        start_experiment(&app, &seeded, "fresh", "faq").await;

        let faq_id = dataset_id(&app, &seeded, "faq").await;
        let trends: Value = get_trends(&app, &seeded, &faq_id).await.json();
        let names: Vec<&str> = trends["data"][0]["experiments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|experiment| experiment["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["fresh"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_dataset_named_experiment_trends_is_still_a_dataset() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "experiment-trends").await;

        let response = get(&app, &seeded, "experiment-trends").await;
        assert_eq!(response.status, 200);
        let dataset: Value = response.json();
        assert_eq!(dataset["name"], "experiment-trends");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn trends_refuse_a_bad_id_list_and_need_only_view() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, "faq").await;
        let faq_id = dataset_id(&app, &seeded, "faq").await;

        for ids in [
            String::new(),
            "faq".to_owned(),
            format!("{faq_id},{faq_id}"),
        ] {
            let response = get_trends(&app, &seeded, &ids).await;
            assert_eq!(response.status, 400, "{ids}");
            assert_eq!(field_errors(&response)[0].0, "ids", "{ids}");
        }
        let many: Vec<String> = (0..101).map(|_| Uuid::now_v7().to_string()).collect();
        let too_many = get_trends(&app, &seeded, &many.join(",")).await;
        assert_eq!(too_many.status, 400);

        let read = app
            .request(
                Method::GET,
                &format!(
                    "/api/v1/projects/{}/dataset-trends?ids={faq_id}",
                    seeded.project.id
                ),
            )
            .header("authorization", &format!("Bearer {}", seeded.read_key))
            .send()
            .await;
        assert_eq!(read.status, 200);
    }

    #[tokio::test]
    async fn from_trace_logs_hold_no_content() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        faq_with_traces(&app, &seeded).await;
        let (status, logs) = capture_logs(async {
            from_trace(
                &app,
                &seeded,
                json!({ "trace_id": T1, "input": "canary-trace-3" }),
            )
            .await
            .status
        })
        .await;
        assert_eq!(status, 201);
        assert!(!logs.contains("canary-trace-3"), "{logs}");
    }
}
