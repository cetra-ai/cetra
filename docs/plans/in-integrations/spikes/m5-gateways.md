# M5 spike: gateways (LiteLLM Proxy run for real; fifteen gateways triaged)

Run 2026-11-23 (M5), worktree `integrations` at `2ec7d25`. Python 3.13, uv 0.13.6, `exclude-newer = 2026-11-16T17:52Z`. Scratch in
`/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m5/`:

- `litellm/`: one uv project, `litellm[proxy]` **1.109.1** (newest at the cutoff), `openai` 3.51.0, `opentelemetry-sdk` and `opentelemetry-exporter-otlp-proto-http` 1.45.2. `run.py` starts an in-process fake OpenAI server (a tool call on the first turn when `tools` is sent, a text turn after; 82/17 then 131/12 tokens; it answers `gpt-4o-mini-2024-07-18` unless the request model contains `/`), an in-process OTLP/HTTP receiver that saves each POST body and headers, and the proxy (`litellm --config config.yaml`) as a subprocess with a `sitecustomize.py` that blocks and logs every non-loopback DNS lookup. `dump.py` decodes the protobuf bodies and prints every span; `out*/` hold the runs. Runs: `out1` (one model, the dump below), `out` (nine models, a stream), `outA` (literal-space header, embeddings), `outB` (content logging off), `outC` (`OTEL_SEMCONV_STABILITY_OPT_IN=gen_ai_latest_experimental`), `outD` (margin, discount and custom price), `outE` (traceparent, real model cost map).
- "cetra today" is the real `cetra_otel::normalize` with the built-in price table, through the M4 spike's `norm` binary (path dependency on this worktree). The OTLP/JSON given to it is the received protobuf converted with ids in hex and enums as integers.
- Every gateway other than LiteLLM was read from the vendor docs (WebFetch and WebSearch, 2026-11-23), with the usual "docs, not run" caveat; the one exception is Portkey's open-source package, whose code was inspected. Doc pages that redirected to another domain were not followed (Envoy's pages redirect to `theagentrouter.ai`; the content was read from the GitHub copy and search excerpts instead).

## Findings that change the design

1. **Vercel AI Gateway does export OTLP** (trace drains, Pro and Enterprise plans, OTLP/HTTP only, GenAI semconv plus `vercel.ai_gateway.*`, **no prompt or completion content**). The design table says "no OTLP export"; it is wrong. Its trace has a root span (`<operation> <model>`, SERVER) that carries the tokens and `vercel.ai_gateway.cost.total`, and one CLIENT span per provider attempt (`<model> (<provider>)`) with provider and model but no tokens. Not capturable locally (SaaS). See section 6.
2. **LiteLLM's `gen_ai.response.model` is the proxy's model alias** (`model_name` in `config.yaml`), not the upstream model: `or-claude`, `fw`, `az`, `vllm`. The request model is the upstream's (`anthropic/claude-3.5-sonnet`, `gpt-4o-mini`). With the stock normalizer, 6 of the 9 routed models stay `cost_source=none` (only `openai/gpt-4o-mini`, where alias equals model, is priced). The proxy computed the cost itself and reports it, so D-IN-16 fixes pricing for all of them; **without D-IN-16 the LiteLLM page would show no cost for any aliased model.** D-IN-20's request-model retry does not help (the alias is not the request model with a prefix removed).
3. **LiteLLM's cost keys are `gen_ai.cost.total_cost`, `gen_ai.cost.input_cost`, `gen_ai.cost.output_cost`** (its `CostBreakdown` dict, one attribute per field), not `gen_ai.usage.*_cost`. `total_cost` = `original_cost` - discount + margin (checked with a 5 % discount and a 10 % margin); `input_cost` and `output_cost` are the raw token costs, **before** discount and margin, and `input_cost` counts non-cached input only. They do not add up to `total_cost`. Read the total as the reported total; read the parts as they are.
4. **LiteLLM's documented `OTEL_HEADERS` example with `%20` does not work with cetra**: the proxy sends the value as is, so the request carries `authorization: Bearer%20test-key` (observed). A literal space (`OTEL_HEADERS="Authorization=Bearer sk-cetra-..."`) arrives as `Bearer sk-cetra-...`. The page must show the literal space and say why. (The research note assumed `%20` from LiteLLM's Traceloop example.)
5. **Tool-call responses lose their output**: `gen_ai.output.messages` is `[{"role":"assistant","parts":[],"finish_reason":"tool_calls"}]` and the tool call is only in `gen_ai.completion.0.function_call.{name,arguments}`. The `llm` span has no output preview and no tool-call part on a tool-calling turn (text turns are fine).
6. **LiteLLM embeddings have an empty `gen_ai.system`** and cetra types them `llm` (operation `aembedding`), not `embedding`; provider none, cost none (the proxy reports no cost keys on embeddings either).
7. **Network calls of the proxy**: with `LITELLM_LOCAL_MODEL_COST_MAP=True` none on the happy path. Without it, three lookups of `raw.githubusercontent.com` at start (the model cost map). On an upstream error for a Llama-family model it fetches a tokenizer (`huggingface.co/Xenova/llama-3-tokenizer`, eight blocked attempts in the groq error run). The capture runner must set `LITELLM_LOCAL_MODEL_COST_MAP=True` and block the rest.

## LiteLLM Proxy (run for real)

### Setup that was run

```yaml
# config.yaml
model_list:
  - model_name: gpt-4o-mini                      # the alias clients send
    litellm_params: {model: openai/gpt-4o-mini, api_base: http://127.0.0.1:<fake>/v1, api_key: sk-fake}
  # ... eight more: openrouter/anthropic/claude-3.5-sonnet, groq/llama-3.3-70b-versatile, deepseek/deepseek-chat,
  #     together_ai/meta-llama/Llama-3.3-70B-Instruct-Turbo, azure/gpt-4o-mini (api_version), mistral/mistral-small-latest,
  #     fireworks_ai/accounts/fireworks/models/llama-v3p3-70b-instruct, hosted_vllm/qwen
litellm_settings:
  callbacks: ["otel"]
  turn_off_message_logging: false                # true: no content (outB)
general_settings: {master_key: sk-...}           # required: the proxy refuses to start without one
```
```bash
OTEL_EXPORTER=otlp_http                          # also: otlp_grpc (needs litellm[grpc])
OTEL_ENDPOINT=https://<cetra-host>               # "/v1/traces" is appended when missing (OTEL_EXPORTER_OTLP_ENDPOINT is read too)
OTEL_HEADERS="Authorization=Bearer <key>"        # comma-separated key=value, split on the first "="; NO url-decoding (finding 4)
OTEL_SERVICE_NAME=litellm-proxy
LITELLM_LOCAL_MODEL_COST_MAP=True
```

