//! Eval job routes and manual runs (spec 09 § Evaluation & alerts, spec 06 § Online rules,
//! D-I-22). Project members and above start a manual run; every project reader lists and reads
//! jobs.

use std::collections::BTreeSet;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use cetra_core::ids::{SpanId, TraceId};
use cetra_eval::VariableMapping;
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use super::alert_rules::{decode_cursor, encode_cursor};
use crate::analytics::actor::Actor;
use crate::analytics::catalog::Event;
use crate::analytics::record;
use crate::api::common::{FieldError, Page};
use crate::api::connections::TestErrorClass;
use crate::api::eval_jobs::{
    EvalJob, EvalJobKind, EvalJobListParams, EvalJobStatus, RunEvaluatorRequest,
};
use crate::auth::principal::{self, Principal};
use crate::auth::roles::ProjectPerm;
use crate::eval::rule_check::{Subject, field_error, unmapped_variables};
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::http::restore::refuse_outbound;
use crate::meta::MetaError;
use crate::meta::eval_jobs::{self, JobFilter, JobRow, NewJob};
use crate::meta::evaluators;

/// Eval job routes. Reads need project view; the manual run needs
/// [`ProjectPerm::ManageEvaluation`] (D-I-24). Not session-only: API keys are refused by the
/// handler, which takes the session user.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/eval-jobs",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_eval_jobs),
        },
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/eval-jobs/{id}",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(get_eval_job),
        },
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/v1/projects/{project}/evaluators/{id}/run",
            access: Access::Project {
                perm: ProjectPerm::ManageEvaluation,
                session_only: false,
            },
            documented: true,
            handler: post(run_evaluator),
        },
    ]
}

#[derive(Debug, Deserialize)]
struct IdPath {
    id: Uuid,
}

/// Jobs of the project, newest first.
async fn list_eval_jobs(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Query(params): error::Query<EvalJobListParams>,
) -> Result<error::Json<Page<EvalJob>>, ApiError> {
    let before = params.cursor.as_deref().map(decode_cursor).transpose()?;
    let limit = usize::try_from(params.limit.get()).unwrap_or(usize::MAX);
    let fetch = i64::from(params.limit.get()) + 1;
    let filter = JobFilter {
        rule_id: params.rule_id,
        evaluator_id: params.evaluator_id,
        status: params.status.map(EvalJobStatus::as_str),
        kind: params.kind.map(EvalJobKind::as_str),
    };
    let mut rows = eval_jobs::list_jobs(
        state.meta.reader(),
        access.project.id,
        &filter,
        before,
        fetch,
    )
    .await?;
    let more = rows.len() > limit;
    rows.truncate(limit);
    let next_cursor = match (more, rows.last()) {
        (true, Some(last)) => Some(encode_cursor(last.created_at, last.id)?),
        _ => None,
    };
    let data = rows.iter().map(job_json).collect::<Result<Vec<_>, _>>()?;
    Ok(error::Json(Page { data, next_cursor }))
}

