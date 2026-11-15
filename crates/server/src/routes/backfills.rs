//! Online rule backfill routes (spec 09 § Evaluation & alerts, spec 06 § Online rules ›
//! Backfill; D-I-25, D-I-38 to D-I-42).

use std::collections::BTreeMap;
use std::time::Duration;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use llmobs_core::ids::{OrgId, ProjectId};
use llmobs_storage::RunError;
use serde::Deserialize;
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::api::backfills::{Backfill, BackfillEstimate, BackfillStatus, CreateBackfillRequest};
use crate::api::common::{FieldError, Page};
use crate::auth::principal::{self, Principal};
use crate::auth::roles::ProjectPerm;
use crate::eval::backfill::{self, BackfillRule, JudgeCost, Window};
use crate::eval::budget;
use crate::eval::rule_check::Subject;
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError};
use crate::http::restore::refuse_outbound;
use crate::meta::MetaError;
use crate::meta::backfills::{self as backfills, BackfillRow, NewBackfill};
use crate::meta::evaluators;
use crate::meta::online_rules::{self as store, RuleRow};

use super::online_rules::judge_adapter;

/// Longest a judge estimate may read (`routes::read::READ_TIMEOUT`'s value).
const ESTIMATE_TIMEOUT: Duration = Duration::from_secs(30);

/// Backfill routes. Reads need project view; starting or estimating a backfill needs
/// [`ProjectPerm::ManageEvaluation`] (D-I-24). Not session-only.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![
        RouteDef {
            method: axum::http::Method::GET,
            path: "/api/v1/projects/{project}/online-rules/{id}/backfills",
            access: Access::Project {
                perm: ProjectPerm::View,
                session_only: false,
            },
            documented: true,
            handler: get(list_backfills),
        },
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/v1/projects/{project}/online-rules/{id}/backfills/estimate",
            access: Access::Project {
                perm: ProjectPerm::ManageEvaluation,
                session_only: false,
            },
            documented: true,
            handler: post(estimate_backfill),
        },
        RouteDef {
            method: axum::http::Method::POST,
            path: "/api/v1/projects/{project}/online-rules/{id}/backfills",
            access: Access::Project {
                perm: ProjectPerm::ManageEvaluation,
                session_only: false,
            },
            documented: true,
            handler: post(create_backfill),
        },
    ]
}

#[derive(Debug, Deserialize)]
struct IdPath {
    id: Uuid,
}

/// The newest 50 backfills of a rule, newest first. Not paged (D-I-42).
async fn list_backfills(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
) -> Result<error::Json<Page<Backfill>>, ApiError> {
    let project = access.project.id;
    store::get_rule(state.meta.reader(), project, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let rows = backfills::list_backfills(state.meta.reader(), id).await?;
    let data = rows
        .iter()
        .map(backfill_json)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(error::Json(Page {
        data,
        next_cursor: None,
    }))
}

/// Counts what a backfill of the body would read and cost (D-I-38). Writes nothing.
async fn estimate_backfill(
    State(state): State<AppState>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
    error::Json(body): error::Json<CreateBackfillRequest>,
) -> Result<error::Json<BackfillEstimate>, ApiError> {
    let project = access.project.id;
    let org = access.project.org_id;
    let now = state.clock.now_ns();
    let rule = store::get_rule(state.meta.reader(), project, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let window = checked_window(&body, rule.sample_rate, now)?;
    let rule_spec = BackfillRule::new(&rule, org)?;
    let judge = judge_cost(&state, org, &rule, now).await?;
    let estimate = tokio::time::timeout(
        ESTIMATE_TIMEOUT,
        backfill::estimate(&state, &rule_spec, window, judge.as_ref()),
    )
    .await
    .map_err(|_| ApiError::from(RunError::Timeout))??;
    let cost = estimate.cost;
    tracing::debug!(
        project_id = %project,
        rule_id = %id,
        matched = estimate.matched,
        sampled = estimate.sampled,
        "backfill estimated"
    );
    Ok(error::Json(BackfillEstimate {
        matched: estimate.matched,
        capped: estimate.capped,
        sampled: estimate.sampled,
        estimated_input_tokens: cost.map(|cost| cost.input_tokens),
        estimated_output_tokens: cost.map(|cost| cost.output_tokens),
        estimated_cost_usd: cost.and_then(|cost| cost.cost_usd),
        from: window.from,
        to: window.to,
    }))
}

/// Starts a backfill: one `running` row, then the runner is woken (D-I-39).
async fn create_backfill(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Path(IdPath { id }): error::Path<IdPath>,
    error::Json(body): error::Json<CreateBackfillRequest>,
) -> Result<(StatusCode, error::Json<Backfill>), ApiError> {
    refuse_outbound(&state, "backfill")?;
    let user_id = principal::session_user(&principal)?.id;
    let project = access.project.id;
    let now = state.clock.now_ns();
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move { create_checked(conn, project, id, &body, user_id, now).await })
        })
        .await?;
    match outcome {
        Create::Missing => Err(ApiError::NotFound),
        Create::Invalid(errors) => Err(ApiError::BadRequest {
            detail: None,
            errors,
        }),
        Create::Inactive => Err(ApiError::Conflict {
            detail: "the rule must be enabled and not paused to backfill",
        }),
        Create::Created(row) => {
            state.eval.wake_backfills();
            tracing::info!(
                target: "audit",
                backfill_id = %row.id,
                rule_id = %id,
                project_id = %project,
                user_id = %user_id,
                max_targets = row.max_targets,
                "online rule backfill started"
            );
            Ok((StatusCode::ACCEPTED, error::Json(backfill_json(&row)?)))
        }
    }
}

