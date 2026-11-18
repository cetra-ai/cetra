//! Facets (filtering plan D1): values, keys or a histogram of one field over a list's rows.

use axum::extract::{Extension, State};
use axum::http::Method;
use axum::routing::get;
use cetra_storage as storage;

use crate::api::facets::{
    FacetBucket, FacetHistogram, FacetKind, FacetValue, FacetView, FacetsParams, FacetsResponse,
};
use crate::auth::roles::ProjectPerm;
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::routes::read::{Deadline, check_search, list_scope, open_query};

/// The limit when the request names none.
const DEFAULT_LIMIT: u32 = 10;

/// Facet read route. Documented and needs project view.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![RouteDef {
        method: Method::GET,
        path: "/api/v1/projects/{project}/facets",
        access: Access::Project {
            perm: ProjectPerm::View,
            session_only: false,
        },
        documented: true,
        handler: get(get_facets),
    }]
}

async fn get_facets(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<FacetsParams>,
) -> Result<error::Json<FacetsResponse>, ApiError> {
    check_search(params.search.as_deref())?;
    let scope = list_scope(
        params.from,
        params.to,
        params.include_eval,
        params.include_experiments,
    )?;
    let deadline = Deadline::start();
    let ctx = open_query(&state, access.project.org_id, access.project.id, None).await?;
    let data = storage::facet(
        &ctx,
        storage::FacetRequest {
            view: storage_view(params.view),
            field: &params.field,
            filter: params.filter.as_deref(),
            search: params.search.as_deref(),
            scope,
            query: params.q.as_deref(),
            limit: usize::try_from(params.limit.unwrap_or(DEFAULT_LIMIT)).unwrap_or(usize::MAX),
            timeout: deadline.remaining()?,
        },
    )
    .await?;
    Ok(error::Json(response(params.view, params.field, data)))
}

fn storage_view(view: FacetView) -> storage::FacetView {
    match view {
        FacetView::Traces => storage::FacetView::Traces,
        FacetView::Spans => storage::FacetView::Spans,
        FacetView::Sessions => storage::FacetView::Sessions,
    }
}

fn response(view: FacetView, field: String, data: storage::FacetData) -> FacetsResponse {
    let kind = match data.kind {
        storage::FacetKind::Values => FacetKind::Values,
        storage::FacetKind::Keys => FacetKind::Keys,
        storage::FacetKind::Histogram => FacetKind::Histogram,
    };
    let listed = data.kind != storage::FacetKind::Histogram;
    FacetsResponse {
        view,
        field,
        kind,
        total: data.total,
        missing: data.missing,
        values: listed.then(|| {
            data.values
                .into_iter()
                .map(|value| FacetValue {
                    value: value.value,
                    count: value.count,
                })
                .collect()
        }),
        more: listed.then_some(data.more),
        histogram: data.histogram.map(|histogram| FacetHistogram {
            min: histogram.min,
            max: histogram.max,
            p50: histogram.p50,
            p95: histogram.p95,
            p99: histogram.p99,
            buckets: histogram
                .buckets
                .into_iter()
                .map(|bucket| FacetBucket {
                    lo: bucket.lo,
                    hi: bucket.hi,
                    count: bucket.count,
                })
                .collect(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::Value as Json;

    use super::*;
    use crate::auth::roles::{OrgRole, ProjectOverride};
    use crate::meta::members::set_project_override;
    use crate::test_support::{Seeded, TestApp};

    const WINDOW: &str = "from=2026-01-01T10:00:00Z&to=2026-01-01T11:00:00Z";

    /// A seeded app with the example dataset loaded.
    async fn loaded() -> (TestApp, Seeded) {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.load_example_dataset(seeded.project.id).await;
        (app, seeded)
    }

    async fn facets(app: &TestApp, seeded: &Seeded, query: &str) -> (StatusCode, Json) {
        let response = app
            .request(
                Method::GET,
                &format!("/api/v1/projects/{}/facets?{query}", seeded.project.id),
            )
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        let body = response.json::<Json>();
        (response.status, body)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn values_answer_matches_storage() {
        let (app, seeded) = loaded().await;
        let (status, body) =
            facets(&app, &seeded, &format!("view=traces&field=status&{WINDOW}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let response: FacetsResponse = serde_json::from_value(body).unwrap();
        assert_eq!(response.view, FacetView::Traces);
        assert_eq!(response.kind, FacetKind::Values);
        assert_eq!(response.total, 5);
        assert_eq!(response.missing, 0);
        assert_eq!(response.more, Some(false));
        assert!(response.histogram.is_none());
        let values = response.values.expect("values");
        assert_eq!(
            values
                .iter()
                .map(|value| (value.value.as_str(), value.count))
                .collect::<Vec<_>>(),
            [("ok", 3), ("error", 1), ("unset", 1)]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn histogram_answer_shape() {
        let (app, seeded) = loaded().await;
        let (status, body) = facets(
            &app,
            &seeded,
            &format!("view=traces&field=duration_ms&{WINDOW}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.get("values").is_none(), "{body}");
        assert!(body.get("more").is_none(), "{body}");
        let response: FacetsResponse = serde_json::from_value(body).unwrap();
        assert_eq!(response.kind, FacetKind::Histogram);
        let histogram = response.histogram.expect("histogram");
        assert!(matches!(histogram.buckets.len(), 1 | 20 | 21));
        let sum: u64 = histogram.buckets.iter().map(|bucket| bucket.count).sum();
        assert_eq!(sum, response.total - response.missing);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keys_answer_has_values_and_more() {
        let (app, seeded) = loaded().await;
        let (status, body) = facets(
            &app,
            &seeded,
            &format!("view=traces&field=metadata&{WINDOW}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["kind"], "keys");
        assert_eq!(body["more"], false);
        assert_eq!(body["values"].as_array().map(Vec::len), Some(2));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bad_field_is_400_bad_request() {
        let (app, seeded) = loaded().await;
        let (status, body) = facets(&app, &seeded, "view=traces&field=input").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "bad_request");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bad_filter_is_400_invalid_filter() {
        let (app, seeded) = loaded().await;
        let (status, body) = facets(
            &app,
            &seeded,
            "view=traces&field=status&filter=name%20%3D%20'x'%20)",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "invalid_filter");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sessions_search_is_400() {
        let (app, seeded) = loaded().await;
        let (status, body) =
            facets(&app, &seeded, "view=sessions&field=user_id&search=refund").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "bad_request");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_member_without_project_view_gets_404() {
        let (app, seeded) = loaded().await;
        let member = app
            .add_member(&seeded, "member@example.com", OrgRole::Member)
            .await;
        let (project, member_id) = (seeded.project.id, member.id);
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    set_project_override(conn, project, member_id, ProjectOverride::None).await
                })
            })
            .await
            .unwrap();
        let response = app
            .request(
                Method::GET,
                &format!(
                    "/api/v1/projects/{}/facets?view=traces&field=status",
                    seeded.project.id
                ),
            )
            .header("cookie", &member.cookie)
            .send()
            .await;
        // A project the caller cannot view is hidden: 404, as on the other project routes.
        assert_eq!(response.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_cookie_is_unauthorized() {
        let (app, seeded) = loaded().await;
        let response = app
            .request(
                Method::GET,
                &format!(
                    "/api/v1/projects/{}/facets?view=traces&field=status",
                    seeded.project.id
                ),
            )
            .send()
            .await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    }
}
