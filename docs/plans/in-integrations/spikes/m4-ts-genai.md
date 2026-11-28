# M4 spike: TypeScript native GenAI frameworks (Mastra, VoltAgent) and the Agno `output.value` rule

Run 2026-11-23 (track IN, M4). Node v26.13.0, npm 11 with `--before=2026-11-16T23:59:59Z` (the 7-day rule at the run date). Throwaway projects in
`/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m4ts/`:

- `fake.mjs`: in-process OpenAI fake. Chat completions (stream and non-stream) and Responses, by position: turn 1 = tool call `get_weather({"city":"Paris"})`, usage 82/17; turn 2 = text "It is sunny in Paris, 18C.", usage 131/12. `CACHE=1` adds `cached_tokens: 64` and `reasoning_tokens: 4` to turn 2.
- `recv.mjs`: in-process OTLP/HTTP protobuf receiver (decodes spans, can save the raw bodies).
- `mastra/run.mjs`, `mastra/wf.mjs` (a two-step workflow), `volt/run.mjs`, `volt/run2.mjs` (string model, streaming), `volt/run4.mjs` (global provider).
- `norm/`: a scratch crate that depends on `cetra-otel` by path and runs the real `normalize` on the saved protobuf bodies (the numbers under "today" below are the real normalizer output at this branch, not a reading of the spec). No tracked file changed.

Both frameworks ran the same agent: one tool `get_weather`, model `openai.chat("gpt-4o-mini")` from `@ai-sdk/openai` 3.0.136 with `baseURL` on the fake, two model calls.

## Findings that change the design

1. **D-IN-6's detection rule does not detect most VoltAgent spans.** "Any `voltagent.*` attribute" matches only the root agent span (`voltagent.operation_id`, always) and workflow spans (`voltagent.label`). The `llm`, `tool`, `memory`, `guardrail`, `retriever`, `embedding` and `middleware` spans carry **no** `voltagent.*` key (they carry `span.type`, `span.label`, `operation.id`, `entity.*`). The one marker every VoltAgent span carries is the instrumentation scope name `@voltagent/core` (default; `VoltAgentObservability({ instrumentationScopeName })` can change it). D-IN-6 and the design's `ReadContext` already plan to pass the scope name, so the fix is cheap: § Proposed normalizer rules, V1. D-IN-6 needs updating first.
2. **VoltAgent's `llm` span reports only the last step's tokens.** `llm:generateText` is one span per `generateText` call, and it records `response.usage` (AI SDK: the final step). The two-step run has one `llm` span with 131/12; the 82/17 of step 1 is on no `llm` span. The agent span carries the sum (`usage.prompt_tokens` 213, `usage.completion_tokens` 29) but, being an `agent`, its tokens are not normalized (rule 3). So cetra's cost for a VoltAgent run with tools undercounts by every earlier step. No normalizer rule can fix this; the page must say it.
3. **VoltAgent's `llm` span has no output.** Only `llm.messages` (the last 10 input messages, role and content) and `llm.finish_reason`. The answer text is on the agent span only (`output`).
4. **Mastra: the research's gaps are confirmed and the exact keys are known**, plus one the research missed: the agent span's input and output are in `mastra.agent_run.input` / `mastra.agent_run.output`, so an agent span has no input or output today. The `chat` span (the model call) already normalizes completely (kind, provider, model, tokens, cost, messages with the tool call).
5. **Agno (item 3): one fixture changes, and the fix removes a bool from the code.** § 3.

## Versions (the newest upload on or before 2026-11-16)

| Package | Pinned (date) | Latest at run (date) | Notes |
|---|---|---|---|
| `@mastra/core` | 1.83.0 (2026-11-12) | 1.85.0 (2026-11-18) | node `>=22.13.0`; peers `zod ^3.25 \|\| ^4`; resolves `@ai-sdk/provider-v6` 3.0.19 itself |
| `@mastra/otel-exporter` | 1.5.2 (2026-11-12) | 1.6.1 (2026-11-18) | peer `@mastra/core >=1.16.0-0 <2`; peer `@opentelemetry/exporter-trace-otlp-proto >=0.218.0 <0.300.0` (0.223.0, the capture pin, fits; 0.301.x does not) |
| `@mastra/observability` | 1.24.0 (2026-11-12) | n/a | a dependency of the exporter, pinned by it |
| `@voltagent/core` | 2.11.1 (2026-11-04) | same | peers `ai ^6.0.0`, `@ai-sdk/provider-utils 4.x`, `zod`; resolved `ai` 6.0.332 (2026-11-13) |
| `@ai-sdk/openai` | 3.0.136 (the capture pin `@ai-sdk/openai-v6`) | | spec v3 model; `openai.chat(...)` gives `modelVersion: v3`, `modelProvider: openai.chat` |
| `@opentelemetry/exporter-trace-otlp-proto` 0.223.0, `sdk-trace-base`/`-node`/`resources` 2.12.0, `api` 1.9.0, `zod` 4.6.7 | the capture pins | | |

