# M4 spike: Python native-GenAI frameworks (Pydantic AI, Semantic Kernel, Strands and AgentCore, Microsoft Agent Framework, AG2, AutoGen, Mirascope, OpenHands SDK)

Run 2026-11-23 (D-IN-17), worktree `integrations` at `a14bc7a`. Python 3.13, uv 0.13.6, `exclude-newer = "7 days"` (cutoff 2026-11-16T17:52Z), scripts in
`/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m4py/`:

- one uv project per framework (`pydanticai/`, `sk/`, `strands/`, `maf/`, `ag2/`, `autogen/`, `mirascope/`, `openhands/`), each pinning `openai==3.45.0` and `opentelemetry-sdk==1.45.2` like `tools/capture/py/pyproject.toml`; `t.py` is the scenario, `dump*.txt` the span dump, `otlp*.json` the exported OTLP/JSON, `otlp*.norm.jsonl` the normalized spans;
- `shared/common.py`: the throwaway fake OpenAI server (an in-process `http.server` answering canned chat-completions JSON by position: a tool call, then a text turn; 82/17 then 131/12 tokens, model `gpt-4o-mini-2024-07-18`), the `InMemorySpanExporter`, the span dumper, an OTLP/JSON writer, and a hook that records every non-loopback DNS lookup and blocks it ("phones home" check);
- `norm/`: a 15-line Rust binary (outside the repo, `cetra-otel` as a path dependency on this worktree, the worktree's `Cargo.lock`) that decodes the OTLP/JSON and calls the real `cetra_otel::normalize` with the built-in price table. **The "cetra today" columns below are that normalizer's output, not a hand reading**; the gaps were then traced to spec 02 rows and `crates/otel/src` by hand;
- `conflicts/`: copies of the capture project's `pyproject.toml` with the M4 packages added, to find lock conflicts; `conflicts/all2/` is the capture project plus every M4 package except OpenHands, and all seven non-OpenHands scenarios were re-run inside its venv (same span names and counts).

Every scenario: one agent, one tool (`get_weather`), two non-streaming `POST /v1/chat/completions` turns (tool call, then answer), native OTel on, content capture on, exported to an `InMemorySpanExporter` (OpenHands: its own Laminar exporter, received by an in-process OTLP/HTTP sink). The model client points at the fake server's `base_url`.

## Findings that change the design

1. **AG2's `record_usage` spans must NOT become `llm` (D-IN-5 conflict).** Every `chat` span is followed by a `record_usage model_call` span that repeats the same tokens (`ag2.usage.kind=model_call`, 82/17 and 131/12) and the response model. Mapping it to `llm` doubles tokens and cost on every call. AG2's `UsageEvent` docstring says one such record is emitted per LLM call and is "the single source of truth" for its usage report, so the duplication is by design. Keep it `span`. The kinds `compaction` and `aggregation` (LLM calls AG2 makes itself) may have no `chat` span; not captured, so no rule until a capture shows it.
2. **Framework names are written into `gen_ai.provider.name`, not only `gen_ai.system`.** Strands writes `strands-agents` into both keys on every span (so every Strands `chat` span has provider `strands-agents` and **cost_source `none`**, unpriced), and Microsoft Agent Framework writes `microsoft.agent_framework` into `gen_ai.provider.name` on `invoke_agent` spans (and into `gen_ai.system` when `OTEL_SEMCONV_STABILITY_OPT_IN` is set to anything without `gen_ai_latest_experimental`). Spec 02's framework-name table skips only `gen_ai.system` values. D-IN-15's table has to cover `gen_ai.provider.name` too, and gain `strands-agents` and `microsoft.agent_framework`.
3. **Two instrumentations double count when combined, and the design's AutoGen row says to combine them.** `openinference-instrumentation-autogen-agentchat` 0.1.29 emits its own `llm` span per `ChatCompletionClient.create` call, with tokens, model and messages. Adding `openinference-instrumentation-openai` adds a nested `ChatCompletion` `llm` span for the same call: two `llm` spans, both with tokens and both priced. The page must say: enable the AutoGen instrumentor only (the same finding as M3's Agno, smolagents and Haystack). The design row "with the provider's instrumentor" is wrong for the OpenAI client; this is a user decision (§ Open questions).
4. **OpenHands (Laminar) double counts cost.** Laminar writes a wrapper span `llm.<model>` with `lmnr.span.type=LLM`, no model, no tokens, but `gen_ai.usage.cost` (reported cost). Its child `litellm.completion` has the tokens and the model. Both are `llm`, so the reported cost (2.25e-05) and the computed cost (2.25e-05) are both summed. D-IN-5's "capture only" for OpenHands is not enough.
5. **Semantic Kernel sends message content only as OTel log records.** Prompts and completions are `gen_ai.system.message`, `gen_ai.user.message`, `gen_ai.assistant.message`, `gen_ai.tool.message` and `gen_ai.choice` log records (body JSON, `event.name` attribute), not span events or attributes. cetra drops logs (D-G-40: counted only). With the sensitive switch on, the spans still carry tool arguments and results and the `invoke_agent` span's output, but no `chat` span has input or output. No normalizer rule can fix this; it is a limit of the page, or a feature (§ Open questions).
6. **Strands' default (legacy event) content does not normalize, and spec 02's "Strands is covered" is wrong.** The legacy events carry Bedrock-Converse blocks (`[{"text": ...}]`, `[{"toolUse": ...}]`, `[{"toolResult": ...}]`), not OpenAI messages. cetra stores every part as an unknown part (`parts: []`, `message` as an extra field), so every preview is empty. With `OTEL_SEMCONV_STABILITY_OPT_IN=gen_ai_latest_experimental` the content is on three `gen_ai.client.inference.operation.details` events per `chat` span, one key each, and the reader looks only at the first such event: `chat` spans end with no input and no output. With `gen_ai_latest_experimental,gen_ai_span_attributes_only` content is on span attributes and works today. The page can ship on that setting with no normalizer change; rule R5 fixes the event case.
7. **Strands' `gen_ai.server.time_to_first_token` is an integer in milliseconds** (134 for a 134.8 ms span; the source copies `timeToFirstByteMs`), not seconds. D-IN-5 says seconds.
8. **Microsoft Agent Framework's workflow spans carry no `gen_ai.operation.name`.** `workflow.build`, `executor.process <id>` and `edge_group.process <type>` have only `workflow.id`, `executor.id`, `edge_group.id`, `message.*` attributes. D-IN-5's "operation names mapped to kinds" cannot apply; the rule must key on those attributes (or the span name).
9. **Mirascope's models are `<provider>/<model>`, both request and response**, so cost is `none`: `openai/gpt-4o-mini` prices nothing, and Mirascope does not report the API's dated model name. D-IN-20 (strip a `vendor/` prefix and retry) does not cover it because both models carry the prefix.
10. **Pydantic AI writes `{"type":"tool_call_response","id","name","result"}` parts**, not `response`; cetra's canonical part requires `response`, so these parts become unknown parts.
11. **Capture-project conflicts** (full list in § Capture project): Mirascope 2.8.2 caps `anthropic<1`, `google-genai<2`, `opentelemetry-instrumentation-google-genai<1`; `openhands-sdk>=1.62.0` needs `lmnr<0.8` (capture has 0.8.8) and `mcp>=2` (Semantic Kernel 1.47 needs `mcp<2`); the `agent-framework` meta package needs `opentelemetry-util-genai==0.3b0` (capture pins 1.1b0). Without an explicit pin, uv silently backtracks to Mirascope 0.1.3, openhands-sdk 1.28.0, agent-framework 1.0.0b251120.
12. **Telemetry phoning home**: only OpenHands, through LiteLLM (see § OpenHands). The other seven made no non-loopback connection attempt.

## Versions (the newest upload on or before the 2026-11-16 cutoff, unless noted)

| Package | Used (upload date) | Latest (after cutoff) | Notes |
|---|---|---|---|
| pydantic-ai-slim | 2.68.0 (2026-11-12), extra `openai` | 2.71.0 | `logfire` not installed or needed |
| semantic-kernel | 1.47.0 (2026-11-05) | same | `mcp>=1.26,<2` |
| strands-agents | 1.65.0 (2026-11-13), extra `openai` | 1.66.0 | |
| agent-framework-core / agent-framework-openai | 1.26.0 (2026-11-12) / 1.18.2 (2026-11-12) | same | the `agent-framework` meta package pulls 211 packages and conflicts (see Capture project) |
| ag2 | 1.3.2 (2026-11-15), extras `openai`, `tracing` | 1.3.4 | |
| autogen-agentchat, autogen-core, autogen-ext | 0.7.5 (2025-09-30), extra `openai` | same | no 0.7.x after September 2025 |
| openinference-instrumentation-autogen-agentchat | 0.1.29 (2026-11-12) | 0.1.32 | |
| openinference-instrumentation-openai | 0.1.69 (capture pin; newest at cutoff is 0.1.73) | 0.1.77 | only used to show finding 3 |
| mirascope | 2.8.2 (2026-10-22), extras `openai`, `ops` | same | `ops` needs the `anthropic` and `google-genai` packages installed (its import check loads them) |
| openhands-sdk | 1.62.0 (2026-11-11) | 1.63.1 | pulls LiteLLM 1.105.7 and `lmnr` |
| lmnr | 0.8.8 (capture pin, 2026-11-03; openhands-sdk wants `<0.8`) | 0.9.2 | |

