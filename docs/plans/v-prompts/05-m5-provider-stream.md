Status: implemented (2026-11-20).

# V · M5 — Streaming provider layer

Implementation plan for milestone M5 of track V (`docs/plans/v-prompts/00-design.md` § Milestones, D-V-19). It has 7 sub-phases, **V5.1 to V5.7**, done in order by one agent per sub-phase in one worktree. **Haiku** implements each sub-phase in one go, with no context beyond this plan, the design doc and the files they name. Each sub-phase is one commit and ends with `just check-rust` green (`just check` for V5.7). Sonnet reviews after V5.5 and after V5.7.

M5 gives `cetra-eval` a provider chat layer:

- a **streaming egress POST** with the SSRF guard (D166), a body cap and an idle timeout;
- a **chat request** in one shape for the three adapters (D21): a model, canonical messages (spec 01), tools, a tool choice, a response format, parameters, a token cap and timeouts;
- a **streamed call** (`stream_chat`) that yields text deltas, complete tool calls and a final `Done` event (finish reason, usage in OTel semantics, response model), for OpenAI-compatible (`stream: true` with `include_usage`), Anthropic (Messages SSE) and Gemini (`streamGenerateContent?alt=sse`) endpoints;
- a **buffered call** (`complete_chat`, crate-internal) that sends the same request without streaming. The judges are moved onto it with no change in behaviour: the request bytes, the answer parsing, the errors and every existing judge test stay the same.

M5 adds no server route, no UI, no table and no new dependency. The playground route that uses `stream_chat` is M6.

## Prerequisites (check before you start)

- The branch contains the track V design doc. This must print a line, otherwise STOP and report:

```bash
grep -n "D-V-19" docs/plans/v-prompts/00-design.md
```

- The judge adapters are where this plan says. This must list five files, otherwise STOP and report:

```bash
ls crates/eval/src/judge/
```

  (expected: `answer.rs anthropic.rs gemini.rs http.rs mod.rs openai.rs`; `answer.rs` is the sixth and stays.)

## Read first (every sub-phase)

- `AGENTS.md`, all of it.
- `docs/plans/v-prompts/00-design.md`: § Architecture (the `cetra-eval` row), D-V-19, D-V-20, D-V-24, D-V-25.
- `docs/mvp-decisions.md`: D21, D157–D160, D166, D-I-44, D-I-46 (search for them).
- `docs/spec/06-evaluation.md` § `llm_judge` (the structured output and requests bullets).
- `docs/spec/01-data-model.md` § Canonical message format, and `crates/core/src/message.rs` (`Message`, `Role`, `Part`).
- This file up to § V5.1, then your own sub-phase.

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D21 | Hand-written HTTP adapters: OpenAI-compatible, Anthropic, Gemini. No provider SDK crate. |
| D157 | Structured output per connection: `native`, `tool`, `prompt`. |
| D159 | Judge requests: Chat Completions with `max_completion_tokens`, Messages with `max_tokens`, `generateContent` with `maxOutputTokens`; 10 s connect, whole-call timeout ≤ 300 s, a response over 4 MiB is an error. |
| D160 | Error classes: retryable, pause, permanent. |
| D166 | SSRF guard on every outbound call. |
| D-I-44, D-I-46 | Provider error parsing; extra headers go first and the adapter's own headers win. |
| D-V-19 | One streaming chat layer in `cetra-eval`; the judge becomes one caller; its behaviour and tests stay the same. The egress client gains a streaming POST with the SSRF guard, the 4 MiB cap and an idle timeout. |
| D-V-20 | The event set M6 must be able to send: text deltas, complete tool calls, and `done` with the finish reason, usage, response model; errors with D160's classes and no prompt or answer text. |
| D-V-24 | Per call: timeout at most 300 s, a stream that sends nothing for 60 s fails, `max_tokens` defaults to 4,096 and is at most 65,536. |
| D-V-25 | Sent: text, `tool_call` and `tool_call_response` parts, system instructions, tool definitions, tool choice, and a response schema through the connection's structured-output mode (`tool` mode: the forced tool's arguments are the output text). |
| **D-V-29** (new, this plan) | **Judges call the layer without streaming.** The layer has a buffered call that sends the same request without `stream` and decodes one JSON body into the same reply shape; judges use it, the playground streams. A judge reads the whole answer before parsing it, so streaming gains it nothing, and a streamed judge would change every request body the judge tests pin. |
| **D-V-30** (new, this plan) | **Stream size caps.** D159's 4 MiB applies to the answer a stream assembles (text plus tool-call arguments) and to any one SSE event; the raw stream is capped at 32 MiB. The SSE framing of a 65,536-token answer (D-V-24) is several times its text (OpenAI sends about 250 bytes per token chunk), so a 4 MiB raw cap would cut answers at about 16,000 tokens. A non-200 body is still read up to 4 MiB. |
| **D-V-31** (new, this plan) | **Message and parameter mapping.** System messages become the provider's system field (Anthropic `system`, Gemini `systemInstruction`; OpenAI keeps them in place). `tool` messages become OpenAI `tool` messages, Anthropic `tool_result` blocks in a user turn, and Gemini `functionResponse` parts whose name comes from the earlier call with the same id. Consecutive messages that map to the same Anthropic or Gemini role are merged. Reasoning parts are not sent; media and unknown parts, unknown roles, and tool parts in the wrong role are refused with the message index. A parameter the provider has no field for is not sent: Anthropic gets `temperature`, `top_p` and `stop`; Gemini gets all but `reasoning_effort`; OpenAI gets all. A Gemini tool call without an id gets `call_<n>`. |

D-V-29 to D-V-31 are recorded in `docs/mvp-decisions.md` § Track V and in spec 06 by V5.7.

**The user's answers.** None were asked: the user is away. D-V-29 to D-V-31 refine D-V-19, D-V-24 and D-V-25; the coordinating session (which owns the design doc, PR #181) was told about them when this plan was committed, and folds them into the design doc's D-V-19 row or overrides them. If it overrides one, this plan is updated before the sub-phase that depends on it.

## What it is, and what it is not

**When M5 is done:**

- `cetra_eval::stream_chat(&EgressClient, &JudgeConnection, &ChatRequest)` sends one streamed chat call and returns a `ChatStream`. `ChatStream::next().await` yields `ChatEvent::Text`, `ChatEvent::ToolCall` and one final `ChatEvent::Done`, then `None`. Dropping the stream closes the provider connection.
- `cetra_eval::EgressClient::post_stream` sends a streamed POST through the SSRF guard with a whole-call timeout, an idle timeout and a byte cap.
- Every judge call runs through the chat layer's buffered call. Every existing judge test passes unchanged, except the unit tests that move with their code (listed in each sub-phase).
- `tools/capture`'s mock provider already streams the three APIs (text, tool calls, usage); V5.7 tests the layer against it. `tools/capture` is not changed.

**Out of scope:**

- The playground route, its limits, rendering of `{{var}}`, tracing of runs: M6.
- Turning a forced tool call into output text in `tool` mode (D-V-25): the layer reports the call as a `ToolCall` whose `name` is the response format's `tool_name`; M6 shows its arguments as the output.
- Media parts, provider-specific reasoning settings (Anthropic thinking, Gemini thinking budgets), the OpenAI Responses API, retries: none in V.
- Renaming `JudgeConnection`, `Adapter` and `StructuredOutput`: they are public names the server uses; they stay in `crate::judge` and the chat layer imports them.

## Conventions for every sub-phase

- **Privacy.** No log line and no error text contains a prompt, an answer, a tool argument, a header value, a key, or a URL path or query (AGENTS.md § Privacy). Every new public type that holds payload has a hand-written `Debug` that prints lengths or names only, as specified.
- **Errors.** `cetra-eval` errors are `thiserror` enums in `crates/eval/src/error.rs`. No `unwrap()`/`expect()` outside tests unless a comment states the invariant.
- **JSON key order.** The workspace enables `serde_json/preserve_order`, and the judge snapshot tests (`crates/eval/tests/snapshots/*.snap`) compare request bodies **in key order**. Insert keys exactly in the order this plan gives.
- **No placeholders.** Every item a sub-phase adds is used by non-test code in that sub-phase (`cargo clippy --all-targets -- -D warnings` fails on dead code).
- **Tests.** Unit tests in the module they test (`#[cfg(test)] mod tests`). The crate's integration-style tests are modules under `crates/eval/tests/` included from `crates/eval/src/lib.rs` with `#[cfg(test)] #[path = "../tests/…"]`; follow that pattern for new test files. Files included with `#[path]` start with `#![allow(clippy::expect_used)]` (clippy's test allowance does not reach them).
- **Machine load.** Other sessions build on this machine. Do not run cargo while iterating. Compile and test once, right before the commit, with `CARGO_BUILD_JOBS=8 just check-rust`; if it fails, fix and run it again. Never kill a process you did not start.
- **Commands.**

```bash
CARGO_BUILD_JOBS=8 just check-rust
```

For V5.7 (the last sub-phase), the full check:

```bash
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
```

```bash
CARGO_BUILD_JOBS=8 just check
```

## Versions

No new dependency. Checked on 2026-11-20:

- `reqwest` 0.12.28 (workspace, features `rustls-tls`, `json`): `Response::chunk()` reads the body chunk by chunk without the `stream` feature, so the streaming POST needs no feature change.
- `tokio` 1.54.1 (workspace features include `time`, `net`, `io-util`): `tokio::time::timeout` gives the idle timeout; the test server uses `tokio::net::TcpListener` with `AsyncReadExt`/`AsyncWriteExt`.
- `cetra-capture` (workspace path crate `tools/capture`) becomes a **dev-dependency** of `cetra-eval` in V5.7, as D-I-43 allows. It depends on no workspace crate, so there is no cycle.

## Dependency graph

```
V5.1 egress stream ──────────────────────────────┐
V5.2 chat foundation ─► V5.3 OpenAI ─► V5.4 Anthropic ─► V5.5 Gemini + complete_chat ─► V5.6 streaming ─► V5.7 capture tests, docs
```

All sub-phases touch `crates/eval/src/lib.rs` or `crates/eval/src/chat/`, so they run in order in one worktree. V5.1 does not depend on V5.2–V5.5, but runs first.

| Sub-phase | Files |
|---|---|
| V5.1 | `crates/eval/src/egress.rs`, `crates/eval/src/lib.rs`, new `crates/eval/tests/support/stream.rs` |
| V5.2 | new `crates/eval/src/chat/mod.rs`, `crates/eval/src/judge/http.rs` → `crates/eval/src/chat/http.rs`, `crates/eval/src/error.rs`, `crates/eval/src/judge/{mod,openai,anthropic,gemini}.rs`, `crates/eval/src/runner.rs`, `crates/eval/src/trace.rs`, `crates/eval/src/lib.rs` |
| V5.3 | `crates/eval/src/chat/mod.rs`, `crates/eval/src/judge/openai.rs` → `crates/eval/src/chat/openai.rs`, `crates/eval/src/judge/mod.rs`, `crates/eval/src/error.rs`, `crates/eval/src/lib.rs` |
| V5.4 | `crates/eval/src/chat/mod.rs`, `crates/eval/src/judge/anthropic.rs` → `crates/eval/src/chat/anthropic.rs`, `crates/eval/src/judge/mod.rs` |
| V5.5 | `crates/eval/src/chat/mod.rs`, `crates/eval/src/judge/gemini.rs` → `crates/eval/src/chat/gemini.rs`, `crates/eval/src/judge/mod.rs` |
| V5.6 | `crates/eval/src/chat/{mod,openai,anthropic,gemini,http}.rs`, new `crates/eval/src/chat/{sse,stream}.rs`, `crates/eval/src/error.rs`, `crates/eval/src/lib.rs`, new `crates/eval/tests/chat_stream.rs` |
| V5.7 | `crates/eval/Cargo.toml`, `Cargo.lock`, `crates/eval/src/lib.rs`, `crates/eval/src/chat/{openai,anthropic,gemini}.rs`, new `crates/eval/tests/chat_capture.rs`, `docs/spec/06-evaluation.md`, `docs/mvp-decisions.md`, this plan's status line |

## Final layout after M5

```
crates/eval/src/
  chat/
    mod.rs        public request/event types, constants, request checks, encoding helpers,
                  prompt mode, ChatReply, send_buffered, complete_chat, stream_chat
    http.rs       provider_error (moved from judge/http.rs), stream_error
    sse.rs        SSE line decoder
    stream.rs     ChatStream, Sink, Decoder
    openai.rs     prepare, decode_reply, StreamState     (moved from judge/openai.rs)
    anthropic.rs  prepare, decode_reply, StreamState     (moved from judge/anthropic.rs)
    gemini.rs     prepare, decode_reply, StreamState     (moved from judge/gemini.rs)
  judge/
    mod.rs        Adapter, StructuredOutput, JudgeConnection, JudgeCall, JudgeReply,
                  call → chat request → complete_chat → judge_reply
    answer.rs     unchanged
  egress.rs       + EgressStreamRequest, EgressStream, EgressClient::post_stream
crates/eval/tests/
  support/stream.rs   scripted chunked HTTP server (raw TCP)
  chat_stream.rs      streamed calls against the scripted server
  chat_capture.rs     streamed calls against cetra-capture's mock provider
```

---

## V5.1 — Streaming egress POST

**Goal:** `EgressClient::post_stream` sends a POST through the same guard as `post` and returns the status and a body read chunk by chunk, with an idle timeout and a byte cap.

### Edit

- `crates/eval/src/egress.rs`
- `crates/eval/src/lib.rs`
- new `crates/eval/tests/support/stream.rs`

Nothing else.

### Steps

1. **Types** in `egress.rs`, after `EgressResponse`:

