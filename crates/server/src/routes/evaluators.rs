//! Evaluator routes and built-in templates (spec 09 § Evaluation & alerts, spec 06
//! § Evaluators, D-I-19, D-I-20, D-I-24). Members and above write; every project reader
//! lists and reads.

use axum::extract::{Extension, State};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use axum::http::StatusCode;
use axum::routing::{delete, get, patch, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cetra_core::enums::ScoreDataType;
use cetra_core::ids::{OrgId, ProjectId};
use cetra_eval::{Adapter, EvaluatorConfig, JudgeOutput, OutputScore, VariableMapping};
use cetra_otel::PriceTable;
use cetra_storage::TimeWindow;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use crate::api::common::{FieldError, Page, PageParams};
use crate::api::evaluators::{
    CreateEvaluatorRequest, Evaluator, EvaluatorKind, EvaluatorScoreStats, EvaluatorStats,
    EvaluatorStatsParams, EvaluatorStatsResponse, EvaluatorTemplateList, EvaluatorTestRequest,
    EvaluatorTestResult, EvaluatorVersion, ExperimentWithScore, ExperimentsWithScore,
    OutputScoreBody, StatsWindow, UpdateEvaluatorRequest, builtin_template_list,
};
use crate::api::online_rules::RuleTarget;
use crate::audit::catalog::EvaluatorAction;
use crate::audit::{self, Event, TargetKind};
use crate::auth::principal::{self, Principal};
use crate::auth::roles::ProjectPerm;
use crate::crypto::{Aad, SecretBox};
use crate::eval::definitions::fits_definition;
use crate::eval::evaluator::{self, EvaluatorError, Prepared, WebhookSecrets};
use crate::eval::rule_check::{RuleShape, Subject, check_rule};
use crate::eval::stats::{self, ScoredExperiment};
use crate::eval::test_run;
use crate::experiments::visible_from;
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::http::restore::refuse_outbound;
use crate::meta::MetaError;
use crate::meta::connections;
use crate::meta::eval_jobs::{self, DAY_NS};
use crate::meta::evaluators::{self as store, EvaluatorFields, EvaluatorRow};
use crate::meta::experiments::{self as experiment_store, ExperimentRow};
use crate::meta::online_rules::{self as rules, RuleRow};
use crate::meta::score_definitions::list_definitions;
use crate::routes::read::{Deadline, open_query_windowed};

use super::orgs::page_named;

/// Evaluator routes. Reads need project view; writes need
/// [`ProjectPerm::ManageEvaluation`] (member and above, D-I-24). Not session-only.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/evaluators",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_evaluators),
        },
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/evaluators/stats",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_evaluator_stats),
        },
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/v1/projects/{project}/evaluators",
            access: Access::Project {
                perm: ProjectPerm::ManageEvaluation,
                session_only: false,
            },
            documented: true,
            handler: post(create_evaluator),
        },
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/v1/projects/{project}/evaluators/test",
            access: Access::Project {
                perm: ProjectPerm::ManageEvaluation,
                session_only: false,
            },
            documented: true,
            handler: post(test_evaluator),
        },
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/evaluators/{id}",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_evaluator),
        },
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/evaluators/{id}/versions",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_evaluator_versions),
        },
        RouteDef {
            method: axum::http::Method::PATCH,
            path: "/api/v1/projects/{project}/evaluators/{id}",
            access: Access::Project {
                perm: ProjectPerm::ManageEvaluation,
                session_only: false,
            },
            documented: true,
            handler: patch(update_evaluator),
        },
        RouteDef {
            method: axum::http::Method::DELETE,
            path: "/api/v1/projects/{project}/evaluators/{id}",
            access: Access::Project {
                perm: ProjectPerm::ManageEvaluation,
                session_only: false,
            },
            documented: true,
            handler: delete(delete_evaluator),
        },
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/evaluator-templates",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_evaluator_templates),
        },
    ]
}

#[derive(Debug, Deserialize)]
struct IdPath {
    id: Uuid,
}

/// What a write transaction did.
enum Outcome {
    /// The row as stored.
    Updated(Box<EvaluatorRow>),
    /// The evaluator is missing.
    Gone,
    /// A judge's connection is not in the org.
    ConnectionMissing,
    /// The change would break rules that use the evaluator (D-I-19), one error per rule.
    BreaksRules(Vec<FieldError>),
}

/// What a delete transaction did.
enum Removal {
    Removed,
    Missing,
    /// Rules use the evaluator; nothing was deleted (D-I-19).
    InUse(Vec<RuleRow>),
}

async fn list_evaluators(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<PageParams>,
) -> Result<error::Json<Page<Evaluator>>, ApiError> {
    Ok(error::Json(evaluator_page(&state, &access, params).await?))
}

/// One page of the project's evaluators, ordered by `(name, id)`. Also served to the assistant's
/// `list_evaluators` tool (AS M2).
pub(crate) async fn evaluator_page(
    state: &AppState,
    access: &ProjectAccess,
    params: PageParams,
) -> Result<Page<Evaluator>, ApiError> {
    let rows = store::list_evaluators(state.meta.reader(), access.project.id).await?;
    let items = rows
        .iter()
        .map(|row| Ok((row.name.clone(), row.id.to_string(), evaluator_json(row)?)))
        .collect::<Result<Vec<_>, ApiError>>()?;
    page_named(items, params.cursor.as_deref(), params.limit.get())
}

/// The ids of `evaluator_ids`: 1 to 50 distinct UUIDs, comma-separated. `None` when absent.
fn stats_ids(raw: Option<&str>) -> Result<Option<BTreeSet<Uuid>>, ApiError> {
    let Some(raw) = raw else { return Ok(None) };
    let invalid = || {
        bad_request(vec![field_error(
            "evaluator_ids",
            "evaluator_ids must be 1 to 50 distinct UUIDs, comma-separated",
        )])
    };
    let mut ids = BTreeSet::new();
    for part in raw.split(',') {
        let id = part.trim().parse::<Uuid>().map_err(|_| invalid())?;
        if !ids.insert(id) {
            return Err(invalid());
        }
    }
    if ids.is_empty() || ids.len() > stats::MAX_RESTRICT {
        return Err(invalid());
    }
    Ok(Some(ids))
}

/// `GET /evaluators/stats`: the 7-day figures of every evaluator, or of the named ones (D-I-48).
/// Not cached: the statements are narrowed by the ids and share one deadline.
async fn get_evaluator_stats(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<EvaluatorStatsParams>,
) -> Result<error::Json<EvaluatorStatsResponse>, ApiError> {
    Ok(error::Json(
        evaluator_stats(&state, &access, params.evaluator_ids.as_deref()).await?,
    ))
}

/// The 7-day figures of every evaluator, or of the ids named (D-I-48). Also served to the assistant's
/// evaluator tools (AS M2).
pub(crate) async fn evaluator_stats(
    state: &AppState,
    access: &ProjectAccess,
    evaluator_ids: Option<&str>,
) -> Result<EvaluatorStatsResponse, ApiError> {
    let wanted = stats_ids(evaluator_ids)?;
    let deadline = Deadline::start();
    let project = access.project.id;
    let org = access.project.org_id;
    let now = state.clock.now_ns();
    let (from, to) = stats::window(now);
    let window = StatsWindow { from, to };

    let mut rows = store::list_evaluators(state.meta.reader(), project).await?;
    if let Some(wanted) = &wanted {
        rows.retain(|row| wanted.contains(&row.id));
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    if rows.is_empty() {
        return Ok(EvaluatorStatsResponse {
            window,
            data: Vec::new(),
        });
    }
    let outputs = rows
        .iter()
        .map(|row| {
            serde_json::from_str::<OutputScore>(&row.output_score)
                .map_err(|_| corrupt("output_score"))
        })
        .collect::<Result<Vec<_>, ApiError>>()?;

    let jobs = eval_jobs::finished_stats(state.meta.reader(), project, from, to).await?;
    let latencies = eval_jobs::latency_percentiles(state.meta.reader(), project, from, to).await?;

    let restrict: Option<Vec<Uuid>> = wanted.as_ref().map(|ids| ids.iter().copied().collect());
    let has_categorical = outputs
        .iter()
        .any(|output| output.data_type == ScoreDataType::Categorical);
    let scores_ctx =
        open_query_windowed(state, org, project, Some(TimeWindow::new(from, to)?)).await?;
    let aggregates = stats::read_scores(
        &scores_ctx,
        stats::ScoreGroup::Evaluator,
        from,
        restrict.as_deref(),
        has_categorical,
        deadline,
    )
    .await?;

    // Experiments: a score named like the output score of a kept evaluator (D-I-48).
    let names: Option<Vec<String>> = wanted.as_ref().map(|_| {
        outputs
            .iter()
            .map(|output| output.name.clone())
            .collect::<BTreeSet<String>>()
            .into_iter()
            .collect()
    });
    let experiment_ctx = open_query_windowed(
        state,
        org,
        project,
        Some(TimeWindow::new(
            now - stats::EXPERIMENT_DAYS * DAY_NS,
            i64::MAX,
        )?),
    )
    .await?;
    let scored = stats::read_experiment_scores(&experiment_ctx, names.as_deref(), deadline).await?;
    let experiment_ids: Vec<Uuid> = outputs
        .iter()
        .flat_map(|output| stats::experiments_for(&scored, &output.name))
        .map(|score| score.experiment_id)
        .collect::<BTreeSet<Uuid>>()
        .into_iter()
        .collect();
    let visible = visible_from(state, org);
    let mut experiments: HashMap<Uuid, ExperimentRow> = HashMap::new();
    for chunk in experiment_ids.chunks(1000) {
        for row in experiment_store::get_many(state.meta.reader(), project, chunk, visible).await? {
            experiments.insert(row.id, row);
        }
    }

    let mut data = Vec::with_capacity(rows.len());
    for (row, output) in rows.iter().zip(&outputs) {
        let resolved: Vec<(&ScoredExperiment, &ExperimentRow)> =
            stats::experiments_for(&scored, &output.name)
                .into_iter()
                .filter_map(|score| {
                    experiments
                        .get(&score.experiment_id)
                        .map(|experiment| (score, experiment))
                })
                .collect();
        let experiments_with_score = ExperimentsWithScore {
            total: u64::try_from(resolved.len()).unwrap_or(u64::MAX),
            recent: resolved
                .iter()
                .take(stats::RECENT_EXPERIMENTS)
                .map(|(score, experiment)| ExperimentWithScore {
                    id: score.experiment_id,
                    name: experiment.name.clone(),
                    last_scored_at: score.last_scored_at,
                })
                .collect(),
        };
        let aggregate = aggregates.get(&row.id);
        let summary = stats::score_summary(
            output.data_type,
            aggregate,
            from,
            DAY_NS,
            stats::ScoreGroup::Evaluator.buckets(),
        );
        data.push(EvaluatorStats {
            evaluator_id: row.id,
            jobs: stats::job_stats(jobs.get(&row.id), latencies.get(&row.id).copied(), from),
            scores: EvaluatorScoreStats {
                data_type: output.data_type,
                count: aggregate.map_or(0, |aggregate| aggregate.count),
                average: summary.average,
                pass_rate: summary.pass_rate,
                top_category: summary.top_category,
                daily: summary.series,
            },
            experiments_with_score,
        });
    }
    Ok(EvaluatorStatsResponse { window, data })
}

async fn get_evaluator(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
) -> Result<error::Json<Evaluator>, ApiError> {
    Ok(error::Json(evaluator_detail(&state, &access, id).await?))
}

/// One evaluator with its configuration (`GET /evaluators/{id}`). Also served to the assistant's
/// `get_evaluator` tool (AS M2).
pub(crate) async fn evaluator_detail(
    state: &AppState,
    access: &ProjectAccess,
    id: Uuid,
) -> Result<Evaluator, ApiError> {
    let row = store::get_evaluator(state.meta.reader(), access.project.id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    evaluator_json(&row)
}

async fn list_evaluator_versions(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
    error::Query(params): error::Query<PageParams>,
) -> Result<error::Json<Page<EvaluatorVersion>>, ApiError> {
    let project = access.project.id;
    store::get_evaluator(state.meta.reader(), project, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let before = params
        .cursor
        .as_deref()
        .map(decode_version_cursor)
        .transpose()?;
    let limit = params.limit.get();
    let limit_usize = usize::try_from(limit).unwrap_or(usize::MAX);
    let mut rows = store::list_evaluator_versions(
        state.meta.reader(),
        project,
        id,
        before,
        i64::from(limit) + 1,
    )
    .await?;
    let more = rows.len() > limit_usize;
    rows.truncate(limit_usize);
    let next_cursor = if more {
        rows.last()
            .map(|row| encode_version_cursor(i64::from(row.version)))
            .transpose()?
    } else {
        None
    };
    let data = rows
        .iter()
        .map(version_json)
        .collect::<Result<Vec<_>, ApiError>>()?;
    Ok(error::Json(Page { data, next_cursor }))
}

async fn create_evaluator(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    ctx: audit::Ctx,
    error::Json(body): error::Json<CreateEvaluatorRequest>,
) -> Result<(StatusCode, error::Json<Evaluator>), ApiError> {
    let user = principal::session_user(&principal)?;
    let id = Uuid::now_v7();
    let project = access.project.id;
    let org = access.project.org_id;
    let kind = body.kind;
    let output = output_of(&body.output_score);
    let prepared = evaluator::prepare(
        id,
        &body.name,
        kind,
        &body.config,
        &output,
        body.webhook_secret.as_deref(),
        "",
    )
    .map_err(bad_request)?;
    check_against_store(state.meta.reader(), org, project, &prepared).await?;
    let host = prepared
        .secrets
        .as_ref()
        .and_then(|values| evaluator::url_host(&values.url));
    let secret_enc = prepared
        .secrets
        .as_ref()
        .map(|values| seal_secrets(&state.secrets, id, values))
        .transpose()?;
    let fields = EvaluatorFields {
        name: prepared.evaluator.name.clone(),
        config: prepared.config_json,
        output_score: prepared.output_json,
        connection_id: prepared.connection_id,
        secret_enc,
        version: 1,
    };
    let user_id = user.id;
    let connection_id = fields.connection_id;
    let now = state.clock.now_ns();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                // The connection is read again inside the insert's transaction.
                if let Some(connection) = connection_id
                    && connections::get_connection_tx(conn, org, connection)
                        .await?
                        .is_none()
                {
                    return Ok(Outcome::ConnectionMissing);
                }
                let row = store::insert_evaluator(
                    conn,
                    id,
                    project,
                    kind.as_str(),
                    fields,
                    Some(user_id),
                    now,
                )
                .await?;
                let action = EvaluatorAction::Created {
                    kind: kind.as_str(),
                    url_host: host,
                };
                let event = Event::new(action)
                    .org(org)
                    .project(project)
                    .target(TargetKind::Evaluator, id);
                audit::record(conn, &ctx, event, now).await?;
                Ok(Outcome::Updated(Box::new(row)))
            })
        })
        .await?;
    match outcome {
        Outcome::Updated(row) => {
            Ok((StatusCode::CREATED, error::Json(evaluator_json(&row)?)))
        }
        Outcome::ConnectionMissing => Err(bad_request(vec![connection_missing()])),
        Outcome::Gone => Err(ApiError::NotFound),
        // A new evaluator has no rules yet; the arm exists for the shared enum.
        Outcome::BreaksRules(errors) => Err(ApiError::ConflictWith {
            detail: "the change would break online rules that use this evaluator",
            errors,
        }),
    }
}

