# Track IN — Integrations: design

Status: **draft, under review with the user** (2026-11-23). Decisions `D-IN-<n>`. The user answered Q1–Q4 on 2026-11-23 (§ User answers).

## Summary

cetra documents 19 integrations; the seven competitors together document about 180 more (`int-competitor-matrix.md` in the session notes, summarised below). The user asked to support all of them (2026-11-23). Integration listings are the main distribution channel before launch, so each supported integration gets its own page, a tested sample where one can be tested, and captured spans where the library runs offline.

cetra ingests OTLP, so most integrations are documentation. The research found four kinds of real work behind the pages:

1. **Provider attribution.** OpenAI-compatible providers (OpenRouter, Ollama, vLLM, Novita, DashScope, Hugging Face, Databricks, Cloudflare, SambaNova, Nebius, every gateway) used through the OpenAI client are recorded as provider `openai` by OpenInference, OpenLLMetry and the official OTel instrumentation. Cost is then null, or comes from an `openai` row with the same model name. This is the largest correctness gap and blocks most provider pages.
2. **Normalizer gaps in native GenAI frameworks.** Pydantic AI, Mastra, Strands, Microsoft Agent Framework, AG2, Semantic Kernel, Mirascope, Pipecat and LiveKit emit `gen_ai.*`, but use operation names, token keys, TTFT keys or input/output keys that spec 02 does not read. VoltAgent uses its own keys and is not read at all.
3. **Capture infrastructure.** The capture mock serves only the OpenAI, Anthropic and Gemini APIs on `127.0.0.1`. Native SDKs (Cohere, Ollama, Azure, Vertex) need new routes, provider attribution needs the real hostnames to reach the mock, and Java and .NET need runners.
4. **Integrations that cannot be supported** with spans-only OTLP (D-CA-2) or at all: listed in § Not supported.

## Goals

- Every integration that at least one competitor documents and that can send OTLP spans to cetra has a page under `website/src/content/docs/docs/integrations/`.
- Each page says what it is tested against: captured spans, an SDK test, or setup steps only. No page claims more than its tests show.
- Spans from each documented integration normalize into the right kinds, model, provider, tokens, cost, input and output, and session, as far as the library sends them.
- The user knows which integrations are not supported, and why.

## Non-goals

- New cetra SDKs for Java, Go, .NET, Ruby or Rust. Each language's own OpenTelemetry SDK exports to cetra (D-IN-9).
- OTLP logs and metrics (D-CA-2 stands): Cursor, Kiro, Cline, E2B and Modal export only those, and wait for a logs-and-metrics track.
- Proxy or gateway ingestion by cetra itself (D4 stands).
- Importing traces from a competitor's API. The Langfuse and Laminar pages keep their SDKs and export spans.
- Evaluation-library integrations beyond tracing their LLM calls: Ragas and Promptfoo pages show how to trace them and how to send their results as scores with the existing SDK, nothing more.
- Marketing pages, logos and docs screenshots (these wait until the product is frozen).

## The integration list

Counts are how many of the seven competitors list it (Langfuse, Phoenix, Opik, LangWatch, Laminar, Helicone, MLflow). The route is what cetra's page uses. "Gap" names normalizer work.

### Providers

