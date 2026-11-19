//! Session reads (spec 09 § Observability data).

use axum::extract::{Extension, State};
use axum::http::Method;
use axum::routing::get;
use cetra_storage as storage;
use serde::Deserialize;

use crate::api::common::{ExclusionParams, Page};
use crate::api::convert;
use crate::api::sessions::{
    SessionDetail, SessionListParams, SessionSort, SessionSummary, SessionTurn,
};
use crate::auth::roles::ProjectPerm;
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::routes::read::{
    Deadline, attach_session_scores, attach_trace_scores, list_scope, open_query, page_limit,
    sort_order,
};

/// Session read routes. All are documented and need project view.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/sessions",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_sessions),
        },
        RouteDef {
            method: Method::GET,
            path: "/api/v1/projects/{project}/sessions/{session_id}",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_session),
        },
    ]
}

#[derive(Debug, Deserialize)]
struct SessionPath {
    session_id: String,
}

async fn list_sessions(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<SessionListParams>,
) -> Result<error::Json<Page<SessionSummary>>, ApiError> {
    let scope = list_scope(
        params.from,
        params.to,
        params.include_eval,
        params.include_experiments,
    )?;
    let deadline = Deadline::start();
    let ctx = open_query(&state, access.project.org_id, access.project.id, None).await?;
    let page = storage::session_list(
        &ctx,
        storage::SessionListRequest {
            filter: params.filter.as_deref(),
            scope,
            sort: session_sort(params.sort),
            order: sort_order(params.order),
            limit: page_limit(params.limit),
            cursor: params.cursor.as_deref(),
            timeout: deadline.remaining()?,
        },
    )
    .await?;
    let mut data = convert::session_summaries(&page.batches)?;
    attach_session_extras(&ctx, &mut data, scope, deadline).await?;
    attach_session_scores(&ctx, &mut data, deadline).await?;
    Ok(error::Json(Page {
        data,
        next_cursor: page.next_cursor,
    }))
}