async fn get_eval_job(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
) -> Result<error::Json<EvalJob>, ApiError> {
    let row = eval_jobs::get_job(state.meta.reader(), access.project.id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(error::Json(job_json(&row)?))
}

/// Queues a manual run of the evaluator on one trace or span, at the highest priority (D-I-22).
/// It shares the test runs' limit of 10 a minute per user.
async fn run_evaluator(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
    error::Json(body): error::Json<RunEvaluatorRequest>,
) -> Result<(StatusCode, error::Json<EvalJob>), ApiError> {
    refuse_outbound(&state, "evaluator run")?;
    let user = principal::session_user(&principal)?;
    let project = access.project.id;
    let now = state.clock.now_ns();
    state
        .limits
        .connection_tests
        .check(&user.id, now)
        .map_err(|retry| ApiError::RateLimited {
            retry_after_s: retry.0.as_secs().max(1),
        })?;
    let row = evaluators::get_evaluator(state.meta.reader(), project, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let subject = Subject::from_row(&row).map_err(internal)?;
    let checked = check_run(&body, &subject)?;
    // D-M-6: a deleted trace is not found. The trace is not loaded here; the executor's check A
    // covers its end users.
    if state
        .deletions
        .guard
        .blocking(project, checked.trace_id, &[], state.clock.now_ns())
        .is_some()
    {
        return Err(ApiError::NotFound);
    }
    let job_id = Uuid::now_v7();
    let job = NewJob {
        id: job_id,
        project_id: project,
        kind: eval_jobs::KIND_MANUAL,
        rule_id: None,
        evaluator_id: id,
        variable_mapping: Some(checked.mapping_json),
        trace_id: checked.trace_id,
        span_id: checked.span_id,
        trace_start_ns: body.trace_start.ns(),
    };
    state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                eval_jobs::insert_job(conn, job, now)
                    .await
                    .map(|_inserted| ())
            })
        })
        .await?;
    // The id is new, so the insert cannot conflict: the row is there to read back.
    let queued = eval_jobs::get_job(state.meta.reader(), project, job_id)
        .await?
        .ok_or_else(|| {
            internal(MetaError::Corrupt {
                table: "eval_jobs",
                column: "id",
            })
        })?;
    state.eval.wake();
    tracing::info!(
        target: "audit",
        job_id = %job_id,
        evaluator_id = %id,
        project_id = %project,
        user_id = %user.id,
        "evaluator run queued"
    );
    record(
        &state,
        &Event::EvaluatorRun,
        &Actor::project(&principal, &access),
    );
    Ok((StatusCode::ACCEPTED, error::Json(job_json(&queued)?)))
}

/// A run body that passed every check of § Eval job contract.
struct Checked {
    trace_id: TraceId,
    span_id: Option<SpanId>,
    /// The mapping as stored in the job: a JSON object of expressions.
    mapping_json: String,
}

/// The body checks of a manual run (§ Eval job contract). Every field error is collected.
fn check_run(body: &RunEvaluatorRequest, subject: &Subject) -> Result<Checked, ApiError> {
    let mut errors = Vec::new();
    let trace = match body.trace_id.parse::<TraceId>() {
        Ok(id) => Some(id),
        Err(_) => {
            errors.push(field_error("trace_id", "trace_id must be 32 hex digits"));
            None
        }
    };
    let span = match body.span_id.as_deref() {
        None => None,
        Some(text) => match text.parse::<SpanId>() {
            Ok(id) => Some(id),
            Err(_) => {
                errors.push(field_error("span_id", "span_id must be 16 hex digits"));
                None
            }
        },
    };
    let mapping = &body.variable_mapping;
    let mut mapping_ok = true;
    if !(1..=32).contains(&mapping.len()) {
        errors.push(field_error("variable_mapping", "map 1 to 32 variables"));
        mapping_ok = false;
    }
    for (name, expression) in mapping {
        if expression.len() > 1024 {
            errors.push(field_error(
                &format!("variable_mapping.{name}"),
                "expression must be at most 1024 bytes",
            ));
            mapping_ok = false;
        }
    }
    let mut parsed: Option<VariableMapping> = None;
    if mapping_ok {
        let value = Value::Object(
            mapping
                .iter()
                .map(|(name, expression)| (name.clone(), Value::String(expression.clone())))
                .collect(),
        );
        match VariableMapping::from_json(&value) {
            Ok(mapping) => {
                if mapping.needs_item() {
                    errors.push(field_error(
                        "variable_mapping",
                        "expected and item.* are for experiments only",
                    ));
                }
                if mapping.needs_target_span() && body.span_id.is_none() {
                    errors.push(field_error("variable_mapping", "span.* needs span_id"));
                }
                parsed = Some(mapping);
            }
            Err(error) => errors.push(field_error("variable_mapping", &error.to_string())),
        }
    }
    if let Subject::Judge(judge) = subject {
        let keys: BTreeSet<String> = mapping.keys().cloned().collect();
        errors.extend(unmapped_variables(judge, &keys));
    }
    let (Some(trace_id), Some(_), true) = (trace, parsed, errors.is_empty()) else {
        return Err(bad_request(errors));
    };
    Ok(Checked {
        trace_id,
        span_id: span,
        mapping_json: serde_json::to_string(mapping).map_err(internal)?,
    })
}

