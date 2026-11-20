# IC M2 · Provider calls and clustering math — plan

M2 adds to `cetra-eval` everything the server's LLM issue sources (M3) need: a structured completion that takes any JSON schema, an embeddings call for OpenAI-compatible and Gemini connections, the call traces for both, the error fingerprint, and the clustering math. Nothing outside `crates/eval` changes, and nothing is user-visible yet (no `CHANGELOG.md` line).

**Prerequisites:** the IC design (`00-design.md`) on branch `claude/ic-issue-clustering-351594`.

**Read first:**
- `AGENTS.md`;
- `00-design.md` § D-IC-2, D-IC-4, D-IC-5, D-IC-8, D-IC-15, D-IC-17;
- `crates/eval/src/lib.rs`;
- `crates/eval/src/judge/mod.rs`, `judge/openai.rs`, `judge/anthropic.rs`, `judge/gemini.rs`, `judge/answer.rs` (`answer_schema`, `find_object`), `judge/http.rs` (`provider_error`);
- `crates/eval/src/runner.rs` (`EvalRunner::run`, `judge`, `trace_of`, `now_ns`);
- `crates/eval/src/trace.rs` (`TraceInput`, `ChildSpan`, `build`, `export_request`, `span_id`, `judge_metadata`, `usage_attrs` and the private helpers);
- `crates/eval/src/error.rs` (`JudgeError`, its `class()` and `Debug`);
- `crates/eval/src/check.rs` (`compile_schema`);
- `crates/eval/tests/support/mock.rs`, `tests/openai.rs`, `tests/gemini.rs`, `tests/modes.rs`, `tests/hostile.rs` (`config()`).

## What it is, and what it is not

When M2 is done:
- `EvalRunner::complete` sends one structured call to any connection (all three adapters and all three `StructuredOutput` modes), validates the answer against the caller's schema, and returns the object with a priced call trace;
- `EvalRunner::embed` embeds one text with an OpenAI-compatible or Gemini connection and returns an L2-normalized vector with a priced call trace;
- `cetra_eval::issue` holds the fingerprint, the message normalization and the clustering math as pure, tested functions.

Out of scope: any server code, any route, the connection test (M3 adds the embedding test to it), prompts for summaries and names (M3), and the budget (M3).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-IC-2 | Error fingerprint fields and normalization rules |
| D-IC-4 | Summary calls use the judge adapters and structured-output modes |
| D-IC-5 | Embeddings from the org's connection: OpenAI-compatible or Gemini, 512 dimensions requested |
| D-IC-8 | Average linkage, nearest-neighbour chain, cut at `1 − floor` |
| D-IC-15 | These pieces live in `cetra-eval` |
| D-IC-17 | Fingerprint bytes, placeholders, tie-breaks, L2-normalized vectors (≤ 4 096 dimensions, finite), call-trace shape |
| D160 | Provider error classes (`http::provider_error`) |
| D161 | Call traces are built as OTLP and normalized |

## Versions

No new dependency. `sha2`, `hex`, `regex`, `serde_json`, `jsonschema` and `proptest` are already dependencies of `cetra-eval` (`crates/eval/Cargo.toml`).

## Phases

| Phase | Edits | Depends on |
|---|---|---|
| M2.1 Generic call | `judge/*.rs`, `runner.rs`, `tests/{openai,anthropic,gemini,modes}.rs` | — |
| M2.2 Call traces and `complete` | `trace.rs`, `runner.rs`, `error.rs`, `lib.rs`, `judge/mod.rs`, new `tests/complete.rs` | M2.1 |
| M2.3 Embeddings | new `judge/embed.rs`, `judge/mod.rs`, `judge/openai.rs`, `judge/gemini.rs`, `runner.rs`, `trace.rs`, `error.rs`, `lib.rs`, `tests/support/mock.rs`, new `tests/embed.rs`, `tests/hostile.rs` | M2.2 |
| M2.4 Fingerprint | new `src/issue/mod.rs`, `src/issue/fingerprint.rs`, `lib.rs` | — |
| M2.5 Clustering math | new `src/issue/cluster.rs`, `src/issue/mod.rs` | M2.4 (module) |

All phases run one after the other in one worktree. Each ends with `just check-rust` green and one commit.

**Rules for every phase:**
- Follow the plan exactly. If a step is unclear, contradicts the code or cannot be done as written, stop and report instead of improvising.
- Touch only the phase's **Edit** files.
- No `unwrap()`/`expect()` outside tests unless a comment states the invariant. Never panic on provider input. No payload text (prompts, answers, embedded text) in errors or logs.
- Rust: compile and test only right before the commit, once (`just check-rust`); no `CARGO_BUILD_JOBS` cap.
- A `LazyLock`/`expect` on a constant needs `#[allow(clippy::expect_used)]` and an invariant comment, as at `trace.rs:145`.
- New integration-style test files (`tests/complete.rs`, `tests/embed.rs`) copy their own private helpers (`client()`, `connection()`, `prices()` from `builtin_price_rows`, and `ingest()` with `ingest_time` = the current clock, as `tests/runner.rs` builds it) from `tests/openai.rs` and `tests/runner.rs`; helpers are private per file.
- insta snapshots of traces are taken only in `trace.rs` unit tests with fixed times (they land in `crates/eval/src/snapshots/`), never in `tests/*.rs`, because `complete` and `embed` read the real clock.
- Commit once with the given subject, only when **Done when** passes.

---

### M2.1 Generic call

**Goal:** the adapters take a JSON schema and a tool name instead of a judge output, with no change in what judges send.

