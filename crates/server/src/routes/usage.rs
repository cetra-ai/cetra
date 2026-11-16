//! Org usage by UTC day (spec 09, D-P-16, D-P-42).

use axum::extract::{Extension, State};
use axum::routing::get;
use chrono::{DateTime, NaiveDate};

use crate::api::usage::{OrgUsage, UsageParams};
use crate::auth::roles::OrgPerm;
use crate::http::AppState;
use crate::http::access::{Access, OrgAccess, RouteDef};
use crate::http::error::{self, ApiError, field_error};
use crate::meta::usage::org_days;
use crate::usage::month_bounds;

/// Most days one request may span: `to - from` is at most this.
const MAX_SPAN_DAYS: i64 = 365;

/// `GET /api/v1/orgs/{org}/usage`.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![RouteDef {
        method: axum::http::Method::GET,
        path: "/api/v1/orgs/{org}/usage",
        access: Access::Org {
            perm: OrgPerm::View,
            session_only: false,
        },
        documented: true,
        handler: get(get_usage),
    }]
}

async fn get_usage(
    State(state): State<AppState>,
    Extension(access): Extension<OrgAccess>,
    error::Query(params): error::Query<UsageParams>,
) -> Result<error::Json<OrgUsage>, ApiError> {
    let now = state.clock.now_ns();
    let from = match params.from.as_deref() {
        Some(text) => parse_day("from", text)?,
        None => month_bounds(now).0,
    };
    let to = match params.to.as_deref() {
        Some(text) => parse_day("to", text)?,
        None => DateTime::from_timestamp_nanos(now).date_naive(),
    };
    if from > to {
        return Err(ApiError::BadRequest {
            detail: Some("from must not be after to".to_owned()),
            errors: Vec::new(),
        });
    }
    if (to - from).num_days() > MAX_SPAN_DAYS {
        return Err(ApiError::BadRequest {
            detail: Some("the range may span at most 365 days".to_owned()),
            errors: Vec::new(),
        });
    }
    let days = org_days(state.meta.reader(), access.org.id, from, to).await?;
    Ok(error::Json(OrgUsage {
        from: from.format("%Y-%m-%d").to_string(),
        to: to.format("%Y-%m-%d").to_string(),
        days,
    }))
}

