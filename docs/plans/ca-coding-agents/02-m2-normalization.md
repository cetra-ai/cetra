Status: plan, written 2026-11-20 from the code at `eb80b5a`; cold-reviewed once, findings settled here.

# CA · M2 — Normalizing coding-agent spans

This is milestone M2 of track CA. Its sources:
- the design, `docs/plans/ca-coding-agents/00-design.md`: § Agent detection and rules, § Data contract, § Normalization API;
- decisions D-CA-3 to D-CA-7;
- the spike results, research § 6.

M2 teaches `cetra-otel` to recognize Claude Code, Codex, Gemini CLI and Copilot spans and to normalize them. It then blesses M1's fixtures.

**Haiku implements it one phase at a time.** If a step is unclear, contradicts the code, or a captured attribute differs from what this plan or research § 6 says, **stop and report**; do not improvise.

## Prerequisites

- M1 is merged into this branch. These fixtures exist:
  - `fixtures/otlp/claude_code/bash-tool`, `fixtures/otlp/codex/exec-command` and `fixtures/otlp/gemini_cli/shell-tool` (awaiting bless);
  - `fixtures/otlp/genai/copilot-cli-tool` (blessed by M1).

  If any is missing, stop.
- Branch `ca-m2-normalization`, cut from the M1 branch.
- `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH` before `just`.
- Load rule: compile and test Rust once per phase, right before the commit.

## Read first

- Spec 02, all of it.
- `crates/otel/src`:
  - `normalize.rs`: `normalize`, `Batch`, `EncodedResource`, `accept_span` and the order of its steps, `SCHEMA_VERSION`, the test helpers;
  - `bag.rs`: `get`, `take`, `take_payload`, `drop_payload`, `into_attributes`;
  - `kind.rs`, and `context.rs`: `fill`, `ContextFields`, `USER_ID`;
  - `usage.rs`: `Usage`, `usage`, `ttft`, `take`, `read_millis`;
  - `value.rs`: `as_i64`, `as_str`;
  - `payload.rs`: `Io`, `Payload`, `assemble`, `Assembled`, `write`, `decode_json_text`, `non_message_payload`;
  - `convert/mod.rs`: the re-exports `gemini_contents` and `gemini_system_instruction`;
  - `conventions/openinference.rs`: `REDACTED`, `captured_text`, `raw_value`;
  - `cost.rs`: `apply`;
  - `warn.rs`.
- `crates/eval/src/trace.rs`, lines 133–148: how a hashed span id is built with the all-zero guard.
- `crates/otel/tests/{golden.rs,fixtures.rs,proptest.rs,support/mod.rs}` and `fixtures/otlp/README.md`.
- `crates/core/src/{span.rs,ids.rs,message.rs}`.
- The four coding-agent fixtures listed under Prerequisites.

## What it is, and what it is not

**When M2 is done:**
- Spans of the four agents are stored with the right kinds, names, tokens and payloads, plus `session_id`, `user_id` and the `metadata` keys `coding_agent`, `repository` and `branch`.
- Codex's internal spans are discarded and counted, and its kept spans are re-linked.
- `normalize` takes `agents: &AgentContexts` and stamps spans from it. The server passes an empty one until M3.
- Every fixture is blessed with `schema_version` 2.
- Spec 02 says all of this.

**Not in M2:**
- The server store and the route (M3).
- `cetra-core` type changes: none (D-CA-3).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-CA-3 | Detection in `cetra-otel`; agent rules run after the generic extraction and before cost |
| D-CA-4 | `metadata` keys `coding_agent`, `repository` (canonical), `branch`. The agent's own value replaces a client value only when the agent has one |
| D-CA-5 | Person order: account email, then the context's `user`, then account id, then the generic sources plus `enduser.pseudo.id` |
| D-CA-6 | Span shapes; Codex discard and re-link; Gemini conversion and resource drop; redaction; tool names |
| D-CA-7 | Cost from the price table |
| D-CA-9 | The context fills `repository` and `branch` over the span's own values, and `user_id` at its rank |
| D110 | An input count that excludes cache is added back |