**Edit:** `crates/eval/src/judge/mod.rs`, `judge/openai.rs`, `judge/anthropic.rs`, `judge/gemini.rs`, `judge/answer.rs` (visibility only), `crates/eval/src/runner.rs`, `crates/eval/tests/openai.rs`, `tests/anthropic.rs`, `tests/gemini.rs`, `tests/modes.rs` (and any other file that builds a `JudgeCall`; `grep -rn JudgeCall crates/eval` lists them, and each one found is in scope).

**Steps:**
1. In `judge/mod.rs`, rename `JudgeCall` to `CompletionCall` and replace its fields `output: &'a JudgeOutput` and `include_reason: bool` with:
   ```rust
   /// JSON Schema of the answer object.
   pub(crate) schema: &'a Value,
   /// Function name forced in [`StructuredOutput::Tool`].
   pub(crate) tool_name: &'a str,
   /// Function description sent in [`StructuredOutput::Tool`].
   pub(crate) tool_description: &'a str,
   ```
   Keep `model`, `prompt`, `temperature`, `max_output_tokens` and `timeout`. Its `Debug` prints `model`, `prompt_bytes`, `tool_name`, `temperature`, `max_output_tokens` and `timeout`, and never the schema or the prompt.
2. `user_message(mode, call)` pretty-prints `call.schema` instead of calling `answer_schema`. The text format stays byte-identical (`"{prompt}\n\n{PROMPT_INSTRUCTION}\n{schema}"`).
3. In each adapter's `request_body`, use `call.schema` where it called `answer_schema(call.output, call.include_reason)`. Use `call.tool_name` and `call.tool_description` where it used `JUDGEMENT_TOOL` and `JUDGEMENT_DESCRIPTION`. For OpenAI Native, keep `json_schema.name = "judge_answer"` for judges: add a field `pub(crate) schema_name: &'a str` to `CompletionCall`, sent as that name (OpenAI requires `^[a-zA-Z0-9_-]{1,64}$`).
4. `call(client, conn, call: &CompletionCall)` keeps its signature, with the type renamed.
5. In `runner.rs` `judge()`, build the schema once: `let schema = answer_schema(&config.output, config.include_reason);`. Pass `schema: &schema`, `schema_name: "judge_answer"`, `tool_name: JUDGEMENT_TOOL` and `tool_description: JUDGEMENT_DESCRIPTION`. Make `answer_schema`, `JUDGEMENT_TOOL` and `JUDGEMENT_DESCRIPTION` reachable from `runner.rs` (`pub(crate)` re-exports in `judge/mod.rs`).
6. Update every test that builds a `JudgeCall` to build a `CompletionCall` with the same schema, so the expected request bodies stay byte-identical.

**Tests:** no new test. Every existing judge test and snapshot must pass unchanged, which proves the requests did not change.

**Done when:** `just check-rust` passes, and `git diff --stat` shows only the Edit files.

**Commit:** `Make the judge call take any JSON schema`

---

### M2.2 Call traces and `complete`

**Goal:** `EvalRunner::complete` runs a structured call and returns the validated object with a priced call trace.

**Edit:** `crates/eval/src/trace.rs`, `runner.rs`, `error.rs`, `lib.rs`, `judge/mod.rs`, `judge/answer.rs` (visibility of `find_object` only), new `crates/eval/tests/complete.rs`, plus its `#[path]` line in `lib.rs`.

**Public types** (in `runner.rs`, re-exported from `lib.rs`):
```rust
/// Where a call's trace goes and how it is priced (D-IC-17).
#[derive(Debug, Clone, Copy)]
pub struct CallTrace<'a> {
    /// Id of the call trace. The caller derives one per attempt.
    pub trace_id: TraceId,
    /// Name of the root span, e.g. `cetra.issue.summarize`.
    pub root_name: &'a str,
    /// Copied onto the root span as `cetra.metadata.<key>` attributes (ids only, never payload).
    pub metadata: &'a [(&'a str, &'a str)],
    /// Project, clock and limits for normalization.
    pub ingest: &'a IngestContext,
    /// Prices for the child span.
    pub prices: &'a PriceTable,
}

/// One structured completion (D-IC-4).
pub struct CompletionInput<'a> {
    pub connection: &'a JudgeConnection,
    pub model: &'a str,
    /// Sent as one user message.
    pub prompt: &'a str,
    /// JSON Schema (draft 2020-12) of the answer object. Must have `"type": "object"`.
    pub schema: &'a Value,
    /// OpenAI `json_schema.name` in native mode, and the forced function in tool mode.
    /// `^[a-zA-Z0-9_-]{1,64}$`.
    pub name: &'a str,
    /// Function description in tool mode.
    pub description: &'a str,
    pub temperature: Option<f64>,
    pub max_output_tokens: u32,
    pub timeout: Duration,
    pub trace: CallTrace<'a>,
}

/// The answer or the failure, and the call trace either way.
pub struct CompletionRun {
    /// The answer object, valid against the schema.
    pub outcome: Result<Map<String, Value>, EvalError>,
    /// Token counts, when the provider sent them.
    pub usage: Option<JudgeUsage>,
    /// `cost_total` of the `llm` span.
    pub cost_usd: Option<f64>,
    /// The `llm` span had tokens and no price.
    pub unpriced_model: bool,
    /// Root and `llm` spans, normalized.
    pub trace: Normalized,
}
```
`Debug` for `CompletionInput` prints `model`, `name`, `prompt_bytes`, `max_output_tokens`, `timeout` and the trace id. `Debug` for `CompletionRun` prints the outcome's error class or the number of keys, `cost_usd`, and the span count. Neither prints payload.

