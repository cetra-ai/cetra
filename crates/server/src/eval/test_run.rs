//! One synchronous evaluator test run (D-I-21, spec 06 § Evaluators). It writes the judge trace
//! and the judge usage, and no score. The body rules and the answer are § Test run contract.

use std::collections::BTreeMap;
use std::time::Instant;

use llmobs_core::ids::{SpanId, TraceId};
use llmobs_eval::{
    ErrorClass, EvalOrigin, EvalRun, EvalTarget, EvaluatorConfig, OutputScore, PromptTemplate,
    Secret, TraceData, VariableMapping, Variables,
};
use serde_json::Value;
use uuid::Uuid;

use crate::api::common::{FieldError, TimeParam};
use crate::api::connections::TestErrorClass;
use crate::api::evaluators::{
    EvaluatorKind, EvaluatorTestError, EvaluatorTestRequest, EvaluatorTestResult,
    EvaluatorTestUsage, OutputScoreBody, TestEvaluatorBody,
};
use crate::eval::evaluator::{self, EvaluatorError, Prepared};
use crate::eval::execute::{self, RunArgs, render};
use crate::eval::trace_load::{self, LoadError};
use crate::http::AppState;
use crate::http::error::ApiError;
use crate::meta::connections;
use crate::meta::evaluators::{self as store, EvaluatorRow};
use crate::meta::projects::ProjectRow;

/// Longest rendered variable in an answer, in `char`s, before the cut (D-I-21).
const ANSWER_CHARS: usize = 2_000;
/// Most literal variables (§ Test run contract).
const MAX_VARIABLES: usize = 32;
/// Most bytes the literal variables may take in all.
const MAX_VARIABLE_BYTES: usize = 256 * 1024;

/// Where a run's variables come from, as the body asks. Built only when the body is valid.
enum Source {
    /// A stored trace, read with a mapping.
    Trace {
        trace_id: TraceId,
        span_id: Option<SpanId>,
        around: Option<i64>,
        mapping: VariableMapping,
    },
    /// Literal variables.
    Literal(BTreeMap<String, Value>),
}

