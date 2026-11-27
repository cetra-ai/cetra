//! Online rule routes (spec 09 § Evaluation & alerts, spec 06 § Online rules, D-I-19, D-I-30,
//! D-I-32). Project members and above write; every project reader lists and reads.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, patch, post};
use cetra_core::enums::ScoreDataType;
use cetra_core::ids::{OrgId, ProjectId};
use cetra_eval::{Adapter, OutputScore, VariableMapping};
use cetra_storage::{MAX_MATCH_COUNT, MatchCountRequest, TimeWindow, ViewName, match_count};
use serde::Deserialize;
use serde_json::Value;
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::api::common::{FieldError, PageParams, TimeParam};
use crate::api::evaluators::StatsWindow;
use crate::api::online_rules::{
    BudgetPeriod, CreateOnlineRuleRequest, JobCounts as ApiJobCounts, OnlineRule, OnlineRulePage,
    OnlineRulePreview, OnlineRulePreviewRequest, OnlineRuleStats, OnlineRuleStatsResponse,
    PauseReason, RuleJobsHour, RuleScoreStats, RuleSpend, RuleTarget, UpdateOnlineRuleRequest,
};
use crate::audit::catalog::OnlineRuleAction;
use crate::audit::{self, Event, TargetKind};
use crate::auth::principal::{self, Principal};
use crate::auth::roles::ProjectPerm;
use crate::eval::budget::{self, DAY_NS, day_start};
use crate::eval::rule_check::{RuleShape, Subject, check_rule, field_error};
use crate::eval::rule_limit::{self, RuleLimitError};
use crate::eval::stats::{self, ScoreGroup};
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::meta::MetaError;
use crate::meta::connections;
use crate::meta::eval_jobs::{self, JobCounts};
use crate::meta::evaluators::{self as evaluators, EvaluatorRow};
use crate::meta::online_rules::{self as store, NewRule, PAUSE_BUDGET, RuleFields, RuleRow};
use crate::meta::rule_spend;

use super::orgs::page_named;
use super::read::{Deadline, open_query, open_query_windowed};

/// Online rule routes. Reads need project view; writes need
/// [`ProjectPerm::ManageEvaluation`] (D-I-24). Not session-only.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/online-rules",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_online_rules),
        },
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/online-rules/stats",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_online_rule_stats),
        },
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/v1/projects/{project}/online-rules",
            access: Access::Project {
                perm: ProjectPerm::ManageEvaluation,
                session_only: false,
            },
            documented: true,
            handler: post(create_online_rule),
        },
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/v1/projects/{project}/online-rules/preview",
            access: Access::Project {
                perm: ProjectPerm::ManageEvaluation,
                session_only: false,
            },
            documented: true,
            handler: post(preview_online_rules),
        },
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/online-rules/{id}",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_online_rule),
        },
        RouteDef {
            method: axum::http::Method::PATCH,
            path: "/api/v1/projects/{project}/online-rules/{id}",
            access: Access::Project {
                perm: ProjectPerm::ManageEvaluation,
                session_only: false,
            },
            documented: true,
            handler: patch(update_online_rule),
        },
        RouteDef {
            method: axum::http::Method::DELETE,
            path: "/api/v1/projects/{project}/online-rules/{id}",
            access: Access::Project {
                perm: ProjectPerm::ManageEvaluation,
                session_only: false,
            },
            documented: true,
            handler: delete(delete_online_rule),
        },
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/v1/projects/{project}/online-rules/{id}/resume",
            access: Access::Project {
                perm: ProjectPerm::ManageEvaluation,
                session_only: false,
            },
            documented: true,
            handler: post(resume_online_rule),
        },
    ]
}

#[derive(Debug, Deserialize)]
struct IdPath {
    id: Uuid,
}

/// A rule as it would be stored: the request's values, or the stored ones merged with a PATCH.
pub(crate) struct RuleDraft {
    pub(crate) name: String,
    pub(crate) target: RuleTarget,
    /// Trimmed; `None` when blank.
    pub(crate) filter: Option<String>,
    pub(crate) mapping: BTreeMap<String, String>,
    pub(crate) sample_rate: f64,
    pub(crate) budget_usd: Option<f64>,
    pub(crate) budget_period: Option<BudgetPeriod>,
    pub(crate) enabled: bool,
}

impl RuleDraft {
    fn fields(&self) -> Result<RuleFields, ApiError> {
        Ok(RuleFields {
            name: self.name.clone(),
            filter: self.filter.clone(),
            variable_mapping: serde_json::to_string(&self.mapping).map_err(internal)?,
            sample_rate: self.sample_rate,
            budget_usd: self.budget_usd,
            budget_period: self.budget_period.map(|period| period.as_str().to_owned()),
            enabled: self.enabled,
        })
    }
}

/// What a create writes: the rule, and the evaluator row its checks ran against.
struct Insert {
    org: OrgId,
    evaluator: EvaluatorRow,
    new: NewRule,
    fields: RuleFields,
}

/// What a PATCH writes: the rule and evaluator rows its checks ran against, and the merged fields.
struct Update {
    org: OrgId,
    id: Uuid,
    rule: RuleRow,
    evaluator: EvaluatorRow,
    fields: RuleFields,
    budget_changed: bool,
}

/// What a create or PATCH transaction did.
enum RuleWrite {
    /// The row as stored. `budget_resumed` says the write cleared a budget pause (D-I-15).
    Done {
        row: Box<RuleRow>,
        budget_resumed: bool,
    },
    /// The rule is missing.
    Missing,
    /// The evaluator a create names is missing.
    EvaluatorGone,
    /// Creating or enabling the rule passes the active-rule limit (D-I-32).
    OverLimit(RuleLimitError),
}

/// What a resume transaction did.
enum Resume {
    Missing,
    /// The rule was not paused: nothing is written.
    Unchanged(Box<RuleRow>),
    Resumed(Box<RuleRow>),
}

async fn list_online_rules(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<PageParams>,
) -> Result<error::Json<OnlineRulePage>, ApiError> {
    Ok(error::Json(rule_page(&state, &access, params).await?))
}

/// One page of the project's online rules, each with its last 24 hours of jobs (`GET /online-rules`).
/// Also served to the assistant's `list_online_rules` tool (AS M2).
pub(crate) async fn rule_page(
    state: &AppState,
    access: &ProjectAccess,
    params: PageParams,
) -> Result<OnlineRulePage, ApiError> {
    let project = access.project.id;
    let now = state.clock.now_ns();
    let rows = store::list_rules(state.meta.reader(), project).await?;
    let page = page_named(
        rows.into_iter()
            .map(|row| (row.name.clone(), row.id.to_string(), row))
            .collect(),
        params.cursor.as_deref(),
        params.limit.get(),
    )?;
    let counts = eval_jobs::counts_since(state.meta.reader(), project, now - DAY_NS).await?;
    let mut data = Vec::with_capacity(page.data.len());
    for row in &page.data {
        let jobs = counts.get(&row.id).copied().unwrap_or_default();
        data.push(rule_json(state, row, &jobs, now).await?);
    }
    let mut conn = state
        .meta
        .reader()
        .acquire()
        .await
        .map_err(MetaError::from)?;
    let org_enabled_rules = store::count_enabled_rules(&mut conn, access.project.org_id).await?;
    Ok(OnlineRulePage {
        data,
        next_cursor: page.next_cursor,
        org_enabled_rules,
    })
}

/// `GET /online-rules/stats`: the last 24 hours of every online rule of the project (D-I-50).
/// Not cached: the score statement is narrowed by the rule ids and shares one deadline.
async fn get_online_rule_stats(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
) -> Result<error::Json<OnlineRuleStatsResponse>, ApiError> {
    Ok(error::Json(rule_stats(&state, &access).await?))
}

/// The last 24 hours of every online rule of the project (D-I-50). Also served to the assistant's
/// online rule tools (AS M2).
pub(crate) async fn rule_stats(
    state: &AppState,
    access: &ProjectAccess,
) -> Result<OnlineRuleStatsResponse, ApiError> {
    let deadline = Deadline::start();
    let project = access.project.id;
    let org = access.project.org_id;
    let now = state.clock.now_ns();
    let (from, to) = stats::rule_window(now);
    let window = StatsWindow { from, to };

    // Rules come in list order. Each rule's evaluator is read for its current output type.
    let rules = store::list_rules(state.meta.reader(), project).await?;
    if rules.is_empty() {
        return Ok(OnlineRuleStatsResponse {
            window,
            data: Vec::new(),
        });
    }
    let mut output_types: HashMap<Uuid, ScoreDataType> = HashMap::new();
    for row in evaluators::list_evaluators(state.meta.reader(), project).await? {
        let output = serde_json::from_str::<OutputScore>(&row.output_score)
            .map_err(|_| corrupt_in("evaluators", "output_score"))?;
        output_types.insert(row.id, output.data_type);
    }
    let data_types = rules
        .iter()
        .map(|rule| {
            output_types
                .get(&rule.evaluator_id)
                .copied()
                .ok_or_else(|| corrupt("evaluator_id"))
        })
        .collect::<Result<Vec<_>, ApiError>>()?;

    let jobs = eval_jobs::rule_finished_hourly(state.meta.reader(), project, from, to).await?;
    let scores_ctx =
        open_query_windowed(state, org, project, Some(TimeWindow::new(from, to)?)).await?;
    // At most `MAX_RESTRICT` ids per statement: the placeholders stay under the sandbox's operator
    // limit and the hourly rows (ids × 24) under its row limit, whatever the number of rules.
    let mut aggregates = HashMap::new();
    for (chunk, types) in rules
        .chunks(stats::MAX_RESTRICT)
        .zip(data_types.chunks(stats::MAX_RESTRICT))
    {
        let ids: Vec<Uuid> = chunk.iter().map(|rule| rule.id).collect();
        let has_categorical = types.contains(&ScoreDataType::Categorical);
        aggregates.extend(
            stats::read_scores(
                &scores_ctx,
                ScoreGroup::Rule,
                from,
                Some(ids.as_slice()),
                has_categorical,
                deadline,
            )
            .await?,
        );
    }

    let mut data = Vec::with_capacity(rules.len());
    for (rule, data_type) in rules.iter().zip(data_types) {
        let hours = jobs
            .get(&rule.id)
            .copied()
            .unwrap_or([eval_jobs::RuleHourJobs::default(); eval_jobs::RULE_STATS_HOURS]);
        let aggregate = aggregates.get(&rule.id);
        let summary = stats::score_summary(
            data_type,
            aggregate,
            from,
            ScoreGroup::Rule.bucket_ns(),
            ScoreGroup::Rule.buckets(),
        );
        data.push(OnlineRuleStats {
            rule_id: rule.id,
            jobs_hourly: hours
                .iter()
                .enumerate()
                .map(|(index, hour)| RuleJobsHour {
                    time: stats::bucket_time(from, eval_jobs::HOUR_NS, index),
                    done: hour.done,
                    failed: hour.failed,
                    skipped: hour.skipped,
                })
                .collect(),
            scores: RuleScoreStats {
                data_type,
                count: aggregate.map_or(0, |aggregate| aggregate.count),
                average: summary.average,
                pass_rate: summary.pass_rate,
                top_category: summary.top_category,
                hourly: summary.series,
            },
        });
    }
    Ok(OnlineRuleStatsResponse { window, data })
}

async fn get_online_rule(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
) -> Result<error::Json<OnlineRule>, ApiError> {
    Ok(error::Json(rule_detail(&state, &access, id).await?))
}

