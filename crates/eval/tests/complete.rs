//! Structured completions (`EvalRunner::complete`) against the in-process mock.
//! Nothing here calls a real provider.
#![allow(clippy::expect_used)]

use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cetra_core::enums::SpanKind;
use cetra_core::ids::{ProjectId, TraceId};
use cetra_core::names::environment;
use cetra_otel::{IngestContext, IngestLimits, PriceTable, builtin_price_rows};
use serde_json::{Value, json};

use crate::error::{ErrorClass, EvalError, JudgeError, assert_no_payload};
use crate::mock::{Mock, MockReply};
use crate::{
    Adapter, CallTrace, CompletionInput, EgressPolicy, EvalRunner, JudgeConnection, Secret,
    StructuredOutput,
};

const KEY: &str = "sk-test-SECRET-PAYLOAD";
const PROJECT: &str = "01900000-0000-7000-8000-000000000001";
const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const PROMPT: &str = "Summarize the issue.";
const NAME: &str = "issue_summary";
const MODEL: &str = "gpt-4o-mini";

fn prices() -> &'static PriceTable {
    static PRICES: OnceLock<PriceTable> = OnceLock::new();
    PRICES.get_or_init(|| PriceTable::new(builtin_price_rows()).expect("built-in prices"))
}

fn now_ns() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos(),
    )
    .unwrap_or(i64::MAX)
}

fn ingest() -> IngestContext {
    IngestContext {
        project_id: PROJECT.parse::<ProjectId>().expect("project"),
        ingest_time: now_ns(),
        retention_days: Some(7),
        limits: IngestLimits::default(),
    }
}

fn runner() -> EvalRunner {
    EvalRunner::new(EgressPolicy {
        allow_private_network: true,
        restore_mode: false,
    })
    .expect("runner")
}

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "summary": {"type": "string"},
            "is_issue": {"type": "boolean"}
        },
        "required": ["summary", "is_issue"],
        "additionalProperties": false
    })
}

fn connection(adapter: Adapter, base: reqwest::Url, mode: StructuredOutput) -> JudgeConnection {
    JudgeConnection {
        adapter,
        base_url: Some(base),
        api_key: Secret::new(KEY.to_owned()),
        extra_headers: Vec::new(),
        structured_output: mode,
    }
}

fn trace<'a>(ingest: &'a IngestContext, metadata: &'a [(&'a str, &'a str)]) -> CallTrace<'a> {
    CallTrace {
        trace_id: TRACE.parse::<TraceId>().expect("trace id"),
        root_name: "cetra.issue.summarize",
        metadata,
        ingest,
        prices: prices(),
    }
}

fn input<'a>(
    connection: &'a JudgeConnection,
    model: &'a str,
    name: &'a str,
    schema: &'a Value,
    trace: CallTrace<'a>,
) -> CompletionInput<'a> {
    CompletionInput {
        connection,
        model,
        prompt: PROMPT,
        schema,
        name,
        description: "Summarize the issue.",
        temperature: None,
        max_output_tokens: 256,
        timeout: Duration::from_secs(2),
        trace,
    }
}

fn reply(body: Value) -> MockReply {
    MockReply {
        status: 200,
        body,
        delay: Duration::ZERO,
    }
}

fn openai_text(text: &str) -> MockReply {
    reply(json!({
        "model": MODEL,
        "choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": text}}],
        "usage": {"prompt_tokens": 1000, "completion_tokens": 100}
    }))
}

fn openai_tool(arguments: &str) -> MockReply {
    reply(json!({
        "model": MODEL,
        "choices": [{
            "finish_reason": "tool_calls",
            "message": {
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": NAME, "arguments": arguments}
                }]
            }
        }]
    }))
}

fn anthropic_text(text: &str) -> MockReply {
    reply(json!({
        "model": "claude-sonnet-4-5",
        "stop_reason": "end_turn",
        "content": [{"type": "text", "text": text}],
        "usage": {"input_tokens": 10, "output_tokens": 5}
    }))
}

fn gemini_text(text: &str) -> MockReply {
    reply(json!({
        "modelVersion": "gemini-2.5-flash",
        "candidates": [{
            "finishReason": "STOP",
            "content": {"role": "model", "parts": [{"text": text}]}
        }]
    }))
}

