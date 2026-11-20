//! Embeddings (`EvalRunner::embed`) against the in-process mock.
//! Nothing here calls a real provider.
#![allow(clippy::expect_used)]

use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cetra_core::enums::SpanKind;
use cetra_core::ids::{ProjectId, TraceId};
use cetra_otel::{IngestContext, IngestLimits, PriceTable, builtin_price_rows};
use serde_json::{Value, json};

use crate::error::{ErrorClass, EvalError, JudgeError, assert_no_payload};
use crate::mock::{Mock, MockReply};
use crate::{
    Adapter, CallTrace, EgressPolicy, EmbeddingInput, EvalRunner, JudgeConnection,
    MAX_EMBEDDING_DIMENSIONS, Secret, StructuredOutput,
};

const KEY: &str = "sk-test-SECRET-PAYLOAD";
const PROJECT: &str = "01900000-0000-7000-8000-000000000001";
const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const MODEL: &str = "text-embedding-3-small";
const GEMINI_MODEL: &str = "text-embedding-004";

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

fn connection(adapter: Adapter, base: reqwest::Url) -> JudgeConnection {
    JudgeConnection {
        adapter,
        base_url: Some(base),
        api_key: Secret::new(KEY.to_owned()),
        extra_headers: Vec::new(),
        structured_output: StructuredOutput::Native,
    }
}

fn trace<'a>(ingest: &'a IngestContext, metadata: &'a [(&'a str, &'a str)]) -> CallTrace<'a> {
    CallTrace {
        trace_id: TRACE.parse::<TraceId>().expect("trace id"),
        root_name: "cetra.issue.embed",
        metadata,
        ingest,
        prices: prices(),
    }
}