/// One online rule (`GET /online-rules/{id}`). Also served to the assistant's `get_online_rule` tool
/// (AS M2).
pub(crate) async fn rule_detail(
    state: &AppState,
    access: &ProjectAccess,
    id: Uuid,
) -> Result<OnlineRule, ApiError> {
    let project = access.project.id;
    let now = state.clock.now_ns();
    let row = store::get_rule(state.meta.reader(), project, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let counts = eval_jobs::counts_since(state.meta.reader(), project, now - DAY_NS).await?;
    let jobs = counts.get(&id).copied().unwrap_or_default();
    rule_json(state, &row, &jobs, now).await
}

async fn create_online_rule(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    ctx: audit::Ctx,
    error::Json(body): error::Json<CreateOnlineRuleRequest>,
) -> Result<(StatusCode, error::Json<OnlineRule>), ApiError> {
    let user = principal::session_user(&principal)?;
    let project = access.project.id;
    let org = access.project.org_id;
    let now = state.clock.now_ns();
    let draft = RuleDraft {
        name: body.name.trim().to_owned(),
        target: body.target,
        filter: normalize_filter(body.filter),
        mapping: body.variable_mapping,
        sample_rate: body.sample_rate,
        budget_usd: body.budget_usd,
        budget_period: body.budget_period,
        enabled: body.enabled,
    };
    // D-T-8: the price table is read and the write made under the price write lock, so a
    // price write cannot pass its guard between the two.
    let prices_write = state.ingest.prices().lock_writes().await;
    let evaluator_id = body.evaluator_id;
    let evaluator = evaluators::get_evaluator(state.meta.reader(), project, evaluator_id).await?;
    validate(&state, org, project, &draft, evaluator.as_ref(), now).await?;
    // `validate` refuses a missing evaluator, so this is the same answer if the row went meanwhile.
    let evaluator = evaluator.ok_or_else(|| bad_request(vec![evaluator_missing()]))?;
    let id = Uuid::now_v7();
    let insert = Insert {
        org,
        evaluator,
        new: NewRule {
            id,
            project_id: project,
            evaluator_id,
            target: draft.target.as_str().to_owned(),
            created_by: Some(user.id),
        },
        fields: draft.fields()?,
    };
    let limit = rule_limit::limit(&state, org);
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let out = insert_checked(conn, insert, limit, now).await?;
                if matches!(out, RuleWrite::Done { .. }) {
                    let action = OnlineRuleAction::Created { evaluator_id };
                    let event = Event::new(action)
                        .org(org)
                        .project(project)
                        .target(TargetKind::OnlineRule, id);
                    audit::record(conn, &ctx, event, now).await?;
                }
                Ok(out)
            })
        })
        .await;
    drop(prices_write);
    let outcome = outcome?;
    match outcome {
        RuleWrite::Done { row, .. } => {
            let json = rule_json(&state, &row, &JobCounts::default(), now).await?;
            Ok((StatusCode::CREATED, error::Json(json)))
        }
        RuleWrite::EvaluatorGone => Err(bad_request(vec![evaluator_missing()])),
        RuleWrite::OverLimit(error) => Err(error.into()),
        RuleWrite::Missing => Err(ApiError::NotFound),
    }
}

async fn update_online_rule(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
    ctx: audit::Ctx,
    error::Json(body): error::Json<UpdateOnlineRuleRequest>,
) -> Result<error::Json<OnlineRule>, ApiError> {
    // The request fields that were sent, by name (plan P-2): never their values.
    let mut changed_fields: Vec<&'static str> = Vec::new();
    if body.name.is_some() {
        changed_fields.push("name");
    }
    if body.filter.is_some() {
        changed_fields.push("filter");
    }
    if body.variable_mapping.is_some() {
        changed_fields.push("variable_mapping");
    }
    if body.sample_rate.is_some() {
        changed_fields.push("sample_rate");
    }
    if body.budget_usd.is_some() {
        changed_fields.push("budget_usd");
    }
    if body.budget_period.is_some() {
        changed_fields.push("budget_period");
    }
    if body.clear_budget {
        changed_fields.push("clear_budget");
    }
    if body.enabled.is_some() {
        changed_fields.push("enabled");
    }
    principal::session_user(&principal)?;
    let project = access.project.id;
    let org = access.project.org_id;
    let now = state.clock.now_ns();
    // D-T-8: the price table is read and the write made under the price write lock, so a
    // price write cannot pass its guard between the two.
    let prices_write = state.ingest.prices().lock_writes().await;
    let stored = store::get_rule(state.meta.reader(), project, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if body.clear_budget && (body.budget_usd.is_some() || body.budget_period.is_some()) {
        return Err(bad_request(vec![field_error(
            "budget_usd",
            "give budget_usd and budget_period together",
        )]));
    }
    let draft = merge(&stored, body)?;
    let evaluator = evaluators::get_evaluator(state.meta.reader(), project, stored.evaluator_id)
        .await?
        .ok_or(ApiError::Conflict {
            detail: "evaluator changed, retry",
        })?;
    validate(&state, org, project, &draft, Some(&evaluator), now).await?;
    let budget_changed = stored.budget_usd != draft.budget_usd
        || stored.budget_period.as_deref() != draft.budget_period.map(BudgetPeriod::as_str);
    let update = Update {
        org,
        id,
        rule: stored,
        evaluator,
        fields: draft.fields()?,
        budget_changed,
    };
    let limit = rule_limit::limit(&state, org);
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let out = update_checked(conn, project, update, limit, now).await?;
                if let RuleWrite::Done { budget_resumed, .. } = &out {
                    let action = OnlineRuleAction::Updated {
                        fields: changed_fields,
                        budget_resumed: *budget_resumed,
                    };
                    let event = Event::new(action)
                        .org(org)
                        .project(project)
                        .target(TargetKind::OnlineRule, id);
                    audit::record(conn, &ctx, event, now).await?;
                }
                Ok(out)
            })
        })
        .await;
    drop(prices_write);
    let outcome = outcome?;
    match outcome {
        RuleWrite::Done {
            row,
            budget_resumed,
        } => {
            if budget_changed {
                // The reservations and spend of the old budget no longer apply (D-I-15).
                state.eval.budgets.forget(id);
            }
            if budget_resumed {
                // As a manual resume: the breaker starts over and the dispatcher wakes.
                state.eval.breakers.reset(id);
                state.eval.wake();
            }
            let counts =
                eval_jobs::counts_since(state.meta.reader(), project, now - DAY_NS).await?;
            let jobs = counts.get(&id).copied().unwrap_or_default();
            Ok(error::Json(rule_json(&state, &row, &jobs, now).await?))
        }
        RuleWrite::Missing => Err(ApiError::NotFound),
        RuleWrite::OverLimit(error) => Err(error.into()),
        RuleWrite::EvaluatorGone => Err(ApiError::Conflict {
            detail: "evaluator changed, retry",
        }),
    }
}

async fn delete_online_rule(
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
    // Jobs and spend of the rule go with it (schema cascade).
    let went = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                if !store::delete_rule(conn, project, id).await? {
                    return Ok(false);
                }
                let event = Event::new(OnlineRuleAction::Deleted)
                    .org(org)
                    .project(project)
                    .target(TargetKind::OnlineRule, id);
                audit::record(conn, &ctx, event, now).await?;
                Ok(true)
            })
        })
        .await?;
    if !went {
        return Err(ApiError::NotFound);
    }
    state.eval.forget_rule(id);
    Ok(StatusCode::NO_CONTENT)
}

async fn resume_online_rule(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
    ctx: audit::Ctx,
) -> Result<error::Json<OnlineRule>, ApiError> {
    principal::session_user(&principal)?;
    let project = access.project.id;
    let org = access.project.org_id;
    let now = state.clock.now_ns();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let Some(row) = store::get_rule_tx(conn, project, id).await? else {
                    return Ok(Resume::Missing);
                };
                if row.paused_reason.is_none() {
                    return Ok(Resume::Unchanged(Box::new(row)));
                }
                Ok(match store::resume_rule(conn, project, id, now).await? {
                    Some(row) => {
                        let event = Event::new(OnlineRuleAction::Resumed { reason: "manual" })
                            .org(org)
                            .project(project)
                            .target(TargetKind::OnlineRule, id);
                        audit::record(conn, &ctx, event, now).await?;
                        Resume::Resumed(Box::new(row))
                    }
                    None => Resume::Missing,
                })
            })
        })
        .await?;
    let row = match outcome {
        Resume::Missing => return Err(ApiError::NotFound),
        Resume::Unchanged(row) => row,
        Resume::Resumed(row) => {
            state.eval.breakers.reset(id);
            state.eval.wake();
            row
        }
    };
    let counts = eval_jobs::counts_since(state.meta.reader(), project, now - DAY_NS).await?;
    let jobs = counts.get(&id).copied().unwrap_or_default();
    Ok(error::Json(rule_json(&state, &row, &jobs, now).await?))
}

/// Longest window the preview counts (D-I-9).
const MAX_PREVIEW_WINDOW_NS: i64 = 30 * DAY_NS;

/// Time allowed for the preview's count (D-I-9).
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(10);

/// `POST …/online-rules/preview`: counts what a filter matches in a window, with strict
/// exclusions (D-I-9). The count stops at 10 001 rows, so the answer is at most 10 000.
async fn preview_online_rules(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Json(body): error::Json<OnlineRulePreviewRequest>,
) -> Result<error::Json<OnlineRulePreview>, ApiError> {
    let project = access.project.id;
    let now = state.clock.now_ns();
    let to = body.to.map_or(now, TimeParam::ns);
    let from = body
        .from
        .map_or_else(|| to.saturating_sub(DAY_NS), TimeParam::ns);
    let mut errors = Vec::new();
    if from >= to {
        errors.push(field_error("from", "from must be before to"));
    } else if to.saturating_sub(from) > MAX_PREVIEW_WINDOW_NS {
        errors.push(field_error("from", "the window is at most 30 days"));
    }
    if body.filter.as_ref().is_some_and(|text| text.len() > 4096) {
        errors.push(field_error("filter", "filter must be at most 4096 bytes"));
    }
    if !errors.is_empty() {
        return Err(bad_request(errors));
    }
    let view = match body.target {
        RuleTarget::Trace => ViewName::Traces,
        RuleTarget::Span => ViewName::Spans,
    };
    let ctx = open_query(&state, access.project.org_id, project, None).await?;
    let count = match_count(
        &ctx,
        MatchCountRequest {
            view,
            filter: body.filter.as_deref(),
            window: TimeWindow::new(from, to)?,
            cap: MAX_MATCH_COUNT,
            strict_exclusions: true,
            timeout: PREVIEW_TIMEOUT,
        },
    )
    .await?;
    let capped = count > MAX_MATCH_COUNT;
    let matched = u64::try_from(count.min(MAX_MATCH_COUNT)).unwrap_or(u64::MAX);
    tracing::debug!(
        project_id = %project,
        target = body.target.as_str(),
        matched,
        capped,
        "online rule preview counted"
    );
    Ok(error::Json(OnlineRulePreview {
        matched,
        capped,
        from,
        to,
    }))
}

