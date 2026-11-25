//! Issue routes (spec 09 § Issues, D-IC-1, D-IC-3, D-IC-9, D-IC-10, D-IC-13): sources, and the
//! issues they find with their counts, occurrences and history. Project members and above write
//! the sources; every project reader lists and reads.

use std::collections::{BTreeMap, HashMap};

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, patch, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cetra_core::ids::{OrgId, ProjectId};
use cetra_core::score::{Score, ScoreTarget};
use cetra_eval::Adapter;
use cetra_eval::issue::cluster;
use cetra_storage::{TimeWindow, ViewName};
use serde::{Deserialize, Serialize};
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::api::common::{FieldError, PageParams, TimeParam};
use crate::api::issues::{
    CreateIssueSourceRequest, Issue, IssueCandidateCounts, IssueDay, IssueEvent, IssueEventPage,
    IssueExample, IssueListParams, IssueOccurrenceParams, IssueOccurrences, IssuePage, IssueRefit,
    IssueRefitPage, IssueSource, IssueSourceKind, IssueSourcePage, IssueState, MergeIssueRequest,
    UpdateIssueRequest, UpdateIssueSourceRequest,
};
use crate::api::online_rules::BudgetPeriod;
use crate::audit::catalog::IssueSourceAction;
use crate::audit::{self, Event, TargetKind};
use crate::auth::principal::{self, Principal};
use crate::auth::roles::ProjectPerm;
use crate::eval::budget::{self, DAY_NS, day_start};
use crate::eval::rule_check::field_error;
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::issues::read::{
    OCCURRENCE_DEFAULT_PERIOD_NS, Tally, example_issues, most_occurrences_first, name_counts,
    names_of, occurrence_scores, score_name, tallies, total,
};
use crate::issues::source_limit::{self, SourceLimitError};
use crate::meta::MetaError;
use crate::meta::connections;
use crate::meta::issue_links;
use crate::meta::issues::{self as store, IssueSort, RefitRow, SourceKind, SourceRow};
use crate::meta::online_rules::PAUSE_BUDGET;

use super::alert_rules::checked_channels;
use super::orgs::page_named;
use super::read::open_query;

/// Most ids one issue list filters by (`ids`).
const MAX_IDS: usize = 100;
/// Longest period of the occurrence days (`from` to `to`).
const MAX_OCCURRENCE_PERIOD_NS: i64 = 90 * DAY_NS;
/// Examples read per name, and examples returned.
const EXAMPLES_PER_NAME: usize = 20;
const MAX_EXAMPLES: usize = 20;
/// Events an issue read returns, newest first.
const MAX_EVENTS: u32 = 100;
/// Longest title, in characters, and longest description (`PATCH …/issues/{id}`).
const MAX_TITLE_CHARS: usize = 200;
const MAX_DESCRIPTION_CHARS: usize = 2_000;
/// Refits a source's refit list returns, newest first.
const MAX_REFITS: u32 = 50;
/// The defaults of an LLM source (D-IC-1, D-IC-7).
const DEFAULT_SAMPLE_RATE: f64 = 1.0;
const DEFAULT_DAILY_CAP: u32 = 500;
const DEFAULT_FLOOR: f64 = 0.80;
const DEFAULT_MIN_ISSUE_SIZE: u32 = 5;
/// The filter a new topic source gets when it has none (D-IC-1).
const TOPIC_FILTER: &str = "environment = 'production'";
/// The feedback sources a new `feedback` source reads when it names none.
const FEEDBACK_SOURCES: [&str; 2] = ["end_user", "human"];
/// Most score names of a `score` or `feedback` source, and the longest one in bytes.
const MAX_SCORE_NAMES: usize = 8;
const MAX_SCORE_NAME_BYTES: usize = 255;
/// Largest budget in USD, as online rules take it.
const MAX_BUDGET_USD: f64 = 1_000_000.0;
/// The first refit of a new LLM source comes an hour after its creation (D-IC-8).
const FIRST_REFIT_DELAY_NS: i64 = 3_600_000_000_000;
/// "Refit now" is refused when a refit started less than this long ago (D-IC-8).
const REFIT_COOLDOWN_NS: i64 = 600_000_000_000;

/// Issue routes. Reads need project view; source writes need
/// [`ProjectPerm::ManageEvaluation`] (D-I-24). Triage (`PATCH` of an issue and its merge) needs a
/// session: an API key or a personal access token is refused, as on the other session-only routes.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/issue-sources",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_issue_sources),
        },
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/v1/projects/{project}/issue-sources",
            access: Access::Project {
                perm: ProjectPerm::ManageEvaluation,
                session_only: false,
            },
            documented: true,
            handler: post(create_issue_source),
        },
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/issue-sources/{id}",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_issue_source),
        },
        RouteDef {
            method: axum::http::Method::PATCH,
            path: "/api/v1/projects/{project}/issue-sources/{id}",
            access: Access::Project {
                perm: ProjectPerm::ManageEvaluation,
                session_only: false,
            },
            documented: true,
            handler: patch(update_issue_source),
        },
        RouteDef {
            method: axum::http::Method::DELETE,
            path: "/api/v1/projects/{project}/issue-sources/{id}",
            access: Access::Project {
                perm: ProjectPerm::ManageEvaluation,
                session_only: false,
            },
            documented: true,
            handler: delete(delete_issue_source),
        },
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/v1/projects/{project}/issue-sources/{id}/refit",
            access: Access::Project {
                perm: ProjectPerm::ManageEvaluation,
                session_only: false,
            },
            documented: true,
            handler: post(post_issue_source_refit),
        },
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/issue-sources/{id}/refits",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_issue_source_refits),
        },
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/issues",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_issues),
        },
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/issues/{id}",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_issue),
        },
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/issues/{id}/occurrences",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_issue_occurrences),
        },
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/issues/{id}/events",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_issue_events),
        },
        RouteDef {
            method: axum::http::Method::PATCH,
            path: "/api/v1/projects/{project}/issues/{id}",
            access: Access::Project {
                perm: ProjectPerm::Annotate,
                session_only: true,
            },
            documented: true,
            handler: patch(patch_issue),
        },
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/v1/projects/{project}/issues/{id}/merge",
            access: Access::Project {
                perm: ProjectPerm::Annotate,
                session_only: true,
            },
            documented: true,
            handler: post(post_issue_merge),
        },
    ]
}

#[derive(Debug, serde::Deserialize)]
struct IdPath {
    id: Uuid,
}

/// What a create or PATCH transaction did.
enum SourceWrite {
    Done(Box<SourceRow>),
    /// The source is missing.
    Missing,
    /// Creating or enabling the source passes the active-source limit (D-IC-13).
    OverLimit(SourceLimitError),
    /// A channel was deleted after the request's check and before the write.
    UnknownChannel,
}

/// The `channel_ids` field error of a channel that no longer exists.
fn unknown_channel() -> ApiError {
    ApiError::BadRequest {
        detail: None,
        errors: vec![field_error("channel_ids", "unknown channel")],
    }
}

async fn list_issue_sources(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<PageParams>,
) -> Result<error::Json<IssueSourcePage>, ApiError> {
    let project = access.project.id;
    let rows = store::list_sources(state.meta.reader(), project).await?;
    let page = page_named(
        rows.into_iter()
            .map(|row| (row.name.clone(), row.id.to_string(), row))
            .collect(),
        params.cursor.as_deref(),
        params.limit.get(),
    )?;
    let open = store::open_counts(state.meta.reader(), project).await?;
    let channels = issue_links::source_channel_ids_by_source(state.meta.reader(), project).await?;
    let now = state.clock.now_ns();
    let mut data = Vec::with_capacity(page.data.len());
    for row in &page.data {
        let figures = llm_figures(&state, row, now).await?;
        let channel_ids = channels.get(&row.id).cloned().unwrap_or_default();
        data.push(source_json(
            row,
            open_issues(&open, row.id),
            channel_ids,
            figures.as_ref(),
        ));
    }
    let mut conn = state
        .meta
        .reader()
        .acquire()
        .await
        .map_err(MetaError::from)?;
    let org_active_sources = store::count_enabled_sources(&mut conn, access.project.org_id).await?;
    Ok(error::Json(IssueSourcePage {
        data,
        next_cursor: page.next_cursor,
        org_active_sources,
    }))
}

async fn get_issue_source(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
) -> Result<error::Json<IssueSource>, ApiError> {
    let project = access.project.id;
    let row = store::get_source(state.meta.reader(), project, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let open = store::open_counts(state.meta.reader(), project).await?;
    let channel_ids = issue_links::source_channel_ids(state.meta.reader(), id).await?;
    let figures = llm_figures(&state, &row, state.clock.now_ns()).await?;
    Ok(error::Json(source_json(
        &row,
        open_issues(&open, id),
        channel_ids,
        figures.as_ref(),
    )))
}

async fn create_issue_source(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    ctx: audit::Ctx,
    error::Json(body): error::Json<CreateIssueSourceRequest>,
) -> Result<(StatusCode, error::Json<IssueSource>), ApiError> {
    let user = principal::session_user(&principal)?;
    let project = access.project.id;
    let org = access.project.org_id;
    let now = state.clock.now_ns();
    let requested_channels = body.channel_ids.clone();
    let errors = unused(source_kind(body.kind), &create_given(&body));
    let kind = source_kind(body.kind).as_str();
    let row = created_row(body, project, user.id, now);
    validate(&state, org, project, &row, errors, now).await?;
    let channels = checked_channels(&state, project, &requested_channels).await?;
    let id = row.id;
    let limit = source_limit::limit(&state, org);
    let written = channels.clone();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let written_row = insert_checked(conn, org, row, limit, &written, now).await?;
                if let SourceWrite::Done(_) = &written_row {
                    let event = Event::new(IssueSourceAction::Created { kind })
                        .org(org)
                        .project(project)
                        .target(TargetKind::IssueSource, id);
                    audit::record(conn, &ctx, event, now).await?;
                }
                Ok(written_row)
            })
        })
        .await?;
    match outcome {
        SourceWrite::Done(row) => {
            reload_score_rules(&state).await;
            state.issues_wake();
            let figures = llm_figures(&state, &row, now).await?;
            Ok((
                StatusCode::CREATED,
                error::Json(source_json(&row, 0, channels, figures.as_ref())),
            ))
        }
        SourceWrite::OverLimit(error) => Err(error.into()),
        SourceWrite::UnknownChannel => Err(unknown_channel()),
        SourceWrite::Missing => Err(ApiError::NotFound),
    }
}

async fn update_issue_source(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    ctx: audit::Ctx,
    error::Path(IdPath { id }): error::Path<IdPath>,
    error::Json(body): error::Json<UpdateIssueSourceRequest>,
) -> Result<error::Json<IssueSource>, ApiError> {
    principal::session_user(&principal)?;
    let project = access.project.id;
    let org = access.project.org_id;
    let now = state.clock.now_ns();
    // The JSON names of the fields the body gave (P-2): names, never values.
    let changed_fields: Vec<&'static str> = [
        ("name", body.name.is_some()),
        ("filter", body.filter.is_some()),
        ("enabled", body.enabled.is_some()),
        ("channel_ids", body.channel_ids.is_some()),
        ("sample_rate", body.sample_rate.is_some()),
        ("daily_cap", body.daily_cap.is_some()),
        ("score_names", body.score_names.is_some()),
        ("feedback_sources", body.feedback_sources.is_some()),
        ("chat_connection_id", body.chat_connection_id.is_some()),
        ("chat_model", body.chat_model.is_some()),
        ("embed_connection_id", body.embed_connection_id.is_some()),
        ("embed_model", body.embed_model.is_some()),
        ("floor", body.floor.is_some()),
        ("min_issue_size", body.min_issue_size.is_some()),
        ("budget_usd", body.budget_usd.is_some()),
        ("budget_period", body.budget_period.is_some()),
        ("clear_budget", body.clear_budget),
    ]
    .into_iter()
    .filter_map(|(name, given)| given.then_some(name))
    .collect();
    let stored = store::get_source(state.meta.reader(), project, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mut errors = unused(stored.kind, &update_given(&body));
    errors.extend(update_refusals(&body));
    let requested_channels = body.channel_ids;
    let name = body
        .name
        .map_or_else(|| stored.name.clone(), |name| name.trim().to_owned());
    let filter = match body.filter {
        Some(text) => normalize_filter(text),
        None => stored.filter.clone(),
    };
    let row = SourceRow {
        name,
        filter,
        enabled: body.enabled.unwrap_or(stored.enabled),
        updated_at: now,
        sample_rate: body.sample_rate.or(stored.sample_rate),
        daily_cap: body.daily_cap.or(stored.daily_cap),
        score_names: body
            .score_names
            .map_or_else(|| stored.score_names.clone(), trimmed),
        feedback_sources: body
            .feedback_sources
            .unwrap_or_else(|| stored.feedback_sources.clone()),
        chat_connection_id: body.chat_connection_id.or(stored.chat_connection_id),
        chat_model: body.chat_model.or_else(|| stored.chat_model.clone()),
        floor: body.floor.or(stored.floor),
        min_issue_size: body.min_issue_size.or(stored.min_issue_size),
        budget_usd: match body.clear_budget {
            true => None,
            false => body.budget_usd.or(stored.budget_usd),
        },
        budget_period: match body.clear_budget {
            true => None,
            false => body
                .budget_period
                .map(|period| period.as_str().to_owned())
                .or_else(|| stored.budget_period.clone()),
        },
        ..stored.clone()
    };
    validate(&state, org, project, &row, errors, now).await?;
    let channels = match requested_channels {
        Some(ids) => Some(checked_channels(&state, project, &ids).await?),
        None => None,
    };
    let limit = source_limit::limit(&state, org);
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let written =
                    update_checked(conn, project, org, stored, row, limit, channels, now).await?;
                if let SourceWrite::Done(_) = &written {
                    let event = Event::new(IssueSourceAction::Updated {
                        fields: changed_fields,
                    })
                    .org(org)
                    .project(project)
                    .target(TargetKind::IssueSource, id);
                    audit::record(conn, &ctx, event, now).await?;
                }
                Ok(written)
            })
        })
        .await?;
    match outcome {
        SourceWrite::Done(row) => {
            reload_score_rules(&state).await;
            state.issues_wake();
            let open = store::open_counts(state.meta.reader(), project).await?;
            let channel_ids = issue_links::source_channel_ids(state.meta.reader(), id).await?;
            let figures = llm_figures(&state, &row, now).await?;
            Ok(error::Json(source_json(
                &row,
                open_issues(&open, id),
                channel_ids,
                figures.as_ref(),
            )))
        }
        SourceWrite::OverLimit(error) => Err(error.into()),
        SourceWrite::UnknownChannel => Err(unknown_channel()),
        SourceWrite::Missing => Err(ApiError::NotFound),
    }
}

async fn delete_issue_source(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    ctx: audit::Ctx,
    error::Path(IdPath { id }): error::Path<IdPath>,
) -> Result<StatusCode, ApiError> {
    principal::session_user(&principal)?;
    let project = access.project.id;
    let org = access.project.org_id;
    let now = state.clock.now_ns();
    // The source's issues and their events go with it (schema cascade, D-IC-3).
    let went = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let went = store::delete_source(conn, project, id).await?;
                if went {
                    let event = Event::new(IssueSourceAction::Deleted)
                        .org(org)
                        .project(project)
                        .target(TargetKind::IssueSource, id);
                    audit::record(conn, &ctx, event, now).await?;
                }
                Ok(went)
            })
        })
        .await?;
    if !went {
        return Err(ApiError::NotFound);
    }
    reload_score_rules(&state).await;
    state.issues_wake();
    Ok(StatusCode::NO_CONTENT)
}