/// The API view of a stored job.
fn job_json(row: &JobRow) -> Result<EvalJob, ApiError> {
    let kind = EvalJobKind::from_name(&row.kind).ok_or_else(|| corrupt("kind"))?;
    let status = EvalJobStatus::from_name(&row.status).ok_or_else(|| corrupt("status"))?;
    let error_class = row.error_class.as_deref().map(error_class_of).transpose()?;
    Ok(EvalJob {
        id: row.id,
        kind,
        status,
        rule_id: row.rule_id,
        evaluator_id: row.evaluator_id,
        trace_id: row.trace_id,
        span_id: row.span_id,
        attempts: row.attempts,
        error_class,
        error: row.error.clone(),
        judge_trace_id: row.judge_trace_id,
        score_id: row.score_id,
        input_tokens: row.input_tokens,
        output_tokens: row.output_tokens,
        cost_usd: row.cost_usd,
        latency_ms: row.latency_ms,
        not_before: row.not_before,
        created_at: row.created_at,
        finished_at: row.finished_at,
    })
}

/// The stored `error_class`. `pause` is what a failed run of a paused connection records.
fn error_class_of(text: &str) -> Result<TestErrorClass, ApiError> {
    match text {
        "retryable" => Ok(TestErrorClass::Retryable),
        "pause" => Ok(TestErrorClass::Pause),
        "permanent" => Ok(TestErrorClass::Permanent),
        _ => Err(corrupt("error_class")),
    }
}

fn bad_request(errors: Vec<FieldError>) -> ApiError {
    ApiError::BadRequest {
        detail: None,
        errors,
    }
}