async fn update_evaluator(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
    ctx: audit::Ctx,
    error::Json(body): error::Json<UpdateEvaluatorRequest>,
) -> Result<error::Json<Evaluator>, ApiError> {
    // The request fields that were sent, by name (plan P-2): never their values.
    let mut changed_fields: Vec<&'static str> = Vec::new();
    if body.name.is_some() {
        changed_fields.push("name");
    }
    if body.config.is_some() {
        changed_fields.push("config");
    }
    if body.output_score.is_some() {
        changed_fields.push("output_score");
    }
    if body.webhook_secret.is_some() {
        changed_fields.push("webhook_secret");
    }
    let user = principal::session_user(&principal)?;
    let project = access.project.id;
    let org = access.project.org_id;
    // D-T-8: the price table is read and the write made under the price write lock, so a
    // price write cannot pass its guard between the two.
    let prices_write = state.ingest.prices().lock_writes().await;
    let expected = store::get_evaluator(state.meta.reader(), project, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let kind = EvaluatorKind::from_name(&expected.kind).ok_or_else(|| corrupt("kind"))?;
    let (current, opened) = evaluator::load(&state.secrets, &expected).map_err(load_error)?;
    // Merge the PATCH over the stored values. The sealed values are opened above, so
    // sealing (which needs the key) happens outside the transaction.
    let config_sent = body.config.is_some();
    let secret_sent = body.webhook_secret.is_some();
    // A new webhook URL must not take the saved signing secret.
    if kind == EvaluatorKind::Webhook && config_sent && !secret_sent {
        return Err(bad_request(vec![field_error(
            "webhook_secret",
            evaluator::CONFIG_NEEDS_SECRET,
        )]));
    }
    let name = body.name.unwrap_or_else(|| expected.name.clone());
    let output = body
        .output_score
        .as_ref()
        .map_or_else(|| current.output.clone(), output_of);
    let config = match body.config {
        Some(config) => config,
        None => evaluator::config_document(&current, opened.as_ref()).map_err(load_error)?,
    };
    let secret = body
        .webhook_secret
        .or_else(|| opened.as_ref().map(|values| values.signing_secret.clone()));
    let prepared = evaluator::prepare(id, &name, kind, &config, &output, secret.as_deref(), "")
        .map_err(bad_request)?;
    check_against_store(state.meta.reader(), org, project, &prepared).await?;
    let host = prepared
        .secrets
        .as_ref()
        .and_then(|values| evaluator::url_host(&values.url));
    // The sealed values stay as they are unless the PATCH sent webhook values.
    let secret_enc = match prepared.secrets.as_ref() {
        None => None,
        Some(_) if !config_sent && !secret_sent => expected.secret_enc.clone(),
        Some(values) => Some(seal_secrets(&state.secrets, id, values)?),
    };
    // The version moves only when the stored config, output or sealed values change.
    let before = opened.as_ref().map(secrets_text).transpose()?;
    let after = prepared.secrets.as_ref().map(secrets_text).transpose()?;
    let changed = prepared.config_json != expected.config
        || prepared.output_json != expected.output_score
        || before != after;
    let version = if changed {
        expected.version.saturating_add(1)
    } else {
        expected.version
    };
    // The rules of this evaluator are checked against the new config in the write (D-I-19).
    let checks = RuleChecks {
        subject: Subject::from_config(&prepared.evaluator.config),
        prices: state.ingest.prices().for_project(org, project),
        now: state.clock.now_ns(),
    };
    let fields = EvaluatorFields {
        name: prepared.evaluator.name.clone(),
        config: prepared.config_json,
        output_score: prepared.output_json,
        connection_id: prepared.connection_id,
        secret_enc,
        version,
    };
    let user_id = user.id;
    let now = checks.now;
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let out = write_update(
                    conn,
                    org,
                    project,
                    id,
                    expected,
                    fields,
                    Some(user_id),
                    &checks,
                )
                .await?;
                if matches!(out, Outcome::Updated(_)) {
                    let action = EvaluatorAction::Updated {
                        fields: changed_fields,
                        url_host: host,
                    };
                    let event = Event::new(action)
                        .org(org)
                        .project(project)
                        .target(TargetKind::Evaluator, id);
                    audit::record(conn, &ctx, event, now).await?;
                }
                Ok(out)
            })
        })
        .await;
    drop(prices_write);
    let outcome = outcome?;
    match outcome {
        Outcome::Updated(row) => Ok(error::Json(evaluator_json(&row)?)),
        Outcome::Gone => Err(ApiError::NotFound),
        Outcome::ConnectionMissing => Err(bad_request(vec![connection_missing()])),
        Outcome::BreaksRules(errors) => Err(ApiError::ConflictWith {
            detail: "the change would break online rules that use this evaluator",
            errors,
        }),
    }
}

