//! Issue routes (spec 09 § Issues, D-IC-1, D-IC-3, D-IC-9, D-IC-10, D-IC-13): sources, and the
//! issues they find with their counts, occurrences and history. Project members and above write
//! the sources; every project reader lists and reads.

use std::collections::{BTreeMap, HashMap};

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, patch, post};
use cetra_core::enums::ScoreSource;
use cetra_core::ids::{OrgId, ProjectId};
use cetra_core::score::{Score, ScoreTarget};
use cetra_storage::{ScoreListRequest, ScoreNameCountsRequest, TimeWindow, ViewName};
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::api::common::{FieldError, PageParams, TimeParam};
use crate::api::convert;
use crate::api::issues::{
    CreateIssueSourceRequest, Issue, IssueDay, IssueEvent, IssueEventPage, IssueExample,
    IssueListParams, IssueOccurrenceParams, IssueOccurrences, IssuePage, IssueSource,
    IssueSourceKind, IssueSourcePage, IssueState, UpdateIssueSourceRequest,
};
use crate::auth::principal::{self, Principal};
use crate::auth::roles::ProjectPerm;
use crate::eval::budget::DAY_NS;
use crate::eval::rule_check::field_error;
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::issues::source_limit::{self, SourceLimitError};
use crate::meta::MetaError;
use crate::meta::issues::{self as store, IssueSort, SourceKind, SourceRow};
use crate::routes::alert_rules::{decode_cursor, encode_cursor};

use super::orgs::page_named;
use super::read::{READ_TIMEOUT, open_query};

/// Most ids one issue list filters by (`ids`).
const MAX_IDS: usize = 100;
/// Longest period of the occurrence days (`from` to `to`).
const MAX_OCCURRENCE_PERIOD_NS: i64 = 90 * DAY_NS;
/// Days an occurrence read covers when `from` is absent.
const OCCURRENCE_DEFAULT_PERIOD_NS: i64 = 14 * DAY_NS;
/// Most names an occurrence read takes examples from: the issue, then its merged issues (D-IC-9).
const MAX_EXAMPLE_NAMES: usize = 8;
/// Examples read per name, and examples returned.
const EXAMPLES_PER_NAME: usize = 20;
const MAX_EXAMPLES: usize = 20;
/// Events an issue read returns, newest first.
const MAX_EVENTS: u32 = 100;

/// Issue routes. Reads need project view; source writes need
/// [`ProjectPerm::ManageEvaluation`] (D-I-24). Not session-only.
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
    let data = page
        .data
        .iter()
        .map(|row| source_json(row, open_issues(&open, row.id)))
        .collect();
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
    Ok(error::Json(source_json(&row, open_issues(&open, id))))
}

async fn create_issue_source(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Json(body): error::Json<CreateIssueSourceRequest>,
) -> Result<(StatusCode, error::Json<IssueSource>), ApiError> {
    let user = principal::session_user(&principal)?;
    let project = access.project.id;
    let org = access.project.org_id;
    let now = state.clock.now_ns();
    let name = body.name.trim().to_owned();
    let filter = normalize_filter(body.filter);
    validate(&state, org, project, &name, filter.as_deref()).await?;
    let id = Uuid::now_v7();
    let row = SourceRow {
        id,
        project_id: project,
        name,
        kind: source_kind(body.kind),
        filter,
        enabled: body.enabled,
        paused_reason: None,
        paused_at: None,
        created_by: Some(user.id),
        created_at: now,
        updated_at: now,
    };
    let limit = source_limit::limit(&state, org);
    let outcome = state
        .meta
        .write(move |conn| Box::pin(async move { insert_checked(conn, org, row, limit).await }))
        .await?;
    match outcome {
        SourceWrite::Done(row) => {
            tracing::info!(
                target: "audit",
                source_id = %id,
                project_id = %project,
                user_id = %user.id,
                "issue source created"
            );
            Ok((StatusCode::CREATED, error::Json(source_json(&row, 0))))
        }
        SourceWrite::OverLimit(error) => Err(error.into()),
        SourceWrite::Missing => Err(ApiError::NotFound),
    }
}

async fn update_issue_source(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
    error::Json(body): error::Json<UpdateIssueSourceRequest>,
) -> Result<error::Json<IssueSource>, ApiError> {
    let user = principal::session_user(&principal)?;
    let project = access.project.id;
    let org = access.project.org_id;
    let now = state.clock.now_ns();
    let stored = store::get_source(state.meta.reader(), project, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let name = body
        .name
        .map_or_else(|| stored.name.clone(), |name| name.trim().to_owned());
    let filter = match body.filter {
        Some(text) => normalize_filter(text),
        None => stored.filter.clone(),
    };
    validate(&state, org, project, &name, filter.as_deref()).await?;
    let row = SourceRow {
        name,
        filter,
        enabled: body.enabled.unwrap_or(stored.enabled),
        updated_at: now,
        ..stored.clone()
    };
    let limit = source_limit::limit(&state, org);
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move { update_checked(conn, project, org, stored, row, limit).await })
        })
        .await?;
    match outcome {
        SourceWrite::Done(row) => {
            tracing::info!(
                target: "audit",
                source_id = %id,
                project_id = %project,
                user_id = %user.id,
                "issue source updated"
            );
            let open = store::open_counts(state.meta.reader(), project).await?;
            Ok(error::Json(source_json(&row, open_issues(&open, id))))
        }
        SourceWrite::OverLimit(error) => Err(error.into()),
        SourceWrite::Missing => Err(ApiError::NotFound),
    }
}

