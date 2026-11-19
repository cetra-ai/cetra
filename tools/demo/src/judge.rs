//! The judge endpoint of the generator (DO6): an OpenAI-compatible
//! `POST /v1/chat/completions` that answers in the shape the request's JSON schema
//! asks for. It is not a language model. It draws plausible answers, so the demo's
//! evaluators and online rules run their real judge path (D187). Request bodies are
//! never logged.

use std::net::SocketAddr;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use rand::RngExt;
use rand::rngs::StdRng;
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;

use crate::error::DemoError;

/// The `model` of an answer when the request names none.
const MODEL_NAME: &str = "cetra-demo-judge";
/// Canned explanations, one of which answers a schema that has an `explanation` field.
const EXPLANATIONS: [&str; 20] = [
    "The answer addresses the question that was asked.",
    "The answer is supported by the context it was given.",
    "The answer leaves out part of what the question asked for.",
    "The answer repeats the question without answering it.",
    "The answer is correct but terse.",
    "The answer is polite and on topic.",
    "The answer contradicts the provided context.",
    "The answer states a fact that the context does not contain.",
    "The answer is clear and well organised.",
    "The answer uses more words than it needs.",
    "The output is a close match to the expected answer.",
    "The output differs from the expected answer in its main claim.",
    "The response follows the requested format.",
    "The response ignores part of the requested format.",
    "Nothing in the answer is harmful or offensive.",
    "The tone is neutral and factual.",
    "The answer hedges more than the evidence warrants.",
    "The answer names a source that is not in the context.",
    "The answer is complete for a short question.",
    "The answer needs a follow-up question to be useful.",
];
/// Share of boolean answers that are `true`.
const TRUE_SHARE: f64 = 0.8;
/// Share of categorical answers that take the schema's first label; the rest take another label.
const FIRST_LABEL_SHARE: f64 = 0.7;
/// Exponent applied to a uniform draw: `u^0.25` lies in [0, 1) and has a mean of 0.8.
const NUMBER_EXPONENT: f64 = 0.25;
/// Rough characters per token, for the `usage` counts.
const CHARS_PER_TOKEN: usize = 4;

#[derive(Clone)]
struct Judge {
    key: String,
}

/// Binds the judge endpoint. Separate from [`serve`] so a bad address fails at startup.
pub(crate) async fn bind(addr: SocketAddr) -> Result<TcpListener, DemoError> {
    TcpListener::bind(addr).await.map_err(DemoError::Judge)
}

/// Serves the judge until the process ends. Requests must carry `Authorization: Bearer <key>`.
pub(crate) async fn serve(listener: TcpListener, key: String) -> Result<(), DemoError> {
    let app = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(Judge { key });
    axum::serve(listener, app).await.map_err(DemoError::Judge)
}

async fn chat_completions(State(judge): State<Judge>, headers: HeaderMap, body: Bytes) -> Response {
    if !authorized(&headers, &judge.key) {
        return refuse(
            StatusCode::UNAUTHORIZED,
            "the bearer key is missing or wrong",
        );
    }
    let Ok(request) = serde_json::from_slice::<Value>(&body) else {
        return refuse(StatusCode::BAD_REQUEST, "the body is not JSON");
    };
    let Some(schema) = request.pointer("/response_format/json_schema/schema") else {
        return refuse(
            StatusCode::BAD_REQUEST,
            "the request has no response_format.json_schema.schema",
        );
    };
    let mut rng: StdRng = rand::make_rng();
    let Some(answer) = answer(schema, &mut rng) else {
        return refuse(
            StatusCode::BAD_REQUEST,
            "the schema is not a judge schema (explanation, value or label)",
        );
    };
    let content = answer.to_string();
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(MODEL_NAME);
    let prompt_tokens = body.len() / CHARS_PER_TOKEN;
    let completion_tokens = content.len().div_ceil(CHARS_PER_TOKEN);
    Json(json!({
        "id": "chatcmpl-cetra-demo",
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens
        }
    }))
    .into_response()
}

fn authorized(headers: &HeaderMap, key: &str) -> bool {
    let expected = format!("Bearer {key}");
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|given| same_bytes(given.as_bytes(), expected.as_bytes()))
}

