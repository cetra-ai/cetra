# M6 spike: OpenLIT, MCP, vector databases, tool APIs, evaluation tools, Harbor

Run 2026-11-24 (D-IN-17), worktree `integrations` at `7f7d3fd`. Python 3.13 (uv 0.13.6), Node 26.13.0 (nvm; the system has no Node on PATH), `exclude-newer = "7 days"` (cutoff 2026-11-17). No Docker on this machine. Scratch dir (scripts, dumps, locks):
`/tmp/claude-1000/-home-sebastien-travaux-llm-observer/8e518996-ebba-4428-93c2-4baf3ee35424/scratchpad/m6b/`.

- One uv project per tool family: `openlit/` (OpenLIT 1.45.0 with its vector database targets), `llmetry/` (OpenLLMetry instrumentors), `mcpt/` (mcp 2.3.0), `mcp1/` (mcp 1.30.0), `fc1/` (Firecrawl 1.x), `evalpy/` (Ragas, UQLM, Azure AI Evaluation), `pf/` (Promptfoo from npm), `hb/` (Harbor's trajectory converter).
- `shared/common.py` is the M4 spike's harness (fake OpenAI-compatible server, span dumper, OTLP sink, real `cetra_otel::normalize` through the `norm` binary, rebuilt against `7f7d3fd`), extended with an embeddings route, a per-path `extra` hook, and an OTLP sink that reads chunked and JSON bodies. It blocks and records every non-loopback DNS lookup. **"cetra today" is the real normalizer's output.**
- Every run printed `EXTERNAL_HOSTS_ATTEMPTED: none`, except OpenLIT's pricing fetch (`raw.githubusercontent.com`, finding 2). Vendor telemetry was off everywhere (`ANONYMIZED_TELEMETRY=False`, `RAGAS_DO_NOT_TRACK=true`, `PROMPTFOO_DISABLE_TELEMETRY=1`, and so on). Nothing was exported to a remote service.

## Verdicts

| Item | Verdict | Why |
|---|---|---|
| OpenLIT | **Captured case** (needs rule R3 and `openlit>=1.37`) | GenAI attributes since 1.37; the `gen_ai.content.*` events of older versions are not read (the page pins `>=1.37`) |
| MCP | **Captured case** (native `mcp>=2` spans; OpenLLMetry on `mcp<2`), and the registry row `mcp` | The row is OpenInference's instrumentor, which creates **no spans** (propagation only); the page says so |
| Chroma, Qdrant, Milvus | **Captured cases** (OpenLLMetry and OpenLIT) with rules R1, R2 | Run locally and offline |
| Pinecone | **Captured case** with OpenLLMetry (`pinecone<9`, a fake REST host); OpenLIT's Pinecone target is broken on Pinecone 10 | Runs offline against a fake data-plane host |
| Weaviate | **Setup-only page** (OpenLLMetry) | Needs a Weaviate server; spans read from source |
| Exa, Tavily, Parallel AI | **Not supported** | No instrumentation in OpenLIT, OpenLLMetry or OpenInference; they appear only as tool spans of a traced agent |
| Firecrawl | **Not supported** (current SDK) | OpenLIT's target is the v1 `FirecrawlApp.scrape_url`; the v4 SDK has none and OpenLIT fails silently; works only on `firecrawl<2` |
| Ragas | **SDK-test page** (traces: provider instrumentation; scores: `cetra.score`) | No spans of its own; results are usable objects |
| Promptfoo | **Captured case** from its own OTLP export (needs rule R4) | GenAI spans plus `gen_ai.evaluation.*` grader spans; OTLP/JSON |
| UQLM | **SDK-test page** | Offline-capable; `to_df()` result |
| Azure AI Evaluation | **SDK-test page** | Evaluators return dicts; its own result events go to Azure Monitor as log records (dropped by cetra) |
| Cleanlab | **Setup-only page** | `cleanlab-tlm` needs a cloud key; the result is a 0 to 1 float; not run |
| Coval | **Not supported** | Consumes traces; no documented results API (CLI JSON, MCP, webhooks); paid SaaS |
| Morph Reflex | **Not supported (needs the user)** | A hosted per-turn classifier; its docs page returned 404; no SDK or result format confirmed |
| Testable Minds | **Not supported (needs the user)** | No product of that name found (closest: TestMu AI's Agent Testing Platform, which tests an HTTP endpoint and has no SDK); ask the user for the URL |
| Harbor | **Setup-only page**, optional hand-written fixture from the converter's output | Needs Docker to run; the post-hoc `harbor-atif2otel` converter works offline, but its JSON output is rejected by cetra today (finding 12) |

Docker: none needed for any captured case. Paid accounts: Cleanlab, Coval, Morph; Pinecone and Weaviate hosted services (the captures use a fake host or are setup-only).

## Findings that change the design

1. **OpenLIT vector database spans carry `gen_ai.operation.name = vectordb`**, which no cetra row maps, so every vector span is `span`. D-IN-5's `retriever` rule must key on this and on the scope (R1).
2. **OpenLIT reports `gen_ai.usage.cost = 0` whenever its pricing table has no row**, including the default offline case: `init()` fetches `https://raw.githubusercontent.com/openlit/openlit/main/assets/pricing.json` and, when that fails (no network), writes the int `0` on every span. cetra's step 1 ("reported wins outright") stores `cost_total = 0`, `cost_source = reported`, which hides the computed price. With a local `pricing_json` the spans carry real numbers. Rule R3 needs the user's decision (it narrows D109 for one scope).
3. **OpenLLMetry's Milvus and Pinecone instrumentors 0.63.1 are not installable**: both require `opentelemetry-semantic-conventions-ai>=0.5.2`, which PyPI does not have (latest 0.5.1, 2026-03-26). The newest installable is 0.60.0 for both, and the captures used it. Chroma, Qdrant and Weaviate 0.63.1 install.
4. **OpenLLMetry's Pinecone instrumentor supports `pinecone>=5.1.0,<9` only**; with Pinecone 10.3.0 it logs one line and instruments nothing. OpenLIT's Pinecone target (`pinecone.db_data.index.Index.upsert`) fails to import on Pinecone 10 (`Failed to instrument pinecone`).
5. **The registry row `mcp` (OpenInference `MCPInstrumentor`, 2.1.5) creates no spans.** It wraps the stdio, SSE and streamable-HTTP transports and propagates the W3C context through `_meta`, so a server's spans join the client's trace. Proved on mcp 1.30.0: with it, the server's `tool-body` span has the client's trace id and the client span as parent; without it, a separate trace. On `mcp>=2` it logs one harmless `ValueError: <Token ...> was created in a different Context` and is redundant, because mcp 2 propagates natively.
6. **mcp 2.x has native OTel tracing** (scope `mcp-python-sdk`, no content); mcp 1.x has none. mcp 2.3.0 (2026-10-02) is the newest; 1.30.0 is the last 1.x at the cutoff.
7. **OpenLLMetry's MCP instrumentor 0.63.1 crashes on `mcp>=2`** (`PathResolutionError: streamablehttp_client`, raised by `McpInstrumentor().instrument()`); it works on 1.30.0.
8. **OpenLIT 1.45.0's MCP instrumentor on mcp 2.3.0 writes junk spans** (`mcp session/init`, `mcp transport/request`, duplicated `mcp tools/call` with `mcp_coroutine` placeholder payloads). It targets mcp 1.x internals. Not recommended.
9. **OpenLIT 1.45.0 needs `openai<3`**, while `tools/capture/py/pyproject.toml` pins `openai==3.45.0`. An OpenLIT case needs its own capture environment, or the capture project's pin changes (a plan decision).
10. **OpenLIT auto-instruments `httpx`, `requests`, `urllib3` and `aiohttp`**, so each model call has a child `POST` span (scope `opentelemetry.instrumentation.httpx`, kind `span`, `http.*` attributes). `disabled_instrumentors` turns it off; the page says so.
11. **A plain string in `gen_ai.input.messages` / `gen_ai.output.messages`** (OpenLIT's Firecrawl 1.x: `http://x.test`, `# Hello`) is stored as text but sets `cetra.normalize_error = messages:genai:input,messages:genai:output`. Same shape as M4's OpenInference `output.value` fix (R5, low priority).
12. **Harbor's JSON export is rejected by cetra.** `harbor-atif2otel` writes OTLP/JSON with enum names (`"kind": "SPAN_KIND_INTERNAL"`, `"code": "STATUS_CODE_OK"`, from protobuf's `MessageToDict`); cetra's JSON decoder fails (`decode: Json { line: 1, column: 542 }`) and decodes after the names are replaced by integers. Harbor's direct upload (`--endpoint`, the job plugin) is an MLflow-only uploader (`x-mlflow-experiment-id`). The usable path is `--encoding pb` plus a POST of each file (not run).

## OpenLIT 1.45.0 (2026-08-03; the newest)

Setup (the SDK reuses an existing global tracer provider; with none it builds its own and exports with `otlp_endpoint` or `OTEL_EXPORTER_OTLP_ENDPOINT`, posting `/v1/traces` and `/v1/logs`):

```python
openlit.init(otlp_endpoint="http://127.0.0.1:8080", otlp_headers="Authorization=Bearer pk-cetra-...",
             disable_metrics=True, pricing_json="pricing.json")   # a local file keeps it from fetching
```

Export to a local OTLP/HTTP sink worked (spans and a `/v1/logs` post). Dependencies: `openai<3,>=1.92`, `anthropic<1`, `boto3`, and a dozen `opentelemetry-instrumentation-*` (the app also gets HTTP client and web framework spans).

**Content: `gen_ai.content.*` events against spec 02.** Two eras (checked by reading the OpenAI instrumentation of 1.34.38, 1.35.7, 1.36.0, 1.37.1, 1.39.0, 1.41.0, 1.45.0):

| OpenLIT | Content | cetra |
|---|---|---|
| `<= 1.36.x` | span events `gen_ai.content.prompt` (attribute `gen_ai.prompt`, a flattened string) and `gen_ai.content.completion` (`gen_ai.completion`) | **Not read.** Spec 02 reads `gen_ai.client.inference.operation.details`, `gen_ai.{system,user,assistant,tool}.message` and `gen_ai.choice` events and the `gen_ai.input.messages` family. The span has no input or output. (The only `gen_ai.content.*` name cetra knows is `…completion.chunk`, for TTFT.) No rule: the page requires `openlit>=1.37` |
| `>= 1.37` | attributes `gen_ai.input.messages`, `gen_ai.output.messages` (current GenAI JSON with `parts`, `type: tool_call`) | Read as is. For AG2, AI21, Cohere, Julep, MultiOn, Mistral, PremAI and Together, 1.45.0 also writes the same strings as `gen_ai.content.prompt` / `gen_ai.content.completion` events, with the code comment "To be removed once the change to span_attributes (from span events) is complete". They duplicate attributes, so ignoring them loses nothing (Mistral's embeddings event holds `str(inputs)`, but the attribute is right) |

**OpenAI chat with a tool call, plus embeddings** (fake server; `pricing_json` local):

| span | scope | attributes |
|---|---|---|
| `chat gpt-4o-mini` (CLIENT) | `openlit.instrumentation.openai` (no version) | `gen_ai.operation.name=chat`, `gen_ai.provider.name=openai`, request and response models, `gen_ai.usage.input_tokens` 82, `output_tokens` 17, `gen_ai.client.token.usage` 99, **`gen_ai.usage.cost` [double]**, `gen_ai.usage.cache_read.input_tokens`, `gen_ai.usage.cache_creation.input_tokens`, `gen_ai.server.time_to_first_token` [double, seconds, written even without streaming], `gen_ai.input.messages`, `gen_ai.output.messages`, `gen_ai.tool.name/args/call.id` (the model's tool call, on the LLM span), `gen_ai.sdk.version`, `openai.api.type`, `server.address/port`, `service.name`, `telemetry.sdk.name=openlit`, `deployment.environment`, `openlit.agent.version_hash`, `gen_ai.request.*` defaults (`seed` 0, `temperature` 1.0) |
| `embeddings text-embedding-3-small` | same | `gen_ai.operation.name=embeddings`, `gen_ai.embeddings.dimension.count`, `gen_ai.usage.input_tokens` 8, `gen_ai.usage.cost` |
| `POST` (child) | `opentelemetry.instrumentation.httpx` | `http.method/status_code/url` (finding 10) |

cetra today: `llm`, provider `openai`, models, 82/17/99, `cost_source = reported`, `conventions = [genai]`, input and output present; `embedding` with 8 input tokens. With the pricing fetch failing: `cost_total = 0.0`, `reported` (finding 2). TTFT is read only under Strands' scope, so OpenLIT's is correctly ignored.

**OpenLIT vector databases** (Chroma 1.5.9 in process, Qdrant 1.20.0 `:memory:`, Milvus Lite through pymilvus 3.0.3; offline):

| call | span name | scope | attributes |
|---|---|---|---|
| Chroma `add` | `INSERT docs` | `openlit.instrumentation.chroma` | `gen_ai.operation.name=vectordb`, `db.system.name=chroma`, `db.operation.name=INSERT`, `db.collection.name`, `db.query.summary`, `db.query.text`, `db.vector.count`, `db.client.operation.duration`, `db.sdk.version`, `server.address=localhost`, `server.port=8000` (fixed defaults, not the real host) |
| Chroma `query`, `get` | `GET docs` for both | same | `db.operation.name=GET`, `db.vector.query.top_k`, `db.response.returned_rows`, `db.filter` |
| Chroma `delete` | `DELETE docs` | same | `db.vector.count` |
| Qdrant `create_collection`, `upsert`, `query_points` | `create_collection unknown`, `UPSERT unknown`, `QUERY unknown` | `openlit.instrumentation.qdrant` | `db.system.name=qdrant`; **the collection name is `unknown`** (positional arguments not read); `db.query.text` is a Python repr |
| Milvus `create_collection`, `insert`, `search`, `query`, `delete` | `create_collection docs`, `INSERT unknown`, `SEARCH unknown`, `QUERY unknown`, `DELETE unknown` | `openlit.instrumentation.milvus` | `db.system.name=milvus`; collection `unknown`; **`DELETE` raised `AttributeError: 'list' object has no attribute 'get'` inside OpenLIT's wrapper and the span is ERROR although the delete worked** |

No content events, no documents or vectors. cetra today: all `span` (operation `vectordb`). The Pinecone and Firecrawl targets did not instrument (findings 4, and § Tool APIs).

## Vector databases through OpenLLMetry (0.63.1: Chroma, Qdrant, Weaviate; 0.60.0: Milvus, Pinecone)

| call | span name | scope | kind | attributes |
|---|---|---|---|---|
| Chroma `add` | `chroma.add` | `opentelemetry.instrumentation.chromadb@0.63.1` | INTERNAL | `db.system=chroma`, `db.operation=add`, `db.chroma.add.{ids,documents,embeddings,metadatas}_count` |
| Chroma `query`, `get`, `delete` | `chroma.query`, `chroma.get`, `chroma.delete` | same | INTERNAL | `db.chroma.query.n_results`, `…embeddings_count`; event `db.query.result` (`db.query.result.{id,distance,document,metadata}`) |
| Qdrant `upsert`, `query_points` | `qdrant.upsert`, `qdrant.query_points` | `…qdrant@0.63.1` | CLIENT | `db.system=Qdrant` (capital Q), `qdrant.upsert.collection_name`, `qdrant.search.collection_name`, `db.vector.query.top_k`, `qdrant.upsert.points_count`; no `db.operation` |
| Milvus | `milvus.create_collection`, `.insert`, `.search`, `.query`, `.upsert`, `.delete` | `…milvus@0.60.0` | INTERNAL | `db.system=milvus`, `db.operation=<method>`, `db.milvus.search.{limit,result_count,…}`; events `db.search.result` (`id`, `distance`, `entity`), `db.query.result` |
| Pinecone 8.1.2 (a fake REST host) | `pinecone.upsert`, `pinecone.query`, `pinecone.delete` | `…pinecone@0.60.0` | CLIENT | `db.system=Pinecone`, `server.address` (the host URL), `pinecone.query.top_k`, `pinecone.usage.read_units/write_units`; events `db.query.embeddings` (the vector), `db.query.result` (`id`, `score`, `vector`) |
| Weaviate (source read of 0.63.1; not run) | `db.weaviate.collections.query.*` (`fetch_objects`, `near_text`, …), `db.weaviate.gql.query.get.do`, `db.weaviate.collections.{get,create,delete}`, `db.weaviate.collections.data.{insert,replace,update}`, `db.weaviate.collections.batch.add_object` | `…weaviate` | | `db.system=weaviate`, `db.operation=<method>` |

None writes `gen_ai.operation.name`, `traceloop.span.kind` or `llm.request.type`: **no convention is detected, the kind is `span`**, and every attribute stays in `attributes`. Input is not captured (counts only); results are span events, which cetra does not read. The OpenLLMetry OpenAI instrumentation 0.63.1 next to them writes `gen_ai.input.messages` and `gen_ai.openai.api_base`, and normalizes as a priced `embedding`.

## Rules proposed for the normalizer

Each needs a captured case (§ Capture plan) and is scope-gated, as D-IN-5 says.

- **R1, OpenLIT vector spans.** Scope name starts with `openlit.instrumentation.` and `gen_ai.operation.name` is `vectordb`: `retriever` when `db.operation.name`, compared case-insensitively, is `get`, `query` or `search` (Chroma's `query` is `GET`, so reads all land here); every other operation stays `span`. Decision for the user: D-IN-5 says "`retriever`" without limiting it to reads; an all-operations rule would make inserts and deletes retrievers.
- **R2, OpenLLMetry vector spans.** Scopes `opentelemetry.instrumentation.{chromadb,qdrant,milvus,pinecone,weaviate}` (lancedb and marqo exist in the repo and were not run): `retriever` for read operations, recognised by span name because Qdrant and Pinecone write no `db.operation`: `chroma.query`, `chroma.get`, `qdrant.query_points`, `qdrant.search`, `qdrant.scroll`, `milvus.search`, `milvus.query`, `milvus.get`, `pinecone.query`, `pinecone.fetch`, `db.weaviate.collections.query.*`, `db.weaviate.gql.query.*`. A fixed table in `kind.rs`, like the Mastra and VoltAgent rows. Optional later: the `db.query.result` events as the span's output.
- **R3, OpenLIT reported cost 0.** On a span whose scope starts with `openlit`, a `gen_ai.usage.cost` of exactly 0 while the span has tokens is treated as absent, so the price table computes the cost. Reason: finding 2. A free OpenLIT call (a local model) then stays unpriced (`none`), never `$0`, which spec 02 already prefers. Narrows D109. Alternative: only document `pricing_json`. Recommended: the rule, because the failure is silent.
- **R4, Promptfoo grader spans.** `gen_ai.evaluation.name` present, on scope `promptfoo.providers` → `evaluator`. The grader's own LLM call is a child `chat` span (`llm`) and keeps its tokens. Optional: read `promptfoo.request.body` and `promptfoo.response.body` (plain strings, 4 KB cap) as input and output on that scope's `llm` spans (they are the only content). `promptfoo.test_case` and the `openai:chat:…` provider wrapper stay `span` (no tokens, so nothing double counts).
- **R5 (low priority).** A non-JSON string in `gen_ai.input.messages` or `gen_ai.output.messages` is read as text without a normalize error. Only needed for Firecrawl 1.x, which the verdicts drop; skip unless Firecrawl is kept.
- **R6, MCP: none.** Native `mcp>=2`: the server's `tools/call <name>` span has `gen_ai.operation.name=execute_tool` and `gen_ai.tool.name`, so it is already `tool`; the client's `MCP send …` span stays `span`. OpenLLMetry: `<tool>.tool` has `traceloop.span.kind=tool` and is `tool` today with input and output previews; `initialize.mcp`, `tools/list.mcp` and six `ResponseStreamWriter` noise spans per session stay `span`. cetra's discard mechanism today is coding-agent-only (Codex); a discard rule for the writer spans would be new, so leave them and say so on the page.
- **R7, Harbor (a decision, not a spike result).** Accept the OTLP/JSON enum names `SPAN_KIND_*` and `STATUS_CODE_*` in `crates/otel/src/decode.rs` (valid proto3 JSON, finding 12), or leave Harbor to the protobuf path.

Not proposed: reading `gen_ai.content.prompt` events (older OpenLIT; pin `>=1.37`), any rule for `mcp.*` or `db.*` keys, any change to OpenLLMetry detection.

## MCP (the page and the row `mcp`)

Versions: `mcp` 2.3.0 (2026-10-02; 1.30.0 is the last 1.x), `openinference-instrumentation-mcp` 2.1.5 (2026-11-16; 2.1.8 is 2026-11-21, too new), `opentelemetry-instrumentation-mcp` 0.63.1 (2026-11-16).

1. **Native, mcp 2.3.0** (client and server in separate stdio processes, both exported through OTLP). Scope `mcp-python-sdk`, no version.

| span | kind | parent | attributes |
|---|---|---|---|
| `MCP send tools/call get_weather` | CLIENT | the caller | `mcp.method.name`, `jsonrpc.request.id` |
| `tools/call get_weather` (server process) | SERVER | the client span (context in `_meta`) | `gen_ai.operation.name=execute_tool`, `gen_ai.tool.name`, `mcp.method.name`, `mcp.protocol.version` (`2026-07-28`), `jsonrpc.request.id`; on error `error.type=tool_error` and status ERROR |
| `server/discover`, `tools/list`, `resources/read`, `prompts/get greet` (with `gen_ai.prompt.name`) | | | the same `mcp.*` keys |

   No arguments or results are recorded. cetra today: the server `tools/call` span is `tool` (operation `execute_tool`, `conventions = [genai]`, error status kept); the rest `span`. With the in-process transport (`Client(server)`) only server spans exist.
2. **OpenLLMetry on `mcp<2`** (1.30.0). Scope `opentelemetry.instrumentation.mcp.instrumentation`. Spans `initialize.mcp`, `tools/list.mcp` (`traceloop.entity.input/output`), `get_weather.tool` (`traceloop.span.kind=tool`, `traceloop.entity.name`, input `{"tool_name","arguments"}`, output `{"result": …}`), and the `ResponseStreamWriter` noise (`mcp.request.id`, `mcp.response.value`). cetra today: `get_weather.tool` is `tool` with input and output previews; the others `span`, `conventions = [openllmetry]`. The stdio server process emitted no spans.
3. **OpenInference**: no spans (finding 5). The row would be `Instrumentor(name="mcp", extra="mcp", module="openinference.instrumentation.mcp", cls="MCPInstrumentor")`. The page says what it does (joins server traces to client traces) and that `mcp>=2` does it natively.

Verdict: a captured case with native `mcp==2.3.0` (client `span`, server `tool`); a second with OpenLLMetry on `mcp==1.30.0` only if the plan wants the `.tool` input and output, the only MCP spans with content. Scenario sketch (a Python program against the mock, in `tools/capture/py`): start an `MCPServer` with `get_weather` in process, run one OpenAI chat turn (tool call), call the tool through MCP, then the final turn. Expected: a root `chain` or `agent`, `llm`, `tool` (the MCP server span), `llm`. The mock needs no change. Lock risk: `mcp` 2 conflicts with `semantic-kernel` (`mcp<2`), and OpenHands needs `mcp>=2` (M4 spike finding 11); the MCP case may need its own environment.

## Tool APIs

- **Exa** (`exa-py` 2.28.0, 2026-11-06; 2.29.0 is 2026-11-19), **Tavily** (`tavily-python` 0.8.5, 2026-10-06), **Parallel AI** (`parallel-web` 1.3.7, 2026-10-23): none appears in OpenLIT's instrumentor directory, OpenLLMetry's `packages/` or OpenInference's `python/instrumentation` (GitHub API listing; OpenInference has translators `openlit` and `openllmetry`, plus `promptflow`, `pipecat`, `portkey`, `typesafe`). No SDK writes spans. **Not supported.** Called through a traced framework (LangChain's Tavily tool, the Exa or Tavily MCP servers), the framework's tool span covers the call, and each framework page already shows that.
- **Firecrawl** (`firecrawl-py` 4.57.0, 2026-11-12; the `firecrawl` distribution is the same release): OpenLIT's targets are `firecrawl.firecrawl.FirecrawlApp.scrape_url` and `crawl_url`. The v4 SDK has neither, and the instrumentor's `try/except: pass` hides it: **no OpenLIT span** (only the `requests` instrumentation's `POST`). It also checks the distribution name `firecrawl` (with `firecrawl-py` installed it says "no installed version was found"). On `firecrawl==1.17.0` it works: `scrape http://x.test` (CLIENT, scope `openlit.instrumentation.firecrawl`, `gen_ai.operation.name=scrape`, `gen_ai.provider.name=firecrawl`, `gen_ai.input.messages` = the URL and `gen_ai.output.messages` = the markdown as plain strings (finding 11), `gen_ai.crawl.result.*`, `gen_ai.response.title`, `server.address=api.firecrawl.dev`), kind `span`. **Not supported** on the current SDK.

## Evaluation tools

Common shape: the judge or model calls inside an eval tool are LLM calls, traced by the provider's own instrumentation (a normal `llm` span; checked for Ragas, UQLM and Azure with the OpenLLMetry OpenAI instrumentation: `llm`, priced). The result goes to cetra as a score with `cetra.score(name=…, value=…, trace_id=…, span_id=…, reason=…)` (`sdks/python/src/cetra/_scores.py`; `value` is bool, int, float or str; a rejected input logs and returns `None`).

| Tool | Version (date) | Spans of its own | Result API |
|---|---|---|---|
| Ragas | 0.4.4 (2026-11-14) | none (`ragas/_analytics.py` sends analytics unless `RAGAS_DO_NOT_TRACK=true`) | `await metric.ascore(llm=…, …)` returns `MetricResult(value, reason)`, run: `MetricResult(value=pass, reason='The answer is correct.')`; `evaluate()` returns an `EvaluationResult` (`.scores`, `.to_pandas()`, not run) |
| Promptfoo | 0.125.1 (2026-10-25; 0.126.0 is 2026-11-18, too new) | yes, OTLP/JSON (below) | `promptfoo eval -o out.json`: `results.results[]` with `gradingResult.{pass,score,reason,namedScores,componentResults[]}`, `results.stats`, and `traces[]` |
| UQLM | 0.6.8 (2026-11-08) | none | `await BlackBoxUQ(llm=…, scorers=["exact_match"]).generate_and_score(prompts, num_responses=3)` returns a `UQResult`; `.to_df()` rows (`exact_match` 1.0). Run offline; the neural scorers download Hugging Face models and were not run |
| Azure AI Evaluation | 1.18.12 (2026-11-12) | none (its results go out as OTel **log events** to Azure Monitor; cetra drops logs, D-G-40) | `F1ScoreEvaluator()(response=…, ground_truth=…)` returns `{"f1_score": 0.75, "f1_score_result": "pass", "f1_score_reason": None, "f1_score_threshold": 0.5, …}`. `CoherenceEvaluator` with a fake judge failed to parse the reply (a format matter, not a normalizer one) |
| Cleanlab | `cleanlab-tlm` 1.1.39 (2025-11-21) | none | `TLM().get_trustworthiness_score(prompt, response)` returns a float 0 to 1 (a batch returns a list); needs a Cleanlab key. Not run |
| Coval | | consumes OTel, emits none | CLI JSON, an MCP server, webhooks, a human-review API; no results endpoint found (docs.coval.dev unread) |
| Morph Reflex | | | a hosted per-turn classifier (labels in under 90 ms, the vendor's claim); docs page 404 |
| Testable Minds | | | not found |

**Promptfoo's trace** (0.125.1 against the fake server with `PROMPTFOO_OTEL_ENABLED=true` and `PROMPTFOO_OTEL_ENDPOINT=http://…/v1/traces`). The exporter posts **OTLP/JSON with integer enums**, which cetra decodes. The `tracing.forwarding` config key did not export Promptfoo's own spans (it forwards what Promptfoo receives). Scope `promptfoo.providers@1.0.0`, `service.name=promptfoo`:

| span | cetra today |
|---|---|
| `promptfoo.test_case` | `span` (`promptfoo.test.success`, `promptfoo.test.score`, `promptfoo.eval.id`) |
| `openai:chat:gpt-4o-mini` (provider wrapper) | `span` (`promptfoo.provider.id`, `promptfoo.span.role=target`) |
| `chat gpt-4o-mini` (target call) | `llm`, provider openai, 20/2 tokens, computed cost; no input or output (the content is in `promptfoo.request.body` / `promptfoo.response.body`) |
| `grader contains`, `grader llm-rubric` | `span`, `conventions = [genai]`, `gen_ai.evaluation.name`, `…score.value` 1, `…score.label` pass, `…explanation` (R4) |
| `grader provider openai:chat:…` and its child `chat gpt-4o-mini` | `span`, then `llm` with the grader's 30/8 tokens |

The capture harness would run `promptfoo eval` from a pinned npm lockfile against the mock (the TypeScript runners in `tools/capture/ts` show how) and record the OTLP/JSON. The run needs `PROMPTFOO_DISABLE_TELEMETRY=1`, `PROMPTFOO_DISABLE_UPDATE=1`, `PROMPTFOO_DISABLE_SHARING=1`; no connection attempt was seen (a run without them was not tried).

**Page code sketches** (`cetra.init()` already ran):

```python
# Ragas
from ragas.llms import llm_factory
from ragas.metrics import DiscreteMetric
metric = DiscreteMetric(name="correctness", prompt="Is {response} right for {question}?", allowed_values=["pass", "fail"])
r = await metric.ascore(llm=llm_factory("gpt-4o-mini", client=client), question=q, response=a)
cetra.score(name="correctness", value=r.value, reason=r.reason, trace_id=trace_id)

# UQLM
res = await BlackBoxUQ(llm=llm, scorers=["exact_match"]).generate_and_score(prompts=[q], num_responses=3)
for row in res.to_df().to_dict("records"):
    cetra.score(name="uq_exact_match", value=row["exact_match"], trace_id=trace_id)

# Azure AI Evaluation
out = F1ScoreEvaluator()(response=a, ground_truth=gt)
cetra.score(name="f1", value=out["f1_score"], reason=out["f1_score_reason"], trace_id=trace_id,
            metadata={"threshold": out["f1_score_threshold"], "result": out["f1_score_result"]})

# Cleanlab (untested: needs a key)
cetra.score(name="trustworthiness", value=TLM().get_trustworthiness_score(prompt, response), trace_id=trace_id)
```

```bash
# Promptfoo
PROMPTFOO_OTEL_ENABLED=true PROMPTFOO_OTEL_ENDPOINT=http://localhost:8080/v1/traces promptfoo eval -c promptfooconfig.yaml
```

Where `trace_id` comes from is the plan's to settle against spec 10 (the current span's trace id); the sketches show the shape only. Whether Promptfoo's exporter reads `OTEL_EXPORTER_OTLP_HEADERS` for the auth header was not tested (the run used none).

Verdicts: Ragas, UQLM and Azure AI Evaluation are SDK-test pages (snippets run in `sdks/python/tests/test_examples.py` against the fake server; UQLM `exact_match` and Azure's F1 need no model); no registry rows; no fixture, since their `llm` spans are ordinary provider spans. Promptfoo is a captured case. Cleanlab is setup-only. Coval, Morph Reflex and Testable Minds are dropped (D-IN-10 keeps unsupported items off the site); Morph and Testable Minds go back to the user first.

## Harbor

`harbor` (harbor-framework, from the Terminal-Bench authors; PyPI `0.26.1.dev202611160728` at the cutoff, dev builds only) runs agents in containers (Docker locally; Daytona, Modal, Blaxel and others in the cloud). It needs Docker for any run. The repository (2026-11-23) has **no tracing of its own**. A workspace package, **`harbor-atif2otel` 0.1.1** (2026-08-22; depends on `opentelemetry-proto` only), converts each trial's ATIF trajectory (`agent/trajectory.json`) into spans after the run: a root AGENT span, an LLM span per agent step, a TOOL span per tool call, subagent trees. Attributes are OpenInference's: `openinference.span.kind`, `llm.model_name`, `llm.token_count.prompt/completion/prompt_details.cache_read`, `llm.cost.total`, `tool.name`, `input.value`, `output.value`, `session.id`. Scope `harbor-atif2otel@0.1.0`. Entry points: `harbor trace export --format otel` (`--output <file.jsonl>`, `--encoding pb`, `--endpoint`) and a job plugin (`OtelPlugin`; `OTEL_EXPORTER_OTLP_ENDPOINT`, `HARBOR_OTEL_OUTPUT_DIR`). Its one uploader is MLflow's (finding 12).

Run offline on the package's own fixture `trajectory_pass.json` (12 steps, 24 spans), with enum names replaced by integers: one `agent` (`claude-code`, `conventions = [openinference]`, session id), 11 `llm` spans (provider `anthropic`, `claude-opus-4-6`, first one 34,547 in and 220 out, computed cost 0.0689), 12 `tool` spans (`Read`, …); no rejections. The root span also carries the run totals; it is `agent`, so it adds nothing to the sums. As received, the JSON is rejected (finding 12).

Verdict: a **setup-only page** ("Harbor and Terminal-Bench"): `harbor trace export --format otel --encoding pb --output traces/`, then POST each file to cetra's `/v1/traces` with `Content-Type: application/x-protobuf` and the bearer header (the `pb` export was not run). A hand-written fixture from the converter's output (integer enums) is possible, tier "hand-written" (D-IN-8). A captured case is impossible (Docker).

## Capture plan (what this spike supports)

| Case | Group | Packages (pins at the 2026-11-17 cutoff) | Program | Expected kinds |
|---|---|---|---|---|
| OpenLIT | `genai` | `openlit==1.45.0`, `openai<3` (its own environment, finding 9) | a chat with a tool call, an embeddings call, local `pricing_json` | `llm` ×2 (reported cost), `embedding`, `span` (HTTP noise) |
| OpenLIT vector | `genai` | the same, plus `chromadb==1.5.9`, `qdrant-client==1.20.0`, `pymilvus[milvus_lite]==3.0.3` | add, query, delete on each | `retriever` for `GET`, `QUERY`, `SEARCH` (R1), others `span` |
| OpenLLMetry vector | `openllmetry` | `opentelemetry-instrumentation-{chromadb,qdrant,weaviate}==0.63.1`, `…-{milvus,pinecone}==0.60.0`, `pinecone<9` | the same; Pinecone needs a fake REST host (a mock route) or stays hand-written | `retriever` for the read spans (R2) |
| MCP | `genai` | `mcp==2.3.0` | a tool call through `Client(server)` | `tool` (server span), `span` |
| MCP, old SDK | `openllmetry` | `mcp==1.30.0`, `opentelemetry-instrumentation-mcp==0.63.1` | a tool call over memory streams or stdio | `tool` (`.tool`), `span` noise |
| Promptfoo | `genai` | `promptfoo@0.125.1` (npm) | `eval` with a `contains` and an `llm-rubric` assertion | `llm` ×2, `evaluator` ×2 (R4), `span` |
| Harbor | hand-written | `harbor-atif2otel==0.1.1` fixture | none | `agent`, `llm`, `tool` |

## Not tested, and risks

- Weaviate runs (needs a server; `connect_to_embedded` downloads a binary from GitHub).
- OpenLIT's other instrumentors (`crawl4ai`, `browser_use`, `astra`, `mem0`), its guards and controller mode, and streaming calls.
- Promptfoo against a real provider and with the telemetry switches off; the `tracing.forwarding` path.
- Cleanlab, Coval, Morph and Testable Minds were read from search results, not run, and their vendors' own docs were not fully read.
- Capture determinism: OpenLIT writes `openlit.agent.version_hash`, which changes on every process start (two values seen in two runs), so `scrub.rs` and `just check-capture` need to drop or fix it.