The research's numbers (core 1.85.0, exporter 1.6.1) are after the cutoff. The M4 plan pins 1.83.0 and 1.5.2 and re-checks on the day (a newer exporter may change the keys below: the plan's recapture will say).

## 1. Mastra

### How it is wired (what ran)

```ts
new Mastra({
  agents: { agent },
  observability: new Observability({ configs: { otel: { serviceName, exporters: [
    new OtelExporter({
      provider: { custom: { endpoint: <url>, protocol: "http/protobuf" } },
      signals: { logs: false },
    }),
  ] } } }),
});
```

- `provider.custom.endpoint` is the full traces URL; a URL without a path also worked (the exporter appended `/v1/traces`). The `custom` provider does **not** read `OTEL_EXPORTER_OTLP_*`, so the scenario passes `requiredEnv("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")` (the capture runner sets it, `tools/capture/src/run.rs:160`). `headers` is a `custom` field, so the page's `Authorization: Bearer sk-cetra-...` works.
- By default the exporter also POSTs a log batch to `<base>/v1/logs` (one warn record: "No `storage` configured on Mastra"). `signals: { logs: false }` stops it (verified: only `/v1/traces` is received). The capture receiver accepts and discards `/v1/logs` anyway (`receiver.rs:221`), but the page should tell users to set it, since cetra's `/v1/logs` is for GenAI log events only.
- Batch delay 5 s; `await mastra.shutdown()` flushes (verified, 15 spans delivered; it takes about 8 s only when the endpoint is dead). Without storage Mastra warns and uses an in-memory store (no effect on the spans).
- Mastra uses the AI SDK model for a v3 spec model; it called `/v1/chat/completions` non-streaming twice (`generate`).
- Resource: `service.name` = `serviceName`, `service.version` = `@mastra/core` version, `telemetry.sdk.name=@mastra/otel-exporter`. Scope `@mastra/otel-exporter`, version = exporter version.

### Spans (15 for the two-step agent run; names, parents, span kind)

Tree: `invoke_agent weather-agent` > `generic weather-agent`, `model_generation gpt-4o-mini` > 2 x `agent_step weather-agent` > (`processor_run <name>` x2 per step, `chat gpt-4o-mini`, `model_chunk weather-agent` x1 or 2, `execute_tool get_weather` in step 1). OTLP span kind: `chat` CLIENT (3), every other span INTERNAL (1).

Every span carries `gen_ai.operation.name`, `mastra.span.type` and `mastra.metadata.runId`. All 15 are detected as `genai` only (`conventions=["genai"]`).

| Span (name) | `gen_ai.operation.name` | `mastra.span.type` | Other keys (value shape) |
|---|---|---|---|
| `invoke_agent weather-agent` | `invoke_agent` | `agent_run` | `gen_ai.agent.id`, `gen_ai.agent.name`, `gen_ai.system_instructions` (text), `gen_ai.tool.definitions` (`["get_weather"]`: names, not schemas), `mastra.agent_run.input` (plain text), `mastra.agent_run.output` (JSON text `{"text":"...","files":[]}`), `gen_ai.conversation.id` when a thread/conversation exists, `mastra.agent_run.max_steps` |
| `generic weather-agent` | `generic` | `generic` | `mastra.generic.output` (`{"toolCount":1}`) |
| `model_generation gpt-4o-mini` | `model_generation` | `model_generation` | `mastra.model_generation.input` (JSON, whole prompt incl. `providerOptions.mastra.createdAt`), `.output` (JSON `{files, reasoning, sources, text, toolCalls, warnings}`). No usage, no model key |
| `agent_step weather-agent` | `agent_step` | `model_step` | `mastra.model_step.input` / `.output` (JSON), `.step_index`, `.is_continued`, `mastra.metadata.modelMetadata` (JSON), `mastra.metadata.headers` (JSON incl. the response `date`) |
| `processor_run <processor name>` | `processor_run` | `processor_run` | `mastra.processor_run.input` (JSON, full prompt with `createdAt`), `.output` (`{"shortCircuited":false}`) |
| `chat gpt-4o-mini` | `chat` | `model_inference` | the model call, see below |
| `model_chunk weather-agent` | `model_chunk` | `model_chunk` | `mastra.model_chunk.output` (JSON), `mastra.metadata.toolCallId/toolName` |
| `execute_tool get_weather` | `execute_tool` | `tool_call` | `gen_ai.tool.name`, `gen_ai.tool.call.id`, `gen_ai.tool.call.arguments` (JSON text), `gen_ai.tool.call.result` (JSON text), `gen_ai.tool.description`, `gen_ai.tool.type=tool` |
| `invoke_workflow calc` (a workflow run, `wf.mjs`) | `invoke_workflow` | `workflow_run` | `mastra.workflow_run.input/output/status` |
| `workflow_step double` | `workflow_step` | `workflow_step` | `mastra.workflow_step.input/output/status/step_id` |