**Settled here** (details, not design). N4 records the first one in D-CA-6 and the design:
- **Discards are counted.** They go in `Normalized.discarded: u64`, so that `spans + rejections + discarded` equals the input span count. The server ignores the field.
- **Fixtures show discards.** `expected.json` gains an optional `discarded` count, omitted when 0. The fixtures test allows span ids that are absent from `input.json` in group `codex` only, because the turn span is re-linked.
- **The API module** is `crates/otel/src/agents/` rather than the design's `agent_context.rs`; the public items are the same.
- **The Codex discard check runs before the id and time validations,** so an internal span is never counted as rejected.
- **`cetra.span_name`** is written only when a rename changes the name.
- **An agent that decides the kind** skips the generic `span_kind`, so `cetra.span.kind` on such spans is ignored. Span names the agent does not list fall through to the generic path.
- **Redaction** applies to Claude Code's `user_prompt` and to the tool output text. Codex spans carry no content.
- **The call id** (Gemini `gen_ai.tool.call_id`, Codex `call_id`) stays in `attributes` as an unread attribute.

## Versions

No new dependency (`sha2` is already a dependency of `cetra-otel`).

## Phases

```
N1 (API) ──► N2 (detection + Claude Code) ──► N3 (Codex, Gemini CLI, Copilot) ──► N4 (spec, schema_version, fixtures, bless)
```

---

## N1 — The API: `AgentContexts`, `canonical_repository`, the fourth argument, `discarded`

**Goal:** the public API of design § Normalization API exists, every caller passes an empty lookup, and no output changes.

**Edit:**
- `crates/otel/src/agents/mod.rs` (new)
- `crates/otel/src/lib.rs`
- `crates/otel/src/normalize.rs`
- `crates/otel/src/fuzz_entry.rs`
- `crates/otel/tests/golden.rs`
- `crates/otel/tests/proptest.rs`
- `crates/eval/src/trace.rs`
- `crates/server/src/ingest/mod.rs` (the `normalize` call at about line 373, the test call at about 686, and the `Normalized { … }` literal at about line 1281)
- `crates/server/src/ingest/otlp.rs`
- `crates/server/src/ingest/fixtures_tests.rs`
- `crates/server/src/routes/otlp.rs`
- `crates/server/src/eval/execute.rs` (the `Normalized { … }` literal at about line 1712)

First run `grep -rn "normalize(\|Normalized {" crates --include=*.rs`. If it finds another site, stop and report it.

**Steps:**
1. **`agents/mod.rs`** holds exactly the public items of design § Normalization API:
   - `AgentContext` (`repository`, `branch`, `user`, `updated_at`), deriving `Debug, Clone, PartialEq, Eq, Default`;
   - `AgentContexts`, wrapping a private `HashMap<(String, String), AgentContext>` and deriving `Debug, Clone, Default`, with `get`, `insert`, `remove`, `retain`, `len` and `is_empty`, as signed in the design;
   - `canonical_repository`.

   In `lib.rs`: `mod agents;` and `pub use agents::{AgentContext, AgentContexts, canonical_repository};`.
2. **`canonical_repository`**, in order:
   1. Trim, then lowercase.
   2. If the text contains `://`, drop everything through `://`. Otherwise, if an `@` comes before the first `:` and no `/` comes before that `:`, rewrite `<user>@<host>:<path>` to `<host>/<path>`.
   3. Drop everything through the last `@` before the first `/`.
   4. Split at the first `/` into host and path. Remove a `:<digits>` suffix from the host.
   5. Trim trailing `/`, then one `.git`, then trailing `/`.
   6. The segments are the non-empty `/`-separated pieces of the path. Return `None` when the host is empty or there are fewer than 2 segments; otherwise `host/seg1/…/segN`.
3. **`normalize.rs`**:
   - the signature becomes `normalize(request, ctx, prices, agents: &AgentContexts) -> Normalized`;
   - `Batch` gains `agents: &'a AgentContexts`;
   - `Normalized` gains `pub discarded: u64`, documented "Spans dropped on purpose (Codex internals, D-CA-6); neither rejected nor stored.", always 0 in this phase.
4. Every caller passes `&AgentContexts::default()`. Every `Normalized { … }` literal adds `discarded: 0`.
5. **`proptest.rs`**: the conservation check adds `first.discarded`, cast the way the test already casts the other two terms.

**Tests** (`agents/mod.rs`):
- `canonical_repository`:
  - `https://github.com/Acme/Widgets.git` → `github.com/acme/widgets`;
  - `https://tok@github.com/o/r/` → `github.com/o/r`;
  - `git@github.com:o/r.git` → `github.com/o/r`;
  - `ssh://git@host.example:2222/o/r` → `host.example/o/r`;
  - `https://gitlab.com/g/sub/proj.git` → `gitlab.com/g/sub/proj`;
  - `HTTPS://GitHub.com/O/R` → `github.com/o/r`;
  - `https://github.com/acme` → `None`;
  - `/home/u/repo` → `None`;
  - `""` → `None`.