| Integration | Count | Route | Gap |
|---|---|---|---|
| OpenRouter | 7 | OpenAI client, base URL | attribution |
| Groq | 6 | OpenAI client; `openinference-instrumentation-groq` | price rows missing from the snapshot |
| Ollama | 6 | OpenAI client on localhost; `openinference-instrumentation-ollama` | attribution (localhost) |
| xAI Grok | 6 | OpenAI client | — |
| Cohere | 5 | `openinference-instrumentation-cohere` | mock route `/v2/chat` |
| DeepSeek | 5 | OpenAI client | — |
| Fireworks AI | 5 | OpenAI client | alias `fireworks` |
| Mistral | 5 | `openinference-instrumentation-mistralai` | — |
| Novita AI | 5 | OpenAI client | attribution, price rows |
| Together AI | 5 | OpenAI client; `openinference-instrumentation-together` | alias `together`, `togetherai` |
| Google Vertex AI | 4 | Google Gen AI SDK with Vertex | mock route for Vertex paths |
| Azure OpenAI | 2+ | OpenAI client (`AzureOpenAI`) | mock route for deployments; deployment names as models |
| Qwen (DashScope) | 3 | OpenAI client | attribution |
| Cerebras, Perplexity | 3, 2 | OpenAI client | — |
| vLLM | 2 | OpenAI client on localhost | attribution (self-hosted) |
| Hugging Face Inference Providers | 2 | OpenAI client | attribution, price rows |
| Cloudflare Workers AI | 2 | OpenAI client | attribution, price rows |
| Databricks | 2 | OpenAI client | attribution |
| Moonshot (Kimi), Z.ai (GLM), SambaNova, Nebius | 1–2 | OpenAI client | attribution |
| IBM watsonx | 2 | OpenLLMetry `opentelemetry-instrumentation-watsonx` | alias `watsonx`, `ibm`; new mock route |

### Gateways

| Integration | Count | Route | Gap |
|---|---|---|---|
| LiteLLM Proxy | 5 | the proxy's `otel` callback | LiteLLM provider ids, `gen_ai.cost.*` |
| Portkey | 4 | OpenAI client, or Portkey's OTLP export (enterprise and self-hosted) | provider values unverified |
| TrueFoundry | 4 | OpenAI client, or its OTLP export | convention unverified |
| Vercel AI Gateway | 4 | AI SDK or OpenAI client (no OTLP export) | attribution |
| Kong AI Gateway | 3 | Kong's `opentelemetry` plugin (GenAI semconv) | provider values unverified |
| Helicone | 3 | OpenAI client (no OTLP export) | attribution |
| Cloudflare AI Gateway | 1 | OpenAI client, or its OTLP export | `gen_ai.model.provider`, `gen_ai.prompt_json`, `gen_ai.completion_json` |
| Bifrost | 1 | `otel` plugin, `trace_type: genai_extension` | provider values unverified |
| Envoy AI Gateway | 1 | OpenInference or GenAI semconv | — |
| Azure API Management | 1 | unverified; may log to Application Insights only | decided in M5's spike |

### Agent frameworks

| Integration | Count | Route | Gap |
|---|---|---|---|
| Claude Agent SDK (Python, TS) | 6 | Claude Code's native spans (D-CA-13) | — (own page split from Claude Code) |
| Instructor | 6 | OpenInference | — |
| Mastra | 6 | `@mastra/otel-exporter` | span types, `mastra.completion_start_time` |
| Pydantic AI | 6 | native GenAI | agent span input/output in `pydantic_ai.all_messages`, `final_result` |
| Semantic Kernel (Python) | 6 | native GenAI | `text_completions` |
| Agno | 5 | OpenInference | — |
| AutoGen (agentchat) | 5 | OpenInference | `gen_ai.system=autogen` read as provider |
| Haystack | 5 | OpenInference | — |
| Microsoft Agent Framework (Python) | 5 | native GenAI | workflow operations |
| smolagents | 5 | OpenInference | — |
| Strands Agents | 5 | native GenAI | loop operations, `gen_ai.server.time_to_first_token` |
| BeeAI (Python, TS) | 4 | OpenInference | — |
| Temporal (with the OpenAI Agents SDK) | 4 | OpenInference plus Temporal's tracing interceptor | Temporal spans |
| AG2 | 3 | native middleware | `thinking_tokens`, `record_usage` spans |
| Bedrock AgentCore | 3 | Strands code; the AWS runtime is untested | as Strands |
| DeepAgents | 3 | OpenInference LangChain | — |
| LangGraph | 2+ | OpenInference LangChain | — (own page split from LangChain) |
| VoltAgent | 3 | its own keys | new convention (D-IN-6) |
| Mirascope | 1 | native GenAI | `mirascope.trace.*`, `mirascope.ops.session.id` |
| OpenHands SDK | 1 | Laminar SDK over OTLP | `gen_ai.usage.cost` on its spans |
| Guardrails AI | 2 | OpenInference | — |