/// `POST …/issue-sources/{id}/refit` (D-IC-8): asks for a refit now. An `error` source has no refit
/// (400), and a disabled or paused source cannot be refit (400) until it is enabled or resumed.
/// Refused with 409 when a refit started less than 10 minutes ago, or when one is already due.
/// Answers 202 with the source. The refit records the trigger `manual`.
async fn post_issue_source_refit(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
) -> Result<(StatusCode, error::Json<IssueSource>), ApiError> {
    let user = principal::session_user(&principal)?;
    let project = access.project.id;
    let now = state.clock.now_ns();
    let row = store::get_source(state.meta.reader(), project, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !row.kind.is_llm() {
        return Err(bad_request_field("kind", "only LLM sources have refits"));
    }
    if !row.enabled {
        return Err(bad_request_field(
            "enabled",
            "a disabled source cannot be refit",
        ));
    }
    if row.paused_reason.is_some() {
        return Err(bad_request_field(
            "paused_reason",
            "a paused source cannot be refit; resume it first",
        ));
    }
    if let Some(started) = store::last_refit_started(state.meta.reader(), id).await?
        && now - started < REFIT_COOLDOWN_NS
    {
        return Err(ApiError::Conflict {
            detail: "A refit started less than 10 minutes ago.",
        });
    }
    if row.next_refit_at.is_some_and(|due| due <= now) {
        return Err(ApiError::Conflict {
            detail: "A refit is already due.",
        });
    }
    state
        .meta
        .write(move |conn| {
            Box::pin(async move { store::set_next_refit(conn, id, now, "manual").await })
        })
        .await?;
    tracing::info!(
        source_id = %id,
        project_id = %project,
        user_id = %user.id,
        "issue source refit requested"
    );
    state.issues_wake();
    let row = SourceRow {
        next_refit_at: Some(now),
        ..row
    };
    let open = store::open_counts(state.meta.reader(), project).await?;
    let channel_ids = issue_links::source_channel_ids(state.meta.reader(), id).await?;
    let figures = llm_figures(&state, &row, now).await?;
    Ok((
        StatusCode::ACCEPTED,
        error::Json(source_json(
            &row,
            open_issues(&open, id),
            channel_ids,
            figures.as_ref(),
        )),
    ))
}

/// `GET …/issue-sources/{id}/refits`: the newest 50 refits of the source, newest first.
async fn list_issue_source_refits(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
) -> Result<error::Json<IssueRefitPage>, ApiError> {
    let project = access.project.id;
    store::get_source(state.meta.reader(), project, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let rows = store::list_refits(state.meta.reader(), id, MAX_REFITS).await?;
    Ok(error::Json(IssueRefitPage {
        data: rows.into_iter().map(refit_json).collect(),
    }))
}

/// Reloads the score rules after a source write (A11). A failure is logged; the next sweeper tick
/// reads the rules again.
async fn reload_score_rules(state: &AppState) {
    if let Err(error) = crate::issues::triggers::refresh_score_rules(state).await {
        tracing::warn!(error = %error, "issue score rules could not be reloaded");
    }
}

/// The create's write: when the source is enabled, the active-source limit is checked with it
/// counted (D-IC-13). An enabled source starts its notification clock now (D-IC-21).
async fn insert_checked(
    conn: &mut SqliteConnection,
    org: OrgId,
    row: SourceRow,
    limit: Option<u64>,
    channels: &[Uuid],
    now: i64,
) -> Result<SourceWrite, MetaError> {
    if !issue_links::channels_exist(conn, row.project_id, channels).await? {
        return Ok(SourceWrite::UnknownChannel);
    }
    if row.enabled {
        let active = store::count_enabled_sources(conn, org).await? + 1;
        if let Err(error) = source_limit::check(active, limit) {
            return Ok(SourceWrite::OverLimit(error));
        }
    }
    store::insert_source(conn, &row).await?;
    issue_links::set_source_channels(conn, row.id, channels).await?;
    if row.enabled {
        issue_links::set_last_notified_at(conn, row.id, now).await?;
    }
    Ok(SourceWrite::Done(Box::new(row)))
}

/// The PATCH's write: the source must be the row the checks ran against. Enabling a disabled
/// source checks the active-source limit (D-IC-13). A changed budget clears a `budget` pause in the
/// same write, as a changed rule budget does (D-I-15).
///
/// The notification clock starts again (D-IC-21) when the source is enabled, or when its channel
/// list goes from empty to non-empty, so a backlog is never sent (Q12). Other channel changes keep
/// the cut-off.
#[allow(clippy::too_many_arguments)]
async fn update_checked(
    conn: &mut SqliteConnection,
    project: ProjectId,
    org: OrgId,
    stored: SourceRow,
    mut row: SourceRow,
    limit: Option<u64>,
    channels: Option<Vec<Uuid>>,
    now: i64,
) -> Result<SourceWrite, MetaError> {
    let Some(current) = store::get_source_tx(conn, project, stored.id).await? else {
        return Ok(SourceWrite::Missing);
    };
    if current != stored {
        return Err(MetaError::Conflict("issue source changed, retry"));
    }
    if let Some(ids) = &channels
        && !issue_links::channels_exist(conn, project, ids).await?
    {
        return Ok(SourceWrite::UnknownChannel);
    }
    if !current.enabled && row.enabled {
        let active = store::count_enabled_sources(conn, org).await? + 1;
        if let Err(error) = source_limit::check(active, limit) {
            return Ok(SourceWrite::OverLimit(error));
        }
    }
    let budget_changed =
        current.budget_usd != row.budget_usd || current.budget_period != row.budget_period;
    if budget_changed && row.paused_reason.as_deref() == Some(PAUSE_BUDGET) {
        row.paused_reason = None;
        row.paused_at = None;
        row.paused_detail = None;
    }
    let had_channels = issue_links::source_has_channels(conn, row.id).await?;
    if !store::update_source(conn, &row).await? {
        return Ok(SourceWrite::Missing);
    }
    if let Some(channels) = &channels {
        issue_links::set_source_channels(conn, row.id, channels).await?;
    }
    let enabling = !current.enabled && row.enabled;
    let first_channels = channels
        .as_ref()
        .is_some_and(|ids| !ids.is_empty() && !had_channels);
    if enabling || first_channels {
        issue_links::set_last_notified_at(conn, row.id, now).await?;
    }
    Ok(SourceWrite::Done(Box::new(row)))
}

/// Checks a source as it would be stored (D-IC-1): the name, the filter and, for LLM kinds, the
/// LLM fields. `errors` holds the errors the request already has; every check runs and all the
/// errors come back together. The filter is parsed last, by storage: a traces expression for LLM
/// kinds, a spans expression for `error` sources.
async fn validate(
    state: &AppState,
    org: OrgId,
    project: ProjectId,
    row: &SourceRow,
    mut errors: Vec<FieldError>,
    now: i64,
) -> Result<(), ApiError> {
    if !(1..=128).contains(&row.name.chars().count()) {
        errors.push(field_error("name", "name must be 1 to 128 characters"));
    }
    if row.filter.as_deref().is_some_and(|text| text.len() > 4096) {
        errors.push(field_error("filter", "filter must be at most 4096 bytes"));
    }
    if row.kind.is_llm() {
        check_llm(state, org, row, now, &mut errors).await?;
    }
    if !errors.is_empty() {
        return Err(bad_request(errors));
    }
    if let Some(text) = &row.filter {
        let view = if row.kind.is_llm() {
            ViewName::Traces
        } else {
            ViewName::Spans
        };
        let ctx = open_query(state, org, project, None).await?;
        cetra_storage::validate_filter(&ctx, view, text)?;
    }
    Ok(())
}

/// The LLM fields of an LLM source (D-IC-1, D-IC-5, D-IC-7, D-IC-8): their ranges, the score
/// names and feedback sources of their kinds, the connections and the budget with its prices.
async fn check_llm(
    state: &AppState,
    org: OrgId,
    row: &SourceRow,
    now: i64,
    errors: &mut Vec<FieldError>,
) -> Result<(), ApiError> {
    if row
        .sample_rate
        .is_some_and(|rate| !(rate > 0.0 && rate <= 1.0))
    {
        errors.push(field_error(
            "sample_rate",
            "sample_rate must be above 0 and at most 1",
        ));
    }
    if row
        .daily_cap
        .is_some_and(|cap| !(1..=10_000).contains(&cap))
    {
        errors.push(field_error("daily_cap", "daily_cap must be 1 to 10000"));
    }
    if row
        .floor
        .is_some_and(|floor| !(0.5..=0.99).contains(&floor))
    {
        errors.push(field_error("floor", "floor must be 0.5 to 0.99"));
    }
    if row
        .min_issue_size
        .is_some_and(|size| !(2..=100).contains(&size))
    {
        errors.push(field_error(
            "min_issue_size",
            "min_issue_size must be 2 to 100",
        ));
    }
    if matches!(row.kind, SourceKind::Score | SourceKind::Feedback) {
        let names = &row.score_names;
        let valid = (1..=MAX_SCORE_NAMES).contains(&names.len())
            && names
                .iter()
                .all(|name| !name.is_empty() && name.len() <= MAX_SCORE_NAME_BYTES);
        if !valid {
            errors.push(field_error(
                "score_names",
                "score_names must be 1 to 8 names of 1 to 255 bytes",
            ));
        }
    }
    if row.kind == SourceKind::Feedback && !valid_feedback_sources(&row.feedback_sources) {
        errors.push(field_error(
            "feedback_sources",
            "feedback_sources must be end_user, human or both",
        ));
    }
    let chat = connection_adapter(
        state,
        org,
        "chat_connection_id",
        row.chat_connection_id,
        errors,
    )
    .await?;
    let embed = connection_adapter(
        state,
        org,
        "embed_connection_id",
        row.embed_connection_id,
        errors,
    )
    .await?;
    for (field, model) in [
        ("chat_model", &row.chat_model),
        ("embed_model", &row.embed_model),
    ] {
        if model.as_deref().is_none_or(|name| name.trim().is_empty()) {
            errors.push(field_error(field, "required for LLM sources"));
        }
    }
    if embed == Some(Adapter::Anthropic) {
        errors.push(field_error(
            "embed_connection_id",
            "this provider has no embeddings API",
        ));
    }
    if row.budget_usd.is_some() != row.budget_period.is_some() {
        errors.push(field_error(
            "budget_usd",
            "give budget_usd and budget_period together",
        ));
    }
    if let Some(budget) = row.budget_usd
        && !(budget.is_finite() && budget > 0.0 && budget <= MAX_BUDGET_USD)
    {
        errors.push(field_error(
            "budget_usd",
            "budget_usd must be above 0 and at most 1000000",
        ));
    }
    if row.budget_usd.is_some() {
        let prices = state.ingest.prices().for_org(org);
        if let (Some(adapter), Some(model)) = (chat, row.chat_model.as_deref())
            && budget::model_price(&prices, model, adapter, now).is_none()
        {
            errors.push(field_error(
                "budget_usd",
                "the chat model has no price, so the source cannot have a budget; add one under Settings → Model prices",
            ));
        }
        if let (Some(adapter), Some(model)) = (embed, row.embed_model.as_deref()) {
            let priced = prices
                .price_for(model, Some(adapter.provider_name()), now)
                .is_some_and(|price| price.price_input.is_some());
            if !priced {
                errors.push(field_error(
                    "budget_usd",
                    "the embedding model has no input price, so the source cannot have a budget; add one under Settings → Model prices",
                ));
            }
        }
    }
    Ok(())
}

/// The adapter of the org's connection `id` for `field`. A missing id or an unknown connection
/// is a field error; so is an unknown stored adapter, which is an internal error.
async fn connection_adapter(
    state: &AppState,
    org: OrgId,
    field: &str,
    id: Option<Uuid>,
    errors: &mut Vec<FieldError>,
) -> Result<Option<Adapter>, ApiError> {
    let Some(id) = id else {
        errors.push(field_error(field, "required for LLM sources"));
        return Ok(None);
    };
    let Some(connection) = connections::get_connection(state.meta.reader(), org, id).await? else {
        errors.push(field_error(field, "no such connection in this org"));
        return Ok(None);
    };
    Adapter::from_name(&connection.adapter)
        .map(Some)
        .map_err(|_| ApiError::Internal("an LLM connection has an unknown adapter".into()))
}

/// Whether `sources` is one or two different feedback sources, each `end_user` or `human`.
fn valid_feedback_sources(sources: &[String]) -> bool {
    let known = sources
        .iter()
        .all(|source| FEEDBACK_SOURCES.contains(&source.as_str()));
    match sources {
        [one] => known && FEEDBACK_SOURCES.contains(&one.as_str()),
        [first, second] => known && first != second,
        _ => false,
    }
}

/// Whether a source kind reads the LLM field `field` (D-IC-1). Only the score names and the
/// feedback sources differ by kind.
fn uses(kind: SourceKind, field: &str) -> bool {
    match kind {
        SourceKind::Error => false,
        SourceKind::Score => field != "feedback_sources",
        SourceKind::Feedback => true,
        SourceKind::Topic => !matches!(field, "score_names" | "feedback_sources"),
    }
}

/// One field error per given LLM field that the kind does not read.
fn unused(kind: SourceKind, given: &[(&str, bool)]) -> Vec<FieldError> {
    let message = format!("not used by {} sources", kind.as_str());
    given
        .iter()
        .filter(|(field, present)| *present && !uses(kind, field))
        .map(|(field, _)| field_error(field, &message))
        .collect()
}

/// Which LLM fields a create gives.
fn create_given(body: &CreateIssueSourceRequest) -> Vec<(&'static str, bool)> {
    vec![
        ("sample_rate", body.sample_rate.is_some()),
        ("daily_cap", body.daily_cap.is_some()),
        ("score_names", body.score_names.is_some()),
        ("feedback_sources", body.feedback_sources.is_some()),
        ("chat_connection_id", body.chat_connection_id.is_some()),
        ("chat_model", body.chat_model.is_some()),
        ("embed_connection_id", body.embed_connection_id.is_some()),
        ("embed_model", body.embed_model.is_some()),
        ("floor", body.floor.is_some()),
        ("min_issue_size", body.min_issue_size.is_some()),
        ("budget_usd", body.budget_usd.is_some()),
        ("budget_period", body.budget_period.is_some()),
    ]
}

/// Which LLM fields a PATCH gives. The embedding fields are refused by [`update_refusals`].
fn update_given(body: &UpdateIssueSourceRequest) -> Vec<(&'static str, bool)> {
    vec![
        ("sample_rate", body.sample_rate.is_some()),
        ("daily_cap", body.daily_cap.is_some()),
        ("score_names", body.score_names.is_some()),
        ("feedback_sources", body.feedback_sources.is_some()),
        ("chat_connection_id", body.chat_connection_id.is_some()),
        ("chat_model", body.chat_model.is_some()),
        ("floor", body.floor.is_some()),
        ("min_issue_size", body.min_issue_size.is_some()),
        ("budget_usd", body.budget_usd.is_some()),
        ("budget_period", body.budget_period.is_some()),
    ]
}

/// The PATCH errors that come from the body alone: the embedding fields are fixed after create,
/// and `clear_budget` takes no budget values.
fn update_refusals(body: &UpdateIssueSourceRequest) -> Vec<FieldError> {
    let mut errors = Vec::new();
    if body.embed_connection_id.is_some() {
        errors.push(field_error(
            "embed_connection_id",
            "fixed after create; create a new source",
        ));
    }
    if body.embed_model.is_some() {
        errors.push(field_error(
            "embed_model",
            "fixed after create; create a new source",
        ));
    }
    if body.clear_budget && (body.budget_usd.is_some() || body.budget_period.is_some()) {
        errors.push(field_error(
            "budget_usd",
            "clear_budget cannot come with budget_usd or budget_period",
        ));
    }
    errors
}

/// The row a create stores: the body's values, and for an LLM kind the defaults, the topic filter
/// and the first refit time (D-IC-1, D-IC-8). Refused fields are checked before this runs.
fn created_row(
    body: CreateIssueSourceRequest,
    project: ProjectId,
    user: Uuid,
    now: i64,
) -> SourceRow {
    let kind = source_kind(body.kind);
    let llm = kind.is_llm();
    let feedback_sources = match body.feedback_sources {
        Some(sources) => sources,
        None if kind == SourceKind::Feedback => FEEDBACK_SOURCES
            .iter()
            .map(|source| (*source).to_owned())
            .collect(),
        None => Vec::new(),
    };
    let filter = normalize_filter(body.filter)
        .or_else(|| (kind == SourceKind::Topic).then(|| TOPIC_FILTER.to_owned()));
    SourceRow {
        id: Uuid::now_v7(),
        project_id: project,
        name: body.name.trim().to_owned(),
        kind,
        filter,
        enabled: body.enabled,
        paused_reason: None,
        paused_at: None,
        created_by: Some(user),
        created_at: now,
        updated_at: now,
        sample_rate: llm.then(|| body.sample_rate.unwrap_or(DEFAULT_SAMPLE_RATE)),
        daily_cap: llm.then(|| body.daily_cap.unwrap_or(DEFAULT_DAILY_CAP)),
        score_names: body.score_names.map_or_else(Vec::new, trimmed),
        feedback_sources,
        chat_connection_id: body.chat_connection_id,
        chat_model: body.chat_model,
        embed_connection_id: body.embed_connection_id,
        embed_model: body.embed_model,
        embed_dimension: None,
        floor: llm.then(|| body.floor.unwrap_or(DEFAULT_FLOOR)),
        min_issue_size: llm.then(|| body.min_issue_size.unwrap_or(DEFAULT_MIN_ISSUE_SIZE)),
        budget_usd: body.budget_usd,
        budget_period: body.budget_period.map(|period| period.as_str().to_owned()),
        paused_detail: None,
        next_refit_at: llm.then_some(now + FIRST_REFIT_DELAY_NS),
    }
}

/// Score names as stored: trimmed.
fn trimmed(names: Vec<String>) -> Vec<String> {
    names
        .into_iter()
        .map(|name| name.trim().to_owned())
        .collect()
}

/// The filter as stored: trimmed, and absent when blank.
fn normalize_filter(text: Option<String>) -> Option<String> {
    text.map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// The period `[from, to)` of a read: `to` defaults to `now`, and `from` to `default_ns` before
/// `to`.
fn period(
    from: Option<TimeParam>,
    to: Option<TimeParam>,
    now: i64,
    default_ns: i64,
) -> Result<TimeWindow, ApiError> {
    let to = to.map_or(now, TimeParam::ns);
    let from = from.map_or(to.saturating_sub(default_ns), TimeParam::ns);
    Ok(TimeWindow::new(from, to)?)
}

/// The states a list asks for (D-IC-10). `all` is every state but `merged`. Default `open`.
fn parse_states(text: Option<&str>) -> Result<Vec<store::IssueState>, ApiError> {
    let text = text
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or("open");
    let mut states = Vec::new();
    for word in text.split(',').map(str::trim) {
        let named: &[store::IssueState] = match word {
            "open" => &[store::IssueState::Open],
            "resolved" => &[store::IssueState::Resolved],
            "ignored" => &[store::IssueState::Ignored],
            "merged" => &[store::IssueState::Merged],
            "all" => &[
                store::IssueState::Open,
                store::IssueState::Resolved,
                store::IssueState::Ignored,
            ],
            _ => {
                return Err(bad_request_field(
                    "state",
                    "state must be open, resolved, ignored, merged or all",
                ));
            }
        };
        for state in named {
            if !states.contains(state) {
                states.push(*state);
            }
        }
    }
    Ok(states)
}

/// The sort of a list: `last_seen` by default, or `first_seen`.
fn parse_sort(text: Option<&str>) -> Result<IssueSort, ApiError> {
    match text.map(str::trim) {
        None | Some("" | "last_seen") => Ok(IssueSort::LastSeen),
        Some("first_seen") => Ok(IssueSort::FirstSeen),
        Some(_) => Err(bad_request_field(
            "sort",
            "sort must be last_seen or first_seen",
        )),
    }
}

/// The ids a list filters by. Absent or blank is `None` (no filter).
fn parse_ids(text: Option<&str>) -> Result<Option<Vec<Uuid>>, ApiError> {
    let Some(text) = text.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(None);
    };
    let ids = text
        .split(',')
        .map(|part| part.trim().parse::<Uuid>())
        .collect::<Result<Vec<Uuid>, _>>()
        .map_err(|_| bad_request_field("ids", "ids must be issue ids"))?;
    if ids.len() > MAX_IDS {
        return Err(bad_request_field("ids", "at most 100 ids"));
    }
    Ok(Some(ids))
}

/// The sort value of a row, the first half of its cursor.
fn sort_value(row: &store::IssueRow, sort: IssueSort) -> i64 {
    match sort {
        IssueSort::LastSeen => row.last_seen,
        IssueSort::FirstSeen => row.first_seen,
    }
}

/// The name of a sort, as a cursor records it.
fn sort_name(sort: IssueSort) -> &'static str {
    match sort {
        IssueSort::LastSeen => "last_seen",
        IssueSort::FirstSeen => "first_seen",
    }
}

/// The cursor of an issue page: the sort the page was taken with, and the sort value and id of
/// its last row. A cursor is only good for its own sort (D-IC-9).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueCursor {
    sort: String,
    value: i64,
    id: Uuid,
}