- `agent_contexts_insert_get_remove_retain`.

**Done when:**
- `cargo test -p cetra-otel` passes.
- `cargo test -p cetra-eval` passes.
- `cargo test -p cetra-server ingest` passes.
- `cargo test -p cetra-server eval::execute` passes.
- `cargo clippy --workspace --all-targets -- -D warnings` passes.
- No fixture output changes.

**Commit:** `Add the AgentContexts lookup to normalize`

---

## N2 — Detection and the Claude Code rules

**Goal:** Claude Code spans normalize as design § Spans says.

**Edit:**
- `crates/otel/src/agents/mod.rs`
- `crates/otel/src/agents/claude_code.rs` (new)
- `crates/otel/src/agents/test_support.rs` (new, `#[cfg(test)]`)
- `crates/otel/src/normalize.rs`
- `crates/otel/src/context.rs` (`USER_ID` only)
- `crates/otel/src/usage.rs` (makes `take` and `read_millis` `pub(crate)`, nothing else)

**Steps:**
1. **`agents/mod.rs`:**

   ```rust
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   pub(crate) enum CodingAgent { ClaudeCode, Codex, GeminiCli, Copilot }
   impl CodingAgent { pub(crate) fn wire_name(self) -> &'static str } // "claude-code" | "codex" | "gemini-cli" | "copilot"

   pub(crate) const SPAN_NAME_ATTRIBUTE: &str = "cetra.span_name";
   pub(crate) mod key { pub(crate) const CODING_AGENT: &str = "coding_agent"; pub(crate) const REPOSITORY: &str = "repository"; pub(crate) const BRANCH: &str = "branch"; }
   pub(crate) fn is_redacted(text: &str) -> bool { matches!(text, "<REDACTED>" | "[REDACTED]") }

   /// D-CA-3. First match wins.
   pub(crate) fn detect_agent(service_name: Option<&str>, scope_name: Option<&str>, span_name: &str, attributes: &[KeyValue]) -> Option<CodingAgent>;

   /// Values the post-build step needs, read before the bag is consumed.
   #[derive(Debug, Default)]
   pub(crate) struct Finish {
       pub(crate) tool_name: Option<String>, // Claude: gen_ai.tool.name, else tool_name. Codex: tool_name. Gemini, Copilot: gen_ai.tool.name
       pub(crate) turn_id: Option<String>,   // Codex only: turn.id, else turn_id
   }
   pub(crate) fn finish_inputs(agent: Option<CodingAgent>, bag: &AttrBag) -> Finish;
   pub(crate) fn kind(agent: Option<CodingAgent>, span_name: &str) -> Option<SpanKind>;
   pub(crate) fn context(agent: Option<CodingAgent>, span_name: &str, bag: &mut AttrBag, fields: &mut ContextFields, agents: &AgentContexts);
   pub(crate) fn usage(agent: Option<CodingAgent>, span_name: &str, kind: SpanKind, bag: &mut AttrBag, warnings: &mut Warnings) -> Option<Usage>;
   pub(crate) fn ttft(agent: Option<CodingAgent>, bag: &mut AttrBag, warnings: &mut Warnings) -> Option<i64>;
   pub(crate) fn payload(agent: Option<CodingAgent>, span_name: &str, bag: &mut AttrBag, events: &[Event], assembled: &mut Assembled, warnings: &mut Warnings);
   pub(crate) fn finish(agent: Option<CodingAgent>, span_name: &str, kind: SpanKind, inputs: &Finish, built: &mut Span);
   ```

   - **`detect_agent`** reads the raw OTLP attributes slice, so it runs before the bag exists. In order:
     1. `span_name.starts_with("claude_code.")` gives Claude Code.
     2. A `service_name` that starts with `codex` gives Codex.
     3. `gen_ai.agent.name` equal to the string `gemini-cli`, or `service_name == Some("gemini-cli")`, gives Gemini CLI.
     4. Copilot, when any of these holds: `scope_name` starts with `github.copilot` or `copilot`; `service_name` is `github-copilot` or `copilot-chat`; any key starts with `github.copilot.`.
     5. Otherwise `None`.
   - Each function dispatches to the agent's module and returns `None` or does nothing when the agent has no rule.
