# M6 spike (part A): apps, no-code, voice, browser agents, sandboxes and the appendix items

Run 2026-11-24 (track IN, M6), worktree `integrations` at `7f7d3fd`. Python 3.13 (uv 0.13.6, `exclude-newer = 2026-11-17T00:00:00Z`: the 7-day rule at the run date), Node v26.13.0 (npm with `--before=2026-11-17T00:00:00Z`). Scratch in
`/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m6a/`:

- `shared/common.py` (the M4 fake OpenAI server, extended: `FAKE_BIND`/`FAKE_HOST`, and a **chunked-body** OTLP sink, see Finding 9), `shared/h.py` (dump of the received protobuf, then the real `cetra_otel::normalize`), `hook/sitecustomize.py` (blocks and logs every non-loopback DNS lookup), `norm/` (path dependency on this worktree's `cetra-otel`; "cetra today" below is its real output with the built-in price table, not a reading of the spec).
- One uv project per item: `pf` (Prompt Flow), `lk` (LiveKit), `pc` (Pipecat), `bu` (Browser Use 0.13 + OpenLIT), `bul` (Browser Use 1.0 + Laminar / plain OTel), `lf` (Langflow server), `n8n/` (npm), `sh/` (Stagehand).
- Fake model for every run: turn 1 = tool call `get_weather({"city":"Paris"})` (82/17 tokens) when `tools` is sent, turn 2 = text "It is 18C and sunny in Paris." (131/12), model answered `gpt-4o-mini-2024-07-18`. Real Chromium 1254 (Playwright cache) for the browser agents. Every telemetry and vendor endpoint was pointed at a loopback receiver or turned off.
- Items researched from vendor docs and source only (marked "docs, not run"): Kernel, Twilio Agent Connect, Docker and Vercel sandboxes, Daytona, Gradio, Langdock, LobeChat, Ragflow, Scalekit, Bubble, Trieve. Two Sonnet research passes (web) produced those; I did not re-read their pages.

**One deviation from the brief, to be honest about:** the Stagehand run (section 7) made one real HTTPS call to OpenAI's API with the fake key `sk-fake` and the text of a sample prompt (a local test page's accessibility tree and "find the sign in button"). Stagehand v4 runs the model call inside a Chrome extension service worker, which has no base-URL option, so my loopback fake could not intercept it. The answer was "Incorrect API key"; no real key, no user data. I did not repeat it. The Stagehand case is therefore setup-only (a fake-model capture is not possible), and nothing else was sent anywhere.

## Verdicts

| Item | Version pinned (date) / latest at run | Emits | Verdict |
|---|---|---|---|
| n8n | 2.48.4 (2026-11-1x) / 2.48.8 (2026-11-23) | native OTLP/HTTP protobuf, `N8N_OTEL_*`; `workflow.execute` and `node.execute` (all nodes share the one name) with `n8n.*`; LLM nodes carry `gen_ai.*` and tokens, **no content** | **captured case** (D-IN-5 scoped rules by scope `n8n-workflow`) |
| Langflow | 1.13.2 (2026-11-16) / 1.13.3 (2026-11-17) | Traceloop SDK (`TRACELOOP_BASE_URL`, `TRACELOOP_API_KEY`): component spans (`langflow` scope, `inputs`/`outputs` JSON) plus OpenLLMetry's GenAI `ChatOpenAI.chat` span in a **separate trace** | **captured case** (scoped rules by scope `langflow`) |
| Prompt Flow | 1.18.5 (2026-05-01) / same | `setup_exporter_from_environ()` + `OTEL_EXPORTER_OTLP_*`; own keys (`span_type`, `inputs`, `output`, `llm.usage.*`); endpoint is **not** suffixed with `/v1/traces` | **captured case** (scoped rules by scope `promptflow`) |
| LiveKit Agents | 1.8.12 (2026-11-1x) / 1.8.13 (2026-11-18) | GenAI + `lk.*`; content in `lk.pii.*` and in `gen_ai.*` on `llm_request`; 3 llm-shaped spans per model call | **captured case** (text-only session, no LiveKit server) |
| Pipecat | 1.16.0 (2026-11-1x) / 1.17.0 (2026-11-22) | GenAI + plain `input`/`output`; `tts`/`stt` spans typed `llm` today | **captured case** (pipeline with the OpenAI service, no transport) |
| Browser Use | 1.0.6 (pinned `uv` result) and 0.13.14 / 1.0.8 (2026-11-22) | 1.0.x is built on Pydantic AI: native `pydantic-ai` GenAI spans (works today, no rule); 0.x gets OpenLIT's browser-use instrumentation | **captured case** (1.0.x + plain OTel or Laminar); 0.x/OpenLIT noted on the page and fixed by one rule |
| Stagehand | 4.4.0 (2026-11-04) / same | v4 `telemetry: { traces: { endpoint, headers } }`; OTLP/JSON from a Chrome extension; JSON-RPC and log spans, no LLM span seen | **setup-only page** (not capturable offline), plus one discard rule once a real capture exists |
| Kernel (browser) | `kernel` 0.154.0 / `@onkernel/sdk` 0.154.0 (2026-11-20) | OTLP **logs** only (`/v1/logs`), browser events, from Kernel's cloud | **not supported** (logs only, not spans; endpoint must be public) |
| Twilio Agent Connect | `twilio-agent-connect` 2.6.0 (2026-11-17) | nothing (Opik decorator only) | **not supported** |
| Docker sandboxes | `sbx` v0.54.0 (2026-11-16) | nothing | **not supported** |
| Vercel Sandbox | `@vercel/sandbox` 4.1.0 (2026-11-20), `vercel-sandbox` 1.2.0 | nothing | **not supported** |
| Daytona | `daytona` 2.1.0 (2026-11-20); npm `@daytona/sdk` 2.1.0 (`@daytonaio/sdk` 0.232.0 is deprecated) | SDK spans, `DAYTONA_OTEL_ENABLED`, no LLM | **not supported** as an LLM source (infrastructure spans only; "not an LLM library") |
| Gradio | 6.32.0 (2026-11-12) | none of its own | **merged into** the instrumentation pages: setup-only page that points at the OpenInference, OpenLLMetry or OpenLIT instrumentor of the client library |
| Langdock | hosted | Langfuse only | **not supported** |
| LobeChat | v2.3.5 (2026-11-22) | native OTLP, `ENABLE_TELEMETRY=1`, `gen_ai.*` keys defined (content unverified) | **setup-only page**, "unverified" (Docker or Node server, not run) |
| Ragflow | v1.0.0 (2026-11-06) | Langfuse only | **not supported** |
| Scalekit | `scalekit-sdk-python` 2.25.0 | none (app-side spans only) | **not supported** |
| Bubble | hosted | none | **not supported** |
| Trieve | last release 2025-03-07 | none; unmaintained | **not supported** (stale) |