Observed on the wire: `POST /v1/traces`, `content-type: application/x-protobuf`, `user-agent: OTel-OTLP-Exporter-Python/1.45.2`, the two custom headers; resource `service.name=litellm-proxy`, `deployment.environment=production`, `model_id=litellm-proxy`, `telemetry.sdk.*`. Without a master key the proxy exits at start (`UnsafeMasterKeyError`; `LITELLM_DANGEROUSLY_PERMIT_WEAK_OR_UNSET_MASTER_KEY=true` overrides it). No database is needed. The proxy honours a client `traceparent` (the request's spans joined the client's trace id; the client's parent span is of course not exported).

### Span dump (one chat request with a tool, model `gpt-4o-mini`; `out1`)

Spans in start order; every other span of the proxy appears in every request. Values cut at 110 characters. The `GET /health/liveliness` probe my driver made is exported as its own four-span trace, like every HTTP request to the proxy (so a Kubernetes probe makes a trace each time).

```
RESOURCE {"telemetry.sdk.language": "python", "telemetry.sdk.name": "opentelemetry", "telemetry.sdk.version": "1.45.2", "service.instance.id": "73f138cb-a5be-45d0-a602-5abe194cfda4", "service.name": "litellm-proxy", "deployment.environment": "production", "model_id": "litellm-proxy"}
### #0 `POST /v1/chat/completions` scope=fastapi@0.163.1 kind=SERVER parent=#- trace=Ec6c8Zal status=UNSET dur_ms=316.0
  http.request.method [str] = POST
  http.response.status_code [int] = 200
  http.route [str] = /v1/chat/completions
  network.protocol.version [str] = 1.1
  server.address [str] = 127.0.0.1
  server.port [int] = 54467
  url.path [str] = /v1/chat/completions
  url.scheme [str] = http
### #1 `fastapi.dependencies` scope=fastapi@0.163.1 kind=INTERNAL parent=#0 trace=Ec6c8Zal status=UNSET dur_ms=137.3
  code.function.name [str] = litellm.proxy.proxy_server.chat_completion
### #2 `auth` scope=litellm@ kind=INTERNAL parent=#3 trace=Ec6c8Zal status=STATUS_CODE_OK dur_ms=46.4
  call_type [str] = /v1/chat/completions
  service [str] = auth
### #3 `Received Proxy Server Request` scope=litellm@ kind=SERVER parent=#1 trace=Ec6c8Zal status=STATUS_CODE_OK dur_ms=308.0
  http.response.status_code [int] = 200
  http.route [str] = /v1/chat/completions
  litellm.preprocessing.duration_ms [double] = 301.0990619659424
  url.path [str] = /v1/chat/completions
### #4 `fastapi.endpoint` scope=fastapi@0.163.1 kind=INTERNAL parent=#0 trace=Ec6c8Zal status=UNSET dur_ms=174.6
  code.function.name [str] = litellm.proxy.proxy_server.chat_completion
### #5 `proxy_pre_call` scope=litellm@ kind=INTERNAL parent=#3 trace=Ec6c8Zal status=STATUS_CODE_OK dur_ms=0.1
  call_type [str] = add_litellm_data_to_request
  service [str] = proxy_pre_call
### #6 `router` scope=litellm@ kind=INTERNAL parent=#3 trace=Ec6c8Zal status=STATUS_CODE_OK dur_ms=22.0
  call_type [str] = acompletion
  service [str] = router
### #7 `router` scope=litellm@ kind=INTERNAL parent=#3 trace=Ec6c8Zal status=STATUS_CODE_OK dur_ms=0.3
  call_type [str] = async_get_available_deployment
  service [str] = router
### #8 `litellm_request` scope=litellm@ kind=INTERNAL parent=#3 trace=Ec6c8Zal status=STATUS_CODE_OK dur_ms=13.2
  gen_ai.completion.0.function_call.arguments [str] = {"city":"Paris"}
  gen_ai.completion.0.function_call.name [str] = get_weather
  gen_ai.cost.discount_amount [double] = 0.0
  gen_ai.cost.discount_percent [double] = 0.0
  gen_ai.cost.input_cost [double] = 1.2299999999999999e-05
  gen_ai.cost.margin_fixed_amount [double] = 0.0
  gen_ai.cost.margin_percent [double] = 0.0
  gen_ai.cost.margin_total_amount [double] = 0.0
  gen_ai.cost.original_cost [double] = 2.2499999999999998e-05
  gen_ai.cost.output_cost [double] = 1.0199999999999999e-05
  gen_ai.cost.tool_usage_cost [double] = 0.0
  gen_ai.cost.total_cost [double] = 2.2499999999999998e-05
  gen_ai.input.messages [str] = [{"role": "system", "parts": [{"type": "text", "content": "You are a weather bot."}]}, {"role": "user", "parts...(177B)
  gen_ai.operation.name [str] = acompletion
  gen_ai.output.messages [str] = [{"role": "assistant", "parts": [], "finish_reason": "tool_calls"}]
  gen_ai.request.model [str] = gpt-4o-mini
  gen_ai.response.finish_reasons [str] = ["tool_calls"]
  gen_ai.response.id [str] = chatcmpl-1
  gen_ai.response.model [str] = gpt-4o-mini
  gen_ai.system [str] = openai
  gen_ai.usage.input_tokens [int] = 82
  gen_ai.usage.output_tokens [int] = 17
  gen_ai.usage.total_tokens [int] = 99
  hidden_params [str] = {"model_id": "194032c34e1dae8be2414c2e2e3d39d4d83735aa7a64bcfb2d4d021ac3d3c770", "cache_key": null, "api_base"...(702B)
  http.route [str] = /v1/chat/completions
  litellm.cache_hit [bool] = false
  litellm.call_id [str] = 88ee9435-5749-4467-94c7-d4ffb0a0851c
  litellm.model_group [str] = gpt-4o-mini
  litellm.provider.model [str] = openai/gpt-4o-mini
  llm.is_streaming [str] = False
  llm.request.functions.0.description [str] = Get weather
  llm.request.functions.0.name [str] = get_weather
  llm.request.functions.0.parameters [str] = {"type": "object", "properties": {"city": {"type": "string"}}}
  llm.request.type [str] = acompletion
  llm.user [str] = user-1
  metadata.applied_guardrails [str] = []
  metadata.cold_storage_object_key [str] = 
  metadata.kubernetes_pod_routing [str] = 
  metadata.mcp_tool_call_metadata [str] = 
  metadata.prompt_management_metadata [str] = 
  metadata.requester_custom_headers [str] = {}
  metadata.requester_ip_address [str] = 127.0.0.1
  metadata.requester_metadata [str] = {'trace_id': '11ce9cf196a51ee21ea7d3999c7fd065', 'headers': {'accept-encoding': 'identity', 'content-length': ...(273B)
  metadata.routing_decision [str] = 
  metadata.spend_logs_metadata [str] = 
  metadata.team_alias [str] = 
  metadata.team_id [str] = 
  metadata.usage_object [str] = {'completion_tokens': 17, 'prompt_tokens': 82, 'total_tokens': 99, 'completion_tokens_details': None, 'prompt_...(132B)
  metadata.used_client_oauth_token [bool] = false
  metadata.user_agent [str] = Python-urllib/3.13
  metadata.user_api_key_alias [str] = 
  metadata.user_api_key_auth_metadata [str] = {}
  metadata.user_api_key_budget_reset_at [str] = 
  metadata.user_api_key_end_user_id [str] = user-1
  metadata.user_api_key_hash [str] = litellm_proxy_master_key
  metadata.user_api_key_max_budget [str] = 
  metadata.user_api_key_org_alias [str] = 
  metadata.user_api_key_org_id [str] = 
  metadata.user_api_key_project_alias [str] = 
  metadata.user_api_key_project_id [str] = 
  metadata.user_api_key_request_route [str] = /v1/chat/completions
  metadata.user_api_key_spend [double] = 0.0
  metadata.user_api_key_team_alias [str] = 
  metadata.user_api_key_team_id [str] = 
  metadata.user_api_key_team_max_budget [str] = 
  metadata.user_api_key_team_spend [str] = 
  metadata.user_api_key_user_email [str] = 
  metadata.user_api_key_user_id [str] = default_user_id
  metadata.user_api_key_user_max_budget [str] = 
  metadata.user_api_key_user_spend [str] = 
  metadata.vector_store_request_metadata [str] = 
### #9 `raw_gen_ai_request` scope=litellm@ kind=INTERNAL parent=#8 trace=Ec6c8Zal status=STATUS_CODE_OK dur_ms=13.2
  llm.openai.choices [str] = [{'finish_reason': 'tool_calls', 'index': 0, 'message': {'content': None, 'role': 'assistant', 'tool_calls': [...(220B)
  llm.openai.created [int] = 1
  llm.openai.extra_body [str] = {}
  llm.openai.id [str] = chatcmpl-1
  llm.openai.messages [str] = [{'role': 'system', 'content': 'You are a weather bot.'}, {'role': 'user', 'content': 'What is the weather in ...(119B)
  llm.openai.model [str] = gpt-4o-mini-2024-07-18
  llm.openai.object [str] = chat.completion
  llm.openai.tools [str] = [{'type': 'function', 'function': {'name': 'get_weather', 'description': 'Get weather', 'parameters': {'type':...(167B)
  llm.openai.usage [str] = {'completion_tokens': 17, 'prompt_tokens': 82, 'total_tokens': 99}
  llm.openai.user [str] = user-1
### #10 `self` scope=litellm@ kind=INTERNAL parent=#3 trace=Ec6c8Zal status=STATUS_CODE_OK dur_ms=6.4
  call_type [str] = make_openai_chat_completion_request <- track_llm_api_timing
  service [str] = self
### #11 `fastapi.serialization` scope=fastapi@0.163.1 kind=INTERNAL parent=#0 trace=Ec6c8Zal status=UNSET dur_ms=0.1
  code.function.name [str] = litellm.proxy.proxy_server.chat_completion
```

Span inventory per request (all scopes `fastapi@0.163.1` or `litellm@`, no version on the litellm scope):

| Span | Kind | Carries |
|---|---|---|
| `POST /v1/chat/completions` | SERVER | the HTTP root: method, route, status |
| `fastapi.dependencies`, `fastapi.endpoint`, `fastapi.serialization` | INTERNAL | `code.function.name` only |
| `Received Proxy Server Request` | SERVER | route, status, `litellm.preprocessing.duration_ms` |
| `auth`, `proxy_pre_call`, `router` (x2), `self` | INTERNAL | `service`, `call_type` (the proxy's own timing) |
| **`litellm_request`** | INTERNAL | **the LLM call**: tokens, model, cost, content, `litellm.*`, `metadata.*` |
| `raw_gen_ai_request` | INTERNAL | child of `litellm_request`: the provider's raw request and response as `llm.openai.*` strings |
| `Failed Proxy Server Request` | INTERNAL | on errors only |

Attribute facts from the nine-model run (`out`), all on `litellm_request`:

| Alias (client model) | upstream `litellm.provider.model` | `gen_ai.system` | `gen_ai.request.model` | `gen_ai.response.model` | `gen_ai.cost.total_cost` |
|---|---|---|---|---|---|
| gpt-4o-mini | `openai/gpt-4o-mini` | `openai` | `gpt-4o-mini` | `gpt-4o-mini` | 2.25e-05 |
| or-claude | `openrouter/anthropic/claude-3.5-sonnet` | `openrouter` | `anthropic/claude-3.5-sonnet` | `or-claude` | 0.0 (fake model, no map row) |
| groq-llama | `groq/llama-3.3-70b-versatile` | `groq` | `llama-3.3-70b-versatile` | (error: the fake's reply broke LiteLLM's Groq parser; the span has `error.*` and an exception event, no tokens) | none |
| ds | `deepseek/deepseek-chat` | `deepseek` | `deepseek-chat` | `ds` | 3.01e-05 |
| tg | `together_ai/meta-llama/Llama-3.3-70B-Instruct-Turbo` | `together_ai` | `meta-llama/Llama-3.3-70B-Instruct-Turbo` | `tg` | 1.0296e-04 |
| az | `azure/gpt-4o-mini` | `azure` | `gpt-4o-mini` | `az` | 2.25e-05 |
| mistral | `mistral/mistral-small-latest` | `mistral` | `mistral-small-latest` | `mistral` | 2.25e-05 |
| fw | `fireworks_ai/accounts/fireworks/models/llama-v3p3-70b-instruct` | `fireworks_ai` | `accounts/fireworks/models/llama-v3p3-70b-instruct` | `fw` | 8.91e-05 |
| vllm | `hosted_vllm/qwen` | `hosted_vllm` | `qwen` | `vllm` | 0.0 |
| emb (embedding) | `openai/text-embedding-3-small` | `""` (empty) | `text-embedding-3-small` | `emb` | no cost keys |

- **Provider** is the raw LiteLLM `custom_llm_provider` in `gen_ai.system` (D-IN-15 ids confirmed: `azure`, `mistral`, `hosted_vllm`, ...). No `gen_ai.provider.name` in the default mode. **LiteLLM never writes `litellm_proxy`** (the proxy's spans carry the upstream's id), so D-IN-13 needs no `litellm_proxy` gateway entry.
- **Tokens**: `gen_ai.usage.input_tokens`, `output_tokens`, `total_tokens` (ints). No cache or reasoning token attributes appeared (the fake sends none; unverified for real cached calls, `gen_ai.cost.cache_read_cost` exists in the cost dict).
- **Cost** (`gen_ai.cost.*`, doubles): `total_cost`, `input_cost`, `output_cost`, `original_cost`, `discount_amount`, `discount_percent`, `margin_fixed_amount`, `margin_percent`, `margin_total_amount`, `tool_usage_cost`; and, when set, `cache_read_cost`, `cache_creation_cost`, `reasoning_cost`, `guardrail_cost`, `service_tier`, `data_residency`, `vertex_location` (from LiteLLM's `CostBreakdown`; the last three are strings). Present on every successful chat span, absent on errors and embeddings. With `cost_margin_config: {global: 0.10}` and `cost_discount_config: {openai: 0.05}`: `total_cost` 2.35125e-05 = 2.25e-05 - 1.125e-06 + 2.1375e-06. With `input_cost_per_token`/`output_cost_per_token` on a `model_info`-less custom deployment (`my-finetune`): `total_cost` 0.12122 (per the custom prices), which cetra cannot compute (no row for the alias or the model).
- **Content**: `gen_ai.input.messages` (JSON: `[{"role":"system","parts":[{"type":"text","content":...}]}, ...]`, the OTel GenAI input shape), `gen_ai.output.messages` (same shape; empty `parts` on a tool call, finding 5), `llm.request.functions.N.{name,description,parameters}`, and the provider's raw strings in `raw_gen_ai_request`. `turn_off_message_logging: true` removes `gen_ai.input/output.messages`, `llm.request.functions.*`, `gen_ai.operation.name` and the tool-call keys; `llm.user` stays (`outB`: one `litellm_request` with tokens, model, cost, no content).
- **Other keys**: `gen_ai.operation.name` = LiteLLM call type (`acompletion`, `aembedding`, not an OTel operation), `gen_ai.response.id`, `gen_ai.response.finish_reasons` (a JSON string, not an array), `litellm.call_id`, `litellm.model_group` (the alias), `litellm.provider.model`, `litellm.cache_hit`, `llm.is_streaming` ("False" string), `llm.request.type`, `llm.user` (the request's `user`), `hidden_params`, and about 35 `metadata.*` strings (key hash, team, requester IP, `metadata.requester_metadata` with the request headers, `metadata.usage_object`, ...). No session id, no time to first token (streaming run: same keys), no `server.address` (so D-IN-2's host rule cannot apply; the provider is already explicit).
- **`OTEL_SEMCONV_STABILITY_OPT_IN=gen_ai_latest_experimental`** (`outC`): the LLM span becomes `chat gpt-4o-mini`, kind CLIENT, `gen_ai.operation.name=chat`, `gen_ai.provider.name=openai` (no `gen_ai.system`), content still on span attributes, `raw_gen_ai_request` disappears; the cost keys are unchanged. cetra reads it as is (provider `openai`, operation `chat`, cost from the table). Not needed by the page; the default mode is the one to capture and document, with the setting noted as an alternative.
- **Error span** (`litellm_request`, status ERROR): adds `error.code`, `error.message`, `error.type`, `http.response.status_code` (and `error.llm_provider`, `error.stack_trace` on upstream failures) and an exception event; no tokens.

### cetra today (real normalizer)

`out` (nine models plus a stream) and `outA` (embeddings):

| Span | kind | provider | request / response model | tokens in/out/total | cost (source) | input | output |
|---|---|---|---|---|---|---|---|
| `litellm_request` openai | llm | openai | gpt-4o-mini / gpt-4o-mini | 82/17/99 | 2.25e-05 (computed) | present | **none** (tool call, finding 5) |
| `litellm_request` openai text turn (stream) | llm | openai | gpt-4o-mini / gpt-4o-mini | 131/12/143 | 2.685e-05 (computed) | present | present |
| `litellm_request` openrouter | llm | openrouter | anthropic/claude-3.5-sonnet / or-claude | 82/17/99 | **none** | present | none |
| `litellm_request` deepseek | llm | deepseek | deepseek-chat / ds | 82/17/99 | **none** (reported 3.01e-05 ignored) | present | none |
| `litellm_request` together_ai | llm | together_ai | meta-llama/Llama-3.3-70B-Instruct-Turbo / tg | 82/17/99 | **none** | present | none |
| `litellm_request` azure | llm | `azure.ai.openai` (alias exists) | gpt-4o-mini / az | 82/17/99 | **none** | present | none |
| `litellm_request` mistral | llm | `mistral_ai` (alias exists) | mistral-small-latest / mistral | 82/17/99 | **none** | present | none |
| `litellm_request` fireworks_ai | llm | fireworks_ai | accounts/fireworks/... / fw | 82/17/99 | **none** | present | none |
| `litellm_request` hosted_vllm | llm | **`hosted_vllm`** (passes through, unmapped) | qwen / vllm | 82/17/99 | none | present | none |
| `litellm_request` groq (error) | llm | groq | llama-3.3-70b-versatile / - | none | none | present | none, status error |
| `litellm_request` embedding | **llm** (should be embedding) | **none** | text-embedding-3-small / emb | 5/0/5 | none | "hello" | - |
| `raw_gen_ai_request` | span | - | - | none | none | - | - |
| every other span (`fastapi.*`, `auth`, `router`, `self`, `Received Proxy Server Request`, ...) | span | - | - | - | - | - | - |

No double counting: `raw_gen_ai_request` carries its usage only inside `llm.openai.usage` (a string), so it stays `span`. The one-`litellm_request`-per-call count holds across retries (the groq run made three upstream attempts and one `litellm_request` span plus several `router` spans).

`outB` (content off): the span is still `llm` (tokens and model are enough), with cost, no previews. `outD`: the margin run's cost is computed 2.25e-05 by cetra (the table's price) while the proxy reports 2.35125e-05; the custom-priced `my-finetune` is `none`.

### Normalizer gaps for LiteLLM (exact)

1. **D-IN-16 keys**: add `gen_ai.cost.total_cost` to `TOTAL_KEYS`, `gen_ai.cost.input_cost` to `INPUT_KEYS`, `gen_ai.cost.output_cost` to `OUTPUT_KEYS` in `crates/otel/src/cost.rs` (spec 02 § Cost computation step 1). Not added: `original_cost`, `discount_*`, `margin_*`, `tool_usage_cost`, `cache_*_cost`, `reasoning_cost`, `guardrail_cost` (they stay in `attributes`). With them, all nine models above take the reported cost. Note the reported cost then differs from the price table when margin or discount is set; that is the intent (D-IN-16: the gateway knows its fees). `cost_input + cost_output` need not equal `cost_total`.
2. **D-IN-15 ids**: `azure` and `mistral` already alias correctly. Still unmapped among the ids seen: `hosted_vllm` (should stay a pass-through or become `vllm`? a self-hosted server: leave as sent, which is today's behaviour; no rule). The ids D-IN-15 lists but this run did not produce (`bedrock`, `azure_ai`, `vertex_ai`, `ollama_chat`, `cohere_chat`, `text-completion-openai`) are not verified by the capture. `deepseek`, `groq`, `together_ai`, `fireworks_ai`, `openrouter`, `openai` are already canonical. **No new alias is required by the capture.**
3. **Embeddings**: `gen_ai.operation.name` `aembedding` and `embedding` should map to the `embedding` kind (today `llm`). The provider is empty (`gen_ai.system=""` is skipped), so the embedding span stays provider-less; cost stays none because the proxy reports none. Also map `atext_completion`, `text_completion` to `llm` (already `llm` by default).
4. **Tool-call output**: read `gen_ai.completion.0.function_call.{name,arguments}` as the tool-call part of the output when `gen_ai.output.messages` has empty `parts`. Optional; the page can live with an empty output on tool turns.
5. **Response model is an alias** (finding 2): no normalizer fix; the reported cost covers pricing. The page says the response model column shows the proxy's alias.
6. **Noise**: the proxy exports ~10 plumbing spans per request and one trace per health probe; all become `span`. No rule; the page tells users to point probes elsewhere or accept them. (A server-side discard rule is out of scope; cetra's SDK export filter does not apply to a gateway.)
7. **User id**: `llm.user` and `metadata.user_api_key_end_user_id` are not read (user_id empty). Optional rule: `llm.user` -> user id. Not proposed unless the plan wants it.

### Nondeterminism and the capture

Differs between runs: trace and span ids, timestamps, durations, `litellm.call_id` (uuid), `gen_ai.response.id` on embeddings (uuid), `hidden_params.model_id` (a hash that changed between runs), `metadata.requester_metadata.trace_id` and `server.port`, `service.instance.id`, `litellm.preprocessing.duration_ms`, span batching (one run produced one OTLP POST, the nine-model run two). Stable: span names, the attribute key set (compared on the chat spans across runs and models: identical except `error.*` on failed calls and `llm.user`/function keys that depend on the request), values of tokens, models, cost. The capture scenario needs: a `litellm` runner (a `uv` project with the pins above), one chat turn with a tool and one text turn, `scrub.rs` rewriting `server.address`/`server.port`/IPs, `litellm.call_id`, `hidden_params`, `metadata.*` that include the requester address, and the receiver accepting protobuf (it is OTLP/HTTP protobuf, not JSON). A tool turn plus a text turn on the same model alias gives both output cases.

## Gateways one by one

Format: (1) OTLP export, (2) convention and keys, (3) local offline, (4) verdict. "Host" notes whether `ENDPOINT_PROVIDERS` (`crates/otel/src/context.rs`) already has it.

### 1. LiteLLM Proxy

(1) Yes, the `otel` callback; config above (`OTEL_ENDPOINT`, `OTEL_HEADERS`, HTTP protobuf or gRPC). (2) GenAI-style legacy keys (`gen_ai.system` raw ids, `gen_ai.usage.*`), cost in `gen_ai.cost.*`, model alias as response model; details above. (3) Yes: pip package, runs offline against a fake server (done). (4) **Captured**, with D-IN-16 (and the embedding kind rule). The OpenAI-client route (clients calling `http://localhost:4000/v1`) carries no cetra data on its own: the proxy is the exporter. Host: loopback, no mapping needed.

### 2. Portkey

(1) The hosted gateway has no export to arbitrary OTLP; the self-hosted/Enterprise gateway has an experimental one: `EXPERIMENTAL_GEN_AI_OTEL_TRACES_ENABLED=true`, `EXPERIMENTAL_GEN_AI_OTEL_EXPORTER_OTLP_ENDPOINT`, `..._HEADERS` (comma-separated `key=value`), `..._RESOURCE_ATTRIBUTES`; the docs label it "Enterprise/Self-Hosted", say the path is `/v1/traces` (their own example shows `/otel`, so the final URL is unverified) and do not name HTTP vs gRPC. The open-source npm package `@portkey-ai/gateway` 1.15.2 (2026-01-12, newest before the cutoff) was installed and its bundle searched: no `EXPERIMENTAL_GEN_AI_OTEL` string, no OpenTelemetry code (only `gen_ai.tool.*` keys for MCP tool spans). So the export is not in the OSS gateway. (2) GenAI semconv 1.40.0: `gen_ai.operation.name`, `gen_ai.provider.name`, `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.usage.input_tokens`, `output_tokens`, `cache_creation.input_tokens`, `cache_read.input_tokens`, `gen_ai.input.messages`, `gen_ai.output.messages`, `gen_ai.system_instructions`; no cost key listed; provider values unverified. (3) OSS gateway runs locally (npm, `node build/start-server.js`) but cannot export, so nothing to capture; the Enterprise gateway is not obtainable. (4) **OpenAI-client page** (setup only): `base_url=https://api.portkey.ai/v1` with Portkey headers; host `api.portkey.ai` -> `portkey` is already in `ENDPOINT_PROVIDERS` and `portkey` is in `GATEWAY_PROVIDERS` (D-IN-13). The Enterprise OTLP export gets a short "setup only, not verified" section. The OpenInference Portkey instrumentor (`openinference-instrumentation-portkey`) is a client-side route outside this milestone.

### 3. TrueFoundry

(1) Yes: dashboard AI Gateway -> Controls -> Settings -> OTEL Config, "Otel Traces Exporter": traces endpoint (full URL, `/v1/traces`), encoding Proto (HTTP) or gRPC, an `Authorization`-style header typed as you want it, additional resource attributes, an "Exclude Request Data" toggle. (2) With `ENABLE_GEN_AI_OTEL_ATTRIBUTES=true` it writes `gen_ai.operation.name` (`chat`, `embeddings`, `text_completion`, `generate_content`, `invoke_agent`, `image_generation`), `gen_ai.provider.name` (`openai`, `anthropic`, `aws.bedrock`, `azure.ai.openai`, `gcp.vertex_ai`; other integrations fall back to the integration slug), `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.usage.input_tokens`/`output_tokens`/`total_tokens`/`cache_*`/`reasoning.output_tokens`, `gen_ai.input.messages`, `gen_ai.output.messages`, `gen_ai.response.time_to_first_chunk` (seconds); always its own `tfy.*` keys (`tfy.model.metric.input_tokens`, `tfy.input`, `tfy.output`, `tfy.span_type`, ...). **Cost is only under a `tfy.*` key the docs do not name** (USD). One partner guide says the `gen_ai` mode uses `gen_ai.usage.prompt_tokens`/`completion_tokens` (legacy names); two docs disagree, so the keys are unverified. (3) SaaS (or a self-hosted TrueFoundry control plane on Kubernetes): no. (4) **OpenAI-client page, OTLP section setup only.** Host `gateway.truefoundry.ai` (the research note) and tenant hosts (`*.truefoundry.cloud`, unverified) are **not** in `ENDPOINT_PROVIDERS`; adding them is optional (the provider would be `truefoundry`, a gateway with no price rows; D-IN-13 would need to list it). Recommend: no host entry; the gateway's own `gen_ai.provider.name` is the upstream's, which is better.

### 4. Kong AI Gateway

(1) Yes: the `opentelemetry` plugin: `traces_endpoint: https://<host>/v1/traces`, `headers: {Authorization: ...}` (the plugin page links the reference and says nothing about headers; the field name is from the research note, unverified), OTLP/HTTP Protobuf only, batch settings `batch_span_count`, `batch_flush_delay`. The Gen AI attributes need Kong Gateway 3.13 or later (one doc says "AI Gateway 2.0"), come from the `ai-proxy` plugins, and "Opik's own plugin is enterprise-only" is irrelevant here. (2) GenAI semconv, span named `{operation} {model}` (`chat gpt-4o-mini`, per a partner doc): `gen_ai.operation.name`, `gen_ai.provider.name` (`openai` in the Jaeger guide; values not listed), `gen_ai.request.model`, `gen_ai.request.max_tokens`, `gen_ai.request.temperature`, `gen_ai.input.messages`, `gen_ai.output.messages`, `gen_ai.output.type`, `gen_ai.response.id`, `gen_ai.response.model`, `gen_ai.response.finish_reasons`, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, `gen_ai.usage.cache_creation.input_tokens`, `gen_ai.usage.cache_read.input_tokens`, and a separate tool-call span (`gen_ai.tool.call.id`, `gen_ai.tool.name`, `gen_ai.tool.type`, plus the model and usage keys again: **a tool-call span repeats the usage attributes, so it may double count; unverified**). **No cost attribute** (cost is metrics only). Content capture is governed by the OpenTelemetry policy page (not read). (3) Local run needs Kong's Docker image or a `.deb`/`.rpm` binary (Kong Gateway OSS 3.x; the Gen AI attributes need 3.13+ per the docs): **needs approval**, not downloaded. (4) **Not captured; setup-only page with the plugin config**, upgraded to captured if the user approves the Kong image (candidate: `kong/kong-gateway`, size to be checked, roughly several hundred MB). Everything it writes is already read by cetra; the provider is the OTel well-known value; pricing uses the table (the model is the upstream's).

### 5. Cloudflare AI Gateway

(1) Yes: gateway Settings -> OTEL exporters: URL (required), Headers (`Authorization: Bearer ...`), Authorization from a Secrets Store reference, Content type `json` (default) or `protobuf` (Phoenix's guide says its endpoint must end in `/v1/traces` and wants protobuf; cetra accepts both OTLP/HTTP JSON and protobuf, **verify**). Trace linking with `cf-aig-otel-trace-id` (32 hex) and `cf-aig-otel-parent-span-id` (16 hex) request headers; `cf-aig-metadata` becomes span attributes (names starting `gen_ai.` are reserved). (2) Attributes: `gen_ai.request.model`, **`gen_ai.model.provider`**, `gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`, **`gen_ai.prompt_json`**, **`gen_ai.completion_json`**, `gen_ai.usage.cost` (double; already a reported-cost key). Span name not documented; no `gen_ai.operation.name` listed, so kind detection depends on tokens/model (cetra falls back to `llm` on those). (3) SaaS: no (`wrangler dev` does not emulate AI Gateway). (4) **OpenAI-client page** (host `gateway.ai.cloudflare.com` -> `cloudflare_ai_gateway`, already mapped and in `GATEWAY_PROVIDERS`), **plus the OTLP export documented as setup only** with the gaps: `gen_ai.model.provider` is not a provider source and `gen_ai.prompt_json`/`gen_ai.completion_json` are not parsed. The two proposed rules are in § Proposed normalizer rules, marked "only with a hand-written fixture" since the key shape and JSON format of `_json` values is unverified (likely the provider's raw request/response JSON).

### 6. Vercel AI Gateway

(1) Yes (the design says no): Vercel **Trace Drains** (Team Settings -> Drains -> Add Drain; Pro and Enterprise; OTLP/HTTP only, no gRPC; billed $0.05 per 1,000 traces plus $0.50/GB egress; sampling rules). A drain endpoint takes a URL and headers (the Drains docs, not read). (2) GenAI semconv plus `vercel.ai_gateway.*`; resource `service.name=ai-gateway`, `service.namespace=vercel`. Spans: root `<operation> <model>` (SERVER; operation `chat`, `embeddings`, `generate_content`, `retrieval`) with `gen_ai.operation.name`, `gen_ai.provider.name` (OTel well-known value, e.g. `aws.bedrock`), `gen_ai.request.model` (the slug the client sent, `openai/gpt-5`), `gen_ai.response.model` (the resolved canonical model), `gen_ai.response.finish_reasons`, `gen_ai.request.stream`, `gen_ai.response.time_to_first_chunk` (seconds), `gen_ai.usage.input_tokens`/`output_tokens`/`reasoning.output_tokens`/`cache_creation.input_tokens`/`cache_read.input_tokens`, `http.response.status_code`, and **cost as strings** `vercel.ai_gateway.cost.total` (decimal string, USD), `vercel.ai_gateway.cost.market` (the provider's list price), `.currency`; `vercel.ai_gateway.provider` (exact slug such as `vertexAnthropic`), `vercel.ai_gateway.user.id`, `.tags`, ...; `vercel.ai_gateway.routing` and `vercel.ai_gateway.model_attempt <model>` (INTERNAL) and one `<model> (<provider>)` CLIENT span per provider attempt carrying provider, model and no usage. **No prompt or completion content.** (3) SaaS: no. (4) **Setup-only page for the trace drain** (user-owned Vercel account needed; nothing to capture without it), plus the OpenAI-client route: host `ai-gateway.vercel.sh` -> `vercel_ai_gateway` is mapped and a gateway for D-IN-13 (the OpenAI-client route covers the AI SDK and the OpenAI SDK, models `openai/gpt-4o`). Gaps if the page documents the drain, all unverifiable without a real run: cost is a string under a Vercel key (`vercel.ai_gateway.cost.total`, not read), the root is SERVER with `gen_ai.operation.name=chat` (kind probably `llm`: usage is there), and the per-attempt CLIENT spans have a model and provider but no usage so they would also be typed `llm` with no tokens (no double count of tokens or cost, but a duplicate-looking row per attempt). Do not add rules without a capture; recommend the page says "drain support is documented but not verified by cetra" or leaves the drain out.

### 7. Helicone

(1) No OTLP export in the docs read (the gateway page lists dashboards, sessions, caching, rate limits). Helicone's own platform is open source and self-hostable (docker-compose); a third-party guide describes sending gen_ai spans to OpenObserve *from the client*, not from Helicone. (2) n/a. (3) The AI Gateway is open source (Rust); `@helicone/ai-gateway` exists on npm (newest before the cutoff `0.2.0-beta.30`, 2025-07-21, a launcher that downloads a binary at install/run: **needs approval**, not installed). (4) **OpenAI-client page** (setup only): hosts `ai-gateway.helicone.ai` and `oai.helicone.ai`, both already in `ENDPOINT_PROVIDERS` -> `helicone`, a `GATEWAY_PROVIDERS` entry. Models are named by the gateway (`gpt-4o-mini`, or `provider/model`); D-IN-13 covers the `vendor/model` case.

### 8. Bifrost

(1) Yes: `otel` plugin in `config.json` (Bifrost gateway mode):
```json
{"plugins": [{"enabled": true, "name": "otel", "config": {
  "service_name": "bifrost", "collector_url": "https://<cetra-host>/v1/traces",
  "trace_type": "genai_extension", "protocol": "http",
  "headers": {"Authorization": "env.OTEL_API_KEY"}}}]}
```
From v1.5.8 the same fields go in a `profiles` array inside `config`. `headers` values accept `env.NAME` (a literal value is also possible, per docs "key-value pairs"); also `trace_headers`, `metrics_headers`, `disable_content_logging`, `excluded_attributes`, `export_raw_payloads`, `group_traces_by_session`. `trace_type` is `genai_extension` (released) or `vercel` / `open_inference` ("coming soon"). `protocol` is `http` or `grpc`. (2) GenAI extension, chat span named `gen_ai.chat`: `gen_ai.provider.name` (Bifrost's provider ids, values unverified), `gen_ai.request.model`, **`gen_ai.usage.prompt_tokens`, `gen_ai.usage.completion_tokens`, `gen_ai.usage.total_tokens`** (legacy names; cetra reads them as fallbacks), **`gen_ai.usage.cost`** (already a reported-cost key). (3) Bifrost is a Go binary distributed as `npx @maximhq/bifrost` (npm 1.6.3, 2026-05-20, newest before the cutoff; a launcher that **downloads a platform binary at first run**) or a Docker image (`maximhq/bifrost`): **needs approval**, not run. (4) **Captured only if the user approves the binary or image; else a setup-only page** (the config above is from the docs). No known normalizer gap: its keys are already read; a capture is needed to see provider values (e.g. `openai`, `anthropic`, `bedrock`?) and the operation-name mapping of `gen_ai.chat`.

### 9. Envoy AI Gateway (now "Agent Router")

(1) Yes, through the external processor's standard OTel environment: `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_SERVICE_NAME` (default `ai-gateway`), `OTEL_METRICS_EXPORTER=none`; per-Gateway endpoint and service name through the `GatewayConfig` CRD. The page documents no protocol or header variables, so the standard `OTEL_EXPORTER_OTLP_PROTOCOL` / `OTEL_EXPORTER_OTLP_HEADERS` are assumed (a SkyWalking guide uses gRPC on 11800); the doc is Kubernetes/Helm only. (2) **OpenInference by default** (`llm.model_name`, `llm.system`, `llm.token_count.prompt`, `llm.input_messages.N.*`); `AI_GATEWAY_TRACING_SEMCONV=gen_ai` switches to `gen_ai.request.model`, `gen_ai.response.model`, `gen_ai.provider.name`, `gen_ai.usage.input_tokens`, `gen_ai.input.messages`, CLIENT kind, `error.type`. Redaction: `OPENINFERENCE_HIDE_INPUTS`/`_OUTPUTS`/`_EMBEDDINGS_*` (default off), or `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT=true` to enable content in gen_ai mode. No cost attribute. Provider values unverified. (3) Runs on Kubernetes with Envoy Gateway; the docs read give no standalone mode (an `aigw` CLI exists upstream but this page does not cover it): needs a binary or Docker/kind: **needs approval**, not run. (4) **Setup-only page**, both conventions already normalize (OpenInference and GenAI are supported); captured only if a standalone binary is approved. Host: n/a (self-hosted; the upstream provider is explicit in the span).

### 10. Azure API Management

(1) No evidence of a native OTLP trace export: APIM's AI gateway logs prompts and completions to Azure Monitor / Application Insights and emits token metrics with `llm-emit-token-metric` (custom metrics, up to five dimensions). A community lab ("Azure-Samples/ai-gateway") claims OTLP export of GenAI signals to any OTLP endpoint; an authoritative Microsoft page was not found, so unverified. Langfuse's guide uses APIM only as a forwarding proxy for a client's OTLP (that works for cetra too, as a plain HTTP forward, but is not APIM's telemetry). (2) n/a. (3) Needs an Azure account: no. (4) **Not supported as a trace source; OpenAI-client page** for APIM-fronted endpoints: host `<instance>.azure-api.net` (tenant-specific) is **not** in `ENDPOINT_PROVIDERS` and should not be: the upstream is unknown (Azure OpenAI, Bedrock, Anthropic, ...). The client's own instrumentation provider applies (an Azure OpenAI deployment through APIM keeps the Azure client's provider). The page links the Azure OpenAI page.

### 11. Gravitee

(1) Gravitee's native OpenTelemetry support emits gateway HTTP spans without LLM attributes (Arize's guide says to instrument the client instead); the LLM Proxy needs APIM 4.10+ with an Enterprise licence. The `gravitee.io/corpus/...` pages found by search read as auto-generated and were disregarded. (2) n/a. (3) Self-hosted APIM is a Java/Docker stack with an Enterprise licence: no. (4) **OpenAI-client page** (setup only): the proxy exposes an OpenAI-compatible API on the user's own host, so provider stays `openai` and D-IN-2 does not apply; a custom price row prices it (like vLLM). Not worth its own page unless the plan wants the count; merge into a short "other gateways" page.

### 12. OrcaRouter

(1) No OTLP export documented (a commercial OpenAI-compatible router with "private by architecture" marketing). (2) n/a. (3) SaaS: no. (4) **OpenAI-client page** (setup only): base URL `https://api.orcarouter.ai/v1` (several pages; one Haystack discussion says `.com`, so the domain is unconfirmed). Not in `ENDPOINT_PROVIDERS`. Candidate row `api.orcarouter.ai` -> `orcarouter` if the plan wants attribution; it has no price rows, so a span would be unpriced unless it is added to the D-IN-13 gateway list with vendor-prefixed models. Recommend: no host row, a long-tail line in the appendix instead (like BytePlus, CometAPI).

### 13. Anannas AI

(1) None documented (Langfuse's and Opik's pages wrap the OpenAI client; OpenObserve instruments the OpenAI client with OpenTelemetry). (2) n/a. (3) SaaS: no. (4) **OpenAI-client page** (setup only): base URL `https://api.anannas.ai/v1` (Opik, Langfuse, Pipecat; OpenObserve writes `https://anannas.ai/v1`); models `openai/gpt-4o`, `anthropic/claude-3-5-sonnet` (vendor-prefixed). Not in `ENDPOINT_PROVIDERS`; as OrcaRouter: appendix line, a host row only if added to the gateway list.

### 14. Pydantic AI Gateway

(1) No: the gateway is now the Logfire AI Gateway; it records requests as spans **in a Logfire project** only. "Any OpenTelemetry backend" is listed as "coming soon". Pydantic AI's own spans (client side) are the M4 page. (2) n/a. (3) SaaS (or self-hosted Logfire): no. (4) **OpenAI-client page / link to the Pydantic AI page**: hosts `gateway-us.pydantic.dev` and `gateway-eu.pydantic.dev` (path `/proxy/<provider>`, e.g. `/proxy/openai`), self-hosted `<logfire-host>/proxy`. Pydantic AI users call it with `gateway/openai:gpt-5` through the Pydantic AI instrumentation (M4 captured), which writes the provider; the OpenAI-client route would be provider `openai` (the host is neither in `ENDPOINT_PROVIDERS` nor a price source). No host row.

### 15. Databricks AI Gateway

(1) No export out of the gateway: Unity AI Gateway logs traffic to a Unity Catalog "unified trace table" (OTel-format rows) and accepts spans *in* at `https://<workspace>/ai-gateway/otel/v1/traces` (protobuf over HTTP, uncompressed, Beta, per-workspace enablement). MLflow tracing (client side) can export to an external OTLP collector (`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, dual export), which is MLflow's route, not the gateway's. (2) n/a. (3) SaaS: no. (4) **Merged into the Databricks provider page** (already in the design). OpenAI-client route: workspace hosts `*.cloud.databricks.com`, `*.azuredatabricks.net`, `*.gcp.databricks.com` -> `databricks`, all already in `ENDPOINT_PROVIDERS`; the page says "not a trace export" and notes the unified trace table.

## Proposed normalizer rules

Everything below traces to a captured or documented key; items marked "capture-backed" are from the LiteLLM run above.

1. **D-IN-16, capture-backed.** `cost.rs`: `TOTAL_KEYS` += `gen_ai.cost.total_cost`; `INPUT_KEYS` += `gen_ai.cost.input_cost`; `OUTPUT_KEYS` += `gen_ai.cost.output_cost`. Spec 02 § Cost computation step 1 lists them. Tests: a golden fixture of the captured `litellm_request` span (computed and reported agree for `gpt-4o-mini`; for `openrouter/...` and an aliased `ds` the cost is reported, not none), plus a margin case (`total_cost` != the table's price, reported wins) from the margin run.
2. **Embedding kind, capture-backed.** `gen_ai.operation.name` values `embedding` and `aembedding` map to `embedding` (spec 02 § Span kind). LiteLLM's other call types (`acompletion`, `completion`, `atext_completion`, `aresponses`, ...) stay `llm` by default. Fixture: the captured `aembedding` span (provider empty, tokens 5/0/5).
3. **Tool-call output (optional, capture-backed).** When `gen_ai.output.messages` has an assistant message with empty `parts` and `gen_ai.completion.0.function_call.name` is set, build the output tool-call part from `gen_ai.completion.0.function_call.{name,arguments}`. Skip if the plan prefers; the page can say tool turns show no output.
4. **No LiteLLM alias rule needed.** `azure`, `mistral`, `deepseek`, `groq`, `together_ai`, `fireworks_ai`, `openrouter`, `openai` map or are canonical today; `hosted_vllm` passes through (a self-hosted vLLM server; a custom price row prices it). D-IN-15's other LiteLLM ids remain unverified by a capture; keep them (they are documented ids) but label them "not captured" in the plan.
5. **No `litellm_proxy` in D-IN-13's gateway list** (capture-backed: the proxy writes the upstream's provider).
6. **Cloudflare (hand-written fixture only).** Add `gen_ai.model.provider` to the provider sources (spec 02 provider precedence, after `gen_ai.system`), and parse `gen_ai.prompt_json` / `gen_ai.completion_json` as the input/output messages (OpenAI chat JSON, unverified). Do not implement without a recorded export or a vendor sample; otherwise the Cloudflare page documents the OpenAI-client route and lists the OTLP export as "provider and content not shown".
7. **Vercel trace drain (hand-written, unverified).** If ever supported: `vercel.ai_gateway.cost.total` (a decimal string) as a reported-cost key; the per-attempt CLIENT spans (`<model> (<provider>)`, no usage) should not be priced. Not proposed for M5.
8. **Gateway hosts.** Keep `ENDPOINT_PROVIDERS` as it is for M5. Optional additions only if the plan wants attribution for them (each also needs D-IN-13's gateway list and `UNPRICED_ENDPOINT_PROVIDERS` in the `context.rs` test): `api.anannas.ai`, `api.orcarouter.ai`, `gateway.truefoundry.ai`. This spike recommends against them (no price rows, no evidence they matter to the 15-gateway count).

## Needs the user's approval

Nothing was downloaded outside PyPI/npm packages installed in the scratchpad. To capture more gateways locally the user would have to approve, one by one:

| Item | Source | Size | Why |
|---|---|---|---|
| Bifrost binary via `npx @maximhq/bifrost` (npm 1.6.3; the launcher fetches a Go binary) or Docker image `maximhq/bifrost` | npm / Docker Hub | the package is small; binary and image sizes not checked (likely tens of MB / a few hundred MB) | captured case for the `otel` plugin with `trace_type: genai_extension` |
| Kong Gateway (OSS or Gateway) Docker image `kong/kong-gateway` (3.13 or later for Gen AI spans) or a `.deb` | Docker Hub / Kong | several hundred MB | the `opentelemetry` plugin plus `ai-proxy` against the fake OpenAI server |
| Envoy AI Gateway standalone binary (`aigw`) or the Envoy Gateway images with a kind cluster | GitHub releases / Docker Hub | binary tens of MB, the cluster images in the GB range | its default (OpenInference) and `gen_ai` modes |
| Helicone AI Gateway: `npx @helicone/ai-gateway` (npm `0.2.0-beta.30`, downloads a binary) | npm / GitHub | not checked | only to confirm there is no OTLP export; low value |
| Any SaaS account (Cloudflare AI Gateway, Vercel Pro drains, TrueFoundry, Portkey Enterprise, APIM) | vendor | n/a | accounts and plans: account creation is the user's call; recommended only for Cloudflare (free gateway with an OTEL exporter) if a recorded fixture is wanted |

Rust build and `uv`/npm installs that were made are all inside `m5/` (litellm, portkey) and the M4 `norm-target`.

## Summary table

| Gateway | OTLP export itself | Convention | Provider written | Reported-cost key | Local offline | Verdict |
|---|---|---|---|---|---|---|
| LiteLLM Proxy | yes (`otel` callback, HTTP protobuf or gRPC, `OTEL_ENDPOINT`, `OTEL_HEADERS`) | legacy `gen_ai.*` + `gen_ai.cost.*` | raw LiteLLM id in `gen_ai.system` (`azure`, `mistral`, `hosted_vllm`, ...) | `gen_ai.cost.total_cost` (+ `input_cost`, `output_cost`) | yes (pip, ran) | **captured**; D-IN-16 and the embedding kind rule |
| Portkey | Enterprise/self-hosted only, experimental; not in the OSS npm gateway | GenAI 1.40.0 | `gen_ai.provider.name`, values unverified | none listed | OSS gateway runs, cannot export | OpenAI-client page (`api.portkey.ai` mapped) |
| TrueFoundry | yes (dashboard OTEL config, Proto HTTP or gRPC) | gen_ai (opt-in) + `tfy.*` | `gen_ai.provider.name` (OTel ids, else slug) | a `tfy.*` key, name not documented | no (SaaS) | OpenAI-client page, OTLP setup only; no host row |
| Vercel AI Gateway | **yes** (Trace Drains, Pro/Enterprise, OTLP/HTTP, no content) | GenAI + `vercel.ai_gateway.*` | `gen_ai.provider.name` OTel well-known | `vercel.ai_gateway.cost.total` (string) | no (SaaS) | OpenAI-client page (host mapped); drain setup-only, unverified |
| Kong AI Gateway | yes (`opentelemetry` plugin, HTTP protobuf, 3.13+) | GenAI semconv | `gen_ai.provider.name`, values undocumented | none (metrics only) | needs Docker/binary | setup-only; captured if approved |
| Helicone | no | n/a | n/a | n/a | needs binary | OpenAI-client page (2 hosts mapped) |
| Cloudflare AI Gateway | yes (dashboard exporter, JSON or protobuf) | GenAI-like, odd keys | `gen_ai.model.provider` (unread) | `gen_ai.usage.cost` (read) | no (SaaS) | OpenAI-client page (host mapped); OTLP setup-only, gaps listed |
| Bifrost | yes (`otel` plugin, HTTP or gRPC) | GenAI extension, legacy usage names | `gen_ai.provider.name`, values unverified | `gen_ai.usage.cost` (read) | needs binary/Docker | captured if approved, else setup-only |
| Envoy AI Gateway | yes (OTel env on the extProc) | OpenInference default, `gen_ai` option | `llm.system` or `gen_ai.provider.name` | none | needs binary/cluster | setup-only; captured if approved |
| Azure API Management | no native OTLP trace export (Application Insights) | n/a | n/a | n/a | no | not supported as a trace source; OpenAI-client note |
| Gravitee | HTTP gateway spans only, no LLM attributes | n/a | n/a | n/a | no | OpenAI-client page, merge into an "other gateways" page |
| OrcaRouter | no | n/a | n/a | n/a | no | OpenAI-client page; no host row |
| Anannas AI | no | n/a | n/a | n/a | no | OpenAI-client page; no host row |
| Pydantic AI Gateway | no (Logfire only; other backends "coming soon") | n/a | n/a | n/a | no | link to the Pydantic AI page; no host row |
| Databricks AI Gateway | no (writes a Unity Catalog table; accepts OTLP in) | n/a | n/a | n/a | no | merged into the Databricks page (hosts mapped) |