/// Runs one test (D-I-21). `user` is the caller, for the rate limit.
///
/// # Errors
///
/// Returns [`ApiError::RateLimited`] past 10 tests a minute, [`ApiError::BadRequest`] with every
/// field error of an invalid body, 404 for an unknown evaluator or trace, and 503 when storage is
/// busy. A failed evaluator is an answer with `ok: false`, not an error.
pub(crate) async fn run(
    state: &AppState,
    project: &ProjectRow,
    user: Uuid,
    body: EvaluatorTestRequest,
) -> Result<EvaluatorTestResult, ApiError> {
    state
        .limits
        .connection_tests
        .check(&user, state.clock.now_ns())
        .map_err(|retry| ApiError::RateLimited {
            retry_after_s: retry.0.as_secs().max(1),
        })?;
    let EvaluatorTestRequest {
        evaluator_id,
        evaluator,
        trace_id,
        span_id,
        trace_start,
        variable_mapping,
        variables,
    } = body;
    let mut errors = Vec::new();
    let source = parse_source(
        trace_id.as_deref(),
        span_id.as_deref(),
        trace_start.map(TimeParam::ns),
        variable_mapping,
        variables,
        &mut errors,
    );
    let saved = match evaluator_id {
        Some(id) => Some(
            store::get_evaluator(state.meta.reader(), project.id, id)
                .await?
                .ok_or(ApiError::NotFound)?,
        ),
        None => None,
    };
    let prepared = evaluator_to_run(state, saved.as_ref(), evaluator, &mut errors)?;
    let (Some(source), Some(prepared)) = (source, prepared) else {
        return Err(bad_request(errors));
    };
    let connection = match prepared.connection_id {
        Some(connection) => {
            let row = connections::get_connection(state.meta.reader(), project.org_id, connection)
                .await?;
            match row {
                Some(row) => Some(
                    state
                        .connections
                        .get(&state.secrets, &row)
                        .map_err(internal)?,
                ),
                None => {
                    return Err(bad_request(vec![field_error(
                        "evaluator.config.connection_id",
                        "connection not found in this organization",
                    )]));
                }
            }
        }
        None => None,
    };

    let (variables, target, truncated, users) = match source {
        Source::Literal(map) => (
            Variables::from_map(map),
            EvalTarget {
                trace_id: None,
                span_id: None,
            },
            false,
            Vec::new(),
        ),
        Source::Trace {
            trace_id,
            span_id,
            around,
            mapping,
        } => {
            let loaded =
                match trace_load::load_trace(state, project.id, trace_id, span_id, around).await {
                    Ok(loaded) => loaded,
                    Err(LoadError::TooLarge) => {
                        return Ok(refused(TestErrorClass::Permanent, "trace too large", true));
                    }
                    Err(LoadError::NotFound | LoadError::NoTargetSpan) => {
                        return Err(ApiError::NotFound);
                    }
                    Err(LoadError::EvaluationTrace) => {
                        return Err(ApiError::BadRequest {
                            detail: Some("target is an evaluation trace".to_owned()),
                            errors: Vec::new(),
                        });
                    }
                    Err(LoadError::Busy | LoadError::Saturated) => {
                        return Err(ApiError::Unavailable {
                            detail: "storage is busy",
                        });
                    }
                    Err(error @ (LoadError::Failed(_) | LoadError::Convert(_))) => {
                        return Err(internal(error));
                    }
                };
            // D-M-6, check A: the target is loaded and nothing has run yet. A deleted trace is
            // not found, as it is not for the caller any more.
            let users = execute::users_of(&loaded.spans);
            if state
                .deletions
                .guard
                .blocking(
                    project.id,
                    trace_id,
                    &execute::borrowed_users(&users),
                    state.clock.now_ns(),
                )
                .is_some()
            {
                return Err(ApiError::NotFound);
            }
            let data = TraceData::new(&loaded.spans, span_id);
            match llmobs_eval::resolve(&mapping, Some(&data), None) {
                Ok(variables) => (
                    variables,
                    EvalTarget {
                        trace_id: Some(trace_id),
                        span_id,
                    },
                    loaded.truncated,
                    users,
                ),
                // A mapping error is refused before the evaluator runs: no judge trace.
                Err(error) => {
                    return Ok(refused(
                        TestErrorClass::Permanent,
                        &error.to_string(),
                        false,
                    ));
                }
            }
        }
    };
    if execute::levenshtein_too_large(&prepared.evaluator, &variables) {
        return Err(bad_request(vec![field_error(
            "variables",
            "levenshtein inputs must be at most 64 KiB each",
        )]));
    }
    let rendered = rendered_variables(&variables);
    let prompt = match &prepared.evaluator.config {
        EvaluatorConfig::LlmJudge(judge) => PromptTemplate::compile(&judge.prompt)
            .and_then(|template| template.render(&variables))
            .ok(),
        EvaluatorConfig::Check(_) | EvaluatorConfig::Webhook(_) => None,
    };

    let judge_trace_id = judge_trace_id()?;
    // The target of a trace run is guarded before the commit and after it (D-M-6).
    let guarded = target.trace_id;
    let guard_users = execute::borrowed_users(&users);
    let now = state.clock.now_ns();
    let signing_secret = prepared
        .secrets
        .as_ref()
        .map(|values| Secret::new(values.signing_secret.clone()));
    let evaluator_id = prepared.evaluator.id;
    let started = Instant::now();
    let run = execute::run_evaluator(
        state,
        RunArgs {
            project,
            evaluator: prepared.evaluator,
            connection,
            webhook_secret: signing_secret,
            variables,
            target,
            origin: EvalOrigin::Test,
            judge_trace_id,
            now,
        },
    )
    .await
    .map_err(internal)?;
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    let usage = execute::judge_usage(&run);
    if let Some(usage) = usage {
        execute::count_judge_usage(state, project, now, usage);
    }
    let EvalRun {
        outcome,
        judge_trace,
    } = run;
    // D-M-6: check B, right before the commit of the judge trace.
    if let Some(target_trace) = guarded
        && state
            .deletions
            .guard
            .blocking(project.id, target_trace, &guard_users, state.clock.now_ns())
            .is_some()
    {
        return Err(ApiError::NotFound);
    }
    // The judge trace is committed on its own (D-I-16); its spans are not billable (D-I-31).
    execute::commit_judge_trace(state, project, now, judge_trace).await?;
    // D-M-6: check C, after the commit. A block stores the follow-up deletion of the judge trace
    // and the target (execute::after_commit).
    if let Some(target_trace) = guarded
        && execute::after_commit(
            state,
            project.id,
            target_trace,
            &guard_users,
            judge_trace_id,
        )
        .await
    {
        return Err(ApiError::NotFound);
    }

    let mut result = EvaluatorTestResult {
        ok: false,
        value: None,
        reason: None,
        error: None,
        prompt,
        variables: rendered,
        usage: usage.map(|usage| EvaluatorTestUsage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cost_usd: usage.cost_usd,
        }),
        latency_ms,
        trace_truncated: truncated,
        judge_trace_id: Some(judge_trace_id),
    };
    match outcome {
        Ok(outcome) => {
            result.ok = true;
            result.value = Some(outcome.value);
            result.reason = outcome.reason;
        }
        Err(error) => {
            result.error = Some(EvaluatorTestError {
                class: test_class(error.class()),
                // The display quotes no payload (D160).
                message: error.to_string(),
            });
        }
    }
    tracing::info!(
        target: "audit",
        evaluator_id = %evaluator_id,
        project_id = %project.id,
        user_id = %user,
        ok = result.ok,
        latency_ms,
        "evaluator tested"
    );
    Ok(result)
}