/// The opaque cursor of a page taken with `sort`, after the row `(value, id)`.
fn encode_issue_cursor(sort: IssueSort, value: i64, id: Uuid) -> Result<String, ApiError> {
    let cursor = IssueCursor {
        sort: sort_name(sort).to_owned(),
        value,
        id,
    };
    let bytes = serde_json::to_vec(&cursor).map_err(|error| ApiError::Internal(Box::new(error)))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The position of a cursor that was taken with `sort`. A cursor that does not decode, or that
/// was taken with another sort, is a `400` on `cursor`.
fn decode_issue_cursor(text: &str, sort: IssueSort) -> Result<(i64, Uuid), ApiError> {
    let invalid = || bad_request_field("cursor", "invalid cursor");
    let bytes = URL_SAFE_NO_PAD.decode(text).map_err(|_| invalid())?;
    let cursor: IssueCursor = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if cursor.sort != sort_name(sort) {
        return Err(invalid());
    }
    Ok((cursor.value, cursor.id))
}

/// `GET …/issues`: the issues of the project, with counts over the period.
async fn list_issues(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<IssueListParams>,
) -> Result<error::Json<IssuePage>, ApiError> {
    let project = access.project.id;
    let org = access.project.org_id;
    let now = state.clock.now_ns();
    let states = parse_states(params.state.as_deref())?;
    let sort = parse_sort(params.sort.as_deref())?;
    let ids = parse_ids(params.ids.as_deref())?;
    let cursor = params
        .cursor
        .as_deref()
        .map(|text| decode_issue_cursor(text, sort))
        .transpose()?;
    let window = period(params.from, params.to, now, DAY_NS)?;
    let q = params
        .q
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty());
    let limit = params.limit.get();
    let page_len = usize::try_from(limit).unwrap_or(usize::MAX);
    let query = store::IssueListQuery {
        states: &states,
        source: params.source_id,
        q,
        ids: ids.as_deref(),
        sort,
        cursor,
        limit,
    };
    let mut rows = store::list_issues(state.meta.reader(), project, &query).await?;
    // The read returns one row more than a page when a next page exists (D-IC-9 cursor).
    let more = rows.len() > page_len;
    rows.truncate(page_len);
    let next_cursor = match rows.last() {
        Some(row) if more => Some(encode_issue_cursor(sort, sort_value(row, sort), row.id)?),
        _ => None,
    };

    let sources = sources_of(&state, project).await?;
    let mut merged = Vec::with_capacity(rows.len());
    for row in &rows {
        merged.push(store::merged_ids(state.meta.reader(), project, row.id).await?);
    }
    let names: Vec<String> = rows
        .iter()
        .zip(&merged)
        .flat_map(|(row, merged)| names_of(row.id, merged))
        .collect();
    let counts = tallies(&state, org, project, &names, window).await?;
    for list in &mut merged {
        most_occurrences_first(list, &counts);
    }

    let mut data = Vec::with_capacity(rows.len());
    for (row, merged) in rows.iter().zip(merged) {
        let source = sources.get(&row.source_id).ok_or_else(missing_source)?;
        let tally = total(&names_of(row.id, &merged), &counts);
        data.push(issue_json(row, source, merged, tally));
    }
    Ok(error::Json(IssuePage { data, next_cursor }))
}

/// `GET …/issues/{id}`: one issue, with its counts over the last 24 hours.
async fn get_issue(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
) -> Result<error::Json<Issue>, ApiError> {
    let project = access.project.id;
    let row = issue_or_missing(&state, project, id).await?;
    Ok(error::Json(
        read_issue(&state, access.project.org_id, project, row).await?,
    ))
}

/// `PATCH …/issues/{id}`: renames, describes, resolves, ignores or reopens an issue (D-IC-10).
/// Answers the issue as `get` does.
async fn patch_issue(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
    error::Json(body): error::Json<UpdateIssueRequest>,
) -> Result<error::Json<Issue>, ApiError> {
    let user = principal::session_user(&principal)?;
    let project = access.project.id;
    let org = access.project.org_id;
    let now = state.clock.now_ns();
    let user_id = user.id;
    let update = UpdateIssueRequest {
        title: body.title.map(|title| title.trim().to_owned()),
        description: body.description.map(normalize_filter),
        state: body.state,
    };
    check_update(&update)?;
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move { patch_checked(conn, project, id, update, now, user_id).await })
        })
        .await?;
    match outcome {
        IssueWrite::Done { row, changed } => {
            if changed {
                tracing::info!(
                    issue_id = %id,
                    project_id = %project,
                    user_id = %user_id,
                    "issue updated"
                );
            }
            Ok(error::Json(read_issue(&state, org, project, *row).await?))
        }
        IssueWrite::Missing => Err(ApiError::NotFound),
        IssueWrite::Merged => Err(ApiError::Conflict {
            detail: "a merged issue cannot change",
        }),
    }
}

/// `POST …/issues/{id}/merge`: folds the issue into `into`, one level deep (D-IC-9). Answers
/// the target issue.
async fn post_issue_merge(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
    error::Json(body): error::Json<MergeIssueRequest>,
) -> Result<error::Json<Issue>, ApiError> {
    let user = principal::session_user(&principal)?;
    let project = access.project.id;
    let org = access.project.org_id;
    let now = state.clock.now_ns();
    let user_id = user.id;
    let into = body.into;
    if into == id {
        return Err(ApiError::Conflict {
            detail: "an issue cannot merge into itself",
        });
    }
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move { merge_checked(conn, project, id, into, now, user_id).await })
        })
        .await?;
    match outcome {
        MergeWrite::Done(target) => {
            tracing::info!(
                issue_id = %id,
                into = %into,
                project_id = %project,
                user_id = %user_id,
                "issue merged"
            );
            Ok(error::Json(
                read_issue(&state, org, project, *target).await?,
            ))
        }
        MergeWrite::Missing => Err(ApiError::NotFound),
        MergeWrite::Refused(detail) => Err(ApiError::Conflict { detail }),
    }
}

/// What a PATCH transaction did.
enum IssueWrite {
    /// The issue as it is now. `changed` is false when the body changed nothing.
    Done {
        row: Box<store::IssueRow>,
        changed: bool,
    },
    /// The issue is missing.
    Missing,
    /// The issue is merged, and the body would change it.
    Merged,
}

/// What a merge transaction did.
enum MergeWrite {
    /// The target issue after the merge.
    Done(Box<store::IssueRow>),
    /// The issue or the target is missing.
    Missing,
    /// The merge is not allowed (D-IC-9).
    Refused(&'static str),
}

/// Checks a PATCH body before the write: the title and description lengths, and that `state`
/// is not `merged`. Every failure is reported together.
fn check_update(update: &UpdateIssueRequest) -> Result<(), ApiError> {
    let mut errors = Vec::new();
    if update
        .title
        .as_deref()
        .is_some_and(|title| !(1..=MAX_TITLE_CHARS).contains(&title.chars().count()))
    {
        errors.push(field_error("title", "title must be 1 to 200 characters"));
    }
    if update
        .description
        .as_ref()
        .and_then(Option::as_deref)
        .is_some_and(|text| text.chars().count() > MAX_DESCRIPTION_CHARS)
    {
        errors.push(field_error(
            "description",
            "description must be at most 2000 characters",
        ));
    }
    if update.state == Some(IssueState::Merged) {
        errors.push(field_error(
            "state",
            "state must be open, resolved or ignored; merge an issue with the merge operation",
        ));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(bad_request(errors))
    }
}

/// The PATCH's write. The issue is read in the same transaction it changes. A merged issue
/// takes no change (409), and a body that changes nothing writes nothing.
async fn patch_checked(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
    update: UpdateIssueRequest,
    now: i64,
    user: Uuid,
) -> Result<IssueWrite, MetaError> {
    let Some(mut row) = store::get_issue_tx(conn, project, id).await? else {
        return Ok(IssueWrite::Missing);
    };
    let renamed = update
        .title
        .as_ref()
        .is_some_and(|title| *title != row.title);
    let described = update
        .description
        .as_ref()
        .is_some_and(|description| *description != row.description);
    let restated = update
        .state
        .is_some_and(|state| store_state(state) != row.state);
    if row.state == store::IssueState::Merged {
        if renamed || described || restated {
            return Ok(IssueWrite::Merged);
        }
        return Ok(IssueWrite::Done {
            row: Box::new(row),
            changed: false,
        });
    }

    let mut changed = false;
    let mut events = Vec::new();
    if let Some(title) = update.title.filter(|title| *title != row.title) {
        events.push(user_event(
            id,
            store::EventKind::Renamed,
            user,
            Some(row.title.clone()),
            Some(title.clone()),
            now,
        ));
        row.title = title;
        changed = true;
    }
    if let Some(description) = update
        .description
        .filter(|description| *description != row.description)
    {
        row.description = description;
        changed = true;
    }
    if let Some(next) = update
        .state
        .map(store_state)
        .filter(|next| *next != row.state)
    {
        let from = row.state;
        match next {
            store::IssueState::Resolved => {
                row.resolved_at = Some(now);
                row.resolved_by = Some(user);
                row.regressed = false;
            }
            store::IssueState::Open | store::IssueState::Ignored => {
                row.resolved_at = None;
                row.resolved_by = None;
            }
            // `check_update` refuses `merged` before the write; a merge is never a PATCH.
            store::IssueState::Merged => return Ok(IssueWrite::Merged),
        }
        events.push(user_event(
            id,
            store::EventKind::StateChanged,
            user,
            Some(from.as_str().to_owned()),
            Some(next.as_str().to_owned()),
            now,
        ));
        row.state = next;
        changed = true;
    }
    if changed {
        row.updated_at = now;
        store::update_issue(conn, &row).await?;
        for event in &events {
            store::insert_event(conn, event).await?;
        }
    }
    Ok(IssueWrite::Done {
        row: Box::new(row),
        changed,
    })
}

/// The merge's write (D-IC-9). Both issues are read in the same transaction that changes them.
/// The merged issue's own merged issues point at the target, so chains stay one level deep.
async fn merge_checked(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
    into: Uuid,
    now: i64,
    user: Uuid,
) -> Result<MergeWrite, MetaError> {
    let Some(mut issue) = store::get_issue_tx(conn, project, id).await? else {
        return Ok(MergeWrite::Missing);
    };
    let Some(mut target) = store::get_issue_tx(conn, project, into).await? else {
        return Ok(MergeWrite::Missing);
    };
    if issue.source_id != target.source_id {
        return Ok(MergeWrite::Refused(
            "issues merge only within one issue source",
        ));
    }
    if issue.state == store::IssueState::Merged {
        return Ok(MergeWrite::Refused("the issue is already merged"));
    }
    if target.state == store::IssueState::Merged {
        return Ok(MergeWrite::Refused("the target issue is merged"));
    }

    issue.state = store::IssueState::Merged;
    issue.merged_into = Some(into);
    issue.updated_at = now;
    store::update_issue(conn, &issue).await?;
    store::retarget_merged(conn, id, into).await?;

    target.first_seen = target.first_seen.min(issue.first_seen);
    target.last_seen = target.last_seen.max(issue.last_seen);
    // The target's centroid becomes the member-weighted mean of both (D-IC-9). An error issue has
    // no centroid, so its target keeps its own; the counts add up either way.
    let merged = match (target.centroid.as_deref(), issue.centroid.as_deref()) {
        (Some(kept), Some(other)) => {
            match cluster::merge_centroids(kept, target.member_count, other, issue.member_count) {
                Ok(merged) => Some(merged),
                Err(error) => {
                    tracing::warn!(
                        issue_id = %id,
                        target_id = %into,
                        error_class = ?error,
                        "issue centroid could not be merged; the target keeps its centroid"
                    );
                    None
                }
            }
        }
        _ => None,
    };
    if merged.is_some() {
        target.centroid = merged;
    }
    target.member_count = target.member_count.saturating_add(issue.member_count);
    target.updated_at = now;
    store::update_issue(conn, &target).await?;

    store::insert_event(
        conn,
        &user_event(
            id,
            store::EventKind::Merged,
            user,
            None,
            Some(into.to_string()),
            now,
        ),
    )
    .await?;
    store::insert_event(
        conn,
        &user_event(
            into,
            store::EventKind::Merged,
            user,
            Some(id.to_string()),
            None,
            now,
        ),
    )
    .await?;
    Ok(MergeWrite::Done(Box::new(target)))
}

/// An event a user caused, at `at`.
fn user_event(
    issue: Uuid,
    kind: store::EventKind,
    actor: Uuid,
    from: Option<String>,
    to: Option<String>,
    at: i64,
) -> store::EventRow {
    store::EventRow {
        id: Uuid::now_v7(),
        issue_id: issue,
        kind,
        actor_user_id: Some(actor),
        from_value: from,
        to_value: to,
        at,
    }
}

/// The store's state of an API state.
fn store_state(state: IssueState) -> store::IssueState {
    match state {
        IssueState::Open => store::IssueState::Open,
        IssueState::Resolved => store::IssueState::Resolved,
        IssueState::Ignored => store::IssueState::Ignored,
        IssueState::Merged => store::IssueState::Merged,
    }
}

/// The issue as the API shows it: its source, its merged issues and its counts over the last 24
/// hours (the read of `get`).
async fn read_issue(
    state: &AppState,
    org: OrgId,
    project: ProjectId,
    row: store::IssueRow,
) -> Result<Issue, ApiError> {
    let now = state.clock.now_ns();
    let source = store::get_source(state.meta.reader(), project, row.source_id)
        .await?
        .ok_or_else(missing_source)?;
    let mut merged = store::merged_ids(state.meta.reader(), project, row.id).await?;
    let names = names_of(row.id, &merged);
    let window = TimeWindow::new(now.saturating_sub(DAY_NS), now)?;
    let counts = tallies(state, org, project, &names, window).await?;
    most_occurrences_first(&mut merged, &counts);
    Ok(issue_json(&row, &source, merged, total(&names, &counts)))
}

/// `GET …/issues/{id}/occurrences`: the days of the period with occurrences, and the newest
/// examples (D-IC-9, D-IC-11).
async fn get_issue_occurrences(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
    error::Query(params): error::Query<IssueOccurrenceParams>,
) -> Result<error::Json<IssueOccurrences>, ApiError> {
    let project = access.project.id;
    let org = access.project.org_id;
    let now = state.clock.now_ns();
    let window = period(params.from, params.to, now, OCCURRENCE_DEFAULT_PERIOD_NS)?;
    if window.to().saturating_sub(window.from()) > MAX_OCCURRENCE_PERIOD_NS {
        return Err(bad_request_field("from", "the period is at most 90 days"));
    }
    issue_or_missing(&state, project, id).await?;
    let merged = store::merged_ids(state.meta.reader(), project, id).await?;
    let names = names_of(id, &merged);
    let ctx = open_query(&state, org, project, None).await?;

    let counts = name_counts(&ctx, &names, window, Some(DAY_NS)).await?;
    let mut by_day: BTreeMap<i64, Tally> = BTreeMap::new();
    let mut by_name: HashMap<String, Tally> = HashMap::new();
    for row in counts {
        let name = by_name.entry(row.name).or_default();
        name.count = name.count.saturating_add(row.count);
        let day = by_day.entry(row.bucket_start).or_default();
        day.count = day.count.saturating_add(row.count);
        day.users = day.users.max(row.users);
    }
    let days = by_day
        .into_iter()
        .map(|(day, tally)| IssueDay {
            day,
            count: tally.count,
            users: tally.users,
        })
        .collect();

    // The issue, then the merged issues with the most occurrences in the period (D-IC-9).
    let issues = example_issues(id, &merged, &by_name);
    let scored = occurrence_scores(&ctx, project, &issues, EXAMPLES_PER_NAME, None).await?;
    let mut examples: Vec<IssueExample> = scored
        .iter()
        .filter_map(|(issue, score)| example_of(score, *issue))
        .collect();
    examples.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.trace_id.cmp(&a.trace_id))
    });
    examples.truncate(MAX_EXAMPLES);
    Ok(error::Json(IssueOccurrences { days, examples }))
}