`OTEL_SEMCONV_STABILITY_OPT_IN=gen_ai_latest_experimental` is honoured by Strands (and is Microsoft Agent Framework's default when the variable is unset); Pydantic AI, Semantic Kernel, AG2, Mirascope and Laminar ignore it.

---

## Pydantic AI 2.68.0

Setup (no `logfire`; a plain provider):

```python
from pydantic_ai import Agent
from pydantic_ai.agent import InstrumentationSettings
Agent.instrument_all(InstrumentationSettings(tracer_provider=provider, include_content=True))   # version=5 by default
agent = Agent(OpenAIChatModel("gpt-4o-mini", provider=OpenAIProvider(base_url=..., api_key=...)), name="weather_agent")
```

Span dump (non-streaming run; the streaming run adds `gen_ai.client.operation.time_to_first_chunk` [double, seconds] to each `chat` span and nothing else):

| # | span name | scope · kind · parent · status | attributes: `key [type] = value` | events |
|---|---|---|---|---|
| 0 | `invoke_agent weather_agent` | pydantic-ai@2.68.0 · INTERNAL · root · UNSET | agent_name [str] = weather_agent<br>final_result [str] = It is 18C and sunny in Paris.<br>gen_ai.agent.call.id [str] = 01a20995-f1d1-708a-aa68-ca40cc8e49b2<br>gen_ai.agent.name [str] = weather_agent<br>gen_ai.aggregated_usage.input_tokens [int] = 213<br>gen_ai.aggregated_usage.output_tokens [int] = 29<br>gen_ai.conversation.id [str] = 01a20995-f1d1-708a-aa68-ca41e60f8e1c<br>gen_ai.operation.name [str] = invoke_agent<br>logfire.json_schema [str] = {"type":"object","properties":{"pydantic_ai.all_messages":{"type":…<br>logfire.msg [str] = weather_agent run<br>model_name [str] = gpt-4o-mini<br>pydantic_ai.all_messages [str] = [{"role":"system","parts":[{"type":"text","content":"You are … | — |
| 1 | `chat gpt-4o-mini` | pydantic-ai@2.68.0 · CLIENT · #0 · UNSET | gen_ai.agent.call.id [str] = 01a20995-f1d1-708a-aa68-ca40cc8e49b2<br>gen_ai.agent.name [str] = weather_agent<br>gen_ai.conversation.id [str] = 01a20995-f1d1-708a-aa68-ca41e60f8e1c<br>gen_ai.input.messages [str] = [{"role":"system","parts":[{"type":"text","content":"You are a w…<br>gen_ai.operation.name [str] = chat<br>gen_ai.output.messages [str] = [{"role":"assistant","parts":[{"type":"tool_call","id":"call_1"…<br>gen_ai.provider.name [str] = openai<br>gen_ai.request.model [str] = gpt-4o-mini<br>gen_ai.response.finish_reasons [array] = ["tool_call"]<br>gen_ai.response.id [str] = chatcmpl-1<br>gen_ai.response.model [str] = gpt-4o-mini-2024-07-18<br>gen_ai.system [str] = openai<br>gen_ai.tool.definitions [str] = [{"type":"function","name":"get_weather","description":"Get th…<br>gen_ai.usage.input_tokens [int] = 82<br>gen_ai.usage.output_tokens [int] = 17<br>logfire.json_schema [str] = {"type":"object","properties":{"gen_ai.input.messages":{"type":"ar…<br>model_request_parameters [str] = {"function_tools":[{"name":"get_weather","parameters_json_sch…<br>operation.cost [double] = 2.25e-05<br>server.address [str] = 127.0.0.1<br>server.port [int] = 42203 | — |
| 2 | `execute_tool get_weather` | pydantic-ai@2.68.0 · INTERNAL · #0 · UNSET | gen_ai.agent.call.id [str] = 01a20995-f1d1-708a-aa68-ca40cc8e49b2<br>gen_ai.agent.name [str] = weather_agent<br>gen_ai.conversation.id [str] = 01a20995-f1d1-708a-aa68-ca41e60f8e1c<br>gen_ai.operation.name [str] = execute_tool<br>gen_ai.tool.call.arguments [str] = {"city":"Paris"}<br>gen_ai.tool.call.id [str] = call_1<br>gen_ai.tool.call.result [str] = 18C sunny in Paris<br>gen_ai.tool.name [str] = get_weather<br>logfire.json_schema [str] = {"type":"object","properties":{"gen_ai.tool.call.arguments":{"type…<br>logfire.msg [str] = running tool: get_weather | — |
| 3 | `chat gpt-4o-mini` | pydantic-ai@2.68.0 · CLIENT · #0 · UNSET | gen_ai.agent.call.id [str] = 01a20995-f1d1-708a-aa68-ca40cc8e49b2<br>gen_ai.agent.name [str] = weather_agent<br>gen_ai.conversation.id [str] = 01a20995-f1d1-708a-aa68-ca41e60f8e1c<br>gen_ai.input.messages [str] = [{"role":"system","parts":[{"type":"text","content":"You are a w…<br>gen_ai.operation.name [str] = chat<br>gen_ai.output.messages [str] = [{"role":"assistant","parts":[{"type":"text","content":"It is 1…<br>gen_ai.provider.name [str] = openai<br>gen_ai.request.model [str] = gpt-4o-mini<br>gen_ai.response.finish_reasons [array] = ["stop"]<br>gen_ai.response.id [str] = chatcmpl-2<br>gen_ai.response.model [str] = gpt-4o-mini-2024-07-18<br>gen_ai.system [str] = openai<br>gen_ai.tool.definitions [str] = [{"type":"function","name":"get_weather","description":"Get th…<br>gen_ai.usage.input_tokens [int] = 131<br>gen_ai.usage.output_tokens [int] = 12<br>logfire.json_schema [str] = {"type":"object","properties":{"gen_ai.input.messages":{"type":"ar…<br>model_request_parameters [str] = {"function_tools":[{"name":"get_weather","parameters_json_sch…<br>operation.cost [double] = 2.685e-05<br>server.address [str] = 127.0.0.1<br>server.port [int] = 42203 | — |

cetra today (real normalizer):

| span name | kind | provider | request / response model | in / out / total tokens | cost (source) | TTFT | session | input | output |
|---|---|---|---|---|---|---|---|---|---|
| `invoke_agent weather_agent` | agent | — | — / — | — | — | — | yes | empty | empty |
| `chat gpt-4o-mini` | llm | openai | gpt-4o-mini / gpt-4o-mini-2024-07-18 | 82 / 17 / 99 | 2.25e-05 (computed) | — | yes | What is the weather in Paris? | present (no text) |
| `execute_tool get_weather` | tool | — | — / — | — | — | — | yes | {"city":"Paris"} | 18C sunny in Paris |
| `chat gpt-4o-mini` | llm | openai | gpt-4o-mini / gpt-4o-mini-2024-07-18 | 131 / 12 / 143 | 2.68e-05 (computed) | — | yes | present (no text) | It is 18C and sunny in Paris. |

Streaming run, same spans plus `gen_ai.client.operation.time_to_first_chunk` read as `ttft_ns` (9.1 ms and 4.1 ms): TTFT already works.

Gaps:

| # | Key | Example | Should map to | Extends |
|---|---|---|---|---|
| P1 | `pydantic_ai.all_messages` (on `invoke_agent`) | `[{"role":"system","parts":[…]},{"role":"user",…},{"role":"assistant","parts":[{"type":"tool_call",…}],"finish_reason":"tool_call"},{"role":"user","parts":[{"type":"tool_call_response",…}]},{"role":"assistant",…}]` | `input` (canonical messages, format `messages`) | § Input/output, GenAI row (R1) |
| P2 | `final_result` (on `invoke_agent`) | `It is 18C and sunny in Paris.` (a JSON string for structured output) | `output` (non-message value, rule 10) | same |
| P3 | `parts[].result` of `tool_call_response` | `{"type":"tool_call_response","id":"call_1","name":"get_weather","result":"18C sunny in Paris"}` | `response` | message conversion, GenAI row (R6) |
| P4 | `operation.cost` (on `chat`) | `2.25e-05` | nothing: it equals the computed cost here; leaving it unread keeps one cost path | none (no change proposed) |

Nothing else: kinds (`invoke_agent`, `chat`, `execute_tool`), provider (`openai`, from `gen_ai.provider.name`; `gen_ai.system` repeats it), models (request `gpt-4o-mini`, response `gpt-4o-mini-2024-07-18`), tokens, cost, session (`gen_ai.conversation.id`, a UUIDv7 per run) and TTFT all map. `gen_ai.aggregated_usage.*` on the agent span is ignored, which is right (rule 3). Extra attributes that stay in `attributes`: `gen_ai.agent.name`, `gen_ai.agent.call.id`, `model_request_parameters` (886 B JSON), `logfire.json_schema`, `logfire.msg`, `agent_name`, `model_name`, `server.address`, `server.port`.

Nondeterministic: `gen_ai.agent.call.id` and `gen_ai.conversation.id` (UUIDv7, attributes only); `server.port` (the scrub rewrites loopback). Volatile list for the scenario: `gen_ai.agent.call.id`, `gen_ai.conversation.id`. No timestamps in payloads. Phones home: no (no network attempt).

## Semantic Kernel 1.47.0

Setup (the global tracer provider; the two switches are read at import):

```python
os.environ["SEMANTICKERNEL_EXPERIMENTAL_GENAI_ENABLE_OTEL_DIAGNOSTICS"] = "true"
os.environ["SEMANTICKERNEL_EXPERIMENTAL_GENAI_ENABLE_OTEL_DIAGNOSTICS_SENSITIVE"] = "true"
trace.set_tracer_provider(provider)
svc = OpenAIChatCompletion(ai_model_id="gpt-4o-mini", async_client=AsyncOpenAI(base_url=..., api_key=...))
agent = ChatCompletionAgent(service=svc, name="WeatherAgent", instructions="…", plugins=[WeatherPlugin()])
await agent.get_response(messages="…")
```

Span dump:

| # | span name | scope · kind · parent · status | attributes: `key [type] = value` | events |
|---|---|---|---|---|
| 0 | `invoke_agent WeatherAgent` | semantic_kernel.utils.telemetry.agent_diagnostics.decorators · INTERNAL · root · UNSET | gen_ai.agent.id [str] = 294809be-be79-444b-bd31-12137657eab2<br>gen_ai.agent.name [str] = WeatherAgent<br>gen_ai.input.messages [str] = []<br>gen_ai.operation.name [str] = invoke_agent<br>gen_ai.output.messages [str] = [{"role": "assistant", "content": "It is 18C and sunny in Paris…<br>gen_ai.tool.definitions [str] = [{"type": "function", "function": {"name": "W-get_weather", "d… | — |
| 1 | `AutoFunctionInvocationLoop` | semantic_kernel.connectors.ai.chat_completion_client_base · INTERNAL · #0 · UNSET | sk.available_functions [str] = W-get_weather | — |
| 2 | `chat gpt-4o-mini` | semantic_kernel.utils.telemetry.model_diagnostics.decorators · INTERNAL · #1 · UNSET | gen_ai.operation.name [str] = chat<br>gen_ai.request.model [str] = gpt-4o-mini<br>gen_ai.response.finish_reason [str] = FinishReason.TOOL_CALLS<br>gen_ai.response.id [str] = chatcmpl-1<br>gen_ai.system [str] = openai<br>gen_ai.usage.input_tokens [int] = 82<br>gen_ai.usage.output_tokens [int] = 17<br>server.address [str] = http://127.0.0.1:34751/v1/ | — |
| 3 | `execute_tool W-get_weather` | semantic_kernel.functions.kernel_function · INTERNAL · #1 · UNSET | gen_ai.operation.name [str] = execute_tool<br>gen_ai.tool.call.arguments [str] = {"city": "Paris"}<br>gen_ai.tool.call.id [str] = call_1<br>gen_ai.tool.call.result [str] = 18C sunny in Paris<br>gen_ai.tool.description [str] = Get the weather<br>gen_ai.tool.name [str] = W-get_weather | — |
| 4 | `chat gpt-4o-mini` | semantic_kernel.utils.telemetry.model_diagnostics.decorators · INTERNAL · #1 · UNSET | gen_ai.operation.name [str] = chat<br>gen_ai.request.model [str] = gpt-4o-mini<br>gen_ai.response.finish_reason [str] = FinishReason.STOP<br>gen_ai.response.id [str] = chatcmpl-2<br>gen_ai.system [str] = openai<br>gen_ai.usage.input_tokens [int] = 131<br>gen_ai.usage.output_tokens [int] = 12<br>server.address [str] = http://127.0.0.1:34751/v1/ | — |

cetra today:

| span name | kind | provider | request / response model | in / out / total tokens | cost (source) | TTFT | session | input | output |
|---|---|---|---|---|---|---|---|---|---|
| `invoke_agent WeatherAgent` | agent | — | — / — | — | — | — | — | present (no text) | It is 18C and sunny in Paris. |
| `AutoFunctionInvocationLoop` | span | — | — / — | — | — | — | — | empty | empty |
| `chat gpt-4o-mini` | llm | openai | gpt-4o-mini / — | 82 / 17 / 99 | 2.25e-05 (computed) | — | — | empty | empty |
| `execute_tool W-get_weather` | tool | — | — / — | — | — | — | — | {"city":"Paris"} | 18C sunny in Paris |
| `chat gpt-4o-mini` | llm | openai | gpt-4o-mini / — | 131 / 12 / 143 | 2.68e-05 (computed) | — | — | empty | empty |

A second run through `OpenAITextCompletion` (`/v1/completions`) gives one span `text_completions gpt-3.5-turbo-instruct` with `gen_ai.operation.name = text_completions`; it is already `llm` (through the `gen_ai.request.model` fallback), with provider `openai`, 5/2 tokens, computed cost, `operation = text_completions`.

The content (OTel log records, with the sensitive switch on, from a `LoggerProvider` bridged to the `logging` module; one record each, attributes `event.name`, `gen_ai.system`, `CHAT_MESSAGE_INDEX`, body = JSON string):

| Record (`event.name`) | Body example |
|---|---|
| `gen_ai.system.message` | `{"role": "system", "content": "You are a weather bot.", "name": "WeatherAgent"}` |
| `gen_ai.user.message` | `{"role": "user", "content": "What is the weather in Paris?"}` |
| `gen_ai.assistant.message` | `{"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function", "function": {…}}]}` |
| `gen_ai.tool.message` | `{"role": "tool", "content": "18C sunny in Paris", "tool_call_id": "call_1"}` |
| `gen_ai.choice` | `{"message": {"role": "assistant", "content": "It is 18C and sunny in Paris."}, "finish_reason": "stop"}` |
| `gen_ai.prompt` (text completion) | the prompt text (the body is the string) |

Gaps:

| # | Key | Example | Should map to | Extends |
|---|---|---|---|---|
| S1 | log records above | see table | `input` / `output` of the `chat` span, matched by trace and span id | not a normalizer rule: logs are dropped (D-G-40). Open question 3 |
| S2 | `gen_ai.system` = `googleai` | SK's Google AI connector | provider `gcp.gemini` | rule 12 alias table. SK's other names (`openai` also for Azure OpenAI, `anthropic`, `bedrock`, `huggingface`, `mistralai`, `nvidia`, `ollama`, `vertexai`) already map or stay as sent (source-read, only `openai` captured) |
| S3 | `gen_ai.operation.name` = `text_completions` | text completion span | kind `llm` explicitly (works today through the model fallback); the operation column keeps `text_completions` | § Span kind, GenAI row (R2) |
| S4 | `gen_ai.response.finish_reason` (singular string, `FinishReason.TOOL_CALLS`) | | nothing: it names a Python enum, and content is not read anyway | none |
| S5 | `invoke_agent` span: `gen_ai.input.messages` = `[]` | | empty list is already treated as absent (the agent span shows an output preview and no input) | none |

Kinds map (`invoke_agent` → agent, `chat` → llm, `execute_tool` → tool); `AutoFunctionInvocationLoop` (no operation, no model) stays `span`, which is acceptable (a `chain` would be nicer, but nothing in the span says what it is except its name and `sk.available_functions`; no rule proposed). `server.address` is a full URL (`http://127.0.0.1:…/v1/`); spec 02's host rule already reduces URLs to hosts. The response model is not reported, so cost uses the request model.

Nondeterministic: `gen_ai.agent.id` (UUID4), `server.address` port. Phones home: no. SK's own telemetry setting is off by default.

## Strands Agents 1.65.0 (and Bedrock AgentCore)

Setup: `trace.set_tracer_provider(provider)` first, then `StrandsTelemetry(tracer_provider=provider)` does **not** make it global (with an explicit provider it only stores it), and Strands' tracer reads `trace.get_tracer_provider()` at construction. In production `StrandsTelemetry().setup_otlp_exporter()` does both and reads `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_EXPORTER_OTLP_HEADERS`.

```python
agent = Agent(model=OpenAIModel(client_args={"base_url": ..., "api_key": ...}, model_id="gpt-4o-mini"),
              tools=[get_weather], name="weather_agent", system_prompt="…", trace_attributes={"session.id": "s1"})
```

Three configurations were run. `session.id` appears only because the scenario passes it in `trace_attributes`; Strands has no session id of its own on spans.

**(a) Default (legacy events), `OTEL_SEMCONV_STABILITY_OPT_IN` unset:**

| # | span name | scope · kind · parent · status | attributes: `key [type] = value` | events |
|---|---|---|---|---|
| 0 | `invoke_agent weather_agent` | strands.telemetry.tracer · INTERNAL · root · OK | gen_ai.agent.name [str] = weather_agent<br>gen_ai.agent.tools [str] = ["get_weather"]<br>gen_ai.event.end_time [str] = 2026-11-23T18:06:00.292783+00:00<br>gen_ai.event.start_time [str] = 2026-11-23T18:06:00.069905+00:00<br>gen_ai.operation.name [str] = invoke_agent<br>gen_ai.request.model [str] = gpt-4o-mini<br>gen_ai.system [str] = strands-agents<br>gen_ai.usage.cache_creation.input_tokens [int] = 0<br>gen_ai.usage.cache_read.input_tokens [int] = 0<br>gen_ai.usage.cache_read_input_tokens [int] = 0<br>gen_ai.usage.cache_write_input_tokens [int] = 0<br>gen_ai.usage.completion_tokens [int] = 29<br>gen_ai.usage.input_tokens [int] = 213<br>gen_ai.usage.output_tokens [int] = 29<br>gen_ai.usage.prompt_tokens [int] = 213<br>gen_ai.usage.total_tokens [int] = 242<br>session.id [str] = s1 | gen_ai.system.message: content[str]=[{"text": "You are a weather bot."}]<br>gen_ai.user.message: content[str]=[{"text": "What is the weather in Paris?"}]<br>gen_ai.choice: finish_reason[str]=end_turn, message[str]=It is 18C and sunny in Paris.  |
| 1 | `execute_event_loop_cycle` | strands.telemetry.tracer · INTERNAL · #0 · OK | event_loop.cycle_id [str] = 1ba137d0-2d3c-47b9-9633-e42cdcc5faba<br>gen_ai.event.end_time [str] = 2026-11-23T18:06:00.287566+00:00<br>gen_ai.event.start_time [str] = 2026-11-23T18:06:00.070217+00:00<br>gen_ai.operation.name [str] = execute_event_loop_cycle<br>gen_ai.system [str] = strands-agents<br>session.id [str] = s1 | gen_ai.user.message: content[str]=[{"text": "What is the weather in Paris?"}]<br>gen_ai.choice: message[str]=[{"toolUse": {"toolUseId": "call_1", "name": "get_weather", "input": {"city"… |
| 2 | `chat` | strands.telemetry.tracer · INTERNAL · #1 · OK | gen_ai.event.end_time [str] = 2026-11-23T18:06:00.285860+00:00<br>gen_ai.event.start_time [str] = 2026-11-23T18:06:00.070428+00:00<br>gen_ai.operation.name [str] = chat<br>gen_ai.request.model [str] = gpt-4o-mini<br>gen_ai.server.time_to_first_token [int] = 214<br>gen_ai.system [str] = strands-agents<br>gen_ai.usage.completion_tokens [int] = 17<br>gen_ai.usage.input_tokens [int] = 82<br>gen_ai.usage.output_tokens [int] = 17<br>gen_ai.usage.prompt_tokens [int] = 82<br>gen_ai.usage.total_tokens [int] = 99<br>session.id [str] = s1 | gen_ai.system.message: content[str]=[{"text": "You are a weather bot."}]<br>gen_ai.user.message: content[str]=[{"text": "What is the weather in Paris?"}]<br>gen_ai.choice: finish_reason[str]=tool_use, message[str]=[{"toolUse": {"toolUseId": "call_1", "name": "g… |
| 3 | `execute_tool get_weather` | strands.telemetry.tracer · INTERNAL · #1 · OK | gen_ai.event.end_time [str] = 2026-11-23T18:06:00.287153+00:00<br>gen_ai.event.start_time [str] = 2026-11-23T18:06:00.286748+00:00<br>gen_ai.operation.name [str] = execute_tool<br>gen_ai.system [str] = strands-agents<br>gen_ai.tool.call.id [str] = call_1<br>gen_ai.tool.description [str] = Get the weather.<br>gen_ai.tool.json_schema [str] = {"properties": {"city": {"description": "Parameter city", "typ…<br>gen_ai.tool.name [str] = get_weather<br>gen_ai.tool.status [str] = success<br>session.id [str] = s1 | gen_ai.tool.message: content[str]={"city": "Paris"}, id[str]=call_1, role[str]=tool<br>gen_ai.choice: id[str]=call_1, message[str]=[{"text": "18C sunny in Paris"}] |
| 4 | `execute_event_loop_cycle` | strands.telemetry.tracer · INTERNAL · #0 · OK | event_loop.cycle_id [str] = 6a9a7bb8-4c58-4187-90ed-519144a3d995<br>event_loop.parent_cycle_id [str] = 1ba137d0-2d3c-47b9-9633-e42cdcc5faba<br>gen_ai.event.end_time [str] = 2026-11-23T18:06:00.292565+00:00<br>gen_ai.event.start_time [str] = 2026-11-23T18:06:00.287651+00:00<br>gen_ai.operation.name [str] = execute_event_loop_cycle<br>gen_ai.system [str] = strands-agents<br>session.id [str] = s1 | gen_ai.user.message: content[str]=[{"text": "What is the weather in Paris?"}]<br>gen_ai.assistant.message: content[str]=[{"toolUse": {"toolUseId": "call_1", "name": "get_weather", "inpu…<br>gen_ai.tool.message: content[str]=[{"toolResult": {"toolUseId": "call_1", "status": "success", "content"… |
| 5 | `chat` | strands.telemetry.tracer · INTERNAL · #4 · OK | gen_ai.event.end_time [str] = 2026-11-23T18:06:00.292448+00:00<br>gen_ai.event.start_time [str] = 2026-11-23T18:06:00.287849+00:00<br>gen_ai.operation.name [str] = chat<br>gen_ai.request.model [str] = gpt-4o-mini<br>gen_ai.server.time_to_first_token [int] = 4<br>gen_ai.system [str] = strands-agents<br>gen_ai.usage.completion_tokens [int] = 12<br>gen_ai.usage.input_tokens [int] = 131<br>gen_ai.usage.output_tokens [int] = 12<br>gen_ai.usage.prompt_tokens [int] = 131<br>gen_ai.usage.total_tokens [int] = 143<br>session.id [str] = s1 | gen_ai.system.message: content[str]=[{"text": "You are a weather bot."}]<br>gen_ai.user.message: content[str]=[{"text": "What is the weather in Paris?"}]<br>gen_ai.assistant.message: content[str]=[{"toolUse": {"toolUseId": "call_1", "name": "get_weather", "inpu…<br>gen_ai.tool.message: content[str]=[{"toolResult": {"toolUseId": "call_1", "status": "success", "content"…<br>gen_ai.choice: finish_reason[str]=end_turn, message[str]=[{"text": "It is 18C and sunny in Paris."}] |

cetra today:

| span name | kind | provider | request / response model | in / out / total tokens | cost (source) | TTFT | session | input | output |
|---|---|---|---|---|---|---|---|---|---|
| `invoke_agent weather_agent` | agent | strands-agents | gpt-4o-mini / — | — | — | — | yes | present (no text) | present (no text) |
| `execute_event_loop_cycle` | span | strands-agents | — / — | — | — | — | yes | present (no text) | present (no text) |
| `chat` | llm | strands-agents | gpt-4o-mini / — | 82 / 17 / 99 | — (none) | — | yes | present (no text) | present (no text) |
| `execute_tool get_weather` | tool | strands-agents | — / — | — | — | — | yes | present (no text) | present (no text) |
| `execute_event_loop_cycle` | span | strands-agents | — / — | — | — | — | yes | present (no text) | empty |
| `chat` | llm | strands-agents | gpt-4o-mini / — | 131 / 12 / 143 | — (none) | — | yes | present (no text) | present (no text) |

**(b) `OTEL_SEMCONV_STABILITY_OPT_IN=gen_ai_latest_experimental`** (content in `gen_ai.client.inference.operation.details` events, three events per `chat` span, one key each; `gen_ai.provider.name` replaces `gen_ai.system`):

| # | span name | scope · kind · parent · status | attributes: `key [type] = value` | events |
|---|---|---|---|---|
| 0 | `invoke_agent weather_agent` | strands.telemetry.tracer · INTERNAL · root · OK | gen_ai.agent.name [str] = weather_agent<br>gen_ai.agent.tools [str] = ["get_weather"]<br>gen_ai.event.end_time [str] = 2026-11-23T17:56:18.255497+00:00<br>gen_ai.event.start_time [str] = 2026-11-23T17:56:18.102086+00:00<br>gen_ai.operation.name [str] = invoke_agent<br>gen_ai.provider.name [str] = strands-agents<br>gen_ai.request.model [str] = gpt-4o-mini<br>gen_ai.system_instructions [str] = [{"type": "text", "content": "You are a weather bot."}]<br>gen_ai.usage.cache_creation.input_tokens [int] = 0<br>gen_ai.usage.cache_read.input_tokens [int] = 0<br>gen_ai.usage.completion_tokens [int] = 29<br>gen_ai.usage.input_tokens [int] = 213<br>gen_ai.usage.output_tokens [int] = 29<br>gen_ai.usage.prompt_tokens [int] = 213<br>gen_ai.usage.total_tokens [int] = 242<br>session.id [str] = s1 | gen_ai.client.inference.operation.details: gen_ai.input.messages[str]=[{"role": "user", "parts": [{"type…<br>gen_ai.client.inference.operation.details: gen_ai.output.messages[str]=[{"role": "assistant", "parts": [… |
| 1 | `execute_event_loop_cycle` | strands.telemetry.tracer · INTERNAL · #0 · OK | event_loop.cycle_id [str] = 2a3be41c-a12c-4142-b39f-5a0f34fd500a<br>gen_ai.event.end_time [str] = 2026-11-23T17:56:18.250229+00:00<br>gen_ai.event.start_time [str] = 2026-11-23T17:56:18.102368+00:00<br>gen_ai.operation.name [str] = execute_event_loop_cycle<br>gen_ai.provider.name [str] = strands-agents<br>session.id [str] = s1 | gen_ai.client.inference.operation.details: gen_ai.input.messages[str]=[{"role": "user", "parts": [{"type…<br>gen_ai.client.inference.operation.details: gen_ai.input.messages[str]=[{"role": "user", "parts": [{"type… |
| 2 | `chat` | strands.telemetry.tracer · INTERNAL · #1 · OK | gen_ai.event.end_time [str] = 2026-11-23T17:56:18.248936+00:00<br>gen_ai.event.start_time [str] = 2026-11-23T17:56:18.102575+00:00<br>gen_ai.operation.name [str] = chat<br>gen_ai.provider.name [str] = strands-agents<br>gen_ai.request.model [str] = gpt-4o-mini<br>gen_ai.server.time_to_first_token [int] = 145<br>gen_ai.usage.completion_tokens [int] = 17<br>gen_ai.usage.input_tokens [int] = 82<br>gen_ai.usage.output_tokens [int] = 17<br>gen_ai.usage.prompt_tokens [int] = 82<br>gen_ai.usage.total_tokens [int] = 99<br>session.id [str] = s1 | gen_ai.client.inference.operation.details: gen_ai.system_instructions[str]=[{"type": "text", "content": …<br>gen_ai.client.inference.operation.details: gen_ai.input.messages[str]=[{"role": "user", "parts": [{"type…<br>gen_ai.client.inference.operation.details: gen_ai.output.messages[str]=[{"role": "assistant", "parts": [… |
| 3 | `execute_tool get_weather` | strands.telemetry.tracer · INTERNAL · #1 · OK | gen_ai.event.end_time [str] = 2026-11-23T17:56:18.249826+00:00<br>gen_ai.event.start_time [str] = 2026-11-23T17:56:18.249468+00:00<br>gen_ai.operation.name [str] = execute_tool<br>gen_ai.provider.name [str] = strands-agents<br>gen_ai.tool.call.arguments [str] = {"city": "Paris"}<br>gen_ai.tool.call.id [str] = call_1<br>gen_ai.tool.call.result [str] = [{"text": "18C sunny in Paris"}]<br>gen_ai.tool.description [str] = Get the weather.<br>gen_ai.tool.json_schema [str] = {"properties": {"city": {"description": "Parameter city", "typ…<br>gen_ai.tool.name [str] = get_weather<br>gen_ai.tool.status [str] = success<br>session.id [str] = s1 | gen_ai.client.inference.operation.details: gen_ai.input.messages[str]=[{"role": "tool", "parts": [{"type…<br>gen_ai.client.inference.operation.details: gen_ai.output.messages[str]=[{"role": "tool", "parts": [{"typ… |
| 4 | `execute_event_loop_cycle` | strands.telemetry.tracer · INTERNAL · #0 · OK | event_loop.cycle_id [str] = cfea0a80-a467-4163-88d1-12843fc256d6<br>event_loop.parent_cycle_id [str] = 2a3be41c-a12c-4142-b39f-5a0f34fd500a<br>gen_ai.event.end_time [str] = 2026-11-23T17:56:18.255246+00:00<br>gen_ai.event.start_time [str] = 2026-11-23T17:56:18.250315+00:00<br>gen_ai.operation.name [str] = execute_event_loop_cycle<br>gen_ai.provider.name [str] = strands-agents<br>session.id [str] = s1 | gen_ai.client.inference.operation.details: gen_ai.input.messages[str]=[{"role": "user", "parts": [{"type… |
| 5 | `chat` | strands.telemetry.tracer · INTERNAL · #4 · OK | gen_ai.event.end_time [str] = 2026-11-23T17:56:18.255121+00:00<br>gen_ai.event.start_time [str] = 2026-11-23T17:56:18.250503+00:00<br>gen_ai.operation.name [str] = chat<br>gen_ai.provider.name [str] = strands-agents<br>gen_ai.request.model [str] = gpt-4o-mini<br>gen_ai.server.time_to_first_token [int] = 4<br>gen_ai.usage.completion_tokens [int] = 12<br>gen_ai.usage.input_tokens [int] = 131<br>gen_ai.usage.output_tokens [int] = 12<br>gen_ai.usage.prompt_tokens [int] = 131<br>gen_ai.usage.total_tokens [int] = 143<br>session.id [str] = s1 | gen_ai.client.inference.operation.details: gen_ai.system_instructions[str]=[{"type": "text", "content": …<br>gen_ai.client.inference.operation.details: gen_ai.input.messages[str]=[{"role": "user", "parts": [{"type…<br>gen_ai.client.inference.operation.details: gen_ai.output.messages[str]=[{"role": "assistant", "parts": [… |

cetra today:

| span name | kind | provider | request / response model | in / out / total tokens | cost (source) | TTFT | session | input | output |
|---|---|---|---|---|---|---|---|---|---|
| `invoke_agent weather_agent` | agent | strands-agents | gpt-4o-mini / — | — | — | — | yes | What is the weather in Paris? | empty |
| `execute_event_loop_cycle` | span | strands-agents | — / — | — | — | — | yes | What is the weather in Paris? | empty |
| `chat` | llm | strands-agents | gpt-4o-mini / — | 82 / 17 / 99 | — (none) | — | yes | empty | empty |
| `execute_tool get_weather` | tool | strands-agents | — / — | — | — | — | yes | present (no text) | [{"text":"18C sunny in Paris"}] |
| `execute_event_loop_cycle` | span | strands-agents | — / — | — | — | — | yes | present (no text) | empty |
| `chat` | llm | strands-agents | gpt-4o-mini / — | 131 / 12 / 143 | — (none) | — | yes | empty | empty |

**(c) `OTEL_SEMCONV_STABILITY_OPT_IN=gen_ai_latest_experimental,gen_ai_span_attributes_only,gen_ai_tool_definitions`** (content as span attributes; `gen_ai.tool.definitions` is Strands' own shape `[{"name", "description", "inputSchema": {"json": …}}]`): the same spans as (b), with `gen_ai.input.messages`, `gen_ai.output.messages`, `gen_ai.system_instructions` on the span and no events.

| span name | kind | provider | request / response model | in / out / total tokens | cost (source) | TTFT | session | input | output |
|---|---|---|---|---|---|---|---|---|---|
| `invoke_agent weather_agent` | agent | strands-agents | gpt-4o-mini / — | — | — | — | yes | What is the weather in Paris? | It is 18C and sunny in Paris.  |
| `execute_event_loop_cycle` | span | strands-agents | — / — | — | — | — | yes | present (no text) | empty |
| `chat` | llm | strands-agents | gpt-4o-mini / — | 82 / 17 / 99 | — (none) | — | yes | What is the weather in Paris? | present (no text) |
| `execute_tool get_weather` | tool | strands-agents | — / — | — | — | — | yes | present (no text) | present (no text) |
| `execute_event_loop_cycle` | span | strands-agents | — / — | — | — | — | yes | present (no text) | empty |
| `chat` | llm | strands-agents | gpt-4o-mini / — | 131 / 12 / 143 | — (none) | — | yes | present (no text) | It is 18C and sunny in Paris. |

(Provider and cost are still wrong in (c): see T1. Inputs and outputs work; the empty cells are the tool-call-only first `chat` output and the tool span, which keep `output` and `input` in a non-preview shape.)

Gaps:

| # | Key | Example | Should map to | Extends |
|---|---|---|---|---|
| T1 | `gen_ai.system` and `gen_ai.provider.name` = `strands-agents` (every span) | | skipped (framework name); the provider then comes from the model-name inference (`gpt-4o-mini` → priced by the `openai` row) | rule 12 framework-name table, and the provider source row to cover both keys (R3) |
| T2 | `gen_ai.operation.name` = `execute_event_loop_cycle` (captured), `execute_structured_output`, `decision` (source-read: `agent/agent.py:1194`, `decisions/decision_model.py:62`) | | kind `chain` | § Span kind, GenAI row (R2) |
| T3 | `gen_ai.server.time_to_first_token` = `134` (int, **milliseconds**) on `chat` | | `ttft_ns` = value × 1e6, only under scope `strands.telemetry.tracer` | § Model, provider, usage, `ttft_ns` row (R4) |
| T4 | `gen_ai.client.inference.operation.details` events, one key per event | `gen_ai.input.messages`, then `gen_ai.output.messages`, on separate events | read each key from the first event that carries it | GenAI row, event fallback (R5) |
| T5 | legacy `gen_ai.user.message` etc. with `content` = `[{"text": …}]`, `[{"toolUse": {"toolUseId","name","input"}}]`, `[{"toolResult": {…}}]`; `gen_ai.choice` with `message`, `tool.result`, `finish_reason` | | nothing if the page requires setting (c); otherwise a Bedrock-Converse block converter (open question 4) | GenAI legacy events row |
| T6 | `gen_ai.usage.cache_read_input_tokens`, `cache_write_input_tokens`, `cache_read.input_tokens`, `cache_creation.input_tokens` (all `0` here), `prompt_tokens`, `completion_tokens`, `total_tokens` | | already mapped (first listed key wins; they agree) | none |
| T7 | `gen_ai.agent.tools` (`["get_weather"]`), `gen_ai.tool.status`, `gen_ai.tool.json_schema`, `gen_ai.event.start_time`, `gen_ai.event.end_time` (ISO strings), `event_loop.cycle_id`, `event_loop.parent_cycle_id` | | stay in `attributes` | none |

On `invoke_agent` the usage is cumulative (213/29); it stays unread, which is right (rule 3). `execute_event_loop_cycle` spans parent the `chat` and `execute_tool` spans: as `chain` (T2) they would show the loop.

Bedrock AgentCore: the code is Strands (or another framework) run by AgentCore's runtime; the model call is Bedrock Converse, which the capture mock does not serve, so the captured case is this Strands scenario and the page documents the AWS runtime setup only (D-IN-11). Strands on Bedrock writes the same `strands-agents` names. ADOT-based setup stays untested.

Nondeterministic: `event_loop.cycle_id` and `event_loop.parent_cycle_id` (UUID4), `gen_ai.event.start_time` / `gen_ai.event.end_time` (wall clock, ISO 8601 with microseconds: the capture tool's timestamp-attribute list shifts them), the `gen_ai.server.time_to_first_token` value (a duration: it differs per run, 134 and 3 ms here; a captured case would pin a value that `check-capture` recaptures differently, so `timing excluded` must cover it or the key goes in `volatile_attributes`, which handles strings only). Phones home: no. `ThreadingInstrumentor` is enabled by Strands (a side effect: it patches `threading` globally).

## Microsoft Agent Framework 1.26.0

Setup (the global provider; `configure_otel_providers()` does both and reads the OTLP environment; `enable_instrumentation` is for an existing provider):

```python
trace.set_tracer_provider(provider)
enable_instrumentation(enable_sensitive_data=True)          # content on
client = OpenAIChatCompletionClient(model="gpt-4o-mini", api_key=..., base_url=...)   # agent_framework.openai
agent = Agent(client=client, name="WeatherAgent", instructions="…", tools=[get_weather])
await agent.run("…")
```

Span dump, agent run:

| # | span name | scope · kind · parent · status | attributes: `key [type] = value` | events |
|---|---|---|---|---|
| 0 | `invoke_agent WeatherAgent` | agent_framework@1.26.0 · INTERNAL · root · UNSET | gen_ai.agent.id [str] = 12f208be-8981-47fe-91a5-09c488bb950e<br>gen_ai.agent.name [str] = WeatherAgent<br>gen_ai.input.messages [str] = [{"role": "user", "parts": [{"type": "text", "content": "What is…<br>gen_ai.operation.name [str] = invoke_agent<br>gen_ai.output.messages [str] = [{"role": "assistant", "parts": [{"type": "tool_call", "id": "c…<br>gen_ai.provider.name [str] = microsoft.agent_framework<br>gen_ai.request.choice.count [int] = 1<br>gen_ai.request.model [str] = gpt-4o-mini<br>gen_ai.response.finish_reasons [str] = ["stop"]<br>gen_ai.system_instructions [str] = [{"type": "text", "content": "You are a weather bot."}]<br>gen_ai.tool.definitions [str] = [{"type": "function", "name": "get_weather", "description": "G…<br>gen_ai.usage.input_tokens [int] = 213<br>gen_ai.usage.output_tokens [int] = 29 | — |
| 1 | `chat gpt-4o-mini` | agent_framework@1.26.0 · INTERNAL · #0 · UNSET | gen_ai.input.messages [str] = [{"role": "user", "parts": [{"type": "text", "content": "What is…<br>gen_ai.operation.name [str] = chat<br>gen_ai.output.messages [str] = [{"role": "assistant", "parts": [{"type": "tool_call", "id": "c…<br>gen_ai.provider.name [str] = openai<br>gen_ai.request.choice.count [int] = 1<br>gen_ai.request.model [str] = gpt-4o-mini<br>gen_ai.response.finish_reasons [str] = ["tool_calls"]<br>gen_ai.response.id [str] = chatcmpl-1<br>gen_ai.response.model [str] = gpt-4o-mini-2024-07-18<br>gen_ai.system_instructions [str] = [{"type": "text", "content": "You are a weather bot."}]<br>gen_ai.usage.input_tokens [int] = 82<br>gen_ai.usage.output_tokens [int] = 17<br>server.address [str] = http://127.0.0.1:41309/v1/ | — |
| 2 | `execute_tool get_weather` | agent_framework@1.26.0 · INTERNAL · #0 · UNSET | agent_framework.function.invocation.duration [double] = 0.00018033001106232405<br>gen_ai.operation.name [str] = execute_tool<br>gen_ai.tool.call.arguments [str] = {"city": "Paris"}<br>gen_ai.tool.call.id [str] = call_1<br>gen_ai.tool.call.result [str] = 18C sunny in Paris<br>gen_ai.tool.description [str] = Get the weather.<br>gen_ai.tool.name [str] = get_weather<br>gen_ai.tool.type [str] = function | — |
| 3 | `chat gpt-4o-mini` | agent_framework@1.26.0 · INTERNAL · #0 · UNSET | gen_ai.input.messages [str] = [{"role": "user", "parts": [{"type": "text", "content": "What is…<br>gen_ai.operation.name [str] = chat<br>gen_ai.output.messages [str] = [{"role": "assistant", "parts": [{"type": "text", "content": "I…<br>gen_ai.provider.name [str] = openai<br>gen_ai.request.choice.count [int] = 1<br>gen_ai.request.model [str] = gpt-4o-mini<br>gen_ai.response.finish_reasons [str] = ["stop"]<br>gen_ai.response.id [str] = chatcmpl-2<br>gen_ai.response.model [str] = gpt-4o-mini-2024-07-18<br>gen_ai.system_instructions [str] = [{"type": "text", "content": "You are a weather bot."}]<br>gen_ai.usage.input_tokens [int] = 131<br>gen_ai.usage.output_tokens [int] = 12<br>server.address [str] = http://127.0.0.1:41309/v1/ | — |

cetra today:

| span name | kind | provider | request / response model | in / out / total tokens | cost (source) | TTFT | session | input | output |
|---|---|---|---|---|---|---|---|---|---|
| `invoke_agent WeatherAgent` | agent | microsoft.agent_framework | gpt-4o-mini / — | — | — | — | — | What is the weather in Paris? | It is 18C and sunny in Paris. |
| `chat gpt-4o-mini` | llm | openai | gpt-4o-mini / gpt-4o-mini-2024-07-18 | 82 / 17 / 99 | 2.25e-05 (computed) | — | — | What is the weather in Paris? | present (no text) |
| `execute_tool get_weather` | tool | — | — / — | — | — | — | — | {"city":"Paris"} | 18C sunny in Paris |
| `chat gpt-4o-mini` | llm | openai | gpt-4o-mini / gpt-4o-mini-2024-07-18 | 131 / 12 / 143 | 2.68e-05 (computed) | — | — | What is the weather in Paris? | It is 18C and sunny in Paris. |

A minimal two-executor workflow (`WorkflowBuilder(start_executor=upper).add_edge(upper, sink).build()`, `await wf.run("hello")`):

| # | span name | scope · kind · parent · status | attributes: `key [type] = value` | events |
|---|---|---|---|---|
| 0 | `workflow.build` | agent_framework@1.26.0 · INTERNAL · root · UNSET | workflow.definition [str] = {"name": "WorkflowBuilder-3bc740f5-fc4b-4d33-b173-19fbe5e452a1", "…<br>workflow.id [str] = 1a99574f-cd81-46e9-a6d4-7f723381be0f<br>workflow_builder.name [str] = WorkflowBuilder-3bc740f5-fc4b-4d33-b173-19fbe5e452a1 | build.started: <br>build.validation_completed: <br>build.completed:  |
| 1 | `invoke_workflow` | agent_framework@1.26.0 · INTERNAL · root · UNSET | gen_ai.operation.name [str] = invoke_workflow<br>workflow.id [str] = 1a99574f-cd81-46e9-a6d4-7f723381be0f | workflow.started: <br>workflow.completed:  |
| 2 | `edge_group.process InternalEdgeGroup` | agent_framework@1.26.0 · INTERNAL · #1 · UNSET | edge_group.delivered [bool] = true<br>edge_group.delivery_status [str] = delivered<br>edge_group.id [str] = InternalEdgeGroup/a0032b32-3a3d-4add-9891-06c69dd95dcd<br>edge_group.type [str] = InternalEdgeGroup<br>message.source_id [str] = internal:upper<br>message.target_id [str] = upper | — |
| 3 | `executor.process upper` | agent_framework@1.26.0 · INTERNAL · #1 · UNSET | executor.id [str] = upper<br>executor.type [str] = FunctionExecutor<br>message.payload_type [str] = WorkflowMessage<br>message.type [str] = MessageType.STANDARD | — |
| 4 | `message.send` | agent_framework@1.26.0 · PRODUCER · #3 · UNSET | message.type [str] = str | — |
| 5 | `edge_group.process SingleEdgeGroup` | agent_framework@1.26.0 · INTERNAL · #1 · UNSET | edge_group.delivered [bool] = true<br>edge_group.delivery_status [str] = delivered<br>edge_group.id [str] = SingleEdgeGroup/df3304c2-6330-4a37-9321-cdfaf5bbb31f<br>edge_group.type [str] = SingleEdgeGroup<br>message.source_id [str] = upper | — |
| 6 | `executor.process sink` | agent_framework@1.26.0 · INTERNAL · #1 · UNSET | executor.id [str] = sink<br>executor.type [str] = FunctionExecutor<br>message.payload_type [str] = WorkflowMessage<br>message.type [str] = MessageType.STANDARD | — |

cetra today:

| span name | kind | provider | request / response model | in / out / total tokens | cost (source) | TTFT | session | input | output |
|---|---|---|---|---|---|---|---|---|---|
| `workflow.build` | span | — | — / — | — | — | — | — | empty | empty |
| `invoke_workflow` | chain | — | — / — | — | — | — | — | empty | empty |
| `edge_group.process InternalEdgeGroup` | span | — | — / — | — | — | — | — | empty | empty |
| `executor.process upper` | span | — | — / — | — | — | — | — | empty | empty |
| `message.send` | span | — | — / — | — | — | — | — | empty | empty |
| `edge_group.process SingleEdgeGroup` | span | — | — / — | — | — | — | — | empty | empty |
| `executor.process sink` | span | — | — / — | — | — | — | — | empty | empty |

With `OTEL_SEMCONV_STABILITY_OPT_IN` set to a value without `gen_ai_latest_experimental` (for example `http`, which people set for HTTP semconv), Agent Framework switches to the v1.36 baseline: `gen_ai.system` replaces `gen_ai.provider.name` and **no content is on the spans** (it goes to log records). The page must say to leave the variable unset or include `gen_ai_latest_experimental`.

Gaps:

| # | Key | Example | Should map to | Extends |
|---|---|---|---|---|
| M1 | `gen_ai.provider.name` = `microsoft.agent_framework` (on `invoke_agent`; in baseline mode on `gen_ai.system`) | | skipped (framework name) | framework-name table (R3) |
| M2 | span `workflow.build` with `workflow.id`, `workflow_builder.name`, `workflow.definition` | no operation | `chain` | § Span kind fallback (R2) |
| M3 | span `executor.process <id>` with `executor.id`, `executor.type`, `message.*` | no operation | `chain` | same |
| M4 | span `edge_group.process <type>` with `edge_group.id`, `edge_group.type`, `edge_group.delivered` | no operation | `chain` (D-IN-5) | same |
| M5 | span `message.send` (PRODUCER) with `message.type` | no operation | stays `span` | none |
| M6 | provider names the other clients write (source-read): `azure.ai.foundry`, `google.vertex.ai`, `mistralai`, `ollama`, `anthropic`, `aws.bedrock`, `azure.ai.inference`, `gcp.gemini`, `azure.ai.openai`, `unknown`, `agui` | | `google.vertex.ai` → `gcp.vertex_ai`; the others already map or stay as sent | rule 12 alias table |
| M7 | `gen_ai.response.finish_reasons` is a JSON string `["stop"]` on `invoke_agent`, an `array` elsewhere | | both are read today | none |

`invoke_workflow` (checked) is already `chain`. `gen_ai.conversation.id` appears only with an agent thread; this simple run has no session. `gen_ai.tool.type`, `gen_ai.tool.description`, `agent_framework.function.invocation.duration` (seconds) stay in `attributes`.

Nondeterministic: `gen_ai.agent.id` (UUID4), `workflow.id`, `workflow_builder.name` (`WorkflowBuilder-<uuid>`), `workflow.definition` (JSON with both ids), `edge_group.id` (`<type>/<uuid>`), `server.address` port. Phones home: no. The `agent-framework` meta package installs 30 sub-packages (211 packages); the captured scenario needs `agent-framework-core` and `agent-framework-openai` only.

## AG2 1.3.2

Setup (native middleware; `tracer_provider` is optional, default is the global one):

```python
mw = TelemetryMiddleware(tracer_provider=provider, agent_name="weather_agent", provider_name="openai", model_name="gpt-4o-mini")
agent = Agent("weather_agent", prompt="…", config=OpenAIConfig(model="gpt-4o-mini", api_key=..., base_url=...), tools=[get_weather], middleware=[mw])
await agent.ask("…")
```

`provider_name`, `model_name` and `agent_name` are constructor arguments; without them `gen_ai.provider.name` is absent from the agent and usage spans and `gen_ai.agent.name` is `unknown`. Span dump:

| # | span name | scope · kind · parent · status | attributes: `key [type] = value` | events |
|---|---|---|---|---|
| 0 | `invoke_agent weather_agent` | opentelemetry.instrumentation.ag2 · INTERNAL · root · UNSET | ag2.span.type [str] = agent<br>gen_ai.agent.name [str] = weather_agent<br>gen_ai.operation.name [str] = invoke_agent<br>gen_ai.provider.name [str] = openai<br>gen_ai.request.model [str] = gpt-4o-mini | — |
| 1 | `chat gpt-4o-mini` | opentelemetry.instrumentation.ag2 · CLIENT · #0 · UNSET | ag2.span.type [str] = llm<br>gen_ai.input.messages [str] = [{"content": "What is the weather in Paris?", "role": "user"}]<br>gen_ai.operation.name [str] = chat<br>gen_ai.output.messages [str] = [{"content": null, "role": "assistant", "tool_calls": [{"id": "…<br>gen_ai.provider.name [str] = openai<br>gen_ai.request.model [str] = gpt-4o-mini<br>gen_ai.response.finish_reasons [array] = ["tool_calls"]<br>gen_ai.response.model [str] = gpt-4o-mini-2024-07-18<br>gen_ai.usage.input_tokens [int] = 82<br>gen_ai.usage.output_tokens [int] = 17 | — |
| 2 | `record_usage model_call` | opentelemetry.instrumentation.ag2 · INTERNAL · #0 · UNSET | ag2.span.type [str] = usage<br>ag2.usage.event_id [str] = 15d33d519c14454cabf90d859d2d5660<br>ag2.usage.kind [str] = model_call<br>ag2.usage.total_tokens [int] = 99<br>gen_ai.provider.name [str] = openai<br>gen_ai.response.model [str] = gpt-4o-mini-2024-07-18<br>gen_ai.usage.input_tokens [int] = 82<br>gen_ai.usage.output_tokens [int] = 17 | — |
| 3 | `execute_tool get_weather` | opentelemetry.instrumentation.ag2 · INTERNAL · #0 · UNSET | ag2.span.type [str] = tool<br>gen_ai.operation.name [str] = execute_tool<br>gen_ai.tool.call.arguments [str] = {"city": "Paris"}<br>gen_ai.tool.call.id [str] = call_1<br>gen_ai.tool.call.result [str] = 18C sunny in Paris<br>gen_ai.tool.name [str] = get_weather<br>gen_ai.tool.type [str] = function | — |
| 4 | `chat gpt-4o-mini` | opentelemetry.instrumentation.ag2 · CLIENT · #0 · UNSET | ag2.span.type [str] = llm<br>gen_ai.input.messages [str] = [{"content": "What is the weather in Paris?", "role": "user"}, {…<br>gen_ai.operation.name [str] = chat<br>gen_ai.output.messages [str] = [{"content": "It is 18C and sunny in Paris.", "role": "assistan…<br>gen_ai.provider.name [str] = openai<br>gen_ai.request.model [str] = gpt-4o-mini<br>gen_ai.response.finish_reasons [array] = ["stop"]<br>gen_ai.response.model [str] = gpt-4o-mini-2024-07-18<br>gen_ai.usage.input_tokens [int] = 131<br>gen_ai.usage.output_tokens [int] = 12 | — |
| 5 | `record_usage model_call` | opentelemetry.instrumentation.ag2 · INTERNAL · #0 · UNSET | ag2.span.type [str] = usage<br>ag2.usage.event_id [str] = 73b9d3a18fd84f41a648d33bde7fcc67<br>ag2.usage.kind [str] = model_call<br>ag2.usage.total_tokens [int] = 143<br>gen_ai.provider.name [str] = openai<br>gen_ai.response.model [str] = gpt-4o-mini-2024-07-18<br>gen_ai.usage.input_tokens [int] = 131<br>gen_ai.usage.output_tokens [int] = 12 | — |

cetra today:

| span name | kind | provider | request / response model | in / out / total tokens | cost (source) | TTFT | session | input | output |
|---|---|---|---|---|---|---|---|---|---|
| `invoke_agent weather_agent` | agent | openai | gpt-4o-mini / — | — | — | — | — | empty | empty |
| `chat gpt-4o-mini` | llm | openai | gpt-4o-mini / gpt-4o-mini-2024-07-18 | 82 / 17 / 99 | 2.25e-05 (computed) | — | — | What is the weather in Paris? | present (no text) |
| `record_usage model_call` | span | openai | — / gpt-4o-mini-2024-07-18 | — | — | — | — | empty | empty |
| `execute_tool get_weather` | tool | — | — / — | — | — | — | — | {"city":"Paris"} | 18C sunny in Paris |
| `chat gpt-4o-mini` | llm | openai | gpt-4o-mini / gpt-4o-mini-2024-07-18 | 131 / 12 / 143 | 2.68e-05 (computed) | — | — | What is the weather in Paris? | It is 18C and sunny in Paris. |
| `record_usage model_call` | span | openai | — / gpt-4o-mini-2024-07-18 | — | — | — | — | empty | empty |

Gaps:

| # | Key | Example | Should map to | Extends |
|---|---|---|---|---|
| A1 | span `record_usage model_call` with `ag2.usage.kind`, `gen_ai.usage.*`, `gen_ai.response.model`, no operation, no request model | repeats the `chat` span's usage | **nothing** (stays `span`): D-IN-5's `llm` mapping double counts (finding 1) | none; D-IN-5 amended |
| A2 | `gen_ai.usage.thinking_tokens` (set only when non-zero; source `telemetry.py:442`; not captured) | `128` | `reasoning_tokens`, with the other `gen_ai.usage.*` (output includes reasoning: unverified, AG2 does not say) | § Model, provider, usage, `reasoning_tokens` row (R7) |
| A3 | `gen_ai.usage.cache_read_input_tokens` and `cache_creation_input_tokens` (source; not captured) | | already mapped | none |
| A4 | `gen_ai.usage.cache_creation_extended_input_tokens` (source) | | stays in `attributes`; not added to cache write | none |
| A5 | `await_human_input` span (source; not captured) | | `span` (the default; no rule) | none |
| A6 | `ag2.span.type` (`agent`, `llm`, `tool`, `usage`, `human_input`), `ag2.usage.event_id`, `ag2.usage.total_tokens`, `gen_ai.tool.type`, `gen_ai.tool.call.id` | | stay in `attributes` | none |

AG2's `chat` span carries `gen_ai.input.messages` and `gen_ai.output.messages` as OpenAI-format messages (with `tool_calls`), which the D-B1-3 path converts correctly (the first `chat` span's output keeps the tool call). The `invoke_agent` span has no input, output or usage. No session id. The instrumentation scope name is `opentelemetry.instrumentation.ag2` (not `ag2`); a scope gate must use that.

Nondeterministic: `ag2.usage.event_id` (UUID4 hex). Phones home: no.

## AutoGen (`autogen-agentchat` 0.7.5, OpenInference instrumentor 0.1.29)

Setup:

```python
AutogenAgentChatInstrumentor().instrument(tracer_provider=provider)
# do NOT also run OpenAIInstrumentor (finding 3)
client = OpenAIChatCompletionClient(model="gpt-4o-mini", base_url=..., api_key=...)
agent = AssistantAgent("weather_agent", model_client=client, tools=[get_weather], system_message="…", reflect_on_tool_use=True)   # reflect: otherwise one model turn only
await agent.run(task="…")
```

Span dump, with the OpenInference AutoGen instrumentor **and** `OpenAIInstrumentor` (this is the design's route; the `ChatCompletion` spans are the OpenAI instrumentor's):

| # | span name | scope · kind · parent · status | attributes: `key [type] = value` | events |
|---|---|---|---|---|
| 0 | `create_agent weather_agent` | autogen-core · CLIENT · root · UNSET | gen_ai.agent.description [str] = An agent that provides assistance with ability to use tools.<br>gen_ai.agent.name [str] = weather_agent<br>gen_ai.operation.name [str] = create_agent<br>gen_ai.system [str] = autogen | — |
| 1 | `invoke_agent weather_agent` | autogen-core · CLIENT · root · UNSET | gen_ai.agent.description [str] = An agent that provides assistance with ability to use tools.<br>gen_ai.agent.name [str] = weather_agent<br>gen_ai.operation.name [str] = invoke_agent<br>gen_ai.system [str] = autogen | — |
| 2 | `weather_agent.on_messages_stream` | openinference.instrumentation.autogen_agentchat@0.1.29 · INTERNAL · #1 · OK | graph.node.id [str] = weather_agent<br>graph.node.parent_id [str] = start<br>input.value [str] = {"messages": ["id='3b7233cc-af6f-442a-9a02-6d13a76078e3' source='user' mod…<br>openinference.span.kind [str] = AGENT | exception: exception.escaped[str]=False, exception.message[str]=, exception.stacktrace[str]=Traceback (m… |
| 3 | `OpenAIChatCompletionClient.create` | openinference.instrumentation.autogen_agentchat@0.1.29 · INTERNAL · #2 · OK | llm.input_messages.0.message.content [str] = You are a weather bot.<br>llm.input_messages.0.message.role [str] = developer<br>llm.input_messages.1.message.content [str] = What is the weather in Paris?<br>llm.input_messages.1.message.role [str] = user<br>llm.model_name [str] = gpt-4o-mini-2024-07-18<br>llm.output_messages.0.message.role [str] = assistant<br>llm.output_messages.0.message.tool_calls.0.tool_call.function.arguments [str] = {"city": "Pari…<br>llm.output_messages.0.message.tool_calls.0.tool_call.function.name [str] = get_weather<br>llm.output_messages.0.message.tool_calls.0.tool_call.id [str] = call_1<br>llm.token_count.completion [int] = 17<br>llm.token_count.prompt [int] = 82<br>llm.token_count.total [int] = 99<br>openinference.span.kind [str] = LLM<br>output.mime_type [str] = application/json<br>output.value [str] = {"finish_reason": "function_calls", "content": [{"id": "call_1", "argumen… | — |
| 4 | `ChatCompletion` | openinference.instrumentation.openai@0.1.69 · INTERNAL · #3 · OK | input.mime_type [str] = application/json<br>input.value [str] = {"model": "gpt-4o-mini", "stream": false, "messages": [{"content": "You ar…<br>llm.finish_reason [str] = tool_calls<br>llm.input_messages.0.message.content [str] = You are a weather bot.<br>llm.input_messages.0.message.role [str] = system<br>llm.input_messages.1.message.content [str] = What is the weather in Paris?<br>llm.input_messages.1.message.name [str] = user<br>llm.input_messages.1.message.role [str] = user<br>llm.invocation_parameters [str] = {"model": "gpt-4o-mini", "stream": false, "tool_choice": "au…<br>llm.model_name [str] = gpt-4o-mini-2024-07-18<br>llm.output_messages.0.message.role [str] = assistant<br>llm.output_messages.0.message.tool_calls.0.tool_call.function.arguments [str] = {"city": "Pari…<br>llm.output_messages.0.message.tool_calls.0.tool_call.function.name [str] = get_weather<br>llm.output_messages.0.message.tool_calls.0.tool_call.id [str] = call_1<br>llm.system [str] = openai<br>llm.token_count.completion [int] = 17<br>llm.token_count.prompt [int] = 82<br>llm.token_count.total [int] = 99<br>llm.tools.0.tool.json_schema [str] = {"type": "function", "function": {"name": "get_weather", …<br>openinference.span.kind [str] = LLM<br>output.mime_type [str] = application/json<br>output.value [str] = {"id":"chatcmpl-1","choices":[{"finish_reason":"tool_calls","index":0,"me… | — |
| 5 | `weather_agent.get_weather` | openinference.instrumentation.autogen_agentchat@0.1.29 · INTERNAL · #2 · OK | input.mime_type [str] = application/json<br>input.value [str] = {"city": "Paris"}<br>openinference.span.kind [str] = TOOL<br>output.mime_type [str] = text/plain<br>output.value [str] = 18C sunny in Paris<br>tool.description [str] = Get the weather.<br>tool.name [str] = get_weather<br>tool.parameters [str] = {"city": "Paris"} | — |
| 6 | `execute_tool get_weather` | autogen-core · INTERNAL · #5 · UNSET | gen_ai.operation.name [str] = execute_tool<br>gen_ai.system [str] = autogen<br>gen_ai.tool.call.id [str] = call_1<br>gen_ai.tool.description [str] = Get the weather.<br>gen_ai.tool.name [str] = get_weather | — |
| 7 | `OpenAIChatCompletionClient.create` | openinference.instrumentation.autogen_agentchat@0.1.29 · INTERNAL · #2 · OK | llm.input_messages.0.message.content [str] = You are a weather bot.<br>llm.input_messages.0.message.role [str] = developer<br>llm.input_messages.1.message.content [str] = What is the weather in Paris?<br>llm.input_messages.1.message.role [str] = user<br>llm.input_messages.2.message.role [str] = assistant<br>llm.input_messages.2.message.tool_calls.0.tool_call.function.arguments [str] = {"city": "Paris…<br>llm.input_messages.2.message.tool_calls.0.tool_call.function.name [str] = get_weather<br>llm.input_messages.2.message.tool_calls.0.tool_call.id [str] = call_1<br>llm.input_messages.3.function.0 [str] = {"name": "get_weather", "content": "18C sunny in Paris…<br>llm.input_messages.3.message.role [str] = function<br>llm.model_name [str] = gpt-4o-mini-2024-07-18<br>llm.output_messages.0.message.content [str] = It is 18C and sunny in Paris.<br>llm.output_messages.0.message.role [str] = assistant<br>llm.token_count.completion [int] = 12<br>llm.token_count.prompt [int] = 131<br>llm.token_count.total [int] = 143<br>openinference.span.kind [str] = LLM<br>output.mime_type [str] = application/json<br>output.value [str] = {"finish_reason": "stop", "content": "It is 18C and sunny in Paris.", "us… | — |
| 8 | `ChatCompletion` | openinference.instrumentation.openai@0.1.69 · INTERNAL · #7 · OK | input.mime_type [str] = application/json<br>input.value [str] = {"model": "gpt-4o-mini", "stream": false, "messages": [{"content": "You ar…<br>llm.finish_reason [str] = stop<br>llm.input_messages.0.message.content [str] = You are a weather bot.<br>llm.input_messages.0.message.role [str] = system<br>llm.input_messages.1.message.content [str] = What is the weather in Paris?<br>llm.input_messages.1.message.name [str] = user<br>llm.input_messages.1.message.role [str] = user<br>llm.input_messages.2.message.role [str] = assistant<br>llm.input_messages.2.message.tool_calls.0.tool_call.function.arguments [str] = {"city": "Paris…<br>llm.input_messages.2.message.tool_calls.0.tool_call.function.name [str] = get_weather<br>llm.input_messages.2.message.tool_calls.0.tool_call.id [str] = call_1<br>llm.input_messages.3.message.content [str] = 18C sunny in Paris<br>llm.input_messages.3.message.role [str] = tool<br>llm.input_messages.3.message.tool_call_id [str] = call_1<br>llm.invocation_parameters [str] = {"model": "gpt-4o-mini", "stream": false}<br>llm.model_name [str] = gpt-4o-mini-2024-07-18<br>llm.output_messages.0.message.content [str] = It is 18C and sunny in Paris.<br>llm.output_messages.0.message.role [str] = assistant<br>llm.system [str] = openai<br>llm.token_count.completion [int] = 12<br>llm.token_count.prompt [int] = 131<br>llm.token_count.total [int] = 143<br>openinference.span.kind [str] = LLM<br>output.mime_type [str] = application/json<br>output.value [str] = {"id":"chatcmpl-2","choices":[{"finish_reason":"stop","index":0,"message"… | — |

cetra today:

| span name | kind | provider | request / response model | in / out / total tokens | cost (source) | TTFT | session | input | output |
|---|---|---|---|---|---|---|---|---|---|
| `create_agent weather_agent` | agent | — | — / — | — | — | — | — | empty | empty |
| `invoke_agent weather_agent` | agent | — | — / — | — | — | — | — | empty | empty |
| `weather_agent.on_messages_stream` | agent | — | — / — | — | — | — | — | {"messages": ["id='3b7233cc-af6f-442a-… | empty |
| `OpenAIChatCompletionClient.create` | llm | openai | gpt-4o-mini-2024-07-18 / — | 82 / 17 / 99 | 2.25e-05 (computed) | — | — | What is the weather in Paris? | present (no text) |
| `ChatCompletion` | llm | openai | gpt-4o-mini-2024-07-18 / — | 82 / 17 / 99 | 2.25e-05 (computed) | — | — | What is the weather in Paris? | present (no text) |
| `weather_agent.get_weather` | tool | — | — / — | — | — | — | — | {"city":"Paris"} | 18C sunny in Paris |
| `execute_tool get_weather` | tool | — | — / — | — | — | — | — | empty | empty |
| `OpenAIChatCompletionClient.create` | llm | openai | gpt-4o-mini-2024-07-18 / — | 131 / 12 / 143 | 2.68e-05 (computed) | — | — | What is the weather in Paris? | It is 18C and sunny in Paris. |
| `ChatCompletion` | llm | openai | gpt-4o-mini-2024-07-18 / — | 131 / 12 / 143 | 2.68e-05 (computed) | — | — | What is the weather in Paris? | It is 18C and sunny in Paris. |

AutoGen's native spans alone (no instrumentor): three spans only, `create_agent`, `invoke_agent`, `execute_tool`, with `gen_ai.system=autogen`, no model, no tokens, no messages. They are also present (duplicated) when the instrumentor is on, because `AUTOGEN_DISABLE_RUNTIME_TRACING=true` only disables the runtime's spans: the agent-chat spans stay.

Gaps:

| # | Key | Example | Should map to | Extends |
|---|---|---|---|---|
| G1 | the AutoGen instrumentor's `llm` span (`OpenAIChatCompletionClient.create`) and the OpenAI instrumentor's `ChatCompletion` both `llm` with tokens | two priced spans per call | page: enable the AutoGen instrumentor only | none (page); open question 2 |
| G2 | `llm.input_messages.N.message.role` = `function`, with `llm.input_messages.N.function.0` = `{"name":"get_weather","content":"18C sunny in Paris","call_id":"call_1","is_error":false}` | tool result of the second model call | a `tool` message with one `tool_call_response` part `{id: call_id, response: content}` | § Input/output, OpenInference row (R8). Today the message is `{"role":"function","parts":[]}` and the result is lost |
| G3 | `gen_ai.system` = `autogen` on the native spans | | skipped (already in the table, D-IN-15); verified: provider stays empty | none |
| G4 | no provider on the AutoGen `llm` span (no `llm.system`, no `llm.provider`, no host) | provider shows `openai` only because the price table infers it from `gpt-4o-mini-2024-07-18` | nothing: documented limit (a non-OpenAI model through the OpenAI client is priced by model name only) | none |
| G5 | agent span `input.value` = `{"messages": ["id='…' source='user' models_usage=None metadata={} created_at=datetime.datetime(…) content='…' type='TextMessage'"], "cancellation_token": "<…CancellationToken object at 0x7f…>"}` (a Python repr, no `input.mime_type`) | | stays text; works | none, but nondeterministic (below) |
| G6 | an `exception` event on the (OK) agent span: `asyncio.exceptions.CancelledError`, empty message, a stack trace with local file paths | | stays in `events`; noise from AutoGen's cancellation token | none |

Kinds: native `create_agent`/`invoke_agent` → agent, `execute_tool` → tool; OpenInference `AGENT` / `LLM` / `TOOL` kinds map. An agent invocation therefore shows twice (native `invoke_agent` root and the instrumentor's `<name>.on_messages_stream` child) and a tool call twice (the instrumentor's `<agent>.<tool>` span is the parent of the native `execute_tool`). Neither carries tokens.

Nondeterministic (this scenario is the worst of the eight): the agent span's `input.value` holds a message UUID, a `created_at` timestamp and a memory address (`0x7f…`); `volatile_attributes` replaces whole attribute values per key, so masking `input.value` would also mask the `ChatCompletion` request payloads. The capture needs either a scrub rule for `0x[0-9a-f]+` and ISO timestamps inside `input.value`, or a different AutoGen API surface for the case. Phones home: no.

## Mirascope 2.8.2

Setup (no environment bootstrap: the provider is built in code; `mirascope[ops]` imports `anthropic` and `google-genai` at import time, so all three SDKs must be installed):

```python
ops.configure(tracer_provider=provider)
ops.instrument_llm()
@ops.trace
@llm.call("openai/gpt-4o-mini", tools=[get_weather])
def ask(q: str): return q
with ops.session(id="sess-1"):
    resp = ask("…")
    while resp.tool_calls: resp = resp.resume(resp.execute_tools())
```

The model id `openai/gpt-4o-mini` selects OpenAI's Responses API, so the captured case needs the mock's `/v1/responses` route (it exists, D-IN-4); the dump below is that run (canned Responses JSON). `openai:completions/gpt-4o-mini` (registered with `llm.register_provider("openai:completions", scope="openai:completions/", api_key=…, base_url=…)`) selects chat completions and writes `gen_ai.provider.name = openai:completions` and the model `openai:completions/gpt-4o-mini`.

Span dump (Responses API):

| # | span name | scope · kind · parent · status | attributes: `key [type] = value` | events |
|---|---|---|---|---|
| 0 | `run` | mirascope.ops · INTERNAL · root · UNSET | mirascope.fn.is_async [bool] = false<br>mirascope.fn.module [str] = __main__<br>mirascope.fn.qualname [str] = run<br>mirascope.ops.session.id [str] = sess-1<br>mirascope.trace.arg_types [str] = {"q":"str"}<br>mirascope.trace.arg_values [str] = {"q":"What is the weather in Paris?"}<br>mirascope.trace.output [str] = It is 18C and sunny in Paris.<br>mirascope.type [str] = trace | — |
| 1 | `ask.call` | mirascope.ops · INTERNAL · #0 · UNSET | mirascope.fn.is_async [bool] = false<br>mirascope.fn.module [str] = __main__<br>mirascope.fn.qualname [str] = ask.call<br>mirascope.ops.session.id [str] = sess-1<br>mirascope.response.content [str] = [{"type":"tool_call","id":"call_1","name":"get_weather","ar…<br>mirascope.response.messages [str] = [{"role":"user","content":[{"type":"text","text":"What is …<br>mirascope.response.model_id [str] = openai/gpt-4o-mini<br>mirascope.response.provider_id [str] = openai<br>mirascope.response.usage [str] = {"input_tokens":82,"output_tokens":17,"cache_read_tokens":0,"…<br>mirascope.trace.arg_types [str] = {"q":"str"}<br>mirascope.trace.arg_values [str] = {"q":"What is the weather in Paris?"}<br>mirascope.trace.output [str] = **ToolCall (get_weather):** {"city": "Paris"}<br>mirascope.type [str] = trace | — |
| 2 | `chat openai/gpt-4o-mini` | mirascope.llm · CLIENT · #1 · UNSET | gen_ai.input.messages [str] = [{"role":"user","parts":[{"type":"text","content":"What is the w…<br>gen_ai.operation.name [str] = chat<br>gen_ai.output.messages [str] = [{"role":"assistant","parts":[{"type":"tool_call","id":"call_1"…<br>gen_ai.output.type [str] = text<br>gen_ai.provider.name [str] = openai<br>gen_ai.request.model [str] = openai/gpt-4o-mini<br>gen_ai.response.finish_reasons [array] = ["stop"]<br>gen_ai.response.id [str] = resp_1<br>gen_ai.response.model [str] = openai/gpt-4o-mini<br>gen_ai.tool.definitions [str] = [{"name":"get_weather","description":"Get the weather.","type"…<br>gen_ai.usage.input_tokens [int] = 82<br>gen_ai.usage.output_tokens [int] = 17<br>mirascope.response.content [str] = [{"type":"tool_call","id":"call_1","name":"get_weather","ar…<br>mirascope.response.messages [str] = [{"role":"user","content":[{"type":"text","text":"What is …<br>mirascope.response.usage [str] = {"input_tokens":82,"output_tokens":17,"cache_read_tokens":0,"… | — |
| 3 | `Response.resume openai/gpt-4o-mini` | mirascope.ops · INTERNAL · #0 · UNSET | mirascope.ops.session.id [str] = sess-1<br>mirascope.response.content [str] = [{"type":"text","text":"It is 18C and sunny in Paris."}]<br>mirascope.response.messages [str] = [{"role":"user","content":[{"type":"text","text":"What is …<br>mirascope.response.model_id [str] = openai/gpt-4o-mini<br>mirascope.response.provider_id [str] = openai<br>mirascope.response.usage [str] = {"input_tokens":131,"output_tokens":12,"cache_read_tokens":0,…<br>mirascope.trace.output [str] = It is 18C and sunny in Paris.<br>mirascope.type [str] = response_resume | — |
| 4 | `chat openai/gpt-4o-mini` | mirascope.llm · CLIENT · #3 · UNSET | gen_ai.input.messages [str] = [{"role":"user","parts":[{"type":"text","content":"What is the w…<br>gen_ai.operation.name [str] = chat<br>gen_ai.output.messages [str] = [{"role":"assistant","parts":[{"type":"text","content":"It is 1…<br>gen_ai.output.type [str] = text<br>gen_ai.provider.name [str] = openai<br>gen_ai.request.model [str] = openai/gpt-4o-mini<br>gen_ai.response.finish_reasons [array] = ["stop"]<br>gen_ai.response.id [str] = resp_2<br>gen_ai.response.model [str] = openai/gpt-4o-mini<br>gen_ai.tool.definitions [str] = [{"name":"get_weather","description":"Get the weather.","type"…<br>gen_ai.usage.input_tokens [int] = 131<br>gen_ai.usage.output_tokens [int] = 12<br>mirascope.response.content [str] = [{"type":"text","text":"It is 18C and sunny in Paris."}]<br>mirascope.response.messages [str] = [{"role":"user","content":[{"type":"text","text":"What is …<br>mirascope.response.usage [str] = {"input_tokens":131,"output_tokens":12,"cache_read_tokens":0,… | — |

cetra today:

| span name | kind | provider | request / response model | in / out / total tokens | cost (source) | TTFT | session | input | output |
|---|---|---|---|---|---|---|---|---|---|
| `run` | span | — | — / — | — | — | — | — | empty | empty |
| `ask.call` | span | — | — / — | — | — | — | — | empty | empty |
| `chat openai/gpt-4o-mini` | llm | openai | openai/gpt-4o-mini / openai/gpt-4o-mini | 82 / 17 / 99 | — (none) | — | — | What is the weather in Paris? | present (no text) |
| `Response.resume openai/gpt-4o-mini` | span | — | — / — | — | — | — | — | empty | empty |
| `chat openai/gpt-4o-mini` | llm | openai | openai/gpt-4o-mini / openai/gpt-4o-mini | 131 / 12 / 143 | — (none) | — | — | present (no text) | It is 18C and sunny in Paris. |

Gaps:

| # | Key | Example | Should map to | Extends |
|---|---|---|---|---|
| R-a | `gen_ai.request.model` and `gen_ai.response.model` = `openai/gpt-4o-mini` | `<provider>/<model>`; the API's own `gpt-4o-mini-2024-07-18` is not forwarded | request and response model with the leading `<provider>/` removed when it equals the raw `gen_ai.provider.name` (`openai/` for `openai`) | § Model, provider, usage (R9). Without it cost is `none` |
| R-b | `gen_ai.provider.name` = `openai:completions`, `openai:responses`, `anthropic-beta` (source-read provider ids; captured: `openai`, `openai:completions`) | | `openai`, `openai`, `anthropic` | rule 12 alias table (R3) |
| R-c | `mirascope.trace.arg_values` (JSON object) and `mirascope.trace.output` (text) on `@ops.trace` spans (`run`, `ask.call`, `Response.resume …`; `mirascope.type` = `trace`, `response_resume`) | `{"q":"What is the weather in Paris?"}` / `It is 18C and sunny in Paris.` | `input` (json) and `output` (text), on spans of scope `mirascope.ops` | § Input/output, new Mirascope row (R10) |
| R-d | `mirascope.ops.session.id` | `sess-1` | `session_id` (it is on every Mirascope span inside `ops.session`) | § Context fields, `session_id` (R10) |
| R-e | `mirascope.response.usage` (JSON), `mirascope.response.content`, `mirascope.response.messages`, `mirascope.response.model_id`, `mirascope.response.provider_id` on `ask.call` and `Response.resume` | usage `{"input_tokens":82,"output_tokens":17,…}` | **nothing**: these spans are `span`, and tokens are only read on `llm` spans, so they cannot double count; leave unread | none |
| R-f | `gen_ai.output.type`, `mirascope.fn.*`, `mirascope.trace.arg_types`, `mirascope.type`, `gen_ai.response.finish_reasons` = `["stop"]` even for the tool-call turn | | stay in `attributes` | none |

Kinds: `chat …` → llm (operation); `@ops.trace` spans have no operation → `span` (a `chain` would be natural; Mirascope marks them only with `mirascope.type`; open question 5). `gen_ai.tool.definitions` is Mirascope's own shape `[{"name","description","type":"function","parameters"}]` and normalizes. The chat span has no `server.address`, so the host rule does not apply (provider comes from `gen_ai.provider.name`).

Nondeterministic: none of ids or timestamps in attributes (the span ids are re-minted by the capture tool); `mirascope.fn.module` is `__main__`; `mirascope.response.messages` holds the whole conversation as JSON. Phones home: no non-loopback connection attempt was seen.

## OpenHands SDK 1.62.0 (through Laminar `lmnr` 0.8.8)

Setup (environment only; **never set `LMNR_PROJECT_API_KEY`**: with it Laminar defaults to `https://api.lmnr.ai` and sends the spans to Laminar's cloud. The first run of this spike set it to a dummy value by mistake: Laminar logged that it would export to `https://api.lmnr.ai`, the content was only the fake scenario's, and whether the request succeeded was not checked. All later runs omitted the key):

```
OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://localhost:8080/v1/traces     # used verbatim as the URL
OTEL_EXPORTER_OTLP_TRACES_HEADERS="Authorization=Bearer%20sk-cetra-…"
OTEL_EXPORTER_OTLP_TRACES_PROTOCOL=http/protobuf    # or OTEL_EXPORTER=otlp_http
LMNR_FORCE_HTTP=true  OPENHANDS_SUPPRESS_BANNER=1  LITELLM_LOCAL_MODEL_COST_MAP=True
```

```python
llm = LLM(model="openai/gpt-4o-mini", base_url=..., api_key=..., usage_id="agent")
conv = Conversation(agent=Agent(llm=llm, tools=[]), workspace=tmpdir)
conv.send_message("…"); conv.run()
```

`tools=[]` leaves only the built-in `finish` tool (the `think` tool of the first canned turn does not exist in 1.62: the SDK records an `InvalidToolCall` span, which is a realistic error path, and the second turn calls `finish`). A `think`-less script with a real tool would need `openhands-tools` (a terminal and tmux); skipped.

Span dump (received by the in-process OTLP/HTTP sink from Laminar's exporter):

| # | span name | scope · kind · parent · status | attributes: `key [type] = value` | events |
|---|---|---|---|---|
| 0 | `conversation.send_message` | lmnr.tracer · INTERNAL · root · UNSET | lmnr.association.properties.session_id [str] = 51d6389a-b6dd-4fd1-ab06-483f571d42ac<br>lmnr.span.ids_path [array] = ["00000000-0000-0000-01d2-b177b24cbe0f", "00000000-0000-0000-aaff…<br>lmnr.span.input [str] = {"message":"What is the weather in Paris?"}<br>lmnr.span.instrumentation_source [str] = python<br>lmnr.span.language_version [str] = python@3.13<br>lmnr.span.path [array] = ["conversation", "conversation.send_message"]<br>lmnr.span.sdk_version [str] = 0.8.8<br>lmnr.span.type [str] = DEFAULT | — |
| 1 | `conversation.run` | lmnr.tracer · INTERNAL · root · UNSET | lmnr.association.properties.session_id [str] = 51d6389a-b6dd-4fd1-ab06-483f571d42ac<br>lmnr.span.ids_path [array] = ["00000000-0000-0000-01d2-b177b24cbe0f", "00000000-0000-0000-3f6e…<br>lmnr.span.input [str] = {}<br>lmnr.span.instrumentation_source [str] = python<br>lmnr.span.language_version [str] = python@3.13<br>lmnr.span.path [array] = ["conversation", "conversation.run"]<br>lmnr.span.sdk_version [str] = 0.8.8<br>lmnr.span.type [str] = DEFAULT | — |
| 2 | `agent.step` | lmnr.tracer · INTERNAL · #1 · UNSET | lmnr.association.properties.session_id [str] = 51d6389a-b6dd-4fd1-ab06-483f571d42ac<br>lmnr.span.ids_path [array] = ["00000000-0000-0000-01d2-b177b24cbe0f", "00000000-0000-0000-3f6e…<br>lmnr.span.input [str] = {"on_token":null,"conversation":"<openhands.sdk.conversation.impl.loca…<br>lmnr.span.instrumentation_source [str] = python<br>lmnr.span.language_version [str] = python@3.13<br>lmnr.span.path [array] = ["conversation", "conversation.run", "agent.step"]<br>lmnr.span.sdk_version [str] = 0.8.8<br>lmnr.span.type [str] = DEFAULT | — |
| 3 | `llm.openai/gpt-4o-mini` | lmnr.tracer · INTERNAL · #2 · UNSET | gen_ai.usage.cache_creation_input_tokens [int] = 0<br>gen_ai.usage.cache_read_input_tokens [int] = 0<br>gen_ai.usage.cost [double] = 2.2499999999999998e-05<br>lmnr.association.properties.session_id [str] = 51d6389a-b6dd-4fd1-ab06-483f571d42ac<br>lmnr.span.ids_path [array] = ["00000000-0000-0000-01d2-b177b24cbe0f", "00000000-0000-0000-3f6e…<br>lmnr.span.instrumentation_source [str] = python<br>lmnr.span.language_version [str] = python@3.13<br>lmnr.span.parent_ids_path [array] = []<br>lmnr.span.parent_path [array] = []<br>lmnr.span.path [array] = ["conversation", "conversation.run", "agent.step", "llm.openai/gpt-4o…<br>lmnr.span.sdk_version [str] = 0.8.8<br>lmnr.span.type [str] = LLM | — |
| 4 | `litellm.completion` | lmnr.tracer · INTERNAL · #3 · UNSET | gen_ai.input.messages [str] = [{"content":[{"type":"text","text":"<SOUL>\nYou are OpenHands ag…<br>gen_ai.output.messages [str] = [{"content":null,"role":"assistant","tool_calls":[{"function":{…<br>gen_ai.request.max_tokens [int] = 16384<br>gen_ai.request.model [str] = gpt-4o-mini<br>gen_ai.response.id [str] = chatcmpl-1<br>gen_ai.response.model [str] = gpt-4o-mini-2024-07-18<br>gen_ai.system [str] = openai<br>gen_ai.tool.definitions [str] = [{"type":"function","function":{"name":"finish","description":…<br>gen_ai.usage.input_tokens [int] = 82<br>gen_ai.usage.output_tokens [int] = 17<br>llm.usage.total_tokens [int] = 99<br>lmnr.association.properties.session_id [str] = 51d6389a-b6dd-4fd1-ab06-483f571d42ac<br>lmnr.span.ids_path [array] = ["00000000-0000-0000-01d2-b177b24cbe0f", "00000000-0000-0000-3f6e…<br>lmnr.span.instrumentation_scope.name [str] = litellm<br>lmnr.span.instrumentation_scope.version [str] = 1.105.7<br>lmnr.span.instrumentation_source [str] = python<br>lmnr.span.language_version [str] = python@3.13<br>lmnr.span.parent_ids_path [array] = []<br>lmnr.span.parent_path [array] = []<br>lmnr.span.path [array] = ["conversation", "conversation.run", "agent.step", "llm.openai/gpt-4o…<br>lmnr.span.sdk_version [str] = 0.8.8<br>lmnr.span.type [str] = LLM | — |
| 5 | `InvalidToolCall` | lmnr.tracer · INTERNAL · root · UNSET | lmnr.association.properties.metadata.tool_call_id [str] = call_1<br>lmnr.association.properties.session_id [str] = 51d6389a-b6dd-4fd1-ab06-483f571d42ac<br>lmnr.span.ids_path [array] = ["00000000-0000-0000-01d2-b177b24cbe0f", "00000000-0000-0000-0a08…<br>lmnr.span.input [str] = {"tool_input":{"id":"call_1","responses_item_id":null,"name":"think","…<br>lmnr.span.instrumentation_source [str] = python<br>lmnr.span.language_version [str] = python@3.13<br>lmnr.span.output [str] = {"role":"tool","content":[{"cache_prompt":false,"type":"text","text":…<br>lmnr.span.path [array] = ["conversation", "InvalidToolCall"]<br>lmnr.span.sdk_version [str] = 0.8.8<br>lmnr.span.type [str] = TOOL | — |
| 6 | `agent.step` | lmnr.tracer · INTERNAL · #1 · UNSET | lmnr.association.properties.session_id [str] = 51d6389a-b6dd-4fd1-ab06-483f571d42ac<br>lmnr.span.ids_path [array] = ["00000000-0000-0000-01d2-b177b24cbe0f", "00000000-0000-0000-3f6e…<br>lmnr.span.input [str] = {"on_token":null,"conversation":"<openhands.sdk.conversation.impl.loca…<br>lmnr.span.instrumentation_source [str] = python<br>lmnr.span.language_version [str] = python@3.13<br>lmnr.span.path [array] = ["conversation", "conversation.run", "agent.step"]<br>lmnr.span.sdk_version [str] = 0.8.8<br>lmnr.span.type [str] = DEFAULT | — |
| 7 | `llm.openai/gpt-4o-mini` | lmnr.tracer · INTERNAL · #6 · UNSET | gen_ai.usage.cache_creation_input_tokens [int] = 0<br>gen_ai.usage.cache_read_input_tokens [int] = 0<br>gen_ai.usage.cost [double] = 2.685e-05<br>lmnr.association.properties.session_id [str] = 51d6389a-b6dd-4fd1-ab06-483f571d42ac<br>lmnr.span.ids_path [array] = ["00000000-0000-0000-01d2-b177b24cbe0f", "00000000-0000-0000-3f6e…<br>lmnr.span.instrumentation_source [str] = python<br>lmnr.span.language_version [str] = python@3.13<br>lmnr.span.parent_ids_path [array] = []<br>lmnr.span.parent_path [array] = []<br>lmnr.span.path [array] = ["conversation", "conversation.run", "agent.step", "llm.openai/gpt-4o…<br>lmnr.span.sdk_version [str] = 0.8.8<br>lmnr.span.type [str] = LLM | — |
| 8 | `litellm.completion` | lmnr.tracer · INTERNAL · #7 · UNSET | gen_ai.input.messages [str] = [{"content":[{"type":"text","text":"<SOUL>\nYou are OpenHands ag…<br>gen_ai.output.messages [str] = [{"content":null,"role":"assistant","tool_calls":[{"function":{…<br>gen_ai.request.max_tokens [int] = 16384<br>gen_ai.request.model [str] = gpt-4o-mini<br>gen_ai.response.id [str] = chatcmpl-1<br>gen_ai.response.model [str] = gpt-4o-mini-2024-07-18<br>gen_ai.system [str] = openai<br>gen_ai.tool.definitions [str] = [{"type":"function","function":{"name":"finish","description":…<br>gen_ai.usage.input_tokens [int] = 131<br>gen_ai.usage.output_tokens [int] = 12<br>llm.usage.total_tokens [int] = 143<br>lmnr.association.properties.session_id [str] = 51d6389a-b6dd-4fd1-ab06-483f571d42ac<br>lmnr.span.ids_path [array] = ["00000000-0000-0000-01d2-b177b24cbe0f", "00000000-0000-0000-3f6e…<br>lmnr.span.instrumentation_scope.name [str] = litellm<br>lmnr.span.instrumentation_scope.version [str] = 1.105.7<br>lmnr.span.instrumentation_source [str] = python<br>lmnr.span.language_version [str] = python@3.13<br>lmnr.span.parent_ids_path [array] = []<br>lmnr.span.parent_path [array] = []<br>lmnr.span.path [array] = ["conversation", "conversation.run", "agent.step", "llm.openai/gpt-4o…<br>lmnr.span.sdk_version [str] = 0.8.8<br>lmnr.span.type [str] = LLM | — |
| 9 | `FinishAction` | lmnr.tracer · INTERNAL · #6 · UNSET | lmnr.association.properties.metadata.tool_call_id [str] = call_1<br>lmnr.association.properties.session_id [str] = 51d6389a-b6dd-4fd1-ab06-483f571d42ac<br>lmnr.span.ids_path [array] = ["00000000-0000-0000-01d2-b177b24cbe0f", "00000000-0000-0000-3f6e…<br>lmnr.span.input [str] = {"action":{"message":"It is 18C and sunny in Paris.","kind":"FinishAct…<br>lmnr.span.instrumentation_source [str] = python<br>lmnr.span.language_version [str] = python@3.13<br>lmnr.span.output [str] = {"content":[{"cache_prompt":false,"type":"text","text":"It is 18C and…<br>lmnr.span.path [array] = ["conversation", "conversation.run", "agent.step", "FinishAction"]<br>lmnr.span.sdk_version [str] = 0.8.8<br>lmnr.span.type [str] = TOOL | — |

cetra today:

| span name | kind | provider | request / response model | in / out / total tokens | cost (source) | TTFT | session | input | output |
|---|---|---|---|---|---|---|---|---|---|
| `conversation.send_message` | chain | — | — / — | — | — | — | yes | {"message":"What is the weather in Par… | empty |
| `conversation.run` | chain | — | — / — | — | — | — | yes | {} | empty |
| `agent.step` | chain | — | — / — | — | — | — | yes | {"on_token":null,"conversation":"<open… | empty |
| `llm.openai/gpt-4o-mini` | llm | — | — / — | — | 2.25e-05 (reported) | — | yes | empty | empty |
| `litellm.completion` | llm | openai | gpt-4o-mini / gpt-4o-mini-2024-07-18 | 82 / 17 / 99 | 2.25e-05 (computed) | — | yes | What is the weather in Paris? | present (no text) |
| `InvalidToolCall` | tool | — | — / — | — | — | — | yes | {"tool_input":{"id":"call_1","response… | present (no text) |
| `agent.step` | chain | — | — / — | — | — | — | yes | {"on_token":null,"conversation":"<open… | empty |
| `llm.openai/gpt-4o-mini` | llm | — | — / — | — | 2.68e-05 (reported) | — | yes | empty | empty |
| `litellm.completion` | llm | openai | gpt-4o-mini / gpt-4o-mini-2024-07-18 | 131 / 12 / 143 | 2.68e-05 (computed) | — | yes | What is the weather in Paris? | present (no text) |
| `FinishAction` | tool | — | — / — | — | — | — | yes | {"action":{"message":"It is 18C and su… | {"content":[{"cache_prompt":false,"typ… |

Gaps:

| # | Key | Example | Should map to | Extends |
|---|---|---|---|---|
| O1 | `lmnr.span.type` = `LLM` on the wrapper span `llm.<model>`: only `gen_ai.usage.cost`, `gen_ai.usage.cache_*_tokens` (0), no model, no tokens | cost `2.25e-05`, `cost_source=reported`, next to the child's computed `2.25e-05` | kind `chain` when an `LLM`/`CACHED` span has neither a request or response model nor an input or output token count | § Span kind, `lmnr.span.type` row (R11) |
| O2 | `gen_ai.usage.cost` on the wrapper (a float) | | already read as reported cost (D109); stops mattering once O1 makes the wrapper `chain` | none |
| O3 | `gen_ai.system` = `openai` on `litellm.completion` | | provider `openai` (works); `lmnr.span.instrumentation_scope.name` = `litellm` | none |
| O4 | `lmnr.association.properties.session_id` | one per conversation | `session_id` (works) | none |
| O5 | `lmnr.association.properties.metadata.tool_call_id` | `call_1` | `metadata.tool_call_id` (works) | none |
| O6 | `llm.usage.total_tokens` on the child | `99` | `total_tokens` (works; also makes the span list `openinference` among `conventions`) | none |

The conventions list of the child is `["genai", "openinference", "laminar"]`. Tool spans are `lmnr.span.type=TOOL` → tool, with `lmnr.span.input` and `lmnr.span.output` as JSON; `DEFAULT` spans (`conversation.send_message`, `conversation.run`, `agent.step`) become `chain`. Message conversion works (system, user, tool call).

Nondeterministic: the conversation id (a UUID4, also the `session_id`), `lmnr.span.ids_path` (arrays of UUID-shaped ids), `lmnr.span.input` of `agent.step` holds a Python repr with a memory address (`<…LocalConversation object at 0x…>`), the system prompt (15 KB, stable per version but changes between versions). **Phones home: yes.** LiteLLM fetches its model-cost map from `raw.githubusercontent.com` at start (`LITELLM_LOCAL_MODEL_COST_MAP=True` stops it; with it set and DNS blocked there was no other external attempt). Laminar itself sent nothing outside the configured endpoint without the project key. OpenHands' wall-clock durations are small and stable; no timestamps in payloads.

---

## Capture project (`tools/capture/py/pyproject.toml`) conflicts

The capture project pins `openai==3.45.0`, `opentelemetry-sdk==1.45.2`, `anthropic==2.4.0`, `google-genai==2.34.0`, `opentelemetry-instrumentation-google-genai==1.1b1`, `opentelemetry-util-genai==1.1b0`, `lmnr==0.8.8`, and already overrides `jiter>=0.16` and `openai==3.45.0`. Each M4 package was added to a copy and locked (all at the 7-day cutoff):

| Package | Result | Detail |
|---|---|---|
| `pydantic-ai-slim[openai]==2.68.0` | locks, runs | none |
| `strands-agents[openai]==1.65.0` | locks, runs | none |
| `ag2[openai,tracing]==1.3.2` | locks, runs | none |
| `autogen-agentchat`, `autogen-ext[openai]` 0.7.5, `openinference-instrumentation-autogen-agentchat` 0.1.29 | locks, runs | none |
| `semantic-kernel` | locks, runs, but **unpinned it resolved 1.44.0**; pin `==1.47.0` | caps `mcp<2` |
| `agent-framework-core` 1.26.0 + `agent-framework-openai` | locks, runs | none |
| `agent-framework` (meta) | **conflict**; unpinned it resolves to `1.0.0b251120` (a November 2025 beta) | 1.26.0 needs `opentelemetry-util-genai==0.3b0` (through `microsoft-opentelemetry`); the capture pins 1.1b0. Use the two split packages |
| `mirascope[openai,anthropic,google,ops]==2.8.2` | **conflict**; unpinned it resolves to `mirascope 0.1.3` | `[anthropic]` caps `anthropic<1.0` (capture 2.4.0), `[google]` caps `google-genai<2` (2.34.0), `[ops]` caps `opentelemetry-instrumentation-google-genai<1` (1.1b1), and the package caps `openai<3`. With `override-dependencies` for `anthropic==2.4.0`, `google-genai==2.34.0`, `opentelemetry-instrumentation-google-genai==1.1b1` (plus the existing `openai`) it locks and **runs** (scenario re-run in the capture-like venv) |
| `openhands-sdk==1.62.0` | **conflict**; unpinned it resolves to `1.28.0` | needs `lmnr>=0.7.60,<0.8` (capture 0.8.8), `litellm<1.106` which caps `openai<3` (covered by the override), and `fastmcp>=4.0.3` → `mcp>=2.0`, which conflicts with `semantic-kernel>=1.47` (`mcp<2`). An `lmnr==0.8.8` override makes it run (this spike ran 1.62.0 with it); the `mcp` conflict with Semantic Kernel cannot be overridden away without breaking one of them, so OpenHands cannot share the lock |

Without OpenHands, all seven others lock together with the capture project (323 packages; `mcp 1.30.0`, `opentelemetry-util-genai 1.1b0`; the overrides in `conflicts/all2/pyproject.toml` are the existing `jiter>=0.16` and `openai==3.45.0` plus `opentelemetry-instrumentation-google-genai==1.1b1`, `anthropic==2.4.0`, `google-genai==2.34.0` for Mirascope, and `lmnr==0.8.8`, which only OpenHands needs) and the seven scenarios ran inside that environment with the same spans. Plan options for OpenHands: its own capture project (a second `pyproject.toml`; the harness has one per language today), a hand-written fixture from this spike's dump (tier "hand-written", D-IN-8), or setup-only. The OpenHands scenario is also the only one that writes the system prompt into every `gen_ai.input.messages` (15 KB per call).

## Proposed normalizer rules

Each rule names the spec 02 row, the key and the value, and the evidence. "Captured" means seen in this spike's run; "source" means read in the framework's source and not yet captured (D-IN-5 says a capture backs every rule: a source-only row needs a capture in the plan or is dropped).

**R1. Agent-span I/O (Pydantic AI)** — § Input/output, GenAI row. On a span whose `gen_ai.operation.name` is `invoke_agent` and whose instrumentation scope name is `pydantic-ai`: `pydantic_ai.all_messages` (a JSON string; canonical messages) → `input` (the whole conversation, including the final answer; the alternative "everything before the last assistant message" needs a split rule and was not chosen); `final_result` (a string, or JSON for structured output) → `output` (rule 10). Both attributes are payload attributes (rule 2). Scope-gated because the keys are not standard; precedence after `gen_ai.input.messages`/`gen_ai.output.messages`, which `invoke_agent` spans of Pydantic AI do not carry. Captured.

**R2. Kinds** — § Span kind.
- `gen_ai.operation.name` row: add `execute_event_loop_cycle`, `execute_structured_output`, `decision` → `chain` (Strands; the first captured, the other two source). Add `text_completions` → `llm` (Semantic Kernel; captured; today `llm` through the model fallback, so this only makes it explicit). `await_human_input` needs no row: it is `span` by default.
- No operation attribute, so a new fallback row **before** the model fallback, scope-gated to instrumentation scope `agent_framework` (the Microsoft Agent Framework scope; the key names are specific, the gate is for safety): `executor.id` present → `chain`; `edge_group.id` present → `chain`; `workflow.id` present without `gen_ai.operation.name` (that is `workflow.build`) → `chain`. `message.send` stays `span`. All captured. (D-IN-5 says "operation names"; these spans have none, so D-IN-5's wording changes to "keys".)
- `lmnr.span.type` row: `LLM` and `CACHED` → `llm` **only when** the span has `gen_ai.request.model`, `gen_ai.response.model`, `llm.model_name` or an input/output token count; otherwise `chain` (OpenHands wrapper, finding 4). Captured. Checked: every `LLM` span in `fixtures/otlp/laminar/` (five cases) has a request model and token counts, so no existing fixture changes.

**R3. Provider** — § Model, provider, usage, `provider` row, and rule 12.
- Framework-name table: add `strands-agents` and `microsoft.agent_framework` to `autogen`, `gcp.vertex.agent`, `spring_ai`. The skip applies to **`gen_ai.provider.name` and `gen_ai.system` alike** (today only `gen_ai.system`). Conflict to resolve: a framework that wrote a framework name into `gen_ai.provider.name` and a real provider into `gen_ai.system` would still resolve (the next source decides); no such case seen. Captured for both names.
- Alias table, exact: `googleai` → `gcp.gemini` (Semantic Kernel, source), `google.vertex.ai` → `gcp.vertex_ai` (Agent Framework, source), `openai:completions`, `openai:responses` → `openai` (Mirascope; `openai:completions` captured), `anthropic-beta` → `anthropic` (Mirascope, source). Each alias is a fixed point check for D-IN-14's test.

**R4. TTFT** — § Model, provider, usage, `ttft_ns` row. `gen_ai.server.time_to_first_token`: **milliseconds** (×1e6) on spans whose instrumentation scope name starts with `strands`; unread elsewhere (no other emitter seen). This replaces D-IN-5's "(seconds)". Captured. (`gen_ai.server.request.duration`, which the Strands source also writes in milliseconds when it has a latency, was absent from this capture and is not read.)

**R5. Details events** — § Input/output, GenAI row. The fallback to `gen_ai.client.inference.operation.details` reads each key (`gen_ai.input.messages`, `gen_ai.output.messages`, `gen_ai.system_instructions`, `gen_ai.tool.definitions`) from **the first event of that name that carries the key**, not from the first event of that name. Captured (Strands latest mode: three events on a `chat` span). In the same row a span that has several events with the same key (Strands' event-loop spans have two `gen_ai.input.messages` events) uses the first.

**R6. `tool_call_response` part alias** — message conversion (GenAI reader, before `parse_messages`): in a `tool_call_response` part that has `result` and no `response`, rename `result` to `response`. Captured (Pydantic AI). No `cetra-core` change: the rename is in `cetra-otel`'s GenAI reader; the part's other fields (`name`) stay as extras.

**R7. Reasoning key** — § Model, provider, usage, `reasoning_tokens` row: add `gen_ai.usage.thinking_tokens` (last). Semantics (output includes reasoning) follows the GenAI row; AG2 does not document it (source only; needs a capture or the row is dropped).

**R8. OpenInference `function` role** — § Input/output, OpenInference row. A flattened message with `llm.input_messages.N.message.role` = `function` and no `message.content`, but with `llm.input_messages.N.function.M` JSON objects `{"name","content","call_id","is_error"}`, becomes one `tool` message with one `tool_call_response` part per object: `{id: call_id, response: content}` (`is_error` and `name` as extras). Captured (AutoGen's instrumentor). Gate: none needed, the key family is specific.

**R9. Model-name prefix** — § Model, provider, usage, `request_model` and `response_model` rows (and § Cost computation step 2's model). When `gen_ai.provider.name` (raw, lowercased, before the alias table) is `p` and the model starts with `p/`, the model is stored without that prefix. Captured (Mirascope `openai/gpt-4o-mini`). Not applied when the prefix differs from the provider (`openai/gpt-oss-120b` on `groq`, OpenRouter's `vendor/model`), so D-IN-13/D-IN-14 are untouched. The alternative, a rule gated to scope `mirascope.llm`, is narrower and was not chosen only because `google/` and `openai/` are the same shape elsewhere; flagged for the user.

**R10. Mirascope** — § Input/output and § Context fields. On spans of scope `mirascope.ops`: `mirascope.trace.arg_values` → `input` (json), `mirascope.trace.output` → `output` (text or json, rule 10); `mirascope.ops.session.id` → `session_id` on **every** Mirascope span (any scope), last in the `session_id` list. The `mirascope.response.*` attributes are never read (they repeat usage that the `chat` span already carries, finding of R-e). Captured.

**R11. Strands legacy events** — open (question 4). Either no rule (the page requires `gen_ai_latest_experimental,gen_ai_span_attributes_only`, which works today) or a Bedrock-Converse block converter on the legacy event path (`text` → text part, `toolUse` → `tool_call`, `toolResult` → `tool_call_response`).

**Conflicts between frameworks and with decisions**

- D-IN-5 vs AG2: `record_usage` → `llm` is withdrawn (double count). D-IN-5 vs Strands: seconds → milliseconds for `gen_ai.server.time_to_first_token`. D-IN-5 vs Agent Framework: operation names → attribute keys. D-IN-5's "Mastra's `mastra.completion_start_time`" is for M4's TypeScript spike, untouched here.
- R3's skip on `gen_ai.provider.name` and R9's prefix strip both read `gen_ai.provider.name`; R9 compares against the raw value, so a framework name in the table (skipped) never strips a prefix.
- R2's Laminar change touches no existing fixture (checked: all five Laminar cases have a model and tokens on their `LLM` spans).
- R5 and R1 do not interact: R1 is scope-gated to Pydantic AI's `invoke_agent`; R5 only widens the event lookup.
- R10's session rule applies on any scope; Mirascope is the only emitter of `mirascope.ops.session.id`.

## Open questions for the user

1. **OpenHands in capture** (finding 11): a second capture project (own lock, the `mcp` conflict), a hand-written fixture from this dump, or setup only? Recommendation: hand-written fixture (D-IN-8 tier), page tells readers to set `LITELLM_LOCAL_MODEL_COST_MAP=True` and never `LMNR_PROJECT_API_KEY`.
2. **AutoGen route** (finding 3): the page enables the AutoGen instrumentor only (recommended: no double count, no change to the normalizer), or both instrumentors with a rule that turns the AutoGen instrumentor's `*ChatCompletionClient.create` spans into `chain` (needs a scope-gated name rule; the OpenAI instrumentor then provides provider, host and finish reasons, but every non-OpenAI client needs its own instrumentor).
3. **Semantic Kernel content** (finding 5): accept "no content on `chat` spans" and say so in Limits (recommended for M4), or schedule a separate item to ingest GenAI log records and attach them to their span by trace and span id (amends D-G-40).
4. **Strands default content** (finding 6): require setting (c) in the page and fix only R5 (recommended), or also write the Converse-block converter (R11) so a default Strands setup shows content.
5. **Mirascope `@ops.trace` spans**: `span` today. Make them `chain` when `mirascope.type` is `trace` or `response_resume` (one more scope-gated row), or leave them?
6. **Pydantic AI agent-span input** (R1): the whole `all_messages` (recommended, simplest), or only the messages before the final answer.

## Spec 02 rows to change (summary for the plan)

§ Span kind (`gen_ai.operation.name` list, a new fallback row for Agent Framework keys, the `lmnr.span.type` row); § Input/output (GenAI row: details events, Pydantic AI and Mirascope keys, the `tool_call_response` alias; OpenInference row: `function` role); § Model, provider, usage (`provider` row: both keys skip the framework-name table; `request_model`/`response_model`: provider prefix; `ttft_ns`; `reasoning_tokens`); § Context fields (`session_id`); rule 12 (aliases, framework-name table). Spec 10 § Python instrumentors: `autogen-agentchat` registry row installs the AutoGen instrumentor only.