Cut-off effect: the 7-day rule (uploads on or before 2026-11-17) put these below the latest: livekit-agents 1.8.12 (1.8.13 on 11-18), pipecat 1.16.0 (1.17.0 on 11-22), browser-use 0.13.14 with openlit, 1.0.6 alone (1.0.8 on 11-22), langflow 1.13.2 (1.13.3 on 11-17, the cutoff day), n8n 2.48.4 (2.48.8 on 11-23). The plans pin these and recapture on the day.

## Findings that change the design

1. **LiveKit's `gen_ai.provider.name` is the endpoint host (`api.openai.com`), not `openai`.** In `livekit-plugins-openai` it is `self._client._base_url.netloc` (`LLM.provider`). The normalizer takes the value as is: provider `api.openai.com`, cost `none`, for every real OpenAI run. D-IN-2's endpoint rule does not fire (it only rewrites an empty or `openai` provider). The M5 fix needs a sibling: on scope `livekit-agents`, a provider that is a host is treated as the endpoint (§ Proposed rules, R-LK-1). Verified by rewriting the loopback host in my capture to `api.openai.com`: `cost_source=none`.
2. **Voice STT and TTS spans become `llm` today.** Pipecat writes `gen_ai.operation.name` `tts`/`stt` (and `setup`, `model_turn`, `response`, `transcription`, `tool_call` for Gemini Live and OpenAI Realtime) with a `gen_ai.request.model`; the kind table does not know those operations, so the fallback "has a request model" makes them `llm` with no tokens. Checked with a synthetic span: `tts` → `llm`, `stt` → `llm`.
3. **LiveKit emits three llm-shaped spans per model call:** `llm_node` (model, no tokens), `llm_request` (the real call: tokens, `gen_ai.input.messages`, `gen_ai.output.messages`) and `llm_request_run` (model only). Today all three are `llm`; only `llm_request` has tokens, so cost is right but the llm count is tripled.
4. **Prompt Flow posts to exactly the URL it is given.** `OTLPSpanExporter(endpoint=...)` is built by hand, so `OTEL_EXPORTER_OTLP_ENDPOINT=http://host` posts to `/`, not `/v1/traces`. The page must tell users to give the full `https://<cetra>/v1/traces`. (`OTEL_EXPORTER_OTLP_HEADERS` works: `authorization: Bearer ...` arrived.)
5. **Langflow's model call is not in the flow's trace.** The component spans (`langflow` scope, root `<flow name> - <uuid>`) are one trace; the Traceloop `ChatOpenAI.chat` span is another trace (different trace id, no parent) in the same run. The LLM span itself is complete (GenAI keys, tokens, cost). Langflow's own `OTEL_EXPORTER_OTLP_*` support (`lfx/observability.py`) exports application telemetry only (HTTP and flow spans, scope `langflow.observability`, allowlisted), no LLM content: it is **not** the path for cetra.
6. **n8n names every node span `node.execute`.** The node is only in `n8n.node.name`; a trace view shows a list of identical names. The Agent and Chat Model nodes carry `gen_ai.*` and tokens but **no prompt or completion content**, on all three versions of the docs I read.
7. **OpenLIT's Browser Use spans misuse `gen_ai.operation.name`.** The agent span has `gen_ai.operation.name=run` and the step span `step`; the real GenAI operation is in the non-standard `gen_ai.operation.type` (`invoke_agent`, `execute_task`). Both become `llm` today (a model is present), with provider `browser_use`. OpenLIT also wrote `gen_ai.usage.cost = 0` (it could not fetch its pricing file offline); cetra stores that as `cost_source=reported`, `cost_total=0`, which hides its own computed price. A reported cost of 0 on a span with tokens should read as "no report" (R-OL-2).
8. **Browser Use 1.0 is a different product.** 1.0.x (1.0.6 resolved) is rewritten on Pydantic AI: its spans are Pydantic AI's native GenAI spans (`invoke_agent agent`, `chat <model>`, `execute_tool ...`), they normalize fully today (agent, llm with cost, tool, input and output previews, `session_id` from `gen_ai.conversation.id`), through Laminar (`lmnr`, extra `lmnr.*` keys) or a plain OTel exporter with `pydantic_ai.Agent.instrument_all(True)`. The 0.x line (0.13.14) has its own `@observe` hooks and the OpenLIT path of finding 7. The design's "OpenLIT or Laminar" route is the 0.x route.
9. **Capture tooling gap: chunked OTLP bodies.** n8n (Node `fetch`) sends the OTLP request with `Transfer-Encoding: chunked` and no `Content-Length`. The M4 sink read 0 bytes and showed an empty trace (I lost twenty minutes). `tools/capture/src/receiver.rs` must decode chunked request bodies (check that `axum`/`hyper` already do; it is only my Python sink that did not). Also `/v1/metrics` is acknowledged and dropped by the server (`routes/otlp.rs`), which Traceloop (Langflow) needs: it posts there too.
10. **Stagehand v4's spans come out of a Chrome extension as OTLP/JSON**, and 42 of the 68 spans of one `goto` plus one `observe` are `stagehand.span.type=log` spans (`CDP call`, `CDP message`, `CDP response`), each with a payload in `stagehand.log.data`. They are noise in a trace view. See section 7.

## 1. n8n (run for real, `n8n start` with a webhook workflow)

n8n **2.48.4** (the newest upload on or before 2026-11-17; 2.48.8 is 2026-11-23). `npm install n8n --before=2026-11-17T00:00:00Z`: about 1,100 packages, 6 minutes, Node 26. Docs: OpenTelemetry arrived in 2.15 (2026-03-30, Preview); the docs page is `docs.n8n.io/deploy/host-n8n/configure-n8n/basic-configuration/use-environment-variables/opentelemetry`.