/// A `YYYY-MM-DD` day, or a `400` naming `field`.
fn parse_day(field: &str, text: &str) -> Result<NaiveDate, ApiError> {
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .map_err(|_| field_error(field, "expected YYYY-MM-DD"))
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, header};
    use chrono::{DateTime, Utc};
    use serde_json::{Value, json};

    use crate::auth::session::start_session;
    use crate::meta::users::{NewUser, insert_user};
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

    /// Adds usage to `day_ns`'s day for `project` and flushes it to `usage_daily`.
    async fn add_usage(
        app: &TestApp,
        org: OrgId,
        project: ProjectId,
        day_ns: i64,
        spans: u64,
        scores: u64,
    ) {
        app.state.usage.add(org, project, day_ns, spans, 0, scores);
        app.state.usage.flush(&app.state.meta).await.unwrap();
    }

    /// A signed-in user who is not a member of any org; returns the session cookie.
    async fn outsider(app: &TestApp) -> String {
        let now = app.state.clock.now_ns();
        let user = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_user(
                        conn,
                        NewUser {
                            email: "outsider@example.com".to_owned(),
                            name: None,
                            password_hash: None,
                            email_verified_at: Some(now),
                            email_verified_via: Some(
                                crate::meta::users::EmailVerifiedVia::MailToken,
                            ),
                            is_instance_admin: false,
                        },
                        now,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        let set_cookie = start_session(
            &app.state,
            user.id,
            None,
            None,
            crate::auth::session::SessionOrigin::Password,
        )
        .await
        .unwrap();
        set_cookie
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }

    async fn get(app: &TestApp, cookie: &str, query: &str) -> TestResponse {
        app.request(Method::GET, &format!("/api/v1/orgs/acme/usage{query}"))
            .header(header::COOKIE.as_str(), cookie)
            .send()
            .await
    }

    fn days(body: &Value) -> Vec<(String, u64)> {
        body["days"]
            .as_array()
            .unwrap()
            .iter()
            .map(|day| {
                (
                    day["day"].as_str().unwrap().to_owned(),
                    day["units"].as_u64().unwrap(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn two_projects_on_a_day_are_summed() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let org = seeded.org_id;
        let day = at("2026-01-01T12:00:00Z");
        add_usage(&app, org, seeded.project.id, day, 10, 5).await;
        add_usage(&app, org, ProjectId::new_v7(), day, 20, 0).await;

        let response = get(&app, &seeded.cookie, "?from=2026-01-01&to=2026-01-01").await;
        assert_eq!(response.status.as_u16(), 200);
        let body: Value = response.json();
        assert_eq!(body["from"], json!("2026-01-01"));
        assert_eq!(body["to"], json!("2026-01-01"));
        assert_eq!(
            body["days"],
            json!([{
                "day": "2026-01-01",
                "spans": 30,
                "scores": 5,
                "units": 35,
                "bytes": 0,
                "judge_input_tokens": 0,
                "judge_output_tokens": 0,
                "judge_cost_usd": 0.0
            }])
        );
    }

    #[tokio::test]
    async fn bounds_are_inclusive_and_days_without_usage_are_absent() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let org = seeded.org_id;
        for day in ["2026-01-01", "2026-01-02", "2026-01-03", "2026-01-04"] {
            add_usage(
                &app,
                org,
                seeded.project.id,
                at(&format!("{day}T12:00:00Z")),
                1,
                0,
            )
            .await;
        }
        add_usage(
            &app,
            org,
            seeded.project.id,
            at("2026-01-06T12:00:00Z"),
            1,
            0,
        )
        .await;

        let response = get(&app, &seeded.cookie, "?from=2026-01-02&to=2026-01-04").await;
        assert_eq!(response.status.as_u16(), 200);
        assert_eq!(
            days(&response.json()),
            vec![
                ("2026-01-02".to_owned(), 1),
                ("2026-01-03".to_owned(), 1),
                ("2026-01-04".to_owned(), 1),
            ]
        );
    }

    #[tokio::test]
    async fn the_default_range_is_this_month_to_today() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let org = seeded.org_id;
        app.state.clock.set(at("2026-01-10T12:00:00Z"));
        add_usage(
            &app,
            org,
            seeded.project.id,
            at("2025-12-31T12:00:00Z"),
            1,
            0,
        )
        .await;
        add_usage(
            &app,
            org,
            seeded.project.id,
            at("2026-01-05T12:00:00Z"),
            2,
            0,
        )
        .await;
        add_usage(
            &app,
            org,
            seeded.project.id,
            at("2026-01-10T12:00:00Z"),
            3,
            0,
        )
        .await;

        let response = get(&app, &seeded.cookie, "").await;
        assert_eq!(response.status.as_u16(), 200);
        let body: Value = response.json();
        assert_eq!(body["from"], json!("2026-01-01"));
        assert_eq!(body["to"], json!("2026-01-10"));
        assert_eq!(
            days(&body),
            vec![("2026-01-05".to_owned(), 2), ("2026-01-10".to_owned(), 3)]
        );
    }

    #[tokio::test]
    async fn from_after_to_is_refused() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let response = get(&app, &seeded.cookie, "?from=2026-01-05&to=2026-01-04").await;
        assert_eq!(response.status.as_u16(), 400);
    }

    #[tokio::test]
    async fn a_range_of_365_days_is_allowed_and_366_is_refused() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let allowed = get(&app, &seeded.cookie, "?from=2025-01-01&to=2026-01-01").await;
        assert_eq!(allowed.status.as_u16(), 200);
        let refused = get(&app, &seeded.cookie, "?from=2024-12-31&to=2026-01-01").await;
        assert_eq!(refused.status.as_u16(), 400);
    }

    #[tokio::test]
    async fn bad_dates_are_refused() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        for query in ["?from=yesterday", "?to=2026-02-30", "?from=2026/01/01"] {
            let response = get(&app, &seeded.cookie, query).await;
            assert_eq!(response.status.as_u16(), 400, "{query}");
        }
        let response = get(&app, &seeded.cookie, "?from=2026-01-01&to=2026-01-01").await;
        assert_eq!(response.status.as_u16(), 200);
    }

    #[tokio::test]
    async fn outsiders_get_404() {
        let app = TestApp::new().await;
        app.seed().await;
        let cookie = outsider(&app).await;
        let response = get(&app, &cookie, "").await;
        assert_eq!(response.status.as_u16(), 404);
    }
}
