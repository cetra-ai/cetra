# M9 spike (part C): eight long-tail platforms

Run 2026-11-24 (track IN, M9), worktree `integrations` (branch `claude/in-integrations`, HEAD `8dab8de`). Node v26.13.0 (npm `--before=2026-11-17T00:00:00Z` for runs; `npm pack` with explicit versions below). Evidence dir: `/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m9/c/` (`dl/` tarballs, `pydl/` wheel, `restate/` server binary, `rs-*` and `an-*` runs, `x/` extracted packages).

Runs use the M7 loopback harness (`scratchpad/m7/tools/netrun.sh`, `server.py` fake OpenAI/Anthropic provider and OTLP sink, `h.py` = cetra's real normalizer). Fake key `sk-fake-m9`. Every run printed `real-home touches: none` and ran in a user+network namespace with only loopback.

Tags: **run** = observed by a command in this spike (evidence path given); **docs** = read from the item's docs or package (URL or file:line); **search** = seen only in a web search snippet, not read; **unverified** = not established.

## Verdicts

| Item | Identity and pin (rule 2) | OTLP route | Verdict |
|---|---|---|---|
| Claude Managed Agents | Anthropic hosted product (beta, header `managed-agents-2026-04-01`). Its SDK side: `@anthropic-ai/sdk` **1.9.0** (2026-11-16) | The Claude Platform exports API spans from Anthropic's servers to a Console or Admin-API destination (OTLP/HTTP, public HTTPS endpoint). The SDK (1.9.0) emits its own spans in the app | **(b) setup only** for the Managed Agents session spans. The SDK's own spans were **run** and normalize well (`anthropic.messages.create` as `llm`, tokens, cost). Suggest a section of the Anthropic page, not a page of its own (decision for the user) |
| Cloudflare Agents | npm `agents` **0.33.0** (2026-11-03), the Cloudflare Agents SDK (`cloudflare/agents`). Newer 0.34.0 (2026-11-20) is post-cut | Workers OpenTelemetry export: an account-level Cloudflare destination set in the dashboard, OTLP/HTTP **JSON** only. `wrangler dev` does not forward spans to destinations (MLflow doc) | **(b) setup only**. Not runnable offline: the export path is cloud-side. Gen-AI spans exist (`wrapAISDK`, docs file:line below) |
| Pi Durable | npm `@earendil-works/pi-durable` **1.5.2** (2026-11-10), from `earendil-works/pi`, described as "Durable conversation, task, and document runtime for Pi". Experimental. Pairs with `agents` 0.33.0 (its peer is `~1.5.0`) | None. No OTLP or OpenTelemetry code in the package. Telemetry is an in-process adapter and an `onReport` callback | **(c) not supported** |
| watsonx Orchestrate | PyPI `ibm-watsonx-orchestrate` **2.20.0** (2026-11-11), IBM ADK and CLI (`orchestrate`) | Developer Edition (Docker) sets `FLOW_TRACING_OTLP_ENDPOINT` (empty by default; `default.env:262`); the platform's trace export is a CLI pull (`orchestrate observability traces`), and an inbound OTLP ingest that is IBM's endpoint. Outbound export from SaaS is not confirmed in IBM's docs | **(b) setup only, unverified** (see open question 4) |
| Restate | Server `restate-server` **v1.8.1-hotfix.2** (GitHub release 2026-11-02, sha256 verified against the release's own `.sha256`). TS SDK `@restatedev/restate-sdk` **1.18.3** (2026-11-04) | Yes: `RESTATE_TRACING_ENDPOINT=otlp+http://host:port/v1/traces` (OTLP/gRPC for `http(s)://`). Runtime spans only (`ingress`, `invocation-start`, `invocation-attempt`, `invocation-end`), with no GenAI attributes | **(c) not supported as an LLM source**. Runs offline, but its spans carry no model, token or tool data. LLM spans come from the framework's instrumentor (Phoenix's Restate page). Judgment call, open question 2 |
| Xcode (AI features) | Apple Xcode 26.3+ agentic coding (Claude Agent SDK and Codex integrations). macOS only | None documented. A search found nothing on OpenTelemetry (search) | **(c) not supported**, unverified. Cannot be run here |
| Upstash QStash | npm `@upstash/qstash` **2.12.2** (2026-11-12). HTTP message queue | None. Only SDK telemetry headers to Upstash's cloud (`telemetryHeaders`, `cloudflare.js:301-309`). No OTLP | **(c) not supported**. Not a trace source (same class as Slack and PagerDuty) |
| Upstash RAG Chat | npm `@upstash/rag-chat` **2.0.3** (registry publish 2024-11-20). Depends on LangChain (`@langchain/openai`, `anthropic`, `mistralai`), `ai` 3.x, `langsmith`, `@upstash/vector` | None of its own. Model calls go through LangChain and `ai`; its tracing is LangSmith (`globalTracer`, `project_name: "Upstash Rag Chat"`), not OTLP | **Duplicate**: no page. Its model calls are covered by the LangChain and Vercel AI SDK instrumentors cetra already documents. Not run (needs an Upstash Vector account or a mock) |

## Findings that change the design

1. **Anthropic's SDK emits its own GenAI spans now, and they normalize well.** `@anthropic-ai/sdk` 1.9.0 creates `anthropic.messages.create` spans (scope `com.anthropic.sdk.typescript@1.9.0`) with `gen_ai.*` attributes, token counts and `anthropic.message.stop_reason`. cetra reads them as `llm` with provider `anthropic`, tokens and a computed cost (run: `/tmp/.../m9/c/an-run/recv/`, normalizer output in § Claude Managed Agents, SDK side). Phoenix warns not to run the OpenInference Anthropic instrumentor on the same client, because each request then appears twice (`platforms/claude-managed-agents/claude-managed-agents-tracing.mdx:115`). **Open question 6:** the Anthropic page must say this. First version that emits spans was not checked (only 1.9.0 is pinned here).
2. **The Claude Platform export is server-push to a public endpoint.** Anthropic's servers send spans to a destination the org admin sets up. The endpoint needs a public hostname that resolves to a public IP, and it must answer within 5 s (TLS and headers) and 10 s in total (docs, "Delivery and health" and "Delivery errors"). No loopback run can exercise it. This is the same class as Cloudflare's dashboard destinations.
3. **Two platforms push to cloud-only destinations and have no offline equivalent.** Cloudflare's `wrangler dev` does not forward spans to destinations (MLflow doc § Local Development). Anthropic's destinations require the Console and the public endpoint.
4. **Pi Durable has no OTLP route.** Cetra's Pi coding-agent page (track IN M7, `m7-pi-extension.md`) found the same for the Pi coding agent: no telemetry backend in `pi-coding-agent`. Pi Durable is a different package and has none either (run-free check: 0 OTLP or OpenTelemetry hits across the 36 files of `1.5.2` that mention telemetry, tracing, otel or otlp).

## Method and versions

Every package below was checked against the registry's per-version publish times with `npm view <pkg> time --json` or PyPI's JSON. "Latest pre-cut" means the newest version published on or before 2026-11-17T00:00Z.

| Package | Version pinned | Published | Source | How used |
|---|---|---|---|---|
| `@earendil-works/pi-durable` | 1.5.2 | 2026-11-10T12:21:58Z | npm tarball, sha256 `915648ed07c48ca4a2cf8bde97e9b1693a0127cb1332225b6883bf0d5f6f9082` (1 043 221 bytes) | Read: `package.json`, `README.md`, grep of `dist/` (no run) |
| `agents` (Cloudflare) | 0.33.0 | 2026-11-03T13:16:52Z | npm tarball, sha256 `985a3e04db1c8500b6dbc6c79cbb1aa43579fef2f2ad903bde7cd3bfa764ff7f` (2 879 834 bytes) | Read: `docs/observability.md`, `dist/observability/`, `package.json` peers (no run) |
| `@restatedev/restate-sdk` | 1.18.3 | 2026-11-04T17:43:50Z | npm tarball, sha256 `5ef6815bc0ad11d563ece12c5767d5230c74360e232a24b9a135c57bbbbc4991` (2 841 431 bytes); also installed to `rs-node/` | Run: `service.mjs` (see § Restate) |
| Restate server | v1.8.1-hotfix.2 (`restate-server 1.8.1-hotfix.2`) | GitHub release 2026-11-02T12:56:28Z | `gh release download` of `restate-server-x86_64-unknown-linux-musl.tar.xz`, sha256 `57d4496453104ce3a02cdd5b520db14de9086cce20f1483795b6b9b3f65cde03` (50 143 660 bytes), matches the release's `.sha256` | Run |
| `@upstash/qstash` | 2.12.2 | 2026-11-12T13:45:26Z | npm tarball, sha256 `282727bf98fbf327b6b02ea9e315eea6ca264b70974ffb3bfb8f697e5af72592` (406 184 bytes) | Read: grep (no run) |
| `@upstash/rag-chat` | 2.0.3 | 2024-11-20T08:34:22Z (per the version's publish time) | npm tarball, sha256 `52367f0456653397596ebaae8386fe97b93a2a03b1478d1d6cb52733cd749ee6` (29 154 bytes) | Read: `package.json`, `index.js` (no run) |
| `ibm-watsonx-orchestrate` | 2.20.0 | 2026-11-11T17:20:42 (PyPI) | wheel `ibm_watsonx_orchestrate-2.20.0-py3-none-any.whl`, sha256 `ab1ac5474f30ad8a46bb38fde67da8a5f3dbfc1f548de7d3648e9a898b4af22c` (4 682 803 bytes); sdist sha256 `b29dcf56ac360884f883996a16c0719f1dabd89247c20940d9890d093fa19c5b` (4 572 192 bytes) | Read: unzipped wheel (no run; needs Docker and an IBM entitlement key) |
| `@anthropic-ai/sdk` | 1.9.0 | 2026-11-16T16:53:24Z | npm tarball, sha256 `15e7a716a40e55b9e3c199173e60d666428f62ecd74c10577d23f1413397410a` (2 852 574 bytes) | Run: `an-node/agent.mjs` |
| `@opentelemetry/sdk-trace-node`, `@opentelemetry/exporter-trace-otlp-http`, `@opentelemetry/api` | 2.12.1, 0.301.2, 1.10.0 | installed with `npm --before=2026-11-17T00:00:00Z --legacy-peer-deps` | npm | Run only, to ship OTLP/HTTP to the sink |

**Mistake, corrected:** the first Anthropic download was `@anthropic-ai/sdk` 1.13.0 (published after the cut). I deleted it (`dl/anthropic-ai-sdk-1.13.0.tgz` and its extracted tree) and re-pinned 1.9.0. Nothing from 1.13.0 was run.

**Harness caveat:** the first Anthropic run failed with `ECONNREFUSED` because `inner.sh` read a stale `port.json` left by a previous run in the same work directory. The rerun removed `port.json`, `base.url`, `recv/`, `req/` and `http.jsonl`. Worth fixing in `m7/tools/inner.sh` (delete `$M7_PORT_FILE` before starting the server).

## 1. Claude Managed Agents

**Identity.** Anthropic's hosted agent product (`platform.claude.com/docs/en/managed-agents/overview`, docs). Core objects: `/v1/agents`, `/v1/sessions`, `/v1/environments`. Beta header `managed-agents-2026-04-01`. Not eligible for ZDR or HIPAA (docs, "Data handling"). Sessions run in an Anthropic-hosted sandbox or a self-hosted one. The product cannot run offline: sessions and tools run on Anthropic's infrastructure.

**OTLP route (docs).**
- The Claude Platform exports spans for Messages API requests and for Managed Agents sessions (`platform.claude.com/docs/en/manage-claude/opentelemetry/api`, "What you need" and "1. Add a destination"). Needs the Console admin role, or the Admin API (`POST /v1/organizations/telemetry_destinations`, with `endpoint_url`, `headers`, `traces.content_mode` = `metadata_only` or full content, `workspaces`).
- Provider: "Custom" or a listed platform; cetra would be "Custom". Endpoint: OTLP/HTTP protobuf (the doc says protobuf), a hostname on port 443 (not an IP), `/v1/traces` appended by the API, gzip, `User-Agent: Claude-Platform`.
- Delivery: at least once, best effort. Retries on timeout, connection, TLS, DNS, 408, 429 and 5xx. A `3xx` disables the destination, as does 24 h of failures. Limits: 10 destinations per org, 20 headers per destination.
- Span names (docs, `platform.claude.com/docs/en/api/opentelemetry/spans`; read through a saved excerpt, so not every name was checked): `anthropic.messages.create`, `anthropic.sessions.create`, `anthropic.sessions.events.send`, `anthropic.sessions.events.stream`, `anthropic.sessions.update`, `anthropic.sessions.list`, `anthropic.session.turn`, `anthropic.model_request`, `anthropic.tool_use`, `anthropic.mcp_tool_use`, `anthropic.custom_tool_use`, `anthropic.compaction`, `anthropic.session.outcome_evaluation`, `anthropic.messages.tool_runner`. Kind: `CLIENT` from the SDK, `SERVER` from the API. `service.name` = `claude-platform` for API spans. Content mode: `metadata_only` (default) or full content; full content carries prompts, messages and tool arguments.
- Caveat in the docs: span names and attributes can change during the beta.

**SDK route (run).** `@anthropic-ai/sdk` 1.9.0 emits spans in the app. Its tracing lives in `src/lib/internal/tracing/` (`api-call.ts`, `attributes.ts`, `content.ts`, `environments.ts`, `response.ts`). Options: `ANTHROPIC_OPEN_TELEMETRY_TRACES_CONTENT_MODE=content`, `ANTHROPIC_OPEN_TELEMETRY=false`, `ANTHROPIC_OPEN_TELEMETRY_PROPAGATION=false` (docs, `manage-claude/opentelemetry/sdk`, "4. Configure the SDK").

Run (`an-node/agent.mjs`): an app span `weather-agent` (tracer `m9-app`) around two `client.messages.create` calls with a `get_weather` tool, `FAKE_TOOL=get_weather` on the fake provider. Turn 1 returned `tool_use`, turn 2 returned text. Content mode was left at the default (metadata only).

Spans received (3 spans, `an-run/recv/0001..0003-traces.json`):
- `weather-agent` (scope `m9-app`), parent of both calls.
- `anthropic.messages.create` (scope `com.anthropic.sdk.typescript@1.9.0`), parent `weather-agent`, `gen_ai.provider.name=anthropic`, `gen_ai.operation.name=chat`, `gen_ai.request.model=claude-haiku-5-5`, `gen_ai.request.max_tokens=256`, `gen_ai.usage.input_tokens=82`, `gen_ai.usage.output_tokens=17`, `gen_ai.response.id=msg_m7`, `anthropic.message.stop_reason=tool_use`, `http.route=/v1/messages`, `server.address`, `server.port`, `url.full`.
- Second `anthropic.messages.create`: input 131, output 12, `stop_reason=end_turn`.

Normalized by cetra today (`h.py`, cetra's real normalizer): both calls are `span_kind=llm`, `provider=anthropic`, `request_model` = `response_model` = `claude-haiku-5-5`, `operation=chat`, tokens 82/17 (total 99) and 131/12 (total 143), cost computed by cetra's price snapshot. The app span is a plain `span`. Left unread: `anthropic.message.stop_reason`, `gen_ai.response.id`, `gen_ai.request.max_tokens` and the `http.*` and `url.*` attributes. No duplicates.

Not run: a Managed Agents session. It needs the real Sessions API on `api.anthropic.com`, so a fake provider cannot stand in for it. Its session and tool spans come from the API side and go to a public destination.

**Competitors.** Phoenix documents the Managed Agents route: `phoenix/docs/phoenix/integrations/platforms/claude-managed-agents/claude-managed-agents-tracing.mdx:20-21` (two routes: Console export and the app SDK), `:38-61` (the Console destination), `:115` (do not run OpenInference on the same client). No other competitor found (grep over the seven checkouts).

**Recommendation: (b) setup only, as a section of the Anthropic page** (the design's appendix already merges "Claude Platform (Anthropic OTel)" into the Anthropic page). The page would give the Console steps (admin role, public endpoint, Full content for model input, and the Limits: beta, metadata-only default, 10 destinations) and state that the session side cannot run offline. The SDK side is a captured case if the Anthropic page wants one (run above, sha256 in § Method). It needs a rule: none, because the `gen_ai` spans already normalize. But the Anthropic page must warn about double counting with OpenInference (finding 1).

## 2. Cloudflare Agents

**Identity.** The Cloudflare Agents SDK (`github.com/cloudflare/agents`), npm `agents` (TypeScript; `@cloudflare/agents` on npm is an old 0.0.16 and not the SDK). Runs on Workers and Durable Objects. Pi Durable is an optional harness (see § Pi Durable).

**OTLP route (docs and package).**
- The SDK's `docs/observability.md` (in `agents` 0.33.0): traces go to "Workers Observability and configured OTLP destinations" (`observability.md:275-282`). AI SDK calls are wrapped with `wrapAISDK`, which records GenAI spans using the OpenTelemetry GenAI semantic conventions (`:275-330`); `gen_ai.input.messages` and `gen_ai.output.messages` are opt-in (`:416`, `:458`). Enable with `observability.traces.enabled` in `wrangler.jsonc`.
- Cloudflare's export: an account-level "Observability > Destinations" entry in the dashboard, then the Worker's `wrangler` config names it. OTLP/HTTP with **JSON** encoding only ("Cloudflare does not support the binary format for OTLP ingestion"); endpoints `/v1/traces` and `/v1/logs`; custom headers for auth (`developers.cloudflare.com/workers/observability/exporting-opentelemetry-data/`, docs, WebFetch). A Cloudflare account is required; the plan is not stated.
- `wrangler dev` records native spans in Local Explorer and does not forward them to destinations (MLflow doc, `mlflow/docs/docs/genai/tracing/integrations/listing/cloudflare-agents.mdx:172-174`, competitor). MLflow's own check used a local relay reading Local Explorer, which is not a supported path for cetra.

**Run.** Not run. The export to any OTLP endpoint is done by Cloudflare's service, not by the local runtime. A local run would show nothing to the sink. A relay would be a third-party tool, which the rules do not allow.

**Competitors.** MLflow has a page: `mlflow/docs/docs/genai/tracing/integrations/listing/cloudflare-agents.mdx` (destination setup at `:39`, local development at `:172`). No other competitor found.

**Recommendation: (b) setup only.** A page with the destination steps (dashboard destination, wrangler `observability.traces`, OTLP JSON, a public endpoint), Limits saying the local `wrangler dev` does not export, and no captured case. Open question 5: whether cetra's production OTLP receiver accepts JSON bodies (the normalizer harness reads JSON; the live receiver was not checked here).

## 3. Pi Durable

**Identity.** `@earendil-works/pi-durable` 1.5.2, from `github.com/earendil-works/pi` (`packages/durable`). Earendil released it on 2026-10-01 with Pi 1.0 as an experimental package (search, "Pi 1.0 Adds MCP Support and Introduces Experimental Pi Durable"; not read). Its README says `**Experimental.** The API changes without notice between releases` (`package/README.md`, docs). Not the Pi coding agent (`@earendil-works/pi-coding-agent`, covered by the M7 spike), and not Microsoft's "Durable extension" (a different product; search snippet only). **Identity is unambiguous**, so no question on that.

**OTLP route.** None. The package has no `opentelemetry` or `otlp` code. Telemetry is a host-side adapter: `pi.durable.entry`, `pi.durable.submission`, `pi.durable.tasks.active` events, and an `onReport` callback on `Harness.open()` with tags such as `storage_failed` and `task_interrupted` (`package/CHANGELOG.md`, docs). The Cloudflare harness doc (`developers.cloudflare.com/agents/harnesses/pi/`) describes `onReport` and no OpenTelemetry export.

**Run.** Not run: there is no OTLP route to exercise. A user could forward the adapter's events to a tracer in their own code, but that is not a route cetra can document.

**Competitors.** LangWatch has a Pi coding-agent page (`langwatch/docs/coding-agents/pi.mdx`) and ADR 188 for the Pi coding agent. It has no Pi Durable entry (`grep -c durable` on `pi.mdx` = 0).

**Recommendation: (c) not supported.** Move it to design doc § Not supported (D-IN-10), with the reason: no OTLP export; the in-process telemetry adapter is not a route. Revisit when Pi Durable gets an exporter.

## 4. watsonx Orchestrate

**Identity.** IBM's agent platform (watsonx Orchestrate), with its ADK on PyPI: `ibm-watsonx-orchestrate` 2.20.0 (CLI `orchestrate`). Not the watsonx.ai model provider that cetra already documents (`website/src/content/docs/docs/integrations/watsonx.mdx`, the OpenLLMetry instrumentor; D-IN-11). Two different products, same vendor.

**OTLP route (read from the wheel, docs and search).**
- Developer Edition runs as Docker containers (`ibm_watsonx_orchestrate/developer_edition/resources/docker/compose-lite.yml`), needs IBM registry images and an entitlement key. Not runnable offline.
- `default.env:262`: `FLOW_TRACING_OTLP_ENDPOINT=` (empty). `cli/commands/server/server_command.py:640-645`: when `--with-ibm-telemetry` is set, the endpoint defaults to `http://jaeger:4318/v1/traces`; otherwise it is blank. `compose-lite.yml:1223`: `WXO_OTLP_ENDPOINT: http://jaeger:4318/v1/traces` (an internal Jaeger). So the developer edition has a user-settable OTLP key for flow tracing. Which spans it emits, and their shape, was not checked.
- Pull: `orchestrate observability traces` exports one trace's spans as "OpenTelemetry-compliant" JSON to a file or stdout (`observability/traces/traces_command.py:376`). A pull, not an OTLP route.
- Inbound (IBM's endpoint, not cetra's): external agents send OTLP/HTTP to `https://api.watson-orchestrate.ibm.com/instances/<id>/v1/orchestrate/inject/traces` with a bearer token and `tenant.id` and `deployment.environment` attributes (the otel-export page, read via WebFetch; it describes the inbound direction only).
- Outbound from SaaS to a third-party OTLP endpoint: search snippets say Orchestrate forwards telemetry to Langfuse, and that MLflow is a documented target. Neither page was read here. **Unverified** (open question 4).

**Run.** Not run (Docker, IBM images and an entitlement key; no offline path).

**Competitors.** None found in the seven checkouts (`grep -i orchestrate`: no watsonx Orchestrate hits). Opik's watsonx entry is for watsonx.ai (`opik/README.md:353`).

**Recommendation: (b) setup only, provisional.** If IBM documents an outbound OTLP setting for SaaS, the page would give the steps (cloud account, the endpoint, the span shape once known) with no captured case. If IBM does not, move it to (c). Needs the user's call (open question 4).

## 5. Restate

**Identity.** Restate, `github.com/restatedev` (durable execution runtime; `restate-server` binary, `restate-sdk` for TypeScript, Python, Java and Go, `restatectl`). Vendor: Restate (restate.dev). Not a "restate" LLM library.

**OTLP route (docs and run).**
- Docs: `docs.restate.dev/operate/monitoring/tracing` (via WebFetch). Config: `tracing-endpoint` (CLI `--tracing-endpoint`, env `RESTATE_TRACING_ENDPOINT`), `tracing-headers` (TOML table). `http(s)://` means OTLP/gRPC; `otlp+http(s)://` means OTLP/HTTP and usually needs `/v1/traces`. `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES` (`service.*`) set the resource service name (default `Restate`). Other `OTEL_*` variables appear in the binary (`strings`) but are not documented.
- The server `--dump-config` shows no tracing keys by default (`rs-run/dump.txt`, only `disable-telemetry = false` in the telemetry area), so the endpoint is opt-in.
- Spans created by the runtime (docs): `ingress <target>`, `invocation-start <target>`, `invocation-attempt <target>`, `invocation-end <target>`. Attributes: `restate.invocation.id`, `restate.invocation.target`, `rpc.service`, `rpc.method`, `restate.deployment.*`, `restate.invocation.result`, and events `restate.invocation.lifecycle.new_command`, `...run_ended`, `...suspended`, `...yielded`. **No GenAI or LLM attributes** (docs: "None are mentioned on the page").
- SDK spans: the Restate SDKs create a span per handler attempt and a child span per `ctx.run` block, when tracing is enabled in the app (docs; TypeScript and Java pages deferred to there, not read). This run did not enable them.

**Run (`rs-run2/`, `rs-scripts/run.sh`, `rs-node/service.mjs`).** `restate-server` v1.8.1-hotfix.2 started with `RESTATE_TRACING_ENDPOINT=otlp+http://127.0.0.1:<sink>/v1/traces`. A TypeScript service `Assistant` (`@restatedev/restate-sdk` 1.18.3, `restate.serve` on port 9080) with one handler `ask` that calls the fake OpenAI endpoint inside `ctx.run("llm-call", …)`. Registered with `POST http://127.0.0.1:9070/deployments`; invoked twice through ingress `POST http://127.0.0.1:8080/Assistant/ask`. Both invocations returned the fake text (the fake provider received 2 requests).

Spans received: 8 (4 per invocation) in 2 OTLP files, service name `Restate`, version `1.8.1-hotfix.2`. Per invocation: `ingress Assistant/ask` (parent none), `invocation-start` (parent ingress), `invocation-attempt` (parent start, 3 events including `run_ended`), `invocation-end`. Attributes as in the docs. Normalized by cetra: all four are plain `span` kind; `invocation-attempt` status `ok`. Nothing maps to `llm`, model, tokens or tool calls. The model call and its tokens are invisible.

**Competitors.** Phoenix documents Restate with an LLM framework: `phoenix/docs/phoenix/integrations/python/restate/restate-tracing.mdx:8` (Restate's workflow spans), `:15` (install `restate-sdk openai-agents arize-phoenix-otel openinference-instrumentation-openai-agents`), `:36-50` (`OpenInferenceTracingProcessor` plus `RestateTracerProvider` to nest the agent's spans under Restate's parent span). So the LLM spans come from the OpenAI Agents instrumentor, not from Restate.

**Recommendation: (c) not supported as its own integration.** The OTLP route works offline, but it carries runtime lifecycle spans only, which would be noise for an LLM observability product. LLM spans arrive through the framework's instrumentor (OpenInference, as in Phoenix's page), which the framework's own cetra page would cover (not checked page by page). The Restate page would only add duplication. Open question 2.

## 6. Xcode (AI features)

**Identity.** Apple's Xcode 26.3+ agentic coding: AI agents (Anthropic's Claude Agent SDK, and OpenAI's Codex) inside Xcode, with settings under Intelligence. Search snippets (heise, MacRumors, TechCrunch, Apple's newsroom, Feb 2026) describe it. Apple's developer forum reports the built-in agent entries disappeared in Xcode 26.6 (search; not read). **Identity is the Xcode agent integration, not Xcode the IDE.**

**OTLP route.** None documented. A search for OpenTelemetry or telemetry export found nothing (search). The Anthropic side is the Claude Agent SDK, which is covered separately by the track CA coding-agent work. Whether Xcode's agent session enables any OpenTelemetry export is **unverified**. This needs a macOS check, which this environment cannot do.

**Run.** Not run: macOS only (the Xcode app and its AI features need macOS, and this spike ran on Linux).

**Competitors.** None found: grep for `xcode` in the seven checkouts hits only unrelated strings (`langwatch/modules/coding-agent/...`, which covers other coding agents).

**Recommendation: (c) not supported, unverified.** Revisit if Xcode documents a telemetry setting; if it does, the route would be the Claude Agent SDK's or Codex's, not an Xcode page.

## 7. Upstash QStash

**Identity.** Upstash QStash (`github.com/upstash/qstash-js`, npm `@upstash/qstash`): an HTTP message queue and scheduler. Not an LLM library.

**OTLP route.** None. The package has no OpenTelemetry or OTLP code (grep: 0 hits in the tarball). It sends SDK telemetry headers to Upstash's cloud (`cloudflare.js:301-309`, `telemetryHeaders`), which is product analytics, not spans.

**Run.** Not run; no LLM and no trace output to examine.

**Competitors.** None found. The only hits are in unrelated files (`phoenix/docs.json`, a Helicone blog page).

**Recommendation: (c) not supported.** Not a trace source (like Slack, PagerDuty, Mixpanel and PostHog in the design's appendix, out of scope). Move it to the design doc's "out of scope" row, not § Not supported.

## 8. Upstash RAG Chat

**Identity.** `@upstash/rag-chat` 2.0.3 (`github.com/upstash/rag-chat`). A RAG chat library over Upstash Vector. Depends on LangChain (`@langchain/openai`, `@langchain/anthropic`, `@langchain/mistralai`, `@langchain/core`, `langchain`), `ai` 3.x (`@ai-sdk/openai`), `langsmith`, `@upstash/vector`. The package's version publish date is 2024-11-20, the oldest item in this spike (npm `time.modified` shows 2025-11-25, a metadata change).

**OTLP route.** None of its own. Its traces go to LangSmith (`index.js:1116` and `:1073`: `global.globalTracer`, `tracingEnabled`, `project_name: "Upstash Rag Chat"`), not OTLP. Its model calls are LangChain and `ai` calls, so an OTLP route would be the LangChain or Vercel AI SDK instrumentor that cetra already documents (`website/src/content/docs/docs/integrations/langchain.mdx`, `vercel-ai-sdk.mdx`).

**Run.** Not run. It needs an Upstash Vector REST endpoint (a cloud account, or a mock of the REST API), and the model route is already covered by the LangChain instrumentor. A run would only re-test that instrumentor.

**Competitors.** None found in the seven checkouts (`grep -i upstash`: only `langwatch` package-lock, `helicone` docs and a blog page).

**Recommendation: duplicate, no page.** Add a note on the LangChain page (and the Vercel AI SDK page if its `ai` path is used) that RAG Chat calls them. Its own LangSmith tracing is out of scope.

## Open questions (for the user)

1. **Claude Managed Agents placement.** Recommend a section of the Anthropic page (setup only for the session spans, plus the captured SDK spans), not its own page. Confirm.
2. **Restate.** Its OTLP route runs offline but carries no LLM data. I recommend (c) and a note on the framework pages. Do you want it listed as a runtime in § Not supported, or left out entirely?
3. **Xcode.** Unverified: no OTLP route found in docs or search, and it needs a macOS check. Accept (c) without a macOS check, or ask someone on macOS to check Xcode's agent settings and any telemetry export?
4. **watsonx Orchestrate.** The developer edition has a user-settable OTLP key (`FLOW_TRACING_OTLP_ENDPOINT`), and the platform's outbound SaaS export is not confirmed by IBM docs I could read. Provisional (b). Should I read IBM's outbound-export page (not found by search), or move it to (c) now?
5. **Cloudflare Agents.** The Workers export is OTLP **JSON** only. Does cetra's production receiver (`crates/server`, not the normalizer harness) accept OTLP/HTTP JSON? Not checked here.
6. **Anthropic SDK native spans.** `@anthropic-ai/sdk` 1.9.0 emits spans and cetra reads them well, but the Anthropic page must tell users not to also run OpenInference's Anthropic instrumentor (double counting). Which SDK version first emitted spans was not checked (only 1.9.0). Should the Anthropic page get a captured case from this run, or a setup note only? Needs a decision.
7. **Pi Durable identity** is settled (`@earendil-works/pi-durable`). If you meant a different "Pi Durable", tell me.

## Network and downloads

- Every instrumented run was inside `netrun.sh` (loopback-only namespace). No run reached a non-loopback host. Inside the namespace, `inner.sh` reported `real-home touches: none` for each run.
- Host-side (outside the namespace): `npm pack` and `npm install` for the registry packages above (registry.npmjs.org), `pip`/`curl` for PyPI (`files.pythonhosted.org`), `gh release download` for `restatedev/restate` (the item's own repo), and WebFetch/WebSearch for the item docs (`platform.claude.com`, `docs.restate.dev`, `developers.cloudflare.com`, `developer.watson-orchestrate.ibm.com`) and for search snippets (news sites, not read). Competitor docs were read from the checkouts (read only).
- Downloads with sha256 are in § Method. Total: about 53 MB (Restate server 50.1 MB, npm packages about 9 MB, the watsonx wheel 4.7 MB).
- `m7/tools/.venv` (protobuf) was used read-only by `h.py`.

Sources: [Claude API OpenTelemetry](https://platform.claude.com/docs/en/manage-claude/opentelemetry/api) · [SDK tracing](https://platform.claude.com/docs/en/manage-claude/opentelemetry/sdk) · [OpenTelemetry span reference](https://platform.claude.com/docs/en/api/opentelemetry/spans) · [Managed Agents overview](https://platform.claude.com/docs/en/managed-agents/overview) · [Restate tracing](https://docs.restate.dev/operate/monitoring/tracing) · [Cloudflare OpenTelemetry export](https://developers.cloudflare.com/workers/observability/exporting-opentelemetry-data/) · [Cloudflare Pi harness](https://developers.cloudflare.com/agents/harnesses/pi/) · [IBM Orchestrate OTel export](https://developer.watson-orchestrate.ibm.com/traces/otel-export.md).

## Follow-up (spike D)

Status at the VM stop (2026-11-24, `ubuntu-dev`): **nothing run and no docs read in this part.** No process of mine is running there.

- **5. Inferable (platform repository): not read.** Planned: search `github.com/inferablehq/inferable` (read only) for `opentelemetry`, `otlp`, `OTEL_` and tracing exports in the platform and the SDKs. Verdict stays "not supported for the SDK"; the platform route is open.
- **6. watsonx Orchestrate (SaaS OTLP export): not read.** Planned: IBM's docs for an outbound OTLP export from the SaaS (not only the developer edition). Verdict stays "setup only or not supported" until read.
- **7. eve (Laminar and Langfuse docs): not read.** This is an item of spike A's identity table (see that file). Planned: read Laminar's and Langfuse's public docs pages about eve (read only).

### Update (resumed): items 5, 6

- **5. Inferable platform: read, verdict (c) not supported for customer traces.** Read with `gh api` (read only) on `inferablehq/inferable`. The only OpenTelemetry code is the platform's own telemetry: `control-plane/src/modules/observability/hyperdx.ts` (`@hyperdx/node-opentelemetry`, enabled by `HYPERDX_API_KEY`, service `control-plane`), `app/instrumentation.ts` (the same for the Next app), and `control-plane/src/modules/observability/tracer.ts` (an in-process tracer named `inferable-tracer`). No OTLP export is configurable for customers, and the docs page (`docs.inferable.ai/pages/observability`) only mentions a workflow timeline and "plug into your own observability tools", without a protocol. The SDK has no telemetry (spike A). Code-search for `otlp` in the repo returned nothing.
- **6. watsonx Orchestrate: read, verdict (c) not supported.** IBM's developer docs (`developer.watson-orchestrate.ibm.com/traces/otel-export`, found via search and read by WebFetch): the OTLP route runs **into** Orchestrate. Agents outside Orchestrate send spans over OTLP/HTTP to `OTEL_EXPORT_URL=https://api.watson-orchestrate.ibm.com/instances/<instance-id>/v1/orchestrate/inject/traces`, with `Authorization: Bearer` (IBM Cloud IAM or MCSP v2) or `x-api-key`, and `OTEL_RESOURCE_ATTRIBUTES` with `tenant.id` and `deployment.environment`. No outbound export from the SaaS to a third party is documented. Availability: SaaS only (the search result says AWS SaaS Commercial and IBM Cloud Trial, Essential and Standard non-isolated instances). Traces can be exported back with the ADK CLI (`orchestrate observability traces export`). The MLflow page is a separate MLflow route. The inbound endpoint is IBM's, not cetra's, so cetra cannot be a destination for it.
- **7. eve:** moved to spike A's update above (eve's Laminar and Langfuse docs are read there).
- Not checked: the full IBM setup page (only search results and the otel-export page), and the Arize, Sentry and Logfire eve pages.