```bash
N8N_OTEL_ENABLED=true
N8N_OTEL_EXPORTER_OTLP_ENDPOINT=https://<cetra-host>            # base; "/v1/traces" appended (N8N_OTEL_EXPORTER_OTLP_TRACING_PATH)
N8N_OTEL_EXPORTER_OTLP_HEADERS="authorization=Bearer sk-cetra-..."   # comma-separated key=value
N8N_OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf                    # default; grpc exists from 2.39
N8N_OTEL_TRACES_PRODUCTION_ONLY=false                            # default true: manual editor runs are not exported
N8N_DIAGNOSTICS_ENABLED=false                                    # n8n's own telemetry, off in the run
```

`n8n execute --id=...` (the CLI runner) exports **nothing**: OTel starts only in `n8n start`. The run used a Webhook, AI Agent (typeVersion 2.2) and OpenAI Chat Model (`lmChatOpenAi` 1.2) workflow, credential `openAiApi` with `url` on the fake. `Authorization: Bearer ...` arrived verbatim; body `application/x-protobuf`, chunked, uncompressed; resource `service.name=n8n`, `service.version=2.48.4`, `n8n.instance.id`, `n8n.instance.role=main`, `host.*`, `process.*`.

Spans (4; scope `n8n-workflow`; all INTERNAL):

```
#0 workflow.execute   n8n.workflow.id/.name/.node_count/.version_id, n8n.execution.id/.mode(webhook)/.status/.is_retry, n8n.project.id
#1 node.execute       Webhook      n8n.node.id/.name/.type/.type_version, n8n.node.items.input/.output, n8n.workflow.*, n8n.execution.id
#2 node.execute       AI Agent     + gen_ai.operation.name=invoke_agent, gen_ai.agent.id=<node id>, gen_ai.agent.name=<node name>, gen_ai.provider.name=openai,
                                     gen_ai.request.model, gen_ai.response.model, gen_ai.usage.input_tokens=131, gen_ai.usage.output_tokens=12
#3 node.execute       OpenAI Chat Model  + gen_ai.operation.name=chat, provider, request/response model, usage 131/12
```

Cetra today: `workflow.execute` and the Webhook span `span`; the Agent node `agent` (provider `openai`, tokens not counted, no cost); the Chat Model node `llm` with provider `openai`, model, 131/12, cost computed (2.685e-05). Names are all `node.execute`. No input or output anywhere. The agent tokens repeat the child chat node's tokens (an `agent`-kind span does not carry cost, so no double count, but the trace view shows tokens twice).

Not run: the agent runtime of the "agent tracing" docs (`<agent>.generate` spans, `gen_ai.prompt`, `execute_tool <name>`; `N8N_AGENTS_TRACING_*`), which the docs say does not cover the AI Agent workflow node, and tool nodes (a tool-call turn). The span reference lists `gen_ai.prompt` and tool arguments and results for that runtime; if it is in the page's scope the recapture must show it.

**Rules (D-IN-5, scope `n8n-workflow`):** (R-N8N-1) span name displayed as `n8n.node.name` when the name is `node.execute` (and `n8n.workflow.name` for `workflow.execute`); (R-N8N-2) `workflow.execute` → `chain`; (R-N8N-3) `node.execute` without `gen_ai.operation.name` → `span`. The design's note "M6 (`n8n`) widens it to `[a-z0-9_]+`" is not needed by anything I saw: nothing in n8n's spans or gen_ai keys carries a framework name; if the widening is for `gen_ai.provider.name`, say so in the plan.