2. **Integers and milliseconds.** Agent token counts are read with `bag.get`, then `value::as_i64`. On `Ok`, `bag.take` the key. On `Err`, leave the key and add `Warning::InvalidNumber`. Negative counts are invalid (rule 20). TTFT reuses usage.rs's `take(bag, key, warnings, read_millis)`; make both `pub(crate)`.
3. **Wiring in `accept_span`** (step numbers are those of the current function):
   1. **Detect.** At the top of `accept_span`, before the validity checks: `let agent = agents::detect_agent(resource.service_name.as_deref(), scope_name.as_deref(), &span.name, &span.attributes);`. N3 adds the Codex discard right after this line.
   2. **Kind.** `let span_kind = match agents::kind(agent, &span.name) { Some(kind) => kind, None => span_kind(&mut bag) };`.
   3. **Context.** After `fill`: `agents::context(agent, &span.name, &mut bag, &mut fields, batch.agents);`.
   4. **Usage.** `let counted = match agents::usage(agent, &span.name, span_kind, &mut bag, &mut warnings) { Some(u) => u, None => usage(/* existing call */) };`. `agents::usage` returns `None` unless `span_kind` is `Llm` and the agent has its own tokens on this span.
   5. **TTFT.** After the generic `ttft`, when it gave `None`: `agents::ttft(agent, &mut bag, &mut warnings)`.
   6. **Payload.** After `payload::assemble`: `agents::payload(agent, &span.name, &mut bag, &span.events, &mut assembled, &mut warnings);`.
   7. **Before `into_attributes`:** `let finish = agents::finish_inputs(agent, &bag);`.
   8. **After `payload::write(&mut built, …)`:** `agents::finish(agent, &span.name, span_kind, &finish, &mut built);`.
4. **Claude Code** (`claude_code.rs`).
   - **Kind:** `claude_code.interaction` → `Agent`; `claude_code.llm_request` → `Llm`; `claude_code.tool` → `Tool`; any other `claude_code.*` → `Span`.
   - **Usage** on `claude_code.llm_request`:
     - read `input_tokens`, `output_tokens`, `cache_read_tokens` and `cache_creation_tokens`;
     - if `input_tokens` and `output_tokens` are both absent, return `None`;
     - `cache_read = cache_read_tokens`, `cache_write = cache_creation_tokens`;
     - `input = input_tokens.map(|i| i.saturating_add(cache_read.unwrap_or(0)).saturating_add(cache_write.unwrap_or(0)))`, so the cache is added back (D110);
     - `output = output_tokens`, `reasoning = None`;
     - `total = input + output` (saturating) when both are present, else `None`.
   - **TTFT:** `ttft_ms` through `take(…, read_millis)`.
   - **Context:**
     - The context is `agents.get("claude-code", s)` when `fields.session_id` is `Some(s)`.
     - `user_id`, in order: `user.email` (taken, trimmed, lowercased); the context's `user`; `user.account_uuid`, then `user.account_id` (taken); the existing `fields.user_id`.
     - `repository`: the context's `repository`, else `canonical_repository` of `vcs.repository.url.full`.
     - `branch`: the context's `branch`.
     - Insert each into `fields.metadata` only when the value is `Some`.
   - **Payload:**
     - **`claude_code.interaction`:** if `user_prompt` is a non-empty string and not `is_redacted`, `io.input = Some(Payload::Text(prompt))`. In either case `bag.drop_payload("user_prompt")`.
     - **`claude_code.tool`:**
       - If any of `full_command`, `file_path`, `skill_name` and `subagent_type` is a non-empty string, `io.input = Some(Payload::Json(object))` with those keys, and each key is taken.
       - The first event named `tool.output` gives `io.output = Payload::Text`, from its `output`, else `diff`, else `content`: the first non-empty string that is not `is_redacted`. Push the event's index to `assembled.dropped_events` in every case.
     - **Errors.** When `io.input` or `io.output` is set here, `assembled.io.errors.retain(|e| !e.ends_with(":input"))` (or `":output"`). Error strings are `messages:<convention>:<field>`.
   - **Finish.** On `claude_code.tool` with `finish.tool_name = Some(t)` and `t != built.name`: insert `cetra.span_name` with the old name, then `built.name = t`.