`chat` span keys: `gen_ai.request.model=gpt-4o-mini`, `gen_ai.response.model=gpt-4o-mini-2024-07-18`, `gen_ai.provider.name=openai.chat`, `gen_ai.usage.input_tokens/output_tokens` (82/17, then 131/12), `gen_ai.usage.cache_read.input_tokens`, `gen_ai.usage.reasoning_tokens`, `gen_ai.response.finish_reasons=["tool-calls"]` (AI SDK spelling; `["stop"]`), `gen_ai.input.messages` and `gen_ai.output.messages` in the **semconv parts shape** (`{"role":"user","parts":[{"type":"text","content":"..."}]}`; tool call part `{"type":"tool_call","id","name","arguments":"<JSON text>"}`), `gen_ai.agent.id/name`, `mastra.completion_start_time` (RFC 3339 with milliseconds, `2026-11-23T17:53:04.491Z`; 3 ms and 28 ms after the span start in the two calls, both within the span).

With `CACHE=1`: step 2 `gen_ai.usage.input_tokens=131` with `cache_read.input_tokens=64` (input **includes** cache) and `reasoning_tokens=4` (output includes reasoning). Both match the GenAI row of the token table.

### Today, per spec 02 (real normalizer output)

| Span | cetra kind | provider | tokens | input | output | Verdict |
|---|---|---|---|---|---|---|
| `invoke_agent` | agent | none | null (rule 3) | null | null | **gap**: `system_instructions` and `tool_definitions` (`["get_weather"]`) fill, but input/output missing |
| `generic`, `model_generation`, `agent_step`, `processor_run`, `model_chunk` | `span` | none | null | null | null | **gap**: all `span`, payload in unread `mastra.*` keys |
| `chat` | llm | `openai` (`openai.chat` alias) | 82/17 + total 99, cost computed 0.00015 | messages (system, user) | messages with `tool_call` part, `arguments` parsed to `{"city":"Paris"}`, finish reason `tool-calls` | **ok** except TTFT (`mastra.completion_start_time` stays in `attributes`) |
| `execute_tool` | tool | none | null | `{"city":"Paris"}` json | `{"temperature_c":18,"city":"Paris"}` json | ok |

No double counting: exactly one span per model call carries `gen_ai.request.model` and usage. In older core/observability pairings (no `model-inference-span` feature) `model_generation` takes the `chat` role instead (`features.d.ts`); the operation-name row decides it as `llm` first, so the rules below do not touch that case.

### Gaps and proposed rules