```rust
/// One streamed POST. The caller owns the URL, both timeouts, and the body cap.
#[derive(Debug)]
pub struct EgressStreamRequest<'a> {
    /// Destination. Only http and https are allowed.
    pub url: &'a Url,
    /// Extra request headers, in order.
    pub headers: Vec<(HeaderName, HeaderValue)>,
    /// Request body, sent as is.
    pub body: Vec<u8>,
    /// Whole-request timeout, from connecting to the last body byte.
    pub timeout: Duration,
    /// Longest wait for the response head, and between two body chunks.
    pub idle_timeout: Duration,
    /// More body bytes than this fail with [`EgressError::TooLarge`].
    pub max_response_bytes: usize,
}

/// A response whose body is read chunk by chunk. Dropping it closes the connection.
///
/// [`Debug`](std::fmt::Debug) is hand-written: `reqwest::Response`'s prints the URL.
pub struct EgressStream {
    status: StatusCode,
    response: reqwest::Response,
    host: String,
    idle_timeout: Duration,
    max_response_bytes: usize,
    read: usize,
}
```

   `impl fmt::Debug for EgressStream`: `debug_struct("EgressStream").field("status", &self.status).field("host", &self.host).field("read", &self.read).finish_non_exhaustive()`. (`EgressStreamRequest` derives `Debug` like the existing `EgressRequest`.)

2. **Shared guard and builder.** Add two private methods on `EgressClient` and make `post` use them (its behaviour does not change):

```rust
/// Restore mode, the scheme, and literal addresses (D166). Returns the host label.
fn guard(&self, url: &Url) -> Result<String, EgressError> {
    if self.policy.restore_mode {
        return Err(EgressError::RestoreMode);
    }
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(EgressError::Scheme);
    }
    let host = host_label(url);
    if !self.policy.allows_url(url) {
        return Err(EgressError::Denied { host });
    }
    Ok(host)
}

fn builder(
    &self,
    url: &Url,
    headers: Vec<(HeaderName, HeaderValue)>,
    body: Vec<u8>,
    timeout: Duration,
) -> reqwest::RequestBuilder {
    let mut builder = self.http.post(url.clone()).timeout(timeout).body(body);
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    builder
}
```

   `post` becomes: `let host = self.guard(req.url)?;`, then `self.builder(req.url, req.headers, req.body, req.timeout).send().await.map_err(|err| classify(&err, &host))?`, then the existing body loop unchanged.

3. **`post_stream`**, after `post`:

```rust
/// POSTs `req` and returns once the response head arrives. The body is read with
/// [`EgressStream::next_chunk`].
///
/// # Errors
///
/// The same refusals as [`Self::post`]. [`EgressError::Timeout`] when the head takes
/// longer than `idle_timeout` or `timeout`.
pub async fn post_stream(&self, req: EgressStreamRequest<'_>) -> Result<EgressStream, EgressError>
```

   Steps: `let host = self.guard(req.url)?;` → `let send = self.builder(req.url, req.headers, req.body, req.timeout).send();` → `match tokio::time::timeout(req.idle_timeout, send).await`: `Err(_)` → `Err(EgressError::Timeout)`; `Ok(Err(err))` → `Err(classify(&err, &host))`; `Ok(Ok(response))` → `Ok(EgressStream { status: response.status(), response, host, idle_timeout: req.idle_timeout, max_response_bytes: req.max_response_bytes, read: 0 })`.

4. **`EgressStream` methods:**

```rust
impl EgressStream {
    /// HTTP status of the response.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The next body chunk, or `None` at the end of the body.
    ///
    /// # Errors
    ///
    /// [`EgressError::Timeout`] when no chunk arrives within the idle timeout or the
    /// whole-request timeout ends, [`EgressError::TooLarge`] when the body passes the cap,
    /// and the other client failures as in [`EgressClient::post`].
    pub async fn next_chunk(&mut self) -> Result<Option<Bytes>, EgressError>

    /// Reads the rest of the body. At most `max_bytes` (and the stream's own cap).
    ///
    /// # Errors
    ///
    /// [`EgressError::TooLarge`] with `limit: max_bytes` when the body passes `max_bytes`,
    /// and the errors of [`Self::next_chunk`].
    pub async fn read_to_end(mut self, max_bytes: usize) -> Result<Bytes, EgressError>
}
```

   `next_chunk`: `match tokio::time::timeout(self.idle_timeout, self.response.chunk()).await`: `Err(_)` → `Err(EgressError::Timeout)`; `Ok(Err(err))` → `Err(classify(&err, &self.host))`; `Ok(Ok(None))` → `Ok(None)`; `Ok(Ok(Some(chunk)))` → `self.read = self.read.saturating_add(chunk.len())`; if `self.read > self.max_response_bytes` → `Err(EgressError::TooLarge { limit: self.max_response_bytes })`; else `Ok(Some(chunk))`.

   `read_to_end`: loop over `self.next_chunk().await?`, appending to a `Vec<u8>`; before appending, if `collected.len() + chunk.len() > max_bytes` → `Err(EgressError::TooLarge { limit: max_bytes })`. Return `Bytes::from(collected)`.

5. **Re-export** in `lib.rs`: `pub use egress::{EgressClient, EgressPolicy, EgressRequest, EgressResponse, EgressStream, EgressStreamRequest, identity_http_client};`.

6. **Test server** `crates/eval/tests/support/stream.rs`, included from `lib.rs` as `#[cfg(test)] #[path = "../tests/support/stream.rs"] mod stream_server;` (after the `mock` module). It is a raw TCP HTTP/1.1 server so a test controls each body chunk and each pause. Contract:

```rust
//! A scripted HTTP/1.1 server for streamed responses: raw TCP, chunked transfer encoding.
//! Each connection serves the next scripted reply, then closes.
#![allow(clippy::expect_used)]

/// One step of a scripted body.
pub(crate) enum Step {
    /// One chunk, written and flushed. An empty vector writes nothing.
    Send(Vec<u8>),
    /// A pause before the next step.
    Sleep(std::time::Duration),
}

/// One scripted response.
pub(crate) struct StreamReply {
    /// HTTP status.
    pub(crate) status: u16,
    /// Wait before the status line is written.
    pub(crate) head_delay: std::time::Duration,
    /// The body, step by step.
    pub(crate) steps: Vec<Step>,
}

/// A request the server received.
#[derive(Clone)]
pub(crate) struct StreamRequest {
    /// Path without the query.
    pub(crate) path: String,
    /// Query without `?`.
    pub(crate) query: Option<String>,
    /// Header names in lowercase, with their values.
    pub(crate) headers: Vec<(String, String)>,
    /// Parsed JSON body, or `Null`.
    pub(crate) body: serde_json::Value,
}

pub(crate) struct StreamServer { /* port, reply queue, seen requests: Arc<Mutex<…>> */ }

impl StreamServer {
    pub(crate) async fn start() -> Self;
    /// `http://127.0.0.1:{port}{path}`.
    pub(crate) fn url(&self, path: &str) -> reqwest::Url;
    /// `http://127.0.0.1:{port}/v1` (OpenAI-compatible base).
    pub(crate) fn base_v1(&self) -> reqwest::Url;
    /// `http://127.0.0.1:{port}` (Anthropic and Gemini base).
    pub(crate) fn origin(&self) -> reqwest::Url;
    pub(crate) fn enqueue(&self, reply: StreamReply);
    pub(crate) fn requests(&self) -> Vec<StreamRequest>;
}

/// `data: {data}\n\n`.
pub(crate) fn sse(data: &str) -> Vec<u8>;
/// `event: {event}\ndata: {data}\n\n`.
pub(crate) fn sse_named(event: &str, data: &str) -> Vec<u8>;
/// A 200 reply that sends each item as one chunk, with no pauses.
pub(crate) fn ok_chunks(chunks: Vec<Vec<u8>>) -> StreamReply;
```

   Server steps: bind `127.0.0.1:0`; spawn an accept loop; each accepted connection is handled in its own task: read until `\r\n\r\n` (stop and drop the connection past 64 KiB); parse the request line (`METHOD target HTTP/1.1`, split the target at the first `?`) and the headers (name lowercased, value trimmed); read `content-length` more bytes as the body; record a `StreamRequest` (`body` = `serde_json::from_slice` or `Value::Null`); pop the next reply (none → status 500, no steps); sleep `head_delay`; write `HTTP/1.1 {status} X\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n`; for each step: `Send(bytes)` with bytes non-empty → write `format!("{:x}\r\n", bytes.len())`, the bytes, `\r\n`, then flush; `Sleep(d)` → `tokio::time::sleep(d)`; finally write `0\r\n\r\n` and shut down the write half. A write error (the client went away) ends the task silently. Until V5.6, the egress tests leave `base_v1`, `origin`, `sse`, `sse_named`, `ok_chunks` and the `StreamRequest` fields `query`, `headers` and `body` unused: put `#[allow(dead_code)]` with the comment `// Used by the chat stream tests.` on each of them, as `tests/support/mock.rs` does.

### Tests (`egress.rs` `mod tests`)

Each uses `crate::stream_server::{StreamServer, StreamReply, Step, ok_chunks}` and a client with `allow_private_network: true` unless the test says otherwise. A helper `stream(client, url, timeout, idle, cap) -> Result<EgressStream, EgressError>` sends an empty body.

- `post_stream_yields_chunks_in_order`: reply `Send("a")`, `Sleep(50 ms)`, `Send("b")`; collect every `next_chunk` until `None` → `"ab"`; `status() == 200`; one request recorded with path `/stream`.
- `post_stream_fails_after_an_idle_gap`: `Send("a")`, `Sleep(600 ms)`, `Send("b")`; idle 150 ms, timeout 5 s → the first chunk is `"a"`, a later `next_chunk` is `Err(EgressError::Timeout)`.
- `post_stream_idle_timeout_covers_the_head`: `head_delay` 600 ms, idle 150 ms → `post_stream` is `Err(EgressError::Timeout)`.
- `post_stream_whole_timeout_ends_a_slow_drip`: 20 × (`Send("x")`, `Sleep(50 ms)`), idle 1 s, timeout 300 ms → reading ends with `Err(EgressError::Timeout)`.
- `post_stream_caps_the_body`: `Send` of 10 bytes, cap 4 → `next_chunk` is `Err(EgressError::TooLarge { limit: 4 })`; a cap of 10 reads it whole.
- `read_to_end_reads_an_error_body_and_caps_it`: status 500, `Send("oops")` → `status() == 500`, `read_to_end(64)` is `"oops"`; a second request with `read_to_end(2)` is `Err(TooLarge { limit: 2 })`.
- `post_stream_keeps_the_guard`: a strict client (`allow_private_network: false`) → `127.0.0.1` and `localhost` URLs are `Err(Denied { .. })`; `restore_mode: true` → `Err(RestoreMode)`; an `ftp://` URL → `Err(Scheme)`; no request reached the server; `assert_no_secret` holds for each error (the URLs carry `?token=SECRET-PAYLOAD`).
- The existing `post` tests stay unchanged and green.

### Done when

`CARGO_BUILD_JOBS=8 just check-rust` passes.

### Commit

`Add a streaming egress POST with an idle timeout`

---

## V5.2 — Chat foundation: shared provider pieces move to `chat`

**Goal:** a `chat` module holds what every provider shares (model check, endpoint join, headers, error classification, token usage) and a `ChatError`; the judge adapters use them, with no change in behaviour.

### Edit

- new `crates/eval/src/chat/mod.rs`
- `git mv crates/eval/src/judge/http.rs crates/eval/src/chat/http.rs`, then edit it
- `crates/eval/src/error.rs`
- `crates/eval/src/judge/mod.rs`, `crates/eval/src/judge/openai.rs`, `crates/eval/src/judge/anthropic.rs`, `crates/eval/src/judge/gemini.rs`
- `crates/eval/src/runner.rs`, `crates/eval/src/trace.rs`
- `crates/eval/src/lib.rs`

### Steps

1. **`ChatError`** in `error.rs`, after `JudgeError`'s `Debug` impl:

```rust
/// A chat call could not be sent, or the provider refused or broke it.
///
/// No variant stores prompt text, answer text, tool arguments or header values.
/// [`Self::Provider`] keeps the provider message for the UI and leaves it out of
/// [`Display`](std::fmt::Display) and [`Debug`](std::fmt::Debug).
#[derive(Clone, PartialEq, thiserror::Error)]
pub enum ChatError {
    /// The model name cannot be placed in a URL or a request body.
    #[error("invalid model name")]
    InvalidModel,
    /// An extra header name or value, or the API key, is not a valid HTTP header.
    #[error("invalid header")]
    InvalidHeader,
    /// The request breaks a bound, or its body could not be encoded.
    #[error("invalid request")]
    InvalidRequest,
    /// The provider returned a non-success status, or an error event in a stream.
    #[error("provider status {status}")]
    Provider {
        /// HTTP status code; for an error event in a stream, the status its type stands for.
        status: u16,
        /// Class derived from the status and the provider's type or code.
        class: ErrorClass,
        /// Provider error type, when it is a short token.
        provider_type: Option<String>,
        /// Provider error code, when it is a short token.
        provider_code: Option<String>,
        /// Provider error message, cut to 500 characters. Never logged; `Debug` prints its length.
        provider_message: Option<String>,
        /// Optional hint for the UI.
        hint: Option<&'static str>,
    },
    /// The HTTP client failed before a provider status was available, or a cap was passed.
    #[error(transparent)]
    Transport(#[from] EgressError),
}
```

   `impl fmt::Debug for ChatError`: `Provider` exactly as `JudgeError`'s (`provider_message_chars`), `Transport(e)` → `debug_tuple("Transport").field(e)`, unit variants by name.

   `impl ChatError { #[must_use] pub fn class(&self) -> ErrorClass }`: `Provider { class, .. }` → `*class`; `Transport(e)` → `e.class()`; the others → `ErrorClass::Permanent`.

   `impl From<ChatError> for JudgeError`: `InvalidModel` → `InvalidModel`; `InvalidHeader` → `InvalidHeader`; `InvalidRequest` → `InvalidRequest`; `Provider { .. }` → `JudgeError::Provider` with the same six fields; `Transport(e)` → `Transport(e)`. (V5.3 and V5.6 add variants and extend this match.)

