//! End-user reads (spec 09 § Observability data).

use axum::extract::{Extension, State};
use axum::http::Method;
use axum::routing::get;
use cetra_storage as storage;
use serde::Deserialize;

use crate::api::common::{ExclusionParams, Page};
use crate::api::convert;
use crate::api::sessions::SessionSummary;
use crate::api::users::{EndUserDetail, EndUserSummary, UserListParams, UserSort};
use crate::auth::roles::ProjectPerm;
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::routes::read::{
    Deadline, attach_session_scores, list_scope, open_query, page_limit, sort_order,
};

/// End-user read routes. All are documented and need project view.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/users",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_users),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/users/{user_id}",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_user),
        },
    ]
}

#[derive(Debug, Deserialize)]
struct UserPath {
    user_id: String,
}

async fn list_users(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<UserListParams>,
) -> Result<error::Json<Page<EndUserSummary>>, ApiError> {
    let deadline = Deadline::start();
    let scope = list_scope(
        params.from,
        params.to,
        params.include_eval,
        params.include_experiments,
    )?;
    let ctx = open_query(&state, access.project.org_id, access.project.id, None).await?;
    let page = storage::user_list(
        &ctx,
        storage::UserListRequest {
            filter: params.filter.as_deref(),
            scope,
            sort: user_sort(params.sort),
            order: sort_order(params.order),
            limit: page_limit(params.limit),
            cursor: params.cursor.as_deref(),
            timeout: deadline.remaining()?,
        },
    )
    .await?;
    let mut users = convert::end_user_summaries(&page.batches)?;
    attach_user_extras(&ctx, &mut users, params.filter.as_deref(), scope, deadline).await?;
    Ok(error::Json(Page {
        data: users,
        next_cursor: page.next_cursor,
    }))
}

