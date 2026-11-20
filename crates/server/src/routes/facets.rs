//! Facets (filtering plan D1, D-C-23): a batch of values, keys or histograms over a list's rows.

use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::Method;
use axum::routing::post;
use cetra_storage as storage;

use crate::api::facets::{
    Facet, FacetBucket, FacetHistogram, FacetKind, FacetResult, FacetValue, FacetView,
    FacetsRequest, FacetsResponse,
};
use crate::auth::roles::ProjectPerm;
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::routes::read::{Deadline, check_search, list_scope, open_query};

/// The limit when the request names none.
const DEFAULT_LIMIT: u32 = 10;

/// Largest facet batch body. A batch of 20 fields is a few kilobytes, as the widget query body is.
const FACETS_BODY_LIMIT: usize = 64 * 1024;

/// Facet batch route. Documented; a read by POST (D-H-4).
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![RouteDef {
        method: Method::POST,
        path: "/api/v1/projects/{project}/facets",
        access: Access::Project {
            perm: ProjectPerm::Query,
            session_only: false,
        },
        documented: true,
        handler: post(post_facets).layer(DefaultBodyLimit::max(FACETS_BODY_LIMIT)),
    }]
}

async fn post_facets(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Json(body): error::Json<FacetsRequest>,
) -> Result<error::Json<FacetsResponse>, ApiError> {
    check_search(body.search.as_deref())?;
    let scope = list_scope(
        body.from,
        body.to,
        body.include_eval,
        body.include_experiments,
    )?;
    let fields: Vec<storage::FacetField<'_>> = body
        .fields
        .iter()
        .map(|ask| storage::FacetField {
            field: &ask.field,
            query: ask.q.as_deref(),
            limit: usize::try_from(ask.limit.unwrap_or(DEFAULT_LIMIT)).unwrap_or(usize::MAX),
        })
        .collect();
    let deadline = Deadline::start();
    let ctx = open_query(&state, access.project.org_id, access.project.id, None).await?;
    let data = storage::facets(
        &ctx,
        storage::FacetsRequest {
            view: storage_view(body.view),
            filter: body.filter.as_deref(),
            search: body.search.as_deref(),
            scope,
            fields: &fields,
            timeout: deadline.remaining()?,
        },
    )
    .await?;
    // One result per field, in request order. A field's own error does not fail the others.
    let results = body
        .fields
        .iter()
        .zip(data)
        .enumerate()
        .map(|(index, (ask, result))| {
            let field = ask.field.clone();
            match result {
                Ok(data) => FacetResult {
                    field,
                    facet: Some(facet(data)),
                    error: None,
                },
                Err(error) => {
                    let error = ApiError::QueryAt {
                        field: format!("fields[{index}]"),
                        error,
                    };
                    error.log();
                    FacetResult {
                        field,
                        facet: None,
                        error: Some(error.problem()),
                    }
                }
            }
        })
        .collect();
    Ok(error::Json(FacetsResponse {
        view: body.view,
        results,
    }))
}

fn storage_view(view: FacetView) -> storage::FacetView {
    match view {
        FacetView::Traces => storage::FacetView::Traces,
        FacetView::Spans => storage::FacetView::Spans,
        FacetView::Sessions => storage::FacetView::Sessions,
    }
}

fn facet(data: storage::FacetData) -> Facet {
    let kind = match data.kind {
        storage::FacetKind::Values => FacetKind::Values,
        storage::FacetKind::Keys => FacetKind::Keys,
        storage::FacetKind::Histogram => FacetKind::Histogram,
    };
    let listed = data.kind != storage::FacetKind::Histogram;
    Facet {
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