- **M1 kind** (`mastra.span.type`, after `gen_ai.operation.name` in `kind.rs`, so the existing `chat`/`execute_tool`/`invoke_agent`/`invoke_workflow` rows still win): `model_generation`, `model_step`, `processor_run`, `workflow_step` → `chain`. `generic` and `model_chunk` get no row (`span`). Not captured, so no row until a case exists: `workflow_conditional`, `workflow_conditional_eval`, `workflow_parallel`, `workflow_loop`, `workflow_sleep`, `workflow_wait_event` (all workflow control flow, `chain` by the same logic), `mapping`, `rag_*`, `scorer_*`, `classifier_evaluation`, `memory_operation`, `graph_action`, `skill_*`, `workspace_action`, `agent_signal`, `client_tool_call`, `mcp_server_request`. The `SpanType` enum has 34 values (`@mastra/core` 1.83.0); `mcp_tool_call` and `provider_tool_call` arrive as `execute_tool`, `rag_embedding` as `embeddings`.
- **M2 input and output**: on a span with `mastra.span.type = T`, `mastra.T.input` is the input and `mastra.T.output` the output, when the GenAI sources gave nothing. Value: rule 10 (a JSON object or array text is `json`, any other text is `text`; `mastra.agent_run.input` is plain text, the others are JSON text). Types captured: `agent_run`, `generic` (output), `model_generation`, `model_step`, `processor_run`, `model_chunk` (output), `workflow_run`, `workflow_step`. Consumed as payload attributes (rule 2). `processor_run` and `model_generation` inputs repeat the whole prompt with `createdAt` stamps; they are big but the library's choice.
- **M3 TTFT**: `mastra.completion_start_time` is an instant (RFC 3339 text), read like `langfuse.observation.completion_start_time` (`ttft = instant - start_time`, null when negative). D-IN-5 already says so.
- Already fine, no rule: `gen_ai.conversation.id` (session), `gen_ai.usage.*`, `gen_ai.tool.*`, the `openai.chat` alias. `gen_ai.tool.definitions` as names stays a list of strings (rule 7 expects schema objects; a list of strings is stored as is). `mastra.metadata.*` and `mastra.tags` stay in `attributes`. `gen_ai.usage.cache_creation.5m/1h_input_tokens` was not emitted in this run.
- No new `Convention` variant: Mastra spans are `genai`, so M1 to M3 are key reads keyed on `mastra.span.type` / `mastra.*`.

### Offline capture with `tools/capture/ts`

Yes. The mock's OpenAI chat route serves it (`api: "openai.chat"`, non-streaming; `agent.stream` would use the streaming route). Harness and project changes:

- `package.json`: add `@mastra/core` 1.83.0, `@mastra/otel-exporter` 1.5.2, `@mastra/observability` 1.24.0 (exact, `minimumReleaseAge`); reuse `@ai-sdk/openai-v6`; `zod` 4.6.7 is already there. The capture `libraries` list in `scenario.json` names `@mastra/core`, `@mastra/otel-exporter` (`writeMeta` reads `node_modules/@mastra/core/package.json` and checks `name`; scoped names work with `path.join`).
- `harness.ts`: `setup()` registers a `NodeTracerProvider`, which Mastra does not use (its exporter owns the provider). The scenario does not call `setup()`; it builds the `OtelExporter` with `provider.custom.endpoint = requiredEnv("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")`, `signals: { logs: false }`, `serviceName: process.env.OTEL_SERVICE_NAME`, then `await mastra.shutdown()` and `writeMeta(aliases)` (already exported; `finish()` needs a provider, so a scenario-local `try/finally` is enough, no harness change). Model: `createOpenAI({ baseURL: mockUrl("/v1"), apiKey: process.env.OPENAI_API_KEY }).chat(MODEL)`.
- Cases (group `mastra`): `agent-tool` (this run) and `workflow` (two steps, no model; pins `workflow_run`/`workflow_step`).

### Nondeterminism to pin

- `mastra.metadata.runId`: random UUID on every span: `volatile_attributes`.
- `mastra.metadata.headers` (agent_step): the mock's response headers incl. `date`: `volatile_attributes` (or drop `agent_step` spans via `span_names`, but M1 needs one).
- `mastra.model_generation.input`, `mastra.processor_run.input`: JSON text with `createdAt` epoch milliseconds: `volatile_attributes`, or keep out of `span_names` (`processor_run` spans carry nothing the rules read; the processor names are Mastra internals "Provider History Compat", "Unsupported File Handler" and will change between versions). Recommended `span_names`: `invoke_agent weather-agent`, `generic weather-agent`, `model_generation gpt-4o-mini`, `agent_step weather-agent`, `chat gpt-4o-mini`, `execute_tool get_weather`, `model_chunk weather-agent`.
- `mastra.completion_start_time`: `timestamp_attributes` with format `rfc3339`.
- `model_chunk` spans (2 in step 1, 1 in step 2, stable over 3 runs) are an implementation detail of the exporter: keep at most one in `span_names` (names are not unique, so all with that name are kept) or none.
- The exporter batches: all 15 spans arrived in one `POST` (10357 bytes); children before parents.
- `agent.generate` is non-streaming here; the usage and finish reasons are stable.

