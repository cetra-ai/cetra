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
    CreateDatasetRequest, Dataset, DatasetListParams, UpdateDatasetRequest,
};
use crate::auth::principal::Principal;
use crate::auth::roles::ProjectPerm;
use crate::datasets::{
    ItemValues, MAX_SCHEMA_CHECK_BYTES, MAX_SCHEMA_FAILURES, Schemas, check_description,
    check_item, check_name, check_schema, json_bytes,
};
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::meta::MetaError;
use crate::meta::datasets as store;
use crate::meta::datasets::{DatasetFields, DatasetKey, DatasetRow};

use super::orgs::page_named;

/// Bodies of dataset create and update: a name, a description and two schemas of ≤ 64 KiB.
const DATASET_BODY_LIMIT: usize = 256 * 1024;

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
    ]
}

/// The `{dataset}` segment: an id or a name (D-J-4).
#[derive(Debug, Deserialize)]
pub(crate) struct DatasetPath {
    pub(crate) dataset: String,
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

/// What the update's write transaction answers when it writes nothing.
enum UpdateOutcome {
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
        Err(UpdateOutcome::NotFound) => return Err(ApiError::NotFound),
        Err(UpdateOutcome::Invalid(errors)) => return Err(bad_request(errors)),
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
) -> Result<Result<DatasetRow, UpdateOutcome>, MetaError> {
    let Some(row) = store::get_dataset_tx(conn, project, &key).await? else {
        return Ok(Err(UpdateOutcome::NotFound));
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
            return Ok(Err(UpdateOutcome::Invalid(errors)));
        }
    }
    Ok(
        match store::update_dataset(conn, project, row.id, fields, now).await? {
            Some(row) => Ok(row),
            None => Err(UpdateOutcome::NotFound),
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

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::*;
    use crate::auth::roles::OrgRole;
    use crate::meta::datasets::{NewRevision, VersionChange};
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
}