**Steps:**
1. `error.rs`: add to `JudgeError`:
   ```rust
   /// The answer object does not match the caller's schema. No detail is kept.
   #[error("answer does not match the schema")]
   SchemaMismatch,
   /// The caller's schema does not compile, or is not an object schema.
   #[error("invalid answer schema")]
   InvalidSchema,
   /// The call name does not match `^[a-zA-Z0-9_-]{1,64}$`.
   #[error("invalid call name")]
   InvalidName,
   ```
   Add each to `class()` (all `Permanent`), to the manual `Debug`, and to every exhaustive allow-list in `tests/hostile.rs` users, only if those lists must compile.
2. `trace.rs`: add a second builder for non-evaluator calls:
   ```rust
   pub(crate) struct CallTraceInput<'a> {
       pub(crate) trace: &'a CallTrace<'a>,
       pub(crate) started_ns: i64,
       pub(crate) ended_ns: i64,
       /// `Err` sets the root status to error with the error's `Display`.
       pub(crate) result: Result<(), &'a EvalError>,
       /// The existing `ChildSpan` enum: `Llm` here, `Embedding` from M2.3. `Webhook` is never passed.
       pub(crate) child: Option<ChildSpan<'a>>,
   }
   pub(crate) fn build_call(input: &CallTraceInput<'_>) -> Normalized
   ```
   `build_call` writes one `ResourceSpans` exactly like `export_request`: the resource `service.name = environment::EVAL` and the same scope. The root (span index 0) has:
   - name `trace.root_name`;
   - `cetra.span.kind = chain` and `cetra.environment = cetra-eval`;
   - one `cetra.metadata.<key>` string attribute per metadata pair (through `metadata_attr`);
   - no input or output;
   - status error with the error's `Display` on `Err`.

   The child (index 1) is built by the same code that builds a `ChildSpan` for `export_request`: move that code (the match on `ChildSpan` that makes the child `Span`) into a private `fn child_span(trace_id: TraceId, parent: SpanId, child: &ChildSpan<'_>) -> Span` called by both builders, so the attributes are identical. `build_call` then calls `normalize(&request, trace.ingest, trace.prices)` and logs rejections like `build`, with `root_name` instead of the evaluator id.
3. `trace.rs`: add `pub(crate) fn call_cost(trace: &Normalized, kind: SpanKind) -> (Option<f64>, bool)`. It finds the first span of `kind` and returns `(cost_total, unpriced)`, where `unpriced = (input_tokens.is_some() || output_tokens.is_some()) && cost_total.is_none()`, the same rule as `judge_metadata`. It returns `(None, false)` when there is no such span.
4. `runner.rs`: add `pub async fn complete(&self, input: CompletionInput<'_>) -> CompletionRun`, in this order:
   1. `started = now_ns()`.
   2. Validate `input.name` against `^[a-zA-Z0-9_-]{1,64}$` (byte checks, no regex). On failure the outcome is `InvalidName` with no child span.
   3. Compile the schema with `check::compile_schema`, and require `schema["type"] == "object"`. On failure the outcome is `InvalidSchema` with no child span.
   4. Build a `CompletionCall` (`schema_name` and `tool_name` both `input.name`, `tool_description = input.description`), then `call(...)`.
   5. On `Ok(reply)`, get the object with `answer::find_object(reply.answer)` (make it `pub(crate)`). Its errors are `EmptyAnswer`, `NotJson` and `NotAnObject`. Validate the object with the compiled validator: invalid is `SchemaMismatch`.
   6. On `Err(error)` from `call`, the outcome is that error, and the child has no output, no usage and the error. On a reply whose `find_object` or validation fails, the child keeps `output_text`, `usage` and `response_model` from the reply and carries the error, and `CompletionRun.usage` is the reply's usage: exactly what `judge()` does after `parse_answer` fails (`runner.rs`, around lines 220–235).
   7. `ended = now_ns().max(started)`, build the trace with `build_call`, take `(cost_usd, unpriced_model) = call_cost(&trace, SpanKind::Llm)`, set `usage` from the reply when there is one, and return.

   Log one `tracing::debug!` with `root_name`, `duration_ns` and `error_class`, as `run` does.
5. `lib.rs`: `pub use runner::{CallTrace, CompletionInput, CompletionRun, ...}` next to the existing runner exports.

**Tests** (`tests/complete.rs`, against `support/mock.rs`, with `KEY` and `assert_no_payload` as in `tests/openai.rs`):
- `complete_returns_the_object_openai_native`: the request has `response_format.json_schema.name == name` and the schema; the reply `{"summary":"x","is_issue":true}` comes back as a map.
- `complete_tool_mode_forces_the_named_function` (OpenAI): `tool_choice.function.name == name`.
- `complete_prompt_mode_appends_the_schema` (Anthropic): the user message ends with the pretty schema.
- `complete_gemini_native_sends_response_json_schema`.
- `complete_rejects_an_answer_that_misses_a_required_field`: `SchemaMismatch`, `Permanent`, and the trace has a root and an `llm` child.
- `complete_rejects_a_non_object_schema`: `InvalidSchema`, no request is sent (the mock recorded nothing), and the trace has a root only.
- `complete_rejects_a_bad_name`: `"has space"` and a 65-character name are both `InvalidName`.
- `complete_maps_a_429_to_retryable` and `complete_maps_a_401_to_pause`.
- `complete_trace_is_priced`: with `gpt-4o-mini` from `builtin_price_rows` and usage in the reply, `cost_usd` is `Some` and `unpriced_model` is false.
- `complete_trace_root_carries_name_and_metadata`: root span name, kind `chain`, environment `cetra-eval`, and `metadata["k"] == "v"`.
- `complete_errors_never_contain_the_prompt` with `SECRET-PAYLOAD` in the prompt and in the answer.
- In `trace.rs`'s unit tests (fixed `START`/`END`, the module's `ingest()` and `prices()`): insta snapshot `call_trace_completion_success` of `build_call` with an `Llm` child and metadata `[("k","v")]`, written like `judge_trace_success`.
- Note for the caller (M3): OpenAI native and tool modes send `strict: true`, which needs `additionalProperties: false` and every property in `required`. `complete` does not check this; M3's schemas follow it.

