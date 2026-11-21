//! Facets: the values, keys or histogram of one field over a list's rows (filtering plan D1).
//!
//! `POST /api/v1/projects/{project}/facets` answers a batch of fields over one list's rows
//! (D-C-24). The field grammar and the counting rules are in the operation's description.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::common::{Problem, TimeParam};

/// Body of `POST /api/v1/projects/{project}/facets`: fields over the rows of one list (D-C-24).
// Deserialize only, because `TimeParam` has no `Serialize`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FacetsRequest {
    /// The list the facets describe.
    pub view: FacetView,
    /// Inclusive range start. RFC 3339 or epoch milliseconds.
    #[serde(default)]
    pub from: Option<TimeParam>,
    /// Exclusive range end. RFC 3339 or epoch milliseconds.
    #[serde(default)]
    pub to: Option<TimeParam>,
    /// Filter DSL expression over `view`. Blank means no filter.
    #[serde(default)]
    pub filter: Option<String>,
    /// Full-text search. `traces` and `spans` only. At most 1,024 bytes.
    #[serde(default)]
    pub search: Option<String>,
    /// Include traces from the reserved environments (`cetra-eval`, `cetra-playground`, `cetra-assistant`). Default false.
    #[serde(default)]
    pub include_eval: bool,
    /// Include experiment traces. Default false.
    #[serde(default)]
    pub include_experiments: bool,
    /// The fields, 1 to 20. Answered in this order.
    pub fields: Vec<FacetFieldRequest>,
}

/// One field of a facet batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FacetFieldRequest {
    /// `column` or `map['key']`. A bare map column facets its keys.
    pub field: String,
    /// Most values or keys returned (1 to 100, default 10). Ignored by histograms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1, maximum = 100)]
    pub limit: Option<u32>,
    /// Case-insensitive substring the value or key must contain. 1 to 200 characters after
    /// trimming. Ignored by histograms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
}

/// The list a facet describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FacetView {
    /// The traces list.
    Traces,
    /// The spans list.
    Spans,
    /// The sessions list, one row per session.
    Sessions,
}

/// What a facet describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FacetKind {
    /// Values of a string, boolean, list or `map['key']` field.
    Values,
    /// The keys of a bare map field.
    Keys,
    /// A histogram of a number field.
    Histogram,
}

/// One value or key and the rows under it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FacetValue {
    /// The value or key. Booleans are `true` or `false`.
    pub value: String,
    /// Rows counted under it.
    pub count: u64,
}

/// One histogram bucket. `[lo, hi)`, except the last bucket, which is `[lo, hi]`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct FacetBucket {
    /// Lower bound.
    pub lo: f64,
    /// Upper bound.
    pub hi: f64,
    /// Rows in the bucket.
    pub count: u64,
}

/// A histogram of a number field. `min` and `max` are exact; the percentiles are approximate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct FacetHistogram {
    /// Smallest value.
    pub min: f64,
    /// Largest value.
    pub max: f64,
    /// Median.
    pub p50: f64,
    /// 95th percentile.
    pub p95: f64,
    /// 99th percentile.
    pub p99: f64,
    /// Every bucket, empty ones included.
    pub buckets: Vec<FacetBucket>,
}

/// The answer of a facet batch: one result per requested field, in request order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct FacetsResponse {
    /// The list the facets describe.
    pub view: FacetView,
    /// One result per requested field, in request order.
    pub results: Vec<FacetResult>,
}

/// One field's result: exactly one of `facet` and `error`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct FacetResult {
    /// The field, as requested.
    pub field: String,
    /// The facet, when the field was answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facet: Option<Facet>,
    /// Why this field was not answered. The other fields are not affected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Problem>,
}

