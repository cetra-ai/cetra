# V · M6 — Playground API: streamed runs, reserved environment, shared limits

Implementation plan for milestone M6 of track V. It has 5 phases, **V6.1 to V6.5**, one commit each, done in order. **Haiku** implements each phase with no context beyond this plan, the design doc and the files it names; **Sonnet** reviews the milestone. If this plan and the code disagree, **STOP and report** instead of guessing.

**Checks (the user's rule, 2026-11-20):** no phase compiles, tests or builds anything. Each phase writes its code and tests carefully and commits. When V6.5 is committed, the milestone runs the full checks once (`just check`), and fixes what fails.

**Prerequisites** (each must print at least one line, else STOP):

```bash
grep -n "pub async fn stream_chat" crates/eval/src/chat/mod.rs
grep -n "pub mod plain_template" crates/eval/src/lib.rs
grep -n "pub const EVAL: &str" crates/core/src/names.rs
grep -n "struct DemoLimits" crates/server/src/auth/limits.rs
grep -n "pub(crate) struct ConnectionLimit" crates/server/src/eval/dispatch.rs
```

**Read first** (every phase): `AGENTS.md`; `docs/plans/v-prompts/00-design.md` D-V-19 to D-V-31; `docs/plans/v-prompts/05-m5-provider-stream.md` § the chat layer's API; this file up to § V6.1, then your phase.

## What M6 is, and what it is not

M6 delivers:
- `POST /api/v1/projects/{project}/playground/runs`: one model run answered as `text/event-stream`, with `{{var}}` rendering, placeholders, tools, tool choice and a response format, through the org's LLM connection and M5's `stream_chat`;
- every run traced into the project as one `llm` span in the reserved environment `cetra-playground`, priced, not billed;
- three **shared APIs** that track AS (the assistant) will call too:
  1. `cetra_core::names::environment::{RESERVED, is_reserved, external_alias}`: the reserved-environment test, applied everywhere `cetra-eval` was special-cased;
  2. `crate::ingest::billable_spans`: the units rule (spans in a reserved environment are not units);
  3. `crate::eval::interactive::acquire`: an interactive call's connection slot and rate token, waiting at most a given time, ahead of queued rule jobs;
- per-user and per-org playground limits, `ProjectPerm::RunPlayground`, and an end-to-end test against the capture mock.

M6 does not add the web page (M7), dataset runs (design Q1), or tool execution (D-V-25).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-V-19 | the M5 chat layer streams; judges stay buffered (D-V-29) |
| D-V-20 | SSE answering a POST; events `start`, `text`, `tool_call`, `done`, `error`, and a ping every 15 s; one request per model column; dropping the request cancels the provider call |
| D-V-21 | the run request: connection, model, messages (canonical), variables, placeholders, params, tools, tool choice, response format, source |
| D-V-22 | one `llm` span per run in the reserved environment `cetra-playground`, priced with the project's table, metadata `cetra.playground.run_by` and the source; reserved like `cetra-eval` everywhere |
| D-V-23 | playground spans are not units; no units-cap check |
| D-V-24 | 60 runs/min and 4 at once per user, 16 at once per org; the connection's slot and token, waiting ≤ 10 s; 300 s per call, 60 s idle; `max_tokens` 4 096 default, ≤ 65 536 |
| D-V-25 | text, tool-call and tool-result parts; media refused; tools never run |
| D-V-26 | `ProjectPerm::RunPlayground`: member, session or read-write PAT, never an API key |
| D-V-30, D-V-31 | the chat layer's caps and message mapping (M5) |

New decisions made by this plan (V6.5 records them in `docs/mvp-decisions.md` § Track V and the design's tables; check the numbers are free with `grep -n "D-V-3[3-7]" docs/mvp-decisions.md`, and if M4's D-V-32 is not there yet, still use these numbers):

| ID | Decision |
|---|---|
| D-V-33 | **Reserved environments, one test.** `cetra_core::names::environment` gains `PLAYGROUND = "cetra-playground"`, `PLAYGROUND_EXTERNAL = "cetra-playground-external"`, `RESERVED: &[&str] = &[EVAL, PLAYGROUND]`, `is_reserved(env) -> bool` and `external_alias(env) -> Option<&'static str>`. Every place that compared with `EVAL` to hide, skip, refuse or not bill a trace uses `is_reserved`; the OTLP handler rewrites any reserved name to its `-external` alias. The judge-trace builder and the GDPR judge cascade (which find judge traces by their target) stay `EVAL`-specific. A later track adds its environment to `RESERVED` and gets every rule. |
| D-V-34 | **The units rule is one function**, `crate::ingest::billable_spans(&[Span]) -> usize` (spans whose environment is not reserved), used by `Ingest::commit`. |
| D-V-35 | **Interactive connection permits.** `crate::eval::interactive::acquire(state, connection, wait)` waits up to `wait` for the connection's concurrency slot (tokio's FIFO semaphore puts the waiter ahead of the dispatcher's polling `try_acquire`), then for a rate token, and returns a permit that frees the slot on drop; it fails with `Busy` at the deadline. The dispatcher and the interactive path share one `ConnectionLimit` per connection. The playground passes 10 s. |
| D-V-36 | **Playground runs and connection status.** A run uses the connection whatever its `status` (as connection tests and test runs do) and never marks it `failing`: a pause-class error is reported to the user in the `error` event only. |
| D-V-37 | **Request shape.** `messages` are canonical messages (spec 01) or `{type: "placeholder", name}` entries; a placeholder is replaced, in place, by the messages given under its name in `placeholders`; every `text` part of the resulting messages that came from `messages` (not from a placeholder) is rendered with `variables` (D-V-3); a missing variable or placeholder is a 400 before the stream starts. Trace ids are random. The run's trace is committed after the stream ends, also when the client cancelled (status error, message `cancelled`). |

## Versions

No new workspace dependency (`axum` SSE, `tokio`, `futures` exist). `crates/server` gains the existing workspace crate `cetra-capture` as a dev-dependency (Precision 1).

## Phase graph

Strictly in order: V6.1 (reserved environments, units rule) → V6.2 (permit, limits, permission) → V6.3 (cetra-eval: rendering and the run trace builder) → V6.4 (the route) → V6.5 (end-to-end test, docs, decisions).

## Conventions for every phase

- AGENTS.md: no `unwrap`/`expect` outside tests (clippy denies them even with a comment in `cetra-server`; return errors); never panic on input; logs carry ids, sizes, counts, statuses and error classes, never prompt text, messages, outputs, tool arguments, variable values or keys.
- Commit once per phase with the exact subject, staging only your files by path (never `git add -A`); no attribution lines; no build or test runs (§ Checks).

## Precisions (from the cold review; they override the phase text where they differ)

1. **Test dependency.** V6.4 adds `cetra-capture.workspace = true` under `[dev-dependencies]` of `crates/server/Cargo.toml` (a `tools/` crate as a test dependency, D-I-43); `crates/server/Cargo.toml` and `Cargo.lock` join V6.4's Edit list. The server's own `MockLlm` cannot stream. Test 8's provider error still uses a hand-written axum stub.
2. **Static schema.** `fixtures/api/static/schema.json` holds the catalog's `environment` description verbatim and is hand-maintained: V6.1 step 7 edits it with exactly the new `catalog.rs` sentence, and `fixtures/api/README.md` mentions `cetra-playground` where it mentions `cetra-eval`. Both join V6.1's Edit list.
3. **Request checks.** Do not add a `check_chat_request` wrapper. In V6.3 make `check_request` itself `pub` in `chat/mod.rs` and export it from `lib.rs` as `check_chat_request` (`pub use chat::check_request as check_chat_request;`). The handler (V6.4 step 5) first validates per field, with 400s in this order: `messages` not empty (`messages`: "must not be empty"); each tool name `[A-Za-z0-9_-]{1,64}` (`tools[i].name`); tool names unique (`tools[i].name`: "must repeat no tool name"); a `tool_choice` naming a tool exists in `tools` (`tool_choice`); no tool named `submit_answer` when `response_format` is set (`tools[i].name`: "submit_answer is reserved"); the model is 1–256 bytes with no whitespace, control character or `/ \ % ? #` (`model`). Then `check_chat_request` as a backstop: `UnsupportedMessage { index, reason }` → field `messages[index]` with `reason`; anything else → field `request` with the error's `Display`. A tool named `auto`, `none` or `required` cannot be forced by name through `tool_choice` (documented in the OpenAPI description).
4. **Limits types.** `PLAYGROUND_PER_MINUTE: u32 = 60`, `PLAYGROUND_USER_IN_FLIGHT: u32 = 4`, `PLAYGROUND_ORG_IN_FLIGHT: u32 = 16`; `rate: KeyedLimiter::new(Bucket { capacity: PLAYGROUND_PER_MINUTE, per: Duration::from_secs(60) }, MAX_KEYS)` as `McpLimits::new` does; `InFlight::new(max: u32)`.
5. **Ingest context and commit.** `state.ingest.context(&project, None, ended_ns)` (it takes the `&ProjectRow`). Commit with `crate::eval::execute::commit_judge_trace(&state, &project, ended_ns, normalized)` (make it `pub(crate)` if it is not), which does `reserve_exact(0)` and `commit_in_task`. Read `cost_usd` from `normalized.spans[0].cost_total` before the value moves into the commit.
6. **Clocks.** `TestApp`'s clock is manual and frozen. The run task takes `started_ns = state.clock.now_ns()` and a `tokio::time::Instant` `t0`; `ended_ns = started_ns + t0.elapsed()` in ns; `latency_ms` and `ttft_ms` come from `Instant`s. `interactive::acquire` uses `tokio::time::Instant` and `tokio::time::timeout_at`. Route tests call `app.state.clock.set(<real now>)` before seeding, as `dispatch.rs`' `seeded_app` does. Rate and token tests are fine with a frozen clock (nothing refills).
7. **Metadata keys are attributes.** `PLAYGROUND_RUN_BY` and `PLAYGROUND_SOURCE` (V6.3) go into `names.rs`' `ATTRIBUTES`, its catalog tuples and branded list, and two rows of `website/src/content/docs/docs/reference/attributes.md` (`cetra.playground.run_by`: the user who ran a playground run; `cetra.playground.source`: `prompt:<name>@<version>` or `span:<trace>:<span>`). `attributes.md` and `crates/core/src/names.rs` join V6.3's Edit list. `cetra-playground` (with a hyphen) does not need an attributes row.
8. **Keep test imports.** In `ingest/mod.rs` and `trace_load.rs`, change only the production comparisons; tests that use `EVAL`/`EVAL_ENVIRONMENT` keep their imports.
9. **Trace builder helpers and keys.** `trace.rs` has `string_attr`, `int_attr`, `metadata_attr`, `usage_attrs`, `otel_span` and `error_status`. V6.3 adds, in `trace.rs` (visibility `pub(crate)`), `double_attr(key, f64)` and `string_array_attr(key, &[String])` built like `string_attr` with `AnyValue` `DoubleValue` / `ArrayValue`. Keys: tool definitions under `gen_ai.tool.definitions` as a JSON-array string of `{name, description, parameters}` (`crates/otel/src/conventions/genai.rs`); request params as `gen_ai.request.temperature`, `gen_ai.request.top_p`, `gen_ai.request.max_tokens`, `gen_ai.request.stop` (array), `gen_ai.request.presence_penalty`, `gen_ai.request.frequency_penalty`, `gen_ai.request.seed`, `gen_ai.request.reasoning_effort` (normalization collects any `gen_ai.request.<name>` into `request_params`); the finish reason is set on the assistant `Message` (`finish_reason`) inside `cetra.output`, not as a span attribute.
10. **Run task edge cases.** The output message is `None` when both the text and the tool calls are empty. The handler formats `source` as `prompt:<name>@<version>` or `span:<trace hex>:<span hex>` before spawning. On a commit failure the task still sends `done` (or `error` when the run failed); the failure is logged with ids only.
11. **Docs.** V6.5 also adds a `RunPlayground` row to `docs/spec/08-auth.md`'s permission table (member, admin, owner; not viewer) and a sentence; and adds `cetra-playground` beside `cetra-eval` where these pages call it reserved: `website/src/content/docs/docs/concepts/data-model.mdx`, `evaluation/evaluators.mdx`, `reference/python/index.mdx`, `tracing/metadata-and-tags.mdx`.
12. **Serde and OpenAPI.** A `messages[i]` entry that matches neither variant is the 400 `messages[i]`: "must be a message or a placeholder". `PlaygroundRunRequest.messages` uses `#[schema(value_type = Vec<Object>)]`; register `PlaygroundRunRequest`, `PlaygroundParams`, `PlaygroundTool`, `PlaygroundResponseFormat`, `PlaygroundSource` in `collect_schemas`. The request types derive `Serialize` too (the request-example round-trip needs it), with `skip_serializing_if` on optional and empty fields so the canonical example round-trips; register the example in both tables of `tests/api_examples.rs` (the round-trip list and `requests`).
13. **SSE helpers.** Make `sse_response`, `send`, `json_event` and `ping_event` `pub(crate)` in `routes/streams.rs`; `json_event(name, id, value)` takes a per-stream counter as id; `send` takes `StreamTiming::DEFAULT.send` (30 s); the ping interval is `StreamTiming::DEFAULT.ping` (15 s); shutdown is `crate::live::shut(&mut state.live.closed())` as in `streams.rs`.
14. **Handler extractors**: `State(state)`, `Extension(principal): Extension<Principal>`, `Extension(access): Extension<ProjectAccess>`, `error::Json(body): error::Json<PlaygroundRunRequest>`, as `routes/eval_jobs.rs`.
15. **Limit tests.** Use `max_concurrency` ≥ 5 on the connection when testing the per-user limit. Make the 10 s wait a `pub(crate) const INTERACTIVE_WAIT: Duration` in `routes/playground.rs` and run the slot-busy test with `#[tokio::test(start_paused = true)]` if the test app works under a paused clock; otherwise accept the 10 s.
16. **Mock turns.** Tool calls go in the turn's `message.tool_calls` (see `tools/capture/src/scenario.rs`); the turn's `model` must equal the request's model.
17. **e2e streaming.** `Api.post` reads JSON; the e2e builds the POST with `server.client`, the session cookie and the `origin` header the `Api` helper sends, then reads it with `read_until`.
19. **V6.3 stale text.** In V6.3 step 2, ignore `gen_ai.request.stop_sequences` and the `gen_ai.response.finish_reasons` span attribute: use `gen_ai.request.stop` and put the finish reason on the output `Message` (Precision 9). `trace.rs` is not "visibility only": it gains `double_attr` and `string_array_attr`.
20. **V6.4 housekeeping.** The request example is registered only in `crates/server/tests/api_examples.rs` (its round-trip list and its `requests` table), not in `src/api_examples.rs`. Add `"cetra-capture"` to `cetra-server`'s dependency list in `Cargo.lock` by hand (sorted), or leave it to the milestone's `cargo check`. Tests that need a connection create it through `POST /api/v1/orgs/acme/llm-connections` (copy the private `connection` helper of `dispatch.rs`' tests) with `requests_per_minute: 600` unless the test is about tokens. Test 10 reads the stream to its end inside `capture_logs`.
18. **Notes to record in the spec text** (V6.5): playground traces carry no end-user id, so a delete-by-user never matches them (they go with project and trace deletions); the overview's "Evaluation cost" widget counts `cetra-eval` only; a tool-mode response format forces its own tool, so user tools are not callable in the same run.

---

## V6.1 — Reserved environments and the units rule (shared APIs 1 and 2)

**Commit subject:** `core: one reserved-environment test, used everywhere cetra-eval was special-cased`

### Read first

`crates/core/src/names.rs` (`mod environment`, the tests `catalog`/snapshot, `branded_constants_start_with_the_product_name`); `crates/storage/src/filter/scope.rs` (`exclusions`, `strict_exclusions`, `traces_clean`); `crates/server/src/ingest/mod.rs` (`EVAL_ENVIRONMENT` import, `Ingest::commit`'s `billable`); `crates/server/src/ingest/environment.rs` (`rewrite_reserved_environment` and its tests); `crates/server/src/eval/pending.rs` (`group_by_trace`); `crates/server/src/eval/trace_load.rs` (the evaluation-trace refusal); `sdks/python/src/cetra/_config.py` and `sdks/typescript/src/config.ts` (the reserved environment check) and their tests; `crates/storage/src/views/catalog.rs` (the `environment` description); `crates/server/src/api/openapi.rs`, `crates/server/tests/openapi.rs`, `crates/server/src/mcp/tools.rs`, `crates/server/src/api/evaluators.rs` (texts naming `cetra-eval`).

### Edit

| File | Change |
|---|---|
| `crates/core/src/names.rs` | the new constants and functions, tests, snapshot entries |
| `crates/core/tests/snapshots/names.snap` | updated at the milestone's check (do not edit by hand; note it) |
| `crates/storage/src/filter/scope.rs` | exclusions over every reserved environment |
| `crates/server/src/ingest/mod.rs` | `billable_spans` |
| `crates/server/src/ingest/environment.rs` | rewrite through `external_alias` |
| `crates/server/src/eval/pending.rs`, `crates/server/src/eval/trace_load.rs` | `is_reserved` |
| `sdks/python/src/cetra/_config.py`, `sdks/python/tests/test_config.py`, `sdks/typescript/src/config.ts`, `sdks/typescript/src/config.test.ts` | both reserved names |
| `crates/storage/src/views/catalog.rs`, `crates/server/src/api/openapi.rs`, `crates/server/tests/openapi.rs`, `crates/server/src/mcp/tools.rs`, `crates/server/src/api/evaluators.rs` | texts |
| `website/src/content/docs/docs/reference/attributes.md` | only if the names test requires listing the new constants there (check the test) |
| `docs/spec/01-data-model.md`, `02-ingestion.md`, `05-query.md`, `10-sdk.md` | the reserved list |

### Steps

1. **`names.rs`**, in `pub mod environment` after `EVAL_EXTERNAL`:
   ```rust
   /// Reserved for playground runs (D-V-22, D-V-33).
   pub const PLAYGROUND: &str = crate::cetra_name!("-playground");
   /// What external data that claimed [`PLAYGROUND`] is rewritten to (D-V-33).
   pub const PLAYGROUND_EXTERNAL: &str = crate::cetra_name!("-playground-external");
   /// Environments only the server writes. Their traces are hidden by default, excluded from
   /// online rules, refused as evaluation targets, rewritten when received over OTLP, and not
   /// billed (D-V-33, D-V-34). A new reserved environment is added here.
   pub const RESERVED: &[&str] = &[EVAL, PLAYGROUND];

   /// Whether `environment` is reserved (D-V-33).
   #[must_use]
   pub fn is_reserved(environment: &str) -> bool {
       RESERVED.contains(&environment)
   }

   /// The name external data that claims a reserved environment is rewritten to.
   #[must_use]
   pub fn external_alias(environment: &str) -> Option<&'static str> {
       match environment {
           e if e == EVAL => Some(EVAL_EXTERNAL),
           e if e == PLAYGROUND => Some(PLAYGROUND_EXTERNAL),
           _ => None,
       }
   }
   ```
   Add `PLAYGROUND` and `PLAYGROUND_EXTERNAL` to the test lists that hold `EVAL`/`EVAL_EXTERNAL` (the catalog rows and the branded list). Tests in `names.rs`: `reserved_environments` (`is_reserved("cetra-eval")`, `is_reserved("cetra-playground")` true; `"production"`, `"cetra-eval-external"` false; `external_alias` maps both and returns `None` for others; every `RESERVED` entry has an alias).
2. **Storage exclusions** (`scope.rs`): replace the single `IS DISTINCT FROM lit(environment::EVAL)` clause by one `IS DISTINCT FROM` clause per entry of `environment::RESERVED`, joined with `AND` (keep the `NULL`-environment rows; the same condition `!(include_eval || filter_references_environment)` turns them off). `include_eval` keeps its name and meaning in the API ("include evaluation and playground traces" in its doc comment and descriptions). Add a test beside the existing exclusion tests: a trace in `cetra-playground` is hidden by default, shown with `include_eval`, shown when the filter names `environment`, and left out by strict exclusions.
3. **Units rule** (`ingest/mod.rs`):
   ```rust
   /// Spans that count as units (D173): every span outside a reserved environment (D-V-34).
   pub(crate) fn billable_spans(spans: &[cetra_core::span::Span]) -> usize {
       spans.iter().filter(|span| !span.environment.as_deref().is_some_and(environment::is_reserved)).count()
   }
   ```
   (use the real `Span` path the module already imports). `Ingest::commit` uses it instead of the inline filter; remove the `EVAL_ENVIRONMENT` import if unused. Unit test `billable_spans_skips_reserved_environments` (spans in `None`, `production`, `cetra-eval`, `cetra-playground` → 2).
4. **OTLP rewrite** (`environment.rs`): for each span whose environment is reserved, set it to `external_alias(env)` and add the `reserved_environment` warning as today. Add a test for `cetra-playground` → `cetra-playground-external`.
5. **Online rules and evaluation targets**: `pending.rs`' `group_by_trace` skips spans whose environment `is_reserved` (keep the experiment test); `trace_load.rs` refuses a trace with any span in a reserved environment (same error). Add one test each where the module has tests for the `cetra-eval` case (copy it for `cetra-playground`).
6. **SDKs**: Python `_RESERVED_ENVIRONMENTS = frozenset({"cetra-eval", "cetra-playground"})` with the check `if value in _RESERVED_ENVIRONMENTS`; TypeScript `const RESERVED_ENVIRONMENTS = new Set(["cetra-eval", "cetra-playground"])` with `has(value)`; update the comment pointing at `cetra_core::names::environment::RESERVED`; one test each for `cetra-playground`.
7. **Texts**: every user-facing description that says `cetra-eval` is excluded (catalog `environment` description, OpenAPI descriptions, the `tests/openapi.rs` text constant asserted against them, MCP tool descriptions, `api/evaluators.rs`) says "evaluation and playground traces (`environment` `cetra-eval` or `cetra-playground`)". Keep each sentence's structure; change only the names.
8. **Specs**: spec 01 (`environment` row: reserved `cetra-eval` and `cetra-playground`), spec 02 (the OTLP rewrite to `-external` for both), spec 05 (default and strict exclusions over every reserved environment), spec 10 rule 8 (both names).

### Tests

As listed in the steps (names, exclusions, billable spans, rewrite, pending, trace load, two SDK config tests).

---

## V6.2 — Interactive connection permits, playground limits, `RunPlayground` (shared API 3)

**Commit subject:** `server: interactive connection permits, playground limits and RunPlayground`

### Read first

`crates/server/src/eval/mod.rs` (`EvalQueue`, its `connections` map, the private `lock` helper); `crates/server/src/eval/dispatch.rs` (`ConnectionLimit`, `TokenBucket`, `pick_limits`, `admit`, the tests around slots and buckets); `crates/server/src/meta/connections.rs` (`ConnectionRow`, `get_connection`); `crates/server/src/auth/limits.rs` (`KeyedLimiter`, `InFlight`, `DemoLimits` and its `admit`, `Limits`, `Limits::prune`); `crates/server/src/auth/roles.rs` (`ProjectPerm`, `min_role`, `key_scope`, its tests); `crates/server/src/http/access_suite.rs`; `web/src/lib/roles.ts` and its test.

### Edit

| File | Change |
|---|---|
| `crates/server/src/eval/dispatch.rs` | extract `connection_limit` (get-or-create, rebuild on change); `pick_limits` uses it |
| `crates/server/src/eval/interactive.rs` | new: `acquire`, `InteractivePermit`, `Busy` |
| `crates/server/src/eval/mod.rs` | `pub(crate) mod interactive;` |
| `crates/server/src/auth/limits.rs` | `PlaygroundLimits` in `Limits`, `prune` |
| `crates/server/src/auth/roles.rs` | `ProjectPerm::RunPlayground`, tests |
| `crates/server/src/http/access_suite.rs` | only if an exhaustive match or table needs the new permission |
| `web/src/lib/roles.ts`, `roles.test.ts` | `canRunPlayground` (member and above) |

### Steps

1. **Extract** in `dispatch.rs`:
   ```rust
   /// The shared limits of `connection` (D-I-4, D-I-6, D-V-35): created on first use, rebuilt when
   /// `limits` changed (running holders keep their old permits).
   pub(crate) fn connection_limit(state_eval: &EvalQueue, id: Uuid, limits: (u32, u32), now: i64)
       -> Arc<ConnectionLimit>;
   ```
   (use the real type name of `state.eval`; move the logic `pick_limits` has inline into it and call it from `pick_limits`, behaviour unchanged). Give `ConnectionLimit` two `pub(super)` accessors: `slots(&self) -> &Arc<Semaphore>` and `take_token(&self, now: i64) -> bool` (locks the bucket and calls `try_take`).
2. **`interactive.rs`**:
   ```rust
   //! Interactive calls on an LLM connection: a playground run (and later tracks' interactive calls)
   //! takes the connection's concurrency slot and a rate token like a queued job, but waits for them
   //! instead of polling, so it goes ahead of rule jobs (D-V-35).

   /// The connection is busy for longer than the caller wants to wait.
   #[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
   #[error("the connection is busy")]
   pub(crate) struct Busy;

   /// Holds the connection's concurrency slot until dropped. The rate token is spent.
   #[derive(Debug)]
   pub(crate) struct InteractivePermit { _slot: OwnedSemaphorePermit }

   /// Waits up to `wait` for a slot of `connection` (FIFO, ahead of the dispatcher's
   /// `try_acquire`), then for a rate token (polled every 100 ms with the state's clock), and
   /// returns the permit. At the deadline: `Busy`, holding nothing.
   pub(crate) async fn acquire(state: &AppState, connection: &ConnectionRow, wait: Duration)
       -> Result<InteractivePermit, Busy>;
   ```
   Steps of `acquire`: `deadline = Instant::now() + wait`; `limit = connection_limit(&state.eval, connection.id, (connection.max_concurrency, connection.requests_per_minute), state.clock.now_ns())`; `slot = timeout_at(deadline, Arc::clone(limit.slots()).acquire_owned())` → elapsed or closed → `Busy`; then loop: `limit.take_token(state.clock.now_ns())` → return the permit; else if `Instant::now() + 100 ms > deadline` → `Busy` (the slot drops); else `sleep(100 ms)`. Log nothing (the caller logs).
   Tests (`#[tokio::test]`, a `TestApp` with a connection seeded the way `dispatch.rs`' tests do): `takes_a_slot_and_a_token`; `waits_for_a_slot` (max_concurrency 1, hold one permit, a second `acquire` with 2 s waits, then succeeds after the first drops); `busy_at_the_deadline` (slot held, wait 200 ms → `Busy`); `busy_without_tokens` (requests_per_minute 1: two acquires, the second `Busy` within the wait); `dispatcher_sees_the_slot_taken` (while a permit is held on a 1-slot connection, the dispatcher's `try_acquire` on the same `connection_limit` fails).
3. **`PlaygroundLimits`** in `limits.rs`, shaped like `DemoLimits`:
   ```rust
   pub(crate) const PLAYGROUND_PER_MINUTE: u32 = 60;
   pub(crate) const PLAYGROUND_USER_IN_FLIGHT: usize = 4;
   pub(crate) const PLAYGROUND_ORG_IN_FLIGHT: usize = 16;
   pub(crate) struct PlaygroundLimits { rate: KeyedLimiter<Uuid>, users: InFlight<Uuid>, orgs: InFlight<OrgId> }
   pub(crate) struct PlaygroundGuard { _user: InFlightGuard<Uuid>, _org: InFlightGuard<OrgId> }
   impl PlaygroundLimits {
       /// In-flight first (org, then user), then the rate; a refusal spends no token and holds no slot.
       pub(crate) fn admit(&self, user: Uuid, org: OrgId, now: i64) -> Result<PlaygroundGuard, RetryAfter>;
   }
   ```
   (use the real types of `InFlight`'s keys and `max_keys` values the other limiters use). `Limits` gains `pub(crate) playground: PlaygroundLimits`; `Limits::prune` prunes its rate limiter. Tests: 4 in flight per user then refused; 16 per org across users; the 61st within a minute refused; a refused in-flight admit spends no rate token.
4. **`ProjectPerm::RunPlayground`** after `ManagePrompts`, doc "Run the playground (D-V-26): member and above, a session or a read-write personal access token; never an API key."; `min_role` → `Member`; `key_scope` → `None`; test asserts beside the others; `canRunPlayground` in `web/src/lib/roles.ts` with its test (same shape as `canManagePrompts`).

---

## V6.3 — `cetra-eval`: rendering playground messages and building the run trace

**Commit subject:** `eval: render playground messages and build playground run traces`

### Read first

`crates/eval/src/trace.rs` (`build`, `export_request`, `span_id`, the private helpers `string_attr`, `int_attr`, `metadata_attr`, `otel_span`, `error_status`, `usage_attrs`, the `ChildSpan::Llm` attributes); `crates/eval/src/chat/mod.rs` (`ChatRequest`, `ToolDefinition`, `ChatParams`, `TokenUsage`, `FinishReason`, `ToolCall`, `check_request`); `crates/eval/src/plain_template.rs`; `cetra_core::message` (the canonical `Message` and parts); `crates/otel` (which attribute keys normalization maps to `tool_definitions`, `request_params` and `ttft_ns`: grep `TOOL_DEFINITIONS`, `REQUEST_PARAMS`, `TTFT` in `crates/otel/src` and `crates/core/src/names.rs`).

### Edit

| File | Change |
|---|---|
| `crates/eval/src/playground.rs` | new: `render_messages`, `PlaygroundEntry`, `RenderError`, `PlaygroundTrace`, `build_playground_trace` |
| `crates/eval/src/trace.rs` | the helpers it shares become `pub(crate)` (visibility only) |
| `crates/eval/src/chat/mod.rs` | `pub fn check_chat_request(&ChatRequest) -> Result<(), ChatError>` wrapping the private `check_request` (only if it is not already public under another name; then use that) |
| `crates/eval/src/lib.rs` | `pub mod playground;` and the `check_chat_request` export |

### Types and functions

```rust
//! Playground runs (D-V-21, D-V-22, D-V-37): rendering the request's messages and building the
//! run's trace for the normalizer, as judge traces are built (D161).

/// One request entry: a canonical message or a placeholder.
#[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum PlaygroundEntry {
    Placeholder { r#type: PlaceholderTag, name: String },
    Message(cetra_core::message::Message),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaceholderTag { Placeholder }

/// Why the messages could not be rendered. Holds names and indexes only.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenderError {
    #[error("message {index}: {source}")]
    Template { index: usize, part: usize, source: plain_template::PlainTemplateError },
    #[error("missing placeholder {name}")]
    MissingPlaceholder { name: String },
    #[error("too many messages")]
    TooMany,
}

/// Expands placeholders in place and renders the `text` parts of the request's own messages with
/// `variables` (placeholder messages are copied unrendered). At most 200 messages after expansion.
pub fn render_messages(entries: &[PlaygroundEntry], variables: &BTreeMap<String, String>,
    placeholders: &BTreeMap<String, Vec<cetra_core::message::Message>>) -> Result<Vec<cetra_core::message::Message>, RenderError>;

/// One finished (or failed, or cancelled) run.
pub struct PlaygroundTrace<'a> {
    pub trace_id: TraceId,
    pub started_ns: i64,
    pub ended_ns: i64,
    pub adapter: Adapter,
    pub request: &'a ChatRequest,           // rendered messages, tools, params, model
    pub response_model: Option<&'a str>,
    pub output: Option<&'a cetra_core::message::Message>,  // the assistant message assembled from the stream
    pub usage: Option<&'a TokenUsage>,
    pub finish_reason: Option<&'a str>,
    pub ttft_ns: Option<i64>,
    pub error: Option<&'a str>,             // a payload-free error text, `cancelled` for a cancel
    pub run_by: Uuid,                       // the user
    pub source: Option<String>,             // `prompt:<name>@<version>` or `span:<trace hex>:<span hex>`
    pub prompt: Option<(&'a str, Option<i64>)>,  // the source prompt's name and version
}
// Debug: hand-written, ids and sizes only.

/// Builds the run as OTLP and normalizes it: one `llm` span, environment `cetra-playground`,
/// resource `service.name = cetra-playground`.
pub fn build_playground_trace(input: &PlaygroundTrace<'_>, ingest: &IngestContext, prices: &PriceTable) -> Normalized;
```

### Steps

1. `render_messages`: walk `entries`; a `Placeholder` with a name absent from `placeholders` → `MissingPlaceholder`; present → append clones of its messages; a `Message` → clone it, and for every part whose type is text render its content with `plain_template::render(content, variables)` (errors become `Template { index: <entry index>, part, source }`); count messages and return `TooMany` above 200.
2. `build_playground_trace`: one `ResourceSpans` with `service.name = environment::PLAYGROUND` and the crate's scope, one root span (parent none) with span id `trace::span_id(trace_id, 0)`, name `playground {model}`, start/end from the input, status error with `error` when set. Attributes, using the shared helpers: `cetra.span.kind = llm` (the attribute the judge child uses), `cetra.environment = cetra-playground`, `gen_ai.operation.name = chat`, `gen_ai.provider.name = adapter.provider_name()`, `gen_ai.request.model`, `gen_ai.response.model` when known, the input as `cetra.input` (JSON of the rendered canonical messages) with `cetra.input.format = messages`, the output as `cetra.output` (JSON array of the one assistant message) with format `messages` when present, the tool definitions under the key normalization maps to `tool_definitions` (as JSON), the request params under the keys normalization maps into `request_params` (`gen_ai.request.temperature`, `gen_ai.request.top_p`, `gen_ai.request.max_tokens`, `gen_ai.request.stop_sequences`, `gen_ai.request.presence_penalty`, `gen_ai.request.frequency_penalty`, `gen_ai.request.seed` — set only those present; if normalization reads other spellings, use them and say so), the usage attributes as `usage_attrs` writes them, `gen_ai.response.finish_reasons` (array of one) when known, `cetra.ttft_ns` (`attr::TTFT_NS`) when known, `cetra.prompt.name`/`cetra.prompt.version` (`attr::PROMPT_NAME`/`PROMPT_VERSION`) when `prompt` is set, and metadata `cetra.playground.run_by` (the user id) and `cetra.playground.source` (when set) through `metadata_attr` (add the two key constants to `cetra_core::names::metadata_key` as `PLAYGROUND_RUN_BY = cetra_name!(".playground.run_by")` and `PLAYGROUND_SOURCE = cetra_name!(".playground.source")`, add `crates/core/src/names.rs` to this phase's edits, with their snapshot/test list entries). Then `cetra_otel::normalize(&request, ingest, prices)`; log rejections with counts only, as `trace::build` does.
3. Tests (`playground.rs`): `renders_variables_and_placeholders` (a system message with `{{product}}`, a placeholder `history` with two messages containing `{{not_rendered}}`, a user message: the output has 4 messages, the placeholder text unrendered); `missing_variable_names_its_message` (index and part); `missing_placeholder`; `too_many_messages` (201 after expansion); `trace_has_one_llm_span` (build with a request of 2 messages, usage 10/3, ttft 5 ms, a prompt `support-reply@2`: one normalized span, kind `llm`, environment `cetra-playground`, `prompt_name`/`prompt_version` set, `ttft_ns` set, tool definitions present, metadata `cetra.playground.run_by`; `cost_total` priced when the price table knows the model: use the builtin table and a model it prices, e.g. the one `trace.rs`' tests use); `cancelled_run_is_an_error_span` (error `cancelled`, no output).

---

## V6.4 — `POST /playground/runs`

**Commit subject:** `server: stream playground runs over SSE and trace them`

### Read first

`crates/server/src/routes/streams.rs` (`sse_response`, `send`, `ping_event`, `json_event`, the `select!` loop with `tx.closed()` and `shut`, the cancel tests); `crates/server/src/eval/execute.rs` (`commit_judge_trace`: `reserve_exact(0)` and `commit_in_task`); `crates/server/src/eval/test_run.rs` and `routes/connections.rs` (loading and decrypting a connection: `meta::connections::get_connection`, `state.connections.get(&state.secrets, &row)`, the ErrorClass mapping by hand); `crates/server/src/http/restore.rs` (`refuse_outbound`); `crates/server/src/ingest/mod.rs` (`Ingest::context`, prices `for_project`); `crates/server/src/routes/prompts.rs` (route style); `crates/server/src/api/openapi.rs` (`sse(...)` helper, how `traces/stream` is documented) and `tests/openapi.rs`.

### Edit

| File | Change |
|---|---|
| `crates/server/src/api/playground.rs` | new: request types |
| `crates/server/src/api/mod.rs` | `pub mod playground;` |
| `crates/server/src/routes/playground.rs` | new: the route, the run task, tests |
| `crates/server/src/routes/mod.rs`, `crates/server/src/http/access.rs` | registration |
| `crates/server/src/api/openapi.rs`, `crates/server/tests/openapi.rs` | the operation (the generated files are regenerated at the milestone's check) |
| `crates/server/src/api_examples.rs`, `crates/server/tests/api_examples.rs`, `fixtures/api/requests/playground-run.json` | a request example (POST: no manifest entry is needed for a non-GET; follow how other POST request examples are registered) |

### Request types (`api/playground.rs`, `Deserialize`, `ToSchema`, `deny_unknown_fields`)

```rust
pub struct PlaygroundRunRequest {
    pub connection_id: Uuid,
    pub model: String,                                   // 1–256 bytes
    #[schema(schema_with = any_json)] pub messages: Vec<serde_json::Value>,   // PlaygroundEntry each
    #[serde(default)] pub variables: BTreeMap<String, String>,
    #[serde(default)] #[schema(value_type = Object)] pub placeholders: BTreeMap<String, Vec<serde_json::Value>>,
    #[serde(default)] pub params: PlaygroundParams,
    #[serde(default)] pub tools: Vec<PlaygroundTool>,
    #[serde(default)] pub tool_choice: Option<String>,   // auto | none | required | a tool name
    #[serde(default)] pub response_format: Option<PlaygroundResponseFormat>,
    #[serde(default)] pub source: Option<PlaygroundSource>,
}
pub struct PlaygroundParams { temperature: Option<f64>, top_p: Option<f64>, max_tokens: Option<u32>, stop: Vec<String> (default), presence_penalty: Option<f64>, frequency_penalty: Option<f64>, seed: Option<i64>, reasoning_effort: Option<String> }   // Default
pub struct PlaygroundTool { name: String, #[serde(default)] description: Option<String>, #[schema(schema_with = open_object)] parameters: serde_json::Value }
pub struct PlaygroundResponseFormat { name: String, #[schema(schema_with = open_object)] schema: serde_json::Value, #[serde(default)] strict: bool }
#[serde(rename_all = "snake_case")]
pub enum PlaygroundSource { Prompt { name: String, version: i64 }, Span { trace_id: TraceId, span_id: SpanId } }
```

(Write each struct with its doc comments and `pub` fields; `messages` and `placeholders` are parsed into `PlaygroundEntry`/`Message` in the handler, field errors under `messages[i]`/`placeholders.<name>[i]`.)

### Route

`POST /api/v1/projects/{project}/playground/runs`, `Access::Project { perm: ProjectPerm::RunPlayground, session_only: false }`, `documented: true`, body limit 2 MiB (`DefaultBodyLimit::max(2 * 1024 * 1024)`).

Handler, in this order (any failure before step 9 is an ordinary problem answer):
1. `refuse_outbound(&state, "playground run")?`.
2. `user = principal::session_user(&principal)?` (a key cannot reach the route anyway).
3. Parse `messages` into `PlaygroundEntry` and `placeholders` into messages (400 field errors); `model` 1–256 bytes; `params.max_tokens` default `CHAT_MAX_TOKENS_DEFAULT`, at most `CHAT_MAX_TOKENS_MAX` (400 `params.max_tokens`).
4. `cetra_eval::playground::render_messages(...)` → 400 with field `messages[index].parts[part]` (template, with the position as the problem's `position` when the error is `Invalid`), `placeholders` (missing placeholder), or `messages` (too many).
5. Build the `ChatRequest` (rendered messages, tools, `tool_choice` mapped `auto`/`none`/`required`/else a tool name, `response_format` with `tool_name = "submit_answer"` and `tool_description = "Submit the answer."` when given, params, `max_tokens`, `timeout = CHAT_TIMEOUT_MAX`, `idle_timeout = CHAT_IDLE_TIMEOUT`); `cetra_eval::check_chat_request(&request)` → 400 (field `messages[index]` for `UnsupportedMessage { index, .. }` with its reason, `tools`/`tool_choice`/`response_format`/`model` for the others; messages never include payload text).
6. Connection: `meta::connections::get_connection(state.meta.reader(), access.project.org_id, body.connection_id)`; `None` → 400 field `connection_id`: "connection not found in this organization"; decrypt with `state.connections.get(&state.secrets, &row)` (500 on failure). `status` is not checked (D-V-36).
7. `guard = state.limits.playground.admit(user.id, access.project.org_id, now)` → `ApiError::RateLimited { retry_after_s }`.
8. `permit = eval::interactive::acquire(&state, &row, Duration::from_secs(10)).await` → `Busy` → `ApiError::RateLimited { retry_after_s: 1 }`.
9. `trace_id` = 16 random bytes (`crate::auth::secret::random_bytes(16)` as `routes/connections.rs` does). Spawn the run task (below) owning `guard`, `permit`, the decrypted connection, the request, the project row, `user.id`, the source, and a clone of `state`; return `sse_response(rx)` (copy `streams.rs`' `sse_response` and helpers into this module or make them `pub(crate)` there: prefer making `sse_response`, `send`, `json_event`, `ping_event` `pub(crate)` in `streams.rs`).

Run task, in this order:
1. Send `start` `{trace_id}` (hex). `started = now_ns`, `t0 = Instant::now()`.
2. `stream = cetra_eval::stream_chat(state.eval_runner.egress(), &connection, &request).await`; an `Err(e)` → go to step 5 with the error.
3. Loop with `tokio::select! { biased; shutdown => cancelled, tx.closed() => cancelled, ping.tick() => send ping, event = stream.next() => … }` (ping every 15 s as `streams.rs`). `Text(delta)`: record `ttft` on the first `Text` or `ToolCall`, append to the output text, send `text` `{delta}`. `ToolCall(call)`: when `response_format` is set and `call.name == "submit_answer"`, append `call.arguments` as compact JSON to the output text and send it as a `text` event instead; otherwise keep it in the output's tool calls and send `tool_call` `{id, name, arguments}`. `Done(done)`: keep it; leave the loop. `Ok(None)`: leave. `Err(e)`: keep the error; leave.
4. On cancel: drop `stream` (closes the provider request); error text `cancelled`.
5. Build the assistant `Message` (text part when the text is not empty, then the tool-call parts), `ended = now_ns`, `PlaygroundTrace { … }` with the error text (`ChatError`'s `Display` — it holds no payload — or `cancelled`), build with `state.ingest.context(project.id, None, ended)` and `state.ingest.prices().for_project(org, project)`, commit like `commit_judge_trace` (`reserve_exact(0)`, `commit_in_task`, which survives the client's departure); a commit error is logged (ids only) and ends the task without a `done`.
6. If the client is still there: send `done` `{finish_reason, usage: {input, output, cache_read, cache_write, reasoning}, cost_usd, latency_ms, ttft_ms, output}` (`cost_usd` = the normalized span's `cost_total`; `output` = the assistant message) or, on error, `error` `{class, message}` with `class` = `retryable` | `pause` | `permanent` (map `ChatError::class()` by hand as `routes/connections.rs` does) and `message` = the error's `Display`.
7. Audit log: `tracing::info!(target: "audit", project_id, connection_id, user_id, trace_id, outcome = "done"|"error"|"cancelled", latency_ms, "playground run")`.
8. `guard` and `permit` drop at the end of the task.

### OpenAPI

Operation `post_playground_run`, tag `playground` (new tag "Playground runs (D-V-20)."), request body `PlaygroundRunRequest`, response `200` with `sse("…")` describing the events (`start {trace_id}`, `text {delta}`, `tool_call {id, name, arguments}`, `done {…}`, `error {class, message}`, `ping {}`), errors `400 401 403 404 413 429 503`; description "Members and above, through a session or a read-write personal access token (D-V-26). Not units (D-V-23)." Add it to `OPERATIONS`, `TAGS`, and the error branch with exactly that set (add an `else if` if none matches). Request example `fixtures/api/requests/playground-run.json` (canonical JSON): connection `01900000-0000-7000-8000-000000000400` (the example connection id in `fixtures/api/README.md`; check), model `gpt-4o`, a system message with `{{product}}` and a user message, `variables: {"product": "cetra"}`, `params: {"temperature": 0.2}`, `source: {"prompt": {"name": "support-reply", "version": 2}}`; registered in the request round-trip table.

### Tests (`routes/playground.rs`, with `cetra_capture::Mock` served on `127.0.0.1:0` and the test app configured with `allow_private_network_egress = true`, and a connection seeded the way `dispatch.rs`' tests do, pointing at the mock)

1. `streams_a_run` — a mock OpenAI turn with text in 3 chunks and usage 10/3: the events are `start`, three `text`, `done` (usage, `output` with the full text); afterwards the project has one trace in `cetra-playground` (read with `include_eval`) with the rendered input, the output, `prompt_name` from the source, and the traces list without `include_eval` does not show it; `usage_daily` units unchanged.
2. `missing_variable_is_400` — field `messages[0].parts[0]`.
3. `connection_of_another_org_is_400`.
4. `viewer_and_keys_are_refused` — a viewer session 403, a `write` key 403, a read-only PAT 403, a read-write PAT of a member streams.
5. `limits` — with a 1-slot connection held through `interactive::acquire`, a run answers 429 after about 10 s (use `TestApp::with_config` or a test seam to shorten the wait if the code allows passing it; if not, assert with a 10 s timeout); the fifth concurrent run of one user is 429 (use a mock turn with a long `ttft_ms` to hold runs open).
6. `cancel_drops_the_provider_call_and_records_the_run` — a mock turn with `ttft_ms` 5 000; drop the response after `start`; within 2 s the trace exists with status error and message `cancelled`; the mock log shows the request.
7. `tool_calls_and_response_format` — a mock turn with a tool call: a `tool_call` event; a run with a response format on a `tool`-mode connection: the forced tool's arguments arrive as `text`.
8. `provider_error_is_an_error_event` — a hand-written axum stub answering 401: an `error` event with class `pause`, and the connection's `status` is still `ok` (D-V-36).
9. `restore_mode_refuses` — `restore::tests::assert_refused`.
10. `audit_log_has_no_payload` — capture logs around a run whose messages contain `canary-text`: logs contain `playground run`, not `canary-text`.

---

## V6.5 — End-to-end test, docs, decisions

**Commit subject:** `Track V M6: playground end-to-end test, specs and decisions`

### Edit

| File | Change |
|---|---|
| `crates/cli/tests/e2e.rs` | `playground_run_end_to_end` |
| `docs/spec/09-api.md` | the route (§ Prompts or a new § Playground) |
| `docs/spec/06-evaluation.md` | one paragraph: playground runs use the chat layer and the shared connection limits (D-V-35) |
| `docs/spec/12-operations.md` | the playground limits next to other rate limits, if the spec lists them |
| `docs/mvp-decisions.md`, `docs/plans/v-prompts/00-design.md` | D-V-33 to D-V-37 rows |
| `CHANGELOG.md`, `docs/roadmap.md` | lines |

### Steps

1. **e2e** (template `online_rules_and_backfill_end_to_end`): start a `cetra_capture::Mock` with one streamed OpenAI turn, the server with `allow_private_network_egress = true`, create the connection through the API, POST a run with the admin's cookie, read the stream with `read_until` until `event: done`, then poll the traces list with `include_eval=true` until the playground trace appears (environment `cetra-playground`), and check the default traces list does not list it.
2. **Docs and decisions** as the Edit table; the changelog line: `- Playground API (D-V-20 to D-V-26, D-V-33 to D-V-37): POST /playground/runs streams a model run over SSE through the org's LLM connection, traces it into the reserved cetra-playground environment (not billed), with per-user and per-org limits; cetra-playground is reserved everywhere cetra-eval is.`; the roadmap V row lists M5 and M6 as implemented.
3. **Website**: the user-facing page for the playground comes with M7 (UI). In this phase, only `website/src/content/docs/docs/reference/attributes.md` or the data-model page if they list reserved environments (grep `cetra-eval` under `website/src/content/docs/docs/` and add `cetra-playground` where the reserved list is given).

### Milestone checks (after V6.5 is committed)

Run once: `cargo check --workspace --all-targets`, fix; `INSTA_UPDATE=always cargo test -p cetra-core` (names snapshot: only the new constants); `just openapi`; `just api-examples`; `just docs-gen`; then `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH && just check`; fix and re-run only what failed. Commit the fixes as `Make track V M6 build`.