5. **Common to every detected agent**, in `agents::context`: `fields.metadata.insert("coding_agent", wire_name)`. The agent's own `repository` and `branch` replace a client value only when the agent has a value; a client value is never removed.
6. **`context.rs`, `USER_ID`:** append `span("enduser.pseudo.id", Convention::Genai)` last.
7. **`agents/test_support.rs`** (`#[cfg(test)] pub(crate)`) holds the builders the agent tests share:

   ```rust
   pub(crate) fn kv(key: &str, value: AnyValue) -> KeyValue;
   pub(crate) fn s(text: &str) -> AnyValue;
   pub(crate) fn i(n: i64) -> AnyValue;
   pub(crate) fn span(name: &str, attributes: Vec<KeyValue>, events: Vec<Event>) -> opentelemetry_proto::…::Span; // valid ids and times inside the context's window
   pub(crate) fn request(service_name: &str, scope_name: &str, spans: Vec<…::Span>) -> ExportTraceServiceRequest;
   pub(crate) fn run(request: &ExportTraceServiceRequest, agents: &AgentContexts) -> Normalized; // normalize with the normalize.rs test context and empty prices
   pub(crate) fn run_priced(request: &ExportTraceServiceRequest, rows: Vec<PriceRow>) -> Normalized;
   ```

   Copy the context and id helpers from `normalize.rs`'s test module; do not move them.

**Tests** (in `claude_code.rs` and `agents/mod.rs`):
- `detect_agent` returns each agent from its signal, `None` for a plain GenAI span, and Copilot from a `github.copilot.turn_id` key alone.
- `claude_llm_request_tokens_add_cache_back`: 100, 300, 20 and 12 give input 420, cache read 300, cache write 20, output 12, total 432, kind `llm`.
- `claude_llm_request_output_only`: only `output_tokens = 5` gives output 5, and input and total `None`.
- `claude_llm_request_without_tokens_uses_the_generic_path`: none of the four keys but `gen_ai.usage.input_tokens = 7` gives input 7.
- `claude_llm_request_is_priced_with_cache`: `run_priced` with a row for `claude-sonnet-4-5` (input 3, output 15, cache_read 0.3, cache_write 3.75 per Mtok), and the span's `gen_ai.request.model = claude-sonnet-4-5` with the four counts above, gives `cost_total` computed by spec 02's formula. Write the expected value as the formula's arithmetic in the test.
- `claude_ttft_from_ms`: `ttft_ms = 12` gives `ttft_ns = 12_000_000`.
- `claude_interaction_prompt_is_text_input`, and `claude_redacted_prompt_is_no_input`: no input, and `user_prompt` absent from `attributes`.
- `claude_tool_is_named_after_the_tool`: the name is `Bash`; `attributes["cetra.span_name"] = "claude_code.tool"`; the input is `{"full_command":"echo hello"}`; the output is `hello`; no `tool.output` event remains.
- `claude_tool_without_events_has_no_output`.
- `claude_children_are_spans`: `claude_code.tool.execution` is kind `span`, its name unchanged, with no `cetra.span_name`.
- `claude_metadata_from_vcs`: repository `github.com/acme/widgets`, `coding_agent = claude-code`, no `branch`.
- `claude_context_stamps_branch_repository_and_user`: a context for `("claude-code", "s1")` with `gitlab.com/x/y`, `main` and `dev@example.com`, on a span with `session.id = s1`, github `vcs.*` and `user.id = anon`, gives `gitlab.com/x/y`, `main` and `dev@example.com`.
- `claude_email_beats_the_context_user`: `user.email = A@B.example` gives `a@b.example`.
- `claude_account_uuid_when_no_email_and_no_context`: `user.account_uuid = u1` and `user.id = anon` give `u1`.
- `claude_anonymous_id_last`: only `user.id = anon` gives `anon`.
- `no_agent_no_coding_agent_key`.
- `client_branch_kept_when_the_agent_has_none`: a Claude span with `cetra.metadata.branch = x` and no context keeps `branch = x`.
- `copilot_pseudo_user_id`: `enduser.pseudo.id` fills `user_id` on a span with no other user source.

**Done when:**
- `cargo test -p cetra-otel --lib` passes.
- `cargo clippy -p cetra-otel --all-targets -- -D warnings` passes.

The golden test may now fail on `genai/copilot-cli-tool`, through `coding_agent` and the new `user_id` source: N4 re-blesses it.

**Commit:** `Normalize Claude Code spans`

---

## N3 — Codex, Gemini CLI and Copilot

**Goal:** the other three agents.

