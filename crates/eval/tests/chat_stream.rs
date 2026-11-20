//! Streamed calls against the scripted server: the three providers' event shapes, the
//! caps, the timeouts, and the errors (D-V-19, D-V-24, D-V-30). Nothing calls a provider.
#![allow(clippy::expect_used)]

use std::time::Duration;

use cetra_core::message::{Message, Part, Role};
use serde_json::{Map, json};

use crate::stream_server::{Step, StreamReply, StreamServer, ok_chunks, sse, sse_named};
use crate::{
    Adapter, CHAT_IDLE_TIMEOUT, ChatDone, ChatError, ChatEvent, ChatParams, ChatRequest,
    ChatStream, EgressClient, EgressError, EgressPolicy, ErrorClass, FinishReason, JudgeConnection,
    ResponseFormat, Secret, StructuredOutput, TokenUsage, ToolCall, stream_chat,
};

const KEY: &str = "sk-test-SECRET-PAYLOAD";
const PAYLOAD: &str = "SECRET-PAYLOAD";

fn client() -> EgressClient {
    EgressClient::new(EgressPolicy {
        allow_private_network: true,
        restore_mode: false,
    })
    .expect("client")
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

fn request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.to_owned(),
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
        tools: Vec::new(),
        tool_choice: None,
        response_format: None,
        params: ChatParams::default(),
        max_tokens: 64,
        timeout: Duration::from_secs(10),
        idle_timeout: CHAT_IDLE_TIMEOUT,
    }
}

fn text(value: &str) -> ChatEvent {
    ChatEvent::Text(value.to_owned())
}

fn done(
    finish_reason: Option<FinishReason>,
    usage: Option<TokenUsage>,
    response_model: Option<&str>,
) -> ChatEvent {
    ChatEvent::Done(ChatDone {
        finish_reason,
        usage,
        response_model: response_model.map(str::to_owned),
    })
}

fn openai_content(content: &str) -> Vec<u8> {
    sse(&json!({"choices": [{"index": 0, "delta": {"content": content}}]}).to_string())
}

async fn collect(stream: &mut ChatStream) -> Result<Vec<ChatEvent>, ChatError> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await? {
        events.push(event);
    }
    Ok(events)
}

/// The error the stream ends with. Panics when it ends without one.
async fn next_error(stream: &mut ChatStream) -> ChatError {
    loop {
        match stream.next().await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("the stream ended without an error"),
            Err(error) => return error,
        }
    }
}

#[tokio::test]
async fn openai_streams_text_tool_calls_and_usage() {
    let server = StreamServer::start().await;
    server.enqueue(ok_chunks(vec![
        sse(r#"{"model":"gpt-x","choices":[{"index":0,"delta":{"role":"assistant","content":""}}],"usage":null}"#),
        sse(r#"{"choices":[{"index":0,"delta":{"content":"Hel"}}],"usage":null}"#),
        sse(r#"{"choices":[{"index":0,"delta":{"content":"lo"}}]}"#),
        sse(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_9","type":"function","function":{"name":"get_weather","arguments":"{\"ci"}}]}}]}"#),
        sse(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ty\":\"Paris\",\"note\":\"SECRET-PAYLOAD\"}"}}]}}]}"#),
        sse(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#),
        sse(r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3}}"#),
        sse("[DONE]"),
    ]));
    let conn = connection(Adapter::OpenAi, server.base_v1());
    let mut stream = stream_chat(&client(), &conn, &request("gpt-x"))
        .await
        .expect("stream");
    let events = collect(&mut stream).await.expect("events");
    assert_eq!(
        events,
        vec![
            text("Hel"),
            text("lo"),
            ChatEvent::ToolCall(ToolCall {
                id: "call_9".to_owned(),
                name: "get_weather".to_owned(),
                arguments: json!({"city": "Paris", "note": PAYLOAD}),
            }),
            done(
                Some(FinishReason::ToolCall),
                Some(TokenUsage {
                    input: 10,
                    output: 3,
                    ..TokenUsage::default()
                }),
                Some("gpt-x"),
            ),
        ]
    );
    assert_eq!(stream.next().await.expect("after done"), None);
    assert!(!format!("{events:?}").contains(PAYLOAD));

    let seen = server.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/v1/chat/completions");
    assert_eq!(seen[0].body["stream"], json!(true));
    assert_eq!(
        seen[0].body["stream_options"],
        json!({"include_usage": true})
    );
}

