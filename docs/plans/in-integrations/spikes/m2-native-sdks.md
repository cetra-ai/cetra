# M2 spike: native SDKs (Groq, Mistral, Cohere, Ollama, Together, Azure OpenAI, Vertex AI)

Run 2026-11-23 in a throwaway uv project (Python 3.13, `exclude-newer` 2026-11-16, i.e. 7 days before the run), scripts in
`/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m2/`. Each SDK ran against an in-process
transport that records the request and returns the canned response, under its OpenInference instrumentor with an `InMemorySpanExporter`.
Required response fields were found by dropping each key of the response in turn and checking the SDK still parsed it and still returned the text and the token counts (`minimize.py`), so "required" means parse or lose data.

## Versions

"Usable" is the newest non-pre-release upload on or before 2026-11-16 (uv's 7-day exclude-newer). The "resolved" column is what uv picked together in one project.

| Package | Latest on PyPI (date) | Usable (date) | Resolved together |
|---|---|---|---|
| groq | 1.7.0 (2026-08-26) | 1.7.0 | 1.7.0 |
| mistralai | 3.8.0 (2026-11-18) | 3.7.0 (2026-11-01) | 3.7.0 |
| cohere | 7.3.2 (2026-11-19) | 7.3.1 (2026-11-04) | 7.3.1 |
| ollama | 0.6.3 (2026-09-29) | 0.6.3 | 0.6.3 |
| together | 3.3.0 (2026-11-19) | 3.2.0 (2026-11-16) | 3.1.1 (the instrumentor constrains it; recheck at M2) |
| openai | 3.57.0 (2026-11-20) | 3.52.0 (2026-11-16) | 3.51.0 |
| google-genai | 3.6.1 (2026-11-18) | 3.4.0 (2026-11-16) | 3.3.0 |
| openinference-instrumentation-groq | 0.1.43 (2026-11-21) | 0.1.40 (2026-11-12) | 0.1.40 |
| openinference-instrumentation-mistralai | 2.3.9 (2026-11-21) | 2.3.6 (2026-11-16) | 2.3.5 |
| openinference-instrumentation-cohere | 0.1.25 (2026-11-21) | 0.1.22 (2026-11-12) | 0.1.22 |
| openinference-instrumentation-ollama | 0.1.23 (2026-11-21) | 0.1.20 (2026-11-12) | 0.1.20 |
| openinference-instrumentation-together | 0.1.24 (2026-11-21) | 0.1.21 (2026-11-16) | 0.1.20 |
| openinference-instrumentation-google-genai | 1.6.7 (2026-11-21) | 1.6.4 (2026-11-12) | 1.6.4 |
| openinference-instrumentation-openai | 0.1.77 (2026-11-21) | 0.1.74 (2026-11-16) | 0.1.73 |

Every instrumentor requires Python `>=3.10,<3.16` (mistralai `>=3.9`). The capture project pins `==3.13.*`, so nothing blocks. The capture
project pins `google-genai==2.34.0` and `openai==3.45.0`; PyPI is now at 3.x of google-genai, so M2 decides whether to move the pin.

Mistral's package is a namespace: `from mistralai.client import Mistral` (not `from mistralai import Mistral`; the instrumentor imports `mistralai.client`).

## HTTP libraries and client injection (D-IN-3)

`openai` 3.x uses **httpx2**; **every other SDK here uses plain `httpx` 0.28.1**. The harness's `RealHostTransport` subclasses `httpx2.BaseTransport`, so M2
needs a twin `httpx.BaseTransport` (same rewrite of scheme, host and port; the `Host` header and path stay). httpx is a transitive dependency; pin it
explicitly in the capture project like httpx2.

| SDK | Parameter | Type | Default host (what the instrumentation sees) |
|---|---|---|---|
| Groq | `Groq(http_client=)` | `httpx.Client` | `https://api.groq.com` (path `/openai/v1/chat/completions`) |
| Together | `Together(http_client=)` | `httpx.Client` | base `https://api.together.ai`, but chat posts an absolute URL on **`api-inference.together.ai`** (`/v1/chat/completions`) |
| Mistral | `Mistral(client=)` | `httpx.Client` (its `HttpClient` protocol) | `https://api.mistral.ai` |
| Cohere | `cohere.ClientV2(httpx_client=)` | `httpx.Client` | `https://api.cohere.com` |
| Ollama | `ollama.Client(host=, transport=...)` | any `httpx.Client` kwarg, so `transport=` works | `http://127.0.0.1:11434` (Host `127.0.0.1:11434`); `host="http://localhost:11434"` gives Host `localhost:11434` |
| Azure OpenAI | `AzureOpenAI(http_client=)` | `httpx2.Client` | `azure_endpoint="https://<resource>.openai.azure.com"` |
| Vertex (google-genai) | `genai.Client(http_options=HttpOptions(httpx_client=))` | `httpx.Client` | `https://<location>-aiplatform.googleapis.com` (`aiplatform.googleapis.com` for `global`) |

All seven worked with a recording transport on the real host. Observed request lines:

- Groq: `POST https://api.groq.com/openai/v1/chat/completions`, body `{"messages":[...],"model":"..."}`.
- Together: `POST https://api-inference.together.ai/v1/chat/completions`, same body shape.
- Mistral: `POST https://api.mistral.ai/v1/chat/completions`, body adds `"stream":false`.
- Cohere: `POST https://api.cohere.com/v2/chat`, body `{"model","messages","stream":false}`.
- Ollama: `POST http://localhost:11434/api/chat`, body `{"model","stream":false,"messages":[...],"tools":[]}`.
- Azure: `POST https://my-resource.openai.azure.com/openai/deployments/<deployment>/chat/completions?api-version=2024-10-21`, `api-key` header, body `"model"` is the **deployment name**.
- Vertex: `POST https://us-central1-aiplatform.googleapis.com/v1beta1/projects/<p>/locations/<l>/publishers/google/models/<m>:generateContent`, `Authorization: Bearer <token>`, body `{"contents":[...]}`.

## Groq

- Versions: groq 1.7.0, openinference-instrumentation-groq 0.1.40 (usable; latest 0.1.43 is 2 days old).
- Injection: `http_client=httpx.Client(transport=...)`.
- Wire: OpenAI-compatible chat. The mock's existing `/chat/completions` suffix route serves it (path `/openai/v1/chat/completions`). The mock's full chat
  body (with `system_fingerprint`, `refusal`, `logprobs`, `prompt_tokens_details`, `completion_tokens_details`) parses with `_strict_response_validation=True`.
- Minimal response (strict parse):

```json
{"id":"c","object":"chat.completion","created":1,"model":"llama-3.3-70b-versatile",
 "choices":[{"index":0,"message":{"role":"assistant","content":"Hello"},"finish_reason":"stop"}],
 "usage":{"prompt_tokens":11,"completion_tokens":3,"total_tokens":14}}
```

- Span (`Completions`, scope `openinference.instrumentation.groq`):

| Attribute | Value |
|---|---|
| `openinference.span.kind` | `LLM` |
| `llm.provider` | `groq` |
| `llm.system` | absent |
| `llm.model_name` | `llama-3.3-70b-versatile` (request model; the response model is not recorded separately) |
| `llm.token_count.prompt` / `.completion` / `.total` | 11 / 3 / 14 |
| `llm.finish_reason`, `llm.invocation_parameters` | `stop`, `{"model": ...}` |
| `llm.input_messages.N.message.*`, `llm.output_messages.N.message.*` | role and content |
| `input.value`, `output.value` (+ mime types) | JSON |
| `server.address`, `url.full` | **absent** |

- Resolved provider: `groq` (explicit `llm.provider`, a semconv well-known value). The host `api.groq.com` is in the endpoint table, but the explicit value already wins.
- Verdict: **captured through the existing route.**

## Together

- Versions: together 3.2.0 usable (3.1.1 resolved), openinference-instrumentation-together 0.1.21 usable (0.1.20 resolved).
- Injection: `http_client=httpx.Client(transport=...)`.
- Wire: OpenAI-compatible chat on `/v1/chat/completions`. The existing suffix route serves it. **The host the SDK uses for chat is `api-inference.together.ai`,
  which is not in spec 02's host table** (`api.together.xyz`, `api.together.ai` are). It does not matter for this SDK because the provider is explicit, but
  the OpenAI client pointed at `https://api-inference.together.ai/v1` (a documented base URL) would resolve `openai`. Decide whether to add the host.
- The SDK's strict validation requires a Together-specific `"prompt": []`; the default (non-strict) mode, which real use and the harness use, parses the
  OpenAI-shaped body, including the mock's, with no change. Minimal response (strict parse, so it also holds non-strict):

```json
{"id":"c","object":"chat.completion","created":1,"model":"meta-llama/Llama-3.3-70B-Instruct-Turbo","prompt":[],
 "choices":[{"index":0,"message":{"role":"assistant","content":"Hello"},"finish_reason":"stop"}],
 "usage":{"prompt_tokens":11,"completion_tokens":3,"total_tokens":14}}
```

- Span (`Completions`, scope `openinference.instrumentation.together`): same attribute set as Groq, with `llm.provider = together`, `llm.model_name` the
  request model, tokens 11 / 3 / 14, no `llm.system`, no `server.address`.
- Resolved provider: `together` → alias → `together_ai`.
- Verdict: **captured through the existing route.**

## Mistral

- Versions: mistralai 3.7.0, openinference-instrumentation-mistralai 2.3.6 usable (2.3.5 resolved).
- Injection: `Mistral(client=httpx.Client(...))`; `server_url=` also exists.
- Wire: OpenAI-compatible chat on `/v1/chat/completions`. The existing route serves it; the mock's full chat body parses.
- Minimal response: `id`, `object`, `created`, `model`, `choices[].{index,message.content,finish_reason}` and `usage.total_tokens` are required to parse;
  send the full usage:

```json
{"id":"c","object":"chat.completion","created":1,"model":"mistral-small-latest",
 "choices":[{"index":0,"message":{"role":"assistant","content":"Hello"},"finish_reason":"stop"}],
 "usage":{"prompt_tokens":11,"completion_tokens":3,"total_tokens":14}}
```

- Span (`MistralClient.chat`, scope `openinference.instrumentation.mistralai`): kind `LLM`, `llm.provider = mistralai`, `llm.system = mistralai`,
  `llm.model_name` the request model, tokens 11 / 3 / 14, `llm.invocation_parameters` carries `""` for unset parameters (cosmetic), no `server.address`.
- Resolved provider: `mistralai` → `mistral_ai`.
- Verdict: **captured through the existing route.**

## Cohere (v2 chat)

- Versions: cohere 7.3.1, openinference-instrumentation-cohere 0.1.22 usable (latest 0.1.25 is 2 days old).
- Injection: `cohere.ClientV2(httpx_client=httpx.Client(...))`. `base_url=` also exists but would hide the real host.
- Wire: native. `POST /v2/chat`, body `{"model","messages":[{"role","content"}],"stream":false}`. The mock has no such route (`/messages`, `/chat/completions`
  do not match `/v2/chat`).
- Minimal response (dropping `id` and `finish_reason` still parses; both are kept to match the real API):

```json
{"id":"c14c80c3-18eb-4519-9460-6c92edd8cfb4","finish_reason":"COMPLETE",
 "message":{"role":"assistant","content":[{"type":"text","text":"Hello"}]},
 "usage":{"billed_units":{"input_tokens":11,"output_tokens":3},"tokens":{"input_tokens":11,"output_tokens":3}}}
```

  `usage.tokens` is what the instrumentor reads; `usage.billed_units` is what Cohere bills. The response carries no model name. Streaming (`chat_stream`)
  is SSE with a different event stream; not tried.
- Span (`ClientV2.chat`, scope `openinference.instrumentation.cohere`): kind `LLM`, `llm.provider = cohere`, `llm.system = cohere`, `llm.model_name` the
  request model (`command-a-03-2025`), `llm.finish_reason = COMPLETE`, tokens 11 / 3 / 14, input and output messages, no `server.address`.
- Resolved provider: `cohere` (well-known value, kept).
- Verdict: **needs a new route** (`POST …/v2/chat`, D-IN-4), plus an `httpx` real-host transport.

## Ollama

- Versions: ollama 0.6.3, openinference-instrumentation-ollama 0.1.20 usable (latest 0.1.23 is 2 days old).
- Injection: `ollama.Client(host="http://localhost:11434", transport=<httpx.BaseTransport>)`; the kwargs go to `httpx.Client`. The default host
  is `127.0.0.1:11434`, so the scenario must pass `host=` to match the `localhost:11434` scrub host of D-IN-4.
- Wire: native. `POST /api/chat`, body `{"model","stream":false,"messages":[...],"tools":[]}`. Not served by the mock today. With `stream=true` it is
  NDJSON (one JSON object per line); not tried.
- Minimal response (`prompt_eval_count` and `eval_count` are the token counts; the rest is kept for realism):

```json
{"model":"llama3.2","created_at":"2026-11-23T10:00:00Z",
 "message":{"role":"assistant","content":"Hello"},
 "done":true,"done_reason":"stop","total_duration":1,"load_duration":1,
 "prompt_eval_count":11,"prompt_eval_duration":1,"eval_count":3,"eval_duration":1}
```

  Only `message.{role,content}`, `prompt_eval_count` and `eval_count` were required.
- Span (`Chat`, scope `openinference.instrumentation.ollama`): kind `LLM`, `llm.provider = ollama`, no `llm.system`, `llm.model_name` the request model,
  `llm.token_count.prompt = prompt_eval_count`, `.completion = eval_count`, `.total` their sum, `llm.finish_reason = stop`, `llm.invocation_parameters =
  {"stream": false}`, no `server.address`.
- Resolved provider: `ollama`.
- Verdict: **needs a new route** (`POST …/api/chat`, with Host `localhost:11434` allowed for it).

## Azure OpenAI

- Versions: openai 3.52.0 usable (3.51.0 resolved), openinference-instrumentation-openai 0.1.74 usable (0.1.73 resolved). No Azure-specific instrumentor.
- Injection: `AzureOpenAI(azure_endpoint="https://<resource>.openai.azure.com", api_version="2024-10-21", http_client=httpx2.Client(...))`, the same httpx2 client
  as the OpenAI case, so the existing `RealHostTransport` works unchanged.
- Wire: OpenAI chat on `/openai/deployments/<deployment>/chat/completions?api-version=…`; the existing suffix route serves it (query ignored). The request
  `model` is the deployment name; the response `model` is the base model, which the mock answers from the scenario turn's model. Minimal response is the
  OpenAI chat one (the mock's body works).
- Span run under cetra's runtime (`instrument=["openai"]`, so the D-IN-18 host stamp is active):

| Attribute | Value |
|---|---|
| `openinference.span.kind` | `LLM` |
| `llm.provider` | `azure` |
| `llm.system` | `openai` |
| `llm.model_name` | `gpt-4o-2024-08-06` (the **response** model, not the deployment `my-gpt4o-deployment`; the deployment appears only in `input.value` and `llm.invocation_parameters`) |
| `llm.token_count.*` | 11 / 3 / 14 |
| `server.address` | `my-resource.openai.azure.com` (stamped by cetra's `HostStamp`; absent with plain OpenInference) |

- Resolved provider: `azure` → alias → `azure.ai.openai` (`llm.provider` precedes `llm.system`). The host would also resolve `azure.ai.openai` through `*.openai.azure.com`.
  Request model equals the base model, so pricing needs no `response_model` fallback here (OpenInference puts the base model in `llm.model_name`; it differs
  from D-IN-4's assumption that the span names the deployment).
- Verdict: **captured through the existing route** (a scenario host `<resource>.openai.azure.com` and turn model = the base model).

## Vertex AI (google-genai, `vertexai=True`)

- Versions: google-genai 3.4.0 usable (3.3.0 resolved), openinference-instrumentation-google-genai 1.6.4 (the capture project pins 1.5.0 and google-genai 2.34.0).
- Credentials: `genai.Client(vertexai=True, project="p", location="us-central1", credentials=google.oauth2.credentials.Credentials(token="fake"), http_options=HttpOptions(httpx_client=...))`.
  **`google.auth.credentials.AnonymousCredentials` does not work** (`Anonymous credentials cannot be refreshed`: the SDK refreshes when `token` is empty).
  A `google.oauth2.credentials.Credentials(token=...)` with no expiry is accepted without refresh and offline; the token goes out as `Authorization: Bearer fake`.
  Without `credentials`, the SDK looks for Application Default Credentials. `HttpOptions(base_url=...)` is the alternative that hides the real host.
- Wire: Gemini `generateContent` with Vertex's path prefix. The mock's `/models/{m}:generateContent` rule accepts it: `rsplit("/models/")` yields `gemini-2.5-flash`
  from `…/publishers/google/models/gemini-2.5-flash:generateContent`. Minimal response (only `candidates[].content.parts[].text`, `usageMetadata` counts were required):

```json
{"candidates":[{"content":{"role":"model","parts":[{"text":"Hello"}]},"finishReason":"STOP","index":0}],
 "usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":3,"totalTokenCount":14},
 "modelVersion":"gemini-2.5-flash"}
```

- Span (`GenerateContent`, scope `openinference.instrumentation.google_genai`):

| Attribute | Value |
|---|---|
| `openinference.span.kind` | `LLM` |
| `llm.provider` | **`google`** (the same value as for the Gemini Developer API) |
| `llm.system` | absent |
| `llm.model_name` | `gemini-2.5-flash` |
| `llm.token_count.*` | 11 / 3 / 14 |
| `llm.finish_reason` | `STOP` |
| `server.address`, `url.full` | absent |

- Resolved provider: `google` → alias → **`gcp.gen_ai`**, not `gcp.vertex_ai`. Nothing on the span says Vertex, and the endpoint rule does not run when a provider is
  already set. To show `gcp.vertex_ai` the normalizer would need either a spec 02 rule (a host under `*aiplatform.googleapis.com` overriding `gcp.gen_ai`, which breaks
  D-IN-2's "explicit provider wins") or the SDK stamping. This goes back to the user (D-IN-17) before the page claims a Vertex provider.
- Verdict: **captured through the existing route** (needs the `httpx` transport and a `credentials=` object); the provider finding above is the open point.

## Findings that change M2's plan

1. Only Cohere and Ollama need new mock routes; Groq, Together, Mistral, Azure and Vertex ride the existing suffix routes.
2. A second real-host transport on `httpx` is required (six SDKs), next to the httpx2 one (OpenAI and Azure).
3. Together chat goes to `api-inference.together.ai`, missing from the host table; the Together instrumentor sets the provider, so only OpenAI-client users are affected.
4. Azure's `llm.model_name` is the base model (response model), so no deployment-name pricing problem arises with OpenInference.
5. Vertex resolves `gcp.gen_ai` through OpenInference; `gcp.vertex_ai` is unreachable without a rule change.
6. None of the five new instrumentors write `server.address` or `url.full`; none of them is affected by D-IN-18's OpenAI-only stamp.
