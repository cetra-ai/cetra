//! Issue source routes (spec 09 § Issues, D-IC-1, D-IC-3, D-IC-13). Project members and above
//! write; every project reader lists and reads.

use std::collections::HashMap;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, patch, post};
use cetra_core::ids::{OrgId, ProjectId};
use cetra_storage::ViewName;
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::api::common::{FieldError, PageParams};
use crate::api::issues::{
    CreateIssueSourceRequest, IssueSource, IssueSourceKind, IssueSourcePage,
    UpdateIssueSourceRequest,
};
use crate::auth::principal::{self, Principal};
use crate::auth::roles::ProjectPerm;
use crate::eval::rule_check::field_error;
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::issues::source_limit::{self, SourceLimitError};
use crate::meta::MetaError;
use crate::meta::issues::{self as store, SourceKind, SourceRow};

use super::orgs::page_named;
use super::read::open_query;

/// Issue source routes. Reads need project view; writes need
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
    use axum::http::{Method, header};
    use serde_json::{Value, json};

    use super::*;
    use crate::api::common::ProblemCode;
    use crate::auth::roles::OrgRole;
    use crate::meta::issues::{IssueRow, IssueState};
    use crate::test_support::{Seeded, TestApp, TestResponse, set_limits};

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
}