#[tokio::test]
async fn complete_returns_the_object_openai_native() {
    let mock = Mock::start().await;
    mock.enqueue(openai_text(r#"{"summary":"x","is_issue":true}"#));
    let conn = connection(Adapter::OpenAi, mock.url(), StructuredOutput::Native);
    let schema = schema();
    let ingest = ingest();
    let run = runner()
        .complete(input(&conn, MODEL, NAME, &schema, trace(&ingest, &[])))
        .await;
    let object = run.outcome.as_ref().expect("answer");
    assert_eq!(object["summary"], "x");
    assert_eq!(object["is_issue"], true);
    let seen = mock.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/v1/chat/completions");
    let format = &seen[0].body["response_format"]["json_schema"];
    assert_eq!(format["name"], NAME);
    assert_eq!(format["schema"], schema);
}

#[tokio::test]
async fn complete_tool_mode_forces_the_named_function() {
    let mock = Mock::start().await;
    mock.enqueue(openai_tool(r#"{"summary":"x","is_issue":false}"#));
    let conn = connection(Adapter::OpenAi, mock.url(), StructuredOutput::Tool);
    let schema = schema();
    let ingest = ingest();
    let run = runner()
        .complete(input(&conn, MODEL, NAME, &schema, trace(&ingest, &[])))
        .await;
    let object = run.outcome.as_ref().expect("answer");
    assert_eq!(object["is_issue"], false);
    let seen = mock.requests();
    assert_eq!(seen[0].body["tool_choice"]["function"]["name"], NAME);
}

#[tokio::test]
async fn complete_prompt_mode_appends_the_schema() {
    let mock = Mock::start().await;
    mock.enqueue(anthropic_text(r#"{"summary":"x","is_issue":true}"#));
    let conn = connection(Adapter::Anthropic, mock.origin(), StructuredOutput::Prompt);
    let schema = schema();
    let ingest = ingest();
    let run = runner()
        .complete(input(
            &conn,
            "claude-sonnet-4-5",
            NAME,
            &schema,
            trace(&ingest, &[]),
        ))
        .await;
    assert!(run.outcome.is_ok());
    let seen = mock.requests();
    assert_eq!(seen[0].path, "/v1/messages");
    let content = seen[0].body["messages"][0]["content"]
        .as_str()
        .expect("user content");
    let pretty = serde_json::to_string_pretty(&schema).expect("schema");
    assert!(content.starts_with(PROMPT));
    assert!(content.ends_with(&pretty), "{content}");
}

#[tokio::test]
async fn complete_gemini_native_sends_response_json_schema() {
    let mock = Mock::start().await;
    mock.enqueue(gemini_text(r#"{"summary":"x","is_issue":true}"#));
    let conn = connection(Adapter::Gemini, mock.origin(), StructuredOutput::Native);
    let schema = schema();
    let ingest = ingest();
    let run = runner()
        .complete(input(
            &conn,
            "gemini-2.5-flash",
            NAME,
            &schema,
            trace(&ingest, &[]),
        ))
        .await;
    assert!(run.outcome.is_ok());
    let seen = mock.requests();
    assert_eq!(
        seen[0].path,
        "/v1beta/models/gemini-2.5-flash:generateContent"
    );
    assert_eq!(
        seen[0].body["generationConfig"]["responseJsonSchema"],
        schema
    );
}

#[tokio::test]
async fn complete_rejects_an_answer_that_misses_a_required_field() {
    let mock = Mock::start().await;
    mock.enqueue(openai_text(r#"{"summary":"x"}"#));
    let conn = connection(Adapter::OpenAi, mock.url(), StructuredOutput::Native);
    let schema = schema();
    let ingest = ingest();
    let run = runner()
        .complete(input(&conn, MODEL, NAME, &schema, trace(&ingest, &[])))
        .await;
    assert!(matches!(
        run.outcome,
        Err(EvalError::Judge(JudgeError::SchemaMismatch))
    ));
    assert_eq!(
        run.outcome.as_ref().err().map(EvalError::class),
        Some(ErrorClass::Permanent)
    );
    assert_eq!(run.trace.spans.len(), 2);
    assert_eq!(run.trace.spans[1].span_kind, SpanKind::Llm);
    assert!(run.usage.is_some());
}

#[tokio::test]
async fn complete_rejects_a_non_object_schema() {
    let mock = Mock::start().await;
    let conn = connection(Adapter::OpenAi, mock.url(), StructuredOutput::Native);
    let schema = json!({"type": "array", "items": {"type": "string"}});
    let ingest = ingest();
    let run = runner()
        .complete(input(&conn, MODEL, NAME, &schema, trace(&ingest, &[])))
        .await;
    assert!(matches!(
        run.outcome,
        Err(EvalError::Judge(JudgeError::InvalidSchema))
    ));
    assert!(mock.requests().is_empty());
    assert_eq!(run.trace.spans.len(), 1);
}

#[tokio::test]
async fn complete_rejects_a_bad_name() {
    let mock = Mock::start().await;
    let conn = connection(Adapter::OpenAi, mock.url(), StructuredOutput::Native);
    let schema = schema();
    let ingest = ingest();
    let long = "a".repeat(65);
    for name in ["has space", long.as_str()] {
        let run = runner()
            .complete(input(&conn, MODEL, name, &schema, trace(&ingest, &[])))
            .await;
        assert!(
            matches!(run.outcome, Err(EvalError::Judge(JudgeError::InvalidName))),
            "{name:?}"
        );
        assert_eq!(run.trace.spans.len(), 1);
    }
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn complete_maps_a_429_to_retryable() {
    let mock = Mock::start().await;
    mock.enqueue(MockReply {
        status: 429,
        body: json!({"error": {"type": "rate_limit_error", "message": "slow down"}}),
        delay: Duration::ZERO,
    });
    let conn = connection(Adapter::OpenAi, mock.url(), StructuredOutput::Native);
    let schema = schema();
    let ingest = ingest();
    let run = runner()
        .complete(input(&conn, MODEL, NAME, &schema, trace(&ingest, &[])))
        .await;
    assert_eq!(
        run.outcome.as_ref().err().map(EvalError::class),
        Some(ErrorClass::Retryable)
    );
    assert_eq!(run.trace.spans.len(), 2);
}

#[tokio::test]
async fn complete_maps_a_401_to_pause() {
    let mock = Mock::start().await;
    mock.enqueue(MockReply {
        status: 401,
        body: json!({"error": {"type": "authentication_error", "message": "bad key"}}),
        delay: Duration::ZERO,
    });
    let conn = connection(Adapter::OpenAi, mock.url(), StructuredOutput::Native);
    let schema = schema();
    let ingest = ingest();
    let run = runner()
        .complete(input(&conn, MODEL, NAME, &schema, trace(&ingest, &[])))
        .await;
    assert_eq!(
        run.outcome.as_ref().err().map(EvalError::class),
        Some(ErrorClass::Pause)
    );
}

#[tokio::test]
async fn complete_trace_is_priced() {
    let mock = Mock::start().await;
    mock.enqueue(openai_text(r#"{"summary":"x","is_issue":true}"#));
    let conn = connection(Adapter::OpenAi, mock.url(), StructuredOutput::Native);
    let schema = schema();
    let ingest = ingest();
    let run = runner()
        .complete(input(&conn, MODEL, NAME, &schema, trace(&ingest, &[])))
        .await;
    assert!(run.outcome.is_ok());
    assert!(run.cost_usd.is_some());
    assert!(!run.unpriced_model);
    assert!(run.usage.is_some_and(|usage| usage.input == 1000));
}

#[tokio::test]
async fn complete_trace_root_carries_name_and_metadata() {
    let mock = Mock::start().await;
    mock.enqueue(openai_text(r#"{"summary":"x","is_issue":true}"#));
    let conn = connection(Adapter::OpenAi, mock.url(), StructuredOutput::Native);
    let schema = schema();
    let ingest = ingest();
    let metadata = [("k", "v")];
    let run = runner()
        .complete(input(
            &conn,
            MODEL,
            NAME,
            &schema,
            trace(&ingest, &metadata),
        ))
        .await;
    assert_eq!(run.trace.rejections.total(), 0);
    let root = &run.trace.spans[0];
    assert_eq!(root.name, "cetra.issue.summarize");
    assert_eq!(root.span_kind, SpanKind::Chain);
    assert_eq!(root.environment.as_deref(), Some(environment::EVAL));
    assert_eq!(root.metadata.get("k").map(String::as_str), Some("v"));
}

#[tokio::test]
async fn complete_errors_never_contain_the_prompt() {
    let mock = Mock::start().await;
    mock.enqueue(openai_text(r#"{"summary":"SECRET-PAYLOAD"}"#));
    let conn = connection(Adapter::OpenAi, mock.url(), StructuredOutput::Native);
    let schema = schema();
    let ingest = ingest();
    let call = CompletionInput {
        prompt: "SECRET-PAYLOAD prompt",
        ..input(&conn, MODEL, NAME, &schema, trace(&ingest, &[]))
    };
    assert_no_payload(&format!("{call:?}"), "SECRET-PAYLOAD");
    let run = runner().complete(call).await;
    let error = run.outcome.as_ref().expect_err("error");
    assert_no_payload(&error.to_string(), "SECRET-PAYLOAD");
    assert_no_payload(&format!("{error:?}"), "SECRET-PAYLOAD");
    assert_no_payload(&format!("{run:?}"), "SECRET-PAYLOAD");
}