**Done when:** `just check-rust` passes. Accept a new snapshot by reading `crates/eval/src/snapshots/<name>.snap.new` and renaming it to `.snap`.

**Commit:** `Add structured completions with call traces to cetra-eval`

---

### M2.3 Embeddings

**Goal:** `EvalRunner::embed` returns a normalized vector from OpenAI-compatible or Gemini connections.

**Edit:** new `crates/eval/src/judge/embed.rs`, `judge/mod.rs`, `judge/openai.rs`, `judge/gemini.rs`, `runner.rs`, `trace.rs`, `error.rs`, `lib.rs`, `tests/support/mock.rs`, new `tests/embed.rs` (with its `#[path]` line in `lib.rs`), and `tests/hostile.rs`.

**Public types** (`runner.rs`, re-exported):
```rust
/// Largest vector accepted (D-IC-17).
pub const MAX_EMBEDDING_DIMENSIONS: usize = 4096;

/// One embeddings call (D-IC-5).
pub struct EmbeddingInput<'a> {
    pub connection: &'a JudgeConnection,
    pub model: &'a str,
    /// The text to embed. Never logged or traced in full: see step 4.
    pub text: &'a str,
    /// Requested size: OpenAI `dimensions`, Gemini `outputDimensionality`. `None` omits it.
    pub dimensions: Option<u32>,
    pub timeout: Duration,
    pub trace: CallTrace<'a>,
}

pub struct EmbeddingRun {
    /// L2-normalized vector, 1 to 4 096 finite values.
    pub outcome: Result<Vec<f32>, EvalError>,
    /// Input tokens the provider reported. `None` when it reported none (Gemini may not).
    pub input_tokens: Option<u64>,
    /// `cost_total` of the `embedding` span; `None` without tokens or price.
    pub cost_usd: Option<f64>,
    /// The span had tokens and no price.
    pub unpriced_model: bool,
    pub trace: Normalized,
}
```
`Debug` for both prints no text and no vector (it prints the vector length).

**Steps:**
1. `error.rs`: add to `JudgeError`:
   - `#[error("this provider has no embeddings API")] EmbeddingUnsupported`;
   - `#[error("invalid embedding")] EmbeddingInvalid`.

   Both are `Permanent`. Add them to `Debug`.
2. `judge/embed.rs`:
   ```rust
   pub(crate) struct EmbedCall<'a> { pub(crate) model: &'a str, pub(crate) text: &'a str,
       pub(crate) dimensions: Option<u32>, pub(crate) timeout: Duration }
   pub(crate) struct EmbedReply { pub(crate) vector: Vec<f32>, pub(crate) input_tokens: Option<u64>,
       pub(crate) response_model: Option<String> }
   pub(crate) async fn embed(client: &EgressClient, conn: &JudgeConnection, call: &EmbedCall<'_>)
       -> Result<EmbedReply, JudgeError>
   ```
   - `validate_model(call.model)?`.
   - **OpenAI:**
     - URL: `endpoint(conn.base_url.as_ref(), "https://api.openai.com/v1", "embeddings")`.
     - Headers: the same as `openai::headers`. Move that function's body into a `pub(super) fn openai_headers(conn)` in `openai.rs`, and have both call it.
     - Body: `{"model": m, "input": text, "encoding_format": "float"}`, plus `"dimensions": d` when `Some`.
     - Non-200 goes to `provider_error(Adapter::OpenAi, StructuredOutput::Prompt, status, &body)`, so no structured-output hint is added.
     - Parse: `data[0].embedding` must be an array of numbers; otherwise `EmbeddingInvalid`. Tokens come from `usage.prompt_tokens`, and the model from the top-level `model`.
   - **Gemini:**
     - URL: `endpoint(base, "https://generativelanguage.googleapis.com", &format!("v1beta/models/{model}:embedContent"))`, after `validate_model`.
     - Headers: the same as `gemini::headers`, shared the same way (`pub(super) fn gemini_headers`).
     - Body: `{"model": "models/" + m, "content": {"parts": [{"text": text}]}}`, plus `"outputDimensionality": d` when `Some`. The `model` field is in the REST docs of `embedContent`; the request body carries it besides the URL path.
     - Errors: the same mapping, with `Adapter::Gemini`.
     - Parse: `embedding.values` must be an array of numbers. Tokens come from `usageMetadata.promptTokenCount` when present, else `None`. The model is `None`.
   - **Anthropic:** returns `Err(EmbeddingUnsupported)` before any request.
   - The parsers are two private functions in `embed.rs`, each taking the 200 body: `fn parse_openai_embedding(body: &[u8]) -> Result<EmbedReply, JudgeError>` and `fn parse_gemini_embedding(body: &[u8]) -> Result<EmbedReply, JudgeError>`. A body that is not JSON is `NotJson`. Each returns the raw (not yet normalized) vector after the checks below.
   - Response limit: `RESPONSE_MAX_BYTES`.
   - Conversion: each JSON number becomes `as_f64()`, then `f32`. A non-number, or a value that is not finite after conversion, is `EmbeddingInvalid`. An empty array, or more than `MAX_EMBEDDING_DIMENSIONS` values, is `EmbeddingInvalid`. If `dimensions` was requested and the answer length differs, that is fine: the server checks the dimension (D-IC-17).