**Edit:**
- `crates/otel/src/agents/mod.rs`
- `crates/otel/src/agents/codex.rs` (new)
- `crates/otel/src/agents/gemini_cli.rs` (new)
- `crates/otel/src/agents/copilot.rs` (new)
- `crates/otel/src/normalize.rs`
- `crates/otel/tests/proptest.rs`

**Steps:**
1. **The Codex discard.**
   - `accept_span` takes a `tally: &mut Tally` in place of `rejections: &mut Rejections`. Define it in `normalize.rs`: `struct Tally { rejections: Rejections, discarded: u64 }`. This keeps the argument count at 7.
   - Right after `detect_agent`, before the validity checks: `if agent == Some(CodingAgent::Codex) && !codex::kept(&span.name, &span.attributes) { tally.discarded += 1; return None; }`.
   - `kept` is true for `session_task.turn`, and for a span whose `codex.tool.phase` is the string `dispatch` and whose `tool_name` is a non-empty string.
   - `normalize` builds `Normalized { rejections: tally.rejections, discarded: tally.discarded, … }`.
2. **Codex rules** (`codex.rs`):
   - **Kind:** `session_task.turn` → `Llm`; any other kept span (a dispatch) → `Tool`.
   - **Context.**
     - `session_id`: `conversation.id`; on `session_task.turn`, else `thread.id`. The first non-empty **string** wins, and it is taken. An integer `thread.id`, the OS thread, is ignored.
     - `request_model`: `model` when not set (taken).
     - The context lookup is `("codex", session_id)`, and fills `repository`, `branch` and `user_id` (rank 2; Codex has no rank 1 or 3).
   - **Usage** on `session_task.turn`. Read under `codex.turn.token_usage.`:
     - `input_tokens` (includes cache), `cached_input_tokens` → cache read, `cache_write_input_tokens` → cache write;
     - `output_tokens` (includes reasoning), `reasoning_output_tokens`;
     - `total_tokens`, else input + output.

     `None` when input and output are both absent.
   - **Finish.**
     - On `session_task.turn` with `finish.turn_id = Some(t)`: `built.span_id = relink(t)` and `built.parent_span_id = None`.
     - On a dispatch span with `Some(t)`: `built.parent_span_id = Some(relink(t))`, and the rename to `finish.tool_name` with `cetra.span_name` when the name changes.

     `relink` follows `crates/eval/src/trace.rs:133-148`:

     ```rust
     /// D-CA-6: the first 8 bytes of SHA-256("cetra.codex.turn" ‖ turn id), never all zero.
     pub(crate) fn relink(turn_id: &str) -> SpanId
     ```

     - Hash `b"cetra.codex.turn"` followed by the turn id's bytes, and copy the first 8 bytes.
     - If they are all zero, set the last byte to 1.
     - Build the id the way trace.rs does (`SpanId::from_bytes`), with `#[allow(clippy::expect_used)]` and the invariant comment, as there.
3. **Gemini CLI rules** (`gemini_cli.rs`):
   - **Kind:** `llm_call` → `Llm`; `tool_call` → `Tool`; `schedule_tool_calls` → `Chain`; `user_prompt` and `agent_call` → `Agent`. Other names give `None`.
   - **Context:** the lookup is `("gemini-cli", session_id)`, filling `repository`, `branch` and `user_id` (rank 2).
   - **Payload on `llm_call`.** Decode each attribute with `payload::decode_json_text`.
     - `gen_ai.input.messages` as an array goes through `crate::convert::gemini_contents`. `Ok` with at least one message sets `io.input = Payload::Messages`.
     - `gen_ai.output.messages`: an object is wrapped in a one-element array first, then the same conversion sets `io.output`.
     - `gen_ai.tool.definitions`: an array of objects with a `functionDeclarations` array. The flattened elements replace `io.tool_definitions`.
     - `gen_ai.system_instructions`: `io.system_instructions = Some(crate::convert::gemini_system_instruction(&value))`.
     - Each attribute used is `drop_payload`ed, and the errors of the fields set are removed as in N2.
     - A conversion error leaves the generic result.
   - **Payload on `tool_call` and `schedule_tool_calls`.** `gen_ai.input.messages` and `gen_ai.output.messages`, decoded, become `Payload::Json`. When the text is not JSON, use `payload::non_message_payload` (rule 10). Both are `drop_payload`ed.
   - **Finish.**
     - On `tool_call`: the rename to `finish.tool_name`.
     - On every Gemini span: remove `process.command_args`, `process.command` and `process.owner` from `built.resource_attributes`.
