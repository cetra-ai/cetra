//! Streamed and buffered calls against `cetra-capture`'s mock provider (D-I-43): the wire
//! shapes of the three APIs, complete tool calls, usage, and one judge call. Nothing calls
//! a provider.
#![allow(clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use cetra_capture::{Mock, Turn};
use cetra_core::message::{Message, Part, Role};
use cetra_core::score::ScoreValue;
use reqwest::Url;
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;

use crate::judge::{
    CompletionCall, JUDGEMENT_DESCRIPTION, JUDGEMENT_TOOL, answer_schema, call, parse_answer,
};
use crate::{
    Adapter, CHAT_IDLE_TIMEOUT, ChatDone, ChatEvent, ChatParams, ChatRequest, ChatStream,
    EgressClient, EgressPolicy, FinishReason, JudgeConnection, JudgeOutput, Secret,
    StructuredOutput, TokenUsage, ToolCall, ToolDefinition, stream_chat,
};

const KEY: &str = "sk-test-capture";

fn client() -> EgressClient {
    EgressClient::new(EgressPolicy {
        allow_private_network: true,
        restore_mode: false,
    })
    .expect("client")
}

/// The connection to the mock. OpenAI's base ends in `/v1`; the others' is the host.
fn connection(adapter: Adapter, addr: SocketAddr) -> JudgeConnection {
    let base = match adapter {
        Adapter::OpenAi => format!("http://{addr}/v1"),
        Adapter::Anthropic | Adapter::Gemini => format!("http://{addr}"),
    };
    JudgeConnection {
        adapter,
        base_url: Some(Url::parse(&base).expect("base")),
        api_key: Secret::new(KEY.to_owned()),
        extra_headers: Vec::new(),
        structured_output: StructuredOutput::Native,
    }
}

fn request(tools: Vec<ToolDefinition>) -> ChatRequest {
    ChatRequest {
        model: "m-1".to_owned(),
        messages: vec![Message {
            role: Role::User,
            name: None,
            parts: vec![Part::Text {
                content: "Hi".to_owned(),
                extra: Map::new(),
            }],
            finish_reason: None,
            extra: Map::new(),
        }],
        tools,
        tool_choice: None,
        response_format: None,
        params: ChatParams::default(),
        max_tokens: 64,
        timeout: Duration::from_secs(10),
        idle_timeout: CHAT_IDLE_TIMEOUT,
    }
}

fn weather() -> ToolDefinition {
    ToolDefinition {
        name: "get_weather".to_owned(),
        description: Some("Weather of a city.".to_owned()),
        parameters: json!({"type": "object"}),
    }
}

fn turn(value: Value) -> Turn {
    serde_json::from_value(value).expect("turn")
}

/// Starts the mock on a free loopback port. The server runs until the test ends.
async fn start(turns: Vec<Turn>) -> (Arc<Mock>, SocketAddr) {
    let mock = Mock::new(turns);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("address");
    let router = mock.router();
    tokio::spawn(async move { axum::serve(listener, router).await.expect("serve") });
    (mock, addr)
}

/// The mock served one turn, as a stream or as a buffered body, and no error was logged.
fn assert_served(mock: &Mock, streamed: bool) {
    let log = mock.finish();
    assert!(log.errors.is_empty(), "{:?}", log.errors);
    assert_eq!(log.served.len(), 1, "{:?}", log.served);
    assert_eq!(log.served[0].stream, streamed);
}

async fn collect(stream: &mut ChatStream) -> Vec<ChatEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await.expect("event") {
        events.push(event);
    }
    events
}

async fn text_turn_streams(adapter: Adapter, api: &str) {
    let (mock, addr) = start(vec![turn(json!({
        "api": api,
        "model": "m-1",
        "message": {"text": "Hello there, world."},
        "finish": "stop",
        "usage": {"input": 10, "output": 3},
        "chunks": 3,
        "ttft_ms": 0,
        "chunk_delay_ms": 0
    }))])
    .await;
    let mut stream = stream_chat(&client(), &connection(adapter, addr), &request(Vec::new()))
        .await
        .expect("stream");
    let events = collect(&mut stream).await;
    let texts: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            ChatEvent::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(texts.len() >= 2, "{texts:?}");
    assert_eq!(texts.concat(), "Hello there, world.");
    assert_eq!(
        events.last(),
        Some(&ChatEvent::Done(ChatDone {
            finish_reason: Some(FinishReason::Stop),
            usage: Some(TokenUsage {
                input: 10,
                output: 3,
                ..TokenUsage::default()
            }),
            response_model: Some("m-1".to_owned()),
        }))
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ChatEvent::Done(_)))
            .count(),
        1
    );
    assert_served(&mock, true);
}

