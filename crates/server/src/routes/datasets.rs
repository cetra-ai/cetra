//! Dataset routes (spec 09 § Datasets & experiments, D-J-2 to D-J-5). Project readers list and
//! read; `ManageDatasets` writes (member and above, a `write` key, a read-write token).

use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::{Method, StatusCode};
use axum::routing::{delete, get, patch, post};
use llmobs_core::ids::ProjectId;
use serde::Deserialize;
use serde_json::Value;
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::api::common::{FieldError, Page};
use crate::api::datasets::{
    AddItemsRequest, AddItemsResponse, CreateDatasetRequest, Dataset, DatasetItem,
    DatasetItemListParams, DatasetItemParams, DatasetListParams, DatasetVersion,
    DatasetVersionListParams, NewDatasetItem, UpdateDatasetRequest, UpdateItemRequest,
};
use crate::auth::principal::Principal;
use crate::auth::roles::ProjectPerm;
use crate::datasets::{
    ItemValues, MAX_BATCH_ITEMS, MAX_SCHEMA_CHECK_BYTES, MAX_SCHEMA_FAILURES, Schemas,
    check_description, check_item, check_name, check_schema, json_bytes,
};
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::meta::MetaError;
use crate::meta::datasets as store;
use crate::meta::datasets::{
    DatasetFields, DatasetKey, DatasetRow, ItemRow, NewRevision, VersionChange,
};

use super::orgs::page_named;

/// Bodies of dataset create and update: a name, a description and two schemas of ≤ 64 KiB.
const DATASET_BODY_LIMIT: usize = 256 * 1024;

/// A batch add: up to 1 000 items of at most 1 MiB, within 16 MiB (D-J-4).
const ITEMS_BODY_LIMIT: usize = 16 * 1024 * 1024;

/// One item edit: an item of at most 1 MiB plus JSON overhead.
const ITEM_BODY_LIMIT: usize = 2 * 1024 * 1024;

/// Page size of the live-item read when a new schema is checked.
const SCHEMA_PAGE: u32 = 1_000;

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
    ]
}

