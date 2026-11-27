//! Ollama Chat bodies.
//!
//! Streaming is not mocked in M2. The mock refuses a `stream: true` request for this route.

use serde_json::json;

use crate::scenario::{Finish, Turn};

use super::{Ids, Rendered};

/// Renders a non-streaming Ollama `/api/chat` response.
///
/// Ollama has no response id, so `ids` is not used.
pub fn chat(turn: &Turn, ids: &Ids) -> Rendered {
    let _ = ids;
    let text = turn
        .message
        .as_ref()
        .and_then(|message| message.text.as_deref())
        .unwrap_or("");
    Rendered::Json(json!({
        "model": turn.model,
        "created_at": "2026-01-01T00:00:00Z",
        "message": { "role": "assistant", "content": text },
        "done": true,
        "done_reason": done_reason(turn.finish),
        "total_duration": 1,
        "load_duration": 1,
        "prompt_eval_count": turn.usage.input,
        "prompt_eval_duration": 1,
        "eval_count": turn.usage.output,
        "eval_duration": 1,
    }))
}

/// `stop` or `length`. `scenario.rs` allows no other finish for this API.
fn done_reason(finish: Option<Finish>) -> &'static str {
    match finish {
        Some(Finish::Length) => "length",
        _ => "stop",
    }
}
