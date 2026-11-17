//! End-user reads (spec 09 § Observability data).

use axum::extract::{Extension, State};
use axum::http::Method;
use axum::routing::get;
use cetra_storage as storage;
use serde::Deserialize;

use crate::api::common::Page;
use crate::api::convert;
use crate::api::sessions::SessionSummary;
use crate::api::users::{
    EndUserDetail, EndUserSummary, UserDetailParams, UserListParams, UserSort,
};
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
    error::Query(params): error::Query<UserDetailParams>,
) -> Result<error::Json<EndUserDetail>, ApiError> {
    let deadline = Deadline::start();
    let scope = list_scope(
        params.from,
        params.to,
        params.include_eval,
        params.include_experiments,
    )?;
    let ctx = open_query(&state, access.project.org_id, access.project.id, None).await?;
    let data = storage::user_detail(&ctx, &path.user_id, scope, deadline.remaining()?)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mut user = convert::end_user_summaries(std::slice::from_ref(&data.user))?
        .pop()
        .ok_or(ApiError::NotFound)?;
    attach_user_extras(&ctx, std::slice::from_mut(&mut user), None, scope, deadline).await?;
    let mut sessions: Vec<SessionSummary> = convert::session_summaries(&data.sessions)?;
    attach_session_scores(&ctx, &mut sessions, deadline).await?;
    Ok(error::Json(EndUserDetail { user, sessions }))
}

/// Fills `activity`, `scores` and `metadata` of each user (spec 09 § Observability data).
///
/// The activity window is the request's, or without one `[first_seen, last_seen]` of the users.
/// A one-sided request window (`from` or `to` alone) takes the open side from the users instead:
/// the start is `first_seen` and the end is `last_seen + 1`. If the clamped window is empty, the
/// users' own span is used. A user without traces in scope gets an empty activity of zeros.
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
    let users_span = || storage::TimeWindow::new(first, last.saturating_add(1));
    let activity = match scope.window {
        Some(window) => {
            let start = if window.from() == i64::MIN {
                first
            } else {
                window.from()
            };
            let end = if window.to() == i64::MAX {
                last.saturating_add(1)
            } else {
                window.to()
            };
            if start < end {
                storage::TimeWindow::new(start, end)?
            } else {
                users_span()?
            }
        }
        None => users_span()?,
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

    /// Trace start of `2026-01-01T10:05:00Z` in nanoseconds (`date -u -d ... +%s`).
    const USER_42_SECOND_TRACE_NS: i64 = 1_767_261_900_000_000_000;

    #[tokio::test(flavor = "multi_thread")]
    async fn detail_with_a_window_counts_only_traces_in_it() {
        let (app, seeded) = loaded().await;
        // user-42's traces start at 10:00, 10:05 and 10:20 (the 10:01 eval trace
        // is excluded). [10:04, 10:10) holds only the 10:05 trace, of sess-001.
        let (status, body) = get_json(
            &app,
            &seeded,
            "/users/user-42?from=2026-01-01T10:04:00Z&to=2026-01-01T10:10:00Z",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let detail: EndUserDetail = serde_json::from_value(body).unwrap();
        assert_eq!(detail.user.trace_count, 1);
        assert_eq!(detail.user.first_seen, USER_42_SECOND_TRACE_NS);
        assert_eq!(detail.user.activity.len(), 24);
        assert_eq!(detail.user.activity.iter().sum::<i64>(), 1);
        let sessions: Vec<(&str, i64)> = detail
            .sessions
            .iter()
            .map(|s| (s.session_id.as_str(), s.trace_count))
            .collect();
        assert_eq!(sessions, [("sess-001", 1)]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn detail_with_only_from_has_bucketed_activity() {
        let (app, seeded) = loaded().await;
        // Before user-42's first trace (10:00); no upper bound.
        let (status, body) =
            get_json(&app, &seeded, "/users/user-42?from=2026-01-01T00:00:00Z").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let detail: EndUserDetail = serde_json::from_value(body).unwrap();
        assert_eq!(detail.user.trace_count, 3);
        assert_eq!(detail.user.activity.len(), 24);
        assert_eq!(
            detail.user.activity.iter().sum::<i64>(),
            detail.user.trace_count
        );
        // The buckets span `[from, last_seen]`: the traces, ten hours after `from`, are in the
        // last ones. An unbounded end would put them all in the first bucket.
        assert_eq!(detail.user.activity[0], 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn detail_with_only_to_has_bucketed_activity() {
        let (app, seeded) = loaded().await;
        // After user-42's last trace (10:20); no lower bound.
        let (status, body) =
            get_json(&app, &seeded, "/users/user-42?to=2026-01-01T12:00:00Z").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let detail: EndUserDetail = serde_json::from_value(body).unwrap();
        assert_eq!(detail.user.trace_count, 3);
        assert_eq!(detail.user.activity.len(), 24);
        assert_eq!(
            detail.user.activity.iter().sum::<i64>(),
            detail.user.trace_count
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_with_only_from_has_bucketed_activity() {
        let (app, seeded) = loaded().await;
        let (status, body) = get_json(&app, &seeded, "/users?from=2026-01-01T00:00:00Z").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let page: Page<EndUserSummary> = serde_json::from_value(body).unwrap();
        assert!(!page.data.is_empty());
        for user in &page.data {
            assert_eq!(user.activity.len(), 24, "{}", user.user_id);
            assert_eq!(
                user.activity.iter().sum::<i64>(),
                user.trace_count,
                "{}",
                user.user_id
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn detail_outside_the_window_is_404() {
        let (app, seeded) = loaded().await;
        let (status, body) = get_json(
            &app,
            &seeded,
            "/users/user-42?from=2025-01-01T00:00:00Z&to=2025-01-02T00:00:00Z",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn detail_rejects_unknown_params() {
        let (app, seeded) = loaded().await;
        let (status, body) = get_json(&app, &seeded, "/users/user-42?foo=1").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
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