3. L2-normalize in `embed()` with `crate::issue::cluster::l2_normalize` (added in M2.5; until then, write a private `fn l2_normalize(v: &mut [f32]) -> bool` in `embed.rs`, and M2.5 moves it). A zero vector, with norm 0, is `EmbeddingInvalid`.
4. `trace.rs`: add the variant `ChildSpan::Embedding { started_ns, ended_ns, adapter, request_model, response_model, text_chars: usize, input_tokens: Option<u64>, dimensions: Option<usize>, error: Option<&JudgeError> }`. The span (index 1) is named `embeddings {model}`, with:
   - `cetra.span.kind = embedding`, `cetra.environment = cetra-eval`, `gen_ai.operation.name = embeddings`;
   - `gen_ai.provider.name = adapter.provider_name()`, `gen_ai.request.model`, and `gen_ai.response.model` when known;
   - `gen_ai.usage.input_tokens` when known;
   - `cetra.input` = the JSON `{"chars": text_chars}` with `cetra.input_format = json`, never the text;
   - `cetra.output` = `{"dimensions": n}` (format `json`) on success;
   - error status on failure.

   The text is not traced: it is a summary derived from payloads and would be stored twice. Its length is enough to debug.
5. `runner.rs`: `pub async fn embed(&self, input: EmbeddingInput<'_>) -> EmbeddingRun`, structured as `complete`: started, call, normalize the vector, ended, `build_call` with the `Embedding` child, `call_cost(&trace, SpanKind::Embedding)`, and a debug log.
6. `tests/support/mock.rs`: route `POST /v1/embeddings` to the same scripted-reply handler as `/v1/chat/completions`. Gemini's `/v1beta/models/{*rest}` already matches `:embedContent`.
7. `lib.rs`: export `EmbeddingInput`, `EmbeddingRun` and `MAX_EMBEDDING_DIMENSIONS`.

**Tests** (`tests/embed.rs`):
- `openai_embed_sends_model_input_and_dimensions`: path `/v1/embeddings`, the body exactly `{"model":"text-embedding-3-small","input":"hello","encoding_format":"float","dimensions":512}`, and the bearer header.
- `openai_embed_omits_dimensions_when_none`.
- `openai_embed_returns_a_unit_vector`: the reply `[3.0, 4.0]` gives `[0.6, 0.8]` (within 1e-6), and `input_tokens == Some(2)` from usage.
- `gemini_embed_sends_output_dimensionality` (path ends `:embedContent`, header `x-goog-api-key`), and `gemini_embed_without_usage_has_no_tokens_and_no_cost`.
- `anthropic_embed_is_unsupported`: no request is recorded, and the error is `EmbeddingUnsupported`.
- `embed_rejects_empty_nan_too_long_and_zero_vectors`: four cases, each `EmbeddingInvalid`. NaN is sent as a huge number like `1e39` that overflows f32 to infinity, since JSON has no NaN.
- `embed_maps_errors_like_judges`: 429 is retryable, 401 is pause, and `insufficient_quota` is pause.
- `embed_trace_is_priced`: `text-embedding-3-small` with `prompt_tokens: 1000` gives `cost_usd` within 1e-12 of `1000 × price_input / 1e6`, with the price read from `builtin_price_rows`.
- `embed_trace_never_holds_the_text`: the text `SECRET-PAYLOAD` appears in no span field. Serialize the spans and search them.
- In `trace.rs`'s unit tests: insta `call_trace_embedding_success` of `build_call` with an `Embedding` child, with fixed times.
- `tests/hostile.rs` gains two `pub(crate)` strategies, `openai_embedding_reply()` and `gemini_embedding_reply()`, built like `openai_reply()` (valid shapes mixed with hostile values: strings, nulls, huge numbers, nested arrays, empty arrays). A `#[cfg(test)] mod tests` in `judge/embed.rs` has `a_hostile_openai_embedding_decodes_or_fails_cleanly` and `a_hostile_gemini_embedding_decodes_or_fails_cleanly`, using `crate::hostile_tests::config()`. Each parse returns `Ok` with 1–4096 finite values, or `EmbeddingInvalid` or `NotJson`. It never panics.

**Done when:** `just check-rust` passes, and the snapshots are reviewed and moved into place.

**Commit:** `Add embeddings for OpenAI-compatible and Gemini connections`

---

### M2.4 Fingerprint

**Goal:** the error-message normalization and the fingerprint of D-IC-2 and D-IC-17, as pure functions.

**Edit:** new `crates/eval/src/issue/mod.rs`, new `crates/eval/src/issue/fingerprint.rs`, `crates/eval/src/lib.rs` (add `pub mod issue;`).