/// `GET …/issues/{id}/events`: the newest events of the issue.
async fn get_issue_events(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
) -> Result<error::Json<IssueEventPage>, ApiError> {
    let project = access.project.id;
    issue_or_missing(&state, project, id).await?;
    let events = store::list_events(state.meta.reader(), id, MAX_EVENTS).await?;
    Ok(error::Json(IssueEventPage {
        data: events.into_iter().map(event_json).collect(),
    }))
}

/// The issue of `project` with `id`, or 404.
async fn issue_or_missing(
    state: &AppState,
    project: ProjectId,
    id: Uuid,
) -> Result<store::IssueRow, ApiError> {
    store::get_issue(state.meta.reader(), project, id)
        .await?
        .ok_or(ApiError::NotFound)
}

/// The sources of `project` by id.
async fn sources_of(
    state: &AppState,
    project: ProjectId,
) -> Result<HashMap<Uuid, SourceRow>, ApiError> {
    Ok(store::list_sources(state.meta.reader(), project)
        .await?
        .into_iter()
        .map(|row| (row.id, row))
        .collect())
}

/// An issue whose source is missing. The foreign key makes this impossible, so it is an internal
/// error.
fn missing_source() -> ApiError {
    ApiError::Internal("issue without a source".into())
}

/// The issue as the API shows it, with its merged issues and counts.
fn issue_json(
    row: &store::IssueRow,
    source: &SourceRow,
    merged_from: Vec<Uuid>,
    tally: Tally,
) -> Issue {
    Issue {
        id: row.id,
        project_id: row.project_id.as_uuid(),
        source_id: row.source_id,
        source_name: source.name.clone(),
        kind: kind_of(source.kind),
        score_name: score_name(row.id),
        title: row.title.clone(),
        description: row.description.clone(),
        state: state_of(row.state),
        regressed: row.regressed,
        merged_into: row.merged_into,
        merged_from,
        first_seen: row.first_seen,
        last_seen: row.last_seen,
        resolved_at: row.resolved_at,
        resolved_by: row.resolved_by,
        created_at: row.created_at,
        updated_at: row.updated_at,
        count: tally.count,
        users: tally.users,
    }
}

fn state_of(state: store::IssueState) -> IssueState {
    match state {
        store::IssueState::Open => IssueState::Open,
        store::IssueState::Resolved => IssueState::Resolved,
        store::IssueState::Ignored => IssueState::Ignored,
        store::IssueState::Merged => IssueState::Merged,
    }
}

fn event_json(row: store::EventRow) -> IssueEvent {
    IssueEvent {
        id: row.id,
        kind: row.kind.as_str().to_owned(),
        actor_user_id: row.actor_user_id,
        from_value: row.from_value,
        to_value: row.to_value,
        at: row.at,
    }
}

/// The occurrence as an example, when its target is a trace (occurrences always are).
fn example_of(score: &Score, issue: Uuid) -> Option<IssueExample> {
    let ScoreTarget::Trace { trace_id } = &score.target else {
        return None;
    };
    Some(IssueExample {
        trace_id: trace_id.to_string(),
        span_id: score.metadata.get("cetra.issue.span_id").cloned(),
        created_at: score.created_at,
        reason: score.reason.clone(),
        issue_id: issue,
    })
}

fn bad_request_field(field: &str, message: &str) -> ApiError {
    bad_request(vec![field_error(field, message)])
}

/// Open issues of `id` in `counts`. A source without open issues is absent from the map.
fn open_issues(counts: &HashMap<Uuid, u64>, id: Uuid) -> u64 {
    counts.get(&id).copied().unwrap_or_default()
}

/// The LLM figures of an LLM source: its candidates of the last 24 hours and the spend of the
/// current UTC day (D-IC-12). `None` for an `error` source.
struct LlmFigures {
    candidates: IssueCandidateCounts,
    spent_today_usd: f64,
}

async fn llm_figures(
    state: &AppState,
    row: &SourceRow,
    now: i64,
) -> Result<Option<LlmFigures>, ApiError> {
    if !row.kind.is_llm() {
        return Ok(None);
    }
    let counts = store::candidate_counts(state.meta.reader(), row.id, now - DAY_NS).await?;
    let candidates = IssueCandidateCounts {
        queued: counts.queued,
        pending: counts.pending,
        assigned: counts.assigned,
        dropped: counts.dropped,
        failed: counts.failed,
        skipped: counts.skipped,
    };
    let today = day_start(now);
    let spent_today_usd =
        store::spent_between(state.meta.reader(), row.id, today, today + DAY_NS).await?;
    Ok(Some(LlmFigures {
        candidates,
        spent_today_usd,
    }))
}

/// The source as the API shows it. The LLM fields are set for the LLM kinds, and the score
/// names for `score` and `feedback`, the feedback sources for `feedback`.
fn source_json(
    row: &SourceRow,
    open_issues: u64,
    channel_ids: Vec<Uuid>,
    figures: Option<&LlmFigures>,
) -> IssueSource {
    IssueSource {
        id: row.id,
        project_id: row.project_id.as_uuid(),
        name: row.name.clone(),
        kind: kind_of(row.kind),
        filter: row.filter.clone(),
        enabled: row.enabled,
        channel_ids,
        paused_reason: row.paused_reason.clone(),
        created_by: row.created_by,
        created_at: row.created_at,
        updated_at: row.updated_at,
        open_issues,
        sample_rate: row.sample_rate,
        daily_cap: row.daily_cap,
        score_names: matches!(row.kind, SourceKind::Score | SourceKind::Feedback)
            .then(|| row.score_names.clone()),
        feedback_sources: (row.kind == SourceKind::Feedback).then(|| row.feedback_sources.clone()),
        chat_connection_id: row.chat_connection_id,
        chat_model: row.chat_model.clone(),
        embed_connection_id: row.embed_connection_id,
        embed_model: row.embed_model.clone(),
        embed_dimension: row.embed_dimension,
        floor: row.floor,
        min_issue_size: row.min_issue_size,
        budget_usd: row.budget_usd,
        budget_period: row
            .budget_period
            .as_deref()
            .and_then(BudgetPeriod::from_name),
        paused_detail: row.paused_detail.clone(),
        next_refit_at: row.next_refit_at,
        candidates_24h: figures.map(|figures| figures.candidates),
        spent_today_usd: figures.map(|figures| figures.spent_today_usd),
    }
}

/// A refit as the API shows it.
fn refit_json(row: RefitRow) -> IssueRefit {
    IssueRefit {
        id: row.id,
        trigger: row.trigger.to_owned(),
        started_at: row.started_at,
        finished_at: row.finished_at,
        pending: row.pending,
        clusters: row.clusters,
        created: row.created,
        joined: row.joined,
        outliers: row.outliers,
        cost_usd: row.cost_usd,
        error_class: row.error_class,
    }
}

fn kind_of(kind: SourceKind) -> IssueSourceKind {
    match kind {
        SourceKind::Error => IssueSourceKind::Error,
        SourceKind::Score => IssueSourceKind::Score,
        SourceKind::Feedback => IssueSourceKind::Feedback,
        SourceKind::Topic => IssueSourceKind::Topic,
    }
}

fn source_kind(kind: IssueSourceKind) -> SourceKind {
    match kind {
        IssueSourceKind::Error => SourceKind::Error,
        IssueSourceKind::Score => SourceKind::Score,
        IssueSourceKind::Feedback => SourceKind::Feedback,
        IssueSourceKind::Topic => SourceKind::Topic,
    }
}

