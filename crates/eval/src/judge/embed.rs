//! Embeddings (D-IC-5): OpenAI-compatible `/embeddings` and Gemini `embedContent`.
//!
//! Anthropic has no embeddings API, so its connections get
//! [`JudgeError::EmbeddingUnsupported`] before any request is sent.

use std::time::Duration;

use serde_json::{Map, Value};

use super::gemini::{DEFAULT_BASE as GEMINI_BASE, gemini_headers};
use super::http::provider_error;
use super::openai::{DEFAULT_BASE as OPENAI_BASE, openai_headers};
use super::{
    Adapter, JudgeConnection, RESPONSE_MAX_BYTES, StructuredOutput, endpoint, validate_model,
};
use crate::egress::{EgressClient, EgressRequest};
use crate::error::JudgeError;
use crate::issue::cluster::l2_normalize;
use crate::runner::MAX_EMBEDDING_DIMENSIONS;

/// One request to embed `text`.
pub(crate) struct EmbedCall<'a> {
    /// Provider embedding model id.
    pub(crate) model: &'a str,
    /// Text to embed. Sent in the request body only.
    pub(crate) text: &'a str,
    /// Requested size: OpenAI `dimensions`, Gemini `outputDimensionality`. `None` omits it.
    pub(crate) dimensions: Option<u32>,
    /// Whole-call timeout.
    pub(crate) timeout: Duration,
}

/// The normalized vector and what the provider said about the call.
pub(crate) struct EmbedReply {
    /// L2-normalized, 1 to [`MAX_EMBEDDING_DIMENSIONS`] finite values.
    pub(crate) vector: Vec<f32>,
    /// Input tokens the provider reported, when it reported any.
    pub(crate) input_tokens: Option<u64>,
    /// Model the provider says it ran.
    pub(crate) response_model: Option<String>,
}

/// Sends one embeddings call and returns the normalized vector.
///
/// # Errors
///
/// Returns [`JudgeError::EmbeddingUnsupported`] for Anthropic, the same errors as
/// a judge call for transport and status, [`JudgeError::NotJson`] for a body that
/// is not JSON, and [`JudgeError::EmbeddingInvalid`] for a vector that is empty,
/// too long, not finite, or zero.
pub(crate) async fn embed(
    client: &EgressClient,
    conn: &JudgeConnection,
    call: &EmbedCall<'_>,
) -> Result<EmbedReply, JudgeError> {
    validate_model(call.model)?;
    let mut reply = match conn.adapter {
        Adapter::OpenAi => openai_embed(client, conn, call).await?,
        Adapter::Gemini => gemini_embed(client, conn, call).await?,
        Adapter::Anthropic => return Err(JudgeError::EmbeddingUnsupported),
    };
    if !l2_normalize(&mut reply.vector) {
        return Err(JudgeError::EmbeddingInvalid);
    }
    Ok(reply)
}

async fn openai_embed(
    client: &EgressClient,
    conn: &JudgeConnection,
    call: &EmbedCall<'_>,
) -> Result<EmbedReply, JudgeError> {
    let url = endpoint(conn.base_url.as_ref(), OPENAI_BASE, "embeddings")?;
    let headers = openai_headers(conn)?;
    let mut body = Map::new();
    body.insert("model".to_owned(), Value::String(call.model.to_owned()));
    body.insert("input".to_owned(), Value::String(call.text.to_owned()));
    body.insert(
        "encoding_format".to_owned(),
        Value::String("float".to_owned()),
    );
    if let Some(dimensions) = call.dimensions {
        body.insert("dimensions".to_owned(), Value::from(dimensions));
    }
    let body = serde_json::to_vec(&Value::Object(body)).map_err(|_| JudgeError::InvalidRequest)?;
    let response = client
        .post(EgressRequest {
            url: &url,
            headers,
            body,
            timeout: call.timeout,
            max_response_bytes: RESPONSE_MAX_BYTES,
        })
        .await?;
    if response.status.as_u16() != 200 {
        return Err(provider_error(
            Adapter::OpenAi,
            StructuredOutput::Prompt,
            response.status.as_u16(),
            &response.body,
        ));
    }
    parse_openai_embedding(&response.body)
}