async fn tool_turn_streams_a_complete_call(adapter: Adapter, api: &str, finish: FinishReason) {
    let (mock, addr) = start(vec![turn(json!({
        "api": api,
        "model": "m-1",
        "message": {"tool_calls": [
            {"id": "call_a", "name": "get_weather", "arguments": {"city": "Paris"}}
        ]},
        "finish": "tool_calls",
        "usage": {"input": 10, "output": 3},
        "ttft_ms": 0,
        "chunk_delay_ms": 0
    }))])
    .await;
    let mut stream = stream_chat(
        &client(),
        &connection(adapter, addr),
        &request(vec![weather()]),
    )
    .await
    .expect("stream");
    let events = collect(&mut stream).await;
    assert_eq!(
        events.first(),
        Some(&ChatEvent::ToolCall(ToolCall {
            id: "call_a".to_owned(),
            name: "get_weather".to_owned(),
            arguments: json!({"city": "Paris"}),
        }))
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, ChatEvent::ToolCall(_)))
            .count(),
        1
    );
    assert_eq!(
        events.last(),
        Some(&ChatEvent::Done(ChatDone {
            finish_reason: Some(finish),
            usage: Some(TokenUsage {
                input: 10,
                output: 3,
                ..TokenUsage::default()
            }),
            response_model: Some("m-1".to_owned()),
        }))
    );
    assert_served(&mock, true);
}

#[tokio::test]
async fn openai_text_turn_streams_through_the_capture_mock() {
    text_turn_streams(Adapter::OpenAi, "openai.chat").await;
}

#[tokio::test]
async fn anthropic_text_turn_streams_through_the_capture_mock() {
    text_turn_streams(Adapter::Anthropic, "anthropic.messages").await;
}

#[tokio::test]
async fn gemini_text_turn_streams_through_the_capture_mock() {
    text_turn_streams(Adapter::Gemini, "gemini.generate_content").await;
}

#[tokio::test]
async fn openai_tool_turn_streams_a_complete_call() {
    tool_turn_streams_a_complete_call(Adapter::OpenAi, "openai.chat", FinishReason::ToolCall).await;
}

#[tokio::test]
async fn anthropic_tool_turn_streams_a_complete_call() {
    tool_turn_streams_a_complete_call(
        Adapter::Anthropic,
        "anthropic.messages",
        FinishReason::ToolCall,
    )
    .await;
}

/// Gemini reports a function call with `STOP`.
#[tokio::test]
async fn gemini_tool_turn_streams_a_complete_call() {
    tool_turn_streams_a_complete_call(
        Adapter::Gemini,
        "gemini.generate_content",
        FinishReason::Stop,
    )
    .await;
}

#[tokio::test]
async fn the_capture_mock_still_serves_buffered_judges() {
    let (mock, addr) = start(vec![turn(json!({
        "api": "openai.chat",
        "model": "gpt-test",
        "message": {"text": "{\"explanation\": \"Relevant.\", \"value\": 0.9}"},
        "finish": "stop",
        "usage": {"input": 10, "output": 3},
        "ttft_ms": 0
    }))])
    .await;
    let output = JudgeOutput::Numeric { min: 0.0, max: 1.0 };
    let schema = answer_schema(&output, true);
    let judge = CompletionCall {
        model: "gpt-test",
        prompt: "Grade the answer.",
        schema: &schema,
        schema_name: "judge_answer",
        tool_name: JUDGEMENT_TOOL,
        tool_description: JUDGEMENT_DESCRIPTION,
        temperature: None,
        max_output_tokens: 64,
        timeout: Duration::from_secs(10),
    };
    let reply = call(&client(), &connection(Adapter::OpenAi, addr), &judge)
        .await
        .expect("judge call");
    assert_eq!(reply.response_model.as_deref(), Some("gpt-test"));
    assert_eq!(
        reply.usage,
        TokenUsage {
            input: 10,
            output: 3,
            ..TokenUsage::default()
        }
    );
    let answer = parse_answer(reply.answer, &output, true).expect("answer");
    assert_eq!(answer.value, ScoreValue::Numeric(0.9));
    assert_served(&mock, false);
}
