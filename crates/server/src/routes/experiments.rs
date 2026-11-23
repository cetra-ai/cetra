//! Experiment routes (spec 09 § Datasets & experiments, D-J-10, D-J-14). Project readers list and
//! read; `ManageDatasets` writes. A hidden experiment (D-J-14) is 404 on every route but `DELETE`.

use std::collections::{HashMap, HashSet};
use std::ops::RangeInclusive;

use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::{Method, StatusCode};
use axum::routing::{delete, get, post};
use cetra_core::ids::{ProjectId, TraceId};
use serde::Deserialize;
use serde_json::{Map, Value};
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::analytics::actor::Actor;
use crate::analytics::catalog::Event;
use crate::analytics::record;
use crate::api::common::{FieldError, Page};
use cetra_storage::{
    MAX_EXPERIMENT_SCORES, MAX_EXPERIMENT_TRACES, QueryContext, RunError, experiment_scores,
    experiment_traces,
};

use crate::api::experiments::{
    CLOCK_SKEW_NS, CREATE_ITEMS_PAGE, CompareParams, CreateExperimentRequest,
    CreateExperimentResponse, Experiment, ExperimentComparison, ExperimentListParams,
    ExperimentResult, ExperimentResults, ExperimentResultsRequest, ExperimentResultsResponse,
    ExperimentStatus, ExperimentSummaries, FinishExperimentRequest, MAX_ERROR_CHARS,
    MAX_EXPERIMENT_ROWS, MAX_METADATA_BYTES, MAX_NAME_CHARS, MAX_OUTPUT_BYTES,
    MAX_OUTPUTS_PER_REQUEST, MAX_REPETITIONS, MAX_RESULTS_PER_REQUEST, RESULTS_BODY_LIMIT,
    ResultsParams, SummariesParams,
};
use crate::auth::principal::Principal;
use crate::auth::roles::ProjectPerm;
use crate::datasets::check_description;
use crate::experiments::{
    Loaded, PAGE_BYTES, compare, compare_items, page_bytes, page_len, parse_results_cursor,
    result_rows, summary, visible_from,
};
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
use super::read::{Deadline, open_query};

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
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/experiments/{id}/results",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_results),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/experiments/compare",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(compare_experiments),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/experiments/summaries",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_experiment_summaries),
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

