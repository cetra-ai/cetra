# M9 spike B: RAG and data frameworks, MCP and optimisation tools

Spike for track IN milestone M9 (long-tail frameworks, `00-design.md` § Milestones M9, § Not supported, D-IN-10, D-IN-11). It decides, per item, (a) captured, (b) setup only, or (c) not supported, and notes duplicates. Evidence: `/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m9/b/` (runs in `runs/`, downloads in `dl/`, `npm-dl/`, static extracts in `static/`).

Time cutoff for "published at least 7 days before now": 2026-11-17T17:00Z (now: 2026-11-24 17:00 UTC).

## Verdicts

| # | Item | Identity | Verdict | Reason |
|---|---|---|---|---|
| 1 | txtai | PyPI `txtai` 9.14.0 (neuml/txtai) | (c) not supported | No OpenTelemetry code. Its only tracing is `mlflow-txtai` (MLflow autolog, MLflow's own backend, not OTLP). |
| 2 | Embedchain | PyPI `embedchain` 0.1.128 (last release 2025-03-25) | (c) not supported | No OpenTelemetry code. The only documented route is Helicone's proxy (base URL change), which is Helicone's own gateway, not OTLP. |
| 3 | Pixeltable | PyPI `pixeltable` 0.7.24 + `opentelemetry-instrumentation-pixeltable` 0.2.0 | (b) setup only, provisional | Native OTLP route (`[otel]` extra, `init()`). The offline run did not complete: Pixeltable's embedded Postgres needs a non-root system user, and the sandbox could not provide one and loopback together. Decision needed (open question 1). |
| 4 | Cognee | PyPI `cognee` 1.6.8 (topoteretes/cognee) | (b) setup only | Native OTLP tracing (`COGNEE_TRACING_ENABLED=true`, `OTEL_EXPORTER_OTLP_ENDPOINT`). Not run: its ingest needs an embeddings endpoint, which the fake provider does not serve. |
| 5 | LangServe | PyPI `langserve` 0.3.3 (langchain-ai/langserve) | duplicate, no page | No OpenTelemetry code. Spans come from the host app (FastAPI, LangChain instrumentors), which cetra already covers. |
| 6 | mcp-use | PyPI `mcp-use` 1.7.1 (Python) and npm `mcp-use` 2.12.5 (TypeScript), both from mcp-use/mcp-use | (c) not supported | Neither package has OpenTelemetry code. Python ships Langfuse and Laminar (Laminar's own SDK) integrations only. |
| 7 | Weco | PyPI `weco` 0.4.1 (WecoAI/weco-cli) | (c) not supported | No OpenTelemetry code. Its Langfuse integration reads Langfuse datasets and evaluators as an eval backend; it sends no traces. |
| 8 | TanStack AI | npm `@tanstack/ai` 1.5.0 (TanStack/ai) | **(a) captured** | Native OTel middleware (`@tanstack/ai/middlewares/otel`) plus the OpenInference middleware (`@arizeai/openinference-tanstack-ai` 0.3.5). Both ran offline and normalize. One normalizer concern: the native root span repeats the run's usage (see item 8). |

Summary for the user (D-IN-17): one captured case (TanStack AI); one setup-only candidate pending a decision (Pixeltable, provisional); Cognee setup only; LangServe is a duplicate; txtai, Embedchain, mcp-use, Weco are not supported (no OTLP route of their own). Open questions are at the end.

## Versions and downloads

Rule 2: the newest release published at least 7 days before now. Every download below is checked against PyPI's or npm's published sha256 or integrity field.

### PyPI (wheels, sha256 from the PyPI JSON, verified)

| Package | Version | Upload (UTC) | Size (bytes) | sha256 | Use |
|---|---|---|---|---|---|
| txtai | 9.14.0 | 2026-10-06 | 352912 | e507d34dddbfa27df9666d06bd92414cfdb739922365b72ca26d339f0262bbe7 | static scan only (latest 9.15.0 is in the listing but was not used; its upload date was not checked) |
| embedchain | 0.1.128 | 2025-03-25 | 211343 | 380e848c053a335b06d535efcbfdc6b98a5d0b2a6a1f553aae94cb1c85676183 | static scan only |
| pixeltable | 0.7.24 | 2026-11-07 | 1490312 | a60bc6b694d89933b7504bb6d31e1bd9bd6373b3fee644fbeb77481874a7c060 | installed in the venv; run attempted |
| opentelemetry-instrumentation-pixeltable | 0.2.0 | 2026-11-02 | 16321 | a553ca8716f11de83c5e94bf6cd90ec040e3576662bc808f9260dd5d5f712657 | installed (pinned by pixeltable's `otel` extra) |
| opentelemetry-instrumentation-pixeltable | 0.1.0 | 2026-10-22 | 15125 | 6fb41d932984ed2cc875b279ae4d2885853814d3ada063bbe038e3a45fec6509 | static scan only (Opik's page cites this version; it is older) |
| cognee | 1.6.8 | 2026-11-12 | 24355857 | a20e31a89b5cac308e565c77196d32567c49f94563fa307aea253378f111ee58 | static scan only (latest 1.6.10 is too new or unchecked) |
| langserve | 0.3.3 | 2025-10-17 | 1173205 | fac3bb70293b55e87ba27ddcf2d577b5f3bed3fc1b282f978af0b7fbe5ae8578 | static scan only |
| mcp-use (Python) | 1.7.1 | 2026-09-10 | 200821 | e92ee5032c02e8f4a2cde7b2ec1877c72064ff209543ff011c1170a5125d8922 | static scan only |
| weco | 0.4.1 | 2026-11-03 | 261647 | 0293494227340e1d79e6823845338d8db0212a2f1c5c9394e085173aba950ed0 | static scan only |
| mlflow-txtai | 0.4.0 | 2025-06-22 | 10804 | aabea4dae5dad7c5dfe4e55b73826b4003d8eaf244e1b5beab490d6412a47453 | static scan (txtai's tracing route) |

### npm (tarballs, sha256 of the tgz file, computed here)

| Package | Version | Published (UTC) | Size | sha256 | Use |
|---|---|---|---|---|---|
| @tanstack/ai | 1.5.0 | 2026-11-17T15:27 | 1.4M | 5e6b2569268e017c8e8b16f1db2f21e4f281e9bc955790cb795c5298b06f88fc | run (captured) |
| @tanstack/ai-openai | 1.1.1 | 2026-11-05 | 190.2K | 4a089b9beda5775a26190292b81d495b2650d006e65e56572612f24e5bf87074 | run |
| @tanstack/ai-client | 1.1.1 | 2026-11-17T15:28 | 535.4K | 16335a51bf4c86214daea98999cdd477f2375c2d7692bd22d45ea2ef4ff80eab | static scan only |
| @arizeai/openinference-tanstack-ai | 0.3.5 | 2026-11-12 | 30.4K | f78967a487670df4c129f54ac6447e3e77b01cfea92ba819e32b3ced092a539a | run (OpenInference route) |
| mcp-use | 2.12.5 | 2026-11-17T01:00 | 439.7K | 211e88a0fd0c31194435a5bdaa99885f2eb57944ee9a8a5a6471887ee4a5fb4d | static scan (0 OTel hits) |

Also: `mcp-use@2.12.6` was packed and scanned first (0 OTel hits) but it was published 2026-11-17T18:59Z, after the cutoff, so the tarball was deleted and 2.12.5 is the recorded version.

Other npm installs for the TanStack run (in `m9/b/tanstack/`, `--before 2026-11-17T23:59:59Z`; exact versions): `@opentelemetry/api` 1.9.1, `@opentelemetry/sdk-trace-node` 2.12.1, `@opentelemetry/sdk-trace-base` 2.12.1, `@opentelemetry/exporter-trace-otlp-http` 0.200.0, `zod` 4.6.7.

### Version deviations (rule 2)

- `@opentelemetry/api`: the newest eligible 1.10.0 conflicts with `sdk-trace-base` 2.12.1 (peer `<1.10.0`), so 1.9.1 is used.
- `@opentelemetry/exporter-trace-otlp-http`: the newest eligible 0.301.2 needs `api ^1.10`, which conflicts with the same SDK. 0.200.0 installs cleanly with the others. The SDK and exporter lines are therefore not the newest; the implementer should re-check a compatible set (open question 3).

Python venv for Pixeltable: `/usr/bin/python3` (CPython 3.12.3), `uv` 0.13.6 with `UV_CACHE_DIR` in the evidence dir. Packages from `uv pip list`: pixeltable 0.7.24, pixeltable-pgserver 0.7.1, opentelemetry-api 1.45.2, opentelemetry-exporter-otlp-proto-http 1.45.2, openai 3.58.0. The Pixeltable install is in `m9/b/venv/`.

## Method

- Metadata: `m7/tools/pypimeta.mjs` and `npmmeta.mjs` (registry JSON), cutoff above.
- Static scan: each wheel or tarball extracted to `m9/b/static/`, grep for `opentelemetry|OTLP|openinference|OTEL_|langfuse|laminar` (`static_scan.py`, run with `/usr/bin/python3 -I`).
- Runs: `m7/tools/netrun.sh <workdir> <timeout> -- <cmd>` (loopback-only namespace, fake OpenAI/Anthropic provider and OTLP sink on loopback). Fake key `sk-fake-m9`. Normalized with cetra's normalizer through `m7/tools/h.py` (`norm-target/release/norm`).
- Competitor checkouts (read only): `/home/sebastien/github/{mlflow,helicone,opik,langfuse,phoenix}`.

## Per item

### 8. TanStack AI (captured, (a))

**Identity.** `@tanstack/ai` 1.5.0 (npm, TanStack/ai, repo `git+https://github.com/TanStack/ai.git`), with the OpenAI adapter `@tanstack/ai-openai` 1.1.1. The OpenAI adapter calls the Responses API (`openai.responses.create`, `dist/esm/adapters/text.js`; `baseURL` is a config option).

**OTLP route.** Two routes:

1. **Native, TanStack's own**: `otelMiddleware({ tracer, meter?, captureContent?, redact?, ... })` from `@tanstack/ai/middlewares/otel` (`dist/esm/middlewares/otel.js:180`; the subpath is in `package.json` exports). It needs `@opentelemetry/api` as an optional peer. It opens a `chat` root span, one `iteration` span per model call and one `tool` span per tool call, with `gen_ai.*` attributes. Content is off by default (`captureContent`). Docs: `README.md:271` links `tanstack.com/ai/latest/docs/advanced/otel`; source `src/middlewares/otel.ts`.
2. **OpenInference (Arize)**: `openInferenceMiddleware({ tracer })` from `@arizeai/openinference-tanstack-ai` 0.3.5 (`src/index.ts`). Phoenix documents it (`phoenix/docs/phoenix/release-notes/05-2026/05-01-2026-tanstack-ai-tracing.mdx`, `phoenix/docs/phoenix/integrations/typescript/tanstack-ai/tanstack-ai-tracing.mdx`). Spans are `ai.chat` (agent), `ai.llm N` and `ai.tool <name>`, with OpenInference attributes.

**Config.** The app creates the OTel SDK and an OTLP HTTP exporter. The exporter URL is the sink's `/v1/traces`. No TanStack-specific env vars were needed.

**Run (native route, run `runs/tanstack-otel`, `M9_MODE=otel`, `M9_CAPTURE=1`).** Program: `m9/b/tanstack/run-tanstack.mjs`. Key lines:

```js
const provider = new NodeTracerProvider({ spanProcessors: [new SimpleSpanProcessor(new OTLPTraceExporter({ url: `${base}/v1/traces` }))] });
provider.register();
const stream = chat({
  adapter: openaiText("gpt-4o-mini", { baseURL: `${base}/v1` }),
  threadId: "m9-thread-1",
  messages: [{ role: "user", content: "Read hello.txt and tell me what it says." }],
  tools: [readFile],               // toolDefinition({ name: "read_file", ... }).server(...)
  middleware: [otelMiddleware({ tracer, captureContent: true })],
});
```

Result: the model answered from the fake provider after one `read_file` tool call (`assistant text: hello.txt says: hello from the M7 spike.`). Two `/v1/responses` requests. Four OTLP posts (`0001`..`0004-traces.json`), 4 spans: `chat gpt-4o-mini` (root), `chat gpt-4o-mini #0` (iteration 0, finish `tool_calls`, 82 in / 17 out), `execute_tool read_file` (tool, outcome `success`), `chat gpt-4o-mini #1` (iteration 1, 131 in / 12 out, finish `stop`). Parent links: iterations and tool under the root.

**What cetra's normalizer makes of it** (`runs/tanstack-otel/h-out.txt`):
- `chat gpt-4o-mini` (root): `span_kind="llm"`, provider openai, tokens **213 in / 29 out / 242 total**, cost computed. Iteration spans: 82/17 and 131/12.
- The `execute_tool` span: `span_kind="tool"`, `gen_ai.tool.name` read_file.
- Duplication: the root carries the run's aggregate usage (213 = 82 + 131, cost equal to the sum of the two iterations). Any per-span sum double-counts usage and cost. This is a normalizer rule need (see below).
- The attributes left unmapped on the root: `tanstack.ai.iterations`, `gen_ai.system`, `gen_ai.response.finish_reasons` (kept as attributes only).
- `gen_ai.operation.name` is absent on the root and present on the iterations.

**Run (OpenInference route, run `runs/tanstack-oi`, `M9_MODE=oi`).** Same program with `middleware: [openInferenceMiddleware({ tracer })]`. Four spans: `ai.chat` (agent, `openinference.span.kind`), `ai.llm 1` (82/17 tokens, model gpt-4o-mini), `ai.tool read_file` (tool, input and output JSON), `ai.llm 2` (131/12). The root carries no token counts, so there is no double count. Normalizer: `ai.chat` is `span_kind="agent"`; llm spans are `llm` with tokens and cost; tool span is `tool`. Minor: the input preview of `ai.llm 2` shows the first user message, not the tool message.

**Competitors.** Phoenix ships the OpenInference middleware and docs (above). Langfuse and Opik have no TanStack AI page (grep for `@tanstack/ai` found nothing in `langfuse/`, `opik/` source except unrelated packages). LangWatch, Helicone, Laminar and MLflow: no mention found.

**Recommendation: (a) captured.** One page with the native `otelMiddleware` route (TanStack docs), and a note that the OpenInference middleware is the alternative. Case: `tanstack-ai/otel-tool` (native, captureContent off in the page's default). Normalizer rule needed: when a `chat` root span carries `gen_ai.usage.*` and its `iteration` children carry the same tokens, count only the children (or drop the root's usage). Decide with the normalizer owner before the page (open question 4). Also the node SDK versions in the page must be the compatible set above.

### 3. Pixeltable (provisional (b), run not completed)

**Identity.** `pixeltable` 0.7.24 (pixeltable/pixeltable, PyPI; Python ≥3.11). Its OTLP route is native: the `otel` extra pulls `opentelemetry-instrumentation-pixeltable==0.2.0` plus the OTLP exporters and `opentelemetry-instrumentation-fastapi` (`METADATA`, lines 65-72 of the installed dist-info). Config section `[otel]` (`pixeltable/config.py:1046-1054`): `exporter_otlp_endpoint`, `exporter_otlp_protocol` (`http/protobuf` default), `exporter_otlp_headers`, `span_level` (`info`/`debug`/`trace`), `logs` (default false), `metrics` (default: only with an endpoint). Env: `OTEL_EXPORTER_OTLP_ENDPOINT` is also read. The bridge `init()` (`opentelemetry/instrumentation/pixeltable/_sdk.py:46`) takes `endpoint`, `protocol`, `span_level`, `metrics`, `logs`, and appends `/v1/traces` (the Opik page says so too).

**Documentation cross-check.** Opik's Pixeltable page (`opik/apps/opik-documentation/documentation/fern/docs-v2/integrations/pixeltable.mdx`) cites `opentelemetry-instrumentation-pixeltable` 0.1.0. That is stale: 0.2.0 is the version pinned by pixeltable 0.7.24. Opik routes it to its own OTLP endpoint, so it is a competitor reference for the native route, not a cetra page.

**Run attempt.** Program `m9/b/pixeltable-run/run_pixeltable.py`: sets `[openai] api_key/base_url` in `PIXELTABLE_HOME/config.toml`, `init(endpoint=base, protocol="http/protobuf", span_level="debug", metrics=False, logs=False)`, creates a table, adds a computed column `pxt.functions.openai.chat_completions(...)` (fake provider), collects. Outcomes, in order:
1. Import fails when `platformdirs` rejects the XDG runtime dir because the sandbox path sits below `/tmp` (owner shows as uid 65534). Worked around in the script by pointing `platformdirs.user_runtime_path` at the run's runtime dir (pgserver calls it at import).
2. `pixeltable_pgserver` (the embedded Postgres) runs `ensure_user_exists('pgserver')` when root (`pixeltable_pgserver/postgres_server.py:76-82`, `utils.py:200-217`). `netrun.sh` maps root (`unshare -Urn`), and `useradd` is not available to root there. Not solved.
3. A variant (`m9/b/netrun-user.sh`, `unshare --map-user=1000 -n`) avoids root, but the loopback interface stays down (`ip link set lo up` fails: no net-admin capability after exec). An `setpriv --ambient-caps` attempt failed with EPERM. Not solved. netrun-user.sh was not used for any reported run.
4. The socket path for pgserver is too long under the evidence dir (`Could not find a suitable socket path`), so the runtime dir needs to be short.

Nothing from Pixeltable was sent to the sink: its spans were lost with the connection errors (`Network unreachable`). No Pixeltable span shape, no normalizer result.

**Recommendation: provisional (b), setup only,** until the user decides how to run an embedded database offline (open question 1). The page would need: `pip install 'pixeltable[otel]'`, `init(endpoint=..., protocol=...)` or `OTEL_EXPORTER_OTLP_ENDPOINT` plus `[otel]` config, the note that `init()` must run before the first operation, and `span_level` (default `info`, `debug` adds row and UDF spans). Expected cetra fit: spans are Pixeltable's own (table ops, UDF calls) with OpenInference LLM spans nested under UDF spans when OpenInference is installed (Opik page). Not verified.

### 4. Cognee (setup only, (b))

**Identity.** `cognee` 1.6.8 (topoteretes/cognee, PyPI; home cognee.ai). Python `<3.15,>=3.10`.

**OTLP route.** Native tracing in `cognee/modules/observability/tracing.py`: a `TracerProvider` with a `BatchSpanProcessor` and an OTLP HTTP exporter (`tracing.py:381-414`, `set_tracer_provider` at 479). Gated by `COGNEE_TRACING_ENABLED` (`base_config.py:17-20`, `141-148`; default off, the operator must set it true). Endpoint: `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_SERVICE_NAME` (default `cognee`). Langfuse is a destination of the same OTLP pipeline when `LANGFUSE_*` keys are set (`base_config.py:100-122`). It has its own `@observe`-style spans (tests `observability/test_tracing.py`, `test_memory_semconv.py` show `cognee.*` and memory semconv attributes).

**Run.** Not run. Cognee's `add`/`cognify` call an embeddings endpoint, and the fake provider (`m7/tools/server.py`) serves only chat, messages, responses and models, so the ingest step would fail offline. Extending the shared fake provider is outside this spike's scope (and the harness is shared), so it is a user question (open question 2). The install (24 MB wheel plus a large dependency set) was not started.

**Recommendation: (b), setup only:** the page would be `pip install cognee`, `COGNEE_TRACING_ENABLED=true`, `OTEL_EXPORTER_OTLP_ENDPOINT=<cetra>`, then the normal cognee calls. Captured case only after the embeddings route exists offline.

### 5. LangServe (duplicate, no page)

**Identity.** `langserve` 0.3.3 (langchain-ai/langserve, PyPI, last release 2025-10-17). A FastAPI layer for LangChain runnables.

**OTLP route.** None of its own: 0 OpenTelemetry references in the wheel. The Opik page says the same: "LangServe does not define its own standalone OpenTelemetry bootstrap" (`opik/.../integrations/langserve.mdx:10`); tracing is configured on the host app (FastAPI and LangChain components). So LangServe's spans are the LangChain instrumentor's spans (and FastAPI's HTTP spans). Opik lists it as an integration only through its host-level setup (`opik/.../docs.yml:829-831`).

**Run.** Not run: the LangChain instrumentor path is already a covered route, so a separate run would duplicate it.

**Recommendation: duplicate.** No page of its own. If a page is wanted, a one-line note in the LangChain page: "LangServe apps are traced by the LangChain instrumentor; there is no LangServe-specific telemetry."

### 6. mcp-use (not supported, (c))

**Identity.** Two packages from one repo (`mcp-use/mcp-use`): Python `mcp-use` 1.7.1 (PyPI, "Full Stack MCP framework for python") and TypeScript `mcp-use` 2.12.5 (npm, `bin: mcp-use`, engines node ≥22.22.2). Ambiguity: the name covers both; both were checked and both are (c).

**OTLP route.** None. Python: 0 OTel references; observability modules are `agents/observability/langfuse.py` (Langfuse callback handler, `mcp_use/agents/observability/callbacks_manager.py:40-47`) and `laminar.py` (`Laminar.initialize(project_api_key=os.getenv("LAMINAR_PROJECT_API_KEY"), instruments=...)`, `laminar.py:25-34`). Laminar's SDK is its own vendor route. TypeScript 2.12.5: 0 OTel references (`static/npm-mcp-use-2.12.5`).

**Competitors.** Langfuse's README lists mcp-use among its integrations (`langfuse/README.md:330`), i.e. the Langfuse callback route. Nothing else.

**Recommendation: (c).** Nothing for cetra to receive. If mcp-use adds OTel later, reopen.

### 7. Weco (not supported, (c))

**Identity.** `weco` 0.4.1 (WecoAI/weco-cli, PyPI; "production-grade autoresearch", an optimisation loop). Command-line tool.

**OTLP route.** None. 0 OTel references in the wheel. Its only observability-looking code is `weco/integrations/langfuse/` (`bridge.py`, `backend.py`, `wizard/`): an eval backend that reads a Langfuse dataset and evaluators (`cli.py:24`, `backend.py:1-21`). It consumes Langfuse data and sends no spans. Competitors: none (a word grep of `weco` over the seven checkouts found no reference).

**Recommendation: (c).**

### 1. txtai (not supported, (c))

**Identity.** `txtai` 9.14.0 (neuml/txtai, PyPI; "all-in-one open-source AI framework for semantic search, LLM orchestration"). No OTel dependency; 0 OTel references in the wheel.

**Tracing route.** Only through `mlflow-txtai` 0.4.0 (neuml/mlflow-txtai, PyPI, "MLflow automatic tracing for txtai"): `autolog()` built on `mlflow.utils.autologging_utils` (`mlflow_txtai/__init__.py:9,17`). That sends to the MLflow tracking server, not OTLP. MLflow lists txtai as an integration (`mlflow/README.md:199`), and `mlflow/mlflow/__init__.py:112-113` loads it lazily from the external package.

**Recommendation: (c).** Note in the page-less record: "txtai tracing = MLflow autolog (vendor API)".

### 2. Embedchain (not supported, (c))

**Identity.** `embedchain` 0.1.128 (PyPI, "simplest open source retrieval (RAG) framework"; last release 2025-03-25, about 20 months before now). Helicone's docs call it "Mem0 Embedchain" (`helicone/docs/other-integrations/embedchain.mdx:2-3`, `helicone/README.md:136`), which suggests the project moved under Mem0. I did not check Mem0's repository, so the identity and current maintenance are unverified (open question 5).

**OTLP route.** None. 0 OTel references in the wheel. Helicone's page sets a Helicone proxy as the `base_url` of the OpenAI client (`embedchain.mdx:29-37`): that is Helicone's own gateway, not OTLP. Helicone is a competitor, not an OTLP route for cetra.

**Recommendation: (c).** Also: stale (no release in 20 months); a page would not be maintained.

## Normalizer and page notes (for the implementer)

- TanStack native root span carries the whole run's `gen_ai.usage.*` (see item 8). Needs a normalizer rule or a documented caveat before any token total is shown.
- TanStack OpenInference root (`ai.chat`) carries no tokens: no double count. Fine as is.
- The TanStack `execute_tool` span has `gen_ai.tool.*` and maps to `span_kind="tool"`; ok.

## Open questions (rule 1: stop and ask)

1. **Pixeltable run.** It needs an embedded Postgres under the sandbox. Options: (a) allow one run outside netrun.sh with a fresh, offline-only Pixeltable database in the evidence dir (fake provider, loopback sink), with the user's OK; (b) accept setup-only for now. Recommendation: (b) for this milestone, then a run later if wanted. Also decide whether `netrun.sh` may run as a non-root user (it maps root; pgserver refuses root without a system user).
2. **Cognee.** Add `/v1/embeddings` to the fake provider (shared harness, `m7/tools/server.py`) so `cognee.add`/`cognify` can run offline? Needs the user's OK because the harness is shared.
3. **OTel JS version set.** The newest eligible OTel JS packages conflict (`api` 1.10 vs `sdk-trace-base` 2.12). Which set does the TanStack page use? I used a compatible set (api 1.9.1, sdk-trace-node/base 2.12.1, OTLP HTTP exporter 0.200.0). A later check of the newest compatible exporter is needed before the page.
4. **Normalizer rule for the TanStack native root.** Count only iteration spans for usage/cost, or treat the root as an aggregate? Belongs to the normalizer owner (D-IN-30 item 4 style decision). Not a page decision.
5. **Embedchain identity.** Confirm with the user whether "Embedchain" means the `embedchain` PyPI project (stale since 2025-03) or a Mem0-hosted successor. I did not check Mem0's repository (outside the allowed sources).
6. **Duplicates.** LangServe (no page) and mcp-use's Laminar route (vendor SDK) are recorded as notes. Confirm the user agrees to record LangServe as a note, not a page.

## Network

- Every run went through `m7/tools/netrun.sh` (loopback-only namespace). The strace logs of the TanStack runs show 4 `connect()` calls each, all to 127.0.0.1 (the sink and fake provider).
- Real-home touches: the nvm node binary (`/home/sebastien/.local/share/nvm/v26.13.0`), which is allowed. The first Pixeltable attempt used a venv on uv's CPython under the real home (`/home/sebastien/.local/share/uv/python/...`): read-only interpreter access, recorded in `runs/pixeltable-info/real-home.txt`. The venv was rebuilt on `/usr/bin/python3` (3.12.3) for the reported attempts.
- Installs: npm into `m9/b/tanstack/` and `m9/b/npm-cache/`; PyPI into `m9/b/venv/` and `m9/b/uv-cache/`; downloads via the PyPI and npm registries only. Nothing installed system-wide; no sudo.
- Non-loopback sinks: none observed. Downloads: see the tables.
- Temporary runtime dir for the Pixeltable attempts: `/dev/shm/m9b-rt` (tmpfs, outside the evidence dir) and `/dev/shm/m9b-run-pxt`, `/dev/shm/m9b-probe`. These can be deleted.

## Not done

- Pixeltable and Cognee offline runs (see open questions 1 and 2).
- The LangChain instrumentor run for LangServe (duplicate; not needed for the decision).
- The Embedchain and Mem0 repository check (outside the allowed sources; open question 5).
- The newest compatible OTel JS exporter (open question 3).

## Follow-up (spike D)

Status at the VM stop (2026-11-24, `ubuntu-dev`): **nothing run in this part.** No process of mine is running there.

- **3. Cognee 1.6.8: not run.** The fake server already has an embeddings branch (`m7/tools/server.py`, `path.endswith("/embeddings")`): it returns one vector of 8 zeros with `usage`. That is too small for Cognee's vector store, so before a run the fake needs a configurable dimension (env var, for example `FAKE_EMBED_DIM`). Plan: `pip` install `cognee==1.6.8` (`uv`, `--exclude-newer 2026-11-17T00:00:00Z`) in `~/m9spike/cognee`, then add → cognify → search with `COGNEE_TRACING_ENABLED=true` and `OTEL_EXPORTER_OTLP_ENDPOINT`. Cognify uses structured output (a schema-shaped answer), which the harness fake does not give; that may block the run. Verdict stays (b) setup only until a run shows spans.
- **8. TanStack AI, root span kind: not re-run.** The earlier run (section 8 above) shows the native root `chat gpt-4o-mini` with the run's aggregate tokens 213/29 (equal to the sum of its two iteration spans), normalized as `llm`. That is a double count in any per-span sum. The re-run to list every span with its normalizer kind is not done. Its remote copy (`m9/b/tanstack`, 111 MB with `node_modules`) is not yet on `ubuntu-dev`.

### Update (resumed): item 3 Cognee, item 8 not re-run

- **3. Cognee 1.6.8: blocked, verdict stays (b) setup only.** Installed with `uv` (`--exclude-newer 2026-11-17`) in `~/m9spike/cognee/.venv`. The fake's `/embeddings` branch now honours `FAKE_EMBED_DIM` and returns one vector per input (copy of `server.py` on ubuntu-dev only, `~/m9spike/tools/server.py`). The run then stopped at the first `cognee.add`/`cognify` step: tiktoken fetches `cl100k_base` from `openaipublic.blob.core.windows.net` (not an approved source; only `o200k_base` was approved). **Decision needed: may cl100k_base also be downloaded (give the sha256 to verify, as with o200k), or should Cognee's tokenizer be set to something that needs no download?** Run artifacts: `~/m9spike/runs/cog1`.
- Config learned (docs by reading the installed source): embedding defaults `openai/text-embedding-3-large`, 3072 dimensions (`infrastructure/databases/vector/embeddings/LiteLLMEmbeddingEngine.py:169`), dimension env `EMBEDDING_DIMENSIONS`. Tracing: `OTEL_EXPORTER_OTLP_ENDPOINT` read in `base_config.py:147`; the exporter tries **gRPC first** unless the endpoint matches the HTTP patterns (`modules/observability/tracing.py:334-353`: `/api/public/otel`, Dynatrace, `:4318`, `:443/`). A plain `http://host:port/v1/traces` therefore goes to the gRPC exporter first. On cetra's side that means the page must give port 4317 for gRPC or 4318 for HTTP; a `/v1/traces` URL on 4318 will use HTTP.
- No spans from Cognee are recorded.
- **8. TanStack AI root span: not re-run** (ubuntu-dev stopped answering before it was started). The earlier run's facts stand: the native root `chat gpt-4o-mini` carries the run's aggregate usage (213 in / 29 out), normalized as `llm`; its two iteration spans carry 82/17 and 131/12, so the root double counts. The confirmation run (list every span with its normalizer kind and tokens) is still needed.
- Downloads in this part: none beyond the pip wheels (cognee 1.6.8, no hash recorded).

## Follow-up (spike E)

Run 2026-11-24 on `ubuntu-dev`, workdir `~/m9spike/runs/cog2` (local copy with dump, normalizer output, http.jsonl, strace: `m9/e/cog2`). **Item 3 Cognee ran for real: verdict (a) captured, with caveats** (details below).

### 3. Cognee 1.6.8: run, (a) captured

- **Approved download (once).** `https://openaipublic.blob.core.windows.net/encodings/cl100k_base.tiktoken`, **1,681,126 bytes, sha256 `223921b76ee99bde995b7ff738513eef100fb51d18c93597a113bcffe865b2a7`**, equal to tiktoken's `expected_hash` (`tiktoken_ext/openai_public.py:78` in the cognee venv's tiktoken). Cached as `~/m9spike/tiktoken-cache/9b5ad71b2ce5302211f9c61530b329a4922fc6a4` (sha1 of the URL) and pointed at with `TIKTOKEN_CACHE_DIR` in the workdir `env`. With it, no tiktoken download is attempted.
- **Tracing deps.** The venv had only `opentelemetry-api`; installed the `[tracing]` extra's HTTP side: `opentelemetry-sdk` 1.45.2 + `opentelemetry-exporter-otlp-proto-http` 1.45.2 (transport 0.66b2; uv, `--exclude-newer 2026-11-17`). Without it Cognee logs "OTel: … exporter package is not installed".
- **Endpoint selection (read in code, then used).** `_requires_http_exporter` (`cognee/modules/observability/tracing.py:334-351`) picks the HTTP exporter only for `/api/public/otel`, `dynatrace.com`, `/api/v2/otlp`, `:4318`, `:443/`; everything else tries gRPC first (which fails silently against an HTTP sink — spike D's cog1 therefore got nothing). Run 2 set `OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:<port>/api/public/otel/v1/traces` (the Langfuse pattern; the sink accepts any `/v1/traces` suffix). **On cetra's side the standard port 4318 matches natively** — the page should just use `http://cetra-host:4318`.
- **Run.** `cognee_min.py` (add → cognify → search), wrapper `~/m9spike/cognee/run-cognee.sh` (fake `openai/gpt-4o-mini`, embeddings `openai/text-embedding-3-large` dim 3072, `FAKE_EMBED_DIM=3072`, `COGNEE_TRACING_ENABLED=true`). rc 1: **add works offline; cognify fails** — its graph extraction expects schema-valid JSON (`response_format`), the fake answers plain text, every structured-output call retries for the tenacity floor (`LLM_MIN_RETRY_SECONDS = 240`, `infrastructure/llm/retry_config.py:38-41`) and then raises `CognifyFailedError` (pydantic `json_invalid`). `search` never ran. 28 model requests (25 chat = probe + retries, 3 embeddings), 20 OTLP posts (5 traces, 15 logs).
- **Spans received (32, scope `cognee@1.6.8`, kind UNSPEC everywhere, 4 separate traces: memory.store, embed, ingest chain, cognify chain):**
  - `cognee.observe.acreate_structured_output` ×2 — the LLM spans. `cognee.span.category=generation`, `langfuse.observation.type=generation`, `gen_ai.request.model=openai/gpt-4o-mini`, `gen_ai.system=litellm-native`, `langfuse.observation.input` = JSON payload `{text_input, system_prompt}` (secrets redacted by cognee), `langfuse.observation.output` = answer text (only on success). One is a **startup probe** (`text_input="test"`); the second is the real extraction call, status error, no output. **No `gen_ai.usage.*` anywhere — a package-wide grep finds no token attributes at all.**
  - `cognee.observe.embed_text` ×3 — `gen_ai.operation.name=embeddings`, `gen_ai.provider.name=openai`, `gen_ai.request.model=openai/text-embedding-3-large`, `gen_ai.embeddings.dimension.count=3072`, `server.address`.
  - Pipeline spans: `memory.store`, `memory.process` (cognify root, error), `cognee.pipeline.task.{resolve_data_directories,ingest_data,classify_documents,extract_chunks_from_documents,index_chunks_for_search,find_indexed_chunks,extract_graph_and_summarize}` with `cognee.result.*`; the failed chain carries pydantic exception events (stacktraces).
  - 15 × `cognee.db.graph.query` with the raw Cypher in `cognee.db.query`.
- **Cetra's normalizer today** (`m9/e/cog2/out/cog2.norm.jsonl`): the generation spans → `span_kind=llm`, provider `litellm-native`, request_model `openai/gpt-4o-mini`, input/output previews (json/text), conventions `genai`+`langfuse`, `cost_source=none`; embed spans → `span_kind=embedding` with model and provider; everything else stays a generic `span`. No tokens, no cost.
- **Noise.** (1) An OTLP **logs** signal ships cognee's own DEBUG logs to the same endpoint (`modules/observability/logs.py`, `LoggingHandler(level=DEBUG)`) — 15 of the 20 posts; one batch even failed to encode because it tried to ship the `CognifyFailedError` object itself. (2) The startup "test" generation span. (3) 15 graph-query spans with Cypher text. (4) Exception events embed the model's answer text (payload content inside `exception.message`). (5) Input/output payloads ship as span attributes by default (secret-redacted, not truncated beyond `_MAX_OBSERVED_CHARS`).
- **Network.** Loopback-only: every `connect()` is 127.0.0.1 except 11 DNS-stub queries to 127.0.0.53 (loopback resolver) with no external connect after them; real-home touches are reads only (`.env`/`.git`/`pyproject.toml` walk-up). `TELEMETRY_DISABLED=1` keeps litellm's posthog client inert (its module is imported, no call).
- **What a capture scenario needs:** embeddings answers with dim 3072; `cl100k_base` pre-seeded in `TIKTOKEN_CACHE_DIR`; an OTLP endpoint matching the HTTP patterns (cetra's 4318 does); and — for a full add → cognify → search flow — **a chat mock that answers `response_format` requests with schema-valid JSON**; with a plain-text mock the run stops at cognify's first extraction (after 240 s of retries). An add-only scenario captures today.
- **Recommendation: (a) captured.** The page: `pip install 'cognee[tracing]'`, `COGNEE_TRACING_ENABLED=true`, `OTEL_EXPORTER_OTLP_ENDPOINT=http://<cetra>:4318` (HTTP-pattern note above), and the caveats that no token usage is emitted (model + previews only), that an OTLP logs stream also arrives, and that input/output content is on by default.
- Competitors: a case-insensitive grep over the seven local checkouts finds no `cognee` reference (run, 2026-11-24).
- Open question (user): may the capture mock (`tools/capture`) be extended to answer `response_format`/JSON-schema chat requests, so a Cognee scenario can cover cognify and search?
