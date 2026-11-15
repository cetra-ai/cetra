//! Experiment routes (spec 09 § Datasets & experiments, D-J-10, D-J-14). Project readers list and
//! read; `ManageDatasets` writes. A hidden experiment (D-J-14) is 404 on every route but `DELETE`.

use std::collections::{HashMap, HashSet};

use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::{Method, StatusCode};
use axum::routing::{delete, get, post};
use llmobs_core::ids::{ProjectId, TraceId};
use serde::Deserialize;
use serde_json::{Map, Value};
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::api::common::{FieldError, Page};
use crate::api::experiments::{
    CREATE_ITEMS_PAGE, CreateExperimentRequest, CreateExperimentResponse, Experiment,
    ExperimentListParams, ExperimentResult, ExperimentResultsRequest, ExperimentResultsResponse,
    ExperimentStatus, FinishExperimentRequest, MAX_ERROR_CHARS, MAX_EXPERIMENT_ROWS,
    MAX_METADATA_BYTES, MAX_NAME_CHARS, MAX_OUTPUT_BYTES, MAX_OUTPUTS_PER_REQUEST, MAX_REPETITIONS,
    MAX_RESULTS_PER_REQUEST, RESULTS_BODY_LIMIT,
};
use crate::auth::principal::Principal;
use crate::auth::roles::ProjectPerm;
use crate::datasets::check_description;
use crate::experiments::visible_from;
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::meta::MetaError;
use crate::meta::datasets as datasets_store;
use crate::meta::datasets::DatasetKey;
use crate::meta::experiments::{
    self as store, ExperimentRow, NewExperiment, NewResult, PinnedItem,
};

use super::datasets::{created_by, find, item_json};

/// Body limit of `POST …/experiments`: the name, the description and 64 KiB of metadata.
const CREATE_BODY_LIMIT: usize = 256 * 1024;

/// Experiment routes. Reads need project view; writes need [`ProjectPerm::ManageDatasets`].
/// Not session-only.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/experiments",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_experiments),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/experiments",
            access: Access::Project {
                perm: ProjectPerm::ManageDatasets,
                session_only: false,
            },
            documented: true,
            handler: post(create_experiment).layer(DefaultBodyLimit::max(CREATE_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/experiments/{id}",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_experiment),
        },
        RouteDef {
            method: Method::DELETE,
            path: "/api/v1/projects/{project}/experiments/{id}",
            access: Access::Project {
                perm: ProjectPerm::ManageDatasets,
                session_only: false,
            },
            documented: true,
            handler: delete(delete_experiment),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/experiments/{id}/items",
            access: Access::Project {
                perm: ProjectPerm::ManageDatasets,
                session_only: false,
            },
            documented: true,
            handler: post(post_results).layer(DefaultBodyLimit::max(RESULTS_BODY_LIMIT)),
        },
        RouteDef {
            method: Method::POST,
            path: "/api/v1/projects/{project}/experiments/{id}/finish",
            access: Access::Project {
                perm: ProjectPerm::ManageDatasets,
                session_only: false,
            },
            documented: true,
            handler: post(finish_experiment),
        },
    ]
}

/// The `{id}` segment of an experiment route.
#[derive(Debug, Deserialize)]
struct ExperimentPath {
    id: Uuid,
}

fn field_error(field: &str, message: impl Into<String>) -> FieldError {
    FieldError {
        field: field.to_owned(),
        message: message.into(),
        position: None,
    }
}

/// A body that failed its checks (D-J-10).
fn invalid(errors: Vec<FieldError>) -> ApiError {
    ApiError::BadRequest {
        detail: Some("invalid experiment".to_owned()),
        errors,
    }
}

fn invalid_cursor() -> ApiError {
    ApiError::BadRequest {
        detail: None,
        errors: vec![field_error("cursor", "invalid cursor")],
    }
}

fn corrupt(column: &'static str) -> ApiError {
    ApiError::from(MetaError::Corrupt {
        table: "experiments",
        column,
    })
}

/// API form of a stored experiment. Stored metadata or status that does not decode is
/// `MetaError::Corrupt`.
fn experiment_json(row: &ExperimentRow) -> Result<Experiment, ApiError> {
    let metadata: Map<String, Value> =
        serde_json::from_str(&row.metadata).map_err(|_| corrupt("metadata"))?;
    let status = match row.status.as_str() {
        "running" => ExperimentStatus::Running,
        "completed" => ExperimentStatus::Completed,
        "failed" => ExperimentStatus::Failed,
        _ => return Err(corrupt("status")),
    };
    Ok(Experiment {
        id: row.id,
        name: row.name.clone(),
        description: row.description.clone(),
        dataset_id: row.dataset_id,
        dataset_name: row.dataset_name.clone(),
        dataset_version: row.dataset_version,
        metadata,
        repetitions: row.repetitions,
        item_count: row.item_count,
        result_count: row.result_count,
        status,
        error: row.error.clone(),
        created_at: row.created_at,
        finished_at: row.finished_at,
    })
}

/// A cursor is an experiment id (UUIDv7).
fn parse_cursor(cursor: Option<&str>) -> Result<Option<Uuid>, ApiError> {
    cursor
        .map(|raw| raw.parse::<Uuid>().map_err(|_| invalid_cursor()))
        .transpose()
}

/// The checked fields of a create body.
struct CreateFields {
    name: String,
    description: Option<String>,
    /// JSON object text.
    metadata: String,
    repetitions: i64,
}