### Coding agents

| Integration | Count | Route | Gap |
|---|---|---|---|
| OpenCode | 4 | experimental OpenTelemetry flag (AI SDK spans) | spike: span names, noise spans |
| Hermes Agent | 3 | community plugin (GenAI and OpenInference) | spike |
| Goose | 2 | native, OTLP HTTP | no GenAI attributes seen: spike |
| Qwen Code | 1 | native GenAI (D-CA-17) | already normalizes generically |
| Pi | 1 | community extension | not capturable (hard-coded provider URLs) |
| Devin | 1 | Devin SDK tracing only | the hosted product is not instrumented |

### Apps, no-code, voice, browser agents and tools

| Integration | Count | Route | Gap |
|---|---|---|---|
| n8n | 6 | native OTLP (2.19 and later) | spike: span names and attributes |
| Langflow | 5 | Traceloop environment variables | spike |
| Prompt Flow | 3 | native OTLP | — |
| LiveKit Agents | 4 | native GenAI plus `lk.*` | content in `lk.pii.*` (Node) |
| Pipecat | 4 | native GenAI | input and output in plain `input`, `output` |
| Browser Use | 1 | OpenLIT or Laminar | — |
| Stagehand | 1 | native (v4) | no GenAI attributes: spike |
| OpenLIT | — | instrumentation library (GenAI semconv) | `gen_ai.content.*` events (check) |
| MCP | — | Python MCP SDK native, OpenInference, OpenLLMetry | — |
| Vector databases (Qdrant, Milvus, Chroma, Pinecone, Weaviate) | 1 | OpenLLMetry or OpenLIT | `db.*` spans as `retriever` |
| Ragas, Promptfoo | 4, 1 | trace the LLM calls; send results as scores | — |
| Harbor | 3 | the agent's own tracing inside the container | spike |
| Daytona | 1 | SDK spans | infrastructure spans only |

### Other languages

| Language | Integrations | Route |
|---|---|---|
| Java | Spring AI, LangChain4j, Quarkus LangChain4j, Koog (Kotlin) | Spring AI's Micrometer GenAI spans (`gen_ai.system=spring_ai`, content in events); OpenInference Java for LangChain4j; Quarkus and Koog native |
| .NET | Semantic Kernel, Microsoft Agent Framework, Microsoft.Extensions.AI | native GenAI (older event shape in Semantic Kernel) |
| Go | Genkit Go, openai-go, langchaingo | OpenInference Go, OpenLIT Go, Genkit native |
| Ruby | ruby_llm | its OTel gem (GenAI) |
| Rust | rig | native GenAI |

### Not supported

None of these can send spans to cetra today. They are recorded here only; the public docs do not list them (D-IN-10):

| Integration | Why |
|---|---|
| Cursor (CLI and IDE), Kiro | logs and metrics only, Enterprise admin export (D-CA-1, D-CA-2) |
| Cline | logs and metrics only |
| E2B, Modal | platform logs and metrics only |
| Antigravity, Roo Code, Aider, Amp, Crush, Skyvern | no OpenTelemetry export found |
| Zapier, Vapi, Retell, LibreChat | no OTLP export; vendor-specific (Langfuse) or none |
| ElevenLabs Agents | OTLP-shaped JSON through a webhook; needs a forwarder |
| Dify, Flowise, Open WebUI | their OTLP export carries HTTP and database spans, not LLM spans; LLM tracing is a per-vendor UI setting |
| Playwright, Puppeteer | browser automation, not LLM libraries; covered through Browser Use and Stagehand |
| Devin (hosted product) | not instrumented; the Devin SDK is |
| langchainrb, PHP libraries, the Rust `genai` crate | no OTLP path found |

## Architecture

The parts this track changes and how they connect. Nothing changes in storage, the server API or the UI.

