# M3 spike: OpenInference frameworks (Instructor, Agno, smolagents, Haystack, BeeAI, Guardrails AI, DeepAgents, Temporal, LangGraph, Claude Agent SDK)

Run 2026-11-23 (D-IN-17). Python 3.13, uv 0.13.6, `exclude-newer = 2026-11-16T23:59:59Z` (the 7-day rule at the run date), scripts in
`/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/`:

- `m3/` (all frameworks in one project, openai 2.54.0 because guardrails-ai caps `openai<3`): `t_instructor.py`, `t_agno.py`, `t_smol.py`, `t_smol2.py`, `t_haystack.py`, `t_bee.py`, `t_guard.py`, `t_guard2.py`, `t_deep.py`; shared `common.py`.
- `m3c/` (the capture project's exact pins plus the M3 packages, `openai==3.45.0`, `httpx2`): every script above re-run there, same results (except where noted).
- `m3t/` (`temporalio-openai-agents[openinference]`, read only), `m3ts/` (BeeAI TypeScript under Deno 2.5.3; Node is not installed).

Every framework ran against a throwaway in-process HTTP server that answers canned OpenAI chat-completions JSON by position (a tool-call turn, then a text
turn), under its OpenInference instrumentor with an `InMemorySpanExporter`. Model client: `base_url` pointing at the fake server (the same shape as the
`langgraph-agent` scenario's `mock_url("/v1")`). All turns were `POST /v1/chat/completions`, non-streaming, model `gpt-4o-mini-2024-07-18`, token counts 82/17
then 131/12.

## Findings that change the design

1. **Guardrails AI: the OpenInference instrumentor does nothing on any current version.** `openinference-instrumentation-guardrails` (0.1.33 usable, 0.1.36 latest) declares `_instruments = ("guardrails-ai>=0.4.5,<0.5.1",)` (guardrails-ai 0.5.1 is from 2024; current is 0.11.0, 2026-08-14) and prints "nothing can be instrumented". `skip_dep_check=True` changes nothing. But guardrails-ai 0.11.0 emits OpenInference-style spans **itself** (scope `guardrails-ai`, `openinference.span.kind=GUARDRAIL`) on the global tracer provider. So the page needs no instrumentor, and **D-IN-7's `guardrails` registry row should not exist** (a `instrument=["guardrails"]` name would be a no-op). It does need the `openai` instrumentor for an `llm` span. The instrumentor also requires Python `<3.15`.
2. **BeeAI TypeScript: not supportable.** `@arizeai/openinference-instrumentation-beeai` 1.6.5 (usable; 1.6.8 latest) has `INSTRUMENTS = [">=0.1.9 <0.1.14"]`: beeai-framework 0.1.13 is from 2025-04-29, the usable version is 0.1.32 (2026-11-02). `manuallyInstrument()` warns `DependencyConflict` and returns. Forcing `patch()` on 0.1.32 patches `BaseAgent.run`, but the event middleware throws "Converting circular structure to JSON" (`getSerializedObjectSafe`, on `DynamicTool.emitter` and `FinalAnswerTool.state`), so no span is ever exported (0 spans, agent and both model calls otherwise worked). BeeAI TypeScript has no native OpenTelemetry (no `@opentelemetry` import in its `dist`). It is an upstream gap, not a cetra normalizer gap, so it does not go to M4: the user decides between a Python-only BeeAI page (recommended) and a TypeScript page that says it is unsupported. Also: beeai-framework 0.1.32 on npm does not declare its `uuid` dependency (Deno failed on `import 'uuid'` until it was added by hand).
3. **Temporal: the design's wording is out of date.** Since temporalio 1.36 the OpenAI Agents integration is the standalone package `temporalio-openai-agents` (`temporalio.openai_agents`; `temporalio.contrib.openai_agents` is a re-export shim). Its OpenTelemetry path is `OpenAIAgentsPlugin(use_otel_instrumentation=True)` plus a global `create_tracer_provider()` (a replay-safe `TracerProvider`); the plugin runs `OpenAIAgentsInstrumentor` itself. Temporal's own spans come from `OpenTelemetryPlugin` (new) or `TracingInterceptor` (legacy), both in `temporalio.contrib.opentelemetry`. See § Temporal for what that means for `instrument=`.
4. **Double counting when two instrumentors cover the same call.** Agno, smolagents and Haystack each emit their own `llm` span; enabling `openai` as well adds a nested `llm` span for the same call (verified for all three: 2 `LLM` spans per call, parent and child, both with tokens). Tokens and cost are normalized on every `llm` span (spec 02 rule 3) and trace totals sum all spans, so the call would count twice. The pages must say: enable the framework's instrumentor only. (Instructor and Guardrails are the opposite: they need `openai`, see their sections.)
5. **Capture project dependency conflicts** (`tools/capture/py/pyproject.toml` pins `openai==3.45.0`, `openai-agents==0.24.0`):
   - `instructor` 1.17.0 requires `jiter<0.15`; openai 3.45.0 requires `jiter>=0.16`. Resolution fails. It runs correctly with `[tool.uv] override-dependencies = ["jiter>=0.16"]` (verified: jiter 0.19.0, openai 3.45.0, instructor 1.17.0, the scenario output is identical).
   - `guardrails-ai` 0.11.0 requires `openai<3`. It runs correctly with `override-dependencies = ["openai==3.45.0"]` (litellm's OpenAI path works on 3.45.0).
   - `temporalio-openai-agents` 1.1.0 (usable) requires `openai-agents>=0.22,<0.23`, which conflicts with the pin 0.24.0; 2.0.0 (2026-11-17, after the cutoff) accepts `<0.25`.
   - With the two overrides and everything else in `m3c/` the lock resolves: agno 3.2.0, haystack-ai 3.5.0, instructor 1.17.0, smolagents 1.26.0, beeai-framework 0.1.85 (litellm pin), deepagents 0.7.32 (langchain 1.4.8 pin), temporalio 1.36.1, guardrails-ai 0.11.0.
6. **Runtime network calls to switch off in capture scenarios.** Agno (`AGNO_TELEMETRY=false`), Haystack (`HAYSTACK_TELEMETRY_ENABLED=False`) and Guardrails (hub telemetry sent to an AWS endpoint by an OTLP exporter of its own; switched off only by `enable_metrics=false` in `~/.guardrailsrc` read at import, setting `settings.rc.enable_metrics` afterwards is too late) phone home by default. `scenario.env` accepts only `OTEL_`, `OPENINFERENCE_`, `TRACELOOP_`, `LANGFUSE_`, `LMNR_`, `ANTHROPIC_`, `ADK_` keys, so these are set in the scenario's `main.py` before the imports (for Guardrails by pointing `HOME` at a temp dir holding the rc file). No capture-tool change needed.
7. **Guardrails leaks the API key into a span.** When the key is passed as a keyword (`guard(..., api_key="k")`), the `call` span's `llm.invocation_parameters` contains it in clear. The page must pass the key through `OPENAI_API_KEY`. (The `input.value` of `guard` and `step` also holds the kwargs.)

## Versions (the newest upload on or before 2026-11-16)

| Package | Usable (date) | Latest (date) | Python | Notes |
|---|---|---|---|---|
| instructor | 1.17.0 (2026-09-09) | same | `>=3.9` | `openai>=2,<4`, `jiter<0.15` |
| openinference-instrumentation-instructor | 0.1.35 (2026-11-12) | 0.1.38 (2026-11-21) | `>=3.10,<3.14` | |
| agno | 3.2.0 (2026-11-12) | 3.2.1 (2026-11-17) | `>=3.9` | |
| openinference-instrumentation-agno | 1.2.1 (2026-11-16) | 1.2.5 (2026-11-21) | `>=3.10,<3.16` | |
| smolagents | 1.26.0 (2026-05-29) | same | `>=3.10` | |
| openinference-instrumentation-smolagents | 0.1.50 (2026-11-12) | 0.1.53 (2026-11-21) | `>=3.10,<3.16` | |
| haystack-ai | 3.5.0 (2026-11-10) | same | `>=3.11` | needs `openai>=3.26` (3.4.1 in the openai 2.x project) |
| openinference-instrumentation-haystack | 0.1.53 (2026-11-12) | 0.1.56 (2026-11-21) | `>=3.10,<3.16` | |
| beeai-framework (PyPI) | 0.1.86 (2026-11-02) | same | `>=3.11,<3.14` | pins `litellm==1.100.4` |
| openinference-instrumentation-beeai | 0.1.36 (2026-11-12) | 0.1.39 (2026-11-21) | `>=3.11,<3.15` | needs `beeai-framework>=0.1.51` |
| beeai-framework (npm) | 0.1.32 (2026-11-02) | same | | |
| @arizeai/openinference-instrumentation-beeai (npm) | 1.6.5 (2026-11-12) | 1.6.8 (2026-11-21) | | supports beeai-framework `>=0.1.9 <0.1.14` only |
| guardrails-ai | 0.11.0 (2026-08-14) | same | `>=3.10,<3.14` | `openai>=2,<3`; pulls litellm |
| openinference-instrumentation-guardrails | 0.1.33 (2026-11-12) | 0.1.36 (2026-11-21) | `>=3.10,<3.15` | supports guardrails-ai `>=0.4.5,<0.5.1`: unusable |
| deepagents | 0.7.39 (2026-11-16) | 0.7.41 (2026-11-20) | `>=3.11` | |
| openinference-instrumentation-langchain | 0.1.87 (2026-11-16) | 0.1.90 (2026-11-21) | | already in the capture project (0.1.81) |
| temporalio | 1.36.1 (2026-11-14) | same | `>=3.11` | |
| temporalio-openai-agents | 1.1.0 (2026-10-01) | 2.0.0 (2026-11-17) | | extra `openinference` |
| openinference-instrumentation-openai-agents | 2.8.4 (2026-11-16) | 2.8.7 (2026-11-21) | | |
| openinference-instrumentation-openai | 0.1.74 (2026-11-16) | 0.1.77 (2026-11-21) | | |

Python 3.15 (D-IN-7): the import test must skip `instructor` (`<3.14`) and `beeai` (`<3.15`) on 3.15, with the reason. Agno, smolagents and Haystack allow 3.15.

## Instructor

- Install: `instructor`, `openinference-instrumentation-instructor`, `openinference-instrumentation-openai`. Registry name `instructor` (class `InstructorInstrumentor`, module `openinference.instrumentation.instructor`); the page also needs `instrument=["instructor", "openai"]`.
- Client injection: `instructor.from_openai(openai.OpenAI(base_url=..., api_key=...))`. Default mode is tools.
- HTTP: one `POST /v1/chat/completions`, model as given, `tools=[User]`, `tool_choice={"type":"function","function":{"name":"User"}}`, not streaming. The response is a tool call with the `User` arguments.
- Spans (2): `instructor.create` [CHAIN] (scope `openinference.instrumentation.instructor`; `llm.model_name`, `llm.provider=openai`, `llm.system=openai`, `llm.invocation_parameters`, `instructor.response_model`; `input.value`, `output.value`, `llm.input_messages.0`; **no token counts**) with child `ChatCompletion` [LLM] (scope `...openai`; `llm.model_name`, `llm.system=openai`, `llm.token_count.prompt/completion/total` 30/9/39, `server.address`, `llm.finish_reason`, input and output messages, `llm.tools.0`).
- cetra: `chain` + `llm`. The `llm` span gets tokens 30/9, provider `openai` (`llm.system`), cost (`gpt-4o-mini-2024-07-18` / `openai` is in `builtin.json`, 0.15/0.6 per 1M). Without the `openai` instrumentor there is no `llm` span and no tokens or cost, so `openai` is part of the setup. No gap.
- Capture: **with a listed change.** One turn: `openai.chat`, `model gpt-4o-mini-2024-07-18`, `message.tool_calls = [{id: call_user_1, name: "User", arguments: {name: "Ada", age: 36}}]`, `finish: tool_calls`, usage 30/9. Change: `override-dependencies = ["jiter>=0.16"]` in the capture `pyproject.toml`. Group `openinference`, case `instructor-structured-output`; libraries `instructor`, `openai`, both instrumentors, `opentelemetry-sdk`.
- Verdict: capture with a listed change (jiter override).

## Agno

- Install: `agno`, `openinference-instrumentation-agno` (needs `openai` for the model). Registry name `agno` (`AgnoInstrumentor`). **Do not also enable `openai`** (finding 4).
- Client injection: `OpenAIChat(id=..., base_url=..., api_key=..., max_retries=0)` (chat completions; `OpenAIResponses` is the other class).
- HTTP: two `POST /v1/chat/completions`, `tools=[get_weather]`, no `tool_choice`, not streaming; message counts 2 then 4.
- Spans (4): `weather_agent.run` [AGENT] (`agent.name`, `graph.node.id`, `session.id`, `agno.agent*`, `agno.run.id`; input and output values; no tokens), then `OpenAIChat.invoke` [LLM] (`llm.model_name`, `llm.provider=OpenAI`, `llm.system=openai`, `llm.token_count.prompt/completion`, no total; input and output messages and `llm.tools.0`), `get_weather` [TOOL] (`tool.name`, `tool.description`, `tool.parameters`, input and output), `OpenAIChat.invoke` [LLM].
- cetra: `agent`, `llm`, `tool`, `llm`. Provider `OpenAI` is lowercased to `openai` (rule 12); `total_tokens` from rule 4; priced. `agent.name`, `graph.node.id` and `agno.*` stay in `attributes` (small ones kept). No gap.
- Capture: **as is.** Turns: (1) `openai.chat` tool call `get_weather {city: Paris}` id `call_weather_1`, finish `tool_calls`, usage 82/17; (2) `openai.chat` text "It is sunny in Paris.", finish `stop`, usage 131/12. In `main.py`: `AGNO_TELEMETRY=false` before the import, and pin the random ids (`session.id`, `graph.node.id`, `agno.run.id`, `agno.agent.id`) with `volatile_attributes` or the uuid patch the `langgraph-agent` scenario uses.
- Verdict: capture as is.

## smolagents

- Install: `smolagents` (extra `openai` for `OpenAIServerModel`), `openinference-instrumentation-smolagents`. Registry name `smolagents` (`SmolagentsInstrumentor`). **Do not also enable `openai`** (finding 4, verified: the `openai` span nests under `OpenAIModel.generate`).
- Client injection: `OpenAIServerModel(model_id=..., api_base=..., api_key=..., client_kwargs={"max_retries": 0})`.
- `CodeAgent` (the common one): two `POST /v1/chat/completions`, `tools=[]` (tools are called from generated code), no `tool_choice`. The turns are plain text with a `<code>` block (`get_weather(city="Paris")`, then `final_answer("Sunny in Paris")`). `ToolCallingAgent` (verified too): `tools=[get_weather, final_answer]`, `tool_choice=required`, two tool-call turns (the second calls `final_answer`).
- Spans (7): `weather_agent.run` [AGENT] (`llm.token_count.prompt/completion/total` 213/29/242 aggregated over the run, `smolagents.max_steps`, `smolagents.tools_names`, input and output), `Step 1` [CHAIN] > `OpenAIModel.generate` [LLM] (`llm.model_name`, `llm.provider=openai`, `llm.system=openai`, tokens 82/17/99, input and output messages, `llm.tools.0` for ToolCallingAgent) and `SimpleTool` [TOOL] (`tool.name=get_weather`); `Step 2` [CHAIN] > `OpenAIModel.generate` [LLM] and `FinalAnswerTool` [TOOL].
- cetra: `agent`, `chain`, `llm`, `tool`. The agent span's aggregated tokens stay unread (tokens only on `llm`/`embedding`, rule 3), so no double count. Priced. `tool.name` is not a column (the span name `SimpleTool` shows; the tool name is in `attributes`). No gap.
- Capture: **as is.** `CodeAgent`, turns: (1) `openai.chat` text `Thought: I will look up the weather.\n<code>\nw = get_weather(city="Paris")\nprint(w)\n</code>`, finish `stop`, usage 82/17; (2) text `Thought: I have it.\n<code>\nfinal_answer("Sunny in Paris")\n</code>`, usage 131/12. The local Python executor needs no sandbox. Check that the scenario output does not contain step timings that vary (`timing_attributes`) when it is written.
- Verdict: capture as is.

## Haystack

- Install: `haystack-ai`, `openinference-instrumentation-haystack`. Registry name `haystack` (`HaystackInstrumentor`). **Do not also enable `openai`** (finding 4).
- Client injection: `OpenAIChatGenerator(model=..., api_base_url=..., api_key=Secret.from_token("k"), max_retries=0)`.
- HTTP (pipeline `ChatPromptBuilder` > `OpenAIChatGenerator`): one `POST /v1/chat/completions`, `tools=[]`, `stream=false`.
- Spans (3): `Pipeline.run` [CHAIN], `ChatPromptBuilder.run` [CHAIN], `OpenAIChatGenerator.run` [LLM] (`llm.model_name`, `llm.provider=openai`, `llm.system=openai`, tokens 25/9/34, input and output messages; input and output values).
- **Gap (instrumentor, not cetra):** Haystack's `Agent` component (`haystack.components.agents.Agent`, with a tool) is not wrapped: the run produced three disconnected root spans (`ChatPromptBuilder.run`, `OpenAIChatGenerator.run` twice, three traces), with no agent or tool span and no `llm.tools`. Pipelines are fine. The page covers pipelines and says that agents are not traced as agents.
- cetra: `chain`, `chain`, `llm`; tokens, provider `openai`, priced. No normalizer gap.
- Capture: **as is.** One turn: `openai.chat` text "Paris is the capital of France.", finish `stop`, usage 25/9. `HAYSTACK_TELEMETRY_ENABLED=False` set in `main.py`. Haystack gives pipelines run ids? None seen in the attributes.
- Verdict: capture as is (pipeline only).

## BeeAI (Python)

- Install: `beeai-framework`, `openinference-instrumentation-beeai`. Registry name `beeai` (`BeeAIInstrumentor`, module `openinference.instrumentation.beeai`). The model calls are covered by BeeAI's own spans, so no `openai`.
- Client injection: `OpenAIChatModel("gpt-4o-mini-2024-07-18", api_key=..., base_url=...)` (it goes through litellm to the OpenAI client; `OPENAI_BASE_URL` also works).
- `RequirementAgent` (the current agent). HTTP: two `POST /v1/chat/completions` with **`response_format=json_schema`** (schema `ToolCall`/`AvailableTools`), `tools=[]`, `tool_choice` not sent. The agent reads the tool choice from the structured reply, so the real replies are JSON **text**: `{"item": {"name": "get_weather", "parameters": {"city": "Paris"}}}`, then `{"item": {"name": "final_answer", "parameters": {"response": "Sunny in Paris"}}}`. A real `tool_calls` reply works too (verified); text JSON is the faithful one.
- Spans (6): `weather-agent` [AGENT] (`agent.name`, `metadata.*` including every step and tool schema, input and output), `OpenAIChatModel` [LLM] (`llm.model_name`, `llm.provider=openai`, tokens 82/17/99, `llm.invocation_parameters`, input and output messages, `llm.tools.0`, **`llm.cost.prompt/completion/total`** from litellm: 1.23e-05 + 1.02e-05 = 2.25e-05), `get_weather` [TOOL], `OpenAIChatModel` [LLM], `final_answer` [AGENT] (the tool's own run, `metadata.path=agent.requirement.final_answer`, input only), `final_answer` [TOOL].
- cetra: `agent`, `llm`, `tool`, `llm`, `agent`, `tool`. Tokens read; **cost is reported** (`llm.cost.*`, D109, `cost_source=reported`), so the price table is not used (the model is priced anyway). A model litellm cannot price would report 0, not null: check in the capture that a priced model is used. The `metadata.*` keys (about 40 per agent span) stay in `attributes` and are not small; the agent span's content columns come from `input.value`/`output.value`. No gap.
- Capture: **as is.** Turns: two `openai.chat` text turns with the JSON above, finish `stop`, usage 82/17 and 131/12. `volatile_attributes`: `metadata.steps.0.id`, `metadata.steps.1.id`, `metadata.id`, `metadata.trace.id`, `metadata.trace.run_id`. No telemetry found in BeeAI Python. litellm's `telemetry` flag is true in `litellm/__init__.py`; the run made no call (offline), recheck with the mock.
- Verdict: capture as is.

## BeeAI (TypeScript)

- Versions in the table. Client injection: `new OpenAIChatModel("gpt-4o-mini-2024-07-18", {}, {apiKey, baseURL})` (Vercel AI SDK, `.chat()`, so chat completions) with `RequirementAgent` (`ToolCallingAgent` is deprecated). HTTP: two `POST /v1/chat/completions`, `tools=[get_weather, final_answer]`, `tool_choice="required"`, not streaming, as under the Python framework. The agent ran correctly against the fake.
- Instrumentation: **0 spans** (finding 2). `manuallyInstrument()` refuses the version; a forced `patch()` throws in the event serializer. I did not try 0.1.13 end to end (it has no `RequirementAgent`; it is a 2025 release).
- Verdict: **page not possible as an OpenInference page** for the current framework. Not a normalizer gap and not M4. The user decides: Python-only BeeAI page (recommended), or a TypeScript section that says the instrumentor supports only `beeai-framework` `>=0.1.9 <0.1.14`. No capture, no TypeScript registry (the cetra TS SDK has no per-instrumentor registry anyway).

## Guardrails AI

- Install: `guardrails-ai`, `openinference-instrumentation-openai`; **no Guardrails instrumentor** (finding 1). No registry row; `cetra.init(instrument=["openai"])`. The global tracer provider is what Guardrails reads (cetra sets it), so call `cetra.init()` before the first `Guard`.
- Client injection: through litellm: `guard(model="gpt-4o-mini-2024-07-18", messages=[...], api_base=..., max_retries=0)` with `OPENAI_API_KEY` in the environment (finding 7). Passing the OpenAI client's `chat.completions.create` as the callable fails (it must return a string). The validator can be a local `@register_validator` class; hub validators need `guardrails hub install` (network), so the capture uses a custom one.
- HTTP: one `POST /v1/chat/completions`, `tools=[]`, no `response_format`, `temperature=0`.
- Spans (5, scope `guardrails-ai`, plus the openai one): `guard` [GUARDRAIL] (`guard.name`, `type=guardrails/guard`, `validation_passed`, `token_consumption`, `number_of_llm_calls`, `input.value`, `output.value`), `step` [GUARDRAIL], `call` [GUARDRAIL] (carries `llm.input_messages`, `llm.output_messages`, `llm.token_count.*` and `llm.invocation_parameters`, with the key if passed as a keyword), `no-banned-word.validate` [GUARDRAIL] (`validator.name`, `validator.on_fail`, `outcome`, `input.value`, plus constant `client.ip=unknown` and similar), and `ChatCompletion` [LLM] from the `openai` instrumentor (`llm.system=openai`, `server.address`, tokens 25/9/34). The `llm` span is a **sibling** of the guardrail spans in the trace, not under `call` (not asserted: check parentage in the capture).
- cetra: `guardrail` ×4, `llm` ×1. The `call` span's `llm.token_count.*` stays unread (kind `guardrail`, rule 3), so no double count. The `llm` span is priced. The validator span's plain `input` attribute is not read (plain `input`/`output` only for `voltagent` and `pipecat`, D-IN-6); `input.value` is. No normalizer gap.
- Capture: **with a listed change.** Capture project: add `guardrails-ai==0.11.0` and `override-dependencies = ["openai==3.45.0"]` (with the jiter override above). Scenario: `instrumentation: "guardrails-ai"` (the spans' producer), libraries `guardrails-ai`, `openinference-instrumentation-openai`, `opentelemetry-sdk`; `main.py` sets `HOME` to a temp dir with `.guardrailsrc` (`enable_metrics=false`) before importing guardrails; `OPENAI_API_KEY` from the runner. One turn: `openai.chat` text "Paris is the capital of France.", finish `stop`, usage 25/9. Volatile: `instance_id`, `validator.instance_id`, `validation_session_id`, `execution_id`, `callId` inside `output.value` (the last needs a different approach: it is inside a JSON string; the case may need `span_names` to drop `step`, or the `input.value`/`output.value` of `step`/`guard` not to be a fixture concern).
- Verdict: capture with a listed change; D-IN-7 and the design's Guardrails row change (instrumentor row removed).

## DeepAgents (Python)

- Install: `deepagents`, `openinference-instrumentation-langchain`, `langchain-openai`. Registry name `langchain` (existing row).
- Client injection: `ChatOpenAI(model=..., base_url=..., api_key=..., max_retries=0)` passed as `create_deep_agent(model=..., tools=[...])`.
- HTTP: two `POST /v1/chat/completions`, `stream=false`, `tools=[ls, read_file, write_file, edit_file, delete, glob, grep, task, get_weather]` (the built-in file and sub-agent tools plus the user's), no `tool_choice`.
- Spans (9): `LangGraph` [CHAIN] root (`metadata` has `ls_integration=deepagents` and `lc_versions.deepagents`), `PatchToolCallsMiddleware.before_agent` [AGENT], `model` [CHAIN] > `ChatOpenAI` [LLM] (`llm.model_name`, `llm.provider=openai`, `llm.system=openai`, tokens 82/17/99 and 131/12/143, `llm.finish_reason`, `llm.tools.0`), `tools` [CHAIN] > `get_weather` [TOOL], `model` [CHAIN] > `ChatOpenAI` [LLM], `_ParentNotifierMiddleware.after_agent` [AGENT]. Same shape as `openinference/langgraph-agent`.
- cetra: as the LangGraph fixture (`chain`, `llm` priced, `tool`). The two middleware spans are `agent` kind although they are plumbing (the page says so). Nothing in `ls_integration` identifies DeepAgents in a column; it is in the `metadata` attribute. No gap.
- Capture: **as is**, the `langgraph-agent` scenario with `create_deep_agent`. Turns: (1) `openai.chat` tool call `get_weather {city: Paris}`, finish `tool_calls`, usage 82/17; (2) text "It is 18 °C and sunny in Paris.", usage 131/12. The scenario needs the same id pinning (`_pin_framework_ids`) and probably more (deepagents middleware and thread ids): extend while writing it. Pin: deepagents 0.7.32 in the capture project (langchain 1.4.8).
- Verdict: capture as is.

## Temporal (with the OpenAI Agents SDK)

- Install: `temporalio-openai-agents[openinference]` (1.1.0; brings `openinference-instrumentation-openai-agents>=2` and `opentelemetry-sdk`), `temporalio`. Setup: `tracer_provider = create_tracer_provider()` (replay-safe), add the processor, `trace.set_tracer_provider(...)`; `Client.connect(..., plugins=[OpenAIAgentsPlugin(use_otel_instrumentation=True, ...)])`. Temporal's own spans: `OpenTelemetryPlugin()` (new; `add_temporal_spans=True` for `StartWorkflow`-style spans) or `TracingInterceptor()` (legacy; zero-duration workflow spans).
- Registry: **no `instrument=` name**; the plugin instruments OpenAI Agents itself, and `instrument=["openai-agents"]` on top would instrument twice. cetra's SDK uses an existing SDK `TracerProvider` as the global one (`Runtime.create`, D-F-4 branch 1, `isinstance(global_provider, TracerProvider)`; `ReplaySafeTracerProvider` is a `TracerProvider` subclass), so the page orders it: `create_tracer_provider()` and `set_tracer_provider` first, then `cetra.init()` (which adds its processor to that provider). Not run (needs a server).
- Model injection: `OpenAIAgentsPlugin(model_provider=...)` (a `ModelProvider` built on `AsyncOpenAI(base_url=...)`), or `OPENAI_BASE_URL`. Model calls run in the `invoke_model_activity` activity (default model class is the Responses API: `openai.responses` turns).
- **Server:** `WorkflowEnvironment.start_time_skipping()` and `start_local()` both **download a binary at runtime** (the test server and the Temporal CLI dev server, to the temp dir) unless `test_server_existing_path` / `dev_server_existing_path` is set. No binary is on this machine and I did not download one (it needs your yes: file, source, size). So: not runnable offline as installed; runnable offline only with a pinned, pre-fetched binary.
- Expected spans (from the docs and source, **not observed**): the OpenInference OpenAI Agents spans (`AGENT`, `LLM`, `TOOL`, …) as in `openai-agents-tool`, parented to the workflow, plus Temporal's spans (`StartWorkflow:…`, `RunWorkflow:…`, `StartActivity:…`, `RunActivity:…` from the interceptors; custom spans from `add_temporal_spans` in the OpenAI Agents trace), which carry no `openinference.span.kind` or `gen_ai.*` keys and stay `span` (D-IN-5's note stands: "Temporal's own spans stay `span`"). Whether OpenInference maps the Temporal custom spans to `chain` is unknown until captured.
- Capture: **needs something new**: fetch and pin the dev-server binary (a recipe with a checksum, like the other pinned tools, and an AGENTS.md § Required tools line), and a capture project that can hold `temporalio-openai-agents` (1.1.0 needs `openai-agents` 0.22.x; the project pins 0.24.0; 2.0.0, which accepts it, is past the 7-day cutoff until 2026-11-24). Turns would be `openai.responses` (default model class): one tool call, one text.
- Verdict: **page setup only** for M3, unless you approve the binary. Recommended: page setup only, with its limits stated, and a note in `docs/roadmap.md` that the capture waits for a pinned server binary.

## LangGraph (own page)

- Backed by `fixtures/otlp/openinference/langgraph-agent` (captured, `just check-capture` covers it): `LangGraph` [chain] root, `model` [chain], `ChatOpenAI` [llm] (`openai`, `gpt-4o-mini-2024-07-18`, 82/17, cost 0.00015), `get_weather` [tool], `tools` [chain], `model` [chain], `ChatOpenAI` [llm] (131/12, cost 0.000147).
- The page says: install `openinference-instrumentation-langchain` and use `cetra.init(instrument=["langchain"])` (the one `langchain` row covers LangChain, LangGraph and DeepAgents); `create_agent` (LangChain 1.x) and `langgraph.prebuilt.create_react_agent` produce the same graph spans; graph nodes show as `chain` spans named after the node, the model call as `llm` with tokens and cost, tools as `tool`; the model's provider comes from the model class, so a non-OpenAI `ChatOpenAI` base URL is attributed by the M1 rules; nothing LangGraph-specific is read (`langgraph_*` keys are in the `metadata` attribute). The LangChain page is retitled and links here (D-IN-1). No new capture.

## Claude Agent SDK (own page)

- Backed by `fixtures/otlp/claude_code/bash-tool` (Claude Code 2.1.323, beta trace export): `claude_code.interaction` [agent] > `claude_code.llm_request` [llm] (`claude-opus-5-5`, 420/12, 450/9), `Bash` [tool], plus `claude_code.tool.blocked_on_user` and `claude_code.tool.execution` [span]. D-CA-13: no code of its own; the SDK passes `TRACEPARENT` so the Claude Code turn nests under the application's span.
- The page says what `claude-code.mdx` § Claude Agent SDK already says (settings keys `CLAUDE_CODE_ENABLE_TELEMETRY=1`, `CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1`, `OTEL_TRACES_EXPORTER=otlp`, `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`; content flags `OTEL_LOG_USER_PROMPTS`, `OTEL_LOG_TOOL_DETAILS`, `OTEL_LOG_TOOL_CONTENT`), for Python and TypeScript, with the sample marked untested (needs the SDK and a model), and links the Claude Code page for the span shapes. No new capture: the fixture is the same process. Note that the page should state the application's own span (the parent) is whatever the application's SDK produces and is not a Claude Code span.

## Summary

| Framework | Versions (usable) | Registry name | Client injection | Turns (`api`) | Verdict | Gaps |
|---|---|---|---|---|---|---|
| Instructor | instructor 1.17.0; oi-instructor 0.1.35 (+ oi-openai 0.1.74) | `instructor` (+ `openai`) | `instructor.from_openai(OpenAI(base_url=))` | 1: `openai.chat` tool call `User{name,age}` | capture with a change: `jiter>=0.16` override | `instructor.create` is a chain with no tokens; needs the `openai` instrumentor; Python 3.15 import test skips (`<3.14`) |
| Agno | agno 3.2.0; oi-agno 1.2.1 | `agno` | `OpenAIChat(base_url=)` | 2: `openai.chat` tool call, text | capture as is | `AGNO_TELEMETRY=false`; random ids; no `openai` instrumentor (double count) |
| smolagents | smolagents 1.26.0; oi-smolagents 0.1.50 | `smolagents` | `OpenAIServerModel(api_base=)` | 2: `openai.chat` text with `<code>` blocks (CodeAgent) | capture as is | agent span carries aggregated tokens (unread, harmless); no `openai` instrumentor (double count) |
| Haystack | haystack-ai 3.5.0; oi-haystack 0.1.53 | `haystack` | `OpenAIChatGenerator(api_base_url=)` | 1: `openai.chat` text (pipeline) | capture as is (pipeline only) | `Agent` component not traced (disconnected spans); `HAYSTACK_TELEMETRY_ENABLED=False`; no `openai` instrumentor |
| BeeAI (Python) | beeai-framework 0.1.86; oi-beeai 0.1.36 | `beeai` | `OpenAIChatModel(base_url=)` | 2: `openai.chat` text JSON `{"item":{"name","parameters"}}` | capture as is | reported cost (`llm.cost.*`); many `metadata.*`; Python 3.15 skip (`<3.15`) |
| BeeAI (TypeScript) | beeai-framework 0.1.32; oi-beeai 1.6.5 | none | `OpenAIChatModel(…, {baseURL})` | n/a | **not supportable**: instrumentor supports `>=0.1.9 <0.1.14`; forced patch exports 0 spans | upstream; user decides: Python-only page |
| Guardrails AI | guardrails-ai 0.11.0; (oi-guardrails 0.1.33 unusable) + oi-openai | **none** (drop the row); `openai` | `guard(model=, api_base=)` via litellm, key in `OPENAI_API_KEY` | 1: `openai.chat` text | capture with a change: add guardrails-ai, `openai==3.45.0` override, rc file | instrumentor no-op on current versions; native `GUARDRAIL` spans; API key leaks in `llm.invocation_parameters` if passed as kwarg; hub telemetry on by default |
| DeepAgents | deepagents 0.7.39; oi-langchain 0.1.87 | `langchain` | `create_deep_agent(model=ChatOpenAI(base_url=))` | 2: `openai.chat` tool call, text | capture as is | middleware spans typed `agent`; id pinning like `langgraph-agent` |
| Temporal | temporalio 1.36.1; temporalio-openai-agents 1.1.0; oi-openai-agents 2.8.4 | none (the plugin instruments) | `OpenAIAgentsPlugin(model_provider=)` or `OPENAI_BASE_URL` | 2: `openai.responses` tool call, text | page setup only (capture needs a pinned server binary) | server binary downloaded at runtime; openai-agents pin conflict; global replay-safe provider before `cetra.init()` |
| LangGraph | existing capture | `langchain` | `ChatOpenAI(base_url=)` | existing (2 `openai.chat`) | page only; backed by `openinference/langgraph-agent` | none |
| Claude Agent SDK | existing fixture | none (D-CA-13) | n/a | existing | page only; backed by `claude_code/bash-tool` | none |

No normalizer gap was found, so nothing moves to M4. Scope changes for the user (D-IN-17): (1) Guardrails AI has no registry row (D-IN-7 and the design table change); (2) BeeAI TypeScript cannot be supported; (3) Temporal is page setup only unless a server binary is fetched; (4) the Temporal row's "tracing interceptor" wording becomes `OpenAIAgentsPlugin(use_otel_instrumentation=True)` plus `OpenTelemetryPlugin`.