async fn gemini_embed(
    client: &EgressClient,
    conn: &JudgeConnection,
    call: &EmbedCall<'_>,
) -> Result<EmbedReply, JudgeError> {
    // The model was checked by `validate_model`, so it cannot leave the path segment.
    let path = format!("v1beta/models/{}:embedContent", call.model);
    let url = endpoint(conn.base_url.as_ref(), GEMINI_BASE, &path)?;
    let headers = gemini_headers(conn)?;
    let mut body = Map::new();
    body.insert(
        "model".to_owned(),
        Value::String(format!("models/{}", call.model)),
    );
    body.insert(
        "content".to_owned(),
        serde_json::json!({"parts": [{"text": call.text}]}),
    );
    if let Some(dimensions) = call.dimensions {
        body.insert("outputDimensionality".to_owned(), Value::from(dimensions));
    }
    let body = serde_json::to_vec(&Value::Object(body)).map_err(|_| JudgeError::InvalidRequest)?;
    let response = client
        .post(EgressRequest {
            url: &url,
            headers,
            body,
            timeout: call.timeout,
            max_response_bytes: RESPONSE_MAX_BYTES,
        })
        .await?;
    if response.status.as_u16() != 200 {
        return Err(provider_error(
            Adapter::Gemini,
            StructuredOutput::Prompt,
            response.status.as_u16(),
            &response.body,
        ));
    }
    parse_gemini_embedding(&response.body)
}

/// `data[0].embedding`, `usage.prompt_tokens`, and the top-level `model`.
fn parse_openai_embedding(body: &[u8]) -> Result<EmbedReply, JudgeError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| JudgeError::NotJson)?;
    let vector = vector_of(
        value
            .pointer("/data/0/embedding")
            .ok_or(JudgeError::EmbeddingInvalid)?,
    )?;
    Ok(EmbedReply {
        vector,
        input_tokens: value
            .pointer("/usage/prompt_tokens")
            .and_then(Value::as_u64),
        response_model: value
            .get("model")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    })
}

/// `embedding.values` and `usageMetadata.promptTokenCount`. Gemini sends no model name.
fn parse_gemini_embedding(body: &[u8]) -> Result<EmbedReply, JudgeError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| JudgeError::NotJson)?;
    let vector = vector_of(
        value
            .pointer("/embedding/values")
            .ok_or(JudgeError::EmbeddingInvalid)?,
    )?;
    Ok(EmbedReply {
        vector,
        input_tokens: value
            .pointer("/usageMetadata/promptTokenCount")
            .and_then(Value::as_u64),
        response_model: None,
    })
}

/// A JSON array of 1 to [`MAX_EMBEDDING_DIMENSIONS`] numbers, each finite as `f32`.
///
/// JSON has no NaN, so an overflowing number such as `1e39` is what arrives as
/// infinity. It is rejected here.
fn vector_of(value: &Value) -> Result<Vec<f32>, JudgeError> {
    let items = value.as_array().ok_or(JudgeError::EmbeddingInvalid)?;
    if items.is_empty() || items.len() > MAX_EMBEDDING_DIMENSIONS {
        return Err(JudgeError::EmbeddingInvalid);
    }
    items
        .iter()
        .map(|item| {
            let number = item.as_f64().ok_or(JudgeError::EmbeddingInvalid)?;
            let narrowed = number as f32;
            if narrowed.is_finite() {
                Ok(narrowed)
            } else {
                Err(JudgeError::EmbeddingInvalid)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{EmbedReply, parse_gemini_embedding, parse_openai_embedding};
    use crate::error::JudgeError;
    use crate::hostile_tests::{body, config, gemini_embedding_reply, openai_embedding_reply};
    use crate::runner::MAX_EMBEDDING_DIMENSIONS;
    use proptest::prelude::*;

    fn check(result: Result<EmbedReply, JudgeError>) {
        match result {
            Ok(reply) => {
                assert!((1..=MAX_EMBEDDING_DIMENSIONS).contains(&reply.vector.len()));
                assert!(reply.vector.iter().all(|value| value.is_finite()));
            }
            Err(JudgeError::EmbeddingInvalid | JudgeError::NotJson) => {}
            Err(other) => panic!("undocumented embedding error {other:?}"),
        }
    }

    proptest! {
        #![proptest_config(config())]

        #[test]
        fn a_hostile_openai_embedding_decodes_or_fails_cleanly(bytes in body(openai_embedding_reply())) {
            check(parse_openai_embedding(&bytes));
        }

        #[test]
        fn a_hostile_gemini_embedding_decodes_or_fails_cleanly(bytes in body(gemini_embedding_reply())) {
            check(parse_gemini_embedding(&bytes));
        }
    }
}