fn corrupt(column: &'static str) -> ApiError {
    ApiError::Internal(Box::new(MetaError::Corrupt {
        table: "eval_jobs",
        column,
    }))
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> ApiError {
    ApiError::Internal(Box::new(error))
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::time::{Duration, Instant};

    use axum::http::{Method, header};
    use cetra_core::enums::PayloadFormat;
    use cetra_core::ids::{SpanId, TraceId};
    use cetra_core::span::Span;
    use serde_json::json;

    use super::*;
    use crate::auth::roles::OrgRole;
    use crate::eval::EvalTiming;
    use crate::meta::connections::{ConnectionFields, insert_connection};
    use crate::meta::online_rules::{NewRule, RuleFields, insert_rule};
    use crate::test_support::mock_llm::{MockLlm, completion};
    use crate::test_support::{Seeded, TestApp, TestResponse, dataset_span};

    /// The fixture judge's connection (`fixtures/api/README.md`).
    const FIXTURE_CONNECTION: &str = "01900000-0000-7000-8000-000000000400";
    const TRACE: &str = "a1000000000000000000000000000001";
    const JUDGE_ANSWER: &str = r#"{"explanation": "Relevant.", "value": 0.9}"#;
    const QUESTION: &str = "What is the refund window?";

    fn real_now() -> i64 {
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos(),
        )
        .expect("fits i64")
    }

    fn text(response: &TestResponse) -> String {
        String::from_utf8_lossy(&response.body).into_owned()
    }

    /// The `(field, message)` pairs of a 400 answer.
    fn field_errors(response: &TestResponse) -> Vec<(String, String)> {
        assert_eq!(response.status, 400, "{}", text(response));
        let body: Value = response.json();
        body["errors"]
            .as_array()
            .expect("errors")
            .iter()
            .map(|error| {
                (
                    error["field"].as_str().expect("field").to_owned(),
                    error["message"].as_str().expect("message").to_owned(),
                )
            })
            .collect()
    }

    /// The fixture run body (`fixtures/api/requests/evaluator-run.json`).
    fn run_body() -> Value {
        serde_json::from_str(include_str!(
            "../../../../fixtures/api/requests/evaluator-run.json"
        ))
        .expect("fixture")
    }

    /// The fixture judge create body (`fixtures/api/requests/evaluator-create.json`).
    fn fixture_judge_body() -> Value {
        serde_json::from_str(include_str!(
            "../../../../fixtures/api/requests/evaluator-create.json"
        ))
        .expect("fixture")
    }

    /// Inserts the fixture connection in `org`, as M1's route would.
    async fn add_fixture_connection(app: &TestApp, seeded: &Seeded) {
        let id = Uuid::from_str(FIXTURE_CONNECTION).expect("uuid");
        let org = seeded.org_id;
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_connection(
                        conn,
                        id,
                        org,
                        ConnectionFields {
                            name: format!("Judge {id}"),
                            adapter: "openai".to_owned(),
                            base_url: None,
                            secret_enc: vec![0x01],
                            api_key_hint: "0123".to_owned(),
                            extra_headers_enc: None,
                            extra_header_names: Vec::new(),
                            default_model: None,
                            structured_output: "native".to_owned(),
                            max_concurrency: 4,
                            requests_per_minute: 300,
                        },
                        None,
                        now,
                    )
                    .await
                })
            })
            .await
            .expect("connection");
    }

    async fn test_app() -> (TestApp, Seeded) {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        add_fixture_connection(&app, &seeded).await;
        (app, seeded)
    }

    /// Creates a judge from `body` and returns its id.
    async fn create_judge(app: &TestApp, seeded: &Seeded, body: Value) -> Uuid {
        let response = app
            .request(
                Method::POST,
                &format!("/api/v1/projects/{}/evaluators", seeded.project.id),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&body)
            .send()
            .await;
        assert_eq!(response.status, 201, "{}", text(&response));
        let value: Value = response.json();
        Uuid::parse_str(value["id"].as_str().expect("id")).expect("uuid")
    }

    async fn run(app: &TestApp, seeded: &Seeded, evaluator: Uuid, body: &Value) -> TestResponse {
        app.request(
            Method::POST,
            &format!(
                "/api/v1/projects/{}/evaluators/{evaluator}/run",
                seeded.project.id
            ),
        )
        .header(header::COOKIE.as_str(), &seeded.cookie)
        .json(body)
        .send()
        .await
    }

    async fn get_job(app: &TestApp, seeded: &Seeded, id: &str) -> TestResponse {
        app.request(
            Method::GET,
            &format!("/api/v1/projects/{}/eval-jobs/{id}", seeded.project.id),
        )
        .header(header::COOKIE.as_str(), &seeded.cookie)
        .send()
        .await
    }

    async fn list(app: &TestApp, seeded: &Seeded, query: &str) -> TestResponse {
        app.request(
            Method::GET,
            &format!("/api/v1/projects/{}/eval-jobs{query}", seeded.project.id),
        )
        .header(header::COOKIE.as_str(), &seeded.cookie)
        .send()
        .await
    }

    async fn job_count(app: &TestApp) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM eval_jobs")
            .fetch_one(app.state.meta.reader())
            .await
            .expect("count")
    }

    #[tokio::test]
    async fn a_manual_run_on_a_deleted_trace_is_404() {
        let (app, seeded) = test_app().await;
        let evaluator = create_judge(&app, &seeded, fixture_judge_body()).await;
        // D-M-6: the guard holds the trace from a deletion made just now.
        app.state.deletions.guard.add_traces(
            seeded.project.id,
            &[TRACE.parse::<TraceId>().expect("trace id")],
            Uuid::now_v7(),
            app.state.clock.now_ns(),
        );
        let before = job_count(&app).await;
        let response = run(&app, &seeded, evaluator, &run_body()).await;
        assert_eq!(response.status, 404, "{}", text(&response));
        assert_eq!(job_count(&app).await, before, "no job is queued");
    }

    #[tokio::test]
    async fn run_queues_a_manual_job() {
        let (app, seeded) = test_app().await;
        let evaluator = create_judge(&app, &seeded, fixture_judge_body()).await;
        let response = run(&app, &seeded, evaluator, &run_body()).await;
        assert_eq!(response.status, 202, "{}", text(&response));
        let job: Value = response.json();
        assert_eq!(job["kind"], "manual");
        assert_eq!(job["status"], "queued");
        assert_eq!(job["attempts"], 0);
        assert_eq!(job["trace_id"], TRACE);
        assert!(job.get("rule_id").is_none(), "{job}");

        let id = Uuid::parse_str(job["id"].as_str().expect("id")).expect("uuid");
        let (priority, dedup): (i64, String) =
            sqlx::query_as("SELECT priority, dedup_key FROM eval_jobs WHERE id = ?")
                .bind(id.to_string())
                .fetch_one(app.state.meta.reader())
                .await
                .expect("row");
        assert_eq!(priority, 0);
        assert_eq!(dedup, format!("manual:{id}"));
        assert!(
            app.state.analytics.counters.get("evaluator.run") >= 1,
            "evaluator.run not counted"
        );
    }

    #[tokio::test]
    async fn a_span_run_keeps_the_span() {
        let (app, seeded) = test_app().await;
        let evaluator = create_judge(&app, &seeded, fixture_judge_body()).await;
        let mut body = run_body();
        body["span_id"] = json!("0000000000000001");
        body["variable_mapping"] = json!({"input": "span.input", "output": "span.output"});
        let response = run(&app, &seeded, evaluator, &body).await;
        assert_eq!(response.status, 202, "{}", text(&response));
        let job: Value = response.json();
        assert_eq!(job["span_id"], "0000000000000001");
    }

    #[tokio::test]
    async fn run_validates_its_body() {
        let (app, seeded) = test_app().await;
        let evaluator = create_judge(&app, &seeded, fixture_judge_body()).await;
        let long = "x".repeat(1025);
        let cases: Vec<(&str, Value, (&str, &str))> = vec![
            (
                "trace id",
                json!({"trace_id": "xyz"}),
                ("trace_id", "trace_id must be 32 hex digits"),
            ),
            (
                "zero trace id",
                json!({"trace_id": "0".repeat(32)}),
                ("trace_id", "trace_id must be 32 hex digits"),
            ),
            (
                "span id",
                json!({"span_id": "short"}),
                ("span_id", "span_id must be 16 hex digits"),
            ),
            (
                "no variables",
                json!({"variable_mapping": {}}),
                ("variable_mapping", "map 1 to 32 variables"),
            ),
            (
                "long expression",
                json!({"variable_mapping": {"input": long, "output": "trace.output"}}),
                (
                    "variable_mapping.input",
                    "expression must be at most 1024 bytes",
                ),
            ),
            (
                "experiment expression",
                json!({"variable_mapping": {"input": "item.input", "output": "trace.output"}}),
                (
                    "variable_mapping",
                    "expected and item.* are for experiments only",
                ),
            ),
            (
                "span expression without a span",
                json!({"variable_mapping": {"input": "span.input", "output": "trace.output"}}),
                ("variable_mapping", "span.* needs span_id"),
            ),
            (
                "unmapped prompt variable",
                json!({"variable_mapping": {"output": "trace.output"}}),
                (
                    "variable_mapping.input",
                    "variable input is used by the prompt but not mapped",
                ),
            ),
        ];
        for (case, change, (field, message)) in cases {
            let mut body = run_body();
            for (key, value) in change.as_object().expect("object") {
                body[key] = value.clone();
            }
            let response = run(&app, &seeded, evaluator, &body).await;
            let errors = field_errors(&response);
            assert!(
                errors.contains(&(field.to_owned(), message.to_owned())),
                "{case}: {errors:?}"
            );
        }
        assert_eq!(job_count(&app).await, 0, "no case queued a job");
    }

    #[tokio::test]
    async fn a_long_expression_does_not_hide_the_unmapped_variable() {
        let (app, seeded) = test_app().await;
        let evaluator = create_judge(&app, &seeded, fixture_judge_body()).await;
        let mut body = run_body();
        body["variable_mapping"] = json!({"input": "x".repeat(1025)});
        let errors = field_errors(&run(&app, &seeded, evaluator, &body).await);
        assert!(
            errors.contains(&(
                "variable_mapping.input".to_owned(),
                "expression must be at most 1024 bytes".to_owned()
            )),
            "{errors:?}"
        );
        assert!(
            errors.contains(&(
                "variable_mapping.output".to_owned(),
                "variable output is used by the prompt but not mapped".to_owned()
            )),
            "{errors:?}"
        );
        assert_eq!(job_count(&app).await, 0);
    }

    #[tokio::test]
    async fn run_with_an_unmapped_prompt_variable_is_400() {
        let (app, seeded) = test_app().await;
        let evaluator = create_judge(&app, &seeded, fixture_judge_body()).await;
        let mut body = run_body();
        body["variable_mapping"] = json!({"output": "trace.output"});
        let response = run(&app, &seeded, evaluator, &body).await;
        assert_eq!(
            field_errors(&response),
            vec![(
                "variable_mapping.input".to_owned(),
                "variable input is used by the prompt but not mapped".to_owned()
            )]
        );
        assert_eq!(job_count(&app).await, 0);
    }

    #[tokio::test]
    async fn run_of_an_unknown_evaluator_is_404() {
        let (app, seeded) = test_app().await;
        let response = run(&app, &seeded, Uuid::now_v7(), &run_body()).await;
        assert_eq!(response.status, 404, "{}", text(&response));
    }

    #[tokio::test]
    async fn run_is_rate_limited_with_tests() {
        let (app, seeded) = test_app().await;
        let evaluator = create_judge(&app, &seeded, fixture_judge_body()).await;
        let test_body = json!({
            "evaluator": {
                "name": "Same",
                "kind": "check",
                "config": {"check": "exact_match"},
                "output_score": {"name": "same", "data_type": "boolean"}
            },
            "variables": {"output": "a", "expected": "a"}
        });
        for _ in 0..10 {
            let response = app
                .request(
                    Method::POST,
                    &format!("/api/v1/projects/{}/evaluators/test", seeded.project.id),
                )
                .header(header::COOKIE.as_str(), &seeded.cookie)
                .json(&test_body)
                .send()
                .await;
            assert_eq!(response.status, 200, "{}", text(&response));
        }
        let limited = run(&app, &seeded, evaluator, &run_body()).await;
        assert_eq!(limited.status, 429, "{}", text(&limited));
    }

    #[tokio::test]
    async fn viewer_and_api_key_cannot_run() {
        let (app, seeded) = test_app().await;
        let evaluator = create_judge(&app, &seeded, fixture_judge_body()).await;
        let path = format!(
            "/api/v1/projects/{}/evaluators/{evaluator}/run",
            seeded.project.id
        );
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let refused = app
            .request(Method::POST, &path)
            .header(header::COOKIE.as_str(), &viewer.cookie)
            .json(&run_body())
            .send()
            .await;
        assert_eq!(refused.status, 403, "{}", text(&refused));

        let key = app
            .request(Method::POST, &path)
            .header(
                header::AUTHORIZATION.as_str(),
                &format!("Bearer {}", seeded.read_key),
            )
            .json(&run_body())
            .send()
            .await;
        assert_eq!(key.status, 403, "{}", text(&key));
        assert_eq!(job_count(&app).await, 0);
    }

    /// A trace rule on `evaluator`, with no budget.
    async fn rule(app: &TestApp, seeded: &Seeded, evaluator: Uuid) -> Uuid {
        let id = Uuid::now_v7();
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let fields = RuleFields {
            name: format!("rule-{id}"),
            filter: None,
            variable_mapping: r#"{"input": "trace.input", "output": "trace.output"}"#.to_owned(),
            sample_rate: 1.0,
            budget_usd: None,
            budget_period: None,
            enabled: true,
        };
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    insert_rule(
                        conn,
                        NewRule {
                            id,
                            project_id: project,
                            evaluator_id: evaluator,
                            target: "trace".to_owned(),
                            created_by: None,
                        },
                        fields,
                        now,
                    )
                    .await
                    .map(|_| ())
                })
            })
            .await
            .expect("rule");
        id
    }

    /// Queues a rule job of `rule` on `trace`, created at `created` (nanoseconds).
    async fn rule_job(
        app: &TestApp,
        seeded: &Seeded,
        rule: Uuid,
        evaluator: Uuid,
        trace: TraceId,
        created: i64,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let project = seeded.project.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    eval_jobs::insert_job(
                        conn,
                        NewJob {
                            id,
                            project_id: project,
                            kind: eval_jobs::KIND_RULE,
                            rule_id: Some(rule),
                            evaluator_id: evaluator,
                            variable_mapping: None,
                            trace_id: trace,
                            span_id: None,
                            trace_start_ns: created,
                        },
                        created,
                    )
                    .await
                    .map(|_| ())
                })
            })
            .await
            .expect("job");
        id
    }

    #[tokio::test]
    async fn jobs_list_filters_and_pages() {
        let (app, seeded) = test_app().await;
        let evaluator = create_judge(&app, &seeded, fixture_judge_body()).await;
        let first_rule = rule(&app, &seeded, evaluator).await;
        let second_rule = rule(&app, &seeded, evaluator).await;
        let now = app.state.clock.now_ns();
        let trace = TraceId::from_str(TRACE).expect("trace");
        let oldest = rule_job(
            &app,
            &seeded,
            first_rule,
            evaluator,
            trace,
            now - 2_000_000_000,
        )
        .await;
        let middle = rule_job(
            &app,
            &seeded,
            second_rule,
            evaluator,
            trace,
            now - 1_000_000_000,
        )
        .await;
        let manual = run(&app, &seeded, evaluator, &run_body()).await;
        assert_eq!(manual.status, 202, "{}", text(&manual));
        let manual_id = manual.json::<Value>()["id"]
            .as_str()
            .expect("id")
            .to_owned();

        let ids = |response: &TestResponse| -> Vec<String> {
            let page: Value = response.json();
            page["data"]
                .as_array()
                .expect("data")
                .iter()
                .map(|job| job["id"].as_str().expect("id").to_owned())
                .collect()
        };
        let all = list(&app, &seeded, "").await;
        assert_eq!(all.status, 200, "{}", text(&all));
        assert_eq!(
            ids(&all),
            vec![manual_id.clone(), middle.to_string(), oldest.to_string()],
            "newest first"
        );

        let by_rule = list(&app, &seeded, &format!("?rule_id={first_rule}")).await;
        assert_eq!(ids(&by_rule), vec![oldest.to_string()]);
        let by_evaluator = list(&app, &seeded, &format!("?evaluator_id={evaluator}")).await;
        assert_eq!(ids(&by_evaluator).len(), 3);
        let by_kind = list(&app, &seeded, "?kind=manual").await;
        assert_eq!(ids(&by_kind), vec![manual_id.clone()]);
        let by_status = list(&app, &seeded, "?status=done").await;
        assert!(ids(&by_status).is_empty());

        let first = list(&app, &seeded, "?limit=1").await;
        assert_eq!(ids(&first), vec![manual_id.clone()]);
        let cursor = first.json::<Value>()["next_cursor"]
            .as_str()
            .expect("a second page exists")
            .to_owned();
        let second = list(&app, &seeded, &format!("?limit=1&cursor={cursor}")).await;
        assert_eq!(ids(&second), vec![middle.to_string()]);
        let cursor = second.json::<Value>()["next_cursor"]
            .as_str()
            .expect("a third page exists")
            .to_owned();
        let third = list(&app, &seeded, &format!("?limit=1&cursor={cursor}")).await;
        assert_eq!(ids(&third), vec![oldest.to_string()]);
        assert!(third.json::<Value>()["next_cursor"].is_null());

        let garbage = list(&app, &seeded, "?cursor=garbage").await;
        assert_eq!(
            field_errors(&garbage),
            vec![("cursor".to_owned(), "invalid cursor".to_owned())]
        );
    }

    #[tokio::test]
    async fn another_projects_job_is_not_listed_or_read() {
        let (app, seeded) = test_app().await;
        let evaluator = create_judge(&app, &seeded, fixture_judge_body()).await;
        let queued = run(&app, &seeded, evaluator, &run_body()).await;
        let id = queued.json::<Value>()["id"]
            .as_str()
            .expect("id")
            .to_owned();
        let (other, _) = app.add_project(&seeded, "beta", None).await;
        let other_seeded = Seeded {
            user_id: seeded.user_id,
            org_id: seeded.org_id,
            project: other,
            cookie: seeded.cookie.clone(),
            ingest_key: seeded.ingest_key.clone(),
            read_key: seeded.read_key.clone(),
            public_key: seeded.public_key.clone(),
        };
        let listed = list(&app, &other_seeded, "").await;
        assert!(
            listed.json::<Value>()["data"]
                .as_array()
                .expect("data")
                .is_empty()
        );
        let read = get_job(&app, &other_seeded, &id).await;
        assert_eq!(read.status, 404, "{}", text(&read));
        let missing = get_job(&app, &seeded, &Uuid::now_v7().to_string()).await;
        assert_eq!(missing.status, 404, "{}", text(&missing));
    }

    #[tokio::test]
    async fn a_manual_run_completes_through_the_loops() {
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
        })
        .await;
        app.state.clock.set(real_now());
        let seeded = app.seed().await;
        let now = app.state.clock.now_ns();
        let trace = TraceId::from_str(TRACE).expect("trace");
        let mut row: Span = dataset_span(0);
        row.trace_id = trace;
        row.span_id = SpanId::from_bytes(1_u64.to_be_bytes()).expect("span");
        row.parent_span_id = None;
        row.input = Some(QUESTION.to_owned());
        row.input_format = Some(PayloadFormat::Text);
        row.output = Some("30 days.".to_owned());
        row.output_format = Some(PayloadFormat::Text);
        row.environment = Some("production".to_owned());
        row.start_time = now - 30_000_000_000;
        row.end_time = now - 20_000_000_000;
        app.commit_spans(&seeded.project, vec![row]).await;

        let mock = MockLlm::start(vec![completion(JUDGE_ANSWER)]).await;
        let connection = app
            .request(Method::POST, "/api/v1/orgs/acme/llm-connections")
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&json!({
                "name": "Mocked",
                "adapter": "openai",
                "api_key": "sk-test-key-0123456789",
                "base_url": mock.base_url(),
                "default_model": "gpt-test",
                "max_concurrency": 4,
                "requests_per_minute": 300
            }))
            .send()
            .await;
        assert_eq!(connection.status, 201, "{}", text(&connection));
        let connection_id = connection.json::<Value>()["id"]
            .as_str()
            .expect("id")
            .to_owned();
        let mut judge = fixture_judge_body();
        judge["config"]["connection_id"] = json!(connection_id);
        judge["config"]["model"] = json!("gpt-test");
        let evaluator = create_judge(&app, &seeded, judge).await;

        // The trace start is the trace's own start, in epoch milliseconds.
        let mut body = run_body();
        body["trace_start"] = json!(((now - 30_000_000_000) / 1_000_000).to_string());
        let tasks = crate::eval::spawn(&app.state, EvalTiming::TEST);
        let queued = run(&app, &seeded, evaluator, &body).await;
        assert_eq!(queued.status, 202, "{}", text(&queued));
        let id = queued.json::<Value>()["id"]
            .as_str()
            .expect("id")
            .to_owned();

        let started = Instant::now();
        let finished = loop {
            let job: Value = get_job(&app, &seeded, &id).await.json();
            if matches!(job["status"].as_str(), Some("done" | "failed")) {
                break job;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "{job}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        assert_eq!(finished["status"], "done", "{finished}");
        assert!(finished["score_id"].is_string(), "{finished}");
        let judge_trace = finished["judge_trace_id"].as_str().expect("judge trace");

        let scores = app
            .request(
                Method::GET,
                &format!(
                    "/api/v1/projects/{}/scores?evaluator_id={evaluator}",
                    seeded.project.id
                ),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        let page: Value = scores.json();
        let rows = page["data"].as_array().expect("data");
        assert_eq!(rows.len(), 1, "{page}");
        assert!(rows[0].get("rule_id").is_none(), "{page}");

        // The judge trace was written just now, on the real clock.
        let around = app.state.clock.now_ns() / 1_000_000;
        let detail = app
            .request(
                Method::GET,
                &format!(
                    "/api/v1/projects/{}/traces/{judge_trace}?around={around}",
                    seeded.project.id
                ),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(detail.status, 200, "{}", text(&detail));
        tasks.shutdown(&app.state).await;
    }

    #[tokio::test]
    async fn run_evaluator_is_refused_in_restore_mode() {
        let app = TestApp::with_config(|config| config.restore_mode = true).await;
        let seeded = app.seed().await;
        add_fixture_connection(&app, &seeded).await;
        let evaluator = create_judge(&app, &seeded, fixture_judge_body()).await;
        let response = run(&app, &seeded, evaluator, &run_body()).await;
        crate::http::restore::tests::assert_refused(&response, "evaluator run");
    }
}