```
integration ──OTLP──▶ cetra-otel normalize ──▶ storage
                         │  provider attribution (D-IN-2)
                         │  new operation names, keys, VoltAgent convention (D-IN-5, D-IN-6)
tools/capture ──▶ fixtures/otlp/<group>/<case> ──▶ cetra-otel golden test
   │ host routing to the mock (D-IN-3), new mock routes, Java and .NET runners (Q2)
sdks/python registry ──▶ instrument=["groq", "agno", …] (D-IN-7)
website integrations pages ──▶ check-samples, check-backing, index table, sidebar (D-IN-1, D-IN-8)
```

### Contracts

- **Spec 02** is the contract for every normalizer change: § Convention detection gains `voltagent`; § Mapping tables gain the new operation names, token, TTFT, input/output and session keys; rule 12's alias table gains the provider ids; a new subsection "Provider from the endpoint" defines D-IN-2. `cetra-core`'s `Convention` enum gains `voltagent` (a shared contract: this track's brief allows that one change).
- **Fixtures**: new groups only where a library's own convention is new (`voltagent`, and one per native framework whose spans are captured: `pydantic_ai`, `mastra`, `strands`, …; the plans fix the names). Provider-attribution cases go in the existing `openinference`, `openllmetry` and `genai` groups.
- **Capture scenarios** gain an optional `host` per turn (D-IN-3) and new `api` values for the new routes. The scenario schema in `tools/capture/src/scenario.rs` is the contract.
- **Python SDK**: new registry rows and extras. TypeScript gets no new helpers; its pages use each library's own exporter or `@cetra-ai/sdk`'s tracer provider.
- **Docs**: one page per integration (D-IN-1), the index table, the sidebar in `astro.config.mjs`, the `<Backing>` box where fixtures or tests exist.

## Decisions

| ID | Topic | Decision | Rejected |
|---|---|---|---|
| D-IN-1 | Pages | **One page per integration**, including each OpenAI-compatible provider, under `integrations/`. Short pages share one shape: install, set up, what you get, limits, next. A framework page links to the provider pages for the model calls. | One "OpenAI-compatible providers" page: shorter to maintain, but the listings, search and the competitors' parity all count pages. |
| D-IN-2 | Provider attribution | **`cetra-otel` derives the provider from the endpoint when a span names `openai` (or nothing) as provider and carries the server host**: `server.address` (GenAI semconv, OpenAI v2 instrumentation), `url.full`, `gen_ai.openai.api_base` (OpenLLMetry), or OpenInference's equivalent (M1's spike confirms which keys each instrumentation writes). A fixed host table maps known hosts to provider names (`openrouter.ai` → `openrouter`, `api.groq.com` → `groq`, port `11434` on a local host → `ollama`, …). An explicit provider other than `openai` always wins. A local or unknown host leaves the provider as sent, and track T's custom prices still price it. | Inferring from the model name when the provider is `openai`: wrong for OpenRouter's `vendor/model` names and for open models served everywhere. An SDK-side span processor: helps only cetra's SDK users, not plain OpenInference or OpenLLMetry. |
| D-IN-3 | Capturing real hosts | **A capture scenario may name the real host of a turn; the capture tool routes that host to the mock** (a client transport or proxy in the harness; the plan picks one), so the recorded spans carry `api.groq.com`, not `127.0.0.1`. | Capturing against `127.0.0.1` with a base URL: the spans would not show the attribution the page claims. |
| D-IN-4 | Mock routes | **New mock routes only for native wire formats that pages document**: Cohere `/v2/chat`, Ollama `/api/chat`, Azure `/openai/deployments/{d}/chat/completions`, Vertex `…/publishers/google/models/*:generateContent`, watsonx. OpenAI-compatible providers reuse `/v1/chat/completions`. | A generic record/replay proxy: hides wire-format mistakes the mock catches. |
| D-IN-5 | Native GenAI gaps | **Spec 02 reads the extra keys the research found**: operation names mapped to kinds (Strands' loop and decision operations, Microsoft Agent Framework's executor and workflow operations, Semantic Kernel's `text_completions`, AG2's `await_human_input`); `gen_ai.usage.thinking_tokens` as reasoning; `gen_ai.server.time_to_first_token` and `mastra.completion_start_time` as TTFT; Pydantic AI's agent messages and final result as the agent span's input and output; Mirascope's trace input/output and session id; Pipecat's `input`/`output`; framework names in `gen_ai.system` (`autogen`, `spring_ai`, …) skipped like `gcp.vertex.agent`. Each rule is backed by a captured fixture, never only by documentation. | Leaving them as `span`: the pages would show traces without kinds or I/O. |
| D-IN-6 | VoltAgent | **A new convention `voltagent`**, detected by `voltagent.*` or `entity.*` keys, reading its kinds, model, tokens and messages; its `llm.*` keys stop counting as OpenInference on spans that carry `voltagent.*`. | An SDK adapter: VoltAgent users do not use cetra's SDK. Not supporting it: three competitors list it. |
| D-IN-7 | Python registry | **Every OpenInference instrumentor a page documents becomes an `instrument=` name and an extra** (`groq`, `mistralai`, `cohere`, `ollama`, `together`, `agno`, `smolagents`, `haystack`, `instructor`, `autogen-agentchat`, `beeai`, `guardrails`, `mcp`), with the SDK test of the existing rows. Native-OTel frameworks get no registry row; their page sets up the tracer provider with `cetra.init()`. | Pages that install OpenInference by hand: two ways to do one thing for SDK users. |
| D-IN-8 | Test tiers | **Three tiers, shown in the index's "Tested against" column**: captured (spans recorded from the real library against the mock), SDK test (the registry import test), setup only (no recorded spans; the page says so in Limits, with no `<Backing>` box). Samples that need a provider key or a cloud account are marked `{/* untested: … */}` as today; no test calls a real provider. | Live provider tests in CI: needs paid keys and is flaky; `just eval-live` stays the only live suite (D167). |
| D-IN-9 | Other languages | **One page per framework or language ecosystem with the language's OTel SDK and the framework's native GenAI spans; no new cetra SDK. Java and .NET get capture runners in `tools/capture` and captured cases; Go, Ruby and Rust pages are setup only** (the user, 2026-11-23). | New cetra SDKs per language: a large surface to maintain for listings that plain OTel already satisfies. |
| D-IN-10 | Not supported | **Integrations that cannot send spans are recorded in this design doc only, not on the public site** (the user, 2026-11-23). No page pretends to support them. | A "Not supported yet" section on the index: the user preferred the site to list only what works. |
| D-IN-11 | Paid or cloud-only | **Integrations whose cloud side needs a paid account are listed with setup-only steps** (the user, 2026-11-23): Bedrock AgentCore's runtime, Vertex AI, Azure OpenAI, Databricks, Portkey's and TrueFoundry's OTLP export, Kong Enterprise. The model-call side is captured against the mock where it can be; the page's Limits say which part is untested. | Leaving them out until someone can test them. |
| D-IN-12 | New coding agents | **OpenCode, Goose, Hermes Agent and Qwen Code become first-class coding agents as in track CA** (the user, 2026-11-23): `cetra agents setup <agent>` and `remove`, detection in `cetra-otel` with `metadata['coding_agent']`, `repository` and `branch` (D-CA-3, D-CA-4), the session-context hook where the agent has hooks, and the agents in the Coding agents dashboard template. This supersedes D-CA-17's "not added". An agent whose spike shows no usable spans (Goose shows no GenAI attributes yet) gets a page only, and the user is told. | Pages only, with setup later. |

