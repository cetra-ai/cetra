# Spike: which host keys the OpenAI instrumentations write (M1)

Run 2026-11-23 in `tools/capture/py`'s environment (`openai` 3.45.0, `openinference-instrumentation-openai` 0.1.69, `opentelemetry-instrumentation-openai` 0.62.4, `opentelemetry-instrumentation-openai-v2` 2.4b0). One chat completion through `OpenAI(base_url=…, http_client=httpx.Client(transport=httpx.MockTransport(…)))`; the client kept the real base URL and the transport answered (the D-IN-3 mechanism; the request URL seen by the transport was the real one). Attributes whose key contains `server`, `url`, `api_base`, `provider`, `system` or `base`:

| Base URL | OpenInference | OpenLLMetry | OpenAI v2 |
|---|---|---|---|
| `https://openrouter.ai/api/v1` | `llm.system=openai` | `gen_ai.provider.name=openrouter`, `gen_ai.openai.api_base=https://openrouter.ai/api/v1/` | `gen_ai.provider.name=openai` |
| `https://api.groq.com/openai/v1` | `llm.provider=groq`, `llm.system=openai` | `gen_ai.provider.name=openai`, `gen_ai.openai.api_base=https://api.groq.com/openai/v1/` | `gen_ai.provider.name=openai` |
| `https://api.novita.ai/openai` | `llm.system=openai` | `gen_ai.provider.name=openai`, `gen_ai.openai.api_base=…novita…` | `gen_ai.provider.name=openai` |
| `http://localhost:11434/v1` | `llm.system=openai` | `gen_ai.provider.name=openai`, `gen_ai.openai.api_base=http://localhost:11434/v1/` | `gen_ai.provider.name=openai` |

Findings:

- Only OpenLLMetry records the endpoint (`gen_ai.openai.api_base`). OpenInference and OpenAI v2 write no `server.address`, no `url.full` and no base URL.
- OpenInference attributes from its own host table only (Groq here); an unknown host leaves `llm.system=openai`.
- OpenAI v2 always writes `gen_ai.provider.name=openai`.
- So the host rule of D-IN-2 can fix OpenLLMetry spans only, unless the span gets the host from elsewhere (D-IN-18).
- The httpx-transport routing of D-IN-3 works: the instrumentation saw the real host.

## Can cetra's Python SDK add the host?

Same environment. After `OpenAIInstrumentor().instrument()`, wrapping `openai.resources.chat.completions.Completions.create` with `wrapt` (so cetra's wrapper is the outermost) to set a context variable to `instance._client.base_url.host`, and a span processor whose `on_start` copies it into `server.address`, gives `server.address=api.novita.ai` on both the OpenInference span and the OpenAI v2 span. Wrapping `SyncAPIClient.request` instead does not work: OpenInference wraps `OpenAI.request`, so a wrapper on the base class runs inside the span. This supports D-IN-18.