2. **`chat/mod.rs`**:

```rust
//! Provider chat calls: one request shape for the OpenAI-compatible, Anthropic and
//! Gemini APIs (D21, D-V-19). Judges send it without streaming (D-V-29); the
//! playground streams it.

mod http;

use reqwest::Url;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

pub(crate) use http::provider_error;

use crate::error::ChatError;
use crate::judge::JudgeConnection;
use crate::secret::Secret;
```

   Move here, from `judge/mod.rs`, unchanged except that every `JudgeError` becomes `ChatError`: `RESPONSE_MAX_BYTES`, `MODEL_MAX_BYTES`, `validate_model`, `forbidden_in_model`, `endpoint` (with their doc comments). Move `JudgeUsage` here **renamed `TokenUsage`** (same derives, fields and docs; the struct doc becomes "Token counts in OTel semantics. Input includes cached prompt tokens. Output includes reasoning tokens."). Add:

```rust
/// Request headers: the connection's extra headers first, then `own`, which replace an
/// extra header of the same name (D-I-46).
pub(crate) fn request_headers(
    conn: &JudgeConnection,
    own: Vec<(HeaderName, HeaderValue)>,
) -> Result<Vec<(HeaderName, HeaderValue)>, ChatError> {
    let mut map = HeaderMap::new();
    for (name, value) in &conn.extra_headers {
        let name = HeaderName::try_from(name.as_str()).map_err(|_| ChatError::InvalidHeader)?;
        map.insert(name, secret_header(value)?);
    }
    for (name, value) in own {
        map.insert(name, value);
    }
    Ok(map
        .into_iter()
        .filter_map(|(name, value)| name.map(|name| (name, value)))
        .collect())
}

/// A secret as a header value.
pub(crate) fn secret_header(value: &Secret) -> Result<HeaderValue, ChatError> {
    HeaderValue::from_bytes(value.expose().as_bytes()).map_err(|_| ChatError::InvalidHeader)
}
```

3. **`chat/http.rs`** (moved): the module doc stays. `use super::…` becomes `use crate::judge::{Adapter, StructuredOutput};` and `use crate::error::{ChatError, ErrorClass};`. `provider_error` returns `ChatError` and builds `ChatError::Provider { … }`; its doc says "Builds a [`ChatError::Provider`] …". Nothing else changes in this sub-phase (V5.3 makes `is_token` visible to `chat/mod.rs`). In its tests: `use crate::error::{ChatError, ErrorClass};`, `use crate::judge::{Adapter, StructuredOutput};`, and every `JudgeError` becomes `ChatError` (mechanical; the assertions do not change).

4. **`judge/mod.rs`**: delete `mod http;`, the moved items and `JudgeUsage`. `JudgeReply::usage` becomes `TokenUsage` (`use crate::chat::TokenUsage;`). `user_message` keeps returning `JudgeError`.

5. **Judge adapters** (`openai.rs`, `anthropic.rs`, `gemini.rs`): import `use crate::chat::{RESPONSE_MAX_BYTES, TokenUsage, endpoint, provider_error, request_headers, secret_header, validate_model};` (only what each file uses) instead of the items from `super` and `super::http`. `JudgeUsage` → `TokenUsage`. Each `headers(conn)` body becomes one call to `request_headers(conn, vec![…])` with the adapter's own headers in their current order (OpenAI: `authorization` = `bearer(&conn.api_key)?`, `content-type`; Anthropic: `x-api-key` = `secret_header(&conn.api_key)?`, `anthropic-version`, `content-type`; Gemini: `x-goog-api-key` = `secret_header(&conn.api_key)?`, `content-type`); delete the local `header_value` functions; OpenAI's `bearer` and `header_bytes` return `ChatError::InvalidHeader`. Functions that return `JudgeError` use `?` (the `From` of step 1); `return Err(provider_error(…))` becomes `return Err(provider_error(…).into())`. In the moved unit tests of `openai.rs` and `anthropic.rs`, `validate_model(…)` and `endpoint(…)` now return `ChatError`: change `JudgeError::InvalidModel` to `ChatError::InvalidModel` and the `chat_completions_url` helper's return type to `Result<Url, ChatError>` (mechanical). `gemini.rs`'s `generate_content_url` keeps returning `JudgeError`: its tail becomes `Ok(endpoint(base, DEFAULT_BASE, &path)?)` (the `?` converts), so its tests do not change.

6. **`runner.rs`, `trace.rs`**: `JudgeUsage` → `TokenUsage`, imported from `crate::chat`.

7. **`lib.rs`**: add `mod chat;` (after `mod check;`), `pub use chat::TokenUsage;`, add `ChatError` to the `error` re-export, and drop `JudgeUsage` from the `judge` re-export.

### Tests

- `error.rs` `class_tests`: `chat_error_classes_and_debug`: `ChatError::Provider { status: 400, class: Permanent, provider_message: Some("echoed SECRET-PAYLOAD"), … }`'s `Debug` has `provider_message_chars: Some(21)` and no `SECRET-PAYLOAD`; `class()` is the stored class; `Transport(Timeout).class()` is `Retryable`; `InvalidModel`, `InvalidHeader`, `InvalidRequest` are `Permanent`; `JudgeError::from(ChatError::Provider { … })` keeps all six fields; `JudgeError::from(ChatError::InvalidHeader) == JudgeError::InvalidHeader`.
- `chat/mod.rs` `mod tests`: `request_headers_put_own_headers_last`: a connection with extra headers `X-A: 1` and `Content-Type: text/plain`, own `content-type: application/json` → the result holds `x-a: 1` and `content-type: application/json` once; an extra header named `bad name` → `Err(ChatError::InvalidHeader)`.
- Every existing test in `crates/eval` passes, with the mechanical edits of steps 3 and 5 only.

### Done when

`CARGO_BUILD_JOBS=8 just check-rust` passes, and the integration tests are untouched: this prints nothing:

```bash
git diff --stat "$(git merge-base HEAD origin/main)" -- crates/eval/tests ':!crates/eval/tests/support/stream.rs'
```

### Commit

`Move the shared provider pieces into a chat module`

---

## V5.3 — The chat request, OpenAI on the chat layer

**Goal:** the public chat request types exist, OpenAI requests of any conversation are encoded and decoded by `chat::openai`, and OpenAI judges run through it with no change in behaviour.

### Edit

- `crates/eval/src/chat/mod.rs`, `crates/eval/src/chat/http.rs` (`is_token` becomes `pub(super)`)
- `git mv crates/eval/src/judge/openai.rs crates/eval/src/chat/openai.rs`, then rewrite it
- `crates/eval/src/judge/mod.rs`
- `crates/eval/src/error.rs`
- `crates/eval/src/lib.rs`

### Steps

1. **`ChatError::UnsupportedMessage`** (error.rs), between `InvalidRequest` and `Provider`:

```rust
    /// A message cannot be sent (D-V-25, D-V-31). `index` is its position in the request.
    #[error("message {index}: {reason}")]
    UnsupportedMessage {
        /// Zero-based index into the request's messages.
        index: usize,
        /// Fixed reason, such as `media part`.
        reason: &'static str,
    },
```

   `Debug`: `debug_struct("UnsupportedMessage").field("index", index).field("reason", reason)`. `class()`: `Permanent`. `From<ChatError> for JudgeError`: `UnsupportedMessage { .. }` → `JudgeError::InvalidRequest`.

2. **Public types** in `chat/mod.rs` (add `use std::time::Duration;`, `use cetra_core::message::{Message, Part, Role};`, `use serde_json::{Map, Value};`):

```rust
/// Longest whole-call timeout (D-V-24, D159).
pub const CHAT_TIMEOUT_MAX: Duration = Duration::from_secs(300);
/// A stream that sends nothing for this long fails (D-V-24).
pub const CHAT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// `max_tokens` when a playground request has none (D-V-24). Callers apply it.
pub const CHAT_MAX_TOKENS_DEFAULT: u32 = 4_096;
/// Largest `max_tokens` (D-V-24).
pub const CHAT_MAX_TOKENS_MAX: u32 = 65_536;

/// Instruction appended in [`StructuredOutput::Prompt`](crate::StructuredOutput::Prompt), before the schema.
pub(crate) const PROMPT_INSTRUCTION: &str =
    "Reply with one JSON object and nothing else. It must match this JSON Schema:";

const TOOL_NAME_MAX_BYTES: usize = 64;

/// One chat call. The caller owns every field; [`stream_chat`] and the judges check the bounds.
#[derive(Clone, PartialEq)]
pub struct ChatRequest {
    /// Provider model id.
    pub model: String,
    /// Canonical messages (spec 01), at least one.
    pub messages: Vec<Message>,
    /// Tools the model may call.
    pub tools: Vec<ToolDefinition>,
    /// How the model may use `tools`. `None` leaves the provider default.
    pub tool_choice: Option<ToolChoice>,
    /// Schema the answer must match, sent through the connection's structured-output mode.
    pub response_format: Option<ResponseFormat>,
    /// Sampling parameters.
    pub params: ChatParams,
    /// Completion cap, including reasoning tokens. 1..=[`CHAT_MAX_TOKENS_MAX`].
    pub max_tokens: u32,
    /// Whole-call timeout. Above zero and at most [`CHAT_TIMEOUT_MAX`].
    pub timeout: Duration,
    /// Longest silence of a stream. Above zero. Not used by buffered calls.
    pub idle_timeout: Duration,
}

/// One tool the model may call.
#[derive(Clone, PartialEq)]
pub struct ToolDefinition {
    /// `[A-Za-z0-9_-]{1,64}`.
    pub name: String,
    /// What the tool does.
    pub description: Option<String>,
    /// JSON Schema of the arguments.
    pub parameters: Value,
}

/// How the model may use the request's tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    /// The model decides.
    Auto,
    /// No tool call.
    None,
    /// At least one tool call.
    Required,
    /// This tool. It must be one of the request's tools.
    Tool(String),
}

/// The schema an answer must match (D-V-25).
#[derive(Clone, PartialEq)]
pub struct ResponseFormat {
    /// Schema name in native mode (OpenAI `json_schema.name`).
    pub name: String,
    /// JSON Schema of the answer.
    pub schema: Value,
    /// OpenAI `strict`, in native and tool mode.
    pub strict: bool,
    /// The tool forced in tool mode. `[A-Za-z0-9_-]{1,64}`.
    pub tool_name: String,
    /// The forced tool's description.
    pub tool_description: String,
}

/// Sampling parameters. A provider without the field does not get it (D-V-31).
#[derive(Clone, PartialEq, Default)]
pub struct ChatParams {
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Nucleus sampling.
    pub top_p: Option<f64>,
    /// Stop sequences. Empty sends none.
    pub stop: Vec<String>,
    /// OpenAI and Gemini.
    pub presence_penalty: Option<f64>,
    /// OpenAI and Gemini.
    pub frequency_penalty: Option<f64>,
    /// OpenAI and Gemini.
    pub seed: Option<i64>,
    /// OpenAI only, sent as given.
    pub reasoning_effort: Option<String>,
}

/// One complete tool call from the model.
#[derive(Clone, PartialEq)]
pub struct ToolCall {
    /// Provider call id; `call_<n>` when the provider gave none.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Arguments. Text that is not JSON is kept as a JSON string (spec 01).
    pub arguments: Value,
}

/// Why generation stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    /// The model finished.
    Stop,
    /// The token cap was reached.
    Length,
    /// The model called a tool.
    ToolCall,
    /// A content filter stopped the answer.
    ContentFilter,
    /// The model refused.
    Refusal,
    /// Another provider value, kept when it is a short token.
    Other(String),
}

impl FinishReason {
    /// `stop`, `length`, `tool_call`, `content_filter`, `refusal`, or the provider's token.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolCall => "tool_call",
            Self::ContentFilter => "content_filter",
            Self::Refusal => "refusal",
            Self::Other(reason) => reason,
        }
    }
}
```

   Hand-written `Debug` (the derive would print payload):
   - `ChatRequest`: `debug_struct("ChatRequest")` with `model`, `messages` (the count, field name `messages`), `tools` (the names: `self.tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>()`), `tool_choice`, `response_format` (`self.response_format.as_ref().map(|f| f.name.as_str())`), `params`, `max_tokens`, `timeout`, `idle_timeout`.
   - `ToolDefinition`: `name` only, then `finish_non_exhaustive()`.
   - `ResponseFormat`: `name`, `strict`, `tool_name`, then `finish_non_exhaustive()`.
   - `ChatParams`: every field except `stop`, which prints as `stop_count` (`self.stop.len()`).
   - `ToolCall`: `id`, `name`, then `finish_non_exhaustive()`.

3. **Request check** in `chat/mod.rs`:

```rust
/// The bounds of D-V-24, tool names, and the parts D-V-25 allows.
pub(crate) fn check_request(request: &ChatRequest) -> Result<(), ChatError>
```

   In this order, each failure `Err(ChatError::InvalidRequest)` unless stated:
   1. `max_tokens` in `1..=CHAT_MAX_TOKENS_MAX`.
   2. `timeout` above zero and at most `CHAT_TIMEOUT_MAX`; `idle_timeout` above zero.
   3. `messages` not empty.
   4. every `tools[i].name` and, when `response_format` is set, its `tool_name`, pass `valid_tool_name`: 1 to 64 bytes, each `[A-Za-z0-9_-]`.
   5. `tool_choice == Some(ToolChoice::Tool(name))` → `name` is one of `tools[..].name`.
   6. `check_messages(&request.messages)?`.