/// One field's facet.
///
/// `values` and `more` are present for kinds `values` and `keys`. `histogram` is present for
/// kind `histogram` when at least one row has a value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Facet {
    /// What the answer describes.
    pub kind: FacetKind,
    /// Rows counted.
    pub total: u64,
    /// Rows with no value: null, or an empty list or map.
    pub missing: u64,
    /// At most `limit` values or keys, by count descending then value ascending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<FacetValue>>,
    /// True when more distinct values matched than `limit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub more: Option<bool>,
    /// The histogram of a number field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub histogram: Option<FacetHistogram>,
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::{Value as Json, json};

    use super::*;
    use crate::auth::roles::{OrgRole, ProjectOverride};
    use crate::meta::members::set_project_override;
    use crate::test_support::{Seeded, TestApp};

    /// A seeded app with the example dataset loaded.
    async fn loaded() -> (TestApp, Seeded) {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        app.load_example_dataset(seeded.project.id).await;
        (app, seeded)
    }

    /// A batch over the hour the tests share.
    fn request(view: &str, fields: &Json) -> Json {
        json!({
            "view": view,
            "from": "2026-01-01T10:00:00Z",
            "to": "2026-01-01T11:00:00Z",
            "fields": fields,
        })
    }

    /// `POST` of `body` to the project's facets with the seeded owner's cookie.
    async fn facets(app: &TestApp, seeded: &Seeded, body: &Json) -> (StatusCode, Json) {
        let response = app
            .request(
                Method::POST,
                &format!("/api/v1/projects/{}/facets", seeded.project.id),
            )
            .header("cookie", &seeded.cookie)
            .json(body)
            .send()
            .await;
        let body = response.json::<Json>();
        (response.status, body)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn values_answer_matches_storage() {
        let (app, seeded) = loaded().await;
        let (status, body) = facets(
            &app,
            &seeded,
            &request("traces", &json!([{ "field": "status" }])),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let response: FacetsResponse = serde_json::from_value(body).unwrap();
        assert_eq!(response.view, FacetView::Traces);
        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].field, "status");
        assert!(response.results[0].error.is_none());
        let facet = response.results[0].facet.as_ref().expect("facet");
        assert_eq!(facet.kind, FacetKind::Values);
        assert_eq!(facet.total, 5);
        assert_eq!(facet.missing, 0);
        assert_eq!(facet.more, Some(false));
        assert!(facet.histogram.is_none());
        let values = facet.values.as_ref().expect("values");
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
            &request("traces", &json!([{ "field": "duration_ms" }])),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let facet_json = &body["results"][0]["facet"];
        assert!(facet_json.get("values").is_none(), "{body}");
        assert!(facet_json.get("more").is_none(), "{body}");
        let response: FacetsResponse = serde_json::from_value(body).unwrap();
        let facet = response.results[0].facet.as_ref().expect("facet");
        assert_eq!(facet.kind, FacetKind::Histogram);
        let histogram = facet.histogram.as_ref().expect("histogram");
        assert!(matches!(histogram.buckets.len(), 1 | 20 | 21));
        let sum: u64 = histogram.buckets.iter().map(|bucket| bucket.count).sum();
        assert_eq!(sum, facet.total - facet.missing);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keys_answer_has_values_and_more() {
        let (app, seeded) = loaded().await;
        let (status, body) = facets(
            &app,
            &seeded,
            &request("traces", &json!([{ "field": "metadata" }])),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let facet = &body["results"][0]["facet"];
        assert_eq!(facet["kind"], "keys");
        assert_eq!(facet["more"], false);
        assert_eq!(facet["values"].as_array().map(Vec::len), Some(2));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bad_field_is_a_field_error() {
        let (app, seeded) = loaded().await;
        let (status, body) = facets(
            &app,
            &seeded,
            &request(
                "traces",
                &json!([{ "field": "status" }, { "field": "input" }]),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["results"][0]["field"], "status");
        assert_eq!(body["results"][0]["facet"]["kind"], "values");
        assert_eq!(body["results"][1]["field"], "input");
        assert!(body["results"][1]["facet"].is_null(), "{body}");
        assert_eq!(body["results"][1]["error"]["code"], "bad_request");
        assert_eq!(body["results"][1]["error"]["status"], 400);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bad_filter_is_400_invalid_filter() {
        let (app, seeded) = loaded().await;
        let mut batch = request("traces", &json!([{ "field": "status" }]));
        batch["filter"] = json!("name = 'x' )");
        let (status, body) = facets(&app, &seeded, &batch).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "invalid_filter");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sessions_search_is_400() {
        let (app, seeded) = loaded().await;
        let batch = json!({
            "view": "sessions",
            "search": "refund",
            "fields": [{ "field": "user_id" }],
        });
        let (status, body) = facets(&app, &seeded, &batch).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "bad_request");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn empty_or_too_many_fields_is_400() {
        let (app, seeded) = loaded().await;
        let (status, body) = facets(&app, &seeded, &request("traces", &json!([]))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "bad_request");

        let too_many: Vec<Json> = (0..21).map(|_| json!({ "field": "status" })).collect();
        let (status, body) =
            facets(&app, &seeded, &request("traces", &Json::Array(too_many))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "bad_request");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_member_is_400() {
        let (app, seeded) = loaded().await;
        let (status, body) = facets(
            &app,
            &seeded,
            &request("traces", &json!([{ "field": "status", "size": 3 }])),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["code"], "bad_request");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_is_not_a_route() {
        let (app, seeded) = loaded().await;
        let response = app
            .request(
                Method::GET,
                &format!(
                    "/api/v1/projects/{}/facets?view=traces&field=status",
                    seeded.project.id
                ),
            )
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED);
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
                Method::POST,
                &format!("/api/v1/projects/{}/facets", seeded.project.id),
            )
            .header("cookie", &member.cookie)
            .json(&request("traces", &json!([{ "field": "status" }])))
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
                Method::POST,
                &format!("/api/v1/projects/{}/facets", seeded.project.id),
            )
            .json(&request("traces", &json!([{ "field": "status" }])))
            .send()
            .await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    }
}