/// The body rules that need no database (§ Test run contract). Each failure is pushed to
/// `errors`; the source is returned only when none was pushed.
fn parse_source(
    trace_id: Option<&str>,
    span_id: Option<&str>,
    trace_start: Option<i64>,
    mapping: Option<BTreeMap<String, String>>,
    variables: Option<BTreeMap<String, Value>>,
    errors: &mut Vec<FieldError>,
) -> Option<Source> {
    let before = errors.len();
    let has_trace = trace_id.is_some();
    if has_trace == variables.is_some() {
        errors.push(field_error(
            "variables",
            "give trace_id with variable_mapping, or variables",
        ));
    }
    if has_trace && mapping.is_none() {
        errors.push(field_error(
            "variable_mapping",
            "variable_mapping is required with trace_id",
        ));
    }
    if !has_trace && mapping.is_some() {
        errors.push(field_error(
            "trace_id",
            "trace_id is required with variable_mapping",
        ));
    }
    if !has_trace && (span_id.is_some() || trace_start.is_some()) {
        errors.push(field_error(
            "trace_id",
            "trace_id is required with span_id and trace_start",
        ));
    }
    let trace = match trace_id.map(str::parse::<TraceId>) {
        Some(Ok(id)) => Some(id),
        Some(Err(_)) => {
            errors.push(field_error("trace_id", "trace_id must be 32 hex digits"));
            None
        }
        None => None,
    };
    let span = match span_id.map(str::parse::<SpanId>) {
        Some(Ok(id)) => Some(id),
        Some(Err(_)) => {
            errors.push(field_error("span_id", "span_id must be 16 hex digits"));
            None
        }
        None => None,
    };
    let mapping = mapping.and_then(|map| {
        let value = Value::Object(
            map.into_iter()
                .map(|(name, expression)| (name, Value::String(expression)))
                .collect(),
        );
        match VariableMapping::from_json(&value) {
            Ok(mapping) if mapping.needs_item() => {
                errors.push(field_error(
                    "variable_mapping",
                    "expected and item.* are for experiments only",
                ));
                None
            }
            Ok(mapping) => Some(mapping),
            Err(error) => {
                errors.push(field_error("variable_mapping", &error.to_string()));
                None
            }
        }
    });
    if let Some(map) = &variables {
        if map.len() > MAX_VARIABLES {
            errors.push(field_error("variables", "at most 32 variables"));
        }
        if !map.keys().all(|name| valid_name(name)) {
            errors.push(field_error("variables", "invalid variable name"));
        }
        let total = map
            .values()
            .map(|value| serde_json::to_string(value).map_or(0, |text| text.len()))
            .fold(0_usize, usize::saturating_add);
        if total > MAX_VARIABLE_BYTES {
            errors.push(field_error(
                "variables",
                "variables must be at most 256 KiB in total",
            ));
        }
    }
    if errors.len() > before {
        return None;
    }
    match variables {
        Some(map) if !has_trace => Some(Source::Literal(map)),
        _ => Some(Source::Trace {
            trace_id: trace?,
            span_id: span,
            around: trace_start,
            mapping: mapping?,
        }),
    }
}

