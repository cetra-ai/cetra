//! Cohere Chat (v2) bodies.
//!
//! Streaming is not mocked in M2. The mock refuses a `stream: true` request for this route.

use serde_json::json;

use crate::scenario::{Finish, Turn};

use super::{Ids, Rendered};

/// Renders a non-streaming Cohere v2 chat response.
pub fn chat(turn: &Turn, ids: &Ids) -> Rendered {
    let text = turn
        .message
        .as_ref()
        .and_then(|message| message.text.as_deref())
        .unwrap_or("");
    Rendered::Json(json!({
        "id": ids.cohere_chat(),
        "finish_reason": finish_reason(turn.finish),
        "message": {
            "role": "assistant",
            "content": [{ "type": "text", "text": text }],
        },
        "usage": {
            "billed_units": {
                "input_tokens": turn.usage.input,
                "output_tokens": turn.usage.output,
            },
            "tokens": {
                "input_tokens": turn.usage.input,
                "output_tokens": turn.usage.output,
            },
        },
    }))
}

/// `stop` is `COMPLETE` and `length` is `MAX_TOKENS`. `scenario.rs` allows no other finish.
fn finish_reason(finish: Option<Finish>) -> &'static str {
    match finish {
        Some(Finish::Length) => "MAX_TOKENS",
        _ => "COMPLETE",
    }
}
