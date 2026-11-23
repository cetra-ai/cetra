# Track IN, M4: native GenAI frameworks and VoltAgent — plan

The frameworks that emit their own spans: Pydantic AI, Semantic Kernel (Python), Strands Agents (and Bedrock AgentCore), Microsoft Agent Framework (Python), AG2, AutoGen (OpenInference), Mirascope, the OpenHands SDK (through Laminar), Mastra, and VoltAgent, with its own convention. M4 is the milestone that changes the Rust normalizer:
- the instrumentation scope reaches the readers;
- new kind, provider, model, TTFT, session and payload rules;
- a `voltagent` convention;
- the Agno mime fix from M3.

Every rule is backed by a captured case. The spikes (`spikes/m4-python-genai.md`, `spikes/m4-ts-genai.md`) ran each framework and each normalization, and are the evidence for every key named below.

Implementer: the GLM 5.3 agent (the user, 2026-11-23), one agent per phase. Conventions as in `03-m3-frameworks.md`:
- Node on `PATH`: `export PATH=$HOME/.local/share/nvm/v26.13.0/bin:$PATH`;
- `CARGO_TARGET_DIR=/home/sebastien/travaux/llm-observer/target` on cargo and just commands;
- `git commit -m … -- <paths>`;
- `touch tools/capture/src/main.rs` before the first capture (another session's binary may sit in the shared target).

**Read first:**
- `docs/plans/in-integrations/00-design.md`: D-IN-5, D-IN-6, D-IN-7, D-IN-15, D-IN-17.
- Both M4 spikes. Each phase names the sections it relies on.
- `docs/spec/02-ingestion.md`: § Convention detection, § Mapping tables, the rules.
- `crates/otel/src/{normalize,detect,kind,context,usage,payload,fields}.rs` and `crates/otel/src/conventions/{mod,genai,openinference,laminar,vercel}.rs`.
- `fixtures/otlp/README.md` § Workflows.

## What it is, and what it is not

When it is done:
- every framework above has a captured case (OpenHands: a hand-written case) and a page;
- the spans normalize with their kinds, provider, model, tokens, cost, input, output and session, as far as each framework sends them;
- the `voltagent` convention exists;
- the Agno agent span's text output no longer records a normalize error.

Not in M4:
- **Semantic Kernel content.** Semantic Kernel sends message content as OTLP log records, which cetra drops (D-CA-2, D-G-40). Its page says so. A logs track would change that.
- **Mastra and VoltAgent span types no case captures:** Mastra `rag_*`, `scorer_*` and MCP spans; VoltAgent workflows, retrievers and guardrails. They keep their current kind until a case captures them.
- **VoltAgent's first-step usage.** VoltAgent's `llm` span records only the last step's tokens, so its cost undercounts on tool loops. The page says so.
- **Java and .NET** (M8).

## Decisions this plan relies on

| ID | Summary |
|---|---|
| D-IN-5 | Native GenAI gaps, each backed by a capture. Amended by D-IN-23. |
| D-IN-6 | VoltAgent convention, placed after `genai`, plain `input`/`output` scoped. Amended by D-IN-23 (detection). |
| D-IN-7 | The `autogen-agentchat` registry row. |
| D-IN-15 | Aliases and the framework-name table. Amended by D-IN-23. |
| D-IN-23 (new, recorded by IN4.10) | M4's decisions from the spikes and the main session's answers (below). |

**D-IN-23, in full:**
1. **AG2 `record_usage` stays `span`.** Its tokens repeat the `chat` span's (double count). `gen_ai.usage.thinking_tokens` is read only if the AG2 capture shows it.
2. **The framework-name table applies to `gen_ai.provider.name` and `gen_ai.system` alike,** and gains `strands-agents` and `microsoft.agent_framework`.
3. **The AutoGen page enables the AutoGen instrumentor only.** Adding OpenInference's OpenAI instrumentor would record each call twice.
4. **Semantic Kernel content is out of scope** (logs are dropped).
5. **Strands content is read** both from its latest-semconv details events (R5) and from its default legacy events, through a Bedrock-Converse block converter (R11).
6. **Mirascope's `@ops.trace` spans are `chain`.** The provider prefix of its model names is stripped on Mirascope spans only.
7. **Pydantic AI's agent span input** is its whole `pydantic_ai.all_messages`.
8. **OpenHands is hand-written.** OpenHands cannot share the capture project's lock (`mcp` conflict), so its case is hand-written from the spike's real dump. A Laminar `LLM` span with no model and no tokens is `chain`.
9. **VoltAgent detection:** a `voltagent.*` attribute, or an instrumentation scope name starting with `@voltagent/`. Only the root span carries `voltagent.*`.
10. **Fixture groups:**
    - the native-GenAI frameworks go in `genai`: `pydantic-ai-agent-tool`, `semantic-kernel-agent-tool`, `strands-agent-tool`, `strands-agent-tool-latest`, `agent-framework-agent-tool`, `agent-framework-workflow`, `ag2-agent-tool`, `mirascope-agent-tool`, `mastra-agent-tool`;
    - AutoGen goes in `openinference` (`autogen-agentchat-tool`);
    - OpenHands goes in `laminar` (`openhands-agent`, hand-written);
    - VoltAgent gets the new group `voltagent` (`agent-tool`).

    This replaces the design's per-framework group list, and avoids `ag2`'s digits in a group name.
11. **Agno (M3):** an OpenInference `input.value` or `output.value` labelled `application/json` that does not parse as JSON is text, with no normalize error.

## Versions

From the spikes, at the 7-day rule. Re-check with `uv lock` and pnpm; report what was locked.

| Package | Version |
|---|---|
| pydantic-ai-slim (with `openai` extra) | 2.68.0 |
| semantic-kernel | 1.47.0 |
| strands-agents (with `openai` extra) | 1.65.0 |
| agent-framework-core, agent-framework-openai | 1.26.0, 1.18.2 |
| ag2 (with `openai` extra) | 1.3.2 |
| autogen-agentchat, autogen-ext (with `openai` extra), openinference-instrumentation-autogen-agentchat | 0.7.5, 0.7.5, 0.1.29 |
| mirascope (with `openai` extra) | 2.8.2 |
| @mastra/core, @mastra/otel-exporter, @mastra/observability | 1.83.0, 1.5.2, 1.24.0 |
| @voltagent/core, ai, @ai-sdk/openai | 2.11.1, 6.0.322, 3.0.136 |

Capture-project overrides, as the Python spike's § Capture project describes:
- Mirascope needs `anthropic`, `google-genai` and `opentelemetry-instrumentation-google-genai` overridden to the versions the capture project already pins.
- Agent Framework uses `agent-framework-core` plus `agent-framework-openai`, not the meta package.

## Phases and dependencies

| Phase | Edits | Depends on |
|---|---|---|
| IN4.1 Python captures | `tools/capture/py/{pyproject.toml,uv.lock}`, eight scenario directories, their fixture directories (`case.json`, `input.json` only) | — |
| IN4.2 TypeScript captures | `tools/capture/ts/{package.json,pnpm-lock.yaml}`, `tools/capture/src/scenario.rs` and `crates/otel/tests/support/mod.rs` (group `voltagent`), `fixtures/otlp/README.md` (layout line), two scenario directories, their fixture directories (`case.json`, `input.json`) | — |
| IN4.3 Registry row | the IN2.3 file set (SDK registry, extras, spec 10, README, reference, generated skill references) | — |
| IN4.4 Scope plumbing | `crates/otel/src/{normalize,detect,kind,context,payload}.rs` | — |
| IN4.5 Kind, provider, model, time, session rules | `crates/otel/src/{kind,context,usage,fields}.rs`, `crates/otel/src/conventions/laminar.rs`, `docs/spec/02-ingestion.md` | IN4.4 |
| IN4.6 Payload rules | `crates/otel/src/payload.rs`, `crates/otel/src/conventions/{genai,openinference}.rs`, `crates/otel/src/convert/*` (new Converse converter), `docs/spec/02-ingestion.md` | IN4.5 |
| IN4.7 VoltAgent convention | `crates/core/src/enums.rs`, `crates/otel/src/{detect,payload,kind,context,usage}.rs`, `crates/otel/src/conventions/{mod,voltagent (new)}.rs`, `crates/otel/tests/proptest.rs`, `crates/server/openapi.json`, `web/src/api/schema.gen.ts`, `docs/spec/02-ingestion.md` | IN4.6 |
| IN4.8 Bless and OpenHands | the new fixtures' `expected.json`, `fixtures/otlp/openinference/agno-agent-tool/expected.json`, `fixtures/otlp/laminar/openhands-agent/` (hand-written) | IN4.1, IN4.2, IN4.7 |
| IN4.9 Pages | eleven pages, generated skill references | IN4.8 |
| IN4.10 Index, sidebar, records | as IN3.4, plus D-IN-23 | IN4.9 |

- IN4.1, IN4.2 and IN4.3 touch disjoint files.
- IN4.4 to IN4.7 run in order: they share `crates/otel` and spec 02.
- IN4.8 to IN4.10 run in order.

**Rules for every phase:**
- Run only "Done when". Between phases, the golden test skips the new cases that have no `expected.json` yet (`fixtures/otlp/README.md` § Workflows).
- Rust phases add unit tests next to the code they change, in the existing `mod tests` style.
- Every spec 02 change goes in the same commit as its code.
- Stop and report when a step cannot be done as written, or when an existing `expected.json` changes. The one expected change is `openinference/agno-agent-tool`, in IN4.8.

---

## IN4.1 — Python captures (input only)

**Goal:** captured `input.json` for the eight Python cases.

**Steps.**
1. Pins and overrides, as in § Versions. Run `uv lock`. List every changed existing pin in the report, and stop if one changes without being named in § Versions.
2. Eight scenarios. Start each one from the spike program in `/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m4py/`. Each scenario:
   - routes the model client to `mock_url("/v1")`, as `openinference/langgraph-agent` does;
   - runs one agent with one tool, `get_weather(city)`, in two `openai.chat` turns: a tool call `get_weather {"city": "Paris"}` with id `call_weather_1`, finish `tool_calls`, usage 82/17; then the text `It is sunny in Paris.`, finish `stop`, usage 131/12;
   - uses model `gpt-4o-mini-2024-07-18`.

   Native OpenTelemetry content capture is on, and each scenario follows its spike section for the setup:

   | Case (group `genai` unless noted) | Setup and notes |
   |---|---|
   | `pydantic-ai-agent-tool` | `Agent.instrument_all(InstrumentationSettings(tracer_provider=provider, include_content=True))` (the spike's exact call) |
   | `semantic-kernel-agent-tool` | `SEMANTICKERNEL_EXPERIMENTAL_GENAI_ENABLE_OTEL_DIAGNOSTICS_SENSITIVE=true` set in `main.py`; the content goes to logs and is not captured (D-IN-23 item 4) |
   | `strands-agent-tool` | Strands' default (legacy events) |
   | `strands-agent-tool-latest` | `OTEL_SEMCONV_STABILITY_OPT_IN=gen_ai_latest_experimental` in `scenario.json` `env` |
   | `agent-framework-agent-tool` | Agent Framework with `enable_sensitive_data=True` (the spike's call) |
   | `agent-framework-workflow` | a two-executor workflow, as in the spike (`workflow.build`, `executor.process`, `edge_group.process`) |
   | `ag2-agent-tool` | AG2's telemetry middleware, as in the spike |
   | `autogen-agentchat-tool` (group `openinference`) | `AutogenAgentChatInstrumentor` only (D-IN-23 item 3) |
   | `mirascope-agent-tool` | Mirascope's ops tracing plus a `@ops.trace` function wrapping the call (the spike's program) |

   That is nine cases; IN4.1 captures all nine.
3. Pin the nondeterminism the spike lists, per framework, with `volatile_attributes` or program pins (the AutoGen agent span's `input.value` holds a UUID, a timestamp and a memory address: pin them in the program). Telemetry stays off: none of these frameworks phones home in the spike, apart from LiteLLM, which none of them uses.
4. `just capture genai/<case> … openinference/autogen-agentchat-tool`. Do NOT run `fixtures-bless`. The golden test skips cases without `expected.json`.

**Done when:**

```bash
cargo run -q -p cetra-capture -- run <the nine cases> --check
cargo run -q -p cetra-capture -- run --all --check
cd tools/capture/py && uv run ruff format --check && uv run ruff check && uv run mypy
```

All pass. The `--all` run is the guard on the existing cases. If an existing case drifts, stop and report the diff.

**Commit:** `Capture native GenAI framework runs (input only)`

## IN4.2 — TypeScript captures (input only)

**Goal:** captured `input.json` for Mastra and VoltAgent.

**Steps.**
1. Add the § Versions packages to `tools/capture/ts/package.json` with exact versions, then `pnpm --dir tools/capture/ts install`. The `minimumReleaseAge` setting must allow them; if it refuses one, stop and report.
2. Add `voltagent` to `GROUPS` in `tools/capture/src/scenario.rs` and in `crates/otel/tests/support/mod.rs`, and to `fixtures/otlp/README.md` § Layout.
3. Two scenarios, written from the TypeScript spike's programs (`/tmp/…/scratchpad/m4ts/`), with the same two turns as IN4.1:
   - **`genai/mastra-agent-tool`:** Mastra's exporter reads `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` itself, with `signals: { logs: false }`, and skips the harness `setup()`.
   - **`voltagent/agent-tool`:** the harness's provider, and `process.exit(0)` after `finish`, because VoltAgent keeps the process alive.

   Volatile keys and `span_names` follow the spike's § Nondeterminism: drop `processor_run` and memory spans.
4. Capture without blessing.

**Done when:**

```bash
cargo nextest run -p cetra-capture
cargo run -q -p cetra-capture -- run genai/mastra-agent-tool voltagent/agent-tool --check
pnpm --dir tools/capture/ts run typecheck
pnpm exec biome check tools/capture/ts
```

All pass.

**Commit:** `Capture Mastra and VoltAgent runs (input only)`

## IN4.3 — Registry row

**Goal:** `instrument=["autogen-agentchat"]`.

**Steps:** as IN2.3, with one row:
- name `autogen-agentchat`, extra `autogen-agentchat`;
- module `openinference.instrumentation.autogen_agentchat`, class `AutogenAgentChatInstrumentor` (check the name in the installed package);
- `openinference-instrumentation-autogen-agentchat>=0.1.29`.

The import test count becomes nineteen.

**Commit:** `Add AutoGen to the instrumentor registry`

---

## IN4.4 — Scope plumbing

**Goal:** the span's instrumentation scope name is available to detection, kind, field reading and payload reading. Behaviour is unchanged.

**Steps.**
1. In `normalize.rs` `accept_span`, the scope is already in hand (`Option<&InstrumentationScope>`, used for `agents::detect_agent`). Compute `let scope_name: &str = scope.map_or("", |s| s.name.as_str());` once.
2. Pass `scope_name: &str` to `detect(&bag, &span.events, scope_name)`, `span_kind(&mut bag, scope_name)` and `fill(&mut bag, &resource.view, scope_name, &mut warnings)`. Add `scope: &'a str` to `ReadContext` in `payload.rs`, set where `ReadContext` is built.
3. Every existing call site and test passes `""` where it has no scope.
4. Nothing reads the new parameter yet. Allow the unused name with a leading underscore only where clippy requires it, and remove the underscore in the phase that reads it.

**Tests:** existing tests only, updated to the new signatures.

**Done when:**

```bash
cargo nextest run -p cetra-otel
cargo clippy -p cetra-otel --all-targets -- -D warnings
```

Both pass, and every `expected.json` is unchanged (the golden test).

**Commit:** `Pass the instrumentation scope to the normalizer's readers`

## IN4.5 — Kind, provider, model, time and session rules

**Goal:** the non-payload rules R2, R3, R4, R9, R10 (session), M1, M3 and D-IN-23 items 2, 6 and 8.

**Steps.** In each case the spike's section is the evidence, and the spec 02 row named is edited in the same commit.

1. **Kinds** (`kind.rs`, spec 02 § Span kind):
   - `gen_ai.operation.name` row: add `execute_event_loop_cycle` → `chain`, `text_completions` → `llm`.
   - New row after the `gen_ai.operation.name` row: `mastra.span.type` ∈ {`model_generation`, `model_step`, `processor_run`, `workflow_step`} → `chain` (M1).
   - New rows before the model fallback, scope-gated:
     - when `scope_name == "agent_framework"` and there is no `gen_ai.operation.name`: `executor.id`, `edge_group.id` or `workflow.id` present → `chain`;
     - when the scope name starts with `mirascope` and `mirascope.type` is `trace` or `response_resume` → `chain`.
   - `lmnr.span.type` row: `LLM` and `CACHED` → `llm` only when the span has `gen_ai.request.model`, `gen_ai.response.model`, `llm.model_name`, or an input or output token count (any key the usage readers know); otherwise `chain`. Every existing Laminar case has a model and tokens, so it is unchanged.
   - Do not add `execute_structured_output` or `decision`: they are not captured.
2. **Provider** (`context.rs`, spec 02 rule 12):
   - The `FRAMEWORK_NAMES` skip applies to `gen_ai.provider.name` as well as `gen_ai.system`.
   - Add `microsoft.agent_framework` and `strands-agents`, keeping the list sorted.
   - Add the exact aliases `openai:completions` and `openai:responses` → `openai`, which the Mirascope capture shows.
3. **TTFT** (`usage.rs` or wherever `ttft_ns` is read; spec 02 `ttft_ns` row):
   - `gen_ai.server.time_to_first_token` is read in milliseconds (×1 000 000) on spans whose scope name starts with `strands`.
   - `mastra.completion_start_time` is read as an RFC 3339 instant, exactly like `langfuse.observation.completion_start_time` (`ttft = instant − start_time`, null when negative), after it in precedence.
4. **Model prefix** (`context.rs`, spec 02 `request_model`/`response_model` rows): on spans whose scope name starts with `mirascope`, when the raw lowercased `gen_ai.provider.name` is `p` and a model starts with `p/`, store the model without that prefix.
5. **Session** (`context.rs`, spec 02 § Context fields): append `mirascope.ops.session.id` to the end of the `session_id` sources.

**Tests** (unit, in the touched modules' tests):
- each operation value → kind;
- the `mastra.span.type` values, including `generic` staying `span`;
- the Agent Framework keys with and without the scope;
- Mirascope `mirascope.type` with and without the scope;
- Laminar `LLM` with and without a model and tokens;
- `gen_ai.provider.name=strands-agents` skipped and left unread, so the next source decides;
- the aliases;
- Strands TTFT `350` → 350 000 000 ns, and nothing on another scope;
- the Mastra instant, positive and negative;
- the Mirascope prefix strip (`openai/gpt-4o-mini` with provider `openai` → `gpt-4o-mini`), and no strip without the scope or when the prefix differs from the provider;
- the session source.

**Done when:**

```bash
cargo nextest run -p cetra-otel
cargo clippy -p cetra-otel --all-targets -- -D warnings
```

Both pass. The golden test must show no change to an existing `expected.json`.

**Commit:** `Read native GenAI frameworks' kinds, providers, TTFT and sessions`

## IN4.6 — Payload rules

**Goal:** R1, R5, R6, R8, R10 (input/output), R11, M2 and A1.

**Steps** (spec 02 § Input / output and § message conversion, edited with each):
1. **R5** (`conventions/genai.rs`): the fallback to the `gen_ai.client.inference.operation.details` event reads each key from the first event of that name that carries the key, not from the first event of that name.
2. **R6** (GenAI message reading, before conversion): in a `tool_call_response` part with `result` and no `response`, rename `result` to `response`.
3. **R1** (GenAI reader): on a span whose `gen_ai.operation.name` is `invoke_agent` and whose scope name is `pydantic-ai`, `pydantic_ai.all_messages` (JSON, canonical messages) → input, and `final_result` → output (rule 10). Both come after `gen_ai.input.messages`/`gen_ai.output.messages`, and both are payload attributes.
4. **R10** (I/O): on spans whose scope name starts with `mirascope`, `mirascope.trace.arg_values` → input (json) and `mirascope.trace.output` → output (rule 10), when nothing higher filled them.
5. **M2:** on a span with `mastra.span.type = T` (`T` matching `[a-z_]+`), `mastra.<T>.input` → input and `mastra.<T>.output` → output (rule 10), when nothing higher filled them.
6. **R11 (legacy events):** a converter for Bedrock-Converse content blocks, in `convert/`, used by the legacy GenAI event path when a message's `content` is an array of blocks with `text`, `toolUse` or `toolResult` keys:
   - `{"text": s}` → text part;
   - `{"toolUse": {"toolUseId", "name", "input"}}` → `tool_call` `{id, name, arguments: input}`;
   - `{"toolResult": {"toolUseId", "content": [blocks], "status"}}` → `tool_call_response` `{id, response: <the text of its text blocks, joined with "\n", or the JSON of its content when there are no text blocks>}`.

   An unknown block keeps the existing failure path (`messages:genai:<field>`).
7. **R8** (`conventions/openinference.rs`): a flattened message with role `function`, no `message.content`, and `llm.input_messages.N.function.M` JSON objects `{name, content, call_id, is_error}` becomes one `tool` message with one `tool_call_response` part per object: `{id: call_id, response: content}`, with `name` and `is_error` kept as extras the way the converter keeps unknown fields.
8. **A1** (`conventions/openinference.rs` `mime_payload`): a value labelled `application/json` that does not parse as JSON is `Payload::Text`, with no error.
   - Delete the now-unused failure `bool` from `mime_payload` and `raw_value`, and the two `if failed` pushes at their call sites in `read`.
   - Spec 02's OpenInference row says so.

**Tests:**
- R5 with three events, the key on the second;
- R6;
- R1 with and without the scope;
- R10;
- M2 for two values of `T`;
- R11 for each block type, mixed blocks, a `toolResult` with JSON content, and an unknown block;
- R8 with two function objects;
- A1: JSON-labelled plain text gives text with no error, and valid JSON still gives json.

**Done when:** as IN4.5. The only `expected.json` allowed to change is `openinference/agno-agent-tool`, and only if the golden test is run with bless. Do not bless here: IN4.8 blesses. The golden test is expected to fail on that one case only; report its diff, and stop if any other case fails.

**Commit:** `Read native GenAI frameworks' inputs, outputs and Converse messages`

## IN4.7 — VoltAgent convention

**Goal:** `Convention::Voltagent`, D-IN-6 as amended by D-IN-23 item 9, rules V1–V6 of the TypeScript spike.

**Steps.**
1. `crates/core/src/enums.rs`: add `Voltagent` (serde `voltagent`) to `Convention`, in precedence position right after `Genai`. Update the snapshot test the core crate has for enums (bless with the crate's documented bless variable, if one exists, or update the snapshot file).
2. `detect.rs`: `voltagent` is present when the span has any `voltagent.*` attribute, or its scope name starts with `@voltagent/`. `PRECEDENCE` gains it after `Genai`. `slot` and `mark` follow the existing pattern. `payload.rs`'s precedence ordinal (`rank`) follows.
3. `conventions/voltagent.rs` (registered in `conventions/mod.rs` like the others), read only on `voltagent` spans:
   - **V2, kind:**
     - `span.type`: `llm` → `llm`, `tool` → `tool`;
     - without `span.type`: `entity.type` `agent` → `agent`;
     - the other values the spike lists are not mapped (not captured).
   - **V3, model:** `llm.model`, then `ai.model.name`, after `gen_ai.request.model`.
   - **V4, tokens** (`llm` spans): `llm.usage.prompt_tokens` → input, `llm.usage.completion_tokens` → output, `llm.usage.cached_tokens` → cache read, `llm.usage.reasoning_tokens` → reasoning. Token semantics: input includes cache, output includes reasoning (spec 02 § Token semantics gains a VoltAgent row).
   - **V5, input and output:**
     - `llm.messages` (JSON array of AI SDK messages) → input messages through the existing Vercel `CoreMessage` converter, on `llm` spans;
     - otherwise plain `input` → input and `output` → output (rule 10).

     The plain `input`/`output` rule is written once and shared with D-IN-6's Pipecat scope (`pipecat*`), which M6 uses.
   - **V6, session:** `conversation.id`, else `voltagent.conversation_id`.
4. Regenerate `crates/server/openapi.json` and `web/src/api/schema.gen.ts` with `just openapi`.
5. `crates/otel/tests/proptest.rs`: add `voltagent` keys to the generator the same way the other conventions appear.
6. Spec 02: the precedence line, the detection table row, and each mapping table's VoltAgent entries.

**Tests:**
- detection by key and by scope;
- precedence over `openinference` for `llm.model` and `llm.usage.*`;
- each V rule;
- a span with `llm.*` keys and no VoltAgent signal stays OpenInference.

**Done when:**

```bash
cargo nextest run -p cetra-core -p cetra-otel
cargo nextest run -p cetra-server -E 'test(openapi)'
cargo clippy -p cetra-core -p cetra-otel -p cetra-server --all-targets -- -D warnings
cd web && pnpm exec tsc --noEmit
```

All pass, apart from the golden failure on `openinference/agno-agent-tool` carried over from IN4.6.

**Commit:** `Add the VoltAgent convention`

---

## IN4.8 — Bless and OpenHands

**Goal:** every M4 case has a reviewed `expected.json`.

**Steps.**
1. `just fixtures-bless`. It writes `expected.json` for the eleven new cases and rewrites `openinference/agno-agent-tool`. Stop if any other case changes.
2. Check each new `expected.json` against the spike's predictions:
   - the kinds the rules give;
   - provider `openai` everywhere, not `strands-agents` or `microsoft.agent_framework`;
   - tokens on `llm` spans only, with no double counting (AG2 `record_usage` is `span`; AutoGen has one `llm` per call);
   - cost `computed`;
   - input and output present where the spikes say: the Pydantic AI agent span, Mirascope trace spans, Mastra agent and step spans, and Strands content in both the legacy and latest cases;
   - TTFT on Strands and Mastra;
   - session on Mirascope;
   - VoltAgent spans with `conventions` containing `voltagent`;
   - no `cetra.normalize_error`, except where a spike documents an expected one.

   Stop and report every deviation.
3. `openinference/agno-agent-tool`: the agent span's output is the text, with no `cetra.normalize_error`.
4. **OpenHands, hand-written:** `fixtures/otlp/laminar/openhands-agent/`.
   - `case.json`: `origin: "hand_written"`, a `description`, `spec_refs` (`02 § Mapping tables / Span kind`, `02 § Cost computation`), and `notes` naming the source (the M4 spike's OpenHands dump, openhands-sdk 1.62.0 and lmnr 0.8.8).
   - `input.json`: the spike's dumped spans, as OTLP JSON. Only these are kept: the wrapper `llm.<model>` span (`lmnr.span.type=LLM`, `gen_ai.usage.cost`, no model or tokens), its child model span, and the agent and tool spans. Ids and times are fixed values as in other hand-written cases (`fixtures/otlp/edge/*`). Payloads are shortened to the scenario's text.
   - Bless it, and check that the wrapper is `chain`, so its reported cost does not add to the child's.

**Done when:**

```bash
cargo nextest run -p cetra-otel --test golden --test fixtures
cargo run -q -p cetra-capture -- run --all --check
```

Both pass.

**Commit:** `Bless M4's captured cases and add the OpenHands case`

## IN4.9 — Pages

**Goal:** eleven pages:
- `pydantic-ai`, `semantic-kernel`, `strands`, `bedrock-agentcore`, `agent-framework`, `ag2`, `autogen` and `mirascope` (Python);
- `openhands` (Python);
- `mastra` and `voltagent` (TypeScript).

**Steps.** Pages follow the M2 template, in the shape of the M3 pages: one tab, `{/* untested: … */}` samples written from the capture programs without the harness, **What you get** from `expected.json`, and **Limits** from `case.json` versions plus these warnings:
- **Semantic Kernel:** message content is sent as OTLP logs, which cetra does not store, so model spans show no messages.
- **Strands:** both its default and `OTEL_SEMCONV_STABILITY_OPT_IN=gen_ai_latest_experimental` work.
- **Bedrock AgentCore:** the agent code is Strands (link the Strands page). The AgentCore runtime's own export to cetra is untested, because it needs AWS (D-IN-11). Its `<Backing>` cites the Strands cases.
- **Agent Framework:** workflow steps are `chain` spans; set `enable_sensitive_data=True` for content.
- **AG2:** usage-recording spans are plain spans; tokens are counted on model calls.
- **AutoGen:** enable only `instrument=["autogen-agentchat"]`, not `openai` as well.
- **Mirascope:** sessions come from Mirascope's session.
- **OpenHands:** setup only for the run; a hand-written case backs the shape. Set `LITELLM_LOCAL_MODEL_COST_MAP=True` to avoid a download, and never set `LMNR_PROJECT_API_KEY`, which sends spans to Laminar's cloud. Configure the OTLP exporter through `OTEL_EXPORTER_OTLP_*` as the spike describes.
- **Mastra:** set `signals: { logs: false }` on the exporter. Processor and chunk spans appear as plain spans.
- **VoltAgent:** the model span carries only the last step's tokens, so cost undercounts runs with tool calls.

**Done when:** as IN3.3 (`check-terms`, `check-samples`, `check-backing`, `skill-references`).

**Commit:** `Add pages for native GenAI frameworks, Mastra and VoltAgent`

## IN4.10 — Index, sidebar, records

**Goal:** as IN3.4, for the eleven pages.

**Steps.**
1. **Index and sidebar.**
   - Index rows in Frameworks: Python ✓ except Mastra and VoltAgent (TypeScript ✓ only).
   - Tiers: `captured`, except OpenHands (`hand-written`) and Bedrock AgentCore (`captured`, through the Strands cases).
   - Cards for every row, with short descriptions in the style of the existing cards.
   - The sidebar's Frameworks group stays sorted.
2. **Design.**
   - Add D-IN-23 (this plan's text) to the decisions table.
   - Amend D-IN-5's cell: AG2 `record_usage` withdrawn; Strands TTFT in milliseconds; Agent Framework by keys.
   - Amend D-IN-6's cell: detection by key or scope.
   - Amend D-IN-15's cell: the skip covers both keys; the new names.
   - § Contracts and edit points, the fixture group list: as D-IN-23 item 10.
   - § Agent frameworks rows: the routes as built.
   - **In `docs/mvp-decisions.md`,** the same amendments.
3. **Changelog and roadmap.**
   - **CHANGELOG `Added`:** pages and captured spans for the eleven frameworks, the `voltagent` convention, and the `autogen-agentchat` extra.
   - **CHANGELOG `Fixed`:** OpenInference outputs labelled JSON that are plain text (Agno) no longer record a normalize error.
   - **Roadmap:** add the M4 plan link; status "M1–M4 implemented"; decisions `D-IN-1`–`D-IN-23`.

**Done when:** as IN3.4.

**Commit:** `List M4's framework pages and record D-IN-23`

## After the last phase

The main session:
1. touches the crates that embed `CARGO_MANIFEST_DIR`, then runs `just check` once;
2. runs a Sonnet code review;
3. has a GLM 5.3 agent fix the review's findings;
4. pushes to PR #242.