fn input<'a>(
    connection: &'a JudgeConnection,
    model: &'a str,
    text: &'a str,
    dimensions: Option<u32>,
    trace: CallTrace<'a>,
) -> EmbeddingInput<'a> {
    EmbeddingInput {
        connection,
        model,
        text,
        dimensions,
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

fn openai_vector(values: &[f64]) -> MockReply {
    reply(json!({
        "object": "list",
        "model": MODEL,
        "data": [{"object": "embedding", "index": 0, "embedding": values}],
        "usage": {"prompt_tokens": 2, "total_tokens": 2}
    }))
}

fn gemini_vector(values: &[f64], prompt_tokens: Option<u64>) -> MockReply {
    let mut body = json!({"embedding": {"values": values}});
    if let Some(tokens) = prompt_tokens {
        body["usageMetadata"] = json!({"promptTokenCount": tokens});
    }
    reply(body)
}

/// The built-in OpenAI price per million input tokens for `model`.
fn openai_price(model: &str) -> f64 {
    builtin_price_rows()
        .iter()
        .find(|row| row.model_name == model && row.provider.as_deref() == Some("openai"))
        .and_then(|row| row.price_input)
        .expect("built-in price for the model")
}

#[tokio::test]
async fn openai_embed_sends_model_input_and_dimensions() {
    let mock = Mock::start().await;
    mock.enqueue(openai_vector(&[3.0, 4.0]));
    let conn = connection(Adapter::OpenAi, mock.url());
    let ingest = ingest();
    let run = runner()
        .embed(input(&conn, MODEL, "hello", Some(512), trace(&ingest, &[])))
        .await;
    assert!(run.outcome.is_ok());
    let seen = mock.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/v1/embeddings");
    assert_eq!(
        seen[0].body,
        json!({
            "model": MODEL,
            "input": "hello",
            "encoding_format": "float",
            "dimensions": 512
        })
    );
    let bearer = format!("Bearer {KEY}");
    assert_eq!(seen[0].header("authorization"), Some(bearer.as_str()));
}

#[tokio::test]
async fn openai_embed_omits_dimensions_when_none() {
    let mock = Mock::start().await;
    mock.enqueue(openai_vector(&[3.0, 4.0]));
    let conn = connection(Adapter::OpenAi, mock.url());
    let ingest = ingest();
    let run = runner()
        .embed(input(&conn, MODEL, "hello", None, trace(&ingest, &[])))
        .await;
    assert!(run.outcome.is_ok());
    let seen = mock.requests();
    assert!(seen[0].body.get("dimensions").is_none());
    assert_eq!(seen[0].body["encoding_format"], "float");
}

#[tokio::test]
async fn openai_embed_returns_a_unit_vector() {
    let mock = Mock::start().await;
    mock.enqueue(openai_vector(&[3.0, 4.0]));
    let conn = connection(Adapter::OpenAi, mock.url());
    let ingest = ingest();
    let run = runner()
        .embed(input(&conn, MODEL, "hello", None, trace(&ingest, &[])))
        .await;
    let vector = run.outcome.as_ref().expect("vector");
    assert_eq!(vector.len(), 2);
    assert!((vector[0] - 0.6).abs() < 1e-6, "{vector:?}");
    assert!((vector[1] - 0.8).abs() < 1e-6, "{vector:?}");
    assert_eq!(run.input_tokens, Some(2));
}

#[tokio::test]
async fn gemini_embed_sends_output_dimensionality() {
    let mock = Mock::start().await;
    mock.enqueue(gemini_vector(&[3.0, 4.0], None));
    let conn = connection(Adapter::Gemini, mock.origin());
    let ingest = ingest();
    let run = runner()
        .embed(input(
            &conn,
            GEMINI_MODEL,
            "hello",
            Some(512),
            trace(&ingest, &[]),
        ))
        .await;
    assert!(run.outcome.is_ok());
    let seen = mock.requests();
    assert_eq!(
        seen[0].path,
        format!("/v1beta/models/{GEMINI_MODEL}:embedContent")
    );
    assert_eq!(seen[0].header("x-goog-api-key"), Some(KEY));
    assert!(seen[0].query.is_none());
    assert_eq!(
        seen[0].body,
        json!({
            "content": {"parts": [{"text": "hello"}]},
            "outputDimensionality": 512
        })
    );
}

#[tokio::test]
async fn gemini_embed_without_usage_has_no_tokens_and_no_cost() {
    let mock = Mock::start().await;
    mock.enqueue(gemini_vector(&[3.0, 4.0], None));
    let conn = connection(Adapter::Gemini, mock.origin());
    let ingest = ingest();
    let run = runner()
        .embed(input(
            &conn,
            GEMINI_MODEL,
            "hello",
            None,
            trace(&ingest, &[]),
        ))
        .await;
    assert!(run.outcome.is_ok());
    assert_eq!(run.input_tokens, None);
    assert_eq!(run.cost_usd, None);
    assert!(!run.unpriced_model);
}

#[tokio::test]
async fn anthropic_embed_is_unsupported() {
    let mock = Mock::start().await;
    let conn = connection(Adapter::Anthropic, mock.url());
    let ingest = ingest();
    let run = runner()
        .embed(input(
            &conn,
            "claude-embed",
            "hello",
            None,
            trace(&ingest, &[]),
        ))
        .await;
    assert!(mock.requests().is_empty());
    assert!(matches!(
        run.outcome,
        Err(EvalError::Judge(JudgeError::EmbeddingUnsupported))
    ));
}

#[tokio::test]
async fn embed_rejects_empty_nan_too_long_and_zero_vectors() {
    let vectors = [
        json!([]),
        // JSON has no NaN. 1e39 overflows f32 to infinity.
        json!([1e39, 0.5]),
        Value::Array(vec![json!(0.5); MAX_EMBEDDING_DIMENSIONS + 1]),
        json!([0.0, 0.0]),
    ];
    for vector in vectors {
        let mock = Mock::start().await;
        mock.enqueue(reply(json!({"data": [{"embedding": vector}]})));
        let conn = connection(Adapter::OpenAi, mock.url());
        let ingest = ingest();
        let run = runner()
            .embed(input(&conn, MODEL, "hello", None, trace(&ingest, &[])))
            .await;
        assert!(
            matches!(
                run.outcome,
                Err(EvalError::Judge(JudgeError::EmbeddingInvalid))
            ),
            "{:?}",
            run.outcome
        );
    }
}

#[tokio::test]
async fn embed_maps_errors_like_judges() {
    let cases: [(u16, Value, ErrorClass); 3] = [
        (
            429,
            json!({"error": {"type": "rate_limit_error", "code": "rate_limit_exceeded", "message": "SECRET-PAYLOAD"}}),
            ErrorClass::Retryable,
        ),
        (
            401,
            json!({"error": {"type": "invalid_request_error", "code": "invalid_api_key", "message": "SECRET-PAYLOAD"}}),
            ErrorClass::Pause,
        ),
        (
            429,
            json!({"error": {"type": "insufficient_quota", "code": "insufficient_quota", "message": "SECRET-PAYLOAD"}}),
            ErrorClass::Pause,
        ),
    ];
    for (status, body, class) in cases {
        let mock = Mock::start().await;
        mock.enqueue(MockReply {
            status,
            body,
            delay: Duration::ZERO,
        });
        let conn = connection(Adapter::OpenAi, mock.url());
        let ingest = ingest();
        let run = runner()
            .embed(input(&conn, MODEL, "hello", None, trace(&ingest, &[])))
            .await;
        let error = run.outcome.expect_err("provider error");
        assert_eq!(error.class(), class, "status {status}");
        assert_no_payload(&format!("{error:?}"), "SECRET-PAYLOAD");
        assert_no_payload(&error.to_string(), "SECRET-PAYLOAD");
    }
}

#[tokio::test]
async fn embed_trace_is_priced() {
    let price = openai_price(MODEL);
    let mock = Mock::start().await;
    mock.enqueue(reply(json!({
        "model": MODEL,
        "data": [{"embedding": [3.0, 4.0]}],
        "usage": {"prompt_tokens": 1000, "total_tokens": 1000}
    })));
    let conn = connection(Adapter::OpenAi, mock.url());
    let ingest = ingest();
    let run = runner()
        .embed(input(&conn, MODEL, "hello", None, trace(&ingest, &[])))
        .await;
    assert!(run.outcome.is_ok());
    let cost = run.cost_usd.expect("priced embedding");
    assert!((cost - 1000.0 * price / 1e6).abs() < 1e-12, "{cost}");
    assert!(!run.unpriced_model);
    let embedding = &run.trace.spans[1];
    assert_eq!(embedding.span_kind, SpanKind::Embedding);
    assert_eq!(embedding.input_tokens, Some(1000));
}

#[tokio::test]
async fn embed_trace_never_holds_the_text() {
    let text = "SECRET-PAYLOAD and the rest of the document";
    let mock = Mock::start().await;
    mock.enqueue(openai_vector(&[3.0, 4.0]));
    let conn = connection(Adapter::OpenAi, mock.url());
    let ingest = ingest();
    let run = runner()
        .embed(input(&conn, MODEL, text, None, trace(&ingest, &[])))
        .await;
    assert!(run.outcome.is_ok());
    let spans = serde_json::to_string(&run.trace.spans).expect("spans");
    assert_no_payload(&spans, "SECRET-PAYLOAD");
    assert_no_payload(&format!("{run:?}"), "SECRET-PAYLOAD");
    let chars = text.chars().count();
    assert_eq!(
        run.trace.spans[1].input.as_deref(),
        Some(format!(r#"{{"chars":{chars}}}"#).as_str())
    );
}