/// The evaluator to run (§ Test run contract). `None` when the body is invalid; the errors are in
/// `errors`.
fn evaluator_to_run(
    state: &AppState,
    saved: Option<&EvaluatorRow>,
    body: Option<TestEvaluatorBody>,
    errors: &mut Vec<FieldError>,
) -> Result<Option<Prepared>, ApiError> {
    match (saved, body) {
        (None, None) => {
            errors.push(field_error(
                "evaluator",
                "give evaluator_id, evaluator, or both",
            ));
            Ok(None)
        }
        (Some(row), None) => {
            let (current, opened) = evaluator::load(&state.secrets, row).map_err(load_error)?;
            let config =
                evaluator::config_document(&current, opened.as_ref()).map_err(load_error)?;
            let secret = opened.map(|values| values.signing_secret);
            Ok(keep(
                evaluator::prepare(
                    current.id,
                    &current.name,
                    kind_of(&current.config),
                    &config,
                    &current.output,
                    secret.as_deref(),
                    "evaluator.",
                ),
                errors,
            ))
        }
        (Some(row), Some(body)) => {
            let (current, opened) = evaluator::load(&state.secrets, row).map_err(load_error)?;
            let kind = kind_of(&current.config);
            if body.kind != kind {
                errors.push(field_error(
                    "evaluator.kind",
                    "kind must match the saved evaluator",
                ));
                return Ok(None);
            }
            if kind == EvaluatorKind::Webhook
                && body.config.is_some()
                && body.webhook_secret.is_none()
            {
                errors.push(field_error(
                    "evaluator.webhook_secret",
                    evaluator::CONFIG_NEEDS_SECRET,
                ));
                return Ok(None);
            }
            let name = body.name.unwrap_or_else(|| current.name.clone());
            let config = match body.config {
                Some(config) => config,
                None => {
                    evaluator::config_document(&current, opened.as_ref()).map_err(load_error)?
                }
            };
            let secret = body
                .webhook_secret
                .or_else(|| opened.map(|values| values.signing_secret));
            Ok(keep(
                evaluator::prepare(
                    current.id,
                    &name,
                    kind,
                    &config,
                    &output_of(&body.output_score),
                    secret.as_deref(),
                    "evaluator.",
                ),
                errors,
            ))
        }
        (None, Some(body)) => {
            let name = body.name.unwrap_or_else(|| "Test".to_owned());
            match body.config {
                Some(config) => Ok(keep(
                    evaluator::prepare(
                        Uuid::nil(),
                        &name,
                        body.kind,
                        &config,
                        &output_of(&body.output_score),
                        body.webhook_secret.as_deref(),
                        "evaluator.",
                    ),
                    errors,
                )),
                None => {
                    errors.push(field_error(
                        "evaluator.config",
                        "config is required without evaluator_id",
                    ));
                    if body.kind == EvaluatorKind::Webhook && body.webhook_secret.is_none() {
                        errors.push(field_error(
                            "evaluator.webhook_secret",
                            "webhook_secret is required",
                        ));
                    }
                    Ok(None)
                }
            }
        }
    }
}

/// Keeps a prepared evaluator, or moves its field errors into `errors`.
fn keep(
    result: Result<Prepared, Vec<FieldError>>,
    errors: &mut Vec<FieldError>,
) -> Option<Prepared> {
    match result {
        Ok(prepared) => Some(prepared),
        Err(found) => {
            errors.extend(found);
            None
        }
    }
}

fn kind_of(config: &EvaluatorConfig) -> EvaluatorKind {
    match config {
        EvaluatorConfig::LlmJudge(_) => EvaluatorKind::LlmJudge,
        EvaluatorConfig::Check(_) => EvaluatorKind::Check,
        EvaluatorConfig::Webhook(_) => EvaluatorKind::Webhook,
    }
}

fn output_of(body: &OutputScoreBody) -> OutputScore {
    OutputScore {
        name: body.name.clone(),
        data_type: body.data_type,
        categories: body.categories.clone(),
    }
}

/// A variable name: `^[A-Za-z_][A-Za-z0-9_]{0,63}$`.
fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.is_ascii()
        && name.len() <= 64
        && chars
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|next| next.is_ascii_alphanumeric() || next == '_')
}