/// The create's write: the evaluator is read again and must be the row the checks ran against,
/// then the active-rule limit is checked before the insert (D-I-32).
async fn insert_checked(
    conn: &mut SqliteConnection,
    insert: Insert,
    limit: Option<u64>,
    now: i64,
) -> Result<RuleWrite, MetaError> {
    let project = insert.new.project_id;
    let Some(current) = evaluators::get_evaluator_tx(conn, project, insert.evaluator.id).await?
    else {
        return Ok(RuleWrite::EvaluatorGone);
    };
    if current != insert.evaluator {
        return Err(MetaError::Conflict("evaluator changed, retry"));
    }
    if insert.fields.enabled {
        let active = store::count_enabled_rules(conn, insert.org).await? + 1;
        if let Err(error) = rule_limit::check(active, limit) {
            return Ok(RuleWrite::OverLimit(error));
        }
    }
    let row = store::insert_rule(conn, insert.new, insert.fields, now).await?;
    Ok(RuleWrite::Done {
        row: Box::new(row),
        budget_resumed: false,
    })
}

/// The PATCH's write: the rule and the evaluator must be the rows the checks ran against, then
/// the active-rule limit is checked when the PATCH enables the rule. A changed budget clears a
/// budget pause in the same write (D-I-15).
async fn update_checked(
    conn: &mut SqliteConnection,
    project: ProjectId,
    update: Update,
    limit: Option<u64>,
    now: i64,
) -> Result<RuleWrite, MetaError> {
    let Some(current) = store::get_rule_tx(conn, project, update.id).await? else {
        return Ok(RuleWrite::Missing);
    };
    if current != update.rule {
        return Err(MetaError::Conflict("online rule changed, retry"));
    }
    let Some(evaluator) = evaluators::get_evaluator_tx(conn, project, update.evaluator.id).await?
    else {
        return Err(MetaError::Conflict("evaluator changed, retry"));
    };
    if evaluator != update.evaluator {
        return Err(MetaError::Conflict("evaluator changed, retry"));
    }
    if !current.enabled && update.fields.enabled {
        let active = store::count_enabled_rules(conn, update.org).await? + 1;
        if let Err(error) = rule_limit::check(active, limit) {
            return Ok(RuleWrite::OverLimit(error));
        }
    }
    let Some(row) = store::update_rule(conn, project, update.id, update.fields, now).await? else {
        return Ok(RuleWrite::Missing);
    };
    let (row, budget_resumed) =
        if update.budget_changed && row.paused_reason.as_deref() == Some(PAUSE_BUDGET) {
            match store::resume_rule(conn, project, update.id, now).await? {
                Some(resumed) => (resumed, true),
                None => (row, false),
            }
        } else {
            (row, false)
        };
    Ok(RuleWrite::Done {
        row: Box::new(row),
        budget_resumed,
    })
}

/// Runs § Rule validation for `draft` against `evaluator` (the stored row, or `None` when the
/// id names none). Every check runs and all errors come back together; the filter is checked
/// last, by storage (check 3).
pub(crate) async fn validate(
    state: &AppState,
    org: OrgId,
    project: ProjectId,
    draft: &RuleDraft,
    evaluator: Option<&EvaluatorRow>,
    now: i64,
) -> Result<(), ApiError> {
    let mut errors = Vec::new();
    let mapping = check_fields(draft, &mut errors);
    match evaluator {
        None => errors.push(evaluator_missing()),
        Some(row) => {
            let subject = Subject::from_row(row).map_err(internal)?;
            let adapter = judge_adapter(state, org, &subject).await?;
            // The checks on the mapping's keys run even when an expression does not parse.
            let variables: BTreeSet<String> = draft.mapping.keys().cloned().collect();
            let prices = state.ingest.prices().for_project(org, project);
            let shape = RuleShape {
                target: draft.target,
                mapping: mapping.as_ref(),
                variables: &variables,
                budgeted: draft.budget_usd.is_some(),
            };
            errors.extend(check_rule(&shape, &subject, adapter, &prices, now));
        }
    }
    if !errors.is_empty() {
        return Err(bad_request(errors));
    }
    if let Some(text) = &draft.filter {
        let view = match draft.target {
            RuleTarget::Trace => ViewName::Traces,
            RuleTarget::Span => ViewName::Spans,
        };
        let ctx = open_query(state, org, project, None).await?;
        cetra_storage::validate_filter(&ctx, view, text)?;
    }
    Ok(())
}

/// The checks that need no store (checks 1, 4–7, 11–13, 16). Returns the parsed mapping when it
/// parses, so the rule checks that need it can run on it.
fn check_fields(draft: &RuleDraft, errors: &mut Vec<FieldError>) -> Option<VariableMapping> {
    if !(1..=128).contains(&draft.name.chars().count()) {
        errors.push(field_error("name", "name must be 1 to 128 characters"));
    }
    if draft.filter.as_ref().is_some_and(|text| text.len() > 4096) {
        errors.push(field_error("filter", "filter must be at most 4096 bytes"));
    }
    let mut mapping_ok = true;
    if !(1..=32).contains(&draft.mapping.len()) {
        errors.push(field_error("variable_mapping", "map 1 to 32 variables"));
        mapping_ok = false;
    }
    for (name, expression) in &draft.mapping {
        if expression.len() > 1024 {
            errors.push(field_error(
                &format!("variable_mapping.{name}"),
                "expression must be at most 1024 bytes",
            ));
            mapping_ok = false;
        }
    }
    if !(draft.sample_rate.is_finite() && draft.sample_rate > 0.0 && draft.sample_rate <= 1.0) {
        errors.push(field_error(
            "sample_rate",
            "sample_rate must be above 0 and at most 1",
        ));
    }
    if draft.budget_usd.is_some() != draft.budget_period.is_some() {
        errors.push(field_error(
            "budget_usd",
            "give budget_usd and budget_period together",
        ));
    }
    if let Some(budget) = draft.budget_usd
        && !(budget.is_finite() && budget > 0.0 && budget <= 1_000_000.0)
    {
        errors.push(field_error(
            "budget_usd",
            "budget_usd must be above 0 and at most 1000000",
        ));
    }
    if !mapping_ok {
        return None;
    }
    let value = Value::Object(
        draft
            .mapping
            .iter()
            .map(|(name, expression)| (name.clone(), Value::String(expression.clone())))
            .collect(),
    );
    match VariableMapping::from_json(&value) {
        Ok(mapping) => {
            if mapping.needs_item() {
                errors.push(field_error(
                    "variable_mapping",
                    "expected and item.* are for experiments only",
                ));
            }
            Some(mapping)
        }
        Err(error) => {
            errors.push(field_error("variable_mapping", &error.to_string()));
            None
        }
    }
}

/// The adapter of a judge's connection, `None` for other kinds or when the connection is gone.
pub(crate) async fn judge_adapter(
    state: &AppState,
    org: OrgId,
    subject: &Subject,
) -> Result<Option<Adapter>, ApiError> {
    let Subject::Judge(judge) = subject else {
        return Ok(None);
    };
    let Some(connection) =
        connections::get_connection(state.meta.reader(), org, judge.connection_id).await?
    else {
        return Ok(None);
    };
    Adapter::from_name(&connection.adapter)
        .map(Some)
        .map_err(|_| corrupt_in("llm_connections", "adapter"))
}

/// The rule a PATCH would store: the body's values over the stored ones. `filter: ""` clears
/// the filter; `clear_budget` removes the budget.
fn merge(stored: &RuleRow, body: UpdateOnlineRuleRequest) -> Result<RuleDraft, ApiError> {
    let target = RuleTarget::from_name(&stored.target).ok_or_else(|| corrupt("target"))?;
    let stored_mapping: BTreeMap<String, String> =
        serde_json::from_str(&stored.variable_mapping).map_err(|_| corrupt("variable_mapping"))?;
    let stored_period = stored
        .budget_period
        .as_deref()
        .map(|text| BudgetPeriod::from_name(text).ok_or_else(|| corrupt("budget_period")))
        .transpose()?;
    let (budget_usd, budget_period) = if body.clear_budget {
        (None, None)
    } else {
        (
            body.budget_usd.or(stored.budget_usd),
            body.budget_period.or(stored_period),
        )
    };
    Ok(RuleDraft {
        name: body
            .name
            .map_or_else(|| stored.name.clone(), |name| name.trim().to_owned()),
        target,
        filter: match body.filter {
            Some(text) => normalize_filter(Some(text)),
            None => stored.filter.clone(),
        },
        mapping: body.variable_mapping.unwrap_or(stored_mapping),
        sample_rate: body.sample_rate.unwrap_or(stored.sample_rate),
        budget_usd,
        budget_period,
        enabled: body.enabled.unwrap_or(stored.enabled),
    })
}