/// Compares without stopping at the first difference.
fn same_bytes(given: &[u8], expected: &[u8]) -> bool {
    given.len() == expected.len()
        && given
            .iter()
            .zip(expected)
            .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

fn refuse(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": {"message": message}}))).into_response()
}

/// An answer that matches `schema`: every property, nothing else. `None` when the schema
/// has a property the judge does not know, or no `value` or `label`.
fn answer(schema: &Value, rng: &mut StdRng) -> Option<Value> {
    let properties = schema.get("properties")?.as_object()?;
    if properties
        .keys()
        .any(|name| !matches!(name.as_str(), "explanation" | "value" | "label"))
    {
        return None;
    }
    let mut out = Map::new();
    if properties.contains_key("explanation") {
        let text = EXPLANATIONS[rng.random_range(0..EXPLANATIONS.len())];
        out.insert("explanation".to_owned(), json!(text));
    }
    if let Some(value) = properties.get("value") {
        out.insert("value".to_owned(), value_answer(value, rng)?);
    } else {
        out.insert(
            "label".to_owned(),
            label_answer(properties.get("label")?, rng)?,
        );
    }
    Some(Value::Object(out))
}

fn value_answer(value_schema: &Value, rng: &mut StdRng) -> Option<Value> {
    match value_schema.get("type")?.as_str()? {
        "boolean" => Some(json!(rng.random_bool(TRUE_SHARE))),
        "number" => {
            let draw: f64 = rng.random_range(0.0..1.0);
            Some(json!(draw.powf(NUMBER_EXPONENT)))
        }
        _ => None,
    }
}

fn label_answer(label_schema: &Value, rng: &mut StdRng) -> Option<Value> {
    let labels = label_schema.get("enum")?.as_array()?;
    if labels.is_empty() {
        return None;
    }
    if labels.len() == 1 || rng.random_bool(FIRST_LABEL_SHARE) {
        return Some(labels[0].clone());
    }
    Some(labels[rng.random_range(1..labels.len())].clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::Client;

    const KEY: &str = "judge-test-key";

    /// Starts the judge on a free loopback port and returns its chat completions URL.
    async fn start() -> String {
        let listener = bind("127.0.0.1:0".parse().expect("address"))
            .await
            .expect("bind a loopback port");
        let addr = listener.local_addr().expect("local address");
        tokio::spawn(serve(listener, KEY.to_owned()));
        format!("http://{addr}/v1/chat/completions")
    }

    fn numeric(explanation: bool) -> Value {
        let mut properties = json!({"value": {"type": "number", "description": "0 to 1"}});
        let mut required = vec!["value"];
        if explanation {
            properties["explanation"] = json!({"type": "string"});
            required.insert(0, "explanation");
        }
        json!({"type": "object", "properties": properties,
               "required": required, "additionalProperties": false})
    }

    fn boolean(explanation: bool) -> Value {
        let mut schema = numeric(explanation);
        schema["properties"]["value"] = json!({"type": "boolean"});
        schema
    }

    fn categorical(explanation: bool) -> Value {
        let mut schema = numeric(explanation);
        schema["properties"]["value"] = json!({"type": "string"});
        schema["properties"]
            .as_object_mut()
            .expect("object")
            .remove("value");
        schema["properties"]["label"] = json!({
            "type": "string",
            "enum": ["correct", "partially_correct", "incorrect"]
        });
        schema["required"] = json!(if explanation {
            vec!["explanation", "label"]
        } else {
            vec!["label"]
        });
        schema
    }

    /// Checks an answer against the schema's `type`, `required` and `enum`: every
    /// property is required, present, of its type, and no field is extra.
    fn assert_matches(schema: &Value, answer: &Value) {
        let properties = schema["properties"].as_object().expect("properties");
        let object = answer.as_object().expect("the answer is an object");
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required")
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(object.len(), properties.len(), "no field beyond the schema");
        for (name, property) in properties {
            assert!(required.contains(&name.as_str()), "{name} is required");
            let value = object.get(name).expect("the answer has every property");
            match property["type"].as_str().expect("a type") {
                "boolean" => assert!(value.is_boolean(), "{name} is boolean"),
                "number" => {
                    let number = value.as_f64().expect("a number");
                    assert!((0.0..=1.0).contains(&number), "{name} is in [0, 1]");
                }
                "string" => {
                    assert!(value.is_string(), "{name} is a string");
                    if let Some(options) = property["enum"].as_array() {
                        assert!(options.contains(value), "{name} is one of the labels");
                    }
                }
                other => panic!("unexpected type {other}"),
            }
        }
    }

    async fn ask(url: &str, schema: Option<&Value>, key: Option<&str>) -> reqwest::Response {
        let mut body = json!({"model": "judge", "messages": [{"role": "user", "content": "x"}]});
        if let Some(schema) = schema {
            body["response_format"] = json!({
                "type": "json_schema",
                "json_schema": {"name": "judge", "strict": true, "schema": schema}
            });
        }
        let mut request = Client::new().post(url).json(&body);
        if let Some(key) = key {
            request = request.bearer_auth(key);
        }
        request.send().await.expect("the judge answers")
    }

    async fn answer_to(url: &str, schema: &Value) -> Value {
        let response = ask(url, Some(schema), Some(KEY)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.expect("JSON");
        assert_eq!(body["object"], "chat.completion");
        assert!(body["usage"]["total_tokens"].as_u64().is_some());
        let content = body["choices"][0]["message"]["content"]
            .as_str()
            .expect("content is text");
        serde_json::from_str(content).expect("the content is the JSON answer")
    }

    #[tokio::test]
    async fn numeric_answers_match_the_schema_with_and_without_explanation() {
        let url = start().await;
        for explanation in [true, false] {
            let schema = numeric(explanation);
            for _ in 0..20 {
                let answer = answer_to(&url, &schema).await;
                assert_matches(&schema, &answer);
                assert_eq!(answer.get("explanation").is_some(), explanation);
            }
        }
    }

    #[tokio::test]
    async fn boolean_answers_match_the_schema_with_and_without_explanation() {
        let url = start().await;
        for explanation in [true, false] {
            let schema = boolean(explanation);
            for _ in 0..20 {
                let answer = answer_to(&url, &schema).await;
                assert_matches(&schema, &answer);
                assert_eq!(answer.get("explanation").is_some(), explanation);
            }
        }
    }

    #[tokio::test]
    async fn categorical_answers_match_the_schema_with_and_without_explanation() {
        let url = start().await;
        for explanation in [true, false] {
            let schema = categorical(explanation);
            for _ in 0..20 {
                let answer = answer_to(&url, &schema).await;
                assert_matches(&schema, &answer);
                assert_eq!(answer.get("explanation").is_some(), explanation);
            }
        }
    }

    #[tokio::test]
    async fn the_answers_follow_the_intended_spread() {
        let url = start().await;
        let (mut trues, mut first, mut sum) = (0_u32, 0_u32, 0.0_f64);
        let (boolean_schema, label_schema) = (boolean(false), categorical(false));
        for _ in 0..200 {
            let answer = answer_to(&url, &boolean_schema).await;
            trues += u32::from(answer["value"] == true);
            let answer = answer_to(&url, &numeric(false)).await;
            sum += answer["value"].as_f64().expect("number");
            let answer = answer_to(&url, &label_schema).await;
            first += u32::from(answer["label"] == "correct");
        }
        assert!((140..=180).contains(&trues), "true answers: {trues} of 200");
        assert!((0.7..=0.9).contains(&(sum / 200.0)), "mean {}", sum / 200.0);
        assert!((100..=160).contains(&first), "first label: {first} of 200");
    }

    #[tokio::test]
    async fn a_missing_or_wrong_key_is_401() {
        let url = start().await;
        let schema = boolean(false);
        let none = ask(&url, Some(&schema), None).await;
        assert_eq!(none.status(), StatusCode::UNAUTHORIZED);
        let wrong = ask(&url, Some(&schema), Some("nope")).await;
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_request_without_a_schema_is_400() {
        let url = start().await;
        let response = ask(&url, None, Some(KEY)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response = Client::new()
            .post(&url)
            .bearer_auth(KEY)
            .body("not json")
            .send()
            .await
            .expect("answers");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_schema_with_an_unknown_field_is_refused() {
        let mut rng: StdRng = rand::make_rng();
        let schema = json!({"type": "object", "properties": {
            "value": {"type": "boolean"}, "reasoning": {"type": "string"}}});
        assert!(answer(&schema, &mut rng).is_none());
    }
}
