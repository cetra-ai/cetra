//! `GET /api/internal/v1/usage`: the control plane reads every org's usage by day (D-P-24, D-P-39).

use axum::extract::State;
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::common::Page;
use crate::api::internal::{InternalUsageQuery, InternalUsageRow};
use crate::http::AppState;
use crate::http::access::{Access, RouteDef};
use crate::http::error::{self, ApiError, field_error};
use crate::meta::usage::all_orgs_page;

/// Most days one request may span: `to - from` is at most this.
const MAX_SPAN_DAYS: i64 = 92;
const DEFAULT_LIMIT: u32 = 1_000;
const MAX_LIMIT: u32 = 10_000;

/// The internal usage route. The control plane holds a `control_plane` token with scope `usage`.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![RouteDef {
        method: axum::http::Method::GET,
        path: "/api/internal/v1/usage",
        access: Access::ControlPlane { scope: "usage" },
        documented: true,
        handler: get(list_usage),
    }]
}

/// The position after the last row of a page: its day and org.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    day: String,
    org_id: Uuid,
}

/// `GET /api/internal/v1/usage`. Rows ordered by `(day, org_id)`, keyset-paged.
async fn list_usage(
    State(state): State<AppState>,
    error::Query(params): error::Query<InternalUsageQuery>,
) -> Result<error::Json<Page<InternalUsageRow>>, ApiError> {
    let from = parse_day("from", &params.from)?;
    let to = parse_day("to", &params.to)?;
    if from > to {
        return Err(field_error("to", "must not be before from"));
    }
    if (to - from).num_days() > MAX_SPAN_DAYS {
        return Err(field_error("to", "the range may span at most 92 days"));
    }
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(field_error("limit", "must be 1 to 10000"));
    }
    let after = params.cursor.as_deref().map(decode_cursor).transpose()?;
    // One row more than the page tells whether a next page exists.
    let mut rows = all_orgs_page(
        state.meta.reader(),
        from,
        to,
        after
            .as_ref()
            .map(|cursor| (cursor.day.as_str(), cursor.org_id)),
        limit + 1,
    )
    .await?;
    let page_size = usize::try_from(limit).unwrap_or(usize::MAX);
    let has_next = rows.len() > page_size;
    rows.truncate(page_size);
    let next_cursor = match (has_next, rows.last()) {
        (true, Some(last)) => Some(encode_cursor(&last.day, last.org_id)?),
        _ => None,
    };
    Ok(error::Json(Page {
        data: rows,
        next_cursor,
    }))
}

/// A `YYYY-MM-DD` day, or a `400` naming `field`.
fn parse_day(field: &str, text: &str) -> Result<NaiveDate, ApiError> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .map_err(|_| field_error(field, "expected YYYY-MM-DD"))
}

fn encode_cursor(day: &str, org_id: Uuid) -> Result<String, ApiError> {
    let bytes = serde_json::to_vec(&Cursor {
        day: day.to_owned(),
        org_id,
    })
    .map_err(|error| ApiError::Internal(Box::new(error)))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Decodes a cursor. A malformed one, or one whose day is not `YYYY-MM-DD`, is a `400` on
/// field `cursor`.
fn decode_cursor(text: &str) -> Result<Cursor, ApiError> {
    let bytes = URL_SAFE_NO_PAD.decode(text).map_err(|_| invalid_cursor())?;
    let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| invalid_cursor())?;
    let day = NaiveDate::parse_from_str(&cursor.day, "%Y-%m-%d").map_err(|_| invalid_cursor())?;
    // `%Y-%m-%d` also parses `2026-1-5`; only the padded form is the day the cursor wrote.
    if day.format("%Y-%m-%d").to_string() != cursor.day {
        return Err(invalid_cursor());
    }
    Ok(cursor)
}