/// Every variable rendered and cut to [`ANSWER_CHARS`] characters, with a final `…` when cut.
fn rendered_variables(variables: &Variables) -> BTreeMap<String, String> {
    variables
        .iter()
        .map(|(name, value)| {
            let text = render(value);
            let text = if text.chars().count() > ANSWER_CHARS {
                let mut cut: String = text.chars().take(ANSWER_CHARS).collect();
                cut.push('…');
                cut
            } else {
                text
            };
            (name.to_owned(), text)
        })
        .collect()
}

/// The judge trace id: 16 random bytes (as M1's connection test).
fn judge_trace_id() -> Result<TraceId, ApiError> {
    let bytes = crate::auth::secret::random_bytes(16).map_err(internal)?;
    let bytes = <[u8; 16]>::try_from(bytes.as_slice()).map_err(internal)?;
    // Refuses all-zero bytes only.
    TraceId::from_bytes(bytes).map_err(internal)
}

fn test_class(class: ErrorClass) -> TestErrorClass {
    match class {
        ErrorClass::Retryable => TestErrorClass::Retryable,
        ErrorClass::Pause => TestErrorClass::Pause,
        ErrorClass::Permanent => TestErrorClass::Permanent,
    }
}

/// A failed answer that wrote no judge trace and ran nothing.
fn refused(class: TestErrorClass, message: &str, truncated: bool) -> EvaluatorTestResult {
    EvaluatorTestResult {
        ok: false,
        value: None,
        reason: None,
        error: Some(EvaluatorTestError {
            class,
            message: message.to_owned(),
        }),
        prompt: None,
        variables: BTreeMap::new(),
        usage: None,
        latency_ms: 0,
        trace_truncated: truncated,
        judge_trace_id: None,
    }
}

fn field_error(field: &str, message: &str) -> FieldError {
    FieldError {
        field: field.to_owned(),
        message: message.to_owned(),
        position: None,
    }
}

fn bad_request(errors: Vec<FieldError>) -> ApiError {
    ApiError::BadRequest {
        detail: None,
        errors,
    }
}

