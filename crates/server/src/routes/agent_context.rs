//! `POST /v1/agent-context`: stores a coding agent's session context (D-CA-9). The route takes
//! an `sk` key with the `ingest` scope and is served on the main port and on the OTLP port.
//! The context is kept in memory and written to `meta.db` first.

use std::sync::PoisonError;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use cetra_core::ids::ProjectId;
use cetra_otel::{AgentContext, canonical_repository};

use crate::agent_context::{MAX_PER_PROJECT, REWRITE_AFTER_NS};
use crate::api::agent_context::AgentContextBody;
use crate::api::common::FieldError;
use crate::http::AppState;
use crate::http::access::ProjectAccess;
use crate::http::error::{self, ApiError};
use crate::meta::agent_contexts::{AgentContextRow, delete_oldest, upsert_context};
use crate::routes::otlp::refuse_over_cap;

/// The agents whose hook posts a context, by their wire names (D-CA-9).
const AGENTS: [&str; 4] = ["claude-code", "codex", "gemini-cli", "copilot"];
/// Longest session id, branch and person, in characters (D-CA-9).
const MAX_TEXT: usize = 256;

/// A body that passed validation. The repository is canonical, the branch and person are
/// trimmed, and the person is lowercased. Each optional field keeps the body's distinction
/// between absent (`None`), `null` (`Some(None)`) and a value (`Some(Some(_))`).
struct Post {
    agent: String,
    session_id: String,
    repository: Option<Option<String>>,
    branch: Option<Option<String>>,
    user: Option<Option<String>>,
}