## Milestones

Ordered by how many competitors list them, and grouped by the work they need. Every milestone adds its pages to the index and sidebar, a `CHANGELOG.md` line, and its rows to `docs/roadmap.md`.

**M1 — Provider attribution.** Delivers: calls through the OpenAI client to OpenRouter, Groq, Ollama, Novita, DashScope, Hugging Face, Cloudflare, Databricks, SambaNova, Nebius, Moonshot and Z.ai get the right provider and cost; the alias table gains Fireworks, Together, watsonx, IBM, Oracle and LiteLLM's provider ids; the price snapshot's missing providers (Groq has no rows) are explained or fixed in the converter. The capture tool routes real hosts to the mock (D-IN-3), and captured cases pin attribution for OpenInference, OpenLLMetry and OpenAI v2. Depends on: nothing. Leaves out: the provider pages (M2).

**M2 — Providers.** Delivers: a page for every provider in § Providers, the index regrouped (Providers, Gateways, Frameworks, Coding agents, Apps, Voice, Browser agents, Tools, Languages, Instrumentation libraries, Migrating), the mock routes of D-IN-4 with their captured cases, and the registry rows for Groq, Mistral, Cohere, Ollama and Together. Depends on: M1. Leaves out: gateways (M5).

**M3 — OpenInference frameworks.** Delivers: pages, registry rows and captured cases for Instructor, Agno, smolagents, Haystack, AutoGen, BeeAI (Python and TypeScript), Guardrails AI, DeepAgents and Temporal; own pages for LangGraph and the Claude Agent SDK. No normalizer work expected; a gap found by a capture is fixed in M4. Depends on: nothing (runs in parallel with M1: no shared files except the index, merged in order). Leaves out: native-OTel frameworks.