## 2. VoltAgent

### How it is wired

Two routes worked and gave identical spans:

1. The documented one: `new VoltAgentObservability({ serviceName, spanProcessors: [new SimpleSpanProcessor(new OTLPTraceExporter({ url, headers }))] })` passed to `new VoltAgent({ agents, observability })`.
2. **A global provider.** With a `NodeTracerProvider` registered (what `harness.ts` `setup()` does) and `new VoltAgent({ agents })` using its default observability, VoltAgent's tracer is the global one and the three spans reach the harness's exporter with no change to the harness (resource is the user's). The capture scenario therefore uses `setup()` unchanged.

The process does **not** exit by itself after the run (`done` printed, still alive after 40 s with every server closed; VoltAgent keeps a handle): the scenario must `await finish(provider)` and then call `process.exit()`. (Mastra exits by itself.) The run also logs `AI SDK Warning: System messages in the prompt...` on stderr.

Three spans for the plain run (scope `@voltagent/core` version `1.0.0`: a fixed default of `serviceVersion`, not the package version). With `userId`/`conversationId` options VoltAgent adds `memory.read`, `memory.write` x3 and `memory.steps.write` x2 spans (kind INTERNAL, `span.type=memory`), each with `input` (JSON with the message). OTLP span kind: `llm` and `tool` CLIENT (3), the root and memory spans INTERNAL (1).

### Keys, per span (plain run)

| Span | Keys |
|---|---|
| `weather-agent` (root, parent none) | `entity.id/name/type=agent`, `operation.id`, **`voltagent.operation_id`** (same UUID), `input` (plain text), `output` (plain text), `agent.instructions`, `agent.messages` (JSON), `agent.messages.ui` (JSON, random ids), `agent.state=completed`, `agent.stateSnapshot` (JSON), `agent.workingMemory.enabled`, `ai.model.name=gpt-4o-mini`, `ai.model.max_steps`, `ai.response.finish_reason`, `usage.prompt_tokens/completion_tokens/total_tokens/cached_tokens/reasoning_tokens` (the **sum**: 213/29/242). With a string model `"openai/gpt-4o-mini"` (Responses API route): `ai.model.provider=openai` (and `ai.model.name` is still `gpt-4o-mini`). With a conversation: `voltagent.conversation_id`, `conversation.id`, `user.id` |
| `llm:generateText` / `llm:streamText` (child, CLIENT) | `span.type=llm`, `entity.*`, `operation.id`, `llm.operation`, `llm.model=gpt-4o-mini`, `llm.stream`, `llm.messages` (JSON array of `{role, content}`, content is a string or an AI SDK parts array; last 10 messages), `llm.messages.count`, `llm.tools` (comma-separated names), `llm.tools.count`, `llm.usage.prompt_tokens/completion_tokens/total_tokens` (131/12/143: **last step only**), `llm.usage.cached_tokens` and `llm.usage.reasoning_tokens` (64 and 4 under `CACHE=1`; absent otherwise), `llm.finish_reason`, `llm.attempt`, `llm.max_retries`, `llm.model_index`, `usage.prompt_context.*` (estimates). **No `llm.provider`** in either model form (the source sets it only when `modelName` contains `/`, and it did not for the resolved model); no output; no `ai.*` |
| `tool.execution:get_weather` (child, CLIENT) | `span.type=tool`, `span.label=get_weather`, `entity.*`, `operation.id`, `tool.name`, `tool.call.id`, `tool.description`, `tool.parameters` (JSON of the zod internals), `input` (JSON args text), `output` (JSON result text) |

Prefix census (this run and `index.mjs` 2.11.1): `voltagent.*` only `voltagent.operation_id` (root), `voltagent.conversation_id` (root, with a conversation), `voltagent.label` (workflow root, workflow steps, the `write_todos` tool span), `voltagent.type` (only when a span is created through `VoltAgentObservability.startSpan(name, { type })`, never by the agent), `voltagent.is_subagent` (subagent spans). `entity.*`: every agent or workflow span. `llm.*`: `llm` spans only. `ai.*`: root agent span (and `ai.model.provider` only for a `provider/model` string). `tool.*`: tool spans. Plain `input`/`output`: root agent, tool, memory, retriever, workflow root. `span.type` values in 2.11.1: `llm`, `tool`, `memory`, `guardrail`, `middleware`, `embedding`, `vector`, `retriever`, `summary`, `scorer`, `workflow-step`; root agent and workflow spans have none (`entity.type` is `agent` or `workflow`).

**Answer to "does EVERY VoltAgent span carry `voltagent.*`": no.** In the plain run, 1 of 3 does.

Tokens semantics (from `CACHE=1`): `llm.usage.prompt_tokens=131` with `cached_tokens=64` (input includes cache); `completion_tokens=12` with `reasoning_tokens=4` (output includes reasoning). Same as the GenAI and AI SDK rows.

### Today, per spec 02 (real normalizer output)

| Span | conventions | cetra kind | model, provider | tokens | input | output |
|---|---|---|---|---|---|---|
| root agent | `vercel` (the `ai.*` keys) | `span` | none (`ai.model.name` is not a source; `ai.model.id` is) | null | null | null |
| `llm:generateText` | `openinference` (the `llm.*` keys) | `span` | none (`llm.model` is not a source; `llm.model_name` is) | null (`llm.usage.prompt_tokens` is not a source) | null | null |
| `tool.execution:get_weather` | `openinference` (the `tool.*` keys) | `span` | none | null | null (`input` is not `input.value`) | null |

So the research's "VoltAgent is not read at all" holds, with one correction: the root span is `vercel`, not `openinference`, and nothing lists a `voltagent` convention. All payload keys stay in `attributes` (unconsumed, so kept up to the truncation limits).

### Proposed rules (the `voltagent` convention): see "Proposed normalizer rules", V1 to V6.

### Offline capture

Yes, with no harness change except the exit: `setup()` as is (route 2), model `createOpenAI({ baseURL: mockUrl("/v1") }).chat(MODEL)` from `@ai-sdk/openai-v6`, `await finish(provider, ALIASES)`, then `process.exit(0)`. Project changes: `@voltagent/core` 2.11.1; its peer `ai ^6.0.0` is imported by the bare name `ai`, but the capture project only has the alias `ai-v6` (`npm:ai@6.0.322`): add a plain `"ai": "6.0.322"` (pnpm would otherwise auto-install a separate peer copy; the spike resolved `ai` 6.0.332). `@voltagent/logger` is a dependency (INFO logs go to stdout/stderr; nothing contains payloads in the plain run, but the capture scrubber should check the `AI SDK Warning`/logger lines do not break a run). Group `voltagent`, case `agent-tool`; an optional second case with `userId`/`conversationId` pins `conversation.id` (the extra `memory.*` spans are noise: `span_names` keeps the three spans).

### Nondeterminism to pin

- `operation.id` and `voltagent.operation_id` (UUID, every span): `volatile_attributes` both keys.
- `agent.messages.ui` (random message ids), `agent.stateSnapshot` (no clock, but it embeds names and the tool schema; stable) : `agent.messages.ui` volatile.
- The memory spans (with a conversation) carry message ids and `createdAt`-like fields in `input`: keep them out via `span_names`.
- Streaming: `llm:streamText` has the same keys (`llm.stream=true`); a non-streaming scenario is enough.
- VoltAgent's own logger lines are not spans; nothing to pin.

## 3. Agno `output.value` with `application/json`

### The input

`fixtures/otlp/openinference/agno-agent-tool/input.json`, span `weather_agent.run` (`openinference.span.kind=AGENT`): `output.value = "It is sunny in Paris."` and `output.mime_type = "application/json"`. Agno labels a plain string as JSON. The expected output today: `output = "\"It is sunny in Paris.\""` (a JSON string literal), `output_format = "json"`, `output_preview = "\"It is sunny in Paris.\""`, and `cetra.normalize_error = messages:openinference:output`. A text value is not a conversion failure, so the error is wrong.

### Where

`crates/otel/src/conventions/openinference.rs`:

- `read` (lines 62 to 67 for the input, 85 to 90 for the output) takes `raw_value(ctx, INPUT_VALUE, INPUT_MIME)` / `raw_value(ctx, OUTPUT_VALUE, OUTPUT_MIME)`, which returns `(Payload, bool)`, and pushes `messages:openinference:input` / `:output` when the bool is true.
- `raw_value` (line 188) passes through `mime_payload` (line 208), the only place the bool can be true: with `mime == Some("application/json")`, `decode_json_text` returning `None` and the value a string gives `(Payload::Json(Json::String(text)), true)` (line 218); a non-string value `decode_json_text` does not decode (int, double, bool, bytes) gives `(Payload::Json(to_json(value)), true)` (line 220).

### The rule change

A value labelled `application/json` that does not parse as JSON is the text it is. No error.

In `mime_payload`: when `mime == Some("application/json")`:

1. `decode_json_text` gives a value: `Payload::Json(value)` (unchanged).
2. Otherwise, a string value: `Payload::Text(text)` (was `Json(String)` with an error).
3. Otherwise, a non-string value: `Payload::Json(to_json(value, warnings))` (same payload, no error; an int, double or bool is valid JSON).

Then no path of `mime_payload` returns an error: remove the `bool` from `mime_payload` and `raw_value`, and the `if failed { io.errors.push(...) }` blocks that wrap them at the two call sites (the flattened-message `messages()` path keeps its own bool and error codes). No dead `bool` stays (AGENTS.md: no unnecessary abstractions). Add unit tests in `openinference.rs`: JSON-mime text that is not JSON becomes `text` with no error (input and output), JSON-mime text that is JSON stays `json`, a JSON-mime JSON string literal `"\"hi\""` keeps today's behaviour (it parses), a numeric `output.value` with the JSON mime is `json` with no error.

Spec 02 changes with it: § Input / output, OpenInference row ("format from `input.mime_type`: `application/json` → json, else text") becomes "`application/json` and the value parses as JSON → json; otherwise text, including a value labelled `application/json` that is not JSON (Agno labels its plain-text agent output that way)". The sentence under "Message conversion" about error codes needs no change (the `openinference` codes stay possible from flattened messages), but the code list in § Rule 21 is unaffected. `website/src/content/docs/docs/integrations/agno.mdx` line 71 states that cetra "records a normalize error on it"; it becomes "stores the agent span's output as text". The design's M4 text already scopes this fix.

### Which fixtures change

`grep -rl cetra.normalize_error fixtures/otlp/*/*/expected.json` finds two files: `fixtures/otlp/edge/messages-conversion-failure/expected.json` (a `gen_ai.input.messages` case: `messages:genai:input`, unchanged) and `fixtures/otlp/openinference/agno-agent-tool/expected.json`. Only the latter changes: span `weather_agent.run` gets `output = "It is sunny in Paris."`, `output_format = "text"`, `output_preview = "It is sunny in Paris."` (no quotes), and loses `cetra.normalize_error` (`output.mime_type = application/json` stays as a consumed attribute under 256 bytes, rule 2). No other expected file has an `openinference` error code, and `crates/otel/tests/fixtures.rs`, `proptest.rs` and the fuzz regressions do not name the code (grep). Re-bless with `CETRA_BLESS=1` (`crates/otel/tests/golden.rs`) and review the diff to be exactly these three fields.

## Proposed normalizer rules

Everything here needs a captured case (D-IN-5): `mastra/agent-tool`, `mastra/workflow`, `voltagent/agent-tool`. Spec 02 sections to edit: § Convention detection (precedence line, detection table, `voltagent` row), § Mapping tables (Span kind, Input / output, Model/provider/usage, Context fields), § Token semantics, and rule 2's note on format hints is unchanged.

### Mastra (D-IN-5; no new convention, keyed on `mastra.span.type`)

| Id | Rule |
|---|---|
| M1 | Span kind: row after `gen_ai.operation.name`: `mastra.span.type` ∈ {`model_generation`, `model_step`, `processor_run`, `workflow_step`} → `chain`. Other values do not decide (they stay `span`, e.g. `generic`, `model_chunk`) until a case captures them. The `chat`, `execute_tool`, `invoke_agent`, `invoke_workflow` operation names keep deciding first |
| M2 | Input / output: on a span with `mastra.span.type = T` (`[a-z_]+`), `mastra.T.input` → input and `mastra.T.output` → output, when no higher source filled it; rule 10 decides `json` or `text`; payload attributes are consumed (rule 2). Captured T: `agent_run`, `generic`, `model_generation`, `model_step`, `processor_run`, `model_chunk`, `workflow_run`, `workflow_step` |
| M3 | TTFT: `mastra.completion_start_time` (RFC 3339 instant) is read like `langfuse.observation.completion_start_time` |

No change for provider (`openai.chat` alias exists), model, tokens, cache, reasoning, tool name (the span name `execute_tool get_weather` and `gen_ai.tool.name` are kept), session (`gen_ai.conversation.id`).

### VoltAgent (D-IN-6; `Convention::Voltagent`)

| Id | Rule |
|---|---|
| V1 | **Detection**: a span is `voltagent` when it has any `voltagent.*` attribute **or** its instrumentation scope name is `@voltagent/core` (starts with `@voltagent/`). Needs the scope name in `detect` (today `detect(bag, events)`; add the scope, from the same crate-internal source `ReadContext` gets). Precedence: after `genai`, before `openinference` (D-IN-6 unchanged). `conventions` still also lists `openinference` (`llm.*`, `tool.*`) or `vercel` (`ai.*`) when those keys exist. D-IN-6's wording "detected by any `voltagent.*` attribute" must change to include the scope name (finding 1) |
| V2 | Span kind (a row after `gen_ai.operation.name`, only on `voltagent` spans): `span.type` `llm` → `llm`; `tool` → `tool`; `retriever` → `retriever`; `embedding` → `embedding`; `guardrail` → `guardrail`; `scorer` → `evaluator`; `workflow-step`, `middleware` → `chain`; `memory`, `summary`, `vector` do not decide (`span`). When there is no `span.type`: `entity.type` `agent` → `agent`, `workflow` → `chain`. Only captured values get a row: `llm`, `tool`, `agent` from `voltagent/agent-tool`; the rest are source-read from `@voltagent/core` 2.11.1 and wait for a case (a workflow case is cheap: no model) |
| V3 | Model: `request_model` also reads `llm.model` (`llm` spans) and `ai.model.name` (the root agent span), after `gen_ai.request.model`, only on `voltagent` spans. Provider: no new source (`llm.provider` and `ai.model.provider` are already read, and VoltAgent sets them only for `provider/model` strings); otherwise the endpoint or the price table decide |
| V4 | Tokens (`llm` spans only, rule 3): `llm.usage.prompt_tokens` → input, `llm.usage.completion_tokens` → output, `llm.usage.cached_tokens` → cache read, `llm.usage.reasoning_tokens` → reasoning; `llm.usage.total_tokens` is already read. Token semantics: input includes cache, output includes reasoning (verified with `CACHE=1`: 131 with 64 cached, 12 with 4 reasoning). The root's `usage.*` stay in `attributes` (agent span). The limitation of finding 2 goes on the page |
| V5 | Input / output (the plain `input`/`output` rule of D-IN-6, one rule shared with the Pipecat scope): `input` → input and `output` → output on `voltagent` spans, rule 10 decides `json` or `text` (root agent: text; tool: JSON args and JSON result). `llm.messages` (a JSON array of AI SDK `{role, content}` messages, content a string or a parts array) → input messages through the existing Vercel `CoreMessage` converter, on `voltagent` `llm` spans, ahead of `input`; `llm` spans have no output. Payload attributes consumed. `agent.instructions` may fill `system_instructions` on the agent span; optional, not needed by the page |
| V6 | Context: `conversation.id` (and `voltagent.conversation_id` as the fallback) → `session_id` on `voltagent` spans (every span carries `conversation.id` when a conversation exists; `user.id` is already a `user_id` source). Tool name: nothing to read: the span name is `tool.execution:<name>` and `tool.name` stays in `attributes`; no rename (the coding-agent rename is not extended) |

### The Agno rule (item 3)

A1: in `crates/otel/src/conventions/openinference.rs`, a value labelled `application/json` that does not parse as JSON is `text`, and the `bool` returned by `mime_payload` and `raw_value` goes away with the two `if failed` blocks that used it. Spec 02 § Input / output OpenInference row, `agno.mdx` line 71 and `fixtures/otlp/openinference/agno-agent-tool/expected.json` follow. No other fixture changes.

## Unverified

- Mastra 1.85.0 / exporter 1.6.1 (published 2026-11-18, after the cutoff) were not run; the plan's recapture shows any key change.
- Mastra streaming (`agent.stream`): not run (same exporter code path; the mock streams).
- Mastra `rag_*`, `scorer_*`, `memory_operation`, MCP and conditional workflow span types: read from the exporter source only (`getOperationName`, `getAttributes`), not captured.
- VoltAgent subagents, workflows, guardrails, retrievers, `userId` memory spans with a real store, and `providerMetadata` cost (`usage.cost`, OpenRouter only): read from source only.
- VoltAgent with `instrumentationScopeName` overridden: detection then falls back to `voltagent.*` keys (root and workflow spans only).