/// `POST /v1/agent-context`. Answers `204` whether or not the context was written.
pub(crate) async fn post_agent_context(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Json(body): error::Json<AgentContextBody>,
) -> Result<StatusCode, ApiError> {
    refuse_over_cap(&state, &access).await?;
    let post = validate(body)?;
    let project = access.project.id;
    let written = store(&state, project, &post).await?;
    tracing::info!(
        target: "ingest",
        project_id = %project,
        agent = %post.agent,
        written,
        "agent context"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Checks every field and collects the errors. The messages never contain a submitted value.
fn validate(body: AgentContextBody) -> Result<Post, ApiError> {
    let mut errors = Vec::new();
    if !AGENTS.contains(&body.agent.as_str()) {
        errors.push(field_error(
            "agent",
            "must be claude-code, codex, gemini-cli or copilot",
        ));
    }
    if !is_bounded(&body.session_id) {
        errors.push(field_error("session_id", "must be 1 to 256 characters"));
    }
    let branch = body
        .branch
        .map(|value| value.map(|text| text.trim().to_owned()));
    let user = body
        .user
        .map(|value| value.map(|text| text.trim().to_lowercase()));
    for (field, value) in [("branch", &branch), ("user", &user)] {
        if matches!(value, Some(Some(text)) if !is_bounded(text)) {
            errors.push(field_error(field, "must be 1 to 256 characters"));
        }
    }
    if !errors.is_empty() {
        return Err(ApiError::BadRequest {
            detail: Some("invalid request".to_owned()),
            errors,
        });
    }
    // An address that does not canonicalize is stored as `null`, which clears the repository.
    let repository = body
        .repository
        .map(|value| value.and_then(|raw| canonical_repository(&raw)));
    Ok(Post {
        agent: body.agent,
        session_id: body.session_id,
        repository,
        branch,
        user,
    })
}

/// Whether `text` is 1 to 256 characters long.
fn is_bounded(text: &str) -> bool {
    (1..=MAX_TEXT).contains(&text.chars().count())
}

fn field_error(field: &str, message: &str) -> FieldError {
    FieldError {
        field: field.to_owned(),
        message: message.to_owned(),
        position: None,
    }
}

/// Merges the post into the stored context, then writes `meta.db` and the map, in that order.
/// Returns whether they were written. An unchanged context written less than an hour ago is
/// not written again (D-CA-9); neither is one whose project is gone.
async fn store(state: &AppState, project: ProjectId, post: &Post) -> Result<bool, ApiError> {
    let contexts = state.ingest.contexts();
    let _writes = contexts.writes().lock().await;
    let existing = contexts.project(project).and_then(|map| {
        let guard = map.read().unwrap_or_else(PoisonError::into_inner);
        guard.get(&post.agent, &post.session_id).cloned()
    });
    let is_new = existing.is_none();
    let old = existing.unwrap_or_default();
    let now = state.clock.now_ns();
    let merged = AgentContext {
        repository: post
            .repository
            .clone()
            .unwrap_or_else(|| old.repository.clone()),
        branch: post.branch.clone().unwrap_or_else(|| old.branch.clone()),
        user: post.user.clone().unwrap_or_else(|| old.user.clone()),
        updated_at: now,
    };
    let unchanged = !is_new
        && merged.repository == old.repository
        && merged.branch == old.branch
        && merged.user == old.user;
    if unchanged && now.saturating_sub(old.updated_at) < REWRITE_AFTER_NS {
        return Ok(false);
    }

    let row = AgentContextRow {
        project_id: project,
        agent: post.agent.clone(),
        session_id: post.session_id.clone(),
        repository: merged.repository.clone(),
        branch: merged.branch.clone(),
        user_id: merged.user.clone(),
        updated_at: now,
    };
    let (evicted, written) = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let evicted = if is_new {
                    delete_oldest(conn, project, MAX_PER_PROJECT).await?
                } else {
                    None
                };
                let written = upsert_context(conn, &row).await?;
                Ok((evicted, written))
            })
        })
        .await?;
    if !written {
        return Ok(false);
    }
    let map = contexts.project_or_insert(project);
    {
        let mut guard = map.write().unwrap_or_else(PoisonError::into_inner);
        if let Some((agent, session_id)) = &evicted {
            guard.remove(agent, session_id);
        }
        guard.insert(&post.agent, &post.session_id, merged);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::sync::PoisonError;
    use std::time::Duration;

    use axum::http::{Method, StatusCode};
    use cetra_core::ids::ProjectId;
    use cetra_otel::AgentContext;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::*;
    use crate::meta::agent_contexts::load_contexts;
    use crate::test_support::{TestApp, TestResponse, assert_logs_clean, set_limits};

    const SESSION: &str = "s1";

    async fn post_context(app: &TestApp, key: &str, body: &Value) -> TestResponse {
        app.request(Method::POST, "/v1/agent-context")
            .header("authorization", &format!("Bearer {key}"))
            .json(body)
            .send()
            .await
    }

    /// The `codex` session `session` as the project's map holds it.
    fn stored(app: &TestApp, project: ProjectId, session: &str) -> Option<AgentContext> {
        let map = app.state.ingest.contexts().project(project)?;
        let guard = map.read().unwrap_or_else(PoisonError::into_inner);
        guard.get("codex", session).cloned()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stores_a_context_and_answers_204() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let response = post_context(
            &app,
            &seeded.ingest_key,
            &json!({
                "agent": "codex",
                "session_id": SESSION,
                "repository": "https://github.com/Acme/Widgets.git",
                "branch": "feature/x",
                "user": "Alice"
            }),
        )
        .await;
        assert_eq!(response.status, StatusCode::NO_CONTENT);

        let context = stored(&app, seeded.project.id, SESSION).expect("the context is in memory");
        assert_eq!(
            context.repository.as_deref(),
            Some("github.com/acme/widgets")
        );
        assert_eq!(context.branch.as_deref(), Some("feature/x"));
        assert_eq!(context.user.as_deref(), Some("alice"));

        let rows = load_contexts(app.state.meta.reader()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].repository.as_deref(),
            Some("github.com/acme/widgets")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn absent_keeps_and_null_clears() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let key = seeded.ingest_key.clone();
        let first = post_context(
            &app,
            &key,
            &json!({
                "agent": "codex",
                "session_id": SESSION,
                "repository": "https://github.com/acme/widgets",
                "branch": "main"
            }),
        )
        .await;
        assert_eq!(first.status, StatusCode::NO_CONTENT);
        let second = post_context(
            &app,
            &key,
            &json!({"agent": "codex", "session_id": SESSION, "branch": null}),
        )
        .await;
        assert_eq!(second.status, StatusCode::NO_CONTENT);

        let context = stored(&app, seeded.project.id, SESSION).expect("the context is in memory");
        assert_eq!(
            context.repository.as_deref(),
            Some("github.com/acme/widgets")
        );
        assert_eq!(context.branch, None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unchanged_post_within_an_hour_writes_nothing() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let key = seeded.ingest_key.clone();
        let body = json!({"agent": "codex", "session_id": SESSION, "branch": "main"});
        assert_eq!(
            post_context(&app, &key, &body).await.status,
            StatusCode::NO_CONTENT
        );
        let first = stored(&app, seeded.project.id, SESSION)
            .expect("stored")
            .updated_at;

        app.state.clock.advance(Duration::from_secs(60));
        assert_eq!(
            post_context(&app, &key, &body).await.status,
            StatusCode::NO_CONTENT
        );
        let again = stored(&app, seeded.project.id, SESSION)
            .expect("stored")
            .updated_at;
        assert_eq!(again, first);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unchanged_post_after_an_hour_rewrites() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let key = seeded.ingest_key.clone();
        let body = json!({"agent": "codex", "session_id": SESSION, "branch": "main"});
        post_context(&app, &key, &body).await;
        let first = stored(&app, seeded.project.id, SESSION)
            .expect("stored")
            .updated_at;

        app.state.clock.advance(Duration::from_secs(3_601));
        assert_eq!(
            post_context(&app, &key, &body).await.status,
            StatusCode::NO_CONTENT
        );
        let again = stored(&app, seeded.project.id, SESSION)
            .expect("stored")
            .updated_at;
        assert!(again > first, "{again} is not after {first}");
        assert_eq!(again, app.state.clock.now_ns());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn posts_for_a_dropped_project_leave_no_map() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("DELETE FROM projects WHERE id = ?")
                        .bind(project.to_string())
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();

        let response = post_context(
            &app,
            &seeded.ingest_key,
            &json!({"agent": "codex", "session_id": SESSION, "branch": "main"}),
        )
        .await;
        assert_eq!(response.status, StatusCode::NO_CONTENT);
        assert!(app.state.ingest.contexts().project(project).is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bad_request_body_leaks_nothing() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let key = seeded.ingest_key.clone();
        let response = assert_logs_clean(
            &["dev@example.com", "feature/secret", key.as_str()],
            post_context(
                &app,
                &key,
                &json!({
                    "agent": "dev@example.com",
                    "session_id": SESSION,
                    "branch": "feature/secret"
                }),
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        let text = String::from_utf8_lossy(&response.body).into_owned();
        assert!(!text.contains("dev@example.com"), "{text}");
        assert!(!text.contains("feature/secret"), "{text}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_unknown_agent_bad_session_and_unknown_fields() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let key = seeded.ingest_key.clone();
        let cases = [
            (
                json!({"agent": "cursor", "session_id": SESSION}),
                Some("agent"),
            ),
            (
                json!({"agent": "codex", "session_id": ""}),
                Some("session_id"),
            ),
            (
                json!({"agent": "codex", "session_id": "x".repeat(257)}),
                Some("session_id"),
            ),
            (
                json!({"agent": "codex", "session_id": SESSION, "branch": "   "}),
                Some("branch"),
            ),
            (
                json!({"agent": "codex", "session_id": SESSION, "user": "a".repeat(257)}),
                Some("user"),
            ),
            (
                json!({"agent": "codex", "session_id": SESSION, "foo": 1}),
                None,
            ),
            (json!({"session_id": SESSION}), None),
        ];
        for (body, field) in cases {
            let response = post_context(&app, &key, &body).await;
            assert_eq!(response.status, StatusCode::BAD_REQUEST, "{body}");
            if let Some(field) = field {
                let fields: Vec<String> = response
                    .problem()
                    .errors
                    .into_iter()
                    .map(|error| error.field)
                    .collect();
                assert!(
                    fields.iter().any(|name| name == field),
                    "{body}: {fields:?}"
                );
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn needs_the_ingest_scope() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let body = json!({"agent": "codex", "session_id": SESSION});
        let anonymous = app
            .request(Method::POST, "/v1/agent-context")
            .json(&body)
            .send()
            .await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
        for key in [&seeded.read_key, &seeded.public_key] {
            let response = post_context(&app, key, &body).await;
            assert_eq!(response.status, StatusCode::FORBIDDEN);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refused_over_the_units_cap() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2025-12-01T00:00:00Z",
            &json!({"units_cap": 10}),
        );
        app.state.usage.add(
            seeded.org_id,
            seeded.project.id,
            app.state.clock.now_ns(),
            10,
            0,
            0,
        );
        let response = post_context(
            &app,
            &seeded.ingest_key,
            &json!({"agent": "codex", "session_id": SESSION}),
        )
        .await;
        assert_eq!(response.status, StatusCode::PAYMENT_REQUIRED);
        assert!(stored(&app, seeded.project.id, SESSION).is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn body_over_4_kib_is_413() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let response = app
            .request(Method::POST, "/v1/agent-context")
            .header("authorization", &format!("Bearer {}", seeded.ingest_key))
            .header("content-type", "application/json")
            .body(vec![b' '; 4_097])
            .send()
            .await;
        assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn served_on_the_otlp_port() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let request = axum::http::Request::post("/v1/agent-context")
            .header("authorization", format!("Bearer {}", seeded.ingest_key))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({"agent": "codex", "session_id": SESSION}).to_string(),
            ))
            .expect("the request builds");
        let response = crate::http::otlp_router(app.state.clone())
            .oneshot(request)
            .await
            .expect("the router never fails");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(stored(&app, seeded.project.id, SESSION).is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn logs_hold_no_values() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let key = seeded.ingest_key.clone();
        let response = assert_logs_clean(
            &["dev@example.com", "feature/secret", key.as_str()],
            post_context(
                &app,
                &key,
                &json!({
                    "agent": "codex",
                    "session_id": SESSION,
                    "user": "dev@example.com",
                    "branch": "feature/secret"
                }),
            ),
        )
        .await;
        assert_eq!(response.status, StatusCode::NO_CONTENT);
    }
}