fn invalid_cursor() -> ApiError {
    field_error("cursor", "invalid cursor")
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, header};
    use chrono::{DateTime, Utc};
    use serde_json::Value;

    use super::*;
    use crate::test_support::{TestApp, TestResponse};
    use cetra_core::ids::{OrgId, ProjectId};

    /// Nanoseconds of an RFC 3339 instant.
    fn at(rfc3339: &str) -> i64 {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
            .timestamp_nanos_opt()
            .unwrap()
    }

    /// Adds usage to `day_ns`'s day for `org` and `project` and flushes it to `usage_daily`.
    async fn add_usage(app: &TestApp, org: OrgId, project: ProjectId, day_ns: i64, spans: u64) {
        app.state.usage.add(org, project, day_ns, spans, 0, 0);
        app.state.usage.flush(&app.state.meta).await.unwrap();
    }

    async fn get(app: &TestApp, token: &str, query: &str) -> TestResponse {
        app.request(Method::GET, &format!("/api/internal/v1/usage{query}"))
            .control_plane(token)
            .send()
            .await
    }

    /// Two orgs over three days: the seeded `first` org has 15 units on day 1 (two projects)
    /// and 3 on day 3; `second` has 1 on day 1 and 2 on day 2. Returns `(first, second)`.
    async fn seed_usage(app: &TestApp) -> (OrgId, OrgId) {
        let seeded = app.seed().await;
        let first = seeded.org_id;
        let second = OrgId::new_v7();
        let day1 = at("2026-01-01T12:00:00Z");
        add_usage(app, first, seeded.project.id, day1, 10).await;
        add_usage(app, first, ProjectId::new_v7(), day1, 5).await;
        add_usage(app, second, ProjectId::new_v7(), day1, 1).await;
        add_usage(
            app,
            second,
            ProjectId::new_v7(),
            at("2026-01-02T12:00:00Z"),
            2,
        )
        .await;
        add_usage(app, first, seeded.project.id, at("2026-01-03T12:00:00Z"), 3).await;
        (first, second)
    }

    fn rows(page: &Page<InternalUsageRow>) -> Vec<(Uuid, String, u64)> {
        page.data
            .iter()
            .map(|row| (row.org_id, row.day.clone(), row.units))
            .collect()
    }

    #[tokio::test]
    async fn rows_are_summed_per_org_and_day_in_order() {
        let app = TestApp::new().await;
        let (first, second) = seed_usage(&app).await;
        let response = get(&app, "usage", "?from=2026-01-01&to=2026-01-03").await;
        assert_eq!(response.status.as_u16(), 200);
        let page: Page<InternalUsageRow> = response.json();
        assert_eq!(page.next_cursor, None);
        // Day 1 lists the orgs by id; then day 2 and day 3.
        let mut day1 = vec![
            (first.as_uuid(), "2026-01-01".to_owned(), 15),
            (second.as_uuid(), "2026-01-01".to_owned(), 1),
        ];
        day1.sort_by_key(|row| row.0);
        let mut expected = day1;
        expected.push((second.as_uuid(), "2026-01-02".to_owned(), 2));
        expected.push((first.as_uuid(), "2026-01-03".to_owned(), 3));
        assert_eq!(rows(&page), expected);
        let first_row = &page.data[0];
        assert_eq!(first_row.spans, first_row.units);
        assert_eq!(first_row.scores, 0);
        assert_eq!(first_row.bytes, 0);
    }

    #[tokio::test]
    async fn pages_follow_the_cursor_to_the_end() {
        let app = TestApp::new().await;
        seed_usage(&app).await;
        let token = "usage";
        let full: Page<InternalUsageRow> = get(&app, token, "?from=2026-01-01&to=2026-01-03")
            .await
            .json();

        let mut collected = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0;
        loop {
            let query = match &cursor {
                Some(cursor) => {
                    format!("?from=2026-01-01&to=2026-01-03&limit=2&cursor={cursor}")
                }
                None => "?from=2026-01-01&to=2026-01-03&limit=2".to_owned(),
            };
            let response = get(&app, token, &query).await;
            assert_eq!(response.status.as_u16(), 200);
            let page: Page<InternalUsageRow> = response.json();
            assert!(page.data.len() <= 2);
            pages += 1;
            collected.extend(page.data);
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
            assert!(pages < 10, "the cursor never ends");
        }
        assert_eq!(pages, 2);
        assert_eq!(collected, full.data);
    }

    #[tokio::test]
    async fn the_range_is_inclusive_and_at_most_92_days() {
        let app = TestApp::new().await;
        seed_usage(&app).await;
        let token = "usage";
        // 2026-01-01 + 92 days is 2026-04-03.
        let allowed = get(&app, token, "?from=2026-01-01&to=2026-04-03").await;
        assert_eq!(allowed.status.as_u16(), 200);
        let refused = get(&app, token, "?from=2026-01-01&to=2026-04-04").await;
        assert_eq!(refused.status.as_u16(), 400);
        let inverted = get(&app, token, "?from=2026-01-03&to=2026-01-01").await;
        assert_eq!(inverted.status.as_u16(), 400);
    }

    #[tokio::test]
    async fn missing_or_bad_dates_are_refused() {
        let app = TestApp::new().await;
        seed_usage(&app).await;
        let token = "usage";
        for query in [
            "?to=2026-01-03",
            "?from=2026-01-01",
            "",
            "?from=yesterday&to=2026-01-03",
            "?from=2026-01-01&to=2026-02-30",
        ] {
            let response = get(&app, token, query).await;
            assert_eq!(response.status.as_u16(), 400, "{query}");
        }
    }

    #[tokio::test]
    async fn limit_is_between_1_and_10000() {
        let app = TestApp::new().await;
        seed_usage(&app).await;
        let token = "usage";
        let base = "?from=2026-01-01&to=2026-01-03";
        for limit in ["0", "10001", "-1"] {
            let response = get(&app, token, &format!("{base}&limit={limit}")).await;
            assert_eq!(response.status.as_u16(), 400, "limit {limit}");
        }
        let response = get(&app, token, &format!("{base}&limit=10000")).await;
        assert_eq!(response.status.as_u16(), 200);
        let page: Value = response.json();
        assert_eq!(page["data"].as_array().map(Vec::len), Some(4));
    }

    #[tokio::test]
    async fn a_bad_cursor_is_refused() {
        let app = TestApp::new().await;
        seed_usage(&app).await;
        let token = "usage";
        let not_json = URL_SAFE_NO_PAD.encode(b"not json");
        let bad_day = encode_cursor("2026-13-01", Uuid::now_v7()).unwrap();
        let unpadded = encode_cursor("2026-1-5", Uuid::now_v7()).unwrap();
        for cursor in [
            "!!!",
            not_json.as_str(),
            bad_day.as_str(),
            unpadded.as_str(),
        ] {
            let response = get(
                &app,
                token,
                &format!("?from=2026-01-01&to=2026-01-03&cursor={cursor}"),
            )
            .await;
            assert_eq!(response.status.as_u16(), 400, "{cursor}");
            assert_eq!(response.problem().errors[0].field, "cursor");
        }
    }

    #[tokio::test]
    async fn needs_the_usage_scope() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let query = "?from=2026-01-01&to=2026-01-03";

        let orgs = get(&app, "orgs", query).await;
        assert_eq!(orgs.status.as_u16(), 403);

        let session = app
            .request(Method::GET, &format!("/api/internal/v1/usage{query}"))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(session.status.as_u16(), 403);

        let anonymous = app
            .request(Method::GET, &format!("/api/internal/v1/usage{query}"))
            .send()
            .await;
        assert_eq!(anonymous.status.as_u16(), 401);
    }
}