/// Every field error of a create body, or the checked fields.
fn create_fields(body: &CreateExperimentRequest) -> Result<CreateFields, Vec<FieldError>> {
    let mut errors = Vec::new();
    let name = body.name.trim();
    if !(1..=MAX_NAME_CHARS).contains(&name.chars().count()) {
        errors.push(field_error("name", "name must be 1 to 200 characters"));
    }
    let description = match &body.description {
        Some(text) => match check_description(text) {
            Ok(value) => value,
            Err(error) => {
                errors.push(error);
                None
            }
        },
        None => None,
    };
    let metadata = match &body.metadata {
        None => "{}".to_owned(),
        Some(map) => match serde_json::to_string(map) {
            Ok(text) if text.len() <= MAX_METADATA_BYTES => text,
            Ok(_) => {
                errors.push(field_error("metadata", "metadata must be at most 64 KiB"));
                String::new()
            }
            Err(_) => {
                errors.push(field_error("metadata", "metadata is not valid JSON"));
                String::new()
            }
        },
    };
    let repetitions = body.repetitions.unwrap_or(1);
    if !(1..=MAX_REPETITIONS).contains(&repetitions) {
        errors.push(field_error("repetitions", "repetitions must be 1 to 20"));
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(CreateFields {
        name: name.to_owned(),
        description,
        metadata,
        repetitions,
    })
}

/// What the create's write transaction needs from the request and the server.
struct CreateInput {
    fields: CreateFields,
    requested_version: Option<i64>,
    id: Uuid,
    created_at: i64,
    created_by: Option<String>,
}

/// What the create's write transaction refuses. Nothing is written for either.
enum CreateRefusal {
    NotFound,
    Invalid(Vec<FieldError>),
}

async fn list_experiments(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<ExperimentListParams>,
) -> Result<error::Json<Page<Experiment>>, ApiError> {
    let project = access.project.id;
    let visible = visible_from(&state, access.project.org_id);
    let dataset = match &params.dataset {
        Some(raw) => Some(find(&state, project, raw).await?.id),
        None => None,
    };
    let cursor = parse_cursor(params.cursor.as_deref())?;
    let (rows, next) = store::list(
        state.meta.reader(),
        project,
        dataset,
        visible,
        params.limit.get(),
        cursor,
    )
    .await?;
    let data = rows
        .iter()
        .map(experiment_json)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(error::Json(Page {
        data,
        next_cursor: next.map(|id| id.to_string()),
    }))
}

async fn get_experiment(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(ExperimentPath { id }): error::Path<ExperimentPath>,
) -> Result<error::Json<Experiment>, ApiError> {
    let visible = visible_from(&state, access.project.org_id);
    let row = store::get(state.meta.reader(), access.project.id, id, visible)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(error::Json(experiment_json(&row)?))
}

async fn create_experiment(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Json(body): error::Json<CreateExperimentRequest>,
) -> Result<(StatusCode, error::Json<CreateExperimentResponse>), ApiError> {
    let fields = create_fields(&body).map_err(invalid)?;
    let project = access.project.id;
    let key = DatasetKey::parse(&body.dataset);
    let actor = created_by(&principal);
    let input = CreateInput {
        fields,
        requested_version: body.dataset_version,
        id: Uuid::now_v7(),
        created_at: state.clock.now_ns(),
        created_by: actor.clone(),
    };
    let outcome = state
        .meta
        .write(move |conn| Box::pin(async move { create_tx(conn, project, &key, input).await }))
        .await?;
    let row = match outcome {
        Ok(row) => row,
        Err(CreateRefusal::NotFound) => return Err(ApiError::NotFound),
        Err(CreateRefusal::Invalid(errors)) => return Err(invalid(errors)),
    };
    let experiment = experiment_json(&row)?;
    let dataset_id = row.dataset_id.ok_or_else(|| corrupt("dataset_id"))?;
    // The first page of the pinned version. The version is immutable, so reading it after the
    // write answers the same items the write pinned.
    let mut rows = datasets_store::list_items(
        state.meta.reader(),
        dataset_id,
        row.dataset_version,
        None,
        CREATE_ITEMS_PAGE,
    )
    .await?;
    let page = CREATE_ITEMS_PAGE as usize;
    let more = rows.len() > page;
    rows.truncate(page);
    let next_cursor = if more {
        rows.last().map(|last| last.item_id.to_string())
    } else {
        None
    };
    let items = rows.iter().map(item_json).collect();
    tracing::info!(
        target: "audit",
        experiment_id = %row.id,
        dataset_id = %dataset_id,
        project_id = %project,
        actor = %actor.as_deref().unwrap_or("-"),
        item_count = row.item_count,
        repetitions = row.repetitions,
        "experiment created"
    );
    Ok((
        StatusCode::CREATED,
        error::Json(CreateExperimentResponse {
            experiment,
            items,
            next_cursor,
        }),
    ))
}

/// The create's write transaction (D-J-10): the dataset is read, the version and its item count
/// checked, and the experiment inserted, all in one transaction, so a dataset deleted in
/// between is a 404 and never a foreign-key failure.
async fn create_tx(
    conn: &mut SqliteConnection,
    project: ProjectId,
    key: &DatasetKey,
    input: CreateInput,
) -> Result<Result<ExperimentRow, CreateRefusal>, MetaError> {
    let Some(dataset) = datasets_store::get_dataset_tx(conn, project, key).await? else {
        return Ok(Err(CreateRefusal::NotFound));
    };
    if dataset.current_version == 0 {
        return Ok(Err(CreateRefusal::Invalid(vec![field_error(
            "dataset",
            "the dataset has no items",
        )])));
    }
    let version = match input.requested_version {
        None => dataset.current_version,
        Some(version) if (1..=dataset.current_version).contains(&version) => version,
        Some(_) => {
            return Ok(Err(CreateRefusal::Invalid(vec![field_error(
                "dataset_version",
                format!("dataset_version must be 1 to {}", dataset.current_version),
            )])));
        }
    };
    let item_count = store::count_version_items_tx(conn, dataset.id, version).await?;
    if item_count == 0 {
        return Ok(Err(CreateRefusal::Invalid(vec![field_error(
            "dataset_version",
            "the version has no items",
        )])));
    }
    if item_count.saturating_mul(input.fields.repetitions) > MAX_EXPERIMENT_ROWS {
        return Ok(Err(CreateRefusal::Invalid(vec![field_error(
            "repetitions",
            "items × repetitions must be at most 20000",
        )])));
    }
    store::insert_tx(
        conn,
        &NewExperiment {
            id: input.id,
            project_id: project,
            dataset_id: dataset.id,
            dataset_name: dataset.name,
            dataset_version: version,
            name: input.fields.name,
            description: input.fields.description,
            metadata: input.fields.metadata,
            repetitions: input.fields.repetitions,
            item_count,
            created_by: input.created_by,
            created_at: input.created_at,
        },
    )
    .await?;
    let row = store::get_tx(conn, project, input.id, None)
        .await?
        .ok_or(MetaError::Corrupt {
            table: "experiments",
            column: "id",
        })?;
    Ok(Ok(row))
}

async fn delete_experiment(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(ExperimentPath { id }): error::Path<ExperimentPath>,
) -> Result<StatusCode, ApiError> {
    let project = access.project.id;
    // The plan window does not apply: deleting reveals nothing, and the rows hold outputs (D-J-14).
    let deleted = state
        .meta
        .write(move |conn| Box::pin(async move { store::delete_tx(conn, project, id).await }))
        .await?;
    if !deleted {
        return Err(ApiError::NotFound);
    }
    let actor = created_by(&principal);
    tracing::info!(
        target: "audit",
        experiment_id = %id,
        project_id = %project,
        actor = %actor.as_deref().unwrap_or("-"),
        "experiment deleted"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// A result that passed the checks that need no stored state. `index` is its position in the
/// request, for field errors.
struct CheckedResult {
    index: usize,
    dataset_item_id: Uuid,
    repetition: i64,
    trace_id: Option<TraceId>,
    /// JSON text.
    output: Option<String>,
    error: Option<String>,
    latency_ns: Option<i64>,
}

/// Every check of a results batch that needs no stored state. Field errors name `results[i]`.
fn check_results(results: &[ExperimentResult]) -> Result<Vec<CheckedResult>, Vec<FieldError>> {
    if !(1..=MAX_RESULTS_PER_REQUEST).contains(&results.len()) {
        return Err(vec![field_error(
            "results",
            "results must hold 1 to 500 results",
        )]);
    }
    let mut errors = Vec::new();
    let mut seen = HashSet::new();
    let mut output_total = 0usize;
    let mut checked = Vec::with_capacity(results.len());
    for (index, result) in results.iter().enumerate() {
        if !seen.insert((result.dataset_item_id, result.repetition)) {
            errors.push(field_error(
                &format!("results[{index}].dataset_item_id"),
                "duplicate item and repetition",
            ));
        }
        let output = match &result.output {
            None => None,
            Some(value) => match serde_json::to_string(value) {
                Ok(text) if text.len() <= MAX_OUTPUT_BYTES => {
                    output_total += text.len();
                    Some(text)
                }
                Ok(_) => {
                    errors.push(field_error(
                        &format!("results[{index}].output"),
                        "output must be at most 256 KiB",
                    ));
                    None
                }
                Err(_) => {
                    errors.push(field_error(
                        &format!("results[{index}].output"),
                        "output is not valid JSON",
                    ));
                    None
                }
            },
        };
        if result
            .error
            .as_deref()
            .is_some_and(|text| text.chars().count() > MAX_ERROR_CHARS)
        {
            errors.push(field_error(
                &format!("results[{index}].error"),
                "error must be at most 4096 characters",
            ));
        }
        if result.latency_ns.is_some_and(|ns| ns < 0) {
            errors.push(field_error(
                &format!("results[{index}].latency_ns"),
                "latency_ns must be 0 or more",
            ));
        }
        checked.push(CheckedResult {
            index,
            dataset_item_id: result.dataset_item_id,
            repetition: result.repetition,
            trace_id: result.trace_id,
            output,
            error: result.error.clone(),
            latency_ns: result.latency_ns,
        });
    }
    if output_total > MAX_OUTPUTS_PER_REQUEST {
        errors.push(field_error(
            "results",
            "outputs must total at most 32 MiB per request",
        ));
    }
    if errors.is_empty() {
        Ok(checked)
    } else {
        Err(errors)
    }
}

/// What the results write refuses. Nothing is written for any of them.
enum ResultsRefusal {
    NotFound,
    Invalid(Vec<FieldError>),
}

async fn post_results(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(ExperimentPath { id }): error::Path<ExperimentPath>,
    error::Json(body): error::Json<ExperimentResultsRequest>,
) -> Result<error::Json<ExperimentResultsResponse>, ApiError> {
    let checked = check_results(&body.results).map_err(invalid)?;
    let project = access.project.id;
    let visible = visible_from(&state, access.project.org_id);
    let now = state.clock.now_ns();
    let writer = state.clone();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(
                async move { results_tx(conn, &writer, project, id, visible, checked, now).await },
            )
        })
        .await?;
    match outcome {
        Ok(response) => {
            tracing::debug!(
                experiment_id = %id,
                stored = response.stored,
                skipped = response.skipped,
                "experiment results stored"
            );
            Ok(error::Json(response))
        }
        Err(ResultsRefusal::NotFound) => Err(ApiError::NotFound),
        Err(ResultsRefusal::Invalid(errors)) => Err(invalid(errors)),
    }
}

/// The results write (D-J-10). The repetition check and the deletion guard run after the
/// experiment is read, inside this transaction. A deletion's resolve write (D-J-9) is serialized
/// with this transaction: a block recorded before the guard check below skips the result, and a
/// deletion recorded after it runs its cascade after this commit, which removes these rows.
async fn results_tx(
    conn: &mut SqliteConnection,
    state: &AppState,
    project: ProjectId,
    id: Uuid,
    visible: Option<i64>,
    results: Vec<CheckedResult>,
    now: i64,
) -> Result<Result<ExperimentResultsResponse, ResultsRefusal>, MetaError> {
    let Some(experiment) = store::get_tx(conn, project, id, visible).await? else {
        return Ok(Err(ResultsRefusal::NotFound));
    };
    let repetitions = experiment.repetitions;
    let mut errors = Vec::new();
    for result in &results {
        if !(0..repetitions).contains(&result.repetition) {
            errors.push(field_error(
                &format!("results[{}].repetition", result.index),
                format!("repetition must be 0 to {}", repetitions - 1),
            ));
        }
    }
    if !errors.is_empty() {
        return Ok(Err(ResultsRefusal::Invalid(errors)));
    }
    let mut kept = Vec::with_capacity(results.len());
    let mut skipped = 0i64;
    for result in results {
        let blocked = result.trace_id.is_some_and(|trace| {
            state
                .deletions
                .guard
                .blocking(project, trace, &[], now)
                .is_some()
        });
        if blocked {
            skipped += 1;
        } else {
            kept.push(result);
        }
    }
    if experiment.status != "running" {
        return Err(MetaError::Conflict("the experiment is finished"));
    }
    let Some(dataset) = experiment.dataset_id else {
        return Err(MetaError::Conflict("the experiment's dataset was deleted"));
    };
    let items: Vec<Uuid> = kept.iter().map(|result| result.dataset_item_id).collect();
    let pinned: HashMap<Uuid, PinnedItem> =
        store::pinned_items_tx(conn, dataset, experiment.dataset_version, &items).await?;
    let mut rows = Vec::with_capacity(kept.len());
    let mut missing = Vec::new();
    for result in kept {
        match pinned.get(&result.dataset_item_id) {
            Some(item) => rows.push(NewResult {
                dataset_item_id: result.dataset_item_id,
                repetition: result.repetition,
                revision_id: item.revision_id,
                input: item.input.clone(),
                expected_output: item.expected_output.clone(),
                trace_id: result.trace_id,
                output: result.output,
                error: result.error,
                latency_ns: result.latency_ns,
            }),
            None => missing.push(field_error(
                &format!("results[{}].dataset_item_id", result.index),
                "the item is not in the pinned version",
            )),
        }
    }
    if !missing.is_empty() {
        return Ok(Err(ResultsRefusal::Invalid(missing)));
    }
    let written = store::upsert_results_tx(conn, id, &rows, now).await?;
    Ok(Ok(ExperimentResultsResponse {
        stored: i64::try_from(written).unwrap_or(i64::MAX),
        skipped,
    }))
}

async fn finish_experiment(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(ExperimentPath { id }): error::Path<ExperimentPath>,
    error::Json(body): error::Json<FinishExperimentRequest>,
) -> Result<error::Json<Experiment>, ApiError> {
    if body
        .error
        .as_deref()
        .is_some_and(|text| text.chars().count() > MAX_ERROR_CHARS)
    {
        return Err(invalid(vec![field_error(
            "error",
            "error must be at most 4096 characters",
        )]));
    }
    let project = access.project.id;
    let visible = visible_from(&state, access.project.org_id);
    let now = state.clock.now_ns();
    let error = body.error;
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move { finish_write(conn, project, id, visible, error, now).await })
        })
        .await?;
    let Some((row, finished)) = outcome else {
        return Err(ApiError::NotFound);
    };
    if finished {
        let actor = created_by(&principal);
        tracing::info!(
            target: "audit",
            experiment_id = %id,
            project_id = %project,
            actor = %actor.as_deref().unwrap_or("-"),
            status = %row.status,
            "experiment finished"
        );
    }
    Ok(error::Json(experiment_json(&row)?))
}