/// What the create transaction found.
enum Create {
    Missing,
    /// The body fails the limits (D-I-42); nothing is written.
    Invalid(Vec<FieldError>),
    /// Disabled or paused: nothing is written.
    Inactive,
    Created(Box<BackfillRow>),
}

/// The create transaction. The rule must still exist, the body must pass the limits (with the
/// rule's sample rate read here, so it is the rule's rate in this transaction), and the rule must
/// be enabled and unpaused. The partial unique index refuses a second running backfill of the
/// rule, as a `Conflict`.
async fn create_checked(
    conn: &mut SqliteConnection,
    project: ProjectId,
    id: Uuid,
    body: &CreateBackfillRequest,
    user_id: Uuid,
    now: i64,
) -> Result<Create, MetaError> {
    let Some(rule) = store::get_rule_tx(conn, project, id).await? else {
        return Ok(Create::Missing);
    };
    let window = match backfill::validate(
        body.from.ns(),
        body.to.ns(),
        body.max_targets,
        body.sample_rate,
        rule.sample_rate,
        now,
    ) {
        Ok(window) => window,
        Err(errors) => return Ok(Create::Invalid(errors)),
    };
    if !rule.enabled || rule.paused_reason.is_some() {
        return Ok(Create::Inactive);
    }
    let new = NewBackfill {
        id: Uuid::now_v7(),
        rule_id: id,
        from_ns: window.from,
        to_ns: window.to,
        max_targets: window.max_targets,
        sample_rate: window.sample_rate,
        created_by: Some(user_id),
    };
    Ok(Create::Created(Box::new(
        backfills::insert_backfill(conn, new, now).await?,
    )))
}

/// Checks the body against the limits (D-I-42). The sample rate defaults to the rule's.
fn checked_window(
    body: &CreateBackfillRequest,
    rule_rate: f64,
    now: i64,
) -> Result<Window, ApiError> {
    backfill::validate(
        body.from.ns(),
        body.to.ns(),
        body.max_targets,
        body.sample_rate,
        rule_rate,
        now,
    )
    .map_err(|errors| ApiError::BadRequest {
        detail: None,
        errors,
    })
}