async fn delete_evaluator(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
    ctx: audit::Ctx,
) -> Result<StatusCode, ApiError> {
    principal::session_user(&principal)?;
    let project = access.project.id;
    let org = access.project.org_id;
    let now = state.clock.now_ns();
    let removal = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                // Only this project's rules: an id from another project must answer 404 and
                // name none of that project's rules.
                let mut rules = rules::rules_of_evaluator(conn, id).await?;
                rules.retain(|rule| rule.project_id == project);
                if !rules.is_empty() {
                    return Ok(Removal::InUse(rules));
                }
                if !store::delete_evaluator(conn, project, id).await? {
                    return Ok(Removal::Missing);
                }
                let event = Event::new(EvaluatorAction::Deleted)
                    .org(org)
                    .project(project)
                    .target(TargetKind::Evaluator, id);
                audit::record(conn, &ctx, event, now).await?;
                Ok(Removal::Removed)
            })
        })
        .await?;
    match removal {
        Removal::Removed => {}
        Removal::Missing => return Err(ApiError::NotFound),
        Removal::InUse(rules) => {
            return Err(ApiError::ConflictWith {
                detail: "online rules use this evaluator",
                errors: rules
                    .iter()
                    .map(|rule| FieldError {
                        field: format!("rules.{}", rule.id),
                        message: format!("{}: uses this evaluator", rule.name),
                        position: None,
                    })
                    .collect(),
            });
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Runs one evaluator once and answers the outcome. It writes no score (D-I-21).
async fn test_evaluator(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Json(body): error::Json<EvaluatorTestRequest>,
) -> Result<error::Json<EvaluatorTestResult>, ApiError> {
    refuse_outbound(&state, "evaluator test")?;
    let user = principal::session_user(&principal)?;
    let result = test_run::run(&state, &access.project, user.id, body).await?;
    Ok(error::Json(result))
}

async fn list_evaluator_templates() -> error::Json<EvaluatorTemplateList> {
    error::Json(builtin_template_list())
}

/// What the rules of an evaluator are checked with in a PATCH's write: the new configuration,
/// the prices and the time.
struct RuleChecks {
    subject: Subject,
    prices: Arc<PriceTable>,
    now: i64,
}

/// Step 7 of the update: refuses a row that changed since `expected` was read, then
/// writes `fields`. A judge's connection is read in the same transaction, and so are the
/// rules of the evaluator: a change of the config or the connection that breaks one of them is
/// refused (D-I-19). Other changes (a rename, the webhook values) leave the rules alone.
#[allow(clippy::too_many_arguments)] // the transaction, the ids, the row read before, the new values, their author and the rule checks
async fn write_update(
    conn: &mut SqliteConnection,
    org: OrgId,
    project: ProjectId,
    id: Uuid,
    expected: EvaluatorRow,
    fields: EvaluatorFields,
    updated_by: Option<Uuid>,
    checks: &RuleChecks,
) -> Result<Outcome, MetaError> {
    let Some(current) = store::get_evaluator_tx(conn, project, id).await? else {
        return Ok(Outcome::Gone);
    };
    if current != expected {
        return Err(MetaError::Conflict("evaluator changed, retry"));
    }
    let connection = match fields.connection_id {
        Some(connection) => match connections::get_connection_tx(conn, org, connection).await? {
            Some(row) => Some(row),
            None => return Ok(Outcome::ConnectionMissing),
        },
        None => None,
    };
    let rules_affected =
        fields.config != expected.config || fields.connection_id != expected.connection_id;
    if rules_affected {
        let broken = broken_rules(
            conn,
            id,
            &checks.subject,
            connection.as_ref(),
            &checks.prices,
            checks.now,
        )
        .await?;
        if !broken.is_empty() {
            return Ok(Outcome::BreaksRules(broken));
        }
    }
    Ok(
        match store::update_evaluator(conn, project, id, fields, updated_by, checks.now).await? {
            Some(row) => Outcome::Updated(Box::new(row)),
            None => Outcome::Gone,
        },
    )
}

/// One error per rule of evaluator `id` that the new `subject` would break: the rule's first
/// check error, named after the rule (D-I-19).
async fn broken_rules(
    conn: &mut SqliteConnection,
    id: Uuid,
    subject: &Subject,
    connection: Option<&connections::ConnectionRow>,
    prices: &PriceTable,
    now: i64,
) -> Result<Vec<FieldError>, MetaError> {
    let adapter = match connection {
        Some(row) => Some(
            Adapter::from_name(&row.adapter).map_err(|_| MetaError::Corrupt {
                table: "llm_connections",
                column: "adapter",
            })?,
        ),
        None => None,
    };
    let mut broken = Vec::new();
    for rule in rules::rules_of_evaluator(conn, id).await? {
        let value: Value =
            serde_json::from_str(&rule.variable_mapping).map_err(|_| MetaError::Corrupt {
                table: "online_rules",
                column: "variable_mapping",
            })?;
        let mapping = VariableMapping::from_json(&value).map_err(|_| MetaError::Corrupt {
            table: "online_rules",
            column: "variable_mapping",
        })?;
        let variables: BTreeSet<String> = value
            .as_object()
            .map(|map| map.keys().cloned().collect())
            .unwrap_or_default();
        let target = RuleTarget::from_name(&rule.target).ok_or(MetaError::Corrupt {
            table: "online_rules",
            column: "target",
        })?;
        let shape = RuleShape {
            target,
            mapping: Some(&mapping),
            variables: &variables,
            budgeted: rule.budget_usd.is_some(),
        };
        if let Some(first) = check_rule(&shape, subject, adapter, prices, now).first() {
            broken.push(FieldError {
                field: format!("rules.{}", rule.id),
                message: format!("{}: {}", rule.name, first.message),
                position: None,
            });
        }
    }
    Ok(broken)
}

/// The checks that need `meta.db`: a judge's connection is in the org, and the output fits
/// the live score definition of its name (spec 06 § Evaluator contract).
pub(crate) async fn check_against_store(
    reader: &SqlitePool,
    org: OrgId,
    project: ProjectId,
    prepared: &Prepared,
) -> Result<(), ApiError> {
    let mut errors = Vec::new();
    if let Some(connection) = prepared.connection_id
        && connections::get_connection(reader, org, connection)
            .await?
            .is_none()
    {
        errors.push(connection_missing());
    }
    let definitions = list_definitions(reader, project, false).await?;
    let judge: Option<&JudgeOutput> = match &prepared.evaluator.config {
        EvaluatorConfig::LlmJudge(judge) => Some(&judge.output),
        EvaluatorConfig::Check(_) | EvaluatorConfig::Webhook(_) => None,
    };
    if let Some(definition) = definitions
        .iter()
        .find(|definition| definition.name == prepared.evaluator.output.name)
        && !fits_definition(&prepared.evaluator.output, judge, definition)
    {
        errors.push(field_error(
            "output_score",
            "output_score does not match the score definition",
        ));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(bad_request(errors))
    }
}

/// The evaluator as the API shows it: the stored JSON, without the sealed values.
fn evaluator_json(row: &EvaluatorRow) -> Result<Evaluator, ApiError> {
    Ok(Evaluator {
        id: row.id,
        name: row.name.clone(),
        kind: EvaluatorKind::from_name(&row.kind).ok_or_else(|| corrupt("kind"))?,
        config: parse_config(&row.config)?,
        output_score: parse_output(&row.output_score)?,
        has_signing_secret: row.secret_enc.is_some(),
        version: i64::from(row.version),
        created_by: row.created_by,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

fn parse_config(text: &str) -> Result<Value, ApiError> {
    serde_json::from_str(text).map_err(|_| corrupt("config"))
}

fn parse_output(text: &str) -> Result<OutputScoreBody, ApiError> {
    serde_json::from_str::<OutputScore>(text)
        .map(|output| body_of(&output))
        .map_err(|_| corrupt("output_score"))
}

fn version_json(row: &store::EvaluatorVersionRow) -> Result<EvaluatorVersion, ApiError> {
    Ok(EvaluatorVersion {
        version: i64::from(row.version),
        name: row.name.clone(),
        kind: EvaluatorKind::from_name(&row.kind).ok_or_else(|| corrupt("kind"))?,
        config: parse_config(&row.config)?,
        output_score: parse_output(&row.output_score)?,
        created_by: row.created_by,
        created_at: row.created_at,
    })
}

pub(crate) fn output_of(body: &OutputScoreBody) -> OutputScore {
    OutputScore {
        name: body.name.clone(),
        data_type: body.data_type,
        categories: body.categories.clone(),
    }
}

fn body_of(output: &OutputScore) -> OutputScoreBody {
    OutputScoreBody {
        name: output.name.clone(),
        data_type: output.data_type,
        categories: output.categories.clone(),
    }
}

/// Seals the webhook values for the row `id`.
fn seal_secrets(
    secrets: &SecretBox,
    id: Uuid,
    values: &WebhookSecrets,
) -> Result<Vec<u8>, ApiError> {
    let bytes = serde_json::to_vec(values).map_err(internal)?;
    secrets
        .seal(
            &bytes,
            Aad {
                table: "evaluators",
                column: "secret_enc",
                row_id: &id,
            },
        )
        .map_err(internal)
}

/// The webhook values as the JSON text compared for the version rule.
fn secrets_text(values: &WebhookSecrets) -> Result<String, ApiError> {
    serde_json::to_string(values).map_err(internal)
}

fn connection_missing() -> FieldError {
    field_error(
        "config.connection_id",
        "connection not found in this organization",
    )
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

fn corrupt(column: &'static str) -> ApiError {
    ApiError::Internal(Box::new(MetaError::Corrupt {
        table: "evaluators",
        column,
    }))
}

fn load_error(error: EvaluatorError) -> ApiError {
    ApiError::Internal(Box::new(error))
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> ApiError {
    ApiError::Internal(Box::new(error))
}

/// The position of a version page: the version the next page starts below.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionCursor {
    version: i64,
}

fn encode_version_cursor(version: i64) -> Result<String, ApiError> {
    let bytes = serde_json::to_vec(&VersionCursor { version })
        .map_err(|error| ApiError::Internal(Box::new(error)))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Decodes a version cursor. Any malformed one is a `400` on field `cursor`.
fn decode_version_cursor(text: &str) -> Result<i64, ApiError> {
    let invalid = || bad_request(vec![field_error("cursor", "invalid cursor")]);
    let bytes = URL_SAFE_NO_PAD.decode(text).map_err(|_| invalid())?;
    let cursor: VersionCursor = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    Ok(cursor.version)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use axum::http::{Method, header};
    use serde_json::{Value, json};

    use super::*;
    use crate::auth::roles::OrgRole;
    use crate::meta::connections::{ConnectionFields, insert_connection};
    use crate::meta::eval_jobs::{self, KIND_MANUAL, NewJob};
    use crate::meta::orgs::insert_org;
    use crate::test_support::mock_llm::{MockLlm, Reply, completion};
    use crate::test_support::mock_webhook::MockWebhook;
    use crate::test_support::{
        Seeded, TestApp, TestResponse, TestScore, capture_logs, dataset_span, put_score,
    };
    use cetra_core::enums::PayloadFormat;
    use cetra_core::ids::{SpanId, TraceId};
    use cetra_core::score::ScoreValue;
    use cetra_core::span::Span;

    /// The fixture judge's connection (`fixtures/api/README.md`).
    const FIXTURE_CONNECTION: &str = "01900000-0000-7000-8000-000000000400";
    const WEBHOOK_SECRET: &str = "signing-secret-0123456789";
    const WEBHOOK_URL: &str = "https://hooks.example.com/private/path?token=query-secret";
    const WEBHOOK_HEADER_VALUE: &str = "header-secret-value";
    const MOVED_SECRET: &str = "moved-signing-secret-456";
    const MOVED_URL: &str = "https://hooks.example.net/second/route?key=moved-query-secret";
    const MOVED_HEADER: &str = "moved-header-value";

    fn path(project: ProjectId) -> String {
        format!("/api/v1/projects/{project}/evaluators")
    }

    /// The fixture create body (`fixtures/api/requests/evaluator-create.json`).
    fn judge_body() -> Value {
        serde_json::from_str(include_str!(
            "../../../../fixtures/api/requests/evaluator-create.json"
        ))
        .unwrap()
    }

    fn check_body(name: &str) -> Value {
        json!({
            "name": name,
            "kind": "check",
            "config": {"check": "contains", "value": "refund"},
            "output_score": {"name": "mentions_refund", "data_type": "boolean"}
        })
    }

    fn webhook_body(name: &str) -> Value {
        json!({
            "name": name,
            "kind": "webhook",
            "config": {
                "url": WEBHOOK_URL,
                "headers": {"X-Team": WEBHOOK_HEADER_VALUE}
            },
            "output_score": {"name": "hook", "data_type": "numeric"},
            "webhook_secret": WEBHOOK_SECRET
        })
    }

    /// Inserts a connection with `id` in `org`, as M1's route would, and returns its id.
    async fn add_connection(app: &TestApp, org: OrgId, id: Uuid) -> Uuid {
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_connection(
                        conn,
                        id,
                        org,
                        ConnectionFields {
                            name: format!("Judge {id}"),
                            adapter: "openai".to_owned(),
                            base_url: None,
                            secret_enc: vec![0x01],
                            api_key_hint: "0123".to_owned(),
                            extra_headers_enc: None,
                            extra_header_names: Vec::new(),
                            default_model: None,
                            structured_output: "native".to_owned(),
                            max_concurrency: 4,
                            requests_per_minute: 300,
                        },
                        None,
                        now,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        id
    }

    /// The fixture connection, in `seeded`'s org.
    async fn fixture_connection(app: &TestApp, seeded: &Seeded) {
        add_connection(
            app,
            seeded.org_id,
            Uuid::from_str(FIXTURE_CONNECTION).unwrap(),
        )
        .await;
    }

    async fn create(app: &TestApp, seeded: &Seeded, body: &Value) -> TestResponse {
        app.request(Method::POST, &path(seeded.project.id))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(body)
            .send()
            .await
    }

    async fn create_ok(app: &TestApp, seeded: &Seeded, body: &Value) -> Value {
        let response = create(app, seeded, body).await;
        assert_eq!(response.status, 201, "{}", text(&response));
        response.json()
    }

    async fn patch(app: &TestApp, seeded: &Seeded, id: &str, body: &Value) -> TestResponse {
        app.request(Method::PATCH, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(body)
            .send()
            .await
    }

    async fn patch_ok(app: &TestApp, seeded: &Seeded, id: &str, body: &Value) -> Value {
        let response = patch(app, seeded, id, body).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        response.json()
    }

    fn text(response: &TestResponse) -> String {
        String::from_utf8_lossy(&response.body).into_owned()
    }

    /// The `(field, message)` pairs of a 400 answer.
    fn field_errors(response: &TestResponse) -> Vec<(String, String)> {
        assert_eq!(response.status, 400, "{}", text(response));
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

    /// The stored `secret_enc` of `id`.
    async fn stored_secret(app: &TestApp, id: Uuid) -> Vec<u8> {
        sqlx::query_scalar::<_, Option<Vec<u8>>>("SELECT secret_enc FROM evaluators WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap()
            .expect("a sealed webhook value")
    }

    /// The evaluator without the fields that differ between runs.
    fn without_identity(value: &Value) -> Value {
        let mut value = value.clone();
        let object = value.as_object_mut().unwrap();
        for key in ["id", "created_by", "created_at", "updated_at"] {
            object.remove(key);
        }
        value
    }

    fn id_of(value: &Value) -> String {
        value["id"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn judge_round_trip() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &judge_body()).await;
        let expected: Value = serde_json::from_str(include_str!(
            "../../../../fixtures/api/static/evaluator.json"
        ))
        .unwrap();
        // The fixture's author is a fixed id; the seeded owner is not, so it is not compared.
        assert_eq!(without_identity(&created), without_identity(&expected));
        assert!(created["created_by"].is_string());
        let id = id_of(&created);
        let read = app
            .request(Method::GET, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(read.status, 200);
        assert_eq!(read.json::<Value>(), created);
        let page = app
            .request(Method::GET, &path(seeded.project.id))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await
            .json::<Value>();
        assert_eq!(page["data"], json!([created]));
    }

    #[tokio::test]
    async fn check_round_trip() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, &check_body("Mentions refund")).await;
        assert_eq!(created["kind"], "check");
        assert_eq!(created["config"]["check"], "contains");
        // The folded needle is stored in `values`; `value` stays as null.
        assert_eq!(created["config"]["values"], json!(["refund"]));
        assert!(created["config"]["value"].is_null());
        assert_eq!(
            created["output_score"],
            json!({"name": "mentions_refund", "data_type": "boolean"})
        );
        assert_eq!(created["has_signing_secret"], false);
    }

    #[tokio::test]
    async fn webhook_values_are_sealed() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, &webhook_body("Hooked")).await;
        assert_eq!(
            created["config"],
            json!({
                "url_host": "hooks.example.com",
                "timeout_ms": 10_000,
                "header_names": ["x-team"]
            })
        );
        assert_eq!(created["has_signing_secret"], true);
        let response = created.to_string();
        for sealed in [
            WEBHOOK_SECRET,
            "/private/path",
            "query-secret",
            WEBHOOK_HEADER_VALUE,
        ] {
            assert!(!response.contains(sealed), "response shows {sealed}");
        }
        // The stored values open with their own row id, and with no other.
        let id = Uuid::from_str(&id_of(&created)).unwrap();
        let sealed = stored_secret(&app, id).await;
        let aad = Aad {
            table: "evaluators",
            column: "secret_enc",
            row_id: &id,
        };
        let opened: Value =
            serde_json::from_slice(&app.state.secrets.open(&sealed, aad).unwrap()).unwrap();
        assert_eq!(opened["signing_secret"], WEBHOOK_SECRET);
        assert_eq!(opened["url"], WEBHOOK_URL);
        assert_eq!(opened["headers"]["X-Team"], WEBHOOK_HEADER_VALUE);
        let other = Uuid::now_v7();
        let wrong = Aad {
            table: "evaluators",
            column: "secret_enc",
            row_id: &other,
        };
        assert!(app.state.secrets.open(&sealed, wrong).is_err());
    }

    #[tokio::test]
    async fn webhook_needs_a_secret() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let mut body = webhook_body("Hooked");
        body.as_object_mut().unwrap().remove("webhook_secret");
        assert_eq!(
            field_errors(&create(&app, &seeded, &body).await),
            [(
                "webhook_secret".to_owned(),
                "webhook_secret is required".to_owned()
            )]
        );
        body["webhook_secret"] = json!("short");
        assert_eq!(
            field_errors(&create(&app, &seeded, &body).await),
            [(
                "webhook_secret".to_owned(),
                "webhook_secret must be 16 to 256 characters".to_owned()
            )]
        );
    }

    #[tokio::test]
    async fn webhook_secret_only_for_webhooks() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        let mut body = judge_body();
        body["webhook_secret"] = json!(WEBHOOK_SECRET);
        assert_eq!(
            field_errors(&create(&app, &seeded, &body).await),
            [(
                "webhook_secret".to_owned(),
                "webhook_secret is only for webhook evaluators".to_owned()
            )]
        );
    }

    #[tokio::test]
    async fn validation_maps_config_errors() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;

        let mut unknown_check = check_body("Bad");
        unknown_check["config"] = json!({"check": "nope"});
        let errors = field_errors(&create(&app, &seeded, &unknown_check).await);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].0, "config");
        assert!(errors[0].1.starts_with("invalid configuration: "));

        let mut no_prompt = judge_body();
        no_prompt["config"]["prompt"] = json!("");
        assert_eq!(
            field_errors(&create(&app, &seeded, &no_prompt).await),
            [("config.prompt".to_owned(), "must be non-empty".to_owned())]
        );

        let mut mismatch = judge_body();
        mismatch["output_score"]["data_type"] = json!("boolean");
        assert_eq!(
            field_errors(&create(&app, &seeded, &mismatch).await),
            [(
                "output_score.data_type".to_owned(),
                "data_type must be numeric for this evaluator".to_owned()
            )]
        );

        let mut no_name = judge_body();
        no_name["name"] = json!("   ");
        assert_eq!(
            field_errors(&create(&app, &seeded, &no_name).await),
            [("name".to_owned(), "must be 1 to 128 characters".to_owned())]
        );
    }

    #[tokio::test]
    async fn connection_must_be_in_the_org() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let other = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(
                    async move { insert_org(conn, OrgId::new_v7(), "Other", "other", 1).await },
                )
            })
            .await
            .unwrap();
        let foreign = add_connection(&app, other.id, Uuid::now_v7()).await;
        let mut body = judge_body();
        body["config"]["connection_id"] = json!(foreign);
        assert_eq!(
            field_errors(&create(&app, &seeded, &body).await),
            [(
                "config.connection_id".to_owned(),
                "connection not found in this organization".to_owned()
            )]
        );
        // A connection that does not exist at all is refused the same way.
        body["config"]["connection_id"] = json!(Uuid::now_v7());
        assert_eq!(field_errors(&create(&app, &seeded, &body).await).len(), 1);
    }

    #[tokio::test]
    async fn output_must_fit_the_definition() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        let definition = app
            .request(
                Method::POST,
                &format!("/api/v1/projects/{}/score-definitions", seeded.project.id),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&json!({
                "name": "relevance",
                "data_type": "numeric",
                "min_value": 0.0,
                "max_value": 1.0
            }))
            .send()
            .await;
        assert_eq!(definition.status, 201);

        // The judge's range 0 to 2 is wider than the definition's 0 to 1.
        let mut wide = judge_body();
        wide["config"]["output"]["max"] = json!(2.0);
        assert_eq!(
            field_errors(&create(&app, &seeded, &wide).await),
            [(
                "output_score".to_owned(),
                "output_score does not match the score definition".to_owned()
            )]
        );
        create_ok(&app, &seeded, &judge_body()).await;
    }

    #[tokio::test]
    async fn version_bumps_only_on_content() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &judge_body()).await;
        let id = id_of(&created);
        assert_eq!(created["version"], 1);

        let renamed = patch_ok(&app, &seeded, &id, &json!({"name": "Relevance v2"})).await;
        assert_eq!(renamed["version"], 1);
        assert_eq!(renamed["name"], "Relevance v2");

        let mut config = judge_body()["config"].clone();
        config["prompt"] = json!("Rate the answer. {{ input }} {{ output }}");
        let reworded = patch_ok(&app, &seeded, &id, &json!({"config": config})).await;
        assert_eq!(reworded["version"], 2);

        let webhook = create_ok(&app, &seeded, &webhook_body("Hooked")).await;
        let hook_id = id_of(&webhook);
        let resealed = patch_ok(
            &app,
            &seeded,
            &hook_id,
            &json!({"webhook_secret": "another-signing-secret-99"}),
        )
        .await;
        assert_eq!(resealed["version"], 2);
        // The same secret again changes nothing.
        let same = patch_ok(
            &app,
            &seeded,
            &hook_id,
            &json!({"webhook_secret": "another-signing-secret-99"}),
        )
        .await;
        assert_eq!(same["version"], 2);
    }

    #[tokio::test]
    async fn rename_of_a_check_keeps_the_version() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, &check_body("Refund")).await;
        let id = id_of(&created);
        assert_eq!(created["version"], 1);
        let renamed = patch_ok(&app, &seeded, &id, &json!({"name": "Refund mention"})).await;
        assert_eq!(renamed["name"], "Refund mention");
        assert_eq!(renamed["version"], 1);
    }

    #[tokio::test]
    async fn kind_cannot_change() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &judge_body()).await;
        let response = patch(&app, &seeded, &id_of(&created), &json!({"kind": "check"})).await;
        assert_eq!(response.status, 400, "{}", text(&response));
    }

    #[tokio::test]
    async fn patch_webhook_without_config_keeps_the_sealed_values() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, &webhook_body("Hooked")).await;
        let id = id_of(&created);
        let uuid = Uuid::from_str(&id).unwrap();
        let before = stored_secret(&app, uuid).await;

        patch_ok(&app, &seeded, &id, &json!({"name": "Renamed"})).await;
        assert_eq!(stored_secret(&app, uuid).await, before);

        let moved = patch_ok(
            &app,
            &seeded,
            &id,
            &json!({
                "config": {"url": "https://hooks.example.net/new", "headers": {"X-Team": "new-value"}},
                "webhook_secret": WEBHOOK_SECRET
            }),
        )
        .await;
        assert_eq!(moved["version"], 2);
        assert_eq!(moved["config"]["url_host"], "hooks.example.net");
        assert_eq!(moved["config"]["header_names"], json!(["x-team"]));
        let aad = Aad {
            table: "evaluators",
            column: "secret_enc",
            row_id: &uuid,
        };
        let opened: Value = serde_json::from_slice(
            &app.state
                .secrets
                .open(&stored_secret(&app, uuid).await, aad)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(opened["signing_secret"], WEBHOOK_SECRET);
        assert_eq!(opened["url"], "https://hooks.example.net/new");
        assert_eq!(opened["headers"]["X-Team"], "new-value");
    }

    #[tokio::test]
    async fn patch_config_without_secret_is_400() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, &webhook_body("Hooked")).await;
        let id = id_of(&created);
        let uuid = Uuid::from_str(&id).unwrap();
        let before = stored_secret(&app, uuid).await;

        let response = patch(
            &app,
            &seeded,
            &id,
            &json!({"config": {"url": "https://hooks.example.net/new"}}),
        )
        .await;
        assert_eq!(
            field_errors(&response),
            vec![(
                "webhook_secret".to_owned(),
                "webhook_secret is required with config".to_owned()
            )]
        );
        assert_eq!(stored_secret(&app, uuid).await, before);
    }

    #[tokio::test]
    async fn duplicate_name_is_409() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        create_ok(&app, &seeded, &judge_body()).await;
        let response = create(&app, &seeded, &judge_body()).await;
        assert_eq!(response.status, 409, "{}", text(&response));
        assert!(text(&response).contains("an evaluator with this name already exists"));
    }

    #[tokio::test]
    async fn member_writes_viewer_reads() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        let member = app
            .add_member(&seeded, "member@example.com", OrgRole::Member)
            .await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let response = app
            .request(Method::POST, &path(seeded.project.id))
            .header(header::COOKIE.as_str(), &member.cookie)
            .json(&judge_body())
            .send()
            .await;
        assert_eq!(response.status, 201, "{}", text(&response));
        let listed = app
            .request(Method::GET, &path(seeded.project.id))
            .header(header::COOKIE.as_str(), &viewer.cookie)
            .send()
            .await;
        assert_eq!(listed.status, 200);
        let refused = app
            .request(Method::POST, &path(seeded.project.id))
            .header(header::COOKIE.as_str(), &viewer.cookie)
            .json(&check_body("Refused"))
            .send()
            .await;
        assert_eq!(refused.status, 403);
    }

    #[tokio::test]
    async fn api_key_cannot_write() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let listed = app
            .request(Method::GET, &path(seeded.project.id))
            .header(
                header::AUTHORIZATION.as_str(),
                &format!("Bearer {}", seeded.read_key),
            )
            .send()
            .await;
        assert_eq!(listed.status, 200);
        let refused = app
            .request(Method::POST, &path(seeded.project.id))
            .header(
                header::AUTHORIZATION.as_str(),
                &format!("Bearer {}", seeded.read_key),
            )
            .json(&check_body("Refused"))
            .send()
            .await;
        assert_eq!(refused.status, 403);
    }

    #[tokio::test]
    async fn templates_match_the_fixture() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let response = app
            .request(
                Method::GET,
                &format!("/api/v1/projects/{}/evaluator-templates", seeded.project.id),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, 200);
        let expected: Value = serde_json::from_str(include_str!(
            "../../../../fixtures/api/static/evaluator-templates.json"
        ))
        .unwrap();
        assert_eq!(response.json::<Value>(), expected);
    }

    /// Review 2026-11-15 #3: another org's connection id answered 409 while used, not 404.
    #[tokio::test]
    async fn connection_delete_from_another_org_is_404_while_used() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        create_ok(&app, &seeded, &judge_body()).await;
        let other = app
            .request(Method::POST, "/api/v1/orgs")
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&json!({ "name": "Other" }))
            .send()
            .await;
        assert_eq!(other.status, 201, "{}", text(&other));
        let slug = other.json::<Value>()["slug"].as_str().unwrap().to_owned();
        let response = app
            .request(
                Method::DELETE,
                &format!("/api/v1/orgs/{slug}/llm-connections/{FIXTURE_CONNECTION}"),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, 404, "{}", text(&response));
    }

    #[tokio::test]
    async fn connection_delete_is_409_while_used() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &judge_body()).await;
        let connection = format!("/api/v1/orgs/acme/llm-connections/{FIXTURE_CONNECTION}");
        let blocked = app
            .request(Method::DELETE, &connection)
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(blocked.status, 409, "{}", text(&blocked));
        assert!(text(&blocked).contains("connection is used by evaluators"));

        let removed = app
            .request(
                Method::DELETE,
                &format!("{}/{}", path(seeded.project.id), id_of(&created)),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(removed.status, 204);
        let freed = app
            .request(Method::DELETE, &connection)
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(freed.status, 204);
    }

    #[tokio::test]
    async fn another_projects_evaluator_is_404() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (other, _) = app.add_project(&seeded, "other", None).await;
        let created = create_ok(&app, &seeded, &check_body("Mine")).await;
        let id = id_of(&created);
        let read = app
            .request(Method::GET, &format!("{}/{id}", path(other.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(read.status, 404);
        let edited = app
            .request(Method::PATCH, &format!("{}/{id}", path(other.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&json!({"name": "x"}))
            .send()
            .await;
        assert_eq!(edited.status, 404);
        let removed = app
            .request(Method::DELETE, &format!("{}/{id}", path(other.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(removed.status, 404);
    }

    #[tokio::test]
    async fn patch_validation_maps_fields() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &judge_body()).await;
        let id = id_of(&created);

        let mut config = judge_body()["config"].clone();
        config["prompt"] = json!("");
        assert_eq!(
            field_errors(&patch(&app, &seeded, &id, &json!({"config": config})).await),
            [("config.prompt".to_owned(), "must be non-empty".to_owned())]
        );
        assert_eq!(
            field_errors(
                &patch(
                    &app,
                    &seeded,
                    &id,
                    &json!({"output_score": {"name": "", "data_type": "numeric"}})
                )
                .await
            ),
            [(
                "output_score.name".to_owned(),
                "output_score.name must be 1 to 255 bytes".to_owned()
            )]
        );
    }

    #[tokio::test]
    async fn a_stale_patch_is_409() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &judge_body()).await;
        let id = Uuid::from_str(&id_of(&created)).unwrap();
        let project = seeded.project.id;
        let org = seeded.org_id;
        let now = app.state.clock.now_ns();

        let expected = store::get_evaluator(app.state.meta.reader(), project, id)
            .await
            .unwrap()
            .unwrap();
        let versions_before = version_count(&app, id).await;
        // Another write lands between the PATCH's read and its write.
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("UPDATE evaluators SET updated_at = updated_at + 1 WHERE id = ?")
                        .bind(id.to_string())
                        .execute(&mut *conn)
                        .await?;
                    Ok::<(), MetaError>(())
                })
            })
            .await
            .unwrap();
        let fields = EvaluatorFields {
            name: "Stale".to_owned(),
            config: expected.config.clone(),
            output_score: expected.output_score.clone(),
            connection_id: expected.connection_id,
            secret_enc: None,
            version: 2,
        };
        let checks = RuleChecks {
            subject: Subject::from_row(&expected).unwrap(),
            prices: app.state.ingest.prices().builtin(),
            now,
        };
        let stale = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    write_update(conn, org, project, id, expected, fields, None, &checks).await
                })
            })
            .await;
        assert!(matches!(
            stale,
            Err(MetaError::Conflict("evaluator changed, retry"))
        ));
        assert_eq!(version_count(&app, id).await, versions_before);

        // A judge whose connection row is gone is reported, not written.
        let current = store::get_evaluator(app.state.meta.reader(), project, id)
            .await
            .unwrap()
            .unwrap();
        let fields = EvaluatorFields {
            name: current.name.clone(),
            config: current.config.clone(),
            output_score: current.output_score.clone(),
            connection_id: Some(Uuid::now_v7()),
            secret_enc: None,
            version: current.version,
        };
        let checks = RuleChecks {
            subject: Subject::from_row(&current).unwrap(),
            prices: app.state.ingest.prices().builtin(),
            now,
        };
        let outcome = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    write_update(conn, org, project, id, current, fields, None, &checks).await
                })
            })
            .await
            .unwrap();
        assert!(matches!(outcome, Outcome::ConnectionMissing));
    }

    /// A trace rule on `evaluator` that maps `input` and `output`, written straight to the store.
    async fn rule_on(
        app: &TestApp,
        seeded: &Seeded,
        evaluator: Uuid,
        name: &str,
        budget: Option<f64>,
    ) -> Uuid {
        let (project, now) = (seeded.project.id, app.state.clock.now_ns());
        let name = name.to_owned();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let id = Uuid::now_v7();
                    rules::insert_rule(
                        conn,
                        rules::NewRule {
                            id,
                            project_id: project,
                            evaluator_id: evaluator,
                            target: "trace".to_owned(),
                            created_by: None,
                        },
                        rules::RuleFields {
                            name,
                            filter: None,
                            variable_mapping: r#"{"input":"trace.input","output":"trace.output"}"#
                                .to_owned(),
                            sample_rate: 1.0,
                            budget_usd: budget,
                            budget_period: budget.map(|_| "day".to_owned()),
                            enabled: true,
                        },
                        now,
                    )
                    .await?;
                    Ok(id)
                })
            })
            .await
            .unwrap()
    }

    /// The judge body with `prompt` replaced, as a PATCH `config`.
    fn judge_config_with_prompt(prompt: &str) -> Value {
        let mut config = judge_body()["config"].clone();
        config["prompt"] = json!(prompt);
        config
    }

    /// D-T-8: the rules are checked against the prices in the write, so the PATCH waits for a
    /// price write in progress.
    #[tokio::test]
    async fn a_patch_waits_for_a_price_write() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &judge_body()).await;
        let id = id_of(&created);
        let guard = app.state.ingest.prices().lock_writes().await;
        let rename = json!({"name": "Renamed judge"});
        let mut pending = std::pin::pin!(patch(&app, &seeded, &id, &rename));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), pending.as_mut())
                .await
                .is_err(),
            "the patch did not wait for the price write"
        );
        drop(guard);
        let response = pending.await;
        assert_eq!(response.status, 200, "{}", text(&response));
    }

    #[tokio::test]
    async fn delete_with_rules_is_409_listing_them() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &judge_body()).await;
        let id = id_of(&created);
        let rule = rule_on(
            &app,
            &seeded,
            Uuid::from_str(&id).unwrap(),
            "Blocks delete",
            None,
        )
        .await;
        let response = app
            .request(Method::DELETE, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, 409, "{}", text(&response));
        let problem = response.problem();
        assert_eq!(
            problem.detail.as_deref(),
            Some("online rules use this evaluator")
        );
        assert_eq!(problem.errors.len(), 1);
        assert_eq!(problem.errors[0].field, format!("rules.{rule}"));
        assert_eq!(
            problem.errors[0].message,
            "Blocks delete: uses this evaluator"
        );
        let kept = app
            .request(Method::GET, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(kept.status, 200);
    }

    /// Review 2026-11-15 #1: the rule check ran before the project check, so another project's
    /// evaluator id answered 409 with that project's rule ids and names.
    #[tokio::test]
    async fn delete_from_another_project_names_none_of_its_rules() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        let (other, _) = app.add_project(&seeded, "other", None).await;
        let created = create_ok(&app, &seeded, &judge_body()).await;
        let id = id_of(&created);
        rule_on(
            &app,
            &seeded,
            Uuid::from_str(&id).unwrap(),
            "Secret rule name",
            None,
        )
        .await;
        let response = app
            .request(Method::DELETE, &format!("{}/{id}", path(other.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, 404, "{}", text(&response));
        assert!(!text(&response).contains("Secret rule name"));
    }

    #[tokio::test]
    async fn patch_that_drops_a_mapped_prompt_variable_is_409() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &judge_body()).await;
        let id = id_of(&created);
        let rule = rule_on(
            &app,
            &seeded,
            Uuid::from_str(&id).unwrap(),
            "Maps both",
            None,
        )
        .await;
        let config = judge_config_with_prompt("Q: {{ input }} A: {{ output }} C: {{ context }}");
        let response = patch(&app, &seeded, &id, &json!({"config": config})).await;
        assert_eq!(response.status, 409, "{}", text(&response));
        let problem = response.problem();
        assert_eq!(
            problem.detail.as_deref(),
            Some("the change would break online rules that use this evaluator")
        );
        assert_eq!(problem.errors.len(), 1);
        assert_eq!(problem.errors[0].field, format!("rules.{rule}"));
        assert_eq!(
            problem.errors[0].message,
            "Maps both: variable context is used by the prompt but not mapped"
        );
        let read = app
            .request(Method::GET, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await
            .json::<Value>();
        assert_eq!(read["version"], created["version"]);
    }

    #[tokio::test]
    async fn patch_that_moves_to_an_unpriced_model_breaks_a_budgeted_rule() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &judge_body()).await;
        let id = id_of(&created);
        rule_on(
            &app,
            &seeded,
            Uuid::from_str(&id).unwrap(),
            "Budgeted",
            Some(5.0),
        )
        .await;
        let mut config = judge_body()["config"].clone();
        config["model"] = json!("no-such-model-0");
        let response = patch(&app, &seeded, &id, &json!({"config": config})).await;
        assert_eq!(response.status, 409, "{}", text(&response));
        assert_eq!(
            response.problem().errors[0].message,
            "Budgeted: the judge model has no price, so the rule cannot have a budget; add one under Settings → Model prices"
        );
    }

    #[tokio::test]
    async fn a_rule_saved_during_the_patch_is_checked() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &judge_body()).await;
        let id = Uuid::from_str(&id_of(&created)).unwrap();
        let (project, org, now) = (seeded.project.id, seeded.org_id, app.state.clock.now_ns());
        // The PATCH read the evaluator; a rule is saved before its write.
        let current = store::get_evaluator(app.state.meta.reader(), project, id)
            .await
            .unwrap()
            .unwrap();
        rule_on(&app, &seeded, id, "Saved late", None).await;
        let config = judge_config_with_prompt("Q: {{ input }} A: {{ output }} C: {{ context }}");
        let parsed = EvaluatorConfig::from_parts("llm_judge", &config).unwrap();
        let fields = EvaluatorFields {
            name: current.name.clone(),
            config: config.to_string(),
            output_score: current.output_score.clone(),
            connection_id: current.connection_id,
            secret_enc: None,
            version: current.version + 1,
        };
        let checks = RuleChecks {
            subject: Subject::from_config(&parsed),
            prices: app.state.ingest.prices().builtin(),
            now,
        };
        let outcome = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    write_update(conn, org, project, id, current, fields, None, &checks).await
                })
            })
            .await
            .unwrap();
        let Outcome::BreaksRules(errors) = outcome else {
            panic!("the saved rule breaks the change");
        };
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.starts_with("Saved late: "), "{errors:?}");
    }

    #[tokio::test]
    async fn a_rename_does_not_check_the_rules() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        // The judge's prompt uses `context`, which the rule does not map: the rule is broken already.
        let mut body = judge_body();
        body["config"]["prompt"] = json!("Q: {{ input }} A: {{ output }} C: {{ context }}");
        let created = create_ok(&app, &seeded, &body).await;
        let id = Uuid::from_str(&id_of(&created)).unwrap();
        rule_on(&app, &seeded, id, "Already broken", None).await;
        let renamed = patch(&app, &seeded, &id.to_string(), &json!({"name": "Renamed"})).await;
        assert_eq!(renamed.status, 200, "{}", text(&renamed));
        assert_eq!(renamed.json::<Value>()["name"], "Renamed");
    }

    #[tokio::test]
    async fn webhook_header_values_are_checked() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let mut body = webhook_body("Hooked");
        body["config"]["headers"] = json!({"X-A": "one", "x-a": "two"});
        assert_eq!(
            field_errors(&create(&app, &seeded, &body).await),
            [("config.headers.x-a".to_owned(), "invalid header".to_owned())]
        );
        body["config"]["headers"] = json!({"X-Bad": "line\r\nbreak"});
        assert_eq!(
            field_errors(&create(&app, &seeded, &body).await),
            [(
                "config.headers.x-bad".to_owned(),
                "invalid header".to_owned()
            )]
        );
        body["config"]["headers"] = json!({"X-Long": "a".repeat(4097)});
        assert_eq!(
            field_errors(&create(&app, &seeded, &body).await),
            [(
                "config.headers.x-long".to_owned(),
                "invalid header".to_owned()
            )]
        );
    }

    #[tokio::test]
    async fn evaluator_cap_is_500() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    for index in 0..500 {
                        let fields = EvaluatorFields {
                            name: format!("row-{index:03}"),
                            config: "{}".to_owned(),
                            output_score: "{}".to_owned(),
                            connection_id: None,
                            secret_enc: None,
                            version: 1,
                        };
                        store::insert_evaluator(
                            conn,
                            Uuid::now_v7(),
                            project,
                            "check",
                            fields,
                            None,
                            now,
                        )
                        .await?;
                    }
                    Ok::<(), MetaError>(())
                })
            })
            .await
            .unwrap();
        let response = create(&app, &seeded, &check_body("Five hundred and first")).await;
        assert_eq!(response.status, 409, "{}", text(&response));
        assert!(text(&response).contains("a project has at most 500 evaluators"));
    }

    #[tokio::test]
    async fn list_pages_by_name() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        for name in ["c", "a", "b"] {
            create_ok(&app, &seeded, &check_body(name)).await;
        }
        let first = app
            .request(Method::GET, &format!("{}?limit=2", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await
            .json::<Value>();
        let names = |page: &Value| -> Vec<String> {
            page["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["name"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(names(&first), ["a", "b"]);
        let cursor = first["next_cursor"].as_str().unwrap();
        let second = app
            .request(
                Method::GET,
                &format!("{}?limit=2&cursor={cursor}", path(seeded.project.id)),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await
            .json::<Value>();
        assert_eq!(names(&second), ["c"]);
        assert!(second["next_cursor"].is_null());
    }

    #[tokio::test]
    async fn no_webhook_values_in_logs() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (outcome, logs) = capture_logs(async {
            let created = create(&app, &seeded, &webhook_body("Hooked")).await;
            assert_eq!(created.status, 201, "{}", text(&created));
            let id = id_of(&created.json::<Value>());
            let renamed = patch(&app, &seeded, &id, &json!({"name": "Renamed"})).await;
            assert_eq!(renamed.status, 200);
            let moved = patch(
                &app,
                &seeded,
                &id,
                &json!({
                    "config": {"url": MOVED_URL, "headers": {"X-Team": MOVED_HEADER}},
                    "webhook_secret": MOVED_SECRET
                }),
            )
            .await;
            assert_eq!(moved.status, 200, "{}", text(&moved));
            moved.status
        })
        .await;
        assert_eq!(outcome, 200);
        let rows = evaluator_audit_rows(&app).await;
        let actions: Vec<&str> = rows.iter().map(|row| row.0.as_str()).collect();
        assert_eq!(
            actions,
            ["evaluator.created", "evaluator.updated", "evaluator.updated"]
        );
        // The host is recorded; the URL, the headers and the secrets are not.
        assert_eq!(rows[0].2, r#"{"kind":"webhook","url_host":"hooks.example.com"}"#);
        assert_eq!(rows[1].2, r#"{"fields":["name"],"url_host":"hooks.example.com"}"#);
        assert_eq!(
            rows[2].2,
            r#"{"fields":["config","webhook_secret"],"url_host":"hooks.example.net"}"#
        );
        for sealed in [
            WEBHOOK_SECRET,
            "/private/path",
            "query-secret",
            WEBHOOK_HEADER_VALUE,
            MOVED_SECRET,
            "/second/route",
            "moved-query-secret",
            MOVED_HEADER,
        ] {
            assert!(!logs.contains(sealed), "logs show {sealed}");
            for row in &rows {
                assert!(!row.2.contains(sealed), "audit details show {sealed}");
            }
        }
    }

    /// `(action, target_id, details)` of every audit row of an evaluator, oldest first.
    async fn evaluator_audit_rows(app: &TestApp) -> Vec<(String, Option<String>, String)> {
        sqlx::query_as(
            "SELECT action, target_id, details FROM audit_events \
             WHERE target_kind = 'evaluator' ORDER BY seq",
        )
        .fetch_all(app.state.meta.reader())
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn evaluator_delete_in_use_records_nothing() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &judge_body()).await;
        let id = id_of(&created);
        rule_on(
            &app,
            &seeded,
            Uuid::from_str(&id).unwrap(),
            "Blocks delete",
            None,
        )
        .await;
        let response = app
            .request(Method::DELETE, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, 409, "{}", text(&response));
        let actions: Vec<String> = evaluator_audit_rows(&app)
            .await
            .into_iter()
            .map(|row| row.0)
            .collect();
        assert_eq!(actions, ["evaluator.created"]);
    }

    /// The trace of the test-run tests, and its root, child and evaluation-trace ids.
    /// The number of `evaluator_versions` rows of evaluator `id`.
    async fn version_count(app: &TestApp, id: Uuid) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM evaluator_versions WHERE evaluator_id = ?")
            .bind(id.to_string())
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap()
    }

    /// `GET {evaluator}/versions{query}` as `seeded`'s owner.
    async fn get_versions(app: &TestApp, seeded: &Seeded, id: &str, query: &str) -> TestResponse {
        app.request(
            Method::GET,
            &format!("{}/{id}/versions{query}", path(seeded.project.id)),
        )
        .header(header::COOKIE.as_str(), &seeded.cookie)
        .send()
        .await
    }

    /// The version numbers of a versions answer, newest first.
    fn version_numbers(page: &Value) -> Vec<i64> {
        page["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|version| version["version"].as_i64().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn versions_follow_creates_and_updates() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &judge_body()).await;
        let id = id_of(&created);
        let original = judge_body()["config"]["prompt"].clone();
        patch_ok(
            &app,
            &seeded,
            &id,
            &json!({"config": judge_config_with_prompt("Rate the answer again.")}),
        )
        .await;

        let response = get_versions(&app, &seeded, &id, "").await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let page: Value = response.json();
        assert_eq!(version_numbers(&page), [2, 1]);
        assert_eq!(
            page["data"][0]["config"]["prompt"],
            "Rate the answer again."
        );
        assert_eq!(page["data"][1]["config"]["prompt"], original);
        assert!(page["data"][0]["created_by"].is_string());
        assert!(page["data"][1]["created_by"].is_string());
        assert!(page["next_cursor"].is_null());
        assert!(
            page["data"][0]["created_at"].as_str().unwrap()
                >= page["data"][1]["created_at"].as_str().unwrap()
        );
    }

    #[tokio::test]
    async fn versions_match_the_evaluator_after_many_patches() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, &check_body("Counted")).await;
        let id = id_of(&created);
        for value in ["one", "two", "three", "four"] {
            patch_ok(
                &app,
                &seeded,
                &id,
                &json!({"config": {"check": "contains", "value": value}}),
            )
            .await;
        }
        let read = app
            .request(Method::GET, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await
            .json::<Value>();
        assert_eq!(read["version"], 5);
        let uuid = Uuid::from_str(&id).unwrap();
        let (max, count): (i64, i64) = sqlx::query_as(
            "SELECT MAX(version), COUNT(*) FROM evaluator_versions WHERE evaluator_id = ?",
        )
        .bind(uuid.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .unwrap();
        assert_eq!((max, count), (5, 5));
        let page: Value = get_versions(&app, &seeded, &id, "").await.json();
        assert_eq!(version_numbers(&page), [5, 4, 3, 2, 1]);
    }

    #[tokio::test]
    async fn a_patch_that_changes_nothing_writes_no_version() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, &check_body("Still")).await;
        let id = id_of(&created);
        let same = patch_ok(
            &app,
            &seeded,
            &id,
            &json!({
                "config": {"check": "contains", "value": "refund"},
                "output_score": {"name": "mentions_refund", "data_type": "boolean"}
            }),
        )
        .await;
        assert_eq!(same["version"], 1);
        let uuid = Uuid::from_str(&id).unwrap();
        assert_eq!(version_count(&app, uuid).await, 1);
    }

    #[tokio::test]
    async fn a_rename_writes_no_version() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, &check_body("Refund")).await;
        let id = id_of(&created);
        patch_ok(&app, &seeded, &id, &json!({"name": "Renamed"})).await;
        let page: Value = get_versions(&app, &seeded, &id, "").await.json();
        assert_eq!(version_numbers(&page), [1]);
        assert_eq!(page["data"][0]["name"], "Refund");
        let read: Value = app
            .request(Method::GET, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await
            .json();
        assert_eq!(read["version"], 1);
        assert_eq!(read["name"], "Renamed");
    }

    #[tokio::test]
    async fn a_patch_that_breaks_a_rule_writes_no_version() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        fixture_connection(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &judge_body()).await;
        let id = id_of(&created);
        rule_on(
            &app,
            &seeded,
            Uuid::from_str(&id).unwrap(),
            "Maps both",
            None,
        )
        .await;
        let config = judge_config_with_prompt("Q: {{ input }} A: {{ output }} C: {{ context }}");
        let response = patch(&app, &seeded, &id, &json!({"config": config})).await;
        assert_eq!(response.status, 409, "{}", text(&response));
        let uuid = Uuid::from_str(&id).unwrap();
        assert_eq!(version_count(&app, uuid).await, 1);
        let read: Value = app
            .request(Method::GET, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await
            .json();
        assert_eq!(read["version"], 1);
    }

    #[tokio::test]
    async fn a_duplicate_name_patch_writes_no_version() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, &check_body("First")).await;
        let second = create_ok(&app, &seeded, &check_body("Second")).await;
        let id = id_of(&second);
        let response = patch(
            &app,
            &seeded,
            &id,
            &json!({
                "name": "First",
                "config": {"check": "contains", "value": "changed"}
            }),
        )
        .await;
        assert_eq!(response.status, 409, "{}", text(&response));
        let uuid = Uuid::from_str(&id).unwrap();
        assert_eq!(version_count(&app, uuid).await, 1);
        let read: Value = app
            .request(Method::GET, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await
            .json();
        assert_eq!(read["version"], 1);
    }

    #[tokio::test]
    async fn versions_page_with_a_cursor() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, &check_body("Paged")).await;
        let id = id_of(&created);
        for value in ["one", "two", "three"] {
            patch_ok(
                &app,
                &seeded,
                &id,
                &json!({"config": {"check": "contains", "value": value}}),
            )
            .await;
        }
        let first: Value = get_versions(&app, &seeded, &id, "?limit=3").await.json();
        assert_eq!(version_numbers(&first), [4, 3, 2]);
        let cursor = first["next_cursor"].as_str().unwrap().to_owned();
        let second: Value = get_versions(&app, &seeded, &id, &format!("?limit=3&cursor={cursor}"))
            .await
            .json();
        assert_eq!(version_numbers(&second), [1]);
        assert!(second["next_cursor"].is_null());
        let bad = get_versions(&app, &seeded, &id, "?cursor=nope").await;
        assert_eq!(
            field_errors(&bad),
            [("cursor".to_owned(), "invalid cursor".to_owned())]
        );
    }

    #[tokio::test]
    async fn versions_never_hold_webhook_values() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, &webhook_body("Hooked")).await;
        let id = id_of(&created);
        patch_ok(
            &app,
            &seeded,
            &id,
            &json!({
                "config": {"url": MOVED_URL, "headers": {"X-Team": MOVED_HEADER}},
                "webhook_secret": MOVED_SECRET
            }),
        )
        .await;
        let response = get_versions(&app, &seeded, &id, "").await;
        let body = text(&response);
        for (index, secret) in [
            WEBHOOK_SECRET,
            WEBHOOK_HEADER_VALUE,
            MOVED_SECRET,
            MOVED_HEADER,
            "/private/path",
            "query-secret",
            "/second/route",
            "moved-query-secret",
            "secret_enc",
            "signing_secret",
            "has_signing_secret",
        ]
        .into_iter()
        .enumerate()
        {
            assert!(
                !body.contains(secret),
                "forbidden value #{index} in the versions page"
            );
        }
        let page: Value = response.json();
        let evaluator: Value = app
            .request(Method::GET, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await
            .json();
        assert_eq!(page["data"][0]["config"], evaluator["config"]);
        let stored: String = sqlx::query_scalar(
            "SELECT group_concat(config || output_score) FROM evaluator_versions",
        )
        .fetch_one(app.state.meta.reader())
        .await
        .unwrap();
        for (index, secret) in [
            WEBHOOK_SECRET,
            MOVED_SECRET,
            WEBHOOK_HEADER_VALUE,
            MOVED_HEADER,
        ]
        .into_iter()
        .enumerate()
        {
            assert!(!stored.contains(secret), "secret #{index} in the history");
        }
    }

    #[tokio::test]
    async fn viewer_reads_versions() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, &check_body("Visible")).await;
        let id = id_of(&created);
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let response = app
            .request(
                Method::GET,
                &format!("{}/{id}/versions", path(seeded.project.id)),
            )
            .header(header::COOKIE.as_str(), &viewer.cookie)
            .send()
            .await;
        assert_eq!(response.status, 200, "{}", text(&response));
    }

    #[tokio::test]
    async fn a_read_api_key_reads_versions() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, &check_body("Keyed")).await;
        let id = id_of(&created);
        let response = app
            .request(
                Method::GET,
                &format!("{}/{id}/versions", path(seeded.project.id)),
            )
            .header(
                header::AUTHORIZATION.as_str(),
                &format!("Bearer {}", seeded.read_key),
            )
            .send()
            .await;
        assert_eq!(response.status, 200, "{}", text(&response));
    }

    #[tokio::test]
    async fn another_projects_evaluator_versions_are_404() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let (other, _) = app.add_project(&seeded, "other", None).await;
        let created = create_ok(&app, &seeded, &check_body("Mine")).await;
        let id = id_of(&created);
        let foreign = app
            .request(Method::GET, &format!("{}/{id}/versions", path(other.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(foreign.status, 404);
        let unknown = get_versions(&app, &seeded, &Uuid::now_v7().to_string(), "").await;
        assert_eq!(unknown.status, 404);
    }

    #[tokio::test]
    async fn deleting_an_evaluator_removes_its_versions() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, &check_body("Going")).await;
        let id = id_of(&created);
        let uuid = Uuid::from_str(&id).unwrap();
        assert_eq!(version_count(&app, uuid).await, 1);
        let removed = app
            .request(Method::DELETE, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(removed.status, 204);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM evaluator_versions")
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    const RUN_TRACE: &str = "a1000000000000000000000000000001";
    const EVAL_TRACE: &str = "a1000000000000000000000000000002";
    const ROOT: &str = "b100000000000001";
    const CHILD: &str = "b100000000000002";
    const QUESTION: &str = "What is the refund window?";
    const ANSWER: &str = "30 days.";

    /// A test app that may reach mocks on `127.0.0.1`. The clock is set to the real time before
    /// anything is seeded, so the judge trace's span times and the session cookie agree with it
    /// (`crate::eval::test_run`, § Conventions).
    async fn test_app() -> (TestApp, Seeded) {
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
        })
        .await;
        let real_now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )
        .unwrap();
        app.state.clock.set(real_now);
        let seeded = app.seed().await;
        (app, seeded)
    }

    /// A stored span of `dataset_span(0)`'s shape, with the given ids and payloads.
    fn stored_span(
        trace: &str,
        span: &str,
        parent: Option<&str>,
        input: &str,
        output: &str,
        environment: &str,
    ) -> Span {
        let mut row = dataset_span(0);
        row.trace_id = trace.parse::<TraceId>().unwrap();
        row.span_id = span.parse::<SpanId>().unwrap();
        row.parent_span_id = parent.map(|id| id.parse::<SpanId>().unwrap());
        row.input = Some(input.to_owned());
        row.input_format = Some(PayloadFormat::Text);
        row.output = Some(output.to_owned());
        row.output_format = Some(PayloadFormat::Text);
        row.environment = Some(environment.to_owned());
        row
    }

    /// The root span of `RUN_TRACE`: the refund question and its answer.
    fn root_span() -> Span {
        stored_span(RUN_TRACE, ROOT, None, QUESTION, ANSWER, "production")
    }

    /// Creates a connection of `acme` that reaches `mock`, with the default model `gpt-test`.
    async fn mocked_connection(app: &TestApp, cookie: &str, mock: &MockLlm) -> Uuid {
        let response = app
            .request(Method::POST, "/api/v1/orgs/acme/llm-connections")
            .header(header::COOKIE.as_str(), cookie)
            .json(&json!({
                "name": "Mocked",
                "adapter": "openai",
                "api_key": "sk-test-key-0123456789",
                "base_url": mock.base_url(),
                "default_model": "gpt-test"
            }))
            .send()
            .await;
        assert_eq!(response.status, 201, "{}", text(&response));
        Uuid::from_str(&id_of(&response.json::<Value>())).unwrap()
    }

    /// The judge of `fixtures/api/requests/evaluator-create.json`, on `connection` and `gpt-test`.
    fn judge_on(connection: Uuid) -> Value {
        let mut config = judge_body()["config"].clone();
        config["connection_id"] = json!(connection);
        config["model"] = json!("gpt-test");
        json!({
            "kind": "llm_judge",
            "config": config,
            "output_score": {"name": "relevance", "data_type": "numeric"}
        })
    }

    /// An `exact_match` check that writes a boolean named `same`.
    fn same_check() -> Value {
        json!({
            "kind": "check",
            "config": {"check": "exact_match"},
            "output_score": {"name": "same", "data_type": "boolean"}
        })
    }

    /// A trace-mode body with `same_check` over `mapping`.
    fn trace_run(trace: &str, span: Option<&str>, mapping: &Value) -> Value {
        let mut body = json!({
            "evaluator": same_check(),
            "trace_id": trace,
            "variable_mapping": mapping
        });
        if let Some(span) = span {
            body["span_id"] = json!(span);
        }
        body
    }

    /// `POST …/evaluators/test`.
    async fn run_test(app: &TestApp, seeded: &Seeded, cookie: &str, body: &Value) -> TestResponse {
        app.request(Method::POST, &format!("{}/test", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), cookie)
            .json(body)
            .send()
            .await
    }

    /// `run_test` that must answer 200, as JSON.
    async fn run_ok(app: &TestApp, seeded: &Seeded, body: &Value) -> Value {
        let response = run_test(app, seeded, &seeded.cookie, body).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        response.json()
    }

    /// The judge trace's id of an answer.
    fn judge_trace_of(answer: &Value) -> String {
        answer["judge_trace_id"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn judge_test_on_a_trace() {
        let mock = MockLlm::start(vec![completion(
            r#"{"explanation": "Relevant.", "value": 0.9}"#,
        )])
        .await;
        let (app, seeded) = test_app().await;
        let connection = mocked_connection(&app, &seeded.cookie, &mock).await;
        app.commit_spans(&seeded.project, vec![root_span()]).await;

        let answer = run_ok(
            &app,
            &seeded,
            &json!({
                "evaluator": judge_on(connection),
                "trace_id": RUN_TRACE,
                "variable_mapping": {"input": "trace.input", "output": "trace.output"}
            }),
        )
        .await;
        assert_eq!(answer["ok"], true, "{answer}");
        assert_eq!(
            answer["value"],
            json!({"data_type": "numeric", "value": 0.9})
        );
        assert_eq!(answer["reason"], "Relevant.");
        assert_eq!(answer["variables"]["input"], QUESTION);
        assert!(answer["prompt"].as_str().unwrap().contains(QUESTION));
        assert_eq!(answer["usage"]["input_tokens"], 10);
        assert_eq!(answer["trace_truncated"], false);
        let judge = judge_trace_of(&answer);

        let read = app
            .request(
                Method::GET,
                &format!("/api/v1/projects/{}/traces/{judge}", seeded.project.id),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(read.status, 200, "{}", text(&read));
        assert!(
            text(&read).contains("cetra-eval"),
            "the judge trace is in cetra-eval"
        );

        app.state
            .usage
            .flush(&app.state.meta)
            .await
            .expect("usage flushes");
        let counted: (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(judge_input_tokens), 0), COALESCE(SUM(spans), 0) \
             FROM usage_daily WHERE project_id = ?",
        )
        .bind(seeded.project.id.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .unwrap();
        assert_eq!(
            counted,
            (10, 1),
            "the target span counts, the judge spans do not"
        );
    }

    #[tokio::test]
    async fn test_with_literal_variables() {
        let mock = MockLlm::start(vec![completion(
            r#"{"explanation": "Relevant.", "value": 0.4}"#,
        )])
        .await;
        let (app, seeded) = test_app().await;
        let connection = mocked_connection(&app, &seeded.cookie, &mock).await;
        let answer = run_ok(
            &app,
            &seeded,
            &json!({
                "evaluator": judge_on(connection),
                "variables": {"input": "a literal question", "output": "a literal answer"}
            }),
        )
        .await;
        assert_eq!(answer["ok"], true, "{answer}");
        assert_eq!(
            answer["value"],
            json!({"data_type": "numeric", "value": 0.4})
        );
        assert_eq!(answer["variables"]["input"], "a literal question");
        assert_eq!(answer["trace_truncated"], false);
        assert!(answer.get("judge_trace_id").is_some());
    }

    #[tokio::test]
    async fn unsaved_evaluator_runs_without_saving() {
        let (app, seeded) = test_app().await;
        let answer = run_ok(
            &app,
            &seeded,
            &json!({"evaluator": same_check(), "variables": {"output": "a", "expected": "a"}}),
        )
        .await;
        assert_eq!(answer["ok"], true, "{answer}");
        let saved: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM evaluators")
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap();
        assert_eq!(saved, 0);
    }

    #[tokio::test]
    async fn check_test_runs_without_a_provider() {
        let (app, seeded) = test_app().await;
        let answer = run_ok(
            &app,
            &seeded,
            &json!({"evaluator": same_check(), "variables": {"output": "a", "expected": "a"}}),
        )
        .await;
        assert_eq!(answer["ok"], true, "{answer}");
        assert_eq!(
            answer["value"],
            json!({"data_type": "boolean", "value": true})
        );
        assert!(answer.get("usage").is_none(), "a check makes no LLM call");
    }

    #[tokio::test]
    async fn mapping_error_has_no_judge_trace() {
        let (app, seeded) = test_app().await;
        app.commit_spans(&seeded.project, vec![root_span()]).await;
        // `span.input` has no target span without `span_id`.
        let answer = run_ok(
            &app,
            &seeded,
            &trace_run(
                RUN_TRACE,
                None,
                &json!({"output": "span.output", "expected": "span.output"}),
            ),
        )
        .await;
        assert_eq!(answer["ok"], false, "{answer}");
        assert_eq!(answer["error"]["class"], "permanent");
        assert!(answer.get("judge_trace_id").is_none());
        assert_eq!(answer["variables"], json!({}));
        assert_eq!(answer["trace_truncated"], false);
    }

    #[tokio::test]
    async fn evaluation_trace_is_refused() {
        let (app, seeded) = test_app().await;
        app.commit_spans(
            &seeded.project,
            vec![stored_span(
                EVAL_TRACE,
                ROOT,
                None,
                "judge input",
                "judge output",
                "cetra-eval",
            )],
        )
        .await;
        let response = run_test(
            &app,
            &seeded,
            &seeded.cookie,
            &trace_run(
                EVAL_TRACE,
                None,
                &json!({"output": "trace.output", "expected": "trace.output"}),
            ),
        )
        .await;
        assert_eq!(response.status, 400, "{}", text(&response));
        assert!(text(&response).contains("target is an evaluation trace"));
    }

    #[tokio::test]
    async fn unknown_trace_is_404() {
        let (app, seeded) = test_app().await;
        let response = run_test(
            &app,
            &seeded,
            &seeded.cookie,
            &trace_run(
                "a1000000000000000000000000000009",
                None,
                &json!({"output": "trace.output", "expected": "trace.output"}),
            ),
        )
        .await;
        assert_eq!(response.status, 404, "{}", text(&response));
    }

    #[tokio::test]
    async fn provider_failure_is_reported() {
        let mock = MockLlm::start(vec![Reply {
            status: 500,
            body: json!({"error": {"message": "boom"}}),
        }])
        .await;
        let (app, seeded) = test_app().await;
        let connection = mocked_connection(&app, &seeded.cookie, &mock).await;
        let answer = run_ok(
            &app,
            &seeded,
            &json!({
                "evaluator": judge_on(connection),
                "variables": {"input": "q", "output": "a"}
            }),
        )
        .await;
        assert_eq!(answer["ok"], false, "{answer}");
        assert_eq!(answer["error"]["class"], "retryable");
    }

    #[tokio::test]
    async fn long_variables_are_cut() {
        let (app, seeded) = test_app().await;
        let long = "x".repeat(3_000);
        let answer = run_ok(
            &app,
            &seeded,
            &json!({"evaluator": same_check(), "variables": {"output": long, "expected": "x"}}),
        )
        .await;
        assert_eq!(answer["ok"], true, "{answer}");
        let shown = answer["variables"]["output"].as_str().unwrap();
        assert_eq!(shown.chars().count(), 2_001);
        assert!(shown.ends_with('…'));
        assert_eq!(answer["variables"]["expected"], "x");
    }

    #[tokio::test]
    async fn test_runs_are_rate_limited() {
        let (app, seeded) = test_app().await;
        let body =
            json!({"evaluator": same_check(), "variables": {"output": "a", "expected": "a"}});
        for _ in 0..10 {
            let response = run_test(&app, &seeded, &seeded.cookie, &body).await;
            assert_eq!(response.status, 200, "{}", text(&response));
        }
        let limited = run_test(&app, &seeded, &seeded.cookie, &body).await;
        assert_eq!(limited.status, 429, "{}", text(&limited));
    }

    #[tokio::test]
    async fn viewer_cannot_test() {
        let (app, seeded) = test_app().await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let response = run_test(
            &app,
            &seeded,
            &viewer.cookie,
            &json!({"evaluator": same_check(), "variables": {"output": "a", "expected": "a"}}),
        )
        .await;
        assert_eq!(response.status, 403, "{}", text(&response));
    }

    #[tokio::test]
    async fn no_payload_in_logs() {
        let (app, seeded) = test_app().await;
        let (answer, logs) = capture_logs(run_test(
            &app,
            &seeded,
            &seeded.cookie,
            &json!({
                "evaluator": same_check(),
                "variables": {"output": "private-answer-xyz", "expected": "private-answer-xyz"}
            }),
        ))
        .await;
        assert_eq!(answer.status, 200, "{}", text(&answer));
        assert!(logs.contains("evaluator tested"), "{logs}");
        assert!(
            !logs.contains("private-answer-xyz"),
            "logs show the payload: {logs}"
        );

        // A judge run: the rendered prompt and the provider's answer must stay out of the logs.
        let mock = MockLlm::start(vec![completion(
            r#"{"explanation": "MODEL-ANSWER-TEXT-7731", "value": 0.9}"#,
        )])
        .await;
        let connection = mocked_connection(&app, &seeded.cookie, &mock).await;
        let (judged, logs) = capture_logs(run_test(
            &app,
            &seeded,
            &seeded.cookie,
            &json!({
                "evaluator": judge_on(connection),
                "variables": {"input": "PROMPT-TEXT-PRIVATE-4412", "output": "a"}
            }),
        ))
        .await;
        assert_eq!(judged.status, 200, "{}", text(&judged));
        let answer: Value = judged.json();
        assert_eq!(answer["ok"], true, "{answer}");
        assert_eq!(answer["reason"], "MODEL-ANSWER-TEXT-7731", "{answer}");
        let prompt = answer["prompt"].as_str().expect("a rendered prompt");
        assert!(prompt.contains("PROMPT-TEXT-PRIVATE-4412"), "{prompt}");
        assert!(logs.contains("evaluator tested"), "{logs}");
        assert!(
            !logs.contains("PROMPT-TEXT-PRIVATE-4412"),
            "logs show the prompt: {logs}"
        );
        assert!(!logs.contains(prompt), "logs show the prompt: {logs}");
        assert!(
            !logs.contains("MODEL-ANSWER-TEXT-7731"),
            "logs show the provider answer: {logs}"
        );
    }

    #[tokio::test]
    async fn webhook_test_signs_with_the_saved_secret() {
        let hook = MockWebhook::start(200, json!({"value": 0.5})).await;
        let (app, seeded) = test_app().await;
        let created = create_ok(
            &app,
            &seeded,
            &json!({
                "name": "Hooked",
                "kind": "webhook",
                "config": {"url": hook.url()},
                "output_score": {"name": "hook", "data_type": "numeric"},
                "webhook_secret": "0123456789abcdef"
            }),
        )
        .await;
        let answer = run_ok(
            &app,
            &seeded,
            &json!({"evaluator_id": id_of(&created), "variables": {"output": "x"}}),
        )
        .await;
        assert_eq!(answer["ok"], true, "{answer}");
        assert_eq!(
            answer["value"],
            json!({"data_type": "numeric", "value": 0.5})
        );
        let requests = hook.requests();
        assert_eq!(requests.len(), 1);
        let signature = requests[0]
            .0
            .get("x-cetra-signature")
            .expect("a signature header")
            .to_str()
            .unwrap();
        assert!(signature.starts_with("t="), "{signature}");
        assert!(signature.contains("v1="), "{signature}");
    }

    #[tokio::test]
    async fn both_given_takes_the_saved_webhook_values() {
        let hook = MockWebhook::start(200, json!({"value": 0.5})).await;
        let (app, seeded) = test_app().await;
        let created = create_ok(
            &app,
            &seeded,
            &json!({
                "name": "Hooked",
                "kind": "webhook",
                "config": {"url": hook.url()},
                "output_score": {"name": "hook", "data_type": "numeric"},
                "webhook_secret": "0123456789abcdef"
            }),
        )
        .await;
        let answer = run_ok(
            &app,
            &seeded,
            &json!({
                "evaluator_id": id_of(&created),
                "evaluator": {
                    "kind": "webhook",
                    "output_score": {"name": "other", "data_type": "numeric"}
                },
                "variables": {"output": "x"}
            }),
        )
        .await;
        assert_eq!(answer["ok"], true, "{answer}");
        let requests = hook.requests();
        assert_eq!(requests.len(), 1, "the saved URL was called");
        assert!(requests[0].0.get("x-cetra-signature").is_some());
    }

    #[tokio::test]
    async fn both_given_with_another_kind_is_400() {
        let hook = MockWebhook::start(200, json!({"value": 0.5})).await;
        let (app, seeded) = test_app().await;
        let created = create_ok(
            &app,
            &seeded,
            &json!({
                "name": "Hooked",
                "kind": "webhook",
                "config": {"url": hook.url()},
                "output_score": {"name": "hook", "data_type": "numeric"},
                "webhook_secret": "0123456789abcdef"
            }),
        )
        .await;
        let response = run_test(
            &app,
            &seeded,
            &seeded.cookie,
            &json!({
                "evaluator_id": id_of(&created),
                "evaluator": {
                    "kind": "check",
                    "output_score": {"name": "same", "data_type": "boolean"}
                },
                "variables": {"output": "a", "expected": "a"}
            }),
        )
        .await;
        assert_eq!(
            field_errors(&response),
            [(
                "evaluator.kind".to_owned(),
                "kind must match the saved evaluator".to_owned()
            )]
        );
        assert!(hook.requests().is_empty());
    }

    #[tokio::test]
    async fn span_target_resolves_span_variables() {
        let (app, seeded) = test_app().await;
        app.commit_spans(
            &seeded.project,
            vec![
                root_span(),
                stored_span(
                    RUN_TRACE,
                    CHILD,
                    Some(ROOT),
                    "child question",
                    "child answer",
                    "production",
                ),
            ],
        )
        .await;
        let answer = run_ok(
            &app,
            &seeded,
            &trace_run(
                RUN_TRACE,
                Some(CHILD),
                &json!({"output": "span.output", "expected": "span.output"}),
            ),
        )
        .await;
        assert_eq!(answer["ok"], true, "{answer}");
        assert_eq!(answer["variables"]["output"], "child answer");
    }

    #[tokio::test]
    async fn trace_start_is_accepted() {
        let (app, seeded) = test_app().await;
        app.commit_spans(&seeded.project, vec![root_span()]).await;
        let start_ms = dataset_span(0).start_time / 1_000_000;
        let mut body = trace_run(
            RUN_TRACE,
            None,
            &json!({"output": "trace.output", "expected": "trace.output"}),
        );
        body["trace_start"] = json!(start_ms.to_string());
        let answer = run_ok(&app, &seeded, &body).await;
        assert_eq!(answer["ok"], true, "{answer}");
    }

    #[tokio::test]
    async fn body_rules() {
        let (app, seeded) = test_app().await;
        let check = same_check();

        let many: serde_json::Map<String, Value> = (0..33)
            .map(|index| (format!("v{index}"), json!("a")))
            .collect();
        let errors = field_errors(
            &run_test(
                &app,
                &seeded,
                &seeded.cookie,
                &json!({"evaluator": check, "variables": many}),
            )
            .await,
        );
        assert!(
            errors.contains(&("variables".to_owned(), "at most 32 variables".to_owned())),
            "{errors:?}"
        );

        let short_trace = trace_run(
            "a100000000000000000000000000001",
            None,
            &json!({"output": "trace.output", "expected": "trace.output"}),
        );
        assert_eq!(
            field_errors(&run_test(&app, &seeded, &seeded.cookie, &short_trace).await),
            [(
                "trace_id".to_owned(),
                "trace_id must be 32 hex digits".to_owned()
            )]
        );

        let span_only = json!({
            "evaluator": same_check(),
            "span_id": ROOT,
            "variables": {"output": "a", "expected": "a"}
        });
        assert_eq!(
            field_errors(&run_test(&app, &seeded, &seeded.cookie, &span_only).await),
            [(
                "trace_id".to_owned(),
                "trace_id is required with span_id and trace_start".to_owned()
            )]
        );

        let neither = json!({"variables": {"output": "a", "expected": "a"}});
        assert_eq!(
            field_errors(&run_test(&app, &seeded, &seeded.cookie, &neither).await),
            [(
                "evaluator".to_owned(),
                "give evaluator_id, evaluator, or both".to_owned()
            )]
        );
    }

    #[tokio::test]
    async fn variables_are_capped_at_256_kib() {
        let (app, seeded) = test_app().await;
        let big = "y".repeat(90_000);
        let body = json!({
            "evaluator": same_check(),
            "variables": {"a": big, "b": big, "c": big}
        });
        assert_eq!(
            field_errors(&run_test(&app, &seeded, &seeded.cookie, &body).await),
            [(
                "variables".to_owned(),
                "variables must be at most 256 KiB in total".to_owned()
            )]
        );
    }

    #[tokio::test]
    async fn levenshtein_inputs_are_capped_at_64_kib() {
        let (app, seeded) = test_app().await;
        let body = json!({
            "evaluator": {
                "kind": "check",
                "config": {"check": "levenshtein"},
                "output_score": {"name": "similar", "data_type": "numeric"}
            },
            "variables": {"output": "x".repeat(64 * 1024 + 1), "expected": "x"}
        });
        assert_eq!(
            field_errors(&run_test(&app, &seeded, &seeded.cookie, &body).await),
            [(
                "variables".to_owned(),
                "levenshtein inputs must be at most 64 KiB each".to_owned()
            )]
        );
    }

    #[tokio::test]
    async fn all_zero_trace_id_is_refused() {
        let (app, seeded) = test_app().await;
        let body = trace_run(
            &"0".repeat(32),
            None,
            &json!({"output": "trace.output", "expected": "trace.output"}),
        );
        assert_eq!(
            field_errors(&run_test(&app, &seeded, &seeded.cookie, &body).await),
            [(
                "trace_id".to_owned(),
                "trace_id must be 32 hex digits".to_owned()
            )]
        );
    }

    #[tokio::test]
    async fn unknown_evaluator_is_404() {
        let (app, seeded) = test_app().await;
        let response = run_test(
            &app,
            &seeded,
            &seeded.cookie,
            &json!({
                "evaluator_id": "01900000-0000-7000-8000-000000000999",
                "variables": {"output": "a", "expected": "a"}
            }),
        )
        .await;
        assert_eq!(response.status, 404, "{}", text(&response));
    }

    #[tokio::test]
    async fn config_is_required_without_evaluator_id() {
        let (app, seeded) = test_app().await;
        let body = json!({
            "evaluator": {
                "kind": "check",
                "output_score": {"name": "same", "data_type": "boolean"}
            },
            "variables": {"output": "a", "expected": "a"}
        });
        assert_eq!(
            field_errors(&run_test(&app, &seeded, &seeded.cookie, &body).await),
            [(
                "evaluator.config".to_owned(),
                "config is required without evaluator_id".to_owned()
            )]
        );
    }

    #[tokio::test]
    async fn new_webhook_config_needs_a_secret_with_evaluator_id() {
        let hook = MockWebhook::start(200, json!({"value": 0.5})).await;
        let (app, seeded) = test_app().await;
        let created = create_ok(&app, &seeded, &webhook_body("Hooked")).await;
        let response = run_test(
            &app,
            &seeded,
            &seeded.cookie,
            &json!({
                "evaluator_id": id_of(&created),
                "evaluator": {
                    "kind": "webhook",
                    "config": {"url": hook.url()},
                    "output_score": {"name": "hook", "data_type": "numeric"}
                },
                "variables": {"output": "x"}
            }),
        )
        .await;
        assert_eq!(
            field_errors(&response),
            [(
                "evaluator.webhook_secret".to_owned(),
                "webhook_secret is required with config".to_owned()
            )]
        );
        assert!(hook.requests().is_empty(), "the saved secret never signs");
    }

    #[tokio::test]
    async fn new_webhook_config_needs_a_secret_without_evaluator_id() {
        // Without `evaluator_id` the body rule of § Test run contract applies, not the
        // "with config" message, which is only for a saved evaluator.
        let (app, seeded) = test_app().await;
        let response = run_test(
            &app,
            &seeded,
            &seeded.cookie,
            &json!({
                "evaluator": {
                    "kind": "webhook",
                    "config": {"url": "https://hooks.example.com/unsent"},
                    "output_score": {"name": "hook", "data_type": "numeric"}
                },
                "variables": {"output": "x"}
            }),
        )
        .await;
        assert_eq!(
            field_errors(&response),
            [(
                "evaluator.webhook_secret".to_owned(),
                "webhook_secret is required".to_owned()
            )]
        );
    }

    #[tokio::test]
    async fn test_evaluator_is_refused_in_restore_mode() {
        let app = TestApp::with_config(|config| config.restore_mode = true).await;
        let seeded = app.seed().await;
        let body = json!({
            "evaluator": judge_on(Uuid::nil()),
            "variables": {"input": "a literal question", "output": "a literal answer"}
        });
        let response = run_test(&app, &seeded, &seeded.cookie, &body).await;
        crate::http::restore::tests::assert_refused(&response, "evaluator test");
    }

    const DAY: i64 = 86_400_000_000_000;
    /// `2026-01-01T12:00:00Z`: the clock of the stats tests (see `eval::stats`).
    const STATS_NOW: i64 = 1_767_268_800_000_000_000;

    /// `GET {project}/evaluators/stats{query}` with `seeded`'s cookie.
    async fn stats(app: &TestApp, seeded: &Seeded, query: &str) -> TestResponse {
        app.request(
            Method::GET,
            &format!("{}/stats{query}", path(seeded.project.id)),
        )
        .header(header::COOKIE.as_str(), &seeded.cookie)
        .send()
        .await
    }

    /// Writes a manual job of `evaluator` in `project` that finished at `finished_at` with the
    /// given status, cost and latency.
    async fn finished_job(
        app: &TestApp,
        project: ProjectId,
        evaluator: Uuid,
        status: &'static str,
        finished_at: i64,
        cost: Option<f64>,
        latency: Option<i64>,
    ) {
        let id = Uuid::now_v7();
        let trace_id: TraceId = format!("{:032x}", id.as_u128()).parse().unwrap();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    eval_jobs::insert_job(
                        conn,
                        NewJob {
                            id,
                            project_id: project,
                            kind: KIND_MANUAL,
                            rule_id: None,
                            evaluator_id: evaluator,
                            variable_mapping: Some("{}".to_owned()),
                            trace_id,
                            span_id: None,
                            trace_start_ns: finished_at,
                        },
                        finished_at,
                    )
                    .await?;
                    sqlx::query(
                        "UPDATE eval_jobs SET status = ?, finished_at = ?, cost_usd = ?, \
                         latency_ms = ? WHERE id = ?",
                    )
                    .bind(status)
                    .bind(finished_at)
                    .bind(cost)
                    .bind(latency)
                    .bind(id.to_string())
                    .execute(&mut *conn)
                    .await?;
                    Ok::<(), MetaError>(())
                })
            })
            .await
            .unwrap();
    }

    /// Writes an experiment named `name` in `project`, over a dataset of its own, created at
    /// `created_at`. Returns its id.
    async fn add_experiment(
        app: &TestApp,
        project: ProjectId,
        name: &str,
        created_at: i64,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let dataset = Uuid::now_v7();
        let name = name.to_owned();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    crate::meta::datasets::insert_dataset(
                        conn,
                        dataset,
                        project,
                        crate::meta::datasets::DatasetFields {
                            name: format!("set {dataset}"),
                            description: None,
                            input_schema: None,
                            expected_schema: None,
                        },
                        created_at,
                    )
                    .await?;
                    crate::meta::experiments::insert_tx(
                        conn,
                        &crate::meta::experiments::NewExperiment {
                            id,
                            project_id: project,
                            dataset_id: dataset,
                            dataset_name: "faq".to_owned(),
                            dataset_version: 1,
                            name,
                            description: None,
                            metadata: "{}".to_owned(),
                            repetitions: 1,
                            item_count: 0,
                            created_by: None,
                            created_at,
                        },
                    )
                    .await?;
                    Ok::<(), MetaError>(())
                })
            })
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn stats_lists_every_evaluator_with_zeros() {
        let app = TestApp::new().await;
        app.state.clock.set(STATS_NOW);
        let seeded = app.seed().await;
        create_ok(&app, &seeded, &check_body("Beta")).await;
        create_ok(&app, &seeded, &check_body("Alpha")).await;

        let response = stats(&app, &seeded, "").await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let body: EvaluatorStatsResponse = response.json();
        assert_eq!(
            body.window.to - body.window.from,
            6 * DAY + 12 * 3_600_000_000_000,
            "six days back from the start of today, then the rest of today"
        );
        assert_eq!(body.data.len(), 2);
        let alpha = &body.data[0];
        assert_eq!(alpha.jobs.done, 0);
        assert_eq!(alpha.jobs.runs_daily.len(), 7);
        assert!(
            alpha
                .jobs
                .runs_daily
                .iter()
                .all(|point| point.value == Some(0.0))
        );
        assert_eq!(alpha.scores.count, 0);
        assert_eq!(alpha.scores.average, None);
        assert_eq!(alpha.experiments_with_score.total, 0);
        assert!(alpha.experiments_with_score.recent.is_empty());
    }

    #[tokio::test]
    async fn stats_combine_jobs_scores_and_experiments() {
        let app = TestApp::new().await;
        app.state.clock.set(STATS_NOW);
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let at = |k: i64| now - 1 - k * DAY;
        let created = create_ok(&app, &seeded, &check_body("Refund check")).await;
        let evaluator: Uuid = created["id"].as_str().unwrap().parse().unwrap();
        for (cost, latency) in [(0.01, 100), (0.02, 200), (0.03, 300)] {
            finished_job(
                &app,
                seeded.project.id,
                evaluator,
                "done",
                at(0),
                Some(cost),
                Some(latency),
            )
            .await;
        }
        finished_job(
            &app,
            seeded.project.id,
            evaluator,
            "failed",
            at(0),
            None,
            None,
        )
        .await;
        for (k, value) in [(0, true), (2, true), (2, false)] {
            put_score(
                &app,
                seeded.project.id,
                TestScore {
                    name: "mentions_refund",
                    value: ScoreValue::Boolean(value),
                    evaluator_id: Some(evaluator),
                    experiment_id: None,
                    created_at: at(k),
                    deleted: false,
                    score_id: None,
                    updated_at: None,
                },
            )
            .await;
        }
        let experiment = add_experiment(&app, seeded.project.id, "candidate", now).await;
        put_score(
            &app,
            seeded.project.id,
            TestScore {
                name: "mentions_refund",
                value: ScoreValue::Boolean(true),
                evaluator_id: None,
                experiment_id: Some(experiment),
                created_at: at(1),
                deleted: false,
                score_id: None,
                updated_at: None,
            },
        )
        .await;

        let response = stats(&app, &seeded, &format!("?evaluator_ids={evaluator}")).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let body: EvaluatorStatsResponse = response.json();
        assert_eq!(body.data.len(), 1);
        let mine = &body.data[0];
        assert_eq!(mine.jobs.done, 3);
        assert_eq!(mine.jobs.failed, 1);
        assert_eq!(mine.jobs.latency_p50_ms, Some(200));
        assert_eq!(mine.jobs.latency_p95_ms, Some(300));
        assert!((mine.jobs.cost_usd_total - 0.06).abs() < 1e-12);
        assert!((mine.jobs.cost_usd_avg.unwrap() - 0.02).abs() < 1e-12);
        assert_eq!(mine.scores.count, 3);
        assert!((mine.scores.pass_rate.unwrap() - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(mine.scores.daily[6].value, Some(1.0));
        assert_eq!(mine.scores.daily[4].value, Some(0.5));
        assert_eq!(mine.experiments_with_score.total, 1);
        assert_eq!(mine.experiments_with_score.recent[0].name, "candidate");
        assert_eq!(mine.experiments_with_score.recent[0].id, experiment);
    }

    #[tokio::test]
    async fn evaluator_ids_filters_and_rejects_bad_values() {
        let app = TestApp::new().await;
        app.state.clock.set(STATS_NOW);
        let seeded = app.seed().await;
        let first = create_ok(&app, &seeded, &check_body("First")).await;
        create_ok(&app, &seeded, &check_body("Second")).await;
        let first_id = first["id"].as_str().unwrap().to_owned();

        let response = stats(&app, &seeded, &format!("?evaluator_ids={first_id}")).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let body: EvaluatorStatsResponse = response.json();
        assert_eq!(body.data.len(), 1);
        assert_eq!(body.data[0].evaluator_id.to_string(), first_id);

        let unknown = Uuid::from_u128(0xdead).to_string();
        let response = stats(&app, &seeded, &format!("?evaluator_ids={unknown}")).await;
        let body: EvaluatorStatsResponse = response.json();
        assert!(body.data.is_empty());

        let all_ids: Vec<String> = (1..=51u128)
            .map(|n| Uuid::from_u128(n).to_string())
            .collect();
        let bad = [
            "not-a-uuid".to_owned(),
            String::new(),
            format!("{first_id},{first_id}"),
            all_ids.join(","),
            format!("{first_id},not-a-uuid"),
        ];
        for query in bad {
            let response = stats(&app, &seeded, &format!("?evaluator_ids={query}")).await;
            let errors = field_errors(&response);
            assert_eq!(errors[0].0, "evaluator_ids", "{query}");
            let body: Value = response.json();
            assert_eq!(body["code"], "bad_request");
        }
    }

    #[tokio::test]
    async fn stats_do_not_leak_across_projects() {
        let app = TestApp::new().await;
        app.state.clock.set(STATS_NOW);
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let created = create_ok(&app, &seeded, &check_body("Leak")).await;
        let evaluator: Uuid = created["id"].as_str().unwrap().parse().unwrap();
        let (other, _) = app.add_project(&seeded, "other", None).await;
        put_score(
            &app,
            other.id,
            TestScore {
                name: "mentions_refund",
                value: ScoreValue::Boolean(true),
                evaluator_id: Some(evaluator),
                experiment_id: None,
                created_at: now - 1,
                deleted: false,
                score_id: None,
                updated_at: None,
            },
        )
        .await;
        finished_job(
            &app,
            other.id,
            evaluator,
            "done",
            now - 1,
            Some(0.5),
            Some(10),
        )
        .await;

        let response = stats(&app, &seeded, &format!("?evaluator_ids={evaluator}")).await;
        let body: EvaluatorStatsResponse = response.json();
        assert_eq!(body.data[0].scores.count, 0);
        assert_eq!(body.data[0].jobs.done, 0);

        let response = app
            .request(Method::POST, &path(other.id))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&check_body("OnlyB"))
            .send()
            .await;
        assert_eq!(response.status, 201, "{}", text(&response));
        let only_b: Value = response.json();
        let only_b = only_b["id"].as_str().unwrap().to_owned();
        let response = stats(&app, &seeded, &format!("?evaluator_ids={only_b}")).await;
        let body: EvaluatorStatsResponse = response.json();
        assert!(body.data.is_empty());
    }

    #[tokio::test]
    async fn viewer_can_read_stats_and_anonymous_cannot() {
        let app = TestApp::new().await;
        app.state.clock.set(STATS_NOW);
        let seeded = app.seed().await;
        let viewer = app
            .add_member(&seeded, "v@example.com", OrgRole::Viewer)
            .await;
        let response = app
            .request(Method::GET, &format!("{}/stats", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &viewer.cookie)
            .send()
            .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let response = app
            .request(Method::GET, &format!("{}/stats", path(seeded.project.id)))
            .send()
            .await;
        assert_eq!(response.status, 401, "{}", text(&response));
    }

    #[tokio::test]
    async fn old_experiment_scores_are_left_out() {
        let app = TestApp::new().await;
        app.state.clock.set(STATS_NOW);
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let created = create_ok(&app, &seeded, &check_body("Refund")).await;
        let evaluator: Uuid = created["id"].as_str().unwrap().parse().unwrap();
        let old = add_experiment(&app, seeded.project.id, "old", now - DAY).await;
        let recent = add_experiment(&app, seeded.project.id, "recent", now - DAY).await;
        for (experiment, days) in [(old, 31), (recent, 29)] {
            put_score(
                &app,
                seeded.project.id,
                TestScore {
                    name: "mentions_refund",
                    value: ScoreValue::Boolean(true),
                    evaluator_id: None,
                    experiment_id: Some(experiment),
                    created_at: now - days * DAY,
                    deleted: false,
                    score_id: None,
                    updated_at: None,
                },
            )
            .await;
        }

        let response = stats(&app, &seeded, &format!("?evaluator_ids={evaluator}")).await;
        let body: EvaluatorStatsResponse = response.json();
        let experiments = &body.data[0].experiments_with_score;
        assert_eq!(experiments.total, 1);
        assert_eq!(experiments.recent[0].id, recent);
    }
}