/// The `{dataset}` segment: an id or a name (D-J-4).
#[derive(Debug, Deserialize)]
pub(crate) struct DatasetPath {
    pub(crate) dataset: String,
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
        Principal::ControlPlane { .. } => None,
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
    let rows = store::list_datasets(state.meta.reader(), access.project.id).await?;
    let needle = params
        .q
        .as_deref()
        .filter(|q| !q.trim().is_empty())
        .map(str::to_lowercase);
    let mut items = Vec::new();
    for row in rows {
        let lower = row.name.to_lowercase();
        if needle
            .as_deref()
            .is_some_and(|needle| !lower.contains(needle))
        {
            continue;
        }
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
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Json(body): error::Json<CreateDatasetRequest>,
) -> Result<(StatusCode, error::Json<Dataset>), ApiError> {
    let fields = create_fields(&body).map_err(bad_request)?;
    let id = Uuid::now_v7();
    let project = access.project.id;
    let now = state.clock.now_ns();
    let row = state
        .meta
        .write(move |conn| {
            Box::pin(async move { store::insert_dataset(conn, id, project, fields, now).await })
        })
        .await?;
    let actor = created_by(&principal);
    tracing::info!(
        target: "audit",
        dataset_id = %id,
        project_id = %project,
        actor = %actor.as_deref().unwrap_or("-"),
        "dataset created"
    );
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
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(DatasetPath { dataset }): error::Path<DatasetPath>,
    error::Json(body): error::Json<UpdateDatasetRequest>,
) -> Result<error::Json<Dataset>, ApiError> {
    let checked = check_update(&body).map_err(bad_request)?;
    let project = access.project.id;
    let key = DatasetKey::parse(&dataset);
    let now = state.clock.now_ns();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move { update_tx(conn, project, key, checked, now).await })
        })
        .await?;
    let row = match outcome {
        Ok(row) => row,
        Err(WriteOutcome::NotFound) => return Err(ApiError::NotFound),
        Err(WriteOutcome::Invalid(errors)) => return Err(bad_request(errors)),
    };
    let actor = created_by(&principal);
    tracing::info!(
        target: "audit",
        dataset_id = %row.id,
        project_id = %project,
        actor = %actor.as_deref().unwrap_or("-"),
        "dataset updated"
    );
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
        if let Some(errors) = check_live_items(conn, row.id, field, &schemas).await? {
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

/// Checks the live items of the dataset against `schemas`. `Some(errors)` lists the first
/// [`MAX_SCHEMA_FAILURES`] failures, or the one "too large" error when the items hold more than
/// [`MAX_SCHEMA_CHECK_BYTES`] of JSON. `field` names the schema that changed.
async fn check_live_items(
    conn: &mut SqliteConnection,
    dataset: Uuid,
    field: &str,
    schemas: &Schemas,
) -> Result<Option<Vec<FieldError>>, MetaError> {
    let mut errors = Vec::new();
    let mut bytes = 0usize;
    let mut after = None;
    loop {
        let mut page = store::live_items_tx(conn, dataset, after, SCHEMA_PAGE).await?;
        let more = page.len() > SCHEMA_PAGE as usize;
        page.truncate(SCHEMA_PAGE as usize);
        for item in &page {
            bytes += json_bytes(&item.input, item.expected_output.as_ref(), &item.metadata);
            if bytes > MAX_SCHEMA_CHECK_BYTES {
                return Ok(Some(vec![FieldError {
                    field: field.to_owned(),
                    message: "the dataset is too large to validate against a new schema".to_owned(),
                    position: None,
                }]));
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
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(DatasetPath { dataset }): error::Path<DatasetPath>,
) -> Result<StatusCode, ApiError> {
    let project = access.project.id;
    let id = find(&state, project, &dataset).await?.id;
    let deleted = state
        .meta
        .write(move |conn| Box::pin(async move { store::delete_dataset(conn, project, id).await }))
        .await?;
    if !deleted {
        return Err(ApiError::NotFound);
    }
    let actor = created_by(&principal);
    tracing::info!(
        target: "audit",
        dataset_id = %id,
        project_id = %project,
        actor = %actor.as_deref().unwrap_or("-"),
        "dataset deleted"
    );
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

/// `?version=`: 0 and the default both mean the current version. Any other value must be
/// between 1 and the current version.
fn version_of(row: &DatasetRow, requested: Option<i64>) -> Result<i64, ApiError> {
    match requested {
        None | Some(0) => Ok(row.current_version),
        Some(version) if (1..=row.current_version).contains(&version) => Ok(version),
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
        })
        .collect();
    Ok(error::Json(Page { data, next_cursor }))
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
    let limit = params.limit.get();
    let mut items = store::list_items(state.meta.reader(), row.id, version, after, limit).await?;
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
        target: "audit",
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

async fn update_item(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(ItemPath { dataset, item_id }): error::Path<ItemPath>,
    error::Json(body): error::Json<UpdateItemRequest>,
) -> Result<error::Json<DatasetItem>, ApiError> {
    let project = access.project.id;
    let key = DatasetKey::parse(&dataset);
    let now = state.clock.now_ns();
    let created_by = created_by(&principal);
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                update_item_tx(conn, project, key, item_id, body, created_by, now).await
            })
        })
        .await?;
    match outcome {
        Ok(item) => Ok(error::Json(item_json(&item))),
        Err(WriteOutcome::NotFound) => Err(ApiError::NotFound),
        Err(WriteOutcome::Invalid(errors)) => Err(bad_request(errors)),
    }
}

/// The item edit's write transaction. A body that changes nothing writes no version and answers
/// the live revision (D-J-3). Otherwise the live revision closes and its replacement is
/// inserted in one new version; the source ids of the item are kept (D-J-9).
async fn update_item_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    key: DatasetKey,
    item: Uuid,
    body: UpdateItemRequest,
    created_by: Option<String>,
    now: i64,
) -> Result<Result<ItemRow, WriteOutcome>, MetaError> {
    let Some(row) = store::get_dataset_tx(conn, project, &key).await? else {
        return Ok(Err(WriteOutcome::NotFound));
    };
    let Some(live) = store::get_live_item_tx(conn, row.id, item).await? else {
        return Ok(Err(WriteOutcome::NotFound));
    };
    let values = ItemValues {
        input: body.input.unwrap_or_else(|| live.input.clone()),
        expected_output: match body.expected_output {
            None => live.expected_output.clone(),
            Some(value) => value,
        },
        metadata: body.metadata.unwrap_or_else(|| live.metadata.clone()),
    };
    let unchanged = values.input == live.input
        && values.expected_output == live.expected_output
        && values.metadata == live.metadata;
    if unchanged {
        return Ok(Ok(live));
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
    store::write_version(conn, row.id, change, now).await?;
    let updated = store::get_live_item_tx(conn, row.id, item)
        .await?
        .ok_or(MetaError::Corrupt {
            table: "dataset_items",
            column: "item_id",
        })?;
    Ok(Ok(updated))
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
    let created_by = created_by(&principal);
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(
                async move { delete_item_tx(conn, project, key, item_id, created_by, now).await },
            )
        })
        .await?;
    match outcome {
        Ok(version) => Ok(error::Json(version)),
        // The delete writes no item values, so it has no `Invalid` outcome.
        Err(_) => Err(ApiError::NotFound),
    }
}

/// The item delete's write transaction: the live revision closes in one new version that has
/// no revision of its own.
async fn delete_item_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    key: DatasetKey,
    item: Uuid,
    created_by: Option<String>,
    now: i64,
) -> Result<Result<DatasetVersion, WriteOutcome>, MetaError> {
    let Some(row) = store::get_dataset_tx(conn, project, &key).await? else {
        return Ok(Err(WriteOutcome::NotFound));
    };
    if store::get_live_item_tx(conn, row.id, item).await?.is_none() {
        return Ok(Err(WriteOutcome::NotFound));
    }
    let description = format!("delete item {item}");
    let change = VersionChange {
        close: vec![item],
        insert: Vec::new(),
        description: description.clone(),
        created_by: created_by.clone(),
    };
    let version = store::write_version(conn, row.id, change, now).await?;
    Ok(Ok(DatasetVersion {
        version,
        description,
        created_by,
        created_at: now,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::*;
    use crate::auth::roles::OrgRole;
    use crate::test_support::{Seeded, TestApp, TestResponse, capture_logs};

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
        assert!(logs.contains("actor=key:"), "{logs}");
        assert!(logs.contains(&id), "{logs}");
        assert!(!logs.contains("canary-dataset-name"), "{logs}");
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
}