**M4 — Native GenAI frameworks.** Delivers: captured cases, the D-IN-5 normalizer rules and pages for Pydantic AI, Mastra, Semantic Kernel (Python), Strands Agents and Bedrock AgentCore, Microsoft Agent Framework (Python), AG2, Mirascope and the OpenHands SDK; the VoltAgent convention (D-IN-6) and its page. Depends on: M1 (both change the normalizer; run in order). Leaves out: .NET and Java (M8).

**M5 — Gateways.** Delivers: pages for every gateway in § Gateways; normalization of LiteLLM Proxy's and Cloudflare AI Gateway's keys; captured cases for the gateways that run locally against the mock (LiteLLM Proxy at least; the plan decides Bifrost, Envoy and Kong from a spike); a spike on Azure API Management. Depends on: M1, M2.

**M6 — Apps, voice, browser agents and tools.** Delivers: pages for n8n, Langflow, Prompt Flow, LiveKit Agents, Pipecat, Browser Use, Stagehand, OpenLIT, MCP, vector databases, Ragas, Promptfoo, Harbor; captured cases and normalizer rules where a spike shows a gap (Pipecat, LiveKit, Stagehand, vector-database spans as `retriever`). Depends on: M4 (normalizer, in order).

**M7 — Coding agents.** Delivers: pages for OpenCode, Goose, Hermes Agent, Qwen Code, Pi and the Devin SDK; captured cases and normalization for the ones a spike shows need it (OpenCode's noise spans, as for Codex); `cetra agents setup`, detection, metadata and dashboard-template support for OpenCode, Goose, Hermes Agent and Qwen Code (D-IN-12). Depends on: M4. Leaves out: Pi and the Devin SDK as first-class agents (pages only).

**M8 — Other languages.** Delivers: pages for Java (Spring AI, LangChain4j, Quarkus, Koog), .NET (Semantic Kernel, Microsoft Agent Framework, Microsoft.Extensions.AI), Go, Ruby and Rust, and Java and .NET capture runners with captured cases (D-IN-9) and the normalizer rules they show (Spring AI's events, Semantic Kernel .NET's older events). Depends on: M4.

M1 and M3 can run in parallel. M2 follows M1; M4 follows M1; M5 follows M2; M6, M7 and M8 follow M4 and can run in parallel with each other when their normalizer edits do not overlap (each plan says).

## User answers

The user, 2026-11-23:

- **Q1. Unsupported integrations:** recorded in this design doc only, not on the site (D-IN-10).
- **Q2. Java and .NET:** capture runners, captured cases and the normalizer fixes they show; Go, Ruby and Rust setup only (D-IN-9).
- **Q3. Paid or cloud-only integrations:** listed with setup-only steps (D-IN-11).
- **Q4. New coding agents:** full setup support for OpenCode, Goose, Hermes Agent and Qwen Code (D-IN-12).