4. **Copilot rules** (`copilot.rs`):
   - **Kind:** no rule of its own (`None`).
   - **Context:**
     - `repository`: `canonical_repository(github.copilot.git.repository)`, else `canonical_repository(copilot_chat.repo.remote_url)`;
     - `branch`: `github.copilot.git.branch`, else `copilot_chat.repo.head_branch_name`;
     - no lookup (Copilot has no hook).
   - **Finish:** a span of kind `tool` with `finish.tool_name` is renamed.
5. **`proptest.rs`**: add to the generators the span names `claude_code.interaction`, `claude_code.llm_request`, `claude_code.tool`, `session_task.turn`, `fs.get_metadata`, `exec_command`, `llm_call` and `tool_call`. Add the resource service names `codex_exec`, `gemini-cli` and `claude-code`, and the attribute keys `codex.tool.phase` (value `dispatch`), `tool_name`, `turn.id`, `input_tokens` and `codex.turn.token_usage.input_tokens`. Follow how the file already picks names and promoted keys. Conservation and no-panic now cover agent spans.

**Tests:**
- `codex_internal_spans_are_discarded_and_counted`: `fs.get_metadata`, `turn/start` and `handle_responses` under service `codex_exec` give 0 spans, `discarded == 3` and 0 rejections.
- `codex_discard_before_validation`: a Codex `auth` span with a start before 2000 is discarded, not rejected.
- `codex_turn_is_the_llm_root`: `session_task.turn`, `turn.id = t1`, `thread.id` both as int 32 and as string `conv-1` (string last), `model = gpt-5.5-codex`, and token attributes as `intValue`s 800, 600, 0, 60, 20, 860. It gives kind `llm`, no parent, `span_id == relink("t1")`, `session_id = conv-1`, input 800, cache read 600, output 60, reasoning 20, total 860, and the request model.
- `codex_tool_hangs_under_its_turn`: `exec_command` with `codex.tool.phase = dispatch`, `tool_name = exec_command`, `turn.id = t1` and `conversation.id = conv-1` gives parent `relink("t1")`, kind `tool`, the name unchanged, and no `cetra.span_name`.
- `relink_is_stable`: `relink("t1")` equals a fixed hex. Compute it once and paste it in.
- `gemini_llm_call_messages_are_converted`: the input array has a user text; the output is an object `{"role":"model","parts":[{"functionCall":{"name":"run_shell_command","args":{"command":"echo hello"}}}]}`. This gives canonical messages, an assistant output with a `tool_call` part, and no `cetra.normalize_error`.
- `gemini_tool_definitions_are_flattened`.
- `gemini_tool_call_is_named_and_json`.
- `gemini_process_attributes_are_dropped`.
- `copilot_tool_is_renamed_and_git_metadata_set`.
- `proptest` passes.

**Done when:**
- `cargo test -p cetra-otel --lib` passes.
- `cargo test -p cetra-otel --test proptest` passes.
- `cargo clippy -p cetra-otel --all-targets -- -D warnings` passes.

**Commit:** `Normalize Codex, Gemini CLI and Copilot spans`

---

## N4 — Spec, `schema_version` 2, fixtures, bless

**Goal:** the specs say what N1–N3 built; fixtures carry discards; every expected output is blessed.

**Edit:**
- `docs/spec/02-ingestion.md`, `docs/spec/01-data-model.md`
- `docs/plans/ca-coding-agents/00-design.md` (§ Spans, the discard bullet only) and `docs/mvp-decisions.md` (the D-CA-6 row only: "…neither stored nor rejected; counted in `Normalized.discarded`…")
- `crates/otel/src/normalize.rs` (`SCHEMA_VERSION` only)
- `crates/otel/tests/support/mod.rs`, `crates/otel/tests/golden.rs`, `crates/otel/tests/fixtures.rs`
- `fixtures/otlp/README.md`
- `fixtures/otlp/**/expected.json` (written by the bless)
- `fixtures/otlp/{claude_code,codex,gemini_cli}/*/case.json` and `tools/capture/ts/scenarios/{claude_code,codex,gemini_cli}/*/scenario.json` (`spec_refs` only)
- `crates/eval/src/snapshots/judge_trace_*.snap`, `crates/cli/tests/snapshots/e2e__sdk-*.snap` (regenerated)
- `CHANGELOG.md`