```rust
/// Refuses what D-V-25 and D-V-31 do not send.
pub(crate) fn check_messages(messages: &[Message]) -> Result<(), ChatError>
```

   For each `(index, message)`, with `unsupported = |reason| ChatError::UnsupportedMessage { index, reason }`:
   - `Role::Other(_)` → `"unknown role"`.
   - For each part:
     - `Part::Text` in a `Role::Tool` message → `"text in a tool message"`.
     - `Part::Reasoning` → accepted (it is not sent).
     - `Part::ToolCall { id, .. }`: role not `Assistant` → `"tool call outside an assistant message"`; `id` `None` or empty → `"tool call without an id"`.
     - `Part::ToolCallResponse { id, .. }`: role not `Tool` → `"tool result outside a tool message"`; `id` `None` or empty → `"tool result without an id"`.
     - `Part::Uri` or `Part::Blob` → `"media part"`.
     - `Part::Unknown(_)` → `"unknown part"`.

4. **Encoding helpers** in `chat/mod.rs`:

```rust
/// The text parts of `message`, in order.
pub(crate) fn texts(message: &Message) -> Vec<&str>

/// One text: a JSON string. Several: `[{"type":"text","text":…}, …]`.
pub(crate) fn text_content(texts: &[&str]) -> Value

/// Tool-call arguments as JSON text: a JSON string as is, `None` as `{}`, any other
/// value as compact JSON.
pub(crate) fn arguments_text(arguments: Option<&Value>) -> Result<String, ChatError>

/// A tool result as text: a JSON string as is, any other value as compact JSON.
pub(crate) fn response_text(response: &Value) -> Result<String, ChatError>

/// A finite number. NaN and infinities are `InvalidRequest`.
pub(crate) fn number(value: f64) -> Result<Value, ChatError>

/// A provider finish value outside the known ones: kept when it is 1–64 characters of
/// `[A-Za-z0-9_.-]`, else `None`.
pub(crate) fn other_finish(raw: &str) -> Option<FinishReason>
```

   `serde_json::to_string` errors map to `ChatError::InvalidRequest`. `number` is `serde_json::Number::from_f64(value).map(Value::Number).ok_or(ChatError::InvalidRequest)`. `other_finish` checks length with `chars().count()` and the byte set with the same rule as `http.rs`'s `is_token` (make `is_token` `pub(super)` in `http.rs` and call it).

5. **Prompt mode** in `chat/mod.rs`:

```rust
/// The messages to send in prompt mode: [`PROMPT_INSTRUCTION`] and the pretty-printed
/// schema are appended, after a blank line, to the last text part of the last user
/// message that has one. When no user message has text, a user message
/// `"{PROMPT_INSTRUCTION}\n{schema}"` is appended.
pub(crate) fn apply_prompt_mode(messages: &[Message], schema: &Value) -> Result<Vec<Message>, ChatError>
```

   Steps: `pretty = serde_json::to_string_pretty(schema)` (error → `InvalidRequest`); clone the messages; find the last message with `role == Role::User` that holds a `Part::Text`; in it, the last `Part::Text`: `content.push_str("\n\n"); content.push_str(PROMPT_INSTRUCTION); content.push('\n'); content.push_str(&pretty);`. When none is found, push `Message { role: Role::User, name: None, parts: vec![Part::Text { content: format!("{PROMPT_INSTRUCTION}\n{pretty}"), extra: Map::new() }], finish_reason: None, extra: Map::new() }`.

```rust
/// The messages to encode: prompt mode applied when a response format is set.
pub(crate) fn messages_to_send<'a>(
    mode: StructuredOutput,
    request: &'a ChatRequest,
) -> Result<std::borrow::Cow<'a, [Message]>, ChatError>
```

   `Some(format)` and `mode == StructuredOutput::Prompt` → `Cow::Owned(apply_prompt_mode(&request.messages, &format.schema)?)`; otherwise `Cow::Borrowed(&request.messages)`.

```rust
/// The mode the error hint is computed for: the connection's mode when a response
/// format was sent, else prompt mode (which gives no hint).
pub(crate) fn hint_mode(conn: &JudgeConnection, request: &ChatRequest) -> StructuredOutput
```

6. **Buffered sending** in `chat/mod.rs`:

```rust
/// A request ready to send.
pub(crate) struct Prepared {
    pub(crate) url: Url,
    pub(crate) headers: Vec<(HeaderName, HeaderValue)>,
    pub(crate) body: Vec<u8>,
}

/// POSTs `prepared` without streaming and returns the 200 body (at most 4 MiB, D159).
/// Another status is [`provider_error`] with [`hint_mode`].
pub(crate) async fn send_buffered(
    client: &EgressClient,
    conn: &JudgeConnection,
    request: &ChatRequest,
    prepared: Prepared,
) -> Result<bytes::Bytes, ChatError>
```

   Body: `client.post(EgressRequest { url: &prepared.url, headers: prepared.headers, body: prepared.body, timeout: request.timeout, max_response_bytes: RESPONSE_MAX_BYTES }).await?`; status not 200 → `Err(provider_error(conn.adapter, hint_mode(conn, request), status, &response.body))`; else `Ok(response.body)`.

7. **The buffered reply** in `chat/mod.rs`:

```rust
/// A decoded non-streamed answer. Decoders never fail: every shape maps to a field.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChatReply {
    /// The provider's stop reason. Each decoder applies its provider's priority
    /// (refusal, truncation, filter), and leaves it `None` when the answer container is missing.
    pub(crate) finish_reason: Option<FinishReason>,
    /// `None` when the provider returned no answer container (no choice or message,
    /// no content array, no candidate or parts).
    pub(crate) content: Option<ReplyContent>,
    /// Token counts; `None` when the body had no usage object.
    pub(crate) usage: Option<TokenUsage>,
    /// Model the provider says it ran.
    pub(crate) response_model: Option<String>,
}

/// The answer's text and tool calls.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReplyContent {
    /// The text parts joined; `None` when there was no text part.
    pub(crate) text: Option<String>,
    /// Tool calls in order.
    pub(crate) tool_calls: Vec<ToolCall>,
}
```

   (`ChatReply` is crate-internal and holds test-visible payload; its derived `Debug` is allowed because nothing logs it. Add that sentence as a comment above the derive.)

8. **`chat/openai.rs`** (rewritten from the moved file). Keep `DEFAULT_BASE`, `bearer`, `header_bytes`, `usage_of` (returns `TokenUsage`), `u64_field`. Remove `complete`, `request_body`, `parse_completion`, `tool_arguments`, `headers`. Add:

```rust
/// POSTs `request` to Chat Completions without streaming.
pub(crate) async fn complete(
    client: &EgressClient,
    conn: &JudgeConnection,
    request: &ChatRequest,
) -> Result<ChatReply, ChatError> {
    let prepared = prepare(conn, request)?;
    let body = send_buffered(client, conn, request, prepared).await?;
    Ok(decode_reply(&body))
}

pub(crate) fn prepare(conn: &JudgeConnection, request: &ChatRequest) -> Result<Prepared, ChatError>
fn request_body(mode: StructuredOutput, request: &ChatRequest) -> Result<Vec<u8>, ChatError>
fn encode_messages(messages: &[Message]) -> Result<Vec<Value>, ChatError>
fn finish_reason(raw: &str) -> Option<FinishReason>
pub(crate) fn decode_reply(body: &[u8]) -> ChatReply
```

   `prepare`: (1) `validate_model(&request.model)?`; (2) `check_request(request)?`; (3) `url = endpoint(conn.base_url.as_ref(), DEFAULT_BASE, "chat/completions")?`; (4) `headers = request_headers(conn, vec![(authorization, bearer(&conn.api_key)?), (content-type, application/json)])?`; (5) `body = request_body(conn.structured_output, request)?`.

   `request_body`: a `Map`, keys inserted in this order, absent ones skipped:
   1. `"model"`: the model.
   2. `"messages"`: `encode_messages(&messages_to_send(mode, request)?)?`.
   3. `"max_completion_tokens"`: `max_tokens`.
   4. `"temperature"`, `"top_p"`: `number(…)?`; `"stop"`: the strings, when not empty; `"presence_penalty"`, `"frequency_penalty"`: `number(…)?`; `"seed"`; `"reasoning_effort"`.
   5. native mode with a format: `"response_format": {"type": "json_schema", "json_schema": {"name": name, "strict": strict, "schema": schema}}`.
   6. `"tools"`, when the list is not empty: each request tool as `{"type": "function", "function": {"name": …, "description": … (only when set), "parameters": …}}`, then, in tool mode with a format, `{"type": "function", "function": {"name": tool_name, "description": tool_description, "parameters": schema, "strict": strict}}`.
   7. `"tool_choice"`: tool mode with a format → `{"type": "function", "function": {"name": tool_name}}`; else, **only when `request.tools` is not empty** (providers refuse a choice without tools), the request's choice: `Auto` → `"auto"`, `None` → `"none"`, `Required` → `"required"`, `Tool(name)` → `{"type": "function", "function": {"name": name}}`.
   8. `serde_json::to_vec` (error → `InvalidRequest`).

   `encode_messages`, per message, in order (skip a message that would be empty):
   - `System`, `User`: `texts` not empty → `{"role": "system"|"user", "content": text_content(&texts)}`.
   - `Assistant`: `{"role": "assistant", "content": text_content(&texts) or null when there is no text}`, then, when it has `ToolCall` parts, `"tool_calls": [{"id": id, "type": "function", "function": {"name": name, "arguments": arguments_text(arguments.as_ref())?}}, …]`. Skip it when it has neither.
   - `Tool`: one `{"role": "tool", "tool_call_id": id, "content": response_text(response)?}` per `ToolCallResponse` part.
   - Reasoning parts are left out; the message `name` is not sent.
   - Ids of `ToolCall` and `ToolCallResponse` parts are read with `id.as_deref().unwrap_or_default()`: `check_request` already refused a missing or empty id, so the default is never used (say so in a comment). The Anthropic and Gemini encoders do the same.

   `finish_reason`: `"stop"` → `Stop`; `"length"` → `Length`; `"tool_calls"` or `"function_call"` → `ToolCall`; `"content_filter"` → `ContentFilter`; other → `other_finish(raw)`.

   `decode_reply` (the old `parse_completion`'s order, D-V-29):
   1. `value = serde_json::from_slice(body).unwrap_or(Value::Null)`.
   2. `usage` = `value.get("usage")` when it is an object → `Some(usage_of(Some(usage)))`, else `None`; `response_model` = `value.model` as a string.
   3. `choice = value.choices[0]`; when missing → `finish_reason: None, content: None`.
   4. `message = choice.get("message")`; when the key is missing → `finish_reason: None, content: None`.
   5. `finish_reason`: `message.refusal` is a non-empty string → `Some(Refusal)`; else `choice.finish_reason` as a string → `finish_reason(raw)`; else `None`.
   6. `tool_calls`: when `message.tool_calls` is an array, one `ToolCall` per entry, **including entries that are not objects**: `id` = `entry.id` when a non-empty string, else `format!("call_{n}")` with `n` the entry's position; `name` = `entry.function.name` as a string, else `""`; `arguments` = `entry.function.arguments`: a string → `serde_json::from_str` when it parses, else `Value::String(raw)`; missing or not a string → `Value::Null`.
   7. `text` = `message.content` when it is a string.
   8. `content: Some(ReplyContent { text, tool_calls })`.

9. **Judge on the chat layer** in `judge/mod.rs`:

```rust
/// The chat request of one judge call: the prompt as one user message, the answer
/// schema as the response format (`judge_answer`, strict), and [`JUDGEMENT_TOOL`] as the
/// forced tool in tool mode.
fn chat_request(call: &JudgeCall<'_>) -> ChatRequest {
    ChatRequest {
        model: call.model.to_owned(),
        messages: vec![Message {
            role: Role::User,
            name: None,
            parts: vec![Part::Text {
                content: call.prompt.to_owned(),
                extra: Map::new(),
            }],
            finish_reason: None,
            extra: Map::new(),
        }],
        tools: Vec::new(),
        tool_choice: None,
        response_format: Some(ResponseFormat {
            name: "judge_answer".to_owned(),
            schema: answer::answer_schema(call.output, call.include_reason),
            strict: true,
            tool_name: JUDGEMENT_TOOL.to_owned(),
            tool_description: JUDGEMENT_DESCRIPTION.to_owned(),
        }),
        params: ChatParams {
            temperature: call.temperature,
            ..ChatParams::default()
        },
        max_tokens: call.max_output_tokens,
        timeout: call.timeout,
        idle_timeout: CHAT_IDLE_TIMEOUT,
    }
}

/// The judge's reading of a reply. Refusal, truncation and filtering fail first, then a
/// missing answer, then the first tool call (its arguments must be a JSON object), then the text.
pub(crate) fn judge_reply(reply: ChatReply) -> Result<JudgeReply, JudgeError>
```

   `judge_reply` steps: (1) `finish_reason` `Some(Refusal)` → `Err(Refused)`; `Some(Length)` → `Err(Truncated)`; `Some(ContentFilter)` → `Err(ContentFiltered)`. (2) `content = reply.content.ok_or(EmptyAnswer)?`. (3) `usage = reply.usage.unwrap_or_default()`. (4) first tool call: `object = call.arguments.as_object().cloned().ok_or(NotJson)?`, `output_text = serde_json::to_string(&call.arguments).map_err(|_| NotJson)?`, answer `RawAnswer::Object(object)`. (5) else `text = content.text.ok_or(EmptyAnswer)?`, answer `RawAnswer::Text(text.clone())`, `output_text = text`. Keep `response_model`.

   `call` becomes: `Adapter::OpenAi => judge_reply(crate::chat::openai::complete(client, conn, &chat_request(call)).await?)`; the Anthropic and Gemini arms stay as they are. `chat/mod.rs` declares `pub(crate) mod openai;`, and `judge/mod.rs` drops its `mod openai;` line (V5.4 and V5.5 drop `mod anthropic;` and `mod gemini;` the same way). `user_message` and `PROMPT_INSTRUCTION`'s old copy: `judge/mod.rs` now imports `PROMPT_INSTRUCTION` from `crate::chat` (Anthropic and Gemini still call `user_message` until V5.5).

10. **`lib.rs`**: `pub use chat::{CHAT_IDLE_TIMEOUT, CHAT_MAX_TOKENS_DEFAULT, CHAT_MAX_TOKENS_MAX, CHAT_TIMEOUT_MAX, ChatParams, ChatRequest, FinishReason, ResponseFormat, TokenUsage, ToolCall, ToolChoice, ToolDefinition};`.

### Tests

`chat/mod.rs` `mod tests` (a helper `request()` builds a valid `ChatRequest` with one user text message, `max_tokens` 16, timeouts 2 s):
- `check_request_enforces_the_bounds`: `max_tokens` 0 and 65,537 → `InvalidRequest`; 65,536 → `Ok`; `timeout` zero and 301 s → `InvalidRequest`; 300 s → `Ok`; `idle_timeout` zero → `InvalidRequest`; no messages → `InvalidRequest`.
- `check_request_checks_tool_names_and_choice`: tool names `""`, `"a b"`, `"é"`, 65 × `a` → `InvalidRequest`; 64 × `a` and `"get_weather-2"` → `Ok`; `tool_choice: Tool("missing")` → `InvalidRequest`; `Tool("get_weather")` with that tool → `Ok`; a response format with `tool_name: "bad name"` → `InvalidRequest`.
- `check_messages_names_the_index_and_reason`: one case per reason of step 3 (each message at index 1 after a valid user message) → `UnsupportedMessage { index: 1, reason }`; a user message with a reasoning part → `Ok`; the `Display` is `message 1: media part` for the media case.
- `prompt_mode_appends_the_schema_after_a_blank_line` (moved here from `judge/mod.rs` in V5.5; in V5.3 add it under this name with the same expected string as the judge test, calling `apply_prompt_mode` on one user message `"Grade."` and the boolean answer schema written as a literal, `json!({"type": "object", "properties": {"value": {"type": "boolean"}}, "required": ["value"], "additionalProperties": false})` in that key order (`answer_schema` is private to `judge::answer`)): the last text equals the judge test's expected text.
- `prompt_mode_targets_the_last_user_text`: messages `[user "a", assistant "b", user "c" + "d"]` → only `"d"` gains the suffix; `[system "s"]` → a user message `"{PROMPT_INSTRUCTION}\n{schema}"` is appended.
- `argument_and_result_helpers`: `arguments_text(Some(&json!("{\"a\":1}")))` is `{"a":1}` verbatim; `Some(&json!({"a": 1}))` → `{"a":1}`; `None` → `{}`; `response_text(&json!("ok"))` → `ok`, `&json!({"t": 1})` → `{"t":1}`.
- `number_refuses_non_finite`: NaN, ±∞ → `InvalidRequest`; 0.5 → `0.5`.
- `other_finish_keeps_tokens_only`: `"pause_turn"` → `Some(Other("pause_turn"))`; `""`, 65 × `a`, `"a b"` → `None`.
- `debug_hides_payload`: a `ChatRequest` whose message text, tool description, tool parameters, stop sequence and response schema all contain `SECRET-PAYLOAD`, and a `ToolCall` whose arguments contain it: `format!("{request:?} {call:?}")` has no `SECRET-PAYLOAD` and contains the model and the tool name.

`chat/openai.rs` `mod tests`:
- Moved from the old file: `joins_the_default_and_a_custom_base`, `rejects_model_names_that_cannot_be_sent` (unchanged since V5.2), and `a_hostile_completion_decodes_or_fails_cleanly`, whose body becomes `match crate::judge::judge_reply(decode_reply(&bytes)) { … }` with the same arms.
- `encodes_a_tool_conversation`: messages `[system "Be brief.", user "Hi" + "there", assistant "Checking." + tool_call {id "call_1", name "get_weather", arguments {"city": "Paris"}}, tool tool_call_response {id "call_1", response {"temp": 21}}, assistant reasoning "…" + text "21 degrees."]` → `messages` equals `[{"role":"system","content":"Be brief."}, {"role":"user","content":[{"type":"text","text":"Hi"},{"type":"text","text":"there"}]}, {"role":"assistant","content":"Checking.","tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}]}, {"role":"tool","tool_call_id":"call_1","content":"{\"temp\":21}"}, {"role":"assistant","content":"21 degrees."}]`.
- `encodes_params_tools_and_choices`: every `ChatParams` field set → the body has `temperature`, `top_p`, `stop`, `presence_penalty`, `frequency_penalty`, `seed`, `reasoning_effort` with those values; two tools, one without a description → no `description` key on it; `tool_choice` `Auto`, `None`, `Required`, `Tool("t")` → `"auto"`, `"none"`, `"required"`, `{"type":"function","function":{"name":"t"}}`; an empty `stop` sends no `stop` key; `tool_choice: Some(Auto)` with no tools sends no `tool_choice` key.
- `structured_modes_with_user_tools`: native mode with one user tool and a format → `response_format` and `tools` with the user tool only; tool mode → `tools` is `[user tool, forced tool]` and `tool_choice` is the forced tool even when the request's choice is `Auto`; prompt mode → no `response_format`, the last user text carries the schema.
- `decode_reply_maps_finish_reasons`: bodies with `finish_reason` `stop`, `length`, `tool_calls`, `content_filter`, `"weird_one"`, `"has space"` → `Stop`, `Length`, `ToolCall`, `ContentFilter`, `Other("weird_one")`, `None`; a non-empty `refusal` with `finish_reason: "length"` → `Refusal`; a choice without `message` and `finish_reason: "length"` → `finish_reason: None, content: None`; no choices → both `None`.
- `decode_reply_reads_tool_calls_and_usage`: two tool calls, the first without an id and with arguments `"not json"`, the second with id `"c2"` and arguments `"{\"a\":1}"` → `[ToolCall { id: "call_0", name, arguments: "not json" as a JSON string }, ToolCall { id: "c2", …, arguments: {"a": 1} }]`; a `usage` object → `Some`; none → `None`.

`judge/mod.rs` `mod tests`:
- `judge_reply_keeps_the_old_priority`: `Refusal` with content → `Refused`; `Length` with no content → `Truncated`; `ContentFilter` → `ContentFiltered`; `Stop` with `content: None` → `EmptyAnswer`; content with a first tool call whose arguments are `[1]` and text `"{}"` → `NotJson`; first tool call with an object → `RawAnswer::Object`, `output_text` compact JSON; no tool call and `text: None` → `EmptyAnswer`; text `"x"` → `RawAnswer::Text("x")`; `usage: None` → `TokenUsage::default()`.
- `crates/eval/tests/openai.rs` and the OpenAI cases of `crates/eval/tests/modes.rs` pass unchanged (the snapshots `openai_chat_*.snap` too).

### Done when

`CARGO_BUILD_JOBS=8 just check-rust` passes, and the `git diff --stat` command of V5.2's **Done when** prints nothing.

### Commit

`Run OpenAI judges through the chat layer`

---

## V5.4 — Anthropic on the chat layer

**Goal:** `chat::anthropic` encodes and decodes Anthropic Messages calls of any conversation, and Anthropic judges run through it with no change in behaviour.

### Edit

- `git mv crates/eval/src/judge/anthropic.rs crates/eval/src/chat/anthropic.rs`, then rewrite it
- `crates/eval/src/chat/mod.rs` (`pub(crate) mod anthropic;`, `arguments_object`)
- `crates/eval/src/judge/mod.rs`

### Steps

0. Add to `chat/mod.rs` (Anthropic and Gemini use it; OpenAI sends arguments as text):

```rust
/// Tool-call arguments as a JSON object: an object as is; `None` and `null` as `{}`; a
/// JSON string that parses to an object as that object. Anything else is
/// `UnsupportedMessage { index, reason: "tool call arguments are not a JSON object" }`.
pub(crate) fn arguments_object(index: usize, arguments: Option<&Value>) -> Result<Map<String, Value>, ChatError>
```

   Its test in `chat/mod.rs` `mod tests`, `arguments_object_wants_an_object`: `arguments_object(2, …)`: object → itself; `None` and `Some(&Value::Null)` → `{}`; `json!("{\"a\":1}")` → `{"a":1}`; `json!("[1]")`, `json!(3)`, `json!("x")` → `UnsupportedMessage { index: 2, reason: "tool call arguments are not a JSON object" }`.

1. Keep `DEFAULT_BASE`, `ANTHROPIC_VERSION`, `usage_of` (→ `TokenUsage`), `u64_field`, `stop_details_refusal`. Remove `complete` (old), `headers`, `request_body`, `parse_message`, `tool_input`. Add `complete`, `prepare`, `request_body`, `encode_messages`, `finish_reason`, `decode_reply` with the same signatures as OpenAI's (V5.3 step 8), plus:

```rust
/// The stop reason, with Anthropic's priority: truncation, then refusal.
fn finish_reason(stop_reason: Option<&str>, refusal_details: bool) -> Option<FinishReason>
```

   In this order: `Some("max_tokens")` → `Length`; else `stop_reason == Some("refusal") || refusal_details` → `Refusal` (this test runs before the arms below, so `None` with refusal details is `Refusal`, as the old `parse_message` did); then `"end_turn"` or `"stop_sequence"` → `Stop`; `"tool_use"` → `ToolCall`; other → `other_finish`; `None` → `None`.

2. `prepare`: `validate_model`, `check_request`, `endpoint(base, DEFAULT_BASE, "v1/messages")`, `request_headers(conn, vec![(x-api-key, secret_header(&conn.api_key)?), (anthropic-version, ANTHROPIC_VERSION), (content-type, application/json)])`, `request_body`.

3. `request_body`: keys in this order, absent ones skipped:
   1. `"model"`; 2. `"max_tokens"`; 3. `"system"`: `[{"type": "text", "text": t}, …]` for every text part of every system message, in order, when there is at least one; 4. `"messages"`; 5. `"temperature"`, `"top_p"` (`number`), `"stop_sequences"` (when not empty); 6. native mode with a format: `"output_config": {"format": {"type": "json_schema", "schema": schema}}`; 7. `"tools"` when not empty: request tools as `{"name", "description" (when set), "input_schema": parameters}`, then in tool mode with a format `{"name": tool_name, "description": tool_description, "input_schema": schema}`; 8. `"tool_choice"`: tool mode with a format → `{"type": "tool", "name": tool_name}`; else, only when `request.tools` is not empty, `Auto` → `{"type": "auto"}`, `None` → `{"type": "none"}`, `Required` → `{"type": "any"}`, `Tool(n)` → `{"type": "tool", "name": n}`.
   `presence_penalty`, `frequency_penalty`, `seed` and `reasoning_effort` are not sent (D-V-31).

4. `encode_messages(messages: &[Message]) -> Result<Vec<Value>, ChatError>` (system messages are skipped here; step 3 sends them). Build `(role, blocks)` entries:
   - `User` → `("user", [{"type":"text","text":t} …])`.
   - `Assistant` → `("assistant", blocks)` in part order: text → `{"type":"text","text":t}`; tool call → `{"type":"tool_use","id":id,"name":name,"input": arguments_object(index, arguments.as_ref())?}`.
   - `Tool` → `("user", [{"type":"tool_result","tool_use_id":id,"content":response_text(response)?} …])`.
   - Skip an entry with no blocks. When the previous entry has the same role, append the blocks to it instead.
   - Each entry becomes `{"role": role, "content": c}` where `c` is the block's text as a JSON string when the entry has exactly one block and it is a text block, else the blocks array.

5. `decode_reply` (the old `parse_message`'s order):
   1. `value` (invalid → `Null`); `usage` = `value.usage` when an object → `Some(usage_of(…))`; `response_model` = `value.model`.
   2. `finish_reason = finish_reason(value.stop_reason as str, stop_details_refusal(&value))`.
   3. `blocks = value.content` as an array; missing → `content: None`.
   4. `tool_calls`: every block with `type == "tool_use"` **that has an `input` key**, in order: `id` = `block.id` when a non-empty string, else `call_{n}` (`n` = count of tool calls collected so far); `name` = `block.name` or `""`; `arguments` = `input.clone()`.
   5. `text`: the `text` strings of the blocks with `type == "text"`, joined with no separator; `None` when there is none.

6. `judge/mod.rs`: the `Adapter::Anthropic` arm becomes `judge_reply(crate::chat::anthropic::complete(client, conn, &chat_request(call)).await?)`.

### Tests (`chat/anthropic.rs` `mod tests`)

- Moved: `joins_the_default_and_a_custom_base`, `rejects_a_model_name_with_a_slash` (unchanged since V5.2), `a_hostile_message_decodes_or_fails_cleanly` (body `match crate::judge::judge_reply(decode_reply(&bytes))`, same arms).
- `encodes_a_tool_conversation`: the V5.3 conversation → `system` is `[{"type":"text","text":"Be brief."}]`; `messages` is `[{"role":"user","content":[{"type":"text","text":"Hi"},{"type":"text","text":"there"}]}, {"role":"assistant","content":[{"type":"text","text":"Checking."},{"type":"tool_use","id":"call_1","name":"get_weather","input":{"city":"Paris"}}]}, {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"{\"temp\":21}"}]}, {"role":"assistant","content":"21 degrees."}]`.
- `merges_a_tool_result_with_the_next_user_message`: `[user "q", assistant tool_call, tool response, user "and?"]` → three entries; the third is `{"role":"user","content":[tool_result block, {"type":"text","text":"and?"}]}`.
- `tool_call_arguments_must_be_an_object`: an assistant tool call with arguments `"[1]"` at index 1 → `UnsupportedMessage { index: 1, reason: "tool call arguments are not a JSON object" }`.
- `encodes_params_tools_and_choices`: all params set → only `temperature`, `top_p`, `stop_sequences` appear; tool choices → `{"type":"auto"}`, `{"type":"none"}`, `{"type":"any"}`, `{"type":"tool","name":"t"}`; tool mode puts the forced tool after the user tools.
- `decode_reply_maps_stop_reasons`: `max_tokens` with refusal details → `Length`; `refusal` → `Refusal`; `end_turn` with `stop_details.type == "refusal"` → `Refusal`; `end_turn`, `stop_sequence` → `Stop`; `tool_use` → `ToolCall`; `pause_turn` → `Other("pause_turn")`; no content array → `content: None`.
- `decode_reply_skips_tool_use_without_input`: blocks `[tool_use without input, tool_use {id "t2", input {"a":1}}]` → one tool call `t2`.
- `crates/eval/tests/anthropic.rs` and the Anthropic cases of `crates/eval/tests/modes.rs` pass unchanged.

### Done when

`CARGO_BUILD_JOBS=8 just check-rust` passes, and the `git diff --stat` command of V5.2's **Done when** prints nothing.

### Commit

`Run Anthropic judges through the chat layer`

---

## V5.5 — Gemini on the chat layer, and `complete_chat`

**Goal:** `chat::gemini` encodes and decodes Gemini calls; every judge call goes through one `complete_chat`; the old judge request code is gone.

### Edit

- `git mv crates/eval/src/judge/gemini.rs crates/eval/src/chat/gemini.rs`, then rewrite it
- `crates/eval/src/chat/mod.rs`
- `crates/eval/src/judge/mod.rs`

### Steps

1. Keep `DEFAULT_BASE`, `FILTERED_FINISH`, `generate_content_url` (now `-> Result<Url, ChatError>`; it still calls `validate_model` first), `blocked`, `usage_of` (→ `TokenUsage`), `u64_field`. Remove `complete` (old), `headers`, `request_body`, `parse_generate_content`, `function_args`. Add `complete`, `prepare`, `request_body`, `encode_messages`, `finish_reason(raw: &str) -> Option<FinishReason>`, `decode_reply`.

2. `prepare`: `validate_model`, `check_request`, `generate_content_url(conn.base_url.as_ref(), &request.model)`, `request_headers(conn, vec![(x-goog-api-key, secret_header(&conn.api_key)?), (content-type, application/json)])`, `request_body`.

3. `request_body`: keys in this order, absent ones skipped:
   1. `"systemInstruction"`: `{"parts": [{"text": t}, …]}` for every system text, when there is one;
   2. `"contents"`;
   3. `"generationConfig"`, keys in order: `"maxOutputTokens"`, `"temperature"`, `"topP"`, `"stopSequences"` (when not empty), `"presencePenalty"`, `"frequencyPenalty"`, `"seed"`, then in native mode with a format `"responseMimeType": "application/json"` and `"responseJsonSchema": schema`. `reasoning_effort` is not sent (D-V-31).
   4. `"tools"`, when there is at least one declaration: `[{"functionDeclarations": [request tools as {"name", "description" (when set), "parametersJsonSchema": parameters}, then in tool mode with a format {"name": tool_name, "description": tool_description, "parametersJsonSchema": schema}]}]`;
   5. `"toolConfig"`: tool mode with a format → `{"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": [tool_name]}}`; else, only when `request.tools` is not empty, `Auto` → `{"functionCallingConfig": {"mode": "AUTO"}}`, `None` → `"NONE"`, `Required` → `"ANY"`, `Tool(n)` → `{"mode": "ANY", "allowedFunctionNames": [n]}`.

4. `encode_messages`: walk the messages in order, recording `id → name` for each assistant `ToolCall` part as it is met (so a tool result can only name an earlier call). Build `(role, parts)` entries, system messages skipped:
   - `User` → `("user", [{"text": t} …])`.
   - `Assistant` → `("model", …)` in part order: text → `{"text": t}`; tool call → `{"functionCall": {"name": name, "args": arguments_object(index, arguments.as_ref())?}}` (the id is not sent).
   - `Tool` → `("user", [{"functionResponse": {"name": names[id], "response": r}} …])`, where `r` is the response when it is an object, else `{"result": response}`; an id with no earlier call → `UnsupportedMessage { index, reason: "tool result for an unknown tool call" }`.
   - Skip entries with no parts; merge into the previous entry when the role is the same. Each entry → `{"role": role, "parts": parts}`.

5. `finish_reason`: `"MAX_TOKENS"` → `Length`; in `FILTERED_FINISH` → `ContentFilter`; `"STOP"` → `Stop`; other → `other_finish`.

6. `decode_reply` (the old `parse_generate_content`'s order):
   1. `value`; `usage` = `usageMetadata` when an object; `response_model` = `modelVersion`.
   2. `blocked(&value)` → `finish_reason: Some(ContentFilter), content: None`.
   3. `candidate = candidates[0]`; missing → both `None`.
   4. `finish_reason` = `candidate.finishReason` as a string → `finish_reason(raw)`.
   5. `parts = candidate.content.parts` as an array; missing → `content: None`.
   6. `tool_calls`: each part whose `functionCall` **has an `args` key**: `id` = `functionCall.id` when a non-empty string, else `call_{n}`; `name` or `""`; `arguments` = `args.clone()`.
   7. `text`: the `text` strings of parts whose `thought` is not `true`, joined; `None` when there is none.

7. **`complete_chat`** in `chat/mod.rs`:

```rust
/// Sends `request` without streaming (judges, D-V-29).
pub(crate) async fn complete_chat(
    client: &EgressClient,
    conn: &JudgeConnection,
    request: &ChatRequest,
) -> Result<ChatReply, ChatError> {
    match conn.adapter {
        Adapter::OpenAi => openai::complete(client, conn, request).await,
        Adapter::Anthropic => anthropic::complete(client, conn, request).await,
        Adapter::Gemini => gemini::complete(client, conn, request).await,
    }
}
```

   The provider modules become private (`mod openai;` …) with `pub(crate)` items as needed.

8. **`judge/mod.rs`**: `call` becomes `judge_reply(crate::chat::complete_chat(client, conn, &chat_request(call)).await?)`. Delete `user_message`, its test `prompt_mode_appends_the_schema_after_a_blank_line` (the chat test of V5.3 has the same expected text), and the `PROMPT_INSTRUCTION` import it was the last user of. Keep `JUDGEMENT_TOOL`, `JUDGEMENT_DESCRIPTION`, the types and the other tests.

### Tests (`chat/gemini.rs` `mod tests`)

- Moved: `puts_the_model_in_the_path_and_keeps_the_key_out_of_the_query`, `rejects_a_model_name_before_building_a_url`, `rejects_a_model_that_would_leave_the_models_path` (expected errors become `ChatError::InvalidModel`; mechanical), `a_hostile_generation_decodes_or_fails_cleanly` (body `match crate::judge::judge_reply(decode_reply(&bytes))`, same arms).
- `encodes_a_tool_conversation`: the V5.3 conversation → `systemInstruction` `{"parts":[{"text":"Be brief."}]}`; `contents` `[{"role":"user","parts":[{"text":"Hi"},{"text":"there"}]}, {"role":"model","parts":[{"text":"Checking."},{"functionCall":{"name":"get_weather","args":{"city":"Paris"}}}]}, {"role":"user","parts":[{"functionResponse":{"name":"get_weather","response":{"temp":21}}}]}, {"role":"model","parts":[{"text":"21 degrees."}]}]`.
- `wraps_a_non_object_tool_result`: response `"sunny"` → `"response": {"result": "sunny"}`.
- `a_tool_result_needs_an_earlier_call`: a tool message with id `"x"` and no call → `UnsupportedMessage { index, reason: "tool result for an unknown tool call" }`.
- `encodes_params_tools_and_choices`: all params → `generationConfig` has `maxOutputTokens`, `temperature`, `topP`, `stopSequences`, `presencePenalty`, `frequencyPenalty`, `seed` and no reasoning key; choices → `AUTO`, `NONE`, `ANY`, `ANY` + `allowedFunctionNames: ["t"]`.
- `decode_reply_reads_blocks_finishes_and_calls`: a block reason → `ContentFilter`, `content: None`; `MAX_TOKENS` without parts → `Length`, `content: None`; `SAFETY` → `ContentFilter`; `STOP` → `Stop`; `OTHER` → `Other("OTHER")`; a `functionCall` without `args` is skipped and one with `args` and no id gets `call_0`; thought parts are left out of the text.
- `crates/eval/tests/gemini.rs`, `modes.rs`, `runner.rs` and the rest pass unchanged.

### Done when

`CARGO_BUILD_JOBS=8 just check-rust` passes, the `git diff --stat` command of V5.2's **Done when** prints nothing, and `ls crates/eval/src/judge/` lists only `answer.rs` and `mod.rs`.

### Commit

`Run every judge through the chat layer`

---

## Review checkpoint (after V5.5)

The coordinating session runs a Sonnet code review of V5.1–V5.5 (`git diff main...HEAD -- crates/eval`): behaviour of the judges unchanged, key orders, privacy of `Debug` and errors, D166 kept. Fixes go in by a Haiku agent with an exact brief before V5.6.

The review (2026-11-20) found no change in judge behaviour. Fixed in one commit, `Fix the review findings of the chat foundation`:

- `EgressRequest` and `EgressStreamRequest` get a hand-written `Debug` (the derive printed the URL, the header values, among them the API key, and the body): `url_host` (`self.url.host_str()`), `headers` (the names only), `body_bytes` (`self.body.len()`), the timeouts and the cap.
- `ChatParams`' `Debug` prints `reasoning_effort` as `reasoning_effort_set` (`is_some()`).
- `arguments_text`: `None` and `Some(Value::Null)` are both `{}` (OpenAI refuses `"null"`).
- Anthropic: in a user turn made by merging, `tool_result` blocks come before the other blocks (Anthropic requires them first); a stable partition, the order inside each group kept.
- `check_request`: in a request with a response format, a user tool named like `tool_name` is `InvalidRequest` (tool mode would send two tools of that name).

Left as they are, and said in spec 06 by V5.7: `check_request`'s bounds also apply to judges (their config already enforces the same ones); an empty text part is sent as is; a request whose messages are all system messages is sent and the provider answers 400; tool mode replaces the request's `tool_choice` with the forced tool. V5.6 removes the `#[allow(dead_code)]` of `tests/support/stream.rs` that its tests make unneeded.

---

## V5.6 — Streamed calls

**Goal:** `stream_chat` sends a streamed request to any of the three providers and `ChatStream::next` yields text deltas, complete tool calls and one `Done` event, within the caps of D-V-30 and the timeouts of D-V-24.

### Edit

- `crates/eval/src/chat/mod.rs`, `crates/eval/src/chat/openai.rs`, `crates/eval/src/chat/anthropic.rs`, `crates/eval/src/chat/gemini.rs`, `crates/eval/src/chat/http.rs`
- new `crates/eval/src/chat/sse.rs`, new `crates/eval/src/chat/stream.rs`
- `crates/eval/src/error.rs`
- `crates/eval/src/lib.rs`
- new `crates/eval/tests/chat_stream.rs`

### Steps

1. **`ChatError::BadStream`** (error.rs), after `UnsupportedMessage`: `#[error("invalid provider stream")] BadStream` — "An event of the stream is not UTF-8 or not the JSON its provider sends." `class()`: `Permanent`; `Debug`: `BadStream`; `From<ChatError> for JudgeError`: `BadStream` → `JudgeError::InvalidRequest` (judges never stream).

2. **Public stream types** in `chat/mod.rs`:

```rust
/// Largest answer a stream may assemble, text plus tool-call arguments, and largest
/// single SSE event (D-V-30, D159).
pub(crate) const ANSWER_MAX_BYTES: usize = RESPONSE_MAX_BYTES;
/// Largest raw stream (D-V-30).
pub(crate) const STREAM_MAX_BYTES: usize = 32 * 1024 * 1024;

/// One event of a streamed call.
#[derive(Clone, PartialEq)]
pub enum ChatEvent {
    /// A piece of the answer text. Never empty.
    Text(String),
    /// A complete tool call. In tool mode, the forced tool's call arrives here (D-V-25).
    ToolCall(ToolCall),
    /// The last event.
    Done(ChatDone),
}

/// How a streamed call ended.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatDone {
    /// Why generation stopped, when the provider said.
    pub finish_reason: Option<FinishReason>,
    /// Token counts; `None` when the stream carried none (the run has no cost).
    pub usage: Option<TokenUsage>,
    /// Model the provider says it ran.
    pub response_model: Option<String>,
}
```

   `Debug for ChatEvent`: `Text(t)` → `debug_struct("Text").field("bytes", &t.len()).finish()`; `ToolCall(c)` → `debug_tuple("ToolCall").field(c)`; `Done(d)` → `debug_tuple("Done").field(d)`.

   ```rust
   /// Arguments assembled from a stream: blank text is `{}`, JSON text is its value, other
   /// text is kept as a JSON string.
   pub(crate) fn stream_arguments(raw: &str) -> Value
   ```

3. **`chat/sse.rs`**, the SSE decoder (spec: the WHATWG event-stream format; only `data` and `event` fields are used):

```rust
/// One dispatched event.
pub(crate) struct SseEvent {
    /// The `event:` field, when one was sent.
    pub(crate) event: Option<String>,
    /// The `data:` lines joined with `\n`.
    pub(crate) data: String,
}

/// Splits a byte stream into events. Lines end with `\n` or `\r\n`.
pub(crate) struct SseDecoder {
    line: Vec<u8>,
    event: Option<String>,
    data: String,
    has_data: bool,
    max_event_bytes: usize,
}

impl SseDecoder {
    pub(crate) fn new(max_event_bytes: usize) -> Self;
    /// Feeds `chunk`; returns the events it completed, in order.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, ChatError>;
    /// End of stream: a last unterminated line is read, and pending data is dispatched.
    pub(crate) fn finish(&mut self) -> Result<Vec<SseEvent>, ChatError>;
}
```

   `push`: for each byte: `\n` → take `self.line` and call `self.line_done(&line, &mut out)?`; else push it to `self.line`, and when `self.line.len() + self.data.len() > self.max_event_bytes` → `Err(ChatError::Transport(EgressError::TooLarge { limit: self.max_event_bytes }))`.
   `line_done(raw)`: strip one trailing `\r`. Empty → when `has_data`, push `SseEvent { event: self.event.take(), data: std::mem::take(&mut self.data) }` and clear `has_data`; else clear `event`. Otherwise `std::str::from_utf8` (error → `BadStream`); a line starting with `:` is a comment (ignored); split at the first `:` into field and value (no `:` → the whole line is the field, the value is empty), and strip one leading space from the value; `data` → push `\n` first when `has_data`, then the value, set `has_data`, and check the size as above; `event` → `self.event = Some(value.to_owned())`; other fields are ignored.
   `finish`: when `line` is not empty, `line_done` it; then when `has_data`, dispatch as for an empty line.

4. **`chat/http.rs`**, mid-stream errors:

```rust
/// An error event inside a stream, as a [`ChatError::Provider`] with no hint. The status is
/// Gemini's `error.code` (400–599), else the one the error type stands for: `invalid_request_error`
/// 400, `authentication_error` 401, `billing_error` 402, `permission_error` 403,
/// `not_found_error` 404, `request_too_large` 413, `rate_limit_error` 429,
/// `overloaded_error` 529, anything else 500.
pub(crate) fn stream_error(adapter: Adapter, value: &Value) -> ChatError
```

   `chat/mod.rs` re-exports it next to `provider_error`: `pub(crate) use http::{provider_error, stream_error};`, and the provider modules import it from `super`. Body: compute the status; `let body = serde_json::to_vec(value).unwrap_or_default();` then `provider_error(adapter, StructuredOutput::Prompt, status, &body)`. (Gemini's code: `value.pointer("/error/code").and_then(Value::as_u64)` in `400..=599`, converted with `u16::try_from`; the type: `value.pointer("/error/type").and_then(Value::as_str)`.)

5. **`chat/stream.rs`**:

```rust
/// Collects the events a decoder produces, and counts the answer's bytes (D-V-30).
#[derive(Default)]
pub(crate) struct Sink {
    events: VecDeque<ChatEvent>,
    answer_bytes: usize,
}

impl Sink {
    /// Queues a text piece. Empty text is dropped.
    pub(crate) fn text(&mut self, text: &str) -> Result<(), ChatError>;
    /// Queues a complete tool call. Its arguments' compact JSON counts toward the cap.
    pub(crate) fn tool_call(&mut self, call: ToolCall) -> Result<(), ChatError>;
    /// Queues the last event.
    pub(crate) fn done(&mut self, done: ChatDone);
}
```

   The byte count adds `text.len()`, or `serde_json::to_string(&call.arguments)` length plus `call.name.len()` (a serialization error counts 0); past `ANSWER_MAX_BYTES` → `Err(ChatError::Transport(EgressError::TooLarge { limit: ANSWER_MAX_BYTES }))`.

```rust
/// Whether the decoder saw the provider's end of stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    Continue,
    End,
}

pub(crate) enum Decoder {
    OpenAi(openai::StreamState),
    Anthropic(anthropic::StreamState),
    Gemini(gemini::StreamState),
}

/// A streamed chat call. Dropping it closes the provider connection.
pub struct ChatStream {
    body: EgressStream,
    sse: SseDecoder,
    decoder: Decoder,
    sink: Sink,
    finished: bool,
    error: Option<ChatError>,
}

impl ChatStream {
    /// The next event, or `None` after [`ChatEvent::Done`] or an error.
    ///
    /// # Errors
    ///
    /// [`ChatError::Transport`] for a timeout (whole call or 60 s of silence), a failed
    /// connection or a passed cap; [`ChatError::Provider`] for an error event;
    /// [`ChatError::BadStream`] for an event that cannot be read.
    pub async fn next(&mut self) -> Result<Option<ChatEvent>, ChatError>;
}
```

   `chat/mod.rs` declares `mod sse;` and `mod stream;` and re-exports `pub use stream::ChatStream;`; `stream.rs` uses `super::{openai, anthropic, gemini}` for the state types.

   ```rust
   impl ChatStream {
       /// A stream reading `body` with `decoder`.
       pub(crate) fn new(body: EgressStream, decoder: Decoder) -> Self
   }
   ```

   It sets `sse: SseDecoder::new(ANSWER_MAX_BYTES)`, `sink: Sink::default()`, `finished: false`, `error: None`. `ChatStream` has one more field, `error: Option<ChatError>`.

   `Debug for ChatStream`: `debug_struct("ChatStream").field("finished", &self.finished).finish_non_exhaustive()`.

   `next` loop:
   1. `if let Some(event) = self.sink.events.pop_front() { return Ok(Some(event)); }`
   2. `if self.finished { return Ok(None); }`
   3. `match self.body.next_chunk().await`:
      - `Ok(Some(chunk))` → for each event of `self.sse.push(&chunk)`, call the decoder's `on_event(&event, &mut self.sink)`; when it returns `Flow::End`, call the decoder's `finish(&mut self.sink)`, set `finished`, and stop feeding events (the rest of the body is not read).
      - `Ok(None)` → feed the events of `self.sse.finish()` the same way; when not yet finished, call the decoder's `finish_at_eof(&mut self.sink)` (below); set `finished`.
      - `Err(e)` → the error below.
   4. Any error from step 3 (egress, SSE, decoder) → set `finished` and store it in `self.error`; the events already queued are still delivered first.
   5. Loop.

   Step 1 becomes: pop a queued event and return it; else, when `self.error` is `Some`, `take` it and return `Err(error)`. After the error is returned, `next` returns `Ok(None)`.

   A small private `fn feed(&mut self, events: Vec<SseEvent>) -> Result<(), ChatError>` does the per-event part; `Decoder` has `fn on_event(&mut self, event: &SseEvent, sink: &mut Sink) -> Result<Flow, ChatError>`, `fn finish(&mut self, sink: &mut Sink) -> Result<(), ChatError>` and `fn finish_at_eof(&mut self, sink: &mut Sink) -> Result<(), ChatError>` that match on the variant.

   **A body that ends without the provider's end signal** is not a success: `finish_at_eof` returns `Err(ChatError::BadStream)` unless the state saw its end signal, and otherwise calls `finish`. The end signals: OpenAI — a `finish_reason` was seen (the `[DONE]` line itself returns `Flow::End`); Anthropic — `message_stop` (which returns `Flow::End`, so at EOF it was never seen: always `BadStream`); Gemini — a `finishReason` was seen or the prompt was blocked. Each `StreamState` exposes `fn ended(&self) -> bool` for this. OpenAI and Gemini keep a separate `saw_finish: bool` field, set whenever a `finish_reason` / `finishReason` **string** arrives (even one `finish_reason(raw)` maps to `None`); `ended()` reads it, not the `finish_reason` field.

6. **`stream_chat`** in `chat/mod.rs`:

```rust
/// Sends `request` as a streamed call (D-V-19).
///
/// # Errors
///
/// The request errors of [`ChatError`] before anything is sent; a non-200 answer is
/// [`ChatError::Provider`] (its body read up to 4 MiB); transport failures are
/// [`ChatError::Transport`].
pub async fn stream_chat(
    client: &EgressClient,
    conn: &JudgeConnection,
    request: &ChatRequest,
) -> Result<ChatStream, ChatError>
```

   Steps: (1) `prepared` and `decoder` by adapter: `openai::prepare(conn, request, true)?` with `Decoder::OpenAi(openai::StreamState::default())`, and the same for the other two. (2) `client.post_stream(EgressStreamRequest { url: &prepared.url, headers: prepared.headers, body: prepared.body, timeout: request.timeout, idle_timeout: request.idle_timeout, max_response_bytes: STREAM_MAX_BYTES }).await?`. (3) Status not 200 → `bytes = body.read_to_end(RESPONSE_MAX_BYTES).await?` and `Err(provider_error(conn.adapter, hint_mode(conn, request), status, &bytes))`. (4) `Ok(ChatStream::new(body, decoder))` with `SseDecoder::new(ANSWER_MAX_BYTES)`, an empty `Sink`, `finished: false`.

   `prepare` of each provider gains a `stream: bool` argument (`complete` passes `false`):
   - OpenAI: after the last key of V5.3, `"stream": true` and `"stream_options": {"include_usage": true}`.
   - Anthropic: after the last key of V5.4, `"stream": true`.
   - Gemini: the body does not change; the URL path is `v1beta/models/{model}:streamGenerateContent` (give `generate_content_url` a `stream: bool` argument choosing the method; its moved tests pass `false`, and `puts_the_model_in_the_path_and_keeps_the_key_out_of_the_query` also checks that `true` with the custom base gives path `/gemini/v1beta/models/gemini-2.5-flash:streamGenerateContent` and query `version=1&alt=sse`), then, inside `generate_content_url` when `stream` is true, `url.query_pairs_mut().append_pair("alt", "sse")` on the joined URL (bind it `let mut url = endpoint(…)?;`; the base's own query is kept before it).

7. **`openai::StreamState`**:

```rust
#[derive(Default)]
pub(crate) struct StreamState {
    calls: BTreeMap<u64, PartialCall>,
    finish_reason: Option<FinishReason>,
    refused: bool,
    usage: Option<TokenUsage>,
    response_model: Option<String>,
    emitted: usize,
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}
```

   `on_event(&mut self, event: &SseEvent, sink: &mut Sink) -> Result<Flow, ChatError>`:
   1. `event.data == "[DONE]"` → `Ok(Flow::End)`.
   2. `value = serde_json::from_str::<Value>(&event.data)` (error → `BadStream`).
   3. `value.error` is an object → `Err(stream_error(Adapter::OpenAi, &value))`.
   4. `value.model` as a string → `response_model`; `value.usage` as an object → `usage = Some(usage_of(Some(u)))`.
   5. `choice = value.choices[0]`; none → `Ok(Continue)`.
   6. `delta = choice.delta`: `delta.content` string → `sink.text(..)?`; `delta.refusal` non-empty string → `refused = true` and `sink.text(..)?`; `delta.tool_calls` array → for `(position, entry)`: `index = entry.index` as u64, else `position`; the `PartialCall` at `index` (created when new): a non-empty string `entry.id` replaces `id`; a non-empty string `entry.function.name` replaces `name`; a string `entry.function.arguments` is appended.
   7. `choice.finish_reason` as a string → `finish_reason = finish_reason(raw)` (V5.3's function), then `self.flush(sink)?`.
   8. `Ok(Flow::Continue)`.

   `flush`: take the calls in index order; each → `ToolCall { id: if empty { format!("call_{}", self.emitted) } else { id }, name, arguments: stream_arguments(&arguments) }`, `sink.tool_call(..)?`, `emitted += 1`.
   `finish(&mut self, sink)`: `flush`, then `sink.done(ChatDone { finish_reason: if refused { Some(Refusal) } else { take(finish_reason) }, usage, response_model })`.

8. **`anthropic::StreamState`**:

```rust
#[derive(Default)]
pub(crate) struct StreamState {
    blocks: BTreeMap<u64, Block>,
    usage: Map<String, Value>,
    stop_reason: Option<String>,
    refusal_details: bool,
    response_model: Option<String>,
    emitted: usize,
}

enum Block {
    Text,
    ToolUse { id: String, name: String, json: String },
    Other,
}
```

   `on_event`: `value` (error → `BadStream`); `kind = value.type` as a string, else `event.event`; then:
   - `"message_start"`: `message.model` → `response_model`; merge `message.usage`.
   - `"content_block_start"`: `index = value.index` as u64 (else 0); `content_block.type` `"text"` → `Block::Text`, and its `text` when non-empty → `sink.text`; `"tool_use"` → `Block::ToolUse { id, name, json: String::new() }` from its `id`/`name` strings (else empty); other → `Block::Other`.
   - `"content_block_delta"`: `delta.type` `"text_delta"` → `sink.text(delta.text)`; `"input_json_delta"` → append `delta.partial_json` to the `ToolUse` block at `index`; others ignored.
   - `"content_block_stop"`: remove the block at `index`; a `ToolUse` → emit it (below).
   - `"message_delta"`: `delta.stop_reason` string → `stop_reason`; `delta.stop_details.type == "refusal"` → `refusal_details = true`; merge `value.usage`.
   - `"message_stop"` → `Ok(Flow::End)`.
   - `"error"` → `Err(stream_error(Adapter::Anthropic, &value))`.
   - anything else (`ping`, unknown) → ignored.
   Merge: when the value is an object, insert each key and value into `self.usage` (a later value replaces an earlier one).
   Emit: `ToolCall { id: if empty { call_{emitted} } else { id }, name, arguments: stream_arguments(&json) }`, `sink.tool_call`, `emitted += 1`.
   `finish`: emit the `ToolUse` blocks still open, in index order; `sink.done(ChatDone { finish_reason: finish_reason(stop_reason.as_deref(), refusal_details), usage: (!self.usage.is_empty()).then(|| usage_of(Some(&Value::Object(take(&mut self.usage))))), response_model })`.

9. **`gemini::StreamState`**:

```rust
#[derive(Default)]
pub(crate) struct StreamState {
    finish_reason: Option<FinishReason>,
    blocked: bool,
    usage: Option<TokenUsage>,
    response_model: Option<String>,
    emitted: usize,
}
```

   `on_event`: `value` (error → `BadStream`); `value.error` an object → `Err(stream_error(Adapter::Gemini, &value))`; `modelVersion` → `response_model`; `usageMetadata` an object → `usage`; `blocked(&value)` → `blocked = true`; `candidate = candidates[0]`, when present: for each part of `content.parts`: `thought == true` → skip; `text` string → `sink.text`; `functionCall` object → `ToolCall { id: functionCall.id when non-empty, else call_{emitted}; name or ""; arguments: functionCall.args cloned, or {} when missing }` (a streamed call without `args` is a call with no arguments; the buffered decoder keeps skipping it, as the old judge parser did, which only matters to judges), `sink.tool_call`, `emitted += 1`; `finishReason` string → `finish_reason(raw)`. `Ok(Flow::Continue)` (Gemini ends at the end of the body).
   `finish`: `sink.done(ChatDone { finish_reason: if blocked { Some(ContentFilter) } else { finish_reason }, usage, response_model })`.

10. **`lib.rs`**: add `ChatDone`, `ChatEvent`, `ChatStream`, `stream_chat` to the `chat` re-export; include the new test module: `#[cfg(test)] #[path = "../tests/chat_stream.rs"] mod chat_stream_tests;`.

### Tests

`chat/sse.rs` `mod tests`:
- `splits_events_across_chunks`: `"data: a\n"`, `"\ndata: b\r\n\r\n"` pushed separately → events `a`, `b`; a chunk boundary inside a line works.
- `joins_data_lines_and_reads_event_names`: `"event: x\ndata: 1\ndata: 2\n\n"` → `SseEvent { event: Some("x"), data: "1\n2" }`; the next event without `event:` has `None`.
- `ignores_comments_and_other_fields`: `": ping\nid: 3\nretry: 5\ndata:no-space\n\n"` → one event `no-space`.
- `finish_dispatches_a_last_unterminated_event`: `"data: tail"` then `finish()` → one event `tail`.
- `caps_an_event`: `max_event_bytes` 8, `"data: 0123456789"` → `Err(Transport(TooLarge { limit: 8 }))`.
- `refuses_invalid_utf8`: `b"data: \xff\n\n"` → `Err(BadStream)`.

`chat/mod.rs` `mod tests`:
- `stream_arguments_keeps_what_it_cannot_parse`: `""` and `"  "` → `{}`; `"{\"a\":1}"` → `{"a":1}`; `"{\"a\":"` → `"{\"a\":"` as a JSON string.
- `chat_event_debug_hides_payload`: `ChatEvent::Text("SECRET-PAYLOAD")` debug is `Text { bytes: 14 }`.

`crates/eval/tests/chat_stream.rs` (uses `crate::stream_server::*`, a client with `allow_private_network: true`, connections built like `tests/openai.rs`'s with `structured_output: Native`, and a helper `collect(stream) -> Result<Vec<ChatEvent>, ChatError>` that calls `next` until `None`):
- `openai_streams_text_tool_calls_and_usage`: chunks `{"model":"gpt-x","choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}`, two content deltas `"Hel"` and `"lo"`, a tool call over two chunks (`{"index":0,"id":"call_9","function":{"name":"get_weather","arguments":"{\"ci"}}` then `{"index":0,"function":{"arguments":"ty\":\"Paris\"}"}}`), a finish chunk `"finish_reason":"tool_calls"`, a usage chunk with `"choices":[]` and `{"prompt_tokens":10,"completion_tokens":3}`, then `[DONE]` → events `[Text("Hel"), Text("lo"), ToolCall { id: "call_9", name: "get_weather", arguments: {"city": "Paris"} }, Done { finish_reason: Some(ToolCall), usage: Some(TokenUsage { input: 10, output: 3, ..TokenUsage::default() }), response_model: Some("gpt-x") }]`; the request body has `"stream": true` and `"stream_options": {"include_usage": true}`; `next` after `Done` returns `None`.
- `openai_refusal_is_text_and_a_refusal_finish`: a `refusal` delta `"No."` and `finish_reason: "stop"` → `[Text("No."), Done { finish_reason: Some(Refusal), usage: None, .. }]`.
- `anthropic_streams_text_tool_use_and_merged_usage`: named events `message_start` (model `claude-x`, usage `{"input_tokens":7,"cache_read_input_tokens":3,"output_tokens":1}`), `ping`, a text block (`content_block_start` with empty text, two `text_delta`s, `content_block_stop`), a `tool_use` block (`id "toolu_1"`, two `input_json_delta`s making `{"q":"x"}`), `message_delta` (`stop_reason: "tool_use"`, usage `{"output_tokens":12}`), `message_stop` → `[Text, Text, ToolCall { id: "toolu_1", .. arguments {"q": "x"} }, Done { finish_reason: Some(ToolCall), usage: Some(TokenUsage { input: 10, output: 12, cache_read: 3, ..TokenUsage::default() }), response_model: Some("claude-x") }]`; the request body has `"stream": true`.
- `anthropic_error_event_is_a_classified_provider_error`: after `message_start`, `event: error` with `{"type":"error","error":{"type":"overloaded_error","message":"SECRET-PAYLOAD"}}` → `Err(Provider { status: 529, class: Retryable, .. })`; its `Display` is `provider status 529`; then `next` is `Ok(None)`.
- `gemini_streams_text_function_calls_and_usage`: the request path ends with `:streamGenerateContent`, the query is `alt=sse`, the header `x-goog-api-key` is the key and the query has no key; chunks: a thought part (skipped), two text chunks, a final chunk with a `functionCall` without id (`{"name":"lookup","args":{"id":1}}`), `finishReason: "STOP"`, `usageMetadata {"promptTokenCount":5,"candidatesTokenCount":2,"thoughtsTokenCount":1}`, `modelVersion "gemini-x"` → `[Text, Text, ToolCall { id: "call_0", name: "lookup", .. }, Done { finish_reason: Some(Stop), usage: Some(TokenUsage { input: 5, output: 3, reasoning: 1, ..TokenUsage::default() }), response_model: Some("gemini-x") }]`.
- `gemini_block_is_a_content_filter_finish`: one chunk `{"promptFeedback":{"blockReason":"SAFETY"}}` → `[Done { finish_reason: Some(ContentFilter), .. }]`.
- `a_non_200_answer_is_a_provider_error`: status 429, body `{"error":{"type":"rate_limit_error","message":"SECRET-PAYLOAD"}}` (Anthropic) → `stream_chat` is `Err(Provider { status: 429, class: Retryable, .. })`, its `Debug` and `Display` have no `SECRET-PAYLOAD`; with native mode and a response format, a 400 carries the structured-output hint; without a response format it carries none.
- `an_idle_stream_fails_with_a_timeout`: idle 150 ms; one OpenAI content chunk, then `Sleep(600 ms)` → the first event arrives, then `Err(Transport(Timeout))`, then `Ok(None)`.
- `an_answer_over_4_mib_fails`: five OpenAI chunks each with 1 MiB of content → `Err(Transport(TooLarge { limit: 4194304 }))` before `Done`.
- `invalid_json_is_a_bad_stream`: `data: {nope` → `Err(BadStream)`.
- `a_stream_cut_before_its_end_is_a_bad_stream`: an OpenAI stream with one content chunk and no finish reason or `[DONE]` → `[Text]` then `Err(BadStream)`; an Anthropic stream without `message_stop` → `Err(BadStream)` after its text; a Gemini stream whose chunks carry no `finishReason` → `Err(BadStream)`.
- `events_before_an_error_are_delivered`: one chunk holding an Anthropic `text_delta` event and then an `error` event → `Text` first, then `Err(Provider { .. })`.
- `request_errors_come_before_sending`: a model `"a/b"` → `Err(InvalidModel)`; `max_tokens: 0` → `Err(InvalidRequest)`; a media part → `Err(UnsupportedMessage { .. })`; the server saw no request.
- `streams_leak_no_payload`: every event and error `Debug` of the tests above that carried `SECRET-PAYLOAD` text (put it in the text and tool arguments of the OpenAI test) contains no `SECRET-PAYLOAD`.
- All judge tests still pass unchanged.

### Done when

`CARGO_BUILD_JOBS=8 just check-rust` passes.

### Commit

`Stream chat calls to OpenAI, Anthropic and Gemini`

---

## V5.7 — Capture-mock tests, hostile streams, spec and decisions

**Goal:** the streamed calls are tested against `cetra-capture`'s mock provider and hostile event data, and the spec and decisions describe the layer.

### Edit

- `crates/eval/Cargo.toml` (dev-dependency), `Cargo.lock`
- `crates/eval/src/lib.rs`
- `crates/eval/src/chat/openai.rs`, `crates/eval/src/chat/anthropic.rs`, `crates/eval/src/chat/gemini.rs` (tests only)
- new `crates/eval/tests/chat_capture.rs`
- `docs/spec/06-evaluation.md`, `docs/mvp-decisions.md`
- `docs/plans/v-prompts/05-m5-provider-stream.md` (status line only)

`docs/roadmap.md` is not changed: track V's row is the coordinating session's, and M5 alone does not change the track's status.

### Steps

1. `crates/eval/Cargo.toml` `[dev-dependencies]`: add `cetra-capture.workspace = true` with the comment `# The chat layer's streams are tested against the capture mock provider (D-I-43).`. Run `cargo metadata --format-version 1 > /dev/null` once so `Cargo.lock` records it.
2. `lib.rs`: `#[cfg(test)] #[path = "../tests/chat_capture.rs"] mod chat_capture_tests;`.
3. **`tests/chat_capture.rs`**: start `cetra_capture::Mock::new(turns)` on `127.0.0.1:0` (as `crates/cli/tests/e2e.rs` does: `TcpListener::bind`, `axum::serve(listener, mock.router())` in a spawned task). Turns are built with `serde_json::from_value(json!({…}))` with `"ttft_ms": 0` and `"chunk_delay_ms": 0`. Bases: OpenAI `http://{addr}/v1`, Anthropic and Gemini `http://{addr}`. After each test, `mock.finish().errors` is empty.
4. **Hostile streams**: in each provider's `mod tests`, a proptest named `a_hostile_stream_event_decodes_or_fails_cleanly` (`#![proptest_config(config())]`, as the moved decode tests) that feeds `body(<provider>_reply())` bytes (from `crate::hostile_tests`), as one `SseEvent { event: None, data: String::from_utf8_lossy(&bytes).into_owned() }`, to a fresh `StreamState::default()` with a fresh `Sink::default()`, then calls `finish` when `on_event` returned `Ok`. Every outcome is `Ok` or one of `BadStream`, `Provider { .. }`, `Transport(TooLarge { .. })`; any other is a failure. For Anthropic, also feed each generated body as the `data` of events named `content_block_start`, `content_block_delta` and `message_delta` (the decoder reads the type from the data first, so set nothing else).
5. **Spec 06** (`docs/spec/06-evaluation.md`):
   - In § `llm_judge`, at the end of the "Requests (D159)" bullet, add: "Judges send these requests through the provider chat layer below, without streaming (D-V-29)."
   - Add a section `## Provider chat layer` before `## Library`, in the spec's style, saying: one request shape for the three adapters (model, canonical messages, tools, tool choice, response format through the connection's structured-output mode, parameters, `max_tokens`, timeouts); the streamed call (OpenAI `stream: true` with `stream_options.include_usage`, Anthropic Messages SSE, Gemini `streamGenerateContent?alt=sse`) and its events (text pieces, complete tool calls, a last event with the finish reason `stop|length|tool_call|content_filter|refusal|<provider token>`, usage in OTel semantics or none, and the response model); the limits (whole call ≤ 300 s, 60 s of silence fails, `max_tokens` 1–65,536 with 4,096 as the playground default, D-V-24; the caps of D-V-30); the mapping of D-V-31; errors (D160's classes; an error event in a stream is a provider error with the status its type stands for; nothing from the prompt or answer in an error); the egress (SSRF guard D166, no redirects, no proxy, a stream dropped closes the connection).
6. **`docs/mvp-decisions.md`** § Track V: add rows `D-V-29`, `D-V-30`, `D-V-31` after `D-V-28`, with the topics "Judges stay buffered", "Stream size caps", "Message and parameter mapping" and the decision texts of § Decisions this plan relies on (without the "(new, this plan)" marker).
7. Add `Status: implemented (2026-11-20).` and a blank line as the first two lines of this plan.

### Tests (`tests/chat_capture.rs`)

For each provider (`openai.chat`, `anthropic.messages`, `gemini.generate_content`):
Every turn JSON carries `"api"`: `"openai.chat"`, `"anthropic.messages"` or `"gemini.generate_content"` (`Turn.api` is required; `Finish` is `stop`, `tool_calls` or `length`).

- `<provider>_text_turn_streams_through_the_capture_mock`: turn `{"api": <api>, "model": "m-1", "message": {"text": "Hello there, world."}, "finish": "stop", "usage": {"input": 10, "output": 3}, "chunks": 3}` → the `Text` events joined are `Hello there, world.`, there are at least 2 `Text` events, and `Done` has `finish_reason: Some(Stop)`, `usage: Some(TokenUsage { input: 10, output: 3, ..TokenUsage::default() })`, `response_model: Some("m-1")`.
- `<provider>_tool_turn_streams_a_complete_call`: turn with `"api": <api>, "model": "m-1", "message": {"tool_calls": [{"id": "call_a", "name": "get_weather", "arguments": {"city": "Paris"}}]}`, `"finish": "tool_calls"` → exactly one `ToolCall { id: "call_a", name: "get_weather", arguments: {"city": "Paris"} }`, and `Done.finish_reason` is `Some(ToolCall)` for OpenAI and Anthropic and `Some(Stop)` for Gemini (Gemini reports `STOP` with function calls).
- `the_capture_mock_still_serves_buffered_judges`: one OpenAI judge call through `crate::judge::call` against a non-stream turn (as `judge_turn()` in `crates/cli/tests/e2e.rs`) parses to the numeric answer.

### Done when

```bash
export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH
```

```bash
CARGO_BUILD_JOBS=8 just check
```

passes, and `git status` is clean after the commit.

### Commit

`Test chat streams against the capture mock and document the chat layer`

---

## Final review

After V5.7, a Sonnet code review of the whole milestone (`git diff main...HEAD`), with these questions: does any judge request, answer or error differ from `main`; does any public type print payload in `Debug`; is every outbound call guarded (D166); can any network input panic a decoder; do the docs match the code. Fixes by a Haiku agent with an exact brief, then `just check` again.