**Verdict: captured case.** Sketch: `n8n@2.48.x` started by the runner (the capture harness needs `N8N_USER_FOLDER`, `N8N_DIAGNOSTICS_ENABLED=false`, `N8N_VERSION_NOTIFICATIONS_ENABLED=false`, `N8N_TEMPLATES_ENABLED=false`, `N8N_AI_ENABLED=false`, `N8N_OTEL_*`), a workflow imported with `n8n import:workflow`, a credential imported with `import:credentials`, `n8n update:workflow --active=true`, then one `POST /webhook/<path>`. Expected kinds: `chain`, `span`, `agent`, `llm`. Cost: node ~1,100 packages, heavy for CI (a pinned tarball cache like M5's binaries); startup is about 40 s.

## 2. Langflow (run for real: `langflow run`, Basic Prompting)

`langflow` **1.13.2** (the newest upload on or before 2026-11-17; 1.13.3 is 2026-11-17). Install is fast with a warm uv cache; start about 45 s. Environment of the run:

```bash
TRACELOOP_BASE_URL=https://<cetra-host>      # "/v1/traces" appended; the Traceloop SDK also posts "/v1/metrics"
TRACELOOP_API_KEY=sk-cetra-...               # sent as "Authorization: Bearer <key>"; must be non-empty or the tracer stays off
LANGFLOW_NATIVE_TRACING=false                # the run only; native tracing writes to Langflow's own DB
LANGFLOW_DO_NOT_TRACK=true  DO_NOT_TRACK=true  TRACELOOP_TELEMETRY=false
```

The flow's model call used `OPENAI_BASE_URL` (honoured by the unified Language Model component) with `LANGFLOW_PROVIDER_CREDENTIAL_ALLOWED_HOSTS=http://127.0.0.2` (Langflow refuses to send a server-environment key to a non-default endpoint) and a fake that binds `127.0.0.2` (Langflow rewrites `localhost` and `127.0.0.1` to the container host when it thinks it runs in a container, which this machine does). The fake model selection needed `model` set in the starter flow (the `ModelInput` value, with `metadata.model_class=ChatOpenAI`). The server tried `raw.githubusercontent.com`, `models.dev`, `pypi.org`, `host.docker.internal`, `host.containers.internal`: all blocked, all harmless to the run. The run printed `Traceloop exporting traces to http://127.0.0.1:... authenticating with bearer token`.

Received (`/v1/traces` and `/v1/metrics`), resource `service.name=Langflow`:

```
trace A (flow): `spike - <flow id>` scope=langflow  attrs flow_id, flow_name, workflow_id, workflow_name, start_time, end_time, time_elapsed, outputs (JSON string)
  `Chat Input (ChatInput-x)`       trace_type=chain   inputs={"input_value": ..}   outputs={"message": ..}   vertex_id, trace_id, trace_name, sender, session_id="" ...
  `Prompt Template (Prompt-x)`     trace_type=prompt
  `Language Model (LanguageModelComponent-x)`  trace_type=llm  inputs (JSON, 592 B, the whole component config)  outputs={"text_output": ..}
  `Chat Output (ChatOutput-x)`     trace_type=chain
trace B (model call, no parent): `ChatOpenAI.chat` scope=opentelemetry.instrumentation.langchain@0.63.1 kind=CLIENT
  gen_ai.operation.name=chat, gen_ai.provider.name=openai, gen_ai.request.model, gen_ai.response.model, gen_ai.usage.input_tokens/output_tokens/total_tokens,
  gen_ai.input.messages, gen_ai.output.messages, gen_ai.system_instructions, traceloop.association.properties.*  (ls_provider, ls_model_name, lc_versions ...)
```

Langflow's `trace_type` values in source: `chain`, `prompt`, `llm`, `tool`, `agent`, `retriever`, `embedding`, `chat_memory`, `document_transformer`, `text_splitter`.

Cetra today: the `ChatOpenAI.chat` span is complete (`llm`, provider `openai`, 131/12, cost computed, conventions `genai`, `openllmetry`, messages). Every component span and the flow root are `span` with no input or output (their keys are `inputs`, `outputs`, plain). The component span `trace_type=llm` would make a second `llm` if it were read by type.

**Rules (D-IN-5, scope `langflow`):** (R-LF-1) the root (`flow_id` present) → `chain`; component spans: `trace_type` `agent` → `agent`, `tool` → `tool`, `retriever` → `retriever`, `embedding` → `embedding` (the component span, not a model call), everything else including `llm` → `chain` (the real model call is trace B; reading `llm` as `llm` double counts the llm count); (R-LF-2) input from `inputs`, output from `outputs` on scope `langflow` (JSON strings, not messages: stored as JSON). `session_id` is empty in these spans; nothing to read.

**Verdict: captured case**, with the page saying "the model call is a separate trace" (a limit; it is Langflow's context propagation, not ours). Sketch: `langflow==1.13.x` (about 600 packages), start the server, create the flow from the Basic Prompting starter with the model value set, run it by `POST /api/v1/run/<id>` with an API key; `OPENAI_BASE_URL` to the mock (the mock needs a non-`127.0.0.1` loopback name, or `LANGFLOW_PROVIDER_CREDENTIAL_ALLOWED_HOSTS` plus a container-detection workaround; the D-IN-3 host routing does not help because the SDK is inside the server). Heavy but runs offline.

## 3. Prompt Flow (run for real: `promptflow-tracing` + `promptflow-devkit` 1.18.5)

`promptflow-tracing` **1.18.5** (2026-05-01, also the latest). `@trace`-decorated functions and an `openai.OpenAI` call (promptflow injects the OpenAI client). The documented export path (devkit installed) is `setup_exporter_from_environ()` with the standard environment:

```bash
OTEL_EXPORTER_OTLP_ENDPOINT=https://<cetra-host>/v1/traces     # NOT suffixed: it is posted to as given (finding 4)
OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer sk-cetra-..."
```

Posted `application/x-protobuf` with the header intact (`OTel-OTLP-Exporter-Python/1.38.0`); resource `service.name=promptflow`; scope `promptflow` (no version). The `start_trace()` path of the docs (with devkit) sends to the local `pfs` service; the OTLP path is the one above. `promptflow-tracing` alone installs no exporter.

```
`flow` (the @trace function)       span_type=Function  framework=promptflow  function  inputs (JSON, pretty-printed) output (JSON)  __computed__.cumulative_token_count.{completion,prompt,total}
`openai_chat`                       span_type=LLM  llm.response.model  llm.usage.{prompt,completion,total}_tokens  llm.generated_message  inputs  output (the whole response, JSON)  EVENTs promptflow.function.inputs/output, promptflow.llm.generated_message
`weather` (a @trace tool)           span_type=Function
```

`TraceType` in source: `LLM`, `Function`, `LangChain`, `Flow`, `Embedding`, `Retrieval`. Python warns about `None` values (`llm.usage.completion_tokens_details`: invalid attribute type): harmless.

Cetra today: every span `span`; the LLM spans list convention `openinference` (the `llm.` prefix) but read nothing: no model (the request model is only inside the `inputs` JSON, `llm.response.model` is not a read key), no tokens (`llm.usage.*` are not `llm.token_count.*`), no input or output.

**Rules (D-IN-5, scope `promptflow`):** (R-PF-1) kind from `span_type`: `LLM` → `llm`, `Embedding` → `embedding`, `Retrieval` → `retriever`, `Flow`/`LangChain` → `chain`, `Function` → `span` (a tool in Prompt Flow's own language is a function; no `tool` mapping, because a plain `@trace` function is anything); (R-PF-2) response model from `llm.response.model`, request model from `inputs.model`, tokens from `llm.usage.{prompt,completion,total}_tokens`; (R-PF-3) input from `inputs`, output from `output` (JSON text; for `LLM` spans the OpenAI request and response objects, which cetra's OpenAI-shaped reader may already parse, to be checked in the plan). Azure OpenAI through Prompt Flow gives the deployment in `inputs.model`: price by the response model.

**Verdict: captured case.** Sketch: `promptflow-tracing==1.18.5`, `promptflow-devkit==1.18.5`, `openai`, `opentelemetry-exporter-otlp-proto-http`; the program defines two `@trace` functions around two chat completions (tool call, then text); client `base_url` on the mock; `setup_exporter_from_environ()` with `OTEL_EXPORTER_OTLP_ENDPOINT` set to the runner's traces URL. Expected kinds: `span` (flow), `llm` x2, `span` (tool). Hand-over caveat: Prompt Flow is in maintenance (last release 2026-05).

## 4. LiveKit Agents (run for real, text mode)

`livekit-agents` **1.8.12** (2026-11-1x; 1.8.13 is 2026-11-18) with `livekit-plugins-openai` 1.8.12. No LiveKit server, no room, no audio: `AgentSession(llm=openai.LLM(base_url=<mock>))`, `session.start(Agent)`, `session.run(user_input=...)`, a `@function_tool`. Export by the documented hook:

```python
from livekit.agents.telemetry import set_tracer_provider
set_tracer_provider(provider, metadata=None, allow_pii=True)   # allow_pii=False (or LIVEKIT_TELEMETRY_ALLOW_PII=false) strips `lk.pii.*` and the GenAI content keys in-process
```

(The provider carries a `BatchSpanProcessor(OTLPSpanExporter(.../v1/traces))`; with `OTEL_EXPORTER_OTLP_*` the stock exporter reads the environment.) Resource: `service.name` from the provider. Scope `livekit-agents` (no version). All spans INTERNAL, status unset.

```
agent_session            gen_ai.operation.name=invoke_workflow, gen_ai.workflow.name=agent_session, lk.agent_label; events agent_state_changed
  session_start > start_agent_activity   gen_ai.operation.name=create_agent, gen_ai.request.model, gen_ai.provider.name=<netloc>
  agent_turn             gen_ai.operation.name=invoke_agent, gen_ai.agent.name, lk.generation_count, lk.pii.user_input, lk.pii.response.text, lk.speech_id ...
    llm_node             gen_ai.request.model, gen_ai.provider.name, lk.function_tools, lk.pii.chat_ctx (JSON), lk.pii.response.text, lk.pii.response.function_calls, lk.response.ttft (s)
      llm_request        gen_ai.operation.name=chat, gen_ai.input.messages, gen_ai.output.messages, gen_ai.system_instructions, gen_ai.tool.definitions,
                         gen_ai.usage.input_tokens/output_tokens, gen_ai.response.time_to_first_chunk, langfuse.observation.completion_start_time, lk.llm_metrics
        llm_request_run  gen_ai.request.model, lk.retry_count
    function_tool        gen_ai.operation.name=execute_tool, gen_ai.tool.name/.call.id/.call.arguments/.call.result, lk.pii.function_tool.arguments/.output
    llm_node > llm_request > llm_request_run   (second model call, after the tool)
    agent_speaking
  session_close > drain_agent_activity > on_exit
```

Other spans in the source, not produced in text mode: `user_turn`, `eou_detection`, `tts_node`, `tts_request`, `stt_request` (names from the plugins' base classes), `realtime_inference`, `job_entrypoint`/`job_dispatch`, `room_disconnect`, `amd`, `judge_evaluation`. Content is also under `lk.pii.*` (`lk.pii.chat_ctx`, `lk.pii.user_input`, `lk.pii.response.text`, `lk.pii.response.function_calls`, `lk.pii.function_tool.*`, `lk.pii.input_text` for TTS, `lk.pii.user_transcript`).

Cetra today: `agent_session` `chain` (op `invoke_workflow`), `start_agent_activity` `agent` (op `create_agent`), `agent_turn` `agent`, `function_tool` `tool` (arguments and result read from `gen_ai.tool.call.*`), `llm_request` `llm` complete (82/17 and 131/12, input preview "What is the weather in Paris?"; the tool-call turn has no output preview, the same tool-call gap as M5's finding 5 for `gen_ai.output.messages` whose tool call part carries the arguments as an object), **`llm_node` and `llm_request_run` `llm`** with no tokens (finding 3), `session_start`, `setup_toolsets`, `on_enter`, `agent_speaking`, `session_close`, `drain_agent_activity`, `on_exit` `span`. Provider is the netloc `127.0.0.1:<port>` (`api.openai.com` in real runs: finding 1). `agent_turn` and `llm_node` have no input or output (`lk.pii.*` unread). `llm_request` also carries `langfuse.observation.completion_start_time`, so its `conventions` is `genai`, `langfuse`; the key is a TTFT instant (ISO string) that cetra already reads for Langfuse; `gen_ai.response.time_to_first_chunk` (seconds) is the GenAI-shaped one, to be added to the TTFT list of D-IN-5.

**Rules (D-IN-5, scope `livekit-agents`):**
- (R-LK-1) `gen_ai.provider.name` that contains a `.` or `:` is a host: feed it to the endpoint rule (D-IN-2) instead of the provider column. Unknown host: provider left empty rather than a host string.
- (R-LK-2) kinds: `llm_node` → `chain`, `llm_request_run` → `span`, `start_agent_activity` stays `agent` only if it adds nothing: prefer `span` (the `create_agent` operation is setup, and `gen_ai.request.model` there would otherwise count). `llm_request` stays `llm`.
- (R-LK-3) content: `lk.pii.user_input` / `lk.pii.response.text` as input and output of `agent_turn`; `lk.pii.response.text` and `lk.pii.chat_ctx` as output and input of `llm_node`; `lk.pii.function_tool.arguments` / `.output` and `lk.pii.input_text` (TTS), `lk.pii.user_transcript` (user turn) as fallbacks only on spans whose GenAI content keys are absent. D-IN-5 already names `lk.pii.*`; the keys above are the verified list. `lk.pii.chat_ctx` is LiveKit's own JSON (`{"items": [...]}`) and needs a reader or is stored as JSON.
- (R-LK-4) the `lk.pii.*` content is stripped before every exporter but LiveKit Cloud's only when `allow_pii=False`; the default is `True`. The page says "leave `allow_pii` at its default to see content".

**Verdict: captured case** (text mode). Sketch: `livekit-agents==1.8.x` + `livekit-plugins-openai==1.8.x` (about 70 packages, no native audio needed, no `silero`), one `AgentSession` with an `Agent` and a `get_weather` function tool, `session.run(user_input=...)`; the OpenAI plugin's `base_url` on the mock; the OTLP exporter from the runner's environment. Expected kinds: `chain`, `span`, `agent`, `chain` (llm_node), `llm`, `span`, `tool`. A voice capture (STT, TTS, EOU spans) needs audio plugins and a LiveKit server: out of scope; the page says which spans are not captured.

## 5. Pipecat (run for real, no transport)

`pipecat-ai` **1.16.0** (2026-11-1x; 1.17.0 is 2026-11-22). `setup_tracing(service_name, exporter=OTLPSpanExporter(...))` and `PipelineTask(..., enable_tracing=True, conversation_id=..., enable_turn_tracking=True)`; pipeline `[user aggregator, OpenAILLMService(base_url=<mock>), assistant aggregator]`, one `LLMRunFrame`, a registered `get_weather` function. (The Smart Turn model loads from the wheel, offline. Python 3.11+. Many `DeprecationWarning`s: `PipelineTask` → `PipelineWorker` in 2.0.) Resource `service.name`, `service.instance.id`, `deployment.environment=development`. The exporter is user-built; the page gives the HTTP exporter with the cetra URL and headers.

```
`conversation`  scope=pipecat.turn   conversation.id, conversation.type=voice
  `turn`        scope=pipecat.turn   conversation.id, turn.number, turn.type, turn.duration_seconds, turn.was_interrupted
    `llm`       scope=pipecat        gen_ai.operation.name=chat, gen_ai.provider.name=openai, gen_ai.request.model, gen_ai.system_instructions, gen_ai.usage.input_tokens/output_tokens,
                                     input (JSON messages), output (text), tools (JSON), tool_count, stream, metrics.ttfb (s), param.*, gen_ai.request.top_k="NOT_GIVEN"
    `llm`       (second call, after the function call)
```

The tool-call turn has **no output** (no `output` attribute when the response is only a tool call), and the **function call itself has no span** (no tool span at all). The second `llm`'s `input` carries the assistant tool call and the tool result. STT and TTS spans (`stt`, `tts`; `transcript`, `text`, `voice_id`, `metrics.*`; `gen_ai.operation.name` `stt`/`tts`), Gemini Live and OpenAI Realtime spans (`gen_ai.operation.name` `setup`, `model_turn`, `response`, `transcription`, `tool_call`; `transcript`, `text_output`, `input`, `tools.*`) are in the source; not produced here (no audio services).

Cetra today: `llm` spans are right (`llm`, `openai`, model, 82/17 and 131/12, cost computed) but **no input and no output** (plain `input`/`output` unread, confirmed by a synthetic span on scope `pipecat`; the same two keys on a span of another scope stay unread by design); `conversation` and `turn` are `span`; STT and TTS are `llm` (finding 2).

**Rules (D-IN-6, D-IN-5, scope starts with `pipecat`):**
- (R-PC-1) plain `input` and `output` read as the span's input and output (D-IN-6's one rule). `input` is the OpenAI-shaped message list for the OpenAI service; for other services it is that service's own messages JSON (not run), stored as JSON when it does not parse.
- (R-PC-2) `gen_ai.operation.name` `tts`, `stt` (and the realtime values listed) → `span`, in the global kind table (non-standard values; no other library writes them). Keep `chat` → `llm`.
- (R-PC-3) `conversation` → `chain`, `turn` → `chain` (scope `pipecat.turn`); session from `conversation.id` on those two spans only (the `llm` spans do not carry it, so a session view will not group the model calls: a limit to state, or the SDK-side `conversation_id` as a trace-level key).
- (R-PC-4) ignore the string `NOT_GIVEN` in numeric request parameters: `gen_ai.request.top_k` is the literal string `NOT_GIVEN` (the normalizer showed it left in `attributes`; confirm it does not produce a warning in the plan).

**Verdict: captured case.** Sketch: `pipecat-ai[openai]==1.16.x` (about 100 packages, `onnxruntime` for Smart Turn), the program above with a `get_weather` handler, `base_url` on the mock, an OTLP exporter from the environment; expected kinds `span`/`chain` (conversation, turn), `llm` x2. The page notes: no tool span, STT/TTS only with those services.

## 6. Browser Use (run for real, headless Chromium)

Two lines, both run:

- **Browser Use 1.0.6** (`browser-use` resolved under the cutoff; 1.0.8 is 2026-11-22), Python 3.11+. `Browser("chromium", headless=True, executable_path=...)`, `Agent(task, llm=ChatOpenAI(base_url=<mock>), browser=..., judge=False)`. Built on Pydantic AI 2.66.0 (scope `pydantic-ai@2.66.0`). With `lmnr` 0.9.1 (`Laminar.initialize(project_api_key=..., base_url=<cetra>, http_port=..., force_http=True)`; Laminar logged "No path found in HTTP endpoint URL. Adding default path /v1/traces") or with a plain provider and `pydantic_ai.Agent.instrument_all(True)`: `invoke_agent agent` (agent, `session_id` from `gen_ai.conversation.id`, output), `chat gpt-4o-mini` (llm: tokens, cost computed, input and output previews), `execute_tool on_text` (tool). The `done` of the fake's JSON came back as an `on_text` output tool call. Nothing to fix. Laminar adds `lmnr.*` keys (convention `laminar` is listed too).
- **Browser Use 0.13.14** with `openlit` 1.45.0 (`openlit.init(otlp_endpoint=..., disable_metrics=True, capture_message_content=True)`): 21 spans: `invoke_agent browser_use` (scope `openlit.instrumentation.browser_use`, `gen_ai.operation.name=run`, `gen_ai.operation.type=invoke_agent`, `gen_ai.agent.*`, `gen_ai.provider.name=browser_use`, `server.address=browser-use.com`), `invoke_model gpt-4o-mini` (op `step`, type `execute_task`), the real `chat gpt-4o-mini` spans (scope `openlit.instrumentation.openai`, complete GenAI keys), and `opentelemetry.instrumentation.httpx` spans: the CDP discovery polls (`GET http://127.0.0.1:<chrome>/json/version`) show **14 spans with status error** while Chromium starts (noise, and they make the trace look failed). Cetra today: the two Browser Use spans are `llm` (op `run`, `step`), provider `browser_use`, no tokens; the real `chat` spans are right but `cost_source=reported` with `cost_total=0` (finding 7); the httpx spans `span`.

Network of the 0.x run: `clients2.google.com` (Chromium), `pypi.org` (browser-use update check), `raw.githubusercontent.com` (OpenLIT pricing): blocked. Set `ANONYMIZED_TELEMETRY=false` and `BROWSER_USE_CLOUD_SYNC=false`.

**Rules:** (R-OL-1) on scopes starting `openlit.instrumentation.`: `gen_ai.operation.type` (not `gen_ai.operation.name`) decides when the name is not a standard operation: `invoke_agent` → `agent`, `execute_task` → `chain`; (R-OL-2, global) a reported cost of 0 with non-zero tokens is no report: price it (`gen_ai.usage.cost` and the `*_cost` keys); (R-OL-3) add `browser_use` to the framework-name skip table (D-IN-15) so `gen_ai.provider.name=browser_use` is skipped; (R-OL-4, optional) drop `opentelemetry.instrumentation.httpx` spans with status error and a loopback `http.url` is overreach: leave it, the page says to disable the httpx instrumentation.

**Verdict: captured case** for 1.0.x (plain OTel + Pydantic AI instrumentation, and Laminar as a second line of the page): sketch `browser-use==1.0.x`, `opentelemetry-sdk`, `opentelemetry-exporter-otlp-proto-http`, Chromium from the capture runner's Playwright cache (does the runner have one? `~/.cache/ms-playwright` here; CI needs `playwright install chromium`, 150 MB), a one-step agent whose fake model returns the `done` JSON; expected kinds `agent`, `llm`, `tool`. The OpenLIT/0.x path is a **setup-only paragraph** on the same page (the design's "OpenLIT or Laminar"), backed by R-OL-1..3 and the OpenLIT page of this milestone.

## 7. Stagehand 4.4.0 (run for real up to the model call)

`@browserbasehq/stagehand` **4.4.0** (2026-11-04, also the latest; MIT; dependencies `@opentelemetry/api` 1.9.1 and `@opentelemetry/core` 2.9.0 only; the old 1.14.0 is a transitive dependency of n8n). v4 is a client SDK talking JSON-RPC to a **Chrome extension** (service worker, version `1.3.0` in the scope) that does the work and calls the model. Setup:

```ts
const browser = await localBrowser.launch({ headless: true });
const stagehand = await Stagehand.create({
  browser,
  model: { modelName: "openai/gpt-4o-mini", apiKey: "..." },        // no baseURL; `headers` only
  telemetry: { traces: { endpoint: "https://<cetra>/v1/traces", headers: { authorization: "Bearer sk-cetra-..." } } },
});
```

Observed: one POST, `Authorization: Bearer ...` intact, body **OTLP/JSON** (`{"resourceSpans": ...}`; not protobuf; cetra's `/v1/traces` must accept `application/json`: check its content-type handling in the plan), resource `service.name=stagehand-service-worker`, `service.namespace=browserbase`, `stagehand.client.name=stagehand-sdk-ts`, `stagehand.client.version=4.4.0`, scope `@browserbasehq/stagehand@1.3.0`. 68 spans for `init`, `context.pages`, `goto`, `observe`, `close`:

- 26 spans: the JSON-RPC methods (client `kind=CLIENT` from the SDK, `kind=SERVER` from the worker): `stagehand.init`, `page.goto`, `stagehand.observe`, `stagehand.close`, with `rpc.system.name=jsonrpc`, `rpc.method`, `jsonrpc.request.id`; the failing `stagehand.observe` has `error.type=AI_APICallError`, `rpc.response.status_code=-32603` and the model error as the status message.
- 42 spans: `stagehand.span.type=log` records as spans (`CDP call`, `CDP message`, `CDP response`, `Starting observation`, `Captured accessibility snapshot...`) with `stagehand.log.level`, `.message`, `.data` (CDP params, truncated by the SDK). `stagehand.span.type=operation` marks the operation spans.
- **No LLM span, no `gen_ai.*`, no `ai.*`** in the failing run. I could not see the successful `act`/`extract`/`observe` spans.

Cetra today: everything `span` (the error status is kept).

**Verdict: setup-only page** with a stated limit ("tested up to the model call"). The page tells users: run Stagehand for the browser steps and add the provider's own instrumentation in the app if they want LLM spans, or accept method spans only. If a later capture with a real key (a user's job) shows LLM spans, add: (R-SH-1) drop spans with `stagehand.span.type=log` (CDP noise, 42 of 68 here); (R-SH-2) `stagehand.act`/`observe`/`extract` as `agent`/`chain`. Nothing in this spike supports writing those into the normalizer yet.

## 8. Docs-only items

Version dates are from PyPI, npm and GitHub; everything below was read from vendor docs and source by a research pass, not run. No Docker on this machine; none of the following ran.

**Kernel (browser).** `kernel` (PyPI) and `@onkernel/sdk` 0.154.0 (2026-11-20). Telemetry is a server-side export of browser sessions: an OTLP destination (`name`, `endpoint`, `headers`) is created through Kernel's API and selected per browser by `telemetry.export.otlp.destination`; Kernel appends `/v1/logs`. **Logs only** ("doesn't send traces or metrics"); one log record per event (`network_response`, `console_error`, `api_call`, `cdp_command`, `page_navigation`), attributes `kernel.event.type`, `kernel.event.category`, `kernel.session_id`, `http.*`, `url.full`; resource `service.name=kernel-browser`; bodies can carry page content. The endpoint must resolve to a public address. cetra's `/v1/logs` takes GenAI log events only, so nothing would show. Verdict: **not supported**: logs only, no LLM content. A Kernel browser driven by Stagehand or Browser Use is traced by those.

**Twilio Agent Connect.** GA 2026-05-06; `twilio-agent-connect` 2.6.0 (2026-11-17, imports as `tac`), no OpenTelemetry dependency; the docs list Opik (`@opik.track`) only. The model calls are the user's code (the OpenAI adapter). Verdict: **not supported**.

**Docker sandboxes (`sbx` v0.54.0, 2026-11-16).** microVMs for coding agents; docs mention no OTLP export; the CLI's own usage telemetry goes to Docker (`sbx settings set telemetry.disabled true`). The Docker Python SDK is a plain Engine API client. Verdict: **not supported**.

**Vercel Sandbox.** `@vercel/sandbox` 4.1.0 (2026-11-20), `vercel-sandbox` 1.2.0 (2026-11-24); no OpenTelemetry dependency; observability is dashboard metrics; a SigNoz guide wraps calls by hand. Needs a Vercel account. Verdict: **not supported**.

**Daytona.** `daytona` 2.1.0 (2026-11-20); the TypeScript SDK moved to `@daytona/sdk` 2.1.0 (`@daytonaio/sdk` 0.232.0 is deprecated; the design names the old one). SDK-side OTLP exists: `DAYTONA_OTEL_ENABLED=true` (or `otelEnabled`/`otel_enabled`), `OTEL_EXPORTER_OTLP_ENDPOINT` (no path), `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_EXPORTER_OTLP_PROTOCOL`; spans are SDK operations (create, start, stop, delete, snapshots, volumes) and HTTP calls to the Daytona API; span and scope names not documented; no LLM, no `gen_ai.*`. Needs a Daytona account or a self-hosted server (Docker). Verdict: **not supported** as an LLM source: a trace of sandbox lifecycle calls is not what cetra's pages are for (the design says "spike decides"; the decision is: no page). If the user wants it anyway, it is a four-line setup-only paragraph and the spans arrive as plain `span`s.

**Gradio.** `gradio` 6.32.0 (2026-11-12); no OpenTelemetry of its own (the OpenTelemetry demo's chatbot is Gradio with its own provider). Users instrument the LLM client library. Verdict: **merged into** the instrumentation pages (an "In a Gradio app" paragraph on the OpenAI, Anthropic and LiteLLM pages, or one setup-only page naming OpenInference, OpenLLMetry and OpenLIT); no capture of its own (Gradio adds no spans).

**Langdock.** Hosted SaaS. Per-agent logging sends to **Langfuse** (cloud or self-hosted, `Tracing cloud URL`, keys); no OTLP. Verdict: **not supported** (a Langfuse-compatible endpoint is not what cetra serves; the design's Langfuse migration page is the only link).

**LobeChat.** lobehub/lobe-chat v2.3.5 (2026-11-22). Native OTLP, off by default: `ENABLE_TELEMETRY=1`, `OTEL_EXPORTER_OTLP_ENDPOINT` (base; `/v1/traces` appended), `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_SERVICE_NAME`; the package `packages/observability-otel` uses `@opentelemetry/exporter-trace-otlp-http` and defines `gen_ai.*` keys (`gen_ai.operation.name`, `gen_ai.system`, `gen_ai.request.model`, `gen_ai.usage.input_tokens/output_tokens`, `gen_ai.response.id`, finish reasons). An older build's published example (1.143.3) showed only HTTP server spans; whether prompts and completions are recorded is **unverified**. Self-hostable (Docker or Node) but not run. Verdict: **setup-only page**, "tested against: not tested" (D-IN-8 wording per the design); spans' LLM readiness unknown.

**Ragflow.** infiniflow/ragflow v1.0.0 (2026-11-06); Langfuse integration only, no OTLP found; Docker compose only. Verdict: **not supported**.

**Scalekit.** `scalekit-sdk-python` 2.25.0 (2026-11-13), `@scalekit-sdk/node` 2.23.0: no tracing; vendor guides (Phoenix, Arize) wrap `executeTool` in the application's own spans. No LLM calls in the SDK. Verdict: **not supported**.

**Bubble.** Hosted no-code; no OpenTelemetry or tracing hook found. Verdict: **not supported**.

**Trieve.** devflowinc/trieve: last release `trieve-helm-0.2.2` on 2025-03-07, three commits re-dated 2026-01-25, `tracing` + `sentry` only in `server/Cargo.toml`, no OpenTelemetry crate. Unmaintained. Verdict: **not supported** (stale; as Helicone's list says).

## Proposed normalizer rules (summary)

All gated on the instrumentation scope name (D-IN-5, D-IN-6): "scope starts with" is the same gate `mirascope` uses in `kind.rs`.

| ID | Scope gate | Rule |
|---|---|---|
| R-PC-1 | starts `pipecat` (`pipecat`, `pipecat.turn`) | plain `input`, `output` are the span's input and output (D-IN-6) |
| R-PC-2 | none (global kind table) | `gen_ai.operation.name` `tts`, `stt`, `setup`, `model_turn`, `response`... realtime values → `span`; only `tts`, `stt` are verified by a run's source, the realtime ones by source only: add the verified two, the rest after a capture |
| R-PC-3 | `pipecat.turn` | `conversation`, `turn` → `chain`; session from `conversation.id` |
| R-LK-1 | `livekit-agents` | `gen_ai.provider.name` with `.` or `:` is a host for D-IN-2 |
| R-LK-2 | `livekit-agents` | `llm_node` → `chain`; `llm_request_run`, `start_agent_activity` → `span` |
| R-LK-3 | `livekit-agents` | content from `lk.pii.user_input`, `lk.pii.response.text`, `lk.pii.chat_ctx`, `lk.pii.function_tool.*` (fallback to GenAI keys); TTFT from `gen_ai.response.time_to_first_chunk` (seconds) |
| R-PF-1..3 | `promptflow` | kind from `span_type`; models and tokens from `llm.response.model`, `inputs.model`, `llm.usage.*`; input from `inputs`, output from `output` |
| R-LF-1..2 | `langflow` | root and component kinds from `trace_type` (`llm` component → `chain`); input from `inputs`, output from `outputs` |
| R-N8N-1..3 | `n8n-workflow` | display name from `n8n.node.name`; `workflow.execute` → `chain`; `node.execute` without an operation → `span` |
| R-OL-1 | starts `openlit.instrumentation.` | `gen_ai.operation.type` decides when `gen_ai.operation.name` is not standard |
| R-OL-2 | none | a reported cost of 0 with non-zero tokens is no report |
| R-OL-3 | none | `browser_use` in the framework-name skip table |

No rule is needed for Browser Use 1.0, Gradio or any sandbox. Not written into any rule until a capture shows them: Stagehand's `log` spans, LobeChat's content, Pipecat's non-OpenAI `input` shapes, n8n's agent-runtime spans (`<agent>.generate`, `gen_ai.prompt`), LiveKit's STT, TTS and EOU spans.

## What each captured case needs (for the plan)

| Case | Packages (pinned at the cutoff) | Binaries / services | Program | Expected kinds |
|---|---|---|---|---|
| `n8n/agent-webhook` | `n8n@2.48.x` (npm, about 1,100 packages, Node 22+) | none; started by the runner (about 40 s, port from the runner) | Webhook → AI Agent → OpenAI Chat Model; one `POST /webhook/...` | `chain`, `span`, `agent`, `llm` |
| `langflow/basic-prompting` | `langflow==1.13.x` (Python 3.10-3.14) | server process, SQLite; `OPENAI_BASE_URL`, a credential allow-list host | starter flow, `POST /api/v1/run` with an API key | `chain` x4 (root and components), `llm` (separate trace) |
| `promptflow/openai-trace` | `promptflow-tracing` and `promptflow-devkit` 1.18.5 | none | two `@trace` functions, two chat calls | `span`, `llm` x2, `span` |
| `livekit/text-session` | `livekit-agents`, `livekit-plugins-openai` 1.8.x | none (no server, no audio) | `AgentSession.run` with a function tool | `chain`, `agent`, `llm`, `tool`, noise `span` |
| `pipecat/openai-pipeline` | `pipecat-ai[openai]` 1.16.x | none | pipeline with a function call | `chain`, `llm` x2 |
| `browser-use/one-step` | `browser-use` 1.0.x, `opentelemetry-*` | Chromium (Playwright cache, 150 MB) | one-step agent, `done` JSON | `agent`, `llm`, `tool` |

None needs Docker or a paid account. n8n, Langflow and Browser Use are heavy installs (CI minutes, tarball cache like M5's `fetch-tools.sh`); the other three are light.