async fn get_user(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<UserPath>,
    error::Query(params): error::Query<ExclusionParams>,
) -> Result<error::Json<EndUserDetail>, ApiError> {
    let deadline = Deadline::start();
    let ctx = open_query(&state, access.project.org_id, access.project.id, None).await?;
    let data = storage::user_detail(
        &ctx,
        &path.user_id,
        params.include_eval,
        params.include_experiments,
        deadline.remaining()?,
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    let mut user = convert::end_user_summaries(std::slice::from_ref(&data.user))?
        .pop()
        .ok_or(ApiError::NotFound)?;
    let scope = storage::ListScope {
        window: None,
        include_eval: params.include_eval,
        include_experiments: params.include_experiments,
    };
    attach_user_extras(&ctx, std::slice::from_mut(&mut user), None, scope, deadline).await?;
    let mut sessions: Vec<SessionSummary> = convert::session_summaries(&data.sessions)?;
    attach_session_scores(&ctx, &mut sessions, deadline).await?;
    Ok(error::Json(EndUserDetail { user, sessions }))
}

/// Fills `activity`, `scores` and `metadata` of each user (spec 09 § Observability data).
///
/// The activity window is the request's, or without one `[first_seen, last_seen]` of the users.
/// A user without traces in scope gets an empty activity of zeros.
async fn attach_user_extras(
    ctx: &storage::QueryContext,
    users: &mut [EndUserSummary],
    filter: Option<&str>,
    scope: storage::ListScope,
    deadline: Deadline,
) -> Result<(), ApiError> {
    let Some(first) = users.iter().map(|user| user.first_seen).min() else {
        return Ok(());
    };
    let last = users
        .iter()
        .map(|user| user.last_seen)
        .max()
        .unwrap_or(first);
    let activity = match scope.window {
        Some(window) => window,
        None => storage::TimeWindow::new(first, last.saturating_add(1))?,
    };
    let ids: Vec<&str> = users.iter().map(|user| user.user_id.as_str()).collect();
    let extras = storage::user_extras(
        ctx,
        storage::UserExtrasRequest {
            filter,
            scope,
            user_ids: &ids,
            activity,
            timeout: deadline.remaining()?,
        },
    )
    .await?;
    let mut by_user = convert::user_extras(&extras)?;
    for user in users.iter_mut() {
        let extra = by_user.remove(&user.user_id).unwrap_or_default();
        user.activity = if extra.activity.is_empty() {
            vec![0; storage::ACTIVITY_BUCKETS]
        } else {
            extra.activity
        };
        user.scores = extra.scores;
        user.metadata = extra.metadata;
    }
    Ok(())
}

fn user_sort(sort: UserSort) -> storage::UserSort {
    match sort {
        UserSort::LastSeen => storage::UserSort::LastSeen,
        UserSort::TraceCount => storage::UserSort::TraceCount,
        UserSort::CostTotal => storage::UserSort::CostTotal,
        UserSort::SessionCount => storage::UserSort::SessionCount,
        UserSort::TotalTokens => storage::UserSort::TotalTokens,
        UserSort::ErrorCount => storage::UserSort::ErrorCount,
        UserSort::ErrorRate => storage::UserSort::ErrorRate,
        UserSort::TracesPerSession => storage::UserSort::TracesPerSession,
        UserSort::FirstSeen => storage::UserSort::FirstSeen,
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::Value as Json;

    use super::*;
    use crate::test_support::{Seeded, TestApp};

    /// A seeded app with the example dataset loaded.
    async fn loaded() -> (TestApp, Seeded) {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.load_example_dataset(seeded.project.id).await;
        (app, seeded)
    }

    async fn get_json(app: &TestApp, seeded: &Seeded, path: &str) -> (StatusCode, Json) {
        let response = app
            .request(
                Method::GET,
                &format!("/api/v1/projects/{}{path}", seeded.project.id),
            )
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        let body = response.json::<Json>();
        (response.status, body)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_orders_users_by_last_seen() {
        let (app, seeded) = loaded().await;
        let (status, body) = get_json(&app, &seeded, "/users").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let page: Page<EndUserSummary> = serde_json::from_value(body).unwrap();
        let users: Vec<(&str, i64, i64)> = page
            .data
            .iter()
            .map(|u| (u.user_id.as_str(), u.trace_count, u.session_count))
            .collect();
        assert_eq!(users, [("user-42", 3, 1), ("user-7", 1, 1)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn include_eval_counts_the_evaluation_trace() {
        let (app, seeded) = loaded().await;
        let (status, body) = get_json(&app, &seeded, "/users?include_eval=true").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let page: Page<EndUserSummary> = serde_json::from_value(body).unwrap();
        let user_42 = page.data.iter().find(|u| u.user_id == "user-42").unwrap();
        assert_eq!(user_42.trace_count, 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn detail_lists_the_user_sessions_with_scores() {
        let (app, seeded) = loaded().await;
        let (status, body) = get_json(&app, &seeded, "/users/user-42").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let detail: EndUserDetail = serde_json::from_value(body).unwrap();
        assert_eq!(detail.user.user_id, "user-42");
        let ids: Vec<&str> = detail
            .sessions
            .iter()
            .map(|s| s.session_id.as_str())
            .collect();
        assert_eq!(ids, ["sess-001"]);
        assert_eq!(detail.sessions[0].trace_count, 2);
        assert_eq!(detail.sessions[0].scores.len(), 1);
        assert_eq!(detail.sessions[0].scores[0].name, "resolved");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_users_returns_activity_scores_and_metadata() {
        let (app, seeded) = loaded().await;
        let (status, body) = get_json(&app, &seeded, "/users").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let page: Page<EndUserSummary> = serde_json::from_value(body).unwrap();
        let user = page.data.iter().find(|u| u.user_id == "user-42").unwrap();
        assert_eq!(user.activity.len(), 24);
        assert_eq!(user.activity.iter().sum::<i64>(), user.trace_count);
        assert_eq!(user.error_trace_count, 0);
        assert_eq!(user.metadata["name"], "\"Ada Example\"");
        assert_eq!(user.metadata["plan"], "\"pro\"");
        let helpfulness = user
            .scores
            .iter()
            .find(|score| score.name == "helpfulness")
            .unwrap();
        assert_eq!(helpfulness.count, 2);
        assert!((helpfulness.avg - 0.7).abs() < 1e-9, "{}", helpfulness.avg);
        let thumbs = user
            .scores
            .iter()
            .find(|score| score.name == "thumbs")
            .unwrap();
        assert_eq!(thumbs.count, 1);
        assert!((thumbs.avg - 1.0).abs() < 1e-9, "{}", thumbs.avg);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_sorts_by_error_rate_desc() {
        let (app, seeded) = loaded().await;
        let (status, body) = get_json(&app, &seeded, "/users?sort=error_rate&order=desc").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let page: Page<EndUserSummary> = serde_json::from_value(body).unwrap();
        let mut expected: Vec<(f64, &str)> = page
            .data
            .iter()
            .map(|user| {
                (
                    user.error_trace_count as f64 / user.trace_count as f64,
                    user.user_id.as_str(),
                )
            })
            .collect();
        expected.sort_by(|left, right| right.0.total_cmp(&left.0).then(right.1.cmp(left.1)));
        let expected_ids: Vec<&str> = expected.iter().map(|(_, id)| *id).collect();
        let ids: Vec<&str> = page.data.iter().map(|u| u.user_id.as_str()).collect();
        assert_eq!(ids, expected_ids);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_accepts_total_tokens_sort() {
        let (app, seeded) = loaded().await;
        let (status, body) = get_json(&app, &seeded, "/users?sort=total_tokens&order=asc").await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_user_is_not_found() {
        let (app, seeded) = loaded().await;
        let (status, body) = get_json(&app, &seeded, "/users/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["code"], "not_found");
    }
}