async fn delete_issue_source(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
) -> Result<StatusCode, ApiError> {
    let user = principal::session_user(&principal)?;
    let project = access.project.id;
    let user_id = user.id;
    // The source's issues and their events go with it (schema cascade, D-IC-3).
    let went = state
        .meta
        .write(move |conn| Box::pin(async move { store::delete_source(conn, project, id).await }))
        .await?;
    if !went {
        return Err(ApiError::NotFound);
    }
    tracing::info!(
        target: "audit",
        source_id = %id,
        project_id = %project,
        user_id = %user_id,
        "issue source deleted"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// The create's write: when the source is enabled, the active-source limit is checked with it
/// counted (D-IC-13).
async fn insert_checked(
    conn: &mut SqliteConnection,
    org: OrgId,
    row: SourceRow,
    limit: Option<u64>,
) -> Result<SourceWrite, MetaError> {
    if row.enabled {
        let active = store::count_enabled_sources(conn, org).await? + 1;
        if let Err(error) = source_limit::check(active, limit) {
            return Ok(SourceWrite::OverLimit(error));
        }
    }
    store::insert_source(conn, &row).await?;
    Ok(SourceWrite::Done(Box::new(row)))
}

/// The PATCH's write: the source must be the row the checks ran against. Enabling a disabled
/// source checks the active-source limit (D-IC-13).
async fn update_checked(
    conn: &mut SqliteConnection,
    project: ProjectId,
    org: OrgId,
    stored: SourceRow,
    row: SourceRow,
    limit: Option<u64>,
) -> Result<SourceWrite, MetaError> {
    let Some(current) = store::get_source_tx(conn, project, stored.id).await? else {
        return Ok(SourceWrite::Missing);
    };
    if current != stored {
        return Err(MetaError::Conflict("issue source changed, retry"));
    }
    if !stored.enabled && row.enabled {
        let active = store::count_enabled_sources(conn, org).await? + 1;
        if let Err(error) = source_limit::check(active, limit) {
            return Ok(SourceWrite::OverLimit(error));
        }
    }
    if !store::update_source(conn, &row).await? {
        return Ok(SourceWrite::Missing);
    }
    Ok(SourceWrite::Done(Box::new(row)))
}

/// Checks a source's name and filter (D-IC-1). The field checks all run and their errors come
/// back together; the filter is parsed last, by storage.
async fn validate(
    state: &AppState,
    org: OrgId,
    project: ProjectId,
    name: &str,
    filter: Option<&str>,
) -> Result<(), ApiError> {
    let mut errors = Vec::new();
    if !(1..=128).contains(&name.chars().count()) {
        errors.push(field_error("name", "name must be 1 to 128 characters"));
    }
    if filter.is_some_and(|text| text.len() > 4096) {
        errors.push(field_error("filter", "filter must be at most 4096 bytes"));
    }
    if !errors.is_empty() {
        return Err(bad_request(errors));
    }
    if let Some(text) = filter {
        let ctx = open_query(state, org, project, None).await?;
        cetra_storage::validate_filter(&ctx, ViewName::Spans, text)?;
    }
    Ok(())
}

/// The filter as stored: trimmed, and absent when blank.
fn normalize_filter(text: Option<String>) -> Option<String> {
    text.map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

/// Occurrence count and user count of a name set (D-IC-9).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Tally {
    count: u64,
    /// The largest user count of the names, not their sum: a user can occur under several.
    users: u64,
}

/// The occurrence score name of an issue (D-IC-11).
fn score_name(issue: Uuid) -> String {
    format!("issue:{issue}")
}

/// The score names an issue counts: its own, then those of the issues merged into it (D-IC-9).
fn names_of(issue: Uuid, merged: &[Uuid]) -> Vec<String> {
    std::iter::once(issue)
        .chain(merged.iter().copied())
        .map(score_name)
        .collect()
}

/// The tally of `names`, from the per-name tallies. Names without occurrences count nothing.
fn total(names: &[String], tallies: &HashMap<String, Tally>) -> Tally {
    names
        .iter()
        .filter_map(|name| tallies.get(name))
        .fold(Tally::default(), |sum, tally| Tally {
            count: sum.count.saturating_add(tally.count),
            users: sum.users.max(tally.users),
        })
}

/// Live occurrences of each name in `window`, per name (D-IC-9).
async fn tallies(
    state: &AppState,
    org: OrgId,
    project: ProjectId,
    names: &[String],
    window: TimeWindow,
) -> Result<HashMap<String, Tally>, ApiError> {
    if names.is_empty() {
        return Ok(HashMap::new());
    }
    let ctx = open_query(state, org, project, None).await?;
    let counts = cetra_storage::score_name_counts(
        &ctx,
        ScoreNameCountsRequest {
            names,
            window,
            by_day: false,
            timeout: READ_TIMEOUT,
        },
    )
    .await?;
    Ok(counts
        .into_iter()
        .map(|row| {
            (
                row.name,
                Tally {
                    count: row.count,
                    users: row.users,
                },
            )
        })
        .collect())
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
    let cursor = params.cursor.as_deref().map(decode_cursor).transpose()?;
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
        Some(row) if more => Some(encode_cursor(sort_value(row, sort), row.id)?),
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
    let org = access.project.org_id;
    let now = state.clock.now_ns();
    let row = issue_or_missing(&state, project, id).await?;
    let source = store::get_source(state.meta.reader(), project, row.source_id)
        .await?
        .ok_or_else(missing_source)?;
    let merged = store::merged_ids(state.meta.reader(), project, id).await?;
    let names = names_of(id, &merged);
    let window = TimeWindow::new(now.saturating_sub(DAY_NS), now)?;
    let counts = tallies(&state, org, project, &names, window).await?;
    Ok(error::Json(issue_json(
        &row,
        &source,
        merged,
        total(&names, &counts),
    )))
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

    let counts = cetra_storage::score_name_counts(
        &ctx,
        ScoreNameCountsRequest {
            names: &names,
            window,
            by_day: true,
            timeout: READ_TIMEOUT,
        },
    )
    .await?;
    let mut by_day: BTreeMap<i64, Tally> = BTreeMap::new();
    for row in counts {
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

    // The issue, then the merged issues by their latest occurrence (D-IC-9).
    let mut example_issues = vec![id];
    if !merged.is_empty() {
        let merged_rows = store::list_issues(
            state.meta.reader(),
            project,
            &store::IssueListQuery {
                states: &[],
                source: None,
                q: None,
                ids: Some(merged.as_slice()),
                sort: IssueSort::LastSeen,
                cursor: None,
                limit: u32::try_from(merged.len()).unwrap_or(u32::MAX),
            },
        )
        .await?;
        example_issues.extend(merged_rows.iter().map(|row| row.id));
    }
    example_issues.truncate(MAX_EXAMPLE_NAMES);

    let mut examples = Vec::new();
    for issue in example_issues {
        let name = score_name(issue);
        let page = cetra_storage::score_list(
            &ctx,
            ScoreListRequest {
                target: None,
                trace_id: None,
                span_id: None,
                session_id: None,
                name: Some(name.as_str()),
                source: Some(ScoreSource::Issue),
                evaluator_id: None,
                rule_id: None,
                window: None,
                limit: EXAMPLES_PER_NAME,
                cursor: None,
                timeout: READ_TIMEOUT,
            },
        )
        .await?;
        let scores = convert::scores(&page.batches, project)?;
        examples.extend(scores.iter().filter_map(|score| example_of(score, issue)));
    }
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

/// The source as the API shows it.
fn source_json(row: &SourceRow, open_issues: u64) -> IssueSource {
    IssueSource {
        id: row.id,
        project_id: row.project_id.as_uuid(),
        name: row.name.clone(),
        kind: kind_of(row.kind),
        filter: row.filter.clone(),
        enabled: row.enabled,
        paused_reason: row.paused_reason.clone(),
        created_by: row.created_by,
        created_at: row.created_at,
        updated_at: row.updated_at,
        open_issues,
    }
}

fn kind_of(kind: SourceKind) -> IssueSourceKind {
    match kind {
        SourceKind::Error => IssueSourceKind::Error,
    }
}

fn source_kind(kind: IssueSourceKind) -> SourceKind {
    match kind {
        IssueSourceKind::Error => SourceKind::Error,
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
    use crate::meta::issues::{EventKind, EventRow, IssueRow, IssueState};
    use crate::scores::{self as score_writes, Writer};
    use crate::test_support::{Seeded, TestApp, TestResponse, set_limits};

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

    /// An issue of `source` with one time for its first and last occurrence.
    fn issue_row(
        project: ProjectId,
        source: Uuid,
        id: Uuid,
        title: &str,
        state: IssueState,
        seen: i64,
    ) -> IssueRow {
        IssueRow {
            id,
            project_id: project,
            source_id: source,
            fingerprint: None,
            title: title.to_owned(),
            description: None,
            state,
            regressed: false,
            merged_into: None,
            first_seen: seen,
            last_seen: seen,
            resolved_at: None,
            resolved_by: None,
            created_at: seen,
            updated_at: seen,
        }
    }

    async fn put_issue(app: &TestApp, row: IssueRow) {
        app.state
            .meta
            .write(move |conn| Box::pin(async move { store::insert_issue(conn, &row).await }))
            .await
            .unwrap();
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
}