async fn get_session(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(path): error::Path<SessionPath>,
    error::Query(params): error::Query<ExclusionParams>,
) -> Result<error::Json<SessionDetail>, ApiError> {
    let deadline = Deadline::start();
    let ctx = open_query(&state, access.project.org_id, access.project.id, None).await?;
    let data = storage::session_detail(
        &ctx,
        &path.session_id,
        params.include_eval,
        params.include_experiments,
        deadline.remaining()?,
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    let mut session = convert::session_summaries(std::slice::from_ref(&data.session))?
        .pop()
        .ok_or(ApiError::NotFound)?;
    // No window: the detail covers the whole session, with the same exclusions as `session_detail`.
    let scope = storage::ListScope {
        window: None,
        include_eval: params.include_eval,
        include_experiments: params.include_experiments,
    };
    attach_session_extras(&ctx, std::slice::from_mut(&mut session), scope, deadline).await?;
    attach_session_scores(&ctx, std::slice::from_mut(&mut session), deadline).await?;
    let mut traces = convert::trace_summaries(&data.turns)?;
    attach_trace_scores(&ctx, &mut traces, deadline).await?;
    let mut roots = convert::root_payloads(&data.roots)?;
    let turns = traces
        .into_iter()
        .map(|trace| {
            let (input, output) = roots.remove(&trace.trace_id).unwrap_or((None, None));
            SessionTurn {
                trace,
                input,
                output,
            }
        })
        .collect();
    Ok(error::Json(SessionDetail {
        session,
        turns,
        truncated: data.truncated,
    }))
}

/// Fills the extras of each session (spec 09 § Observability data): its error trace count and
/// previews, from the traces inside `scope`. A session without traces there keeps 0 and no previews.
///
/// Callers pass at most one page of sessions.
async fn attach_session_extras(
    ctx: &storage::QueryContext,
    sessions: &mut [SessionSummary],
    scope: storage::ListScope,
    deadline: Deadline,
) -> Result<(), ApiError> {
    if sessions.is_empty() {
        return Ok(());
    }
    let ids: Vec<String> = sessions
        .iter()
        .map(|session| session.session_id.clone())
        .collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let batches = storage::session_extras(
        ctx,
        storage::SessionExtrasRequest {
            scope,
            session_ids: &refs,
            timeout: deadline.remaining()?,
        },
    )
    .await?;
    let mut extras = convert::session_extras(&batches)?;
    for session in sessions.iter_mut() {
        if let Some(extra) = extras.remove(&session.session_id) {
            session.error_trace_count = extra.error_trace_count;
            session.first_input_preview = extra.first_input_preview;
            session.last_output_preview = extra.last_output_preview;
        }
    }
    Ok(())
}

fn session_sort(sort: SessionSort) -> storage::SessionSort {
    match sort {
        SessionSort::StartTime => storage::SessionSort::StartTime,
        SessionSort::EndTime => storage::SessionSort::EndTime,
        SessionSort::TraceCount => storage::SessionSort::TraceCount,
        SessionSort::CostTotal => storage::SessionSort::CostTotal,
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use cetra_core::score::ScoreValue;
    use serde_json::Value as Json;

    use super::*;
    use crate::api::payload::Payload;
    use crate::test_support::{Seeded, TestApp};

    const TRACE_1: &str = "a1000000000000000000000000000001";
    const TRACE_2: &str = "a1000000000000000000000000000002";
    const TRACE_5: &str = "a1000000000000000000000000000005";

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
    async fn list_orders_sessions_and_attaches_latest_scores() {
        let (app, seeded) = loaded().await;
        let (status, body) = get_json(&app, &seeded, "/sessions").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let page: Page<SessionSummary> = serde_json::from_value(body).unwrap();
        let ids: Vec<&str> = page.data.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, ["sess-002", "sess-001"]);

        let sess_001 = &page.data[1];
        assert_eq!(sess_001.trace_count, 2);
        assert_eq!(sess_001.tags, ["beta", "refund"]);
        assert_eq!(sess_001.scores.len(), 1);
        assert_eq!(sess_001.scores[0].name, "resolved");
        assert_eq!(sess_001.scores[0].value, ScoreValue::Boolean(true));

        let sess_002 = &page.data[0];
        assert!(sess_002.tags.is_empty());
        assert!(sess_002.scores.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn include_eval_adds_the_evaluation_trace_to_its_session() {
        let (app, seeded) = loaded().await;
        let (status, body) = get_json(&app, &seeded, "/sessions?include_eval=true").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let page: Page<SessionSummary> = serde_json::from_value(body).unwrap();
        let sess_001 = page
            .data
            .iter()
            .find(|s| s.session_id == "sess-001")
            .unwrap();
        assert_eq!(sess_001.trace_count, 3);
        assert_eq!(sess_001.tags, ["beta", "eval", "refund"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn detail_lists_turns_oldest_first_with_root_payloads() {
        let (app, seeded) = loaded().await;
        let (status, body) = get_json(&app, &seeded, "/sessions/sess-001").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let detail: SessionDetail = serde_json::from_value(body).unwrap();
        assert!(!detail.truncated);
        assert_eq!(detail.session.scores.len(), 1);
        let ids: Vec<String> = detail
            .turns
            .iter()
            .map(|turn| turn.trace.trace_id.to_string())
            .collect();
        assert_eq!(ids, [TRACE_1, TRACE_2]);
        assert!(matches!(detail.turns[0].input, Some(Payload::Text(_))));

        let (_, traces) = get_json(&app, &seeded, "/traces").await;
        let listed = traces["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["trace_id"] == TRACE_1)
            .unwrap();
        assert_eq!(
            serde_json::to_value(&detail.turns[0].trace.scores).unwrap(),
            listed["scores"]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn detail_with_include_eval_adds_the_evaluation_turn() {
        let (app, seeded) = loaded().await;
        let (status, body) = get_json(&app, &seeded, "/sessions/sess-001?include_eval=true").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let detail: SessionDetail = serde_json::from_value(body).unwrap();
        let ids: Vec<String> = detail
            .turns
            .iter()
            .map(|turn| turn.trace.trace_id.to_string())
            .collect();
        assert_eq!(ids, [TRACE_1, TRACE_5, TRACE_2]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_session_is_not_found() {
        let (app, seeded) = loaded().await;
        let (status, body) = get_json(&app, &seeded, "/sessions/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["code"], "not_found");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_and_detail_carry_the_session_extras() {
        let (app, seeded) = loaded().await;
        // Expected values come from the committed detail fixture of sess-001, not from this server.
        let fixture: Json = serde_json::from_str(include_str!(
            "../../../../fixtures/api/responses/sessions/sess-001.json"
        ))
        .unwrap();
        let turns = fixture["turns"].as_array().unwrap();
        let errors = turns
            .iter()
            .filter(|turn| turn["trace"]["status"] == "error")
            .count();
        let first = turns[0]["trace"]["input_preview"].as_str().unwrap();
        let last = turns[turns.len() - 1]["trace"]["output_preview"]
            .as_str()
            .unwrap();

        let (status, body) = get_json(&app, &seeded, "/sessions").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let page: Page<SessionSummary> = serde_json::from_value(body).unwrap();
        let listed = page
            .data
            .iter()
            .find(|s| s.session_id == "sess-001")
            .unwrap();
        assert_eq!(listed.error_trace_count, i64::try_from(errors).unwrap());
        assert_eq!(listed.first_input_preview.as_deref(), Some(first));
        assert_eq!(listed.last_output_preview.as_deref(), Some(last));

        let (status, body) = get_json(&app, &seeded, "/sessions/sess-001").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let detail: SessionDetail = serde_json::from_value(body).unwrap();
        assert_eq!(detail.session.error_trace_count, listed.error_trace_count);
        assert_eq!(
            detail.session.first_input_preview,
            listed.first_input_preview
        );
        assert_eq!(
            detail.session.last_output_preview,
            listed.last_output_preview
        );
    }
}
