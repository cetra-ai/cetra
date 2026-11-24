# Track IN — Integrations: design

Status: **draft, under review with the user** (2026-11-23). The user answered Q1–Q4 on 2026-11-23 (§ User answers). Revised the same day after two Sonnet cold reviews (round 1: 6 blockers, 11 majors; round 2: 0 blockers, 4 majors; all addressed below). **Approved by the user on 2026-11-23**; D-IN-18 added the same day after M1's spike, with the user's answer.

Decisions are `D-IN-<n>`, in `docs/mvp-decisions.md` § Track IN. They are distinct from track I's `D-I-<n>`.

## Summary

cetra's integrations index has 21 rows: 17 libraries, conventions and migrations, and 4 coding agents. The seven competitors (Langfuse, Phoenix, Opik, LangWatch, Laminar, Helicone, MLflow) list about 180 integrations that cetra does not (session notes `int-competitor-matrix.md`). The user asked to support all of them (2026-11-23). Integration listings are the main distribution channel before launch, so each supported integration gets its own page, a tested sample where one can be tested, and captured spans where the library runs offline.

cetra ingests OTLP, so most integrations are documentation. The research (`int-tech-*.md`) found four kinds of real work behind the pages:

1. **Provider attribution.** How a call through the OpenAI client to another provider is attributed depends on the instrumentation, not on the provider:
   - OpenInference writes `llm.provider` from its own host table (Groq, xAI, DeepSeek, Fireworks, Together, Cerebras, Perplexity, Moonshot's `.cn` host, Z.ai, ollama.com, Azure, IBM, Oracle, MiniMax). Any other host (OpenRouter, Novita, DashScope, Hugging Face, Cloudflare, Databricks, SambaNova, Nebius, every gateway) leaves only `llm.system=openai`.
   - OpenLLMetry recognises Azure, AWS, Vertex and OpenRouter, and records everything else as `openai`.
   - The official OpenAI v2 instrumentation always records `openai`, but writes `server.address`.

   cetra then shows provider `openai`, and a provider-specific price never applies. On top of that, the built-in price snapshot has no rows for Groq, OpenRouter, Novita, Nebius, Hugging Face, Cloudflare Workers AI, DeepInfra, Baseten, NVIDIA NIM or watsonx, although LiteLLM's file has rows for several of them. So attribution alone would turn some priced spans into unpriced ones (D-IN-13, D-IN-14). M1's spike (`spikes/m1-host-keys.md`) found that only OpenLLMetry records the endpoint; OpenInference and OpenAI v2 write no host at all, so cetra's SDK adds it (D-IN-18).
2. **Normalizer gaps in native GenAI frameworks.** Pydantic AI, Mastra, Strands, Microsoft Agent Framework, AG2, Semantic Kernel, Mirascope, Pipecat and LiveKit emit `gen_ai.*`, but use operation names, token keys, TTFT keys or input/output keys that spec 02 does not read. VoltAgent uses its own keys and is not read at all.
3. **Capture infrastructure.** The capture mock serves only the OpenAI, Anthropic and Gemini APIs, and scenarios reach it through a base URL on `127.0.0.1`. Instrumentations that attribute by host therefore never see a real host. Native SDKs (Cohere, Ollama, Azure, Vertex) need new routes. Java and .NET need runners.
4. **Integrations that cannot be supported** with spans-only OTLP (D-CA-2) or at all: § Not supported, recorded here only (D-IN-10).

## Goals

- Every integration that a competitor documents, and that can send OTLP spans to cetra, has a page under `website/src/content/docs/docs/integrations/`. The appendix gives every matrix row a disposition: a milestone, merged into another page, or not supported with a reason. None is dropped silently.
- Each page says what it is tested against (D-IN-8), and a check keeps the index and the pages in agreement.
- Spans from each documented integration normalize into the right kinds, model, provider, tokens, cost, input and output, and session, as far as the library sends them.
- The user knows which integrations are not supported, and why.

## Non-goals

- New cetra SDKs for Java, Go, .NET, Ruby or Rust. Each language's own OpenTelemetry SDK exports to cetra (D-IN-9).
- OTLP logs and metrics. D-CA-2 stands: Cursor, Kiro, Cline, E2B and Modal export only those, and wait for a logs-and-metrics track.
- Proxy or gateway ingestion by cetra itself (D4 stands).
- Importing traces from a competitor's API.
- Evaluation libraries beyond tracing their LLM calls and sending their results as scores with the existing SDK.
- Marketing pages, logos and docs screenshots. These wait until the product is frozen.
- Destinations that are not trace sources: Slack, PagerDuty, Mixpanel, PostHog, Trubrics, ClickHouse, Hugging Face Datasets, and the competitors' own products (appendix).

## Architecture

```
integration ──OTLP──▶ cetra-otel normalize ──▶ storage
                         │ provider from the endpoint (D-IN-2), aliases (D-IN-15)
                         │ gateway pricing (D-IN-13)
                         │ new keys and operation names (D-IN-5), VoltAgent (D-IN-6)
                         │ new coding agents (D-IN-12)
tools/capture ──▶ fixtures/otlp/<group>/<case> ──▶ cetra-otel golden test
   │ real hosts routed to the mock (D-IN-3), new mock routes (D-IN-4)
   │ Java and .NET runners (D-IN-9)
just prices-update ──▶ crates/otel/prices/builtin.json (D-IN-14)
sdks/python registry ──▶ instrument=["groq", "agno", …] (D-IN-7)
sdks/python ──▶ server.address on OpenAI-client spans (D-IN-18)
cetra agents setup ──▶ OpenCode, Qwen Code, then Goose and Hermes if their spikes pass (D-IN-12)
web dashboard templates ──▶ the Coding agents template counts the new agents (D-IN-12)
website integrations pages ──▶ index, sidebar, check-samples, check-backing, check-integrations (D-IN-1, D-IN-8)
```

What changes, by area:

- **`crates/otel`**: the provider-from-endpoint rule, the alias table, gateway pricing, new key and operation mappings, the VoltAgent convention, detection of the new coding agents, and the price converter.
- **`crates/core`**: one variant, `Convention::Voltagent` (this track's briefs allow that one change).
- **`crates/server`**: the regenerated `openapi.json` (the `Convention` enum and the agent names), and the agent-context route's accepted agent names (D-CA-9) in `routes/agent_context.rs` and `api/agent_context.rs`. The agent list is also hard-coded in `crates/cli/src/agents`, `crates/otel/src/agents/mod.rs`, `crates/otel/tests/proptest.rs` and spec 04 § agent contexts; M7 edits all of them.
- **`crates/cli`**: `cetra agents setup` and `remove` for the new agents.
- **`web/`**: `src/api/schema.gen.ts` (regenerated). The Coding agents template is agent-agnostic and needs no change.
- **`tools/capture`**: host routing, suffix path matching and new routes in the mock, new `api` values, new groups, Java and .NET runners.
- **`crates/otel/tests/prices.rs`**: the price converter (test code run by `just prices-update`), and `crates/otel/prices/builtin.json`.
- **`sdks/python`**: registry rows and extras, and the host stamp of D-IN-18. `sdks/typescript` needs no change for attribution.
- **`website/`**: pages, the index, the sidebar and one new check.

Nothing changes in storage, the server's other routes or the UI pages.

### Contracts and edit points

- **Spec 02** is the contract for every normalizer change, edited in the same commit as the code:
  - § Convention detection: the precedence line and the detection table gain `voltagent`.
  - § Mapping tables: new operation names, token, TTFT, input/output and session keys.
  - Rule 12: alias rows and the framework-name skip table (D-IN-15).
  - § Cost computation, step 2: gateway pricing (D-IN-13).
  - A new subsection, "Provider from the endpoint" (D-IN-2).
  - § Coding agents: the new agents.
  - § Testing: the table gains the new libraries.
- **`crates/otel` read context**: scope-gated reads (D-IN-5, D-IN-6) need the span's instrumentation scope name, which today only `agents::detect_agent` sees. `ReadContext` gains the scope name (crate-internal), and the kind and field readers that D-IN-5 and D-IN-6 scope take it from there.
- **New fixture group names** are fixed here, as amended by D-IN-23 item 10 and D-IN-26 item 3 (M4 adds only `voltagent`; its native frameworks go in `genai`, `openinference` and `laminar`; M6 adds `promptflow` and `langflow`, and its Browser Use, MCP, OpenLIT and Promptfoo cases go in `genai`, the OpenLLMetry vector case in `openllmetry`): `voltagent`, `pipecat`, `livekit`, `promptflow`, `langflow`, `opencode`, `qwen_code`, `goose`, `hermes`, `n8n`, `litellm_proxy`, `java`, `dotnet`. Provider-attribution and provider cases go in `openinference`, `openllmetry` and `genai`. A milestone that adds a group edits all three allow-lists: `tools/capture/src/scenario.rs` `GROUPS`, `crates/otel/tests/support/mod.rs` `GROUPS`, and `fixtures/otlp/README.md` § Layout. `website/scripts/check-backing.mjs` accepts `[a-z0-9_]+` groups, widened by M6 for `n8n` (D-IN-26 item 3).
- **`Convention::Voltagent`** touches:
  - `crates/core/src/enums.rs`;
  - `crates/otel/src/detect.rs` (`PRECEDENCE`, `slot`, `mark`);
  - `crates/otel/src/payload.rs` (the precedence ordinal);
  - the conventions module;
  - `crates/otel/tests/proptest.rs`;
  - `crates/server/openapi.json` and `web/src/api/schema.gen.ts` (both through `just openapi`).
- **Capture scenarios** gain an optional per-turn `host` (D-IN-3) and the `api` values of D-IN-4. `tools/capture/src/scenario.rs` is the contract. `ENV_PREFIXES` gains the prefixes the Java and .NET runners need.
- **Python SDK**: the registry rows and extras of D-IN-7. Spec 10 § Python instrumentors lists them.
- **Docs**: one page per integration (D-IN-1), the index, the sidebar in `website/astro.config.mjs`, and `<Backing>` where fixtures or tests exist. `skills/references.json` keeps only the index and the existing pages: the skill points agents at the index, not at 120 pages.
- **Each milestone** also adds its decisions to `docs/mvp-decisions.md` § Track IN, updates the track's row in `docs/roadmap.md`, adds a `CHANGELOG.md` line, and updates spec 02 or spec 10 where it changes them.

### Provider from the endpoint (D-IN-2)

The rule, written into spec 02 as its own subsection:

1. **When it applies.** On `llm` and `embedding` spans only (a plain HTTP client span to `api.groq.com` stays without a provider), after the provider sources of § Model, provider, usage are read, and only when the resolved provider is empty or `openai`. It runs before inference from the model name.
2. **The host.** The keys are tried in this order, and the first one whose value yields a host (a non-empty string from which a host can be read) decides; a key with an empty or unparsable value is skipped:
   - `server.address`;
   - the host of `url.full`;
   - the host of `gen_ai.openai.api_base` (OpenLLMetry).

   M1's spike found that OpenLLMetry writes `gen_ai.openai.api_base` and that OpenInference and OpenAI v2 write none of them; cetra's SDKs write `server.address` for them (D-IN-18).
3. **Parsing.** It never panics, never logs a URL or a host, and reads only the host part. `url.full` may carry `?key=` or `api-key=` query values. The attributes are not consumed and stay in `attributes`.
4. **Matching.** The host is lowercased and compared with a fixed table in `cetra-otel`. An entry is an exact host, or a `*.` suffix that matches only on a label boundary (`*.openai.azure.com` matches `x.openai.azure.com`, not `evilopenai.azure.com`). The first matching entry gives the provider.
5. **No match.** An unknown host, a local host (`localhost`, loopback, private ranges) or `api.openai.com` leaves the provider as it was. A self-hosted server (vLLM, Ollama through the OpenAI client, LM Studio) keeps the provider its instrumentation wrote. Track T's custom prices still price it: a custom row without a provider matches any span.

The initial table. M1 checks every host against the provider's documentation, and every provider name against the price snapshot's names:

| Host | Provider |
|---|---|
| `openrouter.ai` | `openrouter` |
| `api.groq.com` | `groq` |
| `api.x.ai` | `x_ai` |
| `api.deepseek.com` | `deepseek` |
| `api.fireworks.ai` | `fireworks_ai` |
| `api.together.xyz`, `api.together.ai` | `together_ai` |
| `api-inference.together.ai` | `together_ai` |
| `api.novita.ai` | `novita` |
| `dashscope.aliyuncs.com`, `dashscope-intl.aliyuncs.com` | `dashscope` |
| `*.maas.aliyuncs.com` | `dashscope` |
| `router.huggingface.co` | `huggingface` |
| `api.cloudflare.com` | `cloudflare` |
| `*.cloud.databricks.com`, `*.azuredatabricks.net` | `databricks` |
| `*.gcp.databricks.com` | `databricks` |
| `api.cerebras.ai` | `cerebras` |
| `api.perplexity.ai` | `perplexity` |
| `api.moonshot.ai`, `api.moonshot.cn` | `moonshot` |
| `api.z.ai`, `open.bigmodel.cn` | `zai` |
| `api.sambanova.ai` | `sambanova` |
| `api.studio.nebius.com`, `api.tokenfactory.nebius.com` | `nebius` |
| `api.mistral.ai` | `mistral_ai` |
| `*.openai.azure.com` | `azure.ai.openai` |
| `*.services.ai.azure.com` | `azure.ai.inference` |
| `*.cognitiveservices.azure.com` | `azure.ai.openai` |
| `api.deepinfra.com` | `deepinfra` |
| `inference.baseten.co` | `baseten` |
| `*.api.baseten.co` | `baseten` |
| `api.hyperbolic.xyz` | `hyperbolic` |
| `integrate.api.nvidia.com` | `nvidia_nim` |
| `api.minimax.io` | `minimax` |
| `api.minimaxi.com` | `minimax` |
| `api.cohere.com` | `cohere` |
| `api.cohere.ai` | `cohere` |
| `ollama.com` | `ollama` |
| `aiplatform.googleapis.com` | `gcp.vertex_ai` |
| `ark.cn-beijing.volces.com` | `volcengine` |
| `ark.ap-southeast.bytepluses.com` | `byteplus` |
| `api.sarvam.ai` | `sarvam` |
| `qianfan.baidubce.com` | `qianfan` |
| `api.cometapi.com` | `cometapi` |
| `api.doubleword.ai` | `doubleword` |
| `ai-gateway.vercel.sh` | `vercel_ai_gateway` (gateway) |
| `oai.helicone.ai`, `ai-gateway.helicone.ai` | `helicone` (gateway) |
| `api.portkey.ai` | `portkey` (gateway) |
| `gateway.ai.cloudflare.com` | `cloudflare_ai_gateway` (gateway) |

Rows marked "gateway" are priced by D-IN-13. `openrouter` is also a gateway for pricing.

## The integration list

Counts are how many of the seven competitors list the integration. "Route" is what cetra's page uses. "Work" names what the page needs beyond docs. Every OpenAI-compatible provider page covers the three instrumentations; the attribution each one gives is pinned by M1's cases and the host table's unit tests.

### Providers (M2)

| Integration | Count | Route | Work |
|---|---|---|---|
| OpenRouter | 7 | OpenAI client | D-IN-2, D-IN-13, price rows |
| Groq | 6 | OpenAI client; `openinference-instrumentation-groq` | price rows (D-IN-14) |
| Ollama | 6 | `openinference-instrumentation-ollama` (provider `ollama`); OpenAI client on localhost keeps `openai` (Limits say so) | mock route `/api/chat` |
| xAI Grok | 6 | OpenAI client | — |
| Cohere | 5 | `openinference-instrumentation-cohere` | mock route `/v2/chat` |
| DeepSeek, Fireworks AI, Together AI | 5 each | OpenAI client; Together also `openinference-instrumentation-together` | aliases (D-IN-15) |
| Mistral | 5 | `openinference-instrumentation-mistralai` | — |
| Novita AI | 5 | OpenAI client | price rows |
| Google Vertex AI | 4 | Google Gen AI SDK with `vertexai=True` | Vertex paths through suffix matching; separate page, linked from Google Gen AI |
| Azure OpenAI | 2+ | `AzureOpenAI` client | deployment paths through suffix matching. Priced by `response_model` (the base model Azure returns), per spec 02 step 2 |
| Qwen (DashScope), Cerebras, Perplexity, Moonshot, Z.ai, SambaNova, Nebius, Hugging Face, Cloudflare Workers AI, Databricks | 1–3 | OpenAI client | price rows where missing |
| vLLM | 2 | OpenAI client on a local host | none: provider as sent, priced by a custom row (Limits say so) |
| IBM watsonx | 2 | OpenLLMetry `opentelemetry-instrumentation-watsonx` | setup only (D-IN-11); aliases |
| Long-tail OpenAI-compatible providers | 1–3 | OpenAI client | appendix: BytePlus, Baseten, DeepInfra, Hyperbolic, MiniMax, NVIDIA NIM, Sarvam, Qianfan, CometAPI, Doubleword |

### Gateways (M5)

| Integration | Count | Route | Work |
|---|---|---|---|
| LiteLLM Proxy | 5 | the proxy's `otel` callback; captured (M5, the proxy run against the mock) | its reported-cost keys (D-IN-16); no `litellm_proxy` gateway entry: the proxy writes the upstream provider (D-IN-13) |
| Portkey | 4 | OpenAI client (D-IN-2, D-IN-13); its OTLP export is enterprise and self-hosted (setup only, not verified) | page setup only |
| TrueFoundry | 4 | OpenAI client; its OTLP export is setup only | page setup only; no host row, its `tfy.*` cost key is not read |
| Vercel AI Gateway | 4 | AI SDK or OpenAI client; its trace drain (OTLP/HTTP, Pro and Enterprise, no content) is setup only | D-IN-2, D-IN-13; the drain's `vercel.ai_gateway.cost.total` is not read |
| Kong AI Gateway | 3 | `opentelemetry` plugin (GenAI semconv) | page setup only: the captures need Docker, which the capture machine lacks (D-IN-25) |
| Helicone | 3 | OpenAI client (no OTLP export) | D-IN-2, D-IN-13 |
| Cloudflare AI Gateway | 1 | OpenAI client; its OTLP export is setup only, with its gaps listed | `gen_ai.model.provider`, `gen_ai.prompt_json` and `gen_ai.completion_json` not read: no recorded export exists |
| Bifrost | 1 | `otel` plugin, `trace_type: genai_extension`; captured (M5) | root span a chain, flattened tool calls, `bifrost.cost.*` (D-IN-25, D-IN-16) |
| Envoy AI Gateway | 1 | OpenInference by default, GenAI opt-in; captured (M5, `aigw` with Envoy) | none: its keys are already read; the model column shows the response model (rule E1 not applied) |
| OrcaRouter, Anannas AI, Pydantic AI Gateway | 1 each | no page and no host row: OrcaRouter and Anannas AI get one appendix line each (D-IN-25), Pydantic AI Gateway is merged into the Pydantic AI page | — |

### Agent frameworks (M3: OpenInference; M4: native GenAI)

| Integration | Count | Milestone | Route | Work |
|---|---|---|---|---|
| Claude Agent SDK (Python, TS) | 6 | M3 | Claude Code's native spans (D-CA-13) | own page; the Claude Code page links it |
| Instructor | 6 | M3 | `openinference-instrumentation-instructor` | — |
| Agno | 5 | M3 | `openinference-instrumentation-agno` | — |
| Haystack | 5 | M3 | `openinference-instrumentation-haystack` | — |
| smolagents | 5 | M3 | `openinference-instrumentation-smolagents` | — |
| BeeAI (Python) | 4 | M3 | OpenInference (Python) | — |
| Temporal (with the OpenAI Agents SDK) | 4 | M3 | `temporalio-openai-agents`' `OpenAIAgentsPlugin(use_otel_instrumentation=True)`; captured (M5, with a pinned dev-server binary, D-IN-25) | Temporal's own spans are not sent by cetra's default export filter; with `export_filter` they arrive as `span` |
| DeepAgents | 3 | M3 | OpenInference LangChain | — |
| LangGraph | 2+ | M3 | OpenInference LangChain | own page; the LangChain page is retitled "LangChain" and links it |
| Guardrails AI | 2 | M3 | Guardrails' own spans, plus `instrument=["openai"]` | — |
| Mastra | 6 | M4 | `@mastra/otel-exporter` | D-IN-5 |
| Pydantic AI | 6 | M4 | native GenAI | D-IN-5 |
| Semantic Kernel (Python) | 6 | M4 | native GenAI | D-IN-5 |
| AutoGen (`autogen-agentchat`) | 5 | M4 | `openinference-instrumentation-autogen-agentchat`, enabled alone with `instrument=["autogen-agentchat"]` (D-IN-23 item 3); the user installs the package (D-IN-24) | `gen_ai.system=autogen` skipped (D-IN-15) |
| Microsoft Agent Framework (Python) | 5 | M4 | native GenAI | D-IN-5 |
| Strands Agents | 5 | M4 | native GenAI | D-IN-5 |
| AG2 (`ag2`, the AutoGen fork) | 3 | M4 | native telemetry middleware | D-IN-5 |
| Bedrock AgentCore | 3 | M4 | Strands code captured through the Strands cases; the AWS runtime setup only (D-IN-11) | as Strands |
| VoltAgent | 3 | M4 | its own keys | D-IN-6 |
| Mirascope | 1 | M4 | native GenAI; `@ops.trace` spans are `chain`, and the provider prefix of model names is stripped on its spans (D-IN-23 item 6) | D-IN-5 |
| OpenHands SDK | 1 | M4 | Laminar SDK over OTLP (`gen_ai.usage.cost` is already read as reported cost) | hand-written from the spike's dump (D-IN-23 item 8) |

### Coding agents (M7)

| Integration | Count | Route | Work |
|---|---|---|---|
| OpenCode | 4 | the v2 build's telemetry environment variables (`opencode.llm`, tool and session spans); the npm 1.x build sends no model spans | first-class, v2 only (D-IN-12, D-IN-27) |
| Qwen Code | 1 | native GenAI (D-CA-17 research: already normalizes generically) | first-class (D-IN-12); its logs and metrics are accepted and dropped (D-IN-27) |
| Pi | 1 | a cetra-owned pi extension that `cetra agents setup pi` installs (D-IN-27) | first-class |
| Hermes Agent | 3 | community plugin `hermes-otel` | captured page (D-IN-27) |
| Goose | 2 | native OTLP over HTTP | spike: first-class candidate with a double-count rule; the M7 plan decides |
| Oh My Pi | 1 | native GenAI over the `OTEL_*` environment | spike: first-class candidate; the M7 plan decides |
| OpenClaw | 3 | the built-in `diagnostics-otel` plugin, which runs in OpenClaw's gateway | first-class (D-IN-27) |
| Omnigent | 1 | native OTLP (`OMNIGENT_TELEMETRY_ENABLED` and the `OTEL_*` environment) | setup only (D-IN-27) |
| Augment Code | 1 | bundled `otel` extension; needs a login; its resource carries the user's email | setup only (D-IN-27) |
| Devin SDK | 1 | the SDK's tracing (the hosted product is not instrumented) | setup only |

### Apps, no-code, voice, browser agents, tools and evaluation (M6)

As built. D-IN-26 records M6's decisions.

| Integration | Count | Route | Work |
|---|---|---|---|
| n8n | 6 | native OTLP (2.19 and later); captured (M6, `n8n/agent-webhook`) | span names kept: `node.execute` / `workflow.execute` (D-IN-26 item 8) |
| Langflow | 5 | the Traceloop export that Langflow includes; captured (M6, `langflow/basic-prompting`) | `trace_type` sets the kind; its model call is a separate trace |
| Prompt Flow | 3 | native OTLP; captured (M6, `promptflow/openai-trace`, in its own environment, D-IN-26 item 1) | `span_type` sets the kind; the exporter needs the full `/v1/traces` URL |
| LiveKit Agents | 4 | native GenAI plus `lk.*`; captured in text mode (M6, `livekit/text-session`) | D-IN-5 (content in `lk.pii.*`); the voice spans were not captured |
| Pipecat | 4 | native GenAI; captured (M6, `pipecat/openai-pipeline`) | D-IN-5 (scoped `input`/`output`, D-IN-6); `tts` and `stt` are spans |
| Browser Use | 1 | Pydantic AI's own spans (1.0 is built on Pydantic AI); captured (M6, `genai/browser-use-agent`). The 0.x line's OpenLIT spans are unit-tested only (D-IN-26 item 11) | capture |
| Stagehand | 1 | native (v4), the `telemetry` option | setup only: its spans are OTLP/JSON, and no LLM span was seen |
| Kernel (browser), Twilio Agent Connect, Docker and Vercel sandboxes, Daytona | 1 each | § Not supported (D-IN-26) | spike decided each |
| OpenLIT | — | instrumentation library page (GenAI semconv); captured (M6, `genai/openlit-chat`, `genai/openlit-vector`) | pin `>=1.37`; its zero cost is priced by cetra (D-IN-26 item 14) |
| MCP | — | Python MCP SDK 2.x native; captured (M6, `genai/mcp-tool`); registry row `mcp` added (D-IN-26 item 7). OpenLLMetry's MCP instrumentor is not covered: it crashes on `mcp>=2` | capture |
| Vector databases (Qdrant, Milvus, Chroma, Pinecone, Weaviate) | 1 | OpenLLMetry and OpenLIT; captured for Chroma, Qdrant and Milvus (M6, `genai/openlit-vector`, `openllmetry/vector-databases`); Pinecone and Weaviate are setup-only sections | reads are `retriever`s and writes stay `span`s (Q1 (a), D-IN-26 item 13) |
| Tool APIs (Exa, Firecrawl, Tavily, Parallel AI) | 1 each | § Not supported (D-IN-26): no instrumentation; Firecrawl's OpenLIT target is the v1 SDK | spike |
| Ragas, Promptfoo, Cleanlab, UQLM, Azure AI Evaluation, Coval, Morph Reflex, Testable Minds | 1–4 | Ragas, UQLM and Azure AI Evaluation (Q4 (a), D-IN-26 item 5) and Cleanlab: setup-only pages that trace the LLM calls and send results with `cetra.score`; Promptfoo captured (M6, `genai/promptfoo-eval`); Coval, Morph Reflex and Testable Minds: § Not supported (Q3, D-IN-26 item 15) | spike |
| Harbor | 3 | setup-only page with Terminal-Bench: the protobuf export, sent as files (D-IN-26 item 6) | not run |

### Other languages (M8)

| Language | Integrations | Route | Captured |
|---|---|---|---|
| Java | Spring AI, LangChain4j, Quarkus LangChain4j, Koog (Kotlin), Arconia, Embabel | Spring AI's Micrometer GenAI spans (`gen_ai.system=spring_ai`, content in events); OpenInference Java for LangChain4j; Quarkus, Koog and Embabel native | yes, Java runner |
| .NET | Semantic Kernel, Microsoft Agent Framework, Microsoft.Extensions.AI | native GenAI (Semantic Kernel uses an older event shape) | yes, .NET runner |
| Go | Genkit Go, openai-go, langchaingo | OpenInference Go, OpenLIT Go, Genkit native | setup only |
| Ruby | ruby_llm | its OTel gem (GenAI) | setup only |
| Rust | rig, Swiftide | native GenAI | setup only |
| C++ | AI SDK C++ | its OTel export | setup only |

### Not supported

Recorded here only; the public docs do not list them (D-IN-10). None of these can send LLM spans to cetra today:

| Integration | Why |
|---|---|
| Cursor (CLI and IDE), Kiro | logs and metrics only, through an Enterprise admin export (D-CA-1, D-CA-2) |
| Cline | logs and metrics only (its variables are `CLINE_OTEL_*`) |
| E2B, Modal | platform logs and metrics only |
| Antigravity, Roo Code, Aider, Amp, Crush, Skyvern | no OpenTelemetry export found |
| Zapier, Vapi, Retell, LibreChat | no OTLP export; vendor-specific (Langfuse) or none |
| ElevenLabs Agents | OTLP-shaped JSON through a webhook; needs a forwarder |
| Dify, Flowise, Open WebUI | their OTLP export carries HTTP and database spans, not LLM spans; LLM tracing is a per-vendor UI setting |
| Playwright, Puppeteer | browser automation, not LLM libraries; covered by Browser Use and Stagehand |
| Devin (hosted product) | not instrumented; the Devin SDK is (M7) |
| langchainrb, PHP libraries, the Rust `genai` crate | no OTLP path found |
| TypeSafe | a decision-model API, not chat (D-IN-21) |
| Anyscale Endpoints | discontinued (D-IN-21) |
| Predibase | acquired, no public API (D-IN-21) |
| PrismML | no official API found (D-IN-21) |
| Meta Llama API | waitlisted preview, on a disputed host (D-IN-21) |
| BeeAI Framework (TypeScript) | the OpenInference instrumentation supports only 2025 versions (`>=0.1.9 <0.1.14`) and exports no span on current ones (D-IN-22) |
| Azure API Management | no OTLP trace export, Application Insights only (D-IN-25) |
| Gravitee | HTTP spans without LLM attributes; an OpenAI-compatible endpoint behind it is traced by the app's own instrumentation, as on the OpenAI page (D-IN-25) |
| Morph Reflex | Langfuse uses it as an OpenAI-compatible judge that answers with a bare label; cetra's judge parser needs a JSON object with `label` (D158) (D-IN-26 item 15) |
| Testable Minds | a Langfuse-API client: it polls traces and writes scores back through Langfuse's API, configured on the vendor's side (D-IN-26 item 15) |
| Kernel (browser) | exports logs only (D-IN-26) |
| Twilio Agent Connect, the Docker and Vercel sandboxes, Scalekit, Bubble | no traces (D-IN-26) |
| Daytona | infrastructure spans only (D-IN-26) |
| Langdock, Ragflow | Langfuse only (D-IN-26) |
| Trieve | stale (D-IN-26) |
| Exa, Tavily, Parallel AI | no instrumentation (D-IN-26) |
| Firecrawl | OpenLIT's target is the v1 SDK (D-IN-26) |
| Coval | no results API (D-IN-26) |

A spike that finds no OTLP span path moves its item here, and the user is told.

## Decisions

| ID | Topic | Decision | Rejected |
|---|---|---|---|
| D-IN-1 | Pages | **One page per integration**, including each OpenAI-compatible provider. Short pages share one shape: install, set up, what you get, limits, next. A framework page links to the provider pages for model calls. Existing pages keep their scope: `litellm` stays the LiteLLM SDK (the proxy gets `litellm-proxy`); `google-genai` links the new `vertex-ai`; `bedrock` links the new `bedrock-agentcore`; `langchain` is retitled "LangChain" and links the new `langgraph`; `claude-code` links the new `claude-agent-sdk`. | One "OpenAI-compatible providers" page. It is shorter to maintain, but listings, search and parity with the competitors all count pages. |
| D-IN-2 | Provider attribution | **The provider comes from the endpoint's host when the instrumentation left it empty or `openai`**, by the rule and table in § Provider from the endpoint. An explicit provider other than `openai` always wins. Local and unknown hosts are left as sent. | Inferring from the model name when the provider is `openai`: wrong for OpenRouter's `vendor/model` names and for open models served everywhere. An SDK span processor: it helps only cetra SDK users, not plain OpenInference or OpenLLMetry. |
| D-IN-3 | Capturing real hosts | **A scenario turn may set `host` (for example `api.groq.com`). The client is configured with the real base URL, and the harness routes the request to the mock over plain HTTP, without DNS or TLS:** Python passes the SDK's own HTTP-client injection (OpenAI and Groq `http_client`; the Cohere and Mistral SDKs use other parameter names, which M2 checks per SDK) an `httpx` client whose transport rewrites scheme, host and port to the mock and keeps the path; TypeScript passes a `fetch` that does the same. The mock matches paths by suffix (D-IN-4), so `/api/v1/chat/completions` (OpenRouter) or `/compatible-mode/v1/chat/completions` (DashScope) reach the chat route. An SDK with no client injection is captured with a base URL on the mock, and its case does not test attribution. The instrumentation sees the real host, so its own attribution is what a real run produces. `scrub.rs` rewrites only loopback addresses (D116), so it leaves these spans alone. Java and .NET runners keep a base URL on the mock: their cases do not test attribution. Amends D114. | A per-turn host that only feeds the scrub rewrite: the fixture would carry a host the instrumentation never saw, and the OpenInference and OpenLLMetry cases would prove nothing. |
| D-IN-4 | Mock routes | **The mock matches the existing routes by path suffix** (`…/chat/completions`, `…/responses`, `…/embeddings`, `…/messages`, and `…/models/{m}:{action}` for Gemini and Vertex), ignoring the query string, so provider prefixes (`/api/v1`, `/v3/openai`, `/openai/v1`, Azure's `/openai/deployments/{d}` with `api-version`, Vertex's `/v1/projects/{p}/locations/{l}/publishers/google`) need no route of their own. Azure's case answers the base model in `model` (the scenario turn's model) while the request names the deployment. **New routes only for native wire formats that pages document**: Cohere `/v2/chat` and Ollama `/api/chat`. Ollama's scrub host is `ollama.com`, and the `localhost:11434` allowance of D116 is dropped (D-IN-21). watsonx gets no route: setup only (D-IN-11). OpenAI-compatible providers reuse `/v1/chat/completions`. Amends D114. | A generic record/replay proxy: it hides the wire-format mistakes the mock catches. |
| D-IN-5 | Native GenAI gaps | **Spec 02 reads the extra keys the research found, each backed by a captured case**: operation names mapped to kinds (Strands' `execute_event_loop_cycle` → `chain`, `execute_structured_output` → `chain`, `decision` → `chain`; Microsoft Agent Framework's `executor.process`, `edge_group.process`, `workflow.build` → `chain` and `message.send` → `span`; Semantic Kernel's `text_completions` → `llm`; AG2's `await_human_input` → `span`); `gen_ai.usage.thinking_tokens` as reasoning; AG2's `record_usage` spans as `llm` when they carry usage and a model; `gen_ai.server.time_to_first_token` (seconds) and `mastra.completion_start_time` (an instant) as TTFT; Pydantic AI's `pydantic_ai.all_messages` and `final_result` as the agent span's input and output; Mirascope's `mirascope.trace.*` input and output and `mirascope.ops.session.id`; LiveKit's `lk.pii.*` content; Mastra's span types; OpenLLMetry and OpenLIT vector-database spans as `retriever`, only under those instrumentations' scopes. The plans pin the exact keys and values from the captures. A capture that contradicts a mapping here changes this decision first. **Amended by D-IN-23:** AG2's `record_usage` stays `span` (item 1); `gen_ai.server.time_to_first_token` is milliseconds under Strands' scope, not seconds; Microsoft Agent Framework's workflow spans are read by their keys (`executor.id`, `edge_group.id`, `workflow.id`) on its scope; `execute_structured_output`, `decision` and `gen_ai.usage.thinking_tokens` are not read, because no capture shows them. | Leaving them as `span`: the pages would show traces without kinds or I/O. |
| D-IN-6 | VoltAgent and plain `input`/`output` | **A new convention `voltagent`, detected by any `voltagent.*` attribute, placed in the extraction precedence right after `genai` and before `openinference`**, so for a field that both yield, VoltAgent's reading of its own `llm.*`, `ai.*` and `entity.*` keys wins over the OpenInference and Vercel readers. Detection stays one pass: a VoltAgent span is also listed under `openinference` or `vercel` when it carries their keys (spec 02's rule that `conventions` lists every match is unchanged). **Plain `input` and `output` are read in exactly two cases: on `voltagent` spans, and on spans whose instrumentation scope name starts with `pipecat`.** One rule in spec 02 defines this, and D-IN-5's Pipecat mapping cites it. **Amended by D-IN-23:** detection is a `voltagent.*` attribute or an instrumentation scope starting with `@voltagent/`. The model, token and session rules (V3, V4, V6) read only on `voltagent` spans and are not rows of the static tables. **Amended by D-IN-26 item 9:** the scoped plain keys are read by `crates/otel/src/conventions/scoped.rs` without a convention, filling a field only where the convention readers left it empty: plain `input`/`output` on scopes starting with `pipecat` (as before), `lk.pii.*` on scope `livekit-agents`, and `inputs`/`output` on scope `promptflow` and `inputs`/`outputs` on scope `langflow`. | Detecting by `entity.*`: a generic key family, so false positives would flip kinds. An SDK adapter: VoltAgent users do not use cetra's SDK. |
| D-IN-7 | Python registry | **Every OpenInference instrumentor a page documents becomes an `instrument=` name and an extra**: `groq`, `mistralai`, `cohere`, `ollama`, `together` (M2); `agno`, `smolagents`, `haystack`, `instructor`, `beeai` (M3; `guardrails` gets no row, D-IN-22); `autogen-agentchat` (M4); `mcp` (M6, added; D-IN-26 item 7). Each gets an extra, a place in `all`, the dev group (so the parametrized import test covers every row, not a sample), `uv.lock`, spec 10 § Python instrumentors, and the website's Python SDK reference page that `just check-py` checks. A package without Python 3.15 support skips its import test on that runtime, with the reason. Native-OTel frameworks get no row; their page sets up tracing with `cetra.init()`. Amends D-F-7's list. The import test skips a row whose extra is not installed, as for the existing rows; the dev group does not grow for them (D-IN-21). Amended by D-IN-24. | Pages that install OpenInference by hand: two ways to do one thing for SDK users. |
| D-IN-8 | Test tiers | **Four tiers, shown in the index's "Tested against" column**: captured (spans recorded from the real library), hand-written (a fixture written from the vendor's attribute reference, as for Copilot), SDK test (the registry import test), setup only (no fixture; the page says so in Limits and has no `<Backing>` box). **A new `website/scripts/check-integrations.mjs` (M2) fails when** an index row's tier disagrees with its page (captured needs a `<Backing>` with a captured fixture, hand-written one with a hand-written fixture, SDK test one with tests, setup only none), or when an index row's page is missing, or a page has no row. Samples that need a provider key or a cloud account are `{/* untested: … */}` as today, so most provider pages carry untested samples. Java, Go, Ruby, Rust and C# blocks are not code samples for `check-samples.mjs`; their pages cite the fixture or say "setup only". No test calls a real provider. | Live provider tests in CI: they need paid keys and are flaky. `just eval-live` stays the only live suite (D167). |
| D-IN-9 | Other languages | **One page per framework, plus one per language ecosystem for Go, Ruby, Rust and C++. Each uses the language's OTel SDK and the framework's native GenAI spans; there is no new cetra SDK. Java and .NET get capture runners in `tools/capture` (`java/`, a Maven project; `dotnet/`, a .NET project with `packages.lock.json`), with exact pins and the 7-day release-age rule applied by hand, and captured cases in the groups `java` and `dotnet`. Go, Ruby, Rust and C++ pages are setup only** (the user, 2026-11-23). The runners need a JDK 21 and the .NET 10 SDK, only for `just capture` of those groups and `just check-capture`. AGENTS.md § Required tools lists them. Amends D115 and D117. | New cetra SDKs per language: a large surface to maintain, for listings that plain OTel already satisfies. |
| D-IN-10 | Not supported | **Integrations that cannot send spans are recorded in this design doc only, not on the public site** (the user, 2026-11-23). No page pretends to support them. | A "Not supported yet" section on the index: the user preferred the site to list only what works. |
| D-IN-11 | Paid or cloud-only | **Integrations whose cloud side needs a paid account are listed with setup-only steps** (the user, 2026-11-23): Bedrock AgentCore's runtime, Vertex AI's credentials, Azure OpenAI, Databricks, watsonx, Portkey's and TrueFoundry's OTLP export, Kong Enterprise. The model-call side is captured against the mock where it can be. The page's Limits say which part is untested. | Leaving them out until someone can test them. |
| D-IN-12 | New coding agents | **OpenCode and Qwen Code become first-class coding agents as in track CA** (the user, 2026-11-23): `cetra agents setup <agent>` and `remove`, detection in `cetra-otel` with `metadata['coding_agent']` (`opencode`, `qwen-code`), `repository` and `branch`, the session-context hook where the agent has hooks, and the agents in the Coding agents dashboard template. **Goose and Hermes Agent get the same treatment if their spikes show stable spans with a model, tokens and tool calls; otherwise they get a page only.** The spike results and the decision are recorded in this doc, and told to the user, before M7's plan is written. Amends D-CA-1 (the agent list), D-CA-3 (detection), D-CA-4 (the `coding_agent` values), D-CA-9 (the accepted agent names) and D-CA-10 (setup); supersedes D-CA-17's "not added". | Pages only, with setup later (the user chose full support). Committing to Goose and Hermes before seeing their spans. |
| D-IN-13 | Gateway pricing | **A span whose provider is a gateway (`openrouter`, `vercel_ai_gateway`, `helicone`, `portkey`, `cloudflare_ai_gateway`; `litellm_proxy` only if M5's capture shows the proxy writes it) and that no row of that gateway prices is priced by the model's publisher, when the model is `vendor/name` and `vendor` is in a fixed vendor table** in spec 02: `openai` → `openai`, `anthropic` → `anthropic`, `google` → `gcp.gemini`, `x-ai` → `x_ai`, `mistralai` → `mistral_ai`, `deepseek` → `deepseek`, `cohere` → `cohere` (the publisher-rank providers of the snapshot). The lookup uses only rows of that publisher, for `name` with any `:suffix` removed, then for `name` with `.` replaced by `-` (`claude-sonnet-4.5` → `claude-sonnet-4-5`). A `:free` model is not priced by the fallback. A model naming no known publisher, and a `:free` model, stays unpriced: never a reseller's row. `cost_source` stays `computed`. Gateway fees are not modelled, and the gateway pages say so. Amends spec 02 § Cost computation step 2 for gateways only; `cost.rs`'s lookup gains this second lookup. A gateway model without `vendor/` (OpenLLMetry strips it for OpenRouter) is looked up among each publisher's rows in the table's order (`anthropic`, `cohere`, `deepseek`, `google`, `mistralai`, `openai`, `x-ai`), and the first publisher with a row wins (M1's captures). | Leaving gateway spans unpriced: OpenRouter (7 of 7) would show no cost. A no-provider fallback: it can pick a reseller's row. Mapping gateway hosts to the upstream provider: the host does not tell which upstream served the call. |
| D-IN-14 | Price snapshot | **This track owns the fix to the converter behind `just prices-update`** (`crates/otel/tests/prices.rs`; track T is done). Root cause (`spikes/m1-price-converter.md`): `model_name()` drops every key that still contains `/` after the provider prefix, so `groq/openai/gpt-oss-120b`, Novita, Nebius, DeepInfra, Baseten, Hyperbolic, watsonx, Together and Vercel AI Gateway rows are missing. The converter keeps such names (the part after the prefix is the model id the client sends); a key without its provider prefix that contains `/` stays skipped, and so does a nested name on a cloud platform (rank 1: Vertex AI, Bedrock, Azure, watsonx), whose prefixes are regions or deployments (`us-east-1/…`). The snapshot is refreshed under D104 (the source commit in the commit message). Consequences: the snapshot grows (M1 measures it and checks the compiled pattern set stays within the regex size limit); new resellers rank after publishers, and D-IN-13 never falls back to them; NVIDIA NIM stays unpriced (LiteLLM has only `rerank` rows); OpenRouter and Hugging Face have no LiteLLM rows. A new test checks that every provider in `builtin.json` is a fixed point of rule 12, so the converter's mapping and the alias table cannot drift apart. | Hand-written price rows: the snapshot is regenerated each release and would drop them. |
| D-IN-15 | Aliases and framework names | **Rule 12 gains**: `fireworks` → `fireworks_ai`; `together`, `togetherai` → `together_ai`; `watsonx`, `ibm` → `ibm.watsonx.ai`; `oracle` → `oci`; and LiteLLM's ids `bedrock` → `aws.bedrock`, `azure_ai` → `azure.ai.inference`, `mistral` → `mistral_ai`, `vertex_ai` and `vertex_ai-*` → `gcp.vertex_ai`, `ollama_chat` → `ollama`, `cohere_chat` → `cohere`, `text-completion-openai` → `openai`. M1 checks each LiteLLM id against LiteLLM's provider list. **A named table "framework names in `gen_ai.system`" lists values that are never providers**: `gcp.vertex.agent` (existing, D-B1-8), `autogen`, `spring_ai`, plus any a capture finds. A value in it is skipped and stays unread. **Amended by D-IN-23:** the framework-name skip covers both `gen_ai.provider.name` and `gen_ai.system`, and its table gains `strands-agents` and `microsoft.agent_framework`. | Per-case exceptions in prose, which later frameworks would have to rediscover. |
| D-IN-16 | Gateway reported cost | **A gateway's own cost attribute is read as reported cost (D109, spec 02 step 1)**, once a captured case shows its exact keys: LiteLLM Proxy's cost keys (M5). Keys that no capture shows are not added. Amends D109's key list. **Amended by D-IN-25:** the keys are LiteLLM's `gen_ai.cost.*` and Bifrost's `bifrost.cost.input` and `bifrost.cost.output`, as captured. | Ignoring gateway cost: the gateway knows its own fees. |
| D-IN-17 | Spikes | **A milestone's spikes run before its plan is written**: a throwaway capture or a source read in the track's worktree, written up in `docs/plans/in-integrations/spikes/<item>.md` (what was run, versions, the span names and keys seen, the decision it supports). A spike result that changes a decision or the scope updates this doc first, and the user hears of any change in scope. | Spikes inside plan phases: a plan would then make design decisions. |
| D-IN-18 | SDK host stamp | **cetra's Python SDK wraps the OpenAI client's resource methods (chat completions, responses, embeddings, sync and async) as the outermost wrapper, once, when `instrument` names `openai`; while a call runs, a context variable holds the client's base-URL host, and cetra's span processor sets `server.address` on spans that start during it and do not already have one.** The server's host rule (D-IN-2) then attributes OpenInference and OpenAI v2 spans. TypeScript needs nothing: OpenInference JS (4.5.0) already writes `server.address` and `server.port` for every client with a string `baseURL` (`spikes/m1-ts-host-stamp.md`), so the host rule attributes its spans as they are (the user, 2026-11-23, `spikes/m1-host-keys.md`). The host is not a payload and is not logged. | Docs only (provider `openai` on unknown hosts for SDK users). Wrapping `SyncAPIClient.request`: it runs inside OpenInference's span, so the processor sees nothing. |
| D-IN-19 | M1 cases and the mock host check | **M1 captures four cases: OpenRouter and Novita through OpenInference (run through cetra's runtime with the test seam `Runtime.create(config, exporter=…)`, so they pin D-IN-18's stamp) and through OpenLLMetry. No OpenAI v2 case**: OpenAI v2 writes no host and cetra's stamp is tied to the OpenInference `openai` instrumentor, so such a case would pin only `openai`, unpriced. The mock checks a turn's `host` against the request's `Host` header. LiteLLM's `clf_ai_gateway` is renamed `cloudflare_ai_gateway` by the converter and by rule 12. BytePlus and Volcengine hosts wait for M2's long-tail providers. | Six cases including OpenAI v2: they would fail the snapshot pricing test and prove nothing new. |
| D-IN-20 | Request-model retry | **When the response model finds no price row and equals the request model with its `vendor/` prefix removed, pricing repeats the lookup with the request model.** OpenLLMetry strips the `vendor/` prefix from `gen_ai.response.model` (`openai/gpt-4o-mini` → `gpt-4o-mini`) but keeps it in the request model, so reseller spans such as Novita's would otherwise stay unpriced (found by M1's OpenLLMetry captures). OpenRouter through OpenLLMetry strips the prefix from both models; D-IN-13's bare-name fallback covers it. | Pricing the request model first: a response model is more precise (dated snapshot names) when both price. |
| D-IN-21 | M2 scope | **M2's scope details.** Pages for TypeSafe, Anyscale Endpoints, Predibase, PrismML and the Meta Llama API are dropped and listed in § Not supported (TypeSafe is a decision-model API, not chat; Anyscale Endpoints is discontinued; Predibase was acquired and has no public API; PrismML has no official API; the Meta Llama API is a waitlisted preview on a disputed host). Vertex AI shows `gcp.gen_ai`, not `gcp.vertex_ai`: OpenInference writes `llm.provider=google` for both Google endpoints, so the host does not reach cetra; `gcp.vertex_ai` is not a provider for Vertex in M2. The registry import test keeps skipping a row whose extra is not installed, and the dev group does not grow. Pages for OpenAI-compatible providers without a capture are setup only: the endpoint table's unit tests cover the host, not the provider's spans. Ollama's scrub host is `ollama.com`; its `localhost:11434` allowance is dropped, since the Ollama instrumentor writes no host, so no loopback address reaches the scrub. | Pages for the five drops: each has no public API to call or no span path. Mapping Vertex to `gcp.vertex_ai` from the host: the instrumentation's `google` provider is not empty or `openai`, so the host rule never runs. Keeping the Ollama loopback allowance: no span carries the loopback host. |
| D-IN-22 | M3 scope | **M3's scope changes from the spike.** No `guardrails` registry row: Guardrails' native spans plus `instrument=["openai"]`. BeeAI TypeScript is not supported upstream. Temporal is captured in M5, with a pinned dev-server binary (D-IN-25); in M3 it was setup only. The Temporal route is `temporalio-openai-agents`' `OpenAIAgentsPlugin(use_otel_instrumentation=True)`, not "a tracing interceptor". Agno, smolagents, Haystack and BeeAI pages tell readers not to enable `openai` as well, because a second `llm` span per call would count tokens and cost twice. Instructor and Guardrails need `openai`. The Guardrails and Temporal pages use `cetra.init(use_global_provider=True, …)`: `cetra.init()` uses its own provider by default, so spans emitted on the global provider never reach cetra. In that mode cetra's default export filter keeps only the spans of LLM instrumentations, so Temporal's own workflow and activity spans are not sent; the `export_filter` option keeps them. | The spike's scope: a `guardrails` registry row (the OpenInference Guardrails instrumentor supports only guardrails-ai below 0.5.1); a BeeAI TypeScript page (the instrumentor exports no span on current versions); a Temporal capture (its dev-server binary is downloaded at runtime, not pinned); `cetra.init()` with its own provider on pages whose library writes to the global one. |
| D-IN-23 | M4 decisions | **M4's decisions, from the spikes and the user's answers.** (1) AG2 `record_usage` stays `span`: its tokens repeat the `chat` span's, so it would double count. `gen_ai.usage.thinking_tokens` is read only if a capture shows it; none does, so it is not read. (2) The framework-name table applies to `gen_ai.provider.name` and `gen_ai.system` alike, and gains `strands-agents` and `microsoft.agent_framework`. (3) The AutoGen page enables the AutoGen instrumentor only: adding OpenInference's OpenAI instrumentor records each call twice. (4) Semantic Kernel content is out of scope: its messages are OTLP log records, which cetra drops. (5) Strands content is read both from its latest-semconv details events and from its default legacy events, through a Bedrock-Converse block converter. (6) Mirascope's `@ops.trace` spans are `chain`; the provider prefix of its model names is stripped on Mirascope spans only. (7) Pydantic AI's agent span input is its whole `pydantic_ai.all_messages`. (8) OpenHands is hand-written, from the spike's real dump: it cannot share the capture project's lock (`mcp` conflict). A Laminar `LLM` span with no model and no tokens is `chain`. (9) VoltAgent detection: a `voltagent.*` attribute, or an instrumentation scope starting with `@voltagent/`. Only the root span carries `voltagent.*`. (10) Fixture groups: the native GenAI frameworks go in `genai` (`pydantic-ai-agent-tool`, `semantic-kernel-agent-tool`, `strands-agent-tool`, `strands-agent-tool-latest`, `agent-framework-agent-tool`, `agent-framework-workflow`, `ag2-agent-tool`, `mirascope-agent-tool`, `mastra-agent-tool`); AutoGen in `openinference` (`autogen-agentchat-tool`); OpenHands in `laminar` (`openhands-agent`, hand-written); VoltAgent in the new group `voltagent` (`agent-tool`). This replaces the design's per-framework group list, and avoids `ag2`'s digits in a group name. (11) Agno (M3): an OpenInference `input.value` or `output.value` labelled `application/json` that does not parse as JSON is text, with no normalize error. | — |
| D-IN-24 | AutoGen without an extra | **`instrument=["autogen-agentchat"]` exists, but `cetra-sdk` has no `autogen-agentchat` extra: the user installs `openinference-instrumentation-autogen-agentchat` themselves** (the user, 2026-11-23). The AutoGen instrumentor pulls `autogen-core`, which pins protobuf below 5.30; as an extra it capped the SDK's universal lock (protobuf 7.36 → 5.29). A registry row may name a distribution instead of an extra. Amends D-IN-7. | A `cetra-sdk[autogen-agentchat]` extra (protobuf 5.29 for the whole SDK lock); no registry name (the page would call the instrumentor by hand). |
| D-IN-25 | M5 decisions | **M5's decisions, from the spikes and the user's approvals.** (1) **Pinned binaries.** The user approved Temporal CLI 1.10.1, Bifrost 2.5.1, aigw 1.2.1 and Envoy 1.39.1 (2026-11-23 and 2026-11-24). Temporal, aigw and Envoy come from the vendors' GitHub releases; Bifrost comes from `downloads.getmaxim.ai` (the host Bifrost's own `npx` launcher uses; its GitHub releases carry no binaries). All four are pinned by SHA-256 and fetched by `just capture-tools` into `target/capture-tools/`. Envoy is fetched from `envoyproxy/envoy`, never by `aigw` at run time. (2) **Kong** is setup only, because there is no Docker. (3) **Bifrost's server root span** (scope `bifrost`, no `gen_ai.operation.name`) is `chain`. (4) **A `tool_calls` element with `name`/`args` and no `function` object** is a tool call (Bifrost). (5) **LiteLLM's empty assistant output with `gen_ai.completion.0.function_call.*`** becomes a tool-call output. (6) **`gen_ai.operation.name` `embedding` and `aembedding`** map to `embedding`. (7) **Gateway pages warn that an instrumented app behind a traced gateway counts each call twice,** because both honour `traceparent`. (8) **Azure API Management and Gravitee** go to § Not supported; **Pydantic AI Gateway and Databricks AI Gateway** are merged into their providers' pages. (9) **Deviations from the spike's verdicts:** (a) OrcaRouter and Anannas AI: the spike suggested setup-only OpenAI-client pages in step (4) of each entry but recommended appendix lines; the plan follows the recommendation. No page, one appendix line each. (b) Gravitee: the spike's summary table offered an OpenAI-client page. It goes to § Not supported instead: its spans are HTTP spans without LLM attributes, so there is nothing for a page to show. An OpenAI-compatible endpoint behind Gravitee is traced by the app's own instrumentation, as on the OpenAI page. (10) **LiteLLM Proxy is a pinned tool environment, and `--check` sorts attributes.** `litellm[proxy]` cannot join the capture project (its `mcp` and `rich` pins clash with semantic-kernel's and together's), so it gets its own uv project, `tools/capture/litellm-proxy/`, that `just capture-tools` installs next to the binaries; the scenarios launch it with `tool_binary`. `capture run --check` sorts every span's, event's and link's attributes by key before comparing, because Bifrost (Go) writes them in a random order. (11) **Bifrost's root-span text.** On scope `bifrost`, a `gen_ai.input.messages` or `gen_ai.output.messages` value that is not a JSON array is the field's plain value, stored as a plain `input.value` is stored (JSON text becomes a JSON value, other text stays text), with no normalize error; a JSON array still goes through the message converter. Other scopes are unchanged. | — |
| D-IN-26 | M6 decisions | **M6's decisions, from the spikes and the user's answers (2026-11-24).** (1) **The capture harness is its own uv package** `cetra-capture-harness` (`tools/capture/harness/`, module `capture_py`), which `tools/capture/py` and every environment depend on by path; a Python scenario whose packages cannot share the capture project's lock lives under `tools/capture/envs/<env>/scenarios/`, each environment a uv project with its own `uv.lock` (`package = false`, `exclude-newer` 7 days, the capture project's ruff and mypy settings with a dev group pinning them). (2) **Tool environments** (n8n and Promptfoo as npm workspace projects under `tools/capture/`, Langflow as a uv project like `litellm-proxy`, the `envs/*` projects, and Chromium for Browser Use) are fetched by `just capture-tools` into `target/capture-tools/`. (3) **New fixture groups:** `promptflow` and `langflow` join the design's list; Browser Use, MCP, OpenLIT and Promptfoo go in `genai`, and the OpenLLMetry vector case in `openllmetry`; `check-backing.mjs` accepts `[a-z0-9_]+` (for `n8n`). (4) **Pages with no capture:** setup only for Stagehand, LobeChat, the Weaviate and Pinecone sections of the Vector databases page, Cleanlab, and Harbor and Terminal-Bench; Gradio's setup-only page points at the client library's instrumentors. (5) **Eval tools:** Ragas, UQLM and Azure AI Evaluation get setup-only pages, not SDK-test pages, and their samples are not run (Q4 (a)). (6) **OTLP/JSON enum names stay rejected**; the Harbor page uses `harbor trace export --encoding pb` and a POST of each file. (7) **The `mcp` registry row is added** (OpenInference's `MCPInstrumentor`): it records no spans of its own, joins an `mcp<2` server's spans to the client trace, and `mcp>=2` traces natively. (8) **n8n span names are not rewritten:** the stored name stays `node.execute` / `workflow.execute`; the node name is in `n8n.node.name`. (9) **Scoped content has no convention:** a fixed scope table (`pipecat*`, `livekit-agents`, `promptflow`, `langflow`) in `crates/otel/src/conventions/scoped.rs` fills input and output only where the convention readers left them empty. (10) **Chromium** for Browser Use comes from an explicit `playwright==1.65.2` pin in `envs/browser-use`, locked by `uv.lock`, not checksummed. (11) **Not read, and unit-tested only:** Promptfoo's `promptfoo.request.body` and `promptfoo.response.body`; OpenLIT's Browser Use 0.x spans (R-OL-1, R-OL-3); the Pinecone and Weaviate span-name rows. Captures run offline in a private network namespace with only loopback (`unshare --user --map-root-user --net`, `UV_OFFLINE=1`, a throwaway `/root`), the receiver answers `HEAD` probes on `/v1/traces`, `/v1/metrics` and `/v1/logs` with 200 and an empty body, and `--check` ignores span order and masks span ids when the scenario sets `unordered_spans` (Langflow). (12) Items 13 to 15 record Q1–Q3's answers. (13) **Vector spans (Q1 (a)): reads are `retriever`s, writes stay `span`s.** OpenLIT: `db.operation.name`, lowercased, is `get`, `query` or `search` on its `vectordb` spans. OpenLLMetry: the span-name table on the scopes `opentelemetry.instrumentation.{chromadb,qdrant,milvus,pinecone,weaviate}` (spec 02 § Mapping tables / Span kind). (14) **A reported cost of exactly 0 on a span with input or output tokens counts as no report, for every source** (Q2 (a)): the keys are consumed, their values ignored, and the price table prices the span (spec 02 § Cost computation step 1; amends D109; OpenLIT writes `gen_ai.usage.cost = 0` when its price table has no row). (15) **Morph Reflex and Testable Minds are not supported** (Q3): Morph Reflex is an OpenAI-compatible judge that answers with a bare label, which cetra's judge parser cannot read (D158); Testable Minds polls traces through Langfuse's public API and writes human scores back, and its connector lives on Testable's side. | — |
| D-IN-27 | M7 scope | **M7's scope, from the spike and the user's answers (2026-11-24).** (1) OpenCode: only the v2 build is supported; npm 1.x sends no model spans. (2) Pi becomes first-class through a pi extension that cetra writes and owns: `cetra agents setup pi` installs it in pi's extension folder (`~/.pi/agent/extensions/`, or `$PI_CODING_AGENT_DIR/extensions`), with a marker line so setup and `remove` never touch a file they did not write; the extension emits GenAI semconv spans (`invoke_agent`, `chat <model>`, `execute_tool <name>`, `gen_ai.usage.*`) over the `OTEL_EXPORTER_OTLP_*` environment, so the normalizer needs no Pi rule. LangWatch does the same (their ADR-188). (3) Hermes Agent gets a captured page, not `agents setup` support: its telemetry is a community plugin. (4) Augment Code gets a setup-only page whose Limits say its resource carries the user's email. (5) Qwen Code's logs and metrics are accepted and dropped. (6) OpenClaw is spiked again through its gateway, the route LangWatch documents; the first spike ran the CLI without a gateway and saw no spans. (7) Omnigent is omnigent.ai (PyPI `omnigent`; MLflow lists it) and is spiked. (8) **OpenClaw is first-class** (the user, 2026-11-24). Its spans export only when the `@openclaw/diagnostics-otel` plugin is installed from npm or ClawHub; a local-file install exports nothing. `cetra agents setup openclaw` runs the plugin install, at the version matching the user's OpenClaw, then merges `openclaw.json`. This amends D-CA-10's "setup writes settings only". (9) **Omnigent gets a setup-only page** (the user, 2026-11-24). On its documented route it sends no per-call model spans: one agent span with the turn's token total, and mostly FastAPI/httpx plumbing spans. | Pi setup only (the community extension's `llm.usage.*` keys would need a rule for a third-party format); Hermes first-class (setup cannot install the plugin); OpenClaw not supported (five competitors support it) |
| D-IN-28 | M7 part 1 | **The decisions of `07-m7-coding-agents.md` § Decisions** (the plan, 2026-11-24): detection keys (service `qwen-code`, scope `cetra-pi`, scope `openclaw`); Pi's endpoint and key reach pi through the environment and `cetra agents setup pi` prints the lines (no profile edit, no wrapper); the extension file `cetra.ts` with a marker line; one trace per pi prompt, no system prompt; OpenClaw setup runs the plugin install matching `openclaw --version`; OpenClaw metrics and logs off; Qwen hook events `SessionStart` and `UserPromptSubmit`, timeout in seconds; the pinned versions win, later pins follow the rolling 7-day rule; no `agents.toml` for Pi. | A `cetra pi` wrapper (a new command and process wrapper); a shell-profile edit |

## Milestones

The milestones run in order. Every milestone that adds libraries edits the capture projects' lockfiles (`tools/capture/py/uv.lock`, `tools/capture/ts/pnpm-lock.yaml`), the index, the sidebar and `CHANGELOG.md`, so two milestones never run at once. Phases inside a milestone may run in parallel where its plan's dependency graph allows. Every milestone adds its pages to the index and sidebar, its decisions to `mvp-decisions.md`, a `CHANGELOG.md` line, and updates the track's roadmap row.

**M1 — Provider attribution.** Delivers:
- calls through the OpenAI client to the providers in the host table get the right provider (D-IN-2);
- the aliases and the framework-name table (D-IN-15);
- the converter fix and a refreshed snapshot (D-IN-14);
- gateway pricing (D-IN-13);
- host routing in the capture harness and suffix path matching in the mock (D-IN-3, D-IN-4), with captured cases for OpenRouter and Novita through OpenInference (via cetra's SDK) and OpenLLMetry (four cases, D-IN-19), plus unit tests for every host table row;
- the Python SDK host stamp (D-IN-18); the OpenInference cases go through cetra's SDK wiring so they pin the stamp.

Spikes: done (`spikes/m1-*.md`). Depends on: nothing. Leaves out: the pages (M2).

**M2 — Providers.** Delivers:
- a page for every provider in § Providers and the long-tail providers of the appendix;
- the index regrouped (Providers, Gateways, Frameworks, Coding agents, Apps and no-code, Voice, Browser agents, Tools and data, Evaluation, Languages, Instrumentation libraries, Migrating), and the sidebar likewise;
- `check-integrations.mjs` (D-IN-8);
- the Cohere and Ollama routes of D-IN-4, with one captured case per native SDK (Groq, Mistral, Cohere, Together, Ollama, Azure, Vertex);
- the registry rows for Groq, Mistral, Cohere, Ollama and Together (D-IN-7).

Depends on: M1. Leaves out: gateways (M5).

**M3 — OpenInference frameworks.** Delivers pages, registry rows and captured cases for Instructor, Agno, smolagents, Haystack, BeeAI (Python and TypeScript), Guardrails AI, DeepAgents and Temporal, and own pages for LangGraph and the Claude Agent SDK. No normalizer work. A capture that shows a gap moves that integration to M4, and this doc says so. Depends on: M2 (lockfiles and index).

**M4 — Native GenAI frameworks.** Delivers captured cases, the D-IN-5 and D-IN-6 rules and pages for Pydantic AI, Mastra, Semantic Kernel (Python), Strands Agents and Bedrock AgentCore, Microsoft Agent Framework (Python), AG2, AutoGen, Mirascope, the OpenHands SDK and VoltAgent (with `Convention::Voltagent`). It also fixes OpenInference `output.value` labelled `application/json` that is not JSON (Agno's agent spans): read it as text, without a normalize error (found by M3's capture). Spikes: none (the research read their source); the captures pin the keys. Depends on: M3. Leaves out: .NET and Java (M8).

**M5 — Gateways.** Delivers:
- a page for every gateway in § Gateways that its spike keeps;
- LiteLLM Proxy's provider ids and reported cost (D-IN-15, D-IN-16), with a captured case from the proxy running against the mock;
- captured cases for Bifrost, Envoy and Kong where their spikes show they run locally against the mock, and setup-only pages otherwise.

Spikes: Portkey, TrueFoundry, Kong, Cloudflare AI Gateway, Bifrost, Envoy, Azure API Management and the long-tail gateways. Depends on: M4.

**M6 — Apps, voice, browser agents, tools and evaluation.** Delivers pages for n8n, Langflow, Prompt Flow, LiveKit Agents, Pipecat, Browser Use, Stagehand, OpenLIT, MCP (and its registry row), vector databases, Ragas and Promptfoo, plus the spike-approved items of § Apps. It adds captured cases for each item that runs locally, and the D-IN-5 rules for Pipecat, LiveKit and vector databases. Spikes: n8n, Langflow, Stagehand, OpenLIT, Harbor, and every "spike decides" item. Depends on: M5.

**M7 — Coding agents.** Delivers first-class support for OpenCode and Qwen Code (D-IN-12): captures from pinned builds, detection, discard rules where noise spans need them, `cetra agents setup` and `remove`, the hook where the agent has one, and the dashboard template. Goose and Hermes Agent get the same if their spikes pass, else a page. Pi gets the same through a cetra-owned pi extension (D-IN-27). Hermes Agent gets a captured page; Augment Code, Omnigent and the Devin SDK get setup-only pages; OpenClaw is first-class (D-IN-27). Spikes: done (`spikes/m7-coding-agents.md`); OpenClaw (through its gateway) and Omnigent are spiked again (D-IN-27). Depends on: M6.

**M8 — Other languages.** Delivers:
- the Java and .NET capture runners (D-IN-9);
- captured cases for Spring AI, LangChain4j, Quarkus LangChain4j, Koog, Semantic Kernel .NET, Microsoft Agent Framework .NET and Microsoft.Extensions.AI;
- the normalizer rules those captures show (Spring AI's content events, Semantic Kernel .NET's older events, `spring_ai` in the framework-name table);
- pages for each, and setup-only pages for Go, Ruby, Rust and C++.

Depends on: M7.

**M9 — Long-tail frameworks.** Delivers a page for each long-tail framework in the appendix whose spike shows an OTLP path: a captured case when it runs offline, setup only otherwise. The others move to § Not supported, and the user is told. Depends on: M8.

## User answers

The user, 2026-11-23:

- **Q1. Unsupported integrations:** recorded in this design doc only, not on the site (D-IN-10).
- **Q2. Java and .NET:** capture runners, captured cases and the normalizer fixes they show. Go, Ruby and Rust are setup only (D-IN-9).
- **Q3. Paid or cloud-only integrations:** listed with setup-only steps (D-IN-11).
- **Q4. New coding agents:** full setup support. The cold review's split is recorded in D-IN-12: OpenCode and Qwen Code now; Goose and Hermes Agent after their spikes.

## Open questions

None open. Spike results that change the scope go back to the user (D-IN-17).

## Appendix: every other matrix row

The matrix rows not named above, with their disposition. "Spike" means the milestone's spike decides between a page and § Not supported.

| Row (count) | Disposition |
|---|---|
| TypeSafe Jev (5) | Unidentified name in the matrix; M2 identifies it as TypeSafe, a decision-model API, and moves it to § Not supported (D-IN-21) |
| BytePlus (3), Baseten, DeepInfra, Hyperbolic, MiniMax, NVIDIA NIM (and Dynamo), Sarvam AI, Qianfan, CometAPI, Doubleword | M2, OpenAI-compatible provider pages. Predibase, PrismML, Anyscale and Meta Llama API move to § Not supported (D-IN-21) |
| aisuite | M2: no spans of its own; its page uses the provider instrumentors |
| Claude Platform (Anthropic OTel), OpenAI Assistants API | merged into the Anthropic and OpenAI pages, each as a section |
| eve (3), AgentSpec, Atomic Agents, Claude Managed Agents, Cloudflare Agents, Flue, Graphite, Inferable, LangServe, mcp-use, MetaGPT, NVIDIA NeMo Agent Toolkit, Pi Durable, TanStack AI, txtai, watsonx Orchestrate, Restate, Embedchain, Pixeltable, EverOS, Weco, Xcode, Upstash QStash, Upstash RAG Chat, Cognee | M9, spike |
| Arconia, Embabel | M8 (Java) |
| Swiftide, AI SDK C++ | M8, setup only |
| OpenClaw (3), Augment Code, Oh My Pi, Omnigent, Open Devin | M7 (D-IN-27; Open Devin is the old name of OpenHands: merged into the OpenHands SDK page) |
| Gradio, Langdock, LobeChat, Ragflow, Scalekit, Bubble | M6, resolved (D-IN-26 item 4): Gradio's setup-only page points at the client library's instrumentors, and LobeChat's setup-only page shows its telemetry variables; Langdock, Ragflow, Scalekit and Bubble move to § Not supported |
| Pydantic AI Gateway, Databricks AI Gateway | M5: merged into the Pydantic AI and Databricks pages (D-IN-25) |
| OrcaRouter | no page and no host row: an OpenAI-compatible router with no OTLP export (D-IN-25) |
| Anannas AI | no page and no host row: an OpenAI-compatible router with no OTLP export (D-IN-25) |
| Trieve | M6, resolved: § Not supported, stale (Helicone lists it as stale; D-IN-26) |
| Hugging Face Datasets, ClickHouse / ClickStack | not trace sources: out of scope |
| Slack, PagerDuty, Mixpanel, PostHog, Trubrics | destinations or analytics, not trace sources: out of scope (Slack alerts already exist, track K) |
| MLflow AI Gateway, Opik LLM Gateway, Arize / Phoenix, MLflow, Langfuse (as a backend) | competitors' own products: out of scope; migration pages for Langfuse and Laminar already exist |