/// The finish write (D-J-10). `None` when the experiment is absent or hidden. `Some((row, true))`
/// when this call finished it. A finished experiment with the same outcome answers unchanged
/// (`false`), so an SDK retry is safe; another outcome is a conflict.
async fn finish_write(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
    visible: Option<i64>,
    error: Option<String>,
    now: i64,
) -> Result<Option<(ExperimentRow, bool)>, MetaError> {
    let Some(current) = store::get_tx(conn, project, id, visible).await? else {
        return Ok(None);
    };
    if store::finish_tx(conn, id, error.as_deref(), now).await? {
        let row = store::get_tx(conn, project, id, visible)
            .await?
            .ok_or(MetaError::Corrupt {
                table: "experiments",
                column: "id",
            })?;
        return Ok(Some((row, true)));
    }
    let requested = if error.is_some() {
        "failed"
    } else {
        "completed"
    };
    if current.status != requested {
        return Err(MetaError::Conflict("the experiment is finished"));
    }
    Ok(Some((current, false)))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::api::datasets::AddItemsResponse;
    use crate::auth::roles::OrgRole;
    use crate::meta::experiments::ResultKey;
    use crate::test_support::{Seeded, TestApp, TestResponse, capture_logs, set_limits};

    fn base(seeded: &Seeded) -> String {
        format!("/api/v1/projects/{}/experiments", seeded.project.id)
    }

    async fn create(app: &TestApp, seeded: &Seeded, body: Value) -> TestResponse {
        app.request(Method::POST, &base(seeded))
            .header("cookie", &seeded.cookie)
            .json(&body)
            .send()
            .await
    }

    async fn create_ok(app: &TestApp, seeded: &Seeded, body: Value) -> CreateExperimentResponse {
        let response = create(app, seeded, body).await;
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        response.json()
    }

    /// The field names of a 400 answer.
    fn fields(response: &TestResponse) -> Vec<String> {
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
            .map(|error| error["field"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Creates dataset `name` and adds `count` items in batches of 1 000. Returns the item ids.
    async fn dataset_with_items(
        app: &TestApp,
        seeded: &Seeded,
        name: &str,
        count: usize,
    ) -> Vec<Uuid> {
        let project = seeded.project.id;
        let created = app
            .request(
                Method::POST,
                &format!("/api/v1/projects/{project}/datasets"),
            )
            .header("cookie", &seeded.cookie)
            .json(&json!({ "name": name }))
            .send()
            .await;
        assert_eq!(created.status, 201);
        let mut ids = Vec::new();
        let inputs: Vec<Value> = (0..count)
            .map(|i| json!({ "question": format!("q{i}") }))
            .collect();
        for chunk in inputs.chunks(1_000) {
            let items: Vec<Value> = chunk
                .iter()
                .map(|input| json!({ "input": input }))
                .collect();
            let response = app
                .request(
                    Method::POST,
                    &format!("/api/v1/projects/{project}/datasets/{name}/items"),
                )
                .header("cookie", &seeded.cookie)
                .json(&json!({ "items": items }))
                .send()
                .await;
            assert_eq!(
                response.status,
                201,
                "{}",
                String::from_utf8_lossy(&response.body)
            );
            let answer: AddItemsResponse = response.json();
            ids.extend(answer.item_ids);
        }
        ids
    }

    async fn post_results(
        app: &TestApp,
        seeded: &Seeded,
        experiment: Uuid,
        results: Value,
    ) -> TestResponse {
        app.request(
            Method::POST,
            &format!("{}/{experiment}/items", base(seeded)),
        )
        .header("cookie", &seeded.cookie)
        .json(&json!({ "results": results }))
        .send()
        .await
    }

    async fn finish(app: &TestApp, seeded: &Seeded, experiment: Uuid, body: Value) -> TestResponse {
        app.request(
            Method::POST,
            &format!("{}/{experiment}/finish", base(seeded)),
        )
        .header("cookie", &seeded.cookie)
        .json(&body)
        .send()
        .await
    }

    fn result(item: Uuid, repetition: i64, output: &str) -> Value {
        json!({ "dataset_item_id": item, "repetition": repetition, "output": output })
    }

    async fn stored(
        app: &TestApp,
        seeded: &Seeded,
        experiment: Uuid,
        results: Value,
    ) -> ExperimentResultsResponse {
        let response = post_results(app, seeded, experiment, results).await;
        assert_eq!(
            response.status,
            200,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        response.json()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_pins_the_version_and_returns_items() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        dataset_with_items(&app, &seeded, "faq", 3).await;
        let created = create_ok(&app, &seeded, json!({ "name": "run", "dataset": "faq" })).await;
        assert_eq!(created.experiment.dataset_version, 1);
        assert_eq!(created.experiment.item_count, 3);
        assert_eq!(created.experiment.status, ExperimentStatus::Running);
        assert_eq!(created.items.len(), 3);
        assert_eq!(created.next_cursor, None);

        let added = app
            .request(
                Method::POST,
                &format!("/api/v1/projects/{}/datasets/faq/items", seeded.project.id),
            )
            .header("cookie", &seeded.cookie)
            .json(&json!({ "items": [{ "input": { "question": "new" } }] }))
            .send()
            .await;
        assert_eq!(added.status, 201);
        let pinned = create_ok(
            &app,
            &seeded,
            json!({ "name": "pinned", "dataset": "faq", "dataset_version": 1 }),
        )
        .await;
        assert_eq!(pinned.experiment.item_count, 3);
        let latest = create_ok(&app, &seeded, json!({ "name": "latest", "dataset": "faq" })).await;
        assert_eq!(latest.experiment.dataset_version, 2);
        assert_eq!(latest.experiment.item_count, 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_validates() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        dataset_with_items(&app, &seeded, "faq", 3).await;
        dataset_with_items(&app, &seeded, "empty", 0).await;
        let big = json!({ "k": "x".repeat(65 * 1024) });
        let cases = [
            (json!({ "name": "  ", "dataset": "faq" }), "name"),
            (
                json!({ "name": "run", "dataset": "faq", "repetitions": 21 }),
                "repetitions",
            ),
            (
                json!({ "name": "run", "dataset": "faq", "repetitions": 0 }),
                "repetitions",
            ),
            (
                json!({ "name": "run", "dataset": "faq", "dataset_version": 9 }),
                "dataset_version",
            ),
            (
                json!({ "name": "run", "dataset": "faq", "dataset_version": 0 }),
                "dataset_version",
            ),
            (
                json!({ "name": "run", "dataset": "faq", "metadata": big }),
                "metadata",
            ),
            (json!({ "name": "run", "dataset": "empty" }), "dataset"),
        ];
        for (body, field) in cases {
            assert_eq!(fields(&create(&app, &seeded, body).await), vec![field]);
        }
        let missing = create(&app, &seeded, json!({ "name": "run", "dataset": "nope" })).await;
        assert_eq!(missing.status, 404);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_caps_items_times_repetitions() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        dataset_with_items(&app, &seeded, "big", 1_001).await;
        let refused = create(
            &app,
            &seeded,
            json!({ "name": "run", "dataset": "big", "repetitions": 20 }),
        )
        .await;
        assert_eq!(fields(&refused), vec!["repetitions"]);
        let created = create_ok(
            &app,
            &seeded,
            json!({ "name": "run", "dataset": "big", "repetitions": 19 }),
        )
        .await;
        assert_eq!(created.experiment.item_count, 1_001);
        assert_eq!(created.items.len(), 1_000);
        assert!(created.next_cursor.is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn results_snapshot_the_pinned_revision() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let ids = dataset_with_items(&app, &seeded, "faq", 3).await;
        let created = create_ok(&app, &seeded, json!({ "name": "run", "dataset": "faq" })).await;
        let before = created.items[0].input.clone();
        let patched = app
            .request(
                Method::PATCH,
                &format!(
                    "/api/v1/projects/{}/datasets/faq/items/{}",
                    seeded.project.id, ids[0]
                ),
            )
            .header("cookie", &seeded.cookie)
            .json(&json!({ "input": { "question": "changed" } }))
            .send()
            .await;
        assert_eq!(patched.status, 200);
        let answer = stored(
            &app,
            &seeded,
            created.experiment.id,
            json!([result(ids[0], 0, "a")]),
        )
        .await;
        assert_eq!(
            answer,
            ExperimentResultsResponse {
                stored: 1,
                skipped: 0
            }
        );
        let page = store::results_page(
            app.state.meta.reader(),
            created.experiment.id,
            None,
            false,
            10,
        )
        .await
        .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(&page[0].input).unwrap(),
            before
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn results_replace_per_repetition() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let ids = dataset_with_items(&app, &seeded, "faq", 3).await;
        let created = create_ok(&app, &seeded, json!({ "name": "run", "dataset": "faq" })).await;
        let id = created.experiment.id;
        stored(&app, &seeded, id, json!([result(ids[0], 0, "a")])).await;
        stored(&app, &seeded, id, json!([result(ids[0], 0, "b")])).await;
        let page = store::results_page(app.state.meta.reader(), id, None, false, 10)
            .await
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].output.as_deref(), Some("\"b\""));
        let experiment = app
            .request(Method::GET, &format!("{}/{id}", base(&seeded)))
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        assert_eq!(experiment.json::<Experiment>().result_count, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn results_validate() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let ids = dataset_with_items(&app, &seeded, "faq", 3).await;
        let other = dataset_with_items(&app, &seeded, "other", 1).await;
        let id = create_ok(&app, &seeded, json!({ "name": "run", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let repetition = post_results(&app, &seeded, id, json!([result(ids[0], 1, "a")])).await;
        assert_eq!(fields(&repetition), vec!["results[0].repetition"]);
        let foreign = post_results(&app, &seeded, id, json!([result(other[0], 0, "a")])).await;
        assert_eq!(fields(&foreign), vec!["results[0].dataset_item_id"]);
        let many = vec![result(ids[0], 0, "a"); 501];
        let too_many = post_results(&app, &seeded, id, json!(many)).await;
        assert_eq!(fields(&too_many), vec!["results"]);
        let duplicate = post_results(
            &app,
            &seeded,
            id,
            json!([result(ids[0], 0, "a"), result(ids[0], 0, "b")]),
        )
        .await;
        assert_eq!(fields(&duplicate), vec!["results[1].dataset_item_id"]);
        let big = "x".repeat(300 * 1024);
        let large = post_results(&app, &seeded, id, json!([result(ids[0], 0, &big)])).await;
        assert_eq!(fields(&large), vec!["results[0].output"]);
        // Any JSON value is an output, not only an object.
        let string = stored(&app, &seeded, id, json!([result(ids[0], 0, "Paris")])).await;
        assert_eq!(string.stored, 1);
        let page = store::results_page(app.state.meta.reader(), id, None, false, 10)
            .await
            .unwrap();
        assert_eq!(page[0].output.as_deref(), Some("\"Paris\""));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn outputs_are_capped_per_request() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let ids = dataset_with_items(&app, &seeded, "faq", 10).await;
        let id = create_ok(
            &app,
            &seeded,
            json!({ "name": "run", "dataset": "faq", "repetitions": 20 }),
        )
        .await
        .experiment
        .id;
        let big = "x".repeat(200 * 1024);
        // 170 results of 200 KiB are 34 MiB, over the 32 MiB cap and under the body limit.
        let results: Vec<Value> = (0..170usize)
            .map(|i| result(ids[i % 10], (i / 10) as i64, &big))
            .collect();
        let over = post_results(&app, &seeded, id, json!(results)).await;
        assert_eq!(fields(&over), vec!["results"]);
        let under = stored(&app, &seeded, id, json!(results[..160])).await;
        assert_eq!(under.stored, 160);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_on_a_missing_dataset_is_not_found() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        dataset_with_items(&app, &seeded, "faq", 1).await;
        let project = seeded.project.id;
        let missing = DatasetKey::Id(Uuid::now_v7());
        let input = CreateInput {
            fields: CreateFields {
                name: "run".to_owned(),
                description: None,
                metadata: "{}".to_owned(),
                repetitions: 1,
            },
            requested_version: None,
            id: Uuid::now_v7(),
            created_at: app.state.clock.now_ns(),
            created_by: None,
        };
        let outcome = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move { create_tx(conn, project, &missing, input).await })
            })
            .await
            .unwrap();
        assert!(matches!(outcome, Err(CreateRefusal::NotFound)));
        let response = create(
            &app,
            &seeded,
            json!({ "name": "run", "dataset": Uuid::now_v7() }),
        )
        .await;
        assert_eq!(response.status, 404);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn blocked_traces_are_skipped() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let ids = dataset_with_items(&app, &seeded, "faq", 3).await;
        let id = create_ok(&app, &seeded, json!({ "name": "run", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let blocked = TraceId::from_bytes([0x77; 16]).expect("trace id");
        app.state.deletions.guard.add_traces(
            seeded.project.id,
            &[blocked],
            Uuid::now_v7(),
            app.state.clock.now_ns(),
        );
        let answer = stored(
            &app,
            &seeded,
            id,
            json!([
                { "dataset_item_id": ids[0], "repetition": 0, "trace_id": blocked, "output": "a" },
                result(ids[1], 0, "b"),
            ]),
        )
        .await;
        assert_eq!(
            answer,
            ExperimentResultsResponse {
                stored: 1,
                skipped: 1
            }
        );
        let keys: Vec<ResultKey> = store::result_keys(app.state.meta.reader(), id)
            .await
            .unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].dataset_item_id, ids[1]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_filters_and_pages() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        dataset_with_items(&app, &seeded, "faq", 1).await;
        dataset_with_items(&app, &seeded, "other", 1).await;
        let first = create_ok(&app, &seeded, json!({ "name": "a", "dataset": "faq" })).await;
        let second = create_ok(&app, &seeded, json!({ "name": "b", "dataset": "faq" })).await;
        create_ok(&app, &seeded, json!({ "name": "c", "dataset": "other" })).await;
        let list = |query: &str| {
            app.request(Method::GET, &format!("{}{query}", base(&seeded)))
                .header("cookie", &seeded.cookie)
                .send()
        };
        let faq: Page<Experiment> = list("?dataset=faq").await.json();
        let ids: Vec<Uuid> = faq.data.iter().map(|experiment| experiment.id).collect();
        assert_eq!(ids, vec![second.experiment.id, first.experiment.id]);
        let page: Page<Experiment> = list("?limit=1").await.json();
        assert_eq!(page.data.len(), 1);
        let cursor = page.next_cursor.expect("a next page");
        let rest: Page<Experiment> = list(&format!("?limit=1&cursor={cursor}")).await.json();
        assert_eq!(rest.data.len(), 1);
        assert!(rest.next_cursor.is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hidden_experiments_are_absent() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let ids = dataset_with_items(&app, &seeded, "faq", 3).await;
        let id = create_ok(&app, &seeded, json!({ "name": "run", "dataset": "faq" }))
            .await
            .experiment
            .id;
        set_limits(
            &app,
            seeded.org_id,
            "2026-01-01T00:00:00Z",
            &json!({ "data_access_days": 30 }),
        );
        // The session cookie expires with the clock, so the reads after the advance use a key.
        let key = format!("Bearer {}", app.write_key(&seeded).await);
        app.state.clock.advance(Duration::from_secs(31 * 24 * 3600));
        let one = app
            .request(Method::GET, &format!("{}/{id}", base(&seeded)))
            .header("authorization", &key)
            .send()
            .await;
        assert_eq!(one.status, 404);
        let list: Page<Experiment> = app
            .request(Method::GET, &base(&seeded))
            .header("authorization", &key)
            .send()
            .await
            .json();
        assert!(list.data.is_empty());
        let posted = app
            .request(Method::POST, &format!("{}/{id}/items", base(&seeded)))
            .header("authorization", &key)
            .json(&json!({ "results": [result(ids[0], 0, "a")] }))
            .send()
            .await;
        assert_eq!(posted.status, 404);
        let finished = app
            .request(Method::POST, &format!("{}/{id}/finish", base(&seeded)))
            .header("authorization", &key)
            .json(&json!({}))
            .send()
            .await;
        assert_eq!(finished.status, 404);
        let deleted = app
            .request(Method::DELETE, &format!("{}/{id}", base(&seeded)))
            .header("authorization", &key)
            .send()
            .await;
        assert_eq!(deleted.status, 204);
        let row = store::get(app.state.meta.reader(), seeded.project.id, id, None)
            .await
            .unwrap();
        assert!(row.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deleting_the_dataset_keeps_experiments() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let ids = dataset_with_items(&app, &seeded, "faq", 3).await;
        let id = create_ok(&app, &seeded, json!({ "name": "run", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let deleted = app
            .request(
                Method::DELETE,
                &format!("/api/v1/projects/{}/datasets/faq", seeded.project.id),
            )
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        assert_eq!(deleted.status, 204);
        let experiment: Value = app
            .request(Method::GET, &format!("{}/{id}", base(&seeded)))
            .header("cookie", &seeded.cookie)
            .send()
            .await
            .json();
        assert!(experiment.get("dataset_id").is_none());
        assert_eq!(experiment["dataset_name"], "faq");
        let posted = post_results(&app, &seeded, id, json!([result(ids[0], 0, "a")])).await;
        assert_eq!(posted.status, 409);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn finish_is_idempotent_per_outcome() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let ids = dataset_with_items(&app, &seeded, "faq", 3).await;
        let id = create_ok(&app, &seeded, json!({ "name": "run", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let done = finish(&app, &seeded, id, json!({})).await;
        assert_eq!(done.status, 200);
        let first: Experiment = done.json();
        assert_eq!(first.status, ExperimentStatus::Completed);
        assert!(first.finished_at.is_some());
        let again = finish(&app, &seeded, id, json!({})).await;
        assert_eq!(again.status, 200);
        let second: Experiment = again.json();
        assert_eq!(second.finished_at, first.finished_at);
        assert_eq!(
            finish(&app, &seeded, id, json!({ "error": "late" }))
                .await
                .status,
            409
        );
        let late = post_results(&app, &seeded, id, json!([result(ids[0], 0, "a")])).await;
        assert_eq!(late.status, 409);

        let failed_id = create_ok(&app, &seeded, json!({ "name": "run", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let crashed = finish(&app, &seeded, failed_id, json!({ "error": "crash" })).await;
        assert_eq!(crashed.status, 200);
        let failed: Experiment = crashed.json();
        assert_eq!(failed.status, ExperimentStatus::Failed);
        assert_eq!(failed.error.as_deref(), Some("crash"));
        assert_eq!(
            finish(&app, &seeded, failed_id, json!({ "error": "crash" }))
                .await
                .status,
            200
        );
        assert_eq!(
            finish(&app, &seeded, failed_id, json!({})).await.status,
            409
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn delete_removes_the_experiment() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        dataset_with_items(&app, &seeded, "faq", 1).await;
        let id = create_ok(&app, &seeded, json!({ "name": "run", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let deleted = app
            .request(Method::DELETE, &format!("{}/{id}", base(&seeded)))
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        assert_eq!(deleted.status, 204);
        let gone = app
            .request(Method::GET, &format!("{}/{id}", base(&seeded)))
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        assert_eq!(gone.status, 404);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keys_can_read_and_write() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        dataset_with_items(&app, &seeded, "faq", 1).await;
        let read_header = format!("Bearer {}", seeded.read_key);
        let read = app
            .request(Method::GET, &base(&seeded))
            .header("authorization", &read_header)
            .send()
            .await;
        assert_eq!(read.status, 200);
        let write_by_read = app
            .request(Method::POST, &base(&seeded))
            .header("authorization", &read_header)
            .json(&json!({ "name": "run", "dataset": "faq" }))
            .send()
            .await;
        assert_eq!(write_by_read.status, 403);
        let write_key = app.write_key(&seeded).await;
        let write = app
            .request(Method::POST, &base(&seeded))
            .header("authorization", &format!("Bearer {write_key}"))
            .json(&json!({ "name": "run", "dataset": "faq" }))
            .send()
            .await;
        assert_eq!(write.status, 201);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn roles_and_tokens() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        dataset_with_items(&app, &seeded, "faq", 1).await;
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
            .json(&json!({ "name": "run", "dataset": "faq" }))
            .send()
            .await;
        assert_eq!(write.status, 403);
        let read_write = app.personal_token(seeded.user_id, true).await;
        let created = app
            .request(Method::POST, &base(&seeded))
            .header("authorization", &format!("Bearer {read_write}"))
            .json(&json!({ "name": "run", "dataset": "faq" }))
            .send()
            .await;
        assert_eq!(created.status, 201);
        let read_only = app.personal_token(seeded.user_id, false).await;
        let refused = app
            .request(Method::POST, &base(&seeded))
            .header("authorization", &format!("Bearer {read_only}"))
            .json(&json!({ "name": "run", "dataset": "faq" }))
            .send()
            .await;
        assert_eq!(refused.status, 403);
    }

    #[tokio::test]
    async fn logs_hold_no_content() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let ids = dataset_with_items(&app, &seeded, "faq", 1).await;
        let ((), logs) = capture_logs(async {
            let created = create_ok(
                &app,
                &seeded,
                json!({
                    "name": "secret-name-1",
                    "dataset": "faq",
                    "metadata": { "k": "secret-meta-1" },
                }),
            )
            .await;
            let id = created.experiment.id;
            let posted = post_results(
                &app,
                &seeded,
                id,
                json!([{
                    "dataset_item_id": ids[0],
                    "repetition": 0,
                    "output": "secret-output-1",
                    "error": "secret-error-1",
                }]),
            )
            .await;
            assert_eq!(posted.status, 200);
            let finished = finish(&app, &seeded, id, json!({ "error": "secret-finish-1" })).await;
            assert_eq!(finished.status, 200);
        })
        .await;
        for secret in [
            "secret-name-1",
            "secret-meta-1",
            "secret-output-1",
            "secret-error-1",
            "secret-finish-1",
        ] {
            assert!(!logs.contains(secret), "{logs}");
        }
        assert!(logs.contains("experiment created"), "{logs}");
    }
}