fn bad_request(errors: Vec<FieldError>) -> ApiError {
    ApiError::BadRequest {
        detail: None,
        errors,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use axum::http::{Method, header};
    use cetra_core::ids::TraceId;
    use cetra_core::score::ScoreValue;
    use serde_json::{Value, json};

    use super::*;
    use crate::api::common::ProblemCode;
    use crate::api::scores::ScoreCreate;
    use crate::auth::roles::OrgRole;
    use crate::meta::connections::{ConnectionFields, insert_connection};
    use crate::meta::issues::{
        CandidateRow, CandidateStatus, CandidateTarget, EventKind, EventRow, IssueRow, IssueState,
        RefitRow,
    };
    use crate::scores::{self as score_writes, Writer};
    use crate::test_support::issues::{issue_row, put_channel, put_issue};
    use crate::test_support::{Seeded, TestApp, TestResponse, set_limits};
    use cetra_storage::MAX_COUNT_NAMES;

    const HOUR_NS: i64 = 3_600_000_000_000;
    const TRACE_A: &str = "a1000000000000000000000000000001";
    const TRACE_B: &str = "a1000000000000000000000000000002";
    const TRACE_C: &str = "a1000000000000000000000000000003";

    fn path(project: ProjectId) -> String {
        format!("/api/v1/projects/{project}/issue-sources")
    }

    fn source_body(name: &str) -> Value {
        json!({
            "name": name,
            "kind": "error",
            "filter": "status = 'error'",
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

    #[tokio::test]
    async fn create_list_get_update_delete_round_trip() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, &source_body("  Errors  ")).await;
        assert_eq!(created["name"], "Errors");
        assert_eq!(created["kind"], "error");
        assert_eq!(created["filter"], "status = 'error'");
        assert_eq!(created["enabled"], true);
        assert!(created["paused_reason"].is_null());
        assert_eq!(created["project_id"], seeded.project.id.to_string());
        assert_eq!(created["created_by"], seeded.user_id.to_string());
        assert_eq!(created["open_issues"], 0);
        let id = id_of(&created);

        let read = app
            .request(Method::GET, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(read.status, 200, "{}", text(&read));
        assert_eq!(read.json::<Value>(), created);

        let page = app
            .request(Method::GET, &path(seeded.project.id))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await
            .json::<Value>();
        assert_eq!(page["data"], json!([created]));
        assert_eq!(page["org_active_sources"], 1);
        assert!(page["next_cursor"].is_null());

        let renamed = patch(&app, &seeded, id, &json!({"name": "Renamed", "filter": ""})).await;
        assert_eq!(renamed.status, 200, "{}", text(&renamed));
        let renamed: Value = renamed.json();
        assert_eq!(renamed["name"], "Renamed");
        assert!(renamed["filter"].is_null());
        assert_eq!(renamed["enabled"], true);

        let deleted = app
            .request(Method::DELETE, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(deleted.status, 204, "{}", text(&deleted));
        let gone = app
            .request(Method::GET, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(gone.status, 404, "{}", text(&gone));
    }

    #[tokio::test]
    async fn create_rejects_a_bad_filter_with_its_position() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let mut body = source_body("Bad");
        body["filter"] = json!("error_count >");
        let response = create(&app, &seeded, &body).await;
        assert_eq!(response.status, 400, "{}", text(&response));
        let problem = response.problem();
        assert_eq!(problem.code, ProblemCode::InvalidFilter);
        assert_eq!(problem.errors[0].field, "filter");
        assert!(problem.errors[0].position.is_some());
    }

    #[tokio::test]
    async fn create_rejects_a_traces_only_field() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        // `error_count` is a trace column; the spans view does not have it.
        let mut body = source_body("Traces only");
        body["filter"] = json!("error_count > 0");
        let response = create(&app, &seeded, &body).await;
        assert_eq!(response.status, 400, "{}", text(&response));
        assert_eq!(response.problem().code, ProblemCode::InvalidFilter);
    }

    #[tokio::test]
    async fn free_plan_allows_one_enabled_source() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2025-12-01T00:00:00Z",
            &json!({"max_active_issue_sources": 1}),
        );
        create_ok(&app, &seeded, &source_body("First")).await;

        let refused = create(&app, &seeded, &source_body("Second")).await;
        assert_eq!(refused.status, 403, "{}", text(&refused));
        let problem = refused.problem();
        assert_eq!(problem.code, ProblemCode::PlanLimit);
        assert_eq!(
            problem.detail.as_deref(),
            Some("Your plan allows 1 active issue source.")
        );

        let mut disabled = source_body("Second");
        disabled["enabled"] = json!(false);
        let second = create_ok(&app, &seeded, &disabled).await;
        let enabling = patch(&app, &seeded, id_of(&second), &json!({"enabled": true})).await;
        assert_eq!(enabling.status, 403, "{}", text(&enabling));
        assert_eq!(enabling.problem().code, ProblemCode::PlanLimit);
    }

    #[tokio::test]
    async fn viewer_can_list_but_not_create() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, &source_body("Errors")).await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let refused = app
            .request(Method::POST, &path(seeded.project.id))
            .header(header::COOKIE.as_str(), &viewer.cookie)
            .json(&source_body("Mine"))
            .send()
            .await;
        assert_eq!(refused.status, 403, "{}", text(&refused));
        let listed = app
            .request(Method::GET, &path(seeded.project.id))
            .header(header::COOKIE.as_str(), &viewer.cookie)
            .send()
            .await;
        assert_eq!(listed.status, 200, "{}", text(&listed));
    }

    #[tokio::test]
    async fn delete_cascades_issues() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, &source_body("Errors")).await;
        let source = id_of(&created);
        let (project, now) = (seeded.project.id, app.state.clock.now_ns());
        let issue = Uuid::now_v7();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    store::insert_issue(
                        conn,
                        &IssueRow {
                            id: issue,
                            project_id: project,
                            source_id: source,
                            fingerprint: Some("ab".to_owned()),
                            title: "boom".to_owned(),
                            description: None,
                            state: IssueState::Open,
                            regressed: false,
                            merged_into: None,
                            first_seen: now,
                            last_seen: now,
                            resolved_at: None,
                            resolved_by: None,
                            created_at: now,
                            updated_at: now,
                            centroid: None,
                            member_count: 0,
                        },
                    )
                    .await
                })
            })
            .await
            .unwrap();
        let read = app
            .request(Method::GET, &format!("{}/{source}", path(project)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await
            .json::<Value>();
        assert_eq!(read["open_issues"], 1);

        let deleted = app
            .request(Method::DELETE, &format!("{}/{source}", path(project)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(deleted.status, 204, "{}", text(&deleted));
        let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM issues WHERE source_id = ?")
            .bind(source.to_string())
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap();
        assert_eq!(left, 0);
    }

    #[tokio::test]
    async fn duplicate_name_is_409() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        create_ok(&app, &seeded, &source_body("Errors")).await;
        let response = create(&app, &seeded, &source_body("Errors")).await;
        assert_eq!(response.status, 409, "{}", text(&response));
        assert_eq!(response.problem().code, ProblemCode::Conflict);
    }

    #[tokio::test]
    async fn kind_cannot_be_patched() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, &source_body("Errors")).await;
        let response = patch(&app, &seeded, id_of(&created), &json!({"kind": "error"})).await;
        assert_eq!(response.status, 400, "{}", text(&response));
    }

    #[tokio::test]
    async fn issue_source_update_records_field_names() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let id = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let patched = patch(
            &app,
            &seeded,
            id,
            &json!({"name": "canary-source-name", "enabled": false}),
        )
        .await;
        assert_eq!(patched.status, 200, "{}", text(&patched));

        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT target_id, actor_kind, details FROM audit_events \
             WHERE action = 'issue_source.updated'",
        )
        .fetch_all(app.state.meta.reader())
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        let (target, actor_kind, details) = &rows[0];
        assert_eq!(target, &id.to_string());
        assert_eq!(actor_kind, "user");
        assert_eq!(details, r#"{"fields":["name","enabled"]}"#);
        assert!(!details.contains("canary-source-name"), "{details}");
    }

    fn issues(project: ProjectId) -> String {
        format!("/api/v1/projects/{project}/issues")
    }

    async fn get_issues(app: &TestApp, seeded: &Seeded, suffix: &str) -> TestResponse {
        app.request(
            Method::GET,
            &format!("{}{suffix}", issues(seeded.project.id)),
        )
        .header(header::COOKIE.as_str(), &seeded.cookie)
        .send()
        .await
    }

    /// Writes one occurrence of `issue` on `trace`, `hours_ago` before now, as the issue writer
    /// does (D-IC-18).
    async fn occurrence(
        app: &TestApp,
        seeded: &Seeded,
        source: Uuid,
        issue: Uuid,
        trace: &str,
        hours_ago: i64,
        user: Option<&str>,
    ) {
        let trace_id: TraceId = trace.parse().unwrap();
        let at = app.state.clock.now_ns() - hours_ago * HOUR_NS;
        let item = ScoreCreate {
            score_id: Some(score_writes::occurrence_id(
                seeded.project.id,
                trace_id,
                issue,
                at,
            )),
            target: ScoreTarget::Trace { trace_id },
            session_id: None,
            user_id: user.map(str::to_owned),
            name: format!("issue:{issue}"),
            value: ScoreValue::Boolean(true),
            reason: Some("boom".to_owned()),
            source: None,
            metadata: BTreeMap::from([(
                "cetra.issue.span_id".to_owned(),
                "b100000000000012".to_owned(),
            )]),
            experiment_id: None,
            dataset_item_id: None,
            queue_id: None,
        };
        let written = score_writes::write(
            &app.state,
            &seeded.project,
            Writer::Issue { source_id: source },
            vec![item],
        )
        .await;
        assert!(written.is_ok(), "occurrence write refused");
    }

    /// The ids of the rows of an issue page, in order.
    fn ids_in(page: &Value) -> Vec<String> {
        page["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn list_defaults_to_open_and_counts_occurrences() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let source = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let now = app.state.clock.now_ns();
        let open = Uuid::now_v7();
        let resolved = Uuid::now_v7();
        put_issue(
            &app,
            issue_row(
                project,
                source,
                open,
                "ValueError: parse",
                IssueState::Open,
                now,
            ),
        )
        .await;
        put_issue(
            &app,
            issue_row(
                project,
                source,
                resolved,
                "KeyError: x",
                IssueState::Resolved,
                now,
            ),
        )
        .await;
        occurrence(&app, &seeded, source, open, TRACE_A, 2, Some("u1")).await;
        occurrence(&app, &seeded, source, open, TRACE_B, 1, Some("u2")).await;

        let response = get_issues(&app, &seeded, "").await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let page: Value = response.json();
        assert_eq!(ids_in(&page), vec![open.to_string()]);
        let row = &page["data"][0];
        assert_eq!(row["state"], "open");
        assert_eq!(row["source_name"], "Errors");
        assert_eq!(row["kind"], "error");
        assert_eq!(row["score_name"], format!("issue:{open}"));
        assert_eq!(row["count"], 2);
        assert_eq!(row["users"], 2);
        assert_eq!(row["merged_from"], json!([]));
        assert!(row.get("resolved_at").is_none(), "omitted until resolved");
        assert!(page["next_cursor"].is_null());
    }

    #[tokio::test]
    async fn list_filters_state_source_and_q() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let first = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let mut other = source_body("Other");
        other["enabled"] = json!(false);
        let second = id_of(&create_ok(&app, &seeded, &other).await);
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let c = Uuid::now_v7();
        put_issue(
            &app,
            issue_row(
                project,
                first,
                a,
                "ValueError: parse",
                IssueState::Open,
                now - 3 * HOUR_NS,
            ),
        )
        .await;
        put_issue(
            &app,
            issue_row(
                project,
                first,
                b,
                "KeyError: x",
                IssueState::Resolved,
                now - 2 * HOUR_NS,
            ),
        )
        .await;
        put_issue(
            &app,
            issue_row(
                project,
                second,
                c,
                "ValueError: other",
                IssueState::Open,
                now - HOUR_NS,
            ),
        )
        .await;

        let all_open = get_issues(&app, &seeded, "").await.json::<Value>();
        assert_eq!(ids_in(&all_open), vec![c.to_string(), a.to_string()]);

        let resolved = get_issues(&app, &seeded, "?state=resolved")
            .await
            .json::<Value>();
        assert_eq!(ids_in(&resolved), vec![b.to_string()]);

        let every = get_issues(&app, &seeded, "?state=all")
            .await
            .json::<Value>();
        assert_eq!(
            ids_in(&every),
            vec![c.to_string(), b.to_string(), a.to_string()]
        );

        let by_source = get_issues(&app, &seeded, &format!("?state=all&source_id={second}"))
            .await
            .json::<Value>();
        assert_eq!(ids_in(&by_source), vec![c.to_string()]);

        let searched = get_issues(&app, &seeded, "?state=all&q=valueERROR")
            .await
            .json::<Value>();
        assert_eq!(ids_in(&searched), vec![c.to_string(), a.to_string()]);

        let oldest_first = get_issues(&app, &seeded, "?state=all&sort=first_seen")
            .await
            .json::<Value>();
        assert_eq!(ids_in(&oldest_first).len(), 3);

        let bad = get_issues(&app, &seeded, "?state=bogus").await;
        assert_eq!(bad.status, 400, "{}", text(&bad));
        assert_eq!(bad.problem().errors[0].field, "state");
    }

    #[tokio::test]
    async fn list_pages_with_a_cursor() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let source = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let x = Uuid::now_v7();
        let y = Uuid::now_v7();
        let z = Uuid::now_v7();
        put_issue(
            &app,
            issue_row(
                project,
                source,
                x,
                "one",
                IssueState::Open,
                now - 3 * HOUR_NS,
            ),
        )
        .await;
        put_issue(
            &app,
            issue_row(
                project,
                source,
                y,
                "two",
                IssueState::Open,
                now - 2 * HOUR_NS,
            ),
        )
        .await;
        put_issue(
            &app,
            issue_row(project, source, z, "three", IssueState::Open, now - HOUR_NS),
        )
        .await;

        let first = get_issues(&app, &seeded, "?limit=2").await.json::<Value>();
        assert_eq!(ids_in(&first), vec![z.to_string(), y.to_string()]);
        let cursor = first["next_cursor"].as_str().expect("a next cursor");
        let second = get_issues(&app, &seeded, &format!("?limit=2&cursor={cursor}"))
            .await
            .json::<Value>();
        assert_eq!(ids_in(&second), vec![x.to_string()]);
        assert!(second["next_cursor"].is_null());

        let bad = get_issues(&app, &seeded, "?cursor=x").await;
        assert_eq!(bad.status, 400, "{}", text(&bad));
        assert_eq!(bad.problem().errors[0].field, "cursor");
    }

    #[tokio::test]
    async fn a_cursor_is_good_for_its_own_sort_only() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let source = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        // Oldest first seen, but newest last seen, so the two sorts order the rows differently.
        for (title, first, last) in [
            ("one", now - 3 * HOUR_NS, now - HOUR_NS),
            ("two", now - 2 * HOUR_NS, now - 2 * HOUR_NS),
            ("three", now - HOUR_NS, now - 3 * HOUR_NS),
        ] {
            let mut row = issue_row(
                project,
                source,
                Uuid::now_v7(),
                title,
                IssueState::Open,
                first,
            );
            row.last_seen = last;
            put_issue(&app, row).await;
        }

        let first = get_issues(&app, &seeded, "?limit=2").await.json::<Value>();
        let cursor = first["next_cursor"].as_str().expect("a next cursor");
        let refused = get_issues(
            &app,
            &seeded,
            &format!("?limit=2&sort=first_seen&cursor={cursor}"),
        )
        .await;
        assert_eq!(refused.status, 400, "{}", text(&refused));
        assert_eq!(refused.problem().errors[0].field, "cursor");

        let first_sort = get_issues(&app, &seeded, "?limit=1&sort=first_seen")
            .await
            .json::<Value>();
        let cursor = first_sort["next_cursor"].as_str().expect("a next cursor");
        let refused = get_issues(&app, &seeded, &format!("?limit=1&cursor={cursor}")).await;
        assert_eq!(refused.status, 400, "{}", text(&refused));
        assert_eq!(refused.problem().errors[0].field, "cursor");
    }

    #[tokio::test]
    async fn list_by_ids_and_rejects_101_ids() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let source = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        put_issue(
            &app,
            issue_row(project, source, a, "one", IssueState::Open, now),
        )
        .await;
        put_issue(
            &app,
            issue_row(project, source, b, "two", IssueState::Open, now),
        )
        .await;

        let one = get_issues(&app, &seeded, &format!("?ids={a}"))
            .await
            .json::<Value>();
        assert_eq!(ids_in(&one), vec![a.to_string()]);
        let blank = get_issues(&app, &seeded, "?ids=").await.json::<Value>();
        assert_eq!(ids_in(&blank).len(), 2);

        let many = (0..101)
            .map(|_| Uuid::now_v7().to_string())
            .collect::<Vec<_>>()
            .join(",");
        let refused = get_issues(&app, &seeded, &format!("?ids={many}")).await;
        assert_eq!(refused.status, 400, "{}", text(&refused));
        assert_eq!(refused.problem().errors[0].field, "ids");
    }

    #[tokio::test]
    async fn counts_include_merged_issues() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let source = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let target = Uuid::now_v7();
        let merged = Uuid::now_v7();
        put_issue(
            &app,
            issue_row(project, source, target, "target", IssueState::Open, now),
        )
        .await;
        put_issue(
            &app,
            IssueRow {
                merged_into: Some(target),
                ..issue_row(project, source, merged, "merged", IssueState::Merged, now)
            },
        )
        .await;
        occurrence(&app, &seeded, source, target, TRACE_A, 1, Some("u1")).await;
        occurrence(&app, &seeded, source, merged, TRACE_B, 2, Some("u2")).await;
        occurrence(&app, &seeded, source, merged, TRACE_C, 3, Some("u3")).await;

        let open = get_issues(&app, &seeded, "").await.json::<Value>();
        assert_eq!(ids_in(&open), vec![target.to_string()]);
        assert_eq!(open["data"][0]["count"], 3);
        assert_eq!(open["data"][0]["users"], 2);
        assert_eq!(open["data"][0]["merged_from"], json!([merged.to_string()]));

        let shown = get_issues(&app, &seeded, "?state=merged")
            .await
            .json::<Value>();
        assert_eq!(ids_in(&shown), vec![merged.to_string()]);
        assert_eq!(shown["data"][0]["count"], 2);
        assert_eq!(shown["data"][0]["merged_into"], target.to_string());

        let read = get_issues(&app, &seeded, &format!("/{target}"))
            .await
            .json::<Value>();
        assert_eq!(read["count"], 3);
        assert_eq!(read["merged_from"], json!([merged.to_string()]));
    }

    #[tokio::test]
    async fn get_counts_the_last_day_and_rejects_unknown_ids() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let source = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let issue = Uuid::now_v7();
        put_issue(
            &app,
            issue_row(project, source, issue, "one", IssueState::Open, now),
        )
        .await;
        occurrence(&app, &seeded, source, issue, TRACE_A, 30, None).await;
        occurrence(&app, &seeded, source, issue, TRACE_B, 1, None).await;

        let read = get_issues(&app, &seeded, &format!("/{issue}")).await;
        assert_eq!(read.status, 200, "{}", text(&read));
        assert_eq!(read.json::<Value>()["count"], 1);

        let missing = get_issues(&app, &seeded, &format!("/{}", Uuid::now_v7())).await;
        assert_eq!(missing.status, 404, "{}", text(&missing));
    }

    #[tokio::test]
    async fn occurrences_days_and_examples() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let source = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let issue = Uuid::now_v7();
        put_issue(
            &app,
            issue_row(project, source, issue, "one", IssueState::Open, now),
        )
        .await;
        occurrence(&app, &seeded, source, issue, TRACE_A, 25, Some("u1")).await;
        occurrence(&app, &seeded, source, issue, TRACE_B, 1, Some("u2")).await;
        let path = format!("{}/{issue}/occurrences", issues(project));

        let response = app
            .request(Method::GET, &path)
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let body: Value = response.json();
        let days = body["days"].as_array().unwrap();
        assert_eq!(days.len(), 2, "one entry per day");
        assert_eq!(days[0]["count"], 1);
        assert_eq!(days[1]["count"], 1);
        assert!(days[0]["day"].as_str().unwrap() < days[1]["day"].as_str().unwrap());
        let examples = body["examples"].as_array().unwrap();
        assert_eq!(examples.len(), 2);
        assert_eq!(examples[0]["trace_id"], TRACE_B);
        assert_eq!(examples[0]["span_id"], "b100000000000012");
        assert_eq!(examples[0]["reason"], "boom");
        assert_eq!(examples[0]["issue_id"], issue.to_string());
        assert_eq!(examples[1]["trace_id"], TRACE_A);

        let too_long = app
            .request(Method::GET, &format!("{path}?from=0"))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(too_long.status, 400, "{}", text(&too_long));
        assert_eq!(too_long.problem().errors[0].field, "from");
    }

    #[tokio::test]
    async fn events_are_newest_first() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let source = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let issue = Uuid::now_v7();
        put_issue(
            &app,
            issue_row(project, source, issue, "one", IssueState::Open, now),
        )
        .await;
        let created = Uuid::now_v7();
        let renamed = Uuid::now_v7();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    store::insert_event(
                        conn,
                        &EventRow {
                            id: created,
                            issue_id: issue,
                            kind: EventKind::Created,
                            actor_user_id: None,
                            from_value: None,
                            to_value: None,
                            at: now - 2 * HOUR_NS,
                        },
                    )
                    .await
                })
            })
            .await
            .unwrap();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    store::insert_event(
                        conn,
                        &EventRow {
                            id: renamed,
                            issue_id: issue,
                            kind: EventKind::Renamed,
                            actor_user_id: Some(seeded.user_id),
                            from_value: Some("old".to_owned()),
                            to_value: Some("new".to_owned()),
                            at: now - HOUR_NS,
                        },
                    )
                    .await
                })
            })
            .await
            .unwrap();

        let response = app
            .request(Method::GET, &format!("{}/{issue}/events", issues(project)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let body: Value = response.json();
        let data = body["data"].as_array().unwrap();
        assert_eq!(data[0]["id"], renamed.to_string());
        assert_eq!(data[0]["kind"], "renamed");
        assert_eq!(data[0]["from_value"], "old");
        assert_eq!(data[1]["id"], created.to_string());
        assert_eq!(data[1]["kind"], "created");
        assert!(data[1]["actor_user_id"].is_null());
    }

    #[tokio::test]
    async fn viewer_can_read_issues() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let listed = app
            .request(Method::GET, &issues(seeded.project.id))
            .header(header::COOKIE.as_str(), &viewer.cookie)
            .send()
            .await;
        assert_eq!(listed.status, 200, "{}", text(&listed));
    }

    fn issue_url(project: ProjectId, id: Uuid) -> String {
        format!("{}/{id}", issues(project))
    }

    async fn patch_issue_request(
        app: &TestApp,
        seeded: &Seeded,
        id: Uuid,
        body: &Value,
    ) -> TestResponse {
        app.request(Method::PATCH, &issue_url(seeded.project.id, id))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(body)
            .send()
            .await
    }

    async fn merge_request(app: &TestApp, seeded: &Seeded, id: Uuid, into: Uuid) -> TestResponse {
        app.request(
            Method::POST,
            &format!("{}/merge", issue_url(seeded.project.id, id)),
        )
        .header(header::COOKIE.as_str(), &seeded.cookie)
        .json(&json!({ "into": into }))
        .send()
        .await
    }

    /// The events of `id`, newest first.
    async fn events_of(app: &TestApp, seeded: &Seeded, id: Uuid) -> Vec<Value> {
        let response = app
            .request(
                Method::GET,
                &format!("{}/events", issue_url(seeded.project.id, id)),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        response.json::<Value>()["data"].as_array().unwrap().clone()
    }

    #[tokio::test]
    async fn patch_resolve_records_an_event_and_sets_resolved_at() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let source = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let now = app.state.clock.now_ns();
        let issue = Uuid::now_v7();
        put_issue(
            &app,
            issue_row(project, source, issue, "boom", IssueState::Open, now),
        )
        .await;

        let resolved =
            patch_issue_request(&app, &seeded, issue, &json!({"state": "resolved"})).await;
        assert_eq!(resolved.status, 200, "{}", text(&resolved));
        let body: Value = resolved.json();
        assert_eq!(body["state"], "resolved");
        assert_eq!(body["resolved_by"], seeded.user_id.to_string());
        assert!(body["resolved_at"].is_string());
        let events = events_of(&app, &seeded, issue).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["kind"], "state_changed");
        assert_eq!(events[0]["from_value"], "open");
        assert_eq!(events[0]["to_value"], "resolved");
        assert_eq!(events[0]["actor_user_id"], seeded.user_id.to_string());

        // The same state again changes nothing and writes no event.
        let again = patch_issue_request(&app, &seeded, issue, &json!({"state": "resolved"})).await;
        assert_eq!(again.status, 200, "{}", text(&again));
        assert_eq!(events_of(&app, &seeded, issue).await.len(), 1);
    }

    #[tokio::test]
    async fn patch_reopen_clears_resolved_at() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let source = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let now = app.state.clock.now_ns();
        let issue = Uuid::now_v7();
        put_issue(
            &app,
            issue_row(project, source, issue, "boom", IssueState::Open, now),
        )
        .await;
        let resolved =
            patch_issue_request(&app, &seeded, issue, &json!({"state": "resolved"})).await;
        assert_eq!(resolved.status, 200, "{}", text(&resolved));

        let reopened = patch_issue_request(&app, &seeded, issue, &json!({"state": "open"})).await;
        assert_eq!(reopened.status, 200, "{}", text(&reopened));
        let body: Value = reopened.json();
        assert_eq!(body["state"], "open");
        assert!(body.get("resolved_at").is_none(), "omitted once reopened");
        assert!(body["resolved_by"].is_null());
        let events = events_of(&app, &seeded, issue).await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["from_value"], "resolved");
        assert_eq!(events[0]["to_value"], "open");
    }

    #[tokio::test]
    async fn patch_title_records_renamed_and_description_records_nothing() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let source = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let now = app.state.clock.now_ns();
        let issue = Uuid::now_v7();
        put_issue(
            &app,
            issue_row(project, source, issue, "boom", IssueState::Open, now),
        )
        .await;

        let renamed =
            patch_issue_request(&app, &seeded, issue, &json!({"title": "  Renamed  "})).await;
        assert_eq!(renamed.status, 200, "{}", text(&renamed));
        assert_eq!(renamed.json::<Value>()["title"], "Renamed");
        let events = events_of(&app, &seeded, issue).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["kind"], "renamed");
        assert_eq!(events[0]["from_value"], "boom");
        assert_eq!(events[0]["to_value"], "Renamed");

        let described =
            patch_issue_request(&app, &seeded, issue, &json!({"description": "Seen twice"})).await;
        assert_eq!(described.status, 200, "{}", text(&described));
        assert_eq!(described.json::<Value>()["description"], "Seen twice");
        let cleared =
            patch_issue_request(&app, &seeded, issue, &json!({"description": null})).await;
        assert_eq!(cleared.status, 200, "{}", text(&cleared));
        assert!(cleared.json::<Value>()["description"].is_null());
        assert_eq!(events_of(&app, &seeded, issue).await.len(), 1);

        let unknown =
            patch_issue_request(&app, &seeded, Uuid::now_v7(), &json!({"title": "x"})).await;
        assert_eq!(unknown.status, 404, "{}", text(&unknown));
    }

    #[tokio::test]
    async fn patch_refuses_merged_state_and_blank_title() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let source = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let now = app.state.clock.now_ns();
        let issue = Uuid::now_v7();
        put_issue(
            &app,
            issue_row(project, source, issue, "boom", IssueState::Open, now),
        )
        .await;

        let merged = patch_issue_request(&app, &seeded, issue, &json!({"state": "merged"})).await;
        assert_eq!(merged.status, 400, "{}", text(&merged));
        assert_eq!(merged.problem().errors[0].field, "state");

        let blank = patch_issue_request(&app, &seeded, issue, &json!({"title": "   "})).await;
        assert_eq!(blank.status, 400, "{}", text(&blank));
        assert_eq!(blank.problem().errors[0].field, "title");

        let unknown_field =
            patch_issue_request(&app, &seeded, issue, &json!({"kind": "error"})).await;
        assert_eq!(unknown_field.status, 400, "{}", text(&unknown_field));
    }

    #[tokio::test]
    async fn patch_of_a_merged_issue_is_409() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let source = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let now = app.state.clock.now_ns();
        let target = Uuid::now_v7();
        let merged = Uuid::now_v7();
        put_issue(
            &app,
            issue_row(project, source, target, "target", IssueState::Open, now),
        )
        .await;
        put_issue(
            &app,
            issue_row(project, source, merged, "merged", IssueState::Open, now),
        )
        .await;
        let folded = merge_request(&app, &seeded, merged, target).await;
        assert_eq!(folded.status, 200, "{}", text(&folded));

        let renamed = patch_issue_request(&app, &seeded, merged, &json!({"title": "Other"})).await;
        assert_eq!(renamed.status, 409, "{}", text(&renamed));
        assert_eq!(renamed.problem().code, ProblemCode::Conflict);
        let reopened = patch_issue_request(&app, &seeded, merged, &json!({"state": "open"})).await;
        assert_eq!(reopened.status, 409, "{}", text(&reopened));

        let target_renamed =
            patch_issue_request(&app, &seeded, target, &json!({"title": "Renamed"})).await;
        assert_eq!(target_renamed.status, 200, "{}", text(&target_renamed));
    }

    #[tokio::test]
    async fn merge_moves_counts_to_the_target() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let source = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let now = app.state.clock.now_ns();
        let target = Uuid::now_v7();
        let merged = Uuid::now_v7();
        put_issue(
            &app,
            issue_row(project, source, target, "target", IssueState::Open, now),
        )
        .await;
        put_issue(
            &app,
            issue_row(project, source, merged, "merged", IssueState::Open, now),
        )
        .await;
        occurrence(&app, &seeded, source, target, TRACE_A, 1, Some("u1")).await;
        occurrence(&app, &seeded, source, merged, TRACE_B, 2, Some("u2")).await;
        occurrence(&app, &seeded, source, merged, TRACE_C, 3, Some("u3")).await;

        let response = merge_request(&app, &seeded, merged, target).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let read: Value = response.json();
        assert_eq!(read["id"], target.to_string());
        assert_eq!(read["count"], 3);
        assert_eq!(read["users"], 2);
        assert_eq!(read["merged_from"], json!([merged.to_string()]));

        let shown = get_issues(&app, &seeded, &format!("/{merged}"))
            .await
            .json::<Value>();
        assert_eq!(shown["state"], "merged");
        assert_eq!(shown["merged_into"], target.to_string());

        let open = get_issues(&app, &seeded, "").await.json::<Value>();
        assert_eq!(ids_in(&open), vec![target.to_string()]);
        assert_eq!(open["data"][0]["count"], 3);

        let merged_events = events_of(&app, &seeded, merged).await;
        assert_eq!(merged_events[0]["kind"], "merged");
        assert_eq!(merged_events[0]["to_value"], target.to_string());
        let target_events = events_of(&app, &seeded, target).await;
        assert_eq!(target_events[0]["kind"], "merged");
        assert_eq!(target_events[0]["from_value"], merged.to_string());
    }

    #[tokio::test]
    async fn merge_flattens_chains() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let source = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let now = app.state.clock.now_ns();
        let (a, b, c) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        for (id, title) in [(a, "a"), (b, "b"), (c, "c")] {
            put_issue(
                &app,
                issue_row(project, source, id, title, IssueState::Open, now),
            )
            .await;
        }

        let first = merge_request(&app, &seeded, a, b).await;
        assert_eq!(first.status, 200, "{}", text(&first));
        let second = merge_request(&app, &seeded, b, c).await;
        assert_eq!(second.status, 200, "{}", text(&second));

        let read_a = get_issues(&app, &seeded, &format!("/{a}"))
            .await
            .json::<Value>();
        assert_eq!(read_a["merged_into"], c.to_string());
        let read_b = get_issues(&app, &seeded, &format!("/{b}"))
            .await
            .json::<Value>();
        assert_eq!(read_b["merged_into"], c.to_string());
        let read_c = get_issues(&app, &seeded, &format!("/{c}"))
            .await
            .json::<Value>();
        let from = read_c["merged_from"].as_array().unwrap();
        assert_eq!(from.len(), 2, "{read_c}");
        assert!(from.contains(&json!(a.to_string())));
        assert!(from.contains(&json!(b.to_string())));
    }

    #[tokio::test]
    async fn merge_refuses_self_another_source_a_merged_issue_and_a_merged_target() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let source = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let mut other_body = source_body("Other");
        other_body["enabled"] = json!(false);
        let other = id_of(&create_ok(&app, &seeded, &other_body).await);
        let now = app.state.clock.now_ns();
        let (a, b, c, x) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        for (id, owner, title) in [
            (a, source, "a"),
            (b, source, "b"),
            (c, source, "c"),
            (x, other, "x"),
        ] {
            put_issue(
                &app,
                issue_row(project, owner, id, title, IssueState::Open, now),
            )
            .await;
        }

        let itself = merge_request(&app, &seeded, a, a).await;
        assert_eq!(itself.status, 409, "{}", text(&itself));
        assert_eq!(itself.problem().code, ProblemCode::Conflict);
        let elsewhere = merge_request(&app, &seeded, a, x).await;
        assert_eq!(elsewhere.status, 409, "{}", text(&elsewhere));
        assert_eq!(elsewhere.problem().code, ProblemCode::Conflict);

        let folded = merge_request(&app, &seeded, a, b).await;
        assert_eq!(folded.status, 200, "{}", text(&folded));
        let again = merge_request(&app, &seeded, a, c).await;
        assert_eq!(again.status, 409, "{}", text(&again));
        let into_merged = merge_request(&app, &seeded, c, a).await;
        assert_eq!(into_merged.status, 409, "{}", text(&into_merged));

        let missing = merge_request(&app, &seeded, c, Uuid::now_v7()).await;
        assert_eq!(missing.status, 404, "{}", text(&missing));
    }

    #[tokio::test]
    async fn viewer_can_read_but_not_triage() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let source = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let now = app.state.clock.now_ns();
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        put_issue(
            &app,
            issue_row(project, source, a, "a", IssueState::Open, now),
        )
        .await;
        put_issue(
            &app,
            issue_row(project, source, b, "b", IssueState::Open, now),
        )
        .await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;

        let renamed = app
            .request(Method::PATCH, &issue_url(project, a))
            .header(header::COOKIE.as_str(), &viewer.cookie)
            .json(&json!({"title": "Mine"}))
            .send()
            .await;
        assert_eq!(renamed.status, 403, "{}", text(&renamed));
        let folded = app
            .request(Method::POST, &format!("{}/merge", issue_url(project, a)))
            .header(header::COOKIE.as_str(), &viewer.cookie)
            .json(&json!({"into": b}))
            .send()
            .await;
        assert_eq!(folded.status, 403, "{}", text(&folded));

        let read = app
            .request(Method::GET, &issue_url(project, a))
            .header(header::COOKIE.as_str(), &viewer.cookie)
            .send()
            .await;
        assert_eq!(read.status, 200, "{}", text(&read));
        assert_eq!(read.json::<Value>()["title"], "a");
    }

    #[tokio::test]
    async fn an_issue_with_more_names_than_one_read_takes_counts_them_all() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let source = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let now = app.state.clock.now_ns();
        let target = Uuid::now_v7();
        put_issue(
            &app,
            issue_row(project, source, target, "Target", IssueState::Open, now),
        )
        .await;
        // The target and 1 000 merged issues make 1 001 names, more than one score name read takes.
        let merged: Vec<IssueRow> = (0..MAX_COUNT_NAMES)
            .map(|_| {
                let mut row = issue_row(
                    project,
                    source,
                    Uuid::now_v7(),
                    "Merged",
                    IssueState::Merged,
                    now,
                );
                row.merged_into = Some(target);
                row
            })
            .collect();
        let last = merged.last().unwrap().id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    for row in &merged {
                        store::insert_issue(conn, row).await?;
                    }
                    Ok::<(), MetaError>(())
                })
            })
            .await
            .unwrap();
        occurrence(&app, &seeded, source, last, TRACE_A, 1, None).await;

        let read = app
            .request(Method::GET, &issue_url(project, target))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(read.status, 200, "{}", text(&read));
        let read: Value = read.json();
        assert_eq!(
            read["merged_from"].as_array().unwrap().len(),
            MAX_COUNT_NAMES
        );
        assert_eq!(read["count"], 1);

        let days = app
            .request(
                Method::GET,
                &format!("{}/occurrences", issue_url(project, target)),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(days.status, 200, "{}", text(&days));
        let days: Value = days.json();
        let total: u64 = days["days"]
            .as_array()
            .unwrap()
            .iter()
            .map(|day| day["count"].as_u64().unwrap())
            .sum();
        assert_eq!(total, 1);
    }

    #[tokio::test]
    async fn an_api_key_cannot_triage() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let source = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let now = app.state.clock.now_ns();
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        put_issue(
            &app,
            issue_row(project, source, a, "a", IssueState::Open, now),
        )
        .await;
        put_issue(
            &app,
            issue_row(project, source, b, "b", IssueState::Open, now),
        )
        .await;
        let key = app.write_key(&seeded).await;
        let bearer = format!("Bearer {key}");

        let renamed = app
            .request(Method::PATCH, &issue_url(project, a))
            .header(header::AUTHORIZATION.as_str(), &bearer)
            .json(&json!({"title": "Mine"}))
            .send()
            .await;
        assert_eq!(renamed.status, 403, "{}", text(&renamed));
        let folded = app
            .request(Method::POST, &format!("{}/merge", issue_url(project, a)))
            .header(header::AUTHORIZATION.as_str(), &bearer)
            .json(&json!({"into": b}))
            .send()
            .await;
        assert_eq!(folded.status, 403, "{}", text(&folded));

        let read = app
            .request(Method::GET, &issue_url(project, a))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(read.json::<Value>()["title"], "a");
    }

    #[tokio::test]
    async fn merged_issues_are_ordered_by_occurrences_and_examples_follow_them() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let source = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let now = app.state.clock.now_ns();
        let target = Uuid::now_v7();
        put_issue(
            &app,
            issue_row(project, source, target, "Target", IssueState::Open, now),
        )
        .await;
        occurrence(&app, &seeded, source, target, TRACE_A, 1, None).await;
        // Seven merged issues with one occurrence each, and two with none that were seen last.
        let mut busy = Vec::new();
        for n in 0..7 {
            let issue = Uuid::now_v7();
            let mut row = issue_row(
                project,
                source,
                issue,
                "Busy",
                IssueState::Merged,
                now - 3 * HOUR_NS,
            );
            row.merged_into = Some(target);
            put_issue(&app, row).await;
            let trace = format!("{:032x}", 0x2000 + n);
            occurrence(&app, &seeded, source, issue, &trace, 2, None).await;
            busy.push(issue.to_string());
        }
        let mut quiet = Vec::new();
        for _ in 0..2 {
            let issue = Uuid::now_v7();
            let mut row = issue_row(
                project,
                source,
                issue,
                "Quiet",
                IssueState::Merged,
                now - HOUR_NS,
            );
            row.merged_into = Some(target);
            put_issue(&app, row).await;
            quiet.push(issue.to_string());
        }

        let read = app
            .request(Method::GET, &issue_url(project, target))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(read.status, 200, "{}", text(&read));
        let read: Value = read.json();
        let merged_from: Vec<String> = read["merged_from"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap().to_owned())
            .collect();
        // The busy ones first, then the quiet ones; ties are in id order.
        let mut head = merged_from[..7].to_vec();
        head.sort();
        busy.sort();
        assert_eq!(head, busy);
        let mut tail = merged_from[7..].to_vec();
        tail.sort();
        quiet.sort();
        assert_eq!(tail, quiet);

        // The examples are the issue and the seven merged issues with the most occurrences.
        let occurrences = app
            .request(
                Method::GET,
                &format!("{}/occurrences", issue_url(project, target)),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(occurrences.status, 200, "{}", text(&occurrences));
        let occurrences: Value = occurrences.json();
        let mut example_ids: Vec<String> = occurrences["examples"]
            .as_array()
            .unwrap()
            .iter()
            .map(|example| example["issue_id"].as_str().unwrap().to_owned())
            .collect();
        example_ids.sort();
        example_ids.dedup();
        let mut expected = busy.clone();
        expected.push(target.to_string());
        expected.sort();
        assert_eq!(example_ids, expected);
    }

    /// An LLM connection of the seeded org, named after its adapter.
    async fn connection(app: &TestApp, seeded: &Seeded, adapter: &'static str) -> Uuid {
        let id = Uuid::now_v7();
        let org = seeded.org_id;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let fields = ConnectionFields {
                        name: adapter.to_owned(),
                        adapter: adapter.to_owned(),
                        base_url: None,
                        secret_enc: vec![1],
                        api_key_hint: "WXYZ".to_owned(),
                        extra_headers_enc: None,
                        extra_header_names: Vec::new(),
                        default_model: None,
                        structured_output: "native".to_owned(),
                        max_concurrency: 4,
                        requests_per_minute: 300,
                    };
                    insert_connection(conn, id, org, fields, None, now).await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
        id
    }

    /// A topic source body on `chat` and `embed`, both connections of the same adapter.
    fn topic_body(name: &str, chat: Uuid, embed: Uuid) -> Value {
        json!({
            "name": name,
            "kind": "topic",
            "chat_connection_id": chat,
            "chat_model": "gpt-5-mini",
            "embed_connection_id": embed,
            "embed_model": "text-embedding-3-small"
        })
    }

    /// A refit of `source` that started at `started_at`, already finished.
    async fn insert_refit_row(app: &TestApp, source: Uuid, started_at: i64, id: u128) {
        let row = RefitRow {
            id: Uuid::from_u128(id),
            source_id: source,
            trigger: "manual",
            started_at,
            finished_at: Some(started_at),
            pending: 0,
            clusters: 0,
            created: 0,
            joined: 0,
            outliers: 0,
            cost_usd: 0.0,
            error_class: None,
        };
        app.state
            .meta
            .write(move |conn| Box::pin(async move { store::insert_refit(conn, &row).await }))
            .await
            .unwrap();
    }

    /// A queued or other-status candidate of `source`, created at `created_at`.
    fn candidate(
        id: u128,
        project: ProjectId,
        source: Uuid,
        status: CandidateStatus,
        created_at: i64,
    ) -> CandidateRow {
        CandidateRow {
            id: Uuid::from_u128(id),
            project_id: project,
            source_id: source,
            target: CandidateTarget::Trace,
            trace_id: Some(TraceId::from_slice(&[9; 16]).unwrap()),
            span_id: None,
            session_id: None,
            user_id: None,
            trace_start: Some(created_at),
            score_id: None,
            dedup_key: format!("{source}:{id}"),
            status,
            attempts: 0,
            claims: 0,
            interruptions: 0,
            load_retries: 0,
            not_before: created_at,
            error_class: None,
            issue_id: None,
            summary: None,
            summary_prompt_version: None,
            vector: None,
            embed_model: None,
            dimension: None,
            call_trace_id: None,
            cost_usd: 0.0,
            created_at,
            updated_at: created_at,
        }
    }

    async fn refit_now(app: &TestApp, seeded: &Seeded, id: Uuid) -> TestResponse {
        app.request(
            Method::POST,
            &format!("{}/{id}/refit", path(seeded.project.id)),
        )
        .header(header::COOKIE.as_str(), &seeded.cookie)
        .send()
        .await
    }

    #[tokio::test]
    async fn a_topic_source_gets_its_defaults_and_its_filter() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let openai = connection(&app, &seeded, "openai").await;
        let created = create_ok(&app, &seeded, &topic_body("Topics", openai, openai)).await;
        assert_eq!(created["kind"], "topic");
        assert_eq!(created["filter"], "environment = 'production'");
        assert_eq!(created["sample_rate"], 1.0);
        assert_eq!(created["daily_cap"], 500);
        assert_eq!(created["floor"], 0.8);
        assert_eq!(created["min_issue_size"], 5);
        assert_eq!(created["embed_model"], "text-embedding-3-small");
        assert!(created["next_refit_at"].is_string());
        assert_eq!(created["candidates_24h"]["queued"], 0);
        assert_eq!(created["spent_today_usd"], 0.0);
        assert!(created.get("score_names").is_none());
        assert!(created.get("feedback_sources").is_none());
    }

    #[tokio::test]
    async fn a_feedback_source_reads_both_feedback_sources_by_default() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let openai = connection(&app, &seeded, "openai").await;
        let mut body = topic_body("Feedback", openai, openai);
        body["kind"] = json!("feedback");
        body["score_names"] = json!(["thumbs"]);
        let created = create_ok(&app, &seeded, &body).await;
        assert_eq!(created["feedback_sources"], json!(["end_user", "human"]));
        assert_eq!(created["score_names"], json!(["thumbs"]));
        assert!(created["filter"].is_null() || created.get("filter").is_none());
    }

    #[tokio::test]
    async fn a_score_source_without_score_names_is_400() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let openai = connection(&app, &seeded, "openai").await;
        let mut body = topic_body("Scores", openai, openai);
        body["kind"] = json!("score");
        let response = create(&app, &seeded, &body).await;
        assert_eq!(response.status, 400, "{}", text(&response));
        assert!(text(&response).contains("score_names"));
    }

    #[tokio::test]
    async fn an_anthropic_embedding_connection_is_400() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let openai = connection(&app, &seeded, "openai").await;
        let anthropic = connection(&app, &seeded, "anthropic").await;
        let response = create(&app, &seeded, &topic_body("Topics", openai, anthropic)).await;
        assert_eq!(response.status, 400, "{}", text(&response));
        assert!(text(&response).contains("this provider has no embeddings API"));
    }

    #[tokio::test]
    async fn patching_the_embedding_model_is_400() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let openai = connection(&app, &seeded, "openai").await;
        let created = create_ok(&app, &seeded, &topic_body("Topics", openai, openai)).await;
        let id = id_of(&created);
        let response = patch(&app, &seeded, id, &json!({ "embed_model": "other" })).await;
        assert_eq!(response.status, 400, "{}", text(&response));
        assert!(text(&response).contains("fixed after create"));
    }

    #[tokio::test]
    async fn an_llm_field_on_an_error_source_is_400() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let mut body = source_body("Errors");
        body["floor"] = json!(0.9);
        let response = create(&app, &seeded, &body).await;
        assert_eq!(response.status, 400, "{}", text(&response));
        assert!(text(&response).contains("not used by error sources"));
    }

    #[tokio::test]
    async fn refit_now_is_202_and_refused_when_recent_or_for_an_error_source() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let openai = connection(&app, &seeded, "openai").await;
        let created = create_ok(&app, &seeded, &topic_body("Topics", openai, openai)).await;
        let id = id_of(&created);
        // The first source is not due yet (created an hour ago at the earliest), so the refit is taken.
        let accepted = refit_now(&app, &seeded, id).await;
        assert_eq!(accepted.status, 202, "{}", text(&accepted));
        // Now it is due, so a second request is refused.
        let again = refit_now(&app, &seeded, id).await;
        assert_eq!(again.status, 409, "{}", text(&again));
        // A source whose refit started just now is refused for ten minutes.
        let fresh = create_ok(&app, &seeded, &topic_body("Fresh", openai, openai)).await;
        let fresh_id = id_of(&fresh);
        let now = app.state.clock.now_ns();
        insert_refit_row(&app, fresh_id, now, 0x1d01).await;
        let recent = refit_now(&app, &seeded, fresh_id).await;
        assert_eq!(recent.status, 409, "{}", text(&recent));
        assert!(text(&recent).contains("less than 10 minutes"));
        // An error source has no refit.
        let errors = create_ok(&app, &seeded, &source_body("Errors")).await;
        let refused = refit_now(&app, &seeded, id_of(&errors)).await;
        assert_eq!(refused.status, 400, "{}", text(&refused));
    }

    #[tokio::test]
    async fn refit_now_on_a_disabled_or_paused_source_is_400() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let openai = connection(&app, &seeded, "openai").await;
        let disabled = create_ok(&app, &seeded, &topic_body("Disabled", openai, openai)).await;
        let disabled_id = id_of(&disabled);
        let turned_off = patch(&app, &seeded, disabled_id, &json!({ "enabled": false })).await;
        assert_eq!(turned_off.status, 200, "{}", text(&turned_off));
        let refused = refit_now(&app, &seeded, disabled_id).await;
        assert_eq!(refused.status, 400, "{}", text(&refused));
        assert!(text(&refused).contains("disabled source"));

        let paused = create_ok(&app, &seeded, &topic_body("Paused", openai, openai)).await;
        let paused_id = id_of(&paused);
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    store::pause_source(conn, paused_id, "connection", Some("paused"), now).await
                })
            })
            .await
            .unwrap();
        let refused = refit_now(&app, &seeded, paused_id).await;
        assert_eq!(refused.status, 400, "{}", text(&refused));
        assert!(text(&refused).contains("paused source"));
    }

    #[tokio::test]
    async fn refit_now_makes_the_source_due_as_a_manual_refit() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let openai = connection(&app, &seeded, "openai").await;
        let created = create_ok(&app, &seeded, &topic_body("Topics", openai, openai)).await;
        let id = id_of(&created);
        let accepted = refit_now(&app, &seeded, id).await;
        assert_eq!(accepted.status, 202, "{}", text(&accepted));

        let now = app.state.clock.now_ns();
        let due = store::due_refits(app.state.meta.reader(), now)
            .await
            .unwrap();
        let triggers: Vec<&str> = due
            .iter()
            .filter(|(row, _)| row.id == id)
            .map(|(_, trigger)| *trigger)
            .collect();
        assert_eq!(triggers, vec!["manual"]);
    }

    #[tokio::test]
    async fn merge_weights_the_target_centroid_and_sums_the_counts() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let openai = connection(&app, &seeded, "openai").await;
        let created = create_ok(&app, &seeded, &topic_body("Topics", openai, openai)).await;
        let source = id_of(&created);
        let now = app.state.clock.now_ns();
        let target = Uuid::now_v7();
        let merged = Uuid::now_v7();
        put_issue(
            &app,
            IssueRow {
                centroid: Some(vec![1.0, 0.0, 0.0]),
                member_count: 3,
                ..issue_row(project, source, target, "target", IssueState::Open, now)
            },
        )
        .await;
        put_issue(
            &app,
            IssueRow {
                centroid: Some(vec![0.0, 1.0, 0.0]),
                member_count: 1,
                ..issue_row(project, source, merged, "merged", IssueState::Open, now)
            },
        )
        .await;

        let response = merge_request(&app, &seeded, merged, target).await;
        assert_eq!(response.status, 200, "{}", text(&response));

        let stored = store::get_issue(app.state.meta.reader(), project, target)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.member_count, 4);
        // A 3 to 1 mean of two orthogonal unit vectors, normalized.
        let norm = 0.625_f32.sqrt();
        let expected = [0.75 / norm, 0.25 / norm, 0.0];
        let centroid = stored.centroid.expect("a centroid");
        assert_eq!(centroid.len(), expected.len());
        for (actual, expected) in centroid.iter().zip(expected) {
            assert!((actual - expected).abs() < 1e-5, "{centroid:?}");
        }
    }

    #[tokio::test]
    async fn a_changed_budget_resumes_a_budget_pause_and_another_change_keeps_it() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let org = seeded.org_id;
        let openai = connection(&app, &seeded, "openai").await;
        let created = create_ok(&app, &seeded, &topic_body("Budgeted", openai, openai)).await;
        let id = id_of(&created);
        let now = app.state.clock.now_ns();
        let stored = store::get_source(app.state.meta.reader(), project, id)
            .await
            .unwrap()
            .unwrap();
        // The source is paused because its budget of 1 USD is spent.
        let paused = SourceRow {
            budget_usd: Some(1.0),
            budget_period: Some("day".to_owned()),
            paused_reason: Some("budget".to_owned()),
            paused_at: Some(now),
            paused_detail: Some("budget reached".to_owned()),
            updated_at: now,
            ..stored
        };
        let row = paused.clone();
        app.state
            .meta
            .write(move |conn| Box::pin(async move { store::update_source(conn, &row).await }))
            .await
            .unwrap();

        // A change that leaves the budget alone keeps the pause.
        let renamed = SourceRow {
            name: "Renamed".to_owned(),
            updated_at: now + 1,
            ..paused.clone()
        };
        let outcome = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    update_checked(conn, project, org, paused, renamed, None, None, now + 1).await
                })
            })
            .await
            .unwrap();
        let SourceWrite::Done(kept) = outcome else {
            panic!("the rename is written");
        };
        assert_eq!(kept.paused_reason.as_deref(), Some("budget"));

        // Raising the budget resumes the source.
        let stored = store::get_source(app.state.meta.reader(), project, id)
            .await
            .unwrap()
            .unwrap();
        let raised = SourceRow {
            budget_usd: Some(5.0),
            updated_at: now + 2,
            ..stored.clone()
        };
        let outcome = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    update_checked(conn, project, org, stored, raised, None, None, now + 2).await
                })
            })
            .await
            .unwrap();
        let SourceWrite::Done(resumed) = outcome else {
            panic!("the raise is written");
        };
        assert_eq!(resumed.paused_reason, None);
        assert_eq!(resumed.paused_detail, None);
        let read = store::get_source(app.state.meta.reader(), project, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read.paused_reason, None);
        assert_eq!(read.budget_usd, Some(5.0));
    }

    /// A topic source on `openai` with a 1 USD daily budget, paused with reason `budget`.
    async fn budget_paused(app: &TestApp, seeded: &Seeded, openai: Uuid, name: &str) -> Uuid {
        let mut body = topic_body(name, openai, openai);
        body["budget_usd"] = json!(1.0);
        body["budget_period"] = json!("day");
        let id = id_of(&create_ok(app, seeded, &body).await);
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    store::pause_source(conn, id, PAUSE_BUDGET, Some("budget reached"), now).await
                })
            })
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn a_budget_pause_ends_on_a_budget_change_or_a_clear_but_not_a_rename() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let openai = connection(&app, &seeded, "openai").await;
        let raised = budget_paused(&app, &seeded, openai, "Raised").await;
        let cleared = budget_paused(&app, &seeded, openai, "Cleared").await;
        let renamed = budget_paused(&app, &seeded, openai, "Renamed").await;

        let response = patch(&app, &seeded, raised, &json!({ "budget_usd": 5.0 })).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let body: Value = response.json();
        assert!(body["paused_reason"].is_null(), "{body}");
        let stored = store::get_source(app.state.meta.reader(), project, raised)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.paused_reason, None);
        assert_eq!(stored.budget_usd, Some(5.0));

        let response = patch(&app, &seeded, cleared, &json!({ "clear_budget": true })).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let body: Value = response.json();
        assert!(body["paused_reason"].is_null(), "{body}");
        let stored = store::get_source(app.state.meta.reader(), project, cleared)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.paused_reason, None);
        assert_eq!(stored.budget_usd, None);

        let response = patch(&app, &seeded, renamed, &json!({ "name": "Still paused" })).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let body: Value = response.json();
        assert_eq!(body["paused_reason"], "budget", "{body}");
        let stored = store::get_source(app.state.meta.reader(), project, renamed)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.paused_reason.as_deref(), Some("budget"));
    }

    #[tokio::test]
    async fn the_refits_list_is_newest_first() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let openai = connection(&app, &seeded, "openai").await;
        let created = create_ok(&app, &seeded, &topic_body("Topics", openai, openai)).await;
        let id = id_of(&created);
        let now = app.state.clock.now_ns();
        insert_refit_row(&app, id, now - 2 * HOUR_NS, 0x1d11).await;
        insert_refit_row(&app, id, now - HOUR_NS, 0x1d12).await;
        let response = app
            .request(
                Method::GET,
                &format!("{}/{id}/refits", path(seeded.project.id)),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let body: Value = response.json();
        let data = body["data"].as_array().expect("data is an array");
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["id"], Uuid::from_u128(0x1d12).to_string());
        assert_eq!(data[1]["id"], Uuid::from_u128(0x1d11).to_string());
        assert_eq!(data[0]["trigger"], "manual");
    }

    #[tokio::test]
    async fn candidates_24h_count_the_last_day_by_status() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let openai = connection(&app, &seeded, "openai").await;
        let created = create_ok(&app, &seeded, &topic_body("Topics", openai, openai)).await;
        let id = id_of(&created);
        let now = app.state.clock.now_ns();
        let rows = [
            candidate(
                0x1e01,
                seeded.project.id,
                id,
                CandidateStatus::Queued,
                now - HOUR_NS,
            ),
            candidate(
                0x1e02,
                seeded.project.id,
                id,
                CandidateStatus::Queued,
                now - 2 * HOUR_NS,
            ),
            candidate(
                0x1e03,
                seeded.project.id,
                id,
                CandidateStatus::Pending,
                now - 3 * HOUR_NS,
            ),
            // Older than a day: not counted.
            candidate(
                0x1e04,
                seeded.project.id,
                id,
                CandidateStatus::Dropped,
                now - 25 * HOUR_NS,
            ),
        ];
        for row in rows {
            app.state
                .meta
                .write(move |conn| {
                    Box::pin(async move { store::insert_candidate(conn, &row).await })
                })
                .await
                .unwrap();
        }
        let response = app
            .request(Method::GET, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let body: Value = response.json();
        assert_eq!(body["candidates_24h"]["queued"], 2);
        assert_eq!(body["candidates_24h"]["pending"], 1);
        assert_eq!(body["candidates_24h"]["dropped"], 0);
    }

    /// The channel ids as the API lists them: sorted.
    fn channel_strings(ids: &[Uuid]) -> Value {
        let mut sorted = ids.to_vec();
        sorted.sort();
        json!(sorted.iter().map(Uuid::to_string).collect::<Vec<_>>())
    }

    fn with_channels(mut body: Value, ids: &[Uuid]) -> Value {
        body["channel_ids"] = json!(ids);
        body
    }

    /// The stored notification cut-off of a source, straight from the table.
    async fn last_notified_at(app: &TestApp, id: Uuid) -> Option<i64> {
        sqlx::query_scalar("SELECT last_notified_at FROM issue_sources WHERE id = ?")
            .bind(id.to_string())
            .fetch_one(app.state.meta.reader())
            .await
            .unwrap()
    }

    /// Moves the manual clock an hour on and returns the new time.
    fn advance_an_hour(app: &TestApp) -> i64 {
        let later = app.state.clock.now_ns() + HOUR_NS;
        app.state.clock.set(later);
        later
    }

    async fn get_source(app: &TestApp, seeded: &Seeded, id: Uuid) -> Value {
        let response = app
            .request(Method::GET, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        response.json()
    }

    /// The message of the one field error of a 400.
    fn channel_error(response: &TestResponse) -> String {
        assert_eq!(response.status, 400, "{}", text(response));
        let problem = response.problem();
        assert_eq!(problem.errors.len(), 1, "{}", text(response));
        assert_eq!(problem.errors[0].field, "channel_ids");
        problem.errors[0].message.clone()
    }

    #[tokio::test]
    async fn create_source_with_channels() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let a = put_channel(&app, seeded.project.id, "A").await;
        let b = put_channel(&app, seeded.project.id, "B").await;
        let created = create_ok(
            &app,
            &seeded,
            &with_channels(source_body("Errors"), &[b, a]),
        )
        .await;
        assert_eq!(created["channel_ids"], channel_strings(&[a, b]));
        let id = id_of(&created);

        let read = get_source(&app, &seeded, id).await;
        assert_eq!(read["channel_ids"], channel_strings(&[a, b]));

        let page = app
            .request(Method::GET, &path(seeded.project.id))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await
            .json::<Value>();
        assert_eq!(page["data"][0]["channel_ids"], channel_strings(&[a, b]));
    }

    #[tokio::test]
    async fn create_source_without_channels_returns_an_empty_list() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let created = create_ok(&app, &seeded, &source_body("Errors")).await;
        assert_eq!(created["channel_ids"], json!([]));
        let read = get_source(&app, &seeded, id_of(&created)).await;
        assert_eq!(read["channel_ids"], json!([]));
    }

    #[tokio::test]
    async fn channel_ids_are_validated() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let a = put_channel(&app, seeded.project.id, "A").await;
        let (other, _) = app.add_project(&seeded, "other", None).await;
        let foreign = put_channel(&app, other.id, "Foreign").await;

        let eleven: Vec<Uuid> = (0..11).map(|_| Uuid::now_v7()).collect();
        let too_many = create(&app, &seeded, &with_channels(source_body("Many"), &eleven)).await;
        assert_eq!(channel_error(&too_many), "at most 10 channels");

        let twice = create(&app, &seeded, &with_channels(source_body("Twice"), &[a, a])).await;
        assert_eq!(channel_error(&twice), "duplicate channel");

        let unknown = create(
            &app,
            &seeded,
            &with_channels(source_body("Foreign"), &[foreign]),
        )
        .await;
        assert_eq!(channel_error(&unknown), "unknown channel");
    }

    #[tokio::test]
    async fn a_channel_deleted_before_the_write_is_an_unknown_channel() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let org = seeded.project.org_id;
        let now = app.state.clock.now_ns();
        let gone = vec![Uuid::now_v7()];

        // A create whose channel vanished after the check.
        let row = created_row(
            serde_json::from_value(source_body("Late")).unwrap(),
            project,
            seeded.user_id,
            now,
        );
        let created = app
            .state
            .meta
            .write(move |conn| {
                let gone = gone.clone();
                Box::pin(async move { insert_checked(conn, org, row, None, &gone, now).await })
            })
            .await
            .unwrap();
        assert!(matches!(created, SourceWrite::UnknownChannel));

        // A PATCH whose channel vanished.
        let id = id_of(&create_ok(&app, &seeded, &source_body("Existing")).await);
        let stored = store::get_source(app.state.meta.reader(), project, id)
            .await
            .unwrap()
            .unwrap();
        let row = stored.clone();
        let updated = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    update_checked(
                        conn,
                        project,
                        org,
                        stored,
                        row,
                        None,
                        Some(vec![Uuid::now_v7()]),
                        now,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        assert!(matches!(updated, SourceWrite::UnknownChannel));
    }

    #[tokio::test]
    async fn patch_replaces_and_clears_channels() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let a = put_channel(&app, seeded.project.id, "A").await;
        let b = put_channel(&app, seeded.project.id, "B").await;
        let id = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);

        let set_a = patch(&app, &seeded, id, &json!({"channel_ids": [a]})).await;
        assert_eq!(set_a.status, 200, "{}", text(&set_a));
        assert_eq!(set_a.json::<Value>()["channel_ids"], channel_strings(&[a]));

        let set_b = patch(&app, &seeded, id, &json!({"channel_ids": [b]})).await;
        assert_eq!(set_b.status, 200, "{}", text(&set_b));
        assert_eq!(set_b.json::<Value>()["channel_ids"], channel_strings(&[b]));

        let kept = patch(&app, &seeded, id, &json!({"name": "Renamed"})).await;
        assert_eq!(kept.status, 200, "{}", text(&kept));
        assert_eq!(kept.json::<Value>()["channel_ids"], channel_strings(&[b]));

        let refused = patch(&app, &seeded, id, &json!({"channel_ids": null})).await;
        assert_eq!(refused.status, 400, "{}", text(&refused));

        let cleared = patch(&app, &seeded, id, &json!({"channel_ids": []})).await;
        assert_eq!(cleared.status, 200, "{}", text(&cleared));
        assert_eq!(cleared.json::<Value>()["channel_ids"], json!([]));
    }

    #[tokio::test]
    async fn creating_an_enabled_source_sets_last_notified_at() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let id = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        assert_eq!(last_notified_at(&app, id).await, Some(now));
    }

    #[tokio::test]
    async fn creating_a_disabled_source_leaves_last_notified_at_null() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let mut body = source_body("Off");
        body["enabled"] = json!(false);
        let id = id_of(&create_ok(&app, &seeded, &body).await);
        assert_eq!(last_notified_at(&app, id).await, None);
    }

    #[tokio::test]
    async fn enabling_a_disabled_source_sets_it_to_now() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let mut body = source_body("Off");
        body["enabled"] = json!(false);
        let id = id_of(&create_ok(&app, &seeded, &body).await);
        let later = advance_an_hour(&app);
        let enabled = patch(&app, &seeded, id, &json!({"enabled": true})).await;
        assert_eq!(enabled.status, 200, "{}", text(&enabled));
        assert_eq!(last_notified_at(&app, id).await, Some(later));
    }

    #[tokio::test]
    async fn disabling_and_re_enabling_resets_it() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let id = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        advance_an_hour(&app);
        let disabled = patch(&app, &seeded, id, &json!({"enabled": false})).await;
        assert_eq!(disabled.status, 200, "{}", text(&disabled));
        let later = advance_an_hour(&app);
        let enabled = patch(&app, &seeded, id, &json!({"enabled": true})).await;
        assert_eq!(enabled.status, 200, "{}", text(&enabled));
        assert_eq!(last_notified_at(&app, id).await, Some(later));
    }

    #[tokio::test]
    async fn a_patch_that_keeps_the_source_enabled_leaves_last_notified_at() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let id = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        advance_an_hour(&app);
        let renamed = patch(&app, &seeded, id, &json!({"name": "Renamed"})).await;
        assert_eq!(renamed.status, 200, "{}", text(&renamed));
        assert_eq!(last_notified_at(&app, id).await, Some(now));
    }

    #[tokio::test]
    async fn adding_the_first_channel_resets_the_cutoff() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let a = put_channel(&app, seeded.project.id, "A").await;
        let id = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let later = advance_an_hour(&app);
        let added = patch(&app, &seeded, id, &json!({"channel_ids": [a]})).await;
        assert_eq!(added.status, 200, "{}", text(&added));
        assert_eq!(last_notified_at(&app, id).await, Some(later));
    }

    #[tokio::test]
    async fn changing_channels_that_are_not_empty_keeps_the_cutoff() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let a = put_channel(&app, seeded.project.id, "A").await;
        let b = put_channel(&app, seeded.project.id, "B").await;
        let first = app.state.clock.now_ns();
        let id =
            id_of(&create_ok(&app, &seeded, &with_channels(source_body("Errors"), &[a])).await);
        assert_eq!(last_notified_at(&app, id).await, Some(first));

        advance_an_hour(&app);
        let swapped = patch(&app, &seeded, id, &json!({"channel_ids": [b]})).await;
        assert_eq!(swapped.status, 200, "{}", text(&swapped));
        assert_eq!(last_notified_at(&app, id).await, Some(first));

        let cleared = patch(&app, &seeded, id, &json!({"channel_ids": []})).await;
        assert_eq!(cleared.status, 200, "{}", text(&cleared));
        assert_eq!(last_notified_at(&app, id).await, Some(first));

        // Empty to non-empty again starts the clock again.
        let again = advance_an_hour(&app);
        let readded = patch(&app, &seeded, id, &json!({"channel_ids": [a]})).await;
        assert_eq!(readded.status, 200, "{}", text(&readded));
        assert_eq!(last_notified_at(&app, id).await, Some(again));
    }

    #[tokio::test]
    async fn deleting_a_channel_removes_it_from_a_source() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let a = put_channel(&app, seeded.project.id, "A").await;
        let id =
            id_of(&create_ok(&app, &seeded, &with_channels(source_body("Errors"), &[a])).await);
        let deleted = app
            .request(
                Method::DELETE,
                &format!("/api/v1/projects/{}/channels/{a}", seeded.project.id),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(deleted.status, 204, "{}", text(&deleted));
        let read = get_source(&app, &seeded, id).await;
        assert_eq!(read["channel_ids"], json!([]));
    }

    #[tokio::test]
    async fn a_viewer_cannot_set_channels() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let a = put_channel(&app, seeded.project.id, "A").await;
        let id = id_of(&create_ok(&app, &seeded, &source_body("Errors")).await);
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let created = app
            .request(Method::POST, &path(seeded.project.id))
            .header(header::COOKIE.as_str(), &viewer.cookie)
            .json(&with_channels(source_body("Mine"), &[a]))
            .send()
            .await;
        assert_eq!(created.status, 403, "{}", text(&created));
        let patched = app
            .request(Method::PATCH, &format!("{}/{id}", path(seeded.project.id)))
            .header(header::COOKIE.as_str(), &viewer.cookie)
            .json(&json!({"channel_ids": [a]}))
            .send()
            .await;
        assert_eq!(patched.status, 403, "{}", text(&patched));
    }
}