/// The filter as stored: trimmed, and absent when blank.
pub(crate) fn normalize_filter(text: Option<String>) -> Option<String> {
    text.map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// The rule as the API shows it. `spend` is read from the day rows, and the reservations and
/// the hourly drops come from memory.
async fn rule_json(
    state: &AppState,
    row: &RuleRow,
    jobs: &JobCounts,
    now: i64,
) -> Result<OnlineRule, ApiError> {
    let target = RuleTarget::from_name(&row.target).ok_or_else(|| corrupt("target"))?;
    let variable_mapping: BTreeMap<String, String> =
        serde_json::from_str(&row.variable_mapping).map_err(|_| corrupt("variable_mapping"))?;
    let budget_period = row
        .budget_period
        .as_deref()
        .map(|text| BudgetPeriod::from_name(text).ok_or_else(|| corrupt("budget_period")))
        .transpose()?;
    let paused_reason = row
        .paused_reason
        .as_deref()
        .map(|text| PauseReason::from_name(text).ok_or_else(|| corrupt("paused_reason")))
        .transpose()?;
    let spend = match (row.budget_usd, budget_period) {
        (Some(_), Some(period)) => {
            let period_start = budget::period_start(period, now);
            let spent_usd = rule_spend::spent_between(
                state.meta.reader(),
                row.id,
                period_start,
                day_start(now) + DAY_NS,
            )
            .await?;
            let reserved_usd = state
                .eval
                .budgets
                .view(row.id, period_start)
                .map_or(0.0, |(_, reserved)| reserved);
            Some(RuleSpend {
                period_start,
                spent_usd,
                reserved_usd,
            })
        }
        _ => None,
    };
    Ok(OnlineRule {
        id: row.id,
        name: row.name.clone(),
        evaluator_id: row.evaluator_id,
        target,
        filter: row.filter.clone(),
        variable_mapping,
        sample_rate: row.sample_rate,
        budget_usd: row.budget_usd,
        budget_period,
        enabled: row.enabled,
        paused_reason,
        paused_at: row.paused_at,
        paused_detail: row.paused_detail.clone(),
        spend,
        jobs_last_24h: ApiJobCounts {
            queued: jobs.queued,
            running: jobs.running,
            done: jobs.done,
            failed: jobs.failed,
            skipped: jobs.skipped,
        },
        dropped_last_hour: state.eval.hourly.dropped(row.id, now),
        created_by: row.created_by,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

fn evaluator_missing() -> FieldError {
    field_error("evaluator_id", "evaluator not found in this project")
}

fn bad_request(errors: Vec<FieldError>) -> ApiError {
    ApiError::BadRequest {
        detail: None,
        errors,
    }
}

fn corrupt(column: &'static str) -> ApiError {
    corrupt_in("online_rules", column)
}

fn corrupt_in(table: &'static str, column: &'static str) -> ApiError {
    ApiError::Internal(Box::new(MetaError::Corrupt { table, column }))
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> ApiError {
    ApiError::Internal(Box::new(error))
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, header};
    use cetra_core::enums::SpanStatus;
    use cetra_core::ids::{SpanId, TraceId};
    use cetra_core::span::Span;
    use cetra_otel::{PriceRow, PriceScope, exact_pattern};
    use serde_json::{Value, json};

    use super::*;
    use crate::api::evaluators::CategoryCount;
    use crate::api::online_rules::{OnlineRuleStats, OnlineRuleStatsResponse};
    use crate::auth::roles::OrgRole;
    use crate::meta::connections::{ConnectionFields, insert_connection};
    use crate::meta::eval_jobs::RULE_STATS_HOURS;
    use crate::meta::eval_jobs::{KIND_RULE, NewJob};
    use crate::meta::evaluators::{EvaluatorFields, insert_evaluator};
    use crate::meta::model_prices;
    use crate::meta::online_rules::{PAUSE_BUDGET, PAUSE_ERRORS, pause_rule};
    use crate::meta::rule_spend::add_spend;
    use crate::test_support::{Seeded, TestApp, TestResponse, dataset_span, set_limits};
    use cetra_core::batch::scores_to_batch;
    use cetra_core::enums::ScoreSource;
    use cetra_core::score::{Score, ScoreTarget, ScoreValue};

    /// The judge's prompt maps `input` and `output`.
    const PROMPT: &str = "Question: {{ input }}\nAnswer: {{ output }}";

    fn path(project: ProjectId) -> String {
        format!("/api/v1/projects/{project}/online-rules")
    }

    fn judge_fields(connection: Uuid, model: &str) -> EvaluatorFields {
        EvaluatorFields {
            name: format!("judge {connection}"),
            config: json!({
                "connection_id": connection,
                "model": model,
                "prompt": PROMPT,
                "output": {"type": "boolean"},
                "include_reason": false,
                "temperature": null,
                "max_output_tokens": 1024,
                "timeout_ms": 60000
            })
            .to_string(),
            output_score: json!({"name": "relevance", "data_type": "boolean"}).to_string(),
            connection_id: Some(connection),
            secret_enc: None,
            version: 1,
        }
    }

    fn connection_fields(name: String) -> ConnectionFields {
        ConnectionFields {
            name,
            adapter: "openai".to_owned(),
            base_url: None,
            secret_enc: vec![1],
            api_key_hint: "WXYZ".to_owned(),
            extra_headers_enc: None,
            extra_header_names: Vec::new(),
            default_model: None,
            structured_output: "native".to_owned(),
            max_concurrency: 4,
            requests_per_minute: 300,
        }
    }

    /// A connection and a judge on it with `model`, in `seeded`'s project. Returns the judge id.
    async fn judge_with_model(app: &TestApp, seeded: &Seeded, model: &'static str) -> Uuid {
        let (org, project, now) = (seeded.org_id, seeded.project.id, app.state.clock.now_ns());
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let connection = Uuid::now_v7();
                    insert_connection(
                        conn,
                        connection,
                        org,
                        connection_fields(format!("judge {connection}")),
                        None,
                        now,
                    )
                    .await?;
                    let judge = Uuid::now_v7();
                    insert_evaluator(
                        conn,
                        judge,
                        project,
                        "llm_judge",
                        judge_fields(connection, model),
                        None,
                        now,
                    )
                    .await?;
                    Ok(judge)
                })
            })
            .await
            .unwrap()
    }

    async fn judge(app: &TestApp, seeded: &Seeded) -> Uuid {
        judge_with_model(app, seeded, "gpt-5-mini").await
    }

    /// A check evaluator (`json_valid`) in `seeded`'s project.
    async fn check(app: &TestApp, seeded: &Seeded) -> Uuid {
        let (project, now) = (seeded.project.id, app.state.clock.now_ns());
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let id = Uuid::now_v7();
                    insert_evaluator(
                        conn,
                        id,
                        project,
                        "check",
                        EvaluatorFields {
                            name: "json".to_owned(),
                            config: json!({"check": "json_valid"}).to_string(),
                            output_score: json!({"name": "json", "data_type": "boolean"})
                                .to_string(),
                            connection_id: None,
                            secret_enc: None,
                            version: 1,
                        },
                        None,
                        now,
                    )
                    .await?;
                    Ok(id)
                })
            })
            .await
            .unwrap()
    }

    /// A webhook evaluator whose sealed values are garbage: a rule on it never opens them.
    async fn webhook_with_garbage_secret(app: &TestApp, seeded: &Seeded) -> Uuid {
        let (project, now) = (seeded.project.id, app.state.clock.now_ns());
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let id = Uuid::now_v7();
                    insert_evaluator(
                        conn,
                        id,
                        project,
                        "webhook",
                        EvaluatorFields {
                            name: "hook".to_owned(),
                            config: json!({
                                "url_host": "hooks.example.com",
                                "timeout_ms": 10000,
                                "header_names": []
                            })
                            .to_string(),
                            output_score: json!({"name": "hook", "data_type": "numeric"})
                                .to_string(),
                            connection_id: None,
                            secret_enc: Some(vec![0xde, 0xad, 0xbe, 0xef]),
                            version: 1,
                        },
                        None,
                        now,
                    )
                    .await?;
                    Ok(id)
                })
            })
            .await
            .unwrap()
    }

    fn rule_body(evaluator: Uuid) -> Value {
        json!({
            "name": "Relevance on answers",
            "evaluator_id": evaluator,
            "target": "trace",
            "filter": "status = 'ok'",
            "variable_mapping": {"input": "trace.input", "output": "trace.output"},
            "sample_rate": 0.25,
            "budget_usd": 5.0,
            "budget_period": "day",
            "enabled": true
        })
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

    async fn patch(app: &TestApp, seeded: &Seeded, id: Uuid, body: &Value) -> TestResponse {
        app.request(Method::PATCH, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(body)
            .send()
            .await
    }

    fn text(response: &TestResponse) -> String {
        String::from_utf8_lossy(&response.body).into_owned()
    }

    fn id_of(value: &Value) -> Uuid {
        value["id"].as_str().unwrap().parse().unwrap()
    }

    /// The `(field, message)` pairs of a 400 answer.
    fn field_errors(response: &TestResponse) -> Vec<(String, String)> {
        assert_eq!(response.status, 400, "{}", text(response));
        response
            .problem()
            .errors
            .into_iter()
            .map(|error| (error.field, error.message))
            .collect()
    }

    /// The stored pause reason of rule `id`.
    async fn paused_reason(app: &TestApp, id: Uuid) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT paused_reason FROM online_rules WHERE id = ?",
        )
        .bind(id.to_string())
        .fetch_one(app.state.meta.reader())
        .await
        .unwrap()
    }

    /// A rule written straight to the store (its checks are the write's, not the route's).
    async fn insert_rule_row(
        app: &TestApp,
        seeded: &Seeded,
        evaluator: Uuid,
        name: &str,
    ) -> RuleRow {
        let (project, now) = (seeded.project.id, app.state.clock.now_ns());
        let name = name.to_owned();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    store::insert_rule(
                        conn,
                        NewRule {
                            id: Uuid::now_v7(),
                            project_id: project,
                            evaluator_id: evaluator,
                            target: "trace".to_owned(),
                            created_by: None,
                        },
                        rule_fields(&name, true),
                        now,
                    )
                    .await
                })
            })
            .await
            .unwrap()
    }

    fn rule_fields(name: &str, enabled: bool) -> RuleFields {
        RuleFields {
            name: name.to_owned(),
            filter: None,
            variable_mapping: r#"{"input":"trace.input","output":"trace.output"}"#.to_owned(),
            sample_rate: 1.0,
            budget_usd: None,
            budget_period: None,
            enabled,
        }
    }

    #[tokio::test]
    async fn create_get_list_round_trip() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &rule_body(evaluator)).await;
        assert_eq!(created["target"], "trace");
        assert_eq!(created["filter"], "status = 'ok'");
        assert_eq!(
            created["variable_mapping"],
            json!({"input": "trace.input", "output": "trace.output"})
        );
        assert_eq!(created["budget_usd"], 5.0);
        assert_eq!(created["budget_period"], "day");
        assert_eq!(created["jobs_last_24h"]["queued"], 0);
        assert_eq!(created["jobs_last_24h"]["done"], 0);
        assert_eq!(created["dropped_last_hour"], 0);
        let period_start = created["spend"]["period_start"].as_str().unwrap();
        assert!(
            period_start.ends_with("T00:00:00.000000000Z"),
            "the start of a UTC day: {period_start}"
        );
        assert_eq!(created["spend"]["spent_usd"], 0.0);
        assert_eq!(created["spend"]["reserved_usd"], 0.0);
        assert!(created.get("paused_reason").is_none());

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
        assert_eq!(page["org_enabled_rules"], 1);
        assert!(page["next_cursor"].is_null());
    }

    /// D-T-8: a write waits while a price write holds the lock, so the price table it checks
    /// against is read after that price write has committed.
    #[tokio::test]
    async fn a_create_waits_for_a_price_write() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge(&app, &seeded).await;
        let guard = app.state.ingest.prices().lock_writes().await;
        let rule = rule_body(evaluator);
        let mut pending = std::pin::pin!(create(&app, &seeded, &rule));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), pending.as_mut())
                .await
                .is_err(),
            "the create did not wait for the price write"
        );
        drop(guard);
        let response = pending.await;
        assert_eq!(response.status, 201, "{}", text(&response));
    }

    #[tokio::test]
    async fn a_patch_waits_for_a_price_write() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &rule_body(evaluator)).await;
        let id = id_of(&created);
        let guard = app.state.ingest.prices().lock_writes().await;
        let rename = json!({"name": "Renamed"});
        let mut pending = std::pin::pin!(patch(&app, &seeded, id, &rename));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), pending.as_mut())
                .await
                .is_err(),
            "the patch did not wait for the price write"
        );
        drop(guard);
        let response = pending.await;
        assert_eq!(response.status, 200, "{}", text(&response));
    }

    #[tokio::test]
    async fn create_validates_every_field() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge(&app, &seeded).await;
        let checkish = check(&app, &seeded).await;
        let missing = Uuid::now_v7();
        let long_name = "n".repeat(129);
        let long_expression = format!("trace.input{}", " ".repeat(1024));
        let too_many: BTreeMap<String, String> = (0..33)
            .map(|index| (format!("v{index}"), "trace.input".to_owned()))
            .collect();
        let cases: Vec<(Value, &str, &str)> = vec![
            (
                json!({"name": "   "}),
                "name",
                "name must be 1 to 128 characters",
            ),
            (
                json!({"name": long_name}),
                "name",
                "name must be 1 to 128 characters",
            ),
            (
                json!({"evaluator_id": missing}),
                "evaluator_id",
                "evaluator not found in this project",
            ),
            (
                json!({"filter": "x".repeat(4097)}),
                "filter",
                "filter must be at most 4096 bytes",
            ),
            (
                json!({"variable_mapping": {}}),
                "variable_mapping",
                "map 1 to 32 variables",
            ),
            (
                json!({"variable_mapping": too_many}),
                "variable_mapping",
                "map 1 to 32 variables",
            ),
            (
                json!({"variable_mapping": {"input": long_expression, "output": "trace.output"}}),
                "variable_mapping.input",
                "expression must be at most 1024 bytes",
            ),
            (
                json!({"variable_mapping": {"input": "expected", "output": "trace.output"}}),
                "variable_mapping",
                "expected and item.* are for experiments only",
            ),
            (
                json!({"variable_mapping": {"input": "span.input", "output": "trace.output"}}),
                "variable_mapping",
                "span.* needs a span rule; use a selector such as span[name=…]",
            ),
            (
                json!({"variable_mapping": {"output": "trace.output"}}),
                "variable_mapping.input",
                "variable input is used by the prompt but not mapped",
            ),
            (
                json!({"sample_rate": 0}),
                "sample_rate",
                "sample_rate must be above 0 and at most 1",
            ),
            (
                json!({"sample_rate": 1.5}),
                "sample_rate",
                "sample_rate must be above 0 and at most 1",
            ),
            (
                json!({"budget_usd": 2.0}),
                "budget_usd",
                "give budget_usd and budget_period together",
            ),
            (
                json!({"budget_usd": 0.0}),
                "budget_usd",
                "budget_usd must be above 0 and at most 1000000",
            ),
            (
                json!({"budget_usd": 1_000_001.0}),
                "budget_usd",
                "budget_usd must be above 0 and at most 1000000",
            ),
        ];
        for (overrides, field, message) in cases {
            let mut body = rule_body(evaluator);
            body.as_object_mut().unwrap().remove("filter");
            body.as_object_mut().unwrap().remove("budget_usd");
            body.as_object_mut().unwrap().remove("budget_period");
            for (key, value) in overrides.as_object().unwrap() {
                body[key] = value.clone();
            }
            let errors = field_errors(&create(&app, &seeded, &body).await);
            assert!(
                errors.contains(&(field.to_owned(), message.to_owned())),
                "{field}: {errors:?}"
            );
        }

        // A budget on a check, and a judge whose model has no price (checks 14, 15).
        let mut body = rule_body(checkish);
        body["variable_mapping"] = json!({"output": "trace.output"});
        body["budget_usd"] = json!(2.0);
        assert!(
            field_errors(&create(&app, &seeded, &body).await).contains(&(
                "budget_usd".to_owned(),
                "budgets apply to LLM judge evaluators only".to_owned()
            ))
        );
        let unpriced = judge_with_model(&app, &seeded, "no-such-model-0").await;
        let body = rule_body(unpriced);
        assert!(
            field_errors(&create(&app, &seeded, &body).await).contains(&(
                "budget_usd".to_owned(),
                "the judge model has no price, so the rule cannot have a budget; add one under Settings → Model prices".to_owned()
            ))
        );

        // A check that compares needs `output` and `expected` (§ Rule validation, checks 8 and 14).
        let exact = app
            .request(
                Method::POST,
                &format!("/api/v1/projects/{}/evaluators", seeded.project.id),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&json!({
                "name": "exact",
                "kind": "check",
                "config": {"check": "exact_match"},
                "output_score": {"name": "same", "data_type": "boolean"}
            }))
            .send()
            .await;
        assert_eq!(exact.status, 201, "{}", text(&exact));
        let exact = id_of(&exact.json::<Value>());
        let mut body = rule_body(exact);
        body.as_object_mut().unwrap().remove("budget_usd");
        body.as_object_mut().unwrap().remove("budget_period");
        body["variable_mapping"] = json!({"input": "trace.input"});
        let errors = field_errors(&create(&app, &seeded, &body).await);
        assert!(
            errors.contains(&(
                "variable_mapping.output".to_owned(),
                "the check needs the output variable".to_owned()
            )),
            "{errors:?}"
        );
        assert!(
            errors.contains(&(
                "variable_mapping.expected".to_owned(),
                "the check needs the expected variable".to_owned()
            )),
            "{errors:?}"
        );
    }

    #[tokio::test]
    async fn a_long_expression_does_not_hide_the_other_errors() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge(&app, &seeded).await;
        let mut body = rule_body(evaluator);
        body.as_object_mut().unwrap().remove("budget_usd");
        body.as_object_mut().unwrap().remove("budget_period");
        body["variable_mapping"] = json!({"input": "x".repeat(1025)});
        let errors = field_errors(&create(&app, &seeded, &body).await);
        assert!(
            errors.contains(&(
                "variable_mapping.input".to_owned(),
                "expression must be at most 1024 bytes".to_owned()
            )),
            "{errors:?}"
        );
        assert!(
            errors.contains(&(
                "variable_mapping.output".to_owned(),
                "variable output is used by the prompt but not mapped".to_owned()
            )),
            "{errors:?}"
        );
    }

    #[tokio::test]
    async fn a_duplicate_name_is_409_and_the_501st_rule_is_409() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge(&app, &seeded).await;
        create_ok(&app, &seeded, &rule_body(evaluator)).await;
        let duplicate = create(&app, &seeded, &rule_body(evaluator)).await;
        assert_eq!(duplicate.status, 409, "{}", text(&duplicate));
        assert_eq!(
            duplicate.problem().detail.as_deref(),
            Some("an online rule with this name already exists")
        );

        let project = seeded.project.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    for index in 0..499 {
                        store::insert_rule(
                            conn,
                            NewRule {
                                id: Uuid::now_v7(),
                                project_id: project,
                                evaluator_id: evaluator,
                                target: "trace".to_owned(),
                                created_by: None,
                            },
                            rule_fields(&format!("filler {index}"), false),
                            1,
                        )
                        .await?;
                    }
                    Ok(())
                })
            })
            .await
            .unwrap();
        let mut body = rule_body(evaluator);
        body["name"] = json!("the 501st");
        let over = create(&app, &seeded, &body).await;
        assert_eq!(over.status, 409, "{}", text(&over));
        assert_eq!(
            over.problem().detail.as_deref(),
            Some("a project has at most 500 online rules")
        );
    }

    #[tokio::test]
    async fn a_budget_needs_its_period_and_clear_budget_takes_neither() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge(&app, &seeded).await;
        let mut body = rule_body(evaluator);
        body.as_object_mut().unwrap().remove("budget_period");
        assert_eq!(
            field_errors(&create(&app, &seeded, &body).await),
            [(
                "budget_usd".to_owned(),
                "give budget_usd and budget_period together".to_owned()
            )]
        );

        let created = create_ok(&app, &seeded, &rule_body(evaluator)).await;
        let id = id_of(&created);
        let response = patch(
            &app,
            &seeded,
            id,
            &json!({"clear_budget": true, "budget_usd": 2.0}),
        )
        .await;
        assert_eq!(
            field_errors(&response),
            [(
                "budget_usd".to_owned(),
                "give budget_usd and budget_period together".to_owned()
            )]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_budget_on_a_local_model_is_accepted_once_the_project_prices_it() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge_with_model(&app, &seeded, "llama3.1:8b").await;
        let body = rule_body(evaluator);
        assert_eq!(
            field_errors(&create(&app, &seeded, &body).await),
            [(
                "budget_usd".to_owned(),
                "the judge model has no price, so the rule cannot have a budget; add one under Settings → Model prices".to_owned()
            )]
        );

        let (org, project, now) = (seeded.org_id, seeded.project.id, app.state.clock.now_ns());
        let row = PriceRow {
            scope: PriceScope::Project,
            model_name: "llama3.1:8b".to_owned(),
            match_pattern: exact_pattern("llama3.1:8b"),
            provider: None,
            valid_from: 0,
            price_input: Some(0.0),
            price_output: Some(0.0),
            price_cache_read: None,
            price_cache_write: None,
            price_reasoning: None,
            tiers: None,
        };
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    model_prices::insert_tx(conn, Uuid::now_v7(), org, Some(project), &row, now)
                        .await
                        .map(|_| ())
                })
            })
            .await
            .unwrap();
        app.state
            .ingest
            .prices()
            .reload_org(app.state.meta.reader(), org)
            .await
            .unwrap();
        create_ok(&app, &seeded, &body).await;
    }

    #[tokio::test]
    async fn a_deleted_evaluator_at_write_time_is_refused_by_the_write() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge(&app, &seeded).await;
        let row = evaluators::get_evaluator(app.state.meta.reader(), seeded.project.id, evaluator)
            .await
            .unwrap()
            .unwrap();
        let (org, project) = (seeded.org_id, seeded.project.id);
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    evaluators::delete_evaluator(conn, project, evaluator).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let outcome = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let insert = Insert {
                        org,
                        evaluator: row,
                        new: NewRule {
                            id: Uuid::now_v7(),
                            project_id: project,
                            evaluator_id: evaluator,
                            target: "trace".to_owned(),
                            created_by: None,
                        },
                        fields: rule_fields("late", true),
                    };
                    insert_checked(conn, insert, None, 1).await
                })
            })
            .await
            .unwrap();
        assert!(matches!(outcome, RuleWrite::EvaluatorGone));
    }

    #[tokio::test]
    async fn a_changed_evaluator_at_write_time_is_409() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge(&app, &seeded).await;
        let mut row =
            evaluators::get_evaluator(app.state.meta.reader(), seeded.project.id, evaluator)
                .await
                .unwrap()
                .unwrap();
        row.version -= 1;
        let (org, project) = (seeded.org_id, seeded.project.id);
        let error = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let insert = Insert {
                        org,
                        evaluator: row,
                        new: NewRule {
                            id: Uuid::now_v7(),
                            project_id: project,
                            evaluator_id: evaluator,
                            target: "trace".to_owned(),
                            created_by: None,
                        },
                        fields: rule_fields("stale", true),
                    };
                    insert_checked(conn, insert, None, 1).await.map(|_| ())
                })
            })
            .await
            .unwrap_err();
        assert!(
            matches!(error, MetaError::Conflict("evaluator changed, retry")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_webhook_rule_is_saved_without_opening_its_secret() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let hook = webhook_with_garbage_secret(&app, &seeded).await;
        let mut body = rule_body(hook);
        body["name"] = json!("On the hook");
        body.as_object_mut().unwrap().remove("budget_usd");
        body.as_object_mut().unwrap().remove("budget_period");
        create_ok(&app, &seeded, &body).await;
    }

    #[tokio::test]
    async fn invalid_filter_is_400_with_position_and_spans_check_the_spans_view() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge(&app, &seeded).await;
        let mut body = rule_body(evaluator);
        body["filter"] = json!("error_count >");
        let response = create(&app, &seeded, &body).await;
        assert_eq!(response.status, 400, "{}", text(&response));
        let problem = response.problem();
        assert_eq!(problem.code, crate::api::common::ProblemCode::InvalidFilter);
        assert_eq!(problem.errors[0].field, "filter");
        assert!(problem.errors[0].position.is_some());

        // `span_kind` is a span column: accepted on a span rule, refused on a trace rule.
        let mut span_rule = rule_body(evaluator);
        span_rule["target"] = json!("span");
        span_rule["name"] = json!("On spans");
        span_rule["filter"] = json!("span_kind = 'llm'");
        span_rule["variable_mapping"] = json!({"input": "span.input", "output": "span.output"});
        create_ok(&app, &seeded, &span_rule).await;
        let mut trace_rule = rule_body(evaluator);
        trace_rule["name"] = json!("On traces");
        trace_rule["filter"] = json!("span_kind = 'llm'");
        let refused = create(&app, &seeded, &trace_rule).await;
        assert_eq!(refused.status, 400, "{}", text(&refused));
        assert_eq!(
            refused.problem().code,
            crate::api::common::ProblemCode::InvalidFilter
        );
    }

    #[tokio::test]
    async fn patch_refuses_target_and_evaluator_id() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &rule_body(evaluator)).await;
        let id = id_of(&created);
        let target = patch(&app, &seeded, id, &json!({"target": "span"})).await;
        assert_eq!(target.status, 400, "{}", text(&target));
        let moved = patch(&app, &seeded, id, &json!({"evaluator_id": Uuid::now_v7()})).await;
        assert_eq!(moved.status, 400, "{}", text(&moved));
    }

    #[tokio::test]
    async fn patch_merges_and_clears_the_filter_and_clear_budget_removes_the_budget() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &rule_body(evaluator)).await;
        let id = id_of(&created);
        let renamed = patch(&app, &seeded, id, &json!({"name": "Renamed"})).await;
        assert_eq!(renamed.status, 200, "{}", text(&renamed));
        let renamed: Value = renamed.json();
        assert_eq!(renamed["name"], "Renamed");
        assert_eq!(renamed["filter"], "status = 'ok'");
        assert_eq!(renamed["budget_usd"], 5.0);

        let cleared = patch(&app, &seeded, id, &json!({"filter": ""})).await;
        assert_eq!(cleared.status, 200, "{}", text(&cleared));
        assert!(cleared.json::<Value>().get("filter").is_none());

        let unbudgeted = patch(&app, &seeded, id, &json!({"clear_budget": true})).await;
        assert_eq!(unbudgeted.status, 200, "{}", text(&unbudgeted));
        let unbudgeted: Value = unbudgeted.json();
        assert!(unbudgeted.get("budget_usd").is_none());
        assert!(unbudgeted.get("budget_period").is_none());
        assert!(unbudgeted.get("spend").is_none());
    }

    #[tokio::test]
    async fn patch_of_a_changed_row_is_409() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge(&app, &seeded).await;
        let stored = insert_rule_row(&app, &seeded, evaluator, "changing").await;
        let current =
            evaluators::get_evaluator(app.state.meta.reader(), seeded.project.id, evaluator)
                .await
                .unwrap()
                .unwrap();
        let (org, project, id) = (seeded.org_id, seeded.project.id, stored.id);
        // Another write pauses the rule between the PATCH's read and its write.
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    pause_rule(conn, id, PAUSE_ERRORS, Some("breaker"), 5).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        let update = Update {
            org,
            id,
            rule: stored,
            evaluator: current,
            fields: rule_fields("changing", true),
            budget_changed: false,
        };
        let error = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    update_checked(conn, project, update, None, 6)
                        .await
                        .map(|_| ())
                })
            })
            .await
            .unwrap_err();
        assert!(
            matches!(error, MetaError::Conflict("online rule changed, retry")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_plan_with_one_active_rule_refuses_a_second_enabled_rule_with_403() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2025-12-01T00:00:00Z",
            &json!({"max_active_online_rules": 1}),
        );
        let evaluator = judge(&app, &seeded).await;
        create_ok(&app, &seeded, &rule_body(evaluator)).await;

        let mut second = rule_body(evaluator);
        second["name"] = json!("the second rule");
        let refused = create(&app, &seeded, &second).await;
        assert_eq!(refused.status, 403, "{}", text(&refused));
        let problem = refused.problem();
        assert_eq!(problem.code, crate::api::common::ProblemCode::PlanLimit);
        assert_eq!(
            problem.detail.as_deref(),
            Some("Your plan allows 1 active online rule.")
        );
    }

    #[tokio::test]
    async fn the_active_rule_limit_refuses_create_and_enable_and_counts_paused_rules() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge(&app, &seeded).await;
        let row = evaluators::get_evaluator(app.state.meta.reader(), seeded.project.id, evaluator)
            .await
            .unwrap()
            .unwrap();
        let (org, project) = (seeded.org_id, seeded.project.id);
        let first = Uuid::now_v7();
        let row_for_first = row.clone();
        let outcome = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_checked(
                        conn,
                        Insert {
                            org,
                            evaluator: row_for_first,
                            new: NewRule {
                                id: first,
                                project_id: project,
                                evaluator_id: evaluator,
                                target: "trace".to_owned(),
                                created_by: None,
                            },
                            fields: rule_fields("first", true),
                        },
                        Some(1),
                        1,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        assert!(matches!(outcome, RuleWrite::Done { .. }));
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    pause_rule(conn, first, PAUSE_ERRORS, None, 2)
                        .await
                        .map(|_| ())
                })
            })
            .await
            .unwrap();

        // A paused rule still counts: a second enabled rule is over the limit.
        let second = app
            .state
            .meta
            .write({
                let row = row.clone();
                move |conn| {
                    Box::pin(async move {
                        insert_checked(
                            conn,
                            Insert {
                                org,
                                evaluator: row,
                                new: NewRule {
                                    id: Uuid::now_v7(),
                                    project_id: project,
                                    evaluator_id: evaluator,
                                    target: "trace".to_owned(),
                                    created_by: None,
                                },
                                fields: rule_fields("second", true),
                            },
                            Some(1),
                            3,
                        )
                        .await
                    })
                }
            })
            .await
            .unwrap();
        let RuleWrite::OverLimit(error) = second else {
            panic!("the second enabled rule is over the limit");
        };
        let problem = ApiError::from(error).problem();
        assert_eq!(problem.status, 403);
        assert_eq!(problem.code, crate::api::common::ProblemCode::PlanLimit);
        assert_eq!(
            problem.detail.as_deref(),
            Some("Your plan allows 1 active online rule.")
        );

        // A disabled rule passes at the limit; enabling it does not.
        let disabled = app
            .state
            .meta
            .write({
                let row = row.clone();
                move |conn| {
                    Box::pin(async move {
                        insert_checked(
                            conn,
                            Insert {
                                org,
                                evaluator: row,
                                new: NewRule {
                                    id: Uuid::now_v7(),
                                    project_id: project,
                                    evaluator_id: evaluator,
                                    target: "trace".to_owned(),
                                    created_by: None,
                                },
                                fields: rule_fields("disabled", false),
                            },
                            Some(1),
                            4,
                        )
                        .await
                    })
                }
            })
            .await
            .unwrap();
        let RuleWrite::Done { row: disabled, .. } = disabled else {
            panic!("a disabled rule passes the limit");
        };
        let disabled_id = disabled.id;
        let current = evaluators::get_evaluator(app.state.meta.reader(), project, evaluator)
            .await
            .unwrap()
            .unwrap();
        let update = Update {
            org,
            id: disabled_id,
            rule: *disabled,
            evaluator: current,
            fields: rule_fields("disabled", true),
            budget_changed: false,
        };
        let enabled = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move { update_checked(conn, project, update, Some(1), 5).await })
            })
            .await
            .unwrap();
        assert!(matches!(enabled, RuleWrite::OverLimit(_)));
    }

    #[tokio::test]
    async fn resume_clears_the_pause_and_resets_the_breaker() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &rule_body(evaluator)).await;
        let id = id_of(&created);
        let project = seeded.project.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    pause_rule(conn, id, PAUSE_ERRORS, Some("x"), 5)
                        .await
                        .map(|_| ())
                })
            })
            .await
            .unwrap();
        // Nineteen failures are kept in the window; the resume must start it over.
        for _ in 0..19 {
            assert!(!app.state.eval.breakers.record(id, true));
        }
        let resumed = app
            .request(Method::POST, &format!("{}/{id}/resume", path(project)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(resumed.status, 200, "{}", text(&resumed));
        assert!(resumed.json::<Value>().get("paused_reason").is_none());
        assert_eq!(paused_reason(&app, id).await, None);
        assert!(
            (0..19).all(|_| !app.state.eval.breakers.record(id, true)),
            "the window was reset"
        );
        // A rule that is not paused answers unchanged.
        let again = app
            .request(Method::POST, &format!("{}/{id}/resume", path(project)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(again.status, 200);
    }

    #[tokio::test]
    async fn a_budget_change_clears_a_budget_pause() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &rule_body(evaluator)).await;
        let id = id_of(&created);
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    pause_rule(conn, id, PAUSE_BUDGET, None, 5)
                        .await
                        .map(|_| ())
                })
            })
            .await
            .unwrap();
        assert_eq!(paused_reason(&app, id).await.as_deref(), Some(PAUSE_BUDGET));
        let response = patch(&app, &seeded, id, &json!({"budget_usd": 9.0})).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        assert_eq!(paused_reason(&app, id).await, None);
        // A later budget change finds nothing to clear, so its record says so.
        let response = patch(&app, &seeded, id, &json!({"budget_usd": 10.0})).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let updates: Vec<String> = rule_audit_rows(&app)
            .await
            .into_iter()
            .filter(|row| row.0 == "online_rule.updated")
            .map(|row| row.1)
            .collect();
        assert_eq!(
            updates,
            [
                r#"{"fields":["budget_usd"],"budget_resumed":true}"#,
                r#"{"fields":["budget_usd"],"budget_resumed":false}"#,
            ]
        );
    }

    /// `(action, details)` of every audit row of an online rule, oldest first.
    async fn rule_audit_rows(app: &TestApp) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT action, details FROM audit_events WHERE target_kind = 'online_rule' \
             ORDER BY seq",
        )
        .fetch_all(app.state.meta.reader())
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn rule_resume_is_recorded_as_manual() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &rule_body(evaluator)).await;
        let id = id_of(&created);
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    pause_rule(conn, id, PAUSE_ERRORS, None, 5)
                        .await
                        .map(|_| ())
                })
            })
            .await
            .unwrap();
        let response = app
            .request(
                Method::POST,
                &format!("{}/{id}/resume", path(seeded.project.id)),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let resumes: Vec<String> = rule_audit_rows(&app)
            .await
            .into_iter()
            .filter(|row| row.0 == "online_rule.resumed")
            .map(|row| row.1)
            .collect();
        assert_eq!(resumes, [r#"{"reason":"manual"}"#]);
    }

    #[tokio::test]
    async fn viewer_reads_member_writes_api_key_cannot_write() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge(&app, &seeded).await;
        let member = app
            .add_member(&seeded, "member@example.com", OrgRole::Member)
            .await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let refused = app
            .request(Method::POST, &path(seeded.project.id))
            .header(header::COOKIE.as_str(), &viewer.cookie)
            .json(&rule_body(evaluator))
            .send()
            .await;
        assert_eq!(refused.status, 403, "{}", text(&refused));
        let listed = app
            .request(Method::GET, &path(seeded.project.id))
            .header(header::COOKIE.as_str(), &viewer.cookie)
            .send()
            .await;
        assert_eq!(listed.status, 200, "{}", text(&listed));
        let created = app
            .request(Method::POST, &path(seeded.project.id))
            .header(header::COOKIE.as_str(), &member.cookie)
            .json(&rule_body(evaluator))
            .send()
            .await;
        assert_eq!(created.status, 201, "{}", text(&created));
        let by_key = app
            .request(Method::POST, &path(seeded.project.id))
            .header(
                header::AUTHORIZATION.as_str(),
                &format!("Bearer {}", seeded.read_key),
            )
            .json(&rule_body(evaluator))
            .send()
            .await;
        assert_eq!(by_key.status, 403, "{}", text(&by_key));
    }

    #[tokio::test]
    async fn an_api_key_reads_but_cannot_write() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge(&app, &seeded).await;
        create_ok(&app, &seeded, &rule_body(evaluator)).await;
        let read = app
            .request(Method::GET, &path(seeded.project.id))
            .header(
                header::AUTHORIZATION.as_str(),
                &format!("Bearer {}", seeded.read_key),
            )
            .send()
            .await;
        assert_eq!(read.status, 200);
        let mut body = rule_body(evaluator);
        body["name"] = json!("By key");
        let refused = app
            .request(Method::POST, &path(seeded.project.id))
            .header(
                header::AUTHORIZATION.as_str(),
                &format!("Bearer {}", seeded.read_key),
            )
            .json(&body)
            .send()
            .await;
        assert_eq!(refused.status, 403, "{}", text(&refused));
    }

    #[tokio::test]
    async fn delete_removes_the_rule_its_spend_and_its_breaker() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let evaluator = judge(&app, &seeded).await;
        let created = create_ok(&app, &seeded, &rule_body(evaluator)).await;
        let id = id_of(&created);
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { add_spend(conn, id, day_start(now), 0.5).await })
            })
            .await
            .unwrap();
        app.state.eval.breakers.record(id, true);
        let deleted = app
            .request(Method::DELETE, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(deleted.status, 204, "{}", text(&deleted));
        let spend: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rule_spend WHERE rule_id = ?")
            .bind(id.to_string())
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap();
        assert_eq!(spend, 0);
        let gone = app
            .request(Method::GET, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(gone.status, 404);
        assert!(
            !app.state.eval.breakers.record(id, true),
            "the window is gone"
        );
    }

    /// The clock of the preview tests: 2026-01-02T12:00:00Z.
    const PREVIEW_NOW: i64 = 1_767_355_200_000_000_000;
    const HOUR_NS: i64 = 3_600_000_000_000;

    /// A span of trace `trace` (span `span`) at `start`. Span 1 is the root.
    fn preview_span(trace: u8, span: u8, start: i64, status: SpanStatus, env: &str) -> Span {
        let mut row = dataset_span(0);
        row.trace_id = TraceId::from_bytes([trace; 16]).expect("trace id");
        row.span_id = SpanId::from_bytes([span; 8]).expect("span id");
        row.parent_span_id = (span != 1).then(|| SpanId::from_bytes([1; 8]).expect("span id"));
        row.start_time = start;
        row.end_time = start + 1_000_000;
        row.status = status;
        row.environment = Some(env.to_owned());
        row.metadata.clear();
        row
    }

    /// Epoch milliseconds as a string, the form `TimeParam` takes in a body.
    fn millis(ns: i64) -> String {
        (ns / 1_000_000).to_string()
    }

    async fn preview(app: &TestApp, seeded: &Seeded, body: &Value) -> TestResponse {
        app.request(
            Method::POST,
            &format!("{}/preview", path(seeded.project.id)),
        )
        .header(header::COOKIE.as_str(), &seeded.cookie)
        .json(body)
        .send()
        .await
    }

    #[tokio::test]
    async fn preview_counts_the_last_24_hours() {
        let app = TestApp::new().await;
        app.state.clock.set(PREVIEW_NOW);
        let seeded = app.seed().await;
        app.commit_spans(
            &seeded.project,
            vec![
                preview_span(1, 1, PREVIEW_NOW - HOUR_NS, SpanStatus::Ok, "production"),
                preview_span(
                    2,
                    1,
                    PREVIEW_NOW - 6 * HOUR_NS,
                    SpanStatus::Ok,
                    "production",
                ),
                preview_span(3, 1, PREVIEW_NOW - 2 * DAY_NS, SpanStatus::Ok, "production"),
                preview_span(
                    4,
                    1,
                    PREVIEW_NOW - 2 * HOUR_NS,
                    SpanStatus::Error,
                    "production",
                ),
            ],
        )
        .await;
        let response = preview(
            &app,
            &seeded,
            &json!({"target": "trace", "filter": "status = 'ok'"}),
        )
        .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let body: Value = response.json();
        assert_eq!(body["matched"], 2, "{body}");
        assert_eq!(body["capped"], false);
        assert_eq!(body["from"], "2026-01-01T12:00:00.000000000Z");
        assert_eq!(body["to"], "2026-01-02T12:00:00.000000000Z");
    }

    #[tokio::test]
    async fn preview_counts_spans_for_a_span_target() {
        let app = TestApp::new().await;
        app.state.clock.set(PREVIEW_NOW);
        let seeded = app.seed().await;
        app.commit_spans(
            &seeded.project,
            vec![
                preview_span(1, 1, PREVIEW_NOW - HOUR_NS, SpanStatus::Ok, "production"),
                preview_span(1, 2, PREVIEW_NOW - HOUR_NS, SpanStatus::Error, "production"),
                preview_span(1, 3, PREVIEW_NOW - HOUR_NS, SpanStatus::Error, "production"),
            ],
        )
        .await;
        let errors = preview(
            &app,
            &seeded,
            &json!({"target": "span", "filter": "status = 'error'"}),
        )
        .await;
        assert_eq!(errors.status, 200, "{}", text(&errors));
        assert_eq!(errors.json::<Value>()["matched"], 2);
        let all = preview(&app, &seeded, &json!({"target": "span"})).await;
        assert_eq!(all.json::<Value>()["matched"], 3);
    }

    #[tokio::test]
    async fn preview_leaves_out_evaluation_traces() {
        let app = TestApp::new().await;
        app.state.clock.set(PREVIEW_NOW);
        let seeded = app.seed().await;
        app.commit_spans(
            &seeded.project,
            vec![
                preview_span(1, 1, PREVIEW_NOW - HOUR_NS, SpanStatus::Ok, "production"),
                preview_span(2, 1, PREVIEW_NOW - HOUR_NS, SpanStatus::Ok, "cetra-eval"),
            ],
        )
        .await;
        // The filter cannot bring an evaluation trace back (D-I-17).
        let named = preview(
            &app,
            &seeded,
            &json!({"target": "trace", "filter": "environment = 'cetra-eval'"}),
        )
        .await;
        assert_eq!(named.status, 200, "{}", text(&named));
        assert_eq!(named.json::<Value>()["matched"], 0);
        let all = preview(&app, &seeded, &json!({"target": "trace"})).await;
        assert_eq!(all.json::<Value>()["matched"], 1);
    }

    #[tokio::test]
    async fn preview_validates_the_window() {
        let app = TestApp::new().await;
        app.state.clock.set(PREVIEW_NOW);
        let seeded = app.seed().await;
        let backwards = preview(
            &app,
            &seeded,
            &json!({
                "target": "trace",
                "from": millis(PREVIEW_NOW),
                "to": millis(PREVIEW_NOW - HOUR_NS)
            }),
        )
        .await;
        assert!(
            field_errors(&backwards)
                .contains(&("from".to_owned(), "from must be before to".to_owned()))
        );
        let long = preview(
            &app,
            &seeded,
            &json!({"target": "trace", "from": millis(PREVIEW_NOW - 31 * DAY_NS)}),
        )
        .await;
        assert!(field_errors(&long).contains(&(
            "from".to_owned(),
            "the window is at most 30 days".to_owned()
        )));
        let long_filter = preview(
            &app,
            &seeded,
            &json!({"target": "trace", "filter": "x".repeat(4097)}),
        )
        .await;
        assert!(field_errors(&long_filter).contains(&(
            "filter".to_owned(),
            "filter must be at most 4096 bytes".to_owned()
        )));
    }

    #[tokio::test]
    async fn an_invalid_filter_is_400_with_position() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let response = preview(
            &app,
            &seeded,
            &json!({"target": "trace", "filter": "error_count >"}),
        )
        .await;
        assert_eq!(response.status, 400, "{}", text(&response));
        let problem = response.problem();
        assert_eq!(problem.code, crate::api::common::ProblemCode::InvalidFilter);
        assert_eq!(problem.errors[0].field, "filter");
        assert!(problem.errors[0].position.is_some());
    }

    #[tokio::test]
    async fn viewer_and_api_key_cannot_preview() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let refused = app
            .request(
                Method::POST,
                &format!("{}/preview", path(seeded.project.id)),
            )
            .header(header::COOKIE.as_str(), &viewer.cookie)
            .json(&json!({"target": "trace"}))
            .send()
            .await;
        assert_eq!(refused.status, 403, "{}", text(&refused));
        let by_key = app
            .request(
                Method::POST,
                &format!("{}/preview", path(seeded.project.id)),
            )
            .header(
                header::AUTHORIZATION.as_str(),
                &format!("Bearer {}", seeded.read_key),
            )
            .json(&json!({"target": "trace"}))
            .send()
            .await;
        assert_eq!(by_key.status, 403, "{}", text(&by_key));
    }

    #[tokio::test]
    async fn preview_is_not_a_rule_id() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let get = app
            .request(Method::GET, &format!("{}/preview", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(get.status, 405, "{}", text(&get));
        let post = preview(&app, &seeded, &json!({"target": "trace"})).await;
        assert_eq!(post.status, 200, "{}", text(&post));
    }

    /// The clock of the stats tests: 2026-01-01T12:00:00Z, an hour boundary (the same instant as
    /// the evaluator stats tests).
    const STATS_NOW: i64 = 1_767_268_800_000_000_000;
    const DAY_NS: i64 = 86_400_000_000_000;

    /// `GET {project}/online-rules/stats` with `seeded`'s cookie.
    async fn stats_of(app: &TestApp, seeded: &Seeded, project: ProjectId) -> TestResponse {
        app.request(Method::GET, &format!("{}/stats", path(project)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await
    }

    /// A check evaluator of `project` whose output score is `output` (not run here).
    async fn output_evaluator(app: &TestApp, project: ProjectId, output: Value) -> Uuid {
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let id = Uuid::now_v7();
                    insert_evaluator(
                        conn,
                        id,
                        project,
                        "check",
                        EvaluatorFields {
                            name: format!("output {id}"),
                            config: json!({"check": "json_valid"}).to_string(),
                            output_score: output.to_string(),
                            connection_id: None,
                            secret_enc: None,
                            version: 1,
                        },
                        None,
                        now,
                    )
                    .await?;
                    Ok::<Uuid, MetaError>(id)
                })
            })
            .await
            .unwrap()
    }

    /// A rule named `name` on `evaluator` in `project`, written straight to the store.
    async fn rule_in(app: &TestApp, project: ProjectId, evaluator: Uuid, name: &str) -> Uuid {
        let now = app.state.clock.now_ns();
        let name = name.to_owned();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let id = Uuid::now_v7();
                    store::insert_rule(
                        conn,
                        NewRule {
                            id,
                            project_id: project,
                            evaluator_id: evaluator,
                            target: "trace".to_owned(),
                            created_by: None,
                        },
                        rule_fields(&name, true),
                        now,
                    )
                    .await?;
                    Ok::<Uuid, MetaError>(id)
                })
            })
            .await
            .unwrap()
    }

    /// Appends a score of `name` made by `rule` in `project`, through storage as the ingest path
    /// does.
    async fn put_rule_score(
        app: &TestApp,
        project: ProjectId,
        rule: Uuid,
        name: &str,
        value: ScoreValue,
        created_at: i64,
    ) {
        let score = Score {
            project_id: project,
            score_id: Uuid::now_v7(),
            target: ScoreTarget::Trace {
                trace_id: "0102030405060708090a0b0c0d0e0f10".parse().unwrap(),
            },
            session_id: None,
            user_id: None,
            name: name.to_owned(),
            value,
            reason: None,
            source: ScoreSource::LlmJudge,
            evaluator_id: None,
            rule_id: Some(rule),
            experiment_id: None,
            dataset_item_id: None,
            queue_id: None,
            author_user_id: None,
            metadata: Default::default(),
            created_at,
            updated_at: created_at,
            deleted: false,
            ingest_time: created_at,
        };
        let batch = scores_to_batch(&[score]).unwrap();
        app.state
            .storage
            .append_scores(project, batch)
            .await
            .unwrap();
    }

    /// Writes a rule job of `rule` in `project` that finished at `finished_at` with `status`.
    async fn finished_rule_job(
        app: &TestApp,
        project: ProjectId,
        rule: Uuid,
        evaluator: Uuid,
        status: &'static str,
        finished_at: i64,
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
                            kind: KIND_RULE,
                            rule_id: Some(rule),
                            evaluator_id: evaluator,
                            variable_mapping: None,
                            trace_id,
                            span_id: None,
                            trace_start_ns: finished_at,
                        },
                        finished_at,
                    )
                    .await?;
                    sqlx::query("UPDATE eval_jobs SET status = ?, finished_at = ? WHERE id = ?")
                        .bind(status)
                        .bind(finished_at)
                        .bind(id.to_string())
                        .execute(&mut *conn)
                        .await?;
                    Ok::<(), MetaError>(())
                })
            })
            .await
            .unwrap();
    }

    /// The entry of `rule` in a stats answer.
    fn entry_of(body: &OnlineRuleStatsResponse, rule: Uuid) -> &OnlineRuleStats {
        body.data
            .iter()
            .find(|entry| entry.rule_id == rule)
            .unwrap_or_else(|| panic!("no entry for {rule}"))
    }

    #[tokio::test]
    async fn stats_give_hourly_jobs_and_the_numeric_average() {
        let app = TestApp::new().await;
        app.state.clock.set(STATS_NOW);
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let numeric = output_evaluator(
            &app,
            project,
            json!({"name": "relevance", "data_type": "numeric"}),
        )
        .await;
        let beta = rule_in(&app, project, numeric, "Beta").await;
        let alpha = rule_in(&app, project, numeric, "Alpha").await;
        // `now - 1` is in the hour before `now` (index 22), two hours earlier is index 20.
        let last = now - 1;
        let two_back = now - 1 - 2 * HOUR_NS;
        for score in [1.0, 0.5, 0.25] {
            let at = if score == 0.25 { two_back } else { last };
            put_rule_score(
                &app,
                project,
                beta,
                "relevance",
                ScoreValue::Numeric(score),
                at,
            )
            .await;
        }
        put_rule_score(
            &app,
            project,
            beta,
            "relevance",
            ScoreValue::Numeric(9.0),
            now - 7 * DAY_NS,
        )
        .await;
        put_rule_score(
            &app,
            project,
            alpha,
            "relevance",
            ScoreValue::Numeric(3.0),
            last,
        )
        .await;
        finished_rule_job(&app, project, beta, numeric, "done", last).await;
        finished_rule_job(&app, project, beta, numeric, "done", last).await;
        finished_rule_job(&app, project, beta, numeric, "failed", two_back).await;
        finished_rule_job(&app, project, beta, numeric, "skipped", now - 9 * HOUR_NS).await;

        let response = stats_of(&app, &seeded, seeded.project.id).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let body: OnlineRuleStatsResponse = response.json();
        assert_eq!(
            body.window.to - body.window.from,
            23 * HOUR_NS,
            "the 23 hours before the current hour, and the current one up to now"
        );
        assert_eq!(body.data.len(), 2);
        assert_eq!(body.data[0].rule_id, alpha, "ordered by name");
        assert_eq!(body.data[1].rule_id, beta);

        let mine = entry_of(&body, beta);
        assert_eq!(mine.jobs_hourly.len(), RULE_STATS_HOURS);
        assert_eq!(mine.jobs_hourly[0].time, body.window.from);
        assert_eq!(mine.jobs_hourly[22].done, 2);
        assert_eq!(mine.jobs_hourly[20].failed, 1);
        assert_eq!(mine.jobs_hourly[20].done, 0);
        assert_eq!(mine.jobs_hourly[14].skipped, 1, "nine hours before now");
        assert_eq!(mine.scores.data_type, ScoreDataType::Numeric);
        assert_eq!(
            mine.scores.count, 3,
            "the score of another rule and the old one are left out"
        );
        assert!((mine.scores.average.unwrap() - 0.5833333333).abs() < 1e-9);
        assert_eq!(mine.scores.pass_rate, None);
        assert_eq!(mine.scores.top_category, None);
        assert_eq!(mine.scores.hourly.len(), RULE_STATS_HOURS);
        assert_eq!(mine.scores.hourly[22].value, Some(0.75));
        assert_eq!(mine.scores.hourly[20].value, Some(0.25));
        assert_eq!(mine.scores.hourly[21].value, None);

        let other = entry_of(&body, alpha);
        assert_eq!(other.scores.count, 1);
        assert_eq!(other.scores.average, Some(3.0));
        assert!(other.jobs_hourly.iter().all(|hour| hour.done == 0));
    }

    #[tokio::test]
    async fn stats_read_the_scores_of_many_rules_in_chunks() {
        let app = TestApp::new().await;
        app.state.clock.set(STATS_NOW);
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let numeric = output_evaluator(
            &app,
            project,
            json!({"name": "relevance", "data_type": "numeric"}),
        )
        .await;
        // 450 rules scored in each of the 23 full hours give 10 350 hourly rows, more than one
        // statement may return (SQL_MAX_ROWS): the rules are read in chunks (D-I-50).
        let mut rules = Vec::new();
        for index in 0..450 {
            rules.push(rule_in(&app, project, numeric, &format!("Rule {index:03}")).await);
        }
        let (from, _) = stats::rule_window(now);
        let mut scores = Vec::new();
        for (index, rule) in rules.iter().enumerate() {
            for hour in 0..23_i64 {
                let at = from + hour * HOUR_NS;
                scores.push(Score {
                    project_id: project,
                    score_id: Uuid::now_v7(),
                    target: ScoreTarget::Trace {
                        trace_id: "0102030405060708090a0b0c0d0e0f10".parse().unwrap(),
                    },
                    session_id: None,
                    user_id: None,
                    name: "relevance".to_owned(),
                    value: ScoreValue::Numeric(if index == 0 { 0.25 } else { 0.75 }),
                    reason: None,
                    source: ScoreSource::LlmJudge,
                    evaluator_id: None,
                    rule_id: Some(*rule),
                    experiment_id: None,
                    dataset_item_id: None,
                    queue_id: None,
                    author_user_id: None,
                    metadata: Default::default(),
                    created_at: at,
                    updated_at: at,
                    deleted: false,
                    ingest_time: at,
                });
            }
        }
        let batch = scores_to_batch(&scores).unwrap();
        app.state
            .storage
            .append_scores(project, batch)
            .await
            .unwrap();

        let response = stats_of(&app, &seeded, project).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let body: OnlineRuleStatsResponse = response.json();
        assert_eq!(body.data.len(), 450);
        assert_eq!(entry_of(&body, rules[0]).scores.average, Some(0.25));
        assert_eq!(entry_of(&body, rules[449]).scores.average, Some(0.75));
        assert_eq!(entry_of(&body, rules[449]).scores.count, 23);
    }

    #[tokio::test]
    async fn stats_give_the_pass_rate_and_the_top_label() {
        let app = TestApp::new().await;
        app.state.clock.set(STATS_NOW);
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let boolean = output_evaluator(
            &app,
            project,
            json!({"name": "passes", "data_type": "boolean"}),
        )
        .await;
        let categorical = output_evaluator(
            &app,
            project,
            json!({"name": "label", "data_type": "categorical", "categories": ["a", "b"]}),
        )
        .await;
        let check = rule_in(&app, project, boolean, "Passes").await;
        let labels = rule_in(&app, project, categorical, "Labels").await;
        let last = now - 1;
        for value in [true, false] {
            put_rule_score(
                &app,
                project,
                check,
                "passes",
                ScoreValue::Boolean(value),
                last,
            )
            .await;
        }
        put_rule_score(
            &app,
            project,
            check,
            "passes",
            ScoreValue::Boolean(true),
            now - 2 * HOUR_NS - 1,
        )
        .await;
        for label in ["b", "a", "a"] {
            put_rule_score(
                &app,
                project,
                labels,
                "label",
                ScoreValue::Categorical {
                    label: label.to_owned(),
                    value: None,
                },
                last,
            )
            .await;
        }

        let response = stats_of(&app, &seeded, seeded.project.id).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let body: OnlineRuleStatsResponse = response.json();
        let passes = entry_of(&body, check);
        assert_eq!(passes.scores.data_type, ScoreDataType::Boolean);
        assert!((passes.scores.pass_rate.unwrap() - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(passes.scores.average, None);
        assert_eq!(passes.scores.hourly[22].value, Some(0.5));
        assert_eq!(passes.scores.hourly[20].value, Some(1.0));

        let label = entry_of(&body, labels);
        assert_eq!(label.scores.data_type, ScoreDataType::Categorical);
        assert_eq!(
            label.scores.top_category,
            Some(CategoryCount {
                label: "a".to_owned(),
                count: 2
            })
        );
        assert!(
            label
                .scores
                .hourly
                .iter()
                .all(|point| point.value.is_none())
        );
    }

    #[tokio::test]
    async fn a_rule_without_data_has_zero_jobs_and_no_scores() {
        let app = TestApp::new().await;
        app.state.clock.set(STATS_NOW);
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let numeric = output_evaluator(
            &app,
            project,
            json!({"name": "relevance", "data_type": "numeric"}),
        )
        .await;
        let quiet = rule_in(&app, project, numeric, "Quiet").await;

        let response = stats_of(&app, &seeded, seeded.project.id).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let body: OnlineRuleStatsResponse = response.json();
        let mine = entry_of(&body, quiet);
        assert_eq!(mine.jobs_hourly.len(), RULE_STATS_HOURS);
        assert!(
            mine.jobs_hourly
                .iter()
                .all(|hour| hour.done + hour.failed + hour.skipped == 0)
        );
        assert_eq!(mine.scores.count, 0);
        assert_eq!(mine.scores.average, None);
        assert_eq!(mine.scores.hourly.len(), RULE_STATS_HOURS);
        assert!(mine.scores.hourly.iter().all(|point| point.value.is_none()));
    }

    #[tokio::test]
    async fn stats_do_not_count_other_projects_and_viewers_can_read() {
        let app = TestApp::new().await;
        app.state.clock.set(STATS_NOW);
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let numeric = output_evaluator(
            &app,
            seeded.project.id,
            json!({"name": "relevance", "data_type": "numeric"}),
        )
        .await;
        let mine = rule_in(&app, seeded.project.id, numeric, "Mine").await;
        let (other, _) = app.add_project(&seeded, "other", None).await;
        let other_eval = output_evaluator(
            &app,
            other.id,
            json!({"name": "relevance", "data_type": "numeric"}),
        )
        .await;
        let theirs = rule_in(&app, other.id, other_eval, "Theirs").await;
        put_rule_score(
            &app,
            other.id,
            theirs,
            "relevance",
            ScoreValue::Numeric(1.0),
            now - 1,
        )
        .await;
        finished_rule_job(&app, other.id, theirs, other_eval, "done", now - 1).await;

        let response = stats_of(&app, &seeded, seeded.project.id).await;
        let body: OnlineRuleStatsResponse = response.json();
        assert_eq!(body.data.len(), 1);
        assert_eq!(body.data[0].rule_id, mine);
        assert_eq!(body.data[0].scores.count, 0);
        assert!(body.data[0].jobs_hourly.iter().all(|hour| hour.done == 0));

        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
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
}