**Steps:**
1. `SCHEMA_VERSION = 2`.
2. **Discards in fixtures:**
   - `support/mod.rs`'s `Expected` gains `#[serde(default, skip_serializing_if = "is_zero")] discarded: u64`.
   - `golden.rs` writes and compares it from `Normalized.discarded`.
   - `fixtures.rs`: the count invariant (about line 448) becomes `expected spans + rejected + discarded == input spans`. The check that every expected `(trace_id, span_id)` appears in `input.json` (about line 479) skips cases in group `codex`.
   - `fixtures/otlp/README.md`: document `discarded`, and the `codex` exemption from the id check (Codex spans are re-linked, D-CA-6), next to the rules at lines 63 and 74.
3. **Spec 02.**
   - § Pipeline: the signature line becomes `normalize(request, ingest_context, price_table, agent_contexts) -> Normalized { spans, media, rejections, discarded }`.
   - New section `## Coding agents`, before `## Media extraction`. It states:
     - detection (N2 step 1);
     - per agent, the kind, usage, payload, context and finish rules of N2 and N3, as one table per agent;
     - the person order (D-CA-5);
     - the D-CA-4 keys and `canonical_repository` (N1 step 2);
     - the stamp from `agent_contexts` (D-CA-9);
     - the Codex discard (counted in `discarded`, not rejections) and `relink`;
     - the redaction placeholders;
     - the "Settled here" rules of this plan.
   - § Context fields: `user_id` gains `enduser.pseudo.id` last.
   - § Testing table gains the row `Coding agents | Codex CLI (captured) | Claude Code (captured) | Gemini CLI (captured) | Copilot (hand-written)`.
4. **Spec 01**, § Span:
   - the `metadata` row adds "coding-agent spans carry `coding_agent`, `repository` and `branch` (02 § Coding agents)";
   - the `attributes` row adds "`cetra.span_name`: the original name of a renamed tool span".
5. **`spec_refs`:** `"02 § Convention detection"` becomes `"02 § Coding agents"` in the three `case.json` and the three `scenario.json`.
6. **Bless.** Run `just fixtures-bless`, then review:
   - **Every other case.** For every case outside `claude_code`, `codex`, `gemini_cli` and `genai/copilot-cli-tool`, the only change must be `schema_version` 1 → 2. Check it with a small script that loads the old (`git show HEAD:path`) and new versions, sets the version equal, and compares. List any other difference, and stop.
   - **The coding-agent cases.**
     - Claude Code: 6 spans. `interaction` is `agent` with the prompt. The two `llm_request` are `llm` with inputs 420 and 450 (cache included). `Bash` is `tool`. `coding_agent = claude-code`. Repository `github.com/acme/widgets`.
     - Codex: only the turn and `exec_command` remain, and `discarded` is the rest of the captured spans. The turn has the summed tokens and no parent; the tool's parent is the turn.
     - Gemini CLI: the messages are canonical and there is no `cetra.normalize_error`.
     - Copilot: `user_id = pseudo-user-1`, the tool is named `bash`, `coding_agent = copilot`.

     If a captured attribute differs from what N2 and N3 expect, stop and report. Report the per-case span counts.
7. **Snapshots.** Regenerate `crates/eval/src/snapshots/judge_trace_*.snap` with `cargo insta test -p cetra-eval --accept`, or the way that crate's tests document it. Each diff must be only `schema_version`. For `crates/cli/tests/snapshots/e2e__sdk-*.snap`, run `just e2e` only if it is cheap here; otherwise edit `"schema_version": 1` → `2` in those snapshots, and report that `just e2e` was not run.
8. **Design and decisions.** In the design's § Spans bullet "Codex's internal spans are discarded", replace "neither stored nor rejected: they do not appear in `partial_success`, and they are not units" with "neither stored nor rejected, and not units; `normalize` counts them in `Normalized.discarded`". Make the same change in D-CA-6.
9. **`CHANGELOG.md`** § `[Unreleased]` › `### Added`: `- Claude Code, Codex CLI, Gemini CLI and GitHub Copilot spans are normalized: tool spans are named after their tool, tokens and cost are filled, and traces carry the agent, repository, branch and person.`

**Done when:**
- `cargo test -p cetra-otel` passes.
- `cargo test -p cetra-eval` passes.
- `export PATH=…; just check` passes.

**Commit:** `Document coding-agent normalization and bless the fixtures`