/// What the estimate needs of a judge rule (the template, the mapped variables, the output cap
/// and the model's price). `None` for check and webhook rules.
async fn judge_cost(
    state: &AppState,
    org: OrgId,
    rule: &RuleRow,
    now: i64,
) -> Result<Option<JudgeCost>, ApiError> {
    let evaluator =
        evaluators::get_evaluator(state.meta.reader(), rule.project_id, rule.evaluator_id)
            .await?
            .ok_or_else(|| corrupt("evaluator_id"))?;
    let subject = Subject::from_row(&evaluator).map_err(internal)?;
    let Subject::Judge(judge) = &subject else {
        return Ok(None);
    };
    let adapter = judge_adapter(state, org, &subject).await?;
    let price = adapter.and_then(|adapter| {
        let prices = state.ingest.prices();
        budget::model_price(&prices, &judge.model, adapter, now).cloned()
    });
    let mapping: BTreeMap<String, String> =
        serde_json::from_str(&rule.variable_mapping).map_err(|_| corrupt("variable_mapping"))?;
    Ok(Some(JudgeCost {
        template_chars: judge.prompt.chars().count(),
        variables: mapping.len(),
        max_output_tokens: judge.max_output_tokens,
        price,
    }))
}

/// The API answer of a stored backfill. The error is shown only for a `failed` backfill.
fn backfill_json(row: &BackfillRow) -> Result<Backfill, ApiError> {
    let status = BackfillStatus::from_name(&row.status).ok_or_else(|| {
        ApiError::Internal(Box::new(MetaError::Corrupt {
            table: "backfills",
            column: "status",
        }))
    })?;
    Ok(Backfill {
        id: row.id,
        rule_id: row.rule_id,
        status,
        from: row.from_ns,
        to: row.to_ns,
        max_targets: row.max_targets,
        sample_rate: row.sample_rate,
        matched: row.matched,
        enqueued: row.enqueued,
        error: if status == BackfillStatus::Failed {
            row.error.clone()
        } else {
            None
        },
        created_by: row.created_by,
        created_at: row.created_at,
        finished_at: row.finished_at,
    })
}