fn load_error(error: EvaluatorError) -> ApiError {
    ApiError::Internal(Box::new(error))
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> ApiError {
    ApiError::Internal(Box::new(error))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use axum::http::{Method, header};
    use llmobs_core::enums::PayloadFormat;
    use llmobs_core::ids::{SpanId, TraceId};
    use llmobs_core::span::Span;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use crate::test_support::mock_llm::{MockLlm, completion};
    use crate::test_support::{Seeded, TestApp, TestResponse, dataset_span};

    const TRACE: &str = "a1000000000000000000000000000001";
    const QUESTION: &str = "What is the refund window?";
    const ANSWER: &str = "30 days.";
    const JUDGE_ANSWER: &str = r#"{"explanation": "Relevant.", "value": 0.9}"#;

    /// A test app that may reach the mock on `127.0.0.1`, with the real time as its clock.
    async fn test_app() -> (TestApp, Seeded) {
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
        })
        .await;
        let real_now = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos(),
        )
        .expect("fits i64");
        app.state.clock.set(real_now);
        let seeded = app.seed().await;
        (app, seeded)
    }

    /// Stores the root span of `TRACE`: the question and its answer.
    async fn commit_target(app: &TestApp, seeded: &Seeded) {
        let now = app.state.clock.now_ns();
        let mut row: Span = dataset_span(0);
        row.trace_id = TRACE.parse::<TraceId>().expect("trace id");
        row.span_id = "b100000000000001".parse::<SpanId>().expect("span id");
        row.parent_span_id = None;
        row.input = Some(QUESTION.to_owned());
        row.input_format = Some(PayloadFormat::Text);
        row.output = Some(ANSWER.to_owned());
        row.output_format = Some(PayloadFormat::Text);
        row.environment = Some("production".to_owned());
        row.start_time = now - 30_000_000_000;
        row.end_time = now - 20_000_000_000;
        row.ingest_time = now;
        app.commit_spans(&seeded.project, vec![row]).await;
    }

    /// A connection of `acme` that reaches `mock`, with the default model `gpt-test`.
    async fn connection(app: &TestApp, seeded: &Seeded, mock: &MockLlm) -> Uuid {
        let response = app
            .request(Method::POST, "/api/v1/orgs/acme/llm-connections")
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&json!({
                "name": "Mocked",
                "adapter": "openai",
                "api_key": "sk-test-key-0123456789",
                "base_url": mock.base_url(),
                "default_model": "gpt-test"
            }))
            .send()
            .await;
        assert_eq!(response.status, 201, "{}", text(&response));
        let value: Value = response.json();
        Uuid::parse_str(value["id"].as_str().expect("id")).expect("uuid")
    }

    /// A trace-mode test body with a judge on `connection`.
    fn trace_body(connection: Uuid) -> Value {
        json!({
            "evaluator": {
                "kind": "llm_judge",
                "config": {
                    "connection_id": connection,
                    "model": "gpt-test",
                    "prompt": "Question: {{ input }}\nAnswer: {{ output }}",
                    "output": {"type": "numeric", "min": 0.0, "max": 1.0},
                    "include_reason": true,
                    "temperature": null,
                    "max_output_tokens": 1024,
                    "timeout_ms": 60000
                },
                "output_score": {"name": "relevance", "data_type": "numeric"}
            },
            "trace_id": TRACE,
            "variable_mapping": {"input": "trace.input", "output": "trace.output"}
        })
    }

    /// `POST …/evaluators/test` with the seeded cookie.
    async fn test_run(app: &TestApp, seeded: &Seeded, body: &Value) -> TestResponse {
        app.request(
            Method::POST,
            &format!("/api/v1/projects/{}/evaluators/test", seeded.project.id),
        )
        .header(header::COOKIE.as_str(), &seeded.cookie)
        .json(body)
        .send()
        .await
    }

    /// The trace ids `GET /traces?include_eval=true` lists: the target and every judge trace.
    async fn listed_traces(app: &TestApp, seeded: &Seeded) -> Vec<String> {
        let response = app
            .request(
                Method::GET,
                &format!(
                    "/api/v1/projects/{}/traces?include_eval=true",
                    seeded.project.id
                ),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let page: Value = response.json();
        let mut ids: Vec<String> = page["data"]
            .as_array()
            .expect("data")
            .iter()
            .map(|trace| trace["trace_id"].as_str().expect("trace_id").to_owned())
            .collect();
        ids.sort();
        ids
    }

    fn text(response: &TestResponse) -> String {
        String::from_utf8_lossy(&response.body).into_owned()
    }

    /// Waits until the mock has seen one request (the judge call is in flight).
    async fn judge_called(mock: &MockLlm) {
        for _ in 0..500 {
            if !mock.requests().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the judge was not called");
    }

    #[tokio::test]
    async fn test_run_on_a_deleted_trace_is_404() {
        let mock = MockLlm::start(vec![completion(JUDGE_ANSWER)]).await;
        let (app, seeded) = test_app().await;
        let connection = connection(&app, &seeded, &mock).await;
        commit_target(&app, &seeded).await;
        // D-M-6: the guard holds the trace from a deletion made just now.
        app.state.deletions.guard.add_traces(
            seeded.project.id,
            &[TRACE.parse().expect("trace id")],
            Uuid::now_v7(),
            app.state.clock.now_ns(),
        );

        let response = test_run(&app, &seeded, &trace_body(connection)).await;
        assert_eq!(response.status, 404, "{}", text(&response));
        assert!(
            mock.requests().is_empty(),
            "no request reaches the provider"
        );
        assert_eq!(listed_traces(&app, &seeded).await, vec![TRACE.to_owned()]);
    }

    #[tokio::test]
    async fn test_run_deleted_during_the_call_commits_nothing() {
        let mock =
            MockLlm::start_with_delay(vec![completion(JUDGE_ANSWER)], Duration::from_millis(1500))
                .await;
        let (app, seeded) = test_app().await;
        let connection = connection(&app, &seeded, &mock).await;
        commit_target(&app, &seeded).await;
        let body = trace_body(connection);
        let request = test_run(&app, &seeded, &body);
        let deletion = async {
            judge_called(&mock).await;
            // D-M-6: the deletion lands while the provider is answering.
            app.state.deletions.guard.add_traces(
                seeded.project.id,
                &[TRACE.parse().expect("trace id")],
                Uuid::now_v7(),
                app.state.clock.now_ns(),
            );
        };
        let (response, ()) = tokio::join!(request, deletion);
        assert_eq!(response.status, 404, "{}", text(&response));
        // Only the target is stored: the judge trace was not committed.
        assert_eq!(listed_traces(&app, &seeded).await, vec![TRACE.to_owned()]);
    }
}