**API:**
```rust
// issue/mod.rs
//! Issue clustering helpers (track IC): error fingerprints and clustering math. Pure functions.
pub mod fingerprint;

// issue/fingerprint.rs
/// Longest normalized message, in characters (D-IC-2).
pub const MAX_MESSAGE_CHARS: usize = 512;

/// The four fields of an error fingerprint. An absent field is `None`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ErrorFields<'a> {
    pub service_name: Option<&'a str>,
    pub span_name: &'a str,
    pub exception_type: Option<&'a str>,
    /// Already normalized with [`normalize_message`].
    pub message: Option<&'a str>,
}

/// Replaces volatile parts of an error message with placeholders (D-IC-2, D-IC-17).
pub fn normalize_message(message: &str) -> String;

/// First 16 bytes of the SHA-256 fingerprint (D-IC-17).
pub fn fingerprint(fields: &ErrorFields<'_>) -> [u8; 16];

/// [`fingerprint`] as 32 lowercase hex characters, the stored form.
pub fn fingerprint_hex(fields: &ErrorFields<'_>) -> String;
```
`ErrorFields`'s derived `Debug` would print the message. Write a manual `Debug` that prints the field lengths only.

**Steps:**
0. **Bounded input.** `pub const MAX_INPUT_CHARS: usize = 8_192;` with the doc line "Only the first 8 192 characters are read; volatile parts beyond them do not matter, since the result is cut at 512." `normalize_message` first keeps only the first `MAX_INPUT_CHARS` characters: `message.char_indices().nth(MAX_INPUT_CHARS)` gives a byte index on a char boundary, and `message.get(..end)` cuts there (no panic; a shorter message is used whole). Only that head goes into step 2. The 512-character cut of step 2 still applies after the rules.
1. **Rules.** Ten rules, each a `Regex` built once in a `std::sync::LazyLock<[Rule; 10]>` (`struct Rule { regex: Regex, kind: RuleKind }`). The `expect` on `Regex::new` carries `#[allow(clippy::expect_used)]` and the comment `// The patterns are constants, compiled by every test.`. They run in this order, with exactly these patterns:
   ```rust
   const URL: &str = r##"(?i)\b([a-z][a-z0-9+.\-]*)://([^\s/?#"'<>]+)[^\s"'<>]*"##;
   const EMAIL: &str = r##"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}"##;
   const UUID: &str = r##"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b"##;
   const TIME: &str = r##"\b\d{4}-\d{2}-\d{2}(?:[T ]\d{2}:\d{2}(?::\d{2}(?:[.,]\d+)?)?(?:Z|[+\-]\d{2}:?\d{2})?)?\b"##;
   const IPV4: &str = r##"\b(?:\d{1,3}\.){3}\d{1,3}(?::\d{1,5})?\b"##;
   const IPV6: &str = r##"(?i)\b(?:[0-9a-f]{1,4}:){7}[0-9a-f]{1,4}\b"##;
   const PATH: &str = r##"(?:[A-Za-z]:)?(?:[\\/][\w.@\-]+){2,}[\\/]?"##;
   const HEX: &str = r##"\b[0-9A-Fa-f]{8,}\b"##;
   const QUOTED: &str = r##""[^"\n]{3,}"|'[^'\n]{3,}'|`[^`\n]{3,}`"##;
   const NUMBER: &str = r##"\d+(?:\.\d+)?"##;
   ```
   Replacements:
   - `URL` → `<url:{scheme}://{host}>`. The scheme is group 1, lowercased. The host is group 2, lowercased, after these steps: keep what follows the last `@`; then if it starts with `[`, keep through the first `]`; otherwise remove a trailing `:` followed by digits only.
   - `EMAIL` → `<email>`; `UUID` → `<uuid>`; `TIME` → `<time>`; `IPV4` and `IPV6` → `<ip>`; `PATH` → `<path>`.
   - `HEX` → `<hex>` only when the match holds at least one ASCII letter and at least one digit. Otherwise the match is left as it is, for `NUMBER`.
   - `QUOTED` → `<str>`; `NUMBER` → `<n>`.

   Known limits, accepted and written as a doc comment on `normalize_message`:
   - compressed IPv6 (`fe80::1`) is not recognized;
   - `11/20/2026` becomes `<n><path>`-like text;
   - `don't … can't` may become `<str>`.
2. **No rewriting inside placeholders.** Keep the text as a list of segments `(text, frozen)`, starting with one unfrozen segment. Each rule runs on unfrozen segments only, splitting each into unfrozen text around frozen placeholder segments. After the ten rules, join the segments, collapse every whitespace run (`char::is_whitespace`) into one space, trim, cut to `MAX_MESSAGE_CHARS` characters (not bytes), then `trim_end` again.
3. **`fingerprint`.** Start `Sha256` with the bytes of `cetra.issue-fp.v1`. Then, for each field in the order `service_name`, `span_name`, `exception_type`, `message`, update with the field's UTF-8 byte length as a `u32` big-endian (an absent field is length 0 and no bytes), then its bytes. A field longer than `u32::MAX` bytes cannot happen (spans are cut at ingest), but saturate the length anyway with no panic. Return the first 16 bytes of the digest.
4. `fingerprint_hex` is `hex::encode(fingerprint(fields))`.

**Tests** (unit tests in `fingerprint.rs`):
- Golden normalizations, exact output strings:
  - `"Connection to https://user@API.Example.com:443/v1/x?id=7 failed after 30s"` → `"Connection to <url:https://api.example.com> failed after <n>s"`
  - `"user bob@example.org not found"` → `"user <email> not found"`
  - `"order 3f2a1b9c-1d2e-4f5a-8b6c-7d8e9f0a1b2c missing"` → `"order <uuid> missing"`
  - `"expired at 2026-11-20T10:00:00Z"` → `"expired at <time>"`
  - `"from 10.0.0.12:5432 and 2001:db8:0:0:0:0:0:1"` → `"from <ip> and <ip>"`
  - `"GET http://[::1]:8080/x"` → `"GET <url:http://[::1]>"`
  - `"open /var/lib/cetra/data.db: denied"` → `"open <path>: denied"`
  - `"C:\\Users\\x\\a.txt"` → `"<path>"`
  - `"hash deadbeef42 vs 12345678"` → `"hash <hex> vs <n>"`
  - `"key \"customer-name\" invalid, 'ab' ok"` → `"key <str> invalid, 'ab' ok"`
  - `"took 1.5 ms (retry 3)"` → `"took <n> ms (retry <n>)"`
  - `"  a \n\t b  "` → `"a b"`
  - placeholders are not rewritten: `"https://h.io/2026-01-01"` → `"<url:https://h.io>"`.
- `normalize_cuts_at_512_chars_on_a_char_boundary`: 600 `é` give 512 characters.
- `normalize_reads_only_the_first_8192_chars`: 8 192 `a` followed by `123` give the same result as 8 192 `a` alone. A second case, 8 192 spaces followed by `abc`, gives `""`, so the test also shows the cut happens before the rules.
- `fingerprint_golden`: a fixed `ErrorFields` gives a fixed hex string. Compute it once, check it by hand against a SHA-256 of the documented byte layout in the test (build the bytes manually, hash with `Sha256`, compare), and pin it.
- `fingerprint_separates_fields`: `("ab", "c")` and `("a", "bc")` differ. `None` and `Some("")` hash the same by design (length 0, no bytes); callers turn empty strings into `None`.
- `fingerprint_is_stable_across_volatile_parts`: two messages that differ only in a UUID and a number give the same fingerprint after `normalize_message`.
- Proptest (`crate::hostile_tests::config()`): `normalize_never_panics_and_is_bounded` for any `String`. The output has at most 512 chars and no leading or trailing whitespace. There is no idempotence property: placeholders are plain text on a second pass.

**Done when:** `just check-rust` passes.

**Commit:** `Add error fingerprints for issue clustering`

---

### M2.5 Clustering math

**Goal:** the vector math and the deterministic average-linkage clustering of D-IC-7, D-IC-8 and D-IC-17.

**Edit:** new `crates/eval/src/issue/cluster.rs`, `crates/eval/src/issue/mod.rs` (add `pub mod cluster;`), `crates/eval/src/judge/embed.rs` (use `cluster::l2_normalize` and delete the private copy from M2.3).

**API:**
```rust
/// Largest input to [`average_linkage`] (D-IC-8).
pub const MAX_CLUSTER_INPUT: usize = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ClusterError {
    #[error("vectors have different dimensions")]
    DimensionMismatch,
    #[error("too many vectors to cluster")]
    TooMany,
    #[error("vectors must be finite")]
    NonFinite,
}

/// Scales `v` to unit length. Returns false, leaving `v` unchanged, when its norm is 0 or not finite.
pub fn l2_normalize(v: &mut [f32]) -> bool;

/// Dot product of two unit vectors, which is their cosine similarity.
pub fn cosine(a: &[f32], b: &[f32]) -> Result<f32, ClusterError>;

/// Index and similarity of the centroid nearest to `v`. Ties go to the lowest index. `None` when empty.
pub fn nearest<'a, I>(v: &[f32], centroids: I) -> Result<Option<(usize, f32)>, ClusterError>
where I: IntoIterator<Item = &'a [f32]>;

/// Moves `centroid` (the mean of `members` unit vectors, renormalized) towards `v`:
/// `c += (v − c) / (members + 1)`, then renormalized.
pub fn add_to_centroid(centroid: &mut [f32], members: u64, v: &[f32]) -> Result<(), ClusterError>;

/// The member-weighted mean of two centroids, renormalized (merges, D-IC-9).
pub fn merge_centroids(a: &[f32], a_members: u64, b: &[f32], b_members: u64)
    -> Result<Vec<f32>, ClusterError>;

/// Average-linkage clusters of unit vectors whose merge distance (1 − cosine) is at most
/// `max_distance`. Each cluster lists input indices in ascending order. Clusters are ordered by
/// their first index. Every input index is in exactly one cluster (singletons included).
pub fn average_linkage(vectors: &[Vec<f32>], max_distance: f32) -> Result<Vec<Vec<usize>>, ClusterError>;
```

**Steps:**
1. `l2_normalize`: compute the norm in `f64`. If it is 0 or not finite, return false. Otherwise divide each value and return true.
2. `cosine`: if the lengths differ, `DimensionMismatch`. Otherwise sum `a[i] * b[i]` in `f64`, cast to `f32`, and clamp to `[-1, 1]`.
3. `nearest`: iterate in order and keep the first strictly greater similarity. A dimension mismatch on any centroid is an error.
4. `add_to_centroid`: if the lengths differ, `DimensionMismatch`. Compute `k = 1 / (members + 1)` in `f64`, set `c[i] += (v[i] − c[i]) · k`, then `l2_normalize`. If that returns false (a degenerate zero), leave `c` equal to `v`.
5. `merge_centroids`: the weighted sum `(a · na + b · nb) / (na + nb)`, normalized. With `na + nb == 0`, use equal weights. When the sum cannot be normalized (norm 0, e.g. opposite vectors), return a copy of the heavier centroid (`a` on a tie).
6. `average_linkage`:
   1. If `vectors.len() > MAX_CLUSTER_INPUT`, `TooMany`. All vectors must have the length of the first, else `DimensionMismatch`. Any component of any vector that is NaN or infinite gives `NonFinite`; this check comes before the 0- and 1-vector cases, and it stops the chain from looping on NaN distances. With 0 vectors, return empty. With 1, return `[[0]]`.
   `cosine`, `nearest`, `add_to_centroid` and `merge_centroids` return `NonFinite` the same way when an input holds a non-finite value (checked after their length checks).
   2. Condensed distance matrix `Vec<f32>` of `n(n−1)/2` entries, `d(i, j) = 1 − cosine` with `i < j`, at index `n·i − i(i+1)/2 + (j − i − 1)`. A helper `fn idx(n, i, j)` orders the pair.
   3. **Nearest-neighbour chain** over active clusters (initially every index, size 1):
      - Grow the chain from the lowest active index when it is empty.
      - For the chain's tip `a`, find the active `b ≠ a` with the smallest `d(a, b)`. On a tie, prefer the element just before `a` in the chain if it is among the tied ones, else the lowest index.
      - If `b` is the element before `a`, merge them. Record `(a, b, d(a, b))`, pop both from the chain, and apply Lance–Williams average: for every other active `k`, `d(k, keep) = (size_a · d(k, a) + size_b · d(k, b)) / (size_a + size_b)`, where `keep = min(a, b)`. Mark `max(a, b)` inactive and set `size_keep = size_a + size_b`.
      - Otherwise push `b`.
      - Repeat until one active cluster remains.
   4. Union-find over the `n` indices: union `a` and `b` for every recorded merge whose distance `≤ max_distance + CUT_EPSILON`, with `const CUT_EPSILON: f32 = 1e-6;` (absorbs f32 rounding of identical vectors). Average linkage is monotone, so this gives the clusters of the dendrogram cut at `max_distance`.
   5. Group indices by root, sort each group ascending, and sort the groups by their first index.

   The matrix holds at most 12.5 M `f32` (about 50 MB at 5 000), and time is O(n²). The function is synchronous: the server calls it from `spawn_blocking` (M3).

**Tests** (unit tests in `cluster.rs`):
- `l2_normalize_rejects_zero_and_non_finite`, and `cosine_of_identical_unit_vectors_is_one` (axis-aligned vectors such as `[1, 0, 0]`, exact equality; for a non-axis vector, within 1e-6).
- `cosine_and_nearest_reject_dimension_mismatch`.
- `cosine_rejects_non_finite`: NaN, `inf` and `-inf` in either argument give `NonFinite`; `centroid_functions_reject_non_finite` covers `nearest`, `add_to_centroid` (the centroid is left unchanged) and `merge_centroids`.
- `average_linkage_rejects_nan_and_inf`: each of NaN, `inf` and `-inf`, in the last vector and as the only vector, gives `NonFinite`.
- `nearest_breaks_ties_on_the_lowest_index`, and `nearest_of_no_centroids_is_none`.
- `add_to_centroid_is_the_running_mean`: adding `v` to a 1-member centroid `c` gives `normalize((c + v) / 2)`.
- `merge_centroids_weights_by_members`.
- `average_linkage_two_tight_groups`: 3 vectors near `e1` and 3 near `e2`, cut 0.2, give `[[0,1,2],[3,4,5]]` in input order (interleave the inputs to check the ordering: `[e1a, e2a, e1b, e2b, e1c, e2c]` gives `[[0,2,4],[1,3,5]]`).
- `average_linkage_cut_zero_keeps_singletons_unless_identical`: duplicates merge at distance 0.
- `average_linkage_empty_and_single`.
- `average_linkage_too_many`: 5 001 vectors of length 1 give `TooMany`.
- `average_linkage_matches_a_naive_reference`: a test-only O(n³) average linkage gives the same partition on random unit vectors. It is a proptest with `crate::hostile_tests::config()`, n ≤ 40, dimension 2–8, and the cut drawn from `[0, 1]`.
  - The reference reads the same f32 pairwise distances (`1 − cosine`).
  - It repeatedly merges the closest pair of clusters, by the mean pairwise distance computed in f64, with ties on the lowest pair.
  - It stops when the closest pair is above the cut plus `CUT_EPSILON`.
  - Cases where any reference merge height is within 1e-4 of the cut are skipped (`prop_assume!`), because f32 rounding may order them differently.
- `average_linkage_is_deterministic`: the same input twice gives the same output.
- `average_linkage_partition_covers_every_index_once` (proptest).

**Done when:** `just check-rust` passes. `embed.rs` uses `cluster::l2_normalize`, and the private copy is gone.

**Commit:** `Add clustering math for issue clustering`

---

## Closing

After M2.5, a Sonnet review reads the M2 diff against this plan and the design. Its findings are fixed by a Haiku agent with an exact brief.

M3's plan will call these, all re-exported from `cetra_eval`:
- `EvalRunner::complete(CompletionInput) -> CompletionRun`;
- `EvalRunner::embed(EmbeddingInput) -> EmbeddingRun`;
- `CallTrace`;
- `issue::fingerprint::{normalize_message, fingerprint_hex, ErrorFields}`;
- `issue::cluster::{nearest, add_to_centroid, merge_centroids, average_linkage, cosine, l2_normalize, MAX_CLUSTER_INPUT}`.

M1 uses the fingerprint functions, so M1's error-source phase starts after M2.4 is merged into its branch.