fn corrupt(column: &'static str) -> ApiError {
    ApiError::Internal(Box::new(MetaError::Corrupt {
        table: "online_rules",
        column,
    }))
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> ApiError {
    ApiError::Internal(Box::new(error))
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, header};
    use llmobs_core::enums::SpanStatus;
    use llmobs_core::ids::{SpanId, TraceId};
    use llmobs_core::span::Span;
    use llmobs_eval::Adapter;
    use serde_json::{Value, json};

    use super::*;
    use crate::auth::roles::OrgRole;
    use crate::eval::budget::DAY_NS;
    use crate::meta::backfills::{BackfillEnd, ERROR_INTERRUPTED};
    use crate::meta::connections::{ConnectionFields, insert_connection};
    use crate::meta::evaluators::{EvaluatorFields, insert_evaluator};
    use crate::meta::online_rules::PAUSE_ERRORS;
    use crate::test_support::{Seeded, TestApp, TestResponse, capture_logs, dataset_span};

    /// The judge's prompt maps `input` and `output`.
    const PROMPT: &str = "Question: {{ input }}\nAnswer: {{ output }}";
    /// A model with a price in the built-in table (the budget tests' model).
    const PRICED: &str = "gpt-4o-mini";
    /// 2026-01-02T12:00:00Z. The committed traces start before it.
    const NOW: i64 = 1_767_355_200_000_000_000;
    const HOUR_NS: i64 = 3_600_000_000_000;
    const MINUTE_NS: i64 = 60_000_000_000;

    fn path(project: ProjectId) -> String {
        format!("/api/v1/projects/{project}/online-rules")
    }

    fn backfills_path(project: ProjectId, rule: Uuid) -> String {
        format!("{}/{rule}/backfills", path(project))
    }

    fn judge_fields(connection: Uuid, model: &str) -> EvaluatorFields {
        EvaluatorFields {
            name: format!("judge {connection}"),
            config: json!({
                "connection_id": connection,
                "model": model,
                "prompt": PROMPT,
                "output": {"type": "boolean"},
                "include_reason": false,
                "temperature": null,
                "max_output_tokens": 1024,
                "timeout_ms": 60000
            })
            .to_string(),
            output_score: json!({"name": "relevance", "data_type": "boolean"}).to_string(),
            connection_id: Some(connection),
            secret_enc: None,
            version: 1,
        }
    }

    fn connection_fields(name: String) -> ConnectionFields {
        ConnectionFields {
            name,
            adapter: "openai".to_owned(),
            base_url: None,
            secret_enc: vec![1],
            api_key_hint: "WXYZ".to_owned(),
            extra_headers_enc: None,
            extra_header_names: Vec::new(),
            default_model: None,
            structured_output: "native".to_owned(),
            max_concurrency: 4,
            requests_per_minute: 300,
        }
    }

    /// A connection and a judge on it with `model`. Returns the judge id.
    async fn judge_with_model(app: &TestApp, seeded: &Seeded, model: &'static str) -> Uuid {
        let (org, project, now) = (seeded.org_id, seeded.project.id, app.state.clock.now_ns());
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let connection = Uuid::now_v7();
                    insert_connection(
                        conn,
                        connection,
                        org,
                        connection_fields(format!("judge {connection}")),
                        None,
                        now,
                    )
                    .await?;
                    let judge = Uuid::now_v7();
                    insert_evaluator(
                        conn,
                        judge,
                        project,
                        "llm_judge",
                        judge_fields(connection, model),
                        None,
                        now,
                    )
                    .await?;
                    Ok(judge)
                })
            })
            .await
            .unwrap()
    }

    /// A check evaluator (`json_valid`) in `seeded`'s project.
    async fn check(app: &TestApp, seeded: &Seeded) -> Uuid {
        let (project, now) = (seeded.project.id, app.state.clock.now_ns());
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let id = Uuid::now_v7();
                    insert_evaluator(
                        conn,
                        id,
                        project,
                        "check",
                        EvaluatorFields {
                            name: "json".to_owned(),
                            config: json!({"check": "json_valid"}).to_string(),
                            output_score: json!({"name": "json", "data_type": "boolean"})
                                .to_string(),
                            connection_id: None,
                            secret_enc: None,
                            version: 1,
                        },
                        None,
                        now,
                    )
                    .await?;
                    Ok(id)
                })
            })
            .await
            .unwrap()
    }

    /// A trace rule on `evaluator`, with a budget when `with_budget` (the model must be priced).
    fn trace_rule(evaluator: Uuid, with_budget: bool) -> Value {
        let mut body = json!({
            "name": "Judge on traces",
            "evaluator_id": evaluator,
            "target": "trace",
            "variable_mapping": {"input": "trace.input", "output": "trace.output"},
            "sample_rate": 1.0,
            "enabled": true
        });
        if with_budget {
            body["budget_usd"] = json!(5.0);
            body["budget_period"] = json!("day");
        }
        body
    }

    async fn create_rule(app: &TestApp, seeded: &Seeded, body: &Value) -> Uuid {
        let response = app
            .request(Method::POST, &path(seeded.project.id))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(body)
            .send()
            .await;
        assert_eq!(response.status, 201, "{}", text(&response));
        id_of(&response.json())
    }

    /// A trace of `trace` that starts at `start`.
    fn trace_span(trace: u8, start: i64) -> Span {
        let mut row = dataset_span(0);
        row.trace_id = TraceId::from_bytes([trace; 16]).expect("trace id");
        row.span_id = SpanId::from_bytes([1; 8]).expect("span id");
        row.parent_span_id = None;
        row.start_time = start;
        row.end_time = start + 1_000_000;
        row.status = SpanStatus::Ok;
        row.metadata.clear();
        row
    }

    /// Three traces, one, two and three hours before `NOW`.
    async fn commit_three_traces(app: &TestApp, seeded: &Seeded) {
        app.commit_spans(
            &seeded.project,
            vec![
                trace_span(1, NOW - HOUR_NS),
                trace_span(2, NOW - 2 * HOUR_NS),
                trace_span(3, NOW - 3 * HOUR_NS),
            ],
        )
        .await;
    }

    /// Epoch milliseconds as a string, the form `TimeParam` takes in a body.
    fn millis(ns: i64) -> String {
        (ns / 1_000_000).to_string()
    }

    /// The window of the tests: the last two days, ending 10 minutes ago.
    fn window_body(max_targets: u64) -> Value {
        json!({
            "from": millis(NOW - 2 * DAY_NS),
            "to": millis(NOW - 10 * MINUTE_NS),
            "max_targets": max_targets
        })
    }

    async fn estimate(app: &TestApp, seeded: &Seeded, rule: Uuid, body: &Value) -> TestResponse {
        app.request(
            Method::POST,
            &format!("{}/estimate", backfills_path(seeded.project.id, rule)),
        )
        .header(header::COOKIE.as_str(), &seeded.cookie)
        .json(body)
        .send()
        .await
    }

    async fn create(app: &TestApp, seeded: &Seeded, rule: Uuid, body: &Value) -> TestResponse {
        app.request(Method::POST, &backfills_path(seeded.project.id, rule))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(body)
            .send()
            .await
    }

    async fn list(app: &TestApp, seeded: &Seeded, rule: Uuid) -> TestResponse {
        app.request(Method::GET, &backfills_path(seeded.project.id, rule))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await
    }

    async fn patch(app: &TestApp, seeded: &Seeded, rule: Uuid, body: &Value) -> TestResponse {
        app.request(
            Method::PATCH,
            &format!("{}/{rule}", path(seeded.project.id)),
        )
        .header(header::COOKIE.as_str(), &seeded.cookie)
        .json(body)
        .send()
        .await
    }

    fn text(response: &TestResponse) -> String {
        String::from_utf8_lossy(&response.body).into_owned()
    }

    fn id_of(value: &Value) -> Uuid {
        value["id"].as_str().unwrap().parse().unwrap()
    }

    /// The `(field, message)` pairs of a 400 answer.
    fn field_errors(response: &TestResponse) -> Vec<(String, String)> {
        assert_eq!(response.status, 400, "{}", text(response));
        response
            .problem()
            .errors
            .into_iter()
            .map(|error| (error.field, error.message))
            .collect()
    }

    /// A backfill of `rule` written straight to the store, created at `at`.
    async fn insert_at(app: &TestApp, new: NewBackfill, at: i64) {
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { backfills::insert_backfill(conn, new, at).await.map(|_| ()) })
            })
            .await
            .unwrap();
    }

    async fn end_at(app: &TestApp, id: Uuid, end: BackfillEnd, at: i64) {
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    backfills::finish_backfill(conn, id, end, at)
                        .await
                        .map(|_| ())
                })
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn estimate_counts_the_window() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        commit_three_traces(&app, &seeded).await;
        let evaluator = judge_with_model(&app, &seeded, PRICED).await;
        let rule = create_rule(&app, &seeded, &trace_rule(evaluator, true)).await;
        let response = estimate(&app, &seeded, rule, &window_body(2)).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let body: Value = response.json();
        assert_eq!(body["matched"], 2, "{body}");
        assert_eq!(body["capped"], true);
        assert_eq!(body["sampled"], 2);
        assert_eq!(body["from"], "2025-12-31T12:00:00.000000000Z");
        assert_eq!(body["to"], "2026-01-02T11:50:00.000000000Z");
        // Each target sends the template and two mapped variables of 2 000 characters each.
        let chars = PROMPT.chars().count() + 2 * 2_000;
        assert_eq!(body["estimated_input_tokens"], 2 * chars.div_ceil(4) as u64);
        assert_eq!(body["estimated_output_tokens"], 2 * 1024);
        let prices = app.state.ingest.prices();
        let price = budget::model_price(&prices, PRICED, Adapter::OpenAi, NOW).expect("priced");
        let expected = 2.0 * budget::estimate_usd(chars, 1024, price);
        let cost = body["estimated_cost_usd"].as_f64().expect("a cost");
        assert!((cost - expected).abs() < 1e-12, "{cost} != {expected}");
    }

    #[tokio::test]
    async fn estimate_without_a_price_has_tokens_only() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        commit_three_traces(&app, &seeded).await;
        let evaluator = judge_with_model(&app, &seeded, "gpt-test").await;
        let rule = create_rule(&app, &seeded, &trace_rule(evaluator, false)).await;
        let response = estimate(&app, &seeded, rule, &window_body(10)).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let body: Value = response.json();
        assert_eq!(body["matched"], 3);
        assert_eq!(body["capped"], false);
        assert!(body["estimated_input_tokens"].as_u64().is_some(), "{body}");
        assert_eq!(body["estimated_output_tokens"], 3 * 1024);
        assert!(body.get("estimated_cost_usd").is_none(), "{body}");
    }

    #[tokio::test]
    async fn a_check_rule_estimate_has_no_cost_fields() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        commit_three_traces(&app, &seeded).await;
        let evaluator = check(&app, &seeded).await;
        let rule = create_rule(
            &app,
            &seeded,
            &json!({
                "name": "JSON",
                "evaluator_id": evaluator,
                "target": "trace",
                "variable_mapping": {"output": "trace.output"},
                "sample_rate": 1.0,
                "enabled": true
            }),
        )
        .await;
        let response = estimate(&app, &seeded, rule, &window_body(10)).await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let body: Value = response.json();
        assert_eq!(body["matched"], 3);
        for field in [
            "estimated_input_tokens",
            "estimated_output_tokens",
            "estimated_cost_usd",
        ] {
            assert!(body.get(field).is_none(), "{field} in {body}");
        }
    }

    #[tokio::test]
    async fn estimate_validates_the_body() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        let evaluator = judge_with_model(&app, &seeded, PRICED).await;
        let rule = create_rule(&app, &seeded, &trace_rule(evaluator, true)).await;
        let with = |change: &dyn Fn(&mut Value)| {
            let mut body = window_body(10);
            change(&mut body);
            body
        };
        let too_recent = with(&|body| body["to"] = json!(millis(NOW - MINUTE_NS)));
        let response = estimate(&app, &seeded, rule, &too_recent).await;
        let errors = field_errors(&response);
        assert!(errors.iter().any(|(field, _)| field == "to"), "{errors:?}");

        let backwards = with(&|body| body["from"] = json!(millis(NOW - 5 * MINUTE_NS)));
        let errors = field_errors(&estimate(&app, &seeded, rule, &backwards).await);
        assert!(
            errors.iter().any(|(field, _)| field == "from"),
            "{errors:?}"
        );

        let too_wide = with(&|body| body["from"] = json!(millis(NOW - 31 * DAY_NS)));
        let errors = field_errors(&estimate(&app, &seeded, rule, &too_wide).await);
        assert!(
            errors
                .iter()
                .any(|(field, message)| field == "from" && message.contains("30 days")),
            "{errors:?}"
        );

        for max_targets in [0, 10_001] {
            let body = with(&|body| body["max_targets"] = json!(max_targets));
            let errors = field_errors(&estimate(&app, &seeded, rule, &body).await);
            assert!(
                errors.iter().any(|(field, _)| field == "max_targets"),
                "{errors:?}"
            );
        }
        for sample_rate in [0.0, 1.5] {
            let body = with(&|body| body["sample_rate"] = json!(sample_rate));
            let errors = field_errors(&estimate(&app, &seeded, rule, &body).await);
            assert!(
                errors.iter().any(|(field, _)| field == "sample_rate"),
                "{errors:?}"
            );
        }
        let dry_run = with(&|body| body["dry_run"] = json!(true));
        let response = estimate(&app, &seeded, rule, &dry_run).await;
        assert_eq!(response.status, 400, "{}", text(&response));
    }

    #[tokio::test]
    async fn create_answers_202_and_lists_it() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        commit_three_traces(&app, &seeded).await;
        let evaluator = judge_with_model(&app, &seeded, PRICED).await;
        let rule = create_rule(&app, &seeded, &trace_rule(evaluator, true)).await;
        let mut body = window_body(1000);
        body["sample_rate"] = json!(0.25);
        let response = create(&app, &seeded, rule, &body).await;
        assert_eq!(response.status, 202, "{}", text(&response));
        let created: Value = response.json();
        assert_eq!(created["status"], "running");
        assert_eq!(created["matched"], 0);
        assert_eq!(created["enqueued"], 0);
        assert_eq!(created["sample_rate"], 0.25);
        assert_eq!(created["created_by"], seeded.user_id.to_string());
        assert!(created.get("error").is_none());
        assert!(created.get("finished_at").is_none());

        let listed: Value = list(&app, &seeded, rule).await.json();
        assert_eq!(listed["next_cursor"], Value::Null);
        assert_eq!(listed["data"][0]["id"], created["id"], "{listed}");
    }

    #[tokio::test]
    async fn a_second_backfill_while_one_runs_is_409() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        let evaluator = judge_with_model(&app, &seeded, PRICED).await;
        let rule = create_rule(&app, &seeded, &trace_rule(evaluator, true)).await;
        assert_eq!(
            create(&app, &seeded, rule, &window_body(10)).await.status,
            202
        );
        let second = create(&app, &seeded, rule, &window_body(10)).await;
        assert_eq!(second.status, 409, "{}", text(&second));
        assert!(
            text(&second).contains("already running"),
            "{}",
            text(&second)
        );
    }

    #[tokio::test]
    async fn a_disabled_or_paused_rule_is_409() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        let evaluator = judge_with_model(&app, &seeded, PRICED).await;
        let rule = create_rule(&app, &seeded, &trace_rule(evaluator, true)).await;
        let disabled = patch(&app, &seeded, rule, &json!({"enabled": false})).await;
        assert_eq!(disabled.status, 200, "{}", text(&disabled));
        let refused = create(&app, &seeded, rule, &window_body(10)).await;
        assert_eq!(refused.status, 409, "{}", text(&refused));
        assert!(
            text(&refused).contains("enabled and not paused"),
            "{}",
            text(&refused)
        );

        let enabled = patch(&app, &seeded, rule, &json!({"enabled": true})).await;
        assert_eq!(enabled.status, 200, "{}", text(&enabled));
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("UPDATE online_rules SET paused_reason = ? WHERE id = ?")
                        .bind(PAUSE_ERRORS)
                        .bind(rule.to_string())
                        .execute(&mut *conn)
                        .await
                        .map(|_| ())
                        .map_err(MetaError::from)
                })
            })
            .await
            .unwrap();
        let paused = create(&app, &seeded, rule, &window_body(10)).await;
        assert_eq!(paused.status, 409, "{}", text(&paused));
        assert!(
            text(&paused).contains("enabled and not paused"),
            "{}",
            text(&paused)
        );
    }

    #[tokio::test]
    async fn another_projects_rule_is_404() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        let evaluator = judge_with_model(&app, &seeded, PRICED).await;
        let rule = create_rule(&app, &seeded, &trace_rule(evaluator, true)).await;
        let (other, _) = app.add_project(&seeded, "other", None).await;
        for response in [
            app.request(Method::GET, &backfills_path(other.id, rule))
                .header(header::COOKIE.as_str(), &seeded.cookie)
                .send()
                .await,
            app.request(
                Method::POST,
                &format!("{}/estimate", backfills_path(other.id, rule)),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&window_body(10))
            .send()
            .await,
            app.request(Method::POST, &backfills_path(other.id, rule))
                .header(header::COOKIE.as_str(), &seeded.cookie)
                .json(&window_body(10))
                .send()
                .await,
        ] {
            assert_eq!(response.status, 404, "{}", text(&response));
        }
    }

    #[tokio::test]
    async fn viewers_and_api_keys_cannot_start_or_estimate() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        let evaluator = judge_with_model(&app, &seeded, PRICED).await;
        let rule = create_rule(&app, &seeded, &trace_rule(evaluator, true)).await;
        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let as_viewer = |method: Method, suffix: &str| {
            app.request(
                method,
                &format!("{}{suffix}", backfills_path(seeded.project.id, rule)),
            )
            .header(header::COOKIE.as_str(), &viewer.cookie)
            .json(&window_body(10))
            .send()
        };
        assert_eq!(as_viewer(Method::POST, "/estimate").await.status, 403);
        assert_eq!(as_viewer(Method::POST, "").await.status, 403);
        let listed = app
            .request(Method::GET, &backfills_path(seeded.project.id, rule))
            .header(header::COOKIE.as_str(), &viewer.cookie)
            .send()
            .await;
        assert_eq!(listed.status, 200, "{}", text(&listed));

        let key = format!("Bearer {}", seeded.read_key);
        let by_key_estimate = app
            .request(
                Method::POST,
                &format!("{}/estimate", backfills_path(seeded.project.id, rule)),
            )
            .header(header::AUTHORIZATION.as_str(), &key)
            .json(&window_body(10))
            .send()
            .await;
        assert_eq!(by_key_estimate.status, 403, "{}", text(&by_key_estimate));
        let by_key = app
            .request(Method::POST, &backfills_path(seeded.project.id, rule))
            .header(header::AUTHORIZATION.as_str(), &key)
            .json(&window_body(10))
            .send()
            .await;
        assert_eq!(by_key.status, 403, "{}", text(&by_key));
    }

    #[tokio::test]
    async fn create_is_audited() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        let evaluator = judge_with_model(&app, &seeded, PRICED).await;
        let rule = create_rule(&app, &seeded, &trace_rule(evaluator, true)).await;
        let (response, logs) = capture_logs(create(&app, &seeded, rule, &window_body(10))).await;
        assert_eq!(response.status, 202, "{}", text(&response));
        let backfill = id_of(&response.json());
        assert!(logs.contains("online rule backfill started"), "{logs}");
        assert!(logs.contains(&backfill.to_string()), "{logs}");
        assert!(logs.contains(&seeded.user_id.to_string()), "{logs}");
    }

    #[tokio::test]
    async fn the_list_is_newest_first() {
        let app = TestApp::new().await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        let evaluator = judge_with_model(&app, &seeded, PRICED).await;
        let rule = create_rule(&app, &seeded, &trace_rule(evaluator, true)).await;
        // A backfill created `hours_ago` hours before NOW, and its id.
        let new = |hours_ago: i64| {
            let new = NewBackfill {
                id: Uuid::now_v7(),
                rule_id: rule,
                from_ns: NOW - 2 * DAY_NS,
                to_ns: NOW - 10 * MINUTE_NS,
                max_targets: 100,
                sample_rate: 1.0,
                created_by: Some(seeded.user_id),
            };
            (new, NOW - hours_ago * HOUR_NS)
        };
        let (oldest, oldest_at) = new(3);
        insert_at(&app, oldest, oldest_at).await;
        end_at(
            &app,
            oldest.id,
            BackfillEnd::Done,
            oldest_at + 5_000_000_000,
        )
        .await;
        let (middle, middle_at) = new(2);
        insert_at(&app, middle, middle_at).await;
        end_at(
            &app,
            middle.id,
            BackfillEnd::Failed(ERROR_INTERRUPTED),
            middle_at + 5_000_000_000,
        )
        .await;
        let (newest, newest_at) = new(1);
        insert_at(&app, newest, newest_at).await;

        let listed: Value = list(&app, &seeded, rule).await.json();
        let data = listed["data"].as_array().expect("a list");
        let ids: Vec<&Value> = data.iter().map(|row| &row["id"]).collect();
        assert_eq!(
            ids,
            [&json!(newest.id), &json!(middle.id), &json!(oldest.id)],
            "{listed}"
        );
        assert_eq!(data[0]["status"], "running");
        assert!(data[0].get("error").is_none(), "{listed}");
        assert_eq!(data[1]["status"], "failed");
        assert_eq!(data[1]["error"], ERROR_INTERRUPTED);
        assert_eq!(data[2]["status"], "done");
        assert!(data[2].get("error").is_none(), "{listed}");
        assert!(data[2].get("finished_at").is_some(), "{listed}");
    }

    #[tokio::test]
    async fn create_backfill_is_refused_in_restore_mode() {
        let app = TestApp::with_config(|config| config.restore_mode = true).await;
        app.state.clock.set(NOW);
        let seeded = app.seed().await;
        let evaluator = judge_with_model(&app, &seeded, PRICED).await;
        let rule = create_rule(&app, &seeded, &trace_rule(evaluator, true)).await;
        let response = create(&app, &seeded, rule, &window_body(10)).await;
        crate::http::restore::tests::assert_refused(&response, "backfill");
        let listed: Value = list(&app, &seeded, rule).await.json();
        assert_eq!(listed["data"], json!([]));
    }
}