pub(crate) fn invalid_cursor() -> ApiError {
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
pub(crate) fn experiment_json(row: &ExperimentRow) -> Result<Experiment, ApiError> {
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

/// Creates an experiment (D-J-10). The first item page is read after the create transaction, so
/// it may list fewer items than `item_count` if a cascade deleted a copied revision in between
/// (accepted).
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
        None,
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
    record(
        &state,
        &Event::ExperimentCreated,
        &Actor::project(&principal, &access),
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

/// The results write (D-J-10). Inside this transaction the experiment is read, then its status
/// is checked (a finished one is a 409, before any other check), then the repetition check and
/// the deletion guard run. A deletion's resolve write (D-J-9) is serialized
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
    if experiment.status != "running" {
        return Err(MetaError::Conflict("the experiment is finished"));
    }
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

/// The items a compare reads from storage at a time, so a page's text is never all in memory at
/// once (D-J-13).
const COMPARE_READ_CHUNK: usize = 50;
/// The regression threshold of a comparison when the query names none (D-J-13).
const DEFAULT_THRESHOLD: f64 = 0.1;
/// The largest regression threshold a comparison takes (D-J-13).
const MAX_THRESHOLD: f64 = 1_000.0;
/// The experiments of one `GET …/experiments/summaries` (D-J-20).
const MAX_SUMMARY_IDS: usize = 25;

/// The 400 of an experiment that a route needs summarized and that is too large to summarize.
fn too_large_error() -> ApiError {
    ApiError::BadRequest {
        detail: Some(format!(
            "the experiment is too large to summarize: it has more than {MAX_EXPERIMENT_SCORES} scores or {MAX_EXPERIMENT_TRACES} traces"
        )),
        errors: Vec::new(),
    }
}

/// Loads one experiment: its result keys, and its scores and traces from storage, read from the
/// experiment's `created_at` less the clock skew (D-J-13). `None` when storage refuses one of the
/// reads past its cap, which is the experiment being too large to summarize. Storage refuses a
/// read with `InvalidRequest` only past that cap, and the experiment id is a formatted UUID, so
/// no other `InvalidRequest` reaches this match.
async fn load_if_summarizable(
    state: &AppState,
    ctx: &QueryContext,
    row: ExperimentRow,
    deadline: Deadline,
) -> Result<Option<Loaded>, ApiError> {
    let keys = store::result_keys(state.meta.reader(), row.id).await?;
    let since = row.created_at.saturating_sub(CLOCK_SKEW_NS);
    let id = row.id.to_string();
    let scores = match experiment_scores(ctx, &id, since, deadline.remaining()?).await {
        Ok(scores) => scores,
        Err(RunError::InvalidRequest(_)) => return Ok(None),
        Err(error) => return Err(ApiError::from(error)),
    };
    let traces = match experiment_traces(ctx, &id, since, deadline.remaining()?).await {
        Ok(traces) => traces,
        Err(RunError::InvalidRequest(_)) => return Ok(None),
        Err(error) => return Err(ApiError::from(error)),
    };
    Loaded::new(row, keys, &scores, &traces).map(Some)
}

/// [`load_if_summarizable`] for a route that answers for the experiment: too large is a 400.
async fn load(
    state: &AppState,
    ctx: &QueryContext,
    row: ExperimentRow,
    deadline: Deadline,
) -> Result<Loaded, ApiError> {
    load_if_summarizable(state, ctx, row, deadline)
        .await?
        .ok_or_else(too_large_error)
}

async fn get_results(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(ExperimentPath { id }): error::Path<ExperimentPath>,
    error::Query(params): error::Query<ResultsParams>,
) -> Result<error::Json<ExperimentResults>, ApiError> {
    let project = access.project.id;
    let org = access.project.org_id;
    let visible = visible_from(&state, org);
    let row = store::get(state.meta.reader(), project, id, visible)
        .await?
        .ok_or(ApiError::NotFound)?;
    let after = params
        .cursor
        .as_deref()
        .map(parse_results_cursor)
        .transpose()?;
    let ctx = open_query(&state, org, project, None).await?;
    let loaded = load(&state, &ctx, row, Deadline::start()).await?;
    let experiment_summary = summary(&loaded)?;
    let limit = params.limit.get() as usize;
    let mut page = store::results_page(
        state.meta.reader(),
        id,
        after,
        params.errors_only,
        i64::from(params.limit.get()) + 1,
    )
    .await?;
    // The page holds at most `limit` rows and at most PAGE_BYTES of text (D-J-13).
    let kept = page_len(page.iter().take(limit).map(page_bytes));
    let more = kept < page.len();
    page.truncate(kept);
    let next_cursor = if more {
        page.last()
            .map(|last| format!("{}:{}", last.dataset_item_id, last.repetition))
    } else {
        None
    };
    let rows = result_rows(&loaded, &page)?;
    Ok(error::Json(ExperimentResults {
        summary: experiment_summary,
        rows,
        next_cursor,
    }))
}

/// Comma-separated experiment ids, in request order: distinct, and a count within `bounds`. Any
/// other input is a 400 on `ids`, with `message`.
fn parse_ids(
    raw: &str,
    bounds: RangeInclusive<usize>,
    message: &str,
) -> Result<Vec<Uuid>, ApiError> {
    let invalid = || ApiError::BadRequest {
        detail: None,
        errors: vec![field_error("ids", message)],
    };
    let ids = raw
        .split(',')
        .map(|part| part.trim().parse::<Uuid>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid())?;
    let distinct: HashSet<Uuid> = ids.iter().copied().collect();
    if bounds.contains(&ids.len()) && distinct.len() == ids.len() {
        Ok(ids)
    } else {
        Err(invalid())
    }
}

/// `GET …/experiments/summaries`: the summaries of one to 25 experiments, in request order (D-J-20).
/// Unknown and hidden experiments are left out; a too-large experiment is listed in `skipped`.
async fn get_experiment_summaries(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<SummariesParams>,
) -> Result<error::Json<ExperimentSummaries>, ApiError> {
    let project = access.project.id;
    let org = access.project.org_id;
    let ids = parse_ids(
        &params.ids,
        1..=MAX_SUMMARY_IDS,
        "ids must be 1 to 25 distinct experiment ids",
    )?;
    let visible = visible_from(&state, org);
    let mut found: HashMap<Uuid, ExperimentRow> =
        store::get_many(state.meta.reader(), project, &ids, visible)
            .await?
            .into_iter()
            .map(|row| (row.id, row))
            .collect();
    let ctx = open_query(&state, org, project, None).await?;
    let deadline = Deadline::start();
    let mut data = Vec::with_capacity(found.len());
    let mut skipped = Vec::new();
    for id in &ids {
        let Some(row) = found.remove(id) else {
            continue;
        };
        match load_if_summarizable(&state, &ctx, row, deadline).await? {
            Some(loaded) => data.push(summary(&loaded)?),
            None => skipped.push(*id),
        }
    }
    Ok(error::Json(ExperimentSummaries { data, skipped }))
}

async fn compare_experiments(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<CompareParams>,
) -> Result<error::Json<ExperimentComparison>, ApiError> {
    let project = access.project.id;
    let org = access.project.org_id;
    let ids = parse_ids(&params.ids, 2..=4, "ids must be 2 to 4 experiment ids")?;
    let threshold = params.threshold.unwrap_or(DEFAULT_THRESHOLD);
    if !(threshold.is_finite() && (0.0..=MAX_THRESHOLD).contains(&threshold)) {
        return Err(ApiError::BadRequest {
            detail: None,
            errors: vec![field_error("threshold", "threshold must be 0 to 1000")],
        });
    }
    let visible = visible_from(&state, org);
    let mut found: HashMap<Uuid, ExperimentRow> =
        store::get_many(state.meta.reader(), project, &ids, visible)
            .await?
            .into_iter()
            .map(|row| (row.id, row))
            .collect();
    let mut rows = Vec::with_capacity(ids.len());
    for id in &ids {
        rows.push(found.remove(id).ok_or(ApiError::NotFound)?);
    }
    // The experiments must share one dataset. Experiments of a deleted dataset have no id, so
    // they share one when they have the same name (D-J-10).
    let same_dataset = match rows.split_first() {
        Some((first, rest)) => match first.dataset_id {
            Some(dataset) => rest.iter().all(|row| row.dataset_id == Some(dataset)),
            None => rest
                .iter()
                .all(|row| row.dataset_id.is_none() && row.dataset_name == first.dataset_name),
        },
        None => false,
    };
    if !same_dataset {
        return Err(ApiError::BadRequest {
            detail: None,
            errors: vec![field_error(
                "ids",
                "the experiments must use the same dataset",
            )],
        });
    }
    let ctx = open_query(&state, org, project, None).await?;
    let deadline = Deadline::start();
    let mut loaded = Vec::with_capacity(rows.len());
    for row in rows {
        loaded.push(load(&state, &ctx, row, deadline).await?);
    }
    let limit = params.limit.get() as usize;
    let (items, mut next_cursor) = compare_items(
        &loaded,
        threshold,
        params.only,
        limit,
        params.cursor.as_deref(),
    )?;
    // The rows are read a chunk of items at a time. The page keeps items while its text is under
    // PAGE_BYTES (D-J-13), and its first item is always kept.
    let mut kept: Vec<Uuid> = Vec::new();
    let mut texts: Vec<Vec<crate::meta::experiments::ResultRow>> = vec![Vec::new(); loaded.len()];
    let mut total = 0usize;
    for chunk in items.chunks(COMPARE_READ_CHUNK) {
        let mut chunk_rows = Vec::with_capacity(loaded.len());
        for experiment in &loaded {
            chunk_rows.push(
                store::item_rows(state.meta.reader(), experiment.experiment.id, chunk).await?,
            );
        }
        let mut cut = false;
        for item in chunk {
            if !kept.is_empty() && total >= PAGE_BYTES {
                cut = true;
                break;
            }
            let bytes: usize = chunk_rows
                .iter()
                .flatten()
                .filter(|row| row.dataset_item_id == *item)
                .map(page_bytes)
                .sum();
            total = total.saturating_add(bytes);
            kept.push(*item);
            for (index, rows) in chunk_rows.iter().enumerate() {
                texts[index].extend(
                    rows.iter()
                        .filter(|row| row.dataset_item_id == *item)
                        .cloned(),
                );
            }
        }
        if cut {
            next_cursor = kept.last().map(Uuid::to_string);
            break;
        }
    }
    Ok(error::Json(compare(
        &loaded,
        &texts,
        &kept,
        threshold,
        next_cursor,
    )?))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::api::datasets::AddItemsResponse;
    use crate::api::scores::ScoreCreate;
    use crate::auth::roles::OrgRole;
    use crate::meta::experiments::ResultKey;
    use crate::scores::{Writer, write as write_scores};
    use crate::test_support::{
        Seeded, TestApp, TestResponse, capture_logs, dataset_span, set_limits,
    };
    use cetra_core::ids::SpanId;
    use cetra_core::names::metadata_key;
    use cetra_core::score::{ScoreTarget, ScoreValue};
    use std::collections::BTreeMap;

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

    async fn delete_dataset(app: &TestApp, seeded: &Seeded, name: &str) -> TestResponse {
        app.request(
            Method::DELETE,
            &format!("/api/v1/projects/{}/datasets/{name}", seeded.project.id),
        )
        .header("cookie", &seeded.cookie)
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
        assert!(
            app.state.analytics.counters.get("experiment.created") >= 1,
            "experiment.created not counted"
        );
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

    /// A trace id of sixteen copies of `byte`.
    fn trace_of(byte: u8) -> TraceId {
        match TraceId::from_slice(&[byte; 16]) {
            Ok(trace) => trace,
            Err(_) => panic!("16 bytes"),
        }
    }

    /// Commits one span of `trace` to the seeded project, stamped with `experiment` and priced at
    /// `cost`: the trace the experiment's results point at.
    async fn commit_experiment_trace(
        app: &TestApp,
        seeded: &Seeded,
        trace: TraceId,
        experiment: Uuid,
        cost: f64,
    ) {
        let mut span = dataset_span(0);
        span.trace_id = trace;
        span.span_id = match "d400000000000001".parse::<SpanId>() {
            Ok(span_id) => span_id,
            Err(_) => panic!("span id hex"),
        };
        span.parent_span_id = None;
        span.start_time = app.state.clock.now_ns();
        span.end_time = span.start_time + 1_000_000_000;
        span.ingest_time = span.start_time;
        span.cost_total = Some(cost);
        span.metadata.insert(
            metadata_key::EXPERIMENT_ID.to_owned(),
            experiment.to_string(),
        );
        app.commit_spans(&seeded.project, vec![span]).await;
    }

    /// A score of `trace` that belongs to `experiment` and `item` (D-J-12).
    fn experiment_score(
        trace: TraceId,
        experiment: Uuid,
        item: Uuid,
        name: &str,
        value: ScoreValue,
    ) -> ScoreCreate {
        ScoreCreate {
            score_id: None,
            target: ScoreTarget::Trace { trace_id: trace },
            session_id: None,
            user_id: None,
            name: name.to_owned(),
            value,
            reason: None,
            source: None,
            metadata: BTreeMap::new(),
            experiment_id: Some(experiment),
            dataset_item_id: Some(item),
            queue_id: None,
        }
    }

    /// Writes `scores` as the API writer (the score write checks the experiment ids).
    async fn write_scores_of(app: &TestApp, seeded: &Seeded, scores: Vec<ScoreCreate>) {
        let written = write_scores(
            &app.state,
            &seeded.project,
            Writer::Api {
                author: Uuid::from_u128(0xa0),
            },
            scores,
        )
        .await;
        assert!(written.is_ok(), "score write refused");
    }

    async fn results_get(
        app: &TestApp,
        seeded: &Seeded,
        experiment: Uuid,
        query: &str,
    ) -> TestResponse {
        app.request(
            Method::GET,
            &format!("{}/{experiment}/results{query}", base(seeded)),
        )
        .header("cookie", &seeded.cookie)
        .send()
        .await
    }

    async fn compare_get(app: &TestApp, seeded: &Seeded, query: &str) -> TestResponse {
        app.request(Method::GET, &format!("{}/compare{query}", base(seeded)))
            .header("cookie", &seeded.cookie)
            .send()
            .await
    }

    fn rows_len(body: &Value) -> usize {
        body["rows"].as_array().map_or(0, Vec::len)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn results_join_scores_and_traces() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let ids = dataset_with_items(&app, &seeded, "faq", 3).await;
        let experiment = create_ok(&app, &seeded, json!({ "name": "run", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let traces: Vec<TraceId> = (1..=3).map(trace_of).collect();
        for trace in &traces {
            commit_experiment_trace(&app, &seeded, *trace, experiment, 0.01).await;
        }
        let scores: Vec<ScoreCreate> = ids
            .iter()
            .zip(&traces)
            .map(|(item, trace)| {
                experiment_score(
                    *trace,
                    experiment,
                    *item,
                    "exact",
                    ScoreValue::Boolean(true),
                )
            })
            .collect();
        write_scores_of(&app, &seeded, scores).await;
        let results: Vec<Value> = ids
            .iter()
            .zip(&traces)
            .map(|(item, trace)| {
                json!({
                    "dataset_item_id": item,
                    "repetition": 0,
                    "trace_id": trace,
                    "output": "answer",
                    "latency_ns": 1_000,
                })
            })
            .collect();
        let answer = stored(&app, &seeded, experiment, json!(results)).await;
        assert_eq!(answer.stored, 3);

        let response = results_get(&app, &seeded, experiment, "").await;
        assert_eq!(
            response.status,
            200,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let body: Value = response.json();
        assert_eq!(body["summary"]["result_count"], 3);
        assert_eq!(body["summary"]["scores"][0]["name"], "exact");
        assert_eq!(rows_len(&body), 3);
        assert_eq!(body["rows"][0]["scores"]["exact"]["value_num"], 1.0);
        let cost = body["summary"]["cost_total"].as_f64().unwrap_or(0.0);
        assert!((cost - 0.03).abs() < 1e-9, "cost {cost}");
        assert!(body["next_cursor"].is_null());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn results_pages_over_http() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let ids = dataset_with_items(&app, &seeded, "faq", 5).await;
        let experiment = create_ok(&app, &seeded, json!({ "name": "run", "dataset": "faq" }))
            .await
            .experiment
            .id;
        // The second and the fourth results failed.
        let results: Vec<Value> = ids
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let mut row = json!({ "dataset_item_id": item, "repetition": 0, "output": "a" });
                if index == 1 || index == 3 {
                    row["error"] = json!("boom");
                }
                row
            })
            .collect();
        stored(&app, &seeded, experiment, json!(results)).await;

        let first: Value = results_get(&app, &seeded, experiment, "?limit=2")
            .await
            .json();
        assert_eq!(rows_len(&first), 2);
        let cursor = first["next_cursor"].as_str().unwrap_or_default().to_owned();
        assert!(!cursor.is_empty());
        let second: Value = results_get(
            &app,
            &seeded,
            experiment,
            &format!("?limit=2&cursor={cursor}"),
        )
        .await
        .json();
        assert_eq!(rows_len(&second), 2);
        let cursor = second["next_cursor"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let last: Value = results_get(
            &app,
            &seeded,
            experiment,
            &format!("?limit=2&cursor={cursor}"),
        )
        .await
        .json();
        assert_eq!(rows_len(&last), 1);
        assert!(last["next_cursor"].is_null());

        let errors: Value = results_get(&app, &seeded, experiment, "?errors_only=true")
            .await
            .json();
        assert_eq!(rows_len(&errors), 2);

        let bad = results_get(&app, &seeded, experiment, "?cursor=x").await;
        assert_eq!(fields(&bad), vec!["cursor".to_owned()]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn results_pages_stop_at_16_mib_of_text() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let ids = dataset_with_items(&app, &seeded, "faq", 10).await;
        let id = create_ok(
            &app,
            &seeded,
            json!({ "name": "run", "dataset": "faq", "repetitions": 10 }),
        )
        .await
        .experiment
        .id;
        // 100 rows of 200 KiB output: 20 MiB, in two posts under the 32 MiB cap.
        let big = "x".repeat(200 * 1024);
        let results: Vec<Value> = (0..100usize)
            .map(|i| result(ids[i % 10], (i / 10) as i64, &big))
            .collect();
        stored(&app, &seeded, id, json!(results[..50])).await;
        stored(&app, &seeded, id, json!(results[50..])).await;

        // Each row is 200 KiB plus its 17-byte input. The 82nd row takes the page past 16 MiB.
        let first: Value = results_get(&app, &seeded, id, "?limit=100").await.json();
        assert_eq!(rows_len(&first), 82);
        let cursor = first["next_cursor"].as_str().unwrap_or_default().to_owned();
        assert!(!cursor.is_empty());
        let second: Value = results_get(&app, &seeded, id, &format!("?limit=100&cursor={cursor}"))
            .await
            .json();
        assert_eq!(rows_len(&second), 18);
        assert!(second["next_cursor"].is_null());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compare_cells_hold_the_lowest_repetition_and_any_error() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let ids = dataset_with_items(&app, &seeded, "faq", 1).await;
        let first = create_ok(
            &app,
            &seeded,
            json!({ "name": "a", "dataset": "faq", "repetitions": 3 }),
        )
        .await
        .experiment
        .id;
        let second = create_ok(&app, &seeded, json!({ "name": "b", "dataset": "faq" }))
            .await
            .experiment
            .id;
        stored(
            &app,
            &seeded,
            first,
            json!([
                result(ids[0], 0, "zero"),
                result(ids[0], 1, "one"),
                { "dataset_item_id": ids[0], "repetition": 2, "output": "two", "error": "boom" }
            ]),
        )
        .await;
        stored(&app, &seeded, second, json!([result(ids[0], 0, "b")])).await;

        let body: Value = compare_get(&app, &seeded, &format!("?ids={first},{second}"))
            .await
            .json();
        let cells = body["rows"][0]["cells"]
            .as_array()
            .expect("one row with cells");
        assert_eq!(cells.len(), 2);
        // The cell is the lowest repetition's output, with the error of the repetition that failed.
        assert_eq!(cells[0]["output"], "zero");
        assert_eq!(cells[0]["error"], "boom");
        assert_eq!(cells[1]["output"], "b");
        assert!(cells[1]["error"].is_null());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn experiment_scores_are_not_older_than_their_experiment() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let ids = dataset_with_items(&app, &seeded, "faq", 1).await;
        let experiment = create_ok(&app, &seeded, json!({ "name": "run", "dataset": "faq" }))
            .await
            .experiment
            .id;
        // A client's clock stamped the score id a day before the experiment existed.
        const DAY_MS: u64 = 24 * 3600 * 1000;
        let created_ms = (app.state.clock.now_ns() / 1_000_000) as u64;
        let score_id =
            uuid::Builder::from_unix_timestamp_millis(created_ms - DAY_MS, &[0; 10]).into_uuid();
        let trace = trace_of(7);
        let mut score = experiment_score(
            trace,
            experiment,
            ids[0],
            "exact",
            ScoreValue::Boolean(true),
        );
        score.score_id = Some(score_id);
        write_scores_of(&app, &seeded, vec![score]).await;
        // The row has the score's trace, so the score joins it.
        let posted = post_results(
            &app,
            &seeded,
            experiment,
            json!([{ "dataset_item_id": ids[0], "repetition": 0, "trace_id": trace, "output": "a" }]),
        )
        .await;
        assert_eq!(posted.status, 200);

        let body: Value = results_get(&app, &seeded, experiment, "").await.json();
        assert_eq!(body["rows"][0]["scores"]["exact"]["value_num"], 1.0);
        assert_eq!(body["summary"]["scores"][0]["count"], 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn results_after_finish_are_conflicts_before_the_repetition_check() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let ids = dataset_with_items(&app, &seeded, "faq", 1).await;
        let experiment = create_ok(&app, &seeded, json!({ "name": "run", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let finished = finish(&app, &seeded, experiment, json!({})).await;
        assert_eq!(finished.status, 200);
        // Repetition 9 is out of range for a one-repetition experiment, but the finish comes first.
        let posted = post_results(&app, &seeded, experiment, json!([result(ids[0], 9, "a")])).await;
        assert_eq!(posted.status, 409);
    }

    #[test]
    fn a_storage_cap_is_a_too_large_400() {
        match too_large_error() {
            ApiError::BadRequest {
                detail: Some(message),
                ..
            } => assert!(message.contains("too large to summarize")),
            _ => panic!("a 400 with the too-large message"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn results_body_over_40_mib_is_413() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let ids = dataset_with_items(&app, &seeded, "faq", 1).await;
        let experiment = create_ok(&app, &seeded, json!({ "name": "run", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let big = "x".repeat(41 * 1024 * 1024);
        let posted =
            post_results(&app, &seeded, experiment, json!([result(ids[0], 0, &big)])).await;
        assert_eq!(posted.status, 413);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compare_of_deleted_datasets_needs_one_name() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        dataset_with_items(&app, &seeded, "faq", 1).await;
        dataset_with_items(&app, &seeded, "other", 1).await;
        let first = create_ok(&app, &seeded, json!({ "name": "a", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let second = create_ok(&app, &seeded, json!({ "name": "b", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let third = create_ok(&app, &seeded, json!({ "name": "c", "dataset": "other" }))
            .await
            .experiment
            .id;
        assert_eq!(delete_dataset(&app, &seeded, "faq").await.status, 204);

        // Both experiments of the deleted "faq" share its name, so they compare.
        let same_name = compare_get(&app, &seeded, &format!("?ids={first},{second}")).await;
        assert_eq!(
            same_name.status,
            200,
            "{}",
            String::from_utf8_lossy(&same_name.body)
        );
        // A deleted dataset and a live one do not mix.
        let mixed = compare_get(&app, &seeded, &format!("?ids={first},{third}")).await;
        assert_eq!(fields(&mixed), vec!["ids".to_owned()]);

        assert_eq!(delete_dataset(&app, &seeded, "other").await.status, 204);
        // Two deleted datasets with different names do not compare.
        let other_names = compare_get(&app, &seeded, &format!("?ids={first},{third}")).await;
        assert_eq!(fields(&other_names), vec!["ids".to_owned()]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_counts_results_per_experiment() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let ids = dataset_with_items(&app, &seeded, "faq", 2).await;
        let counted = create_ok(&app, &seeded, json!({ "name": "a", "dataset": "faq" }))
            .await
            .experiment
            .id;
        create_ok(&app, &seeded, json!({ "name": "b", "dataset": "faq" })).await;
        stored(
            &app,
            &seeded,
            counted,
            json!([result(ids[0], 0, "x"), result(ids[1], 0, "y")]),
        )
        .await;
        let page: Value = app
            .request(Method::GET, &base(&seeded))
            .header("cookie", &seeded.cookie)
            .send()
            .await
            .json();
        let data = page["data"].as_array().expect("a page");
        let row_of = |id: Uuid| {
            data.iter()
                .find(|row| row["id"] == json!(id))
                .expect("the experiment is listed")
        };
        assert_eq!(row_of(counted)["result_count"], 2);
        let other = data
            .iter()
            .find(|row| row["id"] != json!(counted))
            .expect("the other experiment");
        assert_eq!(other["result_count"], 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compare_needs_the_same_dataset() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        dataset_with_items(&app, &seeded, "faq", 1).await;
        dataset_with_items(&app, &seeded, "other", 1).await;
        let first = create_ok(&app, &seeded, json!({ "name": "a", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let second = create_ok(&app, &seeded, json!({ "name": "b", "dataset": "other" }))
            .await
            .experiment
            .id;
        let random = Uuid::now_v7();

        let other_dataset = compare_get(&app, &seeded, &format!("?ids={first},{second}")).await;
        assert_eq!(fields(&other_dataset), vec!["ids".to_owned()]);
        let single = compare_get(&app, &seeded, &format!("?ids={first}")).await;
        assert_eq!(fields(&single), vec!["ids".to_owned()]);
        let five: Vec<String> = (0..5).map(|_| Uuid::now_v7().to_string()).collect();
        let many = compare_get(&app, &seeded, &format!("?ids={}", five.join(","))).await;
        assert_eq!(fields(&many), vec!["ids".to_owned()]);
        let repeated = compare_get(&app, &seeded, &format!("?ids={first},{first}")).await;
        assert_eq!(fields(&repeated), vec!["ids".to_owned()]);
        let malformed = compare_get(&app, &seeded, &format!("?ids={first},nope")).await;
        assert_eq!(fields(&malformed), vec!["ids".to_owned()]);
        let unknown = compare_get(&app, &seeded, &format!("?ids={first},{random}")).await;
        assert_eq!(unknown.status, 404);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compare_threshold_bounds() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        dataset_with_items(&app, &seeded, "faq", 1).await;
        let first = create_ok(&app, &seeded, json!({ "name": "a", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let second = create_ok(&app, &seeded, json!({ "name": "b", "dataset": "faq" }))
            .await
            .experiment
            .id;
        for bad in ["-0.1", "1000.5", "NaN", "inf"] {
            let response = compare_get(
                &app,
                &seeded,
                &format!("?ids={first},{second}&threshold={bad}"),
            )
            .await;
            assert_eq!(fields(&response), vec!["threshold".to_owned()], "{bad}");
        }
        for good in ["0", "1000"] {
            let response = compare_get(
                &app,
                &seeded,
                &format!("?ids={first},{second}&threshold={good}"),
            )
            .await;
            assert_eq!(response.status, 200, "{good}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compare_marks_regressions() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let ids = dataset_with_items(&app, &seeded, "faq", 3).await;
        let baseline = create_ok(&app, &seeded, json!({ "name": "a", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let candidate = create_ok(&app, &seeded, json!({ "name": "b", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let baseline_traces: Vec<TraceId> = (0x10..0x13).map(trace_of).collect();
        let candidate_traces: Vec<TraceId> = (0x20..0x23).map(trace_of).collect();
        let candidate_values = [1.0, 0.0, 1.0];
        let mut scores = Vec::new();
        for (index, item) in ids.iter().enumerate() {
            scores.push(experiment_score(
                baseline_traces[index],
                baseline,
                *item,
                "exact",
                ScoreValue::Numeric(1.0),
            ));
            scores.push(experiment_score(
                candidate_traces[index],
                candidate,
                *item,
                "exact",
                ScoreValue::Numeric(candidate_values[index]),
            ));
        }
        write_scores_of(&app, &seeded, scores).await;
        for (experiment, traces) in [(baseline, &baseline_traces), (candidate, &candidate_traces)] {
            let results: Vec<Value> = ids
                .iter()
                .zip(traces.iter())
                .map(|(item, trace)| {
                    json!({ "dataset_item_id": item, "repetition": 0, "trace_id": trace, "output": "a" })
                })
                .collect();
            stored(&app, &seeded, experiment, json!(results)).await;
        }

        let response = compare_get(&app, &seeded, &format!("?ids={baseline},{candidate}")).await;
        assert_eq!(
            response.status,
            200,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let body: Value = response.json();
        let delta = &body["deltas"][0]["scores"]["exact"];
        assert_eq!(delta["n_compared"], 3);
        let value = delta["delta"].as_f64().unwrap_or(0.0);
        assert!((value - (-1.0 / 3.0)).abs() < 1e-9, "delta {value}");
        // Item 2 falls from 1 to 0; the other two items stay.
        assert_eq!(delta["regressed"], 1);
        assert_eq!(delta["improved"], 0);
        assert_eq!(body["deltas"][0]["regressed_items"], 1);
        assert_eq!(body["deltas"][0]["improved_items"], 0);
        let row = body["rows"].as_array().and_then(|rows| {
            rows.iter()
                .find(|row| row["dataset_item_id"] == json!(ids[1]))
        });
        let cell = row
            .and_then(|row| row["cells"].as_array())
            .and_then(|cells| {
                cells
                    .iter()
                    .find(|cell| cell["experiment_id"] == json!(candidate))
            });
        assert_eq!(
            cell.map(|cell| cell["regressed"].clone()),
            Some(json!(true))
        );
    }

    async fn summaries_get(app: &TestApp, seeded: &Seeded, query: &str) -> TestResponse {
        app.request(Method::GET, &format!("{}/summaries{query}", base(seeded)))
            .header("cookie", &seeded.cookie)
            .send()
            .await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn summaries_keep_request_order_and_leave_out_unknown() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        dataset_with_items(&app, &seeded, "faq", 1).await;
        let a = create_ok(&app, &seeded, json!({ "name": "a", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let b = create_ok(&app, &seeded, json!({ "name": "b", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let c = create_ok(&app, &seeded, json!({ "name": "c", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let unknown = Uuid::now_v7();

        let response = summaries_get(&app, &seeded, &format!("?ids={c},{unknown},{a},{b}")).await;
        assert_eq!(
            response.status,
            200,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let body: Value = response.json();
        let listed: Vec<Value> = body["data"]
            .as_array()
            .expect("the summaries")
            .iter()
            .map(|summary| summary["experiment"]["id"].clone())
            .collect();
        assert_eq!(listed, vec![json!(c), json!(a), json!(b)]);
        assert_eq!(body["skipped"], json!([]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn summaries_validate_the_ids() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        dataset_with_items(&app, &seeded, "faq", 1).await;
        let first = create_ok(&app, &seeded, json!({ "name": "a", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let too_many: Vec<String> = (0..26).map(|_| Uuid::now_v7().to_string()).collect();
        let bad = [
            String::new(),
            too_many.join(","),
            format!("{first},{first}"),
            format!("{first},nope"),
        ];
        for ids in &bad {
            let response = summaries_get(&app, &seeded, &format!("?ids={ids}")).await;
            assert_eq!(fields(&response), vec!["ids".to_owned()], "{ids}");
        }
        // The bound is 25 ids: 25 unknown ids answer with no data.
        let bound: Vec<String> = too_many.into_iter().take(25).collect();
        let answered = summaries_get(&app, &seeded, &format!("?ids={}", bound.join(","))).await;
        assert_eq!(answered.status, 200);
        let body: Value = answered.json();
        assert_eq!(body["data"], json!([]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn summaries_route_is_not_the_single_experiment_route() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        dataset_with_items(&app, &seeded, "faq", 1).await;
        let id = create_ok(&app, &seeded, json!({ "name": "a", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let response = summaries_get(&app, &seeded, &format!("?ids={id}")).await;
        assert_eq!(response.status, 200);
        let body: Value = response.json();
        assert_eq!(body["data"].as_array().map(Vec::len), Some(1));
        assert!(body.get("skipped").is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn summaries_leave_out_hidden_experiments() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        dataset_with_items(&app, &seeded, "faq", 1).await;
        let id = create_ok(&app, &seeded, json!({ "name": "a", "dataset": "faq" }))
            .await
            .experiment
            .id;
        set_limits(
            &app,
            seeded.org_id,
            "2026-01-01T00:00:00Z",
            &json!({ "data_access_days": 30 }),
        );
        // The session cookie expires with the clock, so the read after the advance uses a key.
        let key = format!("Bearer {}", app.write_key(&seeded).await);
        app.state.clock.advance(Duration::from_secs(31 * 24 * 3600));
        let response = app
            .request(
                Method::GET,
                &format!("{}/summaries?ids={id}", base(&seeded)),
            )
            .header("authorization", &key)
            .send()
            .await;
        assert_eq!(response.status, 200);
        let body: Value = response.json();
        assert_eq!(body["data"], json!([]));
        assert_eq!(body["skipped"], json!([]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compare_only_takes_three_values() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        dataset_with_items(&app, &seeded, "faq", 1).await;
        let first = create_ok(&app, &seeded, json!({ "name": "a", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let second = create_ok(&app, &seeded, json!({ "name": "b", "dataset": "faq" }))
            .await
            .experiment
            .id;
        for only in ["regressed", "improved", "changed"] {
            let response =
                compare_get(&app, &seeded, &format!("?ids={first},{second}&only={only}")).await;
            assert_eq!(response.status, 200, "{only}");
        }
        let unknown = compare_get(
            &app,
            &seeded,
            &format!("?ids={first},{second}&only=regressions"),
        )
        .await;
        assert_eq!(unknown.status, 400);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hidden_experiments_cannot_be_compared() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        dataset_with_items(&app, &seeded, "faq", 1).await;
        let first = create_ok(&app, &seeded, json!({ "name": "a", "dataset": "faq" }))
            .await
            .experiment
            .id;
        let second = create_ok(&app, &seeded, json!({ "name": "b", "dataset": "faq" }))
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
        let compared = app
            .request(
                Method::GET,
                &format!("{}/compare?ids={first},{second}", base(&seeded)),
            )
            .header("authorization", &key)
            .send()
            .await;
        assert_eq!(compared.status, 404);
        let results = app
            .request(Method::GET, &format!("{}/{first}/results", base(&seeded)))
            .header("authorization", &key)
            .send()
            .await;
        assert_eq!(results.status, 404);
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