#[tokio::test]
async fn openai_refusal_is_text_and_a_refusal_finish() {
    let server = StreamServer::start().await;
    server.enqueue(ok_chunks(vec![
        sse(r#"{"choices":[{"index":0,"delta":{"refusal":"No."}}]}"#),
        sse(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#),
        sse("[DONE]"),
    ]));
    let conn = connection(Adapter::OpenAi, server.base_v1());
    let mut stream = stream_chat(&client(), &conn, &request("gpt-x"))
        .await
        .expect("stream");
    assert_eq!(
        collect(&mut stream).await.expect("events"),
        vec![text("No."), done(Some(FinishReason::Refusal), None, None)]
    );
}

#[tokio::test]
async fn anthropic_streams_text_tool_use_and_merged_usage() {
    let server = StreamServer::start().await;
    server.enqueue(ok_chunks(vec![
        sse_named("message_start", r#"{"type":"message_start","message":{"model":"claude-x","usage":{"input_tokens":7,"cache_read_input_tokens":3,"output_tokens":1}}}"#),
        sse_named("ping", r#"{"type":"ping"}"#),
        sse_named("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#),
        sse_named("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#),
        sse_named("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#),
        sse_named("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        sse_named("content_block_start", r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"search","input":{}}}"#),
        sse_named("content_block_delta", r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}}"#),
        sse_named("content_block_delta", r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"x\"}"}}"#),
        sse_named("content_block_stop", r#"{"type":"content_block_stop","index":1}"#),
        sse_named("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":12}}"#),
        sse_named("message_stop", r#"{"type":"message_stop"}"#),
    ]));
    let conn = connection(Adapter::Anthropic, server.origin());
    let mut stream = stream_chat(&client(), &conn, &request("claude-x"))
        .await
        .expect("stream");
    assert_eq!(
        collect(&mut stream).await.expect("events"),
        vec![
            text("Hel"),
            text("lo"),
            ChatEvent::ToolCall(ToolCall {
                id: "toolu_1".to_owned(),
                name: "search".to_owned(),
                arguments: json!({"q": "x"}),
            }),
            done(
                Some(FinishReason::ToolCall),
                Some(TokenUsage {
                    input: 10,
                    output: 12,
                    cache_read: 3,
                    ..TokenUsage::default()
                }),
                Some("claude-x"),
            ),
        ]
    );
    let seen = server.requests();
    assert_eq!(seen[0].path, "/v1/messages");
    assert_eq!(seen[0].body["stream"], json!(true));
}

#[tokio::test]
async fn anthropic_error_event_is_a_classified_provider_error() {
    let server = StreamServer::start().await;
    server.enqueue(ok_chunks(vec![
        sse_named(
            "message_start",
            r#"{"type":"message_start","message":{"model":"claude-x"}}"#,
        ),
        sse_named(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"SECRET-PAYLOAD"}}"#,
        ),
    ]));
    let conn = connection(Adapter::Anthropic, server.origin());
    let mut stream = stream_chat(&client(), &conn, &request("claude-x"))
        .await
        .expect("stream");
    let error = next_error(&mut stream).await;
    assert!(
        matches!(
            error,
            ChatError::Provider {
                status: 529,
                class: ErrorClass::Retryable,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(error.to_string(), "provider status 529");
    assert!(!format!("{error:?}").contains(PAYLOAD));
    assert_eq!(stream.next().await.expect("after the error"), None);
}

#[tokio::test]
async fn gemini_streams_text_function_calls_and_usage() {
    let server = StreamServer::start().await;
    server.enqueue(ok_chunks(vec![
        sse(r#"{"candidates":[{"content":{"parts":[{"text":"private thought","thought":true}]}}]}"#),
        sse(r#"{"candidates":[{"content":{"parts":[{"text":"Hel"}]}}]}"#),
        sse(r#"{"candidates":[{"content":{"parts":[{"text":"lo"},{"functionCall":{"name":"lookup","args":{"id":1}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"thoughtsTokenCount":1},"modelVersion":"gemini-x"}"#),
    ]));
    let conn = connection(Adapter::Gemini, server.origin());
    let mut stream = stream_chat(&client(), &conn, &request("gemini-x"))
        .await
        .expect("stream");
    assert_eq!(
        collect(&mut stream).await.expect("events"),
        vec![
            text("Hel"),
            text("lo"),
            ChatEvent::ToolCall(ToolCall {
                id: "call_0".to_owned(),
                name: "lookup".to_owned(),
                arguments: json!({"id": 1}),
            }),
            done(
                Some(FinishReason::Stop),
                Some(TokenUsage {
                    input: 5,
                    output: 3,
                    reasoning: 1,
                    ..TokenUsage::default()
                }),
                Some("gemini-x"),
            ),
        ]
    );
    let seen = server.requests();
    assert_eq!(
        seen[0].path,
        "/v1beta/models/gemini-x:streamGenerateContent"
    );
    assert_eq!(seen[0].query.as_deref(), Some("alt=sse"));
    assert!(
        seen[0]
            .headers
            .iter()
            .any(|(name, value)| name == "x-goog-api-key" && value == KEY)
    );
    assert!(!seen[0].query.as_deref().unwrap_or_default().contains(KEY));
}

#[tokio::test]
async fn gemini_block_is_a_content_filter_finish() {
    let server = StreamServer::start().await;
    server.enqueue(ok_chunks(vec![sse(
        r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#,
    )]));
    let conn = connection(Adapter::Gemini, server.origin());
    let mut stream = stream_chat(&client(), &conn, &request("gemini-x"))
        .await
        .expect("stream");
    assert_eq!(
        collect(&mut stream).await.expect("events"),
        vec![done(Some(FinishReason::ContentFilter), None, None)]
    );
}

#[tokio::test]
async fn a_non_200_answer_is_a_provider_error() {
    let server = StreamServer::start().await;
    let body =
        br#"{"type":"error","error":{"type":"rate_limit_error","message":"SECRET-PAYLOAD"}}"#;
    server.enqueue(StreamReply {
        status: 429,
        head_delay: Duration::ZERO,
        steps: vec![Step::Send(body.to_vec())],
    });
    let conn = connection(Adapter::Anthropic, server.origin());
    let err = stream_chat(&client(), &conn, &request("claude-x"))
        .await
        .expect_err("429");
    assert!(
        matches!(
            err,
            ChatError::Provider {
                status: 429,
                class: ErrorClass::Retryable,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(!format!("{err:?}").contains(PAYLOAD));
    assert!(!err.to_string().contains(PAYLOAD));
}

#[tokio::test]
async fn a_400_has_the_structured_output_hint_only_with_a_response_format() {
    let server = StreamServer::start().await;
    let body = br#"{"error":{"type":"invalid_request_error","message":"bad schema"}}"#;
    for _ in 0..2 {
        server.enqueue(StreamReply {
            status: 400,
            head_delay: Duration::ZERO,
            steps: vec![Step::Send(body.to_vec())],
        });
    }
    let conn = connection(Adapter::OpenAi, server.base_v1());
    let mut with_format = request("gpt-x");
    with_format.response_format = Some(ResponseFormat {
        name: "judge".to_owned(),
        schema: json!({"type": "object"}),
        strict: true,
        tool_name: "submit".to_owned(),
        tool_description: "Submit the answer.".to_owned(),
    });
    let err = stream_chat(&client(), &conn, &with_format)
        .await
        .expect_err("400");
    assert!(matches!(
        err,
        ChatError::Provider {
            status: 400,
            hint: Some(_),
            ..
        }
    ));

    let err = stream_chat(&client(), &conn, &request("gpt-x"))
        .await
        .expect_err("400");
    assert!(matches!(
        err,
        ChatError::Provider {
            status: 400,
            hint: None,
            ..
        }
    ));
}

#[tokio::test]
async fn an_idle_stream_fails_with_a_timeout() {
    let server = StreamServer::start().await;
    server.enqueue(StreamReply {
        status: 200,
        head_delay: Duration::ZERO,
        steps: vec![
            Step::Send(openai_content("Hel")),
            Step::Sleep(Duration::from_millis(600)),
        ],
    });
    let conn = connection(Adapter::OpenAi, server.base_v1());
    let mut idle = request("gpt-x");
    idle.idle_timeout = Duration::from_millis(150);
    let mut stream = stream_chat(&client(), &conn, &idle).await.expect("stream");
    assert_eq!(stream.next().await.expect("first"), Some(text("Hel")));
    assert_eq!(
        next_error(&mut stream).await,
        ChatError::Transport(EgressError::Timeout)
    );
    assert_eq!(stream.next().await.expect("after the error"), None);
}

#[tokio::test]
async fn an_answer_over_4_mib_fails() {
    let server = StreamServer::start().await;
    let big = "x".repeat(1024 * 1024);
    server.enqueue(ok_chunks((0..5).map(|_| openai_content(&big)).collect()));
    let conn = connection(Adapter::OpenAi, server.base_v1());
    let mut stream = stream_chat(&client(), &conn, &request("gpt-x"))
        .await
        .expect("stream");
    let mut texts = 0;
    let error = loop {
        match stream.next().await {
            Ok(Some(ChatEvent::Text(_))) => texts += 1,
            Ok(Some(other)) => panic!("unexpected event {other:?}"),
            Ok(None) => panic!("the stream ended without an error"),
            Err(error) => break error,
        }
    };
    assert_eq!(texts, 4);
    assert_eq!(
        error,
        ChatError::Transport(EgressError::TooLarge {
            limit: 4 * 1024 * 1024
        })
    );
}

#[tokio::test]
async fn invalid_json_is_a_bad_stream() {
    let server = StreamServer::start().await;
    server.enqueue(ok_chunks(vec![sse("{nope")]));
    let conn = connection(Adapter::OpenAi, server.base_v1());
    let mut stream = stream_chat(&client(), &conn, &request("gpt-x"))
        .await
        .expect("stream");
    assert_eq!(next_error(&mut stream).await, ChatError::BadStream);
}

#[tokio::test]
async fn a_stream_cut_before_its_end_is_a_bad_stream() {
    let server = StreamServer::start().await;
    server.enqueue(ok_chunks(vec![openai_content("Hel")]));
    server.enqueue(ok_chunks(vec![
        sse_named("message_start", r#"{"type":"message_start","message":{"model":"claude-x"}}"#),
        sse_named("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#),
    ]));
    server.enqueue(ok_chunks(vec![sse(
        r#"{"candidates":[{"content":{"parts":[{"text":"Hel"}]}}]}"#,
    )]));

    let openai = connection(Adapter::OpenAi, server.base_v1());
    let mut stream = stream_chat(&client(), &openai, &request("gpt-x"))
        .await
        .expect("openai stream");
    assert_eq!(stream.next().await.expect("text"), Some(text("Hel")));
    assert_eq!(next_error(&mut stream).await, ChatError::BadStream);

    let anthropic = connection(Adapter::Anthropic, server.origin());
    let mut stream = stream_chat(&client(), &anthropic, &request("claude-x"))
        .await
        .expect("anthropic stream");
    assert_eq!(stream.next().await.expect("text"), Some(text("Hel")));
    assert_eq!(next_error(&mut stream).await, ChatError::BadStream);

    let gemini = connection(Adapter::Gemini, server.origin());
    let mut stream = stream_chat(&client(), &gemini, &request("gemini-x"))
        .await
        .expect("gemini stream");
    assert_eq!(stream.next().await.expect("text"), Some(text("Hel")));
    assert_eq!(next_error(&mut stream).await, ChatError::BadStream);
}

#[tokio::test]
async fn events_before_an_error_are_delivered() {
    let server = StreamServer::start().await;
    let mut chunk = sse_named(
        "content_block_delta",
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
    );
    chunk.extend(sse_named(
        "error",
        r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#,
    ));
    server.enqueue(ok_chunks(vec![chunk]));
    let conn = connection(Adapter::Anthropic, server.origin());
    let mut stream = stream_chat(&client(), &conn, &request("claude-x"))
        .await
        .expect("stream");
    assert_eq!(stream.next().await.expect("text"), Some(text("Hel")));
    assert!(matches!(
        next_error(&mut stream).await,
        ChatError::Provider { status: 529, .. }
    ));
}

#[tokio::test]
async fn request_errors_come_before_sending() {
    let server = StreamServer::start().await;
    let conn = connection(Adapter::OpenAi, server.base_v1());

    let mut bad_model = request("a/b");
    let err = stream_chat(&client(), &conn, &bad_model)
        .await
        .expect_err("model");
    assert_eq!(err, ChatError::InvalidModel);

    bad_model = request("gpt-x");
    bad_model.max_tokens = 0;
    let err = stream_chat(&client(), &conn, &bad_model)
        .await
        .expect_err("max_tokens");
    assert_eq!(err, ChatError::InvalidRequest);

    let mut media = request("gpt-x");
    media.messages[0].parts = vec![Part::Uri {
        modality: None,
        mime_type: None,
        uri: "https://example.com/a.png".to_owned(),
        extra: Map::new(),
    }];
    let err = stream_chat(&client(), &conn, &media)
        .await
        .expect_err("media");
    assert_eq!(
        err,
        ChatError::UnsupportedMessage {
            index: 0,
            reason: "media part",
        }
    );
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn events_before_an_sse_error_are_delivered() {
    let server = StreamServer::start().await;
    let mut chunk = openai_content("Hi");
    chunk.extend_from_slice(b"data: \xff\n\n");
    server.enqueue(ok_chunks(vec![chunk]));
    let conn = connection(Adapter::OpenAi, server.base_v1());
    let mut stream = stream_chat(&client(), &conn, &request("gpt-x"))
        .await
        .expect("stream");
    assert_eq!(stream.next().await.expect("text"), Some(text("Hi")));
    assert_eq!(next_error(&mut stream).await, ChatError::BadStream);
}

#[tokio::test]
async fn empty_data_events_are_ignored() {
    let server = StreamServer::start().await;
    server.enqueue(ok_chunks(vec![
        openai_content("Hel"),
        sse(""),
        openai_content("lo"),
        sse("[DONE]"),
    ]));
    let conn = connection(Adapter::OpenAi, server.base_v1());
    let mut stream = stream_chat(&client(), &conn, &request("gpt-x"))
        .await
        .expect("stream");
    let events = collect(&mut stream).await.expect("events");
    assert_eq!(
        events,
        vec![text("Hel"), text("lo"), done(None, None, None)]
    );
}
